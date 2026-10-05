//! W-GEO kill-probe A (register ABI): "does shrinking `Value` from 72B to
//! <=16B pay for itself through the calling convention alone, independent
//! of any boxing/collection change?"
//!
//! HISTORICAL PREMISE (2026-08-22): the paragraph below describes the tree
//! BEFORE W-GEO stage 4. Since the split-box wave
//! (`docs/W-GEO-STAGE4-SPLITBOX.md`) `size_of::<Value>()` is 32 and
//! `size_of::<PVec>()` is 16 -- so `Value` no longer returns via `sret`
//! on arm64/x86-64 for the reason this probe modeled, and the 72B `Val72`
//! mock now stands for a shape the crate no longer has. The probe's own
//! assertions are on its self-contained mocks, so they still hold and its
//! recorded numbers (`docs/W-GEO-PROBE-VERDICTS.md`, Probe A) remain a
//! valid pricing of 72B-vs-16B; only the "today" in the next paragraph is
//! stale, and it is left verbatim as the record of what stage 4 was aimed
//! at. The remaining 32 -> <=16 step is Probe F's question (`Symbol`/`Str`
//! shrink), not this file's.
//!
//! `Value` today is 72 bytes (`src/value.rs::Value`, pinned by
//! `size_of_value_unchanged_by_bignum_variants`, `src/value.rs:4082`), and
//! the driver of that width is `List(PVec)`/`Vector(PVec)`: `PVec` itself
//! is 64 bytes (measured: `size_of::<mova::internal::PVec>() == 64`,
//! because `PVec::Big` inlines an `imbl::Vector<Value>`, whose `Inline`
//! chunk is sized as a function of `size_of::<Value>()` -- see the scout
//! map's note on self-referential sizing). Every recursive evaluator call
//! in `eval::Interp::eval_form_in` (`src/eval/mod.rs:1060`) returns
//! `Result<Value, RjError>` BY VALUE; on arm64 (and x86-64), an aggregate
//! that fits in <=16 bytes returns in a pair of registers, while anything
//! wider is returned via a hidden out-pointer (`sret`): the callee writes
//! the whole struct into caller-provided stack memory instead of handing
//! it back in registers. This probe prices exactly that difference, with
//! everything else (dispatch order, recursion shape, arg count) held
//! fixed between the two mocks.
//!
//! ## The two mocks
//!
//! * `Val72` -- `Int(i64) | F(f64) | Ptr(Arc<()>) | Pair(i64, i64) |
//!   Pad([u8; 64])`. `Pad` is never constructed (same trick as
//!   `dsbench/src/value.rs`'s `Value::Pad`); it exists purely to force
//!   `size_of::<Val72>() == 72`, asserted below.
//! * `Val16` -- the same three "live" variants (`Int`/`F`/`Ptr`), no pad:
//!   every payload is exactly 8 bytes, so the whole enum comes in at
//!   <=16 bytes (tag + payload), asserted below.
//!
//! ## The eval-shaped chain
//!
//! `eval_form_in`'s match dispatches, IN ORDER: a symbol form (env lookup,
//! `src/eval/mod.rs:1073`) FIRST (ahead of everything else -- the module
//! doc says why: "a symbol lookup is the most frequent thing this
//! evaluator ever does"), then self-evaluating literal atoms
//! (`src/eval/mod.rs:1144`), then a list/call form which recurses into
//! `eval_list` over its subforms and combines them
//! (`src/eval/mod.rs:1156` dispatches to `eval_list`). `eval_op` below
//! mirrors that ordering exactly: `Op::Sym` arm first, literal arms next,
//! `Op::Call` arm last, recursing over 2-3 args (real call arities in the
//! corpus are overwhelmingly 1-3) and combining them -- one
//! `#[inline(never)]` recursive fn per mock, returning `Result<ValN, ()>`
//! (`Result<Value, RjError>`'s shape, minus the error payload -- the ABI
//! question is about the `Ok` value's width, not the error arm).
//!
//! The pseudo-program is a small, fixed-depth expression tree (depth 4,
//! well-typed by construction so every call succeeds -- the ABI cost does
//! not depend on which branch is taken) generated once, deterministically,
//! by the same LCG every probe in this repo uses
//! (`tests/lane_hand_wire_bench.rs`'s `M`/`C` constants), then walked
//! `CALLS` times in the timed region -- matching that file's median-of-5,
//! `black_box`-disciplined idiom.
//!
//! Run: `cargo test --release --test geo_abi_probe -- --ignored --nocapture`

use std::hint::black_box;
use std::sync::Arc;
use std::time::Instant;

const M: u64 = 6_364_136_223_846_793_005;
const C: u64 = 1_442_695_040_888_963_407;

// ---------------------------------------------------------------------------
// The two mocks.
// ---------------------------------------------------------------------------

/// Tuned so `size_of::<Val72>() == 72`, matching real `Value` -- see module
/// doc and `dsbench/src/value.rs`'s identical trick.
const PAD_LEN: usize = 64;

#[derive(Clone)]
#[allow(dead_code)]
enum Val72 {
    Int(i64),
    F(f64),
    Ptr(Arc<()>),
    /// "Pair-ish": a two-word payload, standing in for the compound
    /// cell-ish arms `Value` has beyond bare scalars (`MapEntry`-shaped,
    /// small-tuple-shaped).
    Pair(i64, i64),
    /// Never constructed -- forces the enum to `Value`'s real width. See
    /// module doc.
    Pad([u8; PAD_LEN]),
}

#[derive(Clone)]
enum Val16 {
    Int(i64),
    F(f64),
    Ptr(Arc<()>),
    Pair(i32, i32),
}

#[test]
fn mock_sizes_match_the_probe_premise() {
    assert_eq!(std::mem::size_of::<Val72>(), 72, "Val72 must match real Value's width");
    assert!(
        std::mem::size_of::<Val16>() <= 16,
        "Val16 must be <=16B (register-returnable on arm64/x86-64): got {}",
        std::mem::size_of::<Val16>()
    );
}

// ---------------------------------------------------------------------------
// The pseudo-program: a small well-typed expression tree, LCG-generated
// once. Env slots alternate Int (even index) / Float (odd index), so a
// symbol lookup for a given expected type always hits a well-typed slot --
// no type-mismatch `Err` branches, so the two mocks' measured cost differs
// ONLY in the ABI of the `Ok` value, not in which control-flow path runs.
// ---------------------------------------------------------------------------

const ENV_SLOTS: usize = 16;

#[derive(Clone, Copy, PartialEq)]
enum Ty {
    I,
    F,
}

enum Op {
    Sym(u8),
    LitInt(i64),
    LitFloat(f64),
    /// `opcode`: 0 = add-int (arity 2), 1 = mul-float (arity 2),
    /// 2 = add3-int (arity 3), 3 = mul-float-then-truncate-to-int (arity
    /// 2, `Ty::F` args, `Ty::I` result -- the only way a `Ty::I` context
    /// spawns a `Ty::F` subtree, so the `Float` arm is actually exercised
    /// on the walk instead of being dead code under an all-`Int` root).
    /// "recurses over 2-3 args and combines".
    Call(u8, Vec<Op>),
}

#[inline]
fn lcg_next(x: &mut u64) -> u64 {
    *x = x.wrapping_mul(M).wrapping_add(C);
    *x
}

fn pick_slot(ty: Ty, lcg: &mut u64) -> u8 {
    let half = (ENV_SLOTS / 2) as u64;
    let i = lcg_next(lcg) % half;
    let idx = i * 2 + if ty == Ty::I { 0 } else { 1 };
    idx as u8
}

/// Generates one well-typed subtree of `ty`, `depth` calls deep. `depth==0`
/// bottoms out in a symbol reference (70%) or a literal (30%) -- matching
/// the real corpus, where most leaves are local bindings, not constants.
fn gen(depth: u32, ty: Ty, lcg: &mut u64) -> Op {
    if depth == 0 {
        if lcg_next(lcg) % 10 < 7 {
            Op::Sym(pick_slot(ty, lcg))
        } else if ty == Ty::I {
            Op::LitInt((lcg_next(lcg) % 1000) as i64)
        } else {
            Op::LitFloat((lcg_next(lcg) % 1000) as f64 / 7.0)
        }
    } else {
        match ty {
            Ty::I => match lcg_next(lcg) % 5 {
                0 | 1 => Op::Call(0, vec![gen(depth - 1, Ty::I, lcg), gen(depth - 1, Ty::I, lcg)]),
                2 | 3 => Op::Call(
                    2,
                    vec![
                        gen(depth - 1, Ty::I, lcg),
                        gen(depth - 1, Ty::I, lcg),
                        gen(depth - 1, Ty::I, lcg),
                    ],
                ),
                _ => Op::Call(3, vec![gen(depth - 1, Ty::F, lcg), gen(depth - 1, Ty::F, lcg)]),
            },
            Ty::F => Op::Call(1, vec![gen(depth - 1, Ty::F, lcg), gen(depth - 1, Ty::F, lcg)]),
        }
    }
}

/// Node count of `op` -- the unit `iters/s` is reported in (one node
/// visit == one recursive call returning `Result<ValN, ()>` by value).
fn node_count(op: &Op) -> u64 {
    match op {
        Op::Sym(_) | Op::LitInt(_) | Op::LitFloat(_) => 1,
        Op::Call(_, args) => 1 + args.iter().map(node_count).sum::<u64>(),
    }
}

const PROGRAM_DEPTH: u32 = 4;
const PROGRAM_SEED: u64 = 0x5EED_1234;

fn build_program() -> Op {
    let mut lcg = PROGRAM_SEED;
    gen(PROGRAM_DEPTH, Ty::I, &mut lcg)
}

// ---------------------------------------------------------------------------
// Reference evaluator: plain `i64`/`f64` recursion, independent of both
// mocks' `combine` logic, so the correctness test isn't just checking the
// two mocks agree with EACH OTHER (which could hide a shared bug).
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Num {
    I(i64),
    F(f64),
}

fn expected(op: &Op, ienv: &[i64], fenv: &[f64]) -> Num {
    match op {
        Op::Sym(slot) => {
            let i = *slot as usize;
            if i % 2 == 0 {
                Num::I(ienv[i / 2])
            } else {
                Num::F(fenv[i / 2])
            }
        }
        Op::LitInt(v) => Num::I(*v),
        Op::LitFloat(v) => Num::F(*v),
        Op::Call(0, args) => {
            let (Num::I(a), Num::I(b)) = (expected(&args[0], ienv, fenv), expected(&args[1], ienv, fenv)) else {
                panic!("well-typed by construction")
            };
            Num::I(a.wrapping_add(b))
        }
        Op::Call(1, args) => {
            let (Num::F(a), Num::F(b)) = (expected(&args[0], ienv, fenv), expected(&args[1], ienv, fenv)) else {
                panic!("well-typed by construction")
            };
            Num::F(a * b)
        }
        Op::Call(2, args) => {
            let (Num::I(a), Num::I(b), Num::I(c)) = (
                expected(&args[0], ienv, fenv),
                expected(&args[1], ienv, fenv),
                expected(&args[2], ienv, fenv),
            ) else {
                panic!("well-typed by construction")
            };
            Num::I(a.wrapping_add(b).wrapping_add(c))
        }
        Op::Call(3, args) => {
            let (Num::F(a), Num::F(b)) = (expected(&args[0], ienv, fenv), expected(&args[1], ienv, fenv)) else {
                panic!("well-typed by construction")
            };
            Num::I((a * b) as i64)
        }
        Op::Call(_, _) => unreachable!(),
    }
}

// ---------------------------------------------------------------------------
// `Val72` chain.
// ---------------------------------------------------------------------------

fn env72(ienv: &[i64], fenv: &[f64]) -> [Val72; ENV_SLOTS] {
    std::array::from_fn(|i| {
        if i % 2 == 0 {
            Val72::Int(ienv[i / 2])
        } else {
            Val72::F(fenv[i / 2])
        }
    })
}

#[inline(never)]
fn eval72(op: &Op, env: &[Val72; ENV_SLOTS]) -> Result<Val72, ()> {
    match op {
        // Symbol lookup FIRST -- matches `eval_form_in`'s ordering.
        Op::Sym(slot) => Ok(env[*slot as usize].clone()),
        Op::LitInt(v) => Ok(Val72::Int(*v)),
        Op::LitFloat(v) => Ok(Val72::F(*v)),
        Op::Call(opcode, args) => {
            let a0 = eval72(&args[0], env)?;
            let a1 = eval72(&args[1], env)?;
            match opcode {
                0 => match (a0, a1) {
                    (Val72::Int(a), Val72::Int(b)) => Ok(Val72::Int(a.wrapping_add(b))),
                    _ => Err(()),
                },
                1 => match (a0, a1) {
                    (Val72::F(a), Val72::F(b)) => Ok(Val72::F(a * b)),
                    _ => Err(()),
                },
                2 => {
                    let a2 = eval72(&args[2], env)?;
                    match (a0, a1, a2) {
                        (Val72::Int(a), Val72::Int(b), Val72::Int(c)) => Ok(Val72::Int(a.wrapping_add(b).wrapping_add(c))),
                        _ => Err(()),
                    }
                }
                3 => match (a0, a1) {
                    (Val72::F(a), Val72::F(b)) => Ok(Val72::Int((a * b) as i64)),
                    _ => Err(()),
                },
                _ => Err(()),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// `Val16` chain -- byte-for-byte the same dispatch shape as `eval72`.
// ---------------------------------------------------------------------------

fn env16(ienv: &[i64], fenv: &[f64]) -> [Val16; ENV_SLOTS] {
    std::array::from_fn(|i| {
        if i % 2 == 0 {
            Val16::Int(ienv[i / 2])
        } else {
            Val16::F(fenv[i / 2])
        }
    })
}

#[inline(never)]
fn eval16(op: &Op, env: &[Val16; ENV_SLOTS]) -> Result<Val16, ()> {
    match op {
        Op::Sym(slot) => Ok(env[*slot as usize].clone()),
        Op::LitInt(v) => Ok(Val16::Int(*v)),
        Op::LitFloat(v) => Ok(Val16::F(*v)),
        Op::Call(opcode, args) => {
            let a0 = eval16(&args[0], env)?;
            let a1 = eval16(&args[1], env)?;
            match opcode {
                0 => match (a0, a1) {
                    (Val16::Int(a), Val16::Int(b)) => Ok(Val16::Int(a.wrapping_add(b))),
                    _ => Err(()),
                },
                1 => match (a0, a1) {
                    (Val16::F(a), Val16::F(b)) => Ok(Val16::F(a * b)),
                    _ => Err(()),
                },
                2 => {
                    let a2 = eval16(&args[2], env)?;
                    match (a0, a1, a2) {
                        (Val16::Int(a), Val16::Int(b), Val16::Int(c)) => Ok(Val16::Int(a.wrapping_add(b).wrapping_add(c))),
                        _ => Err(()),
                    }
                }
                3 => match (a0, a1) {
                    (Val16::F(a), Val16::F(b)) => Ok(Val16::Int((a * b) as i64)),
                    _ => Err(()),
                },
                _ => Err(()),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Correctness: both mocks must agree with the independent reference
// evaluator on a handful of seeds/env fixtures before the perf numbers mean
// anything.
// ---------------------------------------------------------------------------

#[test]
fn both_mocks_agree_with_reference_evaluator() {
    for seed in [1u64, 2, 3, 42, 0xDEAD_BEEF] {
        let mut lcg = seed;
        let ienv: Vec<i64> = (0..ENV_SLOTS / 2).map(|_| (lcg_next(&mut lcg) % 1000) as i64).collect();
        let fenv: Vec<f64> = (0..ENV_SLOTS / 2).map(|_| (lcg_next(&mut lcg) % 1000) as f64 / 3.0).collect();
        let prog = build_program();

        let want = expected(&prog, &ienv, &fenv);
        let e72 = env72(&ienv, &fenv);
        let e16 = env16(&ienv, &fenv);
        let got72 = eval72(&prog, &e72).expect("well-typed program");
        let got16 = eval16(&prog, &e16).expect("well-typed program");

        match (want, got72, got16) {
            (Num::I(w), Val72::Int(a), Val16::Int(b)) => {
                assert_eq!(w, a, "seed={seed}: Val72 disagrees with reference");
                assert_eq!(w, b, "seed={seed}: Val16 disagrees with reference");
            }
            (Num::F(w), Val72::F(a), Val16::F(b)) => {
                assert_eq!(w.to_bits(), a.to_bits(), "seed={seed}: Val72 disagrees with reference");
                assert_eq!(w.to_bits(), b.to_bits(), "seed={seed}: Val16 disagrees with reference");
            }
            _ => panic!("seed={seed}: variant mismatch between reference and mock"),
        }
    }
}

// ---------------------------------------------------------------------------
// The kill-probe proper.
// ---------------------------------------------------------------------------

#[test]
#[ignore = "measurement, not a gate -- run with --ignored --nocapture"]
fn register_abi_kill_probe() {
    let prog = build_program();
    let nodes_per_call = node_count(&prog);
    const CALLS: u64 = 400_000;
    let total = nodes_per_call * CALLS;

    let mut lcg = 0xC0FF_EE00u64;
    let ienv: Vec<i64> = (0..ENV_SLOTS / 2).map(|_| (lcg_next(&mut lcg) % 1000) as i64).collect();
    let fenv: Vec<f64> = (0..ENV_SLOTS / 2).map(|_| (lcg_next(&mut lcg) % 1000) as f64 / 3.0).collect();
    let e72 = env72(&ienv, &fenv);
    let e16 = env16(&ienv, &fenv);

    let run72 = || -> i64 {
        let mut acc = 0i64;
        for _ in 0..CALLS {
            if let Ok(Val72::Int(v)) = eval72(black_box(&prog), black_box(&e72)) {
                acc = acc.wrapping_add(v);
            }
        }
        acc
    };
    let run16 = || -> i64 {
        let mut acc = 0i64;
        for _ in 0..CALLS {
            if let Ok(Val16::Int(v)) = eval16(black_box(&prog), black_box(&e16)) {
                acc = acc.wrapping_add(v);
            }
        }
        acc
    };

    // warmup
    black_box(run72());
    black_box(run16());

    // Interleaved rounds: a load spike from a neighbouring build hits both
    // variants alike, per the machine caveat.
    const ROUNDS: usize = 7;
    let mut r72 = Vec::with_capacity(ROUNDS);
    let mut r16 = Vec::with_capacity(ROUNDS);
    for _ in 0..ROUNDS {
        let t0 = Instant::now();
        let s = run72();
        let dt = t0.elapsed().as_secs_f64();
        black_box(s);
        r72.push(total as f64 / dt);

        let t0 = Instant::now();
        let s = run16();
        let dt = t0.elapsed().as_secs_f64();
        black_box(s);
        r16.push(total as f64 / dt);
    }
    let stats = |mut v: Vec<f64>| {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        (v[v.len() / 2], v[v.len() - 1])
    };
    let (med72, max72) = stats(r72);
    let (med16, max16) = stats(r16);

    let speedup_med = med16 / med72;
    let speedup_max = max16 / max72;

    println!("\n=== W-GEO kill-probe A: register ABI (72B vs {}B) ===", std::mem::size_of::<Val16>());
    println!("  {nodes_per_call} nodes/program, {CALLS} calls/round, {ROUNDS} interleaved rounds");
    println!(
        "  Val72 (today's width) : median {:>7.1}M node-evals/s  [max {:>7.1}M]",
        med72 / 1e6,
        max72 / 1e6
    );
    println!(
        "  Val16 (target width)  : median {:>7.1}M node-evals/s  [max {:>7.1}M]",
        med16 / 1e6,
        max16 / 1e6
    );
    println!(
        "  speedup: {:+.1}% (median)   {:+.1}% (best-of-{ROUNDS})",
        (speedup_med - 1.0) * 100.0,
        (speedup_max - 1.0) * 100.0
    );
    println!("BAR: >= +10.0% (either statistic clearing it is enough -- best-of survives machine load).");
    let cleared = speedup_med >= 1.10 || speedup_max >= 1.10;
    if cleared {
        println!("*** W-GEO PROBE A: BAR CLEARED -- register-ABI win funds the geometry campaign. ***");
    } else {
        println!("*** W-GEO PROBE A: BAR MISSED -- register ABI alone does not fund the campaign. ***");
    }

    // Reported, never asserted on the BAR: a probe that misses its bar is a
    // RESULT. Only assert that a measurement actually happened.
    assert!(med72 > 0.0 && med16 > 0.0, "probe produced no measurement");
}
