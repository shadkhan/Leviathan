//! Plain substring search over the source bytes, streamed and resumable.
//!
//! ## Why this is not the query engine
//!
//! JSONPath answers "give me every `status` field". Find answers "where does
//! this string occur", and it is the tool you reach for *first*, on a file whose
//! shape you do not know yet — which is exactly the moment Leviathan is opened
//! (`USER_PERSONAS.md`, Priya). One is a language over structure; the other is a
//! scan over bytes. Building find on the query engine would mean waiting for the
//! query engine, and would answer a different question.
//!
//! ## Why it scans the file rather than the index
//!
//! The tempting shortcut is to search what has already been materialized — the
//! rows the UI is holding. It would be fast, trivial, and wrong: tier 1 records
//! where each record *starts* (C3), and a materialized row holds a **truncated
//! preview** of it (C33). Searching those would quietly search the first eighty
//! characters of each record and report "no matches" for a string that is in the
//! file. A find that can miss is worse than no find, because the user believes
//! it.
//!
//! So this reads the bytes. On the 500 MB fixture that is the same order of work
//! as tier-1 indexing, which runs at memory bandwidth (C27) — the scan is
//! I/O-bound, not compare-bound, which is also why the matcher below is a naive
//! two-stage loop rather than something cleverer.
//!
//! ## What it matches
//!
//! Raw file bytes, like `grep` — not decoded JSON text. A search for `a"b` finds
//! the two-byte escape `\"` written out, and a search for a literal newline
//! finds nothing inside a string, because JSON writes it as `\n` (C21). This is
//! a limitation and it is the honest one: decoding every string in a 500 MB file
//! to search it is the whole cost the product exists to avoid, and a search that
//! silently searched something other than the file would be worse.
//!
//! ASCII case folding only. `Å` never equals `å` here, because Unicode case
//! folding is a table, and a table is a dependency (C9) and a `.wasm` size
//! regression, to serve a case no one has yet asked for.

use crate::index::ChildTable;
use crate::source::{ByteRange, SourceError, read_clamped};

/// How much a single [`Find::advance`] call reads before returning.
///
/// Sized like [`ExpandOptions`](crate::ExpandOptions): large enough that the
/// per-call overhead disappears, small enough that the Worker can paint a frame,
/// report progress, or be cancelled between calls (C39).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindOptions {
    /// Bytes per read from the source.
    pub window: u32,
    /// Bytes to scan before returning from `advance`.
    pub budget: u64,
    /// Stop after this many matches.
    ///
    /// A search for `,` on a 500 MB file has a hundred million answers, and
    /// neither the boundary nor the user has any use for them. The cap is what
    /// makes "find anything" a safe thing to type.
    pub limit: usize,
}

impl Default for FindOptions {
    fn default() -> Self {
        Self {
            window: 256 * 1024,
            budget: 8 * 1024 * 1024,
            limit: 10_000,
        }
    }
}

/// Why a search stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindStop {
    /// The end of the source was reached. Every match has been found.
    Exhausted,
    /// [`FindOptions::limit`] matches were collected; there may be more.
    Limited,
    /// An empty needle. Nothing to look for, so nothing is scanned.
    Empty,
}

/// A resumable search over a source.
///
/// Holds the needle, where it has read up to, and the matches so far — all
/// fixed-size except the match list, which is capped. Nothing about it is
/// proportional to the size of the file being searched.
#[derive(Debug, Clone)]
pub struct Find {
    needle: Vec<u8>,
    fold: bool,
    at: u64,
    scanned: u64,
    matches: Vec<u64>,
    stopped: Option<FindStop>,
}

impl Find {
    /// A search for `needle`, positioned at the start of the source.
    ///
    /// `case_sensitive` false folds ASCII only — see the module docs.
    #[must_use]
    pub fn new(needle: &str, case_sensitive: bool) -> Self {
        let fold = !case_sensitive;
        let bytes = if fold {
            needle.as_bytes().to_ascii_lowercase()
        } else {
            needle.as_bytes().to_vec()
        };

        Self {
            stopped: bytes.is_empty().then_some(FindStop::Empty),
            needle: bytes,
            fold,
            at: 0,
            scanned: 0,
            matches: Vec::new(),
        }
    }

    /// Start the search at a byte other than zero.
    ///
    /// Used for "find next from here": the UI knows where the selection is, and
    /// resuming from it beats scanning the file again and discarding the front.
    #[must_use]
    pub const fn from(mut self, offset: u64) -> Self {
        self.at = offset;
        self
    }

    /// Byte offsets of the matches found so far, ascending.
    #[must_use]
    pub fn matches(&self) -> &[u64] {
        &self.matches
    }

    /// How many bytes have been read.
    #[must_use]
    pub const fn scanned(&self) -> u64 {
        self.scanned
    }

    /// The next byte that will be examined.
    #[must_use]
    pub const fn position(&self) -> u64 {
        self.at
    }

    /// Why the search stopped, or `None` if it can still be advanced.
    #[must_use]
    pub const fn stopped(&self) -> Option<FindStop> {
        self.stopped
    }

    /// Whether every match in the source has been found.
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        matches!(self.stopped, Some(FindStop::Exhausted | FindStop::Empty))
    }

    /// Scan up to [`FindOptions::budget`] more bytes.
    ///
    /// Returns the number of bytes scanned by this call — zero once the search
    /// has stopped, so a caller that keeps going gets a terminating loop rather
    /// than a spin.
    ///
    /// # Errors
    ///
    /// Propagates [`SourceError`] from the underlying source. A short read at
    /// the end of the file is not an error (C40); it is the end of the search.
    pub fn advance<S: ByteRange>(
        &mut self,
        source: &mut S,
        options: &FindOptions,
    ) -> Result<u64, SourceError> {
        if self.stopped.is_some() {
            return Ok(0);
        }

        // A match may straddle two reads, so each window re-reads the last
        // `needle.len() - 1` bytes of the one before it. That overlap is why the
        // window must be larger than the needle — otherwise the scan would
        // advance zero bytes per read and never terminate.
        let overlap = (self.needle.len() - 1) as u64;
        let window = u64::from(options.window).max(overlap * 2 + 1);

        let start_scanned = self.scanned;
        let mut budget = options.budget;

        while budget > 0 {
            let bytes = read_clamped(source, self.at, window)?;
            let read = bytes.len() as u64;

            if read < self.needle.len() as u64 {
                // Not enough left for the needle to fit: the file ends here.
                self.at = self.at.saturating_add(read);
                self.scanned = self.scanned.saturating_add(read);
                self.stopped = Some(FindStop::Exhausted);
                break;
            }

            let base = self.at;
            for position in Matches::new(bytes, &self.needle, self.fold) {
                if self.matches.len() >= options.limit {
                    self.stopped = Some(FindStop::Limited);
                    break;
                }
                self.matches.push(base + position as u64);
            }

            if self.stopped.is_some() {
                break;
            }

            // The tail that could still begin a match is re-read next time, so
            // progress is `read - overlap` rather than `read`.
            let last_window = read < window;
            let step = read - overlap;
            self.at += step;
            self.scanned = self.scanned.saturating_add(step);
            budget = budget.saturating_sub(step);

            if last_window {
                // The overlap we deliberately did not consume has been searched
                // already — this window ran to the end of the source.
                self.at += overlap;
                self.scanned = self.scanned.saturating_add(overlap);
                self.stopped = Some(FindStop::Exhausted);
                break;
            }
        }

        Ok(self.scanned - start_scanned)
    }

    /// Run to completion. Convenience for tests and for the CLI.
    ///
    /// # Errors
    ///
    /// Propagates [`SourceError`] from the underlying source.
    pub fn run_to_end<S: ByteRange>(
        &mut self,
        source: &mut S,
        options: &FindOptions,
    ) -> Result<(), SourceError> {
        while self.stopped.is_none() {
            self.advance(source, options)?;
        }
        Ok(())
    }
}

/// Which row of a table each match falls in.
///
/// A byte offset is where the *engine* thinks; a row is where the *user* thinks.
/// This is the join between them, and it is why a find result can be clicked.
///
/// Matches before the first row — leading whitespace, or a document's opening
/// `[` — have no row and are dropped rather than being attributed to row 0,
/// which would put the user somewhere the string is not.
#[must_use]
pub fn rows_of(table: &ChildTable, matches: &[u64]) -> Vec<usize> {
    matches.iter().filter_map(|&at| table.locate(at)).collect()
}

/// Non-overlapping occurrences of `needle` in `hay`.
///
/// Two stages: skip to a byte that could start a match, then verify. Naive by
/// design — the scan is bounded by how fast bytes arrive from a `Blob.slice`,
/// not by how fast they are compared, so a smarter matcher would optimize the
/// half of the work that is already free while costing a dependency (C9).
struct Matches<'a> {
    hay: &'a [u8],
    needle: &'a [u8],
    fold: bool,
    at: usize,
}

impl<'a> Matches<'a> {
    fn new(hay: &'a [u8], needle: &'a [u8], fold: bool) -> Self {
        Self {
            hay,
            needle,
            fold,
            at: 0,
        }
    }
}

impl Iterator for Matches<'_> {
    type Item = usize;

    fn next(&mut self) -> Option<usize> {
        let first = self.needle[0];
        let last = self.hay.len().checked_sub(self.needle.len())?;

        while self.at <= last {
            if same(self.hay[self.at], first, self.fold)
                && self.hay[self.at..]
                    .iter()
                    .zip(self.needle)
                    .all(|(&h, &n)| same(h, n, self.fold))
            {
                let found = self.at;
                self.at += self.needle.len();
                return Some(found);
            }
            self.at += 1;
        }
        None
    }
}

/// Byte equality, optionally folding ASCII case.
///
/// `needle` is already lowercased when folding, so only the haystack byte needs
/// touching — one branch per byte on the scanning path rather than two.
const fn same(hay: u8, needle: u8, fold: bool) -> bool {
    if fold {
        hay.to_ascii_lowercase() == needle
    } else {
        hay == needle
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find_all(source: &[u8], needle: &str, options: &FindOptions) -> Vec<u64> {
        let mut find = Find::new(needle, true);
        let mut bytes = source;
        find.run_to_end(&mut bytes, options).unwrap();
        find.matches().to_vec()
    }

    fn tiny(window: u32, budget: u64) -> FindOptions {
        FindOptions {
            window,
            budget,
            ..FindOptions::default()
        }
    }

    #[test]
    fn finds_every_occurrence_with_its_byte_offset() {
        let source = b"alpha beta alpha gamma alpha";
        assert_eq!(
            find_all(source, "alpha", &FindOptions::default()),
            [0, 11, 23]
        );
        for at in find_all(source, "alpha", &FindOptions::default()) {
            assert_eq!(&source[at as usize..at as usize + 5], b"alpha");
        }
    }

    #[test]
    fn a_needle_straddling_a_window_boundary_is_still_found() {
        // The property the overlap exists for. `leviathan` is 9 bytes, so with a
        // window of 8 no single read can contain it.
        let source = b"xxxxxxxleviathanxxxxxxx";
        for window in 1u32..40 {
            assert_eq!(
                find_all(source, "leviathan", &tiny(window, 1 << 20)),
                [7],
                "window {window}"
            );
        }
    }

    #[test]
    fn the_window_and_budget_never_change_the_answer() {
        let mut source = Vec::new();
        for i in 0..500 {
            source.extend_from_slice(format!("{{\"id\":{i},\"tag\":\"needle\"}}\n").as_bytes());
        }
        let reference = find_all(&source, "needle", &FindOptions::default());
        assert_eq!(reference.len(), 500);

        for window in [1u32, 2, 7, 64, 4096] {
            for budget in [1u64, 3, 100, 1 << 20] {
                assert_eq!(
                    find_all(&source, "needle", &tiny(window, budget)),
                    reference,
                    "window {window}, budget {budget}"
                );
            }
        }
    }

    #[test]
    fn advancing_yields_and_resumes_exactly_where_it_stopped() {
        // The C39 property, for search: a 500 MB scan must be interruptible, so
        // stopping and continuing has to be indistinguishable from not stopping.
        let mut source = Vec::new();
        for i in 0..2000 {
            source.extend_from_slice(format!("record {i} needle\n").as_bytes());
        }

        let options = tiny(1024, 4096);
        let mut find = Find::new("needle", true);
        let mut bytes = source.as_slice();
        let mut calls = 0;
        while find.stopped().is_none() {
            find.advance(&mut bytes, &options).unwrap();
            calls += 1;
            assert!(calls < 10_000, "advance must make progress every call");
        }

        assert!(calls > 5, "expected several batches, got {calls}");
        assert_eq!(find.matches().len(), 2000);
        assert_eq!(find.stopped(), Some(FindStop::Exhausted));
        assert!(find.is_complete());
    }

    #[test]
    fn matches_do_not_overlap() {
        // `aa` in `aaaa` is two matches, not three. Overlapping counts are
        // technically defensible and always surprising.
        assert_eq!(find_all(b"aaaa", "aa", &FindOptions::default()), [0, 2]);
    }

    #[test]
    fn case_folding_is_ascii_and_opt_in() {
        let source: &[u8] = b"Leviathan LEVIATHAN leviathan";

        let mut sensitive = Find::new("leviathan", true);
        let mut bytes = source;
        sensitive
            .run_to_end(&mut bytes, &FindOptions::default())
            .unwrap();
        assert_eq!(sensitive.matches(), [20]);

        let mut folded = Find::new("LeViAtHaN", false);
        let mut bytes = source;
        folded
            .run_to_end(&mut bytes, &FindOptions::default())
            .unwrap();
        assert_eq!(folded.matches(), [0, 10, 20]);
    }

    #[test]
    fn the_limit_stops_the_scan_and_says_so() {
        let source = vec![b'a'; 10_000];
        let options = FindOptions {
            limit: 25,
            ..FindOptions::default()
        };
        let mut find = Find::new("a", true);
        let mut bytes = source.as_slice();
        find.run_to_end(&mut bytes, &options).unwrap();

        assert_eq!(find.matches().len(), 25);
        assert_eq!(find.stopped(), Some(FindStop::Limited));
        assert!(!find.is_complete(), "a capped search has not seen the file");
    }

    #[test]
    fn an_empty_needle_finds_nothing_rather_than_everything() {
        let mut find = Find::new("", true);
        let mut bytes: &[u8] = b"anything at all";
        assert_eq!(
            find.advance(&mut bytes, &FindOptions::default()).unwrap(),
            0
        );
        assert_eq!(find.matches(), [] as [u64; 0]);
        assert_eq!(find.stopped(), Some(FindStop::Empty));
    }

    #[test]
    fn a_needle_longer_than_the_file_is_not_a_panic() {
        assert_eq!(
            find_all(b"ab", "abcdefghij", &FindOptions::default()),
            [] as [u64; 0]
        );
        assert_eq!(find_all(b"", "a", &FindOptions::default()), [] as [u64; 0]);
    }

    #[test]
    fn a_match_at_the_very_last_byte_is_found() {
        // The end-of-source path skips the overlap it has already searched; an
        // off-by-one there loses the last needle in the file, on every file.
        for tail in ["x", "xy", "xyz"] {
            let source = format!("{}{tail}", "-".repeat(500));
            assert_eq!(
                find_all(source.as_bytes(), tail, &tiny(64, 1 << 20)),
                [500],
                "tail {tail}"
            );
        }
    }

    #[test]
    fn searching_can_start_partway_through() {
        let source: &[u8] = b"needle ... needle ... needle";
        let mut find = Find::new("needle", true).from(7);
        let mut bytes = source;
        find.run_to_end(&mut bytes, &FindOptions::default())
            .unwrap();
        assert_eq!(find.matches(), [11, 22]);
    }

    #[test]
    fn matches_resolve_to_the_row_that_contains_them() {
        use crate::index::RecordScanner;

        let source = b"{\"a\":1}\n{\"a\":2}\n{\"a\":3}\n";
        let mut scanner = RecordScanner::new();
        scanner.feed(source);
        let table = scanner.finish();

        let matches = find_all(source, "\"a\":", &FindOptions::default());
        assert_eq!(matches, [1, 9, 17]);
        assert_eq!(rows_of(&table, &matches), [0, 1, 2]);
    }

    #[test]
    fn a_match_before_the_first_row_belongs_to_no_row() {
        use crate::index::RecordScanner;

        // A document's leading `[` sits before every row. Attributing it to row
        // zero would send the user to a record that does not contain the string.
        let source = b"\n\n{\"x\":1}\n";
        let mut scanner = RecordScanner::new();
        scanner.feed(source);
        let table = scanner.finish();

        assert_eq!(rows_of(&table, &[0, 1]), [] as [usize; 0]);
        assert_eq!(rows_of(&table, &[2, 5]), [0, 0]);
    }
}
