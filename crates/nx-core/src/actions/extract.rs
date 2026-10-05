//! `refactor/transform.clj`: extract-function (selection analysis, threaded contexts, new defn generation).
use super::exec::{Ctx, Edit, Out};
use super::refactors::{of_string, prepend_preserving_comment, An};
use super::rz::*;
use super::tree::{Meta, Tag};
use super::zops::*;

fn past(z: &Loc, row: u32, col: u32) -> bool {
    z.meta().map_or(false, |m| m.row > row || (m.row == row && m.col >= col))
}

fn ws_node(l: &Loc) -> bool {
    is_ws(l.tag())
}

fn sexpr_able(l: &Loc) -> bool {
    !is_printable_only(l.tag())
}

fn single_cursor(row: u32, col: u32, end_row: u32, end_col: u32) -> bool {
    row == end_row && col == end_col
}

fn next_expr_start(start: &Loc, single: bool, sel_end_row: u32, sel_end_col: u32) -> Option<Loc> {
    start.find(&|l| l.right_raw(), &|l| !ws_node(l) && !(!single && past(l, sel_end_row, sel_end_col)))
}

fn locate_parent_expr(start: &Loc) -> Option<Loc> {
    if is_top(start) {
        return Some(start.clone());
    }
    if start.up().map_or(false, |u| matches!(u.tag(), Tag::Vector | Tag::Map | Tag::Set)) {
        return start.up();
    }
    find_op(start)?.up()
}

fn next_not_executable(start: &Loc, expr_start: &Loc) -> bool {
    let next_sexp = expr_start.find(&|l| l.right(), &|l| sexpr_able(l));
    start.tag() == Tag::Whitespace && next_sexp.map_or(true, |n| n.tag() != Tag::List)
}

fn collect_to_first_sexpr(start: &Loc) -> Vec<Loc> {
    let mut all: Vec<Loc> = Vec::new();
    let mut c = Some(start.clone());
    while let Some(x) = c {
        if !ws_node(&x) {
            all.push(x.clone());
        }
        c = x.right_raw();
    }
    // partition-by sexpr-able?
    let mut groups: Vec<Vec<Loc>> = Vec::new();
    for l in all {
        match groups.last_mut() {
            Some(g) if sexpr_able(&g[0]) == sexpr_able(&l) => g.push(l),
            _ => groups.push(vec![l]),
        }
    }
    let mut it = groups.into_iter();
    let first = it.next().unwrap_or_default();
    match it.next() {
        Some(second) => {
            let mut v = first;
            v.push(second[0].clone());
            v
        }
        None => first.into_iter().take(1).collect(),
    }
}

fn find_zlocs_in_range(start: &Loc, row: u32, col: u32, end_row: u32, end_col: u32) -> Vec<Loc> {
    let has_sel = !single_cursor(row, col, end_row, end_col);
    let token = start.tag() == Tag::Token;
    let expr_start = next_expr_start(start, !has_sel, end_row, end_col);
    let inside = |l: &Loc| !past(l, end_row, end_col);
    if !has_sel && (token || expr_start.is_none() || next_not_executable(start, expr_start.as_ref().unwrap())) {
        return locate_parent_expr(start).into_iter().collect();
    }
    if has_sel && expr_start.is_none() {
        return vec![start.clone()];
    }
    let es = expr_start.unwrap();
    if !has_sel {
        return collect_to_first_sexpr(&es);
    }
    let mut exprs = Vec::new();
    let mut c = Some(es.clone());
    while let Some(x) = c {
        if !ws_node(&x) {
            if !inside(&x) {
                break;
            }
            exprs.push(x.clone());
        }
        c = x.right_raw();
    }
    if exprs.is_empty() { vec![es] } else { exprs }
}

const THREAD_FIRST: [&str; 2] = ["->", "some->"];
const THREAD_LAST: [&str; 2] = ["->>", "some->>"];

fn sym_in(l: Option<Loc>, set: &[&str]) -> bool {
    l.map_or(false, |l| l.node.is_sym() && set.contains(&l.node.text.as_str()))
}

fn combine_ranges(sel: &[Loc]) -> Option<Meta> {
    let ml = sel.last()?.meta()?;
    let mf = sel.first()?.meta()?;
    if let Some(pw) = sel[0].left_raw() {
        let pm = pw.meta()?;
        Some(Meta { row: pm.end_row, col: pm.end_col, end_row: ml.end_row, end_col: ml.end_col })
    } else {
        Some(Meta { row: mf.row, col: mf.col, end_row: ml.end_row, end_col: ml.end_col })
    }
}

fn thread_first_exprs(sel: &[Loc]) -> bool {
    sym_in(sel[0].leftmost(), &THREAD_FIRST) && (sel.len() > 1 || !sym_in(sel[0].left(), &THREAD_FIRST))
}
fn thread_last_exprs(sel: &[Loc]) -> bool {
    sym_in(sel[0].leftmost(), &THREAD_LAST) && (sel.len() > 1 || !sym_in(sel[0].left(), &THREAD_LAST))
}

fn is_thread_sym(n: &NR) -> bool {
    n.is_sym() && matches!(n.text.as_str(), "->" | "->>" | "some->" | "some->>")
}

fn selected_thread_op(sel: &[Loc]) -> bool {
    sel.iter().find(|l| !is_wsc(l.tag())).map_or(false, |l| is_thread_sym(&l.node))
}

fn selected_first_threaded_expr(sel: &[Loc]) -> bool {
    let exprs: Vec<&Loc> = if selected_thread_op(sel) { sel.iter().skip_while(|l| is_wsc(l.tag())).skip(1).collect() } else { sel.iter().collect() };
    let first = exprs.iter().find(|l| !is_wsc(l.tag()));
    first.and_then(|f| f.left()).map_or(false, |l| is_thread_sym(&l.node))
}

fn trim_comment_nodes(nodes: &[NR]) -> Vec<NR> {
    nodes
        .iter()
        .map(|n| {
            if n.tag == Tag::Comment {
                let s = n.string();
                let t = s.trim_end_matches(|c| c == '\n' || c == '\r');
                leaf(Tag::Comment, super::tree::Tk::None, t)
            } else {
                n.clone()
            }
        })
        .collect()
}

fn insert_comment_nodes_right(z: Loc, indent: usize, comments: &[NR]) -> Option<Loc> {
    if comments.is_empty() {
        return Some(z);
    }
    let mut acc = z.insert_newline_right(1).right_raw()?;
    for c in comments {
        acc = acc.insert_space_right(indent).right_raw()?;
        acc = acc.insert_right_raw(c.clone()).right_raw()?;
        acc = acc.insert_newline_right(1).right_raw()?;
        acc = acc.insert_space_right(indent).right_raw()?;
    }
    Some(acc)
}

fn wrap_with_threading(body: &[NR], op: &str, includes_initial: bool, initial_symbol: &str) -> Option<Loc> {
    let trimmed = trim_comment_nodes(body);
    // trim-thead-macro: drop leading thread symbols
    let skip = trimmed.iter().take_while(|n| !is_printable_only(n.tag) && is_thread_sym(n)).count();
    let without_macro: Vec<NR> = trimmed[skip..].to_vec();
    let (up_to_first, without_first): (Vec<NR>, Vec<NR>) = if includes_initial {
        let a = without_macro.iter().take_while(|n| is_wsc(n.tag)).cloned().collect::<Vec<_>>();
        let rest: Vec<NR> = without_macro.iter().skip_while(|n| is_wsc(n.tag)).skip(1).cloned().collect();
        (a, rest)
    } else {
        (vec![], without_macro.clone())
    };
    let starting = if includes_initial {
        without_macro.iter().find(|n| !is_printable_only(n.tag)).map(|n| n.string()).unwrap_or_default()
    } else {
        initial_symbol.to_string()
    };
    let expr = format!("({op} {starting})");
    let space_indent = 4 + op.chars().count();
    let start = of_string(&expr)?.down()?.right_raw()?;
    let start = insert_comment_nodes_right(start, space_indent, &up_to_first)?.right_raw()?;
    let mut acc = start;
    for n in &without_first {
        acc = acc.insert_newline_right(1).right_raw()?;
        acc = acc.insert_space_right(space_indent).right_raw()?;
        acc = acc.insert_right_raw(n.clone()).right_raw()?;
    }
    acc.up()
}

fn gen_thread_sym(used: &[String]) -> String {
    for c in ["t", "th", "thd", "x", "a", "b", "c", "d"] {
        if !used.iter().any(|u| u == c) {
            return c.to_string();
        }
    }
    "t1".to_string()
}

fn new_defn_zloc(name: &str, private: bool, use_meta: bool, params: &str, body: &[NR]) -> Option<Loc> {
    let text = if !private {
        format!("(defn {name})")
    } else if use_meta {
        format!("(defn ^:private {name})")
    } else {
        format!("(defn- {name})")
    };
    let root = of_string(&text)?;
    let params_node = leaf(Tag::Token, super::tree::Tk::Sym, params); // printed with pr-str
    let loc = root.append_child_raw(spaces(1)).append_child_raw(params_node);
    let mut acc = loc.down()?.right()?.right()?;
    for n in body {
        acc = acc.insert_right_raw(n.clone()).right_raw()?;
    }
    to_top(&acc)
}

pub fn extract_function(c: &Ctx, z: &Loc, z_end: Option<&Loc>, fn_name: &str) -> Out {
    let sel = find_zlocs_in_range(z, c.row, c.col, c.end_row, c.end_col);
    // check-for-errors
    if sel.iter().all(|l| is_wsc(l.tag())) {
        return Out::Err("No expressions to extract".into(), -32602);
    }
    if !single_cursor(c.row, c.col, c.end_row, c.end_col) {
        let end_up = match z_end {
            Some(e) => e.up(),
            None => to_top(z).and_then(|t| t.up()),
        };
        let start_up = z.up();
        let same = match (&start_up, &end_up) {
            (Some(a), Some(b)) => std::rc::Rc::ptr_eq(&a.node, &b.node),
            (None, None) => true,
            _ => false,
        };
        if !same {
            return Out::Err("Expressions must be at the same level".into(), -32602);
        }
    }
    {
        let rest: Vec<&Loc> = sel.iter().skip_while(|l| is_wsc(l.tag())).collect();
        if rest.len() == 1 && rest[0].node.is_sym() && matches!(rest[0].node.text.as_str(), "->" | "some->" | "->>" | "some->>") {
            return Out::Err("Can't extract a macro".into(), -32602);
        }
    }
    let Some(top_loc) = to_top(z) else { return Out::Nil };
    let private = true;
    let Some(range) = combine_ranges(&sel) else { return Out::Nil };
    let Some(f) = c.q.s.id(&c.uri) else { return Out::Nil };
    let an = An { q: c.q, f };
    let mut invoked: Vec<String> = Vec::new();
    for u in an.local_usages_outside(range) {
        let nm = c.q.name(u).as_str().to_string();
        if !invoked.contains(&nm) {
            invoked.push(nm);
        }
    }
    let body_nodes: Vec<NR> = sel.iter().map(|l| l.node.clone()).collect();
    let threaded = thread_first_exprs(&sel) || thread_last_exprs(&sel);
    let (new_body, used_syms): (Vec<NR>, Vec<String>) = if threaded {
        let first_call = thread_first_exprs(&sel);
        let last_call = thread_last_exprs(&sel);
        let sel_op = selected_thread_op(&sel);
        let sel_first = selected_first_threaded_expr(&sel);
        let tsym = gen_thread_sym(&invoked);
        let op = sel[0].leftmost().map(|l| l.node.text.clone()).unwrap_or_default();
        let Some(wl) = wrap_with_threading(&body_nodes, &op, sel_first, &tsym) else { return Out::Nil };
        let only = !sel_first && !sel_op;
        let used = if first_call && only {
            let mut v = vec![tsym.clone()];
            v.extend(invoked.iter().cloned());
            v
        } else if last_call && only {
            let mut v = invoked.clone();
            v.push(tsym.clone());
            v
        } else {
            invoked.clone()
        };
        (vec![wl.node.clone()], used)
    } else {
        (trim_comment_nodes(&body_nodes), invoked.clone())
    };
    let call = {
        let mut kids = vec![token_sym(fn_name)];
        for p in &invoked {
            kids.push(spaces(1));
            kids.push(token_sym(p));
        }
        list(kids).string()
    };
    let mut inter: Vec<NR> = Vec::new();
    for n in &new_body {
        inter.push(newlines(1));
        inter.push(spaces(2));
        inter.push(n.clone());
    }
    let params = format!("[{}]", used_syms.join(" "));
    let Some(new_defn) = new_defn_zloc(fn_name, private, c.settings.use_metadata_for_privacy, &params, &inter) else { return Out::Nil };
    let Some(e1) = prepend_preserving_comment(&top_loc, &new_defn) else { return Out::Nil };
    Out::Seq(vec![Edit { range: e1.range, text: e1.text }, Edit { range: Some(range), text: call }])
}
