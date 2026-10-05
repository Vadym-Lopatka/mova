//! Oracle-schema JSON emitter (see nx/oracle/SCHEMA.md). Only used by the oracle example / tests.
use super::expr::json_str;
use super::types::*;
use crate::cst::Pos;
use crate::intern::SymId;
use std::fmt::Write;

pub(super) struct W<'a> {
    pub(super) s: &'a mut String,
    pub(super) first: bool,
}

impl<'a> W<'a> {
    pub(super) fn key(&mut self, k: &str) {
        if !self.first {
            self.s.push(',');
        }
        self.first = false;
        self.s.push('"');
        self.s.push_str(k);
        self.s.push_str("\":");
    }
    pub(super) fn raw(&mut self, k: &str, v: &str) {
        self.key(k);
        self.s.push_str(v);
    }
    pub(super) fn str_(&mut self, k: &str, v: &str) {
        self.key(k);
        self.s.push_str(&json_str(v));
    }
    pub(super) fn sym(&mut self, k: &str, v: SymId) {
        if !v.is_none() {
            self.str_(k, v.as_str());
        }
    }
    pub(super) fn sym_or_null(&mut self, k: &str, v: SymId) {
        if v.is_none() {
            self.raw(k, "null");
        } else {
            self.str_(k, v.as_str());
        }
    }
    pub(super) fn num(&mut self, k: &str, v: u32) {
        self.key(k);
        let _ = write!(self.s, "{}", v);
    }
    pub(super) fn num_nz(&mut self, k: &str, v: u32) {
        if v != 0 {
            self.num(k, v);
        }
    }
    pub(super) fn val(&mut self, k: &str, v: Val) {
        if !v.is_none() {
            self.raw(k, v.0.as_str());
        }
    }
    pub(super) fn flag(&mut self, k: &str, v: bool) {
        if v {
            self.raw(k, "true");
        }
    }
    pub(super) fn lang(&mut self, l: u8) {
        match l {
            L_CLJ => self.raw("lang", "\"clj\""),
            L_CLJS => self.raw("lang", "\"cljs\""),
            _ => {}
        }
    }
    pub(super) fn pos(&mut self, p: Pos) {
        self.num_nz("row", p.row);
        self.num_nz("col", p.col);
        self.num_nz("end-row", p.end_row);
        self.num_nz("end-col", p.end_col);
    }
    pub(super) fn name_pos(&mut self, p: Pos) {
        self.num_nz("name-row", p.row);
        self.num_nz("name-col", p.col);
        self.num_nz("name-end-row", p.end_row);
        self.num_nz("name-end-col", p.end_col);
    }
}

pub(super) fn obj(out: &mut String, f: impl FnOnce(&mut W)) {
    out.push('{');
    let mut w = W { s: out, first: true };
    f(&mut w);
    out.push('}');
}

pub(super) fn qname(n: (SymId, SymId)) -> String {
    if n.0.is_none() {
        n.1.as_str().to_owned()
    } else {
        format!("{}/{}", n.0.as_str(), n.1.as_str())
    }
}

pub(super) fn bucket<T>(out: &mut String, first: &mut bool, name: &str, items: &[T], mut f: impl FnMut(&mut String, &T)) {
    if items.is_empty() {
        return;
    }
    if !*first {
        out.push(',');
    }
    *first = false;
    let _ = write!(out, "\"{}\":[", name);
    for (i, it) in items.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        f(out, it);
    }
    out.push(']');
}

/// Local ids renumbered as the oracle does: 1-based rank in sorted order of locals.
fn renumber(fa: &FileAnalysis) -> Vec<u32> {
    let mut idx: Vec<usize> = (0..fa.locals.len()).collect();
    idx.sort_by(|&a, &b| {
        let (x, y) = (&fa.locals[a], &fa.locals[b]);
        let kx = (x.pos.row, x.pos.col, x.pos.end_row, x.pos.end_col, x.name.as_str(), x.scope_end_row, x.scope_end_col, x.str_.as_str());
        let ky = (y.pos.row, y.pos.col, y.pos.end_row, y.pos.end_col, y.name.as_str(), y.scope_end_row, y.scope_end_col, y.str_.as_str());
        kx.cmp(&ky)
    });
    let mut map = vec![0u32; fa.next_local_id as usize + 2];
    for (rank, &i) in idx.iter().enumerate() {
        map[fa.locals[i].id as usize] = rank as u32 + 1;
    }
    map
}

/// Full oracle document for one file.
pub fn to_json(file: &str, lang: &str, fa: &FileAnalysis) -> String {
    let mut out = String::with_capacity(4096);
    let _ = write!(out, "{{\"file\":{},\"lang\":{},\"analysis\":{{", json_str(file), json_str(lang));
    let mut first = true;
    let ids = renumber(fa);
    bucket(&mut out, &mut first, "namespace-definitions", &fa.namespace_definitions, |o, n| {
        obj(o, |w| {
            w.pos(n.pos);
            w.name_pos(n.name_pos);
            w.sym("name", n.name);
            w.sym("doc", n.doc);
            w.val("no-doc", n.no_doc);
            w.val("deprecated", n.deprecated);
            w.val("added", n.added);
            w.val("author", n.author);
            w.flag("in-ns", n.in_ns);
            w.lang(n.lang);
        })
    });
    bucket(&mut out, &mut first, "namespace-usages", &fa.namespace_usages, |o, n| {
        obj(o, |w| {
            w.num_nz("row", n.name_pos.row);
            w.num_nz("col", n.name_pos.col);
            w.name_pos(n.name_pos);
            w.sym("from", n.from);
            w.sym("to", n.to);
            w.sym("alias", n.alias);
            for (k, v) in [("alias-row", n.alias_pos.row), ("alias-col", n.alias_pos.col), ("alias-end-row", n.alias_pos.end_row), ("alias-end-col", n.alias_pos.end_col)] {
                if v == 0 {
                    w.raw(k, "null");
                } else {
                    w.num(k, v);
                }
            }
            w.lang(n.lang);
        })
    });
    bucket(&mut out, &mut first, "var-definitions", &fa.var_definitions, |o, d| {
        obj(o, |w| {
            w.pos(d.pos);
            w.name_pos(d.name_pos);
            w.sym("name", d.name);
            w.sym("ns", d.ns);
            if !d.defined_by.1.is_none() {
                w.str_("defined-by", &qname(d.defined_by));
                w.str_("defined-by->lint-as", &qname(d.defined_by_lint_as));
            }
            if fa.has_callstack {
            w.key("callstack");
            w.s.push('[');
            for i in 0..d.cs.1 {
                if i > 0 {
                    w.s.push(',');
                }
                let (ns, nm) = fa.callstacks[(d.cs.0 + i) as usize];
                w.s.push('{');
                let mut w2 = W { s: w.s, first: true };
                w2.sym_or_null("ns", ns);
                w2.sym_or_null("name", nm);
                w.s.push('}');
            }
            w.s.push(']');
            }
            if d.meta.is_none() {
                w.raw("meta", "{}");
            } else {
                w.raw("meta", d.meta.0.as_str());
            }
            w.sym("doc", d.doc);
            if d.has_arglists {
                w.key("arglist-strs");
                w.s.push('[');
                for i in 0..d.arglists.1 {
                    if i > 0 {
                        w.s.push(',');
                    }
                    w.s.push_str(&json_str(fa.strs[(d.arglists.0 + i) as usize].as_str()));
                }
                w.s.push(']');
            }
            if d.has_fixed {
                fixed(w, d.fixed);
            }
            if d.varargs_min != NO_ARITY {
                w.num("varargs-min-arity", d.varargs_min as u32);
            }
            w.flag("private", d.private);
            w.flag("macro", d.macro_);
            w.flag("test", d.test);
            w.val("deprecated", d.deprecated);
            w.val("added", d.added);
            w.val("export", d.export);
            w.sym("protocol-name", d.protocol_name);
            w.sym("protocol-ns", d.protocol_ns);
            w.sym("imported-ns", d.imported.0);
            w.lang(d.lang);
        })
    });
    bucket(&mut out, &mut first, "var-usages", &fa.var_usages[..fa.var_usages.iter().rposition(|u| !u.synth).map_or(0, |p| p + 1)], |o, u| {
        obj(o, |w| {
            w.pos(u.pos);
            w.name_pos(u.name_pos);
            w.sym("name", u.name);
            w.sym("from", u.from);
            w.sym("from-var", u.from_var);
            w.sym("to", u.to);
            if u.arity != NO_ARITY {
                w.num("arity", u.arity as u32);
            }
            w.sym("alias", u.alias);
            w.flag("refer", u.refer);
            w.flag("defmethod", u.defmethod);
            w.flag("derived-location", u.derived);
            w.flag("derived-name-location", u.derived_name);
            w.sym("dispatch-val-str", u.dispatch_val_str);
            if u.ctx_testing.is_none() {
                w.raw("context", "{}");
            } else {
                w.key("context");
                let _ = write!(w.s, "{{\"clojure.test\":{{\"testing-str\":{}}}}}", u.ctx_testing.as_str());
            }
            if u.has_fixed {
                fixed(w, u.fixed);
            }
            if u.varargs_min != NO_ARITY {
                w.num("varargs-min-arity", u.varargs_min as u32);
            }
            w.flag("macro", u.macro_);
            w.flag("private", u.private);
            w.val("deprecated", u.deprecated);
            w.lang(u.lang);
        })
    });
    bucket(&mut out, &mut first, "locals", &fa.locals, |o, l| {
        obj(o, |w| {
            w.num("id", ids[l.id as usize]);
            w.sym("name", l.name);
            w.sym_or_null("str", l.str_);
            w.pos(l.pos);
            if l.scope_end_row == u32::MAX {
                w.raw("scope-end-row", "null");
                w.raw("scope-end-col", "null");
            } else {
                w.num_nz("scope-end-row", l.scope_end_row);
                w.num_nz("scope-end-col", l.scope_end_col);
            }
            w.raw("derived-location", "null");
            w.lang(l.lang);
        })
    });
    bucket(&mut out, &mut first, "local-usages", &fa.local_usages, |o, l| {
        obj(o, |w| {
            if l.id != 0 {
                w.num("id", ids.get(l.id as usize).copied().unwrap_or(0));
            }
            w.sym("name", l.name);
            w.pos(l.pos);
            w.name_pos(l.name_pos);
            w.lang(l.lang);
        })
    });
    super::extras_emit::emit(&mut out, &mut first, file, fa);
    out.push_str("},\"findings\":[");
    super::lint::emit_findings(&mut out, fa);
    out.push_str("]}");
    out
}

fn fixed(w: &mut W, a: Arities) {
    // the oracle sorts set elements by their printed form ("11" < "3")
    let mut v: Vec<String> = a.iter().map(|n| n.to_string()).collect();
    v.sort();
    w.key("fixed-arities");
    w.s.push('[');
    w.s.push_str(&v.join(","));
    w.s.push(']');
}
