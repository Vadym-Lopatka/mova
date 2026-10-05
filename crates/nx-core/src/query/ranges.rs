//! textDocument/foldingRange (folding.clj), selectionRange (selection_range.clj), linkedEditingRange (linked_editing_range.clj).
use super::symbols::doc_var_defs;
use super::text::Doc;
use super::*;
use std::collections::HashSet;

/// `find-element-definitions` form extents: first ns def, var defs, keyword defs, defmethod usages.
pub fn folding(q: &Q, uri: &str) -> String {
    let Some(f) = q.s.id(uri) else { return "[]".into() };
    let Some(fa) = q.entry(f).fa() else { return "[]".into() };
    let mut ps: Vec<Pos> = Vec::new();
    if let Some(n) = fa.namespace_definitions.first() {
        ps.push(n.pos);
    }
    ps.extend(doc_var_defs(fa, true).into_iter().map(|i| fa.var_definitions[i].pos));
    let mut seen: HashSet<(u32, u32, u32, u32)> = HashSet::new();
    for k in &fa.keywords {
        if !k.reg.is_none() && seen.insert((k.ns.0, k.name.0, k.pos.row, k.pos.col)) {
            ps.push(k.pos);
        }
    }
    let mut seen: HashSet<(u32, u32, u32, u32)> = HashSet::new();
    for u in &fa.var_usages {
        if u.defmethod && !u.derived && !u.derived_name && seen.insert((u.to.0, u.name.0, u.name_pos.row, u.name_pos.col)) {
            ps.push(u.pos);
        }
    }
    let mut out = String::from("[");
    let mut first = true;
    for p in ps.iter().filter(|p| p.row != 0 && p.col != 0 && p.end_row != 0 && p.end_col != 0) {
        if !first {
            out.push(',');
        }
        first = false;
        let _ = write!(
            out,
            "{{\"startLine\":{},\"startCharacter\":{},\"endLine\":{},\"endCharacter\":{},\"kind\":\"region\"}}",
            p.row - 1,
            p.col - 1,
            p.end_row - 1,
            p.end_col - 1
        );
    }
    out.push(']');
    out
}

fn doc_text(q: &Q, f: FileId) -> Option<String> {
    let e = q.entry(f);
    match e.text() {
        Some(t) => Some(t.to_string()),
        None => crate::engine::scan::uri_to_path(&e.uri).and_then(|p| std::fs::read_to_string(p).ok()),
    }
}

/// Nested `{range, parent}` from the innermost node up; inner ranges (`col+1 .. end-col-1`) between levels; the
/// top-level form is outermost.
pub fn selection(q: &Q, at: At) -> String {
    let Some(f) = q.s.id(at.uri) else { return "[null]".into() };
    let Some(text) = doc_text(q, f) else { return "[null]".into() };
    let doc = Doc::new(&text);
    let Some(n0) = doc.find_at(at.row(), at.col()) else { return "[null]".into() };
    let mut chain: Vec<Pos> = vec![doc.cst.pos(n0)];
    let mut cur = n0;
    while let Some(p) = doc.parent(cur) {
        if p == doc.cst.root() {
            break;
        }
        let pp = doc.cst.pos(p);
        chain.push(Pos { col: pp.col + 1, end_col: pp.end_col.saturating_sub(1), ..pp });
        chain.push(pp);
        cur = p;
    }
    let mut s = String::from("[");
    for r in &chain {
        let _ = write!(s, "{{\"range\":{}", range_json(*r));
        s.push_str(",\"parent\":");
    }
    s.truncate(s.len() - ",\"parent\":".len());
    for _ in &chain {
        s.push('}');
    }
    s.push(']');
    s
}

/// Reference ranges of a namespace alias under the cursor: `{ranges}` or null.
pub fn linked_editing(q: &Q, at: At) -> String {
    let Some(e) = q.first_under_cursor(at.uri, at.row(), at.col()) else { return "null".into() };
    if e.b != B::NsAlias {
        return "null".into();
    }
    let alias_len = q.fa(e.f).namespace_usages[e.i as usize].alias.as_str().encode_utf16().count() as u32;
    let mut s = String::from("{\"ranges\":[");
    for (n, h) in q.find_references(e, true, None).iter().enumerate() {
        if n > 0 {
            s.push(',');
        }
        let p = q.name_pos(*h);
        let r = Pos { row: p.row, col: p.col, end_row: p.row, end_col: p.col + alias_len };
        s.push_str(&range_json(r));
    }
    s.push_str("]}");
    s
}
