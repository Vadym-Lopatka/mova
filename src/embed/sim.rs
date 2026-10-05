//! Deterministic simulation, from the host side (L5/W4 — see
//! `docs/L5-VIRTUAL-TIME-DESIGN.md`).
//!
//! # Why this is a free function and not `EngineBuilder::sim(..)`
//!
//! **Sim mode is a property of the PROCESS, not of an `Engine`.** It forces
//! the task runtime onto one shard, disables the direct-switch slot, replaces
//! the timer thread with an inline drain on that shard, and swaps the clock —
//! all of which are `OnceLock`-shaped decisions made at the runtime's first
//! use and shared by every `Engine` in the process (design §2, "Ownership").
//! A builder method would say the opposite: that two engines in one process
//! could disagree, or that a later `build()` could change an earlier one's
//! world. It would also have to be infallible (`EngineBuilder::build` returns
//! an `Engine`, not a `Result`), and enabling sim genuinely CAN fail — after
//! the runtime has started, the honest answer is an error, not a panic and
//! not a silent no-op.
//!
//! So the surface is one fallible, process-scoped call, made BEFORE the first
//! `Engine` does anything with tasks:
//!
//! ```no_run
//! # fn main() -> Result<(), mova::embed::Error> {
//! use mova::embed::{sim, Engine};
//!
//! sim::enable(sim::SimOpts { seed: 0x5EED, epoch_ms: None })?;
//! let mut engine = Engine::builder().build();
//! let report = engine.eval(r#"(simulate {:seed 0x5EED} (fn [] (<! (timeout 30000))))"#)?;
//! # let _ = report;
//! # Ok(())
//! # }
//! ```
//!
//! Everything a run reports — `:result`, `:virtual-ms`, `:resumes`,
//! `:timer-fires`, `:leaked-tasks` — comes back through the `simulate`
//! native's result map, because the thing being simulated is a Mova program
//! and its root has to be a Mova thunk.

use crate::embed::Error;

/// How a simulated world starts.
#[derive(Clone, Copy, Debug)]
pub struct SimOpts {
    /// The seed. THE reproducibility contract: same binary + same program +
    /// same seed => same interleaving => same answer.
    pub seed: u64,
    /// Wall-clock epoch, in milliseconds since the Unix epoch, that the
    /// simulated world starts at — what `(time-ms)` and a 0-arg `(Date.)`
    /// report. `None` keeps the built-in fixed default (2026-01-01T00:00:00Z),
    /// so "what day is it" is part of the seed's contract either way.
    ///
    /// Per-CALL overrides ride on `simulate`'s `:epoch-ms` opt; this is the
    /// process default the calls start from.
    pub epoch_ms: Option<u64>,
}

/// Put this process into deterministic-simulation mode.
///
/// **Must be the process's first runtime use.** Returns an error naming
/// `SIM-E-RUNTIME-STARTED` if the task runtime (or the real-mode timer
/// thread) has already started — see the module doc for why that is a real
/// constraint and not a tidiness rule. Idempotent, and a no-op in a process
/// already in sim mode via `MOVA_SIM_SEED`.
pub fn enable(opts: SimOpts) -> Result<(), Error> {
    crate::builtins::sim::enable_process_sim(opts.seed).map_err(Error::other)?;
    if let Some(ms) = opts.epoch_ms {
        crate::clock::sim_set_process_epoch_ms(ms);
    }
    Ok(())
}

/// Is this process running the deterministic simulation scheduler?
pub fn enabled() -> bool {
    crate::clock::sim_enabled()
}

/// Virtual nanoseconds elapsed in this process's simulation (0 in real mode).
///
/// Process-cumulative and monotonic: virtual time is never reset between
/// `simulate` calls, because `Instant`s minted from it escape into timer
/// deadlines and user data. A single call's elapsed virtual time is that
/// call's `:virtual-ms`, not a difference of two reads of this.
pub fn virtual_ns() -> u64 {
    crate::clock::sim_now_ns()
}
