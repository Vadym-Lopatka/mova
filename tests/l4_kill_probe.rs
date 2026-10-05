//! **P5a — kill-at-park soundness** (docs/L4-SUPERVISION-DESIGN.md §3.5, wall
//! W7, probe plan §6). **Always-on since L4 W3**: the machinery it tests is
//! no longer feature-gated, so this file is the standing regression net for
//! the kill primitive rather than a probe you have to opt into. Run it with
//! `cargo test --release --test l4_kill_probe -- --test-threads=1`
//! (the `--test-threads=1` requirement is unchanged -- see below).
//!
//! What is on trial: the claim that a task PARKED on a chan op can be
//! destroyed by its own shard **without resuming into user code**, by
//! force-unwinding its coroutine, and that this is sound. The mechanism under
//! test is `runtime::TaskWaker::kill` (the `PARKED -> KILLED` CAS) +
//! the shard's `Job::Kill` arm (`runtime::kill_task`, which calls
//! `corosensei::Coroutine::force_unwind` and then recycles the stack).
//!
//! This is a PROBE. Nothing here is a landing, and a clean REFUTATION is a
//! successful outcome.
//!
//! ## Pre-registered decision rule
//!
//! Written here before the first test run, verbatim from the design doc §6:
//!
//! > *force-unwind kill LANDS iff (1) 100k kill/park race loops with a
//! > committing peer show zero lost, zero duplicated, zero double-committed
//! > messages; (2) destructors verifiably run (drop-counter guard on the
//! > coroutine stack); (3) no mutex poisoning observed with parks-hold-no-locks
//! > asserted; (4) RSS flat over the loop (stacks recycled). Any failure →
//! > cooperative-cancel fallback, and the wake-path load goes through the
//! > standard A/B gate.*
//!
//! ## Pre-registered decision rule — P5a-bis (the tombstone)
//!
//! P5a refuted clause (1) for one polarity, and the principal ruled a fix
//! that supersedes both fallbacks the design doc offered: **the kill claims
//! the corpse's commit cells.** After winning `PARKED -> KILLED` the killer
//! CASes `put_commit` `PUT_WAITING -> PUT_KILLED` and writes a `TakeSlot`
//! tombstone under the cell mutex, so the deliverers' EXISTING commit sites
//! arbitrate against it on words they already write. Pre-registered here
//! before the first P5a-bis run:
//!
//! > *Tombstone LANDS iff killed-taker LOST == 0 over 100k raced rounds AND
//! > black-hole 512/512 puts survive AND killed-putter posthumous deliveries
//! > == 0 AND clauses (2)(3)(4) of the original rule still hold.*
//!
//! ## The tests, and which clause each one answers
//!
//! - **A** `kill_a_parked_task_runs_destructors_and_never_resumes_it` — clause
//!   (2), plus "the shard survives and keeps working".
//! - **B** `w7_commit_race_putter_killed` / `w7_commit_race_taker_killed` —
//!   clause (1). The W7 window, both polarities, `L4_KILL_ITERS` rounds each
//!   (default 100 000). Post-tombstone these ASSERT zero loss and zero
//!   posthumous delivery rather than counting them.
//! - **C** `kill_cycles_are_resource_flat` — clause (4).
//! - **D** `kill_storm_*` — clause (3) (asserted directly on
//!   `Chan::state.is_poisoned()`) plus the black-hole bar: 512 killed takers,
//!   then 512 puts that must ALL reach a live taker or the buffer.
//! - **E** `killed_*` — two DETERMINISTIC tests, no race, that pin the two
//!   mechanisms P5a found and P5a-bis closed. They are why B's numbers can be
//!   read as a mechanism rather than as noise.
//! - **F** `force_unwound_destructor_*` — F4's law: destructors on a
//!   force-unwound stack see `in_task() == false`, and NON-PARKING chan ops
//!   from that context work correctly.
//!
//! **Run this file with `--test-threads=1 --include-ignored`.** Every WAIT is
//! keyed to a per-task drop flag and is parallel-safe, but test C's claim is
//! about PROCESS-global resource counters (`stacks_mmaped`, `tasks_killed`),
//! so it is meaningless if another test is spawning tasks beside it — it is
//! therefore `#[ignore]`d (see its own doc) and the other seven run in the
//! default suite either way.
//!
//! Every wait in this file is deadline-bounded and every test is watchdogged:
//! the failure mode this probe hunts is a HANG (a lost wake, a wedged shard, a
//! forced unwind that never terminates), and a hang inside `cargo test`
//! reports nothing. Budgets are seconds against microsecond work.
//!
//! ## RESULTS — see the `RESULTS` banner at the foot of this file
//!
//! P5a headline: **LANDS-WITH-CAVEATS as a runtime primitive, REFUTED as a
//! message-preserving one.** Clauses (2), (3) and (4) passed outright and the
//! CAS admitted exactly one winner in 700 000 raced rounds; clause (1) failed
//! on ONE polarity — killing a task parked in `<!` lost messages, at ~30%
//! under a contested race and at 100% deterministically thereafter.
//!
//! P5a-bis headline: **the tombstone LANDS** against the rule above.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mova::internal::task_chan::{alts_take2_int, chan, close, put_int, take_int, BufferPolicy, Chan};
use mova::runtime::{self, TaskWaker};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// Aborts the process with a diagnostic if the guarded section overruns.
/// Same shape as `tests/l2_direct_switch_test.rs`'s.
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
                "P5a WATCHDOG: {what} did not finish in {secs}s. For this probe that is EVIDENCE, \
                 not a flake: a forced unwind that never terminated, a wedged shard, or a lost wake."
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

fn await_until(what: &str, budget: Duration, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        if cond() {
            return;
        }
        std::thread::yield_now();
    }
    panic!("P5a: timed out waiting for {what}");
}

/// Resident set size in KiB, straight out of `ps` — no dev-dependency needed
/// and no claim of precision beyond "did this grow by megabytes".
fn rss_kib() -> u64 {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .expect("ps");
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap_or(0)
}

/// How many entries — live or corpse — are queued in this chan's
/// `task_takers`. `Chan::state` and `ChanState::task_takers` are both `pub`,
/// so the probe can see the queue the deliverers walk.
fn task_takers_len(ch: &Chan) -> usize {
    ch.state.lock().unwrap_or_else(|e| e.into_inner()).task_takers.len()
}

fn iters(default: usize) -> usize {
    std::env::var("L4_KILL_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// A guard that lives on the COROUTINE's stack across the park. Its `Drop` is
/// the whole of clause (2): if the forced unwind does not run it, the kill is
/// leaking every resource a task holds.
struct DropGuard {
    dropped: Arc<AtomicUsize>,
    /// `runtime::in_task()` as observed from INSIDE the forced unwind. §3.5
    /// says nothing about this; see the results block.
    saw_in_task: Arc<AtomicBool>,
}

impl Drop for DropGuard {
    fn drop(&mut self) {
        self.saw_in_task.store(runtime::in_task(), Ordering::SeqCst);
        self.dropped.fetch_add(1, Ordering::SeqCst);
    }
}

/// A minimal drop guard whose only job is to say "THIS task's stack has
/// finished unwinding".
///
/// Every wait in this file keys off one of these rather than off
/// `runtime::tasks_killed()`. That counter is process-global, so under
/// `cargo test`'s default thread-per-test it is satisfied by some OTHER
/// test's kill and the waiter proceeds before its own task has unwound —
/// which is a harness bug, not a runtime one, but it produced two spurious
/// failures before this existed.
struct DeathFlag(Arc<AtomicBool>);

impl Drop for DeathFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// Spin `kill` until it claims the task or `give_up` says stop.
/// A `false` return is not a failure — it means the task was not PARKED at
/// that instant, which is the CAS doing its job.
fn kill_until(waker: &TaskWaker, mut give_up: impl FnMut() -> bool) -> bool {
    loop {
        if waker.kill() {
            return true;
        }
        if give_up() {
            return false;
        }
        std::hint::spin_loop();
    }
}

// ---------------------------------------------------------------------------
// Test A — destructors run, user code never resumes, the shard survives
// ---------------------------------------------------------------------------

#[test]
fn kill_a_parked_task_runs_destructors_and_never_resumes_it() {
    let _wd = Watchdog::arm(60, "test A");

    let ch = chan(BufferPolicy::Unbuffered);
    let dropped = Arc::new(AtomicUsize::new(0));
    let saw_in_task = Arc::new(AtomicBool::new(true));
    let resumed_into_user_code = Arc::new(AtomicBool::new(false));
    let (wtx, wrx) = channel::<TaskWaker>();

    {
        let ch = ch.clone();
        let dropped = dropped.clone();
        let saw_in_task = saw_in_task.clone();
        let resumed = resumed_into_user_code.clone();
        runtime::spawn_on(0, move || {
            let _g = DropGuard { dropped, saw_in_task };
            wtx.send(runtime::current_waker()).expect("publish waker");
            // Parks forever: nothing will ever put on this chan and nothing
            // closes it. The only way past this line is a resume.
            let _ = take_int(&ch);
            resumed.store(true, Ordering::SeqCst);
        });
    }

    let waker = wrx.recv_timeout(Duration::from_secs(10)).expect("waker");
    let killed_before = runtime::tasks_killed();

    let claimed = kill_until(&waker, || false);
    assert!(claimed, "the CAS must eventually find the task PARKED");

    // Wait on THIS task's own guard, never on the process-global counter —
    // then, and only then, on the counter. The order matters: the guard's
    // `Drop` runs INSIDE `force_unwind`, which is strictly before
    // `kill_task` bumps `TASKS_KILLED`, so the flag is the earlier edge.
    await_until("the stack to unwind", Duration::from_secs(10), || {
        dropped.load(Ordering::SeqCst) > 0
    });
    await_until("the shard to count the kill", Duration::from_secs(10), || {
        runtime::tasks_killed() > killed_before
    });

    assert_eq!(dropped.load(Ordering::SeqCst), 1, "clause (2): the stack guard's Drop must run");
    assert!(
        !resumed_into_user_code.load(Ordering::SeqCst),
        "the task must NEVER resume into user code"
    );
    assert!(waker.is_terminal(), "state must be terminal after the kill");
    assert!(
        runtime::tasks_killed() > killed_before,
        "the kill must be counted on the shard"
    );

    // §3.5 says nothing about the yielder. Record what actually happens, and
    // fail loudly if it ever changes so the landing spec stays honest.
    assert!(
        !saw_in_task.load(Ordering::SeqCst),
        "FINDING (recorded as an assertion so a future change is loud): destructors on a \
         force-unwound stack run with in_task() == FALSE, because the shard zeroed \
         ExecTls::yielder at the park this task never returned from"
    );

    // The shard must still work. Same shard, same slab, recycled stack.
    let (dtx, drx) = channel::<i64>();
    let ch2 = chan(BufferPolicy::Unbuffered);
    {
        let ch2 = ch2.clone();
        runtime::spawn_on(0, move || {
            dtx.send(take_int(&ch2).unwrap_or(-1)).ok();
        });
    }
    assert!(put_int(&ch2, 4242), "put onto the post-kill task");
    assert_eq!(drx.recv_timeout(Duration::from_secs(10)).expect("post-kill task ran"), 4242);
}

// ---------------------------------------------------------------------------
// Test B — the W7 commit race
// ---------------------------------------------------------------------------

/// One round's shared state, handed to the killer and the peer.
struct Round {
    ch: Arc<Chan>,
    bail: Arc<Chan>,
    waker: TaskWaker,
    task_done: Arc<AtomicBool>,
    armed: Arc<AtomicUsize>,
    payload: i64,
    /// Spins the KILLER burns after the gate before its first CAS attempt.
    /// Without it the killer — a tight CAS loop — beats a peer that has to
    /// take a chan lock every single time, and the probe measures one arm of
    /// the race 99.9% of the time (measured: 45 / 100 000 the other way).
    /// Sweeping the killer's arrival across the peer's whole critical
    /// section is what makes the split evidence instead of an artifact.
    jitter: u32,
    /// Set by the peer immediately before it enters its chan op.
    peer_started: Arc<AtomicBool>,
    /// Set by the KILLER, at the instant its `PARKED -> KILLED` CAS succeeds,
    /// to `!peer_started`. It separates the two causes that otherwise look
    /// identical from outside:
    /// `true` = the task was already claimed-dead before the peer had even
    ///          entered its op, so whatever the peer transacted next went
    ///          through a STALE WAITER (test E's mechanism);
    /// `false` = the peer was already inside its critical section, so this is
    ///          the §3.5 pre-`wake()` commit window (or a stale waiter met by
    ///          a slow peer — the flag is a tight LOWER BOUND on the stale
    ///          path, never an upper one).
    kill_before_peer: Arc<AtomicBool>,
}

/// Both racers arm, then spin until BOTH are armed, then go. Without this the
/// killer always wins (it starts while the peer is still being dispatched) and
/// the probe would measure nothing.
fn race_gate(armed: &AtomicUsize) {
    armed.fetch_add(1, Ordering::SeqCst);
    while armed.load(Ordering::SeqCst) < 2 {
        std::hint::spin_loop();
    }
}

/// Deterministic per-round killer jitter, swept LOGARITHMICALLY over
/// 0..4095 spins (~0..4 µs) in 13 buckets.
///
/// A single scale is worthless here and both extremes were measured before
/// this function existed: at a flat 0 the killer's tight CAS loop wins
/// 99.9% of rounds, at a flat 0..4095 the peer wins 99.4%. The crossover is
/// where the race actually lives, so the sweep gives every bucket ~n/13
/// rounds and the split below is the union of all of them — every arm of the
/// race is exercised at five figures.
fn jitter_for(i: usize) -> u32 {
    let bucket = (i % 13) as u32;
    let span = 1u32 << bucket;
    (((i as u64).wrapping_mul(2_654_435_761) >> 13) as u32) % span
}

#[derive(Default, Debug)]
struct Split {
    /// The peer's commit landed and the task lived to see it: the waker won
    /// the `PARKED -> READY` CAS, `kill` returned `false` forever.
    commit_won: u64,
    /// The kill CAS won and nothing was delivered on this chan at all.
    kill_won: u64,
    /// The kill CAS won AND the value still moved. Sub-split below.
    killed_but_value_moved: u64,
    /// ...of which: the peer started AFTER the kill was already claimed, so
    /// it transacted with a stale waiter left on the chan by a dead task.
    via_stale_waiter: u64,
    /// ...of which: the peer was already inside its critical section, so the
    /// commit and the kill genuinely raced (the §3.5 "committed, then killed"
    /// window).
    via_commit_window: u64,
    /// Messages a putter was told it delivered that NO taker ever received.
    lost: u64,
    /// Messages delivered to a taker whose putter was already dead — nothing
    /// is lost, but a killed proc's last message still lands downstream.
    posthumous: u64,
}

/// **W7, putter polarity.** A task parks inside `>!` on an unbuffered chan
/// with its value in `task_putters`. An OS-thread taker (`alts!!` over the
/// chan and a bail chan, so it can never hang) races a killer.
/// The `unused_assignments` allow is deliberate: the failure classes below
/// bump their counter and THEN panic, so the counter is never read on that
/// path. Keeping the bump means the class is still named at the point it is
/// detected -- the panic message and the struct field say the same thing,
/// and W3's de-gating made these warnings visible on every `cargo test`,
/// which is a reason to explain them, not to delete the bookkeeping.
#[allow(unused_assignments)]
#[test]
fn w7_commit_race_putter_killed() {
    let n = iters(100_000);
    let _wd = Watchdog::arm(900, "test B (putter polarity)");

    // Persistent racer threads: one OS-thread spawn per round would cost more
    // than the race it measures.
    let (kill_tx, kill_rx) = channel::<Round>();
    let (kill_rep_tx, kill_rep_rx) = channel::<bool>();
    let killer = std::thread::spawn(move || {
        while let Ok(r) = kill_rx.recv() {
            race_gate(&r.armed);
            for _ in 0..r.jitter {
                std::hint::spin_loop();
            }
            let killed = kill_until(&r.waker, || r.task_done.load(Ordering::SeqCst));
            if killed {
                r.kill_before_peer
                    .store(!r.peer_started.load(Ordering::SeqCst), Ordering::SeqCst);
            }
            // Release the taker no matter who won.
            close(&r.bail);
            kill_rep_tx.send(killed).expect("kill report");
        }
    });

    let (take_tx, take_rx) = channel::<Round>();
    let (take_rep_tx, take_rep_rx) = channel::<Option<i64>>();
    let taker = std::thread::spawn(move || {
        while let Ok(r) = take_rx.recv() {
            race_gate(&r.armed);
            r.peer_started.store(true, Ordering::SeqCst);
            take_rep_tx.send(alts_take2_int(&r.ch, &r.bail)).expect("take report");
        }
    });

    let mut split = Split::default();
    for i in 0..n {
        let payload = i as i64 + 1;
        let ch = chan(BufferPolicy::Unbuffered);
        let bail = chan(BufferPolicy::Unbuffered);
        let task_done = Arc::new(AtomicBool::new(false));
        // -2 = the task never returned; otherwise 1/0 for the `>!` result.
        let put_res = Arc::new(AtomicI64::new(-2));
        let armed = Arc::new(AtomicUsize::new(0));
        let peer_started = Arc::new(AtomicBool::new(false));
        let kill_before_peer = Arc::new(AtomicBool::new(false));
        let (wtx, wrx) = channel::<TaskWaker>();

        {
            let ch = ch.clone();
            let done = task_done.clone();
            let res = put_res.clone();
            runtime::spawn(move || {
                wtx.send(runtime::current_waker()).expect("publish waker");
                let ok = put_int(&ch, payload);
                res.store(i64::from(ok), Ordering::SeqCst);
                done.store(true, Ordering::SeqCst);
            });
        }
        let waker = wrx.recv_timeout(Duration::from_secs(30)).expect("waker");

        let mk = |waker: TaskWaker| Round {
            ch: ch.clone(),
            bail: bail.clone(),
            waker,
            task_done: task_done.clone(),
            armed: armed.clone(),
            payload,
            jitter: jitter_for(i),
            peer_started: peer_started.clone(),
            kill_before_peer: kill_before_peer.clone(),
        };
        kill_tx.send(mk(waker.clone())).expect("dispatch killer");
        take_tx.send(mk(waker)).expect("dispatch taker");

        let killed = kill_rep_rx.recv_timeout(Duration::from_secs(30)).expect("kill report");
        let taken = take_rep_rx.recv_timeout(Duration::from_secs(30)).expect("take report");
        let pr = put_res.load(Ordering::SeqCst);
        let stale = kill_before_peer.load(Ordering::SeqCst);

        // No duplication and no cross-round leakage: the payload is unique
        // per round and the chans are fresh.
        if let Some(v) = taken {
            assert_eq!(v, payload, "round {i}: a DIFFERENT round's value arrived");
        }

        match (killed, taken, pr) {
            // Kill lost the CAS: the task ran to completion and reported a
            // successful put, and the taker holds the value.
            (false, Some(_), 1) => split.commit_won += 1,
            (false, t, p) => panic!(
                "round {i}: kill lost the race but the round did not complete cleanly \
                 (taken={t:?}, put_res={p})"
            ),
            // Kill won outright: nothing was delivered, and the task never
            // returned from `>!`.
            (true, None, -2) => split.kill_won += 1,
            // P5a: the task was killed and the value STILL reached the
            // taker — a POSTHUMOUS delivery (52 174/100 000 pre-tombstone).
            // P5a-bis: the `PUT_KILLED` claim means the corpse's value is
            // dropped by `take_from_task_putter_cold` instead of delivered,
            // so this class must now be EMPTY.
            (true, Some(_), -2) => {
                split.killed_but_value_moved += 1;
                split.posthumous += 1;
                if stale {
                    split.via_stale_waiter += 1;
                } else {
                    split.via_commit_window += 1;
                }
                panic!(
                    "round {i}: POSTHUMOUS DELIVERY — the putter was killed with an unclaimed \
                     commit cell and its value was delivered anyway (stale-waiter path: {stale})"
                );
            }
            (true, t, p) => panic!(
                "round {i}: the task returned from `>!` AND was killed \
                 (taken={t:?}, put_res={p}) — the CAS admitted two winners"
            ),
        }
    }
    drop(kill_tx);
    drop(take_tx);
    killer.join().expect("killer");
    taker.join().expect("taker");

    eprintln!(
        "P5a-bis/B putter-killed, {n} rounds: {split:#?} (kills salvaged process-wide: {})",
        runtime::kills_salvaged()
    );
    assert_eq!(split.commit_won + split.kill_won + split.killed_but_value_moved, n as u64);
    assert_eq!(split.lost, 0, "clause (1): killing a parked PUTTER must lose no message");
    assert_eq!(
        split.posthumous, 0,
        "P5a-bis bar: a killed putter's value must die with it, never be delivered posthumously"
    );
    assert!(
        split.kill_won + split.killed_but_value_moved > 0,
        "the CAS never went the killer's way — the jitter sweep is broken"
    );
    assert!(split.commit_won > 0, "the race never went the peer's way — the gate is broken");
}

/// **W7, taker polarity.** A task parks inside `<!` (a `TakerWaiter` with its
/// one-shot cell in `task_takers`); an OS-thread putter races a killer.
/// The `unused_assignments` allow is deliberate: the failure classes below
/// bump their counter and THEN panic, so the counter is never read on that
/// path. Keeping the bump means the class is still named at the point it is
/// detected -- the panic message and the struct field say the same thing,
/// and W3's de-gating made these warnings visible on every `cargo test`,
/// which is a reason to explain them, not to delete the bookkeeping.
#[allow(unused_assignments)]
#[test]
fn w7_commit_race_taker_killed() {
    let n = iters(100_000);
    let _wd = Watchdog::arm(900, "test B (taker polarity)");

    let (kill_tx, kill_rx) = channel::<Round>();
    let (kill_rep_tx, kill_rep_rx) = channel::<bool>();
    let killer = std::thread::spawn(move || {
        while let Ok(r) = kill_rx.recv() {
            race_gate(&r.armed);
            for _ in 0..r.jitter {
                std::hint::spin_loop();
            }
            let killed = kill_until(&r.waker, || r.task_done.load(Ordering::SeqCst));
            if killed {
                r.kill_before_peer
                    .store(!r.peer_started.load(Ordering::SeqCst), Ordering::SeqCst);
            }
            kill_rep_tx.send(killed).expect("kill report");
        }
    });

    let (put_tx, put_rx) = channel::<Round>();
    let (put_rep_tx, put_rep_rx) = channel::<bool>();
    let putter = std::thread::spawn(move || {
        while let Ok(r) = put_rx.recv() {
            race_gate(&r.armed);
            r.peer_started.store(true, Ordering::SeqCst);
            put_rep_tx.send(put_int(&r.ch, r.payload)).expect("put report");
        }
    });

    let mut split = Split::default();
    for i in 0..n {
        let payload = i as i64 + 1;
        let ch = chan(BufferPolicy::Unbuffered);
        let bail = chan(BufferPolicy::Unbuffered); // unused in this polarity
        let task_done = Arc::new(AtomicBool::new(false));
        // -2 = never returned, -1 = returned nil (chan closed), else the value.
        let take_res = Arc::new(AtomicI64::new(-2));
        let armed = Arc::new(AtomicUsize::new(0));
        let peer_started = Arc::new(AtomicBool::new(false));
        let kill_before_peer = Arc::new(AtomicBool::new(false));
        let (wtx, wrx) = channel::<TaskWaker>();

        {
            let ch = ch.clone();
            let done = task_done.clone();
            let res = take_res.clone();
            runtime::spawn(move || {
                wtx.send(runtime::current_waker()).expect("publish waker");
                let v = take_int(&ch);
                res.store(v.unwrap_or(-1), Ordering::SeqCst);
                done.store(true, Ordering::SeqCst);
            });
        }
        let waker = wrx.recv_timeout(Duration::from_secs(30)).expect("waker");

        let mk = |waker: TaskWaker| Round {
            ch: ch.clone(),
            bail: bail.clone(),
            waker,
            task_done: task_done.clone(),
            armed: armed.clone(),
            payload,
            jitter: jitter_for(i),
            peer_started: peer_started.clone(),
            kill_before_peer: kill_before_peer.clone(),
        };
        kill_tx.send(mk(waker.clone())).expect("dispatch killer");
        put_tx.send(mk(waker)).expect("dispatch putter");

        let killed = kill_rep_rx.recv_timeout(Duration::from_secs(30)).expect("kill report");
        // Release a putter that parked because the taker died before it could
        // register anything to deliver into.
        close(&ch);
        let put_ok = put_rep_rx.recv_timeout(Duration::from_secs(30)).expect("put report");
        let tr = take_res.load(Ordering::SeqCst);
        let stale = kill_before_peer.load(Ordering::SeqCst);

        match (killed, put_ok, tr) {
            (false, true, v) if v == payload => split.commit_won += 1,
            (false, ..) => panic!("round {i}: kill lost but the take did not deliver ({tr})"),
            // P5a: killed, and the put was nonetheless told it SUCCEEDED —
            // the value is GONE (30 400/100 000 pre-tombstone). P5a-bis: the
            // `TakeSlot` tombstone makes `deliver_to_task_taker_cold` cull
            // the corpse instead of committing into it, so this class must
            // now be EMPTY.
            (true, true, -2) => {
                split.killed_but_value_moved += 1;
                split.lost += 1;
                if stale {
                    split.via_stale_waiter += 1;
                } else {
                    split.via_commit_window += 1;
                }
                panic!(
                    "round {i}: MESSAGE SWALLOWED — the taker was killed, the put was told it \
                     succeeded, and nobody received it (stale-waiter path: {stale})"
                );
            }
            // The kill won and the put found no live consumer: it parked in
            // `task_putters` and `close(ch)` released it with `false`. That
            // is NORMAL unbuffered semantics for "no consumer", i.e. the
            // required "never swallowed" outcome.
            (true, false, -2) => split.kill_won += 1,
            (true, ..) => panic!("round {i}: killed AND the take returned ({tr})"),
        }
    }
    drop(kill_tx);
    drop(put_tx);
    killer.join().expect("killer");
    putter.join().expect("putter");

    eprintln!(
        "P5a-bis/B taker-killed, {n} rounds: {split:#?} (kills salvaged process-wide: {})",
        runtime::kills_salvaged()
    );
    assert_eq!(split.commit_won + split.kill_won + split.killed_but_value_moved, n as u64);
    assert!(split.commit_won > 0, "the race never went the peer's way — the gate is broken");
    assert!(
        split.kill_won > 0,
        "the CAS never went the killer's way — the jitter sweep is broken"
    );
    assert_eq!(
        split.lost, 0,
        "P5a-bis BAR: killed-taker LOST must be 0 — every put raced with a taker-kill must \
         reach a live taker, reach the buffer, or return per normal no-consumer semantics"
    );
}

// ---------------------------------------------------------------------------
// Test C — resource flatness
// ---------------------------------------------------------------------------

/// Clause (4), and gate G-KILL's resource half. Kill/respawn cycles on ONE
/// shard, serialized, so the steady state needs exactly one live 8 MiB stack:
/// `stacks_mmaped()` must plateau and RSS must not run away.
///
/// **`#[ignore]`d, and not for flakiness.** Its claim is about PROCESS-global
/// counters (`stacks_mmaped`, `tasks_killed`, `tasks_spawned`), so it is
/// meaningless — and fails outright — if anything else in the binary is
/// spawning tasks beside it. That was invisible while this file was
/// feature-gated (a plain `cargo test` never built it); since L4 W3 made the
/// kill machinery default and this file an always-on regression net, a plain
/// `cargo test` DOES run it, thread-per-test, which is exactly the condition
/// it cannot measure under. The other seven tests are parallel-safe (every
/// wait is keyed to a per-task drop flag) and stay in the default run.
///
/// Run it — and the whole file, the way the gate means it — with:
/// `cargo test --release --test l4_kill_probe -- --test-threads=1 --include-ignored`
#[test]
#[ignore = "process-global resource counters: needs --test-threads=1, see the doc above"]
fn kill_cycles_are_resource_flat() {
    // The G-KILL bar is "≥10k cycles" — `L4_KILL_ITERS` may raise it, never
    // lower it below what the gate asks for.
    let n = iters(10_000).max(10_000);
    let _wd = Watchdog::arm(600, "test C");

    // Warm up so the pool and the slab are at their steady shape before the
    // measurement window opens.
    for _ in 0..64 {
        one_kill_cycle(1);
    }
    let stacks_before = runtime::stacks_mmaped();
    let rss_before = rss_kib();
    let killed_before = runtime::tasks_killed();
    let leaked_before = runtime::kill_stacks_leaked();
    let salvaged_before = runtime::kills_salvaged();
    let spawned_before = runtime::tasks_spawned();

    let t0 = Instant::now();
    for _ in 0..n {
        one_kill_cycle(1);
    }
    let elapsed = t0.elapsed();
    // The last cycle's `DeathFlag` fires inside `force_unwind`, i.e. before
    // its shard bumps `TASKS_KILLED`; let the bookkeeping land.
    await_until("the final kill to be counted", Duration::from_secs(30), || {
        runtime::tasks_killed() - killed_before == n as u64
    });

    // `stacks_mmaped`/`tasks_killed` are PROCESS-global, so this test's
    // resource claim is only meaningful if nothing else in the binary was
    // spawning tasks alongside it. Say so plainly rather than failing on a
    // number that means nothing.
    assert_eq!(
        runtime::tasks_spawned() - spawned_before,
        n as u64,
        "P5a/C measures PROCESS-global stack and kill counters, so it must run alone: \
         `cargo test --release --test l4_kill_probe -- --test-threads=1`"
    );

    let stacks_after = runtime::stacks_mmaped();
    let rss_after = rss_kib();
    let d_stacks = stacks_after - stacks_before;
    let d_rss = rss_after as i64 - rss_before as i64;
    let d_leaked = runtime::kill_stacks_leaked() - leaked_before;

    eprintln!(
        "P5a/C {n} kill cycles on shard 1: {:?} total ({:.2} µs/cycle); \
         stacks_mmaped {stacks_before} -> {stacks_after} (Δ{d_stacks}); \
         RSS {rss_before} -> {rss_after} KiB (Δ{d_rss}); stacks leaked Δ{d_leaked}",
        elapsed,
        elapsed.as_secs_f64() * 1e6 / n as f64
    );

    assert_eq!(
        runtime::tasks_killed() - killed_before,
        n as u64,
        "every cycle must have been killed"
    );
    assert_eq!(
        runtime::kills_salvaged(),
        salvaged_before,
        "nothing here races a commit, so no kill should have been salvaged"
    );
    assert_eq!(d_leaked, 0, "clause (4): no forced unwind may lose its stack");
    assert!(
        d_stacks <= 4,
        "clause (4): {n} serialized kill cycles mmap'ed {d_stacks} new stacks — \
         force-unwind is NOT returning stacks to the pool"
    );
    assert!(
        d_rss < 64 * 1024,
        "clause (4): RSS grew {d_rss} KiB over {n} kill cycles"
    );
}

/// Spawn one task on `shard`, let it park on an empty chan, kill it, and wait
/// for the shard to finish the job.
fn one_kill_cycle(shard: usize) {
    let ch = chan(BufferPolicy::Unbuffered);
    let dead = Arc::new(AtomicBool::new(false));
    let (wtx, wrx) = channel::<TaskWaker>();
    {
        let ch = ch.clone();
        let dead = dead.clone();
        runtime::spawn_on(shard, move || {
            let _d = DeathFlag(dead);
            wtx.send(runtime::current_waker()).expect("publish waker");
            let _ = take_int(&ch);
        });
    }
    let waker = wrx.recv_timeout(Duration::from_secs(30)).expect("waker");
    assert!(kill_until(&waker, || false));
    await_until("kill to complete", Duration::from_secs(30), || {
        dead.load(Ordering::SeqCst)
    });
}

// ---------------------------------------------------------------------------
// Test D — no poisoning, chans stay usable
// ---------------------------------------------------------------------------

/// Clause (3) **and the P5a-bis black-hole bar.**
///
/// §3.5's poisoning argument is "parks never hold chan mutexes (established
/// law), so force-unwind at a park point cannot poison". `Chan::state` is a
/// plain `std::sync::Mutex`, so the claim is directly observable — the whole
/// crate reads through `sync::lock_mutex`, which recovers from poison
/// silently, so a behavioural test alone could never see it.
///
/// The black-hole half is the P5a refutation turned into a bar: 512 killed
/// takers, then 512 puts, every one of which must reach a live taker or the
/// buffer. Pre-tombstone this test measured 512/512 puts SWALLOWED.
#[test]
fn kill_storm_leaves_shared_chans_usable_and_unpoisoned() {
    const CHANS: usize = 64;
    const PER_CHAN: usize = 8;
    let _wd = Watchdog::arm(300, "test D");

    // Half the storm on unbuffered chans (the put must find a LIVE taker),
    // half on `Fixed(8)` (the put must reach the BUFFER). Those are the two
    // legal destinations the bar names, and the two different fall-through
    // paths in `chan_put` behind `deliver_to_task_taker`.
    let unbuf: Vec<Arc<Chan>> = (0..CHANS / 2).map(|_| chan(BufferPolicy::Unbuffered)).collect();
    let buffered: Vec<Arc<Chan>> =
        (0..CHANS / 2).map(|_| chan(BufferPolicy::Fixed(PER_CHAN))).collect();
    let all: Vec<Arc<Chan>> = unbuf.iter().chain(buffered.iter()).cloned().collect();

    let wakers = Arc::new(Mutex::new(Vec::<TaskWaker>::new()));
    let parked = Arc::new(AtomicUsize::new(0));
    let dead = Arc::new(AtomicUsize::new(0));
    struct StormDeath(Arc<AtomicUsize>);
    impl Drop for StormDeath {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    for ch in &all {
        for _ in 0..PER_CHAN {
            let ch = ch.clone();
            let wakers = wakers.clone();
            let parked = parked.clone();
            let dead = dead.clone();
            runtime::spawn(move || {
                let _d = StormDeath(dead);
                wakers.lock().expect("wakers").push(runtime::current_waker());
                parked.fetch_add(1, Ordering::SeqCst);
                let _ = take_int(&ch);
            });
        }
    }
    await_until("all tasks to register", Duration::from_secs(60), || {
        parked.load(Ordering::SeqCst) == CHANS * PER_CHAN
    });

    let ws = std::mem::take(&mut *wakers.lock().expect("wakers"));
    assert_eq!(ws.len(), CHANS * PER_CHAN);
    for w in &ws {
        assert!(kill_until(w, || false), "every parked task must be killable");
    }
    await_until("the storm to drain", Duration::from_secs(120), || {
        dead.load(Ordering::SeqCst) == CHANS * PER_CHAN
    });

    // Clause (3), directly.
    for (i, ch) in all.iter().enumerate() {
        assert!(!ch.state.is_poisoned(), "chan {i}'s state mutex is POISONED after the kill storm");
    }

    // --- black hole, buffered half: 8 puts per chan must all reach the
    // buffer, past 8 corpse waiters each, and read back exactly.
    let mut survived = 0u64;
    for (i, ch) in buffered.iter().enumerate() {
        for k in 0..PER_CHAN {
            assert!(put_int(ch, (i * 100 + k) as i64), "chan {i} put {k} must reach the buffer");
            survived += 1;
        }
    }
    for (i, ch) in buffered.iter().enumerate() {
        for k in 0..PER_CHAN {
            assert_eq!(
                take_int(ch),
                Some((i * 100 + k) as i64),
                "chan {i}: buffered value {k} did not survive the kill storm"
            );
        }
    }

    // --- black hole, unbuffered half: PER_CHAN LIVE takers queued BEHIND the
    // PER_CHAN corpses, then PER_CHAN puts. The corpses sit at the front of
    // `task_takers` (FIFO), so this is the exact shape that used to swallow
    // every message.
    for (i, ch) in unbuf.iter().enumerate() {
        let (tx, rx) = channel::<i64>();
        for _ in 0..PER_CHAN {
            let c = ch.clone();
            let tx = tx.clone();
            runtime::spawn(move || {
                tx.send(take_int(&c).unwrap_or(-1)).ok();
            });
        }
        drop(tx);
        await_until("the live takers to register", Duration::from_secs(30), || {
            task_takers_len(ch) == 2 * PER_CHAN
        });
        for k in 0..PER_CHAN {
            assert!(put_int(ch, (7000 + i * 100 + k) as i64), "chan {i} put {k} after the storm");
            survived += 1;
        }
        let mut got: Vec<i64> = (0..PER_CHAN)
            .map(|_| rx.recv_timeout(Duration::from_secs(30)).expect("post-storm live taker"))
            .collect();
        got.sort_unstable();
        let want: Vec<i64> = (0..PER_CHAN).map(|k| (7000 + i * 100 + k) as i64).collect();
        assert_eq!(
            got, want,
            "chan {i}: a put was swallowed by a corpse instead of reaching a LIVE taker"
        );
        assert!(!ch.state.is_poisoned());
    }

    eprintln!(
        "P5a-bis/D {} killed takers, {survived} subsequent puts — all reached a live taker or \
         the buffer (pre-tombstone: 512/512 swallowed)",
        ws.len()
    );
    assert_eq!(survived, (CHANS * PER_CHAN) as u64, "the bar is 512 surviving puts");
}

// ---------------------------------------------------------------------------
// Test E — the two mechanisms P5a found, now asserted CLOSED
// ---------------------------------------------------------------------------

/// **E1.** Kill a task parked inside `>!`. Its `PutterWaiter` — which carries
/// the VALUE, not merely a waker — is still queued on the chan.
///
/// P5a: the next taker RECEIVED that value, posthumously, from a task that was
/// provably already dead. P5a-bis: the `PUT_KILLED` claim makes
/// `take_from_task_putter_cold`'s CAS fail, so the corpse is culled and its
/// value dropped — the taker must find NOTHING.
#[test]
fn killed_putter_value_dies_with_it() {
    let _wd = Watchdog::arm(60, "test E1");
    let ch = chan(BufferPolicy::Unbuffered);
    let (wtx, wrx) = channel::<TaskWaker>();
    let dead = Arc::new(AtomicBool::new(false));
    {
        let ch = ch.clone();
        let dead = dead.clone();
        runtime::spawn_on(2, move || {
            let _d = DeathFlag(dead);
            wtx.send(runtime::current_waker()).expect("publish waker");
            let _ = put_int(&ch, 909_090);
        });
    }
    let waker = wrx.recv_timeout(Duration::from_secs(10)).expect("waker");
    assert!(kill_until(&waker, || false));
    await_until("kill", Duration::from_secs(10), || dead.load(Ordering::SeqCst));

    // The task is provably gone. Now take.
    let bail = chan(BufferPolicy::Unbuffered);
    let b = bail.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        close(&b);
    });
    let got = alts_take2_int(&ch, &bail);
    eprintln!("P5a-bis/E1 take from a chan whose parked putter was killed: {got:?}");
    assert_eq!(
        got, None,
        "P5a-bis bar: a killed putter's uncommitted value must die with it. \
         (P5a measured Some(909090) here — posthumous delivery.)"
    );
}

/// **E2.** Kill a task parked inside `<!`, then put.
///
/// P5a: `deliver_to_task_taker` handed the value into the dead task's
/// `TakerWaiter` cell, told the putter `true`, and nobody ever read it — a
/// black hole, 100% reproducible. P5a-bis: the tombstone makes the deliverer
/// cull the corpse and fall through, so the put must reach the buffer here.
#[test]
fn killed_taker_is_not_a_black_hole() {
    let _wd = Watchdog::arm(60, "test E2");
    // `Fixed(1)`: with the corpse culled the put has somewhere legal to land,
    // which is what makes "not swallowed" observable in one process.
    let ch = chan(BufferPolicy::Fixed(1));
    let (wtx, wrx) = channel::<TaskWaker>();
    let got_value = Arc::new(AtomicI64::new(-2));
    let dead = Arc::new(AtomicBool::new(false));
    {
        let ch = ch.clone();
        let got = got_value.clone();
        let dead = dead.clone();
        runtime::spawn_on(3, move || {
            let _d = DeathFlag(dead);
            wtx.send(runtime::current_waker()).expect("publish waker");
            got.store(take_int(&ch).unwrap_or(-1), Ordering::SeqCst);
        });
    }
    let waker = wrx.recv_timeout(Duration::from_secs(10)).expect("waker");
    assert!(kill_until(&waker, || false));
    await_until("kill", Duration::from_secs(10), || dead.load(Ordering::SeqCst));
    assert_eq!(task_takers_len(&ch), 1, "the corpse waiter is still queued (lazy cull)");

    let ok = put_int(&ch, 121_212);
    eprintln!("P5a-bis/E2 put onto a chan whose parked taker was killed: returned {ok}");
    assert!(ok, "the put must succeed");
    assert_eq!(
        got_value.load(Ordering::SeqCst),
        -2,
        "the dead task must not have received anything"
    );
    assert_eq!(
        take_int(&ch),
        Some(121_212),
        "P5a-bis BAR: the value must have reached the BUFFER, not the corpse's cell. \
         (P5a measured it swallowed here.)"
    );
    assert_eq!(task_takers_len(&ch), 0, "the deliverer culled the corpse on its way past");
}

// ---------------------------------------------------------------------------
// Test F — F4's law: what a destructor on a force-unwound stack may do
// ---------------------------------------------------------------------------

/// An `ExitGuard`-shaped destructor: the §3.5 `DoneCells` role, reduced to
/// what P5a's F4 finding says it is allowed to be.
struct ExitGuard {
    saw_in_task: Arc<AtomicBool>,
    /// `Fixed(4)`, so the put below has buffer space and CANNOT park.
    report: Arc<Chan>,
    put_result: Arc<AtomicI64>,
    closed_ok: Arc<AtomicBool>,
}

impl Drop for ExitGuard {
    fn drop(&mut self) {
        // F4: the shard zeroed `ExecTls::yielder` at the park this task never
        // returned from, and only the task's own stack knows the address, so
        // this runs OUTSIDE task context. Every chan op below therefore takes
        // the OS-thread path — which is exactly why it must not be able to
        // block: a parking op here would `cv_wait` and wedge the shard.
        self.saw_in_task.store(runtime::in_task(), Ordering::SeqCst);
        // Non-parking op 1: a put with buffer space available.
        self.put_result
            .store(i64::from(put_int(&self.report, 555)), Ordering::SeqCst);
        // Non-parking op 2: close.
        close(&self.report);
        self.closed_ok.store(true, Ordering::SeqCst);
    }
}

/// F4's law, pinned: destructors on a force-unwound stack see
/// `in_task() == false`, and NON-PARKING chan ops from that context complete
/// correctly — the put lands, the close lands, and both are observable from
/// outside after the task is gone.
///
/// Deliberately NOT tested: a PARKING op in a destructor. That is outside the
/// law. It would re-enter `Yielder::suspend` during an active forced unwind,
/// which is a panic inside a panic and therefore an abort — the landing spec
/// must state the restriction, not probe past it.
#[test]
fn force_unwound_destructor_may_use_non_parking_chan_ops() {
    let _wd = Watchdog::arm(60, "test F");
    let report = chan(BufferPolicy::Fixed(4));
    let saw_in_task = Arc::new(AtomicBool::new(true));
    let put_result = Arc::new(AtomicI64::new(-2));
    let closed_ok = Arc::new(AtomicBool::new(false));
    let park_on = chan(BufferPolicy::Unbuffered);
    let (wtx, wrx) = channel::<TaskWaker>();

    {
        let (report, saw, pr, ok, park_on) = (
            report.clone(),
            saw_in_task.clone(),
            put_result.clone(),
            closed_ok.clone(),
            park_on.clone(),
        );
        runtime::spawn_on(4, move || {
            let _g = ExitGuard {
                saw_in_task: saw,
                report,
                put_result: pr,
                closed_ok: ok,
            };
            wtx.send(runtime::current_waker()).expect("publish waker");
            let _ = take_int(&park_on);
        });
    }

    let waker = wrx.recv_timeout(Duration::from_secs(10)).expect("waker");
    assert!(kill_until(&waker, || false));
    // `closed_ok` is the LAST thing the guard sets, so it is both this task's
    // death flag and the proof the destructor ran to completion.
    await_until("kill", Duration::from_secs(10), || closed_ok.load(Ordering::SeqCst));

    assert!(
        !saw_in_task.load(Ordering::SeqCst),
        "F4's law: a force-unwound destructor runs with in_task() == false"
    );
    assert_eq!(put_result.load(Ordering::SeqCst), 1, "the non-parking put must have succeeded");
    assert!(closed_ok.load(Ordering::SeqCst), "the destructor must have run to completion");
    assert_eq!(take_int(&report), Some(555), "the exit report must be readable after the kill");
    assert_eq!(take_int(&report), None, "...and the report chan must be closed");
    eprintln!("P5a-bis/F ExitGuard on a force-unwound stack: in_task=false, put+close both OK");
}

// ---------------------------------------------------------------------------
// RESULTS
// ---------------------------------------------------------------------------

// (see also docs/L4-SUPERVISION-DESIGN.md §3.5, wall W7, probe plan §6)
//
// Machine: aarch64-darwin M4 Pro, `cargo test --release --features
// l4-kill-probe -- --test-threads=1`. Runs reproduced 3x; numbers below are
// one representative run, spreads noted where they matter.
//
// VERDICT vs the pre-registered rule
// ---------------------------------
//
// (1) 100k race loops, zero lost / duplicated / double-committed
//     ** SPLIT: passes for a killed PUTTER, FAILS for a killed TAKER. **
//
//     putter-killed, 100 000 rounds:
//       commit_won              47 788   the waker won PARKED->READY
//       kill_won                    38   kill won, nothing moved
//       killed_but_value_moved  52 174   kill won AND the value reached the taker
//       lost                         0
//       posthumous              52 174
//
//     taker-killed, 100 000 rounds:
//       commit_won              69 671
//       kill_won                     1
//       killed_but_value_moved  30 328
//       lost                    30 328   <-- 30.3% of rounds LOSE A MESSAGE
//
//     Zero duplication and zero double-commit in both polarities (each round
//     asserts the payload identity, and the `(true, .., returned)` /
//     `(false, ..)` arms panic if the CAS ever admitted two winners; it never
//     did across ~700k raced rounds over all runs).
//
//     The asymmetry is structural, not statistical:
//       - a killed PUTTER's value is already in the taker's hand when the
//         commit lands (`take_from_task_putter` RETURNS the value), so the
//         kill can only cost the putter its `true` — nothing is lost;
//       - a killed TAKER's value is written INTO the dying task's
//         `TakeSlot` cell (`deliver_to_task_taker`), the putter is told
//         `true`, and nobody ever reads the cell. That is a lost message.
//
// (2) destructors verifiably run                              ** PASS **
//     Test A: a `DropGuard` living across the park is dropped exactly once by
//     the forced unwind, and the task never resumes into user code (the line
//     after `<!` never executes). Test C repeats it 10 000 times.
//
// (3) no mutex poisoning                                      ** PASS **
//     Test D asserts `Chan::state.is_poisoned() == false` on all 64 storm
//     chans after 512 concurrent kills, and drives full task<->thread round
//     trips through every one of them afterwards. This is a DIRECT check, not
//     a behavioural proxy: the crate reads every chan mutex through
//     `sync::lock_mutex`, which recovers from poison silently, so a
//     behavioural test could never see it. Parks-hold-no-locks holds.
//
// (4) RSS flat / stacks recycled                              ** PASS **
//     Test C, 10 000 serialized kill cycles on one shard:
//       stacks_mmaped   2 -> 2   (Δ0)
//       RSS          4480 -> 4560 KiB (Δ80 KiB)
//       kill_stacks_leaked Δ0
//     Zero 8 MiB stacks lost. Per-cycle wall time 28-31 µs, but that is
//     spawn + mpsc waker handshake + a `yield_now` await loop, NOT the cost
//     of a kill; the probe makes no kill-latency claim.
//
// The corosensei API, exactly as used
// -----------------------------------
// `Coroutine::force_unwind(&mut self)` then `Coroutine::into_stack(self)`.
// `force_unwind` RESUMES the coroutine at its `Yielder::suspend` with an
// `Err(ForcedUnwind)`, which `suspend` turns into a `resume_unwind` of a
// private payload carrying the coroutine's initial stack pointer; the stack
// unwinds from the park point outwards running every destructor, and the
// crate's `catch_forced_unwind` at the root swallows the payload iff it is
// the one it minted. On return `stack_ptr == None`, i.e. `done()`, which is
// exactly `into_stack()`'s assertion — so the pooled stack comes back
// live-object-free, no munmap special case, the identical argument the
// existing panic arm already makes. Sharp edges found:
//   - `force_unwind` LOOPS: a coroutine that suspends again mid-unwind is
//     re-thrown at until it reaches its root. A task whose stack CATCHES the
//     payload and then never parks again would hang the shard inside the
//     kill. `builtins::flow` has four `catch_unwind(AssertUnwindSafe(..))`
//     sites around user transform/init calls (flow.rs:2523, 2723, 3157,
//     3688) — a user transform that parks inside one of them is exactly that
//     shape. Not reachable from this probe's chan-level tests; named here
//     because W3 must handle it.
//   - `Coroutine::drop` force-unwinds AGAIN and aborts via a `scopeguard`
//     double-panic if THAT unwind escapes, so the shard arm must never
//     simply drop a coroutine whose forced unwind escaped. `kill_task`
//     `mem::forget`s it and counts the 8 MiB in `kill_stacks_leaked`
//     instead (observed Δ0 in every run).
//   - `force_unwind` is `&mut self` and the unwind runs on the SHARD's
//     thread, so the arm must wrap it in `catch_unwind` exactly like
//     `resume_task` wraps its resume, or a destructor panic takes the shard
//     down.
//
// What §3.5 gets WRONG (the load-bearing part of this probe)
// ----------------------------------------------------------
//
// F1. **"Registered wakers on a dead task are already harmless" is true of
//     WAKERS and false of WAITERS.** A `TakerWaiter` is a one-shot CELL and a
//     `PutterWaiter` carries the VALUE; the kill retracts neither, and no
//     delivery site checks the target's liveness
//     (`deliver_to_task_taker_cold` / `take_from_task_putter_cold` in
//     builtins/async.rs pop, write, and report success unconditionally).
//     Consequences, both deterministic (tests E1/E2, and D at scale):
//       - E2: kill a task parked in `<!`, then put on that chan -> the put
//         returns TRUE and the value is gone. Test D: 512 killed takers
//         swallowed 512/512 subsequent puts. A killed proc leaves one
//         message-eating trap on every chan it was reading.
//       - E1: kill a task parked in `>!` -> its value is STILL delivered to
//         the next taker, after the sender is dead. A killed proc's last
//         message lands downstream and no one can observe that it was
//         posthumous.
//     "Lazy cull" as §3.5 words it does not exist for waiters, and cannot:
//     nothing ever revisits `task_takers`/`task_putters` except a deliverer,
//     and the deliverer is the party being fooled.
//
// F2. **"Zero per-op cost by construction" does not survive F1.** The only
//     fix that closes E2 inside the existing protocol is for the delivery
//     sites to skip a waiter whose task is terminal — one relaxed atomic
//     load per delivery-to-a-task-waiter, i.e. on the hot chan path, which
//     is precisely what §3.5 claims the design avoids and what the
//     cooperative-cancel fallback was rejected for costing. (The alternative
//     — unregistering the dying task from its chan queues — needs the kill
//     path to know which chans a task is parked on, which nothing records,
//     and would take a chan lock from the shard's kill arm.)
//
// F3. **The "committed, then killed" window is not a corner case.** §3.5
//     dismisses it as "fine". Under a contested race it is 30% of rounds in
//     the taker polarity and 52% in the putter polarity, because the window
//     is commit-store -> drop(guard) -> `cv.notify_all()` -> doorbell rings
//     -> `ring_alts` -> `wake_all`: a microsecond, not a few instructions.
//     For the putter polarity that is benign; for the taker polarity every
//     one of those rounds is a lost message.
//
// F4. **§3.5 never mentions the yielder, and it is already zero.**
//     `resume_task` clears `ExecTls::yielder` at the park the task never
//     returns from, and the address is a fact only the task's own stack
//     knows, so the shard cannot restore it. Every destructor on a
//     force-unwound stack therefore runs with `runtime::in_task() == false`
//     (asserted in test A). This directly constrains §3.5's own proposal
//     that "the `DoneCells` guard is on that stack -> reason `Killed` is put
//     and the cells close": that guard must use only NON-PARKING chan
//     operations, because a blocking op from a destructor would take the
//     OS-thread `cv_wait` path and WEDGE THE SHARD. Fix, if the landing
//     wants task-context destructors: stash the live yielder in `TaskEntry`
//     at switch-out (`e.yielder.replace(0)` instead of `set(0)`, one extra
//     store on the resume path) and restore it in the kill arm. Note that
//     even then a destructor that actually parks double-panics and aborts —
//     abort instead of wedge, which is louder but not better.
//
// F5. **A RUNNING task is unreachable — and "arrange for it to be parked"
//     is not always available.** The probe's killers spin the CAS; a real
//     supervisor must too, and §3.5 should say so explicitly (a single kill
//     attempt fails ~50% of the time against a task that is mid-hop).
//
// ===========================================================================
// P5a-bis RESULTS — the tombstone
// ===========================================================================
//
// VERDICT: ** TOMBSTONE LANDS ** against its pre-registered rule, on all four
// conjuncts. Same machine, `--test-threads=1`, 3 identical repeats.
//
//   killed-taker LOST over 100k raced rounds ....... 0   (P5a: 30 400)
//   black-hole 512/512 puts survive ............... 512  (P5a: 0/512)
//   killed-putter posthumous deliveries ............ 0   (P5a: 52 174)
//   clause (2) destructors ......................... PASS (tests A, C, F)
//   clause (3) no poisoning ........................ PASS (test D, direct)
//   clause (4) stacks/RSS flat ..................... PASS (Δ0 stacks, Δ80-96 KiB)
//
// New Test B splits (100 000 rounds each, same log-swept jitter):
//
//   putter-killed   commit_won 49 261 | kill_won 50 739 | posthumous 0 | lost 0
//   taker-killed    commit_won 87 988 | kill_won 12 012 | posthumous 0 | lost 0
//
// The shape of the change is as informative as the zeros. Pre-tombstone the
// putter polarity had 52 174 rounds in the "killed but the value moved
// anyway" class; those are now CLEAN KILLS (`kill_won` 38 -> 50 739), because
// the arbitration moved EARLIER — from the coarse `PARKED -> KILLED` state
// CAS to the commit cell itself, which is the same word the deliverer writes.
// `kills_salvaged` counts the residue that genuinely could not be arbitrated
// in the killer's favour: ~2.5k (putter) and ~22k (taker) rounds where a
// commit had already landed, each of which resurrected its task
// (`KILLED -> READY` + enqueue) and returned `false` so the supervisor
// retries. Zero of those lost a message; every one of them is a round the
// old code would have lost or delivered posthumously.
//
// Delivery sites touched (all `// P5a-bis:` marked, all feature-gated)
// --------------------------------------------------------------------
//   src/builtins/async.rs:475 `take_from_task_putter_cold`
//       PRE-EXISTING WRITE, UPGRADED TO A CAS. `p.commit.store(PUT_DONE,
//       Release)` -> `compare_exchange(PUT_WAITING, PUT_DONE, AcqRel,
//       Acquire)`, plus a `loop`/`continue` to cull the corpse and serve the
//       next putter. ** THIS IS THE ONE REAL HOT-PATH COST — see below. **
//   src/builtins/async.rs:~505 `promote_task_putters_cold`
//       Same store -> CAS, same cull. Buffered promote path.
//   src/builtins/async.rs:~539 `deliver_to_task_taker_cold`
//       ADDED BRANCH, inside the cell mutex this site ALREADY takes. No new
//       lock, no new atomic. `v` is only `take()`n after the claim succeeds,
//       which is what lets the caller fall through to buffer/park when the
//       queue holds nothing but corpses.
//   src/builtins/async.rs:~1006 `chan_close` taker loop
//       ADDED BRANCH, and the guard was refactored so ONE acquisition serves
//       both the check and the write (the first draft took the mutex twice —
//       fixed). Needed only because the `Some(v)` arm can hand a BUFFERED
//       value to the waiter; the `None` close-marker arm would be harmless.
//   src/builtins/async.rs `chan_close` PUTTER loop — deliberately NOT touched:
//       it drops the waiter's value either way, so a corpse there loses
//       nothing. Documented rather than edited.
//   src/runtime/mod.rs `current_take_cell`
//       ADDED: one RELAXED store re-arming `put_commit` to `PUT_WAITING` on
//       the taker-REGISTRATION path (already `#[cold]`, already inside the
//       chan lock, immediately before a context switch). Required, not
//       cosmetic: without it a task parked in a TAKE carries the previous
//       put's terminal `PUT_DONE`, is indistinguishable from a putter whose
//       commit just landed, and salvage-loops forever instead of dying.
//       ** W3 MOVED THIS. ** The probe only ever killed tasks parked in a
//       chan op, so a re-arm on the taker path looked sufficient. A flow
//       proc spends most of its life parked on a DOORBELL (its read-set
//       wait) or a timer, in no chan queue at all, and such a task carries
//       the last put's terminal `PUT_DONE` too — so the same salvage loop
//       ate the first real kill W3 attempted. The re-arm now lives on the
//       way OUT of `park_task_putter` instead, which makes `PUT_WAITING`
//       the cell's unconditional RESTING value for any task that is not
//       registered as a putter, whatever it is parked on. Same one relaxed
//       store, on a path that just paid for a context switch, and one
//       FEWER store on the taker-registration path (E1's).
//
// HOT-PATH GUARD (item 6) — LOUD
// ------------------------------
// One instruction class was added OUTSIDE an already-held lock:
// `take_from_task_putter_cold` and `promote_task_putters_cold` turn a release
// STORE into a CAS (aarch64 `stlr` -> `casal`). That is the unbuffered
// task->task rendezvous, i.e. E1's path. ** A MANDATORY E1/E4 A/B IS OWED. **
// Scoping, so the A/B measures the right thing:
//   - Both functions are `#[cold] #[inline(never)]` behind `is_empty()`
//     guards that are UNCHANGED, so a chan with no parked task waiter pays
//     literally zero — buffered `>!!`/`<!!` traffic is untouched.
//   - The CAS is uncontended in the common case (the killer only touches the
//     word when a supervisor is actually killing that task), so the cost is
//     the RMW itself on an already-exclusive line, not contention.
//   - At probe time everything was behind `l4-kill-probe`, so the A/B was
//     feature-on vs feature-off on the same commit and the DEFAULT build was
//     byte-identical to `c4348bc`.
//
// ** THE A/B, PAID TWICE. ** Principal, feature-on vs feature-off:
// E1 47.9-49.5 (on) vs 51.8-56.4 (off) ns/hop, E4 within noise. W3 re-ran it
// after de-gating, because the DEFAULT hot path is what changed: interleaved
// runs of this commit's binary against `0e51207`'s, six passes each, first
// discarded as cold (docs/L2-PROBE-RESULTS.md's standing rule).
//   E1 same-shard hop      new 49.3-52.6  vs  base 52.3-55.3 ns/hop
//   E4 uncontended put+take new 8.47-8.68 vs  base 8.48-8.72 ns/op
// The claim CAS is free, and E1 comes out marginally AHEAD -- consistent
// with the `current_take_cell` re-arm moving off the taker-registration path
// (see above), which is a store E1 used to pay on every hop. E4's two-thread
// number is the documented bimodal one (batched ~45 vs lockstep ~135 ns/op,
// a scheduler-regime lottery, not a ruler) and both binaries were observed
// in both regimes.
//
// Still smells wrong for W3  --  ALL FIVE DISCHARGED, see the W3 block below
// -------------------------
// S1. **The corpse waiters violate a documented invariant while they wait to
//     be culled.** `value.rs:3210` states: "`task_takers` non-empty implies
//     `buffer` empty, and `task_takers` and `task_putters` are never both
//     non-empty." A tombstoned waiter is queued but not live, so test D's
//     buffered half deliberately runs a chan with 8 queued takers AND 8
//     buffered values. Nothing asserts the invariant today (no `debug_assert`
//     anywhere in async.rs) and every reader happens to be safe because they
//     all consult `task_takers` before `buffer` — but W3 must either restate
//     the invariant in terms of LIVE waiters or cull eagerly. Leaving a
//     load-bearing comment false is how the next bug gets written.
// S2. **Culling is still lazy.** A killed proc's corpse entries sit on its
//     chans until some deliverer walks past them. Bounded (one per kill per
//     chan) and self-healing, but a supervisor restarting a proc 1k times
//     against an idle chan queues 1k corpses that nothing ever visits.
//     Worth a bound or an eager unregister in W2/W3.
// S3. **The `catch_unwind`/`ForcedUnwind` edge is unchanged and unprobed.**
//     `force_unwind` LOOPS until the coroutine reaches its root; a stack that
//     CATCHES the payload and then never parks again hangs the shard inside
//     the kill. `builtins::flow` has four
//     `catch_unwind(AssertUnwindSafe(..))` sites wrapping user transform/init
//     calls (flow.rs:2523, 2723, 3157, 3688) and a user transform may park
//     inside one. Nothing in P5a-bis changes this and no test here reaches
//     it. W3 needs either a `ForcedUnwind`-aware re-raise at those four sites
//     or an explicit bound on the unwind loop.
// S4. **The salvage path resurrects a task from `KILLED`.** Sound (we hold it
//     exclusively; every `wake()` in the window takes its existing
//     `Err(_) => return` arm, so our enqueue is the only one) but it is a NEW
//     edge in a state machine whose whole documented virtue is that it has
//     exactly five. The landing spec should draw it explicitly, and the
//     module doc's "Waker-only transitions" paragraph needs a third line.
// S5. **F4's law is now load-bearing.** Test F pins that an `ExitGuard`-style
//     destructor works with NON-PARKING chan ops from `in_task() == false`.
//     A parking op there is a panic-inside-a-panic abort. Whatever writes the
//     `Killed` exit reason in W1/W3 must be checked against that law by
//     construction, not by convention.
//
// Recommendation to the landing spec
// ----------------------------------
// P5a said: the mechanism is sound, the message-safety claim is not. The
// principal's tombstone ruling closes the gap, and P5a-bis confirms it on
// evidence — kill is now atomic with respect to the commit, in both
// polarities, with the residue resolved by resurrect-and-retry rather than by
// losing a message. §3.5 should be rewritten around the CELL claim rather
// than the state CAS, which is the arbitration point that actually decides
// W7, and it should drop the "zero per-op cost by construction" claim: the
// price is one store-to-CAS upgrade on the task-putter delivery path, owed an
// E1/E4 A/B. S1-S5 above are the open items W3 inherits.
//
// ===========================================================================
// W3 — HOW S1-S5 WERE DISCHARGED (landed 2026-08-28)
// ===========================================================================
//
// The machinery above is no longer feature-gated: the KILLED state, the
// salvage arm, `Job::Kill`/`kill_task`, and all four delivery-site claims are
// the DEFAULT build, and this file is their standing regression net rather
// than an opt-in probe. `TaskWaker::kill_for_probe` is now `TaskWaker::kill`.
//
// S1. RESTATED, and now checked. `value.rs`'s `ChanState::task_takers` doc
//     says LIVE waiters, names corpses as the exemption, and points at the
//     mechanism. `builtins::async`'s `debug_assert_live_waiter_invariant`
//     checks the live-waiter form at both park-registration sites in debug
//     builds (it locks each queued cell to tell live from corpse, which is
//     why it is debug-only).
// S2. LAZY CULL RULED, eager unregister REJECTED — the full argument is at
//     `TaskWaker::kill`. In short: eager unregister needs a record of which
//     chans a task is parked on, i.e. a new store on the park path plus a
//     chan lock taken from the kill path, to tidy a set that is already
//     bounded by "one corpse per killed task per chan, and a task is killed
//     at most once". S2's own 1k-restarts pathology is unreachable: only a
//     KILL can destroy a task at a park point (every other death has already
//     returned from its park), and W3's only killer is `flow/stop-proc`'s
//     escalation, whose runs do not restart.
// S3. RE-RAISE, not a bound. `builtins::flow`'s `reraise_if_killed` is called
//     first in the cold `Err(payload)` arm of all four `catch_unwind` sites
//     and `resume_unwind`s when `runtime::current_task_is_killed()`. Two
//     bugs in one: the unwind-loop wedge this note predicted, and a phantom
//     transform error reported by a proc that was killed rather than failed.
//     Pinned by `a_killed_proc_reports_no_phantom_transform_error` in
//     tests/l4_supervision_test.rs, which drives BOTH directions (a real
//     throw still reports its incident and the proc continues).
// S4. DRAWN. `runtime`'s module doc now has the killer-only paragraph:
//     `PARKED -> KILLED` plus the enqueue, and the salvage `KILLED -> READY`,
//     with the argument for why the killer is the only party that can move a
//     KILLED task.
// S5. SATISFIED BY CONSTRUCTION. The `Killed` exit reason is written by
//     `ExitGuard::drop` asking `runtime::current_task_is_killed()` — one
//     atomic load, no chan op at all — and the guard's own doc discharges F4
//     op by op for everything else it does. That mechanism (rather than a
//     flag the supervisor sets before killing) is what makes the reason exact
//     rather than racy: it reads the dying task's own state word, which the
//     shard has already published along with the rest of that task's identity
//     before it force-unwinds.
