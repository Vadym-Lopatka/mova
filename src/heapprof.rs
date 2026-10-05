//! M9 diagnostic only (`--features heap-prof`): sampling heap profiler wrapped
//! around mimalloc. 1 sample per ~SAMPLE_BYTES allocated per thread; each keeps
//! the frame-pointer stack and allocating thread's name until freed. With
//! MOVA_HEAPPROF=<dir>, a thread writes <dir>/peak.txt (max live sampled bytes)
//! and <dir>/last.txt every 200 ms: live bytes by (thread, stack), raw IPs +
//! image load address (resolve with atos). Never in the shipped binary.

use std::alloc::{GlobalAlloc, Layout};
use std::cell::Cell;
use std::collections::HashMap;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::Mutex;

const SAMPLE_BYTES: usize = 16 * 1024;
const DEPTH: usize = 28;
const SHARDS: usize = 64;
const BLOOM_BITS: usize = 1 << 22;

#[derive(Clone, Copy)]
struct Rec {
    weight: usize,
    name: [u8; 16],
    clj: [u8; 160],
    stack: [usize; DEPTH],
}

static ENABLED: AtomicBool = AtomicBool::new(false);
static STARTED: AtomicBool = AtomicBool::new(false);
static LIVE: AtomicU64 = AtomicU64::new(0);
static BLOOM: [AtomicU64; BLOOM_BITS / 64] = [const { AtomicU64::new(0) }; BLOOM_BITS / 64];
static MAPS: [Mutex<Option<HashMap<usize, Rec>>>; SHARDS] = [const { Mutex::new(None) }; SHARDS];
static TOTAL: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static IN_HOOK: Cell<bool> = const { Cell::new(false) };
    static COUNTER: Cell<usize> = const { Cell::new(0) };
}

pub struct Prof;

fn h(p: usize) -> usize { (p >> 4).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 20 }

#[inline(always)]
fn frames() -> [usize; DEPTH] {
    let mut out = [0usize; DEPTH];
    #[cfg(target_arch = "aarch64")]
    unsafe {
        let mut fp: usize;
        std::arch::asm!("mov {}, x29", out(reg) fp);
        let lo = fp;
        let mut i = 0;
        while i < DEPTH && fp != 0 && fp & 7 == 0 && fp >= lo && fp < lo + (600 << 20) {
            let next = *(fp as *const usize);
            let lr = *((fp + 8) as *const usize);
            if lr == 0 { break; }
            out[i] = lr;
            i += 1;
            if next <= fp { break; }
            fp = next;
        }
    }
    out
}

fn thread_name() -> [u8; 16] {
    let mut b = [0u8; 16];
    unsafe { libc::pthread_getname_np(libc::pthread_self(), b.as_mut_ptr() as *mut libc::c_char, 16) };
    b
}

impl Prof {
    #[inline(always)]
    fn maybe_sample(p: *mut u8, size: usize) {
        if p.is_null() || !ENABLED.load(Relaxed) { return; }
        let c = COUNTER.with(|c| { let n = c.get() + size; c.set(n); n });
        if c < SAMPLE_BYTES { return; }
        COUNTER.with(|c| c.set(0));
        if IN_HOOK.with(|g| g.replace(true)) { return; }
        let mut rec = Rec { weight: size.max(SAMPLE_BYTES), name: thread_name(), clj: [0; 160], stack: frames() };
        mova::profile::clj_frames(&mut rec.clj);
        let a = p as usize;
        let k = h(a);
        BLOOM[(k % BLOOM_BITS) / 64].fetch_or(1 << (k % 64), Relaxed);
        MAPS[k % SHARDS].lock().unwrap().get_or_insert_with(HashMap::new).insert(a, rec);
        LIVE.fetch_add(rec.weight as u64, Relaxed);
        TOTAL.fetch_add(1, Relaxed);
        if !STARTED.swap(true, Relaxed) { start_dumper(); }
        IN_HOOK.with(|g| g.set(false));
    }
    #[inline(always)]
    fn untrack(p: *mut u8) {
        if TOTAL.load(Relaxed) == 0 { return; }
        let a = p as usize;
        let k = h(a);
        if BLOOM[(k % BLOOM_BITS) / 64].load(Relaxed) & (1 << (k % 64)) == 0 { return; }
        if IN_HOOK.with(|g| g.replace(true)) { return; }
        if let Some(r) = MAPS[k % SHARDS].lock().unwrap().as_mut().and_then(|m| m.remove(&a)) {
            LIVE.fetch_sub(r.weight as u64, Relaxed);
        }
        IN_HOOK.with(|g| g.set(false));
    }
}

unsafe impl GlobalAlloc for Prof {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { mimalloc::MiMalloc.alloc(l) };
        Self::maybe_sample(p, l.size());
        p
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        let p = unsafe { mimalloc::MiMalloc.alloc_zeroed(l) };
        Self::maybe_sample(p, l.size());
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        Self::untrack(p);
        unsafe { mimalloc::MiMalloc.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        Self::untrack(p);
        let q = unsafe { mimalloc::MiMalloc.realloc(p, l, n) };
        Self::maybe_sample(q, n);
        q
    }
}

pub fn init() {
    if std::env::var_os("MOVA_HEAPPROF").is_some() { ENABLED.store(true, Relaxed); }
}

fn start_dumper() {
    std::thread::Builder::new().name("heapprof".into()).spawn(|| {
        let dir = std::env::var("MOVA_HEAPPROF").unwrap();
        let mut peak = 0u64;
        let t0 = std::time::Instant::now();
        let mut series = std::fs::File::create(format!("{dir}/series.txt")).unwrap();
        loop {
            std::thread::sleep(std::time::Duration::from_millis(100));
            IN_HOOK.with(|g| g.set(true));
            let live = LIVE.load(Relaxed);
            let _ = writeln!(series, "{} {}", t0.elapsed().as_millis(), live / 1_000_000);
            let text = snapshot(live);
            if live > peak { peak = live; let _ = std::fs::write(format!("{dir}/peak.txt"), &text); }
            let _ = std::fs::write(format!("{dir}/last.txt"), &text);
            let ms = t0.elapsed().as_millis() / 500;
            let _ = std::fs::write(format!("{dir}/at{ms}.txt"), &text);
        }
    }).unwrap();
}

extern "C" fn visit_ab(_h: *const u8, _a: *const u8, b: *mut u8, _s: usize, arg: *mut u8) -> bool {
    if !b.is_null() { unsafe { (*(arg as *mut std::collections::HashSet<usize>)).insert(b as usize) }; }
    true
}

unsafe extern "C" {
    fn mi_heap_visit_abandoned_blocks(heap: *mut u8, visit_blocks: bool, f: extern "C" fn(*const u8, *const u8, *mut u8, usize, *mut u8) -> bool, arg: *mut u8) -> bool;
}

fn snapshot(live: u64) -> String {
    let only_ab = std::env::var_os("MOVA_HEAPPROF_AB").is_some();
    let mut ab: std::collections::HashSet<usize> = std::collections::HashSet::new();
    if only_ab { unsafe { mi_heap_visit_abandoned_blocks(std::ptr::null_mut(), true, visit_ab, &mut ab as *mut _ as *mut u8) }; }
    let mut agg: HashMap<(String, [usize; DEPTH]), (usize, usize)> = HashMap::new();
    for s in MAPS.iter() {
        let v: Vec<(usize, Rec)> = s.lock().unwrap().as_ref().map(|m| m.iter().map(|(a, r)| (*a, *r)).collect()).unwrap_or_default();
        for (a, r) in v {
            if only_ab && !ab.contains(&a) { continue; }
            let n = String::from_utf8_lossy(&r.name).trim_end_matches('\0').to_string();
            let n = n.trim_end_matches(|c: char| c.is_ascii_digit()).to_string();
            let n = format!("{n} [{}]", String::from_utf8_lossy(&r.clj).trim_end_matches('\0').replace(' ', "_"));
            let e = agg.entry((n, r.stack)).or_default();
            e.0 += r.weight;
            e.1 += 1;
        }
    }
    let mut v: Vec<_> = agg.into_iter().collect();
    v.sort_by(|a, b| b.1 .0.cmp(&a.1 .0));
    let mut o = Vec::new();
    let base = unsafe { _dyld_get_image_header(0) } as usize;
    let _ = writeln!(o, "live {live} base {base:#x} t {:?}", std::time::SystemTime::now());
    for ((n, st), (b, c)) in v.iter().take(4000) {
        let _ = write!(o, "{b} {c} {n} |");
        for ip in st.iter().take_while(|x| **x != 0) { let _ = write!(o, " {ip:#x}"); }
        let _ = writeln!(o);
    }
    String::from_utf8(o).unwrap()
}

unsafe extern "C" {
    fn _dyld_get_image_header(i: u32) -> *const u8;
}
