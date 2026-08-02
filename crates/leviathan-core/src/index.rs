//! Tier-1 indexing: enough structure to draw the first screen and size the
//! scrollbar, and not one byte more.
//!
//! ## What tier 1 is
//!
//! One [`ChildTable`]: the direct children of the root, as byte offsets. That is
//! the whole representation, and it is the answer to the question
//! `DEEP_REASONING.md` C3 left open — whether NDJSON and single-document inputs
//! need different tier-1 structures. They do not. An NDJSON stream is a root
//! whose children are records; a JSON document is a root whose children are its
//! members or elements. Same table, built two ways.
//!
//! Expanding a node later produces another `ChildTable` for that node (tier 2),
//! so there is one addressing model in the whole engine rather than two that
//! drift.
//!
//! ## Why a child offset is 8 bytes and nothing else
//!
//! No kind, no length, no child count, no key text. All of it is re-derived by
//! re-lexing from the offset when a row is actually painted, because a row is
//! painted at most fifty at a time and an index is held for the life of the
//! session. The arithmetic on the 500 MB fixture:
//!
//! | | Per child | 1.77 M records |
//! |---|---:|---:|
//! | Offset only | 8 B | **14 MB** |
//! | \+ kind, count, span | 24 B | 42 MB |
//!
//! The exit criterion is 40 MB. The second row fails it; the first passes with
//! room to spare, and pays for it with a few microseconds per painted row.
//!
//! ## Why the NDJSON path does not parse
//!
//! An NDJSON record boundary is a newline, and — this is the part that is not
//! obvious — a newline is *always* a record boundary. JSON forbids unescaped
//! control characters inside strings, and no other token can contain one, so a
//! raw newline can never occur inside a value (`DEEP_REASONING.md` C21). Scanning
//! for `\n` is therefore not a heuristic that usually works; it is exact.
//!
//! That makes tier 1 for a 500 MB log file a memory-bandwidth-bound scan rather
//! than a parse — the `scan` and `index` rows of `leviathan bench` are nearly
//! the same number — and it is why the file opens before it has been validated.
//! Validation is a separate, later, resumable pass (C6: degrade, never abort).

use crate::format::Format;
use crate::lexer::Token;
use crate::structure::{ContainerKind, Event};

/// The direct children of one container, addressable at random.
///
/// The offsets are where each child's **row** begins, which for an object member
/// is its key rather than its value. That is deliberate: lexing forward from a
/// key yields the key, the colon and the value in one pass, whereas recovering a
/// key from a value offset would require lexing backwards, which no streaming
/// lexer can do.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChildTable {
    kind: Option<ContainerKind>,
    offsets: Vec<u64>,
}

impl ChildTable {
    /// An empty table for a container of the given kind.
    ///
    /// `None` means the synthetic root of an NDJSON stream, which is
    /// array-like — records are positional — but has no `[` in the source.
    #[must_use]
    pub const fn new(kind: Option<ContainerKind>) -> Self {
        Self {
            kind,
            offsets: Vec::new(),
        }
    }

    /// What kind of container these are the children of.
    #[must_use]
    pub const fn kind(&self) -> Option<ContainerKind> {
        self.kind
    }

    /// Whether rows in this table begin with a key.
    #[must_use]
    pub fn keyed(&self) -> bool {
        self.kind == Some(ContainerKind::Object)
    }

    /// How many children.
    #[must_use]
    pub fn len(&self) -> usize {
        self.offsets.len()
    }

    /// Whether there are none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.offsets.is_empty()
    }

    /// The byte offset of the `n`-th child, in O(1).
    ///
    /// The entire reason this is a flat table rather than a linked structure.
    /// Virtual scrolling asks for row 900 000 of five million and must not pay
    /// 900 000 pointer hops to answer (`DEEP_REASONING.md` C4).
    #[must_use]
    pub fn child(&self, n: usize) -> Option<u64> {
        self.offsets.get(n).copied()
    }

    /// A contiguous run of children, clamped to what exists.
    ///
    /// This is the shape `get_rows` wants: one call per visible window, not one
    /// per row.
    #[must_use]
    pub fn range(&self, start: usize, count: usize) -> &[u64] {
        let start = start.min(self.offsets.len());
        let end = start.saturating_add(count).min(self.offsets.len());
        &self.offsets[start..end]
    }

    /// Which child contains byte `offset`, if any.
    ///
    /// The join between where the engine thinks (byte offsets) and where the
    /// user thinks (rows): a find match, a validation error, or a jump-to-offset
    /// all arrive as a byte and have to become a row before anyone can be sent
    /// there. Offsets are ascending by construction, so this is a binary search
    /// — a 500 MB file's 1.77 M records are 21 comparisons, not a scan.
    ///
    /// A byte before the first child belongs to no child. That is not an edge
    /// case to smooth over: a document's opening `[` genuinely precedes every
    /// row, and answering "row 0" would send the user somewhere the byte is not.
    #[must_use]
    pub fn locate(&self, offset: u64) -> Option<usize> {
        match self.offsets.partition_point(|&start| start <= offset) {
            0 => None,
            n => Some(n - 1),
        }
    }

    /// Bytes of heap this table occupies.
    ///
    /// Reported rather than estimated, because "index size < 40 MB" is an exit
    /// criterion and a criterion measured by guesswork is not a criterion.
    #[must_use]
    pub fn heap_bytes(&self) -> usize {
        self.offsets.capacity() * size_of::<u64>()
    }

    /// Append a child offset. Callers must supply them in document order.
    fn push(&mut self, offset: u64) {
        self.offsets.push(offset);
    }

    /// Release the growth headroom a `Vec` keeps.
    ///
    /// Worth an explicit step because of how the numbers land: 1.77 M records
    /// need 14.2 MB, but doubling growth leaves the table holding 16.8 MB — 18 %
    /// of the index is slack that will never be used, on a structure that lives
    /// as long as the session does. One reallocation at build time buys it back.
    fn seal(&mut self) {
        self.offsets.shrink_to_fit();
    }
}

/// Builds an NDJSON record table by scanning for newlines.
///
/// Exact, not heuristic — see the module docs. Feed it chunks in order; it holds
/// no reference to them and allocates only the table itself.
#[derive(Debug, Clone)]
pub struct RecordScanner {
    table: ChildTable,
    offset: u64,
    /// Whether we are looking for the first non-blank byte of a line.
    seeking: bool,
}

impl Default for RecordScanner {
    fn default() -> Self {
        Self::new()
    }
}

impl RecordScanner {
    /// A scanner positioned at the start of a stream.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            table: ChildTable::new(None),
            offset: 0,
            seeking: true,
        }
    }

    /// Consume the next chunk.
    ///
    /// Chunk boundaries are invisible: a record is recorded when its first byte
    /// is seen, and "first byte of a line" survives a boundary because the only
    /// state it needs is one bool.
    pub fn feed(&mut self, chunk: &[u8]) {
        let mut pos = 0usize;

        while pos < chunk.len() {
            if self.seeking {
                // Skip blank lines and leading indentation to the record's
                // first real byte.
                match chunk[pos..]
                    .iter()
                    .position(|b| !matches!(b, b' ' | b'\t' | b'\r' | b'\n'))
                {
                    Some(skip) => {
                        pos += skip;
                        self.table.push(self.offset + pos as u64);
                        self.seeking = false;
                    }
                    None => {
                        pos = chunk.len();
                    }
                }
            } else {
                // Run to the end of the record.
                match chunk[pos..].iter().position(|b| *b == b'\n') {
                    Some(nl) => {
                        pos += nl + 1;
                        self.seeking = true;
                    }
                    None => pos = chunk.len(),
                }
            }
        }

        self.offset += chunk.len() as u64;
    }

    /// How many records so far.
    #[must_use]
    pub fn records(&self) -> usize {
        self.table.len()
    }

    /// The records found so far, without consuming the scanner.
    ///
    /// A tier-1 build reads this while still scanning ([`Build`](crate::Build)),
    /// because a partially indexed file is already a browsable tree — which is
    /// the difference between a viewer that opens a 500 MB file in two seconds
    /// and one that opens it in fifty milliseconds.
    #[must_use]
    pub const fn table(&self) -> &ChildTable {
        &self.table
    }

    /// Release the table's growth headroom, once no more records can arrive.
    ///
    /// See [`RootCollector::seal`] — same reasoning, and the same 40 % (C38).
    pub fn seal(&mut self) {
        self.table.seal();
    }

    /// Finish and take the table.
    #[must_use]
    pub fn finish(mut self) -> ChildTable {
        self.table.seal();
        self.table
    }
}

/// Collects the children of the root from a structural walk.
///
/// Used for single-document input, where finding the root's children means
/// understanding the grammar — unlike NDJSON, where it means finding newlines.
/// Feed it every [`Event`] the walk produces, in order.
#[derive(Debug, Clone, Default)]
pub struct RootCollector {
    table: ChildTable,
    /// The key most recently seen at child depth, if the root is an object.
    pending_key: Option<u64>,
    /// Whether a root container has been opened at all.
    rooted: bool,
}

impl RootCollector {
    /// A collector that has seen nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Observe one structural event.
    pub fn observe(&mut self, event: Event) {
        match event {
            // The root itself: its kind decides whether rows carry keys.
            Event::Open { kind, depth: 0, .. } => {
                self.table = ChildTable::new(Some(kind));
                self.rooted = true;
            }
            // A key at child depth: the row starts here, not at the value.
            Event::Key { token, depth: 1 } => {
                self.pending_key = Some(token.start);
            }
            Event::Open {
                start, depth: 1, ..
            } => self.push_child(start),
            Event::Scalar {
                token: Token { start, .. },
                depth: 1,
            } => self.push_child(start),
            _ => {}
        }
    }

    fn push_child(&mut self, value_start: u64) {
        let row_start = self.pending_key.take().unwrap_or(value_start);
        self.table.push(row_start);
    }

    /// Whether the document's root was a container.
    ///
    /// A document that is a bare scalar (`42`) has a root but no children, and
    /// renders as a single row.
    #[must_use]
    pub const fn rooted(&self) -> bool {
        self.rooted
    }

    /// The children found so far, without consuming the collector.
    ///
    /// Tier-2 expansion reads this while still building (`expand::Expansion`),
    /// because a partially indexed container is already worth showing.
    #[must_use]
    pub const fn table(&self) -> &ChildTable {
        &self.table
    }

    /// Release the table's growth headroom, once no more children can arrive.
    ///
    /// Only worth calling when collection is finished, and then it is worth
    /// quite a lot: a `Vec` that grew to five million entries holds capacity for
    /// 8.4 million, so an expansion reports **67 MB** where its contents are
    /// 40 MB. That 40 % is charged against the cache's memory budget and would
    /// never be used.
    pub fn seal(&mut self) {
        self.table.seal();
    }

    /// How many children have been found so far.
    #[must_use]
    pub fn len(&self) -> usize {
        self.table.len()
    }

    /// Whether none have been found yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.table.is_empty()
    }

    /// Finish and take the table.
    #[must_use]
    pub fn finish(mut self) -> ChildTable {
        self.table.seal();
        self.table
    }
}

/// What tier-1 indexing produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tier1 {
    /// The format the index was actually built for.
    pub format: Format,
    /// The root's direct children.
    pub root: ChildTable,
}

impl Tier1 {
    /// How many rows the tree shows before anything is expanded.
    #[must_use]
    pub fn rows(&self) -> usize {
        self.root.len()
    }

    /// Bytes of heap the index occupies.
    #[must_use]
    pub fn heap_bytes(&self) -> usize {
        self.root.heap_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;
    use crate::structure::{Documents, Structure};

    fn scan_records(source: &[u8], chunk: usize) -> ChildTable {
        let mut scanner = RecordScanner::new();
        for piece in source.chunks(chunk.max(1)) {
            scanner.feed(piece);
        }
        scanner.finish()
    }

    fn collect_root(source: &[u8]) -> ChildTable {
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
        structure.finish().unwrap();
        collector.finish()
    }

    // ---- child table ------------------------------------------------------

    #[test]
    fn a_child_table_answers_random_access_in_one_step() {
        let table = scan_records(b"a\nb\nc\nd\ne\n", 64);
        assert_eq!(table.len(), 5);
        assert_eq!(table.child(0), Some(0));
        assert_eq!(table.child(3), Some(6));
        assert_eq!(table.child(9), None);
    }

    #[test]
    fn a_range_is_clamped_rather_than_panicking() {
        let table = scan_records(b"a\nb\nc\n", 64);
        assert_eq!(table.range(0, 2), [0, 2]);
        assert_eq!(table.range(1, 100), [2, 4]);
        assert_eq!(table.range(99, 5), [] as [u64; 0]);
        assert_eq!(table.range(0, 0), [] as [u64; 0]);
    }

    #[test]
    fn heap_cost_is_eight_bytes_per_child() {
        // The exit criterion is stated in megabytes, so the per-child figure has
        // to be exactly what it claims.
        let mut source = Vec::new();
        for i in 0..1000 {
            source.extend_from_slice(format!("{{\"i\":{i}}}\n").as_bytes());
        }
        let table = scan_records(&source, 4096);
        assert_eq!(table.len(), 1000);
        assert!(
            table.heap_bytes() >= 8000 && table.heap_bytes() <= 16_000,
            "expected ~8 B/child, got {} for 1000",
            table.heap_bytes()
        );
    }

    // ---- NDJSON record scanning -------------------------------------------

    #[test]
    fn record_offsets_point_at_the_first_byte_of_each_record() {
        let source = b"{\"a\":1}\n{\"a\":2}\n{\"a\":3}\n";
        let table = scan_records(source, 64);
        assert_eq!(table.range(0, 9), [0, 8, 16]);
        // Every offset really is a `{`.
        for &offset in table.range(0, 9) {
            assert_eq!(source[offset as usize], b'{');
        }
    }

    #[test]
    fn the_chunk_size_never_changes_the_record_table() {
        let source = b"{\"a\":1}\n  {\"b\":2}\n\n{\"c\":3}\n{\"d\":4}";
        let whole = scan_records(source, source.len());
        for chunk in 1..=source.len() {
            assert_eq!(scan_records(source, chunk), whole, "chunk size {chunk}");
        }
    }

    #[test]
    fn blank_lines_and_indentation_are_not_records() {
        let source = b"\n\n  {\"a\":1}\n\n\t{\"b\":2}\n\n";
        let table = scan_records(source, 3);
        assert_eq!(table.len(), 2);
        assert_eq!(source[table.child(0).unwrap() as usize], b'{');
        assert_eq!(source[table.child(1).unwrap() as usize], b'{');
    }

    #[test]
    fn a_final_record_without_a_trailing_newline_still_counts() {
        // Half the NDJSON in the world ends without one.
        assert_eq!(scan_records(b"1\n2\n3", 64).len(), 3);
        assert_eq!(scan_records(b"1\n2\n3\n", 64).len(), 3);
    }

    #[test]
    fn an_empty_or_blank_stream_has_no_records() {
        assert_eq!(scan_records(b"", 64).len(), 0);
        assert_eq!(scan_records(b"\n\n\n", 64).len(), 0);
        assert_eq!(scan_records(b"   \n \t \n", 64).len(), 0);
    }

    #[test]
    fn a_newline_inside_a_string_cannot_happen_so_scanning_is_exact() {
        // The claim the whole NDJSON path rests on. A record containing what
        // looks like a newline holds the two bytes `\` and `n`, which are not
        // a newline and do not split the record.
        let source = b"{\"text\":\"line1\\nline2\"}\n{\"text\":\"x\"}\n";
        let table = scan_records(source, 7);
        assert_eq!(table.len(), 2, "an escaped newline must not split a record");
        assert_eq!(table.child(1), Some(24));
    }

    // ---- single-document root ---------------------------------------------

    #[test]
    fn an_array_roots_children_are_its_elements() {
        let source = b"[10,20,30]";
        let table = collect_root(source);
        assert_eq!(table.kind(), Some(ContainerKind::Array));
        assert!(!table.keyed());
        assert_eq!(table.range(0, 9), [1, 4, 7]);
    }

    #[test]
    fn an_object_roots_children_start_at_their_keys() {
        // Not at their values: a row shows `"name": "leviathan"`, and lexing
        // forward from the key gets both in one pass. Lexing backwards from the
        // value would be impossible.
        let source = br#"{"a":1,"bb":22}"#;
        let table = collect_root(source);
        assert_eq!(table.kind(), Some(ContainerKind::Object));
        assert!(table.keyed());
        assert_eq!(table.range(0, 9), [1, 7]);
        assert_eq!(source[1], b'"');
        assert_eq!(source[7], b'"');
    }

    #[test]
    fn nested_children_are_not_collected() {
        // Tier 1 is one level. Everything below is tier 2's job, on expand.
        let table = collect_root(br#"{"a":{"deep":{"deeper":1}},"b":[1,[2,[3]]]}"#);
        assert_eq!(table.len(), 2, "two members, however deep they go");
    }

    #[test]
    fn containers_and_scalars_are_both_children() {
        let table = collect_root(br#"[1,"two",{"three":3},[4],null,true]"#);
        assert_eq!(table.len(), 6);
    }

    #[test]
    fn empty_and_scalar_documents_have_no_children() {
        assert_eq!(collect_root(b"[]").len(), 0);
        assert_eq!(collect_root(b"{}").len(), 0);

        let table = collect_root(b"42");
        assert_eq!(table.len(), 0);
        assert_eq!(table.kind(), None, "a scalar root is not a container");
    }

    #[test]
    fn a_root_collector_reports_whether_it_saw_a_container() {
        let mut lexer = Lexer::new();
        let mut structure = Structure::new(Documents::One);
        let mut collector = RootCollector::new();
        for token in lexer.feed(b"42") {
            if let Some(event) = structure.push(token.unwrap()).unwrap() {
                collector.observe(event);
            }
        }
        assert!(!collector.rooted());
    }

    #[test]
    fn a_wide_root_is_addressable_at_any_index() {
        // The C4 case in miniature: the ten-thousandth element is one lookup
        // away, not ten thousand hops.
        let mut source = Vec::from(b"[");
        for i in 0..10_000 {
            if i > 0 {
                source.push(b',');
            }
            source.extend_from_slice(b"1");
        }
        source.push(b']');

        let table = collect_root(&source);
        assert_eq!(table.len(), 10_000);
        assert_eq!(table.range(9_000, 3), [18_001, 18_003, 18_005]);
    }

    #[test]
    fn a_byte_offset_resolves_to_the_row_that_contains_it() {
        // Records at 0, 8, 16, each 8 bytes long.
        let table = scan_records(b"{\"a\":1}\n{\"a\":2}\n{\"a\":3}\n", 64);

        assert_eq!(table.locate(0), Some(0), "the first byte of a row");
        assert_eq!(table.locate(5), Some(0), "inside a row");
        assert_eq!(
            table.locate(8),
            Some(1),
            "a boundary belongs to the row it starts"
        );
        assert_eq!(table.locate(15), Some(1));
        assert_eq!(table.locate(16), Some(2));
        assert_eq!(
            table.locate(9_999),
            Some(2),
            "past the end is the last row, which is where the file ends"
        );
    }

    #[test]
    fn a_byte_before_every_row_belongs_to_no_row() {
        let table = scan_records(b"\n\n {\"a\":1}\n", 64);
        assert_eq!(table.child(0), Some(3));
        assert_eq!(table.locate(0), None);
        assert_eq!(table.locate(2), None);
        assert_eq!(table.locate(3), Some(0));
        assert_eq!(ChildTable::new(None).locate(0), None, "an empty table");
    }

    #[test]
    fn tier1_reports_its_own_size() {
        let tier1 = Tier1 {
            format: Format::Ndjson,
            root: scan_records(b"1\n2\n3\n", 64),
        };
        assert_eq!(tier1.rows(), 3);
        assert!(tier1.heap_bytes() >= 24);
    }
}
