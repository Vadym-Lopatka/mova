//! **L3.6/W1 gate suite: the SPSC transport lane on TASK procs.**
//!
//! Before this wave `plan_transport_links_with` refused a lane to any conn
//! touching a task proc, because `transport.rs`'s `Ring` parked on
//! `std::thread::Thread` identity (L3 §3.2). Since `1c00c5d` made flow procs
//! tasks by default that exclusion applied to essentially every hop, and
//! docs/FLOW-HOP-RECOVERY.md §0 priced it at 280 ns/message -- the whole
//! post-flip throughput regression. The `Ring` now has a TASK ARM: a
//! two-source park registered on the ring's waiter slot AND the proc's
//! `Doorbell`, so a lane-parked task notices data, close, control and inject
//! alike (`Ring::park_task_two_source`, and §7 of that doc).
//!
//! **Every flow in this file is transport-backed by default.** All procs are
//! `flow/map->step` (interpreted, so `plan_fusion` never groups them -- see
//! `carries_step_factory`) and default to `:workload :mixed`, i.e. TASKS.
//! Their 1:1 conns are therefore exactly the case that was ineligible before
//! this wave and is the hot path after it. Nothing here sets
//! `MOVA_FLOW_THREAD_PROCS`; the point is the DEFAULT world.
//!
//! **What is NOT here, and where it is instead.** The two-source park
//! protocol itself -- park torture at capacity 1 with spin pinned to 0, a
//! doorbell ring storm racing the ring's own wakes, a doorbell ring reaching
//! a task parked on an empty lane, and an L4 kill landing on a lane-parked
//! task -- is tested at the transport level in `src/transport/tests.rs`
//! ("THE TASK ARM"), where the flow engine is not in the way and a failure
//! points at the protocol. This file is the end-to-end half: the same
//! properties as observed through `flow/*`.

use mova::embed::{Engine, Profile, Value as EValue};
use mova::internal::{Interp, Value};

fn eval_ok(src: &str) -> Value {
    let mut interp = Interp::new();
    interp
        .eval_str("spsc_task_lane_test", src)
        .unwrap_or_else(|e| panic!("eval error: {}", mova::internal::render(&e, "spsc_task_lane_test", src)))
}

fn get_bool(m: &Value, k: &str) -> bool {
    let Value::Map(m) = m else { panic!("expected a map, got {m:?}") };
    match m.get(&Value::Keyword(k.into())) {
        Some(Value::Bool(b)) => *b,
        other => panic!("expected {k} to be a bool, got {other:?}"),
    }
}

/// Runs `body` on a helper thread under a hard deadline.
///
/// The failure mode a lane can introduce is a LOST WAKE: both sides parked
/// forever at 0% CPU, which a plain `#[test]` reports as "cargo test never
/// finished". Every test here is watchdogged so that failure arrives as a
/// failing test with a diagnostic instead. Same policy as
/// `src/transport/tests.rs` and `tests/flow_test.rs`'s subprocess watchdog,
/// except that a panic on the helper thread is re-raised here so an ordinary
/// assertion failure still reads normally.
fn watchdogged<T: Send + 'static>(what: &'static str, secs: u64, body: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)));
    });
    match rx.recv_timeout(std::time::Duration::from_secs(secs)) {
        Ok(Ok(v)) => v,
        Ok(Err(payload)) => std::panic::resume_unwind(payload),
        Err(_) => panic!(
            "{what}: watchdog fired after {secs}s. That is the lost-wake signature the two-source \
             park exists to make impossible -- see `Ring::park_task_two_source` and \
             docs/FLOW-HOP-RECOVERY.md §7."
        ),
    }
}

// ===========================================================================
// 1. High-volume lane traffic under a control + inject storm.
// ===========================================================================

/// **Gate 3(a).** 6 task procs in an unbranching chain -- so five 1:1 conns,
/// every one of them a task lane -- carrying 6000 messages through 40 rounds
/// of `pause`/`resume` while `inject` writes into the head's SIDE channel
/// concurrently with the lane's own traffic.
///
/// Three distinct things must hold at once, and each of them is a different
/// piece of the two-source park:
///
/// - **No message is lost or reordered.** Every hop is a lane, so this is the
///   ring arm (source A) end to end, including the `InLane::side_take` gate
///   that lets injected traffic interleave with lane traffic.
/// - **No control event is lost.** A `pause` landing while a proc is
///   lane-parked reaches it only through source B; if that registration were
///   missing the proc would sleep through the pause and the run would simply
///   be slow rather than wrong -- so the assertion is on the OBSERVED status
///   after each storm, not merely on delivery.
/// - **`resume` after the storm is lossless.** The paused procs re-park and
///   re-register from scratch on every lap, thousands of times over.
///
/// Run repeatedly (40 rounds) because a lost wake in this protocol is a
/// window measured in nanoseconds: one round would only catch a systematic
/// break, not a racy one.
#[test]
fn high_volume_lane_traffic_survives_a_control_and_inject_storm() {
    let result = watchdogged("lane_traffic_control_storm", 180, || {
        eval_ok(
            r#"(let [out-ch (chan 8000)
                     relay (flow/map->step
                            {:describe (fn [] {:ins {:in {}} :outs {:out {}}})
                             :init (fn [_] {:n 0})
                             :transform (fn [s _ m] [(update s :n inc) {:out [m]}])})
                     sink (flow/map->step
                           {:describe (fn [] {:ins {:in {}} :outs {}})
                            :init (fn [_] {:n 0})
                            :transform (fn [s _ m] (>!! out-ch m) [(update s :n inc) {}])})
                     fl (flow/create-flow
                         {:procs {:a {:proc (flow/process relay)}
                                  :b {:proc (flow/process relay)}
                                  :c {:proc (flow/process relay)}
                                  :d {:proc (flow/process relay)}
                                  :e {:proc (flow/process relay)}
                                  :sink {:proc (flow/process sink)}}
                          :conns [[[:a :out] [:b :in]] [[:b :out] [:c :in]]
                                  [[:c :out] [:d :in]] [[:d :out] [:e :in]]
                                  [[:e :out] [:sink :in]]]})
                     _ (flow/start fl)
                     _ (flow/resume fl)
                     rounds 40
                     per 150
                     ;; Each round: inject 150 messages, then a pause/resume
                     ;; pair landing on procs that are mid-lane-traffic.
                     ;; `@`, not a bare call: `flow/inject` is ASYNC (it
                     ;; returns a future), so two un-deref'd injections race
                     ;; each other and the ordering assertion below would be
                     ;; testing the test harness rather than the lane. This
                     ;; is a property of `inject`, not of the transport --
                     ;; verified by the same reordering appearing under
                     ;; `MOVA_NO_SPSC=1`.
                     _ (dotimes [r rounds]
                         @(flow/inject fl [:a :in] (range (* r per) (* (inc r) per)))
                         (flow/pause fl)
                         (flow/resume fl))
                     total (* rounds per)
                     deadline (timeout 60000)
                     received (loop [k total acc []]
                                (if (zero? k)
                                  acc
                                  (let [v (first (alts!! [out-ch deadline]))]
                                    (recur (dec k) (conj acc v)))))
                     ;; A control command must still be noticed after all
                     ;; that -- the source-B half, observed rather than
                     ;; assumed.
                     _ (flow/pause fl)
                     paused (get-in (flow/ping fl 3000) [:sink :clojure.core.async.flow/status])]
                 (flow/stop fl)
                 {:count (= (count received) total)
                  :no-nils (= 0 (count (filter nil? received)))
                  :in-order (= received (vec (range total)))
                  :control-noticed (= paused :paused)})"#,
        )
    });
    assert!(get_bool(&result, "count"), "a task lane lost or duplicated messages under a control storm");
    assert!(get_bool(&result, "no-nils"), "messages went missing (nil from the collection deadline)");
    assert!(
        get_bool(&result, "in-order"),
        "a task lane reordered messages -- the ring is FIFO and the side channel interleaves at lap \
         granularity, so a chain of 1:1 lanes must preserve order exactly"
    );
    assert!(
        get_bool(&result, "control-noticed"),
        "a `pause` was lost on a lane-parked task proc -- that is source B (the Doorbell arm of \
         `Ring::park_task_two_source`) failing"
    );
}

// ===========================================================================
// 2. Capacity 1: every message is a full two-source park round trip.
// ===========================================================================

/// **Gate 3(d), end to end.** `:buf-or-n 1` on every in-port turns a chain
/// into a ping-pong: the producer can never be more than one message ahead,
/// so essentially every message costs a park on the data side AND a park on
/// the room side, thousands of laps deep. This is the shape that would
/// surface a lost wake fastest -- and it is also the shape that proves
/// `SpscTx::wait_writable_or_doorbell`'s task arm works, since a chain this
/// tight spends most of its life blocked on backpressure.
///
/// The `pause`/`resume` in the middle lands while procs are blocked on a FULL
/// downstream, which is the one place a task's blocked send has no deadline
/// to fall back on: without the doorbell registration in the room direction
/// it would sleep through the pause until the downstream drained.
#[test]
fn a_capacity_one_task_chain_ping_pongs_losslessly_and_still_honours_control() {
    let result = watchdogged("lane_cap1_pingpong", 180, || {
        eval_ok(
            r#"(let [out-ch (chan 4000)
                     relay (flow/map->step
                            {:describe (fn [] {:ins {:in {}} :outs {:out {}}})
                             :init (fn [_] {:n 0})
                             :transform (fn [s _ m] [(update s :n inc) {:out [m]}])})
                     sink (flow/map->step
                           {:describe (fn [] {:ins {:in {}} :outs {}})
                            :init (fn [_] {:n 0})
                            :transform (fn [s _ m] (>!! out-ch m) [(update s :n inc) {}])})
                     one {:in {:buf-or-n 1}}
                     fl (flow/create-flow
                         {:procs {:a {:proc (flow/process relay) :chan-opts one}
                                  :b {:proc (flow/process relay) :chan-opts one}
                                  :c {:proc (flow/process relay) :chan-opts one}
                                  :sink {:proc (flow/process sink) :chan-opts one}}
                          :conns [[[:a :out] [:b :in]] [[:b :out] [:c :in]]
                                  [[:c :out] [:sink :in]]]})
                     _ (flow/start fl)
                     _ (flow/resume fl)
                     n 2000
                     ;; NOT `@`-deref'd, deliberately: `inject` blocks once
                     ;; the head's cap-1 in-chan fills, so the injection has
                     ;; to run CONCURRENTLY with the collection loop below --
                     ;; which is what keeps the whole chain pinned in the
                     ;; backpressured state the room-direction park lives in.
                     ;; (One inject, so there is no ordering hazard.)
                     _ (flow/inject fl [:a :in] (range n))
                     deadline (timeout 90000)
                     received (loop [k n acc []]
                                (if (zero? k)
                                  acc
                                  (let [v (first (alts!! [out-ch deadline]))]
                                    (recur (dec k) (conj acc v)))))
                     _ (flow/pause fl)
                     paused (get-in (flow/ping fl 3000) [:a :clojure.core.async.flow/status])]
                 (flow/stop fl)
                 {:count (= (count received) n)
                  :in-order (= received (vec (range n)))
                  :control-noticed (= paused :paused)})"#,
        )
    });
    assert!(get_bool(&result, "count"), "a capacity-1 task lane lost or duplicated messages");
    assert!(get_bool(&result, "in-order"), "a capacity-1 task lane reordered messages");
    assert!(
        get_bool(&result, "control-noticed"),
        "a `pause` was lost on a task proc blocked sending into a FULL lane -- that is the \
         room-direction doorbell registration (`SpscTx::wait_writable_or_doorbell`) failing"
    );
}

// ===========================================================================
// 3. L4: a supervised death mid-lane-traffic, and auto-resume after it.
// ===========================================================================

/// The crashing step: a proc that dies (a genuine Rust panic unwinding past
/// `ExitGuard`) on its SECOND `::flow/resume` and never again. Adapted from
/// `tests/l4_supervision_test.rs`, which is the file that owns the
/// supervision contract; here it is only the trigger, and the "second
/// resume, once" shape is what makes AUTO-resume observable:
///
/// - `pause-proc`/`resume-proc` would record the user's last wish for that
///   pid, and the supervisor's auto-resume restores exactly that -- so a
///   crash triggered by `pause-proc` restarts into a PAUSED proc, correctly,
///   and proves nothing about auto-resume. A flow-wide `resume` leaves the
///   desired state RESUMED, which is the state auto-resume has to restore.
/// - Crashing only ONCE means the restarted incarnation (auto-resumed, so it
///   takes a third `::flow/resume`) survives and can be observed working.
///
/// The `fired` atom is captured by the step fn itself, which is built once
/// and shared by every incarnation -- `init` reruns, the closure does not.
const CRASH_STEP: &str = r#"(let [resumes (atom 0)]
     (flow/map->step
      {:describe (fn [] {:ins {:in {}} :outs {:out {}}})
       :init (fn [args] {:n 0})
       :transition (fn [s t]
                     (if (= t :clojure.core.async.flow/resume)
                       (if (= 2 (swap! resumes inc)) (test/boom) s)
                       s))
       :transform (fn [s _ m] [(update s :n inc) {:out [m]}])}))"#;

fn crash_engine() -> Engine {
    let mut engine = Engine::builder().profile(Profile::Scripting).build();
    engine.register_fn("test/boom", |_args: &[EValue]| {
        panic!("spsc_task_lane_test: deliberate panic killing a supervised proc mid-lane-traffic")
    });
    engine
}

fn ebool(v: &EValue, k: &str) -> bool {
    v.get_kw(k).and_then(|x| x.as_bool()).unwrap_or_else(|| panic!("expected {k} to be a bool, got {:?}", v.get_kw(k)))
}

/// **Gates 3(b) and 3(c), at the flow level.**
///
/// The topology is `:a -> :b -> :c -> :sink`, with `:c` RESTART-supervised.
/// That makes the split explicit and asserted end to end:
///
/// - `:a -> :b` is a **task lane** (neither endpoint is supervised);
/// - `:b -> :c` and `:c -> :sink` stay on the general `Chan`, because
///   `native_start` filters out every link touching a restart-supervised proc
///   -- a `SpscRing` half dies with the incarnation that owns it, so it is
///   the one piece of wiring that cannot be handed to the next one.
///
/// So a kill/restart NEVER lands on a lane-parked proc through the engine's
/// own escalation ladder, and that is a deliberate structural property, not
/// an accident. (The runtime-level "kill a task that IS lane-parked" case --
/// which a future wave could reach by relaxing that filter -- is covered
/// directly in `src/transport/tests.rs`'s
/// `a_lane_parked_task_can_be_killed_and_leaves_nothing_dangerous_behind`.)
///
/// What this test then proves is the interaction: `:c` dies and restarts
/// WHILE the upstream lane is carrying traffic, the supervisor auto-resumes
/// the new incarnation (L4 W5, owner ruling #5 -- the user last said
/// `resume`, so the restart does not leave it paused and silent), and
/// messages injected AFTER the restart still traverse the lane hop and come
/// out the far end. A lane whose producer had been left parked against a
/// generation that moved during the restart storm would show up here as a
/// hang, which the watchdog turns into a failure.
#[test]
fn a_supervised_death_mid_lane_traffic_auto_resumes_and_the_lane_hop_keeps_delivering() {
    let result = watchdogged("lane_supervised_restart", 120, || {
        let mut engine = crash_engine();
        let result = engine
            .eval(&format!(
                r#"(let [out-ch (chan 2000)
                         relay (flow/map->step
                                {{:describe (fn [] {{:ins {{:in {{}}}} :outs {{:out {{}}}}}})
                                  :init (fn [_] {{:n 0}})
                                  :transform (fn [s _ m] [(update s :n inc) {{:out [m]}}])}})
                         sink (flow/map->step
                               {{:describe (fn [] {{:ins {{:in {{}}}} :outs {{}}}})
                                 :init (fn [_] {{:n 0}})
                                 :transform (fn [s _ m] (>!! out-ch m) [(update s :n inc) {{}}])}})
                         fl (flow/create-flow
                             {{:procs {{:a {{:proc (flow/process relay)}}
                                      :b {{:proc (flow/process relay)}}
                                      :c {{:proc (flow/process {CRASH_STEP})
                                           :supervision {{:policy :restart :backoff {{:initial-ms 0}}}}}}
                                      :sink {{:proc (flow/process sink)}}}}
                              :conns [[[:a :out] [:b :in]] [[:b :out] [:c :in]]
                                      [[:c :out] [:sink :in]]]}})
                         chans (flow/start fl)
                         report (:report-chan chans)
                         _ (flow/resume fl)
                         ;; Traffic through the :a -> :b LANE before the death.
                         _ @(flow/inject fl [:a :in] (range 300))
                         before (loop [k 300 acc []]
                                  (if (zero? k)
                                    acc
                                    (recur (dec k) (conj acc (first (alts!! [out-ch (timeout 20000)]))))))
                         ;; Kill :c. The pipeline is quiescent, so :a and :b
                         ;; are BOTH parked inside `park_task_two_source` on
                         ;; the :a -> :b lane when the death and the restart
                         ;; storm land on the flow around them.
                         _ (flow/pause fl)
                         _ (flow/resume fl)
                         e1 (first (alts!! [report (timeout 15000)]))
                         e2 (first (alts!! [report (timeout 15000)]))
                         ;; No further `flow/resume`: auto-resume is what must
                         ;; bring the new incarnation back.
                         _ @(flow/inject fl [:a :in] (range 1000 1300))
                         after (loop [k 300 acc []]
                                 (if (zero? k)
                                   acc
                                   (recur (dec k) (conj acc (first (alts!! [out-ch (timeout 20000)]))))))]
                     (flow/stop fl)
                     {{:before (= before (vec (range 300)))
                       :exit (= (:clojure.core.async.flow/op e1) :proc-exit)
                       :restart (= (:clojure.core.async.flow/op e2) :proc-restart)
                       :after (= after (vec (range 1000 1300)))}})"#
            ))
            .unwrap_or_else(|e| panic!("eval error: {e}"));
        let report = engine.shutdown();
        assert_eq!(report.flows_failed, 0, "a supervised flow with a task lane must not fail teardown");
        result
    });
    assert!(ebool(&result, "before"), "the lane hop lost messages before the supervised death");
    assert!(ebool(&result, "exit"), "expected the death event first");
    assert!(ebool(&result, "restart"), "expected a :proc-restart event");
    assert!(
        ebool(&result, "after"),
        "the lane hop stopped delivering after a supervised restart + auto-resume -- a producer \
         left parked against a stale generation is exactly what this shape would show"
    );
}

// ===========================================================================
// 4. THE REGRESSION THIS WAVE ACTUALLY INTRODUCED, pinned.
// ===========================================================================

/// **A lane-holding proc that is MULTI-INPUT at runtime.**
///
/// `plan_transport_links_with` only ever sees DECLARED ports, so it happily
/// gives `:b` a lane on its single declared `:in`. Then `:b`'s `init`
/// returns `::flow/in-ports {:extra ...}` and `:b` becomes multi-input --
/// and the multi-input round-robin's park is `Doorbell::wait_for_change`,
/// which the LANE does not ring. Engine traffic on `:in` arrives on the
/// lock-free ring; the doorbell never moves; the proc sleeps.
///
/// On a thread that was a slow lap (`PARK_TIMEOUT` = 2 s, then re-scan). The
/// first cut of this wave made task procs lane-eligible and a task park has
/// no safety net, so it became a **permanent hang** — reproduced at **23 out
/// of 30 runs** against that build, and 0/150 after
/// [`InLane::wait_readable`] made the multi-input branch park on BOTH
/// sources. `tests/flow_test.rs`'s
/// `demotion_on_init_ports_falls_back_to_ordinary_procs_and_still_delivers`
/// is what caught it; this is the same shape kept here on purpose, next to
/// the mechanism it guards, and run 25 times rather than once because 1 run
/// in 4 passed even when it was broken.
#[test]
fn a_lane_holding_proc_that_init_widens_to_multi_input_still_wakes() {
    let result = watchdogged("lane_multi_input_wake", 180, || {
        eval_ok(
            r#"(let [relay (flow/map->step
                            {:describe (fn [] {:ins {:in {}} :outs {:out {}}})
                             :init (fn [_] {:n 0})
                             :transform (fn [s _ m] [(update s :n inc) {:out [m]}])})
                     sink (flow/map->step
                           {:describe (fn [] {:ins {:in {}} :outs {}})
                            :init (fn [_] {:n 0})
                            :transform (fn [s _ m] [(update s :n inc) {}])})
                     round (fn [_]
                             (let [extra (chan 10)
                                   out-ch (chan 100)
                                   ;; :b declares ONE in-port (so it is
                                   ;; lane-eligible) and then widens itself
                                   ;; to two at init (so it takes the
                                   ;; multi-input round-robin at runtime).
                                   widened (flow/map->step
                                            {:describe (fn [] {:ins {:in {}} :outs {:out {}}})
                                             :init (fn [_] {:n 0 :clojure.core.async.flow/in-ports {:extra extra}})
                                             :transform (fn [s _ m] [(update s :n inc) {:out [m]}])})
                                   collect (flow/map->step
                                            {:describe (fn [] {:ins {:in {}} :outs {}})
                                             :init (fn [_] {:n 0})
                                             :transform (fn [s _ m] (>!! out-ch m) [(update s :n inc) {}])})
                                   fl (flow/create-flow
                                       {:procs {:a {:proc (flow/process relay)}
                                                :b {:proc (flow/process widened)}
                                                :c {:proc (flow/process collect)}}
                                        :conns [[[:a :out] [:b :in]] [[:b :out] [:c :in]]]})
                                   _ (flow/start fl)
                                   _ (flow/resume fl)
                                   ;; Engine traffic down the :a -> :b LANE,
                                   ;; and one message straight into the
                                   ;; init-supplied side port.
                                   _ @(flow/inject fl [:a :in] [1 2 3])
                                   _ (>!! extra 99)
                                   vals (loop [n 4 acc []]
                                          (if (zero? n)
                                            acc
                                            (recur (dec n) (conj acc (first (alts!! [out-ch (timeout 8000)]))))))]
                               (flow/stop fl)
                               (sort (map (fn [v] (or v -1)) vals))))
                     rounds (vec (map round (range 25)))]
                 {:all-delivered (every? (fn [r] (= r [1 2 3 99])) rounds)
                  :first (first rounds)})"#,
        )
    });
    assert!(
        get_bool(&result, "all-delivered"),
        "a lane-holding proc that init widened to multi-input lost a wake -- the multi-input \
         round-robin must park on the LANE as well as the doorbell. First round: {}",
        mova::internal::pr_str(&result)
    );
}

// ===========================================================================
// 5. The planner's structural guarantees, as end-to-end behaviour.
// ===========================================================================

/// A task lane must be OBSERVATIONALLY identical to the `Chan` it replaces --
/// the same property `tests/flow_test.rs` pins for the thread world, asserted
/// here for the world that only just became lane-eligible. `MOVA_NO_SPSC=1`
/// is the differential: same program, lanes off, byte-identical output.
#[test]
fn a_task_lane_is_observationally_identical_to_the_chan_it_replaces() {
    const PROGRAM: &str = r#"(let [out-ch (chan 4000)
             relay (flow/map->step
                    {:describe (fn [] {:ins {:in {}} :outs {:out {}}})
                     :init (fn [_] {:n 0})
                     :transform (fn [s _ m] [(update s :n inc) {:out [(* 2 m)]}])})
             sink (flow/map->step
                   {:describe (fn [] {:ins {:in {}} :outs {}})
                    :init (fn [_] {:n 0})
                    :transform (fn [s _ m] (>!! out-ch m) [(update s :n inc) {}])})
             fl (flow/create-flow
                 {:procs {:a {:proc (flow/process relay)}
                          :b {:proc (flow/process relay)}
                          :sink {:proc (flow/process sink)}}
                  :conns [[[:a :out] [:b :in]] [[:b :out] [:sink :in]]]})
             _ (flow/start fl)
             _ (flow/resume fl)
             n 1500
             _ (flow/inject fl [:a :in] (range n))
             deadline (timeout 30000)
             received (loop [k n acc []]
                        (if (zero? k)
                          acc
                          (recur (dec k) (conj acc (first (alts!! [out-ch deadline]))))))
             counts (flow/ping fl 3000)]
         (flow/stop fl)
         (println (pr-str [(= received (vec (map (fn [i] (* 4 i)) (range n))))
                           (get-in counts [:sink :clojure.core.async.flow/count])])))"#;

    let on = run_program(PROGRAM, &[]);
    let off = run_program(PROGRAM, &[("MOVA_NO_SPSC", "1")]);
    assert_eq!(on, "[true 1500]\nnil", "a task lane changed observable behaviour: {on}");
    assert_eq!(on, off, "MOVA_NO_SPSC=1 changed the answer, so the lane is not a pure substitution");
}

/// Runs `program` in a fresh `mova` subprocess with `env` applied, under a
/// watchdog (a lost wake would otherwise block on the child forever). The
/// same helper `tests/flow_test.rs` uses, reduced to what this file needs.
fn run_program(program: &str, env: &[(&str, &str)]) -> String {
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_mova"));
    cmd.arg("-e").arg(program);
    cmd.stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    for k in ["MOVA_NO_FUSION", "MOVA_FUSE_ALL", "MOVA_NO_FASTSTEP", "MOVA_NO_SPSC", "MOVA_FLOW_THREAD_PROCS"] {
        cmd.env_remove(k);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().expect("failed to spawn the mova binary");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    loop {
        match child.try_wait().expect("try_wait") {
            Some(status) => {
                let out = child.wait_with_output().expect("wait_with_output");
                assert!(
                    status.success(),
                    "mova exited {status}: {}",
                    String::from_utf8_lossy(&out.stderr)
                );
                return String::from_utf8_lossy(&out.stdout).trim_end().to_string();
            }
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                panic!("run_program: the child hung (env {env:?}) -- the lost-wake signature");
            }
            None => std::thread::sleep(std::time::Duration::from_millis(20)),
        }
    }
}
