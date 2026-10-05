//! M10 regression: champ's per-thread node pool has no Drop, so every Mova-spawned
//! thread (future, Thread., async/thread, flow procs) used to leak its parked nodes
//! at exit (~90 KB per short future; MBs per clj-kondo worker, ~50 MB end footprint
//! on a clojure-lsp session). The spawn sites now wrap bodies in `memstat::drained`.
//! A counting global allocator measures bytes still allocated after N warm futures.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicIsize, Ordering::Relaxed};

use mova::internal::Interp;

struct Counting;
static LIVE: AtomicIsize = AtomicIsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        LIVE.fetch_add(l.size() as isize, Relaxed);
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        LIVE.fetch_sub(l.size() as isize, Relaxed);
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        LIVE.fetch_add(n as isize - l.size() as isize, Relaxed);
        unsafe { System.realloc(p, l, n) }
    }
}

#[global_allocator]
static A: Counting = Counting;

fn per_thread_growth(src: &str) -> isize {
    let mut interp = Interp::new();
    let run = format!("(dotimes [_ 20] {src})");
    for _ in 0..3 {
        interp.eval_str("warm", &run).unwrap();
    }
    std::thread::sleep(std::time::Duration::from_millis(100));
    let before = LIVE.load(Relaxed);
    for _ in 0..5 {
        interp.eval_str("probe", &run).unwrap();
    }
    std::thread::sleep(std::time::Duration::from_millis(100));
    (LIVE.load(Relaxed) - before) / 100
}

#[test]
fn future_threads_do_not_leak_champ_pool() {
    let g = per_thread_growth("@(future (count (into {} (map (fn [i] [i (vec (range 20))]) (range 2000)))))");
    assert!(g < 65536, "future leaked {g} bytes per thread (champ pool not drained at thread exit?)");
}

#[test]
fn raw_threads_do_not_leak_champ_pool() {
    let g = per_thread_growth("(let [t (Thread. (fn [] (count (into {} (map (fn [i] [i (vec (range 20))]) (range 2000))))))] (.start t) (.join t))");
    assert!(g < 65536, "Thread. leaked {g} bytes per thread (champ pool not drained at thread exit?)");
}

