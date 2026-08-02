# ADR-005 — File access model

**Status:** Accepted · closed at M1
**Date:** 2026-08-01
**Supersedes:** none

## Context

The core stores offsets and re-reads the file to paint anything
([ADR-004](ADR-004-index-representation.md)). That makes reading a byte range a
hot operation rather than a startup step, and it puts the file-access model on
the critical path of every frame.

Three environments have to be served by the same code:

| | Reads by | Cost per read |
|---|---|---|
| Web Worker (the product) | `Blob.slice()` | ~1 ms regardless of size |
| Native CLI (benchmarks, fuzzing) | `pread` | microseconds |
| Tests | a `&[u8]` | free |

And MV3 imposes constraints that are not negotiable: no remote code, so the
`.wasm` is a bundled asset; a viewer page rather than interception of
`application/json` navigations; and — the point of the whole exercise — **zero
host permissions**, so that "your data never leaves your machine" is verifiable
from the manifest rather than promised in a README.

## Decision

**A synchronous pull trait, implemented by the host.**

```rust
pub trait ByteRange {
    fn read(&mut self, start: u64, len: u32) -> Result<&[u8], SourceError>;
    fn len_hint(&self) -> Option<u64> { None }
}
```

The core never opens a file, never awaits, and does not know what a `Blob` is.
It asks for ranges; the host answers. The returned slice borrows from `self`, so
one range is live at a time — deliberate, because it lets an implementation
reuse a single scratch buffer instead of allocating per call.

In the Worker, this is backed by **`FileReaderSync`**, which exists only in a
Worker and blocks the thread that calls it. That is not a workaround; it is the
design landing where it was aimed. The rule this project enforces is that the
**main thread** never blocks. The Worker exists precisely to be the thread that
may, and `FileReaderSync` blocks a thread whose blocking is free.

Two behaviours follow from the trait's contract and are worth naming:

- **Short reads at the end of the file are normal, not errors.** Every reader in
  the engine is speculative at its edges: a row window asks for a budget past the
  last row, an expansion asks past a container's close, a search asks past the
  final match — none of them knows whether the file extends that far. A shared
  `read_clamped` shortens the request, with a probe path for sources that will
  not state their length (what a stream being consumed for the first time looks
  like).
- **One read per screen, not per row.** Siblings are contiguous in the file, so a
  50-row slice spans a few hundred bytes and costs a single read. Natively that
  saves syscalls; in the Worker it is the difference between the design working
  and not — 50 × 1 ms versus 1 × 1 ms.

## Alternatives considered

### Async `ByteRange`

The instinct in a browser, since `Blob.slice().arrayBuffer()` is a promise. It
does not survive contact with the requirement: a promise cannot be awaited from
inside a WASM call, so the core would have to be **inverted** — returning "I
need bytes at X" and being resumed with them. That puts an async hop inside the
lexer's inner loop and a state machine in every caller, and it would make the
CLI worse to serve the browser. Rejected.

### Pre-buffering windows the host predicts

Works where reads are predictable. Indexing is predictable; **expansion is not**
— it decides where to read next and only it knows where. Rejected because it
serves every case except the one that needs it.

### Reading the file into WASM memory once

Simple, fast, and the exact failure being escaped. A 500 MB file would need
500 MB of linear memory before anything could be painted.

### The File System Access API

`showOpenFilePicker` gives a handle that survives a reload and would enable a
"reopen recent" feature. It also asks for a permission prompt that the drop /
picker / paste flow does not need, and the privacy claim is stronger when the
manifest requests nothing at all. Deferred; it is a v1.1 convenience, not a
capability the engine lacks. (`showSaveFilePicker` will be needed for streaming
exports at M6 — a separate decision, on the write path.)

### Intercepting `application/json` navigations

The feature everyone expects from a JSON viewer extension, and MV3 has no clean
way to do it without re-fetching the body — which means either a host permission
or downloading the file twice. Explicitly out of the definition of done. v1
ships the viewer-page flow as primary; interception is revisited only if it can
be done without a host permission.

## Consequences

- **The reusability claim has three independent implementors and the trait has
  not moved once:** `&[u8]` (tests), `FileSource` (CLI, `pread` with a reused
  buffer), `JsSource` (Worker, over a JS callback backed by `FileReaderSync`).
  A trait with one implementor is a trait shaped like its implementor; this one
  has been tested against a slice, a file, and a browser `Blob`.
- **Peak RSS is flat.** 4.3 MB while streaming 500 MB in the CLI — the baseline
  process, unchanged. 22 MB with a tier-1 index resident.
- The engine only runs in a Worker. That was always true by policy; with
  `FileReaderSync` it is true by construction, since the API does not exist on
  the main thread.
- The Worker holds the `File` for the session. A `File` is a handle, not bytes —
  structure-cloning it across `postMessage` moves the handle, and the file is
  never held whole anywhere.
- **Privacy is a property of the manifest**, not a policy: zero permissions,
  zero host permissions, no network code of any kind, `.wasm` bundled. This is
  checkable by anyone who unzips the extension, which is the only form of the
  claim worth making.
