//! Filter expressions: the conditional half of search.
//!
//! Find answers "where does this string occur". This answers "which records
//! satisfy this condition" — `@.status == "error" && @.latency_ms > 1000` — and
//! the two are different tools for different moments. You reach for find on a
//! file whose shape you do not know yet, and for this once you do.
//!
//! ## The syntax is RFC 9535's, and a subset of it
//!
//! Using the standard rather than inventing one means the syntax is already
//! known, `Copy path` already emits something compatible, and there is an
//! official compliance suite to be measured against rather than a README claim.
//!
//! Supported: `@.a.b`, `@['a']`, `@[0]`, the comparisons `== != < <= > >=`,
//! `&&`, `||`, `!`, parentheses, and existence (`@.a` alone). Literals are
//! strings, numbers, `true`, `false`, `null`.
//!
//! Not supported, and rejected rather than misread: descendant segments (`..`),
//! wildcards, slices, and the function extensions (`length()`, `match()`,
//! `search()`, `count()`). A query using one gets an error naming it, because
//! quietly evaluating `$..a == 1` as something else would be worse than
//! refusing.
//!
//! ## How a record is tested without being parsed
//!
//! The expression names a handful of paths. One walk over the record collects
//! exactly those, and the expression is then evaluated over what was collected.
//! Nothing is materialized: no value tree, no map of the record, just a slot per
//! path the query actually mentions.

use crate::lexer::{Lexer, Token, TokenKind};
use crate::rows::unescape;
use crate::structure::{Documents, Event, Structure};

/// How much of a string is compared. Longer than any sane comparand.
const MAX_TEXT: usize = 64 * 1024;

/// Why an expression could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterError {
    /// What is wrong, phrased for whoever typed it.
    pub message: String,
    /// Character offset into the expression, for a caret.
    pub at: usize,
}

impl core::fmt::Display for FilterError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{} (at character {})", self.message, self.at + 1)
    }
}

impl core::error::Error for FilterError {}

/// One step of a path inside a record.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Step {
    Key(String),
    Index(u64),
}

/// A comparison operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

/// A value, from the query or from the record.
#[derive(Debug, Clone, PartialEq)]
enum Value {
    Null,
    Bool(bool),
    Number(f64),
    Text(String),
    /// A container. Comparable only for existence.
    Container,
}

/// A parsed expression.
#[derive(Debug, Clone)]
enum Expr {
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Not(Box<Expr>),
    /// `@.a == 1`. The path is an index into [`Filter::paths`].
    Compare {
        path: usize,
        op: Op,
        value: Value,
    },
    /// `@.a` on its own: true when the path exists.
    Exists(usize),
}

/// A compiled filter expression.
#[derive(Debug, Clone)]
pub struct Filter {
    root: Expr,
    /// Every distinct path the expression mentions, deduplicated so one walk
    /// collects each at most once.
    paths: Vec<Vec<Step>>,
}

impl Filter {
    /// Parse an expression.
    ///
    /// Accepts the full JSONPath filter form `$[?<expr>]` and the bare `<expr>`,
    /// because the second is what a person types into a search box and the
    /// first is what they paste from documentation.
    ///
    /// # Errors
    ///
    /// If the expression does not parse, or uses a construct this subset does
    /// not implement — named explicitly, never silently reinterpreted.
    pub fn parse(source: &str) -> Result<Self, FilterError> {
        let trimmed = source.trim();
        let inner = trimmed
            .strip_prefix("$[?")
            .and_then(|rest| rest.strip_suffix(']'))
            .unwrap_or(trimmed);

        let mut parser = Parser {
            chars: inner.chars().collect(),
            at: 0,
            paths: Vec::new(),
        };
        parser.skip_spaces();
        let root = parser.parse_or()?;
        parser.skip_spaces();
        if parser.at < parser.chars.len() {
            return Err(parser.error("unexpected trailing text"));
        }
        Ok(Self {
            root,
            paths: parser.paths,
        })
    }

    /// How many distinct paths the expression reads.
    #[must_use]
    pub fn path_count(&self) -> usize {
        self.paths.len()
    }

    /// A reusable tester for this filter.
    ///
    /// Testing a record needs a lexer, a grammar walk, a path stack and a slot
    /// per referenced path. Over 1.7 million records those are 1.7 million
    /// allocations of each, so they are owned by the matcher and cleared between
    /// records rather than built and dropped per record. Anything running a
    /// filter over a file should hold one of these for the whole pass.
    #[must_use]
    pub fn matcher(&self) -> Matcher<'_> {
        Matcher {
            filter: self,
            found: vec![None; self.paths.len()],
            tokens: Vec::new(),
            here: Vec::new(),
            counts: Vec::new(),
            named: Vec::new(),
            scratch: String::new(),
        }
    }

    /// Whether `record` satisfies the expression.
    ///
    /// One pass over the record, collecting only the paths the expression
    /// mentions. A record that does not parse does not match — a filter is a
    /// question about content, and `Validate` is what reports broken syntax.
    ///
    /// Allocates its scratch on every call. Convenient for one record; for a
    /// file, hold a [`Matcher`](Filter::matcher).
    #[must_use]
    pub fn matches(&self, record: &[u8]) -> bool {
        self.matcher().matches(record)
    }

    fn evaluate(&self, expr: &Expr, found: &[Option<Value>]) -> bool {
        match expr {
            Expr::And(left, right) => self.evaluate(left, found) && self.evaluate(right, found),
            Expr::Or(left, right) => self.evaluate(left, found) || self.evaluate(right, found),
            Expr::Not(inner) => !self.evaluate(inner, found),
            Expr::Exists(path) => found[*path].is_some(),
            Expr::Compare { path, op, value } => match &found[*path] {
                // RFC 9535: a comparison with a path that does not exist is
                // false — including `!=`, which is why this is not `op == Ne`.
                None => false,
                Some(actual) => compare(actual, *op, value),
            },
        }
    }
}

/// One filter plus the scratch space to run it, reused across records.
///
/// Borrowed from the [`Filter`], which is immutable and can be shared; all the
/// mutable state of a pass lives here.
pub struct Matcher<'a> {
    filter: &'a Filter,
    /// One slot per referenced path: the value found, or nothing.
    found: Vec<Option<Value>>,
    tokens: Vec<Token>,
    /// The path of the value currently being visited.
    here: Vec<Cursor>,
    /// Element counts of the open arrays, for `[0]`, `[1]`, …
    counts: Vec<u64>,
    /// Whether each open container contributed a step to `here`. The record's
    /// own root value contributes none — its path is `@` itself.
    named: Vec<bool>,
    /// Reused buffer for the rare key that needs unescaping.
    scratch: String,
}

/// A step of the *current* position, as opposed to a step of a wanted path.
///
/// A key is a range into the record rather than a `String`, because the hot
/// path visits every key of every record and almost none of them are ones the
/// query asked about. Materializing 17 million strings to discard 17 million
/// strings is the cost this type exists to avoid.
#[derive(Debug, Clone, Copy)]
enum Cursor {
    /// A quoted key, including its quotes, as `start..end` within the record.
    Key(usize, usize),
    Index(u64),
}

impl Matcher<'_> {
    /// Whether `record` satisfies the filter.
    #[must_use]
    pub fn matches(&mut self, record: &[u8]) -> bool {
        self.collect(record);
        let matched = self.filter.evaluate(&self.filter.root, &self.found);
        // Cleared after the answer, not before the next call: a matcher that has
        // been asked once and then dropped should not be holding the record's
        // values alive.
        self.found.iter_mut().for_each(|slot| *slot = None);
        matched
    }

    /// Walk the record once, capturing the value at each wanted path.
    fn collect(&mut self, record: &[u8]) {
        let mut lexer = Lexer::new();
        let mut structure = Structure::new(Documents::One);
        self.tokens.clear();
        self.here.clear();
        self.counts.clear();
        self.named.clear();

        // Gathered rather than streamed so the final flush cannot be forgotten:
        // a number is the only token that needs the byte after it, so a record
        // of `42` yields nothing without it (C30, C37, and four more).
        for token in lexer.feed(record) {
            let Ok(token) = token else { return };
            self.tokens.push(token);
        }
        match lexer.finish() {
            Ok(Some(token)) => self.tokens.push(token),
            Ok(None) => {}
            Err(_) => return,
        }

        let mut pending_key: Option<Cursor> = None;

        for at in 0..self.tokens.len() {
            let token = self.tokens[at];
            let raw = span(record, token.start, token.end);
            let kind = token.kind;
            let Ok(event) = structure.push(token) else {
                return;
            };
            let Some(event) = event else { continue };

            match event {
                Event::Key { token, .. } => {
                    pending_key = Some(Cursor::Key(token.start as usize, token.end as usize));
                }
                Event::Scalar { .. } => {
                    let step = step_here(&mut self.counts, pending_key.take());
                    if let Some(step) = step {
                        self.here.push(step);
                    }
                    self.capture(record, value_of(kind, raw));
                    if step.is_some() {
                        self.here.pop();
                    }
                }
                Event::Open { .. } => {
                    let step = step_here(&mut self.counts, pending_key.take());
                    self.named.push(step.is_some());
                    if let Some(step) = step {
                        self.here.push(step);
                    }
                    self.capture(record, Value::Container);
                    self.counts.push(0);
                }
                Event::Close { .. } => {
                    self.counts.pop();
                    if self.named.pop() == Some(true) {
                        self.here.pop();
                    }
                }
            }
        }
    }

    /// Record `value` against every wanted path that names the current position.
    fn capture(&mut self, record: &[u8], value: Value) {
        for (slot, path) in self.filter.paths.iter().enumerate() {
            if self.found[slot].is_some() || path.len() != self.here.len() {
                continue;
            }
            let same = path
                .iter()
                .zip(&self.here)
                .all(|(want, at)| at.is(want, record, &mut self.scratch));
            if same {
                self.found[slot] = Some(value.clone());
            }
        }
    }
}

impl Cursor {
    /// Whether this position-step is the wanted path-step.
    fn is(self, want: &Step, record: &[u8], scratch: &mut String) -> bool {
        match (self, want) {
            (Cursor::Index(have), Step::Index(wanted)) => have == *wanted,
            (Cursor::Key(start, end), Step::Key(wanted)) => {
                let quoted = record.get(start..end).unwrap_or(&[]);
                let inner = quoted.get(1..quoted.len().saturating_sub(1)).unwrap_or(&[]);
                // The overwhelmingly common key has no escapes at all, and then
                // this is a byte comparison against the query's own bytes.
                if !inner.contains(&b'\\') {
                    return inner == wanted.as_bytes();
                }
                scratch.clear();
                scratch.push_str(&unescape(quoted, MAX_TEXT).0);
                scratch == wanted
            }
            _ => false,
        }
    }
}

/// The step naming the value about to be visited, or `None` for the record's
/// own root value — which is `@` itself and so adds nothing to the path.
///
/// Array indices come from the enclosing container's running count, which is why
/// this takes `counts` mutably: visiting an element is what advances it.
fn step_here(counts: &mut [u64], key: Option<Cursor>) -> Option<Cursor> {
    match key {
        Some(cursor) => Some(cursor),
        None => counts.last_mut().map(|next| {
            let index = *next;
            *next += 1;
            Cursor::Index(index)
        }),
    }
}

/// Compare a record value against a query literal.
///
/// Different types never compare true, per RFC 9535 §2.3.5 — `@.a > "1"` is
/// false for a numeric `a` rather than an error, because a filter over a
/// million heterogeneous records must not stop at the first surprise.
fn compare(actual: &Value, op: Op, expected: &Value) -> bool {
    let ordering = match (actual, expected) {
        (Value::Number(a), Value::Number(b)) => a.partial_cmp(b),
        (Value::Text(a), Value::Text(b)) => Some(a.as_str().cmp(b.as_str())),
        (Value::Bool(a), Value::Bool(b)) => Some(a.cmp(b)),
        (Value::Null, Value::Null) => Some(core::cmp::Ordering::Equal),
        _ => None,
    };

    let Some(ordering) = ordering else {
        // Not comparable: only `!=` can be true, and only because they differ.
        return op == Op::Ne;
    };

    match op {
        Op::Eq => ordering == core::cmp::Ordering::Equal,
        Op::Ne => ordering != core::cmp::Ordering::Equal,
        Op::Lt => ordering == core::cmp::Ordering::Less,
        Op::Le => ordering != core::cmp::Ordering::Greater,
        Op::Gt => ordering == core::cmp::Ordering::Greater,
        Op::Ge => ordering != core::cmp::Ordering::Less,
    }
}

fn value_of(kind: TokenKind, raw: &[u8]) -> Value {
    match kind {
        TokenKind::Null => Value::Null,
        TokenKind::True => Value::Bool(true),
        TokenKind::False => Value::Bool(false),
        TokenKind::Number { .. } => core::str::from_utf8(raw)
            .ok()
            .and_then(|text| text.parse().ok())
            .map_or(Value::Null, Value::Number),
        _ => Value::Text(unescape(raw, MAX_TEXT).0),
    }
}

fn span(bytes: &[u8], from: u64, to: u64) -> &[u8] {
    let start = from as usize;
    let end = (to as usize).min(bytes.len());
    bytes.get(start..end).unwrap_or(&[])
}

// ------------------------------------------------------------------ parsing

struct Parser {
    chars: Vec<char>,
    at: usize,
    paths: Vec<Vec<Step>>,
}

impl Parser {
    fn error(&self, message: &str) -> FilterError {
        FilterError {
            message: message.to_string(),
            at: self.at,
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.at).copied()
    }

    fn skip_spaces(&mut self) {
        while matches!(self.peek(), Some(c) if c.is_whitespace()) {
            self.at += 1;
        }
    }

    fn eat(&mut self, text: &str) -> bool {
        let chars: Vec<char> = text.chars().collect();
        if self.chars[self.at..].starts_with(&chars) {
            self.at += chars.len();
            return true;
        }
        false
    }

    fn parse_or(&mut self) -> Result<Expr, FilterError> {
        let mut left = self.parse_and()?;
        loop {
            self.skip_spaces();
            if self.eat("||") || self.eat("or ") {
                self.skip_spaces();
                let right = self.parse_and()?;
                left = Expr::Or(Box::new(left), Box::new(right));
            } else {
                return Ok(left);
            }
        }
    }

    fn parse_and(&mut self) -> Result<Expr, FilterError> {
        let mut left = self.parse_unary()?;
        loop {
            self.skip_spaces();
            if self.eat("&&") || self.eat("and ") {
                self.skip_spaces();
                let right = self.parse_unary()?;
                left = Expr::And(Box::new(left), Box::new(right));
            } else {
                return Ok(left);
            }
        }
    }

    fn parse_unary(&mut self) -> Result<Expr, FilterError> {
        self.skip_spaces();
        if self.eat("!") && !self.eat("=") {
            self.skip_spaces();
            return Ok(Expr::Not(Box::new(self.parse_unary()?)));
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> Result<Expr, FilterError> {
        self.skip_spaces();

        if self.eat("(") {
            let inner = self.parse_or()?;
            self.skip_spaces();
            if !self.eat(")") {
                return Err(self.error("expected `)`"));
            }
            return Ok(inner);
        }

        // A comparison has a path on one side; a literal on the left is allowed
        // because `1000 < @.latency_ms` reads naturally to some people.
        if matches!(self.peek(), Some('@' | '$')) {
            let path = self.parse_path()?;
            self.skip_spaces();
            let Some(op) = self.parse_op() else {
                return Ok(Expr::Exists(path));
            };
            self.skip_spaces();
            let value = self.parse_literal()?;
            return Ok(Expr::Compare { path, op, value });
        }

        let value = self.parse_literal()?;
        self.skip_spaces();
        let Some(op) = self.parse_op() else {
            return Err(self.error("expected a comparison against a path"));
        };
        self.skip_spaces();
        if !matches!(self.peek(), Some('@' | '$')) {
            return Err(self.error("expected a path like `@.field`"));
        }
        let path = self.parse_path()?;
        Ok(Expr::Compare {
            path,
            op: flip(op),
            value,
        })
    }

    fn parse_op(&mut self) -> Option<Op> {
        for (text, op) in [
            ("==", Op::Eq),
            ("!=", Op::Ne),
            ("<=", Op::Le),
            (">=", Op::Ge),
            ("<", Op::Lt),
            (">", Op::Gt),
        ] {
            if self.eat(text) {
                return Some(op);
            }
        }
        None
    }

    /// Parse `@.a.b[0]['c']`, returning its index in [`Parser::paths`].
    fn parse_path(&mut self) -> Result<usize, FilterError> {
        self.at += 1; // `@` or `$`
        let mut steps = Vec::new();

        loop {
            if self.eat("..") {
                return Err(
                    self.error("descendant segments (`..`) are not supported by this subset")
                );
            }
            if self.eat(".") {
                if self.eat("*") {
                    return Err(self.error("wildcards (`*`) are not supported by this subset"));
                }
                let start = self.at;
                while matches!(self.peek(), Some(c) if c.is_alphanumeric() || c == '_' || c == '-')
                {
                    self.at += 1;
                }
                if self.at == start {
                    return Err(self.error("expected a property name after `.`"));
                }
                steps.push(Step::Key(self.chars[start..self.at].iter().collect()));
                continue;
            }
            if self.eat("[") {
                self.skip_spaces();
                let step = match self.peek() {
                    Some('\'' | '"') => Step::Key(self.parse_quoted()?),
                    Some('*') => {
                        return Err(self.error("wildcards (`*`) are not supported by this subset"));
                    }
                    Some(c) if c.is_ascii_digit() => {
                        let start = self.at;
                        while matches!(self.peek(), Some(d) if d.is_ascii_digit()) {
                            self.at += 1;
                        }
                        let text: String = self.chars[start..self.at].iter().collect();
                        Step::Index(text.parse().unwrap_or(0))
                    }
                    _ => return Err(self.error("expected an index or a quoted name")),
                };
                self.skip_spaces();
                if !self.eat("]") {
                    return Err(self.error("expected `]`"));
                }
                steps.push(step);
                continue;
            }
            break;
        }

        // Deduplicated, so one walk collects each path at most once however
        // often the expression names it.
        if let Some(existing) = self.paths.iter().position(|known| *known == steps) {
            return Ok(existing);
        }
        self.paths.push(steps);
        Ok(self.paths.len() - 1)
    }

    fn parse_quoted(&mut self) -> Result<String, FilterError> {
        let quote = self.peek().unwrap_or('"');
        self.at += 1;
        let mut out = String::new();
        loop {
            match self.peek() {
                None => return Err(self.error("unterminated string")),
                Some(c) if c == quote => {
                    self.at += 1;
                    return Ok(out);
                }
                Some('\\') => {
                    self.at += 1;
                    match self.peek() {
                        Some('n') => out.push('\n'),
                        Some('t') => out.push('\t'),
                        Some(c) => out.push(c),
                        None => return Err(self.error("unterminated escape")),
                    }
                    self.at += 1;
                }
                Some(c) => {
                    out.push(c);
                    self.at += 1;
                }
            }
        }
    }

    fn parse_literal(&mut self) -> Result<Value, FilterError> {
        self.skip_spaces();
        match self.peek() {
            Some('\'' | '"') => Ok(Value::Text(self.parse_quoted()?)),
            Some(c) if c.is_ascii_digit() || c == '-' => {
                let start = self.at;
                self.at += 1;
                while matches!(self.peek(), Some(d) if d.is_ascii_digit() || d == '.' || d == 'e' || d == 'E' || d == '+' || d == '-')
                {
                    self.at += 1;
                }
                let text: String = self.chars[start..self.at].iter().collect();
                text.parse()
                    .map(Value::Number)
                    .map_err(|_| self.error("not a number"))
            }
            _ => {
                if self.eat("true") {
                    Ok(Value::Bool(true))
                } else if self.eat("false") {
                    Ok(Value::Bool(false))
                } else if self.eat("null") {
                    Ok(Value::Null)
                } else if self.eat("length(")
                    || self.eat("count(")
                    || self.eat("match(")
                    || self.eat("search(")
                {
                    Err(self.error("function extensions are not supported by this subset"))
                } else {
                    Err(self.error("expected a value"))
                }
            }
        }
    }
}

/// `1000 < @.x` means the same as `@.x > 1000`.
const fn flip(op: Op) -> Op {
    match op {
        Op::Eq => Op::Eq,
        Op::Ne => Op::Ne,
        Op::Lt => Op::Gt,
        Op::Le => Op::Ge,
        Op::Gt => Op::Lt,
        Op::Ge => Op::Le,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RECORD: &[u8] = br#"{"id":7,"status":"error","latency_ms":1944.05,"ok":true,
        "tags":["ap-south-1","auth"],"meta":{"region":"ap-south-1","retries":2},"nil":null}"#;

    fn matches(query: &str) -> bool {
        Filter::parse(query)
            .unwrap_or_else(|e| panic!("{query}: {e}"))
            .matches(RECORD)
    }

    #[test]
    fn equality_on_a_string_field() {
        assert!(matches(r#"@.status == "error""#));
        assert!(!matches(r#"@.status == "ok""#));
        assert!(matches(r#"@.status != "ok""#));
    }

    #[test]
    fn the_full_jsonpath_wrapper_is_accepted_too() {
        // What someone pastes from documentation, and what they type, both work.
        assert!(matches(r#"$[?@.status == "error"]"#));
    }

    #[test]
    fn numeric_comparison_is_numeric_not_lexicographic() {
        // The bug this test exists for: "1944.05" < "999" as strings.
        assert!(matches("@.latency_ms > 999"));
        assert!(matches("@.latency_ms >= 1944.05"));
        assert!(!matches("@.latency_ms < 1000"));
        assert!(matches("@.id <= 7"));
    }

    #[test]
    fn a_literal_may_come_first() {
        assert!(matches("999 < @.latency_ms"));
        assert!(!matches("999 > @.latency_ms"));
    }

    #[test]
    fn conjunction_disjunction_and_negation() {
        assert!(matches(r#"@.status == "error" && @.latency_ms > 1000"#));
        assert!(!matches(r#"@.status == "error" && @.latency_ms > 9999"#));
        assert!(matches(r#"@.status == "nope" || @.id == 7"#));
        assert!(matches(r#"!(@.status == "ok")"#));
        assert!(matches(r#"@.ok == true && !(@.id > 100)"#));
    }

    #[test]
    fn and_binds_tighter_than_or() {
        // `a || b && c` is `a || (b && c)`. Getting this backwards silently
        // changes which records a user sees.
        assert!(matches(r#"@.id == 999 || @.id == 7 && @.ok == true"#));
        assert!(!matches(r#"(@.id == 999 || @.id == 7) && @.ok == false"#));
    }

    #[test]
    fn nested_and_indexed_paths() {
        assert!(matches(r#"@.meta.region == "ap-south-1""#));
        assert!(matches("@.meta.retries == 2"));
        assert!(matches(r#"@.tags[0] == "ap-south-1""#));
        assert!(matches(r#"@.tags[1] == "auth""#));
        assert!(!matches(r#"@.tags[1] == "ap-south-1""#));
        assert!(matches(r#"@['status'] == "error""#));
    }

    #[test]
    fn existence_is_a_test_on_its_own() {
        assert!(matches("@.status"));
        assert!(!matches("@.missing"));
        assert!(matches("@.nil"), "present-and-null still exists");
        assert!(matches("@.meta"), "a container exists");
    }

    #[test]
    fn a_missing_path_compares_false_including_with_not_equal() {
        // RFC 9535: a comparison against something absent is false, and that
        // includes `!=` — which surprises people, so it has a test.
        assert!(!matches(r#"@.missing == "x""#));
        assert!(!matches(r#"@.missing != "x""#));
        assert!(matches(r#"!(@.missing != "x")"#));
    }

    #[test]
    fn mismatched_types_do_not_match_rather_than_erroring() {
        // A filter over a million heterogeneous records must not stop at the
        // first surprise.
        assert!(!matches(r#"@.id == "7""#));
        assert!(!matches("@.status > 5"));
        assert!(matches(r#"@.id != "7""#), "different types are unequal");
    }

    #[test]
    fn a_record_that_does_not_parse_simply_does_not_match() {
        // Broken syntax is `Validate`'s report to make, not this one's.
        let filter = Filter::parse("@.a == 1").unwrap();
        assert!(!filter.matches(b"{\"a\": "));
        assert!(!filter.matches(b"not json"));
    }

    #[test]
    fn repeated_paths_are_collected_once() {
        let filter = Filter::parse("@.id > 1 && @.id < 100 && @.id != 50").unwrap();
        assert_eq!(filter.path_count(), 1);
        assert!(filter.matches(RECORD));
    }

    #[test]
    fn unsupported_constructs_are_named_not_misread() {
        // Evaluating `$..a == 1` as something else would be worse than refusing.
        for (query, expected) in [
            ("@..a == 1", "descendant"),
            ("@.* == 1", "wildcard"),
            ("@[*] == 1", "wildcard"),
            ("length(@.tags) > 1", "function"),
        ] {
            let error = Filter::parse(query).unwrap_err();
            assert!(
                error.message.contains(expected),
                "{query}: {} should mention {expected}",
                error.message
            );
        }
    }

    #[test]
    fn a_malformed_expression_says_where() {
        let error = Filter::parse("@.a == ").unwrap_err();
        assert!(error.message.contains("value"), "{error}");
        let unbalanced = Filter::parse("(@.a == 1").unwrap_err();
        assert!(unbalanced.message.contains(')'), "{unbalanced}");
        let trailing = Filter::parse("@.a == 1 garbage").unwrap_err();
        assert!(trailing.message.contains("trailing"), "{trailing}");
    }

    #[test]
    fn a_bare_number_record_is_seen() {
        // C30/C37 again: without the final flush a record of `42` yields no
        // tokens at all, and every filter would silently reject it.
        let filter = Filter::parse("@ == 42").unwrap();
        assert!(filter.matches(b"42"));
        assert!(!filter.matches(b"43"));
    }
}
