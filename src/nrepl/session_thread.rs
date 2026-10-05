//! Session threads and the ephemeral worker pool.
//!
//! * A **session** has a FIFO of jobs and, once it has run an eval, one OS
//!   thread (made on the first `submit`). The thread owns the session's
//!   bindings. It waits on a condvar when idle (no polling, no timer) and ends
//!   when the session is closed.
//! * An **ephemeral** request (no `session`) goes to a small pool. A worker
//!   takes a job, pushes a fresh binding frame, runs it, pops the frame. The
//!   pool starts a new worker when every worker is busy (an ephemeral
//!   `(Thread/sleep 10000)` must not block another client), and a worker that
//!   finds the queue empty ends unless it is the one kept idle. So the cost
//!   of "no session" is zero threads at rest apart from that one.

use super::backend::Core;
use super::bindings::Snapshot;
use super::eval::{self, Ctl, EvalJob};
use super::input::StdinQueue;
use crate::value::Value;
use crate::interrupt::Interrupt;
use mova_nrepl::{status, Responder, Session, V};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Workers that stay alive when the queue is empty.
const KEEP_IDLE_WORKERS: usize = 1;


// ---------------------------------------------------------------------------
// persistent sessions
// ---------------------------------------------------------------------------

struct Queue {
    jobs: VecDeque<EvalJob>,
    started: bool,
    closed: bool,
}

/// The eval that runs now (for `interrupt`).
struct Running {
    /// The request `id`, raw bencode (what `Responder::id_raw` gives).
    id: Option<Box<[u8]>>,
    /// Where the eval's replies go (its connection, id and session).
    reply: Responder,
}

#[derive(Default)]
struct RunState {
    /// Counts evals; an escalation timer belongs to one of them.
    seq: u64,
    running: Option<Running>,
}

pub(crate) struct SessionShared {
    q: Mutex<Queue>,
    cv: Condvar,
    /// The bindings as of the last finished eval. `clone` reads this on the IO
    /// thread: a short lock, never the session thread's state.
    published: Mutex<Option<Snapshot>>,
    /// The thread's interrupt flag.
    intr: OnceLock<Arc<Interrupt>>,
    run: Mutex<RunState>,
    /// Signalled when the running eval is over (the escalation timer waits on it).
    run_cv: Condvar,
    /// True from an accepted `interrupt` until the next eval starts: the
    /// interrupted eval sends no `done` (the interrupt reply was its `done`).
    interrupted: AtomicBool,
    /// `*in*` of the session.
    pub(crate) stdin: Arc<StdinQueue>,
}

impl SessionShared {
    pub(crate) fn was_interrupted(&self) -> bool {
        self.interrupted.load(Ordering::SeqCst)
    }

    /// The running eval is over for `interrupt` purposes.
    pub(crate) fn settle(&self) {
        let mut r = self.run.lock().unwrap_or_else(|e| e.into_inner());
        r.running = None;
        self.run_cv.notify_all();
    }

    fn begin(&self, job: &EvalJob) {
        self.interrupted.store(false, Ordering::SeqCst);
        let mut r = self.run.lock().unwrap_or_else(|e| e.into_inner());
        r.seq += 1;
        r.running = Some(Running { id: job.reply.id_raw().map(|b| b.into()), reply: job.reply.clone() });
        drop(r);
        self.stdin.set_reply(Some(job.reply.clone()));
    }
}

/// What a `Session::slot` holds.
pub(crate) struct SessionHandle {
    shared: Arc<SessionShared>,
}

impl SessionHandle {
    pub(crate) fn new(initial: Option<Snapshot>) -> SessionHandle {
        SessionHandle {
            shared: Arc::new(SessionShared {
                q: Mutex::new(Queue { jobs: VecDeque::new(), started: false, closed: false }),
                cv: Condvar::new(),
                published: Mutex::new(initial),
                intr: OnceLock::new(),
                run: Mutex::new(RunState::default()),
                run_cv: Condvar::new(),
                interrupted: AtomicBool::new(false),
                stdin: StdinQueue::new(),
            }),
        }
    }

    /// The bindings to copy into a clone (`None`: the session never had any).
    pub(crate) fn snapshot(&self) -> Option<Snapshot> {
        self.shared.published.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Queues a job; makes the thread on the first one. Never blocks.
    pub(crate) fn submit(&self, core: &Arc<Core>, session: &Arc<Session>, job: EvalJob) {
        let spawn = {
            let mut q = self.shared.q.lock().unwrap_or_else(|e| e.into_inner());
            if q.closed {
                return;
            }
            q.jobs.push_back(job);
            let first = !q.started;
            q.started = true;
            if !first {
                self.shared.cv.notify_one();
            }
            first
        };
        if spawn {
            let (core2, shared, session2) = (core.clone(), self.shared.clone(), session.clone());
            let r = core.spawn_thread("mova-nrepl-session", &core.session_threads, move || session_main(core2, shared, session2));
            if let Err(e) = r {
                // Cannot start a thread: fail every queued job so no client waits.
                let jobs: Vec<EvalJob> = {
                    let mut q = self.shared.q.lock().unwrap_or_else(|e| e.into_inner());
                    q.closed = true;
                    q.jobs.drain(..).collect()
                };
                for j in jobs {
                    fail(&j, &format!("nREPL: cannot start the session thread: {e}\n"));
                }
            }
        }
    }

    /// Ends the thread. An eval that is running is interrupted (hard) and sends
    /// no more replies. Queued jobs are dropped.
    pub(crate) fn close(&self) {
        {
            let mut q = self.shared.q.lock().unwrap_or_else(|e| e.into_inner());
            q.closed = true;
            q.jobs.clear();
            self.shared.cv.notify_all();
        }
        let r = self.shared.run.lock().unwrap_or_else(|e| e.into_inner());
        if let (Some(running), Some(intr)) = (&r.running, self.shared.intr.get()) {
            self.shared.interrupted.store(true, Ordering::SeqCst);
            intr.hard();
            running.reply.outbox().wake_blocked_senders();
        }
        // a read blocked on `*in*` wakes through the interrupt; a closed idle
        // thread wakes through the queue condvar above
    }

    pub(crate) fn stdin(&self) -> &Arc<StdinQueue> {
        &self.shared.stdin
    }

    /// The `interrupt` op (design 5.4). `interrupt_id` is the raw `interrupt-id`
    /// value (the payload of the bencode string), `reply` answers the request.
    pub(crate) fn interrupt(&self, interrupt_id: Option<&[u8]>, reply: &Responder) {
        let sh = &self.shared;
        let mut r = sh.run.lock().unwrap_or_else(|e| e.into_inner());
        let Some(running) = &r.running else {
            reply.send_status(status::SESSION_IDLE);
            return;
        };
        if let Some(want) = interrupt_id {
            // compare with the eval's id (payload of its bencode string)
            let have = running.id.as_deref().and_then(payload_of);
            if have != Some(want) {
                reply.send_status(status::INTERRUPT_ID_MISMATCH);
                return;
            }
        }
        let Some(intr) = sh.intr.get().cloned() else {
            reply.send_status(status::SESSION_IDLE);
            return;
        };
        sh.interrupted.store(true, Ordering::SeqCst);
        // evals queued behind it are lost, as on the JVM (its session thread is replaced)
        sh.q.lock().unwrap_or_else(|e| e.into_inner()).jobs.clear();
        let seq = r.seq;
        // `{id: <eval id>, status [done interrupted]}` on the interrupter's connection
        let eval_id = running.id.clone();
        let to_eval = reply.with_id(eval_id.as_deref());
        let outbox = running.reply.outbox().clone();
        drop(r);
        to_eval.send_status(status::INTERRUPTED);
        reply.send_status(status::DONE);
        intr.soft();
        outbox.wake_blocked_senders();
        // The hard stage: one timed wait that exists only while this interrupt is
        // pending. It ends as soon as the eval does.
        let shared = self.shared.clone();
        let _ = std::thread::Builder::new().name("mova-nrepl-interrupt".into()).stack_size(256 * 1024).spawn(move || {
            let deadline = Instant::now() + Duration::from_millis(100);
            let mut r = shared.run.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if r.seq != seq || r.running.is_none() {
                    return;
                }
                let now = Instant::now();
                if now >= deadline {
                    break;
                }
                r = shared.run_cv.wait_timeout(r, deadline - now).unwrap_or_else(|e| e.into_inner()).0;
            }
            // still running after 100 ms: stage two (also wakes a blocked wait)
            intr.hard();
            outbox.wake_blocked_senders();
        });
    }
}

/// Payload of a raw bencode string (`3:abc` gives `abc`).
fn payload_of(raw: &[u8]) -> Option<&[u8]> {
    let colon = raw.iter().position(|&b| b == b':')?;
    raw.get(colon + 1..)
}

fn fail(job: &EvalJob, text: &str) {
    job.reply.send(&[("err", V::Str(text))]);
    job.reply.send(&[("status", V::Strs(&["done", "error"]))]);
}

fn session_main(core: Arc<Core>, shared: Arc<SessionShared>, session: Arc<Session>) {
    let Some((mut interp, vars)) = core.boot.fork() else {
        // The interpreter did not start: answer what is queued, then stop.
        loop {
            let job = match next_job(&shared) {
                Some(j) => j,
                None => return,
            };
            fail(&job, "nREPL: the interpreter failed to start\n");
        }
    };
    let _ = shared.intr.set(interp.intr.clone());
    let init = shared.published.lock().unwrap_or_else(|e| e.into_inner()).clone();
    vars.push_session(init.as_ref(), shared.stdin.stream_value(interp.intr.clone()));
    while let Some(job) = next_job(&shared) {
        let mut send_done = true;
        shared.begin(&job);
        let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| eval::run(&mut interp, &vars, &job, &Ctl(Some(&shared)))));
        shared.settle();
        // JVM `skip-stdin-newline`: the newline after a form that `(read)` took
        if let Some(Value::HostInst(h)) = vars.inp.current_binding() {
            crate::hostclass::stream_skip_buffered_newline(&h);
        }
        shared.stdin.skip_newline();
        match ran {
            Ok((ns, d)) => {
                session.set_ns(&ns);
                send_done = d;
            }
            Err(_) => {
                job.reply.send(&[("err", V::Str("nREPL: internal error while evaluating\n"))]);
                job.reply.send(&[("status", V::Strs(status::EVAL_ERROR))]);
            }
        }
        // Publish before `done`: a `clone` sent after `done` must see these bindings.
        *shared.published.lock().unwrap_or_else(|e| e.into_inner()) = Some(vars.snapshot());
        if send_done && !shared.was_interrupted() {
            job.send_done();
        }
        shared.stdin.set_reply(None);
    }
    vars.pop_session();
}

/// Blocks until there is a job; `None` when the session is closed.
fn next_job(shared: &SessionShared) -> Option<EvalJob> {
    let mut q = shared.q.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        if q.closed {
            return None;
        }
        if let Some(j) = q.jobs.pop_front() {
            return Some(j);
        }
        q = shared.cv.wait(q).unwrap_or_else(|e| e.into_inner());
    }
}

// ---------------------------------------------------------------------------
// ephemeral requests
// ---------------------------------------------------------------------------

struct PoolState {
    jobs: VecDeque<EvalJob>,
    /// Workers waiting on the condvar.
    idle: usize,
}

pub(crate) struct Pool {
    st: Mutex<PoolState>,
    cv: Condvar,
}

impl Pool {
    pub(crate) fn new() -> Pool {
        Pool { st: Mutex::new(PoolState { jobs: VecDeque::new(), idle: 0 }), cv: Condvar::new() }
    }

    /// Queues a job; starts a worker if every existing one is busy. Never blocks.
    pub(crate) fn submit(&self, core: &Arc<Core>, job: EvalJob) {
        let spawn = {
            let mut st = self.st.lock().unwrap_or_else(|e| e.into_inner());
            st.jobs.push_back(job);
            // Queued jobs include ones a woken worker has not taken yet, so
            // `jobs > idle` means a new worker is really needed.
            let need = st.jobs.len() > st.idle;
            if !need {
                self.cv.notify_one();
            }
            need
        };
        if spawn {
            let core2 = core.clone();
            if let Err(e) = core.spawn_thread("mova-nrepl-eval", &core.pool_threads, move || worker_main(core2)) {
                let jobs: Vec<EvalJob> = self.st.lock().unwrap_or_else(|e| e.into_inner()).jobs.drain(..).collect();
                for j in jobs {
                    fail(&j, &format!("nREPL: cannot start an eval thread: {e}\n"));
                }
            }
        }
    }

    /// Next job, or `None` if this worker should end (queue empty and the
    /// idle quota is taken).
    fn next(&self) -> Option<EvalJob> {
        let mut st = self.st.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(j) = st.jobs.pop_front() {
                return Some(j);
            }
            if st.idle >= KEEP_IDLE_WORKERS {
                return None;
            }
            st.idle += 1;
            st = self.cv.wait(st).unwrap_or_else(|e| e.into_inner());
            st.idle -= 1;
        }
    }
}

fn worker_main(core: Arc<Core>) {
    let Some((mut interp, vars)) = core.boot.fork() else {
        while let Some(job) = core.pool.next() {
            fail(&job, "nREPL: the interpreter failed to start\n");
        }
        return;
    };
    let defaults = vars.defaults();
    let empty_in = super::input::empty_stream();
    while let Some(job) = core.pool.next() {
        // Fresh bindings for every request: nothing a request `set!`s survives it.
        vars.push_session(Some(&defaults), empty_in.clone());
        let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| eval::run(&mut interp, &vars, &job, &Ctl(None))));
        vars.pop_session();
        let mut send_done = true;
        match ran {
            Ok((_, d)) => send_done = d,
            Err(_) => {
                job.reply.send(&[("err", V::Str("nREPL: internal error while evaluating\n"))]);
                job.reply.send(&[("status", V::Strs(status::EVAL_ERROR))]);
            }
        }
        if send_done {
            job.send_done();
        }
    }
}
