//! Whitespace-preserving CST mirroring rewrite-clj's node structure (doubly-linked sibling lists, arena).
//! Why not extend `reader.rs`: it drops trivia and its 32-byte node is on the hot analyzer path; cljfmt needs
//! rewrite-clj's exact whitespace/newline/comma run splitting, so a small dedicated parser is cheaper.

pub const NONE: u32 = u32::MAX;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Tag {
    // containers
    Forms,
    List,
    Vector,
    Map,
    Set,
    Fn,
    Meta,
    MetaStar,
    ReaderMacro,
    NsMap,
    Deref,
    Quote,
    SyntaxQuote,
    Unquote,
    UnquoteSplicing,
    Var,
    Eval,
    Uneval,
    // leaves
    Sym,
    Kw,
    Tok,
    Str,
    MStr,
    Regex,
    Comment,
    Space,
    Comma,
    Newline,
    Qual,
}

impl Tag {
    #[inline]
    pub fn is_container(self) -> bool {
        (self as u8) <= (Tag::Uneval as u8)
    }
    #[inline]
    pub fn is_ws(self) -> bool {
        matches!(self, Tag::Space | Tag::Comma | Tag::Newline)
    }
    /// rewrite-clj `:token` tag (symbols, keywords, numbers, single-line strings...).
    #[inline]
    pub fn is_token(self) -> bool {
        matches!(self, Tag::Sym | Tag::Kw | Tag::Tok | Tag::Str)
    }
    /// Opener text length (cljfmt `start-element`).
    #[inline]
    pub fn opener_len(self) -> u32 {
        match self {
            Tag::Forms => 0,
            Tag::Fn | Tag::Set | Tag::MetaStar | Tag::Var | Tag::Eval | Tag::Uneval | Tag::UnquoteSplicing => 2,
            _ => 1,
        }
    }
    #[inline]
    pub fn closer_len(self) -> u32 {
        matches!(self, Tag::List | Tag::Vector | Tag::Map | Tag::Set | Tag::Fn) as u32
    }
    pub fn opener(self) -> &'static str {
        match self {
            Tag::Forms => "",
            Tag::List => "(",
            Tag::Vector => "[",
            Tag::Map => "{",
            Tag::Set => "#{",
            Tag::Fn => "#(",
            Tag::Meta => "^",
            Tag::MetaStar => "#^",
            Tag::ReaderMacro | Tag::NsMap => "#",
            Tag::Deref => "@",
            Tag::Quote => "'",
            Tag::SyntaxQuote => "`",
            Tag::Unquote => "~",
            Tag::UnquoteSplicing => "~@",
            Tag::Var => "#'",
            Tag::Eval => "#=",
            Tag::Uneval => "#_",
            _ => "",
        }
    }
    pub fn closer(self) -> &'static str {
        match self {
            Tag::List | Tag::Fn => ")",
            Tag::Vector => "]",
            Tag::Map | Tag::Set => "}",
            _ => "",
        }
    }
}

#[derive(Clone)]
pub struct Node {
    pub tag: Tag,
    pub parent: u32,
    pub prev: u32,
    pub next: u32,
    pub first: u32,
    pub last: u32,
    /// Source byte range (leaves); unused for synthetic nodes.
    pub s: u32,
    pub e: u32,
    /// Synthetic node: number of repeated chars (' ' for Space, '\n' for Newline); 0 = source text.
    pub fill: u32,
    /// Start column (UTF-16 units) recorded during the indent pass.
    pub col: u32,
    /// Number of left siblings that are not whitespace/comment/uneval (cljfmt `index-of`).
    pub sidx: i32,
}

pub struct Tree<'a> {
    pub src: &'a str,
    pub n: Vec<Node>,
    pub ascii: bool,
}

impl<'a> Tree<'a> {
    pub fn new(src: &'a str) -> Tree<'a> {
        let mut t = Tree { src, n: Vec::with_capacity(src.len() / 3 + 8), ascii: src.is_ascii() };
        t.add(Tag::Forms, 0, 0);
        t
    }
    #[inline]
    pub fn add(&mut self, tag: Tag, s: u32, e: u32) -> u32 {
        self.n.push(Node { tag, parent: NONE, prev: NONE, next: NONE, first: NONE, last: NONE, s, e, fill: 0, col: 0, sidx: 0 });
        (self.n.len() - 1) as u32
    }
    pub fn synth(&mut self, tag: Tag, fill: u32) -> u32 {
        let id = self.add(tag, 0, 0);
        self.n[id as usize].fill = fill;
        id
    }
    #[inline]
    pub fn tag(&self, n: u32) -> Tag {
        self.n[n as usize].tag
    }
    #[inline]
    pub fn opt(&self, n: u32) -> Option<u32> {
        if n == NONE { None } else { Some(n) }
    }
    /// Text of a leaf (synthetic fill is handled by callers via `push_leaf`).
    pub fn text(&self, n: u32) -> &'a str {
        let nd = &self.n[n as usize];
        &self.src[nd.s as usize..nd.e as usize]
    }

    // ---- linking ----
    /// Attach `kids` (already created, unlinked) as children of `p`, computing `sidx`.
    pub fn set_children(&mut self, p: u32, kids: &[u32]) {
        let mut prev = NONE;
        let mut idx = 0i32;
        for &k in kids {
            let tag = self.n[k as usize].tag;
            let nd = &mut self.n[k as usize];
            nd.parent = p;
            nd.prev = prev;
            nd.next = NONE;
            nd.sidx = idx;
            if prev != NONE {
                self.n[prev as usize].next = k;
            }
            if !(tag.is_ws() || tag == Tag::Comment || tag == Tag::Uneval) {
                idx += 1;
            }
            prev = k;
        }
        self.n[p as usize].first = kids.first().copied().unwrap_or(NONE);
        self.n[p as usize].last = prev;
    }

    // ---- raw zipper moves (rewrite-clj `z/left*` etc.) ----
    #[inline]
    pub fn left(&self, n: u32) -> Option<u32> {
        self.opt(self.n[n as usize].prev)
    }
    #[inline]
    pub fn right(&self, n: u32) -> Option<u32> {
        self.opt(self.n[n as usize].next)
    }
    #[inline]
    pub fn up(&self, n: u32) -> Option<u32> {
        self.opt(self.n[n as usize].parent)
    }
    #[inline]
    pub fn down(&self, n: u32) -> Option<u32> {
        self.opt(self.n[n as usize].first)
    }
    pub fn leftmost(&self, n: u32) -> u32 {
        let p = self.n[n as usize].parent;
        if p == NONE { n } else { self.n[p as usize].first }
    }
    /// `z/next*`: depth-first successor; None at the end.
    pub fn next_star(&self, n: u32) -> Option<u32> {
        let nd = &self.n[n as usize];
        if nd.first != NONE {
            return Some(nd.first);
        }
        let mut c = n;
        loop {
            let x = &self.n[c as usize];
            if x.next != NONE {
                return Some(x.next);
            }
            if x.parent == NONE {
                return None;
            }
            c = x.parent;
        }
    }
    fn deepest_last(&self, mut n: u32) -> u32 {
        while self.n[n as usize].last != NONE {
            n = self.n[n as usize].last;
        }
        n
    }
    /// `z/prev*`.
    pub fn prev_star(&self, n: u32) -> Option<u32> {
        let nd = &self.n[n as usize];
        if nd.prev != NONE {
            Some(self.deepest_last(nd.prev))
        } else {
            self.opt(nd.parent)
        }
    }
    // ---- skipping moves (rewrite-clj `z/right`, `z/left`, `z/down`, `z/leftmost`) ----
    #[inline]
    pub fn is_wsc(&self, n: u32) -> bool {
        let t = self.tag(n);
        t.is_ws() || t == Tag::Comment
    }
    pub fn right_sig(&self, n: u32) -> Option<u32> {
        let mut x = self.right(n);
        while let Some(y) = x {
            if !self.is_wsc(y) {
                return Some(y);
            }
            x = self.right(y);
        }
        None
    }
    pub fn left_sig(&self, n: u32) -> Option<u32> {
        let mut x = self.left(n);
        while let Some(y) = x {
            if !self.is_wsc(y) {
                return Some(y);
            }
            x = self.left(y);
        }
        None
    }
    pub fn down_sig(&self, n: u32) -> Option<u32> {
        let x = self.down(n)?;
        if !self.is_wsc(x) { Some(x) } else { self.right_sig(x) }
    }
    /// `z/leftmost`: first non-ws/comment sibling (may be `n` itself).
    pub fn leftmost_sig(&self, n: u32) -> Option<u32> {
        let x = self.leftmost(n);
        if !self.is_wsc(x) { Some(x) } else { self.right_sig(x) }
    }
    /// `z/next` (skips ws/comments in depth-first order).
    pub fn next_sig(&self, n: u32) -> Option<u32> {
        let mut x = self.next_star(n);
        while let Some(y) = x {
            if !self.is_wsc(y) {
                return Some(y);
            }
            x = self.next_star(y);
        }
        None
    }
    // ---- edits ----
    pub fn insert_left(&mut self, at: u32, new: u32) {
        let p = self.n[at as usize].parent;
        let l = self.n[at as usize].prev;
        self.n[new as usize].parent = p;
        self.n[new as usize].prev = l;
        self.n[new as usize].next = at;
        self.n[new as usize].sidx = self.n[at as usize].sidx;
        self.n[at as usize].prev = new;
        if l != NONE { self.n[l as usize].next = new } else { self.n[p as usize].first = new }
    }
    pub fn insert_right(&mut self, at: u32, new: u32) {
        let p = self.n[at as usize].parent;
        let r = self.n[at as usize].next;
        self.n[new as usize].parent = p;
        self.n[new as usize].next = r;
        self.n[new as usize].prev = at;
        let at_sig = !(self.is_wsc(at) || self.tag(at) == Tag::Uneval) as i32;
        self.n[new as usize].sidx = self.n[at as usize].sidx + at_sig;
        self.n[at as usize].next = new;
        if r != NONE { self.n[r as usize].prev = new } else { self.n[p as usize].last = new }
    }
    /// `z/remove*`: unlink `n`; returns the location preceding it depth-first.
    pub fn remove(&mut self, n: u32) -> u32 {
        let ret = self.prev_star(n).unwrap();
        let (p, l, r) = { let x = &self.n[n as usize]; (x.parent, x.prev, x.next) };
        if l != NONE { self.n[l as usize].next = r } else { self.n[p as usize].first = r }
        if r != NONE { self.n[r as usize].prev = l } else { self.n[p as usize].last = l }
        ret
    }
    pub fn is_root(&self, n: u32) -> bool {
        self.n[n as usize].parent == NONE
    }
    /// cljfmt `top?`: parent is the root.
    pub fn is_top(&self, n: u32) -> bool {
        let p = self.n[n as usize].parent;
        p != NONE && self.n[p as usize].parent == NONE
    }

    // ---- text ----
    pub fn utf16_len(&self, s: &str) -> u32 {
        if self.ascii { s.len() as u32 } else { s.encode_utf16().count() as u32 }
    }
    /// Column after appending leaf `n` at column `col`.
    pub fn advance(&self, col: u32, n: u32) -> u32 {
        let nd = &self.n[n as usize];
        if nd.fill > 0 {
            return if nd.tag == Tag::Newline { 0 } else { col + nd.fill };
        }
        let s = &self.src[nd.s as usize..nd.e as usize];
        match s.rfind('\n') {
            Some(i) => self.utf16_len(&s[i + 1..]),
            None => col + self.utf16_len(s),
        }
    }
    fn push_leaf(&self, n: u32, out: &mut String) {
        let nd = &self.n[n as usize];
        if nd.fill > 0 {
            let c = if nd.tag == Tag::Newline { '\n' } else { ' ' };
            for _ in 0..nd.fill {
                out.push(c);
            }
        } else {
            out.push_str(&self.src[nd.s as usize..nd.e as usize]);
        }
    }
    /// Render node `n` (and children) to `out`.
    pub fn render(&self, n: u32, out: &mut String) {
        let mut stack: Vec<u32> = Vec::new();
        // iterative depth-first render
        let mut c = n;
        loop {
            let nd = &self.n[c as usize];
            if nd.tag.is_container() {
                out.push_str(nd.tag.opener());
                if nd.first != NONE {
                    stack.push(c);
                    c = nd.first;
                    continue;
                }
                out.push_str(nd.tag.closer());
            } else {
                self.push_leaf(c, out);
            }
            // move to next sibling or pop
            loop {
                if c == n {
                    return;
                }
                let nx = self.n[c as usize].next;
                if nx != NONE {
                    c = nx;
                    break;
                }
                let p = stack.pop().unwrap();
                out.push_str(self.n[p as usize].tag.closer());
                c = p;
            }
        }
    }
}
