//! One-engine-per-worker via [`Engine::snapshot`]: a template `Engine` is
//! built ONCE (bootstrapping `core.mova` -- the expensive part), then every
//! worker thread gets its own independent snapshot to mutate freely, with
//! no `Mutex`/`RwLock` serializing them onto a single interpreter. Prints
//! the timing asymmetry -- template build vs per-worker snapshot cost --
//! so the value of "build once, fork cheaply" is visible in the numbers,
//! not just asserted in prose.
//!
//! Run: `cargo run --release --example embed_snapshot_pool`

use std::time::Instant;

use mova::embed::{Engine, Profile, Value};

const WORKERS: usize = 8;

/// Each worker's own workload -- deliberately small and self-contained
/// (`n`-th worker computes a different Fibonacci index), so results are
/// easy to eyeball as genuinely independent per-worker state, not shared
/// leftovers from `self`.
fn worker_script(n: usize) -> String {
    format!(
        "(loop [a 0 b 1 i 0] (if (< i {n}) (recur b (+ a b) (inc i)) a))"
    )
}

fn main() {
    // Paid ONCE: registers every builtin, bootstraps `core.mova` (and, for
    // `Scripting`, `core/async.mova`/`core/flow.mova`).
    let t0 = Instant::now();
    let template = Engine::builder().profile(Profile::Pure).build();
    let build_time = t0.elapsed();

    // Snapshotting N times BEFORE spawning workers (the documented caller
    // contract: no concurrent `def` racing the snapshot walk) -- each
    // `Engine` returned is a fully independent world, ready to hand to its
    // own thread.
    let mut snapshot_times = Vec::with_capacity(WORKERS);
    let mut engines = Vec::with_capacity(WORKERS);
    for _ in 0..WORKERS {
        let t = Instant::now();
        let snap = template.snapshot();
        snapshot_times.push(t.elapsed());
        engines.push(snap);
    }

    println!("-- template build vs per-worker snapshot cost --");
    println!("  template build:   {build_time:>10.2?}  (paid once)");
    let avg_snapshot: std::time::Duration = snapshot_times.iter().sum::<std::time::Duration>() / WORKERS as u32;
    println!("  avg snapshot:     {avg_snapshot:>10.2?}  (paid per worker, x{WORKERS})");
    let ratio = build_time.as_secs_f64() / avg_snapshot.as_secs_f64().max(1e-12);
    println!("  build/snapshot ratio: {ratio:.0}x -- this is the asymmetry snapshot() exploits");

    println!();
    println!("-- {WORKERS} workers, each evaluating independently on its own snapshot --");
    let handles: Vec<_> = engines
        .into_iter()
        .enumerate()
        .map(|(i, mut engine)| {
            std::thread::spawn(move || {
                let script = worker_script(i + 5);
                let t = Instant::now();
                let result = engine.eval(&script).expect("worker script should evaluate cleanly");
                (i, result, t.elapsed())
            })
        })
        .collect();

    let mut results: Vec<(usize, Value, std::time::Duration)> = handles.into_iter().map(|h| h.join().expect("worker thread should not panic")).collect();
    results.sort_by_key(|(i, _, _)| *i);
    for (i, result, elapsed) in &results {
        println!("  worker {i}: fib({}) = {:<8} in {elapsed:.2?}", i + 5, result.as_i64().unwrap());
    }

    println!();
    println!("  each worker mutated its OWN engine (its own `loop`/`recur` bindings,");
    println!("  its own var table) -- no Mutex/RwLock serialized these {WORKERS} threads");
    println!("  onto a shared interpreter; snapshot() is what makes that legal.");
}
