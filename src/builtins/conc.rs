//! Concurrency substrate (v0.2 / A1): `future*`, `promise`, `deliver`,
//! `delay*`, `force`, `sleep-ms`, plus the blocking-wait/force-once helpers
//! `atoms.rs`'s `deref` dispatches into for `Value::Future`/`Promise`/
//! `Delay`. `future?`/`promise?`/`delay?`/`realized?` live in
//! `predicates.rs` (pure state inspection, no blocking).
//!
//! `future`/`delay` themselves are `core/core.mova` macros
//! (`(future body...)` => `(future* (fn [] body...))`) that expand to calls
//! into the `*`-suffixed natives here, mirroring how `lazy-seq`/`lazy-seq*`
//! already work.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::builtins::{reg, ArityHint};
use crate::error::RjError;
use crate::eval::Interp;
use crate::runtime::TaskWaker;
use crate::sync::{cv_wait, cv_wait_timeout, lock_mutex};
use crate::value::{BufferPolicy, Chan, DelayCell, FutureCell, FutureState, PromiseCell, PromiseState, Value};

/// 64 MiB, matching `main.rs`'s `EVAL_STACK_SIZE` reasoning: the
/// interpreter is a tree-walking recursive-descent evaluator, so deep
/// mova-level recursion inside a `(future ...)` body needs real Rust stack
/// headroom just like the main eval thread does.
const FUTURE_STACK_SIZE: usize = 64 * 1024 * 1024;

pub fn register(i: &mut Interp) {
    reg(i, "future*", ArityHint::Exact(1), |interp, args| {
        let f = args[0].clone();
        let cell = Arc::new({
            let mut c = FutureCell::pending();
            c.wrap_failures = true; // JVM: `deref` of a failed `future` throws ExecutionException
            c
        });
        let thread_cell = cell.clone();
        // Everything captured by the spawned closure is `Arc`-rooted
        // (`forked.globals` shares the same globals `Env` `Arc`;
        // `thread_cell`/`f` are `Arc`/`Value` clones) -- no raw references
        // cross the thread boundary, which is exactly what `Value: Send +
        // Sync` (see value.rs's `_assert_send_sync`) makes safe.
        let forked = interp.fork();
        // M4b conveyance: capture the SPAWNING thread's live dynamic
        // bindings here (this line runs on the parent), reproduce them on
        // the child for the closure's whole run, and pop them when the
        // guard drops -- measured Clojure: `(binding [*a* 42] @(future
        // *a*))` is 42. The guard's Drop runs even if `call` panics.
        let conveyed = crate::env::snapshot_thread_bindings();
        // **L5/W3 fence #3 (design §4): in sim, a TASK, not an OS thread.**
        //
        // The body is byte-identical either way (`run_future_body` below);
        // only the placement forks. P6b's F2 is why: the sim advance rule
        // reads "no runnable task" as "the world is quiescent" and jumps
        // virtual time onto the earliest armed deadline — so an in-flight
        // OS thread's work is not merely un-ordered, it is jumped PAST, and
        // the program gets a wrong answer (`p6b-osthread-jump.mova`: real
        // `[:task :thread]`, sim `[:task nil]`). As a task the future's body
        // joins the seeded schedule and the shard cannot be idle while it is
        // runnable. Conveyance is preserved exactly (snapshotted above, on
        // the spawning side, L1/W5b) — the task path is not a second
        // conveyance policy, it is the same closure on a different executor.
        if crate::clock::sim_enabled() {
            crate::runtime::spawn(move || run_future_body(forked, f, thread_cell, conveyed));
            return Ok(Value::Future(cell));
        }
        let builder = std::thread::Builder::new()
            .name("mova-future".to_string())
            .stack_size(FUTURE_STACK_SIZE);
        let spawn_result = builder.spawn(crate::memstat::drained(move || run_future_body(forked, f, thread_cell, conveyed)));
        match spawn_result {
            Ok(handle) => {
                drop(handle); // detach: the result lives in `cell`, not the JoinHandle
                Ok(Value::Future(cell))
            }
            Err(e) => Err(RjError::other(format!("future: couldn't spawn thread: {e}"))),
        }
    });

    reg(i, "promise", ArityHint::Exact(0), |_i, _args| {
        Ok(Value::Promise(Arc::new(PromiseCell::pending())))
    });

    reg(i, "deliver", ArityHint::Exact(2), |_i, args| match &args[0] {
        // Clojure: a second `deliver` on an already-delivered promise is a
        // no-op that returns `nil` (doesn't overwrite, doesn't error); the
        // first returns the promise itself.
        Value::Promise(cell) => Ok(if deliver_promise(cell, args[1].clone()) {
            args[0].clone()
        } else {
            Value::Nil
        }),
        other => Err(RjError::type_err(format!("deliver: expected a promise, got {}", other.type_name()))),
    });

    reg(i, "delay*", ArityHint::Exact(1), |_i, args| {
        Ok(Value::Delay(Arc::new(DelayCell {
            f: std::sync::Mutex::new(Some(args[0].clone())),
            result: std::sync::OnceLock::new(),
        })))
    });

    reg(i, "force", ArityHint::Exact(1), |interp, args| match &args[0] {
        Value::Delay(cell) => force_delay(interp, cell),
        // Clojure's `force` on a non-delay just returns it unchanged.
        other => Ok(other.clone()),
    });

    // SPEC-W3 (defect ledger D5): the runtime half of
    // `clojure.core/locking` -- `(locking x body...)` expands to
    // `(locking* x (fn [] body...))` in core.mova, the same `*`-suffixed
    // shape `future`/`delay`/`lazy-seq` already use. See
    // [`monitor_enter`]'s doc for what the lock actually is and how it
    // differs from a JVM per-object monitor.
    reg(i, "locking*", ArityHint::Exact(2), |interp, args| {
        // The JVM's `monitorenter` on `null` throws NullPointerException,
        // so `(locking nil ..)` never runs its body there either.
        if matches!(args[0], Value::Nil) {
            return Err(RjError::other(
                "locking: can't lock nil (NullPointerException on the JVM)",
            ));
        }
        // Released by `MonitorGuard::drop` -- on the normal return, on an
        // error return, and on a Rust panic alike. That is precisely what
        // upstream's `(try ... (finally (monitor-exit ..)))` buys, and
        // why the guard is a Drop type rather than a manual release.
        let _guard = monitor_enter(&interp.intr)?;
        interp.call(&args[1], &[])
    });

    reg(i, "sleep-ms", ArityHint::Exact(1), |i, args| match &args[0] {
        Value::Int(ms) if *ms >= 0 => {
            // **Fence #2 (design §4), owner-ruled 2026-08-28: VIRTUALIZE
            // EVERYWHERE — task or OS thread. See `sim_sleep_ms`'s doc.**
            if crate::clock::sim_enabled() {
                return sim_sleep_ms(*ms as u64);
            }
            i.intr.sleep(Duration::from_millis(*ms as u64))?;
            Ok(Value::Nil)
        }
        Value::Int(_) => Err(RjError::other("sleep-ms: expected a non-negative int")),
        other => Err(RjError::type_err(format!(
            "sleep-ms: expected an int, got {}",
            other.type_name()
        ))),
    });
}

// ---------------------------------------------------------------------------
// SPEC-W3 (defect ledger D5): `clojure.core/locking`
// ---------------------------------------------------------------------------

/// What `locking` locks: ONE process-wide REENTRANT monitor, not a
/// per-object one.
///
/// # Why a lock at all, and why this one
///
/// mova has real OS threads (`future*` spawns one), so `locking` cannot
/// honestly be a no-op — the guarantee it exists to provide is that two
/// bodies never run at once. It also cannot honestly be a JVM per-object
/// monitor: mova values are immutable and mostly unboxed, so there is no
/// object header to hang a monitor off and no stable identity to key one
/// by (two `=` maps are the same value, and `Value::Uuid`'s doc explains
/// how little room a `Value` has for an extra word).
///
/// So this is deliberately COARSER than the JVM: every `locking` form in
/// the process serializes against every other, whatever object it names.
/// That is strictly stronger than what upstream promises — mutual
/// exclusion is preserved, only concurrency is lost — and it is the
/// trade mova's concurrency model actually implies.
///
/// # Reentrancy is not optional
///
/// JVM monitors are reentrant, and `(locking a (locking b ...))` is
/// ordinary code. With one global lock those two forms collapse onto the
/// same monitor, so a non-reentrant implementation would self-deadlock on
/// a shape that works fine on the JVM. Hence the owner/depth pair: the
/// holding thread re-enters for free and only the outermost exit releases.
///
/// # The divergence, stated honestly
///
/// A thread that holds the monitor and then blocks on work that another
/// thread must take the monitor to finish — `(locking a @(future (locking
/// b ...)))` — deadlocks here where the JVM would not, because `a` and `b`
/// are the same lock. Nothing in the shipped stdlib does that: the one
/// upstream use in scope is `clojure.spec.gen.alpha`'s `dynaload`, which
/// serialises the FIRST `require` of the test.check namespaces and does no
/// cross-thread waiting inside the body. If a real program ever needs
/// per-object monitors, the fix is keyed monitors, not weakening this one.
static MONITOR_STATE: Mutex<MonitorState> = Mutex::new(MonitorState { owner: None, depth: 0 });
static MONITOR_CV: std::sync::Condvar = std::sync::Condvar::new();

struct MonitorState {
    owner: Option<std::thread::ThreadId>,
    depth: u64,
}

/// Held for the dynamic extent of one `locking` body; releases on drop.
struct MonitorGuard;

/// Acquires the monitor (see [`MONITOR_STATE`]), blocking until it is free
/// unless this thread already holds it.
fn monitor_enter(intr: &crate::interrupt::Interrupt) -> Result<MonitorGuard, RjError> {
    let me = std::thread::current().id();
    let mut st = lock_mutex(&MONITOR_STATE);
    loop {
        match st.owner {
            None => {
                st.owner = Some(me);
                st.depth = 1;
                return Ok(MonitorGuard);
            }
            Some(owner) if owner == me => {
                st.depth += 1;
                return Ok(MonitorGuard);
            }
            _ => {
                let (g, aborted) = crate::interrupt::wait_on(intr, &MONITOR_CV, st);
                st = g;
                if aborted {
                    return Err(intr.take_err().unwrap_or_else(|| RjError::interrupted("interrupted")));
                }
            }
        }
    }
}

impl Drop for MonitorGuard {
    fn drop(&mut self) {
        let mut st = lock_mutex(&MONITOR_STATE);
        st.depth -= 1;
        if st.depth == 0 {
            st.owner = None;
            // Same RESOLVER SEQUENCE discipline as `deliver_promise`:
            // drop the state lock BEFORE waking, so a woken thread's first
            // move (re-locking this very mutex) does not bounce.
            drop(st);
            MONITOR_CV.notify_all();
        }
    }
}

/// A `future*` body, independent of where it runs — the OS thread (real
/// mode) or a runtime task (sim, fence #3). ONE function so the two
/// placements cannot drift, exactly as `builtins::async`'s `run_go_body`
/// serves `go*`'s two placements.
///
/// Panic handling is unchanged from the pre-W3 thread closure: a Mova-level
/// error becomes `FutureState::Failed`, and a genuine Rust panic is not
/// caught here (it never was) — it unwinds out of the body, and the cell
/// stays pending, which is the same observable the thread path always had.
fn run_future_body(
    mut forked: Interp,
    f: Value,
    cell: Arc<FutureCell>,
    conveyed: Vec<(Arc<crate::env::VarCell>, Value)>,
) {
    let _bindings = crate::env::BindingConveyance::install(conveyed);
    let result = forked.call(&f, &[]);
    let new_state = match result {
        Ok(v) => FutureState::Done(v),
        Err(e) => FutureState::Failed(e),
    };
    // The runner detaches here (we never join it -- the `FutureCell` `Arc`
    // outlives it and owns the result); `resolve_future` publishes the state
    // and wakes BOTH waiter families (condvar for threads, drained
    // `TaskWaker`s for tasks).
    resolve_future(&cell, new_state);
}

/// **Fence #2 (design §4): `sleep-ms` under sim, owner-ruled 2026-08-28.**
///
/// `sleep-ms` means N ms of the WORLD's time; in sim the world's time is
/// virtual. VIRTUALIZE EVERYWHERE, REFUSE NOWHERE (OWNER-BRIEF-L5.md RULING
/// 2): "if the real program may use thread sleep then our simulation must
/// handle it in a deterministic way without compromises." The OS-thread
/// refusal that used to live here was exactly that compromise, so it is gone
/// — both callers now share ONE path.
///
/// Arms a fresh one-shot `Fixed(0)` chan on the shared timer service and
/// takes from it — `builtins::flow`'s `park_tick` shape, replicated here
/// (three lines) rather than reached across a module boundary for it, since
/// `park_tick` is `flow.rs`-private and the dependency would run the wrong
/// way (`conc` is below `flow`). `ms.max(1)`: the timer service arms in whole
/// milliseconds, and a 0 ms arm would be a deadline already in the past (same
/// floor `timer_push` and `park_tick` apply).
///
/// `chan_take` itself is where the two callers diverge, and it already knows
/// how (`builtins::async::chan_take`'s `in_task()` branch): from a TASK it
/// registers a `TakerWaiter` and parks the task; from an OS THREAD it falls
/// through to the condvar wait (`chan_wait`). Either way `timer_push` (inside
/// `timer_arm`) unconditionally calls `runtime::sim_unpark_shard` on the way
/// out (P6b, the arm-bridge) — which also STARTS the shard if none is running
/// yet (P6b's F1 fix) — so the shard wakes, drains the timer heap inline at
/// its idle point, and the advance rule jumps virtual time onto the deadline
/// regardless of which family is waiting on the chan's close. The sleeper —
/// task or thread — then wakes having slept exactly `ms` VIRTUAL ms: the
/// W4-saga shard-poison footgun dissolves *in sim*, and 30 days of
/// `sleep-ms` costs microseconds, from either caller.
///
/// **The determinism caveat, stated honestly.** An OS-thread caller is
/// outside the seeded schedule — nothing serializes when its `chan_take`
/// registers against the shard's own progress, so its interleaving with the
/// shard (and with any concurrent tasks) is OS-decided, same as any other
/// unmanaged thread touching sim state. What IS deterministic is the instant
/// it wakes at: always exactly the virtual deadline it armed, never early,
/// never a wall-clock guess. Under the `simulate` boundary contract (design
/// §5) every caller is a task, so this caveat never applies there and the
/// whole program is fully deterministic; it is only a live concern for a
/// bare OS thread poking a sim process from outside that contract (e.g. a
/// test driver), which is exactly what RULING 2 accepted the cost of.
///
/// Real mode never reaches this function.
fn sim_sleep_ms(ms: u64) -> Result<Value, RjError> {
    let tick = Arc::new(Chan::new(BufferPolicy::Fixed(0)));
    crate::builtins::r#async::timer_arm(tick.clone(), ms.max(1));
    let _ = crate::builtins::r#async::chan_take(&tick);
    Ok(Value::Nil)
}

/// The delivery half of `deliver`, as a plain function so native code can
/// resolve a promise without going through a `Value`-level call into the
/// interpreter (v0.3 / N2: `builtins::flow_steps`' `flow/step-sink-deliver`
/// delivers from inside a promoted `FastStep::transform`, where entering
/// the interpreter at all is precisely what the native-step tier exists to
/// avoid). Returns `true` iff THIS call was the one that delivered --
/// exactly the distinction `deliver`'s own return value encodes (promise vs
/// `nil`).
pub(crate) fn deliver_promise(cell: &Arc<PromiseCell>, v: Value) -> bool {
    let mut state = lock_mutex(&cell.state);
    if matches!(&*state, PromiseState::Pending) {
        *state = PromiseState::Delivered(v);
        // RESOLVER SEQUENCE (L3/W2b, see [`future_deref`]'s ordering proof):
        // set the state, DROP the state lock, then drain-and-wake. Never
        // wake while holding it -- a woken task's very first move can be to
        // re-lock this cell.
        drop(state);
        cell.cv.notify_all();
        wake_task_waiters(&cell.task_wakers);
        true
    } else {
        false
    }
}

// ---------------------------------------------------------------------------
// L3/W2b: the task arms of `deref` on a future/promise
// ---------------------------------------------------------------------------

/// Distinguishes one parked task's registration from another's on the SAME
/// cell, so a task that woke for an unrelated reason can retract exactly its
/// own entry. Process-wide and monotonic, the same shape (and for the same
/// reason) as `value.rs`'s `NEXT_DOORBELL_TOKEN`.
static NEXT_WAKER_TOKEN: AtomicU64 = AtomicU64::new(1);

/// The resolver half of a [`FutureCell`]: publish `new_state`, then wake
/// both waiter families. Every site that completes a future goes through
/// here -- `future*`'s own thread (above), `hostclass`'s `(Thread. f)`
/// `.start` body, `builtins::flow`'s `flow/inject` -- because a resolver
/// that forgot the task half would strand a parked task forever (there is
/// no safety net on a task park, by design).
///
/// Ordering is load-bearing; see [`future_deref`]'s proof.
pub(crate) fn resolve_future(cell: &FutureCell, new_state: FutureState) {
    {
        let mut g = lock_mutex(&cell.state);
        *g = new_state;
    } // state lock DROPPED here, before either wake below.
    cell.cv.notify_all();
    wake_task_waiters(&cell.task_wakers);
}

/// lsp/host (clojure-lsp-on-Mova campaign): [`resolve_future`]'s
/// "first-settlement-wins" counterpart -- `builtins::deferred`'s
/// manually-completable `Value::Future` (`promesa.core`'s `Deferred`
/// primitive) needs exactly this: TWO independent threads may race to
/// settle the same cell (e.g. `resolve!` and `reject!` called
/// concurrently from different threads on the same deferred), and unlike
/// every existing resolver site (`future*`'s own spawned thread,
/// `(Thread. f)`'s `.start` body, `flow/inject`), which are each the ONE
/// and only writer for their cell by construction, a deferred has no such
/// guarantee. The check-then-set happens under ONE lock acquisition (not
/// `resolve_future`'s check-outside-the-lock, which would race exactly
/// the way this function exists to prevent), so only the first caller
/// ever transitions the cell; every later caller's `bool` return is
/// `false` and it does nothing else.
pub(crate) fn try_resolve_future(cell: &FutureCell, new_state: FutureState) -> bool {
    {
        let mut g = lock_mutex(&cell.state);
        if !matches!(&*g, FutureState::Pending) {
            return false;
        }
        *g = new_state;
    } // state lock DROPPED here, before either wake below -- same ordering
      // proof as `resolve_future`.
    cell.cv.notify_all();
    wake_task_waiters(&cell.task_wakers);
    true
}

/// Take the whole task wait list and wake everyone on it, with the list's
/// own mutex already released.
///
/// DRAINING, not iterating in place, for `Doorbell::ring`'s reason: a woken
/// task's next act may be to lock this very list (to retract a token, or to
/// re-register on its next lap), and a handle left behind here would chase a
/// task that has already moved on to a different park. A task that is still
/// pending after this re-registers itself with a FRESH token.
fn wake_task_waiters(wakers: &Mutex<Vec<(u64, TaskWaker)>>) {
    let drained = std::mem::take(&mut *lock_mutex(wakers));
    for (_, w) in &drained {
        w.wake();
    }
}

/// Register the current task's waker on `wakers` and park, having already
/// established -- under `state_guard`, which the caller still holds -- that
/// the cell is unresolved. Returns once the task has been resumed ONCE, for
/// whatever reason; the caller re-checks the real condition and loops.
///
/// `state_guard` is consumed rather than borrowed to make the sequence
/// unmistakable at the call site: the push happens while it is alive, the
/// park after it is gone.
fn register_and_park<T>(
    state_guard: std::sync::MutexGuard<'_, T>,
    wakers: &Mutex<Vec<(u64, TaskWaker)>>,
) {
    let token = NEXT_WAKER_TOKEN.fetch_add(1, Ordering::Relaxed);
    // PUSH UNDER THE STATE LOCK. This single line is what the whole
    // missed-wake argument rests on -- see [`future_deref`]'s proof.
    lock_mutex(wakers).push((token, crate::runtime::current_waker()));
    drop(state_guard);
    // A resolve landing HERE has already drained our waker and called
    // `wake()`, which finds this task still RUNNING and leaves it NOTIFIED;
    // the shard then re-queues instead of parking it. The wake cannot be
    // lost -- `Doorbell::wait_for_change_task`'s identical window, same
    // argument.
    crate::runtime::park_current_yield();
    // Ours is already gone if a resolve woke us (the drain took it). This is
    // for the other case: a wake that reached this task for an unrelated
    // reason. Leaving a stale entry behind would arm a spurious wake at
    // whatever this task parks on NEXT -- harmless there too (every park in
    // this runtime re-checks its own condition on resume), but pointless,
    // and it would keep a `TaskShared` alive past its use.
    lock_mutex(wakers).retain(|(t, _)| *t != token);
}

/// The WITH-timeout task arms' whole body: park ONCE, woken by whichever
/// comes first -- the cell resolving, or the deadline (L3.5 item 1).
/// `Some(r)` iff `resolved` saw a terminal state; `None` means the deadline
/// won and the caller returns its `default`.
///
/// Generic over the cell so `FutureCell` and `PromiseCell` share one copy of
/// the protocol below rather than two transcriptions of it. `resolved`
/// inspects the locked state and clones out the outcome (`Result<Value,
/// RjError>` for a future, `Value` for a promise) iff it is terminal.
///
/// # Why this is not a poll any more
///
/// Until L3.5 this was a 1ms `park_tick` loop: one fresh `Arc<Chan>`, one
/// timer-heap arm, one timer-thread pop and one wake PER MILLISECOND per
/// waiter. `tests/l35_deref_park_probe.rs` measured what that costs -- 1000
/// tasks sitting in `(deref p 120000 :x)` burned 3.99 CPU cores doing
/// nothing, 10000 burned 11.9 and stretched every other task's tick 6.5x --
/// which fired the probe's pre-registered CPU trigger by 8x. A parked
/// timeout-deref now costs exactly what the no-timeout arm costs while it
/// waits: nothing. One waker registration, one timer entry, one park.
///
/// # The no-lost-wake argument (THE LAW: a task park has no safety net)
///
/// Three actors can end this park, and every one of them is serialized
/// against it:
///
/// 1. **The resolver** (`resolve_future`/`deliver_promise`). Handled by
///    [`future_deref`]'s ordering proof verbatim, because this arm performs
///    the identical check-and-push UNDER THE STATE LOCK that
///    [`register_and_park`] does: we hold `state`, observe unresolved, push
///    `(token, waker)` into `wakers` while still holding it, and only then
///    drop it. A resolver cannot write the terminal state without that lock,
///    so it either (a) got there first, in which case our re-check under the
///    lock sees it and we never park, or (b) comes after, in which case its
///    drain necessarily contains our waker. A `wake()` landing in the window
///    between `drop(guard)` and `park_current_yield()` is absorbed as
///    `NOTIFIED` and consumed at the park boundary -- the runtime's
///    missed-wakeup arm.
/// 2. **The deadline.** `timer_arm_waker` is called AFTER the registration
///    and BEFORE the first park, and returns the deadline it armed. We test
///    `clock_now() >= deadline` against THAT instant, and `timer_loop`
///    fires an entry only once `entry.deadline <= now` -- so "the timer
///    fired" implies "our test passes". This is the load-bearing half of the
///    single-park design: on a wake we cannot explain we re-park WITHOUT
///    re-arming, and the implication above is what guarantees such a re-park
///    is never past a deadline whose only wake has already been spent.
///    (Deriving our own deadline from our own `clock_now()` would break
///    it: ms-rounding, or `MAX_TIMEOUT_MS`, could leave the timer's deadline
///    strictly earlier than ours and strand the task.) **L5: the deadline
///    test and the timer arm must read the SAME clock** (design §2) -- both
///    now go through `crate::clock::clock_now()`, which reads the OS clock
///    directly outside sim and `SIM_ANCHOR + SIM_NOW_NS` under it; P6a
///    proved that a wall-clock test against a virtual-clock arm is a
///    deterministic hang (docs/L5-PROBE-RESULTS.md rule 5).
/// 3. **Anyone else** -- a stale wake from a park this task did earlier.
///    Legal and free: we re-read the state, re-read the clock, and park
///    again. Our registration is still in `wakers` (only a resolve drains
///    that list) and our timer entry is still armed (only its own deadline
///    removes it), so nothing has been consumed by the spurious lap.
///
/// On the way out -- both exits -- we set `cancel` (so the timer skips a
/// wake that can no longer mean anything) and retract our token by identity
/// (so a later resolver does not keep a `TaskShared` alive chasing a task
/// that has moved on). Neither is required for safety; see
/// `timer_arm_waker`'s doc and [`future_deref`]'s "stale wakers are
/// harmless" corollary.
///
/// The one liveness dependency is the timer thread itself: if the OS refused
/// to spawn it (`timer_arm` prints and gives up), an armed deadline never
/// fires -- exactly as `timeout` and the old `park_tick` loop already
/// behaved, since every tick of that loop was also a timer entry.
fn task_timeout_park<S, R>(
    state: &Mutex<S>,
    wakers: &Mutex<Vec<(u64, TaskWaker)>>,
    ms: u64,
    resolved: impl Fn(&S) -> Option<R>,
) -> Option<R> {
    // Fast path, and the whole of the zero/already-past budget case: one
    // look, no registration, no timer entry, no park.
    if let Some(r) = resolved(&lock_mutex(state)) {
        return Some(r);
    }
    if ms == 0 {
        return None;
    }

    let token = NEXT_WAKER_TOKEN.fetch_add(1, Ordering::Relaxed);
    let cancel = Arc::new(AtomicBool::new(false));
    {
        let guard = lock_mutex(state);
        // Re-check UNDER the lock we are about to publish our waker under:
        // case (a) of the argument above.
        if let Some(r) = resolved(&guard) {
            return Some(r);
        }
        // PUSH UNDER THE STATE LOCK -- the one line the whole missed-wake
        // argument rests on, same as [`register_and_park`]'s.
        lock_mutex(wakers).push((token, crate::runtime::current_waker()));
        drop(guard);
    }
    // ONE entry, once per `deref` CALL. Armed after the registration so that
    // a deadline landing immediately still finds a task that is either
    // RUNNING (leaves NOTIFIED) or parked below.
    let deadline = crate::builtins::r#async::timer_arm_waker(crate::runtime::current_waker(), ms, cancel.clone());
    loop {
        crate::runtime::park_current_yield();
        // The guard is named, and every arm drops it explicitly, so that
        // "the state lock is never held across the re-park" is visible
        // rather than inferred from temporary-scope rules. A task that
        // suspended holding it would wedge every resolver of this cell.
        let guard = lock_mutex(state);
        let outcome = match resolved(&guard) {
            Some(r) => Some(r),
            None if crate::clock::clock_now() >= deadline => None,
            // Neither source fired: a stale wake. Re-park -- nothing was
            // consumed, our registration is still in `wakers` and our timer
            // entry is still armed.
            None => {
                drop(guard);
                continue;
            }
        };
        drop(guard);
        cancel.store(true, Ordering::Relaxed);
        lock_mutex(wakers).retain(|(t, _)| *t != token);
        return outcome;
    }
}

/// Blocks (optionally with a `timeout_ms`) until `cell` resolves, then
/// re-propagates its outcome: `Ok(v)` on success, or the *original*
/// `RjError` (cloned out of the cell -- see `error.rs`'s `Clone` doc) on
/// failure, so `(try (deref f) (catch e ...))` sees the same thing a direct
/// (non-threaded) call to the future's fn would have raised. On timeout
/// (only reachable when `timeout_ms.is_some()`), returns `default` (or
/// `nil` if none was given) without waiting further, per Clojure's
/// `(deref f timeout-ms timeout-val)`.
///
/// # Two worlds (L3/W2b)
///
/// A THREAD blocks on `cell.cv`, exactly as it always has -- the code below
/// is byte-for-byte the pre-W2b function. A TASK must not: `cv_wait` parks
/// the SHARD THREAD, so a `go` block deref'ing an unresolved future would
/// freeze every other task placed on that shard until the future resolves
/// (the L1-class gap `builtins::flow`'s `flow/inject` doc reported in W2).
/// Tasks therefore route to [`future_deref_task`] (wake-driven) or
/// [`future_deref_task_timeout`] (bounded poll).
///
/// # Ordering proof (both cells, both task arms)
///
/// The invariant: **a task that parks is always woken.** Two actors, one
/// serialization point -- the cell's `state` mutex.
///
/// - WAITER: lock `state` -> observe unresolved -> push `(token, waker)`
///   into `task_wakers` *while still holding the `state` lock* -> drop the
///   `state` lock -> `park_current_yield()`.
/// - RESOLVER ([`resolve_future`] / [`deliver_promise`]): lock `state` ->
///   write the terminal state -> drop the `state` lock -> drain
///   `task_wakers` -> `wake()` each drained waker.
///
/// Both actors take the `state` mutex, so one of them is first:
///
/// 1. **Resolver first.** It writes the terminal state before the waiter
///    ever acquires the lock. The waiter then observes `Done`/`Failed`/
///    `Delivered` on its very first look and returns without parking. The
///    resolver's drain finding an empty list is correct, not a lost wake.
/// 2. **Waiter first.** Its waker is in the list before it releases the
///    `state` lock, and the resolver cannot write the state until it gets
///    that lock -- so the resolver's later drain necessarily sees the
///    waker and wakes it. The waiter may still be between `drop(guard)` and
///    `park_current_yield()` when that `wake()` lands: harmless, because
///    `TaskWaker::wake` on a RUNNING task leaves it NOTIFIED and the shard
///    re-queues it at the park boundary instead of suspending it (the
///    runtime's missed-wakeup arm; `Doorbell::wait_for_change_task` leans on
///    the identical window).
///
/// Corollaries the code depends on:
///
/// - **Stale wakers are harmless.** A woken task re-checks the state and,
///   if a wake reached it for an unrelated reason, retracts its own token
///   by identity. Even if it did not, waking an already-running or
///   already-finished task is a documented no-op (`TaskWaker::wake`).
/// - **Spurious wakes are legal.** Every arm below re-reads the cell on
///   resume and re-registers with a FRESH token if it is still unresolved,
///   so a wake that carried no information costs one scheduler pass.
/// - **A drained waker always fires.** The drain and the `wake()` calls are
///   in the same function with nothing fallible between them; there is no
///   path that removes a waker without waking it.
/// - **No lock inversion.** The waiter takes `state` then `task_wakers`;
///   the resolver takes `state`, releases it, and only then takes
///   `task_wakers`. Nothing ever takes `task_wakers` before `state`, and
///   nothing calls `wake()` under either lock.
///
/// # Both task arms are wake-driven (L3.5 item 1)
///
/// W2b shipped the timeout arm as a bounded 1ms poll, on the reasoning that
/// a task park has no deadline of its own and combining "wake me on the
/// cell" with "wake me at T" needed machinery this module had no business
/// growing. `tests/l35_deref_park_probe.rs` then measured the poll and it
/// was not bounded in the way that mattered: N parked timeout-derefs cost N
/// x 1000 timer arms per second through one global mutex, 3.99 CPU cores at
/// N=1000 and 11.9 at N=10000, with every other task's tick stretched 6.5x.
///
/// So the machinery got built, and it is smaller than the poll was: the
/// shared timer service grew a second entry kind that wakes a TASK instead
/// of closing a chan (`builtins::async`'s `timer_arm_waker`), and
/// [`task_timeout_park`] registers on the cell exactly as the no-timeout arm
/// does, arms exactly ONE such entry, and parks ONCE. A waiting
/// timeout-deref now costs what a waiting no-timeout deref costs: nothing.
/// What `deref` of a failed future throws: `java.util.concurrent.ExecutionException`
/// (message `cause.toString()`) with the original error as its cause. The
/// place (span, stack) stays the original's, so error reports still point at the
/// failing code. An interrupt passes through untouched.
fn execution_exception(e: &RjError) -> RjError {
    use crate::error::ErrorKind;
    if matches!(e.kind, ErrorKind::Interrupted | ErrorKind::InterruptedHard) {
        return e.clone();
    }
    // Mova lets any value be thrown; the JVM cannot, so there is nothing to wrap.
    if e.kind == ErrorKind::Thrown && !matches!(e.thrown, Some(Value::Inst(_)) | Some(Value::Map(_))) {
        return e.clone();
    }
    let cause = crate::errinfo::exception_value(e);
    let text = crate::printer::display_str(&cause);
    let chain: Vec<String> = ["java.util.concurrent.ExecutionException", "java.lang.Exception", "java.lang.Throwable"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let wrapper = crate::errinfo::mk_exception(&chain, Some(text), cause, Value::Nil);
    let mut w = e.clone();
    w.kind = ErrorKind::Thrown;
    w.thrown = Some(wrapper);
    w
}

pub fn future_deref(cell: &Arc<FutureCell>, timeout_ms: Option<u64>, default: Option<Value>) -> Result<Value, RjError> {
    let r = future_deref_inner(cell, timeout_ms, default);
    if cell.wrap_failures {
        return r.map_err(|e| execution_exception(&e));
    }
    r
}

fn future_deref_inner(cell: &Arc<FutureCell>, timeout_ms: Option<u64>, default: Option<Value>) -> Result<Value, RjError> {
    if crate::runtime::in_task() {
        return match timeout_ms {
            None => future_deref_task(cell),
            Some(ms) => future_deref_task_timeout(cell, ms, default),
        };
    }
    let mut guard = lock_mutex(&cell.state);
    loop {
        match &*guard {
            FutureState::Pending => {}
            FutureState::Done(v) => return Ok(v.clone()),
            FutureState::Failed(e) => return Err(e.clone()),
        }
        match timeout_ms {
            None => {
                let (g, aborted) = crate::interrupt::cv_wait_intr(&cell.cv, guard);
                guard = g;
                if aborted {
                    return Err(RjError::interrupted("java.lang.InterruptedException"));
                }
            }
            Some(ms) => {
                let (g, timed_out) = cv_wait_timeout(&cell.cv, guard, Duration::from_millis(ms));
                guard = g;
                if timed_out {
                    return match &*guard {
                        FutureState::Pending => Ok(default.unwrap_or(Value::Nil)),
                        FutureState::Done(v) => Ok(v.clone()),
                        FutureState::Failed(e) => Err(e.clone()),
                    };
                }
            }
        }
    }
}

/// [`future_deref`]'s no-timeout TASK arm: wake-driven, no deadline, no
/// safety net -- the landing stance for every task park in this runtime.
/// See [`future_deref`]'s ordering proof for why the park cannot be missed.
fn future_deref_task(cell: &Arc<FutureCell>) -> Result<Value, RjError> {
    loop {
        let guard = lock_mutex(&cell.state);
        match &*guard {
            FutureState::Done(v) => return Ok(v.clone()),
            FutureState::Failed(e) => return Err(e.clone()),
            // Still pending: register + park with the state lock live, so
            // the resolver cannot slip between the check and the push.
            FutureState::Pending => register_and_park(guard, &cell.task_wakers),
        }
    }
}

/// [`future_deref`]'s WITH-timeout TASK arm: the no-timeout arm's single
/// wake-driven park, plus a deadline ([`task_timeout_park`], where the
/// protocol and its no-lost-wake argument live).
///
/// Returns the same three outcomes the thread arm does, including the
/// deadline-race one: a cell that resolved while the deadline wake was in
/// flight reports its value, not `default`.
fn future_deref_task_timeout(cell: &Arc<FutureCell>, ms: u64, default: Option<Value>) -> Result<Value, RjError> {
    match task_timeout_park(&cell.state, &cell.task_wakers, ms, |s| match s {
        FutureState::Done(v) => Some(Ok(v.clone())),
        FutureState::Failed(e) => Some(Err(e.clone())),
        FutureState::Pending => None,
    }) {
        Some(outcome) => outcome,
        None => Ok(default.unwrap_or(Value::Nil)),
    }
}

/// [`promise_deref`]'s no-timeout TASK arm -- [`future_deref_task`] with one
/// less terminal state.
fn promise_deref_task(cell: &Arc<PromiseCell>) -> Value {
    loop {
        let guard = lock_mutex(&cell.state);
        match &*guard {
            PromiseState::Delivered(v) => return v.clone(),
            PromiseState::Pending => register_and_park(guard, &cell.task_wakers),
        }
    }
}

/// [`promise_deref`]'s WITH-timeout TASK arm -- see
/// [`future_deref_task_timeout`].
fn promise_deref_task_timeout(cell: &Arc<PromiseCell>, ms: u64, default: Option<Value>) -> Value {
    task_timeout_park(&cell.state, &cell.task_wakers, ms, |s| match s {
        PromiseState::Delivered(v) => Some(v.clone()),
        PromiseState::Pending => None,
    })
    .unwrap_or_else(|| default.unwrap_or(Value::Nil))
}

/// `.get`/`.getAsBoolean`/`.getAsInt`/`.getAsLong`/`.getAsDouble` on an
/// `Atom` or `Delay` receiver (S7 tail wave, measured against the oracle:
/// `clojure.lang.Atom`/`clojure.lang.Delay` unconditionally implement
/// `java.util.function.{Supplier,BooleanSupplier,IntSupplier,LongSupplier,
/// DoubleSupplier}` -- structurally, at the CLASS level, regardless of the
/// cell's current value; only calling the wrong accessor for that value's
/// shape fails, exactly like a JVM unboxing/`ClassCastException` would --
/// see `crate::types::builtin_classes`' `is_supplier`/`is_*supplier`
/// entries, which is what makes `instance?` say `true` unconditionally
/// too). `.getAsBoolean` measured to use CLOJURE truthiness, not `Boolean`
/// unboxing (`(.getAsBoolean (delay nil))` => `false`, not an NPE) --
/// `RT.booleanCast`, not a `(Boolean) x` cast.
///
/// `None` if `field` isn't one of these five names (falls through to the
/// caller's usual unresolved-symbol error) or `target` isn't an
/// `Atom`/`Delay`.
pub fn supplier_dot_method(interp: &mut Interp, field: &str, target: &Value) -> Option<Result<Value, RjError>> {
    let current = match target {
        Value::Atom(cell) => lock_mutex(&cell.state).1.clone(),
        Value::Delay(cell) => match force_delay(interp, cell) {
            Ok(v) => v,
            Err(e) => return Some(Err(e)),
        },
        _ => return None,
    };
    Some(match field {
        "get" => Ok(current),
        "getAsBoolean" => Ok(Value::Bool(current.truthy())),
        "getAsInt" => {
            crate::builtins::numbers::cast_integral(&current, "int", i32::MIN as i64, i32::MAX as i64).map(Value::Int)
        }
        "getAsLong" => crate::builtins::numbers::cast_integral(&current, "long", i64::MIN, i64::MAX).map(Value::Int),
        "getAsDouble" => crate::builtins::numbers::widen_to_f64(&current, "double").map(Value::Float),
        _ => return None,
    })
}

/// Same shape as [`future_deref`] but for promises (no error state --
/// delivery is infallible), task arms and all: see that function's ordering
/// proof, which covers this cell verbatim ([`deliver_promise`] is the
/// resolver, and follows the same set-state / drop-lock / drain / wake
/// sequence).
pub fn promise_deref(cell: &Arc<PromiseCell>, timeout_ms: Option<u64>, default: Option<Value>) -> Result<Value, RjError> {
    if crate::runtime::in_task() {
        return Ok(match timeout_ms {
            None => promise_deref_task(cell),
            Some(ms) => promise_deref_task_timeout(cell, ms, default),
        });
    }
    let mut guard = lock_mutex(&cell.state);
    loop {
        if let PromiseState::Delivered(v) = &*guard {
            return Ok(v.clone());
        }
        match timeout_ms {
            None => {
                let (g, aborted) = crate::interrupt::cv_wait_intr(&cell.cv, guard);
                guard = g;
                if aborted {
                    return Err(RjError::interrupted("java.lang.InterruptedException"));
                }
            }
            Some(ms) => {
                let (g, timed_out) = cv_wait_timeout(&cell.cv, guard, Duration::from_millis(ms));
                guard = g;
                if timed_out {
                    return match &*guard {
                        PromiseState::Delivered(v) => Ok(v.clone()),
                        PromiseState::Pending => Ok(default.unwrap_or(Value::Nil)),
                    };
                }
            }
        }
    }
}

/// Forces `cell` "in the calling thread" (unlike `future*`, `delay` never
/// spawns): fast path returns the memoized `result` if already forced;
/// otherwise holds `cell.f`'s lock for the *entire* call into `interp`,
/// which both (a) serializes concurrent `force`s of the *same* delay from
/// different threads -- the second blocks on this same lock until the first
/// resolves (having published `result`, `Ok` or `Err`, and cleared `f`),
/// and (b) needs no extra `Condvar`: the lock itself is the wait mechanism.
///
/// Caches `Err` exactly like `Ok` (S7 tail wave, measured: real
/// `clojure.lang.Delay` re-throws the SAME exception instance on every
/// `deref` after a failing thunk, it never retries -- see `DelayCell::
/// result`'s doc for the transcript). So a failing thunk runs at most
/// once per cell, same as a succeeding one.
pub fn force_delay(interp: &mut Interp, cell: &Arc<DelayCell>) -> Result<Value, RjError> {
    // Wait-free fast path -- see `DelayCell::result`'s doc for why this
    // is an `OnceLock` read, not a mutex.
    if let Some(r) = cell.result.get() {
        // field4/W-LENS-1: re-forcing a delay that FAILED deep-clones the
        // whole `RjError` (span, label, stack, thrown value) every time.
        // Measured at ~3.7% of delays.clj's pie, and the trigger for the
        // declined exception-cost items; the success arm clones a `Value`
        // and is not a regret.
        if r.is_err() {
            crate::lens::event(crate::lens::Event::DelayErrorReclone);
        }
        return r.clone();
    }
    let mut f_guard = lock_mutex(&cell.f);
    // Re-check: another thread may have finished computing (and released
    // `f_guard`) between our fast-path check above and acquiring this lock.
    if let Some(r) = cell.result.get() {
        return r.clone();
    }
    let Some(f) = f_guard.clone() else {
        // `f` is only ever `None` once `result` is set (checked above);
        // defensive fallback, not reachable in practice.
        return cell.result.get().cloned().unwrap_or(Ok(Value::Nil));
    };
    let computed = interp.call(&f, &[]);
    let _ = cell.result.set(computed.clone());
    *f_guard = None;
    computed
}
