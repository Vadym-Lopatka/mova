//! `mova.nx/tap-*`: nx metrics edge taps (nx METRICS.md). One native call per edge, nothing else in Mova.
//! A fixed, preallocated buffer of small slots: no allocation per tap, never blocks (try-lock, a few spins, else the
//! tap is dropped and counted), drops when full (counted). A slot holds keys only: never a message or document text.
//! A method is stored as an index into the closed set `METHODS` (anything else is "other"), so a slot does not keep
//! a string of the message alive.
//! Main drains only when traffic is quiet: it swaps the buffer with a spare one (O(1) under the lock) and takes the
//! slots in chunks, building Mova values off the lock. Nothing wakes main because a buffer fills up.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use super::r#async::chan_try_put;
use crate::error::RjError;
use crate::eval::Interp;
use crate::value::{Chan, Keyword, Str, Symbol, Value};

const CAP: usize = 65536; // slots per edge (reserved, not touched: pages are used only as far as a batch reaches)
const START_CAP: usize = 1024; // slots reserved at tap-init! (startup is a few dozen taps); main reserves CAP at its first drain
const SHRINK: usize = 256; // a buffer that held more is freed after its drain (gives the touched pages back)
const HEAD: usize = 96; // bytes of a raw frame looked at

// slot kinds (first element of a drained entry)
const IN: u8 = 0; // reader: request / notification / client response
const OUT: u8 = 1; // writer: reply map or server notification/request
const OUT_RAW_BAD: u8 = 2; // writer: raw frame whose head was not understood

// outcomes
const O_NONE: u8 = 0;
const O_OK: u8 = 1;
const O_NULL: u8 = 2;
const O_ERROR: u8 = 3;
const O_CANCELLED: u8 = 4;

/// The closed method set (nx.core.latency/known-ops without the pipeline names). Index 0 = any other method.
const METHODS: [&str; 62] = [
    "other",
    "textDocument/didChange",
    "textDocument/didOpen",
    "textDocument/didClose",
    "workspace/executeCommand",
    "initialize",
    "textDocument/publishDiagnostics",
    "textDocument/completion",
    "textDocument/hover",
    "textDocument/definition",
    "textDocument/references",
    "textDocument/documentHighlight",
    "textDocument/codeAction",
    "textDocument/codeLens",
    "textDocument/semanticTokens/full",
    "textDocument/semanticTokens/range",
    "textDocument/signatureHelp",
    "textDocument/documentSymbol",
    "textDocument/foldingRange",
    "textDocument/selectionRange",
    "textDocument/linkedEditingRange",
    "textDocument/declaration",
    "textDocument/implementation",
    "textDocument/formatting",
    "textDocument/rangeFormatting",
    "textDocument/prepareCallHierarchy",
    "textDocument/prepareRename",
    "textDocument/rename",
    "textDocument/didSave",
    "callHierarchy/incomingCalls",
    "callHierarchy/outgoingCalls",
    "clojure/clojuredocs/raw",
    "clojure/cursorInfo/raw",
    "clojure/cursorInfo/log",
    "clojure/dependencyContents",
    "clojure/serverInfo/raw",
    "clojure/serverInfo/log",
    "clojure/workspace/projectTree/nodes",
    "codeLens/resolve",
    "completionItem/resolve",
    "shutdown",
    "workspace/symbol",
    "workspace/willRenameFiles",
    "nx/ping",
    "initialized",
    "exit",
    "$/cancelRequest",
    "$/setTrace",
    "workspace/didChangeWatchedFiles",
    "workspace/didChangeConfiguration",
    "workspace/didChangeWorkspaceFolders",
    "window/workDoneProgress/cancel",
    "$/progress",
    "window/showMessage",
    "window/showDocument",
    "window/showMessageRequest",
    "window/workDoneProgress/create",
    "workspace/applyEdit",
    "client/registerCapability",
    "workspace/configuration",
    "window/logMessage",
    "client/unregisterCapability",
];
const M_OTHER: u8 = 0;
const M_DID_CHANGE: u8 = 1;
const M_DID_OPEN: u8 = 2;
const M_DID_CLOSE: u8 = 3;
const M_EXEC: u8 = 4;
const M_INIT: u8 = 5;
const M_PUBLISH: u8 = 6;
const M_NONE: u8 = 255; // the message has no method (a reply)
const M_PROGRESS: u8 = method_const("$/progress");

/// Index of `name` in `METHODS`, at compile time (a name that is not there does not compile).
const fn method_const(name: &str) -> u8 {
    let (n, mut i) = (name.as_bytes(), 0);
    while i < METHODS.len() {
        let m = METHODS[i].as_bytes();
        let mut j = 0;
        while j < m.len() && j < n.len() && m[j] == n[j] {
            j += 1;
        }
        if j == m.len() && j == n.len() {
            return i as u8;
        }
        i += 1;
    }
    panic!("not in METHODS")
}

#[inline]
fn fnv(b: &[u8]) -> u64 {
    b.iter().fold(0xcbf29ce484222325u64, |h, c| (h ^ *c as u64).wrapping_mul(0x100000001b3))
}

/// Index of a method in `METHODS` (0 = other): one hash, one compare. Open addressing over 256 cells.
#[inline]
fn method_ix(m: &str) -> u8 {
    static T: OnceLock<[u8; 256]> = OnceLock::new();
    let t = T.get_or_init(|| {
        let mut t = [M_NONE; 256];
        for (i, name) in METHODS.iter().enumerate().skip(1) {
            let mut h = fnv(name.as_bytes()) as usize & 255;
            while t[h] != M_NONE {
                h = (h + 1) & 255;
            }
            t[h] = i as u8;
        }
        t
    });
    let mut h = fnv(m.as_bytes()) as usize & 255;
    loop {
        match t[h] {
            M_NONE => return M_OTHER,
            i if METHODS[i as usize] == m => return i,
            _ => h = (h + 1) & 255,
        }
    }
}

struct Slot {
    t: i64,
    id: Value, // Int, small Str (refcount) or Nil
    method: u8, // index into METHODS, or M_NONE
    key: Option<Str>, // uri (didOpen/didChange/didClose, publishDiagnostics), command (executeCommand), rootUri (initialize), "end" ($/progress end)
    len: u32,
    kind: u8,
    outcome: u8,
}

struct Tap {
    /// One buffer per edge (0 = reader, 1 = writer): an edge only ever meets main's swap on its lock.
    buf: [Mutex<Vec<Slot>>; 2],
    spare: Mutex<[Vec<Slot>; 2]>,
    /// Swapped out, in clock order, not yet handed to main (main only).
    pending: Mutex<std::vec::IntoIter<Slot>>,
    /// The last swap was big: once it is folded, main gives the freed pages back to the OS.
    big: AtomicBool,
    /// Main has reserved CAP slots in all four buffers (its first drain).
    grown: AtomicBool,
    /// 0 = main parked on the bell | 1 = main in its batch timer, no tap since the last tick | 2 = marked.
    flag: AtomicU8,
    lost_in: AtomicU64,
    lost_out: AtomicU64,
    /// Reader thread only: clock stamp taken by `tap-read-bytes`, consumed by `tap-in!`.
    t0: AtomicI64,
    bell: Arc<Chan>,
    /// `tap-init!` time: nx.main is running (startup stamp `:main-us`).
    init: std::time::Instant,
}

static TAP: OnceLock<Tap> = OnceLock::new();

struct Keys {
    id: Value,
    method: Value,
    params: Value,
    text_document: Value,
    uri: Value,
    result: Value,
    error: Value,
    code: Value,
    raw: Value,
    command: Value,
    root_uri: Value,
    value: Value,
    kind: Value,
}

fn keys() -> &'static Keys {
    static K: OnceLock<Keys> = OnceLock::new();
    let k = |n: &str| Value::Keyword(Keyword::construct(n));
    K.get_or_init(|| Keys {
        id: k("id"),
        method: k("method"),
        params: k("params"),
        text_document: k("textDocument"),
        uri: k("uri"),
        result: k("result"),
        error: k("error"),
        code: k("code"),
        raw: k("nx/raw"),
        command: k("command"),
        root_uri: k("rootUri"),
        value: k("value"),
        kind: k("kind"),
    })
}

fn get<'a>(m: &'a Value, k: &Value) -> Option<&'a Value> {
    match m {
        Value::Map(m) => m.get(k),
        _ => None,
    }
}

fn get_str(m: &Value, k: &Value) -> Option<Str> {
    match get(m, k) {
        Some(Value::Str(s)) => Some(s.clone()),
        _ => None,
    }
}

/// Only an int or a string is kept as an id (a refcount for a string; ids are small).
fn id_of(m: &Value, k: &Keys) -> Value {
    match get(m, &k.id) {
        Some(v @ (Value::Int(_) | Value::Str(_))) => v.clone(),
        _ => Value::Nil,
    }
}

#[inline]
fn ring(t: &Tap) {
    let _ = chan_try_put(&t.bell, Value::Int(0));
}

/// Never blocks: a few try-locks, then the tap is dropped (counted). The lock is only ever held for a push or a swap.
#[inline]
fn push(t: &Tap, s: Slot, out: bool) {
    let mut n = usize::MAX;
    for _ in 0..64 {
        if let Ok(mut g) = t.buf[out as usize].try_lock() {
            if g.len() < CAP {
                g.push(s);
                n = g.len();
            } else {
                n = 0;
            }
            break;
        }
        std::hint::spin_loop();
    }
    if n == 0 || n == usize::MAX {
        if out { &t.lost_out } else { &t.lost_in }.fetch_add(1, Ordering::Relaxed);
        if n == usize::MAX {
            return;
        }
    }
    // Coalesced bell: a tap marks the tick; it rings only on the parked -> marked transition.
    if t.flag.load(Ordering::Relaxed) != 2 && t.flag.swap(2, Ordering::AcqRel) == 0 {
        ring(t);
    }
}

fn slot_in(t0: i64, len: u32, msg: &Value) -> Slot {
    let k = keys();
    let method = match get(msg, &k.method) {
        Some(Value::Str(m)) => method_ix(m.as_ref()),
        _ => M_NONE,
    };
    let key = match method {
        M_DID_CHANGE | M_DID_OPEN | M_DID_CLOSE => {
            get(msg, &k.params).and_then(|p| get(p, &k.text_document)).and_then(|d| get_str(d, &k.uri))
        }
        M_EXEC => get(msg, &k.params).and_then(|p| get_str(p, &k.command)),
        M_INIT => get(msg, &k.params).and_then(|p| get_str(p, &k.root_uri)),
        _ => None,
    };
    Slot { t: t0, id: id_of(msg, k), method, key, len, kind: IN, outcome: O_NONE }
}

const RAW_PRE: &[u8] = b"{\"jsonrpc\":\"2.0\",\"id\":"; // head of a native reply frame (fixed in nx.rs)

/// (id, outcome) from the head of a native reply frame `{"jsonrpc":"2.0","id":..,"result"|"error":..`.
fn raw_head(h: &[u8]) -> Option<(Value, u8)> {
    let r = h.strip_prefix(RAW_PRE)?;
    let (id, rest) = if r.first() == Some(&b'"') {
        let mut j = 1;
        let mut esc = false;
        loop {
            match *r.get(j)? {
                b'"' => break,
                b'\\' => {
                    esc = true;
                    j += 2;
                }
                _ => j += 1,
            }
        }
        let tok = std::str::from_utf8(&r[..=j]).ok()?;
        let s = if esc { serde_json::from_str::<String>(tok).ok()? } else { tok[1..j].to_string() };
        (Value::Str(Str::from(s)), &r[j + 1..]) // string ids are rare: this path allocates
    } else {
        let neg = r.first() == Some(&b'-');
        let d0 = neg as usize;
        let mut j = d0;
        let mut n: i64 = 0;
        while let Some(c @ b'0'..=b'9') = r.get(j) {
            n = n.checked_mul(10)?.checked_add((c - b'0') as i64)?;
            j += 1;
        }
        if j == d0 {
            return None;
        }
        (Value::Int(if neg { -n } else { n }), &r[j..])
    };
    let outcome = if let Some(x) = rest.strip_prefix(b",\"result\":") {
        if x == b"null}" { O_NULL } else { O_OK }
    } else if let Some(x) = rest.strip_prefix(b",\"error\":") {
        match x.strip_prefix(b"{\"code\":-32800") {
            Some(y) if !matches!(y.first(), Some(b'0'..=b'9')) => O_CANCELLED,
            _ => O_ERROR,
        }
    } else {
        return None;
    };
    Some((id, outcome))
}

fn slot_raw(t: i64, frame: &Str) -> Slot {
    // A frame of 64 KB or more is a rope: take its head without flattening it.
    let short;
    let s: &str = if frame.as_rope().is_some() {
        short = frame.char_slice(0..HEAD);
        short.as_ref()
    } else {
        frame.as_ref()
    };
    let b = s.as_bytes();
    match raw_head(&b[..b.len().min(HEAD)]) {
        Some((id, outcome)) => Slot { t, id, method: M_NONE, key: None, len: 0, kind: OUT, outcome },
        None => Slot { t, id: Value::Nil, method: M_NONE, key: None, len: 0, kind: OUT_RAW_BAD, outcome: O_NONE },
    }
}

fn slot_out(t: i64, msg: &Value) -> Slot {
    let k = keys();
    if let Some(Value::Str(frame)) = get(msg, &k.raw) {
        return slot_raw(t, frame);
    }
    if let Some(Value::Str(m)) = get(msg, &k.method) {
        let method = method_ix(m.as_ref());
        let key = match method {
            M_PUBLISH => get(msg, &k.params).and_then(|p| get_str(p, &k.uri)),
            // the end of a work-done progress (the project pass is over for the client): startup stamp
            M_PROGRESS => get(msg, &k.params).and_then(|p| get(p, &k.value)).and_then(|v| get_str(v, &k.kind)).filter(|s| s.as_ref() as &str == "end"),
            _ => None,
        };
        return Slot { t, id: Value::Nil, method, key, len: 0, kind: OUT, outcome: O_NONE };
    }
    let outcome = match get(msg, &k.error) {
        Some(e) if !matches!(e, Value::Nil) => {
            if matches!(get(e, &k.code), Some(Value::Int(-32800))) { O_CANCELLED } else { O_ERROR }
        }
        _ => match get(msg, &k.result) {
            None | Some(Value::Nil) => O_NULL,
            _ => O_OK,
        },
    };
    Slot { t, id: id_of(msg, k), method: M_NONE, key: None, len: 0, kind: OUT, outcome }
}

fn entry(s: Slot, ok: &[Value; 5], names: &[Value]) -> Value {
    let st = |o: Option<Str>| o.map(Value::Str).unwrap_or(Value::Nil);
    Value::Vector(
        [Value::Int(s.kind as i64), Value::Int(s.t), Value::Int(s.len as i64), s.id, names.get(s.method as usize).cloned().unwrap_or(Value::Nil), st(s.key), ok[s.outcome as usize].clone()]
            .into_iter()
            .collect(),
    )
}

/// `METHODS` as Mova strings (built once; main only).
fn names() -> &'static [Value] {
    static N: OnceLock<Vec<Value>> = OnceLock::new();
    N.get_or_init(|| METHODS.iter().map(|m| Value::Str(Str::from(*m))).collect())
}

#[track_caller] // the source index records the call site of each registration
fn def(i: &mut Interp, name: &str, f: impl Fn(&mut Interp, &[Value]) -> Result<Value, RjError> + Send + Sync + 'static) {
    let native = crate::value::NativeFn::new(name, f);
    i.globals.set_builtin(Symbol { ns: Some(Str::from("mova.nx")), name: Str::from(name) }, Value::Native(Arc::new(native)));
}

fn tap(op: &str) -> Result<&'static Tap, RjError> {
    TAP.get().ok_or_else(|| RjError::type_err(format!("mova.nx/{op}: tap-init! was not called")))
}

/// Swaps both edge buffers out and returns their slots as one list in clock order. A buffer that grew big is freed.
fn swap_out(t: &Tap) -> Vec<Slot> {
    let mut sp = t.spare.lock().unwrap_or_else(|e| e.into_inner());
    for e in 0..2 {
        let mut g = t.buf[e].lock().unwrap_or_else(|e| e.into_inner());
        std::mem::swap(&mut *g, &mut sp[e]);
    }
    let mut all: Vec<Slot> = Vec::with_capacity(sp[0].len() + sp[1].len());
    let first = !t.grown.swap(true, Ordering::Relaxed);
    for (e, b) in sp.iter_mut().enumerate() {
        let big = b.len() > SHRINK;
        all.append(b);
        if big {
            *b = Vec::with_capacity(CAP);
        } else if first {
            b.reserve(CAP); // the spare is empty here; the buffer that took the old spare is grown below
            let mut g = t.buf[e].lock().unwrap_or_else(|e| e.into_inner());
            let n = g.len();
            g.reserve(CAP - n);
        }
    }
    if all.len() > SHRINK {
        t.big.store(true, Ordering::Relaxed);
    }
    all.sort_by_key(|s| s.t); // t0 of a request is always before t1 of its reply
    all
}

fn kw(n: &str) -> Value {
    Value::Keyword(Keyword::construct(n))
}

fn kmap(pairs: Vec<(Value, Value)>) -> Value {
    Value::Map(pairs.into_iter().collect())
}

/// A stage-stats delta as nx.core.latency's `S`.
fn stage(s: &nx_core::met::StageSnap) -> Value {
    kmap(vec![
        (kw("count"), Value::Int(s.count as i64)),
        (kw("sum-us"), Value::Int(s.sum_us as i64)),
        (kw("max-us"), Value::Int(s.max_us as i64)),
        (kw("buckets"), Value::Vector(s.buckets.iter().map(|b| Value::Int(*b as i64)).collect())),
    ])
}

/// The value of a clock stamp on the taps' clock (`clock_nano_time`), given both clocks read now.
fn tap_ns(now_ns: i64, now: std::time::Instant, t: std::time::Instant) -> i64 {
    now_ns - now.saturating_duration_since(t).as_nanos() as i64
}

/// Start time of this process from the kernel (unix us): the fork of the launcher that later execs Mova.
#[cfg(target_os = "macos")]
fn kernel_start_us() -> Option<i64> {
    unsafe {
        let mut bi: libc::proc_bsdinfo = std::mem::zeroed();
        let sz = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        if libc::proc_pidinfo(libc::getpid(), libc::PROC_PIDTBSDINFO, 0, &mut bi as *mut _ as *mut libc::c_void, sz) != sz {
            return None;
        }
        Some(bi.pbi_start_tvsec as i64 * 1_000_000 + bi.pbi_start_tvusec as i64)
    }
}

#[cfg(not(target_os = "macos"))]
fn kernel_start_us() -> Option<i64> {
    None
}

fn small(v: Option<&Value>, op: &str) -> Result<u8, RjError> {
    match v {
        Some(Value::Int(n)) if (0..=2).contains(n) => Ok(*n as u8),
        _ => Err(RjError::type_err(format!("mova.nx/{op}: flag must be 0, 1 or 2"))),
    }
}

pub fn register(i: &mut Interp) {
    // (mova.nx/tap-init! bell) -> nil. bell = (chan (sliding-buffer 1)) main parks on; rung once per parked -> marked.
    def(i, "tap-init!", |_i, a| {
        let Some(Value::Channel(bell)) = a.first() else {
            return Err(RjError::type_err("mova.nx/tap-init!: expected a channel".to_string()));
        };
        keys();
        method_ix("");
        let _ = TAP.set(Tap {
            buf: [Mutex::new(Vec::with_capacity(START_CAP)), Mutex::new(Vec::with_capacity(START_CAP))],
            spare: Mutex::new([Vec::with_capacity(START_CAP), Vec::with_capacity(START_CAP)]),
            pending: Mutex::new(Vec::new().into_iter()),
            big: AtomicBool::new(false),
            grown: AtomicBool::new(false),
            flag: AtomicU8::new(0),
            lost_in: AtomicU64::new(0),
            lost_out: AtomicU64::new(0),
            t0: AtomicI64::new(0),
            bell: bell.clone(),
            init: std::time::Instant::now(),
        });
        nx_core::met::enable(); // worker jobs and the project pass record natively from here on
        Ok(Value::Nil)
    });
    // (mova.nx/tap-read-bytes in len): mova.io/read-bytes + one clock stamp (before the read) kept for tap-in!.
    def(i, "tap-read-bytes", |_i, a| {
        if let Some(t) = TAP.get() {
            t.t0.store(crate::clock::clock_nano_time(), Ordering::Relaxed);
        }
        super::io::read_bytes(a)
    });
    // (mova.nx/tap-in! len msg) -> nil. Reader thread, after the hand-off.
    def(i, "tap-in!", |_i, a| {
        if let (Some(t), [Value::Int(len), msg]) = (TAP.get(), a) {
            push(t, slot_in(t.t0.load(Ordering::Relaxed), *len as u32, msg), false);
        }
        Ok(Value::Nil)
    });
    // (mova.nx/tap-out! msg) -> nil. Writer, after the flush; the clock is read here.
    def(i, "tap-out!", |_i, a| {
        if let (Some(t), [msg]) = (TAP.get(), a) {
            push(t, slot_out(crate::clock::clock_nano_time(), msg), true);
        }
        Ok(Value::Nil)
    });
    // (mova.nx/tap-flag! to) -> old | (mova.nx/tap-flag! from to) -> true when it was `from`. Main only.
    def(i, "tap-flag!", |_i, a| {
        let t = tap("tap-flag!")?;
        if a.len() == 1 {
            return Ok(Value::Int(t.flag.swap(small(a.first(), "tap-flag!")?, Ordering::AcqRel) as i64));
        }
        let (from, to) = (small(a.first(), "tap-flag!")?, small(a.get(1), "tap-flag!")?);
        Ok(Value::Bool(t.flag.compare_exchange(from, to, Ordering::AcqRel, Ordering::Relaxed).is_ok()))
    });
    // (mova.nx/tap-drain! max) -> up to max of [[kind t len id method key outcome] ...] in clock order; empty = nothing
    // is pending. Main only, and only when traffic is quiet. kind 0 = in (t = t0, len = body bytes), 1 = out, 2 = out raw
    // frame not understood. key = uri | command | rootUri | "end" (of a $/progress). outcome nil | :ok :null :error :cancelled.
    def(i, "tap-drain!", |_i, a| {
        let t = tap("tap-drain!")?;
        let max = match a.first() {
            Some(Value::Int(n)) if *n > 0 => *n as usize,
            _ => return Err(RjError::type_err("mova.nx/tap-drain!: expected a positive int".to_string())),
        };
        let mut p = t.pending.lock().unwrap_or_else(|e| e.into_inner());
        if p.len() == 0 {
            *p = swap_out(t).into_iter();
        }
        let k = |n: &str| Value::Keyword(Keyword::construct(n));
        let ok = [Value::Nil, k("ok"), k("null"), k("error"), k("cancelled")];
        let names = names();
        let out = Value::Vector(p.by_ref().take(max).map(|s| entry(s, &ok, names)).collect());
        if p.len() == 0 {
            *p = Vec::new().into_iter(); // frees the drained list
        }
        drop(p);
        // The empty drain that ends a big batch: its garbage is main's; without this it stays in main's heap while parked.
        if matches!(&out, Value::Vector(v) if v.is_empty()) && t.big.swap(false, Ordering::Relaxed) {
            crate::memstat::collect();
        }
        Ok(out)
    });
    // (mova.nx/met-take!) -> nil when nothing is new, else {:jobs {"job/open"|"job/bg" {:count n :bytes n :stages
    // {:m/queue-us S :m/read-us S :m/parse-us S :m/analyze-us S}}} :passes [{:end-ns n :stages {:m/<span>-us us}
    // :n {<counter> n}}]}. Snapshot and reset of the native stats (nx_core::met): the caller merges the deltas.
    // Main only, when traffic is quiet. end-ns is on the taps' clock.
    def(i, "met-take!", |_i, _a| {
        use nx_core::met;
        let jobs: Vec<(Value, Value)> = met::take_jobs()
            .iter()
            .zip(met::JOB_KINDS)
            .filter(|(j, _)| !j.is_zero())
            .map(|(j, name)| {
                let stages = j.stages.iter().zip(met::JOB_STAGES).filter(|(s, _)| !s.is_zero()).map(|(s, n)| (kw(&format!("m/{n}-us")), stage(s))).collect();
                (Value::Str(Str::from(name)), kmap(vec![(kw("count"), Value::Int(j.count as i64)), (kw("bytes"), Value::Int(j.bytes as i64)), (kw("stages"), kmap(stages))]))
            })
            .collect();
        let (now, now_ns) = (std::time::Instant::now(), crate::clock::clock_nano_time());
        let passes: Vec<Value> = met::take_passes()
            .iter()
            .map(|p| {
                let spans = p.us.iter().zip(met::PASS_SPANS).map(|(us, n)| (kw(&format!("m/{n}-us")), Value::Int(*us as i64))).collect();
                let counts = p.n.iter().zip(met::PASS_COUNTS).map(|(v, n)| (kw(n), Value::Int(*v as i64))).collect();
                kmap(vec![(kw("end-ns"), Value::Int(tap_ns(now_ns, now, p.end.unwrap_or(now)))), (kw("stages"), kmap(spans)), (kw("n"), kmap(counts))])
            })
            .collect();
        if jobs.is_empty() && passes.is_empty() {
            return Ok(Value::Nil);
        }
        Ok(kmap(vec![(kw("jobs"), kmap(jobs)), (kw("passes"), Value::Vector(passes.into_iter().collect()))]))
    });
    // (mova.nx/met-base) -> {:ns n :up-us n :spawn-us n :main-us n :base :kernel|:native}: the base of the startup
    // stamps. ns = the taps' clock now; up-us = us since the process start; spawn-us / main-us = process start -> Mova
    // `main` entered / `tap-init!`. A tap stamp t is at up-us - (ns - t) / 1000. Process start = the kernel's start
    // time of this pid (:kernel; the launcher's fork), else the first native stamp (:native; spawn-us is then 0).
    def(i, "met-base", |_i, _a| {
        let t = tap("met-base")?;
        let (now, now_ns) = (std::time::Instant::now(), crate::clock::clock_nano_time());
        let wall = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_micros() as i64).unwrap_or(0);
        let t0 = crate::load_trace::PROC_T0.get().copied().unwrap_or(t.init);
        let since = |x: std::time::Instant| now.saturating_duration_since(x).as_micros() as i64;
        let (up, base) = match kernel_start_us() {
            Some(s) if wall >= s + since(t0) => (wall - s, "kernel"),
            _ => (since(t0), "native"),
        };
        Ok(kmap(vec![
            (kw("ns"), Value::Int(now_ns)),
            (kw("up-us"), Value::Int(up)),
            (kw("spawn-us"), Value::Int(up - since(t0))),
            (kw("main-us"), Value::Int(up - since(t.init))),
            (kw("base"), kw(base)),
        ]))
    });
    // (mova.nx/tap-methods) -> the closed method set of the taps (the first one stands for any other method).
    def(i, "tap-methods", |_i, _a| Ok(Value::Vector(names().iter().cloned().collect())));
    // (mova.nx/tap-pending) -> taps stored and not yet handed to main.
    def(i, "tap-pending", |_i, _a| {
        let t = tap("tap-pending")?;
        let mut n = t.pending.lock().unwrap_or_else(|e| e.into_inner()).len();
        for b in &t.buf {
            n += b.lock().unwrap_or_else(|e| e.into_inner()).len();
        }
        Ok(Value::Int(n as i64))
    });
    // (mova.nx/tap-lost) -> [lost-in lost-out]: taps dropped (buffer full or lock busy).
    def(i, "tap-lost", |_i, _a| {
        let t = tap("tap-lost")?;
        let n = |c: &AtomicU64| Value::Int(c.load(Ordering::Relaxed) as i64);
        Ok(Value::Vector([n(&t.lost_in), n(&t.lost_out)].into_iter().collect()))
    });
}
