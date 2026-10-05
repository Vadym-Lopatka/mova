//! # L5 / P6b — the two semantics facts the sim kernel owes, as tests
//!
//! P6b's job was to run the existing semantic suites under `MOVA_SIM_SEED`
//! and triage every delta (docs/L5-PROBE-RESULTS.md §P6b). Almost every delta
//! it found belongs to the fence table (design §4) or to the boundary
//! contract (§5) — findings for the landing spec, not code this probe may
//! fix. Exactly one was a kernel defect, and exactly one property is worth
//! pinning as the positive form of G-SEMANTICS. This file is those two.
//!
//! Both run a CHILD PROCESS under a watchdog, for the reason every sim test
//! in this tree does: sim mode is a process-global `OnceLock` read once, so a
//! sim run and a real run cannot coexist in one test binary, and the failure
//! mode under test is a HANG, which a child can be killed out of and an
//! in-process call cannot.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Run `cmd` to completion, killing it at `limit`. Returns
/// `Ok((stdout, stderr, success))` or `Err(())` if the watchdog had to fire.
fn run_watchdogged_io(mut cmd: Command, limit: Duration) -> Result<(String, String, bool), ()> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn");
    let deadline = Instant::now() + limit;
    loop {
        match child.try_wait().expect("try_wait") {
            Some(status) => {
                let out = child.wait_with_output().expect("wait_with_output");
                return Ok((
                    String::from_utf8_lossy(&out.stdout).into_owned(),
                    String::from_utf8_lossy(&out.stderr).into_owned(),
                    status.success(),
                ));
            }
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(());
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

/// [`run_watchdogged_io`] for the callers that only care about stdout.
fn run_watchdogged(cmd: Command, limit: Duration) -> Result<(String, bool), ()> {
    run_watchdogged_io(cmd, limit).map(|(out, _err, ok)| (out, ok))
}

/// **The one class-(a) defect P6b found, as a regression test.**
///
/// In sim there is no timer thread: the shard loop's idle point IS the timer
/// service (design §2, swap 2). So a process that arms a timer without ever
/// spawning a task used to leave the heap ORPHANED — no shard existed, so
/// nothing would ever advance virtual time to the deadline — and the arm hung
/// forever. `(<!! (timeout 50))` as a whole program is the smallest witness;
/// `l3_task_procs_test::the_kill_switch_keeps_the_mult_on_its_own_os_thread`
/// (`MOVA_FLOW_THREAD_PROCS=1`, so the flow spawns zero tasks) was the one
/// that found it, by hanging the suite for 790 s.
///
/// Fixed in `runtime::sim_unpark_shard`: in sim, arming a timer starts the
/// shard if none is running. The bound below is wall time, and it is
/// deliberately loose — the point is "terminates at all", not "terminates
/// fast" (it takes ~20 ms, of which all but a rounding error is `Interp::new`).
#[test]
fn a_timer_armed_before_any_task_exists_still_fires_in_sim() {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mova"));
    cmd.arg("-e").arg("(do (println (<!! (timeout 50))) (println :done))");
    cmd.env("MOVA_SIM_SEED", "0x5EED");
    let (stdout, ok) = run_watchdogged(cmd, Duration::from_secs(20)).expect(
        "a `(timeout ms)` armed with no task in the process HUNG under sim: the timer heap is \
         orphaned again (nothing advances virtual time to its deadline). See \
         `runtime::sim_unpark_shard`.",
    );
    assert!(ok, "the child failed: {stdout}");
    assert!(stdout.contains(":done"), "the program did not run to the end: {stdout}");
}

/// **G-SEMANTICS, in its smallest honest form.**
///
/// A program whose every participant is a TASK must give the same answer in
/// sim as in real mode, for any seed. That is the whole semantic claim: the
/// advance rule reads "no runnable task" as "the world is quiescent", which is
/// sound exactly when nothing outside the task world can still produce work.
///
/// `probes/l5-sim/p6b-task-only.mova` is that program (six chained
/// `(timeout 10)` hops so sim really does walk virtual time, plus a producer
/// that must beat its deadline and a deadline that must beat an empty chan).
/// Its sibling `p6b-osthread-jump.mova` is the same shape with ONE producer
/// moved to an OS thread, and it does NOT agree between modes — that is fence
/// #3 (design §4), and it is a landing item, not a bug this file asserts.
///
/// Run through `sim_probe file`, which is the boundary contract (design §5):
/// whole program inside one root task, driver thread only waiting.
#[test]
fn a_task_only_program_gives_the_same_answer_in_sim_as_in_real_mode() {
    const EXPECTED: &str = "result: [:walked :produced nil]";
    let probe = concat!(env!("CARGO_MANIFEST_DIR"), "/probes/l5-sim/p6b-task-only.mova");

    let mut real = Command::new(env!("CARGO_BIN_EXE_sim_probe"));
    real.args(["file", probe]).env_remove("MOVA_SIM_SEED");
    let (real_out, real_ok) = run_watchdogged(real, Duration::from_secs(30)).expect("real mode hung");
    assert!(real_ok, "real-mode run failed: {real_out}");
    assert!(real_out.contains(EXPECTED), "real mode did not produce {EXPECTED:?}: {real_out}");

    // Several seeds: the schedule changes, the ANSWER must not.
    for seed in ["0", "1", "0x5EED"] {
        let mut sim = Command::new(env!("CARGO_BIN_EXE_sim_probe"));
        sim.args(["file", probe]).env("MOVA_SIM_SEED", seed);
        let (sim_out, sim_ok) = run_watchdogged(sim, Duration::from_secs(30))
            .unwrap_or_else(|()| panic!("sim mode hung at seed {seed}"));
        assert!(sim_ok, "sim run at seed {seed} failed: {sim_out}");
        assert!(
            sim_out.contains(EXPECTED),
            "seed {seed} changed the ANSWER of a task-only program (it may only change the \
             schedule): {sim_out}"
        );
        // The advance rule really did the work: 6*10 + 20 + 30 + the two live
        // 250 ms arms that lose. Zero would mean the program never parked.
        assert!(
            sim_out.contains("virtual ms      : 310.000"),
            "sim did not walk the expected 310 virtual ms at seed {seed}: {sim_out}"
        );
    }
}

// ---------------------------------------------------------------------------
// L5 / W3 — the nondeterminism fence (design §4), as tests
// ---------------------------------------------------------------------------

/// One `mova -e <program>` child under sim, watchdogged. Returns
/// stdout+stderr joined (the refusal tests read the ERROR text, which the
/// renderer writes to stderr) and whether the child exited 0.
fn sim_eval(program: &str, seed: &str, limit: Duration) -> (String, String, bool) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mova"));
    cmd.arg("-e").arg(program).env("MOVA_SIM_SEED", seed);
    run_watchdogged_io(cmd, limit).unwrap_or_else(|()| panic!("child HUNG: {program}"))
}

/// **Fence #1 (design §4): sim refuses `:io` procs — as an ERROR.**
///
/// An `:io` proc runs on its own OS thread by contract, and the sim advance
/// rule cannot see an OS thread: with no runnable task the shard calls the
/// world quiescent and fires every armed deadline instantly while the proc
/// limps along on wall time. P6b's F4 measured the consequence — a
/// `(timeout 5000)` alts collapsed and 12 of 12 messages were dropped. That
/// is a WRONG ANSWER, not merely a nondeterministic one, so `create-flow`
/// refuses before it spawns anything, and the message names the fence and the
/// proc.
#[test]
fn sim_refuses_an_io_proc_at_create_flow() {
    let program = "\
        (defn p ([] {:ins {:in \"i\"} :outs {:out \"o\"}}) ([_] {}) ([s _t] s) ([s _i m] [s {:out [m]}])) \
        (def g (flow/create-flow {:procs {:worker {:proc (flow/process p {:workload :io})}}})) \
        (println :created)";
    let (out, err, ok) = sim_eval(program, "0x5EED", Duration::from_secs(30));
    assert!(!ok, "sim ACCEPTED an :io flow (fence #1 is not holding): {out}{err}");
    assert!(
        err.contains("sim mode refuses :io procs (L5 fence #1)") && err.contains(":worker"),
        "the refusal did not name the fence and the proc: {err}"
    );
    // stdout, not the rendered error (which quotes the source line back).
    assert!(!out.contains(":created"), "create-flow returned instead of erroring: {out}");
}

/// **Fence #1, the kill-switch half:** `MOVA_FLOW_THREAD_PROCS=1` puts EVERY
/// proc on an OS thread, so under sim it is the same wrong-answer machine as
/// a single `:io` proc, flow-wide. Refused at `create-flow` too.
#[test]
fn sim_refuses_the_thread_procs_kill_switch_at_create_flow() {
    let program = "\
        (defn p ([] {:ins {:in \"i\"} :outs {:out \"o\"}}) ([_] {}) ([s _t] s) ([s _i m] [s {:out [m]}])) \
        (def g (flow/create-flow {:procs {:worker {:proc (flow/process p)}}})) \
        (println :created)";
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mova"));
    cmd.arg("-e").arg(program).env("MOVA_SIM_SEED", "0x5EED").env("MOVA_FLOW_THREAD_PROCS", "1");
    let (out, err, ok) = run_watchdogged_io(cmd, Duration::from_secs(30)).expect("child HUNG");
    let both = format!("{out}{err}");
    assert!(!ok, "sim ACCEPTED MOVA_FLOW_THREAD_PROCS=1 (fence #1 is not holding): {both}");
    assert!(
        both.contains("sim mode refuses MOVA_FLOW_THREAD_PROCS=1 (L5 fence #1)"),
        "the refusal did not name the fence: {both}"
    );
}

/// **Fence #4 (design §4): sim refuses `thread*`.**
///
/// `thread`'s whole contract is "give me a preemptive OS thread, because I am
/// about to block in it". Emulating that cooperatively would silently change
/// the semantics the caller reached past `go` to get; leaving it an OS thread
/// would let the advance rule jump virtual time past its work. Neither is
/// honest, so it is an error.
#[test]
fn sim_refuses_thread_star() {
    let (out, err, ok) = sim_eval("(println (<!! (thread :hello)))", "0x5EED", Duration::from_secs(30));
    assert!(!ok, "sim ACCEPTED thread* (fence #4 is not holding): {out}{err}");
    assert!(
        err.contains("sim mode refuses thread* (L5 fence #4)"),
        "the refusal did not name the fence: {err}"
    );
    // stdout, not the rendered error (which quotes the source line back).
    assert!(!out.contains(":hello"), "the thread body ran anyway: {out}");
}

/// **Fence #4, the kill-switch half:** `MOVA_GO_THREADS=1` reverts `go*` to
/// OS threads, which empties the very task world sim schedules. Refused at
/// the first `go*`.
#[test]
fn sim_refuses_the_go_threads_kill_switch() {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mova"));
    cmd.arg("-e")
        .arg("(println (<!! (go :hello)))")
        .env("MOVA_SIM_SEED", "0x5EED")
        .env("MOVA_GO_THREADS", "1");
    let (out, err, ok) = run_watchdogged_io(cmd, Duration::from_secs(30)).expect("child HUNG");
    let both = format!("{out}{err}");
    assert!(!ok, "sim ACCEPTED MOVA_GO_THREADS=1 (fence #4 is not holding): {both}");
    assert!(
        both.contains("sim mode refuses MOVA_GO_THREADS=1 (L5 fence #4)"),
        "the refusal did not name the fence: {both}"
    );
}

/// **Fence #2 (design §4): `sleep-ms` from a task costs VIRTUAL time.**
///
/// `probes/l5-sim/w3-sleep-ms.mova` sleeps 30 s in the root task and then 30 s
/// in a `go` block — 60 000 virtual milliseconds, which real mode would spend
/// a full minute of wall clock on. Under sim, run through the boundary
/// contract, the sleeps are timer-service parks: the assertion is BOTH that
/// the virtual clock walked the full minute AND that the wall clock did not.
///
/// This is also the W4-saga footgun dissolving *in sim*: a `sleep-ms` inside a
/// `go` block poisons its shard in the real world, and here it yields it.
#[test]
fn sleep_ms_in_a_task_costs_virtual_time_not_wall_time_under_sim() {
    let probe = concat!(env!("CARGO_MANIFEST_DIR"), "/probes/l5-sim/w3-sleep-ms.mova");
    let mut sim = Command::new(env!("CARGO_BIN_EXE_sim_probe"));
    sim.args(["file", probe]).env("MOVA_SIM_SEED", "0x5EED");

    let t0 = Instant::now();
    let (out, ok) = run_watchdogged(sim, Duration::from_secs(30)).expect(
        "60 s of `sleep-ms` HUNG under sim: fence #2's task arm did not park on the timer service",
    );
    let wall = t0.elapsed();

    assert!(ok, "the run failed: {out}");
    assert!(out.contains("result: [:root-slept :worker-slept]"), "wrong answer: {out}");
    assert!(
        out.contains("virtual ms      : 60000.000"),
        "sim did not walk the 60 000 virtual ms the two sleeps arm: {out}"
    );
    // Generously loose: the point is "seconds, not a minute". Almost all of
    // it is `Interp::new` evaluating core/*.mova.
    assert!(
        wall < Duration::from_secs(15),
        "60 virtual seconds cost {wall:?} of WALL time -- `sleep-ms` is still thread-sleeping"
    );
}

/// **Fence #2, the other arm: `sleep-ms` from an OS thread also sleeps
/// virtual time (owner-ruled 2026-08-28, OWNER-BRIEF-L5.md RULING 2:
/// "virtualize everywhere, refuse nowhere").**
///
/// `mova -e` evaluates the whole program on the main OS thread (not a
/// task) -- exactly the caller the old refusal named. `sleep-ms` still goes
/// through the shared timer service (`sim_sleep_ms`'s doc): the chan take
/// parks the OS thread on its condvar instead of a task waker, but the
/// arm-bridge (`timer_push` -> `runtime::sim_unpark_shard`) wakes the shard
/// regardless of which family is waiting, so the deadline still fires at an
/// exact virtual instant. Assert BOTH halves: the virtual clock walked the
/// full 60 000 ms (`time-ms` before/after differ by exactly that), and the
/// wall clock did not (the child returns in well under 5 s).
#[test]
fn sleep_ms_from_an_os_thread_sleeps_in_virtual_time_under_sim() {
    let t0 = Instant::now();
    let (out, err, ok) = sim_eval(
        "(def t0 (time-ms)) (sleep-ms 60000) (println (- (time-ms) t0))",
        "0x5EED",
        Duration::from_secs(30),
    );
    let wall = t0.elapsed();

    assert!(ok, "the OS-thread sleep-ms FAILED under sim (fence #2 should virtualize, not refuse): {out}{err}");
    // `mova -e` also echoes the whole program's own trailing value (the
    // `println`'s `nil`) after the printed line, so check the delta as the
    // first line rather than the whole trimmed output.
    assert_eq!(
        out.lines().next(),
        Some("60000"),
        "the OS-thread sleeper did not wake at exactly the virtual deadline: {out}{err}"
    );
    assert!(
        wall < Duration::from_secs(5),
        "60 virtual seconds cost {wall:?} of WALL time -- an OS-thread sleep-ms is still thread-sleeping"
    );
}

/// **Fence #8 (design §4): the seeded user stream.**
///
/// `rand`/`rand-int`/`rand-nth`/`shuffle`, `clojure.math/random`, 0-arg
/// `(java.util.Random.)`, `randomUUID` and `alts!!`'s shuffle all draw their
/// seed from ONE process-global stream derived from `MOVA_SIM_SEED`. The
/// whole claim is two halves and both are asserted here: the same seed
/// reproduces the sequence exactly, and a different seed does not.
///
/// Deliberately a different seed's OUTPUT is compared, not just its first
/// draw: a stream that was seeded but never advanced would pass the first
/// half and fail nothing, which is precisely the bug the pre-W3
/// `sim_user_stream_seed` constant would have been.
#[test]
fn the_rand_family_is_seeded_and_reproducible_under_sim() {
    const PROGRAM: &str = "(println (rand-int 1000000) (rand-int 1000000) (rand-int 1000000) \
                           (nth (shuffle [:a :b :c :d :e :f]) 0) (str (java.util.UUID/randomUUID)))";

    let (a1, e1, ok1) = sim_eval(PROGRAM, "0x5EED", Duration::from_secs(30));
    let (a2, e2, ok2) = sim_eval(PROGRAM, "0x5EED", Duration::from_secs(30));
    let (b, e3, ok3) = sim_eval(PROGRAM, "0xD1FF", Duration::from_secs(30));
    assert!(ok1 && ok2 && ok3, "a child failed:\n{a1}{e1}\n{a2}{e2}\n{b}{e3}");

    assert_eq!(a1, a2, "the SAME seed produced two different random sequences under sim");
    assert_ne!(
        a1, b,
        "two DIFFERENT seeds produced the same random sequence -- the user stream is a constant, \
         not a stream"
    );
}

/// **Fence #4, the door `go*` alone did not close.**
///
/// `MOVA_GO_THREADS=1` diverts THREE natives to the OS-thread placement, not
/// one: `go*`, `put!` and `take!`. Refusing only at `go*` left a program that
/// drives its channels entirely through `put!`/`take!` callbacks free to run
/// under sim with an empty task world — the exact wrong-answer machine fence
/// #4 exists to stop, reached by a different door. This test walks through
/// that door: no `go` block anywhere, just a `put!`.
#[test]
fn sim_refuses_the_go_threads_kill_switch_at_put_bang_too() {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mova"));
    cmd.arg("-e")
        .arg("(def c (chan 1)) (put! c :hello) (println :put-returned)")
        .env("MOVA_SIM_SEED", "0x5EED")
        .env("MOVA_GO_THREADS", "1");
    let (out, err, ok) = run_watchdogged_io(cmd, Duration::from_secs(30)).expect("child HUNG");
    assert!(!ok, "sim ACCEPTED a put! under MOVA_GO_THREADS=1 (fence #4 has a hole): {out}{err}");
    assert!(
        err.contains("sim mode refuses MOVA_GO_THREADS=1 (L5 fence #4)"),
        "the refusal did not name the fence: {err}"
    );
    // stdout, not the rendered error (which quotes the source line back).
    assert!(!out.contains(":put-returned"), "put! returned instead of erroring: {out}");
}
