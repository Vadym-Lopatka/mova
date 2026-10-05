//! Integration tests for R3: `#"pattern"` reader literal, `Value::Regex`,
//! `re-pattern re-find re-matches re-seq`, and the regex-accepting
//! `clojure.string/{split,replace,replace-first}`. Follows
//! `stdlib_test.rs`'s `eval_ok`/`eval_err`/`ps` helper pattern.

use mova::internal::Interp;
use mova::internal::ErrorKind;
use mova::internal::Value;

fn eval_ok(src: &str) -> Value {
    let mut interp = Interp::new();
    interp
        .eval_str("test", src)
        .unwrap_or_else(|e| panic!("eval error for {src:?}: {}", mova::internal::render(&e, "test", src)))
}

fn eval_err_kind(src: &str) -> ErrorKind {
    let mut interp = Interp::new();
    match interp.eval_str("test", src) {
        Ok(v) => panic!("expected an error evaluating {src:?}, got {v:?}"),
        Err(e) => e.kind,
    }
}

fn ps(src: &str) -> String {
    mova::internal::pr_str(&eval_ok(src))
}

// -------------------- literal + type --------------------

#[test]
fn literal_reads_and_type_names_regex() {
    let v = eval_ok(r#"#"a.c""#);
    assert!(matches!(v, Value::Regex(_)));
    assert_eq!(v.type_name(), "regex");
}

#[test]
fn regex_predicate() {
    assert_eq!(ps(r#"(regex? #"a")"#), "true");
    assert_eq!(ps(r#"(regex? "a")"#), "false");
}

#[test]
fn str_is_bare_pattern_pr_str_is_hash_quoted() {
    assert_eq!(ps(r#"(str #"a.c")"#), "\"a.c\"");
    assert_eq!(ps(r#"(pr-str #"a.c")"#), "\"#\\\"a.c\\\"\"");
}

#[test]
fn backslashes_pass_through_raw() {
    // `#"\d"` is the two-char pattern `\d`, not a `d` escape.
    assert_eq!(ps(r#"(str #"\d+")"#), "\"\\\\d+\"");
}

#[test]
fn quote_inside_literal_does_not_terminate_it() {
    // `\"` inside `#"..."` yields a literal `"` in the pattern and keeps
    // reading; the pattern `a"b` matches the literal string `a"b`.
    assert_eq!(ps(r#"(re-find #"a\"b" "xa\"bx")"#), "\"a\\\"b\"");
}

#[test]
fn equality_and_hash_by_pattern_string() {
    assert_eq!(ps(r#"(= #"abc" #"abc")"#), "true");
    assert_eq!(ps(r#"(= #"abc" #"abd")"#), "false");
    assert_eq!(ps(r#"(= #"abc" (re-pattern "abc"))"#), "true");
    // Distinct-pattern regexes as map keys hash/compare independently.
    assert_eq!(ps(r#"(get {#"a" 1 #"b" 2} #"a")"#), "1");
}

#[test]
fn reader_error_on_bad_pattern() {
    assert_eq!(eval_err_kind(r#"#"a(b""#), ErrorKind::Reader);
}

#[test]
fn reader_error_on_unclosed_literal() {
    assert_eq!(eval_err_kind(r#"#"abc"#), ErrorKind::Reader);
}

// -------------------- re-pattern --------------------

#[test]
fn re_pattern_from_string_and_passthrough() {
    assert!(matches!(eval_ok(r#"(re-pattern "a+")"#), Value::Regex(_)));
    assert_eq!(ps(r#"(= (re-pattern #"a+") #"a+")"#), "true");
}

// -------------------- re-find --------------------

#[test]
fn re_find_no_groups() {
    assert_eq!(ps(r#"(re-find #"\d+" "abc123def")"#), "\"123\"");
    assert_eq!(ps(r#"(re-find #"\d+" "abcdef")"#), "nil");
}

#[test]
fn re_find_with_groups_and_nil_for_nonparticipating() {
    assert_eq!(ps(r#"(re-find #"(\d+)([.)])" "12) x")"#), "[\"12)\" \"12\" \")\"]");
    // second alternative group doesn't participate -> nil, not "".
    assert_eq!(ps(r#"(re-find #"(a)|(b)" "b")"#), "[\"b\" nil \"b\"]");
}

// -------------------- re-matches --------------------

#[test]
fn re_matches_requires_full_string_span() {
    assert_eq!(ps(r#"(re-matches #"\d+" "123")"#), "\"123\"");
    assert_eq!(ps(r#"(re-matches #"\d+" "123abc")"#), "nil");
    assert_eq!(ps(r#"(re-matches #"\d+" "abc123")"#), "nil");
}

#[test]
fn re_matches_with_groups() {
    assert_eq!(ps(r#"(re-matches #"(\d+)([.)])" "12)")"#), "[\"12)\" \"12\" \")\"]");
}

// -------------------- re-seq --------------------

#[test]
fn re_seq_successive_matches() {
    assert_eq!(ps(r#"(re-seq #"\d+" "a1 b22 c333")"#), "(\"1\" \"22\" \"333\")");
}

#[test]
fn re_seq_empty_when_no_match() {
    assert_eq!(ps(r#"(re-seq #"\d+" "abc")"#), "()");
}

#[test]
fn re_seq_with_groups() {
    assert_eq!(
        ps(r#"(re-seq #"(\d+)([.)])" "1. 2) 3.")"#),
        "([\"1.\" \"1\" \".\"] [\"2)\" \"2\" \")\"] [\"3.\" \"3\" \".\"])"
    );
}

// -------------------- clojure.string/split --------------------

#[test]
fn split_with_regex_collapses_whitespace_runs() {
    assert_eq!(ps(r#"(clojure.string/split "a b  c" #"\s+")"#), "[\"a\" \"b\" \"c\"]");
}

#[test]
fn split_with_string_pattern_still_literal() {
    assert_eq!(ps(r#"(clojure.string/split "a.b.c" ".")"#), "[\"a\" \"b\" \"c\"]");
}

#[test]
fn split_with_regex_drops_trailing_empties_only() {
    assert_eq!(ps(r#"(clojure.string/split "a,b,,," #",")"#), "[\"a\" \"b\"]");
    // No match anywhere: whole string returned, not exploded per-char.
    assert_eq!(ps(r#"(clojure.string/split "" #",")"#), "[\"\"]");
}

// The 3-arity `limit`, R5: 0 = the 2-arity default above (drop trailing
// empties); positive n = at most n pieces, the last one unsplit; negative
// = every piece, trailing empties kept. Both the regex and literal-string
// patterns get the same three cases.
#[test]
fn split_limit_zero_matches_the_2_arity_default() {
    assert_eq!(ps(r#"(clojure.string/split "a,b,,," #"," 0)"#), "[\"a\" \"b\"]");
    assert_eq!(ps(r#"(clojure.string/split "a,b,,," "," 0)"#), "[\"a\" \"b\"]");
    assert_eq!(ps(r#"(clojure.string/split "" #"," 0)"#), "[\"\"]");
}

#[test]
fn split_limit_positive_caps_piece_count_leaving_the_rest_unsplit() {
    assert_eq!(ps(r#"(clojure.string/split "a,b,c,d" #"," 2)"#), "[\"a\" \"b,c,d\"]");
    assert_eq!(ps(r#"(clojure.string/split "a,b,c,d" "," 2)"#), "[\"a\" \"b,c,d\"]");
    assert_eq!(ps(r#"(clojure.string/split "a,b,,," #"," 3)"#), "[\"a\" \"b\" \",,\"]");
    // A limit bigger than the number of real pieces just yields them all.
    assert_eq!(ps(r#"(clojure.string/split "a,b" #"," 10)"#), "[\"a\" \"b\"]");
}

#[test]
fn split_limit_negative_keeps_every_trailing_empty() {
    // "a,b,,," has 4 commas -> 5 raw pieces; limit 0 (the default) drops
    // the 3 trailing empties down to `["a" "b"]` (pinned above), negative
    // keeps all 5.
    assert_eq!(
        ps(r#"(clojure.string/split "a,b,,," #"," -1)"#),
        "[\"a\" \"b\" \"\" \"\" \"\"]"
    );
    assert_eq!(
        ps(r#"(clojure.string/split "a,b,,," "," -1)"#),
        "[\"a\" \"b\" \"\" \"\" \"\"]"
    );
}

// -------------------- clojure.string/replace + replace-first --------------------

#[test]
fn replace_with_regex_group_reference() {
    assert_eq!(
        ps(r#"(clojure.string/replace "2026-08-14" #"(\d+)-(\d+)-(\d+)" "$3/$2/$1")"#),
        "\"14/08/2026\""
    );
}

#[test]
fn replace_with_regex_replaces_all_occurrences() {
    assert_eq!(ps(r##"(clojure.string/replace "a1b2c3" #"\d" "#")"##), "\"a#b#c#\"");
}

#[test]
fn replace_first_with_regex_replaces_only_first() {
    assert_eq!(ps(r##"(clojure.string/replace-first "a1b2c3" #"\d" "#")"##), "\"a#b2c3\"");
}

#[test]
fn replace_first_with_string_pattern_literal() {
    assert_eq!(ps(r#"(clojure.string/replace-first "aXbXc" "X" "-")"#), "\"a-bXc\"");
}

#[test]
fn replace_with_string_pattern_still_literal_and_replaces_all() {
    assert_eq!(ps(r#"(clojure.string/replace "aXbXc" "X" "-")"#), "\"a-b-c\"");
}

// -------------------- C4: \Q...\E literal quoting --------------------
//
// Every case here mirrors a row measured against the oracle
// (java.util.regex.Pattern via real Clojure 1.13.0-alpha6); see
// compat/regexq-oracle-transcript.txt for the raw transcript.

#[test]
fn quoting_unterminated_q_quotes_to_end_of_pattern() {
    assert_eq!(ps(r#"(re-find #"\Qa.b" "a.b")"#), "\"a.b\"");
    // "." is literal inside the quoted span -- an unrelated char doesn't match.
    assert_eq!(ps(r#"(re-find #"\Qa.b" "axb")"#), "nil");
}

#[test]
fn quoting_empty_span_contributes_nothing() {
    assert_eq!(ps(r#"(re-find #"\Q\Ea.b" "axb")"#), "\"axb\"");
    assert_eq!(ps(r#"(re-find #"x\Q\Ey" "xy")"#), "\"xy\"");
}

#[test]
fn quoting_bare_e_without_q_is_a_reader_error() {
    // Java: PatternSyntaxException "Illegal/unsupported escape sequence"
    // at *compile* time. We don't special-case a bare `\E`; it passes
    // through untouched and the `regex` crate rejects it on its own
    // terms, matching the error-vs-success shape (both error) even
    // though the message differs.
    assert_eq!(eval_err_kind(r#"#"\E""#), ErrorKind::Reader);
    assert_eq!(eval_err_kind(r#"#"a\Eb""#), ErrorKind::Reader);
}

#[test]
fn quoting_bare_e_without_q_is_a_type_error_via_re_pattern() {
    // Same shape through the runtime (non-literal) compile path. `"\\E"`
    // (a plain Clojure string, escaped backslash + `E`) is the 2-char
    // string `\E`, mirroring the reader-literal case above.
    assert_eq!(eval_err_kind(r#"(re-pattern "\\E")"#), ErrorKind::TypeErr);
}

#[test]
fn quoting_content_is_raw_chars_not_escape_processed() {
    // Content inside \Q...\E is literal data -- `\n` here is two literal
    // chars (backslash, n), not a newline.
    assert_eq!(
        ps(r##"(re-find #"\Qa\nb\E" (str "a" (char 92) "nb"))"##),
        "\"a\\\\nb\""
    );
}

#[test]
fn quoting_no_nesting_second_q_is_literal_inside_open_span() {
    assert_eq!(
        ps(r##"(re-find #"\Qa\Qb\E" (str "a" (char 92) "Qb"))"##),
        "\"a\\\\Qb\""
    );
}

#[test]
fn quoting_inside_character_class() {
    assert_eq!(ps(r#"(re-find #"[\Q]a\E]" "]")"#), "\"]\"");
    assert_eq!(ps(r#"(re-find #"[\Q]a\E]" "a")"#), "\"a\"");
    assert_eq!(ps(r#"(re-find #"[\Q]a\E]" "x")"#), "nil");
}

#[test]
fn quoting_e_terminator_found_char_by_char_not_pairwise() {
    // Content `a`, `\`, `\`, `E`, `b`: the terminator is the *second*
    // backslash + `E` (the first backslash is span data), quoting `a\`
    // then matching literal `b` outside the span -> combined literal `a\b`.
    assert_eq!(
        ps(r##"(re-find #"\Qa\\Eb" (str "a" (char 92) "b"))"##),
        "\"a\\\\b\""
    );
}

#[test]
fn quoting_case_sensitive_and_dot_is_literal() {
    assert_eq!(ps(r#"(re-find #"a\Qb.c\Ed" "ab.cd")"#), "\"ab.cd\"");
    assert_eq!(ps(r#"(re-find #"a\Qb.c\Ed" "abxcd")"#), "nil");
    assert_eq!(ps(r#"(re-find #"\QABC\E" "abc")"#), "nil");
}

#[test]
fn quoting_multiple_spans_in_one_pattern() {
    assert_eq!(ps(r#"(re-find #"\Qa.\E-\Qb.\E" "a.-b.")"#), "\"a.-b.\"");
    assert_eq!(ps(r#"(re-find #"\Qa.\E-\Qb.\E" "axb.")"#), "nil");
    assert_eq!(ps(r#"(re-find #"a\Q\E\Q\Eb" "ab")"#), "\"ab\"");
}

#[test]
fn quoting_metacharacters_become_literal() {
    assert_eq!(
        ps(r##"(re-find #"\Q*+()[]{}^$|\E" "*+()[]{}^$|")"##),
        "\"*+()[]{}^$|\""
    );
}

#[test]
fn quoting_q_e_alone_matches_zero_width_empty_string() {
    assert_eq!(ps(r#"(re-find #"\Q\E" "")"#), "\"\"");
    assert_eq!(ps(r#"(re-find #"\Q\E" "abc")"#), "\"\"");
}

#[test]
fn quoting_unterminated_with_metacharacters() {
    assert_eq!(ps(r#"(re-find #"\Q.*+" ".*+")"#), "\".*+\"");
    assert_eq!(ps(r#"(re-find #"\Q.*+" "xxx")"#), "nil");
}

#[test]
fn quoting_unterminated_doubles_raw_backslashes_before_escaping() {
    // Reader stores `\\` (source) as two literal backslash chars; an
    // unterminated span quotes both, so it needs two backslashes on the
    // target side to match -- one lone backslash falls short.
    assert_eq!(
        ps(r##"(re-find #"\Qab\\" (str "ab" (char 92)))"##),
        "nil"
    );
}

#[test]
fn quoting_vendor_errors_corpus_pattern() {
    // The exact literal that unblocked tests/clojure-suite/vendor/errors.clj.
    assert_eq!(
        ps(r#"(re-find #"\Q/f2:+><->!#%&*|b\E" "/f2:+><->!#%&*|b")"#),
        "\"/f2:+><->!#%&*|b\""
    );
}

#[test]
fn quoting_works_via_re_pattern_runtime_path_too() {
    assert_eq!(
        ps(r##"(re-find (re-pattern "\\Qa.b\\E") "a.b")"##),
        "\"a.b\""
    );
    assert_eq!(
        ps(r##"(re-find (re-pattern "\\Qa.b\\E") "axb")"##),
        "nil"
    );
}
