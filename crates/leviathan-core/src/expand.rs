//! Tier-2 indexing: the children of a container, indexed when someone asks.
//!
//! Tier 1 gives the root's children and stops (`DEEP_REASONING.md` C3). Everything
//! below that is indexed here, on expand, and thrown away again under memory
//! pressure. A 500 MB file where the user opens four nodes should cost the index
//! of four nodes.
//!
//! ## Expansion is resumable, because some containers are enormous
//!
//! Enumerating a container's children means walking it: child *n*'s offset is
//! not knowable without having walked children 0..*n*. For a 400 MB array that
//! is two seconds, and a viewer that stalls for two seconds on a click has
//! reintroduced the exact failure this project exists to remove.
//!
//! So [`Expansion`] indexes in batches. [`Expansion::advance`] returns after
//! `batch` children, keeping its lexer and grammar state, and the caller decides
//! whether to continue — on the next frame, when the user scrolls further, or
//! not at all. Rows appear as they are found rather than after everything is
//! found, which SPEC's risk R2 already identified as "arguably the better UX
//! anyway".
//!
//! This works because of a property established two layers down: dropping the
//! lexer's chunk iterator early leaves a well-defined resume point
//! (`Lexer::offset`), so stopping mid-window costs nothing and needs no buffer.
//!
//! ## Eviction is safe because a node id is a byte offset
//!
//! C3 required that node ids survive tier-2 eviction, which ruled out ids that
//! are indexes into a growable array. A byte offset satisfies it for free: it is
//! stable for the life of the file, so evicting an expansion loses only *work*,
//! never *addressability*. Re-expanding the same offset produces the same table.
//! That is the whole reason [`ExpansionCache`] can be a pure cache with no
//! invalidation protocol.

use crate::index::{ChildTable, RootCollector};
use crate::lexer::Lexer;
use crate::source::{ByteRange, SourceError, read_clamped};
use crate::structure::{Documents, Event, Structure};

/// How much of an expansion to do per [`Expansion::advance`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExpandOptions {
    /// Bytes to read at a time.
    pub window: u32,
    /// Children to index before returning, so the caller can paint or cancel.
    pub batch: usize,
}

impl Default for ExpandOptions {
    fn default() -> Self {
        Self {
            // One `Blob.slice` in the Worker; large enough that the read loop is
            // not the cost, small enough that peak memory stays flat.
            window: 256 * 1024,
            // Far more than a screen, so ordinary expansion completes in one
            // call, but bounded so a five-million-element array yields.
            batch: 10_000,
        }
    }
}

/// Why an expansion stopped short.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stopped {
    /// The container closed. The table is complete.
    Closed,
    /// The batch limit was reached. Call [`Expansion::advance`] again.
    Batch,
    /// The bytes ran out before the container closed — a truncated file. The
    /// children found so far are still real children (C6).
    EndOfSource,
    /// The bytes were not valid JSON. Same: what was found before the error is
    /// still good.
    Malformed,
}

impl Stopped {
    /// Whether calling [`Expansion::advance`] again could make progress.
    #[must_use]
    pub const fn resumable(self) -> bool {
        matches!(self, Stopped::Batch)
    }
}

/// One container's children, indexed incrementally.
///
/// Create it at the container's byte offset, then call
/// [`advance`](Expansion::advance) until it stops for a reason that is not
/// [`Stopped::Batch`].
#[derive(Debug, Clone)]
pub struct Expansion {
    start: u64,
    lexer: Lexer,
    structure: Structure,
    collector: RootCollector,
    end: Option<u64>,
    finished: Option<Stopped>,
}

impl Expansion {
    /// An expansion of the container beginning at `start`.
    ///
    /// `start` must be the offset of a `{` or `[` — which is exactly what a
    /// [`ChildTable`] holds for an array element, and what follows the key and
    /// colon for an object member.
    #[must_use]
    pub fn at(start: u64) -> Self {
        Self {
            start,
            lexer: Lexer::resuming_at(start, 1),
            structure: Structure::new(Documents::One),
            collector: RootCollector::new(),
            end: None,
            finished: None,
        }
    }

    /// The container's byte offset — its stable identity.
    #[must_use]
    pub const fn start(&self) -> u64 {
        self.start
    }

    /// Where the container ended, once it has been walked that far.
    #[must_use]
    pub const fn end(&self) -> Option<u64> {
        self.end
    }

    /// How many children have been indexed so far.
    #[must_use]
    pub fn len(&self) -> usize {
        self.collector.len()
    }

    /// Whether none have been found yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.collector.is_empty()
    }

    /// Why the expansion last stopped, or `None` if it has not run.
    #[must_use]
    pub const fn stopped(&self) -> Option<Stopped> {
        self.finished
    }

    /// Whether the container has been indexed to its end.
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        matches!(self.finished, Some(Stopped::Closed))
    }

    /// Whether more work remains and is possible.
    #[must_use]
    pub const fn is_resumable(&self) -> bool {
        match self.finished {
            None => true,
            Some(reason) => reason.resumable(),
        }
    }

    /// The children found so far.
    #[must_use]
    pub const fn table(&self) -> &ChildTable {
        self.collector.table()
    }

    /// Bytes of heap this expansion holds.
    #[must_use]
    pub fn heap_bytes(&self) -> usize {
        self.collector.table().heap_bytes()
    }

    /// Index up to [`ExpandOptions::batch`] more children.
    ///
    /// Returns why it stopped. Only [`Stopped::Batch`] means calling again will
    /// help; the other reasons are terminal, and every one of them still leaves
    /// the children found so far intact and usable.
    ///
    /// # Errors
    ///
    /// Only [`SourceError`]: the bytes could not be read. Malformed *content*
    /// stops the walk and is reported as [`Stopped::Malformed`], not as an
    /// error — a container that goes wrong halfway still has a good first half,
    /// and refusing to show it would be the failure mode this project exists to
    /// remove (C6).
    pub fn advance<S: ByteRange>(
        &mut self,
        source: &mut S,
        options: &ExpandOptions,
    ) -> Result<Stopped, SourceError> {
        if let Some(reason) = self.finished {
            if !reason.resumable() {
                return Ok(reason);
            }
            self.finished = None;
        }

        let target = self.len().saturating_add(options.batch);

        loop {
            let bytes = read_clamped(source, self.lexer.offset(), u64::from(options.window))?;
            if bytes.is_empty() {
                return Ok(self.stop_at_source_end());
            }

            // Borrowed field-by-field so the lexer, the grammar and the
            // collector can all be advanced inside one pass over the window.
            let Expansion {
                lexer,
                structure,
                collector,
                end,
                ..
            } = self;

            let mut outcome = None;
            // The `for` consumes the iterator, so it is dropped here whether the
            // window is drained or broken out of early — and that drop is what
            // folds the consumed count into the lexer's absolute offset, so the
            // next window starts exactly where this one stopped.
            for token in lexer.feed(bytes) {
                let Ok(token) = token else {
                    outcome = Some(Stopped::Malformed);
                    break;
                };
                match structure.push(token) {
                    Ok(Some(event)) => {
                        if let Event::Close {
                            depth: 0, end: at, ..
                        } = event
                        {
                            *end = Some(at);
                            outcome = Some(Stopped::Closed);
                            break;
                        }
                        collector.observe(event);
                        if collector.len() >= target {
                            outcome = Some(Stopped::Batch);
                            break;
                        }
                    }
                    Ok(None) => {}
                    Err(_) => {
                        outcome = Some(Stopped::Malformed);
                        break;
                    }
                }
            }
            if let Some(reason) = outcome {
                return Ok(self.stop(reason));
            }
        }
    }

    /// Index the whole container, however long it takes.
    ///
    /// For callers that are not painting frames — the CLI, a test, an export.
    /// Interactive callers want [`advance`](Expansion::advance) in a loop with a
    /// chance to yield between calls.
    ///
    /// # Errors
    ///
    /// Only [`SourceError`].
    pub fn run_to_end<S: ByteRange>(
        &mut self,
        source: &mut S,
        options: &ExpandOptions,
    ) -> Result<Stopped, SourceError> {
        loop {
            let reason = self.advance(source, options)?;
            if !reason.resumable() {
                return Ok(reason);
            }
        }
    }

    /// Take the table of children found.
    #[must_use]
    pub fn finish(self) -> ChildTable {
        self.collector.finish()
    }

    fn stop(&mut self, reason: Stopped) -> Stopped {
        // A terminal stop means no more children can arrive, so the table's
        // growth headroom is dead weight — and it is charged against the cache's
        // budget for the rest of the session. Five million children hold
        // capacity for 8.4 million; sealing gives back 27 MB of the 67.
        if !reason.resumable() {
            self.collector.seal();
        }
        self.finished = Some(reason);
        reason
    }

    /// The source ran out. Flush whatever the lexer was still holding.
    ///
    /// A number is the only token that cannot be emitted until the byte after it
    /// arrives, so a truncated container ending `[1,2,3,4` has one complete
    /// element the lexer has not handed over yet. Forgetting this loses exactly
    /// the last element of exactly the files that are already damaged — the
    /// worst place to be quietly wrong. This is the third time this bug class
    /// has appeared (see `DEEP_REASONING.md` C30 and C37); it is caught here by
    /// the truncated-container test.
    fn stop_at_source_end(&mut self) -> Stopped {
        if let Ok(Some(token)) = self.lexer.finish() {
            if let Ok(Some(event)) = self.structure.push(token) {
                self.collector.observe(event);
            }
        }
        self.stop(Stopped::EndOfSource)
    }
}

/// The default memory budget for cached expansions.
///
/// SPEC's figure. It is a budget for *index* bytes, not document bytes: at 8
/// bytes per child it holds roughly 32 million expanded children, which is far
/// more than anyone will open by hand and still a bound.
pub const DEFAULT_EXPANSION_BUDGET: usize = 256 * 1024 * 1024;

/// Cached expansions, evicted least-recently-used when over budget.
///
/// Eviction here is unusually cheap to reason about, and that is a consequence
/// of an earlier decision rather than a feature of this type: because a node is
/// addressed by its byte offset, an evicted expansion can always be rebuilt
/// identically, so nothing outside this cache can hold a reference that eviction
/// would invalidate. There is no invalidation protocol because there is nothing
/// to invalidate.
#[derive(Debug, Clone)]
pub struct ExpansionCache {
    entries: Vec<Entry>,
    budget: usize,
    clock: u64,
}

#[derive(Debug, Clone)]
struct Entry {
    expansion: Expansion,
    used_at: u64,
}

impl Default for ExpansionCache {
    fn default() -> Self {
        Self::new(DEFAULT_EXPANSION_BUDGET)
    }
}

impl ExpansionCache {
    /// A cache holding at most `budget` bytes of index.
    #[must_use]
    pub const fn new(budget: usize) -> Self {
        Self {
            entries: Vec::new(),
            budget,
            clock: 0,
        }
    }

    /// How many expansions are held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether none are held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Total index bytes held.
    #[must_use]
    pub fn heap_bytes(&self) -> usize {
        self.entries.iter().map(|e| e.expansion.heap_bytes()).sum()
    }

    /// The expansion for `offset`, creating an empty one if it is not cached.
    ///
    /// Evicts down to budget *before* returning, so the entry handed back is
    /// never the one about to be discarded.
    pub fn entry(&mut self, offset: u64) -> &mut Expansion {
        self.clock += 1;
        let now = self.clock;

        if let Some(index) = self.position(offset) {
            self.entries[index].used_at = now;
            return &mut self.entries[index].expansion;
        }

        self.evict_to_budget();
        self.entries.push(Entry {
            expansion: Expansion::at(offset),
            used_at: now,
        });
        let last = self.entries.len() - 1;
        &mut self.entries[last].expansion
    }

    /// The expansion for `offset`, if it is cached.
    pub fn get(&mut self, offset: u64) -> Option<&mut Expansion> {
        self.clock += 1;
        let now = self.clock;
        let index = self.position(offset)?;
        self.entries[index].used_at = now;
        Some(&mut self.entries[index].expansion)
    }

    /// Forget the expansion for `offset`, if any.
    pub fn forget(&mut self, offset: u64) {
        if let Some(index) = self.position(offset) {
            self.entries.remove(index);
        }
    }

    /// Drop everything.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Evict least-recently-used entries until within budget.
    ///
    /// Called automatically before inserting; public because a caller that has
    /// just grown an expansion in place may want to settle up immediately.
    pub fn evict_to_budget(&mut self) {
        while self.heap_bytes() > self.budget && !self.entries.is_empty() {
            let Some(oldest) = self
                .entries
                .iter()
                .enumerate()
                .min_by_key(|(_, e)| e.used_at)
                .map(|(i, _)| i)
            else {
                break;
            };
            self.entries.remove(oldest);
        }
    }

    /// Linear scan.
    ///
    /// A user can only expand so many nodes by hand, so this is a scan over tens
    /// of entries, not thousands — cheaper in practice than a hash map's
    /// overhead, and it keeps the crate free of one more dependency-shaped
    /// decision.
    fn position(&self, offset: u64) -> Option<usize> {
        self.entries
            .iter()
            .position(|e| e.expansion.start() == offset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::structure::ContainerKind;

    fn expand(source: &[u8], start: u64) -> Expansion {
        let mut expansion = Expansion::at(start);
        let mut src = source;
        expansion
            .run_to_end(&mut src, &ExpandOptions::default())
            .unwrap();
        expansion
    }

    // ---- basic expansion --------------------------------------------------

    #[test]
    fn an_array_expands_to_its_elements() {
        //           0123456789
        let source = b"[10,20,30]";
        let expansion = expand(source, 0);

        assert!(expansion.is_complete());
        assert_eq!(expansion.end(), Some(10));
        assert_eq!(expansion.table().kind(), Some(ContainerKind::Array));
        assert_eq!(expansion.table().range(0, 9), [1, 4, 7]);
    }

    #[test]
    fn an_object_expands_to_members_addressed_by_their_keys() {
        let source = br#"{"a":1,"bb":22}"#;
        let expansion = expand(source, 0);

        assert!(expansion.table().keyed());
        assert_eq!(expansion.table().range(0, 9), [1, 7]);
        assert_eq!(source[1], b'"');
    }

    #[test]
    fn expansion_starts_at_any_offset_not_only_zero() {
        // The whole point: tier 2 expands a container found deep inside a file,
        // and the offsets it reports are absolute.
        let source = br#"{"outer":{"a":1,"b":2}}"#;
        let inner = 9;
        assert_eq!(source[inner as usize], b'{');

        let expansion = expand(source, inner);
        assert!(expansion.is_complete());
        assert_eq!(expansion.table().len(), 2);
        assert_eq!(expansion.table().child(0), Some(10));
        assert_eq!(expansion.end(), Some(22));
    }

    #[test]
    fn nested_children_are_not_collected() {
        let expansion = expand(br#"[[1,2],{"a":[3]},4]"#, 0);
        assert_eq!(expansion.table().len(), 3, "three elements, however deep");
    }

    #[test]
    fn an_empty_container_expands_to_nothing() {
        let expansion = expand(b"[]", 0);
        assert!(expansion.is_complete());
        assert!(expansion.table().is_empty());
        assert_eq!(expansion.end(), Some(2));

        let expansion = expand(b"{}", 0);
        assert!(expansion.is_complete());
        assert!(expansion.table().is_empty());
    }

    // ---- resumability -----------------------------------------------------

    #[test]
    fn a_batch_limit_yields_and_resumes_exactly_where_it_stopped() {
        // The property that keeps a five-million-element expansion from stalling
        // the UI: stop after a batch, return, continue later, same answer.
        let mut source = Vec::from(b"[");
        for i in 0..1000 {
            if i > 0 {
                source.push(b',');
            }
            source.extend_from_slice(format!("{i}").as_bytes());
        }
        source.push(b']');

        let options = ExpandOptions {
            batch: 100,
            ..ExpandOptions::default()
        };
        let mut expansion = Expansion::at(0);
        let mut src = source.as_slice();
        let mut batches = 0;

        loop {
            let reason = expansion.advance(&mut src, &options).unwrap();
            batches += 1;
            if !reason.resumable() {
                assert_eq!(reason, Stopped::Closed);
                break;
            }
            assert_eq!(expansion.len() % 100, 0, "batches should land on the limit");
        }

        assert!(batches >= 10, "expected several batches, got {batches}");
        assert_eq!(expansion.table().len(), 1000);

        // And the answer matches expanding it in one go.
        let whole = expand(&source, 0);
        assert_eq!(expansion.table(), whole.table());
    }

    #[test]
    fn the_batch_size_never_changes_the_result() {
        let source = br#"[{"a":1},[2,3],"four",5,null,true,{"b":[6,7]}]"#;
        let reference = expand(source, 0);

        for batch in [1, 2, 3, 7, 1000] {
            let options = ExpandOptions {
                batch,
                ..ExpandOptions::default()
            };
            let mut expansion = Expansion::at(0);
            let mut src = &source[..];
            expansion.run_to_end(&mut src, &options).unwrap();
            assert_eq!(expansion.table(), reference.table(), "batch {batch}");
            assert_eq!(expansion.end(), reference.end(), "batch {batch}");
        }
    }

    #[test]
    fn the_window_size_never_changes_the_result() {
        // Windows cut tokens in half; the lexer's resume point is what makes
        // that invisible here.
        let source = br#"[{"key":"a value with spaces"},[1,2,3],"another string"]"#;
        let reference = expand(source, 0);

        for window in [1u32, 2, 5, 13, 64, 4096] {
            let options = ExpandOptions {
                window,
                batch: 10_000,
            };
            let mut expansion = Expansion::at(0);
            let mut src = &source[..];
            expansion.run_to_end(&mut src, &options).unwrap();
            assert_eq!(expansion.table(), reference.table(), "window {window}");
        }
    }

    #[test]
    fn a_partial_expansion_is_usable_before_it_finishes() {
        let mut source = Vec::from(b"[");
        for i in 0..500 {
            if i > 0 {
                source.push(b',');
            }
            source.extend_from_slice(b"1");
        }
        source.push(b']');

        let options = ExpandOptions {
            batch: 50,
            ..ExpandOptions::default()
        };
        let mut expansion = Expansion::at(0);
        let mut src = source.as_slice();

        assert_eq!(
            expansion.advance(&mut src, &options).unwrap(),
            Stopped::Batch
        );
        assert_eq!(expansion.len(), 50);
        assert!(!expansion.is_complete());
        assert!(expansion.is_resumable());
        // The first fifty are already real, already addressable.
        assert_eq!(expansion.table().child(0), Some(1));
        assert_eq!(expansion.table().child(49), Some(99));
    }

    // ---- degradation ------------------------------------------------------

    #[test]
    fn a_truncated_container_keeps_the_children_it_found() {
        let expansion = expand(b"[1,2,3,4", 0);
        assert_eq!(expansion.stopped(), Some(Stopped::EndOfSource));
        assert!(!expansion.is_complete());
        assert_eq!(expansion.end(), None);
        assert_eq!(expansion.table().len(), 4, "four good elements");
    }

    #[test]
    fn a_malformed_container_keeps_the_children_it_found() {
        // Half a container is still half a container. C6.
        let expansion = expand(br#"[1,2,@,4]"#, 0);
        assert_eq!(expansion.stopped(), Some(Stopped::Malformed));
        assert_eq!(expansion.table().len(), 2);
        assert!(!expansion.is_resumable(), "a syntax error is terminal");
    }

    #[test]
    fn expanding_something_that_is_not_a_container_finds_no_children() {
        let expansion = expand(b"42", 0);
        assert!(expansion.table().is_empty());
        assert!(!expansion.is_complete());
    }

    #[test]
    fn advancing_a_finished_expansion_is_a_no_op() {
        let source = b"[1,2]";
        let mut expansion = Expansion::at(0);
        let mut src = &source[..];
        let options = ExpandOptions::default();

        assert_eq!(
            expansion.run_to_end(&mut src, &options).unwrap(),
            Stopped::Closed
        );
        assert_eq!(
            expansion.advance(&mut src, &options).unwrap(),
            Stopped::Closed
        );
        assert_eq!(expansion.table().len(), 2, "and does not double-count");
    }

    // ---- cache ------------------------------------------------------------

    #[test]
    fn the_cache_returns_the_same_expansion_for_the_same_offset() {
        let source = br#"[[1,2,3],[4,5]]"#;
        let mut cache = ExpansionCache::default();
        let mut src = &source[..];

        cache
            .entry(1)
            .run_to_end(&mut src, &ExpandOptions::default())
            .unwrap();
        assert_eq!(cache.len(), 1);

        // Asking again returns the work already done, not a fresh walk.
        let again = cache.entry(1);
        assert!(again.is_complete());
        assert_eq!(again.table().len(), 3);
        assert_eq!(cache.len(), 1, "no duplicate entry");
    }

    #[test]
    fn eviction_drops_the_least_recently_used() {
        let source = br#"[[1,2,3,4,5,6,7,8],[1,2,3,4,5,6,7,8],[1,2,3,4,5,6,7,8]]"#;
        // Small enough that a second expansion cannot fit beside the first.
        let mut cache = ExpansionCache::new(64);
        let options = ExpandOptions::default();

        for start in [1u64, 20, 39] {
            let mut src = &source[..];
            cache.entry(start).run_to_end(&mut src, &options).unwrap();
            cache.evict_to_budget();
        }

        assert!(cache.heap_bytes() <= 64, "budget must hold");
        assert!(!cache.is_empty(), "the newest survives");
        assert!(cache.get(39).is_some(), "the most recent is kept");
    }

    #[test]
    fn an_evicted_expansion_rebuilds_identically() {
        // Why eviction needs no invalidation protocol: a byte offset is a stable
        // id, so the cache is a pure cache (C3, C36).
        let source = br#"[[10,20,30],[40,50]]"#;
        let options = ExpandOptions::default();

        let mut cache = ExpansionCache::default();
        let mut src = &source[..];
        cache.entry(1).run_to_end(&mut src, &options).unwrap();
        let before = cache.entry(1).table().clone();

        cache.forget(1);
        assert!(cache.get(1).is_none());

        let mut src = &source[..];
        cache.entry(1).run_to_end(&mut src, &options).unwrap();
        assert_eq!(cache.entry(1).table(), &before);
    }

    #[test]
    fn a_completed_expansion_gives_back_its_growth_headroom() {
        // A `Vec` that grew to n entries holds capacity for up to 2n, and that
        // slack is charged against the cache budget forever. Sealing on the
        // terminal stop reclaims it.
        let mut source = Vec::from(b"[");
        for i in 0..1025 {
            if i > 0 {
                source.push(b',');
            }
            source.push(b'1');
        }
        source.push(b']');

        let expansion = expand(&source, 0);
        assert_eq!(expansion.len(), 1025);
        assert_eq!(
            expansion.heap_bytes(),
            1025 * 8,
            "a sealed table holds exactly its contents"
        );
    }

    #[test]
    fn a_partial_expansion_keeps_its_headroom_for_the_next_batch() {
        // The other side of the same decision: shrinking a table that is about
        // to grow again would just thrash the allocator.
        let mut source = Vec::from(b"[");
        for i in 0..500 {
            if i > 0 {
                source.push(b',');
            }
            source.push(b'1');
        }
        source.push(b']');

        let options = ExpandOptions {
            batch: 10,
            ..ExpandOptions::default()
        };
        let mut expansion = Expansion::at(0);
        let mut src = source.as_slice();
        assert_eq!(
            expansion.advance(&mut src, &options).unwrap(),
            Stopped::Batch
        );
        assert!(
            expansion.heap_bytes() >= expansion.len() * 8,
            "capacity is kept while more children are coming"
        );
    }

    #[test]
    fn the_cache_reports_its_own_size() {
        let source = br#"[[1,2,3,4]]"#;
        let mut cache = ExpansionCache::default();
        assert!(cache.is_empty());
        assert_eq!(cache.heap_bytes(), 0);

        let mut src = &source[..];
        cache
            .entry(1)
            .run_to_end(&mut src, &ExpandOptions::default())
            .unwrap();

        assert_eq!(cache.len(), 1);
        assert!(cache.heap_bytes() >= 32, "four children at 8 bytes each");

        cache.clear();
        assert!(cache.is_empty());
        assert_eq!(cache.heap_bytes(), 0);
    }

    #[test]
    fn a_zero_budget_cache_still_serves_the_entry_it_was_asked_for() {
        // Eviction happens before insertion, so the caller never receives a
        // reference to something already discarded.
        let source = br#"[[1,2,3]]"#;
        let mut cache = ExpansionCache::new(0);
        let mut src = &source[..];

        let expansion = cache.entry(1);
        expansion
            .run_to_end(&mut src, &ExpandOptions::default())
            .unwrap();
        assert_eq!(cache.entry(1).table().len(), 3);
    }
}
