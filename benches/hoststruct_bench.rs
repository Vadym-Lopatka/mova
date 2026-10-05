//! W3 kill-probe + landing measurements (LATENCY-CAMPAIGN.md §W3): the
//! `Value::HostStruct` zero-copy boundary, measured against the REAL
//! interpreter (`Engine::eval`/`call`), not a toy harness -- interleaved
//! same-round A/B per house rules (this machine has ~30% thermal drift
//! over long runs; every number below is a same-round HostStruct-vs-Map
//! comparison, never reported in isolation).
//!
//! Run with: `cargo bench --bench hoststruct_bench`
//! (no `--features serde` needed -- `embed::host` has no serde dependency;
//! the "to_value'd map" comparator below is built directly via
//! `embed::Value::map` and `embed::Value::from`, which is exactly what a
//! flat struct's `to_value` output looks like once materialized).

use std::hint::black_box;
use std::sync::Arc;
use std::time::Instant;

use mova::embed::host::{from_value_arc, from_value_typed, wrap_struct, ShapeBuilder};
use mova::embed::{Engine, Profile, Value};

#[derive(Clone)]
struct FlatStruct {
    id: i64,
    name: String,
    score: f64,
    active: bool,
    count: i64,
    ratio: f64,
    tag: String,
    weight: i64,
    note: String,
    verified: bool,
}

fn fixture() -> FlatStruct {
    FlatStruct {
        id: 42,
        name: "widget-9000".to_string(),
        score: 98.6,
        active: true,
        count: -17,
        ratio: 0.5,
        tag: "Q".to_string(),
        weight: 123_456_789,
        note: "hello, world".to_string(),
        verified: false,
    }
}

fn shape() -> ShapeBuilder<FlatStruct> {
    ShapeBuilder::<FlatStruct>::new("FlatStruct")
        .field("id", |s| Value::from(s.id))
        .field("name", |s| Value::from(s.name.as_str()))
        .field("score", |s| Value::from(s.score))
        .field("active", |s| Value::from(s.active))
        .field("count", |s| Value::from(s.count))
        .field("ratio", |s| Value::from(s.ratio))
        .field("tag", |s| Value::from(s.tag.as_str()))
        .field("weight", |s| Value::from(s.weight))
        .field("note", |s| Value::from(s.note.as_str()))
        .field("verified", |s| Value::from(s.verified))
}

/// Stand-in for `to_value(&fixture())`'s output shape -- an already-
/// materialized flat `Map`, exactly what a host would `engine.def` today
/// without this experiment.
fn as_map(s: &FlatStruct) -> Value {
    Value::map([
        (Value::keyword("id"), Value::from(s.id)),
        (Value::keyword("name"), Value::from(s.name.as_str())),
        (Value::keyword("score"), Value::from(s.score)),
        (Value::keyword("active"), Value::from(s.active)),
        (Value::keyword("count"), Value::from(s.count)),
        (Value::keyword("ratio"), Value::from(s.ratio)),
        (Value::keyword("tag"), Value::from(s.tag.as_str())),
        (Value::keyword("weight"), Value::from(s.weight)),
        (Value::keyword("note"), Value::from(s.note.as_str())),
        (Value::keyword("verified"), Value::from(s.verified)),
    ])
}

fn report(name: &str, ns: f64) {
    println!("{name:<58} {ns:>10.2} ns/op");
}

fn main() {
    // -------- wrap cost: interleaved same-round --------
    let arc_fixture = Arc::new(fixture());
    let sh = shape().build();
    let mut hs_rounds = Vec::new();
    let mut map_rounds = Vec::new();
    for _ in 0..6 {
        let t0 = Instant::now();
        for _ in 0..200_000u64 {
            black_box(wrap_struct(Arc::clone(&arc_fixture), &sh));
        }
        hs_rounds.push(t0.elapsed().as_nanos() as f64 / 200_000.0);

        let t0 = Instant::now();
        for _ in 0..200_000u64 {
            black_box(as_map(&arc_fixture));
        }
        map_rounds.push(t0.elapsed().as_nanos() as f64 / 200_000.0);
    }
    hs_rounds.remove(0);
    map_rounds.remove(0);
    hs_rounds.sort_by(|a, b| a.partial_cmp(b).unwrap());
    map_rounds.sort_by(|a, b| a.partial_cmp(b).unwrap());
    report("WRAP wrap_struct(Arc::clone) [interleaved]", hs_rounds[2]);
    report("WRAP as_map (to_value-shaped, 10 fields) [interleaved]", map_rounds[2]);
    println!(
        "  min-max HostStruct wrap: [{:.2}-{:.2}]  Map build: [{:.2}-{:.2}]",
        hs_rounds.iter().cloned().fold(f64::MAX, f64::min),
        hs_rounds.iter().cloned().fold(f64::MIN, f64::max),
        map_rounds.iter().cloned().fold(f64::MAX, f64::min),
        map_rounds.iter().cloned().fold(f64::MIN, f64::max),
    );

    // -------- real script loop through Engine::call/eval, interleaved --------
    const N: u64 = 200_000;
    let mut engine = Engine::builder().profile(Profile::Pure).build();
    let hs_val = wrap_struct(Arc::clone(&arc_fixture), &sh);
    let map_val = as_map(&arc_fixture);
    engine.def("hs", hs_val);
    engine.def("m", map_val);

    let mut hs_loop_rounds = Vec::new();
    let mut map_loop_rounds = Vec::new();
    for _ in 0..6 {
        let t0 = Instant::now();
        engine.eval(&format!("(dotimes [_ {N}] (:weight hs))")).unwrap();
        hs_loop_rounds.push(t0.elapsed().as_nanos() as f64 / N as f64);

        let t0 = Instant::now();
        engine.eval(&format!("(dotimes [_ {N}] (:weight m))")).unwrap();
        map_loop_rounds.push(t0.elapsed().as_nanos() as f64 / N as f64);
    }
    hs_loop_rounds.remove(0);
    map_loop_rounds.remove(0);
    hs_loop_rounds.sort_by(|a, b| a.partial_cmp(b).unwrap());
    map_loop_rounds.sort_by(|a, b| a.partial_cmp(b).unwrap());
    report("SCRIPT (dotimes [_ N] (:weight hs)) [HostStruct]", hs_loop_rounds[2]);
    report("SCRIPT (dotimes [_ N] (:weight m))  [Map, to_value-shaped]", map_loop_rounds[2]);
    println!(
        "  min-max HostStruct loop: [{:.2}-{:.2}]  Map loop: [{:.2}-{:.2}]",
        hs_loop_rounds.iter().cloned().fold(f64::MAX, f64::min),
        hs_loop_rounds.iter().cloned().fold(f64::MIN, f64::max),
        map_loop_rounds.iter().cloned().fold(f64::MAX, f64::min),
        map_loop_rounds.iter().cloned().fold(f64::MIN, f64::max),
    );

    // -------- IC warm vs a fresh (never-touched-by-this-shape) keyword --------
    let mut warm_rounds = Vec::new();
    for _ in 0..6 {
        let t0 = Instant::now();
        engine.eval(&format!("(dotimes [_ {N}] (:name hs))")).unwrap();
        warm_rounds.push(t0.elapsed().as_nanos() as f64 / N as f64);
    }
    warm_rounds.remove(0);
    warm_rounds.sort_by(|a, b| a.partial_cmp(b).unwrap());
    report("SCRIPT (dotimes [_ N] (:name hs)) [2nd field, IC warms]", warm_rounds[2]);

    // -------- round trip: host -> script reads 2 fields -> typed extraction --------
    let mut rt_rounds = Vec::new();
    for _ in 0..6 {
        let t0 = Instant::now();
        for _ in 0..50_000u64 {
            let v = wrap_struct(Arc::clone(&arc_fixture), &sh);
            engine.def("rt", v);
            let r = engine.eval("[(:weight rt) (:name rt)]").unwrap();
            black_box(&r);
            let held = engine.get("rt").unwrap();
            let extracted: Option<FlatStruct> = from_value_typed(&held);
            black_box(extracted);
        }
        rt_rounds.push(t0.elapsed().as_nanos() as f64 / 50_000.0);
    }
    rt_rounds.remove(0);
    rt_rounds.sort_by(|a, b| a.partial_cmp(b).unwrap());
    report("ROUNDTRIP wrap+2 script reads+typed clone-extract", rt_rounds[2]);

    let mut rt_arc_rounds = Vec::new();
    for _ in 0..6 {
        let t0 = Instant::now();
        for _ in 0..500_000u64 {
            let v = wrap_struct(Arc::clone(&arc_fixture), &sh);
            let arc: Option<Arc<FlatStruct>> = from_value_arc(&v);
            black_box(arc);
        }
        rt_arc_rounds.push(t0.elapsed().as_nanos() as f64 / 500_000.0);
    }
    rt_arc_rounds.remove(0);
    rt_arc_rounds.sort_by(|a, b| a.partial_cmp(b).unwrap());
    report("ROUNDTRIP wrap+from_value_arc (no script)", rt_arc_rounds[2]);

    #[cfg(feature = "serde")]
    real_to_value_reference();
}

/// Reference point ONLY (not part of the interleaved A/B above): the
/// REAL `serde_bridge::to_value` cost on an identically-shaped 10-field
/// struct, which is what `as_map`/`Value::map` above is standing in for.
/// `Value::map` (the public embed constructor, used above so this bench
/// doesn't need `--features serde` to run at all) goes through
/// `PMap::from_iter`'s repeated-`insert` path, NOT `to_value`'s optimized
/// `PMap::from_unique_pairs` bulk build -- this fn exists so the gap
/// between "the public map constructor a bench can reach" and "what
/// to_value's own internal fast path actually costs" is measured
/// explicitly rather than silently conflated. Run with `cargo bench
/// --bench hoststruct_bench --features serde`.
#[cfg(feature = "serde")]
fn real_to_value_reference() {
    #[derive(Clone, serde::Serialize)]
    struct FlatStructSerde {
        id: i64,
        name: String,
        score: f64,
        active: bool,
        count: i64,
        ratio: f64,
        tag: String,
        weight: i64,
        note: String,
        verified: bool,
    }
    let s = FlatStructSerde {
        id: 42,
        name: "widget-9000".into(),
        score: 98.6,
        active: true,
        count: -17,
        ratio: 0.5,
        tag: "Q".into(),
        weight: 123_456_789,
        note: "hello, world".into(),
        verified: false,
    };
    let mut rounds = Vec::new();
    for _ in 0..6 {
        let t0 = Instant::now();
        for _ in 0..200_000u64 {
            black_box(mova::serde_bridge::to_value(black_box(&s)).unwrap());
        }
        rounds.push(t0.elapsed().as_nanos() as f64 / 200_000.0);
    }
    rounds.remove(0);
    rounds.sort_by(|a, b| a.partial_cmp(b).unwrap());
    report("REFERENCE serde_bridge::to_value (real, 10 fields)", rounds[2]);
}
