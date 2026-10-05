//! W-GEO stage-4 kill-probe (Probe E): `Value::MapEntry(PVec)` boxing tax
//! -- MapEntry-specific, priced FIRST because map iteration (the hottest
//! MapEntry-materializing path, `Value::Map`'s `seq` arm in `eval/mod.rs`
//! and the sibling arms it mirrors for `HostStruct`/`SortedMap`/
//! `StructMap`) is common in real workloads and every entry it yields is
//! a fresh `PVec::pair` allocation today (see `src/value.rs`'s
//! `Value::MapEntry` doc: "the `PVec` always holds EXACTLY two elements
//! ... only ever built by `PVec::pair`").
//!
//! Models `tests/geo_boxing_probe.rs`'s structure exactly (self-contained,
//! real `mova::internal::{PMap, PVec, Value}`, best-of-9 interleaved
//! rounds, `stats()` returning `(median, min)`), but the "boxed" side here
//! is a HYPOTHETICAL shape stage 4 would add -- `Value::MapEntry` does not
//! actually hold `Arc<PVec>` today, so the boxed path below is simulated
//! as "what an `Arc::new(PVec::pair(..))` per materialize would cost",
//! standing in for a future `MapEntry(Arc<PVec>)` variant the same way
//! Probe B's `BoxedPVec` stood in for a future `Vector(Arc<PVec>)`.
//!
//! ## Three shapes measured
//!
//! * (a) **materialize** -- construct the 2-element pair the way
//!   `MapEntry` is built (`PVec::pair(k, v)`), inline vs wrapped in a
//!   fresh `Arc::new(..)`.
//! * (b) **read** -- given pre-materialized entries (built once, outside
//!   the timed loop), access both elements (`.get(0)`/`.get(1)`, the
//!   key/val reads a real `key`/`val` builtin performs) inline vs through
//!   the `Arc` deref.
//! * (c) **map-iteration end-to-end** -- iterate a real `PMap::Big` (10k
//!   entries), materializing one pair per step and reading key+val, i.e.
//!   the exact loop shape `Value::Map`'s `seq` arm runs today (see
//!   `src/eval/mod.rs:1658-1667`) -- inline vs boxed, ns/entry. This is
//!   the mapseq-iter proxy Probe C's census logged as pv_churn 220,140 /
//!   pv_access 1,801,026 / pv_scan 2,617,879 / pm_churn 10,965 for.
//!
//! KILL BAR (owner): if shape (c) shows boxed worse than inline by >10%
//! (median), stage 4's five-arm box must not proceed as specced against
//! `MapEntry`/`Queue`. Otherwise PROCEED, with the measured tax quoted
//! honestly.
//!
//! Run: `cargo test --release --test geo_mapentry_probe -- --ignored --nocapture`

use std::hint::black_box;
use std::sync::Arc;
use std::time::Instant;

use mova::internal::{PMap, PVec, Value};

/// `(median, best)` of a sample -- `best` is the MINIMUM (a cost metric,
/// lower is better), identical contract to `geo_boxing_probe.rs`'s
/// `stats`.
fn stats(mut v: Vec<f64>) -> (f64, f64) {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    (v[v.len() / 2], v[0])
}

// ---------------------------------------------------------------------
// Shape (a): materialize -- the constructor MapEntry actually calls.
// ---------------------------------------------------------------------

#[inline(never)]
fn materialize_inline(k: Value, v: Value) -> PVec {
    PVec::pair(k, v)
}

#[inline(never)]
fn materialize_boxed(k: Value, v: Value) -> Arc<PVec> {
    Arc::new(PVec::pair(k, v))
}

const MAT_OPS: u64 = 200_000;

/// Returns `((inline_median, inline_min), (boxed_median, boxed_min))`,
/// ns/op, best-of-9 interleaved rounds.
fn bench_materialize_ns() -> ((f64, f64), (f64, f64)) {
    let run_inline = || {
        let mut acc = 0usize;
        for i in 0..MAT_OPS {
            let p = materialize_inline(black_box(Value::Int(i as i64)), black_box(Value::Int(i as i64 + 1)));
            acc = acc.wrapping_add(p.len());
        }
        acc
    };
    let run_boxed = || {
        let mut acc = 0usize;
        for i in 0..MAT_OPS {
            let p = materialize_boxed(black_box(Value::Int(i as i64)), black_box(Value::Int(i as i64 + 1)));
            acc = acc.wrapping_add(p.len());
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
        ri.push(t0.elapsed().as_secs_f64() / MAT_OPS as f64 * 1e9);
        let t0 = Instant::now();
        black_box(run_boxed());
        rb.push(t0.elapsed().as_secs_f64() / MAT_OPS as f64 * 1e9);
    }
    (stats(ri), stats(rb))
}

// ---------------------------------------------------------------------
// Shape (b): read -- key/val access on pre-materialized entries.
// ---------------------------------------------------------------------

#[inline(never)]
fn read_inline(v: &PVec) -> i64 {
    let k = match v.get(0) {
        Some(Value::Int(i)) => *i,
        _ => 0,
    };
    let val = match v.get(1) {
        Some(Value::Int(i)) => *i,
        _ => 0,
    };
    k.wrapping_add(val)
}

#[inline(never)]
fn read_boxed(v: &Arc<PVec>) -> i64 {
    let k = match v.get(0) {
        Some(Value::Int(i)) => *i,
        _ => 0,
    };
    let val = match v.get(1) {
        Some(Value::Int(i)) => *i,
        _ => 0,
    };
    k.wrapping_add(val)
}

const READ_OPS: usize = 200_000;

fn build_entries_inline() -> Vec<PVec> {
    (0..READ_OPS).map(|i| PVec::pair(Value::Int(i as i64), Value::Int(i as i64 + 1))).collect()
}

fn build_entries_boxed() -> Vec<Arc<PVec>> {
    (0..READ_OPS).map(|i| Arc::new(PVec::pair(Value::Int(i as i64), Value::Int(i as i64 + 1)))).collect()
}

fn bench_read_ns() -> ((f64, f64), (f64, f64)) {
    let entries_inline = build_entries_inline();
    let entries_boxed = build_entries_boxed();
    let run_inline = || {
        let mut acc = 0i64;
        for e in &entries_inline {
            acc = acc.wrapping_add(read_inline(black_box(e)));
        }
        acc
    };
    let run_boxed = || {
        let mut acc = 0i64;
        for e in &entries_boxed {
            acc = acc.wrapping_add(read_boxed(black_box(e)));
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
        ri.push(t0.elapsed().as_secs_f64() / READ_OPS as f64 * 1e9);
        let t0 = Instant::now();
        black_box(run_boxed());
        rb.push(t0.elapsed().as_secs_f64() / READ_OPS as f64 * 1e9);
    }
    (stats(ri), stats(rb))
}

// ---------------------------------------------------------------------
// Shape (c): map-iteration end-to-end -- the mapseq-iter proxy.
//
// Mirrors `Value::Map`'s `seq` arm (`src/eval/mod.rs:1658-1667`) exactly:
// one `PVec::pair`/`MapEntry` materialization per (k, v), then a key+val
// read off it, once per map entry.
// ---------------------------------------------------------------------

const MAP_BIG_LEN: usize = 10_000;

fn build_big_map() -> PMap {
    let mut m = PMap::new();
    for i in 0..MAP_BIG_LEN {
        m = m.update(Value::Int(i as i64), Value::Int(i as i64 * 2));
    }
    m
}

#[inline(never)]
fn iterate_inline(m: &PMap) -> i64 {
    let mut acc = 0i64;
    for (k, v) in m.iter() {
        let entry = Value::MapEntry(PVec::pair(k.clone(), v.clone()));
        if let Value::MapEntry(items) = black_box(&entry) {
            let kk = match items.get(0) {
                Some(Value::Int(i)) => *i,
                _ => 0,
            };
            let vv = match items.get(1) {
                Some(Value::Int(i)) => *i,
                _ => 0,
            };
            acc = acc.wrapping_add(kk).wrapping_add(vv);
        }
    }
    acc
}

/// Hypothetical stage-4 shape: `Value::MapEntry(Arc<PVec>)` -- one extra
/// `Arc::new` allocation per step (the box), then key/val reads go
/// through the `Arc` deref. There is no real `MapEntryBoxed` variant to
/// match on, so the "entry" here is the `Arc<PVec>` itself, exactly
/// mirroring how shape (a)/(b)'s `materialize_boxed`/`read_boxed` model
/// the same hypothetical.
#[inline(never)]
fn iterate_boxed(m: &PMap) -> i64 {
    let mut acc = 0i64;
    for (k, v) in m.iter() {
        let entry: Arc<PVec> = Arc::new(PVec::pair(k.clone(), v.clone()));
        let entry = black_box(entry);
        let kk = match entry.get(0) {
            Some(Value::Int(i)) => *i,
            _ => 0,
        };
        let vv = match entry.get(1) {
            Some(Value::Int(i)) => *i,
            _ => 0,
        };
        acc = acc.wrapping_add(kk).wrapping_add(vv);
    }
    acc
}

fn bench_map_iter_ns_per_entry() -> ((f64, f64), (f64, f64)) {
    let m = build_big_map();
    black_box(iterate_inline(&m));
    black_box(iterate_boxed(&m));
    let mut ri = Vec::new();
    let mut rb = Vec::new();
    for _ in 0..9 {
        let t0 = Instant::now();
        black_box(iterate_inline(&m));
        ri.push(t0.elapsed().as_secs_f64() / MAP_BIG_LEN as f64 * 1e9);
        let t0 = Instant::now();
        black_box(iterate_boxed(&m));
        rb.push(t0.elapsed().as_secs_f64() / MAP_BIG_LEN as f64 * 1e9);
    }
    (stats(ri), stats(rb))
}

// ---------------------------------------------------------------------
// Correctness smoke test (not ignored -- runs under a plain
// `cargo test --release --test geo_mapentry_probe`).
// ---------------------------------------------------------------------

#[test]
fn inline_and_boxed_agree() {
    let p_inline = materialize_inline(Value::Int(1), Value::Int(2));
    let p_boxed = materialize_boxed(Value::Int(1), Value::Int(2));
    assert_eq!(read_inline(&p_inline), read_boxed(&p_boxed));

    let entries_inline = build_entries_inline();
    let entries_boxed = build_entries_boxed();
    assert_eq!(entries_inline.len(), entries_boxed.len());
    for (a, b) in entries_inline.iter().zip(entries_boxed.iter()) {
        assert_eq!(read_inline(a), read_boxed(b));
    }

    let m = build_big_map();
    assert_eq!(m.len(), MAP_BIG_LEN);
    assert_eq!(iterate_inline(&m), iterate_boxed(&m));
}

#[test]
#[ignore = "measurement, not a gate -- run with --ignored --nocapture"]
fn mapentry_boxing_tax_kill_probe() {
    let ((mat_i_med, mat_i_min), (mat_b_med, mat_b_min)) = bench_materialize_ns();
    let ((rd_i_med, rd_i_min), (rd_b_med, rd_b_min)) = bench_read_ns();
    let ((it_i_med, it_i_min), (it_b_med, it_b_min)) = bench_map_iter_ns_per_entry();

    println!("\n=== W-GEO stage-4 kill-probe E: MapEntry boxing tax (real PVec/PMap) ===");
    println!(
        "  {:<32} {:>12} {:>12} {:>12} {:>12} {:>10}",
        "shape (unit)", "inline med", "inline min", "boxed med", "boxed min", "delta(med)"
    );
    println!(
        "  {:<32} {:>12.1} {:>12.1} {:>12.1} {:>12.1} {:>+9.1}%",
        "(a) materialize (1 pair)",
        mat_i_med,
        mat_i_min,
        mat_b_med,
        mat_b_min,
        (mat_b_med / mat_i_med - 1.0) * 100.0
    );
    println!(
        "  {:<32} {:>12.1} {:>12.1} {:>12.1} {:>12.1} {:>+9.1}%",
        "(b) read (key+val)",
        rd_i_med,
        rd_i_min,
        rd_b_med,
        rd_b_min,
        (rd_b_med / rd_i_med - 1.0) * 100.0
    );
    println!(
        "  {:<32} {:>12.1} {:>12.1} {:>12.1} {:>12.1} {:>+9.1}%",
        "(c) map-iter (ns/entry)",
        it_i_med,
        it_i_min,
        it_b_med,
        it_b_min,
        (it_b_med / it_i_med - 1.0) * 100.0
    );

    let c_delta_pct = (it_b_med / it_i_med - 1.0) * 100.0;
    println!("\n  KILL BAR: shape (c) boxed vs inline, median delta = {:+.1}%", c_delta_pct);
    let verdict = if c_delta_pct > 10.0 { "KILL" } else { "PROCEED" };
    println!("  VERDICT: {}", verdict);
    if verdict == "KILL" {
        println!("  (shape (c) regresses >10% median -- stage 4's five-arm box must not proceed as specced against MapEntry/Queue)");
    } else {
        println!("  (shape (c) stays within the 10% bar -- PROCEED, tax quoted above, not assumed away)");
    }

    assert!(mat_i_med > 0.0 && mat_b_med > 0.0 && rd_i_med > 0.0 && rd_b_med > 0.0 && it_i_med > 0.0 && it_b_med > 0.0);
}
