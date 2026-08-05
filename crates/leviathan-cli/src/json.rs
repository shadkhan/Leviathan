//! A JSON value, for the harness only.
//!
//! **This is not part of the product and must never be.** Leviathan's entire
//! thesis is that a 500 MB file is indexed rather than materialized (C1, C57);
//! a `Value` type in the engine would be the failure the engine exists to
//! remove. It lives here because a *test harness* has the opposite problem: the
//! RFC 9535 compliance suite arrives as one 230 KB JSON file of cases that have
//! to be read into memory before any of them can be run.
//!
//! It is built on [`leviathan_core`]'s own lexer and grammar walk, which is
//! worth more than it costs: reading the compliance suite exercises the same
//! parser the suite is testing, and a harness that needed its own parser would
//! be evidence the core's was not usable.
//!
//! ## Strings and numbers keep their source text
//!
//! A string holds its **raw quoted bytes**, escapes and all, and a number holds
//! its literal text. Nothing is unescaped and nothing is reformatted, so
//! [`Json::write`] round-trips a value back to the text it was read from.
//!
//! That is a deliberate trade. It means the harness cannot tell `"A"` from
//! `"A"`, which a general-purpose parser would have to; and it means the harness
//! introduces no unescaping bug of its own into a run whose entire purpose is to
//! judge the engine. For a corpus that compares each value against a copy of
//! itself written in the same file, the first cost is not paid and the second
//! saving is real.

use std::fmt::Write as _;

use leviathan_core::{Documents, Event, Lexer, Structure, TokenKind};

/// A JSON value, as written.
#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    /// The number's literal text, so `1.0` and `1` stay distinguishable.
    Number(String),
    /// The string's raw text **including its quotes**. Never unescaped.
    Str(String),
    Array(Vec<Json>),
    /// Members in source order, keys raw and quoted like [`Json::Str`].
    Object(Vec<(String, Json)>),
}

impl Json {
    /// Parse one JSON document.
    ///
    /// # Errors
    ///
    /// If the text is not one well-formed JSON value.
    pub fn parse(text: &str) -> Result<Json, String> {
        let bytes = text.as_bytes();
        let mut lexer = Lexer::new();
        let mut structure = Structure::new(Documents::One);

        // Frames of containers currently open. The root value lands in `done`.
        let mut stack: Vec<Frame> = Vec::new();
        let mut done: Option<Json> = None;
        let mut pending_key: Option<String> = None;

        let mut tokens = Vec::new();
        for token in lexer.feed(bytes) {
            tokens.push(token.map_err(|e| e.to_string())?);
        }
        // The final flush. Omitting it loses a document that is a bare number,
        // which is the mistake this codebase has made five times (C30, C37).
        if let Some(token) = lexer.finish().map_err(|e| e.to_string())? {
            tokens.push(token);
        }

        for token in tokens {
            let raw = &text[token.start as usize..token.end as usize];
            let kind = token.kind;
            let Some(event) = structure.push(token).map_err(|e| e.to_string())? else {
                continue;
            };

            match event {
                Event::Key { .. } => pending_key = Some(raw.to_string()),
                Event::Scalar { .. } => {
                    let value = match kind {
                        TokenKind::Null => Json::Null,
                        TokenKind::True => Json::Bool(true),
                        TokenKind::False => Json::Bool(false),
                        TokenKind::Number { .. } => Json::Number(raw.to_string()),
                        _ => Json::Str(raw.to_string()),
                    };
                    place(&mut stack, &mut done, pending_key.take(), value);
                }
                Event::Open { kind, .. } => stack.push(Frame {
                    key: pending_key.take(),
                    value: match kind {
                        leviathan_core::ContainerKind::Object => Json::Object(Vec::new()),
                        leviathan_core::ContainerKind::Array => Json::Array(Vec::new()),
                    },
                }),
                Event::Close { .. } => {
                    let frame = stack.pop().ok_or("unbalanced close")?;
                    place(&mut stack, &mut done, frame.key, frame.value);
                }
            }
        }
        structure.finish().map_err(|e| e.to_string())?;
        done.ok_or_else(|| "no JSON value".to_string())
    }

    /// Write the value back out as compact JSON.
    ///
    /// Round-trips: strings and numbers are emitted as they were read, so the
    /// output re-parses to an equal value.
    #[must_use]
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        self.write(&mut out);
        out
    }

    fn write(&self, out: &mut String) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(true) => out.push_str("true"),
            Json::Bool(false) => out.push_str("false"),
            Json::Number(text) | Json::Str(text) => out.push_str(text),
            Json::Array(items) => {
                out.push('[');
                for (at, item) in items.iter().enumerate() {
                    if at > 0 {
                        out.push(',');
                    }
                    item.write(out);
                }
                out.push(']');
            }
            Json::Object(members) => {
                out.push('{');
                for (at, (key, value)) in members.iter().enumerate() {
                    if at > 0 {
                        out.push(',');
                    }
                    let _ = write!(out, "{key}:");
                    value.write(out);
                }
                out.push('}');
            }
        }
    }

    /// The member of an object, by its **unquoted** name.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Json> {
        let Json::Object(members) = self else {
            return None;
        };
        members
            .iter()
            .find(|(key, _)| unquote(key) == name)
            .map(|(_, value)| value)
    }

    /// The elements, if this is an array.
    #[must_use]
    pub fn items(&self) -> Option<&[Json]> {
        match self {
            Json::Array(items) => Some(items),
            _ => None,
        }
    }

    /// The text, if this is a string, with its quotes removed.
    #[must_use]
    pub fn text(&self) -> Option<String> {
        match self {
            Json::Str(raw) => Some(unquote(raw)),
            _ => None,
        }
    }

    /// Whether this is `true`.
    #[must_use]
    pub const fn is_true(&self) -> bool {
        matches!(self, Json::Bool(true))
    }
}

/// A container being built, and the key it will be filed under.
struct Frame {
    key: Option<String>,
    value: Json,
}

/// File a finished value into its parent, or into the root slot.
fn place(stack: &mut [Frame], done: &mut Option<Json>, key: Option<String>, value: Json) {
    match stack.last_mut().map(|frame| &mut frame.value) {
        Some(Json::Array(items)) => items.push(value),
        Some(Json::Object(members)) => members.push((key.unwrap_or_default(), value)),
        _ => *done = Some(value),
    }
}

/// Strip the quotes from a raw string, and undo the escapes a key can contain.
///
/// Only the simple escapes, because that is all the compliance suite's *keys*
/// and *names* use. Values are never unescaped — see the module docs.
fn unquote(raw: &str) -> String {
    let inner = raw
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap_or(raw);
    if !inner.contains('\\') {
        return inner.to_string();
    }

    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('b') => out.push('\u{8}'),
            Some('f') => out.push('\u{c}'),
            Some('u') => {
                let hex: String = chars.by_ref().take(4).collect();
                match u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                    Some(decoded) => out.push(decoded),
                    // A lone surrogate. Kept as written rather than replaced,
                    // so nothing here silently changes a case's meaning.
                    None => {
                        out.push_str("\\u");
                        out.push_str(&hex);
                    }
                }
            }
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalars_round_trip_as_written() {
        for text in ["null", "true", "false", "42", "1.0", "1e3", r#""hi""#] {
            let value = Json::parse(text).unwrap_or_else(|e| panic!("{text}: {e}"));
            assert_eq!(value.to_text(), text, "round trip of {text}");
        }
    }

    #[test]
    fn a_bare_number_is_not_lost_to_the_missing_flush() {
        // The sixth sighting of C30/C37 would have been here.
        assert_eq!(Json::parse("42").unwrap(), Json::Number("42".into()));
    }

    #[test]
    fn containers_round_trip_compactly() {
        let text = r#"{"a":[1,2,{"b":null}],"c":true}"#;
        assert_eq!(Json::parse(text).unwrap().to_text(), text);
    }

    #[test]
    fn one_and_one_point_zero_stay_different() {
        // The harness compares a selected element against a copy of itself, so
        // conflating these would let a wrong answer pass.
        assert_ne!(Json::parse("1").unwrap(), Json::parse("1.0").unwrap());
    }

    #[test]
    fn members_are_reachable_by_unescaped_name() {
        let value = Json::parse(r#"{"abc":1,"plain":2}"#).unwrap();
        assert_eq!(value.get("abc"), Some(&Json::Number("1".into())));
        assert_eq!(value.get("plain"), Some(&Json::Number("2".into())));
        assert_eq!(value.get("missing"), None);
    }

    #[test]
    fn strings_keep_their_escapes_rather_than_being_decoded() {
        // Deliberate: the harness must not introduce an unescaping bug into a
        // run whose purpose is to judge the engine.
        let value = Json::parse(r#"["A"]"#).unwrap();
        assert_eq!(value.to_text(), r#"["A"]"#);
    }

    #[test]
    fn malformed_input_is_an_error_not_a_panic() {
        for text in ["", "{", "[1,]", "nul", r#"{"a" 1}"#] {
            assert!(Json::parse(text).is_err(), "{text} should not parse");
        }
    }
}
