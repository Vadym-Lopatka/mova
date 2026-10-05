//! `potemkin/import-vars`, `import-fn`, `import-macro`, `import-def` (kondo `analyzer/potemkin.clj`):
//! each imported var is defined in the current namespace and points at the imported var.
use super::forms::{DefBy, VarMeta};
use super::lint::uses::{OFF_SYM, OFF_VAR};
use super::*;

impl<'a> Analyzer<'a> {
    fn qualify_ns(&self, s: SymId) -> SymId {
        self.cur_ns().qualify.get(&s).copied().unwrap_or(s)
    }

    /// Analyze the symbol `n` as a var usage with unresolved-var/-symbol disabled, then define `local` in the current namespace.
    fn import_one(&mut self, call: NodeId, usage: NodeId, name_node: NodeId, imported_ns: SymId, imported_var: SymId, local: SymId, by: DefBy) {
        self.scope(|a| {
            a.ctx.off |= OFF_SYM | OFF_VAR;
            a.analyze_expression(usage);
        });
        let mut m = VarMeta::new(self.pos(name_node), by);
        m.imported = (imported_ns, imported_var);
        self.reg_var(local, call, m);
    }

    /// kondo `analyze-import-fn` (`import-fn`, `import-macro`, `import-def`).
    pub(crate) fn analyze_import_fn(&mut self, expr: NodeId, by: DefBy) {
        let kids = self.kids(expr);
        let Some(&sym) = kids.get(1) else { return };
        if self.kind(sym) != Kind::Symbol || self.c.ns(sym).is_none() {
            return;
        }
        let imported_ns = self.qualify_ns(self.c.ns(sym));
        let imported_var = self.c.name(sym);
        let local = match kids.get(2) {
            Some(&r) if self.kind(r) == Kind::Symbol => self.c.name(r),
            _ => imported_var,
        };
        self.import_one(expr, sym, sym, imported_ns, imported_var, local, by);
        self.note_used(imported_ns);
    }

    /// kondo `analyze-import-vars`.
    pub(crate) fn analyze_import_vars(&mut self, expr: NodeId, by: DefBy) {
        let kids = self.kids(expr);
        for &g in kids.iter().skip(1) {
            let gk = self.kind(g);
            let mut vars: Vec<(NodeId, SymId, SymId)> = Vec::new();
            let (imported_ns, written_ns);
            if gk == Kind::Symbol && !self.c.ns(g).is_none() {
                written_ns = SymId::NONE;
                imported_ns = self.qualify_ns(self.c.ns(g));
                vars.push((g, self.c.name(g), SymId::NONE));
            } else if matches!(gk, Kind::Vector | Kind::List) {
                let gc = self.kids(g);
                let Some(&first) = gc.first() else { continue };
                if self.kind(first) != Kind::Symbol {
                    continue;
                }
                written_ns = intern(&self.node_str(first));
                imported_ns = self.qualify_ns(written_ns);
                let rest = &gc[1..];
                let is_kw = |a: &Self, n: Option<&NodeId>, k: &str| n.map_or(false, |&n| a.kind(n) == Kind::Keyword && a.c.name(n).as_str() == k);
                if is_kw(self, rest.first(), "refer") {
                    let mut renames: Vec<(SymId, SymId)> = Vec::new();
                    if is_kw(self, rest.get(2), "rename") {
                        if let Some(&m) = rest.get(3) {
                            let mk = self.kids(m);
                            for p in mk.chunks(2) {
                                if p.len() == 2 && self.kind(p[0]) == Kind::Symbol && self.kind(p[1]) == Kind::Symbol {
                                    renames.push((self.c.name(p[0]), self.c.name(p[1])));
                                }
                            }
                        }
                    }
                    if let Some(&lst) = rest.get(1) {
                        for c in self.kids(lst) {
                            if self.kind(c) == Kind::Symbol {
                                let v = self.c.name(c);
                                let rn = renames.iter().find(|r| r.0 == v).map_or(SymId::NONE, |r| r.1);
                                vars.push((c, v, rn));
                            }
                        }
                    }
                } else {
                    for &c in rest {
                        if self.kind(c) == Kind::Symbol {
                            vars.push((c, self.c.name(c), SymId::NONE));
                        }
                    }
                }
            } else {
                continue;
            }
            for (node, var, rename) in vars {
                let local = if rename.is_none() { var } else { rename };
                let usage = if written_ns.is_none() { node } else { self.c.push_token(Kind::Symbol, Some(node), var, written_ns, 0) };
                self.import_one(expr, usage, node, imported_ns, var, local, by);
            }
            self.note_used(imported_ns);
        }
    }
}
