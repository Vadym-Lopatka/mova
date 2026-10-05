//! Compiled-tier tracking probes (COMPILE-TIER-DESIGN.md, S3).
//!
//! These are `#[ignore]`d tests, not gated assertions: they print
//! throughput for the two shapes the tier exists to make cheap, in BOTH
//! tiers, so a regression is visible in one command and the "compiled /
//! walked" ratio can be recorded in bench/optimization-log.md.
//!
//! ```text
//! cargo test --release compile::bench -- --ignored --nocapture
//! ```
//!
//! Run them in `--release`: a debug build measures `rustc -O0`, not the
//! tier. They live inside the crate (rather than in `tests/`) because
//! `Interp::call` -- the dispatch path a `map`/`filter`/flow step-fn takes,
//! and the one probe (a) is about -- is `pub(crate)`.

use std::time::Instant;

use crate::eval::Interp;
use crate::value::Value;

/// Evaluates `src` in a fresh session of the requested tier and returns the
/// interpreter plus the last form's value (the fn under test).
fn session(compiled: bool, src: &str) -> (Interp, Value) {
    let mut interp = if compiled {
        Interp::new()
    } else {
        Interp::with_compile_enabled(false)
    };
    let v = interp
        .eval_str("bench", src)
        .unwrap_or_else(|e| panic!("{src}: {}", e.message));
    (interp, v)
}

fn report(label: &str, tier: &str, iters: u64, elapsed: std::time::Duration) {
    let per_sec = iters as f64 / elapsed.as_secs_f64();
    println!(
        "{label:24} {tier:9} {iters:>10} in {:>8.3}s = {per_sec:>12.0}/s",
        elapsed.as_secs_f64()
    );
}

/// (a) Call-dispatch throughput: how expensive one mova-level call of a
/// trivial fn is, end to end (arity select, depth guard, stack frame, frame
/// slots, body). This is the multiplier on every `map`/`filter`/`reduce`
/// element and every flow step-fn invocation.
#[test]
#[ignore = "tracking probe, not a gate: cargo test --release -- --ignored --nocapture"]
fn bench_call_dispatch() {
    const CALLS: u64 = 2_000_000;
    for compiled in [true, false] {
        let (mut interp, f) = session(compiled, "(defn probe [x] x)");
        let arg = [Value::Int(1)];
        let t0 = Instant::now();
        for _ in 0..CALLS {
            interp.call(&f, &arg).expect("probe call failed");
        }
        report(
            "call-dispatch",
            if compiled { "compiled" } else { "walked" },
            CALLS,
            t0.elapsed(),
        );
    }
}

/// The multiply-then-add recurrence probes (b), (b') and (b'') all measure.
///
/// It used to be a real 64-bit LCG (`acc' = acc * 6364136223846793005 +
/// 1442695040888963407`, the constants `bench/flow-gen-sink-w2000.mova`'s
/// `burn` uses), which overflows `i64` on its very first iteration. Under
/// v0.4 that silently promoted to `f64` (a documented deviation); the W4C
/// compat campaign made mova's arithmetic oracle-exact instead, so an
/// `i64` overflow now THROWS (`ArithmeticException: long overflow`) -- and
/// these three probes have been panicking in their warmup call ever since,
/// unnoticed because they are `#[ignore]`d tracking probes that no gate
/// runs. (`bench_lcg_ceiling`'s S5 fixup even asserted in a comment that
/// "this benchmark's LCG never overflows"; it was never run to check.)
///
/// `bench/fuel-lcg.mova` hit exactly this and resolved it the same way, for
/// the same reasons, which are reproduced there at length: there is no
/// `rem`/`mod`/bitwise-and in the NumLoop grammar to re-bound a genuine
/// multiplicative accumulator (`ir::NumBin` has only `+`, `-`, `*` and the
/// fold variants), and any integer multiplier of magnitude > 1 diverges
/// past `i64::MAX` within ~63 iterations regardless, so "a real LCG that
/// also never overflows" is not an available shape.
///
/// What is kept is the actual point of these probes -- a multiply-then-add
/// NumLoop body, timed per iteration -- via a multiplier of `-1`:
/// `acc' = (acc * -1) + K`, `K` the original additive constant. That is a
/// period-2 oscillation between `seed` and `K - seed`, bounded for ANY
/// iteration count by construction (there is no growth step to overflow),
/// while still driving the `Mul` and `Add` register ops every iteration. It
/// is no longer a usable PRNG; it was never being read as one here.
///
/// Using the same recurrence as `bench/fuel-lcg.mova` also makes the Rust
/// probe and the `.mova` probe directly comparable, which they previously
/// were not.
const LCG_MUL: i64 = -1;
const LCG_ADD: i64 = 1_442_695_040_888_963_407;

const LCG_SRC: &str = "(defn burn [seed] \
                         (loop [i 0 acc seed] \
                           (if (< i 2000) \
                             (recur (inc i) (+ (* acc -1) 1442695040888963407)) \
                             acc)))";

/// (b) The W=2000 loop of bench/flow-gen-sink-w2000.mova, in isolation:
/// `loop`/`recur` plus the arithmetic intrinsics, with no channel or flow
/// engine in the way. Reported per LOOP ITERATION, since that is the unit
/// the flow bench's WORK parameter counts. See [`LCG_SRC`] for why the
/// recurrence is a `-1` multiply rather than the original LCG's.
#[test]
#[ignore = "tracking probe, not a gate: cargo test --release -- --ignored --nocapture"]
fn bench_lcg_loop() {
    const WORK: u64 = 2_000;
    // W1 (LATENCY-CAMPAIGN.md): raised from 1,000 to 5,000 -- once the
    // compiled leg's inner loop got fast enough (lane variants), 1,000
    // calls' worth of PER-CALL dispatch overhead (arity check, frame
    // setup) stopped being negligible next to the loop itself and started
    // dominating the "compiled" row's reported rate. 5,000 (10M total
    // iterations) amortizes that back down to noise while keeping the
    // "walked" row (still ~1M iters/s) under ~10s.
    const CALLS: u64 = 5_000;
    for compiled in [true, false] {
        let (mut interp, f) = session(compiled, LCG_SRC);
        let arg = [Value::Int(42)];
        // One untimed warmup call per leg, so the first call's one-time
        // costs (fault-in fn's compiled code, first frame allocation) don't
        // land inside the timed region.
        interp.call(&f, &arg).expect("burn warmup call failed");
        let t0 = Instant::now();
        for _ in 0..CALLS {
            interp.call(&f, &arg).expect("burn call failed");
        }
        report(
            "lcg-loop (iters)",
            if compiled { "compiled" } else { "walked" },
            CALLS * WORK,
            t0.elapsed(),
        );
    }
}

/// (b'') The same LCG loop as (b), COMPILED LEG ONLY, reported as a median
/// of 5 rounds with [min-max] -- the A/B instrument W5 (lane-op fusion,
/// `bench/optimization-log.md`) measures with. (b) runs its tree-walked leg
/// too (~9.4s per invocation at 1M iters/s), which makes a 5-round
/// interleaved A/B of the COMPILED number needlessly expensive and adds
/// nine seconds of unrelated work between samples; this probe exists so the
/// fusion A/B can alternate binaries within one session cheaply. Same
/// source, same `CALLS`/`WORK`, same warmup discipline as (b).
#[test]
#[ignore = "tracking probe, not a gate: cargo test --release -- --ignored --nocapture"]
fn bench_lcg_loop_compiled_rounds() {
    const WORK: u64 = 2_000;
    const CALLS: u64 = 5_000;
    const ROUNDS: usize = 5;
    let (mut interp, f) = session(true, LCG_SRC);
    let arg = [Value::Int(42)];
    interp.call(&f, &arg).expect("burn warmup call failed");
    let mut rates = Vec::with_capacity(ROUNDS);
    for _ in 0..ROUNDS {
        let t0 = Instant::now();
        for _ in 0..CALLS {
            interp.call(&f, &arg).expect("burn call failed");
        }
        rates.push((CALLS * WORK) as f64 / t0.elapsed().as_secs_f64());
    }
    rates.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!(
        "lcg-loop compiled: median {:.1}M iters/s  [{:.1}-{:.1}M]",
        rates[ROUNDS / 2] / 1e6,
        rates[0] / 1e6,
        rates[ROUNDS - 1] / 1e6,
    );
}

/// (b') The CEILING for (b): the very same LCG recurrence, written straight
/// in Rust, at four levels of fidelity to mova's semantics. Nothing that
/// runs mova source can beat these; the interesting number is what fraction
/// of `mova-exact` the tier reaches.
///
/// - `wrap-i64`: what the loop would cost if `*`/`+` wrapped like C.
/// - `f64-only`: what it would cost with no integer path at all.
/// - `mova-exact`: `builtins::numbers`'s own checked-and-promoting
///   arithmetic over the unboxed `Num`, in Rust locals -- mova's semantics,
///   with zero interpretive overhead. This is the honest ceiling.
/// - `numop-machine`: the same, but executed as `compile::exec`'s
///   `Ir::NumLoop` op list would execute it (a register file plus a flat
///   three-address program), hand-inlined here so the interpretive overhead
///   of that machine can be read off directly against the row above.
#[test]
#[ignore = "tracking probe, not a gate: cargo test --release -- --ignored --nocapture"]
fn bench_lcg_ceiling() {
    use crate::builtins::numbers::{add, mul, as_f64, Num};

    const N: u64 = 2_000_000;
    // The same recurrence (b) and (b'') run -- see `LCG_SRC`. All four rows
    // below use it, because a ceiling table is only readable if every row
    // computes the identical program.
    const M: i64 = LCG_MUL;
    const C: i64 = LCG_ADD;

    let t0 = Instant::now();
    let mut acc: i64 = 42;
    for i in 0..N {
        acc = acc.wrapping_mul(M).wrapping_add(C);
        std::hint::black_box(i);
    }
    std::hint::black_box(acc);
    report("lcg-ceiling", "wrap-i64", N, t0.elapsed());

    let t0 = Instant::now();
    let mut accf: f64 = 42.0;
    for i in 0..N {
        accf = accf * M as f64 + C as f64;
        std::hint::black_box(i);
    }
    std::hint::black_box(accf);
    report("lcg-ceiling", "f64-only", N, t0.elapsed());

    // mova-exact: the loop as `numbers` would compute it, in locals.
    //
    // The operands go through `black_box` so this stays a real dependency
    // chain: with `LCG_MUL` the recurrence is a 2-cycle, and LLVM will
    // otherwise recognize that and collapse the loop -- which reported an
    // impossible 1.78G iters/s (0.56 ns for six checked ops) and made the
    // row useless as a ceiling. Opaque constants keep the arithmetic in
    // registers while denying the algebraic shortcut, which is exactly the
    // position `run_num_ops` is in.
    let one = std::hint::black_box(Num::I(1));
    let zero = std::hint::black_box(Num::I(0));
    let m = std::hint::black_box(Num::I(M));
    let c = std::hint::black_box(Num::I(C));
    let t0 = Instant::now();
    let mut i = Num::I(0);
    let mut a = Num::I(42);
    loop {
        if as_f64(i) >= 2000.0 * 1000.0 {
            break;
        }
        // S5: the step fns are fallible now (checked `i64` arithmetic
        // throws on overflow). With `LCG_MUL` the recurrence really is
        // bounded by construction (a 2-cycle), so unwrapping here measures
        // the same instruction sequence it always did, with the same
        // taken/not-taken overflow branch the real `run_num_ops` has.
        i = add(i, one).expect("bench recurrence never overflows");
        a = add(
            add(zero, mul(mul(one, a).unwrap(), m).unwrap()).unwrap(),
            c,
        )
        .expect("bench recurrence never overflows");
    }
    std::hint::black_box(as_f64(a));
    report("lcg-ceiling", "mova-ex", N, t0.elapsed());

    // The same program, run by a replica of `Ir::NumLoop`'s op machine.
    #[derive(Clone, Copy)]
    enum Bin {
        Add,
        Mul,
    }
    struct Op {
        dst: u8,
        op: Bin,
        a: u8,
        b: u8,
    }
    // r0 = i, r1 = acc, r2 = 0, r3 = 1, r4 = 2e6, r5 = M, r6 = C, r7..= temps
    let mut regs = [Num::I(0); 16];
    regs[2] = Num::I(0);
    regs[3] = Num::I(1);
    regs[4] = Num::I(2_000_000);
    regs[5] = Num::I(M);
    regs[6] = Num::I(C);
    regs[1] = Num::I(42);
    let ops = [
        Op { dst: 7, op: Bin::Add, a: 0, b: 3 },  // i+1
        Op { dst: 8, op: Bin::Mul, a: 3, b: 1 },  // 1*acc
        Op { dst: 8, op: Bin::Mul, a: 8, b: 5 },  // *M
        Op { dst: 9, op: Bin::Add, a: 2, b: 8 },  // 0+that
        Op { dst: 9, op: Bin::Add, a: 9, b: 6 },  // +C
    ];
    // Opaque, so the program is a *runtime* op list (as it is in the tier)
    // rather than something LLVM can unroll and constant-fold away.
    let ops: &[Op] = std::hint::black_box(&ops[..]);
    let t0 = Instant::now();
    loop {
        if as_f64(regs[0]) >= as_f64(regs[4]) {
            break;
        }
        for op in ops {
            let x = regs[op.a as usize & 15];
            let y = regs[op.b as usize & 15];
            regs[op.dst as usize & 15] = match op.op {
                Bin::Add => add(x, y).expect("bench recurrence never overflows"),
                Bin::Mul => mul(x, y).expect("bench recurrence never overflows"),
            };
        }
        let (x, y) = (regs[7], regs[9]);
        regs[0] = x;
        regs[1] = y;
    }
    std::hint::black_box(as_f64(regs[1]));
    report("lcg-ceiling", "numop-m", N, t0.elapsed());
}

/// (d) Closure CHURN from compiled code: one closure created per call, the
/// shape every `lazy-seq` thunk and every `(fn [x] ..)` passed to `map` has.
/// Before S4 this was `eval_fn_form` -> `parse_fn_like` (deep-cloning the
/// body forms) -> a full `compile_fn` attempt, per creation; now it is
/// `Ir::MakeClosure`: a capture snapshot plus two `Arc` bumps against a
/// template compiled once.
#[test]
#[ignore = "tracking probe, not a gate: cargo test --release -- --ignored --nocapture"]
fn bench_closure_churn() {
    const CALLS: u64 = 300_000;
    for compiled in [true, false] {
        let (mut interp, f) = session(compiled, "(defn mk [n] (fn [] (+ n 1)))");
        let arg = [Value::Int(1)];
        let t0 = Instant::now();
        for _ in 0..CALLS {
            interp.call(&f, &arg).expect("mk call failed");
        }
        report(
            "closure-churn",
            if compiled { "compiled" } else { "walked" },
            CALLS,
            t0.elapsed(),
        );
    }
}

/// (e) Closure churn from a fn that does NOT compile (`macroexpand-1` in the
/// body), so the inner `fn` form is created by the tree-walker on every
/// call. With the tier ON, each of those creations pays a full compile
/// ATTEMPT that is thrown away when the closure dies; with it OFF, nothing.
/// The gap between the two rows is the entire cost of "compile on every
/// tree-walked closure creation" -- the thing a template cache would have to
/// beat. See bench/optimization-log.md for the S4 measurement and decision.
#[test]
#[ignore = "tracking probe, not a gate: cargo test --release -- --ignored --nocapture"]
fn bench_closure_churn_from_tree_walked_code() {
    const CALLS: u64 = 300_000;
    for compiled in [true, false] {
        let (mut interp, f) = session(
            compiled,
            "(defn mk [n] (if false (macroexpand-1 n)) (fn [] (+ n 1)))",
        );
        let arg = [Value::Int(1)];
        let t0 = Instant::now();
        for _ in 0..CALLS {
            interp.call(&f, &arg).expect("mk call failed");
        }
        report(
            "churn (walked parent)",
            if compiled { "compiled" } else { "walked" },
            CALLS,
            t0.elapsed(),
        );
    }
}

/// (c) The same loop shape, but in a closure created under LIVE tree-walked
/// frames -- the `(future (loop ...))` feeder thunk of
/// bench/flow-gen-sink.mova. Before S2.5 this whole fn fell back to the
/// tree-walker because of its free globals (`<`, `inc`), so the
/// compiled/walked ratio here is exactly what S2.5 bought.
#[test]
#[ignore = "tracking probe, not a gate: cargo test --release -- --ignored --nocapture"]
fn bench_loop_under_creation_env() {
    const WORK: u64 = 2_000;
    const CALLS: u64 = 1_000;
    for compiled in [true, false] {
        let (mut interp, f) = session(
            compiled,
            "(let [n 2000] (fn [] (loop [i 0] (if (< i n) (recur (inc i)) i))))",
        );
        let t0 = Instant::now();
        for _ in 0..CALLS {
            interp.call(&f, &[]).expect("thunk call failed");
        }
        report(
            "creation-env loop",
            if compiled { "compiled" } else { "walked" },
            CALLS * WORK,
            t0.elapsed(),
        );
    }
}
