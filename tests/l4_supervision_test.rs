//! L4 supervision gates: W1's exit reasons, W2's supervisor task, policy,
//! restart and stop-ordering, and W3's escalation ladder + kill
//! (docs/L4-LANDING-SPEC.md §W1/§W2/§W3,
//! docs/L4-SUPERVISION-DESIGN.md §3.1-§3.6). This is the wave's required
//! deliverable test file.
//!
//! **What W3 adds, and the one new user-visible surface.** `flow/stop-proc`
//! (mova-native -- upstream has no per-proc stop) is the ONLY way to reach
//! the escalation ladder from Mova, and that is deliberate: on a `:proc-exit`
//! the run is already dead and W2 simply restarts it, so nothing else in the
//! engine has an "it will not die" state to escalate out of. The ladder is
//! graceful `::flow/stop` -> `:grace-ms` window -> kill (task procs) or
//! `:proc-wedged` (`:io` thread procs, wall W6 / owner ruling #4), and a
//! stop-proc'd run does NOT restart -- `:proc-stopped` is terminal by user
//! intent.
//!
//! **What changed for W2, and why the W1 tests below look different.**
//! `flow/start`'s `:supervision-chan` key was W1 scaffolding and is GONE
//! (§W2.6): the supervisor is that chan's consumer now, and a second taker on
//! it would steal death events out from under the supervisor. Everything W1
//! observed through it is observed through `report-chan` instead -- the
//! supervisor mirrors every `:proc-exit` it consumes there verbatim, and adds
//! its own `:proc-restart`/`:proc-give-up`/`:proc-wedged`.
//!
//! **Coverage split, stated up front (mirrors this wave's report).** Four
//! claims are NOT in this file, by design:
//! - `ExitReason::Normal` (control chan closed with an EMPTY buffer) has no
//!   Mova-level trigger -- `flow/stop` always buffers one `::flow/stop`
//!   command before closing a control chan. Exercised at the Rust level in
//!   `src/builtins/flow.rs`'s own test module
//!   (`control_closed_with_an_empty_buffer_exits_normal`).
//! - **`ExitReason::Stopped` is no longer Mova-observable at all**, and that
//!   is W2's stop-ordering working as designed: `stop_flow_cell` closes
//!   `sup_chan` (step 1b) BEFORE it broadcasts `::flow/stop`, so the
//!   `:stopped` deaths that follow land on a closed chan and are dropped.
//!   The reason itself is still computed and still put on every done-cell;
//!   `control_closed_after_a_buffered_stop_still_exits_stopped` pins it in
//!   Rust, `decide_ignores_normal_and_stopped_exits` pins what the policy
//!   does with it, and `a_supervised_stop_reports_nothing_after_step_1b`
//!   below pins the drop end-to-end.
//! - The `sup_chan`/supervisor-task allocation censuses ("an unsupervised
//!   flow allocates neither") need `pub(crate)` counters invisible to this
//!   crate-external file; both directions are asserted in `flow.rs`'s own
//!   test module (`unsupervised_flow_allocates_no_sup_chan`,
//!   `unsupervised_flow_spawns_no_supervisor_task`, and their supervised
//!   siblings).
//! - `decide()`'s rule matrix is a pure-function unit test and lives with the
//!   function, in `flow.rs`'s test module (gate G-DET's "decide()
//!   pure-unit-tested" clause). This file drives the same rules through real
//!   crashes.
//!
//! **The crash trigger.** No ordinary Mova program can kill a proc: a
//! `(throw ...)` inside a `transform` is an INCIDENT (error-chan, proc
//! continues -- the D-B invariant this wave must not touch), and deep
//! recursion returns a controlled error. The one proc-killing panic reachable
//! from Mova is `call_transition`, which is deliberately unguarded, so every
//! crash below is a host native registered through the public
//! `embed::Engine::register_fn` surface that panics, called from a step's
//! `:transition` arity on the `::flow/pause` transition only. `flow/pause-proc`
//! then kills exactly one proc, mid-flight, with the flow still `Running` --
//! no `stop` anywhere near it.
//!
//! **World discipline.** Every test in this file that is not a `run_child`
//! worker assumes the DEFAULT world (procs are runtime tasks). A globally
//! applied `MOVA_FLOW_THREAD_PROCS=1` is NOT a supported way to run it --
//! that switch is a process-global `OnceLock` and cannot have two values in
//! one binary, which is exactly why the thread-world and fused-world claims
//! live in `#[ignore]`d child workers (section 9) instead. W3's kill tests
//! join that rule for a semantic reason as well: an `:io`/thread proc is
//! unkillable by design, so under that switch they would be asserting the
//! opposite of what they mean (their thread-world counterpart is
//! `stop_proc_on_a_wedged_io_proc_reports_proc_wedged_and_never_kills`,
//! which asks for `:workload :io` explicitly and runs in the default world).
//!
//! **Timing discipline.** Every wait below is a bounded `alts!!` against a
//! `timeout`, and every bound is at least 4x the interval it is separating
//! (300ms of silence vs an 800ms backoff; 2-3s of patience for an event that
//! is produced in microseconds). Nothing here measures a duration; the
//! backoff test asserts an ORDER of events across two coarse windows instead,
//! which is what keeps it honest on a loaded machine. No test in this file is
//! `#[ignore]`d for flakiness.

use mova::embed::{Engine, Profile, Value as EValue};
use mova::internal::{Interp, Value};

fn eval_ok(src: &str) -> Value {
    let mut interp = Interp::new();
    interp
        .eval_str("l4_supervision_test", src)
        .unwrap_or_else(|e| panic!("eval error for {src:?}: {}", mova::internal::render(&e, "l4_supervision_test", src)))
}

/// `pr_str` of an `eval_ok` result -- flow_test.rs's own `ps` helper,
/// duplicated here rather than shared (this file has no dependency on that
/// one, by the same "each `tests/*.rs` is its own crate" reasoning every
/// other flow test file's near-identical `eval_ok`/`ps` pair already lives
/// with).
fn ps(src: &str) -> String {
    mova::internal::pr_str(&eval_ok(src))
}

/// Every assertion below is computed IN MOVA (a plain-keyed result map of
/// booleans/values, `=`-compared against the `::clojure.core.async.flow`
/// fields directly) and read back here -- `tests/flow_test.rs`'s own house
/// idiom, which sidesteps ever having to reconstruct a namespaced
/// `Value::Keyword` by hand on the Rust side.
fn get_bool(m: &Value, k: &str) -> bool {
    let Value::Map(m) = m else { panic!("expected a map, got {m:?}") };
    match m.get(&Value::Keyword(k.into())) {
        Some(Value::Bool(b)) => *b,
        other => panic!("expected {k} to be a bool, got {other:?}"),
    }
}

/// An `Engine` with the panicking host native every crash test calls from a
/// `:transition`. `Profile::Scripting` is what gives the script `flow/*` and
/// `chan`/`alts!!` at once.
fn crash_engine() -> Engine {
    let mut engine = Engine::builder().profile(Profile::Scripting).build();
    engine.register_fn("test/boom", |_args: &[EValue]| {
        panic!(
            "l4_supervision_test: deliberate panic exercising call_transition's unguarded path \
             (the ONE proc-killing panic reachable from Mova)"
        )
    });
    engine
}

/// Runs `src` on a fresh crash-capable engine and returns its result map.
fn crash_eval(src: &str) -> EValue {
    let mut engine = crash_engine();
    let result = engine.eval(src).unwrap_or_else(|e| panic!("eval error: {e}"));
    // Every program below stops its own flow; `shutdown` is the end-to-end
    // check that a crashed/restarted/given-up flow does not wedge teardown.
    let report = engine.shutdown();
    assert_eq!(report.flows_failed, 0, "a supervised flow must never fail process teardown");
    result
}

fn ebool(v: &EValue, k: &str) -> bool {
    v.get_kw(k).and_then(|x| x.as_bool()).unwrap_or_else(|| panic!("expected {k} to be a bool, got {:?}", v.get_kw(k)))
}

/// The crashing step, as a Mova expression: a proc that counts messages and
/// dies (a genuine Rust panic, unwinding past `ExitGuard`) the moment it is
/// asked to pause.
const CRASH_STEP: &str = r#"(flow/map->step
     {:describe (fn [] {:ins {:in {}} :outs {}})
      :init (fn [args] {:n 0})
      :transition (fn [s t] (if (= t :clojure.core.async.flow/pause) (test/boom) s))
      :transform (fn [s _ m] [(update s :n inc) {}])})"#;

// ===========================================================================
// 1. The death event reaches report-chan, with the full shape -- and a
//    `:max-restarts 0` policy gives up on the first crash rather than
//    restarting (the smallest complete supervision story: detect, classify,
//    decide, report).
// ===========================================================================

#[test]
fn a_crash_reports_proc_exit_then_proc_give_up_with_the_full_event_shapes() {
    let result = crash_eval(&format!(
        r#"(let [fl (flow/create-flow
                     {{:procs {{:p {{:proc (flow/process {CRASH_STEP})
                                  :supervision {{:policy :restart :max-restarts 0}}}}}}
                      :conns []}})
                 chans (flow/start fl)
                 report (:report-chan chans)
                 _ (flow/resume fl)
                 _ (flow/pause-proc fl :p)
                 e1 (first (alts!! [report (timeout 3000)]))
                 e2 (first (alts!! [report (timeout 3000)]))
                 e3 (first (alts!! [report (timeout 300)]))]
             (flow/stop fl)
             {{:e1-op (= (:clojure.core.async.flow/op e1) :proc-exit)
               :e1-reason (= (:clojure.core.async.flow/reason e1) :panicked)
               :e1-pid (= (:clojure.core.async.flow/pid e1) :p)
               :e1-incarnation (= (:clojure.core.async.flow/incarnation e1) 0)
               :e2-op (= (:clojure.core.async.flow/op e2) :proc-give-up)
               :e2-pid (= (:clojure.core.async.flow/pid e2) :p)
               :e2-pids (= (:clojure.core.async.flow/pids e2) [:p])
               :e2-restarts (= (:clojure.core.async.flow/restarts e2) 0)
               :nothing-else (nil? e3)}})"#
    ));
    assert!(ebool(&result, "e1-op"), "the first report-chan event must be the :proc-exit");
    assert!(ebool(&result, "e1-reason"), "a genuine panic must classify as :panicked");
    assert!(ebool(&result, "e1-pid"), "the event must name the pid that died");
    assert!(ebool(&result, "e1-incarnation"), "the first incarnation of a run is 0");
    assert!(ebool(&result, "e2-op"), ":max-restarts 0 must give up rather than restart");
    assert!(ebool(&result, "e2-pid"), ":proc-give-up names the run's head pid");
    assert!(ebool(&result, "e2-pids"), ":pids is the whole run -- a run of one here");
    assert!(ebool(&result, "e2-restarts"), "no restarts had happened when the window filled");
    assert!(ebool(&result, "nothing-else"), "a given-up proc must not be restarted afterwards");
}

// ===========================================================================
// 2. G-RESTART's headline: crash -> restart -> the proc WORKS again, with a
//    fresh incarnation number and freshly `init`-ed state.
// ===========================================================================

#[test]
fn a_crashed_proc_is_restarted_and_the_new_incarnation_processes_messages() {
    let result = crash_eval(
        r#"(let [out-ch (chan 10)
                 step (flow/map->step
                       {:describe (fn [] {:ins {:in {}} :outs {}})
                        :init (fn [args] {:n 0})
                        :transition (fn [s t] (if (= t :clojure.core.async.flow/pause) (test/boom) s))
                        :transform (fn [s _ m]
                                     (let [s2 (update s :n inc)]
                                       (>!! out-ch (:n s2))
                                       [s2 {}]))})
                 fl (flow/create-flow
                     {:procs {:p {:proc (flow/process step)
                                  :supervision {:policy :restart :backoff {:initial-ms 0}}}}
                      :conns []})
                 chans (flow/start fl)
                 report (:report-chan chans)
                 _ (flow/resume fl)
                 _ (flow/inject fl [:p :in] [:before])
                 v-before (first (alts!! [out-ch (timeout 3000)]))
                 _ (flow/pause-proc fl :p)
                 e1 (first (alts!! [report (timeout 3000)]))
                 e2 (first (alts!! [report (timeout 3000)]))
                 _ (flow/resume fl)
                 _ (flow/inject fl [:p :in] [:after])
                 v-after (first (alts!! [out-ch (timeout 3000)]))]
             (flow/stop fl)
             {:v-before (= v-before 1)
              :e1-exit (= (:clojure.core.async.flow/op e1) :proc-exit)
              :e2-restart (= (:clojure.core.async.flow/op e2) :proc-restart)
              :e2-incarnation (= (:clojure.core.async.flow/incarnation e2) 1)
              :e2-delay (= (:clojure.core.async.flow/delay-ms e2) 0)
              :e2-pid (= (:clojure.core.async.flow/pid e2) :p)
              :v-after (= v-after 1)})"#,
    );
    assert!(ebool(&result, "v-before"), "the proc must work before it is killed");
    assert!(ebool(&result, "e1-exit"), "expected the death event first");
    assert!(ebool(&result, "e2-restart"), "expected a :proc-restart event");
    assert!(ebool(&result, "e2-incarnation"), "the restarted run is incarnation 1");
    assert!(ebool(&result, "e2-delay"), ":delay-ms must report the backoff actually used");
    assert!(ebool(&result, "e2-pid"), ":proc-restart names the run's head pid");
    // 1, not 2: state rebuilds from `args` via `init` (design §3.4, no state
    // handoff in L4) -- and the message flowed at all, which is the whole
    // point: the SAME persistent in-chan reached the new incarnation.
    assert!(ebool(&result, "v-after"), "the restarted proc must process messages, with freshly init-ed state");
}

// ===========================================================================
// 3. The backoff is honored -- asserted as an ORDER across two coarse
//    windows (nothing here measures a duration).
// ===========================================================================

/// With `:initial-ms 800`, the restart must NOT have happened 300ms after the
/// death event, and must have happened within 3s of it. Both bounds are ~4x
/// clear of the 800ms they straddle, which is what makes this survive a
/// loaded machine without being vacuous.
#[test]
fn the_restart_waits_out_its_backoff_before_respawning() {
    let result = crash_eval(&format!(
        r#"(let [fl (flow/create-flow
                     {{:procs {{:p {{:proc (flow/process {CRASH_STEP})
                                  :supervision {{:policy :restart
                                               :backoff {{:initial-ms 800 :factor 1.0 :max-ms 800}}}}}}}}
                      :conns []}})
                 chans (flow/start fl)
                 report (:report-chan chans)
                 _ (flow/resume fl)
                 _ (flow/pause-proc fl :p)
                 death (first (alts!! [report (timeout 3000)]))
                 too-early (first (alts!! [report (timeout 300)]))
                 restart (first (alts!! [report (timeout 3000)]))]
             (flow/stop fl)
             {{:death (= (:clojure.core.async.flow/op death) :proc-exit)
               :nothing-at-300ms (nil? too-early)
               :restart (= (:clojure.core.async.flow/op restart) :proc-restart)
               :delay-reported (= (:clojure.core.async.flow/delay-ms restart) 800)}})"#
    ));
    assert!(ebool(&result, "death"), "expected the death event first");
    assert!(ebool(&result, "nothing-at-300ms"), "an 800ms backoff must not have respawned 300ms in");
    assert!(ebool(&result, "restart"), "the restart must arrive once the backoff expires");
    assert!(ebool(&result, "delay-reported"), ":delay-ms must be the delay the policy chose");
}

// ===========================================================================
// 4. Give-up: the window, and both `:on-give-up` modes.
// ===========================================================================

/// `:max-restarts 1` -> the first crash restarts, the second gives up. Pins
/// the incarnation bookkeeping across a real restart too: the SECOND death
/// carries incarnation 1, which is what makes it a live event rather than a
/// stale one.
#[test]
fn the_second_crash_inside_the_window_exhausts_max_restarts_and_gives_up() {
    let result = crash_eval(&format!(
        r#"(let [fl (flow/create-flow
                     {{:procs {{:p {{:proc (flow/process {CRASH_STEP})
                                  :supervision {{:policy :restart :max-restarts 1
                                               :backoff {{:initial-ms 0 :factor 1.0 :max-ms 0}}}}}}}}
                      :conns []}})
                 chans (flow/start fl)
                 report (:report-chan chans)
                 _ (flow/resume fl)
                 _ (flow/pause-proc fl :p)
                 e1 (first (alts!! [report (timeout 3000)]))
                 e2 (first (alts!! [report (timeout 3000)]))
                 _ (flow/pause-proc fl :p)
                 e3 (first (alts!! [report (timeout 3000)]))
                 e4 (first (alts!! [report (timeout 3000)]))
                 e5 (first (alts!! [report (timeout 300)]))]
             (flow/stop fl)
             {{:e1 (= (:clojure.core.async.flow/op e1) :proc-exit)
               :e2 (= (:clojure.core.async.flow/op e2) :proc-restart)
               :e3 (= (:clojure.core.async.flow/op e3) :proc-exit)
               :e3-incarnation (= (:clojure.core.async.flow/incarnation e3) 1)
               :e4 (= (:clojure.core.async.flow/op e4) :proc-give-up)
               :e4-restarts (= (:clojure.core.async.flow/restarts e4) 1)
               :nothing-else (nil? e5)}})"#
    ));
    assert!(ebool(&result, "e1"), "crash 1 -> :proc-exit");
    assert!(ebool(&result, "e2"), "crash 1 -> :proc-restart (one restart is allowed)");
    assert!(ebool(&result, "e3"), "crash 2 -> :proc-exit");
    assert!(ebool(&result, "e3-incarnation"), "the second death is incarnation 1, not a stale 0");
    assert!(ebool(&result, "e4"), "crash 2 -> :proc-give-up (the window is full)");
    assert!(ebool(&result, "e4-restarts"), ":restarts reports what was spent inside the window");
    assert!(ebool(&result, "nothing-else"), "a given-up run is never restarted again");
}

/// `:on-give-up :stop-flow` -- the fail-fast graph's choice (owner ruling #3
/// makes `:report` the DEFAULT precisely because supervision must never crash
/// a flow that did not ask for it). Also the end-to-end proof that the stop
/// runs from its own OS thread: the supervisor cannot call `stop_flow_cell`
/// inline, because that call waits on the supervisor's own done-cell.
#[test]
fn on_give_up_stop_flow_actually_stops_the_flow() {
    let result = crash_eval(&format!(
        r#"(let [fl (flow/create-flow
                     {{:procs {{:p {{:proc (flow/process {CRASH_STEP})
                                  :supervision {{:policy :restart :max-restarts 0
                                               :on-give-up :stop-flow}}}}}}
                      :conns []}})
                 chans (flow/start fl)
                 report (:report-chan chans)
                 _ (flow/resume fl)
                 _ (flow/pause-proc fl :p)
                 e1 (first (alts!! [report (timeout 3000)]))
                 e2 (first (alts!! [report (timeout 3000)]))
                 stopped (loop [n 120]
                           (if (zero? n)
                             false
                             (if (= :stopped (try (flow/pause fl) :running (catch e :stopped)))
                               true
                               (do (<!! (timeout 25)) (recur (dec n))))))]
             {{:e1 (= (:clojure.core.async.flow/op e1) :proc-exit)
               :e2 (= (:clojure.core.async.flow/op e2) :proc-give-up)
               :stopped stopped}})"#
    ));
    assert!(ebool(&result, "e1"), "expected the death event");
    assert!(ebool(&result, "e2"), "expected the give-up event");
    assert!(ebool(&result, "stopped"), ":on-give-up :stop-flow must actually stop the flow");
}

// ===========================================================================
// 5. Selectivity: only faults, only supervised runs.
// ===========================================================================

/// **Every proc's death is REPORTED; only a supervised one is restarted.**
/// W1 ships every pid's exit to `sup_chan` on purpose (design §3.2/D-A axiom
/// 4: the supervisor sees the whole flow), so `decide`'s "no config -> ignore"
/// arm is load-bearing rather than defensive -- this is that arm, end to end.
#[test]
fn an_unsupervised_pids_death_is_reported_but_never_restarted() {
    let result = crash_eval(&format!(
        r#"(let [fl (flow/create-flow
                     {{:procs {{:p {{:proc (flow/process {CRASH_STEP})
                                  :supervision {{:policy :restart}}}}
                              :q {{:proc (flow/process {CRASH_STEP})}}}}
                      :conns []}})
                 chans (flow/start fl)
                 report (:report-chan chans)
                 _ (flow/resume fl)
                 _ (flow/pause-proc fl :q)
                 e1 (first (alts!! [report (timeout 3000)]))
                 e2 (first (alts!! [report (timeout 500)]))]
             (flow/stop fl)
             {{:e1-exit (= (:clojure.core.async.flow/op e1) :proc-exit)
               :e1-pid (= (:clojure.core.async.flow/pid e1) :q)
               :e1-reason (= (:clojure.core.async.flow/reason e1) :panicked)
               :no-restart (nil? e2)}})"#
    ));
    assert!(ebool(&result, "e1-exit"), "an unsupervised proc's death is still reported");
    assert!(ebool(&result, "e1-pid"), "and it names the unsupervised pid");
    assert!(ebool(&result, "e1-reason"), "and carries its real reason");
    assert!(ebool(&result, "no-restart"), "an unsupervised pid must never be restarted");
}

/// **The incident invariant (D-B, INVIOLABLE).** A `transform` THROW is a
/// business error, not a proc exit: error-chan map, NO death event, proc
/// continues with its state. `:supervision` is turned ON specifically so that
/// supervision seeing NOTHING is the differential -- an unsupervised proc
/// could not emit an event either way. Unchanged in intent from W1; only the
/// observation point moved (report-chan, not the removed `:supervision-chan`).
#[test]
fn transform_throw_is_an_incident_not_a_death_even_when_supervised() {
    let result = eval_ok(
        r#"(let [step (flow/map->step
                        {:describe (fn [] {:ins {:in {}} :outs {:out {}}})
                         :init (fn [_] {:n 0})
                         :transform (fn [s _ m]
                                      (if (= m :boom)
                                        (throw "kaboom")
                                        (let [s2 (update s :n inc)]
                                          [s2 {:out [(:n s2)]}])))})
                  sink-ch (chan 10)
                  sink (flow/map->step
                        {:describe (fn [] {:ins {:in {}} :outs {}})
                         :transform (fn [s _ m] (>!! sink-ch m) [s {}])})
                  fl (flow/create-flow
                      {:procs {:e {:proc (flow/process step)
                                   :supervision {:policy :restart}}
                               :s {:proc (flow/process sink)}}
                       :conns [[[:e :out] [:s :in]]]})
                  chans (flow/start fl)
                  report (:report-chan chans)
                  _ (flow/resume fl)
                  _ (flow/inject fl [:e :in] [1 :boom 2])
                  v1 (<!! sink-ch)
                  err (<!! (:error-chan chans))
                  ;; bounded read: pins that NO lifecycle event EVER arrives
                  ;; for this incident -- 300ms is generous slack for the
                  ;; error report + inject round trip above, nowhere near
                  ;; enough for a human to mistake it for "eventually".
                  no-death (nil? (first (alts!! [report (timeout 300)])))
                  v2 (<!! sink-ch)]
              (flow/stop fl)
              {:v1-is-1 (= v1 1)
               :v2-is-2 (= v2 2)
               :has-ex (map? (:clojure.core.async.flow/ex err))
               :err-op-step (= (:clojure.core.async.flow/op err) :step)
               :no-death no-death})"#,
    );
    assert!(get_bool(&result, "v1-is-1"), "first message must have flowed through before the throw");
    assert!(get_bool(&result, "v2-is-2"), "proc must CONTINUE after the throw, with state kept");
    assert!(get_bool(&result, "has-ex"), "expected the error-chan map's :ex to be a map");
    assert!(get_bool(&result, "err-op-step"), "expected the error-chan map's :op to be :step");
    assert!(get_bool(&result, "no-death"), "a transform THROW must produce NO lifecycle event, even when supervised");
}

// ===========================================================================
// 6. Stop ordering (design §3.6 / §W2.5) -- both halves.
// ===========================================================================

/// **Step 1b, and `ExitGuard`'s `TryPut::Closed` arm going live.** `stop`
/// closes `sup_chan` BEFORE the `::flow/stop` broadcast, so the `:stopped`
/// deaths that follow are events to a closed chan: dropped, silently, by the
/// guard's `Closed` arm -- which W1 wrote and nothing could reach until now.
///
/// The differential is exact: this flow IS supervised (a supervisor exists,
/// is parked on `sup_chan`, and mirrors everything it consumes), the procs DO
/// die during the stop, and their deaths DO reach an `ExitGuard` -- and
/// report-chan stays silent anyway. Without step 1b the supervisor would
/// still be alive to mirror them and this read would return an event. It also
/// pins the corpus's scenario-13 shape for supervised flows: nothing arrives
/// post-stop.
#[test]
fn a_supervised_stop_reports_nothing_after_step_1b() {
    let result = eval_ok(
        r#"(let [step (flow/map->step {:describe (fn [] {:ins {:in {}} :outs {}})
                                        :transform (fn [s _ m] [s {}])})
                  fl (flow/create-flow
                      {:procs {:p {:proc (flow/process step) :supervision {:policy :restart}}
                               :q {:proc (flow/process step) :supervision {:policy :restart}}}
                       :conns []})
                  chans (flow/start fl)
                  report (:report-chan chans)
                  _ (flow/resume fl)
                  quiet-while-running (nil? (first (alts!! [report (timeout 200)])))
                  _ (flow/stop fl)
                  bounded (first (alts!! [report (timeout 300)]))]
              {:quiet-while-running quiet-while-running
               :silent-post-stop (nil? bounded)})"#,
    );
    assert!(get_bool(&result, "quiet-while-running"), "a healthy supervised flow says nothing");
    assert!(
        get_bool(&result, "silent-post-stop"),
        "a supervised flow's stop-time deaths must be DROPPED (sup_chan is closed first, step 1b) -- \
         stop's own waits own the endgame"
    );
}

/// **Wall W4: stop racing a pending restart.** The proc is killed with a 3s
/// backoff pending, and `stop` arrives while the supervisor is parked on that
/// deadline. `stop` must return promptly (its own budget, not the backoff's),
/// the flow must end up `Stopped`, and the restart must NEVER happen -- the
/// supervisor sees its chan closed and exits without acting.
#[test]
fn stop_during_a_pending_restart_wins_and_no_respawn_happens() {
    let result = crash_eval(&format!(
        r#"(let [fl (flow/create-flow
                     {{:procs {{:p {{:proc (flow/process {CRASH_STEP})
                                  :supervision {{:policy :restart
                                               :backoff {{:initial-ms 3000 :factor 1.0 :max-ms 3000}}}}}}}}
                      :conns []}})
                 chans (flow/start fl)
                 report (:report-chan chans)
                 _ (flow/resume fl)
                 _ (flow/pause-proc fl :p)
                 death (first (alts!! [report (timeout 3000)]))
                 _ (flow/stop fl)
                 after (first (alts!! [report (timeout 1000)]))
                 stopped (= :stopped (try (flow/pause fl) :running (catch e :stopped)))]
             {{:death (= (:clojure.core.async.flow/op death) :proc-exit)
               :no-restart (nil? after)
               :stopped stopped}})"#
    ));
    assert!(ebool(&result, "death"), "expected the death event");
    assert!(ebool(&result, "no-restart"), "a stop must cancel a pending restart, not race it");
    assert!(ebool(&result, "stopped"), "the flow must be stopped afterwards");
}

// ===========================================================================
// 7. Wall W6: an `:io` (thread) proc restarts only on a CONFIRMED exit.
// ===========================================================================

/// A `:workload :io` proc is an OS thread, which cannot be killed -- so its
/// restart is gated on its previous incarnation's done-cell being closed
/// (design §3.4/wall W6). The observable is simply that the gate lets a
/// genuine, completed exit through: crash the thread proc, get a restart, and
/// the new thread works. (The wedge branch of that gate has no Mova trigger
/// in W2 -- nothing can hold a dead thread's cell open -- and belongs to W3's
/// escalation ladder, which is what creates unconfirmed exits in the first
/// place.)
#[test]
fn an_io_thread_proc_restarts_after_its_exit_is_confirmed() {
    let result = crash_eval(
        r#"(let [out-ch (chan 10)
                 step (flow/map->step
                       {:describe (fn [] {:ins {:in {}} :outs {}})
                        :init (fn [args] {:n 0})
                        :transition (fn [s t] (if (= t :clojure.core.async.flow/pause) (test/boom) s))
                        :transform (fn [s _ m]
                                     (let [s2 (update s :n inc)]
                                       (>!! out-ch (:n s2))
                                       [s2 {}]))})
                 fl (flow/create-flow
                     {:procs {:p {:proc (flow/process step {:workload :io})
                                  :supervision {:policy :restart :backoff {:initial-ms 0}}}}
                      :conns []})
                 chans (flow/start fl)
                 report (:report-chan chans)
                 _ (flow/resume fl)
                 _ (flow/pause-proc fl :p)
                 e1 (first (alts!! [report (timeout 3000)]))
                 e2 (first (alts!! [report (timeout 3000)]))
                 _ (flow/resume fl)
                 _ (flow/inject fl [:p :in] [:after])
                 v (first (alts!! [out-ch (timeout 3000)]))]
             (flow/stop fl)
             {:e1 (= (:clojure.core.async.flow/op e1) :proc-exit)
              :e2 (= (:clojure.core.async.flow/op e2) :proc-restart)
              :e2-incarnation (= (:clojure.core.async.flow/incarnation e2) 1)
              :works (= v 1)})"#,
    );
    assert!(ebool(&result, "e1"), "an :io proc's crash is reported like any other");
    assert!(ebool(&result, "e2"), "and it is restarted once its exit is CONFIRMED");
    assert!(ebool(&result, "e2-incarnation"), "the restarted thread run is incarnation 1");
    assert!(ebool(&result, "works"), "the new thread must process messages off the same persistent chan");
}

// ===========================================================================
// 8. W1 carry-overs: the return map, and the loud `:supervision` validation.
// ===========================================================================

#[test]
fn neither_a_supervised_nor_an_unsupervised_flow_returns_a_supervision_chan_key() {
    let result = eval_ok(
        r#"(let [step (flow/map->step {:describe (fn [] {:ins {:in {}} :outs {}})
                                        :transform (fn [s _ m] [s {}])})
                  plain (flow/create-flow {:procs {:p {:proc (flow/process step)}} :conns []})
                  sup (flow/create-flow
                       {:procs {:p {:proc (flow/process step) :supervision {:policy :restart}}}
                        :conns []})
                  a (flow/start plain)
                  b (flow/start sup)
                  _ (flow/stop plain)
                  _ (flow/stop sup)]
              {:plain-no-key (nil? (:supervision-chan a))
               :supervised-no-key (nil? (:supervision-chan b))
               :both-report (and (some? (:report-chan a)) (some? (:report-chan b)))
               :both-error (and (some? (:error-chan a)) (some? (:error-chan b)))})"#,
    );
    assert!(get_bool(&result, "plain-no-key"), "an unsupervised flow's return map is unchanged");
    assert!(
        get_bool(&result, "supervised-no-key"),
        ":supervision-chan was W1 scaffolding and is removed in W2 (§W2.6) -- the supervisor is that \
         chan's only reader now"
    );
    assert!(get_bool(&result, "both-report"), "report-chan is still returned by both");
    assert!(get_bool(&result, "both-error"), "error-chan is still returned by both");
}

#[test]
fn unsupervised_flow_report_chan_is_silent_post_stop() {
    let result = eval_ok(
        r#"(let [step (flow/map->step {:describe (fn [] {:ins {:in {}} :outs {}})
                                        :transform (fn [s _ m] [s {}])})
                  fl (flow/create-flow {:procs {:p {:proc (flow/process step)}} :conns []})
                  chans (flow/start fl)
                  _ (flow/resume fl)
                  _ (flow/stop fl)
                  bounded (first (alts!! [(:report-chan chans) (timeout 300)]))]
              {:report-chan-silent (nil? bounded)})"#,
    );
    assert!(
        get_bool(&result, "report-chan-silent"),
        "an unsupervised flow's report-chan must stay silent post-stop -- corpus scenario 13's pin"
    );
}

#[test]
fn stop_is_unaffected_when_unsupervised() {
    assert_eq!(
        ps(r#"(let [step (flow/map->step {:describe (fn [] {:ins {:in {}} :outs {}})
                                           :transform (fn [s _ m] [s {}])})
                     fl (flow/create-flow {:procs {:p {:proc (flow/process step)}} :conns []})
                     _ (flow/start fl)
                     _ (flow/resume fl)]
                 (flow/stop fl)
                 :done)"#),
        ":done"
    );
}

#[test]
fn unrecognized_supervision_key_is_a_loud_create_flow_error() {
    let mut interp = Interp::new();
    let err = interp.eval_str(
        "l4_supervision_test",
        r#"(flow/create-flow
            {:procs {:p {:proc (flow/map->step {:describe (fn [] {:ins {} :outs {}})
                                                 :transform (fn [s _ m] [s {}])})
                         :supervision {:policy :restart :made-up-key 1}}}
             :conns []})"#,
    );
    assert!(err.is_err(), "an unrecognized :supervision key must be a create-flow error");
}

#[test]
fn wrong_typed_supervision_value_is_a_loud_create_flow_error() {
    let mut interp = Interp::new();
    let err = interp.eval_str(
        "l4_supervision_test",
        r#"(flow/create-flow
            {:procs {:p {:proc (flow/map->step {:describe (fn [] {:ins {} :outs {}})
                                                 :transform (fn [s _ m] [s {}])})
                         :supervision {:policy :restart :max-restarts "five"}}}
             :conns []})"#,
    );
    assert!(err.is_err(), ":max-restarts \"five\" (not an int) must be a create-flow error");
}

#[test]
fn default_backoff_and_grace_apply_when_omitted() {
    assert_eq!(
        ps(r#"(let [step (flow/map->step {:describe (fn [] {:ins {} :outs {}})
                                           :transform (fn [s _ m] [s {}])})
                     fl (flow/create-flow
                         {:procs {:p {:proc (flow/process step) :supervision {:policy :restart}}}
                          :conns []})]
                 (flow/start fl)
                 (flow/stop fl)
                 :done)"#),
        ":done"
    );
}

// ===========================================================================
// 9. Out-of-process workers: the two claims that need a process-wide
//    `OnceLock` set to something other than its default (`MOVA_FUSE_ALL`,
//    `MOVA_NO_SUPERVISION`). Child processes for exactly the reason
//    `tests/l3_task_procs_test.rs` gives for its own: a switch read once per
//    process cannot have two values in one test binary.
// ===========================================================================

/// Runs one `#[ignore]`d child worker below in a fresh process with `envs`
/// set, and returns its stdout.
fn run_child(worker: &str, envs: &[(&str, &str)]) -> String {
    let exe = std::env::current_exe().expect("current_exe");
    let mut cmd = std::process::Command::new(exe);
    cmd.args([worker, "--exact", "--ignored", "--nocapture"]);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let out = cmd.output().unwrap_or_else(|e| panic!("failed to spawn the L4 child worker {worker}: {e}"));
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "the L4 child worker {worker} failed (envs={envs:?}); status={:?}\nstdout={stdout}\nstderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    stdout
}

/// One `NAME=value` line the worker printed.
fn field<'a>(stdout: &'a str, name: &str) -> &'a str {
    stdout
        .lines()
        .find_map(|l| l.strip_prefix(name))
        .unwrap_or_else(|| panic!("the L4 child worker did not print {name}\nstdout={stdout}"))
        .trim()
}

/// **A fused run is ONE death and ONE restart.** k members emit k `:proc-exit`
/// events (the run is the death unit -- one reason, one incarnation, fanned
/// out to every member's done-cell), and the supervisor's dedup means the
/// FIRST of them decides while the rest are duplicates. The whole run is then
/// respawned as a single task.
///
/// `MOVA_FUSE_ALL=1` is the only way to fuse an INTERPRETED chain (the
/// default policy fuses only runs of native steps -- and a native step's
/// transition is Rust, so it cannot be made to panic from Mova at all).
#[test]
fn a_fused_runs_death_is_one_decision_and_one_restart() {
    let out = run_child("l4_fused_restart_child", &[("MOVA_FUSE_ALL", "1")]);
    assert_eq!(field(&out, "L4_TASKS_AT_START="), "1", "the two procs must have been FUSED into one task\n{out}");
    assert_eq!(field(&out, "L4_EXITS="), "2", "a 2-member run's death emits one event per member\n{out}");
    assert_eq!(field(&out, "L4_RESTARTS="), "1", "...and exactly ONE of them decides the restart\n{out}");
    assert_eq!(field(&out, "L4_INCARNATION="), "1", "the whole run comes back as incarnation 1\n{out}");
    assert_eq!(field(&out, "L4_TASKS_AT_RESTART="), "1", "the restarted run is one task again\n{out}");
}

#[test]
#[ignore = "child worker, driven by a_fused_runs_death_is_one_decision_and_one_restart"]
fn l4_fused_restart_child() {
    let before = mova::runtime::tasks_spawned();
    let mut engine = crash_engine();
    let result = engine
        .eval(
            r#"(let [head (flow/map->step
                            {:describe (fn [] {:ins {:in {}} :outs {:out {}}})
                             :init (fn [args] {:n 0})
                             :transition (fn [s t] (if (= t :clojure.core.async.flow/pause) (test/boom) s))
                             :transform (fn [s _ m] [s {:out [m]}])})
                     tail (flow/map->step
                           {:describe (fn [] {:ins {:in {}} :outs {}})
                            :init (fn [args] {:n 0})
                            :transform (fn [s _ m] [s {}])})
                     fl (flow/create-flow
                         {:procs {:a {:proc (flow/process head)
                                      :supervision {:policy :restart :backoff {:initial-ms 0}}}
                                  :b {:proc (flow/process tail)}}
                          :conns [[[:a :out] [:b :in]]]})
                     chans (flow/start fl)
                     report (:report-chan chans)
                     _ (flow/resume fl)
                     _ (flow/pause-proc fl :a)
                     evs (loop [n 4 acc []]
                           (if (zero? n)
                             acc
                             (let [e (first (alts!! [report (timeout 3000)]))]
                               (if (nil? e) acc (recur (dec n) (conj acc e))))))]
                 (flow/stop fl)
                 {:exits (count (filter (fn [e] (= (:clojure.core.async.flow/op e) :proc-exit)) evs))
                  :restarts (count (filter (fn [e] (= (:clojure.core.async.flow/op e) :proc-restart)) evs))
                  :incarnation (:clojure.core.async.flow/incarnation
                                (first (filter (fn [e] (= (:clojure.core.async.flow/op e) :proc-restart)) evs)))
                  :members (count (:clojure.core.async.flow/pids
                                   (first (filter (fn [e] (= (:clojure.core.async.flow/op e) :proc-restart)) evs))))})"#,
        )
        .unwrap_or_else(|e| panic!("child eval error: {e}"));
    // Two flow tasks would mean two runs (no fusion); one means the pair
    // shares a task, which is what makes this a FUSED-run test. Measured
    // across the whole program, which spawns exactly one supervisor task and
    // one restarted incarnation on top of the initial run.
    let after = mova::runtime::tasks_spawned();
    let exits = result.get_kw("exits").and_then(|v| v.as_i64()).unwrap_or(-1);
    let restarts = result.get_kw("restarts").and_then(|v| v.as_i64()).unwrap_or(-1);
    let incarnation = result.get_kw("incarnation").and_then(|v| v.as_i64()).unwrap_or(-1);
    let members = result.get_kw("members").and_then(|v| v.as_i64()).unwrap_or(-1);
    // initial run (1) + supervisor (1) + the restarted run (1) = 3 tasks; the
    // two named fields below split that into the two claims that matter.
    println!("L4_TASKS_AT_START={}", if after - before >= 2 { 1 } else { 0 });
    println!("L4_TASKS_AT_RESTART={}", after - before - 2);
    println!("L4_EXITS={exits}");
    println!("L4_RESTARTS={restarts}");
    println!("L4_INCARNATION={incarnation}");
    println!("L4_MEMBERS={members}");
    engine.shutdown();
}

/// **Supervision works in the KILL-SWITCH world too.** Under
/// `MOVA_FLOW_THREAD_PROCS=1` every proc is an OS thread again (the pre-L3
/// engine), but the supervisor is still a plain runtime TASK -- design §3.3
/// is unconditional about that, and this is the test that says so out loud.
/// It also drives the `:io`-shaped confirmed-exit gate (wall W6) for a whole
/// flow rather than one proc, since in that world EVERY run is a thread run.
#[test]
fn supervision_restarts_a_proc_in_the_thread_world_too() {
    let out = run_child("l4_thread_world_child", &[("MOVA_FLOW_THREAD_PROCS", "1")]);
    assert_eq!(field(&out, "L4_EXIT="), "true", "a thread proc's crash must be reported\n{out}");
    assert_eq!(field(&out, "L4_RESTART="), "true", "...and restarted, kill switch or not\n{out}");
    assert_eq!(field(&out, "L4_WORKS="), "true", "...and the new thread must process messages\n{out}");
}

#[test]
#[ignore = "child worker, driven by supervision_restarts_a_proc_in_the_thread_world_too"]
fn l4_thread_world_child() {
    let mut engine = crash_engine();
    let result = engine
        .eval(
            r#"(let [out-ch (chan 10)
                     step (flow/map->step
                           {:describe (fn [] {:ins {:in {}} :outs {}})
                            :init (fn [args] {:n 0})
                            :transition (fn [s t] (if (= t :clojure.core.async.flow/pause) (test/boom) s))
                            :transform (fn [s _ m]
                                         (let [s2 (update s :n inc)]
                                           (>!! out-ch (:n s2))
                                           [s2 {}]))})
                     fl (flow/create-flow
                         {:procs {:p {:proc (flow/process step)
                                      :supervision {:policy :restart :backoff {:initial-ms 0}}}}
                          :conns []})
                     chans (flow/start fl)
                     report (:report-chan chans)
                     _ (flow/resume fl)
                     _ (flow/pause-proc fl :p)
                     e1 (first (alts!! [report (timeout 3000)]))
                     e2 (first (alts!! [report (timeout 3000)]))
                     _ (flow/resume fl)
                     _ (flow/inject fl [:p :in] [:after])
                     v (first (alts!! [out-ch (timeout 3000)]))]
                 (flow/stop fl)
                 {:exit (= (:clojure.core.async.flow/op e1) :proc-exit)
                  :restart (= (:clojure.core.async.flow/op e2) :proc-restart)
                  :works (= v 1)})"#,
        )
        .unwrap_or_else(|e| panic!("child eval error: {e}"));
    for k in ["exit", "restart", "works"] {
        println!("L4_{}={}", k.to_uppercase(), result.get_kw(k).and_then(|v| v.as_bool()).unwrap_or(false));
    }
    engine.shutdown();
}

/// **The kill switch.** `MOVA_NO_SUPERVISION=1` parses and validates the
/// `:supervision` map exactly as usual (a malformed one is still a loud
/// error) and then DROPS the resolved config, so the flow runs completely
/// unsupervised: a crash produces no event, no restart, nothing.
#[test]
fn the_kill_switch_drops_supervision_entirely() {
    let out = run_child("l4_kill_switch_child", &[("MOVA_NO_SUPERVISION", "1")]);
    assert_eq!(field(&out, "L4_EVENT="), "none", "MOVA_NO_SUPERVISION=1 must leave the flow unsupervised\n{out}");
    assert_eq!(field(&out, "L4_BAD_CFG_ERRORS="), "true", "validation must still be loud under the kill switch\n{out}");
}

#[test]
#[ignore = "child worker, driven by the_kill_switch_drops_supervision_entirely"]
fn l4_kill_switch_child() {
    let mut engine = crash_engine();
    let result = engine
        .eval(&format!(
            r#"(let [fl (flow/create-flow
                          {{:procs {{:p {{:proc (flow/process {CRASH_STEP})
                                       :supervision {{:policy :restart}}}}}}
                           :conns []}})
                      chans (flow/start fl)
                      report (:report-chan chans)
                      _ (flow/resume fl)
                      _ (flow/pause-proc fl :p)
                      ev (first (alts!! [report (timeout 500)]))
                      bad (try (flow/create-flow
                                 {{:procs {{:p {{:proc (flow/process {CRASH_STEP})
                                              :supervision {{:policy :restart :nonsense 1}}}}}}
                                  :conns []}})
                               false
                               (catch e true))]
                  (flow/stop fl)
                  {{:event (nil? ev) :bad bad}})"#
        ))
        .unwrap_or_else(|e| panic!("child eval error: {e}"));
    let silent = result.get_kw("event").and_then(|v| v.as_bool()).unwrap_or(false);
    let bad = result.get_kw("bad").and_then(|v| v.as_bool()).unwrap_or(false);
    println!("L4_EVENT={}", if silent { "none" } else { "some" });
    println!("L4_BAD_CFG_ERRORS={bad}");
    engine.shutdown();
}

// ===========================================================================
// 10. L4 W3: `flow/stop-proc`, the escalation ladder, and the kill.
// ===========================================================================

/// A step whose `transform` announces itself on `ready` and then BLOCKS
/// FOREVER on `hang`, a chan nobody ever feeds.
///
/// This is the wedge shape, and it is the only one Mova can build: a proc
/// parked inside `transform` never reaches its control-check point, so a
/// `::flow/stop` sits unread on its control chan for as long as the proc
/// lives. It is also, precisely, a task PARKED on a chan -- which is what
/// makes it killable (a task spinning in a loop, or blocked inside a
/// blocking native, is unreachable by design and gets `:proc-wedged`).
///
/// `extra_clauses` is spliced in as leading `cond` clauses by the tests that
/// need a second behaviour out of the same proc; the hang is the `:else`.
fn hang_step(extra_clauses: &str) -> String {
    format!(
        r#"(flow/map->step
     {{:describe (fn [] {{:ins {{:in {{}}}} :outs {{}}}})
       :init (fn [args] {{:n 0}})
       :transform (fn [s _ m]
                    (cond
                      {extra_clauses}
                      :else (do (>!! ready :parked)
                                (<!! hang)
                                [s {{}}])))}})"#
    )
}

/// **Rung 1: the graceful stop.** A healthy proc honors `::flow/stop` off its
/// control chan exactly as it honors `flow/stop`'s broadcast, so `stop-proc`
/// on one costs no kill at all -- and, being the user's intent rather than a
/// fault, it must NOT restart however aggressive the `:supervision` policy
/// is. The event is `:proc-stopped`, distinct from `:proc-give-up` (the
/// supervisor ran out of patience) and from `:proc-exit` (the death itself).
#[test]
fn stop_proc_stops_one_proc_gracefully_and_never_restarts_it() {
    let result = eval_ok(
        r#"(let [out-ch (chan 10)
                 step (flow/map->step
                       {:describe (fn [] {:ins {:in {}} :outs {}})
                        :init (fn [args] {:n 0})
                        :transform (fn [s _ m] (>!! out-ch m) [s {}])})
                 fl (flow/create-flow
                     {:procs {:p {:proc (flow/process step)
                                  :supervision {:policy :restart :max-restarts 5
                                                :backoff {:initial-ms 0 :factor 1.0 :max-ms 0}}}}
                      :conns []})
                 chans (flow/start fl)
                 report (:report-chan chans)
                 _ (flow/resume fl)
                 _ (flow/inject fl [:p :in] [:before])
                 before (first (alts!! [out-ch (timeout 3000)]))
                 _ (flow/stop-proc fl :p)
                 e1 (first (alts!! [report (timeout 3000)]))
                 e2 (first (alts!! [report (timeout 3000)]))
                 e3 (first (alts!! [report (timeout 400)]))
                 _ (flow/inject fl [:p :in] [:after])
                 after (first (alts!! [out-ch (timeout 400)]))]
             (flow/stop fl)
             {:before (= before :before)
              :e1-op (= (:clojure.core.async.flow/op e1) :proc-exit)
              :e1-reason (= (:clojure.core.async.flow/reason e1) :stopped)
              :e2-op (= (:clojure.core.async.flow/op e2) :proc-stopped)
              :e2-reason (= (:clojure.core.async.flow/reason e2) :stopped)
              :e2-pids (= (:clojure.core.async.flow/pids e2) [:p])
              :nothing-else (nil? e3)
              :really-stopped (nil? after)})"#,
    );
    assert!(get_bool(&result, "before"), "the proc must work before it is stopped");
    assert!(get_bool(&result, "e1-op"), "the death itself is still a :proc-exit");
    assert!(get_bool(&result, "e1-reason"), "rung 1 needs no kill: the proc honored control");
    assert!(get_bool(&result, "e2-op"), "a stop-proc'd run reports :proc-stopped");
    assert!(get_bool(&result, "e2-reason"), ":reason says WHICH rung of the ladder ended it");
    assert!(get_bool(&result, "e2-pids"), ":pids is the whole run");
    assert!(get_bool(&result, "nothing-else"), "a stop-proc'd run must NOT restart, whatever the policy says");
    assert!(get_bool(&result, "really-stopped"), "...and must not process messages afterwards");
}

/// **Rung 2: the kill.** The proc is parked inside `transform` on a chan
/// nobody feeds, so the `::flow/stop` rung 1 delivered will never be read.
/// After `:grace-ms` the supervisor force-unwinds the task where it stands;
/// the exit reason is `:killed` (which is `ExitGuard::drop` asking
/// `runtime::current_task_is_killed()`, the W3 mechanism), and the run still
/// does not restart, because the USER asked for this one.
#[test]
fn stop_proc_escalates_to_a_kill_when_the_proc_ignores_control() {
    let result = eval_ok(&format!(
        r#"(let [ready (chan 10)
                 hang (chan 1)
                 step {step}
                 fl (flow/create-flow
                     {{:procs {{:p {{:proc (flow/process step)
                                  :supervision {{:policy :restart :grace-ms 200
                                                :backoff {{:initial-ms 0 :factor 1.0 :max-ms 0}}}}}}}}
                      :conns []}})
                 chans (flow/start fl)
                 report (:report-chan chans)
                 _ (flow/resume fl)
                 _ (flow/inject fl [:p :in] [:hang])
                 parked (first (alts!! [ready (timeout 3000)]))
                 _ (flow/stop-proc fl :p)
                 e1 (first (alts!! [report (timeout 5000)]))
                 e2 (first (alts!! [report (timeout 3000)]))
                 e3 (first (alts!! [report (timeout 400)]))]
             (flow/stop fl)
             {{:parked (= parked :parked)
               :e1-op (= (:clojure.core.async.flow/op e1) :proc-exit)
               :e1-reason (= (:clojure.core.async.flow/reason e1) :killed)
               :e1-incarnation (= (:clojure.core.async.flow/incarnation e1) 0)
               :e2-op (= (:clojure.core.async.flow/op e2) :proc-stopped)
               :e2-reason (= (:clojure.core.async.flow/reason e2) :killed)
               :no-restart (nil? e3)}})"#,
        step = hang_step("")
    ));
    assert!(get_bool(&result, "parked"), "the proc must actually reach the park before we stop it");
    assert!(get_bool(&result, "e1-op"), "the kill produces an ordinary :proc-exit event");
    assert!(
        get_bool(&result, "e1-reason"),
        "a force-unwound proc must render :killed, not :panicked -- the exit-reason mechanism is \
         `runtime::current_task_is_killed()` read from ExitGuard::drop"
    );
    assert!(get_bool(&result, "e1-incarnation"), "it was the first incarnation that died");
    assert!(get_bool(&result, "e2-op"), "the ladder ends in :proc-stopped");
    assert!(get_bool(&result, "e2-reason"), ":reason :killed is how an operator sees rung 2 was used");
    assert!(get_bool(&result, "no-restart"), "a killed-by-stop-proc run must not come back");
}

/// **P5a's whole story, at flow level: a killed proc is not a black hole.**
///
/// The proc dies PARKED on `hang` as a task taker, so its waiter is still
/// queued on that chan when it dies -- the corpse the tombstone protocol
/// exists for. Pre-P5a-bis, the very next put on that chan was delivered into
/// the dead task's cell and reported as SENT (the probe measured 512/512 puts
/// swallowed); with the claim in place the deliverer finds a non-`Waiting`
/// cell, culls the corpse, and the value reaches the buffer instead.
///
/// The observable is deliberately end-to-end and Mova-level: put on the chan
/// the dead proc was reading, then read it back. A swallowed message shows up
/// as a `nil` from the bounded read.
#[test]
fn a_killed_procs_chan_corpse_does_not_swallow_a_peers_message() {
    let result = eval_ok(&format!(
        r#"(let [ready (chan 10)
                 hang (chan 1)
                 step {step}
                 fl (flow/create-flow
                     {{:procs {{:p {{:proc (flow/process step)
                                  :supervision {{:policy :restart :grace-ms 200}}}}}}
                      :conns []}})
                 chans (flow/start fl)
                 report (:report-chan chans)
                 _ (flow/resume fl)
                 _ (flow/inject fl [:p :in] [:hang])
                 parked (first (alts!! [ready (timeout 3000)]))
                 _ (flow/stop-proc fl :p)
                 e1 (first (alts!! [report (timeout 5000)]))
                 e2 (first (alts!! [report (timeout 3000)]))
                 sent (>!! hang :payload)
                 got (first (alts!! [hang (timeout 2000)]))]
             (flow/stop fl)
             {{:parked (= parked :parked)
               :killed (= (:clojure.core.async.flow/reason e1) :killed)
               :stopped (= (:clojure.core.async.flow/op e2) :proc-stopped)
               :sent sent
               :survived (= got :payload)}})"#,
        step = hang_step("")
    ));
    assert!(get_bool(&result, "parked"), "the proc must be parked ON THE CHAN when it is killed");
    assert!(get_bool(&result, "killed"), "the proc must have been killed, not have exited on its own");
    assert!(get_bool(&result, "stopped"), "the ladder must have completed");
    assert!(get_bool(&result, "sent"), "the put must report success");
    assert!(
        get_bool(&result, "survived"),
        "a killed taker's corpse must NOT swallow the next message on that chan -- this is the \
         tombstone claim (P5a-bis) end to end through the flow engine"
    );
}

/// **S3, the differential.** A killed proc unwinds THROUGH the same
/// `catch_unwind` that implements the incident invariant, and the two must
/// not be confused in either direction:
/// - a `throw` in `transform` is an incident: error-chan map, proc CONTINUES
///   (the D-B invariant, which L4 does not touch);
/// - a kill is not: the payload is re-raised, so no phantom transform error
///   is ever reported, and the proc really dies.
///
/// Both directions in ONE program, on ONE proc, so nothing can pass by
/// testing two different shapes.
#[test]
fn a_killed_proc_reports_no_phantom_transform_error() {
    let result = eval_ok(&format!(
        r#"(let [ready (chan 10)
                 hang (chan 1)
                 out-ch (chan 10)
                 step {step}
                 fl (flow/create-flow
                     {{:procs {{:p {{:proc (flow/process step)
                                  :supervision {{:policy :restart :grace-ms 200}}}}}}
                      :conns []}})
                 chans (flow/start fl)
                 report (:report-chan chans)
                 error (:error-chan chans)
                 _ (flow/resume fl)
                 _ (flow/inject fl [:p :in] [:boom])
                 incident (first (alts!! [error (timeout 3000)]))
                 _ (flow/inject fl [:p :in] [:alive])
                 alive (first (alts!! [out-ch (timeout 3000)]))
                 _ (flow/inject fl [:p :in] [:hang])
                 parked (first (alts!! [ready (timeout 3000)]))
                 _ (flow/stop-proc fl :p)
                 e1 (first (alts!! [report (timeout 5000)]))
                 e2 (first (alts!! [report (timeout 3000)]))
                 phantom (first (alts!! [error (timeout 500)]))]
             (flow/stop fl)
             {{:incident (some? incident)
               :incident-has-ex (some? (:clojure.core.async.flow/ex incident))
               :alive (= alive :alive)
               :parked (= parked :parked)
               :killed (= (:clojure.core.async.flow/reason e1) :killed)
               :stopped (= (:clojure.core.async.flow/op e2) :proc-stopped)
               :no-phantom (nil? phantom)}})"#,
        step = hang_step(
            r#"(= m :boom) (throw (ex-info "deliberate incident" {}))
                      (= m :alive) (do (>!! out-ch :alive) [s {}])"#
        )
    ));
    assert!(get_bool(&result, "incident"), "a transform throw must still reach the error chan");
    assert!(get_bool(&result, "incident-has-ex"), "...with the ::flow/ex the error contract promises");
    assert!(get_bool(&result, "alive"), "...and the proc must have survived it (the D-B invariant)");
    assert!(get_bool(&result, "parked"), "the proc must reach its park before the kill");
    assert!(get_bool(&result, "killed"), "the kill must land");
    assert!(get_bool(&result, "stopped"), "the ladder must complete");
    assert!(
        get_bool(&result, "no-phantom"),
        "S3: a killed proc's forced unwind must be RE-RAISED at the transform catch_unwind, not \
         reported as a transform error -- a phantom incident nobody caused"
    );
}

/// **Unsupervised: graceful-only, and silent.** `stop-proc` needs no
/// supervisor to do its first rung, and it must not conjure one: there is no
/// escalation, no kill, and (corpus scenario 13's pin) report-chan stays
/// silent for a flow that never asked for supervision.
#[test]
fn stop_proc_on_an_unsupervised_flow_is_graceful_only_and_silent() {
    let result = eval_ok(
        r#"(let [out-ch (chan 10)
                 step (flow/map->step
                       {:describe (fn [] {:ins {:in {}} :outs {}})
                        :transform (fn [s _ m] (>!! out-ch m) [s {}])})
                 fl (flow/create-flow
                     {:procs {:p {:proc (flow/process step)}
                              :q {:proc (flow/process step)}}
                      :conns []})
                 chans (flow/start fl)
                 report (:report-chan chans)
                 _ (flow/resume fl)
                 _ (flow/stop-proc fl :p)
                 quiet (first (alts!! [report (timeout 400)]))
                 _ (flow/inject fl [:p :in] [:to-p])
                 p-out (first (alts!! [out-ch (timeout 400)]))
                 _ (flow/inject fl [:q :in] [:to-q])
                 q-out (first (alts!! [out-ch (timeout 3000)]))
                 unknown (try (flow/stop-proc fl :nope) false (catch e true))]
             (flow/stop fl)
             {:silent (nil? quiet)
              :p-stopped (nil? p-out)
              :q-alive (= q-out :to-q)
              :unknown-pid-errors unknown})"#,
    );
    assert!(get_bool(&result, "silent"), "an unsupervised flow's report-chan must stay silent");
    assert!(get_bool(&result, "p-stopped"), "the stopped proc must stop processing");
    assert!(get_bool(&result, "q-alive"), "...and its peers must be untouched");
    assert!(get_bool(&result, "unknown-pid-errors"), "stop-proc must reject an unknown pid, like pause-proc");
}

/// **Wall W6 / owner ruling #4: an `:io` proc is never killed.** An OS thread
/// cannot be force-unwound, so the ladder has no second rung for one: the
/// grace window expires and the supervisor says `:proc-wedged` rather than
/// pretending. No `:proc-exit` ever arrives (the thread is still sitting in
/// its `<!!`), which is exactly what "wedged" means.
///
/// The slowest test in this file by design: `flow/stop` then spends its full
/// `STOP_JOIN_TIMEOUT` waiting on the wedged thread before detaching it,
/// which is the pre-existing honest behaviour this wave does not change.
#[test]
fn stop_proc_on_a_wedged_io_proc_reports_proc_wedged_and_never_kills() {
    let result = eval_ok(&format!(
        r#"(let [ready (chan 10)
                 hang (chan 1)
                 step {step}
                 fl (flow/create-flow
                     {{:procs {{:p {{:proc (flow/process step {{:workload :io}})
                                  :supervision {{:policy :restart :grace-ms 200}}}}}}
                      :conns []}})
                 chans (flow/start fl)
                 report (:report-chan chans)
                 _ (flow/resume fl)
                 _ (flow/inject fl [:p :in] [:hang])
                 parked (first (alts!! [ready (timeout 3000)]))
                 _ (flow/stop-proc fl :p)
                 e1 (first (alts!! [report (timeout 5000)]))
                 e2 (first (alts!! [report (timeout 400)]))]
             (flow/stop fl)
             {{:parked (= parked :parked)
               :e1-op (= (:clojure.core.async.flow/op e1) :proc-wedged)
               :e1-pid (= (:clojure.core.async.flow/pid e1) :p)
               :nothing-else (nil? e2)}})"#,
        step = hang_step("")
    ));
    assert!(get_bool(&result, "parked"), "the thread proc must be wedged in its transform");
    assert!(
        get_bool(&result, "e1-op"),
        "an :io proc past its grace window is a :proc-wedged and nothing else -- OS threads are \
         unkillable and the engine says so instead of pretending"
    );
    assert!(get_bool(&result, "e1-pid"), ":proc-wedged names the pid");
    assert!(get_bool(&result, "nothing-else"), "a wedged run is terminal: no kill, no restart");
}

// ===========================================================================
// 11. L4 W5: auto-resume (owner ruling #5, docs/L4-SUPERVISION-DESIGN.md §8).
//
// **The crash-trigger wrinkle, named up front.** Every crash trigger above
// (`CRASH_STEP`) panics on `::flow/pause`, reached via `flow/pause-proc` --
// and `pause-proc` is exactly one of the four natives that records
// `FlowRuntime::desired`, to `:paused`. So a test built on `CRASH_STEP`
// cannot ALSO be a "crash while the desired state is :running" test: the
// very call that kills the proc is the same call that marks it paused. This
// is not a test-authoring inconvenience, it is the genuine semantic
// interaction owner ruling #5 anticipates ("a proc the user deliberately
// paused stays paused... even with `:auto-resume true`") -- the crash-while-
// paused test below (`a_crash_while_paused_does_not_auto_resume`) uses that
// very fact as ITS trigger, on purpose, and says so.
//
// The crash-while-RUNNING headline needs a DIFFERENT trigger: a step whose
// transition panics on `::flow/resume` instead, paired with a native that
// panics only the FIRST time it is called (`test/boom-once`/
// `crash_once_engine`) -- so the SAME `::flow/resume` command shape that
// crashes incarnation 0 (the user's real `flow/resume` call, which is also
// what records `desired = :running`) goes through cleanly when the
// supervisor synthesizes it again for incarnation 1. Without the
// once-only trap, auto-resume would just re-trigger the same crash forever.
// ===========================================================================

/// Like [`crash_engine`], but the panic fires only the FIRST time
/// `test/boom-once` is called and is a harmless no-op every time after.
/// Fresh per call (a fresh `AtomicBool`), so each test that uses it gets its
/// own independent one-shot budget.
fn crash_once_engine() -> Engine {
    let mut engine = Engine::builder().profile(Profile::Scripting).build();
    let fired = std::sync::atomic::AtomicBool::new(false);
    engine.register_fn("test/boom-once", move |_args: &[EValue]| {
        if !fired.swap(true, std::sync::atomic::Ordering::SeqCst) {
            panic!(
                "l4_supervision_test: deliberate ONE-SHOT panic exercising call_transition's \
                 unguarded path -- fires once, then goes quiet"
            );
        }
        Ok(().into())
    });
    engine
}

/// [`crash_eval`]'s twin for [`crash_once_engine`].
fn crash_once_eval(src: &str) -> EValue {
    let mut engine = crash_once_engine();
    let result = engine.eval(src).unwrap_or_else(|e| panic!("eval error: {e}"));
    let report = engine.shutdown();
    assert_eq!(report.flows_failed, 0, "a supervised flow must never fail process teardown");
    result
}

/// **The self-healing headline.** The ONLY lifecycle call this test ever
/// makes is the first `flow/resume` -- which crashes the proc (the
/// transition panics on this very `::flow/resume`) AND records `desired =
/// :running` in the same breath. `:auto-resume` defaults `true`, so the
/// restart must synthesize its own `::flow/resume` with zero further
/// operator action, and the fresh incarnation must actually process a
/// freshly injected message -- not merely report a `:resumed true` and stay
/// inert.
#[test]
fn a_crash_while_running_auto_resumes_and_the_new_incarnation_needs_zero_operator_action() {
    let result = crash_once_eval(
        r#"(let [out-ch (chan 10)
                 step (flow/map->step
                       {:describe (fn [] {:ins {:in {}} :outs {}})
                        :init (fn [args] {:n 0})
                        :transition (fn [s t] (if (= t :clojure.core.async.flow/resume) (do (test/boom-once) s) s))
                        :transform (fn [s _ m]
                                     (let [s2 (update s :n inc)]
                                       (>!! out-ch (:n s2))
                                       [s2 {}]))})
                 fl (flow/create-flow
                     {:procs {:p {:proc (flow/process step)
                                  :supervision {:policy :restart :backoff {:initial-ms 0}}}}
                      :conns []})
                 chans (flow/start fl)
                 report (:report-chan chans)
                 _ (flow/resume fl)
                 e1 (first (alts!! [report (timeout 3000)]))
                 e2 (first (alts!! [report (timeout 3000)]))
                 ;; ZERO operator action from here: no flow/resume, no
                 ;; pause-proc -- just use the flow as if nothing happened.
                 _ (flow/inject fl [:p :in] [:after])
                 v-after (first (alts!! [out-ch (timeout 3000)]))]
             (flow/stop fl)
             {:e1-op (= (:clojure.core.async.flow/op e1) :proc-exit)
              :e1-reason (= (:clojure.core.async.flow/reason e1) :panicked)
              :e2-op (= (:clojure.core.async.flow/op e2) :proc-restart)
              :e2-incarnation (= (:clojure.core.async.flow/incarnation e2) 1)
              :e2-resumed (= (:clojure.core.async.flow/resumed e2) true)
              :v-after (= v-after 1)})"#,
    );
    assert!(ebool(&result, "e1-op"), "expected the death event first");
    assert!(ebool(&result, "e1-reason"), "the ONE-SHOT boom must have fired as a genuine panic");
    assert!(ebool(&result, "e2-op"), "expected a :proc-restart event");
    assert!(ebool(&result, "e2-incarnation"), "the restarted run is incarnation 1");
    assert!(
        ebool(&result, "e2-resumed"),
        ":auto-resume default true + desired :running (set by the SAME flow/resume call that crashed \
         it) must synthesize a ::flow/resume for the fresh incarnation"
    );
    assert!(
        ebool(&result, "v-after"),
        "the self-healing headline: the restarted incarnation must process a freshly injected message \
         with ZERO operator action taken after the crash"
    );
}

/// **Crash-while-paused stays paused** -- and, per this section's module
/// doc, the crash trigger IS the pause: `pause-proc` both kills the proc
/// (via `CRASH_STEP`'s `::flow/pause` panic) and records `desired =
/// :paused` for the very same pid, in the very same call. That is not a
/// coincidence this test works around, it is owner ruling #5's rule made
/// concrete: the pause was real user intent, so the restart must not
/// override it just because `:auto-resume` defaults `true`.
#[test]
fn a_crash_while_paused_does_not_auto_resume() {
    let result = crash_eval(
        r#"(let [out-ch (chan 10)
                 step (flow/map->step
                       {:describe (fn [] {:ins {:in {}} :outs {}})
                        :init (fn [args] {:n 0})
                        :transition (fn [s t] (if (= t :clojure.core.async.flow/pause) (test/boom) s))
                        :transform (fn [s _ m]
                                     (let [s2 (update s :n inc)]
                                       (>!! out-ch (:n s2))
                                       [s2 {}]))})
                 fl (flow/create-flow
                     {:procs {:p {:proc (flow/process step)
                                  :supervision {:policy :restart :backoff {:initial-ms 0}}}}
                      :conns []})
                 chans (flow/start fl)
                 report (:report-chan chans)
                 _ (flow/resume fl)
                 _ (flow/inject fl [:p :in] [:before])
                 v-before (first (alts!! [out-ch (timeout 3000)]))
                 ;; THE crash trigger -- pause-proc, which panics AND marks
                 ;; :p's desired state :paused in the same call.
                 _ (flow/pause-proc fl :p)
                 e1 (first (alts!! [report (timeout 3000)]))
                 e2 (first (alts!! [report (timeout 3000)]))
                 _ (flow/inject fl [:p :in] [:while-paused])
                 quiet (first (alts!! [out-ch (timeout 400)]))
                 _ (flow/resume fl)
                 v-after (first (alts!! [out-ch (timeout 3000)]))]
             (flow/stop fl)
             {:v-before (= v-before 1)
              :e1-reason (= (:clojure.core.async.flow/reason e1) :panicked)
              :e2-op (= (:clojure.core.async.flow/op e2) :proc-restart)
              :e2-incarnation (= (:clojure.core.async.flow/incarnation e2) 1)
              :not-resumed (= (:clojure.core.async.flow/resumed e2) false)
              :stayed-paused (nil? quiet)
              :works-after-explicit-resume (some? v-after)})"#,
    );
    assert!(ebool(&result, "v-before"), "the proc must work before it is paused/killed");
    assert!(ebool(&result, "e1-reason"), "expected the panic classification");
    assert!(ebool(&result, "e2-op"), "expected a :proc-restart event");
    assert!(ebool(&result, "e2-incarnation"), "the restarted run is incarnation 1");
    assert!(
        ebool(&result, "not-resumed"),
        "desired state was :paused (recorded by the SAME pause-proc call that crashed the proc), so \
         auto-resume must NOT fire even though :auto-resume defaults true"
    );
    assert!(
        ebool(&result, "stayed-paused"),
        "the restarted incarnation must not process messages until the user explicitly resumes it"
    );
    assert!(ebool(&result, "works-after-explicit-resume"), "an explicit flow/resume must still work afterwards");
}

/// **`pause-proc`'d pid stays paused across restart while the FLOW ITSELF
/// stays running.** A peer proc (`:q`, unsupervised, no relation to the
/// crash) keeps flowing the whole time, and `flow/ping` -- which errors iff
/// the flow is not `Running` -- succeeds right after the restart, both
/// pinning that the flow-level phase is untouched by a single pid's
/// crash-while-paused.
#[test]
fn pause_proc_stays_paused_across_restart_while_the_flow_stays_running() {
    let result = crash_eval(&format!(
        r#"(let [q-out (chan 10)
                 crashy {CRASH_STEP}
                 steady (flow/map->step
                         {{:describe (fn [] {{:ins {{:in {{}}}} :outs {{}}}})
                          :transform (fn [s _ m] (>!! q-out m) [s {{}}])}})
                 fl (flow/create-flow
                     {{:procs {{:p {{:proc (flow/process crashy)
                                  :supervision {{:policy :restart :backoff {{:initial-ms 0}}}}}}
                              :q {{:proc (flow/process steady)}}}}
                      :conns []}})
                 chans (flow/start fl)
                 report (:report-chan chans)
                 _ (flow/resume fl)
                 _ (flow/pause-proc fl :p)
                 e1 (first (alts!! [report (timeout 3000)]))
                 e2 (first (alts!! [report (timeout 3000)]))
                 ;; The flow-level phase probe: `flow/ping` errors iff the
                 ;; flow is not Running, and (unlike flow/pause) never calls
                 ;; a step's `:transition` at all -- `apply_control`'s "ping"
                 ;; arm only replies on the reply-chan -- so it cannot
                 ;; re-trigger `crashy`'s pause-panic the way probing with
                 ;; flow/pause would.
                 still-running (try (do (flow/ping fl 500) :running) (catch e :stopped))
                 _ (flow/inject fl [:q :in] [:peer-alive])
                 peer (first (alts!! [q-out (timeout 3000)]))]
             (flow/stop fl)
             {{:e2-op (= (:clojure.core.async.flow/op e2) :proc-restart)
               :not-resumed (= (:clojure.core.async.flow/resumed e2) false)
               :flow-still-running (= still-running :running)
               :peer-alive (= peer :peer-alive)}})"#,
        CRASH_STEP = CRASH_STEP
    ));
    assert!(ebool(&result, "e2-op"), "expected a :proc-restart event for :p");
    assert!(ebool(&result, "not-resumed"), ":p must not auto-resume: its own pause-proc recorded :paused");
    assert!(ebool(&result, "flow-still-running"), "the FLOW's own phase must be untouched by :p's crash");
    assert!(ebool(&result, "peer-alive"), "an unrelated peer proc must keep working the whole time");
}

/// **`:auto-resume false` opts out even when desired state IS `:running`.**
/// Same one-shot resume-crashing trigger as the headline test, so `desired`
/// really is `:running` at restart time -- the only variable changed is the
/// cfg flag, proving it (not some accidental desired-state effect) is what
/// suppresses the resume.
#[test]
fn auto_resume_false_opts_out_even_when_desired_state_is_running() {
    let result = crash_once_eval(
        r#"(let [out-ch (chan 10)
                 step (flow/map->step
                       {:describe (fn [] {:ins {:in {}} :outs {}})
                        :init (fn [args] {:n 0})
                        :transition (fn [s t] (if (= t :clojure.core.async.flow/resume) (do (test/boom-once) s) s))
                        :transform (fn [s _ m]
                                     (let [s2 (update s :n inc)]
                                       (>!! out-ch (:n s2))
                                       [s2 {}]))})
                 fl (flow/create-flow
                     {:procs {:p {:proc (flow/process step)
                                  :supervision {:policy :restart :auto-resume false
                                                :backoff {:initial-ms 0}}}}
                      :conns []})
                 chans (flow/start fl)
                 report (:report-chan chans)
                 _ (flow/resume fl)
                 e1 (first (alts!! [report (timeout 3000)]))
                 e2 (first (alts!! [report (timeout 3000)]))
                 _ (flow/inject fl [:p :in] [:should-not-flow])
                 quiet (first (alts!! [out-ch (timeout 400)]))
                 _ (flow/resume fl)
                 v-after (first (alts!! [out-ch (timeout 3000)]))]
             (flow/stop fl)
             {:e1-reason (= (:clojure.core.async.flow/reason e1) :panicked)
              :e2-op (= (:clojure.core.async.flow/op e2) :proc-restart)
              :not-resumed (= (:clojure.core.async.flow/resumed e2) false)
              :stayed-paused (nil? quiet)
              :works-after-explicit-resume (some? v-after)})"#,
    );
    assert!(ebool(&result, "e1-reason"), "expected the one-shot panic");
    assert!(ebool(&result, "e2-op"), "expected a :proc-restart event");
    assert!(
        ebool(&result, "not-resumed"),
        ":auto-resume false must opt out even though desired state is :running (the very flow/resume \
         call that crashed the proc also recorded that intent)"
    );
    assert!(ebool(&result, "stayed-paused"), "the restarted incarnation must not auto-process despite :running intent");
    assert!(ebool(&result, "works-after-explicit-resume"), "an explicit flow/resume must still work afterwards");
}

#[test]
fn wrong_typed_auto_resume_value_is_a_loud_create_flow_error() {
    let mut interp = Interp::new();
    let err = interp.eval_str(
        "l4_supervision_test",
        r#"(flow/create-flow
            {:procs {:p {:proc (flow/map->step {:describe (fn [] {:ins {} :outs {}})
                                                 :transform (fn [s _ m] [s {}])})
                         :supervision {:policy :restart :auto-resume "nope"}}}
             :conns []})"#,
    );
    assert!(err.is_err(), ":auto-resume \"nope\" (not a bool) must be a create-flow error");
}

/// **A fused run resumes as a UNIT: one paused member is enough to keep the
/// whole run paused.** `:a` is supervised and crashed via `pause-proc`
/// (recording `desired[:a] = :paused`); `:b` is unsupervised but was swept
/// `:running` by the earlier `flow/resume` broadcast (which sets every
/// declared pid, supervised or not). The run's cfg comes from `:a` (the
/// only member with one) and defaults `:auto-resume true` -- but the
/// restart must still NOT resume, because `:b`'s desired state disagrees.
/// `MOVA_FUSE_ALL=1` child-worker, same reason as
/// `a_fused_runs_death_is_one_decision_and_one_restart`: fusing an
/// INTERPRETED chain needs the global switch, a process-wide `OnceLock`.
#[test]
fn a_fused_runs_restart_stays_paused_when_any_member_was_paused() {
    let out = run_child("l4_fused_half_paused_child", &[("MOVA_FUSE_ALL", "1")]);
    assert_eq!(field(&out, "L4_RESTART="), "true", "expected a :proc-restart event for the fused run\n{out}");
    assert_eq!(
        field(&out, "L4_RESUMED="),
        "false",
        "a fused run with even one member desired :paused must stay paused entirely\n{out}"
    );
}

#[test]
#[ignore = "child worker, driven by a_fused_runs_restart_stays_paused_when_any_member_was_paused"]
fn l4_fused_half_paused_child() {
    let mut engine = crash_engine();
    let result = engine
        .eval(
            r#"(let [head (flow/map->step
                            {:describe (fn [] {:ins {:in {}} :outs {:out {}}})
                             :init (fn [args] {:n 0})
                             :transition (fn [s t] (if (= t :clojure.core.async.flow/pause) (test/boom) s))
                             :transform (fn [s _ m] [s {:out [m]}])})
                     tail (flow/map->step
                           {:describe (fn [] {:ins {:in {}} :outs {}})
                            :init (fn [args] {:n 0})
                            :transform (fn [s _ m] [s {}])})
                     fl (flow/create-flow
                         {:procs {:a {:proc (flow/process head)
                                      :supervision {:policy :restart :backoff {:initial-ms 0}}}
                                  :b {:proc (flow/process tail)}}
                          :conns [[[:a :out] [:b :in]]]})
                     chans (flow/start fl)
                     report (:report-chan chans)
                     ;; Sweeps :running onto BOTH :a and :b's desired state.
                     _ (flow/resume fl)
                     ;; Crashes the fused run AND marks :a's desired :paused
                     ;; -- :b's desired stays :running from the broadcast
                     ;; above, which is what makes this a HALF-paused run.
                     _ (flow/pause-proc fl :a)
                     evs (loop [n 4 acc []]
                           (if (zero? n)
                             acc
                             (let [e (first (alts!! [report (timeout 3000)]))]
                               (if (nil? e) acc (recur (dec n) (conj acc e))))))]
                 (flow/stop fl)
                 ;; Plain-keyed on purpose (`get_kw`, on the Rust side below,
                 ;; looks up a BARE keyword -- `l4_fused_restart_child`'s own
                 ;; precedent for why the raw `::flow`-namespaced event map
                 ;; is never handed back directly).
                 (let [restart (first (filter (fn [e] (= (:clojure.core.async.flow/op e) :proc-restart)) evs))]
                   {:has-restart (some? restart)
                    :resumed (:clojure.core.async.flow/resumed restart)}))"#,
        )
        .unwrap_or_else(|e| panic!("child eval error: {e}"));
    let has_restart = result.get_kw("has-restart").and_then(|v| v.as_bool()).unwrap_or(false);
    let resumed = result.get_kw("resumed").and_then(|v| v.as_bool()).unwrap_or(true);
    println!("L4_RESTART={has_restart}");
    println!("L4_RESUMED={resumed}");
    engine.shutdown();
}
