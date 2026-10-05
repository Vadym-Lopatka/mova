//! More special forms: let family, loop, letfn, case, try, protocols, records, defmethod, import.
use super::bindings::BOpts;
use super::forms::*;
use super::*;

impl<'a> Analyzer<'a> {
    // ---- let family ----

    /// kondo `analyze-let-like-bindings`: bindings are pushed onto the scope as they are created.
    pub fn analyze_let_like_bindings(&mut self, bv: NodeId, scoped: NodeId) {
        let kids = self.kids(bv);
        let for_like = matches!(self.ctx.resolved_as_core.as_str(), "for" | "doseq");
        let mut i = 0;
        while i < kids.len() {
            let binding = kids[i];
            let value = kids.get(i + 1).copied();
            i += 2;
            let is_kw = self.kind(binding) == Kind::Keyword;
            if for_like && self.is_kw_named(binding, "let") {
                if let Some(v) = value {
                    self.analyze_let_like_bindings(v, scoped);
                }
                continue;
            }
            let ret = value.and_then(|v| self.analyze_expression(v));
            if is_kw {
                continue;
            }
            let mut out = Vec::new();
            let init_tag = if self.lon && self.cs.len() >= 2 && self.cs[self.cs.len() - 2].1.as_str() == "let" && matches!(self.kind(self.c.unwrap_meta(binding)), Kind::Symbol) { value.map_or(0, |v| self.init_tag_code(v)) } else { 0 };
            self.extract_bindings(binding, scoped, BOpts { tag: init_tag, nil_lit: value.map_or(false, |v| self.kind(v) == Kind::Nil), ..BOpts::default() }, &mut out);
            if let (Some(r), true) = (&ret, self.lon) {
                if out.len() == 1 && self.kind(self.c.unwrap_meta(binding)) == Kind::Symbol {
                    out[0].ar = self.lint_arity_info(r.fixed, r.varargs_min);
                }
            }
            self.bindings.extend(out);
        }
    }

    /// kondo `analyze-like-let` (let, loop, for, doseq, dotimes, with-open, ...).
    pub fn analyze_like_let(&mut self, expr: NodeId) {
        let kids = self.kids(expr);
        if kids.len() < 2 {
            return;
        }
        let bv = kids[1];
        self.lint_let_binding_vector(bv);
        self.scope(|a| {
            a.cs.push((SymId::NONE, intern("let-bindings")));
            if a.kind(bv) == Kind::Vector {
                a.analyze_let_like_bindings(bv, expr);
            }
            a.cs.pop();
            let cur_let = matches!(a.cs.last(), Some(&(ns, n)) if (ns == syms().clojure_core || ns == syms().cljs_core) && n.as_str() == "let");
            a.ctx.let_parent = cur_let && kids.len() == 3;
            let tail = a.lon && cur_let && a.lt.tail_node == Some(expr);
            if tail {
                a.lt.tail_node = kids[2..].last().copied().map(|l| a.c.unwrap_meta(l));
            }
            a.analyze_children(&kids[2..]);
            if tail {
                let r = kids.last().copied().filter(|_| kids.len() > 2).and_then(|l| a.rt_of(l));
                a.lt.let_rets.insert(expr, r);
            }
        });
    }

    /// kondo `analyze-conditional-let` (if-let, when-let, if-some, when-some, when-first).
    pub fn analyze_conditional_let(&mut self, call: &str, expr: NodeId) {
        let kids = self.kids(expr);
        if kids.len() < 2 || self.kind(kids[1]) != Kind::Vector {
            return;
        }
        let bv = kids[1];
        let bvk = self.kids(bv);
        if bvk.len() != 2 {
            let p = self.pos(bv);
            self.lint(lint::FType::Syntax, p, format!("{} binding vector requires exactly 2 forms", call));
        }
        let if_ = matches!(call, "if-let" | "if-some");
        let body = &kids[2..];
        let scoped = if if_ { body.first().copied().unwrap_or(expr) } else { expr };
        let two = bvk.len() == 2;
        if let Some(&cond) = bvk.get(1) {
            self.scope(|a| {
                a.cs.push((SymId::NONE, intern("vector")));
                if call == "when-first" {
                    a.analyze_expression(cond);
                } else {
                    if !a.lt.lint_as_call && !a.lint_is_gen(expr) {
                        a.lint_condition(cond, matches!(call, "if-some" | "when-some"));
                    }
                    a.analyze_condition_nd(cond);
                }
            });
        }
        let mut out = Vec::new();
        self.scope(|a| {
            a.cs.push((SymId::NONE, intern("vector")));
            if two {
                a.extract_bindings(bvk[0], scoped, BOpts::default(), &mut out);
            } else {
                let mut i = 0;
                while i < bvk.len() {
                    a.extract_bindings(bvk[i], scoped, BOpts::default(), &mut out);
                    i += 2;
                }
            }
        });
        self.scope(|a| {
            a.bindings.extend(out.iter().copied());
            if if_ {
                if let Some(&first) = body.first() {
                    a.analyze_expression(first);
                }
                // the else branch does not see the binding
                a.bindings.truncate(a.bindings.len() - out.len());
                a.analyze_children(&body[1.min(body.len())..]);
            } else {
                a.analyze_children(body);
            }
        });
    }

    pub fn analyze_letfn(&mut self, expr: NodeId) {
        let kids = self.kids(expr);
        let Some(&fv) = kids.get(1) else { return };
        let fns: Vec<NodeId> = self.kids(fv);
        let se = self.scope_end(expr);
        self.scope(|a| {
            for &f in &fns {
                if let Some(nm) = a.c.nth(f, 0) {
                    if a.kind(nm) == Kind::Symbol {
                        let name = a.c.name(nm);
                        a.out.next_local_id += 1;
                        let id = a.out.next_local_id;
                        let p = a.pos(nm);
                        a.lint_reg_binding(id, name, nm, false);
                        if a.opts.locals {
                        a.out.locals.push(Local { id, name, str_: SymId::NONE, pos: p, scope_end_row: se.0, scope_end_col: se.1, lang: a.ltag });
                        }
                        a.bindings.push(Binding { key: name, name, id, gen: false, mark_used: false, ar: 0, tag: 0, nil_lit: false });
                    }
                }
            }
            let mut all: Vec<(FnBody, FnArity)> = Vec::new();
            let first_binding = a.bindings.len() - fns.iter().filter(|&&f| a.c.nth(f, 0).map_or(false, |nm| a.kind(nm) == Kind::Symbol)).count();
            let mut bi = first_binding;
            for &f in &fns {
                let fk = a.kids(f);
                if fk.is_empty() {
                    continue;
                }
                let bodies = a.fn_bodies(&fk[1..], f);
                let (mut fixed, mut va) = (Arities::default(), None);
                for b in bodies {
                    let ar = a.analyze_fn_arity(&b);
                    if ar.valid {
                        if let Some(x) = ar.fixed {
                            fixed.add(x);
                        }
                        if let Some(m) = ar.varargs_min {
                            va = Some(m);
                        }
                    }
                    all.push((b, ar));
                }
                if a.c.nth(f, 0).map_or(false, |nm| a.kind(nm) == Kind::Symbol) {
                    if a.lon && bi < a.bindings.len() {
                        let h = a.lint_arity_info(fixed, va);
                        a.bindings[bi].ar = h;
                    }
                    bi += 1;
                }
            }
            for (b, ar) in all {
                a.analyze_fn_body(&b, ar);
            }
            a.analyze_children(&kids[2..]);
        });
    }

    pub fn analyze_case(&mut self, expr: NodeId) {
        let kids = self.kids(expr);
        if kids.len() < 2 {
            return;
        }
        self.dropped(|a| a.analyze_expression(kids[1]));
        let cljs = self.is_cljs();
        let rest = &kids[2..];
        let mut i = 0;
        let mut seen_tests: Vec<String> = Vec::new();
        while i < rest.len() {
            let constant = rest[i];
            let Some(&e) = rest.get(i + 1) else {
                self.analyze_expression(constant);
                break;
            };
            self.lint_case_tests(&[constant], &mut seen_tests);
            let tests: Vec<NodeId> = if self.kind(constant) == Kind::List { self.kids(constant) } else { vec![constant] };
            self.dropped(|a| {
                a.scope(|b| {
                    if cljs {
                        b.ctx.off |= lint::uses::OFF_SYM | lint::uses::OFF_PRIV;
                    }
                    for t in tests {
                        b.analyze_usages2(t, !cljs, false);
                    }
                });
                a.analyze_expression(e);
            });
            i += 2;
        }
    }

    fn symbol_call(&self, n: NodeId) -> Option<&'static str> {
        if self.kind(n) != Kind::List {
            return None;
        }
        let f = self.c.nth(n, 0)?;
        if self.kind(f) == Kind::Symbol && self.c.ns(f).is_none() {
            Some(self.c.name(f).as_str())
        } else {
            None
        }
    }

    /// Name of an unqualified symbol node.
    fn bare_sym(&self, n: NodeId) -> Option<&'static str> {
        (self.kind(n) == Kind::Symbol && self.c.ns(n).is_none()).then(|| self.c.name(n).as_str())
    }

    pub fn analyze_try(&mut self, expr: NodeId) {
        let kids = self.kids(expr);
        let children = &kids[1.min(kids.len())..];
        let cut = children.iter().position(|&c| matches!(self.symbol_call(c), Some("catch") | Some("finally"))).unwrap_or(children.len());
        self.dropped(|a| a.analyze_children(&children[..cut]));
        for &c in &children[cut..] {
            match self.symbol_call(c) {
                Some("catch") => self.analyze_catch(c),
                Some("finally") => {
                    let ck = self.kids(c);
                    self.scope(|a| {
                        a.cs.push((SymId::NONE, intern("finally")));
                        a.analyze_children(&ck[1..]);
                    });
                }
                _ => {
                    self.analyze_expression(c);
                }
            }
        }
    }

    fn analyze_catch(&mut self, expr: NodeId) {
        let kids = self.kids(expr);
        self.scope(|a| {
            a.cs.push((SymId::NONE, intern("catch")));
            if kids.len() < 2 {
                return;
            }
            // Mova: `(catch e body..)` binds any thrown value; the typed form needs a class-looking head
            // (dotted or capitalized) followed by a symbol (Mova `parse_catch_head`).
            let untyped = a.opts.mova && !(kids.len() >= 3 && a.bare_sym(kids[1]).is_some_and(|s| s.contains('.') || s.chars().next().is_some_and(|c| c.is_uppercase())) && a.kind(kids[2]) == Kind::Symbol);
            let bi = if untyped { 1 } else { 2 };
            if !untyped {
                a.dropped(|a| a.analyze_expression(kids[1]));
            }
            let exprs = if kids.len() > bi + 1 { &kids[bi + 1..] } else { &kids[0..0] };
            if let Some(&b) = kids.get(bi) {
                let scoped = exprs.last().copied();
                let mut out = Vec::new();
                match scoped {
                    Some(s) => a.extract_bindings(b, s, BOpts { allow_amp: true, ..Default::default() }, &mut out),
                    None => {
                        // no body: kondo has no scope expr, the keys are present with null
                        a.extract_bindings_se(b, (u32::MAX, u32::MAX), BOpts { allow_amp: true, ..Default::default() }, &mut out)
                    }
                }
                a.bindings.extend(out);
            }
            a.analyze_children(exprs);
        });
    }

    pub fn analyze_as_arrow(&mut self, expr: NodeId) {
        let kids = self.kids(expr);
        if kids.len() < 3 {
            return;
        }
        self.analyze_expression(kids[1]);
        let mut out = Vec::new();
        self.extract_bindings(kids[2], expr, BOpts::default(), &mut out);
        self.scope(|a| {
            a.bindings.extend(out);
            a.analyze_children(&kids[3..]);
        });
    }

    pub fn analyze_areduce(&mut self, expr: NodeId) {
        let kids = self.kids(expr);
        if kids.len() < 6 {
            return;
        }
        let (array, idx, ret, init, body) = (kids[1], kids[2], kids[3], kids[4], kids[5]);
        let mut out = Vec::new();
        self.extract_bindings(idx, expr, BOpts::default(), &mut out);
        self.extract_bindings(ret, expr, BOpts::default(), &mut out);
        self.analyze_expression(array);
        self.analyze_expression(init);
        self.scope(|a| {
            a.bindings.extend(out);
            a.analyze_expression(body);
        });
    }

    pub fn analyze_this_as(&mut self, expr: NodeId) {
        let kids = self.kids(expr);
        if kids.len() < 2 {
            return;
        }
        let mut out = Vec::new();
        self.extract_bindings(kids[1], expr, BOpts::default(), &mut out);
        self.scope(|a| {
            a.bindings.extend(out);
            a.analyze_children(&kids[2..]);
        });
    }

    // ---- defmethod / protocols / records ----

    /// `(pr-str (sexpr node))` approximation.
    pub fn pr_str(&self, n: NodeId) -> String {
        let mut s = String::new();
        self.pr_str_into(n, &mut s);
        s
    }

    fn pr_str_into(&self, n: NodeId, out: &mut String) {
        let join = |a: &Self, kids: &[NodeId], sep: &str, out: &mut String| {
            for (i, &k) in kids.iter().enumerate() {
                if i > 0 {
                    out.push_str(sep);
                }
                a.pr_str_into(k, out);
            }
        };
        match self.kind(n) {
            // sexpr of `::k` reads in the `user` namespace, `::a/k` keeps its alias
            Kind::Keyword if self.c.flags(n) & F_AUTO != 0 => {
                let ns = self.c.ns(n);
                let resolved = if ns.is_none() { "user" } else { ns.as_str() };
                out.push_str(&format!(":{}/{}", resolved, self.c.name(n).as_str()));
            }
            Kind::List => {
                out.push('(');
                join(self, &self.kids(n), " ", out);
                out.push(')');
            }
            Kind::Vector => {
                out.push('[');
                join(self, &self.kids(n), " ", out);
                out.push(']');
            }
            Kind::Set => {
                out.push_str("#{");
                join(self, &self.kids(n), " ", out);
                out.push('}');
            }
            Kind::Map => {
                out.push('{');
                let kids = self.kids(n);
                for (i, pair) in kids.chunks(2).enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    join(self, pair, " ", out);
                }
                out.push('}');
            }
            Kind::Quote => {
                out.push_str("(quote ");
                join(self, &self.kids(n), " ", out);
                out.push(')');
            }
            Kind::Deref => {
                out.push_str("(clojure.core/deref ");
                join(self, &self.kids(n), " ", out);
                out.push(')');
            }
            Kind::Var => {
                out.push_str("(var ");
                join(self, &self.kids(n), " ", out);
                out.push(')');
            }
            Kind::Meta => self.pr_str_into(self.c.unwrap_meta(n), out),
            _ => out.push_str(&self.node_str(n)),
        }
    }

    pub fn analyze_defmethod(&mut self, expr: NodeId) {
        let kids = self.kids(expr);
        if kids.len() < 2 {
            return;
        }
        let (mname, dval) = (kids[1], kids.get(2).copied());
        let dstr = dval.map_or(SymId::NONE, |d| intern(&self.pr_str(d)));
        // kondo: `ctx-without-idx (dissoc ctx :idx :len)`
        self.scope(|a0| {
            a0.ctx.idx = u32::MAX;
            a0.ctx.len = u32::MAX;
            a0.scope(|a| {
                a.ctx.defmethod = true;
                a.ctx.dispatch_val = dstr;
                a.dropped(|a| a.analyze_usages2(mname, false, false));
            });
            if let Some(d) = dval {
                a0.dropped(|a| a.analyze_expression(d));
                let tail: Vec<NodeId> = kids[3..].to_vec();
                let dummy = a0.c.push_token(Kind::Symbol, None, syms().empty, SymId::NONE, 0);
                let mut ch = vec![dummy];
                ch.extend(tail);
                let f = a0.c.push_container(Kind::List, Some(expr), &ch, 0);
                a0.analyze_fn(f);
            }
        });
    }

    /// kondo `analyze-protocol-impls` (registration of protocol-impls is a hook in `extras`).
    pub fn analyze_protocol_impls(&mut self, by: DefBy, children: &[NodeId]) {
        let cljs = self.is_cljs();
        self.dropped(|a| a.analyze_protocol_impls_inner(by, children, cljs));
    }

    fn analyze_protocol_impls_inner(&mut self, by: DefBy, children: &[NodeId], cljs: bool) {
        let def_by = by.by.1.as_str();
        let is_ep = def_by == "extend-protocol";
        let is_et = def_by == "extend-type";
        // kondo loop state: current protocol (symbol; NONE = nil), resolved protocol ns/name, protocol node
        let mut current: Option<Name> = None;
        let (mut pns, mut pname) = (SymId::NONE, SymId::NONE);
        let mut pnode: Option<NodeId> = None;
        let mut methods: Vec<lint::proto::LMethod> = Vec::new();
        // kondo `end?`: no node or a symbol token
        let end_p = |a: &Self, n: Option<&NodeId>| -> bool { n.map_or(true, |&n| a.kind(a.c.unwrap_meta(n)) == Kind::Symbol) };
        for (i, &c0) in children.iter().enumerate() {
            let c = self.c.unwrap_meta(c0);
            let is_name = matches!(self.kind(c), Kind::Symbol | Kind::Nil);
            if is_name {
                let skip = cljs && self.kind(c) == Kind::Symbol && self.c.ns(c).is_none() && matches!(self.c.name(c).as_str(), "Object" | "number" | "function" | "default" | "object" | "string" | "bigint");
                if !skip {
                    self.analyze_expression(c0);
                }
                let sym: Option<Name> = if self.kind(c) == Kind::Symbol { Some((self.c.ns(c), self.c.name(c))) } else { None };
                let (pname2, pnode2, end_node): (Option<Name>, Option<NodeId>, Option<&NodeId>) = if is_ep {
                    match current {
                        None => (sym, Some(c), children.get(i + 2)),
                        Some(cur) => (Some(cur), pnode, children.get(i + 1)),
                    }
                } else if is_et {
                    match current {
                        None => match children.get(i + 1) {
                            Some(&snd) => {
                                let snd = self.c.unwrap_meta(snd);
                                (if self.kind(snd) == Kind::Symbol { Some((self.c.ns(snd), self.c.name(snd))) } else { None }, Some(snd), children.get(i + 2))
                            }
                            None => (None, None, children.get(i + 2)),
                        },
                        Some(_) => (sym, Some(c), children.get(i + 1)),
                    }
                } else {
                    (sym, Some(c), children.get(i + 1))
                };
                if !is_ep || pns.is_none() {
                    match pname2 {
                        Some(n) => {
                            let r = self.resolve_name(true, n, extras::NO_EXPR);
                            if r.found {
                                pns = r.ns;
                                pname = r.name;
                            } else {
                                pns = SymId::NONE;
                                pname = SymId::NONE;
                            }
                        }
                        None => {
                            pns = SymId::NONE;
                            pname = SymId::NONE;
                        }
                    }
                }
                if end_p(self, end_node) {
                    if let Some(pn) = pnode2 {
                        let written = pname2.map_or(SymId::NONE, |(ns, nm)| if ns.is_none() { nm } else { intern(&format!("{}/{}", ns.as_str(), nm.as_str())) });
                        self.lint_reg_proto(pn, pns, written, std::mem::take(&mut methods));
                    }
                }
                methods.clear();
                current = pname2;
                pnode = pnode2;
            } else if self.kind(c) == Kind::List {
                if current.is_some() && def_by != "definterface" {
                    self.protocol_impl_hook(c, by, pns, pname);
                }
                let ret = self.scope(|a| {
                    a.cs.push((SymId::NONE, intern("protocol-method")));
                    a.lt.protocol_next = true;
                    a.analyze_fn(c)
                });
                let first = self.c.children(c).first().copied();
                if let Some(f) = first {
                    let f = self.c.unwrap_meta(f);
                    if self.kind(f) == Kind::Symbol {
                        let (fixed, va) = ret.as_ref().map_or((Arities::default(), None), |r| (r.fixed, r.varargs_min));
                        methods.push(lint::proto::LMethod { name: self.c.name(f), pos: self.pos(f), fixed, varargs: va });
                    }
                }
                if end_p(self, children.get(i + 1)) {
                    if let Some(pn) = pnode {
                        self.lint_reg_proto(pn, pns, pname, std::mem::take(&mut methods));
                    }
                }
            }
        }
    }

    pub fn analyze_defprotocol(&mut self, expr: NodeId, by: DefBy) {
        let kids = self.kids(expr);
        if kids.len() < 2 {
            return;
        }
        let (name_t, name_meta) = self.lift_meta(kids[1]);
        let protocol_name = if self.kind(name_t) == Kind::Symbol { Some(self.c.name(name_t)) } else { None };
        let ns_name = self.cur_ns_name();
        let interface = by.lint_as.1.as_str() == "definterface";
        let docstring = kids.get(2).and_then(|&d| self.string_of(d));
        for &c in kids.iter().skip(2) {
            let c = self.c.unwrap_meta(c);
            if self.kind(c) != Kind::List {
                continue;
            }
            let ck = self.kids(c);
            if ck.is_empty() {
                continue;
            }
            let (nm_t, nm_meta) = self.lift_meta(ck[0]);
            let arities = &ck[1..];
            self.scope(|a| {
                a.ctx.off |= lint::uses::OFF_SYM;
                for &x in arities {
                    a.dropped(|a| a.analyze_usages2(x, false, false));
                }
            });
            if self.kind(nm_t) != Kind::Symbol {
                continue;
            }
            let fn_name = self.c.name(nm_t);
            let mut fixed = Arities::default();
            let mut arglists = Vec::new();
            for &x in arities {
                let x = self.c.unwrap_meta(x); // `(m ^Type [this] ...)` return-type hint
                if self.kind(x) == Kind::Vector {
                    fixed.add(self.c.children(x).len() as u32);
                    arglists.push(intern(&self.node_str(x)));
                }
            }
            let mut m = VarMeta::new(self.pos(nm_t), by);
            m.private = nm_meta.private;
            m.doc = arities.last().and_then(|&d| self.string_of(d));
            m.arglists = if arglists.is_empty() { None } else { Some(arglists) };
            m.fixed = Some(fixed);
            m.protocol = protocol_name.map(|p| (ns_name, p));
            m.deprecated = nm_meta.deprecated;
            m.added = nm_meta.added;
            let saved = self.ctx.skip_reg_var;
            self.ctx.skip_reg_var = interface;
            self.reg_var(fn_name, c, m);
            self.ctx.skip_reg_var = saved;
        }
        if let Some(p) = protocol_name {
            let mut m = VarMeta::new(self.pos(name_t), by);
            m.private = name_meta.private;
            m.doc = docstring.or(name_meta.doc.clone());
            m.user = name_meta.user.clone();
            m.deprecated = name_meta.deprecated;
            m.added = name_meta.added;
            self.reg_var(p, expr, m);
        }
    }

    pub fn analyze_defrecord(&mut self, expr: NodeId, by: DefBy) {
        let kids = self.kids(expr);
        if kids.len() < 2 {
            return;
        }
        let (name_t, name_meta) = self.lift_meta(kids[1]);
        let bv = kids.get(2).copied();
        let Some(bv) = bv else { return };
        let field_count = self.c.children(bv).len() as u32;
        let mut out = Vec::new();
        let saved = self.ctx.mark_bindings_used;
        self.ctx.mark_bindings_used = true;
        self.extract_bindings(bv, expr, BOpts::default(), &mut out);
        self.ctx.mark_bindings_used = saved;
        if self.kind(name_t) != Kind::Symbol {
            return;
        }
        let record = self.c.name(name_t);
        let np = self.pos(name_t);
        let mut m = VarMeta::new(np, by);
        m.private = name_meta.private;
        m.deprecated = name_meta.deprecated;
        m.added = name_meta.added;
        m.class = !self.is_cljs();
        m.doc = name_meta.doc.clone();
        m.user = name_meta.user.clone();
        self.reg_var(record, expr, m.clone());
        let nn = self.cur_ns_name();
        let cur = self.cur;
        self.lint_add_import(cur, record, nn, name_t, true);
        self.cur_ns_mut().imports.insert(record, nn);
        self.java_class_import_record(record, nn);
        let mut m2 = m.clone();
        m2.arglists = Some(vec![intern(&self.node_str(bv))]);
        m2.fixed = Some({
            let mut a = Arities::default();
            a.add(field_count);
            a
        });
        self.reg_var(intern(&format!("->{}", record.as_str())), expr, m2);
        if by.lint_as.1.as_str() == "defrecord" {
            let mut m3 = m.clone();
            m3.arglists = Some(vec![intern("[m]")]);
            m3.fixed = Some({
                let mut a = Arities::default();
                a.add(1);
                a
            });
            self.reg_var(intern(&format!("map->{}", record.as_str())), expr, m3);
        }
        self.scope(|a| {
            a.bindings.extend(out);
            a.analyze_protocol_impls(by, &kids[3..]);
        });
    }

    // ---- import / alias / in-ns ----

    /// kondo `analyze-import` for one libspec: returns `(class, package, class node)`.
    pub fn import_libspec(&mut self, n: NodeId) -> Vec<(SymId, SymId, NodeId)> {
        let n = if self.kind(n) == Kind::Quote { self.c.nth(n, 0).unwrap_or(n) } else { n };
        let mut res = Vec::new();
        match self.kind(n) {
            Kind::Vector | Kind::List => {
                let ch = self.kids(n);
                if let Some(&p) = ch.first() {
                    if self.kind(p) == Kind::Symbol {
                        let pkg = self.c.name(p);
                        let pkg = if self.c.ns(p).is_none() { pkg } else { intern(&format!("{}/{}", self.c.ns(p).as_str(), pkg.as_str())) };
                        for &c in &ch[1..] {
                            if self.kind(c) == Kind::Symbol {
                                res.push((self.c.name(c), pkg, c));
                            }
                        }
                    }
                }
            }
            Kind::Symbol => {
                let full = self.node_str(n);
                if let Some(i) = full.rfind('.') {
                    res.push((intern(&full[i + 1..]), intern(&full[..i]), n));
                } else {
                    res.push((intern(&full), intern(""), n));
                }
            }
            _ => {}
        }
        res
    }

    pub fn analyze_import(&mut self, expr: NodeId) {
        let kids = self.kids(expr);
        for &l in &kids[1..] {
            for (class, pkg, node) in self.import_libspec(l) {
                let cur = self.cur;
                self.lint_add_import(cur, class, pkg, node, false);
                self.cur_ns_mut().imports.insert(class, pkg);
                self.java_class_import(class, pkg, node, false);
            }
        }
    }

    pub fn analyze_alias(&mut self, expr: NodeId) {
        let kids = self.kids(expr);
        let quoted_sym = |a: &Self, n: Option<NodeId>| -> Option<SymId> {
            let n = n?;
            match a.kind(n) {
                Kind::Quote => {
                    let x = a.c.nth(n, 0)?;
                    if a.kind(x) == Kind::Symbol {
                        Some(a.c.name(x))
                    } else {
                        None
                    }
                }
                Kind::List => {
                    if a.symbol_call(n) == Some("quote") {
                        let x = a.c.nth(n, 1)?;
                        if a.kind(x) == Kind::Symbol {
                            return Some(a.c.name(x));
                        }
                    }
                    None
                }
                _ => None,
            }
        };
        let al = quoted_sym(self, kids.get(1).copied());
        let ns = quoted_sym(self, kids.get(2).copied());
        match (al, ns) {
            (Some(a), Some(n)) => {
                let cur = self.cur_ns_mut();
                cur.qualify.insert(a, n);
                cur.aliases.insert(a, n);
            }
            _ => self.analyze_children(&kids[1..]),
        }
    }

    pub fn analyze_in_ns(&mut self, expr: NodeId) {
        let kids = self.kids(expr);
        let name = kids.get(1).and_then(|&q| self.c.nth(q, 0)).filter(|&s| self.kind(s) == Kind::Symbol);
        if let Some(s) = name {
            let nm = self.c.name(s);
            // kondo deep-merges with a namespace of the same name: state is kept
            if let Some(i) = self.nss.iter().position(|n| n.name == nm) {
                self.cur = i;
            } else {
                self.nss.push(NsState::new(nm, self.lang));
                self.cur = self.nss.len() - 1;
            }
        }
        if kids.len() > 1 {
            self.analyze_children(&kids[1..]);
        }
    }
}
