//! The middleware lane (design 5.7): Mova-level middleware and handlers.
//!
//! Off unless `--middleware` / `--handler` is given. Then the router sends
//! every request (after the unknown-session check) to
//! [`MovaBackend::dispatch_slow`](mova_nrepl::Backend::dispatch_slow), which
//! queues it here. Nothing in this file runs on the IO thread except `submit`.
//!
//! ```text
//!  IO thread ── submit ──▶ lane queue ──▶ lane worker (own Interp fork)
//!                                           │ make msg (Mova map: :transport :session ...)
//!                                           │ (handler msg)   <- Mova middleware stack
//!                                           │     innermost: `native` = serialize msg,
//!                                           │       NativeLane::route with a *tap* Outbox
//!                                           ▼
//!                       tap queue ◀── native replies (any thread: eval output, value, done)
//!                           │
//!                           └─ the worker pumps it: for each reply, `(t/send transport reply)`
//!                              on the transport the innermost handler was given (so wrapping
//!                              transports of the middleware see every native reply), ending in
//!                              `send`: encode the map, queue it on the connection.
//! ```
//!
//! * The handler stack is built once, on the boot thread, before the boot
//!   latch opens (`build`): `nrepl.lane/build` requires the namespaces and
//!   composes the stack. A namespace that does not load is fatal at startup.
//! * A worker is busy from the request until its last reply (an eval holds
//!   one worker). Workers are added when all are busy; one stays idle.
//! * Replies of the `forward-system-output` op keep their worker until the
//!   connection's session ends (the tap never closes).
//! * `:session` in a message is an atom holding `{#'*ns* <ns symbol>}`, with
//!   the id as `:id` metadata; writes to it do not reach the session.

use super::backend::{Config, Core};
use crate::error::RjError;
use crate::eval::Interp;
use crate::value::{NativeFn, PMap, Str, Symbol, Value};
use mova_nrepl::bencode::{decode, parse_request, Parsed, Value as Bv};
use mova_nrepl::{NativeLane, OwnedRequest, Responder, Session, SessionId, Tap};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Condvar, Mutex, OnceLock};

/// One request waiting for a lane worker.
pub(crate) struct LaneJob {
    pub req: OwnedRequest,
    pub session: Option<Arc<Session>>,
    pub reply: Responder,
}

struct LaneState {
    jobs: VecDeque<LaneJob>,
    idle: usize,
}

/// The Mova functions a worker needs, found once at build time.
#[derive(Clone)]
pub(crate) struct Ready {
    handler: Value,
    make_msg: Value,
    send: Value,
}

pub(crate) struct Lane {
    st: Mutex<LaneState>,
    cv: Condvar,
    native: Arc<OnceLock<NativeLane>>,
    ready: OnceLock<Ready>,
    pub threads: Arc<AtomicUsize>,
}

impl Lane {
    pub(crate) fn new() -> Lane {
        Lane {
            st: Mutex::new(LaneState { jobs: VecDeque::new(), idle: 0 }),
            cv: Condvar::new(),
            native: Arc::new(OnceLock::new()),
            ready: OnceLock::new(),
            threads: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub(crate) fn attach(&self, n: NativeLane) {
        let _ = self.native.set(n);
    }

    /// Queues a request; starts a worker if all are busy. Never blocks.
    pub(crate) fn submit(&self, core: &Arc<Core>, job: LaneJob) {
        let spawn = {
            let mut st = self.st.lock().unwrap_or_else(|e| e.into_inner());
            st.jobs.push_back(job);
            let need = st.jobs.len() > st.idle;
            if !need {
                self.cv.notify_one();
            }
            need
        };
        if spawn {
            let core2 = core.clone();
            if let Err(e) = core.spawn_thread("mova-nrepl-mw", &self.threads, move || worker_main(core2)) {
                let jobs: Vec<LaneJob> = self.st.lock().unwrap_or_else(|e| e.into_inner()).jobs.drain(..).collect();
                for j in jobs {
                    j.reply.send(&[("err", mova_nrepl::V::Str(&format!("nREPL: cannot start a middleware thread: {e}\n")))]);
                    j.reply.send_status(&["done", "error"]);
                }
            }
        }
    }

    fn next(&self) -> Option<LaneJob> {
        let mut st = self.st.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(j) = st.jobs.pop_front() {
                return Some(j);
            }
            if st.idle >= 1 {
                return None;
            }
            st.idle += 1;
            st = self.cv.wait(st).unwrap_or_else(|e| e.into_inner());
            st.idle -= 1;
        }
    }
}

// ---------------------------------------------------------------------------
// build (boot thread)
// ---------------------------------------------------------------------------

/// Loads the Mova side and builds the handler. `Err` is the text to print
/// before the server exits.
pub(crate) fn build(interp: &mut Interp, cfg: &Config, lane: &Lane) -> Result<(), String> {
    interp.globals.set(Symbol::simple("requiring-resolve"), requiring_resolve());
    let span = crate::reader::Span { start: 0, end: 0 };
    let load = |interp: &mut Interp, ns: &str| -> Result<(), String> {
        let r = interp.require_ns(&Str::from(ns), span);
        interp.current_ns = Str::from(crate::ns::USER_NS);
        r.map(|_| ()).map_err(|e| e.message.to_string())
    };
    load(interp, "nrepl.lane")?;
    let find = |interp: &Interp, name: &str| interp.lookup_global(&super::lookup::parse_symbol(name)).ok_or_else(|| format!("{name} is not defined"));
    let build_fn = find(interp, "nrepl.lane/build")?;
    let make_msg = find(interp, "nrepl.lane/make-msg")?;
    let send = find(interp, "nrepl.transport/send")?;
    let handler_sym = cfg.handler.as_deref().map(|s| Value::Sym(super::lookup::parse_symbol(s))).unwrap_or(Value::Nil);
    let mws: crate::value::PVec = cfg.middleware.iter().map(|s| Value::Sym(super::lookup::parse_symbol(s))).collect();
    let native = native_fn(lane.native.clone());
    let handler = interp.call(&build_fn, &[native, handler_sym, Value::Vector(mws)]).map_err(|e| e.message.to_string())?;
    interp.current_ns = Str::from(crate::ns::USER_NS);
    let _ = lane.ready.set(Ready { handler, make_msg, send });
    Ok(())
}

/// `(requiring-resolve sym)` for plain-Clojure middleware.
fn requiring_resolve() -> Value {
    Value::Native(Arc::new(NativeFn::new("requiring-resolve", |interp, args| {
        let Some(Value::Sym(s)) = args.first() else {
            return Err(RjError::other("requiring-resolve: expected a symbol"));
        };
        if interp.lookup_global(s).is_none() {
            if let Some(ns) = s.ns.clone() {
                let r = interp.require_ns(&ns, crate::reader::Span { start: 0, end: 0 });
                interp.current_ns = Str::from(crate::ns::USER_NS);
                r?;
            }
        }
        // the var itself, as `resolve` gives it
        match interp.lookup_global(&Symbol::simple("resolve")) {
            Some(r) => interp.call(&r, &[Value::Sym(s.clone())]),
            None => Ok(Value::Nil),
        }
    })))
}

// ---------------------------------------------------------------------------
// per request
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct JobCtx {
    session: Option<Arc<Session>>,
    sid: SessionId,
    reply: Responder,
}

thread_local! {
    static JOB: RefCell<Option<JobCtx>> = const { RefCell::new(None) };
    static PENDING: RefCell<Vec<(Value, Tap)>> = const { RefCell::new(Vec::new()) };
}

fn kw(s: &str) -> Value {
    Value::Keyword(crate::keyword::Keyword::from(s))
}

/// `(native msg)`: the innermost handler. Serializes the message, runs the
/// native op with a tap for its replies, and queues the tap for the pump.
fn native_fn(lane: Arc<OnceLock<NativeLane>>) -> Value {
    Value::Native(Arc::new(NativeFn::new("nrepl-native", move |interp, args| {
        let msg = interp.realize_deep(args.first().unwrap_or(&Value::Nil))?;
        let msg = msg.unmeta().clone();
        let Value::Map(m) = &msg else {
            return Err(RjError::other("nREPL: a message must be a map"));
        };
        let transport = m.get(&kw("transport")).cloned().unwrap_or(Value::Nil);
        let bytes = write_message(&msg, &["transport", "session"]);
        let Some(lane) = lane.get() else {
            return Err(RjError::other("nREPL: the middleware lane is not attached"));
        };
        let Some(ctx) = JOB.with(|j| j.borrow().clone()) else {
            return Err(RjError::other("nREPL: the native handler was called outside a request"));
        };
        let Ok(Parsed::Message(req, _)) = parse_request(&bytes) else {
            return Err(RjError::other("nREPL: the message cannot be encoded (is :op or :id of a odd type?)"));
        };
        let (tob, tap) = ctx.reply.outbox().tap();
        let mut out = Vec::new();
        lane.route(&req, ctx.session.clone(), ctx.sid, &mut out, &tob);
        tap.push(&out);
        drop(tob);
        PENDING.with(|p| p.borrow_mut().push((transport, tap)));
        Ok(Value::Nil)
    })))
}

/// `(send-fn map)` of a request's own transport: encode and queue on its connection.
fn send_fn(reply: Responder) -> Value {
    Value::Native(Arc::new(NativeFn::new("nrepl-send", move |interp, args| {
        let v = interp.realize_deep(args.first().unwrap_or(&Value::Nil))?;
        let bytes = write_message(&v, &[]);
        reply.outbox().append(&[&bytes]);
        Ok(Value::Nil)
    })))
}

/// A message map as a bencode dict: keys by name, nil values and the
/// `skip` keys left out.
fn write_message(v: &Value, skip: &[&str]) -> Vec<u8> {
    let mut out = Vec::new();
    match v {
        Value::Map(m) => {
            let mut rows: Vec<(Vec<u8>, &Value)> = m
                .iter()
                .filter(|(_, v)| !matches!(v, Value::Nil))
                .map(|(k, v)| (super::lookup::key_bytes(k), v))
                .filter(|(k, _)| !skip.iter().any(|s| s.as_bytes() == &k[..]))
                .collect();
            rows.sort_by(|a, b| a.0.cmp(&b.0));
            out.push(b'd');
            for (k, v) in rows {
                mova_nrepl::bencode::write_bytes(&mut out, &k);
                match (&k[..], v.unmeta()) {
                    (b"status", Value::Set(set)) => write_status(set.iter(), &mut out),
                    _ => super::lookup::write_plain(v, &mut out),
                }
            }
            out.push(b'e');
        }
        other => super::lookup::write_plain(other, &mut out),
    }
    out
}

/// A `:status` set as a list. The JVM writes a hash set in its own order, and
/// the goldens fix that order for the statuses the server itself sends
/// (`mova_nrepl::status`): a set equal to one of those lists is written in
/// the list's order. Any other set is written sorted by name.
fn write_status<'a>(set: impl Iterator<Item = &'a Value>, out: &mut Vec<u8>) {
    use mova_nrepl::status as st;
    const KNOWN: &[&[&str]] = &[
        st::DONE,
        st::UNKNOWN_OP,
        st::UNKNOWN_SESSION,
        st::SESSION_CLOSED,
        st::NO_CODE,
        st::UNKNOWN_CODE_TYPE,
        st::NAMESPACE_NOT_FOUND,
        st::EVAL_ERROR,
        st::INTERRUPTED,
        st::SESSION_IDLE,
        st::INTERRUPT_ID_MISMATCH,
        st::SESSION_EPHEMERAL,
        st::NEED_INPUT,
        &["nrepl.middleware.print/truncated", "eval-error"],
        &["nrepl.middleware.print/truncated"],
    ];
    let mut names: Vec<String> = set
        .map(|v| match v.unmeta() {
            Value::Keyword(k) => k.text_ref().to_string(),
            Value::Str(s) => s.to_string(),
            other => crate::printer::pr_str(other),
        })
        .collect();
    names.sort();
    names.dedup();
    let known = KNOWN.iter().find(|l| l.len() == names.len() && l.iter().all(|n| names.iter().any(|m| m == n)));
    if let Some(l) = known {
        names = l.iter().map(|s| s.to_string()).collect();
    }
    out.push(b'l');
    for n in &names {
        mova_nrepl::bencode::write_bytes(out, n.as_bytes());
    }
    out.push(b'e');
}

/// A native reply as the middleware sees it: keyword keys, `:status` a set of keywords.
fn reply_map(b: &Bv) -> Value {
    let Bv::Dict(d) = b else { return super::print::to_mova(b) };
    let mut m = PMap::new();
    for (k, v) in d {
        let key = String::from_utf8_lossy(k).into_owned();
        let val = match (key.as_str(), v) {
            ("status", Bv::List(l)) => Value::Set(l.iter().filter_map(|e| e.as_str()).map(kw).collect()),
            ("ops", Bv::Dict(ops)) => ops_value(ops),
            _ => super::print::to_mova(v),
        };
        m.insert(kw(&key), val);
    }
    Value::Map(m)
}

/// `describe`'s `ops`: op names are strings, the doc fields keywords, and the
/// `requires` / `optional` / `returns` maps have string keys (as on the JVM).
fn ops_value(ops: &std::collections::BTreeMap<Vec<u8>, Bv>) -> Value {
    let text = |k: &[u8]| Value::Str(Str::from(String::from_utf8_lossy(k).as_ref()));
    let mut m = PMap::new();
    for (name, info) in ops {
        let mut im = PMap::new();
        if let Bv::Dict(fields) = info {
            for (f, v) in fields {
                let fv = match (v, f.as_slice()) {
                    (Bv::Dict(d), b"requires" | b"optional" | b"returns") => {
                        let mut dm = PMap::new();
                        for (k, x) in d {
                            dm.insert(text(k), super::print::to_mova(x));
                        }
                        Value::Map(dm)
                    }
                    _ => super::print::to_mova(v),
                };
                im.insert(kw(&String::from_utf8_lossy(f)), fv);
            }
        }
        m.insert(text(name), Value::Map(im));
    }
    Value::Map(m)
}

fn worker_main(core: Arc<Core>) {
    let lane = &core.lane;
    let Some((mut interp, _vars)) = core.boot.fork() else {
        while let Some(job) = lane.next() {
            job.reply.send(&[("err", mova_nrepl::V::Str("nREPL: the interpreter failed to start\n"))]);
            job.reply.send_status(&["done", "error"]);
        }
        return;
    };
    let Some(ready) = lane.ready.get().cloned() else { return };
    while let Some(job) = lane.next() {
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_job(&mut interp, &ready, job)));
        if r.is_err() {
            eprintln!("nrepl: panic in the middleware lane");
        }
        JOB.with(|j| *j.borrow_mut() = None);
        PENDING.with(|p| p.borrow_mut().clear());
    }
}

fn log_error(what: &str, e: &RjError) {
    eprintln!("ERROR: {what}: {}", e.message);
}

fn run_job(interp: &mut Interp, ready: &Ready, job: LaneJob) {
    let req = job.req.request();
    let Ok(Some((raw, _))) = decode(req.raw()) else { return };
    let sid = *job.reply.session_id();
    let sid_text = String::from_utf8_lossy(sid.as_bytes()).into_owned();
    let ns = job.session.as_ref().map(|s| s.ns()).unwrap_or_else(|| "user".into());
    let args = [super::print::to_mova(&raw), Value::Str(Str::from(sid_text.as_str())), Value::Str(Str::from(ns.as_str())), send_fn(job.reply.clone())];
    let msg = match interp.call(&ready.make_msg, &args) {
        Ok(m) => m,
        Err(e) => return log_error("cannot build the message", &e),
    };
    JOB.with(|j| *j.borrow_mut() = Some(JobCtx { session: job.session.clone(), sid, reply: job.reply.clone() }));
    if let Err(e) = interp.call(&ready.handler, &[msg]) {
        log_error("Unhandled REPL handler exception processing message", &e);
    }
    JOB.with(|j| *j.borrow_mut() = None);
    // Pump: every native reply goes through the transport the innermost handler got.
    loop {
        let next = PENDING.with(|p| {
            let mut p = p.borrow_mut();
            if p.is_empty() {
                None
            } else {
                Some(p.remove(0))
            }
        });
        let Some((transport, tap)) = next else { break };
        let mut buf = Vec::new();
        while tap.wait_take(&mut buf) {
            let mut pos = 0;
            while pos < buf.len() {
                let Ok(Some((v, used))) = decode(&buf[pos..]) else { break };
                pos += used;
                if let Err(e) = interp.call(&ready.send, &[transport.clone(), reply_map(&v)]) {
                    log_error("transport send failed", &e);
                }
            }
            buf.clear();
        }
    }
}
