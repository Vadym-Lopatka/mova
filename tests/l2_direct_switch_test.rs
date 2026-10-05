//! L2/W1 gates: the runnext direct switch, its budget, and the per-task sudog
//! cell cache (docs/L2-DIRECT-SWITCH-DESIGN.md, docs/L2-LANDING-SPEC.md §W1).
//!
//! These are CORRECTNESS tests and they run in the default `cargo test` sweep
//! — none of them is a perf gate. The two perf benches that measure the lever
//! (`p3_scheduler_only_ping_pong`, `p3_fairness_third_task`) live beside E1 in
//! `tests/task_stress_test.rs`, `#[ignore]`d for a quiet machine.
//!
//! What each test is actually falsifying:
//!
//! - **hit rate** — that the slot is HOT. The probe's headline finding was
//!   that runnext is the smallest of the three levers, and that conclusion is
//!   only worth anything if the lever was firing on ~every hop. A regression
//!   that quietly makes `wake()` fall through to `inject_ready` would cost ~7
//!   ns/hop and break no other test in the repo.
//! - **kill switch** — that `MOVA_NO_DIRECT_SWITCH=1` really does restore L1
//!   behaviour, so a future scheduling bug can be bisected onto or off this
//!   lever in one env var.
//! - **fairness** — that [`runtime`]'s budget turn is a LIVENESS rule.
//!   Asserted by ORDERING, never by wall clock: a co-located task that needs
//!   ordinary scheduler turns must finish before two chatty pairs do, and the
//!   pairs' second half must START at all (its `Job::Spawn`s sit in the inbox
//!   behind the slot).
//! - **slot-occupied fallback** — that keeping the first occupant and
//!   injecting the second (design §6-V1, our deliberate divergence from Go's
//!   replace-and-evict) loses nobody. Two pairs on one shard take that
//!   fallback thousands of times.
//! - **cell reuse** — that a task's ONE take cell and ONE commit cell survive
//!   being cycled through takes, puts, and both terminal close outcomes.
//!
//! **S2 is law here too**: nothing below adds a timeout to a task park. The
//! [`Watchdog`]s are the TEST's failure-reporting mechanism — they turn a lost
//! wake into a diagnostic instead of a wedged harness.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mova::internal::task_chan::{self, BufferPolicy};
use mova::runtime;

/// Same shape as `tests/task_stress_test.rs`'s watchdog: aborts the PROCESS
/// with a diagnostic if the guarded section does not finish, because the
/// failure mode these tests guard against (a starved inbox, a lost direct
/// switch) is a HANG, and a hang inside `cargo test` is otherwise mute.
struct Watchdog(Arc<AtomicBool>);

impl Watchdog {
    fn arm(secs: u64, what: &'static str) -> Watchdog {
        let done = Arc::new(AtomicBool::new(false));
        let flag = done.clone();
        std::thread::spawn(move || {
            for _ in 0..(secs * 20) {
                std::thread::sleep(Duration::from_millis(50));
                if flag.load(Ordering::Relaxed) {
                    return;
                }
            }
            eprintln!(
                "L2 WATCHDOG: {what} did not finish in {secs}s -- a lost wake, a starved inbox, or a \
                 direct switch that dropped an id. Per S2 that is a bug to report with evidence, never \
                 a timeout to raise."
            );
            std::process::abort();
        });
        Watchdog(done)
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Spin-with-sleep until `cond`, or panic. The only place this file waits on
/// anything other than a chan.
fn await_until(what: &str, budget: Duration, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        if cond() {
            return;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    panic!("l2_direct_switch_test: timed out after {budget:?} waiting for {what}");
}

/// Serializes the tests in this file that depend on WHERE their tasks land.
///
/// `runtime::spawn`'s family-local placement only co-locates a child with its
/// parent while that shard holds fewer than `SPAWN_LOCAL_MAX` live tasks
/// (`runtime`'s module doc). `cargo test` runs this file's tests in parallel
/// threads, so without a gate one test's live tasks can push another test's
/// coordinator over the threshold and scatter the family it was trying to
/// co-locate — a flake with a completely misleading message. This is a
/// TEST-harness concern only; nothing in the runtime needs it.
static PLACEMENT_GATE: Mutex<()> = Mutex::new(());

/// Take the placement gate AND wait for the process to hold no live tasks, so
/// every shard's `load` really is 0 before this test's coordinator picks one.
/// The gate alone is not enough: the previous test's tasks store `DONE` (and
/// decrement `load`) a moment AFTER the done-chan signal that released it.
fn quiesced_gate(what: &str) -> std::sync::MutexGuard<'static, ()> {
    // A test that panics while holding it poisons it; the next test still
    // wants to run, and the poison carries no state worth respecting here.
    let g = PLACEMENT_GATE.lock().unwrap_or_else(|e| e.into_inner());
    await_until(&format!("every earlier task to reach DONE before {what}"), Duration::from_secs(30), || {
        runtime::tasks_spawned() == runtime::tasks_finished()
    });
    g
}

// ============================================================================
// L1. Hit rate + kill switch (child processes: both read process-global state)
// ============================================================================

/// Run [`l2_ping_pong_child`] in a fresh process and return
/// `(hops, resumes, direct)`.
///
/// A child process, not an in-process measurement, for two independent
/// reasons: `runtime::direct_switches()`/`tasks_resumed()` are process-global
/// counters that every other test in this binary also moves, and the kill
/// switch is a `OnceLock` read once per process, so "with the switch off" and
/// "with the switch on" cannot both exist in one run. The house pattern is
/// `tests/geo_intern_probe.rs`'s RSS child worker.
fn run_ping_pong_child(kill_switch: bool) -> (u64, u64, u64) {
    let exe = std::env::current_exe().expect("current_exe");
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["l2_ping_pong_child", "--exact", "--ignored", "--nocapture"]);
    if kill_switch {
        cmd.env("MOVA_NO_DIRECT_SWITCH", "1");
    }
    let out = cmd.output().expect("failed to spawn the L2 ping-pong child worker");
    let s = String::from_utf8_lossy(&out.stdout);
    let field = |name: &str| -> u64 {
        s.lines()
            .find_map(|l| l.strip_prefix(name))
            .unwrap_or_else(|| {
                panic!(
                    "the L2 child worker did not print {name} (kill_switch={kill_switch}); \
                     status={:?}\nstdout={s}\nstderr={}",
                    out.status,
                    String::from_utf8_lossy(&out.stderr)
                )
            })
            .trim()
            .parse()
            .expect("counter was not an integer")
    };
    (field("L2_HOPS="), field("L2_RESUMES="), field("L2_DIRECT="))
}

/// **The lever is hot.** A same-shard unbuffered ping-pong must serve
/// essentially every resume out of the runnext slot, not out of the inbox.
///
/// The bound is 95% rather than 100% because the shape has a handful of
/// resumes that are legitimately not direct switches: the coordinator task's
/// own start, the two children's `Job::Spawn`s, and the final put onto the
/// done chan, all of which ride the inbox by construction. The probe measured
/// 600 000 / 600 003 on this shape (docs/L2-PROBE-RESULTS.md §3).
#[test]
fn direct_switch_hit_rate_is_essentially_total() {
    let (hops, resumes, direct) = run_ping_pong_child(false);
    let rate = direct as f64 / resumes as f64;
    println!("L2 hit rate: {direct} direct / {resumes} resumes over {hops} hops = {:.2}%", rate * 100.0);
    assert!(
        rate >= 0.95,
        "direct-switch hit rate {:.2}% (< 95%): the runnext slot is COLD on the one shape it exists \
         for. Either `wake()`'s same-shard compare stopped matching (SHARD_TLS not published? the \
         family stopped co-locating?) or the slot is being found occupied — check \
         `runtime::direct_switches()` against `tasks_resumed()` before touching anything else.",
        rate * 100.0
    );
}

/// **The kill switch really kills it.** `MOVA_NO_DIRECT_SWITCH=1` must take
/// the process back to L1: every wake injects, the ping-pong still completes,
/// and the slot counter never moves.
#[test]
fn kill_switch_restores_the_l1_inject_path() {
    let (hops, resumes, direct) = run_ping_pong_child(true);
    println!("L2 kill switch: {direct} direct / {resumes} resumes over {hops} hops (expect 0 direct)");
    assert_eq!(
        direct, 0,
        "MOVA_NO_DIRECT_SWITCH=1 still served {direct} resumes out of the runnext slot -- the kill \
         switch is not gating `TaskWaker::wake`'s direct arm"
    );
    assert!(resumes >= hops, "the ping-pong did not actually run under the kill switch");
}

/// The child half of [`run_ping_pong_child`]: E1's shape (a coordinator task
/// spawns ping and pong itself, so family-local placement co-locates them on
/// ONE shard), no timing, just the counters.
///
/// `#[ignore]`d so the default sweep skips it; it is only ever reached by an
/// explicit `--exact --ignored` invocation from the parent.
#[test]
#[ignore = "child-process worker for the hit-rate and kill-switch gates"]
fn l2_ping_pong_child() {
    let _wd = Watchdog::arm(60, "l2_ping_pong_child");
    let rounds: u64 = 20_000;
    let a = task_chan::chan(BufferPolicy::Unbuffered);
    let b = task_chan::chan(BufferPolicy::Unbuffered);
    let done = task_chan::chan(BufferPolicy::Unbuffered);

    let (a0, b0, d0) = (a.clone(), b.clone(), done.clone());
    runtime::spawn(move || {
        let (a1, b1) = (a0.clone(), b0.clone());
        runtime::spawn(move || {
            for i in 0..rounds as i64 {
                task_chan::put_int(&a1, i);
                task_chan::take_int(&b1);
            }
        });
        let (a2, b2, d2) = (a0, b0, d0);
        runtime::spawn(move || {
            for i in 0..rounds as i64 {
                task_chan::take_int(&a2);
                task_chan::put_int(&b2, i);
            }
            task_chan::put_int(&d2, 1);
        });
    });
    assert_eq!(task_chan::take_int(&done), Some(1));
    println!("L2_HOPS={}", rounds * 2);
    println!("L2_RESUMES={}", runtime::tasks_resumed());
    println!("L2_DIRECT={}", runtime::direct_switches());
}

// ============================================================================
// L2. The budget is a liveness rule (ordering, not wall clock)
// ============================================================================

/// **`DIRECT_SWITCH_BUDGET` is a correctness requirement.** Six tasks on one
/// shard: two ping-pong pairs that refill the runnext slot on every hop, plus
/// a third task that takes part in no rendezvous at all and is therefore
/// reachable ONLY through `local` — which the slot outranks.
///
/// Two failures are being excluded at once, and neither needs a stopwatch:
///
/// 1. With no budget the shard loop never revisits `local` or the inbox, so
///    the third task never runs AND the second pair's two `Job::Spawn`s are
///    never read. That is a hang, and the watchdog reports it as one.
/// 2. With a budget that is too generous the third task still completes, but
///    only in the gaps, finishing at the very end of the run (measured: at
///    50.021 ms against an all-done of 50.021 ms — docs/L2-PROBE-RESULTS.md
///    §4). That is a fairness failure in substance, and it is exactly what
///    the ORDERING assert below catches: the third task's signal must be the
///    FIRST thing off the done chan, ahead of both pairs.
///
/// The margin is structural rather than a tuned constant, which is why this
/// is safe to run on a loaded machine: the third task needs ~`turns × BUDGET`
/// pair-hops' worth of scheduling turns (2 000 × 16 = 32 000) while the pairs
/// need 160 000 hops of their own, and both scale together because they share
/// one shard.
#[test]
fn budget_turn_lets_a_co_located_task_finish_first() {
    let _gate = quiesced_gate("budget_turn_lets_a_co_located_task_finish_first");
    let _wd = Watchdog::arm(120, "budget_turn_lets_a_co_located_task_finish_first");
    let rounds: u64 = 40_000;
    let third_turns: u64 = 2_000;

    let done = task_chan::chan(BufferPolicy::Fixed(8));
    let counter = Arc::new(AtomicU64::new(0));
    let shards = Arc::new(Mutex::new(Vec::<usize>::new()));

    let (d0, c0, s0) = (done.clone(), counter.clone(), shards.clone());
    runtime::spawn(move || {
        s0.lock().expect("shards").push(runtime::current_shard_index().expect("in a task"));
        for pair in 1..=2i64 {
            let a = task_chan::chan(BufferPolicy::Unbuffered);
            let b = task_chan::chan(BufferPolicy::Unbuffered);
            let (a1, b1, s1) = (a.clone(), b.clone(), s0.clone());
            runtime::spawn(move || {
                s1.lock().expect("shards").push(runtime::current_shard_index().expect("in a task"));
                for i in 0..rounds as i64 {
                    task_chan::put_int(&a1, i);
                    task_chan::take_int(&b1);
                }
            });
            let (a2, b2, d2, s2) = (a, b, d0.clone(), s0.clone());
            runtime::spawn(move || {
                s2.lock().expect("shards").push(runtime::current_shard_index().expect("in a task"));
                for i in 0..rounds as i64 {
                    task_chan::take_int(&a2);
                    task_chan::put_int(&b2, i);
                }
                task_chan::put_int(&d2, pair);
            });
        }
        // The third task: self-wakes (a `wake()` on a RUNNING task leaves
        // NOTIFIED, so the shard re-queues it onto `local`) and parks. It
        // rendezvouses with nobody, so no direct switch can ever carry it.
        let (d3, c3, s3) = (d0, c0, s0);
        runtime::spawn(move || {
            s3.lock().expect("shards").push(runtime::current_shard_index().expect("in a task"));
            for _ in 0..third_turns {
                c3.fetch_add(1, Ordering::Relaxed);
                runtime::current_waker().wake();
                runtime::park_current_yield();
            }
            task_chan::put_int(&d3, 3);
        });
    });

    let mut seen = Vec::new();
    while seen.len() < 3 {
        seen.push(task_chan::take_int(&done).expect("the done chan is never closed"));
    }

    let placed = shards.lock().expect("shards").clone();
    println!(
        "L2 fairness: done order {seen:?}, third-task turns {}, shard placement {placed:?}",
        counter.load(Ordering::Relaxed)
    );
    assert_eq!(counter.load(Ordering::Relaxed), third_turns, "the third task did not take all its turns");
    assert_eq!(
        seen[0], 3,
        "the co-located third task finished AFTER a chatty pair (done order {seen:?}). It is reachable \
         only through `local`, so this means the budget turn is not servicing `local`/inbox often \
         enough -- DIRECT_SWITCH_BUDGET raised, or the streak no longer reset where it should be."
    );
    seen.sort();
    assert_eq!(seen, vec![1, 2, 3], "a pair did not complete");
    assert!(
        placed.windows(2).all(|w| w[0] == w[1]),
        "this gate is only meaningful with all six tasks on ONE shard; they landed on {placed:?}"
    );
}

// ============================================================================
// L3. Slot-occupied fallback (design §6-V1)
// ============================================================================

/// **Keeping the first occupant loses nobody.** Two ping-pong pairs on one
/// shard means a task woken during the OTHER pair's chain finds the slot
/// already full and takes the `inject_ready` fallback — 14 997 times in the
/// probe's two-pair run, so this is the common case and not a corner.
///
/// If the fallback were wrong (dropping the wake, or double-enqueuing so that
/// a resumed task is later resumed again from a stale id), this shape hangs
/// or mis-delivers; the assertion is that all four tasks complete their full
/// round count with every value accounted for.
#[test]
fn slot_occupied_fallback_completes_both_pairs() {
    let _gate = quiesced_gate("slot_occupied_fallback_completes_both_pairs");
    let _wd = Watchdog::arm(120, "slot_occupied_fallback_completes_both_pairs");
    let rounds: u64 = 20_000;

    let done = task_chan::chan(BufferPolicy::Fixed(4));
    // Each pair's echoer sums what it received; the sum is what proves no
    // value was skipped, duplicated, or delivered to the wrong pair.
    let sums = Arc::new([AtomicI64::new(0), AtomicI64::new(0)]);
    let shards = Arc::new(Mutex::new(Vec::<usize>::new()));

    let (d0, m0, s0) = (done.clone(), sums.clone(), shards.clone());
    runtime::spawn(move || {
        s0.lock().expect("shards").push(runtime::current_shard_index().expect("in a task"));
        for pair in 0..2usize {
            let a = task_chan::chan(BufferPolicy::Unbuffered);
            let b = task_chan::chan(BufferPolicy::Unbuffered);
            let (a1, b1, s1) = (a.clone(), b.clone(), s0.clone());
            runtime::spawn(move || {
                s1.lock().expect("shards").push(runtime::current_shard_index().expect("in a task"));
                for i in 0..rounds as i64 {
                    task_chan::put_int(&a1, i);
                    task_chan::take_int(&b1);
                }
            });
            let (a2, b2, d2, m2, s2) = (a, b, d0.clone(), m0.clone(), s0.clone());
            runtime::spawn(move || {
                s2.lock().expect("shards").push(runtime::current_shard_index().expect("in a task"));
                for i in 0..rounds as i64 {
                    let got = task_chan::take_int(&a2).expect("the pair chan is never closed");
                    m2[pair].fetch_add(got, Ordering::Relaxed);
                    task_chan::put_int(&b2, i);
                }
                task_chan::put_int(&d2, pair as i64);
            });
        }
    });

    let mut seen = Vec::new();
    while seen.len() < 2 {
        seen.push(task_chan::take_int(&done).expect("the done chan is never closed"));
    }
    seen.sort();
    assert_eq!(seen, vec![0, 1], "a pair did not complete");

    let expect: i64 = (0..rounds as i64).sum();
    for (pair, s) in sums.iter().enumerate() {
        assert_eq!(
            s.load(Ordering::Relaxed),
            expect,
            "pair {pair} received a different multiset than it was sent -- a wake was dropped or \
             replayed across the slot/inbox boundary"
        );
    }
    let placed = shards.lock().expect("shards").clone();
    println!("L2 slot-occupied fallback: both pairs complete, shard placement {placed:?}");
    assert!(
        placed.windows(2).all(|w| w[0] == w[1]),
        "this gate only exercises the fallback with both pairs on ONE shard; they landed on {placed:?}"
    );
}

// ============================================================================
// L4. Sudog cell reuse (lever 2)
// ============================================================================

/// **One take cell and one commit cell, cycled through everything a task can
/// do with them.** The cells now live on `TaskShared` for the task's whole
/// life instead of being freshly allocated per park (lever 2), so every
/// terminal state one park leaves behind is the state the NEXT park inherits.
///
/// The script walks a single worker task through, in order: a take that parks
/// and receives a value; a put that parks and is collected; a take that parks
/// and receives; a put that parks and is then CLOSED out from under it
/// (`PUT_CLOSED` — the state the commit cell must be re-armed away from); a
/// put on a fresh chan that succeeds (proving the re-arm); a take that parks
/// and is CLOSED (`TakeSlot::Closed` — which `park_task_taker`'s
/// `mem::replace` leaves back at `Waiting`, the invariant that lets the taker
/// side need no re-arm at all); and a final take that receives, proving the
/// take cell survived the close.
///
/// Every step uses a DIFFERENT chan, so a stale cell cannot be mistaken for a
/// fresh one by accident of reusing the same channel.
///
/// The counterparties are this OS thread. Each blocking step waits on a step
/// counter the worker publishes before it blocks, so the worker is
/// overwhelmingly parked when the counterparty acts — and where it is not,
/// the assertion still holds (a close that lands before the park makes
/// `chan_put` return `false` at its `closed` check instead of via
/// `PUT_CLOSED`), so this test cannot flake into a false PASS or FAIL.
#[test]
fn sudog_cells_survive_being_cycled_through_both_terminal_states() {
    let _gate = quiesced_gate("sudog_cells_survive_being_cycled_through_both_terminal_states");
    let _wd = Watchdog::arm(60, "sudog_cells_survive_being_cycled_through_both_terminal_states");

    let step = Arc::new(AtomicUsize::new(0));
    let log = Arc::new(Mutex::new(Vec::<String>::new()));
    let chans: Vec<_> = (0..7).map(|_| task_chan::chan(BufferPolicy::Unbuffered)).collect();
    let done = task_chan::chan(BufferPolicy::Fixed(1));

    let (c, st, lg, d) = (chans.clone(), step.clone(), log.clone(), done.clone());
    runtime::spawn(move || {
        let say = |s: String| lg.lock().expect("log").push(s);
        st.store(1, Ordering::Release);
        say(format!("take0={:?}", task_chan::take_int(&c[0])));
        st.store(2, Ordering::Release);
        say(format!("put1={}", task_chan::put_int(&c[1], 11)));
        st.store(3, Ordering::Release);
        say(format!("take2={:?}", task_chan::take_int(&c[2])));
        st.store(4, Ordering::Release);
        // Parked putter, then close: the commit cell ends at PUT_CLOSED.
        say(format!("put3={}", task_chan::put_int(&c[3], 33)));
        st.store(5, Ordering::Release);
        // Reusing that same commit cell, which must have been re-armed.
        say(format!("put4={}", task_chan::put_int(&c[4], 44)));
        st.store(6, Ordering::Release);
        // Parked taker, then close: the take cell passes through Closed.
        say(format!("take5={:?}", task_chan::take_int(&c[5])));
        st.store(7, Ordering::Release);
        // Reusing that same take cell.
        say(format!("take6={:?}", task_chan::take_int(&c[6])));
        st.store(8, Ordering::Release);
        task_chan::put_int(&d, 1);
    });

    let at = |n: usize| {
        await_until(&format!("the worker to reach step {n}"), Duration::from_secs(10), || {
            step.load(Ordering::Acquire) >= n
        });
        // The worker publishes the step and THEN blocks; give it the gap.
        std::thread::sleep(Duration::from_millis(20));
    };

    at(1);
    assert!(task_chan::put_int(&chans[0], 10), "put onto a parked task taker must succeed");
    at(2);
    assert_eq!(task_chan::take_int(&chans[1]), Some(11), "take from a parked task putter");
    at(3);
    assert!(task_chan::put_int(&chans[2], 22));
    at(4);
    task_chan::close(&chans[3]);
    at(5);
    assert_eq!(task_chan::take_int(&chans[4]), Some(44), "the re-armed commit cell must still commit");
    at(6);
    task_chan::close(&chans[5]);
    at(7);
    assert!(task_chan::put_int(&chans[6], 66), "the take cell must be usable after a Closed delivery");

    assert_eq!(task_chan::take_int(&done), Some(1), "the worker did not finish its script");
    let got = log.lock().expect("log").clone();
    println!("L2 cell reuse: {got:?}");
    assert_eq!(
        got,
        vec![
            "take0=Some(10)".to_string(),
            "put1=true".to_string(),
            "take2=Some(22)".to_string(),
            "put3=false".to_string(),
            "put4=true".to_string(),
            "take5=None".to_string(),
            "take6=Some(66)".to_string(),
        ],
        "a reused sudog cell delivered the wrong outcome; `put4=false` means the commit cell was not \
         re-armed away from PUT_CLOSED, and `take6=None` means the take cell stayed Closed"
    );
}


/// **W2 item 3's ABA guard** (landing spec §W2 self-review L3): a wake
/// carrying a DEAD task's id must never resume whatever later task inherited
/// its slab slot.
///
/// The shape: finish a task while still holding a clone of its waker, then
/// spawn enough tasks on the same shard that its slot is certainly reused,
/// then fire the stale waker and prove the new occupant did not run.
///
/// There are TWO independent defences and this test would catch the loss of
/// either:
///
/// 1. `TaskWaker::wake`'s `PARKED -> READY` CAS fails on a `DONE` task, so a
///    stale waker never even reaches a queue. That is the one doing the work
///    today, which is why the test asserts on `tasks_resumed()` being FLAT
///    rather than on some second-order effect.
/// 2. `Slab::take` rechecks `entry.task.id == id`. Unreachable given (1) —
///    and checked anyway, because "unreachable given the current state
///    machine" is the kind of premise a later change invalidates quietly. If
///    (1) is ever relaxed (say, to let a `DONE` task's wake be absorbed
///    rather than dropped), this test is what stops that from turning into a
///    stranger's resume.
///
/// A spurious resume is not merely a wasted turn: the victim is parked, so
/// resuming it runs it as though its wake had arrived, and it walks past a
/// chan wait whose value has not been delivered.
#[test]
fn stale_waker_for_a_reused_slot_resumes_nobody() {
    let _wd = Watchdog::arm(60, "stale_waker_for_a_reused_slot_resumes_nobody");

    // A task that hands its waker out, then finishes. `waker_out` is an
    // unbuffered chan; the main thread's take rendezvouses with the task's
    // put, so we hold a live `TaskWaker` for a task that then returns.
    let waker_slot: Arc<Mutex<Option<runtime::TaskWaker>>> = Arc::new(Mutex::new(None));
    let done = task_chan::chan(BufferPolicy::Unbuffered);

    let ws = waker_slot.clone();
    let d0 = done.clone();
    runtime::spawn(move || {
        *ws.lock().expect("waker slot") = Some(runtime::current_waker());
        task_chan::put_int(&d0, 1);
    });
    assert_eq!(task_chan::take_int(&done), Some(1), "the donor task never ran");

    let stale = waker_slot.lock().expect("waker slot").clone().expect("no waker captured");

    // Let the donor actually reach DONE (it returns right after its put, but
    // that return is a scheduler turn we do not synchronise with). Poll on
    // `tasks_finished`, which the shard bumps in the same arm that frees the
    // slot.
    let finished_target = runtime::tasks_finished() + 1;
    let deadline = Instant::now() + Duration::from_secs(10);
    while runtime::tasks_finished() < finished_target && Instant::now() < deadline {
        std::thread::yield_now();
    }

    // Churn: many short-lived tasks, spawned from inside a task so W4b puts
    // them on the donor's shard, guaranteeing its slot is recycled.
    let churn = task_chan::chan(BufferPolicy::Fixed(64));
    let c0 = churn.clone();
    runtime::spawn(move || {
        for i in 0..40i64 {
            let c = c0.clone();
            runtime::spawn(move || {
                task_chan::put_int(&c, i);
            });
        }
    });
    for _ in 0..40 {
        assert!(task_chan::take_int(&churn).is_some(), "a churn task never reported");
    }

    // Now the victim: a task parked on a chan nobody will ever put to. If a
    // stale wake resumed it, its take returns and it puts on `escaped`.
    let victim_park = task_chan::chan(BufferPolicy::Unbuffered);
    let escaped = Arc::new(AtomicI64::new(i64::MIN));
    let parked = task_chan::chan(BufferPolicy::Unbuffered);
    let (vp, esc, pk) = (victim_park.clone(), escaped.clone(), parked.clone());
    runtime::spawn(move || {
        task_chan::put_int(&pk, 1);
        let got = task_chan::take_int(&vp);
        esc.store(got.unwrap_or(-1), Ordering::SeqCst);
    });
    assert_eq!(task_chan::take_int(&parked), Some(1), "the victim never started");
    // The victim's `put_int(&pk, ..)` rendezvoused, so it is running; give it
    // the turn that gets it into its park before we fire the stale wake.
    let resumed_before = loop {
        let a = runtime::tasks_resumed();
        std::thread::sleep(Duration::from_millis(50));
        let b = runtime::tasks_resumed();
        if a == b {
            break b;
        }
    };

    // FIRE. This is the whole test.
    stale.wake();
    stale.wake();

    std::thread::sleep(Duration::from_millis(100));
    let resumed_after = runtime::tasks_resumed();
    println!(
        "L2 ABA: resumes {resumed_before} -> {resumed_after} across two stale wakes"
    );
    assert_eq!(
        resumed_after, resumed_before,
        "a stale waker for a completed task caused {} resume(s) -- either the DONE state no longer \
         blocks the enqueue, or Slab::take stopped checking entry.task.id against the woken id",
        resumed_after - resumed_before
    );
    assert_eq!(
        escaped.load(Ordering::SeqCst),
        i64::MIN,
        "the victim task was resumed out of its park by a dead task's waker (slot reuse ABA)"
    );

    // The victim is still parked forever by construction; unblock it so the
    // process is not left holding a wedged task, and confirm a REAL wake does
    // still get through -- otherwise the assertion above would pass for the
    // uninteresting reason that nothing wakes this task at all.
    assert!(task_chan::put_int(&victim_park, 7), "could not release the victim");
    let deadline = Instant::now() + Duration::from_secs(10);
    while escaped.load(Ordering::SeqCst) == i64::MIN && Instant::now() < deadline {
        std::thread::yield_now();
    }
    assert_eq!(
        escaped.load(Ordering::SeqCst),
        7,
        "the victim did not resume on a REAL put -- the ABA assertion above proved nothing"
    );
}
