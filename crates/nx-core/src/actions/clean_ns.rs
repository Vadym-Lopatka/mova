//! `feature/clean_ns.clj`: sort / prune the `ns` form (requires and imports).
use super::rz::*;
use super::transform::ZE;
use super::tree::{Meta, Tag, Tk};
use crate::analyzer::json::Json;
use crate::query::Q;
use std::collections::HashSet;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Indent {
    Keep,
    SameLine,
    NextLine,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SortMode {
    Off,
    Lexi,
    Default,
}

pub struct CleanSettings {
    pub sort_require: SortMode,
    pub sort_import: SortMode,
    pub sort_refer: SortMode,
    pub sort_import_classes: SortMode,
    pub sort_ns: bool,
    pub inner_indent: Indent,
    pub import_classes_indent: Indent,
    pub max_line_length: Option<i64>,
    pub classes_per_line: i64,
    pub auto_after_ns_refactor: bool,
    pub sort_import_bool: bool,
    pub sort_import_classes_bool: bool,
}

fn jget<'a>(j: Option<&'a Json>, path: &[&str]) -> Option<&'a Json> {
    let mut cur = j?;
    for p in path {
        cur = cur.get(p)?;
    }
    Some(cur)
}

fn sort_mode(j: Option<&Json>, path: &[&str]) -> SortMode {
    match jget(j, path) {
        None => SortMode::Default,
        Some(Json::Bool(false)) | Some(Json::Null) => SortMode::Off,
        Some(Json::Str(s)) if s == "lexicographically" || s == ":lexicographically" => SortMode::Lexi,
        Some(_) => SortMode::Default,
    }
}

fn indent_of(j: Option<&Json>, path: &[&str]) -> Option<Indent> {
    match jget(j, path)? {
        Json::Str(s) => match s.trim_start_matches(':') {
            "keep" => Some(Indent::Keep),
            "same-line" => Some(Indent::SameLine),
            "next-line" => Some(Indent::NextLine),
            _ => None,
        },
        _ => None,
    }
}

impl CleanSettings {
    pub fn from_json(j: Option<&Json>) -> CleanSettings {
        let keep_require_at_start = matches!(jget(j, &["keep-require-at-start?"]), Some(Json::Bool(true)));
        let inner = indent_of(j, &["clean", "ns-inner-blocks-indentation"]).unwrap_or(if keep_require_at_start { Indent::SameLine } else { Indent::Keep });
        let max_line = match jget(j, &["clean", "sort", "refer", "max-line-length"]) {
            Some(Json::Num(n)) => Some(*n as i64),
            Some(Json::Null) | Some(Json::Bool(false)) => None,
            _ => Some(80),
        };
        CleanSettings {
            sort_require: sort_mode(j, &["clean", "sort", "require"]),
            sort_import: sort_mode(j, &["clean", "sort", "import"]),
            sort_refer: sort_mode(j, &["clean", "sort", "refer"]),
            sort_import_classes: sort_mode(j, &["clean", "sort", "import-classes"]),
            sort_ns: !matches!(jget(j, &["clean", "sort", "ns"]), Some(Json::Bool(false))),
            inner_indent: inner,
            import_classes_indent: indent_of(j, &["clean", "ns-import-classes-indentation"]).unwrap_or(Indent::NextLine),
            max_line_length: max_line,
            classes_per_line: match jget(j, &["clean", "sort", "import-classes", "classes-per-line"]) {
                Some(Json::Num(n)) => *n as i64,
                _ => 3,
            },
            auto_after_ns_refactor: !matches!(jget(j, &["clean", "automatically-after-ns-refactor"]), Some(Json::Bool(false))),
            sort_import_bool: !matches!(jget(j, &["clean", "sort", "import"]), Some(Json::Bool(false)) | Some(Json::Null)),
            sort_import_classes_bool: !matches!(jget(j, &["clean", "sort", "import-classes"]), Some(Json::Bool(false)) | Some(Json::Null)),
        }
    }
}

pub struct Ctx<'a> {
    pub st: &'a CleanSettings,
    pub unused_aliases: HashSet<String>,
    pub unused_refers: HashSet<String>,
    pub unused_imports: HashSet<String>,
    pub duplicate_requires: HashSet<String>,
}

// ---- small zipper helpers --------------------------------------------------------------------------------

/// `(z/find-value loc z/next v)`.
fn find_value_next(l: &Loc, v: &str) -> Option<Loc> {
    l.find(&|x| Some(x.next()), &|x| x.tag() == Tag::Token && matches!(x.node.tk, Tk::Sym | Tk::Kw) && x.node.text == v)
}

/// `(z/find-next-value loc v)` with `z/right`.
fn find_next_value_right(l: &Loc, v: &str) -> Option<Loc> {
    let r = l.right()?;
    r.find(&|x| x.right(), &|x| x.tag() == Tag::Token && matches!(x.node.tk, Tk::Sym | Tk::Kw) && x.node.text == v)
}

fn child_sexprs(l: &Loc) -> Vec<NR> {
    l.node.kids.iter().filter(|k| !is_printable_only(k.tag)).cloned().collect()
}

fn find_namespace(z: &Loc) -> Option<Loc> {
    let mut c = z.clone();
    while let Some(u) = c.up_raw() {
        c = u;
    }
    find_value_next(&c, "ns")?.up()
}

/// `edit/map-children`: apply `f` to every non-whitespace node of the subtree in depth-first order.
fn map_children(parent: Loc, f: &dyn Fn(Loc) -> Loc) -> Loc {
    if parent.down().is_none() {
        return parent;
    }
    parent.subedit(|sub| {
        let mut loc = sub.down().unwrap();
        loop {
            if loc.is_end() {
                return loc;
            }
            let nxt = f(loc).next();
            if nxt.is_end() {
                return nxt;
            }
            loc = nxt;
        }
    })
}

fn meta_of(l: &Loc) -> Option<Meta> {
    l.meta()
}

// ---- interstitial / comment handling -------------------------------------------------------------------

fn libspec_attached_node(test: &Loc) -> bool {
    match test.left() {
        Some(l) => {
            let lm = l.meta().map(|m| m.end_row);
            let tm = test.meta().map(|m| m.row);
            lm == tm && l.tag() != Tag::Uneval
        }
        None => false,
    }
}

fn remove_same_line_comment(z: Loc) -> Loc {
    let next_right = z.find(&|l| l.right_raw(), &|l| matches!(l.tag(), Tag::Comment | Tag::Newline));
    match next_right {
        Some(c) if c.tag() == Tag::Comment => {
            let removed = c.remove_star();
            let n = removed.next();
            match n.left() {
                Some(l) => l,
                None => n,
            }
        }
        _ => z,
    }
}

fn remove_interstitial_nodes(libspec: Loc) -> Loc {
    let mut prev = libspec.clone();
    let mut test = libspec.left_raw();
    loop {
        let Some(t) = test.clone() else { return prev };
        if (t.tag() == Tag::Comment && !libspec_attached_node(&t)) || t.tag() == Tag::Uneval {
            if t.left_raw().is_none() {
                let r = t.remove_star();
                return r.down().unwrap_or(r);
            }
            let r = t.remove_star();
            prev = t;
            test = Some(r);
            continue;
        }
        if is_ws(t.tag()) {
            prev = t.clone();
            test = t.left_raw();
            continue;
        }
        return t.right().unwrap_or(t);
    }
}

fn remove_libspec_and_associated_comments(l: Loc) -> Loc {
    remove_interstitial_nodes(remove_same_line_comment(l)).remove()
}

// ---- requires ---------------------------------------------------------------------------------------------

fn sort_key_lower(s: &str) -> String {
    s.to_lowercase()
}

fn sort_items<T>(mode: SortMode, mut items: Vec<T>, key: &dyn Fn(&T) -> String, str_key: &dyn Fn(&T) -> String) -> Vec<T> {
    match mode {
        SortMode::Off => items,
        SortMode::Lexi => {
            items.sort_by(|a, b| super::clauses::cmp_clojure_str(&str_key(a), &str_key(b)));
            items
        }
        SortMode::Default => {
            items.sort_by(|a, b| super::clauses::cmp_clojure_str(&key(a), &key(b)));
            items
        }
    }
}

fn refer_node_with_add_new_lines(nodes: Vec<(NR, i64)>) -> Vec<(NR, bool)> {
    let mut out = Vec::new();
    for (i, (n, line)) in nodes.iter().enumerate() {
        let prev = if i == 0 { *line } else { nodes[i - 1].1 };
        out.push((n.clone(), prev != *line));
    }
    out
}

fn sort_refers_checking_new_lines(root: &Loc, initial_sep: usize, ctx: &Ctx, nodes: Vec<NR>) -> Option<Vec<NR>> {
    let key = |n: &NR| sort_key_lower(&n.text);
    let sorted = sort_items(ctx.st.sort_refer, nodes, &key, &|n: &NR| n.text.clone());
    let old = root.next().meta()?;
    let new_initial = 2 + initial_sep as i64;
    let refers_start = {
        let r = root.down()?;
        let refer = r.find(&|x| x.right(), &|x| x.tag() == Tag::Token && x.node.text == ":refer")?;
        refer.next().meta()?
    };
    let diff = if old.row == refers_start.row { new_initial - old.col as i64 } else { 0 };
    let init_refer_sep = 2 + refers_start.col as i64 + diff;
    let max_line = ctx.st.max_line_length;
    let max_chars = max_line.map(|m| m - init_refer_sep);
    let mut res: Vec<(NR, i64)> = Vec::new();
    let (mut line_len, mut cur_line) = (0i64, 0i64);
    for n in sorted {
        match (max_line, max_chars) {
            (Some(ml), Some(mc)) if ml > 0 => {
                let len = n.text.encode_utf16().count() as i64;
                let sep_len = if line_len == 0 { 0 } else { 1 };
                let nl = line_len + sep_len + len;
                if mc < nl {
                    line_len = len;
                    cur_line += 1;
                    res.push((n, cur_line));
                } else {
                    line_len = nl;
                    res.push((n, cur_line));
                }
            }
            _ => res.push((n, 0)),
        }
    }
    let with_nl = refer_node_with_add_new_lines(res);
    let mut out: Vec<NR> = Vec::new();
    for (n, add) in with_nl {
        if add {
            out.push(newlines(1));
            out.push(spaces((init_refer_sep - 2).max(0) as usize));
            out.push(n);
        } else {
            out.push(spaces(1));
            out.push(n);
        }
    }
    if !out.is_empty() {
        out.remove(0);
    }
    Some(out)
}

fn remove_unused_refers(node: Loc, initial_sep: usize, ctx: &Ctx) -> Option<Loc> {
    let refer = find_next_value_right(&node.down()?, ":refer")?;
    let refer_vec = refer.right()?;
    let unused_names: HashSet<String> = ctx.unused_refers.iter().map(|q| q.rsplit_once('/').map_or(q.clone(), |(_, n)| n.to_string())).collect();
    let removed: Vec<NR> = refer_vec.node.kids.iter().filter(|k| !is_printable_only(k.tag)).filter(|k| !unused_names.contains(&k.text)).cloned().collect();
    if removed.is_empty() {
        let ns_only = refer.remove().right()?.remove();
        let up = ns_only.up()?;
        if child_sexprs(&up).len() > 1 {
            Some(ns_only)
        } else {
            Some(up.remove())
        }
    } else {
        let sorted = sort_refers_checking_new_lines(&node, initial_sep, ctx, removed)?;
        let vec = inner(Tag::Vector, sorted);
        Some(refer_vec.replace(vec).up()?)
    }
}

fn remove_unused_duplicate_requires(node: Loc, ctx_uri_aliases: &dyn Fn(&str) -> bool) -> Loc {
    let alias = node.down().and_then(|d| find_next_value_right(&d, ":as")).and_then(|a| a.right()).map(|a| a.node.text.clone());
    match alias {
        Some(a) => {
            if ctx_uri_aliases(&a) { node } else { remove_libspec_and_associated_comments(node) }
        }
        None => node,
    }
}

fn remove_unused_require(node: Loc, initial_sep: usize, ctx: &Ctx, alias_used: &dyn Fn(&str) -> bool) -> Loc {
    if node.tag() != Tag::Vector {
        return node;
    }
    let ns_expr = node.down().and_then(|d| d.leftmost()).map(|l| l.node.text.clone()).unwrap_or_default();
    if ctx.unused_aliases.contains(&ns_expr) {
        return remove_libspec_and_associated_comments(node);
    }
    let has_refer_vec = node.down().and_then(|d| find_next_value_right(&d, ":refer")).and_then(|r| r.right()).map_or(false, |v| v.tag() == Tag::Vector);
    if has_refer_vec {
        return remove_unused_refers(node.clone(), initial_sep, ctx).unwrap_or(node);
    }
    if ctx.duplicate_requires.contains(&ns_expr) {
        return remove_unused_duplicate_requires(node, alias_used);
    }
    node
}

fn remove_unused_requires(nodes: Loc, ctx: &Ctx, initial_sep: usize, alias_used: &dyn Fn(&str) -> bool) -> Loc {
    let single = child_sexprs(&nodes).len() == 1;
    let first = nodes.next();
    let first = if first.is_end() { None } else { Some(first) };
    let first_is_vec = first.as_ref().map_or(false, |f| f.tag() == Tag::Vector);
    let first_ns = if single && first_is_vec { first.as_ref().and_then(|f| f.down()).and_then(|d| d.leftmost()).map(|l| l.node.text.clone()) } else { None };
    let first_refers: Option<HashSet<String>> = if single && first_is_vec {
        let f = first.as_ref().unwrap();
        let has_all = f.down().and_then(|d| find_next_value_right(&d, ":all")).is_some();
        if !has_all {
            let refers = f.down().and_then(|d| find_next_value_right(&d, ":refer")).and_then(|r| r.right());
            refers.map(|r| r.node.kids.iter().filter(|k| !is_printable_only(k.tag)).map(|k| format!("{}/{}", first_ns.clone().unwrap_or_default(), k.text)).collect())
        } else {
            Some(HashSet::new())
        }
    } else {
        None
    };
    let single_unused = if single && first_is_vec {
        let ns = first_ns.clone().unwrap_or_default();
        ctx.unused_aliases.contains(&ns)
            || first_refers.as_ref().map_or(false, |fr| !fr.is_empty() && !ctx.unused_refers.is_empty() && fr.is_subset(&ctx.unused_refers))
    } else {
        false
    };
    if single_unused {
        return remove_unused_require(first.unwrap(), initial_sep, ctx, alias_used);
    }
    map_children(nodes, &|n| remove_unused_require(n, initial_sep, ctx, alias_used))
}

fn inner_indentation_parent_col(parent: &Loc, ctx: &Ctx) -> usize {
    let v = match ctx.st.inner_indent {
        Indent::SameLine => parent.meta().map(|m| m.end_col as i64),
        Indent::NextLine => parent.meta().map(|m| m.col as i64 - 1),
        Indent::Keep => parent.right().and_then(|r| r.meta()).map(|m| m.col as i64 - 1),
    };
    v.unwrap_or(2).max(0) as usize
}

fn calc_keep_first_line_spacing(l: &Loc) -> Option<i64> {
    let rm = l.meta()?;
    let right = l.right()?.meta()?;
    if rm.row == right.row { Some(right.col as i64 - rm.end_col as i64) } else { None }
}

// ---- process-clean-ns -----------------------------------------------------------------------------------

fn comment_or_discard(n: &NR) -> bool {
    n.tag == Tag::Comment || n.tag == Tag::Uneval
}

fn remove_empty_reader_conditional(new: Loc) -> Loc {
    let rm = new.up().and_then(|u| u.up()).map_or(false, |u| u.tag() == Tag::ReaderMacro);
    if !rm {
        return new;
    }
    let up = new.up().unwrap();
    let cnt = up.node.kids.iter().filter(|k| !is_printable_only(k.tag)).count();
    let empty = cnt <= 1 || (matches!(new.tag(), Tag::Vector | Tag::List) && new.node.kids.iter().all(|k| is_printable_only(k.tag)));
    if empty {
        up.up().map(|u| u.remove()).unwrap_or(new)
    } else {
        new
    }
}

#[derive(Clone)]
struct Assoc {
    node: NR,
    after: Option<NR>,
    before: Vec<NR>,
}

fn node_row(n: &NR) -> Option<u32> {
    n.meta.map(|m| m.row)
}
fn node_end_row(n: &NR) -> Option<u32> {
    n.meta.map(|m| m.end_row)
}

fn build_assoc(pre: &[NR], idx: usize) -> Assoc {
    let node = pre[idx].clone();
    let after = pre.get(idx + 1).filter(|r| r.tag == Tag::Comment && node_row(r) == node_end_row(&node)).cloned();
    // find-interstitial-nodes
    let prev_idx = (0..idx).rev().find(|&i| !is_printable_only(pre[i].tag));
    let prev_row: i64 = prev_idx.and_then(|i| node_end_row(&pre[i])).map_or(-1, |r| r as i64);
    let mut before: Vec<NR> = Vec::new();
    let mut n = idx as i64 - 1;
    while n >= 0 && comment_or_discard(&pre[n as usize]) && node_row(&pre[n as usize]).map_or(false, |r| r as i64 > prev_row) {
        before.insert(0, pre[n as usize].clone());
        n -= 1;
    }
    Assoc { node, after, before }
}

fn sort_key(a: &Assoc) -> String {
    let n = &a.node;
    let has_assoc = a.after.is_some() || !a.before.is_empty();
    let k = |n: &NR| -> String {
        if n.tag == Tag::Token {
            return n.text.to_lowercase();
        }
        if matches!(n.tag, Tag::Vector | Tag::List) {
            if let Some(first) = n.kids.iter().find(|k| !is_printable_only(k.tag)) {
                return first.string().to_lowercase();
            }
        }
        if n.tag == Tag::ReaderMacro {
            return "0".to_string();
        }
        // (some-> node n/sexpr first lower-case)
        n.kids.iter().find(|k| !is_printable_only(k.tag)).map(|f| f.string().to_lowercase()).unwrap_or_default()
    };
    let _ = has_assoc;
    // association vectors sort on the embedded namespace, i.e. the libspec's first element
    if a.after.is_some() || !a.before.is_empty() {
        return match n.kids.iter().find(|k| !is_printable_only(k.tag)) {
            Some(first) if matches!(n.tag, Tag::Vector | Tag::List) => first.string().to_lowercase(),
            _ => k(n),
        };
    }
    k(n)
}

fn str_key(a: &Assoc) -> String {
    a.node.string()
}

enum Out {
    N(NR),
    Attached(NR),
}

impl Out {
    fn node(&self) -> &NR {
        match self {
            Out::N(n) | Out::Attached(n) => n,
        }
    }
}

fn process_clean_ns(ns_loc: Loc, remaining: Loc, col: usize, keep_first: Option<i64>, form_type: &str, ctx: &Ctx) -> Option<Loc> {
    let nodes_to_sort = map_children(remaining, &remove_empty_reader_conditional);
    let sep = spaces(col);
    let single = spaces(1);
    let pre: Vec<NR> = nodes_to_sort.node.kids.iter().filter(|k| !is_ws(k.tag)).cloned().collect();
    let assocs: Vec<Assoc> = (0..pre.len()).filter(|&i| !is_printable_only(pre[i].tag)).map(|i| build_assoc(&pre, i)).collect();
    let mode = if form_type == ":require" { ctx.st.sort_require } else { ctx.st.sort_import };
    let sorted = sort_items(mode, assocs, &sort_key, &str_key);
    let mut flat: Vec<Out> = Vec::new();
    for a in sorted {
        match (&a.after, a.before.is_empty()) {
            (Some(c), true) => {
                flat.push(Out::N(a.node.clone()));
                flat.push(Out::Attached(c.clone()));
            }
            (Some(c), false) => {
                flat.extend(a.before.iter().cloned().map(Out::N));
                flat.push(Out::N(a.node.clone()));
                flat.push(Out::Attached(c.clone()));
            }
            (None, false) => {
                flat.extend(a.before.iter().cloned().map(Out::N));
                flat.push(Out::N(a.node.clone()));
            }
            (None, true) => flat.push(Out::N(a.node.clone())),
        }
    }
    let mut formatted: Vec<NR> = vec![keyword(form_type)];
    for (idx, o) in flat.iter().enumerate() {
        let node = o.node();
        let prev_comment = idx > 0 && flat[idx - 1].node().tag == Tag::Comment;
        if comment_or_discard(node) && idx == 0 && !matches!(o, Out::Attached(_)) {
            formatted.extend([newlines(1), sep.clone(), node.clone()]);
        } else if matches!(o, Out::Attached(_)) {
            formatted.extend([single.clone(), node.clone()]);
        } else if comment_or_discard(node) {
            if prev_comment {
                formatted.extend([sep.clone(), node.clone()]);
            } else {
                formatted.extend([newlines(1), sep.clone(), node.clone()]);
            }
        } else if ctx.st.inner_indent == Indent::SameLine && idx == 0 {
            formatted.extend([single.clone(), node.clone()]);
        } else if ctx.st.inner_indent == Indent::Keep && keep_first.is_some() && idx == 0 {
            formatted.extend([spaces(keep_first.unwrap().max(0) as usize), node.clone()]);
        } else if prev_comment {
            formatted.extend([sep.clone(), node.clone()]);
        } else {
            formatted.extend([newlines(1), sep.clone(), node.clone()]);
        }
    }
    let empty = child_sexprs(&nodes_to_sort).is_empty();
    let node_meta = nodes_to_sort.node.meta;
    Some(ns_loc.subedit(|sub| {
        let found = find_value_next(&sub, form_type).and_then(|l| l.up());
        match found {
            Some(l) => {
                if empty {
                    l.remove()
                } else {
                    l.replace(with_meta_of(&inner(Tag::List, formatted.clone()), node_meta))
                }
            }
            None => sub,
        }
    }))
}

fn clean_requires(ns_loc: Loc, ctx: &Ctx, alias_used: &dyn Fn(&str) -> bool) -> Loc {
    let sub = ns_loc.subzip();
    let Some(require_loc) = find_value_next(&sub, ":require") else { return ns_loc };
    let col = inner_indentation_parent_col(&require_loc, ctx);
    let keep_first = calc_keep_first_line_spacing(&require_loc);
    let removed = remove_unused_requires(require_loc.remove(), ctx, col, alias_used);
    process_clean_ns(ns_loc.clone(), removed, col, keep_first, ":require", ctx).unwrap_or(ns_loc)
}

// ---- imports ---------------------------------------------------------------------------------------------

fn package_import(node: &Loc) -> bool {
    matches!(node.tag(), Tag::Vector | Tag::List) && !matches!(node.next().tag(), Tag::Vector | Tag::List)
}

fn remove_unused_package_import(node: Loc, base_package: &str, unused: &HashSet<String>) -> Loc {
    if node.string().contains('.') {
        return node;
    }
    if unused.contains(&format!("{}.{}", base_package, node.string())) {
        return node.remove();
    }
    node
}

fn sorting_package_import_classes(parent: &Loc, ctx: &Ctx, import_loc: &Loc, base_package: &str, classes: Vec<String>, node: &Loc) -> Option<Loc> {
    let parent_col = inner_indentation_parent_col(import_loc, ctx);
    let is_list = parent.tag() == Tag::List;
    let sorted = sort_items(ctx.st.sort_import_classes, classes, &|c: &String| c.clone(), &|c: &String| c.clone());
    let per_line = ctx.st.classes_per_line;
    let move_classes = per_line != -1 && sorted.len() as i64 > per_line;
    let mut nodes: Vec<NR> = Vec::new();
    if !move_classes {
        nodes.push(token_sym(base_package));
        for c in &sorted {
            nodes.push(spaces(1));
            nodes.push(token_sym(c));
        }
    } else if ctx.st.import_classes_indent == Indent::NextLine {
        nodes.push(token_sym(base_package));
        for c in &sorted {
            nodes.push(newlines(1));
            nodes.push(spaces(parent_col + 1));
            nodes.push(token_sym(c));
        }
    } else {
        nodes.push(token_sym(base_package));
        nodes.push(spaces(1));
        nodes.push(token_sym(&sorted[0]));
        let end_col = parent.down().and_then(|d| d.leftmost()).and_then(|l| l.meta()).map_or(0, |m| m.end_col) as usize;
        for c in &sorted[1..] {
            nodes.push(newlines(1));
            nodes.push(spaces(end_col));
            nodes.push(token_sym(c));
        }
    }
    let n = inner(if is_list { Tag::List } else { Tag::Vector }, nodes);
    Some(node.replace(n))
}

fn remove_unused_import(parent: Loc, import_loc: &Loc, ctx: &Ctx) -> Loc {
    if parent.tag() == Tag::Uneval {
        return parent;
    }
    if package_import(&parent) {
        let base = parent.down().and_then(|d| d.leftmost()).map(|l| l.string()).unwrap_or_default();
        let removed = map_children(parent.clone(), &|n| remove_unused_package_import(n, &base, &ctx.unused_imports));
        let child_exprs = child_sexprs(&removed);
        let classes: Vec<String> = child_exprs.iter().skip(1).map(|c| c.text.clone()).collect();
        let remove_whole = child_exprs.len() == 1;
        let node = if remove_whole { remove_libspec_and_associated_comments(removed) } else { removed };
        let first_kw = parent.down().map_or(false, |d| d.node.is_kw() && matches!(d.node.text.as_str(), ":clj" | ":cljs"));
        let up_next_uneval = parent.up().and_then(|u| Some(u.next())).map_or(false, |n| !n.is_end() && n.tag() == Tag::Uneval);
        let up_next_kw = parent.up().map(|u| u.next()).map_or(false, |n| !n.is_end() && n.node.is_kw() && matches!(n.node.text.as_str(), ":clj" | ":cljs"));
        let upup_next_kw = parent.up().and_then(|u| u.up()).map(|u| u.next()).map_or(false, |n| !n.is_end() && n.node.is_kw() && matches!(n.node.text.as_str(), ":clj" | ":cljs"));
        if ctx.st.sort_import_bool
            && ctx.st.sort_import_classes_bool
            && !remove_whole
            && !first_kw
            && !up_next_uneval
            && !up_next_kw
            && !upup_next_kw
            && classes.len() > 1
        {
            return sorting_package_import_classes(&parent, ctx, import_loc, &base, classes, &node).unwrap_or(node);
        }
        return node;
    }
    if ctx.unused_imports.contains(&parent.node.text) && parent.node.is_sym() {
        return remove_libspec_and_associated_comments(parent);
    }
    parent
}

fn clean_imports(ns_loc: Loc, ctx: &Ctx) -> Loc {
    let sub = ns_loc.subzip();
    let Some(import_loc) = find_value_next(&sub, ":import") else { return ns_loc };
    let col = inner_indentation_parent_col(&import_loc, ctx);
    let keep_first = calc_keep_first_line_spacing(&import_loc);
    let removed = map_children(import_loc.remove(), &|n| remove_unused_import(n, &import_loc, ctx));
    process_clean_ns(ns_loc.clone(), removed, col, keep_first, ":import", ctx).unwrap_or(ns_loc)
}

fn sort_ns_children(ns_loc: Loc, ctx: &Ctx) -> Loc {
    if !ctx.st.sort_ns {
        return ns_loc;
    }
    // z/find-next-depth-first: starts one z/next after ns-loc
    let find_list = |kw: &str| {
        let start = ns_loc.next();
        if start.is_end() {
            return None;
        }
        start.find(&|x| Some(x.next()), &|x| x.tag() == Tag::List && {
            let n = x.next();
            !n.is_end() && n.node.is_kw() && n.node.text == kw
        })
    };
    let require_loc = find_list(":require");
    let import_loc = find_list(":import");
    if let (Some(r), Some(i)) = (require_loc, import_loc) {
        if let (Some(im), Some(rm)) = (i.meta(), r.meta()) {
            if im.row < rm.row {
                let rn = r.node.clone();
                let inn = i.node.clone();
                return ns_loc.subedit(|sub| {
                    let l = find_value_next(&sub, ":import").and_then(|l| l.up()).map(|l| l.replace(rn.clone()));
                    let l = l.and_then(|l| l.right());
                    let l = l.and_then(|l| find_value_next(&l, ":require")).and_then(|l| l.up()).map(|l| l.replace(inn.clone()));
                    l.unwrap_or(sub)
                });
            }
        }
    }
    ns_loc
}

pub struct Findings {
    pub unused_aliases: HashSet<String>,
    pub unused_refers: HashSet<String>,
    pub unused_imports: HashSet<String>,
    pub duplicate_requires: HashSet<String>,
}

/// `clean-ns-edits`: None = no-op (no ns form).
pub fn clean_ns_edits(loc: &Loc, st: &CleanSettings, fnd: Findings, alias_used: &dyn Fn(&str) -> bool) -> Option<Vec<ZE>> {
    let ns_loc = find_namespace(loc)?;
    let ctx = Ctx { st, unused_aliases: fnd.unused_aliases, unused_refers: fnd.unused_refers, unused_imports: fnd.unused_imports, duplicate_requires: fnd.duplicate_requires };
    let r = clean_requires(ns_loc, &ctx, alias_used);
    let r = clean_imports(r, &ctx);
    let r = sort_ns_children(r, &ctx);
    Some(vec![ZE { range: meta_of(&r), text: r.string() }])
}

#[allow(dead_code)]
fn _q(_: &Q) {}
