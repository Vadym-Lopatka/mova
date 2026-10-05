//! Minimal clj-kondo config: `:lint-as` from `.clj-kondo/config.edn`, its `:config-paths` and
//! `.clj-kondo/imports/*/*/config.edn`. Hooks and linter settings are not modeled.
use super::defs::{fast_map, FastMap};
use super::Name;
use crate::cst::Kind;
use crate::intern::{intern, SymId};
use std::path::Path;

#[derive(Default, Clone)]
pub struct Config {
    /// fq symbol -> lint-as target
    pub lint_as: FastMap<Name, Name>,
    /// `:hooks {:analyze-call {var hook-fn}}`
    pub hooks: FastMap<Name, Name>,
    /// Linter levels by `FType` index (kondo defaults + `:linters` overrides).
    pub levels: Vec<u8>,
    /// Per-linter options (`:exclude`, ...), by `FType` index.
    pub lcfg: FastMap<u8, super::lint::cfgl::LinterCfg>,
    /// `:skip-comments true`: forms inside `(comment ..)` are not analyzed.
    pub skip_comments: bool,
    /// `:config-in-ns {ns config-map}`: config map sources (EDN text), merged when the namespace starts.
    pub in_ns: FastMap<SymId, Vec<String>>,
}

fn split_fq(s: &str) -> Option<Name> {
    let (ns, n) = s.split_once('/')?;
    Some((intern(ns), intern(n)))
}

impl Config {
    /// Config with kondo's default `:lint-as` table.
    pub fn new() -> Config {
        let mut c = Config { lint_as: fast_map(), hooks: fast_map(), levels: super::lint::FTYPES.iter().map(|t| t.2).collect(), lcfg: fast_map(), skip_comments: false, in_ns: fast_map() };
        for (a, b) in [
            ("cats.core/->=", "clojure.core/->"),
            ("cats.core/->>=", "clojure.core/->>"),
            ("rewrite-clj.custom-zipper.core/defn-switchable", "clojure.core/defn"),
            ("clojure.core.async/go-loop", "clojure.core/loop"),
            ("clojure.test.check.generators/let", "clojure.core/let"),
            ("cljs.core.async/go-loop", "clojure.core/loop"),
            ("cljs.core.async.macros/go-loop", "clojure.core/loop"),
            ("schema.core/defschema", "clojure.core/def"),
            ("compojure.core/defroutes", "clojure.core/def"),
            ("compojure.core/let-routes", "clojure.core/let"),
        ] {
            c.lint_as.insert(split_fq(a).unwrap(), split_fq(b).unwrap());
        }
        c.merge_edn(super::lint::cfgl::DEFAULT_CONFIG);
        c
    }

    pub fn hook(&self, ns: SymId, name: SymId) -> Option<Name> {
        if self.hooks.is_empty() {
            return None;
        }
        self.hooks.get(&(ns, name)).copied()
    }

    pub fn lint_as(&self, ns: SymId, name: SymId) -> Option<Name> {
        if self.lint_as.is_empty() {
            return None;
        }
        self.lint_as.get(&(ns, name)).copied()
    }

    /// Merge `:lint-as` entries of one config.edn text; returns `:config-paths` strings.
    pub fn merge_edn(&mut self, text: &str) -> Vec<String> {
        let c = crate::reader::parse(text);
        let root = c.root();
        let Some(m) = c.children(root).iter().copied().find(|&n| c.kind(n) == Kind::Map) else { return Vec::new() };
        self.merge_map(&c, m)
    }

    /// Merge the entries of a config map node (`:lint-as`, `:hooks`, `:linters`, ...); returns `:config-paths`.
    pub fn merge_map(&mut self, c: &crate::cst::Cst, m: crate::cst::NodeId) -> Vec<String> {
        let mut paths = Vec::new();
        let kids: Vec<_> = c.sig_children(m).collect();
        let mut i = 0;
        while i + 1 < kids.len() {
            let (k, v) = (kids[i], kids[i + 1]);
            i += 2;
            if c.kind(k) != Kind::Keyword {
                continue;
            }
            match c.name(k).as_str() {
                "lint-as" if c.kind(v) == Kind::Map => {
                    let e: Vec<_> = c.sig_children(v).collect();
                    let mut j = 0;
                    while j + 1 < e.len() {
                        let (a, b) = (e[j], e[j + 1]);
                        j += 2;
                        if c.kind(a) == Kind::Symbol && c.kind(b) == Kind::Symbol && !c.ns(a).is_none() && !c.ns(b).is_none() {
                            self.lint_as.insert((c.ns(a), c.name(a)), (c.ns(b), c.name(b)));
                        }
                    }
                }
                "hooks" if c.kind(v) == Kind::Map => {
                    let e: Vec<_> = c.sig_children(v).collect();
                    let mut j = 0;
                    while j + 1 < e.len() {
                        let (a, b) = (e[j], e[j + 1]);
                        j += 2;
                        if c.kind(a) == Kind::Keyword && c.name(a).as_str() == "analyze-call" && c.kind(b) == Kind::Map {
                            let h: Vec<_> = c.sig_children(b).collect();
                            let mut q = 0;
                            while q + 1 < h.len() {
                                let (x, y) = (h[q], h[q + 1]);
                                q += 2;
                                if c.kind(x) == Kind::Symbol && c.kind(y) == Kind::Symbol && !c.ns(x).is_none() && !c.ns(y).is_none() {
                                    self.hooks.insert((c.ns(x), c.name(x)), (c.ns(y), c.name(y)));
                                }
                            }
                        }
                    }
                }
                "linters" if c.kind(v) == Kind::Map => self.merge_linters(c, v),
                "config-in-ns" if c.kind(v) == Kind::Map => {
                    let e: Vec<_> = c.sig_children(v).collect();
                    let mut j = 0;
                    while j + 1 < e.len() {
                        let (a, b) = (e[j], e[j + 1]);
                        j += 2;
                        if c.kind(a) == Kind::Symbol && c.kind(b) == Kind::Map {
                            let ns = intern(&crate::analyzer::node_str(c, a));
                            self.in_ns.entry(ns).or_default().push(crate::analyzer::node_str(c, b));
                        }
                    }
                }
                "skip-comments" if matches!(c.kind(v), Kind::True | Kind::False) => self.skip_comments = c.kind(v) == Kind::True,
                "config-paths" if c.kind(v) == Kind::Vector => {
                    for x in c.sig_children(v) {
                        if c.kind(x) == Kind::String {
                            paths.push(c.string_content(x).to_owned());
                        }
                    }
                }
                _ => {}
            }
        }
        paths
    }

    /// Load from a project root, mirroring clj-kondo `resolve-config`: defaults, home config, the
    /// `.clj-kondo` dir (its `:config-paths`, then auto-discovered `*/*/config.edn` and
    /// `imports/*/*/config.edn` dirs, then its own config last), `CLJ_KONDO_EXTRA_CONFIG_DIR`.
    pub fn load(project_root: &Path) -> Config {
        let mut cfg = Config::new();
        let home = match std::env::var_os("XDG_CONFIG_HOME") {
            Some(x) => Some(Path::new(&x).join("clj-kondo")),
            None => std::env::var_os("HOME").map(|h| Path::new(&h).join(".config").join("clj-kondo")),
        };
        if let Some(h) = home.filter(|h| h.exists()) {
            cfg.process_dir(&h, false, 0);
        }
        let dir = project_root.join(".clj-kondo");
        if dir.is_dir() {
            cfg.process_dir(&dir, true, 0);
        }
        if let Some(x) = std::env::var_os("CLJ_KONDO_EXTRA_CONFIG_DIR") {
            cfg.process_dir(Path::new(&x), false, 0);
        }
        cfg
    }

    /// kondo `process-cfg-dir`: apply `:config-paths` (relative to `dir`) left to right, then the dir's own config.
    /// `auto` = project config dir: also auto-discover config dirs (unless `:auto-load-configs false`).
    fn process_dir(&mut self, dir: &Path, auto: bool, depth: u32) {
        let text = std::fs::read_to_string(dir.join("config.edn")).ok();
        let mut paths = text.as_deref().map(config_paths).unwrap_or_default();
        if auto && !text.as_deref().is_some_and(auto_load_off) {
            let set: std::collections::HashSet<String> = paths.iter().cloned().collect();
            let mut found = Vec::new();
            auto_configs(dir, dir, 1, &mut found);
            found.retain(|p| !set.contains(p));
            found.sort();
            found.dedup();
            paths.extend(found);
        }
        if depth < 16 {
            for p in paths {
                let f = Path::new(&p);
                let d = if f.is_absolute() { f.to_path_buf() } else { dir.join(f) };
                if d.exists() {
                    self.process_dir(&d, false, depth + 1);
                }
            }
        }
        if let Some(t) = text {
            self.merge_edn(&t);
        }
    }
}

/// `:config-paths` strings of a config.edn text.
fn config_paths(text: &str) -> Vec<String> {
    let c = crate::reader::parse(text);
    let Some(m) = c.children(c.root()).iter().copied().find(|&n| c.kind(n) == Kind::Map) else { return Vec::new() };
    let kids: Vec<_> = c.sig_children(m).collect();
    let mut out = Vec::new();
    for kv in kids.chunks(2) {
        if let [k, v] = kv {
            if c.kind(*k) == Kind::Keyword && c.name(*k).as_str() == "config-paths" && matches!(c.kind(*v), Kind::Vector | Kind::List | Kind::Set) {
                out.extend(c.sig_children(*v).filter(|&x| c.kind(x) == Kind::String).map(|x| c.string_content(x).to_owned()));
            }
        }
    }
    out
}

/// `:auto-load-configs false`.
fn auto_load_off(text: &str) -> bool {
    let c = crate::reader::parse(text);
    let Some(m) = c.children(c.root()).iter().copied().find(|&n| c.kind(n) == Kind::Map) else { return false };
    let kids: Vec<_> = c.sig_children(m).collect();
    kids.chunks(2).any(|kv| matches!(kv, [k, v] if c.kind(*k) == Kind::Keyword && c.name(*k).as_str() == "auto-load-configs" && c.kind(*v) == Kind::False))
}

/// kondo `auto-configs`: dirs holding a `config.edn` at exactly 3 path components below `base`
/// (glob `**/**/config.edn`, depth 3) or 4 components under `imports/` (glob `imports/**/**/config.edn`, depth 4).
/// Symlinks are followed. Results are paths relative to `base`.
fn auto_configs(base: &Path, dir: &Path, depth: usize, out: &mut Vec<String>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        let Ok(md) = std::fs::metadata(&p) else { continue };
        if md.is_dir() {
            if depth < 4 {
                auto_configs(base, &p, depth + 1, out);
            }
        } else if e.file_name() == "config.edn" {
            let rel = p.parent().and_then(|d| d.strip_prefix(base).ok()).map(|r| r.to_string_lossy().into_owned()).unwrap_or_default();
            let comps = depth;
            let imports = rel.starts_with("imports/") || rel == "imports";
            if (comps == 3) || (comps == 4 && imports) {
                out.push(rel);
            }
        }
    }
}
