//! `feature/paredit.clj` (+ rewrite-clj.paredit): slurp / barf / raise / kill over the owned zipper.
use super::exec::{Ctx, Edit, Out, Show};
use super::rz::*;
use super::tree::{Meta, Tag};
use super::zops::{find_at_pos, to_top};

#[derive(Clone, Copy, PartialEq)]
enum Dir {
    Right,
    Left,
}

#[derive(Clone, Copy)]
enum Step {
    DownRaw,
    RightRaw,
    Right,
}

fn mv(z: &Loc, d: Dir) -> Option<Loc> {
    if d == Dir::Right { z.right() } else { z.left() }
}
fn mv_raw(z: &Loc, d: Dir) -> Option<Loc> {
    if d == Dir::Right { z.right_raw() } else { z.left_raw() }
}

fn is_seq(t: Tag) -> bool {
    matches!(t, Tag::Forms | Tag::List | Tag::Vector | Tag::Set | Tag::Map | Tag::NsMap)
}

fn node_eq(a: &N, b: &N) -> bool {
    a.tag == b.tag && a.text == b.text && a.kids.len() == b.kids.len() && a.kids.iter().zip(b.kids.iter()).all(|(x, y)| node_eq(x, y))
}

/// `linebreak-and-comment-nodes`: newline and comment nodes of the whitespace/comment run beside `z`.
fn linebreak_and_comment(z: &Loc, d: Dir) -> Vec<NR> {
    let mut out = vec![];
    let mut c = mv_raw(z, d);
    while let Some(x) = c {
        if !is_wsc(x.tag()) {
            break;
        }
        if matches!(x.tag(), Tag::Newline | Tag::Comment) {
            out.push(x.node.clone());
        }
        c = mv_raw(&x, d);
    }
    out
}

fn nav(mut z: Loc, route: &[Step]) -> Option<Loc> {
    for s in route {
        z = match s {
            Step::DownRaw => z.down_raw()?,
            Step::RightRaw => z.right_raw()?,
            Step::Right => z.right()?,
        };
    }
    Some(z)
}

struct Slurp {
    slurper: Loc,
    slurpee: Loc,
    route: Vec<Step>,
}

fn find_slurp_locs_up(zloc: &Loc, d: Dir) -> Option<Slurp> {
    let mut ups: Vec<Loc> = vec![];
    let mut cur = zloc.up();
    while let Some(u) = cur {
        if u.up().is_none() {
            break;
        }
        let stop = mv(&u, d).is_some();
        cur = u.up();
        ups.push(u);
        if stop {
            break;
        }
    }
    let slurper = ups.last()?.clone();
    let slurpee = mv(&slurper, d)?;
    let mut route = vec![];
    for u in ups.iter().rev().skip(1) {
        route.push(Step::DownRaw);
        route.extend(std::iter::repeat(Step::RightRaw).take(u.lefts().len()));
    }
    route.push(Step::DownRaw);
    route.extend(std::iter::repeat(Step::RightRaw).take(zloc.lefts().len()));
    Some(Slurp { slurper, slurpee, route })
}

fn find_slurp_locs(zloc: &Loc, d: Dir, from_current: bool) -> Option<Slurp> {
    if from_current && is_seq(zloc.tag()) {
        if let Some(s) = mv(zloc, d) {
            return Some(Slurp { slurper: zloc.clone(), slurpee: s, route: vec![] });
        }
    }
    find_slurp_locs_up(zloc, d)
}

/// `paredit/slurp-forward-into` (Ok(None) = the Clojure code throws).
fn slurp_forward(zloc: &Loc) -> Option<Loc> {
    let Some(Slurp { slurper, slurpee, route }) = find_slurp_locs(zloc, Dir::Right, true) else { return Some(zloc.clone()) };
    let mut also = linebreak_and_comment(&slurper, Dir::Right);
    if also.first().map_or(false, |n| n.tag == Tag::Comment) {
        also.insert(0, spaces(1));
    }
    let mut z = slurper.remove_right_while(&is_wsc).remove_right1();
    for n in also {
        z = z.append_child_raw(n);
    }
    z = z.append_child(slurpee.node.clone());
    nav(z, &route)
}

fn slurp_backward(zloc: &Loc) -> Option<Loc> {
    let Some(Slurp { slurper, slurpee, mut route }) = find_slurp_locs(zloc, Dir::Left, true) else { return Some(zloc.clone()) };
    let also = linebreak_and_comment(&slurper, Dir::Left);
    if !route.is_empty() {
        route.insert(1, Step::Right);
    }
    let mut z = slurper.remove_left_while(&is_wsc).remove_left1();
    for n in also {
        z = z.insert_child_raw(n);
    }
    z = z.insert_child(slurpee.node.clone());
    nav(z, &route)
}

fn barf_forward(zloc: &Loc) -> Option<Loc> {
    if zloc.up().is_none() {
        return Some(zloc.clone());
    }
    let barfee = zloc.rightmost()?;
    let also = linebreak_and_comment(&barfee, Dir::Left);
    let left_sibs = zloc.lefts().len();
    let barf_loc = if is_wsc(zloc.tag()) { zloc.right().or_else(|| zloc.left()) } else { Some(zloc.clone()) };
    let same = barf_loc.map_or(false, |b| b.lefts().len() == barfee.lefts().len());
    let mut z = barfee.remove_left_while(&is_wsc).remove_right_while(&is_ws).remove_and_move_up()?;
    z = z.insert_right(barfee.node.clone());
    if !also.is_empty() && z.right_raw().map_or(false, |r| is_ws(r.tag())) {
        z = z.remove_right1();
    }
    for n in also {
        z = z.insert_right_raw(n);
    }
    if same {
        z.right()
    } else {
        let mut d = z.down()?;
        for _ in 0..left_sibs {
            d = d.right_raw()?;
        }
        Some(d)
    }
}

fn barf_backward(zloc: &Loc) -> Option<Loc> {
    if zloc.up().is_none() {
        return Some(zloc.clone());
    }
    let barfee = zloc.leftmost()?;
    let also = linebreak_and_comment(&barfee, Dir::Right);
    let right_sibs = zloc.rights().len();
    let barf_loc = if is_wsc(zloc.tag()) { zloc.left().or_else(|| zloc.right()) } else { Some(zloc.clone()) };
    let same = barf_loc.map_or(false, |b| b.lefts().len() == barfee.lefts().len());
    let mut z = barfee.remove_left_while(&is_ws).remove_right_while(&is_wsc).remove_and_move_up()?;
    z = z.insert_left(barfee.node.clone());
    for n in also {
        z = z.insert_left_raw(n);
    }
    if same {
        z.left()
    } else {
        // z/down* z/rightmost* then back `right_sibs` raw steps
        let mut d = z.down_raw()?.rightmost_raw();
        for _ in 0..right_sibs {
            d = d.left_raw()?;
        }
        Some(d)
    }
}

fn raise(zloc: &Loc) -> Option<Loc> {
    match zloc.up() {
        Some(c) => Some(c.replace(zloc.node.clone())),
        None => Some(zloc.clone()),
    }
}

fn kill(zloc: &Loc) -> Option<Loc> {
    let z = zloc.remove_right_while(&|_| true);
    Some(z.remove_and_move_left().unwrap_or_else(|| z.remove_star()))
}

/// The paredit commands that edit text; Out::Nil when the Clojure code throws or finds nothing.
pub fn paredit_op(c: &Ctx, cmd: &str) -> Out {
    let (Some(root), Some(orig)) = (c.root.as_ref(), c.loc.as_ref()) else { return Out::Nil };
    let Some(om) = orig.meta() else { return Out::Nil };
    let (off_row, off_col) = (c.row as i64 - om.row as i64, c.col as i64 - om.col as i64);
    let Some(pos) = find_at_pos(root, c.row, c.col) else { return Out::Nil };
    let res = match cmd {
        "forward-slurp" => slurp_forward(&pos),
        "forward-barf" => barf_forward(&pos),
        "backward-slurp" => slurp_backward(&pos),
        "backward-barf" => barf_backward(&pos),
        "raise-sexp" => raise(&pos),
        _ => kill(&pos),
    };
    let Some(z) = res else { return Out::Nil };
    let move_cursor = cmd == "raise-sexp";
    let (mut nr, mut nc) = z.start_pos();
    if node_eq(&orig.node, &z.node) {
        nr = (nr as i64 + off_row) as u32;
        nc = (nc as i64 + off_col) as u32;
    } else {
        (nr, nc) = (c.row, c.col);
    }
    let Some(top) = to_top(&z) else { return Out::Nil };
    let Some(rootz) = top.up() else { return Out::Nil };
    let range = if move_cursor {
        let Some(m) = orig.up().and_then(|u| u.meta()) else { return Out::Nil };
        Meta { row: m.row, col: m.col, end_row: m.row, end_col: m.col }
    } else {
        Meta { row: nr, col: nc, end_row: nr, end_col: nc }
    };
    // meta of the :forms root = the whole original document
    let doc = c.text.as_deref().unwrap_or("");
    let last = doc.rsplit('\n').next().unwrap_or("");
    let whole = Meta { row: 1, col: 1, end_row: 1 + doc.matches('\n').count() as u32, end_col: 1 + last.encode_utf16().count() as u32 };
    Out::Map {
        changes: vec![(c.uri.clone(), vec![Edit { range: Some(whole), text: rootz.string() }])],
        resources: vec![],
        show: Some(Show { uri: c.uri.clone(), range: Some(range) }),
    }
}
