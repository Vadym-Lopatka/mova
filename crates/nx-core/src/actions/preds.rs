//! Applicability predicates of clojure-lsp code actions (`refactor/transform.clj`, `feature/*.clj`), over the lossless tree.
use super::tree::*;
use crate::engine::index::B;
use crate::engine::store::FileId;
use crate::query::{At, El, Q};
use std::sync::Arc;

pub const DEFS: [&str; 10] = ["defn", "defn-", "def", "defmacro", "defmulti", "defmethod", "defonce", "deftest", "deftype", "defrecord"];

/// `edit/find-op`.
pub fn find_op<'a>(z: Z<'a>) -> Option<Z<'a>> {
    let start = if z.tag() == Tag::List { z.down() } else { None };
    let mut op = start.or_else(|| z.leftmost());
    loop {
        let o = op?;
        let up = o.up()?;
        if up.tag() == Tag::List {
            return Some(o);
        }
        op = up.leftmost();
    }
}

/// Clojure `(name (symbol s))` for the op string compare in `find-ops-up`.
fn name_part(s: &str) -> &str {
    if s == "/" {
        return s;
    }
    match s.find('/') {
        Some(i) => &s[i + 1..],
        None => s,
    }
}

/// `edit/find-ops-up`.
pub fn find_ops_up<'a>(z: Z<'a>, ops: &[&str]) -> Option<Z<'a>> {
    let mut op = find_op(z);
    loop {
        let o = op?;
        if o.tag() == Tag::Token && ops.contains(&name_part(o.text())) {
            return Some(o);
        }
        op = o.up()?.leftmost();
    }
}

pub fn find_function_form<'a>(z: Z<'a>) -> Option<Z<'a>> {
    find_ops_up(z, &DEFS)
}

pub fn can_add_let(z: Z) -> bool {
    z.skip_ws_right().is_some() || (!z.is_top() && z.skip_ws_up().is_some())
}

const THREAD_INVALID: [&str; 18] = [
    "defn", "defn-", "def", "defmacro", "defmulti", "defmethod", "defonce", "deftest", "deftype", "defrecord", "->", "->>", "ns", ":require",
    ":import", "testing", "comment", "when",
];

fn thread_invalid(z: Option<Z>) -> bool {
    match z {
        Some(z) => z.sym_text().map_or(false, |t| THREAD_INVALID.contains(&t) || t == "if"),
        None => false,
    }
}

pub fn can_thread(z: Z) -> bool {
    let Some(z) = z.skip_ws_up() else { return false };
    let list_ok = |z: Z| z.tag() == Tag::List && !thread_invalid(z.next());
    if list_ok(z) {
        return true;
    }
    z.tag() == Tag::Token && z.up().map_or(false, |u| u.tag() == Tag::List) && !thread_invalid(z.up().and_then(|u| u.next()))
}

pub const THREAD_SYMS: [&str; 4] = ["->", "->>", "some->", "some->>"];

pub fn can_unwind_thread(z: Z) -> bool {
    find_ops_up(z, &THREAD_SYMS).map_or(false, |o| THREAD_SYMS.contains(&o.text()))
}

pub fn find_other_colls(z: Z) -> Vec<&'static str> {
    let tag = match z.tag() {
        Tag::Vector => "vector",
        Tag::Set => "set",
        Tag::List => "list",
        Tag::Map => "map",
        _ => return vec![],
    };
    ["vector", "set", "list", "map"].into_iter().filter(|t| *t != tag).collect()
}

fn is_uneval(z: &Z) -> bool {
    z.tag() == Tag::Uneval
}

/// `thread-get/z-children`: `z/down` then `z/right`, sexpr-able only.
pub fn z_children<'a>(z: Z<'a>) -> Vec<Z<'a>> {
    let mut out = Vec::new();
    let mut c = z.down();
    while let Some(x) = c {
        if !is_uneval(&x) {
            out.push(x);
        }
        c = x.right();
    }
    out
}

fn sexpr_is_nil(z: Z) -> bool {
    z.tag() == Tag::Token && z.tk() == Tk::Const && z.text() == "nil"
}

pub fn can_get_in_more(z: Z) -> bool {
    if z.tag() != Tag::List {
        return false;
    }
    let ch = z_children(z);
    let Some(&op) = ch.first() else { return false };
    let more = &ch[1..];
    if op.is_name("get-in") || op.is_name("get") {
        // [map-loc key-or-path-loc default-loc & more]
        if more.len() > 3 {
            return false;
        }
        let (Some(map_loc), Some(_key)) = (more.first(), more.get(1)) else { return false };
        if map_loc.tag() != Tag::List {
            return false;
        }
        let inner = z_children(*map_loc);
        inner.len() == 2
    } else if !sexpr_is_nil(op) {
        // [map-loc default-loc & more]
        !(more.len() > 2 || more.is_empty())
    } else {
        false
    }
}

pub fn can_get_in_less(z: Z) -> bool {
    if z.tag() != Tag::List {
        return false;
    }
    let ch = z_children(z);
    if ch.len() > 4 {
        return false;
    }
    let Some(&op) = ch.first() else { return false };
    let key_or_path = ch.get(2).copied();
    let default = ch.get(3).copied();
    let key_loc: Option<Z>;
    if op.is_name("get-in") {
        let Some(path) = key_or_path else { return false };
        if path.tag() != Tag::Vector {
            return false;
        }
        let Some(first) = path.down() else { return false };
        key_loc = Some(first);
    } else if op.is_name("get") {
        key_loc = key_or_path;
    } else {
        return false;
    }
    match default {
        None => true,
        Some(_) => match key_loc {
            Some(k) => k.is_kw() || (k.tag() == Tag::Quote && k.down().map_or(false, |d| d.tag() == Tag::Token)),
            None => false,
        },
    }
}

pub fn find_function_usage_name_loc<'a>(z: Z<'a>) -> Option<Z<'a>> {
    z.find_up(|x| matches!(x.tag(), Tag::List | Tag::Fn))?.down()
}

fn find_nearby_node<'a>(z: Z<'a>, name: &str) -> Option<Z<'a>> {
    if z.text() == name {
        return z.up()?.skip_ws_up();
    }
    if z.left().map_or(false, |l| l.text() == name) {
        return z.skip_ws_up();
    }
    if z.right().map_or(false, |r| r.text() == name) {
        return z.skip_ws_up();
    }
    // z/find-tag z/right :list
    let mut c = Some(z);
    while let Some(x) = c {
        if x.tag() == Tag::List {
            return Some(x);
        }
        c = x.right();
    }
    None
}

pub fn near(z: Z, name: &str) -> bool {
    find_nearby_node(z, name).and_then(|n| n.down()).map_or(false, |d| d.text() == name)
}

/// `can-extract-to-def?`: `(or (skip-ws right) (skip-ws up))` then a top form above.
pub fn can_extract_to_def(z: Z) -> bool {
    let Some(zz) = z.skip_ws_right().or_else(|| z.skip_ws_up()) else { return false };
    zz.to_top().is_some()
}

pub fn can_demote_fn(z: Z) -> bool {
    convert_fn_to_literal_params(z).is_some()
}

fn outer_fn_form(z: Z) -> bool {
    z.tag() == Tag::List && z.down().map_or(false, |d| d.is_name("fn"))
}

fn convert_fn_to_literal_params<'a>(z: Z<'a>) -> Option<Z<'a>> {
    let fn_z = if outer_fn_form(z) { z } else { find_ops_up(z, &["fn"])?.up()? };
    // params vector: first :vector sibling after the op
    let mut c = fn_z.down();
    let mut vec = None;
    while let Some(x) = c {
        if x.tag() == Tag::Vector {
            vec = Some(x);
            break;
        }
        c = x.right();
    }
    let v = vec?;
    // every param must be a symbol (child-sexprs)
    let mut p = v.down();
    while let Some(x) = p {
        if !is_uneval(&x) && !x.is_sym() {
            return None;
        }
        p = x.right();
    }
    Some(fn_z)
}

/// `promote-fn-params`: :literal-to-fn / :fn-to-defn.
pub fn can_promote_fn(z: Z) -> Option<&'static str> {
    let f = z.find_up(|x| x.tag() == Tag::Fn || outer_fn_form(x))?;
    if f.tag() == Tag::Fn {
        Some("#() to fn")
    } else {
        Some("fn to defn")
    }
}

/// Text of a file (open document, dependency entry or disk).
pub fn file_text(q: &Q, f: FileId) -> Option<Arc<str>> {
    let e = q.entry(f);
    if let Some(t) = &e.text {
        return Some(t.clone());
    }
    super::read_uri_text(&e.uri).map(|s| Arc::from(s.as_str()))
}

fn el_row_col(q: &Q, e: El) -> (u32, u32) {
    let p = q.name_pos(e);
    (p.row, p.col)
}

/// `q/find-definition-from-cursor` at (row, col) of `uri`.
pub fn def_from_cursor(q: &Q, uri: &str, row: u32, col: u32) -> Option<El> {
    let e = q.first_under_cursor(uri, row, col)?;
    q.find_definition(e)
}

fn def_pos(q: &Q, d: El) -> (u32, u32) {
    // name-row / name-col of the definition element
    el_row_col(q, d)
}

/// `inline-symbol?`.
pub fn inline_symbol(q: &Q, uri: &str, row: u32, col: u32) -> bool {
    let Some(d) = def_from_cursor(q, uri, row, col) else { return false };
    if !matches!(d.b, B::Local | B::VarDef) {
        return false;
    }
    let (nr, nc) = def_pos(q, d);
    let Some(text) = file_text(q, d.f) else { return false };
    let tree = Tree::parse(&text);
    if tree.err {
        return false;
    }
    let Some(z) = find_at_pos(&tree, nr, nc) else { return false };
    match find_op(z) {
        Some(op) => op.is_name("let") || op.is_name("def"),
        None => false,
    }
}

pub fn can_destructure_keys(q: &Q, uri: &str, tree: &Tree, z: Z) -> bool {
    let m = z.meta();
    let Some(d) = def_from_cursor(q, uri, m.row, m.col) else { return false };
    if d.b != B::Local {
        return false;
    }
    let Some(top) = z.to_top() else { return false };
    let (dr, dc) = def_pos(q, d);
    let _ = top;
    let Some(def_z) = find_at_pos(tree, dr, dc) else { return false };
    let Some(up) = def_z.up() else { return false };
    if up.tag() == Tag::Vector {
        // loc-destructuring-key: vector is a map value whose key is a keyword
        if let (Some(upup), Some(left)) = (up.up(), up.left()) {
            if upup.tag() == Tag::Map && left.is_kw() {
                let t = left.text();
                let name = t.trim_start_matches(':');
                let name = name.rsplit('/').next().unwrap_or(name);
                if matches!(name, "keys" | "syms" | "strs") {
                    return false;
                }
            }
        }
    }
    true
}

/// `map-to-restructure` (only its truthiness).
pub fn can_restructure_keys(q: &Q, uri: &str, tree: &Tree, z: Z) -> bool {
    let m = z.meta();
    let def = def_from_cursor(q, uri, m.row, m.col);
    let mut loc = z;
    if let Some(d) = def {
        if d.b == B::Local {
            let (dr, dc) = def_pos(q, d);
            if let Some(up) = find_at_pos(tree, dr, dc).and_then(|x| x.up()) {
                loc = if up.tag() == Tag::Vector { match up.up() { Some(u) => u, None => return false } } else { up };
            } else {
                return false;
            }
        }
    }
    // find-locals-under-form: locals of the file inside the form scope
    let lm = loc.meta();
    let Some(f) = q.s.id(uri) else { return false };
    let fa = q.fa(f);
    let inside = fa.locals.iter().any(|l| {
        let (r, c) = (l.pos.row, l.pos.col);
        (lm.row < r || (lm.row == r && lm.col <= c)) && (r < lm.end_row || (r == lm.end_row && c <= lm.end_col))
    });
    if !inside {
        return false;
    }
    loc.tag() == Tag::Map || loc.tag() == Tag::NsMap
}

const SHADOWING: [&str; 7] = ["let", "loop", "fn", "if-let", "when-let", "doseq", "for"];

fn past(z: Z, row: u32, col: u32) -> bool {
    let m = z.meta();
    m.row > row || (m.row == row && m.col >= col)
}

/// `can-move-to-:let?`.
pub fn can_move_to_let_kw(z: Z) -> bool {
    let Some(expr) = z.skip_ws_right() else { return false };
    let Some(zu) = z.up() else { return false };
    let Some(op) = find_ops_up(zu, &["for", "doseq"]) else { return false };
    let Some(second) = op.right() else { return false };
    // path-to-op-clear?
    let om = op.meta();
    let mut loc = Some(expr);
    let mut clear = false;
    while let Some(t) = loc {
        if SHADOWING.contains(&t.text()) || !past(t, om.row, om.col) {
            clear = t.text() == "doseq" || t.text() == "for";
            break;
        }
        loc = if t.leftmost_p() { t.up().and_then(|u| u.leftmost()) } else { t.leftmost() };
    }
    if !clear {
        return false;
    }
    let Some(opu) = op.up() else { return false };
    if opu.tag() != Tag::List || !(op.text() == "for" || op.text() == "doseq") || second.tag() != Tag::Vector {
        return false;
    }
    let sm = second.meta();
    past(expr, sm.end_row, sm.end_col)
}

pub fn fn_name_loc_for_inline<'a>(z: Z<'a>) -> Option<(Z<'a>, Z<'a>)> {
    let defn = find_ops_up(z, &["defn", "defn-"])?;
    let name = defn.right()?;
    Some((defn, name))
}

/// `find-fn-body` -> false for `:error`.
fn fn_body_ok(name: Z) -> bool {
    let mut loc = name.right();
    loop {
        let Some(l) = loc else { return false };
        if l.tag() == Tag::Token && l.tk() == Tk::Str {
            loc = l.right();
        } else if l.tag() == Tag::Vector {
            return true;
        } else if l.tag() == Tag::List {
            if l.right().is_none() {
                loc = l.down();
            } else {
                return false;
            }
        } else {
            return false;
        }
    }
}

/// `can-inline-fn?`.
pub fn can_inline_fn(q: &Q, uri: &str, tree: &Tree, z: Z) -> bool {
    let Some((_defn, name)) = fn_name_loc_for_inline(z) else { return false };
    let m = name.meta();
    let (row, col) = (m.end_row, m.end_col);
    let Some(def) = def_from_cursor(q, uri, row, col) else { return false };
    let at = At { uri, line: row - 1, ch: col - 1 };
    let calls = q.references_from_cursor(at, false, false);
    let in_same_ns = match &calls {
        Some(c) => c.iter().all(|e| !is_from_ne_to(q, *e)),
        None => false,
    };
    if !in_same_ns || !fn_body_ok(name) {
        return false;
    }
    let calls = calls.unwrap();
    if calls.is_empty() {
        return false;
    }
    // arglist of the definition (first arglist-str), no destructuring
    let Some(arglist) = first_arglist(q, def) else { return false };
    let inner = &arglist[1..arglist.len().saturating_sub(1).max(1)];
    if inner.contains('[') || inner.contains('{') {
        return false;
    }
    let nargs = {
        let parts: Vec<&str> = if inner.trim().is_empty() { vec![] } else { inner.split(' ').collect() };
        parts.iter().take_while(|p| **p != "&").count()
    };
    // validate-all-arg-counts: every call site must have >= nargs actual args (in the calls' own files)
    for c in calls {
        if c.f != q.s.id(uri).unwrap_or(c.f) {
            // call sites are only validated in this file by clojure-lsp (`parser/zloc-of-file db uri`)
        }
        let cm = q.form_pos(c);
        let Some(call) = find_at_pos(tree, cm.row, cm.col) else { return false };
        // (z/right (z/down call)) then iterate z/right nargs times; nil encountered => invalid
        let mut cur = call.down().and_then(|d| d.right());
        let mut missing = false;
        for _ in 0..nargs {
            match cur {
                Some(x) => cur = x.right(),
                None => {
                    missing = true;
                    break;
                }
            }
        }
        if missing {
            return false;
        }
    }
    true
}

fn is_from_ne_to(q: &Q, e: El) -> bool {
    if e.b != B::VarUsage {
        return false;
    }
    let u = &q.fa(e.f).var_usages[e.i as usize];
    u.from != u.to
}

pub fn first_arglist(q: &Q, d: El) -> Option<String> {
    if d.b != B::VarDef {
        return None;
    }
    let fa = q.fa(d.f);
    let v = &fa.var_definitions[d.i as usize];
    if !v.has_arglists || v.arglists.1 == 0 {
        return None;
    }
    Some(fa.strs[v.arglists.0 as usize].as_str().to_string())
}

fn find_token_right<'a>(start: Z<'a>, text: &str) -> Option<Z<'a>> {
    let mut c = Some(start);
    while let Some(x) = c {
        if x.tag() == Tag::Token && x.text() == text {
            return Some(x);
        }
        c = x.right();
    }
    None
}

pub fn can_refer_to_as(z: Z) -> bool {
    let Some(start) = z.leftmost() else { return false };
    let Some(refer) = find_token_right(start, ":refer") else { return false };
    let Some(next) = refer.right() else { return false };
    next.tag() == Tag::Vector && find_token_right(start, ":as").is_some()
}

pub fn can_as_to_refer(z: Z) -> bool {
    let Some(start) = z.leftmost() else { return false };
    let refer = find_token_right(start, ":refer");
    let ok = match refer {
        None => true,
        Some(r) => r.right().map_or(false, |n| n.tag() == Tag::Vector),
    };
    ok && find_token_right(start, ":as").is_some()
}

/// `edit/find-namespace`: the form holding the first `ns` token (depth-first).
pub fn find_namespace<'a>(t: &'a Tree<'a>) -> Option<Z<'a>> {
    let mut c = Some(t.root());
    while let Some(z) = c {
        if z.tag() == Tag::Token && z.is_sym() && z.text() == "ns" {
            return z.up();
        }
        c = z.next();
    }
    None
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KwStatus {
    AutoToNs,
    NsToAuto,
}

/// `cycle-keyword-auto-resolve-status`.
pub fn cycle_kw_status(tree: &Tree, z: Z) -> Option<KwStatus> {
    let ns_form = find_namespace(tree)?;
    let ns_name = ns_form.next()?.next()?.text();
    if !z.is_kw() {
        return None;
    }
    let kw = z.text();
    let auto = kw.starts_with("::");
    let slash = kw.contains('/');
    if !slash && auto {
        return Some(KwStatus::AutoToNs);
    }
    if slash && !auto {
        let first = kw.split('/').next().unwrap_or("");
        if first.get(1..).unwrap_or("") == ns_name {
            return Some(KwStatus::NsToAuto);
        }
    }
    None
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NsMapStatus {
    MapToNs,
    NsToMap,
}

#[derive(Clone, PartialEq, Eq, Debug)]
struct Qual {
    auto: bool,
    prefix: Option<String>,
}

fn key_qualifier(k: Z) -> Option<Qual> {
    let t = k.text();
    if k.is_kw() && k.tag() == Tag::Token {
        let auto = k.tk() == Tk::KwAuto;
        let body = t.trim_start_matches(':');
        let ns = body.find('/').filter(|&i| i > 0).map(|i| body[..i].to_string());
        if auto || ns.is_some() {
            return Some(Qual { auto, prefix: ns });
        }
        return None;
    }
    if k.is_sym() {
        let ns = t.find('/').filter(|&i| i > 0 && t != "/").map(|i| t[..i].to_string());
        return ns.map(|p| Qual { auto: false, prefix: Some(p) });
    }
    None
}

fn map_key_nodes<'a>(map: Z<'a>) -> Vec<Z<'a>> {
    map.kid_zs().filter(|c| !matches!(c.tag(), Tag::Whitespace | Tag::Newline | Tag::Comma | Tag::Comment | Tag::Uneval)).step_by(2).collect()
}

fn most_frequent_qualifier(keys: &[Z]) -> Option<Qual> {
    let quals: Vec<Qual> = keys.iter().filter_map(|k| key_qualifier(*k)).collect();
    if quals.is_empty() {
        return None;
    }
    let cnt = |q: &Qual| quals.iter().filter(|x| *x == q).count();
    let max = quals.iter().map(|q| cnt(q)).max()?;
    quals.iter().find(|q| cnt(q) == max).cloned()
}

fn enclosing_map<'a>(z: Z<'a>) -> Option<Z<'a>> {
    let is = |t: Tag| matches!(t, Tag::Map | Tag::NsMap);
    let m = if is(z.tag()) {
        z
    } else if z.up().map_or(false, |u| is(u.tag())) {
        z.up()?
    } else {
        return None;
    };
    if m.up().map_or(false, |u| u.tag() == Tag::NsMap) {
        m.up()
    } else {
        Some(m)
    }
}

/// `cycle-namespaced-map-status`.
pub fn cycle_nsmap_status(z: Z) -> Option<NsMapStatus> {
    let m = enclosing_map(z)?;
    if m.tag() == Tag::NsMap {
        let kids: Vec<Z> = m.kid_zs().collect();
        let qual = *kids.first()?;
        let map = *kids.last()?;
        if kids.iter().any(|k| k.tag() == Tag::Comment) {
            return None;
        }
        let keys = map_key_nodes(map);
        let auto_q = qual.text().starts_with("::");
        if keys.iter().any(|k| key_qualifier(*k).map_or(false, |q| q.auto && q.prefix.as_deref() == Some("_"))) {
            return None;
        }
        if auto_q && keys.iter().any(|k| k.is_sym() && key_qualifier(*k).is_none()) {
            return None;
        }
        Some(NsMapStatus::NsToMap)
    } else {
        let keys = map_key_nodes(m);
        if keys.iter().any(|k| key_qualifier(*k).map_or(false, |q| q.prefix.as_deref() == Some("_"))) {
            return None;
        }
        most_frequent_qualifier(&keys)?;
        Some(NsMapStatus::MapToNs)
    }
}
