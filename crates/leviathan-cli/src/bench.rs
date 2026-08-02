//! The benchmark harness.
//!
//! Built before the lexer, deliberately. M1 is the phase that can invalidate the
//! product thesis, and the way that goes wrong is that the engine gets built
//! first and the measurement gets designed afterwards to fit it. So the
//! measuring apparatus comes first and the numbers land in it as they are
//! earned. See DEEP_REASONING C7.
//!
//! ## What is measured
//!
//! Two workloads are **ceilings** — the fastest anything could possibly go on
//! this machine and this file — and the rest are the engine, layer by layer:
//!
//! - `read` — stream the file in chunks and touch nothing. The I/O ceiling.
//! - `scan` — count newlines. The memory-bandwidth ceiling, and the operation
//!   the NDJSON tier-1 index is built out of (DEEP_REASONING C3).
//! - `sniff` — format detection on a 64 KiB prefix. Bounded work, so its cost
//!   should not move with file size; if it ever does, that is a bug.
//! - `lex` — tokenize the whole file.
//! - `walk` — tokenize *and* check the grammar. Sits next to `lex` so the
//!   structural layer's cost is attributable rather than absorbed. This is also
//!   full well-formedness validation, arriving early.
//! - `index` — build the tier-1 index. The row that answers the index-size exit
//!   criterion, because it reports the index's size as well as the build time.
//! - `rows` — fetch a slice of fifty rows from deep inside the index, going back
//!   to the file for every field. The other half of the same bargain: an index
//!   that stores almost nothing is only a good trade if this is cheap.
//! - `expand` — tier-2 indexing of the document's root, the largest container a
//!   file has, so the worst case rather than a flattering one.
//! - `find` — scan the whole file for a literal string that is deliberately
//!   *absent*, so every byte is read. A needle that hit early would measure how
//!   fast the scan can stop, which is a property of the fixture.
//!
//! `index` is the only workload whose *path* depends on the file: NDJSON scans
//! for newlines and never parses, a single document is walked. The two are
//! nearly an order of magnitude apart, and that gap is the product thesis. It
//! drives [`leviathan_core::Build`] — the same loop the Worker runs — rather
//! than a copy of it, so this row measures what ships (DEEP_REASONING C41).
//!
//! The ceilings are not filler. "300 MB/s" says nothing on its own; "300 MB/s
//! against a 960 MB/s memory-bandwidth ceiling" says there is roughly 3× of
//! headroom left, which is what decides whether the R2 fallback ladder (SIMD,
//! bigger chunks) is worth climbing.
//!
//! ## On reading these numbers
//!
//! A single run is a sample, not a result, and there are two separate reasons
//! why. Ordinary scheduling noise is ±15 %. Far larger, for any workload that
//! reads the whole file, is whether the fixture is resident in the OS page
//! cache: seven identical `index` runs spanned **345 ms to 1.07 s**, a 3×
//! spread with no code difference (DEEP_REASONING C49). This harness cannot
//! control that, so it does not pretend to — figures are published cold-to-warm,
//! and the ceiling rows exist partly so the *ratio* between the engine and the
//! fastest possible pass stays meaningful when neither absolute number does.
//!
//! What guards against reading too much into one run is the `observed` column,
//! which is exact and deterministic: the same fixture always lexes to the same
//! token count and indexes to the same record count, at every chunk size and
//! every batch size. A changed count is a bug; a changed wall time usually is
//! not. Cherry-picking the fastest of five is how benchmarks stop meaning
//! anything.

use std::fmt::Write as _;
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;
use std::time::{Duration, Instant};

use crate::cli::human_bytes;
use crate::sys::{self, Machine};

/// How much is read per call. 1 MiB is large enough that syscall overhead is
/// noise and small enough that peak memory stays flat.
pub const DEFAULT_CHUNK: usize = 1024 * 1024;

/// What a workload's wall time actually means.
///
/// This distinction exists because ignoring it produced a lie. `sniff` was
/// reported at 228 GB/s — faster than the machine's memory bandwidth, and
/// therefore obviously wrong. The cause: `sniff_format` early-exits on NDJSON
/// the moment it sees two value-starting lines, so it never reads the 64 KiB it
/// was handed. Dividing bytes-provided by time-taken manufactured throughput out
/// of work that was never done.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Metric {
    /// Cost scales with the bytes consumed, so bytes/second is meaningful.
    Throughput,
    /// Cost is bounded and data-dependent. The wall time *is* the result;
    /// dividing it by a byte count would say nothing true.
    Latency,
    /// The workload stopped at an error. Its wall time measures how long it took
    /// to *find* the error, which is dominated by opening the file, so a
    /// throughput computed from it is noise wearing a unit.
    Aborted,
}

/// One workload run against one fixture.
pub struct Run {
    /// The fixture, as the user named it.
    pub fixture: String,
    /// Which workload.
    pub workload: &'static str,
    /// Bytes the workload was given. For a [`Metric::Latency`] workload this is
    /// an upper bound on what it read, not a measure of what it did.
    pub bytes: u64,
    /// Whether this workload's wall time is a throughput or a latency.
    pub metric: Metric,
    /// Wall time for one pass. For workloads short enough to need repeating,
    /// this is the mean of [`Run::reps`] passes.
    pub wall: Duration,
    /// How many passes were timed to produce [`Run::wall`].
    pub reps: u32,
    /// Process peak RSS after this workload, if measurable.
    pub peak_rss: Option<u64>,
    /// A workload-specific result, printed so it is obvious the work was real
    /// and not optimized away.
    pub observed: String,
}

impl Run {
    /// Throughput in bytes per second, or `None` when that would be a fiction.
    #[must_use]
    pub fn throughput(&self) -> Option<f64> {
        if self.metric != Metric::Throughput {
            return None;
        }
        let seconds = self.wall.as_secs_f64();
        (seconds > 0.0 && self.bytes > 0).then(|| self.bytes as f64 / seconds)
    }
}

/// Every workload the harness knows, in run order.
///
/// The order is the point: `read` and `scan` are ceilings, `lex` is the engine,
/// and reading them top to bottom says what fraction of the possible the engine
/// achieved. A bare MB/s says nothing; "62 % of memory bandwidth" says whether
/// there is headroom left worth chasing.
pub const WORKLOADS: [&str; 9] = [
    "read", "scan", "sniff", "lex", "walk", "index", "rows", "expand", "find",
];

/// Run every workload in `workloads` against `path`.
///
/// # Errors
///
/// The fixture cannot be opened or read.
pub fn run_file(path: &Path, workloads: &[&'static str], chunk: usize) -> io::Result<Vec<Run>> {
    let name = path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into(),
    );

    let mut runs = Vec::new();
    for workload in workloads {
        let run = match *workload {
            "read" => measure(&name, "read", path, chunk, |buf, state| {
                // `state` accumulates a checksum for the same reason the other
                // workloads report something: a loop whose result is unused is
                // a loop the optimizer is entitled to delete.
                *state = state.wrapping_add(buf.len() as u64);
                Flow::Continue
            })?,
            "scan" => measure(&name, "scan", path, chunk, |buf, state| {
                *state += buf.iter().filter(|b| **b == b'\n').count() as u64;
                Flow::Continue
            })?,
            "sniff" => sniff(&name, path)?,
            "lex" => lex(&name, path, chunk)?,
            "walk" => walk(&name, path, chunk)?,
            "index" => index(&name, path, chunk)?,
            "rows" => rows(&name, path, chunk)?,
            "expand" => expand(&name, path)?,
            "find" => find(&name, path, chunk)?,
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown workload: {other}"),
                ));
            }
        };
        runs.push(run);
    }
    Ok(runs)
}

/// Whether the read loop should keep going.
///
/// Exists for one case: a workload that hits a syntax error has stopped doing
/// work, and reading the remaining 400 MB would put time on the clock that no
/// bytes were spent on. That is the same class of mistake as the 228 GB/s
/// `sniff` (see [`Metric`]) — a throughput divided by work that never happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flow {
    Continue,
    Stop,
}

/// Stream `path` in `chunk`-sized reads, applying `step` to each chunk.
///
/// Peak memory here should be flat regardless of file size — that is the
/// property being demonstrated, and the harness would show it immediately if
/// the read loop ever started accumulating.
fn measure<F>(
    name: &str,
    workload: &'static str,
    path: &Path,
    chunk: usize,
    mut step: F,
) -> io::Result<Run>
where
    F: FnMut(&[u8], &mut u64) -> Flow,
{
    let mut file = File::open(path)?;
    let mut buf = vec![0u8; chunk];
    let mut state = 0u64;
    let mut bytes = 0u64;

    let start = Instant::now();
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        let flow = step(&buf[..n], &mut state);
        bytes += n as u64;
        if flow == Flow::Stop {
            break;
        }
    }
    let wall = start.elapsed();

    Ok(Run {
        fixture: name.to_string(),
        workload,
        bytes,
        metric: Metric::Throughput,
        wall,
        // Streaming a file is never repeated: the second pass would be served
        // from the page cache and would measure the OS, not the engine.
        reps: 1,
        peak_rss: sys::peak_rss(),
        observed: match workload {
            "scan" => format!("{state} lines"),
            "lex" => format!("{state} tokens"),
            _ => format!("{} read", human_bytes(bytes)),
        },
    })
}

/// Tokenize the whole file with [`leviathan_core::Lexer`].
///
/// This is the first workload that measures the engine rather than a ceiling,
/// and it is deliberately *only* the lexer: no index, no allocation, no
/// structure. Whatever the index costs on top of this is then a number that can
/// be attributed, instead of a single figure with no way to tell which half is
/// slow.
///
/// A malformed fixture is not a failure of the run — `truncated` and `badutf8`
/// exist to be lexed and rejected. It stops at the error, reports it with its
/// position, and bills itself only for the bytes it actually reached.
fn lex(name: &str, path: &Path, chunk: usize) -> io::Result<Run> {
    let mut lexer = leviathan_core::Lexer::new();
    let mut tokens = 0u64;
    let mut failure = None;

    let mut run = measure(name, "lex", path, chunk, |buf, state| {
        for token in lexer.feed(buf) {
            match token {
                Ok(_) => tokens += 1,
                Err(err) => {
                    failure = Some(err);
                    break;
                }
            }
        }
        // Mirrored into `state` so the token loop cannot be optimized away, and
        // so `measure` can render it without knowing what a token is.
        *state = tokens;
        if failure.is_some() {
            Flow::Stop
        } else {
            Flow::Continue
        }
    })?;

    if failure.is_none() {
        match lexer.finish() {
            Ok(Some(_)) => tokens += 1,
            Ok(None) => {}
            Err(err) => failure = Some(err),
        }
    }

    if let Some(err) = failure {
        // Bill only the bytes that were lexed, not the bytes that were read —
        // and do not report a rate at all, because finding an error 19 bytes in
        // measures `File::open`, not the lexer.
        run.bytes = lexer.offset();
        run.metric = Metric::Aborted;
        run.observed = format!("{tokens} tokens, then {err}");
    } else {
        run.observed = format!("{tokens} tokens ({})", rate(tokens, run.wall));
    }
    Ok(run)
}

/// Tokenize *and* check the grammar.
///
/// Sits directly next to `lex` so the structural layer's cost is attributable
/// rather than absorbed: the difference between the two rows is what enforcing
/// JSON's grammar costs on top of recognizing its tokens.
///
/// This is also full well-formedness validation — M3's job, arriving early
/// because it is what the walk already does.
fn walk(name: &str, path: &Path, chunk: usize) -> io::Result<Run> {
    let mut lexer = leviathan_core::Lexer::new();
    // `Many` accepts everything `One` accepts, so it is the safe mode for a
    // fixture whose format has not been established.
    let mut structure = leviathan_core::Structure::new(leviathan_core::Documents::Many);
    let mut events = 0u64;
    let mut failure = None;

    let mut run = measure(name, "walk", path, chunk, |buf, state| {
        for token in lexer.feed(buf) {
            let outcome = token
                .map_err(|e| e.to_string())
                .and_then(|t| structure.push(t).map_err(|e| e.to_string()));
            match outcome {
                Ok(Some(_)) => events += 1,
                Ok(None) => {}
                Err(text) => {
                    failure = Some(text);
                    break;
                }
            }
        }
        *state = events;
        if failure.is_some() {
            Flow::Stop
        } else {
            Flow::Continue
        }
    })?;

    if failure.is_none() {
        match close_out(&mut lexer, &mut structure) {
            Ok(Some(_)) => events += 1,
            Ok(None) => {}
            Err(text) => failure = Some(text),
        }
    }

    if let Some(text) = failure {
        run.bytes = lexer.offset();
        run.metric = Metric::Aborted;
        run.observed = format!("{events} events, then {text}");
    } else {
        run.observed = format!(
            "{events} events, {} documents ({})",
            structure.completed(),
            rate(events, run.wall)
        );
    }
    Ok(run)
}

/// Drain the last pending token and close both machines.
///
/// Easy to forget, and silent when forgotten — which is why it is a named
/// function rather than three lines copied twice. Only a *number* can still be
/// pending at end of input (every other token is self-terminating), so omitting
/// this drops exactly one value from exactly those files that end without a
/// delimiter: `...,42` with no trailing newline. That is a large share of
/// hand-written NDJSON, and the loss would show up as an off-by-one nobody
/// could explain.
fn close_out(
    lexer: &mut leviathan_core::Lexer,
    structure: &mut leviathan_core::Structure,
) -> Result<Option<leviathan_core::Event>, String> {
    let trailing = match lexer.finish().map_err(|e| e.to_string())? {
        Some(token) => structure.push(token).map_err(|e| e.to_string())?,
        None => None,
    };
    structure.finish().map_err(|e| e.to_string())?;
    Ok(trailing)
}

/// Build the tier-1 index, the way the engine actually would.
///
/// Format is sniffed from a prefix first, and the two paths genuinely differ:
/// NDJSON scans for newlines (exact, not heuristic — see `leviathan_core::index`)
/// while a single document is walked. The row reports the resulting index size,
/// which is the M1 exit criterion nobody can argue with.
fn index(name: &str, path: &Path, chunk: usize) -> io::Result<Run> {
    let format = sniff_prefix(path)?;
    let mut source = crate::file_source::FileSource::open(path)?;
    let options = leviathan_core::BuildOptions {
        window: u32::try_from(chunk.max(64)).unwrap_or(u32::MAX),
        ..leviathan_core::BuildOptions::default()
    };

    let mut build = leviathan_core::Build::new(format);
    let mut batches = 0u32;

    let began = Instant::now();
    let stopped = loop {
        let reason = build
            .advance(&mut source, &options)
            .map_err(|e| io::Error::other(e.to_string()))?;
        batches += 1;
        if !reason.resumable() {
            break reason;
        }
    };
    let wall = began.elapsed();

    let malformed = stopped == leviathan_core::Built::Malformed;
    let bytes = build.consumed();
    let rows = build.rows();
    let heap = build.heap_bytes();

    Ok(Run {
        fixture: name.to_string(),
        workload: "index",
        bytes,
        // A build that stopped at a syntax error indexed everything up to it and
        // that index is real (C6) — but its wall time bought only `consumed`
        // bytes, so it is billed as an abort rather than as a whole-file rate
        // (C23).
        metric: if malformed {
            Metric::Aborted
        } else {
            Metric::Throughput
        },
        wall,
        reps: 1,
        peak_rss: sys::peak_rss(),
        observed: format!(
            "{rows} {noun}, {index} index ({share}), {batches} batch(es){note}",
            noun = if format == leviathan_core::Format::Ndjson {
                "records"
            } else {
                "root children"
            },
            index = human_bytes(heap as u64),
            share = share_of(heap as u64, bytes),
            note = if malformed {
                ", stopped: malformed"
            } else {
                ""
            },
        ),
    })
}

/// How many rows a screen of virtual scrolling asks for at once.
///
/// Fifty is a tall window plus overscan. The number matters because the exit
/// criterion is stated per *slice*, not per row: the engine is allowed one byte
/// range for the whole window, so a per-row figure would understate the design.
const SLICE: usize = 50;

/// Fetch a slice of rows from the middle of the index — the M1 random-access
/// exit criterion.
///
/// The criterion reads: *fetch rows 900 000–900 050 of a 5 M-element array in
/// under 20 ms, including byte-range re-read*. That "including" is the whole
/// test. The index stores only offsets, so every visible field — key, kind,
/// preview, child count — is reconstructed here by going back to the file. If
/// this is slow, C1's bargain was a bad one and the index should have stored
/// more.
///
/// Building the index is not timed: the criterion is about scrolling, and by the
/// time a user scrolls, the index exists.
fn rows(name: &str, path: &Path, chunk: usize) -> io::Result<Run> {
    let table = build_table(path, chunk)?;
    let mut source = crate::file_source::FileSource::open(path)?;
    let options = leviathan_core::RowOptions::default();

    if table.is_empty() {
        return Ok(Run {
            fixture: name.to_string(),
            workload: "rows",
            bytes: 0,
            metric: Metric::Aborted,
            wall: Duration::ZERO,
            reps: 0,
            peak_rss: sys::peak_rss(),
            observed: "no indexable rows".to_string(),
        });
    }

    // Deep into the table, never at the start: reading row 0 of anything is
    // easy, and would measure nothing the product depends on.
    let start = table.len().saturating_sub(SLICE) * 9 / 10;

    // The cold fetch is the one a user feels when they drag the scrollbar
    // somewhere new; the warm mean is what continuous scrolling costs. Both are
    // worth knowing and they are not the same number.
    let cold_start = Instant::now();
    let first = leviathan_core::materialize(&table, start, SLICE, &mut source, &options)
        .map_err(|e| io::Error::other(e.to_string()))?;
    let cold = cold_start.elapsed();

    let (wall, reps, sample) = repeat(|| {
        leviathan_core::materialize(&table, start, SLICE, &mut source, &options)
            .map(|rows| rows.len())
            .unwrap_or(0)
    });

    let containers = first.iter().filter(|r| r.kind.is_container()).count();
    let exact = first
        .iter()
        .filter(|r| r.children.is_exact() && r.kind.is_container())
        .count();

    // The size column reports the file span the slice covers — which is what
    // was re-read to draw it, and the number that makes "including byte-range
    // re-read" checkable rather than a claim.
    let span = match (first.first(), first.last()) {
        (Some(a), Some(b)) => b.value_end.unwrap_or(b.offset).saturating_sub(a.offset),
        _ => 0,
    };

    Ok(Run {
        fixture: name.to_string(),
        workload: "rows",
        bytes: span,
        metric: Metric::Latency,
        wall,
        reps,
        peak_rss: sys::peak_rss(),
        observed: format!(
            "{sample} rows from #{start}, {cold_text} cold, {containers} containers ({exact} counted exactly)",
            cold_text = human_duration(cold),
        ),
    })
}

/// Expand the document's root container — tier-2 indexing, measured.
///
/// The root is chosen because it is the largest container a file has, so this is
/// the worst case rather than a flattering one. On the 5 M-element fixture it
/// expands five million children; on NDJSON it expands record zero, which is
/// small by construction — expanding a *record* is never the expensive case, and
/// the row that matters there is `index`.
///
/// Reported as a throughput because expansion cost genuinely scales with the
/// container's bytes: unlike a row fetch, there is no way to enumerate child *n*
/// without walking children 0..*n*.
fn expand(name: &str, path: &Path) -> io::Result<Run> {
    let start = first_value_offset(path)?;
    let mut source = crate::file_source::FileSource::open(path)?;
    let options = leviathan_core::ExpandOptions::default();

    let mut expansion = leviathan_core::Expansion::at(start);
    let mut batches = 0u32;

    let began = Instant::now();
    let reason = loop {
        let reason = expansion
            .advance(&mut source, &options)
            .map_err(|e| io::Error::other(e.to_string()))?;
        batches += 1;
        if !reason.resumable() {
            break reason;
        }
    };
    let wall = began.elapsed();

    // Bill the bytes actually walked, which for a truncated or malformed
    // container is less than the file — and zero when there was no container
    // there at all, which `throughput` already declines to divide by.
    let bytes = expansion.end().unwrap_or(start).saturating_sub(start);

    let complete = expansion.is_complete();
    let children = expansion.len();
    let heap = expansion.heap_bytes();

    Ok(Run {
        fixture: name.to_string(),
        workload: "expand",
        bytes,
        metric: if complete {
            Metric::Throughput
        } else {
            Metric::Aborted
        },
        wall,
        reps: 1,
        peak_rss: sys::peak_rss(),
        observed: format!(
            "{children} children, {} index, {batches} batch(es), {}",
            human_bytes(heap as u64),
            match reason {
                leviathan_core::Stopped::Closed => "complete".to_string(),
                other => format!("stopped: {other:?}"),
            }
        ),
    })
}

/// The needle the `find` workload searches for.
///
/// Deliberately a string that is **not** in any fixture. A search that finds
/// nothing reads every byte of the file, which is the worst case and the only
/// one whose cost is a property of the engine rather than of where the fixture
/// generator happened to put a match. A needle that hits on line one would
/// measure how quickly the scan can stop.
const NEEDLE: &str = "leviathan-does-not-occur-here";

/// Scan the whole file for a literal string — what the find box does.
///
/// A throughput, and one of the few workloads where that unit is unambiguous:
/// every byte is read and compared, so cost scales with the file exactly. The
/// interesting number is not the absolute rate but the ratio to `scan`, the
/// memory-bandwidth ceiling measured on the same file: find does strictly more
/// work per byte than counting newlines, and how much more is the question.
fn find(name: &str, path: &Path, chunk: usize) -> io::Result<Run> {
    let mut source = crate::file_source::FileSource::open(path)?;
    let options = leviathan_core::FindOptions {
        window: u32::try_from(chunk.max(64)).unwrap_or(u32::MAX),
        budget: 8 * 1024 * 1024,
        // Uncapped in the benchmark: the cap exists to protect a UI from a
        // hundred million results, and letting it stop the scan early here
        // would bill a partial read as a whole-file rate (C23).
        limit: usize::MAX,
    };

    let mut search = leviathan_core::Find::new(NEEDLE, true);
    let mut batches = 0u32;

    let began = Instant::now();
    while search.stopped().is_none() {
        search
            .advance(&mut source, &options)
            .map_err(|e| io::Error::other(e.to_string()))?;
        batches += 1;
    }
    let wall = began.elapsed();

    let matches = search.matches().len();
    Ok(Run {
        fixture: name.to_string(),
        workload: "find",
        bytes: search.scanned(),
        metric: Metric::Throughput,
        wall,
        reps: 1,
        peak_rss: sys::peak_rss(),
        observed: format!("{matches} matches, {batches} batch(es)"),
    })
}

/// The offset of the first non-whitespace byte — where the root value begins.
fn first_value_offset(path: &Path) -> io::Result<u64> {
    const PREFIX: usize = 64 * 1024;
    let mut file = File::open(path)?;
    let mut buf = Vec::with_capacity(PREFIX);
    file.by_ref().take(PREFIX as u64).read_to_end(&mut buf)?;

    let bom = usize::from(buf.starts_with(&[0xEF, 0xBB, 0xBF])) * 3;
    let offset = buf[bom.min(buf.len())..]
        .iter()
        .position(|b| !matches!(b, b' ' | b'\t' | b'\r' | b'\n'))
        .unwrap_or(0);
    Ok((bom + offset) as u64)
}

/// Build whichever tier-1 table this file supports, for workloads that need one.
fn build_table(path: &Path, chunk: usize) -> io::Result<leviathan_core::ChildTable> {
    let format = sniff_prefix(path)?;
    let mut file = File::open(path)?;
    let mut buf = vec![0u8; chunk];

    if format == leviathan_core::Format::Ndjson {
        let mut scanner = leviathan_core::RecordScanner::new();
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            scanner.feed(&buf[..n]);
        }
        return Ok(scanner.finish());
    }

    let mut lexer = leviathan_core::Lexer::new();
    let mut structure = leviathan_core::Structure::new(leviathan_core::Documents::One);
    let mut collector = leviathan_core::RootCollector::new();
    'outer: loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        for token in lexer.feed(&buf[..n]) {
            match token
                .map_err(|e| e.to_string())
                .and_then(|t| structure.push(t).map_err(|e| e.to_string()))
            {
                Ok(Some(event)) => collector.observe(event),
                Ok(None) => {}
                // A malformed document still indexes as far as it got: the rows
                // before the error are perfectly good rows (C6).
                Err(_) => break 'outer,
            }
        }
    }
    let _ = close_out(&mut lexer, &mut structure).map(|trailing| {
        if let Some(event) = trailing {
            collector.observe(event);
        }
    });
    Ok(collector.finish())
}

/// Detect the format from the same 64 KiB prefix the engine would use.
fn sniff_prefix(path: &Path) -> io::Result<leviathan_core::Format> {
    const PREFIX: usize = 64 * 1024;
    let mut file = File::open(path)?;
    let mut buf = Vec::with_capacity(PREFIX);
    file.by_ref().take(PREFIX as u64).read_to_end(&mut buf)?;
    Ok(leviathan_core::sniff_format(&buf))
}

/// Render `part` as a percentage of `whole`, for the index-size criterion.
fn share_of(part: u64, whole: u64) -> String {
    if whole == 0 {
        return "—".to_string();
    }
    #[allow(clippy::cast_precision_loss)]
    let percent = (part as f64 / whole as f64) * 100.0;
    format!("{percent:.1}% of file")
}

/// Tokens per second, rendered.
///
/// Reported alongside MB/s because the two say different things, and only one of
/// them is about the lexer. A document of `[[[[[` costs about one token per
/// byte; a document of long strings costs one per hundred. So MB/s is largely a
/// statement about the *fixture's* token density, while tokens/s is a statement
/// about the engine — and across fixtures whose MB/s differ four-fold, the
/// tokens/s barely moves. See DEEP_REASONING C22.
fn rate(tokens: u64, wall: Duration) -> String {
    let seconds = wall.as_secs_f64();
    if seconds <= 0.0 || tokens == 0 {
        return "—".to_string();
    }
    #[allow(clippy::cast_precision_loss)]
    let per_second = tokens as f64 / seconds;
    if per_second >= 1e6 {
        format!("{:.1} M/s", per_second / 1e6)
    } else if per_second >= 1e3 {
        format!("{:.1} k/s", per_second / 1e3)
    } else {
        format!("{per_second:.0}/s")
    }
}

/// The shortest total run the harness will draw a conclusion from.
///
/// A single `sniff` over 64 KiB finishes in well under a microsecond. Timing
/// that once and dividing produces a throughput figure made almost entirely of
/// clock granularity — the first version of this harness cheerfully reported
/// 93 GB/s. Repeating until the total is comfortably above the timer's
/// resolution turns the number back into a measurement.
const MIN_MEASURED: Duration = Duration::from_millis(20);

/// Format detection is a [`Metric::Latency`] workload.
///
/// It is handed a 64 KiB prefix but reads only as far as it needs: an NDJSON
/// file is settled within two lines, a single document may take the whole
/// window. So the question it answers is "how long before the UI knows what
/// this file is", and that answer must not move with file size. If it ever
/// does, that is a bug this row will show.
fn sniff(name: &str, path: &Path) -> io::Result<Run> {
    const PREFIX: usize = 64 * 1024;

    let mut file = File::open(path)?;
    let mut buf = Vec::with_capacity(PREFIX);
    file.by_ref().take(PREFIX as u64).read_to_end(&mut buf)?;

    let (wall, reps, format) = repeat(|| leviathan_core::sniff_format(&buf));

    Ok(Run {
        fixture: name.to_string(),
        workload: "sniff",
        bytes: buf.len() as u64,
        metric: Metric::Latency,
        wall,
        reps,
        peak_rss: sys::peak_rss(),
        observed: format.as_str().to_string(),
    })
}

/// Run `work` enough times to measure it, and return the mean cost of one call.
///
/// Calibrates rather than guessing a repeat count: time one call, extrapolate
/// how many are needed to fill [`MIN_MEASURED`], then run that many. The result
/// each call returns is threaded back out so the optimizer cannot decide the
/// loop is dead.
fn repeat<T, F: FnMut() -> T>(mut work: F) -> (Duration, u32, T) {
    let probe_start = Instant::now();
    let mut last = work();
    let probe = probe_start.elapsed();

    // Fast enough to need repeating? Extrapolate; otherwise one run is honest.
    let reps: u32 = if probe.is_zero() {
        100_000
    } else if probe >= MIN_MEASURED {
        return (probe, 1, last);
    } else {
        u32::try_from(MIN_MEASURED.as_nanos() / probe.as_nanos().max(1)).unwrap_or(u32::MAX)
    };

    let start = Instant::now();
    for _ in 0..reps {
        last = work();
    }
    let total = start.elapsed();

    (total / reps.max(1), reps, last)
}

/// Render the human-readable report.
#[must_use]
pub fn report(runs: &[Run], machine: &Machine) -> String {
    let mut out = String::new();

    let _ = writeln!(out, "\nleviathan bench");
    let _ = writeln!(out, "machine: {machine}");
    if !sys::is_optimized() {
        let _ = writeln!(
            out,
            "\n  !! debug build — these numbers are meaningless.\n     use: cargo run --profile bench-native -p leviathan-cli -- bench ..."
        );
    }
    let _ = writeln!(
        out,
        "\n  {:<24} {:>10}  {:<8} {:>10} {:>13}  {:>10}  observed",
        "fixture", "size", "workload", "wall", "throughput", "peak RSS"
    );
    let _ = writeln!(out, "  {}", "-".repeat(103));

    let mut repeated = false;
    let mut latency_seen = false;
    let mut aborted_seen = false;
    for run in runs {
        let throughput = run.throughput().map_or_else(
            || match run.metric {
                Metric::Latency => {
                    latency_seen = true;
                    "n/a †".to_string()
                }
                Metric::Aborted => {
                    aborted_seen = true;
                    "n/a ‡".to_string()
                }
                Metric::Throughput => "—".to_string(),
            },
            |bps| format!("{}/s", human_bytes(bps as u64)),
        );
        let peak = run.peak_rss.map_or_else(|| "—".to_string(), human_bytes);
        repeated |= run.reps > 1;

        let _ = writeln!(
            out,
            "  {:<24} {:>10}  {:<8} {:>10} {:>13}  {:>10}  {}{}",
            truncate(&run.fixture, 24),
            human_bytes(run.bytes),
            run.workload,
            human_duration(run.wall),
            throughput,
            peak,
            run.observed,
            if run.reps > 1 {
                format!("  (mean of {})", run.reps)
            } else {
                String::new()
            },
        );
    }

    let _ = writeln!(
        out,
        "\n  peak RSS is a per-process high-water mark, so it accumulates across\n  \
         the workloads in one run. Baselines are flat, so it reads as a constant\n  \
         here; once indexing lands, memory criteria get one process per workload."
    );
    if repeated {
        let _ = writeln!(
            out,
            "  Workloads finishing under {} ms are repeated and averaged — timing a\n  \
             single sub-microsecond pass measures the clock, not the code.",
            MIN_MEASURED.as_millis()
        );
    }
    if latency_seen {
        let _ = writeln!(
            out,
            "  † a bounded workload: its cost does not scale with the file, so the\n  \
             wall time is the result and a rate would be division by work that\n  \
             never happened. `sniff` stops as soon as it has its answer; `rows`\n  \
             reads only the window its slice needs, which is the size shown."
        );
    }
    if aborted_seen {
        let _ = writeln!(
            out,
            "  ‡ this workload stopped at a syntax error — which for a malformed\n  \
             fixture is the correct outcome, not a failed run. The size column is\n  \
             the bytes it reached, and no rate is reported: the clock was mostly\n  \
             spent opening the file."
        );
    }
    out.push('\n');
    out
}

/// Render a duration at a resolution that does not hide what it is.
///
/// `0.000s` is not a measurement, it is a rounding artifact.
fn human_duration(d: Duration) -> String {
    #[allow(clippy::cast_precision_loss)]
    let ns = d.as_nanos() as f64;
    if ns < 1_000.0 {
        format!("{ns:.0} ns")
    } else if ns < 1_000_000.0 {
        format!("{:.2} µs", ns / 1e3)
    } else if ns < 1_000_000_000.0 {
        format!("{:.2} ms", ns / 1e6)
    } else {
        format!("{:.3} s", d.as_secs_f64())
    }
}

/// Render the machine-readable report, for CI regression tracking.
///
/// Hand-rolled rather than `serde_json`, to keep the crate dependency-free.
/// It is thirty lines and the shape is fixed.
#[must_use]
pub fn report_json(runs: &[Run], machine: &Machine) -> String {
    let mut out = String::new();
    let _ = write!(
        out,
        r#"{{"machine":{{"cpus":{},"arch":"{}","os":"{}","optimized":{}}},"runs":["#,
        machine.cpus,
        escape(machine.arch),
        escape(machine.os),
        sys::is_optimized(),
    );
    for (i, run) in runs.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(
            out,
            r#"{{"fixture":"{}","workload":"{}","metric":"{}","bytes":{},"wall_ns":{},"reps":{},"throughput_bps":{},"peak_rss":{},"observed":"{}"}}"#,
            escape(&run.fixture),
            run.workload,
            match run.metric {
                Metric::Latency => "latency",
                Metric::Aborted => "aborted",
                Metric::Throughput => "throughput",
            },
            run.bytes,
            run.wall.as_nanos(),
            run.reps,
            run.throughput()
                .map_or_else(|| "null".to_string(), |t| format!("{t:.0}")),
            run.peak_rss
                .map_or_else(|| "null".to_string(), |p| p.to_string()),
            escape(&run.observed),
        );
    }
    out.push_str("]}\n");
    out
}

/// Escape a string for embedding in JSON.
///
/// Not optional: fixture names are file paths, and on Windows those are full of
/// backslashes. Emitting them raw would produce invalid JSON on the platform
/// this is developed on.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let kept: String = text.chars().take(max.saturating_sub(1)).collect();
    format!("{kept}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn fixture(bytes: &[u8]) -> (tempdir::Dir, std::path::PathBuf) {
        let dir = tempdir::Dir::new();
        let path = dir.path().join("fixture.ndjson");
        File::create(&path).unwrap().write_all(bytes).unwrap();
        (dir, path)
    }

    #[test]
    fn read_and_scan_see_the_whole_file() {
        let data = b"{\"a\":1}\n{\"a\":2}\n{\"a\":3}\n";
        let (_dir, path) = fixture(data);

        let runs = run_file(&path, &["read", "scan"], 8).unwrap();
        assert_eq!(runs[0].bytes, data.len() as u64);
        assert_eq!(runs[1].bytes, data.len() as u64);
        // Reading in 8-byte chunks must not lose or double-count lines.
        assert_eq!(runs[1].observed, "3 lines");
    }

    #[test]
    fn the_chunk_size_does_not_change_the_answer() {
        let mut data = Vec::new();
        for i in 0..500 {
            let _ = writeln!(data, r#"{{"id":{i}}}"#);
        }
        let (_dir, path) = fixture(&data);

        for chunk in [1, 3, 7, 64, 4096, 1 << 20] {
            let runs = run_file(&path, &["read", "scan"], chunk).unwrap();
            assert_eq!(runs[0].bytes, data.len() as u64, "chunk {chunk}");
            assert_eq!(runs[1].observed, "500 lines", "chunk {chunk}");
        }
    }

    #[test]
    fn lex_counts_every_token_in_the_file() {
        // 3 records × 5 tokens: `{`, key, `:`, value, `}`.
        let (_dir, path) = fixture(b"{\"a\":1}\n{\"a\":2}\n{\"a\":3}\n");
        let runs = run_file(&path, &["lex"], DEFAULT_CHUNK).unwrap();
        assert!(
            runs[0].observed.starts_with("15 tokens ("),
            "{}",
            runs[0].observed
        );
        assert_eq!(runs[0].metric, Metric::Throughput);
        assert!(runs[0].throughput().is_some());
    }

    #[test]
    fn the_chunk_size_does_not_change_the_token_count() {
        // The lexer's resumability, asserted through the harness rather than in
        // its own unit tests: chunk boundaries are an I/O accident and must
        // never be visible in a benchmark result.
        let mut data = Vec::new();
        for i in 0..500 {
            let _ = writeln!(data, r#"{{"id":{i},"name":"row \"{i}\"","ok":true}}"#);
        }
        let (_dir, path) = fixture(&data);

        for chunk in [1, 3, 7, 64, 4096, 1 << 20] {
            let runs = run_file(&path, &["lex"], chunk).unwrap();
            assert!(
                runs[0].observed.starts_with("6500 tokens"),
                "chunk {chunk}: {}",
                runs[0].observed
            );
            assert_eq!(runs[0].bytes, data.len() as u64, "chunk {chunk}");
        }
    }

    #[test]
    fn a_file_ending_in_a_bare_number_does_not_lose_its_last_value() {
        // The bug this guards: a number is the one token that cannot be emitted
        // until the byte after it arrives, so a workload that forgets
        // `lexer.finish()` silently drops the final value of any file that ends
        // without a delimiter — which is most hand-written NDJSON.
        let (_dir, path) = fixture(b"1\n2\n3");
        let runs = run_file(&path, &["walk", "index"], DEFAULT_CHUNK).unwrap();
        assert!(
            runs[0].observed.contains("3 documents"),
            "{}",
            runs[0].observed
        );
        assert!(
            runs[1].observed.starts_with("3 records"),
            "{}",
            runs[1].observed
        );

        // And the same file with a trailing newline must agree.
        let (_dir2, path2) = fixture(b"1\n2\n3\n");
        let with_newline = run_file(&path2, &["walk"], DEFAULT_CHUNK).unwrap();
        assert!(with_newline[0].observed.contains("3 documents"));
    }

    #[test]
    fn walk_validates_what_lex_accepts() {
        // `[1,2` lexes perfectly — every token is well formed — and is still not
        // a document. That gap is exactly what the walk workload measures.
        let (_dir, path) = fixture(b"[1,2");
        let runs = run_file(&path, &["lex", "walk"], DEFAULT_CHUNK).unwrap();
        assert_eq!(runs[0].metric, Metric::Throughput, "lex is happy");
        assert_eq!(runs[1].metric, Metric::Aborted, "walk is not");
        assert!(
            runs[1].observed.contains("unclosed"),
            "{}",
            runs[1].observed
        );
    }

    #[test]
    fn rows_materializes_a_slice_from_the_middle() {
        let mut data = Vec::new();
        for i in 0..500 {
            let _ = writeln!(data, r#"{{"id":{i},"name":"row {i}"}}"#);
        }
        let (_dir, path) = fixture(&data);

        let runs = run_file(&path, &["rows"], DEFAULT_CHUNK).unwrap();
        assert_eq!(runs[0].metric, Metric::Latency, "a slice has no throughput");
        assert!(runs[0].observed.contains("50 rows"), "{}", runs[0].observed);
        assert!(
            runs[0]
                .observed
                .contains("50 containers (50 counted exactly)"),
            "{}",
            runs[0].observed
        );
        assert!(
            runs[0].bytes > 0,
            "the slice's file span should be reported"
        );
    }

    #[test]
    fn expand_indexes_a_containers_children() {
        let (_dir, path) = fixture(br#"[10,20,30,{"a":1}]"#);
        let runs = run_file(&path, &["expand"], DEFAULT_CHUNK).unwrap();
        assert!(
            runs[0].observed.starts_with("4 children"),
            "{}",
            runs[0].observed
        );
        assert!(
            runs[0].observed.contains("complete"),
            "{}",
            runs[0].observed
        );
        assert_eq!(runs[0].metric, Metric::Throughput);
    }

    #[test]
    fn expand_reports_a_truncated_container_without_losing_it() {
        // C6 once more: an unclosed container still yields its children, and the
        // run is marked so no throughput is published for a partial walk.
        let (_dir, path) = fixture(b"[1,2,3,4");
        let runs = run_file(&path, &["expand"], DEFAULT_CHUNK).unwrap();
        assert!(
            runs[0].observed.starts_with("4 children"),
            "{}",
            runs[0].observed
        );
        assert!(
            runs[0].observed.contains("EndOfSource"),
            "{}",
            runs[0].observed
        );
        assert_eq!(runs[0].metric, Metric::Aborted);
    }

    #[test]
    fn rows_on_an_unindexable_file_reports_rather_than_failing() {
        let (_dir, path) = fixture(b"   \n\n  \n");
        let runs = run_file(&path, &["rows"], DEFAULT_CHUNK).unwrap();
        assert_eq!(runs[0].metric, Metric::Aborted);
        assert_eq!(runs[0].observed, "no indexable rows");
    }

    #[test]
    fn index_size_is_eight_bytes_per_record() {
        // The M1 exit criterion is a number, so the harness has to report the
        // real one: 100 records is 800 bytes of index, not "about" 800.
        let mut data = Vec::new();
        for i in 0..100 {
            let _ = writeln!(data, r#"{{"id":{i}}}"#);
        }
        let (_dir, path) = fixture(&data);

        let runs = run_file(&path, &["index"], DEFAULT_CHUNK).unwrap();
        assert!(
            runs[0].observed.starts_with("100 records, 800 B index"),
            "{}",
            runs[0].observed
        );
    }

    #[test]
    fn a_malformed_file_still_indexes_because_tier_one_does_not_parse() {
        // C6, degrade never abort: the NDJSON index is a newline scan, so a file
        // that fails validation still opens and is browsable. `walk` is the one
        // that objects, and it objects separately.
        let (_dir, path) = fixture(b"{\"a\":1}\n{oops\n{\"c\":3}\n");
        let runs = run_file(&path, &["index", "walk"], DEFAULT_CHUNK).unwrap();
        assert!(
            runs[0].observed.starts_with("3 records"),
            "{}",
            runs[0].observed
        );
        assert_eq!(runs[0].metric, Metric::Throughput);
        assert_eq!(runs[1].metric, Metric::Aborted);
    }

    #[test]
    fn lex_reports_a_syntax_error_and_bills_only_what_it_read() {
        // The `truncated` and `badutf8` fixtures are supposed to fail. A run
        // over them must say where, and must not claim throughput over the
        // bytes it never reached — the same mistake that produced 228 GB/s.
        let mut data = Vec::from(b"{\"a\":1}\n");
        data.extend(std::iter::repeat_n(b'x', 100_000));
        let (_dir, path) = fixture(&data);

        let runs = run_file(&path, &["lex"], 64).unwrap();
        assert!(
            runs[0].observed.contains("5 tokens, then"),
            "{}",
            runs[0].observed
        );
        assert!(runs[0].observed.contains("byte 8"), "{}", runs[0].observed);
        assert_eq!(runs[0].bytes, 8, "billed for the bytes lexed, not the file");
        assert_eq!(runs[0].metric, Metric::Aborted);
        assert_eq!(
            runs[0].throughput(),
            None,
            "no rate from a run that stopped"
        );

        let text = report(&runs, &Machine::detect());
        assert!(text.contains("n/a ‡"), "{text}");
        assert!(
            text.contains("correct outcome"),
            "footnote missing:\n{text}"
        );
    }

    #[test]
    fn sniff_never_reports_a_throughput() {
        // The regression this guards: `sniff_format` early-exits on NDJSON as
        // soon as it has seen two value-starting lines, so it does not read the
        // 64 KiB it was handed. Dividing bytes-given by time-taken reported
        // 228 GB/s — faster than the machine's memory bandwidth, and therefore
        // self-evidently a lie. Latency is the only honest metric here.
        let mut data = Vec::new();
        while data.len() < 200_000 {
            let _ = writeln!(data, r#"{{"id":1,"pad":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}}"#);
        }
        let (_dir, path) = fixture(&data);

        let runs = run_file(&path, &["sniff"], DEFAULT_CHUNK).unwrap();
        assert_eq!(runs[0].metric, Metric::Latency);
        assert_eq!(runs[0].throughput(), None);

        let text = report(&runs, &Machine::detect());
        assert!(text.contains("n/a †"), "{text}");
        assert!(text.contains("never happened"), "footnote missing:\n{text}");
    }

    #[test]
    fn streaming_workloads_do_report_a_throughput() {
        let (_dir, path) = fixture(&vec![b'x'; 100_000]);
        for run in run_file(&path, &["read", "scan"], 4096).unwrap() {
            assert_eq!(run.metric, Metric::Throughput);
            assert!(
                run.throughput().is_some(),
                "{} should have a throughput",
                run.workload
            );
        }
    }

    #[test]
    fn sniff_reports_the_prefix_not_the_file() {
        // A 300 kB file, but sniff must only ever account for 64 KiB.
        let mut data = Vec::new();
        while data.len() < 300_000 {
            let _ = writeln!(data, r#"{{"id":1,"pad":"xxxxxxxxxxxxxxxxxxxxxxxxxxxx"}}"#);
        }
        let (_dir, path) = fixture(&data);

        let runs = run_file(&path, &["sniff"], DEFAULT_CHUNK).unwrap();
        assert_eq!(runs[0].bytes, 64 * 1024, "capped at the prefix window");
        assert_eq!(runs[0].observed, "ndjson");
        // And the cost of answering must not scale with the file behind it:
        // the same content at 6× the size costs the same to identify.
        let mut bigger = data.clone();
        while bigger.len() < 1_800_000 {
            bigger.extend_from_slice(&data);
        }
        let (_dir2, big_path) = fixture(&bigger);
        let big = run_file(&big_path, &["sniff"], DEFAULT_CHUNK).unwrap();
        assert_eq!(big[0].bytes, runs[0].bytes);
    }

    #[test]
    fn short_workloads_are_repeated_so_the_number_means_something() {
        let (_dir, path) = fixture(b"{\"a\":1}\n{\"a\":2}\n");
        let runs = run_file(&path, &["sniff"], DEFAULT_CHUNK).unwrap();

        // Sniffing 16 bytes takes nanoseconds; timing that once would report
        // whatever the clock's granularity happens to be.
        assert!(
            runs[0].reps > 1,
            "expected repetition, got {}",
            runs[0].reps
        );
        assert!(runs[0].wall > Duration::ZERO, "mean should be non-zero");
        assert!(
            runs[0].wall < Duration::from_millis(1),
            "a mean of one sniff should be tiny, got {:?}",
            runs[0].wall
        );
    }

    #[test]
    fn streaming_workloads_are_never_repeated() {
        // A second pass would be served from the page cache and would measure
        // the OS rather than the engine.
        let (_dir, path) = fixture(b"{\"a\":1}\n");
        for run in run_file(&path, &["read", "scan"], DEFAULT_CHUNK).unwrap() {
            assert_eq!(run.reps, 1, "{} should not repeat", run.workload);
        }
    }

    #[test]
    fn repeat_calibrates_toward_the_minimum_measured_window() {
        // Work that already exceeds the window runs exactly once.
        let (wall, reps, value) = repeat(|| {
            std::thread::sleep(Duration::from_millis(25));
            7
        });
        assert_eq!(reps, 1);
        assert_eq!(value, 7);
        assert!(wall >= Duration::from_millis(20));

        // Work far below it is repeated, and the result still comes back.
        let mut calls = 0u32;
        let (_, reps, value) = repeat(|| {
            calls += 1;
            "done"
        });
        assert!(reps > 1);
        assert_eq!(value, "done");
        assert!(calls >= reps, "every rep should have run: {calls} < {reps}");
    }

    #[test]
    fn durations_are_rendered_at_a_useful_resolution() {
        assert_eq!(human_duration(Duration::from_nanos(400)), "400 ns");
        assert_eq!(human_duration(Duration::from_nanos(1_500)), "1.50 µs");
        assert_eq!(human_duration(Duration::from_micros(1_500)), "1.50 ms");
        assert_eq!(human_duration(Duration::from_millis(1_500)), "1.500 s");
        // The artifact this replaced: sub-millisecond work must not read "0.000s".
        assert_ne!(human_duration(Duration::from_nanos(700)), "0.000s");
    }

    #[test]
    fn an_empty_file_does_not_divide_by_zero() {
        let (_dir, path) = fixture(b"");
        let runs = run_file(&path, &WORKLOADS, DEFAULT_CHUNK).unwrap();
        for run in &runs {
            assert_eq!(run.bytes, 0);
            assert_eq!(run.throughput(), None);
        }
        // And the report renders rather than panicking on the em dash path.
        assert!(report(&runs, &Machine::detect()).contains("read"));
    }

    #[test]
    fn find_reads_the_whole_file_when_the_needle_is_absent() {
        // The property that makes the number meaningful: a miss is a full scan,
        // so `bytes` must equal the file and the rate must be a whole-file rate.
        let (_dir, path) = fixture(b"{\"a\":1}\n{\"a\":2}\n{\"a\":3}\n");
        let runs = run_file(&path, &["find"], DEFAULT_CHUNK).unwrap();

        assert_eq!(runs[0].bytes, 24, "every byte was scanned");
        assert!(runs[0].observed.starts_with("0 matches"));
        assert!(
            runs[0].throughput().is_some(),
            "a completed scan has a rate"
        );
    }

    #[test]
    fn a_missing_fixture_is_an_error_not_a_panic() {
        assert!(run_file(Path::new("does-not-exist.ndjson"), &["read"], 64).is_err());
    }

    #[test]
    fn json_output_escapes_windows_paths() {
        let run = Run {
            fixture: r#"C:\fixtures\"quoted".ndjson"#.to_string(),
            workload: "read",
            bytes: 100,
            metric: Metric::Throughput,
            wall: Duration::from_millis(10),
            reps: 1,
            peak_rss: Some(1024),
            observed: "ok".to_string(),
        };
        let json = report_json(&[run], &Machine::detect());
        assert!(
            json.contains(r#"C:\\fixtures\\\"quoted\".ndjson"#),
            "{json}"
        );
        assert!(!json.contains('\n') || json.ends_with('\n'));
    }

    #[test]
    fn json_renders_unmeasurable_values_as_null() {
        let run = Run {
            fixture: "x".to_string(),
            workload: "read",
            bytes: 0,
            metric: Metric::Throughput,
            wall: Duration::ZERO,
            reps: 1,
            peak_rss: None,
            observed: String::new(),
        };
        let json = report_json(&[run], &Machine::detect());
        assert!(json.contains(r#""throughput_bps":null"#), "{json}");
        assert!(json.contains(r#""peak_rss":null"#), "{json}");
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
                    "leviathan-bench-test-{}-{unique}",
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
