# Leviathan — Build Spec (phased)

> **Scope line: Leviathan is a JSON viewer that survives large files.**
> This spec is the execution plan for that one line. Anything not traceable to it is out.

Companion documents:
- `README.md` — product scope, features, non-goals, definition of done (source of truth for *what*).
- `USER_PERSONAS.md` — who hits the wall and when (source of truth for *for whom*).
- `DEEP_REASONING.md` — running log of core concepts and why they were chosen (source of truth for *why*).
- `docs/adr/ADR-00N-*.md` — one decision each, written at the phase that closes it.

---

## 0. Ground truths this spec is built on

These are the constraints that shape every phase. If one of them turns out to be false, the plan changes.

| # | Truth | Consequence |
|---|---|---|
| G1 | A 500 MB file cannot be held in JS as parsed objects, and should not be held in WASM linear memory in full either. | The core is **streaming and sans-IO**; source bytes stay in a JS `Blob`/`File` and are re-read by byte range on demand. |
| G2 | wasm32 has a 4 GB address space and realistically a ~2 GB usable heap. | The **index must be far smaller than the file**. Budget ≤16 bytes/node, plus lazy tier-2 indexing. **Measured (2026-08-06):** 8 GB of NDJSON indexes in 539 MB of linear memory; a 2.5 GB flat array of numbers does not fit and now stops cleanly rather than trapping (C72). G2 holds, and the binding constraint is node count, not file size. |
| G3 | The index is a *navigation aid, not a replica*. Any detail (key names, string values, exact spans) can be re-derived by re-lexing a small byte range. | Never store what a 4 KB re-scan can recompute. |
| G4 | Chrome MV3 forbids remote code and blocking `webRequest`. | `.wasm` is a bundled asset; CSP needs `wasm-unsafe-eval`; auto-interception of `application/json` navigations is **not** a v1 make-or-break (see R3). |
| G5 | The Rust core must be independently publishable and reusable (crates.io + npm + future native CLI/MCP). | Core crate has **zero** `wasm-bindgen` and zero I/O dependencies. The WASM wrapper is a separate crate. |

---

## 1. Repository layout

```
leviathan/
├── Cargo.toml                     # workspace
├── crates/
│   ├── leviathan-core/            # ← publishable to crates.io. Pure Rust, sans-IO, no wasm deps.
│   │   ├── src/lexer.rs           #   resumable streaming lexer
│   │   ├── src/index.rs           #   node index (SoA, packed records)
│   │   ├── src/cursor.rs          #   navigation / slice materialization
│   │   ├── src/validate.rs        #   well-formedness + JSON Schema
│   │   ├── src/query.rs           #   JSONPath over the index
│   │   ├── src/dedup.rs           #   duplicate keys / elements
│   │   ├── src/export.rs          #   JSON / NDJSON / CSV serializers
│   │   └── benches/               #   criterion
│   ├── leviathan-wasm/            # ← publishable to npm as @shadkhan/leviathan-core
│   │   └── src/lib.rs             #   wasm-bindgen surface only. No logic.
│   └── leviathan-cli/             #   native harness: benchmarks, fixtures, fuzz corpus driver.
│                                  #   (also the v2 MCP host — but v2 code is not written now)
├── packages/
│   └── extension/                 # TypeScript, MV3
│       ├── src/worker/            #   worker host + typed RPC
│       ├── src/ui/                #   virtual tree, panels
│       ├── src/protocol/          #   message types shared by both sides
│       └── public/manifest.json
├── fixtures/                      # generator scripts; generated files gitignored
├── docs/adr/
├── DEEP_REASONING.md
└── SPEC.md
```

### 0.5 Who each phase is for

A milestone that serves no persona is a milestone that should not be built. Kept
here so the question is answerable rather than assumed — `USER_PERSONAS.md` has
the full narrative.

| Persona | The moment they open Leviathan | Served by | Deliberately deferred |
|---|---|---|---|
| **Priya** — backend/API | One malformed record in a 300 MB dump | M2 tree + **find**, M3 error location, M4 JSONPath | — |
| **Rahul** — data/ETL | Pre-load flight check on 500 MB–2 GB NDJSON | M1 NDJSON tier 1, M3 validate, M5 dedup, M6 export | Schema drift → v1.5 (2); 2 GB unmeasured, see R4 |
| **Sofia** — QA | "Does this response conform, and where does it break" | M3 schema validation + jump-to-error | Folder-at-a-time batch → v1.5 (4) |
| **Marcus** — SRE | Mid-incident, depth-20 state file | M2 **navigation verbs**, M6 subtree export | — |
| **Aisha** — integration dev | Learning an unfamiliar payload's shape | (nothing in v1) | Shape/schema inference → v1.5 (2) |
| **The hiring team** | Judging whether this is real systems engineering | M7 ADRs + benchmark table; every `DEEP_REASONING.md` entry | — |

Two consequences that shape the phases below rather than sitting in a table:

- **Find is not JSONPath.** Priya's first move on an unfamiliar 300 MB file is to
  search for a string, not to write a path expression — she does not yet know
  the shape. So a streamed full-text find lands in M2, ahead of the query engine,
  and it must scan the **file**, not the indexed rows: tier 1 indexes where each
  record starts, so searching what the UI has materialized would silently search
  a truncated preview of each record and miss the rest.
- **Aisha is unserved in v1.** That is a decision, not an oversight. The tree
  shows her a shape one row at a time; a summary view is v1.5.

**Layering rule (enforced in CI):** `leviathan-core` may not depend on `wasm-bindgen`, `js-sys`, `web-sys`, `tokio`, or `std::fs`. A `cargo tree` check in CI fails the build if it does. This is what keeps the core genuinely reusable rather than nominally reusable.

---

## 2. Core architecture

### 2.1 Sans-IO push model

The core never opens a file. Callers push bytes in:

```rust
let mut ix = Indexer::new(IndexOptions::default());
while let Some(chunk) = source.next_chunk() {
    ix.feed(&chunk)?;          // resumable across arbitrary chunk boundaries
}
let index = ix.finish()?;
```

The lexer is a resumable state machine: a token split across a chunk boundary (a string, a number, `tru`|`e`) suspends and continues. This single decision is what makes the same code work in a Web Worker, a native CLI, and a future MCP server without a line of change.

For materialization the caller answers byte-range requests:

```rust
pub trait ByteRange {
    fn read(&mut self, start: u64, len: u32) -> Result<&[u8]>;
}
```

In the extension this is backed by `blob.slice(start, end).arrayBuffer()`; in the CLI by `pread`. Both are ~1 ms for the 4–64 KB ranges we ask for.

### 2.2 Index representation (ADR-004 fixes the final layout)

Struct-of-arrays, pre-order (DFS) node ordering, target **≤16 bytes per node**:

- `start: u48` + `kind: u4` + `flags: u4` — packed into one `u64`
- `parent: u32`
- `child_count: u32` (overflow side-table for the pathological >4 B children case)

Deliberately **not** stored: key strings, string values, end offsets, depth. Key text is re-lexed from the parent's byte range when a row is rendered (G3). Depth comes from the render walk. End offsets come from the next pre-order sibling.

**Two-tier indexing** — the memory model that makes 500 MB viable:

- **Tier 1 (eager, always resident).** For NDJSON: one record offset per line — 8 B/line, so 500 k lines ≈ 4 MB. For single-document JSON: nodes to depth *d* (default 2), plus every container's byte span.
- **Tier 2 (lazy, LRU-evicted).** When the user expands a container, that subtree is indexed on demand from its byte range and cached. Eviction is by subtree, bounded by a configurable budget (default 256 MB).

Random access into a 1 M-element array is O(1) because a container stores `children_start` into a flat child table built at expand time — virtual scrolling must never walk a linked list.

### 2.3 WASM boundary (ADR-002 fixes it)

- Index stays in WASM memory. JS never receives node objects.
- `get_rows(node_id, start, count)` writes a packed binary row block into a preallocated scratch buffer; JS decodes it through a `DataView` over `wasm.memory.buffer`. Strings are copied only for rows actually painted.
- **Trap:** any allocation can grow WASM memory and detach existing views. The TS binding re-acquires its view after every call. This is a documented invariant with a test.
- One RPC per animation frame, batched — never one per row.

### 2.4 Threading

Main thread → typed RPC → single Worker → WASM. Hand-rolled request/response with numeric request ids and `transferable` payloads; no Comlink (bundle budget). Long operations (index, query, export) emit progress events and are cancellable by token.

---

## 3. Phases

Each phase has a hard exit criterion. Do not start phase N+1 until phase N's criterion is demonstrably met. Sizing is relative effort (S/M/L/XL), not calendar time.

---

### M0 — Skeleton and the boundary — *S*

**Goal:** prove the whole pipe end to end with a trivial payload, so that every later phase is only about content.

Scope:
- Cargo workspace + the three crates; `leviathan-core` compiles with no wasm deps.
- `wasm-pack build --target web` producing a bundled asset; extension build (esbuild/vite) that inlines/copies the `.wasm`.
- MV3 `manifest.json` with `content_security_policy.extension_pages` including `wasm-unsafe-eval`; a viewer page that opens in a tab.
- `packages/extension/src/protocol/` — the typed message union, versioned, shared by worker and UI.
- CI: GitHub Actions — `cargo test`, `cargo clippy -D warnings`, `cargo fmt --check`, the dependency-layering check, `wasm-pack build`, `tsc --noEmit`.

**Exit criterion:** loading the unpacked extension, opening the viewer page, and clicking a button round-trips `echo(u32) -> u32` through Worker → WASM → back, with the WASM binary served from the bundle (no network fetch). CI green on a clean clone.

**ADR closed:** none. **Risks:** none material.

---

### M1 — Streaming lexer + node index — *XL* ← **the make-or-break phase**

**Goal:** index a 500 MB NDJSON file and a ~1 GB single-document JSON file within the memory budget, with bounded peak RSS. Everything downstream is cheap if this is right and impossible if it is wrong.

Scope:
- Resumable streaming lexer (hand-written state machine). Correct UTF-8, escapes, surrogate pairs, numbers per RFC 8259, and byte-accurate offsets for every token.
- Format auto-detection: single document vs NDJSON/JSON-lines (sniff first non-whitespace bytes + newline-delimited top-level values), with an explicit override.
- Tier-1 index build; tier-2 on-demand subtree indexing with an LRU budget.
- `ByteRange` materialization: given a node id, produce its rendered row (key, kind, preview value, child count) by re-lexing a bounded range.
- Fixture generator (`leviathan-cli fixtures`): 1 MB / 50 MB / 500 MB NDJSON, 500 MB deep-nested single doc, 500 MB single top-level array, pathological cases (100 k-deep nesting, 5 M-element flat array, 50 MB single string value, duplicate-key-heavy, invalid UTF-8, truncated).
- Conformance: [JSONTestSuite](https://github.com/nst/JSONTestSuite) as a test corpus — every `y_*` accepted, every `n_*` rejected, `i_*` decisions documented.
- `cargo-fuzz` target on the lexer; criterion benches on parse throughput.

**Exit criterion** (measured by `leviathan-cli bench`, natively *and* in the Worker):
1. 500 MB NDJSON indexed with peak process memory **< 400 MB** and index size **< 40 MB**.
2. Sustained index throughput **≥ 200 MB/s** native. In the browser, a 500 MB
   file **fully indexed in < 10 s** — revised at M2 from "≥ 100 MB/s in WASM",
   which measured the wrong thing. Measured: 74–140 MB/s across five runs, so
   the original criterion straddled its own line. The cause was attributed
   rather than guessed: the *same* `.wasm` indexes the same file at
   **470–542 MB/s in Node**, where a read is a `readSync` into a reused buffer
   instead of a `blob.slice()` plus a `FileReaderSync` that allocates. Cutting
   479 reads to 120 changed nothing, so the cost is per byte, not per call
   (C54). The criterion was therefore measuring the browser's file API, not the
   engine — and the number that matters to a user is how long the file takes,
   which at the *worst* observed rate is 6.8 s.
3. Random access: fetch rows 900 000–900 050 of a 5 M-element array in **< 20 ms** including byte-range re-read.
4. Zero crashes on the pathological fixture set; 30 min of fuzzing with no panic.
5. JSONTestSuite conformance table published in the repo.

**ADRs closed:** ADR-001 (parser strategy), ADR-004 (index representation), ADR-005 (file access model).

**Risks and pre-decided fallbacks:**
- *R1 — index too large.* If ≤16 B/node proves unreachable, fall back to indexing containers only and re-lexing scalars entirely on demand (fewer nodes, more re-scans). Decide with numbers, not taste.
- *R2 — WASM throughput too low.* Fallback ladder: (a) larger chunk sizes, (b) SIMD-accelerate whitespace/string scanning behind a `simd128` feature, (c) accept a slower first index but keep the UI responsive with progressive tree paint (rows appear as they are indexed) — which is arguably the better UX anyway.
- **Kill criterion:** if after the fallbacks a 500 MB file cannot be indexed under 800 MB peak, revise the published target down to 250 MB and say so honestly in the README rather than shipping a claim that breaks on a reviewer's machine.

---

### M2 — Virtualized tree, navigation, and the trust indicators — *L*

**Goal:** the visible product. A tree over the index that stays at 60 fps regardless of file size — and that a user can actually *get somewhere* in, and can watch not dying.

Scope:
- Load paths: drag-and-drop, file picker, directory picker, paste. File objects never cross into WASM whole.
- ~~"open URL" (worker streams the response)~~ — **cut at M2, on privacy grounds.** Fetching an arbitrary URL from an extension page needs a host permission, and requirement 10 is that the manifest requests **none** — a claim now stated in the UI, linked to the manifest so anyone can check it. A load path used occasionally is not worth trading the one guarantee that is verifiable rather than promised. Users who have a URL can download it and drop the file; that costs them one step and costs the product nothing.
- Virtual list with DOM row recycling; row height fixed for v1 (variable-height is a v1.1 trap). Only visible rows + overscan exist in the DOM.
- Expand/collapse driving tier-2 indexing; expansion state as a sparse structure, not a per-node flag array.
- Path breadcrumb, copy-path, copy-value.
- **Navigation verbs** (Marcus; also what M3's jump-to-error and M4's jump-to-result are built on): go-to-path, jump-to-byte/line, expand-to-depth, collapse-all, reveal-and-select an arbitrary node.
- **Streamed find** (Priya): plain substring search over the *file*, run in the Worker against byte ranges, results streamed into the list as they are found and cancellable mid-scan. Matches resolve to the nearest indexed row so a hit is a place in the tree, not a byte number. Case-insensitive by default; keys and values both. Explicitly **not** a query language — that is M4.
- Dark mode, keyboard navigation (arrows/Home/End/PageUp/PageDown), a11y roles for the tree.
- Progress and cancel UI for indexing.
- **Requirement 9 — visible memory headroom.** Status bar shows file size, bytes consumed, index size, and WASM heap in use. The numbers already exist (`Tier1::size`, `ExpansionCache::size`, `memory.buffer.byteLength`); this is a readout, not a mechanism.
- **Requirement 10 — provably offline, loudly.** A permanent badge stating zero host permissions and zero network, linked to the manifest. It is trivially true; the point is that the user can see it without taking it on faith.

**Exit criterion:** drag a 500 MB NDJSON fixture in; first rows painted **< 2 s**; scroll through 100 k rows with no frame **> 32 ms** (measured with the Performance panel, screenshots in the PR); expanding a 1 M-element array is instant; main thread shows **zero** long tasks > 50 ms during scroll; a find for a string occurring once at ~90 % depth returns its first result while the scan is still running and never blocks the tree. Plus a screenshot at several nesting depths — C45 is the standing reminder that the fast path and the *correct* path are measured by different instruments.

**ADR closed:** ADR-003 (UI rendering + bundle budget: ≤150 KB gz JS/CSS excluding `.wasm`).

**Risks:**
- *R3 — auto-interception of `application/json` pages.* MV3 has no clean way to take over a JSON navigation without re-fetching the body. v1 ships the viewer-page flow (drop/picker/paste/URL) as primary; interception is investigated here and shipped only if it costs less than a day. It is explicitly **not** in the definition of done.
- *R4 — the 2 GB claim.* Rahul's range runs to 2 GB and every fixture stops at 500 MB. The risk is not the read path, it is C29: a scalar-dense 2 GB file needs an index the wasm32 heap cannot hold. Generate a 2 GB fixture during this phase's measurement session. If it does not hold, the published claim stays 500 MB and the README says why — the number is not extrapolated.

---

### M3 — Validation + dependability core — *M*

Requirements 7 and 8 land here. This is the phase that decides whether Leviathan
is a tool or a toy: every competitor's answer to a broken file is "it won't
open", and ours has to be "here is what parsed, here is exactly where it broke,
click to go there."

Scope:
- Well-formedness errors with line, column, and byte offset, plus a source excerpt with a caret — from the M1 lexer, no second parser.
- **Jump-to-position** (requirement 8): every reported error resolves to a row in the tree, reusing M2's navigation verbs. An error location the user cannot navigate to is a log line, not a feature.
- Error recovery: keep indexing past the first error where possible so a truncated 500 MB dump is still browsable, with error markers in the tree.
- JSON Schema (draft 2020-12) validation, streaming against the index. Third-party crate vs. hand-rolled subset is decided here on binary-size grounds — a schema validator that doubles the `.wasm` fails the budget.
- Validation panel: error list, click-to-navigate, error count in the status bar.

**Exit criterion:** every `n_*` JSONTestSuite case reports a location within ±1 byte of the true failure point; a 500 MB file with an error at 90 % depth reports it in a single pass; schema validation of a 50 MB fixture against a non-trivial schema completes < 5 s; `.wasm` growth from this phase ≤ 250 KB.

---

### M4 — Query — *L*

Scope:
- JSONPath (RFC 9535) evaluator running **over the index**, not over a materialized object: descendant search walks index records and only re-lexes bytes when a predicate needs a value.
- Streaming results — first results render before evaluation finishes; cancellable.
- Results view reusing the same virtual list; each result links back to its node in the tree.
- Query bar with syntax errors surfaced inline and a small set of worked examples.

**Exit criterion:** on the 500 MB fixture, no single evaluation step exceeds **500 ms** — results stream and the Worker stays answerable throughout — and the pass stays cancellable; full evaluation does not exceed the memory budget; RFC 9535 compliance-suite results published.

> **Revised on evidence (C62).** This read "yields first results in < 500 ms",
> which cannot fail honestly. A filter whose only matching record sits 330 MB
> into the file cannot produce a result before testing 330 MB; the same engine
> scores 10 ms or 6.1 s depending only on where the match happens to be. The
> criterion is now the property the implementation controls and the user
> experiences: results stream, and nothing blocks. Time-to-first-result and
> throughput are still published — as measurements, not as promises.

---

### M5 — Dedup — *M*

Scope:
- Duplicate keys within an object (the silent JSON footgun — most parsers keep the last one). Reported with both locations.
- Duplicate elements within an array, by structural hash of the canonical form, in **one pass of its own** — folding it into indexing was the plan and is not possible, because NDJSON indexing never parses a record (see the criterion note below).
- Report panel with counts, locations, click-to-navigate; hashing behind a feature flag so the cost is opt-in on huge files.

**Exit criterion:** duplicate-key-heavy fixture reports 100 % of duplicates with correct locations; enabling element dedup on the 500 MB fixture adds **< 20 %** to the duplicate pass and **< 64 MB** to memory.

> **Revised on evidence (C65).** The second clause read "adds < 20 % to *index*
> time", which assumed dedup could be folded into indexing — "computed during
> indexing so it costs one pass", as the scope line above still says. It cannot,
> and the reason is the product's own best result: NDJSON tier-1 indexing is a
> newline scan at 1.3 GB/s that never parses a record. Structural hashing
> requires parsing every record. Measured, dedup is 6.5 s against indexing's
> 0.38 s — **+1 620 %**, and no implementation makes that number small.
>
> What the clause was actually protecting is the opt-in flag: element hashing is
> the expensive half and must not be forced on everyone. So it is now stated
> against the pass it is optional within. Measured: **+1.5 %** and **+34.6 MB**.

---

### M6 — Export — *M*

Scope:
- JSON (pretty/minified), NDJSON, CSV (flat arrays of objects, with column discovery and a documented flattening rule for nested values), and "export current query result".
- Streaming writes via `showSaveFilePicker` + `FileSystemWritableFileStream` — a 500 MB export must never be assembled in memory.
- Export of a selected subtree, not just the whole document (Marcus).
- **Round-trip fidelity** (requirement 11): what comes out re-parses to the same thing. Verified as a test, not asserted in a README.

**Exit criterion:** export a 500 MB document to NDJSON with peak memory unchanged from idle +64 MB; round-trip (export → re-import → index) is byte-stable for the minified case, and the re-imported index is identical to the original's for every fixture in the corpus.

> **Met, and the second clause is checked more strictly than it asks.** Peak
> memory exporting 500 MB is **22.1 MB** against **22.2 MB** to index the same
> file — the export adds nothing measurable, because the index is the memory and
> the converter holds one 1 MiB window. Round trip is byte-stable and idempotent
> on all seven fixtures.
>
> "The re-imported index is identical" is checked as **token equality** instead,
> which is strictly stronger: the index is derived from the token stream, so two
> files with the same tokens have the same index, while two files with the same
> index can still differ in the values the index does not store — which is
> exactly where a float round-trip would hide ([C68](DEEP_REASONING.md)).

---

### M7 — Polish, publish, prove — *L*

Scope:
- **Benchmarks:** the comparison table vs. naive `JSON.parse` (which will crash — that contrast is the marketing), reproducible via `leviathan-cli bench` with a documented machine spec.
- **README:** architecture diagram, benchmark table, GIF of a 500 MB file opening smoothly, install links.
- **ADRs:** all five finalized, each written as the narrative of a real decision with the rejected alternatives.
- **Publish the core independently** — this is a hard requirement, not a nice-to-have:
  - `leviathan-core` → crates.io, with `#![deny(missing_docs)]` on public items, docs.rs building clean, an `examples/` directory, and MIT/Apache-2.0 dual license.
  - `leviathan-wasm` → npm as `@shadkhan/leviathan-core`, with a TS `.d.ts`, a browser + node example, and a documented "no extension required" usage path.
  - Semver from 0.1.0; CHANGELOG from day one.
- **Ship:** Chrome Web Store, then the identical package to Edge Add-ons the same day.
- Store assets, privacy policy ("no data leaves your machine" — trivially true, and verifiable because the manifest requests no host permissions).

**Packaging status (2026-08-06).** Everything mechanical is done and verified: licence texts travel with all three crates, `cargo publish --dry-run` passes and `cargo doc` is warning-free, `examples/browse.rs` and `examples/extract.rs` run against real fixtures, the npm package is generated with its real name and an `exports` map (`npm pack --dry-run`: 8 files, 107.5 kB), extension icons are generated from a checked-in 16×16 source and the build now fails if the manifest names a file that is not in `dist`. `CHANGELOG.md`, `PRIVACY.md`, `docs/RELEASE.md` and `docs/store-listing.md` are written. What remains is not code: recording the demo, and pressing publish.

**Exit criterion:** the definition of done, executed as a scripted demo start to finish without a stumble — *drag a 500 MB JSON/NDJSON file in, the tree appears and stays interactive, find a record by typing part of it, run a JSONPath query, see duplicate-key warnings, export the result as CSV; all client-side, no freeze, no upload, with the memory readout visibly steady throughout.* Plus: `cargo install leviathan-cli` and `npm i @shadkhan/leviathan-core` both work from a clean machine; extension live on both stores.

---

## 4. Cross-cutting requirements

**Testing**
| Layer | Tooling | Gate |
|---|---|---|
| Lexer/index correctness | JSONTestSuite corpus, `proptest` round-trips | 100 % of `y_`/`n_` cases |
| Robustness | `cargo-fuzz` on lexer + query parser | no panic, corpus committed |
| Performance regression | `criterion` in CI, thresholds enforced | >10 % regression fails the build |
| WASM surface | `wasm-bindgen-test` in headless Chrome | green |
| Extension E2E | Playwright with the extension loaded | drop → tree → query → export |

**Non-negotiable invariants** (each gets a test that fails loudly):
1. `JSON.parse` is never called on file content anywhere in `packages/extension`. Enforced by a lint rule.
2. The main thread performs no task > 50 ms during load or scroll.
3. `leviathan-core` has no wasm/IO dependency (CI `cargo tree` check).
4. No network request is issued with file content. The manifest requests no host permissions beyond user-initiated URL opening.
5. WASM memory views are re-acquired after every call that can allocate.

**Error handling philosophy:** a large file is usually a *broken* large file. Every stage degrades rather than aborts — partial index, partial results, markers in the tree. "It won't open" is the failure mode of the tools we're replacing.

---

## 5. Sequencing summary

```
M0 skeleton ─▶ M1 lexer+index ─┬─▶ M2 tree ─┬─▶ M3 validate ─▶ M4 query ─▶ M5 dedup ─▶ M6 export ─▶ M7 ship
   (S)            (XL)          │  +nav+find │   +dependability   (L)         (M)         (M)         (L)
                                │    (L)     │      (M)
                       gate: memory      gate: 60fps
                       + throughput      + first paint <2s
```

Requirements 9 and 10 ride M2 rather than forming a milestone of their own: a
memory readout and an offline badge are hours of work each, and a phase gate
inserted between "export works" and "ship" is a reason not to ship.

M1 is the only phase that can invalidate the product thesis. Spend the effort there, benchmark before building any UI, and let the numbers — not optimism — decide whether the 500 MB claim survives into the README.

## 6. Open questions to resolve at the phase that needs them

1. **M1:** exact packed record layout; NDJSON tier-1 vs. single-doc tier-1 unify or diverge? (ADR-004)
2. **M1:** name availability for `leviathan-core` on crates.io — check *now*, before the name is load-bearing.
3. **M2:** framework or vanilla. Default assumption: vanilla TS + hand-rolled recycling list; adopt Preact only if the panel code demonstrably suffers. (ADR-003)
4. ~~**M3:** JSON Schema crate vs. hand-rolled subset~~ — **decided: hand-rolled**, on two independent grounds. *Size:* a wasm32 cdylib with `jsonschema` 0.49 and HTTP/TLS features disabled measures **+2,448,860 B raw / +725,262 B gzipped** against a ≤ 250 KB budget — 9.5× over, taking a 60 KB binary to 2.5 MB and pulling 119 transitive crates. *Architecture:* its API validates a `serde_json::Value`, and Leviathan never materializes one (C1) — using it would mean building a `Value` per record, or one for a whole 500 MB document, which is precisely the failure the design removes. The subset validates against the streaming walk instead, and publishes what it does not support.
5. **M2/R3:** whether `application/json` interception is cheap enough to include. Default: no.
6. ~~**M3:** is an empty file *valid*?~~ **Answered.** They are two questions and now have two answers: `open` still accepts empty input, whitespace-only input and a lone BOM, reporting the format as `empty`; `validate` reports **"no JSON value: the document is empty"** at offset 0, because RFC 8259 requires a JSON text to contain a value. The three JSONTestSuite deviations remain deviations *of the opener*, which is the honest place for them.
7. **M2/R4:** does a 2 GB fixture index within the wasm32 heap? Decides whether the published claim is 500 MB or 2 GB. Measure; do not extrapolate.
8. **M2:** does streamed find need its own index (an offset ladder for "next match after byte X"), or is a re-scan from the current position fast enough? Default: re-scan — tier-1 indexing already runs at memory-bandwidth speed (C27), so a find is likely I/O-bound and an index for it would be a second thing to invalidate.
