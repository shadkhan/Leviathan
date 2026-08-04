//! RFC 8259 conformance against an external corpus.
//!
//! Runs the engine's accept/reject predicate over a [JSONTestSuite][suite]
//! checkout — ~300 files chosen by people who spent a long time thinking about
//! where JSON parsers go wrong — and prints a table suitable for pasting into
//! the README.
//!
//! [suite]: https://github.com/nst/JSONTestSuite
//!
//! The corpus is not vendored. It is someone else's repository with its own
//! licence and its own history, and a copy in this tree would be a copy that
//! goes stale. `crates/leviathan-core/tests/conformance.rs` holds a committed
//! subset so that a clean clone still proves conformance offline; this command
//! is the wider net, run deliberately.
//!
//! ## The naming convention, which is the whole harness
//!
//! JSONTestSuite encodes the expected verdict in the filename:
//!
//! | Prefix | Meaning | Failing here means |
//! |---|---|---|
//! | `y_` | must be accepted | we reject valid JSON — a **correctness bug** |
//! | `n_` | must be rejected | we accept invalid JSON — a **laxity bug** |
//! | `i_` | implementation-defined | nothing; the choice is recorded, not judged |
//!
//! `i_` cases are counted and listed rather than scored, because scoring them
//! would be inventing a right answer the specification declines to give.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use leviathan_core::{Documents, Lexer, Structure, Validate, ValidateOptions, sniff_format};

/// Cases where this engine knowingly disagrees with the corpus.
///
/// Not a way to make the gate green. An entry here is a claim that the
/// deviation is deliberate, and it is checked in **both** directions: an
/// undocumented disagreement fails the run, and so does an entry that no longer
/// deviates — a stale exemption is a lie about the engine that nothing else
/// would catch.
///
/// All three entries are the same decision. RFC 8259 requires a JSON text to
/// contain a value, so an empty file is not valid JSON. Leviathan opens it
/// anyway: `sniff_format` reports `empty`, the viewer says so, and nothing
/// pretends a value was found. Refusing to open a zero-byte file — a truncated
/// export, an interrupted download — would be the "it won't open" failure this
/// product exists to replace (DEEP_REASONING C6).
///
/// These are deviations of the **opener**, and only of the opener. Since M3,
/// `Validate` answers the other question separately and correctly: an empty
/// document reports "no JSON value" at offset 0. Opening a zero-byte file and
/// calling it valid JSON would have been one predicate answering two questions;
/// now each is answered where it belongs.
const KNOWN_DEVIATIONS: &[(&str, &str)] = &[
    ("n_structure_no_data.json", "empty input opens as `empty`"),
    (
        "n_single_space.json",
        "whitespace-only input opens as `empty`",
    ),
    (
        "n_structure_UTF8_BOM_no_data.json",
        "a BOM with no value opens as `empty`",
    ),
];

fn documented(name: &str) -> Option<&'static str> {
    KNOWN_DEVIATIONS
        .iter()
        .find(|(case, _)| *case == name)
        .map(|(_, why)| *why)
}

/// What the engine decided about one file.
struct Case {
    name: String,
    expected: Expect,
    accepted: bool,
    why: Option<String>,
    /// Why the reported location is unusable, for a rejected case.
    unlocated: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Expect {
    Accept,
    Reject,
    Either,
}

impl Case {
    /// Whether the verdict disagrees with the filename's claim.
    const fn disagrees(&self) -> bool {
        match self.expected {
            Expect::Accept => !self.accepted,
            Expect::Reject => self.accepted,
            Expect::Either => false,
        }
    }
}

/// Whether a rejected case also reported a *usable* location.
///
/// M3's exit criterion asks that every `n_` case locate its failure. The corpus
/// carries no ground-truth offsets, so "within ±1 byte of the true failure
/// point" cannot be checked automatically against it — what can be checked, on
/// all 188 of them, is that a location exists, lies inside the file, and is
/// 1-based. Exact offsets are asserted separately in the core's own corpus,
/// where the right answer is known because the case was written for it.
///
/// Returns the complaint, if the location is unusable.
fn locates(bytes: &[u8]) -> Option<String> {
    let mut pass = Validate::new(sniff_format(bytes));
    let mut source = bytes;
    let options = ValidateOptions::default();
    let mut spins = 0;

    while !pass.is_done() {
        if pass.advance(&mut source, &options).is_err() {
            return Some("the source could not be read".to_string());
        }
        spins += 1;
        if spins > 10_000 {
            return Some("validation did not terminate".to_string());
        }
    }

    let Some(first) = pass.errors().first() else {
        return Some("rejected, but validation found nothing to report".to_string());
    };
    if first.offset > bytes.len() as u64 {
        return Some(format!(
            "offset {} is past the end of a {}-byte file",
            first.offset,
            bytes.len()
        ));
    }
    if first.line == 0 || first.column == 0 {
        return Some(format!(
            "positions must be 1-based: line {} column {}",
            first.line, first.column
        ));
    }
    if first.message.is_empty() {
        return Some("the error has no message".to_string());
    }
    None
}

/// Whether the engine accepts `bytes` as one well-formed JSON document.
///
/// The same predicate as `bench walk` and as the core's committed corpus: lex,
/// walk the grammar in single-document mode, flush the final token, close.
fn accepts(bytes: &[u8]) -> Result<(), String> {
    let mut lexer = Lexer::new();
    let mut structure = Structure::new(Documents::One);

    for token in lexer.feed(bytes) {
        let token = token.map_err(|e| e.to_string())?;
        structure.push(token).map_err(|e| e.to_string())?;
    }
    if let Some(token) = lexer.finish().map_err(|e| e.to_string())? {
        structure.push(token).map_err(|e| e.to_string())?;
    }
    structure.finish().map_err(|e| e.to_string())?;
    Ok(())
}

/// Find the directory of `.json` cases inside whatever the user pointed at.
///
/// Accepts the repository root, the `test_parsing` directory itself, or any
/// directory of files named by the convention — being fussy about which of
/// those the user typed would serve nothing.
fn case_directory(root: &Path) -> io::Result<PathBuf> {
    for candidate in [root.join("test_parsing"), root.to_path_buf()] {
        if candidate.is_dir() {
            let has_cases = fs::read_dir(&candidate)?.filter_map(Result::ok).any(|e| {
                e.path().extension().is_some_and(|x| x == "json")
                    && prefix_of(&e.file_name().to_string_lossy()).is_some()
            });
            if has_cases {
                return Ok(candidate);
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!(
            "no y_/n_/i_ cases under {}\n\nget the corpus with:\n    \
             git clone --depth 1 https://github.com/nst/JSONTestSuite",
            root.display()
        ),
    ))
}

fn prefix_of(name: &str) -> Option<Expect> {
    match name.get(..2) {
        Some("y_") => Some(Expect::Accept),
        Some("n_") => Some(Expect::Reject),
        Some("i_") => Some(Expect::Either),
        _ => None,
    }
}

/// Run every case under `root` and render the report.
///
/// # Errors
///
/// The directory cannot be read, or holds no recognizable cases.
pub fn run(root: &Path) -> io::Result<(String, bool)> {
    let dir = case_directory(root)?;

    let mut cases: Vec<Case> = Vec::new();
    for entry in fs::read_dir(&dir)? {
        let path = entry?.path();
        if path.extension().is_none_or(|x| x != "json") {
            continue;
        }
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let Some(expected) = prefix_of(&name) else {
            continue;
        };

        let bytes = fs::read(&path)?;
        let outcome = accepts(&bytes);
        // A rejected case owes a location as well as a verdict (M3).
        let unlocated = if outcome.is_err() {
            locates(&bytes)
        } else {
            None
        };
        cases.push(Case {
            name,
            expected,
            accepted: outcome.is_ok(),
            why: outcome.err(),
            unlocated,
        });
    }

    cases.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(report(&cases, &dir))
}

/// Build the report, and say whether the run passed.
fn report(cases: &[Case], dir: &Path) -> (String, bool) {
    use std::fmt::Write as _;

    let count = |e: Expect| cases.iter().filter(|c| c.expected == e).count();
    let passed = |e: Expect| {
        cases
            .iter()
            .filter(|c| c.expected == e && !c.disagrees())
            .count()
    };

    let accepted_i = cases
        .iter()
        .filter(|c| c.expected == Expect::Either && c.accepted)
        .count();
    let total_i = count(Expect::Either);

    // Three buckets, not two. A documented deviation is expected; an
    // undocumented one is a bug; and an exemption that no longer applies is a
    // stale claim about the engine, which is its own kind of wrong.
    let (deviations, unexpected): (Vec<&Case>, Vec<&Case>) = cases
        .iter()
        .filter(|c| c.disagrees())
        .partition(|c| documented(&c.name).is_some());
    let stale: Vec<&str> = KNOWN_DEVIATIONS
        .iter()
        .map(|(name, _)| *name)
        .filter(|name| cases.iter().any(|c| c.name == *name && !c.disagrees()))
        .collect();

    let mut out = String::new();
    let _ = writeln!(out, "leviathan conformance\ncorpus: {}\n", dir.display());
    let _ = writeln!(
        out,
        "  class  meaning                          passed        \n  {}",
        "-".repeat(54)
    );
    let _ = writeln!(
        out,
        "  y_     must be accepted                 {:>4} / {:<4}",
        passed(Expect::Accept),
        count(Expect::Accept)
    );
    let _ = writeln!(
        out,
        "  n_     must be rejected                 {:>4} / {:<4}",
        passed(Expect::Reject),
        count(Expect::Reject)
    );
    let _ = writeln!(
        out,
        "  i_     implementation-defined           {:>4} accepted of {}",
        accepted_i, total_i
    );

    // M3: a rejection without a location is a log line, not a feature.
    let rejected = cases.iter().filter(|c| !c.accepted).count();
    let unlocated: Vec<&Case> = cases.iter().filter(|c| c.unlocated.is_some()).collect();
    let _ = writeln!(
        out,
        "  loc    every rejection locatable        {:>4} / {:<4}",
        rejected - unlocated.len(),
        rejected
    );

    if unexpected.is_empty() && stale.is_empty() {
        let _ = writeln!(
            out,
            "\n  no undocumented disagreements: every y_ case parsed, every n_ case\n  \
             was refused except the {} documented below, and every rejection\n  \
             reported a location inside the file.",
            deviations.len()
        );
    }

    if !unexpected.is_empty() {
        let _ = writeln!(
            out,
            "\n  {} UNDOCUMENTED disagreement(s):\n",
            unexpected.len()
        );
        for case in &unexpected {
            let verdict = if case.accepted {
                "accepted, should have been rejected".to_string()
            } else {
                format!(
                    "rejected, should have been accepted — {}",
                    case.why.as_deref().unwrap_or("no reason given")
                )
            };
            let _ = writeln!(out, "    {:<44} {verdict}", case.name);
        }
    }

    if !unlocated.is_empty() {
        let _ = writeln!(
            out,
            "
  {} rejection(s) with no usable location:
",
            unlocated.len()
        );
        for case in &unlocated {
            let _ = writeln!(
                out,
                "    {:<44} {}",
                case.name,
                case.unlocated.as_deref().unwrap_or("")
            );
        }
    }

    if !stale.is_empty() {
        let _ = writeln!(
            out,
            "\n  {} STALE exemption(s) — these now agree with the corpus and must be\n  \
             removed from KNOWN_DEVIATIONS:\n",
            stale.len()
        );
        for name in &stale {
            let _ = writeln!(out, "    {name}");
        }
    }

    if !deviations.is_empty() {
        let _ = writeln!(out, "\n  documented deviations:\n");
        for case in &deviations {
            let _ = writeln!(
                out,
                "    {:<44} {}",
                case.name,
                documented(&case.name).unwrap_or("")
            );
        }
    }

    // The i_ list is printed in full: these are decisions, and a decision that
    // changes silently between releases is the thing this command exists to
    // prevent.
    if total_i > 0 {
        let _ = writeln!(out, "\n  implementation-defined cases, and our answer:\n");
        for case in cases.iter().filter(|c| c.expected == Expect::Either) {
            let _ = writeln!(
                out,
                "    {:<44} {}",
                case.name,
                if case.accepted { "accept" } else { "reject" }
            );
        }
    }

    (
        out,
        unexpected.is_empty() && stale.is_empty() && unlocated.is_empty(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a corpus directory on the fly, so the runner is tested without the
    /// external repository being present.
    fn corpus(files: &[(&str, &[u8])]) -> (tempdir::TempDir, PathBuf) {
        let dir = tempdir::TempDir::new("conformance");
        let path = dir.path().to_path_buf();
        for (name, bytes) in files {
            fs::write(path.join(name), bytes).unwrap();
        }
        (dir, path)
    }

    #[test]
    fn a_clean_corpus_passes() {
        let (_dir, path) = corpus(&[
            ("y_ok.json", b"[1,2]"),
            ("n_bad.json", b"[1,]"),
            ("i_choice.json", b"[1E400]"),
        ]);

        let (text, ok) = run(&path).unwrap();
        assert!(ok, "expected a pass:\n{text}");
        assert!(text.contains("1 / 1"), "y_ and n_ both counted:\n{text}");
        assert!(text.contains("no undocumented disagreements"), "{text}");
        assert!(
            text.contains("i_choice.json"),
            "i_ cases are listed:\n{text}"
        );
    }

    #[test]
    fn a_wrongly_accepted_case_fails_the_run() {
        // The laxity bug this command exists to catch: `n_` means the parser
        // must refuse, and a harness that reports success anyway is worthless.
        let (_dir, path) = corpus(&[("n_actually_fine.json", b"[1]")]);

        let (text, ok) = run(&path).unwrap();
        assert!(!ok, "a wrongly accepted case must fail the run:\n{text}");
        assert!(text.contains("should have been rejected"), "{text}");
    }

    #[test]
    fn a_wrongly_rejected_case_reports_the_reason() {
        let (_dir, path) = corpus(&[("y_actually_broken.json", b"[1,]")]);

        let (text, ok) = run(&path).unwrap();
        assert!(!ok);
        assert!(text.contains("should have been accepted"), "{text}");
        // Without the parser's own message a failure is a filename and a shrug.
        assert!(text.contains("—"), "the reason is included:\n{text}");
    }

    #[test]
    fn a_documented_deviation_does_not_fail_the_run() {
        // Empty input: the corpus says reject, this engine opens it as `empty`
        // on purpose. The gate must stay green, and must still say so out loud.
        let (_dir, path) = corpus(&[("n_structure_no_data.json", b"")]);

        let (text, ok) = run(&path).unwrap();
        assert!(ok, "a documented deviation is not a failure:\n{text}");
        assert!(text.contains("documented deviations"), "{text}");
        assert!(
            text.contains("opens as `empty`"),
            "the reason is shown:\n{text}"
        );
    }

    #[test]
    fn an_exemption_that_no_longer_applies_fails_the_run() {
        // The half everyone forgets. If the engine starts rejecting empty input,
        // the exemption becomes a false claim about the engine — and nothing
        // else in the suite would ever notice.
        let (_dir, path) = corpus(&[("n_single_space.json", b"[1,]")]);

        let (text, ok) = run(&path).unwrap();
        assert!(!ok, "a stale exemption must fail the run:\n{text}");
        assert!(text.contains("STALE exemption"), "{text}");
    }

    #[test]
    fn files_not_named_by_the_convention_are_ignored() {
        let (_dir, path) = corpus(&[("y_ok.json", b"[1]"), ("README.json", b"not json at all")]);

        let (text, ok) = run(&path).unwrap();
        assert!(ok, "{text}");
    }

    #[test]
    fn a_directory_without_cases_says_how_to_get_them() {
        let (_dir, path) = corpus(&[("notes.txt", b"nothing here")]);

        let error = run(&path).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(
            error.to_string().contains("JSONTestSuite"),
            "the error should say where the corpus comes from: {error}"
        );
    }

    /// A directory that deletes itself, so the tests leave nothing behind.
    mod tempdir {
        use std::path::{Path, PathBuf};

        pub struct TempDir(PathBuf);

        impl TempDir {
            pub fn new(tag: &str) -> Self {
                let mut path = std::env::temp_dir();
                let unique = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0);
                path.push(format!("leviathan-{tag}-{unique}"));
                std::fs::create_dir_all(&path).unwrap();
                Self(path)
            }

            pub fn path(&self) -> &Path {
                &self.0
            }
        }

        impl Drop for TempDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }
}
