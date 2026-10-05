//! Design-spike measurement bin for `embed/probe-snapshot` (NOT part of the
//! public crate surface): times `Interp::snapshot()` at increasing world
//! sizes, and gives a rough per-snapshot memory cost by comparing RSS before
//! and after retaining 100 snapshots. Run with `cargo run --release --bin
//! snapshot_bench` -- release matters, this is a perf number, not a
//! correctness check.

use std::time::{Duration, Instant};

use mova::internal::{render, Interp};

/// Builds a world with `n_defs` plain `def`s plus 300 `defn`s (a fixed
/// "few hundred" per the spike's brief, independent of `n_defs` so the
/// def-count sweep isn't confounded by a changing fn count too).
fn build_world(n_defs: usize) -> Interp {
    let mut src = String::with_capacity(n_defs * 16 + 300 * 40);
    for i in 0..n_defs {
        src.push_str(&format!("(def x{i} {i})\n"));
    }
    for i in 0..300 {
        src.push_str(&format!("(defn f{i} [a b] (+ a b {i}))\n"));
    }
    let mut interp = Interp::new();
    interp
        .eval_str("bench", &src)
        .unwrap_or_else(|e| panic!("world build failed at n_defs={n_defs}: {}", render(&e, "bench", &src)));
    interp
}

fn median(mut xs: Vec<Duration>) -> Duration {
    xs.sort();
    xs[xs.len() / 2]
}

/// Current process RSS in bytes, macOS/Darwin (`ru_maxrss` is already
/// bytes there, unlike Linux's KB) -- fine for this spike since we only
/// ever grow retained memory across the measurement window, so "max ever"
/// tracks "current" closely.
fn rss_bytes() -> u64 {
    #[cfg(target_os = "macos")]
    {
        unsafe {
            let mut usage: libc::rusage = std::mem::zeroed();
            let rc = libc::getrusage(libc::RUSAGE_SELF, &mut usage);
            assert_eq!(rc, 0, "getrusage failed");
            usage.ru_maxrss as u64
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        unsafe {
            let mut usage: libc::rusage = std::mem::zeroed();
            let rc = libc::getrusage(libc::RUSAGE_SELF, &mut usage);
            assert_eq!(rc, 0, "getrusage failed");
            usage.ru_maxrss as u64 * 1024
        }
    }
}

fn main() {
    println!("=== snapshot() timing, median of 5 runs ===");
    for &n in &[100usize, 1_000, 10_000] {
        let interp = build_world(n);
        let mut samples = Vec::with_capacity(5);
        for _ in 0..5 {
            let start = Instant::now();
            let clone = interp.snapshot();
            let elapsed = start.elapsed();
            std::hint::black_box(&clone);
            samples.push(elapsed);
            drop(clone);
        }
        let med = median(samples.clone());
        println!(
            "N={n:>6}  median={:>10} ns  ({:.3} ms)   samples(ns)={:?}",
            med.as_nanos(),
            med.as_secs_f64() * 1000.0,
            samples.iter().map(|d| d.as_nanos()).collect::<Vec<_>>()
        );
    }

    println!();
    println!("=== RSS cost of retaining 100 snapshots at N=1000 ===");
    let interp = build_world(1_000);
    // Warm up: one throwaway snapshot + a moment, so allocator metadata /
    // lazily-faulted pages from the FIRST-ever snapshot path don't bias the
    // "before" baseline against the other 99.
    drop(interp.snapshot());
    let rss_before = rss_bytes();
    let mut retained = Vec::with_capacity(100);
    for _ in 0..100 {
        retained.push(interp.snapshot());
    }
    let rss_after = rss_bytes();
    std::hint::black_box(&retained);
    let delta = rss_after.saturating_sub(rss_before);
    println!("RSS before: {rss_before} bytes ({:.2} MiB)", rss_before as f64 / (1024.0 * 1024.0));
    println!("RSS after:  {rss_after} bytes ({:.2} MiB)", rss_after as f64 / (1024.0 * 1024.0));
    println!(
        "delta:      {delta} bytes ({:.2} MiB) over 100 snapshots -> ~{:.1} KiB/snapshot",
        delta as f64 / (1024.0 * 1024.0),
        delta as f64 / 100.0 / 1024.0
    );
    // Keep `retained` alive until here so the compiler can't drop it early.
    println!("retained count: {}", retained.len());
}
