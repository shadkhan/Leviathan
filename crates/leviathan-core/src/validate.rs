//! Well-formedness validation with byte-accurate locations, and recovery.
//!
//! ## Why this is not a second parser
//!
//! It is the M1 lexer and the M1 grammar walk, driven to the end of the file
//! instead of stopping at the first thing that fails. Every position it reports
//! comes from the same state machine that produces the index, so an error's
//! offset is the offset the tree will send you to — a validator that agreed with
//! itself but not with the viewer would be worse than none.
//!
//! ## Recovery is per record, and only NDJSON has records
//!
//! C24 put recovery in the caller because "where is it safe to resume" is a
//! question about structure, and the lexer deliberately knows none. This is that
//! caller, and for NDJSON the answer is exact: a newline is always a record
//! boundary (C21), so a bad record can be skipped and the next one validated
//! from a clean state. **A 500 MB log with nine broken lines reports all nine.**
//!
//! A single document gets one error and stops, because that is the honest
//! answer: after a syntax error at depth 12 every subsequent token is being
//! interpreted against a stack that is already wrong, and a list of forty
//! consequential errors is noise wearing a location. The one exception is that
//! everything *before* the error was really validated, and the byte count says
//! so.
//!
//! ## Bounded, like everything else
//!
//! Errors are capped, the scan yields between batches, and neither the error
//! list nor the working state grows with the size of the file.

use crate::format::Format;
use crate::lexer::Lexer;
use crate::source::{ByteRange, SourceError, read_clamped};
use crate::structure::{Documents, Event, Structure};

/// How much a single [`Validate::advance`] call reads before returning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidateOptions {
    /// Bytes per read from the source.
    pub window: u32,
    /// Bytes to check before returning, so a host can paint or cancel.
    pub budget: u64,
    /// Stop after this many errors.
    ///
    /// A file whose every line is broken has as many errors as it has lines, and
    /// nobody reads the ten-thousandth. The cap is what makes "validate
    /// anything" safe to press.
    pub limit: usize,
}

impl Default for ValidateOptions {
    fn default() -> Self {
        Self {
            window: 256 * 1024,
            budget: 8 * 1024 * 1024,
            limit: 1_000,
        }
    }
}

/// One thing wrong with the document, and exactly where.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invalid {
    /// Byte offset of the offending token.
    pub offset: u64,
    /// 1-based line.
    pub line: u64,
    /// 1-based column, in bytes.
    pub column: u64,
    /// What went wrong, phrased for a person.
    pub message: String,
}

/// A validation pass in progress.
#[derive(Debug)]
pub struct Validate {
    /// Whether a newline ends a record, as tier 1 says it does for NDJSON.
    per_line: bool,
    /// Next byte to read.
    cursor: u64,
    /// Line the record in progress started on.
    line: u64,
    lexer: Lexer,
    structure: Structure,
    errors: Vec<Invalid>,
    checked: u64,
    values: u64,
    done: bool,
}

impl Validate {
    /// A validator for a document of the given format.
    ///
    /// Each record is checked as its own document, so a stream is a sequence of
    /// independent verdicts rather than one long one — which is what lets a
    /// broken line be skipped without the next line inheriting its state.
    #[must_use]
    pub fn new(format: Format) -> Self {
        Self {
            per_line: format == Format::Ndjson,
            cursor: 0,
            line: 1,
            lexer: Lexer::new(),
            structure: Structure::new(Documents::One),
            errors: Vec::new(),
            checked: 0,
            values: 0,
            done: false,
        }
    }

    /// Errors found so far, in file order.
    #[must_use]
    pub fn errors(&self) -> &[Invalid] {
        &self.errors
    }

    /// Bytes examined so far — the numerator of a progress bar.
    #[must_use]
    pub const fn checked(&self) -> u64 {
        self.checked
    }

    /// Top-level values seen: records, for NDJSON.
    #[must_use]
    pub const fn values(&self) -> u64 {
        self.values
    }

    /// Whether the pass has finished.
    #[must_use]
    pub const fn is_done(&self) -> bool {
        self.done
    }

    /// Whether the document is well-formed as far as it has been checked.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.errors.is_empty()
    }

    /// Check the next batch.
    ///
    /// Returns the number of bytes examined by this call — zero once finished,
    /// so a caller that keeps going terminates rather than spins.
    ///
    /// # Errors
    ///
    /// Propagates [`SourceError`]. A *content* error is never a `SourceError`:
    /// malformed JSON is the thing being looked for, not a failure to look.
    pub fn advance<S: ByteRange>(
        &mut self,
        source: &mut S,
        options: &ValidateOptions,
    ) -> Result<u64, SourceError> {
        if self.done {
            return Ok(0);
        }

        let start = self.checked;
        let mut budget = options.budget;

        while budget > 0 && !self.done {
            let bytes = read_clamped(source, self.cursor, u64::from(options.window))?;
            if bytes.is_empty() {
                self.close_segment();
                self.finish_pass();
                break;
            }

            // A record ends at a newline, exactly as tier 1 says it does (C27).
            // Validating a stream any other way would let an unclosed bracket on
            // one line swallow the next, and report the error against a record
            // the user can see is fine — a validator disagreeing with the
            // indexer about what a record *is*.
            let split = if self.per_line {
                bytes.iter().position(|b| *b == b'\n')
            } else {
                None
            };
            let take = split.unwrap_or(bytes.len());
            let consumed = split.map_or(bytes.len(), |n| n + 1) as u64;

            let outcome = self.consume(&bytes[..take]);
            self.checked = self.cursor + take as u64;

            match outcome {
                Ok(()) => {
                    if split.is_some() {
                        self.close_segment();
                        self.open_segment(self.cursor + consumed, self.line + 1);
                    }
                }
                Err(failure) => {
                    self.record(failure, options);
                    if self.done {
                        break;
                    }
                    if !self.per_line {
                        self.done = true;
                        break;
                    }
                    // Skip the rest of this record; the next line is a clean
                    // state because a newline is always a boundary.
                    let resume = self.skip_to_newline(source, options)?;
                    match resume {
                        Some(at) => self.open_segment(at, self.line + 1),
                        None => {
                            self.done = true;
                            break;
                        }
                    }
                }
            }

            budget = budget.saturating_sub(consumed.max(1));
            self.cursor = self.cursor.max(self.checked);
        }

        Ok(self.checked.saturating_sub(start))
    }

    /// Begin a fresh record at `offset`, on `line`.
    fn open_segment(&mut self, offset: u64, line: u64) {
        self.lexer = Lexer::resuming_at(offset, line);
        self.structure = Structure::new(Documents::One);
        self.cursor = offset;
        self.checked = offset;
        self.line = line;
    }

    /// Flush and close the record in progress, reporting what it got wrong.
    ///
    /// A blank line closes with nothing fed, and empty input is valid in either
    /// mode — so blank lines in NDJSON are not errors, which is what every tool
    /// that writes NDJSON assumes.
    fn close_segment(&mut self) {
        match self.lexer.finish() {
            Ok(Some(token)) => {
                let position = self.lexer.position_of(token.start);
                match self.structure.push(token) {
                    // A number is only complete once the byte after it arrives,
                    // so a file ending `1\n2\n3` loses its last record without
                    // this (C30, C37 — a fourth sighting).
                    Ok(Some(Event::Open { depth: 0, .. } | Event::Scalar { depth: 0, .. })) => {
                        self.values += 1;
                    }
                    Ok(_) => {}
                    Err(error) => {
                        self.push_error(error.offset, position.line, position.column, &error.kind);
                    }
                }
            }
            Ok(None) => {}
            Err(error) => {
                self.push_error(error.at.offset, error.at.line, error.at.column, &error.kind);
            }
        }
        if let Err(error) = self.structure.finish() {
            let position = self.lexer.position_of(error.offset);
            self.push_error(error.offset, position.line, position.column, &error.kind);
        }
        self.checked = self.checked.max(self.lexer.offset());
    }

    /// Feed one window through the lexer and the grammar.
    ///
    /// A grammar error carries only an offset, and turning that into a line and
    /// column means asking the lexer — which the token iterator has borrowed.
    /// So the failure is carried out of the loop and resolved after it, which is
    /// also when the iterator's consumed count has been folded back into the
    /// lexer's absolute offset (C20).
    fn consume(&mut self, bytes: &[u8]) -> Result<(), Invalid> {
        let mut lexed: Option<Invalid> = None;
        let mut structural: Option<(u64, String)> = None;
        let mut values = 0u64;

        {
            let Self {
                lexer, structure, ..
            } = self;

            for token in lexer.feed(bytes) {
                let token = match token {
                    Ok(token) => token,
                    Err(error) => {
                        lexed = Some(Invalid {
                            offset: error.at.offset,
                            line: error.at.line,
                            column: error.at.column,
                            message: error.kind.to_string(),
                        });
                        break;
                    }
                };

                let start = token.start;
                match structure.push(token) {
                    Ok(Some(Event::Open { depth: 0, .. } | Event::Scalar { depth: 0, .. })) => {
                        values += 1;
                    }
                    Ok(_) => {}
                    Err(error) => {
                        structural = Some((error.offset.max(start), error.kind.to_string()));
                        break;
                    }
                }
            }
        }

        self.values += values;

        if let Some(invalid) = lexed {
            return Err(invalid);
        }
        if let Some((offset, message)) = structural {
            let position = self.lexer.position_of(offset);
            return Err(Invalid {
                offset,
                line: position.line,
                column: position.column,
                message,
            });
        }
        Ok(())
    }

    /// Close the pass, and answer the question opening the file did not.
    ///
    /// RFC 8259 requires a JSON text to contain a value, so empty input — an
    /// empty file, whitespace only, a lone BOM — is **not valid JSON**. The
    /// viewer opens it anyway and reports the format as `empty`, because
    /// refusing a zero-byte file is the "it won't open" failure this project
    /// replaces (C6), and JSONTestSuite records that as one of three deliberate
    /// deviations.
    ///
    /// Those are two different questions and they were being answered by one
    /// predicate (SPEC §6 open question 6). This is where they part: *opening*
    /// still succeeds, and *validating* says what is true — there is no JSON
    /// value here. Said once, at offset 0, rather than as a syntax error, since
    /// there is no syntax to be wrong.
    fn finish_pass(&mut self) {
        if self.values == 0 && self.errors.is_empty() {
            self.errors.push(Invalid {
                offset: 0,
                line: 1,
                column: 1,
                message: "no JSON value: the document is empty".to_string(),
            });
        }
        self.done = true;
    }

    /// Note an error, and stop if the cap is reached.
    fn record(&mut self, failure: Invalid, options: &ValidateOptions) {
        if self.errors.len() >= options.limit {
            self.done = true;
            return;
        }
        self.errors.push(failure);
    }

    /// The byte after the next newline at or beyond the cursor.
    fn skip_to_newline<S: ByteRange>(
        &mut self,
        source: &mut S,
        options: &ValidateOptions,
    ) -> Result<Option<u64>, SourceError> {
        let mut at = self.checked;
        loop {
            let bytes = read_clamped(source, at, u64::from(options.window))?;
            if bytes.is_empty() {
                return Ok(None);
            }
            if let Some(found) = bytes.iter().position(|b| *b == b'\n') {
                return Ok(Some(at + found as u64 + 1));
            }
            at += bytes.len() as u64;
        }
    }

    fn push_error(&mut self, offset: u64, line: u64, column: u64, kind: &impl core::fmt::Display) {
        self.errors.push(Invalid {
            offset,
            line,
            column,
            message: kind.to_string(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn validate(source: &[u8], format: Format) -> Validate {
        let mut pass = Validate::new(format);
        let mut bytes = source;
        let options = ValidateOptions::default();
        while !pass.is_done() {
            pass.advance(&mut bytes, &options).unwrap();
        }
        pass
    }

    #[test]
    fn a_well_formed_document_reports_nothing() {
        let pass = validate(br#"{"a":[1,2,{"b":null}]}"#, Format::SingleDocument);
        assert!(pass.is_valid());
        assert_eq!(pass.errors(), []);
        assert_eq!(pass.checked(), 22);
    }

    #[test]
    fn well_formed_ndjson_reports_nothing_and_counts_records() {
        let pass = validate(b"{\"a\":1}\n{\"a\":2}\n{\"a\":3}\n", Format::Ndjson);
        assert!(pass.is_valid());
        assert_eq!(pass.values(), 3);
    }

    #[test]
    fn an_error_carries_a_usable_location() {
        // Line 2, and the column of the offending byte — the whole point of M3.
        let pass = validate(b"{\n  \"a\": tru\n}", Format::SingleDocument);
        let error = &pass.errors()[0];
        assert_eq!(error.line, 2, "the line the bad token is on");
        assert!(error.column > 1, "and a column inside that line");
        assert!(!error.message.is_empty());
    }

    #[test]
    fn every_broken_record_is_reported_not_only_the_first() {
        // The reason recovery exists. A log with three bad lines among six must
        // report three, or the user fixes one and runs it again, three times.
        let source = b"{\"a\":1}\n{bad\n{\"a\":3}\n[1,2\n{\"a\":5}\ntru\n";
        let pass = validate(source, Format::Ndjson);

        assert_eq!(pass.errors().len(), 3, "{:?}", pass.errors());
        assert_eq!(
            pass.errors().iter().map(|e| e.line).collect::<Vec<_>>(),
            [2, 4, 6],
            "one per broken line, at the right lines"
        );
    }

    #[test]
    fn recovery_keeps_counting_lines() {
        // An error's line number is only useful if it is the line the user sees
        // in their editor. Resuming with a fresh lexer that started at line 1
        // would report every later error against the wrong line.
        let source = b"{\"ok\":1}\n{bad\n{\"ok\":1}\n{\"ok\":1}\n{also bad\n";
        let pass = validate(source, Format::Ndjson);
        assert_eq!(
            pass.errors().iter().map(|e| e.line).collect::<Vec<_>>(),
            [2, 5]
        );
    }

    #[test]
    fn a_single_document_reports_one_error_and_stops() {
        // Everything after a syntax error is interpreted against a stack that is
        // already wrong, so the rest would be consequences, not findings.
        let pass = validate(br#"{"a": , "b": , "c": }"#, Format::SingleDocument);
        assert_eq!(pass.errors().len(), 1);
        assert!(pass.is_done());
    }

    #[test]
    fn a_truncated_document_is_reported_at_its_end() {
        // The fixture the product exists for: a killed export. The error is at
        // end-of-input, not at some arbitrary earlier byte.
        let pass = validate(b"[1,2,3", Format::SingleDocument);
        assert_eq!(pass.errors().len(), 1);
        assert!(pass.errors()[0].offset >= 5, "{:?}", pass.errors()[0]);
    }

    #[test]
    fn a_final_record_without_a_newline_is_still_validated() {
        // C30/C37, a fourth time: a number is only complete when the byte after
        // it arrives, so the last value of a file with no trailing newline is
        // the one a missing flush loses.
        assert!(validate(b"1\n2\n3", Format::Ndjson).is_valid());
        assert_eq!(validate(b"1\n2\n3", Format::Ndjson).values(), 3);
        assert!(!validate(b"1\n2\n3x", Format::Ndjson).is_valid());
    }

    #[test]
    fn the_error_limit_stops_the_pass() {
        let mut source = Vec::new();
        for _ in 0..100 {
            source.extend_from_slice(b"{bad\n");
        }
        let mut pass = Validate::new(Format::Ndjson);
        let options = ValidateOptions {
            limit: 7,
            ..ValidateOptions::default()
        };
        let mut bytes = source.as_slice();
        while !pass.is_done() {
            pass.advance(&mut bytes, &options).unwrap();
        }
        assert_eq!(pass.errors().len(), 7);
    }

    #[test]
    fn the_window_and_budget_never_change_the_verdict() {
        // The streaming property, for validation: where the chunk boundary falls
        // must not change what is wrong with the file.
        let source = b"{\"a\":1}\n{bad\n{\"a\":\"\\uD83D\\uDE00\"}\n[1,2\n";
        let reference = validate(source, Format::Ndjson);

        for window in [1u32, 2, 7, 64, 4096] {
            for budget in [1u64, 5, 1 << 20] {
                let mut pass = Validate::new(Format::Ndjson);
                let options = ValidateOptions {
                    window,
                    budget,
                    ..ValidateOptions::default()
                };
                let mut bytes = source.as_slice();
                let mut spins = 0;
                while !pass.is_done() {
                    pass.advance(&mut bytes, &options).unwrap();
                    spins += 1;
                    assert!(spins < 10_000, "advance must make progress");
                }
                assert_eq!(
                    pass.errors(),
                    reference.errors(),
                    "window {window}, budget {budget}"
                );
            }
        }
    }

    #[test]
    fn an_empty_document_has_no_json_value() {
        // The open/validate split, settled. RFC 8259 requires a JSON text to
        // contain a value, so these are invalid — and the viewer still opens
        // them, which is the deliberate deviation conformance records.
        for empty in [b"".as_slice(), b"   \n ", b"\xEF\xBB\xBF"] {
            let pass = validate(empty, Format::Empty);
            assert!(!pass.is_valid(), "{empty:?} should not validate");
            assert_eq!(pass.errors().len(), 1);
            assert_eq!(pass.errors()[0].offset, 0);
            assert!(pass.errors()[0].message.contains("no JSON value"));
        }
    }

    #[test]
    fn a_stream_of_only_blank_lines_has_no_json_value_either() {
        let pass = validate(b"\n\n   \n\n", Format::Ndjson);
        assert!(!pass.is_valid());
        assert_eq!(pass.values(), 0);
    }

    #[test]
    fn one_value_is_enough_to_not_be_empty() {
        // The boundary of the rule: the check is "no value at all", not "few".
        assert!(validate(b"null", Format::SingleDocument).is_valid());
        assert!(validate(b"\n\n{\"a\":1}\n\n", Format::Ndjson).is_valid());
    }
}
