//! # leviathan-core
//!
//! A streaming JSON indexing core for files that are too large to parse into memory.
//!
//! ## Design in one paragraph
//!
//! `leviathan-core` never opens a file, never awaits, and never allocates a
//! representation of your document. Callers *push* bytes in through
//! [`Indexer::feed`] and *pull* byte ranges back out through the [`ByteRange`]
//! trait. What the core keeps is an index — where each node starts, what kind it
//! is, who its parent is — which is a navigation aid, not a replica. Values and
//! key names are re-derived by re-reading a few kilobytes at the moment they are
//! needed. That is what lets a 500 MB document be browsed in bounded memory.
//!
//! This "sans-IO" shape is deliberate: the same code runs unchanged in a Web
//! Worker (byte ranges backed by `Blob.slice`), in a native CLI (backed by
//! `pread`), or in a server process. See `DEEP_REASONING.md` C2.
//!
//! ## Status
//!
//! **M0 — skeleton.** This release establishes the crate, its public shape and
//! its boundary contract. [`sniff_format`] is real but heuristic; the streaming
//! lexer and node index land in M1. The API below is not yet stable.
//!
//! ## Example
//!
//! ```
//! use leviathan_core::{sniff_format, Format};
//!
//! assert_eq!(sniff_format(br#"{"a":1}"#), Format::SingleDocument);
//! assert_eq!(sniff_format(b"{\"a\":1}\n{\"a\":2}\n"), Format::Ndjson);
//! assert_eq!(sniff_format(b"not json"), Format::Unknown);
//! ```

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

mod format;
mod index;
mod lexer;
mod rows;
mod source;
mod structure;

pub use format::{Format, sniff_format};
pub use index::{ChildTable, RecordScanner, RootCollector, Tier1};
pub use lexer::{LexError, LexErrorKind, Lexer, Position, Token, TokenKind, Tokens};
pub use rows::{Count, Row, RowOptions, ValueKind, materialize};
pub use source::{ByteRange, SourceError};
pub use structure::{
    ContainerKind, DEFAULT_MAX_DEPTH, Documents, Event, StructError, StructErrorKind, Structure,
};

/// The version of this crate, as reported across the WASM boundary and by the CLI.
///
/// Used by the extension to assert that the bundled `.wasm` matches the
/// TypeScript protocol version it was built against.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Identity function used to prove the JS ↔ Worker ↔ WASM boundary is live.
///
/// This is the M0 exit criterion: a value that leaves the UI thread, crosses
/// into the worker, crosses the WASM boundary, and comes back unchanged. It is
/// retained deliberately as a permanent boundary smoke test — the extension
/// calls it on startup, so a broken build fails visibly instead of subtly.
#[must_use]
pub fn echo(value: u32) -> u32 {
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn echo_is_identity() {
        assert_eq!(echo(0), 0);
        assert_eq!(echo(u32::MAX), u32::MAX);
    }

    #[test]
    fn version_is_populated() {
        assert!(!VERSION.is_empty());
    }
}
