# ADR-001 — Parser strategy

**Status:** Accepted · closed at M1
**Date:** 2026-07-28 (measurements added 2026-08-02)
**Supersedes:** none

## Context

Leviathan opens JSON files that are too large to parse. That is the entire
product, so the parser is not an implementation detail to be swapped later — its
shape decides whether the memory model works at all.

Three constraints, and any one of them eliminates most of the field:

1. **Peak memory must be bounded, not proportional to file size.** A 500 MB file
   must not require 500 MB of anything. This rules out every parser that takes
   the document as a slice.
2. **The core must not own its input.** It has to run in a Web Worker reading a
   `Blob`, in a native CLI reading a file, and eventually in a server process —
   without a line of difference. See [ADR-005](ADR-005-file-access-model.md).
3. **The `.wasm` ships in an extension.** Binary size is user-visible, and the
   budget is 150 KB gzipped for all JS and CSS with the `.wasm` on top.

A fourth requirement is less obvious and turned out to matter more than any of
them: the parser must **survive a chunk boundary landing anywhere**, including
in the middle of a `\uD83D` escape, because bytes arrive in whatever sizes the
host chooses.

## Decision

**A hand-written resumable streaming lexer, with zero dependencies.**

The lexer is a state machine over bytes. It is fed slices, emits tokens, and
carries between calls only two things: where the current token began (`u64`) and
where in the grammar it is (an enum). Grammar checking is a separate layer
(`structure.rs`) that consumes tokens and emits structural events, so tokenizing
and validating are independently usable and independently measurable.

Three properties fell out of the design and each one earned its place:

- **A token is a span, not content** (`start`, `end`, kind — no bytes). Nothing
  is copied, so a 50 MB string value costs exactly what a 3-byte one does. The
  carry-over buffer that a naive resumable lexer needs does not exist, and with
  it an entire class of "pathological input exhausts memory" bug. Measured: a
  100 kB string fed in 64-byte chunks produces one token spanning `0..100_002`.
- **Errors are sticky.** Once failed, the lexer yields nothing further and
  replays the same error. Recovery is the caller's business, because "where is
  it safe to resume" is a question about records or trees, and the lexer
  deliberately knows about neither. `Lexer::resuming_at(offset, line)` produces a
  fresh lexer whose spans are still absolute.
- **Line and column are free.** JSON forbids unescaped control characters inside
  strings, so every newline in a well-formed document is whitespace *between*
  tokens. Only the whitespace skipper needs to count them — the least-travelled
  loop in the machine. String bodies, where most bytes live, never test for a
  newline at all.

## Alternatives considered

### `serde_json`

The obvious choice, and wrong here for a structural reason rather than a
performance one: it builds values. `from_reader` streams input but still
materializes a `Value` graph, which is the exact failure mode being escaped —
`JSON.parse`'s problem reimplemented in Rust. Its `StreamDeserializer` avoids
that for NDJSON but not for a single large document, and it wants to own its
reader, which collides with constraint 2.

Rejected. It would also have been the only dependency in the core, and the
layering check that keeps the crate portable exists precisely to prevent that
kind of creep.

### `simd-json`

Genuinely fast — several GB/s on the right hardware — and unusable here. It
requires the **whole document in mutable memory** and rewrites it in place, so a
500 MB file needs 500 MB of buffer before it starts. That is the constraint the
product exists to avoid. Its wasm32 SIMD story also depends on `simd128`, which
would be a feature flag and a second code path to test.

Rejected on constraint 1, before performance was even considered.

### A pull parser / event API from an existing crate

Closer to the right shape, but every candidate either owns its reader or
allocates per token. Adopting one would have traded a week of writing a state
machine for a permanent constraint on where the core can run, and for a
dependency whose `.wasm` contribution nobody was measuring.

### Byte-offset indexing without a lexer at all

Considered seriously for NDJSON, where record boundaries are newlines and a scan
finds them at memory bandwidth. It is in fact what tier 1 does — see
[ADR-004](ADR-004-index-representation.md) — but it cannot answer "what kind of
value is this" or "where does this string end", which every painted row needs.
A lexer is required; the insight was that it is not required *first*.

## Consequences

**Measured on the 500 MB NDJSON fixture** (8 × x86_64, Windows, `bench-native`):

| | Wall | Rate | Peak RSS |
|---|---:|---:|---:|
| `lex` — tokenize | 1.74 s | 288 MB/s · 62 M tokens/s | 4.4 MB |
| `walk` — tokenize + check grammar | 2.32 s | 216 MB/s | 4.4 MB |
| `scan` — count newlines (ceiling) | 408 ms | 1.2 GB/s | 4.3 MB |

Against an exit criterion of ≥ 200 MB/s native, with **4.4 MB peak RSS while
streaming 500 MB** — unchanged from the baseline process before the lexer
existed. Lexing half a gigabyte added nothing measurable to memory.

**The unit is tokens, not bytes.** The same lexer on three fixtures: 288 MB/s on
records, 232 MB/s on a flat scalar array, 96 MB/s on 100 000-deep nesting — a
3.4× spread. Tokens/s barely moves (47–72 M/s). The `deep` fixture is ~1.3 bytes
per token, so it looks catastrophically slow in MB/s while being the *fastest*
run by work done. Consequence: a MB/s figure is only meaningful for a named
fixture, and the harness prints both.

**The regression signal is a count, not a time.** The 500 MB fixture lexes to
exactly **108,133,846 tokens** on every run, at every chunk size, whether fed
1 byte or 1 MiB at a time. Wall time varies ±15 % from scheduling and up to 3×
from page-cache state (`DEEP_REASONING.md` C49); the count does not vary at all.
CI gates on the count.

**Costs accepted:**

- Writing and fuzzing a JSON state machine by hand, including UTF-8 validation,
  surrogate pairs, and RFC 8259 numbers. Paid once, in one file. Of the core's
  177 tests, the lexer's are the largest single group.
- Every consumer must call `Lexer::finish()`. A number is the only JSON token
  that cannot be emitted until the byte after it arrives, so a consumer that
  stops feeding without flushing silently drops a final value — invisible on
  every fixture ending in `}` or `]`, catastrophic on `[1,2,3,4`. This has now
  been the same bug three times (C30, C37), which is why a truncated-input test
  is the price of admission for a new lexer consumer.

**Conformance.** A committed corpus of ~110 cases in the JSONTestSuite naming
convention (`y_` must accept, `n_` must reject, `i_` implementation-defined)
runs under `cargo test` with no network or submodule, **each case at three chunk
sizes** — one byte, three bytes, whole — because a parser that is correct only
when the input arrives in one piece is not a streaming parser. `leviathan
conformance <dir>` runs the same predicate over the full external corpus.

The `i_` answers are decisions, recorded so they cannot drift silently:

| Case | Answer | Why |
|---|---|---|
| Huge exponents, 30-digit integers | **accept** | A token is a span; the lexer never computes a value, so range is not its business |
| Lone or reversed surrogates | **accept** | Syntactically valid `\uXXXX`; U+FFFD is substituted at display time rather than refusing to open the file |
| UTF-8 BOM | **accept** (skipped) | Strictly a JSON text should not carry one, but real Windows exports do, and refusing them fails the user this product exists for |
| Empty / whitespace-only input | **accept** | Nothing to show is not a parse error; the sniffer reports `empty` |
| Invalid UTF-8 anywhere, including outside strings | **reject** | Row text is handed to JavaScript, and a `String` that is not UTF-8 has nowhere to go |

**Robustness.** The property worth fuzzing for is not "does it crash" — the core
is `#![forbid(unsafe_code)]` and a panic was never the likely failure. It is
**chunk invariance**: a resumable lexer can silently give different answers
depending on where the boundary falls. `leviathan fuzz` therefore runs every
input at three chunk sizes and requires identical verdicts, while asserting that
token spans are ordered and bounded and that error positions are inside the
input with 1-based lines and columns. Three quarters of the corpus is *mutated
valid JSON* rather than noise, because random bytes are rejected within a few
bytes and reach nothing; ~10 % of mutations still parse, and a test asserts that
ratio stays non-zero. Runs are reproducible from their seed, and a 400 ms run is
part of the ordinary test suite.

Measured: **1,969,106,501 cases in 30 minutes** (1.09 M/s), 200,191,239 of them
accepted, no panic and no chunk-size disagreement.

**Deliberately not done:** SIMD. The R2 fallback ladder (bigger chunks, then
`simd128` behind a feature) exists and has not been needed — the engine reaches
~30 % of the trivial-work ceiling, and no measurement yet says the compare loop
is what limits anything.
