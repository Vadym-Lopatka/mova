//! Execution-context identity: the one thing dynamic `binding` frames are
//! keyed on (docs/L1-TASK-RUNTIME-DESIGN.md §3.4, L1-LANDING-SPEC §W1).
//!
//! Thread identity WAS that key. It stops being usable the moment `go`
//! blocks become tasks multiplexed M:1 onto shard threads: two tasks resumed
//! on one OS thread would share a single `ThreadId` and therefore a single
//! binding stack per var -- silent semantic corruption, not a performance
//! problem. So the key becomes a context id that the scheduler owns: a plain
//! thread mints one lazily and keeps it forever, while a shard thread
//! rewrites it on every switch-in ([`set_ctx`]/[`swap_ctx`]) so the running
//! task's id is what `env.rs` sees.
//!
//! With no tasks in the process this is exactly the old behavior: one
//! thread, one id, assigned once, stable for the thread's life -- a
//! `Cell<u64>` load in place of a `ThreadId` load on the same paths.
//!
//! Binding state is TWO things and only one of them lives in the cell above:
//! the per-var stacks (`VarCell::dyn_bindings`, keyed by ctx id) and the
//! per-context conveyance bookkeeping (`ACTIVE_BINDINGS`/`PUSH_FRAMES`).
//! The latter pair is thread-local storage that the scheduler must swap out
//! with the ctx id, which is what [`BindingLocals`] is for. It is DEFINED in
//! `env.rs`, beside the only code that ever mutates it (`VarCell::
//! push_binding`/`pop_binding`, `push_thread_bindings`/`pop_thread_bindings`)
//! and re-exported here: moving the two thread-locals into this module would
//! buy nothing but a crate-visible handle on them and one more hop on the
//! `binding` hot path. The scheduler still sees one coherent switch API --
//! `ctx::{set_ctx, take_binding_locals, install_binding_locals}`.
//!
//! ## One block, one thunk (L2/W2 item 1)
//!
//! On aarch64-darwin every `thread_local!` access is a CALL through the
//! TLV descriptor -- there is no `[tpidr + const]` addressing to fold it
//! into. A scheduler pass used to touch six distinct thread-locals
//! (`CURRENT_CTX` here; `CURRENT_YIELDER`, `CURRENT_TASK`, `SHARD_TLS`,
//! `DIRECT_NEXT` in `runtime`; `ACTIVE_BINDINGS`/`PUSH_FRAMES` in `env`),
//! and paid a thunk per access -- ~14 static access SITES per hop
//! (docs/L2-PROBE-RESULTS.md §6 item 1). The consolidation: everything a
//! switch reads or writes lives in ONE [`ExecTls`] block, so one thunk
//! resolves the block and every field after that is a plain load/store at
//! a constant offset. **Measured honestly, this bought under 1 ns/hop**:
//! LLVM already CSEs the thunk per thread-local per function, so the real
//! dynamic count was ~5, not 14 (`runtime`'s module doc, "What W2
//! bought", has the profile). The block earns its keep as the home of the
//! fields W2's real wins store through -- the `task` borrow and the
//! runnext pair -- not as a thunk-count optimization.
//!
//! The block lives HERE and not in `runtime` because `runtime` already
//! depends on `ctx` and this keeps the dependency edge one-way -- `runtime`
//! still owns the MEANING of every field but `ctx` (it is the only writer
//! of `yielder`/`shard`/`direct_next`, and their doc comments say so).
//!
//! Two properties are load-bearing and must survive any edit here:
//!
//! - **All fields are `Cell`s of `Copy` scalars, and the block is
//!   `const`-initialized.** A `Drop` field (an `Option<Arc<..>>`, say)
//!   would force the thread-local into its lazy/destructor-registering form
//!   and put a check in front of every access -- including
//!   [`current_ctx`], which every dynamic-var read on every PLAIN thread
//!   goes through.
//! - **A non-scheduler thread's block is all zeros and stays that way.**
//!   `yielder == 0` is `runtime::in_task() == false`, `shard == 0` is "I am
//!   nobody's scheduler", `ctx == 0` is "never asked for an id". So a plain
//!   OS thread behaves exactly as it did before this block existed.

use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};

pub use crate::env::{
    install_binding_locals, swap_binding_locals, take_binding_locals, BindingLocals,
};

/// The execution identity of whatever is running on this thread right now:
/// one thread-local block, one TLV thunk (module doc, "One block, one
/// thunk").
///
/// Only [`with_exec`] hands one of these out, and only within a closure --
/// the reference must not outlive the access, and there is no way to make
/// one that does.
pub(crate) struct ExecTls {
    /// The key `VarCell`'s binding stacks are indexed by; 0 = this thread
    /// has never asked. See [`current_ctx`]. Written by a scheduler on
    /// every switch and by [`current_ctx`] itself on a plain thread's first
    /// call.
    pub(crate) ctx: Cell<u64>,
    /// `runtime`: the running task's `Yielder`, as a `usize`; 0 = this
    /// thread is not inside a task (`runtime::in_task`). A `usize` and not a
    /// pointer so the block stays `const`-initializable and nothing here
    /// ever needs to be `Send`.
    pub(crate) yielder: Cell<usize>,
    /// `runtime`: the running task's `TaskShared`, BORROWED from the shard's
    /// own `TaskEntry` for exactly the duration of one resume; null outside a
    /// task. Typed `*const ()` rather than `*const runtime::TaskShared` only
    /// to keep this module free of a dependency on `runtime` (the edge runs
    /// the other way); `runtime::resume_task` is the sole writer, the sole
    /// caster, and the place the validity argument is written.
    pub(crate) task: Cell<*const ()>,
    /// `runtime`: `Arc::as_ptr` of the shard this thread IS, as a `usize`;
    /// 0 on every thread that is not a shard loop. Identity only -- never
    /// dereferenced.
    pub(crate) shard: Cell<usize>,
    /// `runtime`: the L2 runnext slot -- one task id, 0 = empty.
    pub(crate) direct_next: Cell<u64>,
    /// `runtime`: the shard-slab index of [`direct_next`](Self::direct_next)'s
    /// task. Meaningless while `direct_next` is 0; the id is what makes the
    /// slot trustworthy, not the other way round.
    pub(crate) direct_slot: Cell<u32>,
}

thread_local! {
    /// This thread's [`ExecTls`]. `const`-initialized and `Drop`-free on
    /// purpose -- see the module doc.
    static EXEC: ExecTls = const {
        ExecTls {
            ctx: Cell::new(0),
            yielder: Cell::new(0),
            task: Cell::new(std::ptr::null()),
            shard: Cell::new(0),
            direct_next: Cell::new(0),
            direct_slot: Cell::new(0),
        }
    };
}

/// Run `f` against this thread's [`ExecTls`]. ONE thread-local access, no
/// matter how many fields `f` touches -- which is the entire point, so
/// callers on the switch path should batch their reads and writes into a
/// single call rather than making several.
#[inline]
pub(crate) fn with_exec<R>(f: impl FnOnce(&ExecTls) -> R) -> R {
    EXEC.with(f)
}

/// Ids start at 1 so that 0 can mean "this thread has never asked". Handing
/// out 2^64 of them is not a scenario: at one task per nanosecond the
/// counter needs ~584 years to wrap, so no reuse policy is warranted.
static NEXT_CTX: AtomicU64 = AtomicU64::new(1);

/// The key `VarCell`'s binding stacks are indexed by. Assigns this thread a
/// fresh id on first call and never changes it again unless a scheduler
/// calls [`set_ctx`] -- so an ordinary thread's id is as stable as its
/// `ThreadId` was, and a shard thread's id is whichever task it is running.
#[inline]
pub fn current_ctx() -> u64 {
    with_exec(|e| {
        let id = e.ctx.get();
        if id != 0 {
            return id;
        }
        let fresh = fresh_ctx();
        e.ctx.set(fresh);
        fresh
    })
}

/// Installs `id` as the running context. Scheduler-only: every caller owes
/// a matching restore, because everything keyed on the context id (binding
/// stacks) follows this call and nothing validates it.
///
/// A scheduler on the switch path should NOT use this: it costs a whole
/// thread-local access for one field, and the switch already holds an
/// [`ExecTls`] reference it can store through. It stays public because
/// `tests/ctx_binding_test.rs` drives a switch by hand with it, and because
/// it is the honest one-field spelling for anything off the hot path.
#[inline]
pub fn set_ctx(id: u64) {
    with_exec(|e| e.ctx.set(id));
}

/// [`set_ctx`] returning the id it displaced, so a switch-in site can save
/// the outgoing context without a second thread-local access. Returns 0 if
/// the thread had never been assigned one (a shard thread that has only ever
/// run tasks never mints an id of its own).
#[inline]
pub fn swap_ctx(id: u64) -> u64 {
    with_exec(|e| e.ctx.replace(id))
}

/// A never-before-used context id, for a task being spawned. Independent of
/// any thread: the caller decides which thread (if any) it becomes current
/// on.
pub fn fresh_ctx() -> u64 {
    NEXT_CTX.fetch_add(1, Ordering::Relaxed)
}
