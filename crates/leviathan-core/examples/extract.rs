//! Filter a file's records and write the matches back out, streaming.
//!
//! ```sh
//! cargo run --example extract -- data.ndjson '@.level == "error"' out.ndjson
//! ```
//!
//! This is `jq 'select(...)'` over a file too large to hand `jq`, in about
//! sixty lines and with no dependencies. Two things worth noticing:
//!
//! - **Peak memory does not depend on the file.** One record is read, tested,
//!   converted and dropped. A 500 MB input costs the index plus a 1 MiB window.
//! - **The output re-parses to exactly what was selected.** Export re-emits the
//!   source's own tokens, so a number keeps the spelling the file gave it —
//!   `1e400` stays `1e400` and does not become `Infinity` (C68).

use std::env;
use std::fs::File;
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};

use leviathan_core::{
    Build, BuildOptions, ByteRange, Export, ExportFormat, Filter, SourceError, sniff_format,
};

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
    let (Some(path), Some(query)) = (args.next(), args.next()) else {
        eprintln!("usage: extract <FILE> <FILTER> [OUT]");
        eprintln!(r#"   eg: extract log.ndjson '@.level == "error" && @.ms > 1000'"#);
        std::process::exit(2);
    };

    // A filter that does not parse is refused with a message naming what it did
    // not understand, rather than being reinterpreted as something else (C59).
    let filter = Filter::parse(&query).map_err(|e| io::Error::other(e.to_string()))?;

    let file = File::open(&path)?;
    let len = file.metadata()?.len();
    let mut source = FileSource {
        file,
        len,
        scratch: Vec::new(),
    };

    let format = sniff_format(source.read(0, 64 * 1024).map_err(to_io)?);
    let mut build = Build::new(format);
    build
        .run_to_end(&mut source, &BuildOptions::default())
        .map_err(to_io)?;
    let table = build.table();

    let mut out: Box<dyn Write> = match args.next() {
        Some(target) => Box::new(BufWriter::new(File::create(target)?)),
        None => Box::new(BufWriter::new(io::stdout())),
    };

    // One matcher for the whole pass, not one per record: it owns the lexer and
    // the path stack, and rebuilding those per record is the allocation C61
    // exists to avoid.
    let mut matcher = filter.matcher();
    let mut export = Export::new(ExportFormat::Ndjson);
    out.write_all(export.open())?;

    let mut matched = 0u64;
    for row in 0..table.len() {
        let Some(start) = table.child(row) else {
            continue;
        };
        let end = table.child(row + 1).unwrap_or(len);
        let record = source
            .read(start, (end - start).min(u32::MAX as u64) as u32)
            .map_err(to_io)?
            .to_vec();

        if matcher.matches(&record) {
            out.write_all(export.push(&mut source, start, end).map_err(to_io)?)?;
            matched += 1;
        }
    }
    out.write_all(export.close())?;
    out.flush()?;

    eprintln!("{matched} of {} records matched", table.len());
    Ok(())
}

fn to_io(error: SourceError) -> io::Error {
    io::Error::other(error.to_string())
}
