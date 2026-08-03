# ADR-003 — UI rendering

**Status:** Accepted · closed at M2
**Date:** 2026-08-04
**Supersedes:** none

## Context

The viewer must draw a tree over a file with 1.77 million rows — and, once a
container is expanded, potentially five million more — while the main thread
stays free enough to keep scrolling at 60 fps. Three constraints shape every
option:

1. **Row data is asynchronous.** Rows come from a Worker, one packed block per
   round trip ([ADR-002](ADR-002-wasm-boundary.md)). A renderer that assumes
   `items[index]` is available synchronously cannot be used unmodified.
2. **The bundle ships in an extension.** ≤ 150 KB gzipped for all JS and CSS,
   enforced by the build.
3. **Nothing may block.** The whole product is the promise that the tab stays
   responsive; a rendering strategy that stalls is the failure mode being sold
   against.

A fourth constraint is not obvious until the numbers are real: **browsers cap
how tall an element may be.** 1.77 M rows at 24 px is 42.5 M pixels. Chrome
stops near 33.5 M, Firefox near 17.9 M. The naive "spacer of `rows × height`"
does not merely get slow at the sizes this product exists for — it silently
stops working, and the last rows of the file become unreachable.

## Decision

**Vanilla TypeScript, a hand-rolled recycling virtual list, and three modules
that each know one thing.**

| Module | Knows | Knows nothing about |
|---|---|---|
| `tree.ts` | The *shape* — which containers are open, and therefore what row 4,812,907 is | Row content, the DOM |
| `store.ts` | The *content* — a bounded LRU of decoded blocks, and how to ask the Worker | Geometry, the DOM |
| `list.ts` | The *geometry* — how many rows exist, which are visible, which element to reuse | JSON, anything above it |

`main.ts` is the join and holds no state of its own beyond selection.

Three decisions inside that:

- **Fixed row height.** Variable heights require measuring or estimating every
  row, which reintroduces a per-row cost proportional to the file. Deferred
  permanently rather than to v1.1.
- **The scrollbar lies, deliberately.** The canvas is capped at 8 M px — well
  below every engine's limit rather than at any one of them — and the mapping
  from scroll position to first visible row is scaled. Below the cap the scale
  is exactly 1 and this is an ordinary virtual list. Above it, one pixel is
  worth several rows, which is the bargain a minimap or a hex editor makes.
  Keyboard navigation moves by *rows* regardless, so precision is never lost —
  only the pointer's grip on it.
- **Rows are positioned with `transform`, never `top`,** and recycled elements
  are parked off-screen rather than removed, because removing and re-appending
  is the expensive half of what a recycling list exists to avoid.

## Alternatives considered

### React

Rejected on behaviour before size. A virtual DOM earns its keep by knowing what
*didn't* change; during a scroll through 100 000 rows, every visible row's
content changes every frame, so reconciliation is pure overhead on top of the
DOM writes it still has to make. The measured win at M2 came from the opposite
direction — skipping DOM writes by comparing against a per-element cache — which
is the same idea implemented in fifteen lines with no reconciler.

### Preact

The interesting case, because at ~4 KB gzipped it fits the budget comfortably.
Rejected for the same reason as React: the component model's value is in the
diff, and the diff is worthless here. It would have bought a nicer syntax for
the panels — the toolbar, the find bar — at the cost of a rendering model that
does not match the one thing that has to be fast.

Revisit if the panel code ever demonstrably suffers. After M2 it has not: the
find bar and its filter toggle are a few dozen lines of direct DOM.

### An off-the-shelf virtual list

`react-window`, TanStack Virtual and similar are well built and solve a
different problem. Two blockers, either of which is fatal:

- They assume **synchronous** item access. Rows here arrive from a Worker, and a
  miss must paint a placeholder that is replaced when the block lands.
- None of them handles the **maximum element height**, which is the thing that
  actually breaks at 1.77 M rows. Working around it inside a library that owns
  the scroll container means fighting the library.

### `content-visibility: auto`

Genuinely useful for long documents and no help here: it skips *rendering* work
for off-screen elements but still requires an element per row. 1.77 M DOM nodes
is not survivable regardless of how cheaply each one paints.

### Canvas or WebGL rows

Fast, and it forfeits everything a developer tool needs: text selection, the
browser's own find, `role="treeitem"` and the accessibility tree, and per-row
CSS. A JSON viewer whose values cannot be selected is not usable for the job.

## Consequences

**Measured at M2** — 500 MB NDJSON, 1,772,686 rows, scrolling 100,000 rows:

| | Target | Measured |
|---|---|---:|
| First rows painted | < 2 s | **124–143 ms** ✅ |
| Frame time | — | median **16.6 ms**, p95 **16.9 ms** |
| Longest frame | < 32 ms | **35.5 ms** ❌ — 2 frames of 2,500 |
| Long tasks > 50 ms | 0 | **0** ✅ |
| Bundle, JS + CSS | ≤ 150 KB gz | **17.2 KB** — 11 % |

p95 equal to the median means 95 % of frames land at 60 fps. The criterion is
missed by two frames exceeding 32 ms by 3.5 ms, and that is published rather
than rounded away.

**The budget never bound the decision.** At 11 % it could have afforded Preact
three times over. The choice was made on rendering behaviour, and the budget
merely confirmed it — which is worth recording, because "we avoided a framework
to save bytes" would be a tidier story and a false one.

**Getting there took four measured rounds**, and the sequence is the useful part:

| | p95 | over 32 ms | long tasks |
|---|---:|---:|---:|
| as built | 23.3 ms | 17 | 0 |
| \+ memoized row decode, no `toLocaleString`, skip unchanged DOM writes | 17.7 ms | 10 | **3** |
| \+ bounded the block cache (`MAX_BLOCKS` 256 → 64) | **16.9 ms** | **2** | **0** |

Zero long tasks beside a 58.9 ms frame is what pointed at allocation rather than
at a slow function: nothing blocked the thread for 50 ms, so there was no hot
call to find. The middle row is the caution — caching decoded rows removed the
garbage but *retained* it, and that run was the first ever to record long tasks.
Trading many small collections for one big one is not an optimisation.

**The split paid off in a way that was not planned for.** Filtering search
results to matching records — which changes what the tree *contains* — turned
out to need no change to `Tree`, `RowStore` or `VirtualList` at all. It is one
function in the renderer translating a visible position to a record position,
because the tree works in visible positions and the store works in record
positions and nothing else ever needed to know the difference.

**Costs accepted:**

- No component model, so panels are imperative DOM. Fine at this size; the thing
  to watch is the validation and query panels at M3–M4.
- The scaled scrollbar is a real usability compromise above 330 000 rows: a
  pixel of drag covers several rows. Keyboard and find navigation are exact,
  which is why it is acceptable.
- A stylesheet and a renderer share `--row-h`, and the renderer reads it from
  the computed style so they cannot disagree. An earlier version had them
  disagree in a subtler way — the row's indent padding was cancelled by a
  negative margin on the gutter, so nesting rendered flat at every depth
  (`DEEP_REASONING.md` C45). CSS is the one layer here with no test, and it is
  where the only rendering bug of M2 lived.
