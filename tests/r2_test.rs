//! Integration tests for R2: metadata reader syntax, tagged-literal
//! pass-through, `#'x`/`(var x)` + invocable vars, class-tolerant `catch`,
//! and `read-string`/`eval`. Follows `regex_test.rs`'s `eval_ok`/`eval_err_*`/
//! `ps` helper pattern. Compile-tier parity for these shapes lives in
//! `differential_test.rs` (this file mostly runs the default `Interp::new()`,
//! i.e. compiled tier); the flow/var wiring test lives in `flow_test.rs`
//! alongside the rest of that engine's tests.

use mova::internal::ErrorKind;
use mova::internal::Interp;
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

// ---------------------------------------------------------------------------
// 1. Metadata reader syntax
//
// S5/M3 CHANGED THIS SECTION. R2 parsed `^`-metadata and threw it away;
// M3 attaches it. The tests below kept their coverage (every shorthand,
// stacking, and the `def`/`defn`/`defonce` spellings that carry
// `^:private`) and gained the assertion R2 could not make: that the
// metadata actually ARRIVES somewhere. Every expectation here was
// measured against real Clojure 1.13.0-alpha6.
// ---------------------------------------------------------------------------

#[test]
fn private_metadata_on_def_lands_on_the_var() {
    assert_eq!(ps("(def ^:private x 1) x"), "1");
    // ... and, unlike R2, is now readable back off the var.
    assert_eq!(ps("(def ^:private x 1) (:private (meta (var x)))"), "true");
}

#[test]
fn private_metadata_on_defn_lands_on_the_var_not_the_fn() {
    assert_eq!(ps("(defn ^:private f [x] x) (f 5)"), "5");
    assert_eq!(ps("(defn ^:private f [x] x) (:private (meta (var f)))"), "true");
    // Measured: `(do (defn ^:foo g [] 1) (meta g))` is `nil` -- the
    // metadata goes on the VAR, never on the fn value it holds.
    assert_eq!(ps("(defn ^:foo g [] 1) (meta g)"), "nil");
}

#[test]
fn private_metadata_on_defonce_is_accepted_and_defonce_still_works() {
    assert_eq!(ps("(defonce ^:private a* (atom nil)) (deref a*)"), "nil");
    assert_eq!(ps("(defonce ^:private a* (atom nil)) (reset! a* 1) (defonce ^:private a* (atom 99)) (deref a*)"), "1");
}

/// The four shorthands from `reader::Reader::read_meta`'s desugaring
/// table. `^Tag`/`^"tag"` both mean `:tag`; `^:kw` means `{:kw true}`.
#[test]
fn every_metadata_shape_desugars_to_its_measured_map() {
    assert_eq!(ps("(meta ^:kw [1])"), "{:kw true}");
    assert_eq!(ps("(meta ^{:a 1} [2])"), "{:a 1}");
    assert_eq!(ps("(meta ^sym [3])"), "{:tag sym}");
    assert_eq!(ps("(meta ^\"str\" [4])"), "{:tag \"str\"}");
}

/// Metadata on a non-`IObj` literal is a READ-time error, matching
/// Clojure's own "Metadata can only be applied to IMetas" (measured:
/// `(read-string "^:a 1")` throws `IllegalArgumentException`). Before M3
/// these evaluated to the bare literal, because the metadata was
/// discarded.
#[test]
fn metadata_on_a_non_iobj_literal_is_a_reader_error() {
    assert_eq!(eval_err_kind("^:kw 1"), ErrorKind::Reader);
    assert_eq!(eval_err_kind("^{:a 1} \"s\""), ErrorKind::Reader);
    assert_eq!(eval_err_kind("^:a ^:b 42"), ErrorKind::Reader);
}

#[test]
fn stacked_metadata_merges_with_the_outer_write_winning() {
    // Both keys survive (measured `{:b true, :a true}`; mova's map print
    // order differs from Clojure's array-map insertion order across the
    // board -- see `(pr-str {:b 1 :a 2})` -- so this asserts the SET of
    // keys, which is the part metadata semantics actually fixes).
    assert_eq!(ps("(sort (keys (meta ^:a ^:b [42])))"), "(:a :b)");
    // On a conflict the OUTER (leftmost) write wins: measured `{:a 1}`.
    assert_eq!(ps("(meta ^{:a 1} ^{:a 2} [42])"), "{:a 1}");
    assert_eq!(ps("(def ^:private ^:extra y 7) y"), "7");
    assert_eq!(ps("(def ^:private ^:extra y 7) (sort (keys (select-keys (meta (var y)) [:private :extra])))"),
               "(:extra :private)");
}

/// `read-string` does NOT resolve a `:tag` symbol (measured: `(meta
/// (read-string "^String x"))` is `{:tag String}`), and neither does
/// mova's evaluator -- see `Interp::eval_meta_form`'s doc for why type
/// hints stay inert here rather than resolving to a JVM class.
#[test]
fn tag_metadata_stays_an_unresolved_symbol() {
    assert_eq!(ps("(meta (read-string \"^String x\"))"), "{:tag String}");
    assert_eq!(ps("(meta ^String [1])"), "{:tag String}");
    // A hinted parameter still BINDS -- the regression the `Form::meta`
    // field design exists to prevent.
    assert_eq!(ps("((fn [^long n] (+ n 1)) 41)"), "42");
    assert_eq!(ps("(defn h [^String s ^long n] [s n]) (h \"a\" 1)"), "[\"a\" 1]");
}

/// Non-symbol metadata VALUES evaluate normally (measured: `(meta ^{:a
/// (+ 1 2)} [1])` is `{:a 3}`), unlike `quote`/`read-string`, which
/// leave the whole map unevaluated.
#[test]
fn metadata_values_evaluate_except_bare_symbols() {
    assert_eq!(ps("(meta ^{:a (+ 1 2)} [1])"), "{:a 3}");
    assert_eq!(ps("(meta (quote ^:a x))"), "{:a true}");
}

// ---------------------------------------------------------------------------
// 2. Tagged-literal pass-through
// ---------------------------------------------------------------------------

#[test]
fn cpp_tagged_literal_reads_as_its_payload() {
    assert_eq!(ps("#cpp 300"), "300");
}

#[test]
fn other_tags_also_pass_through_unchanged() {
    // SPEC-W1 task 4: `#inst` left the pass-through set (like `#uuid`
    // before it) -- it reads as a `java.util.Date` value and prints in
    // reader syntax. Every OTHER tag still passes its payload through.
    assert_eq!(ps("#inst \"2020-01-01\""), "#inst \"2020-01-01T00:00:00.000-00:00\"");
    assert_eq!(ps("#my.ns/tag [1 2 3]"), "[1 2 3]");
}

#[test]
fn genuinely_unsupported_dispatch_chars_still_error() {
    assert_eq!(eval_err_kind("#)"), ErrorKind::Reader);
}

// ---------------------------------------------------------------------------
// 3. Var quote + invocable vars
// ---------------------------------------------------------------------------

#[test]
fn var_quote_reader_desugars_to_var_special_form() {
    assert_eq!(ps("(def x 1) (= (var x) #'x)"), "true");
}

#[test]
fn var_prints_readably_as_hash_quote_name() {
    assert_eq!(ps("(def x 1) (pr-str #'x)"), "\"#'user/x\"");
}

#[test]
fn var_is_invocable_directly() {
    assert_eq!(ps("(def f (fn [x] (* x 2))) (#'f 21)"), "42");
}

#[test]
fn var_is_invocable_through_higher_order_fns() {
    assert_eq!(ps("(def f (fn [x] (* x 2))) (vec (map #'f [1 2 3]))"), "[2 4 6]");
}

#[test]
fn var_before_def_late_binds_to_the_cell_def_later_writes() {
    // `#'later` is taken BEFORE `later` is defined; the returned Var must
    // be the SAME cell `def` goes on to fill (R2 mission: "the cell def
    // will later write").
    assert_eq!(
        ps("(def vref (var later)) (def later (fn [] 99)) (vref)"),
        "99"
    );
}

#[test]
fn deref_on_a_var_reads_its_current_value() {
    assert_eq!(ps("(def x 5) (deref (var x))"), "5");
    assert_eq!(ps("(def x 5) @(var x)"), "5");
}

#[test]
fn deref_on_a_var_sees_redefinition() {
    assert_eq!(ps("(def x 1) (def vr (var x)) (def x 2) @vr"), "2");
}

#[test]
fn two_resolutions_of_the_same_var_are_equal() {
    assert_eq!(ps("(def x 1) (= #'x #'x)"), "true");
}

#[test]
fn compiled_fn_bailing_on_var_still_evaluates_correctly() {
    // `(var x)` inside a fn body bails the WHOLE fn out of the compiled
    // tier (`compile::resolve`'s bail list); this pins that the bail
    // doesn't silently produce a wrong answer, only a slower one.
    assert_eq!(
        ps("(def x 10) (defn g [] (deref (var x))) (g)"),
        "10"
    );
}

// ---------------------------------------------------------------------------
// 4. Class-tolerant catch
// ---------------------------------------------------------------------------

#[test]
fn untyped_catch_still_works() {
    assert_eq!(ps("(try (throw :boom) (catch e e))"), ":boom");
}

#[test]
fn dotted_class_token_catch_binds_the_second_symbol() {
    // C3g: a dotted class token is still PARSED as (class, binding) --
    // `parse_catch_head`'s heuristic is unchanged -- but the class is now
    // genuinely MATCHED at catch time (`catch_class_matches`), not
    // ignored. `jank.runtime.object_ref` names nothing in this catch's
    // real ancestor chain (an `ex-info` throw is `clojure.lang.
    // ExceptionInfo <: RuntimeException <: Exception <: Throwable`), so
    // this clause correctly does NOT fire and the throw propagates past
    // the whole `try` uncaught -- this is the NEW, correct behavior,
    // superseding the pre-C3g version of this test (which asserted the
    // class-blind catch fired regardless of the token).
    let err = eval_err_kind(
        r#"(try (throw (ex-info "boom" {})) (catch jank.runtime.object_ref e (ex-message e)))"#,
    );
    assert_eq!(err, ErrorKind::Thrown);
}

#[test]
fn dotted_class_token_that_matches_binds_the_second_symbol() {
    // The parsing half of the fixed-above test still needs a positive
    // case: a dotted token that DOES genuinely match binds the second
    // symbol and its body runs against the caught value.
    assert_eq!(
        ps(r#"(try (throw (ex-info "boom" {})) (catch clojure.lang.ExceptionInfo e (ex-message e)))"#),
        "\"boom\""
    );
}

#[test]
fn capitalized_class_token_catch_binds_the_second_symbol() {
    assert_eq!(
        ps(r#"(try (throw (ex-info "boom" {})) (catch Exception e (ex-message e)))"#),
        "\"boom\""
    );
}

#[test]
fn ambiguous_two_symbol_head_favors_the_class_reading() {
    // `(catch Exception e)`: `Exception` is uppercase, so this reads as
    // "class `Exception`, binding `e`, empty body" (-> nil), NOT "binding
    // `Exception`, body `(e)`" -- documented tie-break in the R2 mission
    // notes. C3g: the class must now also genuinely MATCH what's thrown
    // for this clause to run at all, so the thrown value is an `ex-info`
    // map (real class `Exception` matches) rather than the pre-C3g
    // fake-class-name-over-a-bare-int shape, which no longer reaches this
    // clause under class-aware `catch` (see
    // `class_tolerant_catch_does_not_match_an_unrelated_class` below).
    assert_eq!(
        ps(r#"(try (throw (ex-info "boom" {})) (catch Exception e))"#),
        "nil"
    );
}

#[test]
fn class_tolerant_catch_binding_actually_carries_the_thrown_value() {
    // W4D-TIERS: expected string was stale legacy shape
    // (`{:ex/data ..., :ex/message ...}`), predating the session-8
    // ExceptionInfo taxonomy work -- same fix as
    // `differential_test.rs`'s `r2_class_tolerant_catch_agrees_between_tiers`.
    assert_eq!(
        ps(r#"(try (throw (ex-info "boom" {})) (catch Exception e e))"#),
        "#error {\n :cause \"boom\"\n :data {}\n :via\n [{:type clojure.lang.ExceptionInfo\n   :message \"boom\"\n   :data {}}]\n :trace\n []}"
    );
}

#[test]
fn class_tolerant_catch_does_not_match_an_unrelated_class() {
    // C3g NEW: a fake/unregistered class name (the pre-C3g `E` this test
    // family used to use for both halves of the R2 parse-ambiguity check)
    // no longer catches anything -- including a bare, non-`Throwable`
    // thrown value like a plain integer, which has NO class ancestry at
    // all (real Clojure requires `throw`'s argument to already be a
    // `Throwable`; see `eval::special_forms::thrown_value_class_chain`).
    let err = eval_err_kind("(try (throw 1) (catch E e))");
    assert_eq!(err, ErrorKind::Thrown);
}

#[test]
fn class_tolerant_catch_survives_no_compile_tier_too() {
    let mut interp = Interp::with_compile_enabled(false);
    let v = interp
        .eval_str("test", r#"(try (throw (ex-info "x" {})) (catch Exception e (ex-message e)))"#)
        .unwrap_or_else(|e| panic!("eval error: {e:?}"));
    assert_eq!(mova::internal::pr_str(&v), "\"x\"");
}

// ---------------------------------------------------------------------------
// 5. read-string + eval
// ---------------------------------------------------------------------------

#[test]
fn eval_of_read_string_runs_arithmetic() {
    assert_eq!(ps(r#"(eval (read-string "(+ 1 2)"))"#), "3");
}

#[test]
fn eval_errors_are_catchable_not_a_crash() {
    assert_eq!(
        ps(r#"(try (eval (read-string "(no-such-fn)")) (catch e :caught))"#),
        ":caught"
    );
}

#[test]
fn read_string_reader_errors_are_catchable_not_a_crash() {
    assert_eq!(
        ps(r#"(try (read-string "(1 2") (catch e :caught))"#),
        ":caught"
    );
}

#[test]
fn read_string_of_tagged_literal_reads_the_payload() {
    assert_eq!(ps(r##"(read-string "#cpp 300")"##), "300");
}

#[test]
fn eval_of_def_interns_into_the_current_namespace() {
    assert_eq!(ps("(eval (read-string \"(def q1 9)\")) q1"), "9");
}

#[test]
fn read_string_reads_only_the_first_form() {
    assert_eq!(ps(r#"(read-string "1 2 3")"#), "1");
}
