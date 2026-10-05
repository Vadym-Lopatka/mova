//! Perceus-lite phase 3: the RUST-caller argument handover
//! (`Interp::call_owned` -> `apply_closure_owned` -> moved parameter
//! slots/bindings). See `src/builtins/reuse.rs` for the invariant and
//! bench/optimization-log.md's phase-3 entry for the measurements.
//!
//! Two things need pinning, and they are different in kind:
//!
//! 1. **The feature is alive and the switch is alive.** Phase 2 measured
//!    0.00% unique hits on every `reduce`-shaped accumulator and named the
//!    reason (the caller's temp args array outlives the call). If that pin
//!    ever comes back, every program here still prints the same numbers --
//!    only the `MOVA_MAP_PROBE` counter notices. So the counter is the
//!    assertion, exactly as in `tests/reuse_test.rs`.
//!
//! 2. **Nothing became observable.** The handover changes how many handles
//!    exist on a value, and handle counts are invisible to the language
//!    (`builtins::reuse`'s invariant: a non-unique handle merely copies).
//!    The proof obligation is therefore a differential -- same program, same
//!    output, handover on and off -- with the flow error-recovery contract
//!    ("the proc survives on its PREVIOUS state") as the sharpest case,
//!    because that contract is precisely the one a wrong handover would
//!    break: it says the pre-call state must still exist after a failed
//!    call, i.e. that the callee must NOT own it uniquely.

use std::process::Command;

use mova::embed::{Engine, Value};

fn engine() -> Engine {
    Engine::builder().build()
}

fn eval_ok(src: &str) -> Value {
    engine()
        .eval_named("test", src)
        .unwrap_or_else(|e| panic!("eval error for {src:?}: {}", e.render_plain()))
}

/// Runs the real binary on `program` with the given extra env, returning
/// `(stdout, stderr)`. Every phase-3/phase-1 switch is `env_remove`d first:
/// the gate matrix runs this suite under `MOVA_NO_REUSE=1` etc., and an
/// inherited switch would make a test assert about an environment it did not
/// choose. Each test states the child's switch state outright.
fn run(program: &str, extra_env: &[(&str, &str)], tag: &str) -> (String, String) {
    let dir = std::env::temp_dir().join(format!("mova-moveargs-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join(format!("{tag}.mova"));
    std::fs::write(&path, program).expect("write program");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mova"));
    cmd.arg(&path)
        .env_remove("MOVA_NO_MOVEARGS")
        .env_remove("MOVA_NO_REUSE")
        .env_remove("MOVA_NO_LASTUSE")
        // Uniqueness needs BOTH halves of Perceus-lite: the handover frees
        // the caller's handle, last-use analysis frees the callee's slot.
        // The tree-walked tier has no last-use analysis, so a unique-hit
        // assertion under `MOVA_NO_COMPILE=1` would be asserting the wrong
        // thing -- the tier differential below sets it explicitly instead.
        .env_remove("MOVA_NO_COMPILE");
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("run mova");
    assert!(
        out.status.success(),
        "mova failed ({tag}): {}",
        String::from_utf8_lossy(&out.stderr)
    );
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// The `unique` count for `op` in the probe's (D) section.
fn unique_hits(stderr: &str, op: &str) -> u64 {
    let section = stderr
        .split("(D): consuming-path unique-hit rate")
        .nth(1)
        .unwrap_or_else(|| panic!("no (D) section in probe output:\n{stderr}"));
    let row = section
        .lines()
        .find(|l| l.starts_with(&format!("{op} ")))
        .unwrap_or_else(|| panic!("no {op} row in (D):\n{stderr}"));
    row.split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("unparseable {op} row: {row:?}"))
}

// --- (1) the feature, and the kill switch, are both alive ----------------

/// THE phase-3 shape: `reduce`'s accumulator. Phase 1 and phase 2 both
/// measured 0.00% unique here (phase 1's table C, phase 2's table C) and
/// both named the same cause -- `interp.call(&f, &[acc, h])` builds a
/// temporary array that outlives the call, so `acc` always had a second live
/// handle no matter what the callee's frame gave up.
const REDUCE_SHAPE: &str = r#"(println (count (reduce (fn [m i] (assoc m i i)) {} (range 6))))"#;

#[test]
fn the_reduce_accumulator_is_unique_by_default() {
    let (out, err) = run(REDUCE_SHAPE, &[("MOVA_MAP_PROBE", "1")], "reduce-on");
    assert_eq!(out.trim(), "6");
    // 6 assocs; the first receives the `{}` the IR's own constant still
    // holds, so 5 is the structural maximum.
    assert_eq!(
        unique_hits(&err, "assoc"),
        5,
        "expected 5 of 6 reduce accumulator assocs to hit a uniquely-owned \
         receiver -- 0 means the caller's args buffer is pinning it again\n{err}"
    );
}

#[test]
fn mova_no_moveargs_restores_the_pin() {
    // Guards against a SILENTLY DEAD KILL SWITCH: the gate matrix's
    // "the suite passes identically with MOVA_NO_MOVEARGS=1" leg is
    // worthless if the variable does nothing.
    let (out, err) = run(
        REDUCE_SHAPE,
        &[("MOVA_MAP_PROBE", "1"), ("MOVA_NO_MOVEARGS", "1")],
        "reduce-off",
    );
    assert_eq!(out.trim(), "6");
    assert_eq!(
        unique_hits(&err, "assoc"),
        0,
        "MOVA_NO_MOVEARGS=1 must restore phase 2's borrow-and-clone \
         behaviour, i.e. 0.00% unique on this shape\n{err}"
    );
}

#[test]
fn a_flow_transform_state_is_never_unique_even_with_the_handover() {
    // NOT a limitation to be fixed later -- a theorem, and worth a test so a
    // later change cannot quietly trade the error-recovery contract for a
    // benchmark number. `process_message_to` KEEPS `*ctx.state` across the
    // step call because FLOW-DESIGN.md says a failed transform leaves the
    // proc running on its previous state; a value that must survive the call
    // cannot be one the callee is free to destroy in place.
    let program = r#"
      (def done (chan 1))
      (def sink
        (flow/map->step
         {:describe (fn [] {:ins {:in {}} :outs {:done {}}})
          :init (fn [_] {:n 0 :clojure.core.async.flow/out-ports {:done done}})
          :transform (fn [s _ m]
                       (let [s2 (assoc s :n (inc (:n s)))]
                         (if (= (:n s2) 20) [s2 {:done [(:n s2)]}] [s2 {}])))}))
      (def fl (flow/create-flow {:procs {:sink {:proc (flow/process sink)}} :conns []}))
      (flow/start fl)
      (flow/resume fl)
      (flow/inject fl [:sink :in] (range 20))
      (println (<!! done))
      (flow/stop fl)"#;
    let (out, err) = run(program, &[("MOVA_MAP_PROBE", "1")], "flow-state");
    assert_eq!(out.trim(), "20");
    assert_eq!(
        unique_hits(&err, "assoc"),
        0,
        "the flow proc's kept recovery handle must keep the transform's \
         state map non-unique\n{err}"
    );
}

// --- (2) nothing became observable ---------------------------------------

/// A flow program that exercises every path phase 3 touched at once: a
/// stateful transform whose state is grown with `assoc` (a consuming
/// native's receiver), a transform that THROWS on selected messages (the
/// error-recovery path), a variadic user fn behind `apply` (the `& rest`
/// wrap, which the owned binder rebuilds itself), `swap!` (its own handover
/// site), and `reduce`/`reduce-kv` (accumulator handover). Output is fully
/// ordered and deterministic.
const FLOW_DIFFERENTIAL: &str = r#"
  (def sink-ch (chan 100))
  (def hits (atom {}))
  (defn tally [& xs] (apply + xs))
  (def step
    (flow/map->step
     {:describe (fn [] {:ins {:in {}} :outs {:out {}}})
      :init (fn [_] {:n 0 :seen {}})
      :transform (fn [s _ m]
                   (if (= 0 (mod m 4))
                     (throw (str "boom-" m))
                     (let [s2 (assoc s :n (inc (:n s)) :seen (assoc (:seen s) m m))]
                       [s2 {:out [[(:n s2) (count (:seen s2)) (tally m (:n s2) 1)]]}])))}))
  (def sink
    (flow/map->step
     {:describe (fn [] {:ins {:in {}} :outs {}})
      :transform (fn [s _ m] (>!! sink-ch m) (swap! hits update :c (fn [c] (inc (or c 0)))) [s {}])}))
  (def fl (flow/create-flow
           {:procs {:e {:proc (flow/process step)} :s {:proc (flow/process sink)}}
            :conns [[[:e :out] [:s :in]]]}))
  (def chans (flow/start fl))
  (flow/resume fl)
  (flow/inject fl [:e :in] (range 1 13))
  ;; 3 of the 12 messages throw (4, 8, 12), so 9 reach the sink.
  (doseq [_ (range 9)] (println "sink" (<!! sink-ch)))
  (doseq [_ (range 3)]
    (let [err (<!! (:error-chan chans))]
      (println "err"
               (:clojure.core.async.flow/pid err)
               (:clojure.core.async.flow/msg err)
               ;; THE CONTRACT: the state reported with the error is the
               ;; PREVIOUS state, intact -- both its counter and the map it
               ;; had been growing with `assoc`.
               (:n (:clojure.core.async.flow/state err))
               (count (:seen (:clojure.core.async.flow/state err))))))
  (println "hits" @hits)
  (println "reduce" (reduce (fn [m i] (assoc m i i)) {} (range 12)))
  (println "reduce-kv" (reduce-kv (fn [m k v] (assoc m k (inc v))) {} {:a 1 :b 2}))
  (println "update-in" (update-in {:a {:b [1 2 3]}} [:a :b] conj 4))
  (flow/stop fl)"#;

#[test]
fn flow_and_accumulator_behaviour_is_identical_with_the_handover_on_and_off() {
    let (on, _) = run(FLOW_DIFFERENTIAL, &[], "diff-on");
    let (off, _) = run(FLOW_DIFFERENTIAL, &[("MOVA_NO_MOVEARGS", "1")], "diff-off");
    assert_eq!(on, off, "handover changed observable behaviour");
    // Also assert the content, so a program that silently stopped running
    // (both legs printing nothing) cannot pass.
    assert_eq!(on.matches("sink ").count(), 9, "expected 9 delivered messages:\n{on}");
    assert_eq!(on.matches("err ").count(), 3, "expected 3 error reports:\n{on}");
    assert!(on.contains("hits {:c 9}"), "swap! tally wrong:\n{on}");
}

#[test]
fn the_handover_is_identical_under_the_tree_walked_tier_too() {
    // `MOVA_NO_COMPILE=1` routes every fn through `run_closure_body_owned`
    // (env bindings) instead of `run_compiled_body_owned` (slots): the same
    // differential must hold on the tier that has no last-use analysis at
    // all.
    let (on, _) = run(FLOW_DIFFERENTIAL, &[("MOVA_NO_COMPILE", "1")], "diff-tw-on");
    let (off, _) = run(
        FLOW_DIFFERENTIAL,
        &[("MOVA_NO_COMPILE", "1"), ("MOVA_NO_MOVEARGS", "1")],
        "diff-tw-off",
    );
    assert_eq!(on, off, "handover changed observable behaviour (tree-walked)");
    assert_eq!(on.matches("sink ").count(), 9, "expected 9 delivered messages:\n{on}");
}

// --- the error-recovery contract, in process ------------------------------

#[test]
fn a_failed_transform_leaves_the_assoc_grown_state_intact_for_the_next_message() {
    // The sharpest statement of the contract under the new ownership flow:
    // the state is grown with `assoc` (so it IS a consuming native's
    // receiver), several messages in a row throw, and the run must continue
    // from the state as it stood before the FIRST failure -- structurally
    // whole, not truncated, not `nil`.
    let result = eval_ok(
        r#"(let [step (flow/map->step
                        {:describe (fn [] {:ins {:in {}} :outs {:out {}}})
                         :init (fn [_] {:seen {}})
                         :transform (fn [s _ m]
                                      (if (< 2 m 6)
                                        (throw "boom")
                                        (let [s2 (assoc s :seen (assoc (:seen s) m m))]
                                          [s2 {:out [(count (:seen s2))]}])))})
                  sink-ch (chan 10)
                  sink (flow/map->step
                        {:describe (fn [] {:ins {:in {}} :outs {}})
                         :transform (fn [s _ m] (>!! sink-ch m) [s {}])})
                  fl (flow/create-flow
                      {:procs {:e {:proc (flow/process step)}
                               :s {:proc (flow/process sink)}}
                       :conns [[[:e :out] [:s :in]]]})
                  chans (flow/start fl)
                  _ (flow/resume fl)
                  _ (flow/inject fl [:e :in] [1 2 3 4 5 6])
                  a (<!! sink-ch)
                  b (<!! sink-ch)
                  e1 (<!! (:error-chan chans))
                  e2 (<!! (:error-chan chans))
                  e3 (<!! (:error-chan chans))
                  c (<!! sink-ch)]
              (flow/stop fl)
              [a b
               ;; every error report carries the kept state, whole
               (count (:seen (:clojure.core.async.flow/state e1)))
               (count (:seen (:clojure.core.async.flow/state e2)))
               (count (:seen (:clojure.core.async.flow/state e3)))
               ;; and the next good message resumes from it
               c])"#,
    );
    let expected: Vec<Value> = [1, 2, 2, 2, 2, 3].iter().map(|n| Value::from(*n as i64)).collect();
    assert_eq!(result, Value::from(expected));
}

// --- binder edges the owned path rebuilds itself --------------------------

#[test]
fn moved_arguments_bind_variadic_and_multi_arity_fns_exactly_as_borrowed_ones_did() {
    // The owned binder consumes each argument index at most once and rebuilds
    // the `& rest` list itself, so these are the shapes where an off-by-one
    // would show up as a `nil` argument rather than a wrong number.
    assert_eq!(
        eval_ok(
            r#"(let [f (fn ([] :none) ([a] [:one a]) ([a & r] [:many a r]))]
                 [(apply f []) (apply f [1]) (apply f [1 2 3])
                  (f) (f 1) (f 1 2 3)
                  (reduce (fn [acc x] (conj acc x)) [] [1 2 3])
                  (apply (fn [a b & r] [a b r]) 1 [2])])"#
        ),
        eval_ok(
            r#"[:none [:one 1] [:many 1 '(2 3)]
                :none [:one 1] [:many 1 '(2 3)]
                [1 2 3]
                [1 2 nil]]"#
        )
    );
}

#[test]
fn a_moved_argument_is_still_visible_to_a_closure_that_captured_the_caller_side() {
    // The caller's OWN handle (a `let` binding, a captured value) is
    // untouched by the handover -- only the args buffer gives up its handle.
    assert_eq!(
        eval_ok(
            r#"(let [m {:a 1}
                     f (fn [x] (assoc x :b 2))
                     r (f m)]
                 [m r (reduce (fn [acc _] (f acc)) m [1 2])])"#
        ),
        eval_ok(r#"[{:a 1} {:a 1 :b 2} {:a 1 :b 2}]"#)
    );
}
