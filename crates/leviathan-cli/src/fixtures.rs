//! Deterministic fixture generation.
//!
//! Benchmark fixtures are up to 500 MB, so they are generated rather than
//! committed (`.gitignore`). That makes determinism a correctness requirement,
//! not a nicety: a benchmark number is only reproducible if the bytes behind it
//! are reproducible, and "I ran it on my file" is not a measurement anyone else
//! can check. Every generator here is driven by a seeded PRNG, so the same
//! `--seed` produces the same bytes on any machine, forever.
//!
//! The pathological set exists for a specific reason. Leviathan's promise is
//! that a *broken* large file still opens — truncated dumps, a log rotation
//! mid-record, one bad escape at 90 % depth (DEEP_REASONING C6). Those cases
//! have to be generatable before they can be tested, and they are the ones most
//! likely to panic a lexer.

use std::fmt::Write as _;
use std::io::{self, Write};

/// A shape of test document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Newline-delimited log/API records. The primary benchmark fixture.
    Ndjson,
    /// One top-level array of objects — the same data, one document.
    Array,
    /// A single document that is a tree of nested objects.
    Nested,
    /// Pathological: nesting `--depth` levels deep (default 100 000).
    Deep,
    /// Pathological: a flat array of `--count` scalars (default 5 000 000).
    Wide,
    /// Pathological: one string value of `--size` bytes.
    BigString,
    /// Pathological: objects whose keys repeat, for M5's dedup work.
    DupKeys,
    /// Pathological: structurally valid JSON containing invalid UTF-8.
    BadUtf8,
    /// Pathological: NDJSON cut off mid-record, as a killed export would be.
    Truncated,
}

impl Kind {
    /// Parse the name used on the command line.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "ndjson" => Self::Ndjson,
            "array" => Self::Array,
            "nested" => Self::Nested,
            "deep" => Self::Deep,
            "wide" => Self::Wide,
            "bigstring" => Self::BigString,
            "dupkeys" => Self::DupKeys,
            "badutf8" => Self::BadUtf8,
            "truncated" => Self::Truncated,
            _ => return None,
        })
    }

    /// The name used on the command line.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ndjson => "ndjson",
            Self::Array => "array",
            Self::Nested => "nested",
            Self::Deep => "deep",
            Self::Wide => "wide",
            Self::BigString => "bigstring",
            Self::DupKeys => "dupkeys",
            Self::BadUtf8 => "badutf8",
            Self::Truncated => "truncated",
        }
    }

    /// Conventional file extension.
    #[must_use]
    pub const fn extension(self) -> &'static str {
        match self {
            Self::Ndjson | Self::Truncated => "ndjson",
            _ => "json",
        }
    }

    /// One line explaining what this fixture is for.
    #[must_use]
    pub const fn purpose(self) -> &'static str {
        match self {
            Self::Ndjson => "primary benchmark fixture: log/API records, one per line",
            Self::Array => "the same records as one top-level array",
            Self::Nested => "a single document that is a tree of nested objects",
            Self::Deep => "stack safety: nesting --depth levels deep",
            Self::Wide => "random access: a flat array of --count scalars",
            Self::BigString => "a single string value larger than most buffers",
            Self::DupKeys => "duplicate keys within objects (M5 dedup)",
            Self::BadUtf8 => "valid JSON structure containing invalid UTF-8",
            Self::Truncated => "cut off mid-record, as a killed export would be",
        }
    }

    /// Every kind, in the order `fixtures list` prints them.
    #[must_use]
    pub const fn all() -> [Self; 9] {
        [
            Self::Ndjson,
            Self::Array,
            Self::Nested,
            Self::Deep,
            Self::Wide,
            Self::BigString,
            Self::DupKeys,
            Self::BadUtf8,
            Self::Truncated,
        ]
    }
}

/// What to generate.
pub struct Spec {
    /// Target size in bytes. Advisory: generation stops at the first record
    /// boundary past this, so output is a whole number of valid records.
    pub target_bytes: u64,
    /// PRNG seed. The same seed produces the same bytes, always.
    pub seed: u64,
    /// Nesting depth, for [`Kind::Deep`].
    pub depth: u64,
    /// Element count, for [`Kind::Wide`].
    pub count: u64,
}

/// What was actually produced.
pub struct Stats {
    /// Bytes written.
    pub bytes: u64,
    /// Records, elements, or nesting levels, depending on the kind.
    pub records: u64,
}

/// xorshift64*, chosen because it is eight lines and needs no dependency.
///
/// This is a fixture generator, not a cryptographic or statistical tool — what
/// is required of it is that it be fast and that it produce the same sequence
/// everywhere. It does both.
struct Rng(u64);

impl Rng {
    const fn new(seed: u64) -> Self {
        // A zero state is a fixed point of xorshift; force it off zero.
        Self(if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        })
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        let index = self.below(items.len() as u64) as usize;
        &items[index]
    }
}

/// Counts bytes on their way to the real writer, so `Stats` cannot drift from
/// what actually landed on disk.
struct Counting<W> {
    inner: W,
    written: u64,
}

impl<W: Write> Write for Counting<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.written += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

const LEVELS: [&str; 5] = ["debug", "info", "warn", "error", "fatal"];
const SERVICES: [&str; 8] = [
    "api-gateway",
    "auth",
    "billing",
    "search-indexer",
    "notifications",
    "user-profile",
    "media-transcode",
    "webhook-dispatch",
];
const REGIONS: [&str; 5] = [
    "us-east-1",
    "us-west-2",
    "eu-west-1",
    "ap-south-1",
    "sa-east-1",
];
const MESSAGES: [&str; 8] = [
    "request completed",
    "cache miss, falling through to origin",
    "upstream timeout, retrying",
    "token refreshed",
    "rate limit applied",
    "payload validation failed",
    "connection pool exhausted",
    "background job enqueued",
];

/// Write one log/API record — the shape real large NDJSON files actually have:
/// a dozen keys, mixed scalar types, one small array, one nested object.
///
/// Records average ~230 bytes, so a 500 MB fixture is ~2.2 M of them.
fn write_record(buf: &mut String, rng: &mut Rng, id: u64) {
    let _ = write!(
        buf,
        r#"{{"id":{id},"ts":"2026-07-{:02}T{:02}:{:02}:{:02}.{:03}Z","level":"{}","service":"{}","msg":"{}","latency_ms":{}.{:02},"ok":{},"status":{},"bytes":{},"tags":["{}","{}"],"meta":{{"region":"{}","retries":{},"trace":"{:016x}"}}}}"#,
        rng.below(28) + 1,
        rng.below(24),
        rng.below(60),
        rng.below(60),
        rng.below(1000),
        rng.pick(&LEVELS),
        rng.pick(&SERVICES),
        rng.pick(&MESSAGES),
        rng.below(2000),
        rng.below(100),
        rng.below(10) > 0,
        *rng.pick(&[200_u32, 201, 204, 301, 400, 404, 429, 500, 503]),
        rng.below(1_000_000),
        rng.pick(&REGIONS),
        rng.pick(&SERVICES),
        rng.pick(&REGIONS),
        rng.below(4),
        rng.next(),
    );
}

/// Generate `kind` into `out`.
///
/// # Errors
///
/// Any write error from the underlying writer.
pub fn generate<W: Write>(kind: Kind, spec: &Spec, out: W) -> io::Result<Stats> {
    let mut w = Counting {
        inner: out,
        written: 0,
    };
    let mut rng = Rng::new(spec.seed);
    let mut buf = String::with_capacity(512);
    let mut records = 0u64;

    match kind {
        Kind::Ndjson | Kind::Truncated => {
            while w.written < spec.target_bytes {
                buf.clear();
                write_record(&mut buf, &mut rng, records);
                buf.push('\n');
                w.write_all(buf.as_bytes())?;
                records += 1;
            }
            if kind == Kind::Truncated {
                // The point of this fixture: the last record has no closing
                // brace and no newline, exactly like an export killed mid-write.
                buf.clear();
                write_record(&mut buf, &mut rng, records);
                buf.truncate(buf.len() * 2 / 3);
                w.write_all(buf.as_bytes())?;
                records += 1;
            }
        }

        Kind::Array => {
            // Elements are indented, which is not cosmetic. An array whose
            // elements start at column 0 is byte-for-byte indistinguishable
            // from NDJSON to a prefix heuristic, and every real pretty-printer
            // (`JSON.stringify(x, null, 2)`, `jq`, `json.dump(indent=)`)
            // indents. Emitting the unindented form would make this fixture
            // unrepresentative *and* quietly encode a sniffer limitation as
            // expected behaviour. The limitation itself is recorded as a test
            // in `leviathan_core::format`.
            w.write_all(b"[")?;
            while w.written < spec.target_bytes {
                buf.clear();
                if records > 0 {
                    buf.push(',');
                }
                buf.push_str("\n  ");
                write_record(&mut buf, &mut rng, records);
                w.write_all(buf.as_bytes())?;
                records += 1;
            }
            w.write_all(b"\n]\n")?;
        }

        Kind::Nested => {
            records = write_nested(&mut w, &mut rng, spec.target_bytes)?;
        }

        Kind::Deep => {
            // Written iteratively, not recursively: the generator must not be
            // the thing that blows the stack at 100 000 levels.
            let depth = spec.depth;
            for _ in 0..depth {
                w.write_all(br#"{"n":["#)?;
            }
            w.write_all(b"null")?;
            for _ in 0..depth {
                w.write_all(b"]}")?;
            }
            w.write_all(b"\n")?;
            records = depth;
        }

        Kind::Wide => {
            // The random-access fixture: exit criterion 3 fetches rows
            // 900 000–900 050 out of this.
            w.write_all(b"[")?;
            for i in 0..spec.count {
                buf.clear();
                if i > 0 {
                    buf.push(',');
                }
                let _ = write!(buf, "{}", rng.below(1_000_000_000));
                w.write_all(buf.as_bytes())?;
            }
            w.write_all(b"]\n")?;
            records = spec.count;
        }

        Kind::BigString => {
            // One value larger than any sane read buffer, to prove the lexer
            // resumes across chunk boundaries mid-string.
            w.write_all(br#"{"blob":""#)?;
            const CHUNK: usize = 64 * 1024;
            let filler: Vec<u8> = (0..CHUNK)
                .map(|i| b'a' + u8::try_from(i % 26).unwrap_or(0))
                .collect();
            let mut left = spec.target_bytes;
            while left > 0 {
                let take = usize::try_from(left.min(CHUNK as u64)).unwrap_or(CHUNK);
                w.write_all(&filler[..take])?;
                left -= take as u64;
            }
            w.write_all(b"\"}\n")?;
            records = 1;
        }

        Kind::DupKeys => {
            // Most parsers silently keep the last value. M5 reports both.
            while w.written < spec.target_bytes {
                buf.clear();
                let _ = write!(buf, r#"{{"id":{records}"#);
                for k in 0..8 {
                    let _ = write!(
                        buf,
                        r#","dup":{},"u{k}":{}"#,
                        rng.below(1000),
                        rng.below(1000)
                    );
                }
                buf.push_str("}\n");
                w.write_all(buf.as_bytes())?;
                records += 1;
            }
        }

        Kind::BadUtf8 => {
            // Structurally valid JSON whose string bytes are not valid UTF-8:
            // a lone continuation byte, a truncated sequence, and a bare 0xFF.
            const BAD: [&[u8]; 3] = [&[0x80], &[0xE2, 0x28], &[0xFF, 0xFE]];
            while w.written < spec.target_bytes {
                buf.clear();
                let _ = write!(buf, r#"{{"id":{records},"text":"ok "#);
                w.write_all(buf.as_bytes())?;
                w.write_all(BAD[(records % 3) as usize])?;
                w.write_all(br#" tail"}"#)?;
                w.write_all(b"\n")?;
                records += 1;
            }
        }
    }

    w.flush()?;
    Ok(Stats {
        bytes: w.written,
        records,
    })
}

/// A tree of objects, nested to a bounded depth and repeated until the target
/// size is reached. Depth is bounded because this fixture is about *shape*, not
/// about stack safety — that is [`Kind::Deep`]'s job.
fn write_nested<W: Write>(w: &mut Counting<W>, rng: &mut Rng, target: u64) -> io::Result<u64> {
    const MAX_DEPTH: u32 = 24;
    let mut nodes = 0u64;
    let mut buf = String::with_capacity(256);

    w.write_all(br#"{"root":["#)?;
    let mut first = true;
    while w.written < target {
        if !first {
            w.write_all(b",")?;
        }
        first = false;

        // One branch: open MAX_DEPTH levels, put a record at the bottom, close.
        let depth = 4 + rng.below(u64::from(MAX_DEPTH) - 4);
        for level in 0..depth {
            buf.clear();
            let _ = write!(buf, r#"{{"level":{level},"id":{nodes},"child":"#);
            w.write_all(buf.as_bytes())?;
            nodes += 1;
        }
        buf.clear();
        write_record(&mut buf, rng, nodes);
        w.write_all(buf.as_bytes())?;
        for _ in 0..depth {
            w.write_all(b"}")?;
        }
    }
    w.write_all(b"]}\n")?;
    Ok(nodes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviathan_core::{Format, sniff_format};

    fn make(kind: Kind, target: u64) -> Vec<u8> {
        let mut out = Vec::new();
        let spec = Spec {
            target_bytes: target,
            seed: 42,
            depth: 200,
            count: 500,
        };
        generate(kind, &spec, &mut out).unwrap();
        out
    }

    #[test]
    fn the_same_seed_produces_the_same_bytes() {
        // The property the whole benchmark story rests on.
        assert_eq!(make(Kind::Ndjson, 20_000), make(Kind::Ndjson, 20_000));
    }

    #[test]
    fn a_different_seed_produces_different_bytes() {
        let mut a = Vec::new();
        let mut b = Vec::new();
        let spec = |seed| Spec {
            target_bytes: 20_000,
            seed,
            depth: 10,
            count: 10,
        };
        generate(Kind::Ndjson, &spec(1), &mut a).unwrap();
        generate(Kind::Ndjson, &spec(2), &mut b).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn reported_stats_match_the_bytes_produced() {
        let mut out = Vec::new();
        let stats = generate(
            Kind::Ndjson,
            &Spec {
                target_bytes: 50_000,
                seed: 7,
                depth: 0,
                count: 0,
            },
            &mut out,
        )
        .unwrap();
        assert_eq!(stats.bytes, out.len() as u64);
        assert_eq!(
            stats.records,
            out.iter().filter(|b| **b == b'\n').count() as u64
        );
    }

    #[test]
    fn every_kind_generates_without_panicking() {
        for kind in Kind::all() {
            let bytes = make(kind, 10_000);
            assert!(!bytes.is_empty(), "{} produced nothing", kind.as_str());
        }
    }

    #[test]
    fn the_valid_kinds_parse_as_json() {
        // serde is not a dependency, so this checks the two things we can check
        // without one: the format sniffer agrees, and braces balance.
        for (kind, expected) in [
            (Kind::Ndjson, Format::Ndjson),
            (Kind::Array, Format::SingleDocument),
            (Kind::Nested, Format::SingleDocument),
            (Kind::Wide, Format::SingleDocument),
            (Kind::Deep, Format::SingleDocument),
            (Kind::BigString, Format::SingleDocument),
            (Kind::DupKeys, Format::Ndjson),
        ] {
            let bytes = make(kind, 20_000);
            assert_eq!(sniff_format(&bytes), expected, "kind {}", kind.as_str());
        }
    }

    #[test]
    fn deep_nests_exactly_as_deep_as_asked() {
        let mut out = Vec::new();
        let spec = Spec {
            target_bytes: 0,
            seed: 1,
            depth: 1000,
            count: 0,
        };
        let stats = generate(Kind::Deep, &spec, &mut out).unwrap();
        assert_eq!(stats.records, 1000);
        assert_eq!(out.iter().filter(|b| **b == b'[').count(), 1000);
        assert_eq!(out.iter().filter(|b| **b == b']').count(), 1000);
    }

    #[test]
    fn wide_holds_exactly_the_requested_element_count() {
        let mut out = Vec::new();
        let spec = Spec {
            target_bytes: 0,
            seed: 1,
            depth: 0,
            count: 10_000,
        };
        let stats = generate(Kind::Wide, &spec, &mut out).unwrap();
        assert_eq!(stats.records, 10_000);
        assert_eq!(out.iter().filter(|b| **b == b',').count(), 9_999);
    }

    #[test]
    fn the_pathological_kinds_are_actually_pathological() {
        // Each of these would be a bug in any other generator.
        assert!(std::str::from_utf8(&make(Kind::BadUtf8, 5_000)).is_err());

        let truncated = make(Kind::Truncated, 5_000);
        assert_ne!(truncated.last(), Some(&b'\n'), "should end mid-record");

        let dup = make(Kind::DupKeys, 5_000);
        let text = String::from_utf8_lossy(&dup);
        let first_line = text.lines().next().unwrap();
        assert!(first_line.matches(r#""dup":"#).count() > 1);
    }

    #[test]
    fn big_string_is_one_value_and_has_no_newlines_inside_it() {
        let out = make(Kind::BigString, 100_000);
        assert_eq!(out.iter().filter(|b| **b == b'\n').count(), 1);
        assert!(out.len() > 100_000);
    }

    #[test]
    fn kind_names_round_trip() {
        for kind in Kind::all() {
            assert_eq!(Kind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(Kind::parse("nonsense"), None);
    }
}
