//! The `Backend` the `mova` binary hands to the wire server.

use super::bindings::Vars;
use super::eval::{EvalJob, ReadCond};
pub use super::eval::ErrorMode;
use super::session_thread::{Pool, SessionHandle};
use crate::eval::Interp;
use crate::value::{Str, Symbol, Value};
use mova_nrepl::{status, Backend, Call, Session, V};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};

/// Server settings that reach the interpreter.
#[derive(Clone, Debug)]
pub struct Config {
    /// Stack of every eval thread. The interpreter is a recursive tree-walker;
    /// this is only address space, a thread pays for the pages it touches.
    pub stack_size: usize,
    /// `Interp::max_depth`: the mova call-depth guard (`stack overflow` error).
    pub max_depth: usize,
    /// Where `(require ...)` looks for namespace files.
    pub module_paths: Vec<PathBuf>,
    /// What `err` holds for an error: JVM text only, or text plus Mova's report.
    pub errors: ErrorMode,
    /// `--middleware`: namespace-qualified symbols of Mova-level middleware vars.
    pub middleware: Vec<String>,
    /// `--handler`: symbol of a Mova-level handler var.
    pub handler: Option<String>,
    /// Called (then the process exits 1) when the middleware lane cannot start.
    pub fatal_hook: Option<fn()>,
}

impl Default for Config {
    fn default() -> Config {
        Config { stack_size: 512 * 1024 * 1024, max_depth: 10_000, module_paths: vec![PathBuf::from(".")], errors: ErrorMode::Rich, middleware: Vec::new(), handler: None, fatal_hook: None }
    }
}

enum BootState {
    Pending,
    Ready { base: Interp, vars: Arc<Vars> },
    Failed,
}

/// The boot latch: the first eval waits on it, nothing else does.
pub(crate) struct Boot {
    st: Mutex<BootState>,
    cv: Condvar,
}

impl Boot {
    /// A sibling of the base interpreter plus the resolved session vars, or
    /// `None` if boot failed. Blocks until boot is done.
    pub(crate) fn fork(&self) -> Option<(Interp, Arc<Vars>)> {
        let mut st = self.st.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            match &*st {
                BootState::Pending => st = self.cv.wait(st).unwrap_or_else(|e| e.into_inner()),
                BootState::Ready { base, vars } => return Some((base.fork(), vars.clone())),
                BootState::Failed => return None,
            }
        }
    }
}

/// State shared by the IO thread and every eval thread.
pub(crate) struct Core {
    pub boot: Boot,
    pub pool: Pool,
    pub cfg: Config,
    /// Live session threads / ephemeral workers (for tests and `--verbose`).
    pub session_threads: Arc<AtomicUsize>,
    pub pool_threads: Arc<AtomicUsize>,
    /// The `completions` / `lookup` worker (at most one thread).
    pub tooling: super::tooling::Tooling,
    pub tool_threads: Arc<AtomicUsize>,
    /// The middleware lane (design 5.7). Idle and empty unless configured.
    pub lane: super::middleware::Lane,
}

/// Counts a thread while it lives.
struct Live(Arc<AtomicUsize>);
impl Drop for Live {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Core {
    /// Starts an eval thread. Tries the configured stack, then 64 MiB (a
    /// process with strict overcommit may refuse hundreds of MiB).
    pub(crate) fn spawn_thread(
        &self,
        name: &str,
        counter: &Arc<AtomicUsize>,
        f: impl FnOnce() + Send + 'static,
    ) -> std::io::Result<std::thread::JoinHandle<()>> {
        let f = std::sync::Arc::new(Mutex::new(Some(f)));
        let mut last = None;
        for size in [self.cfg.stack_size, 64 * 1024 * 1024] {
            let f2 = f.clone();
            counter.fetch_add(1, Ordering::SeqCst);
            let live = Live(counter.clone());
            let r = std::thread::Builder::new().name(name.into()).stack_size(size).spawn(crate::memstat::drained(move || {
                let _live = live;
                let job = f2.lock().unwrap_or_else(|e| e.into_inner()).take();
                if let Some(job) = job {
                    job();
                }
            }));
            match r {
                Ok(h) => return Ok(h),
                Err(e) => last = Some(e),
            }
        }
        Err(last.unwrap_or_else(|| std::io::Error::other("thread spawn failed")))
    }
}

/// nREPL backend that runs Mova code. See the module docs in `nrepl/mod.rs`.
pub struct MovaBackend {
    core: Arc<Core>,
}

impl MovaBackend {
    pub fn new(cfg: Config) -> Arc<MovaBackend> {
        Arc::new(MovaBackend {
            core: Arc::new(Core {
                boot: Boot { st: Mutex::new(BootState::Pending), cv: Condvar::new() },
                pool: Pool::new(),
                cfg,
                session_threads: Arc::new(AtomicUsize::new(0)),
                pool_threads: Arc::new(AtomicUsize::new(0)),
                tooling: super::tooling::Tooling::new(),
                tool_threads: Arc::new(AtomicUsize::new(0)),
                lane: super::middleware::Lane::new(),
            }),
        })
    }

    /// Builds the base interpreter on the calling thread and opens the latch.
    /// Call it on a thread with a big stack (`boot_thread` does). Evals that
    /// arrive earlier wait for it.
    pub fn boot_blocking(&self) {
        let cfg = &self.core.cfg;
        let booted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut interp = Interp::with_capabilities(crate::builtins::Capabilities::ALL, Some(cfg.max_depth));
            interp.module_paths = cfg.module_paths.clone();
            // the JIT's native loops have no interrupt check: keep them off for sessions
            interp.intr_armed = true;
            // Vars a client may ask for that `core.mova` leaves undefined.
            for (name, v) in [("*file*", "NO_SOURCE_PATH"), ("*source-path*", "NO_SOURCE_FILE")] {
                let sym = Symbol::simple(name);
                if interp.globals.find_any_cell(&sym).is_none() {
                    interp.globals.set(sym, Value::Str(Str::from(v)));
                }
            }
            let vars = Arc::new(Vars::resolve(&interp));
            // Mova-level middleware / handler: load and compose before anyone is served.
            if self.lane_on() {
                if let Err(e) = super::middleware::build(&mut interp, cfg, &self.core.lane) {
                    eprintln!("nREPL: could not load the middleware: {e}");
                    if let Some(h) = cfg.fatal_hook {
                        h();
                    }
                    std::process::exit(1);
                }
            }
            (interp, vars)
        }));
        let mut st = self.core.boot.st.lock().unwrap_or_else(|e| e.into_inner());
        *st = match booted {
            Ok((base, vars)) => BootState::Ready { base, vars },
            Err(_) => BootState::Failed,
        };
        self.core.boot.cv.notify_all();
    }

    fn lane_on(&self) -> bool {
        !self.core.cfg.middleware.is_empty() || self.core.cfg.handler.is_some()
    }

    /// Middleware-lane worker threads alive now.
    pub fn lane_threads(&self) -> usize {
        self.core.lane.threads.load(Ordering::SeqCst)
    }

    /// Starts the boot thread (stack as configured). The caller may start the
    /// IO loop at once; the first eval waits for the latch.
    pub fn boot_thread(
        self: &Arc<Self>,
        after: impl FnOnce() + Send + 'static,
    ) -> std::io::Result<std::thread::JoinHandle<()>> {
        let me = self.clone();
        let counter = Arc::new(AtomicUsize::new(0));
        self.core.spawn_thread("mova-nrepl-boot", &counter, move || {
            me.boot_blocking();
            after();
        })
    }

    /// Session threads alive now (made by the first eval of a session, ended by `close`).
    pub fn session_threads(&self) -> usize {
        self.core.session_threads.load(Ordering::SeqCst)
    }

    /// Ephemeral-request workers alive now (at rest: at most one).
    pub fn pool_threads(&self) -> usize {
        self.core.pool_threads.load(Ordering::SeqCst)
    }

    /// `eval` and `load-file`: copy the request off the IO thread and queue it.
    fn dispatch_eval(&self, call: Call<'_>, load_file: bool) {
        let req = call.req;
        let reply = call.reply;
        let content = if load_file { req.file() } else { req.code() };
        let Some(content) = content else {
            // load-file without `file` (eval without `code` was answered by the router)
            reply.send_status(status::NO_CODE);
            return;
        };
        let Some(code) = content.as_bytes() else {
            reply.send_status(status::UNKNOWN_CODE_TYPE);
            return;
        };
        // `line` / `column` that are not ints: the JVM throws before it answers,
        // so the request gets no reply at all. Same here.
        let num = |v: Option<mova_nrepl::Val<'_>>| -> Result<usize, ()> {
            match v {
                None => Ok(1),
                Some(v) => v.as_int().map(|n| n.max(0) as usize).ok_or(()),
            }
        };
        let (Ok(line), Ok(column)) = (num(req.line()), num(req.column())) else {
            return;
        };
        let text = |v: Option<mova_nrepl::Val<'_>>| v.and_then(|v| v.as_bytes()).map(|b| String::from_utf8_lossy(b).into_owned());
        let read_cond = match req.read_cond().and_then(|v| v.as_str()) {
            None | Some("allow") => ReadCond::Allow,
            Some("preserve") => ReadCond::Preserve,
            Some(_) => ReadCond::Refuse,
        };
        // a `keys` option that is not a list: the JVM never answers
        let Ok(print) = super::print::parse(req) else {
            return;
        };
        let (file, file_name, line, column) = if load_file {
            (text(req.file_path()), text(req.file_name()), 1, 1)
        } else {
            (text(req.file()), text(req.file_name()), line, column)
        };
        let job = EvalJob {
            code: String::from_utf8_lossy(code).into_owned(),
            ns: text(req.ns()),
            file,
            file_name,
            line,
            column,
            read_cond,
            errors: self.core.cfg.errors,
            load_file,
            print,
            out_limit: req.get(b"out-limit").and_then(|v| v.as_int()).filter(|n| *n > 0).map(|n| n as usize),
            eval_fn: text(req.eval()),
            reply,
        };
        match call.session {
            Some(session) => {
                let handle = session_handle(&session);
                handle.submit(&self.core, &session, job);
            }
            None => self.core.pool.submit(&self.core, job),
        }
    }


    /// `completions` and `lookup`: copy the request and queue it for the tooling worker.
    fn dispatch_tool(&self, call: Call<'_>, completions: bool) {
        let req = call.req;
        let text = |v: Option<mova_nrepl::Val<'_>>| v.and_then(|v| v.as_bytes()).map(|b| String::from_utf8_lossy(b).into_owned());
        let ns = text(req.ns());
        let job = super::tooling::ToolJob {
            req: if completions {
                super::tooling::ToolReq::Completions {
                    prefix: text(req.prefix()),
                    ns,
                    complete_fn: text(req.complete_fn()),
                    options: req.options().and_then(|o| mova_nrepl::bencode::decode(o.raw()).ok().flatten()).map(|(v, _)| v),
                }
            } else {
                super::tooling::ToolReq::Lookup { sym: text(req.sym()), ns, lookup_fn: text(req.lookup_fn()) }
            },
            session_ns: call.session.as_ref().map(|s| s.ns()).unwrap_or_else(|| "user".into()),
            reply: call.reply,
        };
        self.core.tooling.submit(&self.core, job);
    }

    /// Tooling worker threads alive now (at most one).
    pub fn tool_threads(&self) -> usize {
        self.core.tool_threads.load(Ordering::SeqCst)
    }
}

/// The session's handle, made on first use (a session that was not cloned
/// from another one has none yet).
fn session_handle(session: &Arc<Session>) -> &SessionHandle {
    let slot = session.slot.get_or_init(|| Box::new(SessionHandle::new(None)));
    slot.downcast_ref::<SessionHandle>().expect("session slot holds a SessionHandle")
}

impl Backend for MovaBackend {
    fn dispatch(&self, call: Call<'_>) {
        match call.req.op().and_then(|o| o.as_str()) {
            Some("eval") => self.dispatch_eval(call, false),
            Some("load-file") => self.dispatch_eval(call, true),
            Some("interrupt") => match &call.session {
                // no session: an ephemeral request has nothing to interrupt
                None => {
                    call.reply.send_status(status::SESSION_EPHEMERAL);
                }
                Some(s) => {
                    let id = call.req.interrupt_id().and_then(|v| v.as_bytes());
                    session_handle(s).interrupt(id, &call.reply);
                }
            },
            Some("stdin") => {
                if let Some(s) = &call.session {
                    let q = session_handle(s).stdin().clone();
                    match call.req.stdin().and_then(|v| v.as_bytes()) {
                        Some(b) if !b.is_empty() => q.add(b),
                        _ => q.add_eof(),
                    }
                }
                call.reply.send_status(status::DONE);
            }
            Some("forward-system-output") => {
                // no session: nothing to register, but the op still answers `done`
                if let Some(s) = &call.session {
                    super::forward::register(s.id, call.reply.clone());
                }
                call.reply.send_status(status::DONE);
            }
            Some("completions") => self.dispatch_tool(call, true),
            Some("lookup") => self.dispatch_tool(call, false),
            // Not built yet (P5): answer as an unknown op so the client does not hang.
            _ => {
                let echo = V::Raw(call.req.op().map(|o| o.raw()).unwrap_or(b"le"));
                call.reply.send(&[("op", echo), ("status", V::Strs(status::UNKNOWN_OP))]);
            }
        }
    }

    fn middleware_lane(&self) -> bool {
        self.lane_on()
    }

    fn dispatch_slow(&self, call: Call<'_>) {
        let job = super::middleware::LaneJob { req: call.req.to_owned_request(), session: call.session, reply: call.reply };
        self.core.lane.submit(&self.core, job);
    }

    fn attach_lane(&self, lane: mova_nrepl::NativeLane) {
        self.core.lane.attach(lane);
    }

    fn session_cloned(&self, parent: Option<&Arc<Session>>, child: &Arc<Session>) {
        // Copy the parent's last published bindings (not `*ns*`: the child starts in `user`).
        let snap = parent.and_then(|p| p.slot.get()).and_then(|s| s.downcast_ref::<SessionHandle>()).and_then(|h| h.snapshot());
        let _ = child.slot.set(Box::new(SessionHandle::new(snap)));
    }

    fn session_closed(&self, session: &Arc<Session>) {
        super::forward::unregister(&session.id);
        if let Some(h) = session.slot.get().and_then(|s| s.downcast_ref::<SessionHandle>()) {
            h.close();
        }
    }
}
