//! WebAssembly bindings for [`leviathan_core`].
//!
//! **This crate contains no logic and must stay that way.** Every function here
//! is a one-line translation between JS types and core types. Logic that lands
//! here is logic the native CLI and the future MCP server cannot use, and logic
//! that can only be tested in a browser. If a binding needs a body longer than a
//! few lines, the body belongs in `leviathan-core`.
//!
//! See `docs/adr/ADR-002` for how larger payloads (node slices) cross this
//! boundary without allocating per-row objects — from M1 onward, index data is
//! read by JS directly out of WASM linear memory rather than returned by value.

use wasm_bindgen::prelude::*;

/// Version of the `leviathan-core` engine compiled into this module.
///
/// The extension asserts this against its own expected version at startup, so a
/// stale `.wasm` in `dist/` fails loudly instead of behaving strangely.
#[wasm_bindgen(js_name = coreVersion)]
#[must_use]
pub fn core_version() -> String {
    leviathan_core::VERSION.to_string()
}

/// Boundary smoke test: returns its argument unchanged.
///
/// The M0 exit criterion, and kept permanently as a startup self-check.
#[wasm_bindgen]
#[must_use]
pub fn echo(value: u32) -> u32 {
    leviathan_core::echo(value)
}

/// Detect whether a prefix of the input is a single JSON document or NDJSON.
///
/// Returns one of `"single-document"`, `"ndjson"`, `"empty"`, `"unknown"`.
///
/// Takes a prefix — 64 KiB is ample. `&[u8]` is copied into WASM memory by
/// `wasm-bindgen`, which is exactly why this takes a prefix and not a file.
#[wasm_bindgen(js_name = sniffFormat)]
#[must_use]
pub fn sniff_format(prefix: &[u8]) -> String {
    leviathan_core::sniff_format(prefix).as_str().to_string()
}
