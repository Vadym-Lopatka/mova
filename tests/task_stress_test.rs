//! L1/W5 verification wave (docs/L1-LANDING-SPEC.md §W5): stress and
//! perf gates on the LANDED task runtime, run standalone -- these are not
//! part of the default `cargo test` suite (see the module docs of
//! `tests/task_chan_test.rs` and `tests/task_runtime_go_test.rs` for the
//! harness patterns this file borrows: deadline loops instead of bare
//! blocks, a process-abort watchdog for the class of failure a lost wake
//! produces, and process-global counters read as deltas).
//!
//! **S2 is law** (docs/L1-LANDING-SPEC.md self-review): task parks carry no
//! safety-net timeout by design. If any hammer below hangs, that is a
//! MISSED-RING BUG to report with evidence, never a reason to add a net
//! inside the runtime. The deadline loops/watchdogs here are the TEST's own
//! failure-reporting mechanism (same discipline as the two W3/W4 files) --
//! they detect and diagnose a hang, they do not paper over one.
//!
//! Run the `#[ignore]`d stress/perf tests one at a time, each its own
//! foreground process:
//! ```text
//! cargo test --release --test task_stress_test <name> -- --ignored --nocapture
//! ```
//! The four fast correctness tests (binding interleave/conveyance,
//! deep-stack safe depth, deep-stack overflow-in-a-child-process) are NOT
//! ignored -- they run in a few seconds and stay in the default sweep of
//! this file.

use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use mova::embed::{Engine, Value};
use mova::internal::task_chan::{self, BufferPolicy, Doorbell};
use mova::runtime;

fn engine() -> Engine {
    Engine::builder().build()
}

fn eval(e: &mut Engine, src: &str) -> Value {
    e.eval_named("task-stress", src).unwrap_or_else(|err| panic!("eval error: {}", err.render_plain()))
}

/// Spin-with-sleep until `cond`, or panic with a pointed diagnostic. Every
/// wait in this file is one of these (or a [`Watchdog`]) -- never a bare
/// block -- per S2: a hang here means a wake was lost, not that the
/// machine was slow (the budgets are seconds to minutes against
/// microsecond-to-millisecond-scale work).
fn await_until(what: &str, budget: Duration, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        if cond() {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("task_stress_test: timed out after {budget:?} waiting for {what} -- a wake was lost");
}

/// Same shape as `tests/task_runtime_go_test.rs`'s `Watchdog`: aborts the
/// PROCESS (not just the test) with a diagnostic if the guarded section
/// does not finish in time, so a lost-wake hang is reported instead of
/// wedging the harness.
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
                "TASK STRESS WATCHDOG: {what} did not finish in {secs}s -- lost wake / deadlock, per S2 this is a bug to report, not a timeout to raise"
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

/// Self RSS in KiB, via `ps` (portable across the darwin/linux boxes this
/// suite runs on; both accept `-o rss=`).
fn rss_kib() -> u64 {
    let pid = std::process::id().to_string();
    let out = Command::new("ps")
        .args(["-o", "rss=", "-p", &pid])
        .output()
        .expect("ps -o rss= must run");
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap_or(0)
}

/// Samples [`rss_kib`] every 100ms on a background thread and tracks the
/// max, until dropped. A plain internal helper thread -- not a backgrounded
/// shell command -- so it carries none of the "never background a
/// long-running Bash call" risk; it is just test code.
struct RssMonitor {
    peak_kib: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl RssMonitor {
    fn start() -> RssMonitor {
        let peak_kib = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (p, s) = (peak_kib.clone(), stop.clone());
        let handle = std::thread::spawn(move || {
            while !s.load(Ordering::Relaxed) {
                let now = rss_kib();
                p.fetch_max(now, Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(100));
            }
        });
        RssMonitor { peak_kib, stop, handle: Some(handle) }
    }

    fn stop_and_peak_kib(mut self) -> u64 {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        self.peak_kib.load(Ordering::Relaxed)
    }
}

// ============================================================================
// A. MILLION-GO: 1,000,000 tasks fanning into a handful of buffered chans,
//    spawned from a few coordinator tasks (batch spawns -> family-local
//    placement -> spillover past SPAWN_LOCAL_MAX, exactly the shape the
//    module doc's "Spawn placement" section describes).
// ============================================================================

const MILLION_WORKERS: i64 = 1_000_000;
const MILLION_COORDINATORS: i64 = 20;
const MILLION_RESULT_CHANS: usize = 8;
const MILLION_TAKERS: usize = 8;

#[test]
#[ignore = "slow: run standalone, see module doc"]
fn a_million_go_fan_in_checksum() {
    assert_eq!(MILLION_WORKERS % MILLION_COORDINATORS, 0);
    let per_coord = MILLION_WORKERS / MILLION_COORDINATORS;

    let chans: Arc<Vec<Arc<task_chan::Chan>>> = Arc::new(
        (0..MILLION_RESULT_CHANS).map(|_| task_chan::chan(BufferPolicy::Fixed(4096))).collect(),
    );
    let puts_done = Arc::new(AtomicU64::new(0));
    let takers_done = Arc::new(AtomicUsize::new(0));
    let taken_sum = Arc::new(AtomicI64::new(0));
    let taken_count = Arc::new(AtomicU64::new(0));

    let mon = RssMonitor::start();
    let baseline_kib = rss_kib();
    let t0 = Instant::now();

    // A HANDFUL of taker tasks, one per result chan, each draining until
    // its chan reports closed-and-drained (`None`).
    for k in 0..MILLION_RESULT_CHANS {
        let (ch, sum, count, done) =
            (chans[k].clone(), taken_sum.clone(), taken_count.clone(), takers_done.clone());
        runtime::spawn(move || {
            loop {
                match task_chan::take_int(&ch) {
                    Some(v) => {
                        sum.fetch_add(v, Ordering::Relaxed);
                        count.fetch_add(1, Ordering::Relaxed);
                    }
                    None => break,
                }
            }
            done.fetch_add(1, Ordering::Relaxed);
        });
    }

    // A FEW coordinator tasks, each batch-spawning `per_coord` workers.
    // Spawned from the main thread (root spawns -> round-robin across
    // shards); each worker is spawned FROM ITS COORDINATOR (family-local
    // placement while the shard isn't flooded, spilling to round-robin
    // once `Shard::load` crosses `SPAWN_LOCAL_MAX` -- `per_coord` = 50 000
    // is three orders of magnitude past that threshold, so every
    // coordinator's batch exercises both regimes).
    for c in 0..MILLION_COORDINATORS {
        let base = c * per_coord;
        let (chans, puts_done) = (chans.clone(), puts_done.clone());
        runtime::spawn(move || {
            for local in 0..per_coord {
                let id = base + local;
                let (chans, puts_done) = (chans.clone(), puts_done.clone());
                runtime::spawn(move || {
                    let ch = &chans[(id as usize) % MILLION_RESULT_CHANS];
                    let sent = task_chan::put_int(ch, id);
                    assert!(sent, "worker {id}: put reported false on a chan nobody closed yet");
                    let prev = puts_done.fetch_add(1, Ordering::AcqRel);
                    if prev + 1 == MILLION_WORKERS as u64 {
                        // Single-closer election: exactly one worker's
                        // fetch_add observes the post-increment total, so
                        // exactly one close! per chan happens, after every
                        // put has landed.
                        for ch in chans.iter() {
                            task_chan::close(ch);
                        }
                    }
                });
            }
        });
    }

    await_until(
        "every taker to drain its chan to closed",
        Duration::from_secs(300),
        || takers_done.load(Ordering::Relaxed) == MILLION_TAKERS,
    );
    let elapsed = t0.elapsed();
    let peak_kib = mon.stop_and_peak_kib();

    let expected_sum = MILLION_WORKERS * (MILLION_WORKERS - 1) / 2; // Gauss sum 0..999999
    assert_eq!(taken_count.load(Ordering::Relaxed), MILLION_WORKERS as u64, "A: not every worker's put was drained");
    assert_eq!(taken_sum.load(Ordering::Relaxed), expected_sum, "A: checksum mismatch -- a value was dropped, duplicated, or corrupted");
    assert_eq!(puts_done.load(Ordering::Relaxed), MILLION_WORKERS as u64);

    let spawns_per_sec = MILLION_WORKERS as f64 / elapsed.as_secs_f64();
    println!(
        "A million-go: {MILLION_WORKERS} tasks in {elapsed:?} ({spawns_per_sec:.0} spawns/s); \
         RSS baseline {baseline_kib} KiB -> peak {peak_kib} KiB ({:.2} GiB); \
         checksum {} == expected {expected_sum}; shards = {}",
        peak_kib as f64 / 1024.0 / 1024.0,
        taken_sum.load(Ordering::Relaxed),
        runtime::shard_count(),
    );
    assert!(
        peak_kib < 24 * 1024 * 1024,
        "A: peak RSS {peak_kib} KiB exceeds the 24 GiB sanity bar"
    );
}

// ============================================================================
// B. NO-NET SOAK HAMMER (S2's proving ground): ~10k tasks, ~64 shared chans,
//    a randomized op mix per task, seeded xorshift32 (pure Mova, so the
//    seed and the whole run are reproducible independent of any host RNG),
//    driven through the REAL `go`/`>!!`/`<!!`/`alts!!`/`poll!`/`offer!`/
//    `close!` surface (W4's shipping spelling), because the mixed-op-kind
//    alts + poll!/offer! surface has no raw-Rust equivalent exposed through
//    `internal::task_chan` (that module only wraps the specific primitives
//    W3's own gate tests needed).
// ============================================================================

const SOAK_SEED: i64 = 0x5EED_C0FFEE; // printed below; fixed for reproducibility
// A first attempt at the spec's literal 10 000 tasks x 30 ops used a
// `(go (loop [] (if ready (close-all!) (do (sleep-ms 20) (recur)))))`
// liveness backstop that self-livelocked (sleep-ms blocks its shard
// outright, so the busy-poll never yielded that shard back to any other
// task placed there -- see docs/L1-LANDING-RESULTS.md's soak section for
// the full account). Once the backstop was moved to poll from the main
// OS thread instead (below), 10 000 tasks at a MUCH higher per-task quota
// than originally planned completed in ~1s per 160k ops -- so QUOTA here
// is raised well past the spec's 30, not shrunk, to land in its stated
// 30-60s wall-clock band.
const SOAK_TASKS: i64 = 10_000;
const SOAK_CHANS: i64 = 64;
const SOAK_QUOTA: i64 = 400; // ops per task -> 4 000 000 randomized ops total
const SOAK_CLOSEABLE: i64 = 8; // last 8 chans are the designated-closeable subset

#[test]
#[ignore = "slow: run standalone, see module doc"]
fn b_no_net_soak_hammer_10k_tasks_64_chans() {
    println!("B soak: seed = 0x{SOAK_SEED:X}, tasks = {SOAK_TASKS}, chans = {SOAK_CHANS}, quota = {SOAK_QUOTA}/task");
    let safety_net_before = Doorbell::safety_net_hits();

    let mut e = engine();
    let _w = Watchdog::arm(240, "B no-net soak hammer");
    let t0 = Instant::now();

    // Evidence thread (rule 4): the interpreter's own eval() call below
    // blocks this thread synchronously until the whole soak script
    // returns, so if it stalls the ONLY way to see whether tasks are
    // actually making progress is through the process-global runtime
    // counters, read from a second thread. Prints every 5s; if the
    // Watchdog fires, whatever this printed is the evidence of where
    // things stood (spawned vs finished vs resumed vs panicked, and the
    // per-shard finished spread) at the moment of the hang.
    let evidence_stop = Arc::new(AtomicBool::new(false));
    let evidence_handle = {
        let stop = evidence_stop.clone();
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_secs(5));
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                println!(
                    "B soak evidence @ {:?}: tasks_spawned={} tasks_finished={} tasks_resumed={} tasks_panicked={} \
                     shard_finished={:?}",
                    Instant::now(),
                    runtime::tasks_spawned(),
                    runtime::tasks_finished(),
                    runtime::tasks_resumed(),
                    runtime::tasks_panicked(),
                    runtime::shard_finished_counts(),
                );
            }
        })
    };

    let src = format!(
        r#"
        (def seed {SOAK_SEED})
        (def num-tasks {SOAK_TASKS})
        (def num-chans {SOAK_CHANS})
        (def quota {SOAK_QUOTA})
        (def closeable-start (- num-chans {SOAK_CLOSEABLE}))

        ;; 64 chans, 5 buffer policies round-robin: unbuffered, fixed-1,
        ;; fixed-8, dropping-4, sliding-4 -- the mix docs/L1-LANDING-SPEC.md
        ;; §W5(b) asks for.
        (def chans
          (loop [i 0 acc []]
            (if (< i num-chans)
              (recur (inc i)
                     (conj acc
                           (let [k (mod i 5)]
                             (cond
                               (= k 0) (chan)
                               (= k 1) (chan 1)
                               (= k 2) (chan 8)
                               (= k 3) (chan (dropping-buffer 4))
                               :else (chan (sliding-buffer 4))))))
              acc)))

        (def tasks-done (atom 0))
        (def ops-done (atom 0))
        (def puts-true (atom 0))
        (def puts-false (atom 0))
        (def takes-some (atom 0))
        (def takes-nil (atom 0))
        (def alts-done (atom 0))
        (def polls-done (atom 0))
        (def offers-done (atom 0))
        (def done-ch (chan num-tasks))

        ;; Pure xorshift32, masked to stay in [0, 2^32) at every step so a
        ;; shift never sees a negative operand -- deterministic and
        ;; independent of any host RNG, hence reproducible by seed alone.
        (defn xorshift32 [x]
          (let [x (bit-and (bit-xor x (bit-shift-left x 13)) 0xffffffff)
                x (bit-xor x (bit-shift-right x 17))
                x (bit-and (bit-xor x (bit-shift-left x 5)) 0xffffffff)]
            x))

        (defn run-task [task-id]
          (go
            (loop [i 0 state (bit-or (bit-and (* (inc task-id) 2654435761) 0xffffffff) 1)]
              (if (< i quota)
                (let [state (xorshift32 state)
                      ch-idx (mod state num-chans)
                      ch (nth chans ch-idx)
                      op-kind (mod (bit-shift-right state 8) 6)]
                  (cond
                    (= op-kind 0)
                    (do (if (>!! ch task-id) (swap! puts-true inc) (swap! puts-false inc)))

                    (= op-kind 1)
                    (do (if (nil? (<!! ch)) (swap! takes-nil inc) (swap! takes-some inc)))

                    (= op-kind 2)
                    (do (poll! ch) (swap! polls-done inc))

                    (= op-kind 3)
                    (do (offer! ch task-id) (swap! offers-done inc))

                    (= op-kind 4)
                    (do (alts!! [ch [(nth chans (mod (inc ch-idx) num-chans)) task-id]])
                        (swap! alts-done inc))

                    :else
                    (do (alts!! [ch (timeout 2)])
                        (swap! alts-done inc)))
                  (swap! ops-done inc)
                  (recur (inc i) state))
                (do (swap! tasks-done inc)
                    (>!! done-ch task-id))))))

        ;; A dedicated closer task: mid-soak (staggered, so some ops land
        ;; before and some after each close), close! the designated
        ;; closeable subset. Every op above already handles a closed chan
        ;; by branching on the false/nil it gets back -- nothing in
        ;; run-task treats a close as an error. Bounded by construction
        ;; (exactly 8 iterations, unconditional) -- unlike an earlier
        ;; version of this test, nothing here waits on another task's
        ;; progress, so it can never monopolize its shard.
        (go
          (loop [k closeable-start]
            (if (< k num-chans)
              (do (sleep-ms 5) (close! (nth chans k)) (recur (inc k)))
              nil)))

        (defn close-all-chans! []
          (loop [k 0]
            (if (< k num-chans)
              (do (close! (nth chans k)) (recur (inc k)))
              nil)))

        (loop [i 0]
          (if (< i num-tasks)
            (do (run-task i) (recur (inc i)))
            nil))
        :spawned
        "#
    );
    eval(&mut e, &src);

    // Liveness backstop, run from THIS (main, non-shard) OS thread rather
    // than from inside a go task: a purely random independent op mix
    // across 64 chans has no a-priori guarantee every blocking >!!/<!!
    // draws a matching partner before the whole population exhausts its
    // quota -- an unbuffered/fixed chan can end up with a parked putter
    // (or taker) nobody will ever visit again. An EARLIER version of this
    // backstop ran as a `(go (loop [] (if ready (close-all!) (do
    // (sleep-ms 20) (recur)))))` task -- and it self-livelocked: `sleep-ms`
    // blocks its shard's OS thread outright (R1), so a task busy-polling
    // it in a tight recur never yields that shard back to ANY other task
    // placed there, which starved exactly the tasks the backstop existed
    // to unstick (confirmed: `runtime::shard_finished_counts()` showed one
    // shard frozen at 0 while its 13 siblings finished ~140 each and also
    // stalled). Polling from the main thread has no such hazard -- it
    // never occupies a shard at all, so it cannot starve one. `close!` is
    // the ONLY mechanism used to unstick anything, and it is the exact,
    // already-proven primitive `tests/task_chan_test.rs`'s W3 gates drive
    // directly -- this is a test-orchestration liveness guarantee, not a
    // net inside the runtime's own parks (S2).
    // Plateau detection, not a flat deadline: trigger the backstop as soon
    // as progress stops for `PLATEAU` in a row, so a fast run doesn't pay
    // a fixed tax before the backstop fires. `HARD_CAP` is still an outer
    // bound (a real hang must still surface as a failure via the
    // Watchdog, not an infinite plateau-wait).
    //
    // The progress signal polled is `@ops-done` (increments on EVERY op,
    // thousands of times per task), not `@tasks-done` (increments only
    // once a task finishes its WHOLE quota). An earlier version of this
    // loop polled `tasks-done` and, at quota=400, false-triggered at
    // "0/10000 for 750ms" -- true, but meaningless: with a 400-op quota no
    // task CAN finish in 750ms even when every single op is a healthy
    // rendezvous, so the backstop fired at the START of the run and
    // converted almost the entire soak into closed-chan fast-paths
    // (measured: puts_false/takes_nil each ~91% of their op kind) instead
    // of exercising real contention. `ops-done` moving means SOME task
    // somewhere is making real progress; only a plateau across the WHOLE
    // 10 000-task population is a genuine stall.
    const POLL_EVERY: Duration = Duration::from_millis(50);
    const PLATEAU: Duration = Duration::from_millis(750);
    const HARD_CAP: Duration = Duration::from_secs(120);
    let hard_deadline = Instant::now() + HARD_CAP;
    let mut last_ops = -1_i64;
    let mut last_change = Instant::now();
    let mut closed_early = false;
    loop {
        let tasks_done: i64 = eval(&mut e, "@tasks-done").to_string().parse().expect("tasks-done");
        if tasks_done >= SOAK_TASKS {
            break;
        }
        let ops_now: i64 = eval(&mut e, "@ops-done").to_string().parse().expect("ops-done");
        if ops_now != last_ops {
            last_ops = ops_now;
            last_change = Instant::now();
        } else if last_change.elapsed() >= PLATEAU {
            closed_early = true;
            println!(
                "B soak: ops-done plateaued at {ops_now} (tasks-done={tasks_done}/{SOAK_TASKS}) for {PLATEAU:?} -- invoking the liveness backstop (close-all-chans!)"
            );
            eval(&mut e, "(close-all-chans!)");
            break;
        }
        if Instant::now() >= hard_deadline {
            closed_early = true;
            println!("B soak: hard cap {HARD_CAP:?} reached at ops-done={ops_now} tasks-done={tasks_done}/{SOAK_TASKS} -- invoking the liveness backstop anyway");
            eval(&mut e, "(close-all-chans!)");
            break;
        }
        std::thread::sleep(POLL_EVERY);
    }
    if closed_early {
        // Give every now-unstuck straggler a moment to race through its
        // remaining (now-closed-chan, fast) ops and reach done-ch.
        await_until("stragglers to reach tasks-done == SOAK_TASKS after the backstop close", Duration::from_secs(30), || {
            eval(&mut e, "@tasks-done").to_string().parse::<i64>().unwrap_or(0) >= SOAK_TASKS
        });
    }

    // Block for every task's completion signal -- proof every task reached
    // its op quota (a straggler unstuck by the backstop above still runs
    // its remaining ops, now fast no-ops against closed chans, and signals
    // done-ch exactly like everyone else). No timeout on the drain itself:
    // the done-ch put IS the mechanism; liveness was already guaranteed
    // above.
    let out = eval(
        &mut e,
        r#"(loop [i 0 seen #{}]
             (if (< i num-tasks)
               (recur (inc i) (conj seen (<!! done-ch)))
               (count seen)))"#,
    );
    let elapsed = t0.elapsed();
    evidence_stop.store(true, Ordering::Relaxed);
    let _ = evidence_handle.join();
    let distinct_finishers: i64 = out.to_string().parse().expect("distinct-finisher count");
    assert_eq!(distinct_finishers, SOAK_TASKS, "B: every task must signal done-ch exactly once, with a distinct id");
    println!("B soak: backstop invoked = {closed_early}");

    let ops_done = eval(&mut e, "@ops-done").to_string();
    let puts_true = eval(&mut e, "@puts-true").to_string();
    let puts_false = eval(&mut e, "@puts-false").to_string();
    let takes_some = eval(&mut e, "@takes-some").to_string();
    let takes_nil = eval(&mut e, "@takes-nil").to_string();
    let alts_done = eval(&mut e, "@alts-done").to_string();
    let polls_done = eval(&mut e, "@polls-done").to_string();
    let offers_done = eval(&mut e, "@offers-done").to_string();
    let expected_ops = (SOAK_TASKS * SOAK_QUOTA).to_string();
    assert_eq!(ops_done, expected_ops, "B: total op count must equal tasks * quota exactly");

    let safety_net_after = Doorbell::safety_net_hits();
    println!(
        "B soak: {SOAK_TASKS} tasks x {SOAK_QUOTA} ops in {elapsed:?}; ops_done={ops_done} \
         (puts_true={puts_true} puts_false={puts_false} takes_some={takes_some} takes_nil={takes_nil} \
         alts_done={alts_done} polls={polls_done} offers={offers_done}); \
         safety_net_hits {safety_net_before} -> {safety_net_after}"
    );
    assert_eq!(
        safety_net_after, safety_net_before,
        "B: Doorbell safety-net fallback fired {} time(s) during a soak that should exercise ONLY task \
         parks (no-net by design) -- report this delta, it means an OS-thread doorbell wait fell back \
         to its timeout somewhere in this run",
        safety_net_after - safety_net_before
    );
}

// ============================================================================
// C. BINDING-IN-GO (W1's raison d'etre)
// ============================================================================

/// Many concurrent `(binding [*x* n] (<!! signal) (= *x* n))`-shaped
/// bodies, spawned well over SPAWN_LOCAL_MAX from ONE coordinator go block
/// so they pack (and spill) onto shared shards, then all parked
/// concurrently on the SAME unbuffered `signal` chan before being released
/// one at a time -- maximal interleaving on whichever shards they land on.
/// Every task must see its OWN bound value on resume.
#[test]
fn c1_binding_in_go_many_interleaved_tasks_see_own_value() {
    let _w = Watchdog::arm(60, "C1 binding-in-go interleave");
    let mut e = engine();
    let n = 500; // >> SPAWN_LOCAL_MAX (8)
    let src = format!(
        r#"
        (def ^:dynamic *x* -1)
        (def signal (chan))
        (def n {n})
        (def result-chans
          (<!! (go
            (loop [i 0 acc []]
              (if (< i n)
                (recur (inc i) (conj acc (go (binding [*x* i] (<!! signal) (= *x* i)))))
                acc)))))
        (loop [i 0] (if (< i n) (do (>!! signal :release) (recur (inc i))) nil))
        (loop [i 0 all-ok true]
          (if (< i n)
            (recur (inc i) (and all-ok (<!! (nth result-chans i))))
            all-ok))
        "#
    );
    let out = eval(&mut e, &src);
    assert_eq!(out.to_string(), "true", "C1: at least one task observed a foreign binding -- ctx isolation broke under interleave");
}

/// Conveyance: a binding made at the SPAWN SITE of a `go` must be visible
/// inside a `future` spawned FROM that task -- two conveyance hops
/// (binding -> go's fork, go's ctx -> future's fork).
///
/// **BUG, confirmed and isolated 2026-08-27 -- this currently FAILS.**
/// `(<!! (binding [*y* 42] (go *y*)))` alone (no future involved) already
/// returns `0`, not `42`: `go*` never conveys the spawning context's
/// dynamic bindings into the task at all. Isolated with `mova -e`:
/// - `(binding [*y* 42] (deref (future *y*)))` (bare `future`, no `go`) ->
///   `42`, correctly conveyed.
/// - `(<!! (binding [*y* 42] (go *y*)))` -> `0`, wrong.
/// - `(<!! (binding [*y* 42] (go (deref (future *y*)))))` -> `0`, wrong
///   (the outer `go` never conveys, so the inner `future` starts from an
///   unbound `*y*` regardless of ITS own conveyance being correct).
/// - `(<!! (go (binding [*y* 42] (deref (future *y*)))))` (binding placed
///   INSIDE the go body instead of at its spawn site) -> `42`: this is a
///   trivially different case (no conveyance needed, the binding is live
///   in the task's own context already).
///
/// Root cause (read, not fixed -- file ownership for W5 is tests/docs
/// only): `src/builtins/conc.rs`'s `future*` explicitly calls
/// `crate::env::snapshot_thread_bindings()` on the parent thread and
/// installs it via `crate::env::BindingConveyance::install(conveyed)` on
/// the spawned closure (its own doc: "M4b conveyance ... measured
/// Clojure: `(binding [*a* 42] @(future *a*))` is 42"). `go*`/`thread*` in
/// `src/builtins/async.rs` (`reg(i, "go*", ...)` / `reg(i, "thread*",
/// ...)`) do neither -- they call `interp.fork()` and hand the thunk
/// straight to `spawn_go_task`/`spawn_go_thread` with no binding snapshot
/// or install step anywhere in between.
///
/// **NOT an L1 regression.** Reproduces identically three ways, none of
/// which touch the task runtime: (a) `MOVA_GO_THREADS=1` (the pre-L1
/// OS-thread `go*` path, kill-switched back) also returns `0`; (b)
/// `thread` (always-OS-thread, never task-backed, unaffected by the kill
/// switch either way) also returns `0`; (c) `git log` on
/// `src/builtins/conc.rs` shows conveyance was added in commit `3f003d3`
/// ("M4b core -- per-thread dynamic bindings ..., conveyance into
/// futures") -- the commit title says "into futures", singular, and never
/// touched `go*`/`thread*`. This gap predates L1 entirely; L1 only made it
/// newly relevant because docs/L1-LANDING-SPEC.md §W5(c) and this wave's
/// own brief both assert `(binding [*x* 42] (go @(future *x*)))` delivers
/// 42, which was already false on `main` before this branch existed.
///
/// Left FAILING on purpose (rule: report bugs as failing tests, do not
/// paper over). The likely fix is small -- mirror `future*`'s three lines
/// (`snapshot_thread_bindings` before the fork, `BindingConveyance::install`
/// inside `run_go_body`/the thread thunk) in `go*` and `thread*` -- but
/// deciding whether/how to apply it is the conductor's call, not W5's.
#[test]
fn c2_binding_conveyance_from_go_into_future() {
    let mut e = engine();
    let out = eval(
        &mut e,
        r#"(def ^:dynamic *y* 0)
           (<!! (binding [*y* 42] (go (deref (future *y*)))))"#,
    );
    assert_eq!(
        out.to_string(),
        "42",
        "BUG (pre-existing, not an L1 regression -- see this test's doc comment): \
         go*/thread* never convey dynamic bindings from their spawn site, only future* does"
    );
}

// ============================================================================
// D. DEEP-STACK (R5)
// ============================================================================

/// ~1000 non-tail interpreted frames (docs/L1-PROBE-RESULTS.md: 6192 B/frame,
/// ~1354 fit an 8 MiB task stack -- "~1000 frames safe") inside a `go` body,
/// several interpreted levels below the go block itself (each `deep` call
/// is its own frame). Must complete cleanly.
#[test]
fn d1_deep_stack_go_task_completes_at_safe_depth() {
    let _w = Watchdog::arm(30, "D1 deep-stack safe depth");
    let mut e = Engine::builder().max_depth(1500).build();
    let out = eval(
        &mut e,
        r#"(defn deep [n] (if (<= n 0) 0 (+ 1 (deep (dec n)))))
           (<!! (go (deep 1000)))"#,
    );
    assert_eq!(out.to_string(), "1000", "D1: 1000-deep non-tail recursion inside a go task must complete");
}

/// Documented (not gated): deeper than ~1354 non-tail frames overflows the
/// task's 8 MiB reservation. Cheap verification that the guard page turns
/// this into a clean abort (a caught signal) rather than silent corruption
/// -- run in a CHILD PROCESS (the compiled `mova` CLI, `mova -e`, whose
/// own `EVAL_MAX_CALL_DEPTH` is 10 000 -- see `src/main.rs` -- so nothing
/// about the CLI's own depth guard intervenes before the coroutine's real
/// 8 MiB stack does) so this test's own process survives either way.
#[test]
fn d2_deep_stack_overflow_aborts_cleanly_in_a_child_process() {
    let bin = env!("CARGO_BIN_EXE_mova");
    let expr = r#"(defn deep [n] (if (<= n 0) 0 (+ 1 (deep (dec n))))) (println (<!! (go (deep 5000))))"#;
    let mut child = Command::new(bin)
        .arg("-e")
        .arg(expr)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to spawn the mova binary");

    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("D2: the overflow child did not exit within 30s -- an overflowing task stack must fail fast, not hang");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let out = child.wait_with_output().expect("wait_with_output");
    assert!(!status.success(), "D2: a 5000-deep non-tail recursion in a go task (>> the ~1354-frame guard-page threshold) was expected to overflow, not exit 0");

    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        println!(
            "D2 deep-stack overflow: child exit status = {status:?}, signal = {:?}, stderr tail = {:?}",
            status.signal(),
            String::from_utf8_lossy(&out.stderr).lines().rev().take(5).collect::<Vec<_>>()
        );
        assert!(
            status.signal().is_some(),
            "D2: expected the overflow to be reported as a killing signal (guard page -> SIGSEGV/SIGBUS, or a caught \
             'stack overflow' abort), got a plain nonzero exit instead -- report this, it may mean the recursion \
             returned an ordinary (wrong) mova-level error rather than actually overflowing"
        );
    }
}

// ============================================================================
// E. PERF NUMBERS (quiet machine; gated where the spec states a gate)
// ============================================================================

/// Same-shard ping-pong: a coordinator task spawns ping and pong itself, so
/// W4b family-local placement co-locates them on ONE shard -- every hop is
/// a same-shard scheduler pass with no interpreter in the loop (raw
/// `internal::task_chan` primitives). Gate: <= 250 ns/hop (W4b measured
/// ~220; P2's own number was 194).
#[test]
#[ignore = "perf: run standalone on a quiet machine"]
fn e1_same_shard_ping_pong_hop_cost() {
    let rounds: u64 = 300_000;
    let a = task_chan::chan(BufferPolicy::Unbuffered);
    let b = task_chan::chan(BufferPolicy::Unbuffered);
    let done = task_chan::chan(BufferPolicy::Unbuffered);

    let t0 = Instant::now();
    let (a0, b0, d0) = (a.clone(), b.clone(), done.clone());
    runtime::spawn(move || {
        // Coordinator: both children are spawned FROM this task.
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
    let elapsed = t0.elapsed();
    let hops = rounds * 2;
    let ns_per_hop = elapsed.as_nanos() as f64 / hops as f64;
    println!(
        "E1 same-shard ping-pong: {ns_per_hop:.1} ns/hop over {hops} hops in {elapsed:?} \
         (resumes {}, direct {})",
        runtime::tasks_resumed(),
        runtime::direct_switches()
    );
    assert!(ns_per_hop <= 250.0, "E1 gate: {ns_per_hop:.1} ns/hop > 250 (W4b measured ~220, P2 194)");
}

/// Cross-shard ping-pong: ping/pong spawned from THIS (root) thread, pure
/// round-robin, so on this 14-core box they land on different shards.
/// Reported only -- no gate (design expects ~2.4 us/hop, the real
/// `Thread::unpark`/futex wake cost).
#[test]
#[ignore = "perf: run standalone on a quiet machine"]
fn e2_cross_shard_ping_pong_hop_cost() {
    let rounds: u64 = 100_000;
    let a = task_chan::chan(BufferPolicy::Unbuffered);
    let b = task_chan::chan(BufferPolicy::Unbuffered);
    let done = task_chan::chan(BufferPolicy::Unbuffered);
    let (a1, b1) = (a.clone(), b.clone());
    runtime::spawn(move || {
        for i in 0..rounds as i64 {
            task_chan::put_int(&a1, i);
            task_chan::take_int(&b1);
        }
    });
    let (a2, b2, d2) = (a.clone(), b.clone(), done.clone());
    runtime::spawn(move || {
        for i in 0..rounds as i64 {
            task_chan::take_int(&a2);
            task_chan::put_int(&b2, i);
        }
        task_chan::put_int(&d2, 1);
    });
    let t0 = Instant::now();
    assert_eq!(task_chan::take_int(&done), Some(1));
    let elapsed = t0.elapsed();
    let hops = rounds * 2;
    let ns_per_hop = elapsed.as_nanos() as f64 / hops as f64;
    println!("E2 cross-shard ping-pong: {ns_per_hop:.1} ns/hop over {hops} hops in {elapsed:?} (report only, no gate)");
}

/// go spawn+complete cost including `Interp::fork`, two ways: COLD (the
/// very first spawn in this process -- pool empty, shard thread not yet
/// started) and STEADY-STATE (warm pool, batches of trivial go bodies
/// spawned then drained). A companion raw (no-interpreter, no fork)
/// steady-state number isolates roughly where `fork` sits, per the design
/// stance: MEASURE, don't optimize.
#[test]
#[ignore = "perf: run standalone on a quiet machine; must be the first runtime::spawn in the process for the cold number to mean anything"]
fn e3_go_spawn_cost_cold_and_steady_state() {
    assert_eq!(runtime::tasks_spawned(), 0, "E3: this test must run alone in a fresh process -- see its #[ignore] reason");

    // COLD: first spawn ever in this process -- pays shard-thread startup.
    let t0 = Instant::now();
    let (tx, rx) = std::sync::mpsc::channel();
    runtime::spawn(move || {
        let _ = tx.send(());
    });
    rx.recv().expect("cold spawn must complete");
    let cold = t0.elapsed();
    println!("E3 cold spawn (pool empty, shard thread cold-started): {cold:?}");

    // STEADY-STATE, WITH Interp::fork: batches of trivial `go` bodies,
    // spawned then drained per batch (so most of a batch's wake latency
    // overlaps instead of serializing one full round trip per task).
    let mut e = engine();
    let warmup = r#"
        (defn one-batch [k]
          (let [chans (loop [i 0 acc []] (if (< i k) (recur (inc i) (conj acc (go i))) acc))]
            (loop [i 0 sum 0] (if (< i k) (recur (inc i) (+ sum (<!! (nth chans i)))) sum))))
        (one-batch 2000)
        "#;
    eval(&mut e, warmup);
    let batches = 20u64;
    let batch_size = 5000u64;
    let t0 = Instant::now();
    let out = eval(
        &mut e,
        &format!(
            r#"(loop [b 0 acc 0] (if (< b {batches}) (recur (inc b) (+ acc (one-batch {batch_size}))) acc))"#
        ),
    );
    let elapsed = t0.elapsed();
    let total_spawns = batches * batch_size;
    let _ = out; // just the checksum of returned ints, not asserted here
    let ns_per_spawn_incl_fork = elapsed.as_nanos() as f64 / total_spawns as f64;

    // Companion: steady-state spawn+complete with NO Interp::fork (raw
    // task_chan primitives), same batching shape, to see where fork sits.
    let native_batches = 20u64;
    let native_batch = 5000u64;
    // warm up the native path the same way
    {
        let done = task_chan::chan(BufferPolicy::Unbuffered);
        for _ in 0..2000u64 {
            let d = done.clone();
            runtime::spawn(move || {
                task_chan::put_int(&d, 1);
            });
            task_chan::take_int(&done);
        }
    }
    let t1 = Instant::now();
    for _ in 0..native_batches {
        let done = task_chan::chan(BufferPolicy::Fixed(native_batch as usize));
        for _ in 0..native_batch {
            let d = done.clone();
            runtime::spawn(move || {
                task_chan::put_int(&d, 1);
            });
        }
        for _ in 0..native_batch {
            task_chan::take_int(&done);
        }
    }
    let native_elapsed = t1.elapsed();
    let native_total = native_batches * native_batch;
    let ns_per_spawn_native = native_elapsed.as_nanos() as f64 / native_total as f64;

    println!(
        "E3 steady-state spawn+complete: {ns_per_spawn_incl_fork:.0} ns/spawn incl. Interp::fork \
         ({total_spawns} spawns in {elapsed:?}); native (no fork, no interpreter) {ns_per_spawn_native:.0} ns/spawn \
         ({native_total} spawns in {native_elapsed:?}); fork's approximate share = {:.0} ns/spawn",
        (ns_per_spawn_incl_fork - ns_per_spawn_native).max(0.0)
    );
    assert!(
        ns_per_spawn_incl_fork <= 2000.0,
        "E3 gate: {ns_per_spawn_incl_fork:.0} ns/spawn incl. fork > 2000 ns design gate"
    );
}

/// S3: W3's additions to `chan_put`/`chan_take` must not cost the pure
/// OS-thread `>!!`/`<!!` path over a `Fixed(1024)` chan -- no tasks are
/// involved anywhere in this test, so every W3 queue here is always empty.
///
/// **W5c rewrote this test's gate, after the first version of it caught a
/// real regression and then proved unable to measure the fix.** Two
/// findings, both worth keeping:
///
/// 1. **The regression was real, and is fixed.** Measured A/B against a
///    worktree at pre-L1 `main` (7b34e3b) with an identical harness: W3 as
///    first landed cost **+24%** on this path (7.87 -> 9.79 ns/op), not the
///    "one load and one branch" S3 predicted. Cause: LLVM hoisted
///    `runtime::in_task()`'s thread-local access -- on aarch64-darwin a
///    call through the TLV descriptor -- into `chan_take`'s PROLOGUE, and
///    inlining W3's park/hand-off machinery grew `chan_take` from 326 to
///    868 instructions (bigger frame, an extra callee-saved pair spilled
///    per call) plus a `Vec<TaskWaker>` constructed on every op. Outlining
///    all of it behind `#[cold] #[inline(never)]` walls and replacing the
///    `Vec` with a no-alloc enum brought it to **8.43 ns/op, +7%** over
///    pre-L1 -- the genuine cost of W3's two queue checks.
///
/// 2. **A two-thread throughput number cannot gate that.** This shape is
///    BIMODAL by ~5x: when the producer outruns the consumer the buffer
///    stays full and nobody parks (~35-45 ns/op, which is where
///    bench/optimization-log.md:1160-1175's 39.1-40.2 ns/op band comes
///    from); when the two run in lockstep every op takes a condvar park
///    (~150-250 ns/op). Which regime a round lands in is a scheduler race,
///    so best-of-5 is not a stable statistic: pre-L1 `main` ITSELF measured
///    best-of-12 anywhere from 32.9 to 70.1 ns/op across interleaved passes
///    on an otherwise-quiet M4 Pro. The original 40.2*1.10 gate is one
///    pre-L1 `main` cannot hold either.
///
/// So the tight gate below is the UNCONTENDED per-op cost (fill 512, drain
/// 512, one thread) -- the same `chan_put`/`chan_take` instructions with
/// the scheduler and the cross-core cache ping-pong taken out. It is
/// reproducible to +/-0.1% run to run, and it is what actually caught both
/// the regression and the fix. The two-thread number is still measured and
/// printed, and still gated, but loosely -- as a smoke test for a
/// catastrophic change in park behaviour, not a 10% ruler.
#[test]
#[ignore = "perf: run standalone on a quiet machine"]
fn e4_buffered_1024_os_thread_throughput() {
    // --- the tight gate: uncontended chan_put + chan_take cost -----------
    const SOLO_BATCH: i64 = 512;
    const SOLO_OPS: i64 = 4_000_000;
    let solo_round = || -> f64 {
        let ch = task_chan::chan(BufferPolicy::Fixed(1024));
        let mut done = 0i64;
        let t0 = Instant::now();
        while done < SOLO_OPS {
            for i in 0..SOLO_BATCH {
                task_chan::put_int(&ch, i);
            }
            for _ in 0..SOLO_BATCH {
                task_chan::take_int(&ch).expect("just buffered SOLO_BATCH values");
            }
            done += SOLO_BATCH * 2;
        }
        t0.elapsed().as_nanos() as f64 / done as f64
    };
    let _solo_warmup = solo_round();
    let mut solo: Vec<f64> = (0..5).map(|_| solo_round()).collect();
    solo.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let solo_ns = solo[0];
    println!("E4 uncontended buffered-1024 put+take: {solo_ns:.2} ns/op best of 5; rounds = {solo:?}");
    // 9.30 ns/op. Measured on a quiet M4 Pro, interleaved A/B, best of 5:
    // pre-L1 main (7b34e3b) 7.87, W3-as-first-landed 9.79, W3 after the
    // outlining fix 8.43 -- so this sits above the fix with room for a
    // slower box and still fails the regression it was written for.
    const SOLO_GATE_NS: f64 = 9.30;
    assert!(
        solo_ns <= SOLO_GATE_NS,
        "E4 uncontended gate: {solo_ns:.2} ns/op > {SOLO_GATE_NS:.2} -- W3 machinery is back on the hot \
         path of chan_put/chan_take. Check that the park/hand-off arms are still behind #[cold] \
         #[inline(never)] walls (an inlined runtime::in_task() puts a TLS thunk in the prologue) and \
         that `Wakes` still allocates nothing when empty."
    );

    // Matches bench/optimization-log.md's own discipline (its doc, ~line
    // 1150: "1 discarded warmup + 5 measured rounds, [min, max]") rather
    // than a single untouched-cache round: a fresh chan and a freshly
    // spawned consumer thread each round, N=2_000_000 (ONE_WAY_N there)
    // per round, 1 warmup discarded, 5 measured, report [min, max]. A
    // first pass at this gate used ONE cold round of 5_000_000 -- no
    // warmup -- and measured 180.7 ns/op, 4.5x the reference band; this
    // rewrite exists to tell a genuine regression apart from first-round
    // cold effects (thread spawn, first page touches, turbo-boost ramp).
    const N: i64 = 2_000_000;
    let one_round = || -> f64 {
        let ch = task_chan::chan(BufferPolicy::Fixed(1024));
        let ch2 = ch.clone();
        let producer = std::thread::spawn(move || {
            for i in 0..N {
                task_chan::put_int(&ch2, i);
            }
        });
        let t0 = Instant::now();
        let mut count = 0i64;
        while count < N {
            task_chan::take_int(&ch).expect("producer thread is still alive and hasn't closed the chan");
            count += 1;
        }
        let elapsed = t0.elapsed();
        producer.join().expect("producer thread");
        elapsed.as_nanos() as f64 / N as f64
    };

    let _warmup = one_round();
    let mut rounds: Vec<f64> = (0..5).map(|_| one_round()).collect();
    rounds.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let (min_ns, max_ns) = (rounds[0], rounds[rounds.len() - 1]);
    println!(
        "E4 buffered-1024 OS-thread throughput: [{min_ns:.1}, {max_ns:.1}] ns/op over 5 rounds of {N} msgs each; \
         rounds = {rounds:?}"
    );
    // Loose by design -- see the test doc. The fast regime is
    // bench/optimization-log.md's 39.1-40.2 ns/op band and the parking
    // regime is 150-250 ns/op, and a scheduler race picks between them, so
    // pre-L1 `main` itself measures best-of-12 anywhere in 32.9-70.1 ns/op
    // on a quiet box. 150 ns/op is therefore not a throughput ruler: it is
    // "a round somewhere managed to stay out of the parking regime", which
    // fails only if park behaviour has broken outright (a lost wake turning
    // into a timeout, a spin that never settles). The 10%-class regression
    // question is answered by the uncontended gate above.
    let gate_ns = 150.0;
    assert!(
        min_ns <= gate_ns,
        "E4 two-thread smoke gate: best of 5 rounds = {min_ns:.1} ns/op > {gate_ns:.1} -- not a throughput \
         regression signal (see the uncontended gate for that) but a sign that NO round escaped the \
         park-every-op regime, i.e. park/wake behaviour on this path has changed shape"
    );
}

// ============================================================================
// P3. L2 DIRECT-SWITCH BENCHES (docs/L2-LANDING-SPEC.md §W1 item 6)
// ============================================================================

/// **The scheduler-pass floor.** E1's shape -- two tasks co-located on ONE
/// shard by family-local placement, because a coordinator task spawns them
/// -- with NO channels anywhere in the loop: each task owns an `AtomicU8`
/// flag and publishes its `TaskWaker` once into a `OnceLock`, and a hop is
/// `flag.store(1); partner.wake()` on one side against `flag.swap(0) == 1
/// else park_current_yield()` on the other.
///
/// So this measures the runtime pass and nothing else: one `wake()` CAS, one
/// enqueue (the runnext slot, or the inbox when the slot is busy), one
/// shard-loop pop, one `tasks` map remove/insert, one ctx/binding swap, one
/// corosensei switch. **`E1 - P3` is therefore the chan tax**, and that split
/// is the whole reason this bench exists: it is what told the L2 design that
/// its ≤ 25 ns/hop gate was below the floor (measured 36.2 ns with all three
/// levers in, docs/L2-PROBE-RESULTS.md §1/§6) and re-gated the landing at
/// ≤ 65. W1 reports it beside E1; W2 attacks it directly.
///
/// Reported, not gated -- the gate lives on E1. Best-of-5 separate processes,
/// discarding run 1 (always cold by 30-90 ns/hop: first touch of the stack
/// pool, shard-thread cold start, turbo ramp).
///
/// The re-check-after-resume discipline is `park_task_putter`'s, verbatim in
/// spirit: on resume re-read the flag and re-park if it is still 0, and
/// consume it with a `swap`. A wake that lands while its target is still
/// RUNNING leaves NOTIFIED behind and the shard re-queues it -- the
/// missed-wakeup arm, exercised on roughly every hop that races.
#[test]
#[ignore = "perf: run standalone on a quiet machine"]
fn p3_scheduler_only_ping_pong() {
    use std::sync::OnceLock;

    let _wd = Watchdog::arm(120, "p3_scheduler_only_ping_pong");
    let rounds: u64 = 300_000;
    let done = task_chan::chan(BufferPolicy::Unbuffered);

    let flag_ping = Arc::new(AtomicU8::new(0));
    let flag_pong = Arc::new(AtomicU8::new(0));
    let waker_ping: Arc<OnceLock<runtime::TaskWaker>> = Arc::new(OnceLock::new());
    let waker_pong: Arc<OnceLock<runtime::TaskWaker>> = Arc::new(OnceLock::new());

    let t0 = Instant::now();
    let (fi, fo, wi, wo, d0) =
        (flag_ping, flag_pong, waker_ping, waker_pong, done.clone());
    runtime::spawn(move || {
        // Coordinator: both children are spawned FROM this task, so W4b
        // family-local placement lands them on this shard (E1's trick).
        let (f1, f2, w1, w2) = (fi.clone(), fo.clone(), wi.clone(), wo.clone());
        runtime::spawn(move || {
            // ping: signal, then wait.
            let _ = w1.set(runtime::current_waker());
            for _ in 0..rounds {
                f2.store(1, Ordering::Release);
                if let Some(w) = w2.get() {
                    w.wake();
                }
                while f1.swap(0, Ordering::AcqRel) != 1 {
                    runtime::park_current_yield();
                }
            }
        });
        let (f1, f2, w1, w2, d2) = (fi, fo, wi, wo, d0);
        runtime::spawn(move || {
            // pong: wait, then signal.
            let _ = w2.set(runtime::current_waker());
            for _ in 0..rounds {
                while f2.swap(0, Ordering::AcqRel) != 1 {
                    runtime::park_current_yield();
                }
                f1.store(1, Ordering::Release);
                if let Some(w) = w1.get() {
                    w.wake();
                }
            }
            task_chan::put_int(&d2, 1);
        });
    });
    assert_eq!(task_chan::take_int(&done), Some(1));
    let elapsed = t0.elapsed();
    let hops = rounds * 2;
    let ns_per_hop = elapsed.as_nanos() as f64 / hops as f64;
    println!(
        "P3 scheduler-only ping-pong: {ns_per_hop:.1} ns/hop over {hops} hops in {elapsed:?} \
         (resumes {}, direct {})",
        runtime::tasks_resumed(),
        runtime::direct_switches()
    );
}

/// **The BUDGET=16 latency table, reproducible.** One coordinator spawns FIVE
/// tasks onto its own shard: two ping-pong pairs (each on its own two
/// unbuffered chans) plus a third task that needs ordinary scheduler turns --
/// it self-wakes (`current_waker().wake()` on a RUNNING task leaves NOTIFIED,
/// so the shard re-queues it onto `local`) and parks, 10 000 times.
///
/// The third task takes part in no rendezvous, so it is reachable ONLY
/// through `local`, which the runnext slot outranks. That makes this the
/// shape that priced [`DIRECT_SWITCH_BUDGET`](mova::runtime) --
/// docs/L2-PROBE-RESULTS.md §4:
///
/// | BUDGET | third task done at | everything done at | pair ns/hop |
/// |---|---|---|---|
/// | 16 (landed) | **10.6-10.9 ms** | 51.6-52.8 ms | 64.5-65.9 |
/// | 64 | 25.8-26.3 ms | 51.3-51.7 ms | 64.1-64.6 |
/// | 256 | 26.1-26.3 ms | 51.4-51.8 ms | 64.3-64.8 |
/// | none | 50.021 ms (= end of run) | 50.021 ms | 62.5 |
///
/// Read the last row carefully: with no budget nothing DEADLOCKS, but the
/// third task's work is squeezed entirely into the gaps and finishes at the
/// very end -- and the second pair's two tasks spend that whole run as
/// unstarted `Job::Spawn`s in the inbox behind a saturated slot. That is why
/// the budget is stated as a liveness rule.
///
/// The ORDERING half of this is a real gate and runs in the default suite:
/// `tests/l2_direct_switch_test.rs`'s
/// `budget_turn_lets_a_co_located_task_finish_first`. This one is the
/// wall-clock instrument that produces the numbers in the table above.
#[test]
#[ignore = "perf: run standalone on a quiet machine"]
fn p3_fairness_third_task() {
    let _wd = Watchdog::arm(120, "p3_fairness_third_task");
    let rounds: u64 = 200_000;
    let third_turns: u64 = 10_000;

    let done = task_chan::chan(BufferPolicy::Fixed(8));
    let counter = Arc::new(AtomicU64::new(0));

    let t0 = Instant::now();
    let (d0, c0) = (done.clone(), counter.clone());
    runtime::spawn(move || {
        for pair in 1..=2i64 {
            let a = task_chan::chan(BufferPolicy::Unbuffered);
            let b = task_chan::chan(BufferPolicy::Unbuffered);
            let (a1, b1) = (a.clone(), b.clone());
            runtime::spawn(move || {
                for i in 0..rounds as i64 {
                    task_chan::put_int(&a1, i);
                    task_chan::take_int(&b1);
                }
            });
            let (a2, b2, d2) = (a, b, d0.clone());
            runtime::spawn(move || {
                for i in 0..rounds as i64 {
                    task_chan::take_int(&a2);
                    task_chan::put_int(&b2, i);
                }
                task_chan::put_int(&d2, pair);
            });
        }
        let (d3, c3) = (d0, c0);
        runtime::spawn(move || {
            for _ in 0..third_turns {
                c3.fetch_add(1, Ordering::Relaxed);
                runtime::current_waker().wake();
                runtime::park_current_yield();
            }
            task_chan::put_int(&d3, 3);
        });
    });

    let mut third_at: Option<Duration> = None;
    let mut seen = Vec::new();
    while seen.len() < 3 {
        let v = task_chan::take_int(&done).expect("the done chan is never closed");
        if v == 3 {
            third_at = Some(t0.elapsed());
        }
        seen.push(v);
    }
    let all = t0.elapsed();
    let third = third_at.expect("the third task must report");
    seen.sort();
    assert_eq!(seen, vec![1, 2, 3]);
    assert_eq!(counter.load(Ordering::Relaxed), third_turns);
    let pairs_hops = rounds * 2 * 2;
    println!(
        "P3-FAIR (BUDGET=16): third task ({third_turns} turns) done at {third:?}; all done at {all:?}; \
         pairs {pairs_hops} hops -> {:.1} ns/hop (both pairs share one shard, so this is 2x-contended); \
         resumes {}, direct {}",
        all.as_nanos() as f64 / pairs_hops as f64,
        runtime::tasks_resumed(),
        runtime::direct_switches()
    );
}
