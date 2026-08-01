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

## 2026-07-28 — The lexer lands: what the bytes taught

### C20 — A token is a span, so the lexer never needs a buffer — **validated**

The obvious way to write a resumable lexer is to keep a scratch buffer: when a
token runs off the end of a chunk, copy what you have and finish it when the next
chunk arrives. That design has a failure mode built into it — a 50 MB string
value means a 50 MB buffer, and the `bigstring` fixture exists precisely to hit
it.

Leviathan's tokens carry `start` and `end` offsets and no content, which removes
the reason the buffer existed. What has to survive a chunk boundary is not the
bytes but the *state*: where the token began (a `u64`), and where in the grammar
the machine is (an enum). Both fixed-size. So the lexer's entire footprint is
constant regardless of what it is lexing:

| Fixture | Size | Peak RSS while lexing |
|---|---:|---:|
| `ndjson` | 500 MB | **4.3 MB** |
| `wide` (5 M elements) | 49.4 MB | **4.3 MB** |
| `deep` (100 k levels) | 800 kB | **4.3 MB** |

4.3 MB is the *baseline process*, unchanged from the C16 measurements taken
before the lexer existed. Lexing 500 MB added nothing measurable.

A test asserts the property directly rather than trusting the argument: a
100 kB string fed in 64-byte chunks produces exactly one token, spanning
`0..100_002`.

*Rules out:* any carry-over buffer, and with it an entire class of
"pathological input exhausts memory" bug.

### C21 — Counting lines is free because JSON forbids raw newlines in strings — **validated**

M3 owes users an error location with line and column. The apparent cost is a
comparison per byte, on the hot path, forever — which is the sort of tax that
gets paid quietly and then shows up in a benchmark nobody can explain.

It turns out not to be owed. **JSON forbids unescaped control characters inside
strings**, so a raw newline in a string is a syntax error, not content. Every
newline in a well-formed document is therefore whitespace *between* tokens, and
the only loop that must watch for one is the whitespace skipper — already
inspecting those exact bytes, and the least-travelled loop in the machine.
String bodies, where most bytes in most JSON documents live, never test for a
newline at all.

This is the kind of thing worth writing down because the reasoning is invisible
in the code: `step_ready` counting newlines and `step_string` not counting them
looks like an oversight until you know it is a proof.

*Rules out:* a separate newline-indexing pass, and the "line numbers are
expensive so make them optional" compromise.

### C22 — The lexer's cost is per token, not per byte — **validated**

The exit criterion is written in MB/s, and after the first measurements that
looks like the wrong unit. The same lexer, same build, same machine:

| Fixture | Shape | MB/s | Tokens/s |
|---|---|---:|---:|
| `ndjson` 500 MB | records with string values | 248–327 | 54–71 M |
| `wide` 49 MB | flat array of scalars | 232 | 47 M |
| `deep` 800 kB | nothing but `[` and `{` | 96 | **72 M** |

MB/s varies 3.4×. Tokens/s barely moves. The `deep` fixture is the tell: it is
~1.3 bytes per token, so it looks catastrophically slow in MB/s while actually
being the *fastest* run in the table by the measure that reflects work done.

The consequence is not academic. MB/s is a statement about the fixture's token
density as much as about the engine, so "≥200 MB/s" is only meaningful *for a
stated fixture* — and a future optimization could raise MB/s on one file while
regressing the engine on another. The harness now prints both, and the M7 README
will publish both with the fixture named.

This is C15's lesson recurring at a different altitude: the first version of the
harness picked a unit that flattered a workload; this is picking a unit that
describes one.

*Rules out:* a single headline MB/s figure for "the lexer".

### C23 — A benchmark that stops early must not report a rate — **validated**

Benching the `badutf8` fixture reported **5.9 kB/s**, which is not a lexer
speed — it is 19 valid bytes divided by the time to open a file. The lexer had
correctly refused the 20th byte and stopped, and the harness went on dividing
anyway.

Two fixes, and the second matters more than the first:

1. The read loop can now be told to stop, so the clock is not left running while
   400 MB of unlexed bytes are read for nothing.
2. A run that ends in an error is marked `Aborted` and reports **no rate at
   all** — the size column becomes "how far it got", and a footnote says the
   outcome is correct rather than failed, because for `truncated` and `badutf8`
   stopping *is* the expected result.

This is the third time this harness has been caught manufacturing a rate from
work that did not happen (C14 timing resolution, C15 early exit, C23 abort). The
pattern is consistent enough to state as a rule: **before dividing by time, ask
what the numerator actually measured.**

*Rules out:* publishing a throughput column for a run that did not complete.

### C24 — Recovery belongs to the caller, not to the state machine — **assumed**

C6 says degrade, never abort: a truncated 500 MB dump should still be browsable.
The tempting implementation is a recovery mode inside the lexer — skip to the
next plausible boundary and carry on. That would put "which byte is a safe place
to resume" (a question about NDJSON records, or about the tree the indexer is
building) inside a state machine that deliberately knows nothing about
structure.

So errors here are *sticky*: once failed, the lexer yields nothing further and
replays the same error. Resuming is an explicit act by the layer that knows where
it is safe — `Lexer::resuming_at(offset, line)` produces a fresh lexer whose
spans are still absolute, so tokens from a resumed lexer are indistinguishable
from tokens of one that started at byte 0. A test asserts exactly that.

The same primitive buys something not needed yet: if lexing ever has to be
parallelized across an NDJSON file, one lexer per byte range already works, with
no shared state to reconcile. That was not the goal; it fell out of keeping the
recovery decision at the right layer.

*Rules out:* a resynchronization heuristic inside the lexer. Validated when M3
builds recovery on top of it — if that turns out to need something this shape
cannot give, this entry gets a **revised** note.

### C25 — 108 133 846 tokens, exactly, every time — **validated**

The throughput of a single run varies ±15 % on a desktop OS, which makes any one
number a sample rather than a result. What does not vary is the token count: the
500 MB fixture lexes to 108 133 846 tokens on every run, at every chunk size,
whether fed 1 byte or 1 MiB at a time.

That is the assertion worth putting weight on. A changed token count is a
correctness regression and cannot be dismissed as noise, whereas a 12 % slower
run usually can. So the harness reports the exact count next to the rate, the
tests assert counts rather than timings, and published figures are stated as
ranges over repeated runs rather than the best of five.

*Rules out:* CI performance gates on wall time (flaky by construction); the
regression signal is the deterministic count.

---

## 2026-07-29 — Structure and tier 1: the index gets a shape

### C26 — Tier 1 is one child table, and both formats share it — **validated**

C3 left an open question for ADR-004: whether NDJSON and single-document input
need different tier-1 structures. They do not, and seeing why collapses a lot of
anticipated complexity.

An NDJSON stream is a root whose children are records. A JSON document is a root
whose children are its members or elements. Both are *the direct children of the
root*, and both are served by one structure — a flat `Vec<u64>` of byte offsets
with O(1) access to the *n*-th. Expanding a node later produces another table of
the same type, so there is one addressing model in the engine rather than two
that drift apart at the third bug.

What the two formats do not share is how the table gets *built* — a newline scan
versus a structural walk (C27) — and that difference is an implementation
detail behind one type, which is exactly where a difference should live.

*Rules out:* a separate NDJSON index type, and the `if ndjson { … } else { … }`
that would have spread from it into every consumer.

### C27 — A newline is always a record boundary, so tier 1 does not parse — **validated**

The obvious way to find NDJSON records is to parse and note where each top-level
value ends. The fast way is to look for `\n`. The fast way is usually also a
heuristic that breaks on a value containing a newline — except that in JSON, it
cannot. Unescaped control characters are forbidden inside strings and no other
token can contain one (C21 again, third appearance). So scanning for newlines is
not "usually right"; it is exact.

The consequence is the difference between opening a file and parsing one:

| Workload on the 500 MB fixture | Wall | Rate |
|---|---:|---:|
| `scan` — count newlines (the ceiling) | 419 ms | 1.2 GB/s |
| **`index` — build tier 1** | **388 ms** | **1.3 GB/s** |
| `walk` — full parse and validate | 2.30 s | 218 MB/s |

Tier 1 runs *at the ceiling*, six times faster than parsing. A 500 MB log file is
browsable in under half a second, before a single byte has been validated — which
is C6 (degrade, never abort) turning out to be a performance strategy and not
just an error-handling one.

Three independent methods agree on 1 772 686: newline count, record count, and
the document count from the full grammatical walk. Cross-validation across three
different algorithms is worth more than any one of them being tested.

*Rules out:* parsing as a precondition for opening a file.

### C28 — A row's offset points at its key, not its value — **validated**

A row in an object reads `"name": "leviathan"`, so painting it needs both. The
child table stores one offset per child, and the choice of which one is not
symmetric: from the key, lexing forward yields key, colon and value in a single
pass; from the value, recovering the key would mean lexing *backwards*, which a
streaming lexer cannot do at any price.

So a child offset is defined as "where the row starts" rather than "where the
value starts", and for arrays the two coincide. One sentence of definition
removes an entire impossible problem.

*Rules out:* storing key spans alongside value offsets — which would have doubled
the index.

### C29 — 8 bytes per child is cheap per byte and expensive per element — **validated, and a limit**

The index is 8 bytes per child and nothing else (no kind, no length, no count —
all re-derived on paint). On the fixture the product exists for, that is
excellent:

| Fixture | File | Index | Ratio |
|---|---:|---:|---:|
| `ndjson` 500 MB | 500 MB | **14.2 MB** | 2.8 % |
| `wide` 5 M elements | 49.4 MB | **40.0 MB** | **80.9 %** |

The second row is the finding. An array of small scalars costs ~10 bytes per
element in the file and 8 bytes per element in the index, so the index approaches
the size of the data. Extrapolated, a 500 MB file shaped like `wide` would need
~400 MB of index and would miss the < 40 MB criterion by an order of magnitude.

Stating it plainly rather than quietly benchmarking only the flattering fixture:
**the exit criterion is met for record-shaped data and not for scalar-dense
arrays.** Two mitigations are identified and neither is built yet, because
building them before the row materializer would be optimizing a number nobody is
yet looking at:

1. **Delta + varint.** Consecutive offsets differ by ~10, so the deltas are one
   byte each — a 4–8× reduction, at the cost of O(1) access becoming
   O(1)-per-block.
2. **Sparse indexing.** Store every 64th offset and re-lex within the bucket to
   reach the rest: 64× smaller, ~64 elements of re-scan per access, which is
   microseconds. This is C1's own trade applied to the index itself.

Note that SPEC's pre-declared risk R1 anticipated the wrong failure. It assumed
the danger was per-node *width* (">16 B/node") and proposed indexing containers
only. The width is 8 B and the problem is per-node *count*, which that fallback
does not address. The pre-declared mitigation was reasonable and it was aimed at
the wrong variable — which is the argument for measuring before optimizing, not
against pre-declaring.

*Rules out:* claiming the index-size criterion is met without naming the shape of
data it is met for.

### C30 — The token that needs a flush is the one most files end with — **validated**

Two new bench workloads were written and both had the same bug: neither called
`Lexer::finish()`. Nothing failed. The fixtures all end in `}` or `]`, and every
token except a number is self-terminating (C14), so the omission was invisible.

It is invisible on exactly the wrong inputs. A file ending `...,42` with no
trailing newline silently loses its final value — and hand-written NDJSON ends
without a trailing newline constantly. The symptom would have been an
off-by-one in a record count that nothing else could explain.

The fix was three lines; the lesson is about where the three lines live. They are
now one named function with the reasoning in its doc comment, called from both
workloads, and a test feeds `1\n2\n3` with no trailing newline and asserts three
documents. A shared step that is easy to forget and silent when forgotten should
not be a thing each caller remembers to do.

*Rules out:* open-coding the lexer/structure shutdown sequence at each call site.

---

## 2026-07-29 — Rows: the other half of C1's bargain

### C31 — Storing nothing was the right trade, by two orders of magnitude — **validated**

C1 asserted that storing 8 bytes per node and re-reading a few kilobytes to
paint a row beats storing the row. It was reasoned, not measured, and it was the
single assumption the whole index rests on. Now measured — fetching 50 rows from
deep inside two fixtures, reconstructing every field (key, kind, preview, child
count) from the file:

| Fixture | Slice at row | Bytes re-read | Cold | Warm |
|---|---:|---:|---:|---:|
| 5 M-element array | #4 499 955 | 495 B | **65–119 µs** | **14–18 µs** |
| 500 MB NDJSON | #1 595 372 | 14.1 kB | **0.74–1.09 ms** | **98–248 µs** |

The exit criterion is **20 ms**. The cold figure on the array is ~170× inside it,
and the warm figure ~1000×. There was never a version of this where storing more
would have been worth it.

Two caveats stated rather than buried. The file is in the OS page cache — which
it genuinely is, because building the index just read it end to end — so these
are warm-file numbers, not cold-disk ones. And a browser's `Blob.slice()` costs
more per call than a `pread`, which is exactly why the design does one read per
*slice* and not one per row (C32).

*Rules out:* revisiting the index's per-node width. It has margin to spare.

### C32 — One read per screen, because siblings are contiguous — **validated**

Fifty rows could have meant fifty byte-range reads. Siblings are adjacent in the
file, so instead the whole run is one read: rows 900 000–900 050 of the array
span 495 bytes and cost a single `read`. The measurement above is of one read,
not fifty.

Natively that saves syscalls, which is nice. In the Worker it is the difference
between the design working and not: `Blob.slice().arrayBuffer()` costs about a
millisecond regardless of how few bytes it fetches, so fifty of them is 50 ms and
one is 1 ms — the criterion missed versus met, decided entirely by batching. Same
reasoning as C5's "one RPC per animation frame", one layer down.

The window is capped, so a run larger than the cap becomes several reads rather
than one enormous one, and a test asserts that the window size never changes the
rows — window size is an I/O tactic and must be invisible in the output.

*Rules out:* a per-row `get_row(id)` API. The unit of materialization is a slice.

### C33 — Every row has a budget, because one row must not stall a screen — **validated**

Painting a row means answering "how many children?", and that means walking the
container. For a 400 MB container that is seconds — one row freezing the viewer,
which is the exact failure Leviathan exists to avoid, reintroduced at the last
step.

So the walk stops after `row_budget` bytes (8 KiB) and reports `AtLeast(n)`
instead of `Exact(n)`. The UI shows `1,000+ items`; the exact number arrives when
the node is expanded and genuinely indexed. Previews are truncated the same way,
so a 50 MB string value costs what a short one costs.

The fixtures show both sides working. On the 500 MB NDJSON slice, **50 of 50**
containers are counted exactly — real records fit in 8 KiB, so the budget is
invisible in practice. On the 100 000-deep fixture, the single root row reports
`AtLeast` — the budget engaged exactly where it should.

*Rules out:* an exact child count as an unconditional guarantee. Bounded and
approximate beats exact and unbounded, when the alternative is a frozen tab.

### C34 — A broken row is a row — **validated**

C6 said degrade, never abort, and until now that was a policy without a
mechanism. Row materialization is where it becomes concrete: a value that does
not lex renders as `ValueKind::Invalid` with the reason in its preview, and its
neighbours render normally. The only error that propagates out of `materialize`
is a *source* error — the file handle was revoked, the disk went away — because
that is the one case where there are no bytes to show.

Measured on the fixtures that exist to be broken: the `truncated` fixture
materializes 50 good rows from the middle, and `badutf8` materializes 50 rows of
which none can be counted exactly (each record's invalid byte stops the count) —
and both still open, scroll and display. A viewer that refused these files would
be failing precisely the user who most needs to see inside them.

*Rules out:* `Result` as the return type for anything the user should be able to
look at.

### C35 — The sans-IO claim finally has a second implementor — **validated**

C2 said the core is reusable because it has no I/O to be coupled to, and CI has
enforced the negative (no `wasm-bindgen`, no `std::fs`) since M0. But a trait
with one implementor is a trait shaped like its one implementor, and until now
`ByteRange` was implemented only for `&[u8]` — inside the crate, by tests.

`FileSource` in the CLI is the first outside implementor: a real file, seeking
backwards, reusing one scratch buffer across calls, discovering its own end when
a speculative window overruns it. It needed no change to the trait, which is the
first actual evidence for the reusability claim rather than an argument for it.

One thing it did surface: row windows ask for bytes past the end of the file as a
matter of course (the last row's budget extends past EOF), so "out of range" is a
normal condition rather than an error. `SourceError::OutOfRange` carries
`available`, so one probe recovers — and the Worker's `Blob` implementation will
hit exactly the same case.

*Rules out:* treating a short read at end-of-file as a failure.

---

## 2026-08-01 — Tier 2: what a click costs

### C36 — An expansion is disposable because a node id is a byte offset — **validated**

C3 committed to a tier 2 that is built on demand and thrown away under memory
pressure, and left the hard half unstated: if expansions can be evicted, what is
a node id?

The tempting answer is an index into the index — a `u32` row number, dense and
half the width. It does not survive eviction. The moment tier 2 drops an
expansion, every outstanding id that pointed into it means something else or
nothing — and the UI is holding those ids: in a scroll position, in a breadcrumb,
in a request that crossed the Worker boundary two frames ago. Making that safe
needs generation counters, an invalidation message, and a rule for what the UI
does when its selection evaporates. That is a protocol spanning three layers,
existing to service an implementation detail of a cache.

A byte offset is 8 bytes instead of 4 and has none of it. It is derived from the
file rather than from the index, so it is stable for the life of the file no
matter what the cache does. Eviction therefore loses *work* and never
*addressability*, and re-expanding the same offset produces a byte-identical
table — asserted directly, because the entire argument rests on it
(`an_evicted_expansion_rebuilds_identically`).

That is why `ExpansionCache` is a pure LRU: no generation counters, no
invalidation protocol, no callback to the UI. It may drop anything at any moment
and the only cost is doing the work again. Its lookup is a linear scan for the
same reason — a person expands a few dozen nodes by hand, not a few thousand, so
a scan over tens of entries beats a hash map's overhead and one more
dependency-shaped decision.

*Rules out:* node ids that index into anything the cache owns; an invalidation
protocol between tier 2 and its consumers.

### C37 — The end-of-source flush is a bug class, not a bug — **validated**

Third sighting. C30 found it in two bench workloads at once; `rows` carries a
comment citing C30 at the single place it lexes to a boundary; expansion had it
again, in what is now `stop_at_source_end`.

The shape never changes. A number is the only JSON token that cannot be emitted
until the byte after it arrives (C20), so any path that stops feeding the lexer
without calling `Lexer::finish()` drops a final pending value. It is silent, and
it is silent on exactly the wrong inputs: every fixture ending in `}` or `]`
behaves identically with the bug and without it. What breaks is `[1,2,3,4` — a
container truncated by a killed export or a full disk, where the user is looking
at the file *because* it is damaged, and where losing the last element quietly is
the worst failure available.

C30's conclusion was to make the shutdown one named function with the reasoning
attached. That was right and it was not sufficient, because the sequence is not
actually shared: `rows` stops at a budget, expansion stops at a container close,
the bench workloads stop at end of file, and each derives its own path to the
same edge. Three occurrences across four consumers says the interface invites the
omission rather than that the callers were careless.

What caught it here was not review. It was a test that feeds `[1,2,3,4` and
asserts **four** children (`a_truncated_container_keeps_the_children_it_found`).
That test is now the price of admission: a new consumer of `Lexer` owes a
truncated-input case before it owes anything else.

*Rules out:* relying on review, or on a well-commented helper, to prevent this;
adding a lexer consumer without a truncated-input test.

### C38 — A growing index and a resident index have different costs — **validated**

`ChildTable` is a `Vec`, and a `Vec` grown to five million entries holds capacity
for 8 388 608 — it doubles, and the final doubling is always mostly unused. At 8
bytes per child (C29) that is **67.1 MB of allocation for 40.0 MB of contents**.
The 27 MB of difference is not transient. It is charged against the cache's
256 MB budget for as long as the expansion is resident, which is the rest of the
session, and it can never be used, because the container is closed and no further
child can arrive.

So a terminal stop seals the table. Measured on the 5 M-element fixture: **40.0
MB for 5 000 000 children**, exactly 8 bytes each, no headroom. That is 40 % more
expansions per budget, bought with one `shrink_to_fit` at the one moment it is
provably free.

Only at a terminal stop, though. A batch-limited stop keeps every byte of its
capacity, because a table about to grow again would only re-allocate and re-copy
— the same reasoning that makes amortized growth worth having at all. There are
two tests, one per side, because the distinction is the point and neither half is
interesting alone.

The general shape: a structure with amortized growth living inside a memory
budget has two lifetimes, and the allocator's default policy is tuned for one of
them.

*Rules out:* treating index size as a single number; a blanket shrink after every
batch.

### C39 — Expansion yields, because the container that stalls is the one worth opening — **validated**

Enumerating a container's children means walking it, and there is no way around
that: child *n*'s offset is not knowable without having walked children 0..*n*.
Expanding the root of the 5 M-element fixture takes **515 ms**. Doing that inside
a single call means a viewer that stops answering for half a second when someone
clicks a triangle — the failure this project exists to remove, reintroduced at
the last remaining interaction.

`Expansion::advance` therefore returns after `batch` children (10 000) holding
its lexer, grammar and collector state, and the caller decides whether to
continue. The 515 ms is 501 such calls of roughly a millisecond each, and between
any two of them the Worker can paint what it has, honour a cancel, or simply not
come back. Rows appear as they are found rather than after all of them are found
— which SPEC's risk R2 had already noted is "arguably the better UX anyway".

This is only possible because of a property established two layers down: dropping
the lexer's chunk iterator early folds the consumed count into an absolute offset
(C20), so stopping mid-window costs nothing and needs no buffer. Two tests pin
the consequence — the batch size never changes the result, and neither does the
window size.

The rate is **96.0 MB/s**, against the same walk performed by tier 1 in one pass:

| Workload | Wall | Rate | Result |
|---|---:|---:|---|
| `lex` | 350 ms | 141.1 MB/s | 10 000 001 tokens |
| `index` (tier 1) | 382 ms | 129.5 MB/s | 40.0 MB table |
| `expand` (tier 2) | 515 ms | 96.0 MB/s | the same 40.0 MB table |

Same bytes, same lexer, same collector, 35 % slower. The likely cause is read
size: tier 1 streams 1 MB chunks, while expansion reads a 256 KiB window chosen
to be one `Blob.slice` in the Worker. That is a hypothesis, not a measurement —
`ExpandOptions::window` is not reachable from the bench CLI yet. **Open:** wire it
through and find out whether the 35 % is the window, the per-batch bookkeeping,
or both. The answer decides whether the Worker's window should be larger than a
comfortable slice.

Worth stating plainly: this is the worst container in the corpus, and it is one
no user has. Expanding a *record* — what NDJSON viewing actually does — indexes
eleven children in **1.03 ms**.

*Rules out:* a blocking `expand(node)` anywhere in the boundary; publishing
96 MB/s as "tier 2's cost" without the caveat that it is the root of a 5 M-element
array.

### C40 — Every reader is speculative at its edge, so clamping belongs to the source — **validated**

C35 established that a short read at end of file is a normal condition rather
than an error, and `rows` grew a private `read_clamped` to deal with it.
Expansion then needed the identical function for a different reason: `rows` asks
for a budget past the last row without knowing whether the file extends that far,
and expansion asks for a window past the container's close out of the same
ignorance.

Two independent readers deriving the same helper is the signal that it belongs to
neither. It moved to `source`, beside the trait whose contract it is really
describing: every consumer of `ByteRange` reads speculatively at its edge, and
every one of them wants the short read rather than the diagnostic. The version
living there keeps the probe path for sources that will not state their length —
which is what a stream being consumed for the first time looks like, and what the
Worker's `Blob` will look like before anyone asks it.

*Rules out:* a per-module clamp; treating `len_hint()` as required for a source
to be usable.

---

## 2026-08-01 — The boundary, for real

### C41 — The loop around the index belongs in the core — **validated**

Tier 1 was, until now, a set of parts with no assembly: `RecordScanner` for
NDJSON, `Lexer` + `Structure` + `RootCollector` for a document, and no code
anywhere that read a source and drove either one. Every consumer wrote its own.
The CLI's benchmark had a `build_table` doing it, and the Worker was about to
grow a second version in TypeScript — on the far side of the WASM boundary,
where it could not be unit-tested and where its bugs would be indistinguishable
from engine bugs.

`Build` is that loop, and moving it into the core was worth more than the
deduplication. It made three things true at once that were not true before:
`Tier1` finally has something that constructs it; the flush-at-end-of-source
rule (C37) exists in one place per format instead of once per consumer; and the
NDJSON and single-document paths became genuinely interchangeable to callers,
which is what C26 claimed when it said one `ChildTable` and two builders.

The shape is pulled rather than pushed, which is the part that took a decision.
Both underlying builders take fed bytes, so the obvious design is for the host
to read a chunk and hand it over. That was rejected because tier 2 *cannot* work
that way — `Expansion` decides where to read next and only it knows where — and
a host that implements one byte-delivery mechanism for indexing and a different
one for expansion has two things to get right instead of one. Everything pulls
through `ByteRange` now, and the host implements exactly one method.

**Open, and stated plainly:** the CLI's `build_table` has *not* been moved onto
`Build` yet, so the duplication this entry describes is currently two
implementations rather than one. That matters more than tidiness — the `index`
benchmark now measures a code path the engine no longer uses, and its published
throughput is therefore a number for the old loop. Porting it will change that
figure (`Build` reads a 1 MB window and yields every 4 MB, where the bench
streams `--chunk` bytes straight through), which is exactly why it is a separate
change with its own before-and-after rather than a quiet edit inside this one.

*Rules out:* a push-based tier-1 entry point; a second indexing loop in any
consumer, in any language — once the CLI is ported.

### C42 — `FileReaderSync` is what makes the sans-IO design work in a browser — **validated**

The core reads synchronously: `ByteRange::read` returns bytes, not a future.
That was chosen for portability (C2) and it collides head-on with the browser,
where reading a `Blob` is `slice().arrayBuffer()` — a promise. A promise cannot
be awaited from inside a WASM call, so on the face of it the design does not run
in the place it was designed for.

There were three ways out, and the choice matters enough to write down:

1. **Invert the core** so it returns "I need bytes at X" and is resumed with
   them. Portable, and it puts an async hop inside the lexer's inner loop and a
   state machine in every caller. It would make the CLI worse to serve the
   browser.
2. **Pre-buffer** windows the host predicts. Works only where reads are
   predictable, which excludes expansion — the case that needs it most.
3. **`FileReaderSync`.** Synchronous, blocking, and available *only* in a
   Worker.

The third is not a workaround, it is the design landing where it was aimed. The
rule this project enforces is that the **main thread** never blocks; the Worker
exists precisely to be the thread that may. `FileReaderSync` blocks a thread
whose blocking is free, and the core crosses into the browser with no change at
all — the third implementor of `ByteRange`, after `&[u8]` and the CLI's
`FileSource`, needing no change to the trait for the third time.

*Rules out:* an async `ByteRange`; the pull-inversion in option 1; running the
engine anywhere but a Worker.

### C43 — Rows cross as one buffer, and offsets cross as doubles — **validated**

ADR-002 has claimed since M0 that index data reaches JS without allocating an
object per row. It was a claim about a mechanism that did not exist. It does
now: `pack.rs` writes a screen of rows as a header, fixed-width 40-byte records,
and one UTF-8 blob, and the whole thing crosses as a single transferred
`ArrayBuffer`. Fifty rows are one allocation and one transfer instead of fifty
objects and a hundred strings, and a row's two strings are decoded only when
that row is actually painted — so a block scrolled past costs nothing beyond the
transfer.

Two details are worth keeping because both were nearly decided the other way.

**Strings are located by length, not by offset.** The decoder walks rows in
order and accumulates, so two lengths (6 bytes) replace two offsets (8 bytes)
and the constraint they impose — decode in order — is one the consumer obeys
anyway. The prefix sum is built once on first random access.

**Offsets are `f64`, not `BigInt`.** A double is exact to 2^53, which is nine
petabytes; no JSON file will ever reach it. `BigInt` would be correct and would
put a conversion in the renderer's hot path to buy a range that does not exist.
The same reasoning applies on both sides of the boundary, so `u64` fields in the
packed rows are read as two `u32`s rather than with `getBigUint64`.

And the layout is versioned, asserted at Worker startup and again by the
decoder. A layout skew between a rebuilt bundle and a stale `.wasm` is not a
type error — it is plausible-looking wrong rows, which is the worst failure
mode available. Eight bytes of header turns it into a sentence.

*Rules out:* one JS object per row; `BigInt` anywhere in the row path; an
unversioned binary layout.

### C44 — The bug an array element could never have shown — **validated**

Wiring the boundary turned up a defect in `rows`, sitting in code that had been
green for two days: **an object member whose value is a container reported zero
children.** `{"tags":[1,2,3]}` said `tags` was empty. The tree would have shown
no expand arrow on it.

The cause is one argument. `count_children` was called with the *row's* offset
and the row's bytes, and a row's offset points at its key, not its value (C28).
So the walk started at `"tags"`, read a complete JSON document — a string is a
document — and called the `:` after it trailing garbage. It returned zero,
politely, exactly as C6 says a failed walk should.

What makes this worth an entry is why every existing test missed it. For an
**array element**, the row's offset and its value's offset are the same number,
so the wrong argument is indistinguishable from the right one. And every fixture
that exercised child counting was an array of arrays, or NDJSON — whose tier-1
table is unkeyed, so its rows are array-shaped too. C33 reported "50 of 50
containers counted exactly" on the NDJSON fixture and that measurement was
correct; it simply could not see this. A test suite built entirely on unkeyed
tables cannot distinguish a row's offset from its value's offset, and neither
can a reviewer reading the call.

The lesson generalizes past this bug: where two quantities coincide in the
common case, the tests must include a case where they differ, or that pair is
untested no matter how many tests exist. There is now one for keyed containers.
It also says something about what integration work is for — this was not found
by review, or by 223 unit tests, but by asking the engine a question the UI
would ask.

*Rules out:* fixtures that are exclusively arrays or NDJSON for anything
row-related; treating a row's offset and its value's offset as interchangeable.

---

## Log of revisions

*(Append here as concepts are validated or revised. Format: date — concept id — what changed — the number that changed it.)*

- **2026-08-01 — C3 — fully discharged.** Two-tier lazy indexing has both tiers.
  Tier 2 expands the worst container in the corpus — 5 000 000 children — in
  **515 ms** across 501 yielding batches for a **40.0 MB** table, and the
  eviction question C3 left open is answered by C36: byte-offset ids make the
  cache pure, so there is no invalidation protocol to design.
- **2026-08-01 — C6 — third mechanism.** "Degrade, never abort" was policy in C6
  and became row-level in C34; it is now container-level. A truncated or
  malformed container reports *why* it stopped and keeps every child it found,
  and all three stops are terminal without being destructive.
- **2026-08-01 — C29 — holds at tier 2.** The 8-bytes-per-child figure was
  measured on tier 1; tier 2 reproduces it exactly for the same container, and
  sealing (C38) makes the resident cost equal to the contents rather than 1.68×
  them.
- **2026-08-01 — C5 — half discharged.** The boundary now moves a payload
  rather than a scalar, and the hazard C5 named is closed by construction on the
  way out: wasm-bindgen copies a returned `Vec<u8>` out of linear memory before
  JS sees it, so the buffer the Worker transfers is never a view onto WASM
  memory and cannot be detached by a heap growth. The test C5 actually owes —
  growing WASM memory *during* a read — is still owed, and now writable.
- **2026-08-01 — C2 — validated by a third implementor.** `JsSource` in
  `leviathan-wasm` implements `ByteRange` over a JS callback, with no change to
  the trait. Three implementors — a slice, a file, a browser `Blob` — and the
  trait has not moved once (C42).
- **2026-08-01 — ADR-002 — claim now has a mechanism.** "Node slices cross
  without allocating per-row objects" was an assertion in a doc comment from M0
  until C43 built the packed layout behind it. The ADR can now be written from
  a thing that exists.
- **2026-08-01 — C33 — measurement stands, coverage did not.** The "50 of 50
  containers counted exactly" figure is still correct, and it was measured
  entirely on unkeyed tables, which is why it could not see C44. Both are true
  and the second is the more useful thing to remember.

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
- **2026-07-28 — C16 — partially discharged.** The ceiling it established
  (`scan` at ~960 MB/s) now has an engine measured against it: the lexer reaches
  248–327 MB/s, or roughly 30 % of the trivial-work ceiling, against an exit
  criterion of 200 MB/s. Headroom for the index build exists; the R2 fallback
  ladder is not needed yet.
- **2026-07-29 — C3 — open question closed.** "Whether NDJSON and
  single-document tiers share one representation or diverge" is answered by C26:
  one `ChildTable`, two builders. This was ADR-004's first open question.
- **2026-07-29 — C1 — fully validated.** Both halves now measured: index size
  **14.2 MB** against a 40 MB budget, and a random 50-row slice materialized in
  **65–119 µs** against a 20 ms budget (C31). The trade C1 proposed — store
  offsets, re-read to paint — was right by roughly two orders of magnitude.
- **2026-07-29 — C2 — validated by a second implementor.** `FileSource` in the
  CLI implements `ByteRange` against a real file with no change to the trait
  (C35). The layering check proved the core has no I/O; this proves the core is
  usable without it.
- **2026-07-29 — C19 — resolved, at a different layer than predicted.** The
  unindented-array ambiguity is settled by `Structure` in `Documents::One` mode:
  a second top-level value is `TrailingContent`, so the walk knows exactly
  whether a file is one array or many records. The prefix heuristic remains for
  the instant answer the UI needs; the walk corrects it. Note the fix landed in
  the structural layer, not the lexer as C19 originally guessed — see the
  2026-07-28 note below.
- **2026-07-28 — C19 — superseded by the entry above.** The unindented-array
  ambiguity was to be resolved by "the lexer knows when it has closed a
  top-level value". The lexer deliberately does *not* know that — tracking open
  containers is structural state, and it has none (C24). The fix moves to the
  indexer, which is the layer that counts depth. The named test in
  `leviathan_core::format` stands until then.
- **2026-07-28 — C3 — first supporting number.** Newline scanning over 500 MB
  runs at 961 MB/s with 4.3 MB peak RSS, so the claim that NDJSON tier-1
  indexing is "memory-bandwidth-bound and embarrassingly fast" now has a
  measured ceiling behind it rather than an assertion.
