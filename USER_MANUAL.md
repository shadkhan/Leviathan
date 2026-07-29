# Leviathan — User Manual

Installing, using, and testing Leviathan.

> **Which version is this manual for?** `0.1.0` — milestone **M0**.
>
> M0 builds the machinery, not the product. The Rust core, the WASM boundary,
> the Worker, and the extension shell all exist and work. The **streaming lexer,
> the node index, and the tree view do not exist yet**, so Leviathan cannot open
> a large JSON file today. What it *can* do is prove that every layer between
> your click and the Rust engine is connected — which is exactly what M0 set out
> to do, and what this manual covers.
>
> If you came here to open a 500 MB file, that lands in M2. Watch the
> [roadmap](README.md#roadmap).

---

## Contents

1. [Installing](#1-installing)
2. [Using the viewer](#2-using-the-viewer)
3. [Testing](#3-testing)
4. [Troubleshooting](#4-troubleshooting)
5. [Using the engine without the extension](#5-using-the-engine-without-the-extension)
6. [Uninstalling](#6-uninstalling)

---

## 1. Installing

Leviathan is not on the Chrome Web Store yet (that is M7). For now you build it
from source. It takes about two minutes.

### 1.1 Prerequisites

| Tool | Version | Install |
|---|---|---|
| Rust | stable (1.85+) | <https://rustup.rs> |
| wasm-pack | 0.13+ | `cargo install wasm-pack` |
| Node.js | 22+ | <https://nodejs.org> |
| pnpm | 10+ | `npm install -g pnpm` |

The `wasm32-unknown-unknown` target is installed automatically —
`rust-toolchain.toml` requests it, and `rustup` honours that on first build.

Verify:

```sh
rustc --version && wasm-pack --version && node --version && pnpm --version
```

### 1.2 Build

```sh
git clone https://github.com/shadkhan/leviathan
cd leviathan
pnpm install
pnpm build
```

`pnpm build` does two things: compiles the Rust core to WebAssembly via
`wasm-pack`, then bundles the TypeScript and copies everything into
`packages/extension/dist/`.

A successful build ends with a size report:

```
  background.js                      115 B       121 B gz
  leviathan_wasm_bg.wasm           15626 B      7247 B gz
  manifest.json                      569 B       340 B gz
  viewer.css                        4723 B      1606 B gz
  viewer.html                       2906 B      1242 B gz
  viewer.js                         3498 B      1698 B gz
  worker.js                         3603 B      1613 B gz

  JS+CSS budget: 5038 / 153600 B gz (3%)
```

If the budget line ever reads over 100 %, the build fails on purpose. See
[Troubleshooting](#4-troubleshooting).

### 1.3 Load into Chrome

1. Open `chrome://extensions`
2. Turn on **Developer mode** — the toggle at the top right
3. Click **Load unpacked**
4. Select the **`packages/extension/dist`** folder (not the repo root, not
   `packages/extension`)
5. Leviathan appears in the list. Pin it from the puzzle-piece menu if you like.

Works the same in **Microsoft Edge** at `edge://extensions`, and in Brave, Opera,
and Vivaldi. Firefox needs a port (different API namespace) — that is a
post-v1 item.

### 1.4 Development loop

```sh
pnpm dev
```

Rebuilds on every save, with inline sourcemaps and no minification. Chrome does
**not** hot-reload extensions, so after each rebuild:

- **Viewer or Worker change** → just reload the viewer tab (Ctrl/Cmd-R)
- **Manifest or background change** → click ↻ on the Leviathan card in
  `chrome://extensions`

Changing Rust code needs a full `pnpm build` — `pnpm dev` does not watch the
crates.

---

## 2. Using the viewer

Click the 🐋 toolbar icon. A new tab opens with the viewer.

At M0 the viewer is a **self-check page** rather than a tree. That is
deliberate: the milestone is the boundary, so the page shows the boundary. In M2
the tree replaces this and these panels move behind a debug flag.

### 2.1 The status light

Top right of the header:

| Light | Meaning |
|---|---|
| 🟡 pulsing — *Starting engine…* | WebAssembly is instantiating. Normally under 50 ms. |
| 🟢 *Engine ready* | The engine is live. The footer shows `engine 0.1.0 · protocol 1`. |
| 🔴 *Engine failed to start* | Something is wrong — see [Troubleshooting](#4-troubleshooting). |

### 2.2 Boundary check

Enter any number from 0 to 4,294,967,295 and click **Round-trip**.

The value travels from the page → the Worker → across the WebAssembly boundary
→ into Rust → and back. A green `42 — round-tripped intact` means every layer
is connected.

This is not a toy. It stays in the product permanently as a startup self-check,
because a stale or broken `.wasm` otherwise fails in ways that look like
unrelated bugs much later.

### 2.3 Format detection

Drop a file on the dashed area, click to pick one, or paste JSON into the
textarea. Leviathan reports what it thinks the file is:

| Result | Meaning |
|---|---|
| **Single JSON document** | One JSON value spanning the file |
| **NDJSON / JSON-lines** | One independent JSON value per line |
| **Empty** | No non-whitespace content |
| **Not JSON** | Nothing here starts a JSON value |

Two things worth noticing:

**Only the first 64 KiB is read.** Drop a 2 GB file and the result comes back
instantly, having touched 64 KiB. The byte count in the result line tells you
how much was read. This is the whole design in miniature — file size never
dictates memory use.

**Log files are correctly rejected.** `2026-07-27 INFO started` begins with a
digit, and a naive check would call it a JSON number. Leviathan reads whole
tokens: `2026` is a number, `2026-07-27` is not, and the `-` proves it.

### 2.4 What it cannot do yet

Not built: the tree view, expand/collapse, search, JSONPath queries, schema
validation, duplicate detection, and export. Dropping a large file will
correctly identify its format and then do nothing else with it. Milestones M1–M6.

---

## 3. Testing

### 3.1 Everything at once

```sh
pnpm check
```

Runs formatting, lints, Rust tests, and the TypeScript typecheck. Exit code 0
means all green.

### 3.2 The individual suites

| Command | What it proves | Expected |
|---|---|---|
| `cargo test --workspace` | Core logic — lexer, grammar, tier-1 index, row materialization, byte ranges, fixtures, bench harness | 186 passed |
| `cargo clippy --workspace --all-targets -- -D warnings` | No lint warnings | `Finished` |
| `cargo fmt --all --check` | Formatting | no output |
| `bash scripts/check-layering.sh` | Core has no wasm/IO dependency, still builds for `wasm32` | `layering contract holds.` |
| `pnpm -C packages/extension smoke` | The built `.wasm` instantiates; its `Format` strings match the Rust tests | `all good` |
| `pnpm typecheck` | Both TS projects typecheck | no output |
| `pnpm build` | Bundles and enforces the size budget | size table |

Test counts move as milestones land; `cargo test --workspace` reporting a number
*lower* than a previous run is the thing worth investigating.

The layering check is worth understanding, because it is what makes
"`leviathan-core` is reusable" a fact rather than a claim. It asserts three
things and fails the build on any of them:

```
layering: leviathan-core

  ok     no external dependencies
  ok     no I/O in crates/leviathan-core/src
  ok     compiles for wasm32-unknown-unknown

layering contract holds.
```

To satisfy yourself it actually works, break it on purpose:

```sh
echo 'fn _x() { let _ = std::fs::metadata("x"); }' >> crates/leviathan-core/src/lib.rs
bash scripts/check-layering.sh    # → FAIL, exit 1
git checkout crates/leviathan-core/src/lib.rs
```

### 3.3 Manual browser test

Some things only a browser can prove: the CSP, the manifest, the Worker
boundary, and that the `.wasm` loads from the bundle without a network fetch.
This is the **M0 exit criterion**, and it is checked by hand.

**Setup.** Build, load unpacked ([§1.3](#13-load-into-chrome)), open the viewer,
and open DevTools (F12) on the Console tab.

| # | Step | Pass |
|---|---|---|
| 1 | Open the viewer | Status goes 🟡 → 🟢 *Engine ready* within a second |
| 2 | Read the footer | `engine 0.1.0 · protocol 1` |
| 3 | Check the Console | **No** errors. Specifically no CSP violation and no `WebAssembly.instantiate` failure |
| 4 | Enter `42`, click **Round-trip** | Green `42 — round-tripped intact` |
| 5 | Enter `4294967295`, round-trip | Green, unchanged — the full u32 range survives |
| 6 | Paste `{"a":1}` into the textarea | `Single JSON document — from 7 bytes of pasted text` |
| 7 | Paste `{"a":1}` newline `{"a":2}` | `NDJSON / JSON-lines` |
| 8 | Paste `2026-07-27 INFO started` | Red `Not JSON — nothing here starts a JSON value` |
| 9 | Drop a `.json` file on the drop zone | Correct format, and a byte count of **65536 or less** regardless of file size |
| 10 | Open the **Network** tab, reload the viewer | Requests only to `chrome-extension://…`. **Nothing external.** |

Step 10 is the privacy claim, verified rather than asserted.

**Confirming the Worker is real.** In DevTools, open the ⋮ menu → **More tools**
→ **Task Manager**, or check the Sources tab — you should see a
`leviathan-engine` worker thread. If parsing were happening on the main thread,
there would not be one.

### 3.4 Test files

The fixture generator arrives with M1. Until then, these four cover the
interesting cases. They use `node` for everything, so they work identically in
PowerShell, cmd, and any Unix shell:

Write them into `fixtures/generated/`, which is gitignored — following this
manual should never dirty your working tree:

```sh
mkdir -p fixtures/generated

# A small single document
node -e "require('fs').writeFileSync('fixtures/generated/small.json', JSON.stringify({name:'leviathan',tags:['json','wasm'],size:500}))"

# 100k-line NDJSON, about 4 MB
node -e "for(let i=0;i<100000;i++)console.log(JSON.stringify({id:i,ok:i%3>0,msg:'line '+i}))" > fixtures/generated/big.ndjson

# A log file — should be detected as "Not JSON"
node -e "for(let i=0;i<1000;i++)console.log(`2026-07-27T09:${String(i%60).padStart(2,'0')}:00Z INFO event ${i}`)" > fixtures/generated/app.log

# A pretty-printed document — must NOT be mistaken for NDJSON
node -e "console.log(JSON.stringify({a:1,b:[2,3],c:{d:4}},null,2))" > fixtures/generated/pretty.json
```

Expected: `small.json` → single document · `big.ndjson` → NDJSON ·
`app.log` → not JSON · `pretty.json` → single document.

`pretty.json` is the one to care about. It is multi-line JSON, which a
newline-counting heuristic would call NDJSON — getting it right is the reason
detection looks at what starts each line at column 0 rather than counting `\n`.

### 3.5 Command-line testing

The core also runs natively, which is the fastest way to check engine behaviour
without a browser in the loop:

```sh
cargo run -q -p leviathan-cli -- version                              # → 0.1.0
cargo run -q -p leviathan-cli -- echo 42                              # → 42
cargo run -q -p leviathan-cli -- sniff fixtures/generated/big.ndjson  # → ndjson
cargo run -q -p leviathan-cli -- sniff fixtures/generated/app.log     # → unknown
cargo run -q -p leviathan-cli -- help                                 # all commands
```

It reads from stdin when no file is given, so it composes:

```sh
cat fixtures/generated/app.log | cargo run -q -p leviathan-cli -- sniff   # → unknown
```

Note that even the CLI reads at most 64 KiB — the rule that file size must never
drive memory use holds everywhere, not just in the browser.

That this binary compiles at all is itself a test: it links the same crate the
extension compiles to WASM, against real files, with no changes. If the core
ever grows a browser assumption, this stops building.

### 3.6 Generating fixtures

`leviathan fixtures` replaces the hand-rolled snippets in §3.4 with something
reproducible. The same `--seed` produces the same bytes on any machine, forever
— which is what makes a benchmark number something another person can check
rather than something they have to believe.

```sh
cargo run -q -p leviathan-cli -- fixtures list
cargo run -q -p leviathan-cli -- fixtures ndjson --size 500MB
cargo run -q -p leviathan-cli -- fixtures wide --count 5000000
cargo run -q -p leviathan-cli -- fixtures deep --depth 100000
```

Output lands in `fixtures/generated/` (gitignored) unless `--out` says otherwise.
A 500 MB NDJSON fixture takes under two seconds.

| Kind | What it is for |
|---|---|
| `ndjson` | The primary benchmark fixture: log/API records, one per line |
| `array` | The same records as one top-level array |
| `nested` | A single document that is a tree of nested objects |
| `deep` | Stack safety — nesting `--depth` levels deep |
| `wide` | Random access — a flat array of `--count` scalars |
| `bigstring` | One string value larger than any read buffer |
| `dupkeys` | Duplicate keys within objects (M5) |
| `badutf8` | Valid JSON structure containing invalid UTF-8 |
| `truncated` | Cut off mid-record, as a killed export would be |

The last six exist because a large JSON file is usually a *broken* large JSON
file, and those are the cases most likely to panic a parser.

### 3.7 Benchmarking

```sh
cargo run --profile bench-native -p leviathan-cli -- bench fixtures/generated/ndjson-500.0MB.ndjson
```

**Use `--profile bench-native`.** The default `release` profile is tuned for
WASM binary size (`opt-level = "s"`), which understates native throughput. The
harness prints the profile it was built with and labels a debug build *"do not
publish these numbers"*.

Three workloads run per fixture:

Six workloads run per fixture — two ceilings, then the engine layer by layer:

| Workload | Measures |
|---|---|
| `read` | Streaming a file in chunks. The I/O ceiling |
| `scan` | Counting newlines. The memory-bandwidth ceiling, and the operation NDJSON tier-1 indexing is built from |
| `sniff` | Format detection latency on a 64 KiB prefix |
| `lex` | Tokenizing the whole file |
| `walk` | Tokenizing *and* checking the grammar — i.e. full well-formedness validation |
| `index` | Building the tier-1 index. Reports the index's **size** as well as the time |
| `rows` | Fetching 50 rows from deep inside the index, re-reading the file for every field |

The gap between `lex` and `walk` is what enforcing JSON's grammar costs on top of
recognizing its tokens. The gap between `walk` and `index` is the point of the
whole product: on NDJSON, indexing is ~6× faster than parsing, because it scans
for newlines and never parses at all.

`rows` is the other half of that bargain. The index stores 8 bytes per node and
nothing else, so a row's key, kind, preview and child count are all rebuilt from
the file when it is painted. It reports two times — the **cold** fetch you feel
when you drag the scrollbar somewhere new, and the repeated **warm** mean that
continuous scrolling costs — plus how many of the slice's containers it managed
to count exactly within their budget.

Add `--json` for machine-readable output, `--workload <name>` to run one, and
`--chunk <size>` to vary the read size.

Three things in the output are worth understanding, because they all exist to
stop the harness flattering itself:

- **`n/a †` in the throughput column** is deliberate. `sniff` stops as soon as it
  has an answer, so it does not read the bytes it was given — a bytes/second
  figure would be division by work that never happened. Its wall time *is* the
  result.
- **`n/a ‡`** means the workload hit a syntax error and stopped. For the
  `truncated` and `badutf8` fixtures that is the *correct* outcome, not a failed
  run: the observed column names the byte, line and column where it stopped, and
  the size column shows how far it got.
- **`(mean of N)`** means the workload finished faster than the timer can
  resolve, so it was repeated and averaged. Timing a single sub-microsecond pass
  measures the clock, not the code.

`lex` reports both MB/s and tokens/s, and the second is the one to watch. A file
of nothing but `[` costs about one token per byte and a file of long strings
costs one per hundred, so MB/s is partly a statement about your fixture rather
than about the engine — the `deep` fixture reads as 96 MB/s while actually being
the *fastest* run by tokens/s.

Two things you should see on a 500 MB NDJSON fixture:

- **Peak RSS of a few megabytes**, unchanged whether the run is lexing, walking
  or just reading. That is the entire memory thesis, visible in one column. The
  `index` row is the exception and should be higher by roughly the index size,
  because that is the one workload that keeps something.
- **`index` finishing at close to the `scan` rate**, and `walk` at roughly a
  sixth of it. If `index` ever slows to `walk`'s rate, the newline-scan path has
  been lost and the file no longer opens before it is validated.

Try `bench` on the `truncated` and `badutf8` fixtures to see this from the other
side: `index` succeeds and reports its records, `rows` materializes 50 perfectly
good rows out of the middle, and only `walk` stops — at the exact byte where the
file went wrong. A broken file still opens, scrolls and displays. That is
deliberate, and it is the behaviour most other JSON viewers get wrong.

---

## 4. Troubleshooting

### The status light stays red

The message names the layer that failed. The common ones:

| Message | Cause | Fix |
|---|---|---|
| *Could not start the Leviathan engine. The bundled WebAssembly module failed to load.* | `.wasm` missing from `dist/`, or the CSP blocked it | `pnpm build`, then reload the extension |
| *Protocol mismatch: the UI speaks v1, the worker speaks v2* | `dist/` has bundles from two different builds | `pnpm build` |
| *Unknown method "…"* | Same cause — mismatched bundles | `pnpm build` |
| *The Leviathan worker crashed* | The Worker bundle failed to load or parse | Check the Console for the underlying error |

### Build failures

| Message | Fix |
|---|---|
| `src/wasm/leviathan_wasm_bg.wasm is missing` | Run `pnpm build:wasm` — the WASM package is generated, never committed |
| `Bundle budget exceeded: … > 153600 B gz` | Intentional. Cut something, or change the budget in `build.mjs` and justify it in an ADR |
| `error: linker … wasm32-unknown-unknown` | `rustup target add wasm32-unknown-unknown` |
| `Ignored build scripts: esbuild` | `pnpm approve-builds`, or check `onlyBuiltDependencies` in `pnpm-workspace.yaml` |

### Chrome shows "Manifest file is missing or unreadable"

You selected the wrong folder. It must be **`packages/extension/dist`** — the
built output, not the source.

### Changes are not showing up

Chrome caches extension pages aggressively.

1. Confirm the build actually ran (check the timestamp on `dist/viewer.js`)
2. Reload the viewer tab
3. If it is a manifest or background change, click ↻ on `chrome://extensions`
4. As a last resort, remove and re-load the unpacked extension

### `pnpm install` refuses to run esbuild's install script

pnpm 10 blocks install scripts by default. `pnpm-workspace.yaml` allowlists
`esbuild` explicitly. If it still complains, run `pnpm approve-builds`.

---

## 5. Using the engine without the extension

The engine is a standalone library. You do not need the browser extension —
or a browser — to use it.

### Node / JavaScript

```js
import init, { sniffFormat, coreVersion } from './src/wasm/leviathan_wasm.js';
import { readFile } from 'node:fs/promises';

await init({ module_or_path: await readFile('./leviathan_wasm_bg.wasm') });

console.log(coreVersion());                                    // "0.1.0"
sniffFormat(new TextEncoder().encode('{"a":1}\n{"a":2}\n'));   // "ndjson"
```

A complete working version is
[`packages/extension/scripts/smoke.mjs`](packages/extension/scripts/smoke.mjs).
Run it with `pnpm -C packages/extension smoke`.

### Rust

```toml
[dependencies]
leviathan-core = "0.1"
```

```rust
use leviathan_core::{sniff_format, ByteRange, Format};

assert_eq!(sniff_format(br#"{"a":1}"#), Format::SingleDocument);

// Byte ranges come from wherever you keep the bytes — a slice, a file, a socket.
let mut source: &[u8] = br#"{"name":"leviathan"}"#;
assert_eq!(source.read(8, 11).unwrap(), br#""leviathan""#);
```

Implement `ByteRange` over your own storage and the core works against it
unchanged. That is the whole point of the sans-IO design.

Both packages ship in M7 — until then, use the git dependency or a path.

---

## 6. Uninstalling

`chrome://extensions` → **Remove** on the Leviathan card.

Leviathan stores nothing: no `chrome.storage`, no cookies, no IndexedDB, no
cache. Removing the extension removes everything.

To remove the source too, delete the cloned folder. Build artefacts live in
`target/`, `node_modules/`, `packages/extension/dist/`, and
`packages/extension/src/wasm/` — all gitignored, all regenerable.

---

## Getting help

- **Bugs and questions:** <https://github.com/shadkhan/leviathan/issues>
- **Why is it built this way:** [`DEEP_REASONING.md`](DEEP_REASONING.md)
- **What is planned and when:** [`SPEC.md`](SPEC.md)

Licensed [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
