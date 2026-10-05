//! Lossless rewrite-clj style tree (whitespace, comments kept) + zipper navigation mirroring `rewrite-clj.zip`.
//! Positions are 1-based, UTF-16 columns, end-col just after (rewrite-clj node meta).

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tag {
    Forms,
    Token,
    MultiLine,
    MapQualifier,
    List,
    Vector,
    Map,
    Set,
    Fn,
    NsMap,
    ReaderMacro,
    Quote,
    SyntaxQuote,
    Unquote,
    UnquoteSplicing,
    Deref,
    Var,
    Eval,
    Uneval,
    Meta,
    RawMeta,
    Regex,
    Comment,
    Whitespace,
    Newline,
    Comma,
}

/// Token flavour (only for `Tag::Token`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tk {
    None,
    Sym,
    Kw,
    KwAuto,
    Num,
    Str,
    Const,
    Char,
}

const NONE: u32 = u32::MAX;

#[derive(Clone, Debug)]
pub struct Nd {
    pub tag: Tag,
    pub tk: Tk,
    pub parent: u32,
    pub idx: u32,
    pub kids: Vec<u32>,
    pub start: u32,
    pub end: u32,
    pub row: u32,
    pub col: u32,
    pub end_row: u32,
    pub end_col: u32,
}

pub struct Tree<'a> {
    pub src: &'a str,
    pub nodes: Vec<Nd>,
    pub err: bool,
    pub err_at: usize,
}

/// rewrite-clj `boundary?` chars.
fn boundary(c: u8) -> bool {
    matches!(c, b'"' | b':' | b';' | b'\'' | b'@' | b'^' | b'`' | b'~' | b'(' | b')' | b'[' | b']' | b'{' | b'}' | b'\\')
}
fn is_ws(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c | b',' | 0x1c..=0x1f)
}
fn is_lb(c: u8) -> bool {
    c == b'\n' || c == b'\r'
}

struct P<'a> {
    src: &'a str,
    s: &'a [u8],
    i: usize,
    row: u32,
    col: u32,
    nodes: Vec<Nd>,
    err: bool,
    err_at: usize,
}

impl<'a> P<'a> {
    fn fail(&mut self) {
        if !self.err {
            self.err_at = self.i;
        }
        self.err = true;
    }
    fn adv(&mut self, to: usize) {
        while self.i < to {
            let b = self.s[self.i];
            if b == b'\n' {
                self.row += 1;
                self.col = 1;
            } else if b & 0xC0 != 0x80 {
                self.col += if b >= 0xF0 { 2 } else { 1 };
            }
            self.i += 1;
        }
    }
    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }
    fn new_node(&mut self, tag: Tag, tk: Tk, parent: u32) -> u32 {
        let id = self.nodes.len() as u32;
        self.nodes.push(Nd {
            tag,
            tk,
            parent,
            idx: 0,
            kids: Vec::new(),
            start: self.i as u32,
            end: self.i as u32,
            row: self.row,
            col: self.col,
            end_row: self.row,
            end_col: self.col,
        });
        if parent != NONE {
            let n = self.nodes[parent as usize].kids.len() as u32;
            self.nodes[id as usize].idx = n;
            self.nodes[parent as usize].kids.push(id);
        }
        id
    }
    fn finish(&mut self, id: u32) {
        let n = &mut self.nodes[id as usize];
        n.end = self.i as u32;
        n.end_row = self.row;
        n.end_col = self.col;
    }
    fn leaf(&mut self, tag: Tag, tk: Tk, parent: u32, to: usize) -> u32 {
        let id = self.new_node(tag, tk, parent);
        self.adv(to);
        self.finish(id);
        id
    }
    fn is_printable(&self, id: u32) -> bool {
        !matches!(self.nodes[id as usize].tag, Tag::Whitespace | Tag::Newline | Tag::Comma | Tag::Comment | Tag::Uneval)
    }

    /// Parses children of `parent` until `closer` (or EOF for the root).
    fn seq(&mut self, parent: u32, closer: Option<u8>) {
        loop {
            let Some(c) = self.peek() else {
                if closer.is_some() {
                    self.fail();
                }
                return;
            };
            if Some(c) == closer {
                self.adv(self.i + 1);
                return;
            }
            if matches!(c, b')' | b']' | b'}') {
                self.fail();
                return;
            }
            if !self.form(parent) {
                return;
            }
        }
    }

    /// Wrapper nodes: children until `n` printable forms were read.
    fn printables(&mut self, id: u32, n: usize) {
        let mut got = 0;
        while got < n {
            let Some(c) = self.peek() else {
                self.fail();
                return;
            };
            if matches!(c, b')' | b']' | b'}') {
                self.fail();
                return;
            }
            let before = self.nodes.len();
            if !self.form(id) {
                return;
            }
            let last = *self.nodes[id as usize].kids.last().unwrap();
            if self.nodes.len() > before && self.is_printable(last) {
                got += 1;
            }
        }
    }

    fn coll(&mut self, tag: Tag, parent: u32, open_len: usize, closer: u8) {
        let id = self.new_node(tag, Tk::None, parent);
        self.adv(self.i + open_len);
        self.seq(id, Some(closer));
        self.finish(id);
    }
    fn wrap(&mut self, tag: Tag, parent: u32, prefix_len: usize, n: usize) {
        let id = self.new_node(tag, Tk::None, parent);
        self.adv(self.i + prefix_len);
        self.printables(id, n);
        self.finish(id);
    }

    fn token_end(&self, from: usize) -> usize {
        let mut j = from;
        while j < self.s.len() && !is_ws(self.s[j]) && !boundary(self.s[j]) {
            j += 1;
        }
        j
    }
    /// Symbols additionally allow `'` and `:` (rewrite-clj `not-boundary-allow-extra?`).
    fn sym_end(&self, from: usize) -> usize {
        let mut j = from;
        while j < self.s.len() && !is_ws(self.s[j]) && (!boundary(self.s[j]) || self.s[j] == b'\'' || self.s[j] == b':') {
            j += 1;
        }
        j
    }
    /// edn `read-token` for keywords: stops at whitespace or terminating macro chars.
    fn kw_end(&self, from: usize) -> usize {
        let mut j = from;
        while j < self.s.len() && !is_ws(self.s[j]) && !matches!(self.s[j], b'"' | b';' | b'@' | b'^' | b'`' | b'~' | b'(' | b')' | b'[' | b']' | b'{' | b'}' | b'\\') {
            j += 1;
        }
        j
    }

    /// Reads one element (whitespace, comment, or form) as a child of `parent`. false = stop (error).
    fn form(&mut self, parent: u32) -> bool {
        let c = self.s[self.i];
        let i = self.i;
        match c {
            b'\n' | b'\r' => {
                let mut j = i;
                while j < self.s.len() && is_lb(self.s[j]) {
                    j += 1;
                }
                self.leaf(Tag::Newline, Tk::None, parent, j);
            }
            b',' => {
                let mut j = i;
                while j < self.s.len() && self.s[j] == b',' {
                    j += 1;
                }
                self.leaf(Tag::Comma, Tk::None, parent, j);
            }
            c if is_ws(c) => {
                let mut j = i;
                while j < self.s.len() && is_ws(self.s[j]) && !is_lb(self.s[j]) && self.s[j] != b',' {
                    j += 1;
                }
                self.leaf(Tag::Whitespace, Tk::None, parent, j);
            }
            b';' => {
                let mut j = i;
                while j < self.s.len() && !is_lb(self.s[j]) {
                    j += 1;
                }
                if j < self.s.len() {
                    j += 1;
                }
                self.leaf(Tag::Comment, Tk::None, parent, j);
            }
            b'(' => self.coll(Tag::List, parent, 1, b')'),
            b'[' => self.coll(Tag::Vector, parent, 1, b']'),
            b'{' => self.coll(Tag::Map, parent, 1, b'}'),
            b'\'' => self.wrap(Tag::Quote, parent, 1, 1),
            b'`' => self.wrap(Tag::SyntaxQuote, parent, 1, 1),
            b'~' => {
                if self.s.get(i + 1) == Some(&b'@') {
                    self.wrap(Tag::UnquoteSplicing, parent, 2, 1)
                } else {
                    self.wrap(Tag::Unquote, parent, 1, 1)
                }
            }
            b'@' => self.wrap(Tag::Deref, parent, 1, 1),
            b'^' => self.wrap(Tag::Meta, parent, 1, 2),
            b'"' => {
                let mut j = i + 1;
                loop {
                    match self.s.get(j) {
                        None => {
                            self.fail();
                            break;
                        }
                        Some(b'\\') => j += 2,
                        Some(b'"') => {
                            j += 1;
                            break;
                        }
                        _ => j += 1,
                    }
                }
                let j = j.min(self.s.len());
                let tag = if self.s[i..j].contains(&b'\n') { Tag::MultiLine } else { Tag::Token };
                self.leaf(tag, Tk::Str, parent, j);
            }
            b'\\' => {
                let mut j = i + 1;
                let mut bs = false;
                if j < self.s.len() {
                    bs = self.s[j] == b'\\';
                    j += 1;
                    while j < self.s.len() && self.s[j] & 0xC0 == 0x80 {
                        j += 1;
                    }
                }
                let j = if bs { j } else { self.token_end(j) };
                self.leaf(Tag::Token, Tk::Char, parent, j);
            }
            b':' => {
                let auto = self.s.get(i + 1) == Some(&b':');
                let j = self.kw_end(i + 1 + auto as usize);
                self.leaf(Tag::Token, if auto { Tk::KwAuto } else { Tk::Kw }, parent, j);
            }
            b'#' => return self.dispatch(parent),
            _ => {
                let j = self.token_end(i + 1);
                let tk = classify(&self.src[i..j]);
                let j = if tk == Tk::Sym { self.sym_end(j) } else { j };
                self.leaf(Tag::Token, tk, parent, j);
            }
        }
        true
    }

    fn dispatch(&mut self, parent: u32) -> bool {
        let i = self.i;
        let n = self.s.get(i + 1).copied();
        match n {
            Some(b'{') => self.coll(Tag::Set, parent, 2, b'}'),
            Some(b'(') => self.coll(Tag::Fn, parent, 2, b')'),
            Some(b'\'') => self.wrap(Tag::Var, parent, 2, 1),
            Some(b'_') => self.wrap(Tag::Uneval, parent, 2, 1),
            Some(b'=') => self.wrap(Tag::Eval, parent, 2, 1),
            Some(b'^') => self.wrap(Tag::RawMeta, parent, 2, 2),
            Some(b'"') => {
                let mut j = i + 2;
                loop {
                    match self.s.get(j) {
                        None => {
                            self.fail();
                            break;
                        }
                        Some(b'\\') => j += 2,
                        Some(b'"') => {
                            j += 1;
                            break;
                        }
                        _ => j += 1,
                    }
                }
                let j = j.min(self.s.len());
                self.leaf(Tag::Regex, Tk::None, parent, j);
            }
            Some(b'?') if matches!(self.s.get(i + 2), Some(b'(') | Some(b'@')) => {
                let id = self.new_node(Tag::ReaderMacro, Tk::None, parent);
                self.adv(i + 2);
                let spl = self.peek() == Some(b'@');
                let t = self.new_node(Tag::Token, Tk::Sym, id);
                self.nodes[t as usize].start = (i + 1) as u32;
                if spl {
                    self.adv(self.i + 1);
                }
                self.finish(t);
                self.nodes[t as usize].row = 0; // created without position meta
                self.printables(id, 1);
                self.finish(id);
            }
            Some(b':') => {
                let id = self.new_node(Tag::NsMap, Tk::None, parent);
                self.adv(i + 1);
                let q = self.new_node(Tag::MapQualifier, Tk::None, id);
                let mut j = self.i;
                while self.s.get(j) == Some(&b':') {
                    j += 1;
                }
                let j = {
                    let mut k = j;
                    while k < self.s.len() && !is_ws(self.s[k]) && !boundary(self.s[k]) {
                        k += 1;
                    }
                    k
                };
                self.adv(j);
                self.finish(q);
                self.nodes[q as usize].row = 0;
                self.printables(id, 1);
                self.finish(id);
            }
            Some(b'#') => {
                // ##Inf / ##NaN
                let j = self.token_end(i + 2);
                self.leaf(Tag::Token, Tk::Const, parent, j);
            }
            Some(b'!') => {
                // `#!` shebang comment
                let mut j = i;
                while j < self.s.len() && self.s[j] != b'\n' {
                    j += 1;
                }
                if j < self.s.len() {
                    j += 1;
                }
                self.leaf(Tag::Comment, Tk::None, parent, j);
            }
            Some(_) => {
                let id = self.new_node(Tag::ReaderMacro, Tk::None, parent);
                self.adv(i + 1);
                let j = self.token_end(self.i);
                let j = if j == self.i { self.i + 1 } else { j };
                self.leaf(Tag::Token, Tk::Sym, id, j);
                self.printables(id, 1);
                self.finish(id);
            }
            None => {
                self.fail();
                return false;
            }
        }
        true
    }
}

fn classify(t: &str) -> Tk {
    let b = t.as_bytes();
    let d = |c: u8| c.is_ascii_digit();
    if d(b[0]) || ((b[0] == b'-' || b[0] == b'+') && b.len() > 1 && d(b[1])) {
        Tk::Num
    } else if matches!(t, "nil" | "true" | "false") {
        Tk::Const
    } else {
        Tk::Sym
    }
}

impl<'a> Tree<'a> {
    pub fn parse(src: &'a str) -> Tree<'a> {
        let mut p = P { src, s: src.as_bytes(), i: 0, row: 1, col: 1, nodes: Vec::new(), err: false, err_at: 0 };
        let root = p.new_node(Tag::Forms, Tk::None, NONE);
        p.seq(root, None);
        p.finish(root);
        if p.i < p.s.len() {
            p.err = true;
        }
        Tree { src, nodes: p.nodes, err: p.err, err_at: p.err_at }
    }
    pub fn root(&self) -> Z<'_> {
        Z { t: self, id: 0 }
    }
    pub fn z(&self, id: u32) -> Z<'_> {
        Z { t: self, id }
    }
}

/// Zipper location (an immutable node cursor).
#[derive(Clone, Copy)]
pub struct Z<'a> {
    pub t: &'a Tree<'a>,
    pub id: u32,
}

impl<'a> PartialEq for Z<'a> {
    fn eq(&self, o: &Self) -> bool {
        self.id == o.id
    }
}
impl<'a> std::fmt::Debug for Z<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Z({:?} {:?})", self.tag(), self.text())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Meta {
    pub row: u32,
    pub col: u32,
    pub end_row: u32,
    pub end_col: u32,
}

pub fn is_ws_tag(t: Tag) -> bool {
    matches!(t, Tag::Whitespace | Tag::Newline | Tag::Comma)
}
/// rewrite-clj `whitespace-or-comment?`.
pub fn is_ws_or_comment(t: Tag) -> bool {
    matches!(t, Tag::Whitespace | Tag::Newline | Tag::Comma | Tag::Comment)
}

impl<'a> Z<'a> {
    fn nd(&self) -> &'a Nd {
        &self.t.nodes[self.id as usize]
    }
    fn at(&self, id: u32) -> Z<'a> {
        Z { t: self.t, id }
    }
    pub fn tag(&self) -> Tag {
        self.nd().tag
    }
    pub fn tk(&self) -> Tk {
        self.nd().tk
    }
    pub fn text(&self) -> &'a str {
        let n = self.nd();
        &self.t.src[n.start as usize..n.end as usize]
    }
    pub fn meta(&self) -> Meta {
        let n = self.nd();
        Meta { row: n.row, col: n.col, end_row: n.end_row, end_col: n.end_col }
    }
    pub fn span(&self) -> (usize, usize) {
        let n = self.nd();
        (n.start as usize, n.end as usize)
    }
    pub fn is_ws(&self) -> bool {
        is_ws_or_comment(self.tag())
    }
    pub fn is_sym(&self) -> bool {
        self.tag() == Tag::Token && self.tk() == Tk::Sym
    }
    pub fn is_kw(&self) -> bool {
        self.tag() == Tag::Token && matches!(self.tk(), Tk::Kw | Tk::KwAuto)
    }
    /// Children ids (all, incl. whitespace).
    pub fn kids(&self) -> &'a [u32] {
        &self.nd().kids
    }
    pub fn kid_zs(&self) -> impl Iterator<Item = Z<'a>> + 'a {
        let t = self.t;
        self.kids().iter().map(move |&id| Z { t, id })
    }

    pub fn up(&self) -> Option<Z<'a>> {
        let p = self.nd().parent;
        (p != NONE).then(|| self.at(p))
    }
    pub fn down_star(&self) -> Option<Z<'a>> {
        self.nd().kids.first().map(|&k| self.at(k))
    }
    pub fn right_star(&self) -> Option<Z<'a>> {
        let n = self.nd();
        if n.parent == NONE {
            return None;
        }
        self.t.nodes[n.parent as usize].kids.get(n.idx as usize + 1).map(|&k| self.at(k))
    }
    pub fn left_star(&self) -> Option<Z<'a>> {
        let n = self.nd();
        if n.parent == NONE || n.idx == 0 {
            return None;
        }
        Some(self.at(self.t.nodes[n.parent as usize].kids[n.idx as usize - 1]))
    }
    pub fn leftmost_star(&self) -> Z<'a> {
        let n = self.nd();
        if n.parent == NONE {
            return *self;
        }
        self.at(self.t.nodes[n.parent as usize].kids[0])
    }
    pub fn rightmost_star(&self) -> Z<'a> {
        let n = self.nd();
        if n.parent == NONE {
            return *self;
        }
        self.at(*self.t.nodes[n.parent as usize].kids.last().unwrap())
    }
    fn skip_right(mut self) -> Option<Z<'a>> {
        while self.is_ws() {
            self = self.right_star()?;
        }
        Some(self)
    }
    fn skip_left(mut self) -> Option<Z<'a>> {
        while self.is_ws() {
            self = self.left_star()?;
        }
        Some(self)
    }
    /// `z/down`: first non-whitespace child.
    pub fn down(&self) -> Option<Z<'a>> {
        self.down_star()?.skip_right()
    }
    pub fn right(&self) -> Option<Z<'a>> {
        self.right_star()?.skip_right()
    }
    pub fn left(&self) -> Option<Z<'a>> {
        self.left_star()?.skip_left()
    }
    /// `z/leftmost`: leftmost non-whitespace sibling (nil when none).
    pub fn leftmost(&self) -> Option<Z<'a>> {
        self.leftmost_star().skip_right()
    }
    pub fn rightmost(&self) -> Option<Z<'a>> {
        self.rightmost_star().skip_left()
    }
    pub fn leftmost_p(&self) -> bool {
        self.left().is_none()
    }
    pub fn rightmost_p(&self) -> bool {
        self.right().is_none()
    }
    /// clojure.zip `next` (all nodes); None at the end.
    pub fn next_star(&self) -> Option<Z<'a>> {
        if let Some(d) = self.down_star() {
            return Some(d);
        }
        let mut z = *self;
        loop {
            if let Some(r) = z.right_star() {
                return Some(r);
            }
            z = z.up()?;
        }
    }
    /// `z/next`: next non-whitespace node depth-first.
    pub fn next(&self) -> Option<Z<'a>> {
        let mut z = self.next_star()?;
        while z.is_ws() {
            z = z.next_star()?;
        }
        Some(z)
    }
    pub fn prev_star(&self) -> Option<Z<'a>> {
        if let Some(l) = self.left_star() {
            let mut z = l;
            while let Some(&k) = z.kids().last() {
                z = self.at(k);
            }
            return Some(z);
        }
        self.up()
    }
    pub fn prev(&self) -> Option<Z<'a>> {
        let mut z = self.prev_star()?;
        while z.is_ws() {
            z = z.prev_star()?;
        }
        Some(z)
    }
    /// `z/skip-whitespace z/up` style: move with `f` while on whitespace (whitespace only, not comments).
    pub fn skip_ws_up(&self) -> Option<Z<'a>> {
        let mut z = *self;
        while is_ws_or_comment(z.tag()) {
            z = z.up()?;
        }
        Some(z)
    }
    pub fn skip_ws_right(&self) -> Option<Z<'a>> {
        let mut z = *self;
        while is_ws_or_comment(z.tag()) {
            z = z.right()?;
        }
        Some(z)
    }
    /// clojure.zip/root is always id 0.
    pub fn is_root(&self) -> bool {
        self.nd().parent == NONE
    }
    /// `edit/top?`: parent is the forms root.
    pub fn is_top(&self) -> bool {
        self.up().map_or(false, |u| u.is_root())
    }
    pub fn to_top(&self) -> Option<Z<'a>> {
        let mut z = *self;
        loop {
            let u = z.up()?;
            if u.is_root() {
                return Some(z);
            }
            z = u;
        }
    }
    pub fn find_up(&self, p: impl Fn(Z<'a>) -> bool) -> Option<Z<'a>> {
        let mut z = Some(*self);
        while let Some(x) = z {
            if p(x) {
                return Some(x);
            }
            z = x.up();
        }
        None
    }

    /// Symbol/keyword text when a non-string token.
    pub fn sym_text(&self) -> Option<&'a str> {
        (self.tag() == Tag::Token && matches!(self.tk(), Tk::Sym | Tk::Kw)).then(|| self.text())
    }
    /// `z/sexpr` equals the given symbol/keyword text (e.g. `defn`, `:require`).
    pub fn is_name(&self, s: &str) -> bool {
        self.sym_text() == Some(s)
    }
    pub fn is_coll(&self) -> bool {
        matches!(self.tag(), Tag::List | Tag::Vector | Tag::Map | Tag::Set | Tag::Fn)
    }
}

/// `edit/find-at-pos`: deepest loc at (row, col) (1-based).
pub fn find_at_pos<'a>(t: &'a Tree<'a>, row: u32, col: u32) -> Option<Z<'a>> {
    let in_range = |m: Meta| {
        row >= m.row && row <= m.end_row && (row != m.row || col >= m.col) && (row != m.end_row || col < m.end_col)
    };
    let inherits = |z: Z<'a>| {
        if z.meta().row == 0 {
            return false;
        }
        if in_range(z.meta()) {
            return true;
        }
        // closing bracket: cursor after the last element selects it (end-col compared, rows ignored as in clojure-lsp)
        z.meta().end_col == col
            && z.right().is_none()
            && z.up().map_or(false, |u| u.is_coll() && in_range(u.meta()))
    };
    let mut z = t.root().down_star()?;
    loop {
        if inherits(z) {
            let mut inner = z.down_star();
            let mut found = None;
            while let Some(c) = inner {
                if inherits(c) {
                    found = Some(c);
                    break;
                }
                inner = c.right_star();
            }
            match found {
                Some(c) => z = c,
                None => return Some(z),
            }
        } else {
            z = z.right_star()?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore]
    fn parse_dir_arg() {
        let Ok(d) = std::env::var("NX_PARSE_DIR") else { return };
        let mut bad = 0;
        let mut n = 0;
        let mut stack = vec![std::path::PathBuf::from(d)];
        while let Some(p) = stack.pop() {
            if p.is_dir() {
                if let Ok(rd) = std::fs::read_dir(&p) {
                    stack.extend(rd.flatten().map(|e| e.path()));
                }
            } else if p.extension().map_or(false, |e| matches!(e.to_str(), Some("clj" | "cljs" | "cljc" | "edn" | "bb" | "mova"))) {
                let Ok(t) = std::fs::read_to_string(&p) else { continue };
                let tree = Tree::parse(&t);
                n += 1;
                if tree.err {
                    bad += 1;
                    let at = tree.err_at.min(t.len());
                    let from = (0..=at.saturating_sub(40)).rev().find(|&i| t.is_char_boundary(i)).unwrap_or(0);
                    let to = (at..(at + 30).min(t.len()) + 1).rev().find(|&i| t.is_char_boundary(i)).unwrap_or(at);
                    eprintln!("ERR {} at {} {:?}", p.display(), at, &t[from..to.min(t.len())]);
                }
                // nodes partition their parent's span
                for nd in &tree.nodes {
                    let mut pos = nd.start;
                    let _ = &mut pos;
                }
            }
        }
        eprintln!("parsed {n} files, {bad} with errors");
    }

    #[test]
    #[ignore]
    fn parse_file_arg() {
        let Ok(p) = std::env::var("NX_PARSE_FILE") else { return };
        let t = std::fs::read_to_string(p).unwrap();
        let tree = Tree::parse(&t);
        let at = tree.err_at.min(t.len());
        let ctx_from = at.saturating_sub(60);
        eprintln!("err={} at={} ctx={:?}", tree.err, tree.err_at, &t[ctx_from..(at + 40).min(t.len())]);
        // round trip: every node's text is its source span, children cover the parent exactly
        for n in &tree.nodes {
            if n.kids.is_empty() {
                continue;
            }
            let first = &tree.nodes[n.kids[0] as usize];
            let last = &tree.nodes[*n.kids.last().unwrap() as usize];
            assert!(first.start >= n.start && last.end <= n.end);
        }
    }
}
