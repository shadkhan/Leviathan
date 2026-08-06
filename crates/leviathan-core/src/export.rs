//! Serializers: JSON, NDJSON and CSV, written a record at a time.
//!
//! ## Round-trip fidelity is structural, not tested-in
//!
//! Requirement 11 says what comes out must re-parse to the same thing. The way
//! to be sure of that is not to test a converter harder — it is to not convert.
//!
//! Minified JSON and NDJSON are produced by **re-emitting the source's own
//! tokens**, in order, with the whitespace between them dropped. Nothing is
//! parsed into a value and rendered back, so there is no float formatter to lose
//! `1.0000000000000002`, no escape normalizer to turn `A` into `A`, and no
//! integer path to overflow on `10000000000000000000`. A number is emitted as
//! the bytes the file spelled it with. The test suite checks this; the design is
//! what makes it true.
//!
//! Pretty JSON adds newlines and indentation *between* tokens by the same rule,
//! so it re-parses to the same document — byte-identical only after minifying
//! again, which is what "pretty" means.
//!
//! CSV is the exception and is honest about it: a table is a lossy view of a
//! tree, and [`Export::columns`] plus the flattening rule below say exactly how
//! much is lost.
//!
//! ## Nothing is assembled in memory
//!
//! One record is read, converted and handed back; the caller writes it and the
//! buffer is reused. Peak memory is one record and one column list, not one
//! file — which is the same bargain as everything else here (C1).

use crate::lexer::{Lexer, Token, TokenKind};
use crate::rows::unescape;
use crate::source::{ByteRange, SourceError, read_clamped};
use crate::structure::{ContainerKind, Documents, Event, Structure};

/// Longest single value read for export.
///
/// A record larger than this is emitted truncated and flagged, rather than
/// read whole into memory — the failure this product exists to avoid.
const MAX_RECORD: u64 = 64 * 1024 * 1024;

/// Longest text pulled out of a string for a CSV cell.
const MAX_CELL: usize = 64 * 1024;

/// Bytes read per host call, covering many records at once.
///
/// C66's lesson, applied before it could be relearned: reading each record's own
/// range costs one call per record, which on the 500 MB fixture is 1.77 million
/// of them. A window covering ~3 700 records costs 500.
const WINDOW: u64 = 1 << 20;

/// What to write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    /// One JSON array, minified. Byte-exact re-emission of the source's tokens.
    Json,
    /// One JSON array, indented. Re-parses identically; not byte-identical.
    JsonPretty,
    /// One minified value per line. The format this tool's users mostly have.
    Ndjson,
    /// A table. Lossy by nature — see [`Export::columns`].
    Csv,
}

impl ExportFormat {
    /// A stable lowercase identifier.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            ExportFormat::Json => "json",
            ExportFormat::JsonPretty => "json-pretty",
            ExportFormat::Ndjson => "ndjson",
            ExportFormat::Csv => "csv",
        }
    }

    /// The file extension conventionally used for it.
    #[must_use]
    pub const fn extension(self) -> &'static str {
        match self {
            ExportFormat::Json | ExportFormat::JsonPretty => "json",
            ExportFormat::Ndjson => "ndjson",
            ExportFormat::Csv => "csv",
        }
    }

    /// Whether this format needs a discovery pass before any record is written.
    #[must_use]
    pub const fn needs_columns(self) -> bool {
        matches!(self, ExportFormat::Csv)
    }
}

/// A streaming serializer.
///
/// Driven a record at a time: [`open`](Export::open), then
/// [`push`](Export::push) per record, then [`close`](Export::close). Each
/// returns the bytes to write — the caller owns the writing, because the core
/// does no I/O (C2).
pub struct Export {
    format: ExportFormat,
    indent: usize,
    /// Bytes produced by the most recent call.
    out: Vec<u8>,
    /// Records emitted so far, for the separators.
    written: u64,
    /// CSV column order, as discovered.
    columns: Vec<String>,
    /// Scratch: one record's fields, reused.
    fields: Vec<(String, String)>,
    /// Scratch: one record's bytes, reused.
    record: Vec<u8>,
    /// Whether any record was too large to read whole.
    truncated: bool,
    /// Whether JSON output wraps its records in an array.
    wrap: bool,
    /// Whether the record in hand lexed to a complete value.
    clean: bool,
    /// Records copied verbatim because they do not parse.
    salvaged: u64,
    /// A forward read cache: bytes from `window_at`, covering many records.
    window: Vec<u8>,
    window_at: u64,
}

impl Export {
    /// A serializer for `format`.
    #[must_use]
    pub fn new(format: ExportFormat) -> Self {
        Self {
            format,
            indent: 2,
            out: Vec::new(),
            written: 0,
            columns: Vec::new(),
            fields: Vec::new(),
            record: Vec::new(),
            truncated: false,
            wrap: true,
            clean: true,
            salvaged: 0,
            window: Vec::new(),
            window_at: 0,
        }
    }

    /// Set the indent width used by [`ExportFormat::JsonPretty`].
    #[must_use]
    pub const fn with_indent(mut self, spaces: usize) -> Self {
        self.indent = spaces;
        self
    }

    /// Whether JSON output wraps the records in an array. Default `true`.
    ///
    /// False when the "records" are really one whole document: a file holding
    /// `{"a":1}` must export as `{"a":1}` and not as `[{"a":1}]`. A sequence of
    /// records still wraps, because that is how a sequence becomes one JSON
    /// document.
    #[must_use]
    pub const fn wrapped(mut self, wrap: bool) -> Self {
        self.wrap = wrap;
        self
    }

    /// Records written so far.
    #[must_use]
    pub const fn written(&self) -> u64 {
        self.written
    }

    /// Whether any record exceeded the read bound and was cut short.
    ///
    /// Reported rather than hidden: a truncated export that claims to be
    /// complete is the kind of thing that costs someone a day.
    #[must_use]
    pub const fn truncated(&self) -> bool {
        self.truncated
    }

    /// Records copied verbatim because they do not parse.
    ///
    /// A record containing invalid UTF-8 — a real thing in real log exports —
    /// stops the lexer partway. Emitting the tokens found before that point
    /// would write a record that looks complete and is not: the first version
    /// turned a 1.0 MB fixture into 608 KB and said nothing. Copying the bytes
    /// instead keeps every one of them, which is both more faithful and the only
    /// honest reading of "minified is the source's own tokens" when the source
    /// has no tokens to give.
    #[must_use]
    pub const fn salvaged(&self) -> u64 {
        self.salvaged
    }

    /// The CSV columns discovered so far, in first-seen order.
    ///
    /// ## The flattening rule, stated once
    ///
    /// A record's **leaf scalars** become columns, named by their dotted path:
    /// `{"meta":{"region":"eu"}}` yields `meta.region`. An **array** becomes one
    /// column holding its minified JSON, rather than one column per element —
    /// otherwise a single 10 000-element array in one record would add 10 000
    /// columns to the table and the file would be unusable.
    ///
    /// A key absent from a record yields an empty cell, which is *not* the same
    /// as a key present with an empty string; the second is quoted, the first is
    /// not. That distinction is the only thing CSV offers here, and it is used.
    #[must_use]
    pub fn columns(&self) -> &[String] {
        &self.columns
    }

    /// Examine a record for its columns, without writing anything.
    ///
    /// Only [`ExportFormat::Csv`] needs this, and it needs *all* records before
    /// the first can be written: a column that first appears in record 900 000
    /// still belongs in the header. Two passes over the file is the price of a
    /// header that is right, and it is cheaper than holding the table.
    ///
    /// # Errors
    ///
    /// If the source cannot be read.
    pub fn discover<S: ByteRange>(
        &mut self,
        source: &mut S,
        start: u64,
        end: u64,
    ) -> Result<(), SourceError> {
        if !self.format.needs_columns() {
            return Ok(());
        }
        self.load(source, start, end)?;
        let record = core::mem::take(&mut self.record);
        self.fields.clear();
        collect_fields(&record, &mut self.fields);
        for (path, _) in &self.fields {
            if !self.columns.iter().any(|known| known == path) {
                self.columns.push(path.clone());
            }
        }
        self.record = record;
        Ok(())
    }

    /// The bytes that begin the output.
    pub fn open(&mut self) -> &[u8] {
        self.out.clear();
        match self.format {
            ExportFormat::Json | ExportFormat::JsonPretty if self.wrap => self.out.push(b'['),
            ExportFormat::Json | ExportFormat::JsonPretty => {}
            ExportFormat::Ndjson => {}
            ExportFormat::Csv => {
                let header: Vec<String> = self.columns.clone();
                for (at, column) in header.iter().enumerate() {
                    if at > 0 {
                        self.out.push(b',');
                    }
                    write_cell(&mut self.out, column);
                }
                self.out.extend_from_slice(b"\r\n");
            }
        }
        &self.out
    }

    /// Convert one record, and return the bytes to write for it.
    ///
    /// # Errors
    ///
    /// If the source cannot be read. A record that does not parse is written as
    /// far as it lexes and does not stop the export — a partial export of a
    /// broken file is what "degrades instead of aborting" means (C6).
    pub fn push<S: ByteRange>(
        &mut self,
        source: &mut S,
        start: u64,
        end: u64,
    ) -> Result<&[u8], SourceError> {
        self.load(source, start, end)?;
        let record = core::mem::take(&mut self.record);
        self.out.clear();

        match self.format {
            ExportFormat::Json => {
                if self.written > 0 {
                    self.out.push(b',');
                }
                self.emit_value(&record);
            }
            ExportFormat::JsonPretty => {
                let indent = self.indent;
                if self.wrap {
                    if self.written > 0 {
                        self.out.push(b',');
                    }
                    self.out.push(b'\n');
                    pad(&mut self.out, indent);
                    prettify_into(&record, indent, 1, &mut self.out);
                } else {
                    // One whole document, so it starts at column zero rather
                    // than indented inside an array it is not in.
                    prettify_into(&record, indent, 0, &mut self.out);
                }
            }
            ExportFormat::Ndjson => {
                self.emit_value(&record);
                self.out.push(b'\n');
            }
            ExportFormat::Csv => {
                self.fields.clear();
                collect_fields(&record, &mut self.fields);
                let columns = core::mem::take(&mut self.columns);
                for (at, column) in columns.iter().enumerate() {
                    if at > 0 {
                        self.out.push(b',');
                    }
                    if let Some((_, value)) = self.fields.iter().find(|(path, _)| path == column) {
                        write_cell(&mut self.out, value);
                    }
                }
                self.columns = columns;
                self.out.extend_from_slice(b"\r\n");
            }
        }

        self.record = record;
        self.written += 1;
        Ok(&self.out)
    }

    /// The bytes that end the output.
    pub fn close(&mut self) -> &[u8] {
        self.out.clear();
        match self.format {
            ExportFormat::Json if self.wrap => self.out.push(b']'),
            ExportFormat::JsonPretty if self.wrap => {
                if self.written > 0 {
                    self.out.push(b'\n');
                }
                self.out.push(b']');
                self.out.push(b'\n');
            }
            // An unwrapped document still ends with a newline when pretty —
            // it is a file, and files end with one.
            ExportFormat::JsonPretty => self.out.push(b'\n'),
            ExportFormat::Json | ExportFormat::Ndjson | ExportFormat::Csv => {}
        }
        &self.out
    }

    /// Write one value: its tokens if it has any, its bytes if it does not.
    fn emit_value(&mut self, record: &[u8]) {
        if self.clean {
            minify_into(record, &mut self.out);
            return;
        }
        self.salvaged += 1;
        let trimmed = record
            .iter()
            .rposition(|b| !b.is_ascii_whitespace())
            .map_or(&[][..], |at| &record[..=at]);
        let from = trimmed
            .iter()
            .position(|b| !b.is_ascii_whitespace())
            .unwrap_or(0);
        self.out.extend_from_slice(&trimmed[from..]);
    }

    /// Read one record into the reusable buffer.
    fn load<S: ByteRange>(
        &mut self,
        source: &mut S,
        start: u64,
        end: u64,
    ) -> Result<(), SourceError> {
        let want = end.saturating_sub(start).min(MAX_RECORD);
        if end.saturating_sub(start) > MAX_RECORD {
            self.truncated = true;
        }

        // Records are visited in ascending order, so one forward-moving window
        // serves nearly all of them. A record larger than the window is read on
        // its own rather than growing the cache to fit it.
        let inside =
            start >= self.window_at && start + want <= self.window_at + self.window.len() as u64;
        if !inside {
            if want > WINDOW {
                let bytes = read_clamped(source, start, want)?;
                self.record.clear();
                self.record.extend_from_slice(bytes);
                self.finish_load();
                return Ok(());
            }
            let bytes = read_clamped(source, start, WINDOW)?;
            self.window.clear();
            self.window.extend_from_slice(bytes);
            self.window_at = start;
        }

        let from = (start - self.window_at) as usize;
        let to = (from + want as usize).min(self.window.len());
        self.record.clear();
        self.record
            .extend_from_slice(self.window.get(from..to).unwrap_or(&[]));
        // The caller's `end` is where the *next* row begins, which for the last
        // member of an object is past its parent's closing brace. Trimming to
        // where the value actually ends is what stops an export emitting a stray
        // `}` — and it makes `push` correct however generous the caller was.
        self.finish_load();
        Ok(())
    }

    /// Trim the record in hand to where its value actually ends.
    fn finish_load(&mut self) {
        match value_end(&self.record) {
            Some(at) => {
                self.record.truncate(at);
                self.clean = true;
            }
            // No complete value here: invalid UTF-8, a truncated tail, or bytes
            // that are not JSON at all. `emit_value` copies them rather than
            // writing the fragment that did lex.
            None => self.clean = false,
        }
    }
}

/// Where the first complete JSON value in `bytes` ends.
///
/// `None` if it does not end within `bytes` — a truncated value, which the
/// caller keeps whole rather than guessing at.
#[must_use]
pub(crate) fn value_end(bytes: &[u8]) -> Option<usize> {
    let mut structure = Structure::new(Documents::One);
    let mut depth = 0usize;

    for token in tokens_of(bytes) {
        let at = token.end as usize;
        let Ok(event) = structure.push(token) else {
            return None;
        };
        match event {
            Some(Event::Open { .. }) => depth += 1,
            Some(Event::Close { .. }) => {
                depth -= 1;
                if depth == 0 {
                    return Some(at.min(bytes.len()));
                }
            }
            Some(Event::Scalar { .. }) if depth == 0 => return Some(at.min(bytes.len())),
            _ => {}
        }
    }
    None
}

// ------------------------------------------------------------------- JSON

/// Re-emit `bytes` with the whitespace between tokens removed.
///
/// Every token is copied verbatim, so numbers keep their spelling and strings
/// keep their escapes. This is what makes minified output re-parse to exactly
/// the input rather than to something a formatter thought was equivalent.
pub(crate) fn minify_into(bytes: &[u8], out: &mut Vec<u8>) {
    for token in tokens_of(bytes) {
        let from = token.start as usize;
        let to = (token.end as usize).min(bytes.len());
        if let Some(slice) = bytes.get(from..to) {
            out.extend_from_slice(slice);
        }
    }
}

/// Minify into a fresh buffer.
#[must_use]
pub(crate) fn minify(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    minify_into(bytes, &mut out);
    out
}

/// Re-emit `bytes` indented, starting at `depth`.
fn prettify_into(bytes: &[u8], indent: usize, depth: usize, out: &mut Vec<u8>) {
    let mut structure = Structure::new(Documents::One);
    let mut level = depth;
    // Whether the last thing emitted was a key, so its value stays on the line.
    let mut after_key = false;
    let mut first_in_container = true;

    for token in tokens_of(bytes) {
        let from = token.start as usize;
        let to = (token.end as usize).min(bytes.len());
        let raw = bytes.get(from..to).unwrap_or(&[]).to_vec();
        let kind = token.kind;
        let Ok(event) = structure.push(token) else {
            // Malformed past this point: emit what is left verbatim rather than
            // dropping it, so a broken record still exports its readable half.
            out.extend_from_slice(bytes.get(from..).unwrap_or(&[]));
            return;
        };
        let Some(event) = event else { continue };

        let opening = matches!(event, Event::Open { .. });
        let closing = matches!(event, Event::Close { .. });

        if closing {
            level = level.saturating_sub(1);
            if !first_in_container {
                out.push(b'\n');
                pad(out, indent * level);
            }
            out.extend_from_slice(&raw);
            first_in_container = false;
            after_key = false;
            continue;
        }

        if after_key {
            after_key = false;
        } else {
            if !first_in_container {
                out.push(b',');
            }
            if level > depth || !first_in_container {
                out.push(b'\n');
                pad(out, indent * level);
            }
            first_in_container = false;
        }

        out.extend_from_slice(&raw);

        if matches!(event, Event::Key { .. }) {
            out.extend_from_slice(b": ");
            after_key = true;
        }
        if opening {
            level += 1;
            first_in_container = true;
        }
        let _ = kind;
    }
}

fn pad(out: &mut Vec<u8>, spaces: usize) {
    out.resize(out.len() + spaces, b' ');
}

/// Lex `bytes` to completion, including the final flush.
///
/// The flush is not optional: a document that is a bare number yields no tokens
/// without it, and an export would write an empty file for `42` (C30, C37, and
/// the four sightings after them).
fn tokens_of(bytes: &[u8]) -> Vec<Token> {
    let mut lexer = Lexer::new();
    let mut tokens = Vec::new();
    for token in lexer.feed(bytes) {
        let Ok(token) = token else { break };
        tokens.push(token);
    }
    if let Ok(Some(token)) = lexer.finish() {
        tokens.push(token);
    }
    tokens
}

// -------------------------------------------------------------------- CSV

/// Flatten one record into `(dotted path, cell text)` pairs.
///
/// See [`Export::columns`] for the rule and why arrays are one cell.
fn collect_fields(bytes: &[u8], out: &mut Vec<(String, String)>) {
    let mut structure = Structure::new(Documents::One);
    let mut path: Vec<String> = Vec::new();
    let mut pending_key: Option<String> = None;
    let mut skip_until: Option<u32> = None;
    let mut array_start: Option<u64> = None;

    for token in tokens_of(bytes) {
        let from = token.start as usize;
        let to = (token.end as usize).min(bytes.len());
        let raw = bytes.get(from..to).unwrap_or(&[]).to_vec();
        let kind = token.kind;
        let start = token.start;
        let end = token.end;
        let Ok(event) = structure.push(token) else {
            return;
        };
        let Some(event) = event else { continue };

        // Inside an array being taken whole: wait for its close, then emit the
        // minified span as one cell.
        if let Some(depth) = skip_until {
            if let Event::Close { depth: at, .. } = event {
                if at == depth {
                    let span = array_start.unwrap_or(start) as usize;
                    let text = String::from_utf8_lossy(&minify(
                        bytes
                            .get(span..(end as usize).min(bytes.len()))
                            .unwrap_or(&[]),
                    ))
                    .into_owned();
                    out.push((path.join("."), text));
                    path.pop();
                    skip_until = None;
                    array_start = None;
                }
            }
            continue;
        }

        match event {
            Event::Key { .. } => {
                pending_key = Some(unescape(&raw, MAX_CELL).0);
            }
            Event::Scalar { .. } => {
                if let Some(key) = pending_key.take() {
                    path.push(key);
                }
                let text = if matches!(kind, TokenKind::String { .. }) {
                    unescape(&raw, MAX_CELL).0
                } else {
                    String::from_utf8_lossy(&raw).into_owned()
                };
                out.push((path.join("."), text));
                path.pop();
            }
            Event::Open {
                kind: container,
                depth,
                ..
            } => {
                if let Some(key) = pending_key.take() {
                    path.push(key);
                } else if !path.is_empty() {
                    // An array element that is itself a container, inside an
                    // array we are already taking whole — cannot happen, since
                    // that array is skipped.
                }
                if container == ContainerKind::Array {
                    skip_until = Some(depth);
                    array_start = Some(start);
                }
            }
            Event::Close { .. } => {
                path.pop();
            }
        }
    }
}

/// Write one CSV cell, quoted per RFC 4180 when it has to be.
///
/// Note what this deliberately does **not** do: it does not prefix a leading
/// `=`, `+`, `-` or `@` to stop a spreadsheet treating the cell as a formula.
/// That mitigation corrupts the value, and requirement 11 is that what comes out
/// is what was in there. The data being exported is the user's own; mangling it
/// to protect them from their own spreadsheet is a trade this tool does not make
/// silently.
fn write_cell(out: &mut Vec<u8>, text: &str) {
    // An empty value is quoted, so that a key present with an empty string
    // (`""`) reads differently from a key that is absent (nothing at all).
    // RFC 4180 does not require this; it is the only way CSV can carry the
    // distinction, and the distinction is real.
    let needs_quotes = text.is_empty()
        || text
            .bytes()
            .any(|b| matches!(b, b',' | b'"' | b'\n' | b'\r'));
    if !needs_quotes {
        out.extend_from_slice(text.as_bytes());
        return;
    }
    out.push(b'"');
    for byte in text.bytes() {
        if byte == b'"' {
            out.push(b'"');
        }
        out.push(byte);
    }
    out.push(b'"');
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Export a whole NDJSON-ish source, record per line, and return the bytes.
    fn export(source: &str, format: ExportFormat) -> String {
        let records: Vec<(u64, u64)> = {
            let mut at = 0u64;
            let mut out = Vec::new();
            for line in source.split_inclusive('\n') {
                let trimmed = line.trim_end();
                if !trimmed.is_empty() {
                    out.push((at, at + trimmed.len() as u64));
                }
                at += line.len() as u64;
            }
            out
        };

        let mut export = Export::new(format);
        let mut bytes = source.as_bytes();

        if format.needs_columns() {
            for (start, end) in &records {
                export.discover(&mut bytes, *start, *end).expect("discover");
            }
        }

        let mut out = Vec::new();
        out.extend_from_slice(export.open());
        for (start, end) in &records {
            let chunk = export.push(&mut bytes, *start, *end).expect("push");
            out.extend_from_slice(chunk);
        }
        out.extend_from_slice(export.close());
        String::from_utf8(out).expect("utf-8")
    }

    #[test]
    fn ndjson_out_is_the_tokens_that_went_in() {
        let source = "{ \"a\" : 1 }\n[ 1 , 2 ]\n";
        assert_eq!(export(source, ExportFormat::Ndjson), "{\"a\":1}\n[1,2]\n");
    }

    #[test]
    fn a_number_keeps_the_spelling_the_file_gave_it() {
        // The reason minification re-emits tokens instead of parsing values. A
        // float round-trip would render these four as three, and a big integer
        // would arrive as 1e19.
        let source = "1.0\n1.0000000000000002\n10000000000000000000\n1e400\n-0\n";
        assert_eq!(
            export(source, ExportFormat::Ndjson),
            "1.0\n1.0000000000000002\n10000000000000000000\n1e400\n-0\n"
        );
    }

    #[test]
    fn a_string_keeps_the_escapes_the_file_gave_it() {
        // `A` and `A` are the same string and not the same bytes. An
        // exporter that normalized them would produce a file that no longer
        // matches the one it came from.
        let source = r#"{"a":"A","b":"A","c":"\/","d":"é"}"#;
        assert_eq!(
            export(&format!("{source}\n"), ExportFormat::Ndjson),
            format!("{source}\n")
        );
    }

    #[test]
    fn json_wraps_the_records_in_an_array() {
        assert_eq!(
            export("{\"a\":1}\n{\"b\":2}\n", ExportFormat::Json),
            "[{\"a\":1},{\"b\":2}]"
        );
        assert_eq!(export("", ExportFormat::Json), "[]");
    }

    #[test]
    fn minified_output_re_parses_to_the_same_tokens() {
        // Requirement 11, as a property rather than as a claim: the token
        // stream of the output equals the token stream of the input.
        let source = r#"{"a":[1,2,{"b":null}],"c":"x y","d":true}"#;
        let out = export(&format!("{source}\n"), ExportFormat::Ndjson);

        let before: Vec<String> = tokens_of(source.as_bytes())
            .iter()
            .map(|t| {
                String::from_utf8_lossy(&source.as_bytes()[t.start as usize..t.end as usize])
                    .into_owned()
            })
            .collect();
        let trimmed = out.trim_end();
        let after: Vec<String> = tokens_of(trimmed.as_bytes())
            .iter()
            .map(|t| {
                String::from_utf8_lossy(&trimmed.as_bytes()[t.start as usize..t.end as usize])
                    .into_owned()
            })
            .collect();

        assert_eq!(before, after);
    }

    #[test]
    fn pretty_output_re_parses_to_the_same_tokens() {
        let source = r#"{"a":[1,2],"b":{"c":null},"d":"x"}"#;
        let out = export(&format!("{source}\n"), ExportFormat::JsonPretty);

        let mut flattened = minify(out.as_bytes());
        // Unwrap the array the exporter added.
        assert_eq!(flattened.first(), Some(&b'['));
        assert_eq!(flattened.last(), Some(&b']'));
        flattened.remove(0);
        flattened.pop();
        assert_eq!(String::from_utf8(flattened).unwrap(), source);
    }

    #[test]
    fn pretty_output_is_actually_indented() {
        let out = export("{\"a\":1,\"b\":[2]}\n", ExportFormat::JsonPretty);
        assert!(out.contains("\n  {"), "record indented: {out}");
        assert!(out.contains("\"a\": 1"), "key and value on one line: {out}");
        assert!(out.ends_with("]\n"), "{out}");
    }

    #[test]
    fn csv_discovers_every_column_including_late_ones() {
        // A column first seen in the last record still belongs in the header,
        // which is the whole reason discovery is its own pass.
        let source = "{\"a\":1}\n{\"a\":2,\"b\":3}\n{\"c\":4}\n";
        let out = export(source, ExportFormat::Csv);
        let lines: Vec<&str> = out.lines().collect();

        assert_eq!(lines[0], "a,b,c");
        assert_eq!(lines[1], "1,,");
        assert_eq!(lines[2], "2,3,");
        assert_eq!(lines[3], ",,4");
    }

    #[test]
    fn csv_flattens_objects_by_path_and_arrays_into_one_cell() {
        // 10 000 elements in one record must not become 10 000 columns.
        let source = r#"{"id":1,"meta":{"region":"eu","n":2},"tags":["a","b"]}"#;
        let out = export(&format!("{source}\n"), ExportFormat::Csv);
        let lines: Vec<&str> = out.lines().collect();

        assert_eq!(lines[0], "id,meta.region,meta.n,tags");
        assert_eq!(lines[1], r#"1,eu,2,"[""a"",""b""]""#);
    }

    #[test]
    fn csv_quotes_what_rfc_4180_says_to_quote() {
        let source = r#"{"a":"x,y","b":"say \"hi\"","c":"one\ntwo","d":"plain"}"#;
        let out = export(&format!("{source}\n"), ExportFormat::Csv);
        // Deliberately not `lines()`: cell `c` holds a newline, which is exactly
        // the case RFC 4180 quoting exists for, and splitting on lines tears the
        // record in half — the mistake this test made on its first run.
        let body = out.split_once("\r\n").map(|(_, rest)| rest).unwrap_or("");
        assert_eq!(
            body, "\"x,y\",\"say \"\"hi\"\"\",\"one\ntwo\",plain\r\n",
            "{out}"
        );
    }

    #[test]
    fn an_absent_key_and_an_empty_string_are_different_cells() {
        // The one distinction CSV offers, and it is used.
        let source = "{\"a\":1,\"b\":\"\"}\n{\"a\":2}\n";
        let out = export(source, ExportFormat::Csv);
        let lines: Vec<&str> = out.lines().collect();

        assert_eq!(lines[0], "a,b");
        assert_eq!(lines[1], "1,\"\"", "present and empty is quoted");
        assert_eq!(lines[2], "2,", "absent is nothing at all");
    }

    #[test]
    fn a_string_cell_is_unescaped_because_a_table_holds_text() {
        // The opposite choice from JSON export, and deliberate: CSV is already
        // a lossy view, and a cell reading `café` would be useless.
        let source = r#"{"a":"café"}"#;
        let out = export(&format!("{source}\n"), ExportFormat::Csv);
        assert_eq!(out.lines().nth(1), Some("café"));
    }

    #[test]
    fn a_record_that_does_not_parse_exports_what_it_can() {
        // C6: a partial export of a broken file beats refusing to export.
        let out = export("{\"a\":1}\n{\"b\":\n", ExportFormat::Ndjson);
        assert!(out.starts_with("{\"a\":1}\n"), "{out}");
    }

    #[test]
    fn a_bare_number_record_is_not_lost_to_the_missing_flush() {
        // The seventh place this would have bitten.
        assert_eq!(export("42\n", ExportFormat::Ndjson), "42\n");
        assert_eq!(export("42\n", ExportFormat::Json), "[42]");
    }

    #[test]
    fn minify_strips_only_the_whitespace_between_tokens() {
        // Also the identity `dedup` relies on to decide two records are the
        // same: whitespace must not matter, and whitespace *inside a string*
        // must matter.
        assert_eq!(minify(b"{ \"a\" : 1 }"), b"{\"a\":1}");
        assert_eq!(minify(b"[1,\n  2]"), b"[1,2]");
        assert_eq!(minify(br#"{"a b":"c d"}"#), br#"{"a b":"c d"}"#);
    }

    #[test]
    fn every_format_names_itself_and_its_extension() {
        for (format, name, ext) in [
            (ExportFormat::Json, "json", "json"),
            (ExportFormat::JsonPretty, "json-pretty", "json"),
            (ExportFormat::Ndjson, "ndjson", "ndjson"),
            (ExportFormat::Csv, "csv", "csv"),
        ] {
            assert_eq!(format.as_str(), name);
            assert_eq!(format.extension(), ext);
        }
    }
}
