//! W-GEO re-plan kill-probe (Probe G): split-box pricing -- box ONLY the
//! `Big` arm of `PVec`/`PMap`, leave `Small` untouched.
//!
//! Probe E (`tests/geo_mapentry_probe.rs`) KILLED the five-arm mechanical
//! box (wrapping `MapEntry`'s pair, which is always `Small`, in a second
//! `Arc` cost +34-40% on map iteration). The re-plan candidate this probe
//! prices, per `OWNER-BRIEF-SESSION13.md`'s follow-up: leave every
//! `Value` arm's payload INLINE, but shrink the payload TYPES themselves
//! by boxing only their `Big` arm:
//!
//! * `PVec`: `Small(Arc<[Value]>) | Big(imbl::Vector<Value>)` (today, 64B
//!   -- driven by `imbl::Vector`'s own 64B footprint, see this module's
//!   own doc in `src/value.rs`) -> `Small(Arc<[Value]>) |
//!   Big(Arc<imbl::Vector<Value>>)` (16B -- `Arc<[Value]>` is a 16-byte
//!   FAT pointer (data + len, `[Value]` is unsized), so it -- not the now
//!   8-byte-thin-pointer `Big` arm -- becomes the new max).
//! * `PMap`: `Small(Arc<Vec<(Value,Value)>>) | Big(PersistentHashMap)`
//!   (today, 24B) -> `Big` arm `Arc`-boxed (16B: `Arc<Vec<..>>` is an
//!   8-byte thin pointer to a heap-allocated `Vec` header, `Vec` itself
//!   being the fat/wide part; boxing `Big` alone leaves `Small`'s own
//!   8-byte pointer as the max, +8 for the discriminant/niche = 16B --
//!   see the size_of block in the measurement test below for the
//!   ACTUAL, not just predicted, number).
//!
//! Under this shape, `PVec::pair`/`Small` -- every `MapEntry`, since
//! `PVec::pair` always builds `Small` (`src/value.rs:1447-1450`) -- keeps
//! EXACTLY today's single-`Arc` behavior: no new allocation, no new
//! deref, this is the whole reason Probe E's five-arm-box KILL doesn't
//! automatically kill this narrower design too. Only `Big`-arm ops pay an
//! extra indirection, and those are the RARE ones on real workloads --
//! Probe E's own `data_structures.clj` census logged `pvec-promote 25` /
//! `pmap-promote 21` out of >500k total touches (see
//! `docs/W-GEO-PROBE-VERDICTS.md`'s Probe E section).
//!
//! ## What is measured (real `PVec`/`imbl::Vector`/`champ`, mocks
//! only for the HYPOTHETICAL split-box shape -- there is no
//! `PVec::Big(Arc<imbl::Vector<Value>>)` in `src/value.rs` today)
//!
//! * (a) **small-churn** -- the `Small`-arm COW-conj steady state
//!   (reset at `SMALL_MAX`, mirroring `geo_boxing_probe.rs`'s
//!   `conj_inline`), current `PVec` vs the `SplitPVec` mock. `Small` is
//!   UNCHANGED representation on both sides (`Arc<[Value]>`); this shape
//!   exists to confirm codegen doesn't regress just because the SIBLING
//!   `Big` arm's type changed underneath it (a shrunk `Big` arm shrinks
//!   the WHOLE enum, which can change how a `Small`-holding value is
//!   moved/copied even when Small's own bytes don't change -- exactly
//!   the effect shapes (a)/(e)/(f) are here to catch).
//! * (b) **big-churn** -- persistent `push_back` on a ~100k-element `Big`
//!   vector: current (`PVec::Big`'s `imbl::Vector::push_back` directly)
//!   vs split-box through `Arc::make_mut`, TWO cases -- uniquely-owned
//!   (fast path: `make_mut` mutates in place, zero extra clone) and
//!   SHARED (a live sibling `Arc` forces `make_mut` to clone the
//!   `imbl::Vector` HANDLE before mutating -- `imbl::Vector`'s own
//!   `Clone` is a shallow bump of its internal RRB-node `Arc`s, NOT a
//!   deep tree copy, confirmed by reading `vendor/imbl/src/vector/
//!   mod.rs`'s `RRB<A>`'s hand-written `Clone` impl).
//! * (c) **big-access** -- 500k LCG random `.get(i)` on the 100k `Big`
//!   vector, current vs through the `Arc` deref.
//! * (d) **big-scan** -- one full traversal of the 100k `Big` vector,
//!   current vs through the `Arc` deref.
//! * (e) **map-iter proxy** -- Probe E's shape (c) re-run, but this time
//!   BOTH sides build a `Small` pair (`PVec::pair`-shaped, unchanged
//!   allocation pattern) -- the only difference is which ENUM holds it:
//!   current `PVec` (64B) vs the `SplitPVec` mock (predicted 16B). This
//!   isolates the pure move/copy-size effect on the exact workload Probe
//!   E used to KILL the five-arm box. **KILL BAR (owner): >10% median
//!   regression here kills this route too** (there would be no reason to
//!   pursue a narrower box that still regresses the same workload).
//! * (f) **value-move** -- clone+drop of an already-materialized `Small`
//!   pair, current `PVec` (64B) vs the `SplitPVec` mock (16B) -- isolates
//!   move/copy/drop overhead of the enum's OWN width, with the
//!   allocation cost already paid before the timed region (shape (e)'s
//!   materialize-inclusive counterpart split out).
//!
//! Run: `cargo test --release --test geo_splitbox_probe -- --ignored
//! --nocapture`

use std::hint::black_box;
use std::sync::Arc;
use std::time::Instant;

use mova::internal::{champ, imbl, PMap, PVec, Symbol, Value};

const M: u64 = 6_364_136_223_846_793_005;
const C: u64 = 1_442_695_040_888_963_407;

#[inline]
fn lcg_next(x: &mut u64) -> u64 {
    *x = x.wrapping_mul(M).wrapping_add(C);
    *x
}

const SMALL_MAX: usize = 16; // PVEC_SMALL_MAX, duplicated (private const in src/value.rs).
const BIG_LEN: usize = 100_000;

/// `(median, best)` -- `best` is the MINIMUM (cost metric), same contract
/// as `geo_boxing_probe.rs`'s/`geo_mapentry_probe.rs`'s `stats()`.
fn stats(mut v: Vec<f64>) -> (f64, f64) {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    (v[v.len() / 2], v[0])
}

// ---------------------------------------------------------------------
// The split-box mocks. Real underlying types (`imbl::Vector`,
// `champ::PersistentHashMap`), Arc-boxed `Big` arm ONLY -- there is
// no such variant in `src/value.rs` today, this is the hypothetical
// stage-4-replan shape being priced.
// ---------------------------------------------------------------------

#[derive(Clone)]
#[allow(dead_code)] // `Big` is never constructed: shapes (b)/(c)/(d) price the
                     // Big arm's own Arc<imbl::Vector<Value>> directly (see
                     // those shapes' doc); this arm exists so size_of::<SplitPVec>()
                     // (measured below) reflects the WHOLE two-variant enum,
                     // matching how PVec's own 64B is driven by its Big arm today.
enum SplitPVec {
    Small(Arc<[Value]>),
    Big(Arc<imbl::Vector<Value>>),
}

impl SplitPVec {
    fn new() -> Self {
        SplitPVec::Small(Arc::from(Vec::<Value>::new()))
    }

    fn len(&self) -> usize {
        match self {
            SplitPVec::Small(a) => a.len(),
            SplitPVec::Big(v) => v.len(),
        }
    }

    fn get(&self, idx: usize) -> Option<&Value> {
        match self {
            SplitPVec::Small(a) => a.get(idx),
            SplitPVec::Big(v) => v.get(idx),
        }
    }

    /// Mirrors `PVec::push_back`'s `Small` arm verbatim
    /// (`src/value.rs:1058-1076`) -- untouched representation, so this
    /// should generate essentially identical code to today's. Never
    /// promotes in this probe (shape (a) resets before `SMALL_MAX`, same
    /// as `geo_boxing_probe.rs`'s `conj_inline`), so the promotion arm
    /// here is a straight-line fallback only, not on the measured path.
    fn push_small(&mut self, value: Value) {
        match self {
            SplitPVec::Small(a) => {
                let mut nv: Vec<Value> = Vec::with_capacity(a.len() + 1);
                nv.extend(a.iter().cloned());
                nv.push(value);
                *self = SplitPVec::Small(Arc::from(nv));
            }
            SplitPVec::Big(_) => unreachable!("shape (a) never promotes"),
        }
    }

    /// Mirrors `PVec::pair` (`src/value.rs:1447-1450`) exactly -- `Small`
    /// representation unchanged, one `Arc::new` alloc.
    fn pair(k: Value, v: Value) -> Self {
        let boxed: Arc<[Value; 2]> = Arc::new([k, v]);
        SplitPVec::Small(boxed)
    }
}

#[derive(Clone)]
#[allow(dead_code)] // constructed only for the size_of measurement below.
enum SplitPMap {
    Small(Arc<Vec<(Value, Value)>>),
    Big(Arc<champ::PersistentHashMap<Value, Value>>),
}

// ---------------------------------------------------------------------
// Shape (a): small-churn -- Small-arm COW conj steady state.
// ---------------------------------------------------------------------

#[inline(never)]
fn conj_current(v: &PVec, val: Value) -> PVec {
    let base = if v.len() >= SMALL_MAX { PVec::new() } else { v.clone() };
    let mut out = base;
    out.push_back(val);
    out
}

#[inline(never)]
fn conj_split(v: &SplitPVec, val: Value) -> SplitPVec {
    let base = if v.len() >= SMALL_MAX { SplitPVec::new() } else { v.clone() };
    let mut out = base;
    out.push_small(val);
    out
}

const SMALL_OPS: u64 = 200_000;

fn bench_small_churn_ns() -> ((f64, f64), (f64, f64)) {
    let run_current = || {
        let mut v = PVec::new();
        for i in 0..SMALL_OPS {
            v = conj_current(black_box(&v), Value::Int(i as i64));
        }
        v.len()
    };
    let run_split = || {
        let mut v = SplitPVec::new();
        for i in 0..SMALL_OPS {
            v = conj_split(black_box(&v), Value::Int(i as i64));
        }
        v.len()
    };
    black_box(run_current());
    black_box(run_split());
    let mut rc = Vec::new();
    let mut rs = Vec::new();
    for _ in 0..9 {
        let t0 = Instant::now();
        black_box(run_current());
        rc.push(t0.elapsed().as_secs_f64() / SMALL_OPS as f64 * 1e9);
        let t0 = Instant::now();
        black_box(run_split());
        rs.push(t0.elapsed().as_secs_f64() / SMALL_OPS as f64 * 1e9);
    }
    (stats(rc), stats(rs))
}

// ---------------------------------------------------------------------
// Shape (b): big-churn -- persistent push_back on the 100k Big vector.
// ---------------------------------------------------------------------

/// Built by wrapping `build_big_imbl()`'s tree DIRECTLY in `PVec::Big`,
/// deliberately NOT via `PVec::new()` + a `push_back` loop from `Small`:
/// that route promotes through `to_big()` (`Small`'s 16 elements
/// `.collect()`ed into a fresh `imbl::Vector`, `src/value.rs:1053-1076`),
/// which can leave a differently-shaped RRB tree than 100k consecutive
/// raw `push_back`s would (bulk `collect` vs incremental growth are not
/// guaranteed to produce identical chunk/depth layouts). `current` and
/// `split` MUST scan/access the exact same tree shape, or shapes (c)/(d)
/// would be measuring a construction-method artifact instead of the
/// `Arc`-wrapper's real cost -- confirmed as the actual cause of an
/// initial, wrong "-43 to -48%" scan reading before this fix (the same
/// class of measurement bug `docs/W-GEO-PROBE-VERDICTS.md`'s Probe C
/// section documents catching for a different reason: right instinct,
/// verify before trusting a countintuitive number).
fn build_big_pvec() -> PVec {
    PVec::Big(build_big_imbl())
}

fn build_big_imbl() -> imbl::Vector<Value> {
    let mut v = imbl::Vector::new();
    for i in 0..BIG_LEN {
        v.push_back(Value::Int(i as i64));
    }
    v
}

/// Extracts the `Big` arm's inner `imbl::Vector` reference DIRECTLY,
/// bypassing `PVec::get`/`PVec::iter`'s own `PVecIter`-wrapping dispatch
/// (`src/value.rs:1011-1013,1038-1044`). Load-bearing for shapes (c)/(d)
/// below: an earlier draft of this probe called `PVec::get`/`.iter()` on
/// the `current` side while `split` called `imbl::Vector::get`/`.iter()`
/// directly on the (identically-built) tree -- which measures
/// `PVecIter`'s own enum-dispatch overhead (a pre-existing PVec property,
/// unrelated to whether `Big`'s payload is boxed) instead of the `Arc`
/// indirection this probe exists to price, and produced a spurious
/// "-43 to -48%" scan reading. Both `current`/`split` now bypass any
/// wrapper equally, isolating exactly one variable: raw `imbl::Vector`
/// access vs the same access through one extra `Arc` deref.
fn big_vec_ref(v: &PVec) -> &imbl::Vector<Value> {
    match v {
        PVec::Big(v) => v,
        PVec::Small(_) => unreachable!("build_big_pvec always builds Big"),
    }
}

const BIG_CHURN_OPS: u64 = 20_000;

#[inline(never)]
fn big_push_current(v: &mut PVec, val: Value) {
    v.push_back(val);
}

#[inline(never)]
fn big_push_split_unique(v: &mut Arc<imbl::Vector<Value>>, val: Value) {
    Arc::make_mut(v).push_back(val);
}

/// Forces the shared path EVERY call: clones a sibling `Arc` right
/// before `make_mut`, so `make_mut` must clone the `imbl::Vector` handle
/// (shallow -- bumps the RRB root's internal `Arc`s, does not deep-copy
/// element data) before mutating. Worst-case shape: a caller that still
/// holds the previous snapshot live (e.g. an old var/atom binding) at the
/// moment of the next `conj`.
#[inline(never)]
fn big_push_split_shared(v: &mut Arc<imbl::Vector<Value>>, val: Value) {
    let sibling = Arc::clone(v);
    Arc::make_mut(v).push_back(val);
    black_box(&sibling);
    drop(sibling);
}

/// Returns `(current, split_unique, split_shared)`, each `(median, min)`
/// ns/op, best-of-9 interleaved.
fn bench_big_churn_ns() -> ((f64, f64), (f64, f64), (f64, f64)) {
    let run_current = || {
        let mut v = build_big_pvec();
        for i in 0..BIG_CHURN_OPS {
            big_push_current(black_box(&mut v), Value::Int(i as i64));
        }
        v.len()
    };
    let run_split_unique = || {
        let mut v = Arc::new(build_big_imbl());
        for i in 0..BIG_CHURN_OPS {
            big_push_split_unique(black_box(&mut v), Value::Int(i as i64));
        }
        v.len()
    };
    let run_split_shared = || {
        let mut v = Arc::new(build_big_imbl());
        for i in 0..BIG_CHURN_OPS {
            big_push_split_shared(black_box(&mut v), Value::Int(i as i64));
        }
        v.len()
    };
    black_box(run_current());
    black_box(run_split_unique());
    black_box(run_split_shared());
    let mut rc = Vec::new();
    let mut ru = Vec::new();
    let mut rs = Vec::new();
    for _ in 0..9 {
        let t0 = Instant::now();
        black_box(run_current());
        rc.push(t0.elapsed().as_secs_f64() / BIG_CHURN_OPS as f64 * 1e9);
        let t0 = Instant::now();
        black_box(run_split_unique());
        ru.push(t0.elapsed().as_secs_f64() / BIG_CHURN_OPS as f64 * 1e9);
        let t0 = Instant::now();
        black_box(run_split_shared());
        rs.push(t0.elapsed().as_secs_f64() / BIG_CHURN_OPS as f64 * 1e9);
    }
    (stats(rc), stats(ru), stats(rs))
}

// ---------------------------------------------------------------------
// Shape (c): big-access -- 500k LCG random .get(i).
// ---------------------------------------------------------------------

const ACCESS_OPS: u64 = 500_000;

#[inline(never)]
fn access_current(v: &PVec, idx: usize) -> i64 {
    match big_vec_ref(v).get(idx) {
        Some(Value::Int(i)) => *i,
        _ => 0,
    }
}

#[inline(never)]
fn access_split(v: &Arc<imbl::Vector<Value>>, idx: usize) -> i64 {
    match v.get(idx) {
        Some(Value::Int(i)) => *i,
        _ => 0,
    }
}

fn bench_big_access_ns() -> ((f64, f64), (f64, f64)) {
    let current = build_big_pvec();
    let split = Arc::new(build_big_imbl());
    let mut lcg = 0xACCE_5500u64;
    let idxs: Vec<usize> = (0..ACCESS_OPS).map(|_| (lcg_next(&mut lcg) as usize) % BIG_LEN).collect();
    let run_current = || {
        let mut acc = 0i64;
        for &i in &idxs {
            acc = acc.wrapping_add(access_current(black_box(&current), i));
        }
        acc
    };
    let run_split = || {
        let mut acc = 0i64;
        for &i in &idxs {
            acc = acc.wrapping_add(access_split(black_box(&split), i));
        }
        acc
    };
    black_box(run_current());
    black_box(run_split());
    let mut rc = Vec::new();
    let mut rs = Vec::new();
    for _ in 0..9 {
        let t0 = Instant::now();
        black_box(run_current());
        rc.push(t0.elapsed().as_secs_f64() / ACCESS_OPS as f64 * 1e9);
        let t0 = Instant::now();
        black_box(run_split());
        rs.push(t0.elapsed().as_secs_f64() / ACCESS_OPS as f64 * 1e9);
    }
    (stats(rc), stats(rs))
}

// ---------------------------------------------------------------------
// Shape (d): big-scan -- one full traversal of the 100k Big vector.
// ---------------------------------------------------------------------

#[inline(never)]
fn scan_current(v: &PVec) -> i64 {
    let mut acc = 0i64;
    for x in big_vec_ref(v).iter() {
        if let Value::Int(i) = x {
            acc = acc.wrapping_add(*i);
        }
    }
    acc
}

#[inline(never)]
fn scan_split(v: &Arc<imbl::Vector<Value>>) -> i64 {
    let mut acc = 0i64;
    for x in v.iter() {
        if let Value::Int(i) = x {
            acc = acc.wrapping_add(*i);
        }
    }
    acc
}

fn bench_big_scan_ns() -> ((f64, f64), (f64, f64)) {
    let current = build_big_pvec();
    let split = Arc::new(build_big_imbl());
    const ROUNDS: usize = 30;
    black_box(scan_current(black_box(&current)));
    black_box(scan_split(black_box(&split)));
    let mut rc = Vec::new();
    let mut rs = Vec::new();
    for _ in 0..ROUNDS {
        let t0 = Instant::now();
        black_box(scan_current(black_box(&current)));
        rc.push(t0.elapsed().as_secs_f64() * 1e9);
        let t0 = Instant::now();
        black_box(scan_split(black_box(&split)));
        rs.push(t0.elapsed().as_secs_f64() * 1e9);
    }
    (stats(rc), stats(rs))
}

// ---------------------------------------------------------------------
// Shape (e): map-iteration proxy -- Probe E's shape (c) re-run, current
// PVec (64B enum) vs SplitPVec mock (predicted 16B enum), both building a
// Small pair (unchanged allocation pattern either side).
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
fn iterate_current(m: &PMap) -> i64 {
    let mut acc = 0i64;
    for (k, v) in m.iter() {
        let entry = PVec::pair(k.clone(), v.clone());
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

#[inline(never)]
fn iterate_split(m: &PMap) -> i64 {
    let mut acc = 0i64;
    for (k, v) in m.iter() {
        let entry = SplitPVec::pair(k.clone(), v.clone());
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
    black_box(iterate_current(&m));
    black_box(iterate_split(&m));
    let mut rc = Vec::new();
    let mut rs = Vec::new();
    for _ in 0..9 {
        let t0 = Instant::now();
        black_box(iterate_current(&m));
        rc.push(t0.elapsed().as_secs_f64() / MAP_BIG_LEN as f64 * 1e9);
        let t0 = Instant::now();
        black_box(iterate_split(&m));
        rs.push(t0.elapsed().as_secs_f64() / MAP_BIG_LEN as f64 * 1e9);
    }
    (stats(rc), stats(rs))
}

// ---------------------------------------------------------------------
// Shape (f): value-move -- clone+drop of an already-materialized Small
// pair, isolating move/copy width (not allocation -- that already
// happened before the timed region).
// ---------------------------------------------------------------------

const MOVE_OPS: u64 = 200_000;

#[inline(never)]
fn clone_drop_current(v: &PVec) -> usize {
    let c = v.clone();
    c.len()
}

#[inline(never)]
fn clone_drop_split(v: &SplitPVec) -> usize {
    let c = v.clone();
    c.len()
}

fn bench_value_move_ns() -> ((f64, f64), (f64, f64)) {
    let current = PVec::pair(Value::Int(1), Value::Int(2));
    let split = SplitPVec::pair(Value::Int(1), Value::Int(2));
    let run_current = || {
        let mut acc = 0usize;
        for _ in 0..MOVE_OPS {
            acc = acc.wrapping_add(clone_drop_current(black_box(&current)));
        }
        acc
    };
    let run_split = || {
        let mut acc = 0usize;
        for _ in 0..MOVE_OPS {
            acc = acc.wrapping_add(clone_drop_split(black_box(&split)));
        }
        acc
    };
    black_box(run_current());
    black_box(run_split());
    let mut rc = Vec::new();
    let mut rs = Vec::new();
    for _ in 0..9 {
        let t0 = Instant::now();
        black_box(run_current());
        rc.push(t0.elapsed().as_secs_f64() / MOVE_OPS as f64 * 1e9);
        let t0 = Instant::now();
        black_box(run_split());
        rs.push(t0.elapsed().as_secs_f64() / MOVE_OPS as f64 * 1e9);
    }
    (stats(rc), stats(rs))
}

// ---------------------------------------------------------------------
// Correctness smoke test (not ignored).
// ---------------------------------------------------------------------

#[test]
fn current_and_split_agree() {
    // (a)
    let mut vc = PVec::new();
    let mut vs = SplitPVec::new();
    for i in 0..40i64 {
        vc = conj_current(&vc, Value::Int(i));
        vs = conj_split(&vs, Value::Int(i));
        assert_eq!(vc.len(), vs.len());
        for j in 0..vc.len() {
            assert_eq!(vc.get(j), vs.get(j));
        }
    }

    // (b)/(c)/(d)
    let big_pvec = build_big_pvec();
    let big_imbl = Arc::new(build_big_imbl());
    assert_eq!(scan_current(&big_pvec), scan_split(&big_imbl));
    assert_eq!(access_current(&big_pvec, 12345), access_split(&big_imbl, 12345));
    let mut pc = big_pvec.clone();
    let mut pu = Arc::new(build_big_imbl());
    let mut ps = Arc::new(build_big_imbl());
    for i in 0..50 {
        big_push_current(&mut pc, Value::Int(i));
        big_push_split_unique(&mut pu, Value::Int(i));
        big_push_split_shared(&mut ps, Value::Int(i));
    }
    assert_eq!(pc.len(), pu.len());
    assert_eq!(pc.len(), ps.len());

    // (e)
    let m = build_big_map();
    assert_eq!(iterate_current(&m), iterate_split(&m));

    // (f)
    let cur = PVec::pair(Value::Int(1), Value::Int(2));
    let spl = SplitPVec::pair(Value::Int(1), Value::Int(2));
    assert_eq!(clone_drop_current(&cur), clone_drop_split(&spl));
}

#[test]
#[ignore = "measurement, not a gate -- run with --ignored --nocapture"]
fn splitbox_pricing_probe() {
    println!("\n=== W-GEO re-plan kill-probe G: split-box (Big-arm-only) pricing ===");
    println!("  size_of::<PVec>()      = {:>3} (current: Small(Arc<[Value]>) | Big(imbl::Vector<Value>))", std::mem::size_of::<PVec>());
    println!("  size_of::<SplitPVec>() = {:>3} (mock:    Small(Arc<[Value]>) | Big(Arc<imbl::Vector<Value>>))", std::mem::size_of::<SplitPVec>());
    println!("  size_of::<PMap>()      = {:>3} (current: Small(Arc<Vec<(V,V)>>) | Big(PersistentHashMap))", std::mem::size_of::<PMap>());
    println!("  size_of::<SplitPMap>() = {:>3} (mock:    Small(Arc<Vec<(V,V)>>) | Big(Arc<PersistentHashMap>))", std::mem::size_of::<SplitPMap>());
    println!("  size_of::<Symbol>()    = {:>3} (Value::Sym's payload -- the NEXT-largest arm after boxing PVec/PMap)", std::mem::size_of::<Symbol>());
    println!("  size_of::<Value>()     = {:>3} (today, pinned by size_of_value_unchanged_by_bignum_variants)", std::mem::size_of::<Value>());
    println!(
        "  predicted Value width if PVec/PMap's Big arm alone is boxed: max arm becomes Symbol ({}B) + discriminant -- see docs/W-GEO-PROBE-VERDICTS.md's Probe G section for the exact arithmetic and caveats (Probe F prices Symbol/Str shrink separately).",
        std::mem::size_of::<Symbol>()
    );

    let ((sc_med, sc_min), (ss_med, ss_min)) = bench_small_churn_ns();
    let ((bc_med, bc_min), (bu_med, bu_min), (bs_med, bs_min)) = bench_big_churn_ns();
    let ((ac_med, ac_min), (as_med, as_min)) = bench_big_access_ns();
    let ((dc_med, dc_min), (ds_med, ds_min)) = bench_big_scan_ns();
    let ((ec_med, ec_min), (es_med, es_min)) = bench_map_iter_ns_per_entry();
    let ((fc_med, fc_min), (fs_med, fs_min)) = bench_value_move_ns();

    println!("\n  {:<38} {:>12} {:>12} {:>12} {:>12} {:>10}", "shape (unit)", "current med", "current min", "split med", "split min", "delta(med)");
    println!(
        "  {:<38} {:>12.2} {:>12.2} {:>12.2} {:>12.2} {:>+9.1}%",
        "(a) small-churn (1 conj)", sc_med, sc_min, ss_med, ss_min, (ss_med / sc_med - 1.0) * 100.0
    );
    println!(
        "  {:<38} {:>12.2} {:>12.2} {:>12.2} {:>12.2} {:>+9.1}%",
        "(b) big-churn unique (1 push)", bc_med, bc_min, bu_med, bu_min, (bu_med / bc_med - 1.0) * 100.0
    );
    println!(
        "  {:<38} {:>12.2} {:>12.2} {:>12.2} {:>12.2} {:>+9.1}%",
        "(b) big-churn shared (1 push)", bc_med, bc_min, bs_med, bs_min, (bs_med / bc_med - 1.0) * 100.0
    );
    println!(
        "  {:<38} {:>12.2} {:>12.2} {:>12.2} {:>12.2} {:>+9.1}%",
        "(c) big-access (1 random .get)", ac_med, ac_min, as_med, as_min, (as_med / ac_med - 1.0) * 100.0
    );
    println!(
        "  {:<38} {:>12.1} {:>12.1} {:>12.1} {:>12.1} {:>+9.1}%",
        "(d) big-scan (1 full 100k-elem)", dc_med, dc_min, ds_med, ds_min, (ds_med / dc_med - 1.0) * 100.0
    );
    println!(
        "  {:<38} {:>12.2} {:>12.2} {:>12.2} {:>12.2} {:>+9.1}%",
        "(e) map-iter proxy (ns/entry)", ec_med, ec_min, es_med, es_min, (es_med / ec_med - 1.0) * 100.0
    );
    println!(
        "  {:<38} {:>12.2} {:>12.2} {:>12.2} {:>12.2} {:>+9.1}%",
        "(f) value-move (1 clone+drop)", fc_med, fc_min, fs_med, fs_min, (fs_med / fc_med - 1.0) * 100.0
    );

    let e_delta_pct = (es_med / ec_med - 1.0) * 100.0;
    println!("\n  KILL BAR: shape (e) map-iter proxy, split-box vs current, median delta = {:+.1}%", e_delta_pct);
    let verdict = if e_delta_pct > 10.0 { "KILL" } else { "PROCEED-to-design" };
    println!("  VERDICT: {}", verdict);
    if verdict == "KILL" {
        println!("  (shape (e) regresses >10% median on the exact workload Probe E used to kill the five-arm box -- this narrower route is killed too)");
    } else {
        println!("  (shape (e) stays within the 10% bar on Probe E's own killing workload -- split-box clears the bar the five-arm box failed)");
    }

    assert!(sc_med > 0.0 && ss_med > 0.0 && ec_med > 0.0 && es_med > 0.0, "probe produced no measurement");
}
