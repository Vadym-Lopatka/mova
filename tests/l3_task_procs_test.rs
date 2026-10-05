//! L3 gates: a flow proc runs as a runtime TASK **by default**, with
//! `:workload :io` as the per-proc opt-out and `MOVA_FLOW_THREAD_PROCS=1`
//! as the whole-process kill switch (docs/L3-FLOW-PROCS-DESIGN.md,
//! docs/L3-LANDING-SPEC.md §W1a and §W1b).
//!
//! These are CORRECTNESS tests -- none of them is a perf gate. What they
//! falsify, one claim per block:
//!
//! - **procs really are tasks by default, and messages really flow through
//!   them.** An unfused 3-proc chain is 3 tasks; the same chain under
//!   `MOVA_FUSE_ALL=1` is ONE task (a fused run is task-spawned iff every
//!   member is a task proc), and both deliver every message in order.
//! - **the pre-L3 engine is still reachable.** The identical program under
//!   `MOVA_FLOW_THREAD_PROCS=1` must spawn exactly ZERO tasks and still
//!   deliver every message. That is what makes the pair a DIFFERENTIAL
//!   rather than a tautology about an env var, and it is the same claim
//!   `cargo test --release --test flow_gold_test` gates at corpus scale in
//!   both worlds.
//! - **control still works on a task proc.** pause -> inject -> resume is
//!   lossless, and `stop` returns promptly -- which is the DONE-CELL path,
//!   since a task proc has no `JoinHandle` for `stop` to join. A regression
//!   there does not corrupt anything; it costs `STOP_JOIN_TIMEOUT` (5s) per
//!   proc, which is why the stop leg is timed rather than merely awaited.
//!   The same program is run in the THREAD world too, so both arms of
//!   `stop`'s handle match (join, and done-cell wait) are exercised.
//! - **backpressure is real, and control beats it.** A task proc feeding a
//!   `:buf-or-n 1` consumer blocks on nearly every send and must still
//!   deliver every message in order; a task proc left BLOCKED mid-send must
//!   still answer `flow/ping` and must still `stop` inside the bar. This is
//!   [`blocked_send_task`]'s block -- the doorbell-family park that replaced
//!   W1a's `panic!` -- and the reason W1a's "size every buffer above the
//!   message count" caveat is repealed rather than restated.
//! - **`:workload :io` is a real opt-out.** An `:io` proc mixed into a task
//!   chain keeps its OS thread (2 tasks for 3 procs) and the chain still
//!   flows in both directions across the thread/task boundary. That its
//!   conns also lose their transport lane is asserted directly on the
//!   planner in `src/builtins/flow.rs`'s own unit tests -- a negative
//!   decision no behavioral differential can catch, since falling back to a
//!   general `Chan` is always *correct*.
//! - **timers wake a task proc.** `(<!! (timeout ms))` inside a transform.
//!   The design says this needs no work (`timer_loop` wakes via
//!   `chan_close`, which drains `task_takers` before its `notify_all`, so
//!   the timer thread is agnostic about what kind of waiter it wakes); this
//!   is the test that turns "should be" into "is".
//! - **W2's de-polled stragglers don't wedge, and `stop`'s budget doesn't
//!   multiply.** `flow/ping` from inside a `go` block (`native_ping`'s
//!   `thread::sleep` poll replaced by a task-safe tick, design §3.7) still
//!   returns the right answer, repeatedly, without stalling a shard. And a
//!   wedged fused run's `stop` -- previously `STOP_JOIN_TIMEOUT` per
//!   member, since every non-head member's done-cell wait re-armed its own
//!   fresh deadline -- is now bounded by ONE shared deadline across the
//!   whole done-cell wait loop (`#[ignore]`d: this one's own runtime is the
//!   ~5s it proves `stop` is bounded to).
//!
//! Child processes, not in-process measurement, for the reason
//! `tests/l2_direct_switch_test.rs` gives for its own: the kill switch is an
//! `OnceLock` read once per process, so both worlds cannot exist in one run,
//! and `runtime::tasks_spawned()` is a process-global counter every other
//! test in a binary would move.

use mova::runtime;

/// Runs one `#[ignore]`d child worker below in a fresh process with `envs`
/// set, and returns its stdout. Panics with the child's full output if it
/// did not exit successfully -- the child's own stderr is the diagnostic
/// for anything that killed it outright.
fn run_child(worker: &str, envs: &[(&str, &str)]) -> String {
    let exe = std::env::current_exe().expect("current_exe");
    let mut cmd = std::process::Command::new(exe);
    cmd.args([worker, "--exact", "--ignored", "--nocapture"]);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let out = cmd.output().unwrap_or_else(|e| panic!("failed to spawn the L3 child worker {worker}: {e}"));
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "the L3 child worker {worker} failed (envs={envs:?}); status={:?}\nstdout={stdout}\nstderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    stdout
}

/// One `NAME=value` line the worker printed.
fn field<'a>(stdout: &'a str, name: &str) -> &'a str {
    stdout
        .lines()
        .find_map(|l| l.strip_prefix(name))
        .unwrap_or_else(|| panic!("the L3 child worker did not print {name}\nstdout={stdout}"))
        .trim()
}

fn field_u64(stdout: &str, name: &str) -> u64 {
    field(stdout, name).parse().unwrap_or_else(|e| panic!("{name} was not an integer: {e}\nstdout={stdout}"))
}

/// Nothing at all: the DEFAULT world, which since W1b is the task world.
const DEFAULT_WORLD: &[(&str, &str)] = &[];
/// `MOVA_FLOW_THREAD_PROCS=1` -- the kill switch, on its own: the pre-L3
/// thread-per-proc engine.
const THREAD_WORLD: &[(&str, &str)] = &[("MOVA_FLOW_THREAD_PROCS", "1")];

// ===========================================================================
// The 3-proc chain: the two worlds' headline differential.
// ===========================================================================

/// `l3_chain_child`'s message count. Every buffer in that program is 500,
/// two orders above this, so it measures task-hood and delivery without
/// backpressure in the picture; backpressure has its own block below.
const CHAIN_MSGS: u64 = 50;

/// `l35_mult_child`'s message count. Small on purpose: that worker's slow
/// sink parks 2 ms per message, so this is the whole test's wall clock
/// (~25 ms), and every message is a full straggler wait on the mult -- the
/// path under test -- rather than a rate the fan-out could ever outrun.
const MULT_MSGS: u64 = 12;

/// **Procs are tasks by default.** An interpreted chain never fuses under
/// the default policy, so its three runs are three procs -- and three tasks.
/// Every message still arrives, in order.
#[test]
fn by_default_an_unfused_chain_is_one_task_per_proc() {
    let out = run_child("l3_chain_child", DEFAULT_WORLD);
    assert_eq!(
        field_u64(&out, "L3_TASKS="),
        3,
        "expected one task per proc in an unfused 3-proc chain\nstdout={out}"
    );
    assert_eq!(field(&out, "L3_SUM="), sum_to(CHAIN_MSGS).to_string());
    assert_eq!(field(&out, "L3_ORDERED="), "true");
}

/// **The kill switch still reaches the pre-L3 engine.** The identical
/// program under `MOVA_FLOW_THREAD_PROCS=1` must spawn zero tasks -- and
/// still deliver every message, which is what makes this a differential
/// rather than a tautology about an env var.
#[test]
fn with_the_kill_switch_a_flow_spawns_no_tasks_at_all() {
    let out = run_child("l3_chain_child", THREAD_WORLD);
    assert_eq!(
        field_u64(&out, "L3_TASKS="),
        0,
        "MOVA_FLOW_THREAD_PROCS=1 still spawned a task: the regression escape hatch is not an escape \
         hatch, and flow-gold's thread-world run is asserting on nothing\nstdout={out}"
    );
    assert_eq!(field(&out, "L3_SUM="), sum_to(CHAIN_MSGS).to_string());
    assert_eq!(field(&out, "L3_ORDERED="), "true");
}

/// **A fused run is task-spawned as ONE task** -- the whole run, or none of
/// it (the spec's "simplest correct rule"). `MOVA_FUSE_ALL=1` is the only
/// way to fuse an interpreted chain, and it also exercises the done-cell
/// path's plural case: three members, three done-cells, one guard closing
/// all three when the run's closure ends.
#[test]
fn a_fused_chain_is_exactly_one_task_for_the_whole_run() {
    let out = run_child("l3_chain_child", &[("MOVA_FUSE_ALL", "1")]);
    assert_eq!(
        field_u64(&out, "L3_TASKS="),
        1,
        "a fully-fused 3-proc run must be ONE task, not one per member\nstdout={out}"
    );
    assert_eq!(field(&out, "L3_SUM="), sum_to(CHAIN_MSGS).to_string());
    assert_eq!(field(&out, "L3_ORDERED="), "true");
}

fn sum_to(n: u64) -> u64 {
    (0..n).sum()
}

// ===========================================================================
// Control + shutdown on a task proc.
// ===========================================================================

/// **pause -> inject -> resume is lossless on task procs, and `stop` lands
/// on the done-cell path.**
///
/// The stop leg is TIMED because its failure mode is silent: a task proc has
/// no `JoinHandle`, so if `stop`'s done-cell wait never observes the close it
/// does not hang forever, it burns `STOP_JOIN_TIMEOUT` (5s) per proc and then
/// abandons the proc -- the flow still "stops", just three times five seconds
/// later and with the procs still running. One second is a generous bar for
/// a wake-driven shutdown of three idle procs and is nowhere near 5s.
#[test]
fn pause_inject_resume_is_lossless_and_stop_takes_the_done_cell_path() {
    let out = run_child("l3_control_child", DEFAULT_WORLD);
    assert_eq!(field_u64(&out, "L3_TASKS="), 3);
    assert_eq!(field(&out, "L3_FIRST="), "[0 1 2]");
    // Paused mid-stream, the injected second batch must be sitting in the
    // chain rather than arriving at the sink.
    assert_eq!(field(&out, "L3_WHILE_PAUSED="), "nil");
    assert_eq!(field(&out, "L3_SECOND="), "[3 4 5]");
    let stop_ms = field_u64(&out, "L3_STOP_MS=");
    assert!(
        stop_ms < 1000,
        "flow/stop took {stop_ms}ms on a 3-task flow: that is the done-cell wait falling back to its \
         STOP_JOIN_TIMEOUT cap, i.e. a proc's done-cell was never closed on exit (or `stop` looked \
         at the wrong cell)\nstdout={out}"
    );
}

/// **The OTHER handle arm.** The identical program under the kill switch:
/// three OS threads, so `stop` takes the `join_with_timeout` branch for each
/// of them rather than the done-cell wait. Post-flip both arms are live code
/// -- a task run has no handle, a thread run's head does -- and this is what
/// keeps the thread arm from rotting behind the default.
#[test]
fn in_the_thread_world_the_same_control_program_stops_by_joining() {
    let out = run_child("l3_control_child", THREAD_WORLD);
    assert_eq!(field_u64(&out, "L3_TASKS="), 0);
    assert_eq!(field(&out, "L3_FIRST="), "[0 1 2]");
    assert_eq!(field(&out, "L3_WHILE_PAUSED="), "nil");
    assert_eq!(field(&out, "L3_SECOND="), "[3 4 5]");
    let stop_ms = field_u64(&out, "L3_STOP_MS=");
    assert!(stop_ms < 1000, "flow/stop took {stop_ms}ms on a 3-THREAD flow\nstdout={out}");
}

// ===========================================================================
// Backpressure: the blocked-send task arm (W1b, design §3.4 as superseded).
// ===========================================================================

/// **A blocked send delivers.** Every hop in `l3_backpressure_child` has a
/// buffer of ONE, so a relay task blocks on very nearly every send and only
/// makes progress when the downstream's take frees room and rings the
/// doorbell family it registered on. Losing or duplicating a message here,
/// or wedging, is the failure this pins; a missed wakeup shows up as the
/// child never finishing (the test harness's own timeout), which is why the
/// count is exact rather than "at least".
#[test]
fn a_task_proc_blocked_on_a_full_downstream_still_delivers_every_message() {
    let out = run_child("l3_backpressure_child", DEFAULT_WORLD);
    assert_eq!(field_u64(&out, "L3_TASKS="), 2);
    assert_eq!(field(&out, "L3_SUM="), sum_to(CHAIN_MSGS).to_string());
    assert_eq!(field(&out, "L3_ORDERED="), "true");
}

/// **Control wins while parked.** `l3_blocked_control_child` deliberately
/// leaves `:a` blocked mid-send forever (its consumer `:b` is never resumed
/// and its in-port holds one message), then asks two things of it:
///
/// - a `flow/ping` must be ANSWERED -- proof that the non-blocking control
///   check inside the blocked-send loop still runs, i.e. that the park is
///   woken by the control chan's ring and not only by room appearing;
/// - `flow/stop` must return inside the bar -- proof that a proc parked on
///   backpressure notices `{:op :stop}` and exits, rather than `stop`
///   burning `STOP_JOIN_TIMEOUT` per proc and abandoning it.
///
/// Both procs answer: `:b` is parked in its ordinary control take, `:a` is
/// parked in [`blocked_send_task`]. The bar is 1s against a 5s cap.
#[test]
fn a_task_proc_blocked_mid_send_still_answers_control_and_stops() {
    let out = run_child("l3_blocked_control_child", DEFAULT_WORLD);
    assert_eq!(field_u64(&out, "L3_TASKS="), 2);
    assert_eq!(
        field(&out, "L3_PINGED_A="),
        "true",
        "the proc parked on a full downstream never answered flow/ping: the blocked-send loop is not \
         checking control, or its park is not woken by a control command\nstdout={out}"
    );
    assert_eq!(field(&out, "L3_PINGED_B="), "true");
    let stop_ms = field_u64(&out, "L3_STOP_MS=");
    assert!(
        stop_ms < 1000,
        "flow/stop took {stop_ms}ms with a proc parked mid-send: that is the done-cell wait falling \
         back to its STOP_JOIN_TIMEOUT cap, i.e. the blocked proc never noticed the stop\nstdout={out}"
    );
}

// ===========================================================================
// `:workload :io` -- the opt-out, mixed into a task chain.
// ===========================================================================

/// **`:io` keeps its OS thread, and the chain still flows across the
/// boundary.** Two of the three procs are tasks; the middle one is a
/// thread, so every message crosses task -> thread -> task on plain `Chan`s
/// (the transport lane those conns would otherwise have taken is refused --
/// asserted on the planner directly in `src/builtins/flow.rs`'s unit tests,
/// because a refused lane is invisible from out here by construction).
#[test]
fn an_io_proc_mixed_into_a_task_chain_stays_a_thread_and_messages_still_flow() {
    let out = run_child("l3_io_mix_child", DEFAULT_WORLD);
    assert_eq!(
        field_u64(&out, "L3_TASKS="),
        2,
        "the `:workload :io` proc did not opt out of task-hood (or a task proc opted out)\nstdout={out}"
    );
    assert_eq!(field(&out, "L3_SUM="), sum_to(CHAIN_MSGS).to_string());
    assert_eq!(field(&out, "L3_ORDERED="), "true");
}

// ===========================================================================
// Timers.
// ===========================================================================

/// **A `timeout` chan wakes a task proc.** Design §2 says this needs no work
/// -- `timer_loop` wakes via `chan_close`, which drains `task_takers` before
/// its `notify_all`, so the timer thread never has to know whether its
/// waiter is a thread or a task. Zero work is still a claim, and this is
/// what pins it on the proc path.
///
/// The elapsed lower bound is what proves the take really PARKED rather than
/// falling through: 10 messages each waiting 20ms cannot finish in under
/// 100ms unless the timeout chan handed back early.
#[test]
fn a_timeout_chan_inside_a_task_proc_parks_and_wakes() {
    let out = run_child("l3_timeout_child", DEFAULT_WORLD);
    assert_eq!(field_u64(&out, "L3_TASKS="), 2);
    assert_eq!(field(&out, "L3_SUM="), sum_to(10).to_string());
    let elapsed = field_u64(&out, "L3_ELAPSED_MS=");
    assert!(
        elapsed >= 100,
        "10 messages x (<!! (timeout 20)) finished in {elapsed}ms: the timeout chan did not actually \
         park the task\nstdout={out}"
    );
    assert!(
        elapsed < 10_000,
        "10 messages x (<!! (timeout 20)) took {elapsed}ms: a timer wake was LOST and the proc only \
         made progress on some other event\nstdout={out}"
    );
}

// ===========================================================================
// W2: de-polled stragglers (docs/L3-LANDING-SPEC.md §W2).
// ===========================================================================

/// **`flow/ping` from inside a `go` block returns, and doesn't wedge.**
/// `native_ping`'s reply-collection loop used to be a `thread::sleep` poll;
/// from inside a task that burns the host SHARD on every tick instead of
/// yielding it (design §3.7's "a `flow/ping` inside a `go` block must not
/// burn a shard"). 20 back-to-back pings inside one `go` body exercise the
/// de-polled tick repeatedly rather than depending on a single lucky (or
/// unlucky) race: the very FIRST scan pass in `native_ping`'s loop almost
/// always finds both replies still in flight (the ping commands were JUST
/// enqueued when the scan runs), so a broken tick -- hung forever, or one
/// that never actually re-scans -- shows up as this test not returning at
/// all, rather than as a subtle timing difference.
#[test]
fn ping_from_inside_a_go_block_returns_and_does_not_wedge() {
    let out = run_child("l3_ping_from_go_child", DEFAULT_WORLD);
    assert_eq!(
        field(&out, "L3_PING_OK="),
        "true",
        "a flow/ping from inside a go block returned an incomplete reply map at least once across 20 \
         calls\nstdout={out}"
    );
    let elapsed = field_u64(&out, "L3_ELAPSED_MS=");
    assert!(
        elapsed < 3000,
        "20 flow/ping calls (1000ms budget each) from inside a go block took {elapsed}ms total: that \
         is close to exhausting their timeout budget instead of returning as soon as both procs \
         replied -- the de-polled tick is likely wedged or not re-scanning\nstdout={out}"
    );
}

/// **`stop` is bounded by ONE deadline across every done-cell wait, not one
/// per wedged member.** A 3-member fused run (`:a` is [`WEDGE`], tagged
/// `:workload :io` so the mixed-fusion rule keeps the whole run an OS
/// thread rather than a task -- see `flow.rs`'s module doc, "Fusion and
/// task-hood"; `:b`/`:c` are plain relay/sink) whose head blocks forever
/// the moment it is handed a message. `stop`'s per-pid loop then sees:
/// `:a` (the run's HEAD) has a real `JoinHandle` -- `join_with_timeout`
/// pays its own full `STOP_JOIN_TIMEOUT` (5s) and detaches, UNAFFECTED by
/// this fix (joins keep their own per-proc cap, pre-L3 exact). `:b` and
/// `:c` are the SAME wedged run's non-head members: their done-cells are
/// closed by the exact same `DoneCells` guard as `:a`'s, which never fires
/// because the run's one thread never returns. Before W2, each of THEIR
/// waits therefore burned its own independent `STOP_JOIN_TIMEOUT` too --
/// 3 * 5s = 15s total for one wedged run. With one shared deadline
/// computed before the per-pid loop starts, `:b` and `:c`'s waits see a
/// deadline already in the past (the whole budget was spent on `:a`'s
/// join) and return immediately: ~5s total, regardless of member count.
///
/// This test's own wall time is therefore itself ~5s (the one join
/// `stop` cannot avoid paying) -- `#[ignore]`d as the slow probe it is;
/// run with `cargo test --release --test l3_task_procs_test -- --ignored
/// stop_is_bounded` to exercise it. The 8s bar leaves ample margin above
/// the ~5s fixed behavior while sitting nowhere near the ~15s broken one.
#[test]
#[ignore]
fn stop_is_bounded_by_one_deadline_not_one_per_wedged_fused_member() {
    let out = run_child("l3_stop_deadline_child", &[("MOVA_FUSE_ALL", "1")]);
    let stop_ms = field_u64(&out, "L3_STOP_MS=");
    assert!(
        stop_ms < 8000,
        "flow/stop on a 3-member wedged fused run took {stop_ms}ms: that is closer to \
         3 * STOP_JOIN_TIMEOUT (~15000ms, one full timeout per member) than to one shared \
         deadline (~5000ms) -- the done-cell wait loop is re-arming a fresh STOP_JOIN_TIMEOUT \
         per pid instead of sharing one across the whole loop\nstdout={out}"
    );
    assert!(
        stop_ms >= 3000,
        "flow/stop on a genuinely wedged fused run returned in only {stop_ms}ms: that is fast \
         enough to suggest the join/wait never actually engaged STOP_JOIN_TIMEOUT at all (e.g. the \
         wedge step didn't really block, or the flow never started)\nstdout={out}"
    );
}

// ===========================================================================
// The mult (fan-out): L3.5 item 2's differential.
// ===========================================================================

/// **A fan-out conn costs ZERO OS threads by default.** L3 left the mult
/// (`run_mult_thread`) an OS thread per fan-out/self-loop out-port -- the
/// last thread the default world spawned per graph node -- on design §3.7's
/// reading that converting it needed a doorbell first. L3.5 item 2 converted
/// it (`run_mult_task`): the mult has no control-chan awareness to race a
/// doorbell against, and both of its waits are task-native after L1/W3, so
/// the conversion is a blocking `chan_put` per straggler in place of the
/// `try_put`/`MULTI_INPUT_BACKOFF`-sleep retry loop.
///
/// This is a WHITE-BOX assertion (`internal::flow_probe::mult_spawn_census`)
/// on purpose. Nothing behavioral can see it -- a fan-out flow delivers
/// every message either way, which is exactly what the same worker's
/// backpressure half checks -- so "the mult is not an OS thread any more"
/// has to be asserted on the spawn seam itself. Counting BOTH worlds' mults
/// is what makes it a differential rather than a tautology: the same
/// topology must produce one THREAD mult under the kill switch and one TASK
/// mult by default, never zero of both (which is what "the mult stopped
/// being wired at all" would look like).
#[test]
fn by_default_a_fan_out_conn_spawns_a_mult_task_and_no_os_thread() {
    let out = run_child("l35_mult_child", DEFAULT_WORLD);
    assert_eq!(
        field_u64(&out, "L35_MULT_TASKS="),
        1,
        "expected the one fan-out out-port's mult to be spawned as a TASK\nstdout={out}"
    );
    assert_eq!(
        field_u64(&out, "L35_MULT_THREADS="),
        0,
        "a fan-out conn still cost an OS thread in the default world -- L3.5 item 2's \
         whole claim is that it costs none\nstdout={out}"
    );
    // 4 procs (none fusable: `:src`'s only out-port fans out, and no sink
    // has a successor) + the one mult.
    assert_eq!(field_u64(&out, "L35_TASKS="), 5, "expected 4 proc tasks + 1 mult task\nstdout={out}");
    assert_eq!(field(&out, "L35_FANOUT_OK="), "true");
}

/// **The kill switch still spawns the mult THREAD.** `MOVA_FLOW_THREAD_PROCS=1`
/// has to reproduce the pre-L3 engine thread for thread, and that includes
/// `run_mult_thread` -- `MULTI_INPUT_BACKOFF` sleep and all, which is why
/// that constant survives at all. Zero tasks, one mult thread, same
/// delivery.
#[test]
fn the_kill_switch_keeps_the_mult_on_its_own_os_thread() {
    let out = run_child("l35_mult_child", THREAD_WORLD);
    assert_eq!(
        field_u64(&out, "L35_MULT_THREADS="),
        1,
        "under MOVA_FLOW_THREAD_PROCS=1 the fan-out out-port must still get its own OS \
         thread -- the kill switch is the pre-L3 engine\nstdout={out}"
    );
    assert_eq!(field_u64(&out, "L35_MULT_TASKS="), 0, "the kill switch must spawn no mult task\nstdout={out}");
    assert_eq!(field_u64(&out, "L35_TASKS="), 0, "the kill switch must spawn no tasks at all\nstdout={out}");
    assert_eq!(field(&out, "L35_FANOUT_OK="), "true");
}

/// **The task mult cannot deadlock a self-loop, even co-shard.** A self-loop
/// always routes through a mult (`plan_fusion`'s "`A != B`" clause), which
/// means the mult's only dest is the very proc feeding it. Fill that dest's
/// buffer and the mult is parked in a blocking put on a chan nobody but that
/// proc can drain -- so the proc MUST get to run. `MOVA_FLOW_PLACEMENT=pin:0`
/// makes that maximally hostile by putting both on one shard, where the only
/// thing that lets the proc run is the mult's park actually yielding the
/// worker (`chan_put` -> `task_putters` + commit cell, L1/W3). A sleeping
/// straggler wait -- the thread world's `MULTI_INPUT_BACKOFF` loop, ported
/// naively -- would hang here forever, which is precisely why L3.5 deleted
/// it rather than de-polling it.
#[test]
fn a_co_sharded_self_loop_mult_task_yields_instead_of_wedging() {
    let out = run_child("l35_self_loop_child", &[("MOVA_FLOW_PLACEMENT", "pin:0")]);
    assert_eq!(
        field_u64(&out, "L35_LOOP_COUNT="),
        MULT_MSGS,
        "the co-sharded self-loop did not deliver every message -- the drain's 5s alts!! \
         timeout fired, i.e. the mult task and the proc it feeds wedged each other on one \
         shard\nstdout={out}"
    );
    assert_eq!(field(&out, "L35_LOOP_ORDERED="), "true");
    // 2 procs (`:l` has two ins and two outs, so nothing fuses) + the mult.
    assert_eq!(field_u64(&out, "L3_TASKS="), 3, "expected 2 proc tasks + 1 mult task\nstdout={out}");
}

/// The same self-loop under the kill switch, where the mult is still an OS
/// thread with its sleep backoff: the pre-L3 behavior, unchanged. Placement
/// is meaningless there (an OS thread's placement belongs to the OS), so
/// this is the differential's other half -- same program, same delivery,
/// zero tasks.
#[test]
fn the_kill_switch_self_loop_still_runs_on_its_mult_thread() {
    let out = run_child("l35_self_loop_child", THREAD_WORLD);
    assert_eq!(field_u64(&out, "L35_LOOP_COUNT="), MULT_MSGS, "stdout={out}");
    assert_eq!(field(&out, "L35_LOOP_ORDERED="), "true");
    assert_eq!(field_u64(&out, "L3_TASKS="), 0, "the kill switch must spawn no tasks at all\nstdout={out}");
}

// ===========================================================================
// The child workers. Each runs one mova program and prints `NAME=value`
// lines for its parent to assert on. `#[ignore]`d so the ordinary sweep runs
// only the parents (which spawn these with the environment they need).
// ===========================================================================

/// `run` an mova program on a fresh `Interp`, printing `L3_TASKS=` (the
/// tasks the whole program spawned) around it. A count, not a probe into the
/// engine: nothing else in these programs spawns a task, so a delta of N is
/// N proc tasks.
fn eval_worker(src: &str) -> mova::internal::Value {
    let mut interp = mova::internal::Interp::new();
    let before = runtime::tasks_spawned();
    let v = interp
        .eval_str("l3-worker", src)
        .unwrap_or_else(|e| panic!("l3 worker eval failed: {}", mova::internal::render(&e, "l3-worker", src)));
    println!("L3_TASKS={}", runtime::tasks_spawned() - before);
    v
}

/// Every proc's in-port buffer, and the sink chan's, in the workers that are
/// NOT about backpressure: ten times `CHAIN_MSGS`, so those workers measure
/// task-hood and delivery with no send ever blocking. The backpressure
/// workers below deliberately use a buffer of 1 instead.
const BUF: u64 = 500;

/// A relay step: passes each message straight through.
const RELAY: &str = r#"(flow/map->step {:describe (fn [] {:ins {:in {}} :outs {:out {}}})
                                        :transform (fn [s _ m] [s {:out [m]}])})"#;

/// A sink step: forwards each message onto `sink-ch` and declares no outs.
const SINK: &str = r#"(flow/map->step {:describe (fn [] {:ins {:in {}} :outs {}})
                                       :transform (fn [s _ m] (>!! sink-ch m) [s {}])})"#;

/// A step that never returns from its first `transform` call -- a
/// deliberately wedged proc, for the stop-deadline test below. `sleep-ms`
/// is a bare `std::thread::sleep`, so this genuinely never comes back
/// (~11.5 days), which is the point: the proc's done-cell never closes for
/// the life of the test process.
const WEDGE: &str = r#"(flow/map->step {:describe (fn [] {:ins {:in {}} :outs {:out {}}})
                                        :transform (fn [s _ m] (sleep-ms 999999999) [s {:out [m]}])})"#;

/// `:chan-opts` giving a proc's `:in` port a buffer far above any worker's
/// message count.
fn big_in() -> String {
    format!(":chan-opts {{:in {{:buf-or-n {BUF}}}}}")
}

/// The 3-proc chain the first three tests share: `:a` -> `:b` -> `:c`,
/// `CHAIN_MSGS` messages injected at the head, collected at the sink.
/// Prints their sum and whether they arrived in order.
#[test]
#[ignore]
fn l3_chain_child() {
    let (relay, sink, opts, n) = (RELAY, SINK, big_in(), CHAIN_MSGS);
    let out = eval_worker(&format!(
        r#"(let [relay {relay}
                 sink-ch (chan {BUF})
                 sink {sink}
                 fl (flow/create-flow
                      {{:procs {{:a {{:proc (flow/process relay) {opts}}}
                                :b {{:proc (flow/process relay) {opts}}}
                                :c {{:proc (flow/process sink) {opts}}}}}
                        :conns [[[:a :out] [:b :in]] [[:b :out] [:c :in]]]}})
                 _ (flow/start fl)
                 _ (flow/resume fl)
                 _ (flow/inject fl [:a :in] (vec (range {n})))
                 got (loop [i 0 acc []]
                       (if (= i {n}) acc (recur (inc i) (conj acc (<!! sink-ch)))))]
             (flow/stop fl)
             [(reduce + got) (= got (vec (range {n})))])"#
    ));
    print_pair(&out, "L3_SUM=", "L3_ORDERED=");
}

/// pause -> inject -> resume -> drain, then a SEPARATELY TIMED `flow/stop`
/// (two `eval_str` calls on one `Interp`, so the stop leg is the only thing
/// inside the timer).
#[test]
#[ignore]
fn l3_control_child() {
    let (relay, sink, opts) = (RELAY, SINK, big_in());
    let mut interp = mova::internal::Interp::new();
    let before = runtime::tasks_spawned();
    let setup = format!(
        r#"(do
             (def sink-ch (chan {BUF}))
             (def fl (flow/create-flow
                       {{:procs {{:a {{:proc (flow/process {relay}) {opts}}}
                                 :b {{:proc (flow/process {relay}) {opts}}}
                                 :c {{:proc (flow/process {sink}) {opts}}}}}
                         :conns [[[:a :out] [:b :in]] [[:b :out] [:c :in]]]}}))
             (flow/start fl)
             (flow/resume fl)
             (flow/inject fl [:a :in] [0 1 2])
             (def first-batch [(<!! sink-ch) (<!! sink-ch) (<!! sink-ch)])
             ;; The chain is idle here (nothing in flight), so this pause is
             ;; unambiguous rather than raced against an arriving message --
             ;; `tests/flow_test.rs`'s convention.
             (flow/pause fl)
             (sleep-ms 50)
             (flow/inject fl [:a :in] [3 4 5])
             (sleep-ms 100)
             (def while-paused (poll! sink-ch))
             (flow/resume fl)
             (def second-batch [(<!! sink-ch) (<!! sink-ch) (<!! sink-ch)])
             [first-batch while-paused second-batch])"#
    );
    let v = interp
        .eval_str("l3-control", &setup)
        .unwrap_or_else(|e| panic!("l3 control worker failed: {}", mova::internal::render(&e, "l3-control", &setup)));
    println!("L3_TASKS={}", runtime::tasks_spawned() - before);
    let mova::internal::Value::Vector(v) = &v else { panic!("expected a 3-vector, got {v:?}") };
    println!("L3_FIRST={}", mova::internal::pr_str(&v[0]));
    println!("L3_WHILE_PAUSED={}", mova::internal::pr_str(&v[1]));
    println!("L3_SECOND={}", mova::internal::pr_str(&v[2]));

    let t = std::time::Instant::now();
    interp.eval_str("l3-stop", "(flow/stop fl)").expect("flow/stop");
    println!("L3_STOP_MS={}", t.elapsed().as_millis());
}

/// `:a` -> `:b` with a buffer of ONE at every hop: `:b`'s in-port, and the
/// `sink-ch` `:b` forwards onto. `:a`'s own in-port is roomy so the
/// `flow/inject` that seeds the run doesn't block the injector instead --
/// the point is to block the PROC, in [`send_with_control_priority`]'s task
/// arm, on essentially every message.
#[test]
#[ignore]
fn l3_backpressure_child() {
    let (relay, sink, n) = (RELAY, SINK, CHAIN_MSGS);
    let out = eval_worker(&format!(
        r#"(let [relay {relay}
                 sink-ch (chan 1)
                 sink {sink}
                 fl (flow/create-flow
                      {{:procs {{:a {{:proc (flow/process relay) :chan-opts {{:in {{:buf-or-n {BUF}}}}}}}
                                :b {{:proc (flow/process sink) :chan-opts {{:in {{:buf-or-n 1}}}}}}}}
                        :conns [[[:a :out] [:b :in]]]}})
                 _ (flow/start fl)
                 _ (flow/resume fl)
                 _ (flow/inject fl [:a :in] (vec (range {n})))
                 got (loop [i 0 acc []]
                       (if (= i {n}) acc (recur (inc i) (conj acc (<!! sink-ch)))))]
             (flow/stop fl)
             [(reduce + got) (= got (vec (range {n})))])"#
    ));
    print_pair(&out, "L3_SUM=", "L3_ORDERED=");
}

/// `:a` blocked mid-send FOREVER, then pinged and stopped.
///
/// `:b` is never resumed, so it never reads its in-port; that port holds
/// exactly one message, so `:a` -- which IS resumed and has a queue of its
/// own -- lands in the blocked-send park and stays there. Everything after
/// the `sleep-ms` is therefore asked of a proc that is parked on
/// backpressure: first a `flow/ping` (which must be answered from inside the
/// blocked-send loop's control check), then a separately timed `flow/stop`.
#[test]
#[ignore]
fn l3_blocked_control_child() {
    let (relay, sink) = (RELAY, SINK);
    let mut interp = mova::internal::Interp::new();
    let before = runtime::tasks_spawned();
    let setup = format!(
        r#"(do
             (def sink-ch (chan 1))
             (def fl (flow/create-flow
                       {{:procs {{:a {{:proc (flow/process {relay}) :chan-opts {{:in {{:buf-or-n {BUF}}}}}}}
                                 :b {{:proc (flow/process {sink}) :chan-opts {{:in {{:buf-or-n 1}}}}}}}}
                         :conns [[[:a :out] [:b :in]]]}}))
             (flow/start fl)
             ;; `:a` only -- `:b` stays paused, so nothing ever drains it.
             (flow/resume-proc fl :a)
             (flow/inject fl [:a :in] (vec (range 20)))
             (sleep-ms 200)
             (def pinged (flow/ping fl 2000))
             [(boolean (get pinged :a)) (boolean (get pinged :b))])"#
    );
    let v = interp.eval_str("l3-blocked", &setup).unwrap_or_else(|e| {
        panic!("l3 blocked-control worker failed: {}", mova::internal::render(&e, "l3-blocked", &setup))
    });
    println!("L3_TASKS={}", runtime::tasks_spawned() - before);
    let mova::internal::Value::Vector(v) = &v else { panic!("expected a 2-vector, got {v:?}") };
    println!("L3_PINGED_A={}", mova::internal::pr_str(&v[0]));
    println!("L3_PINGED_B={}", mova::internal::pr_str(&v[1]));

    let t = std::time::Instant::now();
    interp.eval_str("l3-stop", "(flow/stop fl)").expect("flow/stop");
    println!("L3_STOP_MS={}", t.elapsed().as_millis());
}

/// The same chain with the MIDDLE proc declared `:workload :io`, so it keeps
/// its OS thread while its neighbours become tasks.
#[test]
#[ignore]
fn l3_io_mix_child() {
    let (relay, sink, opts, n) = (RELAY, SINK, big_in(), CHAIN_MSGS);
    let out = eval_worker(&format!(
        r#"(let [relay {relay}
                 sink-ch (chan {BUF})
                 sink {sink}
                 fl (flow/create-flow
                      {{:procs {{:a {{:proc (flow/process relay) {opts}}}
                                :b {{:proc (flow/process relay {{:workload :io}}) {opts}}}
                                :c {{:proc (flow/process sink) {opts}}}}}
                        :conns [[[:a :out] [:b :in]] [[:b :out] [:c :in]]]}})
                 _ (flow/start fl)
                 _ (flow/resume fl)
                 _ (flow/inject fl [:a :in] (vec (range {n})))
                 got (loop [i 0 acc []]
                       (if (= i {n}) acc (recur (inc i) (conj acc (<!! sink-ch)))))]
             (flow/stop fl)
             [(reduce + got) (= got (vec (range {n})))])"#
    ));
    print_pair(&out, "L3_SUM=", "L3_ORDERED=");
}

/// A two-proc flow whose HEAD proc waits on a `timeout` chan inside its own
/// transform before emitting -- the timer wake, on the proc path, in a task.
#[test]
#[ignore]
fn l3_timeout_child() {
    let (sink, opts) = (SINK, big_in());
    let t = std::time::Instant::now();
    let out = eval_worker(&format!(
        r#"(let [slow (flow/map->step
                        {{:describe (fn [] {{:ins {{:in {{}}}} :outs {{:out {{}}}}}})
                          :transform (fn [s _ m] (<!! (timeout 20)) [s {{:out [m]}}])}})
                 sink-ch (chan {BUF})
                 sink {sink}
                 fl (flow/create-flow
                      {{:procs {{:a {{:proc (flow/process slow) {opts}}}
                                :b {{:proc (flow/process sink) {opts}}}}}
                        :conns [[[:a :out] [:b :in]]]}})
                 _ (flow/start fl)
                 _ (flow/resume fl)
                 _ (flow/inject fl [:a :in] (vec (range 10)))
                 got (loop [i 0 acc []]
                       (if (= i 10) acc (recur (inc i) (conj acc (<!! sink-ch)))))]
             (flow/stop fl)
             [(reduce + got) (= got (vec (range 10)))])"#
    ));
    println!("L3_ELAPSED_MS={}", t.elapsed().as_millis());
    print_pair(&out, "L3_SUM=", "L3_ORDERED=");
}

/// A 2-proc flow, pinged 20 times back to back from inside ONE `go` block.
/// See `ping_from_inside_a_go_block_returns_and_does_not_wedge` for what
/// this pins.
#[test]
#[ignore]
fn l3_ping_from_go_child() {
    let (relay, sink, opts) = (RELAY, SINK, big_in());
    let t = std::time::Instant::now();
    let out = eval_worker(&format!(
        r#"(let [relay {relay}
                 sink-ch (chan {BUF})
                 sink {sink}
                 fl (flow/create-flow
                      {{:procs {{:a {{:proc (flow/process relay) {opts}}}
                                :b {{:proc (flow/process sink) {opts}}}}}
                        :conns [[[:a :out] [:b :in]]]}})
                 _ (flow/start fl)
                 _ (flow/resume fl)
                 pings (<!! (go (loop [i 0 acc []]
                                  (if (= i 20)
                                    acc
                                    (recur (inc i) (conj acc (flow/ping fl 1000)))))))]
             (flow/stop fl)
             (every? (fn [m] (and (contains? m :a) (contains? m :b))) pings))"#
    ));
    println!("L3_ELAPSED_MS={}", t.elapsed().as_millis());
    println!("L3_PING_OK={}", mova::internal::pr_str(&out));
}

/// A 3-member fused chain (`:a` [`WEDGE`] tagged `:workload :io`, `:b`
/// [`RELAY`], `:c` [`SINK`]) whose head never returns once given a message.
/// See `stop_is_bounded_by_one_deadline_not_one_per_wedged_fused_member` for
/// what this pins. `MOVA_FUSE_ALL=1` (set by the parent test) is what fuses
/// an otherwise-unbranching interpreted 3-proc chain at all.
#[test]
#[ignore]
fn l3_stop_deadline_child() {
    let (wedge, relay, sink, opts) = (WEDGE, RELAY, SINK, big_in());
    let mut interp = mova::internal::Interp::new();
    let setup = format!(
        r#"(do
             (def sink-ch (chan {BUF}))
             (def fl (flow/create-flow
                       {{:procs {{:a {{:proc (flow/process {wedge} {{:workload :io}}) {opts}}}
                                 :b {{:proc (flow/process {relay}) {opts}}}
                                 :c {{:proc (flow/process {sink}) {opts}}}}}
                         :conns [[[:a :out] [:b :in]] [[:b :out] [:c :in]]]}}))
             (flow/start fl)
             (flow/resume fl)
             (flow/inject fl [:a :in] [0])
             ;; Give the fused thread time to actually pick up the message
             ;; and enter the wedge before `flow/stop` is separately timed
             ;; below -- otherwise `stop` could race a proc that hasn't even
             ;; started its transform yet.
             (sleep-ms 200))"#
    );
    interp.eval_str("l3-stop-deadline-setup", &setup).unwrap_or_else(|e| {
        panic!("l3 stop-deadline setup failed: {}", mova::internal::render(&e, "l3-stop-deadline-setup", &setup))
    });

    let t = std::time::Instant::now();
    interp.eval_str("l3-stop-deadline-stop", "(flow/stop fl)").expect("flow/stop");
    println!("L3_STOP_MS={}", t.elapsed().as_millis());
}

/// `:src` fans out to THREE sinks, one of them deliberately slow behind a
/// one-deep in-port buffer -- so the mult cannot get rid of a message on its
/// fast pass and has to take its straggler wait on essentially every one.
/// That is the path L3.5 item 2 replaced (`try_put` + 200 µs sleep -> a
/// blocking `chan_put`), and this worker exercises it in whichever world its
/// parent set up.
///
/// The slow sink waits on a `timeout` chan, NOT `sleep-ms`: a task must
/// never sleep, and a `sleep-ms` here would stall its whole shard -- along
/// with, quite possibly, the mult task the test is about, which would turn a
/// real backpressure test into an artifact of co-placement.
///
/// Prints the two mult-census deltas, the total task delta, and whether all
/// three sinks received all `MULT_MSGS` messages in order. The last one is
/// the ordinary correctness claim (fan-out never drops, never reorders,
/// never lets a slow dest starve a fast one); the census lines are the
/// white-box claim its parents actually gate.
#[test]
#[ignore]
fn l35_mult_child() {
    let (relay, n) = (RELAY, MULT_MSGS);
    let mut interp = mova::internal::Interp::new();
    let (mult_tasks_before, mult_threads_before) = mova::internal::flow_probe::mult_spawn_census();
    let before = runtime::tasks_spawned();
    let src = format!(
        r#"(let [relay {relay}
                 a-ch (chan {BUF}) b-ch (chan {BUF}) c-ch (chan {BUF})
                 fast-a (flow/map->step {{:describe (fn [] {{:ins {{:in {{}}}} :outs {{}}}})
                                          :transform (fn [s _ m] (>!! a-ch m) [s {{}}])}})
                 fast-c (flow/map->step {{:describe (fn [] {{:ins {{:in {{}}}} :outs {{}}}})
                                          :transform (fn [s _ m] (>!! c-ch m) [s {{}}])}})
                 ;; The slow one. `(<!! (timeout 2))` parks the task on the
                 ;; shared timer service instead of burning its shard.
                 slow-b (flow/map->step {{:describe (fn [] {{:ins {{:in {{}}}} :outs {{}}}})
                                          :transform (fn [s _ m] (<!! (timeout 2)) (>!! b-ch m) [s {{}}])}})
                 fl (flow/create-flow
                      {{:procs {{:src {{:proc (flow/process relay) :chan-opts {{:in {{:buf-or-n {BUF}}}}}}}
                                :a {{:proc (flow/process fast-a) :chan-opts {{:in {{:buf-or-n {BUF}}}}}}}
                                ;; ONE deep: the mult's fast-pass try_put to
                                ;; :b fails almost immediately and it must
                                ;; take the blocking straggler wait.
                                :b {{:proc (flow/process slow-b) :chan-opts {{:in {{:buf-or-n 1}}}}}}
                                :c {{:proc (flow/process fast-c) :chan-opts {{:in {{:buf-or-n {BUF}}}}}}}}}
                        :conns [[[:src :out] [:a :in]]
                                [[:src :out] [:b :in]]
                                [[:src :out] [:c :in]]]}})
                 _ (flow/start fl)
                 _ (flow/resume fl)
                 _ (flow/inject fl [:src :in] (vec (range {n})))
                 drain (fn [ch] (loop [i 0 acc []]
                                  (if (= i {n}) acc (recur (inc i) (conj acc (<!! ch))))))
                 ;; :b LAST, so the fast dests are read while the mult is
                 ;; genuinely parked on :b's full buffer rather than after
                 ;; the backpressure has already drained away.
                 got-a (drain a-ch)
                 got-c (drain c-ch)
                 got-b (drain b-ch)
                 expected (vec (range {n}))]
             (flow/stop fl)
             (and (= got-a expected) (= got-b expected) (= got-c expected)))"#
    );
    let v = interp
        .eval_str("l35-mult", &src)
        .unwrap_or_else(|e| panic!("l35 mult worker failed: {}", mova::internal::render(&e, "l35-mult", &src)));
    println!("L35_TASKS={}", runtime::tasks_spawned() - before);
    let (mult_tasks, mult_threads) = mova::internal::flow_probe::mult_spawn_census();
    println!("L35_MULT_TASKS={}", mult_tasks - mult_tasks_before);
    println!("L35_MULT_THREADS={}", mult_threads - mult_threads_before);
    println!("L35_FANOUT_OK={}", mova::internal::pr_str(&v));
}

/// The adversarial self-loop: `:l`'s `:self-out` feeds a mult whose ONLY
/// dest is `:l`'s own `:self-in`, buffered ONE deep, and the seed message
/// makes `:l` emit a burst of `MULT_MSGS` into it at once. So the mult is
/// parked in a blocking put on a chan that only `:l` can drain, for
/// essentially the whole run -- and its parent runs this with
/// `MOVA_FLOW_PLACEMENT=pin:0`, which puts the mult task and `:l` on the
/// SAME shard. If a parked mult did not yield its shard (a `sleep`-based
/// straggler wait, say -- what `run_mult_thread` still does, and what a
/// naive port of it into the task world would have kept), `:l` could never
/// run to drain `:self-in` and this would deadlock outright.
///
/// `:self-out`'s own buffer is sized above the burst deliberately: `:l` must
/// get all `MULT_MSGS` out and RETURN from its transform, or it would be the
/// PROC blocking on the mult's source chan, which is an ordinary
/// application-level cycle and not the shape under test.
///
/// The drain is `alts!!`-with-timeout rather than a bare `<!!` so a
/// regression here fails the assertion instead of hanging the suite.
#[test]
#[ignore]
fn l35_self_loop_child() {
    let n = MULT_MSGS;
    let out = eval_worker(&format!(
        r#"(let [sink-ch (chan {BUF})
                 looper (flow/map->step
                          {{:describe (fn [] {{:ins {{:in {{}} :self-in {{}}}}
                                              :outs {{:self-out {{}} :out {{}}}}}})
                            :transform (fn [s cid m]
                                         (if (= cid :self-in)
                                           [s {{:out [m]}}]
                                           [s {{:self-out (vec (range {n}))}}]))}})
                 sink {SINK}
                 fl (flow/create-flow
                      {{:procs {{:l {{:proc (flow/process looper)
                                     :chan-opts {{:in {{:buf-or-n {BUF}}}
                                                 :self-in {{:buf-or-n 1}}
                                                 :self-out {{:buf-or-n {BUF}}}}}}}
                                :s {{:proc (flow/process sink) :chan-opts {{:in {{:buf-or-n {BUF}}}}}}}}}
                        :conns [[[:l :self-out] [:l :self-in]]
                                [[:l :out] [:s :in]]]}})
                 _ (flow/start fl)
                 _ (flow/resume fl)
                 _ (flow/inject fl [:l :in] [:go])
                 got (loop [i 0 acc []]
                       (if (= i {n})
                         acc
                         (let [v (first (alts!! [sink-ch (timeout 5000)]))]
                           (if (nil? v) acc (recur (inc i) (conj acc v))))))]
             (flow/stop fl)
             [(count got) (= got (vec (range {n})))])"#
    ));
    print_pair(&out, "L35_LOOP_COUNT=", "L35_LOOP_ORDERED=");
}

/// Prints a worker's `[sum ordered?]` result pair as two named lines.
fn print_pair(v: &mova::internal::Value, sum_name: &str, ordered_name: &str) {
    let mova::internal::Value::Vector(v) = v else { panic!("expected a 2-vector, got {v:?}") };
    println!("{sum_name}{}", mova::internal::pr_str(&v[0]));
    println!("{ordered_name}{}", mova::internal::pr_str(&v[1]));
}
