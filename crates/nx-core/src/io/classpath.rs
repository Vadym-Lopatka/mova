//! Classpath discovery: run each project type's command (same argv as clojure-lsp), union, cache on disk.
use super::project::{self, Settings, Spec};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Classpath {
    pub jars: Vec<String>,
    /// Non-jar entries as printed (may be relative to the project root).
    pub dirs: Vec<String>,
}

#[derive(Debug)]
pub struct Resolved {
    pub classpath: Classpath,
    pub from_cache: bool,
    /// Per-spec failures (`cmd`, stderr); failed specs contribute nothing and the result is not cached.
    pub errors: Vec<(String, String)>,
}

const MAGIC: &str = "NXCP1\n";

/// Cache key (32 hex): sha256 over root, each valid spec's argv, and the content of its dep files
/// (same inputs as clojure-lsp `project-specs->hash`, plus root+argv so different roots/aliases never collide).
pub fn key(root: &Path, settings: &Settings) -> Option<String> {
    let specs = project::discover(root, settings);
    if specs.is_empty() {
        return None;
    }
    let mut parts: Vec<Vec<u8>> = vec![MAGIC.as_bytes().to_vec(), root.as_os_str().as_encoded_bytes().to_vec()];
    for s in &specs {
        parts.push(s.cmd.join(" ").into_bytes());
        for f in project::dep_files(root, s, settings) {
            parts.push(f.as_os_str().as_encoded_bytes().to_vec());
            parts.push(std::fs::read(&f).unwrap_or_default());
        }
    }
    Some(super::hash_parts(parts.iter().map(|v| v.as_slice())))
}

fn cache_path(key: &str) -> PathBuf {
    super::cache_root().join("classpath").join(key)
}

fn parse(text: &str) -> Option<Classpath> {
    let body = text.strip_prefix(MAGIC)?;
    let mut cp = Classpath::default();
    let mut ended = false;
    for l in body.lines() {
        if l == "end" {
            ended = true;
        } else if let Some(p) = l.strip_prefix('j') {
            cp.jars.push(p.into())
        } else if let Some(p) = l.strip_prefix('d') {
            cp.dirs.push(p.into())
        } else {
            return None;
        }
    }
    ended.then_some(cp) // "end" marker guards against truncated files
}

fn write_cache(key: &str, cp: &Classpath) -> std::io::Result<()> {
    let p = cache_path(key);
    std::fs::create_dir_all(p.parent().unwrap())?;
    let mut s = String::from(MAGIC);
    for j in &cp.jars {
        s.push('j');
        s.push_str(j);
        s.push('\n');
    }
    for d in &cp.dirs {
        s.push('d');
        s.push_str(d);
        s.push('\n');
    }
    s.push_str("end\n");
    let tmp = p.with_extension(format!("tmp{}", std::process::id()));
    std::fs::File::create(&tmp)?.write_all(s.as_bytes())?;
    std::fs::rename(tmp, p)
}

/// Warm path: no subprocess. `None` if not cached.
pub fn lookup_cached(root: &Path, settings: &Settings) -> Option<Classpath> {
    let k = key(root, settings)?;
    parse(&std::fs::read_to_string(cache_path(&k)).ok()?)
}

/// Run one spec's command in `root`; classpath = last stdout line split on the path separator.
pub fn run_spec(root: &Path, spec: &Spec) -> Result<Vec<String>, String> {
    let out = Command::new(&spec.cmd[0]).args(&spec.cmd[1..]).current_dir(root).output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).into_owned());
    }
    let s = String::from_utf8_lossy(&out.stdout);
    let last = s.lines().last().unwrap_or("");
    let sep = if cfg!(windows) { ';' } else { ':' };
    Ok(last.split(sep).filter(|x| !x.is_empty()).map(String::from).collect())
}

/// Cold path: run all specs, union (order kept, deduped), split jars (`.jar` substring, like clojure-lsp) / dirs.
pub fn run(root: &Path, settings: &Settings) -> Resolved {
    let mut cp = Classpath::default();
    let mut errors = Vec::new();
    for s in project::discover(root, settings) {
        match run_spec(root, &s) {
            Ok(paths) => {
                for p in paths {
                    let v = if p.contains(".jar") { &mut cp.jars } else { &mut cp.dirs };
                    if !v.contains(&p) {
                        v.push(p)
                    }
                }
            }
            Err(e) => errors.push((s.cmd.join(" "), e)),
        }
    }
    Resolved { classpath: cp, from_cache: false, errors }
}

/// Cached lookup, else run the commands and cache the result (only if every spec succeeded).
pub fn resolve(root: &Path, settings: &Settings) -> Resolved {
    let k = key(root, settings);
    if let Some(k) = &k {
        if let Some(cp) = std::fs::read_to_string(cache_path(k)).ok().and_then(|t| parse(&t)) {
            return Resolved { classpath: cp, from_cache: true, errors: vec![] };
        }
    }
    let r = run(root, settings);
    if let (Some(k), true) = (&k, r.errors.is_empty()) {
        let _ = write_cache(k, &r.classpath);
    }
    r
}
