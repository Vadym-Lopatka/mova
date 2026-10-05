//! fix/closure-env-cycles: the `letfn` shape compiles as a RECURSIVE
//! BINDING GROUP (`Ir::MakeRecGroup` + `Ir::SiblingRef`) instead of bailing
//! the whole enclosing fn to the tree-walker.
//!
//! Two things are asserted, and they need each other:
//!
//! - **semantics**, against the tree-walker's answers -- forward references,
//!   backward references, a sibling that ESCAPES its group as a value, and
//!   sibling IDENTITY (which is what the group's weak member cache exists
//!   for);
//! - **shape**, because falling back is always *correct*: without a direct
//!   node-shape assertion every semantic test here would pass just as well
//!   with the feature switched off, for the wrong reason. That is the same
//!   discipline `compile::tests`' intrinsic/NumLoop shape tests use.
//!
//! The leak half of the claim lives in `tests/leak_cycle_probe.rs`
//! (`compiled_letfn_in_defn_no_leak_no_frames`, `escaped_letfn_closure_no_leak`),
//! which needs the `leak-probe` feature and its process-global counters.

use mova::internal::compile::ir::Ir;
use mova::internal::{Interp, Value};

/// Evaluates `src` and returns the value, panicking with the error message.
fn eval(src: &str) -> Value {
    let mut interp = Interp::new();
    interp
        .eval_str("rec-group-test", src)
        .unwrap_or_else(|e| panic!("{src}: {}", e.message))
}

/// The same program run on BOTH tiers, asserting they agree -- the
/// tree-walker is the oracle for every semantic claim in this file.
fn both_tiers(src: &str) -> Value {
    let compiled = {
        let mut interp = Interp::with_compile_enabled(true);
        interp
            .eval_str("rec-group-test", src)
            .unwrap_or_else(|e| panic!("compiled {src}: {}", e.message))
    };
    let walked = {
        let mut interp = Interp::with_compile_enabled(false);
        interp
            .eval_str("rec-group-test", src)
            .unwrap_or_else(|e| panic!("tree-walked {src}: {}", e.message))
    };
    assert_eq!(
        format!("{compiled:?}"),
        format!("{walked:?}"),
        "tiers disagree on: {src}"
    );
    compiled
}

fn as_bool(v: &Value) -> bool {
    match v {
        Value::Bool(b) => *b,
        other => panic!("expected a bool, got {other:?}"),
    }
}

fn as_int(v: &Value) -> i64 {
    match v {
        Value::Int(i) => *i,
        other => panic!("expected an int, got {other:?}"),
    }
}

// ---------------------------------------------------------------------
// Semantics
// ---------------------------------------------------------------------

const MUTUAL: &str = "(letfn [(even?* [n] (if (zero? n) true (odd?* (dec n)))) \
                             (odd?* [n] (if (zero? n) false (even?* (dec n))))] \
                        (even?* ";

#[test]
fn mutual_recursion_answers_the_tree_walkers_answer() {
    assert!(as_bool(&both_tiers(&format!("{MUTUAL}10))"))));
    assert!(!as_bool(&both_tiers(&format!("{MUTUAL}11))"))));
    assert!(as_bool(&both_tiers(&format!("{MUTUAL}0))"))));
}

/// The same thing inside a `defn`, which is what makes the enclosing fn
/// COMPILED rather than a top-level tree-walked form.
#[test]
fn mutual_recursion_inside_a_compiled_fn() {
    let v = both_tiers(
        "(do (defn lf [n] (letfn [(e [k] (if (zero? k) true (o (dec k)))) \
                                  (o [k] (if (zero? k) false (e (dec k))))] \
                            (e n))) \
             [(lf 10) (lf 11)])",
    );
    assert_eq!(format!("{v:?}"), format!("{:?}", eval("[true false]")));
}

/// A BACKWARD sibling read: `b` names `a`, which is bound EARLIER in the
/// same run. This is the cycle trap -- resolving it to the earlier binding's
/// slot would make it a by-value capture, and a by-value sibling capture in
/// either direction just relocates the `Arc` cycle. It must be a
/// `SiblingRef` like the forward one.
#[test]
fn backward_sibling_reads_resolve_through_the_group() {
    assert_eq!(as_int(&both_tiers("(letfn [(a [] 1) (b [] (a))] (b))")), 1);
    assert_eq!(
        as_int(&both_tiers(
            "(do (defn f [] (letfn [(a [] 1) (b [] (a))] (b))) (f))"
        )),
        1
    );
}

/// A member ESCAPES its group as a value and is called after the `let` that
/// built it has returned. The group is what keeps its siblings reachable --
/// there is no live frame to defer to.
#[test]
fn an_escaped_member_still_reaches_its_siblings() {
    assert!(as_bool(&both_tiers(
        "(let [f (letfn [(e [n] (if (zero? n) true (o (dec n)))) \
                         (o [n] (if (zero? n) false (e (dec n))))] \
                   e)] \
           (f 4))"
    )));
    // ... and from inside a compiled fn, twice over, so the group is built
    // fresh on every call.
    assert!(as_bool(&both_tiers(
        "(do (defn mk [] (letfn [(e [n] (if (zero? n) true (o (dec n)))) \
                                 (o [n] (if (zero? n) false (e (dec n))))] \
                           e)) \
             (and ((mk) 4) ((mk) 8)))"
    )));
}

/// Sibling IDENTITY is stable while any handle is live: two reads of the
/// same sibling answer the same object, exactly as the tree-walker's two
/// reads of one frame binding do.
#[test]
fn repeated_sibling_reads_are_identical() {
    assert!(as_bool(&both_tiers(
        "(letfn [(e [] o) (o [] 1)] (identical? (e) (e)))"
    )));
    assert!(as_bool(&both_tiers(
        "(do (defn f [] (letfn [(e [] o) (o [] 1)] (identical? (e) (e)))) (f))"
    )));
}

/// A member's own `fn` self-name still wins over the sibling binder of the
/// same spelling (innermost binder shadows), and so does a param or an inner
/// `let`.
#[test]
fn closer_binders_shadow_the_sibling_names() {
    // `letfn` names every sibling `fn` after its binding, so `e` inside `e`
    // is the SELF-name, not the sibling -- same closure either way, but this
    // pins that the self-name path is what runs.
    assert_eq!(
        as_int(&both_tiers(
            "(letfn [(e [n] (if (zero? n) 0 (e (dec n)))) (o [] (e 3))] (o))"
        )),
        0
    );
    // A param shadows a sibling name.
    assert_eq!(
        as_int(&both_tiers("(letfn [(e [o] o) (o [] 99)] (e 7))")),
        7
    );
    // An inner `let` shadows a sibling name.
    assert_eq!(
        as_int(&both_tiers("(letfn [(e [] (let [o 5] o)) (o [] 99)] (e))")),
        5
    );
}

/// A fn nested INSIDE a member that mentions a sibling: the nested closure's
/// own `l.me` is not a group member, so this goes through
/// `CaptureSrc::Sibling` -- the same read taken one frame out.
#[test]
fn a_fn_nested_inside_a_member_reaches_the_siblings() {
    assert_eq!(
        as_int(&both_tiers(
            "(letfn [(e [] ((fn [] (o)))) (o [] 42)] (e))"
        )),
        42
    );
    // Two levels deep, so the second level is an ordinary `Capture` of the
    // first level's sibling capture.
    assert_eq!(
        as_int(&both_tiers(
            "(letfn [(e [] ((fn [] ((fn [] (o)))))) (o [] 7)] (e))"
        )),
        7
    );
    // Through `map`, which is the everyday spelling of the same shape.
    assert_eq!(
        format!("{:?}", both_tiers(
            "(letfn [(e [xs] (vec (map (fn [x] (o x)) xs))) (o [x] (inc x))] (e [1 2 3]))"
        )),
        format!("{:?}", eval("[2 3 4]"))
    );
}

/// Members with outer captures: the group's per-member snapshot is taken out
/// of the creating frame exactly like `MakeClosure`'s.
#[test]
fn members_capture_enclosing_locals_by_value() {
    assert_eq!(
        as_int(&both_tiers(
            "(do (defn f [k] (letfn [(e [n] (if (zero? n) k (o (dec n)))) \
                                     (o [n] (if (zero? n) (- k) (e (dec n))))] \
                               (e 3))) (f 5))"
        )),
        -5
    );
}

/// Nested `letfn`s: the inner run recurses into its own detection, and the
/// inner members still reach the OUTER run's names (which crosses the outer
/// member's frame as a `CaptureSrc::Sibling`).
#[test]
fn a_letfn_nested_inside_a_member_still_sees_both_runs() {
    assert_eq!(
        as_int(&both_tiers(
            "(letfn [(e [n] (letfn [(a [k] (if (zero? k) (o) (b (dec k)))) \
                                    (b [k] (a k))] \
                              (a n))) \
                     (o [] 11)] \
               (e 2))"
        )),
        11
    );
}

/// Multi-arity and variadic members: the sibling binder must be visible in
/// EVERY arity (which is why it is not a `scopes` entry -- `compile_arity`
/// resets those per arity).
#[test]
fn sibling_names_are_visible_in_every_arity() {
    assert_eq!(
        as_int(&both_tiers(
            "(letfn [(e ([] (e 3)) ([n] (if (zero? n) 0 (o (dec n))))) \
                     (o [n] (e n))] \
               (e))"
        )),
        0
    );
    assert_eq!(
        as_int(&both_tiers(
            "(letfn [(e [& xs] (o (count xs))) (o [n] n)] (e 1 2 3))"
        )),
        3
    );
}

/// The rebind-drift refusal: a run name rebound LATER in the same vector
/// would make `SiblingRef` keep answering the ORIGINAL member while the
/// tree-walker's one mutable `let` frame answers the later one. Whichever
/// tier runs it, the answer must be the tree-walker's.
#[test]
fn a_rebound_member_name_agrees_with_the_tree_walker() {
    assert_eq!(
        as_int(&both_tiers("(letfn* [e (fn [] (o)) o (fn [] 1) o (fn [] 2)] (e))")),
        2
    );
    assert_eq!(
        as_int(&both_tiers(
            "(do (defn f [] (letfn* [e (fn [] (o)) o (fn [] 1) o (fn [] 2)] (e))) (f))"
        )),
        2
    );
}

/// A run whose members are not consecutive stays on the old path (and the
/// old path's answer is the tree-walker's, whichever tier ends up running).
#[test]
fn a_non_consecutive_run_still_agrees() {
    assert_eq!(
        as_int(&both_tiers(
            "(do (defn f [] (letfn* [e (fn [] (o)) x 5 o (fn [] x)] (e))) (f))"
        )),
        5
    );
}

/// `loop` is deliberately excluded from grouping (a group collapses N
/// bindings into one entry, and a `loop`'s binding count is its `recur`
/// arity). It must still answer what the tree-walker answers.
#[test]
fn a_letfn_shaped_loop_vector_still_agrees() {
    assert_eq!(
        as_int(&both_tiers(
            "(loop [e (fn [] (o)) o (fn [] 3)] (e))"
        )),
        3
    );
}

// ---------------------------------------------------------------------
// Shape: the feature is actually on
// ---------------------------------------------------------------------

/// Whether `src`'s last form (a fn) compiled, and whether its code contains
/// an `Ir::MakeRecGroup` anywhere.
fn group_shape(src: &str) -> (bool, bool) {
    let mut interp = Interp::new();
    // Lazy tier-up: this asserts a def-time compile outcome without ever
    // calling the fn, so force eager compilation.
    interp.set_eager_compile(true);
    let v = interp
        .eval_str("rec-group-test", src)
        .unwrap_or_else(|e| panic!("{src}: {}", e.message));
    let Value::Fn(rc) = v else {
        panic!("{src}: expected a fn")
    };
    let Some(cc) = rc.compiled.compiled() else {
        return (false, false);
    };
    let found = cc
        .code
        .arities
        .iter()
        .any(|a| a.body.iter().any(has_rec_group));
    (true, found)
}

/// Deliberately a `_ => false` walker rather than an exhaustive match: this
/// is a NET over the feature, not a classification of the node set, and an
/// unrelated new `Ir` variant should not fail this file to compile.
fn has_rec_group(ir: &Ir) -> bool {
    fn any(irs: &[Ir]) -> bool {
        irs.iter().any(has_rec_group)
    }
    match ir {
        Ir::MakeRecGroup { .. } => true,
        Ir::If { test, then, els } => {
            has_rec_group(test) || has_rec_group(then) || els.as_deref().is_some_and(has_rec_group)
        }
        Ir::Do(irs) | Ir::VectorLit(irs) | Ir::SetLit(irs) => any(irs),
        Ir::Let { binds, body } | Ir::Loop { binds, body, .. } => {
            binds.iter().any(|(_, i)| has_rec_group(i)) || any(body)
        }
        Ir::Recur { args, .. }
        | Ir::CallGlobal { args, .. }
        | Ir::CallCreationEnv { args, .. }
        | Ir::Intrinsic { args, .. } => any(args),
        Ir::Call { callee, args, .. } => has_rec_group(callee) || any(args),
        Ir::MapLit(kvs) => kvs.iter().any(|(k, v)| has_rec_group(k) || has_rec_group(v)),
        Ir::Throw { value, .. } => has_rec_group(value),
        Ir::Try {
            body,
            catches,
            finally,
        } => {
            any(body)
                || catches.iter().any(|arm| any(&arm.body))
                || finally.as_ref().is_some_and(|b| any(b))
        }
        Ir::Def { value, .. } => value.as_deref().is_some_and(has_rec_group),
        Ir::MakeClosure { template, .. } => template.code.arities.iter().any(|a| any(&a.body)),
        Ir::NumLoop(nl) => has_rec_group(&nl.fallback),
        _ => false,
    }
}

/// THE shape claim: the fn that holds a `letfn` now COMPILES, and holds a
/// recursive binding group. Before this landed, every one of these bailed
/// the whole enclosing fn (`resolve::POISON_BAIL`).
#[test]
fn the_letfn_shape_compiles_as_a_recursive_binding_group() {
    for src in [
        // The flagship: mutual recursion inside a compiled fn.
        "(fn [] (letfn [(e [n] (if (zero? n) true (o (dec n)))) \
                        (o [n] (if (zero? n) false (e (dec n))))] \
                  (e 10)))",
        // A member escaping as the value.
        "(fn [] (letfn [(e [n] (if (zero? n) true (o (dec n)))) \
                        (o [n] (if (zero? n) false (e (dec n))))] \
                  e))",
        // A backward-only reference is still a group.
        "(fn [] (letfn [(a [] 1) (b [] (a))] (b)))",
        // Direct `letfn*`, no macro involved.
        "(fn [] (letfn* [e (fn [] (o)) o (fn [] 1)] (e)))",
        // A run embedded among ordinary bindings.
        "(fn [k] (letfn* [x (inc k) e (fn [] (o)) o (fn [] x) y 2] [(e) y]))",
        // Three members.
        "(fn [] (letfn [(a [] (b)) (b [] (c)) (c [] 1)] (a)))",
        // A sibling read from a fn nested inside a member.
        "(fn [xs] (letfn [(e [ys] (map (fn [y] (o y)) ys)) (o [y] y)] (e xs)))",
    ] {
        let (compiled, grouped) = group_shape(src);
        assert!(compiled, "expected the tier to compile: {src}");
        assert!(grouped, "expected an Ir::MakeRecGroup in: {src}");
    }
}

/// The refusals, each for its own stated reason. None of these may compile
/// into a group -- the first three must not compile at all (they keep
/// today's whole-fn bail), the last two compile without one.
#[test]
fn the_refused_shapes_do_not_become_groups() {
    // Rebind drift: a run name rebound later in the same vector.
    assert_eq!(
        group_shape("(fn [] (letfn* [e (fn [] (o)) o (fn [] 1) o (fn [] 2)] (e)))"),
        (false, false)
    );
    // Non-consecutive: an ordinary binding splits the run.
    assert_eq!(
        group_shape("(fn [] (letfn* [e (fn [] (o)) x 5 o (fn [] x)] (e)))"),
        (false, false)
    );
    // An escaped interop form mentioning a sibling.
    assert_eq!(
        group_shape("(fn [s] (letfn [(e [] (.toUpperCase (str o))) (o [] (e))] (e)))"),
        (false, false)
    );
    // A run of ONE is not a run.
    let (compiled, grouped) = group_shape("(fn [] (let [e (fn [] 1)] (e)))");
    assert!(compiled && !grouped, "a single fn binding is not a group");
    // Consecutive fn bindings that never mention each other are not mutual.
    let (compiled, grouped) = group_shape("(fn [] (let [e (fn [] 1) o (fn [] 2)] [(e) (o)]))");
    assert!(
        compiled && !grouped,
        "non-mutual consecutive fn bindings must not be grouped"
    );
}
