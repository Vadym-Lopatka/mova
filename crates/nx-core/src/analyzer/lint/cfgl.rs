//! `:linters` config: levels and the per-linter options the ported linters consult.
use crate::analyzer::defs::{fast_map, FastMap};
use crate::analyzer::Config;
use crate::cst::{Cst, Kind, NodeId};
use crate::intern::{intern, SymId};

/// One `:exclude` entry.
#[derive(Clone)]
pub enum Excl {
    Sym(SymId),
    Re(regex::Regex),
    /// `(fq/call [sym ...])` or `(fq/call)`: exclude symbols inside calls of `fq/call`.
    Call(SymId, SymId, Option<Vec<SymId>>),
}

#[derive(Clone, Default)]
pub struct LinterCfg {
    pub exclude: Vec<Excl>,
    /// `:exclude {ns [names]}` (unused-referred-var)
    pub exclude_map: FastMap<SymId, Vec<SymId>>,
    pub skip_args: Vec<SymId>,
    pub bools: Vec<(String, bool)>,
    /// `:exclude` symbols split for fast lookup (unresolved-var): plain namespaces and `ns/var`.
    pub excl_ns: crate::analyzer::defs::FastSet<SymId>,
    pub excl_vars: crate::analyzer::defs::FastSet<(SymId, SymId)>,
    /// keyword-valued options other than `:level` (`:sort :case-sensitive`)
    pub kws: Vec<(String, String)>,
    /// `:discouraged-var {fq/sym {..}}` entries by (ns, name).
    pub disc: FastMap<(SymId, SymId), DiscCfg>,
}

/// One `:discouraged-var` entry.
#[derive(Clone, Default)]
pub struct DiscCfg {
    /// 0 = linter level.
    pub level: u8,
    pub off: bool,
    pub message: Option<String>,
    /// `:arities`; -1 = `:varargs`.
    pub arities: Option<Vec<i32>>,
    /// `:positions` bit mask (1 call, 2 value); 3 default.
    pub positions: u8,
    /// `:langs` (`clj`/`cljs`).
    pub langs: Option<Vec<String>>,
}

impl Config {
    pub fn linter_cfg(&self, ty: super::FType) -> Option<&LinterCfg> {
        self.lcfg.get(&(ty as u8))
    }
    pub fn unused_ns_excluded(&self, ns: SymId) -> bool {
        self.linter_cfg(super::FType::UnusedNamespace).map_or(false, |c| c.exclude.iter().any(|e| excl_matches(e, ns.as_str())))
    }
    pub fn unused_referred_excluded(&self, ns: SymId, name: SymId) -> bool {
        self.linter_cfg(super::FType::UnusedReferredVar).map_or(false, |c| c.exclude_map.get(&ns).map_or(false, |v| v.contains(&name)))
    }
    /// Whether `sym` (namespace or var name string) is excluded by a plain `:exclude` of the linter.
    pub fn excluded(&self, ty: super::FType, s: &str) -> bool {
        self.linter_cfg(ty).map_or(false, |c| c.exclude.iter().any(|e| excl_matches(e, s)))
    }

    /// Merge the `:linters` map of a config.edn.
    pub(crate) fn merge_linters(&mut self, c: &Cst, m: NodeId) {
        let kids: Vec<NodeId> = c.sig_children(m).collect();
        let mut i = 0;
        while i + 1 < kids.len() {
            let (k, v) = (kids[i], kids[i + 1]);
            i += 2;
            if c.kind(k) != Kind::Keyword || c.kind(v) != Kind::Map {
                continue;
            }
            let Some(ty) = super::FType::from_name(c.name(k).as_str()) else { continue };
            let e: Vec<NodeId> = c.sig_children(v).collect();
            let mut j = 0;
            while j + 1 < e.len() {
                let (a, b) = (e[j], e[j + 1]);
                j += 2;
                if ty == super::FType::DiscouragedVar && c.kind(a) == Kind::Symbol && c.kind(b) == Kind::Map {
                    let full = crate::analyzer::node_str(c, a);
                    if let Some((ns, nm)) = full.split_once('/') {
                        let d = parse_disc(c, b);
                        self.lcfg.entry(ty as u8).or_default().disc.insert((intern(ns), intern(nm)), d);
                    }
                    continue;
                }
                if c.kind(a) != Kind::Keyword {
                    continue;
                }
                match c.name(a).as_str() {
                    "level" if c.kind(b) == Kind::Keyword => {
                        let l = match c.name(b).as_str() {
                            "off" => super::OFF,
                            "info" => super::INFO,
                            "warning" => super::WARNING,
                            "error" => super::ERROR,
                            _ => continue,
                        };
                        if (ty as usize) < self.levels.len() {
                            self.levels[ty as usize] = l;
                        }
                    }
                    "exclude" => {
                        let cfg = self.lcfg.entry(ty as u8).or_default();
                        match c.kind(b) {
                            Kind::Vector | Kind::Set | Kind::List => {
                                for x in c.sig_children(b) {
                                    if let Some(ex) = parse_excl(c, x) {
                                        if let Excl::Sym(s) = &ex {
                                            match s.as_str().split_once('/') {
                                                Some((ns, nm)) => {
                                                    cfg.excl_vars.insert((intern(ns), intern(nm)));
                                                }
                                                None => {
                                                    cfg.excl_ns.insert(*s);
                                                }
                                            }
                                        }
                                        cfg.exclude.push(ex);
                                    }
                                }
                            }
                            Kind::Map => {
                                let mm: Vec<NodeId> = c.sig_children(b).collect();
                                let mut q = 0;
                                while q + 1 < mm.len() {
                                    if c.kind(mm[q]) == Kind::Symbol {
                                        let ns = intern(&crate::analyzer::node_str(c, mm[q]));
                                        let names: Vec<SymId> = c.sig_children(mm[q + 1]).filter(|&n| c.kind(n) == Kind::Symbol).map(|n| c.name(n)).collect();
                                        cfg.exclude_map.entry(ns).or_default().extend(names);
                                    }
                                    q += 2;
                                }
                            }
                            _ => {}
                        }
                    }
                    "skip-args" if c.kind(b) == Kind::Vector => {
                        let cfg = self.lcfg.entry(ty as u8).or_default();
                        for x in c.sig_children(b) {
                            if c.kind(x) == Kind::Symbol {
                                cfg.skip_args.push(intern(&crate::analyzer::node_str(c, x)));
                            }
                        }
                    }
                    key => {
                        if c.kind(b) == Kind::Keyword {
                            let cfg = self.lcfg.entry(ty as u8).or_default();
                            cfg.kws.retain(|(n, _)| n != key);
                            cfg.kws.push((key.to_owned(), c.name(b).as_str().to_owned()));
                        }
                        if matches!(c.kind(b), Kind::True | Kind::False) {
                            let cfg = self.lcfg.entry(ty as u8).or_default();
                            cfg.bools.retain(|(n, _)| n != key);
                            cfg.bools.push((key.to_owned(), c.kind(b) == Kind::True));
                        }
                    }
                }
            }
        }
    }

    /// `:unsorted-required-namespaces {:sort :case-sensitive}`.
    pub fn lint_sort_case_sensitive(&self, ty: super::FType) -> bool {
        self.linter_cfg(ty).map_or(false, |c| c.kws.iter().any(|(k, v)| k == "sort" && v == "case-sensitive"))
    }

    /// Boolean option of a linter (`:exclude-destructured-as` ...).
    pub fn lint_bool(&self, ty: super::FType, key: &str) -> bool {
        self.linter_cfg(ty).map_or(false, |c| c.bools.iter().any(|(n, v)| n == key && *v))
    }
}

fn parse_disc(c: &Cst, m: NodeId) -> DiscCfg {
    let mut d = DiscCfg { positions: 3, ..Default::default() };
    let kids: Vec<NodeId> = c.sig_children(m).collect();
    let mut i = 0;
    while i + 1 < kids.len() {
        let (k, v) = (kids[i], kids[i + 1]);
        i += 2;
        if c.kind(k) != Kind::Keyword {
            continue;
        }
        match c.name(k).as_str() {
            "level" if c.kind(v) == Kind::Keyword => match c.name(v).as_str() {
                "off" => d.off = true,
                "info" => d.level = super::INFO,
                "warning" => d.level = super::WARNING,
                "error" => d.level = super::ERROR,
                _ => {}
            },
            "message" if c.kind(v) == Kind::String => d.message = Some(c.string_content(v).to_owned()),
            "arities" => {
                let mut l = Vec::new();
                for x in c.sig_children(v) {
                    match c.kind(x) {
                        Kind::Number => {
                            if let Ok(n) = c.text(x).parse::<i32>() {
                                l.push(n);
                            }
                        }
                        Kind::Keyword if c.name(x).as_str() == "varargs" => l.push(-1),
                        _ => {}
                    }
                }
                d.arities = Some(l);
            }
            "positions" => {
                let mut m = 0;
                for x in c.sig_children(v) {
                    match (c.kind(x) == Kind::Keyword).then(|| c.name(x)) {
                        Some(n) if n.as_str() == "call" => m |= 1,
                        Some(n) if n.as_str() == "value" => m |= 2,
                        _ => {}
                    }
                }
                d.positions = m;
            }
            "langs" => d.langs = Some(c.sig_children(v).filter(|&x| c.kind(x) == Kind::Keyword).map(|x| c.name(x).as_str().to_owned()).collect()),
            _ => {}
        }
    }
    d
}

fn parse_excl(c: &Cst, x: NodeId) -> Option<Excl> {
    match c.kind(x) {
        Kind::Symbol => Some(Excl::Sym(intern(&crate::analyzer::node_str(c, x)))),
        Kind::String => regex::Regex::new(c.string_content(x)).ok().map(Excl::Re),
        Kind::List => {
            let ch: Vec<NodeId> = c.sig_children(x).collect();
            let h = *ch.first()?;
            if c.kind(h) != Kind::Symbol {
                return None;
            }
            let (ns, nm) = (c.ns(h), c.name(h));
            let mut syms = None;
            if let Some(&v) = ch.get(1) {
                let mut l = Vec::new();
                for s in c.sig_children(v) {
                    if c.kind(s) == Kind::Symbol {
                        l.push(c.name(s));
                    }
                }
                syms = Some(l);
            }
            Some(Excl::Call(ns, nm, syms))
        }
        _ => None,
    }
}

pub fn excl_matches(e: &Excl, s: &str) -> bool {
    match e {
        Excl::Sym(x) => x.as_str() == s,
        Excl::Re(r) => r.is_match(s),
        Excl::Call(..) => false,
    }
}

#[allow(dead_code)]
fn _f() {
    let _: FastMap<u8, u8> = fast_map();
}

/// kondo `default-config` entries beyond levels that the ported linters consult.
pub const DEFAULT_CONFIG: &str = "{:linters {:unresolved-symbol {:exclude [(leiningen.core.project/defproject) (clojure.test/are [thrown? thrown-with-msg?]) (cljs.test/are [thrown? thrown-with-msg?]) (clojure.test/is [thrown? thrown-with-msg?]) (cljs.test/is [thrown? thrown-with-msg?])]}}}";
