//! create-function (private in the same file, or public in another namespace / new file).
use super::exec::{Ctx, Edit, Out, ResourceChange, Show};
use super::refactors::{of_string, prepend_preserving_comment, An};
use super::rz::*;
use super::tree::{Meta, Tag, Tk};
use super::zops::*;
use crate::engine::index::B;

const THREAD_FIRST: [&str; 2] = ["->", "some->"];
const THREAD_LAST: [&str; 2] = ["->>", "some->>"];

fn param_name(n: Option<&NR>, index: usize) -> String {
    if let Some(n) = n {
        if n.tag == Tag::Token && n.is_sym() {
            let t = &n.text;
            if let Some(rest) = t.strip_prefix('%') {
                if rest.chars().all(|c| c.is_ascii_digit()) {
                    return format!("element{}", rest);
                }
            }
            return t.clone();
        }
    }
    format!("arg{}", index + 1)
}

pub fn create_function(c: &Ctx, z: &Loc) -> Out {
    if z.tag() != Tag::Token {
        return Out::Nil;
    }
    let local = z.node.text.clone();
    let Some(up) = z.up() else { return Out::Nil };
    let fn_sexpr = up.down().map(|d| d.node.text.clone());
    let calling = fn_sexpr.as_deref() == Some(local.as_str());
    let parent_op = if calling { up.up().and_then(|u| u.down()) } else { up.down() };
    let parent_sexpr = parent_op.map(|p| p.node.text.clone()).unwrap_or_default();
    let in_first = THREAD_FIRST.contains(&parent_sexpr.as_str());
    let in_last = THREAD_LAST.contains(&parent_sexpr.as_str());
    let threading = in_first || in_last;
    let fn_call = !calling && !threading;
    let partial = fn_call && z.left().map_or(false, |l| l.node.is_sym() && l.node.text == "partial");
    let qualified = local.find('/').map_or(false, |i| i > 0 && i + 1 < local.len());
    let ns_or_alias = if qualified { local.split_once('/').map(|(n, _)| n.to_string()) } else { None };
    let fn_name = if ns_or_alias.is_some() { local.split_once('/').unwrap().1.to_string() } else { local.clone() };
    let mut args: Vec<Option<NR>> = up.node.kids.iter().filter(|k| !is_ws(k.tag)).skip(1).cloned().map(Some).collect();
    if partial {
        let mut v = vec![None];
        v.extend(args.iter().skip(1).cloned());
        args = v;
    } else if fn_call {
        args = vec![None];
    } else if threading && !calling {
        args = vec![z.left().map(|l| l.node.clone())];
    } else if in_first {
        let mut v = vec![up.left().map(|l| l.node.clone())];
        v.extend(args);
        args = v;
    } else if in_last {
        args.push(up.left().map(|l| l.node.clone()));
    }
    let params: Vec<String> = args.iter().enumerate().map(|(i, a)| param_name(a.as_ref(), i)).collect();
    let private = ns_or_alias.is_none();
    let text = if !private {
        format!("(defn {fn_name})")
    } else if c.settings.use_metadata_for_privacy {
        format!("(defn ^:private {fn_name})")
    } else {
        format!("(defn- {fn_name})")
    };
    let Some(root) = of_string(&text) else { return Out::Nil };
    let params_node = leaf(Tag::Token, Tk::Sym, &format!("[{}]", params.join(" ")));
    let loc = root.append_child_raw(spaces(1)).append_child_raw(params_node);
    let Some(mut acc) = loc.down().and_then(|d| d.right()).and_then(|d| d.right()) else { return Out::Nil };
    for n in [newlines(1), spaces(2)] {
        acc = match acc.insert_right_raw(n).right_raw() {
            Some(a) => a,
            None => return Out::Nil,
        };
    }
    let Some(defn_loc) = to_top(&acc) else { return Out::Nil };
    match ns_or_alias {
        None => {
            let Some(top) = to_top(z) else { return Out::Nil };
            match prepend_preserving_comment(&top, &defn_loc) {
                Some(e) => Out::Seq(vec![Edit { range: e.range, text: e.text }]),
                None => Out::Nil,
            }
        }
        Some(alias) => create_function_for_alias(c, z, &alias, &defn_loc),
    }
}

fn source_path_of(c: &Ctx) -> Option<String> {
    let proj = c.q.s.project.as_ref()?;
    let path = crate::engine::scan::uri_to_path(&c.uri)?;
    let path = std::fs::canonicalize(&path).unwrap_or(path);
    let p = path.to_string_lossy().to_string();
    proj.source_paths.iter().find(|sp| p.starts_with(&format!("{}/", sp.trim_end_matches('/')))).cloned()
}

fn namespace_uri(ns: &str, source_path: &str, file_type: &str) -> String {
    let rel = format!("{}.{}", ns.replace('.', "/").replace('-', "_"), file_type);
    let mut p = std::path::Path::new(source_path).join(rel);
    if p.is_relative() {
        if let Ok(cwd) = std::env::current_dir() {
            p = cwd.join(p);
        }
    }
    crate::engine::scan::path_to_uri(&p)
}

fn create_function_for_alias(c: &Ctx, local: &Loc, alias: &str, defn_loc: &Loc) -> Out {
    let Some(f) = c.q.s.id(&c.uri) else { return Out::Nil };
    let fa = c.q.fa(f);
    let alias_sym = crate::intern::intern(alias);
    let ns_usage = fa.namespace_usages.iter().rposition(|u| u.alias == alias_sym);
    let ns_def = ns_usage.and_then(|i| c.q.find_definition(crate::query::El { f, b: B::NsUsage, i: i as u32 }));
    let source_path = source_path_of(c).unwrap_or_default();
    let file_type = c.uri.rsplit('.').next().unwrap_or("clj").to_string();
    let ns_name = ns_usage.map(|i| fa.namespace_usages[i].to.as_str().to_string());
    let def_uri = match (&ns_def, &ns_name) {
        (Some(d), _) => c.q.uri(d.f).to_string(),
        (None, Some(n)) => namespace_uri(n, &source_path, &file_type),
        (None, None) => namespace_uri(alias, &source_path, &file_type),
    };
    let max = Meta { row: 999999, col: 1, end_row: 999999, end_col: 1 };
    let defn_edits = vec![Edit { range: Some(max), text: defn_loc.string() }, Edit { range: Some(max), text: "\n".to_string() }];
    let show = Some(Show { uri: def_uri.clone(), range: None });
    if ns_def.is_some() {
        return Out::Map { changes: vec![(def_uri, defn_edits)], resources: vec![], show };
    }
    let req = match super::libspec::add_require_suggestion_edits(c, local, alias, Some(alias), None, false) {
        Ok(e) => e,
        Err(o) => return o,
    };
    let min = Meta { row: 1, col: 1, end_row: 1, end_col: 1 };
    let mut def_edits = vec![Edit { range: Some(min), text: format!("(ns {alias})\n") }];
    def_edits.extend(defn_edits);
    Out::Map {
        changes: vec![(c.uri.clone(), req), (def_uri.clone(), def_edits)],
        resources: vec![ResourceChange { uri: def_uri }],
        show,
    }
}

#[allow(dead_code)]
fn _a(_: &An) {}
