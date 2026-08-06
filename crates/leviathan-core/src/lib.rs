//! # leviathan-core
//!
//! A streaming JSON indexing core for files that are too large to parse into memory.
//!
//! ## Design in one paragraph
//!
//! `leviathan-core` never opens a file, never awaits, and never allocates a
//! representation of your document. Callers *pull* byte ranges through the
//! [`ByteRange`] trait, and the core asks for the ranges it needs. What it keeps
//! is an index — where each node starts and what kind it is — which is a
//! navigation aid, not a replica. Values and key names are re-derived by
//! re-reading a few kilobytes at the moment they are needed. That is what lets a
//! 500 MB document be browsed in bounded memory.
//!
//! This "sans-IO" shape is deliberate: the same code runs unchanged in a Web
//! Worker (byte ranges backed by `Blob.slice`), in a native CLI (backed by
//! `pread`), or in a server process. See `DEEP_REASONING.md` C2.
//!
//! ## The shape of the API
//!
//! Indexing is two tiers, both of them incremental, and both stop and resume so
//! a host can paint a frame or cancel between batches:
//!
//! | | | |
//! |---|---|---|
//! | **Tier 1** | [`Build`] | the root's children, over the whole source |
//! | **Tier 2** | [`Expansion`] | one container's children, on demand |
//! | **Rows** | [`materialize`] | a run of children, re-read into paintable [`Row`]s |
//!
//! Every node is addressed by its byte offset, which is stable for the life of
//! the source. Nothing the core hands out is invalidated by anything the core
//! later discards.
//!
//! ## Status
//!
//! **M1.** The lexer, the grammar walk, both index tiers and row materialization
//! are implemented and measured; query, validation, dedup and export are not
//! written yet. The API is not stable before 1.0.
//!
//! ## Example
//!
//! ```
//! use leviathan_core::{Build, BuildOptions, Format, RowOptions, materialize, sniff_format};
//!
//! let source: &[u8] = br#"{"name":"leviathan","tags":[1,2,3]}"#;
//!
//! // 1. What kind of input is this?
//! let format = sniff_format(source);
//! assert_eq!(format, Format::SingleDocument);
//!
//! // 2. Index the root. `source` is a `ByteRange`; a file or a `Blob` works
//! //    the same way, and `advance` would let you stop between batches.
//! let mut build = Build::new(format);
//! let mut bytes = source;
//! build.run_to_end(&mut bytes, &BuildOptions::default())?;
//! assert_eq!(build.rows(), 2);
//!
//! // 3. Paint some rows, re-reading only the bytes they need.
//! let mut bytes = source;
//! let rows = materialize(build.table(), 0, 10, &mut bytes, &RowOptions::default())?;
//! assert_eq!(rows[0].key.as_deref(), Some("name"));
//! assert_eq!(rows[0].preview, "leviathan");
//! assert_eq!(rows[1].children.value(), 3);
//! # Ok::<(), leviathan_core::SourceError>(())
//! ```

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

mod build;
mod dedup;
mod expand;
mod export;
mod find;
mod format;
mod index;
mod lexer;
mod query;
mod rows;
mod schema;
mod source;
mod structure;
mod validate;

pub use build::{Build, BuildOptions, Built};
pub use dedup::{Dedup, DedupOptions, Duplicate, DuplicateKind};
pub use expand::{DEFAULT_EXPANSION_BUDGET, ExpandOptions, Expansion, ExpansionCache, Stopped};
pub use export::{Export, ExportFormat};
pub use find::{Find, FindOptions, FindStop, rows_of};
pub use format::{Format, sniff_format};
pub use index::{ChildTable, RecordScanner, RootCollector, Tier1};
pub use lexer::{LexError, LexErrorKind, Lexer, Position, Token, TokenKind, Tokens};
pub use query::{Filter, FilterError};
pub use rows::{Count, Row, RowOptions, ValueKind, materialize};
pub use schema::{Schema, SchemaError};
pub use source::{ByteRange, SourceError};
pub use structure::{
    ContainerKind, DEFAULT_MAX_DEPTH, Documents, Event, StructError, StructErrorKind, Structure,
};
pub use validate::{Invalid, Validate, ValidateOptions};

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
