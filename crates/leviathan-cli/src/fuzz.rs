//! Robustness fuzzing for the lexer and the grammar walk.
//!
//! SPEC's M1 exit criterion asks for 30 minutes of fuzzing with no panic.
//! `cargo-fuzz` is the usual answer and is not available here: it needs
//! libFuzzer and a nightly toolchain, and it does not support the
//! `x86_64-pc-windows-msvc` target this project is developed on. Rather than
//! make the criterion conditional on a platform, the fuzzer is written the way
//! everything else in this crate is written — a seeded xorshift, no
//! dependencies, and the same sequence on every machine forever (C18).
//!
//! ## What it looks for
//!
//! A panic is the obvious thing and the least likely. `#![forbid(unsafe_code)]`
//! and a state machine that never indexes without bounds mean the interesting
//! failures are not crashes but **disagreements**, and the harness checks four
//! invariants on every input:
//!
//! 1. **No panic.** Caught rather than fatal, so the failing input is reported
//!    instead of being lost in a stack trace.
//! 2. **Chunk invariance.** The same bytes must produce the same verdict
//!    whether fed one byte at a time, seven at a time, or whole. This is the
//!    property a streaming lexer exists to have and the one it can silently
//!    lose — a resumed state machine that mishandles a boundary inside a
//!    `\uD83D` escape fails here and nowhere else.
//! 3. **Positions are inside the input.** An error offset past the end of the
//!    file, or a zero line number, is a caret pointing at nothing — and M3's
//!    whole value is that the caret points at the right byte.
//! 4. **Spans are ordered and bounded.** `start <= end <= len`, and tokens
//!    arrive in non-decreasing order.
//!
//! ## Why mutation, not just random bytes
//!
//! Uniformly random bytes are rejected within a few bytes and exercise almost
//! nothing. Most of the budget therefore goes on **mutating valid JSON** —
//! flipping a bit, deleting a byte, truncating, duplicating a span — which is
//! how a fuzzer reaches the states a parser actually has: a nearly-good escape,
//! a number missing its exponent digits, a string whose closing quote became a
//! backslash.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::{Duration, Instant};

use leviathan_core::{Documents, Lexer, Structure};

/// Valid documents to mutate. Small, and between them they use every token kind,
/// every escape, multi-byte UTF-8, surrogate pairs and nesting.
const SEEDS: &[&str] = &[
    r#"{"a":1}"#,
    r#"[1,2,3]"#,
    r#"{"name":"leviathan","tags":[1,2,3],"ok":true,"n":null}"#,
    r#"["`Īካ","😀","\b\f\n\r\t\/\\\""]"#,
    r#"[0,-0,1e10,-1.5E-3,123.456,1E22]"#,
    r#"{"deep":{"deeper":{"deepest":[{"x":[[[]]]}]}}}"#,
    r#"{"é":"€","emoji":"😀"}"#,
    r#"[{"a":[]},{"b":{}},"",0,false]"#,
    "\u{feff}[1]",
    r#"42"#,
];

/// One case's verdict, reduced to what must be reproducible.
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    Accepted,
    Rejected,
}

/// What went wrong, if anything.
struct Failure {
    case: u64,
    input: Vec<u8>,
    detail: String,
}

impl Failure {
    /// A reproduction recipe, not just a complaint.
    fn render(&self, seed: u64) -> String {
        let hex: String = self
            .input
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join("");
        format!(
            "fuzz failure at case {case} (seed {seed})\n  {detail}\n  {len} bytes: {hex}\n  \
             as text: {text:?}",
            case = self.case,
            detail = self.detail,
            len = self.input.len(),
            text = String::from_utf8_lossy(&self.input),
        )
    }
}

/// Run the engine over `bytes`, fed `chunk` at a time, and report the verdict.
///
/// Returns the verdict plus the first position reported, so positions can be
/// bounds-checked without a second run.
fn exercise(bytes: &[u8], chunk: usize) -> Result<Verdict, String> {
    let mut lexer = Lexer::new();
    let mut structure = Structure::new(Documents::One);
    let mut previous_start = 0u64;

    for piece in bytes.chunks(chunk.max(1)) {
        for token in lexer.feed(piece) {
            match token {
                Ok(token) => {
                    if token.start > token.end {
                        return Err(format!(
                            "token span is inverted: {}..{}",
                            token.start, token.end
                        ));
                    }
                    if token.end > bytes.len() as u64 {
                        return Err(format!(
                            "token ends past the input: {} > {}",
                            token.end,
                            bytes.len()
                        ));
                    }
                    if token.start < previous_start {
                        return Err(format!(
                            "tokens went backwards: {} after {}",
                            token.start, previous_start
                        ));
                    }
                    previous_start = token.start;

                    if structure.push(token).is_err() {
                        return Ok(Verdict::Rejected);
                    }
                }
                Err(error) => {
                    let at = error.at;
                    if at.offset > bytes.len() as u64 {
                        return Err(format!(
                            "error offset past the input: {} > {}",
                            at.offset,
                            bytes.len()
                        ));
                    }
                    if at.line == 0 || at.column == 0 {
                        return Err(format!(
                            "positions are 1-based: line {} column {}",
                            at.line, at.column
                        ));
                    }
                    return Ok(Verdict::Rejected);
                }
            }
        }
    }

    match lexer.finish() {
        Ok(Some(token)) => {
            if structure.push(token).is_err() {
                return Ok(Verdict::Rejected);
            }
        }
        Ok(None) => {}
        Err(_) => return Ok(Verdict::Rejected),
    }

    Ok(match structure.finish() {
        Ok(()) => Verdict::Accepted,
        Err(_) => Verdict::Rejected,
    })
}

/// Check every invariant for one input.
fn check(case: u64, input: &[u8]) -> Option<Failure> {
    let fail = |detail: String| {
        Some(Failure {
            case,
            input: input.to_vec(),
            detail,
        })
    };

    // The panic hook is silenced by the caller, so a caught panic is reported
    // once, with its input, rather than as a wall of stack traces.
    let whole = match catch_unwind(AssertUnwindSafe(|| exercise(input, input.len().max(1)))) {
        Ok(result) => result,
        Err(_) => return fail("panicked while lexing the whole input".to_string()),
    };
    let whole = match whole {
        Ok(verdict) => verdict,
        Err(why) => return fail(why),
    };

    for chunk in [1usize, 7] {
        let piecemeal = match catch_unwind(AssertUnwindSafe(|| exercise(input, chunk))) {
            Ok(result) => result,
            Err(_) => return fail(format!("panicked at chunk size {chunk}")),
        };
        match piecemeal {
            Ok(verdict) if verdict == whole => {}
            Ok(verdict) => {
                return fail(format!(
                    "chunk size {chunk} disagreed: whole={whole:?} chunked={verdict:?}"
                ));
            }
            Err(why) => return fail(why),
        }
    }
    None
}

/// How this input was produced. Reported so a run that finds nothing still says
/// what it actually exercised.
#[derive(Default)]
struct Tally {
    random: u64,
    mutated: u64,
    accepted: u64,
    rejected: u64,
}

/// Fuzz for `budget`, starting from `seed`.
///
/// # Errors
///
/// Returns the failing case, rendered with a reproduction recipe.
pub fn run(seed: u64, budget: Duration, limit: u64) -> Result<String, String> {
    let mut rng = Rng::new(seed);
    let mut tally = Tally::default();
    let mut buffer: Vec<u8> = Vec::new();

    // A panic in a fuzz case is data, not a crash — but the default hook would
    // print a stack trace for every one. Silenced for the duration, restored
    // after, so the report is the report.
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));

    let started = Instant::now();
    let mut case = 0u64;
    let mut failure = None;

    while started.elapsed() < budget && case < limit {
        case += 1;
        buffer.clear();

        // Three quarters mutation, one quarter noise. Pure random bytes are
        // rejected almost immediately and exercise the first branch of the
        // lexer and nothing else; mutations of valid input reach the states a
        // parser actually has.
        if rng.below(4) == 0 {
            tally.random += 1;
            let len = rng.below(64) as usize;
            for _ in 0..len {
                buffer.push((rng.next() & 0xFF) as u8);
            }
        } else {
            tally.mutated += 1;
            let seed_doc = SEEDS[rng.below(SEEDS.len() as u64) as usize];
            buffer.extend_from_slice(seed_doc.as_bytes());
            let mutations = 1 + rng.below(3);
            for _ in 0..mutations {
                mutate(&mut buffer, &mut rng);
            }
        }

        if let Some(found) = check(case, &buffer) {
            failure = Some(found);
            break;
        }

        // Cheap enough to recompute: the accepted/rejected split is what proves
        // the corpus is not all garbage.
        match exercise(&buffer, buffer.len().max(1)) {
            Ok(Verdict::Accepted) => tally.accepted += 1,
            _ => tally.rejected += 1,
        }
    }

    let elapsed = started.elapsed();
    std::panic::set_hook(previous);

    if let Some(found) = failure {
        return Err(found.render(seed));
    }

    let per_second = if elapsed.as_secs_f64() > 0.0 {
        case as f64 / elapsed.as_secs_f64()
    } else {
        0.0
    };

    Ok(format!(
        "leviathan fuzz\n  seed {seed}, {elapsed:.1?}\n\n  \
         cases          {case} ({per_second:.0}/s)\n  \
         mutated        {mutated}\n  \
         random bytes   {random}\n  \
         accepted       {accepted}\n  \
         rejected       {rejected}\n\n  \
         no panics, and every input produced the same verdict at chunk sizes\n  \
         1, 7 and whole.",
        mutated = tally.mutated,
        random = tally.random,
        accepted = tally.accepted,
        rejected = tally.rejected,
    ))
}

/// Apply one mutation in place.
///
/// The operations are chosen to produce *nearly* valid input: a flipped bit in
/// an escape, a truncated number, a duplicated span that unbalances a bracket.
fn mutate(buffer: &mut Vec<u8>, rng: &mut Rng) {
    if buffer.is_empty() {
        buffer.push(b'[');
        return;
    }
    let at = rng.below(buffer.len() as u64) as usize;

    match rng.below(6) {
        // Flip a bit.
        0 => buffer[at] ^= 1 << (rng.below(8) as u8),
        // Replace with a byte that means something to JSON.
        1 => {
            const INTERESTING: &[u8] = b"{}[]\",:\\/eE+-.0123456789 \n\t\0\x7f\xff\xc3\x80";
            buffer[at] = INTERESTING[rng.below(INTERESTING.len() as u64) as usize];
        }
        // Delete a byte.
        2 => {
            buffer.remove(at);
        }
        // Insert a byte.
        3 => buffer.insert(at, (rng.next() & 0xFF) as u8),
        // Truncate — the case a killed export produces, and the one that found
        // the missing-flush bug three times (C30, C37).
        4 => buffer.truncate(at),
        // Duplicate a span.
        _ => {
            let end = (at + 1 + rng.below(8) as usize).min(buffer.len());
            let span: Vec<u8> = buffer[at..end].to_vec();
            let insert_at = rng.below(buffer.len() as u64 + 1) as usize;
            for (offset, byte) in span.into_iter().enumerate() {
                buffer.insert(insert_at + offset, byte);
            }
        }
    }
}

/// xorshift64*, the same generator the fixtures use, for the same reason: the
/// run must be reproducible from its seed on any machine.
struct Rng(u64);

impl Rng {
    const fn new(seed: u64) -> Self {
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
        if n == 0 { 0 } else { self.next() % n }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_run_finds_nothing_and_says_what_it_did() {
        // Also the CI gate: this runs on every `cargo test`, so a regression
        // that breaks chunk invariance fails the build rather than waiting for
        // someone to run the fuzzer by hand.
        let report = run(1, Duration::from_millis(400), 4000).expect("no failure expected");
        assert!(report.contains("no panics"), "{report}");
        assert!(report.contains("mutated"), "{report}");
    }

    #[test]
    fn the_same_seed_produces_the_same_run() {
        // A fuzzer whose failures cannot be reproduced is a rumour generator.
        let a = run(42, Duration::from_secs(60), 300).unwrap();
        let b = run(42, Duration::from_secs(60), 300).unwrap();
        let strip = |s: String| {
            s.lines()
                .filter(|l| !l.contains("/s)") && !l.contains("seed"))
                .collect::<Vec<_>>()
                .join("\n")
        };
        assert_eq!(strip(a), strip(b), "the same seed must fuzz identically");
    }

    #[test]
    fn different_seeds_explore_differently() {
        let a = run(1, Duration::from_secs(60), 300).unwrap();
        let b = run(2, Duration::from_secs(60), 300).unwrap();
        assert_ne!(a, b, "two seeds should not produce identical corpora");
    }

    #[test]
    fn the_corpus_is_not_all_garbage() {
        // A fuzzer that only ever produces invalid input tests the first branch
        // of the lexer and nothing else. Some mutations must still parse.
        let report = run(7, Duration::from_secs(60), 2000).unwrap();
        let accepted: u64 = report
            .lines()
            .find_map(|l| l.trim().strip_prefix("accepted"))
            .and_then(|rest| rest.trim().parse().ok())
            .unwrap_or(0);
        assert!(
            accepted > 0,
            "no mutated input survived as valid:\n{report}"
        );
    }

    #[test]
    fn a_verdict_disagreement_is_reported_with_its_input() {
        // Prove the reporting path, since the invariant checks are only worth
        // having if a failure says which bytes caused it.
        let failure = Failure {
            case: 12,
            input: b"[1,".to_vec(),
            detail: "chunk size 7 disagreed".to_string(),
        };
        let text = failure.render(99);
        assert!(text.contains("case 12"), "{text}");
        assert!(text.contains("seed 99"), "{text}");
        assert!(text.contains("5b312c"), "the bytes are in hex: {text}");
        assert!(text.contains("[1,"), "and as text: {text}");
    }

    #[test]
    fn every_seed_document_is_valid_json() {
        // The mutation corpus is only meaningful if what it mutates parses.
        for doc in SEEDS {
            assert_eq!(
                exercise(doc.as_bytes(), doc.len()),
                Ok(Verdict::Accepted),
                "seed document should be valid: {doc}"
            );
        }
    }
}
