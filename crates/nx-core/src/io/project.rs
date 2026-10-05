//! Project discovery with clojure-lsp semantics (classpath.clj `default-project-specs`,
//! source_paths.clj `process-source-paths`, startup.clj settings defaults).
use super::edn::{self, Edn};
use std::path::{Component, Path, PathBuf};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Lein,
    Deps,
    Boot,
    Shadow,
    Bb,
    Squint,
}

/// One project type: the marker file and the classpath command (argv) clojure-lsp runs in the root.
#[derive(Clone, Debug)]
pub struct Spec {
    pub kind: Kind,
    pub file: &'static str,
    pub cmd: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct Settings {
    /// `:source-aliases` (default `#{:dev :test}`), joined as `-A:dev:test` / `+dev,+test`.
    pub source_aliases: Vec<String>,
    /// `:source-paths` from config (overrides classpath-derived paths).
    pub source_paths: Option<Vec<String>>,
    /// `:source-paths-ignore-regex` (default `["target.*"]`), full-match vs path relative to root.
    pub ignore_regex: Vec<String>,
    /// `:lint-project-files-after-startup?` (default true): publish diagnostics of all project files after startup.
    pub lint_after_startup: bool,
    /// `:linters {:clojure-lsp/unused-public-var {...}}`.
    pub upv: UpvCfg,
}

/// clojure-lsp's own `unused-public-var` linter settings (level + exclusions).
#[derive(Clone, Debug, Default)]
pub struct UpvCfg {
    /// 0 off, 1 info, 2 warning, 3 error; None = default (info).
    pub level: Option<u8>,
    /// simple `:exclude` symbols (var name or namespace) and qualified ones (`ns/var`).
    pub exclude_simple: Vec<String>,
    pub exclude_fq: Vec<String>,
    pub exclude_regex: Vec<regex::Regex>,
    pub exclude_when_defined_by: Vec<String>,
    pub exclude_when_defined_by_regex: Vec<regex::Regex>,
}

fn full_regexes(v: Option<&Edn>) -> Vec<regex::Regex> {
    v.map(|v| v.items().iter().filter_map(|e| regex::Regex::new(&format!("^(?:{})$", e.as_str()?)).ok()).collect()).unwrap_or_default()
}

impl UpvCfg {
    /// Merge one `:clojure-lsp/unused-public-var` map (later sources win per key).
    fn merge(&mut self, m: &Edn) {
        if let Some(Edn::Kw(l)) = m.get("level") {
            self.level = match l.as_str() {
                "off" => Some(0),
                "info" => Some(1),
                "warning" => Some(2),
                "error" => Some(3),
                _ => self.level,
            };
        }
        if let Some(v) = m.get("exclude") {
            let all = v.strs();
            self.exclude_simple = all.iter().filter(|s| !s.contains('/')).cloned().collect();
            self.exclude_fq = all.into_iter().filter(|s| s.contains('/')).collect();
        }
        if let Some(v) = m.get("exclude-regex") {
            self.exclude_regex = full_regexes(Some(v));
        }
        if let Some(v) = m.get("exclude-when-defined-by") {
            self.exclude_when_defined_by = v.strs();
        }
        if m.get("exclude-when-defined-by-regex").is_some() {
            self.exclude_when_defined_by_regex = full_regexes(m.get("exclude-when-defined-by-regex"));
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Settings { source_aliases: vec!["dev".into(), "test".into()], source_paths: None, ignore_regex: vec!["target.*".into()], lint_after_startup: true, upv: UpvCfg::default() }
    }
}

impl Settings {
    /// Global (`$XDG_CONFIG_HOME|~/.config/clojure-lsp/config.edn`) then `<root>/.lsp/config.edn` (project wins).
    pub fn load(root: &Path) -> Settings {
        let mut s = Settings::default();
        let mut files = Vec::new();
        let cfg = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")));
        if let Some(c) = cfg {
            files.push(c.join("clojure-lsp/config.edn"));
        }
        files.push(root.join(".lsp/config.edn"));
        for f in files {
            let Some(e) = edn::read_file(&f) else { continue };
            if let Some(v) = e.get("source-aliases") {
                let a = v.strs();
                if !a.is_empty() {
                    s.source_aliases = a;
                }
            }
            if let Some(v) = e.get("source-paths") {
                s.source_paths = Some(v.strs());
            }
            if let Some(crate::io::edn::Edn::Bool(b)) = e.get("lint-project-files-after-startup?") {
                s.lint_after_startup = *b;
            }
            if let Some(m) = e.get("linters").and_then(|l| l.get("clojure-lsp/unused-public-var")) {
                s.upv.merge(m);
            }
            if let Some(v) = e.get("source-paths-ignore-regex") {
                s.ignore_regex = v.strs();
            }
        }
        s
    }
}

/// All project types, in clojure-lsp order (lein, deps, boot, shadow, bb, squint).
pub fn default_specs(aliases: &[String]) -> Vec<Spec> {
    let lein: Option<Vec<String>> = (!aliases.is_empty()).then(|| vec!["with-profile".into(), aliases.iter().map(|a| format!("+{a}")).collect::<Vec<_>>().join(",")]);
    let deps_a: Option<String> = (!aliases.is_empty()).then(|| format!("-A:{}", aliases.join(":")));
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    let mut lein_cmd = s(&["lein"]);
    lein_cmd.extend(lein.unwrap_or_default());
    lein_cmd.push("classpath".into());
    let mut deps_cmd = s(&["clojure"]);
    deps_cmd.extend(deps_a.clone());
    deps_cmd.push("-Spath".into());
    let mut squint_cmd = s(&["clojure", "-Sdeps", "squint.edn", "-Spath"]);
    squint_cmd.extend(deps_a);
    vec![
        Spec { kind: Kind::Lein, file: "project.clj", cmd: lein_cmd },
        Spec { kind: Kind::Deps, file: "deps.edn", cmd: deps_cmd },
        Spec { kind: Kind::Boot, file: "build.boot", cmd: s(&["boot", "show", "--fake-classpath"]) },
        Spec { kind: Kind::Shadow, file: "shadow-cljs.edn", cmd: s(&["npx", "shadow-cljs", "classpath"]) },
        Spec { kind: Kind::Bb, file: "bb.edn", cmd: s(&["bb", "print-deps", "--format", "classpath"]) },
        Spec { kind: Kind::Squint, file: "squint.edn", cmd: squint_cmd },
    ]
}

/// Specs whose marker file exists in `root` (clojure-lsp runs ALL of them and unions the results).
pub fn discover(root: &Path, settings: &Settings) -> Vec<Spec> {
    default_specs(&settings.source_aliases).into_iter().filter(|s| root.join(s.file).is_file()).collect()
}

fn local_roots(deps: Option<&Edn>) -> Vec<String> {
    let Some(d) = deps else { return vec![] };
    d.entries().iter().filter_map(|(_, v)| v.get("local/root")?.as_str().map(String::from)).collect()
}

fn normalize(p: &Path) -> PathBuf {
    let mut o = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !o.pop() {
                    o.push("..")
                }
            }
            c => o.push(c.as_os_str()),
        }
    }
    o
}

/// Files whose content keys the classpath cache: the marker file, plus (deps.edn only) the
/// `deps.edn` of each `:local/root` dep from top level and `source-aliases` (source_paths.clj).
pub fn dep_files(root: &Path, spec: &Spec, settings: &Settings) -> Vec<PathBuf> {
    let main = root.join(spec.file);
    let mut out = vec![main.clone()];
    if spec.file == "deps.edn" {
        // substring pre-check avoids parsing deps.edn on the warm path
        let txt = std::fs::read_to_string(&main).unwrap_or_default();
        if !txt.contains("local/root") {
            return out;
        }
        if let Some(e) = edn::read_first(&txt) {
            let mut roots = local_roots(e.get("deps"));
            roots.extend(local_roots(e.get("extra-deps")));
            for a in &settings.source_aliases {
                if let Some(al) = e.get("aliases").and_then(|x| x.get(a)) {
                    roots.extend(local_roots(al.get("deps")));
                    roots.extend(local_roots(al.get("extra-deps")));
                }
            }
            let dir = main.parent().unwrap_or(root);
            for r in roots {
                let p = if Path::new(&r).is_absolute() { PathBuf::from(&r) } else { normalize(&dir.join(&r)) };
                let f = p.join("deps.edn");
                if f.is_file() && !out.contains(&f) {
                    out.push(f);
                }
            }
        }
    }
    out
}

fn canon(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| normalize(p))
}

/// clojure-lsp `process-source-paths`: settings > classpath dirs under root > `src`,`test`; then ignore-regex + canonicalize.
/// `classpath_dirs`: non-jar classpath entries (as printed, possibly relative to root).
pub fn source_paths(root: &Path, settings: &Settings, classpath_dirs: &[String]) -> Vec<String> {
    let root_c = canon(root);
    let chosen: Vec<String> = if let Some(g) = &settings.source_paths {
        g.clone()
    } else {
        let v: Vec<String> = classpath_dirs
            .iter()
            .filter(|p| !p.contains(".jar"))
            .map(|p| {
                let pp = Path::new(p);
                canon(&if pp.is_absolute() { pp.to_path_buf() } else { root_c.join(pp) })
            })
            .filter(|p| p.starts_with(&root_c))
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        if v.is_empty() {
            vec!["src".into(), "test".into()]
        } else {
            v
        }
    };
    let res: Vec<regex::Regex> = settings.ignore_regex.iter().filter_map(|r| regex::Regex::new(&format!("^(?:{r})$")).ok()).collect();
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for sp in chosen {
        let spp = Path::new(&sp);
        let abs = if spp.is_absolute() { spp.to_path_buf() } else { root.join(spp) };
        let rel = abs.strip_prefix(root).or_else(|_| abs.strip_prefix(&root_c)).map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| sp.clone());
        if res.iter().any(|r| r.is_match(&rel)) {
            continue;
        }
        let c = canon(&abs).to_string_lossy().into_owned();
        if seen.insert(c.clone()) {
            out.push(c);
        }
    }
    out
}

/// Subprocess-free estimate of the classpath *directories* from project files
/// (deps.edn `:paths` + source-alias `:extra-paths`/`:paths`, bb.edn `:paths`, lein `:source-paths`..., shadow `:source-paths`).
/// Used before the real classpath is known; the real one from `classpath::resolve` is authoritative.
pub fn static_dirs(root: &Path, specs: &[Spec], settings: &Settings) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut add = |v: Vec<String>| {
        for x in v {
            if !out.contains(&x) {
                out.push(x)
            }
        }
    };
    for sp in specs {
        let Some(e) = edn::read_file(&root.join(sp.file)) else { continue };
        match sp.kind {
            Kind::Deps | Kind::Squint | Kind::Bb => {
                add(e.get("paths").map(|p| p.strs()).unwrap_or_else(|| if sp.kind == Kind::Deps { vec!["src".into()] } else { vec![] }));
                for a in &settings.source_aliases {
                    if let Some(al) = e.get("aliases").and_then(|x| x.get(a)) {
                        if let Some(p) = al.get("extra-paths").or_else(|| al.get("paths")) {
                            add(p.strs());
                        }
                    }
                }
            }
            Kind::Shadow => add(e.get("source-paths").map(|p| p.strs()).unwrap_or_default()),
            Kind::Lein => {
                // (defproject name version :k v ...)
                let items = e.items();
                let kv: Vec<(&Edn, &Edn)> = items.iter().skip(3).collect::<Vec<_>>().chunks(2).filter(|c| c.len() == 2).map(|c| (c[0], c[1])).collect();
                let find = |k: &str| kv.iter().find(|(a, _)| matches!(a, Edn::Kw(s) if s == k)).map(|(_, v)| v.strs());
                add(find("source-paths").unwrap_or_else(|| vec!["src".into()]));
                add(find("test-paths").unwrap_or_else(|| vec!["test".into()]));
                add(find("resource-paths").unwrap_or_else(|| vec!["resources".into()]));
            }
            Kind::Boot => {}
        }
    }
    out
}
