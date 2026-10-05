//! E3 (V05-PERF-PLAN) probe: `MOVA_MAP_PROBE=1`-gated instrumentation
//! answering "how big are the maps mova actually touches, and how often
//! does the flow engine hand a step-fn a state map it turns out not to
//! change?" -- the two measurable proxies recon substituted for
//! `Arc::strong_count==1` hit-rate once recon found `Value::Map`'s `Big`
//! tier holds a HAMT directly (`imbl::HashMap` pre-M5, `champ::
//! PersistentHashMap` since -- either way, that HAMT refcounts its own
//! internal nodes/chunks, and that count isn't exposed to callers).
//!
//! Zero cost when the env var is unset: [`enabled`] is an `#[inline]`
//! wrapper around a `OnceLock<bool>` that's already initialized by the time
//! any hot path calls it (first call, from wherever happens to run first),
//! so every subsequent check is a single atomic load -- no branch on
//! `env::var` (which would itself lock), no allocation. Every call site in
//! `collections.rs`/`eval/apply.rs`/`eval/special_forms.rs`/`builtins/
//! flow.rs` guards its real work behind this same check, so with the probe
//! off the entire module costs one `Ordering::Acquire` load per touch site
//! and nothing else.
//!
//! With it on, [`record`] takes a `Mutex<HashMap<..>>` lock per call --
//! fine for a measurement run (this is instrumentation, not something
//! meant to survive into a release build's hot path) but why the whole
//! thing stays behind the flag.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

static ENABLED: OnceLock<bool> = OnceLock::new();

/// `true` iff `MOVA_MAP_PROBE` is set (to anything) in the environment.
/// Checked once per process; every call after the first is a single atomic
/// load behind this `#[inline(always)]` wrapper -- forced (not just
/// hinted) so every one of `collections.rs`'s ~15 hot call sites collapses
/// to the bare atomic load + branch instead of a real call/ret (measured:
/// `map_probe::record` self-samples in `kondo-walk`'s profile).
#[inline(always)]
pub fn enabled() -> bool {
    *ENABLED.get_or_init(|| std::env::var_os("MOVA_MAP_PROBE").is_some())
}

/// `[0, 1-2, 3-4, 5-8, 9-16, 17-64, >64]` -- deliberately straddling
/// Clojure's real array-map/HAMT switchover point (8 entries) so the
/// histogram directly answers "how much of the touched mass falls under
/// that threshold".
const BUCKET_LABELS: [&str; 7] = ["0", "1-2", "3-4", "5-8", "9-16", "17-64", ">64"];

fn bucket_of(len: usize) -> usize {
    match len {
        0 => 0,
        1..=2 => 1,
        3..=4 => 2,
        5..=8 => 3,
        9..=16 => 4,
        17..=64 => 5,
        _ => 6,
    }
}

type Table = Mutex<HashMap<&'static str, [u64; 7]>>;

fn table() -> &'static Table {
    static TABLE: OnceLock<Table> = OnceLock::new();
    TABLE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Records one touch of a map-shaped receiver at `op` (a touch-site label,
/// e.g. `"assoc"`/`"get"`) with `len` entries, bucketed. No-op with the
/// probe off (a single relaxed-ish atomic load via [`enabled`]).
#[inline(always)]
pub fn record(op: &'static str, len: usize) {
    if !enabled() {
        return;
    }
    let b = bucket_of(len);
    let mut t = table().lock().expect("map_probe table poisoned");
    t.entry(op).or_insert([0u64; 7])[b] += 1;
}

// --- (C): flow proc state identity -----------------------------------
//
// `process_message_to` (builtins/flow.rs) calls each step-fn with the
// proc's current state map and gets back `[state' outs]`. `state'` is
// "unchanged" when it's the SAME `imbl::HashMap` allocation as the state
// that went in (`imbl::HashMap::ptr_eq`) -- i.e. the step-fn returned its
// input state verbatim rather than producing a fresh assoc'd map. Every
// "changed" count is a message that paid (B)'s shared-assoc cost at least
// once.

static STATE_UNCHANGED: AtomicU64 = AtomicU64::new(0);
static STATE_CHANGED: AtomicU64 = AtomicU64::new(0);

#[inline]
pub fn record_state_identity(unchanged: bool) {
    if !enabled() {
        return;
    }
    if unchanged {
        STATE_UNCHANGED.fetch_add(1, Ordering::Relaxed);
    } else {
        STATE_CHANGED.fetch_add(1, Ordering::Relaxed);
    }
}

// --- (D): consuming-path unique-hit rate ------------------------------
//
// E3's ORIGINAL question ("how often is the mutated structure's refcount
// 1?") was unanswerable back then: every builtin received `&[Value]`, so
// the refcount at the mutation instant was structurally >= 2 and the
// question had a constant answer. Perceus-lite phase 1
// (`builtins::reuse`) makes it a real question, and this is where it gets
// answered in PRODUCTION rather than in a microbench: every call that
// reaches a consuming entry point records whether the handle it just took
// ownership of was in fact the only one.
//
// MEASUREMENT LIMIT, stated rather than papered over: exact only for the
// `Small` tiers (`PMap::Small`/`PVec::Small`, i.e. maps <=8 and vectors
// <=16 -- which E3(A) showed is >99.99% of mova's real touched mass).
// A `Big` receiver's imbl root refcount is private, unreachable through
// imbl's public API, and cannot be sampled by cloning without destroying
// the uniqueness being sampled -- so `Big` calls are counted in their own
// column and NOT guessed at. Uniqueness is a property of the CALLER's
// expression shape (temporary vs still-live binding), not of the tier, so
// the Small column is representative of both; the Big column exists to
// show how much of the traffic that inference is covering.

#[derive(Default, Clone, Copy)]
struct ReuseRow {
    unique: u64,
    shared: u64,
    big: u64,
}

type ReuseTable = Mutex<HashMap<&'static str, ReuseRow>>;

fn reuse_table() -> &'static ReuseTable {
    static TABLE: OnceLock<ReuseTable> = OnceLock::new();
    TABLE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Records one consuming-path call at `op`. `sample` is invoked ONLY when
/// the probe is on, and should be `PMap::small_is_unique`/
/// `PVec::small_is_unique`: `Some(true)` the handle taken was the only one
/// (the mutation ran in place), `Some(false)` something else still held it
/// (copy-on-write, i.e. exactly the old behaviour), `None` a `Big`
/// receiver whose refcount is not observable.
///
/// TAKING A CLOSURE IS DELIBERATE, not style. `small_is_unique` reads
/// `Arc::strong_count`/`weak_count` -- two Acquire atomic loads on a hot,
/// possibly cross-core cache line. Passed as an eagerly evaluated ARGUMENT
/// (the obvious spelling) they would be paid on every consuming call even
/// with `MOVA_MAP_PROBE` unset, breaking this module's stated
/// zero-cost-when-off contract; behind the closure the whole sample folds
/// away. (Honesty note: an early non-interleaved A/B appeared to show this
/// costing ~18%; a properly interleaved same-binary run showed that
/// difference was machine noise, not the atomics. The closure is kept
/// because the contract is right, not because a regression was measured.)
#[inline]
pub fn record_reuse(op: &'static str, sample: impl FnOnce() -> Option<bool>) {
    if !enabled() {
        return;
    }
    let unique = sample();
    let mut t = reuse_table().lock().expect("map_probe reuse table poisoned");
    let row = t.entry(op).or_default();
    match unique {
        Some(true) => row.unique += 1,
        Some(false) => row.shared += 1,
        None => row.big += 1,
    }
}

/// Total consuming-path calls recorded so far, over every op. Used by the
/// kill-switch test to prove the fast path is actually reached (a silently
/// dead switch, or a silently dead fast path, must fail loudly rather than
/// merely produce the right answers slowly).
pub fn reuse_call_count() -> u64 {
    reuse_table()
        .lock()
        .expect("map_probe reuse table poisoned")
        .values()
        .map(|r| r.unique + r.shared + r.big)
        .sum()
}

/// Unique-hit count for one op (`Small` tier only -- see (D)'s note).
pub fn reuse_unique_count(op: &str) -> u64 {
    reuse_table()
        .lock()
        .expect("map_probe reuse table poisoned")
        .get(op)
        .map_or(0, |r| r.unique)
}

/// Prints both tables to stderr. No-op with the probe off. Called once,
/// from `main.rs`'s run paths, after the interpreter has finished running
/// the program (so any `flow/stop`-joined proc threads have already
/// settled their counters).
pub fn print_report() {
    if !enabled() {
        return;
    }
    let t = table().lock().expect("map_probe table poisoned");
    eprintln!();
    eprintln!("=== MOVA_MAP_PROBE (A): touch-site frequency + map-size histogram ===");
    eprint!("{:<18}", "op");
    for label in BUCKET_LABELS {
        eprint!(" {label:>8}");
    }
    eprintln!(" {:>10}", "total");
    let mut ops: Vec<&&str> = t.keys().collect();
    ops.sort();
    if ops.is_empty() {
        eprintln!("(no map-touching builtin was called)");
    }
    for op in ops {
        let row = t[op];
        let total: u64 = row.iter().sum();
        eprint!("{op:<18}");
        for count in row {
            eprint!(" {count:>8}");
        }
        eprintln!(" {total:>10}");
    }

    let r = reuse_table().lock().expect("map_probe reuse table poisoned");
    eprintln!();
    eprintln!("=== MOVA_MAP_PROBE (D): consuming-path unique-hit rate ===");
    eprintln!("(unique/shared exact for Small tiers only; Big = imbl root refcount not observable)");
    eprintln!(
        "{:<18} {:>10} {:>10} {:>10} {:>10} {:>12}",
        "op", "unique", "shared", "big(n/a)", "total", "unique%(small)"
    );
    let mut rops: Vec<&&str> = r.keys().collect();
    rops.sort();
    if rops.is_empty() {
        eprintln!("(no consuming-path call was made -- reuse off, or no whitelisted builtin ran)");
    }
    for op in rops {
        let row = r[op];
        let small = row.unique + row.shared;
        let total = small + row.big;
        let pct = if small == 0 {
            "n/a".to_string()
        } else {
            format!("{:.2}", 100.0 * row.unique as f64 / small as f64)
        };
        eprintln!(
            "{op:<18} {:>10} {:>10} {:>10} {:>10} {pct:>12}",
            row.unique, row.shared, row.big, total
        );
    }
    drop(r);

    let unchanged = STATE_UNCHANGED.load(Ordering::Relaxed);
    let changed = STATE_CHANGED.load(Ordering::Relaxed);
    let total = unchanged + changed;
    eprintln!();
    eprintln!("=== MOVA_MAP_PROBE (C): flow proc state identity (state' vs state) ===");
    if total == 0 {
        eprintln!("(no flow proc transform calls observed)");
    } else {
        eprintln!(
            "unchanged(ptr_eq)={unchanged:>10}  changed={changed:>10}  total={total:>10}  unchanged%={:.2}",
            100.0 * unchanged as f64 / total as f64
        );
    }
    eprintln!();
}
