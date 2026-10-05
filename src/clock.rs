//! L5 / P6a — the virtual clock, the seeded RNG streams, and the sim trace.
//!
//! **This is PROBE code** (docs/L5-VIRTUAL-TIME-DESIGN.md §9, P6a). It is the
//! minimal deterministic-simulation kernel: one clock function, two seeded
//! SplitMix64 streams, and a line-oriented trace writer. Everything the
//! design doc lists as landing work — the API surface (`simulate`), the
//! wall-clock natives (`clock_epoch_ms`), the fence #2/#3 task-rerouting, the
//! `flow.rs`/`conc.rs` clock sweep — is deliberately ABSENT.
//!
//! ## Sim mode
//!
//! Sim turns on by whichever of two doors comes first:
//!
//! - `MOVA_SIM_SEED=<u64>` (decimal or `0x`-hex) in the environment, read
//!   once at the first [`sim_enabled`] call — the per-PROGRAM smoke lever
//!   (OWNER-BRIEF-L5, RULING 1);
//! - **programmatically**, by the first `(simulate {:seed s} f)` /
//!   `embed::sim::enable` in a process whose runtime is still untouched
//!   (L5/W4) — the contract-honoring surface.
//!
//! The two coexist: `simulate` re-seeds both streams per call anyway
//! ([`sim_call_begin`]), so an env seed only decides the schedule of whatever
//! runs OUTSIDE a `simulate` call. Once on, sim is on for the process's life
//! (design §2, "Ownership": `SHARDS`/`TIMER` are `OnceLock`s that are never
//! torn down, so pretending to un-sim a process would be a lie).
//!
//! ## The clock
//!
//! [`clock_now`] is THE clock for behaviour. Real mode is `Instant::now()`
//! plus one relaxed flag load. Sim mode is `SIM_ANCHOR + SIM_NOW_NS`, where
//! `SIM_ANCHOR` is one real `Instant` captured at sim init and `SIM_NOW_NS`
//! is a virtual-nanosecond counter that ONLY the sim scheduler's advance rule
//! writes (`runtime::sim_next_job` -> `builtins::async::sim_advance_and_fire`).
//! Because `Instant + Duration` is an `Instant`, every existing
//! `Instant`-typed field, deadline and comparison keeps working with no type
//! migration anywhere — design §2.
//!
//! ## The two streams
//!
//! Both are SplitMix64. The initial state of each is the seed XOR a distinct
//! ASCII salt, so a program that adds one `(rand)` call does not shift every
//! scheduling decision after it (design §3, "Seed plumbing"):
//!
//! - **schedule stream** — salt `0x5343_4845_445F_5354` (`"SCHED_ST"`).
//!   Consumed ONLY by the sim `next_job` pick. Single-consumer (the one sim
//!   shard thread), so its state is a plain relaxed `AtomicU64`.
//! - **user stream** — salt `0x5553_4552_5F53_5452` (`"USER_STR"`). Seeds
//!   every user-visible randomness consumer in sim: `alts!!`'s shuffle, the
//!   `rand` family, `clojure.math/random`, 0-arg `(java.util.Random.)` and
//!   `randomUUID`. One process-global state, drawn from via [`user_next`] —
//!   see that function's doc for why a single stream is sound at 1 shard.
//!   (L5/W3 landed this; P6a wired only `alts!!`.)

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------------------
// Mode + seed
// ---------------------------------------------------------------------------

/// Not yet decided: the next [`sim_enabled`] resolves it from the environment.
const MODE_UNKNOWN: u8 = 0;
/// Resolved OFF. Still flippable to [`MODE_ON`] by [`sim_enable`] — but ONLY
/// while `runtime::runtime_untouched()` holds, which is the whole of
/// `simulate`'s enablement precondition (design §2, "Ownership").
const MODE_OFF: u8 = 1;
/// Resolved ON. Irrevocable.
const MODE_ON: u8 = 2;

static SIM_MODE: AtomicU8 = AtomicU8::new(MODE_UNKNOWN);
static SIM_SEED: AtomicU64 = AtomicU64::new(0);

/// L5/W4: `simulate` may turn sim on PROGRAMMATICALLY, so the mode can no
/// longer be a `OnceLock<Option<u64>>` resolved at the first read — a process
/// that reads the clock once in real mode would then be barred from ever
/// simulating. It is an `AtomicU8` tri-state instead: `UNKNOWN` until the
/// first read resolves it from `MOVA_SIM_SEED`, and `OFF -> ON` exactly once
/// more if `simulate`/`embed::sim::enable` asks while the runtime is still
/// untouched. Real mode pays one relaxed load and a compare, which is what
/// the `OnceLock` peek cost before.
#[inline]
pub(crate) fn sim_enabled() -> bool {
    match SIM_MODE.load(Ordering::Relaxed) {
        MODE_ON => true,
        MODE_OFF => false,
        _ => sim_mode_resolve(),
    }
}

/// The cold arm of [`sim_enabled`]: read `MOVA_SIM_SEED` once and latch.
#[cold]
#[inline(never)]
fn sim_mode_resolve() -> bool {
    // The env read races only with itself (every racer computes the same
    // answer) and with `sim_enable`, whose caller has proved the runtime is
    // untouched — i.e. this process has one live thread's worth of relevant
    // activity at that instant. A `Mutex` here would put a lock on the clock
    // path for a decision that is made once.
    static RESOLVED: OnceLock<Option<u64>> = OnceLock::new();
    let seed = *RESOLVED.get_or_init(|| {
        let raw = std::env::var("MOVA_SIM_SEED").ok()?;
        let t = raw.trim();
        let parsed = match t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
            Some(hex) => u64::from_str_radix(hex, 16),
            None => t.parse::<u64>(),
        };
        match parsed {
            Ok(s) => Some(s),
            Err(e) => {
                eprintln!("mova sim: MOVA_SIM_SEED={raw:?} is not a u64 ({e}); sim mode OFF");
                None
            }
        }
    });
    match seed {
        Some(s) => {
            // Only the FIRST resolver arms the streams/trace; a racer that
            // finds the mode already ON skips straight out.
            if SIM_MODE
                .compare_exchange(MODE_UNKNOWN, MODE_ON, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                arm_sim(s);
            }
            true
        }
        None => {
            let _ = SIM_MODE.compare_exchange(
                MODE_UNKNOWN,
                MODE_OFF,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
            SIM_MODE.load(Ordering::Relaxed) == MODE_ON
        }
    }
}

/// Turn sim on programmatically (`simulate`'s first call, or
/// `embed::sim::enable`). The CALLER owns the precondition — sim must be the
/// process's first runtime use — because only `runtime` can answer it; see
/// `runtime::runtime_untouched` and `builtins::sim::simulate`.
///
/// Idempotent: enabling an already-sim process is a no-op (the per-call
/// re-seed in [`sim_call_begin`] is what actually decides a call's schedule).
pub(crate) fn sim_enable(seed: u64) {
    // Resolve the env arm first so an `MOVA_SIM_SEED` process and a
    // `simulate` process take the identical arming path exactly once.
    if sim_enabled() {
        return;
    }
    SIM_MODE.store(MODE_ON, Ordering::Release);
    arm_sim(seed);
}

/// Everything that must be true the instant sim mode becomes ON: the anchor
/// is captured, the two streams are seeded, the epoch window is the
/// whole-process default, and `MOVA_SIM_TRACE` (if set) is open.
fn arm_sim(seed: u64) {
    SIM_SEED.store(seed, Ordering::Relaxed);
    let _ = sim_anchor();
    reseed_streams(seed);
    if let Some(ms) = std::env::var("MOVA_SIM_EPOCH_MS")
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
    {
        SIM_EPOCH_PROCESS_BASE_MS.store(ms, Ordering::Relaxed);
    }
    SIM_EPOCH_CALL_BASE_MS.store(sim_epoch_base_ms(), Ordering::Relaxed);
    SIM_EPOCH_CALL_START_NS.store(0, Ordering::Relaxed);
    if let Ok(path) = std::env::var("MOVA_SIM_TRACE") {
        match open_trace(&path) {
            Ok(w) => {
                let mut g = TRACE_W.lock().unwrap_or_else(|e| e.into_inner());
                *g = Some(w);
                TRACE_ACTIVE.store(true, Ordering::Relaxed);
            }
            Err(e) => eprintln!("mova sim: couldn't open trace {path:?}: {e}"),
        }
    }
}

/// The seed. Only meaningful when [`sim_enabled`].
pub(crate) fn sim_seed() -> u64 {
    SIM_SEED.load(Ordering::Relaxed)
}

/// **L5/W5 kernel fix** — bumped once by every [`sim_call_begin`], never
/// anywhere else. `0` is never returned by a bump (the first bump lands on
/// `1`), so it doubles as an "unseeded" sentinel for consumers that have
/// never compared against it.
///
/// The two `AtomicU64` streams above are re-seeded IN PLACE by
/// `sim_call_begin`, which is enough for `sched_next`/`user_next` (they read
/// the shared state on every draw). It is NOT enough for the PRNG consumers
/// in `builtins::random`/`builtins::math`/`builtins::async`, each of which
/// keeps its own thread-local xorshift64 state, seeded lazily once from a
/// draw on the user stream and then never revisited — a `simulate` call has
/// no way to reach into another thread's TLS to reset it. This counter is
/// the cross-thread signal: each thread-local remembers the generation it
/// was last (re)seeded at, and reseeds itself (from a fresh
/// [`user_next_nonzero`] draw) whenever it notices [`sim_call_gen`] has
/// moved on. Real mode never reads this — every call site is behind the
/// `sim_enabled()` branch each consumer already had.
static SIM_CALL_GEN: AtomicU64 = AtomicU64::new(0);

/// See [`SIM_CALL_GEN`].
pub(crate) fn sim_call_gen() -> u64 {
    SIM_CALL_GEN.load(Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// The clock
// ---------------------------------------------------------------------------

static SIM_ANCHOR: OnceLock<Instant> = OnceLock::new();
static SIM_NOW_NS: AtomicU64 = AtomicU64::new(0);

/// The one real `Instant` a whole simulation is measured from.
fn sim_anchor() -> Instant {
    *SIM_ANCHOR.get_or_init(Instant::now)
}

/// THE clock. Real mode: `Instant::now()`. Sim mode: `anchor + virtual_ns`.
#[inline]
pub(crate) fn clock_now() -> Instant {
    if sim_enabled() {
        sim_anchor() + Duration::from_nanos(SIM_NOW_NS.load(Ordering::Relaxed))
    } else {
        Instant::now()
    }
}

/// `System/nanoTime`: an arbitrary-origin, monotonically nondecreasing
/// nanosecond count -- the JVM's own contract says nothing about epoch,
/// only that two calls' DIFFERENCE is meaningful, so `clock_now() -
/// sim_anchor()` (real `Instant::now()` minus this process's own start
/// anchor in real mode; virtual elapsed ns in sim mode) satisfies it in
/// both modes.
pub(crate) fn clock_nano_time() -> i64 {
    clock_now().saturating_duration_since(sim_anchor()).as_nanos() as i64
}

/// Virtual nanoseconds elapsed in this simulation (0 in real mode).
pub(crate) fn sim_now_ns() -> u64 {
    SIM_NOW_NS.load(Ordering::Relaxed)
}

/// The sim's default wall-clock epoch, in milliseconds since the Unix epoch:
/// 2026-01-01T00:00:00Z. "What day is it" is part of the seed's contract
/// (design §2) -- a program that reads `(time-ms)` or constructs a no-arg
/// `Date` gets a fixed, reproducible answer under sim, not the wall clock at
/// the moment the test happened to run. Overridable via `MOVA_SIM_EPOCH_MS`
/// for the rare program that cares about a specific date; the `simulate`
/// API's `:epoch-ms` opt supersedes this env lever at W4.
const SIM_EPOCH_BASE_MS: u64 = 1_767_225_600_000;

/// The PROCESS-wide epoch base: [`SIM_EPOCH_BASE_MS`], overridden by
/// `MOVA_SIM_EPOCH_MS` at arming and by [`sim_set_process_epoch_ms`]
/// (`embed::sim::enable`'s `epoch_ms`) after it. A `simulate` call's
/// `:epoch-ms` opt overrides it for that call only.
static SIM_EPOCH_PROCESS_BASE_MS: AtomicU64 = AtomicU64::new(SIM_EPOCH_BASE_MS);

fn sim_epoch_base_ms() -> u64 {
    SIM_EPOCH_PROCESS_BASE_MS.load(Ordering::Relaxed)
}

/// `embed::sim::enable`'s `epoch_ms`: move the whole process's default epoch,
/// and re-open the current window on it. Called before the first `simulate`,
/// so "the current window" is the whole-process one.
pub(crate) fn sim_set_process_epoch_ms(ms: u64) {
    SIM_EPOCH_PROCESS_BASE_MS.store(ms, Ordering::Relaxed);
    SIM_EPOCH_CALL_BASE_MS.store(ms, Ordering::Relaxed);
    SIM_EPOCH_CALL_START_NS.store(sim_now_ns(), Ordering::Relaxed);
}

/// The epoch WINDOW currently in force: `epoch_ms` reads
/// `base + (virtual_now - start)`.
///
/// Outside a `simulate` call (`MOVA_SIM_SEED` whole-process runs, and the
/// gaps between calls) it is `(sim_epoch_base_ms(), 0)` — i.e. exactly the
/// pre-W4 behaviour, base plus total virtual ms. A `simulate` call with
/// `:epoch-ms e` installs `(e, call_start_ns)` for its duration, so the call
/// starts on the date it asked for and time moves forward from there; a call
/// WITHOUT `:epoch-ms` installs `(base + call_start_ms, call_start_ns)`,
/// which is the same number the process-wide formula would have produced —
/// the process default, continued (design §2 / OWNER-BRIEF-L5 RULING 1).
static SIM_EPOCH_CALL_BASE_MS: AtomicU64 = AtomicU64::new(SIM_EPOCH_BASE_MS);
static SIM_EPOCH_CALL_START_NS: AtomicU64 = AtomicU64::new(0);

/// THE epoch clock (design §2): "what time is it" for BEHAVIOR that wants a
/// wall-clock millisecond count -- `time-ms`, `System/currentTimeMillis`, the
/// 0-arg `(java.util.Date.)` constructor. Real mode: `SystemTime::now()`
/// since `UNIX_EPOCH`, in milliseconds (same pattern every wall-clock native
/// used before the sweep). Sim mode: the current epoch WINDOW's base plus the
/// virtual milliseconds elapsed inside it, so "what day is it" is
/// deterministic and a function of the seed, the `:epoch-ms` opt (or the
/// `MOVA_SIM_EPOCH_MS` env override) alone.
pub(crate) fn clock_epoch_ms() -> u64 {
    if sim_enabled() {
        let base = SIM_EPOCH_CALL_BASE_MS.load(Ordering::Relaxed);
        let start = SIM_EPOCH_CALL_START_NS.load(Ordering::Relaxed);
        base.saturating_add(sim_now_ns().saturating_sub(start) / 1_000_000)
    } else {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}

/// **L5/W4 — the per-`simulate`-call reset.** Called on the boundary thread
/// with the sim world quiescent (previous call fully drained, shard parked,
/// root not yet spawned), so nothing is drawing from a stream or reading the
/// epoch window while these stores land.
///
/// Two things happen, and only these two: both SplitMix64 streams are
/// RE-SEEDED from this call's seed, and the epoch window is installed. The
/// virtual clock itself is deliberately NOT reset — `Instant` anchors handed
/// out by an earlier call must never go backwards, so virtual time is
/// monotonic for the whole process and a call's `:virtual-ms` is a DELTA
/// against the `call_start_ns` returned here (design §2; owner refinement
/// (a): the seed sweep is the headline idiom, and it must be one process).
pub(crate) fn sim_call_begin(seed: u64, epoch_ms: Option<u64>) -> u64 {
    let call_start_ns = sim_now_ns();
    SIM_SEED.store(seed, Ordering::Relaxed);
    reseed_streams(seed);
    // The `AtomicU64` streams above are reseeded right here, in place, so any
    // consumer reading them mid-draw gets this call's chain. The PRNG
    // CONSUMERS (random.rs/math.rs/async.rs) do not read the streams
    // directly per draw, though -- each keeps its own thread-local xorshift64
    // state, lazily seeded ONCE from a draw on the user stream and never
    // revisited. That is a second, independent piece of per-thread state this
    // reseed does not reach, and re-arming it from another thread is not
    // possible (TLS is thread-owned). The generation counter is the fix:
    // bumping it here is a signal every thread-local can compare against on
    // its own next use, and reseed itself the same way `arm_sim` did the
    // first time -- see `sim_call_gen`.
    SIM_CALL_GEN.fetch_add(1, Ordering::Relaxed);
    let base = epoch_ms.unwrap_or_else(|| sim_epoch_base_ms().saturating_add(call_start_ns / 1_000_000));
    SIM_EPOCH_CALL_BASE_MS.store(base, Ordering::Relaxed);
    SIM_EPOCH_CALL_START_NS.store(call_start_ns, Ordering::Relaxed);
    call_start_ns
}

/// Undo [`sim_call_begin`]'s epoch window: back to the whole-process default,
/// so a `(time-ms)` read BETWEEN calls means the same thing it meant before
/// any call happened.
pub(crate) fn sim_call_end() {
    SIM_EPOCH_CALL_BASE_MS.store(sim_epoch_base_ms(), Ordering::Relaxed);
    SIM_EPOCH_CALL_START_NS.store(0, Ordering::Relaxed);
    // A call boundary is the natural flush point for whatever trace is
    // installed: the world is quiescent, so the file is complete AS OF a
    // meaningful instant, and a driver that never gets to call `trace_flush`
    // (the CLI, an embedder, a test) still finds every finished call on disk.
    // One `write` syscall per `simulate`, against a call that just simulated
    // a program.
    trace_flush();
}

/// R2 (design §8): `Instant + 292 years` overflows on some platforms.
/// 100 virtual years is the cap; the 30-day probe clears it by 3 orders.
const SIM_MAX_NS: u64 = 100 * 365 * 24 * 60 * 60 * 1_000_000_000;

/// Jump virtual time to `target`. Called ONLY by the sim advance rule, on
/// the sim shard thread, with no lock held that a trace write could invert.
/// Monotonic by construction (the target is the earliest live deadline, and
/// every deadline was armed at a `clock_now()` no later than the current
/// virtual now) — the `max` is belt and braces, and the debug assert is the
/// braces.
pub(crate) fn sim_jump_to(target: Instant) {
    let ns = target.saturating_duration_since(sim_anchor()).as_nanos();
    assert!(
        ns <= u128::from(SIM_MAX_NS),
        "mova sim: virtual time ran past the 100-year cap (design §8 R2)"
    );
    let ns = ns as u64;
    let prev = SIM_NOW_NS.load(Ordering::Relaxed);
    debug_assert!(ns >= prev, "mova sim: virtual time went backwards");
    if ns > prev {
        SIM_NOW_NS.store(ns, Ordering::Relaxed);
        trace_clock(ns);
    }
}

// ---------------------------------------------------------------------------
// The two SplitMix64 streams
// ---------------------------------------------------------------------------

const SCHED_SALT: u64 = 0x5343_4845_445F_5354; // b"SCHED_ST"
const USER_SALT: u64 = 0x5553_4552_5F53_5452; // b"USER_STR"

#[inline]
fn splitmix64(state: u64) -> (u64, u64) {
    let s = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = s;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    (s, z ^ (z >> 31))
}

/// Schedule-stream state. Single consumer (the sim shard thread), so a
/// relaxed load/store pair is the honest cost; `AtomicU64` only because it
/// has to be a `static`.
static SCHED_STATE: AtomicU64 = AtomicU64::new(0);

/// Seed (or RE-seed) both streams. Called at sim arming and once per
/// `simulate` call — see [`sim_call_begin`] for why re-seeding per call is
/// what makes `(doseq [s (range 100)] (simulate {:seed s} f))` a sweep of 100
/// independent schedules rather than one 100-segment schedule.
fn reseed_streams(seed: u64) {
    SCHED_STATE.store(seed ^ SCHED_SALT, Ordering::Relaxed);
    USER_STATE.store(seed ^ USER_SALT, Ordering::Relaxed);
}

/// Next draw from the schedule stream. Consumed ONLY by the sim `next_job`
/// pick — nothing else may take from this stream, or the schedule stops
/// being a function of the schedule seed alone.
pub(crate) fn sched_next() -> u64 {
    let (next_state, out) = splitmix64(SCHED_STATE.load(Ordering::Relaxed));
    SCHED_STATE.store(next_state, Ordering::Relaxed);
    out
}

/// User-stream state. See [`user_next`].
static USER_STATE: AtomicU64 = AtomicU64::new(0);

/// **Fence #8 (design §4, L5/W3): the one seeded user stream.**
///
/// Every user-visible randomness consumer in the process draws from HERE in
/// sim mode, and from nowhere else:
///
/// - `builtins::async::next_rand` — `alts!!`'s per-pass op shuffle
/// - `builtins::random::next_u64` — `rand`/`rand-int`/`rand-nth`/`shuffle`
/// - `builtins::math::next_rand_bits` — `clojure.math/random`
/// - `hostclass::random::time_seed` — 0-arg `(java.util.Random.)`
/// - `value::Value::random_uuid_bits` — `(java.util.UUID/randomUUID)`
///
/// Each of those keeps its own cheap thread-local generator (real mode is
/// untouched, to the instruction); what changes in sim is only the SEED that
/// generator starts from — it comes from this stream instead of the wall
/// clock. Since after fences #1–#4 every one of those consumers runs on the
/// single sim shard thread in-contract (design §5), the draws are TOTALLY
/// ORDERED and the whole assignment is a function of the seed alone.
///
/// Deliberately NOT the schedule stream (`sched_next`): a program that adds
/// one `(rand)` call must not shift every scheduling decision after it
/// (design §3, "Seed plumbing").
///
/// Never called in real mode — every call site guards on
/// [`sim_enabled`] first, so a real-mode `rand` pays exactly one relaxed
/// flag load on its FIRST call and nothing thereafter.
pub(crate) fn user_next() -> u64 {
    let (next_state, out) = splitmix64(USER_STATE.load(Ordering::Relaxed));
    USER_STATE.store(next_state, Ordering::Relaxed);
    out
}

/// [`user_next`] forced non-zero — the form every xorshift64 consumer needs
/// (a zero state is xorshift's fixed point).
pub(crate) fn user_next_nonzero() -> u64 {
    user_next() | 1
}

// ---------------------------------------------------------------------------
// The trace
// ---------------------------------------------------------------------------

use std::fs::File;
use std::io::{BufWriter, Write};
use std::sync::Mutex;

/// `MOVA_SIM_TRACE=<path>`: one line per scheduler event, appended by the
/// sim shard thread. Never opened in real mode.
///
/// Line grammar (design §3, "The trace"):
///
/// ```text
/// S <task-id>          spawn placed into the slab
/// R <task-id>          resume
/// P <task-id>          park
/// X <task-id>          done / panicked / killed
/// C <virtual-ns>       clock advance
/// F <timer-seq> <C|W>  timer fire (Close | Wake)
/// ```
///
/// **No `D` (deadlock) line.** The design sketches one at the park site; P6a
/// deliberately omits it, because the park site is reached BEFORE the root
/// spawn arrives (the shard thread starts inside `shards()`, the boundary
/// thread pushes afterwards) and AFTER the driver has taken the done cell —
/// both of which are genuine wall-clock races between the shard and the
/// boundary thread. Writing them into the trace would put a race INTO the
/// determinism artifact. The tripwire is landing work (design §6), where it
/// is a panic/error value rather than a trace line.
///
/// **L5/W4 — the writer is per-CALL, not per-process.** It was a
/// `OnceLock<Option<...>>` resolved from `MOVA_SIM_TRACE` at the first emit;
/// `simulate`'s `:trace` opt needs a file opened and closed around ONE call,
/// so it is a plain `Mutex<Option<..>>` with a stack discipline
/// ([`trace_push`]/[`trace_pop`]). PRECEDENCE: `MOVA_SIM_TRACE` opens a
/// whole-process trace when sim is armed; a call's `:trace` REPLACES it for
/// the duration of that call (its events go to the call's file, not the
/// process's) and the process trace is restored, still open, afterwards.
static TRACE_W: Mutex<Option<BufWriter<File>>> = Mutex::new(None);

/// `TRACE_W.is_some()`, as a lock-free flag: the guard every emit site pays
/// before it formats anything, and the one the shard loop re-reads per hop in
/// sim. Never true in real mode (nothing but [`arm_sim`]/[`trace_push`] sets
/// it, and both require sim).
static TRACE_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Lines written. The probe driver waits for this to go quiet before it
/// flushes, so the file is complete without needing a wall-clock guess.
static TRACE_EVENTS: AtomicU64 = AtomicU64::new(0);

fn open_trace(path: &str) -> std::io::Result<BufWriter<File>> {
    Ok(BufWriter::with_capacity(1 << 16, File::create(path)?))
}

/// A trace writer displaced by [`trace_push`], to be restored by
/// [`trace_pop`]. Opaque on purpose: only `builtins::sim` holds one, and only
/// across a single `simulate` call.
pub(crate) struct TraceSlot(Option<BufWriter<File>>);

/// Install `path` as THE trace for the next stretch, returning whatever was
/// installed before (flushed first, so the displaced file is complete on disk
/// at the moment it stops receiving lines).
pub(crate) fn trace_push(path: &str) -> std::io::Result<TraceSlot> {
    let w = open_trace(path)?;
    let mut g = TRACE_W.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(prev) = g.as_mut() {
        let _ = prev.flush();
    }
    let prev = g.replace(w);
    TRACE_ACTIVE.store(true, Ordering::Relaxed);
    Ok(TraceSlot(prev))
}

/// Flush and drop the current trace, restoring `saved`.
pub(crate) fn trace_pop(saved: TraceSlot) {
    let mut g = TRACE_W.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(cur) = g.as_mut() {
        let _ = cur.flush();
    }
    *g = saved.0;
    TRACE_ACTIVE.store(g.is_some(), Ordering::Relaxed);
}

/// True when a trace is being written. ONE relaxed static load.
#[inline]
pub(crate) fn trace_on() -> bool {
    TRACE_ACTIVE.load(Ordering::Relaxed)
}

fn emit(args: std::fmt::Arguments<'_>) {
    if !trace_on() {
        return;
    }
    let mut g = TRACE_W.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(w) = g.as_mut() {
        let _ = w.write_fmt(args);
        TRACE_EVENTS.fetch_add(1, Ordering::Relaxed);
    }
}

/// `S`/`R`/`P`/`X` — the four task-lifecycle events, one letter each.
#[inline]
pub(crate) fn trace_task(kind: char, id: u64) {
    emit(format_args!("{kind} {id}\n"));
}

#[inline]
fn trace_clock(ns: u64) {
    emit(format_args!("C {ns}\n"));
}

/// `F <seq> <C|W>` — one timer entry fired.
#[inline]
pub(crate) fn trace_timer_fire(seq: u64, kind: char) {
    emit(format_args!("F {seq} {kind}\n"));
}

/// Lines emitted so far. Monotonic; the driver polls it for quiescence.
pub(crate) fn trace_events() -> u64 {
    TRACE_EVENTS.load(Ordering::Relaxed)
}

/// Flush the trace to disk. The probe driver calls this before it exits —
/// a `BufWriter` in a `static` is never dropped, so nothing else would.
pub(crate) fn trace_flush() {
    let mut g = TRACE_W.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(w) = g.as_mut() {
        let _ = w.flush();
    }
}
