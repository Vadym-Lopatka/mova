//! The Mova task runtime: `go` blocks as stackful coroutines multiplexed
//! M:1 onto N pinned shard threads (docs/L1-TASK-RUNTIME-DESIGN.md §3.1–3.3,
//! docs/L1-LANDING-SPEC.md §W2).
//!
//! This is the promoted form of the P2 probe (`src/runtime_probe.rs`), which
//! is evidence and not a foundation: the park/wake handshake below is the
//! one that probe PROVED (docs/L1-PROBE-RESULTS.md §2), and everything the
//! probe left single-shard, unpooled, or un-swapped is upgraded here.
//!
//! ## Shape
//!
//! `N = available_parallelism()` shards, each one OS thread named
//! `mova-shard-<i>`, started lazily on the first [`spawn`] and never joined.
//! A shard owns:
//!
//! - a [`Slab`] of everything alive on it (a `Coroutine` is `!Send`, so a
//!   task's coroutine is BUILT on its shard and dies there — only the
//!   `Box<dyn FnOnce() + Send>` thunk ever crosses a thread boundary, which
//!   is the exact payload `go*` hands `thread::Builder::spawn` today);
//! - a `local` run queue, no lock, for the shard's own re-queues;
//! - an `inbox` (`Mutex` + the shard's `Thread` handle for `unpark`), the
//!   cross-thread injector every [`spawn`] and every [`TaskWaker::wake`]
//!   pushes through;
//! - a [`stack_pool::StackPool`] (see that module for the R8 aging policy).
//!
//! **Tasks never migrate.** Placement happens once, at spawn, and is final.
//! Work stealing is deliberately out of scope for L1 (design §3.2) — pinning
//! is what lets the pool be lock-free, the coroutine stay `!Send`, and the
//! whole runtime need no `unsafe impl Send` anywhere.
//!
//! ## Spawn placement: family-local, with a flood guard (W4b)
//!
//! A root spawn (from a plain OS thread — `Interp::fork`, `go` typed at a
//! REPL, an embedder's entry point) round-robins across shards: unrelated
//! roots benefit from spreading, and there is no "local" shard to prefer.
//! A spawn made FROM inside a task is different: real CSP programs are
//! coordinator/worker trees (a `go` block that fans out N workers and
//! collects their results over chans it owns), and every rendezvous
//! between a coordinator and a worker it just spawned is a cross-shard
//! futex wake under pure round-robin — measured 2.3–2.5 µs/hop
//! (`tests/task_runtime_go_test.rs` G3), against ~194 ns/hop for a same-
//! shard scheduler pass (P2, `docs/L1-PROBE-RESULTS.md` §2). So a task-
//! spawned child is placed on the CALLER's shard instead, exactly Go's
//! local-run-queue precedent for `go` statements, PROVIDED that shard is
//! not already flooded (`Shard::load` below `SPAWN_LOCAL_MAX`); over the
//! threshold, spawn falls back to the same round-robin every root spawn
//! uses. Same-shard placement is the placement half of the L2 direct-switch
//! win (see "Direct switch (L2)" below, which is the scheduling half);
//! it landed first, and independently, because it pays for itself on
//! ordinary `Thread::unpark` alone.
//!
//! ## Why the shard threads are process-static and never joined
//!
//! Identical to `builtins::async`'s `TIMER` (see its doc at async.rs:757,
//! whose rationale applies here verbatim): the shards are SHARED across
//! `Interp` instances (an embedder can hold several, and `Interp::fork` makes
//! more), so tying their life to any one engine's shutdown would let that
//! shutdown silently break another engine's `go` blocks. Between jobs a
//! shard is parked in `thread::park()`, holding no locks and burning no CPU.
//! The one way this runtime differs from `TIMER`: a shard DOES run
//! interpreted code, so a task that never completes keeps its captured
//! `Interp` alive — the same leak a detached `go` thread has today, moved
//! from an OS stack to an 8 MiB coroutine.
//!
//! ## The park/wake handshake (proved in P2, kept verbatim)
//!
//! One `AtomicU8` per task: `RUNNING | PARKED | READY | NOTIFIED | DONE`,
//! plus L4's `KILLED`. Only the shard thread writes `RUNNING`/`PARKED`/
//! `DONE`; only a waker writes `READY` (from `PARKED`, plus enqueue) or
//! `NOTIFIED` (from `RUNNING`, no enqueue). No other transition exists, so
//! "is this task in a run queue?" has exactly one answer at all times.
//!
//! **L4's two extra edges (probe P5a/P5a-bis, `tests/l4_kill_probe.rs`
//! finding S4).** A KILLER — the flow supervisor's escalation, and nothing
//! else — writes [`KILLED`] from `PARKED` (plus a kill enqueue), mirroring
//! the waker's `PARKED -> READY`; the shard then force-unwinds the coroutine
//! and stores `DONE`. The second edge is the SALVAGE: a killer that won the
//! state CAS but found a peer's commit already landed in one of the dying
//! task's cells puts it back with `KILLED -> READY` (plus the enqueue the
//! peer's `wake()` was not allowed to make) and reports the kill as not
//! taken. `KILLED` is terminal to every `wake()` for the whole of that
//! window — `wake`'s existing `Err(_) => return` arm — which is what makes
//! the killer the only party that can move a `KILLED` task and therefore the
//! only possible enqueuer. See [`TaskWaker::kill`].
//!
//! 1. The parking task registers a [`TaskWaker`] clone with whatever will
//!    wake it, **under that thing's lock**; drops the lock; calls
//!    [`park_current_yield`].
//! 2. A wake site drains its registrations under the same lock and calls
//!    [`TaskWaker::wake`] on each after the guard drops (the clone-out
//!    discipline `builtins::async`'s ring sites already use).
//! 3. A wake landing in the window between (1) and the actual suspend finds
//!    the task still `RUNNING` and leaves `NOTIFIED` behind. When the
//!    suspend surfaces in the shard loop, its `RUNNING -> PARKED` CAS fails
//!    and the task is re-queued instead of parked. **A wake is therefore
//!    never lost, and never double-enqueued.**
//!
//! The spec sketches (3) as a separate `wake_pending: AtomicBool` checked
//! before parking a yielded task. `NOTIFIED` **is** that flag — folded into
//! the same atomic as the state on purpose, because a bare bool forces
//! `wake()` to enqueue unconditionally (it cannot tell running from parked),
//! and an unconditional enqueue puts a still-running task's id in the queue
//! a second time. Keeping flag and state in one word makes "set the flag"
//! and "should I enqueue?" a single CAS rather than two operations racing
//! each other.
//!
//! **A task must NOT hold a lock across the suspend.** The shard goes on to
//! run OTHER tasks on the same OS thread, and one of them taking that lock
//! would wedge the whole shard. This is the one place task parking is
//! genuinely weaker than `cv_wait`, which releases the mutex atomically with
//! the wait; docs/L1-PROBE-RESULTS.md §2 spells out the consequence and the
//! sudog hand-off (W3) that dissolves it.
//!
//! ## Direct switch (L2)
//!
//! When a deliverer hands a value to a task parked on ITS OWN shard, the
//! counterparty should run next rather than ride the inbox mutex back around
//! to the shard loop. That is Go's `runnext`, and it lands here in exactly
//! one function: every deliverer in the crate — unbuffered hand-off, buffered
//! promote, `close!`, a doorbell `ring()` — already funnels through
//! [`TaskWaker::wake`], so direct switch is a change to WHO RUNS NEXT with
//! **zero edits to chan code, to the commit cells, or to the registration
//! protocol** (docs/L2-DIRECT-SWITCH-DESIGN.md §2).
//!
//! The mechanism is two fields of the shard thread's identity block
//! ([`ctx::ExecTls`], see "Identity" below). `shard` holds `Arc::as_ptr` of
//! the shard this thread IS (0 on every other thread in the process),
//! published once at `shard_loop` entry. `direct_next` is the runnext slot:
//! one task id, 0 = empty. On its successful `PARKED -> READY`
//! CAS, `wake()` compares the target's shard against `e.shard`; if they
//! match and the slot is free, it leaves the id there and returns — no inbox
//! lock, no `Thread` clone, no `unpark`. Everything else takes the old
//! `inject_ready` path bit for bit: an OS-thread waker reads `e.shard == 0`
//! and fails the compare, a cross-shard waker fails it on the pointer.
//! The shard loop then consumes `direct_next` -> `local` -> inbox -> sleep,
//! and the sleep arm is structurally unreachable while the slot is occupied
//! (it lives past a slot check that came back empty, and only task code
//! running on THIS thread can refill the slot).
//!
//! The invariant the handshake rests on is untouched: the `PARKED -> READY`
//! CAS still succeeds exactly once, and the id still lands in exactly one
//! structure — slot XOR inbox. "Is this task enqueued somewhere?" keeps its
//! single answer.
//!
//! W2 added a second word to every enqueue: the woken task's [`Slab`] index,
//! read off `TaskShared::slot`. **The id remains the authority** — the slab
//! checks `entry.task.id == id` before it resumes anything, because slots are
//! recycled at `DONE` and ids never are. The slot only saves the lookup.
//!
//! **The budget is a LIVENESS rule, not a fairness knob.** A ping-pong pair
//! refills the slot on every hop, so an unbounded runnext loop never looks at
//! `local` or the inbox again — and the inbox is where `Job::Spawn`s live.
//! Measured (docs/L2-PROBE-RESULTS.md §4, §8): under two chatty pairs on one
//! shard, an effectively infinite budget leaves a co-located third task's
//! work squeezed into the gaps and finished at the very end of the run, and
//! a co-located `go` block that has not STARTED yet does not start at all
//! while the slot stays hot. [`DIRECT_SWITCH_BUDGET`] consecutive direct
//! switches therefore owe `local`/inbox one turn. It costs nothing
//! measurable — hop cost is flat across 16/64/256 — because the streak is a
//! register compare and the inbox peek amortizes over the whole budget.
//!
//! Divergence from Go, deliberate (design §6-V1): Go REPLACES a full runnext
//! and kicks the old occupant to the run queue; we keep the first occupant
//! and inject the second, because evicting would need `wake()` to reach the
//! shard's `local` queue — a stack variable of `shard_loop` — for a case
//! that costs exactly today's price when it happens. It happens often, not
//! rarely: two ping-pong pairs on one shard took the fallback 14 997 times
//! in the probe. Replace-and-evict wants the queues to carry entries rather
//! than ids, which is the same prerequisite as deleting the map probes, and
//! both are deferred together.
//!
//! ## What W2 bought, and what it cost (docs/L2-PROBE-RESULTS.md §6)
//!
//! W1 left a measured ~36 ns scheduler-pass floor. W2 took it to ~17 ns and
//! E1 from ~63 to ~50 ns/hop, in four independently-revertable steps, each
//! A/B'd against the previous one with interleaved runs of the SAME two
//! benches (the box drifted by 3 ns/hop over the session, so sequential
//! before/after numbers would have been fiction):
//!
//! | step | P3 sched-only | E1 |
//! |---|---|---|
//! | W1 baseline | 36.0-37.2 | 62.3-65.2 |
//! | 1. one thread-local block | 35.0-36.4 | 63.0-65.1 |
//! | 2. borrow the task, don't clone it | 23.3-24.8 | 58.6-59.8 |
//! | 3. slab instead of hash map | 16.4-17.6 | 49.4-51.2 |
//! | 4. per-shard `resumed` counter | 16.7-17.6 | 50.1-53.4 |
//!
//! **Step 1 bought nothing measurable, and that is a finding worth keeping.**
//! The probe's cost model charged ~14 TLV thunks to a hop and expected the
//! collapse to be the big win. A `sample(1)` profile of the W1 build put
//! `_tlv_get_addr` at 2.1% of shard-thread samples — under 1 ns/hop — because
//! LLVM already CSEs the thunk per thread-local per function, so L1 was
//! paying ~5 of those 14, not 14. The block stayed (it is what steps 2 and 3
//! store their new fields in, and four thread-locals became one), but nobody
//! should re-derive the TLS theory: on this target it is not where scheduler
//! time goes. The same profile named the two things that WERE:
//! `hashbrown::insert` at 6.9% and `park_current_yield` at 15%.
//!
//! Step 4 is likewise inside the noise on these two benches, and expected to
//! be: they run one hot shard, where a process-global counter is uncontended.
//! It is per-shard for the case the benches cannot show — 14 shards fighting
//! over one cache line on every hop.
//!
//! `MOVA_NO_DIRECT_SWITCH=1` (read once, `OnceLock`, the house kill-switch
//! pattern of `MOVA_NO_FASTSTEP`) makes `wake()` skip the slot and always
//! inject — L1 behaviour, for A/B and for bisecting a suspected scheduling
//! bug down to this lever.
//!
//! ## Identity: what a switch actually swaps
//!
//! Dynamic `binding` frames are keyed on a context id, not a `ThreadId`
//! (`src/ctx.rs`, design §3.4). Each task gets [`ctx::fresh_ctx`] at spawn
//! and a default (empty) [`ctx::BindingLocals`]. A switch swaps exactly two
//! things: the ctx id, and that pair of binding `Vec`s. Without them two
//! tasks sharing a shard would share a binding stack — silent semantic
//! corruption, not a performance problem (design §3.4).
//!
//! **What W2 changed is not WHAT is swapped but how many thread-local
//! accesses it costs.** On aarch64-darwin a `thread_local!` access is a call
//! through the TLV descriptor; there is no folding it into an addressing
//! mode. L1 spread the switch state over six thread-locals and touched them
//! one field at a time — `CURRENT_YIELDER` ×3, `CURRENT_TASK` ×3,
//! `SHARD_TLS`, `DIRECT_NEXT` ×2, and the four `ctx::` swap calls — about
//! **14 thunks per hop** (docs/L2-PROBE-RESULTS.md §6 item 1). Now:
//!
//! - every scalar the scheduler switches (`ctx`, `yielder`, `shard`,
//!   `direct_next`, and the running task's borrow) lives in ONE
//!   `const`-initialized, `Drop`-free block, [`ctx::ExecTls`], reached
//!   through [`ctx::with_exec`] — one thunk resolves the block and every
//!   field after it is a plain load or store;
//! - the binding pair is swapped with a single [`ctx::swap_binding_locals`]
//!   in each direction instead of an install/take pair, halving its two
//!   thread-locals' accesses from four to two.
//!
//! `env.rs`'s per-binding-op access pattern is deliberately untouched, and
//! so is every plain OS thread's behaviour: a non-shard thread's block is
//! all zeros (`yielder == 0` is [`in_task`] `== false`, `shard == 0` is "I
//! am nobody's scheduler"), which is bit-for-bit what it was when these were
//! separate thread-locals.
//!
//! ## Panics and `panic=abort`
//!
//! A panicking task is caught at its resume site and its shard continues
//! (design R3, P2/G5). This depends on `-C panic=unwind`, mova's default:
//! corosensei's `catch_unwind_at_root` is a plain `catch_unwind` running ON
//! the coroutine's own stack. **Under `panic=abort` a panicking task takes
//! the process down**, and nothing here can prevent that.
//!
//! ## Accepted footgun (design R1)
//!
//! A blocking native inside a task — file IO, `Thread/sleep`, deref of an
//! unresolved future — blocks its whole SHARD, not just its task. Same class
//! as the JVM's 8-thread `go` pool, with N-core width instead of 8. The
//! escape hatch is `thread`, which keeps spawning a real OS thread. W4
//! documents this in `core/async.mova` before `go` reroutes here.

mod stack_pool;


use std::collections::VecDeque;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use corosensei::stack::DefaultStack;
use corosensei::{Coroutine, CoroutineResult, Yielder};

use crate::ctx::{self, BindingLocals};
use crate::sync::lock_mutex;
use crate::value::{TakeSlot, PUT_KILLED, PUT_WAITING};
use stack_pool::StackPool;

/// Per-task stack RESERVATION (design §3.1 / R5). Reserved, not committed:
/// `DefaultStack` `mmap`s `PROT_NONE` and `mprotect`s the usable range, so an
/// idle task costs address space plus the pages it touched — P1/B3 measured
/// **16.05 KiB/task**, flat from 100k to 1M tasks on a 16 KiB-page machine.
///
/// One eighth of `builtins::async::ASYNC_STACK_SIZE` (64 MiB), and chosen
/// from P2's measurement rather than from taste: the interpreter costs
/// **6192 B per non-tail Mova frame**, so 8 MiB holds ~1354 of them, while
/// `DEFAULT_MAX_CALL_DEPTH = 200` trips at ~1.2 MiB. For default
/// configurations 8 MiB is roomy; an embedder that raises `max_depth` past
/// ~1350 hits a guard page where a 64 MiB thread today survives to ~10 800
/// frames (design R5).
pub const TASK_STACK_SIZE: usize = 8 * 1024 * 1024;

// Task states. Shard-thread-only transitions: -> RUNNING (resume),
// -> PARKED (yield with no pending wake), -> DONE (return/panic/kill).
// Waker-only transitions: PARKED -> READY (plus enqueue),
// RUNNING -> NOTIFIED (no enqueue -- the shard re-queues at park time).
// Killer-only transitions: PARKED -> KILLED (plus the kill enqueue), and
// the salvage KILLED -> READY (plus enqueue). See the module doc.
const RUNNING: u8 = 0;
const PARKED: u8 = 1;
const READY: u8 = 2;
const NOTIFIED: u8 = 3;
const DONE: u8 = 4;
/// A task whose `PARKED -> KILLED` CAS a supervisor has WON, and whose
/// `Job::Kill` is on the way to its shard (docs/L4-SUPERVISION-DESIGN.md
/// §3.5; landed in L4 W3, evidence in `tests/l4_kill_probe.rs`). Transient —
/// the shard stores `DONE` once the coroutine has force-unwound — but for
/// the whole of that window it is terminal *to every waker*:
/// `TaskWaker::wake`'s `Err(_) => return` arm already treats any
/// non-`PARKED`/non-`RUNNING` state as "nothing to do", so a peer that
/// delivers into a dying task and rings its waker is a no-op by the EXISTING
/// contract, with no edit to `wake()` at all.
///
/// Killer-only transition (`PARKED -> KILLED`, plus the kill enqueue),
/// mirroring the waker's `PARKED -> READY`. The two race on one CAS and
/// exactly one wins, which is the whole of wall W7's decision procedure.
const KILLED: u8 = 5;

static NEXT_TASK_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_SHARD: AtomicUsize = AtomicUsize::new(0);
static TASKS_SPAWNED: AtomicU64 = AtomicU64::new(0);
static TASKS_FINISHED: AtomicU64 = AtomicU64::new(0);
static TASKS_PANICKED: AtomicU64 = AtomicU64::new(0);
/// Tasks destroyed by [`TaskWaker::kill`].
static TASKS_KILLED: AtomicU64 = AtomicU64::new(0);
/// Killed tasks whose stack could NOT be returned to the pool because the
/// forced unwind itself unwound out of the shard (a destructor that
/// panicked, or a foreign panic resumed by `force_unwind`). Each one is an
/// 8 MiB reservation leaked on purpose rather than an abort — measured
/// instead of hidden (P5a clause (4) observed Δ0 in every run).
static KILL_STACKS_LEAKED: AtomicU64 = AtomicU64::new(0);
/// Kills that won the `PARKED -> KILLED` CAS and were then ABORTED because a
/// peer's commit had already landed in one of the task's cells. The task was
/// resurrected (`KILLED -> READY` + enqueue) and the caller got `false`.
/// This is the counter that shows the tombstone is doing work rather than
/// getting lucky — and the reason a supervisor's kill is a RETRY LOOP and
/// not a single shot (P5a-bis measured ~2.5k/100k putter-side and
/// ~22k/100k taker-side salvages under a contested race).
static KILLS_SALVAGED: AtomicU64 = AtomicU64::new(0);

type Body = Box<dyn FnOnce() + Send + 'static>;
type TaskCoroutine = Coroutine<(), (), (), DefaultStack>;

// ---------------------------------------------------------------------------
// Task identity, wakers
// ---------------------------------------------------------------------------

/// The cross-thread half of a task: everything a [`TaskWaker`] needs, and
/// nothing the coroutine owns. The `Coroutine` itself is `!Send` and never
/// leaves its shard's `tasks` map.
struct TaskShared {
    id: u64,
    state: AtomicU8,
    shard: Arc<Shard>,
    /// Where this task's [`TaskEntry`] sits in its shard's [`Slab`] (W2 item
    /// 3). [`UNASSIGNED_SLOT`] between [`spawn`] and the shard's `build_task`,
    /// then fixed for the task's life; freed for reuse at `DONE`.
    ///
    /// **The id, not this, is what makes a wake trustworthy.** A slot is
    /// reused, an id never is, so every resume checks `entry.task.id == id`
    /// and treats a mismatch as a no-op — see [`Slab::take`].
    ///
    /// Atomic because a waker on another thread reads it to fill in the
    /// enqueue, but `Relaxed` is enough in both directions: the shard thread
    /// stores it before it ever resumes the task, the task publishes its own
    /// waker under a chan lock, and the waker acquires that lock — so the
    /// store is ordered before any read of it by the lock's release/acquire
    /// edge, with no ordering of its own required.
    slot: AtomicU32,
    /// The task's sudog cells, one pair per TASK rather than one pair per
    /// PARK (L2 lever 2, docs/L2-DIRECT-SWITCH-DESIGN.md §1/§3-Q4). Handed
    /// out by [`current_take_cell`]/[`current_put_cell`], which cost an `Arc`
    /// bump where `builtins::async`'s two registration sites used to call
    /// `Arc::new` — a malloc, plus a free when the waiter drops — on every
    /// blocking chan op that parks. Measured worth 11.8 ns/hop of E1's 93.2
    /// (docs/L2-PROBE-RESULTS.md §3), the joint-largest of the three levers.
    ///
    /// **This changes cell LIFETIME and nothing else** (S5: the commit
    /// protocol is untouched, by construction — see the soundness argument
    /// at [`current_put_cell`]).
    take_cell: Arc<Mutex<TakeSlot>>,
    put_commit: Arc<AtomicU8>,
}

/// A parked task's wake handle — the `Waker::Task(TaskRef)` arm of design
/// §3.3. Cheap to clone (one `Arc` bump), because wake sites clone their
/// whole registration list out from under a lock before ringing anything.
#[derive(Clone)]
pub struct TaskWaker {
    task: Arc<TaskShared>,
}

impl TaskWaker {
    /// Make the task runnable.
    ///
    /// Safe from any thread, from inside another task on the same shard, and
    /// on an already-runnable or already-finished task (both are no-ops).
    /// It touches exactly one atomic plus the target shard's inbox mutex and
    /// never re-enters chan state, so it can neither re-enter nor invert a
    /// lock order — which is what makes the one wake site that must fire
    /// under a chan lock (the unbuffered hand-off) safe as well.
    ///
    /// L2: when the caller IS the target's shard thread, the enqueue becomes
    /// a thread-local store instead — see the module doc's "Direct switch"
    /// section. Every other caller keeps the L1 path unchanged.
    pub fn wake(&self) {
        loop {
            match self.task.state.compare_exchange(
                PARKED,
                READY,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    // W2 item 3: the enqueue carries the slab index beside
                    // the id. The id is still the authority (`Slab::take`
                    // checks it); the slot is only a way to skip the lookup.
                    let slot = self.task.slot.load(Ordering::Relaxed);
                    // A waker can only be minted by `current_waker()`, which
                    // requires the task to have RUN, which happens-after
                    // `build_task`'s slot store -- so `UNASSIGNED_SLOT` here
                    // means that premise broke somewhere, and the release
                    // consequence would be a silently LOST wake
                    // (`Slab::take` rejects the slot). Make it loud in
                    // debug builds instead.
                    debug_assert!(
                        slot != UNASSIGNED_SLOT,
                        "mova runtime: a waker observed an unassigned slab slot \
                         (a TaskWaker existed before its task's first resume)"
                    );
                    if self.try_direct_switch(slot) {
                        return;
                    }
                    self.task.shard.inject_ready(self.task.id, slot);
                    return;
                }
                // Still on the CPU: it registered us and has not suspended
                // yet. Leave the note; the shard consumes it at the park
                // boundary instead of parking. This is the missed-wakeup
                // arm -- see the module doc.
                Err(RUNNING) => {
                    if self
                        .task
                        .state
                        .compare_exchange(RUNNING, NOTIFIED, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                    {
                        return;
                    }
                }
                // READY (already queued), NOTIFIED (already noted), DONE.
                Err(_) => return,
            }
        }
    }

    /// **Destroy this task where it stands** — the L4 kill primitive
    /// (docs/L4-SUPERVISION-DESIGN.md §3.5). Landed in W3; every soundness
    /// argument below is the one probe P5a/P5a-bis validated, and
    /// `tests/l4_kill_probe.rs` is its standing regression net.
    ///
    /// The only caller in the tree is `builtins::flow`'s supervisor
    /// escalation ladder (graceful `::flow/stop`, grace window, then kill),
    /// through [`TaskHandle::kill`]; this method is the same entry reached
    /// from a waker the target minted itself, which is what the probe uses.
    ///
    /// Returns `true` iff the kill was CLAIMED: the `PARKED -> KILLED` CAS
    /// succeeded and a `Job::Kill` is now queued on the task's shard, which
    /// will force-unwind the coroutine without ever resuming user code.
    /// `false` means the task was not parked at the instant of the CAS
    /// (`RUNNING`, `READY`, `NOTIFIED`, already `KILLED`, or `DONE`) — a
    /// supervisor that still wants it dead retries; a task spinning in
    /// `RUNNING` forever is the design's documented unreachable case.
    ///
    /// **The CAS is the entire race protocol.** A peer committing a value
    /// into this task concurrently runs `wake()`, whose first move is the
    /// `PARKED -> READY` CAS on the same word. Exactly one of the two
    /// succeeds:
    /// - waker first → state `READY`, our CAS fails, we return `false`; the
    ///   task is resumed and completes its op normally;
    /// - killer first → state `KILLED`, `wake()` falls into its existing
    ///   `Err(_) => return` arm and does nothing (the "wake on a DONE task
    ///   is harmless" contract, reused verbatim).
    ///
    /// **P5a-bis: the state CAS is necessary but NOT sufficient.** The peer
    /// writes its commit BEFORE it calls `wake()`, under the chan lock, so
    /// "killer wins the state CAS" still admitted *committed, then killed* —
    /// measured at 30% (taker) / 52% (putter) of contested rounds in P5a,
    /// which for a killed taker is a LOST MESSAGE. The fix is a second,
    /// finer arbitration: after winning the state CAS the killer CLAIMS the
    /// dying task's own commit cells, on the very words the deliverers
    /// already write.
    ///
    /// - `put_commit`: `CAS(PUT_WAITING -> PUT_KILLED)`. The deliverers
    ///   (`take_from_task_putter_cold`, `promote_task_putters_cold`) commit
    ///   with `CAS(PUT_WAITING -> PUT_DONE)` on the same word, so exactly one
    ///   of claim and commit wins; a deliverer that loses culls the corpse's
    ///   waiter and moves to the next one, and the corpse's value dies with
    ///   it (correct kill semantics — it was never committed).
    /// - `take_cell`: a [`TakeSlot::Closed`] tombstone written under the
    ///   cell's own mutex — the SAME mutex `deliver_to_task_taker_cold`
    ///   already takes to commit. A deliverer that finds a non-`Waiting`
    ///   cell culls that waiter and tries the next one, or falls through to
    ///   the buffer / its own park. The put is never swallowed.
    ///
    /// `Closed` is the tombstone rather than a `TakeSlot::Cancelled`
    /// variant: it is behaviourally exact — it already means "this cell will
    /// never yield a value" — every existing reader does the right thing
    /// with it, and no LIVE task can observe it, because the tombstone is
    /// only ever written on a path that ends in `inject_kill`, and a task
    /// that is PARKED is by construction present in its shard's slab, so
    /// that kill always completes. (Documented at [`TakeSlot`] too.)
    ///
    /// **Both cells must be at their RESTING value for a kill to land**, and
    /// that is a whole-runtime invariant, not a chan-local one: a task
    /// parked on a `Doorbell` or a timer — which is where a flow proc spends
    /// most of its life — is in no chan queue at all, so the claims below
    /// are the only thing that can distinguish it from a putter whose commit
    /// just landed. `take_cell` rests at `Waiting` (`park_task_taker`'s
    /// `mem::replace` re-arms it on every exit) and `put_commit` rests at
    /// `PUT_WAITING` (`park_task_putter` re-arms it on every exit — see the
    /// note there). Without the put-side re-arm, killing a doorbell-parked
    /// proc that had ever completed a blocking put would salvage-loop
    /// forever instead of ever landing.
    ///
    /// **The salvage arm.** If either claim fails, a commit had already
    /// landed: the value is in the task's hands and killing it now would
    /// destroy it. So the kill is ABORTED — the state goes `KILLED -> READY`
    /// and the task is enqueued, exactly as the peer's `wake()` would have —
    /// and this returns `false`, the supervisor's cue to retry at the next
    /// park. Nobody else can have enqueued it: every `wake()` in that window
    /// saw `KILLED` (or now sees `READY`) and took its existing
    /// `Err(_) => return` arm, so the "id is in exactly one structure"
    /// invariant holds with our enqueue as the only one.
    ///
    /// Claim ORDER is load-bearing: `put_commit` first, because its failure
    /// needs no undo; `take_cell` second, and its failure undoes the put
    /// claim. That undo is safe precisely because a non-`Waiting` take cell
    /// proves the task is a TAKER, hence is in no `task_putters` queue,
    /// hence has no other possible writer of `put_commit`.
    ///
    /// **S2 — the corpse is culled LAZILY, and that is the ruling.** A killed
    /// waiter stays queued on its chan until some deliverer walks past it and
    /// culls it. The alternative the probe floated — eagerly unregistering
    /// the dying task from its chan queues — was weighed and REJECTED, on
    /// machinery rather than taste: nothing records which chans a task is
    /// parked on, so it would need a new store on the park path (the exact
    /// place L2 spent three levers getting work OUT of), a new field on
    /// `TaskShared`, an alts-shaped generalization to N chans, and a chan
    /// lock taken from the kill path. That is a permanent cost on the hot
    /// path to tidy a set whose size is already bounded:
    ///
    /// - **at most one corpse per killed task per chan it was parked on**
    ///   (one park = one queue entry; `alts` is the only multi-chan shape),
    /// - **a task is killed at most once** (a claimed kill is terminal; a
    ///   REFUSED kill leaves nothing behind at all — the salvage arm undoes
    ///   its `put_commit` claim and never writes the take-side tombstone),
    /// - and **a restart leaves no corpses**, because a proc that dies any
    ///   other way — normally, stopped, panicking — has by construction
    ///   already returned from its park and taken its waiter with it. Only a
    ///   kill can destroy a task AT a park point. So the "supervisor
    ///   restarting a proc 1k times queues 1k corpses" pathology S2 imagined
    ///   is not reachable: the one killer in the tree is the flow
    ///   supervisor's `stop-proc` escalation, and a stop-proc'd run does not
    ///   restart.
    ///
    /// Every corpse is also self-healing (the first delivery attempt on that
    /// chan culls it) and harmless while it waits (every deliverer consults
    /// the waiter queue before the buffer and claims before it commits). What
    /// it is NOT is invisible: it makes the queue-vs-buffer invariant true of
    /// LIVE waiters only, which is finding S1 and is now written down where
    /// the invariant lives (`value.rs`'s `ChanState::task_takers`).
    ///
    /// The drop happens on the task's OWN shard, never here: a `TaskEntry`
    /// owns a `!Send` `Coroutine`, so only its shard thread may touch it.
    /// The order carries `(id, slot)` and is ABA-checked by `Slab::take`
    /// exactly like `Job::Run`.
    pub fn kill(&self) -> bool {
        if self
            .task
            .state
            .compare_exchange(PARKED, KILLED, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return false;
        }
        // Same happens-before argument `wake()` makes for this load: the
        // task cannot be PARKED without having run, and `build_task`
        // stored the slot before its first resume.
        let slot = self.task.slot.load(Ordering::Relaxed);
        debug_assert!(
            slot != UNASSIGNED_SLOT,
            "mova runtime: killed a task with an unassigned slab slot"
        );

        // --- P5a-bis: claim the corpse's commit cells, or salvage. ---
        // Put side first: a failed claim needs no undo.
        if self
            .task
            .put_commit
            .compare_exchange(PUT_WAITING, PUT_KILLED, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return self.abort_kill_and_resume(slot);
        }
        // Take side, under the mutex the deliverer commits through.
        {
            let mut cell = lock_mutex(&self.task.take_cell);
            if !matches!(*cell, TakeSlot::Waiting) {
                drop(cell);
                // Undo the put claim. Sound because a non-`Waiting` take cell
                // proves this task is a TAKER, so it sits in no
                // `task_putters` queue and no deliverer can be racing us for
                // this word.
                self.task.put_commit.store(PUT_WAITING, Ordering::Release);
                return self.abort_kill_and_resume(slot);
            }
            *cell = TakeSlot::Closed;
        }

        self.task.shard.inject_kill(self.task.id, slot);
        true
    }

    /// P5a-bis salvage: a commit landed under us after we had already won the
    /// `PARKED -> KILLED` CAS. Put the task back on its feet — `KILLED ->
    /// READY` plus the enqueue the peer's `wake()` was not allowed to make —
    /// and report the kill as not taken.
    #[cold]
    #[inline(never)]
    fn abort_kill_and_resume(&self, slot: u32) -> bool {
        KILLS_SALVAGED.fetch_add(1, Ordering::Relaxed);
        // A plain store, not a CAS: we hold the task exclusively. `KILLED` is
        // terminal to every `wake()`, so no other party can have moved it,
        // and no other party can have enqueued it either — which is what
        // makes the enqueue below the only one.
        self.task.state.store(READY, Ordering::Release);
        self.task.shard.inject_ready(self.task.id, slot);
        false
    }

    /// This task's id — introspection, so a caller can correlate a kill with
    /// the shard-side bookkeeping.
    pub fn task_id(&self) -> u64 {
        self.task.id
    }

    /// `true` once the task has reached a terminal state (`DONE`, or
    /// `KILLED` while its shard works through the order). A supervisor's
    /// retry loop uses it to stop asking.
    pub fn is_terminal(&self) -> bool {
        matches!(self.task.state.load(Ordering::Acquire), DONE | KILLED)
    }

    /// The runnext arm (module doc, "Direct switch"). `true` means the id is
    /// in this thread's slot and the caller owes it no enqueue.
    ///
    /// Reached only from the `PARKED -> READY` arm above, i.e. once per real
    /// park, so the two thread-local reads are on a park-boundary path and
    /// never on pure-OS chan traffic — the W5c rule. (`wake()`'s own callers
    /// are `builtins::async`'s `#[cold] #[inline(never)]` `wake_all_cold` and
    /// `Doorbell::ring`'s post-`drop(guard)` loop; neither is a place LLVM can
    /// hoist a TLV thunk into a hot prologue from.)
    ///
    /// The three checks are ordered by cost and by who pays them: the pointer
    /// compare rejects every OS thread (`ExecTls::shard == 0`) and every
    /// cross-shard waker first, so the kill switch's `OnceLock` load and the
    /// slot read are only ever paid by a same-shard wake — the one case that
    /// is about to profit from them.
    ///
    /// W2: ONE thread-local access for both checks and both stores — the
    /// identity block is resolved once and the shard compare, the runnext
    /// read and the (id, slab-slot) write are plain loads and stores off it.
    #[inline]
    fn try_direct_switch(&self, slot: u32) -> bool {
        let target = Arc::as_ptr(&self.task.shard) as usize;
        ctx::with_exec(|e| {
            if e.shard.get() != target || direct_switch_disabled() {
                return false;
            }
            // Slot occupied: keep the first occupant and inject the second
            // (design §6-V1 — we do NOT replace-and-evict the way Go does;
            // the module doc says why). The loser pays the L1 price.
            if e.direct_next.get() == 0 {
                e.direct_slot.set(slot);
                e.direct_next.set(self.task.id);
                true
            } else {
                false
            }
        })
    }
}

/// A spawn-time handle on a task, for the ONE thing a supervisor needs and
/// nothing else: killing it (L4 W3, docs/L4-SUPERVISION-DESIGN.md §3.5).
///
/// **Why not just hand back a [`TaskWaker`].** A waker minted before the
/// task's first resume has no slab slot yet, and [`TaskWaker::wake`]
/// `debug_assert!`s that it does — waking such a handle would be a lost
/// wake in release and a panic in debug. [`kill`](Self::kill) has no such
/// hazard: it acts only on a task that is already `PARKED`, which is proof
/// the task ran and therefore proof the slot is assigned. Exposing exactly
/// the two methods that are safe from a pre-resume handle is cheaper than
/// documenting a footgun.
///
/// `pub(crate)`: the kill surface is `builtins::flow`'s supervisor and
/// nobody else. (`tests/l4_kill_probe.rs` drives the same machinery through
/// the `pub` [`TaskWaker::kill`], from a waker the target minted itself.)
#[derive(Clone)]
pub(crate) struct TaskHandle {
    waker: TaskWaker,
}

impl TaskHandle {
    /// [`TaskWaker::kill`], verbatim — see there for the whole protocol.
    /// `false` means "not parked at that instant, or already dead": the
    /// caller retries, and gives up on a budget (a task spinning in
    /// `RUNNING` forever is the design's documented unreachable case).
    pub(crate) fn kill(&self) -> bool {
        self.waker.kill()
    }

    /// `true` once this task is `DONE`/`KILLED` — what stops a retry loop.
    pub(crate) fn is_terminal(&self) -> bool {
        self.waker.is_terminal()
    }
}

/// True when `MOVA_NO_DIRECT_SWITCH=1` was set in the environment at process
/// start: `wake()` then always injects, which is L1 behaviour exactly.
///
/// Read ONCE into a `OnceLock` (the house kill-switch pattern —
/// `builtins::flow`'s `MOVA_NO_FASTSTEP`/`MOVA_NO_FUSION`), so no wake ever
/// touches the environment, and so the switch cannot change meaning halfway
/// through a run.
///
/// L5 / P6a: sim mode disables it too (design §3, "One shard"). At one shard
/// the runnext slot fires on ~100% of wakes, which would hard-code ONE
/// schedule; funnelling every wake through `inject_ready` instead puts the
/// whole runnable set into one observable queue for the seeded pick.
///
/// L5/W4: the memo lives OUTSIDE the fn so [`runtime_untouched`] can see
/// whether the answer has been latched yet.
static DIRECT_SWITCH_OFF: OnceLock<bool> = OnceLock::new();

fn direct_switch_disabled() -> bool {
    let flag = &DIRECT_SWITCH_OFF;
    *flag.get_or_init(|| {
        crate::clock::sim_enabled() || std::env::var("MOVA_NO_DIRECT_SWITCH").is_ok_and(|v| v == "1")
    })
}

// ---------------------------------------------------------------------------
// Shards
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Inbox {
    /// New tasks: the `Send` thunk, to be turned into a `Coroutine` on the
    /// shard thread (a `Coroutine` cannot cross a thread boundary).
    spawns: VecDeque<(Arc<TaskShared>, Body)>,
    /// Woken tasks, as `(id, slab slot)`. The id is the authority and the
    /// slot is a hint that saves the lookup — see [`Slab::take`] (W2 item 3).
    ready: VecDeque<(u64, u32)>,
    /// Kill orders (L4 W3), same `(id, slot)` shape and the same
    /// id-is-the-authority rule. Deliberately in the SAME mutex as `ready`
    /// rather than in a queue of its own: `Inbox::thread` is published under
    /// this lock, which is the whole reason "pushed but nobody to unpark" is
    /// impossible, and a second lock would need that argument rebuilt from
    /// scratch (and would let the shard sleep through a kill it had already
    /// walked past).
    kills: VecDeque<(u64, u32)>,
    /// The shard thread's own handle, published by the shard thread itself
    /// before it first looks at this queue. Kept INSIDE the mutex on
    /// purpose: a waker reads it under the same lock it pushed under, so
    /// "pushed but nobody to unpark" is impossible without the shard being
    /// guaranteed to see the push on its first pass.
    thread: Option<std::thread::Thread>,
}

struct Shard {
    index: usize,
    inbox: Mutex<Inbox>,
    /// Tasks that ran to completion (or panicked) here. Introspection only —
    /// it is what proves a 100k-task run actually spread across shards.
    finished: AtomicU64,
    /// Live-task heuristic for spawn placement (W4b): `+1` when a task is
    /// PLACED here (spawn time), `-1` when it reaches `DONE`. Relaxed and
    /// deliberately coarse — it does not distinguish a parked task (idle,
    /// holding no CPU) from a running or freshly-queued one, because
    /// telling them apart would need a CAS or lock on the park/resume hot
    /// path for the sake of a value this only reads as a rough "is this
    /// shard busy?" signal. A family that is mostly parked (waiting on
    /// chans, say) therefore looks "full" to `spawn` sooner than it truly
    /// is; the only consequence is falling back to round-robin a bit
    /// early, never a placement correctness issue (tasks don't migrate
    /// either way, so a "wrong" call just costs one cross-shard hop).
    load: AtomicUsize,
    /// Resumes this shard served out of its runnext slot rather than out of
    /// `local`/inbox (L2). Introspection only, and PER-SHARD rather than
    /// process-global on purpose: the writer is always this shard's own
    /// thread, so an uncontended relaxed `fetch_add` on a line nobody else
    /// touches is the cheapest honest form — the probe priced the global
    /// version of this counter at ≤ 0.7 ns/hop and that was already inside
    /// the noise, but there is no reason to pay even that in cache traffic
    /// once 14 shards are all hot. Summed on read by [`direct_switches`].
    direct_switches: AtomicU64,
    /// Resumes this shard served, by any route. Was a process-global
    /// `AtomicU64` through L1/W1 and is per-shard for W2 item 5, for exactly
    /// the reason spelled out for `direct_switches` above: the writer is
    /// always this shard's own thread, so the honest cost is an uncontended
    /// relaxed `fetch_add` on a line nobody else touches, rather than one
    /// cache line every shard in the process fights over on every hop. The
    /// public [`tasks_resumed`] keeps its signature and sums on read.
    resumed: AtomicU64,
}

impl Shard {
    /// Cross-thread enqueue: push, then unpark. `Thread::unpark` leaves a
    /// token if the shard has not parked yet, so the shard cannot sleep
    /// through a push that raced its emptiness check.
    fn inject_ready(&self, id: u64, slot: u32) {
        let mut g = lock_mutex(&self.inbox);
        g.ready.push_back((id, slot));
        let th = g.thread.clone();
        drop(g);
        if let Some(th) = th {
            th.unpark();
        }
    }

    /// Hand a claimed kill to the shard that owns the task (L4 W3).
    /// Byte-for-byte [`inject_ready`]'s shape, on the same mutex and the same
    /// unpark discipline.
    fn inject_kill(&self, id: u64, slot: u32) {
        let mut g = lock_mutex(&self.inbox);
        g.kills.push_back((id, slot));
        let th = g.thread.clone();
        drop(g);
        if let Some(th) = th {
            th.unpark();
        }
    }

    fn inject_spawn(&self, task: Arc<TaskShared>, body: Body) {
        let mut g = lock_mutex(&self.inbox);
        g.spawns.push_back((task, body));
        let th = g.thread.clone();
        drop(g);
        if let Some(th) = th {
            th.unpark();
        }
    }
}

/// How many shards this process runs. Answerable without starting anything,
/// so an introspection call does not spawn N OS threads as a side effect.
///
/// Two overrides, read once (house kill-switch pattern), in priority order:
///
/// - **sim mode forces 1**, unconditionally (L5 design §3): cross-shard
///   racing is a nondeterminism source that no seed can cover, so the
///   simulation runs the whole task world on one shard. Every N>1 degeneracy
///   was audited in the census — `pick_shard` -> shard 0, `SPAWN_LOCAL_MAX`
///   moot, `segment_shards` -> the already-tested Pin(0) shape.
/// - **`MOVA_SHARDS=<n>`**, clamped to >= 1: a general lever (a quiet-machine
///   bench wants it too), not a sim-only one. Unset means today's answer,
///   byte for byte.
/// [`shard_n`]'s memo, lifted OUT of the function so [`runtime_untouched`]
/// can see whether the answer has been latched yet (L5/W4).
static SHARD_N: OnceLock<usize> = OnceLock::new();

fn shard_n() -> usize {
    let n = &SHARD_N;
    *n.get_or_init(|| {
        if crate::clock::sim_enabled() {
            return 1;
        }
        if let Some(n) = std::env::var("MOVA_SHARDS").ok().and_then(|v| v.trim().parse::<usize>().ok()) {
            return n.max(1);
        }
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
    })
}

static SHARDS: OnceLock<Vec<Arc<Shard>>> = OnceLock::new();

/// The shards, started on first use. `OnceLock` is the house pattern for a
/// process-static service thread (`builtins::async`'s `TIMER`); see the
/// module doc for why these are never joined.
fn shards() -> &'static [Arc<Shard>] {
    SHARDS.get_or_init(|| {
        let n = shard_n();
        let mut v = Vec::with_capacity(n);
        for index in 0..n {
            let shard = Arc::new(Shard {
                index,
                inbox: Mutex::new(Inbox::default()),
                finished: AtomicU64::new(0),
                load: AtomicUsize::new(0),
                direct_switches: AtomicU64::new(0),
                resumed: AtomicU64::new(0),
            });
            let s = shard.clone();
            std::thread::Builder::new()
                .name(format!("mova-shard-{index}"))
                .spawn(crate::memstat::drained(move || shard_loop(s)))
                .expect("mova runtime: couldn't spawn a shard thread");
            v.push(shard);
        }
        v
    })
}

/// **L5/W4 — "sim must be the process's FIRST runtime use", answered.**
///
/// `simulate` may turn sim mode on programmatically, but only while nothing
/// has yet baked a real-mode answer into a `OnceLock`. There are exactly
/// three such answers in this module, and this checks all three:
///
/// - [`SHARDS`] — shard threads exist, running the real-mode cascade with
///   `sim` hoisted false for their whole lives;
/// - [`SHARD_N`] — the shard COUNT was answered (e.g. by a
///   `runtime::shard_count()` introspection call) before any shard started;
///   sim forces 1, so a latched `available_parallelism()` is already wrong;
/// - [`DIRECT_SWITCH_OFF`] — the runnext lever was resolved; sim needs it
///   off, so that every wake lands in one observable queue for the seeded
///   pick (design §3).
///
/// `builtins::sim::simulate` turns a `false` here into the named
/// `SIM-E-RUNTIME-STARTED` error. Nothing else may call it: it is a
/// precondition, not a status.
pub(crate) fn runtime_untouched() -> bool {
    SHARDS.get().is_none() && SHARD_N.get().is_none() && DIRECT_SWITCH_OFF.get().is_none()
}

/// L5 / P6b — the OS-thread arm bridge.
///
/// In sim mode there is no timer thread (design §2): the heap is drained
/// *inline* by the sim shard at its idle point, and the only thing that ever
/// breaks that idle point's `park()` is an `inject_*` unpark. That is
/// sufficient for the P6a boundary contract, where the only entity that can
/// arm a timer is a task running ON the shard — an arm made by a running
/// shard needs no wake, because the shard re-enters `sim_next_job` and
/// consults the heap on its own.
///
/// It is NOT sufficient the moment an OS thread arms a timer, which is what
/// every existing test does: `(<!! (timeout 100))` on a test thread pushes an
/// entry onto a heap nobody is watching and then blocks. The shard is parked;
/// no `inject_*` is coming; virtual time never advances. That is a
/// STRUCTURAL hang, not a schedule — so `timer_push` calls this.
///
/// Shape: unpark shard 0 (sim forces `shard_n() == 1`), on
/// [`Shard::inject_ready`]'s exact discipline — lock the inbox, clone the
/// `Thread`, DROP the guard, then unpark. Two deliberate choices:
///
/// - **In sim the arm STARTS the shard if none exists** (`shards()`, not
///   `SHARDS.get()`). Real mode can afford "arm now, the timer thread is
///   somebody else's problem" because the timer thread exists independently
///   of the task world. Sim has no timer thread: the shard loop IS the timer
///   service (design §2, swap 2). A process that arms a timer without ever
///   spawning a task therefore leaves the heap ORPHANED — nobody will ever
///   advance virtual time to its deadline — and the arm hangs forever. Found
///   by P6b: `(<!! (timeout 50))` as a whole program, and
///   `l3_task_procs_test::the_kill_switch_keeps_the_mult_on_its_own_os_thread`
///   (where `MOVA_FLOW_THREAD_PROCS=1` means the flow spawns zero tasks and
///   a proc's `(<!! (timeout 2))` never returns). Re-entrancy is not a
///   concern: the only caller that reaches the `get_or_init` arm is a thread
///   that has never touched the runtime, and the sim shard thread cannot be
///   one of those. Real mode never reaches this line at all.
/// - **Unconditional in sim, not "only from a foreign thread".** Telling the
///   shard thread apart from a test thread is possible (`ctx`'s exec marker)
///   but the distinction buys nothing: the surplus case is the shard arming
///   its own timer and leaving a token on itself. `park()`'s token is BINARY,
///   so at most one extra `park()` return follows, and the drain loop's next
///   act is to re-check the three queues and re-run the advance rule — both
///   idempotent. Crucially the trace records *jobs, clock jumps and fires*,
///   never park/unpark, so a surplus token cannot even be observed in the
///   determinism artifact. (Verified empirically: P6a rules 1 and 2 re-run
///   post-bridge produce byte-identical traces to the pre-bridge baseline.)
///
/// Real mode: one `sim_enabled()` load — the same relaxed `OnceLock` peek the
/// arming path already pays for the timer-thread branch — then nothing.
pub(crate) fn sim_unpark_shard() {
    if !crate::clock::sim_enabled() {
        return;
    }
    let Some(shard) = shards().first() else {
        return;
    };
    let th = {
        let g = lock_mutex(&shard.inbox);
        g.thread.clone()
    };
    if let Some(th) = th {
        th.unpark();
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Above this many live (placed, not yet `DONE`) tasks on a shard, a
/// task-spawned child stops trying to co-locate with its parent and falls
/// back to the same round-robin a root spawn always uses. There is no work
/// stealing in v0 (module doc, design §3.2), so unconditional affinity
/// would let a CPU-bound family (a `go` that loops spawning workers) pin
/// itself onto one core forever with no mechanism to spread back out — the
/// guard exists to make that spill instead of serialize. 8 is picked, not
/// derived: comfortably above the coordinator-plus-a-handful-of-workers
/// shape a CSP program typically fans out from one `go`, while still
/// tripping well before a spawn-in-a-loop family monopolizes a shard.
pub const SPAWN_LOCAL_MAX: usize = 8;

/// Pick a shard for a new task (module doc, "Spawn placement"). A task
/// spawning from inside another task tries its own shard first, provided
/// that shard isn't flooded; every other caller — and an over-threshold
/// task — round-robins.
fn pick_shard(shards: &[Arc<Shard>]) -> Arc<Shard> {
    if let Some(local) = with_current_task(|t| t.map(|ts| ts.shard.clone())) {
        if local.load.load(Ordering::Relaxed) < SPAWN_LOCAL_MAX {
            return local;
        }
    }
    let idx = NEXT_SHARD.fetch_add(1, Ordering::Relaxed) % shards.len();
    shards[idx].clone()
}

/// Run `thunk` as a task.
///
/// Placement happens once, at spawn, and is FINAL — the task lives and dies
/// on the shard this call picked (design §3.2). Only the thunk crosses the
/// thread boundary; the coroutine and its stack are built on the shard.
/// Which shard: family-local with a flood guard, see the module doc and
/// [`SPAWN_LOCAL_MAX`] — a spawn made from inside a task prefers the
/// caller's own shard (Go's local-run-queue precedent; collapses the
/// common coordinator/worker rendezvous from a ~2.4 µs cross-shard wake to
/// a same-shard scheduler pass, ~194 ns class, P2-proven), a root spawn
/// from a plain OS thread round-robins same as before.
///
/// Returns nothing: a task's observable result is whatever its body does
/// (for `go` that is a put on the result chan), and handing out a task id
/// would be an ownership claim the runtime does not honor — there is no
/// join and no result. (L4 W3 added a `cancel`, but on a SEPARATE entry
/// point: [`spawn_killable`], so that a `go` block still pays nothing for a
/// handle it never asked for.)
pub fn spawn(thunk: impl FnOnce() + Send + 'static) {
    spawn_to(pick_shard(shards()), thunk);
}

/// Run `thunk` as a task on a CALLER-CHOSEN shard: `idx % shard_count()`,
/// so any `usize` is a legal argument and a caller that counts in "logical
/// lanes" never has to know how many shards this machine has.
///
/// This is [`spawn`] with its one policy decision — [`pick_shard`]'s
/// family-locality heuristic — handed to the caller instead. Everything else
/// is identical, including the part that matters most: placement is still
/// FINAL. There is no work stealing and no migration (module doc, design
/// §3.2), so an explicit index is a promise the runtime keeps exactly, and a
/// caller who places badly gets a serialized shard with no mechanism to
/// recover. Use it only where the caller genuinely knows the topology.
///
/// The known such caller is `builtins::flow`'s segment placement (L3 §3.6):
/// a flow's procs form a chain whose neighbors hand each other every
/// message, so a co-shard hop (a direct scheduler switch) beats a
/// cross-shard one (an inbox push plus an unpark) by roughly an order of
/// magnitude — but a 10k-proc chain placed ENTIRELY on one shard would
/// serialize a pipeline the machine could otherwise run in parallel. The
/// flow side therefore slices the chain into contiguous segments and calls
/// this once per proc with its segment's shard; see `flow::segment_shards`.
///
/// [`SPAWN_LOCAL_MAX`] is not consulted: the flood guard exists to stop an
/// ACCIDENTAL pile-up from an inherited heuristic, and there is nothing
/// accidental about an index the caller computed on purpose.
pub fn spawn_on(shard_idx: usize, thunk: impl FnOnce() + Send + 'static) {
    let shards = shards();
    let shard = shards[shard_idx % shards.len()].clone();
    spawn_to(shard, thunk);
}

/// The body both spawn entry points share: everything after "which shard".
///
/// Kept as one function rather than two so the task-construction sequence
/// below — id, `TaskShared`, the two pre-allocated sudog cells, the counter,
/// the load bump, the inject — can never drift between the two placements.
/// [`spawn`] / [`spawn_on`], but handing the caller a [`TaskHandle`] on the
/// task it just created (L4 W3).
///
/// Separate entry points rather than a return value on the existing two, on
/// purpose: `spawn` is documented as returning nothing and is called once
/// per `go` block, and making every spawn in the process pay an `Arc` bump
/// plus a drop for a handle almost nobody wants is the kind of "free"
/// that shows up in E3. The one caller is `builtins::flow`'s `spawn_run`,
/// which retains the handle for the supervisor's escalation ladder.
///
/// `shard: None` means the runtime's own placement, exactly like [`spawn`];
/// `Some(i)` is [`spawn_on`]'s explicit index.
pub(crate) fn spawn_killable(shard: Option<usize>, thunk: impl FnOnce() + Send + 'static) -> TaskHandle {
    let shards = shards();
    let target = match shard {
        Some(i) => shards[i % shards.len()].clone(),
        None => pick_shard(shards),
    };
    let task = spawn_to(target, thunk);
    TaskHandle { waker: TaskWaker { task } }
}

/// Returns the `TaskShared` it just injected, so [`spawn_killable`] can wrap
/// it in a handle; every other caller drops it, which costs nothing (the
/// `Arc` was already cloned into the inject).
fn spawn_to(shard: Arc<Shard>, thunk: impl FnOnce() + Send + 'static) -> Arc<TaskShared> {
    let id = NEXT_TASK_ID.fetch_add(1, Ordering::Relaxed);
    let task = Arc::new(TaskShared {
        id,
        state: AtomicU8::new(READY),
        shard: shard.clone(),
        // Filled in on the shard thread, at `build_task` — this side of the
        // boundary has no business knowing the shard's slab.
        slot: AtomicU32::new(UNASSIGNED_SLOT),
        // L2 lever 2: the sudog cells are allocated ONCE, here, instead of
        // once per park. Two `Arc::new`s added to spawn (~40 ns against E3's
        // ~1.9 µs steady-state spawn budget) to delete two `Arc::new`s plus
        // two frees from every blocking chan op that parks.
        take_cell: Arc::new(Mutex::new(TakeSlot::Waiting)),
        put_commit: Arc::new(AtomicU8::new(PUT_WAITING)),
    });
    TASKS_SPAWNED.fetch_add(1, Ordering::Relaxed);
    shard.load.fetch_add(1, Ordering::Relaxed);
    shard.inject_spawn(task.clone(), Box::new(thunk));
    task
}

/// The running task's `TaskShared`, or `None` if this thread is not inside a
/// task right now.
///
/// **W2 item 2 — this is a BORROW, and the borrow is why a park costs no
/// atomics.** L1 kept an `Arc<TaskShared>` in a thread-local and
/// [`park_current_yield`] cloned it out and put it back around the suspend:
/// two atomic RMWs on every hop, for a value the shard already owns in the
/// `TaskEntry` it is resuming. The pointer below owns nothing;
/// [`resume_task`] publishes it on switch-in and nulls it on switch-out, and
/// the parked task's stack keeps no copy at all.
///
/// SAFETY / validity argument, in full — this is the riskiest line in W2 and
/// review should read it as such:
///
/// - The pointer is only ever `Arc::as_ptr(&entry.task)` for the `entry:
///   TaskEntry` that [`resume_task`] holds in a LOCAL. That local owns a
///   strong `Arc<TaskShared>` for the whole of `entry.co.resume(())`, so the
///   pointee is alive at every instant the pointer is non-null.
///   `Arc::as_ptr` addresses the heap allocation, so moving the `Arc` HANDLE
///   (into the map, out of it, into the slab) never invalidates it.
/// - The window in which it is non-null is opened and closed by
///   `resume_task`, unconditionally on every path INCLUDING the panic path
///   (the switch-out block sits after the `catch_unwind`, not inside it).
/// - The only code that can run on this thread inside that window is
///   `resume_task` itself and the task's own coroutine, nested inside
///   `co.resume()`. There is no second thread to consider: an `ExecTls`
///   block is thread-local and this field is never handed across one.
/// - Every reader is guarded, directly or by contract, by [`in_task`], and
///   `yielder != 0` holds only inside that same window (the coroutine sets
///   the yielder after `resume_task` set this, and `resume_task` clears the
///   yielder in the same store batch that nulls this). The readers that are
///   not guarded ([`current_shard_index`], [`pick_shard`],
///   [`current_task_is_killed`]) get `None`.
/// - The `&TaskShared` is confined to `f` and cannot escape it, so it cannot
///   outlive the resume that made it valid.
/// - **L4 W3 adds a SECOND publisher with the identical shape**: `kill_task`
///   sets this field from its own `Box<TaskEntry>` local across
///   `co.force_unwind()` and nulls it after, so the argument above holds
///   word for word with "resume" read as "forced unwind". That window is the
///   one in which `yielder == 0` while the pointer is live — which is
///   exactly the F4 law, and why [`current_task_is_killed`] does NOT guard
///   itself with `in_task()`.
#[inline]
fn with_current_task<R>(f: impl FnOnce(Option<&TaskShared>) -> R) -> R {
    ctx::with_exec(|e| {
        let p = e.task.get() as *const TaskShared;
        // SAFETY: see this function's doc comment. `p` is either null, or
        // points into an `Arc<TaskShared>` allocation kept alive by the
        // `resume_task` frame below this one on this same thread.
        f(unsafe { p.as_ref() })
    })
}

/// Consume this thread's runnext slot, if occupied.
///
/// Only [`shard_loop`] calls this, and only from a point where no task is
/// running on this thread — which is why "read, clear" needs no atomicity:
/// the sole writer is task code, and task code is not executing.
#[inline]
fn take_direct_next(shard: &Shard) -> Option<(u64, u32)> {
    ctx::with_exec(|e| {
        let id = e.direct_next.get();
        if id == 0 {
            return None;
        }
        e.direct_next.set(0);
        shard.direct_switches.fetch_add(1, Ordering::Relaxed);
        Some((id, e.direct_slot.get()))
    })
}

/// Is the caller running inside a task?
///
/// `false` on every OS thread in the process except a shard thread that is
/// mid-resume — which is exactly what keeps the task-park arm invisible to
/// `>!!`/`<!!` called from a plain thread.
#[inline]
pub fn in_task() -> bool {
    ctx::with_exec(|e| e.yielder.get() != 0)
}

/// **Is the stack unwinding under us a KILL rather than a panic?** (L4 W3,
/// docs/L4-SUPERVISION-DESIGN.md §3.5; probe findings F4 and S3.)
///
/// THE seam between the runtime's kill and `builtins::flow`'s two
/// unwind-aware behaviours, and it exists because a forced unwind and a
/// panic look identical to a destructor and to a `catch_unwind`:
/// - `ExitGuard::drop` uses it to render the exit reason `:killed` instead
///   of `:panicked` (its `reason` cell is unset either way — any unwind
///   skips the guard's `set`);
/// - the four `catch_unwind` sites around user `transform`/`init` calls use
///   it to RE-RAISE the payload instead of reporting a phantom transform
///   error (finding S3: `force_unwind` re-throws until the coroutine reaches
///   its root, so a stack that catches the payload and then never parks
///   again wedges the shard inside the kill — and a killed proc that
///   "recovered" into its incident handler would report an error nobody
///   caused).
///
/// **Why it works with [`in_task`] `== false`.** `kill_task` switches the
/// dying task's identity IN before `force_unwind` — `ExecTls::task`, `ctx`
/// and the binding locals, exactly as `resume_task` does — and only the
/// YIELDER is unrestorable (its address is a fact only the task's own stack
/// knows). So [`with_current_task`] sees the right task throughout the
/// forced unwind even though `in_task()` is false; this reads its state word
/// and finds [`KILLED`], the in-flight marker the shard replaces with `DONE`
/// only once the unwind has finished.
///
/// `false` on an OS thread (the pointer is null there) and `false` inside an
/// ordinary panic on a task (the state is `RUNNING` there). Non-parking by
/// construction — one atomic load — which is what makes it legal under the
/// F4 law governing destructors on a force-unwound stack.
pub(crate) fn current_task_is_killed() -> bool {
    with_current_task(|t| t.is_some_and(|ts| ts.state.load(Ordering::Acquire) == KILLED))
}

/// A wake handle for the currently-running task. Panics outside a task —
/// every call site is expected to be guarded by [`in_task`].
///
/// This one still bumps the refcount, and deliberately: a `TaskWaker` is
/// handed to a chan's waiter queue, where it outlives the resume that made
/// it, so it must OWN its `TaskShared`. W2 kills the refcount traffic on the
/// per-SWITCH path ([`with_current_task`]), not on the per-registration one.
pub fn current_waker() -> TaskWaker {
    with_current_task(|t| TaskWaker {
        task: unsafe {
            // SAFETY: reconstructing an owning handle from a borrowed
            // `&TaskShared` that we know came from `Arc::as_ptr` on a LIVE
            // `Arc` (see `with_current_task`). `increment_strong_count`
            // publishes the new strong reference before `from_raw` claims
            // it, so the count never dips and the borrow the shard still
            // holds is unaffected.
            let p = t.expect("mova runtime: current_waker() called outside a task")
                as *const TaskShared;
            Arc::increment_strong_count(p);
            Arc::from_raw(p)
        },
    })
}

/// The running task's cached taker cell (L2 lever 2, [`TaskShared`]). Panics
/// outside a task — same contract as [`current_waker`], and the same two call
/// sites' guard (`builtins::async`'s `in_task()`) discharges it.
///
/// **No reset needed, by invariant.** `park_task_taker` returns only after a
/// `mem::replace(.., TakeSlot::Waiting)`, so the cell is `Waiting` on every
/// path out of a take — including the `Closed` one. If that ever stops being
/// true, this accessor is where the re-arm belongs.
///
/// L4 W3 leans on that invariant harder than L2 did: `Waiting` is the
/// RESTING value the kill tombstone is defined against
/// ([`TaskWaker::kill`]), so "the cell is `Waiting` whenever this task is
/// not registered as a taker" is now a correctness property of the kill
/// protocol and not merely an optimization. Its put-side twin is re-armed in
/// `builtins::async`'s `park_task_putter`.
pub(crate) fn current_take_cell() -> Arc<Mutex<TakeSlot>> {
    with_current_task(|t| {
        t.expect("mova runtime: current_take_cell() called outside a task").take_cell.clone()
    })
}

/// The running task's cached put-commit cell (L2 lever 2, [`TaskShared`]).
/// Panics outside a task. **The caller owes it a `PUT_WAITING` store before
/// re-registering** — unlike the taker cell this one is left holding the
/// terminal `PUT_DONE`/`PUT_CLOSED` of the previous put.
///
/// Soundness of reusing a cell across parks (design §3-Q4, and the reason S5
/// is not violated: this changes cell LIFETIME, not one word of who writes
/// what under which lock):
///
/// - A waiter is popped out of `task_putters` BEFORE its `commit` is written,
///   and both happen under the chan lock. So a chan never holds a handle on
///   a cell whose owner has already walked on, and a reused cell has exactly
///   one possible writer at a time.
/// - The re-arm store is made under the NEXT chan's lock, before the new
///   waiter is pushed — i.e. before any deliverer can see the waiter and
///   therefore before any deliverer can write the cell. The window in which
///   "still `PUT_DONE` from last time" could be misread does not exist.
/// - A `wake()` that lands late (its target already read its cell and walked
///   on) is absorbed exactly as it was in L1: it leaves `NOTIFIED`, and
///   `park_task_putter`/`park_task_taker` re-read and re-park. That argument
///   never depended on the cell being fresh.
pub(crate) fn current_put_cell() -> Arc<AtomicU8> {
    with_current_task(|t| {
        t.expect("mova runtime: current_put_cell() called outside a task")
            .put_commit
            .clone()
    })
}

/// Suspend the current task, returning control to its shard.
///
/// The caller MUST have registered a [`TaskWaker`] with whatever will wake
/// it, under that thing's lock, before dropping the lock and calling this
/// (module doc, step 1). Otherwise the wake it is waiting for may already
/// have happened and been discarded.
pub fn park_current_yield() {
    let yielder = ctx::with_exec(|e| e.yielder.get());
    assert!(
        yielder != 0,
        "mova runtime: park_current_yield() called outside a task"
    );
    // W2 item 2: nothing to save. The `TaskShared` handle the L1 park cloned
    // out of a thread-local and put back on resume is the shard's to publish
    // -- `resume_task` sets `ExecTls::task` before every resume, this one
    // included -- so the round trip, and its two atomic RMWs, are gone.

    // SAFETY: `yielder` points into this coroutine's OWN root frame -- a
    // `#[repr(transparent)]` view of the parent link corosensei stores
    // there -- so its address is fixed for the coroutine's whole life, and
    // the only code that dereferences it is code running ON that stack,
    // which is where we are.
    let y = unsafe { &*(yielder as *const Yielder<(), ()>) };
    y.suspend(());

    // Resumed. The shard cleared the yielder when the suspend surfaced and
    // cannot restore it (the yielder address is known only here), so the
    // resumed task restores it from its own stack. Sufficient because every
    // resume of a STARTED task lands on exactly this line -- the one after
    // its suspend. `ExecTls::task` needs no such treatment: the shard knows
    // that one and has already re-published it.
    ctx::with_exec(|e| e.yielder.set(yielder));
}

// ---------------------------------------------------------------------------
// Introspection
// ---------------------------------------------------------------------------

/// Shard threads this process will run (`available_parallelism`). Does not
/// start them.
pub fn shard_count() -> usize {
    shard_n()
}

/// Tasks handed to [`spawn`] since process start.
pub fn tasks_spawned() -> u64 {
    TASKS_SPAWNED.load(Ordering::Relaxed)
}

/// Tasks that ran to completion, panics included.
pub fn tasks_finished() -> u64 {
    TASKS_FINISHED.load(Ordering::Relaxed)
}

/// Tasks whose body panicked and was contained at the resume site.
pub fn tasks_panicked() -> u64 {
    TASKS_PANICKED.load(Ordering::Relaxed)
}

/// Tasks destroyed by [`TaskWaker::kill`]. Counted on the SHARD, after the
/// coroutine has actually force-unwound — so this is "kills completed", not
/// "kills claimed".
pub fn tasks_killed() -> u64 {
    TASKS_KILLED.load(Ordering::Relaxed)
}

/// Killed tasks whose 8 MiB stack was NOT recycled because the forced unwind
/// unwound out of the shard (see [`KILL_STACKS_LEAKED`]).
pub fn kill_stacks_leaked() -> u64 {
    KILL_STACKS_LEAKED.load(Ordering::Relaxed)
}

/// Kills aborted by the commit-cell claim (see [`KILLS_SALVAGED`]).
pub fn kills_salvaged() -> u64 {
    KILLS_SALVAGED.load(Ordering::Relaxed)
}

/// Total resumes = spawns + park/wake round trips: the switch count behind
/// any ns/hop figure. Summed over every shard (W2 item 5) — the counter is
/// per-shard now, but this signature and its meaning are unchanged.
pub fn tasks_resumed() -> u64 {
    SHARDS
        .get()
        .map(|v| v.iter().map(|s| s.resumed.load(Ordering::Relaxed)).sum())
        .unwrap_or(0)
}

/// Resumes served out of a shard's runnext slot instead of its inbox
/// (L2, module doc "Direct switch"), summed over every shard.
///
/// Against [`tasks_resumed`] this is the direct-switch HIT RATE, and it is
/// what tells "the lever is cold" apart from "the lever is hot and simply
/// does not buy much" — a distinction the probe had to make before it could
/// trust its own numbers (docs/L2-PROBE-RESULTS.md §3: 600 000 of 600 003
/// resumes on the E1 shape, i.e. 100%).
pub fn direct_switches() -> u64 {
    SHARDS
        .get()
        .map(|v| v.iter().map(|s| s.direct_switches.load(Ordering::Relaxed)).sum())
        .unwrap_or(0)
}

/// Coroutine stacks this process has `mmap`ed, across every shard's pool.
/// The pooling claim in one number (P1/B2b): this plateaus while
/// [`tasks_spawned`] runs away from it, because a spawn that draws a
/// recycled stack issues no VM syscall at all.
pub fn stacks_mmaped() -> u64 {
    stack_pool::stacks_mmaped()
}

/// The shard index the calling task is running on, or `None` outside a
/// task. Introspection for the W4b placement tests — the direct way to
/// check "did this child land on its parent's shard?" without inferring it
/// from finished-counts.
pub fn current_shard_index() -> Option<usize> {
    with_current_task(|t| t.map(|ts| ts.shard.index))
}

/// Per-shard finished-task counts, shard 0 first. Empty until the shards
/// have been started by a first [`spawn`]. This is how a test asserts that
/// round-robin placement actually used every shard rather than piling a
/// whole run onto one.
pub fn shard_finished_counts() -> Vec<u64> {
    SHARDS
        .get()
        .map(|v| v.iter().map(|s| s.finished.load(Ordering::Relaxed)).collect())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// The shard loop
// ---------------------------------------------------------------------------

/// One task, as its shard sees it. The `Coroutine` is the `!Send` half; the
/// `ctx`/`locals` pair is the identity the shard swaps in and out around
/// every resume (module doc).
struct TaskEntry {
    co: TaskCoroutine,
    task: Arc<TaskShared>,
    ctx: u64,
    locals: BindingLocals,
}

enum Job {
    Spawn(Arc<TaskShared>, Body),
    /// `(id, slab slot)` — see [`Inbox::ready`].
    Run(u64, u32),
    /// L4 W3: destroy this task without resuming it (§3.5).
    Kill(u64, u32),
}

// ---------------------------------------------------------------------------
// The shard's task slab (W2 item 3)
// ---------------------------------------------------------------------------

/// A slot index no task holds: what [`TaskShared::slot`] reads between
/// [`spawn`] and the shard's `build_task`. A wake cannot legally observe it
/// (a waker only exists because the task ran, and a task only runs after
/// `build_task` assigned it a real slot), but [`Slab::take`] rejects it
/// anyway rather than index out of bounds on a future mistake.
const UNASSIGNED_SLOT: u32 = u32::MAX;

/// The shard's live tasks, indexed by slot. Private to the shard thread that
/// owns it — nothing here is ever touched from another thread.
///
/// **This replaces the `HashMap<u64, TaskEntry>` L1 kept, and with it the L2
/// identity hasher that map needed** (W1 lever 3, now deleted along with the
/// map). A resume used to pay a hashbrown `remove` probe, a `TaskEntry` move,
/// and a hashbrown `insert` probe; the profile of the W2 baseline put
/// `hashbrown::insert` alone at 6.9% of shard-thread samples. A slot index
/// makes both probes a bounds check, and boxing the entry makes both moves a
/// pointer store — the entry is ~14 words (a `Coroutine`, an `Arc`, a `u64`,
/// and `BindingLocals`' two `Vec`s), and it was being memcpy'd twice per hop.
/// The `Box` costs one allocation per TASK against a ~1.9 µs spawn budget
/// that already `mmap`s a stack.
///
/// The ids still ride the queues, and they still decide: see [`take`](Self::take).
///
/// This is the SAFE half of docs/L2-PROBE-RESULTS.md §6 item 3. The full
/// Go shape — queues carrying the entry itself, so the runnext slot is a true
/// hand-off — needs `wake()` to reach the shard's own queues mid-resume, and
/// is deferred to L2.5 together with replace-and-evict (design §6-V1).
#[derive(Default)]
struct Slab {
    /// `None` = free. Never shrinks: it is sized by a shard's high-water live
    /// count, exactly like the `HashMap` it replaces.
    entries: Vec<Option<Box<TaskEntry>>>,
    /// Slots whose task reached `DONE`, newest first (LIFO keeps the reused
    /// slot the cache-warmest one).
    free: Vec<u32>,
}

impl Slab {
    /// Reserve a slot for a task about to be built.
    fn alloc(&mut self) -> u32 {
        if let Some(slot) = self.free.pop() {
            return slot;
        }
        let slot = self.entries.len();
        // One shard holding 4 billion live tasks would need ~32 TiB of stack
        // reservation, so this is not a scenario — but a silent `as u32`
        // truncation here would alias two live tasks onto one slot, which is
        // the one failure this whole design must not have.
        assert!(
            slot < UNASSIGNED_SLOT as usize,
            "mova runtime: shard slab exhausted (4 billion live tasks on one shard)"
        );
        self.entries.push(None);
        slot as u32
    }

    /// Take the entry for `(id, slot)` out of the slab, or `None` if this
    /// wake is stale.
    ///
    /// **The ABA guard.** Slots are reused; ids never are. So a wake that
    /// names a slot whose occupant has changed — or emptied — must be a
    /// no-op, never a stranger's resume. Three ways this returns `None`, and
    /// all three are ordinary rather than exceptional:
    ///
    /// - out of range, or [`UNASSIGNED_SLOT`]: not a live slot at all;
    /// - empty: the task finished (a stale wake for a completed task), or it
    ///   is running right now (impossible on one shard thread, but the check
    ///   costs nothing);
    /// - occupied by a DIFFERENT id: the slot was freed at `DONE` and handed
    ///   to a later task.
    ///
    /// The task-state CAS in [`TaskWaker::wake`] already refuses to enqueue a
    /// `DONE` task, so in the runtime as it stands the third case is
    /// unreachable. It is checked anyway, and tested
    /// (`tests/l2_direct_switch_test.rs`), because "unreachable given the
    /// current state machine" is exactly the kind of premise a later change
    /// invalidates quietly.
    #[inline]
    fn take(&mut self, id: u64, slot: u32) -> Option<Box<TaskEntry>> {
        let cell = self.entries.get_mut(slot as usize)?;
        if cell.as_ref().is_some_and(|e| e.task.id == id) {
            cell.take()
        } else {
            None
        }
    }

    /// Put a running task's entry back. The slot is still reserved to it —
    /// nothing else can have claimed it, because only `DONE` frees a slot and
    /// only this thread runs tasks.
    #[inline]
    fn put(&mut self, slot: u32, entry: Box<TaskEntry>) {
        self.entries[slot as usize] = Some(entry);
    }

    /// Release a `DONE` task's slot for reuse.
    fn release(&mut self, slot: u32) {
        self.free.push(slot);
    }

    /// L5/W4 — how many tasks this shard is holding right now (placed, not
    /// yet terminal). O(high-water slots), and called only from the sim
    /// scheduler's quiescence point, which is reached once per park.
    ///
    /// `shard.load` is deliberately NOT the answer: it counts a task from the
    /// instant `spawn_to` injects it, i.e. including entries still sitting in
    /// the inbox with no slab slot and no coroutine. The teardown needs the
    /// set it can actually kill, which is exactly this one.
    fn live(&self) -> usize {
        self.entries.iter().filter(|e| e.is_some()).count()
    }
}

// ---------------------------------------------------------------------------
// The fairness turn (L2 lever 1's liveness half)
// ---------------------------------------------------------------------------

/// Consecutive resumes a shard may serve out of its runnext slot before it
/// owes `local`/inbox one turn (module doc, "Direct switch").
///
/// **This is a liveness bound, not a fairness tuning knob** — see the module
/// doc. Measured at 16 / 64 / 256 on two chatty pairs plus a third task, one
/// shard (docs/L2-PROBE-RESULTS.md §4):
///
/// | BUDGET | third task's 10 000 turns done at | pairs' ns/hop |
/// |---|---|---|
/// | 16 | **10.6–10.9 ms** | 64.5–65.9 |
/// | 64 | 25.8–26.3 ms | 64.1–64.6 |
/// | 256 | 26.1–26.3 ms | 64.3–64.8 |
/// | ≈ none | 50.0 ms (= end of run) | 62.5 |
///
/// 16 is free — the hop cost is flat across every setting, because the streak
/// is a register compare and the one inbox peek amortizes over the whole
/// budget — and it buys a 2.4x better worst-case latency for a co-located
/// task than 64 does. Above ~64 the number stops binding at all: a putter
/// that parks with no taker queued wakes nobody, so the slot empties and the
/// streak resets on its own long before 256.
const DIRECT_SWITCH_BUDGET: u32 = 16;

/// The fairness turn's non-blocking inbox look: one inbox mutex pair,
/// amortized over [`DIRECT_SWITCH_BUDGET`] direct switches. Spawns first,
/// same order the blocking drain below uses — and the spawns are the whole
/// point (module doc: a saturated slot starves unstarted `go` blocks, not
/// merely runnable ones).
fn try_pop_inbox(shard: &Shard) -> Option<Job> {
    let mut g = lock_mutex(&shard.inbox);
    // L4 W3: kills before everything else. Not for the race's sake (the CAS
    // in `TaskWaker::kill` has already decided every race by the time an
    // order is in this queue) but because a claimed kill is the one job
    // that frees resources instead of consuming them.
    if let Some((id, slot)) = g.kills.pop_front() {
        return Some(Job::Kill(id, slot));
    }
    if let Some((task, body)) = g.spawns.pop_front() {
        return Some(Job::Spawn(task, body));
    }
    g.ready.pop_front().map(|(id, slot)| Job::Run(id, slot))
}

/// L5 / P6a — the sim scheduler's ONE choice point (design §3, "One choice
/// point: `next_job()`"). Replaces the whole real-mode drain cascade when
/// `MOVA_SIM_SEED` is set; real mode never reaches it.
///
/// Three rules, in order:
///
/// 1. **Kills first, FIFO, never randomized.** This is a RESOURCE rule, not
///    a scheduling one: a claimed kill frees a stack instead of consuming
///    one, and if kill-delivery timing coupled to the seed then the
///    supervisor's escalation ladder would report seed-dependent attempt
///    counts. (`take_direct_next` is not consulted at all — sim forces
///    `direct_switch_disabled()`, so the runnext slot is provably empty.)
/// 2. **Pool `local ∪ spawns ∪ ready`, pick uniformly by the SCHEDULE
///    stream.** Pooling is what makes "different seed → different
///    interleaving" true: FIFO at one shard would be deterministic but
///    single-schedule, and a single-schedule simulator explores nothing.
///    The index space is `local`, then `spawns`, then `ready` — an order
///    that is a pure function of the three queue lengths, so the draw maps
///    to a candidate identically on every run.
/// 3. **Nothing runnable → advance virtual time** (design §2). The heap is
///    drained inline here, on this thread, with the heap lock released
///    before any firing (R5). If it fires nothing, the world really is
///    quiescent and we `park()` exactly as real mode does — the only thing
///    that can wake us then is the boundary thread's root spawn (R6).
///
/// O(n) `VecDeque::remove` is deliberate: n is the runnable set of one shard,
/// and a simulator trades constant factors for a legible choice point.
fn sim_next_job(shard: &Shard, local: &mut VecDeque<(u64, u32)>, slab: &Slab) -> Job {
    loop {
        // L5/W4 step 0 — the `:max-resumes` tripwire. One relaxed load in the
        // common case (`RESUME_TRIP == u64::MAX`); real mode never gets here.
        if crate::builtins::sim::budget_blown() {
            crate::builtins::sim::begin_teardown(crate::builtins::sim::StopReason::MaxResumes);
        }
        // L5/W4 step 0b — while the world is being destroyed, claim a kill on
        // every task still alive, in SLAB SLOT ORDER (a function of the
        // schedule, never of a race). Reuses the L4 protocol verbatim: the
        // `PARKED -> KILLED` CAS plus the commit-cell claims, then a
        // `Job::Kill` this very loop dispatches. A task that REFUSES (P5a-bis
        // salvage: a peer's commit had already landed) is left runnable on
        // purpose — it is let run to its next park below and killed on the
        // next pass, which is the existing retry pattern and not a new one.
        let tearing_down = crate::builtins::sim::tearing_down();
        if tearing_down {
            sim_claim_kills(slab);
        }
        {
            let mut g = lock_mutex(&shard.inbox);
            if let Some((id, slot)) = g.kills.pop_front() {
                return Job::Kill(id, slot);
            }
            if tearing_down {
                // Never-started spawns are dropped rather than built: the
                // world is ending, and building a coroutine only to unwind it
                // is strictly more work with the identical outcome. They still
                // count as leaked tasks.
                let dropped = g.spawns.len();
                if dropped > 0 {
                    g.spawns.clear();
                    shard.load.fetch_sub(dropped, Ordering::Relaxed);
                }
                let next = local.pop_front().or_else(|| g.ready.pop_front());
                drop(g);
                if dropped > 0 {
                    crate::builtins::sim::note_leaked(dropped as u64);
                }
                if let Some((id, slot)) = next {
                    if crate::builtins::sim::spend_teardown_dispatch() {
                        return Job::Run(id, slot);
                    }
                    crate::builtins::sim::finish(Some(format!(
                        "the teardown could not destroy the simulated world within its dispatch \
                         budget (task {id} is still alive and refuses to die). This is a runtime \
                         bug, not a program bug: report it with the seed."
                    )));
                    std::thread::park();
                    continue;
                }
                // Nothing queued and nothing left to kill: the world is gone.
                let live = slab.live();
                if live == 0 {
                    // Anything still armed was armed by a task we just
                    // destroyed — see `sim_drop_armed_timers` for why it is
                    // dropped rather than fired.
                    crate::builtins::r#async::sim_drop_armed_timers();
                    crate::builtins::sim::finish(None);
                } else {
                    // Unreachable by the state machine: a live task that is
                    // neither queued nor `PARKED` would have to be `RUNNING`
                    // (nothing is — we are between dispatches) or `KILLED`
                    // (its `Job::Kill` is popped at the top of this loop).
                    // Reported rather than parked, because parking here would
                    // be the hang this whole wave exists to abolish.
                    crate::builtins::sim::finish(Some(format!(
                        "{live} task(s) survived the teardown while being neither runnable nor \
                         parked — the runtime's task-state machine is in a state the teardown \
                         cannot name. This is a runtime bug; report it with the seed."
                    )));
                }
                std::thread::park();
                continue;
            }
            let n = local.len() + g.spawns.len() + g.ready.len();
            if n > 0 {
                let pick = (crate::clock::sched_next() % n as u64) as usize;
                if pick < local.len() {
                    let (id, slot) = local.remove(pick).expect("pick < local.len()");
                    return Job::Run(id, slot);
                }
                let pick = pick - local.len();
                if pick < g.spawns.len() {
                    let (task, body) = g.spawns.remove(pick).expect("pick < spawns.len()");
                    return Job::Spawn(task, body);
                }
                let pick = pick - g.spawns.len();
                let (id, slot) = g.ready.remove(pick).expect("pick < ready.len()");
                return Job::Run(id, slot);
            }
            // Every queue empty. Drop the inbox lock BEFORE the advance rule:
            // firing a timer entry reaches `chan_close` and `TaskWaker::wake`,
            // and `wake()` takes this very mutex.
        }
        if crate::builtins::r#async::sim_advance_and_fire() {
            continue;
        }
        // L5/W4 — THE quiescence point (design §6). Queues empty, heap empty
        // after the cancelled-skip: at one shard this is the definition of
        // "the world has stopped". Either a `simulate` call is waiting on
        // exactly this instant, or nobody is and we park as before.
        match crate::builtins::sim::on_quiescent(slab.live()) {
            crate::builtins::sim::Quiesce::Teardown(reason) => {
                crate::builtins::sim::begin_teardown(reason);
                continue;
            }
            crate::builtins::sim::Quiesce::Park => {}
        }
        // Park on the same discipline real mode uses: a push that raced the
        // emptiness check left an `unpark` token behind (see
        // `Shard::inject_ready`), so this cannot sleep through work.
        std::thread::park();
    }
}

/// L5/W4 — claim a kill on every task still alive on this shard, in slab slot
/// order. Called only from the teardown pass of [`sim_next_job`], only on the
/// shard's own thread, and only while nothing is running (so every live task
/// is `PARKED` or queued).
///
/// `&Slab` rather than `&mut Slab`: this issues ORDERS, it does not execute
/// them. Execution is `Job::Kill` -> [`kill_task`], on the same thread, one
/// dispatch later — the ordinary L4 path with no shortcut around it.
fn sim_claim_kills(slab: &Slab) {
    let mut claimed = 0u64;
    for cell in slab.entries.iter() {
        let Some(entry) = cell else { continue };
        // A task that is queued rather than parked cannot be killed (the CAS
        // is `PARKED -> KILLED` and that is the whole race protocol). It is
        // about to be dispatched, will run to its next park, and is killed on
        // the pass after that.
        if entry.task.state.load(Ordering::Acquire) != PARKED {
            continue;
        }
        let waker = TaskWaker {
            task: entry.task.clone(),
        };
        if waker.kill() {
            claimed += 1;
        }
    }
    if claimed > 0 {
        crate::builtins::sim::note_leaked(claimed);
    }
}

fn shard_loop(shard: Arc<Shard>) {
    let mut slab = Slab::default();
    let mut pool = StackPool::new();
    // Re-queues this shard makes for itself (the NOTIFIED arm). No lock: it
    // never leaves this thread, so a wake that raced a park costs a
    // `VecDeque` push instead of the inbox mutex plus a self-`unpark`.
    let mut local: VecDeque<(u64, u32)> = VecDeque::new();

    // Publish the handle BEFORE the first inbox look, so that any push that
    // beat us here is one we are about to see, and any push that did not
    // finds a handle to unpark.
    lock_mutex(&shard.inbox).thread = Some(std::thread::current());
    // L2: publish this thread's shard identity for `TaskWaker::wake`. Set
    // once, never cleared -- a shard thread is never joined and never runs
    // anything but this loop.
    ctx::with_exec(|e| e.shard.set(Arc::as_ptr(&shard) as usize));

    // L2: consecutive resumes served out of the runnext slot.
    let mut direct_streak: u32 = 0;

    // L5 / P6a: the mode check, hoisted OUT of the loop -- one bool per SHARD
    // THREAD, not one per hop. Real mode's cascade below is then the exact
    // statements it was before, in the exact order, inside an `else`.
    let sim = crate::clock::sim_enabled();
    // L5/W4: `traced` stays a hoisted register in REAL mode (where it is
    // false for the thread's whole life, because nothing can ever open a
    // trace), and is re-read once per hop in SIM — `simulate`'s `:trace` opt
    // opens and closes a file around ONE call, so a per-thread latch would
    // miss every call after the first. Real mode pays nothing: the re-read
    // sits inside the `if sim` arm.
    //
    // The re-read is AFTER `sim_next_job`, not before, and that is not a
    // detail: `sim_next_job` is where this thread PARKS, and the boundary
    // thread opens the call's trace while it is parked. Read beforehand, the
    // root's own `S` and first `R` would be emitted against the previous
    // call's answer and the file would start mid-run.
    let mut traced = crate::clock::trace_on();

    loop {
        let job = if sim {
            let job = sim_next_job(&shard, &mut local, &slab);
            traced = crate::clock::trace_on();
            job
        } else {
        // Consumption order (module doc, "Direct switch"): runnext -> local
        // -> inbox -> sleep, with a FAIRNESS TURN every
        // `DIRECT_SWITCH_BUDGET` consecutive direct switches that services
        // one local/inbox job first. Without that turn a ping-pong pair
        // refills the slot on every hop and this loop never looks at `local`
        // or the inbox again -- which strands unstarted `Job::Spawn`s, so the
        // turn is a liveness requirement and not a nicety.
        //
        // The sleep arm stays unreachable while the slot is occupied: it sits
        // inside the inbox branch, which is entered only after
        // `take_direct_next` came back empty, and nothing can refill the slot
        // while this thread is down there -- only a `wake()` running ON this
        // thread writes it, and this thread is not running task code.
        let mut turn: Option<Job> = None;
        if direct_streak >= DIRECT_SWITCH_BUDGET {
            // Owed turn. Reset unconditionally: if local and the inbox are
            // both empty there is nothing to be fair TO, and charging the
            // peek again on the next hop would just re-lock the inbox.
            direct_streak = 0;
            turn = match local.pop_front() {
                Some((id, slot)) => Some(Job::Run(id, slot)),
                None => try_pop_inbox(&shard),
            };
        }
        if let Some(job) = turn {
            job
        } else if let Some((id, slot)) = take_direct_next(&shard) {
            direct_streak += 1;
            Job::Run(id, slot)
        } else {
            // Slot empty: the chain broke on its own (a park that woke
            // nobody), so the streak has nothing left to bound.
            direct_streak = 0;
            // Local before the inbox: a self-re-queued task (the NOTIFIED
            // arm) already has work and is the most cache-warm thing on this
            // shard. It cannot starve the inbox, because `local` only ever
            // grows through a task that ran -- and was therefore popped --
            // first.
            if let Some((id, slot)) = local.pop_front() {
                Job::Run(id, slot)
            } else {
                let mut g = lock_mutex(&shard.inbox);
                loop {
                    // L4 W3: same priority, and the same lock, as
                    // `try_pop_inbox` — see the comment there. Being INSIDE
                    // this loop is what makes a kill that raced the
                    // emptiness check impossible to sleep through.
                    if let Some((id, slot)) = g.kills.pop_front() {
                        break Job::Kill(id, slot);
                    }
                    if let Some((task, body)) = g.spawns.pop_front() {
                        break Job::Spawn(task, body);
                    }
                    if let Some((id, slot)) = g.ready.pop_front() {
                        break Job::Run(id, slot);
                    }
                    // Nothing runnable: genuinely sleep. An idle shard must
                    // burn no CPU -- the acceptance bar
                    // FLOW-IDLE-CPU-BUG.md set for the flow engine, and the
                    // reason this is `park()` and not a poll. A spurious
                    // wake (an `unpark` token left by work we already
                    // drained) just re-runs this check.
                    drop(g);
                    std::thread::park();
                    g = lock_mutex(&shard.inbox);
                }
            }
        }
        };

        let (id, slot) = match job {
            Job::Spawn(task, body) => {
                let id = task.id;
                // Assign the slot BEFORE the task can run, so no waker can
                // ever read `UNASSIGNED_SLOT` off a task that has produced
                // one (a waker exists only because the task ran).
                let slot = slab.alloc();
                task.slot.store(slot, Ordering::Relaxed);
                let entry = build_task(&mut pool, task, body);
                slab.put(slot, entry);
                if traced {
                    crate::clock::trace_task('S', id);
                }
                (id, slot)
            }
            Job::Run(id, slot) => (id, slot),
            Job::Kill(id, slot) => {
                kill_task(&shard, &mut slab, &mut pool, id, slot);
                if traced {
                    crate::clock::trace_task('X', id);
                }
                continue;
            }
        };
        if traced {
            crate::clock::trace_task('R', id);
        }
        resume_task(&shard, &mut slab, &mut pool, &mut local, id, slot, traced);
    }
}

/// Turn a `Send` thunk into a live (not yet started) coroutine on this
/// shard's stack pool, plus the fresh identity the task will run under.
fn build_task(pool: &mut StackPool, task: Arc<TaskShared>, body: Body) -> Box<TaskEntry> {
    let stack = pool.take();
    // The body publishes only the yielder: its address is a fact about THIS
    // stack that nothing outside it can know. `ExecTls::task` is the shard's
    // to publish, and `resume_task` has already done it by the time this
    // runs -- which is also why the closure no longer needs to capture a
    // second `Arc<TaskShared>` (W2 item 2).
    let co = Coroutine::with_stack(stack, move |yielder: &Yielder<(), ()>, _input: ()| {
        ctx::with_exec(|e| e.yielder.set(yielder as *const Yielder<(), ()> as usize));
        body();
    });
    Box::new(TaskEntry {
        co,
        task,
        // Design §3.4: a task's dynamic bindings hang off an id of its own,
        // never its shard thread's, and it starts with an empty binding
        // stack exactly like a freshly spawned OS thread does today.
        ctx: ctx::fresh_ctx(),
        locals: BindingLocals::default(),
    })
}

/// Resume one task to its next suspension point.
///
/// Taken OUT of the slab for the duration, so the `&mut Coroutine` borrow
/// does not fight the re-insert — and so that a RUNNING task is, by
/// construction, absent from the slab. That is the structural half of why a
/// waker can never enqueue a running task (it leaves `NOTIFIED` instead).
fn resume_task(
    shard: &Arc<Shard>,
    slab: &mut Slab,
    pool: &mut StackPool,
    local: &mut VecDeque<(u64, u32)>,
    id: u64,
    slot: u32,
    // L5 / P6a: the sim trace switch, resolved ONCE per shard thread and
    // passed down as a register rather than re-read here. False in real
    // mode, always, so the three emit sites below fold to nothing.
    traced: bool,
) {
    // Absent, or occupied by a different id = already running (impossible:
    // one shard thread), already finished (a stale wake for a task that
    // completed), or a slot that has since been reused. All no-ops — see
    // `Slab::take`, which is where the ABA argument is written.
    let Some(mut entry) = slab.take(id, slot) else {
        return;
    };
    entry.task.state.store(RUNNING, Ordering::Release);
    shard.resumed.fetch_add(1, Ordering::Relaxed);

    // --- switch IN: the task's identity becomes this thread's identity.
    // Two thread-local accesses total (the block, then `env`'s binding pair),
    // where L1 paid six — see the module doc's "Identity" section.
    //
    // Publishing `e.task` here, rather than letting the task carry its own
    // handle across the suspend, is W2 item 2 — the borrow, its lifetime and
    // its validity argument are documented at [`with_current_task`]. It is
    // paired with the unconditional null store in the switch-OUT block
    // below, and the two together are the whole extent of the pointer's
    // validity window.
    let outer_ctx = ctx::with_exec(|e| {
        e.task.set(Arc::as_ptr(&entry.task) as *const ());
        e.ctx.replace(entry.ctx)
    });
    let outer_locals = ctx::swap_binding_locals(std::mem::take(&mut entry.locals));

    // R3 / P1-B5: corosensei catches a panic at the coroutine's root
    // (`catch_unwind_at_root`, a plain `catch_unwind` running on the
    // coroutine's OWN stack) and re-throws it here, at the resume site. So a
    // panicking task unwinds its own stack, drops what it owned, and arrives
    // as an ordinary `Err` payload -- no abort, and no way for it to take
    // this shard down. Any chan mutex it held mid-mutation is left poisoned,
    // which `sync.rs`'s documented recover-anyway policy already absorbs.
    let res = std::panic::catch_unwind(AssertUnwindSafe(|| entry.co.resume(())));

    // --- switch OUT. Unconditional, panic path included: a task that dies
    // holding binding frames must not leave its `ACTIVE_BINDINGS` behind on
    // the shard thread for the next task to inherit.
    entry.locals = ctx::swap_binding_locals(outer_locals);
    ctx::with_exec(|e| {
        e.ctx.set(outer_ctx);
        e.yielder.set(0);
        e.task.set(std::ptr::null());
    });

    match res {
        Ok(CoroutineResult::Yield(())) => {
            if entry
                .task
                .state
                .compare_exchange(RUNNING, PARKED, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                slab.put(slot, entry);
                if traced {
                    crate::clock::trace_task('P', id);
                }
            } else {
                // NOTIFIED: a wake landed between the waker's registration
                // and this suspend. Re-queue instead of parking -- the whole
                // missed-wakeup argument, in three lines.
                entry.task.state.store(READY, Ordering::Release);
                slab.put(slot, entry);
                local.push_back((id, slot));
            }
        }
        Ok(CoroutineResult::Return(())) => {
            entry.task.state.store(DONE, Ordering::Release);
            TASKS_FINISHED.fetch_add(1, Ordering::Relaxed);
            shard.finished.fetch_add(1, Ordering::Relaxed);
            // W4b: this shard now has one less live task for `pick_shard`
            // to weigh.
            shard.load.fetch_sub(1, Ordering::Relaxed);
            // The entry is already OUT of the slab (`Slab::take`), so the
            // slot is empty; hand it to the free list. Any wake still in
            // flight for this id now fails the id check — see `Slab::take`.
            slab.release(slot);
            pool.recycle(entry.co.into_stack());
            if traced {
                crate::clock::trace_task('X', id);
            }
        }
        Err(payload) => {
            entry.task.state.store(DONE, Ordering::Release);
            TASKS_PANICKED.fetch_add(1, Ordering::Relaxed);
            TASKS_FINISHED.fetch_add(1, Ordering::Relaxed);
            shard.finished.fetch_add(1, Ordering::Relaxed);
            shard.load.fetch_sub(1, Ordering::Relaxed);
            // Same posture `go*` takes for an escaping Mova error today
            // (async.rs, the `Err(e)` arm): there is no supervisor and no
            // error channel in v0, so render to stderr -- the way an
            // uncaught error escaping the REPL renders -- and carry on. A
            // Rust panic payload is a `Box<dyn Any>`, so "render" is
            // whatever string is actually in it.
            let msg = payload
                .downcast_ref::<&'static str>()
                .copied()
                .or_else(|| payload.downcast_ref::<String>().map(|s| s.as_str()))
                .unwrap_or("<non-string panic payload>");
            eprintln!(
                "mova: task {id} panicked on shard {}: {msg} (shard continues)",
                shard.index
            );
            // Recycling a PANICKED task's stack is sound, and this is the
            // check the spec asked for: corosensei's `catch_unwind_at_root`
            // is `panic::catch_unwind` executing on the coroutine's own
            // stack (`unwind.rs`, `unwind` feature -- on by default and a
            // default feature of the dependency), so the panic has already
            // unwound every frame on that stack and run every destructor
            // before the payload was stashed. `resume_inner` then sets
            // `stack_ptr = None`, which is exactly the `done()` that
            // `into_stack()` asserts on. The stack that comes back is
            // therefore live-object-free, in the same state a normal return
            // leaves it. No munmap-the-panicked-stack special case is
            // needed.
            slab.release(slot);
            pool.recycle(entry.co.into_stack());
            if traced {
                crate::clock::trace_task('X', id);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// P5a probe: the kill primitive (docs/L4-SUPERVISION-DESIGN.md §3.5)
// ---------------------------------------------------------------------------

/// Destroy a task that lost the `PARKED -> KILLED` CAS to a supervisor,
/// WITHOUT resuming it into user code.
///
/// Runs on the task's own shard thread, from the `Job::Kill` arm, for the
/// same reason every other `TaskEntry` touch does: the `Coroutine` is `!Send`
/// and its stack belongs to this shard's pool.
///
/// The bookkeeping is deliberately the PANIC arm of [`resume_task`], copied
/// posture for posture — state store, `TASKS_FINISHED`, `shard.finished`,
/// `shard.load` decrement, `slab.release`, `pool.recycle` — because a killed
/// task and a panicked task leave the runtime in exactly the same shape: an
/// entry taken out of the slab whose coroutine has unwound to its root.
///
/// ## What `force_unwind` actually does (corosensei 0.3.4, `coroutine.rs`)
///
/// It RESUMES the coroutine at its `Yielder::suspend` with an
/// `Err(ForcedUnwind)`, which `suspend` turns into a `resume_unwind` of a
/// private `ForcedUnwind` payload carrying the coroutine's initial stack
/// pointer. So the stack unwinds from the park point outwards, running every
/// destructor, and the crate's own `catch_forced_unwind` at the root
/// swallows the payload if (and only if) it is the one it minted. On return
/// `stack_ptr == None`, i.e. `done()`, which is precisely `into_stack()`'s
/// assertion — the stack comes back live-object-free and pooled, no
/// munmap-the-killed-stack special case, the identical argument the panic arm
/// already makes.
///
/// `force_unwind` also LOOPS: if the unwinding coroutine suspends again
/// (something on the stack caught the payload and parked), it re-throws until
/// the coroutine really reaches its root.
///
/// ## Two sharp edges this arm handles, and one it cannot
///
/// 1. **The unwind can escape.** A destructor that panics inside the forced
///    unwind is a double panic and aborts (Rust's rule, and §3.5 accepts it);
///    but a destructor that panics *outside* it, or a foreign panic
///    `force_unwind` chooses to `resume_unwind`, arrives here as an ordinary
///    unwind through `entry.co.force_unwind()`. Catching it is what keeps a
///    kill from taking the shard down — the same reason `resume_task` wraps
///    its resume.
/// 2. **The stack may not be recoverable.** `into_stack()` asserts `done()`,
///    so on the escape path it is only called when the coroutine really did
///    terminate. When it did not, the entry is `mem::forget`-ed and
///    [`KILL_STACKS_LEAKED`] counts the 8 MiB — dropping it instead would
///    re-enter `Coroutine::drop`, which force-unwinds AGAIN and aborts via
///    its `scopeguard` if that unwind escapes too.
/// 3. **`ExecTls::yielder` is already zero here** — `resume_task` cleared it
///    at the park this task never came back from, and the address is a fact
///    only the task's own stack knows, so the shard cannot restore it. Every
///    destructor on a force-unwound stack therefore runs with
///    [`in_task`] `== false`. This is the probe's F4 law: such a destructor
///    may use NON-PARKING operations only, because a blocking chan op from
///    there would take the OS-thread condvar path ON THIS SHARD THREAD and
///    wedge the shard. `builtins::flow`'s `ExitGuard::drop` is the one such
///    destructor in the tree and its doc discharges the law op by op.
///    Everything else the dying task's identity needs IS switched in (see
///    the block below), which is what lets [`current_task_is_killed`] tell
///    this unwind apart from a panic.
fn kill_task(shard: &Arc<Shard>, slab: &mut Slab, pool: &mut StackPool, id: u64, slot: u32) {
    // The same three-`None` ABA guard `Job::Run` gets, and for the same
    // reasons (`Slab::take`). A kill order cannot in fact go stale in the
    // runtime as it stands — the `PARKED -> KILLED` CAS is claimed exactly
    // once and no `Job::Run` for this id can be queued after it, because
    // `wake()` refuses a non-PARKED task — but the id check is the authority
    // and stays the authority.
    let Some(mut entry) = slab.take(id, slot) else {
        return;
    };

    // Switch IN exactly as `resume_task` does: the destructors about to run
    // are the DYING TASK's, so they must see the dying task's ctx id and
    // binding frames, not whatever ran here last. Unconditionally switched
    // back below, escape path included.
    let outer_ctx = ctx::with_exec(|e| {
        e.task.set(Arc::as_ptr(&entry.task) as *const ());
        e.ctx.replace(entry.ctx)
    });
    let outer_locals = ctx::swap_binding_locals(std::mem::take(&mut entry.locals));

    let escaped = std::panic::catch_unwind(AssertUnwindSafe(|| entry.co.force_unwind())).err();

    entry.locals = ctx::swap_binding_locals(outer_locals);
    ctx::with_exec(|e| {
        e.ctx.set(outer_ctx);
        e.yielder.set(0);
        e.task.set(std::ptr::null());
    });

    // KILLED was the in-flight marker; DONE is the terminal state, so the
    // runtime keeps ONE vocabulary for "this id will never run again" and
    // every existing reader (`wake()`, `Slab::take`) is unchanged.
    entry.task.state.store(DONE, Ordering::Release);
    TASKS_KILLED.fetch_add(1, Ordering::Relaxed);
    TASKS_FINISHED.fetch_add(1, Ordering::Relaxed);
    shard.finished.fetch_add(1, Ordering::Relaxed);
    shard.load.fetch_sub(1, Ordering::Relaxed);
    slab.release(slot);

    match escaped {
        None => pool.recycle(entry.co.into_stack()),
        Some(payload) => {
            let msg = payload
                .downcast_ref::<&'static str>()
                .copied()
                .or_else(|| payload.downcast_ref::<String>().map(|s| s.as_str()))
                .unwrap_or("<non-string panic payload>");
            eprintln!(
                "mova: forced unwind of killed task {id} on shard {} escaped: {msg} (shard continues)",
                shard.index
            );
            if entry.co.done() {
                pool.recycle(entry.co.into_stack());
            } else {
                KILL_STACKS_LEAKED.fetch_add(1, Ordering::Relaxed);
                std::mem::forget(entry);
            }
        }
    }
}
