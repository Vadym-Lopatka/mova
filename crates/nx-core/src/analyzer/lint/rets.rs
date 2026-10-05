//! kondo user-fn return tags and deferred conditions (`types/ret-tag-from-call`, `analyze-fn-arity` `:ret`,
//! `linters/lint-deferred-conditions!`): the tag of a defn is the tag of its last body expression; a call of a
//! var of this file (or a local bound to one) is resolved once the whole file is analyzed.
use super::types::*;
use super::*;
use crate::cst::{Kind, NodeId};
use crate::intern::SymId;

/// Tag ids from here on index `LintState::rtab` (binding tags that are not plain keyword tags).
pub const RT_BASE: u16 = 1000;

#[derive(Clone, Debug)]
pub enum Rt {
    Ty(Ty),
    /// Map literal: `open` unless every key is a known literal; values carry their tag when known.
    Map { open: bool, val: Vec<(String, Option<Ty>)> },
    /// Unresolved call of a var of this file; `kw` = keyword lookups applied to the result.
    Call { ns: SymId, name: SymId, arity: u32, kw: Vec<String> },
}

pub struct Deferred {
    pub pos: Pos,
    pub rt: Rt,
    pub nil_test: bool,
    pub lang: u8,
}

/// Per-arity return tag of a defn of this file: (fixed arity, varargs min arity, tag).
pub type FnRet = (Option<u32>, Option<u32>, Option<Rt>);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Verdict {
    True,
    False,
}

/// kondo `types/constant-verdict` over the members of a tag.
pub fn cond_verdict(ks: &[Kw], nil_test: bool) -> Option<Verdict> {
    let any = kw_named("any").0;
    let nil = kw_named("nil").0;
    let boolean = kw_named("boolean");
    let fls = kw_named("false").0;
    let tr = kw_named("truthy").0;
    let tru = kw_named("true").0;
    let falsy = |k: &Kw| !k.1 && (k.0 == nil || k.0 == fls);
    let non_nil = |k: &Kw| !k.1 && (k.0 == tr || (k.0 != any && !match_kw(*k, kw_named("nil"))));
    let truthy = |k: &Kw| !k.1 && (k.0 == tr || k.0 == tru || (non_nil(k) && !match_kw(*k, boolean)));
    let (then, els): (Box<dyn Fn(&Kw) -> bool>, Box<dyn Fn(&Kw) -> bool>) = if nil_test { (Box::new(non_nil), Box::new(move |k| !k.1 && k.0 == nil)) } else { (Box::new(truthy), Box::new(falsy)) };
    if ks.is_empty() {
        return None;
    }
    if ks.iter().all(|k| els(k)) {
        Some(Verdict::False)
    } else if ks.iter().all(|k| then(k)) {
        Some(Verdict::True)
    } else {
        None
    }
}

impl<'a> Analyzer<'a> {
    /// Rt stored in a binding tag code, if any.
    pub fn binding_rt(&self, tag: u16) -> Option<Rt> {
        if tag == 0 {
            return None;
        }
        let id = (tag >> 1) - 1;
        if id >= RT_BASE {
            return self.lt.rtab.get((id - RT_BASE) as usize).cloned();
        }
        None
    }

    /// Tag code of a binding holding `rt` (call / map tags that are not plain keyword tags).
    pub fn rt_tag_code(&mut self, rt: Rt) -> u16 {
        self.lt.rtab.push(rt);
        let i = self.lt.rtab.len() - 1;
        if i + RT_BASE as usize >= 30000 {
            return 0;
        }
        ((RT_BASE + i as u16 + 1) << 1) as u16
    }

    /// Candidate target of a call head: a var of this file.
    fn call_target(&self, head: NodeId) -> Option<(SymId, SymId)> {
        let (ns, name) = (self.c.ns(head), self.c.name(head));
        let cur = self.cur_ns();
        if ns.is_none() {
            if self.find_binding(name).is_some() {
                return None;
            }
            if let Some(&(rns, rname)) = cur.referred.get(&name) {
                return Some((rns, rname));
            }
            if !cur.vars.contains(&name) && crate::analyzer::defs::core_sym(self.is_cljs(), name) {
                return None;
            }
            Some((cur.name, name))
        } else {
            let q = cur.qualify.get(&ns).copied()?;
            Some((q, name))
        }
    }

    /// Tag of an expression including deferred calls (`None` = unknown).
    pub fn rt_of(&mut self, n: NodeId) -> Option<Rt> {
        let n = self.c.unwrap_meta(n);
        if let Some(r) = self.lt.let_rets.get(&n) {
            return r.clone();
        }
        if self.kind(n) != Kind::Map {
            if let Some(t) = self.ty_of(n) {
                return Some(Rt::Ty(t));
            }
        }
        match self.kind(n) {
            Kind::Symbol if self.c.ns(n).is_none() => {
                let b = self.find_binding(self.c.name(n))?;
                self.binding_rt(b.tag)
            }
            Kind::Map => {
                let kids = self.c.children(n).to_vec();
                let mut open = false;
                let mut val: Vec<(String, Option<Ty>)> = Vec::new();
                let mut i = 0;
                while i + 1 < kids.len() {
                    let k = self.c.unwrap_meta(kids[i]);
                    let known = matches!(self.kind(k), Kind::Keyword | Kind::String | Kind::Number | Kind::Char | Kind::True | Kind::False | Kind::Nil);
                    if known && !(self.kind(k) == Kind::Keyword && self.c.flags(k) & crate::cst::F_AUTO != 0) {
                        let key = self.node_str(k);
                        let t = self.ty_of(kids[i + 1]);
                        val.push((key, t));
                    } else {
                        open = true;
                    }
                    i += 2;
                }
                if kids.len() % 2 == 1 || self.c.is_gen(n) {
                    open = true;
                }
                Some(Rt::Map { open, val })
            }
            Kind::List => {
                let head = self.c.nth(n, 0)?;
                let nargs = self.c.children(n).len() as u32 - 1;
                match self.kind(head) {
                    Kind::Symbol => {
                        let (ns, name) = self.call_target(head)?;
                        Some(Rt::Call { ns, name, arity: nargs, kw: Vec::new() })
                    }
                    Kind::Keyword if nargs == 1 && self.c.flags(head) & crate::cst::F_AUTO == 0 => {
                        let key = self.node_str(head);
                        let arg = self.c.nth(n, 1)?;
                        match self.rt_of(arg)? {
                            Rt::Call { ns, name, arity, mut kw } => {
                                kw.push(key);
                                Some(Rt::Call { ns, name, arity, kw })
                            }
                            Rt::Map { open, val } => match val.iter().find(|(k, _)| *k == key) {
                                Some((_, t)) => t.clone().map(Rt::Ty),
                                None if !open => Some(Rt::Ty(Ty::K(kw_named("nil")))),
                                None => None,
                            },
                            Rt::Ty(_) => None,
                        }
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// Record the return tag of a defn arity (`target` = defn name) from its last body expression.
    pub fn record_fn_ret(&mut self, target: SymId, fixed: Option<u32>, varargs_min: Option<u32>, last: Option<NodeId>) {
        let rt = match last {
            Some(l) => {
                self.lt.tail_node = None;
                let lu = self.c.unwrap_meta(l);
                let skip = self.kind(lu) == Kind::List
                    && self.c.nth(lu, 0).map_or(false, |h| {
                        self.kind(h) == Kind::Symbol && matches!(self.c.name(h).as_str(), "if-let" | "when-let" | "if-some" | "when-some" | "loop" | "for" | "doseq" | "binding" | "with-open" | "letfn" | "let*" | "loop*")
                    });
                if skip {
                    None
                } else {
                    self.rt_of(l)
                }
            }
            None => None,
        };
        let key = (self.cur_ns_name(), target);
        self.lt.fn_rets.entry(key).or_default().push((fixed, varargs_min, rt));
    }

    fn ret_for(&self, ns: SymId, name: SymId, arity: u32) -> Option<Rt> {
        let v = self.lt.fn_rets.get(&(ns, name))?;
        if let Some(e) = v.iter().find(|e| e.0 == Some(arity)) {
            return e.2.clone();
        }
        let e = v.iter().find(|e| e.0.is_none() && e.1.map_or(false, |m| arity >= m))?;
        e.2.clone()
    }

    /// kondo `resolve-arg-type` over this file's fn return tags.
    fn resolve_rt(&self, rt: &Rt, seen: &mut Vec<(SymId, SymId, u32)>) -> Option<Ty> {
        match rt {
            Rt::Ty(t) => Some(t.clone()),
            Rt::Map { .. } => Some(Ty::K(kw_named("map"))),
            Rt::Call { ns, name, arity, kw } => {
                let id = (*ns, *name, *arity);
                if seen.contains(&id) {
                    return None;
                }
                seen.push(id);
                let ret = self.ret_for(*ns, *name, *arity)?;
                if kw.is_empty() {
                    return self.resolve_rt(&ret, seen);
                }
                // keyword lookups need the result to be a map
                let mut cur = ret;
                loop {
                    match cur {
                        Rt::Call { ns, name, arity, kw: ref k2 } if k2.is_empty() => {
                            let sid = (ns, name, arity);
                            if seen.contains(&sid) {
                                return None;
                            }
                            seen.push(sid);
                            cur = self.ret_for(ns, name, arity)?;
                        }
                        _ => break,
                    }
                }
                let Rt::Map { open, val } = cur else { return None };
                if kw.len() != 1 {
                    return None;
                }
                match val.iter().find(|(k, _)| *k == kw[0]) {
                    Some((_, t)) => t.clone(),
                    None if !open => Some(Ty::K(kw_named("nil"))),
                    None => None,
                }
            }
        }
    }

    /// kondo `lint-deferred-conditions!`.
    pub fn lint_deferred(&mut self) {
        let list = std::mem::take(&mut self.lt.deferred);
        for d in list {
            let Some(t) = self.resolve_rt(&d.rt, &mut Vec::new()) else { continue };
            let ks = ty_kws_pub(t);
            let saved = self.ltag;
            self.ltag = d.lang;
            match cond_verdict(&ks, d.nil_test) {
                Some(Verdict::True) => {
                    self.lint(FType::ConstantCondition, d.pos, "Condition always true");
                }
                Some(Verdict::False) => {
                    self.lint(FType::ConstantCondition, d.pos, "Condition always false");
                }
                None => {}
            }
            self.ltag = saved;
        }
    }
}
