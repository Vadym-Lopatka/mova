//! Clojure/cljc/cljs/EDN reader producing a `Cst`. Mirrors clj-kondo's rewrite-clj fork (tokens,
//! positions, error messages). Never panics; recovers from syntax errors and keeps parsing.
//!
//! Entry points: `parse(&str) -> Cst`, `parse_owned(String) -> Cst`. See `cst` for the tree API.

use crate::cst::*;
use crate::intern::{intern, SymId};

const MAX_DEPTH: usize = 400;

/// Parse source text. Never panics; syntax problems land in `Cst::errors()`.
pub fn parse(src: &str) -> Cst {
    parse_owned(src.to_owned())
}

pub fn parse_owned(src: String) -> Cst {
    let src: Box<str> = src.into_boxed_str();
    let cap = src.len() / 6 + 4;
    let mut p = P {
        src: &src,
        s: src.as_bytes(),
        i: 0,
        nodes: Vec::with_capacity(cap),
        index: Vec::with_capacity(cap),
        stack: Vec::new(),
        errors: Vec::new(),
        tp: 0,
        trow: 1,
        tcol: 1,
        depth: 0,
        stop: false,
    };
    p.index.push(NodeId(0)); // shared empty child list
    let root = p.open(Kind::Root, 0, 0);
    let base = p.stack.len();
    p.parse_top();
    p.close(root, base, p.s.len());
    let (nodes, index, errors) = (p.nodes, p.index, p.errors);
    Cst { src, nodes, index, errors, wide: Default::default() }
}

struct P<'a> {
    src: &'a str,
    s: &'a [u8],
    i: usize,
    nodes: Vec<Node>,
    index: Vec<NodeId>,
    stack: Vec<NodeId>,
    errors: Vec<ParseError>,
    // position tracker (monotonic scan cursor)
    tp: usize,
    trow: u32,
    tcol: u32,
    depth: usize,
    stop: bool,
}

/// Java `Character.isWhitespace` for a non-ASCII char.
fn java_ws(c: char) -> bool {
    matches!(c as u32, 0x1680 | 0x2000..=0x2006 | 0x2008..=0x200A | 0x2028 | 0x2029 | 0x205F | 0x3000)
}

#[inline(always)]
fn ascii_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r' | 0x1c..=0x1f | b',')
}

#[inline(always)]
fn is_boundary(b: u8) -> bool {
    matches!(b, b'"' | b':' | b';' | b'\'' | b'@' | b'^' | b'`' | b'~' | b'(' | b')' | b'[' | b']' | b'{' | b'}' | b'\\')
}

#[inline(always)]
fn closer(b: u8) -> bool {
    matches!(b, b')' | b']' | b'}')
}

fn split_ns(tok: &str) -> (&str, &str) {
    match tok.find('/') {
        Some(k) if k > 0 && k + 1 < tok.len() => (&tok[..k], &tok[k + 1..]),
        _ => ("", tok),
    }
}

impl<'a> P<'a> {
    #[inline(always)]
    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }
    #[inline(always)]
    fn at(&self, o: usize) -> Option<u8> {
        self.s.get(self.i + o).copied()
    }

    /// Advance the position tracker to byte `pos`; returns (row, col).
    fn track(&mut self, pos: usize) -> (u32, u32) {
        let pos = pos.min(self.s.len());
        if pos < self.tp {
            self.tp = 0;
            self.trow = 1;
            self.tcol = 1;
        }
        let s = self.s;
        let (mut i, mut row, mut col) = (self.tp, self.trow, self.tcol);
        while i < pos {
            let b = s[i];
            if b >= 0x80 {
                col += utf16_width(b);
            } else if b == b'\n' {
                row += 1;
                col = 1;
            } else if b == b'\r' {
                row += 1;
                col = 1;
                if matches!(s.get(i + 1), Some(b'\n') | Some(0x0c)) && i + 1 < pos {
                    i += 1;
                }
            } else {
                col += 1;
            }
            i += 1;
        }
        self.tp = i;
        self.trow = row;
        self.tcol = col;
        (row, col)
    }

    fn err_at(&mut self, pos: usize, msg: String) {
        let (row, col) = self.track(pos);
        self.errors.push(ParseError { row, col, msg });
    }

    fn open(&mut self, kind: Kind, flags: u8, start: usize) -> NodeId {
        let (row, col) = self.track(start);
        let id = NodeId(self.nodes.len() as u32);
        self.nodes.push(Node::new(kind, flags, start as u32, row, col));
        id
    }

    fn set_end(&mut self, id: NodeId, end: usize) {
        let (row, col) = self.track(end);
        let (c, w) = sat(col);
        let n = &mut self.nodes[id.0 as usize];
        n.end = end as u32;
        n.end_row = row;
        n.end_col = c;
        n.flags |= w;
    }

    /// Finish a container: children = stack[base..]; move them into the child index.
    fn close(&mut self, id: NodeId, base: usize, end: usize) {
        self.set_end(id, end);
        let cnt = self.stack.len() - base;
        if cnt > 0 {
            let at = self.index.len() as u32;
            self.index.push(NodeId(cnt as u32));
            self.index.extend_from_slice(&self.stack[base..]);
            self.nodes[id.0 as usize].a = at;
        }
        self.stack.truncate(base);
    }

    fn token(&mut self, kind: Kind, flags: u8, start: usize, end: usize, name: SymId, ns: SymId) -> NodeId {
        let id = self.open(kind, flags, start);
        self.set_end(id, end);
        let n = &mut self.nodes[id.0 as usize];
        n.a = name.0;
        n.b = ns.0;
        id
    }

    /// Skip whitespace, commas, `;` comments and `#!` lines.
    fn skip_trivia(&mut self) {
        let s = self.s;
        loop {
            let Some(&b) = s.get(self.i) else { return };
            if ascii_ws(b) {
                self.i += 1;
            } else if b == b';' || (b == b'#' && s.get(self.i + 1) == Some(&b'!')) {
                self.skip_line();
            } else if b >= 0x80 {
                match self.char_at(self.i) {
                    Some(c) if java_ws(c) => self.i += c.len_utf8(),
                    _ => return,
                }
            } else {
                return;
            }
        }
    }

    /// Consume through the next linebreak (kondo `read-include-linebreak`).
    fn skip_line(&mut self) {
        let s = self.s;
        while let Some(&b) = s.get(self.i) {
            self.i += 1;
            if b == b'\n' {
                return;
            }
            if b == b'\r' {
                if matches!(s.get(self.i), Some(b'\n') | Some(0x0c)) {
                    self.i += 1;
                }
                return;
            }
        }
    }

    #[inline]
    fn char_at(&self, i: usize) -> Option<char> {
        self.src.get(i..).and_then(|t| t.chars().next())
    }

    /// Next token-ending position: stops at whitespace or boundary. `extra` allows `'` and `:`.
    fn scan_token(&self, from: usize, extra: bool) -> usize {
        let s = self.s;
        let mut j = from;
        while let Some(&b) = s.get(j) {
            if b < 0x80 {
                if ascii_ws(b) || (is_boundary(b) && !(extra && (b == b'\'' || b == b':'))) {
                    break;
                }
                j += 1;
            } else {
                match self.char_at(j) {
                    Some(c) if java_ws(c) => break,
                    Some(c) => j += c.len_utf8(),
                    None => j += 1,
                }
            }
        }
        j
    }

    fn parse_top(&mut self) {
        loop {
            self.skip_trivia();
            match self.peek() {
                None => return,
                Some(c) if closer(c) => {
                    let m = format!("Unmatched bracket: unexpected {}", c as char);
                    self.err_at(self.i, m);
                    self.i += 1;
                }
                Some(_) => {
                    if let Some(id) = self.form() {
                        self.stack.push(id);
                    }
                    if self.stop {
                        self.i = self.s.len();
                        return;
                    }
                }
            }
        }
    }

    /// Parse one form at `self.i` (trivia already skipped, not EOF/closer).
    fn form(&mut self) -> Option<NodeId> {
        if self.depth >= MAX_DEPTH {
            self.err_at(self.i, "Nesting too deep".to_string());
            self.stop = true;
            return None;
        }
        self.depth += 1;
        let r = self.form_inner();
        self.depth -= 1;
        r
    }

    fn form_inner(&mut self) -> Option<NodeId> {
        let b = self.peek()?;
        Some(match b {
            b'(' => self.delim(Kind::List, 0, 1, b')'),
            b'[' => self.delim(Kind::Vector, 0, 1, b']'),
            b'{' => self.delim(Kind::Map, 0, 1, b'}'),
            b'\'' => self.wrap(Kind::Quote, 1, 1),
            b'`' => self.wrap(Kind::SyntaxQuote, 1, 1),
            b'@' => self.wrap(Kind::Deref, 1, 1),
            b'~' => {
                if self.at(1) == Some(b'@') {
                    self.wrap(Kind::UnquoteSplicing, 2, 1)
                } else {
                    self.wrap(Kind::Unquote, 1, 1)
                }
            }
            b'^' => self.wrap(Kind::Meta, 1, 2),
            b'#' => return self.sharp(),
            b'"' => self.string(Kind::String, self.i),
            b':' => self.keyword(),
            b'\\' => self.char_lit(),
            _ => self.symbol_or_number(),
        })
    }

    /// Delimited container; `width` = opening token length (e.g. 2 for `#{`).
    fn delim(&mut self, kind: Kind, flags: u8, width: usize, close: u8) -> NodeId {
        let start = self.i;
        let id = self.open(kind, flags, start);
        let orow = self.nodes[id.0 as usize].row;
        self.i += width;
        let open_ch = self.s[start + width - 1] as char;
        let base = self.stack.len();
        let mut end = None;
        let mut end_override: Option<(u32, u16)> = None;
        loop {
            self.skip_trivia();
            match self.peek() {
                None => {
                    // kondo recovers an unterminated container ending at its last child
                    end = Some(match self.stack.len() > base {
                        true => {
                            let last = self.stack[self.stack.len() - 1];
                            let ln = &self.nodes[last.0 as usize];
                            // the recovered container also covers one more column after its last child
                            end_override = Some((ln.end_row, ln.end_col.saturating_add(1)));
                            ln.end as usize
                        }
                        false => start + width,
                    });
                    let m = format!("Found an opening {} with no matching {}", open_ch, close as char);
                    self.err_at(start, m);
                    let m = format!("Expected a {} to match {} from line {}", close as char, open_ch, orow);
                    self.err_at(self.i, m);
                    self.nodes[id.0 as usize].flags |= F_ERROR;
                    break;
                }
                Some(c) if closer(c) => {
                    if c != close {
                        let (crow, ccol) = self.track(self.i);
                        let m1 = format!("Mismatched bracket: found an opening {} and a closing {} on line {}", open_ch, c as char, crow);
                        let m2 = format!("Mismatched bracket: found an opening {} on line {} and a closing {}", open_ch, orow, c as char);
                        let (srow, scol) = (orow, self.nodes[id.0 as usize].col as u32);
                        let scol = if self.nodes[id.0 as usize].flags & F_WIDE != 0 { col_at(self.s, start) } else { scol };
                        self.errors.push(ParseError { row: srow, col: scol, msg: m1 });
                        self.errors.push(ParseError { row: crow, col: ccol, msg: m2 });
                        self.nodes[id.0 as usize].flags |= F_ERROR;
                    }
                    self.i += 1;
                    break;
                }
                Some(_) => {
                    if let Some(c) = self.form() {
                        self.stack.push(c);
                    }
                    if self.stop {
                        self.nodes[id.0 as usize].flags |= F_ERROR;
                        break;
                    }
                }
            }
        }
        self.close(id, base, end.unwrap_or(self.i));
        if let Some((row, col)) = end_override {
            let n = &mut self.nodes[id.0 as usize];
            n.end_row = row;
            n.end_col = col;
        }
        id
    }

    /// Wrapper node reading `n` operand forms (leading `Uneval`s do not count).
    fn wrap(&mut self, kind: Kind, width: usize, n: usize) -> NodeId {
        let start = self.i;
        let id = self.open(kind, 0, start);
        self.i += width;
        let base = self.stack.len();
        let mut got = 0;
        while got < n {
            self.skip_trivia();
            match self.peek() {
                None => {
                    let msg = if kind == Kind::Uneval { ":uneval node expects 1 value." } else { "Unexpected EOF." };
                    self.err_at(self.i, msg.to_string());
                    self.nodes[id.0 as usize].flags |= F_ERROR;
                    break;
                }
                Some(c) if closer(c) => {
                    self.nodes[id.0 as usize].flags |= F_ERROR;
                    break;
                }
                Some(_) => {
                    if let Some(c) = self.form() {
                        if self.nodes[c.0 as usize].kind != Kind::Uneval || kind == Kind::Uneval && false {
                            got += 1;
                        }
                        self.stack.push(c);
                    }
                    if self.stop {
                        break;
                    }
                }
            }
        }
        self.close(id, base, self.i);
        id
    }

    fn sharp(&mut self) -> Option<NodeId> {
        let start = self.i;
        let Some(c) = self.at(1) else {
            self.i += 1;
            self.err_at(self.i, "Unexpected EOF.".to_string());
            return None;
        };
        Some(match c {
            b'#' => {
                let e = self.scan_token(start + 2, false);
                let name = intern(std::str::from_utf8(&self.s[start + 2..e]).unwrap_or(""));
                self.i = e;
                self.token(Kind::Symbolic, 0, start, e, name, SymId::NONE)
            }
            b'{' => self.delim(Kind::Set, 0, 2, b'}'),
            b'(' => self.delim(Kind::AnonFn, 0, 2, b')'),
            b'"' => {
                self.i += 1;
                self.string(Kind::Regex, start)
            }
            b'^' => self.wrap(Kind::Meta, 2, 2),
            b'\'' => self.wrap(Kind::Var, 2, 1),
            b'=' => self.wrap(Kind::Eval, 2, 1),
            b'_' => self.wrap(Kind::Uneval, 2, 1),
            b':' => self.ns_map(),
            b'?' => self.reader_cond(),
            _ => self.wrap(Kind::Tagged, 1, 2),
        })
    }

    fn ns_map(&mut self) -> NodeId {
        let start = self.i;
        let id = self.open(Kind::NsMap, 0, start);
        let kstart = start + 1;
        let mut j = kstart + 1;
        let mut flags = 0;
        if self.s.get(j) == Some(&b':') {
            flags = F_AUTO;
            j += 1;
        }
        let ne = {
            let mut e = j;
            while let Some(&b) = self.s.get(e) {
                if b == b'{' || ascii_ws(b) || b >= 0x80 && self.char_at(e).is_some_and(java_ws) {
                    break;
                }
                e += 1;
            }
            // keep to a char boundary
            while e < self.s.len() && (self.s[e] & 0xC0) == 0x80 {
                e += 1;
            }
            e
        };
        let name = intern(std::str::from_utf8(&self.s[j..ne]).unwrap_or(""));
        let kid = self.token(Kind::Keyword, flags, kstart, ne, name, SymId::NONE);
        self.i = ne;
        let base = self.stack.len();
        self.stack.push(kid);
        self.skip_trivia();
        match self.peek() {
            None => self.err_at(self.i, "Unexpected EOF.".to_string()),
            Some(c) if closer(c) => {}
            Some(_) => {
                if let Some(m) = self.form() {
                    self.stack.push(m);
                }
            }
        }
        self.close(id, base, self.i);
        id
    }

    fn reader_cond(&mut self) -> NodeId {
        let start = self.i;
        let (flags, w) = if self.at(2) == Some(b'@') { (F_SPLICING, 3) } else { (0, 2) };
        let id = self.open(Kind::ReaderCond, flags, start);
        self.i += w;
        let base = self.stack.len();
        self.skip_trivia();
        match self.peek() {
            None => self.err_at(self.i, "Unexpected EOF.".to_string()),
            Some(c) if closer(c) => {}
            Some(_) => {
                if let Some(m) = self.form() {
                    self.stack.push(m);
                }
            }
        }
        self.close(id, base, self.i);
        id
    }

    /// String or regex; `start` = node start (at `#` for regex), `self.i` at opening quote.
    fn string(&mut self, kind: Kind, start: usize) -> NodeId {
        let s = self.s;
        let mut j = self.i + 1;
        let mut flags = 0;
        loop {
            match s.get(j) {
                None => {
                    flags = F_ERROR;
                    break;
                }
                Some(b'"') => {
                    j += 1;
                    break;
                }
                Some(b'\\') => j += 2,
                Some(_) => j += 1,
            }
        }
        let j = j.min(s.len());
        self.i = j;
        if flags != 0 {
            self.err_at(j, "Unexpected EOF while reading string.".to_string());
        }
        self.token(kind, flags, start, j, SymId::NONE, SymId::NONE)
    }

    fn keyword(&mut self) -> NodeId {
        let start = self.i;
        let mut j = start + 1;
        let mut flags = 0;
        if j >= self.s.len() {
            self.err_at(start, "unexpected EOF while reading keyword.".to_string());
            self.i = j;
            return self.token(Kind::Keyword, F_ERROR, start, j, SymId::NONE, SymId::NONE);
        }
        if self.s[j] == b':' {
            flags = F_AUTO;
            j += 1;
        }
        // edn keyword token: ends at whitespace or `" ; ^ ( ) [ ] { } \`
        let s = self.s;
        let mut e = j;
        while let Some(&b) = s.get(e) {
            if b < 0x80 {
                if ascii_ws(b) || matches!(b, b'"' | b';' | b'^' | b'(' | b')' | b'[' | b']' | b'{' | b'}' | b'\\') {
                    break;
                }
                e += 1;
            } else {
                match self.char_at(e) {
                    Some(c) if java_ws(c) => break,
                    Some(c) => e += c.len_utf8(),
                    None => e += 1,
                }
            }
        }
        self.i = e;
        if e == j {
            self.err_at(start, "Invalid token: :".to_string());
            return self.token(Kind::Keyword, flags | F_ERROR, start, e, SymId::NONE, SymId::NONE);
        }
        let tok = std::str::from_utf8(&s[j..e]).unwrap_or("");
        let (ns, name) = split_ns(tok);
        let nsid = if ns.is_empty() { SymId::NONE } else { intern(ns) };
        self.token(Kind::Keyword, flags, start, e, intern(name), nsid)
    }

    fn char_lit(&mut self) -> NodeId {
        let start = self.i;
        let mut j = start + 1;
        match self.char_at(j) {
            None => {
                self.err_at(j, "Unexpected EOF".to_string());
                self.i = j;
                return self.token(Kind::Char, F_ERROR, start, j, SymId::NONE, SymId::NONE);
            }
            Some(c) => {
                j += c.len_utf8();
                if c != '\\' {
                    j = self.scan_token(j, false);
                }
            }
        }
        self.i = j;
        self.token(Kind::Char, 0, start, j, SymId::NONE, SymId::NONE)
    }

    fn symbol_or_number(&mut self) -> NodeId {
        let start = self.i;
        let s = self.s;
        let b0 = s[start];
        let numeric = b0.is_ascii_digit() || ((b0 == b'+' || b0 == b'-') && s.get(start + 1).is_some_and(|d| d.is_ascii_digit()));
        // first char is always consumed (a bare boundary char cannot reach here)
        let first_len = if b0 < 0x80 { 1 } else { self.char_at(start).map_or(1, |c| c.len_utf8()) };
        let e = self.scan_token(start + first_len, !numeric);
        self.i = e;
        let tok = std::str::from_utf8(&s[start..e]).unwrap_or("");
        if numeric {
            let t = classify_number(tok);
            if t == NUM_INVALID {
                self.err_at(start, format!("Invalid number: {}.", tok));
            }
            return self.token(Kind::Number, t, start, e, SymId::NONE, SymId::NONE);
        }
        let kind = match tok {
            "nil" => Some(Kind::Nil),
            "true" => Some(Kind::True),
            "false" => Some(Kind::False),
            _ => None,
        };
        if let Some(k) = kind {
            return self.token(k, 0, start, e, SymId::NONE, SymId::NONE);
        }
        let (ns, name) = split_ns(tok);
        let nsid = if ns.is_empty() { SymId::NONE } else { intern(ns) };
        self.token(Kind::Symbol, 0, start, e, intern(name), nsid)
    }
}

/// Classify a numeric token (Clojure number grammar); `NUM_INVALID` if it does not match.
fn classify_number(tok: &str) -> u8 {
    let t = tok.strip_prefix(['+', '-']).unwrap_or(tok);
    let all = |x: &str, f: fn(&u8) -> bool| !x.is_empty() && x.bytes().all(|b| f(&b));
    let dig = |b: &u8| b.is_ascii_digit();
    if let Some((n, d)) = t.split_once('/') {
        return if all(n, dig) && all(d, dig) { NUM_RATIO } else { NUM_INVALID };
    }
    if let Some(b) = t.strip_suffix('N') {
        return if is_int(b) { NUM_BIGINT } else { NUM_INVALID };
    }
    if let Some(b) = t.strip_suffix('M') {
        return if is_float(b) { NUM_BIGDEC } else { NUM_INVALID };
    }
    if is_int(t) {
        NUM_INT
    } else if is_float(t) {
        NUM_FLOAT
    } else {
        NUM_INVALID
    }
}

fn is_int(t: &str) -> bool {
    let b = t.as_bytes();
    if b.is_empty() {
        return false;
    }
    if t == "0" {
        return true;
    }
    if b[0].is_ascii_digit() && b[0] != b'0' && b.iter().all(u8::is_ascii_digit) {
        return true;
    }
    if b.len() > 2 && b[0] == b'0' && (b[1] == b'x' || b[1] == b'X') {
        return b[2..].iter().all(u8::is_ascii_hexdigit);
    }
    if b.len() > 1 && b[0] == b'0' {
        return b[1..].iter().all(|c| (b'0'..=b'7').contains(c));
    }
    // radix: [1-9][0-9]?[rR][0-9A-Za-z]+
    let r = b.iter().position(|&c| c == b'r' || c == b'R');
    if let Some(r) = r {
        let (h, tail) = (&b[..r], &b[r + 1..]);
        return (1..=2).contains(&h.len())
            && h[0] != b'0'
            && h.iter().all(u8::is_ascii_digit)
            && !tail.is_empty()
            && tail.iter().all(u8::is_ascii_alphanumeric);
    }
    false
}

fn is_float(t: &str) -> bool {
    let b = t.as_bytes();
    let mut i = 0;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    if i == 0 {
        return false;
    }
    if i < b.len() && b[i] == b'.' {
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        i += 1;
        if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
            i += 1;
        }
        let d = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == d {
            return false;
        }
    }
    i == b.len()
}
