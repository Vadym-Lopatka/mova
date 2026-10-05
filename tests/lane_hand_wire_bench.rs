//! W1 kill-probe 1b (LATENCY-CAMPAIGN.md §3): "Hand-wire (in a test or
//! scratch module inside this worktree) a lane execution of the REAL
//! flow-gen-sink-w2000 LCG loop shape. BAR: <250M iters/s -> shrink to the
//! two-variant design."
//!
//! This is a from-scratch port of the design (not a re-run of the
//! scratchpad `dispatch-probe` crate, which is a different process/binary):
//! a stack-local lane register file (two fixed-size arrays, `i64`/`f64`,
//! NOT boxed/heap `Vec`s -- see the module doc on why that matters), an op
//! list typed per the `feasible_worlds` output the W1 kill-probe 1a run
//! confirmed (2 worlds for this exact loop: `i:Int` always, `acc:Int` on
//! iteration 0 then `acc:Float` forever after -- this hand-wire runs the
//! STEADY STATE lane, `i:Int acc:Float`, which is what the real loop
//! spends 1999 of its 2000 iterations in), `checked_add` on the one
//! `Int`-lane op with a deopt branch that this program never takes.
//!
//! Run with `cargo test --release --test lane_hand_wire_bench -- --nocapture`.

use std::hint::black_box;
use std::time::Instant;

// The EXACT constants and shape from bench/flow-gen-sink-w2000.mova's `burn`:
//   (loop [i 0 acc seed]
//     (if (< i k) (recur (inc i) (+ (* acc 6364136223846793005) 1442695040888963407)) acc))
const M: i64 = 6_364_136_223_846_793_005;
const C: i64 = 1_442_695_040_888_963_407;
const WORK: i64 = 2_000;

/// The stack-local lane register file: fixed-size arrays living in the
/// caller's stack frame (not a heap `Vec`, not a `Box` -- `#[inline(never)]`
/// on the runner plus a disassembly spot check, see below, confirms this
/// compiles to register/stack traffic only, never a per-op memory spill
/// through a pointer indirection).
struct LaneRegs {
    ir: [i64; 4],
    fr: [f64; 4],
}

/// One op in the steady-state `i:Int acc:Float` lane for this loop's body:
/// `i' = i + 1` (checked, deopts -- never taken here) and
/// `acc' = acc * M + C`, fused into one FMulAdd-shaped instruction (TWO
/// roundings: mul then add, matching `builtins::numbers::add(mul(acc,M),C)`
/// exactly -- not an actual FMA, which would change the bit pattern).
///
/// Returns `Some(())` on success, `None` on `i`-lane overflow (deopt to the
/// tagged machine -- modeled by returning `None`; a real caller would
/// re-enter `exec_num_loop`'s tagged path on this iteration).
#[inline(always)]
fn lane_step(r: &mut LaneRegs) -> Option<()> {
    // ir[0]=i ir[1]=one(const)   fr[0]=acc fr[1]=M(const) fr[2]=C(const)
    let next_i = r.ir[0].checked_add(r.ir[1])?;
    let next_acc = r.fr[0] * r.fr[1] + r.fr[2];
    r.ir[0] = next_i;
    r.fr[0] = next_acc;
    Some(())
}

/// The hand-wired lane loop: entry seeds the registers (modeling the ONE
/// tagged warmup iteration that promotes `acc` to `Float`, exactly as
/// `spec_run`/`lane_lcg` do in the validated scratchpad model), then runs
/// the steady-state lane until `i` reaches `k`. `#[inline(never)]` so the
/// call boundary the real `exec_num_loop` -> `run_num_loop`-shaped split
/// would have is actually exercised, not inlined away into the caller's
/// loop-over-CALLS benchmark driver.
#[inline(never)]
fn run_lane_lcg(k: i64, seed: i64) -> f64 {
    let mut r = LaneRegs { ir: [0; 4], fr: [0.0; 4] };
    r.ir[1] = 1;
    r.fr[1] = M as f64;
    r.fr[2] = C as f64;
    // Iteration 0 in the tagged machine (the real entry guard + tag
    // observation): acc = seed*M + C (an Int*Int checked-mul that overflows
    // i64 on this multiplier for any nonzero seed, promoting to Float --
    // see flow-gen-sink-w2000.mova's own header note), i = 1.
    r.fr[0] = (seed as f64) * (M as f64) + (C as f64);
    r.ir[0] = 1;
    while r.ir[0] < k {
        match lane_step(&mut r) {
            Some(()) => {}
            None => unreachable!("no overflow on i for WORK=2000"),
        }
    }
    r.fr[0]
}

fn expected(k: i64, seed: i64) -> f64 {
    let mut i = 0i64;
    let mut acc = seed as f64;
    let mut first = true;
    while i < k {
        acc = if first && i == 0 {
            // first iteration still goes through the Int seed multiply in
            // the real semantics, but the RESULT is what matters here.
            (seed as f64) * (M as f64) + (C as f64)
        } else {
            acc * (M as f64) + (C as f64)
        };
        first = false;
        i += 1;
    }
    acc
}

#[test]
fn hand_wired_lane_matches_reference() {
    for seed in [0i64, 1, 42, -1, i64::MAX, i64::MIN] {
        let want = expected(WORK, seed);
        let got = run_lane_lcg(WORK, seed);
        assert_eq!(got.to_bits(), want.to_bits(), "seed={seed}");
    }
}

/// The kill-probe proper: median of 5 rounds, BAR = 250M iters/s.
#[test]
fn hand_wired_lane_clears_250m_iters_per_s() {
    const CALLS: u64 = 20_000; // WORK=2000 * 20_000 = 40M iterations/round
    const TOTAL: u64 = WORK as u64 * CALLS;

    let run = || -> f64 {
        let mut acc = 0.0;
        for c in 0..CALLS {
            acc += run_lane_lcg(black_box(WORK), black_box(42 + c as i64));
        }
        acc
    };

    // warmup
    black_box(run());

    let mut rates = Vec::new();
    for _ in 0..5 {
        let t0 = Instant::now();
        let sink = run();
        let dt = t0.elapsed().as_secs_f64();
        black_box(sink);
        rates.push(TOTAL as f64 / dt);
    }
    rates.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let min = rates[0];
    let max = rates[rates.len() - 1];
    let med = rates[rates.len() / 2];
    println!(
        "\n=== W1 kill-probe 1b: hand-wired lane, w2000 LCG shape ===\n  median {:.1}M iters/s  [{:.1}-{:.1}M]  ({:.3} ns/iter)",
        med / 1e6,
        min / 1e6,
        max / 1e6,
        1e9 / med
    );
    println!("BAR: 250M iters/s.");
    if med >= 250e6 {
        println!("*** KILL-PROBE 1b: BAR CLEARED. ***");
    } else {
        println!("*** KILL-PROBE 1b: BAR FAILED -- shrink to two hardcoded variants. ***");
    }
    assert!(
        med >= 250e6,
        "kill-probe 1b BAR failed: {:.1}M iters/s < 250M",
        med / 1e6
    );
}

// ---------------------------------------------------------------------------
// W5 autopsy probe (bench/optimization-log.md, "W5: fused lane
// superinstructions -- KILLED"): why the fused FMulAdd superinstruction
// bought nothing.
//
// The W5 kill probe measured a real fused `FMulAddFoldFI` `LaneOp` in the
// landed lane machine at 165.0M iters/s -- EXACTLY the unfused number. The
// hypothesis for that tie is that the landed machine's cost is not op-list
// DISPATCH at all but the fact that its register file is a `&mut [f64;
// NUM_REGS]` -- real memory, reloaded and restored per op, with the
// loop-carried accumulator paying a store-to-load forwarding round trip
// every iteration (twice: once for the op's own dst, once more for the
// simultaneous-rebind step). The hand-wire above cannot see that cost
// because its `LaneRegs` is a function-local struct LLVM promotes straight
// to SSA registers (mem2reg/SROA), so its 769M/s measures a loop with NO
// memory in it at all.
//
// These two probes hold everything else fixed -- same ops, same order, same
// straight-line code, no op-list walk -- and vary ONLY whether the register
// file is promotable.
// ---------------------------------------------------------------------------

/// Same body as `run_lane_lcg`, but the register file is reached through a
/// pointer laundered by `black_box`, which is exactly what a `&mut [f64;
/// NUM_REGS]` handed down from `run_num_loop_lane_aware` is to LLVM: SROA
/// cannot promote it, so every op reloads and restores its operands.
#[inline(never)]
fn run_lane_lcg_via_memory(k: i64, seed: i64) -> f64 {
    let mut regs = LaneRegs { ir: [0; 4], fr: [0.0; 4] };
    let r: &mut LaneRegs = black_box(&mut regs);
    r.ir[1] = 1;
    r.fr[1] = M as f64;
    r.fr[2] = C as f64;
    r.fr[0] = (seed as f64) * (M as f64) + (C as f64);
    r.ir[0] = 1;
    while r.ir[0] < k {
        // The landed machine's shape: op writes its own `dst` temp, then the
        // simultaneous-rebind step copies that temp back into the binding
        // register -- two stores and two loads on the accumulator chain.
        let next_i = match r.ir[0].checked_add(r.ir[1]) {
            Some(v) => v,
            None => unreachable!("no overflow on i for WORK=2000"),
        };
        r.ir[2] = next_i;
        r.fr[3] = 0.0 + (r.fr[0] * r.fr[1]) + r.fr[2];
        r.ir[0] = r.ir[2];
        r.fr[0] = r.fr[3];
    }
    r.fr[0]
}

#[test]
#[ignore = "W5 autopsy probe, not a gate: cargo test --release -- --ignored --nocapture"]
fn w5_autopsy_register_file_in_memory_vs_promoted() {
    const CALLS: u64 = 20_000;
    const TOTAL: u64 = WORK as u64 * CALLS;

    let bench = |f: &dyn Fn(i64, i64) -> f64| -> (f64, f64, f64) {
        let run = || {
            let mut acc = 0.0;
            for c in 0..CALLS {
                acc += f(black_box(WORK), black_box(42 + c as i64));
            }
            acc
        };
        black_box(run());
        let mut rates = Vec::new();
        for _ in 0..5 {
            let t0 = Instant::now();
            let sink = run();
            let dt = t0.elapsed().as_secs_f64();
            black_box(sink);
            rates.push(TOTAL as f64 / dt);
        }
        rates.sort_by(|a, b| a.partial_cmp(b).unwrap());
        (rates[2], rates[0], rates[4])
    };

    let (pm, plo, phi) = bench(&run_lane_lcg);
    let (mm, mlo, mhi) = bench(&run_lane_lcg_via_memory);
    println!(
        "\n=== W5 autopsy: same ops, only the register file differs ===\n  \
         promoted-to-registers : {:.1}M iters/s [{:.1}-{:.1}M]\n  \
         through-memory        : {:.1}M iters/s [{:.1}-{:.1}M]  ({:.2}x slower)",
        pm / 1e6,
        plo / 1e6,
        phi / 1e6,
        mm / 1e6,
        mlo / 1e6,
        mhi / 1e6,
        pm / mm
    );
}
