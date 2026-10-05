//! # P6a — the L5 determinism-kernel probe driver
//!
//! Runs one Mova program (`probes/l5-sim/*.mova`) as the ROOT TASK of the
//! task runtime and reports what the pre-registered rules below need. It is
//! a probe binary, not a product surface: sim mode is reached only through
//! `MOVA_SIM_SEED`, there is no `simulate` native, and nothing here is
//! feature-gated because nothing here is compiled into the library.
//!
//! ```text
//! cargo build --release --bin sim_probe
//! MOVA_SIM_SEED=0x5EED MOVA_SIM_TRACE=/tmp/t1 ./target/release/sim_probe hammer
//! MOVA_SIM_SEED=0x5EED ./target/release/sim_probe days30
//! ./target/release/sim_probe hammer          # real mode (no seed)
//! ```
//!
//! ## PRE-REGISTERED DECISION RULES
//!
//! *Copied verbatim from the P6a brief into this file BEFORE the first run.
//! Every verdict in `docs/` is stated against these exact words. A rule that
//! fails is a SUCCESS if it names the leaking nondeterminism source.*
//!
//! 1. hammer, same seed (0x5EED), 5 fresh-process runs → byte-identical
//!    trace files (`cmp`/`diff`, not hashes).
//! 2. hammer, 8 distinct seeds (0..7) → ≥7 distinct traces.
//! 3. days30: wall time ≤5s; virtual/wall ratio ≥ 10^4 (report both).
//! 4. `timer_threads_spawned() == 0` after a full sim run (assert in driver;
//!    the internal accessor exists:
//!    `mova::internal::async_timer::threads_spawned()`).
//! 5. Any rule failure: identify and NAME the leaking nondeterminism source
//!    (diff the traces, find first divergence, attribute). That is the
//!    probe's product. Do not fix-and-rerun without recording the failure
//!    first.
//!
//! ## The boundary contract (design §5 / §8 R6) — why the shape is this
//!
//! The driver thread does exactly three things: spawn ONE root task, block
//! taking from a done cell, and (after the sim goes quiet) flush the trace.
//! It never evaluates Mova, never spawns a second task, never touches a
//! channel the program touches. Every `go` in the program is therefore
//! spawned FROM INSIDE the root task, on the shard thread, and the two
//! cross-thread edges of the whole run are one `inject_spawn` at the start
//! and one done-cell put at the end — each a single race-free handoff.
//!
//! Building the `Interp` inside the root task (rather than on the driver
//! thread and moving it) is part of the same discipline: `Interp::new()`
//! evaluates `core/*.mova`, and doing that on the driver thread while the
//! shard is already alive would be main-thread evaluation racing the
//! schedule — precisely the leak the contract exists to prevent.
//!
//! ## Why the driver waits for quiescence before flushing
//!
//! Taking the done cell means "the root task's last expression evaluated",
//! not "the shard has finished". The shard still has to emit the root's `X`,
//! and it may still have live timer entries to jump to and fire. Those
//! events are deterministic in CONTENT but arbitrary in wall time, so the
//! driver polls the trace's line counter until it has been unchanged for
//! three consecutive 20 ms samples and only then flushes. Without that, rule
//! 1 would be measuring the driver's exit race, not the scheduler.

use std::time::{Duration, Instant};

use mova::internal::task_chan::{self, BufferPolicy};
use mova::internal::{async_timer, render, Interp};

const HAMMER: &str = include_str!("../../probes/l5-sim/hammer.mova");
const DAYS30: &str = include_str!("../../probes/l5-sim/days30.mova");
// Coverage extensions, added after the first hammer run showed that 100% of
// its 1872 timer fires were `Close` entries — i.e. the advance rule's
// cancelled-`Wake` skip, the rule that makes the compression ratio honest,
// had ZERO coverage from the two programs the brief specified. See each
// file's own header.
const DEREF_HIT: &str = include_str!("../../probes/l5-sim/deref-hit.mova");
const DEREF_MISS: &str = include_str!("../../probes/l5-sim/deref-miss.mova");

fn main() {
    let mut args = std::env::args().skip(1);
    let cmd = args.next().unwrap_or_default();
    // P6b addition: `file <path>` runs an ARBITRARY Mova file under this same
    // driver — i.e. under the boundary contract (design §5), whole program
    // inside ONE root task, driver thread doing nothing but waiting.
    //
    // That is the control experiment P6b needs. `MOVA_SIM_SEED` over an
    // existing test binary runs the program on an ORDINARY OS THREAD, which
    // is out of contract: the sim shard's advance rule reads "no runnable
    // task" as "the world is quiescent" and jumps virtual time to the next
    // deadline — sound only while the sole other thread is blocked on the
    // done cell. Running the SAME program both ways separates "sim broke the
    // semantics" from "the harness shape broke the contract".
    let (name, src): (&str, &'static str) = match cmd.as_str() {
        "hammer" => ("hammer", HAMMER),
        "days30" => ("days30", DAYS30),
        "deref-hit" => ("deref-hit", DEREF_HIT),
        "deref-miss" => ("deref-miss", DEREF_MISS),
        "file" => {
            let path = args.next().unwrap_or_else(|| {
                eprintln!("sim_probe: `file` needs a path");
                std::process::exit(2);
            });
            let owned = std::fs::read_to_string(&path).unwrap_or_else(|e| {
                eprintln!("sim_probe: couldn't read {path:?}: {e}");
                std::process::exit(2);
            });
            // Leaked so it matches the `include_str!` arms' `&'static str`:
            // the root-task closure below is `'static`, and this driver lives
            // exactly as long as the one program it runs.
            ("file", &*Box::leak(owned.into_boxed_str()))
        }
        other => {
            eprintln!(
                "sim_probe: unknown subcommand {other:?} \
                 (want: hammer | days30 | deref-hit | deref-miss | file <path>)"
            );
            std::process::exit(2);
        }
    };

    let sim = mova::internal::sim::enabled();
    let timers_before = async_timer::threads_spawned();

    // `MOVA_SIM_PROBE_WATCHDOG_S=<secs>`: flush the trace and exit(3) after
    // that many seconds. OFF unless set, so the rule-1/2/3 runs never see it.
    //
    // It exists for `deref-miss`, which HANGS on this kernel by design (see
    // that program's header): the driver thread is blocked in the done-cell
    // take and can therefore never flush the trace itself, so without this
    // the refutation's evidence is inferred rather than read. The thread
    // touches nothing in the task world — it reads a clock, flushes a
    // `BufWriter`, and exits the process.
    if let Some(secs) = std::env::var("MOVA_SIM_PROBE_WATCHDOG_S")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
    {
        std::thread::Builder::new()
            .name("sim-probe-watchdog".into())
            .spawn(move || {
                std::thread::sleep(Duration::from_secs(secs));
                eprintln!("sim_probe: WATCHDOG fired after {secs}s — the run did not finish");
                eprintln!(
                    "sim_probe: virtual ns at watchdog = {}, trace events = {}",
                    mova::internal::sim::virtual_ns(),
                    mova::internal::sim::trace_events()
                );
                mova::internal::sim::trace_flush();
                std::process::exit(3);
            })
            .expect("watchdog thread");
    }

    // The done cell: buffered(1), so the root task's final put never depends
    // on whether the driver's blocking take has registered yet. That is the
    // one chan both worlds touch, and this is what keeps the edge race-free.
    let done = task_chan::chan(BufferPolicy::Fixed(1));
    let done_task = done.clone();

    let t0 = Instant::now();
    mova::runtime::spawn(move || {
        let mut interp = Interp::new();
        match interp.eval_str("l5-sim", src) {
            Ok(v) => {
                println!("result: {}", mova::internal::pr_str(&v));
                task_chan::put_int(&done_task, 1);
            }
            Err(e) => {
                eprintln!("{}", render(&e, "l5-sim", src));
                task_chan::put_int(&done_task, 0);
            }
        }
    });

    let ok = task_chan::take_int(&done);
    let wall_to_done = t0.elapsed();

    // Let the shard finish: the root's own `X`, plus any timer entries still
    // armed (a jump-and-fire pass costs microseconds, but it is real work and
    // it belongs in the trace).
    let quiesce_wall = quiesce();
    mova::internal::sim::trace_flush();

    let virtual_ns = mova::internal::sim::virtual_ns();
    let virtual_ms = virtual_ns as f64 / 1e6;
    let wall_ms = wall_to_done.as_secs_f64() * 1e3;
    let timers = async_timer::threads_spawned() - timers_before;

    println!("--- sim_probe {name} ---");
    println!("sim mode        : {sim}");
    println!("shards          : {}", mova::runtime::shard_count());
    println!("root ok         : {ok:?}");
    println!("wall (to done)  : {wall_ms:.3} ms");
    println!("wall (quiesce)  : {:.3} ms", quiesce_wall.as_secs_f64() * 1e3);
    println!("virtual ns      : {virtual_ns}");
    println!("virtual ms      : {virtual_ms:.3}");
    println!("trace events    : {}", mova::internal::sim::trace_events());
    println!("tasks finished  : {}", mova::runtime::tasks_finished());
    println!("tasks resumed   : {}", mova::runtime::tasks_resumed());
    println!("timer arms      : {}", async_timer::arms_total());
    println!("timer threads   : {timers}");
    if wall_ms > 0.0 {
        println!("virtual/wall    : {:.1}", virtual_ms / wall_ms);
    }

    // RULE 4, asserted in-process: a sim run must never start the timer
    // thread. (In real mode the same program starts exactly one, which is
    // what makes this assertion worth having rather than vacuous.)
    if sim {
        assert_eq!(
            timers, 0,
            "RULE 4 FAILED: the timer thread was spawned during a sim run"
        );
    }

    // L5 W1 REGRESSION PROOF: `deref-miss` is the program P6a named as the
    // conc.rs:320 leak's exhibit (that file's own header) -- on the P6a
    // kernel it hangs (MOVA_SIM_PROBE_WATCHDOG_S is the only way it ever
    // reports anything). Post-sweep it must COMPLETE, and its virtual clock
    // must have advanced by EXACTLY `n * 1000ms` (5 timed-out derefs, none
    // delivered): the assert is the sweep's regression proof, not a soft
    // check.
    if sim && name == "deref-miss" && ok == Some(1) {
        const EXPECTED_VIRTUAL_NS: u64 = 5 * 1000 * 1_000_000;
        assert_eq!(
            virtual_ns, EXPECTED_VIRTUAL_NS,
            "L5 W1 REGRESSION FAILED: deref-miss completed but virtual time \
             advanced by {virtual_ns} ns, not the expected {EXPECTED_VIRTUAL_NS} ns \
             (5 * 1000ms timed-out derefs) -- the clock sweep is incomplete or wrong"
        );
        println!(
            "L5 W1 REGRESSION: deref-miss COMPLETED, virtual ns == {virtual_ns} (expected {EXPECTED_VIRTUAL_NS}) -- PASS"
        );
    }

    if ok != Some(1) {
        std::process::exit(1);
    }
}

/// Poll the trace line counter until it has been unchanged for three
/// consecutive 20 ms samples (or 30 s elapse). Returns how long that took.
///
/// `std::thread::sleep` HERE is safe and is not the campaign's `sleep-ms`
/// footgun: this is the driver's own OS thread, which by contract is doing
/// nothing else, and it is not a task — nothing it blocks is a shard.
fn quiesce() -> Duration {
    let t = Instant::now();
    let deadline = t + Duration::from_secs(30);
    let mut last = mova::internal::sim::trace_events();
    let mut stable = 0;
    loop {
        std::thread::sleep(Duration::from_millis(20));
        let now = mova::internal::sim::trace_events();
        if now == last {
            stable += 1;
        } else {
            stable = 0;
            last = now;
        }
        if stable >= 3 || Instant::now() >= deadline {
            return t.elapsed();
        }
    }
}
