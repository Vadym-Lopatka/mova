//! Native JDK layer: class + member definitions of the JDK `src.zip` (what clojure-lsp gets from analyzing the extracted
//! JDK sources with clj-kondo), answering Java interop definition / hover / completion.
//!
//! The index is built natively from `src.zip` (no extraction), cached at `$XDG_CACHE_HOME/nx/jdk/jdk-<key>.idx` and
//! mmapped. It is never built on the open-file path: `start()` spawns a background thread (after the project pass or on
//! the first Java query); queries `wait()` for it only when they actually touch Java.
pub mod index;
pub mod jp;
pub mod parse;

pub use index::{ClassRef, Jdk, MemberRef};

use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

enum St {
    Idle,
    Building,
    Ready(Arc<Jdk>),
    Unavailable,
}

static ST: Mutex<St> = Mutex::new(St::Idle);
static CV: Condvar = Condvar::new();
static ROOT: Mutex<Option<PathBuf>> = Mutex::new(None);
static RELEASE: std::sync::OnceLock<fn()> = std::sync::OnceLock::new();

/// Hook run after an index build to hand freed heap back to the OS (the host binding sets its allocator's collect).
pub fn set_release_hook(f: fn()) {
    let _ = RELEASE.set(f);
}

/// Project root for `.lsp/config.edn` java settings.
pub fn set_root(root: &Path) {
    *ROOT.lock().unwrap() = Some(root.to_path_buf());
}

/// Start loading / building the index in the background (idempotent).
pub fn start() {
    {
        let mut g = ST.lock().unwrap();
        if !matches!(*g, St::Idle) {
            return;
        }
        *g = St::Building;
    }
    let root = ROOT.lock().unwrap().clone();
    std::thread::Builder::new()
        .name("nx-jdk".into())
        .spawn(move || {
            let r = load_or_build(root.as_deref());
            if let Some(f) = RELEASE.get() {
                f();
            }
            let mut g = ST.lock().unwrap();
            *g = match r {
                Some(j) => St::Ready(Arc::new(j)),
                None => St::Unavailable,
            };
            CV.notify_all();
        })
        .ok();
}

/// The index if ready (never blocks, never starts it).
pub fn get() -> Option<Arc<Jdk>> {
    match &*ST.lock().unwrap() {
        St::Ready(j) => Some(j.clone()),
        _ => None,
    }
}

/// The index; starts it when idle and waits up to `max` for a running build. None when there is no JDK source.
pub fn wait(max: Duration) -> Option<Arc<Jdk>> {
    if let Some(j) = get() {
        return Some(j);
    }
    start();
    let end = Instant::now() + max;
    let mut g = ST.lock().unwrap();
    loop {
        match &*g {
            St::Ready(j) => return Some(j.clone()),
            St::Unavailable => return None,
            _ => {}
        }
        let now = Instant::now();
        if now >= end {
            return None;
        }
        g = CV.wait_timeout(g, end - now).unwrap().0;
    }
}

/// Default wait of a query that needs the index.
pub const QUERY_WAIT: Duration = Duration::from_secs(30);

fn cache_dir() -> PathBuf {
    crate::io::cache_root().join("jdk")
}

fn cache_file(zip: &Path) -> PathBuf {
    let (size, mtime) = index::zip_stat(zip);
    let key = crate::io::hash_parts([zip.as_os_str().as_encoded_bytes(), &size.to_le_bytes(), &mtime.to_le_bytes(), &index::PARSE_VERSION.to_le_bytes()]);
    cache_dir().join(format!("jdk-{}.idx", &key[..16]))
}

/// Load the cached index for `zip`, or build and cache it.
pub fn open_zip(zip: &Path, threads: usize) -> Option<Jdk> {
    let idx = cache_file(zip);
    if let Some(j) = Jdk::open(&idx, zip) {
        return Some(j);
    }
    let bytes = index::build(zip, threads).ok()?;
    let dir = cache_dir();
    std::fs::create_dir_all(&dir).ok()?;
    let tmp = dir.join(format!(".tmp-{}-{:?}", std::process::id(), std::thread::current().id()).replace(['(', ')', ' '], ""));
    std::fs::write(&tmp, &bytes).ok()?;
    drop(bytes);
    if std::fs::rename(&tmp, &idx).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    // drop stale indexes of older zips
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p != idx && p.extension().map_or(false, |x| x == "idx") {
                let _ = std::fs::remove_file(p);
            }
        }
    }
    Jdk::open(&idx, zip)
}

fn load_or_build(root: Option<&Path>) -> Option<Jdk> {
    let zip = resolve_zip(root)?;
    let cores = std::thread::available_parallelism().map_or(2, |n| n.get());
    open_zip(&zip, cores.saturating_sub(2).clamp(1, 4))
}

fn uri_or_path(s: &str) -> Option<PathBuf> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if s.starts_with("file:") {
        return crate::engine::scan::uri_to_path(s);
    }
    if s.contains("://") {
        return None; // remote: clojure-lsp downloads it; nx does not
    }
    Some(PathBuf::from(s))
}

/// `(java :jdk-source-uri, java :home-path)` from `~/.config/clojure-lsp/config.edn` then `<root>/.lsp/config.edn`.
fn java_settings(root: Option<&Path>) -> (Option<String>, Option<String>) {
    let mut files = Vec::new();
    let cfg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")));
    if let Some(c) = cfg {
        files.push(c.join("clojure-lsp/config.edn"));
    }
    if let Some(r) = root {
        files.push(r.join(".lsp/config.edn"));
    }
    let (mut uri, mut home) = (None, None);
    for f in files {
        let Some(e) = crate::io::edn::read_file(&f) else { continue };
        if let Some(j) = e.get("java") {
            if let Some(s) = j.get("jdk-source-uri").and_then(|v| v.as_str()) {
                uri = Some(s.to_string());
            }
            if let Some(s) = j.get("home-path").and_then(|v| v.as_str()) {
                home = Some(s.to_string());
            }
        }
    }
    (uri, home)
}

fn src_zip_candidates(root: &Path) -> Vec<PathBuf> {
    let mut v = vec![root.join("src.zip"), root.join("lib/src.zip")];
    if let Some(p) = root.parent() {
        v.push(p.join("src.zip"));
        v.push(p.join("lib/src.zip"));
    }
    v
}

/// The JDK `src.zip`, by clojure-lsp's rule: installed (`<cache>/clojure-lsp/jdk/result`) unless a different custom
/// `:jdk-source-uri` is set; else the custom local zip; else settings `:java :home-path` / `JAVA_HOME` / `java` on PATH.
/// `NX_JDK_ZIP` overrides everything (tests).
pub fn resolve_zip(root: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("NX_JDK_ZIP") {
        return Some(PathBuf::from(p));
    }
    let (custom, home) = java_settings(root);
    let custom_path = custom.as_deref().and_then(uri_or_path);
    let base = match std::env::var_os("XDG_CACHE_HOME") {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".cache"),
    };
    let installed = std::fs::read_to_string(base.join("clojure-lsp/jdk/result")).ok().and_then(|s| uri_or_path(&s)).filter(|p| p.is_file());
    if let Some(i) = &installed {
        if custom.is_none() || custom_path.as_ref() == Some(i) {
            return installed;
        }
    }
    if let Some(c) = custom_path.filter(|p| p.is_file()) {
        return Some(c);
    }
    if custom.is_none() {
        let mut roots: Vec<PathBuf> = Vec::new();
        if let Some(h) = home.or_else(|| std::env::var("JAVA_HOME").ok()) {
            roots.push(PathBuf::from(h));
        }
        if let Some(path) = std::env::var_os("PATH") {
            for d in std::env::split_paths(&path) {
                let j = d.join("java");
                if let Ok(real) = std::fs::canonicalize(&j) {
                    if let Some(h) = real.parent().and_then(|p| p.parent()) {
                        roots.push(h.to_path_buf());
                    }
                }
            }
        }
        for r in roots {
            if let Some(p) = src_zip_candidates(&r).into_iter().find(|p| p.is_file()) {
                return Some(p);
            }
        }
        if installed.is_some() {
            return installed;
        }
    }
    None
}

/// Extracted file of a class's source (`$XDG_CACHE_HOME/nx/jdk/<entry>`, e.g. `java.base/java/lang/String.java`), written on first use.
/// JVM clojure-lsp extracts the whole zip to `<cache>/clojure-lsp/jdk/` with the same relative layout; nx extracts one file per class.
pub fn extracted_path(j: &Jdk, c: ClassRef) -> Option<PathBuf> {
    let entry = j.class_entry(c);
    if entry.split('/').any(|p| p == ".." || p.is_empty()) {
        return None;
    }
    let path = cache_dir().join(entry);
    if path.is_file() {
        return Some(path);
    }
    let bytes = j.class_bytes(c)?;
    std::fs::create_dir_all(path.parent()?).ok()?;
    let tmp = path.with_extension(format!("tmp{}-{:?}", std::process::id(), std::thread::current().id()).replace(['(', ')', ' '], ""));
    std::fs::write(&tmp, &bytes).ok()?;
    if std::fs::rename(&tmp, &path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    path.is_file().then_some(path)
}

/// `jar:file://<zip>!/<entry>` or `zipfile://<zip>::<entry>` per dependency-scheme.
pub fn entry_uri(j: &Jdk, entry: &str, jar_scheme: bool) -> String {
    crate::engine::jarview::jar_entry_uri(&j.zip, entry, jar_scheme)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real JDK sources when present (skipped otherwise).
    #[test]
    fn real_src_zip() {
        let Some(zip) = resolve_zip(None) else { return };
        let dir = std::env::temp_dir().join(format!("nx-jdk-test-{}", std::process::id()));
        std::env::set_var("XDG_CACHE_HOME", &dir);
        let t = Instant::now();
        let j = open_zip(&zip, 4).expect("index");
        eprintln!("cold build {:?}: {} files, {} classes, {} members, {} bytes", t.elapsed(), j.file_count(), j.class_count(), j.member_count(), j.size_bytes());
        let t = Instant::now();
        let j2 = open_zip(&zip, 4).expect("warm");
        eprintln!("warm load {:?}", t.elapsed());
        drop(j2);
        let uuid = j.class("java.util.UUID").expect("UUID");
        let m = j.find_member(uuid, "randomUUID").expect("randomUUID");
        assert_eq!(j.member_pos(m).0, 149);
        let int = j.class("java.lang.Integer").unwrap();
        let m = j.find_member(int, "MAX_VALUE").unwrap();
        assert_eq!(j.member_pos(m).0, 86);
        assert!(j.member_doc(int, m).unwrap().starts_with("/**"));
        assert!(j.class("java.util.Date").is_some());
        let _ = std::fs::remove_dir_all(dir);
    }
}
