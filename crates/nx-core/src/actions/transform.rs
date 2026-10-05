//! Ports of `refactor/transform.clj` and feature namespaces: zipper based refactorings producing text edits.
use super::rz::*;
use super::tree::{Meta, Tag, Tk};
use super::zops::*;

pub struct ZE {
    pub range: Option<Meta>,
    pub text: String,
}

fn ze(range: Option<Meta>, loc: &Loc) -> Option<ZE> {
    Some(ZE { range, text: loc.string() })
}

// ---- collections ----------------------------------------------------------------------------------

fn coll_tag(z: &Loc) -> Option<&'static str> {
    match z.tag() {
        Tag::Vector => Some("vector"),
        Tag::Set => Some("set"),
        Tag::List => Some("list"),
        Tag::Map => Some("map"),
        _ => None,
    }
}

pub fn change_coll(z: &Loc, coll: &str) -> Vec<ZE> {
    if coll_tag(z).is_none() {
        return vec![];
    }
    let kids = z.node.kids.clone();
    let tag = match coll {
        "map" => Tag::Map,
        "vector" => Tag::Vector,
        "set" => Tag::Set,
        "list" => Tag::List,
        _ => return vec![],
    };
    let n = inner(tag, kids);
    let loc = z.replace(n);
    ze(z.meta(), &loc).into_iter().collect()
}

pub fn cycle_coll(z: &Loc) -> Vec<ZE> {
    let Some(t) = coll_tag(z) else { return vec![] };
    let next = match t {
        "map" => "vector",
        "vector" => "set",
        "set" => "list",
        _ => "map",
    };
    change_coll(z, next)
}

// ---- privacy ----------------------------------------------------------------------------------------

/// Metadata keys of a (possibly nested) meta node: (keyword name or ":tag", value text).
fn meta_entries(n: &NR) -> (Vec<(String, String)>, NR) {
    let mut entries = Vec::new();
    let mut cur = n.clone();
    while cur.tag == Tag::Meta {
        let parts: Vec<NR> = cur.kids.iter().filter(|k| !is_wsc(k.tag)).cloned().collect();
        if parts.len() != 2 {
            break;
        }
        let m = &parts[0];
        if m.is_kw() {
            entries.push((m.text.clone(), "true".to_string()));
        } else if m.tag == Tag::Map {
            let items: Vec<&NR> = m.kids.iter().filter(|k| !is_printable_only(k.tag)).collect();
            for p in items.chunks(2) {
                if p.len() == 2 {
                    entries.push((p[0].string(), p[1].string()));
                }
            }
        } else if m.is_sym() || m.tk == Tk::Str {
            entries.push((":tag".to_string(), m.string()));
        }
        let next = parts[1].clone();
        cur = next;
    }
    (entries, cur)
}

pub fn cycle_privacy(z: &Loc, use_meta: bool) -> Vec<ZE> {
    let Some(oploc) = find_ops_up(z, &super::preds::DEFS) else { return vec![] };
    let op = oploc.node.text.clone();
    let switch_defn_minus = op == "defn" && !use_meta;
    let switch_defn = op == "defn-";
    let Some(name_loc) = oploc.right() else { return vec![] };
    let (entries, data) = meta_entries(&name_loc.node);
    let private = switch_defn || entries.iter().any(|(k, v)| k == ":private" && v != "false" && v != "nil");
    let (source, new): (&Loc, NR) = if switch_defn {
        (&oploc, token_sym("defn"))
    } else if switch_defn_minus {
        (&oploc, token_sym("defn-"))
    } else if private {
        let rest: Vec<&(String, String)> = entries.iter().filter(|(k, _)| k != ":private").collect();
        if rest.is_empty() {
            (&name_loc, data)
        } else if rest.len() == 1 && rest[0].1 == "true" {
            (&name_loc, inner(Tag::Meta, vec![keyword(&rest[0].0), spaces(1), data]))
        } else {
            return vec![];
        }
    } else {
        (&name_loc, inner(Tag::Meta, vec![keyword(":private"), spaces(1), name_loc.node.clone()]))
    };
    let loc = source.replace(new);
    ze(source.meta(), &loc).into_iter().collect()
}

// ---- threading ----------------------------------------------------------------------------------------

const THREAD_INVALID: [&str; 18] = [
    "defn", "defn-", "def", "defmacro", "defmulti", "defmethod", "defonce", "deftest", "deftype", "defrecord", "->", "->>", "ns", ":require",
    ":import", "testing", "comment", "when",
];

fn thread_invalid_op(z: Option<Loc>) -> bool {
    match z {
        Some(z) => z.node.tag == Tag::Token && matches!(z.node.tk, Tk::Sym | Tk::Kw) && (THREAD_INVALID.contains(&z.node.text.as_str()) || z.node.text == "if"),
        None => false,
    }
}

pub fn skip_ws_up(z: &Loc) -> Option<Loc> {
    let mut c = z.clone();
    while is_wsc(c.tag()) {
        c = c.up()?;
    }
    Some(c)
}

fn next_opt(z: &Loc) -> Option<Loc> {
    let n = z.next();
    if n.is_end() { None } else { Some(n) }
}

pub fn can_thread_list(z: &Loc) -> bool {
    let Some(z) = skip_ws_up(z) else { return false };
    z.tag() == Tag::List && !thread_invalid_op(next_opt(&z))
}

pub fn can_thread(z: &Loc) -> bool {
    let Some(z) = skip_ws_up(z) else { return false };
    if can_thread_list(&z) {
        return true;
    }
    z.tag() == Tag::Token && z.up().map_or(false, |u| u.tag() == Tag::List) && !thread_invalid_op(z.up().and_then(|u| next_opt(&u)))
}

fn thread_sym(z: &Loc, sym: &str, top_col: u32, keep_parens: bool) -> Option<(Option<Meta>, Loc)> {
    let first_loc = {
        let d = z.down()?;
        if sym == "->" { d.right()? } else { d.right()?.rightmost()? }
    };
    let first_node = first_loc.node.clone();
    let zl = z.left();
    let threaded = zl.as_ref().map_or(false, |l| l.node.is_sym() && l.node.text == sym);
    let meta_node = if threaded { z.up()?.meta() } else { z.meta() };
    let first_col = sym.len() as u32 + top_col;
    let first_loc = first_loc.leftmost()?;
    let after = first_loc
        .edit_path(|l| {
            let m = if sym == "->" { l.right()? } else { l.right()?.rightmost()? };
            Some(m.remove())
        })?
        .up()?;
    let mut loc = after;
    if single_child(&loc) && !keep_parens {
        loc = raise(&loc.down()?);
    }
    if threaded {
        loc = loc.insert_left(first_node.clone());
        loc = loc.left()?;
        loc = loc.insert_right_raw(spaces(first_col as usize));
        loc = loc.insert_right_raw(newlines(1));
        loc = loc.up()?;
    } else {
        loc = wrap_around(&loc, Tag::List);
        loc = loc.insert_child(spaces(first_col as usize));
        loc = loc.insert_child(newlines(1));
        loc = loc.insert_child(first_node);
        loc = loc.insert_child(token_sym(sym));
    }
    Some((meta_node, loc))
}

pub fn thread_one(z: &Loc, sym: &str, keep_parens: bool) -> Vec<ZE> {
    if !can_thread(z) {
        return vec![];
    }
    let Some(m) = z.meta() else { return vec![] };
    match thread_sym(z, sym, m.col, keep_parens) {
        Some((r, loc)) => ze(r, &loc).into_iter().collect(),
        None => vec![],
    }
}

pub fn thread_all(z: &Loc, sym: &str, keep_parens: bool) -> Vec<ZE> {
    if !can_thread(z) {
        return vec![];
    }
    let z = if z.tag() == Tag::List { z.clone() } else { match z.up() { Some(u) => u, None => return vec![] } };
    let Some(top_meta) = z.meta() else { return vec![] };
    // first thread-sym `[]`: clojure-lsp still returns one edit with a nil range and empty text
    let Some((top_range, mut loc)) = thread_sym(&z, sym, top_meta.col, keep_parens) else { return vec![ZE { range: None, text: String::new() }] };
    loop {
        let next_loc = loc.down().and_then(|d| d.right());
        let ok = next_loc.as_ref().map_or(false, |n| can_thread_list(n) && n.down().and_then(|d| d.right()).is_some());
        if ok {
            match thread_sym(&next_loc.unwrap(), sym, top_meta.col, keep_parens) {
                Some((_, l)) => loc = l,
                None => return vec![ZE { range: top_range, text: String::new() }],
            }
        } else {
            return ze(top_range, &loc).into_iter().collect();
        }
    }
}

const THREAD_SYMS: [&str; 4] = ["->", "->>", "some->", "some->>"];

fn unwind_once(z: &Loc) -> Option<(Option<Meta>, Loc)> {
    let thread_loc = find_ops_up(z, &THREAD_SYMS)?;
    let thread_sym = thread_loc.node.text.clone();
    if !THREAD_SYMS.contains(&thread_sym.as_str()) {
        return None;
    }
    let val_loc = thread_loc.right()?;
    let target_loc = val_loc.right()?;
    let extra = target_loc.right().is_some();
    let first = thread_sym.ends_with("->") && !thread_sym.ends_with("->>");
    let val_node = val_loc.node.clone();
    let target_tag = target_loc.tag();
    let up = thread_loc.up()?;
    let range = up.meta();
    let result = up.subedit(|s| {
        let mut l = s.down().unwrap().right().unwrap().remove().right().unwrap();
        if target_tag != Tag::List {
            l = wrap_around(&l, Tag::List);
        }
        l = l.down().unwrap();
        l = if first { l.insert_right(val_node.clone()) } else { l.rightmost().unwrap().insert_right(val_node.clone()) };
        l = l.up().unwrap();
        if !extra {
            l = raise(&l);
        }
        l
    });
    Some((range, result))
}

pub fn unwind_thread(z: &Loc) -> Vec<ZE> {
    match unwind_once(z) {
        Some((r, l)) => ze(r, &l).into_iter().collect(),
        None => vec![],
    }
}

pub fn unwind_all(z: &Loc) -> Vec<ZE> {
    let mut cur = unwind_once(z);
    let mut result: Option<(Option<Meta>, Loc)> = None;
    while let Some((r, l)) = cur {
        let next = unwind_once(&l);
        result = Some((r, l));
        cur = next;
    }
    match result {
        Some((r, l)) => ze(r, &l).into_iter().collect(),
        None => vec![],
    }
}
