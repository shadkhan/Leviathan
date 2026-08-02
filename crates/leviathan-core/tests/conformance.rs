//! RFC 8259 conformance, as a committed corpus.
//!
//! The cases below are modelled on [JSONTestSuite][suite] and use its naming
//! convention — `y_` must be accepted, `n_` must be rejected, `i_` is left to
//! the implementation and our choice is recorded. They are written out here
//! rather than vendored so that `cargo test` on a clean clone proves conformance
//! with no download, no submodule and no network. The full external corpus is
//! run separately by `leviathan conformance <dir>`, which is the same predicate
//! against ~300 more files.
//!
//! [suite]: https://github.com/nst/JSONTestSuite
//!
//! ## What is being tested
//!
//! The predicate is the engine's own: lex the bytes, feed every token to the
//! grammar walk in single-document mode, flush, and close. That is exactly what
//! `bench walk` measures and what M3's validation will report on, so a case
//! passing here is a statement about the shipping code rather than about a test
//! harness.
//!
//! Every case is additionally run at **three chunk sizes** — one byte at a time,
//! three at a time, and whole. A parser that is correct only when the input
//! arrives in one piece is not a streaming parser, and a chunk boundary inside a
//! `\uD83D` escape is the case that finds out.

use leviathan_core::{Documents, Lexer, Structure};

/// Whether the engine accepts `bytes` as one well-formed JSON document.
fn accepts(bytes: &[u8], chunk: usize) -> Result<(), String> {
    let mut lexer = Lexer::new();
    let mut structure = Structure::new(Documents::One);

    for piece in bytes.chunks(chunk.max(1)) {
        for token in lexer.feed(piece) {
            let token = token.map_err(|e| e.to_string())?;
            structure.push(token).map_err(|e| e.to_string())?;
        }
    }

    // A number is the only token that cannot be emitted until the byte after it
    // arrives, so skipping this loses the final value of `[1,2,3` — see
    // DEEP_REASONING C30/C37.
    if let Some(token) = lexer.finish().map_err(|e| e.to_string())? {
        structure.push(token).map_err(|e| e.to_string())?;
    }
    structure.finish().map_err(|e| e.to_string())?;
    Ok(())
}

/// Run one case at every chunk size and require the same verdict from each.
fn verdict(bytes: &[u8]) -> Result<(), String> {
    let whole = accepts(bytes, bytes.len().max(1));
    for chunk in [1usize, 3] {
        let piecemeal = accepts(bytes, chunk);
        assert_eq!(
            whole.is_ok(),
            piecemeal.is_ok(),
            "chunk size {chunk} changed the verdict: whole={whole:?} chunked={piecemeal:?}"
        );
    }
    whole
}

fn accept(name: &str, bytes: &[u8]) {
    if let Err(why) = verdict(bytes) {
        panic!("{name}: should have been accepted, but was rejected — {why}");
    }
}

fn reject(name: &str, bytes: &[u8]) {
    if verdict(bytes).is_ok() {
        panic!("{name}: should have been rejected, but was accepted");
    }
}

// ---------------------------------------------------------------- y_ accept

#[test]
fn y_structures() {
    accept("y_array_empty", b"[]");
    accept("y_array_empty_string", b"[\"\"]");
    accept("y_object_empty", b"{}");
    accept("y_object_empty_key", b"{\"\":0}");
    accept("y_object_simple", b"{\"a\":1}");
    accept(
        "y_object_several",
        b"{\"a\":1,\"b\":[2,3],\"c\":{\"d\":null}}",
    );
    accept("y_array_heterogeneous", b"[null,true,false,0,\"x\",[],{}]");
    accept("y_nested", b"[[[[[[[[[[[[[[[[[[[[]]]]]]]]]]]]]]]]]]]]");
    accept("y_array_with_trailing_space", b"[2] ");
    accept("y_whitespace_everywhere", b" [ 1 , 2 ] ");
    accept("y_tab_and_newline_between_tokens", b"[\n\t1,\r\n2\n]");
}

#[test]
fn y_scalars_at_the_top_level() {
    // RFC 8259 §2: a JSON text is any value, not only an object or array.
    accept("y_toplevel_number", b"42");
    accept("y_toplevel_negative", b"-1");
    accept("y_toplevel_string", b"\"hello\"");
    accept("y_toplevel_true", b"true");
    accept("y_toplevel_false", b"false");
    accept("y_toplevel_null", b"null");
}

#[test]
fn y_numbers() {
    accept("y_number_0e1", b"[0e1]");
    accept("y_number_0e+1", b"[0e+1]");
    accept("y_number_after_space", b"[ 4]");
    accept("y_number_negative_zero", b"[-0]");
    accept("y_number_negative_int", b"[-123]");
    accept("y_number_real_capital_e", b"[1E22]");
    accept("y_number_real_exponent", b"[123e45]");
    accept("y_number_real_fraction_exponent", b"[123.456e78]");
    accept("y_number_real_neg_exp", b"[1e-2]");
    accept("y_number_simple_real", b"[123.456789]");
}

#[test]
fn y_strings() {
    accept("y_string_escaped_quote", b"[\"\\\"\"]");
    accept("y_string_escaped_backslash", b"[\"\\\\\"]");
    accept("y_string_solidus", b"[\"\\/\"]");
    accept("y_string_control_escapes", b"[\"\\b\\f\\n\\r\\t\"]");
    accept("y_string_unicode_escape", b"[\"\\u0060\\u012a\\u12AB\"]");
    accept("y_string_escaped_null", b"[\"\\u0000\"]");
    accept("y_string_surrogate_pair", b"[\"\\uD83D\\uDE00\"]");
    accept("y_string_utf8_two_byte", "[\"é\"]".as_bytes());
    accept("y_string_utf8_three_byte", "[\"€\"]".as_bytes());
    accept("y_string_utf8_four_byte", "[\"😀\"]".as_bytes());
    accept("y_string_space", b"[\" \"]");
    accept("y_string_reverse_solidus_u", b"[\"\\\\u0041\"]");
}

// ---------------------------------------------------------------- n_ reject

#[test]
fn n_commas_and_colons() {
    reject("n_array_trailing_comma", b"[1,]");
    reject("n_object_trailing_comma", b"{\"a\":1,}");
    reject("n_array_leading_comma", b"[,1]");
    reject("n_array_double_comma", b"[1,,2]");
    reject("n_array_just_comma", b"[,]");
    reject("n_array_missing_comma", b"[1 2]");
    reject("n_object_missing_colon", b"{\"a\" 1}");
    reject("n_object_double_colon", b"{\"a\"::1}");
    reject("n_object_missing_value", b"{\"a\":}");
    reject("n_object_missing_key", b"{:1}");
    reject("n_object_comma_instead_of_colon", b"{\"a\",1}");
}

#[test]
fn n_brackets() {
    reject("n_array_unclosed", b"[");
    reject("n_array_unclosed_with_value", b"[1");
    reject("n_object_unclosed", b"{");
    reject("n_object_unclosed_with_member", b"{\"a\":1");
    reject("n_array_mismatch", b"[}");
    reject("n_object_mismatch", b"{]");
    reject("n_close_without_open", b"]");
    reject("n_double_close", b"[]]");
    reject("n_paren", b"()");
}

#[test]
fn n_numbers() {
    reject("n_number_leading_zero", b"[01]");
    reject("n_number_neg_leading_zero", b"[-01]");
    reject("n_number_leading_dot", b"[.5]");
    reject("n_number_trailing_dot", b"[1.]");
    reject("n_number_plus_sign", b"[+1]");
    reject("n_number_bare_minus", b"[-]");
    reject("n_number_exponent_no_digits", b"[1e]");
    reject("n_number_exponent_bare_sign", b"[1e+]");
    reject("n_number_double_exponent", b"[1e1e1]");
    reject("n_number_hex", b"[0x1]");
    reject("n_number_infinity", b"[Infinity]");
    reject("n_number_nan", b"[NaN]");
    reject("n_number_trailing_letter", b"[1a]");
}

#[test]
fn n_literals() {
    reject("n_literal_truncated_true", b"[tru]");
    reject("n_literal_truncated_null", b"[nul]");
    reject("n_literal_capitalised", b"[True]");
    reject("n_literal_uppercase_null", b"[NULL]");
    reject("n_literal_bare_word", b"[undefined]");
}

#[test]
fn n_strings() {
    reject("n_string_unterminated", b"[\"abc]");
    reject("n_string_single_quotes", b"['a']");
    reject("n_string_unquoted_key", b"{a:1}");
    reject("n_string_bad_escape", b"[\"\\x\"]");
    reject("n_string_short_hex", b"[\"\\u00\"]");
    reject("n_string_non_hex", b"[\"\\uZZZZ\"]");
    reject("n_string_escape_at_end", b"[\"abc\\\"]");

    // The property the whole NDJSON path depends on: a raw control character
    // cannot appear inside a string, so a newline is always a record boundary
    // (DEEP_REASONING C21).
    reject("n_string_raw_newline", b"[\"a\nb\"]");
    reject("n_string_raw_tab", b"[\"a\tb\"]");
    reject("n_string_raw_control", b"[\"a\x01b\"]");
}

#[test]
fn n_trailing_content() {
    // Single-document mode: a complete value followed by anything else is an
    // error, and that is what tells a document from an NDJSON stream when the
    // prefix sniff cannot (C19).
    reject("n_two_values", b"[1] [2]");
    reject("n_value_then_junk", b"true story");
    reject("n_number_then_junk", b"1 2");
    reject("n_object_then_comma", b"{},");
}

#[test]
fn n_invalid_utf8() {
    reject("n_string_lone_continuation", b"[\"\x80\"]");
    reject("n_string_truncated_sequence", b"[\"\xC3\"]");
    reject("n_string_overlong_encoding", b"[\"\xC0\xAF\"]");
    reject("n_string_invalid_start_byte", b"[\"\xFF\"]");
}

// ------------------------------------------------- i_ implementation choice

/// Cases RFC 8259 leaves open, with this engine's answer recorded.
///
/// None of these is a bug either way; what matters is that the choice is
/// deliberate and does not drift silently. Each assertion below is a decision.
#[test]
fn i_documented_choices() {
    // ACCEPTED — a number's *value* is never computed by the index (a token is
    // a span, C20), so range is not the lexer's business. A consumer that
    // converts to `f64` gets infinity; the document is still well-formed.
    accept("i_number_huge_exponent", b"[1E400]");
    accept("i_number_huge_integer", b"[123456789012345678901234567890]");
    accept("i_number_tiny_exponent", b"[1E-400]");

    // ACCEPTED — a lone surrogate is syntactically valid `\uXXXX`. It cannot be
    // decoded to a character, so row materialization substitutes U+FFFD at
    // display time rather than refusing to open the file (C6, C34).
    accept("i_string_lone_leading_surrogate", b"[\"\\uD800\"]");
    accept("i_string_lone_trailing_surrogate", b"[\"\\uDEAD\"]");
    accept("i_string_reversed_surrogate_pair", b"[\"\\uDE00\\uD83D\"]");

    // ACCEPTED — a UTF-8 BOM is skipped. Strictly a JSON text should not carry
    // one, but real exports from Windows tooling do, and refusing to open such
    // a file would fail the user this product exists for.
    accept("i_bom_then_value", b"\xEF\xBB\xBF[1]");
    accept("i_bom_only", b"\xEF\xBB\xBF");

    // ACCEPTED — an empty document. There is nothing to show, and the format
    // sniffer reports `empty` so the UI can say so; this is not a parse error.
    accept("i_empty_input", b"");
    accept("i_whitespace_only", b"   \n\t  ");

    // REJECTED — invalid UTF-8 *anywhere*, including outside strings. Some
    // parsers accept raw bytes in the structural positions; this one validates
    // the whole stream, because the row materializer hands text to JavaScript
    // and a `String` that is not UTF-8 has nowhere to go.
    reject("i_invalid_utf8_outside_string", b"[1]\xFF");
}
