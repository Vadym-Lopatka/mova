//! # TIMER-CANCEL — the cancellable timer, real mode (G-CANCEL)
//!
//! `(timeout-put ms ch val)` / `(cancel-timer! t)` / `(timer-armed? t)`:
//! the runtime's one cancellable timer, whose entire design is a one-shot
//! claim cell that the fire and the cancel race for — see
//! `docs/TIMER-CANCEL-DESIGN.md` and `TimerCancel`'s own doc.
//!
//! What needs proving here, and nowhere else:
//!
//! - **exactly-once under a real race** (`fire_and_cancel_race_has_
//!   exactly_one_winner`): over hundreds of rounds of a timer thread
//!   firing against a caller cancelling, every round is delivered XOR
//!   suppressed — never both, never neither, never a duplicate. This is
//!   the whole product claim, so it gets the hammer.
//! - **honest returns**: `cancel-timer!` answers "will the value never be
//!   delivered", so a cancel that lost (fired already, or an earlier
//!   cancel won) says `false` — and is a no-op, never an error.
//! - **the delivery contract is `offer!`** (`a_full_or_closed_chan_drops_
//!   the_put`): the fire runs on the timer thread, which may not park, so
//!   a full or closed chan sheds the put silently — the documented
//!   consumer contract, pinned so it can never silently become a park.
//! - **thread economy is inherited** (`timeout_put_shares_the_one_timer_
//!   thread`): a `Put` entry rides the same process-wide heap as
//!   `timeout`, not a second service.
//!
//! Wall-clock deadlines here use generous margins (the
//! `tests/timer_service_test.rs` discipline); the EXACT-timing claims
//! live in `tests/timer_cancel_sim_test.rs`, where the clock is virtual
//! and margins are zero.

use mova::embed::{Engine, Value};

fn eval_ok(src: &str) -> Value {
    Engine::builder()
        .build()
        .eval_named("timer_cancel_test", src)
        .unwrap_or_else(|e| panic!("eval error for {src:?}: {}", e.render_plain()))
}

fn ps(src: &str) -> String {
    eval_ok(src).to_string()
}

#[test]
fn an_uncancelled_timer_fires_and_delivers() {
    let out = ps(r#"
        (let [c (chan 1)
              t (timeout-put 5 c :v)
              armed-before (timer-armed? t)
              v (<!! c)]
          [armed-before v (timer-armed? t) (cancel-timer! t)])"#);
    // Armed when created; the take gets the value; fired => settled, and
    // a late cancel honestly reports it lost.
    assert_eq!(out, "[true :v false false]");
}

#[test]
fn cancel_before_the_deadline_suppresses_delivery() {
    let out = ps(r#"
        (let [c (chan 1)
              t (timeout-put 40 c :v)
              won (cancel-timer! t)
              ;; Wait well past the deadline, then look: nothing may have
              ;; arrived, ever.
              _ (<!! (timeout 120))]
          [won (timer-armed? t) (poll! c)])"#);
    assert_eq!(out, "[true false nil]");
}

#[test]
fn a_second_cancel_loses_the_claim() {
    let out = ps(r#"
        (let [c (chan 1)
              t (timeout-put 60000 c :v)]
          [(cancel-timer! t) (cancel-timer! t) (cancel-timer! t)])"#);
    assert_eq!(out, "[true false false]");
}

/// THE hammer: the timer thread's fire against the caller's cancel, with
/// the cancel's arrival spread across the deadline by a variable spin.
/// Exactly one side may win each round: `won` => the chan stays empty
/// forever; `!won` => exactly one value arrives. Any `:both`, `:neither`,
/// or duplicate is a failed round, and zero rounds may fail.
#[test]
fn fire_and_cancel_race_has_exactly_one_winner() {
    let out = ps(r#"
        (let [rounds 200
              bad (atom [])]
          (dotimes [i rounds]
            (let [c (chan 4)
                  t (timeout-put 1 c :v)]
              ;; Spread the cancel across the 1ms deadline: some rounds
              ;; cancel instantly, some after real work, some well after
              ;; the fire.
              (loop [n (* (mod i 20) 400) acc 0]
                (when (pos? n) (recur (dec n) (+ acc n))))
              (let [won (cancel-timer! t)
                    ;; Past the deadline for sure; the fire (if it won)
                    ;; has committed its offer! by the time this returns.
                    _ (<!! (timeout 15))
                    got (poll! c)
                    dup (poll! c)]
                (when-not (or (and won (nil? got))
                              (and (not won) (= got :v) (nil? dup)))
                  (swap! bad conj [i won got dup])))))
          [@bad rounds])"#);
    assert_eq!(out, "[[] 200]", "some round was delivered AND cancelled, or neither, or twice");
}

/// The delivery contract: `offer!` semantics, pinned. An unbuffered chan
/// with no taker sheds the put (the fire may not park); a closed chan
/// sheds it too. Either way the fire WON the claim — the shed is the
/// consumer's contract violation, not an un-fire — so a late cancel
/// still honestly reports `false`.
#[test]
fn a_full_or_closed_chan_drops_the_put() {
    let out = ps(r#"
        (let [full (chan)
              t1 (timeout-put 5 full :x)
              closed (chan 1)
              _ (close! closed)
              t2 (timeout-put 5 closed :y)
              _ (<!! (timeout 60))]
          [(cancel-timer! t1) (poll! full)
           (cancel-timer! t2) (poll! closed)])"#);
    assert_eq!(out, "[false nil false nil]");
}

#[test]
fn two_timers_deliver_in_deadline_order() {
    let out = ps(r#"
        (let [c (chan 2)]
          (timeout-put 60 c :later)
          (timeout-put 5 c :sooner)
          [(<!! c) (<!! c)])"#);
    assert_eq!(out, "[:sooner :later]");
}

#[test]
fn the_handle_is_a_first_class_identity_value() {
    let out = ps(r#"
        (let [c (chan 1)
              t (timeout-put 60000 c :v)
              u (timeout-put 60000 c :v)]
          [(pr-str t) (= t t) (= t u) (= (hash t) (hash t))
           (str (class t))
           (try (cancel-timer! c) (catch Exception e :type-error))
           (do (cancel-timer! t) (cancel-timer! u) :cleaned)])"#);
    assert_eq!(out, r##"["#<timer>" true false true "class mova.async.Timer" :type-error :cleaned]"##);
}

#[test]
fn nil_is_rejected_like_every_other_put() {
    let out = ps(r#"
        (try (timeout-put 5 (chan 1) nil)
             (catch Exception e :nil-rejected))"#);
    assert_eq!(out, ":nil-rejected");
}

/// A `Put` entry rides the ONE process-wide timer service — same heap,
/// same thread — not a second mechanism. The white-box hook is the same
/// one `tests/timer_service_test.rs` pins `timeout`'s economy with.
#[test]
fn timeout_put_shares_the_one_timer_thread() {
    let before = mova::internal::async_timer::threads_spawned();
    let out = ps(r#"
        (let [c (chan 8)]
          (dotimes [_ 32] (cancel-timer! (timeout-put 60000 c :v)))
          (timeout-put 1 c :last)
          (<!! c))"#);
    assert_eq!(out, ":last");
    let after = mova::internal::async_timer::threads_spawned();
    assert!(after <= before.max(1), "timeout-put must never spawn its own thread: {before} -> {after}");
}
