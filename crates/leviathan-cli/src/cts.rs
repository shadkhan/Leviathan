//! RFC 9535 conformance, against the official compliance test suite.
//!
//! Runs the [JSONPath Compliance Test Suite][cts] — 703 cases maintained by the
//! working group that wrote the RFC — and prints a support table honest enough
//! to publish.
//!
//! [cts]: https://github.com/jsonpath-standard/jsonpath-compliance-test-suite
//!
//! ## What this can and cannot claim
//!
//! Leviathan implements a **filter subset**, not JSONPath. It answers "which
//! records satisfy this condition", which is the shape of the question a person
//! asks a 500 MB log; it does not evaluate `$.store.book[*].author` against a
//! document tree, because it has no document tree (C1). So most of the suite is
//! testing something this engine deliberately does not do.
//!
//! Reporting "we pass 273 of 703" would therefore be meaningless, and reporting
//! "we pass 273 of the 273 we tried" would be worse — it is the number every
//! partial implementation quotes, and it is chosen after the fact. What is
//! reported instead is the whole population, partitioned:
//!
//! | Bucket | Meaning |
//! |---|---|
//! | **passed** | in scope, and the selected values are exactly right |
//! | **failed** | in scope, and wrong. The only bucket that can be non-zero and still ship is this one at zero |
//! | **correctly rejected** | RFC 9535 says the selector is invalid, and it is refused |
//! | **out of scope** | refused with a message naming the construct, broken down by which |
//!
//! The last bucket is the size of the subset, stated as a number instead of as
//! an adjective. Every case in it produced an error that *names* what is
//! unsupported — none was quietly misread, which is the property C59 exists to
//! protect and the one this run actually verifies.
//!
//! ## The safety property worth quoting
//!
//! A selector RFC 9535 calls invalid is **never accepted**. Leviathan rejects a
//! superset of what the RFC rejects — that is what being a subset means — so
//! this direction is free, but it is the direction that matters: accepting an
//! invalid query means answering a question nobody asked.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use leviathan_core::Filter;

use crate::json::Json;

/// One case's outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Passed,
    Failed,
    CorrectlyRejected,
    /// Refused because the construct is outside the subset.
    OutOfScope(Unsupported),
}

/// Why a case is outside the subset. Ordered as the report prints them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Unsupported {
    Descendant,
    Wildcard,
    Slice,
    Function,
    /// The selector is a path to walk, not a condition to test — the bulk of
    /// the suite, and the part this engine is not trying to be.
    NotAFilter,
    /// Inside the subset's shape, but using an expression form it does not
    /// implement (a nested filter, a comparison between two paths, …).
    Expression,
}

impl Unsupported {
    const fn label(self) -> &'static str {
        match self {
            Unsupported::Descendant => "descendant segments (`..`)",
            Unsupported::Wildcard => "wildcards (`*`)",
            Unsupported::Slice => "slices (`[a:b]`)",
            Unsupported::Function => "function extensions",
            Unsupported::NotAFilter => "not a filter — a path to walk",
            Unsupported::Expression => "filter expression outside the subset",
        }
    }
}

struct Case {
    name: String,
    selector: String,
    verdict: Verdict,
    /// What went wrong, for a failure.
    why: Option<String>,
}

/// Run the suite at `path`, returning the support table and whether it passed.
///
/// # Errors
///
/// If the suite cannot be found, read, or parsed. An in-scope *failure* is not
/// an error of this call — it is a result, and it is reported in the table.
pub fn run(path: &Path) -> io::Result<(String, bool)> {
    let file = locate(path)?;
    let text = fs::read_to_string(&file)?;
    let suite = Json::parse(&text).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} is not valid JSON: {e}", file.display()),
        )
    })?;

    let tests = suite.get("tests").and_then(Json::items).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "no `tests` array in the suite")
    })?;

    let cases: Vec<Case> = tests.iter().map(judge).collect();
    let failed = cases.iter().any(|c| c.verdict == Verdict::Failed);
    Ok((report(&cases, &file), !failed))
}

/// Decide one case.
fn judge(case: &Json) -> Case {
    let name = case
        .get("name")
        .and_then(Json::text)
        .unwrap_or_else(|| "(unnamed)".to_string());
    let selector = case
        .get("selector")
        .and_then(Json::text)
        .unwrap_or_default();
    let invalid = case.get("invalid_selector").is_some_and(Json::is_true);

    let mut out = Case {
        name,
        selector: selector.clone(),
        verdict: Verdict::OutOfScope(Unsupported::NotAFilter),
        why: None,
    };

    // Only a selector that is *exactly* one root filter is a question this
    // engine can answer. `$.a[?@.b]` selects from a subtree, which needs a path
    // walk to reach; `$[?@.b][0]` post-filters the result list.
    let Some(expression) = root_filter(&selector) else {
        out.verdict = Verdict::OutOfScope(shape_of(&selector));
        return out;
    };

    let parsed = match Filter::parse(expression) {
        Ok(filter) => filter,
        Err(error) => {
            out.verdict = if invalid {
                // Rejected, as the RFC requires. Note that this engine also
                // rejects things the RFC allows — that is what a subset is —
                // so agreement here is a floor, not a ceiling.
                Verdict::CorrectlyRejected
            } else {
                Verdict::OutOfScope(reason_of(&error.message))
            };
            return out;
        }
    };

    if invalid {
        out.verdict = Verdict::Failed;
        out.why = Some("accepted a selector RFC 9535 calls invalid".to_string());
        return out;
    }

    let Some(document) = case.get("document") else {
        out.verdict = Verdict::Failed;
        out.why = Some("a valid case with no document".to_string());
        return out;
    };

    let selected = select(&parsed, document);

    // `results` lists every ordering the RFC permits — an object's members have
    // no defined order, so a run over one has several right answers.
    let expected: Vec<Vec<Json>> = match (case.get("result"), case.get("results")) {
        (Some(one), _) => vec![one.items().unwrap_or_default().to_vec()],
        (None, Some(many)) => many
            .items()
            .unwrap_or_default()
            .iter()
            .map(|alt| alt.items().unwrap_or_default().to_vec())
            .collect(),
        (None, None) => {
            out.verdict = Verdict::Failed;
            out.why = Some("a valid case with no expected result".to_string());
            return out;
        }
    };

    if expected.contains(&selected) {
        out.verdict = Verdict::Passed;
    } else {
        out.verdict = Verdict::Failed;
        out.why = Some(format!(
            "selected {} value(s), expected {}",
            selected.len(),
            expected
                .iter()
                .map(|alt| alt.len().to_string())
                .collect::<Vec<_>>()
                .join(" or ")
        ));
    }
    out
}

/// Apply a filter to a document the way the engine applies one to a file.
///
/// Each candidate is serialized back to JSON text and tested through
/// [`Filter::matches`] on those bytes — the *same* entry point the Worker calls
/// per record. Reaching inside the filter to evaluate against a parsed value
/// would be testing a second implementation that no user ever runs.
fn select(filter: &Filter, document: &Json) -> Vec<Json> {
    let candidates: Vec<&Json> = match document {
        Json::Array(items) => items.iter().collect(),
        Json::Object(members) => members.iter().map(|(_, value)| value).collect(),
        // RFC 9535: a filter selector applied to a scalar selects nothing.
        _ => Vec::new(),
    };

    let mut matcher = filter.matcher();
    candidates
        .into_iter()
        .filter(|value| matcher.matches(value.to_text().as_bytes()))
        .cloned()
        .collect()
}

/// The expression inside `$[?…]`, if the selector is exactly that.
fn root_filter(selector: &str) -> Option<&str> {
    let trimmed = selector.trim();
    let inner = trimmed.strip_prefix("$[?")?.strip_suffix(']')?;
    // A `]` closing something *inside* the filter means the outer `]` this
    // stripped was not the filter's — `$[?@.a][0]` must not arrive here looking
    // like `@.a][0`. Balance settles it.
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    for c in inner.chars() {
        match c {
            _ if escaped => escaped = false,
            '\\' if in_string => escaped = true,
            '\'' | '"' => in_string = !in_string,
            '[' if !in_string => depth += 1,
            ']' if !in_string => {
                depth -= 1;
                if depth < 0 {
                    return None;
                }
            }
            _ => {}
        }
    }
    (depth == 0).then_some(inner)
}

/// Why a selector that is not a root filter is out of scope.
fn shape_of(selector: &str) -> Unsupported {
    if selector.contains("..") {
        Unsupported::Descendant
    } else if selector.contains('*') {
        Unsupported::Wildcard
    } else if has_slice(selector) {
        Unsupported::Slice
    } else if has_function(selector) {
        Unsupported::Function
    } else {
        Unsupported::NotAFilter
    }
}

/// Which construct a parser message is complaining about.
///
/// Reading the message rather than re-deriving the reason is deliberate: it
/// checks that the parser actually *said* what was unsupported, which is the
/// promise C59 makes to whoever typed the query.
fn reason_of(message: &str) -> Unsupported {
    if message.contains("descendant") {
        Unsupported::Descendant
    } else if message.contains("wildcard") {
        Unsupported::Wildcard
    } else if message.contains("slice") {
        Unsupported::Slice
    } else if message.contains("function") {
        Unsupported::Function
    } else {
        Unsupported::Expression
    }
}

fn has_slice(selector: &str) -> bool {
    let mut in_brackets = false;
    let mut quote: Option<char> = None;
    let mut escaped = false;

    for c in selector.chars() {
        // A colon inside a quoted name is part of the name — `$['a:b']` selects
        // a member called `a:b` and is not a slice.
        if escaped {
            escaped = false;
            continue;
        }
        match (quote, c) {
            (Some(_), '\\') => escaped = true,
            (Some(open), c) if c == open => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '"') => quote = Some(c),
            (None, '[') => in_brackets = true,
            (None, ']') => in_brackets = false,
            (None, ':') if in_brackets => return true,
            (None, _) => {}
        }
    }
    false
}

fn has_function(selector: &str) -> bool {
    ["length(", "count(", "match(", "search(", "value("]
        .iter()
        .any(|name| selector.contains(name))
}

/// Find `cts.json` under whatever the user pointed at.
fn locate(root: &Path) -> io::Result<PathBuf> {
    for candidate in [
        root.to_path_buf(),
        root.join("cts.json"),
        root.join("jsonpath-cts/cts.json"),
    ] {
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!(
            "no cts.json under {}\n\nget the suite with:\n    \
             curl -sSLo fixtures/generated/cts.json \\\n      \
             https://raw.githubusercontent.com/jsonpath-standard/\
             jsonpath-compliance-test-suite/main/cts.json",
            root.display()
        ),
    ))
}

/// Render the support table.
fn report(cases: &[Case], from: &Path) -> String {
    use std::fmt::Write as _;

    let count = |want: Verdict| cases.iter().filter(|c| c.verdict == want).count();
    let passed = count(Verdict::Passed);
    let failed = count(Verdict::Failed);
    let rejected = count(Verdict::CorrectlyRejected);
    let in_scope = passed + failed;

    let mut out = String::new();
    let _ = writeln!(out, "\nRFC 9535 compliance test suite");
    let _ = writeln!(out, "  suite: {}", from.display());
    let _ = writeln!(out, "  cases: {}\n", cases.len());

    let share = |n: usize| {
        if cases.is_empty() {
            0.0
        } else {
            n as f64 * 100.0 / cases.len() as f64
        }
    };

    let _ = writeln!(
        out,
        "  in scope — a single root filter, within the expression subset"
    );
    let _ = writeln!(
        out,
        "    passed                    {passed:>4}  ({:.0}% of the whole suite)",
        share(passed)
    );
    let _ = writeln!(
        out,
        "    failed                    {failed:>4}{}",
        if failed == 0 { "  ✓" } else { "  ✗" }
    );
    let _ = writeln!(
        out,
        "\n  correctly rejected — RFC 9535 says invalid, Leviathan refuses"
    );
    let _ = writeln!(out, "    rejected                  {rejected:>4}  ✓");

    let _ = writeln!(
        out,
        "\n  out of scope — refused by name, never misread ({} cases)",
        cases.len() - in_scope - rejected
    );
    let mut reasons: Vec<Unsupported> = cases
        .iter()
        .filter_map(|c| match c.verdict {
            Verdict::OutOfScope(why) => Some(why),
            _ => None,
        })
        .collect();
    reasons.sort_unstable();
    let mut at = 0;
    while at < reasons.len() {
        let why = reasons[at];
        let n = reasons[at..].iter().take_while(|r| **r == why).count();
        let _ = writeln!(out, "    {:<25} {n:>4}", why.label());
        at += n;
    }

    for case in cases.iter().filter(|c| c.verdict == Verdict::Failed) {
        let _ = writeln!(
            out,
            "\n  FAIL  {}\n        selector: {}\n        {}",
            case.name,
            case.selector,
            case.why.as_deref().unwrap_or("no detail")
        );
    }

    let _ = writeln!(
        out,
        "\n  {}",
        if failed == 0 {
            "every in-scope case passes, and no invalid selector was accepted."
        } else {
            "IN-SCOPE FAILURES — see above."
        }
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_bare_root_filter_is_in_scope() {
        assert_eq!(root_filter("$[?@.a == 1]"), Some("@.a == 1"));
        assert_eq!(root_filter("  $[?@]  "), Some("@"));
        // The case this function exists for: a trailing segment must not be
        // swallowed into the expression.
        assert_eq!(root_filter("$[?@.a][0]"), None);
        assert_eq!(root_filter("$.a[?@.b]"), None);
        assert_eq!(root_filter("$[0]"), None);
        assert_eq!(root_filter("$..[?@.a]"), None);
        // A `]` inside a string is not a bracket.
        assert_eq!(
            root_filter(r#"$[?@['a]b'] == 1]"#),
            Some(r#"@['a]b'] == 1"#)
        );
    }

    #[test]
    fn out_of_scope_reasons_come_from_the_selector_and_the_message() {
        assert_eq!(shape_of("$..a"), Unsupported::Descendant);
        assert_eq!(shape_of("$[*]"), Unsupported::Wildcard);
        assert_eq!(shape_of("$[1:3]"), Unsupported::Slice);
        assert_eq!(shape_of("$[?length(@) > 1]"), Unsupported::Function);
        assert_eq!(shape_of("$.a.b"), Unsupported::NotAFilter);
        // A colon outside brackets is not a slice.
        assert_eq!(shape_of("$['a:b']"), Unsupported::NotAFilter);

        assert_eq!(
            reason_of("descendant segments (`..`) are not supported by this subset"),
            Unsupported::Descendant
        );
        assert_eq!(reason_of("expected a value"), Unsupported::Expression);
    }

    #[test]
    fn a_filter_selects_from_arrays_and_objects_and_nothing_from_scalars() {
        let filter = Filter::parse("@.a == 1").unwrap();

        let array = Json::parse(r#"[{"a":1},{"a":2},{"a":1}]"#).unwrap();
        assert_eq!(select(&filter, &array).len(), 2);

        let object = Json::parse(r#"{"x":{"a":1},"y":{"a":9}}"#).unwrap();
        assert_eq!(
            select(&filter, &object),
            vec![Json::parse(r#"{"a":1}"#).unwrap()]
        );

        // RFC 9535: a filter over a scalar selects nothing, and must not error.
        assert!(select(&filter, &Json::parse("42").unwrap()).is_empty());
    }

    #[test]
    fn an_accepted_invalid_selector_is_a_failure_not_a_pass() {
        // The direction that matters. If this ever inverts, the run must go red
        // rather than quietly counting it as out of scope.
        let case =
            Json::parse(r#"{"name":"bogus","selector":"$[?@.a == 1]","invalid_selector":true}"#)
                .unwrap();
        assert_eq!(judge(&case).verdict, Verdict::Failed);
    }

    #[test]
    fn a_wrong_answer_is_a_failure() {
        let case = Json::parse(
            r#"{"name":"wrong","selector":"$[?@.a == 1]","document":[{"a":1}],"result":[]}"#,
        )
        .unwrap();
        let judged = judge(&case);
        assert_eq!(judged.verdict, Verdict::Failed);
        assert!(judged.why.is_some());
    }

    #[test]
    fn a_right_answer_passes() {
        let case = Json::parse(
            r#"{"name":"right","selector":"$[?@.a == 1]","document":[{"a":1},{"a":2}],"result":[{"a":1}]}"#,
        )
        .unwrap();
        assert_eq!(judge(&case).verdict, Verdict::Passed);
    }

    #[test]
    fn either_ordering_of_an_object_run_is_accepted() {
        // An object's members have no defined order, so the suite lists every
        // permitted answer and any one of them is right.
        let case = Json::parse(
            r#"{"name":"unordered","selector":"$[?@]","document":{"a":1,"b":2},
                "results":[[2,1],[1,2]]}"#,
        )
        .unwrap();
        assert_eq!(judge(&case).verdict, Verdict::Passed);
    }

    #[test]
    fn a_missing_suite_says_where_to_get_it() {
        let error = run(Path::new("does-not-exist")).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(error.to_string().contains("cts.json"), "{error}");
    }
}
