//! Mova project support: `.mova` files are Clojure sources; the runtime is Mova, not the JVM.
//! The layer = Mova stdlib sources (`core/core.mova`, ...) + Rust natives (`mova --source-index`) + the project's own
//! host natives (`Engine::register_fn*("name", ..)` in its Rust sources), installed as external files.
use crate::analyzer::json::{self, Json};
use crate::analyzer::{analyze_cst, Config, DefsIndex, FileAnalysis, FileKind, Options};
use crate::cst::Pos;
use crate::engine::scan::path_to_uri;
use crate::engine::types::Lang;
use crate::intern::{intern, SymId};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Rank of a layer file for `definition`: a stdlib `.mova` def replaces the native at load time, so it wins.
pub const RANK_NATIVE: u8 = 1;
pub const RANK_STDLIB: u8 = 2;

#[derive(Clone, Debug, PartialEq)]
pub struct Native {
    pub ns: String,
    pub name: String,
    /// Relative to the index root (index natives) or absolute (host natives).
    pub file: String,
    /// 1-based.
    pub line: u32,
}

/// `mova --source-index` (schema v1).
#[derive(Clone, Debug, Default)]
pub struct Index {
    pub root: PathBuf,
    /// (namespace, file relative to root)
    pub namespaces: Vec<(String, String)>,
    pub natives: Vec<Native>,
    /// Second spellings of a var: (ns, name, target ns, target name) (`clojure.core.async/chan` = bare `chan`).
    pub aliases: Vec<(String, String, String, String)>,
    /// Aliases usable without a require: (alias, namespace) (`async` -> `clojure.core.async`).
    pub default_aliases: Vec<(String, String)>,
}

pub struct LayerFile {
    pub uri: String,
    pub lang: Lang,
    pub fa: FileAnalysis,
    pub rank: u8,
}

#[derive(Default)]
pub struct Layer {
    pub files: Vec<LayerFile>,
    /// Namespaces usable without a require: (written prefix, namespace). Namespaces that hold natives
    /// (`mova.fs`, `clojure.string`) map to themselves; default aliases (`async`) to their namespace.
    pub auto_ns: Vec<(SymId, SymId)>,
    /// Stdlib files with no `ns` form: uri -> namespace of their forms (`core/core.mova` -> `clojure.core`).
    pub init_ns: Vec<(String, SymId)>,
    /// Mova checkout the locations point into.
    pub root: PathBuf,
}

pub fn parse_index(s: &str) -> Option<Index> {
    let j = json::parse(s)?;
    let root = PathBuf::from(j.get("root")?.as_str()?);
    let mut ix = Index { root, ..Default::default() };
    for n in j.get("namespaces").and_then(Json::as_arr).unwrap_or(&[]) {
        if let (Some(ns), Some(f)) = (n.get("ns").and_then(Json::as_str), n.get("file").and_then(Json::as_str)) {
            ix.namespaces.push((ns.to_string(), f.to_string()));
        }
    }
    for n in j.get("natives").and_then(Json::as_arr).unwrap_or(&[]) {
        if let (Some(ns), Some(name), Some(f), Some(l)) = (n.get("ns").and_then(Json::as_str), n.get("name").and_then(Json::as_str), n.get("file").and_then(Json::as_str), n.get("line").and_then(Json::as_f64)) {
            ix.natives.push(Native { ns: ns.to_string(), name: name.to_string(), file: f.to_string(), line: l as u32 });
        }
    }
    let st = |n: &Json, k: &str| n.get(k).and_then(Json::as_str).map(str::to_string);
    for n in j.get("aliases").and_then(Json::as_arr).unwrap_or(&[]) {
        if let (Some(ns), Some(name), Some(tns), Some(to)) = (st(n, "ns"), st(n, "name"), st(n, "to_ns"), st(n, "to")) {
            ix.aliases.push((ns, name, tns, to));
        }
    }
    for n in j.get("default_aliases").and_then(Json::as_arr).unwrap_or(&[]) {
        if let (Some(a), Some(ns)) = (st(n, "alias"), st(n, "ns")) {
            ix.default_aliases.push((a, ns));
        }
    }
    Some(ix)
}

/// The Mova binary that can print the index: `MOVA_BIN`, else this process when it is a `mova` binary.
fn mova_bin() -> Option<PathBuf> {
    if let Some(b) = std::env::var_os("MOVA_BIN").filter(|b| !b.is_empty()) {
        return Some(PathBuf::from(b));
    }
    if let Some(exe) = std::env::current_exe().ok().filter(|e| e.file_name().and_then(|n| n.to_str()).map_or(false, |n| n.starts_with("mova"))) {
        return Some(exe);
    }
    on_path("mova")
}

/// First executable file called `name` on `PATH`.
fn on_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?).map(|d| d.join(name)).find(|p| p.is_file())
}

/// Public vars of `clojure.core` in a fresh `mova` process: the fallback for names no index lists
/// (no `mova` binary found, or an index of another revision). Regenerate with `tools/gen_mova_core.sh`.
const CORE_VARS: &str = include_str!("core_vars.txt");

/// Index text: `MOVA_SOURCE_INDEX=<json file>`, else `<mova> --source-index` cached by binary size + mtime.
fn index_text() -> Option<String> {
    if let Some(p) = std::env::var_os("MOVA_SOURCE_INDEX").filter(|p| !p.is_empty()) {
        return std::fs::read_to_string(p).ok();
    }
    let bin = mova_bin()?;
    let md = std::fs::metadata(&bin).ok()?;
    let mtime = md.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos();
    let dir = crate::io::cache_root().join("mova");
    let cached = dir.join(format!("srcindex-{}-{}.json", md.len(), mtime));
    if let Ok(s) = std::fs::read_to_string(&cached) {
        return Some(s);
    }
    let out = std::process::Command::new(&bin).arg("--source-index").stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null()).env_remove("MOVA_IMAGE").env_remove("MOVA_IMAGE_PRELOAD").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    parse_index(&s)?; // never cache garbage
    let _ = std::fs::create_dir_all(&dir);
    let tmp = dir.join(format!(".srcindex-{}.tmp", std::process::id()));
    if std::fs::write(&tmp, &s).is_ok() {
        let _ = std::fs::rename(&tmp, &cached);
    }
    Some(s)
}

pub fn load_index() -> Option<Index> {
    parse_index(&index_text()?)
}

fn is_checkout(p: &Path) -> bool {
    p.join("core/core.mova").is_file()
}

fn skip_dir(name: &str) -> bool {
    name.starts_with('.') || name.starts_with("target") || name == "node_modules"
}

fn walk_ext(dir: &Path, ext: &str, exact: Option<&str>, depth: u32, max: u32, skip: &dyn Fn(&str) -> bool, out: &mut Vec<PathBuf>) {
    if depth > max {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let name = e.file_name();
        let Some(name) = name.to_str() else { continue };
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() {
            if !skip_dir(name) && !skip(name) {
                walk_ext(&e.path(), ext, exact, depth + 1, max, skip, out);
            }
        } else if exact.map_or(name.ends_with(ext), |x| name == x) {
            out.push(e.path());
        }
    }
}

/// `mova = { path = ".." }` of a Cargo.toml text.
fn cargo_mova_path(toml: &str) -> Option<&str> {
    for l in toml.lines() {
        let l = l.trim_start();
        let Some(rest) = l.strip_prefix("mova") else { continue };
        let rest = rest.trim_start();
        if !rest.starts_with('=') {
            continue;
        }
        let i = rest.find("path")?;
        let r = rest[i + 4..].trim_start().strip_prefix('=')?.trim_start().strip_prefix('"')?;
        return r.split('"').next();
    }
    None
}

/// The Mova checkout of this project: the project itself, else the `mova` path dependency of one of its
/// Cargo.toml files, else the tree the running binary was built from.
pub fn mova_root(project: &Path, index_root: &Path) -> PathBuf {
    if is_checkout(project) {
        return project.to_path_buf();
    }
    let mut tomls = Vec::new();
    walk_ext(project, "", Some("Cargo.toml"), 0, 3, &|_| false, &mut tomls);
    tomls.sort();
    for t in tomls {
        let Ok(s) = std::fs::read_to_string(&t) else { continue };
        if let Some(p) = cargo_mova_path(&s) {
            let r = t.parent().unwrap_or(project).join(p);
            if let Ok(r) = std::fs::canonicalize(&r) {
                if is_checkout(&r) {
                    return r;
                }
            }
        }
    }
    index_root.to_path_buf()
}

/// Line (1-based) of the registration of `name` near `line`: the index line when the name is on it or just below,
/// else the nearest line that has the quoted name (another revision of the same file).
fn relocate(lines: &[&str], line: u32, name: &str) -> u32 {
    let q = format!("\"{name}\"");
    let at = line.saturating_sub(1) as usize;
    if (at..(at + 4).min(lines.len())).any(|i| lines[i].contains(&q)) {
        return line;
    }
    let mut best: Option<usize> = None;
    for (i, l) in lines.iter().enumerate() {
        if l.contains(&q) && best.map_or(true, |b| i.abs_diff(at) < b.abs_diff(at)) {
            best = Some(i);
        }
    }
    best.map_or(line, |i| call_start(lines, i) as u32 + 1)
}

/// A name alone on its line (`"assoc",`) belongs to a call opened up to 3 lines above: index of that line.
fn call_start(lines: &[&str], i: usize) -> usize {
    if !lines[i].trim_start().starts_with('"') {
        return i;
    }
    (i.saturating_sub(3)..i).rev().find(|&j| lines[j].trim_end().ends_with('(')).unwrap_or(i)
}

/// Host natives of the project: `register_fn("name"`, `register_fn_with_arity("name"`, ... in its Rust sources
/// (Mova's embed API). Test and example dirs are skipped.
pub fn host_natives(project: &Path) -> Vec<Native> {
    let mut files = Vec::new();
    walk_ext(project, ".rs", None, 0, 8, &|n| matches!(n, "tests" | "benches" | "examples"), &mut files);
    files.sort();
    let re = regex::Regex::new(r#"\bregister_fn(?:_with_arity|_with_reentry)?\s*\(\s*"([^"\\]+)""#).unwrap();
    let mut out = Vec::new();
    for f in files {
        let Ok(s) = std::fs::read_to_string(&f) else { continue };
        if !s.contains("register_fn") {
            continue;
        }
        for c in re.captures_iter(&s) {
            let full = c.get(1).unwrap().as_str();
            let line = s[..c.get(0).unwrap().start()].bytes().filter(|b| *b == b'\n').count() as u32 + 1;
            let (ns, name) = match full.split_once('/') {
                Some((ns, n)) if !ns.is_empty() && !n.is_empty() => (ns, n),
                _ => ("clojure.core", full),
            };
            out.push(Native { ns: ns.to_string(), name: name.to_string(), file: f.to_string_lossy().into_owned(), line });
        }
    }
    out
}

/// A namespace name that is really a host class (`Math`, `java.lang.Math`): its members are interop, not vars.
fn class_ns(ns: &str) -> bool {
    ns.rsplit('.').next().and_then(|s| s.chars().next()).map_or(true, |c| c.is_uppercase())
}

fn symbol_ok(s: &str) -> bool {
    !s.is_empty() && !s.chars().any(|c| c.is_whitespace() || matches!(c, '(' | ')' | '[' | ']' | '{' | '}' | '"' | ';' | '`' | '~' | '@' | '^' | '\\' | ','))
}

/// One synthetic file for a Rust source: a var definition per native, placed at its registration line.
fn native_stub(natives: &[&Native], cfg: &Config) -> Option<FileAnalysis> {
    let mut by_ns: Vec<(&str, Vec<&Native>)> = Vec::new();
    for n in natives {
        if class_ns(&n.ns) || !symbol_ok(&n.ns) || !symbol_ok(&n.name) {
            continue;
        }
        match by_ns.iter_mut().find(|(ns, _)| *ns == n.ns) {
            Some((_, v)) => v.push(n),
            None => by_ns.push((&n.ns, vec![n])),
        }
    }
    if by_ns.is_empty() {
        return None;
    }
    let mut text = String::new();
    for (ns, v) in &by_ns {
        text.push_str(&format!("(ns {ns})\n"));
        for n in v {
            text.push_str(&format!("(def {})\n", n.name));
        }
    }
    let mut fa = analyze_cst(crate::reader::parse_owned(text), FileKind::Clj, cfg, &DefsIndex::new(), Options::external());
    let mut line: HashMap<(SymId, SymId), u32> = HashMap::new();
    let mut first: HashMap<SymId, u32> = HashMap::new();
    for (ns, v) in &by_ns {
        let nsid = intern(ns);
        for n in v {
            line.entry((nsid, intern(&n.name))).or_insert(n.line);
            let e = first.entry(nsid).or_insert(n.line);
            *e = (*e).min(n.line);
        }
    }
    let at = |l: u32| Pos { row: l, col: 1, end_row: l, end_col: 1 };
    fa.var_definitions.retain(|d| line.contains_key(&(d.ns, d.name)));
    for d in &mut fa.var_definitions {
        let l = line[&(d.ns, d.name)];
        (d.pos, d.name_pos) = (at(l), at(l));
    }
    for d in &mut fa.namespace_definitions {
        let l = first.get(&d.name).copied().unwrap_or(1);
        (d.pos, d.name_pos) = (at(l), at(l));
    }
    Some(fa)
}

/// A Mova project: `mova.edn` at the root or a `.mova` source file.
pub fn detect(root: &Path, files: &[PathBuf]) -> bool {
    root.join("mova.edn").is_file() || files.iter().any(|f| f.extension().map_or(false, |e| e == "mova"))
}

/// Top-level directories of `root` that hold `.mova` files (Mova's `--module-path` has no project file: any
/// directory can be a module root). Checked only when something says the project is Mova.
pub fn module_dirs(root: &Path, known_mova: bool) -> Vec<PathBuf> {
    let hint = known_mova
        || root.join("mova.edn").is_file()
        || is_checkout(root)
        || (root.join("Cargo.toml").is_file() && !["deps.edn", "project.clj", "shadow-cljs.edn", "build.boot"].iter().any(|f| root.join(f).is_file()));
    if !hint {
        return Vec::new();
    }
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(root) else { return out };
    for e in rd.flatten() {
        let name = e.file_name();
        let Some(name) = name.to_str() else { continue };
        if !e.file_type().map_or(false, |t| t.is_dir()) || skip_dir(name) {
            continue;
        }
        let mut found = Vec::new();
        walk_ext(&e.path(), ".mova", None, 0, 12, &|_| false, &mut found);
        if !found.is_empty() {
            out.push(e.path());
        }
    }
    out.sort();
    out
}

/// Module root of a `.mova` file, from its `ns` form: `<root>/a/b_c.mova` holds `(ns a.b-c)`.
pub fn module_root(path: &Path, text: &str) -> Option<PathBuf> {
    let i = if text.starts_with("(ns") { 0 } else { text.find("\n(ns")? + 1 };
    let rest = text[i + 3..].strip_prefix(|c: char| c.is_whitespace())?.trim_start();
    let name: String = rest.chars().take_while(|c| !c.is_whitespace() && *c != ')').collect();
    if name.is_empty() || !symbol_ok(&name) {
        return None; // metadata before the name, or not a name
    }
    let suffix = format!("{}.mova", name.replace('.', "/").replace('-', "_"));
    let p = path.to_str()?;
    let root = p.strip_suffix(&suffix)?.strip_suffix('/')?;
    (!root.is_empty()).then(|| PathBuf::from(root))
}

/// Build the layer for the project at `project`. `skip` = project source paths (their files are project files).
pub fn build_layer(project: &Path, skip: &[PathBuf], cfg: &Config) -> Option<Layer> {
    let ix = load_index().unwrap_or_default();
    let host = host_natives(project);
    let root = mova_root(project, &ix.root);
    let relocated = root != ix.root;
    let mut layer = Layer { root: root.clone(), ..Default::default() };
    let inside = |p: &Path| skip.iter().any(|s| p.starts_with(s));
    // stdlib sources
    for (ns, rel) in &ix.namespaces {
        let p = root.join(rel);
        let Ok(text) = std::fs::read_to_string(&p) else { continue };
        let p = std::fs::canonicalize(&p).unwrap_or(p);
        let uri = path_to_uri(&p);
        let nsid = intern(ns);
        layer.init_ns.push((uri.clone(), nsid));
        if inside(&p) {
            continue; // the project is the Mova checkout: a project file
        }
        let lang = Lang::from_path(&uri);
        let Some(kind) = crate::engine::analyze::file_kind(lang) else { continue };
        let mut opts = Options::external();
        opts.init_ns = nsid;
        opts.mova = uri.ends_with(".mova");
        let mut fa = analyze_cst(crate::reader::parse_owned(text), kind, cfg, &DefsIndex::new(), opts);
        if !fa.namespace_definitions.iter().any(|d| d.name == nsid) {
            // no `ns` form (core/core.mova): the namespace is defined by the file itself
            let at = Pos { row: 1, col: 1, end_row: 1, end_col: 1 };
            let none = crate::analyzer::types::Val::NONE;
            fa.namespace_definitions.push(crate::analyzer::types::NsDef { pos: at, name_pos: at, name: nsid, doc: SymId::NONE, no_doc: none, deprecated: none, added: none, author: none, in_ns: false, lang: 0 });
        }
        layer.files.push(LayerFile { uri, lang, fa, rank: RANK_STDLIB });
    }
    // aliases: a native target gets a second native entry; a stdlib target a second var definition in its file
    let mut alias_natives = Vec::new();
    for (ns, name, tns, to) in &ix.aliases {
        if let Some(n) = ix.natives.iter().find(|n| n.ns == *tns && n.name == *to) {
            alias_natives.push(Native { ns: ns.clone(), name: name.clone(), file: n.file.clone(), line: n.line });
            continue;
        }
        let (nsid, nameid, tnsid, toid) = (intern(ns), intern(name), intern(tns), intern(to));
        for f in layer.files.iter_mut() {
            if let Some(d) = f.fa.var_definitions.iter().find(|d| d.ns == tnsid && d.name == toid).copied() {
                f.fa.var_definitions.push(crate::analyzer::types::VarDef { ns: nsid, name: nameid, ..d });
                break;
            }
        }
    }
    for (a, ns) in &ix.default_aliases {
        layer.auto_ns.push((intern(a), intern(ns)));
    }
    // natives, one stub per Rust file
    let mut by_file: Vec<(PathBuf, Vec<Native>)> = Vec::new();
    let mut add = |p: PathBuf, n: Native| match by_file.iter_mut().find(|(f, _)| *f == p) {
        Some((_, v)) => v.push(n),
        None => by_file.push((p, vec![n])),
    };
    for n in ix.natives.iter().chain(&alias_natives) {
        add(root.join(&n.file), n.clone());
    }
    for n in host {
        add(PathBuf::from(&n.file), n);
    }
    for (p, mut v) in by_file {
        if relocated {
            if let Ok(s) = std::fs::read_to_string(&p) {
                let lines: Vec<&str> = s.lines().collect();
                for n in &mut v {
                    n.line = relocate(&lines, n.line, &n.name);
                }
            }
        }
        for n in &v {
            if !class_ns(&n.ns) {
                let ns = intern(&n.ns);
                if !layer.auto_ns.contains(&(ns, ns)) {
                    layer.auto_ns.push((ns, ns));
                }
            }
        }
        let refs: Vec<&Native> = v.iter().collect();
        if let Some(fa) = native_stub(&refs, cfg) {
            let p = std::fs::canonicalize(&p).unwrap_or(p);
            layer.files.push(LayerFile { uri: path_to_uri(&p), lang: Lang::Clj, fa, rank: RANK_NATIVE });
        }
    }
    // core vars no layer file defines (index missing or of another revision): names only, no location
    let core = intern("clojure.core");
    let defined: std::collections::HashSet<SymId> = layer.files.iter().flat_map(|f| f.fa.var_definitions.iter()).filter(|d| d.ns == core).map(|d| d.name).collect();
    let extra = missing_core_names(&defined);
    if !extra.is_empty() {
        let natives: Vec<Native> = extra.into_iter().map(|name| Native { ns: "clojure.core".into(), name, file: String::new(), line: 1 }).collect();
        if let Some(fa) = native_stub(&natives.iter().collect::<Vec<_>>(), cfg) {
            layer.files.push(LayerFile { uri: BUILTIN_URI.to_string(), lang: Lang::Clj, fa, rank: RANK_NATIVE });
        }
    }
    Some(layer)
}

/// Is `name` a public var of `clojure.core` in a fresh `mova` process?
pub fn is_core_var(name: &str) -> bool {
    static SET: std::sync::OnceLock<std::collections::HashSet<&'static str>> = std::sync::OnceLock::new();
    SET.get_or_init(|| CORE_VARS.lines().collect()).contains(name)
}

/// Home of core vars known only by name.
pub const BUILTIN_URI: &str = "file:///nx-mova-builtin/core.mova";

/// Names of [`CORE_VARS`] that neither `defined` nor the Clojure core table has (class names and `a.b.C` skipped).
pub fn missing_core_names(defined: &std::collections::HashSet<SymId>) -> Vec<String> {
    CORE_VARS.lines().filter(|n| !n.contains('.') || n.len() == 1).filter(|n| !n.starts_with(|c: char| c.is_uppercase())).filter(|n| !defined.contains(&intern(n)) && !crate::analyzer::defs::core_sym(false, intern(n))).map(str::to_string).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `NX_MOVA_ROOT=<project> cargo test --release --lib mova::tests::time_layer -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn time_layer() {
        let root = PathBuf::from(std::env::var("NX_MOVA_ROOT").unwrap());
        let t = std::time::Instant::now();
        let ix = load_index().unwrap_or_default();
        eprintln!("index {:?}: {} natives, {} namespaces", t.elapsed(), ix.natives.len(), ix.namespaces.len());
        let t = std::time::Instant::now();
        let h = host_natives(&root);
        eprintln!("host natives {:?}: {}", t.elapsed(), h.len());
        let t = std::time::Instant::now();
        let r = mova_root(&root, &ix.root);
        eprintln!("mova root {:?}: {}", t.elapsed(), r.display());
        let t = std::time::Instant::now();
        let d = module_dirs(&root, true);
        eprintln!("module dirs {:?}: {:?}", t.elapsed(), d);
        let t = std::time::Instant::now();
        let l = build_layer(&root, &[], &Config::new()).unwrap();
        eprintln!("layer {:?}: {} files, {} defs", t.elapsed(), l.files.len(), l.files.iter().map(|f| f.fa.var_definitions.len()).sum::<usize>());
    }

    #[test]
    fn index_parses() {
        let ix = parse_index(r#"{"v":1,"root":"/m","namespaces":[{"ns":"clojure.core","file":"core/core.mova"}],"natives":[{"ns":"clojure.core","name":"assoc","file":"src/b.rs","line":12}]}"#).unwrap();
        assert_eq!(ix.root, PathBuf::from("/m"));
        assert_eq!(ix.namespaces, vec![("clojure.core".to_string(), "core/core.mova".to_string())]);
        assert_eq!(ix.natives[0], Native { ns: "clojure.core".into(), name: "assoc".into(), file: "src/b.rs".into(), line: 12 });
    }

    #[test]
    fn cargo_path_dep() {
        assert_eq!(cargo_mova_path("[dependencies]\n# c\nmova = { path = \"../../mova\" }\nlibc = \"0.2\"\n"), Some("../../mova"));
        assert_eq!(cargo_mova_path("mova-derive = { path = \"x\" }\n"), None);
        assert_eq!(cargo_mova_path("mova = \"0.3\"\n"), None);
    }

    #[test]
    fn relocate_keeps_or_finds_nearest() {
        let l = ["reg(", "  i,", "  \"assoc\",", "x", "reg(i, \"get\", f);", "reg(i, \"assoc\", g);"];
        assert_eq!(relocate(&l, 1, "assoc"), 1); // name just below the call line
        assert_eq!(relocate(&l, 2, "get"), 2); // within the window
        assert_eq!(relocate(&l, 6, "get"), 5);
        assert_eq!(relocate(&["x", "y", "z", "q", "w", "reg(", "  i,", "  \"assoc\","], 1, "assoc"), 6); // back to the call line
        assert_eq!(relocate(&l, 4, "nope"), 4);
    }

    #[test]
    fn module_root_from_ns() {
        let p = Path::new("/r/nx/src/nx/core/obs_x.mova");
        assert_eq!(module_root(p, ";; c\n(ns nx.core.obs-x\n  (:require [a.b]))\n"), Some(PathBuf::from("/r/nx/src")));
        assert_eq!(module_root(p, "(ns nx.core.obs-x)"), Some(PathBuf::from("/r/nx/src")));
        assert_eq!(module_root(p, "(ns other.name)\n"), None);
        assert_eq!(module_root(p, "(def x 1)\n"), None);
        assert_eq!(module_root(p, "(ns ^:meta nx.core.obs-x)\n"), None);
    }

    #[test]
    fn stub_has_defs_at_lines() {
        let n = |ns: &str, name: &str, line| Native { ns: ns.into(), name: name.into(), file: "a.rs".into(), line };
        let v = [n("clojure.core", "time-ms", 35), n("mova.fs", "glob", 7), n("Math", "sqrt", 9), n("clojure.core", "/", 40)];
        let fa = native_stub(&v.iter().collect::<Vec<_>>(), &Config::new()).unwrap();
        let got: Vec<(String, String, u32)> = fa.var_definitions.iter().map(|d| (d.ns.as_str().to_string(), d.name.as_str().to_string(), d.name_pos.row)).collect();
        assert_eq!(got, vec![("clojure.core".into(), "time-ms".into(), 35), ("clojure.core".into(), "/".into(), 40), ("mova.fs".into(), "glob".into(), 7)]);
        assert_eq!(fa.namespace_definitions.len(), 2);
    }

    #[test]
    fn host_natives_from_rust() {
        let d = std::env::temp_dir().join(format!("nx-mova-host-{}", std::process::id()));
        std::fs::create_dir_all(d.join("native/src")).unwrap();
        std::fs::create_dir_all(d.join("tests")).unwrap();
        std::fs::write(d.join("native/src/a.rs"), "fn r(e: &mut Engine) {\n    e.register_fn_with_arity(\n        \"pg-sleep\",\n        Arity::Exact(1), f);\n    e.register_fn(\"host/now\", g);\n}\n").unwrap();
        std::fs::write(d.join("tests/t.rs"), "e.register_fn(\"only-in-test\", g);\n").unwrap();
        let v = host_natives(&d);
        let got: Vec<(&str, &str, u32)> = v.iter().map(|n| (n.ns.as_str(), n.name.as_str(), n.line)).collect();
        assert_eq!(got, vec![("clojure.core", "pg-sleep", 2), ("host", "now", 5)]);
        let _ = std::fs::remove_dir_all(&d);
    }
}
