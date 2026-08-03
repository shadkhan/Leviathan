//! Row materialization: turning an 8-byte offset back into something paintable.
//!
//! The index deliberately stores almost nothing (`DEEP_REASONING.md` C1) — a
//! child is a byte offset and that is all. This module is the other half of that
//! bargain: given an offset, re-read a few kilobytes and reconstruct the key,
//! the kind, a preview and a child count. The trade only works if this is fast,
//! so this is where it gets measured.
//!
//! ## One read per screen, not one per row
//!
//! Siblings are contiguous in the file, so rows 900 000–900 050 of an array
//! occupy one short byte range. Materializing them takes **one** [`ByteRange`]
//! read covering the whole run, not fifty. That matters more in the browser than
//! natively — a `Blob.slice().arrayBuffer()` costs about a millisecond
//! regardless of size, so fifty of them is fifty milliseconds and one is one.
//! Same reasoning as C5's "one RPC per animation frame".
//!
//! ## Everything here is bounded
//!
//! A row must cost a bounded amount of work no matter what the document does,
//! because the alternative is a viewer that freezes on one pathological row:
//!
//! - **Previews are truncated** to [`RowOptions::preview_chars`], so a 50 MB
//!   string value costs the same as a short one.
//! - **Child counts are budgeted.** Counting a container's children means
//!   walking it, and a 400 MB container would take seconds. So the walk stops
//!   after [`RowOptions::row_budget`] bytes and reports [`Count::AtLeast`]. The
//!   UI shows `1,000+ items` rather than blocking, and the exact count arrives
//!   when the node is expanded and actually indexed.
//!
//! ## A broken row is a row, not a failure
//!
//! One malformed record must not blank the screen (C6). A value that does not
//! lex renders as [`ValueKind::Invalid`] with the error as its preview, and its
//! neighbours render normally. The only errors that propagate are failures of
//! the *source* — a revoked file handle, a disk that went away — because there
//! is nothing sensible to paint when the bytes themselves are gone.

use crate::index::ChildTable;
use crate::lexer::{Lexer, TokenKind};
use crate::source::{ByteRange, SourceError, read_clamped};
use crate::structure::{ContainerKind, Documents, Event, Structure};

/// What kind of value a row holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ValueKind {
    /// `{ … }`
    Object,
    /// `[ … ]`
    Array,
    /// A string.
    String,
    /// A number.
    Number,
    /// `true`
    True,
    /// `false`
    False,
    /// `null`
    Null,
    /// The bytes at this offset are not a valid JSON value.
    Invalid,
}

impl ValueKind {
    /// A stable lowercase identifier, for the boundary and the CLI.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            ValueKind::Object => "object",
            ValueKind::Array => "array",
            ValueKind::String => "string",
            ValueKind::Number => "number",
            ValueKind::True => "true",
            ValueKind::False => "false",
            ValueKind::Null => "null",
            ValueKind::Invalid => "invalid",
        }
    }

    /// Whether this value can be expanded to show children.
    #[must_use]
    pub const fn is_container(self) -> bool {
        matches!(self, ValueKind::Object | ValueKind::Array)
    }

    const fn of_token(kind: TokenKind) -> Self {
        match kind {
            TokenKind::String { .. } => ValueKind::String,
            TokenKind::Number { .. } => ValueKind::Number,
            TokenKind::True => ValueKind::True,
            TokenKind::False => ValueKind::False,
            TokenKind::Null => ValueKind::Null,
            TokenKind::ObjectOpen | TokenKind::ObjectClose => ValueKind::Object,
            TokenKind::ArrayOpen | TokenKind::ArrayClose => ValueKind::Array,
            TokenKind::Comma | TokenKind::Colon => ValueKind::Invalid,
        }
    }
}

/// How many children a container has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Count {
    /// Counted to the end of the container.
    Exact(u64),
    /// The budget ran out first. There are at least this many.
    AtLeast(u64),
    /// Not a container.
    None,
}

impl Count {
    /// The number counted so far, whether or not it is final.
    #[must_use]
    pub const fn value(self) -> u64 {
        match self {
            Count::Exact(n) | Count::AtLeast(n) => n,
            Count::None => 0,
        }
    }

    /// Whether counting finished.
    #[must_use]
    pub const fn is_exact(self) -> bool {
        matches!(self, Count::Exact(_))
    }
}

/// One row of the tree, ready to paint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// Byte offset the row starts at — its key, for an object member.
    pub offset: u64,
    /// Byte offset of the value itself, which differs from `offset` when there
    /// is a key.
    pub value_start: u64,
    /// Byte offset one past the value, when it was determined. `None` means the
    /// value ran past the budget and its extent is not yet known.
    pub value_end: Option<u64>,
    /// What kind of value.
    pub kind: ValueKind,
    /// The member's key, unescaped and truncated. `None` for array elements.
    pub key: Option<String>,
    /// A short rendering of the value: the text of a scalar, the message of an
    /// invalid one, empty for a container.
    pub preview: String,
    /// Whether `preview` was cut short.
    pub truncated: bool,
    /// Children, for containers.
    pub children: Count,
}

impl Row {
    /// Whether this row can be expanded.
    #[must_use]
    pub fn expandable(&self) -> bool {
        self.kind.is_container() && self.children.value() > 0
    }
}

/// Limits that keep materializing a row bounded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowOptions {
    /// Characters of preview to keep. A tree row cannot show more than this
    /// anyway, and the cost of a row must not scale with the value in it.
    pub preview_chars: usize,
    /// Bytes to re-read per row before giving up on an exact answer.
    pub row_budget: u32,
    /// Bytes to read at once when materializing a run of rows.
    pub window: u32,
}

impl Default for RowOptions {
    fn default() -> Self {
        Self {
            // Wider than any sane tree row; the UI truncates again for display.
            preview_chars: 120,
            // 8 KiB covers essentially every record in real NDJSON, so counts
            // are exact in the common case and bounded in the pathological one.
            row_budget: 8 * 1024,
            // 256 KiB holds a full screen of rows in one read even when they
            // are large, and is a single `Blob.slice` in the Worker.
            window: 256 * 1024,
        }
    }
}

impl RowOptions {
    /// The window, never smaller than one row's budget — otherwise a window
    /// could fail to make progress.
    const fn effective_window(&self) -> u64 {
        if self.window > self.row_budget {
            self.window as u64
        } else {
            self.row_budget as u64
        }
    }
}

/// Materialize a contiguous run of rows from a child table.
///
/// Reads as few byte ranges as the window allows — one, for any ordinary screen
/// of rows. Content errors become [`ValueKind::Invalid`] rows rather than
/// failures.
///
/// # Errors
///
/// Only [`SourceError`]: the underlying bytes could not be read.
pub fn materialize<S: ByteRange>(
    table: &ChildTable,
    start: usize,
    count: usize,
    source: &mut S,
    options: &RowOptions,
) -> Result<Vec<Row>, SourceError> {
    let offsets = table.range(start, count);
    if offsets.is_empty() {
        return Ok(Vec::new());
    }

    let keyed = table.keyed();
    let mut rows = Vec::with_capacity(offsets.len());
    let mut next = 0usize;

    while next < offsets.len() {
        let window_start = offsets[next];
        let window_len = window_span(offsets, next, options);
        let bytes = read_clamped(source, window_start, window_len)?;
        let window_end = window_start + bytes.len() as u64;

        // How many rows this window can answer. A row needs its whole budget
        // inside the window — or the window to have reached end-of-file, in
        // which case these are all the bytes there will ever be.
        let at_eof = bytes.len() < window_len as usize;
        let mut last = next;
        while last < offsets.len() {
            let budgeted = offsets[last].saturating_add(u64::from(options.row_budget));
            if !at_eof && budgeted > window_end {
                break;
            }
            last += 1;
        }
        // Guarantee progress even when one row alone overflows the window.
        let last = last.max(next + 1).min(offsets.len());

        for &offset in &offsets[next..last] {
            let local = (offset - window_start) as usize;
            let slice = bytes.get(local..).unwrap_or(&[]);
            rows.push(materialize_one(offset, slice, keyed, options));
        }
        next = last;
    }

    Ok(rows)
}

/// How many bytes to ask for to cover rows from `next` onward.
fn window_span(offsets: &[u64], next: usize, options: &RowOptions) -> u64 {
    let start = offsets[next];
    let last = offsets[offsets.len() - 1];
    // Enough for every remaining row plus the last one's budget, capped.
    let wanted = last
        .saturating_sub(start)
        .saturating_add(u64::from(options.row_budget));
    wanted.min(options.effective_window()).max(1)
}

/// Reconstruct one row from bytes beginning at its offset.
fn materialize_one(offset: u64, bytes: &[u8], keyed: bool, options: &RowOptions) -> Row {
    let mut row = Row {
        offset,
        value_start: offset,
        value_end: None,
        kind: ValueKind::Invalid,
        key: None,
        preview: String::new(),
        truncated: false,
        children: Count::None,
    };

    let mut lexer = Lexer::resuming_at(offset, 1);

    if keyed {
        // A key, then a colon, then the value. This is why a child offset points
        // at the key and not the value (C28) — lexing backwards is impossible.
        let Some(key_token) = next_token(&mut lexer, bytes, offset) else {
            row.preview = "unreadable key".to_string();
            return row;
        };
        if !matches!(key_token.kind, TokenKind::String { .. }) {
            row.preview = "expected a key".to_string();
            return row;
        }
        let (text, truncated) = unescape(
            span_of(bytes, offset, key_token.start, key_token.end),
            options.preview_chars,
        );
        row.key = Some(text);
        row.truncated |= truncated;

        match next_token(&mut lexer, bytes, offset) {
            Some(colon) if colon.kind == TokenKind::Colon => {}
            _ => {
                row.preview = "expected `:` after key".to_string();
                return row;
            }
        }
    }

    let Some(token) = next_token(&mut lexer, bytes, offset) else {
        // Nothing lexed. A string too large for the window is the one case worth
        // rescuing: its first bytes are a perfectly good preview.
        return rescue_oversized_string(row, bytes, options);
    };

    row.value_start = token.start;

    match token.kind {
        TokenKind::ObjectOpen | TokenKind::ArrayOpen => {
            row.kind = ValueKind::of_token(token.kind);
            // From the *value*, not from the row. For an array element those are
            // the same offset, which is why passing the row's start went
            // unnoticed until an object member held a container: the walk then
            // began at the key, read a complete string document, and called the
            // colon after it trailing garbage — reporting every such container
            // as empty. See C44.
            let from = usize::try_from(token.start.saturating_sub(offset)).unwrap_or(usize::MAX);
            let walked = walk_container(token.start, bytes.get(from..).unwrap_or(&[]), options);
            row.children = walked.children;
            row.value_end = walked.end;
            row.preview = walked.preview;
            row.truncated |= walked.truncated;
        }
        kind if kind.is_scalar() => {
            row.kind = ValueKind::of_token(kind);
            row.value_end = Some(token.end);
            let raw = span_of(bytes, offset, token.start, token.end);
            let (text, truncated) = if matches!(kind, TokenKind::String { .. }) {
                unescape(raw, options.preview_chars)
            } else {
                (String::from_utf8_lossy(raw).into_owned(), false)
            };
            row.preview = text;
            row.truncated |= truncated;
        }
        other => {
            row.preview = format!("unexpected {}", other.as_str());
        }
    }

    row
}

/// Pull the next token, tolerating the window running out.
///
/// The cursor is derived from the lexer rather than tracked alongside it —
/// `Lexer::offset` is defined as "where the next chunk must begin", which is
/// exactly the position within `bytes` that has not been fed yet. Keeping a
/// second copy of that number would only create something to get out of sync.
fn next_token(lexer: &mut Lexer, bytes: &[u8], base: u64) -> Option<crate::lexer::Token> {
    let cursor = usize::try_from(lexer.offset().saturating_sub(base)).unwrap_or(usize::MAX);

    if cursor < bytes.len() {
        let mut tokens = lexer.feed(&bytes[cursor..]);
        let next = tokens.next();
        drop(tokens);
        if let Some(Ok(token)) = next {
            return Some(token);
        }
    }

    // The bytes ran out mid-token. Only a number can be pending (C30), and
    // flushing it is right when the window ended at end-of-file. When the window
    // merely ran out, this reports a number truncated at the budget — which
    // needs a number longer than 8 KiB to happen, and costs a preview digit.
    lexer.finish().ok().flatten()
}

/// Characters of a single key or value inside a container preview.
///
/// Short on purpose: the point of the preview is to show *which* fields a record
/// has, and a row that spends its whole width on one timestamp has answered a
/// question nobody asked. The whole preview is capped again by
/// [`RowOptions::preview_chars`].
const INLINE_CHARS: usize = 24;

/// What one walk of a container's leading bytes learned.
struct Walked {
    children: Count,
    end: Option<u64>,
    /// The container's contents, rendered inline and without its brackets —
    /// `id: 0, level: "info"` — for the caller to wrap.
    preview: String,
    /// Whether the preview stopped early, either at its budget or the walk's.
    truncated: bool,
}

/// Count a container's children *and* compose an inline preview, in one pass.
///
/// The count and the preview want exactly the same walk over exactly the same
/// bytes, so doing them separately would double the cost of the most frequently
/// executed function in the renderer. Both are bounded by
/// [`RowOptions::row_budget`], so a 400 MB container still costs what an 8 KiB
/// one costs — the preview simply stops early and says so.
///
/// The preview exists because a row reading `{ 11 items }` tells the user
/// nothing about the record they are looking at. Every other JSON viewer shows
/// the first few fields, and a tree that does not is a tree you have to expand
/// before you can decide whether you wanted to.
fn walk_container(offset: u64, bytes: &[u8], options: &RowOptions) -> Walked {
    let limit = (options.row_budget as usize).min(bytes.len());
    let mut lexer = Lexer::resuming_at(offset, 1);
    let mut structure = Structure::new(Documents::One);
    let mut seen = 0u64;

    let mut preview = String::new();
    let mut truncated = false;
    let mut pending_key: Option<String> = None;

    // Once the preview is full the walk continues — the child count is still
    // wanted, and it is nearly free from here.
    let room = |text: &str| text.chars().count() < options.preview_chars;

    for token in lexer.feed(&bytes[..limit]) {
        let Ok(token) = token else {
            return Walked {
                children: Count::AtLeast(seen),
                end: None,
                preview,
                truncated: true,
            };
        };

        match structure.push(token) {
            Ok(Some(Event::Close {
                depth: 0,
                children,
                end,
                ..
            })) => {
                return Walked {
                    children: Count::Exact(children),
                    end: Some(end),
                    preview,
                    truncated,
                };
            }
            Ok(Some(Event::Key { token, depth: 1 })) => {
                if room(&preview) {
                    let (text, cut) =
                        unescape(span_of(bytes, offset, token.start, token.end), INLINE_CHARS);
                    pending_key = Some(if cut { format!("{text}…") } else { text });
                }
            }
            Ok(Some(Event::Scalar { token, depth: 1 })) => {
                seen += 1;
                if room(&preview) {
                    let raw = span_of(bytes, offset, token.start, token.end);
                    let value = if matches!(token.kind, TokenKind::String { .. }) {
                        let (text, cut) = unescape(raw, INLINE_CHARS);
                        format!("\"{text}{}\"", if cut { "…" } else { "" })
                    } else {
                        String::from_utf8_lossy(raw).into_owned()
                    };
                    push_field(&mut preview, pending_key.take(), &value);
                } else {
                    truncated = true;
                }
            }
            Ok(Some(Event::Open { kind, depth: 1, .. })) => {
                seen += 1;
                if room(&preview) {
                    // Nested containers are named, not entered: descending would
                    // make the cost of a row depend on the shape below it.
                    let shape = match kind {
                        ContainerKind::Object => "{…}",
                        ContainerKind::Array => "[…]",
                    };
                    push_field(&mut preview, pending_key.take(), shape);
                } else {
                    truncated = true;
                }
            }
            Ok(_) => {}
            Err(_) => {
                return Walked {
                    children: Count::AtLeast(seen),
                    end: None,
                    preview,
                    truncated: true,
                };
            }
        }
    }

    // Ran out of budget before the container closed.
    Walked {
        children: Count::AtLeast(seen),
        end: None,
        preview,
        truncated: true,
    }
}

/// Append `key: value` (or just `value`) to a comma-separated preview.
fn push_field(preview: &mut String, key: Option<String>, value: &str) {
    if !preview.is_empty() {
        preview.push_str(", ");
    }
    if let Some(key) = key {
        preview.push_str(&key);
        preview.push_str(": ");
    }
    preview.push_str(value);
}

/// A string value larger than the window still has a usable preview.
fn rescue_oversized_string(mut row: Row, bytes: &[u8], options: &RowOptions) -> Row {
    if bytes.first() == Some(&b'"') {
        row.kind = ValueKind::String;
        let (text, _) = unescape_body(&bytes[1..], options.preview_chars);
        row.preview = text;
        row.truncated = true;
    } else {
        row.preview = "unreadable value".to_string();
    }
    row
}

/// The bytes of `[from, to)` given a slice that begins at `base`.
fn span_of(bytes: &[u8], base: u64, from: u64, to: u64) -> &[u8] {
    let start = (from - base) as usize;
    let end = ((to - base) as usize).min(bytes.len());
    bytes.get(start..end).unwrap_or(&[])
}

/// Unescape a quoted JSON string, truncating at `max_chars`.
///
/// Returns the text and whether it was cut short.
fn unescape(quoted: &[u8], max_chars: usize) -> (String, bool) {
    let body = quoted
        .strip_prefix(b"\"")
        .map_or(quoted, |rest| rest.strip_suffix(b"\"").unwrap_or(rest));
    unescape_body(body, max_chars)
}

/// Unescape the inside of a JSON string.
///
/// The lexer has already validated everything here — escapes are well formed,
/// UTF-8 is valid — so this decodes rather than checks, and anything it cannot
/// decode is a lone surrogate.
fn unescape_body(body: &[u8], max_chars: usize) -> (String, bool) {
    let mut out = String::with_capacity(body.len().min(max_chars * 4));
    let mut chars = 0usize;
    let mut i = 0usize;

    while i < body.len() {
        if chars >= max_chars {
            return (out, true);
        }
        let byte = body[i];

        if byte != b'\\' {
            // Copy one whole UTF-8 sequence.
            let width = utf8_width(byte);
            let end = (i + width).min(body.len());
            match core::str::from_utf8(&body[i..end]) {
                Ok(text) => out.push_str(text),
                // A sequence cut in half by the window edge. Nothing is lost
                // that the preview needed.
                Err(_) => return (out, true),
            }
            i = end;
            chars += 1;
            continue;
        }

        let Some(&escape) = body.get(i + 1) else {
            return (out, true);
        };
        i += 2;
        chars += 1;

        match escape {
            b'"' => out.push('"'),
            b'\\' => out.push('\\'),
            b'/' => out.push('/'),
            b'b' => out.push('\u{8}'),
            b'f' => out.push('\u{c}'),
            b'n' => out.push('\n'),
            b'r' => out.push('\r'),
            b't' => out.push('\t'),
            b'u' => {
                let Some(unit) = hex4(body, i) else {
                    return (out, true);
                };
                i += 4;
                out.push(decode_unit(unit, body, &mut i));
            }
            // Unreachable for lexed input; harmless if the window cut an escape.
            _ => return (out, true),
        }
    }

    (out, false)
}

/// Resolve a UTF-16 code unit, consuming a surrogate pair if there is one.
///
/// **The `i_` decision, recorded:** a lone surrogate — a high one not followed
/// by a low, or a stray low — becomes U+FFFD REPLACEMENT CHARACTER rather than
/// being rejected. JSONTestSuite classes these as implementation-defined, and a
/// viewer that refuses to display a record because one field has a broken escape
/// would be failing at its one job (C6).
fn decode_unit(unit: u16, body: &[u8], i: &mut usize) -> char {
    const REPLACEMENT: char = '\u{FFFD}';

    if !(0xD800..0xE000).contains(&unit) {
        return char::from_u32(u32::from(unit)).unwrap_or(REPLACEMENT);
    }
    // A low surrogate on its own has no partner to look for.
    if unit >= 0xDC00 {
        return REPLACEMENT;
    }
    // A high surrogate: look for `\uXXXX` with a low surrogate.
    if body.get(*i) != Some(&b'\\') || body.get(*i + 1) != Some(&b'u') {
        return REPLACEMENT;
    }
    let Some(low) = hex4(body, *i + 2) else {
        return REPLACEMENT;
    };
    if !(0xDC00..0xE000).contains(&low) {
        return REPLACEMENT;
    }
    *i += 6;

    let combined = 0x1_0000 + ((u32::from(unit) - 0xD800) << 10) + (u32::from(low) - 0xDC00);
    char::from_u32(combined).unwrap_or(REPLACEMENT)
}

/// Read four hex digits at `at`.
fn hex4(body: &[u8], at: usize) -> Option<u16> {
    let digits = body.get(at..at + 4)?;
    let mut value = 0u16;
    for &digit in digits {
        let nibble = match digit {
            b'0'..=b'9' => digit - b'0',
            b'a'..=b'f' => digit - b'a' + 10,
            b'A'..=b'F' => digit - b'A' + 10,
            _ => return None,
        };
        value = (value << 4) | u16::from(nibble);
    }
    Some(value)
}

/// Bytes in the UTF-8 sequence a lead byte starts.
const fn utf8_width(lead: u8) -> usize {
    match lead {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{RecordScanner, RootCollector};
    use crate::structure::Structure;

    fn record_table(source: &[u8]) -> ChildTable {
        let mut scanner = RecordScanner::new();
        scanner.feed(source);
        scanner.finish()
    }

    fn root_table(source: &[u8]) -> ChildTable {
        let mut lexer = Lexer::new();
        let mut structure = Structure::new(Documents::One);
        let mut collector = RootCollector::new();
        for token in lexer.feed(source) {
            if let Some(event) = structure.push(token.unwrap()).unwrap() {
                collector.observe(event);
            }
        }
        if let Some(token) = lexer.finish().unwrap() {
            if let Some(event) = structure.push(token).unwrap() {
                collector.observe(event);
            }
        }
        collector.finish()
    }

    fn rows(source: &[u8], table: &ChildTable, start: usize, count: usize) -> Vec<Row> {
        let mut src = source;
        materialize(table, start, count, &mut src, &RowOptions::default()).unwrap()
    }

    // ---- array rows -------------------------------------------------------

    #[test]
    fn array_elements_render_their_values() {
        let source = br#"[1,"two",true,null,3.5]"#;
        let table = root_table(source);
        let rows = rows(source, &table, 0, 10);

        let kinds: Vec<ValueKind> = rows.iter().map(|r| r.kind).collect();
        assert_eq!(
            kinds,
            [
                ValueKind::Number,
                ValueKind::String,
                ValueKind::True,
                ValueKind::Null,
                ValueKind::Number
            ]
        );
        let previews: Vec<&str> = rows.iter().map(|r| r.preview.as_str()).collect();
        assert_eq!(previews, ["1", "two", "true", "null", "3.5"]);
        assert!(rows.iter().all(|r| r.key.is_none()));
    }

    #[test]
    fn object_members_render_key_and_value() {
        let source = br#"{"name":"leviathan","big":true}"#;
        let table = root_table(source);
        let rows = rows(source, &table, 0, 10);

        assert_eq!(rows[0].key.as_deref(), Some("name"));
        assert_eq!(rows[0].preview, "leviathan");
        assert_eq!(rows[1].key.as_deref(), Some("big"));
        assert_eq!(rows[1].kind, ValueKind::True);
    }

    #[test]
    fn a_value_span_is_exact_enough_to_re_read() {
        let source = br#"{"k":"value"}"#;
        let table = root_table(source);
        let row = &rows(source, &table, 0, 1)[0];

        assert_eq!(row.offset, 1, "the row starts at the key");
        assert_eq!(row.value_start, 5, "the value starts after the colon");
        assert_eq!(row.value_end, Some(12));
        assert_eq!(&source[5..12], br#""value""#);
    }

    // ---- containers -------------------------------------------------------

    #[test]
    fn a_container_previews_its_contents_rather_than_only_counting_them() {
        // The complaint this exists for: a row reading `{ 11 items }` says
        // nothing about the record, so a user has to expand every one to find
        // out which is which.
        let source = br#"[{"id":0,"level":"info","tags":["a","b"],"meta":{"n":1}}]"#.to_vec();
        let table = root_table(&source);
        let mut src = source.as_slice();
        let rows = materialize(&table, 0, 1, &mut src, &RowOptions::default()).unwrap();

        assert_eq!(rows[0].kind, ValueKind::Object);
        assert_eq!(rows[0].children, Count::Exact(4));
        assert_eq!(
            rows[0].preview, r#"id: 0, level: "info", tags: […], meta: {…}"#,
            "keys and scalars inline; nested containers named, not entered"
        );
        assert!(!rows[0].truncated);
    }

    #[test]
    fn an_array_previews_its_elements_without_keys() {
        let source = br#"[[1,2,3],["x","y"]]"#.to_vec();
        let table = root_table(&source);
        let mut src = source.as_slice();
        let rows = materialize(&table, 0, 2, &mut src, &RowOptions::default()).unwrap();

        assert_eq!(rows[0].preview, "1, 2, 3");
        assert_eq!(rows[1].preview, r#""x", "y""#);
    }

    #[test]
    fn an_empty_container_previews_as_nothing() {
        let source = br#"[{},[]]"#.to_vec();
        let table = root_table(&source);
        let mut src = source.as_slice();
        let rows = materialize(&table, 0, 2, &mut src, &RowOptions::default()).unwrap();

        assert_eq!(rows[0].preview, "");
        assert_eq!(rows[1].preview, "");
        assert_eq!(rows[0].children, Count::Exact(0));
    }

    #[test]
    fn a_container_preview_stops_at_the_width_and_says_so() {
        // Bounded like every other row cost: a 5 000-key object must paint in
        // the time a 3-key one does.
        let mut source = Vec::from(b"[{");
        for i in 0..500 {
            if i > 0 {
                source.push(b',');
            }
            source.extend_from_slice(format!("\"key{i}\":\"value{i}\"").as_bytes());
        }
        source.extend_from_slice(b"}]");

        let table = root_table(&source);
        let mut src = source.as_slice();
        let options = RowOptions::default();
        let rows = materialize(&table, 0, 1, &mut src, &options).unwrap();

        assert!(rows[0].truncated, "the preview was cut short");
        assert!(
            rows[0].preview.chars().count() < options.preview_chars * 2,
            "and it stayed near its budget: {} chars",
            rows[0].preview.chars().count()
        );
        assert!(rows[0].preview.starts_with("key0: \"value0\""));
    }

    #[test]
    fn containers_report_their_child_counts() {
        let source = br#"[[1,2,3],{"a":1,"b":2},[],{}]"#;
        let table = root_table(source);
        let rows = rows(source, &table, 0, 10);

        assert_eq!(rows[0].kind, ValueKind::Array);
        assert_eq!(rows[0].children, Count::Exact(3));
        assert!(rows[0].expandable());

        assert_eq!(rows[1].kind, ValueKind::Object);
        assert_eq!(rows[1].children, Count::Exact(2));

        assert_eq!(rows[2].children, Count::Exact(0));
        assert!(!rows[2].expandable(), "an empty array has nothing to show");
        assert_eq!(rows[3].children, Count::Exact(0));
    }

    #[test]
    fn a_container_larger_than_the_budget_reports_a_lower_bound() {
        // The bounded-work rule: counting must never walk 400 MB to draw a row.
        let mut source = Vec::from(b"[[");
        for i in 0..5000 {
            if i > 0 {
                source.push(b',');
            }
            source.extend_from_slice(b"12345");
        }
        source.extend_from_slice(b"]]");

        let table = root_table(&source);
        let mut src = source.as_slice();
        let options = RowOptions {
            row_budget: 512,
            ..RowOptions::default()
        };
        let rows = materialize(&table, 0, 1, &mut src, &options).unwrap();

        assert_eq!(rows[0].kind, ValueKind::Array);
        assert!(
            !rows[0].children.is_exact(),
            "should have run out of budget"
        );
        assert!(rows[0].children.value() > 10, "but counted what it saw");
        assert_eq!(rows[0].value_end, None, "extent unknown without the close");
        assert!(rows[0].expandable());
    }

    #[test]
    fn a_container_within_budget_is_counted_exactly() {
        let source = br#"[[1,2,3,4,5]]"#;
        let table = root_table(source);
        assert_eq!(rows(source, &table, 0, 1)[0].children, Count::Exact(5));
    }

    #[test]
    fn an_object_members_container_is_counted_from_the_value_not_the_key() {
        // C44. Every test above this one used an array element, where the row's
        // offset and its value's offset are the same number — so counting from
        // the wrong one of the two was invisible. Here they differ, and counting
        // from the key reads `"items"` as a whole document and reports zero.
        let source = br#"{"items":[1,2,3],"meta":{"a":1,"b":2},"empty":[]}"#;
        let table = root_table(source);
        let rows = rows(source, &table, 0, 10);

        assert_eq!(rows[0].children, Count::Exact(3), "array under a key");
        assert_eq!(rows[1].children, Count::Exact(2), "object under a key");
        assert_eq!(rows[2].children, Count::Exact(0), "and an empty one");
        assert!(rows[0].expandable());
        assert!(!rows[2].expandable(), "nothing to expand into");

        // The extent is found too, which is what a viewer needs to select the
        // whole value.
        assert_eq!(rows[0].value_end, Some(16));
    }

    // ---- previews and escapes ---------------------------------------------

    #[test]
    fn previews_are_truncated_to_the_configured_width() {
        let long = "x".repeat(5000);
        let source = format!("[\"{long}\"]");
        let table = root_table(source.as_bytes());

        let mut src = source.as_bytes();
        let options = RowOptions {
            preview_chars: 10,
            ..RowOptions::default()
        };
        let rows = materialize(&table, 0, 1, &mut src, &options).unwrap();

        assert_eq!(rows[0].preview, "xxxxxxxxxx");
        assert!(rows[0].truncated);
        assert_eq!(rows[0].kind, ValueKind::String);
    }

    #[test]
    fn escapes_are_decoded_for_display() {
        let source = r#"["a\tb\nc","quote:\"","slash:\\","Aé"]"#.as_bytes();
        let table = root_table(source);
        let rows = rows(source, &table, 0, 10);

        assert_eq!(rows[0].preview, "a\tb\nc");
        assert_eq!(rows[1].preview, "quote:\"");
        assert_eq!(rows[2].preview, "slash:\\");
        assert_eq!(rows[3].preview, "Aé");
    }

    #[test]
    fn surrogate_pairs_become_one_character() {
        let source = "[\"🐋 leviathan\"]".as_bytes();
        let table = root_table(source);
        assert_eq!(rows(source, &table, 0, 1)[0].preview, "🐋 leviathan");
    }

    #[test]
    fn a_lone_surrogate_becomes_the_replacement_character() {
        // The recorded `i_` decision: display it, do not refuse the record.
        let source = br#"["\ud83d alone","\udc0b stray"]"#;
        let table = root_table(source);
        let rows = rows(source, &table, 0, 2);
        assert_eq!(rows[0].preview, "\u{FFFD} alone");
        assert_eq!(rows[1].preview, "\u{FFFD} stray");
    }

    #[test]
    fn keys_are_unescaped_too() {
        let source = br#"{"a\tb":1}"#;
        let table = root_table(source);
        assert_eq!(rows(source, &table, 0, 1)[0].key.as_deref(), Some("a\tb"));
    }

    #[test]
    fn multibyte_previews_count_characters_not_bytes() {
        let source = "[\"日本語のテキスト\"]".as_bytes();
        let table = root_table(source);
        let mut src = source;
        let options = RowOptions {
            preview_chars: 3,
            ..RowOptions::default()
        };
        let rows = materialize(&table, 0, 1, &mut src, &options).unwrap();
        assert_eq!(rows[0].preview, "日本語");
        assert!(rows[0].truncated);
    }

    // ---- NDJSON -----------------------------------------------------------

    #[test]
    fn ndjson_records_render_as_rows() {
        let source = b"{\"a\":1}\n[1,2]\n\"text\"\n42\n";
        let table = record_table(source);
        let rows = rows(source, &table, 0, 10);

        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].kind, ValueKind::Object);
        assert_eq!(rows[0].children, Count::Exact(1));
        assert_eq!(rows[1].kind, ValueKind::Array);
        assert_eq!(rows[1].children, Count::Exact(2));
        assert_eq!(rows[2].preview, "text");
        assert_eq!(rows[3].preview, "42");
        assert!(rows.iter().all(|r| r.key.is_none()), "records have no keys");
    }

    #[test]
    fn a_record_that_does_not_parse_renders_as_an_invalid_row() {
        // C6 in one test: the broken record does not take its neighbours with it.
        let source = b"{\"a\":1}\n{oops\n{\"c\":3}\n";
        let table = record_table(source);
        let rows = rows(source, &table, 0, 10);

        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].kind, ValueKind::Object);
        assert_eq!(
            rows[1].kind,
            ValueKind::Object,
            "it does start as an object"
        );
        assert!(!rows[1].children.is_exact(), "but never closes");
        assert_eq!(rows[2].kind, ValueKind::Object);
        assert_eq!(rows[2].children, Count::Exact(1));
    }

    // ---- windowing and ranges ---------------------------------------------

    #[test]
    fn a_middle_slice_is_addressed_directly() {
        // The C4 case: rows 900..903 without touching rows 0..900.
        let mut source = Vec::from(b"[");
        for i in 0..2000 {
            if i > 0 {
                source.push(b',');
            }
            source.extend_from_slice(format!("{i}").as_bytes());
        }
        source.push(b']');

        let table = root_table(&source);
        let rows = rows(&source, &table, 900, 3);
        let previews: Vec<&str> = rows.iter().map(|r| r.preview.as_str()).collect();
        assert_eq!(previews, ["900", "901", "902"]);
    }

    #[test]
    fn a_slice_past_the_end_is_empty_not_an_error() {
        let source = b"[1,2,3]";
        let table = root_table(source);
        assert!(rows(source, &table, 99, 10).is_empty());
        assert!(rows(source, &table, 0, 0).is_empty());
    }

    #[test]
    fn a_slice_straddling_the_end_returns_what_exists() {
        let source = b"[1,2,3]";
        let table = root_table(source);
        assert_eq!(rows(source, &table, 2, 50).len(), 1);
    }

    #[test]
    fn the_window_size_never_changes_the_rows() {
        // Windowing is an I/O optimization and must be invisible in the output,
        // including when a window is far smaller than the rows it covers.
        let mut source = Vec::from(b"[");
        for i in 0..200 {
            if i > 0 {
                source.push(b',');
            }
            source.extend_from_slice(format!("{{\"k{i}\":\"value number {i}\"}}").as_bytes());
        }
        source.push(b']');
        let table = root_table(&source);

        let reference = {
            let mut src = source.as_slice();
            materialize(&table, 0, 200, &mut src, &RowOptions::default()).unwrap()
        };
        assert_eq!(reference.len(), 200);

        for window in [1u32, 16, 64, 500, 4096, 1 << 20] {
            let options = RowOptions {
                window,
                row_budget: 64,
                ..RowOptions::default()
            };
            let mut src = source.as_slice();
            let got = materialize(&table, 0, 200, &mut src, &options).unwrap();
            assert_eq!(got.len(), reference.len(), "window {window}");
            for (a, b) in got.iter().zip(&reference) {
                assert_eq!(a.offset, b.offset, "window {window}");
                assert_eq!(a.kind, b.kind, "window {window}");
                assert_eq!(a.key, b.key, "window {window}");
            }
        }
    }

    #[test]
    fn the_last_row_of_a_file_materializes_without_running_off_the_end() {
        // The window asks for a budget past the last row, which at the bottom of
        // a file is past the end of the source. That is normal, not an error.
        let source = b"[1,2,3]";
        let table = root_table(source);
        let rows = rows(source, &table, 0, 3);
        assert_eq!(rows[2].preview, "3");
        // `3` is the byte at index 5, so its span ends at 6 — the `]`.
        assert_eq!(rows[2].value_end, Some(6));
        assert_eq!(&source[5..6], b"3");
    }

    #[test]
    fn a_string_larger_than_the_window_still_previews() {
        // The `bigstring` fixture case: no complete token fits, but the first
        // bytes are exactly what the preview needed anyway.
        let long = "y".repeat(100_000);
        let source = format!("[\"{long}\"]");
        let table = root_table(source.as_bytes());

        let mut src = source.as_bytes();
        let options = RowOptions {
            preview_chars: 20,
            row_budget: 256,
            window: 256,
        };
        let rows = materialize(&table, 0, 1, &mut src, &options).unwrap();
        assert_eq!(rows[0].kind, ValueKind::String);
        assert_eq!(rows[0].preview, "y".repeat(20));
        assert!(rows[0].truncated);
    }

    #[test]
    fn rows_are_empty_for_an_empty_table() {
        let table = ChildTable::new(None);
        let mut src: &[u8] = b"";
        assert!(
            materialize(&table, 0, 10, &mut src, &RowOptions::default())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn value_kinds_report_stable_names() {
        assert_eq!(ValueKind::Object.as_str(), "object");
        assert_eq!(ValueKind::Invalid.as_str(), "invalid");
        assert!(ValueKind::Array.is_container());
        assert!(!ValueKind::Number.is_container());
        assert_eq!(Count::AtLeast(7).value(), 7);
        assert!(!Count::AtLeast(7).is_exact());
        assert_eq!(Count::None.value(), 0);
    }
}
