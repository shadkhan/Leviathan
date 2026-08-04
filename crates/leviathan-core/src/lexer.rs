//! The resumable streaming lexer.
//!
//! This is the byte-level half of M1: it turns a stream of chunks into a stream
//! of tokens, each carrying the exact byte span it occupies in the source. It
//! knows nothing about JSON *structure* — that a `,` is legal here but not
//! there, that an object needs a key before a colon — which is the next layer's
//! job. What it guarantees is that every token it emits is a well-formed JSON
//! token, and that its span is exact.
//!
//! ## Three properties, and why each one matters
//!
//! **It never accumulates.** A token is a span, not a value. A 50 MB string
//! value spanning fifty chunks costs the same state as a 5-byte one: the lexer
//! remembers *where the token started* and *what state it is in*, never the
//! bytes in between. This is what lets a 500 MB document be lexed inside a
//! buffer the size of one chunk (`DEEP_REASONING.md` C20).
//!
//! **It has no stack.** Nesting depth is the structural layer's concern, so a
//! 100 000-deep document is not a recursion hazard here — it is 100 000
//! `ArrayOpen` tokens and a constant-size lexer. There is no input that can
//! overflow the stack, because there is no stack.
//!
//! **It is resumable at any byte.** Chunk boundaries fall wherever the I/O layer
//! puts them, including inside a `\uD83D` escape or between the two halves of a
//! UTF-8 sequence. Feeding one byte at a time must produce exactly the tokens
//! that feeding the whole file at once produces, and a test asserts that at
//! every chunk size.
//!
//! ## Example
//!
//! ```
//! use leviathan_core::{Lexer, TokenKind};
//!
//! let mut lexer = Lexer::new();
//! let mut kinds = Vec::new();
//! for chunk in br#"{"id":42}"#.chunks(3) {
//!     for token in lexer.feed(chunk) {
//!         kinds.push(token.unwrap().kind);
//!     }
//! }
//! assert!(lexer.finish().unwrap().is_none());
//!
//! assert_eq!(kinds, [
//!     TokenKind::ObjectOpen,
//!     TokenKind::String { escaped: false },
//!     TokenKind::Colon,
//!     TokenKind::Number { integer: true },
//!     TokenKind::ObjectClose,
//! ]);
//! ```

use core::fmt;

/// What a token is.
///
/// Two of these carry a flag, and both flags exist to save work later rather
/// than to describe the token more precisely:
///
/// - `String { escaped }` — an unescaped string can be rendered by copying its
///   bytes. Knowing that at lex time means the row materializer does not have to
///   scan for backslashes it will not find, which is the common case.
/// - `Number { integer }` — the tree shows `42` and `42.0` differently, and a
///   future exporter needs to know which are safe as integers. The lexer already
///   walked the grammar, so it knows for free; recovering it later would mean
///   re-reading the bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TokenKind {
    /// `{`
    ObjectOpen,
    /// `}`
    ObjectClose,
    /// `[`
    ArrayOpen,
    /// `]`
    ArrayClose,
    /// `,`
    Comma,
    /// `:`
    Colon,
    /// A string. The span includes both quotes.
    String {
        /// Whether the string contains at least one backslash escape.
        escaped: bool,
    },
    /// A number.
    Number {
        /// Whether the number has neither a fraction nor an exponent, and so
        /// can be read as an integer.
        integer: bool,
    },
    /// `true`
    True,
    /// `false`
    False,
    /// `null`
    Null,
}

impl TokenKind {
    /// A stable lowercase identifier, for diagnostics and the CLI.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            TokenKind::ObjectOpen => "object-open",
            TokenKind::ObjectClose => "object-close",
            TokenKind::ArrayOpen => "array-open",
            TokenKind::ArrayClose => "array-close",
            TokenKind::Comma => "comma",
            TokenKind::Colon => "colon",
            TokenKind::String { .. } => "string",
            TokenKind::Number { .. } => "number",
            TokenKind::True => "true",
            TokenKind::False => "false",
            TokenKind::Null => "null",
        }
    }

    /// Whether this token is a complete JSON value on its own.
    ///
    /// Containers are not: `{` opens a value, it is not one.
    #[must_use]
    pub const fn is_scalar(self) -> bool {
        matches!(
            self,
            TokenKind::String { .. }
                | TokenKind::Number { .. }
                | TokenKind::True
                | TokenKind::False
                | TokenKind::Null
        )
    }
}

/// One JSON token and the exact bytes it occupies.
///
/// The span is half-open (`start..end`) and **includes delimiters**: a string's
/// span covers both quotes. That is deliberate — it makes
/// `source.read(start, end - start)` return exactly the token, which is the
/// contract the row materializer and the error reporter both rely on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token {
    /// What kind of token this is.
    pub kind: TokenKind,
    /// Absolute byte offset of the token's first byte.
    pub start: u64,
    /// Absolute byte offset one past the token's last byte.
    pub end: u64,
}

impl Token {
    /// The token's length in bytes.
    #[must_use]
    pub const fn byte_len(&self) -> u64 {
        self.end - self.start
    }
}

/// Where something is in the source.
///
/// Byte offset is the authoritative coordinate — it is what the index stores and
/// what `ByteRange` speaks. Line and column come along because a human reading
/// an error message needs them and the lexer can supply them for almost nothing
/// (see [`Lexer`] on why newline counting is free).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    /// Absolute byte offset from the start of the source.
    pub offset: u64,
    /// 1-based line number.
    pub line: u64,
    /// 1-based **byte** column within the line.
    ///
    /// Bytes, not characters: a column past a multi-byte character will not
    /// match what a text editor shows. Byte columns are what a `hexdump` and a
    /// `seek` agree on, which is the more useful answer for a file too large to
    /// open in an editor anyway.
    pub column: u64,
}

impl fmt::Display for Position {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "line {}, column {} (byte {})",
            self.line, self.column, self.offset
        )
    }
}

/// What was wrong with the bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LexErrorKind {
    /// A byte that cannot begin any JSON token.
    UnexpectedByte(u8),
    /// A number with a leading zero, like `01`.
    LeadingZero,
    /// A number that runs off the RFC 8259 grammar, like `1.` or `1e+`.
    InvalidNumber(u8),
    /// A literal misspelled partway through, like `tru3`.
    InvalidLiteral {
        /// The literal that was being matched.
        expected: &'static str,
        /// The byte found instead.
        found: u8,
    },
    /// A raw control character (below `0x20`) inside a string. JSON requires
    /// these to be escaped.
    ControlCharacter(u8),
    /// A backslash followed by something that is not an escape.
    InvalidEscape(u8),
    /// A `\u` escape with a non-hexadecimal digit.
    InvalidHexDigit(u8),
    /// A byte sequence inside a string that is not valid UTF-8.
    InvalidUtf8(u8),
    /// The input ended in the middle of a token.
    UnexpectedEof {
        /// What was still open — `"string"`, `"number"`, `"true"`.
        inside: &'static str,
    },
}

impl fmt::Display for LexErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LexErrorKind::UnexpectedByte(b) => {
                write!(f, "{} cannot start a JSON value", Byte(*b))
            }
            LexErrorKind::LeadingZero => {
                write!(f, "numbers may not have leading zeros")
            }
            LexErrorKind::InvalidNumber(b) => {
                write!(f, "{} is not valid inside a number", Byte(*b))
            }
            LexErrorKind::InvalidLiteral { expected, found } => {
                write!(f, "expected `{expected}`, found {}", Byte(*found))
            }
            LexErrorKind::ControlCharacter(b) => {
                write!(
                    f,
                    "unescaped control character {} in string (write it as \\u{:04x})",
                    Byte(*b),
                    b
                )
            }
            LexErrorKind::InvalidEscape(b) => {
                write!(f, "{} is not a valid escape after a backslash", Byte(*b))
            }
            LexErrorKind::InvalidHexDigit(b) => {
                write!(f, "{} is not a hexadecimal digit in a \\u escape", Byte(*b))
            }
            LexErrorKind::InvalidUtf8(b) => {
                write!(f, "{} is not valid UTF-8 here", Byte(*b))
            }
            LexErrorKind::UnexpectedEof { inside } => {
                write!(f, "input ended inside a {inside}")
            }
        }
    }
}

/// A lexing failure, with its exact location.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LexError {
    /// What went wrong.
    pub kind: LexErrorKind,
    /// Where it went wrong.
    pub at: Position,
}

impl fmt::Display for LexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} at {}", self.kind, self.at)
    }
}

impl core::error::Error for LexError {}

/// Render a byte the way an engineer wants to see it in an error message.
struct Byte(u8);

impl fmt::Display for Byte {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_ascii_graphic() {
            write!(f, "`{}`", self.0 as char)
        } else if self.0 == b' ' {
            write!(f, "a space")
        } else {
            write!(f, "byte 0x{:02x}", self.0)
        }
    }
}

/// State inside a string literal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Str {
    /// Scanning ordinary characters.
    Body,
    /// Just consumed a backslash.
    Escape,
    /// Inside `\uXXXX`, having consumed `n` of the four hex digits.
    Hex(u8),
    /// Inside a multi-byte UTF-8 sequence: `need` continuation bytes remain, and
    /// the next one must be within `lo..=hi`.
    Cont { need: u8, lo: u8, hi: u8 },
}

/// State inside a number, named for what has been consumed so far.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Num {
    /// `-`
    Minus,
    /// A leading `0`, which may not be followed by another digit.
    Zero,
    /// Integer digits.
    Int,
    /// `.`
    Dot,
    /// Fraction digits.
    Frac,
    /// `e` or `E`
    Exp,
    /// `e+` or `e-`
    ExpSign,
    /// Exponent digits.
    ExpDigits,
}

impl Num {
    /// Is a number ending here a complete number?
    ///
    /// This is what makes numbers the one token that cannot be emitted on sight.
    /// `12` is complete, but so is the `12` at the front of `123` — nothing but
    /// the *next* byte can tell them apart, and at a chunk boundary that byte
    /// has not arrived yet. Hence [`Lexer::finish`].
    const fn is_complete(self) -> bool {
        matches!(self, Num::Zero | Num::Int | Num::Frac | Num::ExpDigits)
    }
}

/// Where the lexer is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// At the very start, having matched `n` bytes of a UTF-8 BOM.
    Bom(u8),
    /// Between tokens.
    Ready,
    /// Inside a string that began at `tok_start`.
    Str(Str),
    /// Inside a number that began at `tok_start`.
    Num(Num),
    /// Inside `true`, `false` or `null`.
    Lit {
        kind: TokenKind,
        word: &'static str,
        matched: u8,
    },
    /// An error was reported and the lexer will produce nothing further.
    Failed(LexError),
}

/// What one step of the state machine did.
enum Step {
    /// Made progress; go round again.
    Continue,
    /// Ran out of chunk. State is saved; feed the next chunk.
    NeedMore,
    /// A complete token.
    Emit(Token),
    /// A malformed token.
    Fail(LexError),
}

/// A resumable, allocation-free JSON tokenizer.
///
/// # Feeding it
///
/// Call [`feed`](Lexer::feed) with successive chunks and iterate what it
/// returns; call [`finish`](Lexer::finish) when the input is exhausted. The
/// contract is one line long:
///
/// > every chunk must begin at [`Lexer::offset`].
///
/// Which is automatic if you feed consecutive chunks and drain each one. If you
/// stop iterating early, `offset()` tells you exactly where to resume — nothing
/// is buffered, so there is no hidden state to reconcile.
///
/// # Line counting is free, and that is not a coincidence
///
/// The lexer tracks line and column for error messages, which sounds like it
/// should cost a comparison per byte. It does not, because **JSON forbids raw
/// control characters inside strings** — a literal newline in a string is a
/// syntax error, not content. So every newline in a well-formed document is
/// whitespace *between* tokens, and the only place that needs to watch for one
/// is the whitespace-skipping loop, which is already looking at those bytes and
/// is the cheapest loop in the machine. String bodies, the hot path, never check
/// for newlines at all.
///
/// # Errors are sticky
///
/// After a failure, [`feed`](Lexer::feed) yields nothing and
/// [`finish`](Lexer::finish) returns the same error. Recovering from a syntax
/// error is a deliberate act, not something that happens by continuing to call
/// a broken lexer — construct a fresh lexer with
/// [`resuming_at`](Lexer::resuming_at) pointed at wherever recovery should
/// begin (for NDJSON, the next newline). That is how M3's "keep indexing past
/// the error" works without this state machine growing a recovery mode.
#[derive(Debug, Clone)]
pub struct Lexer {
    state: State,
    /// Absolute offset of `chunk[0]` for the chunk being fed.
    base: u64,
    /// Read position within the current chunk.
    pos: usize,
    line: u64,
    /// Absolute offset of the first byte of the current line.
    line_start: u64,
    /// Where the token under construction began.
    tok_start: u64,
    /// Flag for the string under construction.
    escaped: bool,
    /// Flag for the number under construction.
    integer: bool,
}

impl Default for Lexer {
    fn default() -> Self {
        Self::new()
    }
}

impl Lexer {
    /// A lexer positioned at the start of a document.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: State::Bom(0),
            base: 0,
            pos: 0,
            line: 1,
            line_start: 0,
            tok_start: 0,
            escaped: false,
            integer: true,
        }
    }

    /// A lexer positioned partway into a document.
    ///
    /// `offset` must be the start of a JSON value and `line` the 1-based line it
    /// falls on; the column is taken to be 1, so `offset` should be a line start.
    /// Both hold for the two callers that matter: NDJSON record boundaries, and
    /// error recovery that resynchronizes on the next newline.
    ///
    /// This is what makes a 500 MB NDJSON file divisible. The lexer keeps no
    /// state that spans records, so one lexer per byte range — several at once,
    /// on several threads, or one resuming after a bad record — all produce the
    /// same tokens with the same absolute offsets.
    #[must_use]
    pub const fn resuming_at(offset: u64, line: u64) -> Self {
        Self {
            state: State::Ready,
            base: offset,
            pos: 0,
            line,
            line_start: offset,
            tok_start: offset,
            escaped: false,
            integer: true,
        }
    }

    /// How many bytes have been consumed; equivalently, where the next chunk
    /// must begin.
    #[must_use]
    pub const fn offset(&self) -> u64 {
        self.base + self.pos as u64
    }

    /// The 1-based line the lexer is currently on.
    #[must_use]
    pub const fn line(&self) -> u64 {
        self.line
    }

    /// Where the lexer is, as a full position.
    ///
    /// Useful to a layer that consumes tokens and rejects one of them: **a token
    /// can never span a line**, because JSON forbids raw control characters
    /// inside strings and every other token is delimiter-free (C21). So the
    /// line reported here immediately after a token was emitted is that token's
    /// line, and pairing it with the token's own `start` gives an exact
    /// position for an error the lexer itself had no reason to raise.
    #[must_use]
    pub const fn position(&self) -> Position {
        self.position_at(self.offset())
    }

    /// The position of `offset`, assuming it lies on the current line.
    ///
    /// Correct for any offset at or after the current line's first byte, which
    /// is what a consumer rejecting a just-emitted token has.
    #[must_use]
    pub const fn position_of(&self, offset: u64) -> Position {
        self.position_at(offset)
    }

    /// Whether a failure has stopped this lexer.
    #[must_use]
    pub const fn is_failed(&self) -> bool {
        matches!(self.state, State::Failed(_))
    }

    /// Hand the lexer the next chunk of bytes and iterate the tokens in it.
    ///
    /// The chunk must begin at [`offset`](Lexer::offset). Tokens spanning the
    /// end of the chunk are not lost — they are continued when the next chunk
    /// arrives.
    pub fn feed<'l, 'a>(&'l mut self, chunk: &'a [u8]) -> Tokens<'l, 'a> {
        Tokens { lexer: self, chunk }
    }

    /// Signal end of input, and collect the last token if there is one.
    ///
    /// Only a number can be pending here: every other token is self-terminating,
    /// while a number is only known to have ended once a byte that cannot
    /// continue it arrives. Anything else still open at end of input is an
    /// error — a truncated file, which is one of the fixtures precisely because
    /// it is how large exports usually arrive.
    ///
    /// # Errors
    ///
    /// The input ended inside a token, or an earlier failure is being replayed.
    pub fn finish(&mut self) -> Result<Option<Token>, LexError> {
        match self.state {
            State::Failed(err) => Err(err),
            State::Ready | State::Bom(0) => Ok(None),
            State::Num(num) if num.is_complete() => {
                self.state = State::Ready;
                Ok(Some(Token {
                    kind: TokenKind::Number {
                        integer: self.integer,
                    },
                    start: self.tok_start,
                    end: self.offset(),
                }))
            }
            State::Bom(_) => Err(self.stop(LexErrorKind::UnexpectedEof {
                inside: "byte-order mark",
            })),
            State::Num(_) => Err(self.stop(LexErrorKind::UnexpectedEof { inside: "number" })),
            State::Str(_) => Err(self.stop(LexErrorKind::UnexpectedEof { inside: "string" })),
            State::Lit { word, .. } => Err(self.stop(LexErrorKind::UnexpectedEof { inside: word })),
        }
    }

    /// Produce the next token from `chunk`, or `None` if the chunk is spent.
    fn next_token(&mut self, chunk: &[u8]) -> Option<Result<Token, LexError>> {
        loop {
            let step = match self.state {
                State::Failed(_) => return None,
                State::Bom(matched) => self.step_bom(chunk, matched),
                State::Ready => self.step_ready(chunk),
                State::Str(str_state) => self.step_string(chunk, str_state),
                State::Num(num) => self.step_number(chunk, num),
                State::Lit {
                    kind,
                    word,
                    matched,
                } => self.step_literal(chunk, kind, word, matched),
            };

            match step {
                Step::Continue => {}
                Step::NeedMore => return None,
                Step::Emit(token) => return Some(Ok(token)),
                Step::Fail(err) => {
                    self.state = State::Failed(err);
                    return Some(Err(err));
                }
            }
        }
    }

    /// Skip a UTF-8 byte-order mark, which Windows-produced exports routinely
    /// carry and which is not part of the JSON grammar.
    ///
    /// Handled as a state rather than a one-off check because the BOM can be
    /// split across chunks: a first chunk of one byte is legal input, and a
    /// fuzzer will find it.
    fn step_bom(&mut self, chunk: &[u8], matched: u8) -> Step {
        const BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];

        let mut matched = matched;
        while (matched as usize) < BOM.len() {
            let Some(&byte) = chunk.get(self.pos) else {
                self.state = State::Bom(matched);
                return Step::NeedMore;
            };
            if byte != BOM[matched as usize] {
                if matched == 0 {
                    // Ordinary input. Nothing was consumed.
                    self.state = State::Ready;
                    return Step::Continue;
                }
                // A partial BOM: the bytes already consumed cannot begin a value.
                return Step::Fail(self.error_at(self.base, LexErrorKind::UnexpectedByte(BOM[0])));
            }
            self.pos += 1;
            matched += 1;
        }

        // A BOM does not occupy a line or column that anyone should see.
        self.line_start = self.offset();
        self.state = State::Ready;
        Step::Continue
    }

    /// Between tokens: skip whitespace, then start whatever comes next.
    fn step_ready(&mut self, chunk: &[u8]) -> Step {
        // The only loop in the lexer that counts newlines. See the type docs.
        while let Some(&byte) = chunk.get(self.pos) {
            match byte {
                b'\n' => {
                    self.pos += 1;
                    self.line += 1;
                    self.line_start = self.offset();
                }
                b' ' | b'\t' | b'\r' => self.pos += 1,
                _ => break,
            }
        }

        let Some(&byte) = chunk.get(self.pos) else {
            return Step::NeedMore;
        };
        let start = self.offset();

        let structural = match byte {
            b'{' => Some(TokenKind::ObjectOpen),
            b'}' => Some(TokenKind::ObjectClose),
            b'[' => Some(TokenKind::ArrayOpen),
            b']' => Some(TokenKind::ArrayClose),
            b',' => Some(TokenKind::Comma),
            b':' => Some(TokenKind::Colon),
            _ => None,
        };
        if let Some(kind) = structural {
            self.pos += 1;
            return Step::Emit(Token {
                kind,
                start,
                end: start + 1,
            });
        }

        match byte {
            b'"' => {
                self.pos += 1;
                self.tok_start = start;
                self.escaped = false;
                self.state = State::Str(Str::Body);
                Step::Continue
            }
            b'-' | b'0'..=b'9' => {
                self.pos += 1;
                self.tok_start = start;
                self.integer = true;
                self.state = State::Num(match byte {
                    b'-' => Num::Minus,
                    b'0' => Num::Zero,
                    _ => Num::Int,
                });
                Step::Continue
            }
            b't' | b'f' | b'n' => {
                let (kind, word) = match byte {
                    b't' => (TokenKind::True, "true"),
                    b'f' => (TokenKind::False, "false"),
                    _ => (TokenKind::Null, "null"),
                };
                self.pos += 1;
                self.tok_start = start;
                self.state = State::Lit {
                    kind,
                    word,
                    matched: 1,
                };
                Step::Continue
            }
            // Deliberately not consumed, so the error points at the byte itself.
            _ => Step::Fail(self.error(LexErrorKind::UnexpectedByte(byte))),
        }
    }

    /// Inside a string.
    ///
    /// The body loop is the hot path of the whole lexer — most bytes in most
    /// JSON documents are inside strings — so it scans for the four bytes that
    /// need attention (`"`, `\`, a control character, or a non-ASCII lead byte)
    /// and steps over everything else in one go, rather than dispatching the
    /// state machine per byte.
    fn step_string(&mut self, chunk: &[u8], mut str_state: Str) -> Step {
        loop {
            if str_state == Str::Body {
                let rest = &chunk[self.pos..];
                let interesting = rest
                    .iter()
                    .position(|&b| b == b'"' || b == b'\\' || !(0x20..0x80).contains(&b))
                    .unwrap_or(rest.len());
                self.pos += interesting;
            }

            let Some(&byte) = chunk.get(self.pos) else {
                self.state = State::Str(str_state);
                return Step::NeedMore;
            };

            str_state = match str_state {
                Str::Body => match byte {
                    b'"' => {
                        self.pos += 1;
                        self.state = State::Ready;
                        return Step::Emit(Token {
                            kind: TokenKind::String {
                                escaped: self.escaped,
                            },
                            start: self.tok_start,
                            end: self.offset(),
                        });
                    }
                    b'\\' => {
                        self.pos += 1;
                        self.escaped = true;
                        Str::Escape
                    }
                    0x00..=0x1F => {
                        return Step::Fail(self.error(LexErrorKind::ControlCharacter(byte)));
                    }
                    _ => match utf8_lead(byte) {
                        Some((need, lo, hi)) => {
                            self.pos += 1;
                            Str::Cont { need, lo, hi }
                        }
                        None => return Step::Fail(self.error(LexErrorKind::InvalidUtf8(byte))),
                    },
                },
                Str::Escape => match byte {
                    b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => {
                        self.pos += 1;
                        Str::Body
                    }
                    b'u' => {
                        self.pos += 1;
                        Str::Hex(0)
                    }
                    _ => return Step::Fail(self.error(LexErrorKind::InvalidEscape(byte))),
                },
                Str::Hex(seen) => {
                    if !byte.is_ascii_hexdigit() {
                        return Step::Fail(self.error(LexErrorKind::InvalidHexDigit(byte)));
                    }
                    self.pos += 1;
                    if seen == 3 {
                        Str::Body
                    } else {
                        Str::Hex(seen + 1)
                    }
                }
                Str::Cont { need, lo, hi } => {
                    if byte < lo || byte > hi {
                        return Step::Fail(self.error(LexErrorKind::InvalidUtf8(byte)));
                    }
                    self.pos += 1;
                    if need == 1 {
                        Str::Body
                    } else {
                        Str::Cont {
                            need: need - 1,
                            lo: 0x80,
                            hi: 0xBF,
                        }
                    }
                }
            };
        }
    }

    /// Inside a number.
    ///
    /// The grammar is RFC 8259's, transcribed one state per position:
    /// `-? (0 | [1-9][0-9]*) ('.' [0-9]+)? ([eE] [+-]? [0-9]+)?`
    fn step_number(&mut self, chunk: &[u8], mut num: Num) -> Step {
        loop {
            let Some(&byte) = chunk.get(self.pos) else {
                self.state = State::Num(num);
                return Step::NeedMore;
            };

            let next = match (num, byte) {
                (Num::Minus, b'0') => Some(Num::Zero),
                (Num::Minus, b'1'..=b'9') => Some(Num::Int),
                // Reported here rather than left to the structural layer: `01`
                // would otherwise lex as `0` then `1` and be reported as two
                // values where one was expected, which is true but unhelpful.
                (Num::Zero, b'0'..=b'9') => {
                    return Step::Fail(self.error(LexErrorKind::LeadingZero));
                }
                (Num::Int, b'0'..=b'9') => Some(Num::Int),
                (Num::Zero | Num::Int, b'.') => {
                    self.integer = false;
                    Some(Num::Dot)
                }
                (Num::Zero | Num::Int | Num::Frac, b'e' | b'E') => {
                    self.integer = false;
                    Some(Num::Exp)
                }
                (Num::Dot | Num::Frac, b'0'..=b'9') => Some(Num::Frac),
                (Num::Exp, b'+' | b'-') => Some(Num::ExpSign),
                (Num::Exp | Num::ExpSign | Num::ExpDigits, b'0'..=b'9') => Some(Num::ExpDigits),
                _ => None,
            };

            match next {
                Some(advanced) => {
                    self.pos += 1;
                    num = advanced;
                }
                // Not a continuation. If the number is complete, this byte
                // simply terminates it and belongs to whatever comes next.
                None if num.is_complete() => {
                    self.state = State::Ready;
                    return Step::Emit(Token {
                        kind: TokenKind::Number {
                            integer: self.integer,
                        },
                        start: self.tok_start,
                        end: self.offset(),
                    });
                }
                None => return Step::Fail(self.error(LexErrorKind::InvalidNumber(byte))),
            }
        }
    }

    /// Inside `true`, `false` or `null`.
    ///
    /// Unlike a number, a literal is complete the moment its last byte arrives —
    /// no lookahead, so no pending state at end of input. `truex` lexes as
    /// `true` followed by an unexpected `x`, which is the structural layer's
    /// problem and gets the right offset either way.
    fn step_literal(
        &mut self,
        chunk: &[u8],
        kind: TokenKind,
        word: &'static str,
        mut matched: u8,
    ) -> Step {
        let bytes = word.as_bytes();
        loop {
            if matched as usize == bytes.len() {
                self.state = State::Ready;
                return Step::Emit(Token {
                    kind,
                    start: self.tok_start,
                    end: self.offset(),
                });
            }

            let Some(&byte) = chunk.get(self.pos) else {
                self.state = State::Lit {
                    kind,
                    word,
                    matched,
                };
                return Step::NeedMore;
            };

            if byte != bytes[matched as usize] {
                return Step::Fail(self.error(LexErrorKind::InvalidLiteral {
                    expected: word,
                    found: byte,
                }));
            }
            self.pos += 1;
            matched += 1;
        }
    }

    /// Where the lexer is, as a human-readable position.
    ///
    /// The column is saturating. Line tracking only knows about the line the
    /// lexer is *on*, so an offset from an earlier line — which a caller holding
    /// a remembered token offset can legitimately ask about — would otherwise
    /// underflow and panic. Reporting column 1 for such an offset is wrong by a
    /// few characters; panicking inside a validator is wrong by a whole product.
    const fn position_at(&self, offset: u64) -> Position {
        Position {
            offset,
            line: self.line,
            column: offset.saturating_sub(self.line_start) + 1,
        }
    }

    /// An error at the current byte.
    fn error(&self, kind: LexErrorKind) -> LexError {
        self.error_at(self.offset(), kind)
    }

    /// An error at a specific byte.
    fn error_at(&self, offset: u64, kind: LexErrorKind) -> LexError {
        LexError {
            kind,
            at: self.position_at(offset),
        }
    }

    /// Record a terminal failure and return it, so `finish` can replay it.
    fn stop(&mut self, kind: LexErrorKind) -> LexError {
        let err = self.error(kind);
        self.state = State::Failed(err);
        err
    }
}

/// The tokens in one fed chunk.
///
/// Dropping this early is allowed and well defined: the lexer has consumed
/// exactly the bytes it reported tokens for, and [`Lexer::offset`] says where.
/// Nothing is buffered, so there is nothing to lose.
#[must_use = "a chunk that is fed but never iterated produces no tokens"]
pub struct Tokens<'l, 'a> {
    lexer: &'l mut Lexer,
    chunk: &'a [u8],
}

impl Iterator for Tokens<'_, '_> {
    type Item = Result<Token, LexError>;

    fn next(&mut self) -> Option<Self::Item> {
        self.lexer.next_token(self.chunk)
    }
}

impl Drop for Tokens<'_, '_> {
    fn drop(&mut self) {
        // Fold the chunk-relative cursor into the absolute one, so the next
        // chunk starts where this one stopped — whether it was drained or not.
        self.lexer.base += self.lexer.pos as u64;
        self.lexer.pos = 0;
    }
}

/// Decode a UTF-8 lead byte into the number of continuation bytes it needs and
/// the legal range of the first of them.
///
/// The ranges are what make this a validator rather than a byte counter: `0xE0`
/// with a continuation below `0xA0` is an overlong encoding, and `0xED` above
/// `0x9F` is a UTF-16 surrogate. Both are rejected here, which is why a lexed
/// string span is guaranteed to be valid UTF-8 and can be handed to the UI
/// without re-validation.
const fn utf8_lead(byte: u8) -> Option<(u8, u8, u8)> {
    match byte {
        0xC2..=0xDF => Some((1, 0x80, 0xBF)),
        0xE0 => Some((2, 0xA0, 0xBF)),
        0xE1..=0xEC => Some((2, 0x80, 0xBF)),
        0xED => Some((2, 0x80, 0x9F)),
        0xEE..=0xEF => Some((2, 0x80, 0xBF)),
        0xF0 => Some((3, 0x90, 0xBF)),
        0xF1..=0xF3 => Some((3, 0x80, 0xBF)),
        0xF4 => Some((3, 0x80, 0x8F)),
        // 0x80..=0xBF is a stray continuation; 0xC0/0xC1 are overlong;
        // 0xF5.. is beyond U+10FFFF.
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lex a whole input in `chunk`-sized pieces.
    fn lex_in_chunks(input: &[u8], chunk: usize) -> Result<Vec<Token>, LexError> {
        let mut lexer = Lexer::new();
        let mut tokens = Vec::new();
        for piece in input.chunks(chunk.max(1)) {
            for token in lexer.feed(piece) {
                tokens.push(token?);
            }
        }
        if let Some(token) = lexer.finish()? {
            tokens.push(token);
        }
        Ok(tokens)
    }

    fn lex(input: &[u8]) -> Result<Vec<Token>, LexError> {
        lex_in_chunks(input, input.len().max(1))
    }

    fn kinds(input: &[u8]) -> Vec<TokenKind> {
        lex(input).unwrap().into_iter().map(|t| t.kind).collect()
    }

    fn error(input: &[u8]) -> LexError {
        lex(input).expect_err("expected a lex error")
    }

    const STR: TokenKind = TokenKind::String { escaped: false };
    const ESCAPED: TokenKind = TokenKind::String { escaped: true };
    const INT: TokenKind = TokenKind::Number { integer: true };
    const REAL: TokenKind = TokenKind::Number { integer: false };

    #[test]
    fn structural_tokens_have_one_byte_spans() {
        let tokens = lex(b"{}[],:").unwrap();
        let expected = [
            TokenKind::ObjectOpen,
            TokenKind::ObjectClose,
            TokenKind::ArrayOpen,
            TokenKind::ArrayClose,
            TokenKind::Comma,
            TokenKind::Colon,
        ];
        assert_eq!(tokens.len(), expected.len());
        for (i, (token, kind)) in tokens.iter().zip(expected).enumerate() {
            assert_eq!(token.kind, kind);
            assert_eq!((token.start, token.end), (i as u64, i as u64 + 1));
        }
    }

    #[test]
    fn a_string_span_includes_its_quotes() {
        // The materialization contract: read(start, len) must return the token.
        let input = br#"  "hello"  "#;
        let token = lex(input).unwrap()[0];
        assert_eq!(token.kind, STR);
        assert_eq!((token.start, token.end), (2, 9));
        assert_eq!(&input[2..9], br#""hello""#);
        assert_eq!(token.byte_len(), 7);
    }

    #[test]
    fn the_escaped_flag_reports_only_real_escapes() {
        assert_eq!(kinds(br#""plain""#), [STR]);
        assert_eq!(kinds(br#""with \"quote\"""#), [ESCAPED]);
        assert_eq!(kinds(br#""tab\there""#), [ESCAPED]);
        // A slash is not an escape unless it is escaped, and a multi-byte
        // character is not an escape at all — the flag means "contains a
        // backslash", which is the only thing the materializer has to act on.
        assert_eq!(kinds(br#""a/b""#), [STR]);
        assert_eq!(kinds("\"café\"".as_bytes()), [STR]);
    }

    #[test]
    fn every_json_escape_is_accepted() {
        assert_eq!(kinds(br#""\" \\ \/ \b \f \n \r \t A""#), [ESCAPED]);
    }

    #[test]
    fn integers_and_reals_are_distinguished() {
        assert_eq!(kinds(b"0"), [INT]);
        assert_eq!(kinds(b"-0"), [INT]);
        assert_eq!(kinds(b"42"), [INT]);
        assert_eq!(kinds(b"-17"), [INT]);
        assert_eq!(kinds(b"3.14"), [REAL]);
        assert_eq!(kinds(b"1e10"), [REAL]);
        assert_eq!(kinds(b"1E+10"), [REAL]);
        assert_eq!(kinds(b"-2.5e-3"), [REAL]);
        // An exponent makes it a real even with no fraction: 1e2 is not an int
        // in JSON's grammar, and pretending otherwise would lose precision
        // silently on export.
        assert_eq!(kinds(b"1e2"), [REAL]);
    }

    #[test]
    fn literals_are_lexed_whole() {
        assert_eq!(
            kinds(b"[true,false,null]"),
            [
                TokenKind::ArrayOpen,
                TokenKind::True,
                TokenKind::Comma,
                TokenKind::False,
                TokenKind::Comma,
                TokenKind::Null,
                TokenKind::ArrayClose
            ]
        );
    }

    #[test]
    fn a_literal_followed_by_junk_lexes_then_fails_at_the_junk() {
        // Not the lexer's job to know `true` cannot be followed by `x`, but the
        // offset must still land on the `x`.
        let mut lexer = Lexer::new();
        let mut tokens = lexer.feed(b"truex");
        assert_eq!(tokens.next().unwrap().unwrap().kind, TokenKind::True);
        let err = tokens.next().unwrap().unwrap_err();
        assert_eq!(err.kind, LexErrorKind::UnexpectedByte(b'x'));
        assert_eq!(err.at.offset, 4);
    }

    #[test]
    fn whitespace_between_tokens_is_skipped() {
        assert_eq!(
            kinds(b" \t\r\n { \n } \n"),
            [TokenKind::ObjectOpen, TokenKind::ObjectClose]
        );
    }

    #[test]
    fn an_empty_input_yields_nothing() {
        assert_eq!(lex(b"").unwrap(), []);
        assert_eq!(lex(b"   \n  ").unwrap(), []);
    }

    // ---- resumability -----------------------------------------------------

    #[test]
    fn the_chunk_size_never_changes_the_tokens() {
        // The central property. Chunk boundaries land wherever the I/O layer
        // puts them, including mid-escape and mid-UTF-8-sequence, so the only
        // safe assertion is that *every* boundary is invisible.
        let input = concat!(
            r#"{"id":-12.5e+3,"name":"café \" x","tags":["#,
            r#"true,false,null,0,1e2],"nested":{"deep":{"s":"café"}}}"#,
            "\n",
            r#"{"second":"record"}"#,
            "\n"
        )
        .as_bytes();

        let whole = lex_in_chunks(input, input.len()).unwrap();
        assert!(whole.len() > 30, "fixture should be substantial");

        for chunk in 1..=input.len() {
            assert_eq!(
                lex_in_chunks(input, chunk).unwrap(),
                whole,
                "chunk size {chunk} disagreed"
            );
        }
    }

    #[test]
    fn a_number_at_the_very_end_needs_finish_to_emit() {
        // The one token that cannot be emitted on sight: `12` is complete, but
        // so is the `12` at the front of `123`, and only the next byte knows.
        let mut lexer = Lexer::new();
        assert_eq!(lexer.feed(b"12").count(), 0);
        let token = lexer.finish().unwrap().unwrap();
        assert_eq!(token.kind, INT);
        assert_eq!((token.start, token.end), (0, 2));
    }

    #[test]
    fn a_number_split_across_chunks_keeps_one_span() {
        let mut lexer = Lexer::new();
        assert_eq!(lexer.feed(b"-12").count(), 0);
        assert_eq!(lexer.feed(b"3.4").count(), 0);
        let mut tokens = Vec::new();
        for token in lexer.feed(b"5e-6 ") {
            tokens.push(token.unwrap());
        }
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].kind, REAL);
        // `-123.45e-6`, assembled from three chunks that each split it.
        assert_eq!((tokens[0].start, tokens[0].end), (0, 10));
    }

    #[test]
    fn a_string_larger_than_every_chunk_costs_no_extra_state() {
        // The memory claim, in miniature: a value far larger than the buffer it
        // arrives in produces one token and stores none of its bytes.
        let mut input = Vec::from(b"\"");
        input.extend(core::iter::repeat_n(b'x', 100_000));
        input.push(b'"');

        let tokens = lex_in_chunks(&input, 64).unwrap();
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].kind, STR);
        assert_eq!((tokens[0].start, tokens[0].end), (0, 100_002));
    }

    #[test]
    fn feeding_one_byte_at_a_time_tracks_offsets_exactly() {
        let input = b"[1,\n 22,\n 333]";
        let tokens = lex_in_chunks(input, 1).unwrap();
        let numbers: Vec<(u64, u64)> = tokens
            .iter()
            .filter(|t| matches!(t.kind, TokenKind::Number { .. }))
            .map(|t| (t.start, t.end))
            .collect();
        assert_eq!(numbers, [(1, 2), (5, 7), (10, 13)]);
    }

    #[test]
    fn offset_reports_where_the_next_chunk_must_begin() {
        let mut lexer = Lexer::new();
        assert_eq!(lexer.offset(), 0);
        lexer.feed(b"[1,2]").count();
        assert_eq!(lexer.offset(), 5);
        lexer.feed(b"\n[3]").count();
        assert_eq!(lexer.offset(), 9);
    }

    #[test]
    fn abandoning_a_chunk_early_leaves_a_usable_resume_point() {
        // Drop-mid-iteration is well defined because nothing is buffered.
        let input = b"[1,2,3,4,5]";
        let mut lexer = Lexer::new();
        {
            let mut tokens = lexer.feed(input);
            assert_eq!(tokens.next().unwrap().unwrap().kind, TokenKind::ArrayOpen);
        }
        let resume = lexer.offset() as usize;
        assert_eq!(resume, 1);

        let mut rest = Vec::new();
        for token in lexer.feed(&input[resume..]) {
            rest.push(token.unwrap());
        }
        assert_eq!(rest[0].kind, INT);
        assert_eq!((rest[0].start, rest[0].end), (1, 2));
    }

    #[test]
    fn resuming_at_an_offset_produces_absolute_spans() {
        // What makes NDJSON divisible: a lexer started at record N reports the
        // same offsets a lexer started at byte 0 would have reported.
        let document = b"{\"a\":1}\n{\"b\":22}\n";
        let record_two = 8;

        let mut lexer = Lexer::resuming_at(record_two as u64, 2);
        assert_eq!(lexer.offset(), record_two as u64);
        assert_eq!(lexer.line(), 2);

        let mut tokens = Vec::new();
        for token in lexer.feed(&document[record_two..]) {
            tokens.push(token.unwrap());
        }

        let whole = lex(document).unwrap();
        assert_eq!(tokens, whole[5..], "resumed tokens must match a full pass");
        // Record two ends with a newline, which the lexer counts on its way
        // past — so it now sits on line 3, ready for the record after it.
        assert_eq!(lexer.line(), 3);
    }

    // ---- errors -----------------------------------------------------------

    #[test]
    fn errors_carry_line_column_and_byte_offset() {
        let err = error(b"{\n  \"a\": ?\n}");
        assert_eq!(err.kind, LexErrorKind::UnexpectedByte(b'?'));
        assert_eq!(err.at.offset, 9);
        assert_eq!(err.at.line, 2);
        assert_eq!(err.at.column, 8);
    }

    #[test]
    fn line_and_column_survive_chunking() {
        let input = b"[\n1,\n2,\n#]";
        for chunk in 1..=input.len() {
            let mut lexer = Lexer::new();
            let mut found = None;
            for piece in input.chunks(chunk) {
                for token in lexer.feed(piece) {
                    if let Err(err) = token {
                        found = Some(err);
                    }
                }
            }
            let err = found.unwrap_or_else(|| panic!("chunk {chunk}: expected an error"));
            assert_eq!(err.at.line, 4, "chunk {chunk}");
            assert_eq!(err.at.column, 1, "chunk {chunk}");
            assert_eq!(err.at.offset, 8, "chunk {chunk}");
        }
    }

    #[test]
    fn a_raw_control_character_in_a_string_is_rejected() {
        // Also the reason line counting is cheap: a newline inside a string is
        // not content, it is this error.
        let err = error(b"\"line\nbreak\"");
        assert_eq!(err.kind, LexErrorKind::ControlCharacter(b'\n'));
        assert_eq!(err.at.offset, 5);
        assert_eq!(
            error(b"\"tab\there\"").kind,
            LexErrorKind::ControlCharacter(9)
        );
    }

    #[test]
    fn bad_escapes_and_hex_digits_are_rejected() {
        assert_eq!(error(br#""\x""#).kind, LexErrorKind::InvalidEscape(b'x'));
        assert_eq!(
            error(br#""\u12g4""#).kind,
            LexErrorKind::InvalidHexDigit(b'g')
        );
        assert_eq!(
            error(br#""\u12"#).kind,
            LexErrorKind::UnexpectedEof { inside: "string" }
        );
    }

    #[test]
    fn malformed_numbers_are_rejected_with_the_offending_byte() {
        assert_eq!(error(b"01").kind, LexErrorKind::LeadingZero);
        assert_eq!(error(b"-01").kind, LexErrorKind::LeadingZero);
        assert_eq!(
            error(b"1.").kind,
            LexErrorKind::UnexpectedEof { inside: "number" }
        );
        assert_eq!(error(b"1.e5").kind, LexErrorKind::InvalidNumber(b'e'));
        assert_eq!(
            error(b"1e+").kind,
            LexErrorKind::UnexpectedEof { inside: "number" }
        );
        assert_eq!(
            error(b"-").kind,
            LexErrorKind::UnexpectedEof { inside: "number" }
        );
        assert_eq!(error(b"-x").kind, LexErrorKind::InvalidNumber(b'x'));
    }

    #[test]
    fn a_number_terminated_by_a_letter_lexes_then_fails_after_it() {
        // `123abc` is a complete number followed by junk. The number is real;
        // the error belongs to the byte after it.
        let err = error(b"123abc");
        assert_eq!(err.kind, LexErrorKind::UnexpectedByte(b'a'));
        assert_eq!(err.at.offset, 3);
    }

    #[test]
    fn misspelled_literals_are_rejected() {
        assert_eq!(
            error(b"tru3").kind,
            LexErrorKind::InvalidLiteral {
                expected: "true",
                found: b'3'
            }
        );
        assert_eq!(
            error(b"nul").kind,
            LexErrorKind::UnexpectedEof { inside: "null" }
        );
        assert_eq!(error(b"nullable").kind, LexErrorKind::UnexpectedByte(b'a'));
    }

    #[test]
    fn truncated_input_is_an_error_not_a_silent_success() {
        // The `truncated` fixture exists because this is how large exports
        // actually arrive: a killed process, a full disk, a dropped connection.
        assert_eq!(
            error(br#"{"a":"unterminated"#).kind,
            LexErrorKind::UnexpectedEof { inside: "string" }
        );
        assert_eq!(
            error(b"tr").kind,
            LexErrorKind::UnexpectedEof { inside: "true" }
        );
    }

    #[test]
    fn an_error_is_sticky_and_replayed_by_finish() {
        let mut lexer = Lexer::new();
        let mut tokens = lexer.feed(b"[@]");
        assert_eq!(tokens.next().unwrap().unwrap().kind, TokenKind::ArrayOpen);
        assert!(tokens.next().unwrap().is_err());
        // Nothing further, no matter how hard it is asked.
        assert!(tokens.next().is_none());
        assert!(tokens.next().is_none());
        drop(tokens);

        assert!(lexer.is_failed());
        assert_eq!(lexer.feed(b"[1]").count(), 0);
        assert_eq!(
            lexer.finish().unwrap_err().kind,
            LexErrorKind::UnexpectedByte(b'@')
        );
    }

    // ---- UTF-8 ------------------------------------------------------------

    #[test]
    fn valid_utf8_of_every_length_is_accepted() {
        for text in ["\"é\"", "\"€\"", "\"𝄞\"", "\"日本語\"", "\"🐋 leviathan\""] {
            assert_eq!(kinds(text.as_bytes()), [STR], "{text}");
        }
    }

    #[test]
    fn invalid_utf8_inside_a_string_is_rejected() {
        // Each of these is a distinct class of malformation, and the `badutf8`
        // fixture contains all of them.
        let cases: [(&[u8], u8); 6] = [
            (b"\"\xC0\xAF\"", 0xC0),         // overlong two-byte
            (b"\"\xE0\x80\xAF\"", 0x80),     // overlong three-byte
            (b"\"\xED\xA0\x80\"", 0xA0),     // UTF-16 surrogate half
            (b"\"\xF5\x80\x80\x80\"", 0xF5), // beyond U+10FFFF
            (b"\"\x80\"", 0x80),             // stray continuation
            (b"\"\xE2\x28\xA1\"", 0x28),     // truncated sequence
        ];
        for (input, offending) in cases {
            let err = error(input);
            assert_eq!(
                err.kind,
                LexErrorKind::InvalidUtf8(offending),
                "{input:02x?}"
            );
        }
    }

    #[test]
    fn a_multibyte_character_split_across_chunks_is_still_valid() {
        let input = "\"🐋\"".as_bytes();
        for chunk in 1..=input.len() {
            assert_eq!(
                lex_in_chunks(input, chunk).unwrap().len(),
                1,
                "chunk size {chunk}"
            );
        }
    }

    #[test]
    fn utf8_outside_strings_is_still_rejected() {
        // Only string bodies may hold non-ASCII. A stray multi-byte character
        // between tokens is not a UTF-8 question, it is a syntax error.
        assert_eq!(
            error("é".as_bytes()).kind,
            LexErrorKind::UnexpectedByte(0xC3)
        );
    }

    // ---- BOM --------------------------------------------------------------

    #[test]
    fn a_byte_order_mark_is_skipped() {
        let mut input = Vec::from([0xEF, 0xBB, 0xBF]);
        input.extend_from_slice(br#"{"a":1}"#);

        let tokens = lex(&input).unwrap();
        assert_eq!(tokens[0].kind, TokenKind::ObjectOpen);
        // Offsets stay absolute: the `{` really is at byte 3.
        assert_eq!(tokens[0].start, 3);
    }

    #[test]
    fn a_byte_order_mark_split_across_chunks_is_still_skipped() {
        let mut input = Vec::from([0xEF, 0xBB, 0xBF]);
        input.extend_from_slice(b"[1]");
        for chunk in 1..=input.len() {
            let tokens = lex_in_chunks(&input, chunk).unwrap();
            assert_eq!(tokens.len(), 3, "chunk size {chunk}");
            assert_eq!(tokens[0].start, 3, "chunk size {chunk}");
        }
    }

    #[test]
    fn a_lone_byte_order_mark_byte_is_an_error() {
        assert_eq!(
            error(&[0xEF, b'{']).kind,
            LexErrorKind::UnexpectedByte(0xEF)
        );
    }

    // ---- pathological inputs ----------------------------------------------

    #[test]
    fn deep_nesting_is_not_a_recursion_hazard() {
        // There is no stack in this lexer, so there is no input that can
        // overflow one. Depth becomes the structural layer's problem, with an
        // explicit limit, rather than a crash here.
        const DEPTH: usize = 200_000;
        let mut input = vec![b'['; DEPTH];
        input.extend(core::iter::repeat_n(b']', DEPTH));

        let tokens = lex_in_chunks(&input, 4096).unwrap();
        assert_eq!(tokens.len(), DEPTH * 2);
        assert_eq!(tokens[DEPTH - 1].kind, TokenKind::ArrayOpen);
        assert_eq!(tokens[DEPTH].kind, TokenKind::ArrayClose);
    }

    #[test]
    fn an_ndjson_stream_lexes_as_one_flat_token_stream() {
        // No record separator token: to the lexer, NDJSON is just values with
        // whitespace between them. Splitting them into records is the indexer's
        // decision, which is why the same lexer serves both formats.
        let input = b"{\"a\":1}\n{\"a\":2}\n{\"a\":3}\n";
        let tokens = lex_in_chunks(input, 5).unwrap();
        assert_eq!(tokens.len(), 15);
        assert_eq!(tokens[5].kind, TokenKind::ObjectOpen);
        assert_eq!(tokens[5].start, 8);
    }

    #[test]
    fn token_kinds_report_stable_names() {
        assert_eq!(TokenKind::ObjectOpen.as_str(), "object-open");
        assert_eq!(STR.as_str(), "string");
        assert_eq!(INT.as_str(), "number");
        assert!(STR.is_scalar());
        assert!(TokenKind::Null.is_scalar());
        assert!(!TokenKind::ArrayOpen.is_scalar());
    }

    #[test]
    fn errors_render_for_humans() {
        let err = error(b"{\n  \"a\": ?\n}");
        let text = err.to_string();
        assert!(text.contains("`?`"), "{text}");
        assert!(text.contains("line 2"), "{text}");
        assert!(text.contains("byte 9"), "{text}");

        assert!(
            error(b"\"a\nb\"")
                .to_string()
                .contains("unescaped control character"),
        );
        assert!(error(b"\"\x80\"").to_string().contains("byte 0x80"));
    }
}
