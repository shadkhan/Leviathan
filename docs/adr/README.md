# Architecture decision records

One decision each, written at the phase that closes it — not in advance, and not
as a summary afterwards. Each records what was actually chosen, what was
rejected and why, and what it cost.

| ADR | Decision | Status |
|---|---|---|
| [001](ADR-001-parser-strategy.md) | Parser strategy — hand-written resumable streaming lexer | Accepted · M1 |
| [002](ADR-002-wasm-boundary.md) | WASM boundary — packed binary row blocks, one transfer per screen | Accepted · M1 |
| [003](ADR-003-ui-rendering.md) | UI rendering — vanilla TS, hand-rolled recycling list, capped scroll canvas | Accepted · M2 |
| [004](ADR-004-index-representation.md) | Index representation — 8 bytes per child, byte-offset ids, two tiers | Accepted · M1 |
| [005](ADR-005-file-access-model.md) | File access — synchronous pull trait, `FileReaderSync` in a Worker | Accepted · M1 |

All five are written. ADR-003 was deliberately held back until M2 had been
measured in a browser, because an ADR claiming a rendering decision was correct
without the frame times is the kind of document this directory exists to avoid.
It now records two criteria that are **missed**, and the four rounds of
measurement it took to get there — including one optimisation that made things
worse and was reverted. That is a more useful decision record than a clean pass
would have been.

## How these relate to the other documents

| Document | Holds |
|---|---|
| [`README.md`](../../README.md) | Product scope, features, personas, non-goals, definition of done — the *what* |
| [`SPEC.md`](../../SPEC.md) | Phased build plan with exit criteria — the *when* |
| [`DEEP_REASONING.md`](../../DEEP_REASONING.md) | Running log of every core concept, dated, with what it rules out and how it was validated — the *why*, as it happened |
| `docs/adr/` | The subset of that reasoning that became an architectural commitment, written as one narrative per decision |

`DEEP_REASONING.md` is the primary source; these are the entries that grew into
decisions worth reading on their own. Where an ADR states a number, the entry it
came from is cited by its concept id (C1, C29, C43, …).
