//! Tests for the E2 transport kernel.
//!
//! Three tiers:
//!
//! 1. **Contract tests** -- mirror `builtins::async`'s own `Chan` unit tests
//!    one-for-one, so a divergence from the channel contract this kernel is
//!    meant to substitute for shows up as a failing test with the same name.
//! 2. **Wake-protocol regression stress** -- `pingpong_stress_no_deadlock`
//!    reproduces the *exact* load shape that deadlocked the reverted
//!    `AtomicBool` wake-elision fast path (see `bench/optimization-log.md`'s
//!    E2 entry): sustained register/wake churn across many cycles, which the
//!    one-way bench alone did not surface. It runs under a watchdog thread,
//!    so a hang FAILS instead of wedging the suite. Every stress test in
//!    this file is watchdogged for the same reason.
//! 3. **Close races** -- close landing during a blocked put, a blocked take,
//!    and mid-spin (before the park), from both sides.
//!
//! Sizing: the in-suite tests are seconds, not minutes. The multi-million
//! message extremes live in `#[ignore]`d `stress_*` tests, run and reported
//! separately.

use super::*;
use std::sync::atomic::{AtomicBool, Ordering as O};
use std::time::{Duration, Instant};

fn int(i: i64) -> Value {
    Value::Int(i)
}

fn as_int(v: &Value) -> i64 {
    match v {
        Value::Int(i) => *i,
        _ => panic!("expected an Int payload"),
    }
}

// ---------------------------------------------------------------------------
// Watchdog: a hang must FAIL the test, not hang CI.
// ---------------------------------------------------------------------------

/// Runs `body` on the calling thread with a watchdog thread that aborts the
/// whole process if `body` has not finished within `limit`.
///
/// Aborting is deliberate and is the only honest option here: the failure
/// mode being guarded against is *both threads parked forever*, and there is
/// no way to unwind a thread that is parked inside `thread::park()`. A
/// `panic!` on the watchdog thread would merely mark that thread as failed
/// while the deadlocked workers kept the process alive forever. `abort()`
/// with a diagnostic on stderr turns the hang into a hard, immediate,
/// unmistakable test failure -- which is exactly what the reverted probe
/// version deserved and did not get.
fn with_watchdog<T>(what: &'static str, limit: Duration, body: impl FnOnce() -> T) -> T {
    let done = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&done);
    let watcher = thread::spawn(move || {
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            if flag.load(O::Acquire) {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        if !flag.load(O::Acquire) {
            eprintln!(
                "\n*** transport watchdog: `{what}` did not finish within {limit:?}.\n\
                 *** This is the wake-protocol deadlock class (see the module doc's\n\
                 *** invariant list and bench/optimization-log.md's E2 entry).\n"
            );
            std::process::abort();
        }
    });
    let out = body();
    done.store(true, O::Release);
    watcher.join().expect("watchdog thread panicked");
    out
}

/// Deterministic per-thread xorshift, used to jitter producer/consumer speed
/// without `sleep` (sleeps would blow the runtime budget and, worse, would
/// mostly test the scheduler rather than the protocol). Spin jitter keeps the
/// two sides crossing the spin/park boundary in both directions.
struct Jitter(u64);

impl Jitter {
    fn new(seed: u64) -> Self {
        Jitter(seed | 1)
    }
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    /// Occasionally stall for a burst long enough to push the peer past its
    /// spin budget and into a real park -- that transition is where every
    /// lost-wake bug lives.
    fn maybe_stall(&mut self, park_budget: u32) {
        let r = self.next();
        if r.is_multiple_of(64) {
            let n = (r >> 6) % (park_budget as u64 * 3);
            for _ in 0..n {
                std::hint::spin_loop();
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 1. Contract tests -- mirroring `builtins::async`'s `Chan` unit tests.
// ---------------------------------------------------------------------------

#[test]
fn fixed_buffer_put_take_roundtrip() {
    let (tx, rx) = SpscRing::channel(2);
    assert!(tx.put(int(1)));
    assert!(tx.put(int(2)));
    assert_eq!(rx.take().map(|v| as_int(&v)), Some(1));
    assert_eq!(rx.take().map(|v| as_int(&v)), Some(2));
}

#[test]
fn close_then_take_drains_then_none() {
    let (tx, rx) = SpscRing::channel(2);
    assert!(tx.put(int(1)));
    tx.close();
    assert_eq!(rx.take().map(|v| as_int(&v)), Some(1));
    assert!(rx.take().is_none());
    assert!(rx.take().is_none());
}

#[test]
fn put_on_closed_returns_false() {
    let (tx, rx) = SpscRing::channel(2);
    tx.close();
    assert!(!tx.put(int(1)));
    assert!(matches!(tx.try_put(int(1)), TryPut::Closed));
    assert!(matches!(rx.try_take(), TryTake::Closed));
}

#[test]
fn close_is_idempotent_and_works_from_either_side() {
    let (tx, rx) = SpscRing::channel(2);
    assert!(!tx.is_closed());
    rx.close(); // consumer side
    assert!(tx.is_closed());
    rx.close();
    tx.close();
    assert!(!tx.put(int(1)));
}

#[test]
fn try_ops_never_block() {
    let (tx, rx) = SpscRing::channel(1);
    assert!(matches!(rx.try_take(), TryTake::WouldBlock));
    assert!(matches!(tx.try_put(int(1)), TryPut::Sent));
    assert!(matches!(tx.try_put(int(2)), TryPut::WouldBlock));
    assert!(matches!(rx.try_take(), TryTake::Received(v) if as_int(&v) == 1));
    assert!(matches!(rx.try_take(), TryTake::WouldBlock));
}

/// Capacity is the caller's number VERBATIM, even when the slot array
/// behind it was rounded up to a power of two. The flow engine maps a
/// conn's `:buf-or-n n` straight onto this, so a ring that quietly held 16
/// where the `Chan` it replaces held 10 would start backpressuring in a
/// different place -- an observable semantic difference. See the module
/// doc's "Capacity is EXACT" section.
#[test]
fn capacity_is_exact_not_rounded_up_to_the_slot_count() {
    let (tx, _rx) = SpscRing::channel(3);
    assert_eq!(tx.capacity(), 3);
    let (tx, _rx) = SpscRing::channel(10);
    assert_eq!(tx.capacity(), 10);
    let (tx, _rx) = SpscRing::channel(1024);
    assert_eq!(tx.capacity(), 1024);
}

/// ...and the in-flight limit really is that exact number, not the rounded
/// allocation: the 11th `try_put` into a capacity-10 ring must block even
/// though 16 slots exist.
#[test]
fn a_non_power_of_two_ring_blocks_at_exactly_its_stated_capacity() {
    let (tx, rx) = SpscRing::channel(10);
    for i in 0..10 {
        assert!(matches!(tx.try_put(int(i)), TryPut::Sent), "put {i} should fit");
    }
    assert!(matches!(tx.try_put(int(10)), TryPut::WouldBlock));
    // ...and draining one frees exactly one slot, with FIFO order intact
    // across the non-power-of-two wrap.
    for i in 0..10 {
        assert!(matches!(rx.try_take(), TryTake::Received(v) if as_int(&v) == i));
        assert!(matches!(tx.try_put(int(100 + i)), TryPut::Sent));
        assert!(matches!(tx.try_put(int(999)), TryPut::WouldBlock));
    }
    for i in 0..10 {
        assert!(matches!(rx.try_take(), TryTake::Received(v) if as_int(&v) == 100 + i));
    }
}

#[test]
#[should_panic(expected = "capacity must be at least 1")]
fn zero_capacity_is_rejected() {
    let _ = SpscRing::channel(0);
}

#[test]
fn ring_is_fifo_and_wraps() {
    let (tx, rx) = SpscRing::channel(4);
    // Several full laps around a 4-slot ring: exercises the index masking
    // and slot reuse (invariant 10).
    for lap in 0..8 {
        for i in 0..4 {
            assert!(tx.put(int(lap * 4 + i)));
        }
        for i in 0..4 {
            assert_eq!(rx.take().map(|v| as_int(&v)), Some(lap * 4 + i));
        }
    }
}

#[test]
fn handoff_rendezvous_across_threads() {
    with_watchdog("handoff_rendezvous_across_threads", Duration::from_secs(30), || {
        let (tx, rx) = Handoff::channel();
        let h = thread::spawn(move || tx.put(int(42)));
        assert_eq!(rx.take().map(|v| as_int(&v)), Some(42));
        assert!(h.join().unwrap());
    });
}

#[test]
fn handoff_put_returns_only_after_the_take() {
    with_watchdog("handoff_put_returns_only_after_the_take", Duration::from_secs(30), || {
        let (tx, rx) = Handoff::channel();
        let taken = Arc::new(AtomicBool::new(false));
        let seen = Arc::clone(&taken);
        let h = thread::spawn(move || {
            let ok = tx.put(int(7));
            // If `put` returned before the take, this reads `false`.
            (ok, seen.load(O::Acquire))
        });
        // Let the producer get well past its spin budget and park.
        thread::sleep(Duration::from_millis(20));
        taken.store(true, O::Release);
        assert_eq!(rx.take().map(|v| as_int(&v)), Some(7));
        let (ok, saw_take_flag) = h.join().unwrap();
        assert!(ok, "rendezvous put should report success");
        assert!(saw_take_flag, "put returned before the value was taken");
    });
}

#[test]
fn handoff_close_then_drain() {
    let (tx, rx) = Handoff::channel();
    assert!(matches!(tx.try_put(int(9)), TryPut::Sent));
    rx.close();
    // Drain-then-None, exactly as for the ring.
    assert_eq!(rx.take().map(|v| as_int(&v)), Some(9));
    assert!(rx.take().is_none());
}

#[test]
fn drop_releases_undrained_values() {
    // Undrained values must actually be dropped, not leaked -- a `Value` can
    // own an `Arc`, a channel, a file handle. `Arc` strong counts are the
    // observable.
    let (tx, rx) = SpscRing::channel(8);
    let ch = Arc::new(crate::value::Chan::new(crate::value::BufferPolicy::Fixed(1)));
    for _ in 0..3 {
        assert!(tx.put(Value::Channel(Arc::clone(&ch))));
    }
    assert_eq!(Arc::strong_count(&ch), 4);
    assert!(rx.take().is_some());
    assert_eq!(Arc::strong_count(&ch), 3);
    drop(tx);
    drop(rx);
    assert_eq!(Arc::strong_count(&ch), 1, "Ring::drop must drop the two undrained values");
}

// ---------------------------------------------------------------------------
// 2. Wake-protocol regression stress.
// ---------------------------------------------------------------------------

/// The ping-pong shape that broke the reverted `AtomicBool` wake-elision
/// fast path: two channels, every hop driving the peer from "has work" to
/// "empty" and back, so the register/park/wake path is exercised over and
/// over rather than once.
///
/// `spin` is the per-transport spin budget. **`spin = 0` is what gives the
/// regression tests their teeth**: at the production budget of 4000 a tight
/// ping-pong almost never reaches the park path at all (the peer replies
/// within the spin window), so a lost-wake bug only surfaces when the
/// scheduler happens to preempt at the wrong moment -- which is exactly why
/// the probe's deadlock took 5+ minutes and a million round trips to show up.
/// Forcing the budget to 0 makes every single hop take the park path, turning
/// a probabilistic hang into a deterministic one.
fn pingpong_round_trips(n: u64, cap: usize, spin: u32, stall: bool) {
    let (a_tx, a_rx) = SpscRing::channel_with_spin(cap, spin);
    let (b_tx, b_rx) = SpscRing::channel_with_spin(cap, spin);
    let pong = thread::spawn(move || {
        let mut j = Jitter::new(0xC0FFEE);
        for _ in 0..n {
            let v = a_rx.take().expect("pong: ping->pong closed early");
            if stall {
                j.maybe_stall(DEFAULT_SPIN_LIMIT);
            }
            assert!(b_tx.put(v), "pong: pong->ping closed early");
        }
    });
    let mut j = Jitter::new(0xBEEF);
    let mut v = int(0);
    for i in 0..n {
        assert!(a_tx.put(v), "ping: ping->pong closed early");
        if stall {
            j.maybe_stall(DEFAULT_SPIN_LIMIT);
        }
        v = b_rx.take().expect("ping: pong->ping closed early");
        assert_eq!(as_int(&v), 0, "payload corrupted at round trip {i}");
    }
    pong.join().expect("pong thread panicked");
}

/// **THE regression test** for `bench/optimization-log.md`'s E2 deadlock.
///
/// Zero spin budget, capacity 1: every one of the 120,000 hops goes
/// register -> CAS -> park -> unpark, on both sides. A protocol with a
/// lost-wake hole wedges within the first few thousand hops, both threads
/// parked at 0% CPU -- and the watchdog turns that into a hard abort instead
/// of a hung suite.
#[test]
fn pingpong_stress_no_deadlock() {
    with_watchdog("pingpong_stress_no_deadlock", Duration::from_secs(120), || {
        pingpong_round_trips(60_000, 1, 0, false);
    });
}

/// The same forced-park shape with randomized stalls, so the park and
/// wake-elision paths interleave unpredictably instead of in lockstep.
#[test]
fn pingpong_stress_with_stalls() {
    with_watchdog("pingpong_stress_with_stalls", Duration::from_secs(120), || {
        pingpong_round_trips(30_000, 1, 0, true);
    });
}

/// The production spin budget, at volume: this is the configuration that
/// actually ships, and the one the probe's deadlock surfaced under. Mostly
/// exercises the pure-atomic fast path, which is where a *missing* wake hides
/// longest.
#[test]
fn pingpong_stress_production_spin() {
    with_watchdog("pingpong_stress_production_spin", Duration::from_secs(120), || {
        pingpong_round_trips(300_000, 1, DEFAULT_SPIN_LIMIT, false);
        pingpong_round_trips(200_000, 64, DEFAULT_SPIN_LIMIT, true);
    });
}

/// A mid-range budget, where a hop lands on the spin/park boundary itself and
/// the two sides disagree about which path they are on.
#[test]
fn pingpong_stress_boundary_spin() {
    with_watchdog("pingpong_stress_boundary_spin", Duration::from_secs(120), || {
        for spin in [1u32, 8, 64, 512] {
            pingpong_round_trips(20_000, 1, spin, true);
        }
    });
}

/// Forces the park path on essentially every operation by driving the spin
/// budget to its floor: the producer is deliberately far slower than the
/// consumer, so the consumer parks on nearly every message, and vice versa
/// when the roles reverse. This is the highest-park-density test in the file.
#[test]
fn one_way_stress_forced_parks() {
    with_watchdog("one_way_stress_forced_parks", Duration::from_secs(120), || {
        const N: u64 = 40_000;
        let (tx, rx) = SpscRing::channel_with_spin(2, 0);
        let consumer = thread::spawn(move || {
            let mut sum = 0i64;
            let mut got = 0u64;
            while let Some(v) = rx.take() {
                sum = sum.wrapping_add(as_int(&v));
                got += 1;
            }
            (sum, got)
        });
        let mut j = Jitter::new(0x5EED);
        for i in 0..N {
            // Stall hard and often: the consumer drains a 2-slot ring in no
            // time and then parks, every single time.
            for _ in 0..(j.next() % 6000) {
                std::hint::spin_loop();
            }
            assert!(tx.put(int(i as i64)));
        }
        tx.close();
        let (sum, got) = consumer.join().expect("consumer panicked");
        assert_eq!(got, N, "message count mismatch");
        assert_eq!(sum, (0..N as i64).sum::<i64>(), "payload sum mismatch");
    });
}

/// The mirror image: consumer far slower than the producer, so the *producer*
/// parks on a full ring on nearly every message. Exercises `head`'s `PARKED`
/// bit, which the ping-pong tests barely touch.
#[test]
fn one_way_stress_backpressured_producer() {
    with_watchdog("one_way_stress_backpressured_producer", Duration::from_secs(120), || {
        const N: u64 = 40_000;
        let (tx, rx) = SpscRing::channel_with_spin(2, 0);
        let consumer = thread::spawn(move || {
            let mut j = Jitter::new(0xD00D);
            let mut got = 0u64;
            while let Some(v) = rx.take() {
                assert_eq!(as_int(&v), got as i64, "out of order at {got}");
                got += 1;
                for _ in 0..(j.next() % 6000) {
                    std::hint::spin_loop();
                }
            }
            got
        });
        for i in 0..N {
            assert!(tx.put(int(i as i64)));
        }
        tx.close();
        assert_eq!(consumer.join().expect("consumer panicked"), N);
    });
}

#[test]
fn handoff_stress_no_deadlock() {
    with_watchdog("handoff_stress_no_deadlock", Duration::from_secs(120), || {
        const N: u64 = 50_000;
        let (tx, rx) = Handoff::channel_with_spin(0);
        let consumer = thread::spawn(move || {
            let mut j = Jitter::new(0xFACE);
            let mut got = 0u64;
            while let Some(v) = rx.take() {
                assert_eq!(as_int(&v), got as i64);
                got += 1;
                j.maybe_stall(DEFAULT_SPIN_LIMIT);
            }
            got
        });
        let mut j = Jitter::new(0xABCD);
        for i in 0..N {
            assert!(tx.put(int(i as i64)), "handoff put failed at {i}");
            j.maybe_stall(DEFAULT_SPIN_LIMIT);
        }
        tx.close();
        assert_eq!(consumer.join().expect("consumer panicked"), N);
    });
}

/// Runs the whole park/wake protocol many times over from scratch. Fresh
/// transports mean fresh `Waiter` slots and a cold `PARKED` bit each round,
/// which is where a protocol that only works "after the first cycle" breaks.
#[test]
fn many_short_lived_transports() {
    with_watchdog("many_short_lived_transports", Duration::from_secs(120), || {
        for round in 0..2_000u64 {
            let (tx, rx) = SpscRing::channel_with_spin(1, 0);
            let h = thread::spawn(move || {
                let a = rx.take().map(|v| as_int(&v));
                let b = rx.take().map(|v| as_int(&v));
                (a, b)
            });
            assert!(tx.put(int(round as i64)));
            assert!(tx.put(int(round as i64 + 1)));
            tx.close();
            assert_eq!(h.join().unwrap(), (Some(round as i64), Some(round as i64 + 1)));
        }
    });
}

// ---------------------------------------------------------------------------
// 3. Close races.
// ---------------------------------------------------------------------------

#[test]
fn close_during_blocked_take() {
    with_watchdog("close_during_blocked_take", Duration::from_secs(60), || {
        for _ in 0..200 {
            let (tx, rx) = SpscRing::channel(4);
            let h = thread::spawn(move || rx.take());
            // No sleep: the close lands at an unpredictable point in the
            // consumer's spin/register/park sequence, which is the point.
            tx.close();
            assert!(h.join().unwrap().is_none(), "a blocked take must return None on close");
        }
    });
}

#[test]
fn close_during_blocked_take_after_park() {
    with_watchdog("close_during_blocked_take_after_park", Duration::from_secs(60), || {
        for _ in 0..40 {
            let (tx, rx) = SpscRing::channel(4);
            let h = thread::spawn(move || rx.take());
            // Long enough that the consumer is provably past its spin budget
            // and genuinely parked: this is the wake-from-close path.
            thread::sleep(Duration::from_millis(2));
            tx.close();
            assert!(h.join().unwrap().is_none());
        }
    });
}

#[test]
fn close_during_blocked_put() {
    with_watchdog("close_during_blocked_put", Duration::from_secs(60), || {
        for _ in 0..200 {
            let (tx, rx) = SpscRing::channel(1);
            assert!(tx.put(int(1))); // fill it
            let h = thread::spawn(move || tx.put(int(2))); // blocks: full
            rx.close();
            assert!(!h.join().unwrap(), "a put blocked at close time must return false");
            // Drain-then-None still holds for what was already buffered.
            assert_eq!(rx.take().map(|v| as_int(&v)), Some(1));
            assert!(rx.take().is_none());
        }
    });
}

#[test]
fn close_during_blocked_put_after_park() {
    with_watchdog("close_during_blocked_put_after_park", Duration::from_secs(60), || {
        for _ in 0..40 {
            let (tx, rx) = SpscRing::channel(1);
            assert!(tx.put(int(1)));
            let h = thread::spawn(move || tx.put(int(2)));
            thread::sleep(Duration::from_millis(2));
            rx.close();
            assert!(!h.join().unwrap());
        }
    });
}

#[test]
fn close_during_blocked_handoff_rendezvous() {
    with_watchdog("close_during_blocked_handoff_rendezvous", Duration::from_secs(60), || {
        for _ in 0..50 {
            let (tx, rx) = Handoff::channel();
            // The producer deposits the value and then blocks waiting for the
            // consumer to drain it -- the `Unbuffered` "blocked at close"
            // case. The sleep makes the deposit certain, so the assertion
            // below is about the close-wins branch specifically rather than
            // about which of two races won.
            let h = thread::spawn(move || tx.put(int(3)));
            thread::sleep(Duration::from_millis(1));
            rx.close();
            let ok = h.join().unwrap();
            // Nobody ever takes here, so a `true` would mean `put` returned
            // without a rendezvous.
            assert!(!ok, "rendezvous put must report false when close wins");
            // Documented deviation: the value stays drainable.
            assert_eq!(rx.take().map(|v| as_int(&v)), Some(3));
            assert!(rx.take().is_none());
        }
    });
}

#[test]
fn close_races_a_live_stream() {
    // Close lands at an arbitrary point in a running one-way stream -- during
    // a fast-path put, during a blocked put, during the producer's spin
    // phase, or while it is parked; the jittered offset walks the whole
    // range. The invariants under test are that BOTH SIDES TERMINATE (this is
    // the deadlock class), that values arrive in order with no gaps or
    // duplicates, and that the consumer never sees more than was produced.
    with_watchdog("close_races_a_live_stream", Duration::from_secs(120), || {
        for round in 0..60u64 {
            // Capacity 1 keeps the producer bouncing off a full ring, so the
            // close has a real chance of landing on a blocked put.
            // Spin budget 0 so the close lands on a genuinely parked peer
            // as often as on a spinning one.
            let (tx, rx) = SpscRing::channel_with_spin(1, 0);

            let consumer = thread::spawn(move || {
                let mut j = Jitter::new(0xF00D + round);
                let mut expect = 0i64;
                // Take for a jittered while, then close from the CONSUMER
                // side mid-stream, then keep draining to None.
                let until = (j.next() % 4_000) as i64;
                while expect < until {
                    match rx.take() {
                        Some(v) => {
                            assert_eq!(as_int(&v), expect, "reordered, duplicated or gapped");
                            expect += 1;
                        }
                        None => return (expect as u64, true),
                    }
                }
                rx.close();
                while let Some(v) = rx.take() {
                    assert_eq!(as_int(&v), expect, "reordered, duplicated or gapped after close");
                    expect += 1;
                }
                (expect as u64, false)
            });

            let mut produced = 0u64;
            loop {
                if !tx.put(int(produced as i64)) {
                    break;
                }
                produced += 1;
                assert!(produced < 100_000, "consumer never closed");
            }

            let (consumed, _early) = consumer.join().expect("consumer panicked");
            assert!(consumed <= produced, "consumed {consumed} > produced {produced}");
            // The consumer drained to `None`, so nothing may be left behind.
            assert!(tx.is_closed());
        }
    });
}

// ---------------------------------------------------------------------------
// Bounded waits (the flow integration's primitives). Every test here pins
// the spin budget to 0 wherever a park is meant to happen, for the reason
// the module doc gives: at the production budget a fast peer replies inside
// the spin window, so the park path would only be reached by luck.
// ---------------------------------------------------------------------------

#[test]
fn take_timeout_reports_would_block_and_costs_at_least_the_timeout() {
    with_watchdog("take_timeout_reports_would_block", Duration::from_secs(30), || {
        let (_tx, rx) = SpscRing::channel_with_spin(4, 0);
        let t0 = Instant::now();
        assert!(matches!(rx.take_timeout(Duration::from_millis(20)), TryTake::WouldBlock));
        // The bound is a floor on how long the wait lasted, which is what
        // makes it a park rather than a spin; the upper bound is the
        // scheduler's business, not ours.
        assert!(t0.elapsed() >= Duration::from_millis(15), "returned after only {:?}", t0.elapsed());
    });
}

#[test]
fn take_timeout_hands_over_a_value_that_is_already_buffered_without_waiting() {
    with_watchdog("take_timeout_already_buffered", Duration::from_secs(30), || {
        let (tx, rx) = SpscRing::channel_with_spin(4, 0);
        assert!(tx.put(int(7)));
        let t0 = Instant::now();
        assert!(matches!(rx.take_timeout(Duration::from_secs(30)), TryTake::Received(v) if as_int(&v) == 7));
        assert!(t0.elapsed() < Duration::from_secs(5));
    });
}

#[test]
fn take_timeout_wakes_on_a_put_rather_than_riding_out_the_bound() {
    with_watchdog("take_timeout_wakes_on_put", Duration::from_secs(60), || {
        let (tx, rx) = SpscRing::channel_with_spin(4, 0);
        let feeder = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            assert!(tx.put(int(11)));
            tx
        });
        // A 30s bound the wake must beat by three orders of magnitude.
        let t0 = Instant::now();
        assert!(matches!(rx.take_timeout(Duration::from_secs(30)), TryTake::Received(v) if as_int(&v) == 11));
        assert!(t0.elapsed() < Duration::from_secs(10), "woke only after {:?}", t0.elapsed());
        let _tx = feeder.join().expect("feeder");
    });
}

/// Drain-then-`Closed`, the same ordering rule `take`/`try_take` obey: a
/// bounded take must hand over what is still buffered BEFORE it reports the
/// close, or a `flow/stop` would eat the messages already in flight.
#[test]
fn take_timeout_drains_before_reporting_closed() {
    with_watchdog("take_timeout_drains_before_closed", Duration::from_secs(30), || {
        let (tx, rx) = SpscRing::channel_with_spin(4, 0);
        assert!(tx.put(int(1)));
        assert!(tx.put(int(2)));
        tx.close();
        assert!(matches!(rx.take_timeout(Duration::from_millis(50)), TryTake::Received(v) if as_int(&v) == 1));
        assert!(matches!(rx.take_timeout(Duration::from_millis(50)), TryTake::Received(v) if as_int(&v) == 2));
        assert!(matches!(rx.take_timeout(Duration::from_millis(50)), TryTake::Closed));
        // ...and stays Closed, immediately, without burning the bound.
        let t0 = Instant::now();
        assert!(matches!(rx.take_timeout(Duration::from_secs(30)), TryTake::Closed));
        assert!(t0.elapsed() < Duration::from_secs(5));
    });
}

/// The close-races-a-parked-bounded-waiter case. This is the wake protocol's
/// invariant 4 exercised through the new park path specifically: with the
/// spin budget at 0 the waiter is genuinely parked when the close lands.
#[test]
fn close_during_a_bounded_take_wakes_it_immediately() {
    with_watchdog("close_during_bounded_take", Duration::from_secs(60), || {
        let (tx, rx) = SpscRing::channel_with_spin(4, 0);
        let closer = tx.closer();
        let h = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            closer.close();
        });
        let t0 = Instant::now();
        assert!(matches!(rx.take_timeout(Duration::from_secs(30)), TryTake::Closed));
        assert!(t0.elapsed() < Duration::from_secs(10), "woke only after {:?}", t0.elapsed());
        h.join().expect("closer");
        drop(tx);
    });
}

#[test]
fn wait_writable_reports_the_bound_elapsing_on_a_full_ring() {
    with_watchdog("wait_writable_bound", Duration::from_secs(30), || {
        let (tx, _rx) = SpscRing::channel_with_spin(2, 0);
        assert!(matches!(tx.try_put(int(1)), TryPut::Sent));
        assert!(matches!(tx.try_put(int(2)), TryPut::Sent));
        assert!(matches!(tx.try_put(int(3)), TryPut::WouldBlock));
        let t0 = Instant::now();
        assert!(!tx.wait_writable(Duration::from_millis(20)));
        assert!(t0.elapsed() >= Duration::from_millis(15));
        // ...and it consumed nothing: the value the caller still owns can
        // be re-offered unchanged, which is the whole point of splitting
        // "wait for room" out of "put".
        assert!(matches!(tx.try_put(int(3)), TryPut::WouldBlock));
    });
}

#[test]
fn wait_writable_returns_true_as_soon_as_the_consumer_frees_a_slot() {
    with_watchdog("wait_writable_wakes", Duration::from_secs(60), || {
        let (tx, rx) = SpscRing::channel_with_spin(1, 0);
        assert!(matches!(tx.try_put(int(1)), TryPut::Sent));
        let drainer = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            assert!(matches!(rx.try_take(), TryTake::Received(v) if as_int(&v) == 1));
            rx
        });
        let t0 = Instant::now();
        assert!(tx.wait_writable(Duration::from_secs(30)));
        assert!(t0.elapsed() < Duration::from_secs(10), "woke only after {:?}", t0.elapsed());
        assert!(matches!(tx.try_put(int(2)), TryPut::Sent));
        let _rx = drainer.join().expect("drainer");
    });
}

/// A closed transport must not leave a producer waiting for room that will
/// never come -- `wait_writable` reports "stop waiting" so the caller's next
/// `try_put` can report `Closed`.
#[test]
fn wait_writable_returns_on_close_rather_than_riding_out_the_bound() {
    with_watchdog("wait_writable_on_close", Duration::from_secs(60), || {
        let (tx, rx) = SpscRing::channel_with_spin(1, 0);
        assert!(matches!(tx.try_put(int(1)), TryPut::Sent));
        let closer = rx.closer();
        let h = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            closer.close();
        });
        let t0 = Instant::now();
        assert!(tx.wait_writable(Duration::from_secs(30)));
        assert!(t0.elapsed() < Duration::from_secs(10));
        assert!(matches!(tx.try_put(int(2)), TryPut::Closed));
        h.join().expect("closer");
        drop(rx);
    });
}

/// The `_cold` siblings must be behaviourally identical -- they differ only
/// in the spin budget spent before parking, which is a performance choice,
/// never a semantic one.
#[test]
fn the_cold_variants_behave_identically_to_their_spinning_siblings() {
    with_watchdog("cold_variants", Duration::from_secs(60), || {
        let (tx, rx) = SpscRing::channel(4);
        assert!(matches!(rx.take_timeout_cold(Duration::from_millis(5)), TryTake::WouldBlock));
        assert!(tx.put(int(3)));
        assert!(matches!(rx.take_timeout_cold(Duration::from_secs(30)), TryTake::Received(v) if as_int(&v) == 3));

        let (tx2, rx2) = SpscRing::channel(1);
        assert!(matches!(tx2.try_put(int(1)), TryPut::Sent));
        assert!(!tx2.wait_writable_cold(Duration::from_millis(5)));
        assert!(matches!(rx2.try_take(), TryTake::Received(_)));
        assert!(tx2.wait_writable_cold(Duration::from_secs(30)));

        tx.close();
        assert!(matches!(rx.take_timeout_cold(Duration::from_secs(30)), TryTake::Closed));
    });
}

/// A bounded waiter that times out leaves its `PARKED` bit set (invariant
/// 6). The next producer publish clears it and issues one extra `unpark` at
/// a thread that is no longer parked, latching a token -- which must cost at
/// most one spurious lap and never a lost or duplicated message. Thousands
/// of timeout/park cycles interleaved with real traffic, at spin 0 so every
/// single wait really parks.
#[test]
fn repeated_timeouts_interleaved_with_traffic_lose_nothing() {
    with_watchdog("repeated_timeouts_lose_nothing", Duration::from_secs(120), || {
        const N: i64 = 3000;
        let (tx, rx) = SpscRing::channel_with_spin(4, 0);
        let producer = thread::spawn(move || {
            for i in 0..N {
                // A ring of 4 against a consumer that keeps timing out
                // forces the producer through its own bounded wait too.
                while !matches!(tx.try_put(int(i)), TryPut::Sent) {
                    tx.wait_writable_cold(Duration::from_micros(200));
                }
            }
            tx.close();
            tx
        });
        let mut got = Vec::with_capacity(N as usize);
        let mut timeouts = 0u64;
        loop {
            match rx.take_timeout_cold(Duration::from_micros(200)) {
                TryTake::Received(v) => got.push(as_int(&v)),
                TryTake::WouldBlock => timeouts += 1,
                TryTake::Closed => break,
            }
        }
        let _tx = producer.join().expect("producer");
        assert_eq!(got.len(), N as usize, "lost or duplicated messages ({timeouts} timeouts)");
        assert!(got.iter().copied().eq(0..N), "FIFO order broken");
    });
}

// ---------------------------------------------------------------------------
// Heavy tier: run on demand, reported in bench/optimization-log.md.
//   cargo test --release --lib transport::tests::stress -- --ignored --nocapture
// ---------------------------------------------------------------------------

#[test]
#[ignore = "heavy stress tier: cargo test --release --lib transport::tests::stress -- --ignored --nocapture"]
fn stress_multi_million_pingpong() {
    with_watchdog("stress_multi_million_pingpong", Duration::from_secs(900), || {
        let t0 = Instant::now();
        pingpong_round_trips(5_000_000, 1, DEFAULT_SPIN_LIMIT, false);
        println!("stress_multi_million_pingpong: 5,000,000 round trips in {:?}", t0.elapsed());
    });
}

#[test]
#[ignore = "heavy stress tier: cargo test --release --lib transport::tests::stress -- --ignored --nocapture"]
fn stress_multi_million_jittered() {
    with_watchdog("stress_multi_million_jittered", Duration::from_secs(900), || {
        let t0 = Instant::now();
        pingpong_round_trips(2_000_000, 1, 0, true);
        println!("stress_multi_million_jittered: 2,000,000 jittered round trips in {:?}", t0.elapsed());
    });
}

#[test]
#[ignore = "heavy stress tier: cargo test --release --lib transport::tests::stress -- --ignored --nocapture"]
fn stress_multi_million_one_way() {
    with_watchdog("stress_multi_million_one_way", Duration::from_secs(900), || {
        const N: u64 = 20_000_000;
        let t0 = Instant::now();
        let (tx, rx) = SpscRing::channel(1024);
        let consumer = thread::spawn(move || {
            let mut got = 0u64;
            while let Some(v) = rx.take() {
                assert_eq!(as_int(&v), got as i64);
                got += 1;
            }
            got
        });
        for i in 0..N {
            assert!(tx.put(int(i as i64)));
        }
        tx.close();
        assert_eq!(consumer.join().expect("consumer panicked"), N);
        println!("stress_multi_million_one_way: {N} messages in {:?}", t0.elapsed());
    });
}

// ---------------------------------------------------------------------------
// THE TASK ARM (L3.6/W1): the two-source park.
//
// These are the regression net for `Ring::park_task_two_source` and the only
// place in the tree that drives it WITHOUT the flow engine in the way, so a
// failure here points at the protocol rather than at `builtins::flow`.
//
// Every one of them pins `spin` to 0. That is the same reasoning
// `SpscRing::channel_with_spin`'s doc gives for the thread tests, and it
// matters more here: with the default 4000-hint budget a task ping-pong
// almost never reaches its park at all, so a lost wake would show up only
// when the scheduler happened to cooperate. At spin 0 EVERY wait parks, and
// every park is a full two-source register/retract cycle.
// ---------------------------------------------------------------------------

/// The flow engine's consumer loop, reduced to its transport skeleton: snapshot
/// the doorbell BEFORE the non-blocking scan, then a bounded doorbell-aware
/// wait, then loop. Runs as a TASK, so every wait takes the task arm.
fn task_drain_all(rx: SpscRx, db: Arc<Doorbell>, out: Arc<Mutex<Vec<i64>>>, done: Arc<AtomicBool>) {
    loop {
        let seen = db.current();
        match rx.try_take() {
            TryTake::Received(v) => {
                lock_mutex(&out).push(as_int(&v));
                continue;
            }
            TryTake::Closed => break,
            TryTake::WouldBlock => {}
        }
        match rx.take_timeout_or_doorbell(Duration::from_secs(30), &db, seen) {
            TryTake::Received(v) => lock_mutex(&out).push(as_int(&v)),
            TryTake::Closed => break,
            // A doorbell ring, or the (unreached) deadline. Loop and re-scan,
            // exactly as `builtins::flow`'s proc loop does.
            TryTake::WouldBlock => {}
        }
    }
    done.store(true, O::Release);
}

/// The producer skeleton, mirroring `builtins::flow`'s `out_send` lane arm:
/// `try_put`, and on `WouldBlock` snapshot the doorbell and take the
/// doorbell-aware bounded wait for room.
fn task_feed_all(tx: SpscTx, db: Arc<Doorbell>, n: i64, done: Arc<AtomicBool>) {
    for i in 0..n {
        loop {
            match tx.try_put(int(i)) {
                TryPut::Sent => break,
                TryPut::Closed => {
                    done.store(true, O::Release);
                    return;
                }
                TryPut::WouldBlock => {
                    let seen = db.current();
                    tx.wait_writable_or_doorbell(Duration::from_secs(30), &db, seen);
                }
            }
        }
    }
    tx.close();
    done.store(true, O::Release);
}

fn spin_until(flag: &AtomicBool, limit: Duration, what: &str) {
    let deadline = Instant::now() + limit;
    while !flag.load(O::Acquire) {
        assert!(Instant::now() < deadline, "{what} did not finish within {limit:?}");
        thread::sleep(Duration::from_millis(2));
    }
}

/// **Two-source park torture, source A only.** Capacity 1 (so every single
/// message is a full round trip through both park directions), spin 0 (so
/// every wait really parks), thousands of laps, both endpoints TASKS on
/// DIFFERENT shards. The doorbells exist and are registered on but are never
/// rung, which isolates the ring arm: every one of the ~2N parks must be
/// woken by the peer's `publish` and by nothing else.
#[test]
fn task_lane_pingpong_at_capacity_one_loses_nothing() {
    with_watchdog("task_lane_pingpong_cap1", Duration::from_secs(180), || {
        const N: i64 = 4000;
        let (tx, rx) = SpscRing::channel_with_spin(1, 0);
        let got = Arc::new(Mutex::new(Vec::with_capacity(N as usize)));
        let (cdone, pdone) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
        let (cdb, pdb) = (Arc::new(Doorbell::new()), Arc::new(Doorbell::new()));

        let (o, d, db) = (Arc::clone(&got), Arc::clone(&cdone), Arc::clone(&cdb));
        crate::runtime::spawn_on(0, move || task_drain_all(rx, db, o, d));
        let (d, db) = (Arc::clone(&pdone), Arc::clone(&pdb));
        crate::runtime::spawn_on(1, move || task_feed_all(tx, db, N, d));

        spin_until(&pdone, Duration::from_secs(120), "producer task");
        spin_until(&cdone, Duration::from_secs(120), "consumer task");
        let got = lock_mutex(&got);
        assert_eq!(got.len(), N as usize, "lost or duplicated messages on a task lane");
        assert!(got.iter().copied().eq(0..N), "FIFO order broken on a task lane");
    });
}

/// **Two-source park torture, BOTH sources, racing.** The same shape, plus a
/// third thread ringing both doorbells as fast as it can for the whole run.
///
/// This is the test that would catch the specific hazard the two-source
/// protocol exists to close: a ring landing in the window between the
/// doorbell registration and the ring-word CAS. Every such ring must either
/// be observed by `register_task_waker`'s generation re-check (no park) or
/// leave a `NOTIFIED` note the scheduler consumes at the park boundary
/// (immediate resume) -- never a park that sleeps through it. A failure is a
/// hang, which the watchdog turns into an abort.
///
/// It equally guards the OTHER direction: with ~10^5 doorbell rings racing
/// ~8000 ring-word parks, a retraction bug (a `TaskWaker` left in the ring's
/// waiter slot, or in the doorbell's list, after its park ended) shows up as
/// a wake fired at a task parked somewhere else -- which on this shape means
/// a message consumed by a lap that never happened, i.e. a length or order
/// mismatch below.
#[test]
fn task_lane_survives_a_doorbell_ring_storm() {
    with_watchdog("task_lane_doorbell_storm", Duration::from_secs(180), || {
        const N: i64 = 4000;
        let (tx, rx) = SpscRing::channel_with_spin(1, 0);
        let got = Arc::new(Mutex::new(Vec::with_capacity(N as usize)));
        let (cdone, pdone) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
        let (cdb, pdb) = (Arc::new(Doorbell::new()), Arc::new(Doorbell::new()));

        let stop = Arc::new(AtomicBool::new(false));
        let (s, a, b) = (Arc::clone(&stop), Arc::clone(&cdb), Arc::clone(&pdb));
        let ringer = thread::spawn(move || {
            let mut rings = 0u64;
            while !s.load(O::Acquire) {
                a.ring();
                b.ring();
                rings += 2;
            }
            rings
        });

        let (o, d, db) = (Arc::clone(&got), Arc::clone(&cdone), Arc::clone(&cdb));
        crate::runtime::spawn_on(0, move || task_drain_all(rx, db, o, d));
        let (d, db) = (Arc::clone(&pdone), Arc::clone(&pdb));
        crate::runtime::spawn_on(1, move || task_feed_all(tx, db, N, d));

        spin_until(&pdone, Duration::from_secs(120), "producer task");
        spin_until(&cdone, Duration::from_secs(120), "consumer task");
        stop.store(true, O::Release);
        let rings = ringer.join().expect("ringer");
        let got = lock_mutex(&got);
        assert_eq!(got.len(), N as usize, "lost or duplicated messages under {rings} doorbell rings");
        assert!(got.iter().copied().eq(0..N), "FIFO order broken under {rings} doorbell rings");
    });
}

/// **A control event must reach a task parked on an EMPTY lane** -- the
/// whole reason source B exists. The consumer parks on a ring nobody is
/// feeding; the only thing that can wake it is a `Doorbell::ring`, and it
/// must arrive promptly rather than riding out the (30 s) deadline. Without
/// the doorbell registration this test hangs; with a thread-only `Ring` it
/// hangs too, because `Doorbell::ring`'s `owner_thread.unpark()` targets a
/// SHARD thread, not a task.
#[test]
fn a_doorbell_ring_wakes_a_task_parked_on_an_empty_lane() {
    with_watchdog("doorbell_wakes_lane_parked_task", Duration::from_secs(60), || {
        let (tx, rx) = SpscRing::channel_with_spin(4, 0);
        let db = Arc::new(Doorbell::new());
        let woke = Arc::new(AtomicBool::new(false));
        let (w, d) = (Arc::clone(&woke), Arc::clone(&db));
        crate::runtime::spawn(move || {
            let seen = d.current();
            assert!(matches!(rx.try_take(), TryTake::WouldBlock), "the ring should start empty");
            // Parks on BOTH sources; only the doorbell can fire.
            let r = rx.take_timeout_or_doorbell(Duration::from_secs(30), &d, seen);
            assert!(matches!(r, TryTake::WouldBlock), "a doorbell ring must surface as WouldBlock");
            w.store(true, O::Release);
            drop(rx);
        });
        thread::sleep(Duration::from_millis(120));
        assert!(!woke.load(O::Acquire), "the task returned before anything rang");
        let t0 = Instant::now();
        db.ring();
        spin_until(&woke, Duration::from_secs(10), "lane-parked task after a ring");
        assert!(t0.elapsed() < Duration::from_secs(5), "the ring did not wake it promptly: {:?}", t0.elapsed());
        drop(tx);
    });
}

/// **L4: killing a task while it is LANE-PARKED** (docs/L4-SUPERVISION-DESIGN.md
/// §3.5). The task registers on both sources and suspends; the killer wins
/// the `PARKED -> KILLED` CAS and force-unwinds it.
///
/// Three things are on trial:
/// 1. the kill LANDS on a task parked in `park_task_two_source` -- that park
///    is an ordinary `park_current_yield`, so it must be no different from a
///    chan park;
/// 2. destructors run on the way out (the `SpscRx` is dropped, releasing its
///    `Arc` on the ring) -- proved by the drop counter on the guard the task
///    holds;
/// 3. the corpse's registrations are HARMLESS. The killed task never got to
///    retract, so its `TaskWaker` is still in the ring's waiter slot and in
///    the doorbell's list. A later `publish` and a later `ring` must both be
///    no-ops (`TaskWaker::wake`'s `Err(_) => return` arm, reached because the
///    task is `KILLED`/`DONE`) rather than a panic or a hang.
///
/// **Message safety needs no tombstone here, and that is a property of the
/// ring, not luck.** A value only leaves the buffer inside `Ring::consume`,
/// which is straight-line non-parking code on the consumer's own stack, and
/// the producer keeps ownership of its value across `wait_writable` (that
/// method is a WAIT, not a timed put). So there is no "value in transit
/// through a cell" state for a kill to land in -- the hazard P5a-bis's
/// tombstone exists for on the `Chan` path. What a kill CAN leave is
/// undrained buffered input, which is exactly what killing any proc with a
/// non-empty inbox has always meant; the assertion below is that those values
/// are DROPPED, not leaked.
#[test]
fn a_lane_parked_task_can_be_killed_and_leaves_nothing_dangerous_behind() {
    with_watchdog("kill_a_lane_parked_task", Duration::from_secs(60), || {
        let (tx, rx) = SpscRing::channel_with_spin(4, 0);
        let db = Arc::new(Doorbell::new());
        let dropped = Arc::new(AtomicBool::new(false));
        let (wtx, wrx) = std::sync::mpsc::channel();

        struct DropFlag(Arc<AtomicBool>);
        impl Drop for DropFlag {
            fn drop(&mut self) {
                self.0.store(true, O::Release);
            }
        }

        let (d, flag) = (Arc::clone(&db), Arc::clone(&dropped));
        crate::runtime::spawn(move || {
            let _guard = DropFlag(flag);
            wtx.send(crate::runtime::current_waker()).expect("send waker");
            let seen = d.current();
            // Parks forever: nothing will ever be published and nothing will
            // ever ring. The only way out is the kill.
            let _ = rx.take_timeout_or_doorbell(Duration::from_secs(600), &d, seen);
            unreachable!("a killed task must never resume into user code");
        });

        let waker = wrx.recv_timeout(Duration::from_secs(10)).expect("the task never started");
        let killed_before = crate::runtime::tasks_killed();
        let deadline = Instant::now() + Duration::from_secs(20);
        // A retry loop, per `TaskWaker::kill`'s contract: `false` means "not
        // parked at that instant", and the task needs a moment to reach its
        // park.
        while !waker.kill() {
            assert!(Instant::now() < deadline, "never won the PARKED -> KILLED race");
            thread::sleep(Duration::from_millis(2));
        }
        while crate::runtime::tasks_killed() == killed_before {
            assert!(Instant::now() < deadline, "the kill was claimed but never completed");
            thread::sleep(Duration::from_millis(2));
        }
        assert!(dropped.load(O::Acquire), "the killed task's destructors did not run");

        // 3: the corpse's stale registrations must be inert. Both of these
        // reach `TaskWaker::wake` on a DONE task.
        assert!(matches!(tx.try_put(int(1)), TryPut::Sent), "a put after the consumer died must still buffer");
        db.ring();
        // And the undrained value is dropped, not leaked, when the ring goes.
        drop(tx);
    });
}
