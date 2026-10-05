//! Literal ports of cljfmt/core.cljc passes over the linked tree (rewrite-clj zipper semantics).

use super::config::{CKey, CPart, Compiled, FmtConfig, FnArgIndent, Spec};
use super::tree::{Tag, Tree, NONE};
use std::collections::HashMap;

// ---------- context (alias/refer maps, ns name) ----------

#[derive(Default)]
pub(crate) struct Ctx {
    pub alias: HashMap<String, String>,
    pub refer: HashMap<String, String>,
    pub ns_name: Option<String>,
}

#[derive(Clone, Copy)]
struct SymInfo<'a> {
    ns: Option<&'a str>,
    name: &'a str,
}

fn split_sym(s: &str) -> SymInfo<'_> {
    if s == "/" {
        return SymInfo { ns: None, name: s };
    }
    match s.find('/') {
        Some(i) if i + 1 < s.len() => SymInfo { ns: Some(&s[..i]), name: &s[i + 1..] },
        _ => SymInfo { ns: None, name: s },
    }
}

// ---------- generic edit-all ----------

fn edit_all(t: &mut Tree, root: u32, pred: &dyn Fn(&Tree, u32) -> bool, f: &mut dyn FnMut(&mut Tree, u32) -> u32) {
    let mut z = root;
    if pred(t, z) {
        z = f(t, z);
    }
    'outer: loop {
        let mut c = t.next_star(z);
        while let Some(x) = c {
            if pred(t, x) {
                z = f(t, x);
                continue 'outer;
            }
            c = t.next_star(x);
        }
        return;
    }
}

#[inline]
fn tg(t: &Tree, n: Option<u32>) -> Option<Tag> {
    n.map(|x| t.tag(x))
}

fn is_comment(t: &Tree, n: Option<u32>) -> bool {
    tg(t, n) == Some(Tag::Comment)
}

fn node_len(t: &Tree, n: u32) -> u32 {
    let nd = &t.n[n as usize];
    if nd.fill > 0 { nd.fill } else { t.utf16_len(t.text(n)) }
}

// ---------- remove-consecutive-blank-lines ----------

fn count_newlines(t: &Tree, n: u32) -> u32 {
    let mut total = 0;
    let mut cur = Some(n);
    while let Some(c) = cur {
        if t.tag(c) != Tag::Newline {
            break;
        }
        total += node_len(t, c);
        // skip-whitespace-and-commas defaults to z/next* (depth-first, crosses container ends)
        let mut x = t.right(c);
        while let Some(y) = x {
            if matches!(t.tag(y), Tag::Space | Tag::Comma) {
                x = t.next_star(y);
            } else {
                break;
            }
        }
        cur = x;
    }
    let mut x = Some(n);
    while let Some(y) = x {
        if t.tag(y).is_ws() {
            x = t.left(y);
        } else {
            break;
        }
    }
    if is_comment(t, x) { total + 1 } else { total }
}

fn final_transform_element(t: &Tree, n: u32) -> bool {
    let mut x = t.next_star(n);
    while let Some(y) = x {
        if !t.tag(y).is_ws() {
            return false;
        }
        x = t.next_star(y);
    }
    true
}

pub fn remove_consecutive_blank_lines(t: &mut Tree, root: u32) {
    edit_all(
        t,
        root,
        &|t, n| t.tag(n) == Tag::Newline && count_newlines(t, n) > 2 && !final_transform_element(t, n),
        &mut |t, n| {
            let mut y = n;
            while t.tag(y).is_ws() {
                y = t.next_star(y).unwrap();
            }
            let mut loc = t.prev_star(y).unwrap();
            while t.tag(loc).is_ws() {
                loc = t.remove(loc);
            }
            let k = if t.tag(loc) == Tag::Comment { 1 } else { 2 };
            let nx = t.next_star(loc).unwrap();
            let nl = t.synth(Tag::Newline, k);
            t.insert_left(nx, nl);
            nx
        },
    );
}

// ---------- remove-surrounding-whitespace ----------

pub fn remove_surrounding_whitespace(t: &mut Tree, root: u32) {
    edit_all(
        t,
        root,
        &|t, n| {
            if !t.tag(n).is_ws() || t.is_top(n) {
                return false;
            }
            if t.left(n).is_none() {
                let r = t.right(n);
                let unquote_deref = match r {
                    Some(r) => t.tag(r) == Tag::Deref && tg(t, t.up(r)) == Some(Tag::Unquote),
                    None => false,
                };
                if !unquote_deref && !is_comment(t, r) {
                    return true;
                }
            }
            let mut x = Some(n);
            while let Some(y) = x {
                if !t.tag(y).is_ws() {
                    return false;
                }
                x = t.right(y);
            }
            true
        },
        &mut |t, n| t.remove(n),
    );
}

// ---------- insert-missing-whitespace ----------

pub fn insert_missing_whitespace(t: &mut Tree, root: u32) {
    edit_all(
        t,
        root,
        &|t, n| {
            if t.is_wsc(n) {
                return false;
            }
            if let Some(p) = t.up(n) {
                if matches!(t.tag(p), Tag::ReaderMacro | Tag::NsMap) {
                    return false;
                }
            }
            match t.right(n) {
                Some(r) => !t.is_wsc(r),
                None => false,
            }
        },
        &mut |t, n| {
            let s = t.synth(Tag::Space, 1);
            t.insert_right(n, s);
            n
        },
    );
}

// ---------- remove-multiple-non-indenting-spaces ----------

fn indentation(t: &Tree, n: u32) -> bool {
    t.tag(n) == Tag::Space && matches!(tg(t, t.left(n)), Some(Tag::Newline) | Some(Tag::Comment))
}

pub fn remove_multiple_non_indenting_spaces(t: &mut Tree, root: u32) {
    edit_all(
        t,
        root,
        &|t, n| t.tag(n) == Tag::Space && !indentation(t, n) && !is_comment(t, t.right(n)),
        &mut |t, n| {
            let nd = &mut t.n[n as usize];
            nd.fill = 1;
            n
        },
    );
}

// ---------- remove-trailing-whitespace ----------

pub fn remove_trailing_whitespace(t: &mut Tree, root: u32) {
    edit_all(
        t,
        root,
        &|t, n| {
            if t.tag(n) != Tag::Space {
                return false;
            }
            match t.right(n) {
                Some(r) => t.tag(r) == Tag::Newline,
                None => matches!(t.up(n), Some(p) if t.is_root(p)),
            }
        },
        &mut |t, n| t.remove(n),
    );
}

// ---------- unindent ----------

fn skip_spaces_next(t: &Tree, n: u32) -> Option<u32> {
    let mut x = t.next_star(n);
    while let Some(y) = x {
        if t.tag(y) == Tag::Space {
            x = t.next_star(y);
        } else {
            return Some(y);
        }
    }
    None
}

fn line_comment(t: &Tree, n: u32) -> bool {
    let s = t.text(n).as_bytes();
    s.len() >= 2 && s[0] == b';' && s[1] == b';' && (s.len() == 2 || s[2] != b';')
}

fn comment_next_ok(t: &Tree, n: u32, cfg: &FmtConfig) -> bool {
    // true when indenting/unindenting is allowed (not followed by a blocking comment)
    let nx = skip_spaces_next(t, n);
    if cfg.indent_line_comments {
        match nx {
            Some(z) => !(t.tag(z) == Tag::Comment && !line_comment(t, z)),
            None => true,
        }
    } else {
        !is_comment(t, nx)
    }
}

pub fn unindent(t: &mut Tree, root: u32, cfg: &FmtConfig) {
    edit_all(t, root, &|t, n| indentation(t, n) && comment_next_ok(t, n, cfg), &mut |t, n| t.remove(n));
}

// ---------- indent ----------

pub(crate) struct Indenter<'a> {
    pub cfg: &'a FmtConfig,
    pub cr: &'a Compiled,
    pub ctx: &'a Ctx,
    /// Set when cljfmt would throw (regex key part applied to a nil namespace -> NPE on the JVM).
    pub failed: std::cell::Cell<bool>,
}

fn skip_meta(t: &Tree, n: u32) -> Option<u32> {
    if matches!(t.tag(n), Tag::Meta | Tag::MetaStar) {
        let d = t.down_sig(n)?;
        t.right_sig(d)
    } else {
        Some(n)
    }
}

fn reader_conditional(t: &Tree, n: u32) -> bool {
    if t.tag(n) != Tag::ReaderMacro {
        return false;
    }
    match t.down_sig(n) {
        Some(d) => t.tag(d) == Tag::Sym && matches!(t.text(d), "?" | "?@"),
        None => false,
    }
}

fn sym_info<'a>(t: &Tree<'a>, n: u32) -> Option<SymInfo<'a>> {
    if t.tag(n) == Tag::Sym { Some(split_sym(t.text(n))) } else { None }
}

fn first_symbol_in_rc<'a>(t: &Tree<'a>, l: u32) -> Option<SymInfo<'a>> {
    if !reader_conditional(t, l) {
        return None;
    }
    let d = t.down_sig(l)?;
    let lst = t.right_sig(d)?;
    let mut x = t.down_sig(lst);
    while let Some(y) = x {
        if t.tag(y) == Tag::Kw {
            let v = t.next_sig(y)?;
            let v = skip_meta(t, v)?;
            return sym_info(t, v);
        }
        x = t.right_sig(y);
    }
    None
}

fn form_symbol<'a>(t: &Tree<'a>, n: u32) -> Option<SymInfo<'a>> {
    let l = t.leftmost_sig(n)?;
    if let Some(x) = skip_meta(t, l) {
        if t.tag(x) == Tag::Sym {
            return sym_info(t, x);
        }
        if t.tag(x).is_token() && !matches!(t.text(x), "nil" | "false") {
            return None;
        }
    }
    first_symbol_in_rc(t, l)
}

fn index_of(t: &Tree, n: u32) -> i32 {
    t.n[n as usize].sidx - (t.tag(n) == Tag::Uneval) as i32
}

fn first_form_in_line(t: &Tree, n: u32) -> bool {
    let mut x = n;
    loop {
        match t.left(x) {
            None => return true,
            Some(l) => {
                if t.tag(l) == Tag::Space {
                    x = l;
                } else {
                    return matches!(t.tag(l), Tag::Newline | Tag::Comment);
                }
            }
        }
    }
}

fn nth_form(t: &Tree, z: u32, n: usize) -> Option<u32> {
    let mut cur = t.leftmost_sig(z)?;
    for _ in 0..n {
        cur = t.right_sig(cur)?;
    }
    Some(cur)
}

impl<'a> Indenter<'a> {
    fn col(&self, t: &Tree, n: u32) -> u32 {
        t.n[n as usize].col
    }
    fn coll_indent(&self, t: &Tree, z: u32) -> u32 {
        self.col(t, t.leftmost(z))
    }
    fn two_space(&self, t: &Tree, z: u32) -> bool {
        match self.cfg.function_arguments_indentation {
            FnArgIndent::Community => false,
            FnArgIndent::Cursive => {
                let l = t.leftmost(z);
                match skip_meta(t, l) {
                    Some(x) => !matches!(t.tag(x), Tag::Vector | Tag::Map | Tag::List | Tag::Set),
                    None => true,
                }
            }
            FnArgIndent::Zprint => {
                let k = t.tag(t.leftmost(z));
                k.is_token() || k == Tag::List
            }
        }
    }
    fn list_indent(&self, t: &Tree, z: u32) -> u32 {
        if index_of(t, z) > 1 {
            let l = t.leftmost(z);
            t.right_sig(l).map(|r| self.col(t, r)).unwrap_or(0)
        } else {
            self.coll_indent(t, z) + self.two_space(t, z) as u32
        }
    }
    fn full_sym(&self, i: &SymInfo) -> Option<(String, String)> {
        if let Some(ns) = i.ns {
            let r = self.ctx.alias.get(ns).map(|s| s.as_str()).unwrap_or(ns);
            return Some((r.to_string(), i.name.to_string()));
        }
        if let Some(ns) = self.ctx.refer.get(i.name) {
            return Some((ns.clone(), i.name.to_string()));
        }
        self.ctx.ns_name.as_ref().map(|n| (n.clone(), i.name.to_string()))
    }
    fn key_matches(&self, i: &SymInfo, key: &CKey) -> bool {
        match key {
            CKey::Vec(a, b) => {
                let full = self.full_sym(i);
                let sym_ns: Option<String> = full.map(|f| f.0).or_else(|| i.ns.map(|s| s.to_string()));
                let part = |p: &CPart, s: Option<&str>| match (p, s) {
                    (CPart::Name(n), Some(s)) => n == s,
                    (CPart::Re(r), Some(s)) => r.find(s),
                    (CPart::Re(_), None) => {
                        self.failed.set(true);
                        false
                    }
                    _ => false,
                };
                part(a, sym_ns.as_deref()) && part(b, Some(i.name))
            }
            CKey::Re(p) => p.find(i.name),
            CKey::Qual(ns, name) => match self.full_sym(i) {
                Some((n, m)) => &n == ns && &m == name,
                None => false,
            },
            CKey::Sym(n) => n == i.name,
        }
    }
    fn matches(&self, t: &Tree, node: u32, key: &CKey) -> bool {
        match form_symbol(t, node) {
            Some(i) => self.key_matches(&i, key),
            None => false,
        }
    }
    fn nth_up(&self, t: &Tree, z: u32, d: usize) -> Option<u32> {
        let mut x = z;
        for _ in 0..d {
            x = t.up(x)?;
        }
        Some(x)
    }
    fn inner_indent(&self, t: &Tree, z: u32, key: &CKey, depth: usize, idx: Option<usize>) -> Option<u32> {
        let top = self.nth_up(t, z, depth)?;
        if t.left_sig(z).is_some()
            && self.matches(t, top, key)
            && (idx.is_none() || (depth > 0 && (idx.unwrap() as i32 + 1) == index_of(t, top)))
        {
            let zup = t.up(z)?;
            let w = match t.tag(zup) {
                Tag::List => 2,
                Tag::Fn => 3,
                _ => return None,
            };
            Some(self.col(t, zup) + w)
        } else {
            None
        }
    }
    fn block_indent(&self, t: &Tree, z: u32, key: &CKey, idx: usize) -> Option<u32> {
        if !self.matches(t, z, key) {
            return None;
        }
        let after = nth_form(t, z, idx + 1);
        if (after.is_none() || first_form_in_line(t, after.unwrap())) && index_of(t, z) > idx as i32 {
            self.inner_indent(t, z, key, 0, None)
        } else {
            Some(self.list_indent(t, z))
        }
    }
    fn indenter(&self, t: &Tree, z: u32) -> Option<u32> {
        let cr = self.cr;
        if cr.rules.is_empty() {
            return None;
        }
        // candidate pruning by symbol name at each depth (superset of rules that can match)
        let mut cand: Vec<u32> = Vec::new();
        let mut top = Some(z);
        for d in 0..=cr.max_depth {
            let Some(tp) = top else { break };
            if let Some(i) = form_symbol(t, tp) {
                if let Some(v) = cr.by_name.get(i.name) {
                    cand.extend_from_slice(v);
                }
            }
            if d < cr.max_depth {
                top = t.up(tp);
            }
        }
        cand.extend_from_slice(&cr.others);
        if cand.is_empty() {
            return None;
        }
        cand.sort_unstable();
        cand.dedup();
        for r in cand {
            let rule = &cr.rules[r as usize];
            for spec in &rule.specs {
                let res = match spec {
                    Spec::Inner(d, idx) => self.inner_indent(t, z, &rule.key, *d, *idx),
                    Spec::Block(i) => self.block_indent(t, z, &rule.key, *i),
                    Spec::Default => {
                        if self.matches(t, z, &rule.key) { Some(self.list_indent(t, z)) } else { None }
                    }
                };
                if res.is_some() {
                    return res;
                }
            }
        }
        None
    }
    fn custom_indent(&self, t: &Tree, z: u32) -> u32 {
        match self.indenter(t, z) {
            Some(w) => w,
            None => self.list_indent(t, z),
        }
    }
    fn indent_amount(&self, t: &Tree, mut z: u32) -> u32 {
        loop {
            let Some(p) = t.up(z) else { return 0 };
            let gp_rc = t.up(p).map_or(false, |g| reader_conditional(t, g));
            if gp_rc {
                return self.coll_indent(t, z);
            }
            match t.tag(p) {
                Tag::List | Tag::Fn => return self.custom_indent(t, z),
                Tag::Meta => z = p,
                _ => return self.coll_indent(t, z),
            }
        }
    }
}

/// `indent` pass: walks depth-first tracking columns (UTF-16) of the already-indented prefix.
pub(crate) fn indent(t: &mut Tree, root: u32, start: u32, ind: &Indenter) {
    t.n[root as usize].col = 0;
    let mut z = root;
    let mut active = start == root;
    loop {
        let tag = t.tag(z);
        if z == start {
            active = true;
        }
        if active && (tag == Tag::Newline || tag == Tag::Comment) && comment_next_ok(t, z, ind.cfg) {
            let w = ind.indent_amount(t, z);
            if w > 0 {
                let s = t.synth(Tag::Space, w);
                t.insert_right(z, s);
            }
        }
        // step (z/next*) assigning columns
        let c0 = t.n[z as usize].col;
        let first = t.n[z as usize].first;
        if first != NONE {
            t.n[first as usize].col = c0 + tag.opener_len();
            z = first;
            continue;
        }
        let mut after = if tag.is_container() { c0 + tag.opener_len() + tag.closer_len() } else { t.advance(c0, z) };
        let mut y = z;
        loop {
            let nx = t.n[y as usize].next;
            if nx != NONE {
                t.n[nx as usize].col = after;
                z = nx;
                break;
            }
            let p = t.n[y as usize].parent;
            if p == NONE {
                return;
            }
            after += t.tag(p).closer_len();
            y = p;
        }
    }
}

// ---------- ns name / alias / refer maps ----------

fn is_ns_form(t: &Tree, n: u32) -> bool {
    t.is_top(n) && t.tag(n) == Tag::List && t.down_sig(n).map_or(false, |d| t.tag(d) == Tag::Sym && t.text(d) == "ns")
}

pub(crate) fn find_namespace(t: &Tree, root: u32) -> Option<String> {
    let mut x = t.down_sig(root);
    while let Some(y) = x {
        if is_ns_form(t, y) {
            let d = t.down_sig(y)?;
            let nx = t.next_sig(d)?;
            let mut nx = nx;
            while matches!(t.tag(nx), Tag::Meta | Tag::MetaStar) {
                nx = skip_meta(t, nx)?; // sexpr of a meta node is its target's sexpr (may nest)
            }
            return if t.tag(nx) == Tag::Sym { Some(t.text(nx).to_string()) } else { None };
        }
        x = t.right_sig(y);
    }
    None
}

fn top_level_form(t: &Tree, mut n: u32) -> Option<u32> {
    if t.is_root(n) {
        return None;
    }
    while !t.is_top(n) {
        n = t.up(n)?;
    }
    Some(n)
}

fn first_child_sexpr(t: &Tree, n: u32) -> Option<u32> {
    let mut x = t.down(n);
    while let Some(y) = x {
        if !(t.is_wsc(y) || t.tag(y) == Tag::Uneval) {
            return Some(y);
        }
        x = t.right(y);
    }
    None
}

fn sexpr_str(t: &Tree, n: u32) -> String {
    let mut s = String::new();
    t.render(n, &mut s);
    s
}

fn find_req_zloc(t: &Tree, root: u32) -> Option<u32> {
    let mut x = Some(root);
    while let Some(y) = x {
        if !t.is_wsc(y) && t.tag(y).is_container() {
            if let Some(top) = top_level_form(t, y) {
                if is_ns_form(t, top) {
                    if let Some(f) = first_child_sexpr(t, y) {
                        if t.tag(f) == Tag::Kw && t.text(f) == ":require" {
                            return Some(y);
                        }
                    }
                }
            }
        }
        x = t.next_sig(y);
    }
    None
}

fn is_require_form(t: &Tree, n: u32) -> bool {
    top_level_form(t, n).map_or(false, |top| is_ns_form(t, top))
        && first_child_sexpr(t, n).map_or(false, |f| t.tag(f) == Tag::Kw && t.text(f) == ":require")
}

fn leftmost_symbol(t: &Tree, n: u32) -> Option<u32> {
    let mut x = t.leftmost_sig(n);
    while let Some(y) = x {
        if let Some(k) = skip_meta(t, y) {
            if t.tag(k) == Tag::Sym {
                return Some(k);
            }
        }
        x = t.right_sig(y);
    }
    None
}

fn require_parent(t: &Tree, gp: Option<u32>) -> Option<String> {
    let g = gp?;
    if is_require_form(t, g) {
        return None;
    }
    if matches!(t.tag(g), Tag::Vector | Tag::List) {
        return first_child_sexpr(t, g).map(|f| sexpr_str(t, f));
    }
    None
}

pub(crate) fn alias_refer_maps(t: &Tree, root: u32) -> (HashMap<String, String>, HashMap<String, String>) {
    let mut alias = HashMap::new();
    let mut refer = HashMap::new();
    let Some(req) = find_req_zloc(t, root) else { return (alias, refer) };
    let mut x = t.next_star(req);
    while let Some(y) = x {
        if t.tag(y) == Tag::Kw && (t.text(y) == ":as" || t.text(y) == ":refer") {
            let is_as = t.text(y) == ":as";
            let right = t.right_sig(y);
            let cur = leftmost_symbol(t, y).map(|s| t.text(s).to_string());
            let gp = t.up(y).and_then(|p| t.up(p));
            if let (Some(r), Some(cur)) = (right, cur) {
                let parent = require_parent(t, gp);
                let ns_str = match &parent {
                    Some(p) => format!("{}.{}", p, cur),
                    None => cur,
                };
                if is_as {
                    if t.tag(r) == Tag::Sym {
                        alias.insert(t.text(r).to_string(), ns_str);
                    }
                } else if t.tag(r) == Tag::Vector {
                    let mut c = t.down_sig(r);
                    while let Some(k) = c {
                        if !(t.tag(k) == Tag::Uneval) {
                            refer.insert(sexpr_str(t, k), ns_str.clone());
                        }
                        c = t.right_sig(k);
                    }
                }
            }
        }
        x = t.next_star(y);
    }
    (alias, refer)
}
