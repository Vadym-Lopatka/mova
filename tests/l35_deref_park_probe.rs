//! L3.5 item 1, MEASUREMENT phase: what does the 1ms timeout-deref tick
//! actually cost? NOT a gate -- every function here is `#[ignore]`d and
//! produces NUMBERS. Nothing here asserts a bar; the bar below was fixed
//! BEFORE the first run and the principal fills it in from a quiet machine
//! (the W5c/L2 precedent: profiles beat cost models).
//!
//! Run one at a time, on as quiet a machine as you can get:
//!
//! ```text
//! cargo test --release --test l35_deref_park_probe -- --ignored --exact p5a_deref_tick_cost --nocapture
//! cargo test --release --test l35_deref_park_probe -- --ignored --exact p5b_deref_tick_interference --nocapture
//! ```
//!
//! The defaults ARE the spec'd sweep -- p5a over N = 0/1/100/1000/10000
//! with a 5 s window, p5b over N = 0/1000 at 300 k rounds -- so neither
//! command needs an argument. Four environment knobs exist only for
//! re-running a single point or a smoke: `P5_NS` (comma-separated N list),
//! `P5_WINDOW_MS` (p5a's window), `P5_ROUNDS` (p5b's ping-pong rounds), and
//! `P5_SHARD` (which shard p5b pins its pair to). They are inherited by the
//! child workers, which is how `P5_ROUNDS`/`P5_SHARD` reach them.
//!
//! ## What is being measured, and why it might be expensive
//!
//! W2b gave `deref` on a future/promise a TASK arm. The no-timeout arm is
//! wake-driven and free while it waits. The WITH-timeout arm
//! (`builtins::conc::future_deref_task_timeout` /
//! `promise_deref_task_timeout`) is a BOUNDED POLL: every
//! `DEREF_POLL_TICK` (= 1ms) it calls `conc::park_tick`, which
//!
//!   1. allocates a FRESH `Arc<Chan>` (`BufferPolicy::Fixed(0)`),
//!   2. calls `builtins::async::timer_arm`, which locks the ONE
//!      process-global timer-heap mutex, pushes an entry, and signals the
//!      ONE timer thread's condvar,
//!   3. parks the task on that chan via `chan_take`,
//!
//! and then the timer thread pops the entry, `chan_close`s the chan, the
//! task wakes, re-checks the cell, and re-arms. So ONE task sitting in
//! `(deref p 30000 :x)` costs ~1000 of those cycles per second, and N
//! waiters cost N x 1000/second through ONE mutex serviced by ONE thread.
//!
//! The L3.5 question is whether that is expensive enough to justify
//! building a combined {cell-waker OR deadline} park -- one park that both
//! wake sources can complete, with no polling at all. That is real work in
//! the runtime, so it gets measured first.
//!
//! ## Decision rule (fixed before measurement)
//!
//! Decision rule (fixed before measurement): if at N=1000 the p5b hop cost
//! degrades <10% vs N=0 AND p5a CPU is <0.10 cores, the combined park is
//! NOT built (L3.5 item 1 closes as measured-cheap, item 6 keeps
//! DEREF_POLL_TICK=1ms with measured backing). If hop degradation ≥10%, or
//! CPU ≥0.5 cores at N=1000, or arms/sec falls below 50% of ideal at
//! N≤1000, the combined park is designed.
//!
//! ## VERDICT (measured 2026-08-28, quiet machine — probe kept as regression net)
//!
//! The rule fired on the CPU trigger, 8x over its bar: pre-fix, N=1000
//! parked `(deref p 120000 :x)` tasks burned **3.99 cores** (N=10000:
//! **11.9 cores**, arms/s collapsed to 15.4% of ideal — the 1ms promise
//! broken 6.5x over). p5b showed NO hop degradation at any N (the
//! N=1000 arm actually ran faster — warm-clock artifact of the burning
//! cores; stable across 3 interleaved re-runs). So the combined park was
//! built (commit "combined {cell-waker OR deadline} park", L3.5 item 1):
//! post-fix, the same sweep reads **0.0000 cores and ZERO in-window timer
//! arms at every N up to 10,000** — each deref arms exactly one entry at
//! registration — and p5b's background-waiter resume count dropped from
//! ~480k tick-wakes to exactly N initial parks. This file's decision rule
//! below is the pre-registered record; the machinery it priced
//! (`park_tick`/`DEREF_POLL_TICK` in conc.rs) no longer exists, and the
//! probes now serve as the "the poll never comes back" regression net.
//!
//! One honest caveat on reading the `%ideal` column, seen while smoking
//! this file and NOT a reason to move the bar (it was fixed first): even at
//! N=1, on an idle-ish machine, arms/s came in around 790 against an ideal
//! of 1000. `ideal_arms_per_s = N * 1000` assumes a tick period of exactly
//! `DEREF_POLL_TICK`, but each lap also pays the `Arc<Chan>` allocation,
//! the heap push, the timer thread's pop/close, and the task's own
//! wake-and-recheck -- so the true period is 1 ms PLUS that, and ~80% of
//! ideal is the metric's own floor, not saturation. The 50% threshold
//! should therefore be read as "half of ideal", i.e. clearly below the N=1
//! baseline this same run prints, which is exactly why N=1 is in the sweep.
//!
//! ## Shape
//!
//! Child processes, exactly like `tests/l3_placement_probe.rs` (whose
//! `run_child` this file replicates): the numbers here are properties of a
//! PROCESS -- `getrusage(RUSAGE_SELF)` is whole-process, the timer heap and
//! its arm counter are process-global, and the task shards are
//! process-global too -- so "N waiters parked" and "no waiters parked"
//! cannot be two phases of one run. Each child prints its own
//! machine-parsable `p5a`/`p5b` line; the parent re-prints them verbatim
//! and adds a small derived table.
//!
//! The waiters are spawned THROUGH THE INTERPRETER (`(go (deref p 120000
//! :x))` on a promise that is never delivered until teardown), not through
//! `runtime::spawn` on a hand-built cell, so the arm under measurement is
//! the real `conc.rs` one that production code reaches. The 120 s timeout
//! is far beyond every window here, so no waiter ever reaches its deadline
//! during a measurement -- every tick observed is a steady-state poll tick.
//!
//! `p5b`'s hop loop is `tests/task_stress_test.rs`'s
//! `e1_same_shard_ping_pong_hop_cost`, copied at its own iteration count
//! (300 k rounds = 600 k hops) and stripped of its gate assertion: this is
//! a probe, and the whole point is to see the number move.
//!
//! ## Why p5b PINS the pair instead of copying E1's coordinator
//!
//! The one deliberate deviation from E1. E1 gets its two halves onto one
//! shard by spawning them from inside a coordinator TASK, leaning on
//! `runtime::pick_shard`'s family-locality heuristic -- which applies only
//! while that shard's live-task count is under `SPAWN_LOCAL_MAX` (= 8).
//! That is true at N=0 and FALSE at N=1000: 1000 waiters round-robined over
//! 14 shards leave ~71 live tasks on every one of them, so the heuristic
//! falls back to round-robin and the pair lands on two DIFFERENT shards.
//! The A/B would then be "same-shard hop vs cross-shard hop" -- a ~10x
//! placement artifact sitting on top of the ~few-% effect being looked for,
//! i.e. a measurement of the wrong thing that would look like a
//! catastrophic result. (Observed while smoking this file: at N=1000 the
//! coordinator version reported `direct=0` -- zero direct switches, the
//! signature of a pair that is no longer co-shard.)
//!
//! So both halves are placed with `runtime::spawn_on(P5_SHARD, ..)`
//! (default shard 0), which does not consult `SPAWN_LOCAL_MAX` and is FINAL
//! -- placement is then identical in both arms and the only thing that
//! differs is the interference. The waiters themselves are spawned from the
//! interpreter's root thread, so they round-robin: roughly `N /
//! shard_count()` of them share the pinned shard, and the rest supply the
//! global-timer-heap and timer-thread pressure that the pinned shard also
//! competes with. Both effects are wanted; `p5a` is where they are
//! separated.
//!
//! **Interleaving is the principal's job.** `p5b` makes N=0 and N=1000
//! invocable from the SAME binary in the SAME pattern (one child per N,
//! same argv shape, same settle); if the two need to be interleaved A/B/A/B
//! against machine drift, that is done by running the probe repeatedly, not
//! by anything this file decides.

use std::time::{Duration, Instant};

use mova::embed::{Engine, Value};
use mova::internal::task_chan::{self, BufferPolicy};
use mova::runtime;

// ===========================================================================
// Child-process plumbing (same shape as tests/l3_placement_probe.rs)
// ===========================================================================

/// Run one `#[ignore]`d worker below in a fresh process with `envs` set, and
/// return its stdout. Panics with the child's full output on failure.
fn run_child(worker: &str, envs: &[(&str, String)]) -> String {
    let exe = std::env::current_exe().expect("current_exe");
    let mut cmd = std::process::Command::new(exe);
    cmd.args([worker, "--exact", "--ignored", "--nocapture"]);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let out = cmd.output().unwrap_or_else(|e| panic!("failed to spawn the P5 probe worker {worker}: {e}"));
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "the P5 probe worker {worker} failed (envs={envs:?}); status={:?}\nstdout={stdout}\nstderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    stdout
}

/// Re-print every `prefix`-tagged line the child produced, and return them,
/// so the parent's own stdout is the complete machine-parsable record.
fn relay(stdout: &str, prefix: &str) -> Vec<String> {
    let lines: Vec<String> = stdout.lines().filter(|l| l.trim_start().starts_with(prefix)).map(|l| l.trim().to_string()).collect();
    assert!(!lines.is_empty(), "the P5 worker printed no {prefix:?} line\nstdout={stdout}");
    for l in &lines {
        println!("{l}");
    }
    lines
}

/// Pull `name=<f64>` out of one of the relayed lines.
fn field_f64(lines: &[String], name: &str) -> f64 {
    for l in lines {
        for tok in l.split_whitespace() {
            if let Some(v) = tok.strip_prefix(name) {
                return v.parse().unwrap_or_else(|e| panic!("{name} was not a number in {l:?}: {e}"));
            }
        }
    }
    panic!("no {name} in {lines:?}");
}

fn env_u64(k: &str, dflt: u64) -> u64 {
    std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(dflt)
}

// ===========================================================================
// Child-side helpers
// ===========================================================================

/// This process's own accumulated CPU, `(user, system)` in seconds, per
/// `getrusage(RUSAGE_SELF)` -- the same call `tests/timer_service_test.rs`
/// and `tests/geo_intern_probe.rs` use, kept split here because the tick's
/// cost is expected to be mostly SYSTEM time (mutex, condvar, thread wake)
/// and collapsing the two would hide that.
fn cpu_s() -> (f64, f64) {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    assert_eq!(rc, 0, "getrusage(RUSAGE_SELF) failed");
    let s = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
    (s(ru.ru_utime), s(ru.ru_stime))
}

fn engine() -> Engine {
    Engine::builder().build()
}

fn eval(e: &mut Engine, src: &str) -> Value {
    e.eval_named("l35-deref-park-probe", src).unwrap_or_else(|err| panic!("eval error: {}", err.render_plain()))
}

/// Park `n` interpreted tasks in the WITH-timeout `deref` arm on a promise
/// nothing will deliver until [`release_waiters`], and wait until they have
/// actually gone to sleep there.
///
/// The timeout is 120 s -- long past any window this file measures -- so
/// every tick observed afterwards is a steady-state poll re-arm, never a
/// waiter running out its deadline.
///
/// "Actually parked" is proved two ways, because either alone is weak:
/// `runtime::tasks_spawned()` reaching `n` says the shards have taken them
/// all, and the timer-arm counter climbing says they have reached
/// `park_tick` rather than still being in `Interp::fork`. Then a fixed
/// settle sleep on top, so the measured window contains no spawn work.
fn park_waiters(e: &mut Engine, n: u64) {
    let spawned_before = runtime::tasks_spawned();
    eval(e, "(def p5-cell (promise))");
    if n > 0 {
        eval(e, &format!("(dotimes [i {n}] (go (deref p5-cell 120000 :never)))"));
    }
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        let spawned = runtime::tasks_spawned() - spawned_before >= n;
        // `n` waiters ticking at 1 ms have collectively armed far more than
        // `n` times within a few hundred ms; `n` is a deliberately slack
        // floor that only needs to prove they REACHED `park_tick`.
        let arming = n == 0 || mova::internal::async_timer::arms_total() >= n;
        if spawned && arming {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    // Settle: let the last spawn's allocation traffic and the shards' own
    // wake-up churn drain out of the window that is about to be measured.
    std::thread::sleep(Duration::from_millis(500));
    println!("p5_setup N={n} tasks_spawned={} arms_at_settle={}", runtime::tasks_spawned() - spawned_before, mova::internal::async_timer::arms_total());
}

/// Deliver the promise so every waiter returns on its own instead of being
/// killed mid-park by process exit, and give them a moment to do it. Each
/// waiter notices within one `DEREF_POLL_TICK`.
fn release_waiters(e: &mut Engine) {
    eval(e, "(deliver p5-cell :released)");
    std::thread::sleep(Duration::from_millis(200));
}

// ===========================================================================
// P5a -- what one parked timeout-deref costs, and what N of them cost
// ===========================================================================

/// **P5a: the standing cost of N parked timeout-derefs.**
///
/// For each N, a fresh child parks N waiters, then measures a fixed window
/// with nothing else happening in the process. Two lines come back per N:
///
/// - `p5a N=<n> cpu_user_s=<..> cpu_sys_s=<..>` -- the child's OWN rusage
///   delta across the window. Divided by the window this is CORES, which is
///   the number the decision rule is written in.
/// - `p5a N=<n> timer_arms=<d> window_s=<..> arms_per_s=<..>
///   ideal_arms_per_s=<n*1000>` -- how many re-arms actually got through
///   the one global heap. A shortfall against ideal is not good news: it
///   means the tick is STRETCHING because the single timer thread (or its
///   mutex) cannot keep up, i.e. the 1ms latency promise `DEREF_POLL_TICK`
///   makes is already not being kept at that N.
///
/// N=0 is the baseline: the same process, the same window, no waiters --
/// whatever CPU that shows is the floor everything else is measured from.
#[test]
#[ignore = "probe: run standalone on a quiet machine"]
fn p5a_deref_tick_cost() {
    let window_ms = env_u64("P5_WINDOW_MS", 5_000);
    let ns: Vec<u64> = match std::env::var("P5_NS") {
        Ok(s) => s.split(',').filter_map(|t| t.trim().parse().ok()).collect(),
        Err(_) => vec![0, 1, 100, 1_000, 10_000],
    };
    println!("\nP5a -- standing cost of N parked `(deref p 120000 :x)` tasks, {window_ms} ms window, fresh process per N\n");
    let mut rows = Vec::new();
    for n in ns {
        let out = run_child("p5a_child", &[("P5_N", n.to_string()), ("P5_WINDOW_MS", window_ms.to_string())]);
        relay(&out, "p5_setup ");
        let lines = relay(&out, "p5a ");
        let user = field_f64(&lines, "cpu_user_s=");
        let sys = field_f64(&lines, "cpu_sys_s=");
        let window_s = field_f64(&lines, "window_s=");
        let arms_per_s = field_f64(&lines, "arms_per_s=");
        let ideal = field_f64(&lines, "ideal_arms_per_s=");
        rows.push((n, (user + sys) / window_s, arms_per_s, ideal));
    }
    println!("\n  {:>7}  {:>8}  {:>12}  {:>12}  {:>8}", "N", "cores", "arms/s", "ideal/s", "%ideal");
    for (n, cores, arms, ideal) in &rows {
        let pct = if *ideal > 0.0 { format!("{:.1}", 100.0 * arms / ideal) } else { "-".to_string() };
        println!("  {n:>7}  {cores:>8.4}  {arms:>12.0}  {ideal:>12.0}  {pct:>8}");
    }
    println!("\n  (decision rule: see this file's module doc -- <0.10 cores at N=1000 and >=50% of ideal is the cheap verdict)\n");
}

/// [`p5a_deref_tick_cost`]'s worker: park `P5_N` waiters, measure
/// `P5_WINDOW_MS` of doing nothing else, print, release, exit.
#[test]
#[ignore = "probe worker: spawned by p5a_deref_tick_cost"]
fn p5a_child() {
    let n = env_u64("P5_N", 0);
    let window_ms = env_u64("P5_WINDOW_MS", 5_000);
    let mut e = engine();
    park_waiters(&mut e, n);

    let (u0, s0) = cpu_s();
    let arms0 = mova::internal::async_timer::arms_total();
    let t0 = Instant::now();
    std::thread::sleep(Duration::from_millis(window_ms));
    let window_s = t0.elapsed().as_secs_f64();
    let arms = mova::internal::async_timer::arms_total() - arms0;
    let (u1, s1) = cpu_s();

    println!("p5a N={n} cpu_user_s={:.4} cpu_sys_s={:.4}", u1 - u0, s1 - s0);
    println!(
        "p5a N={n} timer_arms={arms} window_s={window_s:.3} arms_per_s={:.1} ideal_arms_per_s={}",
        arms as f64 / window_s,
        n * 1_000
    );

    release_waiters(&mut e);
}

// ===========================================================================
// P5b -- the co-shard damage number
// ===========================================================================

/// **P5b: what N parked timeout-derefs do to a task that is trying to work.**
///
/// P5a says what the ticks cost in absolute CPU. P5b says what they cost the
/// tasks sharing their shards, which is the number that actually decides the
/// item: a same-shard ping-pong (`tests/task_stress_test.rs`'s E1, same 300 k
/// rounds) run with N background waiters parked on the same shards.
///
/// Prints `p5b N=<n> ns_per_hop=<..>` per N. N=0 and N=1000 are the same
/// binary invoked the same way, one child each.
#[test]
#[ignore = "probe: run standalone on a quiet machine"]
fn p5b_deref_tick_interference() {
    let ns: Vec<u64> = match std::env::var("P5_NS") {
        Ok(s) => s.split(',').filter_map(|t| t.trim().parse().ok()).collect(),
        Err(_) => vec![0, 1_000],
    };
    println!("\nP5b -- E1 same-shard ping-pong hop cost with N parked timeout-derefs, fresh process per N\n");
    let mut rows = Vec::new();
    for n in ns {
        let out = run_child("p5b_child", &[("P5_N", n.to_string())]);
        relay(&out, "p5_setup ");
        let lines = relay(&out, "p5b ");
        rows.push((n, field_f64(&lines, "ns_per_hop=")));
    }
    let base = rows.first().map(|r| r.1).unwrap_or(0.0);
    println!("\n  {:>7}  {:>10}  {:>10}", "N", "ns/hop", "vs N=0");
    for (n, hop) in &rows {
        let d = if base > 0.0 { format!("{:+.1}%", 100.0 * (hop - base) / base) } else { "-".to_string() };
        println!("  {n:>7}  {hop:>10.1}  {d:>10}");
    }
    println!("\n  (decision rule: see this file's module doc -- <10% degradation at N=1000 is the cheap verdict)\n");
}

/// [`p5b_deref_tick_interference`]'s worker: park `P5_N` waiters, then run
/// the E1 loop verbatim.
#[test]
#[ignore = "probe worker: spawned by p5b_deref_tick_interference"]
fn p5b_child() {
    let n = env_u64("P5_N", 0);
    let rounds = env_u64("P5_ROUNDS", 300_000);
    let shard = env_u64("P5_SHARD", 0) as usize;
    let mut e = engine();
    park_waiters(&mut e, n);

    // --- tests/task_stress_test.rs::e1_same_shard_ping_pong_hop_cost: same
    // --- loop, same 300k rounds, minus the gate assertion (this is a probe),
    // --- and with EXPLICIT placement instead of E1's coordinator task. See
    // --- this file's module doc, "Why p5b pins the pair": at N=1000 the
    // --- coordinator trick silently stops working, and an A/B where one arm
    // --- is same-shard and the other is cross-shard measures nothing.
    let a = task_chan::chan(BufferPolicy::Unbuffered);
    let b = task_chan::chan(BufferPolicy::Unbuffered);
    let done = task_chan::chan(BufferPolicy::Unbuffered);

    let t0 = Instant::now();
    let (a1, b1) = (a.clone(), b.clone());
    runtime::spawn_on(shard, move || {
        for i in 0..rounds as i64 {
            task_chan::put_int(&a1, i);
            task_chan::take_int(&b1);
        }
    });
    let (a2, b2, d2) = (a.clone(), b.clone(), done.clone());
    runtime::spawn_on(shard, move || {
        for i in 0..rounds as i64 {
            task_chan::take_int(&a2);
            task_chan::put_int(&b2, i);
        }
        task_chan::put_int(&d2, 1);
    });
    assert_eq!(task_chan::take_int(&done), Some(1));
    let elapsed = t0.elapsed();
    let hops = rounds * 2;
    let ns_per_hop = elapsed.as_nanos() as f64 / hops as f64;
    println!(
        "p5b N={n} ns_per_hop={ns_per_hop:.1} hops={hops} elapsed_s={:.3} shard={shard} shards={} resumes={} direct={}",
        elapsed.as_secs_f64(),
        runtime::shard_count(),
        runtime::tasks_resumed(),
        runtime::direct_switches()
    );

    release_waiters(&mut e);
}
