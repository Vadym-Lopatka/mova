//! E3 (V05-PERF-PLAN) probe (B): UNIQUE-vs-SHARED MUTATION GAP.
//!
//! `#[ignore]`d tracking probes, not gates -- follows `compile::bench`'s
//! pattern (see that module's doc comment) but reports `ns/op` with
//! `[min, max]` over 5 measured rounds after 1 warmup round, per this
//! probe's discipline (V05-PERF-PLAN E3).
//!
//! ```text
//! cargo test --release --lib builtins::map_bench -- --ignored --nocapture
//! ```
//!
//! ## What's being measured, and why
//!
//! `Value::Map` holds an `imbl::HashMap<Value, Value>` directly (a HAMT
//! that refcounts its own internal chunks, invisibly to callers). `imbl`
//! mutates a chunk IN PLACE when its refcount is 1 at the moment of the
//! write, and copy-on-writes it otherwise. Every map-touching builtin in
//! `collections.rs` (`assoc_one`, etc.) does `let mut m2 = coll_ref.clone();
//! m2.insert(...)` -- i.e. it ALWAYS clones the handle before mutating,
//! because it only ever sees `coll: &Value`, never an owned one it could
//! move from. That clone alone is enough to force the chunk copy, EVEN
//! WHEN THE CALLER'S OWN COPY IS ABOUT TO BE DISCARDED (the common
//! `(recur (assoc m k v) ...)` loop shape: the env slot holding the old
//! `m` is still alive -- refcount 2 -- at the exact moment `assoc` clones
//! and mutates, and only gets dropped afterward when `recur` rebinds it).
//!
//! So the "shared" scenario below isn't a contrived worst case -- it's
//! `assoc_one`'s ACTUAL clone-then-mutate pattern, run in a tight loop
//! exactly like a `loop`/`recur` state-threading fn would. The "unique"
//! scenario is the counterfactual: what a move-aware ("Perceus-lite")
//! calling convention could buy back by mutating the caller's handle
//! directly when nothing else is holding a reference to it.
//!
//! The `Vec<(Value, Value)>` "array-map" scenario is the OTHER half of
//! E3's question: at these small sizes, does a linear-scan/clone-on-write
//! small-map representation simply cost less than a HAMT regardless of
//! ownership state?

use std::hint::black_box;
use std::time::{Duration, Instant};

use crate::value::Value;

const SIZES: [usize; 5] = [2, 4, 7, 16, 64];
const WARMUP_ROUNDS: u32 = 1;
const MEASURED_ROUNDS: u32 = 5;

fn kw(n: usize) -> Value {
    Value::Keyword(format!("k{n}").into())
}

fn keys_for(n: usize) -> Vec<Value> {
    (0..n).map(kw).collect()
}

fn fresh_imbl_map(keys: &[Value]) -> imbl::HashMap<Value, Value> {
    let mut m = imbl::HashMap::new();
    for (i, k) in keys.iter().enumerate() {
        m.insert(k.clone(), Value::Int(i as i64));
    }
    m
}

fn fresh_array_map(keys: &[Value]) -> Vec<(Value, Value)> {
    keys.iter()
        .enumerate()
        .map(|(i, k)| (k.clone(), Value::Int(i as i64)))
        .collect()
}

fn array_get<'a>(m: &'a [(Value, Value)], k: &Value) -> Option<&'a Value> {
    m.iter().find(|(kk, _)| kk == k).map(|(_, v)| v)
}

/// Mirrors `assoc_one`'s `Value::Map` arm exactly: clone the whole backing
/// `Vec` (the array-map's only option -- it has no structural sharing) and
/// overwrite/push the entry in the clone.
fn array_assoc(m: &[(Value, Value)], k: &Value, v: Value) -> Vec<(Value, Value)> {
    let mut m2 = m.to_vec();
    match m2.iter_mut().find(|(kk, _)| kk == k) {
        Some(entry) => entry.1 = v,
        None => m2.push((k.clone(), v)),
    }
    m2
}

/// Runs `f` (one full timed pass of `iters` operations) once as a discarded
/// warmup, then `MEASURED_ROUNDS` more times, returning each round's
/// elapsed `Duration`.
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

fn report(label: &str, size: usize, iters: u64, times: &[Duration]) {
    let ns_per_op: Vec<f64> = times.iter().map(|d| d.as_secs_f64() * 1e9 / iters as f64).collect();
    let min = ns_per_op.iter().cloned().fold(f64::INFINITY, f64::min);
    let max = ns_per_op.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    println!(
        "{label:28} n={size:<3} {iters:>9} iters/round x{MEASURED_ROUNDS}  [{min:>8.2}, {max:>8.2}] ns/op"
    );
}

/// (1) imbl `get`: no ownership split -- reads never mutate, so refcount
/// state is irrelevant. One number per size, for comparison against the
/// array-map's linear-scan `get` below.
#[test]
#[ignore = "tracking probe, not a gate: cargo test --release --lib -- --ignored --nocapture"]
fn bench_imbl_get() {
    const ITERS: u64 = 2_000_000;
    for &n in &SIZES {
        let keys = keys_for(n);
        let m = fresh_imbl_map(&keys);
        let times = timed_rounds(|| {
            for i in 0..ITERS {
                let k = &keys[(i as usize) % n];
                black_box(m.get(k));
            }
        });
        report("imbl get", n, ITERS, &times);
    }
}

/// (2) imbl `assoc`/`insert`, UNIQUE handle: `m` is never cloned before the
/// mutating `insert`, so its root chunk sits at refcount 1 for the whole
/// loop -- imbl's in-place mutation path. This is the counterfactual a
/// move-aware calling convention could realize: "the caller's map is dead,
/// so assoc can just mutate it".
#[test]
#[ignore = "tracking probe, not a gate: cargo test --release --lib -- --ignored --nocapture"]
fn bench_imbl_assoc_unique() {
    const ITERS: u64 = 500_000;
    for &n in &SIZES {
        let keys = keys_for(n);
        let times = timed_rounds(|| {
            let mut m = fresh_imbl_map(&keys);
            for i in 0..ITERS {
                let k = &keys[(i as usize) % n];
                m.insert(k.clone(), Value::Int(i as i64));
            }
            black_box(&m);
        });
        report("imbl assoc (unique)", n, ITERS, &times);
    }
}

/// (3) imbl `assoc`/`insert`, SHARED handle: this is `assoc_one`'s actual
/// pattern (`let mut m2 = coll.clone(); m2.insert(...)`), run once per
/// loop iteration exactly as a `(recur (assoc m k v) ...)` state-threading
/// fn would call it -- the "env slot" `m` stays alive (refcount 2) across
/// every single clone+insert, so every insert pays imbl's copy-on-write.
#[test]
#[ignore = "tracking probe, not a gate: cargo test --release --lib -- --ignored --nocapture"]
fn bench_imbl_assoc_shared() {
    const ITERS: u64 = 500_000;
    for &n in &SIZES {
        let keys = keys_for(n);
        let times = timed_rounds(|| {
            let mut m = fresh_imbl_map(&keys);
            for i in 0..ITERS {
                let k = &keys[(i as usize) % n];
                // `assoc_one`'s exact shape: clone the handle the "caller"
                // (this loop's own `m` binding, still alive below) is
                // holding, then mutate the clone.
                let mut m2 = m.clone();
                m2.insert(k.clone(), Value::Int(i as i64));
                black_box(&m2);
                m = m2; // "recur" rebinds; the pre-clone `m` is now dead
            }
        });
        report("imbl assoc (shared)", n, ITERS, &times);
    }
}

/// (4) array-map `get`: linear scan, the candidate small-map repr's read
/// cost model.
#[test]
#[ignore = "tracking probe, not a gate: cargo test --release --lib -- --ignored --nocapture"]
fn bench_array_map_get() {
    const ITERS: u64 = 2_000_000;
    for &n in &SIZES {
        let keys = keys_for(n);
        let m = fresh_array_map(&keys);
        let times = timed_rounds(|| {
            for i in 0..ITERS {
                let k = &keys[(i as usize) % n];
                black_box(array_get(&m, k));
            }
        });
        report("array-map get", n, ITERS, &times);
    }
}

/// (5) array-map `assoc`: clone-the-whole-`Vec`-on-write, the candidate
/// small-map repr's write cost model (it has no structural sharing at all,
/// so there is no unique/shared split to measure -- every assoc pays the
/// same `Vec::clone` + linear scan, at every refcount).
#[test]
#[ignore = "tracking probe, not a gate: cargo test --release --lib -- --ignored --nocapture"]
fn bench_array_map_assoc() {
    const ITERS: u64 = 500_000;
    for &n in &SIZES {
        let keys = keys_for(n);
        let times = timed_rounds(|| {
            let mut m = fresh_array_map(&keys);
            for i in 0..ITERS {
                let k = &keys[(i as usize) % n];
                m = array_assoc(&m, k, Value::Int(i as i64));
                black_box(&m);
            }
        });
        report("array-map assoc", n, ITERS, &times);
    }
}
