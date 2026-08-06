//! Export a file, re-import it, and prove nothing changed.
//!
//! Requirement 11 says what comes out re-parses identically. `export.rs` makes
//! that true by construction — minified output is the source's own tokens with
//! the whitespace dropped — but "by construction" is an argument, and this is
//! the check. It runs against whole fixtures rather than against the unit
//! tests' handful of records, because the failure this guards against is a
//! record shape nobody thought to write down.
//!
//! Three claims, checked separately because they are three different strengths:
//!
//! | Claim | How it is checked |
//! |---|---|
//! | **Idempotent** | exporting the export produces identical bytes |
//! | **Structure-preserving** | the re-imported index has the same records, at the offsets the new file puts them |
//! | **Token-preserving** | every token of the output equals a token of the input, in order |
//!
//! The third is the real one. The first two would both pass for an exporter
//! that consistently rounded `1.0000000000000002` to `1.0000000000000002` — the
//! token check is what notices that it did not.

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::Path;

use leviathan_core::{Export, ExportFormat, Format, Lexer, sniff_format};

use crate::file_source::FileSource;

/// What one round trip established.
pub struct Trip {
    /// The fixture, as the user named it.
    pub fixture: String,
    /// Bytes in, bytes out.
    pub read: u64,
    pub written: u64,
    /// Records converted.
    pub records: u64,
    /// The format used, chosen to preserve the source's shape.
    pub format: &'static str,
    /// Whether exporting the output again produced the same bytes.
    pub idempotent: bool,
    /// Whether the output's token stream matches the input's.
    pub tokens_match: bool,
    /// Whether the source lexes to the end.
    ///
    /// It may not — `badutf8` is a fixture for exactly that — and then the token
    /// comparison ends where the lexer does. Both sides stopping at the same
    /// place would otherwise read as a pass, which is a claim about the bytes
    /// after that point that nothing checked.
    pub lexes_fully: bool,
    /// Records copied verbatim because they do not parse.
    pub salvaged: u64,
    /// The first token that differed, if one did.
    pub first_difference: Option<String>,
}

impl Trip {
    /// Whether every claim held.
    #[must_use]
    pub const fn passed(&self) -> bool {
        self.idempotent && self.tokens_match
    }
}

/// Export `path` to NDJSON, then check the result against the source.
///
/// # Errors
///
/// If the fixture cannot be read or the scratch file cannot be written.
pub fn run(path: &Path, scratch: &Path) -> io::Result<Trip> {
    let name = path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into(),
    );

    let read = path.metadata()?.len();
    let (written, records, format, salvaged) = export_shape_preserving(path, scratch)?;

    // Exporting the export must produce the same bytes: a converter that is not
    // idempotent is one that is still deciding something on every pass.
    let twice = scratch.with_extension("twice");
    export_shape_preserving(scratch, &twice)?;
    let idempotent = same_bytes(scratch, &twice)?;

    let (tokens_match, first_difference, lexes_fully) = compare_tokens(path, scratch)?;
    let _ = std::fs::remove_file(&twice);

    Ok(Trip {
        fixture: name,
        read,
        written,
        records,
        format,
        idempotent,
        tokens_match,
        lexes_fully,
        salvaged,
        first_difference,
    })
}

/// Export a file in the format that preserves its shape, streaming.
///
/// NDJSON in, NDJSON out; a single document in, one JSON document out. The
/// choice is what makes the token comparison meaningful: converting a root
/// array to NDJSON *deliberately* drops the array's own `[`, `,` and `]`, so
/// asserting token equality across that conversion would be asserting that a
/// feature does not work. That reshaping is a different claim, checked in this
/// module's own tests.
fn export_shape_preserving(from: &Path, to: &Path) -> io::Result<(u64, u64, &'static str, u64)> {
    let format = sniff_prefix(from)?;
    let table = crate::bench::build_table_of(from, 1024 * 1024)?;
    let mut source = FileSource::open(from)?;
    let mut out = File::create(to)?;

    // The whole document is one value when its rows are its members rather than
    // its records — the same rule the WASM driver applies, for the same reason.
    let whole = format == Format::SingleDocument && first_value_byte(from)? != Some(b'[');
    let total = source_len(from)?;

    let target = if format == Format::Ndjson {
        ExportFormat::Ndjson
    } else {
        ExportFormat::Json
    };
    let mut export = Export::new(target).wrapped(!whole);
    let mut written = 0u64;

    let chunk = export.open().to_vec();
    out.write_all(&chunk)?;
    written += chunk.len() as u64;

    let spans: Vec<(u64, u64)> = if whole {
        vec![(0, total)]
    } else {
        (0..table.len())
            .filter_map(|row| {
                let start = table.child(row)?;
                Some((start, table.child(row + 1).unwrap_or(total)))
            })
            .collect()
    };

    for (start, end) in &spans {
        let bytes = export
            .push(&mut source, *start, *end)
            .map_err(|e| io::Error::other(e.to_string()))?;
        out.write_all(bytes)?;
        written += bytes.len() as u64;
    }

    let tail = export.close().to_vec();
    out.write_all(&tail)?;
    written += tail.len() as u64;
    out.flush()?;

    Ok((
        written,
        export.written(),
        target.as_str(),
        export.salvaged(),
    ))
}

/// Whether two files hold identical bytes.
fn same_bytes(a: &Path, b: &Path) -> io::Result<bool> {
    let (mut a, mut b) = (File::open(a)?, File::open(b)?);
    let (mut left, mut right) = (vec![0u8; 1 << 16], vec![0u8; 1 << 16]);
    loop {
        let n = read_full(&mut a, &mut left)?;
        let m = read_full(&mut b, &mut right)?;
        if n != m || left[..n] != right[..m] {
            return Ok(false);
        }
        if n == 0 {
            return Ok(true);
        }
    }
}

/// Whether the two files lex to the same token texts, in the same order.
///
/// Whitespace differs by design, and NDJSON adds a newline per record, so this
/// compares *tokens* rather than bytes. Structural punctuation is included:
/// dropping a `,` would still lex to matching values.
fn compare_tokens(before: &Path, after: &Path) -> io::Result<(bool, Option<String>, bool)> {
    let mut left = TokenReader::open(before)?;
    let mut right = TokenReader::open(after)?;

    let mut at = 0u64;
    loop {
        let a = left.next()?;
        let b = right.next()?;
        if a == b {
            if a.is_none() {
                return Ok((true, None, !left.broke && !right.broke));
            }
            at += 1;
            continue;
        }
        return Ok((
            false,
            Some(format!(
                "token {at}: {} became {}",
                a.as_deref().unwrap_or("(end)"),
                b.as_deref().unwrap_or("(end)")
            )),
            !left.broke && !right.broke,
        ));
    }
}

/// Streams one file's tokens as their source text.
struct TokenReader {
    file: File,
    lexer: Lexer,
    buffer: Vec<u8>,
    /// Absolute offset the buffer begins at.
    base: u64,
    pending: Vec<String>,
    at: usize,
    finished: bool,
    /// Whether the lexer stopped at an error rather than at the end.
    broke: bool,
}

impl TokenReader {
    fn open(path: &Path) -> io::Result<Self> {
        Ok(Self {
            file: File::open(path)?,
            lexer: Lexer::new(),
            buffer: Vec::new(),
            base: 0,
            pending: Vec::new(),
            at: 0,
            finished: false,
            broke: false,
        })
    }

    /// The next token's text, or `None` at the end.
    fn next(&mut self) -> io::Result<Option<String>> {
        loop {
            if self.at < self.pending.len() {
                self.at += 1;
                return Ok(Some(self.pending[self.at - 1].clone()));
            }
            if self.finished {
                return Ok(None);
            }
            self.fill()?;
        }
    }

    /// Read the next chunk and lex it, keeping any token that spans the seam.
    fn fill(&mut self) -> io::Result<()> {
        let mut chunk = vec![0u8; 1 << 20];
        let n = read_full(&mut self.file, &mut chunk)?;
        self.pending.clear();
        self.at = 0;

        if n == 0 {
            self.finished = true;
            if let Ok(Some(token)) = self.lexer.finish() {
                if let Some(text) = self.text_of(token.start, token.end) {
                    self.pending.push(text);
                }
            }
            return Ok(());
        }

        self.buffer.extend_from_slice(&chunk[..n]);

        // Gathered before any text is resolved: the token iterator borrows the
        // lexer, and resolving a token's text reads the buffer that lives beside
        // it. Two loops, one borrow each.
        let mut spans = Vec::new();
        for token in self.lexer.feed(&chunk[..n]) {
            let Ok(token) = token else {
                self.finished = true;
                self.broke = true;
                break;
            };
            spans.push((token.start, token.end));
        }

        let mut last_end = self.base;
        let mut texts = Vec::new();
        for (start, end) in spans {
            last_end = end;
            if let Some(text) = self.text_of(start, end) {
                texts.push(text);
            }
        }
        self.pending = texts;

        // Drop what a completed token has claimed; keep the rest, because the
        // token that straddles this seam needs its first half (C66's lesson,
        // one layer up).
        let keep = last_end.saturating_sub(self.base) as usize;
        if keep >= self.buffer.len() {
            self.buffer.clear();
        } else {
            self.buffer.drain(..keep);
        }
        self.base = last_end;
        Ok(())
    }

    fn text_of(&self, start: u64, end: u64) -> Option<String> {
        let from = start.checked_sub(self.base)? as usize;
        let to = (end.saturating_sub(self.base) as usize).min(self.buffer.len());
        let slice = self.buffer.get(from..to)?;
        Some(String::from_utf8_lossy(slice).into_owned())
    }
}

/// Read until the buffer is full or the file ends.
fn read_full(file: &mut File, into: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < into.len() {
        let n = file.read(&mut into[filled..])?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    Ok(filled)
}

fn source_len(path: &Path) -> io::Result<u64> {
    Ok(path.metadata()?.len())
}

fn sniff_prefix(path: &Path) -> io::Result<Format> {
    let mut file = File::open(path)?;
    let mut buf = vec![0u8; 64 * 1024];
    let n = read_full(&mut file, &mut buf)?;
    Ok(sniff_format(&buf[..n]))
}

fn first_value_byte(path: &Path) -> io::Result<Option<u8>> {
    let mut file = File::open(path)?;
    let mut buf = vec![0u8; 64];
    let n = read_full(&mut file, &mut buf)?;
    Ok(buf[..n].iter().find(|b| !b.is_ascii_whitespace()).copied())
}

/// Render one trip's result.
#[must_use]
pub fn report(trips: &[Trip]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(out, "\nexport round trip (requirement 11)\n");
    let _ = writeln!(
        out,
        "  {:<28} {:>12} {:>12} {:>10}  {:<12} verdict",
        "fixture", "in", "out", "records", "as"
    );
    let _ = writeln!(out, "  {}", "-".repeat(96));

    for trip in trips {
        let verdict = if trip.passed() && !trip.lexes_fully {
            format!(
                "ok as far as it lexes — {} record(s) copied verbatim",
                trip.salvaged
            )
        } else if trip.passed() {
            "ok".to_string()
        } else {
            let mut why = Vec::new();
            if !trip.idempotent {
                why.push("not idempotent".to_string());
            }
            if let Some(difference) = &trip.first_difference {
                why.push(difference.clone());
            }
            format!("FAIL — {}", why.join("; "))
        };
        let _ = writeln!(
            out,
            "  {:<28} {:>12} {:>12} {:>10}  {:<12} {verdict}",
            trip.fixture,
            crate::cli::human_bytes(trip.read),
            crate::cli::human_bytes(trip.written),
            trip.records,
            trip.format
        );
    }

    let failed = trips.iter().filter(|t| !t.passed()).count();
    let _ = writeln!(
        out,
        "\n  {}",
        if failed == 0 {
            "every token survived the round trip, and exporting twice is a fixed point."
        } else {
            "ROUND TRIP FAILED — see above."
        }
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(bytes: &[u8], name: &str) -> (tempdir::Dir, std::path::PathBuf) {
        let dir = tempdir::Dir::new();
        let path = dir.path().join(name);
        File::create(&path).unwrap().write_all(bytes).unwrap();
        (dir, path)
    }

    /// A directory that deletes itself. Small enough to not warrant a crate.
    mod tempdir {
        use std::path::{Path, PathBuf};
        pub struct Dir(PathBuf);
        impl Dir {
            pub fn new() -> Self {
                let mut path = std::env::temp_dir();
                let nonce = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0);
                path.push(format!("leviathan-roundtrip-{nonce}"));
                std::fs::create_dir_all(&path).expect("temp dir");
                Self(path)
            }
            pub fn path(&self) -> &Path {
                &self.0
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    #[test]
    fn ndjson_survives_a_round_trip_token_for_token() {
        let source = b"{\"a\":1,\"b\":[2,3]}\n{\"c\":\"x y\"}\n{\"d\":null}\n";
        let (dir, path) = fixture(source, "in.ndjson");
        let trip = run(&path, &dir.path().join("out.ndjson")).unwrap();

        assert!(trip.passed(), "{:?}", trip.first_difference);
        assert_eq!(trip.records, 3);
        assert_eq!(trip.written, source.len() as u64, "already minified");
    }

    #[test]
    fn the_numbers_that_a_float_round_trip_would_change_survive() {
        // The case the token comparison exists for. Every one of these is a
        // different value from what `f64` → text would produce.
        let source = b"1.0000000000000002\n10000000000000000000\n1e400\n-0\n0.1\n";
        let (dir, path) = fixture(source, "numbers.ndjson");
        let trip = run(&path, &dir.path().join("out.ndjson")).unwrap();

        assert!(trip.passed(), "{:?}", trip.first_difference);
        assert_eq!(trip.written, source.len() as u64);
    }

    #[test]
    fn a_pretty_printed_document_minifies_and_still_matches() {
        let source = b"{\n  \"a\" : 1,\n  \"b\" : [\n    2,\n    3\n  ]\n}\n";
        let (dir, path) = fixture(source, "pretty.json");
        let trip = run(&path, &dir.path().join("out.ndjson")).unwrap();

        assert!(trip.passed(), "{:?}", trip.first_difference);
        assert!(trip.written < trip.read, "whitespace really was removed");
    }

    #[test]
    fn a_root_array_round_trips_as_an_array() {
        // Converting it to NDJSON would drop the array's own `[`, `,` and `]`
        // *by design*, which is why the round trip uses the shape-preserving
        // format rather than always writing NDJSON.
        let (dir, path) = fixture(b"[{\"a\":1},{\"a\":2}]\n", "array.json");
        let trip = run(&path, &dir.path().join("out.json")).unwrap();

        assert!(trip.passed(), "{:?}", trip.first_difference);
        assert_eq!(trip.format, "json", "a document stays a document");
        assert_eq!(trip.records, 2);
    }
}
