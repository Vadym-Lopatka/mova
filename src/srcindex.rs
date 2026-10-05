//! `mova --source-index`: where stdlib .mova sources and Rust builtins live.
use crate::value::Symbol;
use std::panic::Location;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

static ON: AtomicBool = AtomicBool::new(false);
static TABLE: Mutex<Vec<(String, String, &'static str, u32)>> = Mutex::new(Vec::new());

/// Qualified aliases of a var (`bind_alias`): (ns, name, target ns, target name).
static ALIASES: Mutex<Vec<(String, String, String, String)>> = Mutex::new(Vec::new());

/// Record `ns/name` as a second spelling of the bare (`clojure.core`) var `to` (same cell).
pub fn note_var_alias(ns: &str, name: &str, to: &str) {
    if !ON.load(Ordering::Relaxed) {
        return;
    }
    if let Ok(mut t) = ALIASES.lock() {
        t.push((ns.to_string(), name.to_string(), "clojure.core".to_string(), to.to_string()));
    }
}

/// Turn recording on (off by default: zero cost at normal startup beyond one relaxed load).
pub fn enable() {
    ON.store(true, Ordering::Relaxed);
}

/// Record the registration site of a builtin (caller location via #[track_caller]).
#[track_caller]
#[inline]
pub fn note(sym: &Symbol) {
    if !ON.load(Ordering::Relaxed) {
        return;
    }
    let loc = Location::caller();
    let ns = sym.ns.as_deref().unwrap_or("clojure.core").to_string();
    if let Ok(mut t) = TABLE.lock() {
        t.push((ns, sym.name.to_string(), loc.file(), loc.line()));
    }
}

/// Record an alias: same location as the bare `clojure.core/<from>` registration.
pub fn note_alias(sym: &Symbol, from: &str) {
    if !ON.load(Ordering::Relaxed) {
        return;
    }
    if let Ok(mut t) = TABLE.lock() {
        let hit = t.iter().rev().find(|e| e.0 == "clojure.core" && e.1 == from).map(|e| (e.2, e.3));
        if let Some((f, l)) = hit {
            let ns = sym.ns.as_deref().unwrap_or("clojure.core").to_string();
            t.push((ns, sym.name.to_string(), f, l));
        }
    }
}

fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn rev() -> String {
    std::process::Command::new("git")
        .args(["-C", env!("CARGO_MANIFEST_DIR"), "rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".into())
}

/// Build the JSON index. Call after an Interp has been created with recording enabled.
pub fn json() -> String {
    let root = env!("CARGO_MANIFEST_DIR");
    let mut nss: Vec<(String, String)> = vec![
        ("clojure.core".into(), "core/core.mova".into()),
        // core/async.mova has no `ns` form: its defs are bare; `clojure.core.async/<name>` are aliases (below)
        ("clojure.core".into(), "core/async.mova".into()),
    ];
    for m in crate::stdlib::embedded_modules() {
        nss.push((m.ns.to_string(), format!("core/lib/{}", m.file)));
    }
    nss.sort();
    nss.dedup();
    let mut nat: Vec<(String, String, String, u32)> = TABLE
        .lock()
        .map(|t| t.iter().map(|(a, b, f, l)| (a.clone(), b.clone(), rel(f, root), *l)).collect())
        .unwrap_or_default();
    // last registration wins (later ones shadow earlier ones)
    let mut seen = std::collections::HashSet::new();
    nat.reverse();
    nat.retain(|(a, b, _, _)| seen.insert((a.clone(), b.clone())));
    nat.sort_by(|x, y| (&x.0, &x.1).cmp(&(&y.0, &y.1)));
    let mut o = format!("{{\"v\":2,\"mova_rev\":{},\"root\":{},\"namespaces\":[", esc(&rev()), esc(root));
    for (i, (ns, f)) in nss.iter().enumerate() {
        if i > 0 {
            o.push(',');
        }
        o.push_str(&format!("{{\"ns\":{},\"file\":{}}}", esc(ns), esc(f)));
    }
    o.push_str("],\"natives\":[");
    for (i, (ns, n, f, l)) in nat.iter().enumerate() {
        if i > 0 {
            o.push(',');
        }
        o.push_str(&format!("{{\"ns\":{},\"name\":{},\"file\":{},\"line\":{}}}", esc(ns), esc(n), esc(f), l));
    }
    o.push_str("],\"aliases\":[");
    let mut al = ALIASES.lock().map(|t| t.clone()).unwrap_or_default();
    al.sort();
    al.dedup();
    for (i, (ns, n, tns, t)) in al.iter().enumerate() {
        if i > 0 {
            o.push(',');
        }
        o.push_str(&format!("{{\"ns\":{},\"name\":{},\"to_ns\":{},\"to\":{}}}", esc(ns), esc(n), esc(tns), esc(t)));
    }
    o.push_str("],\"default_aliases\":[");
    for (i, (a, ns)) in crate::ns::default_aliases().iter().enumerate() {
        if i > 0 {
            o.push(',');
        }
        o.push_str(&format!("{{\"alias\":{},\"ns\":{}}}", esc(a), esc(ns)));
    }
    o.push_str("]}");
    o
}

fn rel(f: &str, root: &str) -> String {
    let f = f.strip_prefix(root).map(|s| s.trim_start_matches('/')).unwrap_or(f);
    f.to_string()
}
