//! `feature/thread_get.clj`: move expressions in/out of `get` / `get-in`.
use super::rz::*;
use super::tree::{Meta, Tag};
use super::transform::ZE;
use super::zops::*;

fn z_children(z: &Loc) -> Vec<Loc> {
    let mut out = Vec::new();
    let mut c = z.down();
    while let Some(x) = c {
        if !is_printable_only(x.tag()) {
            out.push(x.clone());
        }
        c = x.right();
    }
    out
}

fn list_of_nodes(nodes: Vec<NR>) -> Loc {
    // (reduce z/append-child (z/of-node (n/list-node [])) nodes)
    let root = Loc::of_node(forms(vec![list(vec![])]));
    let mut l = root.down().unwrap();
    for n in nodes {
        l = l.append_child(n);
    }
    l
}

fn sym_is(l: &Loc, s: &str) -> bool {
    l.node.is_sym() && l.node.text == s
}

struct MoreData {
    replace_with: &'static str,
    map_loc: Loc,
    key_or_path: Loc,
    default: Option<Loc>,
}

fn get_in_more_data(z: &Loc) -> Option<MoreData> {
    if z.tag() != Tag::List {
        return None;
    }
    let ch = z_children(z);
    let op = ch.first()?;
    let more = &ch[1..];
    let nil_op = op.node.tag == Tag::Token && op.node.text == "nil";
    if sym_is(op, "get-in") || sym_is(op, "get") {
        if more.len() > 3 {
            return None;
        }
        let map_loc = more.first()?;
        let kp = more.get(1)?;
        if map_loc.tag() != Tag::List {
            return None;
        }
        let inner_ch = z_children(map_loc);
        if inner_ch.len() != 2 {
            return None;
        }
        let (new_key, new_map) = (&inner_ch[0], &inner_ch[1]);
        let mut key_or_path = kp.clone();
        if key_or_path.tag() != Tag::Vector {
            key_or_path = wrap_around(&key_or_path, Tag::Vector);
        }
        key_or_path = key_or_path.insert_child(new_key.node.clone());
        Some(MoreData { replace_with: "get-in", map_loc: new_map.clone(), key_or_path, default: more.get(2).cloned() })
    } else if !nil_op {
        if more.len() > 2 || more.is_empty() {
            return None;
        }
        Some(MoreData { replace_with: "get", map_loc: more[0].clone(), key_or_path: op.clone(), default: more.get(1).cloned() })
    } else {
        None
    }
}

fn get_in_more_result(d: &MoreData) -> Loc {
    let mut nodes = vec![token_sym(d.replace_with), d.map_loc.node.clone(), d.key_or_path.node.clone()];
    if let Some(def) = &d.default {
        nodes.push(def.node.clone());
    }
    list_of_nodes(nodes)
}

struct LessData {
    replace_key: bool,
    replace_with: &'static str,
    map_loc: Option<Loc>,
    key_loc: Option<Loc>,
    path_loc: Option<Loc>,
    default: Option<Loc>,
}

fn get_in_less_data(z: &Loc) -> Option<LessData> {
    if z.tag() != Tag::List {
        return None;
    }
    let ch = z_children(z);
    if ch.len() > 4 {
        return None;
    }
    let op = ch.first()?;
    let map_loc = ch.get(1).cloned();
    let kp = ch.get(2).cloned();
    let default = ch.get(3).cloned();
    let (replace_key, replace_with, key_loc, path_loc): (bool, &'static str, Option<Loc>, Option<Loc>) = if sym_is(op, "get-in") {
        let kp = kp?;
        if kp.tag() != Tag::Vector {
            return None;
        }
        let new_key = kp.down()?;
        let new_path = new_key.remove();
        let remaining = z_children(&new_path);
        match remaining.len() {
            0 => (true, "", Some(new_key), None),
            1 => (false, "get", Some(new_key), new_path.down()),
            _ => (false, "get-in", Some(new_key), Some(new_path)),
        }
    } else if sym_is(op, "get") {
        (true, "", kp, None)
    } else {
        return None;
    };
    let ok = default.is_none()
        || key_loc.as_ref().map_or(false, |k| k.node.is_kw() || (k.tag() == Tag::Quote && k.down().map_or(false, |d| d.tag() == Tag::Token)));
    if !ok {
        return None;
    }
    Some(LessData { replace_key, replace_with, map_loc, key_loc, path_loc, default })
}

fn nodes_of(l: &Option<Loc>) -> Option<NR> {
    l.as_ref().map(|x| x.node.clone())
}

fn get_in_less_result(d: &LessData) -> Loc {
    let mut interior: Vec<NR> = Vec::new();
    if d.replace_key {
        interior.extend(nodes_of(&d.key_loc));
        interior.extend(nodes_of(&d.map_loc));
    } else {
        interior.push(token_sym(d.replace_with));
        let inner: Vec<NR> = nodes_of(&d.key_loc).into_iter().chain(nodes_of(&d.map_loc)).collect();
        interior.push(list_of_nodes(inner).node.clone());
        interior.extend(nodes_of(&d.path_loc));
    }
    if let Some(def) = &d.default {
        interior.push(def.node.clone());
    }
    list_of_nodes(interior)
}

fn edits(orig: &Loc, new: &Loc) -> Vec<ZE> {
    vec![ZE { range: orig.meta(), text: new.string() }]
}

pub fn get_in_more(z: &Loc) -> Vec<ZE> {
    match get_in_more_data(z) {
        Some(d) => edits(z, &get_in_more_result(&d)),
        None => vec![],
    }
}

pub fn get_in_all(z: &Loc) -> Vec<ZE> {
    let mut cur = z.clone();
    while let Some(d) = get_in_more_data(&cur) {
        cur = get_in_more_result(&d);
    }
    edits(z, &cur)
}

pub fn get_in_less(z: &Loc) -> Vec<ZE> {
    match get_in_less_data(z) {
        Some(d) => edits(z, &get_in_less_result(&d)),
        None => vec![],
    }
}

pub fn get_in_none(z: &Loc) -> Vec<ZE> {
    let mut cur = z.clone();
    while let Some(d) = get_in_less_data(&cur) {
        cur = get_in_less_result(&d);
    }
    edits(z, &cur)
}

#[allow(dead_code)]
fn _unused(_: Meta) {}
