//! Facade-only tests for Phase B of `EMBED-API-PLAN.md`: fuel wired
//! through `Engine`/`EngineBuilder`/`Profile::Untrusted`, `Engine::snapshot`,
//! `Engine: Send`, the serde adapter, `Value::vector`, and
//! `Engine::register_fn_with_arity`. Like `tests/embed_api.rs`, every test
//! here uses ONLY `mova::embed::*` -- a compile failure here means the
//! facade doesn't cover something a real embedder needs.

use std::time::{Duration, Instant};

use mova::embed::{Arity, Engine, Profile, Value, ValueKind};

/// Binary-searches the minimal fuel budget under which `src` succeeds, by
/// building a fresh engine per probe (so each probe's fuel starts at
/// exactly the candidate budget, never polluted by a previous probe). Used
/// to calibrate a workload's real tick cost without hardcoding a magic
/// constant that would silently rot if `tick_fuel`'s call sites ever
/// change -- see `crate::eval::Interp::tick_fuel`'s doc for what counts as
/// a tick.
fn min_fuel_for(src: &str) -> u64 {
    let mut lo: u64 = 0;
    let mut hi: u64 = 5_000_000;
    assert!(
        Engine::builder().fuel(hi).build().eval(src).is_ok(),
        "calibration ceiling too low for {src:?}"
    );
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let budget = mid.max(1);
        if Engine::builder().fuel(budget).build().eval(src).is_ok() {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    lo.max(1)
}

// ---------------------------------------------------------------------
// Fuel
// ---------------------------------------------------------------------

/// `Profile::Untrusted` with no explicit `.fuel(..)` gets the documented
/// 10,000,000-step default: an unconditionally-recurring loop must still
/// terminate, in bounded wall time, with `is_fuel_exhausted() == true` --
/// and the same engine must remain perfectly usable for a subsequent small
/// eval afterwards (fuel resets per eval, it isn't a one-shot budget for
/// the engine's whole lifetime).
#[test]
fn untrusted_default_fuel_exhausts_in_bounded_time_and_engine_stays_usable() {
    let mut engine = Engine::builder().profile(Profile::Untrusted).build();

    let start = Instant::now();
    let err = engine
        .eval("(loop [] (recur))")
        .expect_err("an unconditionally-recurring loop must hit the Untrusted default fuel budget");
    let elapsed = start.elapsed();
    assert!(
        err.is_fuel_exhausted(),
        "expected a fuel-exhaustion error, got: {err}"
    );
    assert!(
        elapsed < Duration::from_secs(30),
        "10,000,000-step default fuel took suspiciously long: {elapsed:?}"
    );

    // Fuel resets per eval: the SAME engine, right after exhausting its
    // budget, must still be able to run a small, unrelated eval.
    let v = engine.eval("(+ 1 2 3)").expect("engine must remain usable after a prior eval exhausted its fuel");
    assert_eq!(v.as_i64(), Some(6));
}

/// `EngineBuilder::fuel` sets a PER-EVAL budget, not a per-engine total:
/// two evals that each consume roughly 60% of the configured budget must
/// BOTH succeed on the same long-lived engine (fuel is reset to the full
/// budget before each `eval` call, not carried over/decremented
/// cumulatively -- if it were cumulative, 60% + 60% > 100% would fail the
/// second call).
#[test]
fn fuel_budget_resets_on_every_call_not_just_at_build_time() {
    let workload = "(loop [i 0] (if (< i 4000) (recur (inc i)) i))";
    let min_needed = min_fuel_for(workload);
    // Budget sized so one run consumes ~60% of it (with a small safety
    // margin so timing/tick-count noise doesn't flake the test).
    let budget = (min_needed as f64 / 0.6).ceil() as u64 + 10;

    let mut engine = Engine::builder().fuel(budget).build();
    let v1 = engine
        .eval(workload)
        .expect("first eval within budget should succeed");
    assert_eq!(v1.as_i64(), Some(4000));

    let v2 = engine
        .eval(workload)
        .expect("second eval must ALSO succeed -- fuel must have reset, not accumulated consumption");
    assert_eq!(v2.as_i64(), Some(4000));
}

/// An explicit `.fuel(n)` on `Profile::Pure` (not just `Untrusted`) still
/// bounds execution: a tiny budget on an infinite loop must exhaust.
#[test]
fn explicit_fuel_on_pure_profile_bounds_execution() {
    let mut engine = Engine::builder().profile(Profile::Pure).fuel(1_000).build();
    let err = engine
        .eval("(loop [] (recur))")
        .expect_err("explicit small fuel budget on Pure should still exhaust");
    assert!(err.is_fuel_exhausted(), "expected fuel exhaustion, got: {err}");
}

/// `Profile::Scripting`'s default (no `.fuel(..)` call) is unlimited, same
/// as the pre-Phase-B facade: a long-but-finite loop must run to
/// completion and produce the right value, never erroring on fuel.
#[test]
fn scripting_profile_default_has_no_fuel_limit() {
    let mut engine = Engine::builder().profile(Profile::Scripting).build();
    let v = engine
        .eval("(loop [i 0] (if (< i 300000) (recur (inc i)) i))")
        .expect("Scripting's default fuel is unlimited; this must not error");
    assert_eq!(v.as_i64(), Some(300000));
}

/// `Engine::set_fuel` changes the budget after construction, taking effect
/// on the next call.
#[test]
fn set_fuel_changes_budget_for_subsequent_calls() {
    let mut engine = Engine::builder().build(); // Scripting, unlimited by default
    let v = engine.eval("(loop [i 0] (if (< i 50000) (recur (inc i)) i))").unwrap();
    assert_eq!(v.as_i64(), Some(50000));

    engine.set_fuel(Some(10));
    let err = engine
        .eval("(loop [] (recur))")
        .expect_err("after set_fuel(Some(10)) a recurring loop should exhaust almost immediately");
    assert!(err.is_fuel_exhausted());

    engine.set_fuel(None);
    let v = engine.eval("(loop [i 0] (if (< i 50000) (recur (inc i)) i))").unwrap();
    assert_eq!(v.as_i64(), Some(50000), "set_fuel(None) should remove the limit again");
}

// ---------------------------------------------------------------------
// Snapshot
// ---------------------------------------------------------------------

/// `def`s made through a snapshot are invisible to the original, and
/// `def`s made through the original after snapshotting are invisible to
/// the snapshot -- isolation holds in both directions through the facade.
#[test]
fn snapshot_isolates_defs_in_both_directions() {
    let mut original = Engine::builder().build();
    original.eval("(def shared 1)").unwrap();
    let mut snap = original.snapshot();

    snap.eval("(def only-in-snap 42)").unwrap();
    assert!(
        original.eval("only-in-snap").is_err(),
        "original must not see a def made through the snapshot"
    );

    original.eval("(def only-in-original 99)").unwrap();
    assert!(
        snap.eval("only-in-original").is_err(),
        "snapshot must not see a def made through the original after the snapshot point"
    );

    // Both sides still agree on what existed before the snapshot.
    assert_eq!(snap.eval("shared").unwrap().as_i64(), Some(1));
    assert_eq!(original.eval("shared").unwrap().as_i64(), Some(1));
}

/// An atom captured in a pre-snapshot def is Arc-shared: `swap!` through
/// either side is visible through the other (Clojure reference-identity
/// semantics), even though the var TABLE itself is isolated.
#[test]
fn snapshot_shares_pre_existing_atoms() {
    let mut original = Engine::builder().build();
    original.eval("(def counter (atom 0))").unwrap();
    let mut snap = original.snapshot();

    snap.eval("(swap! counter inc)").unwrap();
    assert_eq!(
        original.eval("@counter").unwrap().as_i64(),
        Some(1),
        "swap! through the snapshot must be visible through the original"
    );

    original.eval("(swap! counter inc)").unwrap();
    assert_eq!(
        snap.eval("@counter").unwrap().as_i64(),
        Some(2),
        "swap! through the original must be visible through the snapshot"
    );
}

/// Snapshotting a `Profile::Untrusted` engine carries its fuel
/// configuration along: the snapshot still exhausts on an infinite loop
/// under the (inherited) default budget, and remains usable afterwards,
/// exactly like a freshly-built Untrusted engine would.
#[test]
fn snapshot_of_untrusted_engine_keeps_fuel_behavior() {
    let mut original = Engine::builder().profile(Profile::Untrusted).build();
    original.eval("(def x 1)").unwrap();
    let mut snap = original.snapshot();

    let err = snap
        .eval("(loop [] (recur))")
        .expect_err("snapshot of an Untrusted engine should still be fuel-bounded");
    assert!(err.is_fuel_exhausted(), "expected fuel exhaustion, got: {err}");

    // Fuel resets per eval on the snapshot too.
    let v = snap.eval("(+ x 1)").unwrap();
    assert_eq!(v.as_i64(), Some(2));
}

// ---------------------------------------------------------------------
// Send
// ---------------------------------------------------------------------

/// `Engine: Send` -- a whole engine can be moved to another thread. This
/// is a compile-time property; the test body just has to exist and pass
/// for the assertion below (checked at compile time regardless) to have
/// been exercised as part of the suite.
#[test]
fn engine_is_send() {
    fn assert_send<T: Send>() {}
    assert_send::<Engine>();

    // Exercise it for real too: build an engine, move it into a spawned
    // thread, use it there, and get a result back.
    let engine = Engine::builder().build();
    let handle = std::thread::spawn(move || {
        let mut engine = engine;
        engine.eval("(+ 40 2)").unwrap().as_i64()
    });
    assert_eq!(handle.join().unwrap(), Some(42));
}

// ---------------------------------------------------------------------
// Value::vector
// ---------------------------------------------------------------------

#[test]
fn value_vector_constructor_round_trips() {
    let v = Value::vector(vec![Value::from(1i64), Value::from(2i64), Value::from(3i64)]);
    assert_eq!(v.kind(), ValueKind::Vector);
    assert_eq!(v.len(), 3);
    let elems: Vec<i64> = v.iter().map(|e| e.as_i64().unwrap()).collect();
    assert_eq!(elems, vec![1, 2, 3]);

    // Also from a lazy iterator, not just a `Vec` -- the point of taking
    // `impl IntoIterator` rather than `Vec<Value>` directly.
    let v = Value::vector((0..5i64).map(Value::from));
    assert_eq!(v.len(), 5);
}

// ---------------------------------------------------------------------
// register_fn_with_arity
// ---------------------------------------------------------------------

#[test]
fn register_fn_with_arity_exact_rejects_wrong_count_and_accepts_right_one() {
    let mut engine = Engine::builder().build();
    engine.register_fn_with_arity("host/add2", Arity::Exact(2), |args| {
        let a = args[0].as_i64().unwrap();
        let b = args[1].as_i64().unwrap();
        Ok(Value::from(a + b))
    });

    let err = engine.eval("(host/add2 1)").expect_err("wrong arity should error");
    assert!(
        err.to_string().contains("host/add2"),
        "arity error should name the function, got: {err}"
    );

    let ok = engine.eval("(host/add2 3 4)").unwrap();
    assert_eq!(ok.as_i64(), Some(7));
}

#[test]
fn register_fn_with_arity_range() {
    let mut engine = Engine::builder().build();
    engine.register_fn_with_arity("host/rng", Arity::Range(1, 3), |args| Ok(Value::from(args.len() as i64)));

    assert!(engine.eval("(host/rng)").is_err(), "0 args is below the Range(1, 3) floor");
    assert_eq!(engine.eval("(host/rng 1)").unwrap().as_i64(), Some(1));
    assert_eq!(engine.eval("(host/rng 1 2 3)").unwrap().as_i64(), Some(3));
    assert!(
        engine.eval("(host/rng 1 2 3 4)").is_err(),
        "4 args is above the Range(1, 3) ceiling"
    );
}

#[test]
fn register_fn_with_arity_at_least() {
    let mut engine = Engine::builder().build();
    engine.register_fn_with_arity("host/atleast2", Arity::AtLeast(2), |args| Ok(Value::from(args.len() as i64)));

    assert!(engine.eval("(host/atleast2 1)").is_err(), "1 arg is below AtLeast(2)");
    assert_eq!(engine.eval("(host/atleast2 1 2)").unwrap().as_i64(), Some(2));
    assert_eq!(engine.eval("(host/atleast2 1 2 3 4)").unwrap().as_i64(), Some(4));
}

// ---------------------------------------------------------------------
// serde adapter (feature = "serde")
// ---------------------------------------------------------------------

#[cfg(feature = "serde")]
#[test]
fn serde_round_trip_struct_through_script_mutation() {
    use mova::embed::{from_value, to_value};
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    struct Widget {
        name: String,
        qty: i64,
    }

    let original = Widget {
        name: "gizmo".to_string(),
        qty: 3,
    };
    let v = to_value(&original).unwrap();
    assert_eq!(v.kind(), ValueKind::Map);

    let mut engine = Engine::builder().profile(Profile::Pure).build();
    // `def`-ing a host-built `Value` into the engine: register a zero-arg
    // native returning a clone of it, then have script call through it --
    // the facade has no separate "inject a global" entry point, and
    // doesn't need one, since `register_fn` already covers this.
    engine.register_fn("host/widget", move |_args| Ok(v.clone()));

    let name = engine.eval("(:name (host/widget))").unwrap();
    assert_eq!(name.as_str(), Some("gizmo"));

    // Script reads a field and assoc's a new (struct-unknown) key.
    let updated = engine
        .eval(r#"(assoc (host/widget) :qty 99 :extra "debug-only")"#)
        .unwrap();

    let round_tripped: Widget = from_value(&updated).unwrap();
    assert_eq!(
        round_tripped,
        Widget {
            name: "gizmo".to_string(),
            qty: 99,
        },
        "unknown :extra key must be tolerated (ignored), :qty must reflect the script's assoc"
    );
}
