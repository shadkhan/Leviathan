# ADR-002 — WASM boundary marshalling

**Status:** Accepted · closed at M1
**Date:** 2026-08-01
**Supersedes:** none

## Context

The index lives in WASM linear memory. The renderer lives in JavaScript. Between
them, sixty times a second, passes the answer to "what are rows 900,000 through
900,050?"

The naive shape of that answer is fifty objects, each with a key string, a
preview string, a kind, an offset and a child count. That is fifty allocations
and a hundred strings per frame, produced by `wasm-bindgen`'s automatic
conversion, and it is how WASM projects end up slower than the plain JavaScript
they replaced. The boundary is not a detail here — it is on the hot path of the
one interaction the product is judged on.

Two hazards shape everything below:

1. **Any WASM allocation can grow linear memory and detach every existing JS
   view.** A cached `Uint8Array` silently becomes zero-length. This is a
   use-after-free with no error message.
2. **Round-trips are expensive and easy to hide.** An API that looks like
   `getRow(i)` invites one call per row.

## Decision

**Rows cross as a single packed binary buffer, transferred, decoded lazily.**

`pack.rs` writes a screen of rows as:

```
header (16 B) │ fixed 40-byte records × N │ one UTF-8 string blob
```

The header carries a **layout version**, the row count, and the string-blob
length. Each 40-byte record holds the row's offset, its value's start and end,
a child count, a kind discriminant, flags, and the *lengths* of its two strings.
The whole thing crosses as one transferred `ArrayBuffer`; the TypeScript
`RowBlock` decodes a row's strings only when that row is actually painted.

Fifty rows are **one allocation and one transfer**, and a block scrolled past
without being painted costs nothing beyond the transfer.

Three details were each nearly decided the other way:

- **Strings are located by length, not by offset.** The decoder walks rows in
  order and accumulates, so two `u16`/`u32` lengths (6 bytes) replace two
  offsets (8 bytes), and the constraint they impose — decode in order — is one
  the consumer obeys anyway. A prefix sum is built once on first random access.
- **Offsets are `f64`, not `BigInt`.** A double is exact to 2^53, which is nine
  petabytes; no JSON file will reach it. `BigInt` would be *more* correct and
  would put a conversion in the renderer's hot path to buy a range that does not
  exist. So `u64` fields are read as two `u32`s rather than with
  `getBigUint64`.
- **The layout is versioned and asserted twice** — at Worker startup, and again
  by the decoder. A skew between a rebuilt bundle and a stale `.wasm` is not a
  type error; it is plausible-looking wrong rows, which is the worst failure
  available. A version field in the header turns it into a sentence.

Above the packing, two batching rules:

- **One RPC per animation frame**, never one per row. The UI's request/response
  client is hand-rolled with numeric ids and transferable payloads.
- **Long operations are events, not calls.** Indexing and searching push
  progress from the Worker rather than being pumped by the UI, because driving a
  500 MB index from the main thread would mean 125 round-trips to communicate
  something the UI only reacts to.

## Alternatives considered

### `wasm-bindgen` automatic struct conversion

What the tooling makes easiest: return a `Vec<Row>` and let the bindings build
JS objects. Rejected on allocation count — fifty objects and a hundred strings
per frame, all garbage within two frames. It is also the option that makes the
cost invisible, which is worse than making it high.

### Comlink

A clean RPC proxy over `postMessage`. Rejected twice over: bundle budget, and
more importantly its proxy model *hides when a round-trip happens*. The entire
performance story here depends on round-trips being counted deliberately. A
library whose selling point is that you stop thinking about the boundary is the
wrong library for a project whose thesis is the boundary.

### `SharedArrayBuffer` and zero-copy views into WASM memory

The theoretically fastest option: no copy at all, JS reads the index in place.
Rejected because of hazard 1. A view into linear memory is valid until the next
allocation, and "the next allocation" includes anything the engine does while
the renderer is still holding the view. Making that safe requires re-acquiring
after every call that *might* allocate — a rule that survives exactly until the
refactor that forgets it. `SharedArrayBuffer` additionally needs
cross-origin-isolation headers, which an extension page does not control.

The chosen design closes the hazard **by construction**: `wasm-bindgen` copies a
returned `Vec<u8>` out of linear memory before JS sees it, so the buffer the
Worker transfers is never a view onto WASM memory and cannot be detached by a
heap growth.

### JSON as the wire format

Serializing rows to JSON to cross into a JSON viewer has a certain symmetry and
no other merit. Parsing on the main thread is the one thing this product
forbids.

## Consequences

- **Measured:** fetching 50 rows from row #1,595,372 of the 500 MB fixture takes
  **68 µs warm / 132 µs cold**, against a 20 ms exit criterion — and that is one
  byte-range read, not fifty, because siblings are contiguous in the file. In a
  Worker that distinction is decisive: `Blob.slice().arrayBuffer()` costs about a
  millisecond regardless of size, so fifty reads would be 50 ms and one is 1 ms.
- The `.wasm` is 45 kB raw / 21 kB gzipped; all JS and CSS is 14.8 kB gzipped
  against a 150 KB budget (10 %).
- A new row field means touching three places — the packer, the decoder, and the
  layout version. That friction is deliberate; it is what keeps the record fixed
  width.
- **Still owed:** the test that grows WASM memory *during* a read. The hazard is
  closed by construction rather than by test, and "by construction" is an
  argument until something asserts it.
- `wasm-bindgen` structs returned to JS (`Progress`, `Expanded`, `Found`) are
  pointers with JS wrappers and must be `free()`d explicitly. Left to the
  finalizer, an expand-per-frame UI leaks the WASM heap at the rate the user
  scrolls. Every call site frees in a `finally`.
