//! Facade-only tests for the native→script direction: `Engine::
//! register_fn_with_reentry` + `Reentry::call`. Like `tests/embed_api.rs`
//! and `tests/embed_phase_b.rs`, every test here uses ONLY
//! `mova::embed::*` -- a compile failure here means the facade doesn't
//! cover something a real embedder needs.
//!
//! The motivating host shape (an editor's AppKit live-resize trampoline)
//! appears verbatim in `stored_closure_called_from_a_later_native`: one
//! native stashes a script closure in a host-side cell, a later native --
//! re-entered from an FFI pump, mid-eval -- reads the cell and calls it.

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Mutex;

use mova::embed::{Arity, Engine, Error, Profile, Value, ValueKind};

/// The reentrant native every test below shares: `(host/apply f & args)`
/// calls `f` on the in-flight interpreter and hands its result straight
/// back to script. `Arity::AtLeast(1)` so the arity precheck has something
/// to reject in `arity_precheck_fires_before_the_closure_runs`.
fn register_host_apply(engine: &mut Engine) {
    engine.register_fn_with_reentry("host/apply", Arity::AtLeast(1), |reentry, args| {
        reentry.call(&args[0], &args[1..])
    });
}

/// A script fn value passed to a native survives the round trip into Rust
/// and back into the interpreter: `(host/apply double 21)` is `(double 21)`.
#[test]
fn round_trip_native_calls_a_script_fn_and_returns_its_result() {
    let mut engine = Engine::builder().profile(Profile::Pure).build();
    register_host_apply(&mut engine);
    engine.eval("(defn double [x] (* 2 x))").unwrap();

    let v = engine.eval("(host/apply double 21)").unwrap();
    assert_eq!(v.kind(), ValueKind::Int);
    assert_eq!(v.as_i64(), Some(42));

    // Several args, and a result that is not a scalar -- nothing about the
    // boundary is arity- or shape-specific.
    engine.eval("(defn pack [a b c] [c b a])").unwrap();
    let v = engine.eval("(host/apply pack 1 2 3)").unwrap();
    assert_eq!(v.kind(), ValueKind::Vector);
    let elems: Vec<i64> = v.iter().map(|e| e.as_i64().unwrap()).collect();
    assert_eq!(elems, vec![3, 2, 1]);
}

/// The editor host's real shape: native A (`host/on-resize`) stores the
/// script closure it is handed in a host-side cell; native B
/// (`host/fire-resize`, the one re-entered from the FFI pump) reads the cell
/// back out and calls it. Only B needs reentry -- storing is a leaf
/// operation, which is exactly why the two halves are registered through
/// different entry points here.
#[test]
fn stored_closure_called_from_a_later_native() {
    static HANDLER: Mutex<Option<Value>> = Mutex::new(None);
    // The observable effect the script closure produces, read back from
    // Rust afterwards: proves the stored closure really ran, independently
    // of what `fire` returns.
    static REDRAWS: AtomicI64 = AtomicI64::new(0);

    let mut engine = Engine::builder().profile(Profile::Pure).build();

    engine.register_fn_with_arity("host/on-resize", Arity::Exact(1), |args| {
        *HANDLER.lock().unwrap() = Some(args[0].clone());
        Ok(Value::keyword("registered"))
    });
    engine.register_fn_with_reentry("host/fire-resize", Arity::Exact(1), |reentry, args| {
        let handler = HANDLER.lock().unwrap().clone();
        let Some(handler) = handler else {
            return Err(Error::other("no resize handler registered"));
        };
        reentry.call(&handler, &args[0..1])
    });
    engine.register_fn_with_arity("host/note-redraw", Arity::Exact(1), |args| {
        REDRAWS.fetch_add(args[0].as_i64().unwrap_or(0), Ordering::SeqCst);
        Ok(Value::keyword("noted"))
    });

    engine
        .eval("(host/on-resize (fn [w] (host/note-redraw w) (* w 10)))")
        .unwrap();

    // Nothing has run the closure yet: registration is a leaf operation.
    assert_eq!(REDRAWS.load(Ordering::SeqCst), 0);

    let v = engine.eval("(host/fire-resize 3)").unwrap();
    assert_eq!(v.as_i64(), Some(30));
    assert_eq!(REDRAWS.load(Ordering::SeqCst), 3);

    // Fires repeatedly off the same stored Value -- the cell holds the
    // closure, not a one-shot ticket.
    engine.eval("(host/fire-resize 4)").unwrap();
    assert_eq!(REDRAWS.load(Ordering::SeqCst), 7);

    *HANDLER.lock().unwrap() = None;
}

/// A `throw` inside the reentrantly-called script fn comes back to the
/// native as an ordinary `Err`; returning it propagates the SAME thrown
/// value out to the outer script, where a script-level `catch` still
/// handles it. The host frame in the middle is transparent.
#[test]
fn thrown_error_propagates_through_the_native_and_stays_catchable() {
    let mut engine = Engine::builder().profile(Profile::Pure).build();
    register_host_apply(&mut engine);
    engine.eval("(defn boom [] (throw \"kaboom\"))").unwrap();

    // Uncaught: the failure reaches the host as a normal `Error`, with the
    // SAME shape a direct `(boom)` produces -- a script `throw` reports as
    // "user exception" whether or not a host frame sat in the middle (the
    // thrown value itself rides along in the error, see the `catch` below).
    let direct = engine.eval("(boom)").expect_err("sanity: a direct call must throw");
    let err = engine
        .eval("(host/apply boom)")
        .expect_err("a throw inside the reentrant call must not be swallowed");
    assert_eq!(
        err.to_string(),
        direct.to_string(),
        "a reentrant throw must be indistinguishable from a direct one"
    );
    assert!(!err.is_fuel_exhausted(), "a plain throw is not a fuel exhaustion");

    // Caught by SCRIPT, on the far side of the host frame: the thrown value
    // arrives intact, so `catch` binds `"kaboom"` itself, not some
    // host-invented stand-in.
    let v = engine.eval("(try (host/apply boom) (catch e e))").unwrap();
    assert_eq!(v.as_str(), Some("kaboom"));

    // The engine is fully usable afterwards.
    assert_eq!(engine.eval("(host/apply inc 41)").unwrap().as_i64(), Some(42));
}

/// The arity precheck is the same machinery `register_fn_with_arity` uses
/// and fires BEFORE the closure body -- so a wrong-arity call can't even
/// reach the `args[0]` index that would otherwise panic.
#[test]
fn arity_precheck_fires_before_the_closure_runs() {
    static RAN: AtomicBool = AtomicBool::new(false);

    let mut engine = Engine::builder().profile(Profile::Pure).build();
    engine.register_fn_with_reentry("host/exactly2", Arity::Exact(2), |reentry, args| {
        RAN.store(true, Ordering::SeqCst);
        reentry.call(&args[0], &args[1..])
    });

    let err = engine.eval("(host/exactly2 inc)").expect_err("1 arg must fail Exact(2)");
    let msg = err.to_string();
    assert!(msg.contains("host/exactly2"), "arity error should name the fn, got: {msg}");
    assert!(
        msg.contains("exactly 2 arguments") && msg.contains("got 1"),
        "expected the standard arity message shape, got: {msg}"
    );
    assert!(!RAN.load(Ordering::SeqCst), "the closure body must not run on an arity mismatch");

    let v = engine.eval("(host/exactly2 inc 41)").unwrap();
    assert_eq!(v.as_i64(), Some(42));
    assert!(RAN.load(Ordering::SeqCst));
}

/// Host→script→host: the reentrantly-called script fn itself calls a plain
/// (non-reentrant) registered native. Nothing about the reentrant frame
/// restricts what the script it invokes may do -- it is the same
/// interpreter, with the same globals, one stack level down.
#[test]
fn nested_reentry_script_calls_another_registered_native() {
    let mut engine = Engine::builder().profile(Profile::Pure).build();
    register_host_apply(&mut engine);
    engine.register_fn_with_arity("host/triple", Arity::Exact(1), |args| {
        Ok(Value::from(args[0].as_i64().unwrap() * 3))
    });

    engine.eval("(defn via-host [x] (+ 1 (host/triple x)))").unwrap();
    let v = engine.eval("(host/apply via-host 5)").unwrap();
    assert_eq!(v.as_i64(), Some(16));

    // And one more turn of the crank: script → native(reentry) → script →
    // native(reentry) → script.
    engine.eval("(defn twice-through [x] (host/apply via-host (host/apply inc x)))").unwrap();
    let v = engine.eval("(host/apply twice-through 4)").unwrap();
    assert_eq!(v.as_i64(), Some(16));
}

/// The documented hazard, pinned: a reentrant call SHARES the in-flight
/// fuel counter instead of getting a fresh budget.
///
/// The test calibrates the cost of one `burn` loop, then picks a budget that
/// comfortably covers EITHER half alone (the direct `(burn n)` or the
/// reentrant `(host/apply burn n)`) but is strictly less than the two
/// together. If `Reentry::call` reset fuel the way `Engine::call` does, the
/// combined script would succeed -- the reentrant half would start from a
/// full tank. It must not.
#[test]
fn reentrant_call_shares_the_in_flight_fuel_budget() {
    const PRELUDE: &str = "(defn burn [n] (loop [i 0 acc 0] (if (< i n) (recur (inc i) (+ acc i)) acc)))";
    const DIRECT: &str = "(burn 2000)";
    const REENTRANT: &str = "(host/apply burn 2000)";
    const BOTH: &str = "(do (burn 2000) (host/apply burn 2000))";

    /// Runs `src` on a fresh engine with `fuel` as the per-eval budget.
    /// Fresh per probe so nothing is polluted by a previous probe's
    /// leftovers, exactly like `embed_phase_b.rs`'s `min_fuel_for`.
    fn run(fuel: u64, src: &str) -> Result<Value, Error> {
        let mut engine = Engine::builder().fuel(fuel).build();
        register_host_apply(&mut engine);
        engine.eval(PRELUDE).expect("the defn prelude must fit in every probed budget");
        engine.eval(src)
    }

    /// Minimal budget under which `src` succeeds -- binary-searched rather
    /// than hardcoded, so the test doesn't rot when `tick_fuel`'s call sites
    /// change.
    fn min_fuel_for(src: &str) -> u64 {
        let mut lo: u64 = 1;
        let mut hi: u64 = 5_000_000;
        assert!(run(hi, src).is_ok(), "calibration ceiling too low for {src:?}");
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if run(mid.max(1), src).is_ok() {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }
        lo.max(1)
    }

    let min_direct = min_fuel_for(DIRECT);
    let min_reentrant = min_fuel_for(REENTRANT);

    // Enough for either half on its own, with slack -- but nowhere near
    // enough for both, which is what makes the combined failure below a
    // statement about SHARING rather than about the budget being tight.
    let budget = min_direct.max(min_reentrant) + 64;
    assert!(
        budget < min_direct + min_reentrant,
        "test is only meaningful when the budget cannot cover both halves \
         (budget {budget}, direct {min_direct}, reentrant {min_reentrant})"
    );

    assert!(run(budget, DIRECT).is_ok(), "the direct half alone must fit in {budget}");
    assert!(run(budget, REENTRANT).is_ok(), "the reentrant half alone must fit in {budget}");

    let err = run(budget, BOTH).expect_err(
        "both halves on one shared budget must exhaust it -- a fresh budget per reentrant call would let this pass",
    );
    assert!(
        err.is_fuel_exhausted(),
        "expected fuel exhaustion, got: {err}\n{}",
        err.render_plain()
    );
}

/// The exhaustion is observable INSIDE the reentrant native (that is where
/// the counter runs out), and the `Error` the native sees answers `true` to
/// `is_fuel_exhausted` -- which is what lets a host tell "the script I
/// called back into ran out of budget" apart from "it threw". Propagating
/// it, as documented, keeps it unwinding to the outer call.
#[test]
fn native_observes_fuel_exhaustion_from_inside_the_reentrant_call() {
    static SAW_FUEL_ERROR: AtomicBool = AtomicBool::new(false);

    let mut engine = Engine::builder().fuel(50_000).build();
    engine.register_fn_with_reentry("host/apply", Arity::AtLeast(1), |reentry, args| {
        match reentry.call(&args[0], &args[1..]) {
            Ok(v) => Ok(v),
            Err(e) => {
                SAW_FUEL_ERROR.store(e.is_fuel_exhausted(), Ordering::SeqCst);
                Err(e)
            }
        }
    });
    engine
        .eval("(defn spin [] (loop [i 0] (if (< i 100000000) (recur (inc i)) i)))")
        .unwrap();

    let err = engine
        .eval("(host/apply spin)")
        .expect_err("an unbounded loop must exhaust the 50k budget");
    assert!(err.is_fuel_exhausted(), "expected fuel exhaustion, got: {err}");
    assert!(
        SAW_FUEL_ERROR.load(Ordering::SeqCst),
        "the native's own Reentry::call must have surfaced the exhaustion as is_fuel_exhausted()"
    );

    // `FuelExhausted` also stays uncatchable by SCRIPT across the host
    // frame -- the reentrant native re-raised it rather than absorbing it,
    // so a script `catch` around the call still doesn't get to swallow it.
    let err = engine
        .eval("(try (host/apply spin) (catch e :caught))")
        .expect_err("a propagated fuel exhaustion must not be catchable by script");
    assert!(err.is_fuel_exhausted(), "expected fuel exhaustion, got: {err}");
}
