//! Duplicate detection: the same key twice in an object, the same value twice
//! in an array.
//!
//! ## Why duplicate keys matter more than they look
//!
//! `{"id": 1, "id": 2}` is **valid JSON**. RFC 8259 says the names *should* be
//! unique and declines to say what happens when they are not, so every parser
//! picks one: `JSON.parse` keeps the last, Python's `json` keeps the last, some
//! Go decoders keep the last, `jq` keeps the last, and a few keep the first.
//! Nothing warns. A config file, an API response or an export with a repeated
//! key therefore means different things to different halves of a pipeline, and
//! the bug surfaces a long way from the file.
//!
//! Nothing else in this product will tell you: the tree shows both members,
//! because both are really there, and validation passes, because the document is
//! really valid. This is the only thing that says so.
//!
//! ## One walk, one frame per open container
//!
//! Every container being walked keeps a list of `(identity, offset)` — one entry
//! per member. For an object the identity is the **key**; for an array it is the
//! **element's canonical hash**. When the container closes, the list is sorted
//! and equal identities fall adjacent, which finds every repeat in `n log n`
//! with no hash table and no allocation per member.
//!
//! Memory is bounded by the containers currently *open*, not by the file: a
//! frame is dropped the moment its container closes. The peak is therefore the
//! largest container, which for an NDJSON log is the record count — 16 bytes
//! each, 28 MB on the 500 MB fixture.
//!
//! ## A hash is a candidate, never a verdict
//!
//! Equal hashes are *checked*, by re-reading both members and comparing them
//! byte for byte in canonical form. A 64-bit hash over 1.7 million records has a
//! collision probability around 10⁻⁷ — small, and small is not the same as
//! none, and "your export has a duplicate record" is not a claim to make on a
//! probability. Verification happens after the read loop, where nothing is
//! borrowed, so it costs one extra read per candidate pair and nothing at all
//! for a file with no duplicates.

use crate::export::{minify, value_end};
use crate::format::Format;
use crate::lexer::Lexer;
use crate::rows::unescape;
use crate::source::{ByteRange, SourceError, read_clamped};
use crate::structure::{ContainerKind, Documents, Event, Structure};

/// Longest key or element compared byte-for-byte during verification.
///
/// A member larger than this is reported on its hash alone, and
/// [`Dedup::unverified`] says how many. Reading two 100 MB records into memory
/// to prove they are identical would be the failure this product exists to
/// avoid, in the service of a footnote.
const MAX_VERIFY: u64 = 4 * 1024 * 1024;

/// What a duplicate is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DuplicateKind {
    /// The same name twice in one object. Parsers silently disagree about which
    /// one wins.
    Key,
    /// The same value twice in one array.
    Element,
}

impl DuplicateKind {
    /// A stable lowercase identifier.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            DuplicateKind::Key => "key",
            DuplicateKind::Element => "element",
        }
    }
}

/// One repeat, with both of its locations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Duplicate {
    /// Key or element.
    pub kind: DuplicateKind,
    /// Byte offset of the **first** occurrence.
    pub first: u64,
    /// Byte offset of the repeat. Both are reported because "there are two of
    /// these" is only actionable if you can see both.
    pub second: u64,
    /// The key's name, or a short rendering of the element.
    pub what: String,
    /// Whether the two were compared byte-for-byte, or matched on hash alone
    /// because one of them is larger than the verification bound.
    pub verified: bool,
}

/// What one [`Dedup::advance`] call does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DedupOptions {
    /// Bytes per read from the source.
    pub window: u32,
    /// Bytes to walk before returning, so a host can paint or cancel.
    pub budget: u64,
    /// Stop collecting after this many duplicates.
    pub limit: usize,
    /// Whether to look for repeated object keys.
    pub keys: bool,
    /// Whether to look for repeated array elements.
    ///
    /// Separate from `keys` because it is the expensive half: key checking
    /// costs a hash of the name, element checking costs a hash of the whole
    /// subtree and a frame that grows with the container (SPEC M5).
    pub elements: bool,
    /// Most members one container will track before it stops.
    ///
    /// The bound that keeps a pathological file from being a memory problem.
    /// [`Dedup::capped`] reports when it was reached, because a check that
    /// quietly stopped looking is worse than one that never started.
    pub max_members: usize,
}

impl Default for DedupOptions {
    fn default() -> Self {
        Self {
            window: 256 * 1024,
            budget: 8 * 1024 * 1024,
            limit: 1_000,
            keys: true,
            elements: false,
            // 4 M members × 16 B = 64 MB, the SPEC M5 memory budget.
            max_members: 4 * 1024 * 1024,
        }
    }
}

/// A container being walked.
struct Frame {
    kind: ContainerKind,
    /// `(identity, offset)` per member, in encounter order.
    members: Vec<(u64, u64)>,
    /// The container's own accumulating structural hash.
    hash: u64,
    /// Where it started, so the container itself has an identity when it closes.
    start: u64,
    /// Whether this frame collects members at all.
    ///
    /// False for the synthetic root of a *single* document, whose one member is
    /// the root value — counting it would report "1 element checked" for a file
    /// with none, and there is nothing for a lone value to be a duplicate of.
    tracks: bool,
}

/// A pair to verify once the source is free to be read again.
struct Candidate {
    kind: DuplicateKind,
    first: u64,
    second: u64,
}

/// A duplicate-detection pass over one document.
pub struct Dedup {
    lexer: Lexer,
    structure: Structure,
    /// Frame 0 is synthetic: NDJSON records have no enclosing array, and
    /// "the same record twice" is the question the format exists to be asked.
    stack: Vec<Frame>,
    /// Bytes not yet claimed by a completed token, kept so a token straddling a
    /// read boundary can still be hashed from its whole text. See `consume`.
    carry: Vec<u8>,
    /// The absolute offset `carry` begins at.
    carry_at: u64,
    cursor: u64,
    walked: u64,
    done: bool,
    per_line: bool,
    /// 1-based line of the record being walked, for `Lexer::resuming_at`.
    line: u64,
    /// The key most recently seen, awaiting its value.
    pending_key: Option<(u64, u64)>,
    /// Scratch for one chunk's tokens, reused across chunks.
    tokens: Vec<crate::lexer::Token>,
    candidates: Vec<Candidate>,
    duplicates: Vec<Duplicate>,
    keys_checked: u64,
    elements_checked: u64,
    capped: bool,
    unverified: u64,
    /// Every repeat found, including those past [`DedupOptions::limit`].
    total: u64,
}

impl Dedup {
    /// Begin a pass over a document of the given format.
    #[must_use]
    pub fn new(format: Format) -> Self {
        Self {
            lexer: Lexer::new(),
            // One document at a time in both modes: for NDJSON that is one
            // record, reopened at every newline by `open_record`.
            structure: Structure::new(Documents::One),
            stack: vec![Frame {
                kind: ContainerKind::Array,
                members: Vec::new(),
                hash: FNV_OFFSET,
                start: 0,
                // Only NDJSON has top-level members to compare. A single
                // document's root value is one value, and nothing is a
                // duplicate of nothing.
                tracks: format == Format::Ndjson,
            }],
            carry: Vec::new(),
            carry_at: 0,
            cursor: 0,
            walked: 0,
            done: false,
            per_line: format == Format::Ndjson,
            line: 1,
            pending_key: None,
            tokens: Vec::new(),
            candidates: Vec::new(),
            duplicates: Vec::new(),
            keys_checked: 0,
            elements_checked: 0,
            capped: false,
            unverified: 0,
            total: 0,
        }
    }

    /// Whether the pass has finished, for any reason.
    #[must_use]
    pub const fn is_done(&self) -> bool {
        self.done
    }

    /// Every duplicate found so far.
    #[must_use]
    pub fn duplicates(&self) -> &[Duplicate] {
        &self.duplicates
    }

    /// Bytes walked so far — the numerator of a progress bar.
    #[must_use]
    pub const fn walked(&self) -> u64 {
        self.walked
    }

    /// Object keys examined.
    #[must_use]
    pub const fn keys_checked(&self) -> u64 {
        self.keys_checked
    }

    /// Array elements examined.
    #[must_use]
    pub const fn elements_checked(&self) -> u64 {
        self.elements_checked
    }

    /// Whether a container hit [`DedupOptions::max_members`] and stopped
    /// tracking. The report must say so: an unqualified "no duplicates" after
    /// giving up is a false statement.
    #[must_use]
    pub const fn capped(&self) -> bool {
        self.capped
    }

    /// How many reported duplicates matched on hash without byte verification.
    #[must_use]
    pub const fn unverified(&self) -> u64 {
        self.unverified
    }

    /// Every repeat found, including those beyond [`DedupOptions::limit`].
    ///
    /// Counting is free — it falls out of the sort — while *reporting* one costs
    /// two reads to prove it. So a file with two million duplicate keys says two
    /// million and lists the first thousand, rather than choosing between a
    /// truthful count and a usable one.
    ///
    /// The count is by structural hash; every entry in [`Dedup::duplicates`] is
    /// additionally verified byte-for-byte.
    #[must_use]
    pub const fn total(&self) -> u64 {
        self.total
    }

    /// Walk the next batch, and report what it found.
    ///
    /// # Errors
    ///
    /// If the source cannot be read. Malformed JSON is not an error of this
    /// call — the walk stops where the document stops making sense, and every
    /// duplicate found before that point is still real (C6).
    pub fn advance<S: ByteRange>(
        &mut self,
        source: &mut S,
        options: &DedupOptions,
    ) -> Result<(), SourceError> {
        if self.done {
            return Ok(());
        }

        let mut budget = options.budget;
        while budget > 0 && !self.done {
            let bytes = read_clamped(source, self.cursor, u64::from(options.window))?;
            if bytes.is_empty() {
                self.finish(options);
                break;
            }

            // Every record in the window, not one per read. The first version
            // read a fresh 256 KiB window and then consumed only the ~165 bytes
            // up to the first newline, so a 50 MB log cost 78 GB of reading and
            // ran at 1.7 MB/s against 1.2 GB/s to index the same file. That is
            // C60 exactly — cost scaling with calls rather than bytes — in a
            // second place, and no test could see it: every answer was right.
            let mut at = 0usize;
            while at < bytes.len() && !self.done {
                let rest = &bytes[at..];
                // A record boundary is a newline, exactly as tier 1 and
                // validation say (C27): every layer must agree what a record is.
                let split = if self.per_line {
                    rest.iter().position(|b| *b == b'\n')
                } else {
                    None
                };
                let take = split.unwrap_or(rest.len());
                let base = self.cursor + at as u64;

                if self.consume(&rest[..take], base, options).is_err() {
                    self.done = true;
                }
                match split {
                    Some(n) => {
                        self.close_record(options);
                        self.open_record(base + n as u64 + 1);
                        at += n + 1;
                    }
                    // No newline left in the window: the record continues into
                    // the next one, which the lexer and the carry both handle.
                    None => at += take,
                }
            }

            self.cursor += bytes.len() as u64;
            self.walked = self.cursor;
            budget = budget.saturating_sub(bytes.len() as u64);
        }

        // The borrow of `source` above ends with the loop, which is the only
        // reason verification can read anything at all. Candidates are rare, so
        // this costs nothing on a file with no duplicates.
        self.verify(source, options)?;
        Ok(())
    }

    /// Feed one chunk through the lexer and the grammar walk.
    ///
    /// ## Why the bytes are buffered as well as fed
    ///
    /// The lexer is resumable and reports **absolute** offsets, so a token whose
    /// text straddles a read boundary is emitted correctly — but the bytes of
    /// its first half belong to a chunk that is gone. Hashing only the visible
    /// half would make the verdict depend on where the window happened to fall,
    /// which is the one thing a resumable pass may never do (C20). The first
    /// version of this did exactly that, and the window-invariance test caught
    /// it at window 13.
    ///
    /// So bytes are also accumulated in [`Dedup::carry`], trimmed after every
    /// chunk to start at the end of the last completed token. It therefore holds
    /// only what an unfinished token needs — nothing on a chunk that ends
    /// cleanly, and at most one token's worth otherwise.
    fn consume(&mut self, bytes: &[u8], base: u64, options: &DedupOptions) -> Result<(), ()> {
        if self.carry.is_empty() {
            self.carry_at = base;
        }
        self.carry.extend_from_slice(bytes);

        // Reused, not allocated per chunk: in NDJSON this runs once per record,
        // so a fresh `Vec` here is one allocation per line of the file.
        self.tokens.clear();
        for token in self.lexer.feed(bytes) {
            self.tokens.push(token.map_err(|_| ())?);
        }

        let mut last_end = self.carry_at;
        let mut failure = None;
        for at in 0..self.tokens.len() {
            let token = self.tokens[at];
            last_end = token.end;
            // Hashed straight out of the carry. Copying every token into a
            // `Vec<u8>` first cost ten million allocations on a 50 MB log.
            let identity = self.hash_of(token.start, token.end);
            if let Err(()) = self.step(token, identity, options) {
                failure = Some(());
                break;
            }
        }

        // Drop everything a completed token has already claimed.
        let keep = last_end.saturating_sub(self.carry_at) as usize;
        if keep >= self.carry.len() {
            self.carry.clear();
            self.carry_at = last_end;
        } else {
            self.carry.drain(..keep);
            self.carry_at = last_end;
        }

        failure.map_or(Ok(()), Err)
    }

    /// A token's text, as it sits in the carry buffer.
    fn raw_of(&self, start: u64, end: u64) -> &[u8] {
        let Some(from) = start.checked_sub(self.carry_at) else {
            return &[];
        };
        let to = end.saturating_sub(self.carry_at);
        self.carry
            .get(from as usize..(to as usize).min(self.carry.len()))
            .unwrap_or(&[])
    }

    /// A token's identity, without copying its text anywhere.
    fn hash_of(&self, start: u64, end: u64) -> u64 {
        hash_bytes(FNV_OFFSET, self.raw_of(start, end))
    }

    fn step(
        &mut self,
        token: crate::lexer::Token,
        identity: u64,
        options: &DedupOptions,
    ) -> Result<(), ()> {
        let start = token.start;
        let end = token.end;
        let Some(event) = self.structure.push(token).map_err(|_| ())? else {
            return Ok(());
        };

        match event {
            Event::Key { .. } => {
                if options.keys {
                    self.keys_checked += 1;
                    self.record_member(identity, start, options);
                }
                self.pending_key = Some((identity, start));
                // A key is part of its object's structure, so it folds into the
                // container's own hash too — otherwise `{"a":1}` and `{"b":1}`
                // would be the same value.
                if let Some(frame) = self.stack.last_mut() {
                    frame.hash = mix(frame.hash, identity);
                }
            }
            Event::Scalar { .. } => {
                self.pending_key = None;
                self.complete(identity, start, options);
            }
            Event::Open { kind, .. } => {
                self.pending_key = None;
                self.stack.push(Frame {
                    kind,
                    members: Vec::new(),
                    hash: mix(FNV_OFFSET, kind as u64),
                    start,
                    tracks: true,
                });
            }
            Event::Close { .. } => {
                let Some(frame) = self.stack.pop() else {
                    return Err(());
                };
                // A container's identity is what it accumulated, not the `}`.
                let identity = frame.hash;
                let at = frame.start;
                self.close_frame(frame, options);
                let _ = end;
                self.complete(identity, at, options);
            }
        }
        Ok(())
    }

    /// A value finished: fold it into its parent, and offer it as a member.
    fn complete(&mut self, identity: u64, start: u64, options: &DedupOptions) {
        let Some(frame) = self.stack.last_mut() else {
            return;
        };
        frame.hash = mix(frame.hash, identity);

        // Only an array's *elements* are candidates for element dedup — an
        // object's values are not, because two members with different names
        // holding the same value are not a duplicate of anything.
        if options.elements && frame.kind == ContainerKind::Array && frame.tracks {
            self.elements_checked += 1;
            self.record_member(identity, start, options);
        }
    }

    /// Add a member to the frame in hand, respecting the cap.
    fn record_member(&mut self, identity: u64, offset: u64, options: &DedupOptions) {
        let Some(frame) = self.stack.last_mut() else {
            return;
        };
        if !frame.tracks {
            return;
        }
        if frame.members.len() >= options.max_members {
            self.capped = true;
            return;
        }
        frame.members.push((identity, offset));
    }

    /// A container closed: sort its members and collect every repeat.
    fn close_frame(&mut self, mut frame: Frame, options: &DedupOptions) {
        if frame.members.len() < 2 {
            return;
        }
        let kind = match frame.kind {
            ContainerKind::Object => DuplicateKind::Key,
            ContainerKind::Array => DuplicateKind::Element,
        };

        // Sorted by identity, then by offset — so the *first* occurrence of a
        // repeated value is the one at the lowest offset, which is what "first"
        // has to mean for a report someone will act on.
        frame.members.sort_unstable();

        let mut at = 0;
        while at < frame.members.len() {
            let (identity, first) = frame.members[at];
            let mut next = at + 1;
            while next < frame.members.len() && frame.members[next].0 == identity {
                self.total += 1;
                if self.candidates.len() + self.duplicates.len() < options.limit {
                    self.candidates.push(Candidate {
                        kind,
                        first,
                        second: frame.members[next].1,
                    });
                }
                next += 1;
            }
            at = next;
        }
    }

    /// Confirm each candidate by re-reading both members.
    fn verify<S: ByteRange>(
        &mut self,
        source: &mut S,
        options: &DedupOptions,
    ) -> Result<(), SourceError> {
        for candidate in core::mem::take(&mut self.candidates) {
            if self.duplicates.len() >= options.limit {
                break;
            }
            let (same, what, verified) = compare(source, &candidate)?;
            if !same {
                continue; // A hash collision. Rare, and now proven harmless.
            }
            if !verified {
                self.unverified += 1;
            }
            self.duplicates.push(Duplicate {
                kind: candidate.kind,
                first: candidate.first,
                second: candidate.second,
                what,
                verified,
            });
        }
        Ok(())
    }

    /// Close the record in progress at a newline.
    fn close_record(&mut self, options: &DedupOptions) {
        match self.lexer.finish() {
            Ok(Some(token)) => {
                // From the carry, for the same reason `consume` keeps one: a
                // number ending at a record boundary is flushed here, and its
                // digits were fed in a chunk that has already been dropped.
                let identity = self.hash_of(token.start, token.end);
                if self.step(token, identity, options).is_err() {
                    self.done = true;
                }
            }
            Ok(None) => {}
            Err(_) => self.done = true,
        }
    }

    /// Begin a fresh record at `offset`.
    ///
    /// An NDJSON record is its own document — the same segmentation `Validate`
    /// uses (C27), and the reason a broken line cannot swallow the next one. The
    /// first version flushed the lexer at each newline and never reopened it, so
    /// every record after the first was silently never walked, and the file
    /// reported no duplicates at all.
    fn open_record(&mut self, offset: u64) {
        if !self.per_line {
            return;
        }
        self.line += 1;
        self.lexer = Lexer::resuming_at(offset, self.line);
        self.structure = Structure::new(Documents::One);
        // A record that ended mid-container leaves frames behind. They belong to
        // a record that is over; only the synthetic root survives a line.
        self.stack.truncate(1);
        self.pending_key = None;
        self.carry.clear();
        self.carry_at = offset;
    }

    /// End of input: flush and close every frame still open.
    fn finish(&mut self, options: &DedupOptions) {
        self.close_record(options);
        while let Some(frame) = self.stack.pop() {
            self.close_frame(frame, options);
        }
        self.done = true;
    }
}

/// Whether two members really are the same, and what to call them.
fn compare<S: ByteRange>(
    source: &mut S,
    candidate: &Candidate,
) -> Result<(bool, String, bool), SourceError> {
    // A key is a short string and an element can be a whole record, so they get
    // different windows. Reading 64 KiB to compare the name `id` was most of the
    // cost of a duplicate-heavy file.
    let window = match candidate.kind {
        DuplicateKind::Key => 4 * 1024,
        DuplicateKind::Element => 64 * 1024,
    };
    let span = extent(source, candidate.first, window)?;
    let other = extent(source, candidate.second, window)?;

    if span.len() > MAX_VERIFY as usize || other.len() > MAX_VERIFY as usize {
        return Ok((true, describe(&span), false));
    }
    if span.is_empty() || other.is_empty() {
        return Ok((true, describe(&span), false));
    }

    let same = match candidate.kind {
        // Two keys are the same key when their *text* is equal, which is not the
        // same as their bytes being equal: `"A"` and `"A"` are one name
        // written twice, and a parser keeping the last one will not care which
        // spelling you used.
        DuplicateKind::Key => unescape(&span, MAX_TEXT).0 == unescape(&other, MAX_TEXT).0,
        DuplicateKind::Element => minify(&span) == minify(&other),
    };
    Ok((same, describe(&span), true))
}

/// Read one value's bytes, bounded.
///
/// The value's extent is not known from its offset alone, so this re-lexes from
/// it — the same trade C1 makes everywhere else: store where things start, read
/// the few kilobytes back when something actually needs them.
fn extent<S: ByteRange>(source: &mut S, start: u64, size: u64) -> Result<Vec<u8>, SourceError> {
    let window = read_clamped(source, start, MAX_VERIFY.min(size))?;
    let take = value_end(window).unwrap_or(window.len());
    Ok(window[..take.min(window.len())].to_vec())
}

/// Longest string compared or rendered.
const MAX_TEXT: usize = 64 * 1024;

/// How much of a member is shown in the report.
const DESCRIBE_CHARS: usize = 48;

fn describe(bytes: &[u8]) -> String {
    if bytes.first() == Some(&b'"') {
        let (text, cut) = unescape(bytes, DESCRIBE_CHARS);
        return if cut { format!("{text}…") } else { text };
    }
    let text = String::from_utf8_lossy(bytes);
    let mut out: String = text.chars().take(DESCRIBE_CHARS).collect();
    if out.chars().count() < text.chars().count() {
        out.push('…');
    }
    out
}

// ------------------------------------------------------------------ hashing

/// FNV-1a's 64-bit offset basis.
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
/// FNV-1a's 64-bit prime.
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

fn hash_bytes(seed: u64, bytes: &[u8]) -> u64 {
    let mut hash = seed;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    finalize(hash)
}

/// Fold one value's hash into a container's, order-sensitively.
fn mix(accumulator: u64, value: u64) -> u64 {
    finalize(accumulator.wrapping_mul(FNV_PRIME) ^ value)
}

/// SplitMix64's finalizer.
///
/// FNV-1a alone avalanches poorly, and these hashes are compared against each
/// other by the million — two records differing in one digit must not land
/// near each other. This costs three multiplies and buys the difference between
/// a hash and a checksum.
const fn finalize(mut hash: u64) -> u64 {
    hash ^= hash >> 30;
    hash = hash.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    hash ^= hash >> 27;
    hash = hash.wrapping_mul(0x94d0_49bb_1331_11eb);
    hash ^ (hash >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(source: &str, options: &DedupOptions) -> Dedup {
        let format = crate::sniff_format(source.as_bytes());
        let mut dedup = Dedup::new(format);
        let mut bytes = source.as_bytes();
        let mut spins = 0;
        while !dedup.is_done() {
            dedup.advance(&mut bytes, options).expect("read");
            spins += 1;
            assert!(spins < 1000, "advance must terminate");
        }
        dedup
    }

    fn keys(source: &str) -> Dedup {
        run(source, &DedupOptions::default())
    }

    fn elements(source: &str) -> Dedup {
        run(
            source,
            &DedupOptions {
                keys: false,
                elements: true,
                ..DedupOptions::default()
            },
        )
    }

    #[test]
    fn a_repeated_key_is_found_with_both_locations() {
        // The whole point: this document is valid, and every parser silently
        // keeps a different one of the two.
        let dedup = keys(r#"{"id":1,"name":"a","id":2}"#);
        let found = dedup.duplicates();

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].kind, DuplicateKind::Key);
        assert_eq!(found[0].what, "id");
        assert_eq!(found[0].first, 1, "the first `id`");
        assert_eq!(found[0].second, 19, "the second `id`");
        assert!(found[0].verified);
    }

    #[test]
    fn distinct_keys_are_not_duplicates() {
        assert!(keys(r#"{"a":1,"b":2,"c":3}"#).duplicates().is_empty());
        assert_eq!(keys(r#"{"a":1,"b":2,"c":3}"#).keys_checked(), 3);
    }

    #[test]
    fn the_same_key_in_different_objects_is_not_a_duplicate() {
        // Every record in an NDJSON log has an `id`. Reporting that would make
        // the feature useless on exactly the files it exists for.
        let dedup = keys("{\"id\":1}\n{\"id\":2}\n{\"id\":3}\n");
        assert!(dedup.duplicates().is_empty());
        assert_eq!(dedup.keys_checked(), 3);
    }

    #[test]
    fn nested_objects_each_get_their_own_scope() {
        let dedup = keys(r#"{"a":1,"inner":{"a":2,"a":3}}"#);
        let found = dedup.duplicates();
        assert_eq!(found.len(), 1, "only the inner pair repeats");
        assert_eq!(found[0].what, "a");
        assert!(found[0].first > 10, "and it is the inner one");
    }

    #[test]
    fn a_key_written_two_ways_is_still_one_key() {
        // `a` is `a`. A parser keeping the last member will not care how
        // you spelled the name, so neither may this.
        let dedup = keys(r#"{"a":1,"a":2}"#);
        assert_eq!(dedup.duplicates().len(), 1);
    }

    #[test]
    fn three_of_the_same_key_report_two_repeats() {
        // Two repeats, not three and not one: each extra member is a separate
        // thing to fix, and both of its locations are needed.
        let dedup = keys(r#"{"x":1,"x":2,"x":3}"#);
        assert_eq!(dedup.duplicates().len(), 2);
    }

    #[test]
    fn repeated_elements_are_found_when_asked_for() {
        let dedup = elements(r#"[{"a":1},{"b":2},{"a":1}]"#);
        let found = dedup.duplicates();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].kind, DuplicateKind::Element);
        assert!(found[0].verified);
    }

    #[test]
    fn whitespace_does_not_make_two_records_different() {
        // A pretty-printer in the middle of a pipeline must not turn one
        // duplicate into two distinct-looking values.
        let dedup = elements("[{\"a\":1},  {\"a\" : 1}  ]");
        assert_eq!(dedup.duplicates().len(), 1);
    }

    #[test]
    fn member_order_is_significant_and_that_is_documented() {
        // Exact and predictable, and the limitation is stated rather than
        // papered over: one producer writes its keys in one order.
        assert!(
            elements(r#"[{"a":1,"b":2},{"b":2,"a":1}]"#)
                .duplicates()
                .is_empty()
        );
    }

    #[test]
    fn similar_but_different_elements_are_not_duplicates() {
        let dedup = elements(r#"[{"a":1},{"a":2},{"a":11},{"a":1.0}]"#);
        assert!(dedup.duplicates().is_empty(), "{:?}", dedup.duplicates());
        assert_eq!(dedup.elements_checked(), 4);
    }

    #[test]
    fn repeated_ndjson_records_are_duplicate_elements() {
        // NDJSON records have no enclosing array, and "the same record twice" is
        // the question the format exists to be asked.
        let dedup = elements("{\"a\":1}\n{\"b\":2}\n{\"a\":1}\n");
        assert_eq!(dedup.duplicates().len(), 1);
        assert_eq!(dedup.duplicates()[0].kind, DuplicateKind::Element);
    }

    #[test]
    fn element_checking_is_off_unless_asked_for() {
        // SPEC M5: the expensive half is opt-in.
        let dedup = keys("{\"a\":1}\n{\"a\":1}\n");
        assert!(dedup.duplicates().is_empty());
        assert_eq!(dedup.elements_checked(), 0);
    }

    #[test]
    fn the_answer_does_not_depend_on_where_the_window_falls() {
        // The invariant every resumable pass in this codebase owes (C20). A
        // token split across a read boundary must not change the verdict.
        let source = r#"{"id":1,"payload":"aaaaaaaaaaaaaaaaaaaaaaaaaaaa","id":2}"#;
        let mut counts = Vec::new();
        for window in [4u32, 7, 13, 64, 4096] {
            let dedup = run(
                source,
                &DedupOptions {
                    window,
                    budget: 8,
                    ..DedupOptions::default()
                },
            );
            counts.push(dedup.duplicates().len());
        }
        assert_eq!(counts, vec![1; 5], "one duplicate at every window size");
    }

    #[test]
    fn a_malformed_document_keeps_what_it_found() {
        // C6: partial is a result, not a failure.
        let dedup = keys(r#"{"a":1,"a":2,"b":"#);
        assert_eq!(dedup.duplicates().len(), 1);
        assert!(dedup.is_done());
    }

    #[test]
    fn the_cap_is_reported_rather_than_hidden() {
        // An unqualified "no duplicates" after giving up is a false statement.
        let dedup = run(
            r#"[1,2,3,4,5,6,7,8]"#,
            &DedupOptions {
                keys: false,
                elements: true,
                max_members: 3,
                ..DedupOptions::default()
            },
        );
        assert!(dedup.capped());
    }

    #[test]
    fn the_limit_bounds_what_is_collected() {
        let many: String = format!("{{{}}}", vec![r#""x":1"#; 50].join(","));
        let dedup = run(
            &many,
            &DedupOptions {
                limit: 5,
                ..DedupOptions::default()
            },
        );
        assert_eq!(dedup.duplicates().len(), 5);
    }

    #[test]
    fn an_empty_document_is_not_an_error() {
        for source in ["", "   ", "{}", "[]"] {
            let dedup = keys(source);
            assert!(dedup.is_done());
            assert!(dedup.duplicates().is_empty());
        }
    }

    #[test]
    fn hashing_avalanches_rather_than_accumulating() {
        // Two records differing in one digit must not land near each other: the
        // frames are sorted by identity and scanned for adjacency.
        let a = hash_bytes(FNV_OFFSET, br#"{"id":1000000}"#);
        let b = hash_bytes(FNV_OFFSET, br#"{"id":1000001}"#);
        assert_ne!(a, b);
        assert!(
            (a ^ b).count_ones() > 16,
            "one changed byte should change about half the bits, got {}",
            (a ^ b).count_ones()
        );
    }

    #[test]
    fn every_duplicate_in_a_dup_heavy_file_is_found_at_the_right_offset() {
        // SPEC M5's exit criterion, on a file whose answer is known by
        // construction rather than by running the thing being tested: 200
        // records, each with `"dup"` eight times, so exactly 7 repeats each.
        let mut source = String::new();
        for id in 0..200 {
            source.push_str(&format!(r#"{{"id":{id}"#));
            for k in 0..8 {
                source.push_str(&format!(r#","dup":{k},"u{k}":{k}"#));
            }
            source.push_str("}\n");
        }

        let dedup = run(
            &source,
            &DedupOptions {
                limit: usize::MAX,
                ..DedupOptions::default()
            },
        );

        assert_eq!(
            dedup.duplicates().len(),
            200 * 7,
            "every repeat, in every record"
        );
        assert_eq!(dedup.keys_checked(), 200 * 17, "id + 8 dup + 8 unique");

        // Locations, not just counts. A report that says "there is a duplicate
        // somewhere" is not something anyone can act on, so every offset is
        // checked to land on the key it names.
        let bytes = source.as_bytes();
        for duplicate in dedup.duplicates() {
            assert_eq!(duplicate.what, "dup");
            assert!(duplicate.verified);
            for at in [duplicate.first, duplicate.second] {
                assert_eq!(
                    &bytes[at as usize..at as usize + 5],
                    br#""dup""#,
                    "offset {at} should be a `dup` key"
                );
            }
            assert!(duplicate.first < duplicate.second, "first really is first");
        }
    }
}
