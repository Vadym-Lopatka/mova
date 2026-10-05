//! PERF-PROBE instrumentation: per-namespace load timing + RSS delta, and
//! coarse read/eval phase totals, gated by `MOVA_LOAD_TRACE=1`. Zero cost
//! when unset: every call site below is a single `enabled()` bool check
//! (a `OnceLock` read, effectively a predictable branch) before any work;
//! no field is added to `Interp`, no data is collected, nothing is
//! printed. Written for the clojure-lsp-on-mova startup probe
//! (`PERF-PROBE.md`); not a public API, not stabilized.
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

static ENABLED: OnceLock<bool> = OnceLock::new();

pub static READ_NS: AtomicU64 = AtomicU64::new(0);
pub static EVAL_NS: AtomicU64 = AtomicU64::new(0);
pub static FORMS: AtomicU64 = AtomicU64::new(0);

// Round 2 (macroexpand vs eval-body exclusive split): a manual "pause the
// parent, resume it on child pop" self-time stack -- the standard trick for
// turning nested wall-clock spans into non-double-counted totals without a
// real profiler. Two frame kinds share one stack (`TOP_LEVEL_NS`'s frames
// and `MACRO_NS`'s frames nest arbitrarily -- a top-level form triggers
// macro calls, whose bodies are themselves ordinary evaluated code that can
// trigger *nested* macro calls, which can themselves evaluate a `require`
// that pushes more top-level-form frames for another namespace file --
// all of that is just "child frame", handled uniformly). Single-threaded
// (the tree-walker runs on one dedicated thread), so plain `RefCell`, no
// atomics needed here.
struct StackFrame {
    resumed_at: Instant,
    accumulated_ns: u64,
    is_macro: bool,
    name: String,
}

// NOT `thread_local!`: the `atexit` hook that prints the final tables runs
// after clojure-lsp's own `exit` path, and on this platform that can fire
// after the spawned eval thread's TLS has begun tearing down (measured:
// `thread_local`s here panicked with "cannot access a Thread Local Storage
// value during or after destruction" from inside the `atexit` callback).
// Plain `'static` `Mutex`es are never destructed, so they stay reachable
// no matter when `atexit` runs. Single eval thread in practice, so lock
// contention is not a concern.
static STACK: Mutex<Vec<StackFrame>> = Mutex::new(Vec::new());
// name -> (count, total exclusive ns)
static MACRO_STATS: Mutex<Option<HashMap<String, (u64, u64)>>> = Mutex::new(None);
static FORM_STATS: Mutex<Option<HashMap<String, (u64, u64)>>> = Mutex::new(None);

pub static MACROEXPAND_NS: AtomicU64 = AtomicU64::new(0);
pub static EVAL_BODY_NS: AtomicU64 = AtomicU64::new(0);

fn charge_current_top(now: Instant) {
    let mut s = STACK.lock().unwrap();
    if let Some(top) = s.last_mut() {
        top.accumulated_ns += now.saturating_duration_since(top.resumed_at).as_nanos() as u64;
        top.resumed_at = now;
    }
}

fn push_frame(is_macro: bool, name: String) {
    if !enabled() {
        return;
    }
    let now = Instant::now();
    charge_current_top(now);
    STACK
        .lock()
        .unwrap()
        .push(StackFrame { resumed_at: now, accumulated_ns: 0, is_macro, name });
}

/// Pushed around a macro-transformer call (`Interp::apply_macro`) at the
/// ordinary tree-walk dispatch site (`eval::mod.rs`'s `eval_list`), keyed
/// by the symbol AS WRITTEN at the call site -- true special forms
/// (`def`/`if`/`let`/`fn`/`ns`/`defprotocol`/... -- see
/// `special_forms::SPECIAL_FORM_NAMES`/`is_shadowable_special`) never
/// reach here at all, since they are matched before macro dispatch and
/// have no `Value::Macro` transformer to call -- their cost is 100%
/// eval-body, 0% macroexpand, by construction.
pub fn push_macro(name: &str) {
    push_frame(true, name.to_string());
}

/// Pushed around one top-level form's `eval_form` call in `eval_reader`,
/// keyed by `<ns>/<head-symbol> <defined-name-if-any>` so the "eager init
/// code" top-level forms (building a table, registering a spec) are
/// individually identifiable rather than collapsing into "defn"/"def".
pub fn push_top_level(name: String) {
    push_frame(false, name);
}

pub fn pop_frame() {
    if !enabled() {
        return;
    }
    let now = Instant::now();
    let popped = STACK.lock().unwrap().pop();
    if let Some(mut f) = popped {
        f.accumulated_ns += now.saturating_duration_since(f.resumed_at).as_nanos() as u64;
        if f.is_macro {
            let mut guard = MACRO_STATS.lock().unwrap();
            let map = guard.get_or_insert_with(HashMap::new);
            let e = map.entry(f.name).or_insert((0, 0));
            e.0 += 1;
            e.1 += f.accumulated_ns;
            drop(guard);
            MACROEXPAND_NS.fetch_add(f.accumulated_ns, Ordering::Relaxed);
        } else {
            let mut guard = FORM_STATS.lock().unwrap();
            let map = guard.get_or_insert_with(HashMap::new);
            let e = map.entry(f.name).or_insert((0, 0));
            e.0 += 1;
            e.1 += f.accumulated_ns;
            drop(guard);
            EVAL_BODY_NS.fetch_add(f.accumulated_ns, Ordering::Relaxed);
        }
    }
    // Resume the parent's clock now that the child's duration has been
    // excluded from it -- the whole point of this stack.
    let now2 = Instant::now();
    if let Some(top) = STACK.lock().unwrap().last_mut() {
        top.resumed_at = now2;
    }
}

/// Top-N by exclusive time, printed at exit. `what` labels the table.
fn print_top(what: &str, stats: &HashMap<String, (u64, u64)>, n: usize) {
    let mut v: Vec<(&String, &(u64, u64))> = stats.iter().collect();
    v.sort_by(|a, b| b.1 .1.cmp(&a.1 .1));
    for (name, (count, ns)) in v.into_iter().take(n) {
        eprintln!("[load-trace] TOP-{what} {:.3}ms x{count} {name}", *ns as f64 / 1e6);
    }
}

pub fn print_macro_and_form_tops(n: usize) {
    if enabled() {
        if let Some(m) = MACRO_STATS.lock().unwrap().as_ref() {
            print_top("MACRO", m, n);
        }
        if let Some(m) = FORM_STATS.lock().unwrap().as_ref() {
            print_top("FORM", m, n);
        }
    }
}

/// Cached after first check (env var never changes mid-run). Also
/// registers an `atexit` hook the first time trace is turned on, so
/// `print_phase_totals` still runs even when the CLI action (e.g.
/// clojure-lsp's `--version`) calls `System/exit`-equivalent directly
/// from the evaluator thread instead of returning up through `main`.
pub fn enabled() -> bool {
    *ENABLED.get_or_init(|| {
        let on = std::env::var("MOVA_LOAD_TRACE").map(|v| v == "1").unwrap_or(false);
        if on {
            unsafe {
                libc::atexit(atexit_print);
            }
        }
        on
    })
}

extern "C" fn atexit_print() {
    print_phase_totals();
}

pub fn add_read_ns(n: u64) {
    if enabled() {
        READ_NS.fetch_add(n, Ordering::Relaxed);
    }
}

pub fn add_eval_ns(n: u64) {
    if enabled() {
        EVAL_NS.fetch_add(n, Ordering::Relaxed);
    }
}

pub fn inc_forms() {
    if enabled() {
        FORMS.fetch_add(1, Ordering::Relaxed);
    }
}

/// Peak RSS so far, in KB. Monotonic non-decreasing within the process, so
/// a before/after delta only ever shows growth, never frees -- good enough
/// for "which namespace is responsible for this much peak RSS", which is
/// what the probe needs.
/// Process start mark for env-gated timelines (set first thing in `main`).
pub static PROC_T0: OnceLock<Instant> = OnceLock::new();
pub fn mark_start() {
    let _ = PROC_T0.set(Instant::now());
}
/// Milliseconds since `mark_start` (0 if never marked).
pub fn since_start_ms() -> f64 {
    PROC_T0.get().map(|t| t.elapsed().as_secs_f64() * 1e3).unwrap_or(0.0)
}

pub fn maxrss_kb() -> u64 {
    unsafe {
        let mut ru: libc::rusage = std::mem::zeroed();
        libc::getrusage(libc::RUSAGE_SELF, &mut ru);
        #[cfg(target_os = "macos")]
        {
            (ru.ru_maxrss as u64) / 1024
        }
        #[cfg(not(target_os = "macos"))]
        {
            ru.ru_maxrss as u64
        }
    }
}

pub struct NsSpan {
    name: String,
    depth: usize,
    t0: Instant,
    rss0: u64,
    bytes: usize,
    forms0: u64,
}

pub fn ns_enter(name: &str, depth: usize, bytes: usize) -> Option<NsSpan> {
    if !enabled() {
        return None;
    }
    Some(NsSpan {
        name: name.to_string(),
        depth,
        t0: Instant::now(),
        rss0: maxrss_kb(),
        bytes,
        forms0: FORMS.load(Ordering::Relaxed),
    })
}

pub fn ns_exit(span: Option<NsSpan>) {
    if let Some(s) = span {
        let ms = s.t0.elapsed().as_secs_f64() * 1000.0;
        let rss1 = maxrss_kb();
        let forms1 = FORMS.load(Ordering::Relaxed);
        let indent = s.depth * 2;
        eprintln!(
            "[load-trace]{:indent$}{} {:.2}ms rss+{}KB bytes={} forms={}",
            "",
            s.name,
            ms,
            rss1.saturating_sub(s.rss0),
            s.bytes,
            forms1.saturating_sub(s.forms0),
            indent = indent
        );
    }
}

pub fn print_phase_totals() {
    if enabled() {
        print_macro_and_form_tops(15);
        eprintln!(
            "[load-trace] TOTAL read={:.2}ms eval(nested,inflated)={:.2}ms \
             macroexpand(excl)={:.2}ms eval-body(excl)={:.2}ms forms={} peak_rss={}KB",
            READ_NS.load(Ordering::Relaxed) as f64 / 1e6,
            EVAL_NS.load(Ordering::Relaxed) as f64 / 1e6,
            MACROEXPAND_NS.load(Ordering::Relaxed) as f64 / 1e6,
            EVAL_BODY_NS.load(Ordering::Relaxed) as f64 / 1e6,
            FORMS.load(Ordering::Relaxed),
            maxrss_kb()
        );
        // Only present in `--features leak-probe` builds (see env::FRAME_*/
        // value::CLOSURE_* docs); a second, separately-gated print so the
        // default build's `print_phase_totals` never references them.
        #[cfg(feature = "leak-probe")]
        {
            use std::sync::atomic::Ordering::Relaxed;
            eprintln!(
                "[load-trace] LEAK-PROBE closures_created={} closures_dropped={} frames_created={} frames_dropped={}",
                crate::value::CLOSURE_CREATED.load(Relaxed),
                crate::value::CLOSURE_DROPPED.load(Relaxed),
                crate::env::FRAME_CREATED.load(Relaxed),
                crate::env::FRAME_DROPPED.load(Relaxed),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Off by default: with the env var unset, `enabled()` is false and
    /// every instrumentation entry point is a no-op (spans are `None`,
    /// counters never move).
    #[test]
    fn off_by_default() {
        // Only assert this when the harness itself hasn't set it (CI/dev
        // shells sometimes export stray MOVA_* vars); the real contract is
        // "unset => disabled", which is what the OnceLock init reads.
        if std::env::var("MOVA_LOAD_TRACE").is_err() {
            assert!(!enabled());
            assert!(ns_enter("x", 0, 0).is_none());
        }
    }
}
