//! `feature/add_missing_libspec.clj`: add requires / imports to the ns form, swap namespace with alias.
use super::clean_ns::{clean_ns_edits, CleanSettings};
use super::exec::{ask, Ctx, Edit, Out};
use super::refactors::{of_string, node_of_string};
use super::rz::*;
use super::tree::{Meta, Tag, Tk, Tree};
use super::zops::*;

/// `z/find-value loc z/next v`: token (symbol/keyword/string) whose text is `v`.
pub fn find_value_next(l: &Loc, v: &str) -> Option<Loc> {
    l.find(&|x| Some(x.next()), &|x| x.tag() == Tag::Token && x.node.text == v)
}

pub(crate) fn find_namespace(z: &Loc) -> Option<Loc> {
    let mut c = z.clone();
    while let Some(u) = c.up_raw() {
        c = u;
    }
    find_value_next(&c, "ns")?.up()
}

#[derive(Clone, Debug, Default)]
pub struct Libspec {
    pub ty: &'static str, // ":require" | ":import"
    pub lib: String,      // printed form (symbols as text, JS requires quoted)
    pub lib_is_string: bool,
    pub refer: Option<String>,
    pub alias: Option<String>,
    pub class: Option<String>,
}

fn need_to_add(zloc: &Loc, l: &Libspec) -> bool {
    (l.class.as_ref().map_or(false, |c| find_value_next(zloc, c).is_none()))
        || find_value_next(zloc, &l.lib).is_none()
        || (l.refer.as_ref().map_or(false, |c| find_value_next(zloc, c).is_none()))
        || (l.alias.as_ref().map_or(false, |c| find_value_next(zloc, c).is_none()))
}

fn find_when_starts_with_sym(zloc: &Loc, sym: &str) -> Option<Loc> {
    let start = zloc.next();
    if start.is_end() {
        return None;
    }
    start.find(&|x| Some(x.next()), &|x| {
        x.tag() == Tag::Token && {
            if x.up().map_or(false, |u| u.tag() == Tag::Vector) { x.node.text == sym } else { x.node.text.starts_with(sym) }
        }
    })
}

fn sexpr_vec(items: &[String]) -> NR {
    let mut kids = Vec::new();
    for (i, s) in items.iter().enumerate() {
        if i > 0 {
            kids.push(spaces(1));
        }
        kids.push(node_of_string(s).unwrap_or_else(|| token_sym(s)));
    }
    inner(Tag::Vector, kids)
}

struct Modified {
    existing_import_package_by_full_package: Option<Loc>,
    result: Option<Loc>,
}

fn modified_existing_libform(ns_zip: &Loc, form_type_loc: Option<&Loc>, l: &Libspec) -> Modified {
    let unwrapped = if l.refer.is_some() { find_value_next(ns_zip, &l.lib) } else { None };
    let wrapped = unwrapped.as_ref().and_then(|u| u.up()).filter(|w| w.tag() == Tag::Vector);
    let existing_refer = wrapped.as_ref().and_then(|w| find_value_next(&w.subzip(), ":refer"));
    let pkg_only = if l.class.is_some() { form_type_loc.and_then(|f| f.up()).and_then(|u| find_value_next(&u, &l.lib)) } else { None };
    let by_segment = pkg_only.as_ref().filter(|p| p.prev().map_or(false, |pv| matches!(pv.tag(), Tag::List | Tag::Vector))).and_then(|p| p.up());
    let by_full = if l.class.is_some() && by_segment.is_none() { find_when_starts_with_sym(ns_zip, &l.lib) } else { None };
    let by_single_full = by_full.as_ref().filter(|f| find_when_starts_with_sym(f, &l.lib).is_none()).cloned();
    let refer = l.refer.clone();
    let result = (|| -> Option<Loc> {
        if existing_refer.is_some() {
            let r = ns_zip.subedit(|s| {
                let l2 = find_value_next(&s, &l.lib).and_then(|x| find_value_next(&x, ":refer")).and_then(|x| x.right());
                match l2 {
                    Some(v) => v.append_child_raw(spaces(1)).append_child(token_sym(refer.as_deref().unwrap_or(""))),
                    None => s,
                }
            });
            return Some(r);
        }
        if wrapped.is_some() {
            return Some(ns_zip.subedit(|s| {
                match find_value_next(&s, &l.lib).and_then(|x| x.up()) {
                    Some(v) => v.append_child_raw(spaces(1)).append_child(keyword(":refer")).append_child(sexpr_vec(&[refer.clone().unwrap_or_default()])),
                    None => s,
                }
            }));
        }
        if unwrapped.is_some() {
            return Some(ns_zip.subedit(|s| {
                match find_value_next(&s, &l.lib) {
                    Some(v) => {
                        // paredit/wrap-around :vector
                        let w = paredit_wrap_around(&v, Tag::Vector);
                        match w.up() {
                            Some(u) => u.append_child_raw(spaces(1)).append_child(keyword(":refer")).append_child(sexpr_vec(&[refer.clone().unwrap_or_default()])),
                            None => s,
                        }
                    }
                    None => s,
                }
            }));
        }
        if by_segment.is_some() {
            return Some(ns_zip.subedit(|s| match find_value_next(&s, &l.lib).and_then(|x| x.up()) {
                Some(u) => u.append_child_raw(spaces(1)).append_child(token_sym(l.class.as_deref().unwrap_or(""))),
                None => s,
            }));
        }
        if let Some(single) = &by_single_full {
            let existing = single.string();
            let existing_class = existing.rsplit('.').next().unwrap_or("").to_string();
            let target = single.node.clone();
            return Some(ns_zip.subedit(|s| {
                let found = s.next().find(&|x| Some(x.next()), &|x| std::rc::Rc::ptr_eq(&x.node, &target) || (x.node.text == target.text && x.tag() == Tag::Token && x.node.meta == target.meta));
                match found {
                    Some(f) => f.replace(sexpr_vec(&[l.lib.clone(), existing_class.clone(), l.class.clone().unwrap_or_default()])),
                    None => s,
                }
            }));
        }
        None
    })();
    Modified { existing_import_package_by_full_package: by_full, result }
}

/// `rewrite-clj.paredit/wrap-around`.
fn paredit_wrap_around(z: &Loc, tag: Tag) -> Loc {
    let node = z.node.clone();
    let l = z.insert_left(inner(tag, vec![])).left().unwrap();
    // remove-right-while whitespace, then remove-right
    let mut l = l;
    loop {
        match l.right_raw() {
            Some(r) if is_ws(r.tag()) => l = remove_right_one(&l),
            _ => break,
        }
    }
    l = remove_right_one(&l);
    l.append_child_raw(node).down().unwrap()
}

fn remove_right_one(l: &Loc) -> Loc {
    let p = l.path.as_ref().unwrap();
    if p.r.is_empty() {
        return l.clone();
    }
    Loc {
        node: l.node.clone(),
        path: Some(std::rc::Rc::new(Path { l: p.l.clone(), r: p.r[1..].to_vec(), ppath: p.ppath.clone(), pnode: p.pnode.clone(), changed: true })),
        end: false,
    }
}

pub struct SettingsView<'a> {
    pub inner_indent: super::clean_ns::Indent,
    pub _p: std::marker::PhantomData<&'a ()>,
}

/// `add-to-namespace*` (existing ns form only). Returns edits.
pub fn add_to_namespace(zloc: &Loc, l: &Libspec, st: &CleanSettings) -> Option<Vec<Edit>> {
    let (range, res_loc) = add_to_namespace_loc(zloc, l, st)?;
    Some(vec![Edit { range, text: res_loc.string() }])
}

/// `add-to-namespace*` keeping the changed ns loc (range of the original ns form + loc at the new form).
pub fn add_to_namespace_loc(zloc: &Loc, l: &Libspec, st: &CleanSettings) -> Option<(Option<Meta>, Loc)> {
    let existing = find_namespace(zloc)?;
    let ns_zip = existing.subzip();
    if !need_to_add(&ns_zip, l) {
        return None;
    }
    let add_form_type = find_value_next(&ns_zip, l.ty).is_none();
    let form_type_loc = find_value_next(&existing.subzip(), l.ty);
    let same_line = st.inner_indent == super::clean_ns::Indent::SameLine;
    let col: u32 = match &form_type_loc {
        Some(f) => f.rightmost().and_then(|r| r.meta()).map(|m| m.col)?,
        None => if same_line { 2 } else { 5 },
    };
    let m = modified_existing_libform(&ns_zip, form_type_loc.as_ref(), l);
    let form_to_add: NR = if l.ty == ":import" && m.existing_import_package_by_full_package.is_some() {
        token_sym(&format!("{}.{}", l.lib, l.class.clone().unwrap_or_default()))
    } else if l.ty == ":import" {
        sexpr_vec(&[l.lib.clone(), l.class.clone().unwrap_or_default()])
    } else {
        let mut items: Vec<String> = vec![l.lib.clone()];
        if let Some(a) = &l.alias {
            items.push(":as".into());
            items.push(a.clone());
        }
        if let Some(r) = &l.refer {
            items.push(":refer".into());
            items.push(format!("[{}]", r));
        }
        sexpr_vec(&items)
    };
    let result = match m.result {
        Some(r) => r,
        None => ns_zip.subedit(|s| {
            let mut z = s;
            if add_form_type {
                z = z.append_child(newlines(1));
                z = z.append_child(spaces(2));
                z = z.append_child(list(vec![keyword(l.ty)]));
            }
            let Some(f) = find_value_next(&z, l.ty).and_then(|x| x.up()) else { return z };
            let mut f = f;
            if !add_form_type || !same_line {
                f = f.append_child_raw(newlines(1));
            }
            f = f.append_child_raw(spaces(col as usize - 1));
            f.append_child(form_to_add.clone())
        }),
    };
    // the subedit replaced the ns node inside `existing`'s zipper: `result` sits at the ns position
    let res_loc = existing.replace(result.node.clone());
    Some((existing.meta(), res_loc))
}

pub(crate) fn cleaning_ns_edits(c: &Ctx, edits: Vec<Edit>, settings: &CleanSettings, full_doc_after: &dyn Fn(&Edit) -> String) -> Vec<Edit> {
    let auto = settings.auto_after_ns_refactor;
    if !auto {
        return edits;
    }
    let mut out = Vec::new();
    for e in edits {
        if e.text.contains("(ns") || find_value_in_text(&e.text, "ns") {
            let full = full_doc_after(&e);
            let tree = Tree::parse(&full);
            if !tree.err {
                let root = Loc::of_node(from_tree(&tree));
                if let Some((fnd, aliases)) = super::exec::clean_ns_findings_pub(c) {
                    if let Some(mut v) = clean_ns_edits(&root, settings, fnd, &|a| aliases.iter().any(|x| x == a)) {
                        if !v.is_empty() {
                            let f = v.remove(0);
                            out.push(Edit { range: e.range, text: f.text });
                            continue;
                        }
                    }
                }
            }
        }
        out.push(e);
    }
    out
}

fn find_value_in_text(text: &str, v: &str) -> bool {
    let Some(l) = of_string(text) else { return false };
    let top = {
        let mut c = l;
        while let Some(u) = c.up_raw() {
            c = u;
        }
        c
    };
    find_value_next(&top, v).is_some()
}

pub(crate) fn replace_range(text: &str, e: &Edit) -> String {
    // apply a single edit (1-based row/col, UTF-16 cols) to `text`
    let Some(m) = e.range else { return text.to_string() };
    let off = |row: u32, col: u32| -> usize {
        let mut r = 1u32;
        let mut i = 0usize;
        let b = text.as_bytes();
        while r < row && i < b.len() {
            if b[i] == b'\n' {
                r += 1;
            }
            i += 1;
        }
        let mut c = 1u32;
        let mut j = i;
        for ch in text[i..].chars() {
            if c >= col || ch == '\n' {
                break;
            }
            c += ch.len_utf16() as u32;
            j += ch.len_utf8();
        }
        j
    };
    let (s, en) = (off(m.row, m.col), off(m.end_row, m.end_col));
    format!("{}{}{}", &text[..s], e.text, &text[en..])
}

fn ask_rcf(c: &Ctx, n: usize, mode: &str) -> Result<bool, Out> {
    match c.init_setting(&["add-missing", "add-to-rcf"]).as_deref() {
        Some("always") | Some(":always") => return Ok(true),
        Some("never") | Some(":never") => return Ok(false),
        _ => {}
    }
    match ask(c, n, &format!("Add {mode} inside this comment form?"), &["Yes", "No"])? {
        Some(a) if a == "Yes" => Ok(true),
        _ => Ok(false),
    }
}

/// `add-require-suggestion` / add-known-alias / add-known-refer / add-simple-require.
pub fn add_require_suggestion(c: &Ctx, z: &Loc, chosen_ns: &str, alias: Option<&str>, refer: Option<&str>, js_require: bool) -> Out {
    match add_require_suggestion_edits(c, z, chosen_ns, alias, refer, js_require) {
        Ok(e) if e.is_empty() => Out::Nil,
        Ok(e) => Out::Seq(e),
        Err(o) => o,
    }
}

pub fn add_require_suggestion_edits(c: &Ctx, z: &Loc, chosen_ns: &str, alias: Option<&str>, refer: Option<&str>, js_require: bool) -> Result<Vec<Edit>, Out> {
    let st = CleanSettings::from_json(c.init.as_ref());
    let Some(cursor_sym) = safe_sym(z) else { return Ok(vec![]) };
    let cursor_ns = cursor_sym.split_once('/').map(|(n, _)| n.to_string());
    let alias_or_ns: Option<String> = if refer.is_none() { Some(alias.map(|s| s.to_string()).unwrap_or_else(|| chosen_ns.to_string())) } else { None };
    let in_rcf = find_ops_up(z, &["comment"]).is_some();
    let to_ns;
    let rcf_loc;
    if in_rcf {
        match ask_rcf(c, 0, "require") {
            Ok(true) => {
                rcf_loc = find_ops_up(z, &["comment"]);
                to_ns = false;
            }
            Ok(false) => {
                rcf_loc = None;
                to_ns = true;
            }
            Err(o) => return Err(o),
        }
    } else {
        rcf_loc = None;
        to_ns = true;
    }
    let target = rcf_loc.clone().unwrap_or_else(|| z.clone());
    let lib_text = if js_require { format!("\"{}\"", chosen_ns) } else { chosen_ns.to_string() };
    let libspec = Libspec { ty: ":require", lib: lib_text, lib_is_string: js_require, refer: refer.map(|s| s.to_string()), alias: if refer.is_some() { None } else { alias.map(|s| s.to_string()) }, class: None };
    let edits: Option<Vec<Edit>> = if target.tag() == Tag::Token && target.node.text == "comment" { add_to_rcf(&target, &libspec) } else { add_to_namespace(&target, &libspec, &st) };
    let mut edits = match edits {
        Some(e) => e,
        None => Vec::new(),
    };
    if to_ns {
        let text = c.text.clone();
        let t2 = text.clone();
        edits = cleaning_ns_edits(c, edits, &st, &|e| replace_range(t2.as_deref().unwrap_or(""), e));
    }
    if let Some(a) = &alias_or_ns {
        if let Some(cn) = &cursor_ns {
            let start = if to_ns { find_namespace(&target).map(|n| n.next()) } else { target.up().map(|u| u.subzip()) };
            if let Some(start) = start {
                let mut cur = Some(start);
                while let Some(x) = cur {
                    if x.is_end() {
                        break;
                    }
                    if let Some(s) = safe_sym(&x) {
                        if let Some((sns, name)) = s.split_once('/') {
                            if (chosen_ns == sns || cn == sns) && a != sns {
                                edits.push(Edit { range: x.meta(), text: format!("{}/{}", a, name) });
                            }
                        }
                    }
                    cur = Some(x.next());
                }
            }
        } else if to_ns {
            edits.push(Edit { range: z.meta(), text: format!("{}/{}", a, cursor_sym) });
        }
    }
    Ok(edits)
}

/// `safe-sym`: symbol text of a symbol node, or the symbol of an auto-resolved keyword.
pub fn safe_sym(z: &Loc) -> Option<String> {
    if is_printable_only(z.tag()) {
        return None;
    }
    match (z.tag(), z.node.tk) {
        (Tag::Token, Tk::Sym) => Some(z.node.text.clone()),
        (Tag::Token, Tk::KwAuto) if z.node.text.starts_with("::") => Some(z.node.text[2..].to_string()),
        _ => None,
    }
}

fn add_to_rcf(zloc: &Loc, l: &Libspec) -> Option<Vec<Edit>> {
    let ns_zip = find_namespace(zloc)?.subzip();
    if !need_to_add(&ns_zip, l) {
        return None;
    }
    let rcf_zip = zloc.up()?.subzip();
    let ty = l.ty.trim_start_matches(':');
    let left_ident: i64 = rcf_zip
        .find(&|x| { let n = x.next_raw(); if n.end { None } else { Some(n) } }, &|x| x.tag() == Tag::Newline)
        .map(|n| n.next())
        .and_then(|n| n.meta())
        .map(|m| m.col as i64 - 1)
        .unwrap_or(2);
    let col = left_ident + 2 + ty.len() as i64;
    let first_ok = rcf_zip.down().and_then(|d| d.right()).and_then(|r| r.down()).map(|d| d.string());
    let second_ok = rcf_zip.down().and_then(|d| d.right()).and_then(|r| r.right()).and_then(|r| r.down()).map(|d| d.string());
    let add_form_type = !(first_ok.as_deref() == Some(ty) || second_ok.as_deref() == Some(ty));
    let form_to_add: NR = if l.ty == ":import" {
        sexpr_vec(&[l.lib.clone(), l.class.clone().unwrap_or_default()])
    } else {
        let mut items: Vec<NR> = vec![node_of_string(&l.lib).unwrap_or_else(|| token_sym(&l.lib))];
        let has_more = l.alias.is_some() || l.refer.is_some();
        let mut vecs: Vec<String> = vec![l.lib.clone()];
        if let Some(a) = &l.alias {
            vecs.push(":as".into());
            vecs.push(a.clone());
        }
        if let Some(r) = &l.refer {
            vecs.push(":refer".into());
            vecs.push(format!("[{}]", r));
        }
        let inner_n = if has_more { sexpr_vec(&vecs) } else { items.remove(0) };
        inner(Tag::Quote, vec![inner(Tag::Vector, vec![inner_n])])
    };
    if add_form_type {
        let range = rcf_zip.meta()?;
        let mut kids: Vec<NR> = vec![newlines(1)];
        if left_ident > 0 {
            kids.push(spaces(left_ident as usize));
        }
        kids.push(list(vec![token_sym(ty), spaces(1), form_to_add]));
        let text = forms(kids).string();
        Some(vec![Edit { range: Some(Meta { row: range.row, col: 9, end_row: range.row, end_col: 9 }), text }])
    } else {
        let form = find_value_next(&rcf_zip, ty).and_then(|f| f.up())?;
        let form_zip = form.subzip();
        if !need_to_add(&form_zip, l) {
            return None;
        }
        let m = modified_existing_libform(&form_zip, Some(&form_zip), l);
        let result = match m.result {
            Some(r) => r,
            None => form.subedit(|s| s.append_child_raw(newlines(1)).append_child_raw(spaces(col as usize)).append_child(form_to_add.clone())),
        };
        let res_loc = form.replace(result.node.clone());
        Some(vec![Edit { range: form.meta(), text: res_loc.string() }])
    }
}

#[allow(dead_code)]
fn _u(_: Meta, _: Tag) {}

/// `add-missing-import`.
pub fn add_missing_import(c: &Ctx, z: &Loc, import_name: &str) -> Out {
    let st = CleanSettings::from_json(c.init.as_ref());
    let split: Vec<&str> = import_name.split('.').collect();
    let package = split[..split.len().saturating_sub(1)].join(".");
    let class = split.last().copied().unwrap_or("").to_string();
    let in_rcf = find_ops_up(z, &["comment"]).is_some();
    let mut rcf_loc = None;
    if in_rcf {
        match ask_rcf(c, 0, "import") {
            Ok(true) => rcf_loc = find_ops_up(z, &["comment"]),
            Ok(false) => {}
            Err(o) => return o,
        }
    }
    let target = rcf_loc.clone().unwrap_or_else(|| z.clone());
    let l = Libspec { ty: ":import", lib: package, lib_is_string: false, refer: None, alias: None, class: Some(class) };
    let edits = if target.tag() == Tag::Token && target.node.text == "comment" { add_to_rcf(&target, &l) } else { add_to_namespace(&target, &l, &st) };
    let mut edits = edits.unwrap_or_default();
    if rcf_loc.is_none() {
        let t2 = c.text.clone();
        edits = cleaning_ns_edits(c, edits, &st, &|e| replace_range(t2.as_deref().unwrap_or(""), e));
    }
    if edits.is_empty() { Out::Nil } else { Out::Seq(edits) }
}

fn find_require(ns_loc: &Loc) -> Option<Loc> {
    let start = ns_loc.down()?.right()?;
    start.find(&|x| x.right(), &|x| x.down().map_or(false, |d| d.node.is_kw() && d.node.text == ":require"))
}

/// `swap-namespace-with-alias`.
pub fn swap_namespace_with_alias(z: &Loc, target_ns: &str, chosen_alias: &str) -> Option<Vec<Edit>> {
    let cursor_sym = safe_sym(z)?;
    let cursor_namespace = cursor_sym.split_once('/').map(|(n, _)| n.to_string()).unwrap_or_default();
    let name = cursor_sym.split_once('/').map_or(cursor_sym.clone(), |(_, n)| n.to_string());
    let new_target = z.replace(token_sym(&format!("{chosen_alias}/{name}")));
    let ns_loc = find_namespace(&new_target)?;
    let require_loc = find_require(&ns_loc)?;
    let start = require_loc.down()?;
    let mut c = Some(start);
    let mut libspec: Option<Loc> = None;
    while let Some(x) = c {
        let s = if x.tag() == Tag::Vector { x.down().map(|d| d.string()) } else { Some(x.string()) };
        if s.as_deref() == Some(target_ns) {
            libspec = Some(x);
            break;
        }
        c = x.right();
    }
    let libspec = libspec?;
    let alias_loc = if libspec.tag() == Tag::Vector { libspec.down().and_then(|d| d.find(&|x| x.right(), &|x| x.node.is_kw() && x.node.text == ":as")).and_then(|a| a.right()) } else { None };
    let zm = z.meta();
    let new_text = new_target.string();
    if alias_loc.is_some() {
        return Some(vec![Edit { range: zm, text: new_text }]);
    }
    // find-or-create-libspec
    let libspec_ns = if libspec.tag() == Tag::Vector {
        libspec.down()?
    } else {
        libspec.replace(inner(Tag::Vector, vec![token_sym(&cursor_namespace)])).down()?
    };
    let rm = libspec_ns.rightmost()?;
    let with_alias = if rm.node.is_kw() && rm.node.text == ":as" {
        libspec_ns.right()?.insert_right(token_sym(chosen_alias)).right()?
    } else {
        libspec_ns.insert_right(forms(vec![keyword(":as"), spaces(1), token_sym(chosen_alias)])).right()?
    };
    let replaced = with_alias.up()?;
    Some(vec![Edit { range: libspec.meta(), text: replaced.string() }, Edit { range: zm, text: new_text }])
}
