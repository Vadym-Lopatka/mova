//! Dedicated regression suite for FLOW-IDLE-CPU-BUG.md's `Doorbell` fix
//! (`src/value.rs`'s `Doorbell`, `src/builtins/flow.rs`'s "Control-priority
//! wait design"). `tests/flow_test.rs` proves the flow engine's BEHAVIOR is
//! unchanged by the fix; this file proves the fix itself -- that an idle
//! proc no longer falls back to polling, and that a real event (data,
//! control, injection) wakes a parked proc promptly rather than after a
//! poll interval. Modeled on an earlier Rust flow library's `tests/core/control_priority.rs`
//! (a sibling tokio-based flow implementation's own control-priority/
//! backpressure regression suite) for the spirit -- direct, tight,
//! CI-enforceable assertions about the wake mechanism itself, not just
//! "the observable behavior still matches."
//!
//! ## The measurement: `Doorbell::safety_net_hits()`
//!
//! `Doorbell::wait_for_change` increments a process-wide counter
//! (`mova::internal::Doorbell::safety_net_hits()`) exactly when a park
//! call falls back to its safety-net timeout with NO ring having happened
//! (see that fn's doc in `value.rs` for the precise condition). Before the
//! fix, the equivalent event -- a `cv_wait_timeout` that always times out,
//! every `PARK_TIMEOUT`(1ms, then)/`MULTI_INPUT_BACKOFF`(200µs) -- fired
//! thousands of times per second for ANY idle proc, unconditionally: that
//! IS the bug FLOW-IDLE-CPU-BUG.md reports. After the fix, a genuinely
//! idle proc should hit this ~0 times over a multi-hundred-millisecond
//! window, since every real event rings the doorbell instead. This is
//! exactly acceptance criterion #1 ("< ~200 context switches/s and < 1%
//! CPU... sustained" at idle) expressed as something `cargo test` can
//! assert deterministically, without shelling out to `top`/`sample`.
//!
//! ## Why every test here is serialized
//!
//! `safety_net_hits()` reads a single process-global counter (see its doc
//! for why it isn't per-`Doorbell`: a black-box test only ever has the
//! `flow/*` API, never a handle to one proc's specific `Doorbell`). Tests
//! in THIS file run in the same process (`cargo test` compiles one binary
//! per `tests/*.rs`), so two of them reading/measuring that counter
//! concurrently would corrupt each other's before/after deltas. Every test
//! here therefore takes `test_lock()` for its whole body, serializing this
//! file's tests against EACH OTHER (NOT against other test binaries --
//! those are separate processes with their own counter). Each test still
//! only asserts on the DELTA over its own narrow window, never an absolute
//! value, so residual counts from an earlier test in this file (which
//! could itself legitimately tick the counter once near a 2s
//! `PARK_TIMEOUT` boundary) can't leak into a later one's assertion.
//!
//! ## Two DIFFERENT failure modes, two DIFFERENT kinds of test
//!
//! The four "idle" tests (single-input, multi-input, fused, and
//! transport-backed -- one per tier this fix touches, each checking a low
//! `safety_net_hits()` delta) and the three "wake latency" tests
//! (injected-message, control-command, read-set-recompute) are not
//! redundant -- each catches a failure the other structurally cannot:
//!
//! - The idle tests catch a regression BACK TO POLLING (`PARK_TIMEOUT`/
//!   `MULTI_INPUT_BACKOFF` shortened again, or a park loop that ignores
//!   the doorbell and busy-waits): that shows up as a burst of safety-net
//!   hits within their short idle window. They CANNOT catch "the doorbell
//!   is never registered/wired at all" -- if nothing ever rings a proc's
//!   doorbell, that proc just waits out the full (long) `PARK_TIMEOUT`
//!   safety net exactly ONCE per park, which produces at most one hit in
//!   any window shorter than ~2s; a 500ms idle window is silent either
//!   way, correctly-wired or completely broken. Verified directly, twice:
//!   neutering `resync_read_set_doorbell` (see
//!   `read_set_recompute_does_not_lose_a_wakeup`'s doc for the full
//!   experiment) left the idle tests passing, delta 0, despite the
//!   registration being completely dead; separately, shipping the
//!   transport tier's `Doorbell::ring`-also-unparks half WITHOUT the
//!   generation-check half (see
//!   `idle_transport_backed_proc_does_not_fall_back_to_polling`'s doc)
//!   would ALSO have passed every idle test while leaving control latency
//!   for that tier broken.
//! - The wake-latency tests catch exactly that blind spot: they measure
//!   wake LATENCY for a specific real event, so "parked with no live wake,
//!   silently waiting out the safety net" shows up directly as a slow or
//!   missing response. Against the neutered `resync_read_set_doorbell`,
//!   the injected-message test failed immediately (no delivery within
//!   50ms) and the read-set-recompute test hung until its watchdog fired;
//!   against the unpark-without-generation-check transport version,
//!   `tests/flow_test.rs`'s `transport_lifecycle_surface_matches_the_
//!   chan_path` (a real control-latency scenario, not duplicated in this
//!   file) failed deterministically.
//!
//! Together they cover both directions of the bug: reverting to eager
//! polling (CPU regression, the idle tests) and reverting to nothing-at-all
//! (functional/latency regression -- a message or control command that's
//! simply never noticed until a multi-second safety net expires, the
//! wake-latency tests).

use mova::internal::{Doorbell, Interp, Value};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(())).lock().unwrap_or_else(|e| e.into_inner())
}

fn eval_ok(src: &str) -> Value {
    let mut interp = Interp::new();
    interp
        .eval_str("flow_wake_test", src)
        .unwrap_or_else(|e| panic!("eval error for {src:?}: {}", mova::internal::render(&e, "flow_wake_test", src)))
}

fn eval_ok_on(interp: &mut Interp, src: &str) -> Value {
    interp
        .eval_str("flow_wake_test", src)
        .unwrap_or_else(|e| panic!("eval error for {src:?}: {}", mova::internal::render(&e, "flow_wake_test", src)))
}

fn ps(src: &str) -> String {
    mova::internal::pr_str(&eval_ok(src))
}

// ---------------------------------------------------------------------------
// 1 & 2 & 3: idle procs must not fall back to polling.
//
// Each of these starts a flow, resumes it, sends ZERO messages, sleeps
// 500ms, then stops it -- and checks `safety_net_hits()` barely moved.
// Before this fix, this exact 500ms window would have produced roughly
// 500 unconditional wakes for a single-input proc (1ms `PARK_TIMEOUT`) or
// roughly 2500 for a multi-input one (200µs `MULTI_INPUT_BACKOFF`); after
// it, the window should be silent (the tolerance of 1 below is slack for
// the very first park call's own entry into `wait_for_change`, not an
// expectation that it will actually be used in the passing case).
// ---------------------------------------------------------------------------

#[test]
fn idle_single_input_proc_does_not_fall_back_to_polling() {
    let _guard = test_lock();
    let before = Doorbell::safety_net_hits();
    eval_ok(
        r#"(let [step (flow/map->step {:describe (fn [] {:ins {:in {}} :outs {}})
                                        :transform (fn [s _ m] [s {}])})
                  fl (flow/create-flow {:procs {:p {:proc (flow/process step)}} :conns []})
                  _ (flow/start fl)
                  _ (flow/resume fl)
                  _ (sleep-ms 500)]
              (flow/stop fl)
              :done)"#,
    );
    let after = Doorbell::safety_net_hits();
    let delta = after - before;
    eprintln!("[flow_wake_test] idle_single_input_proc_does_not_fall_back_to_polling: safety_net_hits delta={delta} (before={before}, after={after})");
    assert!(
        delta <= 1,
        "expected an idle single-input proc's park to stay doorbell-driven over a 500ms idle \
         window (>=0, <=1 safety-net fallback allowed for the initial park's own entry), got {delta} \
         -- this is the direct regression signal for FLOW-IDLE-CPU-BUG.md's single-input polling path"
    );
}

#[test]
fn idle_multi_input_proc_does_not_fall_back_to_polling() {
    let _guard = test_lock();
    let before = Doorbell::safety_net_hits();
    eval_ok(
        r#"(let [step (flow/map->step {:describe (fn [] {:ins {:a {} :b {}} :outs {}})
                                        :transform (fn [s _ m] [s {}])})
                  fl (flow/create-flow {:procs {:p {:proc (flow/process step)}} :conns []})
                  _ (flow/start fl)
                  _ (flow/resume fl)
                  _ (sleep-ms 500)]
              (flow/stop fl)
              :done)"#,
    );
    let after = Doorbell::safety_net_hits();
    let delta = after - before;
    eprintln!("[flow_wake_test] idle_multi_input_proc_does_not_fall_back_to_polling: safety_net_hits delta={delta} (before={before}, after={after})");
    assert!(
        delta <= 1,
        "expected an idle multi-input proc's scan-then-park tail to stay doorbell-driven over a \
         500ms idle window (>=0, <=1 safety-net fallback allowed), got {delta} -- this is the \
         direct regression signal for FLOW-IDLE-CPU-BUG.md's multi-input scan-backoff path, a \
         DIFFERENT code path from the single-input one (needs its own test)"
    );
}

#[test]
fn idle_fused_chain_does_not_fall_back_to_polling() {
    let _guard = test_lock();
    let before = Doorbell::safety_net_hits();
    // `step-count`/`step-passthrough` are native (`StepFactory`-carrying)
    // steps, so this 3-proc 1:1 unbranching chain fuses under the DEFAULT
    // policy (`FusionPolicy::PromotedOnly`) with no env var needed -- the
    // exact same proc shapes `ping_proc_mid_chain_fused_matches_the_
    // unfused_reply_exactly` (tests/flow_test.rs) already relies on to
    // prove fusion happened. `run_fused`'s own `Doorbell` (registered on
    // the head's control/in-chan and every member's control chan -- see
    // that fn's doc) is what this test targets: it was NOT part of the
    // original fix's spec (which said "do not touch run_fused"), added as
    // a deviation because the head's boundary reads use the identical
    // `try_take_with_timeout(PARK_TIMEOUT)` poll shape any single-input
    // proc does -- unfixed, every fused chain (the DEFAULT topology
    // outcome for an unbranching pipeline) would have kept polling.
    eval_ok(
        r#"(let [fl (flow/create-flow
                     {:procs {:head {:proc (flow/process (flow/step-count))}
                              :mid {:proc (flow/process (flow/step-passthrough))}
                              :tail {:proc (flow/process (flow/step-passthrough))}}
                      :conns [[[:head :out] [:mid :in]] [[:mid :out] [:tail :in]]]})
                  _ (flow/start fl)
                  _ (flow/resume fl)
                  _ (sleep-ms 500)]
              (flow/stop fl)
              :done)"#,
    );
    let after = Doorbell::safety_net_hits();
    let delta = after - before;
    eprintln!("[flow_wake_test] idle_fused_chain_does_not_fall_back_to_polling: safety_net_hits delta={delta} (before={before}, after={after})");
    assert!(
        delta <= 1,
        "expected an idle FUSED chain's head-of-chain park to stay doorbell-driven over a 500ms \
         idle window (>=0, <=1 safety-net fallback allowed), got {delta} -- run_fused's Doorbell \
         wiring is a deviation from the original fix spec (\"do not touch run_fused\") and had no \
         coverage of its own until this test"
    );
}

// ---------------------------------------------------------------------------
// 4 & 5: a real event must wake a parked proc PROMPTLY -- not just
// "eventually, within some generous multi-second margin" like
// tests/flow_test.rs's style, but within a bound tight enough to prove the
// wake is genuine (doorbell-driven) rather than a lucky safety-net timeout
// landing near the event.
// ---------------------------------------------------------------------------

#[test]
fn injected_message_wakes_a_parked_proc_promptly() {
    let _guard = test_lock();
    let mut interp = Interp::new();
    eval_ok_on(
        &mut interp,
        r#"(def out-ch (chan 10))
           (def fl (flow/create-flow
                    {:procs {:p {:proc (flow/process
                                        (flow/map->step
                                         {:describe (fn [] {:ins {:in {}} :outs {}})
                                          :transform (fn [s _ m] (>!! out-ch m) [s {}])}))}}
                     :conns []}))
           (flow/start fl)
           (flow/resume fl)
           ;; Sleep long enough that the proc is unambiguously parked (not
           ;; mid-lap) before we measure anything.
           (sleep-ms 300)"#,
    );

    let before = Doorbell::safety_net_hits();
    let t0 = Instant::now();
    // `@` forces this call to wait for the injection thread's `chan_put` to
    // actually land before we start polling for delivery -- otherwise the
    // 50ms budget below could be spent waiting on `flow/inject` itself
    // rather than on the proc's wake latency.
    eval_ok_on(&mut interp, "@(flow/inject fl [:p :in] [42])");

    let deadline = t0 + Duration::from_millis(50);
    let mut observed: Option<(Value, Duration)> = None;
    while Instant::now() < deadline {
        let v = eval_ok_on(&mut interp, "(poll! out-ch)");
        if v != Value::Nil {
            observed = Some((v, t0.elapsed()));
            break;
        }
        std::thread::sleep(Duration::from_micros(200));
    }
    let after = Doorbell::safety_net_hits();
    eval_ok_on(&mut interp, "(flow/stop fl)");

    eprintln!(
        "[flow_wake_test] injected_message_wakes_a_parked_proc_promptly: observed={observed:?} safety_net_hits before={before} after={after}"
    );
    let (v, elapsed) = observed.expect("message was not observed within 50ms of flow/inject -- proc did not wake promptly");
    assert_eq!(v, Value::Int(42));
    assert!(elapsed <= Duration::from_millis(50), "delivery took {elapsed:?}, expected <=50ms");
    assert_eq!(
        after, before,
        "a safety-net fallback fired during the idle-then-inject window (delta {}) -- the wake \
         should have been a genuine ring, not a timeout that happened to land near the inject",
        after - before
    );
}

#[test]
fn control_command_wakes_a_proc_parked_on_data() {
    let _guard = test_lock();
    let before = Doorbell::safety_net_hits();
    // `flow/ping-proc`'s own reply-collection loop already gives a clean,
    // tight bound: pass it a SHORT timeout (50ms) right after pausing the
    // proc, and check the reply's status is `:paused`. If `pause-proc`
    // (queued on the same control chan `ping-proc` also uses) weren't
    // noticed within roughly that same window, `:p` would still be
    // `:running` (or ping-proc would time out with no reply at all) --
    // either way this fails loudly rather than silently passing.
    let result = ps(
        r#"(let [step (flow/map->step {:describe (fn [] {:ins {:in {}} :outs {}})
                                        :transform (fn [s _ m] [s {}])})
                  fl (flow/create-flow {:procs {:p {:proc (flow/process step)}} :conns []})
                  _ (flow/start fl)
                  _ (flow/resume fl)
                  _ (sleep-ms 300)
                  _ (flow/pause-proc fl :p)
                  reply (flow/ping-proc fl :p 50)]
              (flow/stop fl)
              (:clojure.core.async.flow/status reply))"#,
    );
    let after = Doorbell::safety_net_hits();
    eprintln!(
        "[flow_wake_test] control_command_wakes_a_proc_parked_on_data: result={result} safety_net_hits before={before} after={after}"
    );
    assert_eq!(
        result, ":paused",
        "pause-proc wasn't noticed within ping-proc's own 50ms reply window -- the parked proc \
         likely fell back to polling instead of waking on the control ring"
    );
    assert!(after - before <= 1, "unexpected safety-net fallback(s) during a 300ms-idle-then-control window: delta={}", after - before);
}

// ---------------------------------------------------------------------------
// 6: the subtlest correctness property -- a read-set recompute racing a
// registration must never lose a wakeup. See this test's own doc below for
// an honest account of what it can and can't prove (checked empirically
// against a deliberately broken `resync_read_set_doorbell` -- see the PR
// notes / final report for the outcome).
// ---------------------------------------------------------------------------

/// Stress-toggles `::flow/input-filter` (via a shared atom the filter
/// closure reads) between "read only `:a`" (collapses the read-set to ONE
/// chan, forcing the SINGLE-input branch and its doorbell-only
/// `try_take_with_timeout` park -- no periodic multi-chan scan to fall
/// back on) and "read `:a` and `:b`" (multi-input branch), while injecting
/// to both ports, and asserts every `:b` message is eventually observed --
/// never silently dropped -- within a bounded total window.
///
/// **Validated to actually catch a regression, not just assert a property
/// that happens to hold.** I ran this test three ways against a
/// deliberately neutered `resync_read_set_doorbell` (a no-op, so NO chan
/// -- read-set or control -- ever gets a live `Doorbell` registration,
/// including the very first one at proc start): a multi-input-shaped
/// version (read-set always 2 chans) still passed every time -- that
/// path's round-robin scan re-examines the WHOLE current read-set
/// non-blockingly every outer lap regardless of the doorbell, so it
/// doesn't exercise the registration race at all. THIS version (which
/// collapses to exactly one chan on each toggle, per the filter above) is
/// the one that actually depends on the registration: against the same
/// neutered `resync_read_set_doorbell` it did NOT cleanly fail fast --
/// instead the run became pathologically slow (the single-input branch's
/// park has no live wake at all when unregistered, so EVERY toggle back
/// into the collapsed state costs a full safety-net wait, and since
/// `flow/inject`'s underlying `chan_put` is BLOCKING once a buffer fills,
/// that slowness backpressures the injection loop itself, not just the
/// collection side) -- confirming a real functional regression, just one
/// that needs a watchdog to observe deterministically rather than hang the
/// whole test binary. Hence the watchdog thread below: it is NOT decor,
/// it is what makes this test able to fail in bounded time instead of
/// wedging `cargo test` when the registration race actually reproduces.
#[test]
fn read_set_recompute_does_not_lose_a_wakeup() {
    let _guard = test_lock();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let result = std::panic::catch_unwind(|| {
            ps(r#"(let [out-ch (chan 1000)
                        allow-b (atom true)
                        step (flow/map->step
                              {:describe (fn [] {:ins {:a {} :b {}} :outs {}})
                               :init (fn [_] {:clojure.core.async.flow/input-filter
                                              (fn [cid] (or (= cid :a) @allow-b))})
                               :transform (fn [s cid m]
                                            (if (= cid :a)
                                              (do (reset! allow-b m) [s {}])
                                              (do (>!! out-ch m) [s {}])))})
                        fl (flow/create-flow {:procs {:f {:proc (flow/process step)}} :conns []})
                        _ (flow/start fl)
                        _ (flow/resume fl)
                        n 60
                        _ (dotimes [i n]
                            @(flow/inject fl [:f :a] [false])
                            @(flow/inject fl [:f :b] [i])
                            @(flow/inject fl [:f :a] [true]))
                        ;; ONE shared deadline for the whole collection loop
                        ;; (not per-message) -- if messages start going
                        ;; missing, every iteration after the deadline
                        ;; fires drains near-instantly with `nil` instead
                        ;; of compounding N separate waits.
                        deadline (timeout 3000)
                        received (loop [k n acc []]
                                   (if (zero? k)
                                     acc
                                     (let [v (first (alts!! [out-ch deadline]))]
                                       (recur (dec k) (conj acc v)))))]
                    (flow/stop fl)
                    [(= (set received) (set (range n))) (count (filter nil? received))])"#)
        });
        let _ = tx.send(result);
    });
    // The internal `(timeout 3000)` above only bounds the COLLECTION half;
    // the injection loop that precedes it has no bound of its own (by
    // design -- see the doc comment above), so the watchdog here is a
    // SEPARATE, overall bound covering the whole thing. 15s is generous
    // slack over the sub-second time this takes when the registration is
    // working; a hang under a real regression is bounded by roughly
    // `n * PARK_TIMEOUT` (60 * 2s = up to 120s in the worst case observed
    // during validation), so this watchdog fires well before that, turning
    // "wedges cargo test" into "fails in 15s with a clear message".
    match rx.recv_timeout(Duration::from_secs(15)) {
        Ok(Ok(result)) => {
            eprintln!("[flow_wake_test] read_set_recompute_does_not_lose_a_wakeup: result={result}");
            assert_eq!(
                result, "[true 0]",
                "some :b message was lost or arrived out of the expected set while the input-filter toggled under load"
            );
        }
        Ok(Err(panic_payload)) => std::panic::resume_unwind(panic_payload),
        Err(_) => panic!(
            "read_set_recompute_does_not_lose_a_wakeup: watchdog fired -- the flow did not finish \
             within 15s, which is itself the failure signature of a lost-wakeup regression (see \
             this test's doc comment for what this looked like when reproduced against a \
             deliberately broken resync_read_set_doorbell)"
        ),
    }
}

// ---------------------------------------------------------------------------
// 7: the transport tier is no longer a gap -- prove it the same way tests
// 1-3 prove it for the other tiers.
// ---------------------------------------------------------------------------

/// The transport (SPSC-lane) tier used to be the ONE deliberate scope-limit
/// in the `Doorbell` fix (a transport-backed proc's data read stayed on a
/// short, fixed, non-`Doorbell` timeout). It no longer is:
/// `transport.rs`'s `Ring` gained additive `_or_doorbell` sibling methods
/// (`Ring::wait_for_data_until_or_doorbell`/`pop_timeout_or_doorbell`,
/// `SpscRx::take_timeout_or_doorbell`/`_cold`) that check a passed-in
/// `Doorbell`'s generation at the same cadence as their existing deadline
/// check, and `Doorbell::ring` now also calls `Thread::unpark()` on the
/// proc's own OS thread (`value.rs`) -- together, a transport-backed
/// proc's data-direction read now runs at the SAME long `PARK_TIMEOUT`
/// safety net as every other tier, genuinely wake-driven rather than
/// polling. See `builtins::flow`'s module doc, "Composition with
/// control/pause/stop", and `transport.rs`'s "BOUNDED waits" section for
/// the full mechanism and why it doesn't touch `Ring`'s existing methods.
///
/// This topology needs no `MOVA_NO_FUSION` env var: `plan_fusion` only
/// ever groups a conn into a fusable run when BOTH endpoints carry a
/// native `StepFactory` (`carries_step_factory`, checked at PLANNING time,
/// not just at runtime demotion) -- an ordinary `flow/map->step`
/// (interpreted) relay chain is therefore never a fusion candidate at
/// all, and its 1:1 conn is transport-eligible by default. (Confirmed
/// directly during development: `sample`-ing this exact shape showed the
/// downstream proc's leaf frame in `in_take_timeout`'s transport branch --
/// `park_timeout`/`semaphore_timedwait_trap` -- not
/// `try_take_with_timeout`'s `Doorbell`/`__psynch_cvwait` path.)
///
/// **Validated to actually catch the regression this closed**: before the
/// `Ring` changes above (i.e. against the version that only widened the
/// timeout and added the raw `Thread::unpark()`, with no generation check
/// inside `Ring`'s wait loop), `tests/flow_test.rs`'s
/// `transport_lifecycle_surface_matches_the_chan_path` failed
/// DETERMINISTICALLY (a transport-backed proc's `flow/ping ... 2000`
/// status came back `nil`, never observed within ping's own 2000ms
/// budget) -- direct evidence that an uncorrelated unpark alone does not
/// make this loop return early (invariant 7: it re-checks real state,
/// finds nothing changed, and re-arms `park_timeout` for the remaining
/// deadline). This test's `safety_net_hits()` delta would NOT have caught
/// that specific failure mode by itself (a control-latency problem, not a
/// polling-rate one -- see tests 1-3's doc note on this exact blind spot),
/// which is why `flow_test.rs`'s transport suite (proof requirement #1,
/// not duplicated here) is what actually pins the fix; this test's job is
/// narrower and complementary: prove the IDLE-CPU side also improved for
/// this tier, the same way tests 1-3 prove it for the others.
#[test]
fn idle_transport_backed_proc_does_not_fall_back_to_polling() {
    let _guard = test_lock();
    let before = Doorbell::safety_net_hits();
    eval_ok(
        r#"(let [relay (flow/map->step {:describe (fn [] {:ins {:in {}} :outs {:out {}}})
                                         :transform (fn [s _ m] [s {:out [m]}])})
                  sink (flow/map->step {:describe (fn [] {:ins {:in {}} :outs {}})
                                         :transform (fn [s _ m] [s {}])})
                  fl (flow/create-flow
                      {:procs {:a {:proc (flow/process relay)} :b {:proc (flow/process sink)}}
                       :conns [[[:a :out] [:b :in]]]})
                  _ (flow/start fl)
                  _ (flow/resume fl)
                  _ (sleep-ms 500)]
              (flow/stop fl)
              :done)"#,
    );
    let after = Doorbell::safety_net_hits();
    let delta = after - before;
    eprintln!(
        "[flow_wake_test] idle_transport_backed_proc_does_not_fall_back_to_polling: safety_net_hits delta={delta} (before={before}, after={after})"
    );
    assert!(
        delta <= 1,
        "expected an idle transport-backed proc's data-direction read to stay doorbell-driven \
         over a 500ms idle window (>=0, <=1 safety-net fallback allowed), got {delta} -- this is \
         the direct regression signal for the transport tier, which used to be exempt from this \
         fix entirely"
    );
}

// ---------------------------------------------------------------------------
// 8: `run_fused`'s mid-chain in-port gap. A `flow/inject` to a NON-head
// member of a fused chain must wake the run just as promptly as one to the
// head -- not just "eventually observed" (that much was already covered by
// `inject_mid_chain_fused_flows_through_the_rest_of_the_chain` in
// `flow_test.rs`, with a multi-second margin), but within the same tight
// bound `injected_message_wakes_a_parked_proc_promptly` (test 4, above)
// holds the single-proc path to.
// ---------------------------------------------------------------------------

/// `run_fused` shares ONE `Doorbell` for the whole run, but before this test
/// existed it was only registered on the head's control/in-chan (the two
/// chans the run's single read loop ever *blocks* on) and on every member's
/// control chan -- NOT on `ms[1..]`'s in-chans. A `flow/inject` straight
/// into a mid-chain port (e.g. `[:mid :in]`) is only ever drained by a
/// non-blocking scan, once per outer-loop lap (the `for idx in 1..n {
/// chan_try_take(&ms[idx].in_chan) ... }` block in `run_fused`); that lap
/// only re-runs promptly if the doorbell the head's blocking read is parked
/// on gets rung. With the head otherwise idle (no traffic to it at all) and
/// the mid-chain in-chan unregistered, nothing rang that doorbell, so the
/// injected message sat unnoticed until the head's `try_take_with_timeout`
/// safety net expired on its own -- up to a full `PARK_TIMEOUT` (2s) later.
/// Registering `ms[1..]`'s in-chans on the same shared doorbell (this
/// fix's change, alongside the head's in-chan and every member's control
/// chan) closes that gap: the inject rings the doorbell, the head's
/// blocking read wakes immediately, and the outer loop revisits the
/// mid-chain scan on its very next lap.
///
/// This test builds the exact fused topology the gap needs -- a 3-proc
/// native chain (`flow/step-count` -> `flow/step-passthrough` ->
/// `flow/step-sink-deliver`) that fuses under the DEFAULT policy, the same
/// proc shapes `idle_fused_chain_does_not_fall_back_to_polling` (test 3,
/// above) already relies on to prove fusion happened -- parks it fully idle
/// (zero traffic to `:head`), then injects directly into `[:mid :in]` and
/// asserts the tail's `step-sink-deliver` promise resolves within 50ms: a
/// bound tight enough that only a genuine doorbell-driven wake (not a lucky
/// safety-net timeout landing near the inject, and nowhere close to the 2s
/// `PARK_TIMEOUT`) could satisfy it. Before this fix, this exact scenario
/// took up to ~2s; after it, delivery is near-instant.
#[test]
fn injected_message_to_mid_chain_port_wakes_a_fused_run_promptly() {
    let _guard = test_lock();
    let mut interp = Interp::new();
    eval_ok_on(
        &mut interp,
        r#"(def done (promise))
           (def fl (flow/create-flow
                    {:procs {:head {:proc (flow/process (flow/step-count))}
                             :mid {:proc (flow/process (flow/step-passthrough))}
                             :tail {:proc (flow/process (flow/step-sink-deliver 1 done))}}
                     :conns [[[:head :out] [:mid :in]] [[:mid :out] [:tail :in]]]}))
           (flow/start fl)
           (flow/resume fl)
           ;; Sleep long enough that the fused run is unambiguously parked
           ;; on the HEAD's in-chan (no traffic to :head at all, ever, in
           ;; this test) before we measure anything -- this is precisely
           ;; the scenario the gap needs: the head's blocking read has
           ;; nothing to wake it except the shared doorbell, and only a
           ;; ring from the mid-chain in-chan registration (this fix) makes
           ;; that happen promptly for a mid-chain inject.
           (sleep-ms 300)"#,
    );

    let before = Doorbell::safety_net_hits();
    let t0 = Instant::now();
    // `@` forces this call to wait for the injection thread's `chan_put` to
    // actually land before we start timing delivery, same as test 4 above.
    eval_ok_on(&mut interp, "@(flow/inject fl [:mid :in] [42])");
    // `(deref done 50 :timeout)` blocks up to 50ms and returns as soon as
    // the promise resolves -- a direct, non-busy-loop measurement of wake
    // latency (unlike test 4's `poll!` loop, a promise `deref` has its own
    // native wait/wake, so this doesn't need a manual polling loop to stay
    // tight).
    let result = mova::internal::pr_str(&eval_ok_on(&mut interp, "(deref done 50 :timeout)"));
    let elapsed = t0.elapsed();
    let after = Doorbell::safety_net_hits();
    eval_ok_on(&mut interp, "(flow/stop fl)");

    eprintln!(
        "[flow_wake_test] injected_message_to_mid_chain_port_wakes_a_fused_run_promptly: result={result} elapsed={elapsed:?} safety_net_hits before={before} after={after}"
    );
    assert_ne!(
        result, ":timeout",
        "tail's step-sink-deliver promise was not resolved within 50ms of injecting into \
         [:mid :in] on an idle fused run -- run_fused's mid-chain in-chan doorbell \
         registration is missing or broken, so the run fell back to waiting out the full \
         PARK_TIMEOUT safety net instead of waking on the inject"
    );
    assert_eq!(result, "1", "expected step-sink-deliver's promise to resolve to the message count (1)");
    assert!(
        elapsed <= Duration::from_millis(50),
        "delivery took {elapsed:?}, expected <=50ms -- this is the direct regression signal for \
         run_fused's mid-chain in-port doorbell gap"
    );
}

// ---------------------------------------------------------------------------
// 9: an ::flow/in-ports chan is read-set membership no other test here
// exercises -- a PLAIN external `>!!` (not `flow/inject`, not a conn) must
// wake a proc parked on it just as promptly.
// ---------------------------------------------------------------------------

/// Every other wake-latency test in this file goes through a mechanism
/// `resync_read_set_doorbell` already had dedicated coverage for by name --
/// `flow/inject`'s underlying `chan_put` (tests 4 and 8) or the control chan
/// (test 5). None of them proves that a chan a proc's `init` installs via
/// `:clojure.core.async.flow/in-ports` actually JOINS the proc loop's
/// read-set doorbell registration in `resync_read_set_doorbell` (flow.rs) --
/// i.e. that `Doorbell::ring` fires for an ordinary, arbitrary-thread
/// `(>!! ch v)` onto such a chan, with no `flow/inject` and no `:conns`
/// wiring involved at all. That's exactly the idiom a sibling project's
/// entire event-driven design leans on: a "ticker replaced by event" --
/// an external producer thread puts onto a chan the flow only ever *reads*,
/// and the flow proc must wake the instant that put lands, not on the next
/// poll. This test pins it directly: build a single proc whose only input
/// is an in-ports chan (`:ins {}` in `:describe`, the chan supplied by
/// `:init`, same idiom `tests/flow_test.rs` uses ~line 1275-1282), park it
/// fully idle, then `(>!! tick 42)` straight from the test thread and
/// assert delivery inside the same tight 50ms/zero-safety-net bound tests 4
/// and 8 hold the inject and control paths to.
#[test]
fn external_put_to_in_ports_chan_wakes_a_parked_proc_promptly() {
    let _guard = test_lock();
    let mut interp = Interp::new();
    eval_ok_on(
        &mut interp,
        r#"(def tick (chan 10))
           (def done (promise))
           (def fl (flow/create-flow
                    {:procs {:p {:proc (flow/process
                                        (flow/map->step
                                         {:describe (fn [] {:ins {} :outs {}})
                                          :init (fn [_] {:clojure.core.async.flow/in-ports {:in tick}})
                                          :transition (fn [s t]
                                                        (when (= t :clojure.core.async.flow/stop) (close! tick))
                                                        s)
                                          :transform (fn [s _ m] (deliver done m) [s {}])}))}}
                     :conns []}))
           (flow/start fl)
           (flow/resume fl)
           ;; Sleep long enough that the proc is unambiguously parked (not
           ;; mid-lap) before we measure anything -- same margin tests 4 and
           ;; 8 use.
           (sleep-ms 300)"#,
    );

    let before = Doorbell::safety_net_hits();
    let t0 = Instant::now();
    // A PLAIN buffered put from this thread -- not `flow/inject`, not a
    // `:conns` wire -- returns immediately (buffer size 10, one item in
    // flight); this put itself is the wake event under test.
    eval_ok_on(&mut interp, "(>!! tick 42)");
    let result = mova::internal::pr_str(&eval_ok_on(&mut interp, "(deref done 50 :timeout)"));
    let elapsed = t0.elapsed();
    let after = Doorbell::safety_net_hits();
    eval_ok_on(&mut interp, "(flow/stop fl)");

    eprintln!(
        "[flow_wake_test] external_put_to_in_ports_chan_wakes_a_parked_proc_promptly: result={result} elapsed={elapsed:?} safety_net_hits before={before} after={after}"
    );
    assert_ne!(
        result, ":timeout",
        "promise was not resolved within 50ms of a plain (>!! tick 42) onto the proc's \
         ::flow/in-ports chan -- resync_read_set_doorbell is not registering an in-ports \
         chan onto the proc loop's read-set doorbell, so an external put to it isn't waking \
         the parked proc at all (it would eventually show up once the safety net expires)"
    );
    assert_eq!(result, "42", "expected the proc to observe the exact value put onto tick");
    assert!(
        elapsed <= Duration::from_millis(50),
        "delivery took {elapsed:?}, expected <=50ms -- this is the direct regression signal for \
         the in-ports chan's read-set doorbell registration"
    );
    assert_eq!(
        after, before,
        "a safety-net fallback fired during the idle-then-external-put window (delta {}) -- the \
         wake should have been a genuine doorbell ring from the in-ports chan joining the \
         read-set, not a timeout that happened to land near the put",
        after - before
    );
}
