//! textDocument/documentSymbol (feature/document_symbol.clj): flat list sorted by selection start.
use super::*;
use std::collections::HashSet;

fn is_by(d: &VarDef, names: &[&str]) -> bool {
    names.contains(&d.defined_by.1.as_str())
}

/// Var definitions as `xf-var-defs` (distinct, record constructors removed).
pub fn doc_var_defs(fa: &FileAnalysis, include_private: bool) -> Vec<usize> {
    let mut seen: HashSet<(u32, u32, u32, u32)> = HashSet::new();
    let mut out = Vec::new();
    for (i, d) in fa.var_definitions.iter().enumerate() {
        if d.name.is_none() || (!include_private && d.private) {
            continue;
        }
        if !seen.insert((d.ns.0, d.name.0, d.pos.row, d.pos.col)) {
            continue;
        }
        let n = d.name.as_str();
        if (is_by(d, &["defrecord"]) && (n.starts_with("->") || n.starts_with("map->"))) || (is_by(d, &["deftype"]) && n.starts_with("->")) {
            continue;
        }
        out.push(i);
    }
    out
}

struct Sym {
    name: String,
    kind: u8,
    range: Pos,
    sel: Pos,
    deprecated: bool,
    detail: Option<&'static str>,
    detail_owned: Option<String>,
}

pub fn document_symbol(q: &Q, uri: &str) -> String {
    let Some(f) = q.s.id(uri) else { return "[]".into() };
    let e = q.entry(f);
    let Some(fa) = e.fa() else { return "[]".into() };
    let mut syms: Vec<Sym> = Vec::new();
    if let Some(n) = fa.namespace_definitions.first() {
        syms.push(Sym { name: n.name.as_str().to_string(), kind: 3, range: n.pos, sel: n.name_pos, deprecated: !n.deprecated.is_none(), detail: None, detail_owned: None });
    }
    for i in doc_var_defs(fa, true) {
        let d = &fa.var_definitions[i];
        let kind = if d.has_fixed || d.varargs_min != NO_ARITY || d.macro_ {
            12
        } else if is_by(d, &["defprotocol", "definterface", "defmulti"]) {
            11
        } else if is_by(d, &["defrecord", "deftype"]) {
            5
        } else {
            13
        };
        syms.push(Sym { name: d.name.as_str().to_string(), kind, range: d.pos, sel: d.name_pos, deprecated: !d.deprecated.is_none(), detail: d.private.then_some("private"), detail_owned: None });
    }
    let mut kseen: HashSet<(u32, u32, u32, u32)> = HashSet::new();
    for k in &fa.keywords {
        if !k.reg.is_none() && kseen.insert((k.ns.0, k.name.0, k.pos.row, k.pos.col)) {
            let reg = k.reg.as_str();
            let detail = reg.rsplit('/').next().unwrap_or(reg);
            // detail is `(name :reg)`; stored as owned string below
            syms.push(Sym { name: k.name.as_str().to_string(), kind: 12, range: k.pos, sel: k.pos, deprecated: false, detail: None, detail_owned: None });
            syms.last_mut().unwrap().detail_owned = Some(detail.to_string());
        }
    }
    let mut seen: HashSet<(u32, u32, u32, u32)> = HashSet::new();
    for u in &fa.var_usages {
        if u.defmethod && !u.derived && !u.derived_name && seen.insert((u.to.0, u.name.0, u.name_pos.row, u.name_pos.col)) {
            let mut name = u.name.as_str().to_string();
            if !u.dispatch_val_str.is_none() {
                name.push(' ');
                name.push_str(u.dispatch_val_str.as_str());
            }
            syms.push(Sym { name, kind: 12, range: u.pos, sel: u.name_pos, deprecated: false, detail: None, detail_owned: None });
        }
    }
    syms.sort_by_key(|s| (s.sel.row, s.sel.col));
    let mut out = String::from("[");
    for (i, s) in syms.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(
            out,
            "{{\"name\":{},\"kind\":{},\"range\":{},\"selectionRange\":{},\"tags\":{}",
            json_str(&s.name),
            s.kind,
            range_json(s.range),
            range_json(s.sel),
            if s.deprecated { "[1]" } else { "[]" }
        );
        if let Some(d) = s.detail {
            let _ = write!(out, ",\"detail\":\"{d}\"");
        } else if let Some(d) = &s.detail_owned {
            let _ = write!(out, ",\"detail\":{}", json_str(d));
        }
        out.push('}');
    }
    out.push(']');
    out
}
