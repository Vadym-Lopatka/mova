//! L1/W4 gate tests for `go`/`put!`/`take!` rerouted onto the real task
//! runtime (docs/L1-LANDING-SPEC.md §W4). Ported from the L1 P2 probe's now-
//! deleted gate suite (docs/L1-PROBE-RESULTS.md): same five claims, driven
//! through the SHIPPING surface now -- plain `go`/`go-loop`, no feature
//! flag and no separate task-spawning native -- because W4's whole point is
//! that `go` reaches the real runtime with zero new spelling.
//!
//! Every gate drives REAL Mova source through the embed facade, so what is
//! under test is the shipping interpreter parking tasks on shipping `Chan`s.
//!
//! Two differences from the probe worth flagging, both because the probe
//! was a single-OS-thread scheduler and the landed runtime is N pinned
//! shards (docs/L1-LANDING-SPEC.md §W2):
//!
//! - There is no `scheduler_threads() == 1` assertion here -- that invariant
//!   was specific to the probe's one-thread-total shape. The landed
//!   equivalent is `runtime::shard_count()`, which is `>= 1` and typically
//!   > 1 on any real machine; a task's shard is chosen round-robin at spawn
//!   and never migrates -- EXCEPT that as of W4b, "round-robin" is only the
//!   fallback: a spawn made from inside a task prefers its caller's own
//!   shard while that shard isn't flooded (`src/runtime/mod.rs`, "Spawn
//!   placement"). A root spawn -- from a plain OS thread, as every G1/G2/G5
//!   `go` here is -- is unaffected and still round-robins.
//! - G1's probe also carried an R5 stack-depth measurement
//!   (`last_park_stack_bytes`/`peak_park_stack_bytes`) that informed the
//!   already-landed `TASK_STACK_SIZE` constant in `src/runtime/mod.rs`.
//!   That instrumentation was probe-only plumbing, not part of the runtime's
//!   public API, and its job is already done (the constant it justified is
//!   what ships) -- so only G1's actual claim (deep park through
//!   interpreted frames succeeds) is ported, not the measurement harness.
//! - G3 now takes TWO measurements instead of one (W4b): ping/pong spawned
//!   from a parent `go` coordinator (the natural CSP shape) co-locate on
//!   one shard under W4b's family-local placement, so that tripwire is
//!   tight again (1000 ns); ping/pong spawned from this file's plain OS
//!   thread are still a GUARANTEED cross-shard rendezvous under round-robin
//!   (W4b only changes task-spawned placement, not root spawns) and keep
//!   W4's widened 6000 ns tripwire. See the comments at both asserts for
//!   the full argument, including the still-open W5 flag.
//!
//! `--test-threads=1` is not required (unlike the probe): counters here are
//! `Relaxed` monotonic process-global atomics read only as `>=` deltas, so a
//! default parallel run is correct, just noisier for the timing prints.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use mova::embed::{Engine, Value};
use mova::internal::task_chan::{self, BufferPolicy};
use mova::runtime;

/// The gates share process-wide runtime counters.
static SERIAL: Mutex<()> = Mutex::new(());

fn engine() -> Engine {
    Engine::builder().build()
}

fn eval(e: &mut Engine, src: &str) -> Value {
    e.eval_named("task-runtime-go", src)
        .unwrap_or_else(|err| panic!("eval error: {}", err.render_plain()))
}

/// Take `SERIAL`, recovering from a poisoned lock. These gates are
/// independent claims sharing only process-global counters for their
/// deltas; one gate's assertion failure must not cascade-fail its siblings
/// just because it happened to hold the mutex when it panicked.
fn serial_lock() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

/// Spin-with-sleep until `cond`, or fail with `what`. A task's own resume
/// (which is what increments e.g. `tasks_panicked`) can race a plain Rust
/// channel send from a SEPARATE task on a different shard, so "the message
/// arrived" is not proof "the counter update landed" -- this closes that
/// gap instead of asserting on a racy read.
fn await_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + std::time::Duration::from_secs(10);
    while Instant::now() < deadline {
        if cond() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    panic!("task_runtime_go_test: timed out waiting for {what}");
}

/// A hung gate is the expected failure mode of a scheduler bug (a lost wake
/// parks a task forever), and `cargo test` has no timeout of its own, so
/// every gate runs under a watchdog that aborts the process with a pointed
/// message instead of wedging a CI box.
struct Watchdog(Arc<AtomicBool>);

impl Watchdog {
    fn arm(secs: u64, what: &'static str) -> Watchdog {
        let done = Arc::new(AtomicBool::new(false));
        let flag = done.clone();
        std::thread::spawn(move || {
            for _ in 0..(secs * 20) {
                std::thread::sleep(std::time::Duration::from_millis(50));
                if flag.load(Ordering::Relaxed) {
                    return;
                }
            }
            eprintln!("TASK RUNTIME WATCHDOG: {what} did not finish in {secs}s -- lost wake / deadlock");
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

// --------------------------------------------------------------------------
// G1 -- deep park: the headline. `helper` is an ordinary Mova fn called from
// the task body; the `<!!` that parks is INSIDE it, several interpreted
// frames below the go block, with native `Interp::call` frames in between.
// That is the asterisk core.async's stackless `go` macro cannot remove
// ("<! used not in (go ...) block"): no CPS transform, no rewriting of the
// callee, and the callee doesn't even know it's in a task.
// --------------------------------------------------------------------------
#[test]
fn g1_deep_park_through_interpreted_frames() {
    let _s = serial_lock();
    let _w = Watchdog::arm(30, "G1");
    let mut e = engine();
    let out = eval(
        &mut e,
        r#"
        (def c (chan))
        (def helper (fn [ch] (<!! ch)))
        (def deeper (fn [ch] (helper ch)))
        (def taker (go (deeper c)))
        (def putter (go (>!! c :handed-over) :sent))
        [(<!! taker) (<!! putter)]
        "#,
    );
    assert_eq!(out.to_string(), "[:handed-over :sent]");
    assert!(runtime::shard_count() >= 1, "G1: at least one shard must exist");
    println!(
        "G1 deep park: ok, shards = {}, resumes = {}",
        runtime::shard_count(),
        runtime::tasks_resumed()
    );
}

// --------------------------------------------------------------------------
// G2 -- scale: 10_000 task PAIRS (20_000 tasks), each pair rendezvousing
// once over its own unbuffered chan, spread across every shard. The sum is
// a checksum that every taker really received its partner's value.
// --------------------------------------------------------------------------
#[test]
fn g2_ten_thousand_task_pairs() {
    let _s = serial_lock();
    let _w = Watchdog::arm(180, "G2");
    let mut e = engine();
    let before = runtime::tasks_finished();
    let t0 = Instant::now();
    let out = eval(
        &mut e,
        r#"
        (def n 10000)
        ;; BOTH halves' result chans are collected and drained: a first cut
        ;; that only waited on the takers passed while a third of the
        ;; putters were still queued, so the checksum has to cover every
        ;; task, not every rendezvous.
        (def dones
          (loop [i 0 acc []]
            (if (< i n)
              (let [c (chan)
                    p (go (>!! c i) 1)
                    d (go (<!! c))]
                (recur (inc i) (conj acc [p d])))
              acc)))
        (loop [i 0 sum 0]
          (if (< i n)
            (let [pair (nth dones i)]
              (recur (inc i) (+ sum (<!! (nth pair 0)) (<!! (nth pair 1)))))
            sum))
        "#,
    );
    let elapsed = t0.elapsed();
    // 10000 putters each returning 1, plus the takers' 0+1+..+9999.
    assert_eq!(out.to_string(), "50005000", "G2: not every task completed");
    assert!(
        runtime::tasks_finished() - before >= 20_000,
        "G2: only {} tasks finished",
        runtime::tasks_finished() - before
    );
    println!(
        "G2 scale: 20000 tasks in {:?} ({:.1} us/task), finished delta = {}, shards = {}",
        elapsed,
        elapsed.as_secs_f64() * 1e6 / 20000.0,
        runtime::tasks_finished() - before,
        runtime::shard_count()
    );
}

// --------------------------------------------------------------------------
// G3 -- latency: 2 tasks, 1 unbuffered chan pair, 100_000 round trips. A
// round trip is 2 rendezvous hops, each hop = 1 park + 1 wake + 2 context
// switches, PLUS the interpreter's own per-iteration cost. Two placements,
// two costs (W4b, `src/runtime/mod.rs` "Spawn placement"):
//
// - SAME-SHARD: ping/pong spawned FROM a parent `go` coordinator -- the
//   natural CSP shape (a task fans out workers and collects their
//   results), and exactly what W4b's family-local affinity co-locates.
//   Each hop is a same-shard scheduler pass, P2's ~194 ns/hop class.
// - CROSS-SHARD: ping/pong spawned from this (plain OS) thread, same as
//   `runtime::spawn`'s root-caller path -- round-robin, so on any real
//   multi-shard machine the two ALWAYS land on different shards (adjacent
//   draws off one `fetch_add` counter) and every hop pays a real
//   `Thread::unpark`/wake round trip, the "4.4 us futex hop" class of cost
//   `Cargo.toml`'s corosensei rationale and
//   `builtins::async::ALTS_PARK_TIMEOUT`'s comment both cite.
//
// Design gate is <= 250 ns/hop (W5) for the same-shard case; the probe's
// own number (through the interpreter) was 194 ns/hop on one OS thread.
// --------------------------------------------------------------------------
#[test]
fn g3_rendezvous_latency() {
    let _s = serial_lock();
    let _w = Watchdog::arm(180, "G3");
    let mut e = engine();
    let rounds: u64 = 100_000;
    eval(
        &mut e,
        r#"
        (def n 100000)
        (def a (chan))
        (def b (chan))
        "#,
    );
    let switches_before = runtime::tasks_resumed();
    let t0 = Instant::now();
    let out = eval(
        &mut e,
        r#"
        ;; Coordinator pattern: ping and pong are spawned FROM this go
        ;; block, not from the top-level (plain-thread) caller -- so W4b
        ;; family-local placement puts them on the coordinator's own
        ;; shard, and the coordinator itself never touches a or b.
        (def coordinator
          (go
            (let [ping (go (loop [i 0] (if (< i n) (do (>!! a i) (<!! b) (recur (inc i))) :ping-done)))
                  pong (go (loop [i 0] (if (< i n) (do (<!! a) (>!! b i) (recur (inc i))) :pong-done)))]
              [(<!! ping) (<!! pong)])))
        (<!! coordinator)
        "#,
    );
    let elapsed = t0.elapsed();
    assert_eq!(out.to_string(), "[:ping-done :pong-done]");
    let hops = rounds * 2;
    let ns_per_hop = elapsed.as_nanos() as f64 / hops as f64;
    let switches = runtime::tasks_resumed() - switches_before;
    println!(
        "G3 latency, same-shard (spawned from a parent go coordinator): {ns_per_hop:.0} ns/hop \
         over {hops} hops in {elapsed:?}; {switches} task resumes ({:.2} per hop).",
        switches as f64 / hops as f64
    );
    // Tightened back down now that placement actually delivers same-shard
    // rendezvous for the coordinator/worker shape: 1000 ns is still ~5x
    // P2's 194 ns/hop headroom (interpreter overhead + this test's own
    // scheduling noise), nowhere near the ~2.3-2.5 us/hop cross-shard class
    // measured below -- so this remains a real regression detector for
    // W4b's placement policy, not a loosened bar.
    assert!(
        ns_per_hop <= 1000.0,
        "G3 same-shard tripwire: {ns_per_hop:.0} ns/hop > 1000 -- W4b placement should have \
         co-located ping/pong on the coordinator's shard (see src/runtime/mod.rs 'Spawn \
         placement'); a regression here means affinity stopped triggering"
    );

    // Deliberately cross-shard: two tasks driving the raw
    // `chan_put`/`chan_take` primitives via `mova::internal::task_chan` and
    // `mova::runtime::spawn` directly, spawned from THIS plain OS thread
    // (not from inside a task) -- W4b's affinity only applies to a
    // task-spawned child, so this is the same round-robin path root spawns
    // always took, isolating the scheduler's park/wake machinery from
    // per-iteration eval cost on top of it.
    let native_rounds: u64 = 100_000;
    let a = task_chan::chan(BufferPolicy::Unbuffered);
    let b = task_chan::chan(BufferPolicy::Unbuffered);
    let done = task_chan::chan(BufferPolicy::Unbuffered);
    let (a1, b1) = (a.clone(), b.clone());
    runtime::spawn(move || {
        for i in 0..native_rounds as i64 {
            task_chan::put_int(&a1, i);
            task_chan::take_int(&b1);
        }
    });
    let (a2, b2, d2) = (a.clone(), b.clone(), done.clone());
    runtime::spawn(move || {
        for i in 0..native_rounds as i64 {
            task_chan::take_int(&a2);
            task_chan::put_int(&b2, i);
        }
        task_chan::put_int(&d2, 1);
    });
    let t0 = Instant::now();
    assert_eq!(task_chan::take_int(&done), Some(1));
    let native = t0.elapsed();
    let native_ns_per_hop = native.as_nanos() as f64 / (native_rounds * 2) as f64;
    println!(
        "G3 latency, cross-shard (spawned from the main thread, no interpreter): \
         {native_ns_per_hop:.0} ns/hop over {} hops in {native:?}",
        native_rounds * 2
    );
    // Kept at W4's 6000 ns -- ~2.5x the ~2.3-2.5 us/hop observed on the
    // reference M4 Pro -- as a real regression detector for the cross-shard
    // wake path itself (unaffected by W4b: root spawns still round-robin).
    // FLAG FOR W5 (unchanged from W4's finding): the landing spec's W5 perf
    // gate (docs/L1-LANDING-SPEC.md) targets <= 250 ns/hop citing P2's 194,
    // which assumes same-shard placement; W4b delivers that for the
    // coordinator/worker shape above, but two UNRELATED roots deliberately
    // rendezvousing (this measurement) still pay the cross-shard cost by
    // design (module doc: "external spawns keep pure round-robin").
    assert!(
        native_ns_per_hop <= 6000.0,
        "G3 cross-shard tripwire: {native_ns_per_hop:.0} ns/hop > 6000 (see the comment above)"
    );
}

// --------------------------------------------------------------------------
// G5 (R3) -- a failing task must not take its shard with it. Two flavors,
// because they are genuinely different mechanisms:
//   (a) a Mova `throw` is an `Err(RjError)`, never a Rust panic: `go*`
//       renders it and closes the result chan, and the task returns
//       normally.
//   (b) a real Rust panic INSIDE a coroutine is caught at that coroutine's
//       root by corosensei and re-thrown at the resume site, so it unwinds
//       the task's own stack, drops what it owned, and surfaces to the
//       shard loop as a catchable payload. Not an abort -- provided the
//       crate is built `-C panic=unwind` (mova's default).
// --------------------------------------------------------------------------
#[test]
fn g5_task_failure_does_not_kill_the_scheduler() {
    let _s = serial_lock();
    let _w = Watchdog::arm(60, "G5");
    let mut e = engine();
    let out = eval(
        &mut e,
        r#"
        (def bad (go (throw (ex-info "task runtime deliberate throw" {}))))
        (def good (go :still-running))
        [(<!! bad) (<!! good)]
        "#,
    );
    assert_eq!(out.to_string(), "[nil :still-running]", "G5a: a throwing task must close its chan, not wedge the peer");

    let panics_before = runtime::tasks_panicked();
    runtime::spawn(|| panic!("task runtime deliberate panic"));
    let (tx, rx) = std::sync::mpsc::channel();
    runtime::spawn(move || {
        let _ = tx.send(42u32);
    });
    let got = rx.recv_timeout(std::time::Duration::from_secs(10)).expect("G5b: the shard died with the panicking task");
    assert_eq!(got, 42);
    // The two tasks above land on different shards and race each other: the
    // sentinel's `send` can complete before the PANICKING task's OWN resume
    // (on its own shard) has finished unwinding and incremented the
    // counter. "The sentinel ran" therefore proves the panic didn't take
    // the whole process down, but not yet that it was counted -- so the
    // counter gets its own bounded wait rather than an immediate assert.
    await_until("the panic to be counted", || runtime::tasks_panicked() - panics_before >= 1);

    // And the interpreter still works afterwards, including a nested go.
    let after = eval(&mut e, r#"(<!! (go (<!! (go :nested))))"#);
    assert_eq!(after.to_string(), ":nested");
    println!(
        "G5: survived; panics = {}, spawned = {}, finished = {}",
        runtime::tasks_panicked(),
        runtime::tasks_spawned(),
        runtime::tasks_finished()
    );
}

// --------------------------------------------------------------------------
// Guard rail, not a gate: `in_task()` must be false on ordinary threads, or
// the task-park arm would fire on a plain `<!!` and suspend a coroutine
// that doesn't exist.
// --------------------------------------------------------------------------
#[test]
fn ordinary_threads_are_not_tasks() {
    assert!(!runtime::in_task());
    let mut e = engine();
    assert_eq!(eval(&mut e, r#"(let [c (chan 1)] (>!! c :x) (<!! c))"#).to_string(), ":x");
    assert!(!runtime::in_task());
}
