//! One-shot CLI over the engine (`nx` binary): project load, symbol resolver, output helpers. Contract: nx/CLI-DESIGN.md.
pub mod check;
pub mod def;
pub mod doc;
pub mod find;
pub mod hook;
pub mod ns;
pub mod outline;
pub mod refs;

use crate::analyzer::json::Json;
use crate::engine::{scan, ClientOpts, Engine, Snapshot};
use crate::intern::{intern, SymId};
use crate::query::{El, Q};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Parsed command line (everything after the command name lands in `args`).
#[derive(Default)]
pub struct Opts {
    pub root: Option<String>,
    pub in_file: Option<String>,
    pub json: bool,
    pub all: bool,
    pub new: bool,
    pub info: bool,
    pub errors: bool,
    pub args: Vec<String>,
}

/// What a command prints and the exit code (usage and project errors are `Err`, exit 2).
pub struct Reply {
    pub out: String,
    pub err: String,
    pub code: i32,
}

impl Reply {
    pub fn out(out: String, code: i32) -> Reply {
        Reply { out, err: String::new(), code }
    }
    pub fn err(err: String, code: i32) -> Reply {
        Reply { out: String::new(), err, code }
    }
}

/// Files and directories that mark a project root.
const MARKERS: [&str; 6] = ["mova.edn", "deps.edn", "bb.edn", "project.clj", "shadow-cljs.edn", ".git"];

/// Project root: `--root`, else the nearest ancestor of the cwd with a project marker, else the cwd.
pub fn find_root(root: Option<&str>) -> Result<PathBuf, String> {
    let cwd = std::env::current_dir().map_err(|e| format!("cwd: {e}"))?;
    let dir = match root {
        Some(r) => cwd.join(r),
        None => nearest_root(&cwd).unwrap_or(cwd),
    };
    dir.canonicalize().map_err(|e| format!("root {}: {e}", dir.display()))
}

/// Nearest ancestor of `start` (itself included) that has a project marker.
pub fn nearest_root(start: &Path) -> Option<PathBuf> {
    start.ancestors().find(|d| MARKERS.iter().any(|m| d.join(m).exists())).map(|d| d.to_path_buf())
}

/// A settled project: every source file analysed.
pub struct Project {
    pub e: Arc<Engine>,
    pub root: PathBuf,
}

impl Project {
    pub fn load(root: PathBuf) -> Project {
        let e = Engine::new(0);
        e.set_client_opts(ClientOpts { jar_scheme: true, arity_on_same_line: true, ..Default::default() });
        let (_, total, _) = e.analyze_project(&root);
        let mut done = 0;
        while done < total {
            let Some(r) = e.await_results(64) else { break };
            done += r.len();
            e.commit(&r);
        }
        Project { e, root }
    }
    pub fn snap(&self) -> Arc<Snapshot> {
        self.e.store.snapshot()
    }
    /// Wait for one queued analysis (an open text or a disk override) and commit it.
    pub fn settle_one(&self) {
        if let Some(r) = self.e.await_results(1) {
            self.e.commit(&r);
        }
    }
    /// Absolute path of a command-line file (relative to the cwd).
    pub fn file_arg(&self, a: &str) -> Result<PathBuf, String> {
        let cwd = std::env::current_dir().map_err(|e| format!("cwd: {e}"))?;
        cwd.join(a).canonicalize().map_err(|_| format!("no such file: {a}"))
    }
    /// A uri as a path a shell can open: relative to the project root for project files, `jar:` paths as they are.
    pub fn show(&self, uri: &str) -> String {
        match scan::uri_to_path(uri) {
            Some(p) => p.strip_prefix(&self.root).unwrap_or(&p).to_string_lossy().into_owned(),
            None => uri.strip_prefix("jar:file://").map_or(uri.to_string(), |r| format!("jar:{r}")),
        }
    }
}

/// Source files that git reports as changed or untracked under `root` (renamed: the new path; deleted: skipped).
fn changed_files(root: &Path) -> Option<Vec<PathBuf>> {
    let git = |args: &[&str]| std::process::Command::new("git").current_dir(root).args(args).output().ok().filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).into_owned());
    let top = PathBuf::from(git(&["rev-parse", "--show-toplevel"])?.trim()).canonicalize().ok()?;
    let status = git(&["status", "--porcelain", "-uall"])?;
    let files = status.lines().filter(|l| l.len() > 3 && !l[..2].contains('D')).map(|l| l[3..].rsplit(" -> ").next().unwrap_or("")).map(|p| top.join(p));
    Some(files.filter(|f| f.starts_with(root) && scan::is_source(f) && f.is_file()).collect())
}

/// Join items (each may span lines) up to `limit` lines; the rest becomes `... +N more (--all)`.
pub fn capped(items: &[String], all: bool, limit: usize) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut lines = 0;
    for it in items {
        lines += it.lines().count();
        if !all && lines > limit && !out.is_empty() {
            break;
        }
        out.push(it);
    }
    let mut s = out.join("\n");
    if out.len() < items.len() {
        s.push_str(&format!("\n... +{} more (--all)", items.len() - out.len()));
    }
    s
}

pub const LIST_CAP: usize = 40;

pub fn jstr(s: &str) -> Json {
    Json::Str(s.to_string())
}

pub fn jobj(v: Vec<(&str, Json)>) -> Json {
    Json::Obj(v.into_iter().map(|(k, j)| (k.to_string(), j)).collect())
}

pub fn json_text(j: &Json) -> String {
    let mut s = String::new();
    j.write(&mut s);
    s
}

/// What a symbol argument names.
pub enum Found {
    One(El),
    Many(Vec<El>),
    Missing,
}

/// The one var a `Found` names, else the reply: `not found`, `not a var`, or the candidate list (exit 1).
pub fn resolve_one(p: &Project, q: &Q, found: Found, sym: &str, all: bool) -> Result<El, Reply> {
    match found {
        Found::Missing => Err(Reply::err(format!("not found: {sym}\n"), 1)),
        Found::One(e) if e.b == crate::engine::index::B::VarDef => Ok(e),
        Found::One(_) => Err(Reply::err(format!("not a var: {sym}\n"), 1)),
        Found::Many(v) => {
            let lines: Vec<String> = v.iter().map(|e| def::head(p, q, *e).0).collect();
            Err(Reply::out(capped(&lines, all, LIST_CAP) + "\n", 1))
        }
    }
}

/// `file:line[:col]` (the file must exist): path, 1-based line, optional 1-based column.
fn parse_pos(sym: &str) -> Option<(PathBuf, u32, Option<u32>)> {
    let (rest, last) = sym.rsplit_once(':')?;
    let last: u32 = last.parse().ok()?;
    let (file, line, col) = match rest.rsplit_once(':').and_then(|(f, l)| Some((f, l.parse::<u32>().ok()?))) {
        Some((f, line)) => (f, line, Some(last)),
        None => (rest, last, None),
    };
    let path = std::env::current_dir().ok()?.join(file).canonicalize().ok()?;
    path.is_file().then_some((path, line, col))
}

/// Resolve `ns/name`, `suffix/name` (a namespace suffix), `name`, `alias/name` (with `in_file`) or `file:line[:col]`.
pub fn resolve(s: &Snapshot, sym: &str, in_file: Option<&Path>) -> Found {
    let q = Q::new(s);
    if let Some((path, line, col)) = parse_pos(sym) {
        return at_position(&q, &path, line, col);
    }
    let var = |ns: SymId, name: SymId| q.last_var_def(ns, name, crate::query::CLJ | crate::query::CLJS, false);
    if let Some((a, name)) = sym.split_once('/').filter(|(a, n)| !a.is_empty() && !n.is_empty()) {
        let (a, name) = (intern(a), intern(name));
        let by_alias = in_file.and_then(|f| s.get(&scan::path_to_uri(f))).and_then(|e| e.fa()).and_then(|fa| fa.namespace_usages.iter().find(|u| u.alias == a).map(|u| u.to));
        let exact = by_alias.and_then(|ns| var(ns, name)).or_else(|| var(a, name));
        if let Some(e) = exact {
            return Found::One(e);
        }
        let mut found: Vec<El> = suffix_order(s, &q, a, name).into_iter().filter_map(|ns| var(ns, name)).collect();
        return match found.len() {
            0 => Found::Missing,
            1 => Found::One(found.remove(0)),
            _ => Found::Many(found),
        };
    }
    let name = intern(sym);
    let (mut project, mut all): (Vec<SymId>, Vec<SymId>) = (Vec::new(), Vec::new());
    for (k, fs) in s.defs.iter().filter(|(k, _)| k.1 == name.0) {
        all.push(SymId(k.0));
        if fs.iter().any(|f| q.internal(*f)) {
            project.push(SymId(k.0));
        }
    }
    if let Some(j) = &s.jars {
        j.layer.jars.iter().for_each(|jar| jar.for_each_def(|r| if r.name == name { all.push(r.ns) }));
    }
    let mut found: Vec<El> = bare_order(project, all).into_iter().filter_map(|ns| var(ns, name)).collect();
    match found.len() {
        0 => Found::Missing,
        1 => Found::One(found.remove(0)),
        _ => Found::Many(found),
    }
}

/// The namespaces that define `name` and end with `suffix` on a segment boundary (`a.b` matches `x.a.b`, not `x.ya.b`): the project's, else the dependencies' (sorted, deduped).
fn suffix_order(s: &Snapshot, q: &Q, suffix: SymId, name: SymId) -> Vec<SymId> {
    let tail = format!(".{}", suffix.as_str());
    let hit = |ns: &str| ns.ends_with(&tail);
    let (mut project, mut all): (Vec<SymId>, Vec<SymId>) = (Vec::new(), Vec::new());
    for (k, fs) in s.defs.iter().filter(|(k, _)| k.1 == name.0 && hit(SymId(k.0).as_str())) {
        all.push(SymId(k.0));
        if fs.iter().any(|f| q.internal(*f)) {
            project.push(SymId(k.0));
        }
    }
    if let Some(j) = &s.jars {
        j.layer.jars.iter().for_each(|jar| jar.for_each_def(|r| if r.name == name && hit(r.ns.as_str()) { all.push(r.ns) }));
    }
    let v = if project.is_empty() { &mut all } else { &mut project };
    v.sort_by_key(|n| n.as_str());
    v.dedup();
    std::mem::take(v)
}

/// The namespaces a bare name means: the project's, else `clojure.core` / `cljs.core`, else every dependency (sorted, deduped).
pub fn bare_order(mut project: Vec<SymId>, mut all: Vec<SymId>) -> Vec<SymId> {
    let by_name = |v: &mut Vec<SymId>| {
        v.sort_by_key(|n| n.as_str());
        v.dedup();
    };
    by_name(&mut project);
    by_name(&mut all);
    if !project.is_empty() {
        return project;
    }
    match ["clojure.core", "cljs.core"].iter().find(|c| all.iter().any(|n| n.as_str() == **c)) {
        Some(c) => vec![intern(c)],
        None => all,
    }
}

/// The definition of the element under a position; without a column the first symbol on the line that resolves to a project definition (else to any).
fn at_position(q: &Q, path: &Path, row: u32, col: Option<u32>) -> Found {
    let uri = scan::path_to_uri(path);
    let width = std::fs::read_to_string(path).ok().and_then(|t| t.lines().nth((row as usize).saturating_sub(1)).map(|l| l.chars().count() as u32 + 1)).unwrap_or(1);
    let mut defs = (col.unwrap_or(1)..=col.unwrap_or(width)).filter_map(|c| q.first_under_cursor(&uri, row, c).and_then(|e| q.find_definition(e)));
    let first = defs.next();
    first.filter(|d| q.internal(d.f)).or_else(|| defs.find(|d| q.internal(d.f))).or(first).map_or(Found::Missing, Found::One)
}

/// Cut `s` to `n` characters (`...` marks a cut).
pub fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    s.chars().take(n.saturating_sub(3)).collect::<String>() + "..."
}

/// A test file by its project-relative path.
pub fn is_test(rel: &str) -> bool {
    format!("/{rel}").contains("/test/") || rel.contains("_test.")
}

/// `path:row` of a var definition (a path outside the project root is absolute).
pub fn loc(p: &Project, q: &Q, e: El) -> String {
    format!("{}:{}", p.show(q.uri(e.f)), q.fa(e.f).var_definitions[e.i as usize].name_pos.row)
}

/// First non-empty line of a doc, trimmed.
pub fn first_line(doc: &str) -> &str {
    doc.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn syms(v: &[&str]) -> Vec<SymId> {
        v.iter().map(|s| intern(s)).collect()
    }
    fn names(v: Vec<SymId>) -> Vec<&'static str> {
        v.into_iter().map(|s| s.as_str()).collect()
    }

    #[test]
    fn bare_name_prefers_project_then_core_then_all() {
        let all = syms(&["some.jar", "clojure.core", "cljs.core", "app.core"]);
        assert_eq!(names(bare_order(syms(&["app.core"]), all.clone())), ["app.core"]);
        assert_eq!(names(bare_order(vec![], all)), ["clojure.core"]);
        assert_eq!(names(bare_order(vec![], syms(&["z.lib", "cljs.core", "a.lib"]))), ["cljs.core"]);
        assert_eq!(names(bare_order(vec![], syms(&["z.lib", "a.lib", "z.lib"]))), ["a.lib", "z.lib"]);
    }

    #[test]
    fn root_is_the_nearest_marker() {
        let base = std::env::temp_dir().join(format!("nx-root-{}", std::process::id()));
        let sub = base.join("a/b/c");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::create_dir_all(base.join(".git")).unwrap();
        assert_eq!(nearest_root(&sub), Some(base.clone()));
        std::fs::write(base.join("a/b/mova.edn"), "{}").unwrap();
        assert_eq!(nearest_root(&sub), Some(base.join("a/b")));
        let _ = std::fs::remove_dir_all(&base);
    }
}
