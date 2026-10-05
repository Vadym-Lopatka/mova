//! # L5 / W4 — the `simulate` API, as tests
//!
//! *Owner-ruled APPROVED (`docs/OWNER-BRIEF-L5.md` RULING 1); the shapes
//! under test are `docs/L5-FLAGSHIP-DEMOS.md`'s, and the semantics are
//! `docs/L5-VIRTUAL-TIME-DESIGN.md` §5/§6 plus P6a deviation #4 (quiescent
//! return).*
//!
//! ## Why these run IN PROCESS
//!
//! Every other sim test in this tree spawns a child, because `MOVA_SIM_SEED`
//! is a whole-process switch and a sim run cannot coexist with a real run in
//! one binary. `simulate` is the surface that FIXES that: it turns sim on
//! programmatically at its first call, provided that call is the process's
//! first runtime use — so an ordinary `cargo test` binary can simulate, which
//! is precisely the claim worth pinning. Running these as children would test
//! the CLI instead of the API.
//!
//! Two consequences the file lives with:
//!
//! - **[`SIM`] serializes every test here.** Sim is process-scoped by design
//!   (§2, "Ownership"); one world, one root, one seed at a time. The lock is
//!   what makes `cargo test`'s parallel harness legal, not a workaround for a
//!   race in the runtime.
//! - **Nothing in this file may touch the runtime before the first
//!   `simulate`** — no `go`, no `timeout`, no `runtime::shard_count()` — or
//!   the enablement precondition fails for every test that follows. The one
//!   test that deliberately violates it (`..._after_a_real_mode_start_is_
//!   refused`) is therefore the only child-process test here.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use mova::internal::{pr_str, render, Interp};

/// See the module doc: one simulated world at a time, process-wide.
static SIM: Mutex<()> = Mutex::new(());

/// Evaluate a whole Mova program in this process and return its value,
/// printed. Panics with the rendered mova error if it throws — an
/// unexpected throw in these tests is a failure, and the rendered form is
/// what makes it diagnosable.
fn ev(src: &str) -> String {
    let _g = SIM.lock().unwrap_or_else(|e| e.into_inner());
    let mut interp = Interp::new();
    match interp.eval_str("l5-sim-api", src) {
        Ok(v) => pr_str(&v),
        Err(e) => panic!("{}", render(&e, "l5-sim-api", src)),
    }
}

/// [`ev`] with a wall-clock assertion around it: these programs simulate
/// minutes-to-days of virtual time, and every one of them must finish in
/// well under a second of REAL time. A blown bound is the hang this wave
/// exists to abolish, reported as a failure instead of a stuck suite.
fn ev_fast(src: &str, limit: Duration) -> String {
    let t = Instant::now();
    let out = ev(src);
    let took = t.elapsed();
    assert!(
        took < limit,
        "the program took {took:?} of WALL time (bound {limit:?}) — virtual time is supposed to \
         be free. Result was {out}"
    );
    out
}

// ---------------------------------------------------------------------------
// The result map
// ---------------------------------------------------------------------------

/// The shape of one call, and `:virtual-ms` EXACT for a known timeout chain.
///
/// 250 + 1000 + 4000 = 5250 virtual ms, three timer fires, and — the part
/// that only virtual time can promise — it costs no wall time at all.
#[test]
fn a_simulate_call_reports_the_whole_result_map() {
    let out = ev_fast(
        r#"(simulate {:seed 0x5EED}
             (fn [] (do (<!! (timeout 250))
                        (<!! (timeout 1000))
                        (<!! (timeout 4000))
                        :chain-done)))"#,
        Duration::from_secs(5),
    );
    assert!(out.contains(":result :chain-done"), "{out}");
    assert!(
        out.contains(":virtual-ms 5250"),
        "the timeout chain must cost EXACTLY 250+1000+4000 virtual ms: {out}"
    );
    assert!(out.contains(":timer-fires 3"), "{out}");
    assert!(out.contains(":leaked-tasks 0"), "{out}");
    assert!(out.contains(":resumes "), "{out}");
}

// ---------------------------------------------------------------------------
// THE SWEEP — the headline idiom (OWNER-BRIEF-L5 RULING 1, refinement (a))
// ---------------------------------------------------------------------------

/// **`(doseq [seed (range n)] (simulate {:seed seed} f))` in ONE process.**
///
/// The demo idiom, tested the way the owner ruling asks for it: ten seeds,
/// the whole sweep run TWICE in the same process, and
///
/// 1. sweep 1 and sweep 2 must be EQUAL element for element — a call's answer
///    is a function of its own seed, not of how many calls preceded it (which
///    is what the per-call stream re-seed in `clock::sim_call_begin` buys,
///    and what a shared, never-reset stream would destroy);
/// 2. the seeds must not all agree with each other — otherwise the sweep is
///    ten copies of one schedule and explores nothing.
///
/// The scenario is chosen so the SCHEDULE is what decides the answer: five
/// tasks wake at the same virtual instant (one `(timeout 10)` each) and race
/// to put on one channel, so the arrival order is exactly the seeded pool
/// pick of `runtime::sim_next_job`.
#[test]
fn the_seed_sweep_is_reproducible_and_seed_sensitive_in_one_process() {
    let out = ev_fast(
        r#"(do
             (defn scenario []
               (let [c (chan 16) out (atom [])]
                 (dotimes [i 5] (go (<! (timeout 10)) (>! c i)))
                 (dotimes [_ 5] (swap! out conj (first (alts!! [c (timeout 1000)]))))
                 @out))
             (defn sweep []
               (mapv (fn [s] [s (:result (simulate {:seed s} scenario))
                                (:virtual-ms (simulate {:seed s} scenario))])
                     (range 10)))
             (let [a (sweep) b (sweep)]
               {:identical (= a b)
                :distinct-schedules (count (distinct (map second a)))
                :first (second (first a))}))"#,
        Duration::from_secs(10),
    );
    assert!(
        out.contains(":identical true"),
        "the same seed gave a different answer on the second sweep — a call's schedule is \
         leaking across calls: {out}"
    );
    let n: i64 = out
        .split(":distinct-schedules ")
        .nth(1)
        .and_then(|s| s.split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|s| s.parse().ok())
        .expect("the map prints :distinct-schedules <int>");
    assert!(
        n >= 7,
        "10 seeds produced only {n} distinct schedules — the seeded pool pick is not doing its \
         job (design §3 rule 2 asks for >= 7 of 8): {out}"
    );
}

// ---------------------------------------------------------------------------
// :epoch-ms
// ---------------------------------------------------------------------------

/// `:epoch-ms` decides what day it is, per call, and virtual time moves
/// forward from there. 1 000 000 000 000 ms is 2001-09-09T01:46:40Z.
#[test]
fn epoch_ms_is_honored_per_call_and_advances_with_virtual_time() {
    let out = ev_fast(
        r#"(let [a (simulate {:seed 1 :epoch-ms 1000000000000} (fn [] (time-ms)))
                 b (simulate {:seed 1 :epoch-ms 1000000000000}
                     (fn [] (do (<!! (timeout 250)) (time-ms))))
                 c (simulate {:seed 1 :epoch-ms 55} (fn [] (time-ms)))]
             [(:result a) (:result b) (:result c)])"#,
        Duration::from_secs(5),
    );
    assert_eq!(
        out, "[1000000000000 1000000000250 55]",
        "each call must start at the epoch it asked for (not at the previous call's), and a \
         250 ms virtual wait must move the epoch clock 250 ms: {out}"
    );
}

// ---------------------------------------------------------------------------
// The deadlock tripwire (design §6)
// ---------------------------------------------------------------------------

/// **A program that can never finish is NAMED, in milliseconds, instead of
/// hanging.** The `ev_fast` bound is the whole assertion's teeth: real mode
/// blocks here forever.
#[test]
fn a_deadlocked_program_is_reported_not_hung() {
    let out = ev_fast(
        r#"(try (simulate {:seed 1} (fn [] (<!! (chan)))) (catch Exception e (ex-message e)))"#,
        Duration::from_secs(10),
    );
    assert!(out.contains("SIM-E-DEADLOCK"), "{out}");
    assert!(
        out.contains("1 live task"),
        "the report must census the stuck tasks: {out}"
    );
    // Design §6 + the P6b F3 amendment: BOTH mechanisms, always.
    assert!(out.contains("parked forever"), "{out}");
    assert!(out.contains("out-of-contract OS thread"), "{out}");
}

/// A deadlock leaves the world clean: the tripwire tears down before it
/// reports, so the very next call in the same process behaves normally.
#[test]
fn the_world_survives_a_deadlock_report() {
    let out = ev_fast(
        r#"(do (try (simulate {:seed 1} (fn [] (<!! (chan)))) (catch Exception e :named))
               (:result (simulate {:seed 2} (fn [] (do (<!! (timeout 100)) :fine)))))"#,
        Duration::from_secs(10),
    );
    assert_eq!(out, ":fine", "{out}");
}

// ---------------------------------------------------------------------------
// Leaked-task teardown
// ---------------------------------------------------------------------------

/// A helper parked forever, a root that finishes anyway: the helper is
/// reported as `:leaked-tasks 1` and DESTROYED, so the next call starts from
/// an empty slab — which is why the second call reports 1 and not 2.
#[test]
fn a_leaked_task_is_counted_and_torn_down() {
    let out = ev_fast(
        r#"(let [f (fn [] (do (go (<! (chan))) :root-done))]
             [(:leaked-tasks (simulate {:seed 1} f))
              (:leaked-tasks (simulate {:seed 1} f))
              (:result (simulate {:seed 1} f))])"#,
        Duration::from_secs(5),
    );
    assert_eq!(
        out, "[1 1 :root-done]",
        "each call must leak exactly ONE task and tear it down; a growing count means the \
         teardown is not running: {out}"
    );
}

/// The determinism claim the teardown exists to protect: a scenario that
/// leaks helpers still gives the SAME answer per seed on a second pass
/// through the same process.
#[test]
fn determinism_survives_leaked_task_teardown() {
    let out = ev_fast(
        r#"(do
             (defn leaky []
               (let [c (chan 8)]
                 (dotimes [i 4] (go (<! (timeout 5)) (>! c i)))
                 (go (<! (chan)))
                 (go (<! (chan)))
                 (mapv (fn [_] (first (alts!! [c (timeout 500)]))) (range 4))))
             (defn sweep [] (mapv (fn [s] [(:result (simulate {:seed s} leaky))
                                           (:leaked-tasks (simulate {:seed s} leaky))])
                                  (range 6)))
             (let [a (sweep) b (sweep)] {:identical (= a b) :a a}))"#,
        Duration::from_secs(10),
    );
    assert!(out.contains(":identical true"), "{out}");
    assert!(
        out.contains("2]"),
        "each run leaks the two parked helpers: {out}"
    );
}

// ---------------------------------------------------------------------------
// :max-resumes
// ---------------------------------------------------------------------------

/// Two tasks handing a token back and forth forever never park the world and
/// never advance virtual time: without a budget this is a live hang. With
/// one it is a named diagnosis, and the world is still usable afterwards.
#[test]
fn max_resumes_names_a_program_that_never_settles() {
    let out = ev_fast(
        r#"(do
             (defn ping-pong []
               (let [a (chan) b (chan)]
                 (go-loop [] (>! a 1) (<! b) (recur))
                 (go-loop [] (<! a) (>! b 1) (recur))
                 (<!! (chan))))
             [(try (simulate {:seed 1 :max-resumes 2000} ping-pong)
                   (catch Exception e (ex-message e)))
              (:result (simulate {:seed 2} (fn [] :still-here)))])"#,
        Duration::from_secs(20),
    );
    assert!(out.contains("SIM-E-MAX-RESUMES"), "{out}");
    assert!(out.contains("2000 task resumes"), "{out}");
    assert!(
        out.contains(":still-here"),
        "the world must survive the budget tripwire: {out}"
    );
}

// ---------------------------------------------------------------------------
// Refusals
// ---------------------------------------------------------------------------

/// `simulate` blocks its calling thread until the world is quiescent, and in
/// sim there is exactly one shard — so a task calling it would wedge the very
/// shard the simulation runs on. Refused, by name.
#[test]
fn simulate_from_inside_a_task_is_refused() {
    let out = ev_fast(
        r#"(:result (simulate {:seed 1}
             (fn [] (<!! (go (try (simulate {:seed 2} (fn [] 1))
                                  (catch Exception e (ex-message e))))))))"#,
        Duration::from_secs(5),
    );
    assert!(out.contains("SIM-E-IN-TASK"), "{out}");
}

/// `:seed` is not optional. A simulation without a seed is not reproducible,
/// and reproducibility is the entire product.
#[test]
fn the_seed_option_is_required() {
    let out = ev_fast(
        r#"(try (simulate {} (fn [] 1)) (catch Exception e (ex-message e)))"#,
        Duration::from_secs(5),
    );
    assert!(out.contains("SIM-E-OPTS"), "{out}");
    assert!(out.contains(":seed is REQUIRED"), "{out}");
}

/// Bad option types are named the same way, not coerced.
#[test]
fn malformed_options_are_named() {
    for (prog, want) in [
        (r#"(simulate [] (fn [] 1))"#, "options map"),
        (r#"(simulate {:seed "x"} (fn [] 1))"#, ":seed must be an integer"),
        (r#"(simulate {:seed 1 :max-resumes -3} (fn [] 1))"#, "max-resumes must be a non-negative"),
        (r#"(simulate {:seed 1 :trace 7} (fn [] 1))"#, ":trace must be a file path"),
    ] {
        let out = ev_fast(
            &format!("(try {prog} (catch Exception e (ex-message e)))"),
            Duration::from_secs(5),
        );
        assert!(out.contains("SIM-E-OPTS"), "{prog} -> {out}");
        assert!(out.contains(want), "{prog} -> {out}");
    }
}

/// **The one constraint that cannot be tested in process** (design §2,
/// "Ownership"): sim forces one shard, kills the direct-switch slot and
/// replaces the timer thread, all decided at the runtime's first use — so a
/// process that has already started the runtime in real mode can never
/// simulate. A child, because proving it POISONS the process.
#[test]
fn simulate_after_a_real_mode_runtime_start_is_refused() {
    let out = child_stdout(
        r#"(do (<!! (go 1))
               (println (try (simulate {:seed 1} (fn [] 1))
                             (catch Exception e (ex-message e)))))"#,
        Duration::from_secs(30),
    );
    assert!(out.contains("SIM-E-RUNTIME-STARTED"), "{out}");
}

// ---------------------------------------------------------------------------
// Rethrow
// ---------------------------------------------------------------------------

/// A thunk that throws rethrows THROUGH `simulate` — after the drain and the
/// teardown, so a caller that catches it can simulate again immediately.
#[test]
fn a_throwing_thunk_rethrows_after_cleanup() {
    let out = ev_fast(
        r#"[(try (simulate {:seed 1} (fn [] (do (go (<! (chan)))
                                                (<!! (timeout 10))
                                                (throw (ex-info "boom" {:k 1})))))
              (catch Exception e (ex-message e)))
           (:result (simulate {:seed 2} (fn [] :world-still-works)))]"#,
        Duration::from_secs(5),
    );
    assert_eq!(out, r#"["boom" :world-still-works]"#, "{out}");
}

// ---------------------------------------------------------------------------
// :trace
// ---------------------------------------------------------------------------

/// `:trace` opens a file for ONE call and closes it after — the events in it
/// are that call's and nothing else's, and the file is complete on disk by
/// the time `simulate` returns (no flush-at-exit dance).
#[test]
fn the_trace_option_writes_one_calls_events_and_closes_the_file() {
    let path = std::env::temp_dir().join("mova-l5-w4-trace.txt");
    let _ = std::fs::remove_file(&path);
    let p = path.to_string_lossy().replace('\\', "\\\\");
    let out = ev_fast(
        &format!(
            r#"(do (simulate {{:seed 1 :trace "{p}"}} (fn [] (<!! (timeout 100))))
                   (simulate {{:seed 1}} (fn [] (<!! (timeout 999999)))))"#
        ),
        Duration::from_secs(5),
    );
    assert!(out.contains(":virtual-ms 999999"), "{out}");
    let trace = std::fs::read_to_string(&path).expect("the :trace file must exist");
    // The whole run, and ONLY this run: spawn the root, resume it, park it on
    // the timeout, jump the clock, fire the timeout's Close, resume, done.
    // (Ids and the absolute clock value are process-global — virtual time is
    // never reset between calls — so the assertion is on the SHAPE.)
    let kinds: Vec<&str> = trace
        .lines()
        .map(|l| l.split(' ').next().unwrap_or(""))
        .collect();
    assert_eq!(
        kinds,
        vec!["S", "R", "P", "C", "F", "R", "X"],
        "a traced call must contain exactly its own events, starting with the root spawn \
         (design §8 R6) and ending with the root's exit — the second, UNTRACED call's 999999 ms \
         jump must be nowhere in this file: {trace:?}"
    );
    let root_id = trace.lines().next().unwrap().split(' ').nth(1).unwrap();
    assert!(
        trace.lines().filter(|l| l.ends_with(root_id)).count() >= 4,
        "every task line in a one-task run names the same root: {trace:?}"
    );
    let _ = std::fs::remove_file(&path);
}

// ---------------------------------------------------------------------------
// The Ep.1 demo shape, end to end
// ---------------------------------------------------------------------------

/// **`probes/l5-sim/w4-mini-fleet.mova` — the W5 demo's SHAPE, pre-flighted.**
///
/// 200 clients retrying with exponential backoff against a server that is
/// down for the first five virtual seconds, eight independently seeded runs
/// of it in one process. Every client must recover; the recovery must take
/// real virtual seconds; the whole sweep must take a fraction of a wall
/// second; and the two parked-forever server loops must be torn down (one per
/// run) rather than accumulating.
#[test]
fn the_mini_fleet_demo_shape_runs_end_to_end() {
    let src = include_str!("../probes/l5-sim/w4-mini-fleet.mova");
    let t = Instant::now();
    let out = ev(src);
    let wall = t.elapsed();
    assert!(
        wall < Duration::from_secs(10),
        "eight 200-client fleet simulations took {wall:?} of wall time"
    );
    // [seed recovered total-attempts recovered-by-ms virtual-ms leaked-tasks]
    let rows: Vec<Vec<i64>> = out
        .trim_matches(|c| c == '[' || c == ']')
        .split("] [")
        .map(|row| {
            row.split_whitespace()
                .map(|n| n.parse().expect("integers"))
                .collect()
        })
        .collect();
    assert_eq!(rows.len(), 8, "{out}");
    for r in &rows {
        assert_eq!(r[1], 200, "every client must recover: {r:?}");
        assert!(
            r[2] > 200,
            "the outage must have forced retries (total attempts {}): {r:?}",
            r[2]
        );
        assert!(
            r[3] >= 5000,
            "nobody can recover before the 5 s outage ends: {r:?}"
        );
        assert_eq!(r[3], r[4], ":recovered-by-ms is read off the same virtual clock as :virtual-ms: {r:?}");
        assert_eq!(r[5], 1, "the server loop is the one leaked task per run: {r:?}");
    }
    let attempts: Vec<i64> = rows.iter().map(|r| r[2]).collect();
    assert!(
        attempts.iter().collect::<std::collections::HashSet<_>>().len() > 1,
        "eight seeds gave the identical retry count — the schedule is not varying: {attempts:?}"
    );
}

// ---------------------------------------------------------------------------
// L5/W5 kernel fix — the sweep-vs-lifted-seed equivalence
// ---------------------------------------------------------------------------

/// A scenario that draws from all three thread-local PRNG consumers the
/// kernel fix touches: `(rand-int)` in the root task, `(rand-int)` in a `go`
/// task, and an `alts!!` (which shuffles its op order off
/// `builtins::async::next_rand`). Shared by the two programs below so both
/// sides of the comparison run the identical scenario.
const RAND_MIX_SCENARIO: &str = r#"
    (defn scenario []
      (let [c (chan 1)]
        (go (>! c (rand-int 1000)))
        [(rand-int 1000)
         (rand-int 1000)
         (first (alts!! [c (timeout 1000)]))]))
"#;

/// **The finding, pinned as a regression test.** `probes/l5-sim/
/// ep1-rand-carryover-repro.mova` demonstrated that `sim_call_begin`
/// re-seeds the SCHEDULE/USER `AtomicU64` streams per `simulate` call, but
/// each PRNG consumer's thread-local (random.rs/math.rs/async.rs) was seeded
/// lazily ONCE and never revisited — so a seed pulled out of a multi-seed
/// sweep and re-run alone, in a fresh process, gave a DIFFERENT answer than
/// it gave inside the sweep. The `clock::SIM_CALL_GEN` generation counter
/// fixes that: this test is the equivalence the fix exists to restore.
///
/// Two claims, both required:
///
/// 1. The whole sweep, run twice in one process, is element-for-element
///    identical (a call's answer is a function of its own seed alone).
/// 2. Seed 3, LIFTED out of the sweep and run alone in a FRESH CHILD
///    PROCESS, gives the exact same answer as the sweep's seed-3 entry — the
///    "the bug report is a number" property the finding broke.
#[test]
fn seed_lifted_out_of_a_rand_using_sweep_matches_a_fresh_process_alone() {
    let sweep_src = format!(
        r#"{RAND_MIX_SCENARIO}
           (defn sweep [] (mapv (fn [s] (:result (simulate {{:seed s}} scenario))) (range 1 6)))
           (let [a (sweep) b (sweep)] [(= a b) (nth a 2)])"#
    );
    let out = ev_fast(&sweep_src, Duration::from_secs(10));
    assert!(
        out.starts_with("[true "),
        "the sweep run twice in one process must be element-identical (root-rand / task-rand / \
         alts-shuffle must all be a function of the call's own seed, not of how many calls \
         preceded it): {out}"
    );
    let seed3_from_sweep = out
        .strip_prefix("[true ")
        .and_then(|s| s.strip_suffix(']'))
        .expect("the map prints [true <seed-3 result>]")
        .to_string();

    let lifted_src = format!(r#"{RAND_MIX_SCENARIO} (println (:result (simulate {{:seed 3}} scenario)))"#);
    let lifted_out = child_stdout(&lifted_src, Duration::from_secs(10));
    // `mova -e` prints its own eval result too (the `println`'s `nil`), on a
    // second line — the `println` line alone is the value under comparison.
    let seed3_alone = lifted_out.lines().next().unwrap_or("").trim();

    assert_eq!(
        seed3_alone, seed3_from_sweep,
        "seed 3 lifted out of the sweep into a fresh process must match the sweep's own seed-3 \
         entry — sweep gave {seed3_from_sweep:?}, the lifted fresh-process run gave {seed3_alone:?} \
         (full sweep output: {out:?}, full child output: {lifted_out:?})"
    );
}

// ---------------------------------------------------------------------------
// child-process helper
// ---------------------------------------------------------------------------

/// Run one program in a fresh `mova` process under a watchdog, returning its
/// stdout+stderr. Same shape `tests/l5_sim_semantics.rs` uses, and for the
/// same reason: the failure mode under test would otherwise be a hang.
fn child_stdout(program: &str, limit: Duration) -> String {
    use std::process::{Command, Stdio};
    let mut child = Command::new(env!("CARGO_BIN_EXE_mova"))
        .arg("-e")
        .arg(program)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mova");
    let deadline = Instant::now() + limit;
    loop {
        match child.try_wait().expect("try_wait") {
            Some(_) => {
                let out = child.wait_with_output().expect("wait_with_output");
                return format!(
                    "{}{}",
                    String::from_utf8_lossy(&out.stdout),
                    String::from_utf8_lossy(&out.stderr)
                );
            }
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("the child hung for {limit:?}: {program}");
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}
