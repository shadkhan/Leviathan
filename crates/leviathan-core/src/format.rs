//! Input format detection.
//!
//! Leviathan supports two shapes of input and must tell them apart before it
//! decides how to index: a single JSON document, or NDJSON / JSON-lines (one
//! independent document per line). The two get very different tier-1 indexes —
//! NDJSON needs only a line-offset table, which is why a 500 MB log file opens
//! almost instantly. See `DEEP_REASONING.md` C3.

/// The detected shape of an input stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Format {
    /// One JSON value spanning the whole input.
    SingleDocument,
    /// Newline-delimited JSON: one independent value per line.
    Ndjson,
    /// Input contains no non-whitespace bytes.
    Empty,
    /// Input does not begin with anything that can start a JSON value.
    Unknown,
}

impl Format {
    /// A stable lowercase identifier, used on the WASM boundary and in the CLI.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Format::SingleDocument => "single-document",
            Format::Ndjson => "ndjson",
            Format::Empty => "empty",
            Format::Unknown => "unknown",
        }
    }
}

const fn is_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\r' | b'\n')
}

/// Bytes that may legitimately follow a complete top-level scalar.
const fn terminates_scalar(b: u8) -> bool {
    is_ws(b) || matches!(b, b',' | b']' | b'}')
}

/// Could `rest` be the start of a JSON value?
///
/// Scalars are checked as whole tokens, not by their first byte. `n` starts
/// `null` but also `not json at all`; `2` starts `2` but also `2026-07-27 INFO
/// started`, which is what the first line of a great many log files looks like.
/// A viewer that cheerfully reports plain text as JSON is worse than one that
/// says it does not know, so a scalar counts only if it is both well-formed and
/// *terminated* — followed by whitespace, a structural byte, or nothing.
///
/// A token truncated by the end of the prefix (`tru` at the 64 KiB mark) is
/// accepted: we are looking at a window, not a document.
///
/// Note what this deliberately does not do: reject `true story`. That is a
/// complete JSON document followed by junk — a parse error, which M3 reports
/// with a byte offset, and not a question about which format to index as.
fn starts_value(rest: &[u8]) -> bool {
    match rest.first() {
        Some(b'{' | b'[' | b'"') => true,
        Some(b'-' | b'0'..=b'9') => scan_number(rest).is_some_and(|n| terminated_at(rest, n)),
        Some(b't') => literal_prefix(rest, b"true"),
        Some(b'f') => literal_prefix(rest, b"false"),
        Some(b'n') => literal_prefix(rest, b"null"),
        _ => false,
    }
}

/// Is a scalar of length `n` followed by something that could end it?
fn terminated_at(rest: &[u8], n: usize) -> bool {
    rest.get(n).is_none_or(|b| terminates_scalar(*b))
}

/// True if `rest` begins with `literal` (or is a truncation of it) and the
/// literal is not merely the head of a longer word — `nullable` is not `null`.
fn literal_prefix(rest: &[u8], literal: &[u8]) -> bool {
    let n = rest.len().min(literal.len());
    rest[..n] == literal[..n] && terminated_at(rest, literal.len())
}

/// Length of the RFC 8259 number at the start of `rest`, or `None` if there
/// isn't one.
///
/// A number cut off by the end of the window returns the length it reached,
/// which [`terminated_at`] then reads as "ran to the edge" and accepts.
fn scan_number(rest: &[u8]) -> Option<usize> {
    let mut i = 0;
    if rest.get(i) == Some(&b'-') {
        i += 1;
    }
    match rest.get(i) {
        None => return Some(i), // a lone `-` at the window edge
        Some(b'0') => i += 1,   // leading zeros are not a JSON number
        Some(b'1'..=b'9') => {
            while matches!(rest.get(i), Some(b'0'..=b'9')) {
                i += 1;
            }
        }
        Some(_) => return None, // `--`, `-x`, `- 1`
    }
    if rest.get(i) == Some(&b'.') {
        i += 1;
        i = scan_digits(rest, i)?;
    }
    if matches!(rest.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(rest.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        i = scan_digits(rest, i)?;
    }
    Some(i)
}

/// Consume one or more digits from `i`. `None` if there are none and the window
/// has not ended — `1.x` is not a number, but `1.` at the edge might be.
fn scan_digits(rest: &[u8], mut i: usize) -> Option<usize> {
    let start = i;
    while matches!(rest.get(i), Some(b'0'..=b'9')) {
        i += 1;
    }
    if i == start && i < rest.len() {
        return None;
    }
    Some(i)
}

/// Detect the format of an input from a prefix of its bytes.
///
/// Callers should pass a prefix (64 KiB is ample); passing the whole file is
/// supported but wasteful. Detection is intentionally cheap: it never parses.
///
/// # Heuristic, and its limits
///
/// A pretty-printed single document also contains newlines, so "has newlines"
/// proves nothing. The signal used here is *two or more lines that begin a new
/// JSON value at column 0*, which a pretty-printed document does not produce
/// (its continuation lines are indented, or begin with a key string inside an
/// already-open container).
///
/// This is deliberately a prefix heuristic and not a parse. It is replaced in M1
/// by the real answer — the streaming lexer knows when it has closed a top-level
/// value and can say with certainty whether another one follows. Until then, the
/// UI always offers a manual override.
///
/// # Examples
///
/// ```
/// # use leviathan_core::{sniff_format, Format};
/// assert_eq!(sniff_format(b"  \n\t "), Format::Empty);
/// assert_eq!(sniff_format(b"[1, 2, 3]"), Format::SingleDocument);
/// ```
#[must_use]
pub fn sniff_format(prefix: &[u8]) -> Format {
    // Skip a UTF-8 BOM if present — common in Windows-produced exports.
    let bytes = prefix.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(prefix);

    let Some(start) = bytes.iter().position(|b| !is_ws(*b)) else {
        return Format::Empty;
    };

    if !starts_value(&bytes[start..]) {
        return Format::Unknown;
    }

    let mut value_starting_lines = 0usize;
    for line in bytes.split(|b| *b == b'\n') {
        // A line that begins a value at column 0 (no leading indentation).
        if starts_value(line) {
            value_starting_lines += 1;
            if value_starting_lines >= 2 {
                return Format::Ndjson;
            }
        }
    }

    Format::SingleDocument
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_and_whitespace() {
        assert_eq!(sniff_format(b""), Format::Empty);
        assert_eq!(sniff_format(b" \t\r\n "), Format::Empty);
    }

    #[test]
    fn not_json() {
        assert_eq!(sniff_format(b"<html>"), Format::Unknown);
        assert_eq!(sniff_format(b"}"), Format::Unknown);
        assert_eq!(sniff_format(b"2026-07-27 INFO started"), Format::Unknown);
    }

    #[test]
    fn text_that_merely_starts_like_a_literal_is_not_json() {
        // The whole reason `starts_value` checks the literal and not the byte.
        assert_eq!(sniff_format(b"not json"), Format::Unknown);
        assert_eq!(sniff_format(b"true story"), Format::SingleDocument); // `true` then trailing junk: a parse error, not a format question
        assert_eq!(sniff_format(b"failed to connect"), Format::Unknown);
    }

    #[test]
    fn literals_are_recognised() {
        assert_eq!(sniff_format(b"null"), Format::SingleDocument);
        assert_eq!(sniff_format(b"true"), Format::SingleDocument);
        assert_eq!(sniff_format(b"false"), Format::SingleDocument);
    }

    #[test]
    fn a_literal_truncated_by_the_prefix_window_still_counts() {
        assert_eq!(sniff_format(b"fals"), Format::SingleDocument);
        assert_eq!(sniff_format(b"n"), Format::SingleDocument);
        assert_eq!(sniff_format(b"-"), Format::SingleDocument);
        assert_eq!(sniff_format(b"1."), Format::SingleDocument);
        assert_eq!(sniff_format(b"1e-"), Format::SingleDocument);
    }

    #[test]
    fn a_literal_that_is_the_head_of_a_word_is_not_a_value() {
        assert_eq!(sniff_format(b"nullable"), Format::Unknown);
        assert_eq!(sniff_format(b"truent"), Format::Unknown);
    }

    #[test]
    fn timestamped_log_lines_are_not_json() {
        // The motivating case: engineers drag log files onto a JSON viewer, and
        // every common log format opens with a digit.
        assert_eq!(sniff_format(b"2026-07-27 INFO started"), Format::Unknown);
        assert_eq!(
            sniff_format(b"2026-07-27T09:15:00Z ERROR boom\n2026-07-27T09:15:01Z INFO ok\n"),
            Format::Unknown
        );
        assert_eq!(
            sniff_format(b"127.0.0.1 - - [27/Jul/2026]"),
            Format::Unknown
        );
    }

    #[test]
    fn malformed_numbers_are_not_values() {
        assert_eq!(sniff_format(b"1.2.3"), Format::Unknown);
        assert_eq!(sniff_format(b"-- a comment"), Format::Unknown);
        assert_eq!(sniff_format(b"12abc"), Format::Unknown);
    }

    #[test]
    fn well_formed_numbers_are_values() {
        for input in [
            &b"0"[..],
            b"-0",
            b"42",
            b"3.14",
            b"1e10",
            b"-2.5E-3",
            b"0.0",
        ] {
            assert_eq!(
                sniff_format(input),
                Format::SingleDocument,
                "{}",
                String::from_utf8_lossy(input)
            );
        }
    }

    #[test]
    fn single_documents() {
        assert_eq!(sniff_format(br#"{"a":1}"#), Format::SingleDocument);
        assert_eq!(sniff_format(b"[1,2,3]"), Format::SingleDocument);
        assert_eq!(sniff_format(b"  \n  42"), Format::SingleDocument);
        assert_eq!(sniff_format(b"\"just a string\""), Format::SingleDocument);
    }

    #[test]
    fn pretty_printed_document_is_not_ndjson() {
        let pretty = b"{\n  \"a\": 1,\n  \"b\": [\n    2,\n    3\n  ]\n}\n";
        assert_eq!(sniff_format(pretty), Format::SingleDocument);
    }

    #[test]
    fn ndjson_variants() {
        assert_eq!(sniff_format(b"{\"a\":1}\n{\"a\":2}\n"), Format::Ndjson);
        assert_eq!(sniff_format(b"[1]\n[2]\n[3]"), Format::Ndjson);
        assert_eq!(sniff_format(b"1\n2\n3\n"), Format::Ndjson);
    }

    #[test]
    fn known_limit_an_unindented_array_is_indistinguishable_from_ndjson() {
        // Recorded, not hidden. A top-level array whose elements begin at
        // column 0 produces exactly the byte pattern this heuristic uses to
        // recognise NDJSON, so a prefix window genuinely cannot tell them
        // apart — the difference is a single `[` and a trailing `]` that may be
        // 500 MB away.
        //
        // Why this is acceptable for now:
        //  - Every mainstream pretty-printer indents array elements, so real
        //    files hit `pretty_printed_document_is_not_ndjson` instead.
        //  - The consequence is a wrong tier-1 index choice, not a wrong parse.
        //  - M1's streaming lexer answers it exactly: it knows when it has
        //    closed a top-level value, so it can say whether another follows.
        //    Until then the UI offers a manual override.
        let unindented = b"[\n{\"a\":1},\n{\"a\":2}\n]";
        assert_eq!(sniff_format(unindented), Format::Ndjson);

        // The indented form — what real tools emit — is read correctly.
        let indented = b"[\n  {\"a\":1},\n  {\"a\":2}\n]";
        assert_eq!(sniff_format(indented), Format::SingleDocument);

        // As is the minified form.
        assert_eq!(
            sniff_format(b"[{\"a\":1},{\"a\":2}]"),
            Format::SingleDocument
        );
    }

    #[test]
    fn bom_is_ignored() {
        let mut input = vec![0xEF, 0xBB, 0xBF];
        input.extend_from_slice(br#"{"a":1}"#);
        assert_eq!(sniff_format(&input), Format::SingleDocument);
    }

    #[test]
    fn single_ndjson_line_reads_as_single_document() {
        // One record is indistinguishable from a document, and treating it as a
        // document is the harmless answer: the tree looks identical.
        assert_eq!(sniff_format(b"{\"a\":1}\n"), Format::SingleDocument);
    }
}
