//! WebAssembly bindings for [`leviathan_core`].
//!
//! **This crate decides nothing about JSON.** Every function here either
//! translates a value between JS and Rust or marshals a payload across the
//! boundary; anything that inspects, parses, or interprets a document belongs in
//! `leviathan-core`, where the native CLI and the future MCP server can reach it
//! and where it can be tested without a browser. The one non-trivial thing that
//! does live here is [`pack`] — a byte layout, which is a property of the
//! boundary and of nothing else.
//!
//! ## How bytes get in
//!
//! They are pulled, not pushed. The host supplies a `ByteReader` — an object
//! with a **synchronous** `read(start, len)` returning a `Uint8Array` — and the
//! core asks it for ranges through [`ByteRange`](leviathan_core::ByteRange),
//! exactly as the CLI's `FileSource` asks a file.
//!
//! Synchronous is the load-bearing word, and it is why this works at all in a
//! browser: `Blob.slice().arrayBuffer()` is a promise, and a promise cannot be
//! awaited from inside a WASM call. `FileReaderSync` can, and it exists *only*
//! in a Worker — which is precisely where all of this is required to run
//! anyway. Blocking a Worker is free; blocking the main thread is the bug this
//! project exists to fix. See `DEEP_REASONING.md` C42.
//!
//! ## Offsets are `f64`
//!
//! Byte offsets cross as JavaScript numbers, not `BigInt`s. A double represents
//! every integer up to 2^53 exactly — nine petabytes — so the precision is not
//! in question, and the alternative would put `BigInt` conversions in the
//! renderer's hot path for a range no file will ever occupy.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

mod pack;

use leviathan_core::{
    Build, BuildOptions, Built, ByteRange, Dedup, DedupOptions, ExpandOptions, ExpansionCache,
    Export, ExportFormat, Filter, Find, FindOptions, FindStop, Format, RowOptions, Schema,
    SourceError, Stopped, Validate, ValidateOptions, materialize, rows_of, sniff_format,
};
use wasm_bindgen::prelude::*;

/// How much of a file is enough to tell single-document from NDJSON.
///
/// Mirrors `SNIFF_PREFIX_BYTES` in the TypeScript protocol.
const SNIFF_PREFIX_BYTES: u64 = 64 * 1024;

/// Records checked per `schemaStep`, so the Worker yields between batches.
const SCHEMA_BATCH: usize = 2_000;

/// Schema problems collected before a pass gives up. A file whose every record
/// is wrong has as many problems as it has records, and nobody reads the
/// thousandth.
const SCHEMA_ERROR_LIMIT: usize = 1_000;

/// Records converted per `exportStep`, so the Worker yields and can write.
const EXPORT_BATCH: usize = 2_000;

/// Records tested per `filterStep`, so the Worker yields between batches.
const FILTER_BATCH: usize = 2_000;

/// How many matching rows are listed. The *count* keeps going past this — a
/// filter that matches half the file should still say so.
const FILTER_MATCH_LIMIT: usize = 10_000;

/// Bytes of records read per host call while filtering. See `filterStep`.
const FILTER_WINDOW: u64 = 1 << 20;

/// Version of the `leviathan-core` engine compiled into this module.
///
/// The extension asserts this against its own expected version at startup, so a
/// stale `.wasm` in `dist/` fails loudly instead of behaving strangely.
#[wasm_bindgen(js_name = coreVersion)]
#[must_use]
pub fn core_version() -> String {
    leviathan_core::VERSION.to_string()
}

/// The row-buffer layout version this module encodes.
///
/// Asserted by the TypeScript decoder before it reads a single row.
#[wasm_bindgen(js_name = rowLayoutVersion)]
#[must_use]
pub fn row_layout_version() -> u32 {
    pack::LAYOUT_VERSION
}

/// Boundary smoke test: returns its argument unchanged.
///
/// The M0 exit criterion, and kept permanently as a startup self-check.
#[wasm_bindgen]
#[must_use]
pub fn echo(value: u32) -> u32 {
    leviathan_core::echo(value)
}

/// Detect whether a prefix of the input is a single JSON document or NDJSON.
///
/// Returns one of `"single-document"`, `"ndjson"`, `"empty"`, `"unknown"`.
///
/// Takes a prefix — 64 KiB is ample. `&[u8]` is copied into WASM memory by
/// `wasm-bindgen`, which is exactly why this takes a prefix and not a file.
#[wasm_bindgen(js_name = sniffFormat)]
#[must_use]
pub fn sniff_format_js(prefix: &[u8]) -> String {
    sniff_format(prefix).as_str().to_string()
}

#[wasm_bindgen]
extern "C" {
    /// The host's byte-range reader: `{ read(start, len): Uint8Array }`.
    ///
    /// Must be synchronous, and may return fewer bytes than asked for at the
    /// end of the source — a short read there is the normal condition, not an
    /// error (C40).
    pub type ByteReader;

    #[wasm_bindgen(method, catch, js_name = read)]
    fn read_range(this: &ByteReader, start: f64, len: f64) -> Result<Vec<u8>, JsValue>;
}

/// A [`ByteRange`] backed by the host's reader.
///
/// The third implementor of the trait, after `&[u8]` and the CLI's `FileSource`
/// — and the one the sans-IO design was actually for (C2, C35).
struct JsSource {
    reader: ByteReader,
    len: u64,
    scratch: Vec<u8>,
}

impl ByteRange for JsSource {
    fn read(&mut self, start: u64, len: u32) -> Result<&[u8], SourceError> {
        if start > self.len {
            return Err(SourceError::OutOfRange {
                start,
                len,
                available: self.len,
            });
        }

        self.scratch = self
            .reader
            .read_range(start as f64, f64::from(len))
            .map_err(|thrown| SourceError::Unavailable(describe(&thrown)))?;

        // The host is trusted to clamp at end-of-source but not to be correct
        // about it: a reader that over-delivers would hand the lexer bytes that
        // are not there, and every offset after that point would be wrong.
        let available = usize::try_from(self.len.saturating_sub(start)).unwrap_or(usize::MAX);
        self.scratch.truncate(available.min(len as usize));
        Ok(&self.scratch)
    }

    fn len_hint(&self) -> Option<u64> {
        Some(self.len)
    }
}

fn describe(thrown: &JsValue) -> String {
    thrown
        .as_string()
        .unwrap_or_else(|| "the host could not read the file".to_string())
}

/// How far a tier-1 index has got.
#[wasm_bindgen]
#[derive(Debug, Clone, Copy)]
pub struct Progress {
    consumed: f64,
    total: f64,
    rows: u32,
    done: bool,
    malformed: bool,
    exhausted: bool,
}

#[wasm_bindgen]
impl Progress {
    /// Bytes indexed so far.
    #[wasm_bindgen(getter)]
    #[must_use]
    pub fn consumed(&self) -> f64 {
        self.consumed
    }

    /// Bytes in the file.
    #[wasm_bindgen(getter)]
    #[must_use]
    pub fn total(&self) -> f64 {
        self.total
    }

    /// Root-level rows found so far. Paintable immediately.
    #[wasm_bindgen(getter)]
    #[must_use]
    pub fn rows(&self) -> u32 {
        self.rows
    }

    /// Whether indexing has stopped, for any reason.
    #[wasm_bindgen(getter)]
    #[must_use]
    pub fn done(&self) -> bool {
        self.done
    }

    /// Whether it stopped because the document does not parse. The rows found
    /// before that point are still real rows (C6) — this is something to show
    /// the user, not something to refuse them.
    #[wasm_bindgen(getter)]
    #[must_use]
    pub fn malformed(&self) -> bool {
        self.malformed
    }

    /// Whether it stopped because the index would not fit in memory.
    ///
    /// Distinct from `malformed` and from a read failure, because the three
    /// send a user to three different places: fix the file, check the disk, or
    /// accept that this shape does not fit in a 32-bit address space. The rows
    /// found before the limit are still real rows.
    #[wasm_bindgen(getter)]
    #[must_use]
    pub fn exhausted(&self) -> bool {
        self.exhausted
    }
}

/// How far one container's expansion has got.
#[wasm_bindgen]
#[derive(Debug, Clone, Copy)]
pub struct Expanded {
    children: u32,
    done: bool,
    complete: bool,
}

#[wasm_bindgen]
impl Expanded {
    /// Children indexed so far. Paintable immediately.
    #[wasm_bindgen(getter)]
    #[must_use]
    pub fn children(&self) -> u32 {
        self.children
    }

    /// Whether expansion has stopped, for any reason.
    #[wasm_bindgen(getter)]
    #[must_use]
    pub fn done(&self) -> bool {
        self.done
    }

    /// Whether the container was walked all the way to its closing bracket. A
    /// container that was truncated or malformed reports `done` without
    /// `complete`, and still yields every child it found.
    #[wasm_bindgen(getter)]
    #[must_use]
    pub fn complete(&self) -> bool {
        self.complete
    }
}

/// How far a search has got, and what it has found.
///
/// The matched rows cross as a `Float64Array` rather than as an array of
/// objects, for the same reason rows do (C43): a search for a common string
/// yields thousands of results, and thousands of JS objects allocated inside a
/// scan the user is watching is exactly the stall the scan was made resumable to
/// avoid. Doubles because a row index is small and an offset is exact to 2^53.
#[wasm_bindgen]
pub struct Found {
    rows: Vec<f64>,
    matches: u32,
    pending: u32,
    scanned: f64,
    done: bool,
    limited: bool,
}

#[wasm_bindgen]
impl Found {
    /// Row indices of every match so far, ascending, one per match.
    ///
    /// Duplicates are meaningful: two hits in one record are two entries with
    /// the same row, and collapsing them here would make "3 of 12" disagree with
    /// what the user can see.
    #[wasm_bindgen(getter)]
    #[must_use]
    pub fn rows(&self) -> Vec<f64> {
        self.rows.clone()
    }

    /// Matches found so far, including any that fell before the first row.
    #[wasm_bindgen(getter)]
    #[must_use]
    pub fn matches(&self) -> u32 {
        self.matches
    }

    /// Matches found in a part of the file that has no rows yet.
    ///
    /// Zero in the ordinary case, because a search runs over a finished index.
    /// Non-zero only if indexing was cancelled: the string really is in the
    /// file, and there is no row to send anyone to. The count is reported rather
    /// than quietly dropped — a find that says "12 matches" while listing 9 is
    /// the kind of thing that makes a tool untrustworthy.
    #[wasm_bindgen(getter)]
    #[must_use]
    pub fn pending(&self) -> u32 {
        self.pending
    }

    /// Bytes read so far — the numerator of the search progress bar.
    #[wasm_bindgen(getter)]
    #[must_use]
    pub fn scanned(&self) -> f64 {
        self.scanned
    }

    /// Whether the scan has stopped, for any reason.
    #[wasm_bindgen(getter)]
    #[must_use]
    pub fn done(&self) -> bool {
        self.done
    }

    /// Whether it stopped at the match limit rather than at the end of the file.
    ///
    /// The difference the UI must show: "1,024 matches" and "10,000+ matches"
    /// are different claims, and only one of them is a count.
    #[wasm_bindgen(getter)]
    #[must_use]
    pub fn limited(&self) -> bool {
        self.limited
    }
}

/// How far validation has got, and what it found.
///
/// Errors cross as two parallel payloads rather than as objects: four doubles
/// each in [`positions`](Validated::positions), and the messages joined into one
/// string. A malformed 500 MB log can have a thousand errors, and a thousand JS
/// objects built inside a pass the user is watching is the stall the pass was
/// made resumable to avoid (C43, one layer up).
#[wasm_bindgen]
pub struct Validated {
    positions: Vec<f64>,
    messages: String,
    checked: f64,
    total: f64,
    values: f64,
    errors: u32,
    done: bool,
}

impl Validated {
    /// The answer when there is nothing running.
    fn empty() -> Self {
        Self {
            positions: Vec::new(),
            messages: String::new(),
            checked: 0.0,
            total: 0.0,
            values: 0.0,
            errors: 0,
            done: true,
        }
    }
}

#[wasm_bindgen]
impl Validated {
    /// Four doubles per error found by *this* step: byte offset, line, column,
    /// and the row it belongs to — or `-1` for a byte before the first row.
    #[wasm_bindgen(getter)]
    #[must_use]
    pub fn positions(&self) -> Vec<f64> {
        self.positions.clone()
    }

    /// The same errors' messages, separated by U+0001.
    ///
    /// A control character rather than a newline: a message could plausibly
    /// contain a newline one day, and a separator that can appear in the data
    /// is a parsing bug waiting for the right input.
    #[wasm_bindgen(getter)]
    #[must_use]
    pub fn messages(&self) -> String {
        self.messages.clone()
    }

    /// Bytes examined so far.
    #[wasm_bindgen(getter)]
    #[must_use]
    pub fn checked(&self) -> f64 {
        self.checked
    }

    /// Bytes in the file.
    #[wasm_bindgen(getter)]
    #[must_use]
    pub fn total(&self) -> f64 {
        self.total
    }

    /// Top-level values checked — records, for NDJSON.
    #[wasm_bindgen(getter)]
    #[must_use]
    pub fn values(&self) -> f64 {
        self.values
    }

    /// Errors found in total, which may exceed what this step reported.
    #[wasm_bindgen(getter)]
    #[must_use]
    pub fn errors(&self) -> u32 {
        self.errors
    }

    /// Whether the pass has finished.
    #[wasm_bindgen(getter)]
    #[must_use]
    pub fn done(&self) -> bool {
        self.done
    }
}

/// How far a duplicate pass has got, and what it found.
///
/// Four doubles per duplicate and one joined string, for the same reason
/// validation uses that shape (C43): a config file with a thousand repeated keys
/// would otherwise build a thousand JS objects inside a pass the user is
/// watching.
#[wasm_bindgen]
pub struct Deduped {
    positions: Vec<f64>,
    messages: String,
    walked: f64,
    total: f64,
    found: u32,
    keys: f64,
    elements: f64,
    done: bool,
    capped: bool,
}

impl Deduped {
    fn empty() -> Self {
        Self {
            positions: Vec::new(),
            messages: String::new(),
            walked: 0.0,
            total: 0.0,
            found: 0,
            keys: 0.0,
            elements: 0.0,
            done: true,
            capped: false,
        }
    }
}

#[wasm_bindgen]
impl Deduped {
    /// Four doubles per duplicate found by *this* step: the first occurrence's
    /// byte offset and row, then the repeat's byte offset and row. `-1` for a
    /// byte that precedes the first row.
    #[wasm_bindgen(getter)]
    #[must_use]
    pub fn positions(&self) -> Vec<f64> {
        self.positions.clone()
    }

    /// The same duplicates' descriptions, separated by U+0001.
    ///
    /// Each is the kind — `key` or `element` — then U+0002, then the name or a
    /// short rendering of the value.
    #[wasm_bindgen(getter)]
    #[must_use]
    pub fn messages(&self) -> String {
        self.messages.clone()
    }

    /// Bytes walked so far.
    #[wasm_bindgen(getter)]
    #[must_use]
    pub fn walked(&self) -> f64 {
        self.walked
    }

    /// Bytes in the file.
    #[wasm_bindgen(getter)]
    #[must_use]
    pub fn total(&self) -> f64 {
        self.total
    }

    /// Every repeat found, including those past the report limit.
    ///
    /// Counting is free; proving one costs two reads. So a file with two million
    /// duplicate keys reports two million and lists the first thousand, rather
    /// than choosing between a truthful count and a usable one.
    #[wasm_bindgen(getter)]
    #[must_use]
    pub fn found(&self) -> u32 {
        self.found
    }

    /// Object keys examined.
    #[wasm_bindgen(getter)]
    #[must_use]
    pub fn keys(&self) -> f64 {
        self.keys
    }

    /// Array elements examined.
    #[wasm_bindgen(getter)]
    #[must_use]
    pub fn elements(&self) -> f64 {
        self.elements
    }

    /// Whether the pass has finished.
    #[wasm_bindgen(getter)]
    #[must_use]
    pub fn done(&self) -> bool {
        self.done
    }

    /// Whether a container was too large to track fully, so "no duplicates" is
    /// not a claim the pass can make about it.
    #[wasm_bindgen(getter)]
    #[must_use]
    pub fn capped(&self) -> bool {
        self.capped
    }
}

/// One instalment of an export.
///
/// The bytes cross as a `Vec<u8>` the host writes and drops. Nothing accumulates
/// on either side of the boundary — a 500 MB export is 500 MB of writes and a
/// few hundred kilobytes of peak memory, which is the entire point of doing it
/// this way rather than building a string and calling `Blob`.
#[wasm_bindgen]
pub struct Exported {
    chunk: Vec<u8>,
    records: f64,
    done: bool,
    truncated: bool,
}

#[wasm_bindgen]
impl Exported {
    /// The bytes to write for this step.
    #[wasm_bindgen(getter)]
    #[must_use]
    pub fn chunk(&self) -> Vec<u8> {
        self.chunk.clone()
    }

    /// Records converted so far.
    #[wasm_bindgen(getter)]
    #[must_use]
    pub fn records(&self) -> f64 {
        self.records
    }

    /// Whether the export has finished.
    #[wasm_bindgen(getter)]
    #[must_use]
    pub fn done(&self) -> bool {
        self.done
    }

    /// Whether any record was too large to read whole and was cut short.
    ///
    /// A truncated export that claims to be complete is the kind of thing that
    /// costs someone a day, so it is a field rather than a silence.
    #[wasm_bindgen(getter)]
    #[must_use]
    pub fn truncated(&self) -> bool {
        self.truncated
    }
}

/// One open file, and every index built over it.
///
/// Owns the tier-1 index, the tier-2 expansion cache, and the host's reader.
/// Dropping it (`free()` from JS) releases all of them at once, which is the
/// entire close protocol.
#[wasm_bindgen]
pub struct Document {
    source: JsSource,
    build: Build,
    cache: ExpansionCache,
    row_options: RowOptions,
    build_options: BuildOptions,
    expand_options: ExpandOptions,
    find: Option<Find>,
    find_options: FindOptions,
    /// Matches already handed to the host, so each step reports only new ones.
    reported: usize,
    validate: Option<Validate>,
    validate_options: ValidateOptions,
    /// Errors already handed to the host, for the same reason.
    validated: usize,
    schema: Option<Schema>,
    /// The next row a schema pass will check, if one is running.
    schema_row: Option<usize>,
    schema_errors: u32,
    filter: Option<Filter>,
    /// The next row a filter pass will test, if one is running.
    filter_row: Option<usize>,
    filter_matches: u32,
    dedup: Option<Dedup>,
    dedup_options: DedupOptions,
    /// Duplicates already handed to the host, so each step reports only new ones.
    deduped: usize,
    export: Option<Export>,
    /// Which rows the export covers: `None` for all of them.
    export_rows: Option<Vec<usize>>,
    /// The next row to convert, and whether the header has been written.
    export_at: usize,
    export_opened: bool,
    /// The next row to examine for CSV columns, while discovery is running.
    export_discovering: Option<usize>,
    /// The whole document as one record, when its rows are not records.
    export_whole: Option<(u64, u64)>,
}

#[wasm_bindgen]
impl Document {
    /// Open a source of `size` bytes, detecting its format from a prefix.
    ///
    /// Nothing is indexed yet — call [`index_step`](Document::index_step) until
    /// it reports `done`. Opening is separated from indexing so the UI can show
    /// the file's name, size and format immediately, which on a 500 MB file is
    /// several seconds before the tree is complete.
    ///
    /// # Errors
    ///
    /// If the host's reader fails on the prefix. An unrecognizable *format* is
    /// not an error — it is [`Format::Unknown`], which still opens.
    #[wasm_bindgen(constructor)]
    pub fn new(size: f64, reader: ByteReader) -> Result<Document, JsError> {
        let len = if size.is_finite() && size > 0.0 {
            size as u64
        } else {
            0
        };

        let mut source = JsSource {
            reader,
            len,
            scratch: Vec::new(),
        };

        let prefix_len = SNIFF_PREFIX_BYTES.min(len) as u32;
        let format = if prefix_len == 0 {
            Format::Empty
        } else {
            sniff_format(source.read(0, prefix_len).map_err(to_js)?)
        };

        Ok(Document {
            source,
            build: Build::new(format),
            cache: ExpansionCache::default(),
            row_options: RowOptions::default(),
            // The core's default window, deliberately. A 4 MB window was tried
            // on the theory that the browser's cost is per *read* — the same
            // `.wasm` indexes the 500 MB fixture at 470–542 MB/s in Node, where
            // a read is a `readSync` into a reused buffer, against 74–140 MB/s
            // in a Worker, where it is a `blob.slice()` plus a `FileReaderSync`
            // that allocates a fresh `ArrayBuffer`. Cutting 479 reads to 120
            // moved the number not at all (96 MB/s, inside the existing spread),
            // so the cost scales with bytes rather than with calls and the
            // larger window bought only bigger transient allocations. Reverted
            // rather than kept: a constant whose comment claims an effect it
            // does not have is worse than no change.
            build_options: BuildOptions::default(),
            expand_options: ExpandOptions::default(),
            find: None,
            find_options: FindOptions::default(),
            reported: 0,
            validate: None,
            validate_options: ValidateOptions::default(),
            validated: 0,
            schema: None,
            schema_row: None,
            schema_errors: 0,
            filter: None,
            filter_row: None,
            filter_matches: 0,
            dedup: None,
            dedup_options: DedupOptions::default(),
            deduped: 0,
            export: None,
            export_rows: None,
            export_at: 0,
            export_opened: false,
            export_discovering: None,
            export_whole: None,
        })
    }

    /// The detected format: `"single-document"`, `"ndjson"`, `"empty"` or
    /// `"unknown"`.
    #[wasm_bindgen(getter)]
    #[must_use]
    pub fn format(&self) -> String {
        self.build.format().as_str().to_string()
    }

    /// The size of the source in bytes.
    #[wasm_bindgen(getter)]
    #[must_use]
    pub fn size(&self) -> f64 {
        self.source.len as f64
    }

    /// Bytes of the file indexed so far — the numerator of a progress bar.
    ///
    /// Also readable after a cancel, which is the only way the host can report
    /// how much of a file a partial index actually covers.
    #[wasm_bindgen(js_name = indexedBytes, getter)]
    #[must_use]
    pub fn indexed_bytes(&self) -> f64 {
        self.build.consumed() as f64
    }

    /// Bytes of index currently held, tier 1 and tier 2 together.
    ///
    /// The number the memory criterion is judged on, and the one a host would
    /// watch before deciding to shrink the expansion budget.
    #[wasm_bindgen(js_name = heapBytes, getter)]
    #[must_use]
    pub fn heap_bytes(&self) -> f64 {
        (self.build.heap_bytes() + self.cache.heap_bytes()) as f64
    }

    /// Index the next batch of the file.
    ///
    /// Returns after roughly 4 MB of work — tens of milliseconds — so the host
    /// can post progress, honour a cancel, or stop calling. Rows found so far
    /// are already addressable by [`rows`](Document::rows) (C39).
    ///
    /// # Errors
    ///
    /// If the host's reader fails.
    #[wasm_bindgen(js_name = indexStep)]
    pub fn index_step(&mut self) -> Result<Progress, JsError> {
        let reason = self
            .build
            .advance(&mut self.source, &self.build_options)
            .map_err(to_js)?;

        Ok(Progress {
            consumed: self.build.consumed() as f64,
            total: self.source.len as f64,
            rows: clamp_u32(self.build.rows()),
            done: !reason.resumable(),
            malformed: reason == Built::Malformed,
            exhausted: reason == Built::Exhausted,
        })
    }

    /// How many rows a container currently offers.
    ///
    /// Pass `undefined` for the root. An offset that has never been expanded
    /// reports zero rather than failing — the host's next move is to expand it,
    /// and making that an error would only mean the host has to catch it.
    #[wasm_bindgen(js_name = rowCount)]
    pub fn row_count(&mut self, container: Option<f64>) -> u32 {
        match container {
            None => clamp_u32(self.build.rows()),
            Some(offset) => self
                .cache
                .get(offset_of(offset))
                .map_or(0, |expansion| clamp_u32(expansion.len())),
        }
    }

    /// Materialize a run of rows, packed into one buffer.
    ///
    /// Pass `undefined` as `container` for the root, or a container's byte
    /// offset for one that has been expanded. See the [`pack`] module for the
    /// layout; the TypeScript decoder is its counterpart and the two are pinned
    /// together by [`row_layout_version`].
    ///
    /// One byte-range read serves the whole run in the ordinary case, because
    /// siblings are contiguous in the file (C32).
    ///
    /// # Errors
    ///
    /// If the host's reader fails. Content that does not lex becomes an
    /// `invalid` row, not an error (C34).
    pub fn rows(
        &mut self,
        container: Option<f64>,
        start: u32,
        count: u32,
    ) -> Result<Vec<u8>, JsError> {
        // Split the borrows: the table is read while the source is written.
        let Document {
            source,
            build,
            cache,
            row_options,
            ..
        } = self;

        let table = match container {
            None => build.table(),
            Some(offset) => match cache.get(offset_of(offset)) {
                Some(expansion) => expansion.table(),
                None => return Ok(pack::rows(&[])),
            },
        };

        let rows = materialize(table, start as usize, count as usize, source, row_options)
            .map_err(to_js)?;
        Ok(pack::rows(&rows))
    }

    /// Index the next batch of one container's children.
    ///
    /// The first call for an offset starts the expansion; later calls continue
    /// it. Returns after 10 000 children so a five-million-element array does
    /// not stall the Worker (C39).
    ///
    /// # Errors
    ///
    /// If the host's reader fails. A truncated or malformed container is
    /// reported as `done` without `complete`, not as an error (C6).
    #[wasm_bindgen(js_name = expandStep)]
    pub fn expand_step(&mut self, offset: f64) -> Result<Expanded, JsError> {
        let Document {
            source,
            cache,
            expand_options,
            ..
        } = self;

        let expansion = cache.entry(offset_of(offset));
        let reason = expansion.advance(source, expand_options).map_err(to_js)?;
        let expanded = Expanded {
            children: clamp_u32(expansion.len()),
            done: !reason.resumable(),
            complete: reason == Stopped::Closed,
        };

        // Settle up only once the expansion has stopped growing: evicting mid
        // expansion could discard the very entry being built.
        if expanded.done {
            cache.evict_to_budget();
        }
        Ok(expanded)
    }

    /// Begin a search of the file for a literal string.
    ///
    /// Replaces any search in progress — typing another character in the find
    /// box is a new search, and the old one's remaining work is worthless.
    /// Nothing is scanned yet; call [`find_step`](Document::find_step).
    ///
    /// `from` is where to start, for "find next from the selection"; pass
    /// `undefined` to search the whole file.
    #[wasm_bindgen(js_name = findStart)]
    pub fn find_start(&mut self, needle: &str, case_sensitive: bool, from: Option<f64>) {
        let search = Find::new(needle, case_sensitive);
        self.find = Some(match from {
            Some(at) => search.from(offset_of(at)),
            None => search,
        });
        self.reported = 0;
    }

    /// Scan the next few megabytes, and report the rows found in them.
    ///
    /// `rows` holds only what *this* step discovered, so a host appends rather
    /// than replacing a growing array every few milliseconds. `matches` is the
    /// running total.
    ///
    /// A match is only reported once the row containing it has been indexed. On
    /// a file still being indexed the search can outrun tier 1, and a match
    /// beyond the indexed frontier would resolve to the last *known* row — which
    /// is a real row, in the wrong place, and the user would be sent there. So
    /// those matches are held and reported by a later step, once the index has
    /// caught up. Counting them immediately would be honest; showing them would
    /// not.
    ///
    /// # Errors
    ///
    /// If the host's reader fails.
    #[wasm_bindgen(js_name = findStep)]
    pub fn find_step(&mut self) -> Result<Found, JsError> {
        let Document {
            source,
            build,
            find,
            find_options,
            reported,
            ..
        } = self;

        let Some(search) = find.as_mut() else {
            return Ok(Found {
                rows: Vec::new(),
                matches: 0,
                pending: 0,
                scanned: 0.0,
                done: true,
                limited: false,
            });
        };

        search.advance(source, find_options).map_err(to_js)?;

        let indexed = if build.is_resumable() {
            build.consumed()
        } else {
            u64::MAX
        };
        let all = search.matches();
        let mut upto = *reported;
        while upto < all.len() && all[upto] < indexed {
            upto += 1;
        }

        let rows = rows_of(build.table(), &all[*reported..upto])
            .into_iter()
            .map(|row| row as f64)
            .collect();
        *reported = upto;

        Ok(Found {
            rows,
            matches: clamp_u32(all.len()),
            pending: clamp_u32(all.len() - upto),
            // The scan's own state, and nothing else. Tying this to whether every
            // match has been mapped would hang a host whose indexing was
            // cancelled: those matches lie in a region that has no rows and
            // never will, so waiting for them is waiting forever.
            done: search.stopped().is_some(),
            limited: search.stopped() == Some(FindStop::Limited),
            scanned: search.scanned() as f64,
        })
    }

    /// Abandon the search in progress.
    #[wasm_bindgen(js_name = findStop)]
    pub fn find_stop(&mut self) {
        self.find = None;
        self.reported = 0;
    }

    /// Begin checking the document for well-formedness.
    ///
    /// Replaces any pass in progress. Nothing is checked yet — call
    /// [`validate_step`](Document::validate_step) until it reports `done`.
    #[wasm_bindgen(js_name = validateStart)]
    pub fn validate_start(&mut self) {
        self.validate = Some(Validate::new(self.build.format()));
        self.validated = 0;
    }

    /// Check the next batch, and report the errors it found.
    ///
    /// Only errors new to *this* step are returned, so a host appends rather
    /// than replacing a growing list every few milliseconds. Each is resolved to
    /// a row here, where the index is, rather than in a second round trip.
    ///
    /// # Errors
    ///
    /// If the host's reader fails. Malformed JSON is the thing being looked
    /// for, and is never an error of this call.
    #[wasm_bindgen(js_name = validateStep)]
    pub fn validate_step(&mut self) -> Result<Validated, JsError> {
        let Document {
            source,
            build,
            validate,
            validate_options,
            validated,
            ..
        } = self;

        let Some(pass) = validate.as_mut() else {
            return Ok(Validated {
                positions: Vec::new(),
                messages: String::new(),
                checked: 0.0,
                total: 0.0,
                values: 0.0,
                errors: 0,
                done: true,
            });
        };

        pass.advance(source, validate_options).map_err(to_js)?;

        let all = pass.errors();
        let fresh = all.get(*validated..).unwrap_or(&[]);
        let mut positions = Vec::with_capacity(fresh.len() * 4);
        let mut messages = String::new();

        for (at, error) in fresh.iter().enumerate() {
            if at > 0 {
                messages.push('\u{1}');
            }
            messages.push_str(&error.message);
            positions.push(error.offset as f64);
            positions.push(error.line as f64);
            positions.push(error.column as f64);
            positions.push(
                build
                    .table()
                    .locate(error.offset)
                    .map_or(-1.0, |row| row as f64),
            );
        }
        *validated = all.len();

        Ok(Validated {
            positions,
            messages,
            checked: pass.checked() as f64,
            total: source.len as f64,
            values: pass.values() as f64,
            errors: clamp_u32(all.len()),
            done: pass.is_done(),
        })
    }

    /// Compile a JSON Schema, ready to check records against.
    ///
    /// Returns the keywords it does **not** implement, separated by U+0001, so
    /// the host can say how much of the schema was actually applied. An empty
    /// string means all of it.
    ///
    /// # Errors
    ///
    /// If the schema is not valid JSON, is not a schema, or uses a remote
    /// `$ref` — which would be a network fetch, and the manifest requests no
    /// host permissions.
    #[wasm_bindgen(js_name = schemaSet)]
    pub fn schema_set(&mut self, source: &str) -> Result<String, JsError> {
        let schema =
            Schema::compile(source.as_bytes()).map_err(|error| JsError::new(&error.message))?;
        let unsupported = schema.unsupported().join("\u{1}");
        self.schema = Some(schema);
        Ok(unsupported)
    }

    /// Begin checking every record against the compiled schema.
    ///
    /// # Errors
    ///
    /// If no schema has been set.
    #[wasm_bindgen(js_name = schemaStart)]
    pub fn schema_start(&mut self) -> Result<(), JsError> {
        if self.schema.is_none() {
            return Err(JsError::new("no schema has been set"));
        }
        self.schema_row = Some(0);
        self.schema_errors = 0;
        Ok(())
    }

    /// Check the next batch of records against the schema.
    ///
    /// Records are checked **one at a time, from their own byte range** — the
    /// index says where each one starts, so each is read, checked and dropped.
    /// Nothing accumulates, which is what lets a 500 MB file be schema-checked
    /// at all: the peak is one record, not one document.
    ///
    /// # Errors
    ///
    /// If the host's reader fails.
    #[wasm_bindgen(js_name = schemaStep)]
    pub fn schema_step(&mut self) -> Result<Validated, JsError> {
        let Document {
            source,
            build,
            schema,
            schema_row,
            schema_errors,
            row_options,
            ..
        } = self;

        let (Some(schema), Some(row)) = (schema.as_ref(), *schema_row) else {
            return Ok(Validated::empty());
        };

        let table = build.table();
        let rows = table.len();
        let last = (row + SCHEMA_BATCH).min(rows);

        let mut positions = Vec::new();
        let mut messages = String::new();
        let mut first = true;

        for index in row..last {
            let Some(start) = table.child(index) else {
                continue;
            };
            // A record ends where the next begins; the last one runs to the end
            // of the file. Capped, so one pathological record cannot be read
            // whole into memory.
            let end = table
                .child(index + 1)
                .unwrap_or(source.len)
                .min(start.saturating_add(u64::from(row_options.row_budget) * 64));
            let length = clamp_u32(usize::try_from(end.saturating_sub(start)).unwrap_or(0));

            let bytes = source.read(start, length).map_err(to_js)?.to_vec();
            for problem in schema.check(&bytes) {
                *schema_errors += 1;
                if *schema_errors as usize > SCHEMA_ERROR_LIMIT {
                    break;
                }
                if !first {
                    messages.push('\u{1}');
                }
                first = false;
                messages.push_str(&problem.message);
                positions.push((start + problem.offset) as f64);
                positions.push(problem.line as f64);
                positions.push(problem.column as f64);
                positions.push(index as f64);
            }
        }

        let done = last >= rows || (*schema_errors as usize) > SCHEMA_ERROR_LIMIT;
        *schema_row = if done { None } else { Some(last) };

        Ok(Validated {
            positions,
            messages,
            checked: table.child(last.min(rows.saturating_sub(1))).unwrap_or(0) as f64,
            total: source.len as f64,
            values: last as f64,
            errors: *schema_errors,
            done,
        })
    }

    /// Compile a filter expression, replacing any already set.
    ///
    /// Separated from running it so a syntax error is reported the moment it is
    /// typed, against an empty results list, rather than after a pass that was
    /// never going to start.
    ///
    /// # Errors
    ///
    /// If the expression does not parse, or uses a JSONPath construct outside
    /// the supported subset. The message names which, and where.
    #[wasm_bindgen(js_name = filterSet)]
    pub fn filter_set(&mut self, source: &str) -> Result<(), JsError> {
        self.filter = Some(Filter::parse(source).map_err(|e| JsError::new(&e.to_string()))?);
        Ok(())
    }

    /// Begin testing every record against the compiled filter.
    ///
    /// # Errors
    ///
    /// If no filter has been set.
    #[wasm_bindgen(js_name = filterStart)]
    pub fn filter_start(&mut self) -> Result<(), JsError> {
        if self.filter.is_none() {
            return Err(JsError::new("no filter has been set"));
        }
        self.filter_row = Some(0);
        self.filter_matches = 0;
        Ok(())
    }

    /// Test the next batch of records, and report the ones that matched.
    ///
    /// Reuses [`Found`], so a filter's results reach the UI down the same path a
    /// find's do — the results list does not need to know which produced them.
    ///
    /// ## Records are read in windows, not one at a time
    ///
    /// The obvious implementation reads each record's own byte range, which is
    /// what schema checking does and what this did first. It costs 23 MB/s on
    /// the 500 MB fixture, against 467 MB/s for indexing the same file — and the
    /// difference is not parsing. It is 1.77 million reads of ~280 bytes, each
    /// one a `readSync` in Node and a `blob.slice()` plus a `FileReaderSync` in
    /// a Worker, where the second allocates a fresh `ArrayBuffer` every time.
    ///
    /// So a window covering many records is read once and each record is tested
    /// against a subslice of it. The peak is one window rather than one record,
    /// which is a bounded amount larger and still nothing next to the file.
    ///
    /// This is the same lesson as C54 read the other way round: there, widening
    /// the *indexing* window changed nothing, because indexing already read in
    /// megabytes and the cost scaled with bytes. Here the cost scaled with
    /// calls, because there were six orders of magnitude more of them.
    ///
    /// # Errors
    ///
    /// If the host's reader fails. A record that does not parse is not an error
    /// of this call — it simply does not match.
    #[wasm_bindgen(js_name = filterStep)]
    pub fn filter_step(&mut self) -> Result<Found, JsError> {
        let Document {
            source,
            build,
            filter,
            filter_row,
            filter_matches,
            row_options,
            ..
        } = self;

        let (Some(filter), Some(row)) = (filter.as_ref(), *filter_row) else {
            return Ok(Found {
                rows: Vec::new(),
                matches: *filter_matches,
                pending: 0,
                scanned: source.len as f64,
                done: true,
                limited: false,
            });
        };

        let table = build.table();
        let total = table.len();
        let cap = u64::from(row_options.row_budget) * 64;

        // One matcher for the whole batch, not one per record: it owns the
        // lexer, the path stack and a slot per referenced path, and rebuilding
        // those two thousand times a step is the allocation this avoids.
        let mut matcher = filter.matcher();
        let mut rows = Vec::new();
        let mut at = row;
        let mut tested = 0usize;

        while at < total && tested < FILTER_BATCH {
            let Some(base) = table.child(at) else {
                at += 1;
                continue;
            };

            // How many records fit in one window. Always at least one, so a
            // record larger than the window is still tested — clipped to `cap`,
            // exactly as a single-record read would have clipped it.
            let mut upto = at + 1;
            while upto < total
                && tested + (upto - at) < FILTER_BATCH
                && table
                    .child(upto)
                    .is_some_and(|start| start - base < FILTER_WINDOW)
            {
                upto += 1;
            }

            // Read before borrowing: `source.len` is behind the same borrow the
            // window holds, so every offset this loop needs is settled first.
            let end_of_file = source.len;
            let stop = table.child(upto).unwrap_or(end_of_file);
            let span = (stop - base).min(FILTER_WINDOW.max(cap));
            let window = source.read(base, clamp_u32(span as usize)).map_err(to_js)?;

            for index in at..upto {
                let Some(start) = table.child(index) else {
                    continue;
                };
                let from = (start - base) as usize;
                let to = table
                    .child(index + 1)
                    .unwrap_or(end_of_file)
                    .min(start.saturating_add(cap))
                    .saturating_sub(base) as usize;
                let Some(record) = window.get(from..to.min(window.len())) else {
                    continue;
                };

                if matcher.matches(record) {
                    *filter_matches += 1;
                    if (*filter_matches as usize) <= FILTER_MATCH_LIMIT {
                        rows.push(index as f64);
                    }
                }
            }

            tested += upto - at;
            at = upto;
        }

        // Unlike find, the limit caps what is *listed*, not what is counted:
        // testing a record costs the same either way, so "2,000 of 41,988" is
        // free to be true where find would have had to stop scanning.
        let done = at >= total;
        *filter_row = if done { None } else { Some(at) };

        Ok(Found {
            rows,
            matches: *filter_matches,
            pending: 0,
            scanned: table.child(at).unwrap_or(source.len) as f64,
            done,
            limited: (*filter_matches as usize) > FILTER_MATCH_LIMIT,
        })
    }

    /// Abandon the filter pass in progress.
    #[wasm_bindgen(js_name = filterStop)]
    pub fn filter_stop(&mut self) {
        self.filter = None;
        self.filter_row = None;
        self.filter_matches = 0;
    }

    /// Begin looking for duplicates.
    ///
    /// `elements` is the expensive half and is opt-in (SPEC M5): checking keys
    /// costs a hash of each name, while checking elements costs a hash of every
    /// subtree and a frame that grows with the container.
    #[wasm_bindgen(js_name = dedupStart)]
    pub fn dedup_start(&mut self, keys: bool, elements: bool) {
        self.dedup = Some(Dedup::new(self.build.format()));
        self.dedup_options = DedupOptions {
            keys,
            elements,
            ..DedupOptions::default()
        };
        self.deduped = 0;
    }

    /// Walk the next batch, and report the duplicates it found.
    ///
    /// Only duplicates new to *this* step are returned, so a host appends rather
    /// than replacing a growing list. Each offset is resolved to a row here,
    /// where the index is, rather than in a second round trip.
    ///
    /// # Errors
    ///
    /// If the host's reader fails.
    #[wasm_bindgen(js_name = dedupStep)]
    pub fn dedup_step(&mut self) -> Result<Deduped, JsError> {
        let Document {
            source,
            build,
            dedup,
            dedup_options,
            deduped,
            ..
        } = self;

        let Some(pass) = dedup.as_mut() else {
            return Ok(Deduped::empty());
        };

        pass.advance(source, dedup_options).map_err(to_js)?;

        let all = pass.duplicates();
        let table = build.table();
        let mut positions = Vec::new();
        let mut messages = String::new();
        for duplicate in &all[(*deduped).min(all.len())..] {
            if !messages.is_empty() {
                messages.push('\u{1}');
            }
            messages.push_str(duplicate.kind.as_str());
            messages.push('\u{2}');
            messages.push_str(&duplicate.what);
            for offset in [duplicate.first, duplicate.second] {
                positions.push(offset as f64);
                positions.push(table.locate(offset).map_or(-1.0, |row| row as f64));
            }
        }
        *deduped = all.len();

        Ok(Deduped {
            positions,
            messages,
            walked: pass.walked() as f64,
            total: source.len as f64,
            found: clamp_u32(usize::try_from(pass.total()).unwrap_or(usize::MAX)),
            keys: pass.keys_checked() as f64,
            elements: pass.elements_checked() as f64,
            done: pass.is_done(),
            capped: pass.capped(),
        })
    }

    /// Abandon the duplicate pass in progress.
    #[wasm_bindgen(js_name = dedupStop)]
    pub fn dedup_stop(&mut self) {
        self.dedup = None;
        self.deduped = 0;
    }

    /// Begin an export.
    ///
    /// `format` is one of `json`, `json-pretty`, `ndjson`, `csv`. `rows` selects
    /// which root rows to write — an empty array means all of them, and the
    /// filter's result is what a host passes here.
    ///
    /// CSV runs a discovery pass first, because a column that first appears in
    /// record 900 000 still belongs in the header. `exportStep` drives both
    /// phases; a host cannot forget the first one, because there is no way to
    /// ask for the second on its own.
    ///
    /// # Errors
    ///
    /// If `format` is not one of the four.
    #[wasm_bindgen(js_name = exportStart)]
    pub fn export_start(&mut self, format: &str, rows: Vec<f64>) -> Result<(), JsError> {
        let format = match format {
            "json" => ExportFormat::Json,
            "json-pretty" => ExportFormat::JsonPretty,
            "ndjson" => ExportFormat::Ndjson,
            "csv" => ExportFormat::Csv,
            other => return Err(JsError::new(&format!("unknown export format: {other}"))),
        };

        self.export_rows = if rows.is_empty() {
            None
        } else {
            Some(rows.iter().map(|row| *row as usize).collect())
        };

        // A single document whose root is an **object** has *members* as its
        // tier-1 rows, not records — exporting those as a sequence would write
        // `"a":1` per line and drop the braces, which is what the first version
        // did. Its rows are only records when the root is an array.
        //
        // The root's kind comes from its first byte, which is one read of a few
        // bytes rather than a second index.
        self.export_whole =
            if self.export_rows.is_none() && self.build.format() == Format::SingleDocument {
                let head = self.source.read(0, 64).map_err(to_js)?;
                let first = head.iter().find(|b| !b.is_ascii_whitespace()).copied();
                (first != Some(b'[')).then_some((0, self.source.len))
            } else {
                None
            };

        self.export_at = 0;
        self.export_opened = false;
        self.export_discovering = format.needs_columns().then_some(0);
        // A whole document is one value, not a sequence, so JSON output must not
        // wrap it: a file holding `{"a":1}` exports as `{"a":1}`.
        self.export = Some(Export::new(format).wrapped(self.export_whole.is_none()));
        Ok(())
    }

    /// Convert the next batch, and return the bytes to write.
    ///
    /// # Errors
    ///
    /// If the host's reader fails.
    #[wasm_bindgen(js_name = exportStep)]
    pub fn export_step(&mut self) -> Result<Exported, JsError> {
        let Document {
            source,
            build,
            export,
            export_rows,
            export_at,
            export_opened,
            export_discovering,
            export_whole,
            row_options,
            ..
        } = self;

        let Some(writer) = export.as_mut() else {
            return Ok(Exported {
                chunk: Vec::new(),
                records: 0.0,
                done: true,
                truncated: false,
            });
        };

        let table = build.table();
        let whole = *export_whole;
        let count = match (whole, export_rows.as_ref()) {
            (Some(_), _) => 1,
            (None, Some(chosen)) => chosen.len(),
            (None, None) => table.len(),
        };
        let row_of = |at: usize| -> Option<usize> {
            match export_rows.as_ref() {
                Some(chosen) => chosen.get(at).copied(),
                None => (at < table.len()).then_some(at),
            }
        };
        // Settled before the source is borrowed for reading, for the same
        // reason `filterStep` settles it: `source.len` lives behind the same
        // borrow the reader takes.
        let end_of_file = source.len;
        let cap = u64::from(row_options.row_budget) * 64;
        let extent = |row: usize| -> Option<(u64, u64)> {
            if let Some(span) = whole {
                return Some(span);
            }
            let start = table.child(row)?;
            let end = table
                .child(row + 1)
                .unwrap_or(end_of_file)
                .min(start.saturating_add(cap));
            Some((start, end))
        };

        // Phase one: CSV column discovery, which reads every record and writes
        // nothing. Reported as progress rather than as a stall.
        if let Some(at) = *export_discovering {
            let last = (at + EXPORT_BATCH).min(count);
            for index in at..last {
                let span = if whole.is_some() {
                    whole
                } else {
                    row_of(index).and_then(extent)
                };
                if let Some((start, end)) = span {
                    writer.discover(source, start, end).map_err(to_js)?;
                }
            }
            *export_discovering = if last >= count { None } else { Some(last) };
            return Ok(Exported {
                chunk: Vec::new(),
                records: last as f64,
                done: false,
                truncated: writer.truncated(),
            });
        }

        let mut chunk = Vec::new();
        if !*export_opened {
            chunk.extend_from_slice(writer.open());
            *export_opened = true;
        }

        let last = (*export_at + EXPORT_BATCH).min(count);
        for index in *export_at..last {
            let span = if whole.is_some() {
                whole
            } else {
                row_of(index).and_then(extent)
            };
            if let Some((start, end)) = span {
                chunk.extend_from_slice(writer.push(source, start, end).map_err(to_js)?);
            }
        }
        *export_at = last;

        let done = last >= count;
        if done {
            chunk.extend_from_slice(writer.close());
        }

        Ok(Exported {
            chunk,
            records: writer.written() as f64,
            done,
            truncated: writer.truncated(),
        })
    }

    /// Abandon the export in progress.
    #[wasm_bindgen(js_name = exportStop)]
    pub fn export_stop(&mut self) {
        self.export = None;
        self.export_rows = None;
    }

    /// Which root row contains byte `offset`, if any.
    ///
    /// The join between where the engine thinks and where the user thinks: a
    /// jump-to-offset, a validation error, and a search hit all arrive as a byte
    /// and have to become a row before anyone can be sent there. A binary search
    /// over tier 1 — 21 comparisons on the 500 MB fixture.
    ///
    /// `undefined` for a byte before the first row, which is a real answer: a
    /// document's opening `[` genuinely precedes every row.
    #[wasm_bindgen(js_name = rowAtByte)]
    pub fn row_at_byte(&self, offset: f64) -> Option<u32> {
        self.build.table().locate(offset_of(offset)).map(clamp_u32)
    }

    /// Forget one container's expansion — what a collapse does.
    ///
    /// Safe at any time: a node is addressed by its byte offset, so nothing the
    /// host is holding becomes invalid, and re-expanding produces the same
    /// table (C36).
    pub fn forget(&mut self, offset: f64) {
        self.cache.forget(offset_of(offset));
    }

    /// Forget every expansion, keeping the tier-1 index.
    #[wasm_bindgen(js_name = forgetAll)]
    pub fn forget_all(&mut self) {
        self.cache.clear();
    }
}

/// Byte offsets arrive as doubles; anything that is not one addresses byte zero.
///
/// A negative or non-finite offset is a host bug, and the useful response to a
/// host bug is a wrong-looking row rather than a thrown exception the renderer
/// has to survive mid-frame.
fn offset_of(value: f64) -> u64 {
    if value.is_finite() && value > 0.0 {
        value as u64
    } else {
        0
    }
}

fn clamp_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

fn to_js(error: SourceError) -> JsError {
    JsError::new(&error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    // `Document` needs a JS host, so it is exercised from the extension's smoke
    // test rather than here. What is testable natively is the arithmetic that
    // would otherwise only fail in a browser.

    #[test]
    fn a_nonsense_offset_addresses_the_start_rather_than_panicking() {
        assert_eq!(offset_of(f64::NAN), 0);
        assert_eq!(offset_of(f64::INFINITY), 0);
        assert_eq!(offset_of(-1.0), 0);
        assert_eq!(offset_of(0.0), 0);
        assert_eq!(offset_of(4096.0), 4096);
    }

    #[test]
    fn offsets_survive_the_largest_file_anyone_will_ever_open() {
        // The f64 claim in the module docs, asserted rather than believed.
        let petabyte = 1024.0_f64 * 1024.0 * 1024.0 * 1024.0 * 1024.0;
        assert_eq!(offset_of(petabyte), 1 << 50);
        assert_eq!(offset_of(9_007_199_254_740_991.0), 9_007_199_254_740_991);
    }

    #[test]
    fn a_count_too_large_for_the_boundary_saturates() {
        assert_eq!(clamp_u32(7), 7);
        assert_eq!(clamp_u32(usize::MAX), u32::MAX);
    }
}
