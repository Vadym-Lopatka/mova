//! L1/W3 gate tests: task parking in `Chan` (the sudog hand-off) and the
//! `Doorbell` task arm (docs/L1-LANDING-SPEC.md §W3).
//!
//! These drive `runtime::spawn` and the REAL `chan_put`/`chan_take`/
//! `chan_close`/`alts!!` primitives directly, through
//! `mova::internal::task_chan`. They cannot be written through the language
//! yet: W3 lands the task arms, W4 is what makes `go` reach them.
//!
//! Discipline, borrowed from `tests/runtime_test.rs`: **every wait is a
//! deadline loop, never a bare block.** A lost wake is the failure mode this
//! whole wave exists to rule out, and a test that hangs on one reports
//! nothing. The budgets are seconds against microsecond-scale work, so a
//! timeout here always means "a wake was dropped", never "the machine was
//! slow". Task parks themselves carry NO safety net by design (landing
//! stance), which is exactly why the tests must supply the deadline.

use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mova::internal::task_chan::{
    alts_take2_int, chan, close, put_int, take_int, BufferPolicy, Doorbell,
};
use mova::runtime;

const BUDGET: Duration = Duration::from_secs(30);

fn await_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + BUDGET;
    while Instant::now() < deadline {
        if cond() {
            return;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    panic!("task_chan_test: timed out waiting for {what} -- a wake was lost");
}

/// A completion log every task appends to, so a test can assert on what
/// happened AND on the order it happened in.
#[derive(Default)]
struct Log(Mutex<Vec<String>>);

impl Log {
    fn push(&self, s: impl Into<String>) {
        self.0.lock().expect("log mutex").push(s.into());
    }
    fn len(&self) -> usize {
        self.0.lock().expect("log mutex").len()
    }
    fn snapshot(&self) -> Vec<String> {
        self.0.lock().expect("log mutex").clone()
    }
}

// ---------------------------------------------------------------------------
// THE forbidden-shape regression
// ---------------------------------------------------------------------------

/// The regression docs/L1-LANDING-SPEC.md §W3 was written around.
///
/// Two task putters park on ONE unbuffered chan; a taker (an OS thread here,
/// deliberately slow) collects twice. **Both puts must return `true`.**
///
/// Under the probe's inference -- "my value was taken" == `buffer.is_empty()`
/// -- this is the case that breaks: a task cannot hold the chan guard across
/// its suspend, so putter B can drop its value into the slot in the window
/// after A's value is collected and before A is resumed; A then wakes, sees a
/// non-empty slot, and parks again with its put ALREADY COMPLETE. It would
/// hang here (and report `false` if the chan then closed). With the value
/// riding in the waiter and a commit cell of its own, neither putter has
/// anything to infer.
#[test]
fn two_task_putters_on_one_unbuffered_chan_both_return_true() {
    let ch = chan(BufferPolicy::Unbuffered);
    let oks = Arc::new(AtomicUsize::new(0));
    let log = Arc::new(Log::default());

    for n in [1_i64, 2] {
        let (ch, oks, log) = (ch.clone(), oks.clone(), log.clone());
        runtime::spawn(move || {
            let sent = put_int(&ch, n);
            assert!(sent, "task putter {n}: put reported false on an open chan");
            oks.fetch_add(1, Ordering::SeqCst);
            log.push(format!("put{n}"));
        });
    }

    // Slow on purpose: it widens the exact window the forbidden inference
    // loses its value in.
    let mut got = Vec::new();
    for _ in 0..2 {
        std::thread::sleep(Duration::from_millis(20));
        got.push(take_int(&ch).expect("a task putter is holding a value out"));
    }
    got.sort_unstable();
    assert_eq!(got, vec![1, 2], "both task-putter values must be delivered");

    await_until("both task putters to resume", || {
        oks.load(Ordering::SeqCst) == 2
    });
    assert_eq!(log.len(), 2);
}

/// The other half of the commit cell's vocabulary: a parked task putter
/// whose channel closes reports `false` (Clojure: "puts blocked at close
/// time return false"), and reports it promptly -- there is no safety net to
/// fall back on.
#[test]
fn parked_task_putter_sees_false_when_the_chan_closes() {
    let ch = chan(BufferPolicy::Unbuffered);
    let result = Arc::new(AtomicI64::new(-1));

    {
        let (ch, result) = (ch.clone(), result.clone());
        runtime::spawn(move || {
            let sent = put_int(&ch, 7);
            result.store(i64::from(sent), Ordering::SeqCst);
        });
    }
    // Let it actually park before closing -- the point is a putter blocked
    // AT close time, not one that raced the close.
    std::thread::sleep(Duration::from_millis(50));
    close(&ch);

    await_until("the parked putter to be committed Closed", || {
        result.load(Ordering::SeqCst) >= 0
    });
    assert_eq!(result.load(Ordering::SeqCst), 0, "a put blocked at close must return false");
}

// ---------------------------------------------------------------------------
// Mixed kinds: neither waiter family may strand the other
// ---------------------------------------------------------------------------

/// OS-thread putter, parked TASK taker. The putter must deliver into the
/// taker's cell rather than buffering (or, on an unbuffered chan,
/// cv-waiting) beside a task that can never be reached by `notify_all`.
#[test]
fn os_thread_putter_reaches_a_parked_task_taker() {
    let ch = chan(BufferPolicy::Unbuffered);
    let got = Arc::new(AtomicI64::new(-1));

    {
        let (ch, got) = (ch.clone(), got.clone());
        runtime::spawn(move || {
            let v = take_int(&ch).expect("value, not a close");
            got.store(v, Ordering::SeqCst);
        });
    }
    std::thread::sleep(Duration::from_millis(50)); // ensure it is parked
    let ch2 = ch.clone();
    let putter = std::thread::spawn(move || put_int(&ch2, 99));

    await_until("the task taker to receive", || got.load(Ordering::SeqCst) == 99);
    assert!(putter.join().expect("putter thread"), "the OS putter's hand-off must report true");
}

/// The mirror: TASK putter, OS-thread taker parked in the condvar path. The
/// putter's enqueue owes `cv.notify_all()`, or the taker sleeps beside a
/// value that is being held out to it.
#[test]
fn task_putter_reaches_a_parked_os_thread_taker() {
    let ch = chan(BufferPolicy::Unbuffered);
    let ch2 = ch.clone();
    let taker = std::thread::spawn(move || take_int(&ch2));

    std::thread::sleep(Duration::from_millis(50)); // taker is in cv_wait
    let sent = Arc::new(AtomicI64::new(-1));
    {
        let (ch, sent) = (ch.clone(), sent.clone());
        runtime::spawn(move || sent.store(i64::from(put_int(&ch, 5)), Ordering::SeqCst));
    }

    assert_eq!(taker.join().expect("taker thread"), Some(5));
    await_until("the task putter to be committed Done", || {
        sent.load(Ordering::SeqCst) >= 0
    });
    assert_eq!(sent.load(Ordering::SeqCst), 1);
}

// ---------------------------------------------------------------------------
// Buffered chans: capacity must never idle while a putter waits
// ---------------------------------------------------------------------------

/// cap-1 chan: a task puts v1 (buffered, no park) and then parks putting v2.
/// A take of v1 frees the slot, which must PROMOTE v2 into it and commit the
/// putter -- nobody else is going to, since a task putter has no condvar
/// retry loop of its own.
#[test]
fn buffered_promote_fills_freed_capacity_from_a_parked_task_putter() {
    let ch = chan(BufferPolicy::Fixed(1));
    let done = Arc::new(AtomicUsize::new(0));

    {
        let (ch, done) = (ch.clone(), done.clone());
        runtime::spawn(move || {
            assert!(put_int(&ch, 1), "v1 fits in the buffer");
            done.fetch_add(1, Ordering::SeqCst);
            assert!(put_int(&ch, 2), "v2 parks, then is promoted");
            done.fetch_add(1, Ordering::SeqCst);
        });
    }

    await_until("the putter to park on v2", || done.load(Ordering::SeqCst) == 1);
    assert_eq!(take_int(&ch), Some(1), "buffered v1 comes out first");
    await_until("v2 to be promoted and its putter woken", || {
        done.load(Ordering::SeqCst) == 2
    });
    assert_eq!(take_int(&ch), Some(2), "v2 was promoted into the freed slot");
}

// ---------------------------------------------------------------------------
// close!
// ---------------------------------------------------------------------------

/// close! decides drain-then-nil FOR parked task takers, because they cannot
/// re-run their own loop: the first gets the value that is in flight, the
/// second gets nil.
#[test]
fn close_gives_the_value_to_the_first_task_taker_and_nil_to_the_second() {
    let ch = chan(BufferPolicy::Fixed(1));
    let log = Arc::new(Log::default());

    // Two takers park in spawn order, so the queue is FIFO by construction.
    for n in 0..2 {
        let (ch, log) = (ch.clone(), log.clone());
        runtime::spawn(move || {
            let got = take_int(&ch);
            log.push(format!("taker{n}={got:?}"));
        });
        std::thread::sleep(Duration::from_millis(30));
    }

    // One value in flight, then close. The put is served straight out of the
    // putter's hand into taker 0's cell (the L2 direct-switch seam), so what
    // close! finds is one parked taker and an empty buffer -- and hands it
    // the closed-marker.
    assert!(put_int(&ch, 42));
    close(&ch);

    await_until("both takers to finish", || log.len() == 2);
    // Sorted: WHICH taker got what is the assertion, not which one's shard
    // happened to write its log line first (the two run on different shards).
    let mut got = log.snapshot();
    got.sort();
    assert_eq!(
        got,
        vec!["taker0=Some(42)".to_string(), "taker1=None".to_string()],
        "drain-then-nil, in taker-queue order: the head taker gets the value, the next gets nil"
    );
}

// ---------------------------------------------------------------------------
// alts!! from tasks
// ---------------------------------------------------------------------------

/// `alts!!` between two TASKS, one putting and one selecting over two chans.
/// The pair must complete no matter where round-robin placement puts them --
/// including on the SAME shard, where a thread-parking `alts!!` would wedge
/// the shard and neither task would ever run again. That is why the
/// `Doorbell` needed a task arm.
///
/// Coverage of the same-shard case is by construction rather than by
/// assertion: `shard_count()` pairs are spawned back to back, and
/// round-robin placement guarantees at least one pair lands both halves on
/// one shard (with an odd shard count, several do). Every pair must
/// complete.
#[test]
fn alts_between_two_tasks_completes_on_every_shard_placement() {
    let pairs = runtime::shard_count().max(2) + 1;
    let done = Arc::new(AtomicUsize::new(0));

    for i in 0..pairs {
        let a = chan(BufferPolicy::Unbuffered);
        let b = chan(BufferPolicy::Unbuffered);
        let n = 100 + i as i64;

        // The putter goes first so that BOTH orders occur across the run:
        // sometimes it parks in `task_putters` before the selector scans,
        // sometimes the selector is already parked on its doorbell.
        {
            let a = a.clone();
            runtime::spawn(move || {
                assert!(put_int(&a, n), "task put into an alts!!-selected chan");
            });
        }
        {
            let (a, b, done) = (a.clone(), b.clone(), done.clone());
            runtime::spawn(move || {
                let got = alts_take2_int(&a, &b);
                assert_eq!(got, Some(n), "alts!! resolved the wrong op");
                done.fetch_add(1, Ordering::SeqCst);
            });
        }
    }

    await_until("every alts!! pair to complete", || {
        done.load(Ordering::SeqCst) == pairs
    });
}

// ---------------------------------------------------------------------------
// The Doorbell task arm, on its own
// ---------------------------------------------------------------------------

/// A task parks in `wait_for_change`; another OS thread rings; the task
/// resumes and observes a moved generation. No timeout is involved anywhere
/// -- the ring IS the mechanism (landing stance), so a hang here is a real
/// missed wake and not a slow deadline.
#[test]
fn doorbell_task_arm_wakes_a_parked_task() {
    // Built ON the shard thread the task will run on would be ideal, but the
    // capture only affects `owner_thread`'s (harmless) unpark -- see its doc.
    let db = Arc::new(Doorbell::new());
    let parked = Arc::new(AtomicUsize::new(0));
    let woke = Arc::new(AtomicI64::new(-1));

    {
        let (db, parked, woke) = (db.clone(), parked.clone(), woke.clone());
        runtime::spawn(move || {
            let seen = db.current();
            parked.store(1, Ordering::SeqCst);
            let now = db.wait_for_change(seen, Duration::from_secs(3600));
            woke.store(now as i64, Ordering::SeqCst);
        });
    }

    await_until("the task to reach its park", || parked.load(Ordering::SeqCst) == 1);
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(woke.load(Ordering::SeqCst), -1, "nothing rang yet, so nothing may have woken");

    db.ring();
    await_until("the rung task to resume", || woke.load(Ordering::SeqCst) >= 0);
    assert!(
        woke.load(Ordering::SeqCst) > 0,
        "the resumed task must observe the moved generation"
    );
}

/// The registration must not outlive the park: a task that returns from
/// `wait_for_change` and parks somewhere else entirely must not be woken by
/// the next ring of a doorbell it no longer cares about. Rings after the
/// task has moved on are inert.
#[test]
fn doorbell_registration_does_not_outlive_its_park() {
    let db = Arc::new(Doorbell::new());
    let ch = chan(BufferPolicy::Unbuffered);
    let phase = Arc::new(AtomicUsize::new(0));

    {
        let (db, ch, phase) = (db.clone(), ch.clone(), phase.clone());
        runtime::spawn(move || {
            let seen = db.current();
            phase.store(1, Ordering::SeqCst);
            db.wait_for_change(seen, Duration::from_secs(3600));
            phase.store(2, Ordering::SeqCst);
            // Now park on something ELSE. Stale doorbell registrations would
            // aim their wakes here.
            let v = take_int(&ch);
            phase.store(3, Ordering::SeqCst);
            assert_eq!(v, Some(11));
        });
    }

    await_until("the task to park on the doorbell", || phase.load(Ordering::SeqCst) == 1);
    db.ring();
    await_until("the task to move on to the chan", || phase.load(Ordering::SeqCst) == 2);

    // Ring a few more times: with the registration correctly removed these
    // reach nobody. (They cannot corrupt anything either way -- a spurious
    // task wake is absorbed by every park loop in W3 -- so what this really
    // pins is that the chan take still completes exactly once, from the put.)
    for _ in 0..5 {
        db.ring();
    }
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(phase.load(Ordering::SeqCst), 2, "a stale ring must not complete a chan take");

    assert!(put_int(&ch, 11));
    await_until("the chan take to complete", || phase.load(Ordering::SeqCst) == 3);
}
