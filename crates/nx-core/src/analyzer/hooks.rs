//! Native ports of clj-kondo `:analyze-call` hooks (the originals are Clojure code run in sci).
//! A hook maps a call node to a replacement node; generated nodes take the position of the original
//! call and are flagged `F_DERIVED` (kondo `hooks/annotate`). Ported: `babashka.fs/with-temp-dir`,
//! `replicant.defalias/{defalias,aliasfn}`, the e2test project hook `hooks.with-timing/with-timing` and clj-kondo's `hooks.one-of/one-of`.
use super::*;

impl<'a> Analyzer<'a> {
    /// borkdude/deflet hook (`deflet*` over the call children, head included): `def`s become nested `let`s.
    fn deflet_star(&mut self, expr: NodeId, children: &[NodeId]) -> Option<NodeId> {
        let f = *children.first()?;
        let r = &children[1..];
        let is_def = self.kind(f) == Kind::List && {
            let fk = self.kids(f);
            fk.first().map_or(false, |&h| self.kind(h) == Kind::Symbol && self.c.ns(h).is_none() && matches!(self.c.name(h).as_str(), "def" | "defp"))
        };
        if is_def {
            let dc = self.kids(f);
            let (a, b) = (*dc.get(1)?, *dc.get(2)?);
            let let_t = self.dtok(f, "clojure.core", "let");
            let vec = self.dvec(f, &[a, b]);
            let rest = if r.is_empty() { None } else { Some(self.deflet_star(expr, r)?) };
            let mut v = vec![let_t, vec];
            v.extend(rest);
            // `(deflet* nil)` is `nil` (the last def has no body)
            if v.len() == 2 {
                v.push(self.c.push_token(Kind::Nil, Some(f), SymId::NONE, SymId::NONE, F_DERIVED));
            }
            return Some(self.c.push_container(Kind::List, Some(f), &v, F_GEN));
        }
        if r.is_empty() {
            return Some(f);
        }
        let do_t = self.dtok(f, "", "do");
        let rest = self.deflet_star(expr, r)?;
        Some(self.c.push_container(Kind::List, Some(f), &[do_t, f, rest], F_GEN))
    }

    fn dtok(&mut self, like: NodeId, ns: &str, name: &str) -> NodeId {
        let ns = if ns.is_empty() { SymId::NONE } else { intern(ns) };
        self.c.push_token(Kind::Symbol, Some(like), intern(name), ns, F_DERIVED)
    }
    fn dlist(&mut self, like: NodeId, kids: &[NodeId]) -> NodeId {
        self.c.push_container(Kind::List, Some(like), kids, F_DERIVED)
    }
    fn dvec(&mut self, like: NodeId, kids: &[NodeId]) -> NodeId {
        self.c.push_container(Kind::Vector, Some(like), kids, F_DERIVED)
    }

    /// Expand `expr` with the hook `h` (fq hook fn symbol); `None` when unsupported or not applicable.
    pub fn expand_hook(&mut self, h: Name, expr: NodeId) -> Option<NodeId> {
        let kids = self.kids(expr);
        let args = &kids[1..];
        match (h.0.as_str(), h.1.as_str()) {
            ("babashka.fs", "with-temp-dir") => {
                let bv = *args.first()?;
                if self.kind(bv) != Kind::Vector {
                    return None;
                }
                let vk = self.kids(bv);
                let sym = *vk.first()?;
                if self.kind(sym) != Kind::Symbol {
                    return None;
                }
                let let_t = self.dtok(expr, "", "let");
                let nil = self.c.push_token(Kind::Nil, Some(expr), SymId::NONE, SymId::NONE, F_DERIVED);
                let vec = self.dvec(expr, &[sym, nil]);
                let mut v = vec![let_t, vec];
                if let Some(&o) = vk.get(1) {
                    v.push(o);
                }
                v.extend_from_slice(&args[1..]);
                Some(self.dlist(expr, &v))
            }
            ("replicant.defalias", "defalias") => {
                let fname = *args.first()?;
                let forms = &args[1..];
                let defn_t = self.dtok(expr, "", "defn");
                let mut v = vec![defn_t, fname];
                let rest: &[NodeId] = if forms.first().map_or(false, |&f| self.kind(f) == Kind::String) {
                    v.push(forms[0]);
                    &forms[1..]
                } else {
                    let d = self.c.push_token(Kind::String, Some(expr), SymId::NONE, SymId::NONE, F_DERIVED);
                    self.syn_str.insert(d, "no docs".to_owned());
                    v.push(d);
                    forms
                };
                v.extend_from_slice(rest);
                Some(self.dlist(expr, &v))
            }
            ("replicant.defalias", "aliasfn") => {
                let forms = &args[1.min(args.len())..];
                let rest: &[NodeId] = if forms.first().map_or(false, |&f| self.kind(f) == Kind::String) { &forms[1..] } else { forms };
                let fn_t = self.dtok(expr, "", "fn");
                let mut v = vec![fn_t];
                v.extend_from_slice(rest);
                Some(self.dlist(expr, &v))
            }
            ("borkdude.deflet", "deflet") => {
                let r = self.deflet_star(expr, &kids);
                if r.is_none() {
                    // the sci hook throws on a `def` without value: kondo reports `:hook` and analyzes the original call
                    let p = self.pos(expr);
                    self.lint(lint::FType::Hook, Pos { row: p.row, col: p.col, end_row: 0, end_col: 0 }, "java.lang.IndexOutOfBoundsException");
                }
                r
            }
            ("hooks.one-of", "one-of") => {
                // clj-kondo's own hook: `(one-of x [a b])` -> `(case x (a b) x)`
                let matchee = *args.first()?;
                let matches = *args.get(1)?;
                let case_t = self.dtok(expr, "", "case");
                let mk = self.kids(matches);
                let lst = self.c.push_container(Kind::List, Some(matches), &mk, 0);
                Some(self.dlist(expr, &[case_t, matchee, lst, matchee]))
            }
            ("hooks.with-timing", "with-timing") => {
                let bv = *args.first()?;
                let body = &args[1..];
                let mut nb: Vec<NodeId> = Vec::new();
                for b in self.kids(bv) {
                    let fn_t = self.dtok(expr, "", "fn");
                    let av = self.dvec(expr, &[]);
                    let zero = self.c.push_token(Kind::Number, Some(expr), SymId::NONE, SymId::NONE, F_DERIVED | 1);
                    let f = self.dlist(expr, &[fn_t, av, zero]);
                    nb.push(b);
                    nb.push(f);
                }
                let let_t = self.dtok(expr, "", "let");
                let vec = self.dvec(expr, &nb);
                let mut v = vec![let_t, vec];
                v.extend_from_slice(body);
                Some(self.dlist(expr, &v))
            }
            _ => None,
        }
    }
}
