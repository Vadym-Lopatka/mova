//! Project discovery (source paths via io::project, no classpath) + file walk + uri/path mapping.
use crate::io::project;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Discovered project. `files` is emptied once handed to the pool (store keeps only the meta).
#[derive(Clone, Debug, Default)]
pub struct ProjectInfo {
    pub root: PathBuf,
    pub source_paths: Vec<String>,
    pub files: Vec<PathBuf>,
    pub total: usize,
    /// `:lint-project-files-after-startup?`.
    pub lint_after_startup: bool,
    pub upv: Arc<project::UpvCfg>,
    /// Mova project (`crate::mova::detect`).
    pub mova: bool,
}

const EXTS: [&str; 8] = ["clj", "cljs", "cljc", "cljd", "edn", "bb", "clj_kondo", "mova"];

/// Source paths like clojure-lsp (settings > deps/lein/shadow/bb static dirs > src,test) + all source files.
pub fn discover(root: &Path) -> ProjectInfo {
    let settings = project::Settings::load(root);
    let specs = project::discover(root, &settings);
    let dirs = project::static_dirs(root, &specs, &settings);
    let mut source_paths = project::source_paths(root, &settings, &dirs);
    let mut files = Vec::new();
    for sp in &source_paths {
        let sp = std::fs::canonicalize(sp).unwrap_or_else(|_| PathBuf::from(sp));
        walk(&sp, &mut files, 0);
    }
    // Mova has no project file: every top-level dir with `.mova` files is a module root (unless `:source-paths` is set)
    if settings.source_paths.is_none() {
        let ignore: Vec<regex::Regex> = settings.ignore_regex.iter().filter_map(|r| regex::Regex::new(&format!("^(?:{r})$")).ok()).collect();
        for d in crate::mova::module_dirs(root, crate::mova::detect(root, &files)) {
            let c = std::fs::canonicalize(&d).unwrap_or(d);
            let rel = c.strip_prefix(std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf())).map(|r| r.to_string_lossy().into_owned()).unwrap_or_default();
            let s = c.to_string_lossy().into_owned();
            if !source_paths.contains(&s) && !ignore.iter().any(|re| re.is_match(&rel)) {
                walk(&c, &mut files, 0);
                source_paths.push(s);
            }
        }
    }
    files.sort();
    files.dedup();
    let total = files.len();
    let mova = crate::mova::detect(root, &files);
    ProjectInfo { root: root.to_path_buf(), source_paths, files, total, lint_after_startup: settings.lint_after_startup, upv: Arc::new(settings.upv), mova }
}

/// `file:` uri through symlinks -> uri of the real path (kondo `:canonical-paths true`); others unchanged.
pub fn canon_uri(uri: &str) -> String {
    match uri_to_path(uri).and_then(|p| std::fs::canonicalize(p).ok()) {
        Some(c) => path_to_uri(&c),
        None => uri.to_string(),
    }
}

pub fn walk(dir: &Path, out: &mut Vec<PathBuf>, depth: u32) {
    if depth > 40 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        // a source path may itself be a file
        if dir.is_file() && is_source(dir) {
            out.push(dir.to_path_buf());
        }
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        let Ok(ft) = e.file_type() else { continue };
        let link = ft.is_symlink();
        let ft = if link { std::fs::metadata(&p).map(|m| m.file_type()).unwrap_or(ft) } else { ft };
        // symlinked dirs / files are analyzed under their real path (JVM: canonical paths)
        let p = if link { std::fs::canonicalize(&p).unwrap_or(p) } else { p };
        if ft.is_dir() {
            walk(&p, out, depth + 1);
        } else if ft.is_file() && is_source(&p) {
            out.push(p);
        }
    }
}

/// `:source-paths-ignore-regex` (full match against the path relative to the project root).
pub fn ignored_regexes(root: &Path) -> Vec<regex::Regex> {
    project::Settings::load(root).ignore_regex.iter().filter_map(|r| regex::Regex::new(&format!("^(?:{r})$")).ok()).collect()
}

pub fn is_source(p: &Path) -> bool {
    p.extension().and_then(|e| e.to_str()).map(|e| EXTS.contains(&e)).unwrap_or(false)
}

/// `file:///abs/path` like Java `Path.toUri` (percent-encodes everything but unreserved + `/:@&=+$,;!~*'()`).
pub fn path_to_uri(p: &Path) -> String {
    let s = p.to_string_lossy();
    let mut out = String::with_capacity(s.len() + 8);
    out.push_str("file://");
    for &b in s.as_bytes() {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'/' | b'.' | b'_' | b'-' | b':' | b'@' | b'&' | b'=' | b'+' | b'$' | b',' | b';' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')' => out.push(b as char),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

/// Inverse of `path_to_uri` for `file:` uris; None for other schemes.
pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    let b = rest.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 3 <= b.len() {
            if let Some(v) = rest.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    Some(PathBuf::from(String::from_utf8_lossy(&out).into_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn uri_roundtrip() {
        let p = Path::new("/tmp/a b/é.clj");
        let u = path_to_uri(p);
        assert_eq!(u, "file:///tmp/a%20b/%C3%A9.clj");
        assert_eq!(uri_to_path(&u).unwrap(), p);
        assert!(uri_to_path("zipfile:///x").is_none());
    }
}
