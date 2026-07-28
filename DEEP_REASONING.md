# DEEP_REASONING.md

A running log of the core concepts Leviathan is built on and why they were chosen. Appended to as development proceeds — each entry states the idea, the reasoning, what it rules out, and how it was validated. Decisions that grow into full narratives graduate to `docs/adr/`.

Status legend: **assumed** (reasoned, not yet measured) · **validated** (measured, with numbers) · **revised** (superseded — kept for the trail).

---

## 2026-07-27 — Initial concept set (all **assumed** until M1 measures them)

### C1 — The index is a navigation aid, not a replica

The instinct when building a viewer is to parse the document into a structure that *is* the document. At 500 MB that structure is the problem, not the solution — it is the same failure as `JSON.parse`, just in a different language.

Leviathan instead builds an index that stores only enough to *find* things: for each node, where it starts, what kind it is, and who its parent is. Key names, string values, and exact end offsets are all recomputed by re-lexing a few kilobytes at the moment a row is painted. A 4 KB re-scan is roughly 10 µs; storing that same information for 10 M nodes would cost hundreds of megabytes. The trade is overwhelmingly one-sided.

*Rules out:* storing string values, key text, and end offsets in the index.
*Validate at M1 by:* index size ≤ 40 MB for a 500 MB NDJSON fixture, and row materialization under 20 ms for a random slice.

### C2 — Sans-IO core

`leviathan-core` never opens a file, never awaits, never knows what a `Blob` is. Bytes are pushed in (`feed(&[u8])`), and byte ranges are pulled back through a `ByteRange` trait the caller implements.

This is what makes the same code run unchanged in a Web Worker (backed by `blob.slice()`), a native CLI (backed by `pread`), and the eventual MCP server. Reusability claimed in a README is marketing; reusability enforced by having no I/O to be coupled to is structural. CI enforces it: the core may not depend on `wasm-bindgen`, `js-sys`, `web-sys`, `tokio`, or `std::fs`.

The cost is a resumable lexer — the state machine must survive a chunk boundary landing in the middle of a `\uD83D` escape. That cost is paid once, in one file, with a fuzzer pointed at it.

*Rules out:* `serde_json`'s `from_reader` and any parser that owns its input source.

### C3 — Two-tier, lazy indexing

Eagerly indexing every node of a 500 MB file is wasted work: the user will look at a few hundred rows of it. So tier 1 indexes only what is needed to draw the first screen and scrollbar — for NDJSON, one offset per line (8 B/line, ~4 MB for 500 k lines); for a single document, nodes down to depth 2. Tier 2 indexes a subtree on demand when the user expands it, cached with an LRU budget.

This is the difference between "500 MB works" and "500 MB works and 5 GB probably also works". It also collapses first-paint latency from "index the whole file" to "scan for newlines", which is memory-bandwidth-bound and embarrassingly fast.

*Rules out:* any design where a node id is a stable index into a fully materialized array — ids must survive tier-2 eviction.
*Open question:* whether NDJSON and single-document tiers share one representation or diverge (ADR-004).

### C4 — Random access beats linked traversal

Virtual scrolling asks for rows 900 000–900 050 of a 5 M-element array. A `first_child`/`next_sibling` linked structure answers that in 900 000 pointer hops — smooth in a benchmark over small trees, catastrophic in exactly the case Leviathan exists to serve.

So containers store an offset into a flat child table built at expand time: O(1) access to the *n*-th child. The 4 bytes per node this costs are the price of the product's entire premise.

*Validate at M1 by:* the random-access exit criterion (< 20 ms for a mid-array slice).

### C5 — The boundary is binary, and views are volatile

Nothing crosses the WASM boundary as an object. `get_rows()` packs a row block into a preallocated scratch buffer; JS decodes it with a `DataView` over `wasm.memory.buffer`, copying only the strings it actually paints. One RPC per animation frame, batched — chatty round-trips are how WASM projects end up slower than plain JS.

The sharp edge: any WASM allocation can grow linear memory and **detach every existing JS view**. A cached `Uint8Array` becomes a zero-length ghost, silently. So the TS binding re-acquires its view after every call that can allocate, and there is a test that grows memory mid-read to prove it.

### C6 — Degrade, never abort

A large JSON file is usually a *broken* large JSON file — truncated dumps, a log rotation mid-record, one bad escape at 90 % depth. The tools Leviathan replaces respond to this by refusing to open the file at all, which is the worst possible answer for someone trying to find out what went wrong.

Every stage therefore produces partial output: indexing continues past a syntax error and marks it in the tree, queries stream what they have, exports write what they can. "It won't open" is a competitor's failure mode, not ours.

### C7 — Measure before building UI

M1 is the only phase that can invalidate the thesis. The benchmark harness and fixture generator are built *before* the tree renderer, and the 500 MB claim is only written into the README after the number exists. A pre-declared kill criterion (if peak memory can't get under 800 MB, publish a 250 MB claim instead) exists so that the decision, if it comes, is arithmetic rather than ego.

---

## 2026-07-27 — M0 closed: what building the boundary actually taught

### C8 — Format detection is a question about tokens, not first bytes — **validated**

The first version of `sniff_format` accepted any byte that *could* begin a JSON
value. That made `2026-07-27 INFO started` a JSON document, because it starts
with a digit — and a timestamp at column zero is what the first line of most log
files looks like. The failure mode is precisely the one the product exists to
avoid: a tool that opens a file into a broken view instead of saying what the
file is.

The fix is to check whole tokens rather than opening bytes: a scalar counts as a
value only if it is both well-formed *and* terminated by whitespace, a structural
byte, or the end of the window. `2026` is a number; `2026-07-27` is not, and the
`-` that follows is what proves it. The same rule kills `nullable` as `null`, for
free.

What it deliberately does **not** reject is `true story` — a complete JSON
document followed by junk. That is a parse error with a byte offset (M3's job),
not a question about which index to build. Keeping detection and validation
separate is what stops the sniffer from slowly becoming a second parser.

*Rules out:* first-byte dispatch anywhere in the detection path.
*Validated by:* `format::tests::timestamped_log_lines_are_not_json` and the
malformed-number cases; the same table is asserted again from JS in the WASM
smoke test, so the Rust and TypeScript views of `Format` cannot drift apart
silently.

### C9 — The reusability claim needs a test, not a paragraph — **validated**

C2 says the core is sans-IO so it stays portable. That is an intention until
something fails when it is violated, so M0 shipped `scripts/check-layering.sh`,
which asserts three things about `leviathan-core`: no forbidden dependency in
`cargo tree`, no `std::fs`/`net`/`io`/`process`/`thread` in the source, and a
clean `cargo check` against `wasm32-unknown-unknown`.

The check was then verified by breaking the rule on purpose — a temporary
`std::fs::metadata` call — and confirming a red build. An unverified guard is
decoration; this one has been seen to fire.

Current state: **zero external dependencies**, which is stronger than the
contract requires. If M1 needs one, the check relaxes to the forbidden-list form
it already implements, and that relaxation is a visible diff rather than a quiet
drift.

### C10 — The compiler should know the Worker has no DOM — **assumed**

The rule "parsing never happens on the main thread" has a mirror image that is
easier to enforce: engine code must not touch the document. Rather than trust
review, the extension compiles as two TypeScript projects — the UI with
`lib: DOM`, `src/worker` with `lib: WebWorker` and `types: []`. A `document.`
reference in Worker code is a type error, not a runtime surprise in a bundle
that only misbehaves once a file is large enough to matter.

The shared `protocol/` module is compiled by both projects, which is what forces
it to stay neutral — it can name `Transferable`, but not `Window` and not
`DedicatedWorkerGlobalScope`.

### C11 — Budgets are set before there is anything to cut — **assumed**

`build.mjs` fails the build if gzipped JS+CSS exceeds 150 KB. At M0 the answer is
5.0 KB — 3 % — which is exactly why the budget went in now. A limit introduced
at M7, with a framework already chosen and a tree renderer already written, is a
number that gets renegotiated. A limit that has been green since the first commit
is a constraint that shapes the choices in ADR-003 instead of judging them
afterwards.

*Current measurements (M0, release build):*

| Artifact | Raw | Gzipped |
|---|---:|---:|
| `leviathan_wasm_bg.wasm` | 15,626 B | 7,247 B |
| `viewer.js` | 3,498 B | 1,698 B |
| `worker.js` | 3,603 B | 1,613 B |
| `viewer.css` | 4,723 B | 1,606 B |
| `background.js` | 115 B | 121 B |
| **JS + CSS total** | | **5,038 B / 153,600 B** |

### C12 — Startup ordering is a rule worth deleting rather than documenting — **assumed**

The Worker cannot answer anything until WASM instantiates, which invites a rule:
"wait for `ready` before calling". Rules like that survive exactly until the
refactor that forgets them, and the resulting bug is a message dropped on the
floor with no error anywhere.

Instead the first request *starts* instantiation and every request chains onto
the same promise, so ordering is preserved by the promise queue and there is no
rule to break. If instantiation fails, the promise still settles: `fatal` is
emitted once, and every queued call rejects rather than hanging in the client's
pending map forever. Applying C6 (degrade, never abort) to startup, not just to
parsing.

---

## 2026-07-28 — M1 begins: the instrument before the engine

### C13 — Build the measuring apparatus first, and let it embarrass you — **validated**

C7 said measure before building UI. M1 sharpened it: measure before building the
*engine*. The failure mode being avoided is subtle and common — the engine gets
built, then the harness gets written to report on it, and every design choice in
the harness quietly flatters the thing it was written for.

Building it first meant the harness had only baselines to measure, and it got
two of them wrong in ways that were caught precisely because there was no
engine to protect:

1. **It reported 93 GB/s, then 228 GB/s.** Both were faster than the machine's
   memory bandwidth, so both were self-evidently lies. Two separate causes.
2. Fixing them produced a distinction worth keeping: some workloads have a
   throughput and some only have a latency (C15).

An instrument that has already been caught lying twice, and hardened against
both, is worth more than one that has never been checked.

### C14 — Sub-millisecond work must be repeated, not timed — **validated**

The first bug. `sniff_format` over 64 KiB takes ~300 ns; `Instant::elapsed`
around it returns a value made mostly of clock granularity, and the reported
`0.000s` hid it. Dividing 65.5 kB by that produced 93 GB/s.

The harness now calibrates: probe once, extrapolate how many passes fill a 20 ms
window, run that many, divide. `sniff` on the 500 MB fixture is now the mean of
33,333 passes and reports **299 ns**. Durations render in ns/µs/ms/s, because
`0.000s` is a rounding artifact and not a measurement.

*Rules out:* any single-shot timing of work below the timer's resolution.

### C15 — Not every workload has a throughput — **validated**

The second bug, and the more interesting one. Even after C14, `sniff` reported
228 GB/s. The cause was not timing: `sniff_format` **early-exits** on NDJSON as
soon as it has seen two value-starting lines, so it reads perhaps 200 bytes of
the 65,536 it was handed. Dividing bytes-*given* by time-*taken* manufactured
throughput out of work that never happened.

Bytes/second is only meaningful when cost scales with bytes. So runs now declare
a `Metric`:

- **Throughput** — `read`, `scan`. Cost scales with input; MB/s means something.
- **Latency** — `sniff`. Bounded, data-dependent, early-exiting. The wall time
  *is* the answer, and the table prints `n/a †` with a footnote rather than a
  flattering number.

This will matter far more later than it does now. "First results in < 500 ms"
(M4) and "first paint < 2 s" (M2) are latencies; only indexing is a throughput.
Encoding the difference in the harness at M1 means the M7 README cannot
accidentally publish a MB/s figure for a workload that never read the bytes.

*Rules out:* a single `bytes / seconds` column applied to every row.

### C16 — Baselines are what make an engine number mean anything — **assumed**

The lexer does not exist, so the harness measures ceilings: `read` (I/O) and
`scan` (memory bandwidth, and exactly the operation NDJSON tier-1 indexing is
built from, per C3). On the reference machine, 500 MB NDJSON:

| Workload | Wall | Result | Peak RSS |
|---|---:|---:|---:|
| `read` | 1.03 s | 485 MB/s | **4.3 MB** |
| `scan` (1.77 M lines) | 520 ms | 961 MB/s | **4.3 MB** |
| `sniff` | 299 ns | `ndjson` | 4.3 MB |

*Machine: 8 × x86_64, Windows, `bench-native` profile.*

Two things this already establishes:

- **Peak RSS is 4.3 MB while streaming 500 MB.** The exit criterion is < 400 MB.
  The streaming model is not in doubt; what M1 must now show is that adding an
  index does not destroy it.
- **The lexer's target has a ceiling.** Exit criterion 2 asks for ≥ 200 MB/s
  native. Newline scanning — the least work anything could do while touching
  every byte — runs at 961 MB/s. So the criterion is asking the lexer to reach
  ~21 % of the trivial-work ceiling, which is demanding but not absurd. Had the
  ceiling come back at 250 MB/s, that criterion would have needed revising
  *before* a month went into chasing it.

### C17 — A size-optimized profile must not produce a published number — **validated**

`[profile.release]` is `opt-level = "s"` because the `.wasm` ships in an
extension. Benchmarking under it would understate the engine and mislead anyone
reproducing the result. A separate `bench-native` profile (`opt-level = 3`, fat
LTO, `panic = "unwind"`, symbols kept) exists solely for measurement, and the
harness prints the profile it was built with — refusing to be quiet about a
debug build, which it labels *"do not publish these numbers"*.

### C18 — Fixtures are generated, so determinism is correctness — **validated**

Fixtures reach 500 MB, so they are generated rather than committed. That makes
reproducibility a property of the generator: the same `--seed` must produce the
same bytes on any machine, forever, or "reproducible benchmarks" is a slogan. A
seeded xorshift64\* does it in eight lines with no dependency, and a test asserts
seed-equality directly.

Generating the 500 MB primary fixture takes **1.76 s** (284 MB/s), which matters
more than it sounds: a fixture nobody wants to wait for is a fixture that gets
replaced by a smaller one, and then the benchmark quietly stops testing the case
the product exists for.

### C19 — An unindented top-level array is indistinguishable from NDJSON — **validated (known limit)**

Found by the generator, not by a user. The `array` fixture wrote elements at
column 0, and `sniff_format` called it NDJSON — correctly, by its own rule,
because that byte pattern *is* the NDJSON pattern. The difference between the two
is a leading `[` and a matching `]` that may be 500 MB away, which no prefix
window can see.

Two responses, and the split between them is the point:

- The **fixture** was wrong and is now indented, because every real pretty-printer
  (`JSON.stringify(x, null, 2)`, `jq`, `json.dump`) indents. Emitting the
  unindented form made the fixture unrepresentative.
- The **limitation** is real and is now a named test in `leviathan_core::format`
  rather than a surprise waiting for someone else. The consequence is a wrong
  tier-1 index choice, not a wrong parse, and M1's streaming lexer resolves it
  exactly — it knows when it has closed a top-level value.

Writing a fixture generator found a core bug before the core had users. That is
the argument for building fixtures early, in one sentence.

---

## Log of revisions

*(Append here as concepts are validated or revised. Format: date — concept id — what changed — the number that changed it.)*

- **2026-07-27 — C2 — validated.** Layering enforced mechanically by
  `scripts/check-layering.sh`; core has zero external dependencies and compiles
  for `wasm32-unknown-unknown`. See C9.
- **2026-07-27 — C5 — partially exercised.** The M0 boundary passes only scalars
  and a bounded byte prefix, so the memory-detachment hazard is documented but
  not yet under test. The test that grows WASM memory mid-read is owed by M1,
  when `get_rows` makes it reachable.
- **2026-07-28 — C7 — sharpened into C13.** "Measure before building UI" became
  "measure before building the engine". The harness caught two of its own
  reporting bugs while it had nothing but baselines to report on.
- **2026-07-28 — C3 — first supporting number.** Newline scanning over 500 MB
  runs at 961 MB/s with 4.3 MB peak RSS, so the claim that NDJSON tier-1
  indexing is "memory-bandwidth-bound and embarrassingly fast" now has a
  measured ceiling behind it rather than an assertion.
