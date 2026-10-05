//! textDocument/implementation (queries.clj `find-implementations`): protocol / multimethod implementations.
use super::*;
use std::collections::HashSet;

fn by(d: &VarDef, names: &[&str]) -> bool {
    names.contains(&d.defined_by.1.as_str()) || names.contains(&d.defined_by_lint_as.1.as_str())
}

impl<'a> Q<'a> {
    /// `find-implementations` of one element.
    pub fn find_implementations(&self, e: El) -> Vec<El> {
        let fa = self.fa(e.f);
        let langs = self.langs(e);
        let mut out: Vec<El> = Vec::new();
        let mut seen: HashSet<(FileId, u32, u32, u32)> = HashSet::new();
        let mut push = |me: &Self, el: El, out: &mut Vec<El>| {
            if me.langs(el) & langs == 0 {
                return;
            }
            let p = me.form_pos(el);
            if seen.insert((el.f, me.name(el).0, p.row, p.col)) {
                out.push(el);
            }
        };
        match e.b {
            B::VarDef => {
                let d = &fa.var_definitions[e.i as usize];
                let proto = by(d, &["defprotocol", "definterface"]);
                let multi = by(d, &["defmulti"]);
                let method = proto && !d.protocol_name.is_none();
                if !(proto || multi) {
                    return out;
                }
                for f in self.s.ns_and_dependents(d.ns) {
                    let fa = self.fa(f);
                    if method {
                        for (i, p) in fa.protocol_impls.iter().enumerate() {
                            if p.protocol_ns == d.ns && p.method_name == d.name {
                                push(self, El { f, b: B::ProtoImpl, i: i as u32 }, &mut out);
                            }
                        }
                    } else {
                        for (i, u) in fa.var_usages.iter().enumerate() {
                            if u.to == d.ns && u.name == d.name && (proto || u.defmethod) && u.name_pos.row != 0 {
                                push(self, El { f, b: B::VarUsage, i: i as u32 }, &mut out);
                            }
                        }
                    }
                }
            }
            B::VarUsage => {
                let u0 = &fa.var_usages[e.i as usize];
                if u0.to == syms().unknown_ns {
                    return out;
                }
                for f in self.s.ns_and_dependents(u0.to) {
                    let fa = self.fa(f);
                    if !u0.defmethod {
                        for (i, p) in fa.protocol_impls.iter().enumerate() {
                            if p.protocol_ns == u0.to && p.method_name == u0.name {
                                push(self, El { f, b: B::ProtoImpl, i: i as u32 }, &mut out);
                            }
                        }
                    }
                    for (i, u) in fa.var_usages.iter().enumerate() {
                        if u.to == u0.to && u.name == u0.name && u.defmethod && u.name_pos.row != 0 {
                            push(self, El { f, b: B::VarUsage, i: i as u32 }, &mut out);
                        }
                    }
                }
            }
            _ => {}
        }
        out
    }
}

pub fn implementation(q: &Q, at: At) -> String {
    let mut s = String::from("[");
    if let Some(e) = q.first_under_cursor(at.uri, at.row(), at.col()) {
        for (n, h) in q.find_implementations(e).iter().enumerate() {
            if n > 0 {
                s.push(',');
            }
            s.push_str(&q.location(*h));
        }
    }
    s.push(']');
    s
}
