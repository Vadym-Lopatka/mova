//! The `completions` and `lookup` ops (design 5.6).
//!
//! One tooling worker thread, started on the first request, owns a forked
//! interpreter. The IO thread only copies the request and queues it; the
//! worker answers. Tooling does not wait behind a running eval of a session
//! (the JVM answers these on its handler thread too), and an idle server has
//! no extra cost: the worker sleeps on a condvar.
//!
//! The work itself is native (`completion.rs`, `lookup.rs`): no Mova code runs
//! unless the request names a `complete-fn` / `lookup-fn`.

use super::backend::Core;
use super::completion::{self, Cand};
use super::lookup;
use crate::eval::Interp;
use crate::keyword::Keyword;
use crate::value::{PMap, PVec, Str, Symbol, Value};
use mova_nrepl::bencode::{write_bytes, write_int};
use mova_nrepl::{status, Responder, V};
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};

const COMPLETIONS_ERROR: &[&str] = &["done", "completions-error", "error"];
const LOOKUP_ERROR: &[&str] = &["done", "error", "lookup-error"];

/// What a client sent, copied off the IO thread.
pub(crate) enum ToolReq {
    Completions {
        prefix: Option<String>,
        ns: Option<String>,
        complete_fn: Option<String>,
        options: Option<mova_nrepl::bencode::Value>,
    },
    Lookup {
        sym: Option<String>,
        ns: Option<String>,
        lookup_fn: Option<String>,
    },
}

pub(crate) struct ToolJob {
    pub req: ToolReq,
    /// The session's `*ns*` when the request has no `ns`.
    pub session_ns: String,
    pub reply: Responder,
}

struct State {
    jobs: VecDeque<ToolJob>,
    started: bool,
}

pub(crate) struct Tooling {
    st: Mutex<State>,
    cv: Condvar,
}

impl Tooling {
    pub(crate) fn new() -> Tooling {
        Tooling { st: Mutex::new(State { jobs: VecDeque::new(), started: false }), cv: Condvar::new() }
    }

    /// Queues a job; starts the worker on the first one. Never blocks.
    pub(crate) fn submit(&self, core: &Arc<Core>, job: ToolJob) {
        let spawn = {
            let mut st = self.st.lock().unwrap_or_else(|e| e.into_inner());
            st.jobs.push_back(job);
            let first = !st.started;
            st.started = true;
            if !first {
                self.cv.notify_one();
            }
            first
        };
        if spawn {
            let core2 = core.clone();
            if let Err(e) = core.spawn_thread("mova-nrepl-tooling", &core.tool_threads, move || worker_main(core2)) {
                let jobs: Vec<ToolJob> = self.st.lock().unwrap_or_else(|e| e.into_inner()).jobs.drain(..).collect();
                for j in jobs {
                    fail(&j, &format!("nREPL: cannot start the tooling thread: {e}"));
                }
            }
        }
    }

    fn next(&self) -> ToolJob {
        let mut st = self.st.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(j) = st.jobs.pop_front() {
                return j;
            }
            st = self.cv.wait(st).unwrap_or_else(|e| e.into_inner());
        }
    }
}

fn error_status(job: &ToolJob) -> &'static [&'static str] {
    match job.req {
        ToolReq::Completions { .. } => COMPLETIONS_ERROR,
        ToolReq::Lookup { .. } => LOOKUP_ERROR,
    }
}

fn fail(job: &ToolJob, message: &str) {
    job.reply.send(&[("message", V::Str(message)), ("status", V::Strs(error_status(job)))]);
}

fn worker_main(core: Arc<Core>) {
    let Some((mut interp, vars)) = core.boot.fork() else {
        loop {
            let job = core.tooling.next();
            fail(&job, "nREPL: the interpreter failed to start");
        }
    };
    // user fns run with the default session bindings
    vars.push_session(Some(&vars.defaults()), super::input::empty_stream());
    loop {
        let job = core.tooling.next();
        let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handle(&mut interp, &job)));
        if ran.is_err() {
            fail(&job, "nREPL: internal error");
        }
    }
}

fn handle(interp: &mut Interp, job: &ToolJob) {
    match &job.req {
        ToolReq::Completions { prefix, ns, complete_fn, options } => {
            let ns = ns.clone().unwrap_or_else(|| job.session_ns.clone());
            // `complete-fn`: a fully qualified Mova fn symbol; one that does not resolve is ignored
            if let Some(f) = complete_fn.as_deref().and_then(|s| resolve_fn(interp, s)) {
                let opts = options.as_ref().map(options_value).unwrap_or_else(|| Value::Map(PMap::new()));
                let args = [prefix.clone().map(|p| Value::Str(Str::from(p))).unwrap_or(Value::Nil), Value::Sym(Symbol::simple(ns.as_str())), opts];
                return reply_user(interp, job, &f, &args, "completions");
            }
            let Some(prefix) = prefix else {
                // what the JVM says when `prefix` is missing (a NullPointerException message)
                return fail(job, "Cannot invoke \"Object.toString()\" because \"s\" is null");
            };
            let cands = completion::completions(interp, prefix, &ns);
            let mut list = Vec::with_capacity(cands.len() * 96 + 2);
            list.push(b'l');
            for c in &cands {
                write_cand(c, &mut list);
            }
            list.push(b'e');
            job.reply.send(&[("completions", V::Raw(&list)), ("status", V::Strs(status::DONE))]);
        }
        ToolReq::Lookup { sym, ns, lookup_fn } => {
            let ns = ns.clone().unwrap_or_else(|| job.session_ns.clone());
            if let Some(f) = lookup_fn.as_deref().and_then(|s| resolve_fn(interp, s)) {
                let Some(sym) = sym else { return fail(job, "no conversion to symbol") };
                let args = [Value::Sym(Symbol::simple(ns.as_str())), Value::Sym(lookup::parse_symbol(sym))];
                return reply_user(interp, job, &f, &args, "info");
            }
            let Some(sym) = sym else { return fail(job, "no conversion to symbol") };
            match lookup::lookup(interp, &ns, sym) {
                Ok(Some(info)) => {
                    job.reply.send(&[("info", V::Raw(&info)), ("status", V::Strs(status::DONE))]);
                }
                // no such symbol: `info` is an empty list (nil on the JVM)
                Ok(None) => {
                    job.reply.send(&[("info", V::Raw(b"le")), ("status", V::Strs(status::DONE))]);
                }
                Err(msg) => fail(job, &msg),
            }
        }
    }
}

/// Calls a user fn and sends its (realized) result under `key`.
fn reply_user(interp: &mut Interp, job: &ToolJob, f: &Value, args: &[Value], key: &str) {
    let result = interp.call(f, args).and_then(|v| interp.realize_deep(&v));
    match result {
        Ok(v) => {
            let mut b = Vec::new();
            lookup::write_plain(&v, &mut b);
            job.reply.send(&[(key, V::Raw(&b)), ("status", V::Strs(status::DONE))]);
        }
        Err(e) => fail(job, &e.message),
    }
}

/// `(requiring-resolve sym)`: loads the namespace if needed. `None` when it
/// does not resolve to a fn (the request then uses the built-in).
fn resolve_fn(interp: &mut Interp, s: &str) -> Option<Value> {
    let sym = lookup::parse_symbol(s);
    let ns = sym.ns.clone()?;
    if interp.lookup_global(&sym).is_none() {
        let _ = interp.require_ns(&ns, crate::reader::Span { start: 0, end: 0 });
    }
    interp.current_ns = Str::from(crate::ns::USER_NS);
    let v = interp.globals.get_exact(&sym).or_else(|| interp.lookup_global(&sym))?;
    matches!(v, Value::Fn(_) | Value::Native(_)).then_some(v)
}

/// The `options` map as the JVM passes it to a completion fn: keys are
/// keywords, and `:extra-metadata` is a set of keywords.
fn options_value(v: &mova_nrepl::bencode::Value) -> Value {
    use mova_nrepl::bencode::Value as B;
    match v {
        B::Dict(d) => {
            let mut m = PMap::new();
            for (k, x) in d {
                let key = String::from_utf8_lossy(k).into_owned();
                let val = if key == "extra-metadata" {
                    match x {
                        B::List(l) => Value::Set(l.iter().filter_map(|e| e.as_str()).map(|s| Value::Keyword(Keyword::from(s))).collect()),
                        other => options_value(other),
                    }
                } else {
                    options_value(x)
                };
                m.insert(Value::Keyword(Keyword::from(key.as_str())), val);
            }
            Value::Map(m)
        }
        B::List(l) => Value::Vector(l.iter().map(options_value).collect::<PVec>()),
        B::Int(n) => Value::Int(*n),
        B::Bytes(b) => Value::Str(Str::from(String::from_utf8_lossy(b).as_ref())),
    }
}

/// One candidate as a bencode dict (keys in sorted order).
fn write_cand(c: &Cand, out: &mut Vec<u8>) {
    out.push(b'd');
    let mut put = |k: &str, v: &str| {
        write_bytes(out, k.as_bytes());
        write_bytes(out, v.as_bytes());
    };
    put("candidate", &c.candidate);
    if let Some(f) = &c.file {
        put("file", f);
    }
    if let Some(n) = &c.ns {
        put("ns", n);
    }
    if let Some(p) = &c.package {
        put("package", p);
    }
    if c.priority {
        write_bytes(out, b"priority");
        write_int(out, 0);
    }
    write_bytes(out, b"type");
    write_bytes(out, c.typ.as_bytes());
    out.push(b'e');
}
