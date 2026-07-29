//! A [`ByteRange`] backed by a file on disk.
//!
//! `leviathan-core` never opens a file (C2), so somebody has to, and on the
//! native side it is this. The Worker's equivalent is thirty lines of TypeScript
//! over `Blob.slice()`; the eventual MCP server's is a `pread`. None of them
//! requires the core to change, which is the whole point of the trait.
//!
//! This type is also the sans-IO claim's only real test. Until something outside
//! the crate implemented `ByteRange` against a genuinely different source, "the
//! core is reusable" was a paragraph in a README.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use leviathan_core::{ByteRange, SourceError};

/// Random-access reads from a file, into a reused scratch buffer.
pub struct FileSource {
    file: File,
    /// Reused across reads: the trait hands back a borrow precisely so that
    /// implementations need not allocate per call, and a scrolling UI calls this
    /// once per frame.
    scratch: Vec<u8>,
    len: u64,
}

impl FileSource {
    /// Open `path` for random access.
    ///
    /// # Errors
    ///
    /// The file cannot be opened or its length cannot be determined.
    pub fn open(path: &Path) -> std::io::Result<Self> {
        let file = File::open(path)?;
        let len = file.metadata()?.len();
        Ok(Self {
            file,
            scratch: Vec::new(),
            len,
        })
    }
}

impl ByteRange for FileSource {
    fn read(&mut self, start: u64, len: u32) -> Result<&[u8], SourceError> {
        let end = start.saturating_add(u64::from(len));
        if end > self.len {
            return Err(SourceError::OutOfRange {
                start,
                len,
                available: self.len,
            });
        }

        self.scratch.resize(len as usize, 0);
        self.file
            .seek(SeekFrom::Start(start))
            .map_err(|e| SourceError::Unavailable(e.to_string()))?;
        self.file
            .read_exact(&mut self.scratch)
            .map_err(|e| SourceError::Unavailable(e.to_string()))?;

        Ok(&self.scratch)
    }

    fn len_hint(&self) -> Option<u64> {
        Some(self.len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn fixture(bytes: &[u8]) -> (tempdir::Dir, std::path::PathBuf) {
        let dir = tempdir::Dir::new();
        let path = dir.path().join("source.json");
        File::create(&path).unwrap().write_all(bytes).unwrap();
        (dir, path)
    }

    #[test]
    fn reads_arbitrary_ranges() {
        let (_dir, path) = fixture(b"0123456789");
        let mut source = FileSource::open(&path).unwrap();

        assert_eq!(source.read(0, 4).unwrap(), b"0123");
        assert_eq!(source.read(6, 4).unwrap(), b"6789");
        // Backwards, repeatedly — a scrolling UI does not read in order.
        assert_eq!(source.read(2, 3).unwrap(), b"234");
        assert_eq!(source.len_hint(), Some(10));
    }

    #[test]
    fn a_range_past_the_end_reports_the_size() {
        // Row windows speculatively ask past the end of the file, and rely on
        // the error saying how much there actually was.
        let (_dir, path) = fixture(b"0123456789");
        let mut source = FileSource::open(&path).unwrap();

        assert_eq!(
            source.read(8, 100),
            Err(SourceError::OutOfRange {
                start: 8,
                len: 100,
                available: 10
            })
        );
    }

    #[test]
    fn an_empty_read_is_allowed() {
        let (_dir, path) = fixture(b"abc");
        let mut source = FileSource::open(&path).unwrap();
        assert_eq!(source.read(3, 0).unwrap(), b"");
    }

    #[test]
    fn the_core_materializes_rows_through_it() {
        // The layering claim, exercised end to end: an index built by the core,
        // rows materialized by the core, bytes supplied by a type the core has
        // never heard of.
        let (_dir, path) = fixture(b"{\"a\":1}\n{\"b\":[1,2,3]}\n");
        let mut scanner = leviathan_core::RecordScanner::new();
        scanner.feed(&std::fs::read(&path).unwrap());
        let table = scanner.finish();

        let mut source = FileSource::open(&path).unwrap();
        let rows = leviathan_core::materialize(
            &table,
            0,
            10,
            &mut source,
            &leviathan_core::RowOptions::default(),
        )
        .unwrap();

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].children, leviathan_core::Count::Exact(1));
        assert_eq!(rows[1].children, leviathan_core::Count::Exact(1));
    }

    /// A minimal scratch directory, so the tests need no `tempfile` dependency.
    mod tempdir {
        use std::path::{Path, PathBuf};
        use std::sync::atomic::{AtomicU32, Ordering};

        static COUNTER: AtomicU32 = AtomicU32::new(0);

        pub struct Dir(PathBuf);

        impl Dir {
            pub fn new() -> Self {
                let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
                let path = std::env::temp_dir().join(format!(
                    "leviathan-source-test-{}-{unique}",
                    std::process::id()
                ));
                std::fs::create_dir_all(&path).unwrap();
                Self(path)
            }

            pub fn path(&self) -> &Path {
                &self.0
            }
        }

        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }
}
