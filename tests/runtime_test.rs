//! L1/W2 gate tests for `src/runtime/` (docs/L1-LANDING-SPEC.md §W2).
//!
//! Every wait in this file is a DEADLINE loop, never a bare block: a lost
//! wake is the failure mode this module exists to rule out, and a test that
//! hangs on one reports nothing. A timeout here always means "the runtime
//! dropped a task", not "the machine was slow" -- the budgets are seconds
//! against microsecond-scale work.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mova::runtime::{self, TaskWaker};

/// Spin-with-sleep until `cond`, or fail with `what`. 30s is ~4 orders of
/// magnitude over the slowest thing any test here asks for.
fn await_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if cond() {
            return;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    panic!("runtime_test: timed out waiting for {what} -- a wake was lost");
}

/// 100k trivial tasks all complete, and round-robin placement actually
/// spread them over every shard rather than piling them onto one.
#[test]
fn hundred_thousand_tasks_complete_across_every_shard() {
    const N: u64 = 100_000;
    let done = Arc::new(AtomicU64::new(0));

    let before: u64 = runtime::shard_finished_counts().iter().sum();
    for _ in 0..N {
        let done = done.clone();
        runtime::spawn(move || {
            done.fetch_add(1, Ordering::Relaxed);
        });
    }
    await_until("100k tasks to finish", || done.load(Ordering::Relaxed) == N);
    // A body finishing precedes its shard's bookkeeping by a few
    // instructions, so the counter claim gets its own (instant) wait.
    await_until("the shard counters to account for them", || {
        runtime::shard_finished_counts().iter().sum::<u64>() >= before + N
    });

    let shards = runtime::shard_count();
    assert!(shards >= 1, "shard_count() must be at least 1");

    // Every shard must have run something. With N = 100_000 and
    // `available_parallelism()` shards, round-robin gives each ~100_000/N
    // tasks; a shard at zero means placement collapsed onto one thread.
    // (Other tests in this binary spawn too, so only the "nonzero
    // everywhere" and "sum accounts for N" claims are made.)
    let counts = runtime::shard_finished_counts();
    assert_eq!(counts.len(), shards, "one finished-counter per shard");
    for (i, c) in counts.iter().enumerate() {
        assert!(*c > 0, "shard {i} ran no tasks at all: {counts:?}");
    }
}

/// A panicking task is contained at its resume site: the shard keeps its
/// thread and keeps serving tasks -- including the ones placed on that SAME
/// shard afterwards.
///
/// NOTE: this test intentionally prints a panic backtrace line for
/// `mova-shard-<i>` plus the runtime's own "task N panicked" line. That
/// stderr noise is the pass condition, not a failure.
#[test]
fn a_panicking_task_does_not_kill_its_shard() {
    let shards = runtime::shard_count();
    let panicked_before = runtime::tasks_panicked();

    // Round-robin placement is a global counter, so this task lands on SOME
    // shard S -- which one is not knowable from here.
    runtime::spawn(|| {
        panic!("runtime_test: deliberate task panic");
    });
    await_until("the panicking task to be counted", || {
        runtime::tasks_panicked() > panicked_before
    });

    // So instead of guessing S, require EVERY shard to finish a task after
    // the panic. If S's thread had died with its task, its counter would
    // freeze here and this would time out. Spawning inside the wait loop
    // keeps the claim deterministic even though other tests in this binary
    // are perturbing the global round-robin cursor concurrently.
    let before = runtime::shard_finished_counts();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let now = runtime::shard_finished_counts();
        if before.iter().zip(now.iter()).all(|(b, n)| n > b) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "a shard stopped finishing tasks after a panic: before={before:?} now={now:?}"
        );
        for _ in 0..(shards * 4) {
            runtime::spawn(|| {});
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Park/wake round trip through a hand-held [`TaskWaker`]: the task parks,
/// a plain OS thread wakes it much later, the task resumes exactly once
/// past its `park_current_yield()`.
#[test]
fn park_then_wake_round_trip() {
    let waker: Arc<Mutex<Option<TaskWaker>>> = Arc::new(Mutex::new(None));
    let parked = Arc::new(AtomicBool::new(false));
    let resumed = Arc::new(AtomicBool::new(false));
    let in_task_seen = Arc::new(AtomicBool::new(false));

    {
        let (waker, parked, resumed, in_task_seen) = (
            waker.clone(),
            parked.clone(),
            resumed.clone(),
            in_task_seen.clone(),
        );
        runtime::spawn(move || {
            in_task_seen.store(runtime::in_task(), Ordering::Release);
            // Register first, THEN announce, THEN yield -- the discipline
            // every real park site owes (register under the waker's lock,
            // drop the lock, suspend).
            *waker.lock().unwrap() = Some(runtime::current_waker());
            parked.store(true, Ordering::Release);
            runtime::park_current_yield();
            resumed.store(true, Ordering::Release);
        });
    }

    await_until("the task to publish its waker", || {
        parked.load(Ordering::Acquire)
    });
    assert!(
        in_task_seen.load(Ordering::Acquire),
        "in_task() must be true inside a task body"
    );
    // Give the task time to actually reach the suspend, so this exercises
    // the PARKED -> READY arm rather than the NOTIFIED one.
    std::thread::sleep(Duration::from_millis(50));
    assert!(
        !resumed.load(Ordering::Acquire),
        "a parked task must not run past its suspend on its own"
    );

    let w = waker.lock().unwrap().clone().expect("waker published");
    w.wake();
    await_until("the woken task to resume", || {
        resumed.load(Ordering::Acquire)
    });

    // in_task() is false on this (plain) OS thread -- the property that
    // keeps the task arm invisible to `>!!`/`<!!` off-shard.
    assert!(!runtime::in_task());
}

/// The wake-before-park race, forced rather than hoped for.
///
/// This is the P2 `wake_pending`/`NOTIFIED` proof: a wake that lands after
/// the task published its waker but BEFORE it suspended must not be lost.
/// The task busy-waits on `fired` so that by the time it calls
/// `park_current_yield()` the `wake()` has provably already happened and has
/// nothing left to enqueue -- if the shard then parked the task on the
/// strength of "it yielded", nobody would ever wake it again and this test
/// would time out.
#[test]
fn wake_landing_before_the_park_is_not_lost() {
    // Repeat: the race is deterministic by construction, but a handful of
    // rounds also exercises it against a shard that is servicing other work.
    for round in 0..64 {
        let waker: Arc<Mutex<Option<TaskWaker>>> = Arc::new(Mutex::new(None));
        let published = Arc::new(AtomicBool::new(false));
        let fired = Arc::new(AtomicBool::new(false));
        let resumed = Arc::new(AtomicBool::new(false));

        {
            let (waker, published, fired, resumed) = (
                waker.clone(),
                published.clone(),
                fired.clone(),
                resumed.clone(),
            );
            runtime::spawn(move || {
                *waker.lock().unwrap() = Some(runtime::current_waker());
                published.store(true, Ordering::Release);
                // Still RUNNING, still on the CPU: wait for the wake to have
                // already been delivered.
                while !fired.load(Ordering::Acquire) {
                    std::hint::spin_loop();
                }
                // ...and only now fall asleep.
                runtime::park_current_yield();
                resumed.store(true, Ordering::Release);
            });
        }

        let waker_thread = {
            let (waker, published, fired) = (waker.clone(), published.clone(), fired.clone());
            std::thread::spawn(move || {
                while !published.load(Ordering::Acquire) {
                    std::hint::spin_loop();
                }
                let w = waker.lock().unwrap().clone().expect("waker published");
                w.wake();
                fired.store(true, Ordering::Release);
            })
        };

        await_until(
            &format!("round {round}: the pre-park wake to be honored"),
            || resumed.load(Ordering::Acquire),
        );
        waker_thread.join().unwrap();
    }
}

/// A waker held past the end of its task is inert, not a crash: `wake()` on
/// a DONE task is a documented no-op, which is what lets a chan wake site
/// drain a stale registration without checking anything first.
#[test]
fn waking_a_finished_task_is_a_no_op() {
    let waker: Arc<Mutex<Option<TaskWaker>>> = Arc::new(Mutex::new(None));
    let done = Arc::new(AtomicBool::new(false));
    {
        let (waker, done) = (waker.clone(), done.clone());
        runtime::spawn(move || {
            *waker.lock().unwrap() = Some(runtime::current_waker());
            done.store(true, Ordering::Release);
        });
    }
    await_until("the task to finish", || done.load(Ordering::Acquire));
    std::thread::sleep(Duration::from_millis(20));
    let w = waker.lock().unwrap().clone().expect("waker published");
    w.wake();
    w.wake();
    // Nothing to assert but "we are still here": a re-queue of a finished
    // task would either resume a completed coroutine (corosensei panics on
    // that) or spin the shard.
    std::thread::sleep(Duration::from_millis(20));
    assert!(runtime::tasks_finished() > 0);
}

/// W4b: a task that spawns a child while its own shard is not flooded
/// places that child on the SAME shard (module doc, "Spawn placement" --
/// the whole point of family-local affinity: a coordinator/worker
/// rendezvous should pay a same-shard scheduler pass, not a cross-shard
/// wake). Retried, not asserted on the first attempt: `Shard::load` is a
/// process-global heuristic shared with every other test in this binary,
/// so a concurrent test can transiently push the parent's shard over
/// `SPAWN_LOCAL_MAX` and cause one attempt to legitimately fall back to
/// round-robin. A real placement bug fails every attempt; noise fails a
/// few and then clears.
#[test]
fn task_spawned_child_colocates_on_idle_parent_shard() {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let parent_idx: Arc<Mutex<Option<usize>>> = Arc::new(Mutex::new(None));
        let child_idx: Arc<Mutex<Option<usize>>> = Arc::new(Mutex::new(None));
        {
            let (parent_idx, child_idx) = (parent_idx.clone(), child_idx.clone());
            runtime::spawn(move || {
                // Neither side blocks or sleeps here -- doing so inside a
                // task body would block the whole shard (module doc,
                // "Accepted footgun"), including the very shard the child
                // needs in order to run. Both sides just publish and
                // return; the OUTER (plain) thread does the waiting.
                *parent_idx.lock().unwrap() = runtime::current_shard_index();
                runtime::spawn(move || {
                    *child_idx.lock().unwrap() = runtime::current_shard_index();
                });
            });
        }
        await_until("parent and child to record their shard", || {
            parent_idx.lock().unwrap().is_some() && child_idx.lock().unwrap().is_some()
        });
        let parent = parent_idx.lock().unwrap().unwrap();
        let child = child_idx.lock().unwrap().unwrap();
        if parent == child {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "W4b: task-spawned child never colocated with an idle parent shard \
             within the retry budget (last attempt: parent shard {parent}, child shard {child})"
        );
    }
}

/// W4b: a task that keeps spawning children past `SPAWN_LOCAL_MAX` spills
/// the excess to other shards instead of piling an unbounded family onto
/// one core (module doc, "Spawn placement" -- there is no work stealing in
/// v0, so unconditional affinity would have no way back out of that pile-
/// up). Children stay alive (parked, not `DONE`) for the DURATION of the
/// parent's spawn loop, so each one keeps counting toward
/// `Shard::load` for its later siblings -- a child that finished instantly
/// would free its slot before the next sibling is placed and the flood
/// would never trip.
#[test]
fn task_spawn_floods_past_local_max_spill_to_other_shards() {
    if runtime::shard_count() < 2 {
        // Nothing to spill TO on a single-shard machine -- the claim is
        // vacuous there.
        return;
    }

    const EXTRA: usize = 8;
    let total = runtime::SPAWN_LOCAL_MAX + EXTRA;

    let parent_idx: Arc<Mutex<Option<usize>>> = Arc::new(Mutex::new(None));
    let child_idxs: Arc<Mutex<Vec<usize>>> = Arc::new(Mutex::new(Vec::new()));
    let wakers: Arc<Mutex<Vec<TaskWaker>>> = Arc::new(Mutex::new(Vec::new()));
    let published = Arc::new(AtomicU64::new(0));

    {
        let (parent_idx, child_idxs, wakers, published) =
            (parent_idx.clone(), child_idxs.clone(), wakers.clone(), published.clone());
        runtime::spawn(move || {
            *parent_idx.lock().unwrap() = runtime::current_shard_index();
            for _ in 0..total {
                let (child_idxs, wakers, published) =
                    (child_idxs.clone(), wakers.clone(), published.clone());
                runtime::spawn(move || {
                    child_idxs
                        .lock()
                        .unwrap()
                        .push(runtime::current_shard_index().expect("running inside a task"));
                    wakers.lock().unwrap().push(runtime::current_waker());
                    published.fetch_add(1, Ordering::Release);
                    // Park (cooperative, not a blocking sleep) so this
                    // child stays live and keeps counting toward its
                    // shard's load for the rest of the parent's loop.
                    runtime::park_current_yield();
                });
            }
        });
    }

    await_until("every child to publish its shard + waker", || {
        published.load(Ordering::Acquire) as usize == total
    });

    let parent = parent_idx.lock().unwrap().expect("parent published its shard");
    let children = child_idxs.lock().unwrap().clone();
    assert_eq!(children.len(), total);
    let on_parent = children.iter().filter(|&&s| s == parent).count();
    assert!(
        on_parent < total,
        "W4b flood guard: all {total} children landed on the parent's shard {parent} \
         -- SPAWN_LOCAL_MAX = {} should have forced spillover",
        runtime::SPAWN_LOCAL_MAX
    );
    assert!(
        on_parent <= runtime::SPAWN_LOCAL_MAX + 1,
        "W4b flood guard: {on_parent} of {total} children co-located -- more than \
         SPAWN_LOCAL_MAX ({}) plus the parent's own slot allows",
        runtime::SPAWN_LOCAL_MAX
    );

    // Release every parked child so it can finish and free its stack.
    for w in wakers.lock().unwrap().iter() {
        w.wake();
    }
}

/// Stack pooling is what makes spawn cheap (P1/B2b: 7.5 ns vs 2.3-4.8 µs
/// unpooled). Batched so that stacks genuinely come back to the pool before
/// the next batch draws from it -- 200 batches x 100 tasks also crosses the
/// pool's `MADVISE_EVERY = 256` aging threshold several times per shard,
/// which is the only exercise the `madvise(MADV_FREE)` path gets.
#[test]
fn batched_tasks_recycle_and_age_pooled_stacks() {
    const BATCHES: u64 = 200;
    const PER: u64 = 100;
    let done = Arc::new(AtomicU64::new(0));
    let mmaped_before = runtime::stacks_mmaped();
    for b in 0..BATCHES {
        for _ in 0..PER {
            let done = done.clone();
            runtime::spawn(move || {
                done.fetch_add(1, Ordering::Relaxed);
            });
        }
        await_until("a batch of pooled spawns", || {
            done.load(Ordering::Relaxed) == (b + 1) * PER
        });
    }

    // The pooling claim, as a number. 20k tasks drawing from pools capped at
    // 64 stacks per shard must not `mmap` 20k stacks: the bound is the
    // in-flight high-water mark, which batching keeps near `shards`. The
    // assertion is generous (other tests in this binary allocate too) --
    // it is calibrated to catch "pooling is not happening at all", which is
    // the 300x spawn-cost regression it exists to prevent.
    let fresh = runtime::stacks_mmaped() - mmaped_before;
    assert!(
        fresh < BATCHES * PER / 4,
        "stack pooling did not engage: {fresh} fresh mmaps for {} tasks",
        BATCHES * PER
    );
}
