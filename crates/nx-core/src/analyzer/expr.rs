//! Expression dispatch: kondo `analyze-expression**`, `analyze-children`, `analyze-call`,
//! `analyze-usages2` and metadata lifting.
use super::defs::core_sym;
use super::lint::FType;
use super::resolve::Resolved;
use super::*;

/// Arity info returned by `analyze_fn` (kondo's `:arity` result meta).
#[derive(Clone, Debug, Default)]
pub(crate) struct ArityInfo {
    pub fixed: Arities,
    pub varargs_min: Option<u32>,
    pub arglists: Vec<SymId>,
}
pub(crate) type Ret = Option<ArityInfo>;

/// Summary of user metadata (`^:private`, `^{:doc "..."}`) relevant to analysis output.
#[derive(Clone, Debug, Default)]
pub(crate) struct MetaInfo {
    pub private: bool,
    pub macro_: bool,
    pub test: bool,
    pub dynamic: bool,
    pub deprecated: Val,
    pub added: Val,
    pub no_doc: Val,
    pub author: Val,
    pub export: Val,
    pub doc: Option<String>,
    /// A `:doc` key with a non-nil value exists.
    pub doc_present: bool,
    pub arglists: Vec<SymId>,
    pub has_arglists: bool,
    /// Retained user metadata (`:arglists`, `:style/indent`): key -> JSON.
    pub user: Vec<(String, String)>,
}

impl Default for Val {
    fn default() -> Val {
        Val::NONE
    }
}

fn hint_syms() -> &'static Vec<SymId> {
    static H: std::sync::OnceLock<Vec<SymId>> = std::sync::OnceLock::new();
    H.get_or_init(|| {
        ["int", "ints", "long", "longs", "float", "floats", "double", "doubles", "void", "short", "shorts", "boolean", "byte", "bytes", "char", "chars", "objects", "_"].iter().map(|s| intern(s)).collect()
    })
}

pub(crate) fn json_str(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for ch in s.chars() {
        match ch {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

impl<'a> Analyzer<'a> {
    pub fn push_cs_str(&mut self, name: &str) {
        self.cs.push((SymId::NONE, intern(name)));
    }

    /// JSON value of a literal node for metadata values.
    pub fn val_of(&self, n: NodeId) -> Val {
        match self.kind(n) {
            Kind::Nil => Val::NONE,
            Kind::True => Val(intern("true")),
            Kind::False => Val(intern("false")),
            Kind::String => Val(intern(&json_str(&unescape(self.c.string_content(n))))),
            Kind::Number => Val(intern(self.c.text(n))),
            Kind::Keyword | Kind::Symbol => Val(intern(&json_str(&self.node_str(n)))),
            _ => Val(intern(&json_str(&self.node_str(n)))),
        }
    }
    /// JSON of `(sexpr node)` as the oracle normalizes it (symbols/keywords -> strings, lists -> arrays).
    pub fn sexpr_json(&self, n: NodeId) -> String {
        match self.kind(n) {
            Kind::String => json_str(&unescape(self.c.string_content(n))),
            Kind::Nil => "null".into(),
            Kind::True => "true".into(),
            Kind::False => "false".into(),
            Kind::Number => self.c.text(n).to_owned(),
            Kind::List | Kind::Vector | Kind::Set => {
                let v: Vec<String> = self.c.children(n).iter().map(|&c| self.sexpr_json(c)).collect();
                format!("[{}]", v.join(","))
            }
            Kind::Meta => self.sexpr_json(self.c.unwrap_meta(n)),
            Kind::Quote => format!("[\"quote\",{}]", self.c.nth(n, 0).map_or("null".into(), |x| self.sexpr_json(x))),
            Kind::Deref => format!("[\"deref\",{}]", self.c.nth(n, 0).map_or("null".into(), |x| self.sexpr_json(x))),
            Kind::Var => format!("[\"var\",{}]", self.c.nth(n, 0).map_or("null".into(), |x| self.sexpr_json(x))),
            Kind::Map => {
                let kids = self.kids(n);
                let mut parts = Vec::new();
                let mut i = 0;
                while i + 1 < kids.len() {
                    let k = kids[i];
                    let ks = match self.kind(k) {
                        Kind::Keyword => {
                            let ns = self.c.ns(k);
                            if ns.is_none() { self.c.name(k).as_str().to_owned() } else { format!("{}/{}", ns.as_str(), self.c.name(k).as_str()) }
                        }
                        Kind::String => unescape(self.c.string_content(k)),
                        _ => self.node_str(k),
                    };
                    parts.push(format!("{}:{}", json_str(&ks), self.sexpr_json(kids[i + 1])));
                    i += 2;
                }
                format!("{{{}}}", parts.join(","))
            }
            Kind::Keyword => {
                let ns = self.c.ns(n);
                json_str(&if ns.is_none() { self.c.name(n).as_str().to_owned() } else { format!("{}/{}", ns.as_str(), self.c.name(n).as_str()) })
            }
            _ => json_str(&self.node_str(n)),
        }
    }
    fn truthy(&self, n: NodeId) -> bool {
        !matches!(self.kind(n), Kind::Nil | Kind::False)
    }

    /// Fold one metadata form into `info` (kondo `meta-node->map`).
    pub fn fold_meta_inner(&self, m: NodeId, info: &mut MetaInfo) {
        let mut set = |key: &str, v: Option<NodeId>| {
            if matches!(key, "arglists" | "style/indent") {
                let j = v.map_or("true".to_owned(), |x| self.sexpr_json(x));
                info.user.retain(|(k, _)| k != key);
                info.user.push((key.to_owned(), j));
            }
            let truthy = v.map_or(true, |x| self.truthy(x));
            match key {
                "private" => info.private = truthy,
                "macro" => info.macro_ = truthy,
                "test" => info.test = truthy,
                "dynamic" => info.dynamic = truthy,
                "deprecated" => info.deprecated = v.map_or(Val(intern("true")), |x| self.val_of(x)),
                "added" => info.added = v.map_or(Val(intern("true")), |x| self.val_of(x)),
                "no-doc" => info.no_doc = v.map_or(Val(intern("true")), |x| self.val_of(x)),
                "author" => info.author = v.map_or(Val(intern("true")), |x| self.val_of(x)),
                "export" => info.export = v.map_or(Val(intern("true")), |x| self.val_of(x)),
                "doc" => {
                    if let Some(x) = v {
                        if self.truthy(x) {
                            info.doc_present = true;
                            if self.kind(x) == Kind::String {
                                info.doc = Some(unescape(self.c.string_content(x)));
                            }
                        }
                    }
                }
                "arglists" => {
                    if let Some(x) = v {
                        if let Some(l) = self.arglists_of(x) {
                            info.has_arglists = true;
                            info.arglists = l;
                        }
                    }
                }
                _ => {}
            }
        };
        match self.kind(m) {
            Kind::Keyword => {
                if self.c.ns(m).is_none() {
                    set(self.c.name(m).as_str(), None);
                } else if self.c.ns(m).as_str() == "style" {
                    set(&format!("style/{}", self.c.name(m).as_str()), None);
                }
            }
            Kind::Map => {
                let kids = self.kids(m);
                let mut i = 0;
                while i + 1 < kids.len() {
                    let (k, v) = (kids[i], kids[i + 1]);
                    i += 2;
                    if self.kind(k) == Kind::Keyword {
                        if self.c.ns(k).is_none() {
                            set(self.c.name(k).as_str(), Some(v));
                        } else if self.c.ns(k).as_str() == "style" {
                            set(&format!("style/{}", self.c.name(k).as_str()), Some(v));
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// `(quote ([x] [x y]))` / `'([x])` -> arglist strings (kondo `meta-arglists-node->strs`).
    pub fn arglists_of(&self, n: NodeId) -> Option<Vec<SymId>> {
        if self.kind(n) != Kind::Quote {
            return None;
        }
        let l = self.c.nth(n, 0)?;
        if self.kind(l) != Kind::List {
            return None;
        }
        let mut out = Vec::new();
        for &v in self.c.children(l) {
            let v = self.c.unwrap_meta(v);
            if self.kind(v) != Kind::Vector {
                return None;
            }
            out.push(intern(&self.node_str(v)));
        }
        Some(out)
    }

    /// kondo `lift-meta-content2`: analyze the metadata forms of `n` and return the target node.
    pub fn lift_meta(&mut self, n: NodeId) -> (NodeId, MetaInfo) {
        let mut info = MetaInfo::default();
        if self.kind(n) != Kind::Meta {
            return (n, info);
        }
        let mut cur = n;
        let mut forms: Vec<NodeId> = Vec::new();
        while let Some((m, t)) = self.c.meta(cur) {
            forms.push(m);
            cur = t;
        }
        for &m in forms.iter().rev() {
            self.analyze_meta_form(m);
            self.fold_meta_inner(m, &mut info);
        }
        (cur, info)
    }

    fn analyze_meta_form(&mut self, m: NodeId) {
        let cljs = self.is_cljs();
        self.lint_meta_keys(m);
        self.scope(|a| {
            a.push_cs_str("metadata");
            a.ctx.in_meta = true;
            if cljs {
                a.ctx.off |= lint::uses::OFF_SYM;
            }
            for &h in hint_syms() {
                a.bindings.push(Binding { key: h, name: SymId::NONE, id: 0, gen: false, mark_used: false, ar: 0, tag: 0, nil_lit: false });
            }
            if cljs {
                for h in ["js", "number"] {
                    a.bindings.push(Binding { key: intern(h), name: SymId::NONE, id: 0, gen: false, mark_used: false, ar: 0, tag: 0, nil_lit: false });
                }
            }
            a.dropped(|a| a.analyze_expression(m));
        });
    }

    pub fn find_binding(&self, key: SymId) -> Option<Binding> {
        self.bindings.iter().rev().find(|b| b.key == key).copied()
    }

    // ---- analyze-children / expression ----

    /// kondo `analyze-children`.
    pub fn analyze_children(&mut self, children: &[NodeId]) {
        let tl = self.ctx.top_level
            && match self.cs.last() {
                Some(&(ns, name)) => (ns == syms().clojure_core || ns == syms().cljs_core) && matches!(name.as_str(), "comment" | "do" | "let"),
                None => false,
            };
        if self.ctx.in_comment && self.lc().skip_comments {
            return;
        }
        let saved = (self.ctx.top_level, self.ctx.idx, self.ctx.len);
        self.ctx.top_level = tl;
        self.ctx.len = children.len() as u32;
        for (i, &c) in children.iter().enumerate() {
            self.ctx.idx = i as u32;
            self.analyze_expression(c);
        }
        self.ctx.top_level = saved.0;
        self.ctx.idx = saved.1;
        self.ctx.len = saved.2;
    }

    /// Children of a container as analyze-children input.
    pub fn analyze_kids(&mut self, n: NodeId) {
        let v = self.kids(n);
        self.analyze_children(&v);
    }

    /// kondo `analyze-expression**`.
    pub fn analyze_expression(&mut self, n: NodeId) -> Ret {
        let n = if self.kind(n) == Kind::Meta {
            let t = self.lift_meta(n).0;
            self.note_list_meta(n, t);
            t
        } else {
            n
        };
        match self.kind(n) {
            Kind::Quote => {
                self.lint_unused_value_expr(n);
                let c = self.ctx;
                if !(self.ctx.sq > 0) {
                    self.ctx.quoted = true;
                }
                self.analyze_kids(n);
                self.ctx = c;
                None
            }
            Kind::SyntaxQuote => {
                self.lint_unused_value_expr(n);
                self.analyze_usages2(n, false, false);
                None
            }
            Kind::Var => {
                self.lint_unused_value_expr(n);
                self.analyze_var(n);
                None
            }
            Kind::Tagged => {
                // reader macro: analyze the form, not the tag
                let v = self.kids(n);
                let c = self.ctx;
                self.analyze_children(&v[1.min(v.len())..]);
                self.ctx = c;
                None
            }
            Kind::Unquote | Kind::UnquoteSplicing => {
                self.analyze_unquote(n);
                None
            }
            Kind::NsMap => {
                self.lint_unused_value_expr(n);
                self.lint_map_keys(n);
                self.analyze_namespaced_map(n);
                None
            }
            Kind::Map => {
                self.lint_unused_value_expr(n);
                self.lint_map_keys(n);
                self.scope(|a| {
                    a.push_cs_str("map");
                    a.ctx.recur = lint::R_NONTAIL;
                    a.analyze_kids(n);
                });
                None
            }
            Kind::Set => {
                self.lint_unused_value_expr(n);
                self.lint_set_keys(n);
                self.scope(|a| {
                    a.push_cs_str("set");
                    a.ctx.recur = lint::R_NONTAIL;
                    a.analyze_kids(n);
                });
                None
            }
            Kind::Vector => {
                self.lint_unused_value_expr(n);
                self.scope(|a| {
                    a.push_cs_str("vector");
                    a.ctx.recur = lint::R_NONTAIL;
                    a.analyze_kids(n);
                });
                None
            }
            Kind::AnonFn => {
                self.lint_unused_value_expr(n);
                if self.ctx.in_fn_literal && !self.lint_is_gen(n) {
                    let p = self.pos(n);
                    self.lint(FType::Syntax, p, "Nested #()s are not allowed");
                }
                let (f, first) = self.expand_fn(n);
                self.scope(|a| {
                    a.ctx.in_fn_literal = true;
                    if first {
                        a.bindings.push(Binding { key: syms().percent, name: SymId::NONE, id: 0, gen: false, mark_used: false, ar: 0, tag: 0, nil_lit: false });
                    }
                    a.analyze_expression(f)
                })
            }
            Kind::Deref => {
                let x = self.c.nth(n, 0);
                if let Some(x) = x {
                    let core = self.core_ns();
                    let head = self.c.push_token(Kind::Symbol, None, syms().deref, core, 0);
                    let l = self.c.push_container(Kind::List, Some(n), &[head, x], 0);
                    return self.analyze_expression(l);
                }
                None
            }
            Kind::Symbol | Kind::Keyword => {
                if self.ctx.quoted {
                    self.quoted_token(n);
                } else {
                    self.analyze_usages2(n, false, false);
                }
                None
            }
            Kind::List => self.analyze_list(n),
            Kind::Regex => {
                self.lint_unused_value_expr(n);
                None
            }
            Kind::String | Kind::Number | Kind::Char | Kind::Nil | Kind::True | Kind::False | Kind::Symbolic => {
                self.lint_unused_value_token(n, false);
                None
            }
            _ => {
                self.scope(|a| {
                    a.push_cs_str("eval");
                    a.analyze_kids(n);
                });
                None
            }
        }
    }

    fn quoted_token(&mut self, n: NodeId) {
        if self.kind(n) == Kind::Keyword {
            self.keyword_usage(n);
        } else {
            self.quoted_symbol(n);
        }
    }

    fn analyze_var(&mut self, n: NodeId) {
        let c = self.ctx;
        self.ctx.private_access = true;
        self.analyze_kids(n);
        self.ctx = c;
    }

    fn analyze_unquote(&mut self, n: NodeId) {
        if self.lon && self.ctx.sq <= 0 && !self.cs.iter().any(|&(ns, nm)| ns.as_str() == "leiningen.core.project" && nm.as_str() == "defproject") {
            let p = self.pos(n);
            let msg = if self.kind(n) == Kind::Unquote { "Unquote (~) not syntax-quoted" } else { "Unquote-splicing (~@) not syntax-quoted" };
            self.lint(FType::UnquoteNotSyntaxQuoted, p, msg);
        }
        let c = self.ctx;
        self.ctx.sq = if self.ctx.sq != 0 { self.ctx.sq - 1 } else { -1 };
        self.analyze_kids(n);
        self.ctx = c;
    }

    fn analyze_namespaced_map(&mut self, n: NodeId) {
        let kids = self.kids(n);
        if kids.len() < 2 {
            return;
        }
        let (k, m) = (kids[0], kids[1]);
        let auto = self.c.flags(k) & F_AUTO != 0;
        let ns = self.c.name(k);
        let resolved = if ns.is_none() {
            self.cur_ns_name()
        } else if auto {
            self.cur_ns().qualify.get(&ns).copied().unwrap_or(ns)
        } else {
            ns
        };
        self.lint_use_ns(resolved);
        let c = self.ctx;
        self.push_cs_str("namespaced-map");
        self.ctx.nsmap_prefix = resolved;
        self.note_nsmap_keys(m, resolved);
        self.analyze_expression(m);
        self.cs.pop();
        self.ctx = c;
    }

    // ---- lists ----

    fn analyze_list(&mut self, n: NodeId) -> Ret {
        let kids = self.kids(n);
        if kids.is_empty() {
            return None;
        }
        let (function, _) = self.lift_meta(kids[0]);
        if self.ctx.quoted {
            self.scope(|a| {
                a.cs.push((SymId::NONE, intern("list")));
                a.analyze_children(&kids);
            });
            return None;
        }
        match self.kind(function) {
            Kind::Map | Kind::Vector | Kind::Set => {
                self.lint_head_call(n, function, kids.len() - 1);
                self.scope(|a| {
                    a.push_cs_str(match a.kind(function) {
                        Kind::Map => "map",
                        Kind::Vector => "vector",
                        _ => "set",
                    });
                    a.analyze_children(&kids);
                });
                None
            }
            Kind::Quote => {
                self.lint_head_call(n, function, kids.len() - 1);
                self.scope(|a| {
                    a.push_cs_str("quote");
                    a.analyze_children(&kids);
                });
                None
            }
            Kind::Keyword => {
                self.lint_head_call(n, function, kids.len() - 1);
                self.analyze_keyword_call(n, &kids);
                None
            }
            Kind::Symbol => {
                let full = (self.c.ns(function), self.c.name(function));
                let simple = full.0.is_none();
                let mut full = full;
                if simple {
                    let (a, b) = self.normalize_sym_name_pub(full.1);
                    if !a.is_none() {
                        full = (a, b);
                    } else {
                        full = (SymId::NONE, b);
                    }
                }
                let binding = if simple { self.find_binding(full.1) } else { None };
                if let Some(b) = binding {
                    self.analyze_binding_call(n, function, b, &kids);
                    None
                } else {
                    self.analyze_call(n, function, full, kids.len() as u32 - 1)
                }
            }
            Kind::True | Kind::False | Kind::String | Kind::Char | Kind::Number => {
                self.lint_head_call(n, function, kids.len() - 1);
                self.scope(|a| {
                    a.push_cs_str("list");
                    a.analyze_children(&kids[1..]);
                });
                None
            }
            Kind::Nil => {
                // kondo: a `nil` head falls to the catch-all branch, analyzed without a callstack frame
                self.analyze_children(&kids);
                None
            }
            _ => {
                self.scope(|a| {
                    a.push_cs_str("list");
                    a.analyze_children(&kids);
                });
                None
            }
        }
    }

    pub fn normalize_sym_name_pub(&self, s: SymId) -> Name {
        // cljs `foo.bar` property access -> `foo` (see resolve.rs)
        self.normalize_sym_name_inner(s)
    }

    fn analyze_keyword_call(&mut self, n: NodeId, kids: &[NodeId]) {
        let _ = n;
        self.keyword_usage(kids[0]);
        self.scope(|a| {
            a.push_cs_str("token");
            a.analyze_children(&kids[1..]);
        });
    }

    fn analyze_binding_call(&mut self, expr: NodeId, f: NodeId, b: Binding, kids: &[NodeId]) {
        self.reg_used_binding(b, self.pos(expr), self.pos(f));
        self.lint_binding_call(expr, b, kids.len() - 1);
        self.scope(|a| {
            a.cs.push((SymId::NONE, b.key));
            a.dropped(|a| a.analyze_children(&kids[1..]));
        });
    }

    /// kondo `namespace/reg-used-binding!` + `analysis/reg-local-usage!`.
    pub fn reg_used_binding(&mut self, b: Binding, pos: Pos, name_pos: Pos) {
        self.lint_use_binding(b.id);
        if b.gen || b.mark_used || !self.opts.locals {
            return;
        }
        let _ = name_pos;
        self.out.local_usages.push(LocalUsage { id: b.id, name: b.name, pos: Pos { row: pos.row, col: pos.col, end_row: pos.end_row, end_col: pos.end_col }, name_pos, lang: self.ltag });
    }

    // ---- usages (kondo analyzer/usages.clj) ----

    /// kondo `analyze-usages2`.
    pub fn analyze_usages2(&mut self, n0: NodeId, quote: bool, syntax_quote: bool) {
        // kondo attaches metadata to the node: tag and children are those of the target
        let n = self.c.unwrap_meta(n0);
        let t = self.kind(n);
        let quote = quote || t == Kind::Quote;
        let sq_tag = t == Kind::SyntaxQuote;
        let unquote = matches!(t, Kind::Unquote | Kind::UnquoteSplicing);
        let saved = self.ctx;
        let ncs = self.cs.len();
        let new_level = self.ctx.sq + sq_tag as i32;
        self.ctx.sq = new_level;
        if sq_tag {
            self.cs.push((SymId::NONE, intern("syntax-quote")));
        }
        let syntax_quote = syntax_quote || sq_tag;
        if new_level > 0 && unquote {
            self.analyze_expression(n0);
        } else if quote {
            if t == Kind::Keyword {
                self.keyword_usage(n);
            }
            for c in self.kids(n) {
                self.analyze_usages2(c, true, syntax_quote);
            }
        } else {
            if n0 != n {
                self.lift_meta_usage(n0);
            }
            match t {
                Kind::Symbol => {
                    self.usage_symbol(n, new_level > 0);
                    self.lint_unused_value_token(n0, true);
                }
                Kind::Keyword => {
                    self.lint_unused_value_token(n0, false);
                    self.keyword_usage(n);
                }
                Kind::Tagged => {
                    for c in self.kids(n).into_iter().skip(1) {
                        self.analyze_usages2(c, quote, syntax_quote);
                    }
                }
                _ => {
                    for c in self.kids(n) {
                        self.analyze_usages2(c, quote, syntax_quote);
                    }
                }
            }
        }
        self.ctx = saved;
        self.cs.truncate(ncs);
    }

    /// `lift-meta-content2` with `only-usage?`: metadata forms go through analyze-usages2.
    fn lift_meta_usage(&mut self, n: NodeId) -> NodeId {
        let mut cur = n;
        let mut forms = Vec::new();
        while let Some((m, t)) = self.c.meta(cur) {
            forms.push(m);
            cur = t;
        }
        for &m in forms.iter().rev() {
            self.scope(|a| {
                a.push_cs_str("metadata");
                a.ctx.in_meta = true;
                for &h in hint_syms() {
                    a.bindings.push(Binding { key: h, name: SymId::NONE, id: 0, gen: false, mark_used: false, ar: 0, tag: 0, nil_lit: false });
                }
                a.analyze_usages2(m, false, false);
            });
        }
        cur
    }

    fn usage_symbol(&mut self, n: NodeId, in_sq: bool) {
        let sym = (self.c.ns(n), self.c.name(n));
        let simple = sym.0.is_none();
        let sym = if simple { self.normalize_sym_name_pub(sym.1) } else { sym };
        let sym = if simple && sym.0.is_none() { (SymId::NONE, sym.1) } else { sym };
        if simple && sym.0.is_none() && !in_sq {
            if let Some(b) = self.find_binding(sym.1) {
                let p = self.pos(n);
                if !self.lt.undefined_locals.is_empty() && self.lt.undefined_locals.contains(&sym.1) {
                    self.lint(FType::DestructuredOrBindingOfSameMap, p, format!("Destructured :or refers to binding of same map: {}", sym.1.as_str()));
                }
                self.reg_used_binding(b, p, p);
                return;
            }
        }
        let mut r = self.resolve_name(false, sym, n);
        if r.unresolved && !sym.1.as_str().is_empty() {
            let s = sym.1.as_str();
            if s != "." && s.ends_with('.') && sym.0.is_none() {
                r = self.resolve_name(true, (SymId::NONE, intern(&s[..s.len() - 1])), n);
            }
        }
        if !in_sq && !r.unresolved_ns.is_none() {
            let p = self.pos(n);
            self.lint_unresolved_ns(r.unresolved_ns, r.name, p);
        }
        if r.found && !r.ns.is_none() {
            self.note_used(r.ns);
        }
        if r.found {
            self.lint_use_ns(r.ns);
        }
        if !r.found || r.ns.is_none() || r.interop {
            return;
        }
        let p = self.pos(n);
        let name = r.name;
        if self.lon && self.c.ns(n).is_none() {
            let full = self.c.name(n);
            self.lt.qualify_self = self.cur_ns().qualify.get(&full) == Some(&full);
        }
        let (defmethod, dv) = (self.ctx.defmethod, self.ctx.dispatch_val);
        self.reg_var_usage(VarUsageArgs { name, pos: p, name_pos: p, arity: NO_ARITY, r, refer: false, defmethod, dispatch_val: dv, testing: SymId::NONE, derived: self.c.flags(n) & F_DERIVED != 0, derived_name: self.c.flags(n) & F_DERIVED != 0, no_from_var: false, from: SymId::NONE, written: sym });
    }

    pub fn reg_var_usage(&mut self, a: VarUsageArgs) {
        if !self.opts.var_usages {
            return;
        }
        let s = syms();
        self.out.var_usages.push(VarUsage {
            pos: a.pos,
            name_pos: a.name_pos,
            name: a.name,
            from: if a.from.is_none() { self.cur_ns_name() } else { a.from },
            from_var: if a.no_from_var { SymId::NONE } else { self.ctx.in_def },
            arity: a.arity,
            alias: a.r.alias,
            refer: a.refer,
            defmethod: a.defmethod,
            derived: a.derived,
            derived_name: a.derived_name,
            dispatch_val_str: a.dispatch_val,
            ctx_testing: a.testing,
            resolved_ns: a.r.ns,
            unresolved: a.r.unresolved,
            clojure_excluded: a.r.clojure_excluded,
            lang: self.ltag,
            to: SymId::NONE,
            fixed: Arities::default(),
            has_fixed: false,
            varargs_min: NO_ARITY,
            macro_: false,
            private: false,
            deprecated: Val::NONE,
            call_lang: if self.lang == Lang::Cljs { L_CLJS } else { L_CLJ },
            synth: false,
        });
        let _ = s;
        self.lint_reg_use(&a);
    }

    // ---- fn literal ----

    /// kondo `macroexpand/expand-fn`: returns the `(fn* [args] (body))` node and whether `%1` exists.
    fn expand_fn(&mut self, n: NodeId) -> (NodeId, bool) {
        let mut found: Vec<u32> = Vec::new(); // 0 = %&
        self.find_percent(n, &mut found);
        found.sort_unstable();
        let varargs = found.first() == Some(&0);
        let args: Vec<u32> = if varargs { found[1..].to_vec() } else { found.clone() };
        let max_n = args.last().copied().unwrap_or(0);
        let kids = self.kids(n);
        let body = self.c.push_container(Kind::List, Some(n), &kids, 0);
        {
            let node = &mut self.c.nodes[body.0 as usize];
            if node.flags & F_WIDE == 0 {
                node.col += 1;
            }
        }
        let mut argv: Vec<NodeId> = Vec::new();
        for i in 1..=max_n {
            let name = intern(&format!("%{}", i));
            argv.push(self.c.push_token(Kind::Symbol, None, name, SymId::NONE, F_SKIP));
        }
        if varargs {
            argv.push(self.c.push_token(Kind::Symbol, None, syms().amp, SymId::NONE, F_SKIP));
            argv.push(self.c.push_token(Kind::Symbol, None, intern("%&"), SymId::NONE, F_SKIP));
        }
        let vec = self.c.push_container(Kind::Vector, None, &argv, 0);
        let head = self.c.push_token(Kind::Symbol, None, syms().fn_star, SymId::NONE, 0);
        let f = self.c.push_container(Kind::List, Some(n), &[head, vec, body], 0);
        (f, args.first() == Some(&1))
    }

    fn find_percent(&self, n: NodeId, out: &mut Vec<u32>) {
        for &c in self.c.children(n) {
            match self.kind(c) {
                Kind::Symbol => {
                    if self.c.ns(c).is_none() {
                        let s = self.c.name(c).as_str();
                        if let Some(rest) = s.strip_prefix('%') {
                            let v = if rest.is_empty() {
                                Some(1)
                            } else if rest == "&" {
                                Some(0)
                            } else if rest.len() <= 2 && rest.bytes().all(|b| b.is_ascii_digit()) {
                                rest.parse::<u32>().ok()
                            } else {
                                None
                            };
                            if let Some(v) = v {
                                if !out.contains(&v) {
                                    out.push(v);
                                }
                            }
                        }
                    }
                }
                _ => self.find_percent(c, out),
            }
        }
    }
}

pub(crate) struct VarUsageArgs {
    pub name: SymId,
    pub pos: Pos,
    pub name_pos: Pos,
    pub arity: u16,
    pub r: Resolved,
    pub refer: bool,
    pub defmethod: bool,
    pub dispatch_val: SymId,
    pub testing: SymId,
    pub derived: bool,
    pub derived_name: bool,
    pub no_from_var: bool,
    /// Namespace the call was made from (`NONE` = current).
    pub from: SymId,
    /// The symbol as written (ns part, name part), for messages.
    pub written: Name,
}

#[allow(dead_code)]
fn _unused() {
    let _ = core_sym;
}
