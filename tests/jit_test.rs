//! E1b (docs/JIT.md): correctness checks for direct calls + inline caches
//! + loop/recur, run with the real binary under both `MOVA_JIT=1` and
//! unset -- `jit::enabled()` is a process-wide `OnceLock`, so (like
//! `explain_test.rs`/`err_stderr_fallback.rs`) the only way to flip it
//! between cases is to spawn a fresh process per case.

use std::process::{Command, Output};

fn run(src: &str, jit: bool) -> Output {
    let bin = env!("CARGO_BIN_EXE_mova");
    let mut cmd = Command::new(bin);
    cmd.arg("-e").arg(src);
    if jit {
        cmd.env("MOVA_JIT", "1");
    } else {
        cmd.env_remove("MOVA_JIT");
    }
    cmd.output().expect("failed to spawn mova binary")
}

/// (stdout, stderr, exit code) -- what "same result/error as MOVA_JIT
/// unset" is checked against, since a Mova runtime error goes to stderr
/// with a non-zero exit rather than into the printed value.
fn triple(o: &Output) -> (String, String, Option<i32>) {
    (
        String::from_utf8_lossy(&o.stdout).to_string(),
        String::from_utf8_lossy(&o.stderr).to_string(),
        o.status.code(),
    )
}

fn assert_same_under_jit(src: &str) -> (String, String, Option<i32>) {
    let with = triple(&run(src, true));
    let without = triple(&run(src, false));
    assert_eq!(with, without, "MOVA_JIT=1 vs unset diverged for:\n{src}");
    with
}

const TLOOP: &str = "(defn tloop [n] (loop [i 0 acc 0] (if (< i n) (recur (inc i) (+ acc (f1 i))) acc)))";

#[test]
fn redefinition_int_to_int_picks_up_new_body() {
    let src = format!(
        "(defn f1 [x] (+ x 1)) {TLOOP} (println (tloop 100000)) (defn f1 [x] (+ x 2)) (println (tloop 100000))"
    );
    let (stdout, stderr, code) = assert_same_under_jit(&src);
    assert_eq!(code, Some(0), "stderr: {stderr}");
    // sum(i+1, i=0..99999) = 4999950000; sum(i+2, ...) = 5000150000.
    assert_eq!(stdout, "5000050000\n5000150000\nnil\n");
}

#[test]
fn redefinition_to_incompatible_type_errors_like_interpreter() {
    let src = format!(
        "(defn f1 [x] (+ x 1)) {TLOOP} (println (tloop 100000)) (defn f1 [x] (str x)) (println (tloop 100000))"
    );
    assert_same_under_jit(&src);
}

#[test]
fn mutual_recursion_matches_interpreter() {
    let src = "(declare od?) \
               (defn ev? [n] (if (zero? n) 1 (od? (dec n)))) \
               (defn od? [n] (if (zero? n) 0 (ev? (dec n)))) \
               (println (ev? 100000))";
    assert_same_under_jit(src);
}

#[test]
fn overflow_inside_jitted_loop_matches_interpreter() {
    let src = "(defn tloop2 [n] (loop [i 0 acc 9223372036854775807] \
                 (if (< i n) (recur (inc i) (+ acc 1)) acc))) \
               (println (tloop2 5))";
    assert_same_under_jit(src);
}

#[test]
fn binding_on_dynamic_var_callee_stays_correct() {
    let src = "(def ^:dynamic *f* (fn [x] (+ x 1))) \
               (defn tloop3 [n] (loop [i 0 acc 0] (if (< i n) (recur (inc i) (+ acc (*f* i))) acc))) \
               (println (tloop3 100000)) \
               (println (binding [*f* (fn [x] (+ x 100))] (tloop3 100000))) \
               (println (tloop3 100000))";
    let (stdout, stderr, code) = assert_same_under_jit(src);
    assert_eq!(code, Some(0), "stderr: {stderr}");
    // sum(i+1)=5000050000; sum(i+100)=4999950000+10000000=5009950000; back to 5000050000.
    assert_eq!(stdout, "5000050000\n5009950000\n5000050000\nnil\n");
}

// -------------------- E3a: generic (any-`Value`) tier --------------------

#[test]
fn error_inside_native_callee_three_levels_deep_matches_interpreter() {
    let src = "(defn c3 [x] (+ x :bad)) \
               (defn c2 [x] (c3 x)) \
               (defn c1 [x] (c2 x)) \
               (c1 5)";
    let (_, _, code) = assert_same_under_jit(src);
    assert_eq!(code, Some(1));
}

#[test]
fn keyword_map_string_args_and_returns_match_interpreter() {
    let src = "(defn ident [x] x) \
               (println (ident :hello)) \
               (println (ident {:a 1 :b 2})) \
               (println (ident \"a-string\")) \
               (println (ident [1 2 3]))";
    let (stdout, stderr, code) = assert_same_under_jit(src);
    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert_eq!(stdout, ":hello\n{:a 1, :b 2}\na-string\n[1 2 3]\nnil\n");
}

#[test]
fn identity_fn_returns_a_cloned_borrowed_arg() {
    // `(fn [x] x)` -- `x` is a borrowed param; the native entry must CLONE
    // it into `*out` (docs/NATIVE-TIER-DESIGN.md's closing note), not just
    // alias the caller's own value.
    let src = "(defn ident [x] x) \
               (let [m {:a 1}] \
                 (println (= m (ident m))) \
                 (println (identical? m (ident m))))";
    assert_same_under_jit(src);
}

#[test]
fn println_in_a_loop_runs_exactly_once_per_iteration() {
    // Generic code has side effects and never bails/re-runs -- unlike E1,
    // a loop calling a global (println) must fire it exactly N times.
    let src = "(defn loop3 [n] \
                 (loop [i 0] \
                   (if (= i n) :done (do (println \"iter\" i) (recur (inc i)))))) \
               (loop3 3)";
    let (stdout, stderr, code) = assert_same_under_jit(src);
    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert_eq!(stdout, "iter 0\niter 1\niter 2\n:done\n");
}

#[test]
fn redefinition_mid_run_generic_tier() {
    let src = "(defn greet [] (println \"v1\")) \
               (defn caller [] (greet)) \
               (caller) \
               (defn greet [] (println \"v2\")) \
               (caller)";
    let (stdout, stderr, code) = assert_same_under_jit(src);
    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert_eq!(stdout, "v1\nv2\nnil\n");
}

#[test]
fn arity_error_through_generic_call_matches_interpreter() {
    let src = "(defn f [a b] (+ a b)) (defn g [] (f 1)) (g)";
    let (_, _, code) = assert_same_under_jit(src);
    assert_eq!(code, Some(1));
}

#[test]
fn cross_ns_call_matches_interpreter() {
    // G2a: a `CallGlobal` light call whose callee lives in another ns --
    // `apply_closure_buf`'s own ns-swap bracket runs unmodified (this file
    // never re-implements it), so the callee body resolves ITS OWN globals
    // correctly regardless of which ns is calling it.
    let src = "(ns jit-a) (def marker :a) (defn helper [x] (+ x (if (= marker :a) 100 0))) \
               (ns jit-b) (def marker :b) \
               (defn caller [x] (jit-a/helper x)) \
               (println (caller 1))";
    let (stdout, stderr, code) = assert_same_under_jit(src);
    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert_eq!(stdout, "101\nnil\n");
}

#[test]
fn builtin_redefinition_of_count_matches_interpreter() {
    // G2a: `count` is called through `CallGlobal` from a compiled fn both
    // before and after `(def count ...)` shadows the core builtin -- the
    // light path's IC must not keep serving the old (native) target once
    // `DEF_EPOCH` has moved.
    let src = "(defn wrap [x] (count x)) \
               (println (wrap [1 2 3])) \
               (def count (fn [_] :shadowed)) \
               (println (wrap [1 2 3]))";
    let (stdout, stderr, code) = assert_same_under_jit(src);
    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert_eq!(stdout, "3\n:shadowed\nnil\n");
}

#[test]
fn deep_tail_recursion_100000_matches_interpreter() {
    let src = "(defn count-down [n] (if (zero? n) :done (recur (dec n)))) \
               (println (count-down 100000))";
    let (stdout, stderr, code) = assert_same_under_jit(src);
    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert_eq!(stdout, ":done\nnil\n");
}

// K1 (docs/JIT.md "Fast call path (K1)"): fast calls must be observably
// identical to the old path -- traces, *ns*, overflow, recur, threads.

#[test]
fn k1_error_three_deep_through_fast_calls_same_trace() {
    let src = "(defn e3 [x] (throw (ex-info \"boom\" {:x x}))) \
               (defn e2 [x] (let [y (e3 x)] y)) \
               (defn e1 [x] (let [y (e2 x)] y)) \
               (defn top [x] (let [y (e1 x)] y)) \
               (top :k)";
    let (_, stderr, code) = assert_same_under_jit(src);
    assert_ne!(code, Some(0));
    assert!(stderr.contains("at (e3)"), "stderr: {stderr}");
}

#[test]
fn k1_error_inside_protocol_impl_via_reduce() {
    let src = "(defprotocol P (pm [this x])) \
               (defrecord R [a] P (pm [_ x] (if (= x :bad) (throw (ex-info \"bad\" {})) a))) \
               (defn step [acc x] (let [v (pm (->R :z) x)] (conj acc v))) \
               (defn go [] (reduce step [] [:a :b :bad])) \
               (println (try (go) (catch Exception e (ex-message e)))) \
               (go)";
    let (stdout, _, code) = assert_same_under_jit(src);
    assert_ne!(code, Some(0));
    assert_eq!(stdout, "bad\n");
}

#[test]
fn k1_cross_ns_fast_call_sees_ns_and_restores_it() {
    let src = "(ns a.core) \
               (defn who [x] (let [n (str *ns*)] [x n])) \
               (defn hop [x] (in-ns 'b.other) x) \
               (ns user) \
               (defn call-who [x] (let [r (a.core/who x)] r)) \
               (println (call-who :k)) \
               (println (callstack*)) \
               (defn call-hop [x] (let [r (a.core/hop x)] (str *ns*))) \
               (println (call-hop :k)) \
               (println (str *ns*))";
    let (_, stderr, code) = assert_same_under_jit(src);
    assert_eq!(code, Some(0), "stderr: {stderr}");
}

#[test]
fn k1_callstack_inside_fast_calls() {
    let src = "(defn c3 [x] (let [s (callstack*)] (count s))) \
               (defn c2 [x] (let [r (c3 x)] r)) \
               (defn c1 [x] (let [r (c2 x)] r)) \
               (println (c1 :k)) \
               (println (reduce (fn [a x] (c1 x)) 0 [:a :b]))";
    let (stdout, stderr, code) = assert_same_under_jit(src);
    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert!(!stdout.is_empty());
}

#[test]
fn k1_stack_overflow_message_matches() {
    let src = "(defn deep [m] (let [r (deep (assoc m :k 1))] r)) \
               (println (try (deep {}) (catch Throwable e (ex-message e)))) \
               (deep {})";
    let (stdout, _, code) = assert_same_under_jit(src);
    assert_ne!(code, Some(0));
    assert!(stdout.contains("stack overflow"), "stdout: {stdout}");
}

#[test]
fn k1_recur_in_fast_called_fns() {
    let src = "(println ((fn [x] (if (pos? x) (recur (dec x)) x)) 3)) \
               (defn cd [x acc] (if (pos? x) (recur (dec x) (conj acc (if (odd? x) :o :e))) acc)) \
               (defn go [] (let [r (cd 5 [])] r)) \
               (println (go)) \
               (defn vr [x & more] (if (pos? x) (recur (dec x) more) [x more])) \
               (defn go2 [] (let [r (vr 3 :a :b)] r)) \
               (println (go2)) \
               (println (reduce (fn [a x] (if (pos? x) (recur a (dec x)) (conj a x))) [] [2 3]))";
    let (stdout, stderr, code) = assert_same_under_jit(src);
    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert_eq!(stdout, "0\n[:o :e :o :e :o]\n[0 (:a :b)]\n[0 0]\nnil\n");
}

#[test]
fn k1_futures_call_fast_fns_concurrently() {
    let src = "(defn leaf [m] (assoc m :n (inc (:n m)))) \
               (defn walk [m k] (if (zero? k) m (let [m2 (leaf m)] (recur m2 (dec k))))) \
               (defn go [] (let [r (walk {:n 0} 20000)] (:n r))) \
               (println (reduce + (map deref (doall (for [_ (range 8)] (future (go)))))))";
    let (stdout, stderr, code) = assert_same_under_jit(src);
    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert_eq!(stdout, "160000\nnil\n");
}

// ---------------------------------------------------------------------------
// L1 (docs/JIT.md "Leaf ops (L1)"): IN-PROCESS cases -- many `Interp`s back
// to back in one process (the shape that broke G2b), compiled vs
// tree-walked, each pinned to an expected value. Runs under whatever
// `MOVA_JIT` the test process has (the gate runs it both ways).
// ---------------------------------------------------------------------------

fn inproc(src: &str) -> String {
    let mut i = mova::internal::Interp::new();
    match i.eval_str("jit_test", src) {
        Ok(v) => match i.realize_deep(&v) {
            Ok(r) => mova::internal::pr_str(&r),
            Err(e) => format!("ERR {}", e.message),
        },
        Err(e) => format!("ERR {}", e.message),
    }
}

fn inproc_walked(src: &str) -> String {
    let mut i = mova::internal::Interp::with_compile_enabled(false);
    match i.eval_str("jit_test", src) {
        Ok(v) => match i.realize_deep(&v) {
            Ok(r) => mova::internal::pr_str(&r),
            Err(e) => format!("ERR {}", e.message),
        },
        Err(e) => format!("ERR {}", e.message),
    }
}

const L1_CASES: &[(&str, &str)] = &[
    ("(defn f [n acc] (if (zero? n) acc (recur (dec n) (if (even? n) (+ acc n) acc)))) (f 1000 0)", "250500"),
    ("(vec (for [x (range 20) :let [y (* x x)] :when (odd? y)] (+ x y)))", "[2 12 30 56 90 132 182 240 306 380]"),
    ("(mapv (fn [g] (g 10)) (vec (for [i (range 5)] (fn [x] (+ x i)))))", "[10 11 12 13 14]"),
    ("(defn ov [x] (+ x 1)) (dotimes [_ 50] (ov 1)) (ov 9223372036854775807)", "ERR"),
    ("(defn in2 [x] (inc x)) (dotimes [_ 50] (in2 1)) (in2 9223372036854775807)", "ERR"),
    ("(defn de2 [x] (dec x)) (dotimes [_ 50] (de2 1)) (de2 -9223372036854775808)", "ERR"),
    ("(defn su [a b] (- a b)) (dotimes [_ 50] (su 1 2)) (su -9223372036854775808 1)", "ERR"),
    ("(defn mu [a b] (* a b)) (dotimes [_ 50] (mu 1 2)) [(mu 3 4) (mu 4611686018427387904 2)]", "ERR"),
    ("(defn a2 [a b] (+ a b)) [(a2 1 2.5) (a2 1.5 2) (a2 1 2) (a2 -1 1)]", "[3.5 3.5 3 0]"),
    ("(defn lt [a b] (< a b)) [(lt 9007199254740993 9007199254740992) (lt 1 2) (lt 2 1) (lt 1 1.5)]", "[false true false true]"),
    ("(defn cmps [a b] [(<= a b) (> a b) (>= a b) (= a b) (zero? a)]) [(cmps 0 0) (cmps 3 2) (cmps 2.0 2)]",
     "[[true false true true true] [false true true false false] [true false true false false]]"),
    ("(defn a3 [x] (+ x 1)) (dotimes [_ 100] (a3 1)) (def + (fn [a b] (str a \"+\" b))) (a3 1)", "\"1+1\""),
    ("(def k 1) (defn gk [] k) (dotimes [_ 100] (gk)) (def k 2) (gk)", "2"),
    ("(def ^:dynamic *d* 1) (defn gd [] *d*) (dotimes [_ 100] (gd)) [(gd) (binding [*d* 5] (gd)) (gd)]", "[1 5 1]"),
    ("(defn mk [a] (fn self [n] (if (zero? n) a (self (dec n))))) ((mk :x) 50)", ":x"),
    ("(defn kw [m] (if (= :a (:t m)) (get m :v) :none)) [(kw {:t :a :v 1}) (kw {:t :b})]", "[1 :none]"),
    ("(defn bld [n] (loop [i 0 m {}] (if (< i n) (recur (inc i) (assoc m i (str i))) (count m)))) (bld 1000)", "1000"),
    ("(defn tr [x] (if x :t :f)) (mapv tr [nil false 0 \"\" [] true 0.0])", "[:f :f :t :t :t :t :t]"),
    ("(defn bad [x] (+ x 1)) (bad \"a\")", "ERR"),
    ("(defn bz [x] (zero? x)) (bz :k)", "ERR"),
    ("(defn nt [x] (not x)) [(nt nil) (nt 1) (nt false)]", "[true false true]"),
    ("(defn dv [a b] (/ a b)) [(dv 6 3) (dv 1 2)]", "[2 1/2]"),
    ("(defn na [a b c] [(+ a b c) (* a b c) (+) (*) (+ a) (* 2 a b c 1)]) [(na 1 2 3) (na 1.5 2 3)]", "[[6 6 0 1 1 12] [6.5 9.0 0 1 1.5 18.0]]"),
    ("(defn na2 [a] (+ a 1 2)) (na2 9223372036854775806)", "ERR"),
    ("(defrecord R [a]) (defn kg [m] [(:a m) (:a (first [m])) (:b m)]) [(kg {:a 1}) (kg (->R 7)) (kg nil) (kg #{:a}) (kg 5)]",
     "[[1 1 nil] [7 7 nil] [nil nil nil] [:a :a nil] [nil nil nil]]"),
    ("(defn kt [xs] (loop [xs xs acc []] (if (seq xs) (let [x (first xs)] (recur (rest xs) (conj acc (:k x)))) acc))) (kt [{:k 1} {:k [2]} {}])", "[1 [2] nil]"),
    ("(defn cc [v] (let [a (first v) b a c [a b]] (conj c a))) (cc [[1] 2])", "[[1] [1] [1]]"),
];

#[test]
fn l1_inprocess_many_interps_match_tree_walker() {
    for round in 0..3 {
        for (src, want) in L1_CASES {
            let got = inproc(src);
            let walked = inproc_walked(src);
            if want.starts_with("ERR") {
                assert!(got.starts_with("ERR"), "round {round}: {src} => {got}");
                assert_eq!(got, walked, "round {round}: {src}");
            } else {
                assert_eq!(&got, want, "round {round}: {src}");
                assert_eq!(got, walked, "round {round}: {src}");
            }
        }
    }
}

#[test]
fn l1_leaf_ops_cli_match_interpreter() {
    for (src, _) in L1_CASES {
        assert_same_under_jit(&format!("(prn (do {src}))"));
    }
}

/// let/loop rebind after a closure compiles (no tree-walk fallback) and
/// matches JVM Clojure with and without MOVA_JIT.
#[test]
fn let_rebind_after_closure_compiles_and_matches_jvm() {
    let cases = [
        ("(defn t [] (let [a 1 f (fn [] a) a 2] [(f) a])) (t)", "[1 2]"),
        ("(defn t [n] (loop [a 1 f (fn [] a) a 2 i 0] (if (< i n) (recur a f (inc a) (inc i)) [(f) a]))) (t 2)", "[1 4]"),
        ("(defn t [] (let [[a b] [1 2] f (fn [] [a b]) [a b] [3 4]] [(f) a b])) (t)", "[[1 2] 3 4]"),
    ];
    for (src, want) in cases {
        for jit in [false, true] {
            let bin = env!("CARGO_BIN_EXE_mova");
            let mut cmd = Command::new(bin);
            cmd.arg("-e").arg(src);
            cmd.env("MOVA_EXPLAIN", "1");
            if jit { cmd.env("MOVA_JIT", "1"); } else { cmd.env_remove("MOVA_JIT"); }
            let o = cmd.output().expect("spawn");
            let (out, err) = (String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
            assert!(!err.contains("tree-walks"), "fell back (jit={jit}) for {src}: {err}");
            assert!(out.lines().any(|l| l.trim() == want), "jit={jit} {src}: got {out:?} {err:?}");
        }
    }
}

/// Runs of fn-valued let/loop bindings are sequential and compile (no fallback); letfn still works.
#[test]
fn let_fn_run_sequential_compiles_and_matches_jvm() {
    let cases = [
        ("(defn t [] (let [a 1 f (fn [] a) a (fn [] 2)] (f))) (t)", "1"),
        ("(defn t [] (let [f (fn [] 1) g (fn [] (f)) f (fn [] 2)] [(g) (f)])) (t)", "[1 2]"),
        ("(defn t [] (let [g (fn g [] 5) h (fn [] (g))] (h))) (t)", "5"),
        ("(defn t [] (let [f (fn f [n] (if (zero? n) :done (f (dec n)))) f (fn [] 2)] (f))) (t)", "2"),
        ("(defn t [] (loop [a 1 f (fn [] a) a (fn [] 2)] (f))) (t)", "1"),
        ("(defn t [] (letfn [(ev? [n] (if (zero? n) true (od? (dec n)))) (od? [n] (if (zero? n) false (ev? (dec n))))] [(ev? 10) (od? 7)])) (t)", "[true true]"),
    ];
    for (src, want) in cases {
        for jit in [false, true] {
            let mut cmd = Command::new(env!("CARGO_BIN_EXE_mova"));
            cmd.arg("-e").arg(src).env("MOVA_EXPLAIN", "1");
            if jit { cmd.env("MOVA_JIT", "1"); } else { cmd.env_remove("MOVA_JIT"); }
            let o = cmd.output().expect("spawn");
            let (out, err) = (String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
            assert!(!err.contains("tree-walks"), "fell back (jit={jit}) for {src}: {err}");
            assert!(out.lines().any(|l| l.trim() == want), "jit={jit} {src}: got {out:?} {err:?}");
        }
    }
}

/// K6: local args aliased into call temps (plain natives read them in place;
/// closures, consuming natives and protocol receivers get promoted clones).
#[test]
fn k6_borrowed_call_args_keep_locals_intact() {
    let src = r#"
(defprotocol P (pk [x k]))
(defrecord R [a b] P (pk [x k] (get x k)))
(defn g [m k] (assoc m k (count m)))
(defn f [m v r n]
  (loop [i 0 acc []]
    (if (< i n)
      (recur (inc i) (conj acc [(get m :a) (count v) (nth v 1) (g m :z) (pk r :b) (str m v) (:a r) (vector m v)]))
      acc)))
(let [m {:a 1 :b 2} v [1 2 3] r (->R 1 2) _ (dotimes [_ 500] (f m v r 5)) out (f m v r 3000)]
  (prn (count out) (last out) m v r (try (f m v r "x") (catch Exception e :err))))
"#;
    let (out, _, code) = assert_same_under_jit(src);
    assert_eq!(code, Some(0));
    assert!(out.contains("3000"), "{out}");
}
