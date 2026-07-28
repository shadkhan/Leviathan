# leviathan-core

Streaming JSON indexing for documents far larger than memory.

[![crates.io](https://img.shields.io/crates/v/leviathan-core.svg)](https://crates.io/crates/leviathan-core)
[![docs.rs](https://docs.rs/leviathan-core/badge.svg)](https://docs.rs/leviathan-core)

`leviathan-core` is the engine behind [Leviathan](https://github.com/shadkhan/leviathan),
a browser JSON viewer that opens files which freeze every other one. The engine
is published separately because nothing about it is browser-specific.

## What makes it different

Ordinary JSON libraries answer "give me this document as a value". At 500 MB
that answer *is* the problem — the parsed representation is several times larger
than the file, and building it is what freezes the tab.

`leviathan-core` answers a different question: **where is everything?** It builds
an index — for each node, where it starts, what kind it is, who its parent is —
and re-derives everything else (key names, string values, spans) by re-lexing a
few kilobytes at the moment you ask. The index is a navigation aid, not a
replica, and it is a small fraction of the document's size.

## Sans-IO

The crate never opens a file, never awaits, and does not know what a `Blob` is.
Bytes are pushed in; byte ranges are pulled back out through a trait you
implement:

```rust
use leviathan_core::ByteRange;

let mut source: &[u8] = br#"{"name":"leviathan"}"#;
assert_eq!(source.read(8, 11).unwrap(), br#""leviathan""#);
```

That is why the same code runs unchanged in a Web Worker (ranges backed by
`Blob.slice`), in a native CLI (backed by `pread`), and in a server process. It
is enforced, not merely intended: CI fails the build if this crate acquires a
dependency on `wasm-bindgen`, `js-sys`, `web-sys`, or an async runtime, or if it
performs I/O.

There are **no dependencies**.

## Status

**M0 — skeleton.** The crate, its public shape, and its boundary contract are
established. Format detection is real; the streaming lexer and node index land
in M1. The API is not yet stable — expect breaking changes before 1.0.

```rust
use leviathan_core::{sniff_format, Format};

assert_eq!(sniff_format(br#"{"a":1}"#), Format::SingleDocument);
assert_eq!(sniff_format(b"{\"a\":1}\n{\"a\":2}\n"), Format::Ndjson);

// A log file is not JSON, even though its first line starts with a digit.
assert_eq!(sniff_format(b"2026-07-27 INFO started"), Format::Unknown);
```

## Design notes

The reasoning behind each decision — why the index stores no strings, why
indexing is two-tier and lazy, why containers use a flat child table instead of
sibling links — is written down in
[`DEEP_REASONING.md`](https://github.com/shadkhan/leviathan/blob/main/DEEP_REASONING.md).

## License

Licensed under either of [MIT](https://github.com/shadkhan/leviathan/blob/main/LICENSE-MIT)
or [Apache-2.0](https://github.com/shadkhan/leviathan/blob/main/LICENSE-APACHE), at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this crate shall be dual licensed as above, without any
additional terms or conditions.
