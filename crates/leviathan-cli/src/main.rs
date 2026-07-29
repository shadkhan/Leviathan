//! Native harness for `leviathan-core`.
//!
//! This binary exists for three reasons, in order of importance:
//!
//! 1. It proves the core is genuinely portable. The same crate the extension
//!    compiles to WASM is linked here against real files with no changes — if
//!    that ever stops being true, this binary stops compiling.
//! 2. It is where benchmarks run. Browser numbers are noisy; native numbers
//!    isolate engine cost from browser cost, and publishing both is more honest
//!    than publishing either alone.
//! 3. It generates the large fixtures that CI and the benchmarks need, so they
//!    never have to be committed.
//!
//! It has **no dependencies**, which is a deliberate cost paid in `cli.rs`:
//! `cargo install leviathan-cli` is a shipping promise, and it should install in
//! seconds on the machine of someone reproducing a benchmark.

mod bench;
mod cli;
mod file_source;
mod fixtures;
mod sys;

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use cli::{Args, human_bytes};
use fixtures::{Kind, Spec};

const USAGE: &str = "\
leviathan — streaming JSON indexing core

USAGE:
    leviathan <COMMAND> [OPTIONS]

COMMANDS:
    version                 Print the core engine version
    echo <N>                Round-trip a u32 through the core (boundary smoke test)
    sniff [FILE]            Detect single-document vs NDJSON (stdin if FILE omitted)
    fixtures <KIND>         Generate a test fixture
    fixtures list           List the fixture kinds
    bench [FILE...]         Benchmark against fixtures
    help                    Print this message

FIXTURES OPTIONS:
    --size <SIZE>           Target size (500MB, 1GiB, 1_000_000). Default 50MB
    --out <PATH>            Output file. Default fixtures/generated/<kind>-<size>.<ext>
    --seed <N>              PRNG seed; the same seed always produces the same bytes. Default 1
    --depth <N>             Nesting depth, for `deep`. Default 100000
    --count <N>             Element count, for `wide`. Default 5000000

BENCH OPTIONS:
    --workload <NAME>       One of read, scan, sniff, lex. Repeatable. Default: all
    --chunk <SIZE>          Read chunk size. Default 1MiB
    --json                  Machine-readable output, for CI regression tracking

Benchmarks must be built for speed — the default release profile is tuned for
WASM size and will understate throughput:

    cargo run --profile bench-native -p leviathan-cli -- bench <FILE>

Later milestones add: index, query, validate, dedup, export.

Workload order is meaningful: read and scan are ceilings (I/O and memory
bandwidth), lex is the engine. The useful number is the ratio between them.
";

/// How much of a file is enough to tell single-document from NDJSON.
const SNIFF_PREFIX_BYTES: usize = 64 * 1024;

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let command = argv.first().map(String::as_str).unwrap_or("help");
    let rest = argv.get(1..).unwrap_or(&[]);

    match run(command, rest) {
        Ok(output) => {
            if !output.is_empty() {
                println!("{output}");
            }
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run(command: &str, rest: &[String]) -> Result<String, String> {
    match command {
        "version" | "--version" | "-V" => Ok(leviathan_core::VERSION.to_string()),
        "echo" => {
            let raw = rest.first().ok_or("echo requires a number")?;
            let value: u32 = raw.parse().map_err(|_| format!("not a u32: {raw}"))?;
            Ok(leviathan_core::echo(value).to_string())
        }
        "sniff" => {
            let prefix = read_prefix(rest.first().map(String::as_str))?;
            Ok(leviathan_core::sniff_format(&prefix).as_str().to_string())
        }
        "fixtures" => fixtures_command(rest),
        "bench" => bench_command(rest),
        "help" | "--help" | "-h" => Ok(USAGE.to_string()),
        other => Err(format!("unknown command: {other}\n\n{USAGE}")),
    }
}

fn fixtures_command(rest: &[String]) -> Result<String, String> {
    let args = Args::parse(rest)?;
    let name = args
        .positional(0)
        .ok_or_else(|| format!("fixtures requires a kind (or `list`)\n\n{}", fixture_list()))?;

    if name == "list" {
        return Ok(fixture_list());
    }

    args.reject_unknown(&["size", "out", "seed", "depth", "count"])?;

    let kind = Kind::parse(name)
        .ok_or_else(|| format!("unknown fixture kind: {name}\n\n{}", fixture_list()))?;

    let spec = Spec {
        target_bytes: args.size("size", 50_000_000)?,
        seed: args.count("seed", 1)?,
        depth: args.count("depth", 100_000)?,
        count: args.count("count", 5_000_000)?,
    };

    let path = match args.get("out") {
        Some(out) => PathBuf::from(out),
        None => default_fixture_path(kind, &spec),
    };

    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("could not create {}: {e}", parent.display()))?;
    }

    let file = std::fs::File::create(&path)
        .map_err(|e| format!("could not create {}: {e}", path.display()))?;
    // A 500 MB fixture through an unbuffered writer is millions of syscalls.
    let writer = std::io::BufWriter::with_capacity(1024 * 1024, file);

    let started = std::time::Instant::now();
    let stats = fixtures::generate(kind, &spec, writer)
        .map_err(|e| format!("writing {}: {e}", path.display()))?;
    let elapsed = started.elapsed();

    Ok(format!(
        "wrote {} ({}, {} records, seed {}) in {:.2}s",
        path.display(),
        human_bytes(stats.bytes),
        stats.records,
        spec.seed,
        elapsed.as_secs_f64(),
    ))
}

fn fixture_list() -> String {
    let mut out = String::from("FIXTURE KINDS:\n");
    for kind in Kind::all() {
        out.push_str(&format!("    {:<12} {}\n", kind.as_str(), kind.purpose()));
    }
    out
}

/// `fixtures/generated/ndjson-500MB.ndjson` — self-describing, and inside the
/// directory `.gitignore` already excludes.
fn default_fixture_path(kind: Kind, spec: &Spec) -> PathBuf {
    let label = match kind {
        Kind::Deep => format!("{}", spec.depth),
        Kind::Wide => format!("{}", spec.count),
        _ => human_bytes(spec.target_bytes).replace(' ', ""),
    };
    PathBuf::from("fixtures/generated").join(format!(
        "{}-{label}.{}",
        kind.as_str(),
        kind.extension()
    ))
}

fn bench_command(rest: &[String]) -> Result<String, String> {
    let args = Args::parse(rest)?;
    args.reject_unknown(&["workload", "chunk", "json"])?;

    let paths = args.positionals_from(0);
    if paths.is_empty() {
        return Err("bench requires at least one fixture\n\n\
             generate one first:\n    \
             leviathan fixtures ndjson --size 500MB"
            .to_string());
    }

    let workloads: Vec<&'static str> = match args.get("workload") {
        Some(name) => {
            let known = bench::WORKLOADS
                .iter()
                .find(|w| **w == name)
                .ok_or_else(|| {
                    format!(
                        "unknown workload: {name} (known: {})",
                        bench::WORKLOADS.join(", ")
                    )
                })?;
            vec![*known]
        }
        None => bench::WORKLOADS.to_vec(),
    };

    let chunk = usize::try_from(args.size("chunk", bench::DEFAULT_CHUNK as u64)?)
        .map_err(|_| "--chunk is too large for this platform".to_string())?;
    if chunk == 0 {
        return Err("--chunk must be at least 1 byte".to_string());
    }

    let mut runs = Vec::new();
    for path in paths {
        let path = Path::new(path);
        if !path.exists() {
            return Err(format!(
                "no such fixture: {}\n\ngenerate it with:\n    leviathan fixtures ndjson --size 500MB",
                path.display()
            ));
        }
        runs.extend(
            bench::run_file(path, &workloads, chunk)
                .map_err(|e| format!("{}: {e}", path.display()))?,
        );
    }

    let machine = sys::Machine::detect();
    Ok(if args.has("json") {
        bench::report_json(&runs, &machine)
    } else {
        bench::report(&runs, &machine)
    })
}

/// Read at most [`SNIFF_PREFIX_BYTES`] from a file or stdin.
///
/// Note what this does *not* do: read the whole file. Even the CLI's trivial
/// commands honour the rule that file size must not drive memory use.
fn read_prefix(path: Option<&str>) -> Result<Vec<u8>, String> {
    let mut buf = Vec::with_capacity(SNIFF_PREFIX_BYTES.min(8 * 1024));
    match path {
        Some(path) => {
            let file = std::fs::File::open(path).map_err(|e| format!("{path}: {e}"))?;
            file.take(SNIFF_PREFIX_BYTES as u64)
                .read_to_end(&mut buf)
                .map_err(|e| format!("{path}: {e}"))?;
        }
        None => {
            std::io::stdin()
                .take(SNIFF_PREFIX_BYTES as u64)
                .read_to_end(&mut buf)
                .map_err(|e| format!("stdin: {e}"))?;
        }
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn echo_round_trips() {
        assert_eq!(run("echo", &argv(&["42"])).unwrap(), "42");
    }

    #[test]
    fn echo_rejects_garbage() {
        assert!(run("echo", &argv(&["banana"])).is_err());
    }

    #[test]
    fn unknown_command_is_an_error() {
        assert!(run("frobnicate", &[]).is_err());
    }

    #[test]
    fn fixtures_list_names_every_kind() {
        let listed = run("fixtures", &argv(&["list"])).unwrap();
        for kind in Kind::all() {
            assert!(listed.contains(kind.as_str()), "missing {}", kind.as_str());
        }
    }

    #[test]
    fn fixtures_rejects_an_unknown_kind() {
        assert!(run("fixtures", &argv(&["parquet"])).is_err());
    }

    #[test]
    fn fixtures_rejects_a_mistyped_flag() {
        // `--sixe` would otherwise silently generate the default 50 MB.
        assert!(run("fixtures", &argv(&["ndjson", "--sixe", "1MB"])).is_err());
    }

    #[test]
    fn bench_without_a_fixture_explains_how_to_make_one() {
        let error = run("bench", &[]).unwrap_err();
        assert!(error.contains("fixtures ndjson"), "{error}");
    }

    #[test]
    fn bench_rejects_an_unknown_workload() {
        assert!(run("bench", &argv(&["x.ndjson", "--workload", "parse"])).is_err());
    }

    #[test]
    fn bench_names_the_missing_file() {
        let error = run("bench", &argv(&["nope.ndjson"])).unwrap_err();
        assert!(error.contains("nope.ndjson"), "{error}");
    }

    #[test]
    fn default_fixture_paths_are_self_describing() {
        let spec = Spec {
            target_bytes: 500_000_000,
            seed: 1,
            depth: 100_000,
            count: 5_000_000,
        };
        assert!(
            default_fixture_path(Kind::Ndjson, &spec)
                .to_string_lossy()
                .ends_with("ndjson-500.0MB.ndjson")
        );
        assert!(
            default_fixture_path(Kind::Deep, &spec)
                .to_string_lossy()
                .ends_with("deep-100000.json")
        );
    }

    #[test]
    fn generated_fixtures_land_where_they_are_asked_to() {
        let dir = std::env::temp_dir().join(format!("leviathan-cli-test-{}", std::process::id()));
        let path = dir.join("tiny.ndjson");
        let out = path.to_string_lossy().to_string();

        let message = run(
            "fixtures",
            &argv(&["ndjson", "--size", "10000", "--out", &out]),
        )
        .unwrap();
        assert!(message.contains("records"), "{message}");
        assert!(path.exists());

        // And the CLI agrees with itself about what it just wrote.
        assert_eq!(run("sniff", &argv(&[&out])).unwrap(), "ndjson");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
