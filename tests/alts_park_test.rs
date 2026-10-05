//! Dedicated regression suite for `alts!!`'s park/wake fix (see
//! `src/builtins/async.rs`'s "`alts!!`: scan, then PARK on a doorbell"),
//! the `alts!!`-shaped sibling of FLOW-IDLE-CPU-BUG.md's flow-engine fix
//! and of `tests/flow_wake_test.rs`.
//!
//! `tests/async_test.rs` proves `alts!!`'s BEHAVIOR (return shapes,
//! closed-chan results, fairness, option parsing) is unchanged; this file
//! proves the mechanism underneath it: a blocked `alts!!` PARKS on a
//! `Doorbell` registered on every op chan instead of scan-sleep-scanning at
//! 500µs, and every event shape that can make one of its ops ready actually
//! rings it. There are four such shapes, one test apiece, because a scan
//! reads three mutable fields and one of them is reachable from two places:
//! a put (`buffer` grows), a take that frees buffer room (`buffer`
//! shrinks), a close (`closed`), and a blocking taker PARKING on an
//! unbuffered chan (`waiting_takers` 0 -> 1, which is what
//! `chan_try_put`'s unbuffered gate reads). Plus a fifth test for the one
//! ring site outside `builtins::async` entirely --
//! `builtins::flow`'s `try_take_with_timeout`, which drives chan state
//! directly.
//!
//! ## Two kinds of test, two different failure modes
//!
//! - **Wake tests** (`alts_wakes_within_50ms_of_a_put`,
//!   `two_concurrent_alts_on_one_chan_both_wake`,
//!   `alts_put_op_wakes_when_a_taker_drains_the_chan`,
//!   `alts_put_op_on_an_unbuffered_chan_wakes_when_a_taker_parks`,
//!   `alts_put_op_wakes_when_a_flow_proc_drains_the_chan`) catch a MISSING
//!   ring: a park that nothing wakes still returns -- after
//!   `ALTS_PARK_TIMEOUT`'s 2s safety net -- so a lost ring shows up as
//!   latency, never as a hang. Both of the last two were confirmed to fail
//!   that way before their fix landed (1797ms and 2010ms respectively, i.e.
//!   the backstop, i.e. no wake at all). They CANNOT catch a regression
//!   back to polling: a 500µs spin also passes a 50ms latency bound, with
//!   room to spare.
//! - **`alts_that_stays_blocked_burns_no_cpu`** catches exactly that: it
//!   measures the process's own CPU time (`getrusage(RUSAGE_SELF)`) across
//!   an otherwise completely idle 600ms window in which 24 `alts!!` calls
//!   sit blocked. A/B'd against the pre-fix code (`git stash` on `src/`,
//!   same test): **1ms parked, 275ms polling**. See that test's own doc for
//!   why the window is shaped the way it is.
//!
//! ## Why every test here is serialized
//!
//! `getrusage(RUSAGE_SELF)` is a WHOLE-PROCESS measurement, and `cargo
//! test` runs this file's tests as threads of one process, in parallel by
//! default -- any other test running concurrently would land in the CPU
//! test's window. Every test here therefore holds `test_lock()` for its
//! whole body (the same discipline `tests/flow_wake_test.rs` adopts for its
//! process-global `safety_net_hits()` counter, for the same reason).
//! `Doorbell::safety_net_hits()` is deliberately NOT read here: a parked
//! `alts!!` only ticks it after a full 2s of silence, which no test in this
//! file waits out.

use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use mova::embed::{Engine, Value};

fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(())).lock().unwrap_or_else(|e| e.into_inner())
}

fn engine() -> Engine {
    Engine::builder().build()
}

fn eval_ok(src: &str) -> Value {
    engine()
        .eval_named("alts_park_test", src)
        .unwrap_or_else(|e| panic!("eval error for {src:?}: {}", e.render_plain()))
}

fn ps(src: &str) -> String {
    eval_ok(src).to_string()
}

fn eval_err(src: &str) -> String {
    match engine().eval_named("alts_park_test", src) {
        Ok(v) => panic!("expected an error evaluating {src:?}, got {v:?}"),
        Err(e) => e.to_string(),
    }
}

/// Parses a printed vector of ints, `"[1 1 250]"`, into its elements.
fn ints(printed: &str) -> Vec<i64> {
    printed
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split_whitespace()
        .map(|s| s.parse().unwrap_or_else(|_| panic!("not a vector of ints: {printed:?}")))
        .collect()
}

/// Total (user + system) CPU milliseconds consumed by the WHOLE process so
/// far, per `getrusage(RUSAGE_SELF)`. Monotonic, so a test reads it before
/// and after its window and asserts on the delta -- which is why every test
/// in this file is serialized (module doc).
fn cpu_ms() -> u64 {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    assert_eq!(rc, 0, "getrusage(RUSAGE_SELF) failed");
    let ms = |t: libc::timeval| (t.tv_sec as u64) * 1000 + (t.tv_usec as u64) / 1000;
    ms(ru.ru_utime) + ms(ru.ru_stime)
}

// -------------------- 1. wake latency --------------------

/// A thread blocks in `alts!!` on an empty unbuffered chan; 200ms later
/// another thread `>!!`s it. The `alts!!` must return that value within
/// 50ms of the put -- measured from `time-ms` taken immediately before the
/// put, INSIDE the language, so the bound is on the wake itself and not on
/// how long the harness took to get there.
///
/// Against the pre-fix polling loop this passed too (a 500µs poll is well
/// inside 50ms) -- see the module doc: this test's job is to catch a
/// MISSING ring, which would stretch the latency to `ALTS_PARK_TIMEOUT`'s
/// 2s.
#[test]
fn alts_wakes_within_50ms_of_a_put() {
    let _g = test_lock();
    let out = ps(
        r#"(let [ch (chan)
                 p (thread (do (sleep-ms 200)
                               (let [t (time-ms)] (>!! ch :v) t)))
                 r (alts!! [ch])
                 done (time-ms)
                 put-at (<!! p)]
             [(first r) (= (second r) ch) (- done put-at)])"#,
    );
    // [:v true <latency-ms>]
    let inner = out.trim_start_matches('[').trim_end_matches(']');
    let parts: Vec<&str> = inner.rsplitn(2, ' ').collect();
    let latency: i64 = parts[0].parse().unwrap_or_else(|_| panic!("unexpected result {out:?}"));
    assert_eq!(parts[1], ":v true", "wrong alts!! result in {out:?}");
    assert!(latency < 50, "alts!! took {latency}ms to see the put (want < 50ms), full result {out:?}");
}

// -------------------- 2. the point of the change: no spin --------------------

/// NO-SPIN -- the point of the whole change. A blocked `alts!!` must
/// consume essentially nothing: park, don't poll.
///
/// Shape of the measurement, and why it is shaped that way:
///
/// - **The window contains NOTHING but the blocked selectors.** The engine
///   is built, the waiter threads are spawned, and they are given 150ms to
///   settle into their park, all BEFORE the first `getrusage`. The window
///   itself is a plain `std::thread::sleep` on the test thread -- no eval,
///   no spawn, no `Interp::fork` -- so the delta is the *idle* cost of N
///   blocked `alts!!` calls and nothing else.
/// - **N = 24 waiters, not 1.** One spinning `alts!!` costs only ~10ms of
///   CPU per 500ms here (the pre-fix loop slept 500µs between scans, so it
///   was a 2%-of-a-core drip, not a hot spin) -- too close to measurement
///   noise for a stable bound. 24 of them multiply the signal without
///   changing its nature: pre-fix ~275ms of CPU across the window, parked
///   ~1ms, either side of the 100ms bound by well over 2x.
/// - **CPU time, not context switches.** `ru_nvcsw`/`ru_nivcsw` would be
///   the crisper discriminator (one voluntary switch per `thread::sleep`,
///   so ~2000/s/waiter pre-fix vs ~0 parked), but macOS does not populate
///   those two `rusage` fields -- probed directly: the delta reads 0 both
///   with and without the fix. `ru_utime + ru_stime` is what this platform
///   actually fills in.
///
/// A/B measured (debug build, this machine, `git stash` on `src/`, same
/// test): **1ms with the fix, 275ms without.**
///
/// The tail doubles as the `Vec`-registration stress the single-chan
/// `two_concurrent_alts_on_one_chan_both_wake` only samples: all 24 waiters
/// are registered on ONE chan, and ONE `close!` must wake every one of
/// them.
#[test]
fn alts_that_stays_blocked_burns_no_cpu() {
    let _g = test_lock();
    const WAITERS: usize = 24;
    // Everything expensive happens before the window opens: building the
    // engine loads the whole `core/*.mova` stdlib, and each `thread` forks
    // an `Interp`.
    let mut e = engine();
    let src = format!(
        r#"(do (def hold (chan))
               (def ws (vec (for [_ (range {WAITERS})] (thread (alts!! [hold])))))
               (count ws))"#
    );
    assert_eq!(
        e.eval_named("alts_park_test", &src)
            .unwrap_or_else(|err| panic!("eval error: {}", err.render_plain()))
            .to_string(),
        WAITERS.to_string()
    );
    std::thread::sleep(std::time::Duration::from_millis(150));

    let cpu0 = cpu_ms();
    let t0 = Instant::now();
    std::thread::sleep(std::time::Duration::from_millis(600));
    let window = t0.elapsed();
    let cpu1 = cpu_ms();
    let cpu = cpu1 - cpu0;

    // Every one of the 24 registrations on `hold` must be rung by this
    // single `close!`, and every waiter must report `[nil hold]`.
    let woke = e
        .eval_named(
            "alts_park_test",
            r#"(do (close! hold)
                   (count (vec (for [w ws] (let [r (<!! w)] (assert (nil? (first r))) (assert (= (second r) hold)) r)))))"#,
        )
        .unwrap_or_else(|err| panic!("eval error: {}", err.render_plain()))
        .to_string();
    assert_eq!(woke, WAITERS.to_string(), "not every parked alts!! was woken by the close!");

    assert!(
        cpu < 100,
        "{WAITERS} blocked alts!! calls burned {cpu}ms of CPU over an idle {window:?} window -- \
         they are polling, not parking (measured: 0ms parked, 236ms with the old 500µs spin)"
    );
}

// -------------------- 3. two parked alts on one chan --------------------

/// The registration is a `Vec`, not an `Option`: two threads parked in
/// `alts!!` on the SAME chan must BOTH be woken, one per put. (With an
/// `Option`-shaped slot the second registration would clobber the first and
/// one of these two would sit out the full 2s safety net -- the 3s deref
/// bound below would still return, with `:timeout` in place of a value.)
#[test]
fn two_concurrent_alts_on_one_chan_both_wake() {
    let _g = test_lock();
    assert_eq!(
        ps(
            r#"(let [ch (chan 2)
                     a (thread (first (alts!! [ch])))
                     b (thread (first (alts!! [ch])))
                     _ (sleep-ms 100)
                     _ (>!! ch :x)
                     _ (>!! ch :y)
                     ra (<!! a)
                     rb (<!! b)]
                 (vec (sort [(str ra) (str rb)])))"#
        ),
        r#"[":x" ":y"]"#
    );
}

// -------------------- 4. an already-closed chan never parks --------------------

/// A closed chan's take op is immediately ready (`chan_try_take` reports
/// `Closed`, not `WouldBlock`), so this must resolve to `[nil that-chan]`
/// on the FIRST scan -- before any doorbell park. Asserted as a wall-clock
/// bound too: a park here would show up as 2s.
#[test]
fn alts_on_a_closed_chan_returns_immediately() {
    let _g = test_lock();
    let t0 = Instant::now();
    assert_eq!(
        ps(r#"(let [c (chan) _ (close! c) r (alts!! [c])] [(nil? (first r)) (= (second r) c)])"#),
        "[true true]"
    );
    assert!(t0.elapsed().as_millis() < 500, "an already-closed chan's op parked ({:?})", t0.elapsed());
}

// -------------------- 5. `:default` is untouched --------------------

/// The `:default` path is the one branch that never registers a doorbell
/// and never parks: one scan, then the default, exactly as before.
#[test]
fn default_still_fires_immediately_when_nothing_is_ready() {
    let _g = test_lock();
    let t0 = Instant::now();
    assert_eq!(ps(r#"(let [c (chan)] (alts!! [c] :default :none))"#), "[:none :default]");
    assert!(t0.elapsed().as_millis() < 500, ":default parked ({:?})", t0.elapsed());
}

// -------------------- 6. put-op wake --------------------

/// The put side parks too, and it is `chan_take`'s ring (not
/// `chan_put`'s/`chan_close`'s) that has to wake it: an `alts!!` whose only
/// op is a put onto a FULL `(chan 1)` blocks, and the event that makes it
/// ready is a TAKER draining that chan -- i.e. it is `chan_take`'s
/// buffer-pop ring, not `chan_put`'s or `chan_close`'s, that has to carry
/// this wake.
#[test]
fn alts_put_op_wakes_when_a_taker_drains_the_chan() {
    let _g = test_lock();
    let out = ps(
        r#"(let [ch (chan 1)
                 _ (>!! ch :fill)
                 a (thread (alts!! [[ch :v]]))
                 _ (sleep-ms 100)
                 t (time-ms)
                 drained (<!! ch)
                 r (<!! a)
                 latency (- (time-ms) t)]
             [drained (first r) (= (second r) ch) (< latency 100) latency])"#,
    );
    assert!(
        out.starts_with("[:fill true true true "),
        "put-op alts!! did not complete promptly after the drain: {out:?}"
    );
}

// -------------------- 11. the waiting_takers ring --------------------

/// A parked `alts!!` scan reads THREE mutable fields, not two: `buffer`,
/// `closed`, and -- through `chan_try_put`'s unbuffered gate,
/// `buffer.is_empty() && waiting_takers > 0` -- `waiting_takers`. So a
/// put-op `alts!!` on an UNBUFFERED chan becomes ready at exactly one
/// instant: `chan_take`'s `waiting_takers += 1`, when a blocking `<!!`
/// gives up on finding a value and parks. Nothing else about the chan
/// changes at that moment; if that increment doesn't ring, the selector
/// sleeps through the only event it was waiting for.
///
/// Measured against the version of this fix that rang only on
/// `buffer`/`closed` changes: the rendezvous completed after **1800ms**,
/// i.e. not at all -- it was the `ALTS_PARK_TIMEOUT` backstop's re-scan
/// that eventually noticed, not a wake. With the increment ringing: ~0ms.
#[test]
fn alts_put_op_on_an_unbuffered_chan_wakes_when_a_taker_parks() {
    let _g = test_lock();
    let out = ps(
        r#"(let [ch (chan)
                 a (thread (let [r (alts!! [[ch :v]])]
                             [(if (= true (first r)) 1 0) (if (= (second r) ch) 1 0)]))
                 _ (sleep-ms 200)
                 t (time-ms)
                 got (<!! ch)
                 latency (- (time-ms) t)
                 r (<!! a)]
             [(first r) (second r) (if (= got :v) 1 0) latency])"#,
    );
    let v = ints(&out);
    assert_eq!(&v[..3], &[1, 1, 1], "the unbuffered rendezvous did not complete correctly: {out:?}");
    assert!(
        v[3] < 50,
        "the taker waited {}ms for a put-op alts!! that was already parked -- the \
         waiting_takers 0->1 transition is not ringing ({out:?})",
        v[3]
    );
}

// -------------------- 12. flow.rs is a ring site too --------------------

/// `builtins::flow`'s `try_take_with_timeout` mutates chan state directly
/// -- its own `buffer.pop_front()`, its own `waiting_takers += 1` -- rather
/// than going through `chan_take`, so it is a SIXTH ring site that
/// `builtins::async`'s five don't cover. This is the end-to-end proof that
/// it discharges the obligation: a real flow proc, reading a `(chan 1)`
/// handed to it as an `::flow/in-ports` input (the single-input park path,
/// which is the one that lands in `try_take_with_timeout`), drains the one
/// buffered message and must thereby wake an `alts!!` put op parked on
/// that same chan for want of buffer room.
///
/// The choreography, all timed off `flow/resume`: the proc's `transform`
/// sleeps 400ms, which is the window in which the chan can be refilled and
/// a selector parked on it while the proc is provably not reading. `:a`
/// goes in at 150ms (proc pops it, enters `transform`), `:b` at 300ms (the
/// chan is now full again), the put-op `alts!!` parks at ~300ms, and the
/// proc's next read -- at ~550ms, when `transform` returns -- frees the
/// room.
///
/// A/B measured (`git stash` on `src/builtins/flow.rs`): **250ms with the
/// ring, 2005ms without** -- the latter being `ALTS_PARK_TIMEOUT`'s 2s
/// backstop firing, which is what "no wake at all" looks like from here.
#[test]
fn alts_put_op_wakes_when_a_flow_proc_drains_the_chan() {
    let _g = test_lock();
    let out = ps(
        r#"(let [ext (chan 1)
                 out (chan 10)
                 step (flow/map->step
                        {:describe (fn [] {:ins {} :outs {:out {}}})
                         :init (fn [_] {:clojure.core.async.flow/in-ports {:in ext}
                                        :clojure.core.async.flow/out-ports {:out out}})
                         :transform (fn [s _ m] (sleep-ms 400) [s {:out [m]}])})
                 fl (flow/create-flow {:procs {:p {:proc (flow/process step)}} :conns []})
                 _ (flow/start fl)
                 _ (flow/resume fl)
                 _ (sleep-ms 150)
                 _ (>!! ext :a)
                 _ (sleep-ms 150)
                 _ (>!! ext :b)
                 a (thread (let [t0 (time-ms) r (alts!! [[ext :c]])]
                             [(if (= true (first r)) 1 0)
                              (if (= (second r) ext) 1 0)
                              (- (time-ms) t0)]))
                 _ (sleep-ms 100)
                 r (<!! a)
                 _ (flow/stop fl)]
             r)"#,
    );
    let v = ints(&out);
    assert_eq!(&v[..2], &[1, 1], "the parked put op did not complete on the chan it named: {out:?}");
    assert!(
        (100..900).contains(&v[2]),
        "the put-op alts!! took {}ms to notice the flow proc's drain -- want ~250ms; \
         anything near 2000ms is ALTS_PARK_TIMEOUT's backstop, i.e. no wake at all ({out:?})",
        v[2]
    );
}

// -------------------- 13. an empty ops vector is an error --------------------

/// `(alts!! [])` has nothing to scan and nothing to register a doorbell on,
/// so no event could ever ring it: it would park until
/// `ALTS_PARK_TIMEOUT`, re-scan zero ops, and park again, forever. Real
/// core.async asserts `(pos? (count ports))`; so does this, before any
/// registration happens. With or without `:default` -- "empty is fine as
/// long as you also pass :default" is not a rule worth having.
#[test]
fn empty_ops_vector_is_rejected() {
    let _g = test_lock();
    let t0 = Instant::now();
    for src in ["(alts!! [])", "(alts!! [] :default :none)"] {
        let err = eval_err(src);
        assert!(
            err.contains("at least one op"),
            "wrong error for {src:?}: {err:?}"
        );
    }
    assert!(t0.elapsed().as_millis() < 500, "(alts!! []) parked instead of erroring");
}
