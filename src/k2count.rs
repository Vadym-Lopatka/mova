//! K2 spike: diagnostic-only IR-node / call counters (`--features k2-count`).
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::Once;
pub static NODES: [AtomicU64; 40] = [const { AtomicU64::new(0) }; 40];
pub static CLJ_CALLS: AtomicU64 = AtomicU64::new(0);
pub static PROTO_CALLS: AtomicU64 = AtomicU64::new(0);
pub static NATIVE_CALLS: AtomicU64 = AtomicU64::new(0);
/// K7b: slot-frame pops: [pops, slots scanned, non-Nil, owning].
pub static POPS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
static INIT: Once = Once::new();
const NAMES: [&str; 32] = ["Const","LoadSlot","LoadSlotTake","LoadCapture","SelfRef","GlobalRef","CreationEnvLookup","If","Do","Let","Loop","NumLoop","MakeClosure","MakeRecGroup","SiblingRef","Try","Def","DynBind","Recur","Call","CallGlobal","CallCreationEnv","Intrinsic","VectorLit","SetLit","MapLit","Throw","Escape","FieldGet","SetMutField","KwCall","LocalCall"];
extern "C" fn dump() {
    let mut v: Vec<(u64, &str)> = (0..32).map(|i| (NODES[i].load(Relaxed), NAMES[i])).collect();
    v.sort_by(|a, b| b.0.cmp(&a.0));
    let tot: u64 = v.iter().map(|x| x.0).sum();
    eprintln!("K2COUNT nodes={} clj_calls={} native_calls={} proto_calls={}", tot, CLJ_CALLS.load(Relaxed), NATIVE_CALLS.load(Relaxed), PROTO_CALLS.load(Relaxed));
    for (n, s) in v.iter().filter(|x| x.0 > 0) { eprintln!("K2COUNT {s} {n}"); }
    eprintln!("K2COUNT pops={} slots={} live={} owning={}", POPS[0].load(Relaxed), POPS[1].load(Relaxed), POPS[2].load(Relaxed), POPS[3].load(Relaxed));
}
#[inline]
pub fn init() { INIT.call_once(|| unsafe { libc::atexit(dump); }); }
#[inline]
pub fn node(i: usize) { NODES[i].fetch_add(1, Relaxed); TOTAL.fetch_add(1, Relaxed); }
/// K5: running total of executed IR nodes (census deltas).
pub static TOTAL: AtomicU64 = AtomicU64::new(0);


/// K5 census: exclusive (nested census frames subtracted) nodes/ns per fallback kind.
pub mod census {
    use super::{TOTAL, Relaxed};
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::sync::{Mutex, Once};
    thread_local!(static STACK: RefCell<Vec<(u64, u64)>> = const { RefCell::new(Vec::new()) });
    static STATS: Mutex<BTreeMap<String, (u64, u64, u64)>> = Mutex::new(BTreeMap::new());
    static INIT: Once = Once::new();
    pub fn on() -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| std::env::var("MOVA_JIT_CENSUS").is_ok_and(|v| v == "1"))
    }
    extern "C" fn dump() {
        let st = STATS.lock().unwrap();
        let mut v: Vec<_> = st.iter().collect();
        v.sort_by(|a, b| b.1 .2.cmp(&a.1 .2));
        for (k, (c, n, ns)) in v.iter().take(40) {
            eprintln!("CENSUS\t{k}\t{c}\t{n}\t{ns}");
        }
    }
    pub struct Tok(u64, std::time::Instant);
    pub fn enter() -> Tok {
        INIT.call_once(|| unsafe { libc::atexit(dump); });
        STACK.with(|s| s.borrow_mut().push((0, 0)));
        Tok(TOTAL.load(Relaxed), std::time::Instant::now())
    }
    pub fn exit(t: Tok, kind: impl FnOnce() -> String) {
        let ns = t.1.elapsed().as_nanos() as u64;
        let n = TOTAL.load(Relaxed) - t.0;
        let (cn, cns) = STACK.with(|s| s.borrow_mut().pop().unwrap_or((0, 0)));
        STACK.with(|s| if let Some(p) = s.borrow_mut().last_mut() { p.0 += n; p.1 += ns; });
        let e = &mut *STATS.lock().unwrap();
        let r = e.entry(kind()).or_insert((0, 0, 0));
        r.0 += 1; r.1 += n.saturating_sub(cn); r.2 += ns.saturating_sub(cns);
    }
}
