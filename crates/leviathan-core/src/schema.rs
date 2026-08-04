//! A JSON Schema subset, checked against the streaming walk.
//!
//! ## Why this is hand-written
//!
//! Measured, not assumed (`DEEP_REASONING.md` C57): a wasm32 build with
//! `jsonschema` 0.49 — HTTP and TLS features already disabled — costs
//! **+2.4 MB raw / +725 kB gzipped** against a 250 kB budget, on a binary that
//! is 60 kB today. That alone settles it, and it is not the stronger reason.
//!
//! Every mainstream Rust schema validator checks a `serde_json::Value`.
//! Leviathan never has one: not materializing the document is the design (C1),
//! and it is why a 500 MB file opens at all. Adopting one would mean building a
//! `Value` per record, or one for a whole file — reintroducing the failure the
//! product exists to remove, in order to add a feature to it.
//!
//! ## The asymmetry that makes this tractable
//!
//! **A schema is small; a document is not.** So the schema *is* parsed into a
//! tree here — a few kilobytes, once — while the instance is never parsed into
//! anything. Instances are checked by walking their events and keeping a stack
//! of "which subschema applies here", which is one pass and constant memory in
//! the size of the value.
//!
//! ## What is supported, and what is not
//!
//! Supported: `type`, `enum`, `const`, `required`, `properties`,
//! `additionalProperties`, `items`, `minimum`, `maximum`, `exclusiveMinimum`,
//! `exclusiveMaximum`, `minLength`, `maxLength`, `minItems`, `maxItems`,
//! `minProperties`, `maxProperties`, local `$ref` into `$defs`, and `$defs`.
//!
//! **Not** supported, and reported rather than ignored — an unsupported keyword
//! that silently passes is a validator that lies:
//!
//! - `pattern` / `patternProperties` — needs a regex engine, which is a
//!   dependency and a binary-size decision of its own. Recorded as a limit.
//! - `allOf` / `anyOf` / `oneOf` / `not` / `if`-`then`-`else` — schema
//!   composition, which needs backtracking over a stream.
//! - Remote `$ref` — a network fetch, and requirement 10 forbids one.
//! - `format`, `multipleOf`, `uniqueItems`, `contains`, tuple-form `prefixItems`.

use crate::lexer::{Lexer, Position, TokenKind};
use crate::rows::unescape;
use crate::structure::{ContainerKind, Documents, Event, Structure};
use crate::validate::Invalid;

/// How much of a string to keep when comparing or measuring.
const MAX_TEXT: usize = 64 * 1024;

/// Why a schema could not be compiled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaError {
    /// What is wrong, phrased for whoever wrote the schema.
    pub message: String,
}

impl core::fmt::Display for SchemaError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.message)
    }
}

impl core::error::Error for SchemaError {}

/// The JSON types a schema can name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct TypeMask(u8);

impl TypeMask {
    const NULL: u8 = 1;
    const BOOL: u8 = 2;
    const OBJECT: u8 = 4;
    const ARRAY: u8 = 8;
    const NUMBER: u8 = 16;
    const STRING: u8 = 32;
    /// `integer` is `number` plus a whole-value check, kept separately so the
    /// difference can be reported.
    const INTEGER: u8 = 64;

    fn of_name(name: &str) -> Option<u8> {
        Some(match name {
            "null" => Self::NULL,
            "boolean" => Self::BOOL,
            "object" => Self::OBJECT,
            "array" => Self::ARRAY,
            "number" => Self::NUMBER,
            "string" => Self::STRING,
            "integer" => Self::INTEGER | Self::NUMBER,
            _ => return None,
        })
    }

    const fn allows(self, bit: u8) -> bool {
        self.0 == 0 || self.0 & bit != 0
    }
}

/// A scalar as it appears in `enum` or `const`.
#[derive(Debug, Clone, PartialEq)]
enum Literal {
    Null,
    Bool(bool),
    Number(f64),
    Text(String),
}

/// What `additionalProperties` says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Additional {
    /// Absent: anything goes.
    Allowed,
    /// `false`: an unlisted property is an error.
    Forbidden,
    /// A schema every unlisted property must satisfy.
    Schema(usize),
}

/// One compiled schema node. Indices refer to [`Schema::nodes`].
#[derive(Debug, Clone, Default)]
struct Node {
    types: TypeMask,
    enumeration: Option<Vec<Literal>>,
    constant: Option<Literal>,
    required: Vec<String>,
    properties: Vec<(String, usize)>,
    additional: Option<Additional>,
    items: Option<usize>,
    minimum: Option<f64>,
    maximum: Option<f64>,
    exclusive_minimum: Option<f64>,
    exclusive_maximum: Option<f64>,
    min_length: Option<u64>,
    max_length: Option<u64>,
    min_items: Option<u64>,
    max_items: Option<u64>,
    min_properties: Option<u64>,
    max_properties: Option<u64>,
    /// A local `$ref`, resolved after the whole schema is compiled.
    reference: Option<String>,
    resolved: Option<usize>,
}

/// A compiled schema.
#[derive(Debug, Clone)]
pub struct Schema {
    nodes: Vec<Node>,
    root: usize,
    /// `$defs` entries by name, for `$ref` to find.
    definitions: Vec<(String, usize)>,
    /// Keywords seen but not implemented, so the caller can say so once rather
    /// than pretend the document was fully checked.
    unsupported: Vec<String>,
}

impl Schema {
    /// Keywords present in the schema that this validator does not implement.
    ///
    /// Never empty-by-accident: a schema using `pattern` is only *partly*
    /// checked, and a validator that does not say so is claiming more than it
    /// did.
    #[must_use]
    pub fn unsupported(&self) -> &[String] {
        &self.unsupported
    }

    /// Compile a schema document.
    ///
    /// # Errors
    ///
    /// If the schema is not valid JSON, or is not an object.
    pub fn compile(source: &[u8]) -> Result<Self, SchemaError> {
        let value = parse(source)?;
        let mut schema = Self {
            nodes: Vec::new(),
            root: 0,
            definitions: Vec::new(),
            unsupported: Vec::new(),
        };
        schema.root = schema.build(&value)?;
        schema.resolve()?;
        schema.unsupported.sort_unstable();
        schema.unsupported.dedup();
        Ok(schema)
    }

    fn note_unsupported(&mut self, keyword: &str) {
        self.unsupported.push(keyword.to_string());
    }

    /// Turn one parsed schema object into a node, recursively.
    fn build(&mut self, value: &Json) -> Result<usize, SchemaError> {
        // `true` and `false` are schemas in their own right.
        let members = match value {
            Json::Bool(true) => return Ok(self.push(Node::default())),
            Json::Bool(false) => {
                // A mask no value satisfies: `false` is the schema nothing meets.
                let node = Node {
                    types: TypeMask(0x80),
                    ..Node::default()
                };
                return Ok(self.push(node));
            }
            Json::Object(members) => members,
            _ => {
                return Err(SchemaError {
                    message: "a schema must be an object, or true/false".to_string(),
                });
            }
        };

        let mut node = Node::default();
        let mut defs: Vec<(String, &Json)> = Vec::new();

        for (key, child) in members {
            match key.as_str() {
                "type" => node.types = TypeMask(self.type_mask(child)?),
                "enum" => node.enumeration = Some(literals(child)?),
                "const" => node.constant = Some(literal(child)?),
                "required" => node.required = strings(child)?,
                "minimum" => node.minimum = number(child),
                "maximum" => node.maximum = number(child),
                "exclusiveMinimum" => node.exclusive_minimum = number(child),
                "exclusiveMaximum" => node.exclusive_maximum = number(child),
                "minLength" => node.min_length = whole(child),
                "maxLength" => node.max_length = whole(child),
                "minItems" => node.min_items = whole(child),
                "maxItems" => node.max_items = whole(child),
                "minProperties" => node.min_properties = whole(child),
                "maxProperties" => node.max_properties = whole(child),
                "$ref" => {
                    if let Json::Text(target) = child {
                        node.reference = Some(target.clone());
                    }
                }
                "$defs" | "definitions" => {
                    if let Json::Object(entries) = child {
                        for (name, body) in entries {
                            defs.push((name.clone(), body));
                        }
                    }
                }
                "properties" => {
                    if let Json::Object(entries) = child {
                        for (name, body) in entries {
                            let at = self.build(body)?;
                            node.properties.push((name.clone(), at));
                        }
                    }
                }
                "additionalProperties" => {
                    node.additional = Some(match child {
                        Json::Bool(false) => Additional::Forbidden,
                        Json::Bool(true) => Additional::Allowed,
                        other => Additional::Schema(self.build(other)?),
                    });
                }
                "items" => node.items = Some(self.build(child)?),
                // Recorded, never silently ignored.
                "pattern" | "patternProperties" | "allOf" | "anyOf" | "oneOf" | "not" | "if"
                | "then" | "else" | "format" | "multipleOf" | "uniqueItems" | "contains"
                | "prefixItems" | "dependentSchemas" | "propertyNames" => {
                    self.note_unsupported(key);
                }
                _ => {}
            }
        }

        let at = self.push(node);
        // `$defs` entries are compiled so `$ref` can find them by name.
        for (name, body) in defs {
            let target = self.build(body)?;
            self.definitions.push((name, target));
        }
        Ok(at)
    }

    fn push(&mut self, node: Node) -> usize {
        self.nodes.push(node);
        self.nodes.len() - 1
    }

    fn type_mask(&mut self, value: &Json) -> Result<u8, SchemaError> {
        let mut mask = 0u8;
        match value {
            Json::Text(name) => {
                mask |= TypeMask::of_name(name).ok_or_else(|| SchemaError {
                    message: format!("unknown type `{name}`"),
                })?;
            }
            Json::Array(items) => {
                for item in items {
                    if let Json::Text(name) = item {
                        mask |= TypeMask::of_name(name).ok_or_else(|| SchemaError {
                            message: format!("unknown type `{name}`"),
                        })?;
                    }
                }
            }
            _ => {
                return Err(SchemaError {
                    message: "`type` must be a string or an array of strings".to_string(),
                });
            }
        }
        Ok(mask)
    }

    /// Point every `$ref` at the node it names.
    fn resolve(&mut self) -> Result<(), SchemaError> {
        let table = self.definitions.clone();
        for index in 0..self.nodes.len() {
            let Some(reference) = self.nodes[index].reference.clone() else {
                continue;
            };
            if !reference.starts_with('#') {
                return Err(SchemaError {
                    message: format!(
                        "only local `$ref` is supported; `{reference}` would need a network fetch"
                    ),
                });
            }
            let name = reference.rsplit('/').next().unwrap_or_default();
            let target = table
                .iter()
                .find(|(defined, _)| defined == name)
                .map(|(_, at)| *at);
            match target {
                Some(at) => self.nodes[index].resolved = Some(at),
                None if reference == "#" => self.nodes[index].resolved = Some(self.root),
                None => {
                    return Err(SchemaError {
                        message: format!("`$ref` target not found: {reference}"),
                    });
                }
            }
        }
        Ok(())
    }

    fn node(&self, at: usize) -> &Node {
        let node = &self.nodes[at];
        match node.resolved {
            Some(target) => &self.nodes[target],
            None => node,
        }
    }

    /// Check one complete instance, reporting every way it fails.
    ///
    /// Offsets in the returned errors are relative to `instance`; a caller
    /// validating a record inside a larger file adds the record's own offset.
    ///
    /// A single pass over the instance's events, with a stack of applicable
    /// subschemas. Nothing is materialized.
    #[must_use]
    pub fn check(&self, instance: &[u8]) -> Vec<Invalid> {
        let mut errors = Vec::new();
        let mut lexer = Lexer::new();
        let mut structure = Structure::new(Documents::One);
        let mut stack: Vec<Frame> = Vec::new();
        let mut pending_key: Option<String> = None;
        let mut root_seen = false;

        // Positions are tracked here rather than asked of the lexer, which the
        // token iterator has borrowed. Tokens arrive in increasing offset order,
        // so a forward-only cursor costs one pass over the instance in total
        // instead of one scan per token.
        let mut cursor = Cursor::new();

        // Collected rather than streamed, so the flush is impossible to forget.
        // A number is the only token that cannot be emitted until the byte after
        // it arrives, so a loop over `feed` alone sees *nothing* in `42` — the
        // omission that has now appeared five times (C30, C37). Gathering the
        // stream in one place means there is one flush, not one per consumer.
        // The instance is a record and is already in memory; this adds no
        // asymptotic cost that `check` was not already paying.
        let mut tokens = Vec::new();
        for token in lexer.feed(instance) {
            let Ok(token) = token else { break };
            tokens.push(token);
        }
        if let Ok(Some(token)) = lexer.finish() {
            tokens.push(token);
        }

        for token in tokens {
            let position = cursor.at(instance, token.start);
            let event = match structure.push(token) {
                Ok(Some(event)) => event,
                Ok(None) => continue,
                Err(_) => break, // Not well-formed; `Validate` reports that.
            };

            match event {
                Event::Key { token, .. } => {
                    let (text, _) = unescape(span(instance, token.start, token.end), MAX_TEXT);
                    if let Some(frame) = stack.last_mut() {
                        frame.keys.push(text.clone());
                    }
                    pending_key = Some(text);
                }
                Event::Scalar { token, depth } => {
                    let at = self.applicable(&stack, pending_key.take(), &mut errors, &position);
                    let kind = token.kind;
                    let raw = span(instance, token.start, token.end);
                    self.check_scalar(at, kind, raw, &position, &mut errors);
                    if depth == 0 {
                        root_seen = true;
                    }
                }
                Event::Open { kind, depth, .. } => {
                    let at = self.applicable(&stack, pending_key.take(), &mut errors, &position);
                    let bit = match kind {
                        ContainerKind::Object => TypeMask::OBJECT,
                        ContainerKind::Array => TypeMask::ARRAY,
                    };
                    self.check_type(at, bit, kind_name(bit), &position, &mut errors);
                    stack.push(Frame {
                        schema: at,
                        kind,
                        keys: Vec::new(),
                        items: 0,
                        start: position,
                    });
                    if depth == 0 {
                        root_seen = true;
                    }
                }
                Event::Close { children, .. } => {
                    if let Some(frame) = stack.pop() {
                        self.check_close(&frame, children, &mut errors);
                    }
                }
            }
        }

        if !root_seen {
            errors.push(Invalid {
                offset: 0,
                line: 1,
                column: 1,
                message: "no JSON value to check against the schema".to_string(),
            });
        }
        errors
    }

    /// The subschema that applies to a value about to be visited.
    fn applicable(
        &self,
        stack: &[Frame],
        key: Option<String>,
        errors: &mut Vec<Invalid>,
        position: &Position,
    ) -> usize {
        let Some(frame) = stack.last() else {
            return self.root;
        };

        match frame.kind {
            ContainerKind::Array => self.node(frame.schema).items.unwrap_or(ANY),
            ContainerKind::Object => {
                let node = self.node(frame.schema);
                let key = key.unwrap_or_default();
                if let Some((_, at)) = node.properties.iter().find(|(name, _)| *name == key) {
                    return *at;
                }
                match node.additional {
                    Some(Additional::Forbidden) => {
                        errors.push(Invalid {
                            offset: position.offset,
                            line: position.line,
                            column: position.column,
                            message: format!("property `{key}` is not allowed here"),
                        });
                        ANY
                    }
                    Some(Additional::Schema(at)) => at,
                    _ => ANY,
                }
            }
        }
    }

    fn check_type(
        &self,
        at: usize,
        bit: u8,
        name: &str,
        position: &Position,
        errors: &mut Vec<Invalid>,
    ) {
        if at == ANY {
            return;
        }
        if !self.node(at).types.allows(bit) {
            errors.push(Invalid {
                offset: position.offset,
                line: position.line,
                column: position.column,
                message: format!("expected {}, found {name}", self.describe_types(at)),
            });
        }
    }

    fn describe_types(&self, at: usize) -> String {
        let mask = self.node(at).types.0;
        let mut names = Vec::new();
        for (bit, name) in [
            (TypeMask::NULL, "null"),
            (TypeMask::BOOL, "boolean"),
            (TypeMask::OBJECT, "object"),
            (TypeMask::ARRAY, "array"),
            (TypeMask::STRING, "string"),
            (TypeMask::INTEGER, "integer"),
            (TypeMask::NUMBER, "number"),
        ] {
            if mask & bit != 0 {
                names.push(name);
                if bit == TypeMask::INTEGER {
                    break;
                }
            }
        }
        if names.is_empty() {
            "nothing".to_string()
        } else {
            names.join(" or ")
        }
    }

    fn check_scalar(
        &self,
        at: usize,
        kind: TokenKind,
        raw: &[u8],
        position: &Position,
        errors: &mut Vec<Invalid>,
    ) {
        if at == ANY {
            return;
        }
        let (bit, name) = match kind {
            TokenKind::Null => (TypeMask::NULL, "null"),
            TokenKind::True | TokenKind::False => (TypeMask::BOOL, "boolean"),
            TokenKind::Number { .. } => (TypeMask::NUMBER, "number"),
            TokenKind::String { .. } => (TypeMask::STRING, "string"),
            _ => return,
        };
        self.check_type(at, bit, name, position, errors);

        let node = self.node(at);
        let complain = |errors: &mut Vec<Invalid>, message: String| {
            errors.push(Invalid {
                offset: position.offset,
                line: position.line,
                column: position.column,
                message,
            });
        };

        if matches!(kind, TokenKind::Number { .. }) {
            let text = core::str::from_utf8(raw).unwrap_or("");
            if let Ok(value) = text.parse::<f64>() {
                if node.types.0 & TypeMask::INTEGER != 0 && value.fract() != 0.0 {
                    complain(errors, format!("expected an integer, found {text}"));
                }
                if let Some(min) = node.minimum {
                    if value < min {
                        complain(errors, format!("{text} is less than the minimum {min}"));
                    }
                }
                if let Some(max) = node.maximum {
                    if value > max {
                        complain(errors, format!("{text} is more than the maximum {max}"));
                    }
                }
                if let Some(min) = node.exclusive_minimum {
                    if value <= min {
                        complain(errors, format!("{text} must be greater than {min}"));
                    }
                }
                if let Some(max) = node.exclusive_maximum {
                    if value >= max {
                        complain(errors, format!("{text} must be less than {max}"));
                    }
                }
            }
        }

        if matches!(kind, TokenKind::String { .. }) {
            let (text, _) = unescape(raw, MAX_TEXT);
            let length = text.chars().count() as u64;
            if let Some(min) = node.min_length {
                if length < min {
                    complain(errors, format!("shorter than the minimum length {min}"));
                }
            }
            if let Some(max) = node.max_length {
                if length > max {
                    complain(errors, format!("longer than the maximum length {max}"));
                }
            }
        }

        if node.enumeration.is_some() || node.constant.is_some() {
            let actual = literal_of(kind, raw);
            if let Some(allowed) = &node.enumeration {
                if !allowed.contains(&actual) {
                    complain(errors, "not one of the permitted values".to_string());
                }
            }
            if let Some(expected) = &node.constant {
                if *expected != actual {
                    complain(errors, "not the required constant value".to_string());
                }
            }
        }
    }

    fn check_close(&self, frame: &Frame, children: u64, errors: &mut Vec<Invalid>) {
        if frame.schema == ANY {
            return;
        }
        let node = self.node(frame.schema);
        let mut complain = |message: String| {
            errors.push(Invalid {
                offset: frame.start.offset,
                line: frame.start.line,
                column: frame.start.column,
                message,
            });
        };

        match frame.kind {
            ContainerKind::Object => {
                for name in &node.required {
                    if !frame.keys.iter().any(|seen| seen == name) {
                        complain(format!("missing required property `{name}`"));
                    }
                }
                if let Some(min) = node.min_properties {
                    if children < min {
                        complain(format!("fewer than {min} properties"));
                    }
                }
                if let Some(max) = node.max_properties {
                    if children > max {
                        complain(format!("more than {max} properties"));
                    }
                }
            }
            ContainerKind::Array => {
                if let Some(min) = node.min_items {
                    if children < min {
                        complain(format!("fewer than {min} items"));
                    }
                }
                if let Some(max) = node.max_items {
                    if children > max {
                        complain(format!("more than {max} items"));
                    }
                }
            }
        }
        let _ = frame.items;
    }
}

/// The node index meaning "no constraints".
const ANY: usize = usize::MAX;

const fn kind_name(bit: u8) -> &'static str {
    if bit == TypeMask::OBJECT {
        "object"
    } else {
        "array"
    }
}

/// One open container, and which subschema governs it.
struct Frame {
    schema: usize,
    kind: ContainerKind,
    keys: Vec<String>,
    items: u64,
    start: Position,
}

/// Line and column for offsets visited in increasing order.
struct Cursor {
    offset: u64,
    line: u64,
    line_start: u64,
}

impl Cursor {
    const fn new() -> Self {
        Self {
            offset: 0,
            line: 1,
            line_start: 0,
        }
    }

    fn at(&mut self, bytes: &[u8], target: u64) -> Position {
        while self.offset < target {
            if bytes.get(self.offset as usize) == Some(&b'\n') {
                self.line += 1;
                self.line_start = self.offset + 1;
            }
            self.offset += 1;
        }
        Position {
            offset: target,
            line: self.line,
            column: target.saturating_sub(self.line_start) + 1,
        }
    }
}

fn span(bytes: &[u8], from: u64, to: u64) -> &[u8] {
    let start = from as usize;
    let end = (to as usize).min(bytes.len());
    bytes.get(start..end).unwrap_or(&[])
}

fn literal_of(kind: TokenKind, raw: &[u8]) -> Literal {
    match kind {
        TokenKind::Null => Literal::Null,
        TokenKind::True => Literal::Bool(true),
        TokenKind::False => Literal::Bool(false),
        TokenKind::Number { .. } => core::str::from_utf8(raw)
            .ok()
            .and_then(|text| text.parse::<f64>().ok())
            .map_or(Literal::Null, Literal::Number),
        _ => Literal::Text(unescape(raw, MAX_TEXT).0),
    }
}

// -------------------------------------------------------------- schema JSON

/// A parsed schema. Small by construction — this is the *schema*, never the
/// document, and the difference is the whole reason this is allowed to exist.
#[derive(Debug, Clone, PartialEq)]
enum Json {
    Null,
    Bool(bool),
    Number(f64),
    Text(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

fn number(value: &Json) -> Option<f64> {
    match value {
        Json::Number(n) => Some(*n),
        _ => None,
    }
}

fn whole(value: &Json) -> Option<u64> {
    number(value).map(|n| n.max(0.0) as u64)
}

fn strings(value: &Json) -> Result<Vec<String>, SchemaError> {
    match value {
        Json::Array(items) => Ok(items
            .iter()
            .filter_map(|item| match item {
                Json::Text(text) => Some(text.clone()),
                _ => None,
            })
            .collect()),
        _ => Err(SchemaError {
            message: "expected an array of strings".to_string(),
        }),
    }
}

fn literal(value: &Json) -> Result<Literal, SchemaError> {
    Ok(match value {
        Json::Null => Literal::Null,
        Json::Bool(b) => Literal::Bool(*b),
        Json::Number(n) => Literal::Number(*n),
        Json::Text(t) => Literal::Text(t.clone()),
        _ => {
            return Err(SchemaError {
                message: "`const` and `enum` support scalar values only".to_string(),
            });
        }
    })
}

fn literals(value: &Json) -> Result<Vec<Literal>, SchemaError> {
    match value {
        Json::Array(items) => items.iter().map(literal).collect(),
        _ => Err(SchemaError {
            message: "`enum` must be an array".to_string(),
        }),
    }
}

/// Parse a schema document into [`Json`], using this crate's own lexer.
fn parse(source: &[u8]) -> Result<Json, SchemaError> {
    let mut lexer = Lexer::new();
    let mut tokens = Vec::new();
    for token in lexer.feed(source) {
        tokens.push(token.map_err(|error| SchemaError {
            message: format!("the schema is not valid JSON: {error}"),
        })?);
    }
    if let Some(token) = lexer.finish().map_err(|error| SchemaError {
        message: format!("the schema is not valid JSON: {error}"),
    })? {
        tokens.push(token);
    }

    let mut at = 0usize;
    let value = parse_value(source, &tokens, &mut at)?;
    Ok(value)
}

fn parse_value(
    source: &[u8],
    tokens: &[crate::lexer::Token],
    at: &mut usize,
) -> Result<Json, SchemaError> {
    let token = tokens.get(*at).ok_or_else(|| SchemaError {
        message: "the schema ended unexpectedly".to_string(),
    })?;
    *at += 1;

    Ok(match token.kind {
        TokenKind::Null => Json::Null,
        TokenKind::True => Json::Bool(true),
        TokenKind::False => Json::Bool(false),
        TokenKind::Number { .. } => Json::Number(
            core::str::from_utf8(span(source, token.start, token.end))
                .ok()
                .and_then(|text| text.parse().ok())
                .unwrap_or(0.0),
        ),
        TokenKind::String { .. } => {
            Json::Text(unescape(span(source, token.start, token.end), MAX_TEXT).0)
        }
        TokenKind::ArrayOpen => {
            let mut items = Vec::new();
            loop {
                match tokens.get(*at).map(|t| t.kind) {
                    Some(TokenKind::ArrayClose) => {
                        *at += 1;
                        break;
                    }
                    Some(TokenKind::Comma) => *at += 1,
                    None => break,
                    _ => items.push(parse_value(source, tokens, at)?),
                }
            }
            Json::Array(items)
        }
        TokenKind::ObjectOpen => {
            let mut members = Vec::new();
            loop {
                match tokens.get(*at).map(|t| t.kind) {
                    Some(TokenKind::ObjectClose) => {
                        *at += 1;
                        break;
                    }
                    Some(TokenKind::Comma | TokenKind::Colon) => *at += 1,
                    None => break,
                    Some(TokenKind::String { .. }) => {
                        let key_token = tokens[*at];
                        *at += 1;
                        let key =
                            unescape(span(source, key_token.start, key_token.end), MAX_TEXT).0;
                        if matches!(tokens.get(*at).map(|t| t.kind), Some(TokenKind::Colon)) {
                            *at += 1;
                        }
                        members.push((key, parse_value(source, tokens, at)?));
                    }
                    _ => {
                        return Err(SchemaError {
                            message: "expected a property name in the schema".to_string(),
                        });
                    }
                }
            }
            Json::Object(members)
        }
        _ => {
            return Err(SchemaError {
                message: "unexpected token in the schema".to_string(),
            });
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema(source: &str) -> Schema {
        Schema::compile(source.as_bytes()).expect("the schema should compile")
    }

    fn errors(schema: &Schema, instance: &str) -> Vec<String> {
        schema
            .check(instance.as_bytes())
            .into_iter()
            .map(|error| error.message)
            .collect()
    }

    const RECORD: &str = r#"{
        "type": "object",
        "required": ["id", "level"],
        "properties": {
            "id": {"type": "integer", "minimum": 0},
            "level": {"type": "string", "enum": ["info", "warn", "error"]},
            "tags": {"type": "array", "items": {"type": "string"}, "maxItems": 3},
            "meta": {"type": "object", "properties": {"retries": {"type": "integer"}}}
        }
    }"#;

    #[test]
    fn a_conforming_record_produces_nothing() {
        let s = schema(RECORD);
        assert_eq!(
            errors(
                &s,
                r#"{"id":1,"level":"warn","tags":["a","b"],"meta":{"retries":2}}"#
            ),
            [] as [String; 0]
        );
    }

    #[test]
    fn a_missing_required_property_is_named() {
        let s = schema(RECORD);
        let found = errors(&s, r#"{"id":1}"#);
        assert_eq!(found.len(), 1);
        assert!(found[0].contains("level"), "{found:?}");
    }

    #[test]
    fn a_wrong_type_says_what_was_expected_and_what_was_found() {
        let s = schema(RECORD);
        let found = errors(&s, r#"{"id":"nope","level":"info"}"#);
        assert_eq!(found.len(), 1);
        assert!(found[0].contains("integer"), "{found:?}");
        assert!(found[0].contains("string"), "{found:?}");
    }

    #[test]
    fn numeric_bounds_are_checked() {
        let s = schema(RECORD);
        assert!(errors(&s, r#"{"id":-1,"level":"info"}"#)[0].contains("minimum"));
        // `integer` is `number` with a whole-value check, and the two differ.
        assert!(errors(&s, r#"{"id":1.5,"level":"info"}"#)[0].contains("integer"));
    }

    #[test]
    fn an_enum_rejects_a_value_outside_it() {
        let s = schema(RECORD);
        let found = errors(&s, r#"{"id":1,"level":"debug"}"#);
        assert_eq!(found.len(), 1);
        assert!(found[0].contains("permitted"), "{found:?}");
    }

    #[test]
    fn nested_and_array_constraints_are_reached() {
        let s = schema(RECORD);
        assert!(errors(&s, r#"{"id":1,"level":"info","tags":[1]}"#)[0].contains("string"));
        assert!(
            errors(&s, r#"{"id":1,"level":"info","tags":["a","b","c","d"]}"#)[0]
                .contains("more than 3")
        );
        assert!(
            errors(&s, r#"{"id":1,"level":"info","meta":{"retries":"x"}}"#)[0].contains("integer"),
            "a constraint two levels down still applies"
        );
    }

    #[test]
    fn every_failure_is_reported_not_just_the_first() {
        // A validator that stops at the first problem makes fixing a record an
        // iterative game of whack-a-mole.
        let s = schema(RECORD);
        let found = errors(&s, r#"{"id":-1,"level":"nope"}"#);
        assert_eq!(found.len(), 2, "{found:?}");
    }

    #[test]
    fn additional_properties_can_be_forbidden() {
        let s = schema(r#"{"type":"object","properties":{"a":{}},"additionalProperties":false}"#);
        assert_eq!(errors(&s, r#"{"a":1}"#), [] as [String; 0]);
        assert!(errors(&s, r#"{"a":1,"b":2}"#)[0].contains("`b`"));
    }

    #[test]
    fn a_local_ref_resolves() {
        // `r##"…"##`, because the schema text contains `"#` — which would end an
        // `r#"…"#` string early, and does so silently enough to be confusing.
        let s = schema(
            r##"{
                "type": "array",
                "items": {"$ref": "#/$defs/entry"},
                "$defs": {"entry": {"type": "object", "required": ["k"]}}
            }"##,
        );
        assert_eq!(errors(&s, r#"[{"k":1},{"k":2}]"#), [] as [String; 0]);
        assert!(errors(&s, r#"[{"k":1},{"j":2}]"#)[0].contains("`k`"));
    }

    #[test]
    fn a_remote_ref_is_refused_rather_than_fetched() {
        // Requirement 10: the manifest requests no host permissions, so there
        // is no such thing as fetching a schema.
        let error = Schema::compile(br#"{"$ref":"https://example.com/s.json"}"#).unwrap_err();
        assert!(error.message.contains("network"), "{error}");
    }

    #[test]
    fn unsupported_keywords_are_reported_not_ignored() {
        // A schema using `pattern` is only partly checked, and a validator that
        // does not say so is claiming more than it did.
        let s = schema(r#"{"type":"string","pattern":"^a+$","allOf":[{"type":"string"}]}"#);
        assert_eq!(s.unsupported(), ["allOf", "pattern"]);
        // The keywords it *does* know still apply.
        assert!(errors(&s, "42")[0].contains("string"));
    }

    #[test]
    fn a_string_length_counts_characters_not_bytes() {
        let s = schema(r#"{"type":"string","minLength":3,"maxLength":4}"#);
        assert_eq!(errors(&s, r#""😀😀😀""#), [] as [String; 0]);
        assert!(errors(&s, r#""😀""#)[0].contains("shorter"));
    }

    #[test]
    fn errors_carry_a_position_inside_the_instance() {
        let s = schema(RECORD);
        let found = s.check(b"{\n  \"id\": -1,\n  \"level\": \"info\"\n}");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].line, 2, "the line the bad value is on");
        assert!(found[0].offset > 0);
    }

    #[test]
    fn a_malformed_instance_is_left_to_the_well_formedness_pass() {
        // Two jobs, two reports. Duplicating the syntax error here would show
        // the user the same problem twice with different wording.
        let s = schema(RECORD);
        let found = s.check(b"{\"id\": ");
        assert!(
            found.iter().all(|e| !e.message.contains("expected")),
            "{found:?}"
        );
    }

    #[test]
    fn a_bare_number_instance_is_seen_at_all() {
        // The fifth sighting of C30/C37, caught here by a test rather than by a
        // user: a number is the only token that cannot be emitted until the byte
        // after it arrives, so an instance of `42` produces *no tokens* unless
        // the lexer is flushed. Without it this reported "no JSON value" for a
        // perfectly good document.
        let s = schema(r#"{"type":"string"}"#);
        let found = errors(&s, "42");
        assert_eq!(found.len(), 1);
        assert!(found[0].contains("string"), "{found:?}");

        // And the same shape for every other flush-sensitive scalar.
        let n = schema(r#"{"type":"number"}"#);
        assert_eq!(errors(&n, "1.5e3"), [] as [String; 0]);
        assert_eq!(errors(&n, "-0"), [] as [String; 0]);
    }

    #[test]
    fn a_schema_that_is_not_json_is_refused_clearly() {
        let error = Schema::compile(b"{not json").unwrap_err();
        assert!(error.message.contains("not valid JSON"), "{error}");
    }
}
