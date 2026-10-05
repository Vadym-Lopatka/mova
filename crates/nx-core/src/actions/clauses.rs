//! `feature/clauses.clj`: clause specs, identification and the drag / sort algorithms over virtual child nodes.
use super::preds::file_text;
use super::tree::*;
use crate::query::Q;

#[derive(Clone, Debug)]
pub struct VN {
    pub tag: Tag,
    pub s: usize,
    pub e: usize,
    pub m: Meta,
    pub id: u32,
}

pub const NOID: u32 = u32::MAX;

pub fn skippable(t: Tag) -> bool {
    matches!(t, Tag::Whitespace | Tag::Newline | Tag::Comma | Tag::Comment | Tag::Uneval)
}

fn utf16_len(s: &str) -> u32 {
    s.chars().map(|c| c.len_utf16() as u32).sum()
}

/// `z-up`'d parent view: parent id and its (possibly overridden) child ids.
pub struct Parent<'a> {
    pub z: Z<'a>,
    pub kids: Vec<u32>,
}

impl<'a> Parent<'a> {
    pub fn of(z: Z<'a>, ov: &Option<(u32, Vec<u32>)>) -> Parent<'a> {
        let kids = match ov {
            Some((p, k)) if *p == z.id => k.clone(),
            _ => z.kids().to_vec(),
        };
        Parent { z, kids }
    }
    pub fn sexprs(&self) -> Vec<u32> {
        self.kids.iter().copied().filter(|&k| !skippable(self.z.t.nodes[k as usize].tag)).collect()
    }
}

#[derive(Clone, Debug)]
pub struct Spec {
    pub ctx: &'static str,
    pub breadth: i64,
    pub rind: (i64, i64),
    pub pulp: i64,
    pub in_threading: bool,
    /// the spec's zloc (node id) and its parent
    pub zloc: u32,
    pub parent: u32,
}

fn simple_sym<'a>(z: Option<Z<'a>>) -> Option<&'a str> {
    let z = z?;
    if !z.is_sym() {
        return None;
    }
    let t = z.text();
    Some(if t == "/" { t } else { t.find('/').map_or(t, |i| &t[i + 1..]) })
}

fn z_first<'a>(z: Z<'a>) -> Option<Z<'a>> {
    // z-down: first non-skippable child
    z.kid_zs().find(|c| !skippable(c.tag()))
}

fn z_right_after<'a>(z: Z<'a>) -> Option<Z<'a>> {
    let mut c = z.right_star();
    while let Some(x) = c {
        if !skippable(x.tag()) {
            return Some(x);
        }
        c = x.right_star();
    }
    None
}

fn z_up_skip<'a>(z: Z<'a>) -> Option<Z<'a>> {
    z.up()
}

fn is_inner(t: Tag) -> bool {
    !matches!(t, Tag::Token | Tag::MultiLine | Tag::Regex | Tag::Comment | Tag::Whitespace | Tag::Newline | Tag::Comma | Tag::MapQualifier)
}

fn in_threading(list: Z) -> bool {
    let Some(up) = z_up_skip(list) else { return false };
    let op = z_first(up);
    matches!(simple_sym(op), Some("->" | "cond->" | "some->"))
}

fn count_children(z: Z, ov: &Option<(u32, Vec<u32>)>) -> i64 {
    if is_inner(z.tag()) {
        Parent::of(z, ov).sexprs().len() as i64
    } else {
        0
    }
}

fn list_spec(list: Z, child_count: i64, ov: &Option<(u32, Vec<u32>)>) -> Option<(&'static str, i64, (i64, i64))> {
    let op = z_first(list);
    let odd = child_count % 2 != 0;
    match simple_sym(op) {
        Some("cond") => Some(("call", 2, (1, 0))),
        Some("cond->" | "cond->>" | "assoc" | "assoc!") => Some(("call", 2, (if in_threading(list) { 1 } else { 2 }, 0))),
        Some("case") => Some((
            "call",
            2,
            if in_threading(list) { (1, if odd { 0 } else { 1 }) } else { (2, if !odd { 0 } else { 1 }) },
        )),
        Some("condp") => {
            let mut has = false;
            let mut c = op;
            while let Some(x) = c {
                if x.tag() == Tag::Token && x.tk() == Tk::Kw && x.text() == ":>>" {
                    has = true;
                    break;
                }
                c = z_right_after(x);
            }
            let breadth = if has { 3 } else { 2 };
            let il = 3;
            let ir = (child_count - il).rem_euclid(breadth);
            if ir == 2 {
                None
            } else {
                Some(("call", breadth, (il, ir)))
            }
        }
        Some("are") => {
            let pc = op.and_then(z_right_after).map_or(0, |v| count_children(v, ov));
            if pc > 0 {
                Some(("call", pc, (3, 0)))
            } else {
                None
            }
        }
        _ => Some(("list", 1, (0, 0))),
    }
}

const BINDING_SYMS: [&str; 8] = ["binding", "doseq", "for", "let", "loop", "with-local-vars", "with-open", "with-redefs"];

fn inside(a: (u32, u32), b: Meta) -> bool {
    (b.row < a.0 || (b.row == a.0 && b.col <= a.1)) && (a.0 < b.end_row || (a.0 == b.end_row && a.1 <= b.end_col))
}

fn establishes_bindings(q: &Q, uri: &str, vec: Z, ov: &Option<(u32, Vec<u32>)>) -> bool {
    // op: z-left of the vector, leftmost, a binding symbol
    let left = {
        let mut c = vec.left_star();
        loop {
            match c {
                Some(x) if skippable(x.tag()) => c = x.left_star(),
                other => break other,
            }
        }
    };
    if let Some(op) = left {
        let leftmost = {
            let mut c = op.left_star();
            loop {
                match c {
                    Some(x) if skippable(x.tag()) => c = x.left_star(),
                    other => break other,
                }
            }
        };
        if leftmost.is_none() && simple_sym(Some(op)).map_or(false, |s| BINDING_SYMS.contains(&s)) {
            return true;
        }
    }
    let children = Parent::of(vec, ov).sexprs();
    if children.len() % 2 != 0 {
        return false;
    }
    let Some(f) = q.s.id(uri) else { return false };
    let fa = q.fa(f);
    let vm = vec.meta();
    let locals: Vec<(u32, u32)> = fa.locals.iter().map(|l| (l.pos.row, l.pos.col)).filter(|p| inside(*p, vm)).collect();
    let t = vec.t;
    for pair in children.chunks(2) {
        let lm = t.z(pair[0]).meta();
        let rm = t.z(pair[1]).meta();
        let l = locals.iter().any(|p| inside(*p, lm));
        let r = locals.iter().any(|p| inside(*p, rm));
        if !(l && !r) {
            return false;
        }
    }
    true
}

/// `clause-spec` (None = not permitted).
pub fn clause_spec<'a>(q: &Q, uri: &str, z: Z<'a>, ov: &Option<(u32, Vec<u32>)>) -> Option<Spec> {
    let parent = z.up()?;
    let (zloc, parent) = if is_inner(parent.tag())
        && !matches!(parent.tag(), Tag::Map | Tag::Set | Tag::Vector | Tag::Forms | Tag::List | Tag::Fn)
    {
        (parent, z_up_skip(parent)?)
    } else {
        (z, parent)
    };
    let child_count = count_children(parent, ov);
    let (ctx, breadth, rind): (&'static str, i64, (i64, i64)) = match parent.tag() {
        Tag::Map => ("map", 2, (0, 0)),
        Tag::Set => ("set", 1, (0, 0)),
        Tag::Forms => ("forms", 1, (0, 0)),
        Tag::Vector => {
            if establishes_bindings(q, uri, parent, ov) {
                ("binding", 2, (0, 0))
            } else {
                ("vector", 1, (0, 0))
            }
        }
        Tag::List | Tag::Fn => list_spec(parent, child_count, ov)?,
        _ => return None,
    };
    let pulp = child_count - rind.0 - rind.1;
    if pulp.rem_euclid(breadth) != 0 {
        return None;
    }
    Some(Spec { ctx, breadth, rind, pulp, in_threading: matches!(parent.tag(), Tag::List | Tag::Fn) && in_threading(parent), zloc: zloc.id, parent: parent.id })
}

/// `sort-clauses/clause-spec` -> context (None when not sortable).
pub fn can_sort(q: &Q, uri: &str, z: Z) -> Option<&'static str> {
    let z = if is_inner(z.tag()) { z.down().unwrap_or(z) } else { z };
    // `(or (and inner? (z/down zloc)) zloc)`: z/down skips whitespace/comments only (not uneval)
    let spec = clause_spec(q, uri, z, &None)?;
    match spec.ctx {
        "forms" | "binding" => None,
        c => Some(c),
    }
}

fn count_siblings(t: &Tree, parent: u32, kids: &[u32], z: u32, left: bool) -> i64 {
    let pos = kids.iter().position(|&k| k == z);
    let _ = (t, parent);
    let Some(p) = pos else { return 0 };
    let range = if left { &kids[..p] } else { &kids[p + 1..] };
    range.iter().filter(|&&k| !skippable(t.nodes[k as usize].tag)).count() as i64
}

/// `f.drag/plan` validity for a direction.
pub fn probable_valid(tree: &Tree, spec: &Spec, ov: &Option<(u32, Vec<u32>)>, forward: bool) -> bool {
    let p = tree.z(spec.parent);
    let kids = Parent::of(p, ov).kids;
    let before = count_siblings(tree, spec.parent, &kids, spec.zloc, true) - spec.rind.0;
    let after = count_siblings(tree, spec.parent, &kids, spec.zloc, false) - spec.rind.1;
    let first = if forward { before } else { after };
    let second = if forward { after } else { before };
    0 <= first && spec.breadth <= second
}

pub fn can_drag(q: &Q, uri: &str, tree: &Tree, z: Z, forward: bool) -> bool {
    match clause_spec(q, uri, z, &None) {
        Some(s) => probable_valid(tree, &s, &None, forward),
        None => false,
    }
}

const PARAM_SYMS: [&str; 3] = ["defn", "defn-", "defmacro"];

pub fn can_drag_param(q: &Q, uri: &str, tree: &Tree, z: Z, forward: bool) -> bool {
    drag_param_plan(q, uri, tree, z).map_or(false, |(spec, ov)| probable_valid(tree, &spec, &ov, forward))
}

/// `drag-param/plan`: the clause spec (+ overridden vector children when a `&` was stripped).
pub fn drag_param_plan<'a>(q: &Q, uri: &str, tree: &'a Tree<'a>, z: Z<'a>) -> Option<(Spec, Option<(u32, Vec<u32>)>)> {
    let vec = z.up()?;
    if vec.tag() != Tag::Vector {
        return None;
    }
    let lm = vec.leftmost()?;
    if !(lm.tag() == Tag::Token && lm.tk() == Tk::Sym && PARAM_SYMS.contains(&lm.text())) {
        return None;
    }
    let kids = vec.kids();
    // vararg marker: first `&` among the vector children (z/find-value from leftmost via z/right)
    let amp = vec.down().and_then(|d| {
        let mut c = Some(d);
        while let Some(x) = c {
            if x.tag() == Tag::Token && x.tk() == Tk::Sym && x.text() == "&" {
                return Some(x);
            }
            c = x.right();
        }
        None
    });
    // note: clojure-lsp searches from `z/leftmost zloc` (may be whitespace-leading); equivalent for well-formed vectors
    let _ = tree;
    let ov = match amp {
        Some(a) => {
            // kill everything right of & then remove & : children before &, minus trailing removed nodes
            let idx = kids.iter().position(|&k| k == a.id)?;
            let remain: Vec<u32> = kids[..idx].to_vec();
            // zloc must still be in the vector (z/find z/right* marked?)
            if !remain.contains(&z.id) {
                return None;
            }
            Some((vec.id, remain))
        }
        None => None,
    };
    let spec = clause_spec(q, uri, z, &ov)?;
    Some((spec, ov))
}

// ---------------------------------------------------------------------------------------------

fn vnodes(t: &Tree, parent: Z, ov: &Option<(u32, Vec<u32>)>) -> Vec<VN> {
    let kids = Parent::of(parent, ov).kids;
    let mut out = Vec::new();
    for &k in &kids {
        let n = &t.nodes[k as usize];
        let m = t.z(k).meta();
        let (s, e) = (n.start as usize, n.end as usize);
        if n.tag == Tag::Comment && t.src[s..e].ends_with('\n') {
            let body = &t.src[s..e - 1];
            let len = utf16_len(body) - 1 + 1; // prefix `;` + s minus `\n`: columns covered
            out.push(VN { tag: Tag::Comment, s, e: e - 1, m: Meta { row: m.row, col: m.col, end_row: m.row, end_col: m.col + len }, id: k });
            out.push(VN {
                tag: Tag::Newline,
                s: e - 1,
                e,
                m: Meta { row: m.row, col: m.col + len, end_row: m.end_row, end_col: m.end_col },
                id: NOID,
            });
        } else {
            out.push(VN { tag: n.tag, s, e, m, id: k });
        }
    }
    out
}

fn is_pad(t: Tag) -> bool {
    matches!(t, Tag::Whitespace | Tag::Newline | Tag::Comma)
}
fn is_ws_c_u(t: Tag) -> bool {
    matches!(t, Tag::Whitespace | Tag::Comment | Tag::Uneval)
}
fn is_wcu(t: Tag) -> bool {
    skippable(t)
}

/// `divide-parent`: sequence of groups alternating padding / element(+affiliated comments).
fn divide_parent(v: &[VN]) -> Vec<Vec<VN>> {
    let mut result: Vec<Vec<VN>> = Vec::new();
    let n = v.len();
    let mut i = 0usize;
    let mut in_padding = true;
    loop {
        if i >= n {
            return result;
        }
        if in_padding {
            let mut j = i;
            while j < n && is_pad(v[j].tag) {
                j += 1;
            }
            result.push(v[i..j].to_vec());
            i = j;
            in_padding = false;
        } else {
            let mut j = i;
            while j < n && is_wcu(v[j].tag) {
                j += 1;
            }
            if j >= n {
                result.push(v[i..n].to_vec());
                return result;
            }
            let prefix = &v[i..j];
            let elem = &v[j];
            let postfix_start = j + 1; // may be n (nil)
            // dividing-start: skip right* while whitespace/comment/uneval
            let mut d = postfix_start;
            while d < n && is_ws_c_u(v[d].tag) {
                d += 1;
            }
            let padding_start = if d < n {
                // z/left* then skip left* while whitespace, then z/right*
                let mut k = d - 1; // d > j so d-1 >= j
                while v[k].tag == Tag::Whitespace {
                    if k == 0 {
                        break;
                    }
                    k -= 1;
                }
                // note: when k stopped at index 0 with whitespace the clojure zipper returns nil for left*; not reachable here
                k + 1
            } else {
                postfix_start
            };
            let mut group: Vec<VN> = prefix.to_vec();
            group.push(elem.clone());
            let mut p = postfix_start;
            while p < n && p != padding_start {
                group.push(v[p].clone());
                p += 1;
            }
            result.push(group);
            i = padding_start;
            in_padding = true;
            if i >= n {
                return result;
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct Item {
    pub idx: Option<usize>,
    pub nodes: Vec<VN>,
}

pub struct Ident {
    pub rind_before: Vec<VN>,
    pub rind_after: Vec<VN>,
    pub items: Vec<Item>,
    pub clause_count: i64,
}

pub fn identify(tree: &Tree, spec: &Spec, ov: &Option<(u32, Vec<u32>)>) -> Ident {
    let parent = tree.z(spec.parent);
    let v = vnodes(tree, parent, ov);
    let groups = divide_parent(&v);
    let ignore_left = spec.rind.0 as usize;
    let split1 = (1 + 2 * ignore_left).min(groups.len());
    let (rind_before, rst) = groups.split_at(split1);
    let n_pulp = (2 * spec.pulp - 1).max(0) as usize;
    let split2 = if spec.pulp == 0 { 0 } else { n_pulp.min(rst.len()) };
    let (pulp, rind_after) = rst.split_at(split2);
    let width = (2 * spec.breadth - 1) as usize;
    let mut items = Vec::new();
    for (ci, chunk) in pulp.chunks(width + 1).enumerate() {
        let cw = width.min(chunk.len());
        let clause: Vec<VN> = chunk[..cw].iter().flatten().cloned().collect();
        let pad: Vec<VN> = chunk[cw..].iter().flatten().cloned().collect();
        items.push(Item { idx: Some(ci), nodes: clause });
        items.push(Item { idx: None, nodes: pad });
    }
    Ident {
        rind_before: rind_before.iter().flatten().cloned().collect(),
        rind_after: rind_after.iter().flatten().cloned().collect(),
        items,
        clause_count: spec.pulp / spec.breadth,
    }
}

pub fn node_text(t: &Tree, nodes: &[VN]) -> String {
    let mut s = String::new();
    for n in nodes {
        s.push_str(&t.src[n.s..n.e]);
    }
    s
}

pub fn nodes_range(nodes: &[VN]) -> Meta {
    let f = nodes.first().map(|n| n.m).unwrap_or(Meta { row: 0, col: 0, end_row: 0, end_col: 0 });
    let l = nodes.last().map(|n| n.m).unwrap_or(f);
    Meta { row: f.row, col: f.col, end_row: l.end_row, end_col: l.end_col }
}

fn trailing_fix(first_lead: Option<&VN>, trailing: Option<&VN>) -> String {
    if trailing.map_or(false, |n| n.tag == Tag::Comment) {
        let col = first_lead.map_or(1, |n| n.m.col);
        let mut s = String::from("\n");
        for _ in 0..(col.saturating_sub(1)) {
            s.push(' ');
        }
        return s;
    }
    String::new()
}

pub struct TextEdit {
    pub range: Meta,
    pub text: String,
}

/// Sorted-clause edit (`sort-clauses`).
pub fn sort_edits(q: &Q, uri: &str, tree: &Tree, z: Z) -> Option<Vec<TextEdit>> {
    let zz = if is_inner(z.tag()) { z.down().unwrap_or(z) } else { z };
    let spec = clause_spec(q, uri, zz, &None)?;
    if matches!(spec.ctx, "forms" | "binding") {
        return None;
    }
    let id = identify(tree, &spec, &None);
    let clauses: Vec<&Item> = id.items.iter().filter(|i| i.idx.is_some()).collect();
    let paddings: Vec<&Item> = id.items.iter().filter(|i| i.idx.is_none()).collect();
    let original: Vec<VN> = id.items.iter().flat_map(|i| i.nodes.iter().cloned()).collect();
    let key = |c: &Item| -> String {
        c.nodes.iter().find(|n| !skippable(n.tag)).map(|n| tree.src[n.s..n.e].to_string()).unwrap_or_default()
    };
    let mut sorted: Vec<&Item> = clauses.clone();
    sorted.sort_by(|a, b| cmp_clojure_str(&key(a), &key(b)));
    // interleave-all sorted clauses with paddings
    let mut new_nodes: Vec<VN> = Vec::new();
    let (mut ci, mut pi) = (0, 0);
    while ci < sorted.len() || pi < paddings.len() {
        if ci < sorted.len() {
            new_nodes.extend(sorted[ci].nodes.iter().cloned());
            ci += 1;
        }
        if pi < paddings.len() {
            new_nodes.extend(paddings[pi].nodes.iter().cloned());
            pi += 1;
        }
    }
    let trailing = id.rind_after.last().or_else(|| new_nodes.last());
    let fix = {
        let lead = id.rind_before.first().or_else(|| original.first());
        trailing_fix(lead, trailing)
    };
    let mut text = node_text(tree, &new_nodes);
    text.push_str(&fix);
    Some(vec![TextEdit { range: nodes_range(&original), text }])
}

/// Clojure `compare` on strings (UTF-16 code unit order).
pub fn cmp_clojure_str(a: &str, b: &str) -> std::cmp::Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

pub fn file_text_of(q: &Q, uri: &str) -> Option<std::sync::Arc<str>> {
    file_text(q, q.s.id(uri)?)
}

// ---- drag ------------------------------------------------------------------------------------------

pub struct DragNodes {
    pub nodes_before: Vec<VN>,
    pub earlier: Vec<VN>,
    pub interstitial: Vec<VN>,
    pub later: Vec<VN>,
    pub nodes_after: Vec<VN>,
    pub origin: Vec<VN>,
}

/// `nodes-to-drag` (rind nodes are intentionally not part of before/after, as in clojure-lsp).
pub fn nodes_to_drag(id: &Ident, origin_idx: usize, forward: bool) -> Option<DragNodes> {
    let dest = if forward { origin_idx as i64 + 1 } else { origin_idx as i64 - 1 };
    let last = id.clause_count - 1;
    let o = origin_idx as i64;
    if !(0 <= o && o <= last && 0 <= dest && dest <= last) {
        return None;
    }
    let earlier_idx = o.min(dest) as usize;
    let items = &id.items;
    let (before, rst) = items.split_at((2 * earlier_idx).min(items.len()));
    let earlier = rst.first()?;
    let inter = rst.get(1)?;
    let later = rst.get(2)?;
    let after = &rst[3.min(rst.len())..];
    Some(DragNodes {
        nodes_before: before.iter().flat_map(|i| i.nodes.iter().cloned()).collect(),
        earlier: earlier.nodes.clone(),
        interstitial: inter.nodes.clone(),
        later: later.nodes.clone(),
        nodes_after: after.iter().flat_map(|i| i.nodes.iter().cloned()).collect(),
        origin: items.iter().find(|i| i.idx == Some(origin_idx)).map(|i| i.nodes.clone()).unwrap_or_default(),
    })
}

/// `node-edits`: swap two clauses.
pub fn node_edits(tree: &Tree, d: &DragNodes) -> Vec<TextEdit> {
    let trailing = d.nodes_after.last().or_else(|| d.earlier.last());
    let lead = d.nodes_before.first().or_else(|| d.earlier.first());
    let fix = trailing_fix(lead, trailing);
    let mut second = node_text(tree, &d.earlier);
    second.push_str(&fix);
    vec![TextEdit { range: nodes_range(&d.earlier), text: node_text(tree, &d.later) }, TextEdit { range: nodes_range(&d.later), text: second }]
}

fn start_of(nodes: &[VN]) -> (i64, i64) {
    let m = nodes.first().map(|n| n.m).unwrap_or(Meta { row: 0, col: 0, end_row: 0, end_col: 0 });
    (m.row as i64, m.col as i64)
}
fn end_of(nodes: &[VN]) -> (i64, i64) {
    let m = nodes.last().map(|n| n.m).unwrap_or(Meta { row: 0, col: 0, end_row: 0, end_col: 0 });
    (m.end_row as i64, m.end_col as i64)
}
fn offset(s: (i64, i64), e: (i64, i64)) -> (i64, i64) {
    let rows = e.0 - s.0;
    (rows, if rows == 0 { e.1 - s.1 } else { e.1 })
}
fn plus_extent(p: (i64, i64), ext: (i64, i64)) -> (i64, i64) {
    (p.0 + ext.0, if ext.0 == 0 { ext.1 + p.1 } else { ext.1 })
}

/// `reposition-cursor`.
pub fn reposition_cursor(cursor: (i64, i64), forward: bool, d: &DragNodes) -> Meta {
    let cursor_offset = offset(start_of(&d.origin), cursor);
    let mut p = start_of(&d.earlier);
    if forward {
        p = plus_extent(p, offset(start_of(&d.later), end_of(&d.later)));
        p = plus_extent(p, offset(start_of(&d.interstitial), end_of(&d.interstitial)));
        p = plus_extent(p, cursor_offset);
    } else {
        p = plus_extent(p, cursor_offset);
    }
    Meta { row: p.0 as u32, col: p.1 as u32, end_row: p.0 as u32, end_col: p.1 as u32 }
}

pub struct DragOut {
    pub edits: Vec<TextEdit>,
    pub show: Meta,
}

pub fn drag(q: &Q, uri: &str, tree: &Tree, z: Z, forward: bool, cursor: (i64, i64)) -> Option<DragOut> {
    let spec = clause_spec(q, uri, z, &None)?;
    if !probable_valid(tree, &spec, &None, forward) {
        return None;
    }
    drag_spec(tree, &spec, &None, forward, cursor)
}

pub fn drag_spec(tree: &Tree, spec: &Spec, ov: &Option<(u32, Vec<u32>)>, forward: bool, cursor: (i64, i64)) -> Option<DragOut> {
    let id = identify(tree, spec, ov);
    let origin = id.items.iter().find(|i| i.idx.is_some() && i.nodes.iter().any(|n| n.id == spec.zloc))?;
    let d = nodes_to_drag(&id, origin.idx?, forward)?;
    Some(DragOut { edits: node_edits(tree, &d), show: reposition_cursor(cursor, forward, &d) })
}

// ---- drag-param ------------------------------------------------------------------------------------

fn z_right_sk<'a>(z: Z<'a>) -> Option<Z<'a>> {
    let mut c = z.right_star();
    while let Some(x) = c {
        if !skippable(x.tag()) {
            return Some(x);
        }
        c = x.right_star();
    }
    None
}
fn z_left_sk<'a>(z: Z<'a>) -> Option<Z<'a>> {
    let mut c = z.left_star();
    while let Some(x) = c {
        if !skippable(x.tag()) {
            return Some(x);
        }
        c = x.left_star();
    }
    None
}

pub struct ParamDrag {
    pub origin_idx: usize,
    pub defn: DragOut,
}

/// The defn part of `drag-param` (plan + identify + drag-clause), with the origin clause index.
pub fn drag_param_defn(q: &Q, uri: &str, tree: &Tree, z: Z, forward: bool, cursor: (i64, i64)) -> Option<(ParamDrag, Spec)> {
    let (spec, ov) = drag_param_plan(q, uri, tree, z)?;
    if !probable_valid(tree, &spec, &ov, forward) {
        return None;
    }
    let id = identify(tree, &spec, &ov);
    let origin = id.items.iter().find(|i| i.idx.is_some() && i.nodes.iter().any(|n| n.id == spec.zloc))?;
    let idx = origin.idx?;
    let out = drag_spec(tree, &spec, &ov, forward, cursor)?;
    Some((ParamDrag { origin_idx: idx, defn: out }, spec))
}

/// `usage-edit`: edits for one call site (None = skipped).
pub fn usage_edit(q: &Q, uri: &str, tree: &Tree, name_row: u32, name_col: u32, clause_idx: usize, forward: bool) -> Option<Vec<TextEdit>> {
    let usage = find_at_pos(tree, name_row, name_col)?;
    let up = usage.up()?;
    if up.tag() != Tag::List {
        return None;
    }
    let leftmost = z_left_sk(usage).is_none();
    let ok = leftmost
        || z_left_sk(usage).map_or(false, |l| z_left_sk(l).is_none() && l.is_sym() && l.text() == "partial");
    if !ok {
        return None;
    }
    let mut arg = z_right_sk(usage);
    for _ in 0..clause_idx {
        arg = arg.and_then(z_right_sk);
    }
    let arg = arg?;
    let spec = clause_spec(q, uri, arg, &None)?;
    if spec.in_threading {
        return None;
    }
    let id = identify(tree, &spec, &None);
    let origin = id.items.iter().find(|i| i.idx.is_some() && i.nodes.iter().any(|n| n.id == spec.zloc))?;
    let d = nodes_to_drag(&id, origin.idx?, forward)?;
    let edits = node_edits(tree, &d);
    if edits.is_empty() { None } else { Some(edits) }
}
