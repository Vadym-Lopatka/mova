//! kondo `java/reg-class-usage!` and its callers (`namespace/reg-used-import!`, `reg-imports!`, `resolve-name`).
use super::extras::NO_EXPR;
use super::*;

const NOPOS: Pos = Pos { row: 0, col: 0, end_row: 0, end_col: 0 };

impl<'a> Analyzer<'a> {
    /// Position of a usage expression (`(meta expr)`); absent for "no expression" and position-less nodes.
    fn expr_pos(&self, expr: NodeId) -> Pos {
        if expr == NO_EXPR {
            NOPOS
        } else {
            self.pos(expr)
        }
    }

    /// The symbol node whose position is `(meta name-sym)`: the call head for calls, else the node.
    fn name_node(&self, expr: NodeId, call: bool) -> NodeId {
        if expr != NO_EXPR && call && self.kind(expr) == Kind::List {
            self.c.children(expr).first().copied().map_or(expr, |h| self.c.unwrap_meta(h))
        } else {
            expr
        }
    }

    /// kondo `java/reg-class-usage!` 6-arity. `name`: explicit name-meta; `import_name`: name fields come with `loc` (reg-used-import!).
    #[allow(clippy::too_many_arguments)]
    fn reg_class_usage(&mut self, class: SymId, method: SymId, expr: NodeId, name: Option<Pos>, import_name: bool, call: u8, mut flags: u8) {
        if self.opts.external {
            return;
        }
        let loc_pre = self.expr_pos(expr);
        if expr != NO_EXPR && self.kind(expr) == Kind::List && self.c.flags(expr) & F_SKIP != 0 {
            flags |= JU_SKIP;
        }
        let mut branch = SymId::NONE;
        let mut tag = SymId::NONE;
        let mut meta_of = |a: &Self, n: NodeId| {
            if n != NO_EXPR {
                if a.ex.branch.contains(&n) {
                    branch = intern(if a.is_cljs() { "cljs" } else { "clj" });
                }
                if let Some(&t) = a.ex.list_tag.get(&n) {
                    tag = t;
                }
            }
        };
        meta_of(self, expr);
        let mut pos = loc_pre;
        let mut name_meta = name;
        if let Some(ctor) = self.ex.ctor {
            let cp = self.pos(ctor);
            if cp.row != 0 {
                pos = cp;
            }
            meta_of(self, ctor);
            if self.c.flags(ctor) & F_SKIP != 0 && self.kind(ctor) == Kind::List {
                flags |= JU_SKIP;
            }
            if name_meta.is_none() || import_name {
                name_meta = Some(loc_pre);
            }
        }
        let name_pos = match name_meta {
            Some(p) => {
                flags |= JU_HAS_NAME;
                p
            }
            None => NOPOS,
        };
        if self.base == BaseLang::Cljc {
            flags |= JU_CLJC;
        }
        if self.is_cljs() {
            flags |= JU_CLJS;
        }
        self.out.java_class_usages.push(JavaUsage { pos, name_pos, class, method, branch, tag, call, flags });
    }

    /// `Foo.Bar/baz` style class usage (kondo `resolve-name`, qualified class branch).
    pub fn java_class_usage_qualified(&mut self, class: SymId, method: SymId, expr: NodeId, call: bool) {
        let nn = self.name_node(expr, call);
        let np = self.expr_pos(nn);
        // `(meta name-sym)` exists for call heads only
        let name = if call && np.row != 0 { Some(np) } else { None };
        self.reg_class_usage(class, method, expr, name, false, call as u8 + 1, 0);
    }

    /// Bare class name usage (kondo `resolve-name`, `class-name?` branch).
    pub fn java_class_usage_simple(&mut self, class: SymId, expr: NodeId) {
        self.reg_class_usage(class, SymId::NONE, expr, None, false, 0, 0);
    }

    /// kondo `namespace/reg-used-import!`: a name that resolved through imports.
    pub fn java_used_import(&mut self, class: SymId, pkg: SymId, name_sym_name: SymId, expr: NodeId, call: bool) {
        let cs = class.as_str();
        let nm = name_sym_name.as_str();
        let method = if nm != cs && !nm.contains('.') { name_sym_name } else { SymId::NONE };
        let full = if pkg.as_str().is_empty() { format!(".{}", cs) } else { format!("{}.{}", pkg.as_str(), cs) };
        let nn = self.name_node(expr, call);
        let np = self.expr_pos(nn);
        let lp = self.expr_pos(expr);
        // name fields: (or (:row name-meta) (:row loc)) per field
        let name = Pos { row: if np.row != 0 { np.row } else { lp.row }, col: if np.row != 0 { np.col } else { lp.col }, end_row: if np.row != 0 { np.end_row } else { lp.end_row }, end_col: if np.row != 0 { np.end_col } else { lp.end_col } };
        self.reg_class_usage(intern(&full), method, expr, Some(name), true, call as u8 + 1, 0);
    }

    /// `(:import (pkg Class))`; `ns_form`: from the ns form (any language), else `(import ...)` (clj only).
    pub fn java_class_import(&mut self, class: SymId, pkg: SymId, node: NodeId, ns_form: bool) {
        if !ns_form && self.is_cljs() {
            return;
        }
        let full = intern(&format!("{}.{}", pkg.as_str(), class.as_str()));
        self.reg_class_usage(full, SymId::NONE, node, None, false, 0, JU_IMPORT);
    }

    /// `defrecord` / `deftype` register the record class as an import (`:clj-kondo/mark-used`).
    pub fn java_class_import_record(&mut self, record: SymId, ns: SymId) {
        if self.is_cljs() {
            return;
        }
        let full = intern(&format!("{}.{}", ns.as_str(), record.as_str()));
        self.reg_class_usage(full, SymId::NONE, NO_EXPR, None, false, 0, JU_IMPORT | JU_MARK_USED);
    }
}
