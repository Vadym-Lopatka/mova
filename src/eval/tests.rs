use crate::pvec;
use crate::value::PVec;
use super::*;
use crate::error::ErrorKind;
use crate::printer::pr_str;
use crate::reader::read_all;

fn eval(src: &str) -> Value {
    let mut interp = Interp::new();
    interp
        .eval_str("test", src)
        .unwrap_or_else(|e| panic!("eval error for {src:?}: {}", crate::error::render(&e, "test", src)))
}

fn eval_err(src: &str) -> RjError {
    let mut interp = Interp::new();
    interp
        .eval_str("test", src)
        .expect_err(&format!("expected an error evaluating {src:?}"))
}

fn ps(src: &str) -> String {
    pr_str(&eval(src))
}

#[test]
fn new_interp_boots_without_panicking() {
    let _interp = Interp::new();
}

/// W3e-1 gate: `special_forms::SPECIAL_FORM_NAMES` is a hand-written
/// mirror of `eval_special`'s `match` arms, and `quasiquote`'s symbol
/// resolution reads it as "mova's stand-in for real Clojure's
/// `clojure.core` macro mappings". A name that drifts out of sync would
/// silently make `` `that-name `` resolve to `<current-ns>/that-name`
/// instead of `clojure.core/that-name` -- i.e. every macro that
/// syntax-quotes it would break in exactly the way this whole change
/// exists to fix, with no test noticing. So: feed every listed name back
/// through the real dispatcher and require it to CLAIM the call (return
/// `Some`, whatever the result). The arguments are deliberately empty --
/// every one of these returns an arity/shape error rather than falling
/// through, and `Some(Err(..))` is exactly the "claimed it" answer this
/// gate is asking for.
///
/// W3e2: and the CONVERSE, which is the direction that actually bites --
/// a head added to `eval_special` and forgotten in the list is the silent
/// failure, not a stale extra entry. There is no way to enumerate a Rust
/// `match`'s arms at runtime, so this scans `special_forms.rs`'s own
/// source (`include_str!`, so it is the file that was compiled, not
/// whatever is on disk later) for the `"name" =>` arms inside
/// `eval_special` and requires each one to be listed. Merging main proved
/// the need: it added `reify` (D1) and `.` (D5) while the list sat still.
#[test]
fn special_form_names_match_dispatch() {
    let mut interp = Interp::new();
    let env = interp.globals.clone();
    for name in crate::eval::special_forms::SPECIAL_FORM_NAMES {
        let claimed = interp.eval_special(name, &[], crate::reader::Span { start: 0, end: 0 }, &env).is_some();
        assert!(
            claimed,
            "SPECIAL_FORM_NAMES lists {name:?} but Interp::eval_special does not dispatch it -- \
             the list has drifted out of sync with the match arms"
        );
    }
    for name in eval_special_head_names_from_source() {
        assert!(
            crate::eval::special_forms::is_special_form_name(&name),
            "Interp::eval_special dispatches {name:?} but SPECIAL_FORM_NAMES does not list it. \
             Add it: without the entry, `` `{name} `` reads as `<current-ns>/{name}` instead of \
             `clojure.core/{name}` and every macro that syntax-quotes it breaks silently \
             (src/eval/quasiquote.rs's `syntax_quote_resolve`)."
        );
    }
}

/// The literal head names of `Interp::eval_special`'s fixed `match` arms,
/// scraped from its own source. Deliberately dumb (a line scanner, not a
/// Rust parser), in the same spirit as this repo's other
/// deliberately-auditable text tools: it reads only the lines between the
/// `fn eval_special` signature and the `_ => None` that closes the match,
/// and only those shaped `"a" | "b" => ...`. The two `other if ...` guard
/// arms carry no literal and are skipped for free, which is correct --
/// they are pattern-shaped (`(.field x)` / `(Ctor. args)`), not names.
fn eval_special_head_names_from_source() -> Vec<String> {
    let src = include_str!("special_forms.rs");
    let body = src
        .split_once("pub(super) fn eval_special(")
        .expect("eval_special's signature is the anchor this scan starts from")
        .1;
    let body = body
        .split_once("_ => None,")
        .expect("eval_special's match ends with a `_ => None,` arm")
        .0;
    let mut out = Vec::new();
    for line in body.lines() {
        let line = line.trim();
        let Some((heads, _)) = line.split_once("=>") else { continue };
        if !heads.trim_start().starts_with('"') {
            continue;
        }
        for head in heads.split('|') {
            let head = head.trim();
            if let Some(name) = head.strip_prefix('"').and_then(|h| h.strip_suffix('"')) {
                out.push(name.to_string());
            }
        }
    }
    assert!(
        out.len() > 25,
        "the eval_special source scan found only {} head names -- it has stopped matching the \
         source's shape and is silently gating nothing",
        out.len()
    );
    out
}

// -------------------- def / basic application --------------------

#[test]
fn def_returns_and_binds_the_value() {
    assert_eq!(ps("(def x 42) x"), "42");
}

/// W-DECL: renamed from `def_without_value_binds_nil` -- that name
/// described the OLD (wrong) behavior. Real Clojure's `(def name)` with
/// no init form interns the var and leaves it genuinely UNBOUND
/// (`Var$Unbound`, not `nil` -- measured, `compat/w-decl-oracle1.txt`'s
/// "declare-then-deref" section: `(declare a) (str a)` is `"Unbound:
/// #'user/a"`, no exception, and `(class (var-get #'a))` is
/// `clojure.lang.Var$Unbound`). mova has no `Var$Unbound` sentinel value
/// (a pre-existing, disclosed gap -- `src/builtins/atoms.rs:346`'s own
/// comment, and nothing in the vendored corpus exercises a bare read of
/// an unbound var: `test.clj`'s own `can-test-unbound-symbol` deftest is
/// `#_`-commented out in the vendored source itself), so this pins
/// mova's own strictly-more-restrictive stand-in instead: reading a
/// declared-but-never-`def`d name is the ordinary unresolved-symbol
/// error, not a silent `nil` the way the pre-fix `eval_def` (which
/// unconditionally `set` a bare `(def x)` to `Value::Nil`) produced.
/// This is exactly the bug that forced `declare` to route around `def`
/// entirely via the old `--intern-unbound!` native -- see core/core.mova's
/// `declare` doc comment.
#[test]
fn def_without_value_leaves_the_var_genuinely_unbound() {
    let err = eval_err("(def x) x");
    assert_eq!(err.kind, ErrorKind::Unresolved);
}

/// W-DECL: the OTHER half of real `(def name)` semantics -- on an
/// ALREADY-bound name it touches nothing at all (no re-`def`-to-`nil`),
/// matching real Clojure's `Compiler.DefExpr` (`Var.bindRoot` is only
/// ever called when the source form actually wrote an init expression).
#[test]
fn def_without_value_does_not_clobber_an_existing_binding() {
    assert_eq!(ps("(def x 42) (def x) x"), "42");
}

/// W-DECL: `declare` itself is now nothing but 1-arg `def` in a loop
/// (core/core.mova), so this is the same fact from the macro's own
/// front door -- `(declare y)` alone must not make `y` silently read as
/// `nil`.
#[test]
fn declare_alone_leaves_the_var_unbound_not_nil() {
    let err = eval_err("(declare zzz-decl-only) zzz-decl-only");
    assert_eq!(err.kind, ErrorKind::Unresolved);
}

/// W-DECL: `declare` followed by a real `def` binds normally -- the
/// forward-reference use case `declare` exists for in the first place.
#[test]
fn declare_then_def_then_use() {
    assert_eq!(ps("(declare y) (def y 42) y"), "42");
}

// -------------------- set! (SPEC-D) --------------------

#[test]
fn set_bang_returns_the_assigned_value() {
    assert_eq!(ps("(def ^:dynamic x 1) (binding [x x] (set! x 41))"), "41");
}

#[test]
fn set_bang_is_visible_to_a_later_read() {
    assert_eq!(ps("(def ^:dynamic x 1) (binding [x x] (set! x 41) x)"), "41");
}

#[test]
fn set_bang_on_a_compiler_knob_var_works() {
    // The exact case that motivates this slice: `vectors.clj`/`string.clj`
    // in the vendored conformance suite open with this line.
    assert_eq!(ps("(binding [*warn-on-reflection* false] (set! *warn-on-reflection* true) *warn-on-reflection*)"), "true");
}

#[test]
fn set_bang_on_undefined_symbol_is_the_ordinary_unresolved_error() {
    let err = eval_err("(set! nonexistent-thing 1)");
    assert_eq!(err.kind, ErrorKind::Unresolved);
    assert!(err.message.contains("nonexistent-thing"), "message: {}", err.message);
}

#[test]
fn set_bang_wrong_arity_errors() {
    let err = eval_err("(def x 1) (set! x)");
    assert_eq!(err.kind, ErrorKind::Arity);
    let err2 = eval_err("(def x 1) (set! x 1 2)");
    assert_eq!(err2.kind, ErrorKind::Arity);
}

#[test]
fn native_application_arithmetic() {
    assert_eq!(ps("(+ 1 2 3)"), "6");
    assert_eq!(ps("(* 2 3 4)"), "24");
    assert_eq!(ps("(- 10 1 2)"), "7");
    assert_eq!(ps("(- 5)"), "-5");
    assert_eq!(ps("(/ 10 2)"), "5");
    // S5 (SPEC-numtower): a non-exact `Long`/`Long` division is a RATIO
    // now, not a Double (measured `(/ 1 3)` => `1/3`). This assertion is
    // the old no-ratio-type behavior, corrected.
    assert_eq!(ps("(/ 1 3)"), "1/3");
}

// -------------------- fn: anonymous, named, multi-arity, variadic --------------------

#[test]
fn anonymous_fn_applies() {
    assert_eq!(ps("((fn [x y] (+ x y)) 1 2)"), "3");
}

#[test]
fn named_fn_self_recurses_without_def() {
    // named `fn` binds its own name inside its body for recursion.
    let src = "((fn count-down [n] (if (= n 0) :done (count-down (- n 1)))) 5)";
    assert_eq!(ps(src), ":done");
}

#[test]
fn multi_arity_fn_dispatches_by_argc() {
    let src = "(def f (fn ([x] x) ([x y] (+ x y)))) [(f 1) (f 1 2)]";
    assert_eq!(ps(src), "[1 3]");
}

#[test]
fn variadic_fn_binds_rest_as_list_or_nil() {
    let src = "(def f (fn [a & more] more)) [(f 1) (f 1 2 3)]";
    assert_eq!(ps(src), "[nil (2 3)]");
}

#[test]
fn closures_capture_lexical_env() {
    let src = "(def make-adder (fn [x] (fn [y] (+ x y)))) (def add5 (make-adder 5)) (add5 10)";
    assert_eq!(ps(src), "15");
}

#[test]
fn arity_error_message_names_fn_and_counts() {
    // C3c: message shape changed to match real `clojure.lang.
    // ArityException.getMessage()` exactly ("Wrong number of args
    // (<actual>) passed to: <ns>/<name>", measured against `.oracle` --
    // see `arity_error_message`'s own doc) -- it no longer states the
    // EXPECTED arity count at all (neither does the JVM), so this only
    // checks the actual count and the fn name, not a stale "expects N".
    let err = eval_err("(def f (fn [x y] x)) (f 1)");
    assert_eq!(err.kind, ErrorKind::Arity);
    assert!(err.message.contains("Wrong number of args (1) passed to"), "message: {}", err.message);
    assert!(err.message.contains('f'), "message: {}", err.message);
    assert_eq!(err.arity_actual, Some(1));
}

// -------------------- let --------------------

#[test]
fn let_sequential_bindings_see_earlier_ones() {
    assert_eq!(ps("(let [a 1 b (+ a 1)] (+ a b))"), "3");
}

#[test]
fn let_shadows_outer_binding() {
    assert_eq!(ps("(def x 1) (let [x 2] x)"), "2");
}

#[test]
fn let_does_not_leak_bindings_outward() {
    assert_eq!(ps("(let [x 2] x) (def y (try x (catch e :undefined))) y"), ":undefined");
}

// -------------------- binding [*ns* ...] (C3f) --------------------
//
// Measured against the oracle: real Clojure's `in-ns` is literally `(set!
// *ns* (the-ns name))` (`clojure.core/in-ns`'s own source), so it composes
// with an enclosing `binding` of `*ns*` the same way any other `set!` does
// -- the switch is visible for the `binding` body's dynamic extent and
// gone once the body exits. Before this fix, `set_current_ns` (`in-ns`/
// `ns`) always wrote `*ns*`'s ROOT value regardless of an active `binding`
// frame, and `current_ns` (the field the tree-walker actually resolves
// symbols against) was never restored at all -- so `orig` below stayed
// permanently unresolvable in `tmp.zzz` once the `binding` exited.

// -------------------- field2/W-NS: lexical vs dynamic namespace --------------------
//
// The split documented on `ns::Interp::switch_ns` and
// `Interp::closure_depth`. Every expectation below was measured against a
// live Clojure 1.12.5 first -- `compat/w-ns-lexical-dynamic-probe.clj` +
// `compat/w-ns-lexical-dynamic-oracle-transcript.txt` carry the full
// differential run.

#[test]
fn a_mid_body_ns_switch_does_not_unresolve_the_rest_of_that_body() {
    // The conductor's exact repro, and `repl.clj`'s `test-dynamic-ns`
    // shape: the JVM compiles the whole fn body in the namespace it is
    // WRITTEN in, so the mid-body `(ns a)` cannot un-resolve `helper`.
    // Oracle: 42.
    assert_eq!(
        ps("(ns w-ns-test.a) (def helper 42) \
            (defmacro switch [] (list 'do (list 'ns 'w-ns-test.gen) 1)) \
            (defn f [] (switch) helper) (f)"),
        "42"
    );
}

#[test]
fn a_mid_body_ns_switch_still_moves_the_dynamic_ns_var() {
    // The other half: `*ns*` DOES move (real `ns`/`in-ns` are `set!` on
    // the var), and stays moved for the rest of the body.
    assert_eq!(
        ps("(ns w-ns-test.b) \
            (defn f [] (ns w-ns-test.gen2) (clojure.core/name (clojure.core/ns-name *ns*))) (f)"),
        "\"w-ns-test.gen2\""
    );
}

#[test]
fn a_top_level_ns_switch_still_moves_both() {
    // Depth 0 is unchanged: sequential load-order switching, exactly as
    // real `Compiler.load` reads and compiles one form at a time.
    assert_eq!(
        ps("(ns w-ns-test.c) (def only-in-c 1) (ns w-ns-test.c2) (resolve 'only-in-c)"),
        "nil"
    );
}

#[test]
fn a_closure_body_resolves_in_its_own_defining_ns_after_an_in_ns() {
    // `in-ns` inside a fn: `*ns*` moves, the body's own free symbols
    // (`d-marker`, defined in the DEFINING ns) keep resolving.
    assert_eq!(
        ps("(ns w-ns-test.d) (def d-marker 7) \
            (defn g [] (in-ns 'w-ns-test.d2) [(clojure.core/name (clojure.core/ns-name *ns*)) d-marker]) (g)"),
        "[\"w-ns-test.d2\" 7]"
    );
}

#[test]
fn binding_ns_does_not_move_lexical_resolution() {
    // Measured (probe row 6): real Clojure compiles the whole `binding`
    // form in the namespace it is written in, so `e-marker` still reads.
    // Before field2/W-NS this answered "Unable to resolve symbol".
    assert_eq!(
        ps("(ns w-ns-test.e) (def e-marker 5) (ns w-ns-test.e2) \
            (binding [*ns* (the-ns 'w-ns-test.e)] 1) \
            (ns w-ns-test.e) \
            [(binding [*ns* (the-ns 'w-ns-test.e2)] [(clojure.core/name (clojure.core/ns-name *ns*)) e-marker]) e-marker]"),
        "[[\"w-ns-test.e2\" 5] 5]"
    );
}

#[test]
fn declaring_a_namespace_marks_it_loaded_like_clojures_ns_macro() {
    // `clojure/core.clj`'s `ns` macro ends in `(commute *loaded-libs*
    // conj '<name>)`, so `require` of an `ns`-declared namespace is a
    // no-op instead of a file lookup. This is the second half of
    // `repl.clj`'s `test-dynamic-ns`.
    assert_eq!(
        ps("(ns w-ns-test.f) (defn g [] (ns w-ns-test.gen3) (require 'w-ns-test.gen3)) (g)"),
        "nil"
    );
}

#[test]
fn binding_ns_restores_current_ns_on_scope_exit() {
    assert_eq!(
        ps("(def orig 1) (binding [*ns* *ns*] (in-ns 'binding-ns-test.zzz)) orig"),
        "1"
    );
}

#[test]
fn binding_ns_makes_in_ns_visible_for_the_body_only() {
    // One vector literal, so both elements evaluate against the SAME
    // interpreter in sequence (`eval_form_in`'s `Vector` arm) without
    // needing a `def` (which would qualify into whichever namespace is
    // current at the time, not name a var reachable from both sides of
    // the `binding`): the first element reads `*ns*` from INSIDE the
    // `binding` body, the second reads it again once the body -- and the
    // `binding` -- have already exited.
    assert_eq!(
        ps(
            "[(binding [*ns* *ns*] (in-ns 'binding-ns-test.zzz2) (clojure.core/name (clojure.core/ns-name clojure.core/*ns*))) \
              (clojure.core/name (clojure.core/ns-name *ns*))]"
        ),
        "[\"binding-ns-test.zzz2\" \"user\"]"
    );
}

#[test]
fn binding_ns_lets_def_reach_the_switched_namespace() {
    assert_eq!(
        ps(
            "(binding [*ns* *ns*] (in-ns 'binding-ns-test.zzz4) (clojure.core/eval '(def x-in-zzz4 42))) \
             (nil? (resolve 'binding-ns-test.zzz4/x-in-zzz4))"
        ),
        "false"
    );
}

// A3: `let` now supports full destructuring targets (see
// eval::special_forms's `bind_pattern`); the exhaustive behavior is covered
// by tests/destructure_test.rs. This unit test just confirms the sequential
// case wires through end-to-end at this layer.
#[test]
fn let_destructuring_binds_sequential_pattern() {
    assert_eq!(ps("(let [[a b] [1 2]] (+ a b))"), "3");
}

#[test]
fn let_malformed_destructuring_target_errors_clearly() {
    let err = eval_err("(let [3 [1 2]] 3)");
    assert!(err.message.contains("invalid destructuring pattern"));
}

// -------------------- if / truthiness --------------------

#[test]
fn if_truthiness_nil_and_false_are_falsey_everything_else_truthy() {
    assert_eq!(ps("(if nil :t :f)"), ":f");
    assert_eq!(ps("(if false :t :f)"), ":f");
    assert_eq!(ps("(if 0 :t :f)"), ":t");
    assert_eq!(ps("(if \"\" :t :f)"), ":t");
    assert_eq!(ps("(if true :t :f)"), ":t");
}

#[test]
fn if_without_else_returns_nil_on_falsey_test() {
    assert_eq!(ps("(if false :t)"), "nil");
}

// -------------------- do --------------------

#[test]
fn do_evaluates_in_order_and_returns_last() {
    assert_eq!(ps("(do (def a 1) (def a 2) a)"), "2");
}

#[test]
fn empty_do_is_nil() {
    assert_eq!(ps("(do)"), "nil");
}

// -------------------- loop/recur --------------------

#[test]
fn loop_recur_countdown() {
    assert_eq!(ps("(loop [n 5 acc 0] (if (= n 0) acc (recur (- n 1) (+ acc n))))"), "15");
}

#[test]
fn loop_recur_100k_iterations_does_not_overflow_stack() {
    let src = "(loop [n 100000] (if (= n 0) :done (recur (- n 1))))";
    assert_eq!(ps(src), ":done");
}

#[test]
fn fn_self_recur_via_recur_100k_iterations() {
    let src = "(def f (fn [n] (if (= n 0) :done (recur (- n 1))))) (f 100000)";
    assert_eq!(ps(src), ":done");
}

#[test]
fn recur_outside_loop_or_fn_is_a_clear_diagnostic() {
    let err = eval_err("(recur 1)");
    assert!(
        err.message.contains("recur used outside loop/fn tail position"),
        "message: {}",
        err.message
    );
}

#[test]
fn recur_arity_mismatch_in_loop_errors() {
    let err = eval_err("(loop [n 5] (recur 1 2))");
    assert_eq!(err.kind, ErrorKind::Arity);
}

// -------------------- quote / quasiquote --------------------

#[test]
fn quote_returns_unevaluated_form() {
    assert_eq!(ps("(quote (+ 1 2))"), "(+ 1 2)");
    assert_eq!(ps("'(+ 1 2)"), "(+ 1 2)");
}

// W3e-1: the `user/` prefixes below are not noise -- they are the whole
// point of syntax-quote. Every row here was re-measured on real Clojure
// 1.13.0-alpha6 in the default `user` namespace after the qualification
// change landed (`compat/sq-qualify-probe.clj`); an UNQUALIFIED `a`/`b`/`c`
// is what mova used to produce and what real Clojure never produces.

#[test]
fn quasiquote_with_unquote_in_list() {
    assert_eq!(ps("(let [x 5] `(a ~x c))"), "(user/a 5 user/c)");
}

#[test]
fn quasiquote_with_splicing_in_list() {
    assert_eq!(ps("(let [xs (list 1 2 3)] `(a ~@xs b))"), "(user/a 1 2 3 user/b)");
}

#[test]
fn quasiquote_with_unquote_in_vector() {
    assert_eq!(ps("(let [x 5] `[a ~x c])"), "[user/a 5 user/c]");
}

#[test]
fn quasiquote_with_splicing_in_vector() {
    assert_eq!(ps("(let [xs [1 2 3]] `[a ~@xs b])"), "[user/a 1 2 3 user/b]");
}

// -------------------- W3e-1: syntax-quote ns qualification ----------------
//
// Corpus + transcript: `compat/sq-qualify-probe.clj` /
// `compat/sq-qualify-oracle-transcript.txt` (real Clojure 1.13.0-alpha6,
// namespace `probe`). mova reproduces every row of that transcript except
// the auto-gensym COUNTER values, which are per-runtime by construction.

#[test]
fn syntax_quote_qualifies_a_core_var_to_clojure_core() {
    assert_eq!(ps("`map"), "clojure.core/map");
    assert_eq!(ps("(= `map 'clojure.core/map)"), "true");
    // A `core.mova`-bootstrapped macro reads the same way.
    assert_eq!(ps("`->"), "clojure.core/->");
}

#[test]
fn syntax_quote_qualifies_an_unmapped_name_to_the_current_ns() {
    assert_eq!(ps("`unmapped-thing"), "user/unmapped-thing");
    assert_eq!(ps("(do (ns probe) `unmapped-thing)"), "probe/unmapped-thing");
}

#[test]
fn syntax_quote_leaves_special_forms_bare() {
    // Clojure's `Compiler.isSpecial` set, quoted verbatim by the reader --
    // `&` above all, or `` `(fn [x & y] ...) `` would emit a qualified `&`
    // and stop being a rest-arg marker.
    assert_eq!(ps("`if"), "if");
    assert_eq!(ps("`do"), "do");
    assert_eq!(ps("`quote"), "quote");
    assert_eq!(ps("`recur"), "recur");
    assert_eq!(ps("`try"), "try");
    assert_eq!(ps("`catch"), "catch");
    assert_eq!(ps("`finally"), "finally");
    assert_eq!(ps("`new"), "new");
    assert_eq!(ps("`def"), "def");
    assert_eq!(ps("`var"), "var");
    assert_eq!(ps("`throw"), "throw");
    assert_eq!(ps("`set!"), "set!");
    assert_eq!(ps("`fn*"), "fn*");
    assert_eq!(ps("`let*"), "let*");
    assert_eq!(ps("`&"), "&");
}

#[test]
fn syntax_quote_reads_movas_structural_specials_as_core_macros() {
    // `let`/`fn`/`ns`/... are `clojure.core` MACROS on the JVM and read as
    // such; mova has no var cell for them (they are structural dispatch),
    // so `special_forms::SPECIAL_FORM_NAMES` stands in for the mapping.
    assert_eq!(ps("`let"), "clojure.core/let");
    assert_eq!(ps("`fn"), "clojure.core/fn");
    assert_eq!(ps("`loop"), "clojure.core/loop");
    assert_eq!(ps("`ns"), "clojure.core/ns");
    assert_eq!(ps("`binding"), "clojure.core/binding");
    // ...and the qualified spelling still DISPATCHES as the special form.
    assert_eq!(ps("(clojure.core/let [x 1] (clojure.core/fn [] x))").starts_with("#object[user$"), true);
    assert_eq!(ps("(eval `(let [x# 41] (inc x#)))"), "42");
}

#[test]
fn syntax_quote_class_and_method_shapes() {
    assert_eq!(ps("`String"), "java.lang.String");
    assert_eq!(ps("`String."), "java.lang.String.");
    assert_eq!(ps("`java.lang.String"), "java.lang.String");
    assert_eq!(ps("`.foo"), ".foo");
    assert_eq!(ps("`foo.bar"), "foo.bar");
}

#[test]
fn syntax_quote_expands_aliases_in_a_qualified_symbol() {
    assert_eq!(
        ps("(do (ns probe (:require [clojure.string :as str])) `str/join)"),
        "clojure.string/join"
    );
    // An unknown namespace prefix is left exactly as written.
    assert_eq!(ps("`nonexistent/foo"), "nonexistent/foo");
}

/// W3e2: the syntax-quote resolution cache (`Interp::sq_cache`) must never
/// outlive the mapping it was computed from. Every row was checked against
/// real Clojure 1.13.0-alpha6 as well -- it agrees on all of them, which is
/// the point: the cache is invisible.
#[test]
fn syntax_quote_cache_invalidates_on_every_mapping_change() {
    // A core name reads as `clojure.core/...` until THIS namespace defines
    // its own -- the same symbol, the same namespace, two answers.
    assert_eq!(
        ps("(ns probe) [`flatten (do (def flatten 99) `flatten)]"),
        "[clojure.core/flatten probe/flatten]"
    );
    // An alias established AFTER the first read changes a qualified
    // template symbol.
    assert_eq!(
        ps("(ns probe) [`str/join (do (require '[clojure.string :as str]) `str/join)]"),
        "[str/join clojure.string/join]"
    );
    // The reading NAMESPACE is part of the key, so the same bare symbol
    // resolves differently either side of an `ns` switch.
    assert_eq!(
        ps("(ns aa) (def r1 `thing) (ns bb) [aa/r1 `thing]"),
        "[aa/thing bb/thing]"
    );
    // An unmapped name that later gains a def in the SAME namespace keeps
    // the same answer (it was already `<this-ns>/name`) -- a cache hit that
    // is also the right answer, not a stale one.
    assert_eq!(
        ps("(ns probe) [`fresh-nm (do (def fresh-nm 1) `fresh-nm)]"),
        "[probe/fresh-nm probe/fresh-nm]"
    );
}

#[test]
fn syntax_quote_qualification_does_not_disturb_auto_gensyms() {
    // `x#` still mints ONE fresh unqualified symbol per top-level backtick.
    assert_eq!(ps("(let [f `(let [x# 1] x#)] (= (nth f 2) (first (nth f 1))))"), "true");
    assert_eq!(ps("(let [f `(let [x# 1] x#)] (namespace (nth f 2)))"), "nil");
    assert_eq!(ps("`a#b"), "user/a#b");
}

// -------------------- W3e-2: clojure.core is a visible namespace ----------
//
// Every row below was measured on real Clojure 1.13.0-alpha6 (with
// `clojure.repl/apropos` referred) before being written down here.

#[test]
fn ns_publics_of_clojure_core_lists_its_bare_interned_vars() {
    // Was `{}` -- `clojure.core`'s cells intern BARE, and this asked for
    // the `clojure.core/`-qualified spelling that mova never writes.
    assert_eq!(ps("(contains? (ns-publics 'clojure.core) 'map)"), "true");
    assert_eq!(ps("(> (count (ns-publics 'clojure.core)) 100)"), "true");
    assert_eq!(ps("(= (get (ns-publics 'clojure.core) 'map) #'map)"), "true");
}

#[test]
fn structural_special_forms_have_clojure_core_vars() {
    // Oracle: `(contains? (ns-publics 'clojure.core) 'defmacro)` => true,
    // `'let` => true, `'if` => false, `'def` => false (the JVM's own
    // special forms have no var there either).
    assert_eq!(ps("(contains? (ns-publics 'clojure.core) 'defmacro)"), "true");
    assert_eq!(ps("(contains? (ns-publics 'clojure.core) 'let)"), "true");
    assert_eq!(ps("(contains? (ns-publics 'clojure.core) 'if)"), "false");
    assert_eq!(ps("(contains? (ns-publics 'clojure.core) 'def)"), "false");
    assert_eq!(ps("(:macro (meta (resolve 'defmacro)))"), "true");
    assert_eq!(ps("(:macro (meta (resolve 'let)))"), "true");
    // ...and dispatch is unchanged: the special form still wins.
    assert_eq!(ps("(let [x 1] (defmacro m2 [] 2) (m2))"), "2");
}

#[test]
fn apropos_finds_clojure_core_defmacro() {
    // clojure.test-clojure.repl/test-apropos, verbatim.
    assert_eq!(ps("(= '[clojure.core/defmacro] (apropos #\"^defmacro$\"))"), "true");
    assert_eq!(ps("(boolean (some #{'clojure.core/defmacro} (apropos #\"def.acr.\")))"), "true");
    assert_eq!(ps("(boolean (some #{'clojure.core/defmacro} (apropos \"efmac\")))"), "true");
    assert_eq!(ps("(boolean (some #{'clojure.core/defmacro} (apropos 'defmacro)))"), "true");
    assert_eq!(ps("(= [] (apropos \"nothing-has-this-name\"))"), "true");
}

/// W3e-3: `(java.math.MathContext. n)` and `(set! *math-context* ..)`.
///
/// The shared, both-runtimes rows live in `tests/conformance/corpus/vars.
/// corpus` (arithmetic only -- see `tests/conformance/DEVIATIONS.md`'s
/// W3e-3 section for why class identity is not asserted anywhere) and in
/// `compat/math-context-probe.clj`. What is left for here is the part that
/// has no JVM spelling: mova takes the rounding mode by NAME, since it has
/// no `java.math.RoundingMode` enum values.
#[test]
fn math_context_ctor_accepts_a_rounding_mode_by_name() {
    assert_eq!(
        ps("(java.math.MathContext. 8)"),
        "{:precision 8, :rounding-mode \"HALF_UP\"}"
    );
    for spelling in ["\"FLOOR\"", "'FLOOR", ":FLOOR"] {
        assert_eq!(
            ps(&format!(
                "(clojure.main/with-bindings \
                   (set! *math-context* (java.math.MathContext. 6 {spelling})) \
                   (+ 3.5555555M 1))"
            )),
            "4.55555M",
            "for rounding-mode spelling {spelling}"
        );
    }
    assert_eq!(
        eval_err("(java.math.MathContext. 6 \"NOPE\")").message,
        "java.math.MathContext: no such rounding mode: NOPE"
    );
    assert_eq!(
        eval_err("(java.math.MathContext. -1)").message,
        "java.math.MathContext: precision must be a non-negative integer, got -1"
    );
    assert_eq!(
        eval_err("(java.math.MathContext.)").message,
        "java.math.MathContext: expected 1 or 2 args, got 0"
    );
    // `instance?` is constant-false, on purpose -- the value is a map and
    // mova cannot tell it from any other map with those keys.
    assert_eq!(ps("(instance? java.math.MathContext (java.math.MathContext. 8))"), "false");
}

/// W3e-4: `def`/`defn` shadowing a name the namespace only reaches through
/// `clojure.core` warns, exactly like `intern` already did -- one rule, one
/// text (`Interp::shadow_warning`). Oracle rows and their exact strings:
/// `compat/def-shadow-warning-probe.clj` /
/// `compat/def-shadow-warning-probe.mova`.
///
/// The channel is mova's shim-shaped dynamic `*err*` atom (there is no real
/// stderr stream -- see `builtins::nsfns::write_shim_err`), so these
/// assertions set one up the way the vendored suite's
/// `with-err-string-writer` does.
#[test]
fn def_shadowing_a_referred_var_warns_to_err() {
    let harness = "(def ^:dynamic *err* nil) \
                   (defn cap [f] (let [w (atom \"\")] (binding [*err* w] (f)) @w)) ";
    assert_eq!(
        ps(&format!(
            "(ns probe) {harness} (cap (fn [] (eval '(defn prefers [] :mine))))"
        )),
        "\"WARNING: prefers already refers to: #'clojure.core/prefers in namespace: probe, \
         being replaced by: #'probe/prefers\\n\""
    );
    // Redefining a name THIS namespace already interned is silent...
    assert_eq!(
        ps(&format!(
            "(ns probe) {harness} (def mine 1) (cap (fn [] (eval '(def mine 2))))"
        )),
        "\"\""
    );
    // ...and so is a brand-new name.
    assert_eq!(
        ps(&format!(
            "(ns probe) {harness} (cap (fn [] (eval '(def totally-fresh 1))))"
        )),
        "\"\""
    );
    // A plain `def` (not just `defn`) warns identically.
    assert_eq!(
        ps(&format!(
            "(ns probe) {harness} (cap (fn [] (eval '(def flatten 1))))"
        )),
        "\"WARNING: flatten already refers to: #'clojure.core/flatten in namespace: probe, \
         being replaced by: #'probe/flatten\\n\""
    );
}

/// W-lsp-kondo bug 2: `(:refer-clojure :rename {old new ...})` must
/// suppress `Interp::shadow_warning` for `old` exactly like `:exclude`
/// does -- real Clojure's `:rename` gives the referred core var a
/// different LOCAL name, freeing the bare `old` name so the namespace's
/// own def of it isn't a shadow at all. Measured against real vendored
/// code: `datalog.parser.impl` (a `.cljc` file) reads, under mova's `:clj`
/// reader-conditional target, as `(:refer-clojure :rename {distinct?
/// core-distinct?})` with no `:exclude` clause (its `:exclude` is
/// `#?@(:cljs ...)`-gated) -- before this fix, mova had no path that
/// recognized `:rename` at all, so `(defn- distinct? ...)` right after it
/// warned even though real Clojure is silent.
#[test]
fn refer_clojure_rename_suppresses_shadow_warning() {
    let harness = "(def ^:dynamic *err* nil) \
                   (defn cap [f] (let [w (atom \"\")] (binding [*err* w] (f)) @w)) ";
    // Renamed name: silent.
    assert_eq!(
        ps(&format!(
            "(ns probe (:refer-clojure :rename {{flatten core-flatten}})) {harness} \
             (cap (fn [] (eval '(defn- flatten [] :mine))))"
        )),
        "\"\""
    );
    // A DIFFERENT core name the ns did NOT rename still warns normally.
    assert_eq!(
        ps(&format!(
            "(ns probe2 (:refer-clojure :rename {{flatten core-flatten}})) {harness} \
             (cap (fn [] (eval '(defn- prefers [] :mine))))"
        )),
        "\"WARNING: prefers already refers to: #'clojure.core/prefers in namespace: probe2, \
         being replaced by: #'probe2/prefers\\n\""
    );
}

#[test]
fn macroexpand_never_expands_a_special_form_head() {
    // `Compiler.macroexpand1` returns `x` unchanged for a special-form
    // head; without that guard the W3e-2 forwarding macros would make
    // `macroexpand`'s fixpoint loop spin forever on `(let ...)`.
    assert_eq!(ps("(macroexpand '(let [x 1] x))"), "(let [x 1] x)");
    assert_eq!(ps("(macroexpand '(if a b))"), "(if a b)");
    assert_eq!(ps("(macroexpand '(defmacro foo [] 1))"), "(defmacro foo [] 1)");
    assert_eq!(ps("(macroexpand '(clojure.core/let [x 1] x))"), "(clojure.core/let [x 1] x)");
    // An ordinary macro still expands, exactly as before.
    assert_eq!(ps("(macroexpand '(when a b))"), "(if a (do b))");
}

#[test]
fn syntax_quote_macro_body_resolves_against_its_defining_ns() {
    // The `test-dynamic-ns` shape: a macro defined in ns `a` whose
    // expansion names an `a`-local helper must keep working after the
    // CALLER switches namespaces.
    assert_eq!(
        ps("(do (ns aa) (def helper 7) (defmacro m [] `(+ helper 1)) (ns bb) (aa/m))"),
        "8"
    );
}

// -------------------- C11: ~@ over LAZY seqs (the double-wrap family) ------
//
// Every row here was measured on real Clojure 1.13.0-alpha6 first; the
// corpus and its transcript live in `compat/qq-probe.clj` /
// `compat/qq-oracle-transcript.txt`. Before C11, `~@` asked
// `Interp::seq_items` for the spliced value's elements and `seq_items` did
// not implement mova's improper-list rule (`builtins::lazy_tail_split`) --
// so a lazy seq's raw `[head, <unforced rest>]` slots were spliced in
// verbatim, contributing exactly TWO things: the first element, and a
// nested list of all the others.

#[test]
fn splicing_a_lazy_map_result_does_not_double_wrap() {
    // Measured oracle: `(:a 1 2 3 :z)`. Pre-C11 mova: `(:a 1 (2 3) :z)`.
    assert_eq!(ps("`(:a ~@(map identity [1 2 3]) :z)"), "(:a 1 2 3 :z)");
    assert_eq!(ps("`(:a ~@(map inc (range 5)) :z)"), "(:a 1 2 3 4 5 :z)");
    // One- and two-element sources are the shapes where the bug printed an
    // empty/singleton nested list rather than an obvious one.
    assert_eq!(ps("`(:a ~@(map inc [10]) :z)"), "(:a 11 :z)");
    assert_eq!(ps("`(:a ~@(map inc [10 20]) :z)"), "(:a 11 21 :z)");
}

#[test]
fn splicing_a_chunked_partition_result_does_not_double_wrap() {
    // The verbatim mission repro. Measured oracle: `(do (+ 1 2) (+ 3 4))`;
    // pre-C11 mova: `(do (+ 1 2) ((+ 3 4)))`.
    assert_eq!(
        ps("`(~'do ~@(map (fn [a] (cons '+ a)) (partition 2 [1 2 3 4])))"),
        "(do (+ 1 2) (+ 3 4))"
    );
    // `clojure.template/do-template`'s own shape: N argument groups, all of
    // which must survive. This is what silently dropped 6 of 7 groups in
    // the vendored suite's numbers.clj.
    assert_eq!(
        ps("(count `(~'do ~@(map (fn [a] (cons :g a)) (partition 1 (range 7)))))"),
        "8"
    );
}

#[test]
fn splicing_covers_every_lazy_producer() {
    assert_eq!(ps("`(:a ~@(filter even? (range 10)) :z)"), "(:a 0 2 4 6 8 :z)");
    assert_eq!(ps("`(:a ~@(mapcat (fn [x] [x x]) [1 2 3]) :z)"), "(:a 1 1 2 2 3 3 :z)");
    assert_eq!(ps("`(:a ~@(concat (map inc [1 2]) (map inc [3 4])) :z)"), "(:a 2 3 4 5 :z)");
    assert_eq!(ps("`(:a ~@(rest (map inc (range 5))) :z)"), "(:a 2 3 4 5 :z)");
    assert_eq!(ps("`(:a ~@(drop 2 (map inc (range 6))) :z)"), "(:a 3 4 5 6 :z)");
    assert_eq!(ps("`(:a ~@(take-while even? [2 4 5 6]) :z)"), "(:a 2 4 :z)");
    assert_eq!(ps("`(:a ~@(interpose :| [1 2 3]) :z)"), "(:a 1 :| 2 :| 3 :z)");
    assert_eq!(ps("`(:a ~@(lazy-seq [1 2 3]) :z)"), "(:a 1 2 3 :z)");
    assert_eq!(ps("`(:a ~@(take 4 (map inc (range))) :z)"), "(:a 1 2 3 4 :z)");
    // Past the chunk boundary: the count must be exact, not 2.
    assert_eq!(ps("(count `(~@(map inc (range 100))))"), "100");
}

/// W-CONCAT-LAZY conformance: `(apply concat <infinite-seq>)` -- and
/// `mapcat` (`core.mova`'s `(apply concat (apply map f colls))`) built on
/// top of it -- must stay lazy over an infinite/non-chunked source, the
/// same way real Clojure's `RestFn.applyTo` never forces a variadic fn's
/// `& rest` seq. Before this fix, `concat` being a Rust native (not a
/// variadic Clojure closure) meant `apply`'s generic native path drained
/// its whole argument seq up front and these hung forever.
#[test]
fn apply_concat_stays_lazy_over_infinite_source() {
    assert_eq!(
        ps("(take 5 (mapcat (fn [x] [x x]) (iterate inc 0)))"),
        "(0 0 1 1 2)"
    );
    assert_eq!(ps("(take 3 (apply concat (repeat [1])))"), "(1 1 1)");
    // Deep recursive `force`s through 200k lazy cons cells need a bigger
    // stack than a debug/test thread's default -- same reason `ns_test.rs`'s
    // `on_big_stack` exists. Mirrors this crate's own stack-safety gate
    // (`(count (take 200000 (mapcat (fn [x] [x]) (iterate inc 0))))`).
    std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024)
        .spawn(|| {
            assert_eq!(
                ps("(count (take 200000 (mapcat (fn [x] [x]) (iterate inc 0))))"),
                "200000"
            );
        })
        .expect("failed to spawn big-stack worker")
        .join()
        .unwrap_or_else(|payload| std::panic::resume_unwind(payload));
    // Finite/empty edges must still match `(class (concat)) => LazySeq`
    // (prints `()`, not a bare `nil`) -- `doall` forces the `ps` helper's
    // raw Rust printer to see the realized list rather than `#<lazy-seq>`
    // (the mova `println`/`pr-str` builtins force implicitly; `ps` does
    // not, so tests reaching for a printed lazy result use `doall` too).
    assert_eq!(ps("(doall (apply concat []))"), "()");
    assert_eq!(ps("(doall (apply concat [1 2] [[3 4] [5 6]]))"), "(1 2 3 4 5 6)");
}

/// SPEC-W6 conformance: `drop-while`'s 2-arity must be as lazy as real
/// Clojure's own `(lazy-seq (step pred coll))` definition -- work is
/// deferred to first consumption of the RESULT, not run eagerly the
/// moment `(drop-while pred coll)` is called. A counting predicate over
/// `(iterate inc 0)` must fire 0 times before `(first r)` is ever forced,
/// and exactly 4 times after (checking 0, 1, 2, 3).
#[test]
fn drop_while_defers_work_to_first_consumption() {
    assert_eq!(
        ps("(let [n (atom 0)
                   r (drop-while (fn [x] (swap! n inc) (< x 3)) (iterate inc 0))]
               [@n (first r) @n])"),
        "[0 3 4]"
    );
}

#[test]
fn splicing_a_lazy_seq_whose_elements_are_themselves_seqs() {
    // These elements are 1- and 2-slot lists -- the same SHAPE as the
    // internal cons-cell marker. They are data and must stay whole.
    assert_eq!(ps("`(:a ~@(map (fn [x] (list x x)) [1 2 3]) :z)"), "(:a (1 1) (2 2) (3 3) :z)");
    assert_eq!(ps("`(:a ~@(map vec (partition 2 [1 2 3 4])) :z)"), "(:a [1 2] [3 4] :z)");
    // The elements here are themselves still-unforced `Value::Lazy`s, and
    // `ps` prints through `printer::pr_str`, which renders one as
    // `#<lazy-seq>`; the `pr-str` BUILTIN deep-realizes first (see
    // `builtins::strings`), so route this row through it to compare the
    // element VALUES rather than their unrealized print form.
    assert_eq!(
        ps("(pr-str `(:a ~@(map (fn [x] (range x)) [1 2 3]) :z))"),
        "\"(:a (0) (0 1) (0 1 2) :z)\""
    );
}

#[test]
fn splicing_a_lazy_seq_into_vector_set_and_nested_templates() {
    assert_eq!(ps("`[:a ~@(map inc (range 4)) :z]"), "[:a 1 2 3 4 :z]");
    assert_eq!(ps("(count `#{~@(map inc (range 10))})"), "10");
    assert_eq!(ps("`(:a [:b ~@(map inc [1 2])] :z)"), "(:a [:b 2 3] :z)");
    assert_eq!(ps("`(:a {:b [~@(map inc [1 2])]} :z)"), "(:a {:b [2 3]} :z)");
}

#[test]
fn splicing_inside_a_map_template() {
    // C11: measured oracle `` `{~@[:a 1] ~@[:b 2]} `` => `{:a 1, :b 2}`.
    // mova used to raise "unquote-splicing (~@) used outside of a
    // sequence" -- its map arm expanded keys and values one at a time.
    assert_eq!(ps("(count `{~@[:a 1] ~@[:b 2]})"), "2");
    assert_eq!(ps("(get `{~@[:a 1] ~@[:b 2]} :b)"), "2");
    assert_eq!(ps("(get `{~@(map identity [:a 1]) ~@(map identity [:b 2])} :a)"), "1");
    // An odd element count AFTER splicing is a runtime error, matching the
    // JVM's `IllegalArgumentException: No value supplied for key: :b`.
    let err = eval_err("`{~@[:a 1 :b] ~@[]}");
    assert!(
        err.message.contains("No value supplied for key"),
        "unexpected message: {}",
        err.message
    );
}

#[test]
fn a_list_template_that_splices_away_to_nothing_is_nil() {
    // Measured oracle: `` `(~@[]) `` is `nil` (syntax-quote compiles a list
    // to `(seq (concat ...))`), while the empty literal `` `() `` stays
    // `()` and vector/set templates stay `[]`/`#{}`.
    assert_eq!(ps("`(~@[])"), "nil");
    assert_eq!(ps("`(~@(map inc []))"), "nil");
    assert_eq!(ps("`()"), "()");
    assert_eq!(ps("`[~@[]]"), "[]");
    assert_eq!(ps("`#{~@[]}"), "#{}");
    // A template with any surviving literal element is NOT collapsed.
    assert_eq!(ps("`(:a ~@[] :z)"), "(:a :z)");
    assert_eq!(ps("(let [b nil] `(~'do ~@b))"), "(do)");
}

#[test]
fn the_improper_list_rule_still_treats_a_lazy_element_as_data() {
    // C7's half of the shared rule: only a `List` can carry the marker, and
    // only in its LAST slot. Measured oracle: `(count [1 (range 5)])` is 2.
    assert_eq!(ps("(count [1 (range 5)])"), "2");
    assert_eq!(ps("(count `(:a ~@[1 (range 5)] :z))"), "4");
    assert_eq!(ps("(count `[~@[1 (range 5)]])"), "2");
    // `pr-str` builtin (not `ps`'s raw `printer::pr_str`) -- the inner
    // `(range 3)` is still an unforced `Lazy` ELEMENT here; see the note in
    // `splicing_a_lazy_seq_whose_elements_are_themselves_seqs`.
    assert_eq!(ps("(pr-str `(:a ~@(list (list 1 (range 3))) :z))"), "\"(:a (1 (0 1 2)) :z)\"");
}

#[test]
fn drop_stays_lazy_after_the_seq_items_change() {
    // `drop` was the ONE caller that handed `Interp::seq_items` a
    // still-lazy tail; C11 routed it through `seq_of` instead, precisely so
    // that making `seq_items` realize improper lists could not turn this
    // into an infinite walk.
    assert_eq!(ps("(take 3 (drop 2 (range)))"), "(2 3 4)");
    assert_eq!(ps("(take 3 (drop 0 (map inc (range))))"), "(1 2 3)");
    assert_eq!(ps("(drop 0 [1 2 3])"), "(1 2 3)");
    assert_eq!(ps("(drop 5 [1 2 3])"), "()");
    // `drop` builds a fresh seq, so it carries no metadata (measured).
    assert_eq!(ps("(meta (drop 0 (with-meta (list 1 2) {:a 1})))"), "nil");
}

// -------------------- C11: metadata on a LIST form is compile-time only ---
//
// Every expectation below was measured on real Clojure 1.13.0-alpha6 --
// rows 200-213 of `compat/qq-probe.clj`, transcript in
// `compat/qq-oracle-transcript.txt`; see `Interp::eval_form_with_meta`'s
// doc for the rule they establish.

#[test]
fn metadata_on_an_evaluated_list_form_does_not_reach_the_value() {
    assert_eq!(ps("(meta ^{:x 1} (list 1 2))"), "nil");
    assert_eq!(ps("(meta ^long (first (range 3)))"), "nil");
    assert_eq!(ps("(meta ^{:x 1} (let [] [1 2]))"), "nil");
    assert_eq!(ps("(meta ^{:x 1} (if true [1 2]))"), "nil");
    assert_eq!(ps("(meta ^{:x 1} (do [1 2]))"), "nil");
    // The evaluated value's OWN metadata survives untouched.
    assert_eq!(ps("(meta ^{:x 1} (with-meta [1] {:y 2}))"), "{:y 2}");
}

#[test]
fn metadata_on_a_collection_literal_or_fn_form_still_reaches_the_value() {
    assert_eq!(ps("(meta ^{:x 1} [1 2])"), "{:x 1}");
    assert_eq!(ps("(meta ^{:x 1} {:k 1})"), "{:x 1}");
    assert_eq!(ps("(meta ^{:x 1} #{1})"), "{:x 1}");
    // The JVM's one list-form exception: `FnExpr` keeps the form's meta.
    assert_eq!(ps("(meta ^{:x 1} (fn [] 1))"), "{:x 1}");
    // Symbol-position metadata was already dropped (S5/M3).
    assert_eq!(ps("(def v (with-meta [1] {:y 2})) (meta ^{:x 1} v)"), "{:y 2}");
}

// clojure-lsp campaign (mova/PLAN.md): `clojure.zip/zipper` builds
// `^{:zip/branch? branch? :zip/children children :zip/make-node
// make-node} [root nil]`, closing an explicit meta MAP over the
// function's own params. Measured on real Clojure 1.13.0-alpha6:
// `(let [x 42] (meta ^{:a x} [1 2]))` is `{:a 42}`, not `{:a x}` -- a
// bare symbol used as a metadata VALUE resolves like any other code in
// that lexical scope. Only a symbol that resolution genuinely cannot
// place (the `^String`/`^long`/`^Zork` type-hint case, which mova has
// no JVM class world to resolve into) stays literal.
#[test]
fn a_local_or_var_symbol_in_an_explicit_meta_map_resolves_to_its_value() {
    assert_eq!(ps("(let [x 42] (meta ^{:a x} [1 2]))"), "{:a 42}");
    assert_eq!(
        ps("(defn f [x] ^{:a x} [1 2]) (meta (f 42))"),
        "{:a 42}"
    );
    assert_eq!(ps("(def x 7) (meta ^{:a x} [1 2])"), "{:a 7}");
    // Genuinely unresolvable symbols still fall back to the literal
    // symbol -- the type-hint compromise this fix must not regress.
    assert_eq!(ps("(meta ^Zork [1 2])"), "{:tag Zork}");
    assert_eq!(ps("(meta ^{:tag Zork} [1 2])"), "{:tag Zork}");
    // W4 regression (clojure-suite protocols.clj, 196 -> 184 assertions):
    // unlike `Zork`, `String`/`Long` ARE resolvable bare symbols in mova
    // (`java.lang.*` auto-import -- `(class String)` is `java.lang.Class`,
    // measured), so the resolve-first behavior above, applied uniformly
    // to `:tag` too, silently turned every `^String`/`^Long` hint's `:tag`
    // from the literal symbol into a resolved `Value::Class`. Real
    // Clojure's `getBasis`/protocol-var-meta both report `:tag` as a
    // SYMBOL (`(:tag (meta (var baz)))` is the symbol `java.lang.String`,
    // measured), never a `Class` object, on mova's non-JVM host -- so
    // `:tag` must stay literal even when the bare symbol resolves fine
    // for ordinary code. See the `is_tag_key` special case in
    // `eval_meta_form`.
    assert_eq!(ps("(meta ^String [1 2])"), "{:tag String}");
    assert_eq!(ps("(meta ^Long [1 2])"), "{:tag Long}");
    assert_eq!(ps("(meta ^java.lang.String [1 2])"), "{:tag java.lang.String}");
    assert_eq!(ps("(= 'String (:tag (meta ^String [1 2])))"), "true");
    assert_eq!(
        ps("(defrecord R [^String a ^Long b c]) (:tag (meta ((R/getBasis) 0)))"),
        "String"
    );
}

// clojure-lsp campaign (mova/PLAN.md): `set!` on a `deftype`'s own
// `^:unsynchronized-mutable`/`^:volatile-mutable` field. Real Clojure's
// OTHER `set!` target besides a Var, and load-bearing for this campaign:
// `clojure.tools.reader.reader-types` (a transitive dependency of
// `rewrite-clj.reader`) builds its `StringReader` entirely on this
// feature (`s-pos`), so mova could not read a single character of
// Clojure source through rewrite-clj's own reader without it.
#[test]
fn set_bang_mutates_a_deftype_unsynchronized_mutable_field() {
    let src = "(defprotocol P (bump [t]) (show [t]))
               (deftype T [^String s ^:unsynchronized-mutable ^long s-pos]
                 P
                 (bump [_] (set! s-pos (inc s-pos)))
                 (show [_] (str s s-pos)))
               (def t (T. \"a\" 3))
               (bump t)
               (show t)";
    assert_eq!(ps(src), "\"a4\"");
}

#[test]
fn set_bang_mutation_persists_across_separate_method_calls() {
    let src = "(defprotocol P (bump [t]) (show [t]))
               (deftype T [^:unsynchronized-mutable ^long n]
                 P
                 (bump [_] (set! n (inc n)))
                 (show [_] n))
               (def t (T. 0))
               (bump t) (bump t) (bump t)
               (show t)";
    assert_eq!(ps(src), "3");
}

#[test]
fn set_bang_on_a_non_mutable_deftype_field_still_errors() {
    let src = "(defprotocol P (bad [t]))
               (deftype T [s] P (bad [_] (set! s 99)))
               (def t (T. \"a\"))
               (bad t)";
    assert_eq!(eval_err(src).kind, ErrorKind::Unresolved);
}

// clojure-lsp campaign (mova/PLAN.md): measured on real Clojure --
// `(conj {:a 1} nil)` is `{:a 1}`, a no-op, on EVERY real Clojure map
// type (plain, sorted, record, struct-map): `APersistentMap.cons`
// short-circuits on a `null` arg, real Clojure's own comment in
// rewrite-clj.reader/read-with-meta explicitly relies on it ("conj is
// more efficient here than into because it doesn't perform transient/
// persistent conversion if the second argument is nil"). Every OTHER
// collection type (vector/list/set) has no such rule -- `(conj [1] nil)`
// is `[1 nil]`, `nil` conj'd as an ordinary element, measured too.
#[test]
fn conj_nil_onto_a_map_is_a_no_op_but_onto_other_collections_is_an_element() {
    assert_eq!(ps("(conj {:a 1} nil)"), "{:a 1}");
    assert_eq!(ps("(conj (sorted-map :a 1) nil)"), "{:a 1}");
    assert_eq!(ps("[1 nil]"), ps("(conj [1] nil)"));
    assert_eq!(ps("(conj #{1} nil)"), ps("(hash-set 1 nil)"));
    assert_eq!(ps("(conj '(1) nil)"), "(nil 1)");
}

#[test]
fn a_type_hint_on_a_call_form_is_inert_for_arithmetic() {
    // The exact numbers.clj `warn-on-boxed` assertion. Before C11 this
    // raised ">: expected a number, got int" -- the `Value::Meta` wrapper
    // the hint produced is not a number to `builtins::numbers`.
    assert_eq!(ps("(> ^long (first (range 3)) 0)"), "false");
    assert_eq!(ps("(+ ^long (first [1 2]) 1)"), "2");
    assert_eq!(ps("(> ^{:x 1} (first (range 3)) -1)"), "true");
}

// -------------------- defmacro / macroexpand --------------------

#[test]
fn defmacro_unless() {
    let src = "(defmacro unless [test body] `(if ~test nil ~body)) (unless false 42)";
    assert_eq!(ps(src), "42");
}

#[test]
fn defmacro_my_when() {
    let src = "(defmacro my-when [test & body] `(if ~test (do ~@body) nil)) (my-when true 1 2 3)";
    assert_eq!(ps(src), "3");
}

#[test]
fn macroexpand_1_expands_once() {
    let src = "(defmacro unless [test body] `(if ~test nil ~body)) (macroexpand-1 '(unless true 1))";
    assert_eq!(ps(src), "(if true nil 1)");
}

#[test]
fn macroexpand_1_is_noop_on_non_macro_call() {
    assert_eq!(ps("(macroexpand-1 '(+ 1 2))"), "(+ 1 2)");
}

/// Native-macro `defn` fast path (`crate::native_macros::native_defn`,
/// installed by `Interp::install_native_macros`) vs the ORIGINAL
/// interpreted `core.mova` `defn` closure: this table's expected strings
/// were captured from `pr-str`ing `macroexpand-1`'s result with
/// `MOVA_NO_NATIVE_MACROS=1` set (native path off, plain interpreted
/// `defn` on) BEFORE the native path existed -- i.e. this is the
/// differential oracle for the fast path, run here with native macros ON
/// (the default), so a behavioral drift between the two shows up as a
/// normal test failure instead of only being caught by a manual A/B run.
#[test]
fn native_defn_matches_interpreted_expansion() {
    let cases: &[(&str, &str)] = &[
        ("(defn f [x] x)", "(def f (fn f ([x] x)))"),
        (r#"(defn f "doc" [x] x)"#, "(def f (fn f ([x] x)))"),
        (r#"(defn f {:added "1.0"} [x] x)"#, "(def f (fn f ([x] x)))"),
        (r#"(defn f "doc" {:added "1.0"} [x] x)"#, "(def f (fn f ([x] x)))"),
        ("(defn f ([a] a) ([a b] (+ a b)))", "(def f (fn f ([a] a) ([a b] (+ a b))))"),
        (
            r#"(defn f "doc" ([a] a) ([a b] (+ a b)) {:extra true})"#,
            "(def f (fn f ([a] a) ([a b] (+ a b))))",
        ),
        ("(defn ^:private f [x] x)", "(def f (fn f ([x] x)))"),
        ("(defn f [x] x {:not-attr true})", "(def f (fn f ([x] x {:not-attr true})))"),
        ("(defn f [] 1)", "(def f (fn f ([] 1)))"),
        ("(defn f [& args] args)", "(def f (fn f ([& args] args)))"),
    ];
    for (form, expected) in cases {
        let src = format!("(macroexpand-1 '{form})");
        assert_eq!(ps(&src), *expected, "form: {form}");
    }
}

/// Metadata precedence on the `def`'d var, through the native `defn` fast
/// path -- same precedence table `core.mova`'s `defn` doc comment states
/// (trailing attr-map > leading attr-map > docstring > name's own
/// `^{...}` reader meta), plus the auto-computed `:arglists`.
#[test]
fn native_defn_var_meta_precedence() {
    let src = "(defn ^{:foo 1} f {:foo 2} [x] x) (:foo (meta (var f)))";
    assert_eq!(ps(src), "2");
    let src2 = "(defn f {:added \"1.0\"} ([a] a) ([a b] b)) (:arglists (meta (var f)))";
    assert_eq!(ps(src2), "([a] [a b])");
    let src3 = "(defn f [x] x) (:private (meta (var f)))";
    assert_eq!(ps(src3), "nil");
    let src4 = "(defn ^:private f [x] x) (:private (meta (var f)))";
    assert_eq!(ps(src4), "true");
}

/// The native fast path bails (falls back to the interpreted closure) on
/// malformed input and reproduces `defn`'s own `ex-info` spec errors
/// exactly -- `bad name` and `bad fdecl`, same wording as `core.mova`'s
/// `defn` doc comment describes.
#[test]
fn native_defn_falls_back_on_malformed_input() {
    fn thrown_msg(e: &RjError) -> String {
        pr_str(e.thrown.as_ref().unwrap_or(&Value::Nil))
    }
    let e = eval_err("(defn \"bad\" [x] x)");
    let m = thrown_msg(&e);
    assert!(m.contains("defn's name must be a symbol"), "{m}");
    let e2 = eval_err("(defn f)");
    let m2 = thrown_msg(&e2);
    assert!(m2.contains("defn's fdecl must be one or more"), "{m2}");
}

// -------------------- defmacro: docstring + attr-map (measured vs Clojure 1.13.0-alpha6) --------------------
// `clojure -e '(macroexpand (quote (defmacro m "doc" [] 42)))'` etc. confirm:
// a leading docstring, then an optional leading attr-map, in that STRICT
// order, are accepted and become var metadata (S6 -- previously accepted
// in either order and silently dropped, since mova had no metadata
// system yet). A leading attr-map BEFORE a docstring is not recognized as
// one at all and is a real Clojure syntax error (measured:
// `clojure.core.specs.alpha` rejects it) -- see
// `defmacro_attr_map_before_docstring_errors` below, which replaces the
// previous (unmeasured) claim that either order worked. A multi-arity
// body may also have one trailing attr-map after the last `([params]
// body...)` clause; a single-arity body's trailing map is just its last
// body form, not stripped.

#[test]
fn defmacro_docstring_single_arity() {
    assert_eq!(ps(r#"(defmacro m "doc" [] 42) (m)"#), "42");
}

#[test]
fn defmacro_docstring_then_attr_map() {
    assert_eq!(
        ps(r#"(defmacro m "doc" {:x 1} [a] a) (m 5)"#),
        "5"
    );
}

#[test]
fn defmacro_docstring_and_attr_map_become_var_meta() {
    eval(r#"(defmacro m "doc" {:added "1.0"} [a] a)"#);
    // Covered end-to-end (var meta contents) by the corpus/golden pair
    // (`defn-meta.corpus`) instead of duplicated here; this just proves
    // the shape doesn't error.
}

#[test]
fn defmacro_attr_map_before_docstring_errors() {
    // S6: previously asserted (wrongly, per an unmeasured code comment)
    // that this shape was accepted. Measured against the oracle: a
    // leading map is not recognized as an attr-map unless a docstring
    // already preceded it, so `"doc"` here is read as the params vector
    // position and fails to parse -- exactly like real Clojure's
    // `clojure.core.specs.alpha` rejection of the same shape.
    let err = eval_err(r#"(defmacro m {:x 1} "doc" [a] a)"#);
    assert_eq!(err.kind, ErrorKind::Other);
}

#[test]
fn defmacro_docstring_multi_arity() {
    let src = r#"(defmacro m "doc" ([a] a) ([a b] (+ a b))) [(m 1) (m 1 2)]"#;
    assert_eq!(ps(src), "[1 3]");
}

#[test]
fn defmacro_multi_arity_trailing_attr_map() {
    let src = "(defmacro m ([a] a) {:x 1}) (m 7)";
    assert_eq!(ps(src), "7");
}

#[test]
fn defmacro_single_arity_trailing_map_is_body_not_attr_map() {
    // Matches real Clojure: only the multi-arity `(... )+` shape treats a
    // trailing map specially; here `{:x 1}` is just the last body form.
    assert_eq!(ps("(defmacro m [] 42 {:x 1}) (m)"), "{:x 1}");
}

// -------------------- fn*: primitive-fn alias of `fn` --------------------

#[test]
fn fn_star_is_an_alias_of_fn() {
    assert_eq!(ps("((fn* [x y] (+ x y)) 3 4)"), "7");
}

#[test]
fn fn_star_named_self_recurses() {
    let src = "((fn* count-down [n] (if (= n 0) :done (count-down (- n 1)))) 5)";
    assert_eq!(ps(src), ":done");
}

#[test]
fn fn_star_multi_arity_dispatches_by_argc() {
    let src = "(def f (fn* ([x] x) ([x y] (+ x y)))) [(f 1) (f 1 2)]";
    assert_eq!(ps(src), "[1 3]");
}

// -------------------- callable collections --------------------

#[test]
fn keyword_as_fn_on_map() {
    assert_eq!(ps("(:a {:a 1 :b 2})"), "1");
    assert_eq!(ps("(:missing {:a 1} :default)"), ":default");
}

#[test]
fn map_as_fn() {
    assert_eq!(ps("({:a 1} :a)"), "1");
    assert_eq!(ps("({:a 1} :b :none)"), ":none");
}

#[test]
fn set_as_fn() {
    assert_eq!(ps("(#{1 2 3} 2)"), "2");
    assert_eq!(ps("(#{1 2 3} 9)"), "nil");
}

#[test]
fn vector_as_fn() {
    assert_eq!(ps("([:a :b :c] 1)"), ":b");
}

#[test]
fn vector_as_fn_out_of_bounds_errors() {
    let err = eval_err("([:a :b] 5)");
    assert!(err.message.contains("out of bounds"));
}

// -------------------- unresolved symbol --------------------

#[test]
fn unresolved_symbol_error_has_span() {
    let err = eval_err("undefined-thing");
    assert_eq!(err.kind, ErrorKind::Unresolved);
    assert!(err.span.is_some());
    assert!(err.message.contains("undefined-thing"));
}

// -------------------- try/catch/finally/throw --------------------

#[test]
fn try_catch_binds_thrown_value() {
    assert_eq!(ps("(try (throw :boom) (catch e e))"), ":boom");
}

#[test]
fn try_without_error_returns_body_value() {
    assert_eq!(ps("(try 42 (catch e :never))"), "42");
}

/// M8 slice 1 / D13: an internal error whose site has NOT been
/// oracle-measured for a JVM class (`RjError::jvm_class` is `None`) still
/// catch-binds the legacy `{:type :error/<kind> :message ..}` info map --
/// the bound scope that keeps this milestone's first slice small. The form
/// used to be `(+ 1 :not-a-number)`, which no longer belongs here: the
/// arithmetic ops were measured this wave (`numbers::number_reject_class`)
/// and now bind a real `java.lang.ClassCastException`, which is what real
/// Clojure raises for it -- see `try_catch_measured_error_binds_host_class`
/// below. `(conj 1 2)` is an unmeasured, untagged `ErrorKind::TypeErr` and
/// so still exercises the fallback path this test is about.
#[test]
fn try_catch_internal_error_as_info_map() {
    // nREPL gaps: every internal error now binds a host exception instance
    // (the legacy `{:type :error/..}` map is gone); `(conj 1 2)` is an
    // untagged `TypeErr`, so it binds the `TypeErr` default class.
    let src = "(try (conj 1 2) (catch e (.getName (class e))))";
    assert_eq!(ps(src), "\"java.lang.ClassCastException\"");
}

/// M8 slice 1 / D13 (`docs/SPEC-PORT-PATCHES.md` item 13, and the two
/// blocked rows of vendored `spec.clj`'s `conform-explain`): a caught
/// internal error that DID measure its JVM class binds a real host
/// exception instance of that class, so `(class e)` answers the JVM class
/// rather than `clojure.lang.PersistentArrayMap`. Both class names are
/// oracle-measured on 1.13.0-alpha6: `(> nil 5)` =>
/// `NullPointerException` (`Numbers.ops` dereferences the argument to read
/// its class before any cast), `(> :k 5)` => `ClassCastException`.
#[test]
fn try_catch_measured_error_binds_host_class() {
    assert_eq!(
        ps(r#"(try (> nil 5) (catch Throwable t (.getName (class t))))"#),
        r#""java.lang.NullPointerException""#
    );
    assert_eq!(
        ps(r#"(try (> :k 5) (catch Throwable t (.getName (class t))))"#),
        r#""java.lang.ClassCastException""#
    );
}

/// The bound host exception keeps answering every question the info map
/// answered: `.getMessage` is mova's own diagnostic text (unchanged),
/// `instance? Throwable` stays true (`clojure.spec.alpha`'s `validate-fn`
/// reads exactly that pair -- see `hostclass::is_exception_named`'s doc),
/// and `ex-data` is `nil`, which is what a real `NullPointerException`
/// with no ex-data answers.
#[test]
fn measured_error_value_supports_throwable_accessors() {
    assert_eq!(
        ps(r#"(try (> nil 5) (catch Throwable t (.getMessage t)))"#),
        r#"">: expected a number, got nil""#
    );
    assert_eq!(ps("(try (> nil 5) (catch Throwable t (instance? Throwable t)))"), "true");
    assert_eq!(ps("(try (> nil 5) (catch Throwable t (ex-data t)))"), "nil");
    // ...and it is no longer a map, which is also the JVM's answer.
    assert_eq!(ps("(try (> nil 5) (catch Throwable t (map? t)))"), "false");
}

/// Typed `catch` MATCHING and the class of the value it BINDS are now
/// built from one `JvmClass::chain()`, so they cannot disagree: the nil
/// argument is caught by `NullPointerException` (it used to fall through
/// to `ClassCastException`, `ErrorKind::TypeErr`'s untagged default) and
/// the keyword argument by `ClassCastException`.
#[test]
fn measured_error_typed_catch_agrees_with_bound_class() {
    assert_eq!(
        ps("(try (> nil 5) (catch NullPointerException t :npe) (catch Throwable t :other))"),
        ":npe"
    );
    assert_eq!(
        ps("(try (> :k 5) (catch ClassCastException t :cce) (catch Throwable t :other))"),
        ":cce"
    );
}

/// A user `(throw (ex-info ..))` is `ErrorKind::Thrown` and never reaches
/// `error_to_info_map` at all (both `eval_try` and `compile::exec::exec_try`
/// bind `e.thrown` directly), so D13 does not touch it -- asserted here so
/// a future widening of the conversion cannot silently double-convert an
/// `ex-info` and drop its `ex-data`.
#[test]
fn thrown_ex_info_is_unaffected_by_measured_error_binding() {
    assert_eq!(
        ps(r#"(try (throw (ex-info "x" {:a 1})) (catch Throwable t [(.getName (class t)) (ex-data t) (ex-message t)]))"#),
        r#"["clojure.lang.ExceptionInfo" {:a 1} "x"]"#
    );
}

#[test]
fn try_finally_runs_via_def_side_effect() {
    let src = "(def ran false) (try 1 (finally (def ran true))) ran";
    assert_eq!(ps(src), "true");
}

#[test]
fn try_finally_runs_even_when_catching() {
    let src = "(def ran false) (try (throw :x) (catch e nil) (finally (def ran true))) ran";
    assert_eq!(ps(src), "true");
}

#[test]
fn uncaught_throw_propagates_as_thrown_kind() {
    let err = eval_err("(throw :oops)");
    assert_eq!(err.kind, ErrorKind::Thrown);
    assert_eq!(err.thrown, Some(Value::Keyword("oops".into())));
}

// -------------------- C3g: multi-clause / typed catch --------------------

#[test]
fn try_supports_more_than_one_catch_clause() {
    // Was a hard "try: only one catch clause is supported" error.
    let src = "(try (throw :boom) (catch ArithmeticException e :wrong) (catch e e))";
    assert_eq!(ps(src), ":boom");
}

#[test]
fn multi_catch_dispatches_to_first_matching_clause_in_order() {
    assert_eq!(
        ps("(try (/ 1 0) (catch ArithmeticException _ :specific) (catch Exception _ :generic))"),
        ":specific"
    );
}

#[test]
fn multi_catch_falls_through_a_non_matching_earlier_clause() {
    let src = r#"(try (throw (ex-info "x" {})) (catch ArithmeticException _ :specific) (catch Exception _ :generic))"#;
    assert_eq!(ps(src), ":generic");
}

#[test]
fn multi_catch_rethrows_when_no_clause_matches() {
    let src = "(try (/ 1 0) (catch IllegalStateException _ :unreached) (catch NumberFormatException _ :also-unreached))";
    let err = eval_err(src);
    assert_eq!(err.kind, ErrorKind::DivideByZero);
    assert_eq!(err.message, "Divide by zero");
}

#[test]
fn typed_catch_matches_a_named_internal_error_kind() {
    // `ArithmeticException` and `Exception` both catch a checked-division
    // overflow (real ancestry: ArithmeticException <: RuntimeException <:
    // Exception <: Throwable).
    assert_eq!(ps("(try (/ 1 0) (catch ArithmeticException _ :caught))"), ":caught");
    assert_eq!(ps("(try (/ 1 0) (catch Exception _ :caught))"), ":caught");
    assert_eq!(ps("(try (/ 1 0) (catch Throwable _ :caught))"), ":caught");
}

#[test]
fn typed_catch_matches_ex_info_as_exception_info() {
    let src = r#"(try (throw (ex-info "boom" {})) (catch clojure.lang.ExceptionInfo e (ex-message e)))"#;
    assert_eq!(ps(src), "\"boom\"");
}

// clojure-lsp campaign (mova/PLAN.md): a defrecord/deftype field holding
// a partially-forced lazy seq must print FLAT, not as literally-nested
// cons pairs. Measured real bug: rewrite-clj.parser's own (->FormsNode
// (->> (repeatedly ...) (take-while identity))) -- computing the forms-
// node's position metadata calls first/last on the seq first, which
// partially forces it without flattening -- printed {:children (1 (2
// nil))} instead of {:children (1 2)}.
#[test]
fn record_field_holding_a_partially_forced_lazy_seq_prints_flat() {
    let src = "(defrecord R [children])
               (def nodes (->> (repeatedly (let [n (atom 0)] #(let [v (swap! n inc)] (when (<= v 2) v))))
                                (take-while identity)))
               (def _touch1 (first nodes))
               (def _touch2 (last nodes))
               (pr-str (->R nodes))";
    assert_eq!(ps(src), "\"#user.R{:children (1 2)}\"");
}

// clojure-lsp campaign (mova/PLAN.md): `.matcher`/`.matches`/`.group` --
// the `java.util.regex.Pattern`/`Matcher` dot-methods `clojure.tools.
// reader.impl.commons`'s number parser calls directly.
#[test]
fn regex_matcher_matches_and_group_dot_methods() {
    let src = r#"(let [m (.matcher #"([-+]?)([0-9]+)" "42")]
                   [(.matches m) (.group m 0) (.group m 1) (.group m 2)])"#;
    assert_eq!(ps(src), "[true \"42\" \"\" \"42\"]");
    // No match: .matches is false and .group then throws.
    assert_eq!(ps(r#"(.matches (.matcher #"[0-9]+" "abc"))"#), "false");
    assert!(eval_err(r#"(.group (.matcher #"[0-9]+" "abc") 0)"#).message.contains("No match found"));
}

// clojure-lsp campaign (mova/PLAN.md): `java.math.BigInteger`'s 2-arg
// (String, radix) ctor and the three arg-free methods `clojure.tools.
// reader.impl.commons`'s number parser calls on it directly.
#[test]
fn biginteger_radix_ctor_and_bit_length_negate_long_value() {
    assert_eq!(ps(r#"(BigInteger. "1F" 16)"#), "31");
    assert_eq!(ps(r#"(BigInteger. "1010" 2)"#), "10");
    // Long.MAX_VALUE and Long.MIN_VALUE both need exactly 63 bits
    // (excluding the sign) -- measured real Java identity.
    assert_eq!(ps(r#"(.bitLength (BigInteger. "9223372036854775807"))"#), "63");
    assert_eq!(
        ps(r#"(.bitLength (.negate (BigInteger. "9223372036854775808")))"#),
        "63"
    );
    assert_eq!(ps(r#"(.negate (BigInteger. "5"))"#), "-5");
    assert_eq!(ps(r#"(.longValue (BigInteger. "42"))"#), "42");
}

// clojure-lsp campaign (mova/PLAN.md): measured -- `(keyword nil "foo")`
// is `:foo`, real Clojure's 2-arity `Keyword/intern(String ns, String
// name)` treating a `null` ns as "no namespace" rather than casting it.
// `rewrite-clj.reader/read-keyword` calls `(keyword ns name)` with `ns`
// genuinely `nil` for every un-namespaced keyword (the overwhelming
// majority in any real source file).
#[test]
fn keyword_two_arg_with_nil_namespace_is_the_bare_keyword() {
    assert_eq!(ps("(keyword nil \"foo\")"), ":foo");
    assert_eq!(ps("(keyword \"ns\" \"name\")"), ":ns/name");
}

// clojure-lsp campaign (mova/PLAN.md): `StringBuilder`/`StringBuffer`'s
// method surface -- `.append` mutates and returns `this` (so chained
// `.append` calls thread the SAME buffer through), `.toString`/`str`
// both read the accumulated content back out. `rewrite-clj.parser`'s own
// `parse-token` builds every token via `(str buf)`, never `.toString`,
// so `str` specifically (not just `.toString`) has to see real content.
#[test]
fn stringbuilder_append_mutates_and_str_reads_the_accumulated_content() {
    let src = "(let [sb (StringBuilder.)]
                 (.append sb \\a)
                 (.append sb \"bc\")
                 (str sb))";
    assert_eq!(ps(src), "\"abc\"");
    // `.append` returns `this` -- chaining threads the same buffer.
    let chained = "(let [sb (StringBuilder.)]
                     (-> sb (.append \\x) (.append \\y))
                     (str sb))";
    assert_eq!(ps(chained), "\"xy\"");
    assert_eq!(ps("(.toString (doto (StringBuilder.) (.append \\a) (.append \\b)))"), "\"ab\"");
    assert_eq!(ps("(.length (doto (StringBuilder.) (.append \"abc\")))"), "3");
}

// clojure-lsp campaign (mova/PLAN.md): `clojure.lang.ExceptionInfo` as an
// ORDINARY resolvable symbol (not just a `catch` clause head, which
// already worked -- see the test right above). `clojure.tools.reader.
// impl.errors/ex-info?` (a transitive dependency of `rewrite-clj.reader`)
// calls `(instance? clojure.lang.ExceptionInfo x)` as a plain function,
// which needs the class to evaluate as a VALUE.
#[test]
fn clojure_lang_exception_info_resolves_as_a_bare_symbol_and_instance_check() {
    assert_eq!(ps("(class clojure.lang.ExceptionInfo)"), "java.lang.Class");
    assert_eq!(ps(r#"(instance? clojure.lang.ExceptionInfo (ex-info "x" {}))"#), "true");
    assert_eq!(ps("(instance? clojure.lang.ExceptionInfo {:a 1})"), "false");
    assert_eq!(ps("(= clojure.lang.ExceptionInfo (class (ex-info \"x\" {})))"), "true");
}

#[test]
fn typed_catch_does_not_match_an_unrelated_class() {
    // A dotted class name that is not actually an ancestor of what was
    // thrown does not catch it -- it propagates past the whole `try`,
    // same as if no catch clause existed at all.
    let src = r#"(try (throw (ex-info "boom" {})) (catch jank.runtime.object_ref e :never))"#;
    let err = eval_err(src);
    assert_eq!(err.kind, ErrorKind::Thrown);
}

#[test]
fn typed_catch_matches_a_plain_non_exception_thrown_value() {
    // W-ERR (field2, host application field report): real Clojure requires `throw`'s
    // argument to already be a Throwable, so a plain thrown value (here, a
    // keyword) has no JVM-faithful SPECIFIC class ancestry -- but mova,
    // unlike the JVM, permits throwing arbitrary values, and an embedder
    // guarding foreign/plugin code with the idiomatic `(catch Exception e
    // ...)`/`(catch Throwable e ...)` must be able to catch ANYTHING that
    // can be thrown, or the guard is a process-killing hole. `Exception`
    // and `Throwable` now both catch a bare non-Throwable thrown value,
    // exactly like they already caught every internal error -- see
    // `thrown_value_class_chain`'s doc. A genuinely unrelated class token
    // (`typed_catch_does_not_match_an_unrelated_class` below) still does
    // NOT match -- only the generic `Exception`/`RuntimeException`/
    // `Throwable` tail is total, not an arbitrary class name.
    assert_eq!(ps("(try (throw :boom) (catch Exception e :caught))"), ":caught");
    assert_eq!(ps("(try (throw :boom) (catch Throwable e :caught))"), ":caught");
    assert_eq!(ps("(try (throw 42) (catch Exception e :caught))"), ":caught");
    assert_eq!(ps("(try (throw 42) (catch Throwable e :caught))"), ":caught");
    // The catch binding still carries the actual thrown VALUE through
    // (same as the untyped-catch path), not a wrapped error map.
    assert_eq!(ps("(try (throw 42) (catch Exception e e))"), "42");
    assert_eq!(ps("(try (throw :boom) (catch Throwable e e))"), ":boom");
}

#[test]
fn untyped_catch_still_matches_unconditionally() {
    // Regression guard: an untyped `(catch e ...)` clause (no class
    // symbol) must keep matching ANY thrown value, exactly like the
    // single pre-C3g catch always did.
    assert_eq!(ps("(try (/ 1 0) (catch e :caught))"), ":caught");
    assert_eq!(ps("(try (throw :boom) (catch e e))"), ":boom");
}

// -------------------- W-ERR: Throwable->map / ex-cause totality --------------------
//
// host application field report: `Throwable->map` used to crash ("Unable to resolve
// symbol: .getMessage") on a caught internal-error map, and `ex-cause` did
// not exist at all. See `core/core.mova`'s `Throwable->map`/`ex-cause` doc
// comments for the fix shape.

#[test]
fn throwable_to_map_is_total_on_a_caught_internal_error() {
    // A plain internal-error map (`error_to_info_map`'s shape) used to
    // crash `Throwable->map` outright; it must now return a real map whose
    // first `:via` link carries the message.
    let out = ps("(:message (first (:via (try (/ 1 0) (catch Exception e (Throwable->map e))))))");
    assert_eq!(out, "\"Divide by zero\"");
}

#[test]
fn throwable_to_map_is_total_on_ex_info_wrapping_an_internal_error() {
    // The mixed shape from the scout brief: a user `ex-info` whose cause
    // is a caught internal-error map. The walk must not crash stepping
    // from the ex-info link (no `:type` key at all) into the internal-
    // error link (namespaced `:type`).
    let src = r#"(try (/ 1 0) (catch Exception e (Throwable->map (ex-info "outer" {} e))))"#;
    let out = ps(&format!("(count (:via {src}))"));
    assert_eq!(out, "2");
    let outer_msg = ps(&format!("(:message (first (:via {src})))"));
    assert_eq!(outer_msg, "\"outer\"");
    let inner_msg = ps(&format!("(:message (second (:via {src})))"));
    assert_eq!(inner_msg, "\"Divide by zero\"");
}

#[test]
fn ex_cause_chain_through_nested_ex_info() {
    let src = r#"(ex-message (ex-cause (ex-info "outer" {} (ex-info "inner" {}))))"#;
    assert_eq!(ps(src), "\"inner\"");
}

#[test]
fn ex_cause_on_non_throwable_is_nil() {
    assert_eq!(ps("(ex-cause 42)"), "nil");
    assert_eq!(ps(r#"(ex-cause "plain string")"#), "nil");
    assert_eq!(ps("(ex-cause nil)"), "nil");
}

// -------------------- W3a: per-site JVM exception classes --------------------
//
// One row per `error::JvmClass` tag added by W3a, asserted the way the
// vendored corpus asserts them: through a TYPED `catch`. Every expected
// class here was measured against the pinned 1.13.0-alpha6 oracle first
// (transcript in the W3a landing commits); these tests exist so a future
// refactor of an error site cannot silently drop the tag and re-open the
// C3g "honesty flip" it closed.

/// `(catch <class> _ :caught)` around `src`, evaluated -- `":caught"` iff
/// the class matched, and a panic (from `eval`) iff it did not, since an
/// unmatched typed catch lets the error propagate.
fn caught_as(src: &str, class: &str) -> String {
    ps(&format!("(try {src} (catch {class} _ :caught))"))
}

#[test]
fn seq_coercion_failure_is_an_illegal_argument_exception() {
    // Real: `RT.seqFrom` throws IllegalArgumentException ("Don't know how
    // to create ISeq from: java.lang.Long"), NOT ClassCastException.
    for src in ["(first 1)", "(next :k)", "(cons 1 2)", "(get-in {:a 1} 5)"] {
        assert_eq!(caught_as(src, "IllegalArgumentException"), ":caught", "{src}");
    }
    // ... and specifically NOT a ClassCastException.
    assert_eq!(
        eval_err("(try (first 1) (catch ClassCastException _ :never))").kind,
        ErrorKind::TypeErr
    );
}

#[test]
fn nth_index_errors_carry_the_receivers_own_index_class() {
    assert_eq!(caught_as("(nth {} 0)", "UnsupportedOperationException"), ":caught");
    assert_eq!(caught_as("(nth #{1 2} 0)", "UnsupportedOperationException"), ":caught");
    assert_eq!(caught_as("(nth [1 2 3] -1)", "IndexOutOfBoundsException"), ":caught");
    assert_eq!(caught_as("(nth '(1 2 3) -1)", "IndexOutOfBoundsException"), ":caught");
    // A string's index error is the IndexOutOfBoundsException SUBCLASS, so
    // both spellings catch it.
    assert_eq!(caught_as("(nth \"abc\" -1)", "StringIndexOutOfBoundsException"), ":caught");
    assert_eq!(caught_as("(nth \"abc\" -1)", "IndexOutOfBoundsException"), ":caught");
    assert_eq!(caught_as("(nth \"abc\" 7)", "StringIndexOutOfBoundsException"), ":caught");
}

#[test]
fn matcher_nth_class_depends_on_whether_a_match_was_found() {
    // `Matcher.group(i)` state-checks BEFORE it index-checks: no match yet
    // => IllegalStateException; a match but no such group => IndexOutOfBounds.
    let unmatched = "(let [m (re-matcher #\"c\" \"abab\")] (re-find m) (nth m 0))";
    assert_eq!(caught_as(unmatched, "IllegalStateException"), ":caught");
    let matched = "(let [m (re-matcher #\"(a)(b)\" \"abab\")] (re-find m) (nth m 3))";
    assert_eq!(caught_as(matched, "IndexOutOfBoundsException"), ":caught");
}

#[test]
fn clojure_string_rejects_nil_as_a_null_pointer_and_others_as_a_cast() {
    assert_eq!(caught_as("(clojure.string/reverse nil)", "NullPointerException"), ":caught");
    assert_eq!(caught_as("(clojure.string/trim nil)", "NullPointerException"), ":caught");
    assert_eq!(caught_as("(clojure.string/reverse 5)", "ClassCastException"), ":caught");
}

#[test]
fn last_index_of_with_a_negative_from_index_is_nil_not_a_throw() {
    // Java's String.lastIndexOf(str, fromIndex) returns -1 for a negative
    // fromIndex (the mirror of indexOf's clamp-to-zero), so this is `nil`.
    assert_eq!(ps("(clojure.string/last-index-of \"abcz\" \"z\" -10)"), "nil");
    assert_eq!(ps("(clojure.string/index-of \"abcz\" \"z\" -10)"), "3");
}

#[test]
fn parity_bit_ops_and_vector_of_carry_their_measured_classes() {
    assert_eq!(caught_as("(even? 1.5)", "IllegalArgumentException"), ":caught");
    assert_eq!(caught_as("(bit-shift-left 1N 1)", "IllegalArgumentException"), ":caught");
    assert_eq!(caught_as("(vector-of :integer)", "IllegalArgumentException"), ":caught");
    assert_eq!(caught_as("(vector-of :int nil)", "NullPointerException"), ":caught");
}

#[test]
fn stack_and_map_construction_errors_carry_their_measured_classes() {
    assert_eq!(caught_as("(pop ())", "IllegalStateException"), ":caught");
    assert_eq!(caught_as("(pop [])", "IllegalStateException"), ":caught");
    assert_eq!(caught_as("(array-map 1 2 3)", "IllegalArgumentException"), ":caught");
    // Reader duplicate-key: raised RAW as an IllegalArgumentException, NOT
    // wrapped in a LispReader$ReaderException.
    assert_eq!(caught_as("(read-string \"{:a 1 :a 2}\")", "IllegalArgumentException"), ":caught");
    assert_eq!(caught_as("(read-string \"#{1 1}\")", "IllegalArgumentException"), ":caught");
}

#[test]
fn unresolved_symbols_surface_as_a_compiler_exception() {
    // Real Clojure raises "Unable to resolve symbol" as a bare
    // RuntimeException during COMPILATION and rethrows it wrapped, so user
    // code always sees a Compiler$CompilerException with that as the cause.
    // Both spellings of the chain must catch it.
    assert_eq!(caught_as("(eval 'nope)", "clojure.lang.Compiler$CompilerException"), ":caught");
    assert_eq!(caught_as("(eval 'nope)", "Compiler$CompilerException"), ":caught");
    assert_eq!(caught_as("(eval 'nope)", "RuntimeException"), ":caught");
    assert_eq!(caught_as("(eval 'nope)", "Exception"), ":caught");
}

#[test]
fn refer_access_errors_are_an_error_not_an_exception() {
    // IllegalAccessError <: IncompatibleClassChangeError <: LinkageError <:
    // Error <: Throwable -- so `Throwable` catches it and `Exception` must NOT.
    let src = "(ns w3a-reftmp) (def known 1) (ns user) \
               (refer 'w3a-reftmp :only '(no-such-var))";
    assert_eq!(caught_as(src, "IllegalAccessError"), ":caught");
    assert_eq!(caught_as(src, "Error"), ":caught");
    assert_eq!(caught_as(src, "Throwable"), ":caught");
    let err = eval_err(&format!("(try {src} (catch Exception _ :never))"));
    assert!(err.message.contains("no-such-var does not exist"), "{}", err.message);
}

#[test]
fn fn_arglist_rejections_are_exception_info() {
    // Real Clojure rejects these via clojure.spec and reports the failure
    // as a Compiler$CompilerException whose cause is an ExceptionInfo;
    // mova has no cause chain, so the ExceptionInfo (what fn.clj/def.clj
    // both assert against) is what the error presents as.
    for src in [r#"(eval '(fn "a" a))"#, "(eval '(fn (1)))", "(eval '(fn))", "(eval '(fn a))"] {
        assert_eq!(caught_as(src, "clojure.lang.ExceptionInfo"), ":caught", "{src}");
    }
}

#[test]
fn defn_rejects_a_non_symbol_name_with_a_spec_shaped_ex_info() {
    let src = r#"(eval '(defn "bad docstring" tname [a b]))"#;
    assert_eq!(caught_as(src, "clojure.lang.ExceptionInfo"), ":caught");
    let msg = ps(&format!("(try {src} (catch e (ex-message e)))"));
    assert!(msg.contains("did not conform to spec"), "{msg}");
    assert!(msg.contains("must be a symbol"), "{msg}");
}

#[test]
fn multimethod_and_protocol_dispatch_failures_are_illegal_argument() {
    assert_eq!(
        caught_as("(defmulti w3a-m identity) (w3a-m 1)", "IllegalArgumentException"),
        ":caught"
    );
    assert_eq!(
        caught_as("(defprotocol W3aP (w3a-foo [this])) (w3a-foo 10)", "IllegalArgumentException"),
        ":caught"
    );
}

#[test]
fn quote_with_extra_args_is_a_compiler_exception_wrapping_the_form() {
    // Real: Compiler$CompilerException whose .getCause is an ExceptionInfo
    // carrying {:form (quote 1 2 3)}. Both levels are asserted by
    // special.clj's `quote-with-multiple-args`.
    assert_eq!(
        caught_as("(eval '(quote 1 2 3))", "clojure.lang.Compiler$CompilerException"),
        ":caught"
    );
    assert_eq!(
        ps("(try (eval '(quote 1 2 3)) (catch e (-> e (.getCause) (ex-data) (:form))))"),
        "(quote 1 2 3)"
    );
}

#[test]
fn a_class_name_in_both_the_class_and_interface_tables_is_one_class() {
    // `java.util.Collection` lives in BOTH types::builtin_classes() and
    // types::builtin_interfaces(), so it can be reached as a
    // ClassVal::Builtin or a ClassVal::Interface. `Hash for Value` hashes
    // just the name for both, so `=` must agree -- otherwise every
    // hash-map lookup keyed on one and probed with the other misses, which
    // is what broke derive-bridged `isa?` before W3a.
    assert_eq!(
        ps("(derive java.util.Collection ::w3a-coll) \
            [(isa? java.util.Collection ::w3a-coll) \
             (isa? clojure.lang.PersistentVector ::w3a-coll)]"),
        "[true true]"
    );
}

#[test]
fn finally_runs_once_after_a_matched_clause_in_a_multi_catch_try() {
    let src = "(def ran (atom nil)) \
               (try (/ 1 0) \
                 (catch ArithmeticException _ :caught) \
                 (catch Exception _ :never) \
                 (finally (reset! ran :ran))) \
               [@ran]";
    assert_eq!(ps(src), "[:ran]");
}

#[test]
fn finally_runs_once_even_when_no_clause_matches_and_error_propagates() {
    let src = "(def ran (atom nil)) \
               (try (try (/ 1 0) \
                       (catch IllegalStateException _ :unreached) \
                       (finally (reset! ran :ran))) \
                 (catch ArithmeticException _ :propagated)) \
               [@ran]";
    // Wrapped in an outer catch purely so `eval` (not `eval_err`) can read
    // back the inner `finally`'s side effect: the point under test is
    // that `finally` still ran exactly once even though the inner
    // `try`'s own catch never matched.
    assert_eq!(ps(src), "[:ran]");
}

#[test]
fn arity_error_matches_both_arity_exception_and_its_real_superclass() {
    // Real ancestry: `clojure.lang.ArityException extends
    // IllegalArgumentException`.
    let src = "(defn g [x] x) (try (g) (catch clojure.lang.ArityException _ :arity))";
    assert_eq!(ps(src), ":arity");
    let src2 = "(defn g [x] x) (try (g) (catch IllegalArgumentException _ :iae))";
    assert_eq!(ps(src2), ":iae");
}

// -------------------- stack overflow --------------------

#[test]
fn stack_overflow_guard_triggers_on_unbounded_non_tail_recursion() {
    // No `recur`, so every call grows the real Rust stack via
    // `apply_closure`; the depth guard must trip before we blow it. Run on
    // an explicitly generous worker stack rather than trusting whatever
    // (often tiny — 512KB on macOS) default `cargo test`'s harness thread
    // happens to get; MAX_CALL_DEPTH itself is tuned against a realistic
    // *main*-thread budget (see its doc comment in eval/mod.rs).
    let handle = std::thread::Builder::new()
        .stack_size(8 * 1024 * 1024)
        .spawn(|| {
            // `RjError` holds `Rc`s (via `Value::thrown`) so it isn't
            // `Send`; extract just the message before crossing threads.
            let src = "(def f (fn [n] (+ 1 (f (+ n 1))))) (f 0)";
            eval_err(src).message
        })
        .expect("spawn worker thread");
    let message = handle.join().expect("worker thread must not itself overflow");
    assert!(message.contains("stack overflow"), "message: {message}");
}

// -------------------- numeric cross-type equality --------------------

#[test]
fn numeric_cross_type_equality() {
    // S5 (SPEC-numtower): `=` is CATEGORY-STRICT, so an `Int` is never
    // `=` to a `Float` (measured `(= 1 1.0)` => `false`; this used to be
    // the `numbers.corpus:29` entry in tests/conformance/DEVIATIONS.md).
    // Cross-type numeric equality now lives in `==`, and the one bridge
    // `=` keeps is INSIDE the integer category.
    assert_eq!(ps("(= 1 1.0)"), "false");
    assert_eq!(ps("(== 1 1.0)"), "true");
    assert_eq!(ps("(= 1 2)"), "false");
    assert_eq!(ps("(= 1 1N)"), "true");
    assert_eq!(ps("(= 1N (biginteger 1))"), "true");
    assert_eq!(ps("(= 1/2 0.5)"), "false");
    assert_eq!(ps("(== 1/2 0.5)"), "true");
}

// -------------------- ns --------------------

#[test]
fn ns_is_accepted_and_recorded() {
    let mut interp = Interp::new();
    interp.eval_str("test", "(ns foo.bar)").unwrap();
    assert_eq!(interp.current_ns.as_ref(), "foo.bar");
}

// -------------------- eval_forms / eval_str plumbing --------------------

#[test]
fn eval_forms_returns_last_value() {
    let mut interp = Interp::new();
    let forms = read_all("1 2 3").unwrap();
    assert_eq!(interp.eval_forms(&forms).unwrap(), Value::Int(3));
}

#[test]
fn eval_str_sets_source_for_diagnostics() {
    let mut interp = Interp::new();
    let _ = interp.eval_str("myfile.mova", "1");
    assert_eq!(interp.source_name.as_ref(), "myfile.mova");
    assert_eq!(interp.source.as_ref(), "1");
}

// -------------------- force / seq_items / values_equal (Interp helpers) --------------------

fn lazy_of(v: Value) -> Value {
    use crate::value::{LazySeq, NativeFn};
    use std::sync::{Arc, Mutex};
    let thunk = Value::Native(Arc::new(NativeFn::new("test-thunk", move |_i, _args| Ok(v.clone()))));
    Value::Lazy(Arc::new(LazySeq {
        thunk: Mutex::new(Some(thunk)),
        realized: Mutex::new(None),
    }))
}

#[test]
fn force_realizes_and_memoizes_a_lazy_seq() {
    let mut interp = Interp::new();
    let lazy = lazy_of(Value::List(pvec![Value::Int(1), Value::Int(2)]));
    let forced = interp.force(&lazy).unwrap();
    assert_eq!(forced, Value::List(pvec![Value::Int(1), Value::Int(2)]));
    // realized cell should now be memoized: forcing again gives the same value
    let forced_again = interp.force(&lazy).unwrap();
    assert_eq!(forced_again, forced);
}

#[test]
fn seq_items_covers_all_seqable_shapes() {
    let mut interp = Interp::new();
    assert_eq!(interp.seq_items(&Value::Nil).unwrap(), None);
    assert_eq!(
        interp
            .seq_items(&Value::Vector(pvec![Value::Int(1)]))
            .unwrap(),
        Some(pvec![Value::Int(1)])
    );
    assert_eq!(interp.seq_items(&Value::Str("ab".into())).unwrap(), Some(pvec![Value::Char('a'), Value::Char('b')]));
    assert_eq!(interp.seq_items(&Value::Vector(PVec::new())).unwrap(), None);
}

/// C3e: the same cell as [`lazy_of`], wrapped in the internal
/// continuation marker -- i.e. what `cons_builtin`/`gen_chunk` park in a
/// cons cell's last slot, as opposed to a lazy seq stored as an element.
fn lazy_tail_of(v: Value) -> Value {
    match lazy_of(v) {
        Value::Lazy(cell) => Value::LazyTail(cell),
        _ => unreachable!("lazy_of always builds a Lazy"),
    }
}

#[test]
fn seq_items_applies_the_improper_list_rule() {
    // C11: THE fix, at the level it was made. A `List` whose LAST slot is
    // the continuation marker is a cons cell -- that slot is the REST of
    // the sequence, not an element -- so `seq_items` must return the
    // REALIZED elements, the same answer `builtins::materialize` gives.
    // Returning the raw slots (`[1, <lazy>]`, length 2) was the `~@`
    // double-wrap bug.
    let mut interp = Interp::new();
    let improper = Value::List(pvec![
        Value::Int(1),
        lazy_tail_of(Value::List(pvec![Value::Int(2), Value::Int(3)]))
    ]);
    assert_eq!(
        interp.seq_items(&improper).unwrap(),
        Some(pvec![Value::Int(1), Value::Int(2), Value::Int(3)])
    );

    // C3e's half of the same rule, and the reason the marker got its own
    // discriminant: an UNMARKED `Lazy` in the same slot is one opaque
    // ELEMENT, whatever collection holds it. Pre-C3e the `List` row here
    // spliced (wrongly -- `(count (seq [:x (range 2 5)]))` was 4) and only
    // the `Vector` row was exempted, by hand.
    let list_with_lazy = Value::List(pvec![Value::Int(1), lazy_of(Value::List(pvec![Value::Int(2)]))]);
    assert_eq!(interp.seq_items(&list_with_lazy).unwrap().unwrap().len(), 2);
    let vec_with_lazy = Value::Vector(pvec![Value::Int(1), lazy_of(Value::List(pvec![Value::Int(2)]))]);
    assert_eq!(interp.seq_items(&vec_with_lazy).unwrap().unwrap().len(), 2);

    // A ONE-slot `[Lazy]` list is not a cons cell either -- the lazy value
    // is that list's single element.
    let single = Value::List(pvec![lazy_of(Value::List(pvec![Value::Int(9)]))]);
    assert_eq!(interp.seq_items(&single).unwrap().unwrap().len(), 1);

    // ... and a plain flat list is untouched.
    assert_eq!(
        interp.seq_items(&Value::List(pvec![Value::Int(1), Value::Int(2)])).unwrap(),
        Some(pvec![Value::Int(1), Value::Int(2)])
    );
}

#[test]
fn values_equal_forces_lazy_before_comparing() {
    let mut interp = Interp::new();
    // `force` requires a seqable-or-nil result, so wrap a list, not a bare
    // scalar. Without forcing, `Value::Lazy` vs `Value::List` never compare
    // equal (see value.rs's `PartialEq`) — so this only passes if
    // `values_equal` actually forces both sides first.
    let inner = Value::List(pvec![Value::Int(5)]);
    let lazy = lazy_of(inner.clone());
    assert!(interp.values_equal(&lazy, &inner).unwrap());
}

#[test]
fn values_equal_is_numeric_category_strict() {
    // S5 renamed and inverted this test: `values_equal` used to BLEND
    // `Int`/`Float` (`1` == `1.0`), which is what `==` does, not what `=`
    // does. Real Clojure's `=` compares `clojure.lang.Numbers.Category`
    // first and answers false across categories -- measured.
    let mut interp = Interp::new();
    assert!(!interp.values_equal(&Value::Int(1), &Value::Float(1.0)).unwrap());
    assert!(!interp.values_equal(&Value::Int(1), &Value::Float(1.5)).unwrap());
    assert!(interp.values_equal(&Value::Int(1), &Value::Int(1)).unwrap());
    assert!(interp.values_equal(&Value::Float(1.0), &Value::Float(1.0)).unwrap());
}

// -------------------- M8: PText rope integration (real builtins, real Interp) --------------------
// Cross-checks the Str Flat/Rope dual representation from value.rs's own
// tests, but end-to-end through the actual builtins (`str`/`subs`/`nth`/
// `count`/`blank?`/`index-of`/`empty?`) an evaluated program would call --
// not just the Str methods those builtins were rewritten to use.

#[test]
fn m8_big_string_literal_promotes_to_rope_and_counts_correctly() {
    let big = "a".repeat(70_000);
    let src = format!("\"{big}\"");
    match eval(&src) {
        Value::Str(s) => {
            assert!(s.is_rope(), "a 70,000-byte string literal should promote to Rope");
            assert_eq!(s.char_count_cached(), 70_000);
        }
        other => panic!("expected a Str, got a {}", other.type_name()),
    }
}

#[test]
fn m8_small_string_literal_stays_flat() {
    match eval("\"hello\"") {
        Value::Str(s) => assert!(!s.is_rope()),
        other => panic!("expected a Str, got a {}", other.type_name()),
    }
}

#[test]
fn m8_editor_splice_shape_stays_rope_native_and_correct() {
    // Exactly omawritejank's `oma.core.edit/insert`: (str (subs text 0
    // from) s (subs text to)) -- the real per-keystroke splice path this
    // milestone exists to speed up. `text` is 70,000 'x's with the range
    // [30000, 30000) replaced by "INSERTED" (a pure insertion, like a
    // single keystroke).
    let big = "x".repeat(70_000);
    let src = format!(
        r#"(let [text "{big}"
                   from 30000
                   to 30000]
               (str (subs text 0 from) "INSERTED" (subs text to)))"#
    );
    match eval(&src) {
        Value::Str(s) => {
            assert!(s.is_rope(), "splicing a Rope-backed document should produce a Rope result");
            assert_eq!(s.char_count_cached(), 70_000 + "INSERTED".len());
            let expected = format!("{}INSERTED{}", "x".repeat(30_000), "x".repeat(40_000));
            assert_eq!(&*s, expected);
        }
        other => panic!("expected a Str, got a {}", other.type_name()),
    }
}

#[test]
fn m8_editor_delete_and_replace_shapes_stay_correct() {
    // 70,000 bytes: comfortably over STR_ROPE_MIN (65,536) so `text`
    // itself promotes to Rope -- a 50,000-byte doc would stay Flat and
    // silently pass this test for the wrong reason.
    let big = "y".repeat(70_000);
    // Pure deletion: (str (subs text 0 10000) (subs text 20000)).
    let del_src = format!(r#"(let [text "{big}"] (str (subs text 0 10000) (subs text 20000)))"#);
    match eval(&del_src) {
        Value::Str(s) => {
            assert!(s.is_rope());
            assert_eq!(s.char_count_cached(), 60_000);
            assert_eq!(&*s, "y".repeat(60_000));
        }
        other => panic!("expected a Str, got a {}", other.type_name()),
    }

    // Replace a mid-document range with different-length text.
    let repl_src = format!(r#"(let [text "{big}"] (str (subs text 0 100) "ZZZZZ" (subs text 200)))"#);
    match eval(&repl_src) {
        Value::Str(s) => {
            assert!(s.is_rope());
            assert_eq!(s.char_count_cached(), 100 + 5 + (70_000 - 200));
            assert_eq!(&*s, format!("{}ZZZZZ{}", "y".repeat(100), "y".repeat(70_000 - 200)));
        }
        other => panic!("expected a Str, got a {}", other.type_name()),
    }
}

#[test]
fn m8_nth_count_blank_index_of_empty_agree_on_a_big_rope_document() {
    let mut doc = "line one\n".to_string();
    doc.push_str(&"filler ".repeat(10_000)); // pushes this over STR_ROPE_MIN
    doc.push_str("\nline three\n");
    let n_chars = doc.chars().count();
    let first_newline = doc.find('\n').unwrap();
    let src = format!(
        r#"(let [text "{doc}"]
               [(count text)
                (nth text 0)
                (clojure.string/index-of text "\n")
                (clojure.string/blank? text)
                (empty? text)
                (empty? "")])"#
    );
    match eval(&src) {
        Value::Vector(items) => {
            assert_eq!(items[0], Value::Int(n_chars as i64));
            assert_eq!(items[1], Value::Char('l'));
            assert_eq!(items[2], Value::Int(first_newline as i64));
            assert_eq!(items[3], Value::Bool(false));
            assert_eq!(items[4], Value::Bool(false));
            assert_eq!(items[5], Value::Bool(true));
        }
        other => panic!("expected a Vector, got a {}", other.type_name()),
    }
}

#[test]
fn m8_cross_representation_equality_through_eval() {
    // A Flat string built from pieces that individually stay below
    // STR_ROPE_MIN, compared against a Rope string of equal content built
    // in one big literal -- must be `=` (representation-blind).
    let big = "q".repeat(70_000);
    let src = format!(r#"(= "{big}" (apply str (repeat 70000 "q")))"#);
    assert_eq!(eval(&src), Value::Bool(true));
}

// -------------------- C3e: the improper-list marker ------------------------
//
// Before C3e the "rest of the sequence continues here" marker was an
// untagged `Value::Lazy` occupying a `Value::List`'s last slot, which is
// byte-for-byte the same shape as a list whose last ELEMENT is a lazy seq.
// The ambiguity leaked in both directions; every row below was measured on
// real Clojure 1.13.0-alpha6 first, and the two families are deliberately
// adjacent so a future change cannot fix one by re-breaking the other.
// See `Value::LazyTail`'s doc for the representation.

#[test]
fn c3e_a_lazy_seq_in_a_list_slot_is_data_not_a_continuation() {
    // The mission repro (`data_structures.clj`'s
    // ordered-collection-equality-test): a two-element sequence whose
    // second element is a range. Pre-C3e: count 4, next `(2 3 4)`.
    assert_eq!(ps("(count (seq [:x (range 2 5)]))"), "2");
    assert_eq!(ps("(pr-str (next (seq [:x (range 2 5)])))"), "\"((2 3 4))\"");
    assert_eq!(ps("(pr-str (seq [:x (range 2 5)]))"), "\"(:x (2 3 4))\"");
    // The `Vector` form always behaved (C7 exempted vectors by hand); the
    // point of C3e is that the guarantee now SURVIVES the seq boundary,
    // where `seq_items` turns that vector into a `List`.
    assert_eq!(ps("(count [:x (range 2 5)])"), "2");
    assert_eq!(ps("(count (list :x (range 2 5)))"), "2");
    assert_eq!(ps("(pr-str (list 1 (range 3)))"), "\"(1 (0 1 2))\"");
    // Same shape reached through every other seq producer.
    assert_eq!(ps("(count (seq (list :x (range 2 5))))"), "2");
    assert_eq!(ps("(count (into [] [:x (range 2 5)]))"), "2");
    assert_eq!(ps("(count (map identity [:x (range 2 5)]))"), "2");
    assert_eq!(ps("(count (reverse [:x (range 2 5)]))"), "2");
    assert_eq!(ps("(count (concat [:x] [(range 2 5)]))"), "2");
    // ... and one level down, as an element of a longer list.
    assert_eq!(ps("(count (seq [:a :b (range 2 5)]))"), "3");
    assert_eq!(ps("(pr-str (last (seq [:a :b (range 2 5)])))"), "\"(2 3 4)\"");
}

#[test]
fn c3e_equality_and_hash_see_a_trailing_lazy_element_as_one_element() {
    // Pre-C3e `values_equal`'s `has_lazy_tail` peeled the trailing range
    // open on one side only, so these disagreed with the oracle.
    assert_eq!(ps("(= (seq [:x (range 2 5)]) (list :x (list 2 3 4)))"), "true");
    assert_eq!(ps("(= (seq [:x (range 2 5)]) (list :x 2 3 4))"), "false");
    assert_eq!(ps("(= [:x (range 2 5)] (seq [:x (range 2 5)]))"), "true");
    // `=` implies equal `hash` -- the contract `sorted::hash_value` and
    // every map/set keyed on such a value depend on.
    assert_eq!(ps("(= (hash (seq [:x (range 2 5)])) (hash (list :x (list 2 3 4))))"), "true");
    // Realized on both sides, so `Value`'s own `Hash` (which cannot force
    // -- no `&mut Interp`) agrees too and set membership finds it. The
    // UNrealized nested-lazy form of this row is the known "lazy value
    // nested inside a key" deviation, unchanged by C3e and out of its
    // scope (see `values_equal`'s KEY LOOKUP note).
    assert_eq!(
        ps("(contains? #{(list :x (list 2 3 4))} (doall (seq [:x (doall (range 2 5))])))"),
        "true"
    );
}

#[test]
fn c3e_a_genuine_cons_cell_still_splices_its_tail() {
    // The other direction: the marker must still MEAN "rest of the
    // sequence" everywhere -- walking, counting, comparing AND printing.
    // `(pr-str (cons 1 (range 3)))` was `"(1 (0 1 2))"` pre-C3e, because
    // `realize_deep` could not tell a cons cell from data and (correctly,
    // given the ambiguity) chose data.
    assert_eq!(ps("(pr-str (cons 1 (range 3)))"), "\"(1 0 1 2)\"");
    assert_eq!(ps("(count (cons 1 (range 3)))"), "4");
    assert_eq!(ps("(= (cons 1 (range 3)) '(1 0 1 2))"), "true");
    assert_eq!(ps("(pr-str (rest (cons 1 (range 3))))"), "\"(0 1 2)\"");
    assert_eq!(ps("(pr-str (cons 1 (lazy-seq [2])))"), "\"(1 2)\"");
    assert_eq!(ps("(pr-str (map inc (range 5)))"), "\"(1 2 3 4 5)\"");
    // Past `gen_chunk`'s 1024-element chunk boundary, where the marker
    // sits at the end of a 1024-slot list rather than a 2-slot one.
    assert_eq!(ps("(count (range 3000))"), "3000");
    assert_eq!(ps("(reduce + (range 3000))"), "4498500");
    assert_eq!(ps("(count (take 2000 (iterate inc 0)))"), "2000");
    assert_eq!(ps("(count (take 2000 (repeat :x)))"), "2000");
    // The marker never escapes as a value: `rest` hands back a plain
    // lazy seq, indistinguishable from any other.
    assert_eq!(ps("(type (rest (cons 1 (range 3))))"), "clojure.lang.LazySeq");
    assert_eq!(ps("(type (rest (list 1 (range 3))))"), "clojure.lang.PersistentList");
}

#[test]
fn c3e_force_seqs_the_realized_value_like_rt_seq() {
    // `LazySeq.sval` calls `RT.seq` on the body's value and keeps the SEQ;
    // mova used to check it against a seqable whitelist and memoize the
    // RAW value, leaving the cell holding a set/string/array. Measured
    // oracle for every row.
    assert_eq!(ps("(= (lazy-seq #{}) ())"), "true");
    assert_eq!(ps("(= (lazy-seq #{:q}) '(:q))"), "true");
    assert_eq!(ps("(= (lazy-seq \"abc\") '(\\a \\b \\c))"), "true");
    assert_eq!(ps("(= (lazy-seq (sorted-set 1 2)) '(1 2))"), "true");
    assert_eq!(ps("(pr-str (lazy-seq (sorted-set 3 1 2)))"), "\"(1 2 3)\"");
    assert_eq!(ps("(= (lazy-seq (into-array [1 2])) '(1 2))"), "true");
    assert_eq!(ps("(= (lazy-seq {:a 1}) '([:a 1]))"), "true");
    // An empty realization is `nil`-the-seq, which is `=` to any empty
    // sequential but NEVER to bare `nil` -- the distinction `values_equal`
    // keeps with its `a_was_lazy` flag, and the reason `RT.seq`'s `nil`
    // must not collapse the LazySeq object itself.
    assert_eq!(ps("(= (lazy-seq #{}) nil)"), "false");
    assert_eq!(ps("(= (lazy-seq (into-array [])) nil)"), "false");
    assert_eq!(ps("(= (lazy-seq nil) ())"), "true");
    assert_eq!(ps("(= (lazy-seq nil) nil)"), "false");
    // A body that returns the improper-list encoding must NOT be realized
    // by `force` -- `(cons x (lazy-seq ...))` stays lazy in the tail, so
    // an infinite chain is still walkable one chunk at a time.
    assert_eq!(ps("(count (take 5 (lazy-seq (cons 1 (iterate inc 2)))))"), "5");
    assert_eq!(ps("(count (lazy-seq (range 3000)))"), "3000");
    // Still an error for a genuinely non-seqable body -- now phrased by
    // `seq_items`, the one conversion, instead of a parallel whitelist.
    assert!(eval_err("(doall (lazy-seq 5))")
        .message
        .contains("don't know how to create a seq from"));
}

#[test]
fn c3e_a_lazy_value_works_as_a_map_or_set_key() {
    // `Value`'s `Hash` cannot force a thunk, so an unrealized `Lazy` used
    // to hash by `Arc` pointer and never find its `=`-equal list key.
    // Normalized at both the INSERT and the LOOKUP boundary.
    assert_eq!(ps("(get {(repeat 1 :x) :z} '(:x))"), ":z");
    assert_eq!(ps("(= {(repeat 1 :x) :z} {'(:x) :z})"), "true");
    // ... and symmetrically, a lazy PROBE against a proper stored key.
    assert_eq!(ps("(get {'(:x) :z} (repeat 1 :x))"), ":z");
    assert_eq!(ps("(contains? #{'(:x)} (repeat 1 :x))"), "true");
    assert_eq!(ps("(contains? #{(repeat 1 :x)} '(:x))"), "true");
    // Every insert boundary, not just the literals.
    assert_eq!(ps("(get (hash-map (repeat 1 :x) :z) '(:x))"), ":z");
    assert_eq!(ps("(get (array-map (repeat 1 :x) :z) '(:x))"), ":z");
    assert_eq!(ps("(get (assoc {} (repeat 1 :x) :z) '(:x))"), ":z");
    assert_eq!(ps("(get (conj {} [(repeat 1 :x) :z]) '(:x))"), ":z");
    assert_eq!(ps("(contains? (set [(repeat 1 :x)]) '(:x))"), "true");
    assert_eq!(ps("(contains? (hash-set (repeat 1 :x)) '(:x))"), "true");
    assert_eq!(ps("(contains? (conj #{} (repeat 1 :x)) '(:x))"), "true");
    // Two spellings of the same key collapse to ONE entry, which is the
    // whole point: `=`-equal keys must be the same key.
    assert_eq!(ps("(count (set [(repeat 1 :x) '(:x)]))"), "1");
    assert_eq!(ps("(count {(repeat 1 :x) :a '(:x) :b})"), "1");
    // An empty lazy seq keys as the empty SEQUENCE, never as nil --
    // matching `values_equal`'s `(lazy-seq) != nil` rule.
    assert_eq!(ps("(get {(lazy-seq) :z} '())"), ":z");
    assert_eq!(ps("(get {(lazy-seq) :z} nil)"), "nil");
    // Non-lazy keys are untouched -- including the float-identity gap in
    // key lookup that `values_equal`'s own note calls out as deliberately
    // NOT closed (`Value` would need a non-reflexive `Eq`). These two rows
    // are here to pin that this normalization did not disturb it: both
    // still report mova's pre-C3e answers, not the oracle's.
    assert_eq!(ps("(get {:a 1} :a)"), "1");
    assert_eq!(ps("(get {0.0 :z} -0.0)"), "nil");
    assert_eq!(ps("(count (set [##NaN ##NaN]))"), "1");
}

#[test]
fn c3e_metadata_is_invisible_to_equality_and_printing_of_a_lazy_seq() {
    // `values_equal` tested the WRAPPER for `Lazy`-ness, so a `Meta(Lazy)`
    // never got forced. Measured: all of these are `true` on the oracle.
    assert_eq!(ps("(= (range 10) (with-meta (range 10) {:a 1}))"), "true");
    assert_eq!(ps("(= (with-meta (range 10) {:a 1}) (range 10))"), "true");
    assert_eq!(ps("(= (with-meta (range 3) {:a 1}) '(0 1 2))"), "true");
    assert_eq!(ps("(= (with-meta (map inc [1 2]) {:a 1}) [2 3])"), "true");
    assert_eq!(
        ps("(= (with-meta (range 3) {:a 1}) (with-meta (range 3) {:b 2}))"),
        "true"
    );
    // Printing: `realize_deep` fell to its catch-all on a `Meta` wrapper,
    // so a metadata-carrying lazy seq printed as `#<lazy-seq>`.
    assert_eq!(ps("(pr-str (with-meta (range 3) {:a 1}))"), "\"(0 1 2)\"");
    assert_eq!(ps("(pr-str (with-meta [(range 3)] {:a 1}))"), "\"[(0 1 2)]\"");
    // ... and the metadata itself survives the realization.
    assert_eq!(ps("(meta (with-meta (range 3) {:a 1}))"), "{:a 1}");
}

#[test]
fn c3e_dot_equals_is_type_strict_where_clojure_equals_is_not() {
    // `.equals` is `Object.equals` (`Util.equals`), not `=`
    // (`Util.equiv`): class-strict at a numeric leaf, in a collection or
    // out of one. Measured oracle for every row.
    assert_eq!(ps("(.equals (seq [3]) (seq [3N]))"), "false");
    assert_eq!(ps("(.equals '(3) '(3N))"), "false");
    assert_eq!(ps("(.equals [3] [3.0])"), "false");
    assert_eq!(ps("(.equals 3 3N)"), "false");
    assert_eq!(ps("(.equals 3 (biginteger 3))"), "false");
    assert_eq!(ps("(.equals [3] [3])"), "true");
    assert_eq!(ps("(.equals [3N] [3N])"), "true");
    // Collection CLASSES stay lax -- only the elements are strict.
    assert_eq!(ps("(.equals [3] '(3))"), "true");
    assert_eq!(ps("(.equals [1 [2 3]] [1 [2 3]])"), "true");
    assert_eq!(ps("(.equals [1 [2 3N]] [1 [2 3]])"), "false");
    // Non-numeric leaves are unaffected.
    assert_eq!(ps("(.equals \"a\" \"a\")"), "true");
    assert_eq!(ps("(.equals [:a] [:a])"), "true");
    assert_eq!(ps("(.equals [3] nil)"), "false");
    // `=` itself, and `.equiv` (which IS `=`), stay loose.
    assert_eq!(ps("(= (seq [3]) (seq [3N]))"), "true");
    assert_eq!(ps("(.equiv (seq [3]) (seq [3N]))"), "true");
}

// -------------------- W-DECL: declare + binding-through-unbound --------------------
//
// Oracle facts measured against real Clojure 1.13.0-alpha6, transcripts
// under `compat/w-decl-*.txt`. session-11 (W-NS) had already found and
// ledgered this pair of defects; this wave fixes both:
//   1. `declare` (core/core.mova) is now literally 1-arg `def` in a loop
//      (real `clojure.core/declare`'s own shape), which only works because
//      `eval_def` (special_forms.rs) now gives a bare `(def name)` its
//      real meaning: intern-if-absent, NEVER touch an existing/fresh root
//      value -- not the old "always bind to `nil`".
//   2. `binding` resolves an interned-but-UNBOUND cell now too
//      (`Env::find_any_cell`, `resolve_binding_pairs`'s new `allow_unbound`
//      flag), and separately enforces real Clojure's `^:dynamic`
//      requirement (`check_dynamic_or_err`) for any cell that went
//      through `def`'s meta pipeline at all.

/// oracle: `compat/w-decl-oracle1.txt` "declare-then-def-then-use".
#[test]
fn w_decl_declare_then_def_then_use() {
    assert_eq!(ps("(declare y) (def y 42) y"), "42");
}

/// oracle: `compat/w-decl-oracle2.txt` "declare-then-binding dynamic" --
/// the exact vendored shape (`def.clj`'s `nested-dynamic-declaration`):
/// `(declare ^:dynamic p)` then ONLY EVER `binding`-ing `p` (no literal
/// `def` ever runs) must work.
#[test]
fn w_decl_binding_through_a_declared_unbound_dynamic_var_works() {
    assert_eq!(
        ps("(declare ^:dynamic p) (defn q [] @p) (binding [p (atom 10)] (q))"),
        "10"
    );
}

/// Same fact, bare (no defn/atom indirection): pins that `binding` alone
/// -- not just the vendored test's specific `defn`+`@`+`atom` shape --
/// resolves an unbound-but-dynamic cell.
#[test]
fn w_decl_binding_a_bare_declared_dynamic_var() {
    assert_eq!(ps("(declare ^:dynamic p2) (binding [p2 1] p2)"), "1");
}

/// oracle: `compat/w-decl-oracle2.txt` "declare-then-binding non-dynamic"
/// -- `(declare ndx) (binding [ndx 1] ndx)` => `java.lang.
/// IllegalStateException: Can't dynamically bind non-dynamic var:
/// user/ndx`. Message text and class both measured against the oracle.
#[test]
fn w_decl_binding_a_declared_non_dynamic_var_throws_illegal_state() {
    assert_eq!(
        caught_as("(declare ndx) (binding [ndx 1] ndx)", "IllegalStateException"),
        ":caught"
    );
    let err = eval_err("(declare ndx2) (binding [ndx2 1] ndx2)");
    assert!(
        err.message.starts_with("Can't dynamically bind non-dynamic var: ")
            && err.message.ends_with("ndx2"),
        "unexpected message: {}",
        err.message
    );
}

/// The non-`declare` half of the same oracle row: an ordinary, already
/// BOUND, non-dynamic `def` is refused by `binding` too (real Clojure
/// makes no exception for a var that already has a root value -- the
/// check is purely on `^:dynamic`, never on boundness).
#[test]
fn w_decl_binding_an_ordinary_bound_non_dynamic_var_throws() {
    assert_eq!(
        caught_as("(def bx 1) (binding [bx 2] bx)", "IllegalStateException"),
        ":caught"
    );
}

/// Regression guard for the one exemption `check_dynamic_or_err` carves
/// out: `*ns*` is wired up directly at boot (`ns.rs`), outside `def`'s
/// meta pipeline entirely, so it carries NO var meta at all (`Value::Nil`,
/// not an empty map) -- and stays bindable regardless, exactly as before
/// this task (this is what the whole vendored-suite runner's per-file
/// namespace-switching depends on, plus every vendored file that does
/// `(binding [*ns* ...] ...)` directly, e.g. `ns_libs.clj`/`evaluation.clj`).
#[test]
fn w_decl_binding_star_ns_star_is_still_unconditionally_permitted() {
    assert_eq!(ps("(binding [*ns* *ns*] :ok)"), ":ok");
}

/// oracle: `compat/w-decl-oracle2.txt` "doc/meta of declared var" --
/// `:declared true` always present, `^{:doc ...}` on the name flows
/// through to the var's `:doc`, exactly like an ordinary `def`'s `^{...}`
/// does (`eval_def`'s `publish_var_meta`, unchanged -- `declare`'s new
/// macro body just calls `def` with `vary-meta`-adjusted names, so it
/// gets this for free).
#[test]
fn w_decl_declared_var_meta_carries_declared_true_and_doc() {
    assert_eq!(ps("(declare zdoc) (:declared (meta #'zdoc))"), "true");
    assert_eq!(
        ps("(declare ^{:doc \"hi\"} zdoc2) (:doc (meta #'zdoc2))"),
        "\"hi\""
    );
}

/// oracle: `compat/w-decl-oracle2.txt` "is-dynamic-meta-after-declare" /
/// "plain declare no meta, is it dynamic?" -- `^:dynamic` on the
/// `declare`d name becomes real `:dynamic true` var meta (this is
/// EXACTLY what `binding`'s new dynamic check reads), and is absent
/// (`nil`, not `false`) for a plain declare.
#[test]
fn w_decl_declare_propagates_dynamic_meta_from_the_name_symbol() {
    assert_eq!(ps("(declare ^:dynamic *ddd*) (:dynamic (meta #'*ddd*))"), "true");
    assert_eq!(ps("(declare plain-v2) (:dynamic (meta #'plain-v2))"), "nil");
}

/// `with-redefs` is explicitly OUT of `resolve_binding_pairs`'s new
/// `allow_unbound` leniency (its own doc comment: "restoring an unbound
/// var's root via `raw_root()`/rebind is untested territory this fix
/// does not touch") -- pin that a declared-but-unbound var still can't
/// be `with-redefs`-ed, unchanged from before this task.
#[test]
fn w_decl_with_redefs_still_rejects_an_unbound_declared_var() {
    let err = eval_err("(declare wrx) (with-redefs [wrx 1] wrx)");
    assert_eq!(err.kind, ErrorKind::Unresolved);
}

/// W-DECL tree-walk/compiled-tier agreement (differential-style, in one
/// process): `Interp::with_compile_enabled(false)` forces every fn --
/// core.mova's included -- through the tree-walker only (see that
/// constructor's own doc), so running the identical program through it
/// and through the normal (compiled-tier-on) `Interp::new()` and
/// requiring the same answer is a real cross-tier check for this new
/// binding/declare semantics, not just two calls to the same code path.
#[test]
fn w_decl_binding_through_unbound_dynamic_var_agrees_across_tiers() {
    let src = "(declare ^:dynamic p3) (defn q3 [] @p3) (binding [p3 (atom 7)] (q3))";
    let mut walked = Interp::with_compile_enabled(false);
    let walked_result = walked
        .eval_str("test", src)
        .unwrap_or_else(|e| panic!("tree-walk eval error: {}", crate::error::render(&e, "test", src)));
    let mut compiled = Interp::new();
    let compiled_result = compiled
        .eval_str("test", src)
        .unwrap_or_else(|e| panic!("compiled-tier eval error: {}", crate::error::render(&e, "test", src)));
    assert_eq!(pr_str(&walked_result), pr_str(&compiled_result));
    assert_eq!(pr_str(&walked_result), "7");
}

/// Same cross-tier check for the non-dynamic rejection -- both tiers must
/// throw, with the same class.
#[test]
fn w_decl_binding_non_dynamic_rejection_agrees_across_tiers() {
    let src = "(declare ndx3) (binding [ndx3 1] ndx3)";
    let mut walked = Interp::with_compile_enabled(false);
    let walked_err = walked
        .eval_str("test", src)
        .expect_err("tree-walk: expected an error");
    let mut compiled = Interp::new();
    let compiled_err = compiled
        .eval_str("test", src)
        .expect_err("compiled tier: expected an error");
    assert_eq!(walked_err.jvm_class, Some(crate::error::JvmClass::IllegalState));
    assert_eq!(compiled_err.jvm_class, Some(crate::error::JvmClass::IllegalState));
}

// --- D12: macro expansion binds the dynamic `*ns*` to the expansion
// site's LEXICAL namespace (docs/SPEC-PORT-PATCHES.md item 12) ---------

/// D12, the ledger's repro. `f6`'s body contains a `binding`, so it BAILS
/// compilation (`compile::resolve`'s binding/with-redefs rule) and is
/// tree-walked, which means its `(marker)` call site is re-expanded on
/// every call rather than frozen once at `defn` time. `marker` records the
/// `*ns*` it expanded under; before the fix the define-time expansion saw
/// `my.lib` and every CALL-time expansion saw the caller's dynamic `*ns*`
/// (`user`), so `(resolve 'secret)` -- what `clojure.spec.alpha`'s `res`
/// does on every `s/&`/`s/coll-of`/`s/def` argument -- answered `nil`.
///
/// Oracle (Clojure 1.13.0-alpha6, this exact program): ONE expansion,
/// under `my.lib`, resolving `#'my.lib/secret`. So every entry of `seen`
/// must read `my.lib`.
#[test]
fn d12_tree_walked_reexpansion_sees_the_lexical_ns() {
    let src = r#"
        (ns my.lib)
        (def secret 7)
        (def seen (atom []))
        (defmacro marker []
          (swap! seen conj [(str *ns*) (pr-str (resolve 'secret))])
          42)
        (defn f6 [] (marker) (binding [*warn-on-reflection* false] 1))
        (in-ns 'user)
        (clojure.core/refer 'clojure.core)
        (my.lib/f6)
        (my.lib/f6)
        [(str *ns*) @my.lib/seen]
    "#;
    // The tree-walk is the point, but the compiled tier must agree: with
    // compilation off the SAME `defn` is tree-walked from the start.
    for mut interp in [Interp::new(), Interp::with_compile_enabled(false)] {
        let got = interp
            .eval_str("test", src)
            .unwrap_or_else(|e| panic!("eval error: {}", crate::error::render(&e, "test", src)));
        let rendered = pr_str(&got);
        assert!(
            rendered.starts_with("[\"user\" ["),
            "the caller's dynamic *ns* must still be `user`: {rendered}"
        );
        assert!(
            !rendered.contains("[\"user\" \"nil\"]"),
            "D12: a re-expansion resolved against the CALLER's *ns*: {rendered}"
        );
        assert!(
            rendered.contains("[\"my.lib\" \"#'my.lib/secret\"]"),
            "every expansion must see the lexical ns: {rendered}"
        );
        assert!(
            !rendered.contains("\"user\" \"#'"),
            "no expansion may run under `user`: {rendered}"
        );
    }
}

/// D12's fast path: when the dynamic `*ns*` already equals the lexical one
/// -- the ordinary case, since a file's top-level `(ns foo)` moves both --
/// `enter_expansion_ns` makes no swap at all, and the answer is unchanged.
#[test]
fn d12_expansion_ns_unchanged_when_dynamic_already_matches() {
    assert_eq!(
        ps("(ns q.lib) (defmacro m [] (str *ns*)) (defn g [] (m) (binding [*warn-on-reflection* false] (m))) (g)"),
        "\"q.lib\""
    );
}

/// D12: the restore is CONDITIONAL. Measured against the oracle (Clojure
/// 1.13.0-alpha6): a macro body that itself runs `(in-ns 'zzz)` at
/// expansion time leaves `*ns*` reading `zzz` afterwards -- the JVM's
/// `*ns*` binding is a per-LOAD bracket, not a per-expansion one -- while
/// the `def` whose compilation was already under way still interns into
/// `aaa`. `leave_expansion_ns` must therefore not clobber that move.
///
/// Oracle output for this program: `zzz`, `#'aaa/x`, `nil`.
#[test]
fn d12_in_ns_inside_a_macro_body_is_not_clobbered_by_the_restore() {
    assert_eq!(
        ps(r#"(ns aaa)
              (defmacro switcher [] (in-ns 'zzz) (clojure.core/refer 'clojure.core) 1)
              (def x (switcher))
              [(str *ns*) (pr-str (resolve 'aaa/x)) (pr-str (resolve 'zzz/x))]"#),
        "[\"zzz\" \"#'aaa/x\" \"nil\"]"
    );
}

/// D12: the bracket unwinds on the ERROR path too -- a macro that throws
/// during expansion must leave the dynamic `*ns*` exactly as it found it.
#[test]
fn d12_expansion_ns_restored_when_the_macro_throws() {
    let src = r#"
        (ns boom.lib)
        (defmacro kaboom [] (throw (ex-info "nope" {})))
        (defn f [] (kaboom) (binding [*warn-on-reflection* false] 1))
        (in-ns 'user)
        (clojure.core/refer 'clojure.core)
        (try (boom.lib/f) (catch Throwable _ :caught))
        (str *ns*)
    "#;
    let mut interp = Interp::new();
    let got = interp
        .eval_str("test", src)
        .unwrap_or_else(|e| panic!("eval error: {}", crate::error::render(&e, "test", src)));
    assert_eq!(pr_str(&got), "\"user\"");
}

/// clojure-lsp campaign (mova/PLAN.md): `.field`/`.method` interop must
/// see through a `Value::Meta` wrapper -- on the real JVM, attaching
/// metadata never changes an object's class, so `(.tag (with-meta rec
/// {...}))` dispatches exactly like the unwrapped record. Measured via
/// rewrite-clj's real parser (`rewrite_clj.node.seq/SeqNode`, whose
/// `tag` field name COLLIDES with the `rewrite-clj.node.protocols/Node`
/// protocol method it implements, `(tag [_node] tag)` -- the collision
/// itself was a red herring: the generated field-getter sugar,
/// `(.tag this)`, broke on ANY meta-wrapped record, colliding field
/// name or not) -- `read-with-meta` wraps every parsed node in row/col
/// metadata before any protocol method ever runs against it, so this
/// reproduces the exact failure mode with a minimal record instead of
/// the full vendored library.
#[test]
fn dot_interop_sees_through_with_meta_on_a_record() {
    // A record whose field name equals a protocol method it implements
    // -- the shape that actually appears in `rewrite-clj.node.seq/
    // SeqNode` -- exercises `wrap_fields_let`'s generated `(.tag this)`
    // getter, not just a bare user `.field` call.
    assert_eq!(
        ps(r#"(defprotocol Node (tag [node]))
              (defrecord R [tag] Node (tag [_node] tag))
              (def x (with-meta (->R :seq) {:row 1}))
              [(.tag x) (:tag x) (tag x)]"#),
        "[:seq :seq :seq]"
    );
    // A plain, non-colliding field name on a meta-wrapped record/deftype
    // -- confirms the fix is generic, not specific to the collision.
    assert_eq!(ps(r#"(defrecord Q [bar]) (.bar (with-meta (->Q 42) {:x 1}))"#), "42");
    assert_eq!(ps(r#"(deftype Q2 [bar]) (.bar (with-meta (->Q2 42) {:x 1}))"#), "42");
}

/// clojure-lsp campaign (mova/PLAN.md): 2-arg `(symbol ns name)` must
/// accept a `nil` `ns` -- real Clojure's own spelling of "no namespace"
/// (`(symbol nil "foo")` is the unqualified symbol `foo`, not an error).
/// `rewrite-clj.node.token/symbol-sexpr` relies on exactly this:
/// `(symbol (some-> ... str) (name value))` passes a bare `nil` `ns` for
/// every unqualified token it turns back into a symbol.
#[test]
fn symbol_two_arg_accepts_nil_namespace() {
    assert_eq!(ps(r#"(symbol nil "foo")"#), "foo");
    assert_eq!(ps(r#"(symbol "my.ns" "foo")"#), "my.ns/foo");
}

/// clojure-lsp campaign (mova/PLAN.md): `fn`/`defn`'s `{:pre [...] :post
/// [...]}` condition map -- previously unimplemented (silently evaluated
/// as an ordinary, discarded body form). `rewrite-clj.zip.removez/remove`
/// writes `{:pre [zloc] :post [%]}`, and bare `%` has no meaning outside
/// a `#(...)` literal, so evaluating the map as a plain body expression
/// threw "Unable to resolve symbol: %". Each condition is now checked
/// individually (a `:pre` violation before the body runs, a `:post`
/// violation after, with `%` bound to the body's own result), matching
/// real Clojure's own macroexpansion.
#[test]
fn fn_pre_post_conditions() {
    assert_eq!(ps("((fn [x] {:pre [(pos? x)]} (* x 2)) 5)"), "10");
    let e = eval_err("((fn [x] {:pre [(pos? x)]} (* x 2)) -5)");
    assert_eq!(e.kind, ErrorKind::Thrown, "{e:?}");
    assert_eq!(ps("((fn [x] {:post [(> % 0)]} (* x 2)) 5)"), "10");
    let e = eval_err("((fn [x] {:post [(> % 0)]} (* x -2)) 5)");
    assert_eq!(e.kind, ErrorKind::Thrown, "{e:?}");
}

/// clojure-lsp campaign (mova/PLAN.md): `some->`'s internal `nil?` check
/// must be namespace-qualified (`clojure.core/nil?`), matching the
/// convention `and`/`or`/`doseq`/`for` already use in `core.mova` for
/// their own hidden symbols -- otherwise a call site that locally shadows
/// `nil?` (`borkdude.rewrite-edn.impl/update*` does exactly this: `(let
/// [nil? (and ...)] ... (some-> x f) ...)`) captures the macro's internal
/// check and throws "boolean is not callable" the moment `some->` runs
/// inside that scope.
#[test]
fn some_arrow_nil_check_is_not_capturable_by_a_local_shadow() {
    assert_eq!(
        ps(r#"(let [nil? false] (some-> 5 inc))"#),
        "6"
    );
    assert_eq!(
        ps(r#"(let [nil? false] (some-> nil inc))"#),
        "nil"
    );
}

/// kondo-wave: `defrecord`/`deftype` implementing `Object` methods
/// (`toString`/`equals`/`hashCode`) directly in the body, alongside a
/// real protocol -- a common Clojure idiom (clj-kondo's vendored
/// rewrite-clj forks every one of its `Node` types this way, e.g.
/// `KeywordNode`: `Object (toString [this] ...)` right after its `Node`
/// protocol impl). Before `interface_name_of`'s fix (`eval::
/// types_forms`), `Object` fell through to `register_protocol_impls_
/// inline`, which requires a genuine protocol map and rejected `Object`
/// (a `ClassVal::Builtin`) with "interface java.lang.Object is not a
/// protocol" -- a load-time error that made every real-Clojure library
/// using this idiom entirely unloadable, not merely a wrong answer.
#[test]
fn defrecord_object_method_alongside_a_protocol_dispatches_via_dot_call() {
    assert_eq!(
        ps(r#"(defprotocol Node (tag [_]))
              (defrecord R [x]
                Node
                (tag [_] :r)
                Object
                (toString [this] (str "R:" (:x this))))
              [(tag (->R 5)) (.toString (->R 5))]"#),
        "[:r \"R:5\"]"
    );
}

/// kondo-wave: the same fix, on a plain `deftype` (no protocol impl at
/// all, `Object` the ONLY group) -- rules out the fix depending on some
/// OTHER group being present to populate `tdef.interfaces`.
#[test]
fn deftype_object_only_method_dispatches_via_dot_call() {
    assert_eq!(
        ps(r#"(deftype T [x]
                Object
                (toString [this] (str "T:" x)))
              (.toString (T. 7))"#),
        "\"T:7\""
    );
}

/// kondo-wave: `&env` -- real Clojure's OTHER implicit macro-arglist
/// param (alongside `&form`, `ns.rs`'s `resolve_symbol`). Only asserts
/// the narrow, honest partial stand-in this task adds: `&env` resolves
/// (doesn't error) inside a macro body and answers an empty map, so
/// `(:ns &env)` -- the one shape edamame's vendored `deftime` macro
/// needs -- is `nil`, matching real JVM Clojure (whose own `&env` never
/// carries an `:ns` key either, that's ClojureScript-analyzer-only).
#[test]
fn env_implicit_macro_param_resolves_to_an_empty_map() {
    assert_eq!(
        ps(r#"(defmacro probe [] (pr-str [&env (:ns &env) (map? &env)]))
              (probe)"#),
        "\"[{} nil true]\""
    );
}

/// kondo-wave: outside any macro expansion, `&env` stays unresolved --
/// same "exactly as unresolved as real Clojure outside a macro" contract
/// `&form` already has.
#[test]
fn env_implicit_macro_param_unresolved_outside_a_macro() {
    let err = eval_err("&env");
    assert!(
        matches!(err.kind, ErrorKind::Unresolved),
        "expected an unresolved-symbol error, got {err:?}"
    );
}

/// kondo-wave: `add-watch`/`remove-watch` are now REAL -- `swap!`/
/// `reset!`/`compare-and-set!` genuinely fire every registered watch
/// with `(f key reference old new)`, and `remove-watch` genuinely stops
/// further firing.
#[test]
fn add_watch_fires_on_swap_reset_and_compare_and_set() {
    assert_eq!(
        ps(r#"(def log (atom []))
              (def a (atom 0))
              (add-watch a :w (fn [k r old new] (swap! log conj [k old new])))
              (swap! a inc)
              (reset! a 10)
              (compare-and-set! a 10 20)
              (compare-and-set! a 999 30)
              @log"#),
        "[[:w 0 1] [:w 1 10] [:w 10 20]]"
    );
}

#[test]
fn remove_watch_stops_firing() {
    assert_eq!(
        ps(r#"(def log (atom []))
              (def a (atom 0))
              (add-watch a :w (fn [k r old new] (swap! log conj new)))
              (swap! a inc)
              (remove-watch a :w)
              (swap! a inc)
              @log"#),
        "[1]"
    );
}

// lsp/io (clj-kondo stdin campaign): `*in*`/`with-in-str`/`slurp`/
// `read-line` -- the veneer that lets clojure-lsp's `kondo.mova` go back
// to upstream's `(with-in-str text (kondo/run! {:lint ["-"] ...}))`
// instead of its tmp-file workaround. See `hostclass::HostKind::
// StringReader`'s doc for the design.

#[test]
fn with_in_str_slurp_in_returns_the_whole_string_byte_exact() {
    assert_eq!(
        ps(r#"(with-in-str "line1\nline2\nline3" (slurp *in*))"#),
        "\"line1\\nline2\\nline3\""
    );
}

#[test]
fn with_in_str_slurp_in_preserves_no_trailing_newline() {
    // Regression guard for the `BufferedReader`-style "reconstruct from
    // split lines" trap this deliberately avoids -- `slurp` must hand
    // back exactly what went in, not a `\n`-rejoined approximation.
    assert_eq!(ps(r#"(with-in-str "no-trailing-newline" (slurp *in*))"#), "\"no-trailing-newline\"");
    assert_eq!(ps(r#"(with-in-str "" (slurp *in*))"#), "\"\"");
}

#[test]
fn with_in_str_read_line_walks_lines_then_nil_at_eof() {
    assert_eq!(
        ps(r#"(with-in-str "aa\nbb\nno-nl"
              [(read-line) (read-line) (read-line) (read-line)])"#),
        "[\"aa\" \"bb\" \"no-nl\" nil]"
    );
}

#[test]
fn read_line_outside_with_in_str_is_unaffected() {
    // `*in*` defaults to `nil`; `(read-line)` must still fall through to
    // its pre-existing real-stdin path (unreachable in a test harness
    // with no stdin, so this only asserts *in*'s default doesn't get
    // mistaken for a bound StringReader -- an actual read attempt would
    // hang/EOF against the test process's own stdin, which is exactly
    // the pre-existing behavior this task must not change).
    assert_eq!(ps("*in*"), "nil");
}

#[test]
fn with_in_str_close_via_with_open_does_not_error() {
    // `with-in-str`'s macroexpansion is a `with-open` over the reader --
    // this exercises `.close` on a `HostKind::StringReader` actually
    // resolving (`call_string_reader_method`'s `"close"` arm), not just
    // the happy-path reads above.
    assert_eq!(ps(r#"(with-in-str "x" (slurp *in*))"#), "\"x\"");
}

#[test]
fn string_reader_class_and_instance_predicate() {
    assert_eq!(ps(r#"(class (java.io.StringReader. "x"))"#), "java.io.StringReader");
    assert_eq!(ps(r#"(instance? java.io.StringReader (java.io.StringReader. "x"))"#), "true");
    assert_eq!(ps(r#"(instance? java.io.StringReader "not-a-reader")"#), "false");
}

#[test]
fn persistent_list_create_static() {
    assert_eq!(ps("(clojure.lang.PersistentList/create [1 '(x/y) 3])"), "(1 (x/y) 3)");
    assert_eq!(ps("(list? (clojure.lang.PersistentList/create [1]))"), "true");
    assert_eq!(ps("(clojure.lang.PersistentList/create [])"), "()");
}

#[test]
fn str_of_string_writer_is_its_text() {
    assert_eq!(ps(r#"(let [w (java.io.StringWriter.)] (binding [*out* w] (print "ab")) (str w))"#), "\"ab\"");
    assert_eq!(ps("(str (java.io.StringWriter.))"), "\"\"");
}

#[test]
fn str_of_host_exception_is_throwable_to_string() {
    assert_eq!(ps(r#"(str (Exception. "boom"))"#), "\"java.lang.Exception: boom\"");
    assert_eq!(ps("(str (RuntimeException.))"), "\"java.lang.RuntimeException\"");
}

#[test]
fn random_access_file_channel_try_lock() {
    let dir = std::env::temp_dir().join(format!("mova-e2-lock-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("lock");
    let src = format!(
        r#"(with-open [raf (java.io.RandomAccessFile. "{p}" "rw") ch (.getChannel raf)]
             (let [l (try (.tryLock ch) (catch java.nio.channels.OverlappingFileLockException _ nil))
                   second (with-open [r2 (java.io.RandomAccessFile. (java.io.File. "{p}") "rw")] (.tryLock (.getChannel r2)))]
               (.release ^java.nio.channels.FileLock l)
               [(some? l) second (.isValid l) (let [l3 (.tryLock ch)] (.release l3) (some? l3))]))"#,
        p = f.display()
    );
    assert_eq!(ps(&src), "[true nil false true]");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn refer_clojure_rename_binds_new_name() {
    assert_eq!(
        ps("(ns e2.rename (:refer-clojure :rename {distinct? core-distinct?})) (defn- distinct? [x] (apply core-distinct? x)) [(distinct? [1 2]) (distinct? [1 1])]"),
        "[true false]"
    );
}

#[test]
fn java_net_url_connection_and_slurp_over_local_http() {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming().take(2) {
            let mut s = stream.unwrap();
            let mut buf = [0u8; 2048];
            let _ = s.read(&mut buf);
            let body = "{:a (x/y)}";
            let _ = write!(s, "HTTP/1.1 200 OK\r\nContent-Type: application/edn\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
        }
    });
    let src = format!(
        r#"(let [u (java.net.URL. "http://127.0.0.1:{port}/x") c (.openConnection u)]
             (.setConnectTimeout c 1000) (.setReadTimeout c 5000)
             [(str u) (.getResponseCode c) (.getContentType c) (slurp (.getInputStream c)) (slurp "http://127.0.0.1:{port}/y")
              (try (java.net.URL. "nope") (catch java.io.IOException e :malformed))])"#
    );
    assert_eq!(
        ps(&src),
        format!(r#"["http://127.0.0.1:{port}/x" 200 "application/edn" "{{:a (x/y)}}" "{{:a (x/y)}}" :malformed]"#)
    );
}
