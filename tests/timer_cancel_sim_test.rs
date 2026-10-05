//! # TIMER-CANCEL under virtual time (G-CANCEL-SIM)
//!
//! The claims real mode can only bound with margins, pinned EXACTLY under
//! `simulate` — plus the two sim-specific rules of
//! `docs/TIMER-CANCEL-DESIGN.md`:
//!
//! - a cancelled `Put` is dropped by the advance rule's step 1, so it
//!   advances virtual time by ZERO and never counts as a fire
//!   (`:virtual-ms` and `:timer-fires` stay honest);
//! - the debounce idiom — the whole reason this surface exists — is
//!   deterministic: a cancel/re-arm ladder costs exactly the virtual time
//!   its arithmetic says, seed after seed.
//!
//! Same in-process discipline as `tests/l5_sim_api.rs` (whose module doc
//! is the normative statement): one sim world at a time ([`SIM`]), and
//! nothing in this file may touch the runtime before its first
//! `simulate`.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use mova::internal::{pr_str, render, Interp};

/// One simulated world at a time, process-wide — see the module doc.
static SIM: Mutex<()> = Mutex::new(());

fn ev(src: &str) -> String {
    let _g = SIM.lock().unwrap_or_else(|e| e.into_inner());
    let mut interp = Interp::new();
    match interp.eval_str("timer-cancel-sim", src) {
        Ok(v) => pr_str(&v),
        Err(e) => panic!("{}", render(&e, "timer-cancel-sim", src)),
    }
}

/// [`ev`] with a wall-clock ceiling: every program here waits on VIRTUAL
/// deadlines (up to 30 s of them), so real time must stay trivial.
fn ev_fast(src: &str, limit: Duration) -> String {
    let t0 = Instant::now();
    let out = ev(src);
    let took = t0.elapsed();
    assert!(took < limit, "simulation took {took:?} (limit {limit:?}): {out}");
    out
}

/// The headline rule: cancelling a 30-SECOND timer costs zero virtual
/// time and zero fires. Jumping the clock 30 s to fire a no-op would
/// make `:virtual-ms` a lie — the advance rule's step 1 drops the
/// settled entry instead, exactly as it already does for `Wake`.
#[test]
fn a_cancelled_timer_advances_virtual_time_by_zero() {
    let out = ev_fast(
        r#"(let [r (simulate {:seed 1}
                     (fn [] (let [c (chan 1)
                                  t (timeout-put 30000 c :never)]
                              [(cancel-timer! t) (timer-armed? t) (poll! c)])))]
             [(:result r) (:virtual-ms r) (:timer-fires r) (:leaked-tasks r)])"#,
        Duration::from_secs(2),
    );
    assert_eq!(out, "[[true false nil] 0 0 0]");
}

#[test]
fn a_live_timer_fires_at_exactly_its_virtual_deadline() {
    let out = ev_fast(
        r#"(let [r (simulate {:seed 1}
                     (fn [] (let [c (chan 1)]
                              (timeout-put 750 c :fired)
                              (<!! c))))]
             [(:result r) (:virtual-ms r) (:timer-fires r)])"#,
        Duration::from_secs(2),
    );
    assert_eq!(out, "[:fired 750 1]");
}

#[test]
fn cancel_after_the_virtual_fire_honestly_loses() {
    let out = ev_fast(
        r#"(:result (simulate {:seed 1}
             (fn [] (let [c (chan 1)
                          t (timeout-put 5 c :x)
                          v (<!! c)]
                      [v (cancel-timer! t)]))))"#,
        Duration::from_secs(2),
    );
    assert_eq!(out, "[:x false]");
}

/// The debounce idiom, byte-exact: ten "keystrokes" 100 virtual ms
/// apart, each cancelling the pending 750 ms save and re-arming it.
/// Arithmetic says: 9 gaps × 100 + one full quiet window of 750 =
/// 1650 virtual ms; fires are the 9 gap `timeout`s + the ONE `Put`
/// that survived — the 9 cancelled `Put`s are dropped, not fired.
/// Then the whole thing again on the same seed: identical, key for key.
#[test]
fn the_debounce_ladder_is_exact_and_reproducible() {
    let out = ev_fast(
        r#"(let [scenario
                 (fn [] (let [c (chan (sliding-buffer 1))]
                          (loop [i 0 t (timeout-put 750 c [:save 0])]
                            (if (< i 9)
                              (do (<!! (timeout 100))
                                  (cancel-timer! t)
                                  (recur (inc i) (timeout-put 750 c [:save (inc i)])))
                              (<!! c)))))
                 key (fn [r] [(:result r) (:virtual-ms r) (:timer-fires r) (:leaked-tasks r)])
                 a (key (simulate {:seed 7} scenario))
                 b (key (simulate {:seed 7} scenario))]
             [a (= a b)])"#,
        Duration::from_secs(3),
    );
    assert_eq!(out, "[[[:save 9] 1650 10 0] true]");
}

/// Sub-ms virtual granularity aside, a cancelled entry deeper in the
/// heap must not perturb the LIVE deadlines around it: cancel the middle
/// of three timers and the other two still fire at exactly their own
/// virtual instants, in deadline order.
#[test]
fn a_cancelled_entry_between_live_ones_is_invisible() {
    let out = ev_fast(
        r#"(let [r (simulate {:seed 1}
                     (fn [] (let [c (chan 3)
                                  _ (timeout-put 100 c :first)
                                  mid (timeout-put 200 c :middle)
                                  _ (timeout-put 300 c :last)]
                              (cancel-timer! mid)
                              [(<!! c) (<!! c)])))]
             [(:result r) (:virtual-ms r) (:timer-fires r)])"#,
        Duration::from_secs(2),
    );
    assert_eq!(out, "[[:first :last] 300 2]");
}
