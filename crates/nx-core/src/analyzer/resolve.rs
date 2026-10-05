//! Name resolution: kondo `namespace/resolve-name` and helpers.
use super::defs::{core_sym, varinfo};
use super::*;

/// Result of `resolve_name` (kondo returns a map; `found == false` is nil).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Resolved {
    pub found: bool,
    pub ns: SymId,
    pub name: SymId,
    pub alias: SymId,
    pub unresolved: bool,
    pub unresolved_ns: SymId,
    pub clojure_excluded: bool,
    pub interop: bool,
    pub resolved_core: bool,
}

impl Resolved {
    pub fn none() -> Resolved {
        Resolved { found: false, ns: SymId::NONE, name: SymId::NONE, alias: SymId::NONE, unresolved: false, unresolved_ns: SymId::NONE, clojure_excluded: false, interop: false, resolved_core: false }
    }
    fn ok(ns: SymId, name: SymId) -> Resolved {
        Resolved { found: true, ns, name, ..Resolved::none() }
    }
}

const SPECIAL: &[&str] = &["&", "monitor-exit", "case*", "try", "reify*", "finally", "loop*", "do", "letfn*", "if", "clojure.core/import*", "new", "deftype*", "let*", "fn*", "recur", "set!", ".", "var", "quote", "catch", "throw", "monitor-enter", "def"];

pub(crate) fn is_special_symbol(s: &str) -> bool {
    SPECIAL.contains(&s)
}

/// kondo `namespace/class-name?`.
pub(crate) fn class_name_p(s: &str) -> bool {
    if let Some(i) = s.rfind('.') {
        if let Some(ch) = s[i + 1..].chars().next() {
            if ch.is_uppercase() {
                return true;
            }
        }
        if let Some(u) = s.find('_') {
            if u > 0 {
                return true;
            }
        }
    }
    false
}

impl<'a> Analyzer<'a> {
    pub fn core_ns(&self) -> SymId {
        if self.is_cljs() {
            syms().cljs_core
        } else {
            syms().clojure_core
        }
    }

    /// kondo `normalize-sym-name` (cljs only): strips `foo.bar.baz` property access.
    pub fn normalize_sym_name_inner(&self, sym: SymId) -> Name {
        if !self.is_cljs() {
            return (SymId::NONE, sym);
        }
        let s = sym.as_str();
        if s.starts_with('.') || s.ends_with('.') || !s.contains('.') {
            return (SymId::NONE, sym);
        }
        let segs: Vec<&str> = s.split('.').collect();
        let prefix = intern(segs[0]);
        if self.bindings.iter().any(|b| b.name == prefix) || self.cur_ns().vars.contains(&prefix) {
            return (SymId::NONE, prefix);
        }
        if self.cur_ns().qualify.contains_key(&sym) {
            return (SymId::NONE, sym);
        }
        let maybe_ns = intern(&segs[..segs.len() - 1].join("."));
        if self.cur_ns().qualify.get(&maybe_ns) == Some(&maybe_ns) {
            return (maybe_ns, intern(segs[segs.len() - 1]));
        }
        if segs[0] != "goog" {
            (SymId::NONE, prefix)
        } else {
            (SymId::NONE, sym)
        }
    }

    fn unresolved_ns_result(&self, expr: NodeId, ns_sym: SymId, name: SymId) -> Resolved {
        let var_name = intern(name.as_str());
        // `NodeId(u32::MAX)` = no expression (kondo passes nil)
        let generated = self.ex.gen || expr.0 != u32::MAX && {
            let name_node = if self.kind(expr) == Kind::List { self.c.nth(expr, 0).unwrap_or(expr) } else { expr };
            self.c.is_gen(name_node)
        };
        if generated {
            Resolved::ok(ns_sym, var_name)
        } else {
            Resolved { found: true, name: var_name, unresolved: true, unresolved_ns: ns_sym, ..Resolved::none() }
        }
    }

    /// kondo `resolve-name`. `expr` is the usage/call node (used for positions and generated checks).
    pub fn resolve_name(&mut self, call: bool, name_sym: Name, expr: NodeId) -> Resolved {
        let cljs = self.is_cljs();
        let s = syms();
        let vi = varinfo();
        let mut name_sym = name_sym;
        let mut orig_name: SymId = SymId::NONE;
        loop {
            let (nsp, nm) = name_sym;
            if !nsp.is_none() {
                let name_str = nm.as_str();
                if !name_str.is_empty() && name_str.bytes().all(|b| b.is_ascii_digit()) {
                    if orig_name.is_none() {
                        orig_name = nm;
                    }
                    name_sym = (SymId::NONE, nsp);
                    continue;
                }
                let ns_str = nsp.as_str();
                let ns_sym = if cljs && ns_str.ends_with("$macros") { intern(ns_str.strip_suffix("$macros").unwrap()) } else { nsp };
                let cur = self.cur_ns();
                let q = cur.qualify.get(&ns_sym).copied().or(if cur.name == ns_sym { Some(ns_sym) } else { None });
                if let Some(ns_star) = q {
                    let core = ns_star == s.clojure_core || ns_star == s.cljs_core;
                    let (var_name, interop) = if cljs {
                        match name_str.split_once('.') {
                            Some((a, _)) => (intern(a), true),
                            None => (nm, false),
                        }
                    } else {
                        (nm, false)
                    };
                    let resolved_core = core && core_sym(cljs, var_name);
                    let alias = cur.aliases.contains_key(&ns_sym);
                    let mut r = Resolved::ok(ns_star, var_name);
                    r.interop = cljs && interop;
                    if alias {
                        r.alias = ns_sym;
                    }
                    if core {
                        r.resolved_core = resolved_core;
                    }
                    return r;
                }
                // imports
                let mut imp: Option<(SymId, SymId)> = None;
                if !cljs {
                    if let Some(&(class, pkg)) = vi.imports.get(&ns_sym) {
                        imp = Some((class, pkg));
                    } else if let Some(&(class, pkg)) = vi.fq_imports.get(&ns_sym) {
                        imp = Some((class, pkg));
                    }
                }
                if imp.is_none() {
                    if let Some(&pkg) = cur.imports.get(&ns_sym) {
                        imp = Some((ns_sym, pkg));
                    } else if cljs {
                        if let Some(&pkg) = cur.referred_globals.get(&ns_sym) {
                            imp = Some((ns_sym, pkg));
                        }
                    }
                }
                if let Some((class, pkg)) = imp {
                    self.lint_use_import(class);
                    self.java_used_import(class, pkg, nm, expr, call);
                    let full = if pkg.as_str().is_empty() { class.as_str().to_owned() } else { format!("{}.{}", pkg.as_str(), class.as_str()) };
                    return Resolved { found: true, interop: true, ns: intern(&full), name: nm, ..Resolved::none() };
                }
                if !cljs {
                    if ns_str != "clojure.core" && class_name_p(ns_str) {
                        self.java_class_usage_qualified(ns_sym, nm, expr, call);
                        return Resolved { found: true, interop: true, ns: ns_sym, name: nm, ..Resolved::none() };
                    }
                    if let Some(full) = self.defs.auto_ns(ns_sym) {
                        return Resolved::ok(full, nm); // Mova native namespace / default alias: no require needed
                    }
                    return self.unresolved_ns_result(expr, ns_sym, nm);
                }
                if !matches!(ns_str, "js" | "goog" | "Math" | "String") {
                    return self.unresolved_ns_result(expr, ns_sym, nm);
                }
                return Resolved::none();
            }
            // unqualified
            if call && is_special_symbol(nm.as_str()) {
                return Resolved { found: true, ns: self.core_ns(), name: nm, resolved_core: true, ..Resolved::none() };
            }
            if cljs {
                let (nn, n2) = self.normalize_sym_name_inner(nm);
                if !nn.is_none() {
                    name_sym = (nn, n2);
                    continue;
                }
                name_sym = (SymId::NONE, n2);
            }
            let nm = name_sym.1;
            let cur = self.cur_ns();
            if let Some(&(ns, name)) = cur.referred.get(&nm) {
                self.lint_use_referred(nm);
                self.lint_refer_all_use(ns, nm);
                return Resolved::ok(ns, name);
            }
            if cur.vars.contains(&nm) {
                return Resolved::ok(cur.name, nm);
            }
            // default imports
            let mut imp: Option<(SymId, SymId)> = None;
            if !cljs {
                if let Some(&(class, pkg)) = vi.imports.get(&nm) {
                    imp = Some((class, pkg));
                } else if let Some(&(class, pkg)) = vi.fq_imports.get(&nm) {
                    imp = Some((class, pkg));
                } else if let Some(&pkg) = cur.imports.get(&nm) {
                    imp = Some((nm, pkg));
                }
            } else {
                let s0 = nm.as_str();
                let fs = s0.split('.').next().map(intern);
                if let Some(fs) = fs {
                    if let Some(&pkg) = cur.imports.get(&fs) {
                        imp = Some((fs, pkg));
                    } else if let Some(&pkg) = cur.referred_globals.get(&fs) {
                        imp = Some((fs, pkg));
                    }
                }
            }
            if let Some((class, pkg)) = imp {
                self.lint_use_import(class);
                self.java_used_import(class, pkg, nm, expr, call);
                return Resolved { found: true, ns: pkg, interop: true, name: class, ..Resolved::none() };
            }
            let excluded = cur.clojure_excluded.contains(&nm);
            if (!excluded && core_sym(cljs, nm)) || (call && matches!(nm.as_str(), ".." | "let" | "fn" | "loop")) {
                return Resolved { found: true, ns: self.core_ns(), name: nm, resolved_core: true, ..Resolved::none() };
            }
            if cljs {
                if let Some(&ns_star) = cur.qualify.get(&nm) {
                    return Resolved::ok(ns_star, nm);
                }
            }
            let referred_all_ns = cur.refer_alls.iter().find(|(_, ex)| !ex.contains(&nm)).map(|(k, _)| *k);
            if let Some(rn) = referred_all_ns {
                self.lint_refer_all_use(rn, nm);
            }
            if referred_all_ns.is_none() && class_name_p(nm.as_str()) {
                self.java_class_usage_simple(nm, expr);
                return Resolved { found: true, interop: true, ..Resolved::none() };
            }
            return Resolved {
                found: true,
                ns: referred_all_ns.unwrap_or(s.unknown_ns),
                name: if orig_name.is_none() { nm } else { orig_name },
                unresolved: true,
                clojure_excluded: excluded,
                ..Resolved::none()
            };
        }
    }
}
