//! Phase E allocation-attribution profile for `to_value` (EMBED-API-PLAN.md
//! §4 Phase E / §2.4): counts allocator calls (`alloc`+`alloc_zeroed`+
//! `realloc`, mirroring `dsbench/src/alloc.rs`'s convention) per `to_value`
//! invocation on the same fixtures as `serde_bridge_bench.rs`, so the
//! ~1.2-1.4us/struct gap vs `serde_json` can be attributed to actual
//! allocator traffic instead of guessed from code inspection alone.
//!
//! A `#[global_allocator]` is process-wide, so this has to be its own bench
//! binary (mixing it into `serde_bridge_bench.rs` would also count every
//! `serde_json` allocation, which isn't the question here).
//!
//! Run with: `cargo bench --features serde --bench serde_alloc_profile`

use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::sync::atomic::{AtomicI64, Ordering};

use serde::{Deserialize, Serialize};

use mova::serde_bridge::{from_value, to_value};

static LIVE_BYTES: AtomicI64 = AtomicI64::new(0);
static ALLOC_CALLS: AtomicI64 = AtomicI64::new(0);

struct CountingAllocator;

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            LIVE_BYTES.fetch_add(layout.size() as i64, Ordering::Relaxed);
            ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
        }
        ptr
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        LIVE_BYTES.fetch_sub(layout.size() as i64, Ordering::Relaxed);
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            LIVE_BYTES.fetch_add(layout.size() as i64, Ordering::Relaxed);
            ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
        }
        ptr
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_ptr = unsafe { System.realloc(ptr, layout, new_size) };
        if !new_ptr.is_null() {
            LIVE_BYTES.fetch_add(new_size as i64 - layout.size() as i64, Ordering::Relaxed);
            ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
        }
        new_ptr
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

fn alloc_calls() -> i64 {
    ALLOC_CALLS.load(Ordering::SeqCst)
}

// ------------------------------- fixtures ---------------------------------
// (identical shapes to serde_bridge_bench.rs's flat/nested fixtures)

#[derive(Clone, Serialize, Deserialize)]
struct FlatStruct {
    id: u64,
    name: String,
    score: f64,
    active: bool,
    count: i32,
    ratio: f32,
    tag: char,
    weight: i64,
    note: String,
    verified: bool,
}

fn flat_fixture() -> FlatStruct {
    FlatStruct {
        id: 42,
        name: "widget-9000".to_string(),
        score: 98.6,
        active: true,
        count: -17,
        ratio: 0.5,
        tag: 'Q',
        weight: 123_456_789,
        note: "hello, world".to_string(),
        verified: false,
    }
}

/// Runs `f` once, warmed up (called once, discarded) so any one-time lazy
/// init (e.g. a thread-local cache's first allocation) doesn't pollute the
/// steady-state count -- then reports the allocator-call delta of ONE more
/// call.
fn count_allocs<R>(mut f: impl FnMut() -> R) -> i64 {
    black_box(f()); // warmup / prime any lazy statics
    let before = alloc_calls();
    black_box(f());
    alloc_calls() - before
}

/// Same as `count_allocs` but amortized over `n` calls in a tight loop --
/// used for the Vec<flat,1000> case's per-item attribution, and to confirm
/// steady-state (post-warmup) behavior of the per-call variant.
fn count_allocs_amortized<R>(mut f: impl FnMut() -> R, n: u64) -> f64 {
    black_box(f()); // warmup
    let before = alloc_calls();
    for _ in 0..n {
        black_box(f());
    }
    (alloc_calls() - before) as f64 / n as f64
}

fn report(label: &str, calls: f64) {
    println!("{label:<52} {calls:>10.2} allocator calls");
}

fn main() {
    println!("mova serde_bridge allocation profile (alloc+alloc_zeroed+realloc calls)\n");

    let flat = flat_fixture();
    report("to_value(flat 10-field struct)  [1 call]", count_allocs(|| to_value(&flat).unwrap()) as f64);
    report(
        "to_value(flat 10-field struct)  [amortized/1000]",
        count_allocs_amortized(|| to_value(&flat).unwrap(), 1000),
    );

    let flat_value = to_value(&flat).unwrap();
    report(
        "from_value(flat 10-field struct) [amortized/1000] (canary)",
        count_allocs_amortized(|| { let _: FlatStruct = from_value(&flat_value).unwrap(); }, 1000),
    );

    // Isolate the field-name-keyword cost: a struct with the SAME field
    // names repeated across many instances (the Vec<flat,1000> shape) vs a
    // struct serialized only once -- if field-name Str::from allocation is
    // a real per-call cost (no caching), the amortized/single-call numbers
    // for `to_value(flat)` above should already show it (every call reruns
    // Str::from on every field regardless of repetition), so this isolates
    // it further by comparing a 1-field struct against the 10-field one:
    // the delta / 9 approximates the per-field-name allocation cost.
    #[derive(Clone, Serialize, Deserialize)]
    struct OneField {
        id: u64,
    }
    let one = OneField { id: 42 };
    let one_calls = count_allocs_amortized(|| to_value(&one).unwrap(), 1000);
    let flat_calls = count_allocs_amortized(|| to_value(&flat).unwrap(), 1000);
    report("to_value(1-field struct) [amortized/1000]", one_calls);
    report("to_value(10-field struct) [amortized/1000] (repeat)", flat_calls);
    println!(
        "  => ~{:.2} allocator calls per ADDITIONAL struct field (9-field delta / 9)",
        (flat_calls - one_calls) / 9.0
    );

    // Vec<FlatStruct, 1000> -- same field-name Str literals hit 1000 times
    // in one to_value call; if field-name keywords are cached, the interior
    // per-item cost should be lower than the standalone per-call cost above
    // once the cache is warm within a single invocation. Measuring the
    // WHOLE Vec<1000> serialize as one "call" (so intra-call reuse can
    // help even without a persistent cache).
    let vec1000: Vec<FlatStruct> = (0..1000).map(|_| flat_fixture()).collect();
    let vec_calls = count_allocs(|| to_value(&vec1000).unwrap());
    report("to_value(Vec<flat>,1000)  [1 call, total]", vec_calls as f64);
    report("to_value(Vec<flat>,1000)  [1 call, per-item]", vec_calls as f64 / 1000.0);
}
