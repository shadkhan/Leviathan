# ADR-004 — Index representation

**Status:** Accepted · closed at M1
**Date:** 2026-07-29 (tier 2 and eviction added 2026-08-01)
**Supersedes:** none

## Context

Leviathan's premise is that a file can be browsed without being parsed into a
value. What stands in for the value is an index, and the index's size *is* the
memory model: wasm32 has a 4 GB address space and realistically a ~2 GB usable
heap, so an index that scales badly reintroduces the failure the product exists
to remove — just later, and more confusingly.

The original budget was **≤ 16 bytes per node**, with a pre-declared fallback
(index containers only) if it proved unreachable.

Two questions had to be answered together:

1. What does a node record hold?
2. How does the UI address a node — and does that address survive the index
   discarding things?

## Decision

### The index stores offsets and nothing else

A `ChildTable` is a flat `Vec<u64>`: the byte offsets of one container's direct
children, in document order. No kind, no length, no child count, no key text, no
end offset, no parent pointer, no depth.

Everything else is **re-derived by re-lexing a few kilobytes** at the moment a
row is painted. That is the trade the whole design rests on, and the arithmetic
is one-sided: storing kind, count and span would cost 24 B/node — 42 MB on the
500 MB fixture against a 40 MB criterion — while offsets alone cost 8 B/node and
14.2 MB, and pay for it with microseconds per painted row.

### A node id is a byte offset

Not an index into anything. This is what makes tier 2 disposable: a cache that
may evict at any moment cannot hand out ids that point into itself. The
alternative — a dense `u32` row number — is half the width and needs generation
counters, an invalidation message, and a rule for what the UI does when its
selection evaporates: a protocol spanning three layers to service an
implementation detail of a cache.

A byte offset is derived from the *file*, so eviction loses **work** and never
**addressability**. Re-expanding the same offset produces a byte-identical
table, which is asserted directly because the entire argument rests on it.

### A child offset points at the row, not the value

For an object member, the offset is where the **key** starts. Lexing forward
from a key yields key, colon and value in one pass; recovering a key from a
value offset would require lexing backwards, which a streaming lexer cannot do
at any price. For arrays the two coincide.

### Two tiers, one structure

- **Tier 1** (eager, always resident): the root's direct children. For NDJSON,
  found by scanning for newlines — exact, not heuristic, because JSON forbids
  raw control characters in strings so a newline can never occur inside a value.
  For a single document, found by walking the grammar.
- **Tier 2** (lazy, LRU, 256 MB budget): one container's children, built on
  expand, in yielding batches of 10,000.

Both produce the same `ChildTable`. That answers the question the spec left open
— NDJSON and single-document tiers do **not** diverge. An NDJSON stream is a
root whose children are records; a document is a root whose children are its
members. One addressing model, two builders.

### Byte → row is a binary search

`ChildTable::locate(offset)` returns the child containing a byte: 21 comparisons
over 1.77 M records. It is the join every feature needs — a find match, a
validation error, a jump-to-offset all arrive as a byte and must become a row. A
byte *before* the first child belongs to no child and returns `None`, because a
document's opening `[` genuinely precedes every row and answering "row 0" would
send the user where the byte is not.

## Alternatives considered

### Struct-of-arrays with packed 16-byte records

The original plan: `start: u48 + kind: u4 + flags: u4` packed into a `u64`, plus
`parent: u32` and `child_count: u32`. Rejected once the re-lex cost was
measured. Every field beyond the offset is recomputable in microseconds and
permanent in megabytes, and the fields most wanted (kind, count) are exactly the
ones a 4 KB re-scan produces for free while it is already reading the row.

### `first_child` / `next_sibling` linked traversal

The textbook tree index, and catastrophic for the one case that matters. Virtual
scrolling asks for rows 900,000–900,050 of a 5 M-element array; a linked
structure answers in 900,000 pointer hops. A flat child table answers in one
lookup. The 8 bytes per node this costs is the price of the product's premise.

### Eager full indexing

Indexing every node of a 500 MB file up front is wasted work — the user will
look at a few hundred rows. Lazy tier 2 is the difference between "500 MB works"
and "500 MB works and 5 GB probably also works", and it collapses first-paint
latency from "index the whole file" to "scan for newlines".

### A generation-counter invalidation protocol

See "a node id is a byte offset" above. Rejected as a three-layer protocol
existing to serve a cache.

## Consequences

**Measured, 500 MB NDJSON:**

| | |
|---|---:|
| Tier-1 index size | **14.2 MB** (2.8 % of file) |
| Tier-1 build | 345 ms warm · 1.07 s cold, at the newline-scan ceiling |
| Random 50-row slice, row #1,595,372 | **68 µs** warm |
| Peak RSS while indexing | **22 MB** |

Against criteria of < 40 MB index, < 400 MB peak, < 20 ms random access. The
per-node width has margin to spare and is not worth revisiting.

**The result is shape-dependent, and one shape misses.** A flat array of 5 M
small scalars costs ~10 bytes per element in the file and 8 in the index —
**40.0 MB of index for a 49.4 MB file, 80.9 %**. Extrapolated, a 500 MB file of
that shape needs ~400 MB of index and misses the criterion by an order of
magnitude. Two mitigations are identified and neither is built: delta+varint
offsets (4–8× smaller, O(1) access becomes O(1)-per-block), or sparse indexing
(store every 64th offset, re-lex within the bucket — the same trade as the index
itself, applied to the index).

Stated plainly: **the criterion is met for record-shaped data and not for
scalar-dense arrays.** The pre-declared risk anticipated the wrong variable — it
assumed per-node *width* and proposed indexing containers only; the width is
8 B and the problem is per-node *count*, which that fallback does not address.

**A sealed table is 40 % smaller than a grown one.** A `Vec` grown to 5 M
entries holds capacity for 8.4 M — 67.1 MB of allocation for 40.0 MB of
contents, charged against the cache budget for the rest of the session and never
usable, because the container is closed. A `shrink_to_fit` at a terminal stop
makes resident cost equal contents exactly. Only at a *terminal* stop: a
batch-limited stop keeps its capacity, because a table about to grow again would
only re-allocate.

**Child counts are bounded, not exact.** Painting a row means asking "how many
children", and for a 400 MB container that is seconds — one row freezing the
viewer. The walk stops after 8 KiB and reports `AtLeast(n)`; the UI shows
`1,000+ items`. On the 500 MB fixture, 50 of 50 containers are counted exactly,
because real records fit in 8 KiB.
