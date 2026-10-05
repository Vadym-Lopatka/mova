//! L3/W2b gate tests: `deref` on a future/promise from inside a TASK
//! (docs/L3-LANDING-SPEC.md §W2b).
//!
//! W2's FINDING was that `builtins::conc::future_deref` (and `promise_deref`,
//! and `hostclass`'s `.join`, which reuses the same cell) had exactly one
//! arm -- a raw `Condvar` wait. A `go` block deref'ing an unresolved future
//! therefore parked the SHARD THREAD, not the task: every other task placed
//! on that shard stopped dead until the future resolved. W2b gives those
//! functions task arms (wake-driven with no timeout, bounded-poll with one)
//! and makes every resolver drain the cell's task wakers.
//!
//! L3.5 item 1 then replaced the bounded poll with a single park woken by
//! whichever of {cell resolution, deadline} comes first -- see this file's
//! "the timeout arm is ONE park" section, which owns that wave's gates. The
//! W2b tests below are unchanged and still the starvation differential.
//!
//! ## The starvation differential is the point
//!
//! Result-correctness tests (`(deref f)` in a `go` returns the value) pass
//! BOTH before and after the fix -- the pre-fix version was slow and
//! shard-hostile, never wrong. So the load-bearing assertion in this file is
//! the differential: while one task waits on a pending cell, its SHARD
//! SIBLINGS must still run. Every differential test therefore reports two
//! numbers, and asserts on both:
//!
//! - `sib` -- when the sibling tasks got to run, in ms from `t0`. Must be
//!   promptly, i.e. long BEFORE the cell resolves.
//! - `deref` -- when the deref'ing task came back. Must be ~the resolution
//!   time, proving it really did wait rather than spin or return early.
//!
//! Measured on this tree while writing the wave, with the task arm
//! deliberately disabled (`if false && in_task()`) and a 400ms resolution:
//! `[:resolved 405 405]` -- the siblings ran only AFTER the future landed.
//! With the arm enabled: `[:resolved 402 0]`. That gap is the whole wave.
//!
//! ## Why co-shard placement is deterministic here
//!
//! A task spawned from INSIDE a task prefers its parent's shard while that
//! shard is under `SPAWN_LOCAL_MAX` (=8) live tasks (`runtime::pick_shard`,
//! W4b family-local placement -- the same lever `tests/
//! task_runtime_go_test.rs`'s G3 leans on for its co-shard measurement). So
//! every differential test wraps its `go` blocks in ONE coordinator `go`,
//! keeping the family well under the flood guard (1 coordinator + 1 deref +
//! ≤4 siblings = 6). Spawning them from the test's own OS thread instead
//! would round-robin them across shards and the differential would be
//! meaningless. If placement ever DID spill, these tests would go green
//! spuriously, never red -- they cannot become flaky in the failing
//! direction.
//!
//! Timing bars are deliberately loose (a 150ms bar on work that measures 0-1
//! ms, against a 400ms resolution): the claim under test is a 400ms-scale
//! starvation, so nothing here is asking the clock a subtle question.

use std::sync::Mutex;

use mova::embed::{Engine, Value};

/// These tests read the wall clock against tasks sharing process-wide
/// shards, so they run one at a time within this binary -- a neighbour's
/// task family landing on the same shard would add scheduling noise to
/// exactly the number being asserted on. (Other test binaries are separate
/// processes with their own shards.)
static SERIAL: Mutex<()> = Mutex::new(());

/// Take `SERIAL`, recovering from a poisoned lock: one failing test must not
/// cascade-fail its siblings just because it held the mutex when it panicked
/// (`tests/task_runtime_go_test.rs`'s `serial_lock`, same reasoning).
fn serial_lock() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

fn eval_ok(src: &str) -> Value {
    Engine::builder()
        .build()
        .eval_named("l3-future-task", src)
        .unwrap_or_else(|e| panic!("eval error: {}", e.render_plain()))
}

fn ps(src: &str) -> String {
    eval_ok(src).to_string()
}

fn items(v: &Value) -> Vec<Value> {
    v.iter().collect()
}

fn as_int(v: &Value) -> i64 {
    v.as_i64().unwrap_or_else(|| panic!("expected an int, got {v:?}"))
}

/// How long every differential test's cell stays pending.
const RESOLVE_MS: i64 = 400;
/// Siblings must have run by here -- generous against their ~0ms real cost,
/// and far enough below [`RESOLVE_MS`] that "they only ran once the cell
/// resolved" (the pre-W2b behavior) cannot sneak under it.
const SIBLING_BAR_MS: i64 = 150;
/// The deref'ing task must not come back before here: it really waited.
const WAITED_BAR_MS: i64 = RESOLVE_MS - 100;

/// `[value deref-ms sibling-max-ms]` from a differential run.
fn differential(src: &str) -> (String, i64, i64) {
    let result = eval_ok(src);
    let it = items(&result);
    assert_eq!(it.len(), 3, "expected [value deref-ms sib-ms], got {result:?}");
    (it[0].to_string(), as_int(&it[1]), as_int(&it[2]))
}

// ---------------------------------------------------------------------------
// THE differential: a parked task must not hold its shard
// ---------------------------------------------------------------------------

#[test]
fn future_deref_in_a_go_block_parks_instead_of_starving_its_shard() {
    let _g = serial_lock();
    let (value, derefed, sib) = differential(
        r#"(let [f (future (sleep-ms 400) :resolved)
                 sibs (chan 16)
                 res (chan 1)
                 t0 (time-ms)]
             (go
               (go (>! res [(deref f) (- (time-ms) t0)]))
               (go (>! sibs (- (time-ms) t0)))
               (go (>! sibs (- (time-ms) t0)))
               (go (>! sibs (- (time-ms) t0)))
               (go (>! sibs (- (time-ms) t0))))
             (let [a (<!! sibs) b (<!! sibs) c (<!! sibs) d (<!! sibs)
                   r (<!! res)]
               [(first r) (second r) (max a b c d)]))"#,
    );
    assert_eq!(value, ":resolved", "the task arm must return the future's value");
    assert!(
        sib < SIBLING_BAR_MS,
        "shard STARVED: siblings co-placed with the deref'ing task first ran at {sib}ms \
         (bar {SIBLING_BAR_MS}ms) -- a task parked in `deref` is holding its shard thread \
         (deref returned at {derefed}ms)"
    );
    assert!(
        derefed >= WAITED_BAR_MS,
        "the deref'ing task returned at {derefed}ms but the future resolves at {RESOLVE_MS}ms \
         -- it did not actually wait for the cell"
    );
}

#[test]
fn promise_deref_in_a_go_block_parks_instead_of_starving_its_shard() {
    let _g = serial_lock();
    let (value, derefed, sib) = differential(
        r#"(let [p (promise)
                 _ (future (sleep-ms 400) (deliver p :delivered))
                 sibs (chan 16)
                 res (chan 1)
                 t0 (time-ms)]
             (go
               (go (>! res [(deref p) (- (time-ms) t0)]))
               (go (>! sibs (- (time-ms) t0)))
               (go (>! sibs (- (time-ms) t0)))
               (go (>! sibs (- (time-ms) t0)))
               (go (>! sibs (- (time-ms) t0))))
             (let [a (<!! sibs) b (<!! sibs) c (<!! sibs) d (<!! sibs)
                   r (<!! res)]
               [(first r) (second r) (max a b c d)]))"#,
    );
    assert_eq!(value, ":delivered");
    assert!(
        sib < SIBLING_BAR_MS,
        "shard STARVED by a promise deref: siblings first ran at {sib}ms (bar {SIBLING_BAR_MS}ms, \
         deref returned at {derefed}ms)"
    );
    assert!(derefed >= WAITED_BAR_MS, "promise deref returned early, at {derefed}ms");
}

/// The WITH-timeout arm is a bounded poll rather than a wake-driven park
/// (`future_deref`'s "Asymmetry" note), so it has its OWN way to starve a
/// shard: a poll that spun, or slept the thread between checks, would hold
/// the shard just as hard as the condvar did. Same differential, plus the
/// timeout's own semantics (`:timed-out` at ~60ms, not the value at 400ms).
#[test]
fn future_deref_with_timeout_in_a_go_block_yields_the_shard_between_polls() {
    let _g = serial_lock();
    let (value, derefed, sib) = differential(
        r#"(let [f (future (sleep-ms 400) :late)
                 sibs (chan 16)
                 res (chan 1)
                 t0 (time-ms)]
             (go
               (go (>! res [(deref f 60 :timed-out) (- (time-ms) t0)]))
               (go (>! sibs (- (time-ms) t0)))
               (go (>! sibs (- (time-ms) t0))))
             (let [a (<!! sibs) b (<!! sibs)
                   r (<!! res)]
               [(first r) (second r) (max a b)]))"#,
    );
    assert_eq!(value, ":timed-out", "a 60ms deref of a 400ms future must time out");
    assert!(
        sib < SIBLING_BAR_MS,
        "shard STARVED by a polling deref: siblings first ran at {sib}ms (bar {SIBLING_BAR_MS}ms)"
    );
    assert!(
        (60..250).contains(&derefed),
        "the timeout arm returned at {derefed}ms; expected ~60ms (its budget), never before it \
         and never near the 400ms resolution"
    );
}

// ---------------------------------------------------------------------------
// Result semantics through the task arms
// ---------------------------------------------------------------------------

/// The `Failed` arm: a future whose body threw must re-raise on a TASK-side
/// deref exactly as it does on a thread-side one (`tests/conc_test.rs`'s
/// `future_propagates_thrown_error_catchable`, moved into a `go`).
#[test]
fn future_deref_in_a_go_block_propagates_a_thrown_error() {
    assert_eq!(
        ps(r#"(let [f (future (sleep-ms 50) (throw :boom))
                    res (chan 1)]
                (go (>! res (try (deref f) (catch e [:caught e]))))
                (<!! res))"#),
        "[:caught :boom]"
    );
}

/// Case 1 of the ordering proof (resolver first): a cell that is ALREADY
/// resolved when the task looks must return on the first check, without
/// registering a waker and without parking. Observable as "instant".
#[test]
fn deref_of_an_already_resolved_cell_in_a_go_block_returns_immediately() {
    let result = eval_ok(
        r#"(let [f (future :done)
                 p (promise)
                 _ (deliver p :already)
                 res (chan 1)
                 _ (sleep-ms 50)
                 t0 (time-ms)]
             (go (>! res [(deref f) (deref p) (- (time-ms) t0)]))
             (<!! res))"#,
    );
    let it = items(&result);
    assert_eq!(it[0].to_string(), ":done");
    assert_eq!(it[1].to_string(), ":already");
    let elapsed = as_int(&it[2]);
    assert!(elapsed < 100, "an already-resolved deref took {elapsed}ms -- it parked");
}

/// The drain wakes EVERY registered waiter, not just the first: six tasks
/// park on one promise and all six must come back (`wake_task_waiters` takes
/// the whole list). Six also exceeds a single shard's family guard, so this
/// spans shards as well.
#[test]
fn six_tasks_parked_on_one_promise_all_wake_on_a_single_deliver() {
    assert_eq!(
        ps(r#"(let [p (promise)
                    res (chan 8)
                    _ (dotimes [i 6] (go (>! res [(deref p) i])))
                    _ (future (sleep-ms 100) (deliver p :one-deliver))
                    seen (loop [n 0 acc []]
                           (if (= n 6)
                             acc
                             (recur (inc n) (conj acc (<!! res)))))]
                [(count seen) (count (distinct (map first seen))) (first (first seen))])"#),
        "[6 1 :one-deliver]"
    );
}

/// The resolver can itself be a TASK (`(go (deliver p ...))`), which routes
/// the wake through `TaskWaker::wake`'s same-shard/direct-switch path rather
/// than a cross-thread inject. Same contract -- and the shape where waking
/// under the wrong lock would bite hardest, hence `deliver`'s drain
/// happening strictly after the state lock is dropped.
#[test]
fn a_task_delivering_the_promise_wakes_a_task_parked_on_it() {
    assert_eq!(
        ps(r#"(let [p (promise)
                    res (chan 1)]
                (go (>! res [(deref p) :woken]))
                (go (deliver p :from-a-task))
                (<!! res))"#),
        "[:from-a-task :woken]"
    );
}

/// The timeout arm's other outcome: the cell resolves well inside the
/// budget, so the value comes back (not the default), promptly (not at the
/// deadline).
#[test]
fn future_deref_with_timeout_in_a_go_block_returns_the_value_when_it_lands() {
    let _g = serial_lock();
    let result = eval_ok(
        r#"(let [f (future (sleep-ms 120) :in-time)
                 res (chan 1)
                 t0 (time-ms)]
             (go (>! res [(deref f 5000 :timed-out) (- (time-ms) t0)]))
             (<!! res))"#,
    );
    let it = items(&result);
    assert_eq!(it[0].to_string(), ":in-time");
    let elapsed = as_int(&it[1]);
    assert!(
        (100..400).contains(&elapsed),
        "expected the value ~120ms in (the resolution), got it at {elapsed}ms"
    );
}

/// Promise, timeout arm, default path.
#[test]
fn promise_deref_with_timeout_in_a_go_block_returns_the_default() {
    let _g = serial_lock();
    assert_eq!(
        ps(r#"(let [p (promise)
                    res (chan 1)]
                (go (>! res (deref p 40 :nothing-yet)))
                (<!! res))"#),
        ":nothing-yet"
    );
    // ... and `nil` when the default is explicitly `nil`, same as the thread
    // arm. (`deref` is 1-or-3-arity here, as in Clojure -- there is no
    // 2-arity "timeout with an implicit nil default" spelling to test.)
    assert_eq!(
        ps(r#"(let [p (promise) res (chan 1)]
                (go (>! res [(deref p 40 nil)]))
                (<!! res))"#),
        "[nil]"
    );
}

// ---------------------------------------------------------------------------
// L3.5 item 1: the timeout arm is ONE park, not a 1ms poll
// ---------------------------------------------------------------------------
//
// W2b shipped the WITH-timeout task arm as a bounded 1ms `park_tick` loop.
// `tests/l35_deref_park_probe.rs` priced it: 1000 tasks parked in `(deref p
// 120000 :x)` burned 3.99 CPU cores and 10000 burned 11.9, stretching every
// other task's tick 6.5x, because each waiter re-armed the ONE global timer
// heap a thousand times a second. `builtins::conc::task_timeout_park` now
// registers on the cell and arms ONE timer entry that wakes the task at the
// deadline, so a waiting timeout-deref costs what a waiting no-timeout deref
// costs: nothing.
//
// The behavioral tests below would have passed before the change too (the
// poll was wasteful, never wrong). The load-bearing one is
// `timer_arms_are_one_per_deref_call`, which counts heap arms through
// `mova::internal::async_timer::arms_total`: it is the tripwire that says
// the poll is gone and stays gone.

/// The deadline half, on a cell that NEVER resolves: the default comes back
/// at the deadline (not before it, and not a poll-interval or a scheduler
/// era after it), and the siblings sharing the shard ran the whole time.
#[test]
fn promise_deref_timeout_fires_at_its_deadline_on_a_cell_that_never_resolves() {
    let _g = serial_lock();
    let (value, derefed, sib) = differential(
        r#"(let [p (promise)
                 sibs (chan 16)
                 res (chan 1)
                 t0 (time-ms)]
             (go
               (go (>! res [(deref p 50 :timed-out) (- (time-ms) t0)]))
               (go (>! sibs (- (time-ms) t0)))
               (go (>! sibs (- (time-ms) t0))))
             (let [a (<!! sibs) b (<!! sibs)
                   r (<!! res)]
               [(first r) (second r) (max a b)]))"#,
    );
    assert_eq!(value, ":timed-out", "nothing ever delivers this promise");
    assert!(
        (50..250).contains(&derefed),
        "a 50ms timeout deref returned at {derefed}ms -- before its deadline, or nowhere near it"
    );
    assert!(
        sib < SIBLING_BAR_MS,
        "shard STARVED by a parked timeout deref: siblings first ran at {sib}ms (bar {SIBLING_BAR_MS}ms)"
    );
}

/// **The regression test for the poll.** One `(deref f 5000 :x)` that waits
/// ~300ms must push EXACTLY ONE entry onto the shared timer heap. The old
/// 1ms poll pushed one per millisecond of waiting -- ~300 here -- so this
/// assertion fails by two orders of magnitude if the poll ever comes back.
///
/// White-box through `mova::internal::async_timer::arms_total`, which is
/// process-wide, hence `serial_lock` (and hence the lock added to every
/// other test in this file that can arm the heap: the timeout derefs and the
/// `flow` one).
#[test]
fn timer_arms_are_one_per_deref_call() {
    let _g = serial_lock();
    let before = mova::internal::async_timer::arms_total();
    let result = eval_ok(
        r#"(let [f (future (sleep-ms 300) :in-time)
                 res (chan 1)
                 t0 (time-ms)]
             (go (>! res [(deref f 5000 :timed-out) (- (time-ms) t0)]))
             (<!! res))"#,
    );
    let arms = mova::internal::async_timer::arms_total() - before;
    let it = items(&result);
    assert_eq!(it[0].to_string(), ":in-time", "resolution must beat the 5s deadline");
    let elapsed = as_int(&it[1]);
    assert!(
        (250..600).contains(&elapsed),
        "the value came back at {elapsed}ms; the future resolves at 300ms and the deadline is 5000ms"
    );
    assert_eq!(
        arms, 1,
        "a single timeout-deref armed {arms} timer entries over a ~300ms wait; it must arm exactly \
         1. The W2b poll armed one per millisecond (~300) -- if this is in the hundreds, the poll \
         is back"
    );
}

/// The stale-deadline case, which is the one the cancel token exists for: a
/// deref whose cell resolves early returns at the resolution, and its timer
/// entry stays in the heap (cancellation is LAZY, by design) until its own
/// deadline -- by which time the task that armed it has not only returned
/// from the deref but FINISHED and had its slab slot freed for reuse.
///
/// Nothing may come of that firing. The token makes the timer skip the wake;
/// and even if it did not, `TaskWaker::wake` on a `DONE` task fails its
/// `PARKED -> READY` CAS and returns (task IDENTITY is the authority, not
/// the slot, so a reused slot cannot be woken by another task's stale
/// waker). This test keeps the process running past the deadline and then
/// keeps using the runtime.
#[test]
fn a_deadline_that_comes_due_after_its_deref_finished_is_a_no_op() {
    let _g = serial_lock();
    assert_eq!(
        ps(r#"(let [p (promise)
                    res (chan 1)]
                (future (sleep-ms 20) (deliver p :early))
                (go (>! res (deref p 300 :nope)))
                (<!! res))"#),
        ":early"
    );
    // Past the armed deadline, with the task long gone.
    std::thread::sleep(std::time::Duration::from_millis(400));
    // Fresh tasks, fresh slab slots -- some of them the finished task's.
    assert_eq!(
        ps(r#"(let [res (chan 1)]
                (go (>! res :still-scheduling))
                (<!! res))"#),
        ":still-scheduling"
    );
}

/// The other stale case: the deref TIMED OUT and returned its default, and
/// the cell resolves afterwards. The late resolver drains the cell's waker
/// list; the timed-out task must not be in it (it retracts its token on the
/// way out), it must not be handed the value it already declined, and a
/// LATER deref of the same cell -- from the same task, after an unrelated
/// park -- must see the delivered value.
#[test]
fn a_resolution_after_a_timed_out_deref_leaks_nothing() {
    let _g = serial_lock();
    assert_eq!(
        ps(r#"(let [p (promise)
                    res (chan 2)]
                (future (sleep-ms 200) (deliver p :late))
                (go
                  (>! res (deref p 40 :timed-out))
                  ;; An unrelated park, which a leaked registration or an
                  ;; uncancelled deadline would be free to disturb.
                  (<! (timeout 300))
                  (>! res (deref p 1000 :never-landed)))
                [(<!! res) (<!! res)])"#),
        "[:timed-out :late]"
    );
}

/// A zero-length budget is answered from the fast path: no waker
/// registration, no timer entry, no park at all. (Clojure's `deref` accepts
/// it; the shape that must not happen is arming a 0ms timer and taking a
/// scheduler round trip to learn what the first look already knew.)
#[test]
fn a_zero_timeout_deref_returns_the_default_without_arming_anything() {
    let _g = serial_lock();
    let before = mova::internal::async_timer::arms_total();
    assert_eq!(
        ps(r#"(let [p (promise) res (chan 1)]
                (go (>! res (deref p 0 :instant-default)))
                (<!! res))"#),
        ":instant-default"
    );
    assert_eq!(
        mova::internal::async_timer::arms_total() - before,
        0,
        "a 0ms timeout armed a timer entry"
    );
    // ... and a 0ms budget on an ALREADY resolved cell still reports the
    // value, not the default: the resolved check comes first.
    assert_eq!(
        ps(r#"(let [p (promise) res (chan 1)]
                (deliver p :there-all-along)
                (go (>! res (deref p 0 :instant-default)))
                (<!! res))"#),
        ":there-all-along"
    );
}

/// Repeated timeout derefs of the SAME cell each arm their own entry with
/// their own token and their own cancel flag -- three waiters on one promise
/// that resolves once, three arms, three correct answers.
#[test]
fn three_concurrent_timeout_derefs_of_one_cell_arm_three_entries() {
    let _g = serial_lock();
    let before = mova::internal::async_timer::arms_total();
    assert_eq!(
        ps(r#"(let [p (promise)
                    res (chan 4)]
                (future (sleep-ms 120) (deliver p :one))
                (go
                  (go (>! res (deref p 5000 :a-timed-out)))
                  (go (>! res (deref p 5000 :b-timed-out)))
                  (go (>! res (deref p 5000 :c-timed-out))))
                (let [a (<!! res) b (<!! res) c (<!! res)]
                  [a b c]))"#),
        "[:one :one :one]"
    );
    let arms = mova::internal::async_timer::arms_total() - before;
    assert_eq!(arms, 3, "three timeout derefs armed {arms} entries; expected one each");
}

// ---------------------------------------------------------------------------
// The two other users of these cells
// ---------------------------------------------------------------------------

/// `(Thread. f)`'s `.start`/`.join` cell IS a `FutureCell` (`hostclass.rs`),
/// so `.join` inherits the task arm for free -- provided `.start`'s body
/// resolves through `conc::resolve_future` rather than storing the state and
/// ringing only the condvar. This test fails if that resolver was missed:
/// the joining task would never be woken at all and `<!!` would hang.
#[test]
fn thread_join_in_a_go_block_inherits_the_future_task_arm() {
    let _g = serial_lock();
    let result = eval_ok(
        r#"(let [t (Thread. (fn [] (sleep-ms 400)))
                 sibs (chan 16)
                 res (chan 1)
                 t0 (time-ms)]
             (.start t)
             (go
               (go (.join t) (>! res (- (time-ms) t0)))
               (go (>! sibs (- (time-ms) t0)))
               (go (>! sibs (- (time-ms) t0))))
             (let [a (<!! sibs) b (<!! sibs)
                   r (<!! res)]
               [r (max a b)]))"#,
    );
    let it = items(&result);
    let joined = as_int(&it[0]);
    let sib = as_int(&it[1]);
    assert!(
        joined >= WAITED_BAR_MS,
        "`.join` in a go block returned at {joined}ms; the thread runs for {RESOLVE_MS}ms"
    );
    assert!(
        sib < SIBLING_BAR_MS,
        "shard STARVED by `.join`: siblings first ran at {sib}ms (bar {SIBLING_BAR_MS}ms)"
    );
}

/// W2's original report: `flow/inject` hands back a `Value::Future` resolved
/// by a one-shot injector OS THREAD, and awaiting it inside a `go` block was
/// the path that surfaced this whole gap. It must now resolve (the injector
/// goes through `conc::resolve_future` too) and answer `nil`, exactly like
/// `tests/flow_test.rs`'s thread-side
/// `inject_returns_a_future_that_resolves_after_the_puts_land`.
#[test]
fn flow_inject_future_awaited_in_a_go_block_resolves() {
    // Serialized for `timer_arms_are_one_per_deref_call`'s sake as much as
    // its own: `flow`'s stragglers arm the SHARED timer heap through their
    // own `park_tick`, and that test counts arms process-wide.
    let _g = serial_lock();
    assert_eq!(
        ps(r#"(let [step (flow/map->step
                           {:describe (fn [] {:ins {:in {}} :outs {}})
                            :transform (fn [s _ m] [s {}])})
                    fl (flow/create-flow {:procs {:p {:proc (flow/process step)}} :conns []})
                    _ (flow/start fl)
                    _ (flow/resume fl)
                    res (chan 1)
                    fut (flow/inject fl [:p :in] [1 2 3])]
                (go (>! res [:awaited (deref fut) (future? fut)]))
                (let [r (<!! res)]
                  (flow/stop fl)
                  r))"#),
        "[:awaited nil true]"
    );
}

// ---------------------------------------------------------------------------
// The thread arm is unchanged
// ---------------------------------------------------------------------------

/// The condvar arm still serves plain OS threads (this test's own thread is
/// one): `deref` blocks, returns the value, and honors a timeout's default.
/// `tests/conc_test.rs` is the real home of that coverage -- this is the
/// tripwire for "W2b's `in_task()` dispatch accidentally captured everyone".
#[test]
fn deref_from_a_plain_thread_still_takes_the_condvar_arm() {
    assert_eq!(
        ps(r#"[(deref (future (sleep-ms 30) :v))
               (deref (promise) 30 :default)
               (let [p (promise)] (deliver p :d) (deref p))]"#),
        "[:v :default :d]"
    );
}
