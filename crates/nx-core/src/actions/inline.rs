//! `feature/inline_symbol.clj`: inline a `def` var or a `let` binding.
use super::exec::{Edit, Out};
use super::preds::file_text;
use super::refactors::An;
use super::rz::*;
use super::tree::{Meta, Tag, Tree};
use super::zops::*;
use crate::engine::index::B;
use crate::query::{El, Q};

const IMPLICIT_DO: [&str; 29] = [
    "binding", "comment", "defn", "defn-", "delay", "do", "doseq", "dosync", "dotimes", "fn", "future", "io!", "let", "letfn", "locking", "loop", "sync", "try",
    "when", "when-first", "when-let", "when-not", "when-some", "while", "with-bindings", "with-in-str", "with-local-vars", "with-open", "with-out-str",
];

fn end_of(l: &Loc) -> Option<(u32, u32)> {
    l.meta().map(|m| (m.end_row, m.end_col))
}
fn start_of(l: &Loc) -> Option<(u32, u32)> {
    l.meta().map(|m| (m.row, m.col))
}

struct Data {
    def_el: El,
    def_loc: Loc,
    def_uri: String,
    def_op: String,
}

fn def_root(q: &Q, el: El) -> Option<Loc> {
    let text = file_text(q, el.f)?;
    let tree = Tree::parse(&text);
    if tree.err {
        return None;
    }
    Some(Loc::of_node(from_tree(&tree)))
}

fn inline_data(q: &Q, uri: &str, row: u32, col: u32) -> Option<Data> {
    let d = super::preds::def_from_cursor(q, uri, row, col)?;
    if !matches!(d.b, B::Local | B::VarDef) {
        return None;
    }
    let np = q.name_pos(d);
    let root = def_root(q, d)?;
    let zloc = find_at_pos(&root, np.row, np.col)?;
    let op = find_op(&zloc)?;
    let name = op.node.text.clone();
    if !(op.node.is_sym() && (name == "let" || name == "def")) {
        return None;
    }
    Some(Data { def_el: d, def_loc: zloc, def_uri: q.uri(d.f).to_string(), def_op: name })
}

pub fn can_inline_symbol(q: &Q, uri: &str, row: u32, col: u32) -> bool {
    inline_data(q, uri, row, col).is_some()
}

struct Ref {
    uri: String,
    name: Meta,
    row: u32,
    col: u32,
}

fn refs_of(q: &Q, el: El) -> Vec<Ref> {
    q.find_references(el, false, None)
        .into_iter()
        .map(|e| {
            let np = q.name_pos(e);
            let fp = q.form_pos(e);
            Ref { uri: q.uri(e.f).to_string(), name: Meta { row: np.row, col: np.col, end_row: np.end_row, end_col: np.end_col }, row: fp.row, col: fp.col }
        })
        .collect()
}

type Changes = Vec<(String, Vec<Edit>)>;

fn push(ch: &mut Changes, uri: &str, e: Edit) {
    match ch.iter_mut().find(|(u, _)| u == uri) {
        Some((_, v)) => v.push(e),
        None => ch.push((uri.to_string(), vec![e])),
    }
}

fn delete_between(start: (u32, u32), end: (u32, u32)) -> Edit {
    Edit { range: Some(Meta { row: start.0, col: start.1, end_row: end.0, end_col: end.1 }), text: String::new() }
}

fn ref_replacements(ch: &mut Changes, refs: &[Ref], val: &str) {
    for r in refs {
        push(ch, &r.uri, Edit { range: Some(r.name), text: val.to_string() });
    }
}

fn skip_meta_up(l: Loc) -> Loc {
    let mut c = l;
    while c.up().map_or(false, |u| u.tag() == Tag::Meta) {
        c = c.up().unwrap();
    }
    c
}

pub fn inline_symbol(q: &Q, uri: &str, row: u32, col: u32) -> Out {
    let Some(d) = inline_data(q, uri, row, col) else { return Out::Nil };
    let refs = refs_of(q, d.def_el);
    let mut ch: Changes = Vec::new();
    if d.def_op == "def" {
        let var_loc = skip_meta_up(d.def_loc.clone());
        let Some(def_loc) = var_loc.up() else { return Out::Nil };
        let start = match def_loc.left() {
            Some(p) => end_of(&p),
            None => start_of(&def_loc),
        };
        let (Some(start), Some(end)) = (start, end_of(&def_loc)) else { return Out::Nil };
        let Some(val) = var_loc.rightmost() else { return Out::Nil };
        push(&mut ch, &d.def_uri, delete_between(start, end));
        ref_replacements(&mut ch, &refs, &val.string());
    } else {
        let local = d.def_loc.clone();
        let Some(val) = local.right() else { return Out::Nil };
        if local.leftmost_p() && val.rightmost_p() {
            // sole binding: splice the body into the surrounding context
            let Some(let_form) = local.up().and_then(|u| u.up()) else { return Out::Nil };
            let range = let_form.meta();
            let implicit_do = match let_form.leftmost() {
                Some(lm) => !let_form.leftmost_p() && !is_printable_only(lm.tag()) && lm.node.is_sym() && IMPLICIT_DO.contains(&lm.node.text.as_str()),
                None => false,
            };
            let mut lf = let_form.clone();
            let root_of_file = def_root_from(q, d.def_el);
            // replace-refs: each reference node inside the let form becomes the value
            for r in &refs {
                if r.uri != d.def_uri {
                    continue;
                }
                let v = val.node.clone();
                lf = lf.subedit(|s| match find_at_pos(&s, r.row, r.col) {
                    Some(t) => t.replace(v.clone()),
                    None => s,
                });
            }
            let _ = root_of_file;
            let body = lf.subedit(|s| {
                let l = s.down().unwrap().remove().down().unwrap().remove();
                l
            });
            let one_child = body.down().map_or(false, |c| c.rightmost_p());
            let text = if one_child || implicit_do {
                forms(body.node.kids.clone()).string()
            } else {
                body.insert_child(token_sym("do")).string()
            };
            push(&mut ch, &d.def_uri, Edit { range, text });
        } else {
            let start = match local.left() {
                Some(p) => end_of(&p),
                None => start_of(&local),
            };
            let (Some(start), Some(end)) = (start, end_of(&val)) else { return Out::Nil };
            push(&mut ch, &d.def_uri, delete_between(start, end));
            ref_replacements(&mut ch, &refs, &val.string());
        }
    }
    Out::Map { changes: ch, resources: vec![], show: None }
}

fn def_root_from(_q: &Q, _el: El) -> Option<Loc> {
    None
}

#[allow(dead_code)]
fn _a(_: &An) {}

// ---- inline-function ---------------------------------------------------------------------------------------

fn find_fn_body(name: &Loc) -> Option<Loc> {
    let mut loc = name.right();
    loop {
        let l = loc?;
        if l.tag() == Tag::Token && l.node.tk == super::tree::Tk::Str {
            loc = l.right();
        } else if l.tag() == Tag::Vector {
            return Some(l.right_raw().unwrap_or_else(|| Loc::of_node(forms(vec![]))));
        } else if l.tag() == Tag::List {
            if l.right().is_none() {
                loc = l.down();
            } else {
                return None;
            }
        } else {
            return None;
        }
    }
}

fn combine_ranges(sel: &[Loc]) -> Option<Meta> {
    let ml = sel.last()?.meta()?;
    let mf = sel.first()?.meta()?;
    if let Some(pw) = sel[0].left_raw() {
        let pm = pw.meta()?;
        Some(Meta { row: pm.end_row, col: pm.end_col, end_row: ml.end_row, end_col: ml.end_col })
    } else {
        Some(Meta { row: mf.row, col: mf.col, end_row: ml.end_row, end_col: ml.end_col })
    }
}

pub fn inline_function(q: &Q, uri: &str, root: &Loc, z: &Loc) -> Out {
    let err = || Out::Err("cannot inline function".into(), -32602);
    let Some(defn) = find_ops_up(z, &["defn", "defn-"]) else { return err() };
    let Some(name) = defn.right() else { return err() };
    let Some(nm) = name.meta() else { return err() };
    let Some(def) = super::preds::def_from_cursor(q, uri, nm.end_row, nm.end_col) else { return err() };
    let at = crate::query::At { uri, line: nm.end_row - 1, ch: nm.end_col - 1 };
    let calls = q.references_from_cursor(at, false, false).unwrap_or_default();
    let Some(body) = find_fn_body(&name) else { return err() };
    // arglist
    let Some(arglist) = super::preds::first_arglist(q, def) else { return err() };
    let inner = &arglist[1..arglist.len().saturating_sub(1).max(1)];
    if inner.contains('[') || inner.contains('{') {
        return err();
    }
    let parts: Vec<String> = if inner.trim().is_empty() { vec![] } else { inner.split(' ').map(|s| s.to_string()).collect() };
    let amp = parts.iter().position(|p| p == "&");
    let normal: Vec<String> = match amp {
        Some(i) => parts[..i].to_vec(),
        None => parts.clone(),
    };
    let vararg_param: Option<String> = amp.and_then(|i| parts.get(i + 1)).cloned();
    let Some(f) = q.s.id(uri) else { return err() };
    // formal usages in the body
    let mut sel: Vec<Loc> = vec![body.clone()];
    let mut c = body.right();
    while let Some(x) = c {
        sel.push(x.clone());
        c = x.right();
    }
    let body_range = combine_ranges(&sel);
    let mut formal_refs: Vec<El> = Vec::new();
    if let Some(br) = body_range {
        let an = An { q, f };
        for u in an.local_usages_outside(br) {
            let np = q.name_pos(u);
            let at = crate::query::At { uri, line: np.row - 1, ch: np.col - 1 };
            for r in q.references_from_cursor(at, false, false).unwrap_or_default() {
                if !formal_refs.contains(&r) {
                    formal_refs.push(r);
                }
            }
        }
    }
    let mut edits: Vec<Edit> = Vec::new();
    for call in &calls {
        let cp = q.form_pos(*call);
        let Some(call_loc) = find_at_pos(root, cp.row, cp.col) else { return err() };
        let mut actuals: Vec<Loc> = Vec::new();
        let mut a = call_loc.down().and_then(|d| d.right());
        while let Some(x) = a {
            actuals.push(x.clone());
            a = x.right();
        }
        // formal name -> actual node
        let n_normal = normal.len();
        let mut map: Vec<(String, NR)> = Vec::new();
        for (i, nmn) in normal.iter().enumerate() {
            if let Some(al) = actuals.get(i) {
                map.push((nmn.clone(), al.node.clone()));
            }
        }
        let num_var = actuals.len() as i64 - n_normal as i64;
        let var_actuals: Vec<&Loc> = if num_var > 0 { actuals[actuals.len() - num_var as usize..].iter().collect() } else { vec![] };
        let mut coll = Loc::of_node(forms(vec![vector(vec![])])).down().unwrap();
        for e in var_actuals {
            coll = coll.append_child(e.node.clone());
        }
        if let Some(vp) = &vararg_param {
            map.push((vp.clone(), coll.node.clone()));
        }
        let mut acc = body.clone();
        for fa in &formal_refs {
            let name = q.name(*fa).as_str().to_string();
            let Some((_, val)) = map.iter().find(|(k, _)| *k == name) else { continue };
            let np = q.name_pos(*fa);
            let v = val.clone();
            acc = match acc.edit_path(|l| find_at_pos(&l, np.row, np.col).map(|t| t.replace(v.clone()))) {
                Some(a) => a,
                None => acc,
            };
        }
        // remove-whitespace + interpose newline/indent
        let mut nodes: Vec<NR> = Vec::new();
        let mut c = Some(acc.clone());
        while let Some(x) = c {
            if !is_ws(x.tag()) {
                nodes.push(x.node.clone());
            }
            c = x.right_raw();
        }
        let indent = format!("\n{}", " ".repeat((cp.col as usize).saturating_sub(1)));
        let mut text = String::new();
        for (i, n) in nodes.iter().enumerate() {
            if i > 0 {
                text.push_str(&indent);
            }
            if n.tag == Tag::Comment {
                text.push_str(n.string().trim_end_matches(|c| c == '\n' || c == '\r'));
            } else {
                text.push_str(&n.string());
            }
        }
        edits.push(Edit { range: Some(Meta { row: cp.row, col: cp.col, end_row: cp.end_row, end_col: cp.end_col }), text });
    }
    let dp = q.form_pos(def);
    edits.push(Edit { range: Some(Meta { row: dp.row, col: dp.col, end_row: dp.end_row, end_col: dp.end_col }), text: String::new() });
    Out::Seq(edits)
}
