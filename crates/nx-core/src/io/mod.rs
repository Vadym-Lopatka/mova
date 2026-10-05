//! IO edge of nx-core: project discovery, classpath, jar reading, on-disk caches.
pub mod cache;
pub mod classpath;
pub mod edn;
pub mod jar;
pub mod project;

use sha2::{Digest, Sha256};
use std::path::PathBuf;

/// `$XDG_CACHE_HOME/nx` (falls back to `$HOME/.cache/nx`).
pub fn cache_root() -> PathBuf {
    let base = match std::env::var_os("XDG_CACHE_HOME") {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".cache"),
    };
    base.join("nx")
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    const H: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(H[(b >> 4) as usize] as char);
        s.push(H[(b & 15) as usize] as char);
    }
    s
}

/// 32-hex-char (128-bit) truncated SHA-256 of the fed parts.
pub(crate) fn hash_parts<'a>(parts: impl IntoIterator<Item = &'a [u8]>) -> String {
    let mut h = Sha256::new();
    for p in parts {
        h.update((p.len() as u64).to_le_bytes());
        h.update(p);
    }
    hex(&h.finalize()[..16])
}
