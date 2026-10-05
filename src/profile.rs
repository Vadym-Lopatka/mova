//! Clojure-level sampling profiler, gated by `MOVA_PROFILE=<file>`. Zero
//! cost when unset: every call site is a single `OnceLock` bool read
//! before any work (same pattern as `load_trace::enabled`). Written to
//! find which CLOJURE fns (not Rust/interpreter frames -- `sample(1)`
//! only sees `eval_list`/`apply_closure`) are hot; see mova/PLAN.md
//! "Token budget" + the clojure-lsp-on-mova campaign notes.
//!
//! Mechanism: each thread keeps a cheap stack of the Clojure fn names it
//! is currently inside (var name, `ns/anon@line` for anonymous fns, or a
//! native's registered name), pushed/popped around `apply_closure`/
//! `apply_closure_buf`/the `apply`-variadic entry (both the tree-walk and
//! compiled tiers all funnel through these) and native calls. A
//! background sampler thread wakes every ~1ms, locks each live thread's
//! stack (best-effort `try_lock`, so a busy stack is just skipped for
//! that tick rather than blocking the profiled thread), and records the
//! top frame (self time) plus every distinct frame in the stack
//! (inclusive time). At exit, top-40 self and top-40 inclusive by sample
//! count are written to the `MOVA_PROFILE` path.
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

static OUT_PATH: OnceLock<Option<String>> = OnceLock::new();

fn out_path() -> Option<&'static str> {
    OUT_PATH.get_or_init(|| std::env::var("MOVA_PROFILE").ok()).as_deref()
}

/// True when `MOVA_PROFILE` is set. Exposed so a call site can skip
/// computing an expensive frame name (e.g. resolving a def-span to a
/// `ns:line:col` string) when profiling is off, rather than paying that
/// cost only to have `push` throw the result away.
pub fn enabled() -> bool {
    out_path().is_some()
}

type ThreadStack = Arc<Mutex<Vec<String>>>;

static THREADS: Mutex<Vec<ThreadStack>> = Mutex::new(Vec::new());
static SELF_COUNTS: Mutex<Option<HashMap<String, u64>>> = Mutex::new(None);
static INCL_COUNTS: Mutex<Option<HashMap<String, u64>>> = Mutex::new(None);

thread_local! {
    static MY_STACK: ThreadStack = {
        let s: ThreadStack = Arc::new(Mutex::new(Vec::new()));
        THREADS.lock().unwrap().push(s.clone());
        s
    };
}

fn ensure_started() {
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        unsafe {
            libc::atexit(atexit_write);
        }
        std::thread::spawn(sampler_loop);
    });
}

fn sampler_loop() {
    loop {
        std::thread::sleep(Duration::from_millis(1));
        let threads: Vec<ThreadStack> = THREADS.lock().unwrap().clone();
        for t in &threads {
            let Ok(guard) = t.try_lock() else { continue };
            if guard.is_empty() {
                continue;
            }
            let top = guard.last().unwrap().clone();
            {
                let mut g = SELF_COUNTS.lock().unwrap();
                *g.get_or_insert_with(HashMap::new).entry(top).or_insert(0) += 1;
            }
            // Inclusive: each distinct frame counted once per sample, so a
            // fn recursing on itself doesn't inflate its own inclusive
            // count relative to a sibling that appears once.
            let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
            let mut g = INCL_COUNTS.lock().unwrap();
            let map = g.get_or_insert_with(HashMap::new);
            for name in guard.iter() {
                if seen.insert(name.as_str()) {
                    *map.entry(name.clone()).or_insert(0) += 1;
                }
            }
        }
    }
}

/// Push the currently-entered Clojure fn (or native) name. Called from
/// `apply_closure`/`apply_closure_buf`/the `apply`-variadic entry (both
/// tiers) and native call dispatch.
pub fn push(name: &str) {
    if out_path().is_some() {
        ensure_started();
        MY_STACK.with(|s| s.lock().unwrap().push(name.to_string()));
    }
}

pub fn pop() {
    if out_path().is_some() {
        MY_STACK.with(|s| {
            s.lock().unwrap().pop();
        });
    }
}

/// M9 heap-prof: innermost Clojure fn names on this thread (non-native first), "" when off.
pub fn clj_frames(out: &mut [u8]) {
    if out_path().is_none() { return; }
    let _ = MY_STACK.try_with(|s| {
        let Ok(g) = s.try_lock() else { return };
        let mut w = 0;
        for n in g.iter().rev().filter(|n| n.contains('/') && !n.starts_with("clojure.core/")).take(4) {
            for &b in n.as_bytes().iter().chain(b" < ") { if w < out.len() { out[w] = b; w += 1; } }
        }
    });
}

fn top_n(stats: &HashMap<String, u64>, n: usize) -> Vec<(String, u64)> {
    let mut v: Vec<(String, u64)> = stats.iter().map(|(k, v)| (k.clone(), *v)).collect();
    v.sort_by(|a, b| b.1.cmp(&a.1));
    v.truncate(n);
    v
}

extern "C" fn atexit_write() {
    let Some(path) = out_path() else { return };
    let mut out = String::new();
    if let Some(m) = SELF_COUNTS.lock().unwrap().as_ref() {
        out.push_str("TOP-40 SELF (samples fn was on top of stack)\n");
        for (name, count) in top_n(m, 40) {
            out.push_str(&format!("{count:>8}  {name}\n"));
        }
    }
    if let Some(m) = INCL_COUNTS.lock().unwrap().as_ref() {
        out.push_str("\nTOP-40 INCLUSIVE (samples fn was anywhere in stack)\n");
        for (name, count) in top_n(m, 40) {
            out.push_str(&format!("{count:>8}  {name}\n"));
        }
    }
    let _ = std::fs::write(path, out);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_by_default() {
        if std::env::var("MOVA_PROFILE").is_err() {
            assert!(out_path().is_none());
        }
    }
}
