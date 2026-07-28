# leviathan-wasm

WebAssembly bindings for [`leviathan-core`](https://crates.io/crates/leviathan-core)
— streaming JSON indexing for files too large to parse.

Published to npm as `@shadkhan/leviathan-core`.

## This crate contains no logic

Every function here is a one-line translation between JS types and core types,
and that is a rule rather than a coincidence. Logic that lands in this crate is
logic the native CLI and the future MCP server cannot use, and logic that can
only be tested in a browser. If a binding needs a body longer than a few lines,
the body belongs in `leviathan-core`.

## Building

```sh
wasm-pack build --target web
```

The release profile is tuned for size (`opt-level = "s"`, LTO, `wasm-opt -Oz`):
this binary ships inside a browser extension, where every kilobyte is
user-visible. The M0 module is **15.6 KB** (7.2 KB gzipped).

## Usage

Works in a browser, in a Web Worker, and in Node — same entry point, different
argument:

```js
import init, { echo, sniffFormat, coreVersion } from '@shadkhan/leviathan-core';

// Browser: pass a URL and the module is streamed and compiled.
await init({ module_or_path: new URL('leviathan_wasm_bg.wasm', import.meta.url) });

// Node: pass the bytes.
// await init({ module_or_path: await readFile('leviathan_wasm_bg.wasm') });

console.log(coreVersion());                        // "0.1.0"
console.log(echo(42));                             // 42
sniffFormat(new TextEncoder().encode('{"a":1}'));  // "single-document"
```

`sniffFormat` returns one of `"single-document"`, `"ndjson"`, `"empty"`,
`"unknown"`. It takes a **prefix** — 64 KiB is ample. Bytes passed across this
boundary are copied into WASM memory, which is exactly why the API asks for a
prefix and never a file.

In a browser extension the `.wasm` must be a bundled asset: MV3 forbids remote
code, and the CSP needs `'wasm-unsafe-eval'` in `extension_pages`.

## Status

**M0 — skeleton.** The boundary is live and the surface is small: `echo`,
`coreVersion`, `sniffFormat`. Indexing and row access land in M1, at which point
index data stops being returned by value and is read directly out of WASM linear
memory instead.

## License

Licensed under either of [MIT](https://github.com/shadkhan/leviathan/blob/main/LICENSE-MIT)
or [Apache-2.0](https://github.com/shadkhan/leviathan/blob/main/LICENSE-APACHE), at your option.
