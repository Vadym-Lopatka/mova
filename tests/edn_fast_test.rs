//! edn/fast differential battery (owner spec, edn/fast branch): pins the
//! golden rule -- `try_read_edn` either fully handles an input and produces
//! a `Value` byte-identical (by `pr_str` and `Value` equality) to what the
//! general reader would have produced, or it bails (`None`) and the input
//! must be one this module's own doc documents as outside its supported
//! subset.
//!
//! Two harnesses:
//! - the full edn.c fast-edn corpus (every file `benches/edn_split_probe.rs`
//!   also measures), read from disk;
//! - a hand-written edge battery covering every documented bail trigger
//!   plus a few near-misses that must NOT bail: numeric-literal edge cases
//!   (19-digit int, `1N`, `1/2`, `3.0M` -- all legitimate `parse_number`
//!   productions this module reuses verbatim) and, as of the string-escape
//!   wave, ordinary escaped strings (`\n \t \r \\ \" \b \f`, BMP `\uXXXX`,
//!   octal) -- only a surrogate-range `\uXXXX` or a genuinely invalid
//!   escape still bails.

use mova::internal::reader::{form_to_value, read_one_user, try_read_edn};
use mova::internal::{pr_str, Value};

/// EDN corpus directory (`MOVA_EDN_CORPUS_DIR`); tests that need it skip when unset.
fn corpus_dir() -> Option<String> {
    let v = std::env::var("MOVA_EDN_CORPUS_DIR").ok();
    if v.is_none() {
        println!("skipped: set MOVA_EDN_CORPUS_DIR");
    }
    v
}

const CORPUS_FILES: &[&str] = &[
    "basic_10.edn",
    "basic_100.edn",
    "basic_1000.edn",
    "basic_10000.edn",
    "basic_100000.edn",
    "keywords_10.edn",
    "keywords_100.edn",
    "keywords_1000.edn",
    "keywords_10000.edn",
    "ints_1400.edn",
    "strings_1000.edn",
    "strings_uni_250.edn",
    "nested_100000.edn",
];

/// The general reader's own `Value`, or `Nil` at true EOF -- matching what
/// `(read-string s)` itself would produce (see `builtins::reflect::
/// read_string`'s `None => Ok(Value::Nil)` arm). Panics if the general
/// reader errors -- every caller of this helper is a case expected to
/// parse cleanly.
fn general_value(src: &str) -> Value {
    match read_one_user(src) {
        Ok(Some(form)) => form_to_value(&form),
        Ok(None) => Value::Nil,
        Err(e) => panic!("general reader errored on {src:?}: {e:?}"),
    }
}

/// Asserts `try_read_edn` produced a value byte-identical (by `pr_str` and
/// `Value::eq`) to the general reader's own result for the same source --
/// the differential battery's core assertion, factored out since both the
/// corpus loop and the edge-battery loop need it.
fn assert_matches_general(src: &str) {
    let fast = try_read_edn(src).unwrap_or_else(|| panic!("expected Some for {src:?}, got None"));
    let general = general_value(src);
    assert_eq!(
        pr_str(&fast),
        pr_str(&general),
        "pr_str mismatch for {src:?}"
    );
    assert!(
        fast == general,
        "Value equality mismatch for {src:?}: fast={}, general={}",
        pr_str(&fast),
        pr_str(&general)
    );
}

#[test]
fn corpus_files_that_take_the_fast_path_match_general() {
    let Some(dir) = corpus_dir() else { return };
    for name in CORPUS_FILES {
        let src = std::fs::read_to_string(format!("{dir}/{name}"))
            .unwrap_or_else(|e| panic!("reading corpus file {name}: {e}"));
        match try_read_edn(&src) {
            Some(fast) => {
                let general = general_value(&src);
                assert_eq!(pr_str(&fast), pr_str(&general), "pr_str mismatch for {name}");
                assert!(fast == general, "Value mismatch for {name}");
            }
            None => {
                // Bailed -- fine, as long as the general reader itself
                // still parses this file without error (a bail must never
                // mean "this file is unparseable", only "the fast path
                // didn't try").
                match read_one_user(&src) {
                    Ok(_) => {}
                    Err(e) => panic!("{name} bailed AND the general reader errored on it: {e:?}"),
                }
            }
        }
    }
}

/// 2b: every one of these MUST take the fast path (`Some`) -- pins the
/// corpus files the perf story depends on, so a future change that
/// silently widens the bail surface for one of them fails loudly here
/// rather than only showing up as a probe regression.
///
/// String-escape wave: `strings_uni_250.edn` and `nested_100000.edn` used
/// to be the two documented bails (both are entirely escape-driven --
/// `strings_uni_250` is ALL `\uXXXX` BMP escapes, no raw non-ASCII bytes;
/// `nested_100000` has 340 backslash bytes, all `\\` and `\"` -- see
/// `git log`/the Wave-2 report for the measured breakdown). Now that
/// `parse_string_escaped`/`decode_escape` support the common escape set,
/// BOTH take the fast path: verified directly (neither contains a
/// `\uXXXX` landing in the `0xD800..=0xDFFF` surrogate range, the one
/// remaining string-escape bail trigger) and asserted here so a future
/// regression widening the bail surface again fails loudly.
#[test]
fn expected_fast_path_corpus_files_all_succeed() {
    let Some(dir) = corpus_dir() else { return };
    let must_succeed = [
        "basic_10.edn",
        "basic_100.edn",
        "basic_1000.edn",
        "basic_10000.edn",
        "basic_100000.edn",
        "keywords_10.edn",
        "keywords_100.edn",
        "keywords_1000.edn",
        "keywords_10000.edn",
        "ints_1400.edn",
        "strings_1000.edn",
        "strings_uni_250.edn",
        "nested_100000.edn",
    ];
    for name in must_succeed {
        let src = std::fs::read_to_string(format!("{dir}/{name}"))
            .unwrap_or_else(|e| panic!("reading corpus file {name}: {e}"));
        let fast = try_read_edn(&src)
            .unwrap_or_else(|| panic!("{name} was expected to take the fast path but bailed"));
        let general = general_value(&src);
        assert_eq!(pr_str(&fast), pr_str(&general), "pr_str mismatch for {name}");
        assert!(fast == general, "Value mismatch for {name}");
    }
}

// --- edge battery -----------------------------------------------------

#[test]
fn empty_string_reads_as_nil() {
    let fast = try_read_edn("").expect("EOF -> Some(Nil)");
    assert_eq!(fast, Value::Nil);
    assert!(read_one_user("").unwrap().is_none()); // general: true EOF, no form
}

#[test]
fn only_whitespace_reads_as_nil() {
    let fast = try_read_edn("   \t\n  ").expect("EOF after trivia -> Some(Nil)");
    assert_eq!(fast, Value::Nil);
    assert!(read_one_user("   \t\n  ").unwrap().is_none());
}

#[test]
fn only_a_comment_reads_as_nil() {
    let fast = try_read_edn("; just a comment, no form").expect("EOF after trivia -> Some(Nil)");
    assert_eq!(fast, Value::Nil);
    assert!(read_one_user("; just a comment, no form").unwrap().is_none());
}

#[test]
fn trailing_garbage_after_first_form_is_ignored() {
    assert_matches_general("(+ 1 2) 3 [this is all garbage {:a b}");
    assert_matches_general("42 (unbalanced");
}

#[test]
fn nineteen_digit_int_does_not_bail() {
    // 19 digits: past the reader's own 18-digit i64-fast-path cutoff, so
    // this exercises `parse_number`'s bignum-promotion path -- still a
    // fully legitimate number, not a bail trigger.
    assert_matches_general("1234567890123456789");
}

#[test]
fn bigint_ratio_bigdecimal_suffixes_do_not_bail() {
    assert_matches_general("1N");
    assert_matches_general("1/2");
    assert_matches_general("3.0M");
    assert_matches_general("-42N");
    assert_matches_general("22/7");
}

#[test]
fn ns_qualified_keyword_and_symbol_do_not_bail() {
    assert_matches_general(":a/b");
    assert_matches_general("clojure.core/map");
}

#[test]
fn all_collection_shapes_round_trip() {
    assert_matches_general("(1 2 3)");
    assert_matches_general("[1 2 3]");
    assert_matches_general("{:a 1 :b 2}");
    assert_matches_general("#{1 2 3}");
    assert_matches_general("[(1 {2 #{3}}) [4 5]]");
    assert_matches_general("{}");
    assert_matches_general("[]");
    assert_matches_general("()");
    assert_matches_general("#{}");
}

/// Every documented bail trigger: asserts `try_read_edn` returns `None`,
/// AND spot-checks the general reader's own behavior on the same input --
/// `Ok` for constructs it legitimately accepts (the fast path merely
/// declines to handle them), `Err` for constructs that are genuine parse
/// errors even on the general reader (duplicate keys, odd maps) -- so a
/// bail here is never silently masking a general-reader regression too.
#[test]
fn documented_bail_triggers() {
    // auto-resolved keyword (`::kw`, `::alias/kw`) -- read-time namespace
    // state this module deliberately never carries.
    assert_bails_general_ok("::kw");
    // metadata.
    assert_bails_general_ok("^{:a 1} [1 2]");
    assert_bails_general_ok("^String x");
    // quote / quasiquote / unquote / unquote-splicing / deref.
    assert_bails_general_ok("'foo");
    assert_bails_general_ok("`foo");
    assert_bails_general_ok("~foo");
    assert_bails_general_ok("~@foo");
    assert_bails_general_ok("@foo");
    // character literal.
    assert_bails_general_ok(r"\a");
    assert_bails_general_ok(r"\newline");
    // `#_` discard -- general reader still reads the SURVIVING form.
    assert_eq!(try_read_edn("#_1 2"), None);
    assert_eq!(general_value("#_1 2"), Value::Int(2));
    // `#uuid` / `#"regex"` / `#?` reader conditional / any other `#tag`.
    assert_eq!(try_read_edn(r#"#uuid "550e8400-e29b-41d4-a716-446655440000""#), None);
    assert!(read_one_user(r#"#uuid "550e8400-e29b-41d4-a716-446655440000""#).is_ok());
    assert_eq!(try_read_edn(r#"#"abc""#), None);
    assert!(read_one_user(r#"#"abc""#).is_ok());
    // `#?(...)` without `{:read-cond :allow}` is a genuine error even on
    // the general (plain `read-string`-shaped) reader -- "Conditional read
    // not allowed" -- so this is BOTH a fast-path bail AND a general-
    // reader error, and that's the correct, matching shape.
    assert_eq!(try_read_edn("#?(:clj 1)"), None);
    assert!(read_one_user("#?(:clj 1)").is_err());
    assert_eq!(try_read_edn("#cpp 300"), None);
    assert!(read_one_user("#cpp 300").is_ok());
    // string escapes this module deliberately still bails on -- see
    // `escaped_strings_match_general` for the (much larger) set that now
    // succeeds.
    //
    // A LONE surrogate half (not immediately followed by its matching
    // other half) is a genuine error on the general reader too --
    // `push_unicode_string_escape` only accepts a high surrogate
    // immediately followed by a valid low surrogate; every other
    // surrogate-range case, high or low, is `Err` there. Only a valid
    // PAIR succeeds on the general reader while this module still bails
    // (it deliberately doesn't replicate the pairing logic).
    assert_bails_general_err("\"\\uD800\""); // lone high surrogate
    assert_bails_general_err("\"\\uDC00\""); // lone low surrogate
    assert_bails_general_ok("\"\\uD83D\\uDE00\""); // valid surrogate PAIR (😀) -- not replicated, bails
    assert_bails_general_err(r#""\q""#); // invalid escape character
    assert_bails_general_err(r#""\400""#); // octal out of [0, 377] range
    assert_bails_general_err("\"\\u12\""); // fewer than 4 hex digits
    // duplicate key / duplicate set element / odd map -- genuine read-time
    // errors on the general reader too.
    assert_bails_general_err("{:a 1 :a 2}");
    assert_bails_general_err("#{1 1}");
    assert_bails_general_err("{:a}");
    // unicode symbol -- any byte >= 0x80 outside a string literal.
    assert_bails_general_ok("héllo");
    assert_bails_general_ok(":héllo");
    // deep nesting past the 200-deep cap -- general reader has no such
    // cap and handles 300 levels fine.
    let deep = format!("{}{}{}", "[".repeat(300), "1", "]".repeat(300));
    assert_eq!(try_read_edn(&deep), None);
    assert!(read_one_user(&deep).is_ok());
}

/// String-escape wave: every supported escape, individually and mixed,
/// plus the exact boundary of the one remaining bail trigger (the
/// surrogate range). Every case here is asserted to MATCH the general
/// reader byte-for-byte (`pr_str` + `Value::eq`) via `assert_matches_general`
/// -- this is the differential battery `decode_escape`'s doc comment
/// promises, not just a shape check.
#[test]
fn escaped_strings_match_general() {
    // Each fixed single-character escape, alone.
    assert_matches_general(r#""\n""#);
    assert_matches_general(r#""\t""#);
    assert_matches_general(r#""\r""#);
    assert_matches_general(r#""\\""#);
    assert_matches_general(r#""\"""#);
    assert_matches_general(r#""\b""#);
    assert_matches_general(r#""\f""#);
    // Escape at the very start / very end of the string body.
    assert_matches_general(r#""\nabc""#);
    assert_matches_general(r#""abc\n""#);
    assert_matches_general(r#""\n""#);
    // Mixed: several different escapes plus plain text runs between them
    // (exercises the memchr2-resume loop in `parse_string_escaped`).
    assert_matches_general(r#""a\nb\tc\rd\\e\"f\bg\fh""#);
    assert_matches_general(r#""line1\nline2\nline3 with \"quotes\" and a \\backslash\\""#);
    // `\uXXXX`, ordinary BMP code points (not landing in the surrogate
    // range): zero, a control char, a printable ASCII char spelled as an
    // escape, and the two values immediately adjacent to the surrogate
    // range on either side.
    assert_matches_general(r#""\u0000""#);
    assert_matches_general(r#""\u0041""#); // 'A'
    assert_matches_general(r#""\uD7FF""#); // one below the surrogate range
    assert_matches_general(r#""\uE000""#); // one above the surrogate range
    assert_matches_general(r#""\uFFFF""#);
    assert_matches_general(r#""prefix\u00e9suffix""#); // 'é' via escape, mid-string
    // Octal escapes: 1, 2, and 3 digits, and the exact max in-range value.
    assert_matches_general("\"\\0\"");
    assert_matches_general("\"\\12\"");
    assert_matches_general("\"\\101\""); // 'A'
    assert_matches_general("\"\\377\""); // max in-range (255)
    // Raw (unescaped) multi-byte UTF-8 text mixed with escapes in the SAME
    // string -- confirms the non-ASCII-bytes-copied-verbatim rule and the
    // escape decode path coexist correctly.
    assert_matches_general("\"héllo\\nwörld\"");
    assert_matches_general("\"日本語\\t\\u00e9\"");
    // Surrogate-range bail: every LONE half (high or low, unpaired) is a
    // genuine error on the general reader too (`push_unicode_string_escape`
    // only pairs a high surrogate immediately followed by a matching low
    // one; every other surrogate-range case is `Err` there), while the one
    // valid PAIR succeeds on the general reader -- this module bails on
    // all five, since it never attempts the pairing logic.
    for src in ["\"\\uD800\"", "\"\\uDBFF\"", "\"\\uDC00\"", "\"\\uDFFF\""] {
        assert_eq!(try_read_edn(src), None, "expected a bail for {src:?}");
        assert!(read_one_user(src).is_err(), "expected the general reader to error on {src:?}");
    }
    assert_eq!(try_read_edn("\"\\uD83D\\uDE00\""), None, "expected a bail for the surrogate pair");
    assert!(read_one_user("\"\\uD83D\\uDE00\"").is_ok(), "general reader should pair-combine this");
    // Invalid escape characters -- genuine errors on the general reader.
    for src in [r#""\q""#, r#""\z""#, r#""\8""#, r#""\9""#] {
        assert_bails_general_err(src);
    }
}

/// Bails, and the general reader accepts the same source cleanly (`Ok`).
fn assert_bails_general_ok(src: &str) {
    assert_eq!(try_read_edn(src), None, "expected a bail for {src:?}");
    match read_one_user(src) {
        Ok(_) => {}
        Err(e) => panic!("expected the general reader to accept {src:?}, got {e:?}"),
    }
}

/// Bails, and the general reader ALSO errors on the same source (a genuine
/// read-time error, not merely an unsupported-by-the-fast-path construct).
fn assert_bails_general_err(src: &str) {
    assert_eq!(try_read_edn(src), None, "expected a bail for {src:?}");
    match read_one_user(src) {
        Err(_) => {}
        Ok(v) => panic!("expected the general reader to error on {src:?}, got {v:?}"),
    }
}
