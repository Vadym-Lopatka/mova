//! kondo-wave (clojure-lsp-on-mova campaign): pure path-string helpers
//! backing the `java.io.File` veneer (`hostclass::call_javafile_method`)
//! plus two bare natives (`directory?`, `canonical-path`) that mirror
//! `builtins::sys`'s existing `file-exists?`/`list-dir` shape for code
//! that wants a stat without going through a `java.io.File` instance at
//! all (the clojure-lsp-kondo overlay's `clojure.java.io` shim does).
//!
//! NEW FILE (not touching `sys.rs`/`hostclass.rs` beyond one small
//! dispatch match each) so this and any other in-flight wave's additions
//! to those shared files don't collide. Real Rust `std::fs`/`std::path`
//! semantics only -- no clj-kondo/babashka.fs-specific hacks; every
//! function here answers the same question the identically-named
//! `java.io.File` method does, nothing narrower.

use std::path::Path;

use crate::builtins::{reg, ArityHint};
use crate::error::RjError;
use crate::eval::Interp;
use crate::value::{Str, Value};

pub fn register(i: &mut Interp) {
    reg(i, "directory?", ArityHint::Exact(1), directory_pred);
    reg(i, "canonical-path", ArityHint::Exact(1), canonical_path_native);
}

/// kondo-wave: accepts a plain string OR a `java.io.File`
/// `HostInst` (unwrapping to its stored path) -- clj-kondo's own
/// `impl/core.clj` calls `(slurp f)`/`(spit dest ..)` with `f`/`dest`
/// bound to a `(io/file ..)` result at several call sites (e.g.
/// `read-edn-file`, `copy-config-entry`), not only a bare string, exactly
/// like real `clojure.java.io`'s `Coercions` protocol accepts either. An
/// owned `Str` (not `&str`) because the `HostInst` case reads its path out
/// from behind a mutex guard that cannot outlive this call.
pub fn expect_path(v: &Value, who: &str) -> Result<Str, RjError> {
    match v {
        // lsp/io: strip a `file://` lead-in like `slurp_path_arg` (sys.rs)
        // does -- real `clojure.java.io` resolves a String arg as a URI
        // first, bare filename second; this only did the second half, so
        // a `file://`-URI `:uri` (every LSP TextDocumentEdit) silently
        // wrote/stat'd the wrong (nonexistent) path.
        Value::Str(s) => Ok(match s.as_ref().strip_prefix("file://") {
            Some(rest) => Str::from(rest),
            None => s.clone(),
        }),
        Value::HostInst(h) if h.kind == crate::hostclass::HostKind::JavaFile => {
            crate::hostclass::java_file_path(v)
                .ok_or_else(|| RjError::type_err(format!("{who}: not a java.io.File")))
        }
        other => Err(RjError::type_err(format!(
            "{who}: expected a string or java.io.File, got {}",
            other.type_name()
        ))),
    }
}

fn directory_pred(_interp: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    let path = expect_path(&args[0], "directory?")?;
    Ok(Value::Bool(is_directory(path.as_ref())))
}

fn canonical_path_native(_interp: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    let path = expect_path(&args[0], "canonical-path")?;
    Ok(Value::Str(canonical_path(path.as_ref()).into()))
}

// ---------------------------------------------------------------------
// Shared path-string logic -- also called directly from
// `hostclass::call_javafile_method` (each `java.io.File` instance method
// is one of these plus the path stored in the `HostState::JavaFile` cell).
// ---------------------------------------------------------------------

/// `(File. parent child)` join -- Java's `File(String, String)`: `child`
/// appended to `parent` with `/`, `parent` alone if `child` is empty,
/// `child` alone if `parent` is empty (matches the no-parent-string
/// constructor rather than throwing, which is what every in-scope call
/// site needs: clj-kondo's `(apply io/file cfg-dir root)` folds `file`
/// left-to-right over a list that always starts from a real dir).
pub fn join(parent: &str, child: &str) -> String {
    if parent.is_empty() {
        child.to_string()
    } else if child.is_empty() {
        parent.to_string()
    } else if parent.ends_with('/') {
        format!("{parent}{child}")
    } else {
        format!("{parent}/{child}")
    }
}

pub fn is_directory(path: &str) -> bool {
    Path::new(path).is_dir()
}

pub fn is_absolute(path: &str) -> bool {
    Path::new(path).is_absolute()
}

/// Java's `File.getParent()`: `None` (real: `null`) for a path with no
/// separator at all (a bare relative single-segment name), `Some` of the
/// text before the last separator otherwise -- including `Some("")` for
/// e.g. `"/x"` -> `""`... except real Java answers `"/"` there, not `""`,
/// so the empty-and-absolute case is special-cased back to `"/"`.
pub fn parent(path: &str) -> Option<String> {
    let p = Path::new(path);
    let parent = p.parent()?;
    let s = parent.to_string_lossy().into_owned();
    if s.is_empty() {
        if path.starts_with('/') {
            Some("/".to_string())
        } else {
            None
        }
    } else {
        Some(s)
    }
}

pub fn absolute_path(path: &str) -> String {
    let p = Path::new(path);
    if p.is_absolute() {
        path.to_string()
    } else {
        match std::env::current_dir() {
            Ok(cwd) => cwd.join(p).to_string_lossy().into_owned(),
            Err(_) => path.to_string(),
        }
    }
}

/// Lexical `.`/`..` collapse of an absolute path, with no filesystem
/// access -- `canonical_path`'s fallback for a path (or path prefix) that
/// doesn't exist, matching real `File.getCanonicalPath()`, which
/// normalizes even nonexistent paths rather than throwing.
fn normalize_lexically(path: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            s => out.push(s),
        }
    }
    format!("/{}", out.join("/"))
}

pub fn canonical_path(path: &str) -> String {
    let abs = absolute_path(path);
    std::fs::canonicalize(&abs)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| normalize_lexically(&abs))
}

pub fn mkdir(path: &str) -> bool {
    std::fs::create_dir(path).is_ok()
}

/// Java's File#createNewFile: atomically creates the file iff it didn't
/// exist. Returns true if this call created it, false if it already existed.
pub fn create_new_file(path: &str) -> bool {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .is_ok()
}

pub fn delete(path: &str) -> bool {
    let p = Path::new(path);
    if p.is_dir() {
        std::fs::remove_dir(p).is_ok()
    } else {
        std::fs::remove_file(p).is_ok()
    }
}

/// `.list`/`.listFiles` -- sorted entry names (same ordering choice as
/// `builtins::sys::list_dir`) of a directory, `None` if `path` isn't a
/// readable directory (real Java: `null` for either a non-directory or an
/// I/O error -- this veneer doesn't distinguish the two, same narrowing
/// `builtins::sys::list_dir` already accepts for the bare `list-dir`).
pub fn list_names(path: &str) -> Option<Vec<String>> {
    let entries = std::fs::read_dir(path).ok()?;
    let mut names: Vec<String> = Vec::new();
    for entry in entries {
        let entry = entry.ok()?;
        names.push(entry.file_name().to_string_lossy().into_owned());
    }
    names.sort();
    Some(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_matches_java_shape() {
        assert_eq!(join("/a/b", "c"), "/a/b/c");
        assert_eq!(join("/a/b/", "c"), "/a/b/c");
        assert_eq!(join("", "c"), "c");
        assert_eq!(join("/a/b", ""), "/a/b");
    }

    #[test]
    fn parent_matches_java_shape() {
        assert_eq!(parent("/a/b"), Some("/a".to_string()));
        assert_eq!(parent("/a"), Some("/".to_string()));
        assert_eq!(parent("a"), None);
    }

    #[test]
    fn normalize_lexically_collapses_dots() {
        assert_eq!(normalize_lexically("/a/./b/../c"), "/a/c");
        assert_eq!(normalize_lexically("/a/../../b"), "/b");
    }

    #[test]
    fn canonical_path_falls_back_when_missing() {
        // A nonexistent path still normalizes lexically instead of erroring.
        let got = canonical_path("/definitely/not/a/real/path/../also-not-real");
        assert_eq!(got, "/definitely/not/a/real/also-not-real");
    }
}
