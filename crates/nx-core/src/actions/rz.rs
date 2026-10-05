//! Owned rewrite-clj style nodes + a zipper mirroring `rewrite-clj.zip` (clojure.zip semantics, whitespace-aware
//! navigation and editing) so refactorings port line by line from clojure-lsp.
use super::tree::{Meta, Tag, Tk, Tree};
use std::rc::Rc;

pub type NR = Rc<N>;

#[derive(Debug)]
pub struct N {
    pub tag: Tag,
    pub tk: Tk,
    /// Leaf text (tokens, comments, whitespace, regex, map qualifier). Empty for inner nodes.
    pub text: String,
    pub kids: Vec<NR>,
    /// Position metadata of parsed nodes.
    pub meta: Option<Meta>,
    /// `::markers` bit set (edit/mark-position).
    pub mark: u32,
}

pub fn is_inner(t: Tag) -> bool {
    !matches!(t, Tag::Token | Tag::MultiLine | Tag::Regex | Tag::Comment | Tag::Whitespace | Tag::Newline | Tag::Comma | Tag::MapQualifier)
}
pub fn is_printable_only(t: Tag) -> bool {
    matches!(t, Tag::Whitespace | Tag::Newline | Tag::Comma | Tag::Comment | Tag::Uneval)
}
/// rewrite-clj `whitespace?` (whitespace, newline, comma).
pub fn is_ws(t: Tag) -> bool {
    matches!(t, Tag::Whitespace | Tag::Newline | Tag::Comma)
}
/// rewrite-clj `whitespace-or-comment?`.
pub fn is_wsc(t: Tag) -> bool {
    matches!(t, Tag::Whitespace | Tag::Newline | Tag::Comma | Tag::Comment)
}

pub(crate) fn fix(t: Tag) -> (&'static str, &'static str) {
    match t {
        Tag::Forms => ("", ""),
        Tag::List => ("(", ")"),
        Tag::Vector => ("[", "]"),
        Tag::Map => ("{", "}"),
        Tag::Set => ("#{", "}"),
        Tag::Fn => ("#(", ")"),
        Tag::NsMap | Tag::ReaderMacro => ("#", ""),
        Tag::Quote => ("'", ""),
        Tag::SyntaxQuote => ("`", ""),
        Tag::Unquote => ("~", ""),
        Tag::UnquoteSplicing => ("~@", ""),
        Tag::Deref => ("@", ""),
        Tag::Var => ("#'", ""),
        Tag::Eval => ("#=", ""),
        Tag::Uneval => ("#_", ""),
        Tag::Meta => ("^", ""),
        Tag::RawMeta => ("#^", ""),
        _ => ("", ""),
    }
}

impl N {
    pub fn string(&self) -> String {
        let mut s = String::new();
        self.write(&mut s);
        s
    }
    pub fn write(&self, out: &mut String) {
        if !is_inner(self.tag) {
            out.push_str(&self.text);
            return;
        }
        let (a, b) = fix(self.tag);
        out.push_str(a);
        for k in &self.kids {
            k.write(out);
        }
        out.push_str(b);
    }
    pub fn is_sym(&self) -> bool {
        self.tag == Tag::Token && self.tk == Tk::Sym
    }
    pub fn is_kw(&self) -> bool {
        self.tag == Tag::Token && matches!(self.tk, Tk::Kw | Tk::KwAuto)
    }
    pub fn with_meta(mut self, m: Option<Meta>) -> N {
        self.meta = m;
        self
    }
}

pub fn leaf(tag: Tag, tk: Tk, text: &str) -> NR {
    Rc::new(N { tag, tk, text: text.to_string(), kids: vec![], meta: None, mark: 0 })
}
pub fn inner(tag: Tag, kids: Vec<NR>) -> NR {
    Rc::new(N { tag, tk: Tk::None, text: String::new(), kids, meta: None, mark: 0 })
}
pub fn spaces(n: usize) -> NR {
    leaf(Tag::Whitespace, Tk::None, &" ".repeat(n))
}
pub fn newlines(n: usize) -> NR {
    leaf(Tag::Newline, Tk::None, &"\n".repeat(n))
}
pub fn token_sym(s: &str) -> NR {
    let tk = if matches!(s, "nil" | "true" | "false") { Tk::Const } else { Tk::Sym };
    leaf(Tag::Token, tk, s)
}
pub fn keyword(s: &str) -> NR {
    leaf(Tag::Token, Tk::Kw, s)
}
pub fn list(kids: Vec<NR>) -> NR {
    inner(Tag::List, kids)
}
pub fn vector(kids: Vec<NR>) -> NR {
    inner(Tag::Vector, kids)
}
pub fn forms(kids: Vec<NR>) -> NR {
    inner(Tag::Forms, kids)
}

/// Copy of `n` with new children (`node/replace-children`), keeping tag and meta.
pub fn with_kids(n: &NR, kids: Vec<NR>) -> NR {
    Rc::new(N { tag: n.tag, tk: n.tk, text: n.text.clone(), kids, meta: n.meta, mark: n.mark })
}
pub fn with_meta_of(n: &NR, m: Option<Meta>) -> NR {
    Rc::new(N { tag: n.tag, tk: n.tk, text: n.text.clone(), kids: n.kids.clone(), meta: m, mark: n.mark })
}
pub fn with_mark(n: &NR, mark: u32) -> NR {
    Rc::new(N { tag: n.tag, tk: n.tk, text: n.text.clone(), kids: n.kids.clone(), meta: n.meta, mark })
}

/// Parsed tree -> owned nodes.
pub fn from_tree(t: &Tree) -> NR {
    fn go(t: &Tree, id: u32) -> NR {
        let z = t.z(id);
        let nd = &t.nodes[id as usize];
        let meta = if nd.row == 0 || nd.tag == Tag::Forms { None } else { Some(z.meta()) };
        if !is_inner(nd.tag) {
            Rc::new(N { tag: nd.tag, tk: nd.tk, text: z.text().to_string(), kids: vec![], meta, mark: 0 })
        } else {
            let kids = nd.kids.iter().map(|&k| go(t, k)).collect();
            Rc::new(N { tag: nd.tag, tk: nd.tk, text: String::new(), kids, meta, mark: 0 })
        }
    }
    go(t, 0)
}

// ---------------------------------------------------------------------------------------------
// Zipper (clojure.zip semantics)

#[derive(Debug)]
pub struct Path {
    pub l: Vec<NR>,
    pub r: Vec<NR>,
    pub ppath: Option<Rc<Path>>,
    pub pnode: NR,
    pub changed: bool,
}

#[derive(Clone, Debug)]
pub struct Loc {
    pub node: NR,
    pub path: Option<Rc<Path>>,
    pub end: bool,
}

impl Loc {
    pub fn of_node(n: NR) -> Loc {
        Loc { node: n, path: None, end: false }
    }
    pub fn tag(&self) -> Tag {
        self.node.tag
    }
    pub fn string(&self) -> String {
        self.node.string()
    }
    pub fn meta(&self) -> Option<Meta> {
        self.node.meta
    }
    pub fn is_branch(&self) -> bool {
        is_inner(self.node.tag)
    }
    fn mk(&self, node: NR, path: Option<Rc<Path>>) -> Loc {
        Loc { node, path, end: false }
    }
    fn with_path_changed(&self, node: NR) -> Loc {
        // mark the current path as changed
        match &self.path {
            Some(p) => Loc {
                node,
                path: Some(Rc::new(Path { l: p.l.clone(), r: p.r.clone(), ppath: p.ppath.clone(), pnode: p.pnode.clone(), changed: true })),
                end: false,
            },
            None => Loc { node, path: None, end: false },
        }
    }

    // ---- raw movement (rewrite-clj.custom-zipper / clojure.zip) ----
    pub fn down_raw(&self) -> Option<Loc> {
        if !self.is_branch() || self.node.kids.is_empty() {
            return None;
        }
        let kids = &self.node.kids;
        Some(self.mk(
            kids[0].clone(),
            Some(Rc::new(Path { l: vec![], r: kids[1..].to_vec(), ppath: self.path.clone(), pnode: self.node.clone(), changed: false })),
        ))
    }
    pub fn up_raw(&self) -> Option<Loc> {
        let p = self.path.as_ref()?;
        if p.changed {
            let mut kids: Vec<NR> = p.l.clone();
            kids.push(self.node.clone());
            kids.extend(p.r.iter().cloned());
            let n = with_kids(&p.pnode, kids);
            let pp = p.ppath.as_ref().map(|pp| Rc::new(Path { l: pp.l.clone(), r: pp.r.clone(), ppath: pp.ppath.clone(), pnode: pp.pnode.clone(), changed: true }));
            Some(Loc { node: n, path: pp, end: false })
        } else {
            Some(Loc { node: p.pnode.clone(), path: p.ppath.clone(), end: false })
        }
    }
    pub fn right_raw(&self) -> Option<Loc> {
        let p = self.path.as_ref()?;
        let (first, rest) = p.r.split_first()?;
        let mut l = p.l.clone();
        l.push(self.node.clone());
        Some(self.mk(first.clone(), Some(Rc::new(Path { l, r: rest.to_vec(), ppath: p.ppath.clone(), pnode: p.pnode.clone(), changed: p.changed }))))
    }
    pub fn left_raw(&self) -> Option<Loc> {
        let p = self.path.as_ref()?;
        let (last, rest) = p.l.split_last()?;
        let mut r = vec![self.node.clone()];
        r.extend(p.r.iter().cloned());
        Some(self.mk(last.clone(), Some(Rc::new(Path { l: rest.to_vec(), r, ppath: p.ppath.clone(), pnode: p.pnode.clone(), changed: p.changed }))))
    }
    pub fn leftmost_raw(&self) -> Loc {
        match &self.path {
            Some(p) if !p.l.is_empty() => {
                let mut r: Vec<NR> = p.l[1..].to_vec();
                r.push(self.node.clone());
                r.extend(p.r.iter().cloned());
                self.mk(p.l[0].clone(), Some(Rc::new(Path { l: vec![], r, ppath: p.ppath.clone(), pnode: p.pnode.clone(), changed: p.changed })))
            }
            _ => self.clone(),
        }
    }
    pub fn rightmost_raw(&self) -> Loc {
        let mut c = self.clone();
        while let Some(n) = c.right_raw() {
            c = n;
        }
        c
    }
    pub fn next_raw(&self) -> Loc {
        if self.end {
            return self.clone();
        }
        if let Some(d) = self.down_raw() {
            return d;
        }
        if let Some(r) = self.right_raw() {
            return r;
        }
        let mut p = self.clone();
        loop {
            match p.up_raw() {
                Some(u) => {
                    if let Some(r) = u.right_raw() {
                        return r;
                    }
                    p = u;
                }
                None => {
                    let mut e = p;
                    e.end = true;
                    return e;
                }
            }
        }
    }
    pub fn prev_raw(&self) -> Option<Loc> {
        if let Some(l) = self.left_raw() {
            let mut z = l;
            while let Some(c) = z.down_raw() {
                z = c.rightmost_raw();
            }
            return Some(z);
        }
        self.up_raw()
    }
    pub fn insert_left_raw(&self, item: NR) -> Loc {
        let p = self.path.as_ref().expect("cannot insert left at top");
        let mut l = p.l.clone();
        l.push(item);
        Loc { node: self.node.clone(), path: Some(Rc::new(Path { l, r: p.r.clone(), ppath: p.ppath.clone(), pnode: p.pnode.clone(), changed: true })), end: false }
    }
    pub fn insert_right_raw(&self, item: NR) -> Loc {
        let p = self.path.as_ref().expect("cannot insert right at top");
        let mut r = vec![item];
        r.extend(p.r.iter().cloned());
        Loc { node: self.node.clone(), path: Some(Rc::new(Path { l: p.l.clone(), r, ppath: p.ppath.clone(), pnode: p.pnode.clone(), changed: true })), end: false }
    }
    pub fn replace_raw(&self, item: NR) -> Loc {
        self.with_path_changed(item)
    }
    pub fn insert_child_raw(&self, item: NR) -> Loc {
        let mut kids = vec![item];
        kids.extend(self.node.kids.iter().cloned());
        self.replace_raw(with_kids(&self.node, kids))
    }
    pub fn append_child_raw(&self, item: NR) -> Loc {
        let mut kids = self.node.kids.clone();
        kids.push(item);
        self.replace_raw(with_kids(&self.node, kids))
    }
    /// clojure.zip `remove`: location at the node that preceded it in a depth-first walk.
    pub fn remove_raw(&self) -> Loc {
        let p = self.path.as_ref().expect("cannot remove at top");
        if let Some((last, rest)) = p.l.split_last() {
            let mut z = Loc {
                node: last.clone(),
                path: Some(Rc::new(Path { l: rest.to_vec(), r: p.r.clone(), ppath: p.ppath.clone(), pnode: p.pnode.clone(), changed: true })),
                end: false,
            };
            while let Some(c) = z.down_raw() {
                z = c.rightmost_raw();
            }
            z
        } else {
            let n = with_kids(&p.pnode, p.r.clone());
            let pp = p.ppath.as_ref().map(|pp| Rc::new(Path { l: pp.l.clone(), r: pp.r.clone(), ppath: pp.ppath.clone(), pnode: pp.pnode.clone(), changed: true }));
            Loc { node: n, path: pp, end: false }
        }
    }
    pub fn root(&self) -> NR {
        let mut c = self.clone();
        if c.end {
            // end loc is the root node
        }
        while let Some(u) = c.up_raw() {
            c = u;
        }
        c.node
    }
    pub fn lefts(&self) -> &[NR] {
        self.path.as_ref().map_or(&[], |p| &p.l[..])
    }
    pub fn rights(&self) -> &[NR] {
        self.path.as_ref().map_or(&[], |p| &p.r[..])
    }

    // ---- rewrite-clj.zip movement (skips whitespace/comments) ----
    fn skip(&self, f: &dyn Fn(&Loc) -> Option<Loc>) -> Option<Loc> {
        let mut c = Some(self.clone());
        while let Some(x) = c {
            if x.end {
                return None;
            }
            if !is_wsc(x.node.tag) {
                return Some(x);
            }
            c = f(&x);
        }
        None
    }
    pub fn skip_ws_right(&self) -> Option<Loc> {
        self.skip(&|l| l.right_raw())
    }
    pub fn skip_ws_left(&self) -> Option<Loc> {
        self.skip(&|l| l.left_raw())
    }
    pub fn skip_ws_next(&self) -> Option<Loc> {
        self.skip(&|l| Some(l.next_raw()))
    }
    pub fn skip_ws_prev(&self) -> Option<Loc> {
        self.skip(&|l| l.prev_raw())
    }
    pub fn down(&self) -> Option<Loc> {
        self.down_raw()?.skip_ws_right()
    }
    pub fn up(&self) -> Option<Loc> {
        self.up_raw()?.skip_ws_left()
    }
    pub fn right(&self) -> Option<Loc> {
        self.right_raw()?.skip_ws_right()
    }
    pub fn left(&self) -> Option<Loc> {
        self.left_raw()?.skip_ws_left()
    }
    pub fn leftmost(&self) -> Option<Loc> {
        self.leftmost_raw().skip_ws_right()
    }
    pub fn rightmost(&self) -> Option<Loc> {
        self.rightmost_raw().skip_ws_left()
    }
    pub fn leftmost_p(&self) -> bool {
        self.left_raw().and_then(|l| l.skip_ws_left()).is_none()
    }
    pub fn rightmost_p(&self) -> bool {
        self.right_raw().and_then(|l| l.skip_ws_right()).is_none()
    }
    /// `z/next`: at the end returns the same loc flagged end.
    pub fn next(&self) -> Loc {
        match self.next_raw_skip() {
            Some(l) => l,
            None => {
                let mut c = self.clone();
                c.end = true;
                c
            }
        }
    }
    fn next_raw_skip(&self) -> Option<Loc> {
        let n = self.next_raw();
        if n.end {
            return None;
        }
        n.skip_ws_next()
    }
    pub fn prev(&self) -> Option<Loc> {
        self.prev_raw()?.skip_ws_prev()
    }
    pub fn is_end(&self) -> bool {
        self.end
    }

    // ---- editing (rewrite-clj.zip) ----
    pub fn replace(&self, n: NR) -> Loc {
        self.replace_raw(n)
    }
    pub fn insert_right(&self, item: NR) -> Loc {
        let next = self.right_raw();
        let mut z = self.clone();
        if next.map_or(false, |n| !is_ws(n.node.tag)) {
            z = z.insert_right_raw(spaces(1));
        }
        z = z.insert_right_raw(item);
        if !(is_ws(self.node.tag) || self.node.tag == Tag::Comment) {
            z = z.insert_right_raw(spaces(1));
        }
        z
    }
    pub fn insert_left(&self, item: NR) -> Loc {
        let prev = self.left_raw();
        let mut z = self.clone();
        if prev.map_or(false, |p| !(is_ws(p.node.tag) || p.node.tag == Tag::Comment)) {
            z = z.insert_left_raw(spaces(1));
        }
        z = z.insert_left_raw(item);
        if !is_ws(self.node.tag) {
            z = z.insert_left_raw(spaces(1));
        }
        z
    }
    pub fn insert_child(&self, item: NR) -> Loc {
        let prev = self.down_raw();
        let mut z = self.clone();
        if prev.map_or(false, |p| !is_ws(p.node.tag)) {
            z = z.insert_child_raw(spaces(1));
        }
        z.insert_child_raw(item)
    }
    pub fn append_child(&self, item: NR) -> Loc {
        let prev = self.down_raw().map(|d| d.rightmost_raw());
        let mut z = self.clone();
        if prev.map_or(false, |p| !(is_ws(p.node.tag) || p.node.tag == Tag::Comment)) {
            z = z.append_child_raw(spaces(1));
        }
        z.append_child_raw(item)
    }
    pub fn insert_space_right(&self, n: usize) -> Loc {
        if n > 0 { self.insert_right_raw(spaces(n)) } else { self.clone() }
    }
    pub fn insert_space_left(&self, n: usize) -> Loc {
        if n > 0 { self.insert_left_raw(spaces(n)) } else { self.clone() }
    }
    pub fn insert_newline_right(&self, n: usize) -> Loc {
        self.insert_right_raw(newlines(n))
    }
    pub fn insert_newline_left(&self, n: usize) -> Loc {
        self.insert_left_raw(newlines(n))
    }

    pub(crate) fn remove_right_while(&self, p: &dyn Fn(Tag) -> bool) -> Loc {
        let mut z = self.clone();
        loop {
            match z.right_raw() {
                Some(r) if p(r.node.tag) => {
                    let pth = z.path.as_ref().unwrap();
                    z = Loc {
                        node: z.node.clone(),
                        path: Some(Rc::new(Path { l: pth.l.clone(), r: pth.r[1..].to_vec(), ppath: pth.ppath.clone(), pnode: pth.pnode.clone(), changed: true })),
                        end: false,
                    };
                }
                _ => return z,
            }
        }
    }
    pub(crate) fn remove_left_while(&self, p: &dyn Fn(Tag) -> bool) -> Loc {
        let mut z = self.clone();
        loop {
            match z.left_raw() {
                Some(l) if p(l.node.tag) => {
                    let pth = z.path.as_ref().unwrap();
                    z = Loc {
                        node: z.node.clone(),
                        path: Some(Rc::new(Path { l: pth.l[..pth.l.len() - 1].to_vec(), r: pth.r.clone(), ppath: pth.ppath.clone(), pnode: pth.pnode.clone(), changed: true })),
                        end: false,
                    };
                }
                _ => return z,
            }
        }
    }

    // ---- rewrite-clj.custom-zipper.utils (paredit) ----
    /// `u/remove-right`: drop the right sibling.
    pub fn remove_right1(&self) -> Loc {
        match &self.path {
            Some(p) if !p.r.is_empty() => Loc { node: self.node.clone(), path: Some(Rc::new(Path { l: p.l.clone(), r: p.r[1..].to_vec(), ppath: p.ppath.clone(), pnode: p.pnode.clone(), changed: true })), end: false },
            _ => self.clone(),
        }
    }
    /// `u/remove-left`: drop the left sibling.
    pub fn remove_left1(&self) -> Loc {
        match &self.path {
            Some(p) if !p.l.is_empty() => Loc { node: self.node.clone(), path: Some(Rc::new(Path { l: p.l[..p.l.len() - 1].to_vec(), r: p.r.clone(), ppath: p.ppath.clone(), pnode: p.pnode.clone(), changed: true })), end: false },
            _ => self.clone(),
        }
    }
    /// `u/remove-and-move-left`: remove this node, land on the left sibling (None when leftmost).
    pub fn remove_and_move_left(&self) -> Option<Loc> {
        let p = self.path.as_ref()?;
        let (last, rest) = p.l.split_last()?;
        Some(Loc { node: last.clone(), path: Some(Rc::new(Path { l: rest.to_vec(), r: p.r.clone(), ppath: p.ppath.clone(), pnode: p.pnode.clone(), changed: true })), end: false })
    }
    /// `u/remove-and-move-up`: remove this node, land on the parent (None = throws: the parent is the root).
    pub fn remove_and_move_up(&self) -> Option<Loc> {
        let p = self.path.as_ref()?;
        let pp = p.ppath.as_ref()?;
        let mut kids = p.l.clone();
        kids.extend(p.r.iter().cloned());
        Some(Loc { node: with_kids(&p.pnode, kids), path: Some(Rc::new(Path { l: pp.l.clone(), r: pp.r.clone(), ppath: pp.ppath.clone(), pnode: pp.pnode.clone(), changed: true })), end: false })
    }
    /// Start (row, col) of the current node in the edited document (`z/position`, 1-based, UTF-16 columns).
    pub fn start_pos(&self) -> (u32, u32) {
        let mut chain = vec![];
        let mut p = self.path.clone();
        while let Some(pa) = p {
            p = pa.ppath.clone();
            chain.push(pa);
        }
        let mut s = String::new();
        for pa in chain.iter().rev() {
            s.push_str(fix(pa.pnode.tag).0);
            for n in &pa.l {
                n.write(&mut s);
            }
        }
        let row = 1 + s.matches('\n').count() as u32;
        let last = s.rsplit('\n').next().unwrap_or("");
        (row, 1 + last.encode_utf16().count() as u32)
    }
    fn depth(&self) -> usize {
        let mut d = 0;
        let mut c = self.clone();
        while let Some(u) = c.up_raw() {
            d += 1;
            c = u;
        }
        d
    }
    fn has_trailing_linebreak_at_eoi(&self) -> bool {
        if self.depth() != 1 || self.right().is_some() {
            return false;
        }
        let mut c = Some(self.clone());
        while let Some(x) = c {
            if x.node.tag == Tag::Newline {
                return true;
            }
            c = x.right_raw();
        }
        false
    }
    /// `z/remove`: removes whitespace appropriately; location at the first non-ws node preceding in a depth-first walk.
    pub fn remove(&self) -> Loc {
        let mut z = self.clone();
        if self.rightmost_p() || self.leftmost_p() {
            z = z.remove_left_while(&is_ws);
        }
        let mut t = z.remove_right_while(&is_ws);
        if self.has_trailing_linebreak_at_eoi() {
            t = t.insert_newline_right(1);
        }
        let r = t.remove_raw();
        r.skip_ws_prev().unwrap_or(r)
    }
    /// `z/remove*`.
    pub fn remove_star(&self) -> Loc {
        self.remove_raw()
    }
    /// `z/splice`.
    pub fn splice(&self) -> Loc {
        if !self.is_branch() {
            return self.clone();
        }
        let ch: Vec<NR> = self.node.kids.clone();
        let mut a = 0;
        while a < ch.len() && is_ws(ch[a].tag) {
            a += 1;
        }
        let mut b = ch.len();
        while b > a && is_ws(ch[b - 1].tag) {
            b -= 1;
        }
        if a >= b {
            return self.remove();
        }
        // (reverse children) inserted right one at a time, so original order ends up after the node
        let mut z = self.clone();
        for c in ch[a..b].iter().rev() {
            z = z.insert_right_raw(c.clone());
        }
        // remove-and-move-right
        let p = z.path.as_ref().unwrap();
        let (first, rest) = p.r.split_first().unwrap();
        let loc = Loc { node: first.clone(), path: Some(Rc::new(Path { l: p.l.clone(), r: rest.to_vec(), ppath: p.ppath.clone(), pnode: p.pnode.clone(), changed: true })), end: false };
        loc.skip_ws_right().unwrap_or(loc)
    }

    /// `z/find f p?` over a movement function.
    pub fn find(&self, f: &dyn Fn(&Loc) -> Option<Loc>, p: &dyn Fn(&Loc) -> bool) -> Option<Loc> {
        let mut c = Some(self.clone());
        while let Some(x) = c {
            if x.end {
                return None;
            }
            if p(&x) {
                return Some(x);
            }
            c = f(&x);
        }
        None
    }
    pub fn find_next(&self, p: &dyn Fn(&Loc) -> bool) -> Option<Loc> {
        self.find(&|l| Some(l.next()), p)
    }
    pub fn find_right(&self, p: &dyn Fn(&Loc) -> bool) -> Option<Loc> {
        self.find(&|l| l.right(), p)
    }
    pub fn find_up(&self, p: &dyn Fn(&Loc) -> bool) -> Option<Loc> {
        self.find(&|l| l.up(), p)
    }
    /// `z/find-tag` with `z/right`.
    pub fn find_tag_right(&self, t: Tag) -> Option<Loc> {
        self.find_right(&|l| l.tag() == t)
    }

    /// `z/subzip`: zipper rooted at the current node.
    pub fn subzip(&self) -> Loc {
        Loc::of_node(self.node.clone())
    }
    /// Replace the node at `target` (a loc obtained from this tree) by `n`, returning the root loc.
    pub fn subedit_at(&self, target: &Loc, n: NR) -> Loc {
        Loc::of_node(target.replace(n).root())
    }
    /// `z/subedit-node`: apply `f` on the sub-tree rooted here; result replaces this node (located here).
    pub fn subedit(&self, f: impl FnOnce(Loc) -> Loc) -> Loc {
        let z2 = f(self.subzip());
        self.replace_raw(z2.root())
    }
    /// `z/edit-node`: apply `f`, then return to the same path (down/right counts) from the root.
    pub fn edit_path(&self, f: impl FnOnce(Loc) -> Option<Loc>) -> Option<Loc> {
        let path = self.path_counts();
        let z2 = f(self.clone())?;
        let root = Loc::of_node(z2.root());
        let mut c = root;
        for n in path {
            c = c.down_raw()?;
            for _ in 0..n {
                c = c.right_raw()?;
            }
        }
        Some(c)
    }
    fn path_counts(&self) -> Vec<usize> {
        let mut v = Vec::new();
        let mut c = self.clone();
        while let Some(u) = c.up_raw() {
            v.push(c.lefts().len());
            c = u;
        }
        v.reverse();
        v
    }
}

// sexpr-ish helpers on nodes ------------------------------------------------------------------

/// `z/sexpr` equals a symbol/keyword named `s` (e.g. `let`, `:as`).
pub fn is_name(n: &N, s: &str) -> bool {
    matches!(n.tag, Tag::Token) && matches!(n.tk, Tk::Sym | Tk::Kw) && n.text == s
}
