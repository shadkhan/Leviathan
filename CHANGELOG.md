# Changelog

All notable changes to this project are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the versions
follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Three crates and one extension share one version number, because they ship as
one thing: the extension bundles the `.wasm`, the `.wasm` wraps the core, and a
version skew between them is the single most likely cause of a confusing bug
here. The protocol asserts it at startup rather than trusting it.

## [Unreleased]

Nothing yet.

## [0.1.0] — unreleased

The first release. Everything below is new, so this entry reads as a feature
list rather than as a diff.

### Engine — `leviathan-core`

- **Streaming lexer**, resumable across arbitrary chunk boundaries, with
  byte/line/column positions on every token and every error. 248–327 MB/s.
- **Two-tier index.** Tier 1 covers the whole file eagerly; tier 2 indexes one
  container on demand and evicts by subtree. 8 bytes per node — 14.2 MB for a
  500 MB file, 2.8 % of it.
- **Row materialization**: a run of rows re-lexed out of the file at paint time,
  65–119 µs for fifty rows from anywhere in the document.
- **Find**: literal search over the file's bytes, streamed and cancellable.
- **Validate**: well-formedness with exact locations, and per-record recovery so
  a 500 MB log with nine broken lines reports all nine.
- **JSON Schema** (draft 2020-12 subset), hand-rolled, reporting which keywords
  it does not implement rather than silently skipping them.
- **Filter**: a JSONPath subset — `@.a.b`, `@[0]`, comparisons, `&&`, `||`, `!`
  — evaluated per record without materializing anything.
- **Dedup**: duplicate object keys and duplicate array elements, each reported
  with both of its locations.
- **Export**: JSON, indented JSON, NDJSON and CSV, written a record at a time.
  Round-trip faithful by construction — the output is the input's own tokens.
- Zero dependencies, `#![forbid(unsafe_code)]`, and no I/O of its own.

### Bindings — `leviathan-wasm` / `@shadkhan/leviathan-core`

- Every engine feature across the WASM boundary, driven in resumable steps so a
  host never blocks.
- Rows cross as one packed binary block per screen rather than as JS objects.
- Byte offsets cross as `f64`, exact to 2^53 — nine petabytes.

### Extension

- Virtualized tree over files far larger than memory; first rows painted in
  141 ms on a 500 MB file, and ~23 ms into the first batch at any size.
- **Out-of-memory is a stopping condition, not a crash.** A file whose *shape*
  needs more index than a 32-bit engine can hold stops part-way, says "index too
  large", and keeps every record it found — browsable, searchable, exportable.
  The viewer also projects the index size from measured bytes-per-node once 2 %
  of the file is read, and warns before the wait rather than after it.
- Drag-and-drop, file picker, folder and paste. JSON and NDJSON auto-detected.
- Find bar that doubles as a filter bar: text searches the file, an expression
  beginning with `@` filters records.
- Validate, JSON Schema, and duplicate detection, all reporting into one panel
  with click-to-jump.
- Export to disk, streamed — a 500 MB export is never assembled in memory.
- Go-to path / byte / line, breadcrumb, collapse-all, full keyboard navigation.
- A live memory readout and a permanent offline badge. The manifest requests
  **zero** host permissions, which is checkable in about ten seconds.

### Proven, not asserted

- **JSONTestSuite** (RFC 8259): 95/95 must-accept, 185/188 must-reject, with the
  three deviations documented and checked in both directions.
- **JSONPath CTS** (RFC 9535): 133/133 in-scope cases pass, 93/93 invalid
  selectors refused, and all 477 out-of-scope cases produce an error naming the
  construct rather than being reinterpreted.
- **Fuzzing**: 1.97 billion cases, no panics, no chunk-size disagreements.
- **Round trip**: seven fixtures, token-exact and idempotent.

### How large is large

Measured against the shipped `.wasm`, not extrapolated:

| File | Records | Index | Peak WASM | Indexed in |
|---:|---:|---:|---:|---:|
| 500 MB | 1.8 M | 14.2 MB | 22 MB | 1.1 s |
| 2 GB | 7.1 M | 56.6 MB | 136 MB | 4.4 s |
| 8 GB | 28.2 M | 226 MB | 539 MB | 17.8 s |

First rows paint ~23 ms into the first batch at every size.

### Known limitations

- One published benchmark miss: 0.7 % of frames exceed 32 ms while scrolling
  100 000 rows (median 16.6 ms, p95 16.9 ms, zero long tasks).
- **Cost follows shape, not size.** The index is 8 bytes per node whatever the
  node is: 2.8 % of a record-shaped file, ~81 % of a flat array of small
  scalars. A 1 GB array of bare numbers needs 2.15 GB of WASM memory, and a
  2.5 GB one does not fit in a 32-bit address space at all — indexing stops
  part-way, says so, and keeps every record it found. Two mitigations are
  identified in ADR-004; neither is built.
- The size claim is **8 GB**, because that is the largest fixture measured.
  Larger is plausible and unproven, and this project does not extrapolate.
- CSV export flattens objects by dotted path and renders arrays as one cell.
  That is lossy, and the rule is documented rather than clever.
- Filter is a subset of RFC 9535, not RFC 9535. The support table says how much.

[Unreleased]: https://github.com/shadkhan/leviathan/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/shadkhan/leviathan/releases/tag/v0.1.0
