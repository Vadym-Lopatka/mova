//! Macro expansions kondo performs on syntax nodes (`macroexpand.clj`): ->, ->>, some->, cond->,
//! doto, .., dot-constructor, method invocation, do-template. Expansions build synthetic nodes
//! in the tree arena; synthetic tokens carry no position (like kondo `token-node`).
use super::*;

impl<'a> Analyzer<'a> {
    fn tok(&mut self, ns: &str, name: &str) -> NodeId {
        let ns = if ns.is_empty() { SymId::NONE } else { intern(ns) };
        self.c.push_token(Kind::Symbol, None, intern(name), ns, 0)
    }

    /// Wrap `target` in the metadata forms of `orig` (kondo `with-meta-of`).
    fn rewrap_meta(&mut self, orig: NodeId, target: NodeId) -> NodeId {
        let mut forms = Vec::new();
        let mut cur = orig;
        while let Some((mf, t)) = self.c.meta(cur) {
            forms.push(mf);
            cur = t;
        }
        let mut node = target;
        for &mf in forms.iter().rev() {
            node = self.c.push_container(Kind::Meta, Some(orig), &[mf, node], 0);
        }
        node
    }

    /// `->` / `->>` expansion.
    pub fn expand_thread(&mut self, expr: NodeId, last: bool) -> Option<NodeId> {
        let kids = self.kids(expr);
        let mut x = *kids.get(1)?;
        for &form in &kids[2..] {
            // a form with metadata (`^String (f)`) is still a list: thread into it and keep the metadata
            let ft = self.c.unwrap_meta(form);
            let threaded = if self.kind(ft) == Kind::List {
                let fk = self.kids(ft);
                let mut v: Vec<NodeId> = Vec::with_capacity(fk.len() + 1);
                if last {
                    v.extend_from_slice(&fk);
                    v.push(x);
                } else {
                    if let Some(&h) = fk.first() {
                        v.push(h);
                    }
                    v.push(x);
                    v.extend_from_slice(&fk[1.min(fk.len())..]);
                }
                let l = self.c.push_container(Kind::List, Some(ft), &v, 0);
                self.rewrap_meta(form, l)
            } else {
                self.c.push_container(Kind::List, Some(form), &[form, x], 0)
            };
            x = threaded;
        }
        Some(x)
    }

    /// Generated symbol positioned like `like`; it carries the metadata of `like` (kondo `with-meta-of`),
    /// so every analysis of the symbol analyzes that metadata again.
    fn gen_sym(&mut self, like: NodeId, prefix: &str) -> NodeId {
        let n = self.fresh();
        let name = intern(&format!("{}{}", prefix, n));
        let tok = self.c.push_token(Kind::Symbol, Some(like), name, SymId::NONE, F_GEN);
        let mut forms = Vec::new();
        let mut cur = like;
        while let Some((m, t)) = self.c.meta(cur) {
            forms.push(m);
            cur = t;
        }
        let mut node = tok;
        for &m in forms.iter().rev() {
            node = self.c.push_container(Kind::Meta, Some(like), &[m, node], 0);
        }
        node
    }

    pub fn expand_some_arrow(&mut self, expr: NodeId, last: bool) -> Option<NodeId> {
        let kids = self.kids(expr);
        let start = *kids.get(1)?;
        let forms = &kids[2..];
        let g = self.gen_sym(start, "G__");
        let thread = if last { "->>" } else { "->" };
        let let_t = self.tok("clojure.core", "let");
        let bv = self.c.push_container(Kind::Vector, None, &[g, start], 0);
        let body = if forms.is_empty() {
            g
        } else {
            let when_t = self.tok("clojure.core", "when");
            let some_t = self.tok("clojure.core", "some?");
            let cond = self.c.push_container(Kind::List, None, &[some_t, g], 0);
            let th = self.tok("clojure.core", thread);
            let mut v = vec![th, g];
            v.extend_from_slice(forms);
            let inner = self.c.push_container(Kind::List, None, &v, 0);
            self.c.push_container(Kind::List, None, &[when_t, cond, inner], 0)
        };
        Some(self.c.push_container(Kind::List, None, &[let_t, bv, body], 0))
    }

    pub fn expand_cond_arrow(&mut self, expr: NodeId, last: bool) -> Option<NodeId> {
        let kids = self.kids(expr);
        let start = *kids.get(1)?;
        let clauses = &kids[2..];
        let g = self.gen_sym(start, "G__");
        let thread = if last { "->>" } else { "->" };
        let mut steps: Vec<NodeId> = Vec::new();
        for ch in clauses.chunks(2) {
            let t = ch[0];
            if self.kind(t) == Kind::Keyword {
                self.lt.cond_arrow.push(t);
            }
            let step = ch.get(1).copied();
            let if_t = self.tok("", "if");
            let th = self.tok("clojure.core", thread);
            let mut inner = vec![th, g];
            if let Some(s) = step {
                inner.push(s);
            }
            let thl = self.c.push_container(Kind::List, None, &inner, 0);
            steps.push(self.c.push_container(Kind::List, None, &[if_t, t, thl, g], 0));
        }
        let mut bv = vec![g, start];
        if !steps.is_empty() {
            for s in &steps[..steps.len() - 1] {
                bv.push(g);
                bv.push(*s);
            }
        }
        let bvn = self.c.push_container(Kind::Vector, None, &bv, 0);
        let let_t = self.tok("clojure.core", "let");
        let body = steps.last().copied().unwrap_or(g);
        Some(self.c.push_container(Kind::List, None, &[let_t, bvn, body], 0))
    }

    pub fn expand_doto(&mut self, expr: NodeId) -> Option<NodeId> {
        let kids = self.kids(expr);
        let x = *kids.get(1)?;
        let gx = self.gen_sym(x, "_");
        let mut body: Vec<NodeId> = Vec::new();
        for &f in &kids[2..] {
            let l = if self.kind(f) == Kind::List {
                let fk = self.kids(f);
                let mut v = vec![fk[0], gx];
                v.extend_from_slice(&fk[1..]);
                self.c.push_container(Kind::List, Some(f), &v, 0)
            } else {
                self.c.push_container(Kind::List, Some(f), &[f, gx], 0)
            };
            body.push(l);
        }
        body.push(gx);
        let let_t = self.tok("clojure.core", "let");
        let bv = self.c.push_container(Kind::Vector, None, &[gx, x], 0);
        let mut v = vec![let_t, bv];
        v.extend(body);
        Some(self.c.push_container(Kind::List, None, &v, 0))
    }

    pub fn expand_double_dot(&mut self, expr: NodeId) -> Option<NodeId> {
        let kids = self.kids(expr);
        let mut x = *kids.get(1)?;
        let mut rest = &kids[2..];
        let mut node;
        loop {
            let form = *rest.first()?;
            let dot = self.tok("", ".");
            node = self.c.push_container(Kind::List, Some(expr), &[dot, x, form], F_SKIP);
            rest = &rest[1..];
            if rest.is_empty() {
                break;
            }
            x = node;
        }
        Some(node)
    }

    /// `(Foo. a b)` -> `(new Foo a b)`.
    pub fn expand_dot_constructor(&mut self, expr: NodeId) -> NodeId {
        let kids = self.kids(expr);
        let ctor = kids[0];
        let s = self.c.name(ctor).as_str();
        let name = intern(&s[..s.len() - 1]);
        let ctor_node = self.c.push_token(Kind::Symbol, Some(ctor), name, SymId::NONE, 0);
        let new_t = self.tok("", "new");
        let mut v = vec![new_t, ctor_node];
        v.extend_from_slice(&kids[1..]);
        self.c.push_container(Kind::List, Some(expr), &v, F_SKIP)
    }

    /// `(.meth obj args)` -> `(. obj meth args)`.
    pub fn expand_method_invocation(&mut self, expr: NodeId) -> NodeId {
        let kids = self.kids(expr);
        let meth = kids[0];
        let s = self.c.name(meth).as_str();
        let name = intern(&s[1..]);
        let meth_node = self.c.push_token(Kind::Symbol, Some(meth), name, SymId::NONE, 0);
        let dot = self.tok("", ".");
        let mut v = vec![dot];
        if let Some(&inv) = kids.get(1) {
            v.push(inv);
        }
        v.push(meth_node);
        if kids.len() > 2 {
            v.extend_from_slice(&kids[2..]);
        }
        self.c.push_container(Kind::List, Some(expr), &v, 0)
    }

    /// Copy `n` replacing symbols found in `map` (kondo `postwalk-replace` in `do-template`).
    fn subst(&mut self, n: NodeId, map: &[(SymId, SymId, NodeId)]) -> NodeId {
        match self.kind(n) {
            Kind::Symbol => {
                let (ns, nm) = (self.c.ns(n), self.c.name(n));
                for &(mns, mnm, to) in map {
                    if mns == ns && mnm == nm {
                        return to;
                    }
                }
                n
            }
            k if Cst::is_container(k) => {
                let kids = self.kids(n);
                let mut changed = false;
                let mut nk = Vec::with_capacity(kids.len());
                for &c in &kids {
                    let r = self.subst(c, map);
                    changed |= r != c;
                    nk.push(r);
                }
                if !changed {
                    return n;
                }
                let copy = self.c.push_clone(n, 0);
                self.c.set_children(copy, &nk);
                copy
            }
            _ => n,
        }
    }

    /// `(are [x y] expr a b c d)` -> `(do (is expr[a b]) (is expr[c d]))`.
    pub fn expand_are(&mut self, expr: NodeId, resolved_ns: SymId) -> Option<NodeId> {
        let kids = self.kids(expr);
        let argv = *kids.get(1)?;
        let template = *kids.get(2)?;
        let is_t = self.c.push_token(Kind::Symbol, None, intern("is"), resolved_ns, 0);
        let is_expr = self.c.push_container(Kind::List, None, &[is_t, template], 0);
        let args = self.kids(argv);
        let c = args.len();
        if c == 0 {
            return None;
        }
        let values = &kids[3..];
        self.lint_do_template(None, c, values.len());
        let mut forms = Vec::new();
        for ch in values.chunks(c) {
            if ch.len() < c {
                // kondo `(partition c values)` drops an incomplete trailing group
                continue;
            }
            let map: Vec<(SymId, SymId, NodeId)> = args.iter().zip(ch.iter()).filter(|(a, _)| self.kind(**a) == Kind::Symbol).map(|(a, v)| (self.c.ns(*a), self.c.name(*a), *v)).collect();
            forms.push(self.subst(is_expr, &map));
        }
        let do_t = self.tok("", "do");
        let mut v = vec![do_t];
        v.extend(forms);
        Some(self.c.push_container(Kind::List, None, &v, 0))
    }
}
