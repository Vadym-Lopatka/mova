//! Per-shard coroutine stack pool (docs/L1-LANDING-SPEC.md §W2).
//!
//! **Why a pool exists at all.** P1/B2 measured the naive
//! "one `mmap`+`mprotect` per spawn, one `munmap` per finish" shape at
//! **2.3–4.8 µs/task** — 100% of it kernel VM bookkeeping, none of it the
//! ~1.5 ns context switch (P1/B1). P1/B2b then measured the same workload
//! through a 64-stack pool recycled with [`Coroutine::into_stack`] at
//! **7.5 ns/task at 8 MiB** — 300x cheaper, and stack SIZE stops mattering
//! once `mprotect` leaves the hot path. That number is the whole reason
//! `go` can be a task: without it a spawn costs more than the OS thread it
//! replaces was ever going to save.
//!
//! **No lock.** One pool per shard, touched only by that shard's own OS
//! thread (tasks never migrate — design §3.2), so it is a plain `&mut`
//! datastructure. There is nothing to synchronize and nothing to contend.
//!
//! **FIFO, deliberately.** P1's surprise #6: a LIFO free list
//! (`Vec::push`/`Vec::pop`) degenerates into reusing the single
//! most-recently-freed stack, which produces a *better*-looking ns/spawn
//! number while leaving the other 63 stacks cold and untested. `VecDeque`
//! front/back rotation actually cycles all [`POOL_CAP`] of them.
//!
//! ## R8: pooled stacks are RSS high-water marks
//!
//! The design's R8 (measured in P1/B2b): a recycled stack NEVER gives its
//! faulted-in pages back. Reusing a stack that was once driven 1 MiB deep
//! for a task that needs 1 KiB moved RSS by exactly **0 KiB** — residency
//! per pooled stack is a high-water mark, not a live-usage figure. One deep
//! task therefore taxes every future occupant of that physical stack.
//!
//! Two policies answer it here, and they are the only two costs this module
//! has beyond a `VecDeque` push/pop:
//!
//! - **Cap.** Past [`POOL_CAP`] stacks the pool stops accepting: the
//!   recycled stack is dropped, and `DefaultStack::drop` `munmap`s it. A
//!   burst that spikes a shard to thousands of live tasks hands its stacks
//!   back to the OS instead of parking gigabytes of high-water marks in a
//!   free list forever.
//! - **Aging.** Every [`MADVISE_EVERY`]th recycle, `madvise(MADV_FREE)` over
//!   the recycled stack's writable range tells the kernel those pages are
//!   reclaimable — an amortized decommit that resets the high-water mark
//!   without a `munmap`/`mmap` pair. Amortized 1/256 so the hot path stays
//!   syscall-free and the P1/B2b 7.5 ns order survives: one `madvise` per
//!   256 spawns is ~4 ns/spawn of a ~1 µs syscall.
//!
//! `MADV_FREE` is the right verb rather than `MADV_DONTNEED`: it is lazy
//! (the kernel reclaims only under pressure, so an immediately-reused stack
//! usually keeps its pages and pays no refault) and it is exactly the
//! "these bytes are garbage, the next writer initializes before it reads"
//! contract a coroutine stack satisfies by construction.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};

use corosensei::stack::{DefaultStack, Stack};

use super::TASK_STACK_SIZE;

/// Stacks a shard keeps parked for reuse. 64 is P1/B2b's measured
/// configuration; at 8 MiB reservations it is 512 MiB of *address space*
/// per shard and (per B2b's constant-depth run) about one committed page
/// per stack for shallow tasks.
const POOL_CAP: usize = 64;

/// Recycles between `madvise(MADV_FREE)` sweeps — the R8 aging period.
const MADVISE_EVERY: u64 = 256;

pub(super) struct StackPool {
    /// FIFO: `pop_front` to hand out, `push_back` to recycle.
    free: VecDeque<DefaultStack>,
    /// Total recycles this pool has seen; the aging trigger is
    /// `recycles % MADVISE_EVERY == 0`.
    recycles: u64,
}

impl StackPool {
    pub(super) fn new() -> Self {
        StackPool { free: VecDeque::new(), recycles: 0 }
    }

    /// A stack for a task about to be created. Recycled if one is parked,
    /// freshly `mmap`ed otherwise.
    ///
    /// A failure here means the process cannot reserve 8 MiB of address
    /// space; there is no useful local recovery (the shard's whole job is
    /// running tasks on stacks) so it panics on the shard thread rather
    /// than silently dropping the task. See the module doc in `mod.rs` for
    /// why that is the same posture the timer service takes.
    pub(super) fn take(&mut self) -> DefaultStack {
        if let Some(stack) = self.free.pop_front() {
            return stack;
        }
        STACKS_MMAPED.fetch_add(1, Ordering::Relaxed);
        DefaultStack::new(TASK_STACK_SIZE).expect("mova runtime: task stack mmap failed")
    }

    /// Hand a finished task's stack back. Past [`POOL_CAP`] the stack is
    /// dropped instead (`munmap`), which is the pool's eviction policy.
    pub(super) fn recycle(&mut self, stack: DefaultStack) {
        if self.free.len() >= POOL_CAP {
            // Dropped here => `DefaultStack::drop` => `munmap`. Deliberate:
            // see the cap rationale in the module doc.
            return;
        }
        self.recycles += 1;
        if self.recycles % MADVISE_EVERY == 0 {
            decommit(&stack);
        }
        self.free.push_back(stack);
    }

}

/// Fresh `mmap`s across all shards' pools — P1/B2b's
/// `fresh_mmap_allocs_beyond_prepop`, as a live counter. Process-global
/// rather than per-pool because a pool lives on its shard thread's stack
/// and is reachable from nowhere else; this is the one number about it a
/// test or a bench can actually observe. Its shape is the whole pooling
/// claim: it must plateau near `POOL_CAP * shards` while
/// `runtime::tasks_spawned()` runs away from it.
static STACKS_MMAPED: AtomicU64 = AtomicU64::new(0);

pub(super) fn stacks_mmaped() -> u64 {
    STACKS_MMAPED.load(Ordering::Relaxed)
}

/// Tell the kernel the stack's writable range is reclaimable (R8 aging).
///
/// Safe to do here and ONLY here: `recycle` is called with the stack owned
/// outright, after its coroutine has run to completion (or been unwound) and
/// been consumed by `into_stack()`, so nothing is executing on it and no
/// pointer into it is live. The next occupant is an ordinary Rust stack
/// frame chain, which writes every local before reading it — the same
/// argument P1/B2b makes for why corosensei never re-zeroes a recycled
/// stack.
///
/// `DefaultStack`'s layout (corosensei `stack/unix.rs`): one `mmap` of
/// `mmap_len` bytes, `limit()` at its low end, `base()` one past its high
/// end, and the FIRST page left `PROT_NONE` as the guard. So the writable
/// range is `[limit + page_size, base)` and the guard page must be skipped —
/// `madvise` on a `PROT_NONE` page is harmless but pointless.
fn decommit(stack: &DefaultStack) {
    let base = stack.base().get();
    let limit = stack.limit().get();
    let page = page_size();
    let start = limit + page;
    if start >= base {
        return;
    }
    let len = base - start;
    // Advisory: a failure (EINVAL on an exotic mapping, ENOSYS on a target
    // without MADV_FREE) costs nothing but the aging benefit, so the return
    // value is deliberately ignored rather than turned into a panic on a
    // shard thread.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    unsafe {
        libc::madvise(start as *mut libc::c_void, len, libc::MADV_FREE);
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (start, len);
    }
}

fn page_size() -> usize {
    // Not cached: `decommit` runs once per MADVISE_EVERY recycles, so one
    // extra `sysconf` there is unmeasurable next to the `madvise` it guards.
    unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize }
}
