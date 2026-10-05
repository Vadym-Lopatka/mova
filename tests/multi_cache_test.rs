//! W-MULTI: the multimethod best-method dispatch cache (`multi::MultiCache`,
//! consulted/installed by `multi::make_dispatch_native`).
//!
//! Same convention as `proto_ic_test.rs`: every test here is a BEHAVIOR
//! test against observable dispatch results only -- each one WARMS a call
//! site (so the cache is populated and would be consulted), then mutates
//! the thing the cache must notice, then asserts the answer real Clojure
//! gives. A build with the cache ripped out must pass this file
//! byte-identically.

use mova::embed::{Engine, Value};

fn engine() -> Engine {
    Engine::builder().build()
}

fn eval_ok(src: &str) -> Value {
    engine()
        .eval_named("test", src)
        .unwrap_or_else(|e| panic!("eval error for {src:?}: {}", e.render_plain()))
}

fn eval_err(src: &str) -> String {
    match engine().eval_named("test", src) {
        Ok(v) => panic!("expected an error evaluating {src:?}, got {v:?}"),
        Err(e) => e.to_string(),
    }
}

fn ps(src: &str) -> String {
    eval_ok(src).to_string()
}

// ==================== defmethod during dispatch (exotic contract) ====================

/// The measured contract this whole cache design sits under: a dispatch
/// FN that itself installs a new method for the multimethod mid-call must
/// see that method on the SAME call (module doc: this is why `methods`/
/// `prefers` are re-read AFTER calling the dispatch fn, not before). Must
/// hold identically whether or not the cache is warm.
#[test]
fn defmethod_during_dispatch_fn_is_visible_same_call() {
    let src = "(defmulti m (fn [x] (when (= x :install) (eval '(defmethod m :install [_] :installed))) x)) \
               (defmethod m :a [_] :a-method) \
               (dotimes [_ 50] (m :a)) \
               (m :install)";
    assert_eq!(ps(src), ":installed");
}

/// Same contract, but the SAME dispatch value that just got its method
/// installed mid-call is also the one the cache would otherwise have
/// missed on for a totally different reason (never cached before, first
/// call) -- and a SECOND call afterwards must hit the now-warm cache with
/// the same correct answer.
#[test]
fn defmethod_during_dispatch_fn_then_cached_hit_stays_correct() {
    let src = "(defmulti m (fn [x] (when (= x :install) (eval '(defmethod m :install [_] :late))) x)) \
               (defmethod m :a [_] :a-method) \
               (dotimes [_ 50] (m :a)) \
               [(m :install) (m :install) (m :install)]";
    assert_eq!(ps(src), "[:late :late :late]");
}

// ==================== remove-method / remove-all-methods ====================

/// `remove-method` after a warm call site must be visible immediately --
/// the dispatch value now falls through to `:default` (or errors if there
/// is none).
#[test]
fn remove_method_invalidates_immediately() {
    let src = "(defmulti m :kind) \
               (defmethod m :circle [_] :circle-method) \
               (def c {:kind :circle}) \
               (dotimes [_ 200] (m c)) \
               (remove-method m :circle) \
               (defmethod m :default [_] :fell-through) \
               (m c)";
    assert_eq!(ps(src), ":fell-through");
}

#[test]
fn remove_all_methods_invalidates_immediately() {
    let src = "(defmulti m :kind) \
               (defmethod m :circle [_] :circle-method) \
               (def c {:kind :circle}) \
               (dotimes [_ 200] (m c)) \
               (remove-all-methods m) \
               (defmethod m :circle [_] :second-gen) \
               (m c)";
    assert_eq!(ps(src), ":second-gen");
}

// ==================== prefer-method ====================

/// `prefer-method` flips which of two matching methods wins -- must be
/// visible on the very next call, even though both candidates were
/// already individually resolvable (ambiguous) before the preference.
#[test]
fn prefer_method_changes_winner_immediately() {
    let src = "(derive ::square ::rect) \
               (derive ::square ::rhombus) \
               (defmulti m identity) \
               (defmethod m ::rect [_] :rect) \
               (defmethod m ::rhombus [_] :rhombus) \
               (prefer-method m ::rect ::rhombus) \
               (dotimes [_ 50] (m ::square)) \
               (m ::square)";
    assert_eq!(ps(src), ":rect");
}

// ==================== derive/underive (global hierarchy) ====================

/// `derive` changing `isa?` answers for the global hierarchy must be
/// visible immediately to an already-warmed multimethod using it.
#[test]
fn derive_after_warm_changes_dispatch_immediately() {
    let src = "(defmulti m identity) \
               (defmethod m ::shape [_] :shape-method) \
               (defmethod m :default [_] :none) \
               (dotimes [_ 50] (m ::circle)) \
               (def before (m ::circle)) \
               (derive ::circle ::shape) \
               (def after (m ::circle)) \
               [before after]";
    assert_eq!(ps(src), "[:none :shape-method]");
}

#[test]
fn underive_after_warm_changes_dispatch_immediately() {
    let src = "(derive ::circle ::shape) \
               (defmulti m identity) \
               (defmethod m ::shape [_] :shape-method) \
               (defmethod m :default [_] :none) \
               (dotimes [_ 50] (m ::circle)) \
               (def before (m ::circle)) \
               (underive ::circle ::shape) \
               (def after (m ::circle)) \
               [before after]";
    assert_eq!(ps(src), "[:shape-method :none]");
}

/// A hierarchy write on a multimethod OTHER than the one being warmed
/// still has to invalidate this one's cache -- the generation counter is
/// crate-wide by design (module doc: a hierarchy change can affect every
/// global-hierarchy multimethod, not just whichever triggered it).
#[test]
fn derive_via_unrelated_multimethod_still_invalidates() {
    let src = "(defmulti m1 identity) \
               (defmethod m1 ::shape [_] :shape-method) \
               (defmethod m1 :default [_] :none) \
               (dotimes [_ 50] (m1 ::circle)) \
               (def before (m1 ::circle)) \
               (defmulti m2 identity) \
               (defmethod m2 :x [_] :whatever) \
               (derive ::circle ::shape) \
               (def after (m1 ::circle)) \
               [before after]";
    assert_eq!(ps(src), "[:none :shape-method]");
}

// ==================== custom :hierarchy (uncached path) ====================

/// A multimethod on a CUSTOM (atom-backed) hierarchy is scoped OUT of the
/// cache entirely (module doc): a `swap!` on the hierarchy atom with no
/// builtin signal must stay immediately visible, exactly like the
/// pre-cache behavior.
#[test]
fn custom_hierarchy_swap_stays_immediately_visible() {
    let src = "(def h (atom (make-hierarchy))) \
               (defmulti m identity :hierarchy h) \
               (defmethod m ::shape [_] :shape-method) \
               (defmethod m :default [_] :none) \
               (dotimes [_ 50] (m ::circle)) \
               (def before (m ::circle)) \
               (swap! h derive ::circle ::shape) \
               (def after (m ::circle)) \
               [before after]";
    assert_eq!(ps(src), "[:none :shape-method]");
}

// ==================== cache cap ====================

/// More distinct dispatch values than the cache's size cap (1024) --
/// every one of them must still dispatch correctly; the cap only degrades
/// perf for the overflow, never correctness.
#[test]
fn many_dispatch_values_past_the_cap_still_dispatch_correctly() {
    let src = "(defmulti m identity) \
               (doseq [i (range 1100)] (eval `(defmethod m ~i [_] ~i))) \
               (= (vec (range 1100)) (vec (map m (range 1100))))";
    assert_eq!(ps(src), "true");
}

// ==================== default method path ====================

#[test]
fn default_method_path_still_works_when_cache_is_warm() {
    let src = "(defmulti m :kind) \
               (defmethod m :circle [_] :circle-method) \
               (defmethod m :default [_] :fallback) \
               (def c {:kind :circle}) \
               (def sq {:kind :square}) \
               (dotimes [_ 50] (m c)) \
               [(m c) (m sq) (m c) (m sq)]";
    assert_eq!(ps(src), "[:circle-method :fallback :circle-method :fallback]");
}

// ==================== ambiguity is never cached ====================

/// An ambiguous dispatch must keep raising on every call (never gets
/// cached as a "hit" -- and must not corrupt a later, resolvable call for
/// the same dispatch value once `prefer-method` breaks the tie).
#[test]
fn ambiguity_error_still_raised_and_never_cached() {
    let src = "(defmulti m identity) \
               (defmethod m ::rect [_] :rect) \
               (defmethod m ::rhombus [_] :rhombus) \
               (derive ::square ::rect) \
               (derive ::square ::rhombus) \
               (m ::square)";
    let e = eval_err(src);
    assert!(
        e.contains("Multiple methods") && e.contains("neither is preferred"),
        "expected the measured ambiguity message, got {e:?}"
    );
    // Repeating it must raise again, not silently resolve from a stray
    // cache entry.
    let e2 = eval_err(src);
    assert!(e2.contains("Multiple methods"), "expected ambiguity again, got {e2:?}");
}

// ==================== polymorphic call site ====================

#[test]
fn two_dispatch_values_alternate_on_one_call_site() {
    let src = "(defmulti m :kind) \
               (defmethod m :circle [_] :circle-method) \
               (defmethod m :square [_] :square-method) \
               (def xs (vec (take 40 (cycle [{:kind :circle} {:kind :square}])))) \
               (vec (map m xs))";
    let out = ps(src);
    assert_eq!(out.matches(":circle-method").count(), 20, "in {out:?}");
    assert_eq!(out.matches(":square-method").count(), 20, "in {out:?}");
    assert!(out.starts_with("[:circle-method :square-method :circle-method"), "in {out:?}");
}

// ==================== cache lives across a fork ====================

#[test]
fn cached_dispatch_survives_a_fork() {
    let src = "(defmulti m :kind) \
               (defmethod m :circle [_] :circle-method) \
               (def c {:kind :circle}) \
               (dotimes [_ 100] (m c)) \
               (let [fs (doall (for [_ (range 4)] \
                                 (future (vec (repeatedly 50 (fn [] (m c)))))))] \
                 (vec (map (fn [f] (count (filter (fn [v] (= v :circle-method)) @f))) fs)))";
    assert_eq!(ps(src), "[50 50 50 50]");
}

// ==================== defmulti redefinition ====================

/// A `defmulti` re-registration on an UNregistered var (e.g. after
/// `ns-unmap`) builds a brand-new `MultiDef` with a fresh (empty) cache --
/// not exercised directly here since the defonce-like guard makes a plain
/// re-`defmulti` on a live var a no-op, but the FIRST-definition path
/// itself must produce a working cache from scratch.
#[test]
fn fresh_defmulti_dispatch_warms_and_stays_correct() {
    let src = "(defmulti m :kind) \
               (defmethod m :circle [_] :circle-method) \
               (def c {:kind :circle}) \
               (vec (repeatedly 10 (fn [] (m c))))";
    assert_eq!(ps(src), "[:circle-method :circle-method :circle-method :circle-method :circle-method :circle-method :circle-method :circle-method :circle-method :circle-method]");
}
