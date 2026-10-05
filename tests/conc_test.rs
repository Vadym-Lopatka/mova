//! Integration tests for v0.2 / A1's concurrency substrate: `builtins::conc`
//! (`future*`/`promise`/`deliver`/`delay*`/`force`/`sleep-ms`), the
//! `future`/`delay` core.mova macros, `deref`'s future/promise/delay
//! extension in `builtins::atoms`, and `swap!`'s CAS-retry loop under real
//! concurrent load.
//!
//! Timing-sensitive assertions use generous margins (10x+ headroom) against
//! `time-ms`/`sleep-ms` so they're not flaky on a loaded CI box.

use mova::embed::{Engine, Value, ValueKind};

fn engine() -> Engine {
    Engine::builder().build()
}

fn eval_ok(src: &str) -> Value {
    engine()
        .eval_named("test", src)
        .unwrap_or_else(|e| panic!("eval error for {src:?}: {}", e.render_plain()))
}

fn ps(src: &str) -> String {
    eval_ok(src).to_string()
}

fn as_int(v: &Value) -> i64 {
    v.as_i64().unwrap_or_else(|| panic!("expected an int, got {v:?}"))
}

// -------------------- future: parallelism + result propagation --------------------

/// Two `(sleep-ms 100)` futures run genuinely in parallel (real OS threads),
/// so deref'ing both back-to-back costs ~100ms total wall time, not ~200ms.
/// Generous 180ms ceiling (vs. a theoretical ~100ms) to absorb scheduling
/// jitter on a loaded CI box without ever tolerating serialized (~200ms)
/// execution.
#[test]
fn futures_run_in_parallel_not_serially() {
    let result = eval_ok(
        r#"
        (let [t0 (time-ms)
              f1 (future (sleep-ms 100) 1)
              f2 (future (sleep-ms 100) 2)
              r1 (deref f1)
              r2 (deref f2)
              elapsed (- (time-ms) t0)]
          [r1 r2 elapsed])
        "#,
    );
    assert_eq!(result.kind(), ValueKind::Vector, "expected a vector");
    let items: Vec<Value> = result.iter().collect();
    assert_eq!(as_int(&items[0]), 1);
    assert_eq!(as_int(&items[1]), 2);
    let elapsed = as_int(&items[2]);
    assert!(elapsed < 180, "expected ~100ms parallel execution, took {elapsed}ms");
}

#[test]
fn future_deref_returns_normal_result() {
    assert_eq!(ps("(deref (future (+ 1 2)))"), "3");
}

/// A future whose body `throw`s must re-raise that same thrown value on
/// `deref`, catchable exactly like a direct (non-threaded) call would be --
/// not wrapped, not swallowed.
#[test]
fn future_propagates_thrown_error_catchable() {
    assert_eq!(ps("(try (deref (future (throw :boom))) (catch e e))"), ":boom");
}

#[test]
fn future_capturing_shared_atom() {
    assert_eq!(
        ps("(let [a (atom 0)] (deref (future (reset! a 99) (deref a))))"),
        "99"
    );
}

// -------------------- promise --------------------

#[test]
fn promise_deliver_deref_across_threads() {
    assert_eq!(
        ps("(let [p (promise) f (future (sleep-ms 30) (deliver p 42))] (deref p))"),
        "42"
    );
}

/// A second `deliver` on an already-delivered promise is a documented
/// no-op (Clojure semantics): it doesn't overwrite the first value.
#[test]
fn deliver_second_time_is_noop() {
    assert_eq!(ps("(let [p (promise)] (deliver p 1) (deliver p 2) (deref p))"), "1");
}

// -------------------- deref with timeout --------------------

#[test]
fn deref_timeout_returns_default_when_not_ready() {
    // The future sleeps 300ms; a 40ms timeout must fire well before it
    // finishes and hand back the default, not block.
    assert_eq!(ps("(deref (future (sleep-ms 300) :done) 40 :timed-out)"), ":timed-out");
}

#[test]
fn deref_timeout_returns_value_when_ready_in_time() {
    // A near-instant future finishes well inside a 2000ms timeout budget.
    assert_eq!(ps("(deref (future :immediate) 2000 :timed-out)"), ":immediate");
}

// -------------------- delay --------------------

#[test]
fn delay_forces_exactly_once() {
    assert_eq!(
        ps(
            r#"(let [counter (atom 0)
                     d (delay (swap! counter inc) :computed)]
                 (force d)
                 (force d)
                 (deref d)
                 (deref counter))"#
        ),
        "1"
    );
}

#[test]
fn delay_force_and_deref_agree() {
    assert_eq!(ps("(let [d (delay 5)] (= (force d) (deref d) 5))"), "true");
}

// -------------------- predicates --------------------

#[test]
fn future_delay_promise_predicates() {
    assert_eq!(
        ps(
            r#"(let [f (future 1) d (delay 1) p (promise) a (atom 1)]
                 [(future? f) (delay? d) (promise? p)
                  (future? d) (delay? p) (promise? a)])"#
        ),
        "[true true true false false false]"
    );
}

#[test]
fn realized_predicate_across_kinds() {
    assert_eq!(
        ps(
            r#"(let [d (delay 42)
                     p (promise)]
                 [(realized? d) (realized? p)
                  (do (force d) (realized? d))
                  (do (deliver p 1) (realized? p))])"#
        ),
        "[false false true true]"
    );
}

// -------------------- sleep-ms --------------------

#[test]
fn sleep_ms_blocks_approximately_the_requested_duration() {
    let elapsed = as_int(&eval_ok("(let [t0 (time-ms)] (sleep-ms 30) (- (time-ms) t0))"));
    assert!((20..1000).contains(&elapsed), "sleep-ms 30 took {elapsed}ms");
}

// -------------------- swap! CAS correctness under real concurrency --------------------

/// The load-bearing correctness test for the CAS-retry design: 8 real OS
/// threads each doing 1000 `swap! inc` on ONE shared atom must land exactly
/// 8000 increments with none lost to a racy read-modify-write. A naive
/// "lock, read, compute, write" swap! done *without* care would still pass
/// this (since the whole read-compute-write is one critical section) -- the
/// real risk this guards against is a regression to a design that reads the
/// value *outside* the lock and writes it back without re-checking (a
/// classic lost-update bug), which the version-counter CAS-retry in
/// `builtins::atoms::register`'s `swap!` is specifically built to avoid.
#[test]
fn eight_threads_1000_swaps_each_land_exact_count() {
    assert_eq!(
        ps(
            r#"(let [counter (atom 0)
                     futures (doall (map (fn [_] (future (dotimes [_ 1000] (swap! counter inc)))) (range 8)))]
                 (doall (map deref futures))
                 (deref counter))"#
        ),
        "8000"
    );
}

/// `swap!`'s update fn may itself `deref` (or, more subtly, `swap!`) other
/// state -- including the SAME atom it's updating, a realistic pattern for
/// e.g. a fn that reads other bindings while computing. Because the
/// CAS-retry loop never holds the atom's lock across the call to `f` (see
/// that fn's doc comment), a same-atom reentrant `deref` inside the update
/// fn must NOT deadlock.
#[test]
fn swap_update_fn_can_reenter_deref_same_atom_without_deadlock() {
    assert_eq!(ps("(let [a (atom 0)] (swap! a (fn [x] (+ x (deref a)))) (deref a))"), "0");
}

/// clj-kondo campaign regression: `(.sym k)` on a `Value::Keyword` was
/// unimplemented in `eval::types_forms::eval_dot_form` (fell through to
/// "Unable to resolve symbol: .sym" unconditionally, single-threaded or
/// not -- see that fn's `Value::Keyword` arm doc). `clj_kondo.impl.utils/
/// kw->sym` calls exactly this, and under clj-kondo's `:parallel true`
/// analyzer (many `future`s, each linting a different file, some of which
/// hit `kw->sym`) the failure looked like a `:parallel`-only race because
/// which file's future happened to reach that call varied run to run. 64
/// futures each hammering `.sym` on both namespaced and bare keywords
/// 500x, checked against the ns/name split `namespace`/`name` already
/// agree on -- fails before the fix on every thread, not just some.
#[test]
fn concurrent_keyword_dot_sym_across_many_futures() {
    assert_eq!(
        ps(
            r#"(let [futures (doall (map (fn [_]
                                            (future
                                              (dotimes [_ 500]
                                                (assert (= (.sym :foo/bar) 'foo/bar))
                                                (assert (= (.sym :baz) 'baz))
                                                (assert (= (namespace (.sym :foo/bar)) (namespace :foo/bar)))
                                                (assert (= (name (.sym :foo/bar)) (name :foo/bar))))))
                                          (range 64)))]
                 (doall (map deref futures))
                 (count futures))"#
        ),
        "64"
    );
}

// -------------------- Bug 1 (mova/PLAN.md): *ns* is per-thread --------------------

/// A `future`'s `(in-ns ...)` must not leak back to the spawning thread --
/// matches the JVM (futures convey bindings; `in-ns`'s `set!` hits the
/// future's own frame). Before the fix, `*ns*`/`in-ns` wrote a shared root
/// (`Interp::fork`'s `globals` `Arc`), so the parent thread's `*ns*` was
/// clobbered to `zzz3` too.
#[test]
fn future_in_ns_does_not_leak_to_parent_thread() {
    assert_eq!(
        ps(r#"(ns a) (defn f [] 1) (ns b)
              (let [p (promise)]
                (future (in-ns 'zzz3) (deliver p 1))
                @p
                (str *ns*))"#),
        "\"b\""
    );
}

/// Same leak, general `set!` path (not `in-ns` specifically): a var written with
/// no active `binding` frame throws (JVM) and never touches the shared root.
#[test]
fn future_set_on_dynamic_var_does_not_leak_to_parent_thread() {
    assert_eq!(
        ps(r#"(def ^:dynamic *flag* :outer)
              (let [p (promise)]
                (future (try (set! *flag* :inner) (catch IllegalStateException e (deliver p (ex-message e)))))
                [@p (name *flag*)])"#),
        "[\"Can't change/establish root binding of: *flag* with set\" \"outer\"]"
    );
}

/// `create-ns` was unresolved in mova; creates (or no-ops on an existing)
/// namespace and returns it WITHOUT switching `*ns*`.
#[test]
fn create_ns_creates_without_switching_current_ns() {
    assert_eq!(
        ps(r#"(let [before (str *ns*)]
                (create-ns 'zzz.created)
                [(= before (str *ns*)) (some? (find-ns 'zzz.created))])"#),
        "[true true]"
    );
}
