//! Built-in var info (kondo cache/built_in + var_info_gen, see tools/gen_builtin.py) and the
//! cross-file `DefsIndex` (ns -> var info) used to fill usage target info.
use super::types::*;
use crate::intern::{intern, SymId};
use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasherDefault, Hasher};
use std::sync::OnceLock;

#[derive(Default, Clone, Copy)]
pub struct IdHasher(u64);
impl Hasher for IdHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, b: &[u8]) {
        for &x in b {
            self.0 = (self.0 ^ x as u64).wrapping_mul(0x100000001b3);
        }
    }
    fn write_u32(&mut self, i: u32) {
        self.0 = (self.0.rotate_left(5) ^ i as u64).wrapping_mul(0x9E3779B97F4A7C15);
    }
    fn write_u8(&mut self, i: u8) {
        self.0 = (self.0.rotate_left(5) ^ i as u64).wrapping_mul(0x9E3779B97F4A7C15);
    }
}
pub type FastMap<K, V> = HashMap<K, V, BuildHasherDefault<IdHasher>>;
pub type FastSet<K> = HashSet<K, BuildHasherDefault<IdHasher>>;
pub fn fast_map<K, V>() -> FastMap<K, V> {
    HashMap::default()
}

/// Source of a definition table: kondo cache dir + language branch.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u8)]
pub enum Src {
    Clj = 0,
    Cljs = 1,
    CljcClj = 2,
    CljcCljs = 3,
}

pub const F_MACRO: u8 = 1;
pub const F_PRIVATE: u8 = 2;
pub const F_CLASS: u8 = 4;
/// `fixed-arities` key present (possibly empty).
pub const F_FIXED: u8 = 8;
/// Only declared (`declare`), a later real definition replaces it.
pub const F_DECLARED: u8 = 16;

#[derive(Clone, Copy, Debug)]
pub struct VarInfo {
    pub flags: u8,
    pub varargs_min: u16,
    pub fixed: Arities,
    pub deprecated: Val,
}

pub type NsTable = FastMap<SymId, VarInfo>;

/// `clojure.core` symbol tables, default imports.
pub struct VarInfoTables {
    pub core_clj: FastSet<SymId>,
    pub core_cljs: FastSet<SymId>,
    /// simple class name -> (class, package)
    pub imports: FastMap<SymId, (SymId, SymId)>,
    /// fully qualified class names usable as `ns` part
    pub fq_imports: FastMap<SymId, (SymId, SymId)>,
}

static VARINFO: OnceLock<VarInfoTables> = OnceLock::new();

pub fn varinfo() -> &'static VarInfoTables {
    VARINFO.get_or_init(|| {
        let txt = include_str!("varinfo.txt");
        let mut t = VarInfoTables { core_clj: fast_map_set(), core_cljs: fast_map_set(), imports: fast_map(), fq_imports: fast_map() };
        let mut sec = "";
        for line in txt.lines() {
            if let Some(s) = line.strip_prefix('@') {
                sec = s;
                continue;
            }
            match sec {
                "core-clj" => {
                    t.core_clj.insert(intern(line));
                }
                "core-cljs" => {
                    t.core_cljs.insert(intern(line));
                }
                "imports" => {
                    if let Some((k, fq)) = line.split_once('\t') {
                        let pkg = fq.strip_suffix(k).and_then(|p| p.strip_suffix('.')).unwrap_or("");
                        t.imports.insert(intern(k), (intern(k), intern(pkg)));
                    }
                }
                "fq-imports" => {
                    if let Some(i) = line.rfind('.') {
                        t.fq_imports.insert(intern(line), (intern(&line[i + 1..]), intern(&line[..i])));
                    }
                }
                _ => {}
            }
        }
        t
    })
}
fn fast_map_set() -> FastSet<SymId> {
    HashSet::default()
}

pub fn core_sym(cljs: bool, s: SymId) -> bool {
    let t = varinfo();
    if cljs {
        t.core_cljs.contains(&s)
    } else {
        t.core_clj.contains(&s)
    }
}

fn load_builtin() -> FastMap<(u8, SymId), NsTable> {
    let txt = include_str!("builtin.txt");
    let mut m: FastMap<(u8, SymId), NsTable> = fast_map();
    let mut cur: Option<(u8, SymId)> = None;
    for line in txt.lines() {
        if let Some(h) = line.strip_prefix('@') {
            let (src, ns) = h.split_once(' ').unwrap();
            let s = match src {
                "clj" => Src::Clj,
                "cljs" => Src::Cljs,
                "cljc-clj" => Src::CljcClj,
                _ => Src::CljcCljs,
            };
            let k = (s as u8, intern(ns));
            m.entry(k).or_default();
            cur = Some(k);
            continue;
        }
        let mut it = line.split('\t');
        let (name, flags, fixed, va, dep) = (it.next().unwrap(), it.next().unwrap_or(""), it.next().unwrap_or(""), it.next().unwrap_or(""), it.next().unwrap_or(""));
        let mut f = 0u8;
        for c in flags.chars() {
            f |= match c {
                'm' => F_MACRO,
                'p' => F_PRIVATE,
                'f' => F_FIXED,
                _ => F_CLASS,
            };
        }
        let mut ar = Arities::default();
        for x in fixed.split(',').filter(|s| !s.is_empty()) {
            ar.add(x.parse().unwrap_or(99));
        }
        let info = VarInfo {
            flags: f,
            varargs_min: if va.is_empty() { NO_ARITY } else { va.parse().unwrap_or(NO_ARITY) },
            fixed: ar,
            deprecated: if dep.is_empty() { Val::NONE } else if dep == "true" { Val(intern("true")) } else { Val(intern(&format!("\"{}\"", dep))) },
        };
        m.get_mut(&cur.unwrap()).unwrap().insert(intern(name), info);
    }
    m
}

static BUILTIN: OnceLock<FastMap<(u8, SymId), NsTable>> = OnceLock::new();
fn builtin() -> &'static FastMap<(u8, SymId), NsTable> {
    BUILTIN.get_or_init(load_builtin)
}

/// Immutable (after building) map of namespace -> var info. Layers, highest priority first:
/// project files, dependency jars, built-in cache. A namespace found in a layer hides lower layers.
#[derive(Default)]
pub struct DefsIndex {
    project: FastMap<(u8, SymId), NsTable>,
    jars: std::sync::Arc<FastMap<(u8, SymId), NsTable>>,
    /// Namespaces kondo would have loaded from its cache (used by some file): clj/cljc dirs.
    used_all: FastSet<SymId>,
    /// ... and the cljs dir (used by cljs/cljc files only).
    used_cljs: FastSet<SymId>,
    /// Project var definition positions (row, col, top namespace of its file) for linters.
    def_pos: FastMap<(SymId, SymId), (u32, u32, SymId)>,
    /// Protocol definitions of project files: (ns, protocol) -> methods.
    protos: FastMap<(u8, SymId, SymId), super::lint::proto::ProtoDef>,
    /// potemkin `import-vars`: (ns, var) -> (imported ns, imported var), see `resolve_call`.
    imported: FastMap<(SymId, SymId), (SymId, SymId)>,
    /// Mova: namespaces usable without a require (they hold natives).
    auto_ns: std::sync::Arc<FastMap<SymId, SymId>>,
}

impl DefsIndex {
    pub fn new() -> DefsIndex {
        DefsIndex::default()
    }

    /// Copy of the dependency layer (shared, not duplicated) without the project layers: a second writer view of the same jars.
    pub fn share_external(&self) -> DefsIndex {
        DefsIndex { jars: self.jars.clone(), used_all: self.used_all.clone(), used_cljs: self.used_cljs.clone(), auto_ns: self.auto_ns.clone(), ..DefsIndex::default() }
    }

    pub fn set_auto_ns(&mut self, v: &[(SymId, SymId)]) {
        self.auto_ns = std::sync::Arc::new(v.iter().copied().collect());
    }
    /// Mova: the namespace a prefix written with no require stands for (`mova.fs`, default alias `async`).
    pub fn auto_ns(&self, prefix: SymId) -> Option<SymId> {
        if self.auto_ns.is_empty() {
            return None;
        }
        self.auto_ns.get(&prefix).copied()
    }

    /// Mark a namespace as loaded from the cache (normally derived from `add_file`).
    pub fn mark_used(&mut self, ns: SymId, cljs: bool) {
        self.used_all.insert(ns);
        if cljs {
            self.used_cljs.insert(ns);
        }
    }

    /// Drop the project layer (files re-added with `add_file_at`); jar layer and used-ns sets stay.
    pub fn clear_project(&mut self) {
        self.project.clear();
    }

    /// Add a var to the jar layer.
    pub fn add_jar_var(&mut self, src: Src, ns: SymId, name: SymId, info: VarInfo) {
        std::sync::Arc::make_mut(&mut self.jars).entry((src as u8, ns)).or_default().insert(name, info);
    }
    /// Add a var to the jar layer unless one exists (definitions from secondary files of a namespace).
    pub fn add_jar_var_weak(&mut self, src: Src, ns: SymId, name: SymId, info: VarInfo) {
        std::sync::Arc::make_mut(&mut self.jars).entry((src as u8, ns)).or_default().entry(name).or_insert(info);
    }
    /// Add (or replace) one var of the jar layer with `info` (see `add_jar_var`), honoring declare/primary rules.
    pub fn add_jar_def(&mut self, src: Src, ns: SymId, name: SymId, info: VarInfo, primary: bool, declared: bool) {
        merge_def(std::sync::Arc::make_mut(&mut self.jars).entry((src as u8, ns)).or_default(), name, info, primary, declared);
    }

    /// Register (possibly empty) namespace in the jar layer.
    pub fn add_jar_ns(&mut self, src: Src, ns: SymId) {
        std::sync::Arc::make_mut(&mut self.jars).entry((src as u8, ns)).or_default();
    }

    /// Add the definitions of one analyzed file to the project layer (all defs override earlier ones).
    pub fn add_file(&mut self, fa: &FileAnalysis) {
        self.add_file_at(fa, None)
    }

    /// Mark the namespaces `fa` uses (and clojure.core / cljs.core) as loaded. Returns the namespaces newly marked:
    /// finished files that resolved against the smaller sets must be finished again.
    pub fn mark_file_used(&mut self, fa: &FileAnalysis) -> Vec<SymId> {
        let base = fa.base_lang.unwrap_or(BaseLang::Clj);
        let s = super::syms();
        let mut new = Vec::new();
        if self.used_all.insert(s.clojure_core) {
            new.push(s.clojure_core);
        }
        for &n in &fa.used_ns {
            if self.used_all.insert(n) {
                new.push(n);
            }
            if base != BaseLang::Clj && self.used_cljs.insert(n) {
                new.push(n);
            }
        }
        if base != BaseLang::Clj && self.used_cljs.insert(s.cljs_core) {
            new.push(s.cljs_core);
        }
        new
    }

    /// Like `add_file`; with a path, defs of namespaces whose own file is another one (`in-ns`
    /// continuations such as clojure/gvec.clj) never override defs of the primary file.
    pub fn add_file_at(&mut self, fa: &FileAnalysis, path: Option<&str>) {
        let base = fa.base_lang.unwrap_or(BaseLang::Clj);
        self.mark_file_used(fa);
        for d in &fa.var_definitions {
            if !d.imported.0.is_none() {
                self.imported.insert((d.ns, d.name), d.imported);
            }
            let t = self.project.entry((src_of(base, d.lang) as u8, d.ns)).or_default();
            if merge_def(t, d.name, var_info(d), is_primary(path, d.ns), d.declared) {
                let top = fa.namespace_definitions.first().map_or(SymId::NONE, |n| n.name);
                self.def_pos.insert((d.ns, d.name), (d.pos.row, d.pos.col, top));
            }
        }
        for n in &fa.namespace_definitions {
            self.project.entry((src_of(base, n.lang) as u8, n.name)).or_default();
        }
        for d in &fa.var_definitions {
            if !d.protocol_name.is_none() {
                let interface = d.defined_by_lint_as.1.as_str() == "definterface";
                let def = self.protos.entry((src_of(base, d.lang) as u8 & 1, d.protocol_ns, d.protocol_name)).or_default();
                if !def.methods.contains(&d.name) {
                    def.methods.push(d.name);
                }
                let e = def.arities.entry(d.name).or_default();
                for a in d.fixed.iter() {
                    e.add(if interface { a + 1 } else { a });
                }
            }
        }
    }

    /// Protocol definition (project files).
    pub fn protocol(&self, src: Src, ns: SymId, name: SymId) -> Option<&super::lint::proto::ProtoDef> {
        self.protos.get(&(src as u8 & 1, ns, name))
    }

    /// Position of a project var definition: (row, col, top namespace of the defining file).
    pub fn def_pos(&self, ns: SymId, name: SymId) -> Option<(u32, u32, SymId)> {
        self.def_pos.get(&(ns, name)).copied()
    }

    /// kondo `:linted-namespaces`: the namespace is known (project file or loaded from the cache).
    pub fn ns_known(&self, ns: SymId) -> bool {
        [Src::Clj, Src::Cljs, Src::CljcClj, Src::CljcCljs].iter().any(|&s| self.ns_table(s, ns).is_some())
    }

    /// Table of a namespace and whether it comes from the built-in cache (which already includes overrides).
    fn ns_table(&self, src: Src, ns: SymId) -> Option<(&NsTable, bool)> {
        let k = (src as u8, ns);
        if let Some(t) = self.project.get(&k) {
            return Some((t, false));
        }
        let loaded = if src == Src::Cljs { self.used_cljs.contains(&ns) } else { self.used_all.contains(&ns) };
        if !loaded {
            return None;
        }
        if let Some(t) = self.jars.get(&k) {
            return Some((t, false));
        }
        builtin().get(&k).map(|t| (t, true))
    }

    fn ns_table_raw(&self, src: Src, ns: SymId) -> Option<&NsTable> {
        let k = (src as u8, ns);
        self.project.get(&k).or_else(|| self.jars.get(&k)).or_else(|| builtin().get(&k))
    }

    pub fn get(&self, src: Src, ns: SymId, name: SymId) -> Option<VarInfo> {
        let (t, from_builtin) = self.ns_table(src, ns)?;
        let base = t.get(&name).copied();
        if from_builtin {
            return base;
        }
        let r = overrides(src, ns, name, base).or(base);
        if r.is_none() && !self.auto_ns.is_empty() {
            // Mova: a namespace is spread over layers (project `core/core.mova` + Rust natives), so a miss falls through
            let k = (src as u8, ns);
            return self.jars.get(&k).and_then(|t| t.get(&name).copied()).or_else(|| builtin().get(&k).and_then(|t| t.get(&name).copied()));
        }
        r
    }

    /// Names (non-class) of a namespace in the given source, for `:refer :all`.
    pub fn names(&self, src: Src, ns: SymId) -> Vec<SymId> {
        self.ns_table_raw(src, ns).map(|t| t.iter().filter(|(_, v)| v.flags & F_CLASS == 0).map(|(k, _)| *k).collect()).unwrap_or_default()
    }

    /// kondo `utils/resolve-call*`.
    pub fn resolve_call(&self, base: BaseLang, call_lang: u8, ns: SymId, name: SymId, unknown_ns_unresolved: bool) -> Option<VarInfo> {
        self.resolve_call_to(base, call_lang, ns, name, unknown_ns_unresolved).0
    }

    /// `resolve_call` plus the (namespace, name) of the var an import chain ended at (`None` when not imported).
    pub fn resolve_call_to(&self, base: BaseLang, call_lang: u8, ns: SymId, name: SymId, unknown_ns_unresolved: bool) -> (Option<VarInfo>, Option<(SymId, SymId)>) {
        let called = self.resolve_call1(base, call_lang, ns, name, unknown_ns_unresolved);
        if self.imported.is_empty() || called.is_none() {
            return (called, None);
        }
        // kondo `resolve-call*`: follow `:imported-ns` / `:imported-var`, falling back on the importing var
        let mut seen: Vec<(SymId, SymId)> = Vec::new();
        let mut cur = (ns, name);
        let mut res = (called, None);
        while let Some(&imp) = self.imported.get(&cur) {
            if seen.contains(&imp) {
                break;
            }
            seen.push(imp);
            match self.resolve_call1(base, call_lang, imp.0, imp.1, false) {
                Some(v) => {
                    res = (Some(v), Some(imp));
                    cur = imp;
                }
                None => break,
            }
        }
        res
    }

    fn resolve_call1(&self, base: BaseLang, call_lang: u8, ns: SymId, name: SymId, unknown_ns_unresolved: bool) -> Option<VarInfo> {
        use Src::*;
        let g = |s: Src| self.get(s, ns, name);
        let cljs_call = call_lang == L_CLJS;
        match (base, cljs_call) {
            (BaseLang::Clj, _) => g(Clj).or_else(|| g(CljcClj)),
            (BaseLang::Cljs, _) => g(Cljs).or_else(|| if unknown_ns_unresolved { None } else { g(CljcCljs).or_else(|| g(Clj)).or_else(|| g(CljcClj)) }),
            (BaseLang::Cljc, false) => g(Clj).or_else(|| g(CljcClj)),
            (BaseLang::Cljc, true) => g(Cljs).or_else(|| g(CljcCljs)).or_else(|| g(Clj)).or_else(|| g(CljcClj)),
        }
    }
}

/// Source table a definition of a file with base language `base` and element language `lang` belongs to.
pub fn src_of(base: BaseLang, lang: u8) -> Src {
    match (base, lang) {
        (BaseLang::Clj, _) => Src::Clj,
        (BaseLang::Cljs, _) => Src::Cljs,
        (BaseLang::Cljc, L_CLJS) => Src::CljcCljs,
        (BaseLang::Cljc, _) => Src::CljcClj,
    }
}

/// Lookup info of a var definition.
pub fn var_info(d: &VarDef) -> VarInfo {
    let mut f = 0;
    if d.macro_ {
        f |= F_MACRO;
    }
    if d.private {
        f |= F_PRIVATE;
    }
    if d.has_fixed {
        f |= F_FIXED;
    }
    if d.declared {
        f |= F_DECLARED;
    }
    VarInfo { flags: f, varargs_min: d.varargs_min, fixed: d.fixed, deprecated: d.deprecated }
}

/// Is `path` the file named after namespace `ns` (`None` = unknown, counts as primary)?
pub fn is_primary(path: Option<&str>, ns: SymId) -> bool {
    path.map_or(true, |p| {
        let (stem, _) = p.rsplit_once('.').unwrap_or((p, ""));
        stem.ends_with(&ns.as_str().replace('.', "/").replace('-', "_"))
    })
}

/// Insert a definition: a real definition replaces a declaration; a secondary file never replaces a real one.
/// Returns true if the table entry was set.
pub fn merge_def(t: &mut NsTable, name: SymId, info: VarInfo, primary: bool, declared: bool) -> bool {
    match t.get(&name) {
        None => {
            t.insert(name, info);
            true
        }
        Some(prev) => {
            if (prev.flags & F_DECLARED != 0 && !declared) || (primary && !declared) {
                t.insert(name, info);
                true
            } else {
                false
            }
        }
    }
}

fn ar(v: &[u32]) -> Arities {
    let mut a = Arities::default();
    for &x in v {
        a.add(x);
    }
    a
}

/// kondo `overrides.clj` for clojure.core / cljs.core special forms.
fn overrides(src: Src, ns: SymId, name: SymId, base: Option<VarInfo>) -> Option<VarInfo> {
    let n = ns.as_str();
    let core = match (src, n) {
        (Src::Clj, "clojure.core") => 1,
        (Src::CljcClj, "cljs.core") => 2,
        (Src::CljcCljs, "cljs.core") => 3,
        (Src::Clj, "cljs.core") => 4,
        _ => return None,
    };
    let mk = |fixed: &[u32], va: Option<u16>| VarInfo { flags: F_MACRO | if fixed.is_empty() { 0 } else { F_FIXED }, varargs_min: va.unwrap_or(NO_ARITY), fixed: ar(fixed), deprecated: Val::NONE };
    match (core, name.as_str()) {
        (4, "throw") => Some(mk(&[1], None)),
        (4, _) => None,
        (_, "def") => Some(mk(&[1, 2, 3], None)),
        (_, "defn") | (_, "defn-") | (_, "defmacro") => Some(mk(&[], Some(2))),
        (_, "quote") | (_, "var") => Some(mk(&[1], None)),
        (3, "set!") => Some(mk(&[2, 3], None)),
        (_, "set!") => Some(mk(&[2], None)),
        (_, "throw") if core == 1 => Some(mk(&[1], None)),
        (_, "if-some") | (_, "if-let") => {
            let mut v = base.unwrap_or(VarInfo { flags: 0, varargs_min: NO_ARITY, fixed: Arities::default(), deprecated: Val::NONE });
            v.varargs_min = NO_ARITY;
            v.fixed = ar(&[2, 3]);
            v.flags |= F_FIXED;
            Some(v)
        }
        _ => None,
    }
}
