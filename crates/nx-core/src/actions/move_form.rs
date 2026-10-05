//! `feature/move_form.clj`: move a top-level def to another namespace, rewriting requires and usages in every file.
use super::clean_ns::CleanSettings;
use super::exec::{Ctx, Edit, Out};
use super::libspec::{add_to_namespace_loc, cleaning_ns_edits, find_namespace, replace_range, Libspec};
use super::preds::file_text;
use super::rz::*;
use super::tree::{Meta, Tag, Tree};
use super::zops::*;
use crate::analyzer::types::VarUsage;
use crate::engine::store::FileId;
use crate::intern::SymId;
use crate::query::Q;
use std::collections::HashSet;

fn root_of(text: &str) -> Option<Loc> {
    let tree = Tree::parse(text);
    if tree.err {
        return None;
    }
    Some(Loc::of_node(from_tree(&tree)))
}

fn ns_names(q: &Q, f: FileId) -> Vec<SymId> {
    let mut v: Vec<SymId> = vec![];
    if let Some(fa) = q.entry(f).fa() {
        for n in &fa.namespace_definitions {
            if !v.contains(&n.name) {
                v.push(n.name);
            }
        }
    }
    v
}

fn name_in(m: Meta, u: &VarUsage) -> bool {
    // edit/loc-encapsulates-usage? (in-range? of the usage name range)
    let p = u.name_pos;
    in_range_meta(m, Meta { row: p.row, col: p.col, end_row: p.end_row, end_col: p.end_col })
}

fn inside_scope(m: Meta, u: &VarUsage) -> bool {
    // shared/inside? usage form-scope (name start within the form)
    let (r, c) = (u.name_pos.row, u.name_pos.col);
    (m.row < r || (m.row == r && m.col <= c)) && (r < m.end_row || (r == m.end_row && c <= m.end_col))
}

/// `edit/find-at-element-name` in a root loc.
fn at_name(root: &Loc, row: u32, col: u32) -> Option<Loc> {
    find_at_pos(root, row, col)
}

fn drop_ns(form: &NR, usages: &[&VarUsage]) -> NR {
    let mut loc = Loc::of_node(form.clone());
    for u in usages {
        let Some(z) = at_name(&loc, u.name_pos.row, u.name_pos.col) else { continue };
        let t = z.node.text.clone();
        let plain = t.rsplit_once('/').map_or(t.clone(), |x| x.1.to_string());
        let sym = token_sym(&plain);
        loc = loc.subedit_at(&z, sym);
    }
    loc.root()
}

fn range_with_left(z: &Loc) -> Option<Meta> {
    let this = z.meta()?;
    match z.left().and_then(|l| l.meta()) {
        Some(l) => Some(Meta { row: l.end_row, col: l.end_col, end_row: this.end_row, end_col: this.end_col }),
        None => Some(this),
    }
}

pub fn move_form(c: &Ctx) -> Out {
    let (Some(zloc), Some(root)) = (c.loc.as_ref(), c.root.as_ref()) else { return Out::Nil };
    let q = c.q;
    let Some(dest_arg) = c.args.first().and_then(|a| a.as_str()).map(|s| s.to_string()) else { return Out::Err("Internal error".into(), -32603) };
    let Some(proj) = q.s.project.as_ref() else { return Out::Nil };
    let abs = if dest_arg.starts_with('/') { std::path::PathBuf::from(&dest_arg) } else { proj.root.join(&dest_arg) };
    let dest_uri = crate::engine::scan::path_to_uri(&abs);
    let Some(src_f) = q.s.id(&c.uri) else { return Out::Nil };
    let Some(dest_f) = q.s.id(&dest_uri) else { return Out::Nil };
    let (src_nses, dest_nses) = (ns_names(q, src_f), ns_names(q, dest_f));
    if src_nses.len() != 1 || dest_nses.len() != 1 {
        return Out::Nil;
    }
    let (source_ns, dest_ns) = (src_nses[0], dest_nses[0]);
    let Some(zm) = zloc.meta() else { return Out::Nil };
    let fa = q.fa(src_f);
    let inner: Vec<&VarUsage> = fa.var_usages.iter().filter(|u| inside_scope(zm, u)).filter(|u| name_in(zm, u)).collect();
    // usages of the form itself (encapsulated); the JVM filters by scope first, then by encapsulation
    if inner.iter().any(|u| u.to == source_ns) {
        return Out::Nil;
    }
    let defs: Vec<usize> = super::super::query::symbols::doc_var_defs(fa, false)
        .into_iter()
        .filter(|&i| {
            let p = fa.var_definitions[i].name_pos;
            in_range_meta(zm, Meta { row: p.row, col: p.col, end_row: p.end_row, end_col: p.end_col })
        })
        .collect();
    let form_loc = match to_top(zloc) {
        Some(t) => t,
        None => return Out::Nil,
    };
    if !(form_loc.node.meta == zloc.node.meta && is_top(zloc)) || defs.len() != 1 {
        return Out::Nil;
    }
    let di = defs[0];
    let def_name = fa.var_definitions[di].name;
    let def_el = crate::query::El { f: src_f, b: crate::engine::index::B::VarDef, i: di as u32 };
    let refs = q.find_references(def_el, false, None);
    let dest_refs: Vec<_> = refs.iter().filter(|e| e.f == dest_f).collect();
    // destination document: insert after its last top-level form
    let Some(dest_text) = file_text(q, dest_f) else { return Out::Nil };
    let Some(dest_root) = root_of(&dest_text) else { return Out::Err("Internal error".into(), -32603) };
    let Some(ins) = dest_root.down_raw().and_then(|d| d.rightmost()).and_then(|l| l.meta()) else { return Out::Err("Internal error".into(), -32603) };
    let mut seen_u: HashSet<(u32, u32, u32)> = HashSet::new();
    let dest_inner: Vec<&VarUsage> = inner.iter().copied().filter(|u| u.to == dest_ns).filter(|u| seen_u.insert((u.name.0, u.pos.row, u.pos.col))).collect();
    let at_end = Meta { row: ins.end_row, col: ins.end_col, end_row: ins.end_row, end_col: ins.end_col };
    let moved = drop_ns(&form_loc.node, &dest_inner);
    let mut changes: Vec<(String, Vec<Edit>)> = vec![(dest_uri.clone(), vec![Edit { range: Some(at_end), text: "\n\n".into() }, Edit { range: Some(at_end), text: moved.string() }])];

    // references inside the destination: the JVM takes `(meta loc)` of a zipper loc as range, which comes out as 0:0
    for e in &dest_refs {
        let p = q.name_pos(**e);
        if let Some(z) = at_name(&dest_root, p.row, p.col) {
            let t = z.node.text.clone();
            let plain = t.rsplit_once('/').map_or(t.clone(), |x| x.1.to_string());
            changes[0].1.push(Edit { range: Some(Meta { row: 1, col: 1, end_row: 1, end_col: 1 }), text: plain });
        }
    }
    let st = CleanSettings::from_json(c.init.as_ref());
    // files with references, in first-seen order, minus the destination
    let mut files: Vec<FileId> = vec![];
    for e in &refs {
        if e.f != dest_f && !files.contains(&e.f) {
            files.push(e.f);
        }
    }
    let src_text = c.text.clone().unwrap_or_else(|| std::sync::Arc::from(""));
    let pairs = super::sugg::alias_ns_pairs(q, &c.uri);
    for f in files {
        let uri = q.uri(f).to_string();
        let Some(text) = file_text(q, f) else { continue };
        let Some(file_loc) = root_of(&text) else { continue };
        let lfa = q.fa(f);
        let source_refer = lfa.var_usages.iter().find(|u| u.refer && u.to == source_ns && u.name == def_name);
        let dest_require = lfa.namespace_usages.iter().find(|n| n.to == dest_ns);
        let suggestion_alias: Option<String> = match dest_require {
            Some(r) => Some(if r.alias.is_none() { String::new() } else { r.alias.as_str().to_string() }),
            None => super::sugg::namespace_suggestions(dest_ns.as_str(), &pairs).into_iter().next().and_then(|s| s.alias),
        };
        let usages: Vec<&VarUsage> = {
            let mut seen: HashSet<(u32, u32, u32)> = HashSet::new();
            lfa.var_usages.iter().filter(|u| !u.refer && u.to == source_ns && u.name == def_name).filter(|u| seen.insert((u.name.0, u.pos.row, u.pos.col))).collect()
        };
        let libspec = Libspec {
            ty: ":require",
            lib: dest_ns.as_str().to_string(),
            lib_is_string: false,
            refer: source_refer.map(|u| u.name.as_str().to_string()),
            alias: suggestion_alias.clone(),
            class: None,
        };
        // determine-ns-edits
        let other_refers = lfa.var_usages.iter().filter(|u| u.refer && u.to == source_ns && u.name != def_name).count();
        let other_usages = lfa.var_usages.iter().filter(|u| !u.refer && u.alias.is_none() && u.to == source_ns && u.name != def_name).count();
        let source_require = lfa.namespace_usages.iter().find(|n| n.to == source_ns);
        let remove_source_require = source_require.is_some() && other_usages == 0;
        let apply = |ns_loc: Loc| -> Loc {
            let mut n = ns_loc;
            if remove_source_require {
                if let Some(sr) = source_require {
                    n = n.subedit(|z| match find_at_pos(&z, sr.name_pos.row, sr.name_pos.col).and_then(|l| l.up()) {
                        Some(u) => Loc::of_node(u.remove().root()),
                        None => z,
                    });
                }
            } else if let Some(sf) = source_refer {
                n = n.subedit(|z| match find_at_pos(&z, sf.name_pos.row, sf.name_pos.col) {
                    Some(l) => {
                        let mut r = l.remove();
                        if other_refers == 0 {
                            r = r.remove().remove();
                        }
                        Loc::of_node(r.root())
                    }
                    None => z,
                });
            }
            n
        };
        let edits0: Vec<Edit> = match add_to_namespace_loc(&file_loc, &libspec, &st) {
            Some((range, res)) => vec![Edit { range, text: apply(res).string() }],
            None => {
                if remove_source_require || source_refer.is_some() {
                    match find_namespace(&file_loc) {
                        Some(ns) => vec![Edit { range: ns.meta(), text: apply(ns.clone()).string() }],
                        None => vec![],
                    }
                } else {
                    vec![]
                }
            }
        };
        let t2 = text.clone();
        let ns_changes = cleaning_ns_edits(c, edits0, &st, &|e| replace_range(&t2, e));
        let repl_ns: String = match &suggestion_alias {
            Some(a) => a.clone(),
            None => dest_ns.as_str().to_string(),
        };
        let mut usage_changes: Vec<Edit> = vec![];
        for u in usages {
            let Some(z) = at_name(&file_loc, u.name_pos.row, u.name_pos.col) else { continue };
            if uri == c.uri || z.node.text.contains('/') {
                usage_changes.push(Edit { range: z.meta(), text: format!("{}/{}", repl_ns, def_name.as_str()) });
            }
        }
        let mut all = ns_changes;
        all.extend(usage_changes);
        changes.push((uri, all));
    }
    let removal = Edit { range: range_with_left(zloc), text: String::new() };
    match changes.iter_mut().find(|(u, _)| *u == c.uri) {
        Some((_, v)) => v.push(removal),
        None => changes.push((c.uri.clone(), vec![removal])),
    }
    let _ = (root, src_text, Tag::Forms);
    Out::Map { changes, resources: vec![], show: None }
}
