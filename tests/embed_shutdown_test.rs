//! Facade-only tests for Phase C of `EMBED-API-PLAN.md`: `Engine::shutdown`
//! and the flow-registry hook it reads. Like `tests/embed_api.rs`/
//! `tests/embed_phase_b.rs`, every test here uses ONLY `mova::embed::*` --
//! a compile failure here means the facade doesn't cover something a real
//! embedder needs. Timing margins follow `tests/flow_test.rs`'s convention:
//! generously above the engine's own internal polling intervals (see
//! `builtins::flow`'s module doc), never a tight race.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use mova::embed::{Engine, ShutdownReport, Value};

fn engine() -> Engine {
    // Default profile is `Profile::Scripting` -- flows/futures/promises
    // all need the `conc`+`flow` capability groups.
    Engine::builder().build()
}

fn eval_ok(engine: &mut Engine, src: &str) -> Value {
    engine
        .eval(src)
        .unwrap_or_else(|e| panic!("eval error for {src:?}: {}", e.render_plain()))
}

/// A single-proc, `:in`-only flow (a pure sink, no `:out`) whose transform
/// calls the registered `host/tick` native on every message it sees --
/// `Engine::shutdown` tests use this to observe production continuing
/// (the counter climbing) and then genuinely ceasing after shutdown.
/// `(sleep-ms 3)` inside the transform keeps each message's processing
/// slow enough that a short `Duration::from_millis` sleep on the Rust side
/// is comfortably guaranteed to land mid-stream, not after the whole
/// injected batch has already drained.
const TICKING_SINK_FLOW: &str = r#"
    (def fl
      (flow/create-flow
       {:procs {:c {:proc (flow/process
                           (flow/map->step
                            {:describe (fn [] {:ins {:in {}} :outs {}})
                             :transform (fn [s _ m] (sleep-ms 3) (host/tick) [s {}])}))}}
        :conns []}))
    (flow/start fl)
    (flow/resume fl)
    (flow/inject fl [:c :in] (range 1000))
    nil
"#;

/// A single-proc, no-conns flow with an inert transform -- used by tests
/// that only care about the flow's Running/Stopped lifecycle, not its
/// throughput (the two-flows and snapshot tests below).
const INERT_FLOW: &str = r#"
    (flow/create-flow
     {:procs {:p {:proc (flow/process
                         (flow/map->step
                          {:describe (fn [] {:ins {:in {}} :outs {}})
                           :transform (fn [s _ m] [s {}])}))}}
      :conns []})
"#;

// ---------------------------------------------------------------------------
// Single flow: flows_stopped == 1, genuinely stopped (both suggested
// observables: output ceasing AND flow/ping erroring).
// ---------------------------------------------------------------------------

#[test]
fn shutdown_stops_a_single_flow_output_ceases_and_ping_errors() {
    let counter = Arc::new(AtomicUsize::new(0));
    let counter_for_native = counter.clone();

    let mut e = engine();
    e.register_fn("host/tick", move |_args| {
        counter_for_native.fetch_add(1, Ordering::SeqCst);
        Ok(Value::from(()))
    });
    eval_ok(&mut e, TICKING_SINK_FLOW);

    // Let production actually get going before we shut down.
    std::thread::sleep(Duration::from_millis(50));
    let mid_flight = counter.load(Ordering::SeqCst);
    assert!(mid_flight > 0, "expected the sink to have processed at least one message by now");
    assert!(
        mid_flight < 1000,
        "expected the injected batch (1000 msgs @ 3ms each = 3s) to still be draining, not finished, at {mid_flight}"
    );

    let report = e.shutdown();
    assert_eq!(report.flows_stopped, 1, "expected exactly one flow to be stopped");
    assert_eq!(report.flows_failed, 0);

    // Output ceasing: allow one message's worth of slack for whatever was
    // already mid-transform when the stop command was noticed, then
    // confirm the count has settled (production genuinely stopped, not
    // just paused).
    let at_shutdown = counter.load(Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(100));
    let after_wait = counter.load(Ordering::SeqCst);
    assert!(
        after_wait <= at_shutdown + 1,
        "sink kept producing after shutdown: at_shutdown={at_shutdown}, after_wait={after_wait}"
    );

    // flow/ping erroring: the flow's phase is `Stopped`, so `flow/ping`
    // (which requires `Running`) must fail -- the engine stays perfectly
    // usable for this follow-up eval.
    let ping_failed = eval_ok(&mut e, "(try (do (flow/ping fl 1000) false) (catch e true))");
    assert_eq!(ping_failed.as_bool(), Some(true), "flow/ping should error once the flow has been shut down");
}

// ---------------------------------------------------------------------------
// Two flows -> flows_stopped == 2.
// ---------------------------------------------------------------------------

#[test]
fn shutdown_stops_two_independent_flows() {
    let mut e = engine();
    eval_ok(
        &mut e,
        &format!(
            r#"(def fl1 {INERT_FLOW})
               (def fl2 {INERT_FLOW})
               (flow/start fl1)
               (flow/resume fl1)
               (flow/start fl2)
               (flow/resume fl2)
               nil"#
        ),
    );

    let report = e.shutdown();
    assert_eq!(report, ShutdownReport { flows_stopped: 2, flows_failed: 0 });
}

// ---------------------------------------------------------------------------
// A flow created then dropped by the script (no live reference anywhere):
// the registry's only handle is a `Weak`, so `Weak::upgrade` deterministically
// fails once the script's own last `Arc` (a transient eval-form result,
// overwritten by the next top-level form) is dropped. `shutdown` must not
// crash and must report it toward neither count.
// ---------------------------------------------------------------------------

#[test]
fn shutdown_silently_skips_a_flow_the_script_already_dropped() {
    let mut e = engine();
    // Two top-level forms: the first's result (the un-started, un-bound
    // flow) is a transient `eval_forms` value, overwritten -- and thus
    // dropped, deterministically, no GC delay -- by the second form's
    // `nil` before `eval` ever returns.
    eval_ok(&mut e, &format!("{INERT_FLOW}\nnil"));

    let report = e.shutdown();
    assert_eq!(
        report,
        ShutdownReport::default(),
        "a dropped flow must count toward neither flows_stopped nor flows_failed"
    );

    // The engine is unaffected -- still evals fine afterwards.
    let v = eval_ok(&mut e, "(+ 1 2 3)");
    assert_eq!(v.as_i64(), Some(6));
}

// ---------------------------------------------------------------------------
// shutdown() twice: second call finds an empty registry (zeros), and the
// engine remains fully usable after both calls.
// ---------------------------------------------------------------------------

#[test]
fn shutdown_twice_is_idempotent_and_engine_stays_usable() {
    let mut e = engine();
    eval_ok(
        &mut e,
        &format!(
            r#"(def fl {INERT_FLOW})
               (flow/start fl)
               (flow/resume fl)
               nil"#
        ),
    );

    let first = e.shutdown();
    assert_eq!(first, ShutdownReport { flows_stopped: 1, flows_failed: 0 });

    let second = e.shutdown();
    assert_eq!(second, ShutdownReport::default(), "a second shutdown must find an empty registry");

    // The Engine keeps working: eval, and even a brand new flow (created
    // AFTER shutdown) is tracked and stoppable by a later shutdown call.
    let v = eval_ok(&mut e, "(+ 40 2)");
    assert_eq!(v.as_i64(), Some(42));

    eval_ok(
        &mut e,
        &format!(
            r#"(def fl2 {INERT_FLOW})
               (flow/start fl2)
               (flow/resume fl2)
               nil"#
        ),
    );
    let third = e.shutdown();
    assert_eq!(third, ShutdownReport { flows_stopped: 1, flows_failed: 0 });
}

// ---------------------------------------------------------------------------
// snapshot(): the snapshot gets a FRESH, EMPTY registry -- its own
// shutdown() must see none of the pre-snapshot flow, while the original
// engine's shutdown() still stops it.
// ---------------------------------------------------------------------------

#[test]
fn snapshot_gets_a_fresh_registry_original_still_stops_its_own_flow() {
    let mut e = engine();
    eval_ok(
        &mut e,
        &format!(
            r#"(def fl {INERT_FLOW})
               (flow/start fl)
               (flow/resume fl)
               nil"#
        ),
    );

    let mut snap = e.snapshot();

    let snap_report = snap.shutdown();
    assert_eq!(
        snap_report,
        ShutdownReport::default(),
        "a snapshot's registry must start empty -- it must not see the original's pre-snapshot flow"
    );

    let orig_report = e.shutdown();
    assert_eq!(
        orig_report,
        ShutdownReport { flows_stopped: 1, flows_failed: 0 },
        "the original engine must still be able to stop the flow it created before the snapshot"
    );
}

// ---------------------------------------------------------------------------
// A flow created inside `(future ...)`: `fork()`'s SHARED registry means
// the parent engine's shutdown() sees and stops it too. Coordinated with a
// promise (per EMBED-API-PLAN.md's guidance) rather than a sleep, so the
// test can't be flaky about whether creation has completed yet: the final
// `@ready` blocks the whole `eval` call until the future body has created,
// started, resumed, and delivered the flow.
// ---------------------------------------------------------------------------

#[test]
fn shutdown_stops_a_flow_created_inside_a_future_fork_shared_registry() {
    let mut e = engine();
    eval_ok(
        &mut e,
        &format!(
            r#"(def ready (promise))
               (future
                 (let [fl {INERT_FLOW}]
                   (flow/start fl)
                   (flow/resume fl)
                   (deliver ready fl)))
               @ready
               nil"#
        ),
    );

    let report = e.shutdown();
    assert_eq!(
        report,
        ShutdownReport { flows_stopped: 1, flows_failed: 0 },
        "a flow created inside a future body must be tracked and stoppable by the parent engine's shutdown"
    );
}
