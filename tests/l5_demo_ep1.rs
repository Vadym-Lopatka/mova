//! # L5 / W5 — FLAGSHIP DEMO, EPISODE 1: "The 30-second outage that never ends"
//!
//! *`docs/L5-FLAGSHIP-DEMOS.md` Episode 1 is the spec; the model is
//! `probes/l5-sim/ep1-retry-storm.mova`; the API under it is W4's
//! `simulate` (`tests/l5_sim_api.rs`). This file is the one that decides
//! whether the Episode 1 rows of the Confirmation Ledger (§6) may turn
//! CONFIRMED. No green bar here → no public claim there.*
//!
//! A **metastable failure** (Bronson et al., HotOS'21; Slack 2021-01-04;
//! AWS DynamoDB 2015-09-20) is a system with two stable states for one set
//! of parameters — healthy, and collapsed — where a transient push moves it
//! from the first to the second and REMOVING THE PUSH DOES NOT MOVE IT
//! BACK. Chaos engineering can trigger one in production and pray. This
//! file makes one a deterministic integer.
//!
//! ## PRE-REGISTERED BARS
//!
//! *Written into this file before the first full-scale run, and quoted
//! verbatim from the W5 brief. Verdicts below are stated against these
//! exact bars. Any bar moved after seeing results is declared, with its
//! rationale, in the DEVIATIONS section.*
//!
//! 0. **CONTROL** *(added by this file, not in the brief — an addition, not
//!    a relaxation)*: the same naive fleet with **no outage at all**, same
//!    seed, same 31.5-virtual-minute window, stays healthy end to end
//!    (zero timeouts, zero rejections, 100% success). Without this row the
//!    "meltdown" could just be an under-provisioned server, which would
//!    prove nothing about metastability.
//! 1. **MELTDOWN**: seed 7, naive-retry — success rate <5% for the entire
//!    final 15 virtual minutes of a ≥30-virtual-minute post-outage window;
//!    `:recovered-at` nil.
//! 2. **HEAL**: jittered-cb-retry across 100 seeds (`doseq`, ONE process —
//!    the sweep idiom) — every seed recovers to ≥90% sustained success
//!    within ≤120,000 virtual ms after outage end.
//! 3. **DETERMINISM**: seed 7 meltdown run, 3 fresh processes → identical
//!    summary (and byte-identical trace if cheap to wire via `:trace`).
//! 4. **CHART DATA**: write the two timelines (naive seed 7, jittered
//!    seed 7) as CSV to `target/l5-demos/ep1-{naive,jittered}.csv` — the
//!    two-curve chart's data. Record row counts.
//! 5. **WALL**: report wall time for the meltdown run and the 100-seed
//!    sweep (no bar, but the ledger takes the measured numbers).
//!
//! ## OPERATIONALIZATIONS (fixed before the first full run)
//!
//! - *success rate*: successes / (successes + timeouts + rejections),
//!   smoothed over a 5-virtual-second window. A window in which nothing
//!   resolved counts as 1.0 — observing no failures is not a failure.
//! - *"for the entire final 15 minutes"* is read in its STRONGEST sense:
//!   the MAXIMUM smoothed rate over every one of the 901 samples in
//!   `[990_000, 1_890_000]` must be <5%, not merely the aggregate.
//! - *recovery is permanent, or it is not recovery*: `:recovered-at` is the
//!   first post-outage sample from which the rate stays ≥90% **through the
//!   end of the observation window**; if the final sample is unhealthy the
//!   run never recovered (`nil` / `-1` in the integer verdict vector).
//! - The bar-2 sweep observes 300,000 virtual ms per seed: 210 s of
//!   post-outage window, i.e. the 120 s the bar allows plus 90 s over which
//!   "sustained" has to hold.
//!
//! ## DEVIATIONS FROM THE BRIEF (declared)
//!
//! - **Bar 3's "byte-identical trace via `:trace`" is not wired.** A traced
//!   meltdown emits one line per scheduler event, and this run has 9.7 M
//!   timer fires and 9.7 M resumes — a ~400 MB file per process, three of
//!   them. The brief scoped it "if cheap to wire"; it is not. Substituted,
//!   and it is a stronger fingerprint anyway: each child prints the verdict
//!   vector AND all 1,890 timeline samples (~13,000 integers, 64,698 bytes)
//!   and the three strings are compared byte for byte.
//! - **Bar 4's CSVs are written by fresh single-`simulate` processes**
//!   (bar 3's children for the naive curve, bar 4's own child for the
//!   jittered one) rather than from the in-process runs, so the published
//!   chart data is canonical under KERNEL FINDING 1. This costs nothing:
//!   bar 3 was already running the naive meltdown in a fresh process.
//! - **Bar 0 (CONTROL) was added**, not present in the brief. It only ever
//!   makes the claim harder.
//! - No bar was moved. Program PARAMETERS were tuned before the first
//!   full-scale run, as the brief allows; they are the ones in
//!   `default-cfg`, and the paper arithmetic that fixes them is the header
//!   comment of the model file.
//!
//! ## KERNEL FINDINGS (reported, NOT fixed — `src/` was frozen for this work)
//!
//! 1. **`(rand)` is not re-seeded per `simulate` call.** The scheduling
//!    stream is (design §3 / `clock::sim_call_begin`), and
//!    `tests/l5_sim_api.rs`'s sweep test pins that. The USER stream is not:
//!    `src/builtins/random.rs::next_u64` keeps a thread-local xorshift
//!    `STATE` that is lazily seeded exactly once (`if x == 0`) from
//!    `clock::user_next_nonzero()` and never reset, so the Nth `simulate`
//!    call in a process continues the chain the previous N-1 calls left
//!    behind. Consequence for the headline sweep idiom: a rand-using
//!    program replays exactly per PROCESS (bar 3 is green, and both CSVs
//!    are byte-reproducible), but seed 42 taken out of a 100-seed `doseq`
//!    and re-run alone gives a DIFFERENT world. That is the difference
//!    between "the bug report is a number" and "the bug report is a number
//!    plus the 41 calls that preceded it". Minimal repro:
//!    `probes/l5-sim/ep1-rand-carryover-repro.mova`.
//!    No bar in this file depends on the broken half — bar 2's assertions
//!    are per-seed properties, and 100 independently-drifting jitter
//!    streams explore MORE worlds, not fewer.
//!
//! ## HOW TO RUN
//!
//! ```text
//! cargo test --release --test l5_demo_ep1 -- --nocapture
//! ```
//!
//! Nothing here is `#[ignore]`d: the whole target is ~60 s of wall clock
//! (see the WALL verdicts) for ~4.5 virtual HOURS of a 5,000-process fleet.
//! The standalone narrative version is
//! `cargo run --release -- probes/l5-sim/ep1-retry-storm.mova`.
//!
//! ## WHY THESE RUN IN PROCESS (and why [`SIM`] exists)
//!
//! Same reason as `tests/l5_sim_api.rs`: `simulate` turns sim on
//! programmatically at its first call provided that call is the process's
//! first runtime use, so an ordinary `cargo test` binary can simulate — and
//! that is exactly the claim worth pinning. Sim is process-scoped (design
//! §2), so [`SIM`] serializes this file's tests; it is what makes cargo's
//! parallel harness legal here, not a workaround for a runtime race. The
//! model file is definitions only, so loading it touches no runtime.

use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use mova::internal::{pr_str, render, Interp};

/// See the module doc: one simulated world at a time, process-wide.
static SIM: Mutex<()> = Mutex::new(());

/// The Mova model, definitions only — everything in the probe file above
/// its `═══DRIVER═══` marker. The text below the marker is the standalone
/// `mova probes/l5-sim/ep1-retry-storm.mova` entry point and would run a
/// full 24-second demo on every include, which is why it is cut here.
fn model() -> &'static str {
    include_str!("../probes/l5-sim/ep1-retry-storm.mova")
        .split(";; ═══DRIVER═══")
        .next()
        .expect("the model file carries its DRIVER marker")
}

/// Evaluate the model plus `driver` in this process and return the printed
/// value of the last form. Panics with the rendered mova error on a throw.
fn run(driver: &str) -> (String, Duration) {
    let _g = SIM.lock().unwrap_or_else(|e| e.into_inner());
    let src = format!("{}\n{driver}\n", model());
    let t = Instant::now();
    let mut interp = Interp::new();
    let out = match interp.eval_str("l5-demo-ep1", &src) {
        Ok(v) => pr_str(&v),
        Err(e) => panic!("{}", render(&e, "l5-demo-ep1", &src)),
    };
    (out, t.elapsed())
}

/// A printed Mova vector of longs, as integers.
fn ints(out: &str) -> Vec<i64> {
    out.trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split_whitespace()
        .map(|n| {
            n.parse()
                .unwrap_or_else(|_| panic!("expected a vector of longs, got {out}"))
        })
        .collect()
}

/// Field names of the model's `verdict` vector, so the assertions below read
/// like the model does. (Keep in sync with `verdict`'s docstring table.)
mod v {
    pub const RECOVERED_AT: usize = 0;
    pub const ATTEMPTS: usize = 1;
    pub const SUCCESSES: usize = 2;
    pub const TIMEOUTS: usize = 3;
    pub const REJECTS: usize = 4;
    pub const OPENS: usize = 5;
    pub const FINAL_ROWS: usize = 6;
    pub const FINAL_SUCC: usize = 7;
    pub const FINAL_RESOLVED: usize = 8;
    pub const FINAL_RATE_PPM: usize = 9;
    pub const FINAL_MAX_RATE_PPM: usize = 10;
    pub const PRE_RATE_PPM: usize = 11;
    pub const OFFERED_PEAK: usize = 12;
    pub const CAPACITY: usize = 13;
    pub const TIMELINE_ROWS: usize = 14;
}

const OUTAGE_TO_MS: i64 = 90_000;
const OBSERVE_MS: i64 = 1_890_000;
const CSV_DIR: &str = "target/l5-demos";

fn csv_dir() -> String {
    std::fs::create_dir_all(CSV_DIR).expect("create target/l5-demos");
    CSV_DIR.to_string()
}

// ---------------------------------------------------------------------------
// BAR 0 — CONTROL
// ---------------------------------------------------------------------------

/// **The server is not under-provisioned.** Identical fleet, identical
/// policy, identical seed, identical 31.5-virtual-minute window — with the
/// 30-second outage deleted. If this row is not perfect, nothing else in
/// this file means what it says: a system that collapses without a trigger
/// is merely overloaded, and overload is not metastability.
#[test]
fn bar_0_the_control_fleet_without_an_outage_never_degrades() {
    let (out, wall) = run(
        r#"(verdict (:result (simulate {:seed 7}
             (fn [] (run-fleet {:policy :naive :outage-from 0 :outage-to 0})))))"#,
    );
    let r = ints(&out);
    println!("BAR 0 CONTROL  wall={wall:?}  verdict={r:?}");
    assert_eq!(r[v::TIMEOUTS], 0, "a healthy fleet times out never: {out}");
    assert_eq!(r[v::REJECTS], 0, "...and is rejected never: {out}");
    assert_eq!(
        r[v::ATTEMPTS],
        r[v::SUCCESSES],
        "every attempt must succeed with no outage: {out}"
    );
    assert_eq!(
        r[v::FINAL_RATE_PPM],
        1_000_000,
        "the final 15 minutes must be 100% healthy: {out}"
    );
    // Physics on paper: lambda0 = 5000 clients / (30 s think + 20 ms service)
    // = 166.6 req/s against C = 6 workers / 20 ms = 300 req/s -> rho = 55.5%.
    assert!(
        r[v::CAPACITY] == 300 && (150..=230).contains(&r[v::OFFERED_PEAK]),
        "the steady state must sit near the paper's 166.6 req/s against 300 req/s of \
         capacity (55.5% utilisation, the brief's <70% bar): {out}"
    );
}

// ---------------------------------------------------------------------------
// BAR 1 — MELTDOWN   (+ BAR 4, the naive half of the chart data)
// ---------------------------------------------------------------------------

/// **The 30-second outage that never ends.** Seed 7, naive retries: fixed
/// 1000 ms delay, no jitter, no breaker — the code everybody has shipped.
/// The server is dead from virtual t=60 s to t=90 s and then returns at
/// FULL capacity; the fleet never comes back.
///
/// (The chart's naive curve is written by BAR 3's children, from a fresh
/// process — see KERNEL FINDING 1.)
#[test]
fn bar_1_naive_retries_turn_a_30_second_outage_into_a_permanent_one() {
    let (out, wall) = run(
        r#"(verdict (:result (simulate {:seed 7} (fn [] (run-fleet {:policy :naive})))))"#,
    );
    let r = ints(&out);
    println!("BAR 1 MELTDOWN  wall={wall:?}  verdict={r:?}");
    println!("BAR 5 WALL      meltdown run = {wall:?}");

    // The pre-outage state must be HEALTHY, or the collapse is not a
    // transition between two states, it is just the only state.
    assert_eq!(
        r[v::PRE_RATE_PPM],
        1_000_000,
        "the first 60 virtual seconds must be 100% healthy: {out}"
    );
    // ...and the post-outage window must be at least 30 virtual minutes.
    assert_eq!(r[v::TIMELINE_ROWS], OBSERVE_MS / 1000, "{out}");
    assert!(
        (OBSERVE_MS - OUTAGE_TO_MS) >= 30 * 60 * 1000,
        "bar 1 asks for a post-outage window of at least 30 virtual minutes"
    );
    assert_eq!(r[v::FINAL_ROWS], 901, "the final 15 minutes, sampled: {out}");

    // THE BAR.
    assert_eq!(
        r[v::RECOVERED_AT],
        -1,
        ":recovered-at must be nil — the fleet never returns to health: {out}"
    );
    assert!(
        r[v::FINAL_MAX_RATE_PPM] < 50_000,
        "BAR 1: the BEST 5-second window in the final 15 virtual minutes was \
         {} ppm success; the bar is <5% (50 000 ppm) for the ENTIRE window: {out}",
        r[v::FINAL_MAX_RATE_PPM]
    );
    assert_eq!(
        r[v::FINAL_SUCC],
        0,
        "not <5% but ZERO: of the {} attempts resolved in the final 15 virtual minutes, this \
         many succeeded: {out}",
        r[v::FINAL_RESOLVED]
    );
    assert!(r[v::FINAL_RESOLVED] > 1_000_000, "the storm is real: {out}");

    // The mechanism, asserted rather than narrated: offered load is pinned
    // an order of magnitude over capacity by retries alone, with the trigger
    // long gone. Paper: lambda1 ~ clients/retry-ms = 5000/1 s = 16.7 x C.
    assert!(
        r[v::OFFERED_PEAK] >= 10 * r[v::CAPACITY],
        "the retry herd must pin offered load far past capacity ({} req/s offered vs {} \
         req/s served): {out}",
        r[v::OFFERED_PEAK],
        r[v::CAPACITY]
    );
    assert_eq!(r[v::OPENS], 0, "the naive policy has no breaker to open: {out}");
}

// ---------------------------------------------------------------------------
// BAR 2 — HEAL
// ---------------------------------------------------------------------------

/// **Does it heal? — asked of 100 universes, in one process, in one
/// `doseq`.** Same fleet, same server, same outage, same seeds' worth of
/// scheduling: the ONLY difference from bar 1 is the client's failure
/// handler — full-jitter exponential backoff plus a circuit breaker that
/// opens for ~30 s after five consecutive failures. That shedding drops
/// offered load to ~166 req/s (0.55 x capacity), the queue drains, the
/// half-open probes land, and the fleet falls back into the healthy state.
///
/// This is the test that a policy review cannot be argued out of: it is not
/// "we added jitter", it is "the fleet recovered in 100 out of 100 worlds,
/// and here is the worst one".
#[test]
fn bar_2_jittered_backoff_and_a_circuit_breaker_heal_across_100_seeds() {
    let (out, wall) = run(
        r#"(let [acc (atom [])]
             (doseq [s (range 100)]
               (let [r (:result (simulate {:seed s}
                         (fn [] (run-fleet {:policy :jittered-cb :observe-ms 300000}))))]
                 (swap! acc conj s)
                 (swap! acc conj (or (:recovered-at r) -1))
                 (swap! acc conj (Math/round (* 1000000.0 (:rate (:pre-outage r)))))
                 (swap! acc conj (:opens (:totals r)))))
             @acc)"#,
    );
    let flat = ints(&out);
    assert_eq!(flat.len(), 400, "100 seeds x 4 fields: {out}");
    let rows: Vec<&[i64]> = flat.chunks(4).collect();

    let mut worst = (-1i64, -1i64);
    for row in &rows {
        let (seed, rec, pre, opens) = (row[0], row[1], row[2], row[3]);
        assert_eq!(pre, 1_000_000, "seed {seed} was not healthy before the outage");
        assert!(opens > 0, "seed {seed} never tripped a breaker — the outage did not bite");
        assert_ne!(rec, -1, "BAR 2: seed {seed} NEVER recovered");
        let after = rec - OUTAGE_TO_MS;
        assert!(
            after <= 120_000,
            "BAR 2: seed {seed} recovered {after} virtual ms after the outage ended; the bar \
             is <= 120 000"
        );
        if after > worst.1 {
            worst = (seed, after);
        }
    }
    let best = rows.iter().map(|r| r[1] - OUTAGE_TO_MS).min().unwrap();
    println!(
        "BAR 2 HEAL      wall={wall:?}  100/100 recovered; worst seed {} at +{} ms, best +{best} ms",
        worst.0, worst.1
    );
    println!("BAR 5 WALL      100-seed sweep = {wall:?}");
    // Seed sensitivity: 100 identical answers would mean the sweep explored
    // one schedule a hundred times.
    let distinct: std::collections::HashSet<i64> = rows.iter().map(|r| r[3]).collect();
    assert!(
        distinct.len() > 10,
        "100 seeds produced only {} distinct breaker-open counts — the seeded schedule is not \
         varying the world",
        distinct.len()
    );
}

// ---------------------------------------------------------------------------
// BAR 3 — DETERMINISM
// ---------------------------------------------------------------------------

/// Run `driver` on top of the model in `n` FRESH `mova` processes at once,
/// returning each one's stdout. Concurrent on purpose: P6a already showed
/// determinism survives machine load, and it turns 3 x 23 s into 23 s. The
/// watchdog is there because the failure mode being excluded is a hang.
fn fresh_children(driver: &str, n: usize, limit: Duration) -> Vec<String> {
    let src = format!("{}\n{driver}\n", model());
    let mut kids: Vec<_> = (0..n)
        .map(|_| {
            Command::new(env!("CARGO_BIN_EXE_mova"))
                .arg("-e")
                .arg(&src)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn mova")
        })
        .collect();
    let deadline = Instant::now() + limit;
    loop {
        let done = kids
            .iter_mut()
            .all(|c| c.try_wait().expect("try_wait").is_some());
        if done {
            break;
        }
        if Instant::now() >= deadline {
            for c in &mut kids {
                let _ = c.kill();
            }
            panic!("a child hung for {limit:?}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    kids.into_iter()
        .map(|c| {
            let o = c.wait_with_output().expect("wait");
            assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
            String::from_utf8_lossy(&o.stdout).into_owned()
        })
        .collect()
}

/// A child prints its integer verdict on line 1 and the whole CSV after it,
/// so the chart data and the determinism fingerprint are literally the same
/// bytes. Split them, write the CSV, and check the deliverable's shape.
fn split_and_write_csv(child_stdout: &str, path: &str) -> (Vec<i64>, usize) {
    let (verdict, csv) = child_stdout
        .split_once('\n')
        .expect("line 1 is the verdict, the rest is the CSV");
    // `mova -e` prints the whole expression's value after the program's own
    // output, and the child's last form is a `println` -> a trailing "nil".
    let csv = csv.trim_start().trim_end();
    let csv = csv.strip_suffix("\nnil").unwrap_or(csv);
    assert!(
        csv.starts_with("t_ms,attempts,successes,timeouts,rejects,breaker_opens,qdepth,success_rate"),
        "the chart's header is part of the deliverable: {path}"
    );
    let rows = csv.trim_end().lines().count() - 1;
    assert_eq!(rows as i64, OBSERVE_MS / 1000, "{path}");
    std::fs::write(path, csv.trim_end().to_string() + "\n").expect("write csv");
    (ints(verdict), rows)
}

/// **Three fresh processes, one universe.** The meltdown is not a
/// statistical tendency that happens to reproduce; it is the same
/// execution. Each child prints the integer verdict AND all 1,890 timeline
/// samples (7 numbers each) — a ~13,000-integer fingerprint of the run —
/// and all three strings must be equal, byte for byte.
///
/// Child 0 also writes BAR 4's naive curve, which is why the chart data is
/// canonical: it comes out of a fresh process whose FIRST and only
/// `simulate` call is this one (see KERNEL FINDING 1 in the module doc for
/// why that distinction is currently load-bearing).
#[test]
fn bar_3_the_meltdown_is_identical_in_three_fresh_processes() {
    let dir = csv_dir();
    let t = Instant::now();
    let outs = fresh_children(
        r#"(let [r (:result (simulate {:seed 7} (fn [] (run-fleet {:policy :naive}))))]
             (println (verdict r))
             (println (timeline-csv (:timeline r))))"#,
        3,
        Duration::from_secs(300),
    );
    let wall = t.elapsed();
    println!(
        "BAR 3 DETERMINISM  wall={wall:?} (3 concurrent fresh processes)  \
         fingerprint={} bytes, {} lines",
        outs[0].len(),
        outs[0].lines().count()
    );
    assert!(
        outs[0].lines().count() > 1_800,
        "the child printed a full timeline: {}",
        &outs[0][..200.min(outs[0].len())]
    );
    assert!(
        outs[0].starts_with("[-1 "),
        "the fingerprint opens with the verdict vector, :recovered-at = -1 first: {}",
        &outs[0][..40.min(outs[0].len())]
    );
    assert_eq!(outs[0], outs[1], "process 1 and 2 disagree");
    assert_eq!(outs[1], outs[2], "process 2 and 3 disagree");

    // BAR 4, the naive curve: the same bytes the three processes agreed on.
    let path = format!("{dir}/ep1-naive.csv");
    let (r, rows) = split_and_write_csv(&outs[0], &path);
    assert_eq!(r[v::RECOVERED_AT], -1, "the canonical curve is the meltdown");
    println!("BAR 4 CHART DATA  {path}  {rows} rows  verdict={r:?}");
}

// ---------------------------------------------------------------------------
// BAR 4 — CHART DATA, the jittered curve
// ---------------------------------------------------------------------------

/// The healing curve, over the SAME 31.5-virtual-minute window as the naive
/// one so the two lines share an x-axis: one chart, two curves, one outage.
/// A fresh process running exactly one `simulate` call, for the same reason
/// as bar 3's child 0.
#[test]
fn bar_4_the_jittered_curve_is_written_over_the_same_window() {
    let dir = csv_dir();
    let t = Instant::now();
    let outs = fresh_children(
        r#"(let [r (:result (simulate {:seed 7} (fn [] (run-fleet {:policy :jittered-cb}))))]
             (println (verdict r))
             (println (timeline-csv (:timeline r))))"#,
        1,
        Duration::from_secs(120),
    );
    let wall = t.elapsed();
    let path = format!("{dir}/ep1-jittered.csv");
    let (r, rows) = split_and_write_csv(&outs[0], &path);
    println!("BAR 4 JITTERED  wall={wall:?}  verdict={r:?}");
    assert_eq!(r[v::TIMELINE_ROWS], OBSERVE_MS / 1000);
    assert_ne!(r[v::RECOVERED_AT], -1, "the jittered fleet must recover");
    assert!(
        r[v::RECOVERED_AT] - OUTAGE_TO_MS <= 120_000,
        "recovered {} ms after the outage ended",
        r[v::RECOVERED_AT] - OUTAGE_TO_MS
    );
    assert_eq!(
        r[v::FINAL_MAX_RATE_PPM],
        1_000_000,
        "and STAYS recovered for the final 15 virtual minutes"
    );
    assert!(r[v::OPENS] > 0, "breakers must actually have tripped");
    println!("BAR 4 CHART DATA  {path}  {rows} rows");
}
