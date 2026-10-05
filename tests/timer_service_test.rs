//! Regression suite for `timeout`'s shared timer service (see
//! `src/builtins/async.rs`'s "`timeout`: one shared timer thread" and
//! `TIMER`'s own doc).
//!
//! `timeout` used to spawn one detached OS thread per call, whose entire
//! job was to sleep and then close one channel. It now arms an entry on a
//! deadline-ordered heap serviced by ONE process-wide `"mova-timer"`
//! thread. Two things therefore need proving, and they pull in opposite
//! directions: that the thread economy is real (`timeouts_share_one_timer_
//! thread`, which is why `mova::internal::async_timer::threads_spawned`
//! exists at all), and that nothing about the OBSERVABLE contract moved --
//! a `timeout` chan is still never put to, still closes ~`ms` later, and
//! several of them still fire in DEADLINE order regardless of arming order
//! (`timeouts_fire_in_deadline_order_not_arming_order`, the direct test of
//! the heap's reversed `Ord`: a max-heap here would make all three fire
//! together at the LATEST deadline).
//!
//! `alts_over_a_timeout_parks_and_fires_on_time` is the integration point
//! with the `alts!!` park fix (`tests/alts_park_test.rs`) -- 24 selectors
//! parked on one shared `timeout`, all woken by the timer thread's single
//! `chan_close`, with the same `getrusage`-based no-spin bound.
//!
//! Every test here holds `test_lock()`: `getrusage(RUSAGE_SELF)` is a
//! whole-process measurement and `cargo test` would otherwise run these as
//! concurrent threads of one process, dropping each other's work into the
//! measured window (the discipline `tests/flow_wake_test.rs` and
//! `tests/alts_park_test.rs` both follow, for the same reason).

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
        .eval_named("timer_service_test", src)
        .unwrap_or_else(|e| panic!("eval error for {src:?}: {}", e.render_plain()))
}

fn ps(src: &str) -> String {
    eval_ok(src).to_string()
}

/// Total (user + system) CPU milliseconds the WHOLE process has consumed,
/// per `getrusage(RUSAGE_SELF)` -- see `tests/alts_park_test.rs`'s copy for
/// why this, and not `ru_nvcsw`, is the metric on this platform.
fn cpu_ms() -> u64 {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    assert_eq!(rc, 0, "getrusage(RUSAGE_SELF) failed");
    let ms = |t: libc::timeval| (t.tv_sec as u64) * 1000 + (t.tv_usec as u64) / 1000;
    ms(ru.ru_utime) + ms(ru.ru_stime)
}

/// Parses a printed vector of ints, `"[12 340]"`, into its elements.
fn ints(printed: &str) -> Vec<i64> {
    printed
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split_whitespace()
        .map(|s| s.parse().unwrap_or_else(|_| panic!("not a vector of ints: {printed:?}")))
        .collect()
}

// -------------------- 7. the contract is unchanged --------------------

/// A `timeout` chan still yields `nil` (never a value) when its deadline
/// passes, and it does so no earlier than `ms` and not much later.
#[test]
fn timeout_closes_at_its_deadline() {
    let _g = test_lock();
    let out = ps(
        r#"(let [t0 (time-ms)
                 ch (timeout 120)
                 v (<!! ch)
                 elapsed (- (time-ms) t0)]
             [(if (nil? v) 1 0) elapsed])"#,
    );
    let v = ints(&out);
    assert_eq!(v[0], 1, "a timeout chan handed out a value instead of closing: {out:?}");
    assert!(
        (120..=320).contains(&v[1]),
        "a (timeout 120) closed after {}ms, want 120..320 ({out:?})",
        v[1]
    );
}

// -------------------- 8. thread economy --------------------

/// The point of the change. 500 `timeout`s armed back to back must produce
/// exactly ONE timer thread for the process (500 before), and all 500 must
/// still fire on their own deadlines -- spot-checked at both ends of the
/// heap, the earliest (50ms) and the latest (549ms).
///
/// The counter is a process global read through
/// `mova::internal::async_timer::threads_spawned`; `cargo test` gives
/// this file its own process, and the count is 1 from the first `timeout`
/// onward regardless of which test in the file ran first, so no ordering
/// assumption is baked in here.
#[test]
fn timeouts_share_one_timer_thread() {
    let _g = test_lock();
    let out = ps(
        r#"(let [t0 (time-ms)
                 ts (vec (for [i (range 500)] (timeout (+ 50 i))))
                 _ (<!! (nth ts 0))
                 first-at (- (time-ms) t0)
                 _ (<!! (nth ts 499))
                 last-at (- (time-ms) t0)]
             [(count ts) first-at last-at])"#,
    );
    let v = ints(&out);
    assert_eq!(v[0], 500, "wrong number of timeout chans: {out:?}");
    assert_eq!(
        mova::internal::async_timer::threads_spawned(),
        1,
        "500 timeouts should share ONE timer thread"
    );
    assert!(
        (50..=450).contains(&v[1]),
        "the earliest timeout (50ms) closed after {}ms ({out:?})",
        v[1]
    );
    assert!(
        (549..=1100).contains(&v[2]),
        "the latest timeout (549ms) closed after {}ms ({out:?})",
        v[2]
    );
}

// -------------------- 9. deadline order, not arming order --------------------

/// Armed 150ms, 50ms, 100ms -- in that order -- they must fire 50, 100,
/// 150. Each take blocks until its own chan closes, so the three elapsed
/// times pin each firing to its own deadline window; a heap ordered the
/// wrong way round would leave all three sitting until 150ms and blow the
/// first bound.
#[test]
fn timeouts_fire_in_deadline_order_not_arming_order() {
    let _g = test_lock();
    let out = ps(
        r#"(let [t0 (time-ms)
                 late (timeout 150)
                 early (timeout 50)
                 mid (timeout 100)
                 _ (<!! early)
                 at-early (- (time-ms) t0)
                 _ (<!! mid)
                 at-mid (- (time-ms) t0)
                 _ (<!! late)
                 at-late (- (time-ms) t0)]
             [at-early at-mid at-late])"#,
    );
    let v = ints(&out);
    assert!((50..120).contains(&v[0]), "the 50ms timeout fired at {}ms ({out:?})", v[0]);
    assert!((100..220).contains(&v[1]), "the 100ms timeout fired at {}ms ({out:?})", v[1]);
    assert!((150..320).contains(&v[2]), "the 150ms timeout fired at {}ms ({out:?})", v[2]);
    assert!(v[0] <= v[1] && v[1] <= v[2], "timeouts fired out of deadline order: {out:?}");
}

// -------------------- 10. integration with the alts!! park fix --------------------

/// Both changes at once: 24 `alts!!` calls, each selecting over a private
/// chan nobody ever touches plus ONE shared `(timeout 800)`, must all
/// return `[nil that-timeout-chan]` at ~800ms -- woken by the single timer
/// thread's single `chan_close` ringing all 24 `alts_doorbells`
/// registrations -- and must burn no CPU while they wait.
///
/// The measured window (500ms) sits entirely inside the wait: it opens
/// 150ms after the selectors are spawned, so no engine build, no
/// `Interp::fork`, and no thread spawn lands in it, and it closes ~150ms
/// before the timeout fires. Same A/B as
/// `alts_park_test::alts_that_stays_blocked_burns_no_cpu`: ~1ms parked
/// against ~275ms polling.
#[test]
fn alts_over_a_timeout_parks_and_fires_on_time() {
    let _g = test_lock();
    let mut e = engine();
    let started = e
        .eval_named(
            "timer_service_test",
            r#"(do (def t0 (time-ms))
                  (def t (timeout 800))
                  (def ws (vec (for [_ (range 24)]
                                 (thread (let [r (alts!! [(chan) t])]
                                           [(if (nil? (first r)) 1 0)
                                            (if (= (second r) t) 1 0)
                                            (- (time-ms) t0)])))))
                  (count ws))"#,
        )
        .unwrap_or_else(|err| panic!("eval error: {}", err.render_plain()))
        .to_string();
    assert_eq!(started, "24");
    std::thread::sleep(std::time::Duration::from_millis(150));

    let cpu0 = cpu_ms();
    let t0 = Instant::now();
    std::thread::sleep(std::time::Duration::from_millis(500));
    let window = t0.elapsed();
    let cpu = cpu_ms() - cpu0;

    let out = e
        .eval_named(
            "timer_service_test",
            r#"(let [rs (vec (for [w ws] (<!! w)))]
                 [(count rs)
                  (count (filter (fn [r] (= 1 (first r))) rs))
                  (count (filter (fn [r] (= 1 (second r))) rs))
                  (count (filter (fn [r] (and (>= (nth r 2) 800) (< (nth r 2) 1400))) rs))])"#,
        )
        .unwrap_or_else(|err| panic!("eval error: {}", err.render_plain()))
        .to_string();
    assert_eq!(
        ints(&out),
        vec![24, 24, 24, 24],
        "[total nil-first timeout-chan-second on-time] -- every parked alts!! should have \
         returned [nil t] at ~800ms, got {out:?}"
    );
    assert_eq!(mova::internal::async_timer::threads_spawned(), 1, "more than one timer thread");
    assert!(
        cpu < 100,
        "24 alts!! parked on a shared timeout burned {cpu}ms of CPU over an idle {window:?} \
         window -- they are polling, not parking"
    );
}

// -------------------- 11. the second entry kind (L3.5 item 1) --------------------

/// The heap grew a second `TimerAction`: wake a parked TASK at its deadline,
/// instead of closing a chan (`builtins::async::timer_arm_waker`, armed by
/// `builtins::conc`'s timeout-`deref` arms). This file owns the claim that
/// it is the SAME heap and the SAME thread -- one service, two entry kinds,
/// still ordered by deadline across both.
///
/// Armed: `(timeout 60)`, a `(deref p 200 :timed-out)` inside a `go` on a
/// promise nothing delivers, and `(timeout 400)`. They must come back 60,
/// 200, 400 -- the waker entry threaded into the middle of two chan entries
/// -- with the thread count still 1.
#[test]
fn a_task_deadline_shares_the_heap_and_thread_with_timeout_entries() {
    let _g = test_lock();
    let out = ps(
        r#"(let [t0 (time-ms)
                 p (promise)
                 res (chan 1)
                 early (timeout 60)
                 late (timeout 400)
                 _ (go (>! res (deref p 200 :timed-out)))
                 _ (<!! early)
                 at-early (- (time-ms) t0)
                 r (<!! res)
                 at-deref (- (time-ms) t0)
                 _ (<!! late)
                 at-late (- (time-ms) t0)]
             [at-early at-deref at-late (if (= r :timed-out) 1 0)])"#,
    );
    let v = ints(&out);
    assert_eq!(v[3], 1, "the deref should have timed out -- nothing delivers p ({out:?})");
    assert!((60..200).contains(&v[0]), "the 60ms timeout fired at {}ms ({out:?})", v[0]);
    assert!(
        (200..400).contains(&v[1]),
        "the 200ms task deadline fired at {}ms -- it must land between the two `timeout` \
         entries, not with one of them ({out:?})",
        v[1]
    );
    assert!((400..900).contains(&v[2]), "the 400ms timeout fired at {}ms ({out:?})", v[2]);
    assert_eq!(mova::internal::async_timer::threads_spawned(), 1, "more than one timer thread");
}

/// **The no-spin gate for L3.5 item 1**, in the file that owns the
/// `getrusage` harness. 500 tasks sitting in `(deref p 60000 :x)` on a
/// promise nobody delivers must burn essentially NO CPU while they wait.
///
/// The W2b arm they replace was a 1ms poll: each waiter re-armed the shared
/// heap ~1000 times a second, and `tests/l35_deref_park_probe.rs` measured
/// 3.99 CPU cores at N=1000 -- so 500 waiters over this 500ms window cost
/// on the order of 1000ms of CPU before the change, against a floor of ~0
/// after it. The bar is set at 250ms: a fifth of the old cost, several times
/// any plausible scheduling noise, and nowhere near tight enough to depend
/// on how busy the box is. (`getrusage(RUSAGE_SELF)` is THIS process only,
/// so a neighbouring build does not land in the window; other tests in this
/// binary do, which is what `test_lock` is for.)
///
/// The window opens 250ms after the spawn so that no engine build, no
/// `Interp::fork` and no task spawn is inside it, and closes long before the
/// 60s deadlines.
#[test]
fn five_hundred_parked_timeout_derefs_burn_no_cpu() {
    let _g = test_lock();
    let mut e = engine();
    let started = e
        .eval_named(
            "timer_service_test",
            r#"(do (def p (promise))
                  (def done (chan 512))
                  (dotimes [_ 500] (go (>! done (deref p 60000 :timed-out))))
                  :spawned)"#,
        )
        .unwrap_or_else(|err| panic!("eval error: {}", err.render_plain()))
        .to_string();
    assert_eq!(started, ":spawned");
    std::thread::sleep(std::time::Duration::from_millis(250));

    let cpu0 = cpu_ms();
    let t0 = Instant::now();
    std::thread::sleep(std::time::Duration::from_millis(500));
    let window = t0.elapsed();
    let cpu = cpu_ms() - cpu0;

    // They really were still parked at the end of the window, and they all
    // wake from the ONE resolution -- not from their deadlines, 60s out.
    let out = e
        .eval_named(
            "timer_service_test",
            r#"(let [_ (deliver p :delivered)
                     rs (loop [n 0 acc []]
                          (if (= n 500) acc (recur (inc n) (conj acc (<!! done)))))]
                 [(count rs) (count (filter (fn [r] (= r :delivered)) rs))])"#,
        )
        .unwrap_or_else(|err| panic!("eval error: {}", err.render_plain()))
        .to_string();
    assert_eq!(
        ints(&out),
        vec![500, 500],
        "[woken delivered] -- all 500 parked derefs should report the value, got {out:?}"
    );
    assert!(
        cpu < 250,
        "500 tasks parked in a timeout-`deref` burned {cpu}ms of CPU over an idle {window:?} \
         window -- the deref timeout arm is POLLING again (L3.5 item 1)"
    );
}
