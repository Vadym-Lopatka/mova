//! textDocument/signatureHelp (feature/signature_help.clj).
use super::callh::file_text;
use super::text::Doc;
use super::*;
use crate::cst::{Cst, Kind, NodeId};

/// `(str form)` of a parsed arglist element (top = a bare parameter, strings unquoted).
fn clj_str(c: &Cst, n: NodeId, top: bool) -> String {
    let join = |kids: Vec<NodeId>, sep: &str| kids.iter().map(|k| clj_str(c, *k, false)).collect::<Vec<_>>().join(sep);
    let kids: Vec<NodeId> = c.sig_children(n).collect();
    match c.kind(n) {
        Kind::String if top => c.string_content(n).to_string(),
        Kind::Vector => format!("[{}]", join(kids, " ")),
        Kind::List => format!("({})", join(kids, " ")),
        Kind::Set => format!("#{{{}}}", join(kids, " ")),
        Kind::Map => {
            let pairs: Vec<String> = kids.chunks(2).map(|p| p.iter().map(|k| clj_str(c, *k, false)).collect::<Vec<_>>().join(" ")).collect();
            format!("{{{}}}", pairs.join(", "))
        }
        Kind::Meta => match c.meta(n) {
            Some((_, t)) => clj_str(c, t, top),
            None => c.text(n).to_string(),
        },
        Kind::Quote => match kids.first() {
            Some(k) => format!("(quote {})", clj_str(c, *k, false)),
            None => c.text(n).to_string(),
        },
        _ => c.text(n).to_string(),
    }
}

/// `arglist-str->parameters`: parameter labels.
fn parameters(arglist: &str) -> Vec<String> {
    let cst = crate::reader::parse(arglist);
    let Some(top) = cst.sig_children(cst.root()).next() else { return Vec::new() };
    if cst.kind(top) != Kind::Vector {
        return Vec::new();
    }
    let ps: Vec<NodeId> = cst.sig_children(top).collect();
    let rest = ps.iter().any(|p| cst.kind(*p) == Kind::Symbol && cst.text(*p) == "&");
    let avail: Vec<NodeId> = ps.into_iter().filter(|p| !(cst.kind(*p) == Kind::Symbol && cst.text(*p) == "&")).collect();
    let n = avail.len();
    avail.iter().enumerate().map(|(i, p)| if rest && i + 1 == n { format!("& {}", clj_str(&cst, *p, true)) } else { clj_str(&cst, *p, true) }).collect()
}

fn before_or_at(p: crate::cst::Pos, row: u32, col: u32) -> bool {
    p.row < row || (p.row == row && p.col <= col)
}

pub fn signature_help(q: &Q, at: At) -> String {
    let Some(f) = q.s.id(at.uri) else { return "null".into() };
    let Some(text) = file_text(q, f) else { return "null".into() };
    let doc = Doc::new(&text);
    let (row, col) = (at.row(), at.col());
    let Some(n) = doc.find_at(row, col) else { return "null".into() };
    let Some(func) = doc.func_name_node(n) else { return "null".into() };
    let Some(list) = doc.parent(func) else { return "null".into() };
    let kids = doc.cst.children(list);
    let Some(fi) = kids.iter().position(|k| *k == func) else { return "null".into() };
    let args: Vec<NodeId> = kids[fi + 1..].to_vec();
    let fp = doc.cst.pos(func);
    let Some(def) = q.first_under_cursor(at.uri, fp.row, fp.col).and_then(|e| q.find_definition(e)) else { return "null".into() };
    if def.b != B::VarDef {
        return "null".into();
    }
    let fa = q.fa(def.f);
    let d = &fa.var_definitions[def.i as usize];
    if !d.has_arglists || d.arglists.1 == 0 {
        return "null".into();
    }
    let arglists: Vec<&str> = (0..d.arglists.1).map(|k| fa.strs[(d.arglists.0 + k) as usize].as_str()).collect();
    let name = d.name.as_str();
    let sigs: Vec<(String, Vec<String>)> = arglists.iter().map(|a| (format!("({} {})", name, a), parameters(a))).collect();
    // active signature
    // kondo does not resolve `core/fn` in the cljs pass of cljs/core.cljc: no `:fixed-arities` for such defs
    let fixed_present = d.has_fixed && !(d.fixed.is_empty() && d.lang == L_CLJS);
    let mut arities: Vec<u32> = if fixed_present {
        let mut v: Vec<u32> = d.fixed.iter().collect();
        if v.len() != arglists.len() {
            match v.iter().max() {
                Some(m) => v.push(m + 1),
                None => return super::rename::internal_error(),
            }
        }
        v
    } else {
        vec![arglists.len() as u32]
    };
    arities.sort();
    let nargs = args.len();
    let active_sig = match arities.iter().position(|a| *a as usize == nargs) {
        Some(i) => i,
        None => {
            if nargs >= arities.len() {
                arities.iter().position(|a| a == arities.iter().max().unwrap()).unwrap_or(0)
            } else {
                0
            }
        }
    };
    if active_sig >= sigs.len() {
        return super::rename::internal_error();
    }
    let pcount = sigs[active_sig].1.len() as i64;
    let selected = args.iter().rev().find(|a| before_or_at(doc.cst.pos(**a), row, col));
    let active_param: i64 = match selected {
        Some(s) => {
            let st = doc.cst.text(*s);
            let idx = args.iter().position(|a| doc.cst.text(*a) == st).unwrap_or(0) as i64;
            if idx > pcount - 1 { pcount - 1 } else { idx }
        }
        None => 0,
    };
    let mut out = String::from("{\"signatures\":[");
    for (i, (label, ps)) in sigs.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let pj: Vec<String> = ps.iter().map(|p| format!("{{\"label\":{}}}", json_str(p))).collect();
        let _ = write!(out, "{{\"label\":{},\"parameters\":[{}]", json_str(label), pj.join(","));
        if !d.doc.is_none() {
            let _ = write!(out, ",\"documentation\":{}", json_str(d.doc.as_str()));
        }
        out.push('}');
    }
    let _ = write!(out, "],\"activeParameter\":{},\"activeSignature\":{}}}", active_param, active_sig);
    out
}
