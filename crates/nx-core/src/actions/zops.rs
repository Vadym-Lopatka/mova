//! `refactor/edit.clj` helpers and small zipper utilities over `rz::Loc`.
use super::rz::*;
use super::tree::{Meta, Tag, Tk};

/// `edit/find-at-pos`: deepest loc at (row, col) (1-based), starting from the root loc.
pub fn find_at_pos(root: &Loc, row: u32, col: u32) -> Option<Loc> {
    let in_range = |m: Meta| row >= m.row && row <= m.end_row && (row != m.row || col >= m.col) && (row != m.end_row || col < m.end_col);
    let inherits = |z: &Loc| {
        let Some(m) = z.meta() else { return false };
        if in_range(m) {
            return true;
        }
        m.end_col == col
            && z.rightmost_p()
            && z.up().map_or(false, |u| matches!(u.tag(), Tag::List | Tag::Vector | Tag::Map | Tag::Set | Tag::Fn) && u.meta().map_or(false, in_range))
    };
    let mut z = if root.tag() == Tag::Forms { root.down_raw()? } else { root.clone() };
    loop {
        if z.is_end() {
            return Some(z);
        }
        if inherits(&z) {
            if let Some(inner) = z.down_raw().and_then(|d| d.find(&|l| l.right_raw(), &inherits)) {
                z = inner;
            } else {
                return Some(z);
            }
        } else {
            z = z.right_raw()?;
        }
    }
}

pub fn is_top(z: &Loc) -> bool {
    z.up().map_or(false, |u| u.tag() == Tag::Forms)
}

pub fn to_top(z: &Loc) -> Option<Loc> {
    z.find(&|l| l.up(), &|l| is_top(l))
}

/// `edit/find-op`.
pub fn find_op(z: &Loc) -> Option<Loc> {
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
pub fn find_ops_up(z: &Loc, ops: &[&str]) -> Option<Loc> {
    let mut op = find_op(z);
    loop {
        let o = op?;
        if o.tag() == Tag::Token && ops.contains(&name_part(&o.node.text)) {
            return Some(o);
        }
        op = o.up()?.leftmost();
    }
}

pub fn single_child(z: &Loc) -> bool {
    match z.down() {
        Some(c) => c.leftmost_p() && c.rightmost_p(),
        None => false,
    }
}

/// `edit/raise`.
pub fn raise(z: &Loc) -> Loc {
    match z.up() {
        Some(c) => c.replace(z.node.clone()),
        None => z.clone(),
    }
}

/// `edit/wrap-around`.
pub fn wrap_around(z: &Loc, tag: Tag) -> Loc {
    let node = z.node.clone();
    let m = node.meta;
    let w = with_meta_of(&inner(tag, vec![]), m);
    z.replace(w).insert_child(node)
}

/// `edit/parent-let?`: the let form above when the loc's leftmost sibling is `let`.
pub fn parent_let(z: &Loc) -> Option<Loc> {
    let op = z.leftmost()?;
    if op.node.is_sym() && op.node.text == "let" {
        op.up()
    } else {
        None
    }
}

/// `edit/join-let`.
pub fn join_let(let_loc: Loc) -> Loc {
    if parent_let(&let_loc).is_none() {
        return let_loc;
    }
    let bind_node = let_loc.down().and_then(|d| d.right()).map(|b| b.node.clone());
    let Some(bind_node) = bind_node else { return let_loc };
    let step = || -> Option<Loc> {
        let l = let_loc.down()?.right()?; // move to inner binding
        let l = l.remove(); // remove inner binding
        let l = l.remove(); // remove inner let moving to prev; the surrounding list
        let l = l.splice(); // splice let body into outer let body
        let l = l.leftmost()?; // move to let
        let l = l.right()?; // move to parent binding
        let l = l.append_child(bind_node.clone()); // place into binding
        let l = l.down()?; // move into binding
        let l = l.rightmost()?; // move to nested binding
        let l = l.splice(); // remove nesting
        let l = l.left()?;
        let l = l.insert_right_raw(newlines(1));
        let l = l.up()?;
        l.up()
    };
    step().unwrap_or(let_loc)
}

pub fn in_range_meta(a: Meta, b: Meta) -> bool {
    // edit/in-range?: b contained within a
    b.row >= a.row && b.end_row <= a.end_row && (if b.row == a.row { b.col >= a.col } else { true }) && (if b.end_row == a.end_row { b.end_col < a.end_col } else { true })
}

/// Text helpers ----------------------------------------------------------------------------------

pub fn is_sym_named(z: &Loc, s: &str) -> bool {
    z.node.is_sym() && z.node.text == s
}

pub fn tk_of(z: &Loc) -> Tk {
    z.node.tk
}
