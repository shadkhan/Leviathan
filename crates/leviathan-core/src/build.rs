//! Tier-1 indexing, driven from a [`ByteRange`] source in resumable batches.
//!
//! The pieces this drives all existed before it: [`RecordScanner`] finds NDJSON
//! records by scanning for newlines, and [`Lexer`] + [`Structure`] +
//! [`RootCollector`] find a single document's root children by walking the
//! grammar. What was missing was the loop around them, and every consumer was
//! writing its own — the CLI's benchmark had one, and the Worker was about to
//! grow a second in TypeScript, on the far side of the WASM boundary where it
//! could never be tested.
//!
//! ## Why this pulls instead of being fed
//!
//! Both underlying builders take pushed bytes, so the obvious shape is for the
//! host to read a chunk and hand it over. That works, and it was rejected,
//! because tier 2 cannot work that way: [`Expansion`](crate::Expansion) must
//! decide *where* to read next, and only it knows. Having tier 1 pushed and
//! tier 2 pulled would mean the Worker implements two byte-delivery mechanisms
//! and the core is honest about its needs in only one of them.
//!
//! So this pulls through [`ByteRange`] like everything else, and the host
//! implements one thing: answer a byte range. See `DEEP_REASONING.md` C41.
//!
//! ## Why it stops before it is finished
//!
//! Indexing 500 MB takes seconds. A [`Build`] therefore consumes at most
//! [`BuildOptions::batch`] bytes per [`advance`](Build::advance) and returns,
//! keeping every piece of its state, so the host can report progress, honour a
//! cancel, or drop the whole thing. This is the same shape as
//! [`Expansion::advance`](crate::Expansion::advance) for the same reason (C39),
//! and it is the reason neither of them needs a thread.

use crate::format::Format;
use crate::index::{ChildTable, RecordScanner, RootCollector, Tier1};
use crate::lexer::Lexer;
use crate::source::{ByteRange, SourceError, read_clamped};
use crate::structure::{Documents, Structure};

/// How much of an index to build per [`Build::advance`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuildOptions {
    /// Bytes to read at a time.
    ///
    /// One `Blob.slice` in the Worker. Larger than tier 2's window because a
    /// build reads strictly forward and always uses every byte it asks for.
    pub window: u32,
    /// Bytes to consume before returning, so the host can report or cancel.
    pub batch: u64,
}

impl Default for BuildOptions {
    fn default() -> Self {
        Self {
            window: 1024 * 1024,
            // At the measured single-document rate (~130 MB/s, C39) this is
            // ~30 ms of work — the upper bound on how long a cancel can take to
            // be noticed. NDJSON scanning is roughly seven times faster, so the
            // same figure buys a much finer progress bar there for free.
            batch: 4 * 1024 * 1024,
        }
    }
}

/// Why a [`Build`] stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Built {
    /// The source ended. The index covers the whole file.
    Complete,
    /// The batch limit was reached. Call [`Build::advance`] again.
    Batch,
    /// The document did not parse. Everything found before the error is still
    /// indexed and still correct (C6) — this is a stopping condition, not a
    /// failure.
    Malformed,
}

impl Built {
    /// Whether calling [`Build::advance`] again could make progress.
    #[must_use]
    pub const fn resumable(self) -> bool {
        matches!(self, Built::Batch)
    }
}

/// The two ways a root's children are found.
///
/// They have nothing in common except their output, which is the whole point of
/// C26: one [`ChildTable`], two builders, so everything downstream — rows,
/// expansion, export — is written once.
#[derive(Debug, Clone)]
enum Mode {
    /// NDJSON: a record begins after every newline.
    Records(RecordScanner),
    /// One document: the root's children come from walking the grammar.
    Document {
        lexer: Lexer,
        structure: Structure,
        collector: RootCollector,
    },
}

/// A tier-1 index under construction.
///
/// Create it for a [`Format`], then call [`advance`](Build::advance) until it
/// stops for a reason that is not [`Built::Batch`].
#[derive(Debug, Clone)]
pub struct Build {
    format: Format,
    mode: Mode,
    consumed: u64,
    finished: Option<Built>,
}

impl Build {
    /// A build for `format`, positioned at the start of the source.
    ///
    /// [`Format::Empty`] and [`Format::Unknown`] take the single-document path.
    /// Neither will produce much, but both produce it the same way every other
    /// unparseable input does — an empty or partial table and [`Built::Malformed`]
    /// — rather than needing the caller to special-case them before it can even
    /// try. Refusing to open a file is the behaviour this project exists to
    /// replace.
    #[must_use]
    pub fn new(format: Format) -> Self {
        let mode = match format {
            Format::Ndjson => Mode::Records(RecordScanner::new()),
            _ => Mode::Document {
                lexer: Lexer::new(),
                structure: Structure::new(Documents::One),
                collector: RootCollector::new(),
            },
        };
        Self {
            format,
            mode,
            consumed: 0,
            finished: None,
        }
    }

    /// The format this build was created for.
    #[must_use]
    pub const fn format(&self) -> Format {
        self.format
    }

    /// Bytes indexed so far — the numerator of a progress bar.
    #[must_use]
    pub const fn consumed(&self) -> u64 {
        self.consumed
    }

    /// Root children found so far.
    #[must_use]
    pub fn rows(&self) -> usize {
        self.table().len()
    }

    /// Bytes of heap the index holds.
    #[must_use]
    pub fn heap_bytes(&self) -> usize {
        self.table().heap_bytes()
    }

    /// Why the build last stopped, or `None` if it has not run.
    #[must_use]
    pub const fn stopped(&self) -> Option<Built> {
        self.finished
    }

    /// Whether the whole source has been indexed.
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        matches!(self.finished, Some(Built::Complete))
    }

    /// Whether more work remains and is possible.
    #[must_use]
    pub const fn is_resumable(&self) -> bool {
        match self.finished {
            None => true,
            Some(reason) => reason.resumable(),
        }
    }

    /// The root's children, as far as they are known.
    ///
    /// Readable mid-build: a partial index is already a usable tree, which is
    /// what lets rows appear while a 500 MB file is still being indexed.
    #[must_use]
    pub const fn table(&self) -> &ChildTable {
        match &self.mode {
            Mode::Records(scanner) => scanner.table(),
            Mode::Document { collector, .. } => collector.table(),
        }
    }

    /// Index up to [`BuildOptions::batch`] more bytes.
    ///
    /// # Errors
    ///
    /// Only [`SourceError`]: the bytes could not be read. A document that does
    /// not parse stops the build and is reported as [`Built::Malformed`], not
    /// as an error (C6).
    pub fn advance<S: ByteRange>(
        &mut self,
        source: &mut S,
        options: &BuildOptions,
    ) -> Result<Built, SourceError> {
        if let Some(reason) = self.finished {
            if !reason.resumable() {
                return Ok(reason);
            }
            self.finished = None;
        }

        let budget = self.consumed.saturating_add(options.batch.max(1));

        while self.consumed < budget {
            let bytes = read_clamped(source, self.next_offset(), u64::from(options.window))?;
            if bytes.is_empty() {
                return Ok(self.stop_at_source_end());
            }

            match &mut self.mode {
                Mode::Records(scanner) => {
                    scanner.feed(bytes);
                    self.consumed += bytes.len() as u64;
                }
                Mode::Document {
                    lexer,
                    structure,
                    collector,
                } => {
                    let mut malformed = false;
                    // Dropping the iterator — here, or early on the `break` —
                    // is what folds the consumed count into the lexer's
                    // absolute offset, so the next window resumes exactly here.
                    for token in lexer.feed(bytes) {
                        let Ok(token) = token else {
                            malformed = true;
                            break;
                        };
                        match structure.push(token) {
                            Ok(Some(event)) => collector.observe(event),
                            Ok(None) => {}
                            Err(_) => {
                                malformed = true;
                                break;
                            }
                        }
                    }
                    // The lexer is the authority on how far the walk actually
                    // got; it may have stopped mid-token inside the window.
                    self.consumed = lexer.offset();
                    if malformed {
                        return Ok(self.stop(Built::Malformed));
                    }
                }
            }
        }

        Ok(self.stop(Built::Batch))
    }

    /// Index the whole source, however long it takes.
    ///
    /// For callers that are not reporting progress — the CLI, a test, an
    /// export. A host with a message loop wants [`advance`](Build::advance).
    ///
    /// # Errors
    ///
    /// Only [`SourceError`].
    pub fn run_to_end<S: ByteRange>(
        &mut self,
        source: &mut S,
        options: &BuildOptions,
    ) -> Result<Built, SourceError> {
        loop {
            let reason = self.advance(source, options)?;
            if !reason.resumable() {
                return Ok(reason);
            }
        }
    }

    /// Finish and take the index.
    #[must_use]
    pub fn finish(self) -> Tier1 {
        let root = match self.mode {
            Mode::Records(scanner) => scanner.finish(),
            Mode::Document { collector, .. } => collector.finish(),
        };
        Tier1 {
            format: self.format,
            root,
        }
    }

    /// Where the next read must begin.
    ///
    /// For the record scanner that is simply everything fed so far. For the
    /// document walk it is the lexer's resume point, which is the same number
    /// by construction — but asking the lexer is what keeps it that way.
    const fn next_offset(&self) -> u64 {
        match &self.mode {
            Mode::Records(_) => self.consumed,
            Mode::Document { lexer, .. } => lexer.offset(),
        }
    }

    fn stop(&mut self, reason: Built) -> Built {
        if !reason.resumable() {
            self.seal();
        }
        self.finished = Some(reason);
        reason
    }

    /// Reached the end of the source. Flush what the lexer is still holding.
    ///
    /// A number is the only token that cannot be emitted until the byte after
    /// it arrives, so a file ending `...,42` with no trailing newline has one
    /// value the lexer has not handed over yet. This is the fourth consumer of
    /// the lexer and the fourth time this has had to be written down; see
    /// `DEEP_REASONING.md` C37, and note that the test below is the only reason
    /// it would have been noticed.
    fn stop_at_source_end(&mut self) -> Built {
        if let Mode::Document {
            lexer,
            structure,
            collector,
        } = &mut self.mode
        {
            if let Ok(Some(token)) = lexer.finish() {
                if let Ok(Some(event)) = structure.push(token) {
                    collector.observe(event);
                }
            }
            let _ = structure.finish();
            self.consumed = lexer.offset();
        }
        self.stop(Built::Complete)
    }

    /// Give back the table's growth headroom, now that no more can arrive (C38).
    fn seal(&mut self) {
        match &mut self.mode {
            Mode::Records(scanner) => scanner.seal(),
            Mode::Document { collector, .. } => collector.seal(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(source: &[u8], format: Format) -> Build {
        let mut build = Build::new(format);
        let mut src = source;
        build
            .run_to_end(&mut src, &BuildOptions::default())
            .unwrap();
        build
    }

    // ---- the two formats --------------------------------------------------

    #[test]
    fn ndjson_indexes_one_row_per_record() {
        let build = build(b"{\"a\":1}\n{\"a\":2}\n{\"a\":3}\n", Format::Ndjson);
        assert!(build.is_complete());
        assert_eq!(build.rows(), 3);
        assert_eq!(build.table().range(0, 3), [0, 8, 16]);
    }

    #[test]
    fn a_single_document_indexes_its_roots_children() {
        let build = build(b"[10,20,30]", Format::SingleDocument);
        assert!(build.is_complete());
        assert_eq!(build.table().range(0, 3), [1, 4, 7]);
    }

    #[test]
    fn an_object_root_is_addressed_by_its_keys() {
        let build = build(br#"{"a":1,"bb":22}"#, Format::SingleDocument);
        assert!(build.table().keyed());
        assert_eq!(build.rows(), 2);
    }

    #[test]
    fn nested_children_belong_to_tier_2_not_here() {
        let build = build(br#"[[1,2],{"a":[3]},4]"#, Format::SingleDocument);
        assert_eq!(build.rows(), 3, "three root children, however deep");
    }

    #[test]
    fn the_finished_index_carries_the_format_it_was_built_for() {
        let tier1 = build(b"1\n2\n", Format::Ndjson).finish();
        assert_eq!(tier1.format, Format::Ndjson);
        assert_eq!(tier1.rows(), 2);
    }

    // ---- resumability -----------------------------------------------------

    #[test]
    fn the_batch_size_never_changes_the_index() {
        let source = br#"[{"a":1},[2,3],"four",5,null,true,{"b":[6,7]}]"#;
        let reference = build(source, Format::SingleDocument);

        for batch in [1u64, 2, 7, 64, 1_000_000] {
            let options = BuildOptions {
                batch,
                ..BuildOptions::default()
            };
            let mut trial = Build::new(Format::SingleDocument);
            let mut src = &source[..];
            trial.run_to_end(&mut src, &options).unwrap();
            assert_eq!(trial.table(), reference.table(), "batch {batch}");
            assert_eq!(trial.consumed(), reference.consumed(), "batch {batch}");
        }
    }

    #[test]
    fn the_window_size_never_changes_the_index() {
        // Windows cut tokens and records in half; neither builder may notice.
        let source = b"{\"a\":1}\n{\"bb\":22}\n{\"ccc\":333}\n";
        let reference = build(source, Format::Ndjson);

        for window in [1u32, 2, 5, 13, 4096] {
            let options = BuildOptions {
                window,
                batch: 1024 * 1024,
            };
            let mut trial = Build::new(Format::Ndjson);
            let mut src = &source[..];
            trial.run_to_end(&mut src, &options).unwrap();
            assert_eq!(trial.table(), reference.table(), "window {window}");
        }
    }

    #[test]
    fn a_partial_build_is_already_a_usable_index() {
        // Why `advance` exists: rows must be paintable before indexing ends.
        let mut source = Vec::new();
        for i in 0..2000u32 {
            source.extend_from_slice(format!("{{\"n\":{i}}}\n").as_bytes());
        }

        let options = BuildOptions {
            window: 4096,
            batch: 1024,
        };
        let mut build = Build::new(Format::Ndjson);
        let mut src = source.as_slice();

        assert_eq!(build.advance(&mut src, &options).unwrap(), Built::Batch);
        assert!(!build.is_complete());
        assert!(build.is_resumable());
        assert!(build.rows() > 0, "rows exist before the build finishes");
        assert_eq!(build.table().child(0), Some(0), "and they are real offsets");

        let partial = build.rows();
        build.run_to_end(&mut src, &options).unwrap();
        assert_eq!(build.rows(), 2000);
        assert!(partial < 2000, "the first batch was genuinely partial");
    }

    #[test]
    fn progress_advances_and_ends_at_the_size_of_the_source() {
        let source = b"[1,2,3,4,5]";
        let build = build(source, Format::SingleDocument);
        assert_eq!(build.consumed(), source.len() as u64);
    }

    // ---- degradation ------------------------------------------------------

    #[test]
    fn a_file_ending_in_a_bare_number_keeps_its_last_value() {
        // C37, fourth sighting. Without the flush in `stop_at_source_end` this
        // reports two children and nothing else fails.
        assert_eq!(build(b"[1,2,3]", Format::SingleDocument).rows(), 3);

        // And the NDJSON path, where a missing trailing newline is the norm.
        assert_eq!(
            build(b"1\n2\n3", Format::Ndjson).rows(),
            3,
            "no trailing newline is not a missing record"
        );
    }

    #[test]
    fn a_truncated_document_keeps_the_children_it_found() {
        let build = build(b"[1,2,3,4", Format::SingleDocument);
        assert_eq!(build.rows(), 4, "four good elements");
        assert!(
            build.is_complete(),
            "the source ended, which is not an error"
        );
    }

    #[test]
    fn a_malformed_document_keeps_the_children_it_found() {
        let build = build(br#"[1,2,@,4]"#, Format::SingleDocument);
        assert_eq!(build.stopped(), Some(Built::Malformed));
        assert_eq!(build.rows(), 2);
        assert!(!build.is_resumable(), "a syntax error is terminal");
    }

    #[test]
    fn an_empty_source_indexes_to_nothing_rather_than_failing() {
        for format in [Format::Empty, Format::Unknown, Format::Ndjson] {
            let build = build(b"", format);
            assert!(build.table().is_empty(), "{format:?}");
            assert_eq!(build.consumed(), 0, "{format:?}");
        }
    }

    #[test]
    fn a_file_that_is_not_json_at_all_still_opens() {
        let build = build(b"not json", Format::Unknown);
        assert_eq!(build.stopped(), Some(Built::Malformed));
        assert!(build.table().is_empty());
    }

    #[test]
    fn advancing_a_finished_build_is_a_no_op() {
        let source = b"[1,2]";
        let mut build = Build::new(Format::SingleDocument);
        let mut src = &source[..];
        let options = BuildOptions::default();

        assert_eq!(
            build.run_to_end(&mut src, &options).unwrap(),
            Built::Complete
        );
        assert_eq!(build.advance(&mut src, &options).unwrap(), Built::Complete);
        assert_eq!(build.rows(), 2, "and does not double-count");
    }

    #[test]
    fn a_finished_index_holds_exactly_its_contents() {
        // C38: the growth headroom goes back at the terminal stop.
        let mut source = Vec::new();
        for i in 0..1025u32 {
            source.extend_from_slice(format!("{i}\n").as_bytes());
        }
        let build = build(&source, Format::Ndjson);
        assert_eq!(build.rows(), 1025);
        assert_eq!(build.heap_bytes(), 1025 * 8);
    }
}
