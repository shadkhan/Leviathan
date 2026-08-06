//! Open a file far larger than memory and print a window of rows from it.
//!
//! ```sh
//! cargo run --example browse -- fixtures/generated/ndjson-500.0MB.ndjson 1595372
//! ```
//!
//! This is the whole product in forty lines: index the file, then paint fifty
//! rows from the middle of it. Nothing here holds the document — the index says
//! where each record starts, and the rows are re-lexed out of the file at the
//! moment they are wanted (`DEEP_REASONING.md` C1).
//!
//! The `ByteRange` implementation below is the only I/O in this program, and it
//! is the reason the core needs none of its own: the same trait is implemented
//! by a `&[u8]` in tests and by a browser `Blob` in the extension.

use std::env;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};

use leviathan_core::{
    Build, BuildOptions, ByteRange, RowOptions, SourceError, materialize, sniff_format,
};

/// A file, as a source of byte ranges.
struct FileSource {
    file: File,
    len: u64,
    scratch: Vec<u8>,
}

impl ByteRange for FileSource {
    fn read(&mut self, start: u64, len: u32) -> Result<&[u8], SourceError> {
        if start > self.len {
            return Err(SourceError::OutOfRange {
                start,
                len,
                available: self.len,
            });
        }
        let want = (len as u64).min(self.len - start) as usize;
        self.scratch.resize(want, 0);
        self.file
            .seek(SeekFrom::Start(start))
            .and_then(|_| self.file.read_exact(&mut self.scratch))
            .map_err(|e| SourceError::Unavailable(e.to_string()))?;
        Ok(&self.scratch)
    }

    fn len_hint(&self) -> Option<u64> {
        Some(self.len)
    }
}

fn main() -> io::Result<()> {
    let mut args = env::args().skip(1);
    let path = args.next().unwrap_or_else(|| {
        eprintln!("usage: browse <FILE> [ROW]");
        std::process::exit(2);
    });
    let from: usize = args.next().and_then(|n| n.parse().ok()).unwrap_or(0);

    let file = File::open(&path)?;
    let len = file.metadata()?.len();
    let mut source = FileSource {
        file,
        len,
        scratch: Vec::new(),
    };

    // One 64 KiB read is enough to say what kind of file this is, which is why
    // a viewer can name it before it has finished indexing it.
    let format = sniff_format(source.read(0, 64 * 1024).map_err(to_io)?);
    println!("{path}: {len} bytes, {}", format.as_str());

    // Indexing runs in batches so a host can paint or cancel between them. Here
    // there is nothing to paint, so it runs to the end.
    let mut build = Build::new(format);
    build
        .run_to_end(&mut source, &BuildOptions::default())
        .map_err(to_io)?;
    let table = build.table();
    println!(
        "{} rows, index is {} bytes\n",
        table.len(),
        build.heap_bytes()
    );

    // The rows are read back out of the file, not out of the index: the index
    // stores offsets, and everything else is a few kilobytes of re-lexing.
    let rows = materialize(table, from, 50, &mut source, &RowOptions::default()).map_err(to_io)?;
    for (at, row) in rows.iter().enumerate() {
        let key = row.key.as_deref().unwrap_or("");
        println!(
            "{:>10}  {:<12} {:<16} {}",
            from + at,
            row.kind.as_str(),
            key,
            row.preview
        );
    }
    Ok(())
}

fn to_io(error: SourceError) -> io::Error {
    io::Error::other(error.to_string())
}
