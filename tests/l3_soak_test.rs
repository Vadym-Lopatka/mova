//! L3 W4 (docs/L3-LANDING-SPEC.md's G-SOAK gate, docs/L3-FLOW-PROCS-DESIGN.md
//! §6): the same no-net soak hammer `tests/task_stress_test.rs`'s `B` runs
//! (seed `0x5EEDC0FFEE`, 10 000 tasks x 64 shared chans x a randomized
//! `>!!`/`<!!`/`poll!`/`offer!`/`alts!!` op mix, `Doorbell::safety_net_hits()`
//! asserted 0 -> 0), extended additively with a 50-proc flow chain -- the
//! DEFAULT task world landed by W1b -- flowing 2,000,000 messages through
//! itself concurrently with the hammer, on the same shared runtime shards.
//!
//! This is the wave's own claim, not a restatement of `B`'s: `B` never
//! touches a flow proc, so it says nothing about a task-runtime park site
//! that only a flow proc reaches (the blocked-send doorbell-family arm,
//! §3.4; the done-cell shutdown path, §3.5). Running both hammers in the
//! SAME process, on the SAME shards, at the SAME time is what makes a
//! flow-proc-specific missed wake or safety-net fallback show up as a
//! failure here instead of hiding behind either hammer's own isolation.
//!
//! House pattern borrowed byte-for-byte from `task_stress_test.rs`'s `B`
//! (duplicated rather than shared, per this wave's file ownership: `B` is
//! not touched): a `Watchdog` aborts the PROCESS on a genuine hang (S2 --
//! task parks carry no safety-net timeout by design, so a hang here is a
//! missed-ring bug to report, never a reason to raise a timeout), and the
//! hammer's own liveness backstop (`close-all-chans!`, polled from THIS
//! non-shard OS thread, never from inside a `go` block -- see `B`'s own
//! module doc for the self-livelock story that ruled out a task-side
//! backstop) is unchanged. The flow chain gets NO backstop of its own: a
//! 50-proc linear relay with one injector and one sink has no cycle and no
//! shared-chan contention with any other task, so (unlike the hammer's
//! random cross-chan `alts!!` mix) it cannot deadlock by construction --
//! its only failure mode is a genuine missed wake, which the bounded
//! `await_expr_at_least` below still catches and reports rather than hangs
//! on.
//!
//! Run standalone, its own process:
//! ```text
//! cargo test --release --test l3_soak_test -- --ignored --nocapture
//! ```

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use mova::embed::{Engine, Value};
use mova::internal::task_chan::Doorbell;

fn engine() -> Engine {
    Engine::builder().build()
}

fn eval(e: &mut Engine, src: &str) -> Value {
    e.eval_named("l3-soak", src).unwrap_or_else(|err| panic!("eval error: {}", err.render_plain()))
}

/// Same shape as `task_stress_test.rs`'s `Watchdog`: aborts the PROCESS (not
/// just the test) with a diagnostic if the guarded section does not finish
/// in time, so a lost-wake hang is reported instead of wedging the harness.
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
                "L3 SOAK WATCHDOG: {what} did not finish in {secs}s -- lost wake / deadlock, per S2 this is a bug to report, not a timeout to raise"
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

/// Polls `expr` (a bare mova expression, e.g. `"@flow-msgs-done"`) every
/// 50ms until it parses to an i64 >= `target`, or panics with a pointed
/// diagnostic after `budget` -- the flow chain's own liveness check,
/// independent of the hammer's plateau/backstop loop below.
fn await_expr_at_least(e: &mut Engine, what: &str, expr: &str, target: i64, budget: Duration) -> i64 {
    let deadline = Instant::now() + budget;
    loop {
        let v: i64 = eval(e, expr).to_string().parse().unwrap_or(0);
        if v >= target {
            return v;
        }
        if Instant::now() >= deadline {
            panic!(
                "l3_soak_test: timed out after {budget:?} waiting for {what} (last value {v}, target {target}) \
                 -- a wake was lost or the flow chain wedged"
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

// ============================================================================
// The same seeded chan-op hammer as `task_stress_test.rs`'s `B`.
// ============================================================================

const SOAK_SEED: i64 = 0x5EED_C0FFEE; // identical seed to B, for reproducibility
const SOAK_TASKS: i64 = 10_000;
const SOAK_CHANS: i64 = 64;
const SOAK_QUOTA: i64 = 400; // 10_000 * 400 = 4_000_000 randomized hammer ops
const SOAK_CLOSEABLE: i64 = 8;

// ============================================================================
// The new W4 half: a 50-proc flow chain in the DEFAULT (task) world, flowing
// 2_000_000 messages continuously alongside the hammer above. Buffers are
// deliberately small (8/proc) so the chain exercises real backpressure --
// the blocked-send doorbell-family task arm (design §3.4) -- across every
// one of its 49 hops, not just the take side.
// ============================================================================

const FLOW_PROC_COUNT: usize = 50;
const FLOW_MSGS: i64 = 2_000_000;
const FLOW_IN_BUF: i64 = 8;
const FLOW_SINK_BUF: i64 = 64;

/// Builds the `:procs` map body and `:conns` vector body for an N-proc
/// linear relay chain `:p0 -> :p1 -> ... -> :p{N-1}`, the last proc wired to
/// `flow-sink` (forwards onto `flow-sink-ch`) instead of `relay`.
fn flow_chain_procs_and_conns(n: usize, buf: i64) -> (String, String) {
    let mut procs = String::new();
    for i in 0..n {
        let step = if i + 1 == n { "flow-sink" } else { "relay" };
        procs.push_str(&format!(":p{i} {{:proc (flow/process {step}) :chan-opts {{:in {{:buf-or-n {buf}}}}}}} "));
    }
    let mut conns = String::new();
    for i in 0..n.saturating_sub(1) {
        let next = i + 1;
        conns.push_str(&format!("[[:p{i} :out] [:p{next} :in]] "));
    }
    (procs, conns)
}

#[test]
#[ignore = "slow: run standalone, see module doc"]
fn g_soak_hammer_plus_50proc_flow_chain() {
    println!(
        "G-SOAK: seed = 0x{SOAK_SEED:X}, tasks = {SOAK_TASKS}, chans = {SOAK_CHANS}, quota = {SOAK_QUOTA}/task, \
         flow procs = {FLOW_PROC_COUNT}, flow msgs = {FLOW_MSGS}"
    );
    let safety_net_before = Doorbell::safety_net_hits();

    let mut e = engine();
    let _w = Watchdog::arm(300, "G-SOAK hammer + flow chain");
    let t0 = Instant::now();

    let (procs, conns) = flow_chain_procs_and_conns(FLOW_PROC_COUNT, FLOW_IN_BUF);

    let src = format!(
        r#"
        (def seed {SOAK_SEED})
        (def num-tasks {SOAK_TASKS})
        (def num-chans {SOAK_CHANS})
        (def quota {SOAK_QUOTA})
        (def closeable-start (- num-chans {SOAK_CLOSEABLE}))

        ;; 64 chans, 5 buffer policies round-robin -- identical to B.
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

        ;; Pure xorshift32 -- identical to B, deterministic by seed alone.
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

        ;; A dedicated closer task: mid-soak, close! the designated
        ;; closeable subset -- identical to B.
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

        ;; ---- L3 W4 addition: a {FLOW_PROC_COUNT}-proc flow chain in the
        ;; DEFAULT task world, flowing {FLOW_MSGS} messages continuously
        ;; while the hammer above runs on the same shared runtime shards. ----
        (def relay (flow/map->step {{:describe (fn [] {{:ins {{:in {{}}}} :outs {{:out {{}}}}}})
                                      :transform (fn [s _ m] [s {{:out [m]}}])}}))
        (def flow-sink-ch (chan {FLOW_SINK_BUF}))
        (def flow-sink (flow/map->step {{:describe (fn [] {{:ins {{:in {{}}}} :outs {{}}}})
                                          :transform (fn [s _ m] (>!! flow-sink-ch m) [s {{}}])}}))
        (def fl (flow/create-flow
                  {{:procs {{{procs}}}
                    :conns [{conns}]}}))
        (flow/start fl)
        (flow/resume fl)

        (def flow-msgs-done (atom 0))
        ;; One injector OS thread, one blocking put per message (backpressure
        ;; paces it to the chain's real drain rate) -- `flow/inject`'s
        ;; existing, already-proven shape (design §3.7: unchanged in L3).
        (flow/inject fl [:p0 :in] (range {FLOW_MSGS}))
        ;; One task draining the far end -- exercises the take-side doorbell
        ;; arm (§3.3) {FLOW_MSGS} times over, concurrently with the hammer's
        ;; 10 000 tasks on the same shards.
        (go
          (loop [i 0]
            (if (< i {FLOW_MSGS})
              (do (<!! flow-sink-ch) (swap! flow-msgs-done inc) (recur (inc i)))
              nil)))

        (loop [i 0]
          (if (< i num-tasks)
            (do (run-task i) (recur (inc i)))
            nil))
        :spawned
        "#
    );
    eval(&mut e, &src);

    // Hammer liveness backstop -- byte-for-byte B's plateau/hard-cap
    // discipline, polled from this (main, non-shard) OS thread. See B's own
    // module doc for why: a task-side poll loop with `sleep-ms` in a tight
    // recur self-livelocks its shard, which is exactly the failure mode a
    // liveness backstop exists to prevent.
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
                "G-SOAK: hammer ops-done plateaued at {ops_now} (tasks-done={tasks_done}/{SOAK_TASKS}) for {PLATEAU:?} \
                 -- invoking the hammer's liveness backstop (close-all-chans!)"
            );
            eval(&mut e, "(close-all-chans!)");
            break;
        }
        if Instant::now() >= hard_deadline {
            closed_early = true;
            println!(
                "G-SOAK: hammer hard cap {HARD_CAP:?} reached at ops-done={ops_now} tasks-done={tasks_done}/{SOAK_TASKS} \
                 -- invoking the liveness backstop anyway"
            );
            eval(&mut e, "(close-all-chans!)");
            break;
        }
        std::thread::sleep(POLL_EVERY);
    }
    if closed_early {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let tasks_done: i64 = eval(&mut e, "@tasks-done").to_string().parse().unwrap_or(0);
            if tasks_done >= SOAK_TASKS {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "G-SOAK: timed out waiting for stragglers to reach tasks-done == SOAK_TASKS after the backstop close"
            );
            std::thread::sleep(POLL_EVERY);
        }
    }

    // Block for every hammer task's completion signal -- identical to B.
    let out = eval(
        &mut e,
        r#"(loop [i 0 seen #{}]
             (if (< i num-tasks)
               (recur (inc i) (conj seen (<!! done-ch)))
               (count seen)))"#,
    );
    let hammer_elapsed = t0.elapsed();
    let distinct_finishers: i64 = out.to_string().parse().expect("distinct-finisher count");
    assert_eq!(distinct_finishers, SOAK_TASKS, "G-SOAK: every hammer task must signal done-ch exactly once, with a distinct id");
    println!("G-SOAK: hammer backstop invoked = {closed_early}; hammer wall time = {hammer_elapsed:?}");

    let ops_done = eval(&mut e, "@ops-done").to_string();
    let puts_true = eval(&mut e, "@puts-true").to_string();
    let puts_false = eval(&mut e, "@puts-false").to_string();
    let takes_some = eval(&mut e, "@takes-some").to_string();
    let takes_nil = eval(&mut e, "@takes-nil").to_string();
    let alts_done = eval(&mut e, "@alts-done").to_string();
    let polls_done = eval(&mut e, "@polls-done").to_string();
    let offers_done = eval(&mut e, "@offers-done").to_string();
    let expected_ops = (SOAK_TASKS * SOAK_QUOTA).to_string();
    assert_eq!(ops_done, expected_ops, "G-SOAK: total hammer op count must equal tasks * quota exactly");

    // The flow chain's own liveness proof: every one of the 2_000_000
    // injected messages must have reached the sink. No backstop is needed
    // (see module doc) -- a genuine missed wake here is a real bug, so this
    // is a bounded wait that panics with evidence, not a silent skip.
    let flow_msgs_done =
        await_expr_at_least(&mut e, "the flow chain to deliver every injected message", "@flow-msgs-done", FLOW_MSGS, Duration::from_secs(120));
    let flow_elapsed = t0.elapsed();
    assert_eq!(flow_msgs_done, FLOW_MSGS, "G-SOAK: flow chain must deliver every one of the {FLOW_MSGS} injected messages exactly once");
    println!("G-SOAK: flow chain delivered {flow_msgs_done}/{FLOW_MSGS} messages through {FLOW_PROC_COUNT} procs; total elapsed = {flow_elapsed:?}");

    eval(&mut e, "(flow/stop fl)");

    let total_elapsed = t0.elapsed();
    let total_ops = SOAK_TASKS * SOAK_QUOTA + FLOW_MSGS;
    let safety_net_after = Doorbell::safety_net_hits();
    println!(
        "G-SOAK: hammer {SOAK_TASKS} tasks x {SOAK_QUOTA} ops (puts_true={puts_true} puts_false={puts_false} \
         takes_some={takes_some} takes_nil={takes_nil} alts_done={alts_done} polls={polls_done} offers={offers_done}) \
         + flow {FLOW_MSGS} msgs through {FLOW_PROC_COUNT} procs = {total_ops} total ops in {total_elapsed:?}; \
         safety_net_hits {safety_net_before} -> {safety_net_after}"
    );
    assert!(
        total_ops >= 2_000_000,
        "G-SOAK: combined op count {total_ops} fell under the wave's 2,000,000-op floor"
    );
    assert_eq!(
        safety_net_after, safety_net_before,
        "G-SOAK: Doorbell safety-net fallback fired {} time(s) during a soak that should exercise ONLY task \
         parks (no-net by design) -- report this delta, it means an OS-thread doorbell wait fell back \
         to its timeout somewhere in this run",
        safety_net_after - safety_net_before
    );
}
