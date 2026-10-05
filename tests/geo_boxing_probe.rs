//! W-GEO kill-probe B (collection boxing tax): if `Value::Vector`/
//! `Value::List` moved from holding `PVec` INLINE (64 bytes -- the width
//! driver behind `Value`'s 72-byte size, see `tests/geo_abi_probe.rs`'s
//! module doc) to holding `Arc<PVec>` (8 bytes), what does that COST on the
//! collection ops that actually run, using the REAL `PVec` from the crate
//! (not a mock)?
//!
//! Boxing buys `Value` a much narrower footprint (feeding straight back
//! into Probe A's register-ABI win, and shrinking every `Value` that is
//! NOT itself a vector/map). It costs one extra heap allocation + one
//! extra pointer indirection per PERSISTENT UPDATE (`conj`/`assoc`-shaped
//! ops): `PVec::push_back` on the `Small` arm already allocates a fresh
//! `Arc<[Value]>` every call (see `src/value.rs:994-1024` -- it does not
//! attempt `Arc::get_mut` reuse the way `set` does), so boxing adds a
//! SECOND allocation (`Arc::new` over the whole updated `PVec`) on top of
//! the one the persistent update already pays. Scans and random access pay
//! only the extra pointer chase to get from `&Value` to `&PVec`, which is
//! not expected to show up as noise -- reported anyway, because "expected
//! to be free" is exactly the kind of claim this campaign should measure
//! instead of assume.
//!
//! ## What is measured (real `PVec`, from `mova::internal`)
//!
//! * `conj_churn` -- repeated persistent `push_back` on a small vector,
//!   reset to empty every time it would promote past `PVEC_SMALL_MAX`
//!   (16) so the measurement stays in the `Small` COW steady state, not
//!   the one-time promotion transition. This is the REGRESSION risk: one
//!   `conj`/`assoc`-shaped call is the unit, matching how Probe C counts
//!   operations (not elements).
//! * `chunked_scan` -- one full iteration-and-sum over a 100k-element
//!   `Big` vector, reported both as ns/FULL-SCAN (the same "one op" unit
//!   `conj_churn` uses, for the netting arithmetic) and ns/element
//!   (supplementary).
//! * `random_access` -- single `.get(i)` at a random index into the same
//!   100k-element vector.
//!
//! `BoxedPVec` here is `Arc<PVec>`, standing in for what `Value::Vector`
//! would hold post-geometry-change; `PVec` alone stands in for today's
//! inline shape.
//!
//! BAR (owner): scan/access wins must net out the churn tax on Probe C's
//! MEASURED op mix (not an assumed one) -- see `tests/geo_census_probe.rs`,
//! which re-measures these same three ops and combines them with its
//! census-derived churn/scan/access weights to print the final verdict.
//! This file's own verdict is the raw ns/op table plus a naive 50/50
//! netting as a sanity check; the decisive netting lives in the census
//! probe.
//!
//! Run: `cargo test --release --test geo_boxing_probe -- --ignored --nocapture`
//!
//! ## STALE PREMISE after W-GEO stage 4 (split-box), read before trusting
//! ## a NEW run of this file
//!
//! Everything above was written when `PVec` was 64 bytes inline. Stage 4
//! (`docs/W-GEO-STAGE4-SPLITBOX.md`) boxed `PVec::Big`'s payload, so the
//! REAL `PVec` this probe imports from `mova::internal` is now 16 bytes
//! and `size_of::<Value>()` is 32, not 72. Two consequences:
//!
//! * The "inline" side of every A/B below now measures the SPLIT-BOX
//!   `PVec`, not the 64-byte one the numbers in
//!   `docs/W-GEO-PROBE-VERDICTS.md` (Probe B) were taken against. Those
//!   recorded numbers stand as a measurement of the pre-stage-4 tree; a
//!   fresh run of this file measures a different question.
//! * The question the "boxed" side asks -- what does `Arc<PVec>` cost --
//!   is the one Probe E answered decisively (KILL, +34-40% on MapEntry
//!   churn) and is NOT what stage 4 shipped. It is kept as-is, unmodified,
//!   because the correctness test below (`boxed_and_inline_pvec_agree`) is
//!   still a real gate on `PVec` itself and the historical measurement
//!   should not be silently rewritten.

use std::hint::black_box;
use std::sync::Arc;
use std::time::Instant;

use mova::internal::{PVec, Value};

const M: u64 = 6_364_136_223_846_793_005;
const C: u64 = 1_442_695_040_888_963_407;

#[inline]
fn lcg_next(x: &mut u64) -> u64 {
    *x = x.wrapping_mul(M).wrapping_add(C);
    *x
}

const SMALL_MAX: usize = 16; // PVEC_SMALL_MAX, duplicated (private const in src/value.rs).

/// One persistent `conj`: `Small`-arm `push_back`, reset to empty right
/// before it would promote to `Big` -- see module doc.
#[inline(never)]
fn conj_inline(v: &PVec, val: Value) -> PVec {
    let base = if v.len() >= SMALL_MAX { PVec::new() } else { v.clone() };
    let mut out = base;
    out.push_back(val);
    out
}

#[inline(never)]
fn conj_boxed(v: &Arc<PVec>, val: Value) -> Arc<PVec> {
    let base = if v.len() >= SMALL_MAX { PVec::new() } else { (**v).clone() };
    let mut out = base;
    out.push_back(val);
    Arc::new(out)
}

#[inline(never)]
fn scan_sum_inline(v: &PVec) -> i64 {
    let mut acc = 0i64;
    for x in v.iter() {
        if let Value::Int(i) = x {
            acc = acc.wrapping_add(*i);
        }
    }
    acc
}

#[inline(never)]
fn scan_sum_boxed(v: &Arc<PVec>) -> i64 {
    let mut acc = 0i64;
    for x in v.iter() {
        if let Value::Int(i) = x {
            acc = acc.wrapping_add(*i);
        }
    }
    acc
}

#[inline(never)]
fn access_inline(v: &PVec, idx: usize) -> i64 {
    match v.get(idx) {
        Some(Value::Int(i)) => *i,
        _ => 0,
    }
}

#[inline(never)]
fn access_boxed(v: &Arc<PVec>, idx: usize) -> i64 {
    match v.get(idx) {
        Some(Value::Int(i)) => *i,
        _ => 0,
    }
}

const BIG_LEN: usize = 100_000;

fn build_big() -> PVec {
    let mut v = PVec::new();
    for i in 0..BIG_LEN {
        v.push_back(Value::Int(i as i64));
    }
    v
}

/// `(median, best)` of a sample. `best` is the MINIMUM here (not the
/// maximum, as `globals_snapshot_bench`'s `stats` uses): these ops are
/// measured in ns/op (a COST, lower is better), while that file measures a
/// throughput RATE (higher is better) -- "least disturbed by a
/// neighbouring load spike" is the min in one unit and the max in the
/// other. Same idea (best-of survives a machine that isn't quiet), same
/// justification, opposite direction.
fn stats(mut v: Vec<f64>) -> (f64, f64) {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    (v[v.len() / 2], v[0])
}

#[test]
fn boxed_and_inline_pvec_agree() {
    let mut inline = PVec::new();
    let mut boxed = Arc::new(PVec::new());
    for i in 0..40i64 {
        inline = conj_inline(&inline, Value::Int(i));
        boxed = conj_boxed(&boxed, Value::Int(i));
        assert_eq!(inline.len(), boxed.len());
        for j in 0..inline.len() {
            let (Value::Int(a), Value::Int(b)) = (inline.get(j).unwrap(), boxed.get(j).unwrap()) else {
                panic!("non-Int element")
            };
            assert_eq!(a, b);
        }
    }
    let big = build_big();
    let big_boxed = Arc::new(big.clone());
    assert_eq!(scan_sum_inline(&big), scan_sum_boxed(&big_boxed));
    assert_eq!(access_inline(&big, 12345), access_boxed(&big_boxed, 12345));
}

/// Shared measurement entry points, reused verbatim by
/// `tests/geo_census_probe.rs` for the decisive netting (duplicated there
/// rather than shared via a `tests/common` module, so each probe file
/// stays a self-contained, independently-committable deliverable per the
/// mission brief).
///
/// Returns `(inline_ns_per_op, boxed_ns_per_op)` for one op kind,
/// best-of-9 interleaved rounds (min, not median -- see `stats`'s doc).
pub fn bench_conj_ns() -> (f64, f64) {
    const OPS: u64 = 200_000;
    let run_inline = || {
        let mut v = PVec::new();
        for i in 0..OPS {
            v = conj_inline(black_box(&v), Value::Int(i as i64));
        }
        v.len()
    };
    let run_boxed = || {
        let mut v = Arc::new(PVec::new());
        for i in 0..OPS {
            v = conj_boxed(black_box(&v), Value::Int(i as i64));
        }
        v.len()
    };
    black_box(run_inline());
    black_box(run_boxed());
    let mut ri = Vec::new();
    let mut rb = Vec::new();
    for _ in 0..9 {
        let t0 = Instant::now();
        black_box(run_inline());
        ri.push(t0.elapsed().as_secs_f64() / OPS as f64 * 1e9);
        let t0 = Instant::now();
        black_box(run_boxed());
        rb.push(t0.elapsed().as_secs_f64() / OPS as f64 * 1e9);
    }
    (stats(ri).1, stats(rb).1)
}

/// `(inline_ns_per_scan, boxed_ns_per_scan)` -- one FULL scan of a 100k
/// `Big` vector is the unit, matching `conj`'s "one op" unit.
pub fn bench_scan_ns() -> (f64, f64) {
    let big = build_big();
    let big_boxed = Arc::new(big.clone());
    const ROUNDS: usize = 30;
    black_box(scan_sum_inline(black_box(&big)));
    black_box(scan_sum_boxed(black_box(&big_boxed)));
    let mut ri = Vec::new();
    let mut rb = Vec::new();
    for _ in 0..ROUNDS {
        let t0 = Instant::now();
        black_box(scan_sum_inline(black_box(&big)));
        ri.push(t0.elapsed().as_secs_f64() * 1e9);
        let t0 = Instant::now();
        black_box(scan_sum_boxed(black_box(&big_boxed)));
        rb.push(t0.elapsed().as_secs_f64() * 1e9);
    }
    (stats(ri).1, stats(rb).1)
}

/// `(inline_ns_per_access, boxed_ns_per_access)`.
pub fn bench_access_ns() -> (f64, f64) {
    let big = build_big();
    let big_boxed = Arc::new(big.clone());
    const OPS: u64 = 500_000;
    let mut lcg = 0xACCE_5500u64;
    let idxs: Vec<usize> = (0..OPS).map(|_| (lcg_next(&mut lcg) as usize) % BIG_LEN).collect();
    let run_inline = || {
        let mut acc = 0i64;
        for &i in &idxs {
            acc = acc.wrapping_add(access_inline(black_box(&big), i));
        }
        acc
    };
    let run_boxed = || {
        let mut acc = 0i64;
        for &i in &idxs {
            acc = acc.wrapping_add(access_boxed(black_box(&big_boxed), i));
        }
        acc
    };
    black_box(run_inline());
    black_box(run_boxed());
    let mut ri = Vec::new();
    let mut rb = Vec::new();
    for _ in 0..9 {
        let t0 = Instant::now();
        black_box(run_inline());
        ri.push(t0.elapsed().as_secs_f64() / OPS as f64 * 1e9);
        let t0 = Instant::now();
        black_box(run_boxed());
        rb.push(t0.elapsed().as_secs_f64() / OPS as f64 * 1e9);
    }
    (stats(ri).1, stats(rb).1)
}

#[test]
#[ignore = "measurement, not a gate -- run with --ignored --nocapture"]
fn boxing_tax_kill_probe() {
    let (ci, cb) = bench_conj_ns();
    let (si, sb) = bench_scan_ns();
    let (ai, ab) = bench_access_ns();

    println!("\n=== W-GEO kill-probe B: collection boxing tax (real PVec) ===");
    println!("  {:<26} {:>14} {:>14} {:>10}", "op (unit)", "inline ns/op", "boxed ns/op", "delta");
    println!(
        "  {:<26} {:>14.1} {:>14.1} {:>+9.1}%",
        "conj (1 push_back)",
        ci,
        cb,
        (cb / ci - 1.0) * 100.0
    );
    println!(
        "  {:<26} {:>14.1} {:>14.1} {:>+9.1}%",
        "scan (1 full 100k-elem)",
        si,
        sb,
        (sb / si - 1.0) * 100.0
    );
    println!(
        "  {:<26} {:>14.1} {:>14.1} {:>+9.1}%",
        "access (1 random .get)",
        ai,
        ab,
        (ab / ai - 1.0) * 100.0
    );
    println!(
        "  scan ns/element: inline {:.3}  boxed {:.3}",
        si / BIG_LEN as f64,
        sb / BIG_LEN as f64
    );

    // Sanity-check netting only, at a naive 50/50 churn/scan-access split.
    // The DECISIVE netting (Probe C's measured mix) lives in
    // `tests/geo_census_probe.rs`.
    let naive_net = 0.5 * (cb - ci) + 0.25 * (sb - si) + 0.25 * (ab - ai);
    println!(
        "\n  naive 50/50 sanity net (churn 50%, scan 25%, access 25%): {:+.2} ns/op-equivalent",
        naive_net
    );
    println!("  BAR (owner): decisive netting against Probe C's MEASURED mix -- see tests/geo_census_probe.rs.");
    if naive_net <= 0.0 {
        println!("  (sanity net <= 0: boxing looks net-neutral-or-better even before weighting by the real mix)");
    } else {
        println!("  (sanity net > 0: churn tax dominates at an even split -- the real mix's churn share decides it)");
    }

    assert!(ci > 0.0 && cb > 0.0 && si > 0.0 && sb > 0.0, "probe produced no measurement");
}
