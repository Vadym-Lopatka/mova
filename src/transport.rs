//! V05-PERF-PLAN.md E2: the production 1:1 transport kernel.
//!
//! Two point-to-point transports carrying `Value` between exactly one
//! producer thread and exactly one consumer thread:
//!
//! - [`SpscRing`] -- a bounded single-producer/single-consumer ring
//!   (mirrors a `Chan` with `BufferPolicy::Fixed(n)`).
//! - [`Handoff`] -- a single-slot rendezvous: the producer's `put` only
//!   returns once the consumer has actually drained the value (mirrors a
//!   `Chan` with `BufferPolicy::Unbuffered`).
//!
//! Both spin briefly and then park, both support `try_put`/`try_take`, and
//! both implement `close` with the exact observable contract
//! `builtins::async`'s `Chan` has, so the flow engine can substitute one
//! for the other on a 1:1 conn without any semantic drift:
//!
//! - close may be called from either side, any number of times (idempotent);
//! - a take drains whatever is still buffered and only then reports closed;
//! - a put on an already-closed transport returns `false`, and a put that is
//!   still blocked when the close lands also returns `false`.
//!
//! ## Why this module exists
//!
//! `bench/optimization-log.md`'s E2 probe measured the general
//! `Chan` (`Mutex` + `Condvar`) at 2.3-2.5us per inter-proc hop -- squarely
//! on the thread park/unpark rung -- against 96-131ns/hop for prototype
//! SPSC/handoff transports, which sit on the cross-core cache-line-exchange
//! rung instead. That is the entire reason for this kernel.
//!
//! ## Deliberate non-goal: no `alts!!`/select
//!
//! Neither transport participates in `alts!!`, `offer!`-across-channels, or
//! any other multi-channel select. Select needs a wait-group that spans
//! channels the caller does not own; supporting it would drag in exactly
//! the shared-condvar machinery this kernel exists to avoid. The general
//! `Chan` keeps `alts!!` (and mult/pub-sub/user-visible channels); the flow
//! integration will only route a conn through this kernel where no alt is
//! involved. That is a scope decision, not an oversight.
//!
//! ## Safety precondition, made unrepresentable
//!
//! The lock-free ring is only data-race-free with exactly one producer and
//! exactly one consumer. Rather than document that and hope, the
//! constructors hand back a [`SpscTx`]/[`SpscRx`] (or
//! [`HandoffTx`]/[`HandoffRx`]) pair: neither is `Clone`, and both are
//! `!Sync`, so there is no safe way to get two threads pushing (or two
//! popping) at once. `close`/`is_closed` are on both halves, since close
//! genuinely comes from either side.
//!
//! ## THE WAKE PROTOCOL
//!
//! This is the part that a previous attempt got wrong (see
//! `bench/optimization-log.md`'s E2 entry: an `AtomicBool` "somebody is
//! parked" hint deadlocked under ping-pong stress after passing a one-way
//! bench), and it is the part that decides both correctness and the
//! throughput number. Read this whole section before changing any
//! `Ordering` below.
//!
//! ### The trap the naive design falls into
//!
//! The obvious shape is "waiter publishes `parked = true` then re-checks the
//! queue; producer fills the queue then checks `parked`". Those two
//! sequences are a textbook store-buffer (Dekker) pattern *on two different
//! locations*:
//!
//! ```text
//!   producer:  store tail        waiter:  store parked
//!              load  parked               load  tail
//! ```
//!
//! Acquire/Release does not forbid *both* loads returning the stale value --
//! a load is allowed to be satisfied before an earlier store drains the
//! store buffer, and on AArch64 it demonstrably is. When both miss, the
//! producer skips the wake and the waiter parks on data that is already
//! there: a permanent, 0%-CPU deadlock. Repairing that with a `SeqCst`
//! fence on each side works, but puts a full barrier on the producer's hot
//! path -- the exact place the 3x throughput gap lives.
//!
//! ### What this module does instead: one location, one modification order
//!
//! The fix is to stop using two locations. Every piece of state a waiter and
//! its peer must agree on is packed into the **single atomic word the peer
//! already has to write anyway**, and every update of that word is a
//! read-modify-write. A single atomic location has a total modification
//! order that all threads agree on, so "who went first" is never ambiguous
//! and there is nothing for a store buffer to reorder across.
//!
//! Two words, each `(count << 2) | CLOSED | PARKED`:
//!
//! - `tail` -- message count *written*. Only the producer advances the
//!   count. Its `PARKED` bit means "the consumer is parked waiting for
//!   data" and is set only by the consumer.
//! - `head` -- message count *read*. Only the consumer advances the count.
//!   Its `PARKED` bit means "the producer is parked waiting for room" and
//!   is set only by the producer.
//!
//! `CLOSED` is set on both words by `close`, and is monotone (never
//! cleared).
//!
//! ### The invariants (why no wake can be lost)
//!
//! Stated for the consumer waiting on `tail`; the producer waiting on `head`
//! is the mirror image, word for word.
//!
//! 1. **The waiter's park intent is published by a CAS that re-validates
//!    the wait condition.** Before parking, the consumer loads `tail` into
//!    `t`, confirms `t`'s count still equals its own `head` count (queue
//!    empty) and that `t` is not `CLOSED`, registers its `Thread` handle,
//!    and only then attempts `tail.compare_exchange(t, t | PARKED)`.
//!    Publishing the intent and re-checking the condition are therefore the
//!    *same* atomic step, not two steps with a window between them.
//!
//! 2. **Every producer advance of the count is a CAS that reports what it
//!    displaced.** `publish` writes `(count + 1) << 2`, preserving `CLOSED`
//!    and clearing `PARKED`, and returns the previous word. The wake
//!    decision is a branch on a bit of a value the producer *already holds
//!    in a register* -- there is no second load, and no fence, on the hot
//!    path.
//!
//! 3. **Therefore the two can never both miss.** Both operations are RMWs on
//!    `tail`, so one strictly precedes the other in `tail`'s modification
//!    order:
//!    - consumer's CAS first: the producer's later CAS reads a word with
//!      `PARKED` set, so it unparks. The consumer may not have called
//!      `park()` yet, but `Thread::unpark` latches a token, so the
//!      subsequent `park()` returns immediately.
//!    - producer's CAS first: the consumer's CAS has a stale expected value
//!      (the count changed) and *fails*. The consumer never parks; it loops
//!      and finds the data.
//!
//! 4. **`CLOSED` cannot be missed either, by the same argument.** `close`
//!    uses a CAS loop that sets `CLOSED` and clears `PARKED` in one step and
//!    reports what it displaced. Either it observes `PARKED` and unparks, or
//!    the waiter's own CAS fails against the now-`CLOSED` word and the
//!    waiter re-loops into the closed branch. A waiter can therefore never
//!    park *after* `CLOSED` becomes visible on its word.
//!
//! 5. **The registered `Thread` handle is safely published.** The consumer
//!    registers (a `Mutex<Option<Thread>>` store) *before* its `AcqRel` CAS;
//!    the producer reads it *after* its own `AcqRel` CAS observed `PARKED`.
//!    Release/Acquire on `tail` orders the two. Because the transport is
//!    strictly 1:1, the handle a producer finds is always the one and only
//!    consumer thread -- there is no "stale registration belonging to a
//!    different waiter" hazard, which is the specific hole the reverted
//!    `AtomicBool` version fell through.
//!
//! 6. **A stale `PARKED` bit is safe, not merely tolerated.** If the
//!    consumer sets `PARKED`, then wakes for any reason and leaves the wait
//!    loop with data in hand, `PARKED` may still be set. The next `publish`
//!    clears it and issues one extra `unpark` on a running thread. That
//!    latches a token, which makes at most one later `park()` return early;
//!    the wait loop re-checks the real condition every iteration, so an
//!    early return costs one spurious lap and nothing else. The bit is thus
//!    self-healing and never needs a clearing RMW on the wait-exit path.
//!
//! 7. **`park()` is never trusted.** Every wait is a loop whose exit
//!    condition is the actual queue/close state, re-read each lap. Spurious
//!    wakeups, latched tokens from a previous cycle, and OS-level early
//!    returns are all absorbed.
//!
//! 8. **Once a waiter has parked it stays in park mode.** After the first
//!    `park()` in a given wait, the spin budget is not refilled, so a
//!    long-idle waiter that is woken spuriously re-parks instead of burning
//!    another spin quota.
//!
//! 9. **`PARKED` is only ever *set* by the waiter and only ever *cleared* by
//!    its peer (`publish` or `close`).** Since both clearers are CAS loops on
//!    the same word, exactly one of them can observe a given `PARKED`
//!    transition, so a registration produces exactly one `unpark`.
//!
//! 10. **The count never wraps in the word.** `head`/`tail` counts are
//!     monotonic `usize`s shifted left by 2, only their `& mask` projection
//!     indexes the buffer. At 64 bits that is 2^62 messages, so there is no
//!     ABA hazard across ring wraparound.
//!
//! The net effect: the no-waiter fast path is *free* -- it is a bit test on
//! the CAS's return value, on a cache line the writer already owns
//! exclusively. There is no separate flag word, no fence, and no mutex on
//! the hot path. The `Mutex<Option<Thread>>` that guards the parked-thread
//! handle is touched only when `PARKED` was actually observed.
//!
//! ## Cached peer indices: the other half of the throughput story
//!
//! The wake protocol removes the mutex, but a naive push still *reads*
//! `head` (and a naive pop still reads `tail`) on every single operation --
//! a load of the line the peer is actively writing, so every message pays a
//! cross-core transfer in both directions.
//!
//! Each handle therefore caches the last count it saw from its peer
//! (`SpscTx::seen_head`, `SpscRx::seen_tail`). Both are **monotone lower
//! bounds** on the peer's real count -- the peer only ever advances -- so
//! acting on a stale cache is conservative in the safe direction:
//!
//! - `tail - seen_head < cap` proves there is room (the real head is at
//!   least `seen_head`), so the push skips the load entirely. Only when the
//!   cache says "full" does it re-read `head` and refresh.
//! - `seen_tail > head` proves message `head` was published, so the pop
//!   skips the load. Only when the cache says "empty" does it re-read
//!   `tail`.
//!
//! With a deep ring this collapses to roughly one cross-core load per
//! *`cap`* messages instead of one per message, and it is what closed the
//! remaining throughput gap to buffered `Chan` (68.7-73.2 -> 39.3-40.1
//! ns/msg; see `bench/optimization-log.md`). The caches are plain `Cell`s,
//! which is sound for exactly the reason the handles exist: each is reachable
//! from one thread only. They are also what makes the handles `!Sync`, so
//! the optimization and the SPSC guarantee are the same fact.
//!
//! ## BOUNDED waits: what the flow integration actually needs
//!
//! `builtins::flow`'s proc loops never block indefinitely on a data
//! channel: they park on the data side for at most a bound (`PARK_TIMEOUT`,
//! now a multi-second SAFETY NET rather than a live polling interval -- see
//! that module's "control-priority wait design" and `value.rs`'s
//! `Doorbell` doc for why a short poll interval is no longer needed here).
//! Substituting a transport for the general `Chan` on a conn therefore
//! needs exactly two extra primitives, and NOT a cross-channel select:
//!
//! - [`SpscRx::take_timeout`] -- `take`, capped: `TryTake::WouldBlock` once
//!   the bound elapses.
//! - [`SpscTx::wait_writable`] -- "wait for room, capped", used between two
//!   `try_put`s so the caller keeps ownership of the value it is trying to
//!   send (mirroring `chan_try_put` + a capped `cv_wait_timeout` on the
//!   target chan, which is what the engine does today).
//!
//! Both are the ordinary wait loops with `thread::park_timeout` in place of
//! `thread::park` and a deadline check taken only at the *park* boundary
//! (never inside the spin loop -- an `Instant::now()` per `spin_loop` hint
//! would cost more than the spin it is metering). Timing out early is
//! always safe: invariants 6 and 7 already say a waiter that leaves its
//! loop with `PARKED` still set is self-healing, and every wait re-reads
//! the real condition, so a bounded wait can only ever cost one extra lap.
//!
//! Each also has a `_cold` sibling that parks immediately (spin budget 0).
//! A caller that has already timed out once knows the peer is idle; re-
//! spending a 4000-hint spin budget every millisecond for a quiescent
//! pipeline would burn a few percent of a core per idle proc for nothing.
//! The distinction is a caller-side fact, so it is a caller-side choice.
//!
//! **`_or_doorbell` siblings (the DATA-read direction only).**
//! [`SpscRx::take_timeout_or_doorbell`]/`_cold` are ADDITIVE siblings of
//! `take_timeout`/`_cold` -- neither of the two methods just described, nor
//! [`Ring::wait_for_data`]/[`Ring::wait_for_data_until`]/[`Ring::pop`]/
//! [`Ring::pop_timeout`], change AT ALL. The sibling pair exists because a
//! `PARK_TIMEOUT`-scale bound (seconds, not milliseconds) is only safe to
//! sit inside if SOMETHING can interrupt it promptly when
//! `builtins::flow`'s control/inject events happen -- those never touch
//! this ring at all (they arrive on a `Chan`, `builtins::flow`'s
//! control/inject side channel). `value.rs`'s `Doorbell::ring` bridges
//! that gap by also calling `Thread::unpark()` on the parked proc's own OS
//! thread, and [`Ring::wait_for_data_until_or_doorbell`] (private,
//! reached through the `SpscRx` methods above) adds exactly one extra
//! check at the SAME cadence as its existing deadline check -- after the
//! spin budget, never inside it -- for whether a passed-in `Doorbell`'s
//! generation moved. Without that check, invariant 7 ("`park()` is never
//! trusted") means the unpark alone would just cost one wasted loop
//! iteration before re-arming `park_timeout` for the remaining deadline;
//! WITH it, the loop actually returns, which is what makes control/inject
//! latency for a transport-backed proc bounded by `Doorbell::ring`'s
//! immediacy rather than by however long is left on the deadline. Every
//! one of this module's own safety invariants (1-10 above) still holds
//! for the new methods -- they add a second, independent way to reach the
//! ALREADY-proven-safe `TimedWait::TimedOut` outcome, nothing more. The
//! room-direction (`wait_for_room`/`wait_for_room_until`/`SpscTx::
//! wait_writable`) has no such sibling: `builtins::flow` never widened
//! that bound (backpressure is real signal, not the idle case the
//! `Doorbell` fix targets), so there was nothing to bridge there.
//!
//! ## THE TASK ARM: a two-source park (L3.6/W1)
//!
//! Until this wave a `Ring` waiter was an OS thread, full stop: `Waiter`
//! held a `std::thread::Thread` and every wait ended in `thread::park`.
//! `builtins::flow` therefore refused a transport lane to any conn touching
//! a TASK proc (L3 §3.2) -- and since "flow procs are tasks by default"
//! (`1c00c5d`), that refusal cost every proc-to-proc hop the lane, measured
//! at 280 ns/message (docs/FLOW-HOP-RECOVERY.md §0).
//!
//! `Waiter` now holds `Thread | TaskWaker`, and the two `_or_doorbell` wait
//! loops (the only two `builtins::flow` reaches) fork at the PARK STEP ONLY
//! into [`Ring::park_task_two_source`]. Everything else -- the word layout,
//! the `publish`/`seal` RMWs, the cached peer indices, every `Ordering`,
//! and invariants 1-10 above -- is unchanged, because the arm swaps only
//! *which* wake primitive a registration ends up calling, and both
//! primitives have the "a wake on a running waiter latches, it is not lost"
//! property invariants 6/7/9 rest on.
//!
//! **Why a task needs a SECOND source, and a thread did not.** A parked flow
//! proc must notice a control/inject event, which arrives on a `Chan` this
//! ring knows nothing about and reaches the proc as a `Doorbell::ring`. For
//! a THREAD, `Doorbell::ring`'s `owner_thread.unpark()` was enough: an
//! `unpark` is a per-OS-thread token, so it broke the ring's own
//! `park_timeout` for free (that is what the `_or_doorbell` early-exit check
//! was added for). A task has no such token -- `TaskWaker::wake` only
//! reaches a task that *registered* the waker somewhere. So the task arm
//! registers in BOTH places before suspending -- this ring's waiter slot and
//! the proc's `Doorbell` -- and retracts both on resume, with the
//! check-register-recheck dance performed independently on each. The full
//! argument, including why the window between the two registrations cannot
//! strand a waiter, is on [`Ring::park_task_two_source`].
//!
//! **Non-goals of the arm, stated so nobody re-derives them.** The
//! UNBOUNDED waits (`wait_for_data`/`wait_for_room`, reached from `push`/
//! `pop`/`Handoff::put`) keep their `debug_assert!(!in_task())`: nothing in
//! `builtins::flow` calls them, and an unbounded thread park from a task
//! would still wedge a shard. The bounded-but-doorbell-less waits
//! (`wait_for_data_until`, `wait_for_room_until`, `pop_timeout`,
//! `wait_writable`) keep theirs too, for the same reason plus one more: a
//! task park has no deadline to fall back on, so a caller that has only a
//! deadline and no doorbell has no second source at all.
//!
//! ## Capacity is EXACT, not rounded
//!
//! `Ring` allocates `capacity.next_power_of_two()` slots (the index masking
//! wants that) but enforces the caller's `capacity` verbatim as the
//! in-flight limit. The two used to be the same number; separating them is
//! what lets the flow engine map a conn's `:buf-or-n 10` onto a transport
//! without silently deepening the buffer to 16 and changing where
//! backpressure begins. The `unsafe` slot argument is unaffected: at most
//! `cap <= alloc` messages are ever live, so `[head, tail)` still maps to
//! distinct slots under `& mask`.

use std::cell::{Cell, UnsafeCell};
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, Thread};
use std::time::{Duration, Instant};

use crate::sync::lock_mutex;
use crate::value::{Doorbell, Value};

// ---------------------------------------------------------------------------
// Word layout
// ---------------------------------------------------------------------------

/// "A waiter is parked (or is committed to parking) on this word."  Set only
/// by the waiting side, cleared only by the peer's `publish`/`close`.
const PARKED: usize = 0b01;
/// "This transport is closed."  Monotone: set by `close`, never cleared.
const CLOSED: usize = 0b10;
/// The message count lives above the two flag bits.
const COUNT_SHIFT: u32 = 2;

#[inline(always)]
fn count_of(word: usize) -> usize {
    word >> COUNT_SHIFT
}

#[inline(always)]
fn word_of(count: usize) -> usize {
    count << COUNT_SHIFT
}

/// Default spin budget before a waiter parks, in `spin_loop` hints. Sized to
/// cover a cross-core cache-line exchange (tens to low hundreds of ns) many
/// times over without ever reaching the park/unpark syscall in steady state;
/// 4000 is the value the E2 probe used and the value a spin sweep confirmed
/// (see `bench/optimization-log.md`).
const DEFAULT_SPIN_LIMIT: u32 = 4000;

/// Spin budget, overridable via `MOVA_TRANSPORT_SPIN` so the spin sweep in
/// `builtins::bench_transport` can be run without a rebuild. Read once.
/// This is on the wait path only -- never on a successful put/take.
fn spin_limit() -> u32 {
    static LIMIT: OnceLock<u32> = OnceLock::new();
    *LIMIT.get_or_init(|| {
        std::env::var("MOVA_TRANSPORT_SPIN").ok().and_then(|s| s.parse().ok()).unwrap_or(DEFAULT_SPIN_LIMIT)
    })
}

// ---------------------------------------------------------------------------
// CachePadded
// ---------------------------------------------------------------------------

/// Pins the wrapped value to its own 64-byte cache line (`repr(align)` also
/// rounds the *size* up, so the next field starts on the next line).
///
/// `head` and `tail` are each written by one side and read by the other on
/// every single operation. Sharing a line between them makes every write
/// invalidate the peer's copy of the field it did *not* touch -- classic
/// false sharing. The E2 probe measured padding these two as roughly
/// *doubling* SPSC one-way throughput.
#[repr(align(64))]
struct CachePadded<T>(T);

impl<T> std::ops::Deref for CachePadded<T> {
    type Target = T;
    #[inline(always)]
    fn deref(&self) -> &T {
        &self.0
    }
}

// ---------------------------------------------------------------------------
// Waiter: the parked thread handle. Slow path only.
// ---------------------------------------------------------------------------

/// WHO is parked on a waiter slot: an OS thread, or a task (L3.6/W1, the
/// SPSC task lane -- docs/FLOW-HOP-RECOVERY.md §7).
///
/// The two arms are wake-equivalent for everything the protocol below needs:
/// `Thread::unpark` latches a token that makes the next `park()` return
/// immediately, and `TaskWaker::wake` on an already-running task leaves a
/// `NOTIFIED` note the scheduler consumes at its next park boundary (see
/// `runtime`'s module doc, step 3). So invariants 6 ("a stale `PARKED` bit is
/// safe"), 7 ("`park()` is never trusted") and 9 ("exactly one `unpark` per
/// registration") hold word for word on both arms; nothing in the wake
/// protocol distinguishes them.
enum Parked {
    Thread(Thread),
    Task(crate::runtime::TaskWaker),
}

/// Holds the wake handle of a parked waiter.
///
/// This is a plain `Mutex` on purpose. Invariant 5 in the module doc is what
/// makes it correct; the reason it is not a throughput problem is invariant
/// 2 -- it is only locked when the peer's CAS actually displaced a `PARKED`
/// bit, i.e. once per real park, not once per message. The reverted probe
/// version locked it on *every* push and pop, and that is precisely where
/// its 3x throughput deficit against buffered `Chan` came from.
struct Waiter(Mutex<Option<Parked>>);

impl Waiter {
    fn new() -> Self {
        Waiter(Mutex::new(None))
    }

    /// Publishes the current thread as the one to unpark. Must be called
    /// *before* the CAS that sets `PARKED` (module doc, invariant 5).
    fn register(&self) {
        *lock_mutex(&self.0) = Some(Parked::Thread(thread::current()));
    }

    /// [`Waiter::register`]'s TASK arm: publishes a [`crate::runtime::
    /// TaskWaker`] instead of a `Thread`. Same placement rule -- before the
    /// CAS that sets `PARKED`, for invariant 5's reason exactly.
    fn register_task(&self, w: crate::runtime::TaskWaker) {
        *lock_mutex(&self.0) = Some(Parked::Task(w));
    }

    /// Clears the slot. Called by a TASK waiter on every path out of its
    /// park (and only by it -- an OS-thread waiter never retracts, because
    /// a stale `Thread` handle is always still the right thread, while a
    /// stale `TaskWaker` is a handle on a task that may now be parked
    /// somewhere else entirely).
    ///
    /// Retracting does NOT clear `PARKED`, and does not need to: the peer's
    /// next `publish`/`close` clears the bit and finds an empty slot, which
    /// is a wake it did not have to issue because the waiter is already
    /// running. The waiter, for its part, re-loads the word and re-runs the
    /// full register-then-CAS at the top of every lap (invariant 1), so it
    /// can never park behind a slot it emptied.
    fn retract(&self) {
        *lock_mutex(&self.0) = None;
    }

    /// Unparks the registered waiter, if any. Only called after observing a
    /// `PARKED` bit that this call displaced (invariant 9), so it runs at
    /// most once per registration.
    ///
    /// The handle is taken OUT from under the lock and woken after the guard
    /// drops -- the clone-out discipline `value.rs`'s `Doorbell::ring` uses,
    /// here because `TaskWaker::wake` reaches into the target shard's inbox
    /// mutex and there is no reason to hold this one across it. (For the
    /// thread arm this is a pure no-op: `unpark` latches a token whether it
    /// is called inside or outside the lock.)
    fn wake(&self) {
        // `let`, not `match` on the call directly: a temporary in a `match`
        // scrutinee lives for the whole `match`, which would hold this mutex
        // across the wake.
        let parked = lock_mutex(&self.0).take();
        match parked {
            Some(Parked::Thread(t)) => t.unpark(),
            Some(Parked::Task(w)) => w.wake(),
            None => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Non-blocking outcomes. These mirror `builtins::async`'s `TryTake`/`TryPut`
// exactly (that module's are `pub(crate)` inside a private module and so
// cannot be named here); keeping the shapes identical is what lets the flow
// integration swap a `Chan` for a transport without touching call sites.
// ---------------------------------------------------------------------------

/// Outcome of [`SpscRx::try_take`] / [`HandoffRx::try_take`].
pub enum TryTake {
    /// A value was buffered (or, for `Handoff`, sitting in the slot).
    Received(Value),
    /// Closed, with nothing left to drain.
    Closed,
    /// Open but empty right now.
    WouldBlock,
}

/// Outcome of [`SpscTx::try_put`] / [`HandoffTx::try_put`].
pub enum TryPut {
    Sent,
    Closed,
    WouldBlock,
}

// ---------------------------------------------------------------------------
// Ring: the shared core both transports are built from.
// ---------------------------------------------------------------------------

/// The lock-free core. `SpscRing` is this with a caller-chosen capacity;
/// `Handoff` is this with capacity 1 plus a "wait until drained" step at the
/// end of `put`. Sharing one core means there is exactly one wake protocol
/// to reason about (and to model-check), not two.
///
/// # Safety invariant for the `UnsafeCell` slot accesses
///
/// Exactly one thread ever pushes and exactly one ever pops (enforced by the
/// `!Sync`, non-`Clone` `Tx`/`Rx` handles). A push writes slot
/// `tail & mask` only when `tail - head < cap`, i.e. only a slot whose
/// previous occupant the consumer has already read; the producer learned
/// that from an `Acquire` load of `head` that synchronizes-with the
/// consumer's `AcqRel` publish of `head`, so the read happens-before the
/// overwrite. A pop reads slot `head & mask` only when `head != tail`, i.e.
/// only a slot the producer has already written; the consumer learned that
/// from an `Acquire` load of `tail` that synchronizes-with the producer's
/// `AcqRel` publish of `tail`, so the write happens-before the read. That is
/// the standard SPSC argument and it is the *whole* justification for the
/// `unsafe` blocks below -- the `PARKED`/`CLOSED` bits ride along in the same
/// words but play no part in it.
///
/// The handles' cached peer indices (`seen_head`/`seen_tail`, threaded in as
/// `&Cell<usize>`) do not weaken this. Both are monotone lower bounds on the
/// peer's real count, so a slot that the cache says is safe to touch is a
/// slot the *real* index also says is safe to touch; the cache can only ever
/// make an operation take the slow path unnecessarily, never let it touch a
/// slot early. The happens-before edge is still established by the `Acquire`
/// load of the real word, which every cache refresh performs.
struct Ring {
    buf: Box<[UnsafeCell<MaybeUninit<Value>>]>,
    /// `buf.len() - 1`, where `buf.len()` is `cap` rounded up to a power of
    /// two. Indexes the slot array only.
    mask: usize,
    /// The caller's EXACT capacity -- the in-flight limit every room check
    /// uses. Deliberately NOT `buf.len()`: see the module doc's "Capacity
    /// is EXACT" section (a conn's `:buf-or-n 10` must stay 10, not become
    /// 16, or backpressure starts in a different place than the `Chan` it
    /// replaces).
    cap: usize,
    /// `(written_count << 2) | CLOSED | PARKED`. `PARKED` here means "the
    /// consumer is waiting for data".
    tail: CachePadded<AtomicUsize>,
    /// `(read_count << 2) | CLOSED | PARKED`. `PARKED` here means "the
    /// producer is waiting for room".
    head: CachePadded<AtomicUsize>,
    /// Woken when data is published or the transport closes.
    consumer: Waiter,
    /// Woken when room appears or the transport closes.
    producer: Waiter,
    /// Spin budget for this transport's waiters. Per-transport rather than
    /// global so a latency-critical conn can spin longer than a bulk one --
    /// and so the wake-protocol regression tests can pin it to 0, forcing the
    /// park path on *every* operation instead of hoping a scheduler hiccup
    /// pushes a waiter past the budget.
    spin: u32,
}

// SAFETY: `Value` is `Send`, and the `UnsafeCell<MaybeUninit<Value>>` slots
// are only reachable through `push`/`pop`/`try_push`/`try_pop`/`Drop`, which
// the struct-level doc proves are data-race-free for one producer plus one
// consumer. `Send` additionally requires that moving the whole `Ring` to
// another thread is fine, which it is: it is only ever moved before either
// handle is used (inside `channel`).
unsafe impl Send for Ring {}

// SAFETY: as above -- shared access is confined to the disjoint
// producer-only and consumer-only method sets, and every cross-thread
// visibility edge is established by the `Acquire`/`Release`/`AcqRel`
// operations on `head`/`tail` documented in the struct doc.
unsafe impl Sync for Ring {}

/// What a wait loop concluded.
#[derive(PartialEq, Eq, Clone, Copy)]
enum Wait {
    /// The waited-for condition holds (data available, or room available).
    Ready,
    /// The transport closed while waiting; the condition may never hold.
    Closed,
}

/// What a BOUNDED wait loop concluded -- [`Wait`] plus the one outcome only
/// a bounded wait has. See the module doc's "BOUNDED waits" section.
#[derive(PartialEq, Eq, Clone, Copy)]
enum TimedWait {
    Ready,
    Closed,
    /// The caller's deadline passed with the condition still unmet. Nothing
    /// was consumed and nothing was published; the caller may simply retry.
    TimedOut,
}

impl Ring {
    fn new(capacity: usize, spin: u32) -> Self {
        assert!(capacity > 0, "transport capacity must be at least 1");
        let alloc = capacity.next_power_of_two();
        let mut v = Vec::with_capacity(alloc);
        v.resize_with(alloc, || UnsafeCell::new(MaybeUninit::uninit()));
        Ring {
            buf: v.into_boxed_slice(),
            mask: alloc - 1,
            cap: capacity,
            tail: CachePadded(AtomicUsize::new(0)),
            head: CachePadded(AtomicUsize::new(0)),
            consumer: Waiter::new(),
            producer: Waiter::new(),
            spin,
        }
    }

    // -- the two RMW primitives the whole protocol rests on --------------

    /// Advances `w`'s count to `new_count`, preserving `CLOSED` and clearing
    /// `PARKED`, and returns the word it displaced. Module doc, invariant 2:
    /// the caller's wake decision is a bit test on this return value, so the
    /// no-waiter fast path costs nothing beyond the RMW the count update
    /// needed anyway.
    ///
    /// The loop only re-runs if the peer set `PARKED` (or someone set
    /// `CLOSED`) between the load and the CAS -- a rare, bounded race, not a
    /// contended spin: nobody else ever writes this word's count.
    #[inline]
    fn publish(w: &AtomicUsize, new_count: usize) -> usize {
        let mut cur = w.load(Ordering::Relaxed);
        loop {
            let new = word_of(new_count) | (cur & CLOSED);
            match w.compare_exchange_weak(cur, new, Ordering::AcqRel, Ordering::Relaxed) {
                Ok(prev) => return prev,
                Err(actual) => cur = actual,
            }
        }
    }

    /// **THE TWO-SOURCE TASK PARK** (L3.6/W1; docs/FLOW-HOP-RECOVERY.md §7,
    /// and the module doc's "The task arm" section).
    ///
    /// Suspends the CURRENT TASK until either of its two independent event
    /// sources fires, having registered with BOTH first:
    ///
    /// - **source A, this ring's word `w`** (data published / room freed /
    ///   `close`): `waiter` slot + the `PARKED` bit, exactly the protocol
    ///   invariants 1-3 describe, with a `TaskWaker` where a `Thread` used
    ///   to sit;
    /// - **source B, `doorbell`** (`builtins::flow`'s control/inject events,
    ///   which never touch this ring at all): the generation-counter
    ///   register-then-recheck `value.rs`'s `Doorbell::register_task_waker`
    ///   performs under the doorbell's own mutex.
    ///
    /// Returns `true` if it actually suspended. Either way the caller
    /// re-loads `w` at the top of its loop and re-decides from scratch, so
    /// `false` is never a special case -- it just means "something had
    /// already happened, do not sleep".
    ///
    /// # Why no wake can be lost (the check-register-recheck on BOTH sources)
    ///
    /// This is the whole correctness argument, and it is two independent
    /// copies of the same argument -- one per source -- plus one paragraph
    /// about their interaction.
    ///
    /// **Source B is airtight** because the comparison `generation == seen`
    /// and the push of our waker happen under the SAME mutex `Doorbell::
    /// ring` takes to bump the generation and drain the list. One mutex is
    /// one total order, so either the ring went first (we observe the moved
    /// generation, register nothing, and return `false` without parking) or
    /// we went first (the ring drains our waker and wakes it). There is no
    /// third interleaving.
    ///
    /// **Source A is airtight** by the module doc's invariants 1-3, verbatim
    /// and unweakened: the registration precedes an `AcqRel`
    /// `compare_exchange` on `w` that *re-validates the exact word we
    /// tested*, and every peer advance of `w` is itself an `AcqRel` RMW that
    /// reports what it displaced. Both are RMWs on ONE location, so one
    /// strictly precedes the other in that location's modification order:
    /// peer first => our CAS fails on the changed count and we do not park;
    /// us first => the peer's CAS observes `PARKED` and wakes what invariant
    /// 5's release/acquire edge published. No fence is added or needed; the
    /// `Ordering`s here are the ones the thread arm has always used.
    ///
    /// **Their interaction cannot strand us either.** The dangerous shape
    /// would be "source B fires in the window between our two registrations,
    /// is consumed, and we then park on source A alone forever". It cannot
    /// happen: in that window the task is still `RUNNING`, so
    /// `TaskWaker::wake` takes its `RUNNING -> NOTIFIED` arm, and the
    /// scheduler consumes that note *at the park boundary* by re-queueing
    /// instead of parking (`runtime`'s module doc, step 3, and its
    /// `resume_task` `Yield` arm). The suspend below therefore returns
    /// immediately, we retract both registrations, and the caller re-checks.
    /// Symmetrically, source A firing in that window makes our CAS fail, so
    /// we never reach the suspend at all.
    ///
    /// **Both registrations are retracted on every path out**, including the
    /// `!published` one. A `TaskWaker` left behind in either place is a
    /// handle on a task that has moved on to park somewhere else entirely,
    /// and the next event there would fire a wake at it -- harmless (every
    /// park in this runtime re-checks its real condition) but wasteful, and
    /// exactly the hazard `Doorbell::wait_for_change_task`'s own `retain`
    /// exists to prevent.
    ///
    /// **There is no deadline on this arm, by design** -- the L1 landing
    /// stance for task parks (docs/L1-LANDING-SPEC.md): a task park gets no
    /// safety net to hide a missing wake behind, and the two sources above
    /// are jointly complete for a flow proc (data/close on the ring,
    /// everything else on the doorbell). That is also why an idle lane-parked
    /// task costs exactly zero CPU: it is suspended, not polling.
    fn park_task_two_source(
        w: &AtomicUsize,
        expected: usize,
        waiter: &Waiter,
        doorbell: &Doorbell,
        seen: u64,
    ) -> bool {
        let me = crate::runtime::current_waker();
        // -- source B first. Cheapest refutation, and it needs no cleanup
        // when it refuses.
        let Ok(token) = doorbell.register_task_waker(seen, me.clone()) else {
            return false;
        };
        // -- source A: register, then the CAS that re-validates `expected`.
        // `expected | PARKED == expected` when a previous lap already set the
        // bit and nothing has moved since; the CAS then trivially succeeds
        // and we simply park again (invariants 6 and 7).
        waiter.register_task(me);
        let published = match w.compare_exchange(expected, expected | PARKED, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => true,
            Err(actual) => actual == expected | PARKED,
        };
        if !published {
            waiter.retract();
            doorbell.unregister_task_waker(token);
            return false;
        }
        crate::runtime::park_current_yield();
        waiter.retract();
        doorbell.unregister_task_waker(token);
        true
    }

    /// Sets `CLOSED` and clears `PARKED` on `w` in one atomic step, returning
    /// the displaced word. Module doc, invariant 4.
    fn seal(w: &AtomicUsize) -> usize {
        let mut cur = w.load(Ordering::Relaxed);
        loop {
            let new = (cur | CLOSED) & !PARKED;
            match w.compare_exchange_weak(cur, new, Ordering::AcqRel, Ordering::Relaxed) {
                Ok(prev) => return prev,
                Err(actual) => cur = actual,
            }
        }
    }

    // -- waiting ---------------------------------------------------------

    /// Spin-then-park until `tail`'s count differs from `head_count` (data
    /// available) or `tail` is `CLOSED`.
    ///
    /// The park step is invariant 1: register, then a single CAS that both
    /// publishes `PARKED` and re-validates that `tail` is exactly what we
    /// tested. If the CAS fails, something changed and we simply loop.
    fn wait_for_data(&self, head_count: usize, seen_tail: &Cell<usize>) -> Wait {
        // R4 (docs/L1-LANDING-SPEC.md §W3): the transport tier is
        // OS-thread-only. `thread::park` is a THREAD primitive -- from
        // inside a task it would park the whole shard, and no `TaskWaker`
        // reaches a `Ring`. Lifting that is L3, not L1.
        debug_assert!(!crate::runtime::in_task(), "transport park from a task (R4)");
        let budget = self.spin;
        let mut spins = 0u32;
        loop {
            let t = self.tail.load(Ordering::Acquire);
            let tc = count_of(t);
            seen_tail.set(tc);
            if tc != head_count {
                return Wait::Ready;
            }
            if t & CLOSED != 0 {
                return Wait::Closed;
            }
            if spins < budget {
                spins += 1;
                std::hint::spin_loop();
                continue;
            }
            self.consumer.register();
            // `t | PARKED == t` when we already registered on a previous lap
            // and nothing has moved since; the CAS then trivially succeeds
            // and we simply park again (invariants 6 and 7).
            let published = match self.tail.compare_exchange(t, t | PARKED, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => true,
                Err(actual) => actual == t | PARKED,
            };
            if published {
                thread::park();
            }
            // Invariant 8: having parked once, never refill the spin budget.
            spins = budget;
        }
    }

    /// Spin-then-park until there is room for message number `tail_count`
    /// (i.e. `tail_count - head_count < cap`) or `head` is `CLOSED`.
    ///
    /// `Handoff` also reuses this as its "wait until the consumer drained my
    /// value" step: with `cap == 1`, room for message `n + 1` is exactly
    /// "the consumer has read message `n`".
    fn wait_for_room(&self, tail_count: usize, seen_head: &Cell<usize>) -> Wait {
        // R4 (docs/L1-LANDING-SPEC.md §W3): the transport tier is
        // OS-thread-only. `thread::park` is a THREAD primitive -- from
        // inside a task it would park the whole shard, and no `TaskWaker`
        // reaches a `Ring`. Lifting that is L3, not L1.
        debug_assert!(!crate::runtime::in_task(), "transport park from a task (R4)");
        let budget = self.spin;
        let mut spins = 0u32;
        loop {
            let h = self.head.load(Ordering::Acquire);
            let hc = count_of(h);
            seen_head.set(hc);
            if tail_count - hc < self.cap {
                return Wait::Ready;
            }
            if h & CLOSED != 0 {
                return Wait::Closed;
            }
            if spins < budget {
                spins += 1;
                std::hint::spin_loop();
                continue;
            }
            self.producer.register();
            let published = match self.head.compare_exchange(h, h | PARKED, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => true,
                Err(actual) => actual == h | PARKED,
            };
            if published {
                thread::park();
            }
            spins = budget;
        }
    }

    // -- bounded waiting (the flow engine's control-priority requirement) --

    /// [`Ring::wait_for_data`] with an upper bound. Word for word the same
    /// protocol -- the ONLY differences are `park_timeout` instead of
    /// `park`, and a deadline test taken at the park boundary (never inside
    /// the spin loop: an `Instant::now()` per `spin_loop` hint would cost
    /// far more than the hint it meters).
    ///
    /// Returning [`TimedWait::TimedOut`] is always safe -- no message can be
    /// lost by it, because nothing has been consumed; the caller simply
    /// loops. A `PARKED` bit left set on the way out is invariant 6's
    /// self-healing case.
    fn wait_for_data_until(&self, head_count: usize, seen_tail: &Cell<usize>, deadline: Instant, spin: u32) -> TimedWait {
        // R4 (docs/L1-LANDING-SPEC.md §W3): the transport tier is
        // OS-thread-only. `thread::park` is a THREAD primitive -- from
        // inside a task it would park the whole shard, and no `TaskWaker`
        // reaches a `Ring`. Lifting that is L3, not L1.
        debug_assert!(!crate::runtime::in_task(), "transport park from a task (R4)");
        let mut spins = 0u32;
        loop {
            let t = self.tail.load(Ordering::Acquire);
            let tc = count_of(t);
            seen_tail.set(tc);
            if tc != head_count {
                return TimedWait::Ready;
            }
            if t & CLOSED != 0 {
                return TimedWait::Closed;
            }
            if spins < spin {
                spins += 1;
                std::hint::spin_loop();
                continue;
            }
            let now = Instant::now();
            if now >= deadline {
                return TimedWait::TimedOut;
            }
            self.consumer.register();
            let published = match self.tail.compare_exchange(t, t | PARKED, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => true,
                Err(actual) => actual == t | PARKED,
            };
            if published {
                thread::park_timeout(deadline - now);
            }
            spins = spin;
        }
    }

    /// [`Ring::wait_for_room`] with an upper bound; the mirror image of
    /// [`Ring::wait_for_data_until`], same reasoning throughout.
    fn wait_for_room_until(&self, tail_count: usize, seen_head: &Cell<usize>, deadline: Instant, spin: u32) -> TimedWait {
        // R4 (docs/L1-LANDING-SPEC.md §W3): the transport tier is
        // OS-thread-only. `thread::park` is a THREAD primitive -- from
        // inside a task it would park the whole shard, and no `TaskWaker`
        // reaches a `Ring`. Lifting that is L3, not L1.
        debug_assert!(!crate::runtime::in_task(), "transport park from a task (R4)");
        let mut spins = 0u32;
        loop {
            let h = self.head.load(Ordering::Acquire);
            let hc = count_of(h);
            seen_head.set(hc);
            if tail_count - hc < self.cap {
                return TimedWait::Ready;
            }
            if h & CLOSED != 0 {
                return TimedWait::Closed;
            }
            if spins < spin {
                spins += 1;
                std::hint::spin_loop();
                continue;
            }
            let now = Instant::now();
            if now >= deadline {
                return TimedWait::TimedOut;
            }
            self.producer.register();
            let published = match self.head.compare_exchange(h, h | PARKED, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => true,
                Err(actual) => actual == h | PARKED,
            };
            if published {
                thread::park_timeout(deadline - now);
            }
            spins = spin;
        }
    }

    /// The bounded counterpart of [`Ring::pop`]: hands over a buffered value
    /// if there is one, reports `Closed` once closed AND drained, and
    /// otherwise gives up after `timeout` with `WouldBlock`.
    fn pop_timeout(&self, seen_tail: &Cell<usize>, timeout: Duration, spin: u32) -> TryTake {
        let hc = count_of(self.head.load(Ordering::Relaxed));
        if seen_tail.get() == hc {
            let tc = count_of(self.tail.load(Ordering::Acquire));
            seen_tail.set(tc);
            if tc == hc {
                match self.wait_for_data_until(hc, seen_tail, Instant::now() + timeout, spin) {
                    TimedWait::Ready => {}
                    TimedWait::Closed => return TryTake::Closed,
                    TimedWait::TimedOut => return TryTake::WouldBlock,
                }
            }
        }
        TryTake::Received(self.consume(hc))
    }

    // -- `Doorbell`-aware bounded waiting (`builtins::flow`'s transport
    // tier, V05/T3): NEW, ADDITIVE siblings of the two methods just above,
    // not a modification of them. Every existing caller of
    // `wait_for_data_until`/`pop_timeout` (bench_transport's spin sweep,
    // any test constructing a `Ring` directly) is untouched byte-for-byte.
    // See `value.rs`'s `Doorbell` doc and `builtins::flow`'s module doc,
    // "Composition with control/pause/stop", for why this exists: a proc
    // parked here on data may need to notice a control/inject event that
    // only rings a `Doorbell`, and `Doorbell::ring`'s `Thread::unpark()`
    // (value.rs) is what actually interrupts the `thread::park_timeout`
    // below -- but per invariant 7 ("park() is never trusted"), an unpark
    // ALONE only causes one extra loop iteration; without the extra
    // `doorbell.current() != seen` check added here, that iteration would
    // just re-arm `park_timeout` for the REMAINING deadline and go back to
    // sleep, since neither `tc != head_count` nor `CLOSED` changed. This
    // check is what turns "woken" into "actually returns to the caller". --

    /// [`Ring::wait_for_data_until`], plus ONE extra early-exit condition
    /// checked at the exact same cadence as the existing deadline check
    /// (after the spin budget is exhausted, never inside the spin loop --
    /// same performance reasoning as that check): if `doorbell`'s
    /// generation has moved past `seen`, return `TimedOut` immediately
    /// instead of parking (or re-parking) toward `deadline`. Safe by the
    /// EXACT same argument the doc above already makes for the deadline
    /// check itself -- returning early here can never lose a message
    /// (nothing has been consumed), so the caller simply loops; a
    /// `PARKED` bit left set is invariant 6's self-healing case, same as
    /// always.
    fn wait_for_data_until_or_doorbell(
        &self,
        head_count: usize,
        seen_tail: &Cell<usize>,
        deadline: Instant,
        spin: u32,
        doorbell: &Doorbell,
        seen: u64,
    ) -> TimedWait {
        // L3.6/W1: this is the ONE data-direction wait with a task arm, and
        // the arm forks at the PARK STEP ONLY -- everything above it (the
        // `Acquire` load, the cached-index update, the ready/closed
        // decisions, the spin budget, the doorbell early-exit) is shared,
        // byte for byte, by both. One `in_task()` TLS read, hoisted.
        let task = crate::runtime::in_task();
        let mut spins = 0u32;
        loop {
            let t = self.tail.load(Ordering::Acquire);
            let tc = count_of(t);
            seen_tail.set(tc);
            if tc != head_count {
                return TimedWait::Ready;
            }
            if t & CLOSED != 0 {
                return TimedWait::Closed;
            }
            if spins < spin {
                spins += 1;
                std::hint::spin_loop();
                continue;
            }
            if doorbell.current() != seen {
                return TimedWait::TimedOut;
            }
            if task {
                // The two-source park. No deadline arm: a task park has no
                // timer to fall back to and deliberately no safety net, so
                // this never reports a safety-net hit and an idle
                // lane-parked proc stays suspended at 0% CPU until one of
                // its two sources fires.
                Self::park_task_two_source(&self.tail, t, &self.consumer, doorbell, seen);
                spins = spin;
                continue;
            }
            let now = Instant::now();
            if now >= deadline {
                // A GENUINE safety-net fallback (the doorbell branch just
                // above did NOT fire): nothing rang for this whole
                // `deadline`. Folded into `Doorbell`'s own counter so
                // `tests/flow_wake_test.rs` can measure this path the same
                // way it measures the `Chan`-based one.
                Doorbell::note_external_safety_net_hit();
                return TimedWait::TimedOut;
            }
            self.consumer.register();
            let published = match self.tail.compare_exchange(t, t | PARKED, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => true,
                Err(actual) => actual == t | PARKED,
            };
            if published {
                thread::park_timeout(deadline - now);
            }
            spins = spin;
        }
    }

    /// [`Ring::wait_for_room_until`]'s `Doorbell`-aware sibling -- the
    /// ROOM-direction mirror of [`Ring::wait_for_data_until_or_doorbell`],
    /// with the same one extra early-exit condition and the same task arm.
    ///
    /// **Why this direction needed a sibling once tasks could hold a lane,
    /// when it did not before.** A THREAD's blocked send waits at most
    /// `BLOCKED_SEND_TIMEOUT` (1 ms), so a `pause`/`stop` arriving while the
    /// downstream is full is noticed on the next lap of `builtins::flow`'s
    /// `out_send` regardless -- there was nothing to bridge, which is exactly
    /// what that constant's doc says. A TASK has no such bound: its park is
    /// indefinite, so without a second source a proc blocked on backpressure
    /// would sleep through `stop` until room appeared. `doorbell` is the
    /// proc's own, already rung by every control command (its control chan
    /// holds it in `ChanState::doorbell`), so registering on it is what makes
    /// the task arm's control latency EQUAL to the ring, and strictly better
    /// than the thread arm's 1 ms poll.
    ///
    /// The thread arm below is [`Ring::wait_for_room_until`] verbatim except
    /// for the `doorbell.current() != seen` test; `builtins::flow` routes
    /// threads to the unchanged method anyway, so no existing thread caller
    /// is touched by the existence of this one.
    fn wait_for_room_until_or_doorbell(
        &self,
        tail_count: usize,
        seen_head: &Cell<usize>,
        deadline: Instant,
        spin: u32,
        doorbell: &Doorbell,
        seen: u64,
    ) -> TimedWait {
        let task = crate::runtime::in_task();
        let mut spins = 0u32;
        loop {
            let h = self.head.load(Ordering::Acquire);
            let hc = count_of(h);
            seen_head.set(hc);
            if tail_count - hc < self.cap {
                return TimedWait::Ready;
            }
            if h & CLOSED != 0 {
                return TimedWait::Closed;
            }
            if spins < spin {
                spins += 1;
                std::hint::spin_loop();
                continue;
            }
            if doorbell.current() != seen {
                return TimedWait::TimedOut;
            }
            if task {
                Self::park_task_two_source(&self.head, h, &self.producer, doorbell, seen);
                spins = spin;
                continue;
            }
            let now = Instant::now();
            if now >= deadline {
                return TimedWait::TimedOut;
            }
            self.producer.register();
            let published = match self.head.compare_exchange(h, h | PARKED, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => true,
                Err(actual) => actual == h | PARKED,
            };
            if published {
                thread::park_timeout(deadline - now);
            }
            spins = spin;
        }
    }

    /// "Wait until a `try_pop` could make progress" -- the DATA-direction
    /// mirror of [`Ring::wait_writable`], and the one wait in this module
    /// that deliberately CONSUMES NOTHING. `false` means the bound elapsed
    /// (or the doorbell rang) first.
    ///
    /// `pop_timeout_or_doorbell` cannot serve this caller:
    /// `builtins::flow`'s MULTI-INPUT round-robin has to park on "this lane
    /// OR any of my other ports OR control", and it re-scans every port from
    /// the top of the next lap. A park primitive that returned a value would
    /// hand it a message outside that scan, at a point in the loop with
    /// nowhere to put it. So this is a pure wait, and the caller's next lap
    /// picks the message up through the ordinary `in_try_take`.
    fn wait_readable_or_doorbell(
        &self,
        seen_tail: &Cell<usize>,
        timeout: Duration,
        spin: u32,
        doorbell: &Doorbell,
        seen: u64,
    ) -> bool {
        let hc = count_of(self.head.load(Ordering::Relaxed));
        let deadline = Instant::now() + timeout;
        self.wait_for_data_until_or_doorbell(hc, seen_tail, deadline, spin, doorbell, seen) != TimedWait::TimedOut
    }

    /// [`Ring::wait_writable`] threaded through
    /// [`Ring::wait_for_room_until_or_doorbell`] -- otherwise identical.
    fn wait_writable_or_doorbell(
        &self,
        seen_head: &Cell<usize>,
        timeout: Duration,
        spin: u32,
        doorbell: &Doorbell,
        seen: u64,
    ) -> bool {
        let tc = count_of(self.tail.load(Ordering::Relaxed));
        let deadline = Instant::now() + timeout;
        self.wait_for_room_until_or_doorbell(tc, seen_head, deadline, spin, doorbell, seen) != TimedWait::TimedOut
    }

    /// [`Ring::pop_timeout`], threaded through
    /// [`Ring::wait_for_data_until_or_doorbell`] instead of
    /// [`Ring::wait_for_data_until`] -- otherwise identical.
    fn pop_timeout_or_doorbell(
        &self,
        seen_tail: &Cell<usize>,
        timeout: Duration,
        spin: u32,
        doorbell: &Doorbell,
        seen: u64,
    ) -> TryTake {
        let hc = count_of(self.head.load(Ordering::Relaxed));
        if seen_tail.get() == hc {
            let tc = count_of(self.tail.load(Ordering::Acquire));
            seen_tail.set(tc);
            if tc == hc {
                match self.wait_for_data_until_or_doorbell(hc, seen_tail, Instant::now() + timeout, spin, doorbell, seen) {
                    TimedWait::Ready => {}
                    TimedWait::Closed => return TryTake::Closed,
                    TimedWait::TimedOut => return TryTake::WouldBlock,
                }
            }
        }
        TryTake::Received(self.consume(hc))
    }

    /// Waits, bounded, until a `try_push` could succeed (room, or closed --
    /// a closed transport's `try_push` reports `Closed` rather than
    /// blocking, so both are "stop waiting"). `false` means the bound
    /// elapsed first.
    fn wait_writable(&self, seen_head: &Cell<usize>, timeout: Duration, spin: u32) -> bool {
        let tc = count_of(self.tail.load(Ordering::Relaxed));
        let deadline = Instant::now() + timeout;
        self.wait_for_room_until(tc, seen_head, deadline, spin) != TimedWait::TimedOut
    }

    // -- producer side ---------------------------------------------------

    /// Blocking put. `false` iff the transport was closed before the value
    /// could be handed over -- matching `chan_put`'s contract.
    fn push(&self, v: Value, seen_head: &Cell<usize>) -> bool {
        let t = self.tail.load(Ordering::Relaxed);
        if t & CLOSED != 0 {
            return false;
        }
        let tc = count_of(t);
        // Cached-index fast path (see `Ring`'s doc): `seen_head` is a
        // monotone LOWER BOUND on the consumer's real head count, so if it
        // already shows room then there really is room -- and this push
        // never touches the consumer's cache line at all.
        if tc - seen_head.get() >= self.cap {
            let hc = count_of(self.head.load(Ordering::Acquire));
            seen_head.set(hc);
            if tc - hc >= self.cap && self.wait_for_room(tc, seen_head) == Wait::Closed {
                return false;
            }
        }
        self.commit(tc, v);
        true
    }

    /// Writes slot `tc & mask` and publishes `tc + 1`, waking a parked
    /// consumer iff this publish displaced its `PARKED` bit.
    ///
    /// Callers must have established that there is room for message `tc`.
    #[inline]
    fn commit(&self, tc: usize, v: Value) {
        // SAFETY: room for message `tc` was established above, so slot
        // `tc & mask` holds no live value -- its previous occupant was read
        // by the consumer at a point that happens-before this write (see the
        // struct's safety invariant). We are the only producer, so no other
        // thread writes this slot.
        unsafe { (*self.buf[tc & self.mask].get()).write(v) };
        let prev = Self::publish(&self.tail, tc + 1);
        if prev & PARKED != 0 {
            self.consumer.wake();
        }
    }

    fn try_push(&self, v: Value, seen_head: &Cell<usize>) -> TryPut {
        let t = self.tail.load(Ordering::Relaxed);
        if t & CLOSED != 0 {
            return TryPut::Closed;
        }
        let tc = count_of(t);
        if tc - seen_head.get() >= self.cap {
            let hc = count_of(self.head.load(Ordering::Acquire));
            seen_head.set(hc);
            if tc - hc >= self.cap {
                return TryPut::WouldBlock;
            }
        }
        self.commit(tc, v);
        TryPut::Sent
    }

    // -- consumer side ---------------------------------------------------

    /// Blocking take. `None` iff the transport is closed *and* drained --
    /// matching `chan_take`'s drain-then-nil contract (the data check
    /// strictly precedes the closed check, here and in `try_pop`).
    fn pop(&self, seen_tail: &Cell<usize>) -> Option<Value> {
        let hc = count_of(self.head.load(Ordering::Relaxed));
        // Cached-index fast path: `seen_tail` is a monotone lower bound on
        // the producer's real tail count and is never below `hc`, so
        // `seen_tail > hc` already proves message `hc` was published.
        if seen_tail.get() == hc {
            let tc = count_of(self.tail.load(Ordering::Acquire));
            seen_tail.set(tc);
            if tc == hc && self.wait_for_data(hc, seen_tail) == Wait::Closed {
                return None;
            }
        }
        Some(self.consume(hc))
    }

    /// Reads slot `hc & mask` and publishes `hc + 1`, waking a parked
    /// producer iff this publish displaced its `PARKED` bit.
    ///
    /// Callers must have established that message `hc` has been published.
    #[inline]
    fn consume(&self, hc: usize) -> Value {
        // SAFETY: message `hc` was observed as published via an `Acquire`
        // load of `tail` that synchronizes-with the producer's `AcqRel`
        // publish, so the slot is initialized and the write happens-before
        // this read. We are the only consumer, so nobody else reads it, and
        // publishing `hc + 1` below is what permits the producer to reuse
        // the slot -- which happens strictly after this move-out.
        let v = unsafe { (*self.buf[hc & self.mask].get()).assume_init_read() };
        let prev = Self::publish(&self.head, hc + 1);
        if prev & PARKED != 0 {
            self.producer.wake();
        }
        v
    }

    fn try_pop(&self, seen_tail: &Cell<usize>) -> TryTake {
        let hc = count_of(self.head.load(Ordering::Relaxed));
        if seen_tail.get() != hc {
            return TryTake::Received(self.consume(hc));
        }
        let t = self.tail.load(Ordering::Acquire);
        seen_tail.set(count_of(t));
        if count_of(t) != hc {
            TryTake::Received(self.consume(hc))
        } else if t & CLOSED != 0 {
            TryTake::Closed
        } else {
            TryTake::WouldBlock
        }
    }

    // -- close -----------------------------------------------------------

    /// Idempotent, callable from either side (or a third thread). Seals both
    /// words and wakes whoever each seal displaced (invariant 4).
    fn close(&self) {
        if Self::seal(&self.tail) & PARKED != 0 {
            self.consumer.wake();
        }
        if Self::seal(&self.head) & PARKED != 0 {
            self.producer.wake();
        }
    }

    fn is_closed(&self) -> bool {
        self.tail.load(Ordering::Acquire) & CLOSED != 0
    }

    /// Number of values currently buffered. Advisory only (it is a snapshot
    /// of two independently-updated words), which is all any `Chan`-level
    /// `count` could be either.
    fn len(&self) -> usize {
        let t = count_of(self.tail.load(Ordering::Acquire));
        let h = count_of(self.head.load(Ordering::Acquire));
        t.saturating_sub(h)
    }
}

impl Drop for Ring {
    fn drop(&mut self) {
        // `&mut self` means both handles are gone, so no concurrency: every
        // slot in `[head, tail)` still holds a live `Value` that nobody
        // consumed, and leaking them would leak whatever they own (an `Arc`
        // cycle root, a channel, a file handle...).
        let head = count_of(*self.head.0.get_mut());
        let tail = count_of(*self.tail.0.get_mut());
        for i in head..tail {
            // SAFETY: `i` is in `[head, tail)`, so message `i` was published
            // and not yet consumed -- slot `i & mask` is initialized. We hold
            // `&mut self`, so there is no other reference to it, and each
            // index in the range maps to a distinct slot because
            // `tail - head <= cap`.
            unsafe { (*self.buf[i & self.mask].get()).assume_init_drop() };
        }
    }
}

// ---------------------------------------------------------------------------
// Handles. `!Sync` + non-`Clone` is what makes "single producer, single
// consumer" a compile-time fact rather than a comment.
// ---------------------------------------------------------------------------


macro_rules! transport_handles {
    (
        $(#[$core_meta:meta])* $core:ident,
        $(#[$tx_meta:meta])* $tx:ident,
        $(#[$rx_meta:meta])* $rx:ident
    ) => {
        $(#[$core_meta])*
        pub struct $core {
            ring: Ring,
        }

        $(#[$tx_meta])*
        pub struct $tx {
            core: Arc<$core>,
            /// Last observed consumer head count -- a monotone lower bound,
            /// so a push that finds room here needs no cross-core load at
            /// all. Safe as a plain `Cell` precisely because this handle is
            /// `!Sync` and non-`Clone`: exactly one thread can ever reach
            /// it. (It is also what MAKES the handle `!Sync`, so it doubles
            /// as the SPSC marker.)
            seen_head: Cell<usize>,
        }

        $(#[$rx_meta])*
        pub struct $rx {
            core: Arc<$core>,
            /// Last observed producer tail count. Same rationale as
            /// `$tx::seen_head`.
            seen_tail: Cell<usize>,
        }

        impl $tx {
            /// Closes the transport (idempotent, either side).
            pub fn close(&self) {
                self.core.ring.close();
            }

            pub fn is_closed(&self) -> bool {
                self.core.ring.is_closed()
            }

            /// Values currently in flight. Advisory.
            pub fn len(&self) -> usize {
                self.core.ring.len()
            }

            pub fn is_empty(&self) -> bool {
                self.len() == 0
            }
        }

        impl $rx {
            /// Blocking take. `None` once the transport is closed *and*
            /// drained: buffered values are handed over first, exactly as
            /// `chan_take` does.
            pub fn take(&self) -> Option<Value> {
                self.core.ring.pop(&self.seen_tail)
            }

            /// Non-blocking take; never parks. See [`TryTake`].
            pub fn try_take(&self) -> TryTake {
                self.core.ring.try_pop(&self.seen_tail)
            }

            /// `take`, capped at `timeout`: `WouldBlock` once the bound
            /// elapses with nothing to hand over. The exact counterpart of
            /// `builtins::flow`'s `try_take_with_timeout` on a `Chan`, and
            /// the primitive that lets a proc loop keep polling its control
            /// chan while parked on data. See the module doc's "BOUNDED
            /// waits" section.
            pub fn take_timeout(&self, timeout: std::time::Duration) -> TryTake {
                let spin = self.core.ring.spin;
                self.core.ring.pop_timeout(&self.seen_tail, timeout, spin)
            }

            /// [`Self::take_timeout`] for a waiter that has ALREADY timed
            /// out once on this transport and therefore knows the producer
            /// is idle: parks straight away (spin budget 0) instead of
            /// re-spending the full spin budget every millisecond. See the
            /// module doc for why this is the caller's call to make.
            pub fn take_timeout_cold(&self, timeout: std::time::Duration) -> TryTake {
                self.core.ring.pop_timeout(&self.seen_tail, timeout, 0)
            }

            /// Closes the transport (idempotent, either side).
            pub fn close(&self) {
                self.core.ring.close();
            }

            pub fn is_closed(&self) -> bool {
                self.core.ring.is_closed()
            }

            /// Values currently in flight. Advisory.
            pub fn len(&self) -> usize {
                self.core.ring.len()
            }

            pub fn is_empty(&self) -> bool {
                self.len() == 0
            }
        }
    };
}

transport_handles! {
    /// A bounded single-producer/single-consumer ring of `Value`.
    ///
    /// Mirrors a `Chan` with `BufferPolicy::Fixed(n)`: a put blocks while
    /// the ring is full, a take blocks while it is empty, and close makes
    /// takes drain-then-`None` and puts `false`. Build one with
    /// [`SpscRing::channel`].
    SpscRing,
    /// The producer half of a [`SpscRing`]. Not `Clone`, not `Sync`.
    SpscTx,
    /// The consumer half of a [`SpscRing`]. Not `Clone`, not `Sync`.
    SpscRx
}

transport_handles! {
    /// A single-slot rendezvous: the producer writes the slot, wakes exactly
    /// the parked consumer, and then waits for the value to be consumed
    /// before its `put` returns.
    ///
    /// Mirrors a `Chan` with `BufferPolicy::Unbuffered`. Build one with
    /// [`Handoff::channel`].
    Handoff,
    /// The producer half of a [`Handoff`]. Not `Clone`, not `Sync`.
    HandoffTx,
    /// The consumer half of a [`Handoff`]. Not `Clone`, not `Sync`.
    HandoffRx
}

impl SpscRing {
    /// Creates a ring of at least `capacity` values and returns its producer
    /// and consumer halves. `capacity` is rounded up to a power of two (the
    /// index masking wants it); it must be at least 1.
    pub fn channel(capacity: usize) -> (SpscTx, SpscRx) {
        Self::channel_with_spin(capacity, spin_limit())
    }

    /// As [`SpscRing::channel`], with an explicit spin budget in
    /// `spin_loop` hints before a waiter parks.
    ///
    /// `spin = 0` parks immediately on every wait. That is the setting the
    /// wake-protocol regression tests use: it makes the park/unpark path
    /// mandatory rather than merely reachable, which is the difference
    /// between a test that catches a lost wake and one that only catches it
    /// when the scheduler happens to cooperate.
    pub fn channel_with_spin(capacity: usize, spin: u32) -> (SpscTx, SpscRx) {
        let core = Arc::new(SpscRing { ring: Ring::new(capacity, spin) });
        (
            SpscTx { core: Arc::clone(&core), seen_head: Cell::new(0) },
            SpscRx { core, seen_tail: Cell::new(0) },
        )
    }

    /// The EXACT capacity requested -- the number of values that can be in
    /// flight at once. The slot array behind it is rounded up to a power of
    /// two; that rounding is deliberately not observable (module doc,
    /// "Capacity is EXACT").
    pub fn capacity(&self) -> usize {
        self.ring.cap
    }
}

/// A `Send + Sync`, freely-clonable handle whose ONLY power is to close (or
/// ask about) a transport.
///
/// The `Tx`/`Rx` halves are deliberately `!Sync` and non-`Clone` -- that is
/// what makes "single producer, single consumer" a compile-time fact -- but
/// close genuinely comes from a third party: `flow/stop` closes every
/// engine-owned link from the calling thread, after every proc thread has
/// exited, exactly as it closes every engine-owned `Chan`. This handle is
/// that capability and nothing else: it cannot push, pop, or observe a
/// value, so it cannot violate the SPSC precondition the `unsafe` blocks
/// rest on.
#[derive(Clone)]
pub struct SpscCloser(Arc<SpscRing>);

impl SpscCloser {
    /// Idempotent, callable any number of times from any thread.
    pub fn close(&self) {
        self.0.ring.close();
    }

    pub fn is_closed(&self) -> bool {
        self.0.ring.is_closed()
    }
}

impl SpscTx {
    /// Blocking put. Returns `false` iff the ring was already closed, or
    /// closed while this put was blocked on a full ring -- matching
    /// `chan_put` on a `Fixed(n)` channel.
    pub fn put(&self, v: Value) -> bool {
        self.core.ring.push(v, &self.seen_head)
    }

    /// Non-blocking put; never parks. Mirrors `chan_try_put` on a
    /// `Fixed(n)` channel: `WouldBlock` on a full-but-open ring, `Closed`
    /// once closed.
    pub fn try_put(&self, v: Value) -> TryPut {
        self.core.ring.try_push(v, &self.seen_head)
    }

    /// Waits, bounded by `timeout`, until a [`Self::try_put`] could make
    /// progress -- i.e. until there is room, or the transport closes.
    /// `false` means the bound elapsed first.
    ///
    /// This is deliberately a *wait*, not a timed put: the caller keeps
    /// ownership of the value it is trying to send, so it can abandon the
    /// attempt to service a control command and retry the identical send
    /// afterwards. That is exactly the shape
    /// `builtins::flow::send_with_control_priority` has against a `Chan`
    /// (`chan_try_put`, then a capped `cv_wait_timeout` on the target).
    pub fn wait_writable(&self, timeout: std::time::Duration) -> bool {
        let spin = self.core.ring.spin;
        self.core.ring.wait_writable(&self.seen_head, timeout, spin)
    }

    /// [`Self::wait_writable`] for a producer that has already timed out
    /// once against this transport: parks straight away. Same rationale as
    /// [`SpscRx::take_timeout_cold`].
    pub fn wait_writable_cold(&self, timeout: std::time::Duration) -> bool {
        self.core.ring.wait_writable(&self.seen_head, timeout, 0)
    }

    /// [`Self::wait_writable`], but also returns `false` early if
    /// `doorbell`'s generation has moved past `seen` -- the ROOM-direction
    /// twin of [`SpscRx::take_timeout_or_doorbell`]. `seen` must be
    /// snapshotted by the caller BEFORE its non-blocking control scan (the
    /// generation-counter missed-wakeup rule; `value.rs`'s `Doorbell` doc).
    ///
    /// This is the method a TASK proc's blocked send uses, and it is what
    /// makes that send interruptible by `pause`/`stop` at all -- see
    /// [`Ring::wait_for_room_until_or_doorbell`] for why a task needs it
    /// where a thread did not.
    pub fn wait_writable_or_doorbell(&self, timeout: std::time::Duration, doorbell: &Doorbell, seen: u64) -> bool {
        let spin = self.core.ring.spin;
        self.core.ring.wait_writable_or_doorbell(&self.seen_head, timeout, spin, doorbell, seen)
    }

    /// [`Self::wait_writable_or_doorbell`] for a producer that already knows
    /// the downstream is idle: parks straight away (spin budget 0). Same
    /// rationale as [`SpscRx::take_timeout_or_doorbell_cold`].
    pub fn wait_writable_or_doorbell_cold(&self, timeout: std::time::Duration, doorbell: &Doorbell, seen: u64) -> bool {
        self.core.ring.wait_writable_or_doorbell(&self.seen_head, timeout, 0, doorbell, seen)
    }

    /// A close-only handle on this transport, safe to hand to any thread.
    /// See [`SpscCloser`].
    pub fn closer(&self) -> SpscCloser {
        SpscCloser(Arc::clone(&self.core))
    }

    pub fn capacity(&self) -> usize {
        self.core.capacity()
    }
}

impl SpscRx {
    /// A close-only handle on this transport. See [`SpscCloser`].
    pub fn closer(&self) -> SpscCloser {
        SpscCloser(Arc::clone(&self.core))
    }

    pub fn capacity(&self) -> usize {
        self.core.capacity()
    }

    /// [`Self::take_timeout`], but also returns `WouldBlock` early if
    /// `doorbell`'s generation has moved past `seen` (a control/inject
    /// ring, not this ring's own data) -- see
    /// [`Ring::wait_for_data_until_or_doorbell`]'s doc for the exact,
    /// minimal difference from the plain wait this is layered on top of.
    /// `seen` should be `doorbell.current()` snapshotted by the caller
    /// BEFORE this call (the generation-counter "missed-wakeup
    /// correctness" rule -- see `value.rs`'s `Doorbell` doc).
    pub fn take_timeout_or_doorbell(&self, timeout: std::time::Duration, doorbell: &Doorbell, seen: u64) -> TryTake {
        let spin = self.core.ring.spin;
        self.core.ring.pop_timeout_or_doorbell(&self.seen_tail, timeout, spin, doorbell, seen)
    }

    /// [`Self::take_timeout_or_doorbell`] for a waiter that has already
    /// timed out once and knows the producer is idle -- parks straight
    /// away, same rationale as [`Self::take_timeout_cold`].
    pub fn take_timeout_or_doorbell_cold(&self, timeout: std::time::Duration, doorbell: &Doorbell, seen: u64) -> TryTake {
        self.core.ring.pop_timeout_or_doorbell(&self.seen_tail, timeout, 0, doorbell, seen)
    }

    /// [`Self::take_timeout_or_doorbell`] as a pure WAIT: parks on both this
    /// ring and `doorbell`, and hands back nothing -- `true` if the ring has
    /// data, `false` if the doorbell rang or the bound elapsed first. See
    /// [`Ring::wait_readable_or_doorbell`] for why a consuming primitive
    /// cannot serve the caller this exists for (`builtins::flow`'s
    /// multi-input round-robin).
    pub fn wait_readable_or_doorbell(&self, timeout: std::time::Duration, doorbell: &Doorbell, seen: u64) -> bool {
        let spin = self.core.ring.spin;
        self.core.ring.wait_readable_or_doorbell(&self.seen_tail, timeout, spin, doorbell, seen)
    }

    /// [`Self::wait_readable_or_doorbell`] for a waiter that already knows
    /// the producer is idle: parks straight away (spin budget 0).
    pub fn wait_readable_or_doorbell_cold(&self, timeout: std::time::Duration, doorbell: &Doorbell, seen: u64) -> bool {
        self.core.ring.wait_readable_or_doorbell(&self.seen_tail, timeout, 0, doorbell, seen)
    }
}

impl Handoff {
    /// Creates a single-slot rendezvous and returns its two halves.
    pub fn channel() -> (HandoffTx, HandoffRx) {
        Self::channel_with_spin(spin_limit())
    }

    /// As [`Handoff::channel`], with an explicit spin budget. See
    /// [`SpscRing::channel_with_spin`] for why `spin = 0` matters.
    pub fn channel_with_spin(spin: u32) -> (HandoffTx, HandoffRx) {
        let core = Arc::new(Handoff { ring: Ring::new(1, spin) });
        (
            HandoffTx { core: Arc::clone(&core), seen_head: Cell::new(0) },
            HandoffRx { core, seen_tail: Cell::new(0) },
        )
    }
}

impl HandoffTx {
    /// Blocking rendezvous put: returns `true` only once the consumer has
    /// actually taken the value, mirroring `chan_put` on an `Unbuffered`
    /// channel. Returns `false` if the transport was already closed, or if
    /// it closes while this put is still waiting.
    ///
    /// ## Documented deviation, close racing a pending rendezvous
    ///
    /// `chan_put`'s `Unbuffered` path *clears the buffer* when a close lands
    /// on a value that is sitting in the slot un-taken, so the value is
    /// discarded. This transport returns the same `false` but leaves the
    /// value drainable by the consumer's remaining `take`s. Reclaiming it
    /// would mean rolling `tail` backwards, which is unsound against a
    /// consumer that may already be reading the slot; and leaving it
    /// drainable is what the (more load-bearing) drain-then-`None` rule asks
    /// for everywhere else. The observable return value is identical.
    pub fn put(&self, v: Value) -> bool {
        let r = &self.core.ring;
        // Only the producer advances the tail count, so reading it here and
        // reusing it after `push` is sound.
        let tc = count_of(r.tail.load(Ordering::Relaxed));
        // Deposit first -- `push` carries the cached-index fast path, which
        // matters here: after the previous `put` returned, `seen_head` is
        // already known to equal `tc`, so this costs no cross-core load.
        if !r.push(v, &self.seen_head) {
            return false;
        }
        // Rendezvous: with cap == 1, "room for message tc + 1" is exactly
        // "the consumer has taken message tc".
        r.wait_for_room(tc + 1, &self.seen_head) == Wait::Ready
    }

    /// Non-blocking rendezvous put.
    ///
    /// ## Documented deviation from `offer!` on an `Unbuffered` channel
    ///
    /// `chan_try_put` additionally requires `waiting_takers > 0`, so that a
    /// non-blocking put can never strand a value on a channel nobody is
    /// listening to. That gate cannot fire usefully here (a 1:1 transport's
    /// consumer spends its first few thousand iterations *spinning*, not
    /// parked, so the gate would reject puts precisely when the consumer is
    /// most ready), and the condition it protects against cannot arise: a
    /// `Handoff` has exactly one, structurally-present consumer. This
    /// succeeds whenever the slot is empty and the transport is open.
    pub fn try_put(&self, v: Value) -> TryPut {
        self.core.ring.try_push(v, &self.seen_head)
    }
}

#[cfg(test)]
mod tests;
