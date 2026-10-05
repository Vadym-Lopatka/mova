//! Namespace-level linters: unused-namespace, unused-referred-var, unused-import, refer-all / use,
//! duplicate-require, aliased-referred-var. kondo `linters/lint-unused-namespaces!`, `lint-unused-imports!`,
//! `namespace/lint-duplicate-requires!`.
use super::*;
use crate::analyzer::defs::{fast_map, FastSet};
use crate::analyzer::expr::json_str;
use crate::analyzer::extras::NO_EXPR;
use crate::intern::SymId;

pub struct Req {
    pub ns: SymId,
    pub pos: Pos,
    pub as_alias: bool,
}
pub struct Ref {
    pub local: SymId,
    pub ns: SymId,
    pub name: SymId,
    pub pos: Pos,
    pub self_macro: bool,
}
pub struct RAll {
    pub ns: SymId,
    pub pos: Pos,
    pub is_use: bool,
    /// keyword form (`:use`) vs symbol (`use`)
    pub kw: bool,
    pub used: Vec<SymId>,
}
pub struct Imp {
    pub class: SymId,
    pub pkg: SymId,
    pub pos: Pos,
    pub mark_used: bool,
}

/// Lint view of one namespace (kondo namespace map keys `:required`, `:used-namespaces`, ...).
pub struct LNs {
    pub required: Vec<Req>,
    pub used: FastSet<SymId>,
    pub referred: Vec<Ref>,
    pub used_referred: FastSet<SymId>,
    pub refer_alls: Vec<RAll>,
    pub imports: Vec<Imp>,
    pub used_imports: FastSet<SymId>,
    /// (namespace symbol, var name, position) of unresolved namespaces, first per symbol.
    pub unresolved_ns: Vec<(SymId, SymId, Pos)>,
    /// Namespace-local config (`:clj-kondo/config`).
    pub local: Option<std::rc::Rc<crate::analyzer::Config>>,
    /// `:refer-clojure :exclude` symbols with positions.
    pub excluded: Vec<(SymId, Pos)>,
}

impl LNs {
    pub fn new() -> LNs {
        LNs { required: Vec::new(), used: fast_set(), referred: Vec::new(), used_referred: fast_set(), refer_alls: Vec::new(), imports: Vec::new(), used_imports: fast_set(), unresolved_ns: Vec::new(), local: None, excluded: Vec::new() }
    }
}

fn fast_set() -> FastSet<SymId> {
    FastSet::default()
}
#[allow(dead_code)]
fn _f() {
    let _ = fast_map::<u8, u8>;
}

impl<'a> Analyzer<'a> {
    pub fn lns(&mut self) -> &mut LNs {
        let i = self.cur;
        while self.lt.ns.len() <= i {
            self.lt.ns.push(LNs::new());
        }
        &mut self.lt.ns[i]
    }
    pub fn lns_at(&mut self, i: usize) -> &mut LNs {
        while self.lt.ns.len() <= i {
            self.lt.ns.push(LNs::new());
        }
        &mut self.lt.ns[i]
    }

    /// kondo `namespace/reg-used-namespace!`.
    #[inline]
    pub fn lint_use_ns(&mut self, ns: SymId) {
        if self.lon && !ns.is_none() && self.lt.last_used_ns != Some((self.cur, ns)) {
            self.lt.last_used_ns = Some((self.cur, ns));
            self.lns().used.insert(ns);
        }
    }
    /// kondo `namespace/reg-used-referred-var!` (local name of the referred var).
    #[inline]
    pub fn lint_use_referred(&mut self, local: SymId) {
        if self.lon {
            self.lns().used_referred.insert(local);
        }
    }
    /// kondo `namespace/reg-used-import!`.
    #[inline]
    pub fn lint_use_import(&mut self, class: SymId) {
        if self.lon {
            let l = self.lns();
            if !l.imports.is_empty() {
                l.used_imports.insert(class);
            }
        }
    }
    /// kondo `reg-referred-all-var!`: a simple symbol resolved to `ns` (maybe through `:refer :all`).
    pub fn lint_refer_all_use(&mut self, ns: SymId, name: SymId) {
        if !self.lon {
            return;
        }
        let l = self.lns();
        if let Some(r) = l.refer_alls.iter_mut().find(|r| r.ns == ns) {
            if !r.used.contains(&name) {
                r.used.push(name);
            }
        }
    }

    /// kondo `lint-duplicate-requires!` over one clause; `seen` holds earlier namespaces.
    pub fn lint_duplicate_requires(&mut self, seen: &mut Vec<SymId>, ns: SymId, pos: Pos) {
        if seen.contains(&ns) {
            self.lint(FType::DuplicateRequire, pos, format!("duplicate require of {}", ns.as_str())).map(|f| f.extra.push(("duplicate-ns", json_str(ns.as_str()))));
        } else {
            seen.push(ns);
        }
    }

    /// End of a language pass: kondo `lint-unused-namespaces!`, `lint-unused-imports!`.
    pub fn lint_ns_end(&mut self) {
        let lns = std::mem::take(&mut self.lt.ns);
        for (i, l) in lns.iter().enumerate() {
            self.lt.ns_cursor = i;
            self.lint_ns_one(l, i);
        }
        self.lt.ns = lns;
    }

    fn lint_ns_one(&mut self, l: &LNs, idx: usize) {
        self.lt.cur_cfg = l.local.clone();
        let cfg_rc = l.local.clone();
        let cfg: &crate::analyzer::Config = cfg_rc.as_deref().unwrap_or(self.cfg);
        // unused-namespace: `(set required)` keeps the first occurrence
        let mut seen: Vec<SymId> = Vec::new();
        for r in &l.required {
            if seen.contains(&r.ns) {
                continue;
            }
            seen.push(r.ns);
            if l.used.contains(&r.ns) || r.as_alias || cfg.unused_ns_excluded(r.ns) {
                continue;
            }
            self.lint(FType::UnusedNamespace, r.pos, format!("namespace {} is required but never used", r.ns.as_str())).map(|f| f.extra.push(("ns", json_str(r.ns.as_str()))));
        }
        for v in &l.referred {
            if l.used_referred.contains(&v.local) || cfg.unused_referred_excluded(v.ns, v.local) || l.refer_alls.iter().any(|r| r.ns == v.ns) || v.self_macro {
                continue;
            }
            let ns = json_str(v.ns.as_str());
            self.lint(FType::UnusedReferredVar, v.pos, format!("#'{}/{} is referred but never used", v.ns.as_str(), v.name.as_str()))
                .map(|f| {
                    f.extra.push(("ns", ns.clone()));
                    f.extra.push(("referred-ns", ns));
                    f.extra.push(("refer", json_str(v.name.as_str())));
                });
        }
        // refer-all / use findings need the usages resolved against definitions: emitted by `finish::refer_alls`
        if self.lc().level(FType::ReferAll) != OFF || self.lc().level(FType::Use) != OFF {
            let from = self.nss.get(self.lt.ns_cursor).map_or(SymId::NONE, |s| s.name);
            for r in &l.refer_alls {
                self.out.lint_ralls.push(RAllRec { from, ns: r.ns, pos: r.pos, is_use: r.is_use, kw: r.kw, lang: self.ltag });
            }
        }
        // aliased-referred-var
        if self.nss.get(idx).map_or(false, |s| !s.referred.is_empty()) && self.lc().level(FType::AliasedReferredVar) != OFF {
            let us: Vec<(Pos, SymId, SymId, SymId)> = self.out.var_usages.iter().filter(|u| !u.alias.is_none() && u.lang == self.ltag && !u.derived).map(|u| (u.pos, u.resolved_ns, u.name, u.alias)).collect();
            let cur = self.cur_ns_name_of(l);
            let _ = cur;
            for (pos, rns, name, alias) in us {
                if self.nss.get(idx).map_or(false, |s| s.referred.values().any(|&(vn, nm)| vn == rns && nm == name)) {
                    let p = Pos { row: pos.row, col: pos.col, end_row: 0, end_col: 0 };
                    self.lint(FType::AliasedReferredVar, p, format!("Var {} is referred but used via alias: {}", name.as_str(), alias.as_str()));
                }
            }
        }
        for (ns, name, pos) in &l.unresolved_ns {
            let msg = format!("Unresolved namespace {}. Are you missing a require?", ns.as_str());
            let (nsj, nj) = (json_str(ns.as_str()), json_str(name.as_str()));
            self.lint(FType::UnresolvedNamespace, *pos, msg).map(|f| {
                f.extra.push(("ns", nsj));
                f.extra.push(("name", nj));
            });
        }
        // unused-excluded-var
        if !l.excluded.is_empty() && self.lc().level(FType::UnusedExcludedVar) != OFF {
            let cljs = self.is_cljs();
            for &(sym, pos) in &l.excluded {
                let used = self.nss.get(idx).map_or(false, |s| s.vars.contains(&sym) || s.referred.contains_key(&sym) || s.referred.values().any(|&(_, nm)| nm == sym)) || self.lt.binds.iter().any(|b| b.active && b.name == sym);
                if !used && crate::analyzer::defs::core_sym(cljs, sym) {
                    self.lint(FType::UnusedExcludedVar, pos, format!("Unused excluded var: {}", sym.as_str()));
                }
            }
        }
        // unused-import
        for i in &l.imports {
            if i.mark_used || l.used_imports.contains(&i.class) {
                continue;
            }
            let cls = json_str(&format!("{}.{}", i.pkg.as_str(), i.class.as_str()));
            self.lint(FType::UnusedImport, i.pos, format!("Unused import {}", i.class.as_str())).map(|f| f.extra.push(("class", cls)));
        }
    }

    fn cur_ns_name_of(&self, _l: &LNs) -> SymId {
        self.cur_ns_name()
    }
}

impl<'a> Analyzer<'a> {
    pub fn lint_use_ns_at(&mut self, idx: usize, ns: SymId) {
        if self.lon && !ns.is_none() {
            self.lns_at(idx).used.insert(ns);
        }
    }
    /// Register an import of the namespace at `idx` (kondo `reg-imports!`).
    pub fn lint_add_import(&mut self, idx: usize, class: SymId, pkg: SymId, node: NodeId, mark_used: bool) {
        if !self.lon {
            return;
        }
        let pos = self.pos(node);
        let l = self.lns_at(idx);
        if !l.imports.iter().any(|i| i.class == class) {
            l.imports.push(Imp { class, pkg, pos, mark_used });
        }
    }
}

impl<'a> Analyzer<'a> {
    /// kondo `usages/analyze-keyword`: `::alias/kw` marks the alias' namespace used or is an unresolved namespace.
    pub fn lint_keyword_ns(&mut self, n: NodeId) {
        use crate::cst::{Kind, F_AUTO};
        if !self.lon || self.kind(n) != Kind::Keyword || self.c.flags(n) & F_AUTO == 0 {
            return;
        }
        let alias = self.c.ns(n);
        if alias.is_none() {
            return;
        }
        let name = self.c.name(n);
        let r = self.resolve_name(false, (alias, name), NO_EXPR);
        if r.found && !r.ns.is_none() {
            self.lint_use_ns(r.ns);
        } else if !r.unresolved_ns.is_none() {
            let p = self.pos(n);
            self.lint_unresolved_ns(r.unresolved_ns, name, p);
        }
    }

    /// kondo `namespace/reg-unresolved-namespace!`.
    pub fn lint_unresolved_ns(&mut self, ns: SymId, name: SymId, pos: Pos) {
        if !self.lon || self.ctx.off & uses::OFF_NS != 0 || self.lc().level(FType::UnresolvedNamespace) == OFF || self.lc().excluded(FType::UnresolvedNamespace, ns.as_str()) {
            return;
        }
        // unresolved namespaces inside an excluded unresolved-symbol call (identity check-fn) are not reported
        if let Some(c) = self.lc().linter_cfg(FType::UnresolvedSymbol) {
            if c.exclude.iter().any(|e| matches!(e, cfgl::Excl::Call(ens, enm, None) if self.cs.iter().any(|&(a, b)| a == *ens && b == *enm))) {
                return;
            }
        }
        // clojure-lsp sets `:report-duplicates true`: every occurrence is reported
        self.lns().unresolved_ns.push((ns, name, pos));
    }
}

/// A `:refer :all` / `:use` of a namespace, reported by `finish::refer_alls` once usages are resolved.
#[derive(Clone, Debug)]
pub struct RAllRec {
    pub from: SymId,
    pub ns: SymId,
    pub pos: Pos,
    pub is_use: bool,
    pub kw: bool,
    pub lang: u8,
}

impl<'a> Analyzer<'a> {
    /// `analyze-ns-decl`: remember `:refer-clojure :exclude` symbols; `unresolved-excluded-var`.
    pub fn lint_excluded_vars(&mut self, idx: usize, nodes: &[(SymId, NodeId)]) {
        if !self.lon || nodes.is_empty() {
            return;
        }
        let cljc = self.base == crate::analyzer::BaseLang::Cljc;
        for &(sym, node) in nodes {
            let p = self.pos(node);
            self.lns_at(idx).excluded.push((sym, p));
            let exists = if cljc { crate::analyzer::defs::core_sym(false, sym) || crate::analyzer::defs::core_sym(true, sym) } else { crate::analyzer::defs::core_sym(self.is_cljs(), sym) };
            if !exists {
                self.lint(FType::UnresolvedExcludedVar, p, format!("Unresolved excluded var: {}", sym.as_str()));
            }
        }
    }
}
