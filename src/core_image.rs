//! P0ab: where the core image lives and what key it is stored under.
//!
//! Key = the binary's own build identity. On macOS that is the Mach-O
//! `LC_UUID` of the main executable: the linker derives it from the output's
//! content, so it changes on every relink with different bytes, is read from
//! memory already mapped (no syscall, ~100 ns), and a copied or moved binary
//! keeps the same key (correct: same natives table). Elsewhere: exe path +
//! size + mtime (one `stat`).

use std::path::PathBuf;
use std::sync::OnceLock;

static IMAGE_PATH: OnceLock<PathBuf> = OnceLock::new();
static BUILD_KEY: OnceLock<String> = OnceLock::new();

/// Outcome of the last core boot, for `--verbose`. 0 none, 1 restored,
/// 2 miss -> booted + saved, 3 invalid/failed -> booted normally.
pub static OUTCOME: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);
pub static NATIVES_US: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static RESTORE_US: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static CHECK_NS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static SAVE_ENCODE_US: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(target_os = "macos")]
fn raw_build_key() -> Option<String> {
    unsafe {
        let h = libc::_dyld_get_image_header(0) as *const u8;
        if h.is_null() {
            return None;
        }
        let ncmds = *(h.add(16) as *const u32);
        let mut p = h.add(32);
        for _ in 0..ncmds {
            let cmd = *(p as *const u32);
            let size = *(p.add(4) as *const u32) as usize;
            if cmd == 0x1b && size >= 24 {
                let u = std::slice::from_raw_parts(p.add(8), 16);
                return Some(u.iter().map(|b| format!("{b:02x}")).collect());
            }
            if size < 8 {
                return None;
            }
            p = p.add(size);
        }
        None
    }
}

#[cfg(not(target_os = "macos"))]
fn raw_build_key() -> Option<String> {
    None
}

fn fallback_key() -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    if let Ok(exe) = std::env::current_exe() {
        exe.hash(&mut h);
        if let Ok(m) = std::fs::metadata(&exe) {
            m.len().hash(&mut h);
            if let Ok(t) = m.modified() {
                t.hash(&mut h);
            }
        }
    }
    format!("fb{:016x}", h.finish())
}

pub fn build_key() -> &'static str {
    BUILD_KEY.get_or_init(|| raw_build_key().unwrap_or_else(fallback_key))
}

/// `$XDG_CACHE_HOME/mova/` else `$HOME/.cache/mova/`.
pub fn cache_dir() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_CACHE_HOME").filter(|v| !v.is_empty()) {
        Some(x) => PathBuf::from(x),
        None => PathBuf::from(std::env::var_os("HOME").filter(|v| !v.is_empty())?).join(".cache"),
    };
    Some(base.join("mova"))
}

/// Turns the core-image cache on for this process (call before the first
/// `Interp::new`). Returns the cache file path.
pub fn enable_cache() -> Option<PathBuf> {
    let p = cache_dir()?.join(format!("core-{}.img", build_key()));
    let _ = IMAGE_PATH.set(p.clone());
    Some(p)
}

pub fn image_path() -> Option<&'static PathBuf> {
    IMAGE_PATH.get()
}

/// The validity header stored inside the image: env flags + build key.
pub fn header() -> String {
    format!("{}|bk:{}", crate::image::core_header(), build_key())
}

/// Drops other builds' images older than 7 days (best effort).
pub fn prune_stale(keep: &std::path::Path) {
    let Some(dir) = keep.parent() else { return };
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let week = std::time::Duration::from_secs(7 * 86400);
    for e in rd.flatten() {
        let p = e.path();
        let n = e.file_name();
        let n = n.to_string_lossy();
        if p != keep && n.starts_with("core-") && (n.ends_with(".img") || n.contains(".tmp")) {
            let old = e.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|a| a > week);
            if old {
                let _ = std::fs::remove_file(&p);
            }
        }
    }
}
