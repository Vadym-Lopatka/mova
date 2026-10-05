//! # L5 / W4 — `simulate`: the deterministic-simulation API surface
//!
//! *Owner-ruled APPROVED, 2026-08-28 (`docs/OWNER-BRIEF-L5.md` RULING 1),
//! against `docs/L5-VIRTUAL-TIME-DESIGN.md` §5/§6 and the demo shapes in
//! `docs/L5-FLAGSHIP-DEMOS.md`.*
//!
//! ```clojure
//! (simulate {:seed 0x5EED} (fn [] ...))
//! ;; => {:result r :virtual-ms n :resumes n :timer-fires n :leaked-tasks n}
//! ```
//!
//! ## What one call is
//!
//! `simulate` runs `thunk` as the ROOT TASK of the sim world and blocks the
//! calling thread until that world is QUIESCENT — which is strictly later
//! than "the thunk returned" (P6a deviation #4: leftover timers keep firing
//! after the root's last expression, and returning at root-done would
//! truncate both `:virtual-ms` and the next call's starting state). The
//! calling thread does nothing else for the duration: it is the boundary
//! thread of design §5, and its only two cross-thread edges are the root
//! spawn and the done signal.
//!
//! ## Sequential calls in ONE process — the headline idiom
//!
//! ```clojure
//! (doseq [seed (range 100)] (simulate {:seed seed} scenario))
//! ```
//!
//! is the shape the flagship demos are built out of, so it is the shape this
//! module is designed around. Per call:
//!
//! - **both SplitMix64 streams are RE-SEEDED** from that call's `:seed`, so a
//!   call's schedule is a function of its own seed and nothing else — not of
//!   how many calls preceded it (`clock::sim_call_begin`);
//! - **virtual time is NOT reset.** It is monotonic for the process's whole
//!   life, because `Instant`s minted from the sim anchor escape into
//!   supervisor state, timer deadlines and user data, and a clock that went
//!   backwards would invert every one of those comparisons. `:virtual-ms` is
//!   therefore a DELTA: virtual-now at root-done minus virtual-now at call
//!   start;
//! - **the epoch window is per call** (`:epoch-ms`), defaulting to the
//!   process-wide `SIM_EPOCH_BASE_MS + total virtual ms` — i.e. the pre-W4
//!   behaviour, continued;
//! - **`:resumes` / `:timer-fires` are deltas** against the process-global
//!   counters;
//! - **the world is left CLEAN**: every task still alive when the root
//!   finishes is killed (see "Teardown"), so call N+1 starts from an empty
//!   slab and an empty timer heap exactly as call 1 did.
//!
//! ## Teardown, and the deadlock tripwire
//!
//! Both live at the sim scheduler's quiescence point (`runtime::sim_next_job`
//! step 4), because that is the ONE place in the process that can see the
//! slab, and both reuse the L4 kill path verbatim — `TaskWaker::kill`'s
//! `PARKED -> KILLED` CAS plus commit-cell claims, then `Job::Kill` ->
//! `kill_task`'s force-unwind on the task's own shard. Nothing new is
//! written; the teardown only decides WHO gets killed and in what order
//! (slab slot order, so it is a function of the schedule and not of a race).
//!
//! - **root done, tasks still alive** → they are parked forever (nothing is
//!   left to wake them). Kill them; report the count as `:leaked-tasks`.
//! - **root NOT done, queues empty, heap empty, tasks alive** → the world is
//!   stuck. `simulate` throws `SIM-E-DEADLOCK` naming the live-task count and
//!   BOTH mechanisms (design §6 + the P6b F3 amendment), then tears down so
//!   the next call is still clean.
//! - **`:max-resumes` exceeded** → `SIM-E-MAX-RESUMES`, same teardown.
//!
//! ## Every error is NAMED
//!
//! `SIM-E-OPTS`, `SIM-E-IN-TASK`, `SIM-E-NESTED`, `SIM-E-RUNTIME-STARTED`,
//! `SIM-E-TRACE`, `SIM-E-DEADLOCK`, `SIM-E-MAX-RESUMES`, `SIM-E-TEARDOWN`.
//! Greppable on purpose: a test asserts on the token, not on prose that may
//! be reworded.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use crate::builtins::{reg, ArityHint};
use crate::error::RjError;
use crate::eval::Interp;
use crate::keyword::Keyword;
use crate::sync::lock_mutex;
use crate::value::{PMap, Value};

// ---------------------------------------------------------------------------
// Call state
// ---------------------------------------------------------------------------

/// Why the sim world is being torn down. Carried from the quiescence point to
/// the waiting caller, where it decides the result vs. the named error.
#[derive(Clone, Copy, Debug)]
pub(crate) enum StopReason {
    /// The root task returned and the shard then drained to quiescence.
    RootDone,
    /// Design §6: root not done, nothing runnable, nothing armed, tasks
    /// alive. `live` is the census at the instant of detection — BEFORE the
    /// teardown, so the error reports what was actually stuck.
    Deadlock { live: usize },
    /// `:max-resumes` exhausted.
    MaxResumes,
}

/// What a `simulate` call ended up being.
struct Outcome {
    /// The root thunk's own result, present unless the call never got that
    /// far (deadlock / budget).
    root: Option<Result<Value, RjError>>,
    reason: StopReason,
    /// Virtual ns at the instant the root task's body returned, absolute.
    /// `u64::MAX` sentinel = the root never finished.
    root_done_ns: u64,
    leaked: u64,
    /// `Some(msg)` when the teardown itself could not finish — the loud
    /// `SIM-E-TEARDOWN` arm.
    teardown_failure: Option<String>,
}

struct CallState {
    seed: u64,
    call_start_ns: u64,
    resumes_base: u64,
    fires_base: u64,
    /// Set by the root task body as its very last act.
    root_done: AtomicBool,
    root_done_ns: AtomicU64,
    root_result: Mutex<Option<Result<Value, RjError>>>,
    /// Kills claimed by the teardown + never-started spawns it dropped.
    leaked: AtomicU64,
    /// Dispatches the teardown is still allowed to make before it gives up.
    teardown_budget: AtomicU64,
    stop_reason: Mutex<Option<StopReason>>,
    outcome: Mutex<Option<Outcome>>,
    cv: Condvar,
}

/// One `simulate` at a time, process-wide. `AtomicBool` rather than a peek at
/// [`CALL`] so the shard's per-quiescence check is one relaxed load.
static CALL_ACTIVE: AtomicBool = AtomicBool::new(false);
static CALL: Mutex<Option<Arc<CallState>>> = Mutex::new(None);

/// The ABSOLUTE `runtime::tasks_resumed()` value at which the live call's
/// `:max-resumes` budget is spent. `u64::MAX` = no budget (the default, and
/// every moment no call is running), so the check the sim scheduler pays per
/// dispatch is one relaxed load and one compare.
static RESUME_TRIP: AtomicU64 = AtomicU64::new(u64::MAX);

/// How many dispatches the teardown may make before it declares defeat. A
/// leaked task normally dies in ONE round (it is parked, so the kill CAS
/// lands); the budget exists for the P5a-bis salvage arm, where a task with a
/// committed value refuses the kill, must be let run to its next park, and is
/// then killed — plus the pathological case where a destructor spawns work.
const TEARDOWN_DISPATCH_BUDGET: u64 = 4096;

fn current_call() -> Option<Arc<CallState>> {
    lock_mutex(&CALL).clone()
}

/// True while a `simulate` call is between its root spawn and its outcome.
#[inline]
pub(crate) fn call_active() -> bool {
    CALL_ACTIVE.load(Ordering::Relaxed)
}

/// True once the live call's `:max-resumes` budget is spent. One relaxed load
/// plus the shard-counter sum the runtime already exposes; `u64::MAX` when no
/// budget is set, so the common case short-circuits on the load.
#[inline]
pub(crate) fn budget_blown() -> bool {
    let trip = RESUME_TRIP.load(Ordering::Relaxed);
    trip != u64::MAX && crate::runtime::tasks_resumed() >= trip
}

/// True while the sim scheduler is destroying the world rather than running
/// it (see this module's "Teardown").
#[inline]
pub(crate) fn tearing_down() -> bool {
    call_active() && TEARING_DOWN.load(Ordering::Relaxed)
}

static TEARING_DOWN: AtomicBool = AtomicBool::new(false);

/// What the sim scheduler should do having found the world quiescent: every
/// queue empty, the timer heap empty after the cancelled-skip, nothing left
/// to fire. `live` is the slab census at that instant.
pub(crate) enum Quiesce {
    /// Genuinely nothing to do — park, exactly as real mode does.
    Park,
    /// Start destroying the world; the reason travels to the caller.
    Teardown(StopReason),
}

/// The quiescence verdict (design §6). Cold: reached once per park, never on
/// a dispatch path.
pub(crate) fn on_quiescent(live: usize) -> Quiesce {
    if !call_active() || TEARING_DOWN.load(Ordering::Relaxed) {
        return Quiesce::Park;
    }
    let Some(st) = current_call() else {
        return Quiesce::Park;
    };
    if st.root_done.load(Ordering::Acquire) {
        return Quiesce::Teardown(StopReason::RootDone);
    }
    // Root not done and NOTHING alive: this is the pre-spawn park (design §8
    // R6) — the shard started inside `shards()` before the boundary thread's
    // `inject_spawn` landed. Not a deadlock; the spawn is the wake.
    if live == 0 {
        return Quiesce::Park;
    }
    Quiesce::Teardown(StopReason::Deadlock { live })
}

/// Arm the teardown. Idempotent: the FIRST reason wins, so a budget blowout
/// that happens to coincide with root-done still reports as the budget.
pub(crate) fn begin_teardown(reason: StopReason) {
    if !call_active() {
        return;
    }
    let Some(st) = current_call() else { return };
    if !TEARING_DOWN.swap(true, Ordering::AcqRel) {
        *lock_mutex(&st.stop_reason) = Some(reason);
        st.teardown_budget
            .store(TEARDOWN_DISPATCH_BUDGET, Ordering::Relaxed);
    }
}

/// One task destroyed by the teardown (a claimed kill, or a never-started
/// spawn dropped out of the inbox).
pub(crate) fn note_leaked(n: u64) {
    if let Some(st) = current_call() {
        st.leaked.fetch_add(n, Ordering::Relaxed);
    }
}

/// Spend one teardown dispatch. `false` once the budget is gone — the caller
/// then reports `SIM-E-TEARDOWN` instead of looping forever.
pub(crate) fn spend_teardown_dispatch() -> bool {
    let Some(st) = current_call() else { return false };
    let left = st.teardown_budget.load(Ordering::Relaxed);
    if left == 0 {
        return false;
    }
    st.teardown_budget.store(left - 1, Ordering::Relaxed);
    true
}

/// The world is empty: publish the outcome and wake the boundary thread.
/// `failure` is `Some` only on the `SIM-E-TEARDOWN` path.
pub(crate) fn finish(failure: Option<String>) {
    let Some(st) = current_call() else { return };
    let reason = lock_mutex(&st.stop_reason).unwrap_or(StopReason::RootDone);
    let root = lock_mutex(&st.root_result).take();
    let out = Outcome {
        root,
        reason,
        root_done_ns: if st.root_done.load(Ordering::Acquire) {
            st.root_done_ns.load(Ordering::Acquire)
        } else {
            u64::MAX
        },
        leaked: st.leaked.load(Ordering::Relaxed),
        teardown_failure: failure,
    };
    // Order matters: stop the scheduler consulting this call BEFORE the
    // boundary thread can wake and start the next one.
    RESUME_TRIP.store(u64::MAX, Ordering::Relaxed);
    TEARING_DOWN.store(false, Ordering::Release);
    CALL_ACTIVE.store(false, Ordering::Release);
    let mut g = lock_mutex(&st.outcome);
    *g = Some(out);
    drop(g);
    st.cv.notify_all();
}

// ---------------------------------------------------------------------------
// The native
// ---------------------------------------------------------------------------

fn kw(name: &str) -> Value {
    Value::Keyword(Keyword::from(name))
}

fn opts_err(msg: impl std::fmt::Display) -> RjError {
    RjError::other(format!("simulate: SIM-E-OPTS {msg}"))
}

/// Read an optional non-negative int opt.
fn int_opt(m: &PMap, name: &str) -> Result<Option<u64>, RjError> {
    match m.get(&kw(name)) {
        None | Some(Value::Nil) => Ok(None),
        Some(Value::Int(n)) if *n >= 0 => Ok(Some(*n as u64)),
        Some(other) => Err(opts_err(format!(
            "{name} must be a non-negative integer, got {}",
            other.type_name()
        ))),
    }
}

pub(crate) fn register(i: &mut Interp) {
    reg(i, "simulate", ArityHint::Exact(2), |interp, args| {
        simulate(interp, &args[0], &args[1])
    });
}

fn simulate(interp: &mut Interp, opts: &Value, thunk: &Value) -> Result<Value, RjError> {
    // --- opts -------------------------------------------------------------
    let Value::Map(m) = opts else {
        return Err(opts_err(format!(
            "the first argument must be an options map, got {}",
            opts.type_name()
        )));
    };
    let seed = match m.get(&kw("seed")) {
        Some(Value::Int(n)) => *n as u64,
        Some(other) => {
            return Err(opts_err(format!(
                ":seed must be an integer, got {}",
                other.type_name()
            )))
        }
        None => {
            return Err(opts_err(
                ":seed is REQUIRED — a simulation without a seed is not reproducible, \
                 and reproducibility is the whole product (e.g. (simulate {:seed 0x5EED} f))",
            ))
        }
    };
    let epoch_ms = int_opt(m, "epoch-ms")?;
    let max_resumes = int_opt(m, "max-resumes")?;
    let trace_path = match m.get(&kw("trace")) {
        None | Some(Value::Nil) => None,
        Some(Value::Str(s)) => Some(s.to_string()),
        Some(other) => {
            return Err(opts_err(format!(
                ":trace must be a file path string, got {}",
                other.type_name()
            )))
        }
    };

    // --- preconditions ----------------------------------------------------
    if crate::runtime::in_task() {
        return Err(RjError::other(
            "simulate: SIM-E-IN-TASK called from inside a task. The calling thread BLOCKS until \
             the simulated world is quiescent, and in sim there is exactly one shard — so a task \
             calling simulate would wedge the very shard the simulation needs in order to run. \
             Call simulate from an ordinary thread (the boundary contract, design §5).",
        ));
    }
    if call_active() {
        return Err(RjError::other(
            "simulate: SIM-E-NESTED a simulate call is already running in this process. \
             Sim is process-scoped (design §2, Ownership): one world, one root, one seed at a \
             time. Sequential calls are the supported idiom; nested/concurrent ones are not.",
        ));
    }
    if !crate::clock::sim_enabled() {
        if !crate::runtime::runtime_untouched() {
            return Err(RjError::other(
                "simulate: SIM-E-RUNTIME-STARTED this process already started the task runtime in \
                 REAL mode. Sim forces one shard, disables the direct-switch slot and replaces the \
                 timer thread with an inline heap drain — all decided once, at the runtime's first \
                 use — so sim must BE that first use (design §2, Ownership). Call simulate before \
                 anything else spawns a task, arms a timer or asks for the shard count; or set \
                 MOVA_SIM_SEED for the whole process.",
            ));
        }
        crate::clock::sim_enable(seed);
    }
    if crate::builtins::r#async::timer_threads_spawned() > 0 {
        return Err(RjError::other(
            "simulate: SIM-E-RUNTIME-STARTED this process already started the real-mode timer \
             thread. In sim the shard loop IS the timer service (design §2, swap 2); a second, \
             wall-clock-driven mutator of the same heap would make virtual time a race.",
        ));
    }

    // --- per-call setup ---------------------------------------------------
    // The world is quiescent here by construction: the previous call (if any)
    // returned only after its teardown emptied the slab and the shard parked.
    let call_start_ns = crate::clock::sim_call_begin(seed, epoch_ms);
    let trace_slot = match &trace_path {
        Some(p) => match crate::clock::trace_push(p) {
            Ok(slot) => Some(slot),
            Err(e) => {
                crate::clock::sim_call_end();
                return Err(RjError::other(format!(
                    "simulate: SIM-E-TRACE couldn't open :trace {p:?}: {e}"
                )));
            }
        },
        None => None,
    };

    let st = Arc::new(CallState {
        seed,
        call_start_ns,
        resumes_base: crate::runtime::tasks_resumed(),
        fires_base: crate::builtins::r#async::timer_fires_total(),
        root_done: AtomicBool::new(false),
        root_done_ns: AtomicU64::new(0),
        root_result: Mutex::new(None),
        leaked: AtomicU64::new(0),
        teardown_budget: AtomicU64::new(TEARDOWN_DISPATCH_BUDGET),
        stop_reason: Mutex::new(None),
        outcome: Mutex::new(None),
        cv: Condvar::new(),
    });
    *lock_mutex(&CALL) = Some(st.clone());
    RESUME_TRIP.store(
        match max_resumes {
            Some(b) => st.resumes_base.saturating_add(b),
            None => u64::MAX,
        },
        Ordering::Relaxed,
    );
    TEARING_DOWN.store(false, Ordering::Release);
    CALL_ACTIVE.store(true, Ordering::Release);

    // --- the root task ----------------------------------------------------
    // `go`-conveyance shape, verbatim (`builtins::async::run_go_body`): fork
    // the interpreter and snapshot the caller's dynamic bindings HERE, on the
    // calling thread, before the spawn.
    let mut forked = interp.fork();
    let f = thunk.clone();
    let conveyed = crate::env::snapshot_thread_bindings();
    let root_st = st.clone();
    crate::runtime::spawn(move || {
        let _bindings = crate::env::BindingConveyance::install(conveyed);
        let result = forked.call(&f, &[]);
        *lock_mutex(&root_st.root_result) = Some(result);
        // The two stores the quiescence point reads. `root_done_ns` first, so
        // a reader that sees `root_done` sees a settled instant.
        root_st
            .root_done_ns
            .store(crate::clock::sim_now_ns(), Ordering::Release);
        root_st.root_done.store(true, Ordering::Release);
    });

    // --- block until the world is quiescent -------------------------------
    let outcome = {
        let mut g = lock_mutex(&st.outcome);
        while g.is_none() {
            g = st
                .cv
                .wait(g)
                .unwrap_or_else(|e| e.into_inner());
        }
        g.take().expect("the loop above only exits with an outcome")
    };
    *lock_mutex(&CALL) = None;
    if let Some(slot) = trace_slot {
        crate::clock::trace_pop(slot);
    }
    crate::clock::sim_call_end();

    // --- report -----------------------------------------------------------
    let resumes = crate::runtime::tasks_resumed().saturating_sub(st.resumes_base);
    let fires = crate::builtins::r#async::timer_fires_total().saturating_sub(st.fires_base);
    if let Some(msg) = outcome.teardown_failure {
        return Err(RjError::other(format!(
            "simulate: SIM-E-TEARDOWN {msg} (seed {seed:#x})"
        )));
    }
    match outcome.reason {
        StopReason::Deadlock { live } => {
            return Err(RjError::other(format!(
                "simulate: SIM-E-DEADLOCK the simulated world is stuck at virtual ms {vms} with \
                 {live} live task(s): nothing is runnable, no timer is armed, and the root thunk \
                 has not returned. Either EVERY task is parked forever (nobody is left to wake \
                 anyone — a genuine deadlock in the program under test), OR an out-of-contract OS \
                 thread is doing work the simulation cannot see (design §4 fence #1/#3/#17: :io \
                 procs, thread*, or a poll loop carrying a virtual deadline it never armed a timer \
                 for). seed {seed:#x}",
                vms = (crate::clock::sim_now_ns().saturating_sub(call_start_ns)) / 1_000_000,
            )));
        }
        StopReason::MaxResumes => {
            return Err(RjError::other(format!(
                "simulate: SIM-E-MAX-RESUMES the :max-resumes budget of {budget} task resumes was \
                 spent before the root thunk returned (seed {seed:#x}, virtual ms {vms}). Either \
                 the budget is too small for this program, or a task is spinning without ever \
                 parking — at one cooperative shard that starves every peer forever (design §8 \
                 R1), which is the hang this tripwire exists to name.",
                budget = max_resumes.unwrap_or(0),
                vms = (crate::clock::sim_now_ns().saturating_sub(call_start_ns)) / 1_000_000,
            )));
        }
        StopReason::RootDone => {}
    }

    let root = outcome
        .root
        .expect("RootDone implies the root task published its result");
    // Rethrow AFTER the drain and the teardown: the world is already clean,
    // so a caller that catches this can simulate again immediately.
    let value = root?;

    let virtual_ms = outcome.root_done_ns.saturating_sub(call_start_ns) / 1_000_000;
    let mut out = PMap::new();
    out.insert(kw("result"), value);
    out.insert(kw("virtual-ms"), Value::Int(virtual_ms as i64));
    out.insert(kw("resumes"), Value::Int(resumes as i64));
    out.insert(kw("timer-fires"), Value::Int(fires as i64));
    out.insert(kw("leaked-tasks"), Value::Int(outcome.leaked as i64));
    Ok(Value::Map(out))
}

/// `embed::sim::enable`'s body — see there for the contract. Kept here so the
/// precondition text stays next to `simulate`'s.
pub(crate) fn enable_process_sim(seed: u64) -> Result<(), String> {
    if crate::clock::sim_enabled() {
        return Ok(());
    }
    if !crate::runtime::runtime_untouched() || crate::builtins::r#async::timer_threads_spawned() > 0
    {
        return Err(
            "SIM-E-RUNTIME-STARTED: this process already started the task runtime (or the timer \
             thread) in real mode; sim mode must be the process's FIRST runtime use."
                .to_string(),
        );
    }
    crate::clock::sim_enable(seed);
    Ok(())
}

/// The seed the live call is running under, for diagnostics.
#[allow(dead_code)]
pub(crate) fn current_seed() -> Option<u64> {
    current_call().map(|st| st.seed)
}

/// Virtual ns at which the live call started, for diagnostics.
#[allow(dead_code)]
pub(crate) fn current_call_start_ns() -> Option<u64> {
    current_call().map(|st| st.call_start_ns)
}
