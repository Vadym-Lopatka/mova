//! Special-form analyzers ported from kondo `analyzer.clj`: fn/defn/def/let/loop/case/try/...
use super::bindings::BOpts;
use super::expr::*;
use super::*;

/// `:defined-by` / `:defined-by->lint-as` of definitions.
#[derive(Clone, Copy, Debug)]
pub(crate) struct DefBy {
    pub by: Name,
    pub lint_as: Name,
}

/// Attributes handed to `reg_var` (kondo `reg-var!` metadata).
#[derive(Clone, Debug)]
pub(crate) struct VarMeta {
    pub name_pos: Pos,
    pub private: bool,
    pub macro_: bool,
    pub test: bool,
    pub fixed: Option<Arities>,
    pub varargs_min: Option<u32>,
    pub doc: Option<String>,
    pub deprecated: Val,
    pub added: Val,
    pub export: Val,
    pub protocol: Option<(SymId, SymId)>,
    pub arglists: Option<Vec<SymId>>,
    pub by: DefBy,
    pub temp: bool,
    pub class: bool,
    pub declared: bool,
    pub user: Vec<(String, String)>,
    /// potemkin import: (imported ns, imported var), NONE otherwise.
    pub imported: (SymId, SymId),
}

impl VarMeta {
    pub fn new(name_pos: Pos, by: DefBy) -> VarMeta {
        VarMeta { name_pos, private: false, macro_: false, test: false, fixed: None, varargs_min: None, doc: None, deprecated: Val::NONE, added: Val::NONE, export: Val::NONE, protocol: None, arglists: None, by, temp: false, class: false, declared: false, user: Vec::new(), imported: (SymId::NONE, SymId::NONE) }
    }
}

/// One fn arity: `kids[0]` is the arg vector, `scope` the node whose end closes the param scope.
#[derive(Clone)]
pub(crate) struct FnBody {
    pub kids: Vec<NodeId>,
    pub scope: NodeId,
}

pub(crate) struct FnArity {
    pub bindings: Vec<Binding>,
    pub fixed: Option<u32>,
    pub varargs_min: Option<u32>,
    pub arglist: SymId,
    pub valid: bool,
}

impl<'a> Analyzer<'a> {
    // ---- vars ----

    /// kondo `namespace/reg-var!`.
    pub fn reg_var(&mut self, name: SymId, expr: NodeId, mut m: VarMeta) {
        self.lint_reg_var(name, expr, m.temp, m.declared, m.by.by);
        let pos = self.pos(expr);
        let ns_now = self.cur_ns_name();
        if !m.temp && (ns_now == syms().clojure_core || ns_now == syms().cljs_core) {
            self.core_override(name.as_str(), &mut m);
        }
        if !m.temp && !name.is_none() {
            let cs_off = self.out.callstacks.len() as u32;
            if self.opts.callstack {
                for i in (0..self.cs.len()).rev() {
                    let e = self.cs[i];
                    self.out.callstacks.push(e);
                }
            }
            let cs_len = self.out.callstacks.len() as u32 - cs_off;
            let (ao, al) = match &m.arglists {
                Some(v) => {
                    let o = self.out.strs.len() as u32;
                    self.out.strs.extend_from_slice(v);
                    (o, v.len() as u32)
                }
                None => (0, 0),
            };
            let doc = match &m.doc {
                Some(d) => intern(d),
                None => SymId::NONE,
            };
            self.out.var_definitions.push(VarDef {
                pos,
                name_pos: m.name_pos,
                name,
                ns: self.cur_ns_name(),
                defined_by: m.by.by,
                defined_by_lint_as: m.by.lint_as,
                cs: (cs_off, cs_len),
                doc,
                arglists: (ao, al),
                has_arglists: m.arglists.is_some(),
                declared: m.declared,
                fixed: m.fixed.unwrap_or_default(),
                has_fixed: m.fixed.is_some(),
                varargs_min: m.varargs_min.map_or(NO_ARITY, |v| v as u16),
                private: m.private,
                macro_: m.macro_,
                test: m.test,
                deprecated: m.deprecated,
                added: m.added,
                export: m.export,
                protocol_name: m.protocol.map_or(SymId::NONE, |p| p.1),
                protocol_ns: m.protocol.map_or(SymId::NONE, |p| p.0),
                meta: if m.user.is_empty() {
                    Val::NONE
                } else {
                    let parts: Vec<String> = m.user.iter().map(|(k, v)| format!("{}:{}", super::expr::json_str(k), v)).collect();
                    Val(intern(&format!("{{{}}}", parts.join(","))))
                },
                imported: m.imported,
                lang: self.ltag,
            });
        }
        if !self.ctx.skip_reg_var && !name.is_none() {
            self.cur_ns_mut().vars.insert(name);
        }
    }

    /// kondo `analysis/reg-var!` attr overrides for clojure.core / cljs.core (see overrides.clj).
    fn core_override(&self, name: &str, m: &mut VarMeta) {
        let ar = |v: &[u32]| {
            let mut a = Arities::default();
            for &x in v {
                a.add(x);
            }
            a
        };
        let cljs_lang = self.lang == Lang::Cljs;
        match (self.base, cljs_lang) {
            (BaseLang::Clj, _) if self.cur_ns_name() == syms().clojure_core => {}
            (BaseLang::Cljc, _) if self.cur_ns_name() == syms().cljs_core => {}
            (BaseLang::Cljs, _) if self.cur_ns_name() == syms().cljs_core => {
                if name == "array" {
                    m.varargs_min = Some(0);
                }
                return;
            }
            _ => return,
        }
        match name {
            "def" => {
                m.macro_ = true;
                m.fixed = Some(ar(&[1, 2, 3]));
            }
            "defn" | "defn-" | "defmacro" => {
                m.macro_ = true;
                m.varargs_min = Some(2);
            }
            "quote" | "var" => {
                m.macro_ = true;
                m.fixed = Some(ar(&[1]));
            }
            "set!" => {
                m.macro_ = true;
                m.fixed = Some(if self.base == BaseLang::Cljc && cljs_lang { ar(&[2, 3]) } else { ar(&[2]) });
            }
            "if-some" | "if-let" => m.fixed = Some(ar(&[2, 3])),
            "throw" if self.base == BaseLang::Clj => {
                m.macro_ = true;
                m.fixed = Some(ar(&[1]));
            }
            _ => {}
        }
    }

    // ---- fn ----

    /// kondo `fn-bodies`.
    pub fn fn_bodies(&mut self, exprs: &[NodeId], whole: NodeId) -> Vec<FnBody> {
        let mut i = 0;
        while i < exprs.len() {
            let e = exprs[i];
            let (t, _) = self.lift_meta(e);
            match self.kind(t) {
                Kind::Vector => {
                    return vec![FnBody { kids: exprs[i..].to_vec(), scope: whole }];
                }
                Kind::List => {
                    return exprs[i..].iter().map(|&x| self.c.unwrap_meta(x)).filter(|&x| self.kind(x) == Kind::List).map(|l| FnBody { kids: self.kids(l), scope: l }).collect();
                }
                _ => {}
            }
            i += 1;
        }
        Vec::new()
    }

    /// kondo `analyze-arity`.
    fn arity_of(&self, argvec: NodeId) -> (Option<u32>, Option<u32>) {
        let mut fixed = 0u32;
        for &c in self.c.children(argvec) {
            if self.is_sym_named(c, "&") {
                return (None, Some(fixed));
            }
            fixed += 1;
        }
        (Some(fixed), None)
    }

    /// kondo `analyze-fn-arity`: extracts the param bindings (registering locals) once.
    pub fn analyze_fn_arity(&mut self, body: &FnBody) -> FnArity {
        let mut fa = FnArity { bindings: Vec::new(), fixed: None, varargs_min: None, arglist: SymId::NONE, valid: false };
        let Some(&argvec) = body.kids.first() else { return fa };
        let (av, _) = self.lift_meta_quiet(argvec);
        if self.kind(av) != Kind::Vector {
            return fa;
        }
        let mut out = Vec::new();
        let saved_dupes = self.lt.fn_dupes.replace(Vec::new());
        self.extract_bindings(argvec, body.scope, BOpts { fn_args: true, ..Default::default() }, &mut out);
        self.lt.fn_dupes = saved_dupes;
        let (f, m) = self.arity_of(av);
        fa.bindings = out;
        fa.fixed = f;
        fa.varargs_min = m;
        fa.arglist = intern(&self.node_str(av));
        fa.valid = true;
        fa
    }

    /// Target of a meta chain without analyzing the metadata.
    pub fn lift_meta_quiet(&self, n: NodeId) -> (NodeId, ()) {
        (self.c.unwrap_meta(n), ())
    }

    /// kondo `analyze-fn-body`.
    pub fn analyze_fn_body(&mut self, body: &FnBody, ar: FnArity) {
        let ret_target = self.lt.ret_target.take();
        if !ar.valid {
            return;
        }
        let (ar_fixed, ar_varargs) = (ar.fixed, ar.varargs_min);
        self.scope(|a| {
            a.bindings.extend(ar.bindings.iter().copied());
            a.ctx.top_level = false;
            a.ctx.recur = match (ar.fixed, ar.varargs_min) {
                (Some(f), _) => f,
                (None, Some(m)) => m + 1,
                _ => lint::R_NIL,
            };
            if a.lang == Lang::Cljs {
                // kondo checks `^:async` on the arg vector, which analyzes its metadata once more
                a.dropped(|a| {
                    a.lift_meta(body.kids[0]);
                });
            }
            let rest: Vec<NodeId> = body.kids[1..].to_vec();
            if a.lon && !a.ctx.docstring && rest.len() > 1 && a.kind(a.c.unwrap_meta(rest[0])) == Kind::String && matches!(a.cs.last(), Some(&(ns, n)) if (ns == syms().clojure_core || ns == syms().cljs_core) && matches!(n.as_str(), "defn" | "defn-")) {
                let p = a.pos(rest[0]);
                a.lint(lint::FType::MisplacedDocstring, p, "Misplaced docstring.");
            }
            let mut children: &[NodeId] = &rest;
            // :pre / :post map
            if rest.len() > 1 {
                let first = rest[0];
                if a.kind(first) == Kind::Map {
                    let mk = a.kids(first);
                    let has = mk.iter().step_by(2).any(|&k| a.is_kw_named(k, "pre") || a.is_kw_named(k, "post"));
                    if has {
                        a.analyze_pre_post_map(first);
                        children = &rest[1..];
                    }
                }
            }
            if a.ctx.shallow {
                return;
            }
            if ret_target.is_some() && a.lon {
                a.lt.tail_node = children.last().copied().map(|l| a.c.unwrap_meta(l));
            }
            a.analyze_children(children);
            if let (Some(t), true) = (ret_target, a.lon) {
                let last = children.last().copied();
                a.record_fn_ret(t, ar_fixed, ar_varargs, last);
            }
        });
    }

    fn analyze_pre_post_map(&mut self, map: NodeId) {
        self.scope(|a| {
            a.cs.push((SymId::NONE, intern("pre-post")));
            let kv = a.kids(map);
            let mut i = 0;
            while i + 1 < kv.len() {
                let (k, v) = (kv[i], kv[i + 1]);
                i += 2;
                a.analyze_expression(k);
                if a.is_kw_named(k, "post") {
                    a.scope(|b| {
                        b.bindings.push(Binding { key: syms().percent, name: SymId::NONE, id: 0, gen: false, mark_used: false, ar: 0, tag: 0, nil_lit: false });
                        b.analyze_expression(v);
                    });
                } else {
                    a.analyze_expression(v);
                }
            }
        });
    }

    /// kondo `analyze-fn` (fn, fn*, bound-fn, #()).
    pub fn analyze_fn(&mut self, expr: NodeId) -> Ret {
        let children = self.kids(expr);
        if children.is_empty() {
            return None;
        }
        let fn_sym = children[0];
        self.lift_meta(fn_sym);
        let name_expr = children.get(1).copied();
        let fn_name = name_expr.filter(|&n| self.kind(n) == Kind::Symbol);
        if let Some(n) = name_expr {
            if self.kind(n) == Kind::Meta {
                self.lift_meta(n);
            }
        }
        let bodies = self.fn_bodies(&children[1..], expr);
        self.lint_def_fn(expr);
        let arities: Vec<FnArity> = bodies.iter().map(|b| self.analyze_fn_arity(b)).collect();
        let mut info = ArityInfo::default();
        let mut have = false;
        for a in &arities {
            if !a.valid {
                continue;
            }
            have = true;
            if let Some(f) = a.fixed {
                info.fixed.add(f);
            }
            if let Some(m) = a.varargs_min {
                info.varargs_min = Some(m);
            }
            info.arglists.push(a.arglist);
        }
        let seen = self.lint_new_seen();
        let pf = std::mem::take(&mut self.lt.protocol_next);
        let self_ar = if fn_name.is_some() && have { self.lint_arity_info(info.fixed, info.varargs_min) } else { 0 };
        self.scope(|a| {
            a.ctx.seen = seen;
            a.ctx.protocol_fn = pf;
            if let Some(n) = fn_name {
                let nm = a.c.name(n);
                a.bindings.push(Binding { key: nm, name: nm, id: 0, gen: false, mark_used: false, ar: self_ar, tag: 0, nil_lit: false });
            }
            for (b, ar) in bodies.iter().zip(arities) {
                a.analyze_fn_body(b, ar);
            }
        });
        if have {
            Some(info)
        } else {
            None
        }
    }

    // ---- defn / def ----

    pub fn string_of(&self, n: NodeId) -> Option<String> {
        if let Some(s) = self.syn_str.get(&n) {
            return Some(s.clone());
        }
        if self.kind(n) == Kind::String {
            Some(unescape(self.c.string_content(n)))
        } else {
            None
        }
    }

    /// kondo `analyze-defn`.
    pub fn analyze_defn(&mut self, expr: NodeId, by: DefBy, test: bool) -> Ret {
        let kids = self.kids(expr);
        if kids.len() < 2 {
            let p = self.pos(expr);
            self.lint(lint::FType::Syntax, p, "Invalid function body.");
            return None;
        }
        let call = self.c.name(kids[0]).as_str();
        let is_defn_minus = call == "defn-";
        let name_node = kids[1];
        self.lint_return_type_hint(name_node);
        let (name_t, name_meta) = self.lift_meta(name_node);
        let mut fn_name = if self.kind(name_t) == Kind::Symbol { Some(self.c.name(name_t)) } else { None };
        let name_pos = self.pos(name_t);
        // kondo `lint-fn-name!`: a qualified name is kept as written
        if self.kind(name_t) == Kind::Symbol && !self.c.ns(name_t).is_none() {
            let full = self.node_str(name_t);
            self.lint(lint::FType::Syntax, name_pos, format!("Function name must be simple symbol but got: {}", full));
            fn_name = Some(intern(&full));
        }
        let mut children: Vec<NodeId> = kids[2..].to_vec();
        let mut docstring: Option<String> = None;
        if let Some(&f) = children.first() {
            if let Some(d) = self.string_of(f) {
                docstring = Some(d);
                children.remove(0);
            }
        }
        let mut meta_node = None;
        if let Some(&f) = children.first() {
            if self.kind(f) == Kind::Map {
                meta_node = Some(f);
                children.remove(0);
            }
        }
        let mut meta_node2 = None;
        if let Some(&f) = children.first() {
            if self.kind(f) == Kind::List {
                if let Some(&l) = children[1..].last() {
                    if self.kind(l) == Kind::Map {
                        meta_node2 = Some(l);
                        children.pop();
                    }
                }
            }
        }
        // analyze attr-maps (callstack entry [] avoids unused-value lint)
        for mn in [meta_node, meta_node2].into_iter().flatten() {
            self.scope(|a| {
                a.cs.push((SymId::NONE, SymId::NONE));
                a.dropped(|a| a.analyze_expression(mn));
            });
        }
        let mut var_meta = name_meta.clone();
        for mn in [meta_node, meta_node2].into_iter().flatten() {
            let mut mi = MetaInfo::default();
            self.fold_meta_pub(mn, &mut mi);
            // attr-map doc wins over a positional docstring
            if mi.doc.is_some() {
                docstring = mi.doc.clone();
            }
            merge_meta(&mut var_meta, &mi);
        }
        if docstring.is_none() && name_meta.doc_present {
            docstring = name_meta.doc.clone();
        }
        let macro_ = by.lint_as.1.as_str() == "defmacro" && matches!(by.lint_as.0.as_str(), "clojure.core" | "cljs.core") || var_meta.macro_;
        let private = is_defn_minus || var_meta.private;
        let bodies = self.fn_bodies(&children, expr);
        if bodies.is_empty() {
            let p = self.pos(expr);
            self.lint(lint::FType::Syntax, p, "Invalid function body.");
        }
        if let Some(n) = fn_name {
            let mut m = VarMeta::new(name_pos, by);
            m.temp = true;
            self.reg_var(n, expr, m);
        }
        let mut arities: Vec<(Option<u32>, Option<u32>, SymId, bool)> = Vec::new();
        for b in &bodies {
            let in_def = fn_name.unwrap_or(SymId::NONE);
            let info = self.scope(|a| {
                a.ctx.docstring = docstring.is_some();
                a.ctx.in_def = in_def;
                a.ctx.macro_ = macro_;
                if macro_ {
                    for k in ["&env", "&form"] {
                        a.bindings.push(Binding { key: intern(k), name: SymId::NONE, id: 0, gen: false, mark_used: false, ar: 0, tag: 0, nil_lit: false });
                    }
                }
                let ar = a.analyze_fn_arity(b);
                let info = (ar.fixed, ar.varargs_min, ar.arglist, ar.valid);
                a.lt.ret_target = if macro_ { None } else { fn_name };
                a.analyze_fn_body(b, ar);
                a.lt.ret_target = None;
                info
            });
            arities.push(info);
        }
        let mut fixed = Arities::default();
        let mut varargs_min: Option<u32> = None;
        for (f, m, _, ok) in &arities {
            if !ok {
                continue;
            }
            if let Some(f) = f {
                fixed.add(*f);
            }
            if m.is_some() {
                varargs_min = *m;
            }
        }
        if let Some(n) = fn_name {
            let mut m = VarMeta::new(name_pos, by);
            m.private = private;
            m.macro_ = macro_;
            m.test = test || var_meta.test;
            m.deprecated = var_meta.deprecated;
            m.added = var_meta.added;
            m.export = var_meta.export;
            m.fixed = if fixed.is_empty() { None } else { Some(fixed) };
            m.varargs_min = varargs_min;
            m.doc = docstring;
            m.user = var_meta.user.clone();
            let arglists = if let Some(mn) = meta_node2.and_then(|x| self.meta_arglists(x)) {
                Some(mn)
            } else if let Some(mn) = meta_node.and_then(|x| self.meta_arglists(x)) {
                Some(mn)
            } else {
                Some(arities.iter().filter(|a| a.3).map(|a| a.2).collect())
            };
            m.arglists = arglists;
            self.reg_var(n, expr, m);
        }
        None
    }

    /// `:arglists '([x])` value of a meta map node.
    fn meta_arglists(&self, map: NodeId) -> Option<Vec<SymId>> {
        let kv = self.kids(map);
        let mut i = 0;
        while i + 1 < kv.len() {
            if self.is_kw_named(kv[i], "arglists") {
                return self.arglists_of(kv[i + 1]);
            }
            i += 2;
        }
        None
    }

    pub fn fold_meta_pub(&self, m: NodeId, info: &mut MetaInfo) {
        self.fold_meta_inner(m, info)
    }

    /// kondo `analyze-def` (def, defonce, defmulti, goog-define).
    pub fn analyze_def(&mut self, expr: NodeId, by: DefBy) -> Ret {
        let kids = self.kids(expr);
        self.lint_def_args(expr, &kids);
        if kids.len() < 2 {
            return None;
        }
        let (name_t, name_meta) = self.lift_meta(kids[1]);
        let mut children: Vec<NodeId> = kids[2..].to_vec();
        let mut docstring: Option<String> = None;
        if children.len() > 1 {
            if let Some(d) = self.string_of(children[0]) {
                docstring = Some(d);
                children.remove(0);
            }
        }
        let defmulti = by.lint_as.1.as_str() == "defmulti" && matches!(by.lint_as.0.as_str(), "clojure.core" | "cljs.core");
        let mut var_meta = name_meta.clone();
        let mut extra_meta_node = None;
        if defmulti {
            if let Some(&c) = children.first() {
                if self.kind(c) == Kind::Map {
                    extra_meta_node = Some(c);
                    children.remove(0);
                    let mut mi = MetaInfo::default();
                    self.fold_meta_pub(c, &mut mi);
                    if mi.doc.is_some() {
                        docstring = mi.doc.clone();
                    }
                    merge_meta(&mut var_meta, &mi);
                }
            }
        }
        if docstring.is_none() && name_meta.doc_present {
            docstring = name_meta.doc.clone();
        }
        let var_name = if self.kind(name_t) == Kind::Symbol {
            let (ns, nm) = (self.c.ns(name_t), self.c.name(name_t));
            if ns.is_none() {
                Some(nm)
            } else if self.cur_ns().qualify.get(&ns) == Some(&self.cur_ns_name()) {
                Some(nm)
            } else {
                None
            }
        } else {
            None
        };
        let core_def = matches!(self.cs.last(), Some(&(ns, n)) if (ns == syms().clojure_core || ns == syms().cljs_core) && n.as_str() == "def");
        let in_def = var_name.unwrap_or(SymId::NONE);
        let mut init: Ret = None;
        let mut analyzed_init = false;
        let shallow = self.ctx.shallow;
        let ctx_saved = self.ctx;
        self.ctx.in_def = in_def;
        self.ctx.defmulti = defmulti;
        if !shallow && core_def && children.len() == 1 {
            init = self.analyze_expression(children[0]);
            analyzed_init = true;
        }
        let name_pos = self.pos(name_t);
        if let Some(n) = var_name {
            let mut m = VarMeta::new(name_pos, by);
            m.private = var_meta.private;
            m.macro_ = var_meta.macro_;
            m.deprecated = var_meta.deprecated;
            m.added = var_meta.added;
            m.export = var_meta.export;
            m.test = var_meta.test;
            m.doc = docstring;
            m.user = var_meta.user.clone();
            if let Some(ai) = &init {
                m.fixed = Some(ai.fixed);
                m.varargs_min = ai.varargs_min;
            }
            let al = self.meta_arglists_all(&name_meta, extra_meta_node, init.as_ref());
            m.arglists = al;
            self.reg_var(n, expr, m);
        }
        if !analyzed_init && !shallow {
            self.analyze_children(&children);
        }
        self.ctx = ctx_saved;
        init
    }

    fn meta_arglists_all(&self, name_meta: &MetaInfo, extra: Option<NodeId>, init: Option<&ArityInfo>) -> Option<Vec<SymId>> {
        if name_meta.has_arglists {
            return Some(name_meta.arglists.clone());
        }
        if let Some(e) = extra {
            if let Some(l) = self.meta_arglists(e) {
                return Some(l);
            }
        }
        init.filter(|i| !i.arglists.is_empty()).map(|i| i.arglists.clone())
    }

    /// kondo `analyze-declare`.
    pub fn analyze_declare(&mut self, expr: NodeId, by: DefBy) {
        let kids = self.kids(expr);
        // kondo: `vars` is read once, before the declared names are registered
        let existing: Vec<SymId> = if self.lon {
            kids[1..].iter().filter_map(|&n| {
                let t = self.c.unwrap_meta(n);
                (self.kind(t) == Kind::Symbol && self.cur_ns().vars.contains(&self.c.name(t))).then(|| self.c.name(t))
            }).collect()
        } else {
            Vec::new()
        };
        for &n in &kids[1..] {
            let (t, _) = self.lift_meta(n);
            if self.kind(t) != Kind::Symbol {
                continue;
            }
            let nm = self.c.name(t);
            if self.lon && existing.contains(&nm) {
                let p = self.pos(expr);
                self.lint(lint::FType::RedundantDeclare, p, format!("Redundant declare: {}", nm.as_str()));
            }
            let mut m = VarMeta::new(self.pos(t), by);
            m.temp = false;
            m.declared = true;
            self.reg_var(nm, expr, m);
        }
    }
}

pub(crate) fn merge_meta(into: &mut MetaInfo, other: &MetaInfo) {
    into.private |= other.private;
    into.macro_ |= other.macro_;
    into.test |= other.test;
    into.dynamic |= other.dynamic;
    if !other.deprecated.is_none() {
        into.deprecated = other.deprecated;
    }
    if !other.added.is_none() {
        into.added = other.added;
    }
    if !other.no_doc.is_none() {
        into.no_doc = other.no_doc;
    }
    if !other.author.is_none() {
        into.author = other.author;
    }
    if !other.export.is_none() {
        into.export = other.export;
    }
    if other.doc_present {
        into.doc_present = true;
        into.doc = other.doc.clone();
    }
    for (k, v) in &other.user {
        into.user.retain(|(kk, _)| kk != k);
        into.user.push((k.clone(), v.clone()));
    }
    if other.has_arglists {
        into.has_arglists = true;
        into.arglists = other.arglists.clone();
    }
}
