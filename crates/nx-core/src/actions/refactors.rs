//! Refactorings that need the analysis (locals, usages) on top of the zipper: let family, extract-to-def, ...
use super::rz::*;
use super::transform::ZE;
use super::tree::{Meta, Tag, Tk, Tree};
use super::zops::*;
use crate::engine::index::B;
use crate::engine::store::FileId;
use crate::query::{El, Q};

/// `z/of-string`: zipper on the first non-whitespace form (inside a forms root).
pub fn of_string(text: &str) -> Option<Loc> {
    let tree = Tree::parse(text);
    if tree.err {
        return None;
    }
    let top = Loc::of_node(from_tree(&tree));
    match top.down_raw().and_then(|d| d.skip_ws_right()) {
        Some(l) => Some(l),
        None => Some(top),
    }
}

/// `(z/node (z/of-string s))`.
pub fn node_of_string(text: &str) -> Option<NR> {
    of_string(text).map(|l| l.node)
}

fn sym(s: &str) -> NR {
    token_sym(s)
}

// ---- analysis helpers -------------------------------------------------------------------------------

pub struct An<'a> {
    pub q: &'a Q<'a>,
    pub f: FileId,
}

#[derive(Clone, Copy, Debug)]
pub struct LocalDef {
    pub row: u32,
    pub col: u32,
    pub end_row: u32,
    pub end_col: u32,
    pub scope_end_row: u32,
    pub scope_end_col: u32,
}

impl<'a> An<'a> {
    fn inside(a: (u32, u32), b_start: (u32, u32), b_end: (u32, u32)) -> bool {
        (b_start.0 < a.0 || (b_start.0 == a.0 && b_start.1 <= a.1)) && (a.0 < b_end.0 || (a.0 == b_end.0 && a.1 <= b_end.1))
    }
    /// `q/find-local-usages-defined-outside-form`: local usage elements (distinct by id) whose definition encloses the form.
    pub fn local_usages_outside(&self, m: Meta) -> Vec<El> {
        let fa = self.q.fa(self.f);
        let ids: std::collections::HashSet<u32> = fa
            .locals
            .iter()
            .filter(|l| Self::inside((m.row, m.col), (l.pos.row, l.pos.col), (l.scope_end_row, l.scope_end_col)))
            .map(|l| l.id)
            .collect();
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for (i, u) in fa.local_usages.iter().enumerate() {
            let p = if u.name_pos.row != 0 { u.name_pos } else { u.pos };
            if Self::inside((p.row, p.col), (m.row, m.col), (m.end_row, m.end_col)) && ids.contains(&u.id) && seen.insert(u.id) {
                out.push(El { f: self.f, b: B::LocalUsage, i: i as u32 });
            }
        }
        out
    }
    /// `widest-scoped-local`.
    pub fn widest_scoped_local(&self, m: Meta) -> Option<LocalDef> {
        let mut acc: Option<LocalDef> = None;
        for u in self.local_usages_outside(m) {
            let Some(d) = self.q.find_definition(u) else { continue };
            if d.b != B::Local {
                continue;
            }
            let l = &self.q.fa(d.f).locals[d.i as usize];
            let cand = LocalDef { row: l.pos.row, col: l.pos.col, end_row: l.pos.end_row, end_col: l.pos.end_col, scope_end_row: l.scope_end_row, scope_end_col: l.scope_end_col };
            acc = match acc {
                Some(a) if !(a.row < cand.row || (a.row == cand.row && a.col < cand.col)) => Some(a),
                _ => Some(cand),
            };
        }
        acc
    }
}

fn in_scope_of_definition(loc: Option<&Loc>, def: Option<LocalDef>) -> bool {
    let Some(d) = def else { return true };
    let Some(l) = loc else { return false };
    let Some(lm) = l.meta() else { return false };
    in_range_meta(Meta { row: d.row, col: d.col, end_row: d.scope_end_row, end_col: d.scope_end_col + 1 }, lm)
}

// ---- introduce-let / expand-let / move-to-let --------------------------------------------------------

fn target_loc(z: &Loc) -> Option<Loc> {
    z.skip_ws_right_loc().or_else(|| if !is_top(z) { super::transform::skip_ws_up(z) } else { None })
}

impl Loc {
    /// `(z/skip-whitespace z/right loc)`: whitespace (not comments) moved over with `z/right`.
    pub fn skip_ws_right_loc(&self) -> Option<Loc> {
        let mut c = self.clone();
        while is_wsc(c.tag()) {
            c = c.right()?;
        }
        Some(c)
    }
}

pub fn introduce_let(z: &Loc, binding: &str) -> Vec<ZE> {
    introduce_let_raw(z, binding).map(|(m, l)| vec![ZE { range: m, text: l.string() }]).unwrap_or_default()
}

/// `introduce-let` returning (range meta, loc).
pub fn introduce_let_raw(z: &Loc, binding: &str) -> Option<(Option<Meta>, Loc)> {
    let zloc = target_loc(z)?;
    let s = sym(binding);
    let col = zloc.meta().map(|m| m.col).unwrap_or(1);
    let mut loc = wrap_around(&zloc, Tag::List);
    loc = loc.insert_child(sym("let"));
    loc = loc.append_child_raw(newlines(1));
    loc = loc.append_child_raw(spaces(col as usize + 1));
    loc = loc.append_child(s.clone());
    loc = loc.down()?;
    loc = loc.right()?;
    loc = wrap_around(&loc, Tag::Vector);
    loc = loc.insert_child(s);
    loc = loc.up()?;
    let loc = join_let(loc);
    Some((loc.meta().or(zloc.meta()), loc))
}

fn multi_arity_fn_definition(z: &Loc) -> bool {
    z.leftmost().map_or(false, |l| l.tag() == Tag::Vector)
}

pub fn expand_let(an: &An, z: &Loc, expand_to_top: bool) -> Option<(Option<Meta>, Loc)> {
    let let_loc = find_ops_up(z, &["let"])?.up()?;
    if is_top(&let_loc) {
        return None;
    }
    let bind_node = let_loc.down()?.right()?.node.clone();
    let parent_let_loc = parent_let(&let_loc);
    let parent_loc = let_loc.up();
    if let Some(pl) = parent_let_loc {
        return Some((pl.meta(), join_let(let_loc)));
    }
    let parent_loc = parent_loc?;
    if !((expand_to_top || !is_top(&parent_loc)) && (expand_to_top || !multi_arity_fn_definition(&let_loc))) {
        return None;
    }
    let def = let_loc.meta().and_then(|m| an.widest_scoped_local(m));
    if !in_scope_of_definition(Some(&parent_loc), def) {
        return None;
    }
    let parent_meta = parent_loc.meta()?;
    let dummy = keyword(":nx/dummy");
    let step = || -> Option<Loc> {
        let l = let_loc.insert_child(dummy.clone());
        let l = l.splice();
        let l = l.right()?;
        let l = l.remove();
        let l = l.right()?;
        let l = l.remove();
        let l = l.find(&|x| x.up(), &|x| x.tag() != Tag::Token)?;
        let l = l.edit_path(|p| {
            let d = p.find_next(&|x| x.tag() == Tag::Token && x.node.text == ":nx/dummy")?;
            Some(d.remove())
        })?;
        let l = wrap_around(&l, Tag::List);
        let l = l.insert_child_raw(spaces(parent_meta.col as usize));
        let l = l.insert_child_raw(newlines(1));
        let l = l.insert_child(bind_node.clone());
        Some(l.insert_child(sym("let")))
    };
    let result = step()?;
    let merge = parent_let(&result).is_some();
    let range = if merge { let_loc.up()?.up()?.meta() } else { Some(parent_meta) };
    Some((range, join_let(result)))
}

fn find_let_form(an: &An, zloc: &Loc) -> Option<Loc> {
    let let_loc = find_ops_up(zloc, &["let"])?.up()?;
    let bindings_loc = let_loc.subzip().down()?.right()?;
    let def = zloc.meta().and_then(|m| an.widest_scoped_local(m));
    let valid = def.is_none()
        || match (bindings_loc.meta(), def) {
            (Some(bm), Some(d)) => in_range_meta(bm, Meta { row: d.row, col: d.col, end_row: d.end_row, end_col: d.end_col }),
            _ => false,
        }
        || in_scope_of_definition(Some(&let_loc), def);
    if valid { Some(let_loc) } else { None }
}

fn find_within(z: &Loc, p: &dyn Fn(&Loc) -> bool) -> Option<Loc> {
    z.subzip().find_next(p)?;
    z.find_next(p)
}

fn replace_in_bind_values(first_bind: Loc, p: &dyn Fn(&Loc) -> bool, replacement: &NR) -> Option<Loc> {
    let mut bind = first_bind;
    let mut marked = false;
    loop {
        let exists = bind.right().and_then(|r| find_within(&r, p)).is_some();
        let bind2 = if exists {
            let b = if !marked { bind.replace(with_mark(&bind.node, 1)) } else { bind.clone() };
            b.edit_path(|b| {
                let r = b.right()?;
                let m = find_within(&r, p)?;
                Some(m.replace(replacement.clone()))
            })
            .unwrap_or(b)
        } else {
            bind.clone()
        };
        match bind2.right().and_then(|r| r.right()) {
            Some(n) => {
                bind = n;
                marked = marked || exists;
            }
            None => return bind2.find(&|l| l.prev(), &|l| l.node.mark & 1 != 0),
        }
    }
}

pub fn move_to_let(an: &An, z: &Loc, binding: &str) -> Option<Vec<ZE>> {
    let zloc = z.skip_ws_right_loc()?;
    if let Some(let_top) = find_let_form(an, &zloc) {
        let sub = let_top.subzip();
        let let_loc = sub.down()?;
        let bound_string = zloc.string();
        let bound_node = zloc.node.clone();
        let binding_sym = sym(binding);
        let bindings_loc = let_loc.right()?;
        let col = bindings_loc.meta()?.col;
        let first_bind = bindings_loc.down();
        let p = |l: &Loc| l.string() == bound_string;
        let bindings_pos = match &first_bind {
            Some(fb) => replace_in_bind_values(fb.clone(), &p, &binding_sym),
            None => None,
        };
        let with_binding = if let Some(bp) = bindings_pos {
            let l = bp.insert_left(binding_sym.clone());
            let l = l.insert_left_raw(bound_node.clone());
            let l = l.insert_left_raw(newlines(1));
            l.insert_left_raw(spaces(col as usize))
        } else {
            let mut l = bindings_loc.clone();
            if first_bind.is_some() {
                l = l.append_child_raw(newlines(1));
                l = l.append_child_raw(spaces(col as usize));
            }
            l = l.append_child(binding_sym.clone());
            l = l.append_child(bound_node.clone());
            l.down()?.rightmost()?
        };
        let mut loc = with_binding.next();
        let new_let = loop {
            if loc.is_end() {
                break let_top.replace(loc.root());
            }
            if loc.string() == bound_string {
                loc = loc.replace(binding_sym.clone()).next();
            } else {
                loc = loc.next();
            }
        };
        let range = let_loc.up()?.meta();
        let _ = range;
        // `(meta (z/node (z/up let-loc)))`: the subzip root (the let form)
        let range = let_top.meta();
        Some(vec![ZE { range, text: new_let.string() }])
    } else {
        // no existing let: introduce-let and expand until it stops
        let mut cur = introduce_let_raw(&zloc, binding);
        let mut prev: Option<(Option<Meta>, Loc)> = None;
        while let Some((r, l)) = cur {
            let next = expand_let(an, &l, false);
            prev = Some((r, l));
            cur = next;
        }
        prev.map(|(r, l)| vec![ZE { range: r, text: l.string() }])
    }
}

// ---- extract-to-def ----------------------------------------------------------------------------------

fn calculate_row_placement(prev_end_row_w_space: Option<u32>, existing_row: u32, existing_end_row: u32) -> u32 {
    match prev_end_row_w_space {
        None => existing_row,
        Some(p) if p == existing_end_row + 1 => p - 1,
        Some(p) => p,
    }
}

/// `prepend-preserving-comment`: an edit placing `new_loc` before `existing`.
pub fn prepend_preserving_comment(existing: &Loc, new_loc: &Loc) -> Option<ZE> {
    let em = existing.meta()?;
    let prev_end = {
        // (z/find-next existing z/left z/sexpr-able?)
        let first = existing.left();
        first.and_then(|l| l.find(&|x| x.left(), &|x| !is_printable_only(x.tag()))).and_then(|l| l.meta()).map(|m| m.end_row + 1)
    };
    let new_row = calculate_row_placement(prev_end, em.row, em.end_row);
    let range = Meta { row: new_row, col: em.col, end_row: new_row, end_col: em.col };
    let edit = new_loc.insert_newline_left(1).insert_newline_right(1).up()?;
    Some(ZE { range: Some(range), text: edit.string() })
}

pub fn extract_to_def(z: &Loc, def_name: Option<&str>, private: bool) -> Option<Vec<ZE>> {
    let zloc = z.skip_ws_right_loc().or_else(|| super::transform::skip_ws_up(z))?;
    let form_loc = to_top(&zloc)?;
    let expr_node = zloc.node.clone();
    let expr_meta = expr_node.meta;
    let name = def_name.unwrap_or("new-value");
    let text = if private { format!("(def ^:private {name}\n  )") } else { format!("(def {name}\n  )") };
    let def_loc = of_string(&text)?.append_child_raw(expr_node);
    let e1 = prepend_preserving_comment(&form_loc, &def_loc)?;
    Some(vec![e1, ZE { range: expr_meta, text: name.to_string() }])
}

// ---- suppress-diagnostic -------------------------------------------------------------------------------

pub fn suppress_diagnostic(z: &Loc, code: &str) -> Option<Vec<ZE>> {
    let ignore = if code.split_once('/').map_or(false, |(ns, _)| ns == "clojure-lsp") { ":clojure-lsp/ignore" } else { ":clj-kondo/ignore" };
    let form = find_op(z).and_then(|o| o.up()).unwrap_or_else(|| z.clone());
    let fm = form.meta()?;
    let map = inner(Tag::Map, vec![keyword(ignore), spaces(1), inner(Tag::Vector, vec![keyword(&format!(":{code}"))])]);
    let mut kids = vec![map, newlines(1)];
    if fm.col > 1 {
        kids.push(spaces(fm.col as usize - 1));
    }
    let un = inner(Tag::Uneval, kids);
    let loc = form.edit_path(|f| Some(f.insert_left_raw(un.clone())))?;
    Some(vec![ZE { range: Some(Meta { row: fm.row, col: fm.col, end_row: fm.row, end_col: fm.col }), text: loc.string() }])
}

#[allow(dead_code)]
fn _t(_: Tk) {}

// ---- destructure-keys ----------------------------------------------------------------------------------

/// Tiny sexpr model to re-print (`n/coerce`) destructuring maps.
#[derive(Clone, Debug)]
enum Sx {
    Atom(String),
    Vec(Vec<Sx>),
    List(Vec<Sx>),
    Set(Vec<Sx>),
    Map(Vec<(Sx, Sx)>),
}

fn sexpr_of(n: &NR) -> Option<Sx> {
    match n.tag {
        Tag::Token | Tag::MultiLine => Some(Sx::Atom(n.text.clone())),
        Tag::Regex => Some(Sx::Atom(n.text.clone())),
        Tag::Vector | Tag::List | Tag::Set => {
            let items: Option<Vec<Sx>> = n.kids.iter().filter(|k| !is_printable_only(k.tag)).map(sexpr_of).collect();
            let items = items?;
            Some(match n.tag {
                Tag::Vector => Sx::Vec(items),
                Tag::List => Sx::List(items),
                _ => Sx::Set(items),
            })
        }
        Tag::Map => {
            let items: Option<Vec<Sx>> = n.kids.iter().filter(|k| !is_printable_only(k.tag)).map(sexpr_of).collect();
            let items = items?;
            if items.len() % 2 != 0 {
                return None;
            }
            Some(Sx::Map(items.chunks(2).map(|c| (c[0].clone(), c[1].clone())).collect()))
        }
        _ => None,
    }
}

fn coerce_str(s: &Sx) -> String {
    match s {
        Sx::Atom(t) => t.clone(),
        Sx::Vec(v) => format!("[{}]", v.iter().map(coerce_str).collect::<Vec<_>>().join(" ")),
        Sx::List(v) => format!("({})", v.iter().map(coerce_str).collect::<Vec<_>>().join(" ")),
        Sx::Set(v) => format!("#{{{}}}", v.iter().map(coerce_str).collect::<Vec<_>>().join(" ")),
        Sx::Map(m) => format!("{{{}}}", m.iter().map(|(k, v)| format!("{} {}", coerce_str(k), coerce_str(v))).collect::<Vec<_>>().join(", ")),
    }
}

struct Destr {
    def_el: El,
    top: Loc,
    def_loc: Loc,
}

fn loc_destructuring_key(l: &Loc) -> Option<String> {
    let up = l.up()?;
    let left = l.left()?;
    if up.tag() == Tag::Map && left.node.is_kw() {
        Some(left.node.text.clone())
    } else {
        None
    }
}

fn local_to_destructure(q: &Q, uri: &str, zloc: &Loc) -> Option<Destr> {
    let m = zloc.meta()?;
    let d = super::preds::def_from_cursor(q, uri, m.row, m.col)?;
    if d.b != B::Local {
        return None;
    }
    let top = to_top(zloc)?;
    let np = q.name_pos(d);
    let def_loc = find_at_pos(&top, np.row, np.col)?;
    let up = def_loc.up();
    let key = match &up {
        Some(u) if u.tag() == Tag::Vector => loc_destructuring_key(u),
        _ => None,
    };
    if let Some(k) = key {
        let name = k.trim_start_matches(':');
        let name = name.rsplit('/').next().unwrap_or(name);
        if matches!(name, "keys" | "syms" | "strs") {
            return None;
        }
    }
    Some(Destr { def_el: d, top, def_loc })
}

struct UsageDatum {
    destructure: bool,
    key: &'static str,
    local: Option<NR>,
    local_usage: Option<String>,
    replacing: Option<Loc>,
}

fn usage_datum(u: Option<Loc>) -> UsageDatum {
    let none = UsageDatum { destructure: false, key: "", local: None, local_usage: None, replacing: None };
    let Some(u) = u else { return none };
    let (Some(left), Some(up)) = (u.left(), u.up()) else { return none };
    if !(left.leftmost_p() && up.tag() == Tag::List) {
        return none;
    }
    let n = &left.node;
    let simple = |s: &str| s.rsplit_once('/').map_or(s.to_string(), |(_, nm)| nm.to_string());
    if n.is_kw() {
        let kw = n.text.trim_start_matches(':');
        let local_usage = simple(kw);
        let qualified = kw.contains('/') && !kw.starts_with('/');
        let local: NR = if n.tk == Tk::KwAuto && (kw.contains('/') || true) && n.text.starts_with("::") {
            left.node.clone()
        } else if qualified {
            token_sym(kw)
        } else {
            token_sym(&local_usage)
        };
        // `(if (qualified-ident? kw) (if auto-resolved? node symbol) local-usage)`: auto-resolved only matters when qualified
        let local = if n.text.starts_with("::") && !kw.contains('/') { token_sym(&local_usage) } else { local };
        return UsageDatum { destructure: true, key: "keys", local: Some(local), local_usage: Some(local_usage), replacing: Some(up) };
    }
    if n.tag == Tag::Quote {
        let Some(d) = left.down() else { return none };
        let sym = d.node.text.clone();
        let local_usage = simple(&sym);
        return UsageDatum { destructure: true, key: "syms", local: Some(token_sym(&sym)), local_usage: Some(local_usage), replacing: Some(up) };
    }
    none
}

pub fn can_destructure_keys(q: &Q, uri: &str, z: &Loc) -> bool {
    local_to_destructure(q, uri, z).is_some()
}

pub fn destructure_keys(q: &Q, uri: &str, z: &Loc) -> Option<Vec<ZE>> {
    let d = local_to_destructure(q, uri, z)?;
    let refs = q.find_references(d.def_el, false, None);
    let usage_data: Vec<UsageDatum> = refs
        .iter()
        .map(|e| {
            let fp = q.form_pos(*e);
            usage_datum(find_at_pos(&d.top, fp.row, fp.col))
        })
        .collect();
    if !usage_data.iter().any(|u| u.destructure) {
        return None;
    }
    // prior destructuring `{... :as x}`
    let prior: Vec<(Sx, Sx)> = if loc_destructuring_key(&d.def_loc).as_deref() == Some(":as") {
        match sexpr_of(&d.def_loc.up()?.node) {
            Some(Sx::Map(m)) => m.into_iter().filter(|(k, _)| !matches!(k, Sx::Atom(a) if a == ":as")).collect(),
            _ => return None,
        }
    } else {
        Vec::new()
    };
    // group-by destructure key (insertion order)
    let mut groups: Vec<(&'static str, Vec<String>)> = Vec::new();
    for u in usage_data.iter().filter(|u| u.destructure) {
        let local = u.local.as_ref().map(|n| n.text.clone()).unwrap_or_default();
        match groups.iter_mut().find(|(k, _)| *k == u.key) {
            Some((_, v)) => {
                if !v.contains(&local) {
                    v.push(local)
                }
            }
            None => groups.push((u.key, vec![local])),
        }
    }
    let new_des: Vec<(String, Vec<String>)> = groups.into_iter().map(|(k, v)| (format!(":{k}"), v)).collect();
    // merge-destructuring: keys of prior then new, distinct
    let mut keys: Vec<String> = Vec::new();
    for (k, _) in &prior {
        let ks = coerce_str(k);
        if !keys.contains(&ks) {
            keys.push(ks);
        }
    }
    for (k, _) in &new_des {
        if !keys.contains(k) {
            keys.push(k.clone());
        }
    }
    let mut entries: Vec<(String, String)> = Vec::new();
    for k in &keys {
        let pv = prior.iter().find(|(pk, _)| coerce_str(pk) == *k).map(|(_, v)| v.clone());
        let nv = new_des.iter().find(|(nk, _)| nk == k).map(|(_, v)| v.clone());
        let is_kw = k.starts_with(':');
        let text = if is_kw {
            let mut items: Vec<String> = Vec::new();
            if let Some(Sx::Vec(v)) = &pv {
                items.extend(v.iter().map(coerce_str));
            } else if let Some(p) = &pv {
                items.push(coerce_str(p));
            }
            if let Some(n) = nv {
                items.extend(n);
            }
            format!("[{}]", items.join(" "))
        } else {
            pv.map(|v| coerce_str(&v)).unwrap_or_default()
        };
        entries.push((k.clone(), text));
    }
    if !usage_data.iter().all(|u| u.destructure) {
        let name = q.name(d.def_el).as_str().to_string();
        entries.push((":as".to_string(), name));
    }
    let text = format!("{{{}}}", entries.iter().map(|(k, v)| format!("{k} {v}")).collect::<Vec<_>>().join(", "));
    let first_range = if prior.is_empty() { d.def_loc.meta() } else { d.def_loc.up()?.meta() };
    let mut out = vec![ZE { range: first_range, text }];
    for u in &usage_data {
        if let (Some(r), Some(lu)) = (&u.replacing, &u.local_usage) {
            out.push(ZE { range: r.meta(), text: lu.clone() });
        }
    }
    Some(out)
}

// ---- cycle-namespaced-map ---------------------------------------------------------------------------

#[derive(Clone, PartialEq, Debug)]
struct Qual {
    auto: bool,
    prefix: Option<String>,
}

/// (auto-resolved?, namespace, name) of a keyword node text.
fn kw_parts(text: &str) -> (bool, Option<String>, String) {
    let auto = text.starts_with("::");
    let body = text.trim_start_matches(':');
    match body.find('/') {
        Some(i) if i > 0 && i + 1 < body.len() => (auto, Some(body[..i].to_string()), body[i + 1..].to_string()),
        _ => (auto, None, body.to_string()),
    }
}

fn key_qualifier(n: &NR) -> Option<Qual> {
    if n.is_kw() {
        let (auto, ns, _) = kw_parts(&n.text);
        if auto || ns.is_some() {
            return Some(Qual { auto, prefix: ns });
        }
        return None;
    }
    if n.is_sym() {
        let t = n.text.as_str();
        return t.find('/').filter(|&i| i > 0 && t != "/").map(|i| Qual { auto: false, prefix: Some(t[..i].to_string()) });
    }
    None
}

fn map_key_nodes(map: &NR) -> Vec<NR> {
    map.kids.iter().filter(|k| !is_printable_only(k.tag)).step_by(2).cloned().collect()
}

fn most_frequent_qualifier(keys: &[NR]) -> Option<Qual> {
    let quals: Vec<Qual> = keys.iter().filter_map(key_qualifier).collect();
    if quals.is_empty() {
        return None;
    }
    let cnt = |q: &Qual| quals.iter().filter(|x| *x == q).count();
    let max = quals.iter().map(|q| cnt(q)).max()?;
    quals.iter().find(|q| cnt(q) == max).cloned()
}

fn update_map_keys(map: &NR, f: &dyn Fn(&NR) -> NR) -> NR {
    let mut key_pos = true;
    let mut out = Vec::with_capacity(map.kids.len());
    for c in &map.kids {
        if is_printable_only(c.tag) {
            out.push(c.clone());
        } else {
            out.push(if key_pos { f(c) } else { c.clone() });
            key_pos = !key_pos;
        }
    }
    with_kids(map, out)
}

fn kw_node(auto: bool, ns: Option<&str>, name: &str) -> NR {
    let mut s = String::from(if auto { "::" } else { ":" });
    if let Some(n) = ns {
        s.push_str(n);
        s.push('/');
    }
    s.push_str(name);
    leaf(Tag::Token, Tk::Kw, &s)
}

fn qualify_key(target: &Qual, n: &NR) -> NR {
    if !(n.is_kw() || n.is_sym()) {
        return n.clone();
    }
    let q = key_qualifier(n);
    if q.as_ref() == Some(target) {
        if n.is_kw() {
            let (_, _, name) = kw_parts(&n.text);
            return kw_node(false, None, &name);
        }
        let name = n.text.split_once('/').map_or(n.text.as_str(), |(_, nm)| nm);
        return token_sym(name);
    }
    if q.is_none() {
        if n.is_kw() {
            let (_, _, name) = kw_parts(&n.text);
            return kw_node(false, Some("_"), &name);
        }
        return token_sym(&format!("_/{}", n.text));
    }
    n.clone()
}

fn dequalify_key(qual_text: &str, n: &NR) -> NR {
    let q_auto = qual_text.starts_with("::");
    let prefix = qual_text.trim_start_matches(':');
    let prefix = if prefix.is_empty() { None } else { Some(prefix) };
    if n.is_kw() {
        let (auto, ns, name) = kw_parts(&n.text);
        if !auto && ns.is_none() {
            return if q_auto { kw_node(true, prefix, &name) } else { kw_node(false, prefix, &name) };
        }
        if !auto && ns.as_deref() == Some("_") {
            return kw_node(false, None, &name);
        }
        return n.clone();
    }
    if n.is_sym() {
        let t = n.text.as_str();
        match t.split_once('/') {
            None => return token_sym(&format!("{}/{}", prefix.unwrap_or(""), t)),
            Some(("_", nm)) => return token_sym(nm),
            _ => return n.clone(),
        }
    }
    n.clone()
}

fn enclosing_map_loc(z: &Loc) -> Option<Loc> {
    let is = |t: Tag| matches!(t, Tag::Map | Tag::NsMap);
    let m = if is(z.tag()) {
        z.clone()
    } else if z.up().map_or(false, |u| is(u.tag())) {
        z.up()?
    } else {
        return None;
    };
    if m.up().map_or(false, |u| u.tag() == Tag::NsMap) { m.up() } else { Some(m) }
}

pub fn cycle_namespaced_map(z: &Loc) -> Option<Vec<ZE>> {
    let m = enclosing_map_loc(z)?;
    if m.tag() == Tag::NsMap {
        let kids = &m.node.kids;
        let qual = kids.first()?;
        let map = kids.last()?;
        if kids.iter().any(|k| k.tag == Tag::Comment) {
            return None;
        }
        let keys = map_key_nodes(map);
        let auto_q = qual.text.starts_with("::");
        if keys.iter().any(|k| key_qualifier(k).map_or(false, |q| q.auto && q.prefix.as_deref() == Some("_"))) {
            return None;
        }
        if auto_q && keys.iter().any(|k| k.is_sym() && key_qualifier(k).is_none()) {
            return None;
        }
        let new = update_map_keys(map, &|k| dequalify_key(&qual.text, k));
        let new = with_meta_of(&new, m.meta());
        let loc = m.replace(new);
        Some(vec![ZE { range: loc.meta(), text: loc.string() }])
    } else {
        let keys = map_key_nodes(&m.node);
        if keys.iter().any(|k| key_qualifier(k).map_or(false, |q| q.prefix.as_deref() == Some("_"))) {
            return None;
        }
        let target = most_frequent_qualifier(&keys)?;
        let new_map = update_map_keys(&m.node, &|k| qualify_key(&target, k));
        let qtext = format!(":{}{}", if target.auto { ":" } else { "" }, target.prefix.clone().unwrap_or_default());
        let qual = leaf(Tag::MapQualifier, Tk::None, &qtext);
        let nsmap = with_meta_of(&inner(Tag::NsMap, vec![qual, new_map]), m.meta());
        let loc = m.replace(nsmap);
        Some(vec![ZE { range: loc.meta(), text: loc.string() }])
    }
}

// ---- cycle-keyword-auto-resolve ------------------------------------------------------------------------

pub fn cycle_keyword(c: &super::exec::Ctx, z: &Loc) -> super::exec::Out {
    use super::exec::{ask, Edit, Out};
    let Some(f) = c.q.s.id(&c.uri) else { return Out::Nil };
    let Some(text) = super::preds::file_text(c.q, f) else { return Out::Nil };
    let tree = Tree::parse(&text);
    let root_z = match super::tree::find_at_pos(&tree, c.row, c.col) {
        Some(x) => x,
        None => return Out::Nil,
    };
    let Some(status) = super::preds::cycle_kw_status(&tree, root_z) else { return Out::Nil };
    let Some(ns_name) = super::preds::find_namespace(&tree).and_then(|n| n.next()).and_then(|n| n.next()).map(|n| n.text().to_string()) else { return Out::Nil };
    let kw = z.node.text.clone();
    let kwd = match status {
        super::preds::KwStatus::AutoToNs => kw[2..].to_string(),
        super::preds::KwStatus::NsToAuto => kw.split('/').nth(1).unwrap_or("").to_string(),
    };
    let new_text = |auto_to_ns: bool| if auto_to_ns { format!(":{ns_name}/{kwd}") } else { format!("::{kwd}") };
    let auto_to_ns = status == super::preds::KwStatus::AutoToNs;
    // q/find-keyword-usages-by-keyword: usages in this file with the same resolved ns + name
    let fa = c.q.fa(f);
    let ns_sym = crate::intern::intern(&ns_name);
    let name_sym = crate::intern::intern(&kwd);
    let usages: Vec<Meta> = fa.keywords.iter().filter(|k| k.ns == ns_sym && k.name == name_sym && k.reg.is_none()).map(|k| Meta { row: k.pos.row, col: k.pos.col, end_row: k.pos.end_row, end_col: k.pos.end_col }).collect();
    let n_usages = usages.len();
    if n_usages > 1 {
        let msg = format!("Change all other {} usages of this keyword in this namespace?", n_usages - 1);
        match ask(c, 0, &msg, &["Yes", "No"]) {
            Err(out) => return out,
            Ok(Some(a)) if a == "Yes" => {
                let mut edits = Vec::new();
                for k in fa.keywords.iter().filter(|k| k.ns == ns_sym && k.name == name_sym && k.reg.is_none()) {
                    if let Some(l) = find_at_pos(&Loc::of_node(from_tree(&tree)), k.pos.row, k.pos.col) {
                        edits.push(Edit { range: l.meta(), text: new_text(auto_to_ns) });
                    }
                }
                return Out::Seq(edits);
            }
            _ => {}
        }
    }
    Out::Seq(vec![Edit { range: z.meta(), text: new_text(auto_to_ns) }])
}

// ---- refer <-> as, replace :refer :all ------------------------------------------------------------------

fn find_token_right_loc(start: &Loc, text: &str) -> Option<Loc> {
    start.find(&|l| l.right(), &|l| l.tag() == Tag::Token && l.node.text == text)
}

fn sexpr_symbols(v: &NR) -> Vec<String> {
    v.kids.iter().filter(|k| !is_printable_only(k.tag)).map(|k| k.text.clone()).collect()
}

fn to_top_or_subzip_top(l: &Loc) -> Loc {
    let mut c = l.clone();
    loop {
        if is_top(&c) {
            return c;
        }
        match c.up() {
            Some(u) => c = u,
            None => return c,
        }
    }
}

fn sort_syms(mut v: Vec<String>) -> Vec<String> {
    v.sort_by(|a, b| {
        let (an, ab) = a.split_once('/').map_or(("", a.as_str()), |(n, b)| (n, b));
        let (bn, bb) = b.split_once('/').map_or(("", b.as_str()), |(n, b)| (n, b));
        an.encode_utf16().cmp(bn.encode_utf16()).then(ab.encode_utf16().cmp(bb.encode_utf16()))
    });
    v.dedup();
    v
}

fn vec_of_syms(names: &[String]) -> NR {
    let mut kids = Vec::new();
    for (i, n) in names.iter().enumerate() {
        if i > 0 {
            kids.push(spaces(1));
        }
        kids.push(token_sym(n));
    }
    inner(Tag::Vector, kids)
}

pub fn refer_to_as(q: &Q, uri: &str, loc: &Loc) -> Option<Vec<ZE>> {
    let f = q.s.id(uri)?;
    let begin = loc.leftmost()?;
    let ns_sym = begin.node.text.clone();
    let refer_loc = find_token_right_loc(&begin, ":refer")?;
    let symbols = sexpr_symbols(&refer_loc.right()?.node);
    let alias = find_token_right_loc(&begin, ":as")?.right()?.node.text.clone();
    let fa = q.fa(f);
    let top = to_top_or_subzip_top(loc);
    let mut out = Vec::new();
    {
        let zloc = refer_loc.right()?.remove().remove().up()?;
        out.push(ZE { range: zloc.meta(), text: zloc.string() });
    }
    for u in fa.var_usages.iter() {
        if symbols.iter().any(|s| s == u.name.as_str()) && !u.refer && u.to.as_str() == ns_sym {
            let np = u.name_pos;
            let l = find_at_pos(&top, np.row, np.col)?;
            out.push(ZE { range: l.meta(), text: format!("{}/{}", alias, u.name.as_str()) });
        }
    }
    Some(out)
}

pub fn as_to_refer(q: &Q, uri: &str, loc: &Loc) -> Option<Vec<ZE>> {
    let f = q.s.id(uri)?;
    let begin = loc.leftmost()?;
    let as_loc = find_token_right_loc(&begin, ":as")?;
    let maybe_refer = find_token_right_loc(&begin, ":refer");
    let as_sym = as_loc.right()?.node.text.clone();
    let fa = q.fa(f);
    let targets: Vec<&crate::analyzer::VarUsage> = fa.var_usages.iter().filter(|u| u.alias.as_str() == as_sym).collect();
    let keep_alias = fa.keywords.iter().any(|k| k.reg.is_none() && k.alias.as_str() == as_sym);
    let remove_alias = |l: Loc| -> Loc { if keep_alias { l } else { l.remove().remove() } };
    let mut symbols: Vec<String> = targets.iter().map(|u| u.name.as_str().to_string()).collect();
    symbols = sort_syms(symbols);
    let top = to_top_or_subzip_top(loc);
    let mut out = Vec::new();
    let zloc = if let Some(r) = maybe_refer {
        let rv = r.right()?;
        let mut all = sexpr_symbols(&rv.node);
        all.extend(symbols.iter().cloned());
        let all = sort_syms(all);
        let l = rv.replace(vec_of_syms(&all));
        let l = l.leftmost()?;
        let l = find_token_right_loc(&l, ":as")?.right()?;
        remove_alias(l).up()?
    } else {
        let l = remove_alias(as_loc.right()?);
        let l = l.insert_right(keyword(":refer")).right()?;
        l.insert_right(vec_of_syms(&symbols)).up()?
    };
    out.push(ZE { range: zloc.meta(), text: zloc.string() });
    for u in &targets {
        let np = u.name_pos;
        let l = find_at_pos(&top, np.row, np.col)?;
        out.push(ZE { range: l.meta(), text: u.name.as_str().to_string() });
    }
    Some(out)
}

pub fn replace_refer_all_with_refer(loc: &Loc, refers: &[String]) -> Vec<ZE> {
    let v = vec_of_syms(refers);
    let new = loc.replace(with_meta_of(&v, loc.meta()));
    vec![ZE { range: loc.meta(), text: new.string() }]
}

pub fn replace_refer_all_with_alias(q: &Q, uri: &str, loc: &Loc) -> Option<Vec<ZE>> {
    let f = q.s.id(uri)?;
    let first = loc.leftmost()?;
    let ns_tok = first.find(&|l| Some(l.next()), &|l| l.tag() == Tag::Token)?;
    let ns = ns_tok.node.text.clone();
    let fake = "an-alias";
    let fa = q.fa(f);
    let alias_loc = loc
        .edit_path(|l| {
            let l = l.replace(with_meta_of(&token_sym(fake), l.meta()));
            let r = l.find(&|x| x.prev(), &|x| x.tag() == Tag::Token && x.node.text == ":refer")?;
            let m = r.meta();
            Some(r.replace(with_meta_of(&keyword(":as"), m)))
        })?
        .up()?;
    let mut out = vec![ZE { range: alias_loc.meta(), text: alias_loc.string() }];
    let top = to_top_or_subzip_top(loc).leftmost()?;
    for u in fa.var_usages.iter().filter(|u| u.to.as_str() == ns) {
        let np = u.name_pos;
        let l = find_at_pos(&top, np.row, np.col)?;
        out.push(ZE { range: l.meta(), text: format!("{}/{}", fake, u.name.as_str()) });
    }
    Some(out)
}

// ---- if <-> cond -----------------------------------------------------------------------------------------

fn find_nearby_node(z: &Loc, name: &str) -> Option<Loc> {
    if z.string() == name {
        return z.up().and_then(|u| super::transform::skip_ws_up(&u));
    }
    if z.left().map_or(false, |l| l.string() == name) {
        return super::transform::skip_ws_up(z);
    }
    if z.right().map_or(false, |r| r.string() == name) {
        return super::transform::skip_ws_up(z);
    }
    z.find_tag_right(Tag::List)
}

fn is_if_expr(l: Option<&Loc>) -> bool {
    l.and_then(|l| l.down()).map_or(false, |d| d.string() == "if")
}

fn gather_comments_left(z: &Loc) -> Vec<NR> {
    let mut v = Vec::new();
    let mut c = z.left_raw();
    while let Some(x) = c {
        if !is_wsc(x.tag()) {
            break;
        }
        if x.tag() == Tag::Comment {
            v.push(x.node.clone());
        }
        c = x.left_raw();
    }
    v.reverse();
    v
}

fn insert_comments_right(mut z: Loc, indent: usize, comments: &[NR]) -> Option<Loc> {
    for c in comments {
        z = z.insert_space_right(indent).right_raw()?;
        z = z.insert_right_raw(c.clone()).right_raw()?;
    }
    Some(z)
}

fn insert_cond_case_part(z: Loc, indent: usize, case: &NR) -> Option<Loc> {
    let z = z.insert_space_right(indent).right_raw()?;
    Some(z.insert_right(case.clone()))
}

pub fn if_to_cond(z: &Loc) -> super::exec::Out {
    if_to_cond_inner(z).unwrap_or(super::exec::Out::Nil)
}

fn if_to_cond_inner(z: &Loc) -> Option<super::exec::Out> {
    use super::exec::Out;
    let Some(if_start) = find_nearby_node(z, "if") else { return Some(Out::Err("Not an if expression".into(), -32602)) };
    let if_range = if_start.meta();
    let indent = if_range.map_or(0, |m| m.col as usize) + 1;
    let Some(cond_start) = of_string("(cond)").and_then(|l| l.down()) else { return None };
    let mut insert_point = cond_start.clone();
    let mut first = true;
    let mut if_form: Option<Loc> = Some(if_start.clone());
    let run = || -> Option<Out> { None };
    let _ = run;
    loop {
        if is_if_expr(if_form.as_ref()) {
            let f = if_form.clone().unwrap();
            let test = f.down()?.right();
            let test = match test { Some(t) => t, None => return None };
            let tru = match test.right() { Some(t) => t, None => return None };
            let true_comments = gather_comments_left(&tru);
            let fals = tru.right();
            let start_of_pair = if first { insert_point.clone() } else { insert_point.insert_newline_right(1).right_raw()? };
            first = false;
            let mut tree = start_of_pair.insert_newline_right(1).right_raw()?;
            tree = insert_comments_right(tree, indent, &true_comments)?;
            tree = insert_cond_case_part(tree, indent, &test.node)?;
            tree = tree.right()?;
            tree = tree.insert_newline_right(1).right_raw()?;
            tree = insert_cond_case_part(tree, indent, &tru.node)?;
            tree = tree.right_raw()?;
            insert_point = tree;
            if_form = fals;
        } else {
            if first && if_form.as_ref().map(|l| l.node.clone()).map_or(false, |n| Rc::ptr_eq(&n, &if_start.node)) {
                return Some(Out::Err("Not an if expression".into(), -32602));
            }
            return Some(match if_form {
                Some(iff) => {
                    let else_comments = gather_comments_left(&iff);
                    let mut l = insert_point.insert_newline_right(1).right_raw();
                    l = l.and_then(|l| l.insert_newline_right(1).right_raw());
                    let Some(mut l) = l else { return None };
                    let Some(l2) = insert_comments_right(l, indent, &else_comments) else { return None };
                    l = l2;
                    let else_node = node_of_string(":else").unwrap();
                    let Some(l3) = insert_cond_case_part(l, indent, &else_node) else { return None };
                    let Some(l4) = l3.right() else { return None };
                    let Some(l5) = l4.insert_newline_right(1).right_raw() else { return None };
                    let Some(l6) = insert_cond_case_part(l5, indent, &iff.node) else { return None };
                    let Some(up) = l6.up() else { return None };
                    Out::Seq(vec![super::exec::Edit { range: if_range, text: up.string() }])
                }
                None => match insert_point.up() {
                    Some(up) => Out::Seq(vec![super::exec::Edit { range: if_range, text: up.string() }]),
                    None => Out::Nil,
                },
            });
        }
    }
}

use std::rc::Rc;

fn keyword_to_true(n: &NR) -> NR {
    if n.is_kw() { node_of_string("true").unwrap() } else { n.clone() }
}

fn insert_if(z: Loc, indent: usize) -> Option<Loc> {
    let z = z.insert_newline_right(1).right_raw()?;
    let z = if indent > 0 { z.insert_space_right(indent).right_raw()? } else { z };
    Some(z.insert_right(node_of_string("(if)")?))
}

fn insert_if_result(z: Loc, indent: usize, result: &NR) -> Option<Loc> {
    let z = if indent > 0 { z.insert_space_right(indent).right_raw()? } else { z };
    z.insert_right(result.clone()).right()
}

pub fn cond_to_if(z: &Loc) -> super::exec::Out {
    use super::exec::{Edit, Out};
    let Some(cond_expr) = find_nearby_node(z, "cond") else { return Out::Err("Not a cond".into(), -32602) };
    if cond_expr.down().map(|d| d.string()) != Some("cond".to_string()) {
        return Out::Err("Not a cond".into(), -32602);
    }
    {
        let mut n = 0;
        let mut c = cond_expr.down().and_then(|d| d.right());
        while let Some(x) = c {
            n += 1;
            c = x.right();
        }
        if n % 2 == 1 {
            return Out::Err("Requires an even number of forms".into(), -32602);
        }
    }
    let cond_indent = cond_expr.meta().map_or(0, |m| m.col as usize - 1);
    let mut if_start = match of_string("dummy") {
        Some(l) => l,
        None => return Out::Nil,
    };
    let mut test_loc = cond_expr.down().and_then(|d| d.right());
    let mut result_loc = test_loc.as_ref().and_then(|t| t.right());
    let mut nesting = 1usize;
    let body = || -> Option<Out> { None };
    let _ = body;
    while let Some(t) = test_loc.clone() {
        let res = match result_loc.clone() { Some(r) => r, None => return Out::Nil };
        let pre = gather_comments_left(&t);
        let post = gather_comments_left(&res);
        let if_indent = 2 * (nesting - 1) + cond_indent;
        let res_indent = 2 * nesting + cond_indent;
        let more = res.right().is_some();
        let test_is_kw = t.node.is_kw();
        let last_else = test_is_kw && nesting > 1 && !more;
        let sub = if last_else {
            (|| -> Option<Loc> {
                let l = if_start.insert_newline_right(1).right_raw()?;
                let l = insert_comments_right(l, if_indent, &pre)?;
                let l = insert_comments_right(l, if_indent, &post)?;
                insert_if_result(l, if_indent, &res.node)
            })()
        } else {
            (|| -> Option<Loc> {
                let l = insert_if(if_start.clone(), if_indent)?;
                let l = l.right()?.down()?;
                let l = l.insert_right(keyword_to_true(&t.node)).right()?.insert_newline_right(1);
                let l = l.right_raw()?;
                let l = insert_comments_right(l, res_indent, &pre)?;
                let l = insert_comments_right(l, res_indent, &post)?;
                insert_if_result(l, res_indent, &res.node)
            })()
        };
        let Some(s) = sub else { return Out::Nil };
        if_start = s;
        test_loc = res.right();
        result_loc = test_loc.as_ref().and_then(|t| t.right());
        nesting += 1;
    }
    let degenerate = nesting == 1 && result_loc.is_none();
    let next_if = if degenerate {
        match if_start.insert_right(node_of_string("nil").unwrap()).right() {
            Some(l) => l,
            None => return Out::Nil,
        }
    } else {
        if_start
    };
    let top = next_if.find(&|l| l.up(), &|l| is_top(l));
    match top {
        Some(t) => Out::Seq(vec![Edit { range: cond_expr.meta(), text: t.string() }]),
        None => Out::Nil,
    }
}

// ---- move-to-for-let ------------------------------------------------------------------------------------

fn find_let_kw_in_bindings(bindings: &Loc) -> Option<Loc> {
    let mut loc = bindings.down();
    while let Some(l) = loc {
        if l.is_end() {
            return None;
        }
        if l.node.is_kw() && l.node.text == ":let" {
            return l.right();
        }
        loc = l.right();
    }
    None
}

fn node_eq(a: &NR, b: &NR) -> bool {
    a.tag == b.tag && a.tk == b.tk && a.string() == b.string()
}

pub fn move_to_for_let(z: &Loc, binding: &str) -> Option<Vec<ZE>> {
    let cursor = z.skip_ws_right_loc()?;
    let for_op = find_ops_up(&cursor.up()?, &["for", "doseq"])?;
    let for_top = for_op.up()?;
    let sub = for_top.subzip();
    let sym_node = sym(binding);
    let op_in_sub = sub.down()?;
    let bindings = op_in_sub.right()?;
    let col = bindings.meta()?.col;
    let let_bindings = find_let_kw_in_bindings(&bindings);
    let with_binding = if let Some(lb) = let_bindings {
        let first_bind = lb.down();
        let let_col = first_bind.as_ref().unwrap_or(&lb).meta().map(|m| m.col);
        let mut l = lb.clone();
        if first_bind.is_some() {
            l = l.append_child_raw(newlines(1));
            l = l.append_child_raw(spaces((let_col.unwrap_or(col + 6) as usize).saturating_sub(1)));
        }
        l = l.append_child(sym_node.clone());
        l = l.append_child_raw(spaces(1));
        l = l.append_child(cursor.node.clone());
        l.down()?.rightmost()?
    } else {
        let mut l = bindings.clone();
        l = l.append_child_raw(newlines(1));
        l = l.append_child_raw(spaces(col as usize));
        l = l.append_child(keyword(":let"));
        l = l.append_child_raw(spaces(1));
        l = l.append_child(inner(Tag::Vector, vec![sym_node.clone(), spaces(1), cursor.node.clone()]));
        l.down()?.rightmost()?.down()?.rightmost()?
    };
    let mut loc = with_binding.next();
    let new_for = loop {
        if loc.is_end() {
            break loc.root();
        }
        if node_eq(&loc.node, &cursor.node) && loc.up().map_or(true, |u| u.tag() != Tag::Quote) {
            loc = loc.replace(sym_node.clone()).next();
        } else {
            loc = loc.next();
        }
    };
    let loc = for_top.replace(new_for);
    Some(vec![ZE { range: for_top.meta(), text: loc.string() }])
}

// ---- restructure-keys ----------------------------------------------------------------------------------

struct RConf {
    replace_loc: Loc,
    map_loc: Loc,
    map_auto: bool,
    map_ns: Option<String>,
}

fn locals_under(q: &Q, uri: &str, m: Meta) -> bool {
    let Some(f) = q.s.id(uri) else { return false };
    q.fa(f).locals.iter().any(|l| An::inside_pub((l.pos.row, l.pos.col), (m.row, m.col), (m.end_row, m.end_col)))
}

impl<'a> An<'a> {
    pub fn inside_pub(a: (u32, u32), b_start: (u32, u32), b_end: (u32, u32)) -> bool {
        Self::inside(a, b_start, b_end)
    }
}

fn map_to_restructure(q: &Q, uri: &str, z: &Loc) -> Option<RConf> {
    let m = z.meta()?;
    let def = super::preds::def_from_cursor(q, uri, m.row, m.col);
    let mut loc = z.clone();
    if let Some(d) = def {
        if d.b == B::Local {
            let np = q.name_pos(d);
            let up = find_at_pos(&to_top(z)?, np.row, np.col)?.up();
            if let Some(u) = up {
                loc = if u.tag() == Tag::Vector { u.up()? } else { u };
            }
        }
    }
    let lm = loc.meta()?;
    if !locals_under(q, uri, lm) {
        return None;
    }
    match loc.tag() {
        Tag::Map => Some(RConf { replace_loc: loc.clone(), map_loc: loc, map_auto: false, map_ns: None }),
        Tag::NsMap => {
            let qual = loc.down()?;
            let t = qual.node.text.clone();
            let auto = t.starts_with("::");
            let prefix = t.trim_start_matches(':').to_string();
            Some(RConf { map_loc: qual.right()?, replace_loc: loc, map_auto: auto, map_ns: if prefix.is_empty() { None } else { Some(prefix) } })
        }
        _ => None,
    }
}

pub fn can_restructure_loc(q: &Q, uri: &str, z: &Loc) -> bool {
    map_to_restructure(q, uri, z).is_some()
}

fn children_seq(l: &Loc) -> Vec<Loc> {
    let mut v = Vec::new();
    let mut c = l.down();
    while let Some(x) = c {
        v.push(x.clone());
        c = x.right();
    }
    v
}

fn kw_name(l: &Loc) -> Option<String> {
    if l.node.is_kw() && l.tag() == Tag::Token {
        let (_, _, name) = kw_parts(&l.node.text);
        return Some(name);
    }
    None
}

struct RData {
    restructure: bool,
    replace_with: Option<NR>,
    refs: Vec<(Meta, String)>, // reference name range + local name
    keep: Option<(NR, NR)>,
}

fn reference_elems(q: &Q, uri: &str, local: &NR, or_pos: Option<Meta>) -> Vec<(Meta, String)> {
    let Some(f) = q.s.id(uri) else { return vec![] };
    let Some(m) = local.meta else { return vec![] };
    let fa = q.fa(f);
    let Some(i) = fa.locals.iter().position(|l| l.pos.row <= m.row && m.row <= l.pos.end_row && l.pos.col <= m.col && m.col <= l.pos.end_col) else { return vec![] };
    let el = El { f, b: B::Local, i: i as u32 };
    let mut out = Vec::new();
    for r in q.find_references(el, false, None) {
        let np = q.name_pos(r);
        let nm = q.name(r).as_str().to_string();
        if let Some(op) = or_pos {
            if An::inside_pub((np.row, np.col), (op.row, op.col), (op.end_row, op.end_col)) {
                continue;
            }
        }
        out.push((Meta { row: np.row, col: np.col, end_row: np.end_row, end_col: np.end_col }, nm));
    }
    out
}

fn restructure_data(q: &Q, uri: &str, key: &Loc, val: &Loc, or_pos: Option<Meta>, conf: &RConf) -> Vec<RData> {
    if let Some(name) = kw_name(key).filter(|n| n == "keys" || n == "syms") {
        let (k_auto, k_ns, _) = kw_parts(&key.node.text);
        let ignore = k_ns.as_deref() == Some("_");
        let implied_ns: Option<String> = if ignore { None } else { k_ns.clone().or_else(|| conf.map_ns.clone()) };
        let implied_auto = !ignore && (k_auto || conf.map_auto);
        let mut out = Vec::new();
        for local in children_seq(val).iter().map(|l| l.node.clone()) {
            let text = local.text.clone();
            let qualified = text.trim_start_matches(':').find('/').map_or(false, |i| i > 0);
            let replace_with: NR = if name == "keys" {
                if local.is_kw() && local.tk == Tk::KwAuto {
                    local.clone()
                } else if qualified {
                    kw_node(false, None, text.trim_start_matches(':')).tap_text_ns()
                } else if let Some(ns) = &implied_ns {
                    kw_node(implied_auto, Some(ns), &text)
                } else if implied_auto {
                    kw_node(true, None, &text)
                } else {
                    kw_node(false, None, &text)
                }
            } else {
                let s = if qualified { text.trim_start_matches(':').to_string() } else if let Some(ns) = &implied_ns { format!("{ns}/{}", text.rsplit('/').next().unwrap_or(&text)) } else { text.clone() };
                inner(Tag::Quote, vec![token_sym(&s)])
            };
            out.push(RData { restructure: true, replace_with: Some(replace_with), refs: reference_elems(q, uri, &local, or_pos), keep: None });
        }
        return out;
    }
    if key.node.is_sym() {
        return vec![RData { restructure: true, replace_with: Some(val.node.clone()), refs: reference_elems(q, uri, &key.node, or_pos), keep: None }];
    }
    vec![RData { restructure: false, replace_with: None, refs: vec![], keep: Some((key.node.clone(), val.node.clone())) }]
}

trait TapKw {
    fn tap_text_ns(self) -> NR;
}
impl TapKw for NR {
    fn tap_text_ns(self) -> NR {
        self
    }
}

fn or_defaults(or_loc: &Option<Loc>) -> Vec<(String, String)> {
    let Some(l) = or_loc else { return vec![] };
    let items: Vec<&NR> = l.node.kids.iter().filter(|k| !is_printable_only(k.tag)).collect();
    items.chunks(2).filter(|p| p.len() == 2).map(|p| (p[0].text.clone(), p[1].string())).collect()
}

fn unshadowed_element_name(q: &Q, uri: &str, replace_loc: &Loc) -> String {
    let scope = find_op(replace_loc).and_then(|o| o.up());
    if let (Some(scope), Some(f)) = (scope, q.s.id(uri)) {
        if let Some(m) = scope.meta() {
            let fa = q.fa(f);
            let mut nums: Vec<i64> = Vec::new();
            for u in &fa.local_usages {
                let p = if u.name_pos.row != 0 { u.name_pos } else { u.pos };
                if An::inside_pub((p.row, p.col), (m.row, m.col), (m.end_row, m.end_col)) {
                    let nm = u.name.as_str();
                    if nm == "element" {
                        nums.push(0);
                    } else if let Some(rest) = nm.strip_prefix("element-") {
                        if let Ok(n) = rest.parse::<i64>() {
                            if rest.chars().all(|c| c.is_ascii_digit()) {
                                nums.push(n);
                            }
                        }
                    }
                }
            }
            if let Some(max) = nums.iter().max() {
                return format!("element-{}", max + 1);
            }
        }
    }
    "element".to_string()
}

pub fn restructure_keys(q: &Q, uri: &str, z: &Loc) -> Option<Vec<ZE>> {
    let conf = map_to_restructure(q, uri, z)?;
    let kids = children_seq(&conf.map_loc);
    let entries: Vec<(Loc, Loc)> = kids.chunks(2).filter(|c| c.len() == 2).map(|c| (c[0].clone(), c[1].clone())).collect();
    let as_loc = entries.iter().find(|(k, _)| kw_name(k).as_deref() == Some("as")).map(|(_, v)| v.clone());
    let or_loc = entries.iter().find(|(k, _)| kw_name(k).as_deref() == Some("or")).map(|(_, v)| v.clone());
    let element = as_loc.as_ref().map(|a| a.node.text.clone()).unwrap_or_else(|| unshadowed_element_name(q, uri, &conf.replace_loc));
    let defaults = or_defaults(&or_loc);
    let or_pos = or_loc.as_ref().and_then(|o| o.meta());
    let mut data: Vec<RData> = Vec::new();
    for (k, v) in &entries {
        if matches!(kw_name(k).as_deref(), Some("as") | Some("or")) {
            continue;
        }
        data.extend(restructure_data(q, uri, k, v, or_pos, &conf));
    }
    let mut edits: Vec<ZE> = Vec::new();
    for d in data.iter().filter(|d| d.restructure) {
        let rw = d.replace_with.clone()?;
        for (range, name) in &d.refs {
            let text = match defaults.iter().find(|(k, _)| k == name) {
                Some((_, dv)) => format!("(get {} {} {})", element, rw.string(), dv),
                None => format!("({} {})", rw.string(), element),
            };
            edits.push(ZE { range: Some(*range), text });
        }
    }
    if edits.is_empty() {
        return None;
    }
    let unr: Vec<&RData> = data.iter().filter(|d| !d.restructure).collect();
    let first = if !unr.is_empty() {
        let mut ents: Vec<(String, String)> = unr.iter().filter_map(|d| d.keep.as_ref()).map(|(k, v)| (k.string(), v.string())).collect();
        let used: Vec<&String> = data.iter().filter(|d| d.restructure).flat_map(|d| d.refs.iter().map(|(_, n)| n)).collect();
        let remaining: Vec<&(String, String)> = defaults.iter().filter(|(k, _)| !used.contains(&k)).collect();
        if !remaining.is_empty() {
            ents.push((":or".into(), format!("{{{}}}", remaining.iter().map(|(k, v)| format!("{k} {v}")).collect::<Vec<_>>().join(", "))));
        }
        ents.push((":as".into(), element.clone()));
        ZE { range: conf.map_loc.meta(), text: format!("{{{}}}", ents.iter().map(|(k, v)| format!("{k} {v}")).collect::<Vec<_>>().join(" ")) }
    } else {
        ZE { range: conf.replace_loc.meta(), text: element.clone() }
    };
    let mut out = vec![first];
    out.extend(edits);
    Some(out)
}
