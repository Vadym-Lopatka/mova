//! Classpath directories outside the project's source paths (git deps, `:local/root`): analyzed as external sources
//! (shallow, definitions only) and registered as non-internal files. Jars go through `src/jars` instead.
use super::analyze::file_kind;
use super::scan::path_to_uri;
use super::types::Lang;
use crate::analyzer::{analyze_cst, Config, DefsIndex, FileAnalysis, Options};
use std::path::{Path, PathBuf};

fn walk(dir: &Path, out: &mut Vec<PathBuf>, depth: u32) {
    if depth > 30 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        let name = e.file_name();
        let name = name.to_string_lossy();
        let Ok(ft) = e.file_type() else { continue };
        let ft = if ft.is_symlink() { std::fs::metadata(&p).map(|m| m.file_type()).unwrap_or(ft) } else { ft };
        if ft.is_dir() {
            if name.starts_with('.') || name == "node_modules" || name == "clj-kondo.exports" {
                continue;
            }
            walk(&p, out, depth + 1);
        } else if ft.is_file() && matches!(Lang::from_path(&name), Lang::Clj | Lang::Cljs | Lang::Cljc) {
            out.push(p);
        }
    }
}

/// Classpath dirs that are real directories not inside (or equal to) a project source path.
pub fn external_dirs(root: &Path, dirs: &[String], source_paths: &[String]) -> Vec<PathBuf> {
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let sps: Vec<PathBuf> = source_paths.iter().map(|s| canon(Path::new(s))).collect();
    let root = canon(root);
    let mut out = Vec::new();
    for d in dirs {
        let p = if Path::new(d).is_absolute() { PathBuf::from(d) } else { root.join(d) };
        if !p.is_dir() {
            continue;
        }
        let p = canon(&p);
        // a dir that contains the project (e.g. ".") or lies inside a source path is project code, not a dependency
        if p == root || root.starts_with(&p) || sps.iter().any(|s| p.starts_with(s) || s.starts_with(&p)) {
            continue;
        }
        if !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

/// Analyze all sources below `dirs` in parallel: (uri, lang, external analysis).
pub fn analyze_dirs(dirs: &[PathBuf], cfg: &Config) -> Vec<(String, Lang, FileAnalysis)> {
    let mut files = Vec::new();
    for d in dirs {
        walk(d, &mut files, 0);
    }
    files.sort();
    files.dedup();
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2).saturating_sub(2).max(1).min(files.len().max(1));
    let next = std::sync::atomic::AtomicUsize::new(0);
    let out = std::sync::Mutex::new(Vec::new());
    let defs = DefsIndex::new();
    std::thread::scope(|sc| {
        for _ in 0..threads {
            sc.spawn(|| {
                let mut local = Vec::new();
                loop {
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(p) = files.get(i) else { break };
                    let uri = path_to_uri(p);
                    let lang = Lang::from_path(&uri);
                    let (Some(k), Ok(b)) = (file_kind(lang), std::fs::read(p)) else { continue };
                    let cst = crate::reader::parse_owned(String::from_utf8_lossy(&b).into_owned());
                    local.push((uri, lang, analyze_cst(cst, k, cfg, &defs, Options::external())));
                }
                out.lock().unwrap().extend(local);
            });
        }
    });
    let mut v = out.into_inner().unwrap();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}
