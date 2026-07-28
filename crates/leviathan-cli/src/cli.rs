//! A small hand-rolled argument parser, and the units the CLI speaks in.
//!
//! Still no `clap`. The reason has changed, though: at M0 it was that a
//! dependency would have been most of the binary. Now it is that `cargo install
//! leviathan-cli` is a shipping promise (SPEC §M7), and a zero-dependency tool
//! installs in seconds on any machine, including the one a reviewer is
//! reproducing benchmarks on. This file is the entire cost of that: ~80 lines,
//! tested, and it does exactly what the four subcommands need.
//!
//! It handles `--flag value`, `--flag=value`, and bare positionals. It does not
//! handle short flags, clustering, or subcommand-specific help text — when it
//! needs to, that is the moment to reconsider.

use std::fmt::Write as _;

/// Parsed command line: positionals in order, flags by name.
pub struct Args {
    positional: Vec<String>,
    flags: Vec<(String, String)>,
}

impl Args {
    /// Parse `--name value`, `--name=value`, and positionals.
    ///
    /// # Errors
    ///
    /// A `--flag` at the end of the line with nothing to consume.
    pub fn parse(raw: &[String]) -> Result<Self, String> {
        let mut positional = Vec::new();
        let mut flags = Vec::new();
        let mut it = raw.iter();

        while let Some(arg) = it.next() {
            let Some(name) = arg.strip_prefix("--") else {
                positional.push(arg.clone());
                continue;
            };
            match name.split_once('=') {
                Some((name, value)) => flags.push((name.to_string(), value.to_string())),
                None => {
                    // A bare `--flag` with no value is a boolean; it only
                    // consumes the next argument if that argument is not
                    // itself a flag.
                    let value = match it.as_slice().first() {
                        Some(next) if !next.starts_with("--") => {
                            it.next();
                            next.clone()
                        }
                        _ => String::new(),
                    };
                    flags.push((name.to_string(), value));
                }
            }
        }
        Ok(Self { flags, positional })
    }

    /// The `n`-th positional argument.
    #[must_use]
    pub fn positional(&self, n: usize) -> Option<&str> {
        self.positional.get(n).map(String::as_str)
    }

    /// All positional arguments from `n` onward.
    #[must_use]
    pub fn positionals_from(&self, n: usize) -> &[String] {
        self.positional.get(n..).unwrap_or(&[])
    }

    /// Is `--name` present, in any form?
    #[must_use]
    pub fn has(&self, name: &str) -> bool {
        self.flags.iter().any(|(flag, _)| flag == name)
    }

    /// The value of `--name`, if it was given one.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&str> {
        self.flags
            .iter()
            .find(|(flag, _)| flag == name)
            .map(|(_, value)| value.as_str())
            .filter(|value| !value.is_empty())
    }

    /// Parse `--name` as a byte size, falling back to `default`.
    ///
    /// # Errors
    ///
    /// The value is not a size this CLI understands.
    pub fn size(&self, name: &str, default: u64) -> Result<u64, String> {
        match self.get(name) {
            Some(raw) => parse_size(raw)
                .ok_or_else(|| format!("--{name}: not a size: {raw} (try 500MB, 1GiB, 1_000_000)")),
            None => Ok(default),
        }
    }

    /// Parse `--name` as a count, falling back to `default`.
    ///
    /// # Errors
    ///
    /// The value is not a number.
    pub fn count(&self, name: &str, default: u64) -> Result<u64, String> {
        match self.get(name) {
            Some(raw) => parse_size(raw).ok_or_else(|| format!("--{name}: not a number: {raw}")),
            None => Ok(default),
        }
    }

    /// Reject flags the subcommand does not know.
    ///
    /// A silently ignored `--sixe 500MB` produces a benchmark run against the
    /// wrong fixture and a number nobody can reproduce. Better to stop.
    ///
    /// # Errors
    ///
    /// Any flag not in `known`.
    pub fn reject_unknown(&self, known: &[&str]) -> Result<(), String> {
        for (flag, _) in &self.flags {
            if !known.contains(&flag.as_str()) {
                let mut message = format!("unknown option: --{flag}\n\nthis command accepts:");
                for name in known {
                    let _ = write!(message, " --{name}");
                }
                return Err(message);
            }
        }
        Ok(())
    }
}

/// Parse a byte size: `1024`, `1_000_000`, `64KiB`, `500MB`, `1.5GB`.
///
/// Both conventions are accepted and they mean different things, as they should:
/// `MB` is 10⁶ and `MiB` is 2²⁰. Fixture sizes are quoted in the decimal units
/// people say out loud ("a 500 MB file"), while buffers are quoted in binary
/// ones.
#[must_use]
pub fn parse_size(raw: &str) -> Option<u64> {
    let text = raw.trim().replace('_', "");
    let digits_end = text
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(text.len());
    let (number, suffix) = text.split_at(digits_end);

    let value: f64 = number.parse().ok()?;
    if !value.is_finite() || value < 0.0 {
        return None;
    }

    let multiplier: f64 = match suffix.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1.0,
        "k" | "kb" => 1e3,
        "m" | "mb" => 1e6,
        "g" | "gb" => 1e9,
        "kib" => 1024.0,
        "mib" => 1024.0 * 1024.0,
        "gib" => 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };

    let bytes = value * multiplier;
    // Sizes beyond this are a typo, not a request. Catching it here beats
    // discovering it when the disk fills.
    if bytes > 1e12 {
        return None;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Some(bytes as u64)
}

/// Render a byte count the way the benchmark table wants it.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [(u64, &str); 4] = [
        (1_000_000_000, "GB"),
        (1_000_000, "MB"),
        (1_000, "kB"),
        (1, "B"),
    ];
    for (scale, unit) in UNITS {
        if bytes >= scale {
            return if scale == 1 {
                format!("{bytes} B")
            } else {
                format!("{:.1} {unit}", bytes as f64 / scale as f64)
            };
        }
    }
    "0 B".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(raw: &[&str]) -> Args {
        Args::parse(&raw.iter().map(|s| (*s).to_string()).collect::<Vec<_>>()).unwrap()
    }

    #[test]
    fn parses_both_flag_forms() {
        let a = args(&["--size", "500MB", "--out=x.json"]);
        assert_eq!(a.get("size"), Some("500MB"));
        assert_eq!(a.get("out"), Some("x.json"));
    }

    #[test]
    fn parses_positionals() {
        let a = args(&["ndjson", "extra", "--size", "1MB"]);
        assert_eq!(a.positional(0), Some("ndjson"));
        assert_eq!(a.positional(1), Some("extra"));
        assert_eq!(a.positional(2), None);
    }

    #[test]
    fn a_bare_flag_is_boolean_and_does_not_eat_the_next_flag() {
        let a = args(&["--json", "--seed", "7"]);
        assert!(a.has("json"));
        assert_eq!(a.get("json"), None);
        assert_eq!(a.get("seed"), Some("7"));
    }

    #[test]
    fn unknown_flags_are_rejected() {
        // The typo that would otherwise silently produce an unreproducible run.
        assert!(
            args(&["--sixe", "500MB"])
                .reject_unknown(&["size"])
                .is_err()
        );
        assert!(args(&["--size", "500MB"]).reject_unknown(&["size"]).is_ok());
    }

    #[test]
    fn decimal_and_binary_suffixes_differ() {
        assert_eq!(parse_size("1MB"), Some(1_000_000));
        assert_eq!(parse_size("1MiB"), Some(1_048_576));
        assert_eq!(parse_size("1kb"), Some(1_000));
        assert_eq!(parse_size("1KiB"), Some(1_024));
    }

    #[test]
    fn sizes_accept_plain_numbers_and_separators() {
        assert_eq!(parse_size("1024"), Some(1024));
        assert_eq!(parse_size("1_000_000"), Some(1_000_000));
        assert_eq!(parse_size("1.5GB"), Some(1_500_000_000));
        assert_eq!(parse_size(" 500MB "), Some(500_000_000));
    }

    #[test]
    fn absurd_or_malformed_sizes_are_rejected() {
        assert_eq!(parse_size("banana"), None);
        assert_eq!(parse_size("12parsecs"), None);
        assert_eq!(parse_size(""), None);
        assert_eq!(parse_size("-5MB"), None);
        assert_eq!(parse_size("999GB"), Some(999_000_000_000)); // large, but sane
        assert_eq!(parse_size("9999GB"), None); // past the 1 TB sanity limit
        assert_eq!(parse_size("2TB"), None); // TB is not a suffix we accept
    }

    #[test]
    fn human_bytes_reads_the_way_the_table_wants() {
        assert_eq!(human_bytes(500_000_000), "500.0 MB");
        assert_eq!(human_bytes(1_500_000_000), "1.5 GB");
        assert_eq!(human_bytes(64_000), "64.0 kB");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(0), "0 B");
    }
}
