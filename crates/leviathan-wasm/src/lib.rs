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
    Build, BuildOptions, Built, ByteRange, ExpandOptions, ExpansionCache, Format, RowOptions,
    SourceError, Stopped, materialize, sniff_format,
};
use wasm_bindgen::prelude::*;

/// How much of a file is enough to tell single-document from NDJSON.
///
/// Mirrors `SNIFF_PREFIX_BYTES` in the TypeScript protocol.
const SNIFF_PREFIX_BYTES: u64 = 64 * 1024;

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
            build_options: BuildOptions::default(),
            expand_options: ExpandOptions::default(),
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
