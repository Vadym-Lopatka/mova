//! # L5 / W4 — the EMBEDDER's door into deterministic simulation
//!
//! `mova::embed::sim::enable` is process-scoped by design (see that module's
//! doc for why it is not an `EngineBuilder` option), so this whole file is one
//! process that turns sim on once, up front, and then embeds normally. The
//! [`SIM`] lock is what keeps `cargo test`'s parallel harness legal — sim
//! allows one simulated world at a time.

use std::sync::Mutex;

use mova::embed::{sim, Engine};

static SIM: Mutex<()> = Mutex::new(());

/// 2001-09-09T01:46:40Z — a date nobody could mistake for "now".
const EPOCH: u64 = 1_000_000_000_000;

fn engine() -> Engine {
    sim::enable(sim::SimOpts {
        seed: 0x5EED,
        epoch_ms: Some(EPOCH),
    })
    .expect("sim::enable before the first runtime use");
    Engine::builder().build()
}

/// The host turns sim on, then embeds exactly as it always did: build an
/// engine, eval script text. What changes is that the script's clock, its
/// scheduler and its randomness are now the host's to reproduce.
#[test]
fn a_host_can_enable_sim_and_then_embed_normally() {
    let _g = SIM.lock().unwrap_or_else(|e| e.into_inner());
    let mut e = engine();
    assert!(sim::enabled());

    let v = e
        .eval(r#"(:virtual-ms (simulate {:seed 1} (fn [] (<!! (timeout 86400000)))))"#)
        .expect("eval");
    assert_eq!(
        v.as_i64(),
        Some(86_400_000),
        "one virtual DAY, and the assertion below says what it cost in wall time"
    );
    assert!(
        sim::virtual_ns() >= 86_400_000_000_000,
        "the process's virtual clock is cumulative and monotonic across calls"
    );
}

/// `epoch_ms` moves the whole process's default date, which a call inherits
/// unless it overrides with its own `:epoch-ms`.
#[test]
fn the_process_epoch_is_the_calls_default() {
    let _g = SIM.lock().unwrap_or_else(|e| e.into_inner());
    let mut e = engine();
    let v = e
        .eval(r#"(:result (simulate {:seed 1} (fn [] (time-ms))))"#)
        .expect("eval");
    let got = v.as_i64().expect("an int");
    assert!(
        got >= EPOCH as i64,
        "the simulated world starts at the epoch the HOST chose, not at the wall clock: {got}"
    );
}

/// Idempotent: a second `enable` in an already-simulating process is fine
/// (and a host that never checks `enabled()` first must not be punished).
#[test]
fn enabling_twice_is_not_an_error() {
    let _g = SIM.lock().unwrap_or_else(|e| e.into_inner());
    let _ = engine();
    sim::enable(sim::SimOpts {
        seed: 99,
        epoch_ms: None,
    })
    .expect("a second enable is a no-op, not a failure");
    assert!(sim::enabled());
}
