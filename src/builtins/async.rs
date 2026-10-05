//! `core.async`-style channels (v0.2 / A2, tasks as of L1/W4): `chan`,
//! `dropping-buffer`, `sliding-buffer`, the blocking `>!!`/`<!!`, `close!`,
//! `timeout`, async `put!`/`take!` (task-backed, see below), `alts!!`/
//! `offer!`/`poll!`, the `go*` native `core/async.mova`'s `go`/`go-loop`
//! macros expand into, and `thread*`, the always-OS-thread native `thread`
//! expands into. `chan?` lives in `predicates.rs` (pure state inspection).
//!
//! ## Channel protocol (read this before touching `Chan`)
//!
//! `Value::Channel(Arc<Chan>)`'s `Chan { state: Mutex<ChanState>, cv:
//! Condvar }` (both defined in `value.rs`, mirroring how `FutureCell`/
//! `PromiseCell`/`DelayCell` split "data lives in value.rs, logic lives in
//! builtins" for exactly this shape). `ChanState.buffer` is a
//! `VecDeque<Value>` used two different ways depending on
//! `ChanState.policy`:
//!
//! - **`Fixed(n)`/`Dropping(n)`/`Sliding(n)`**: `buffer` is a genuine FIFO
//!   queue, capacity-bounded per the policy. `Dropping` silently discards a
//!   put once at capacity (put still "succeeds" -- Clojure's `dropping-
//!   buffer` never blocks a putter or reports failure); `Sliding` evicts the
//!   oldest buffered value to make room for the newest.
//! - **`Unbuffered`**: `buffer` holds AT MOST ONE value at a time and is
//!   (ab)used as a single-slot rendezvous cell rather than a queue. A
//!   blocking put (`chan_put`) is allowed to push its value into an empty
//!   slot unconditionally and then block until a taker empties it again (it
//!   does NOT require a taker to already be parked -- that mirrors real
//!   unbuffered-channel semantics: the putter "hands off" and waits). A
//!   NON-blocking put (`chan_try_put`, i.e. `offer!`/an `alts!!` put-op)
//!   must NOT leave an unconsumed value sitting in the slot forever, so it
//!   only succeeds when `waiting_takers > 0` -- i.e. some thread is already
//!   parked inside `chan_take`'s blocking wait, guaranteeing the value gets
//!   picked up essentially immediately.
//!
//! `waiting_takers`/`waiting_putters` count threads *currently parked*
//! inside `chan_take`/`chan_put`'s condvar wait (incremented right before
//! `cv_wait`, decremented right after it returns), not merely "interested".
//! **They are NOT a complete census of every thread parked on `ch.cv`** --
//! `builtins::flow`'s `send_with_control_priority` blocked-send retry loop
//! (used by BOTH of that module's proc loops) and `try_take_with_timeout`'s
//! single-input idle park (see that module's doc) call `cv_wait_timeout`
//! directly on a chan's condvar WITHOUT going through `chan_take`/
//! `chan_put` at all (`try_take_with_timeout` happens to increment
//! `waiting_takers` itself; the blocked-send loop does NOT touch
//! `waiting_putters`). A Phase F3
//! measure-first pass tried gating every `notify_all()` call below on
//! `waiting_takers > 0` / `waiting_putters > 0` (skip the wake syscall when
//! the counter says nobody's parked) and measured a ~2x THROUGHPUT
//! REGRESSION on the fan-out benchmark: that blocked-send loop's uncounted
//! park silently stopped being woken by `chan_take`'s notify, degrading
//! every backpressured send to its own 1ms `PARK_TIMEOUT` cap
//! instead of an near-instant wake. Reverted (see bench/optimization-log.md
//! for the full writeup) -- this counter pair must stay "used for the
//! `Unbuffered` try-put heuristic only", never repurposed as a
//! notify-elision oracle, unless every `cv_wait`/`cv_wait_timeout` call site
//! on a `Chan`'s condvar (including flow.rs's) is first made to participate
//! in it.
//!
//! They exist solely so `chan_try_put`'s `Unbuffered` case above can tell
//! "someone is actively blocked waiting for a value" apart from "no one is
//! listening right now".
//!
//! **If you add a chan-state mutation site outside this file, it owes the
//! same rings.** `builtins::flow`'s `try_take_with_timeout` is the one that
//! exists today: it does its own `buffer.pop_front()` and its own
//! `waiting_takers += 1` against a raw `ChanState` guard, so it is a 6th
//! site for every obligation the 5 below carry, and it discharges them
//! explicitly (see its body -- with one deliberate exception, spelled out
//! there, for ringing the very doorbell it is about to park on). Any future
//! site that touches `buffer`, `closed`, or `waiting_takers` owes them too:
//! all three are inputs to somebody's parked scan, and a mutation that
//! doesn't ring is a waiter that sleeps through it.
//!
//! **Doorbells.** Every one of the 5 `notify_all()` call sites in this file
//! (`chan_take`, `chan_put`, `chan_try_take`, `chan_try_put`, `chan_close`)
//! also rings the two doorbell registration slots on `ChanState`, in
//! addition to the chan's own condvar:
//!
//! - `ChanState.doorbell` -- a per-proc wake target `builtins::flow`
//!   registers on every chan currently in a proc's read-set (`value.rs`'s
//!   `Doorbell` doc has the full design). `None` for every chan outside the
//!   flow engine.
//! - `ChanState.alts_doorbells` -- one entry per blocking `alts!!` call
//!   currently parked on this chan (see "alts!!" below). Empty for every
//!   chan nobody is selecting over right now.
//!
//! Both are therefore no-ops on the hot path for ordinary
//! `chan`/`>!!`/`<!!` usage (a `None` branch and an empty-slice walk), and
//! `>!!`/`<!!` are otherwise entirely unchanged by their existence.
//!
//! `take` (blocking and non-blocking) always tries `buffer.pop_front()`
//! FIRST, checking `closed` only when the buffer is empty -- this is what
//! gives close! its "puts blocked at close return false; takes drain
//! whatever was already buffered, THEN return nil" semantics for free, with
//! no special-casing at the close! call site: `close!` just flips a flag and
//! notifies every waiter; each waiter's own loop decides what that means.
//!
//! A closed channel's put/take ops are always immediately "ready" per real
//! core.async (`chan_try_take`/`chan_try_put` return `Closed` rather than
//! `WouldBlock` once `closed` is set and nothing is left to drain) -- this
//! is exactly what lets `alts!!` resolve a closed channel's op instantly
//! instead of treating it as merely "not ready yet".
//!
//! ## `alts!!`: scan, then PARK on a doorbell
//!
//! Each `Chan` has its own private `Condvar` and a single `alts!!` call can
//! be selecting across channels it doesn't own, so there is no ONE condvar
//! an `alts!!` could wait on. v0 answered that with a **polling loop**:
//! scan every op, sleep 500µs, scan again -- ~2000 wakeups/s for any
//! blocked `alts!!`, i.e. a whole core burned per blocked selector, the
//! exact shape of the bug FLOW-IDLE-CPU-BUG.md fixed for the flow engine.
//!
//! It now uses the same cure: the missing cross-channel wait-group
//! primitive is a [`Doorbell`], and `alts!!` brings its own.
//!
//! 1. `:default` is unchanged and never parks: one scan, then the default.
//! 2. Otherwise the call creates ONE `Doorbell` **on the calling thread**
//!    (so its `owner_thread` capture is correct) and pushes it onto every
//!    op chan's `alts_doorbells` -- BEFORE the first scan.
//! 3. Then it loops: snapshot the doorbell's generation, run one
//!    freshly-shuffled fairness scan over the ops (identical per-op logic
//!    and return shapes to v0's, via the same non-blocking
//!    `chan_try_take`/`chan_try_put` primitives `poll!`/`offer!` use), and
//!    if nothing was ready, park on the doorbell until it rings.
//! 4. `AltsRegistration`'s `Drop` removes the registration on every exit
//!    path.
//!
//! Missed wakeups are impossible by the standard generation-counter
//! argument (`Doorbell`'s own doc spells it out): registration precedes the
//! first scan, and the snapshot precedes each scan, so an event landing
//! mid-scan has already bumped the generation and the park returns
//! immediately. `ALTS_PARK_TIMEOUT` (2s) is a defensive backstop only.
//!
//! Why `alts_doorbells` is a `Vec` and NOT the flow engine's single
//! `doorbell` slot: N threads can `alts!!` over one chan at once and every
//! one of them must wake, and a chan can be in a flow proc's read-set AND
//! in someone's `alts!!` simultaneously -- one slot would mean one
//! registrant silently clobbering the other.
//!
//! What a scan reads -- and therefore what a mutation owes a ring -- is
//! FIVE fields (three before L1/W3): `buffer`, `closed`, `waiting_takers`,
//! and the two task queues `task_putters`/`task_takers` (see "task parking"
//! below -- `chan_try_take` serves an unbuffered take out of `task_putters`,
//! and `chan_try_put`/`chan_put` serve any put into `task_takers`, so
//! ENQUEUEING on either is a readiness gain and owes a ring; DEQUEUEING is a
//! readiness loss and owes none). `waiting_takers` is read (via
//! `buffer.is_empty() && waiting_takers > 0`). `chan_take`'s
//! `waiting_takers += 1` is consequently a ring site in its own right, and
//! the only one that rings from UNDER the lock -- see the comment there.
//! The matching decrements ring nothing on purpose: they can only make an
//! op LESS ready, and a ring exists solely to say "re-check, something may
//! be ready now".
//!
//! ### `alts!!` and the unbuffered rendezvous (known limitation)
//!
//! Two `alts!!` calls facing each other across ONE UNBUFFERED chan -- one
//! offering a put op, one offering a take op, and nothing else in play --
//! never complete. `chan_try_put`'s unbuffered gate demands a taker already
//! parked inside `chan_take` (that is what makes a non-blocking put safe on
//! an unbuffered chan: the value cannot be left sitting unobserved), and an
//! `alts!!` take op does not park in `chan_take` -- it polls
//! `chan_try_take` and then parks on its own doorbell. So neither side ever
//! enters the handshake the other is waiting for.
//!
//! This is PRE-EXISTING, not introduced by the doorbell change: the same
//! pair deadlocked under the v0 poll loop, spinning at 500µs instead of
//! parking at 2s. It is a deliberate non-goal here. Fixing it properly
//! means making `alts!!` a real participant in the rendezvous protocol
//! (registering an intent a `chan_take`/`chan_put` can complete against,
//! then racing to claim it), which is a redesign of the unbuffered hand-off
//! itself -- and the hand-off's current semantics are load-bearing for
//! `>!!`/`<!!`/`offer!` and for `builtins::flow`. Every mixed case works
//! fine: `alts!!` put op against a blocking `<!!`, `alts!!` take op against
//! a blocking `>!!`, and either against a buffered chan.
//!
//! ## `timeout`: one shared timer thread
//!
//! `timeout` used to spawn a detached thread per call that slept and then
//! closed one channel -- an OS thread, with its stack reservation, per
//! timer. It now arms an entry on ONE process-wide timer service (`TIMER`):
//! a deadline-ordered `BinaryHeap` behind a `Mutex`, plus a `Condvar`, and
//! one `"mova-timer"` thread that closes every due channel and then sleeps
//! exactly until the next deadline. The observable contract is untouched:
//! the returned channel is never put to, and closes ~`ms` later. See
//! `TIMER`'s doc for why that thread is process-static and never joined,
//! and `timer_loop`'s for why `chan_close` is never called under the heap
//! lock.
//!
//! TIMER-CANCEL rides the same service: `(timeout-put ms ch val)` arms a
//! third entry kind ([`TimerAction::Put`]) and hands back a
//! [`crate::value::TimerCancel`] claim cell that `cancel-timer!` and the
//! fire race for -- the runtime's one cancellable timer, built so
//! embedders (a host application's debounce wheel was the motivating case) don't have
//! to fake cancellation in a userland timer service. `timeout` itself is
//! untouched. See `docs/TIMER-CANCEL-DESIGN.md`.
//!
//! ## Task parking: the sudog hand-off (L1/W3)
//!
//! A `go` block is a TASK -- a stackful coroutine on a shard thread
//! (`crate::runtime`) -- and a blocking `<!!`/`>!!` inside one parks the
//! task, not the shard's OS thread. A task is not a thread, so
//! `cv.notify_all()` cannot reach it; and, decisively, **a task cannot hold
//! the chan guard across its suspend** (the shard goes on to run other tasks
//! and one of them would wedge on it). Everything below follows from that
//! one fact.
//!
//! Two queues on `ChanState`, always present, both empty and allocation-free
//! for a chan no task ever touches:
//!
//! - `task_putters: VecDeque<PutterWaiter>` -- **the value rides in the
//!   waiter**, plus an `Arc<AtomicU8>` commit cell (`PUT_WAITING` ->
//!   `PUT_DONE`|`PUT_CLOSED`). A resumed putter reads ONLY that cell. This
//!   is the whole point: the OS protocol infers "my value was taken" from
//!   `buffer.is_empty()`, which is sound only because `cv_wait` releases the
//!   lock atomically with the wait. Without that atomicity a second putter
//!   refills the slot in the gap and the first wakes to someone else's value
//!   -- reporting a spurious extra wait, and `false` for a put that in fact
//!   succeeded. docs/L1-LANDING-SPEC.md §W3 calls that shape FORBIDDEN and
//!   `tests/task_chan_test.rs` regresses it.
//! - `task_takers: VecDeque<TakerWaiter>` -- a one-shot cell
//!   (`TakeSlot::Value`|`Closed`) plus a waker. A putter or `close!` fills
//!   it; the resumed taker reads only the cell.
//!
//! Both queues follow ONE rule: **the deliverer pops the waiter, moves the
//! value and commits the state UNDER the chan lock, drops the guard, and
//! only then wakes** -- the identical clone-out-then-ring discipline the
//! doorbell sites use (`chan_take`'s pop path). No lock is ever held across
//! a suspend, and delivery is atomic with respect to the chan state it was
//! decided from.
//!
//! Who serves whom:
//!
//! - **Take side** (task AND OS thread -- a mixed system must never strand a
//!   waiter of the other kind): buffer first, drain-then-nil unchanged; then
//!   an empty buffer takes the head task putter's value straight from its
//!   hand; and a pop that frees capacity on a `Fixed(n)` chan PROMOTES the
//!   head task putter's value into the freed room, so capacity never idles
//!   while a putter waits.
//! - **Put side** (task AND OS thread): a parked task taker is served
//!   directly, before the buffer is consulted -- a rendezvous that never
//!   touches `buffer`. That site is the L2 direct-switch seam.
//! - **`close!`**: drains both queues under the lock. Takers get buffered
//!   values FIFO first and closed-markers only once the buffer is empty
//!   (drain-then-nil, decided for them because they cannot re-run their own
//!   loop); putters get `PUT_CLOSED`.
//!
//! **S1, accepted and documented:** task putters and OS putters form two
//! queues, and so do takers, so there is no strict global FIFO across the
//! two KINDS on a contended chan (within a kind, order is preserved).
//! Today's condvar wake order already promises nothing of the sort -- this
//! is the same class of nondeterminism, not a new one.
//!
//! Two invariants the sites above maintain and rely on: a parked task taker
//! implies an empty buffer, and the two queues are never both non-empty
//! (each side checks the other's queue before parking).
//!
//! ## `go*`/`put!`/`take!`: task-backed as of L1/W4, `thread*` never is
//!
//! `go*` (what `core/async.mova`'s `go`/`go-loop` expand into) spawns its
//! body as a TASK on `crate::runtime` by default: a stackful coroutine on a
//! shard thread, not a fresh OS thread. `put!`/`take!` get the identical
//! treatment -- their blocking op plus optional callback run as a task, not
//! on a detached thread. All three share one body (`run_go_body`/
//! `run_put_body`/`run_take_body`) between their task and thread placements
//! so the two cannot drift: same forked `Interp`, same result-chan contract
//! (`go*`: `Fixed(1)`, non-nil result put once, then close; an escaping
//! Mova error is rendered to stderr, same as an uncaught REPL error, and the
//! channel just closes).
//!
//! `thread*` (what `core/async.mova`'s `thread` expands into, since L1/W4 --
//! it can no longer share `go*`'s body once `go*` defaults to tasks) ALWAYS
//! spawns a real 64 MiB OS thread, unconditionally. It is the documented
//! escape hatch for the R1 footgun below, and it is also what every internal
//! caller that must not block a shard end up needing.
//!
//! **Kill switch:** `MOVA_GO_THREADS=1`, read once via `OnceLock` (house
//! pattern -- `builtins::flow`'s `MOVA_NO_FASTSTEP`), reverts `go*`/`put!`/
//! `take!` wholesale to the pre-L1 real-OS-thread path (`thread*`'s body,
//! verbatim). `thread*` itself is unaffected either way -- it was already
//! that path.
//!
//! **R1 footgun (accepted, documented loudly in `core/async.mova`):** a
//! blocking native inside a task -- file IO, `Thread/sleep`, deref of an
//! unresolved future/promise/delay -- blocks its whole SHARD, not just its
//! task. Same class as the JVM's 8-thread `go` pool, with N-core width.
//! Escape hatch: `thread`.
//!
//! **S4 (put!/take! semantics shift):** today's callback ran on its own
//! detached thread; as a task it runs on a shard. A blocking callback now
//! blocks that shard, same R1 class as a blocking `go` body -- there is no
//! separate escape hatch for `put!`/`take!`'s callback short of writing a
//! non-blocking one or reaching for `thread`/`<!!` directly.

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use crate::builtins::{reg, ArityHint};
use crate::env::VarCell;
use crate::error::RjError;
use crate::eval::Interp;
use crate::runtime::TaskWaker;
use crate::sync::{cv_wait, cv_wait_timeout, lock_mutex};
use crate::pvec;
use crate::value::{
    BufferPolicy, Chan, ChanState, Doorbell, Keyword, PMap, PutterWaiter, TakeSlot, TakerWaiter,
    Value, PUT_CLOSED, PUT_DONE, PUT_WAITING,
};

/// 64 MiB, matching `builtins::conc::FUTURE_STACK_SIZE`'s reasoning: `go`/
/// `thread` bodies and `put!`/`take!` callbacks run through the same
/// tree-walking recursive-descent evaluator as everything else, so a
/// spawned thread needs real Rust stack headroom for deep mova-level
/// recursion.
const ASYNC_STACK_SIZE: usize = 64 * 1024 * 1024;

/// Outcome of a non-blocking take attempt (`chan_try_take`): distinguishes
/// "a value was sitting in the buffer/slot" from "closed with nothing left
/// to drain" (an `alts!!`/`poll!`-ready outcome, resolves to `nil`
/// immediately) from "open, empty, and no putter is offering right now"
/// (not ready -- `alts!!` moves on to the next op; `poll!` returns `nil`
/// too, since a plain `poll!` caller can't distinguish "not ready" from
/// "closed" any more than real Clojure's `poll!` return value does).
pub(crate) enum TryTake {
    Received(Value),
    Closed,
    WouldBlock,
}

/// Outcome of a non-blocking put attempt (`chan_try_put`). Unlike
/// `TryTake`, `offer!` does NOT collapse `Closed`/`WouldBlock` together --
/// real Clojure's `offer!` reports `Closed` as `false` but `WouldBlock` as
/// `nil` (verified against babashka v1.13's `clojure.core.async/offer!`;
/// see its `reg` call below), so both variants stay meaningfully distinct
/// all the way out to the return value.
pub(crate) enum TryPut {
    Sent,
    Closed,
    WouldBlock,
}

/// Rings every doorbell a blocking `alts!!` has registered on this chan
/// (see [`ChanState::alts_doorbells`]). Called from all 5 `notify_all()`
/// sites below, right beside each one's existing `ChanState::doorbell`
/// ring and following that site's shape (inside the lock where the site
/// rings inside the lock, off a clone taken before `drop(g)` where the
/// site does that). Nearly always a walk of an EMPTY slice -- a chan
/// nobody is `alts!!`-parked on carries no registrations, and cloning an
/// empty `Vec` doesn't allocate.
///
/// `pub(crate)` for `builtins::flow`'s `try_take_with_timeout`, the one
/// chan-state mutation site outside this file (see the module doc's
/// warning about that).
pub(crate) fn ring_alts(dbs: &[Arc<Doorbell>]) {
    for db in dbs {
        db.ring();
    }
}

/// Every blocking OS-thread park in this file goes through here rather than
/// `cv_wait` directly -- one seam, so a future waiter family (there have
/// been two: the probe's now-deleted `task_waiters`, and W3's real
/// `task_takers`/`task_putters`) has exactly one function to extend instead
/// of three call sites to find. Plain `cv_wait` today: real tasks never
/// reach here at all (see the assert below), so there is nothing left to
/// dispatch on.
#[inline]
fn chan_wait<'a>(ch: &'a Chan, g: MutexGuard<'a, ChanState>) -> MutexGuard<'a, ChanState> {
    // W3 invariant, worth pinning: the task runtime never gets here. A task
    // takes a `TakerWaiter`/`PutterWaiter` arm at every one of this file's
    // park boundaries, so a condvar wait always means an OS thread.
    debug_assert!(!crate::runtime::in_task(), "a task reached a condvar park (W3)");
    // P0c: interruptible slice-wait. `WAIT_INTR` is set (non-null) only by an
    // interruptible native (`<!!`) around its call.
    let p = crate::interrupt::WAIT_INTR.with(|c| c.get());
    if p.is_null() {
        return cv_wait(&ch.cv, g);
    }
    // SAFETY: the native that set `p` keeps the `Interp` (and its `Arc`) alive
    // for the whole call and clears `p` before returning.
    let (g, aborted) = crate::interrupt::wait_on(unsafe { &*p }, &ch.cv, g);
    if aborted {
        crate::interrupt::WAIT_ABORT.with(|c| c.set(true));
    }
    g
}

// ---------------------------------------------------------------------------
// L1/W3: the task arms (the sudog hand-off). Always compiled -- `go` IS the
// task runtime from L1 on (docs/L1-LANDING-SPEC.md §W3).
//
// Every helper below is guarded by an `is_empty()` on a `VecDeque` that has
// never been pushed to on a pure-OS-thread chan, so the cost of all of this
// to `>!!`/`<!!` traffic that never meets a task is one load and one branch
// per call (spec self-review S3).
// ---------------------------------------------------------------------------

/// Tasks a site must wake once it has dropped the chan guard. Almost always
/// empty, and an empty `Vec` does not allocate.
pub(crate) enum Wakes {
    /// Nothing to wake. The state every `>!!`/`<!!` that never meets a task
    /// both starts and ends in, and the only one the hot path can observe:
    /// constructing it is a single store of the discriminant and dropping it
    /// is a single compare, where a `Vec<TaskWaker>` cost three stores, a
    /// deallocation check, and (measured, W5c) an extra callee-saved
    /// register pair spilled in `chan_take`/`chan_put`'s prologue.
    None,
    /// Exactly one waker -- what all four hot-path producers
    /// ([`take_from_task_putter`], [`promote_task_putters`],
    /// [`deliver_to_task_taker`]) can ever contribute, since each hands off
    /// to at most one parked task per call.
    One(TaskWaker),
    /// Two or more. Only `chan_close`, which wakes every parked task on the
    /// chan at once, ever gets here -- and `promote_task_putters`' defensive
    /// self-healing `while`, if a future site ever frees more than one slot
    /// at a time.
    Many(Vec<TaskWaker>),
}

impl Wakes {
    #[inline]
    pub(crate) fn new() -> Wakes {
        Wakes::None
    }

    #[inline]
    fn push(&mut self, w: TaskWaker) {
        match self {
            Wakes::None => *self = Wakes::One(w),
            _ => self.push_cold(w),
        }
    }

    #[cold]
    #[inline(never)]
    fn push_cold(&mut self, w: TaskWaker) {
        match std::mem::replace(self, Wakes::None) {
            Wakes::None => *self = Wakes::One(w),
            Wakes::One(first) => *self = Wakes::Many(vec![first, w]),
            Wakes::Many(mut v) => {
                v.push(w);
                *self = Wakes::Many(v);
            }
        }
    }
}

#[inline]
pub(crate) fn wake_all(wakes: &Wakes) {
    if matches!(wakes, Wakes::None) {
        return;
    }
    wake_all_cold(wakes)
}

#[cold]
#[inline(never)]
fn wake_all_cold(wakes: &Wakes) {
    match wakes {
        Wakes::None => {}
        Wakes::One(w) => w.wake(),
        Wakes::Many(v) => {
            for w in v {
                w.wake();
            }
        }
    }
}

/// The hand-off, take side: an `Unbuffered` chan with a parked task putter
/// hands that putter's value STRAIGHT to the taker -- the value never enters
/// `buffer`, so nobody ever has to infer whose value is in the slot (that
/// inference is the shape §W3 forbids).
///
/// Gated on `Unbuffered` on purpose. For `Fixed(n>0)` a parked putter is
/// served by [`promote_task_putters`] instead (its value goes into the
/// freed capacity, which IS delivery for a buffered chan), and for
/// `Fixed(0)` -- a degenerate chan no put of any kind can ever complete on --
/// a task putter parks forever exactly like the OS putter beside it, because
/// a task/OS asymmetry there would be worse than the deadlock both kinds
/// already share.
///
/// Rings nothing: the only state it changes is `task_putters` shrinking,
/// which can make an op LESS ready and never more (the module doc's rule --
/// readiness loss needs no wake).
#[inline]
pub(crate) fn take_from_task_putter(g: &mut ChanState, wakes: &mut Wakes) -> Option<Value> {
    if g.task_putters.is_empty() || !matches!(g.policy, BufferPolicy::Unbuffered) {
        return None;
    }
    take_from_task_putter_cold(g, wakes)
}

#[cold]
#[inline(never)]
fn take_from_task_putter_cold(g: &mut ChanState, wakes: &mut Wakes) -> Option<Value> {
    // L4 W3 (docs/L4-SUPERVISION-DESIGN.md §3.5, regression net
    // tests/l4_kill_probe.rs): **the commit IS the claim.**
    // `runtime::TaskWaker::kill` CASes this same word
    // `PUT_WAITING -> PUT_KILLED` when it destroys a parked putter, so
    // exactly one of "the value is delivered" and "the value dies with its
    // task" happens, arbitrated on a word this site already wrote. Losing
    // the CAS means this waiter is a corpse: cull it, drop the value it was
    // holding out (it was never committed, so nothing is lost), and serve
    // the next putter.
    //
    // THE ONE MEASURED COST of the kill primitive: a release STORE became a
    // CAS (aarch64 `stlr` -> `casal`) on the unbuffered task->task
    // rendezvous, i.e. E1's path. Interleaved A/B, principal-run and
    // re-run in W3: E1 within noise (47.9-49.5 vs 51.8-56.4 ns/hop),
    // E4 within noise. It is uncontended in the common case — the killer
    // only touches the word when a supervisor is actually killing THAT task
    // — so the price is the RMW on an already-exclusive line, and the
    // `is_empty()` guard in the inline wrapper above means a chan with no
    // parked task putter pays literally nothing.
    loop {
        let p = g.task_putters.pop_front()?;
        if p.commit
            .compare_exchange(crate::value::PUT_WAITING, PUT_DONE, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            continue;
        }
        wakes.push(p.waker);
        return Some(p.val);
    }
}

/// The hand-off, buffered side: a take just freed capacity on a `Fixed(n)`
/// chan, so the head parked task putter's value moves into it and commits.
/// Without this, capacity would idle while a putter waits -- a task putter
/// has no condvar to be woken onto a retry loop the way an OS putter does,
/// so whoever frees the room must place the value for it.
///
/// The `while` is self-healing rather than necessary (one pop frees one
/// slot), and costs nothing when the queue is empty.
#[inline]
pub(crate) fn promote_task_putters(g: &mut ChanState, wakes: &mut Wakes) {
    if g.task_putters.is_empty() {
        return;
    }
    promote_task_putters_cold(g, wakes)
}

#[cold]
#[inline(never)]
fn promote_task_putters_cold(g: &mut ChanState, wakes: &mut Wakes) {
    let BufferPolicy::Fixed(n) = g.policy else { return };
    while g.buffer.len() < n {
        let Some(p) = g.task_putters.pop_front() else { return };
        // L4 W3: same claim as `take_from_task_putter_cold` — see there.
        // A corpse's value must not reach the buffer, and its slot must not
        // be consumed, so the `continue` leaves `buffer.len()` untouched and
        // the loop simply serves the next putter. The claim comes BEFORE the
        // buffer push for that reason: a lost CAS must leave no trace.
        if p.commit
            .compare_exchange(crate::value::PUT_WAITING, PUT_DONE, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            continue;
        }
        g.buffer.push_back(p.val);
        wakes.push(p.waker);
    }
}

/// The hand-off, put side -- **L2 landed here.** The `wakes.push` below is
/// what a direct switch acts on: when this deliverer is running on the
/// taker's own shard thread, `runtime::TaskWaker::wake` leaves the taker in
/// that shard's runnext slot instead of injecting it, and the taker runs next
/// rather than riding the inbox mutex around. Nothing in this function
/// changed to make that true -- see `runtime::TaskWaker::wake`.
///
/// A parked task taker is served straight from the putter's hand: the value never
/// touches `buffer`, for any policy. Called by BOTH `chan_put` and
/// `chan_try_put`, and therefore by OS-thread putters too: a mixed system
/// must never strand a parked task taker while an OS putter cv-waits at it.
///
/// Returns `false` if no task taker was waiting (caller falls through to its
/// ordinary buffer path). On `true` the caller owes `wakes` a wake after it
/// drops the guard.
#[inline]
#[must_use]
fn deliver_to_task_taker(g: &mut ChanState, v: &mut Option<Value>, wakes: &mut Wakes) -> bool {
    if g.task_takers.is_empty() {
        return false;
    }
    deliver_to_task_taker_cold(g, v, wakes)
}

#[cold]
#[inline(never)]
fn deliver_to_task_taker_cold(g: &mut ChanState, v: &mut Option<Value>, wakes: &mut Wakes) -> bool {
    // L4 W3: the commit IS the claim, arbitrated by the cell mutex this site
    // ALREADY takes. `runtime::TaskWaker::kill` tombstones the cell
    // (`TakeSlot::Waiting -> Closed`) under the same mutex when it destroys a
    // parked taker; a non-`Waiting` cell here therefore means this waiter is
    // a corpse. Cull it and try the next taker — and note that `v` is only
    // `take()`n AFTER the claim succeeds, so a caller whose queue holds
    // nothing but corpses gets `false` and falls through to its ordinary
    // buffer / park path. **That is what stops a killed taker being a black
    // hole for every subsequent put** — the one thing P5a refuted outright
    // (512/512 puts swallowed) and P5a-bis fixed (512/512 survive).
    //
    // Zero new locks and zero new atomics: the guard was already being taken
    // and the check is a branch inside it.
    loop {
        let Some(t) = g.task_takers.pop_front() else { return false };
        let mut cell = lock_mutex(&t.cell);
        if !matches!(*cell, TakeSlot::Waiting) {
            continue;
        }
        *cell = TakeSlot::Value(v.take().expect("a value is delivered at most once per call"));
        drop(cell);
        wakes.push(t.waker);
        return true;
    }
}

/// `crate::runtime::in_task()` behind an `#[inline(never)]` wall.
///
/// **W5c, and load-bearing for `>!!`/`<!!` throughput.** `in_task()` reads a
/// thread-local, and on aarch64-darwin a TLS read is a call through the TLV
/// descriptor (`adrp`/`ldr`/`blr`). LLVM treats that address computation as
/// loop-invariant and hoists it out of `chan_take`/`chan_put`'s retry loop --
/// which, because the loop is entered unconditionally, lands the `blr` in the
/// FUNCTION PROLOGUE, so every buffered `>!!`/`<!!` paid for a TLS thunk it
/// never used. Behind this wall the TLS access happens only where it is
/// actually needed: at a park boundary.
#[inline(never)]
fn in_task() -> bool {
    crate::runtime::in_task()
}

/// `chan_take`'s task-park arm, outlined (W5c). Verbatim the code that used
/// to sit inline in the loop; see the call site for why each step is where it
/// is. Outlined because inlining it grew `chan_take` from 326 to 868
/// instructions on aarch64 -- a bigger frame, an extra callee-saved pair to
/// spill, and a hoisted TLS thunk -- all of it charged to the buffered pop
/// path that never parks at all.
#[cold]
#[inline(never)]
fn register_and_park_task_taker(mut g: MutexGuard<'_, ChanState>) -> Option<Value> {
    debug_assert_live_waiter_invariant(&g);
    // L2 lever 2: the task's OWN cell, cached on its `TaskShared`, not a
    // fresh allocation per park. `Waiting` here by invariant --
    // `park_task_taker`'s `mem::replace` leaves it so on every return -- so
    // there is nothing to re-arm; see `runtime::current_take_cell`.
    let cell = crate::runtime::current_take_cell();
    g.task_takers.push_back(TakerWaiter { cell: cell.clone(), waker: crate::runtime::current_waker() });
    // The same obligation `chan_take`'s `waiting_takers += 1` discharges, and
    // rung UNDER the lock for the same reason: a parked task taker is the
    // other half of `chan_try_put`'s unbuffered gate, so an `alts!!` PUT op on
    // an unbuffered chan becomes ready at exactly this line.
    if let Some(db) = g.doorbell.as_ref() {
        db.ring();
    }
    ring_alts(&g.alts_doorbells);
    drop(g);
    // No `cv.notify_all()` here, for the reason spelled out at the
    // `waiting_takers` increment in `chan_take`: nothing that a taker's
    // arrival makes ready is something an OS waiter on THIS condvar is
    // waiting for.
    park_task_taker(&cell)
}

/// `chan_put`'s task-park arm (the sudog park), outlined (W5c) -- same
/// reasoning as [`register_and_park_task_taker`]. The value moves INTO the
/// waiter and out of the caller's hands; on resume the only thing read back
/// is the commit cell.
#[cold]
#[inline(never)]
fn register_and_park_task_putter(ch: &Chan, mut g: MutexGuard<'_, ChanState>, val: Value) -> bool {
    debug_assert_live_waiter_invariant(&g);
    // L2 lever 2: the task's OWN commit cell, cached on its `TaskShared`.
    // Re-armed HERE as well as on the way out of the last park (see
    // [`park_task_putter`]): under this chan's lock and before the waiter is
    // pushed, i.e. before any deliverer can see the waiter and therefore
    // before any deliverer can write the cell. That ordering is what keeps
    // "written once per registration" true across reuse; the full argument
    // is at `runtime::current_put_cell`. Belt and braces on purpose -- this
    // one is about the REGISTRATION being clean, the exit-path one is about
    // the cell's RESTING value being unambiguous to a killer.
    let commit = crate::runtime::current_put_cell();
    commit.store(PUT_WAITING, Ordering::Release);
    g.task_putters.push_back(PutterWaiter { val, commit: commit.clone(), waker: crate::runtime::current_waker() });
    // A queued task putter is a new input to every take-side scan
    // (`chan_try_take` serves an unbuffered take out of it), so enqueueing
    // owes the full ring set a buffer push owes -- and the `cv` notify in
    // particular: an OS taker already parked in `chan_take`'s loop must wake
    // and come collect this value, or it sleeps beside a putter that is
    // holding one out to it. Clone out and ring after `drop(g)`, the usual
    // discipline.
    let db = g.doorbell.clone();
    let alts = g.alts_doorbells.clone();
    drop(g);
    ch.cv.notify_all();
    if let Some(db) = db {
        db.ring();
    }
    ring_alts(&alts);
    park_task_putter(&commit)
}

/// Suspend the current TASK until its [`TakerWaiter`] cell is filled.
///
/// The loop is not decoration. A wake can reach a task that is no longer
/// waiting for it: a deliverer pops the waiter, fills the cell and drops the
/// guard, the resumed-by-nothing task reads the cell and walks on, and only
/// THEN does `wake()` land -- leaving `NOTIFIED` behind, which the scheduler
/// spends at whatever this task parks on next. So a task park always
/// re-reads its own cell and re-parks if it is still `Waiting`. (That is the
/// only such path: the two chan queues are pop-before-wake, and
/// `Doorbell::wait_for_change_task` unregisters on resume, so no OTHER party
/// holds a live handle on a task parked here.)
fn park_task_taker(cell: &Arc<Mutex<TakeSlot>>) -> Option<Value> {
    loop {
        // The `let` is load-bearing, not style: a guard produced inside a
        // `match` SCRUTINEE lives until the end of the match, which would
        // hold this cell locked across `park_current_yield()` below -- and
        // the deliverer writes the cell while holding the CHAN lock, so that
        // is a two-lock deadlock, not merely a held lock. Bind, drop, then
        // decide.
        let slot = std::mem::replace(&mut *lock_mutex(cell), TakeSlot::Waiting);
        match slot {
            TakeSlot::Value(v) => return Some(v),
            TakeSlot::Closed => return None,
            TakeSlot::Waiting => crate::runtime::park_current_yield(),
        }
    }
}

/// Suspend the current TASK until its [`PutterWaiter`] commit cell leaves
/// `PUT_WAITING`. On resume it reads THIS AND NOTHING ELSE -- never the
/// buffer (§W3's forbidden inference). Loops for the same reason
/// [`park_task_taker`] does.
fn park_task_putter(commit: &Arc<AtomicU8>) -> bool {
    loop {
        let done = match commit.load(Ordering::Acquire) {
            PUT_DONE => true,
            PUT_CLOSED => false,
            _ => {
                crate::runtime::park_current_yield();
                continue;
            }
        };
        // **L4 W3: re-arm the cell to its RESTING value on the way out.**
        // The kill protocol (`runtime::TaskWaker::kill`) distinguishes "this
        // parked task has an uncommitted put outstanding" from "a commit has
        // already landed under it" by reading exactly this word, so a task
        // that is parked on something ELSE -- a `Doorbell`, a timer, a take
        // -- must not still be carrying the last put's terminal `PUT_DONE`.
        // Without this store, killing a doorbell-parked flow proc that had
        // ever completed a blocking put salvage-loops forever and the kill
        // never lands. (The probe never saw it: its victims were parked in
        // chan ops on their first trip round.)
        //
        // One relaxed store on the resume path of a park that just cost a
        // context switch, written by the cell's ONLY possible writer at this
        // instant: the waiter was popped out of `task_putters` before the
        // commit was written, so no deliverer holds a handle on it any more
        // (`runtime::current_put_cell`'s soundness argument, unchanged), and
        // no killer can be racing us because a killer acts only on a PARKED
        // task and this one is RUNNING.
        commit.store(PUT_WAITING, Ordering::Relaxed);
        return done;
    }
}

/// **The live-waiter invariant** (`value.rs`'s `ChanState::task_takers`,
/// restated for L4 W3 by probe finding S1), checked where it must hold: at
/// the two registration sites, under the chan lock, with the caller's own
/// waiter not yet pushed.
///
/// Debug builds only, and deliberately so: it locks every queued waiter's
/// cell to tell a LIVE waiter from a kill CORPSE, which is O(queue) work on
/// a park path. Lock order is chan -> cell, the same one every deliverer
/// takes, so it cannot invert anything.
///
/// What it does NOT assert: that the queues hold no corpses. They may -- a
/// killed task's waiter stays queued until a deliverer walks past it (that
/// is the documented lazy cull), which is precisely why the unqualified form
/// of this invariant was false and why this one counts LIVE members only.
#[cfg(debug_assertions)]
fn debug_assert_live_waiter_invariant(g: &ChanState) {
    let live_takers =
        g.task_takers.iter().filter(|t| matches!(*lock_mutex(&t.cell), TakeSlot::Waiting)).count();
    let live_putters =
        g.task_putters.iter().filter(|p| p.commit.load(Ordering::Relaxed) == PUT_WAITING).count();
    debug_assert!(
        live_takers == 0 || g.buffer.is_empty(),
        "chan invariant: a LIVE parked taker implies an empty buffer \
         ({live_takers} live takers, {} buffered)",
        g.buffer.len()
    );
    debug_assert!(
        live_takers == 0 || live_putters == 0,
        "chan invariant: live takers and live putters are never both queued \
         ({live_takers} takers, {live_putters} putters)"
    );
}

#[cfg(not(debug_assertions))]
#[inline(always)]
fn debug_assert_live_waiter_invariant(_g: &ChanState) {}

/// Blocking take: drains a buffered/rendezvoused value if one is present;
/// otherwise parks (tracked via `waiting_takers`) until either a put
/// arrives or the channel closes. See the module doc's "channel protocol"
/// section for why checking the buffer before `closed` is exactly what
/// gives close! its drain-then-nil semantics.
/// `pub(crate)` (v0.2 / F1): `builtins::flow`'s engine drives channels
/// directly at this level rather than through the `>!!`/`<!!`/`alts!!`
/// natives -- see that module's doc for why (control-priority racing,
/// batch-drain, mult fan-out all need the raw try/blocking primitives, not
/// a value-level call into the interpreter per message).
pub(crate) fn chan_take(ch: &Chan) -> Option<Value> {
    let mut g = lock_mutex(&ch.state);
    let mut wakes = Wakes::new();
    loop {
        if let Some(v) = g.buffer.pop_front() {
            // W3: the pop freed capacity -- fill it from the task-putter
            // queue before anyone else can, so a parked putter never waits
            // on room that is already there.
            promote_task_putters(&mut g, &mut wakes);
            // Clone out, then ring after `drop(g)`: a registered doorbell
            // wakes a thread whose very first move is to lock this same
            // `state`, so ringing K of them while still holding the lock
            // makes every waker queue behind us. Measured ~10x producer
            // serialization at K=32 registered selectors.
            let db = g.doorbell.clone();
            let alts = g.alts_doorbells.clone();
            drop(g);
            ch.cv.notify_all();
            if let Some(db) = db {
                db.ring();
            }
            ring_alts(&alts);
            wake_all(&wakes);
            return Some(v);
        }
        // W3 hand-off: nothing buffered, but a task putter is holding a
        // value out. Take it from its hand and commit it. No rings -- see
        // `take_from_task_putter`.
        if let Some(v) = take_from_task_putter(&mut g, &mut wakes) {
            drop(g);
            wake_all(&wakes);
            return Some(v);
        }
        if g.closed {
            return None;
        }
        // W3, task context: park as a `TakerWaiter` instead of on the
        // condvar. Registration happens under this lock (so a putter that
        // takes it next cannot miss us), the guard is dropped, and only then
        // does the task suspend -- it must not hold the guard across the
        // suspend or the next task on this shard would wedge on it
        // (`runtime`'s module doc).
        if in_task() {
            return register_and_park_task_taker(g);
        }
        // `waiting_takers` 0 -> 1 is a THIRD input to an `alts!!` scan,
        // besides `buffer` and `closed`: `chan_try_put`'s `Unbuffered`
        // gate is `buffer.is_empty() && waiting_takers > 0`, so a parked
        // put-op `alts!!` on an unbuffered chan becomes ready at exactly
        // this line and nowhere else. It must ring, or that `alts!!` sits
        // until `ALTS_PARK_TIMEOUT` (probed: 1800ms of dead air). Unlike
        // the pop path above, this one rings UNDER the lock -- it has to,
        // since the guard is handed straight to `cv_wait` and there is no
        // point in between where the lock is free. That is fine here: this
        // is a park boundary, reached only by a taker that found nothing
        // and is about to block, not a hot path.
        //
        // The matching DECREMENT below deliberately does not ring, and
        // neither do either of `chan_put`'s `waiting_putters` mutations:
        // 1 -> 0 makes an unbuffered put-op UNready, and a ring exists
        // only to tell a parked scanner "re-check, something may be ready
        // NOW" -- readiness LOSS needs no wake. (`waiting_putters` is not
        // an input to any scan at all: neither `chan_try_take` nor
        // `chan_try_put` reads it.)
        //
        // Note what is NOT added here: a `ch.cv.notify_all()`. Two
        // blocking takers parked on one empty chan would then wake each
        // other forever -- B's park notifies A, A decrements/re-increments
        // and notifies B, ad infinitum. The doorbell families have no such
        // problem: nothing that parks in `chan_take` waits on a doorbell,
        // so a ring can never bounce back here.
        g.waiting_takers += 1;
        if let Some(db) = g.doorbell.as_ref() {
            db.ring();
        }
        ring_alts(&g.alts_doorbells);
        // No task wake here on purpose, for the same reason the decrement
        // rings nothing: `waiting_takers` 0 -> 1 only makes an `alts!!` PUT
        // op ready, and nothing that parks in `task_waiters` reads that
        // counter -- a chan's task waiters are woken by `buffer`/`closed`
        // changes only.
        g = chan_wait(ch, g);
        g.waiting_takers -= 1;
        // P0c: interrupted while parked.
        if crate::interrupt::WAIT_ABORT.with(|c| c.get()) {
            return None;
        }
    }
}

/// Blocking put: returns `true` once `v` is durably handed off (buffered,
/// or -- for `Unbuffered` -- actually collected by a taker), `false`
/// immediately if the channel is already closed, or `false` if it's still
/// `Unbuffered`-pending when the channel closes out from under it (Clojure:
/// "puts blocked at close time return false"). See the module doc for why
/// `Unbuffered` doesn't require a parked taker to *start* the handoff (only
/// the non-blocking `chan_try_put` does).
pub(crate) fn chan_put(ch: &Chan, v: Value) -> bool {
    let mut g = lock_mutex(&ch.state);
    let mut pending = Some(v);
    let mut wakes = Wakes::new();
    loop {
        if g.closed {
            return false;
        }
        // W3, and the site L2's direct switch pays off on (landed -- see
        // `runtime::TaskWaker::wake`). A parked task taker is served before
        // the buffer is even consulted -- for every policy, and from
        // OS-thread putters too.
        if deliver_to_task_taker(&mut g, &mut pending, &mut wakes) {
            drop(g);
            wake_all(&wakes);
            return true;
        }
        let has_room = match g.policy {
            // W3: in TASK context an unbuffered put never uses the
            // rendezvous slot. The OS protocol below parks in the slot and
            // then infers "my value was taken" from `buffer.is_empty()`,
            // which is only sound because `cv_wait` releases the lock
            // atomically with the wait. A task's suspend does not: a second
            // putter refills the slot in the gap and the first wakes to
            // someone else's value (docs/L1-LANDING-SPEC.md §W3, THE
            // FORBIDDEN SHAPE). A task putter with no taker present goes to
            // `task_putters` instead, where its value has an identity.
            //
            // S3: this is the ONLY `in_task()` on the put hot path, and it is
            // reached only by an unbuffered put -- a `Fixed(n)` put with room
            // (the buffered-1024 bench's whole loop) never reads the TLS slot
            // at all.
            BufferPolicy::Unbuffered => g.buffer.is_empty() && !in_task(),
            BufferPolicy::Fixed(n) => g.buffer.len() < n,
            BufferPolicy::Dropping(_) | BufferPolicy::Sliding(_) => true,
        };
        if has_room {
            let val = pending.take().expect("has_room is only reached once per call");
            push_with_policy(&mut g, val);
            if matches!(g.policy, BufferPolicy::Unbuffered) {
                // The ONE ring site that stays under the lock, and it has
                // to: this path must not release `state` between the push
                // and the rendezvous wait just below, or another putter
                // could refill the slot in the gap and we'd mistake their
                // value for our own un-taken one (and, at close, report
                // `false` for a put that in fact succeeded). Cheap where it
                // matters anyway -- an unbuffered putter is about to block
                // regardless, so the O(K) ring costs it nothing it wasn't
                // already going to pay.
                ch.cv.notify_all();
                if let Some(db) = g.doorbell.as_ref() {
                    db.ring();
                }
                ring_alts(&g.alts_doorbells);
                // Block until our value is actually picked up, or the
                // channel closes with it still sitting there un-taken.
                loop {
                    if g.buffer.is_empty() {
                        return true;
                    }
                    if g.closed {
                        g.buffer.clear();
                        return false;
                    }
                    g.waiting_putters += 1;
                    // W3 closed the task hazard this loop used to carry:
                    // `has_room` above is false in task context for an
                    // `Unbuffered` chan, so a task never enters this
                    // rendezvous loop at all -- it parks in `task_putters`
                    // instead, where its value has a waiter identity rather
                    // than a shared slot a second putter could refill out
                    // from under it. Only an OS thread reaches `chan_wait`
                    // here, and `cv_wait` releases the lock atomically with
                    // the wait, so this OS-only rendezvous is exactly as
                    // sound as it always was.
                    g = chan_wait(ch, g);
                    g.waiting_putters -= 1;
                    if crate::interrupt::WAIT_ABORT.with(|c| c.get()) {
                        g.buffer.clear();
                        return false;
                    }
                }
            }
            // Buffered: nothing left to do under the lock, so clone out and
            // ring after `drop(g)` -- see `chan_take`'s pop path for why
            // (a woken selector's first move is to lock this same `state`).
            // This is the hot producer path, and the one the measurement
            // was taken on.
            let db = g.doorbell.clone();
            let alts = g.alts_doorbells.clone();
            drop(g);
            ch.cv.notify_all();
            if let Some(db) = db {
                db.ring();
            }
            ring_alts(&alts);
            return true;
        }
        // W3, task context: the sudog park. The value moves INTO the waiter
        // and out of this call's hands; on resume the only thing read back
        // is the commit cell.
        if in_task() {
            let val = pending.take().expect("the value is still in hand until it is queued");
            return register_and_park_task_putter(ch, g, val);
        }
        g.waiting_putters += 1;
        g = chan_wait(ch, g);
        g.waiting_putters -= 1;
        if crate::interrupt::WAIT_ABORT.with(|c| c.get()) {
            return false;
        }
    }
}

/// Shared push-with-policy logic between `chan_put` (blocking, already
/// confirmed `has_room`) and `chan_try_put` (non-blocking): `Fixed`/
/// `Unbuffered` push unconditionally (capacity already checked by the
/// caller); `Dropping` silently discards past capacity; `Sliding` evicts
/// the oldest element to make room. `Sliding(0)` is a degenerate
/// zero-capacity sliding buffer -- the pushed value has nowhere to live and
/// is immediately discarded, matching a fixed-0 buffer's "put always
/// succeeds, value never observed" shape.
fn push_with_policy(g: &mut ChanState, v: Value) {
    match g.policy {
        BufferPolicy::Unbuffered | BufferPolicy::Fixed(_) => g.buffer.push_back(v),
        BufferPolicy::Dropping(n) => {
            if g.buffer.len() < n {
                g.buffer.push_back(v);
            }
        }
        BufferPolicy::Sliding(n) => {
            if n > 0 {
                if g.buffer.len() >= n {
                    g.buffer.pop_front();
                }
                g.buffer.push_back(v);
            }
        }
    }
}

/// Non-blocking take: never parks. See `TryTake`'s doc for the three
/// outcomes.
/// W3 note: this is a take site like `chan_take`, so it owes the same two
/// task-putter obligations -- promote after a pop, and hand-off when the
/// buffer is empty. The hand-off has a visible consequence: an `alts!!` take
/// op (or a `poll!`) DOES complete against a parked task putter on an
/// unbuffered chan, unlike the module doc's "alts!! and the unbuffered
/// rendezvous" limitation, which stands only for the OS-thread `>!!`
/// protocol. A task putter is a registered intent, which is exactly what
/// that note says the limitation is missing.
pub(crate) fn chan_try_take(ch: &Chan) -> TryTake {
    let mut g = lock_mutex(&ch.state);
    let mut wakes = Wakes::new();
    if let Some(v) = g.buffer.pop_front() {
        promote_task_putters(&mut g, &mut wakes);
        let db = g.doorbell.clone();
        let alts = g.alts_doorbells.clone();
        drop(g);
        ch.cv.notify_all();
        if let Some(db) = db {
            db.ring();
        }
        ring_alts(&alts);
        wake_all(&wakes);
        TryTake::Received(v)
    } else if let Some(v) = take_from_task_putter(&mut g, &mut wakes) {
        drop(g);
        wake_all(&wakes);
        TryTake::Received(v)
    } else if g.closed {
        TryTake::Closed
    } else {
        TryTake::WouldBlock
    }
}

/// Non-blocking BULK take: move up to `max` buffered values into `out`
/// under ONE lock acquisition, with ONE ring/notify epilogue for the whole
/// batch. Returns how many moved; `0` means the buffer was empty and the
/// caller should fall back to [`chan_try_take`], which is where every
/// non-buffer outcome (a parked task putter's hand-off, `closed`,
/// `WouldBlock`) is still decided.
///
/// Introduced by docs/FLOW-HOP-RECOVERY.md. `builtins::flow`'s batch drain
/// used to call [`chan_try_take`] once per message of a batch it was about
/// to process as one unit, paying a full chan-mutex acquisition, a
/// `promote_task_putters` scan, a `Condvar::notify_all` and a
/// `Doorbell::ring` for EVERY message. On a saturated 1:1 flow conn that
/// mutex is contended between two cores, so those were the expensive kind
/// of acquisition, not the ~25ns uncontended kind E2 priced.
///
/// Semantics are exactly [`chan_try_take`] repeated `n` times, and the two
/// obligations it discharges are batch-safe:
/// - `promote_task_putters` is a loop over the room now available, not a
///   per-pop event, so running it once after freeing `n` slots promotes
///   exactly the putters `n` separate calls would have.
/// - the readiness-gaining epilogue (`cv.notify_all` + the chan's
///   `doorbell` + the `alts_doorbells` family) is level-triggered: every
///   waiter re-scans the chan when it wakes, so one ring after `n` freed
///   slots wakes the same set that one ring per slot would have.
///   Under-ringing is the hazard the doorbell protocol guards against, and
///   there is none here -- the single ring happens strictly after the last
///   pop, so no waiter can observe the freed room without a ring following
///   it.
pub(crate) fn chan_drain_into(ch: &Chan, out: &mut Vec<Value>, max: usize) -> usize {
    if max == 0 {
        return 0;
    }
    let mut g = lock_mutex(&ch.state);
    let n = g.buffer.len().min(max);
    if n == 0 {
        return 0;
    }
    out.extend(g.buffer.drain(..n));
    let mut wakes = Wakes::new();
    promote_task_putters(&mut g, &mut wakes);
    let db = g.doorbell.clone();
    let alts = g.alts_doorbells.clone();
    drop(g);
    ch.cv.notify_all();
    if let Some(db) = db {
        db.ring();
    }
    ring_alts(&alts);
    wake_all(&wakes);
    n
}

/// Non-blocking put: never parks, never leaves a durably-unobserved value
/// sitting in an `Unbuffered` channel's slot (see the module doc's
/// `waiting_takers > 0` gate). See `TryPut`'s doc for the three outcomes.
pub(crate) fn chan_try_put(ch: &Chan, v: Value) -> TryPut {
    let mut g = lock_mutex(&ch.state);
    if g.closed {
        return TryPut::Closed;
    }
    // W3, and L2's direct switch reaches this put path too (landed -- see
    // `runtime::TaskWaker::wake`). Checked BEFORE the policy
    // gate, exactly as in `chan_put`, so the two put paths agree about what
    // a parked task taker makes possible. Note what this does NOT do: touch
    // the `waiting_takers > 0` gate below, which keeps meaning precisely
    // what its doc says it means.
    let mut pending = Some(v);
    let mut wakes = Wakes::new();
    if deliver_to_task_taker(&mut g, &mut pending, &mut wakes) {
        drop(g);
        wake_all(&wakes);
        return TryPut::Sent;
    }
    let v = pending.take().expect("undelivered, so still in hand");
    let can_send = match g.policy {
        BufferPolicy::Unbuffered => g.buffer.is_empty() && g.waiting_takers > 0,
        BufferPolicy::Fixed(n) => g.buffer.len() < n,
        BufferPolicy::Dropping(_) | BufferPolicy::Sliding(_) => true,
    };
    if !can_send {
        return TryPut::WouldBlock;
    }
    push_with_policy(&mut g, v);
    let db = g.doorbell.clone();
    let alts = g.alts_doorbells.clone();
    drop(g);
    ch.cv.notify_all();
    if let Some(db) = db {
        db.ring();
    }
    ring_alts(&alts);
    TryPut::Sent
}

/// Idempotent close: only the first call actually flips the flag and wakes
/// waiters (a second `close!` is a documented no-op, matching Clojure).
///
/// W3: an OS waiter re-runs its own loop after the flag flips and works out
/// what close means for it. A parked TASK cannot -- it re-reads only its own
/// cell (that is the point of the cell) -- so close! has to decide FOR it,
/// here, under the lock, and it decides exactly what the loop would have:
/// takers drain the buffer FIRST and see nil only once it is empty; putters
/// blocked at close time report false.
pub(crate) fn chan_close(ch: &Chan) {
    let mut g = lock_mutex(&ch.state);
    if !g.closed {
        g.closed = true;
        let mut wakes = Wakes::new();
        // Takers, FIFO: whatever is buffered goes to the front of the queue
        // (a value that was already in the chan when it closed is still
        // deliverable), a closed-marker to everyone after it. By the
        // `task_takers` invariant the buffer is empty whenever a task taker
        // is parked, so the value arm is defense against a future
        // buffer-push site that forgets to check `task_takers` first -- it
        // costs one `pop_front` on a queue that is almost always empty.
        while let Some(t) = g.task_takers.pop_front() {
            // L4 W3: same claim as `deliver_to_task_taker_cold`, and for one
            // reason only — the `Some(v)` arm below can hand a BUFFERED value
            // to this waiter, and handing it to a corpse would lose it. (The
            // `None` arm is a close-marker and losing that to a corpse would
            // be harmless; `chan_close`'s PUTTER loop below is deliberately
            // NOT claimed for the same reason — it drops the waiter's value
            // either way, so a corpse there loses nothing.) ONE guard, taken
            // exactly where the unmodified code took it, so the claim check
            // adds no second acquisition.
            let mut cell = lock_mutex(&t.cell);
            if !matches!(*cell, TakeSlot::Waiting) {
                continue;
            }
            *cell = match g.buffer.pop_front() {
                Some(v) => TakeSlot::Value(v),
                None => TakeSlot::Closed,
            };
            drop(cell);
            wakes.push(t.waker);
        }
        // Putters: their values die with the channel, and each one's `>!`
        // returns false.
        while let Some(p) = g.task_putters.pop_front() {
            p.commit.store(PUT_CLOSED, Ordering::Release);
            wakes.push(p.waker);
        }
        let db = g.doorbell.clone();
        let alts = g.alts_doorbells.clone();
        drop(g);
        ch.cv.notify_all();
        if let Some(db) = db {
            db.ring();
        }
        ring_alts(&alts);
        wake_all(&wakes);
    }
}

fn require_chan<'a>(v: &'a Value, op: &str) -> Result<&'a Arc<Chan>, RjError> {
    match v {
        Value::Channel(c) => Ok(c),
        other => Err(RjError::type_err(format!("{op}: expected a channel, got {}", other.type_name()))),
    }
}

fn require_timer<'a>(v: &'a Value, op: &str) -> Result<&'a Arc<crate::value::TimerCancel>, RjError> {
    match v {
        Value::Timer(t) => Ok(t),
        other => Err(RjError::type_err(format!("{op}: expected a timer, got {}", other.type_name()))),
    }
}

fn require_nonneg_int(v: &Value, op: &str) -> Result<usize, RjError> {
    match v {
        Value::Int(n) if *n >= 0 => Ok(*n as usize),
        Value::Int(n) => Err(RjError::other(format!("{op}: expected a non-negative int, got {n}"))),
        other => Err(RjError::type_err(format!("{op}: expected an int, got {}", other.type_name()))),
    }
}

fn nil_put_err() -> RjError {
    RjError::other("can't put nil on channel")
}

/// `dropping-buffer`/`sliding-buffer` (see below) hand `chan` a small
/// descriptor `Value::Map` rather than a real buffer object -- `chan`
/// parses it back into a `BufferPolicy`. This indirection exists purely so
/// `(chan (dropping-buffer 5))` reads like real Clojure; the map's shape is
/// an internal protocol between these two natives, never meant to be
/// inspected by user code.
fn buffer_descriptor(kind: &str, n: usize) -> Value {
    let mut m = PMap::new();
    m.insert(Value::Keyword(Keyword::from("mova.async/buffer-type")), Value::Keyword(Keyword::from(kind)));
    m.insert(Value::Keyword(Keyword::from("mova.async/n")), Value::Int(n as i64));
    Value::Map(m)
}

fn parse_buffer_arg(v: &Value) -> Result<BufferPolicy, RjError> {
    match v {
        Value::Int(_) => Ok(BufferPolicy::Fixed(require_nonneg_int(v, "chan")?)),
        Value::Map(m) => {
            let kind = m.get(&Value::Keyword(Keyword::from("mova.async/buffer-type")));
            let n = m.get(&Value::Keyword(Keyword::from("mova.async/n")));
            match (kind, n) {
                (Some(Value::Keyword(k)), Some(Value::Int(n))) if *n >= 0 => match k.as_ref() {
                    "dropping" => Ok(BufferPolicy::Dropping(*n as usize)),
                    "sliding" => Ok(BufferPolicy::Sliding(*n as usize)),
                    other => Err(RjError::type_err(format!("chan: unknown buffer type {other:?}"))),
                },
                _ => Err(RjError::type_err("chan: malformed buffer descriptor")),
            }
        }
        other => Err(RjError::type_err(format!(
            "chan: expected an int (fixed buffer size) or a dropping-buffer/sliding-buffer descriptor, got {}",
            other.type_name()
        ))),
    }
}

/// Tiny thread-local xorshift64 PRNG for `alts!!`'s per-pass randomized op
/// order -- deliberately not a real `rand` dependency (none is in
/// `Cargo.toml`, and this needs no cryptographic or even statistical
/// rigor, just "don't always try the ops in the same order").
///
/// L5/W5 kernel fix: the cell also carries the `clock::sim_call_gen()` this
/// state was last seeded at, so a `simulate` call's re-seed of the USER
/// stream is visible here even though this state is thread-local (and this
/// consumer runs on the sim shard thread, not the `simulate` caller's) --
/// see `clock::SIM_CALL_GEN` and `builtins::random::next_u64` (identical
/// treatment). Real mode is unchanged: the gen check is behind the same
/// `sim_enabled()` branch the old lazy-seed check already used.
fn next_rand() -> u64 {
    use std::cell::Cell;
    thread_local! {
        static STATE: Cell<(u64, u64)> = const { Cell::new((0, 0)) };
    }
    STATE.with(|s| {
        let (mut gen, mut x) = s.get();
        if crate::clock::sim_enabled() {
            let cur_gen = crate::clock::sim_call_gen();
            if x == 0 || gen != cur_gen {
                // L5 / P6a (design §3, "Seed plumbing"): the alts shuffle is
                // the highest-value item on the seeded-PRNG list -- it is
                // user-visible selection nondeterminism today. In sim the
                // thread-local starts from the USER stream (never the
                // schedule stream: a program adding an `alts!!` must not
                // shift every scheduling decision after it).
                //
                // L5/W3 (fence #8): a DRAW from the user stream, not a
                // constant derived from the seed — every consumer now shares
                // one ordered stream, so two threads (or two thread-locals)
                // can no longer be handed the same sequence. See
                // `clock::user_next`.
                x = crate::clock::user_next_nonzero();
                gen = cur_gen;
            }
        } else if x == 0 {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0x2545_F491_4F6C_DD1D);
            x = nanos | 1; // must be nonzero for xorshift
        }
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        s.set((gen, x));
        x
    })
}

fn shuffled_indices(n: usize) -> Vec<usize> {
    let mut idxs: Vec<usize> = (0..n).collect();
    for i in (1..n).rev() {
        let j = (next_rand() as usize) % (i + 1);
        idxs.swap(i, j);
    }
    idxs
}

/// One parsed `alts!!` op: a take (bare channel) or a put (`[ch val]`).
enum AltOp {
    Take(Arc<Chan>),
    Put(Arc<Chan>, Value),
}

fn parse_alt_ops(v: &Value) -> Result<Vec<AltOp>, RjError> {
    let items = match v {
        // S7: a map entry is a 2-element vector everywhere a vector is
        // accepted -- including as an `alts!!` ops vector.
        Value::Vector(items) | Value::MapEntry(items) => items.clone(),
        other => {
            return Err(RjError::type_err(format!(
                "alts!!: expected a vector of ops, got {}",
                other.type_name()
            )))
        }
    };
    let mut ops = Vec::with_capacity(items.len());
    for it in items.iter() {
        match it {
            Value::Channel(c) => ops.push(AltOp::Take(c.clone())),
            Value::Vector(pair) | Value::MapEntry(pair) if pair.len() == 2 => match &pair[0] {
                Value::Channel(c) => {
                    if matches!(pair[1], Value::Nil) {
                        return Err(nil_put_err());
                    }
                    ops.push(AltOp::Put(c.clone(), pair[1].clone()));
                }
                other => {
                    return Err(RjError::type_err(format!(
                        "alts!!: put op's first element must be a channel, got {}",
                        other.type_name()
                    )))
                }
            },
            other => {
                return Err(RjError::type_err(format!(
                    "alts!!: each op must be a channel or a [chan val] pair, got {}",
                    other.type_name()
                )))
            }
        }
    }
    Ok(ops)
}

impl AltOp {
    fn chan(&self) -> &Arc<Chan> {
        match self {
            AltOp::Take(ch) | AltOp::Put(ch, _) => ch,
        }
    }
}

/// Defensive backstop for a parked `alts!!`, deliberately the same 2s as
/// `builtins::flow`'s `PARK_TIMEOUT` (the two parks are the same shape:
/// register a doorbell, snapshot, scan, park). Never relied on for
/// correctness -- every real event rings the doorbell -- so a blocked
/// `alts!!` wakes at most once every 2s, ~4000x less often than the 500µs
/// spin this replaced.
const ALTS_PARK_TIMEOUT: Duration = Duration::from_secs(2);

/// RAII registration of ONE `alts!!` call's doorbell on every chan it is
/// selecting over. Registration must happen BEFORE the first scan (that's
/// half of the missed-wakeup argument -- see [`Doorbell`]'s
/// "missed-wakeup correctness"), and un-registration must happen on EVERY
/// exit path out of the native: a leaked entry is a doorbell nobody will
/// ever wait on again, rung on every put/take/close of what may be a very
/// hot chan, forever. `Drop` is the only construct that covers "returned a
/// value", "propagated an error", and "unwound past this frame" alike, so
/// the un-registration lives there and nowhere else.
struct AltsRegistration {
    /// One entry per registration actually pushed (so a chan appearing
    /// twice in the ops vector is visited twice on drop -- the second
    /// `retain` is then a no-op, since the first already removed both of
    /// that chan's copies).
    chans: Vec<Arc<Chan>>,
    doorbell: Arc<Doorbell>,
}

impl AltsRegistration {
    fn register(ops: &[AltOp], doorbell: Arc<Doorbell>) -> Self {
        let mut chans = Vec::with_capacity(ops.len());
        for op in ops {
            let ch = op.chan();
            lock_mutex(&ch.state).alts_doorbells.push(doorbell.clone());
            chans.push(ch.clone());
        }
        AltsRegistration { chans, doorbell }
    }
}

impl Drop for AltsRegistration {
    fn drop(&mut self) {
        for ch in self.chans.iter() {
            lock_mutex(&ch.state)
                .alts_doorbells
                .retain(|db| !Arc::ptr_eq(db, &self.doorbell));
        }
    }
}

/// One `alts!!` fairness pass: try every op exactly once, in a freshly
/// randomized order (core.async's documented "random choice among ready
/// ops"), returning that op's result value the moment one is ready.
/// `None` means "nothing was ready this pass" -- the caller decides
/// whether that means `:default` or a park.
fn alts_scan(ops: &[AltOp]) -> Option<Value> {
    for &oi in shuffled_indices(ops.len()).iter() {
        match &ops[oi] {
            AltOp::Take(ch) => match chan_try_take(ch) {
                TryTake::Received(v) => return Some(Value::Vector(pvec![v, Value::Channel(ch.clone())])),
                TryTake::Closed => return Some(Value::Vector(pvec![Value::Nil, Value::Channel(ch.clone())])),
                TryTake::WouldBlock => {}
            },
            AltOp::Put(ch, v) => match chan_try_put(ch, v.clone()) {
                TryPut::Sent => return Some(Value::Vector(pvec![Value::Bool(true), Value::Channel(ch.clone())])),
                TryPut::Closed => return Some(Value::Vector(pvec![Value::Bool(false), Value::Channel(ch.clone())])),
                TryPut::WouldBlock => {}
            },
        }
    }
    None
}

/// The blocking `alts!!` protocol itself: register a doorbell on every op
/// chan, then {snapshot, scan, park} until something is ready. Factored out
/// of the native (whose `:default` arm never reaches here) so that
/// `tests/task_chan_test.rs` can drive the REAL registration/scan/park path
/// from Rust rather than a replica of it -- W3's claim is that `alts!!`
/// works from a task with ZERO changes to this protocol, and a test against
/// a copy of it would prove nothing.
///
/// W3 adds nothing here. In task context `Doorbell::new()` captures the
/// shard thread (harmless -- see `owner_thread`'s doc), the scan uses the
/// same `chan_try_take`/`chan_try_put` primitives, and `wait_for_change`
/// takes its task arm internally.
fn alts_park_loop(ops: &[AltOp]) -> Value {
    let doorbell = Arc::new(Doorbell::new());
    let _reg = AltsRegistration::register(ops, doorbell.clone());
    loop {
        let seen = doorbell.current();
        if let Some(res) = alts_scan(ops) {
            return res;
        }
        doorbell.wait_for_change(seen, ALTS_PARK_TIMEOUT);
    }
}

/// `(alts!! [a b])` over two take ops, for `tests/task_chan_test.rs` via
/// `mova::internal::task_chan` (the raw `Chan` primitives are `pub(crate)`,
/// so the test cannot build an `AltOp` itself).
pub(crate) fn alts_take_two(a: Arc<Chan>, b: Arc<Chan>) -> Value {
    alts_park_loop(&[AltOp::Take(a), AltOp::Take(b)])
}

// ---------------------------------------------------------------------------
// `timeout`: ONE process-wide timer thread
// ---------------------------------------------------------------------------

/// What one armed timer entry DOES when its deadline arrives.
///
/// L3.5 item 1 added the second arm. Deliberately an enum on the existing
/// entry rather than a second service: ONE heap, ONE thread, ONE mutex is
/// the whole claim of this module (see [`TIMER`]), and a deadline-waker is
/// the same "wait, then flip something" job a `timeout` already is.
enum TimerAction {
    /// `timeout`'s original entry, unchanged: close the channel.
    Close(Arc<Chan>),
    /// L3.5 item 1: wake a parked task at the deadline, unless it got what
    /// it was waiting for first and retracted by setting `cancelled`.
    ///
    /// The token is checked ON the timer thread, immediately before the
    /// wake, so a `deref` that already returned costs exactly one relaxed
    /// load here and no wake at all. It is a courtesy, not a safety
    /// requirement: see [`timer_arm_waker`]'s doc for why a wake that DOES
    /// get through to a task that has moved on is harmless.
    Wake {
        waker: TaskWaker,
        cancelled: Arc<std::sync::atomic::AtomicBool>,
    },
    /// TIMER-CANCEL: `timeout-put`'s entry -- at the deadline, CLAIM the
    /// handle's cell and, having won, offer `val` onto `ch`. Unlike
    /// `Wake`'s courtesy token, the claim here is AUTHORITATIVE: fire and
    /// `cancel-timer!` contend for one `swap(true)` with exactly one
    /// winner, which is what makes "cancel returned true" mean "the value
    /// will never be delivered" -- see `TimerCancel`'s doc and
    /// `docs/TIMER-CANCEL-DESIGN.md`.
    ///
    /// Boxed so this arm doesn't grow `TimerEntry` for the entries
    /// `timeout` and timeout-derefs arm by the thousand: a `Value` is 32
    /// bytes, and enum size is max-arm size.
    Put(Box<TimeoutPut>),
}

/// [`TimerAction::Put`]'s payload. Delivery is `chan_try_put` -- the fire
/// runs on the timer thread (real mode) or the sim shard loop, neither of
/// which may park -- so a full or closed `ch` drops the put silently.
/// That is the documented consumer contract (arm cancellable timers at
/// buffered/sliding/dropping chans sized for them), not a shortcut.
struct TimeoutPut {
    ch: Arc<Chan>,
    val: Value,
    cancel: Arc<crate::value::TimerCancel>,
}

impl TimerAction {
    /// L5 / P6a: this entry's letter in the sim trace's `F <seq> <kind>`.
    fn kind(&self) -> char {
        match self {
            TimerAction::Close(_) => 'C',
            TimerAction::Wake { .. } => 'W',
            // TIMER-CANCEL: a new letter appears only in traces of
            // programs that use the new surface, so every pre-existing
            // trace stays byte-identical.
            TimerAction::Put(_) => 'P',
        }
    }

    /// L5 / P6a: `true` for a `Wake` whose waiter already got what it wanted
    /// and retracted. The sim advance rule drops these instead of jumping
    /// virtual time to a no-op -- see [`sim_advance_and_fire`] step 1.
    fn is_cancelled(&self) -> bool {
        match self {
            TimerAction::Close(_) => false,
            TimerAction::Wake { cancelled, .. } => cancelled.load(Ordering::Relaxed),
            // TIMER-CANCEL: a settled cell on an entry still IN the heap
            // can only mean `cancel-timer!` won (a fired entry was popped
            // first), so "settled" here IS "cancelled". In sim,
            // cancellation happens on this very thread -- deterministic,
            // same as `Wake`.
            TimerAction::Put(p) => !p.cancel.armed(),
        }
    }

    /// Run this entry's deadline effect. Called by [`timer_loop`] with the
    /// heap lock RELEASED -- both arms reach arbitrary other threads' wake
    /// paths.
    fn fire(self) {
        TIMER_FIRES.fetch_add(1, Ordering::Relaxed);
        match self {
            TimerAction::Close(ch) => chan_close(&ch),
            TimerAction::Wake { waker, cancelled } => {
                if !cancelled.load(Ordering::Relaxed) {
                    waker.wake();
                }
            }
            // TIMER-CANCEL: contend for the cell; deliver only on a win.
            // The `chan_try_put` is the same lock-then-wake class of work
            // as `chan_close` above, so R5 (heap lock released before any
            // fire) covers it with nothing new to say. Its result is
            // deliberately dropped -- see `TimeoutPut`'s doc.
            TimerAction::Put(p) => {
                if p.cancel.claim() {
                    let _ = chan_try_put(&p.ch, p.val);
                }
            }
        }
    }
}

/// One armed timer entry: run `action` at `deadline`. `seq` is a monotonic
/// tiebreaker so two entries armed for the same instant fire in arming
/// order, and -- more importantly -- so `Ord` below is a TOTAL order
/// without `Arc<Chan>` having to be `Ord` (it isn't, and shouldn't be:
/// channel identity is pointer identity, which says nothing about
/// scheduling).
struct TimerEntry {
    deadline: Instant,
    seq: u64,
    action: TimerAction,
}

impl PartialEq for TimerEntry {
    fn eq(&self, other: &Self) -> bool {
        self.deadline == other.deadline && self.seq == other.seq
    }
}
impl Eq for TimerEntry {}

impl Ord for TimerEntry {
    /// REVERSED on purpose: `BinaryHeap` is a MAX-heap and this wants the
    /// EARLIEST deadline on top, so "greater" here means "sooner".
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        other.deadline.cmp(&self.deadline).then_with(|| other.seq.cmp(&self.seq))
    }
}
impl PartialOrd for TimerEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Default)]
struct TimerHeap {
    entries: std::collections::BinaryHeap<TimerEntry>,
    next_seq: u64,
}

struct TimerService {
    heap: std::sync::Mutex<TimerHeap>,
    cv: std::sync::Condvar,
}

/// The one timer service, created on the first `timeout` call in the
/// process and never torn down.
///
/// **Why process-static, and why it is never joined.** Every `timeout`
/// used to spawn its own detached thread that slept and then closed one
/// channel: one OS thread (with its full stack reservation) per call,
/// paid up front, for a task whose entire content is "wait, then flip a
/// flag". A downstream timer service issuing timeouts at any rate at all
/// pays that over and over. One thread servicing a deadline-ordered heap
/// does the identical work.
///
/// It is deliberately NOT owned by an `Interp` and deliberately NOT joined
/// at engine shutdown, in conscious deviation from the "engine-shutdown
/// join" an embedded-resource design note would normally ask for:
///
/// - It is SHARED across `Interp` instances (an embedder can hold several,
///   and `Interp::fork` makes more), so tying its life to any one of them
///   would let one engine's shutdown silently break another's `timeout`s.
/// - It runs ZERO interpreted code -- it touches nothing but `Instant`, a
///   heap of [`TimerAction`]s, `chan_close` and `TaskWaker::wake`. It cannot
///   keep an `Interp` or an `Env` alive, so there is nothing for a shutdown
///   to reclaim by joining it. (`Arc<Chan>`s for timeouts that have not
///   fired yet ARE held, and so are the `Arc<TaskShared>`s of tasks parked
///   in a timeout-`deref` -- bounded by the number of live un-fired
///   entries, and each one released the moment it fires.)
/// - Between firings it is parked in a `Condvar` wait, holding no locks
///   and owning no resources that need flushing, which is exactly the
///   state in which a pure-Rust thread is safe to leave running when the
///   process exits.
static TIMER: OnceLock<TimerService> = OnceLock::new();

/// How many timer-service threads this process has ever spawned. The whole
/// point of this module is that the answer is 1 no matter how many
/// `timeout`s are armed (0 if none ever were), so it is worth being able
/// to assert on -- reached from `tests/timer_service_test.rs` through
/// `mova::internal::async_timer::threads_spawned`, the same white-box
/// hook shape as `Doorbell::safety_net_hits`.
static TIMER_THREADS_SPAWNED: AtomicU64 = AtomicU64::new(0);

pub(crate) fn timer_threads_spawned() -> u64 {
    TIMER_THREADS_SPAWNED.load(Ordering::Relaxed)
}

/// How many entries this process has ever PUSHED onto the timer heap --
/// i.e. how many times [`timer_push`] has run, over both arms
/// ([`timer_arm`] and [`timer_arm_waker`]). Read-only: `next_seq` is
/// already maintained by `timer_push` (it is the heap's tie-break key), so
/// this adds no counter and no work to the arming path; it just reads the
/// number that is already there, under the heap lock it is written under.
/// `0` when the service was never started (no `timeout`, no polling deref
/// has ever run in this process), which is the same "nothing happened yet"
/// answer [`timer_threads_spawned`] gives.
///
/// Same white-box-hook shape and rationale as [`timer_threads_spawned`],
/// reached from `tests/l35_deref_park_probe.rs` through
/// `mova::internal::async_timer::arms_total`: L3.5 item 1 asks what the
/// 1ms timeout-deref tick actually costs, and arms/second through this one
/// global heap is half of that question (the other half is CPU).
pub(crate) fn timer_arms_total() -> u64 {
    match TIMER.get() {
        None => 0,
        Some(service) => lock_mutex(&service.heap).next_seq,
    }
}

/// L5/W4 — how many timer entries this process has ever FIRED, i.e. reached
/// [`TimerAction::fire`]. Distinct from [`timer_arms_total`] in exactly the
/// two ways that matter to `simulate`'s `:timer-fires`: an entry that is
/// still armed has not fired, and a cancelled `Wake` dropped by the sim
/// advance rule's step 1 never fires at all (it is a no-op that must not
/// inflate the count, or the "30 virtual days in 40 ms" story would be told
/// in entries that did nothing).
///
/// One relaxed `fetch_add` per deadline reached, on a path that is already
/// about to close a channel or wake a task — unmeasurable against either.
static TIMER_FIRES: AtomicU64 = AtomicU64::new(0);

pub(crate) fn timer_fires_total() -> u64 {
    TIMER_FIRES.load(Ordering::Relaxed)
}

/// Upper bound on how far out a `timeout` can be armed (~10 years), purely
/// so `Instant + Duration` can never overflow and panic on an absurd
/// argument. Indistinguishable from the old behavior: a thread that slept
/// for a decade and a thread that slept for a millennium both close their
/// channel strictly after this process is gone.
const MAX_TIMEOUT_MS: u64 = 10 * 365 * 24 * 60 * 60 * 1000;

/// Arms `ch` to be closed `ms` from now, starting the timer thread if this
/// is the process's first `timeout`.
pub(crate) fn timer_arm(ch: Arc<Chan>, ms: u64) {
    timer_push(TimerAction::Close(ch), ms);
}

/// Arms `waker` to be woken `ms` from now (the [`TimerAction::Wake`] arm),
/// returning the deadline it actually armed. Setting `cancelled` before that
/// deadline suppresses the wake.
///
/// **The returned `Instant` is the contract.** The caller's "have I timed
/// out?" test must be `Instant::now() >= <this>`, not a deadline it computed
/// itself: then "the timer fired" implies "the caller's deadline has passed"
/// by construction ([`timer_loop`] pops only entries with `deadline <= now`),
/// so a caller that parks ONCE and re-parks on every wake it cannot explain
/// can never re-park past its own deadline and hang. Deriving the deadline
/// twice -- once here, once at the call site -- would reintroduce exactly
/// that hazard through millisecond rounding and through [`MAX_TIMEOUT_MS`].
///
/// **Cancellation is LAZY, and that is the accepted trade.** A cancelled
/// entry stays in the heap until its deadline arrives, then costs one relaxed
/// load and is dropped. So a resolved `(deref p 30000 :x)` leaves one dead
/// entry behind for up to 30 s. The bound is "live + recently-resolved
/// timeout-derefs", i.e. the same order as the waiters themselves, and each
/// entry is three words plus two `Arc` bumps. An O(log n) removal scheme (a
/// handle back into the heap, or a slotted heap with sift-down on cancel) is
/// NOT wanted: it would put a second index and a second invariant on the one
/// structure this module keeps deliberately trivial, to reclaim memory that
/// is already bounded and already self-clearing.
///
/// **A wake that gets through is harmless.** The cancel token races: the
/// timer can pass the `!cancelled` check and then wake a task that has since
/// returned from the deref and parked on something else -- or finished. That
/// is the documented no-op every park in this runtime is built on
/// (`TaskWaker::wake` on a `DONE` task fails its `PARKED -> READY` CAS and
/// returns; on a live task it leaves a `NOTIFIED` that the next park absorbs
/// and re-checks). The token exists to keep that from being the COMMON case,
/// not to make it safe.
pub(crate) fn timer_arm_waker(
    waker: TaskWaker,
    ms: u64,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
) -> Instant {
    timer_push(TimerAction::Wake { waker, cancelled }, ms)
}

/// The arming path both public arms share: start the service on first use,
/// push one entry, poke the thread. Returns the deadline it armed.
fn timer_push(action: TimerAction, ms: u64) -> Instant {
    let service = TIMER.get_or_init(|| TimerService {
        heap: std::sync::Mutex::new(TimerHeap::default()),
        cv: std::sync::Condvar::new(),
    });
    // Spawn outside the heap lock, and exactly once: `OnceLock::get_or_init`
    // above already serialized "who creates the service", so hanging the
    // spawn off a second `OnceLock` keyed to the same first call keeps the
    // thread count at 1 without a second synchronization scheme.
    //
    // L5 / P6a: in SIM mode the thread is never spawned at all. The heap is
    // drained inline by the sim shard loop's advance rule
    // ([`sim_advance_and_fire`]), so a timer thread would be a second,
    // wall-clock-driven mutator of exactly the state the simulation owns.
    // `timer_threads_spawned() == 0` is therefore a sim invariant (design §2,
    // gate rule 4). The `OnceLock` is still consulted so the branch stays a
    // single load on the arming path in real mode.
    static STARTED: OnceLock<()> = OnceLock::new();
    if !crate::clock::sim_enabled() {
        STARTED.get_or_init(|| {
        let started = std::thread::Builder::new()
            .name("mova-timer".to_string())
            .spawn(crate::memstat::drained(|| timer_loop(TIMER.get().expect("the service is initialized before the spawn"))));
        match started {
            Ok(handle) => {
                // Detached: see `TIMER`'s doc for why this thread is never
                // joined.
                drop(handle);
                TIMER_THREADS_SPAWNED.fetch_add(1, Ordering::Relaxed);
            }
            // Nothing sensible is left to do if the OS refuses one thread:
            // the alternative was `timeout` returning an error, which no
            // caller could act on either. Every armed timeout simply never
            // fires, and the counter stays 0 to say so.
            Err(e) => eprintln!("mova: couldn't spawn the timer thread: {e}"),
        }
        });
    }

    // L5: THE clock (design §2). Real mode IS `Instant::now()`; sim mode is
    // the virtual now, so an arm made at virtual t lands at virtual t+ms and
    // the advance rule's "jump to the earliest deadline" is exact.
    let deadline = crate::clock::clock_now() + Duration::from_millis(ms.min(MAX_TIMEOUT_MS));
    let mut g = lock_mutex(&service.heap);
    let seq = g.next_seq;
    g.next_seq = g.next_seq.wrapping_add(1);
    g.entries.push(TimerEntry { deadline, seq, action });
    drop(g);
    // One consumer, so one wake is enough.
    service.cv.notify_one();
    // L5 / P6b: in sim there IS no consumer on that condvar — the sim shard
    // drains this heap inline at its idle point, and only an unpark breaks
    // that idle point. Without this line an OS thread arming a timer (which
    // is what every `(<!! (timeout ms))` in every existing test does) pushes
    // onto a heap nobody is watching and then blocks forever: a STRUCTURAL
    // hang, not a schedule. No-op in real mode; see
    // `runtime::sim_unpark_shard` for why it is unconditional within sim.
    crate::runtime::sim_unpark_shard();
    deadline
}

/// The timer thread's whole life: pop everything due, close it, then sleep
/// until the next deadline (or indefinitely, when nothing is armed).
fn timer_loop(service: &'static TimerService) -> ! {
    loop {
        let due: Vec<TimerAction> = {
            let mut g = lock_mutex(&service.heap);
            // Real-mode-only thread (never spawned in sim): this read goes
            // through THE clock so the tree-wide G-DET-2 grep stays
            // mechanically clean. `clock_now()` here is `Instant::now()`
            // plus one relaxed flag load — behaviorally identical.
            let now = crate::clock::clock_now();
            let mut due = Vec::new();
            while g.entries.peek().is_some_and(|e| e.deadline <= now) {
                due.push(g.entries.pop().expect("just peeked").action);
            }
            due
        };
        if !due.is_empty() {
            // NEVER hold the heap lock across a firing: `chan_close` takes
            // the chan's own lock, notifies its condvar, and rings both
            // doorbell families, and `TaskWaker::wake` takes a shard's inbox
            // mutex -- arbitrary other threads' wake paths either way.
            // Holding the heap lock through that would put the timer
            // service's lock underneath every one of them in the lock
            // order for no reason at all.
            for action in due {
                action.fire();
            }
            continue;
        }
        let g = lock_mutex(&service.heap);
        match g.entries.peek().map(|e| e.deadline) {
            // Nothing armed: sleep until `timer_arm` notifies. (This is
            // also where the thread spends a process's whole life if it
            // was started by a `timeout` that has since fired.)
            None => drop(cv_wait(&service.cv, g)),
            Some(deadline) => {
                // Same real-mode-only clock read as above, for the same
                // G-DET-2 reason: sleep sizing off THE clock, not a raw
                // `Instant::now()`.
                let left = deadline.saturating_duration_since(crate::clock::clock_now());
                if left.is_zero() {
                    // Became due between the pop pass and this re-lock.
                    drop(g);
                } else {
                    // A `timer_arm` for a SOONER deadline notifies, so this
                    // is not merely a poll interval: it is the exact next
                    // deadline, re-derived after every wake.
                    drop(cv_wait_timeout(&service.cv, g, left));
                }
            }
        }
    }
}

/// L5/W4 — drop every entry still armed on the timer heap, WITHOUT firing it.
///
/// Called once, by the sim scheduler's teardown, at the end of a `simulate`
/// call. On the ordinary path it finds nothing: a call reaches quiescence
/// only when [`sim_advance_and_fire`] has said the heap holds nothing live,
/// so an empty heap is what "the world stopped" MEANS. It matters on the two
/// abnormal exits — the deadlock tripwire and `:max-resumes` — where the
/// world is stopped by decree rather than by exhaustion and entries can still
/// be armed.
///
/// Dropped rather than fired, because every one of them was armed BY a task
/// this teardown has just destroyed: firing them would jump virtual time to a
/// deadline nobody is waiting for and charge it to the NEXT call
/// (`:virtual-ms` and `:timer-fires` are per-call deltas, and a stowaway from
/// the previous call would make both lie). Returns how many it dropped, for
/// the caller that wants to say so.
pub(crate) fn sim_drop_armed_timers() -> usize {
    debug_assert!(crate::clock::sim_enabled());
    let Some(service) = TIMER.get() else {
        return 0;
    };
    let mut g = lock_mutex(&service.heap);
    let n = g.entries.len();
    // Drained under the lock into nothing: an entry's `Drop` releases an
    // `Arc<Chan>` or an `Arc<AtomicBool>` plus a `TaskWaker` (or, for a
    // `Put`, a `TimeoutPut` whose `Value`'s own `Drop` never re-enters
    // this heap either), so the R5 lock-order rule (never hold the heap
    // lock across a FIRE) is not in play — nothing is being fired.
    g.entries.clear();
    n
}

/// L5 / P6a — the sim scheduler's virtual-time advance rule (design §2).
///
/// Called by `runtime::sim_next_job` at the ONE point the shard loop has
/// nothing runnable: kills, `local`, `spawns` and `ready` are all empty, and
/// at one shard that is the definition of "the task world is quiescent".
///
/// Three steps, in order:
///
/// 1. **Drop cancelled `Wake` entries off the top of the heap.** A resolved
///    `(deref p 30000 :x)` leaves a dead entry behind (see
///    [`timer_arm_waker`]'s LAZY-cancellation doc); jumping virtual time 30 s
///    to fire a no-op would make the compression ratio a lie and would let a
///    cancelled timeout reorder live ones. Only the TOP matters for the jump
///    decision — a cancelled entry deeper in the heap gets the same treatment
///    when it surfaces. Deterministic because in sim, cancellation happens on
///    this very thread.
/// 2. **Jump**, if anything is left: `SIM_NOW_NS` becomes the earliest live
///    deadline, and ALL entries due at that instant come off the heap in
///    `seq` order (the heap's `Ord` is deadline-then-seq, so a pop sequence
///    IS arming order among ties — design §4 item 5).
/// 3. **Fire, with the heap lock RELEASED** — design §8 R5, and byte for
///    byte the rule [`timer_loop`] already documents: `chan_close` takes the
///    chan lock and rings doorbells, `TaskWaker::wake` takes a shard inbox
///    mutex. Popping into a `Vec` under the lock and firing after the drop is
///    what keeps the timer heap out from underneath every other lock in the
///    process.
///
/// Returns `true` when it fired something (the caller re-drains instead of
/// parking) and `false` when the heap held nothing live (the caller parks —
/// the only thing that can wake it then is the boundary thread's root spawn,
/// design §8 R6).
pub(crate) fn sim_advance_and_fire() -> bool {
    debug_assert!(crate::clock::sim_enabled());
    // No `timeout`/`deref` has ever run in this process: nothing to advance
    // to, and nothing to construct either.
    let Some(service) = TIMER.get() else {
        return false;
    };

    let due: Vec<(u64, TimerAction)> = {
        let mut g = lock_mutex(&service.heap);
        // Step 1.
        while g.entries.peek().is_some_and(|e| e.action.is_cancelled()) {
            g.entries.pop();
        }
        // Step 2.
        let Some(target) = g.entries.peek().map(|e| e.deadline) else {
            return false;
        };
        crate::clock::sim_jump_to(target);
        let mut due = Vec::new();
        while g.entries.peek().is_some_and(|e| e.deadline <= target) {
            let e = g.entries.pop().expect("just peeked");
            due.push((e.seq, e.action));
        }
        due
    };

    // Step 3.
    let traced = crate::clock::trace_on();
    for (seq, action) in due {
        if traced {
            crate::clock::trace_timer_fire(seq, action.kind());
        }
        action.fire();
    }
    true
}

// ---------------------------------------------------------------------------
// L1/W4: `go*`/`put!`/`take!` route to the task runtime by default, `thread*`
// never does. One body per native, shared between whichever placement
// (task or thread) actually runs it, so the two placements cannot drift.
// ---------------------------------------------------------------------------

/// `MOVA_GO_THREADS=1`, read exactly once (house pattern --
/// `builtins::flow::faststep_disabled_by_env`'s `MOVA_NO_FASTSTEP`
/// treatment of `OnceLock`, so no `go*`/`put!`/`take!` call ever touches the
/// environment). Set: `go*`/`put!`/`take!` revert wholesale to the pre-L1
/// real-OS-thread path -- `thread*`'s body, verbatim. Unset (the default):
/// all three spawn a task instead.
fn go_threads_kill_switch() -> bool {
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| std::env::var("MOVA_GO_THREADS").is_ok_and(|v| v == "1"))
}

/// **Fence #4 (design §4, L5/W3): `MOVA_GO_THREADS=1` is the anti-sim
/// configuration.** It reverts `go*`/`put!`/`take!` wholesale to real OS
/// threads — i.e. it empties the task world sim is a simulation OF. P6b's F4
/// measured what that costs: with zero tasks the shard is always idle, so
/// every virtual deadline fires the instant it is armed while the OS threads
/// limp along on wall time, and the program gets WRONG answers rather than
/// merely nondeterministic ones. Refused at the first `go*`, loudly, rather
/// than silently producing them.
///
/// Checked at the THREE natives the kill switch actually diverts — `go*`,
/// `put!` and `take!` — rather than at process start, so a real-mode process
/// (or a sim process that never reaches one of them) is unaffected: the same
/// "cost only what you use" discipline every kill switch in this tree
/// follows. `put!`/`take!` call it inside their kill-switch branch, so their
/// default task placement pays nothing at all; `go*` calls it first thing,
/// before it allocates a result chan it would only throw away.
///
/// All three, not just `go*`: a program that drives its channels entirely
/// through `put!`/`take!` callbacks would otherwise slip the fence and get
/// the same wrong answers by a different door.
fn refuse_go_threads_under_sim() -> Result<(), RjError> {
    if crate::clock::sim_enabled() && go_threads_kill_switch() {
        return Err(RjError::other(
            "sim mode refuses MOVA_GO_THREADS=1 (L5 fence #4): it reverts go* to OS threads, \
             which empties the task world sim schedules -- every virtual deadline would then fire \
             instantly against wall-clock threads (P6b F4). Unset MOVA_GO_THREADS to run under sim.",
        ));
    }
    Ok(())
}

/// The whole contract of a `go`/`go-loop`/`thread` body, independent of
/// where it runs: call `f` under the forked `Interp`, put its non-nil result
/// on `result_ch` exactly once, then close it. An escaping Mova error is
/// rendered to stderr -- the same v0 posture an uncaught error escaping the
/// REPL gets, since there is no supervisor or error channel yet -- and the
/// channel just closes, so a `<!!`/`<!` on it observes `nil`, same as a
/// body that returned `nil`. Shared by `go*`'s task and thread placements
/// and by `thread*`'s always-thread one, so the three cannot drift apart.
///
/// `conveyed` is the SPAWNING side's dynamic environment, snapshotted at the
/// `go*`/`thread*` call site (see the two callers) -- real Clojure conveys
/// bindings into `go`/`thread` bodies (measured: `(binding [*a* 42] @(go
/// *a*))` is `42`), and `future*` (`builtins/conc.rs`) already establishes
/// snapshot-at-spawn as this codebase's conveyance moment. Installed as the
/// body's first act, RAII-popped on return AND on unwind, mirroring
/// `future*`'s guard exactly.
fn run_go_body(mut forked: Interp, f: Value, result_ch: Arc<Chan>, conveyed: Vec<(Arc<VarCell>, Value)>) {
    let _bindings = crate::env::BindingConveyance::install(conveyed);
    let source_name = forked.source_name.clone();
    let source = forked.source.clone();
    match forked.call(&f, &[]) {
        Ok(Value::Nil) => {}
        Ok(v) => {
            chan_put(&result_ch, v);
        }
        Err(e) => {
            eprintln!("{}", crate::error::render(&e, &source_name, &source));
        }
    }
    chan_close(&result_ch);
}

/// `go*`/`thread*`'s OS-thread placement: spawn a real `ASYNC_STACK_SIZE`
/// thread running [`run_go_body`]. `name` also seeds the "couldn't spawn"
/// error message, matching each native's own name. `conveyed` is forwarded
/// verbatim to `run_go_body`, unsnapshotted here -- the snapshot is taken on
/// the SPAWNING thread by the caller, before this fn ever runs, since by the
/// time a real OS thread starts running its closure the parent's dynamic
/// env is no longer reachable from `thread_local` storage.
fn spawn_go_thread(
    name: &'static str,
    forked: Interp,
    f: Value,
    ch: Arc<Chan>,
    conveyed: Vec<(Arc<VarCell>, Value)>,
) -> Result<Value, RjError> {
    let thread_ch = ch.clone();
    let spawn_result = std::thread::Builder::new()
        .name(name.to_string())
        .stack_size(ASYNC_STACK_SIZE)
        .spawn(crate::memstat::drained(move || run_go_body(forked, f, thread_ch, conveyed)));
    match spawn_result {
        Ok(handle) => {
            drop(handle);
            Ok(Value::Channel(ch))
        }
        Err(e) => Err(RjError::other(format!("{name}: couldn't spawn thread: {e}"))),
    }
}

/// `go*`'s task placement: spawn [`run_go_body`] as a task on
/// `crate::runtime` instead of an OS thread. Infallible (`runtime::spawn`
/// never fails to enqueue), unlike the thread path. `conveyed` -- see
/// [`spawn_go_thread`]'s doc, same reasoning applies to a task switch.
fn spawn_go_task(forked: Interp, f: Value, ch: Arc<Chan>, conveyed: Vec<(Arc<VarCell>, Value)>) -> Value {
    let task_ch = ch.clone();
    crate::runtime::spawn(move || run_go_body(forked, f, task_ch, conveyed));
    Value::Channel(ch)
}

/// `put!`'s whole contract, independent of placement: put `val` on `ch`,
/// then (if given) invoke `cb` with whether the put succeeded. `conveyed`
/// -- see [`run_go_body`]'s doc; `put!`'s callback runs on the spawned
/// task/thread same as a go body does, so it gets the same conveyance
/// (measured Clojure: a `put!` callback sees the caller's `binding`s).
fn run_put_body(mut forked: Interp, ch: Arc<Chan>, val: Value, cb: Option<Value>, conveyed: Vec<(Arc<VarCell>, Value)>) {
    let _bindings = crate::env::BindingConveyance::install(conveyed);
    let sent = chan_put(&ch, val);
    if let Some(cb) = cb {
        let _ = forked.call(&cb, &[Value::Bool(sent)]);
    }
}

/// `take!`'s whole contract, independent of placement: take from `ch`
/// (`nil` if closed with nothing left), then (if given) invoke `cb` with
/// the value. `conveyed` -- see [`run_put_body`]'s doc, identical reasoning.
fn run_take_body(mut forked: Interp, ch: Arc<Chan>, cb: Option<Value>, conveyed: Vec<(Arc<VarCell>, Value)>) {
    let _bindings = crate::env::BindingConveyance::install(conveyed);
    let v = chan_take(&ch).unwrap_or(Value::Nil);
    if let Some(cb) = cb {
        let _ = forked.call(&cb, &[v]);
    }
}

pub fn register(i: &mut Interp) {
    reg(i, "chan", ArityHint::Range(0, 1), |_i, args| {
        let policy = match args.first() {
            None => BufferPolicy::Unbuffered,
            Some(v) => parse_buffer_arg(v)?,
        };
        Ok(Value::Channel(Arc::new(Chan::new(policy))))
    });

    reg(i, "dropping-buffer", ArityHint::Exact(1), |_i, args| {
        Ok(buffer_descriptor("dropping", require_nonneg_int(&args[0], "dropping-buffer")?))
    });

    reg(i, "sliding-buffer", ArityHint::Exact(1), |_i, args| {
        Ok(buffer_descriptor("sliding", require_nonneg_int(&args[0], "sliding-buffer")?))
    });

    reg(i, ">!!", ArityHint::Exact(2), |_i, args| {
        let ch = require_chan(&args[0], ">!!")?;
        if matches!(args[1], Value::Nil) {
            return Err(nil_put_err());
        }
        // Interruptible like `<!!` (a put on a full channel can block for good).
        let _g = crate::interrupt::WaitGuard::arm(&_i.intr);
        let ok = chan_put(ch, args[1].clone());
        if crate::interrupt::WAIT_ABORT.with(|c| c.get()) {
            return Err(_i.intr.take_err().unwrap_or_else(|| RjError::interrupted("interrupted")));
        }
        Ok(Value::Bool(ok))
    });

    reg(i, "<!!", ArityHint::Exact(1), |i, args| {
        let ch = require_chan(&args[0], "<!!")?;
        // P0c: arm the interruptible wait for this call only.
        let _g = crate::interrupt::WaitGuard::arm(&i.intr);
        match chan_take(ch) {
            Some(v) => Ok(v),
            None => {
                if crate::interrupt::WAIT_ABORT.with(|c| c.get()) {
                    return Err(i.intr.take_err().unwrap_or_else(|| RjError::interrupted("interrupted")));
                }
                Ok(Value::Nil)
            }
        }
    });

    reg(i, "close!", ArityHint::Exact(1), |_i, args| {
        let ch = require_chan(&args[0], "close!")?;
        chan_close(ch);
        Ok(Value::Nil)
    });

    reg(i, "poll!", ArityHint::Exact(1), |_i, args| {
        let ch = require_chan(&args[0], "poll!")?;
        Ok(match chan_try_take(ch) {
            TryTake::Received(v) => v,
            TryTake::Closed | TryTake::WouldBlock => Value::Nil,
        })
    });

    reg(i, "offer!", ArityHint::Exact(2), |_i, args| {
        let ch = require_chan(&args[0], "offer!")?;
        if matches!(args[1], Value::Nil) {
            return Err(nil_put_err());
        }
        // Matches real core.async: `true` on an immediate successful put,
        // `false` specifically for an already-closed channel, and `nil`
        // (NOT `false`) when the put would simply block right now (e.g. a
        // full but still-open fixed buffer) -- confirmed against babashka
        // v1.13's `clojure.core.async/offer!`, which distinguishes
        // "would block" from "closed" instead of collapsing both to
        // `false`.
        Ok(match chan_try_put(ch, args[1].clone()) {
            TryPut::Sent => Value::Bool(true),
            TryPut::Closed => Value::Bool(false),
            TryPut::WouldBlock => Value::Nil,
        })
    });

    reg(i, "timeout", ArityHint::Exact(1), |_i, args| {
        let ms = require_nonneg_int(&args[0], "timeout")? as u64;
        // Never put to, so the policy is irrelevant; `Fixed(0)` documents
        // that intent (a `timeout` channel's only observable event is its
        // eventual close).
        let ch = Arc::new(Chan::new(BufferPolicy::Fixed(0)));
        // One shared timer thread for the whole process, not one thread per
        // call -- see `TIMER`'s doc. Never fallible from here: the arming
        // itself is a heap push, and a refused thread spawn is reported
        // once, at the spawn, rather than turned into an error every
        // `timeout` caller would have to handle.
        timer_arm(ch.clone(), ms);
        Ok(Value::Channel(ch))
    });

    // TIMER-CANCEL (mova-native; upstream core.async has no cancellable
    // timer -- same superset class as `flow/stop-proc`): "after `ms`,
    // offer `val` onto `ch` -- unless I change my mind." The handle is
    // one claim cell that this timer's fire and `cancel-timer!` race
    // for; see `TimerCancel`'s doc and `docs/TIMER-CANCEL-DESIGN.md`.
    // `timeout` itself deliberately stays fire-and-forget: its
    // chan-close contract is upstream surface, and "cancel closes the
    // chan / never closes it" both wedge parked takers -- a cancellable
    // timer therefore PUTS, so a cancelled one is simply a message that
    // never arrives at a chan the consumer was reading anyway.
    reg(i, "timeout-put", ArityHint::Exact(3), |_i, args| {
        let ms = require_nonneg_int(&args[0], "timeout-put")? as u64;
        let ch = require_chan(&args[1], "timeout-put")?;
        if matches!(args[2], Value::Nil) {
            return Err(nil_put_err());
        }
        let cancel = Arc::new(crate::value::TimerCancel::new());
        timer_push(
            TimerAction::Put(Box::new(TimeoutPut {
                ch: ch.clone(),
                val: args[2].clone(),
                cancel: cancel.clone(),
            })),
            ms,
        );
        Ok(Value::Timer(cancel))
    });

    reg(i, "cancel-timer!", ArityHint::Exact(1), |_i, args| {
        let t = require_timer(&args[0], "cancel-timer!")?;
        // `true` iff THIS call won the claim -- i.e. the value will
        // never be delivered. A cancel that lost (the timer fired, or an
        // earlier cancel won) returns `false`; never an error, so
        // "cancel on the way out, whatever state it's in" is always
        // legal. The heap slot is reclaimed lazily at the deadline, the
        // same accepted trade `timer_arm_waker` documents.
        Ok(Value::Bool(t.claim()))
    });

    reg(i, "timer-armed?", ArityHint::Exact(1), |_i, args| {
        let t = require_timer(&args[0], "timer-armed?")?;
        Ok(Value::Bool(t.armed()))
    });

    reg(i, "put!", ArityHint::Range(2, 3), |interp, args| {
        let ch = require_chan(&args[0], "put!")?.clone();
        if matches!(args[1], Value::Nil) {
            return Err(nil_put_err());
        }
        let val = args[1].clone();
        let cb = args.get(2).cloned();
        let forked = interp.fork();
        // M4b conveyance: snapshotted HERE, on the calling thread/task,
        // before the spawn -- see `run_go_body`'s doc for why this must
        // happen at the call site rather than inside the spawned body.
        let conveyed = crate::env::snapshot_thread_bindings();
        // L1/W4: a task by default (S4 -- a blocking callback then blocks
        // that task's shard, same R1 class as a blocking `go` body), the
        // pre-L1 detached thread under the kill switch.
        if go_threads_kill_switch() {
            // L5/W3 fence #4 -- see `refuse_go_threads_under_sim`. Placed
            // INSIDE the kill-switch branch, not at the top of the native
            // like `go*`'s: this is the OS-thread placement, the only arm
            // the fence is about, and the default (task) path below must not
            // pay even one flag load per `put!`. Refused BEFORE the spawn.
            refuse_go_threads_under_sim()?;
            let spawn_result = std::thread::Builder::new()
                .name("mova-put!".to_string())
                .stack_size(ASYNC_STACK_SIZE)
                .spawn(crate::memstat::drained(move || run_put_body(forked, ch, val, cb, conveyed)));
            spawn_result.map_err(|e| RjError::other(format!("put!: couldn't spawn thread: {e}")))?;
        } else {
            crate::runtime::spawn(move || run_put_body(forked, ch, val, cb, conveyed));
        }
        Ok(Value::Nil)
    });

    reg(i, "take!", ArityHint::Range(1, 2), |interp, args| {
        let ch = require_chan(&args[0], "take!")?.clone();
        let cb = args.get(1).cloned();
        let forked = interp.fork();
        let conveyed = crate::env::snapshot_thread_bindings();
        if go_threads_kill_switch() {
            // L5/W3 fence #4 -- see [`refuse_go_threads_under_sim`] and
            // `put!`'s identical placement note just above.
            refuse_go_threads_under_sim()?;
            let spawn_result = std::thread::Builder::new()
                .name("mova-take!".to_string())
                .stack_size(ASYNC_STACK_SIZE)
                .spawn(crate::memstat::drained(move || run_take_body(forked, ch, cb, conveyed)));
            spawn_result.map_err(|e| RjError::other(format!("take!: couldn't spawn thread: {e}")))?;
        } else {
            crate::runtime::spawn(move || run_take_body(forked, ch, cb, conveyed));
        }
        Ok(Value::Nil)
    });

    reg(i, "alts!!", ArityHint::Min(1), |_i, args| {
        let ops = parse_alt_ops(&args[0])?;
        // Real core.async asserts `(pos? (count ports))`, and it is right
        // to: with no ops there is nothing to scan, nothing to register a
        // doorbell on, and therefore nothing that could ever ring one --
        // `(alts!! [])` would park forever, waking only to re-scan zero ops
        // every `ALTS_PARK_TIMEOUT`. Rejected here, BEFORE any registration.
        // (`:default` would technically save it, but "an empty ops vector
        // is meaningful as long as you also pass :default" is not a rule
        // worth having; core.async doesn't have it either.)
        if ops.is_empty() {
            return Err(RjError::arity("alts!!: expected at least one op, got an empty ops vector"));
        }
        if (args.len() - 1) % 2 != 0 {
            return Err(RjError::arity("alts!!: expected option key/value pairs after the ops vector"));
        }
        let mut default: Option<Value> = None;
        let mut idx = 1;
        while idx < args.len() {
            if let Value::Keyword(k) = &args[idx] {
                if k.as_ref() == "default" {
                    default = Some(args[idx + 1].clone());
                }
            }
            idx += 2;
        }
        // `:default` never parks, so it never registers a doorbell either:
        // one scan, then the default. Unchanged from the polling version
        // (which also could only ever reach its default on the first
        // pass).
        if let Some(d) = &default {
            if let Some(res) = alts_scan(&ops) {
                return Ok(res);
            }
            return Ok(Value::Vector(pvec![d.clone(), Value::Keyword(Keyword::from("default"))]));
        }
        // Parking path. `Doorbell::new()` is called HERE, on the thread
        // that is about to park, so its `owner_thread` capture is correct.
        // Registration happens BEFORE the first scan and the generation
        // snapshot is taken BEFORE each scan: any put/take/close that
        // lands after the snapshot has already moved the generation by the
        // time we park, so `wait_for_change` returns immediately instead
        // of sleeping through an event it just missed (see [`Doorbell`]'s
        // "missed-wakeup correctness"). `_reg`'s `Drop` un-registers on
        // every exit path, including an unwind.
        //
        // KNOWN LIMITATION, pre-existing and deliberately not addressed
        // here (module doc, "alts!! and the unbuffered rendezvous"): two
        // `alts!!` calls facing each other across ONE UNBUFFERED chan --
        // one with a put op, one with a take op -- can never complete.
        // Neither joins the `waiting_takers` handshake `chan_try_put`'s
        // unbuffered gate requires, so each scans, sees "not ready", and
        // parks. Under the old poll loop the same pair spun forever; they
        // now park forever instead, at a 2s re-scan cadence.
        Ok(alts_park_loop(&ops))
    });

    reg(i, "go*", ArityHint::Exact(1), |interp, args| {
        // L5/W3 fence #4 -- see `refuse_go_threads_under_sim`. First thing,
        // before anything is allocated or forked.
        refuse_go_threads_under_sim()?;
        let f = args[0].clone();
        // Fixed-buffer-1: the go block's own result (if any) is put exactly
        // once, then the channel closes -- a single slot is all that's ever
        // needed, and matches real core.async's result-channel shape.
        let ch = Arc::new(Chan::new(BufferPolicy::Fixed(1)));
        let forked = interp.fork();
        // M4b conveyance -- see `run_go_body`'s doc.
        let conveyed = crate::env::snapshot_thread_bindings();
        // L1/W4: a task by default -- `crate::runtime::spawn` of the exact
        // same `run_go_body` thunk the thread path below runs, same
        // result-chan contract either way. `MOVA_GO_THREADS=1` reverts to
        // the pre-L1 real-OS-thread path wholesale (see
        // `go_threads_kill_switch`'s doc and the module doc's R1/S4 notes).
        if go_threads_kill_switch() {
            spawn_go_thread("mova-go", forked, f, ch, conveyed)
        } else {
            Ok(spawn_go_task(forked, f, ch, conveyed))
        }
    });

    // L1/W4: `thread*` is what `core/async.mova`'s `thread` macro expands
    // into. It ALWAYS uses the OS-thread placement -- unconditionally, kill
    // switch or not -- because it is the documented escape hatch FROM the
    // R1 footgun `go*`'s task placement accepts (a blocking native inside a
    // task blocks its whole shard). It can no longer just be `go*` under
    // another name now that `go*` defaults to tasks: the two natives must
    // be free to diverge in placement while sharing `run_go_body`'s
    // contract, which is exactly what `spawn_go_thread` factors out.
    reg(i, "thread*", ArityHint::Exact(1), |interp, args| {
        // **L5/W3 fence #4 (design §4): REFUSED in sim, not emulated.**
        //
        // `thread*` is the documented escape hatch FROM the task world: its
        // whole contract is "give me a preemptive OS thread, because I am
        // about to block in it". Cooperatively emulating that as a task would
        // silently change the exact semantics the caller reached for
        // `thread` instead of `go` to get -- a blocking native inside it
        // would then wedge the sim's single shard rather than parking one of
        // many OS threads. Routing it to a task (fence #3's treatment of
        // `future*` et al.) is therefore wrong here in a way it is not
        // there, and leaving it as an OS thread is wrong too (P6b F4: the
        // shard reads "no runnable task" as quiescence and jumps virtual
        // time straight past the thread's work). So: an error, naming the
        // fence.
        if crate::clock::sim_enabled() {
            return Err(RjError::other(
                "sim mode refuses thread* (L5 fence #4): thread*'s contract IS a preemptive OS \
                 thread, and simulating it cooperatively would silently change the semantics you \
                 chose it for. Use go instead, or run this program in real mode.",
            ));
        }
        let f = args[0].clone();
        let ch = Arc::new(Chan::new(BufferPolicy::Fixed(1)));
        let forked = interp.fork();
        // M4b conveyance -- see `run_go_body`'s doc.
        let conveyed = crate::env::snapshot_thread_bindings();
        spawn_go_thread("mova-thread", forked, f, ch, conveyed)
    });
}

#[cfg(test)]
mod tests {
    //! Unit-level coverage for the internal `Chan` protocol, complementing
    //! `tests/async_test.rs`'s end-to-end (through-the-language) coverage.
    use super::*;

    #[test]
    fn fixed_buffer_put_take_roundtrip() {
        let ch = Chan::new(BufferPolicy::Fixed(2));
        assert!(chan_put(&ch, Value::Int(1)));
        assert!(chan_put(&ch, Value::Int(2)));
        assert_eq!(chan_take(&ch), Some(Value::Int(1)));
        assert_eq!(chan_take(&ch), Some(Value::Int(2)));
    }

    #[test]
    fn close_then_take_drains_then_nil() {
        let ch = Chan::new(BufferPolicy::Fixed(2));
        assert!(chan_put(&ch, Value::Int(1)));
        chan_close(&ch);
        assert_eq!(chan_take(&ch), Some(Value::Int(1)));
        assert_eq!(chan_take(&ch), None);
        assert_eq!(chan_take(&ch), None);
    }

    #[test]
    fn put_on_closed_channel_returns_false() {
        let ch = Chan::new(BufferPolicy::Fixed(2));
        chan_close(&ch);
        assert!(!chan_put(&ch, Value::Int(1)));
    }

    #[test]
    fn dropping_buffer_keeps_first_n() {
        let ch = Chan::new(BufferPolicy::Dropping(2));
        for n in 1..=4 {
            assert!(chan_put(&ch, Value::Int(n)));
        }
        assert_eq!(chan_take(&ch), Some(Value::Int(1)));
        assert_eq!(chan_take(&ch), Some(Value::Int(2)));
        chan_close(&ch);
        assert_eq!(chan_take(&ch), None);
    }

    #[test]
    fn sliding_buffer_keeps_last_n() {
        let ch = Chan::new(BufferPolicy::Sliding(2));
        for n in 1..=4 {
            assert!(chan_put(&ch, Value::Int(n)));
        }
        assert_eq!(chan_take(&ch), Some(Value::Int(3)));
        assert_eq!(chan_take(&ch), Some(Value::Int(4)));
    }

    #[test]
    fn unbuffered_rendezvous_across_threads() {
        let ch = Arc::new(Chan::new(BufferPolicy::Unbuffered));
        let ch2 = ch.clone();
        let handle = std::thread::spawn(move || chan_put(&ch2, Value::Int(42)));
        assert_eq!(chan_take(&ch), Some(Value::Int(42)));
        assert!(handle.join().unwrap());
    }

    #[test]
    fn try_put_on_unbuffered_requires_a_waiting_taker() {
        let ch = Arc::new(Chan::new(BufferPolicy::Unbuffered));
        // No one is parked in chan_take yet: a non-blocking offer must fail.
        assert!(matches!(chan_try_put(&ch, Value::Int(1)), TryPut::WouldBlock));
    }

    #[test]
    fn poll_and_offer_are_never_blocking() {
        let ch = Chan::new(BufferPolicy::Fixed(1));
        assert!(matches!(chan_try_take(&ch), TryTake::WouldBlock));
        assert!(matches!(chan_try_put(&ch, Value::Int(1)), TryPut::Sent));
        assert!(matches!(chan_try_put(&ch, Value::Int(2)), TryPut::WouldBlock));
        assert!(matches!(chan_try_take(&ch), TryTake::Received(Value::Int(1))));
    }
}
