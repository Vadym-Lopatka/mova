//! P0b: in-process boot timing + alloc census. `p0b_boot [N]`
//! env MOVA_BOOT_NATIVES_ONLY=1 measures natives-only boot.
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Instant;
use mova::internal::Interp;

static TOTAL: AtomicU64 = AtomicU64::new(0);
static LIVE_N: AtomicU64 = AtomicU64::new(0);
static LIVE_B: AtomicU64 = AtomicU64::new(0);
struct C;
unsafe impl GlobalAlloc for C {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        TOTAL.fetch_add(1, Relaxed); LIVE_N.fetch_add(1, Relaxed); LIVE_B.fetch_add(l.size() as u64, Relaxed);
        System.alloc(l)
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        LIVE_N.fetch_sub(1, Relaxed); LIVE_B.fetch_sub(l.size() as u64, Relaxed);
        System.dealloc(p, l)
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        LIVE_B.fetch_add(n as u64, Relaxed); LIVE_B.fetch_sub(l.size() as u64, Relaxed);
        TOTAL.fetch_add(1, Relaxed);
        System.realloc(p, l, n)
    }
}
#[global_allocator]
static A: C = C;

fn main() {
    let n: usize = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(30);
    let (n0, b0, t0) = (LIVE_N.load(Relaxed), LIVE_B.load(Relaxed), TOTAL.load(Relaxed));
    let mut v = Vec::new();
    let mut keep = None;
    for i in 0..n {
        let t = Instant::now();
        let it = Interp::new();
        v.push(t.elapsed().as_secs_f64() * 1e3);
        if i == 0 {
            println!("live allocs {} bytes {} total allocs {}", LIVE_N.load(Relaxed) - n0, LIVE_B.load(Relaxed) - b0, TOTAL.load(Relaxed) - t0);
            keep = Some(it);
        }
    }
    drop(keep);
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!("Interp::new ms: n={} min {:.3} median {:.3} first-run(cold) see below", n, v[0], v[n / 2]);
}
