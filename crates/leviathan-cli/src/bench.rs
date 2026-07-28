//! The benchmark harness.
//!
//! Built before the lexer, deliberately. M1 is the phase that can invalidate the
//! product thesis, and the way that goes wrong is that the engine gets built
//! first and the measurement gets designed afterwards to fit it. So the
//! measuring apparatus comes first and the numbers land in it as they are
//! earned. See DEEP_REASONING C7.
//!
//! ## What is measured today
//!
//! The lexer does not exist yet, so what runs here are **ceilings** — the
//! fastest anything could possibly go on this machine and this file:
//!
//! - `read` — stream the file in chunks and touch nothing. The I/O ceiling.
//! - `scan` — count newlines. The memory-bandwidth ceiling, and the operation
//!   the NDJSON tier-1 index is built out of (DEEP_REASONING C3).
//! - `sniff` — format detection on a 64 KiB prefix. Bounded work, so its cost
//!   should not move with file size; if it ever does, that is a bug.
//!
//! These are not filler. When `index` lands next to them, "we hit 40 % of memory
//! bandwidth" is a far more useful statement than a bare MB/s, and the exit
//! criterion (≥200 MB/s native) is only meaningful against a known ceiling.

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
        if self.metric == Metric::Latency {
            return None;
        }
        let seconds = self.wall.as_secs_f64();
        (seconds > 0.0 && self.bytes > 0).then(|| self.bytes as f64 / seconds)
    }
}

/// Every workload the harness knows, in run order.
pub const WORKLOADS: [&str; 3] = ["read", "scan", "sniff"];

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
            })?,
            "scan" => measure(&name, "scan", path, chunk, |buf, state| {
                *state += buf.iter().filter(|b| **b == b'\n').count() as u64;
            })?,
            "sniff" => sniff(&name, path)?,
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
    F: FnMut(&[u8], &mut u64),
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
        step(&buf[..n], &mut state);
        bytes += n as u64;
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
            _ => format!("{} read", human_bytes(bytes)),
        },
    })
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
    for run in runs {
        let throughput = run.throughput().map_or_else(
            || {
                if run.metric == Metric::Latency {
                    latency_seen = true;
                    "n/a †".to_string()
                } else {
                    "—".to_string()
                }
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
            "  † this workload stops as soon as it has its answer, so it does not\n  \
             consume the bytes it was given. Its wall time is the result; a\n  \
             throughput would be division by work that never happened."
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
            if run.metric == Metric::Latency {
                "latency"
            } else {
                "throughput"
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
