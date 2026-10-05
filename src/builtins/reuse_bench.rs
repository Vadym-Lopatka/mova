//! Perceus-lite phase 1 tracking probe: what the consuming calling
//! convention (`builtins::reuse`) is worth, measured through the REAL
//! interpreter in BOTH tiers.
//!
//! `#[ignore]`d tracking probes, not gates -- `compile::bench`'s pattern,
//! `map_bench`'s `[min, max]`-over-5-rounds-after-1-warmup discipline.
//!
//! ```text
//! cargo test --release --lib builtins::reuse_bench -- --ignored --nocapture
//! ```
//!
//! ## The two families, and why both must be here
//!
//! E3's probe (B) measured a 3.65x (n=7) to ~10.9x (n=10000) gap between
//! mutating a UNIQUELY-held persistent map and mutating a SHARED one, and
//! concluded the gap was unrealizable because every builtin received
//! `&[Value]` and therefore always held a shared handle. Phase 1 removes
//! that structural barrier -- but removing the barrier is not the same as
//! the receiver actually being unique, and the difference is exactly what
//! these two families separate:
//!
//! * **`live_binding_*`** -- `(reduce (fn [m i] (assoc m i i)) {} ...)`.
//!   The receiver `m` is a closure frame slot that is STILL BOUND when
//!   `assoc` runs, so the handle phase 1 takes ownership of is not the
//!   only one. This is the CONTROL: it should tie, and the amount by
//!   which it fails to improve is the budget for phase 2 (last-use
//!   analysis, which is what would let the frame slot be moved from).
//!
//! * **`temporary_*`** -- chained and multi-pair assoc, where every
//!   receiver after the first is a value only the native's own frame can
//!   see. This is the case phase 1 CAN win, and does.
//!
//! Run with `MOVA_NO_REUSE=1` to get the same numbers with the consuming
//! path forced off -- that is the in-process A/B, and the difference
//! between the two runs is phase 1's contribution.

use std::hint::black_box;
use std::time::{Duration, Instant};

use crate::eval::Interp;

const WARMUP_ROUNDS: u32 = 1;
const MEASURED_ROUNDS: u32 = 5;

/// One timed pass = evaluating `src` once in an already-built session.
fn timed_rounds(mut f: impl FnMut()) -> Vec<Duration> {
    for _ in 0..WARMUP_ROUNDS {
        f();
    }
    (0..MEASURED_ROUNDS)
        .map(|_| {
            let t0 = Instant::now();
            f();
            t0.elapsed()
        })
        .collect()
}

fn report(label: &str, tier: &str, ops: u64, times: &[Duration]) {
    let ns: Vec<f64> = times.iter().map(|d| d.as_secs_f64() * 1e9 / ops as f64).collect();
    let min = ns.iter().cloned().fold(f64::INFINITY, f64::min);
    let max = ns.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    println!("{label:34} {tier:9} {ops:>9} ops/round  [{min:>8.2}, {max:>8.2}] ns/op");
}

/// Evaluates `src` (a whole program ending in the expression under test)
/// `MEASURED_ROUNDS`+1 times in a fresh session of each tier and reports
/// ns per collection-op.
fn bench_both_tiers(label: &str, src: &str, ops_per_eval: u64) {
    for compiled in [true, false] {
        let tier = if compiled { "compiled" } else { "walked" };
        let mut interp = if compiled {
            Interp::new()
        } else {
            Interp::with_compile_enabled(false)
        };
        let times = timed_rounds(|| {
            let v = interp
                .eval_str("reuse_bench", src)
                .unwrap_or_else(|e| panic!("{label}: {}", e.message));
            black_box(v);
        });
        report(label, tier, ops_per_eval, &times);
    }
}

/// CONTROL: the receiver is a live closure binding, so phase 1 takes a
/// shared handle and can only save the redundant clone, not the copy.
/// N straddles `PMAP_SMALL_MAX` (8): 8 stays Small, 100 and 10000 promote.
#[test]
#[ignore = "tracking probe, not a gate: cargo test --release --lib builtins::reuse_bench -- --ignored --nocapture"]
fn bench_live_binding_assoc() {
    for (n, r) in [(8u64, 20_000u64), (100, 2_000), (10_000, 20)] {
        bench_both_tiers(
            &format!("live-binding-assoc n={n}"),
            &format!(
                "(dotimes [_ {r}] (reduce (fn [m i] (assoc m i i)) {{}} (range {n})))"
            ),
            n * r,
        );
    }
}

/// CONTROL, vector twin. N straddles `PVEC_SMALL_MAX` (16).
#[test]
#[ignore = "tracking probe, not a gate: cargo test --release --lib builtins::reuse_bench -- --ignored --nocapture"]
fn bench_live_binding_conj() {
    for (n, r) in [(8u64, 20_000u64), (100, 2_000), (10_000, 20)] {
        bench_both_tiers(
            &format!("live-binding-conj n={n}"),
            &format!("(dotimes [_ {r}] (reduce (fn [v i] (conj v i)) [] (range {n})))"),
            n * r,
        );
    }
}

/// THE WIN CASE: every receiver but the first is a temporary the native
/// solely owns. `small` stays in `PMap::Small` (`Arc::make_mut`/
/// `Arc::get_mut` reuse); `big` is a 200-entry HAMT (imbl chunk reuse,
/// which is where E4b priced the headroom highest).
#[test]
#[ignore = "tracking probe, not a gate: cargo test --release --lib builtins::reuse_bench -- --ignored --nocapture"]
fn bench_temporary_receiver_assoc() {
    // 4 assoc ops per expression, 3 of them on a sole-owned temporary.
    bench_both_tiers(
        "temporary-assoc small (chained)",
        "(def b {:k 0}) (dotimes [_ 20000] (assoc (assoc (assoc (assoc b :a 1) :b 2) :c 3) :d 4))",
        4 * 20_000,
    );
    bench_both_tiers(
        "temporary-assoc small (multi-kv)",
        "(def b {:k 0}) (dotimes [_ 20000] (assoc b :a 1 :b 2 :c 3 :d 4))",
        4 * 20_000,
    );
    bench_both_tiers(
        "temporary-assoc big (chained)",
        "(def b (reduce (fn [m i] (assoc m i i)) {} (range 200))) \
         (dotimes [_ 20000] (assoc (assoc (assoc (assoc b :a 1) :b 2) :c 3) :d 4))",
        4 * 20_000,
    );
    bench_both_tiers(
        "temporary-assoc big (multi-kv)",
        "(def b (reduce (fn [m i] (assoc m i i)) {} (range 200))) \
         (dotimes [_ 20000] (assoc b :a 1 :b 2 :c 3 :d 4))",
        4 * 20_000,
    );
}

/// THE WIN CASE, vectors.
#[test]
#[ignore = "tracking probe, not a gate: cargo test --release --lib builtins::reuse_bench -- --ignored --nocapture"]
fn bench_temporary_receiver_conj() {
    bench_both_tiers(
        "temporary-conj small (chained)",
        "(def v [1 2 3]) (dotimes [_ 20000] (conj (conj (conj (conj v 4) 5) 6) 7))",
        4 * 20_000,
    );
    bench_both_tiers(
        "temporary-conj big (chained)",
        "(def v (reduce (fn [a i] (conj a i)) [] (range 200))) \
         (dotimes [_ 20000] (conj (conj (conj (conj v 4) 5) 6) 7))",
        4 * 20_000,
    );
}
