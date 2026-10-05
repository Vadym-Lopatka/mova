//! Correctness tests for the embedding-fuel probe (`Interp::fuel`,
//! `Interp::tick_fuel`, `ErrorKind::FuelExhausted`).
//!
//! Three things this suite must demonstrate, per tier config:
//!   1. an unconditionally-recurring `loop` with a finite fuel budget
//!      terminates (in bounded wall time -- these are run with `--release`
//!      by the harness so a bug that made this loop forever would hang the
//!      whole `cargo test` invocation, which is itself a meaningful
//!      regression signal) with a `FuelExhausted` error, not a hang;
//!   2. a program that legitimately needs ~N steps succeeds when given
//!      fuel > N, and the returned VALUE is correct (fuel bookkeeping must
//!      not perturb the computation);
//!   3. `(try .. (catch e ..))` does NOT swallow fuel exhaustion -- the
//!      catch body must never run, and the error must still propagate to
//!      the host as `Err(RjError { kind: FuelExhausted, .. })`.
//!
//! `Interp::with_tiers(compile_enabled, numloop_enabled)` is exercised
//! across all four combinations so no tier's own back-edge check is
//! untested.

use mova::internal::ErrorKind;
use mova::internal::Interp;

const TIER_COMBOS: [(bool, bool); 4] = [
    (true, true),   // compiled tier + NumLoop specialization
    (true, false),  // compiled tier, NumLoop specialization off
    (false, true),  // tree-walker (numloop_enabled is moot when compile is off)
    (false, false), // tree-walker
];

fn interp_for(compile_enabled: bool, numloop_enabled: bool, fuel: Option<u64>) -> Interp {
    let mut interp = Interp::with_tiers(compile_enabled, numloop_enabled);
    interp.fuel = fuel;
    interp
}

/// Requirement 1: `(loop [] (recur))` never returns on its own; a finite
/// fuel budget must still stop it, in every tier config.
#[test]
fn infinite_loop_recur_terminates_on_fuel_exhaustion_every_tier() {
    for (compile, numloop) in TIER_COMBOS {
        let mut interp = interp_for(compile, numloop, Some(1_000_000));
        let err = interp
            .eval_str("fuel-test", "(loop [] (recur))")
            .expect_err(&format!(
                "compile={compile} numloop={numloop}: infinite loop should have hit fuel exhaustion"
            ));
        assert_eq!(
            err.kind,
            ErrorKind::FuelExhausted,
            "compile={compile} numloop={numloop}: wrong error kind: {err:?}"
        );
    }
}

/// Same shape, but the infinite recursion is a fn calling itself (not a
/// `loop`) -- exercises the OTHER back-edge (`run_closure_trampoline` /
/// `compiled_call_body!`'s self-recur arm), not the `loop`-specific one.
#[test]
fn infinite_self_recur_fn_terminates_on_fuel_exhaustion_every_tier() {
    for (compile, numloop) in TIER_COMBOS {
        let mut interp = interp_for(compile, numloop, Some(1_000_000));
        let err = interp
            .eval_str("fuel-test", "(defn spin [] (recur)) (spin)")
            .expect_err(&format!(
                "compile={compile} numloop={numloop}: infinite self-recur should have hit fuel exhaustion"
            ));
        assert_eq!(
            err.kind,
            ErrorKind::FuelExhausted,
            "compile={compile} numloop={numloop}: wrong error kind: {err:?}"
        );
    }
}

/// Infinite (non-tail) recursion via plain fn CALLS -- makes sure the
/// call-entry checkpoint (`apply_closure`/`apply_closure_buf`) alone is
/// sufficient to bound a script that never uses `loop`/self-recur at all.
/// Fuel is kept comfortably BELOW the default `max_depth` (200) so the
/// fuel check (which runs right after the depth check on every call) is
/// what actually stops this, not the depth guard finding "stack overflow"
/// first -- and specifically so this never exercises real deep Rust
/// recursion far enough to be at risk of the test harness's own (small,
/// unlike the CLI's dedicated big-stack thread) default thread stack.
#[test]
fn non_tail_recursion_terminates_on_fuel_exhaustion_every_tier() {
    for (compile, numloop) in TIER_COMBOS {
        let mut interp = interp_for(compile, numloop, Some(50));
        let err = interp
            .eval_str("fuel-test", "(defn spin [n] (+ 1 (spin (inc n)))) (spin 0)")
            .expect_err(&format!(
                "compile={compile} numloop={numloop}: non-tail recursion should have hit fuel exhaustion"
            ));
        assert_eq!(
            err.kind,
            ErrorKind::FuelExhausted,
            "compile={compile} numloop={numloop}: wrong error kind: {err:?}"
        );
    }
}

/// NumLoop-specialized shape specifically: a numeric comparison-guarded
/// `loop`/`recur` that WOULD run to a huge bound, given enough fuel/time.
/// A tiny fuel budget must still stop it well short of that bound, which
/// only passes if `compile::exec::exec_num_loop`'s own fuel-local
/// bookkeeping (independent of `Interp::tick_fuel`, see that fn's doc) is
/// wired up correctly.
#[test]
fn numloop_shaped_loop_terminates_on_fuel_exhaustion() {
    let mut interp = interp_for(true, true, Some(1_000));
    let err = interp
        .eval_str(
            "fuel-test",
            "(loop [i 0] (if (< i 100000000000) (recur (inc i)) i))",
        )
        .expect_err("NumLoop-shaped loop should have hit fuel exhaustion");
    assert_eq!(err.kind, ErrorKind::FuelExhausted, "wrong error kind: {err:?}");
}

/// W-NUMLOOP: the same shape with a NIL-terminal exit (`when`-shaped), which
/// specializes too. Two things to pin: fuel still stops it (the exit branch
/// is different, the fuel counter is not), and under a sufficient budget it
/// returns `nil` -- with the remaining fuel written back through the new
/// `NumLoopExit::Nil` arm exactly as through the numeric one, which the
/// second half checks by spending the SAME interpreter twice.
#[test]
fn nil_exit_numloop_respects_fuel_in_both_directions() {
    let mut interp = interp_for(true, true, Some(1_000));
    let err = interp
        .eval_str("fuel-test", "(loop [i 0] (when (< i 100000000000) (recur (inc i))))")
        .expect_err("nil-exit NumLoop should have hit fuel exhaustion");
    assert_eq!(err.kind, ErrorKind::FuelExhausted, "wrong error kind: {err:?}");

    for (compile, numloop) in TIER_COMBOS {
        let mut interp = interp_for(compile, numloop, Some(1_000_000));
        let v = interp
            .eval_str("fuel-test", "(loop [i 0] (when (< i 1000) (recur (inc i))))")
            .unwrap_or_else(|e| panic!("compile={compile} numloop={numloop}: unexpected error: {e:?}"));
        assert_eq!(
            mova::internal::pr_str(&v),
            "nil",
            "compile={compile} numloop={numloop}: wrong result"
        );
        // A second loop in the SAME interpreter: only reachable if the
        // first one wrote its remaining budget back rather than leaving the
        // pre-loop value (or zero) behind.
        let v = interp
            .eval_str("fuel-test", "(loop [i 0] (when (< i 1000) (recur (inc i))))")
            .unwrap_or_else(|e| panic!("compile={compile} numloop={numloop}: second loop: {e:?}"));
        assert_eq!(mova::internal::pr_str(&v), "nil");
    }
}

/// Requirement 2: a program that legitimately needs ~N steps succeeds, and
/// produces the RIGHT value, when fuel > N -- fuel bookkeeping must be
/// observationally transparent to a script that stays under budget.
#[test]
fn program_under_budget_succeeds_with_correct_value_every_tier() {
    for (compile, numloop) in TIER_COMBOS {
        // ~1000 loop back-edges + ~1000 calls to `inc`/`<` etc; 1_000_000
        // fuel is comfortably above whatever the exact per-iteration tick
        // count turns out to be.
        let mut interp = interp_for(compile, numloop, Some(1_000_000));
        let v = interp
            .eval_str(
                "fuel-test",
                "(loop [i 0] (if (< i 1000) (recur (inc i)) i))",
            )
            .unwrap_or_else(|e| panic!("compile={compile} numloop={numloop}: unexpected error: {e:?}"));
        assert_eq!(
            mova::internal::pr_str(&v),
            "1000",
            "compile={compile} numloop={numloop}: wrong result"
        );
    }
}

/// A fn-self-recur program under budget, twin of the above for the OTHER
/// back-edge shape.
#[test]
fn fn_self_recur_under_budget_succeeds_every_tier() {
    for (compile, numloop) in TIER_COMBOS {
        let mut interp = interp_for(compile, numloop, Some(1_000_000));
        let v = interp
            .eval_str(
                "fuel-test",
                "(defn count-to [n acc] (if (< acc n) (recur n (inc acc)) acc)) (count-to 1000 0)",
            )
            .unwrap_or_else(|e| panic!("compile={compile} numloop={numloop}: unexpected error: {e:?}"));
        assert_eq!(
            mova::internal::pr_str(&v),
            "1000",
            "compile={compile} numloop={numloop}: wrong result"
        );
    }
}

/// `fuel: None` (the default / disabled state) must never raise
/// `FuelExhausted`, no matter how many steps a program takes -- sanity
/// check that the mechanism really is opt-in.
#[test]
fn fuel_none_never_exhausts() {
    let mut interp = Interp::new();
    assert_eq!(interp.fuel, None);
    let v = interp
        .eval_str("fuel-test", "(loop [i 0] (if (< i 200000) (recur (inc i)) i))")
        .expect("unlimited fuel should never error");
    assert_eq!(mova::internal::pr_str(&v), "200000");
}

/// Requirement 3: script-level `(try .. (catch e ..))` must NOT be able to
/// swallow fuel exhaustion -- the catch body must never run, and the error
/// must still propagate all the way out to the host as `Err`.
#[test]
fn try_catch_does_not_mask_fuel_exhaustion_every_tier() {
    for (compile, numloop) in TIER_COMBOS {
        let mut interp = interp_for(compile, numloop, Some(1_000));
        let err = interp
            .eval_str(
                "fuel-test",
                "(try (loop [] (recur)) (catch e :caught))",
            )
            .expect_err(&format!(
                "compile={compile} numloop={numloop}: try/catch should not have masked fuel exhaustion"
            ));
        assert_eq!(
            err.kind,
            ErrorKind::FuelExhausted,
            "compile={compile} numloop={numloop}: try/catch masked the error (or changed its kind): {err:?}"
        );
    }
}

/// Same as above, but with `finally` present too: `finally` is documented
/// to always run (see `eval_try`/`exec_try`), but it must not turn the
/// fuel-exhaustion `Err` into an `Ok` by, say, swallowing it silently --
/// the original error must still win once `finally` itself completes
/// cleanly.
#[test]
fn finally_runs_but_does_not_mask_fuel_exhaustion() {
    let mut interp = interp_for(true, true, Some(1_000));
    interp
        .eval_str("fuel-test", "(def finally-ran (atom false))")
        .unwrap();
    let err = interp
        .eval_str(
            "fuel-test",
            "(try (loop [] (recur)) (catch e :caught) (finally (reset! finally-ran true)))",
        )
        .expect_err("fuel exhaustion should still propagate through a try/finally");
    assert_eq!(err.kind, ErrorKind::FuelExhausted, "wrong error kind: {err:?}");
    let ran = interp.eval_str("fuel-test", "@finally-ran").unwrap();
    assert_eq!(mova::internal::pr_str(&ran), "true", "finally should still have run");
}

/// The host embedding mova sees fuel exhaustion as a perfectly ordinary
/// `Result::Err` from the public `eval_str`/`eval_form` entry points --
/// nothing special is required of a caller beyond the usual `?`/`match`.
#[test]
fn host_can_catch_fuel_exhaustion_as_an_ordinary_result_err() {
    let mut interp = interp_for(true, true, Some(1_000));
    let result: Result<mova::internal::Value, mova::internal::RjError> =
        interp.eval_str("fuel-test", "(loop [] (recur))");
    match result {
        Err(e) if e.kind == ErrorKind::FuelExhausted => {} // host caught it, as expected
        Err(e) => panic!("wrong error kind reached the host: {e:?}"),
        Ok(_) => panic!("host should have seen an Err"),
    }
}
