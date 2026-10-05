//! rewrite-clj-compatible parser producing the trivia-preserving `Tree` (see rewrite_clj/parser/core.cljc).

use super::tree::{Tag, Tree};

#[derive(Debug)]
pub struct ParseErr(pub String);

type R<T> = Result<T, ParseErr>;

fn err<T>(m: &str, at: usize) -> R<T> {
    Err(ParseErr(format!("{} at byte {}", m, at)))
}

/// `Character/isWhitespace` for a char (comma excluded).
#[inline]
pub fn java_ws(c: char) -> bool {
    matches!(c, '\u{9}'..='\u{d}' | '\u{1c}'..='\u{1f}' | ' ' | '\u{1680}' | '\u{2000}'..='\u{2006}' | '\u{2008}'..='\u{200a}'
        | '\u{2028}' | '\u{2029}' | '\u{205f}' | '\u{3000}')
}

struct P<'a, 'b> {
    b: &'a [u8],
    src: &'a str,
    pos: usize,
    t: &'b mut Tree<'a>,
}

const BOUNDARY: &[u8] = b"\":;'@^`~()[]{}\\";

impl<'a, 'b> P<'a, 'b> {
    /// Whitespace char at `pos` (incl. comma): returns byte length or 0.
    #[inline]
    fn ws_len(&self, pos: usize) -> usize {
        match self.b.get(pos) {
            None => 0,
            Some(&c) if c < 0x80 => (c == b',' || java_ws(c as char)) as usize,
            Some(_) => {
                let ch = self.src[pos..].chars().next().unwrap();
                if java_ws(ch) { ch.len_utf8() } else { 0 }
            }
        }
    }
    /// Not whitespace/comma and not a boundary char.
    #[inline]
    fn token_char_len(&self, pos: usize, allow_extra: bool) -> usize {
        match self.b.get(pos) {
            None => 0,
            Some(&c) if c < 0x80 => {
                if c == b',' || java_ws(c as char) {
                    0
                } else if BOUNDARY.contains(&c) {
                    if allow_extra && (c == b'\'' || c == b':') { 1 } else { 0 }
                } else {
                    1
                }
            }
            Some(_) => {
                let ch = self.src[pos..].chars().next().unwrap();
                if java_ws(ch) { 0 } else { ch.len_utf8() }
            }
        }
    }
    #[inline]
    fn char_len_at(&self, pos: usize) -> usize {
        match self.b.get(pos) {
            None => 0,
            Some(&c) if c < 0x80 => 1,
            Some(_) => self.src[pos..].chars().next().unwrap().len_utf8(),
        }
    }

    fn leaf(&mut self, tag: Tag, s: usize, e: usize) -> u32 {
        self.t.add(tag, s as u32, e as u32)
    }

    fn container(&mut self, tag: Tag, kids: Vec<u32>) -> u32 {
        let id = self.t.add(tag, 0, 0);
        self.t.set_children(id, &kids);
        id
    }

    /// parse-next: Ok(None) = EOF at top level or the closing delimiter consumed.
    fn parse_next(&mut self, delim: u8) -> R<Option<u32>> {
        let p = self.pos;
        let c = match self.b.get(p) {
            None => {
                return if delim != 0 { err("Unexpected EOF.", p) } else { Ok(None) };
            }
            Some(&c) => c,
        };
        if self.ws_len(p) > 0 {
            return Ok(Some(self.parse_ws()));
        }
        if c == delim && delim != 0 {
            self.pos += 1;
            return Ok(None);
        }
        let n = match c {
            b'^' => {
                self.pos += 1;
                let k = self.printables(delim, 2)?;
                self.container(Tag::Meta, k)
            }
            b'#' => self.sharp(delim)?,
            b'(' => self.delim_seq(Tag::List, b')')?,
            b'[' => self.delim_seq(Tag::Vector, b']')?,
            b'{' => self.delim_seq(Tag::Map, b'}')?,
            b'}' | b']' | b')' => return err("Unmatched delimiter", p),
            b'~' => {
                self.pos += 1;
                if self.b.get(self.pos) == Some(&b'@') {
                    self.pos += 1;
                    let k = self.printables(delim, 1)?;
                    self.container(Tag::UnquoteSplicing, k)
                } else {
                    let k = self.printables(delim, 1)?;
                    self.container(Tag::Unquote, k)
                }
            }
            b'\'' => {
                self.pos += 1;
                let k = self.printables(delim, 1)?;
                self.container(Tag::Quote, k)
            }
            b'`' => {
                self.pos += 1;
                let k = self.printables(delim, 1)?;
                self.container(Tag::SyntaxQuote, k)
            }
            b';' => self.comment(p),
            b'@' => {
                self.pos += 1;
                let k = self.printables(delim, 1)?;
                self.container(Tag::Deref, k)
            }
            b'"' => self.string(p, p)?,
            b':' => self.keyword()?,
            _ => self.token()?,
        };
        Ok(Some(n))
    }

    fn parse_ws(&mut self) -> u32 {
        let s = self.pos;
        let c = self.b[s];
        if c == b'\n' || c == b'\r' {
            while matches!(self.b.get(self.pos), Some(b'\n') | Some(b'\r')) {
                self.pos += 1;
            }
            return self.leaf(Tag::Newline, s, self.pos);
        }
        if c == b',' {
            while self.b.get(self.pos) == Some(&b',') {
                self.pos += 1;
            }
            return self.leaf(Tag::Comma, s, self.pos);
        }
        // spaces: whitespace that is not newline and not comma (note: \r counts, as in rewrite-clj)
        loop {
            let l = self.ws_len(self.pos);
            if l == 0 || self.b[self.pos] == b',' || self.b[self.pos] == b'\n' {
                break;
            }
            self.pos += l;
        }
        self.leaf(Tag::Space, s, self.pos)
    }

    fn comment(&mut self, start: usize) -> u32 {
        // from ';' (or '#!') to linebreak inclusive
        let mut p = start;
        while p < self.b.len() && self.b[p] != b'\n' && self.b[p] != b'\r' {
            p += 1;
        }
        if p < self.b.len() {
            p += 1;
        }
        self.pos = p;
        self.leaf(Tag::Comment, start, p)
    }

    /// Reads string data starting at the opening quote (`q`); node starts at `node_start`.
    fn string_end(&mut self, q: usize) -> R<(usize, bool)> {
        let mut p = q + 1;
        let mut esc = false;
        let mut ml = false;
        loop {
            let c = match self.b.get(p) {
                None => return err("Unexpected EOF while reading string.", p),
                Some(&c) => c,
            };
            p += 1;
            if !esc && c == b'"' {
                return Ok((p, ml));
            }
            if c == b'\n' {
                ml = true;
            }
            esc = !esc && c == b'\\';
        }
    }

    fn string(&mut self, node_start: usize, q: usize) -> R<u32> {
        let (e, ml) = self.string_end(q)?;
        self.pos = e;
        Ok(self.leaf(if ml { Tag::MStr } else { Tag::Str }, node_start, e))
    }

    fn keyword(&mut self) -> R<u32> {
        let s = self.pos;
        let mut p = s + 1;
        if self.b.get(p) == Some(&b':') {
            p += 1;
        }
        // edn read-token: first char consumed, stops at ws / terminating macro chars
        let start = p;
        loop {
            let c = match self.b.get(p) {
                None => break,
                Some(&c) => c,
            };
            if self.ws_len(p) > 0 {
                break;
            }
            if matches!(c, b'"' | b';' | b'^' | b'(' | b')' | b'[' | b']' | b'{' | b'}' | b'\\') {
                break;
            }
            if matches!(c, b'@' | b'`' | b'~') {
                return err("bad char in keyword", p);
            }
            p += self.char_len_at(p);
        }
        if p == start {
            return err("invalid keyword", s);
        }
        self.pos = p;
        Ok(self.leaf(Tag::Kw, s, p))
    }

    fn token(&mut self) -> R<u32> {
        let s = self.pos;
        let first = self.b[s];
        let mut p = s + self.char_len_at(s);
        if first == b'\\' {
            if p >= self.b.len() {
                return err("Unexpected EOF", p);
            }
            let cl = self.char_len_at(p);
            let c2 = self.b[p];
            p += cl;
            if c2 != b'\\' {
                p = self.read_token_chars(p, false);
            }
        } else {
            p = self.read_token_chars(p, false);
        }
        let mut tag = Tag::Tok;
        if first != b'\\' && first != b'#' {
            let c2 = self.b.get(s + 1).copied().unwrap_or(0);
            let numeric = first.is_ascii_digit() || ((first == b'+' || first == b'-') && c2.is_ascii_digit());
            let txt = &self.src[s..p];
            if !numeric && txt != "nil" && txt != "true" && txt != "false" {
                p = self.read_token_chars(p, true);
                tag = Tag::Sym;
            }
        }
        self.pos = p;
        Ok(self.leaf(tag, s, p))
    }

    fn read_token_chars(&self, mut p: usize, extra: bool) -> usize {
        loop {
            let l = self.token_char_len(p, extra);
            if l == 0 {
                return p;
            }
            p += l;
        }
    }

    fn printables(&mut self, delim: u8, n: usize) -> R<Vec<u32>> {
        let mut kids = Vec::with_capacity(n + 1);
        let mut c = 0;
        while c < n {
            match self.parse_next(delim)? {
                None => return err("node expects more values", self.pos),
                Some(v) => {
                    let tg = self.t.tag(v);
                    if !(tg.is_ws() || tg == Tag::Comment || tg == Tag::Uneval) {
                        c += 1;
                    }
                    kids.push(v);
                }
            }
        }
        Ok(kids)
    }

    fn delim_seq(&mut self, tag: Tag, close: u8) -> R<u32> {
        self.pos += if tag == Tag::Set || tag == Tag::Fn { 2 } else { 1 };
        self.seq_body(tag, close)
    }

    fn seq_body(&mut self, tag: Tag, close: u8) -> R<u32> {
        let mut kids = Vec::new();
        while let Some(v) = self.parse_next(close)? {
            kids.push(v);
        }
        Ok(self.container(tag, kids))
    }

    fn sharp(&mut self, delim: u8) -> R<u32> {
        let start = self.pos;
        self.pos += 1;
        let c = match self.b.get(self.pos) {
            None => return err("Unexpected EOF.", self.pos),
            Some(&c) => c,
        };
        match c {
            b'#' => {
                // symbolic value token ##Inf: starts at first '#'
                self.pos = start + 1;
                let mut p = self.pos + 1;
                p = self.read_token_chars(p, false);
                self.pos = p;
                Ok(self.leaf(Tag::Tok, start, p))
            }
            b'!' => Ok(self.comment(start)),
            b'{' => {
                self.pos += 1;
                self.seq_body(Tag::Set, b'}')
            }
            b'(' => {
                self.pos += 1;
                self.seq_body(Tag::Fn, b')')
            }
            b'"' => {
                let q = self.pos;
                let (e, _) = self.string_end(q)?;
                self.pos = e;
                Ok(self.leaf(Tag::Regex, start, e))
            }
            b'^' => {
                self.pos += 1;
                let k = self.printables(delim, 2)?;
                Ok(self.container(Tag::MetaStar, k))
            }
            b'\'' => {
                self.pos += 1;
                let k = self.printables(delim, 1)?;
                Ok(self.container(Tag::Var, k))
            }
            b'=' => {
                self.pos += 1;
                let k = self.printables(delim, 1)?;
                Ok(self.container(Tag::Eval, k))
            }
            b'_' => {
                self.pos += 1;
                let k = self.printables(delim, 1)?;
                Ok(self.container(Tag::Uneval, k))
            }
            b':' => self.nsmap(),
            b'?' => {
                self.pos += 1;
                let q = self.pos - 1;
                let first;
                match self.b.get(self.pos) {
                    Some(b'(') => first = self.leaf(Tag::Sym, q, q + 1),
                    Some(b'@') => {
                        self.pos += 1;
                        first = self.leaf(Tag::Sym, q, q + 2);
                    }
                    _ => {
                        self.pos = q;
                        let v = self.printables(delim, 1)?;
                        first = v[0];
                    }
                }
                let mut kids = vec![first];
                kids.extend(self.printables(delim, 1)?);
                Ok(self.container(Tag::ReaderMacro, kids))
            }
            _ => {
                let k = self.printables(delim, 2)?;
                Ok(self.container(Tag::ReaderMacro, k))
            }
        }
    }

    fn nsmap(&mut self) -> R<u32> {
        // at ':' after '#'
        let qs = self.pos;
        let mut p = qs + 1;
        let mut auto = false;
        if self.b.get(p) == Some(&b':') {
            auto = true;
            p += 1;
        }
        let ps = p;
        loop {
            let c = match self.b.get(p) {
                None => break,
                Some(&c) => c,
            };
            if self.ws_len(p) > 0 || (c < 0x80 && BOUNDARY.contains(&c)) {
                break;
            }
            p += self.char_len_at(p);
        }
        if !auto && p == ps {
            return err("namespaced map expects a namespace", qs);
        }
        let q = self.leaf(Tag::Qual, qs, p);
        self.pos = p;
        let mut kids = vec![q];
        loop {
            match self.parse_next(0)? {
                None => return err("namespaced map expects a map", self.pos),
                Some(n) => {
                    let tg = self.t.tag(n);
                    kids.push(n);
                    if !tg.is_ws() {
                        if tg != Tag::Map {
                            return err("namespaced map expects a map", self.pos);
                        }
                        break;
                    }
                }
            }
        }
        Ok(self.container(Tag::NsMap, kids))
    }
}

/// Parse `src` into a tree; root id is 0 (`Tag::Forms`).
pub fn parse(src: &str) -> Result<Tree<'_>, ParseErr> {
    let mut t = Tree::new(src);
    let kids = {
        let mut p = P { b: src.as_bytes(), src, pos: 0, t: &mut t };
        let mut kids = Vec::new();
        while let Some(v) = p.parse_next(0)? {
            kids.push(v);
        }
        kids
    };
    t.set_children(0, &kids);
    Ok(t)
}

/// rewrite-clj's `string-reader` normalizes CRLF, CR+FF and lone CR to LF while reading.
pub(crate) fn normalize_reader_newlines(s: &str) -> std::borrow::Cow<'_, str> {
    if !s.contains('\r') {
        return std::borrow::Cow::Borrowed(s);
    }
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    let mut last = 0;
    while i < b.len() {
        if b[i] == b'\r' {
            out.push_str(&s[last..i]);
            out.push('\n');
            i += 1;
            if i < b.len() && (b[i] == b'\n' || b[i] == 0x0c) {
                i += 1;
            }
            last = i;
        } else {
            i += 1;
        }
    }
    out.push_str(&s[last..]);
    std::borrow::Cow::Owned(out)
}
