//! Design-spike tests for `embed/probe-snapshot` Part 2 (drop lifecycle):
//! drives the `drop_probe` bin (each subcommand spawns a background
//! future/go-loop/flow with no natural end, then drops the `Interp` that
//! created it) as a subprocess and asserts the WHOLE PROCESS exits fast --
//! far faster than the 5-second sleep the `future` case is blocked on --
//! which is the empirical proof that `Interp::drop` neither blocks nor
//! keeps the process alive waiting on detached background threads.
//!
//! Cargo builds `drop_probe` (a `src/bin/*.rs` target in this same package)
//! automatically before running integration tests and exposes it via the
//! `CARGO_BIN_EXE_drop_probe` env var -- no manual build step needed here.

use std::process::Command;
use std::time::{Duration, Instant};

/// Comfortably above normal process-spawn/interpreter-bootstrap overhead,
/// comfortably below the 5-second sleep `future`'s background thread is
/// blocked on -- so this ceiling only passes if the process did NOT wait
/// for that thread.
const FAST_EXIT_CEILING: Duration = Duration::from_millis(3000);

fn run_probe(mode: &str) -> (Duration, String) {
    let exe = env!("CARGO_BIN_EXE_drop_probe");
    let start = Instant::now();
    let output = Command::new(exe).arg(mode).output().unwrap_or_else(|e| panic!("failed to launch drop_probe {mode}: {e}"));
    let elapsed = start.elapsed();
    assert!(
        output.status.success(),
        "drop_probe {mode} exited non-zero: {:?}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    (elapsed, String::from_utf8_lossy(&output.stdout).to_string())
}

#[test]
fn baseline_process_exits_fast() {
    let (elapsed, stdout) = run_probe("baseline");
    assert!(stdout.contains("main: returning normally"));
    assert!(elapsed < FAST_EXIT_CEILING, "baseline took {elapsed:?}, expected well under {FAST_EXIT_CEILING:?}");
}

/// (a) `(future (sleep-ms 5000))` created, `Interp` dropped immediately:
/// drop does not block, and the process exits cleanly without waiting the
/// 5s for the sleeping thread -- it is a detached OS thread the process
/// exit kills outright (standard Rust/OS behavior: `main` returning ends
/// the process regardless of other live threads).
#[test]
fn future_sleep_does_not_block_drop_or_process_exit() {
    let (elapsed, stdout) = run_probe("future");
    assert!(stdout.contains("main: returning normally"), "stdout: {stdout}");
    assert!(
        elapsed < FAST_EXIT_CEILING,
        "future probe took {elapsed:?} (>= the 5s sleep would mean the process waited for the thread), stdout: {stdout}"
    );
}

/// (b) a go-loop parked forever on an empty, never-closed channel: same
/// question, same answer -- the parked OS thread does not keep the process
/// alive, and `Interp::drop` does not block on it.
#[test]
fn parked_go_loop_does_not_block_drop_or_process_exit() {
    let (elapsed, stdout) = run_probe("go-loop");
    assert!(stdout.contains("main: returning normally"), "stdout: {stdout}");
    assert!(elapsed < FAST_EXIT_CEILING, "go-loop probe took {elapsed:?}, stdout: {stdout}");
}

/// (c) a `flow/start`ed (and `resume`d) flow that is never `flow/stop`ped:
/// same question. Unlike `flow/stop` itself (which explicitly joins each
/// proc thread with a 5s timeout before detaching -- see
/// `builtins::flow::join_with_timeout`), simply DROPPING the `Interp`
/// never calls `stop` at all, so there is no join/timeout dance to wait
/// out either.
#[test]
fn unstopped_flow_does_not_block_drop_or_process_exit() {
    let (elapsed, stdout) = run_probe("flow");
    assert!(stdout.contains("main: returning normally"), "stdout: {stdout}");
    assert!(elapsed < FAST_EXIT_CEILING, "flow probe took {elapsed:?}, stdout: {stdout}");
}

/// `Interp::drop` itself (not just the surrounding process) is on the order
/// of hundreds of nanoseconds in every case -- printed by `drop_probe`
/// itself via `std::time::Instant` around the literal `drop(interp)` call.
/// This test just pins that the probe reports SOME such measurement, so a
/// future refactor that accidentally makes `Drop` block (e.g. by adding a
/// join) shows up as a change in this printed line, not just as this
/// test's ~3s ceiling silently swallowing a few-hundred-ms regression.
#[test]
fn probe_reports_a_drop_duration_line() {
    for mode in ["future", "go-loop", "flow"] {
        let (_elapsed, stdout) = run_probe(mode);
        assert!(stdout.contains("Interp::drop took"), "{mode} stdout missing drop-duration line: {stdout}");
    }
}
