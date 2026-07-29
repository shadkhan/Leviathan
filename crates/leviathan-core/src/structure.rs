//! The structural layer: tokens in, tree shape out.
//!
//! The lexer guarantees every token is well formed and knows nothing about
//! whether it is *allowed* — `}` after `[` is two valid tokens and one invalid
//! document. This is the layer that knows, and it is the only place in the crate
//! that understands JSON as a grammar rather than as bytes.
//!
//! Everything downstream is built on it. Tier-1 indexing watches for top-level
//! values; tier-2 indexing watches for a container's direct children; validation
//! is this machine reporting its errors; dedup watches keys within one object;
//! export replays the walk. One state machine, five consumers.
//!
//! ## The shape of the output
//!
//! Pushing a token returns at most one [`Event`]. Commas and colons produce
//! none — they are grammar, not content — so the event stream is exactly the
//! document's structure with the punctuation removed.
//!
//! Two things fall out of this that are worth more than they cost:
//!
//! - **[`Event::Close`] carries the container's full byte span.** The open
//!   offset is on the stack and the closing token supplies the end, so every
//!   container's extent is known without storing an end offset per node — which
//!   `DEEP_REASONING.md` C1 explicitly rules out. Tier-2 indexing needs exactly
//!   this span to re-lex a subtree on demand.
//! - **[`Event::Close`] carries the child count.** Counted on the way past, not
//!   by walking the container a second time. This is what lets a row read
//!   `Array (1,234 items)` and what gives the virtual scrollbar its extent.
//!
//! ## Example
//!
//! ```
//! use leviathan_core::{Documents, Event, Lexer, Structure};
//!
//! let source = br#"{"xs":[1,2,3]}"#;
//! let mut lexer = Lexer::new();
//! let mut structure = Structure::new(Documents::One);
//! let mut closes = Vec::new();
//!
//! for token in lexer.feed(source) {
//!     if let Some(Event::Close { start, end, children, .. }) =
//!         structure.push(token.unwrap()).unwrap()
//!     {
//!         closes.push((start, end, children));
//!     }
//! }
//! structure.finish().unwrap();
//!
//! // The inner array spans bytes 6..13 and holds 3 elements; the object
//! // spans the whole document and holds 1 member.
//! assert_eq!(closes, [(6, 13, 3), (0, 14, 1)]);
//! ```

use core::fmt;

use crate::lexer::{Token, TokenKind};

/// Which of the two container kinds a frame is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ContainerKind {
    /// `{ ... }`
    Object,
    /// `[ ... ]`
    Array,
}

impl ContainerKind {
    /// A stable lowercase identifier.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            ContainerKind::Object => "object",
            ContainerKind::Array => "array",
        }
    }

    /// The token that closes this container.
    const fn closing(self) -> TokenKind {
        match self {
            ContainerKind::Object => TokenKind::ObjectClose,
            ContainerKind::Array => TokenKind::ArrayClose,
        }
    }
}

/// How many top-level values the input is allowed to hold.
///
/// This is the difference between a JSON document and an NDJSON stream, and it
/// is the *only* difference as far as the grammar is concerned — which is why
/// one machine serves both instead of two that drift apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Documents {
    /// Exactly one value, as RFC 8259 defines a JSON text. Anything after it is
    /// trailing content and an error.
    One,
    /// A stream of independent values: NDJSON, JSON-lines, and the
    /// whitespace-separated concatenations that tools emit when they mean
    /// NDJSON but forget the newline.
    Many,
}

/// Something the walk found.
///
/// `depth` is the nesting level of the *value itself*: a top-level value is at
/// depth 0 and its direct children at depth 1. [`Event::Open`] and
/// [`Event::Close`] for the same container report the same depth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// A container began.
    Open {
        /// Object or array.
        kind: ContainerKind,
        /// Byte offset of the `{` or `[`.
        start: u64,
        /// Nesting level of this container.
        depth: u32,
    },
    /// A container ended, with everything learned by walking it.
    Close {
        /// Object or array.
        kind: ContainerKind,
        /// Byte offset of the `{` or `[`.
        start: u64,
        /// Byte offset one past the `}` or `]`.
        end: u64,
        /// Members (object) or elements (array) directly inside it.
        children: u64,
        /// Nesting level of this container.
        depth: u32,
    },
    /// An object key. Always immediately followed by its value's event.
    Key {
        /// The string token, span including quotes.
        token: Token,
        /// Nesting level of the *value* this key introduces.
        depth: u32,
    },
    /// A scalar value: string, number, `true`, `false` or `null`.
    Scalar {
        /// The token, span exact.
        token: Token,
        /// Nesting level of this value.
        depth: u32,
    },
}

impl Event {
    /// Byte offset where the thing this event describes begins.
    #[must_use]
    pub const fn start(&self) -> u64 {
        match self {
            Event::Open { start, .. } | Event::Close { start, .. } => *start,
            Event::Key { token, .. } | Event::Scalar { token, .. } => token.start,
        }
    }

    /// Nesting level of the value this event describes.
    #[must_use]
    pub const fn depth(&self) -> u32 {
        match self {
            Event::Open { depth, .. }
            | Event::Close { depth, .. }
            | Event::Key { depth, .. }
            | Event::Scalar { depth, .. } => *depth,
        }
    }
}

/// What was structurally wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StructErrorKind {
    /// A token that the grammar does not allow in this position.
    Unexpected {
        /// What was found.
        found: TokenKind,
        /// A human phrase for what would have been allowed.
        expected: &'static str,
    },
    /// A closing token that does not match the container it would close —
    /// `{"a":1]`.
    Mismatched {
        /// The container that is actually open.
        open: ContainerKind,
        /// The closing token found.
        found: TokenKind,
    },
    /// A second top-level value in a single-document input.
    TrailingContent,
    /// Nesting deeper than the configured limit.
    TooDeep {
        /// The limit that was exceeded.
        limit: u32,
    },
    /// Input ended with containers still open.
    Unclosed {
        /// How many are still open.
        open: u32,
        /// The innermost one.
        innermost: ContainerKind,
    },
    /// Input ended partway through a member or element.
    Incomplete {
        /// A human phrase for what was still awaited.
        expected: &'static str,
    },
}

impl fmt::Display for StructErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StructErrorKind::Unexpected { found, expected } => {
                write!(f, "expected {expected}, found {}", found.as_str())
            }
            StructErrorKind::Mismatched { open, found } => write!(
                f,
                "{} closes an object, but an {} is open",
                found.as_str(),
                open.as_str()
            ),
            StructErrorKind::TrailingContent => write!(
                f,
                "a second top-level value; this input was read as a single document"
            ),
            StructErrorKind::TooDeep { limit } => {
                write!(f, "nesting deeper than the limit of {limit}")
            }
            StructErrorKind::Unclosed { open, innermost } => write!(
                f,
                "input ended with {open} container(s) unclosed, innermost an {}",
                innermost.as_str()
            ),
            StructErrorKind::Incomplete { expected } => {
                write!(f, "input ended where {expected} was expected")
            }
        }
    }
}

/// A structural failure and the byte it happened at.
///
/// Only a byte offset, deliberately: this layer never sees the source bytes, so
/// it cannot count lines. The driver holds the lexer and can turn the offset
/// into a line and column — see [`Lexer::position_of`](crate::Lexer::position_of).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StructError {
    /// What went wrong.
    pub kind: StructErrorKind,
    /// Byte offset of the offending token, or of end-of-input.
    pub offset: u64,
}

impl fmt::Display for StructError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} at byte {}", self.kind, self.offset)
    }
}

impl core::error::Error for StructError {}

/// One open container.
///
/// 24 bytes, and the only per-depth cost in the crate. At the 100 000-deep
/// pathological fixture that is 2.4 MB of stack table — worth measuring, not
/// worth optimizing.
#[derive(Debug, Clone, Copy)]
struct Frame {
    kind: ContainerKind,
    start: u64,
    children: u64,
}

/// What the grammar will accept next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Expect {
    /// A value. Nothing else — this follows a `,` or a `:`.
    Value,
    /// A value or the array's `]`. This follows `[`.
    ValueOrClose,
    /// A key or the object's `}`. This follows `{`.
    KeyOrClose,
    /// A key. This follows a `,` inside an object.
    Key,
    /// `:`
    Colon,
    /// `,` or the matching close.
    CommaOrClose,
    /// A complete single document has been read; nothing may follow.
    Trailing,
}

impl Expect {
    /// A human phrase for an error message.
    const fn describe(self) -> &'static str {
        match self {
            Expect::Value => "a value",
            Expect::ValueOrClose => "a value or `]`",
            Expect::KeyOrClose => "a key or `}`",
            Expect::Key => "a key",
            Expect::Colon => "`:`",
            Expect::CommaOrClose => "`,` or a closing bracket",
            Expect::Trailing => "end of input",
        }
    }
}

/// The default nesting limit.
///
/// Set above the 100 000-deep pathological fixture on purpose: that fixture
/// exists to prove deep input is handled, not rejected. The limit is here to
/// bound memory against adversarial input — a file of nothing but `[` would
/// otherwise grow the frame stack until the allocator gave up — and 1 M frames
/// is 24 MB, which is a bounded loss rather than an unbounded one.
pub const DEFAULT_MAX_DEPTH: u32 = 1_000_000;

/// The JSON grammar, as a resumable state machine over tokens.
///
/// Like the lexer, it is fed incrementally and holds no reference to the source.
/// Unlike the lexer, it has a stack — nesting is the one thing that genuinely
/// cannot be tracked in constant space — and that stack is the reason
/// [`DEFAULT_MAX_DEPTH`] exists.
#[derive(Debug, Clone)]
pub struct Structure {
    stack: Vec<Frame>,
    expect: Expect,
    documents: Documents,
    completed: u64,
    max_depth: u32,
    /// Offset one past the last token seen, for reporting end-of-input errors.
    last_end: u64,
}

impl Structure {
    /// A walk over an input holding `documents` top-level values.
    #[must_use]
    pub fn new(documents: Documents) -> Self {
        Self {
            stack: Vec::new(),
            expect: Expect::Value,
            documents,
            completed: 0,
            max_depth: DEFAULT_MAX_DEPTH,
            last_end: 0,
        }
    }

    /// Set the nesting limit. See [`DEFAULT_MAX_DEPTH`].
    #[must_use]
    pub fn with_max_depth(mut self, max_depth: u32) -> Self {
        self.max_depth = max_depth;
        self
    }

    /// Current nesting depth — how many containers are open.
    #[must_use]
    pub fn depth(&self) -> u32 {
        self.stack.len() as u32
    }

    /// How many top-level values have been completed.
    ///
    /// For NDJSON this is the record count, which is what the tier-1 index and
    /// the scrollbar both need.
    #[must_use]
    pub const fn completed(&self) -> u64 {
        self.completed
    }

    /// Whether a top-level value is currently open.
    #[must_use]
    pub fn in_document(&self) -> bool {
        !self.stack.is_empty()
    }

    /// Feed one token; get back the structural event it produced, if any.
    ///
    /// Commas and colons return `Ok(None)`: they are required by the grammar and
    /// carry no information the consumer could not derive.
    ///
    /// # Errors
    ///
    /// The token is not allowed where it appears.
    pub fn push(&mut self, token: Token) -> Result<Option<Event>, StructError> {
        self.last_end = token.end;

        match self.expect {
            Expect::Value | Expect::ValueOrClose => self.on_value(token),
            Expect::KeyOrClose | Expect::Key => self.on_key(token),
            Expect::Colon => {
                if token.kind == TokenKind::Colon {
                    self.expect = Expect::Value;
                    Ok(None)
                } else {
                    Err(self.unexpected(token, Expect::Colon))
                }
            }
            Expect::CommaOrClose => self.on_comma_or_close(token),
            Expect::Trailing => Err(StructError {
                kind: StructErrorKind::TrailingContent,
                offset: token.start,
            }),
        }
    }

    /// Signal end of input.
    ///
    /// # Errors
    ///
    /// A container is still open, or the input stopped partway through a member.
    pub fn finish(&mut self) -> Result<(), StructError> {
        if let Some(frame) = self.stack.last() {
            return Err(StructError {
                kind: StructErrorKind::Unclosed {
                    open: self.depth(),
                    innermost: frame.kind,
                },
                offset: self.last_end,
            });
        }

        match self.expect {
            // Nothing started, or a document just finished cleanly.
            Expect::Trailing => Ok(()),
            Expect::Value if self.documents == Documents::Many || self.completed == 0 => Ok(()),
            other => Err(StructError {
                kind: StructErrorKind::Incomplete {
                    expected: other.describe(),
                },
                offset: self.last_end,
            }),
        }
    }

    /// A token where a value (or `]`) was expected.
    fn on_value(&mut self, token: Token) -> Result<Option<Event>, StructError> {
        let depth = self.depth();

        let container = match token.kind {
            TokenKind::ObjectOpen => Some(ContainerKind::Object),
            TokenKind::ArrayOpen => Some(ContainerKind::Array),
            _ => None,
        };

        if let Some(kind) = container {
            if depth >= self.max_depth {
                return Err(StructError {
                    kind: StructErrorKind::TooDeep {
                        limit: self.max_depth,
                    },
                    offset: token.start,
                });
            }
            self.stack.push(Frame {
                kind,
                start: token.start,
                children: 0,
            });
            self.expect = match kind {
                ContainerKind::Object => Expect::KeyOrClose,
                ContainerKind::Array => Expect::ValueOrClose,
            };
            return Ok(Some(Event::Open {
                kind,
                start: token.start,
                depth,
            }));
        }

        if token.kind.is_scalar() {
            self.after_value();
            return Ok(Some(Event::Scalar { token, depth }));
        }

        if token.kind == TokenKind::ArrayClose && self.expect == Expect::ValueOrClose {
            return self.close(ContainerKind::Array, token);
        }

        Err(self.unexpected(token, self.expect))
    }

    /// A token where a key (or `}`) was expected.
    fn on_key(&mut self, token: Token) -> Result<Option<Event>, StructError> {
        if matches!(token.kind, TokenKind::String { .. }) {
            self.expect = Expect::Colon;
            // The key introduces a value one level inside the object.
            return Ok(Some(Event::Key {
                token,
                depth: self.depth(),
            }));
        }

        if token.kind == TokenKind::ObjectClose && self.expect == Expect::KeyOrClose {
            return self.close(ContainerKind::Object, token);
        }

        Err(self.unexpected(token, self.expect))
    }

    /// A token where `,` or a close was expected.
    fn on_comma_or_close(&mut self, token: Token) -> Result<Option<Event>, StructError> {
        // Unwrap-free: `CommaOrClose` is only ever entered with a frame open.
        let Some(frame) = self.stack.last() else {
            return Err(self.unexpected(token, Expect::CommaOrClose));
        };
        let open = frame.kind;

        match token.kind {
            TokenKind::Comma => {
                self.expect = match open {
                    ContainerKind::Object => Expect::Key,
                    ContainerKind::Array => Expect::Value,
                };
                Ok(None)
            }
            TokenKind::ObjectClose | TokenKind::ArrayClose => {
                if token.kind == open.closing() {
                    self.close(open, token)
                } else {
                    Err(StructError {
                        kind: StructErrorKind::Mismatched {
                            open,
                            found: token.kind,
                        },
                        offset: token.start,
                    })
                }
            }
            _ => Err(self.unexpected(token, Expect::CommaOrClose)),
        }
    }

    /// Pop a container and emit its `Close`.
    fn close(&mut self, kind: ContainerKind, token: Token) -> Result<Option<Event>, StructError> {
        // Only called with the matching frame on top.
        let Some(frame) = self.stack.pop() else {
            return Err(self.unexpected(token, self.expect));
        };
        let depth = self.depth();
        self.after_value();

        Ok(Some(Event::Close {
            kind,
            start: frame.start,
            end: token.end,
            children: frame.children,
            depth,
        }))
    }

    /// A value has just been completed: count it against its parent and decide
    /// what may follow.
    fn after_value(&mut self) {
        if let Some(parent) = self.stack.last_mut() {
            parent.children += 1;
            self.expect = Expect::CommaOrClose;
        } else {
            self.completed += 1;
            self.expect = match self.documents {
                Documents::One => Expect::Trailing,
                Documents::Many => Expect::Value,
            };
        }
    }

    fn unexpected(&self, token: Token, expect: Expect) -> StructError {
        StructError {
            kind: StructErrorKind::Unexpected {
                found: token.kind,
                expected: expect.describe(),
            },
            offset: token.start,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;

    /// Walk a whole input, collecting events.
    fn walk(source: &[u8], documents: Documents) -> Result<Vec<Event>, String> {
        walk_in_chunks(source, documents, source.len().max(1))
    }

    fn walk_in_chunks(
        source: &[u8],
        documents: Documents,
        chunk: usize,
    ) -> Result<Vec<Event>, String> {
        let mut lexer = Lexer::new();
        let mut structure = Structure::new(documents);
        let mut events = Vec::new();

        for piece in source.chunks(chunk.max(1)) {
            for token in lexer.feed(piece) {
                let token = token.map_err(|e| e.to_string())?;
                if let Some(event) = structure.push(token).map_err(|e| e.to_string())? {
                    events.push(event);
                }
            }
        }
        if let Some(token) = lexer.finish().map_err(|e| e.to_string())? {
            if let Some(event) = structure.push(token).map_err(|e| e.to_string())? {
                events.push(event);
            }
        }
        structure.finish().map_err(|e| e.to_string())?;
        Ok(events)
    }

    fn err(source: &[u8], documents: Documents) -> StructError {
        let mut lexer = Lexer::new();
        let mut structure = Structure::new(documents);
        let mut failure = None;

        for token in lexer.feed(source) {
            match structure.push(token.expect("lexes cleanly")) {
                Ok(_) => {}
                Err(e) => {
                    failure = Some(e);
                    break;
                }
            }
        }
        failure
            .or_else(|| structure.finish().err())
            .expect("expected a structural error")
    }

    // ---- shape ------------------------------------------------------------

    #[test]
    fn a_scalar_document_is_one_event() {
        let events = walk(b"42", Documents::One).unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], Event::Scalar { depth: 0, .. }));
    }

    #[test]
    fn punctuation_produces_no_events() {
        // `[1,2]` is five tokens and four events: open, two scalars, close.
        let events = walk(b"[1,2]", Documents::One).unwrap();
        assert_eq!(events.len(), 4);
    }

    #[test]
    fn close_carries_the_span_and_the_child_count() {
        // The two things that make an end offset unnecessary per node (C1) and
        // a second walk unnecessary for the scrollbar.
        let events = walk(br#"{"xs":[1,2,3]}"#, Documents::One).unwrap();
        let closes: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                Event::Close {
                    kind,
                    start,
                    end,
                    children,
                    depth,
                } => Some((*kind, *start, *end, *children, *depth)),
                _ => None,
            })
            .collect();

        assert_eq!(
            closes,
            [
                (ContainerKind::Array, 6, 13, 3, 1),
                (ContainerKind::Object, 0, 14, 1, 0),
            ]
        );
    }

    #[test]
    fn an_object_counts_members_not_tokens() {
        let events = walk(br#"{"a":1,"b":2,"c":3}"#, Documents::One).unwrap();
        let children = events
            .iter()
            .find_map(|e| match e {
                Event::Close { children, .. } => Some(*children),
                _ => None,
            })
            .unwrap();
        assert_eq!(children, 3, "three members, not six tokens");
    }

    #[test]
    fn empty_containers_close_with_no_children() {
        for (source, kind) in [
            (&b"[]"[..], ContainerKind::Array),
            (&b"{}"[..], ContainerKind::Object),
            (b"[ ]", ContainerKind::Array),
            (b"{ }", ContainerKind::Object),
        ] {
            let events = walk(source, Documents::One).unwrap();
            assert_eq!(events.len(), 2, "{source:?}");
            assert!(
                matches!(events[1], Event::Close { children: 0, kind: k, .. } if k == kind),
                "{source:?} gave {:?}",
                events[1]
            );
        }
    }

    #[test]
    fn depth_counts_from_zero_at_the_top_level() {
        let events = walk(br#"{"a":{"b":[1]}}"#, Documents::One).unwrap();
        let depths: Vec<u32> = events.iter().map(Event::depth).collect();
        //  open{ key"a" open{ key"b" open[ 1  ]  }  }
        assert_eq!(depths, [0, 1, 1, 2, 2, 3, 2, 1, 0]);
    }

    #[test]
    fn a_key_is_reported_before_its_value() {
        let events = walk(br#"{"k":true}"#, Documents::One).unwrap();
        assert!(matches!(events[1], Event::Key { .. }));
        assert!(matches!(events[2], Event::Scalar { .. }));
        // And the key's span is the quoted string, ready to re-read.
        let Event::Key { token, .. } = events[1] else {
            unreachable!()
        };
        assert_eq!((token.start, token.end), (1, 4));
    }

    #[test]
    fn the_chunk_size_never_changes_the_events() {
        let source = br#"{"a":[1,{"b":null},"x"],"c":{"d":[[]]}}"#;
        let whole = walk_in_chunks(source, Documents::One, source.len()).unwrap();
        for chunk in 1..=source.len() {
            assert_eq!(
                walk_in_chunks(source, Documents::One, chunk).unwrap(),
                whole,
                "chunk size {chunk} disagreed"
            );
        }
    }

    // ---- documents --------------------------------------------------------

    #[test]
    fn ndjson_is_a_stream_of_top_level_values() {
        let source = b"{\"a\":1}\n{\"a\":2}\n{\"a\":3}\n";
        let events = walk(source, Documents::Many).unwrap();

        let starts: Vec<u64> = events
            .iter()
            .filter(|e| e.depth() == 0 && matches!(e, Event::Open { .. }))
            .map(Event::start)
            .collect();
        // Exactly the tier-1 record table for this file.
        assert_eq!(starts, [0, 8, 16]);
    }

    #[test]
    fn a_second_top_level_value_is_an_error_in_single_document_mode() {
        let e = err(b"{\"a\":1}\n{\"a\":2}\n", Documents::One);
        assert_eq!(e.kind, StructErrorKind::TrailingContent);
        assert_eq!(e.offset, 8);
    }

    #[test]
    fn records_are_counted() {
        let mut lexer = Lexer::new();
        let mut structure = Structure::new(Documents::Many);
        for token in lexer.feed(b"1\n2\n3\n4\n") {
            structure.push(token.unwrap()).unwrap();
        }
        if let Some(token) = lexer.finish().unwrap() {
            structure.push(token).unwrap();
        }
        structure.finish().unwrap();
        assert_eq!(structure.completed(), 4);
    }

    #[test]
    fn a_stream_may_be_separated_by_any_whitespace() {
        // Tools that mean NDJSON but forget the newline are common enough that
        // rejecting them would be pedantry, not correctness.
        assert!(walk(b"{\"a\":1} {\"a\":2}", Documents::Many).is_ok());
        assert!(walk(b"[1][2]", Documents::Many).is_ok());
    }

    #[test]
    fn empty_input_is_valid_in_both_modes() {
        assert_eq!(walk(b"", Documents::One).unwrap(), []);
        assert_eq!(walk(b"   \n ", Documents::Many).unwrap(), []);
    }

    #[test]
    fn a_single_document_is_also_a_valid_stream_of_one() {
        // Which is why misdetecting the format is a recoverable annoyance and
        // not a parse failure: `Many` accepts everything `One` accepts.
        let source = br#"{"a":[1,2]}"#;
        assert_eq!(
            walk(source, Documents::Many).unwrap(),
            walk(source, Documents::One).unwrap()
        );
    }

    // ---- errors -----------------------------------------------------------

    #[test]
    fn mismatched_brackets_are_caught() {
        let e = err(b"{\"a\":1]", Documents::One);
        assert_eq!(
            e.kind,
            StructErrorKind::Mismatched {
                open: ContainerKind::Object,
                found: TokenKind::ArrayClose
            }
        );
        assert_eq!(e.offset, 6);
    }

    #[test]
    fn trailing_commas_are_rejected() {
        assert!(matches!(
            err(b"[1,]", Documents::One).kind,
            StructErrorKind::Unexpected { .. }
        ));
        assert!(matches!(
            err(br#"{"a":1,}"#, Documents::One).kind,
            StructErrorKind::Unexpected { .. }
        ));
    }

    #[test]
    fn leading_and_doubled_commas_are_rejected() {
        assert!(matches!(
            err(b"[,1]", Documents::One).kind,
            StructErrorKind::Unexpected { .. }
        ));
        assert!(matches!(
            err(b"[1,,2]", Documents::One).kind,
            StructErrorKind::Unexpected { .. }
        ));
    }

    #[test]
    fn an_object_requires_string_keys_and_colons() {
        // Note `{a:1}` is absent: an unquoted key never becomes a token at all,
        // so the lexer rejects it before this layer sees anything. The cases
        // here are the ones that lex perfectly well and are still not JSON.
        assert!(matches!(
            err(br#"{1:2}"#, Documents::One).kind,
            StructErrorKind::Unexpected {
                found: TokenKind::Number { .. },
                ..
            }
        ));
        assert!(matches!(
            err(br#"{"a" 1}"#, Documents::One).kind,
            StructErrorKind::Unexpected { .. }
        ));
        assert!(matches!(
            err(br#"{"a":}"#, Documents::One).kind,
            StructErrorKind::Unexpected { .. }
        ));
    }

    #[test]
    fn a_bare_value_is_not_a_member() {
        assert!(matches!(
            err(br#"{"a"}"#, Documents::One).kind,
            StructErrorKind::Unexpected { .. }
        ));
    }

    #[test]
    fn unclosed_containers_are_reported_at_end_of_input() {
        // The truncated-dump case, from the structural side: the lexer is happy
        // (every token was well formed) and this layer is the one that objects.
        let e = err(br#"{"a":[1,2"#, Documents::One);
        assert_eq!(
            e.kind,
            StructErrorKind::Unclosed {
                open: 2,
                innermost: ContainerKind::Array
            }
        );
    }

    #[test]
    fn input_ending_mid_member_is_reported() {
        assert!(matches!(
            err(br#"{"a":"#, Documents::One).kind,
            StructErrorKind::Unclosed { .. }
        ));
        // A stream that ends after a comma has nothing open but is still cut off.
        assert!(matches!(
            err(b"1 2 ,", Documents::Many).kind,
            StructErrorKind::Unexpected { .. }
        ));
    }

    #[test]
    fn a_stray_close_is_an_error() {
        assert!(matches!(
            err(b"]", Documents::One).kind,
            StructErrorKind::Unexpected { .. }
        ));
        assert!(matches!(
            err(b"[1]]", Documents::One).kind,
            StructErrorKind::TrailingContent
        ));
    }

    // ---- depth ------------------------------------------------------------

    #[test]
    fn deep_nesting_within_the_limit_is_walked() {
        const DEPTH: usize = 100_000;
        let mut source = vec![b'['; DEPTH];
        source.extend(core::iter::repeat_n(b']', DEPTH));

        let events = walk_in_chunks(&source, Documents::One, 4096).unwrap();
        assert_eq!(events.len(), DEPTH * 2);
        assert_eq!(events[DEPTH - 1].depth(), DEPTH as u32 - 1);
    }

    #[test]
    fn nesting_past_the_limit_is_an_error_not_a_crash() {
        // The bound exists so that a file of nothing but `[` costs a bounded
        // amount of memory and then a diagnostic, rather than the allocator.
        let source = vec![b'['; 100];
        let mut lexer = Lexer::new();
        let mut structure = Structure::new(Documents::One).with_max_depth(16);
        let mut failure = None;
        for token in lexer.feed(&source) {
            if let Err(e) = structure.push(token.unwrap()) {
                failure = Some(e);
                break;
            }
        }
        assert_eq!(
            failure.unwrap().kind,
            StructErrorKind::TooDeep { limit: 16 }
        );
    }

    #[test]
    fn depth_and_completed_are_observable_mid_walk() {
        let mut lexer = Lexer::new();
        let mut structure = Structure::new(Documents::Many);
        let mut depths = Vec::new();
        for token in lexer.feed(b"[[1]]\n[2]\n") {
            structure.push(token.unwrap()).unwrap();
            depths.push(structure.depth());
        }
        assert_eq!(depths, [1, 2, 2, 1, 0, 1, 1, 0]);
        assert_eq!(structure.completed(), 2);
        assert!(!structure.in_document());
    }

    #[test]
    fn errors_render_for_humans() {
        let text = err(b"{\"a\":1]", Documents::One).to_string();
        assert!(text.contains("array-close"), "{text}");
        assert!(text.contains("byte 6"), "{text}");

        let text = err(b"[1,]", Documents::One).to_string();
        assert!(text.contains("expected a value"), "{text}");
    }
}
