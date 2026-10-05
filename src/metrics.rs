//! LSP_METRICS_SOCK channel: JSON datagrams to an external collector. Off (unset) = one cached branch.
use crate::value::Value;
use std::cell::Cell;
use std::os::unix::net::UnixDatagram;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

pub const ASSOC_UNIQUE: usize = 0;
pub const ASSOC_SHARED: usize = 1;
pub const CONJ_UNIQUE: usize = 2;
pub const CONJ_SHARED: usize = 3;
/// Counter families for [`count`]: base slot (unique = base, shared = base + 1).
pub const ASSOC: usize = ASSOC_UNIQUE;
pub const CONJ: usize = CONJ_UNIQUE;
const MAX_DGRAM: usize = 16 * 1024;

static ON: OnceLock<bool> = OnceLock::new();
static COUNTING: AtomicBool = AtomicBool::new(false);
static SOCK: OnceLock<Option<UnixDatagram>> = OnceLock::new();
static DROPPED: AtomicU64 = AtomicU64::new(0);
static EPOCH: OnceLock<Instant> = OnceLock::new();

/// Cache-line padded per-thread counters, summed by the sampler.
#[repr(align(64))]
struct Ctr([AtomicU64; 4]);
static ALL: Mutex<Vec<Arc<Ctr>>> = Mutex::new(Vec::new());
thread_local! { static LOCAL: Cell<Option<&'static Ctr>> = const { Cell::new(None) }; }

/// True iff `LSP_METRICS_SOCK` is set (read once); first call also starts the sampler.
#[inline]
pub fn enabled() -> bool {
    match ON.get() {
        Some(b) => *b,
        None => init(),
    }
}

#[cold]
fn init() -> bool {
    *ON.get_or_init(|| {
        let on = std::env::var_os("LSP_METRICS_SOCK").is_some_and(|v| !v.is_empty());
        if on {
            let _ = EPOCH.set(Instant::now());
            COUNTING.store(true, Relaxed);
            let _ = std::thread::Builder::new().name("mova-metrics".into()).spawn(sampler);
        }
        on
    })
}

/// Monotonic ns since first use.
pub fn now_ns() -> i64 {
    EPOCH.get_or_init(Instant::now).elapsed().as_nanos() as i64
}

/// Hot path: bump `slot` (unique when `unique`, else the next slot). Plain load+store on a thread-owned atomic.
#[inline(always)]
pub fn count(base: usize, unique: bool) {
    if COUNTING.load(Relaxed) {
        count_slow(base + (!unique) as usize);
    }
}

#[cold]
#[inline(never)]
fn count_slow(slot: usize) {
    let _ = LOCAL.try_with(|l| {
        let c = match l.get() {
            Some(c) => c,
            None => {
                let a = Arc::new(Ctr(Default::default()));
                let p: &'static Ctr = unsafe { &*Arc::as_ptr(&a) }; // kept alive forever by ALL
                if let Ok(mut g) = ALL.lock() {
                    g.push(a);
                }
                l.set(Some(p));
                p
            }
        };
        let a = &c.0[slot];
        a.store(a.load(Relaxed).wrapping_add(1), Relaxed);
    });
}

fn totals() -> [u64; 4] {
    let mut t = [0u64; 4];
    if let Ok(g) = ALL.lock() {
        for c in g.iter() {
            for (i, a) in c.0.iter().enumerate() {
                t[i] = t[i].wrapping_add(a.load(Relaxed));
            }
        }
    }
    t
}

fn sock() -> Option<&'static UnixDatagram> {
    SOCK.get_or_init(|| {
        let path = std::env::var_os("LSP_METRICS_SOCK")?;
        let s = UnixDatagram::unbound().ok()?;
        s.connect(path).ok()?;
        s.set_nonblocking(true).ok()?;
        Some(s)
    })
    .as_ref()
}

/// Sends one datagram; any failure drops and counts.
fn send(kind: &str, name: &str, dur_ns: Option<i64>, attrs: serde_json::Map<String, serde_json::Value>) {
    let mut o = serde_json::Map::new();
    o.insert("kind".into(), kind.into());
    o.insert("name".into(), name.into());
    if let Some(d) = dur_ns {
        o.insert("dur_ns".into(), d.into());
    }
    o.insert("attrs".into(), serde_json::Value::Object(attrs));
    let ok = serde_json::to_vec(&serde_json::Value::Object(o))
        .ok()
        .filter(|b| b.len() <= MAX_DGRAM)
        .and_then(|b| sock().and_then(|s| s.send(&b).ok()))
        .is_some();
    if !ok {
        DROPPED.fetch_add(1, Relaxed);
    }
}

fn key_text(v: &Value) -> Option<String> {
    match v {
        Value::Str(s) => Some(s.as_ref().to_string()),
        Value::Keyword(k) => Some(k.text().to_string()),
        _ => None,
    }
}

fn attrs_of(v: Option<&Value>) -> serde_json::Map<String, serde_json::Value> {
    use serde_json::Value as J;
    let mut out = serde_json::Map::new();
    if let Some(Value::Map(m)) = v {
        for (k, val) in m.iter() {
            let Some(k) = key_text(k) else { continue };
            let j = match val {
                Value::Int(i) => J::from(*i),
                Value::Float(f) => match serde_json::Number::from_f64(*f) {
                    Some(n) => J::Number(n),
                    None => continue,
                },
                Value::Str(s) => J::String(s.as_ref().to_string()),
                Value::Bool(b) => J::Bool(*b),
                Value::Keyword(kw) => J::String(kw.text().to_string()),
                _ => continue,
            };
            out.insert(k, j);
        }
    }
    out
}

/// Native entry: `kind` = "span" | "event" | "sample".
pub(crate) fn emit_native(kind: &str, args: &[Value]) {
    if !enabled() {
        return;
    }
    let (Some(name), span) = (args.first().and_then(key_text), kind == "span") else { return };
    let (dur, attrs) = if span {
        let Some(Value::Int(t0)) = args.get(1) else { return };
        (Some(now_ns() - *t0), args.get(2))
    } else {
        (None, args.get(1))
    };
    send(kind, &name, dur, attrs_of(attrs));
}

/// `mova.startup` event (no-op when off).
pub fn startup(attrs: Vec<(&str, serde_json::Value)>) {
    if enabled() {
        send("event", "mova.startup", None, attrs.into_iter().map(|(k, v)| (k.to_string(), v)).collect());
    }
}

fn sampler() {
    let mut last: Vec<u64> = Vec::new();
    loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
        let t = totals();
        let jit = crate::jit::stats();
        let cur = vec![t[0], t[1], t[2], t[3], jit.0, jit.1, jit.2, DROPPED.load(Relaxed)];
        if cur == last {
            continue;
        }
        let names = ["assoc.unique", "assoc.shared", "conj.unique", "conj.shared", "jit.compiled", "jit.compile_ns", "jit.aot_bound", "dropped"];
        let attrs = names.iter().zip(&cur).map(|(n, v)| (n.to_string(), serde_json::Value::from(*v))).collect();
        last = cur;
        send("sample", "mova.runtime", None, attrs);
    }
}

pub(crate) fn register(i: &mut crate::eval::Interp) {
    use crate::value::{NativeFn, Symbol};
    // macro (not closure) so #[track_caller] sees each registration line
    macro_rules! reg {
        ($name:expr, $f:expr) => {{
            let f: fn(&mut crate::eval::Interp, &[Value]) -> Result<Value, crate::error::RjError> = $f;
            i.globals.set_builtin(
                Symbol { ns: Some("mova.metrics".into()), name: $name.into() },
                Value::Native(Arc::new(NativeFn::new($name, f))),
            );
        }};
    }
    reg!("enabled?", |_, _| Ok(Value::Bool(enabled())));
    reg!("now-ns", |_, _| Ok(Value::Int(now_ns())));
    reg!("span!", |_, a| { emit_native("span", a); Ok(Value::Nil) });
    reg!("event!", |_, a| { emit_native("event", a); Ok(Value::Nil) });
    reg!("sample!", |_, a| { emit_native("sample", a); Ok(Value::Nil) });
    enabled();
}
