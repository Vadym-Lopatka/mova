//! alloc_attrib: W4's attribution census. A counting global allocator that
//! samples 1-in-N allocation backtraces during a marked measurement window
//! (the flow-gen-sink run), attributing every allocation in the flow hot
//! path to a named source site. Bench-binary only -- never linked into the
//! `mova` binary or the library.
//!
//! Usage: cargo run --profile bench-attrib --bin alloc_attrib [N-msgs]
//!
//! Output: a table  site -> allocs/msg -> size-class histogram, where a
//! "site" is the innermost few mova frames of the sampled backtrace.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::Mutex;

const SAMPLE_EVERY: u64 = 64;

static WINDOW: AtomicBool = AtomicBool::new(false);
static ALLOCS: AtomicU64 = AtomicU64::new(0);
static BYTES: AtomicU64 = AtomicU64::new(0);
static SIZE_HIST: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];

thread_local! {
    static IN_HOOK: Cell<bool> = const { Cell::new(false) };
}

/// site-key -> (sample-count, per-size-class sample counts)
type SampleMap = std::collections::HashMap<String, (u64, [u64; 6])>;
static SAMPLES: Mutex<Option<SampleMap>> = Mutex::new(None);

fn size_class(n: usize) -> usize {
    match n {
        0..=32 => 0,
        33..=64 => 1,
        65..=128 => 2,
        129..=512 => 3,
        513..=4096 => 4,
        _ => 5,
    }
}

const SIZE_NAMES: [&str; 6] = ["<=32", "<=64", "<=128", "<=512", "<=4K", ">4K"];

/// Distill a std::backtrace::Backtrace's Display output into a compact site
/// key: the innermost 1-4 frames that belong to mova/imbl, skipping the
/// allocator plumbing itself.
fn site_key(bt: &str) -> String {
    let mut frames: Vec<String> = Vec::new();
    for line in bt.lines() {
        let line = line.trim();
        // Frame name lines look like "N: symbol" (paths are on following
        // "at ..." lines, which we ignore).
        let Some((_, name)) = line.split_once(": ") else { continue };
        if name.starts_with("at ") {
            continue;
        }
        // Skip the census/allocator machinery and std plumbing.
        if name.contains("alloc_attrib")
            || name.contains("__rust_alloc")
            || name.contains("::alloc::")
            || name.starts_with("alloc::")
            || name.starts_with("std::")
            || name.starts_with("core::")
            || name.contains("RawVec")
            || name.contains("backtrace")
        {
            continue;
        }
        let keep = name.contains("mova") || name.contains("imbl") || name.contains("champ");
        if keep {
            // Strip hash suffixes and generic noise for aggregation.
            let clean = name.split("::h").next().unwrap_or(name);
            frames.push(clean.to_string());
            if frames.len() == 4 {
                break;
            }
        }
    }
    if frames.is_empty() {
        // Keep ONE representative foreign frame so non-mova sites are visible.
        for line in bt.lines() {
            let line = line.trim();
            let Some((_, name)) = line.split_once(": ") else { continue };
            if name.starts_with("at ")
                || name.contains("alloc_attrib")
                || name.contains("__rust_alloc")
                || name.contains("backtrace")
            {
                continue;
            }
            let clean = name.split("::h").next().unwrap_or(name);
            return format!("[foreign] {clean}");
        }
        return "[unresolved]".to_string();
    }
    frames.join(" <- ")
}

struct Attrib;

unsafe impl GlobalAlloc for Attrib {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = System.alloc(layout);
        if p.is_null() || !WINDOW.load(Relaxed) {
            return p;
        }
        let reentrant = IN_HOOK.with(|c| c.get());
        if reentrant {
            return p;
        }
        let n = ALLOCS.fetch_add(1, Relaxed) + 1;
        BYTES.fetch_add(layout.size() as u64, Relaxed);
        SIZE_HIST[size_class(layout.size())].fetch_add(1, Relaxed);
        if n.is_multiple_of(SAMPLE_EVERY) {
            IN_HOOK.with(|c| c.set(true));
            let bt = std::backtrace::Backtrace::force_capture();
            let key = site_key(&format!("{bt}"));
            let sc = size_class(layout.size());
            if let Ok(mut g) = SAMPLES.lock() {
                let map = g.get_or_insert_with(Default::default);
                let e = map.entry(key).or_insert((0, [0; 6]));
                e.0 += 1;
                e.1[sc] += 1;
            }
            IN_HOOK.with(|c| c.set(false));
        }
        p
    }

    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        System.dealloc(p, layout)
    }
}

#[global_allocator]
static A: Attrib = Attrib;

fn main() {
    use mova::internal::Interp;

    let n_msgs: u64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(50_000);

    let mut interp = Interp::new();
    interp.max_depth = 10_000;

    let bench_path = std::env::args().nth(2).unwrap_or_else(|| "bench/flow-gen-sink.mova".to_string());
    let flow_src = std::fs::read_to_string(&bench_path)
        .expect("run from the mova worktree root")
        .replace("(def N 200000)", &format!("(def N {n_msgs})"))
        .replace("(def N 1600000)", &format!("(def N {n_msgs})"));

    WINDOW.store(true, Relaxed);
    let t0 = std::time::Instant::now();
    interp.eval_str("flow-gen-sink", &flow_src).unwrap();
    let wall = t0.elapsed();
    WINDOW.store(false, Relaxed);

    let allocs = ALLOCS.load(Relaxed);
    let bytes = BYTES.load(Relaxed);
    println!("== alloc_attrib: flow-gen-sink N={n_msgs} ==");
    println!(
        "window: {allocs} allocs, {:.2} MB, {:.1} ms wall ({:.0} msg/s)",
        bytes as f64 / 1e6,
        wall.as_secs_f64() * 1e3,
        n_msgs as f64 / wall.as_secs_f64()
    );
    println!(
        "per msg: {:.2} allocs, {:.0} bytes",
        allocs as f64 / n_msgs as f64,
        bytes as f64 / n_msgs as f64
    );
    let hist: Vec<u64> = SIZE_HIST.iter().map(|a| a.load(Relaxed)).collect();
    print!("size classes/msg:");
    for (i, c) in hist.iter().enumerate() {
        print!("  {}:{:.2}", SIZE_NAMES[i], *c as f64 / n_msgs as f64);
    }
    println!();
    println!();

    let g = SAMPLES.lock().unwrap();
    let map = g.as_ref().cloned().unwrap_or_default();
    let total_samples: u64 = map.values().map(|(c, _)| *c).sum();
    let mut rows: Vec<(&String, &(u64, [u64; 6]))> = map.iter().collect();
    rows.sort_by(|a, b| b.1 .0.cmp(&a.1 .0));
    println!(
        "== attribution (1/{SAMPLE_EVERY} sampled, {total_samples} samples; est allocs/msg = share * {:.2}) ==",
        allocs as f64 / n_msgs as f64
    );
    for (site, (count, sizes)) in rows {
        let share = *count as f64 / total_samples as f64;
        let est_per_msg = share * allocs as f64 / n_msgs as f64;
        let dominant = sizes
            .iter()
            .enumerate()
            .max_by_key(|(_, c)| **c)
            .map(|(i, _)| SIZE_NAMES[i])
            .unwrap_or("?");
        println!("{est_per_msg:6.2}/msg  {:5.1}%  [{dominant:5}]  {site}", share * 100.0);
    }
}
