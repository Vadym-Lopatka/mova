//! L4 W4 (docs/L4-LANDING-SPEC.md's W4 section, gate G-SOAK per
//! docs/L4-SUPERVISION-DESIGN.md §5): `tests/l3_soak_test.rs`'s no-net
//! hammer + 50-proc flow chain, extended ADDITIVELY with a small SUPERVISED
//! flow whose procs die and restart repeatedly for the whole soak window,
//! all three running concurrently on the same shared runtime shards.
//!
//! L3's soak said nothing about the supervisor: no flow proc it touches is
//! ever configured with `:supervision`, so `sup_chan`, [`run_supervisor`]'s
//! park (a COMBINED {chan ring OR timer deadline} wait, `src/builtins/
//! flow.rs`'s "L4 W2/W3 SUPERVISOR / POLICY REGION" banner) and the
//! `ExitGuard` death-event path never fire under it. Running the hammer, the
//! healthy chain, AND a crashing supervised flow in the SAME process on the
//! SAME shards is what makes a supervisor-specific missed wake, a
//! backoff-park bug, or a restart racing the hammer's own task churn show up
//! as a failure here instead of hiding behind three separate hammers' own
//! isolation.
//!
//! **The crash trigger, and why it needs no `resume-proc`.** Per
//! `tests/l4_supervision_test.rs`'s own module doc: the one proc-killing
//! panic reachable from Mova is `call_transition`'s unguarded call on the
//! `::flow/pause` transition, reached through `flow/pause-proc`. What makes
//! it a *repeated*-death driver with no companion `resume-proc` needed:
//! `apply_control`'s `"pause"` arm (`src/builtins/flow.rs`) calls
//! `call_transition` UNCONDITIONALLY, regardless of the proc's current
//! run-status -- so a proc that has JUST been restarted (and, per design
//! §3.4, comes back PAUSED, exactly like a freshly started proc) still
//! panics again the next time `pause-proc` reaches it. A tight loop that
//! does nothing but call `flow/pause-proc` on a rotating set of pids is
//! therefore the simplest reliable repeated-death driver: no `resume-proc`,
//! no message injection, no synchronization beyond the control chan's own
//! backpressure (`CONTROL_BUF` = 10 slots; a persistent control chan
//! survives every incarnation, so a queued command is never lost across a
//! restart, only delayed until the new incarnation starts draining it).
//!
//! **House pattern borrowed byte-for-byte from `l3_soak_test.rs`**: a
//! process-aborting [`Watchdog`] for genuine hangs, the hammer's own
//! plateau/hard-cap liveness backstop polled from this (non-shard) OS
//! thread, and the flow chain's own bounded `await_expr_at_least` wait
//! instead of a silent skip.
//!
//! # RETRACTED FINDING -- a test-harness bug, not a runtime bug.
//!
//! An earlier version of this file's crash-driver loop used `(sleep-ms n)`
//! between crash triggers and reported a hang under it as a "pre-existing,
//! panic-independent runtime bug." That diagnosis was WRONG, found wrong
//! under direct challenge (thread-stack sampling via `sample`(1) on the
//! stuck process), and the real cause is a self-inflicted test-harness bug:
//! **`sleep-ms` (`src/builtins/conc.rs`) is a literal `std::thread::sleep`,
//! not task-aware.** Inside a `go` task it blocks that task's SHARD, not
//! just the task, for the call's whole duration; an INFINITE
//! `(go (loop [] ... (sleep-ms n) (recur)))` -- which is exactly what the
//! original driver was -- therefore monopolizes one shard for the test's
//! entire life. Sampling the stuck process confirmed it directly: that
//! shard's thread sat in `nanosleep`/`__semwait_signal` in essentially every
//! sample over minutes, while every OTHER shard sat fully idle
//! (`semaphore_wait_trap`, correctly out of work) -- consistent with roughly
//! one shard's worth of the hammer's 2,000 tasks (the fraction that needed
//! that specific shard) starving indefinitely while the rest drained fine. A
//! CONTROL run confirmed it further: replacing `(go (test/boom))` with a
//! bare `nil` in the same infinite `(sleep-ms n)` loop reproduced the
//! IDENTICAL stuck value -- panics were never the variable at all. This is
//! precisely the footgun `l3_soak_test.rs`'s own module doc already names
//! ("a task-side poll loop with `sleep-ms` in a tight recur self-livelocks
//! its shard") -- a warning this file's own crash-driver failed to heed
//! despite quoting it. The fix is the one-line swap below: the driver loop
//! now paces itself with `(<!! (timeout n))` (task-parked, via the proper
//! timer service) instead of `sleep-ms`, exactly like every other wait in
//! this file and in `l3_soak_test.rs`'s own hammer. With that fix, both soak
//! tests below pass cleanly and repeatably (fast: ~10s wall, 300 restarts;
//! big, at L3's original scale: ~25s wall, 1152 restarts; safety_net_hits
//! 0->0 both times) -- no supervision/runtime bug of any kind was ever
//! present. Neither test is `#[ignore]`d for this reason anymore.
//!
//! **Two sizes, per the landing spec's own instruction** ("keep the
//! default-suite variant <=60s; put a bigger version behind `#[ignore]` with
//! env-tunable duration"):
//! - [`g_soak_hammer_plus_flow_chain_plus_crashing_flow`] -- NOT `#[ignore]`d,
//!   sized to finish comfortably inside 60s (measured ~10s), part of the
//!   default `cargo test` run.
//! - [`g_soak_hammer_plus_flow_chain_plus_crashing_flow_big`] -- `#[ignore]`d,
//!   at (or above) `l3_soak_test.rs`'s original scale, every size knob
//!   overridable via `L4_SOAK_*` env vars (`iters()`-style, `l4_kill_probe.rs`
//!   precedent) for a longer or shorter run without editing the file.
//!
//! A THIRD test, [`restart_churn_resource_counters_return_to_baseline`], is
//! item 2 of this wave's W4 checklist (restart-churn leak check): it does
//! NOT run concurrently with the two above (it needs the runtime's
//! PROCESS-GLOBAL resource counters -- `tasks_spawned`/`tasks_finished`/
//! `stacks_mmaped` -- to mean something, exactly the isolation
//! `l4_kill_probe.rs`'s own test C already establishes the precedent for),
//! so it is `#[ignore]`d and documents the same `--test-threads=1`
//! requirement.
//!
//! Run the two soak tests standalone, their own process:
//! ```text
//! cargo test --release --test l4_soak_test -- --ignored --nocapture
//! ```
//! Run the churn test alone (it is PROCESS-GLOBAL-counter-sensitive, like
//! `l4_kill_probe.rs`'s test C):
//! ```text
//! cargo test --release --test l4_soak_test restart_churn -- --ignored --test-threads=1 --nocapture
//! ```

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use mova::embed::{Engine, Value};
use mova::internal::task_chan::Doorbell;
use mova::runtime;

/// An `Engine` with `test/boom` registered -- the one host native every
/// crash driver in this file calls from a step's `:transition` arity on the
/// `::flow/pause` transition (see this file's module doc).
fn engine() -> Engine {
    let mut e = Engine::builder().build();
    e.register_fn("test/boom", |_args: &[Value]| {
        panic!(
            "l4_soak_test: deliberate panic exercising call_transition's unguarded path \
             (the ONE proc-killing panic reachable from Mova, per l4_supervision_test.rs's \
             own module doc)"
        )
    });
    e
}

fn eval(e: &mut Engine, src: &str) -> Value {
    e.eval_named("l4-soak", src).unwrap_or_else(|err| panic!("eval error: {}", err.render_plain()))
}

/// Same shape as `l3_soak_test.rs`'s `Watchdog`: aborts the PROCESS (not
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
                "L4 SOAK WATCHDOG: {what} did not finish in {secs}s -- lost wake / deadlock / \
                 wedged supervisor, this is a bug to report, not a timeout to raise"
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
/// diagnostic after `budget`.
fn await_expr_at_least(e: &mut Engine, what: &str, expr: &str, target: i64, budget: Duration) -> i64 {
    let deadline = Instant::now() + budget;
    loop {
        let v: i64 = eval(e, expr).to_string().parse().unwrap_or(0);
        if v >= target {
            return v;
        }
        if Instant::now() >= deadline {
            panic!(
                "l4_soak_test: timed out after {budget:?} waiting for {what} (last value {v}, target {target}) \
                 -- a wake was lost or something wedged"
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Polls `expr` (a bare mova boolean expression, e.g. `"@crash-drain-done"`)
/// every 20ms until it prints `true`, or panics after `budget` -- the
/// crash-report drain loop's own shutdown handshake.
fn await_expr_true(e: &mut Engine, what: &str, expr: &str, budget: Duration) {
    let deadline = Instant::now() + budget;
    loop {
        if eval(e, expr).to_string() == "true" {
            return;
        }
        if Instant::now() >= deadline {
            panic!("l4_soak_test: timed out after {budget:?} waiting for {what}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn env_or<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// Resident set size in KiB, straight out of `ps` -- `l4_kill_probe.rs`'s own
/// `rss_kib` idiom, duplicated (no dev-dependency, no claim of precision
/// beyond "did this grow by megabytes").
fn rss_kib() -> u64 {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .expect("ps");
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap_or(0)
}

// ============================================================================
// The same seeded chan-op hammer as `l3_soak_test.rs`'s (== `task_stress_
// test.rs`'s `B`'s) -- byte-for-byte the same generator, sizes overridable.
// ============================================================================

const SOAK_SEED: i64 = 0x5EED_C0FFEE; // identical seed to B/L3, for reproducibility
const SOAK_CHANS: i64 = 64;
const SOAK_CLOSEABLE: i64 = 8;

/// Builds the `:procs`/`:conns` body for an N-proc linear relay chain --
/// `l3_soak_test.rs`'s `flow_chain_procs_and_conns`, duplicated (this
/// wave's own file, not a shared dependency on L3's).
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

/// Builds the `:procs` map body for `n` independent, sink-shaped, SUPERVISED
/// crash procs `:c0 .. :c{n-1}` -- each one `l4_supervision_test.rs`'s own
/// `CRASH_STEP` shape (`:ins {:in {}} :outs {}`, no data ever needed: only
/// `flow/pause-proc`'s control command drives the death), sharing ONE
/// `crash-step` value (proven safe by `flow_chain_procs_and_conns` above
/// already sharing `relay` across every non-tail proc -- state is threaded
/// PER PROC by the engine, never held in the step value itself).
fn crash_procs_map(n: usize, max_restarts: i64, window_ms: i64, initial_ms: i64, factor: f64, max_ms: i64) -> String {
    let mut procs = String::new();
    for i in 0..n {
        procs.push_str(&format!(
            ":c{i} {{:proc (flow/process crash-step) \
             :supervision {{:policy :restart :max-restarts {max_restarts} :window-ms {window_ms} \
             :backoff {{:initial-ms {initial_ms} :factor {factor} :max-ms {max_ms}}}}}}} "
        ));
    }
    procs
}

fn crash_pids_vec(n: usize) -> String {
    (0..n).map(|i| format!(":c{i}")).collect::<Vec<_>>().join(" ")
}

/// Every size/timing knob for one run of the combined soak, so the fast
/// default and the big `#[ignore]`d variant are ONE code path (`run_g_soak`)
/// called with two different `SoakSizes` rather than two near-duplicate test
/// bodies.
struct SoakSizes {
    tag: &'static str,
    soak_tasks: i64,
    soak_quota: i64,
    flow_proc_count: usize,
    flow_msgs: i64,
    flow_in_buf: i64,
    flow_sink_buf: i64,
    crash_proc_count: usize,
    crash_max_restarts: i64,
    crash_window_ms: i64,
    crash_initial_ms: i64,
    crash_factor: f64,
    crash_max_ms: i64,
    /// How long each per-pid crash-driver loop task-parks (via
    /// `(<!! (timeout n))`) between one `flow/pause-proc` crash trigger and
    /// the next. Load-bearing in TWO ways, both measured (see this file's
    /// module doc's "corrected-finding" note for the full story):
    /// - Some pacing at all: an UNPACED driver (bounded only by the control
    ///   chan's own 10-slot backpressure) panics/restarts fast enough
    ///   across `crash_proc_count` procs to be a genuine CPU hog next to
    ///   the hammer (the default panic path's two `eprintln!`s per death,
    ///   serialized on stderr's global lock, are the likely cost center).
    /// - **`(<!! (timeout n))`, never `sleep-ms`.** `sleep-ms`
    ///   (`src/builtins/conc.rs`) is a literal `std::thread::sleep` --
    ///   correct for a real OS thread, but INSIDE a `go` task it blocks the
    ///   task's SHARD, not just the task, for the full duration, every
    ///   iteration, forever (the exact footgun `l3_soak_test.rs`'s own
    ///   module doc already names: "a task-side poll loop with `sleep-ms`
    ///   in a tight recur self-livelocks its shard"). An infinite
    ///   `(go (loop [] ... (sleep-ms n) (recur)))` driver monopolizes one
    ///   shard for the test's ENTIRE life, starving whatever fraction of
    ///   the hammer's tasks needed that one shard -- confirmed by thread
    ///   sampling (`sample`(1)) during an earlier (retracted) version of
    ///   this test that used `sleep-ms` here: that shard's thread sat in
    ///   `nanosleep`/`__semwait_signal` in >99% of samples over minutes,
    ///   while every OTHER shard sat fully idle (`semaphore_wait_trap`,
    ///   nothing left to do) -- a self-inflicted test-harness bug, not a
    ///   supervision/runtime bug. `(<!! (timeout n))` goes through the
    ///   proper timer-service park instead, which is what every OTHER
    ///   task-side wait in this file (and `l3_soak_test.rs`'s own hammer)
    ///   already uses.
    crash_pace_ms: i64,
    /// A floor on how long the crash flow stays alive and dying, regardless
    /// of how fast the hammer+chain above happen to finish -- "for the
    /// whole soak window" (this wave's own instruction) means the crash
    /// flow must keep restarting for a REAL span of wall-clock time, not
    /// just for however long the hammer happens to take.
    min_crash_run: Duration,
    watchdog_secs: u64,
    hard_cap: Duration,
    flow_wait_budget: Duration,
    driver_stop_wait: Duration,
    restart_floor: i64,
}

fn run_g_soak(sizes: SoakSizes) {
    println!(
        "G-SOAK[{}]: seed = 0x{SOAK_SEED:X}, tasks = {}, chans = {SOAK_CHANS}, quota = {}/task, \
         flow procs = {}, flow msgs = {}, crash procs = {}, restart floor = {}",
        sizes.tag,
        sizes.soak_tasks,
        sizes.soak_quota,
        sizes.flow_proc_count,
        sizes.flow_msgs,
        sizes.crash_proc_count,
        sizes.restart_floor
    );
    let safety_net_before = Doorbell::safety_net_hits();

    let mut e = engine();
    let _w = Watchdog::arm(sizes.watchdog_secs, "G-SOAK hammer + flow chain + crashing flow");
    let t0 = Instant::now();

    let (procs, conns) = flow_chain_procs_and_conns(sizes.flow_proc_count, sizes.flow_in_buf);
    let crash_procs = crash_procs_map(
        sizes.crash_proc_count,
        sizes.crash_max_restarts,
        sizes.crash_window_ms,
        sizes.crash_initial_ms,
        sizes.crash_factor,
        sizes.crash_max_ms,
    );
    let crash_pids = crash_pids_vec(sizes.crash_proc_count);

    let src = format!(
        r#"
        (def seed {SOAK_SEED})
        (def num-tasks {soak_tasks})
        (def num-chans {SOAK_CHANS})
        (def quota {soak_quota})
        (def closeable-start (- num-chans {SOAK_CLOSEABLE}))

        ;; 64 chans, 5 buffer policies round-robin -- identical to L3/B.
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

        ;; ---- L4 W4 addition 1/2: the L3 healthy flow chain, unchanged. ----
        (def relay (flow/map->step {{:describe (fn [] {{:ins {{:in {{}}}} :outs {{:out {{}}}}}})
                                      :transform (fn [s _ m] [s {{:out [m]}}])}}))
        (def flow-sink-ch (chan {flow_sink_buf}))
        (def flow-sink (flow/map->step {{:describe (fn [] {{:ins {{:in {{}}}} :outs {{}}}})
                                          :transform (fn [s _ m] (>!! flow-sink-ch m) [s {{}}])}}))
        (def fl (flow/create-flow
                  {{:procs {{{procs}}}
                    :conns [{conns}]}}))
        (flow/start fl)
        (flow/resume fl)

        (def flow-msgs-done (atom 0))
        (flow/inject fl [:p0 :in] (range {flow_msgs}))
        (go
          (loop [i 0]
            (if (< i {flow_msgs})
              (do (<!! flow-sink-ch) (swap! flow-msgs-done inc) (recur (inc i)))
              nil)))

        ;; ---- L4 W4 addition 2/2: {crash_proc_count} independent, supervised
        ;; crash procs, driven by a tight `flow/pause-proc` loop for the
        ;; whole soak window (this file's module doc: `apply_control`'s
        ;; "pause" arm calls the transition unconditionally, so a bare
        ;; pause-proc loop alone is the repeated-death driver -- no
        ;; resume-proc needed). ----
        (def crash-step
          (flow/map->step
            {{:describe (fn [] {{:ins {{:in {{}}}} :outs {{}}}})
              :init (fn [args] {{:n 0}})
              :transition (fn [s t] (if (= t :clojure.core.async.flow/pause) (test/boom) s))
              :transform (fn [s _ m] [(update s :n inc) {{}}])}}))
        (def crash-pids [{crash_pids}])
        (def crash-fl
          (flow/create-flow
            {{:procs {{{crash_procs}}}
              :conns []}}))
        (def crash-chans (flow/start crash-fl))
        (def crash-report (:report-chan crash-chans))
        (def restart-count (atom 0))
        (def exit-count (atom 0))
        (def crash-drain-done (atom false))
        (go
          (loop []
            (let [ev (<!! crash-report)]
              (if (nil? ev)
                (reset! crash-drain-done true)
                (do
                  (cond
                    (= (:clojure.core.async.flow/op ev) :proc-restart) (swap! restart-count inc)
                    (= (:clojure.core.async.flow/op ev) :proc-exit) (swap! exit-count inc)
                    :else nil)
                  (recur))))))
        ;; One PACED driver loop per crash pid (see this file's `SoakSizes::
        ;; crash_pace_ms` doc for why pacing is load-bearing, not cosmetic,
        ;; and why the pace MUST be `(<!! (timeout n))`, never `sleep-ms`):
        ;; each loop task-parks between one `flow/pause-proc` crash trigger
        ;; and the next, so all {crash_proc_count} procs die and restart
        ;; repeatedly and concurrently for the whole soak window without
        ;; starving the hammer/chain tasks sharing the same shards.
        (def driver-stop? (atom false))
        (def drivers-exited (atom 0))
        (defn crash-driver-loop [pid]
          (go
            (loop []
              (if @driver-stop?
                (swap! drivers-exited inc)
                (do
                  (flow/pause-proc crash-fl pid)
                  (<!! (timeout {crash_pace_ms}))
                  (recur))))))
        (loop [i 0]
          (if (< i (count crash-pids))
            (do (crash-driver-loop (nth crash-pids i)) (recur (inc i)))
            nil))

        (loop [i 0]
          (if (< i num-tasks)
            (do (run-task i) (recur (inc i)))
            nil))
        :spawned
        "#,
        soak_tasks = sizes.soak_tasks,
        soak_quota = sizes.soak_quota,
        flow_sink_buf = sizes.flow_sink_buf,
        flow_msgs = sizes.flow_msgs,
        crash_proc_count = sizes.crash_proc_count,
        crash_pace_ms = sizes.crash_pace_ms,
    );
    eval(&mut e, &src);

    // Hammer liveness backstop -- L3's plateau/hard-cap discipline, polled
    // from this (main, non-shard) OS thread.
    const POLL_EVERY: Duration = Duration::from_millis(50);
    const PLATEAU: Duration = Duration::from_millis(750);
    let hard_deadline = Instant::now() + sizes.hard_cap;
    let mut last_ops = -1_i64;
    let mut last_change = Instant::now();
    let mut closed_early = false;
    loop {
        let tasks_done: i64 = eval(&mut e, "@tasks-done").to_string().parse().expect("tasks-done");
        if tasks_done >= sizes.soak_tasks {
            break;
        }
        let ops_now: i64 = eval(&mut e, "@ops-done").to_string().parse().expect("ops-done");
        if ops_now != last_ops {
            last_ops = ops_now;
            last_change = Instant::now();
        } else if last_change.elapsed() >= PLATEAU {
            closed_early = true;
            println!(
                "G-SOAK[{}]: hammer ops-done plateaued at {ops_now} (tasks-done={tasks_done}/{}) for {PLATEAU:?} \
                 -- invoking the hammer's liveness backstop (close-all-chans!)",
                sizes.tag, sizes.soak_tasks
            );
            eval(&mut e, "(close-all-chans!)");
            break;
        }
        if Instant::now() >= hard_deadline {
            closed_early = true;
            println!(
                "G-SOAK[{}]: hard cap {:?} reached at ops-done={ops_now} tasks-done={tasks_done}/{} \
                 -- invoking the liveness backstop anyway",
                sizes.tag, sizes.hard_cap, sizes.soak_tasks
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
            if tasks_done >= sizes.soak_tasks {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "G-SOAK[{}]: timed out waiting for stragglers to reach tasks-done == num-tasks after the backstop close",
                sizes.tag
            );
            std::thread::sleep(POLL_EVERY);
        }
    }

    let out = eval(
        &mut e,
        r#"(loop [i 0 seen #{}]
             (if (< i num-tasks)
               (recur (inc i) (conj seen (<!! done-ch)))
               (count seen)))"#,
    );
    let hammer_elapsed = t0.elapsed();
    let distinct_finishers: i64 = out.to_string().parse().expect("distinct-finisher count");
    assert_eq!(
        distinct_finishers, sizes.soak_tasks,
        "G-SOAK[{}]: every hammer task must signal done-ch exactly once, with a distinct id",
        sizes.tag
    );
    println!("G-SOAK[{}]: hammer backstop invoked = {closed_early}; hammer wall time = {hammer_elapsed:?}", sizes.tag);

    let ops_done = eval(&mut e, "@ops-done").to_string();
    let expected_ops = (sizes.soak_tasks * sizes.soak_quota).to_string();
    assert_eq!(
        ops_done, expected_ops,
        "G-SOAK[{}]: total hammer op count must equal tasks * quota exactly",
        sizes.tag
    );

    // (b) The healthy chain's own liveness/exactness proof: every injected
    // message reached the sink, exactly once.
    let flow_msgs_done = await_expr_at_least(
        &mut e,
        "the flow chain to deliver every injected message",
        "@flow-msgs-done",
        sizes.flow_msgs,
        sizes.flow_wait_budget,
    );
    assert_eq!(
        flow_msgs_done, sizes.flow_msgs,
        "G-SOAK[{}]: flow chain must deliver every one of the {} injected messages exactly once",
        sizes.tag, sizes.flow_msgs
    );
    let flow_elapsed = t0.elapsed();
    println!(
        "G-SOAK[{}]: flow chain delivered {flow_msgs_done}/{} messages through {} procs; total elapsed = {flow_elapsed:?}",
        sizes.tag, sizes.flow_msgs, sizes.flow_proc_count
    );

    // (c) The supervised crashing flow. First, honor `min_crash_run`: keep
    // it alive and dying for a REAL span of wall-clock time even if the
    // hammer+chain above finished early (this file's module doc / this
    // struct's own `min_crash_run` doc -- "for the whole soak window" needs
    // to mean something even on a fast machine).
    let remaining = sizes.min_crash_run.saturating_sub(t0.elapsed());
    if !remaining.is_zero() {
        println!(
            "G-SOAK[{}]: hammer+chain finished at {:?}, under the {:?} soak-window floor -- keeping the \
             crash flow alive {remaining:?} more",
            sizes.tag,
            t0.elapsed(),
            sizes.min_crash_run
        );
        std::thread::sleep(remaining);
    }
    // Then stop the driver loops first (bounded handshake), THEN stop the
    // crashing flow, THEN read its final counts -- in that order, so no
    // `pause-proc` call can land after `flow/stop` has already torn the
    // flow down.
    eval(&mut e, "(reset! driver-stop? true)");
    await_expr_at_least(
        &mut e,
        "every crash-driver loop to observe the stop flag",
        "@drivers-exited",
        sizes.crash_proc_count as i64,
        sizes.driver_stop_wait,
    );
    eval(&mut e, "(flow/stop crash-fl)");
    eval(&mut e, "(close! crash-report)");
    await_expr_true(&mut e, "the crash-report drain loop to see the close", "@crash-drain-done", Duration::from_secs(10));

    let restart_count: i64 = eval(&mut e, "@restart-count").to_string().parse().expect("restart-count");
    let exit_count: i64 = eval(&mut e, "@exit-count").to_string().parse().expect("exit-count");
    println!(
        "G-SOAK[{}]: crashing flow ({} procs) produced {exit_count} :proc-exit / {restart_count} :proc-restart \
         events over the soak window",
        sizes.tag, sizes.crash_proc_count
    );
    assert!(
        restart_count >= sizes.restart_floor,
        "G-SOAK[{}]: the supervised crashing flow restarted only {restart_count} time(s), under the \
         {}-restart floor for this run -- the supervisor is not actually re-crashing/restarting the \
         procs for the whole soak window",
        sizes.tag, sizes.restart_floor
    );

    // (d) Clean stop, within a deadline -- both flows.
    let stop_deadline = Instant::now() + Duration::from_secs(30);
    eval(&mut e, "(flow/stop fl)");
    assert!(
        Instant::now() < stop_deadline,
        "G-SOAK[{}]: flow/stop on the healthy chain did not return within its 30s deadline",
        sizes.tag
    );

    let total_elapsed = t0.elapsed();
    let total_ops = sizes.soak_tasks * sizes.soak_quota + sizes.flow_msgs;
    let safety_net_after = Doorbell::safety_net_hits();
    println!(
        "G-SOAK[{}]: hammer {} tasks x {} ops + flow {} msgs through {} procs + crash flow {} procs \
         ({restart_count} restarts) = {total_ops} total hammer+flow ops in {total_elapsed:?}; \
         safety_net_hits {safety_net_before} -> {safety_net_after}",
        sizes.tag, sizes.soak_tasks, sizes.soak_quota, sizes.flow_msgs, sizes.flow_proc_count, sizes.crash_proc_count
    );
    // (a) THE gate: this soak exercises ONLY task parks (no-net by design,
    // including every supervisor park -- see this file's module doc). A
    // nonzero delta means an OS-thread doorbell wait fell back to its
    // timeout somewhere in this run, task-side OR supervisor-side.
    assert_eq!(
        safety_net_after, safety_net_before,
        "G-SOAK[{}]: Doorbell safety-net fallback fired {} time(s) during a soak that should exercise ONLY \
         task parks (no-net by design, including the supervisor's own {{sup_chan OR timer}} park) -- report \
         this delta, it means a doorbell wait fell back to its OS-thread timeout somewhere in this run",
        sizes.tag,
        safety_net_after - safety_net_before
    );
}

/// The default-suite variant: NOT `#[ignore]`d, sized to finish comfortably
/// inside 60s (measured ~10s). See this file's module doc for the retracted
/// finding this test's `sleep-ms`-vs-`(<!! (timeout n))` fix closed, and the
/// bigger `#[ignore]`d sibling.
#[test]
fn g_soak_hammer_plus_flow_chain_plus_crashing_flow() {
    run_g_soak(SoakSizes {
        tag: "fast",
        soak_tasks: 2_000,
        soak_quota: 100,
        flow_proc_count: 16,
        flow_msgs: 200_000,
        flow_in_buf: 8,
        flow_sink_buf: 64,
        crash_proc_count: 6,
        crash_max_restarts: 1_000_000,
        crash_window_ms: 600_000,
        crash_initial_ms: 5,
        crash_factor: 1.5,
        crash_max_ms: 50,
        crash_pace_ms: 200,
        min_crash_run: Duration::from_secs(10),
        watchdog_secs: 90,
        hard_cap: Duration::from_secs(40),
        flow_wait_budget: Duration::from_secs(30),
        driver_stop_wait: Duration::from_secs(10),
        restart_floor: 20,
    });
}

/// The bigger, `#[ignore]`d variant -- at or above `l3_soak_test.rs`'s
/// original scale by default, every size knob overridable via env vars so a
/// longer (or shorter) run needs no edit:
/// `L4_SOAK_TASKS`, `L4_SOAK_QUOTA`, `L4_SOAK_FLOW_PROCS`,
/// `L4_SOAK_FLOW_MSGS`, `L4_SOAK_CRASH_PROCS`, `L4_SOAK_CRASH_PACE_MS`,
/// `L4_SOAK_MIN_CRASH_SECS` (the crash flow's own minimum wall-clock soak
/// window), `L4_SOAK_WATCHDOG_SECS`, `L4_SOAK_HARD_CAP_SECS`,
/// `L4_SOAK_FLOW_WAIT_SECS`, `L4_SOAK_RESTART_FLOOR`.
///
/// ```text
/// cargo test --release --test l4_soak_test -- --ignored --nocapture g_soak_hammer_plus_flow_chain_plus_crashing_flow_big
/// ```
#[test]
#[ignore = "slow: run standalone, see module doc"]
fn g_soak_hammer_plus_flow_chain_plus_crashing_flow_big() {
    run_g_soak(SoakSizes {
        tag: "big",
        soak_tasks: env_or("L4_SOAK_TASKS", 10_000),
        soak_quota: env_or("L4_SOAK_QUOTA", 400),
        flow_proc_count: env_or("L4_SOAK_FLOW_PROCS", 50usize),
        flow_msgs: env_or("L4_SOAK_FLOW_MSGS", 2_000_000),
        flow_in_buf: 8,
        flow_sink_buf: 64,
        crash_proc_count: env_or("L4_SOAK_CRASH_PROCS", 8usize),
        crash_max_restarts: 10_000_000,
        crash_window_ms: 3_600_000,
        crash_initial_ms: 5,
        crash_factor: 1.5,
        crash_max_ms: 50,
        crash_pace_ms: env_or("L4_SOAK_CRASH_PACE_MS", 150),
        min_crash_run: Duration::from_secs(env_or("L4_SOAK_MIN_CRASH_SECS", 60)),
        watchdog_secs: env_or("L4_SOAK_WATCHDOG_SECS", 300),
        hard_cap: Duration::from_secs(env_or("L4_SOAK_HARD_CAP_SECS", 120)),
        flow_wait_budget: Duration::from_secs(env_or("L4_SOAK_FLOW_WAIT_SECS", 120)),
        driver_stop_wait: Duration::from_secs(15),
        restart_floor: env_or("L4_SOAK_RESTART_FLOOR", 200),
    });
}

// ============================================================================
// Item 2 of this wave's W4 checklist: restart-churn leak check.
// ============================================================================

/// **Restart-churn leak check** (docs/L4-LANDING-SPEC.md W4 item 2):
/// >=1000 crash -> restart cycles on ONE small supervised flow (tight
/// backoff, generous window), then the runtime's PROCESS-GLOBAL resource
/// counters must be back where they started -- `tasks_spawned -
/// tasks_finished` (currently-outstanding tasks) stable, `stacks_mmaped`
/// flat after a warmup phase, RSS growth bounded (`l4_kill_probe.rs`'s test
/// C's own idiom, duplicated: `rss_kib` above). Also pins the incarnation
/// counter's plumbing end-to-end: the LAST `:proc-restart` event's
/// `:incarnation` must be >= 1000.
///
/// **Driven synchronously, no background `go` loop.** Unlike the two soak
/// tests above, this one needs nothing concurrent: `crash-and-wait-restart`
/// below is a single blocking round trip per cycle (`pause-proc`, then drain
/// `report-chan` until the `:proc-restart` that call caused arrives,
/// returning its incarnation), which is both simpler and gives an EXACT
/// cycle count and an EXACT final incarnation to assert on -- no atom/
/// counting-loop machinery needed.
///
/// **`#[ignore]`d, and not for flakiness** -- exactly `l4_kill_probe.rs`'s
/// test C's own reasoning: its claim is about PROCESS-GLOBAL counters
/// (`tasks_spawned`, `tasks_finished`, `stacks_mmaped`), meaningless (and
/// failing outright) if anything else in the binary is spawning tasks
/// alongside it. The two soak tests above are the only OTHER tests in this
/// file and the default one is not `#[ignore]`d, so this test must run
/// ALONE:
/// ```text
/// cargo test --release --test l4_soak_test restart_churn -- --ignored --test-threads=1 --nocapture
/// ```
#[test]
#[ignore = "process-global resource counters: needs --test-threads=1, see this test's own doc"]
fn restart_churn_resource_counters_return_to_baseline() {
    // The wave's bar is ">=1000 cycles" -- `L4_CHURN_ITERS` may raise it,
    // never lower it below what the checklist asks for.
    let measured: i64 = env_or::<i64>("L4_CHURN_ITERS", 1_000).max(1_000);
    // Warmup covers every shard at least once (round-robin `runtime::spawn`
    // placement) so the BASELINE capture below is already past any one-time
    // per-shard stack mmap, not mid-warmup.
    let warmup: i64 = (runtime::shard_count() as i64 * 8).max(200);

    let _wd = Watchdog::arm(300, "restart-churn leak check");
    let mut e = engine();

    eval(
        &mut e,
        r#"
        (def crash-step
          (flow/map->step
            {:describe (fn [] {:ins {:in {}} :outs {}})
             :init (fn [args] {:n 0})
             :transition (fn [s t] (if (= t :clojure.core.async.flow/pause) (test/boom) s))
             :transform (fn [s _ m] [(update s :n inc) {}])}))
        (def fl
          (flow/create-flow
            {:procs {:p {:proc (flow/process crash-step)
                         :supervision {:policy :restart :max-restarts 10000000 :window-ms 3600000
                                       :backoff {:initial-ms 0 :factor 1.0 :max-ms 1}}}}
             :conns []}))
        (def chans (flow/start fl))
        (def report (:report-chan chans))

        ;; One crash -> restart cycle: send the pause command, drain report-
        ;; chan (skipping the :proc-exit that always precedes it) until the
        ;; :proc-restart it caused arrives, and return that event's
        ;; incarnation. A 5s bound per event is enormous against the
        ;; near-zero backoff configured above.
        (defn crash-and-wait-restart []
          (flow/pause-proc fl :p)
          (loop []
            (let [ev (first (alts!! [report (timeout 5000)]))]
              (cond
                (nil? ev) (throw "l4_soak_test: timed out waiting for a report-chan event during restart churn")
                (= (:clojure.core.async.flow/op ev) :proc-restart) (:clojure.core.async.flow/incarnation ev)
                :else (recur)))))
        :ready
        "#,
    );

    println!("restart-churn: warmup = {warmup} cycles, measured = {measured} cycles");
    eval(&mut e, &format!("(loop [i 0] (if (< i {warmup}) (do (crash-and-wait-restart) (recur (inc i))) :warmed-up))"));

    let stacks_before = runtime::stacks_mmaped();
    let rss_before = rss_kib();
    let spawned_before = runtime::tasks_spawned();
    let finished_before = runtime::tasks_finished();
    let outstanding_before = spawned_before as i64 - finished_before as i64;

    let t0 = Instant::now();
    let final_incarnation_str =
        eval(&mut e, &format!("(loop [i 0 last-inc 0] (if (< i {measured}) (recur (inc i) (crash-and-wait-restart)) last-inc))"))
            .to_string();
    let elapsed = t0.elapsed();
    let final_incarnation: i64 = final_incarnation_str.parse().expect("final incarnation");

    let stacks_after = runtime::stacks_mmaped();
    let rss_after = rss_kib();
    let spawned_after = runtime::tasks_spawned();
    let finished_after = runtime::tasks_finished();
    let outstanding_after = spawned_after as i64 - finished_after as i64;

    let d_stacks = stacks_after.saturating_sub(stacks_before);
    let d_rss = rss_after as i64 - rss_before as i64;
    let d_outstanding = outstanding_after - outstanding_before;

    println!(
        "restart-churn: {measured} cycles in {elapsed:?} ({:.2} ms/cycle); final incarnation = {final_incarnation}; \
         stacks_mmaped {stacks_before} -> {stacks_after} (d{d_stacks}); RSS {rss_before} -> {rss_after} KiB (d{d_rss}); \
         outstanding tasks {outstanding_before} -> {outstanding_after} (d{d_outstanding})",
        elapsed.as_secs_f64() * 1000.0 / measured as f64
    );

    assert!(
        final_incarnation >= 1000,
        "restart-churn: final :proc-restart incarnation {final_incarnation} must be >= 1000 -- the \
         incarnation counter's plumbing end-to-end"
    );
    assert!(
        d_stacks <= runtime::shard_count() as u64,
        "restart-churn: {measured} restart cycles mmap'ed {d_stacks} new stacks after warmup -- force-unwind/ \
         panic-unwind is not returning stacks to the pool"
    );
    assert!(
        d_outstanding.abs() <= 4,
        "restart-churn: outstanding tasks (spawned - finished) moved by {d_outstanding} over {measured} cycles \
         -- a task is leaking (never finishing) or double-counted"
    );
    assert!(d_rss < 64 * 1024, "restart-churn: RSS grew {d_rss} KiB over {measured} restart cycles");

    eval(&mut e, "(flow/stop fl)");
}
