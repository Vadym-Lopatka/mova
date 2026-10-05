//! Java source -> kondo `java-class-definitions` / `java-member-definitions` (what clj-kondo's JavaParser pass reports).
//! Mirrors `clj-kondo.impl.analysis.java/source-is->java-member-definitions`: every type declaration with a FQN is a
//! class; its members are ALL non-private fields, constructors, methods and enum constants found anywhere inside it
//! (nested and anonymous classes included), in the order fields, constructors, methods, enum constants.
//! Members are stored once per file in source order; a class owns the contiguous run `lo..hi` of its source extent.

pub const F_PUBLIC: u16 = 1;
pub const F_STATIC: u16 = 2;
pub const F_FINAL: u16 = 4;
pub const F_FIELD: u16 = 8;
pub const F_METHOD: u16 = 16;
/// Category (kondo concat order) in bits 8-9: 0 field, 1 constructor, 2 method, 3 enum constant.
pub const CAT_SHIFT: u16 = 8;

#[derive(Clone, Debug, Default)]
pub struct MemberRec {
    pub name: String,
    /// Field type or method return type (`Type.asString`).
    pub ty: Option<String>,
    /// Parameters (`Parameter.toString`) joined with `\u{1f}`; None for fields.
    pub params: Option<String>,
    pub flags: u16,
    pub row: u32,
    pub col: u32,
    pub end_row: u32,
    pub end_col: u32,
    /// Byte span of the attached comment in the source (0,0 = none).
    pub doc: (u32, u32),
}

#[derive(Default)]
pub struct FileParse {
    /// (binary class name with `$`, lo, hi) into `members`.
    pub classes: Vec<(String, u32, u32)>,
    pub members: Vec<MemberRec>,
    /// The file declares an enum somewhere: clojure-lsp's JavaParser pass drops the whole file (no classes, no members).
    pub has_enum: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum K {
    Id,
    P(u8),
    Lit,
}

#[derive(Clone, Copy)]
struct Tok {
    s: u32,
    e: u32,
    k: K,
}

struct Lexed {
    toks: Vec<Tok>,
    /// (start, end, is_line_comment)
    comments: Vec<(u32, u32, bool)>,
    /// Byte offsets of line starts.
    lines: Vec<u32>,
}

fn lex(src: &str) -> Lexed {
    let b = src.as_bytes();
    let mut toks = Vec::with_capacity(b.len() / 5);
    let mut comments = Vec::new();
    let mut lines = vec![0u32];
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        match c {
            b'\n' => {
                i += 1;
                lines.push(i as u32);
            }
            b' ' | b'\t' | b'\r' | 0x0c => i += 1,
            b'/' if b.get(i + 1) == Some(&b'/') => {
                let s = i;
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
                let mut e = i;
                if e > s && b[e - 1] == b'\r' {
                    e -= 1;
                }
                comments.push((s as u32, e as u32, true));
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                let s = i;
                i += 2;
                while i < b.len() && !(b[i] == b'*' && b.get(i + 1) == Some(&b'/')) {
                    if b[i] == b'\n' {
                        lines.push(i as u32 + 1);
                    }
                    i += 1;
                }
                i = (i + 2).min(b.len());
                comments.push((s as u32, i as u32, false));
            }
            b'"' => {
                let s = i;
                if b[i..].starts_with(b"\"\"\"") {
                    i += 3;
                    while i < b.len() && !b[i..].starts_with(b"\"\"\"") {
                        if b[i] == b'\\' {
                            i += 1;
                        }
                        if i < b.len() && b[i] == b'\n' {
                            lines.push(i as u32 + 1);
                        }
                        i += 1;
                    }
                    i = (i + 3).min(b.len());
                } else {
                    i += 1;
                    while i < b.len() && b[i] != b'"' && b[i] != b'\n' {
                        i += if b[i] == b'\\' { 2 } else { 1 };
                    }
                    i = (i + 1).min(b.len());
                }
                toks.push(Tok { s: s as u32, e: i as u32, k: K::Lit });
            }
            b'\'' => {
                let s = i;
                i += 1;
                while i < b.len() && b[i] != b'\'' && b[i] != b'\n' {
                    i += if b[i] == b'\\' { 2 } else { 1 };
                }
                i = (i + 1).min(b.len());
                toks.push(Tok { s: s as u32, e: i as u32, k: K::Lit });
            }
            b'0'..=b'9' => {
                let s = i;
                let hex = c == b'0' && matches!(b.get(i + 1), Some(b'x') | Some(b'X'));
                while i < b.len() {
                    let d = b[i];
                    let exp_sign = (d == b'+' || d == b'-') && !hex && matches!(b[i - 1], b'e' | b'E');
                    if d.is_ascii_alphanumeric() || d == b'_' || exp_sign || (d == b'.' && b.get(i + 1).map_or(false, |x| x.is_ascii_digit())) {
                        i += 1;
                    } else {
                        break;
                    }
                }
                toks.push(Tok { s: s as u32, e: i as u32, k: K::Lit });
            }
            _ if c.is_ascii_alphabetic() || c == b'_' || c == b'$' || c >= 0x80 => {
                let s = i;
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'$' || b[i] >= 0x80) {
                    i += 1;
                }
                toks.push(Tok { s: s as u32, e: i as u32, k: K::Id });
            }
            _ => {
                toks.push(Tok { s: i as u32, e: i as u32 + 1, k: K::P(c) });
                i += 1;
            }
        }
    }
    Lexed { toks, comments, lines }
}

#[derive(Clone)]
enum Ctx {
    Top,
    Nested(String),
    Unreg,
}

struct P<'a> {
    src: &'a str,
    t: Vec<Tok>,
    comments: Vec<(u32, u32, bool)>,
    lines: Vec<u32>,
    pkg: String,
    out: FileParse,
    taken: std::cell::RefCell<std::collections::HashSet<u32>>,
}

fn modifier_flag(s: &str) -> Option<u16> {
    Some(match s {
        "public" => F_PUBLIC,
        "static" => F_STATIC,
        "final" => F_FINAL,
        "private" => 0x100 << 4, // marker, stripped before storing
        "protected" | "abstract" | "native" | "synchronized" | "transient" | "volatile" | "strictfp" | "default" | "sealed" => 0,
        _ => return None,
    })
}
const PRIVATE_MARK: u16 = 0x100 << 4;

impl<'a> P<'a> {
    fn txt(&self, i: usize) -> &'a str {
        let t = self.t[i];
        &self.src[t.s as usize..t.e as usize]
    }
    fn id(&self, i: usize, w: &str) -> bool {
        matches!(self.t.get(i), Some(t) if t.k == K::Id && &self.src[t.s as usize..t.e as usize] == w)
    }
    fn is_id(&self, i: usize) -> bool {
        matches!(self.t.get(i), Some(t) if t.k == K::Id)
    }
    fn p(&self, i: usize, c: u8) -> bool {
        matches!(self.t.get(i), Some(t) if t.k == K::P(c))
    }
    fn n(&self) -> usize {
        self.t.len()
    }

    /// 1-based (line, column) of a byte offset.
    fn pos(&self, off: u32) -> (u32, u32) {
        let li = match self.lines.binary_search(&off) {
            Ok(i) => i,
            Err(i) => i - 1,
        };
        let ls = self.lines[li] as usize;
        let col = self.src.get(ls..off as usize).map_or(off as usize - ls, |s| s.chars().count());
        (li as u32 + 1, col as u32 + 1)
    }

    /// Index after the group whose opener is at `i` (`(`/`[`/`{`/`<`); counts only that bracket kind.
    fn skip_group(&self, i: usize, open: u8, close: u8) -> usize {
        let mut d = 0i32;
        let mut j = i;
        while j < self.n() {
            if self.p(j, open) {
                d += 1;
            } else if self.p(j, close) {
                d -= 1;
                if d == 0 {
                    return j + 1;
                }
            } else if open == b'<' && (self.p(j, b';') || self.p(j, b'{') || self.p(j, b'}')) {
                return j; // not a type-argument list
            }
            j += 1;
        }
        self.n()
    }

    fn skip_annotation(&self, mut i: usize) -> usize {
        // at `@`
        i += 1;
        if self.is_id(i) {
            i += 1;
        }
        while self.p(i, b'.') && self.is_id(i + 1) {
            i += 2;
        }
        if self.p(i, b'(') {
            i = self.skip_group(i, b'(', b')');
        }
        i
    }

    /// Generic scan to the matching `close` (or `;` for field initializers), registering anonymous and local classes.
    fn scan(&mut self, mut i: usize, close: u8) -> usize {
        while i < self.n() {
            let tk = self.t[i];
            match tk.k {
                K::P(c) if c == close => return i + 1,
                K::P(b'{') => i = self.scan(i + 1, b'}'),
                K::P(b'(') => i = self.scan(i + 1, b')'),
                K::P(b'[') => i = self.scan(i + 1, b']'),
                K::P(b'}') | K::P(b')') | K::P(b']') if close != b';' => return i, // unbalanced: let the caller resync
                K::P(b'}') if close == b';' => return i,
                K::Id => {
                    let w = self.txt(i);
                    let after_dot = i > 0 && self.p(i - 1, b'.');
                    match w {
                        "new" => {
                            let mut j = i + 1;
                            while j < self.n() {
                                if self.p(j, b'@') {
                                    j = self.skip_annotation(j);
                                } else if self.is_id(j) {
                                    j += 1;
                                } else if self.p(j, b'.') {
                                    j += 1;
                                } else if self.p(j, b'<') {
                                    j = self.skip_group(j, b'<', b'>');
                                } else {
                                    break;
                                }
                            }
                            if self.p(j, b'(') {
                                let k = self.scan(j + 1, b')');
                                if self.p(k, b'{') {
                                    i = self.class_body(k + 1, Ctx::Unreg, 0);
                                } else {
                                    i = k;
                                }
                            } else {
                                i = j.max(i + 1);
                            }
                        }
                        "class" | "interface" | "enum" if !after_dot && self.is_id(i + 1) => i = self.type_decl(i, Ctx::Unreg),
                        "record" if !after_dot && self.is_id(i + 1) && (self.p(i + 2, b'(') || self.p(i + 2, b'<')) => i = self.type_decl(i, Ctx::Unreg),
                        _ => i += 1,
                    }
                }
                _ => i += 1,
            }
        }
        i
    }

    /// `class|interface|enum|record` at `i` (after modifiers). Returns the index after the body.
    fn type_decl(&mut self, i: usize, ctx: Ctx) -> usize {
        let kw = self.txt(i);
        let Some(name_i) = (self.is_id(i + 1)).then_some(i + 1) else { return i + 1 };
        let name = self.txt(name_i);
        let fqn = match &ctx {
            Ctx::Top => Some(if self.pkg.is_empty() { name.to_string() } else { format!("{}.{}", self.pkg, name) }),
            Ctx::Nested(p) => Some(format!("{p}${name}")),
            Ctx::Unreg => None,
        };
        let mut j = name_i + 1;
        while j < self.n() && !self.p(j, b'{') {
            if self.p(j, b';') {
                return j + 1; // not a declaration after all
            }
            j += 1;
        }
        if j >= self.n() {
            return j;
        }
        let lo = self.out.members.len() as u32;
        let inner = match &fqn {
            Some(f) => Ctx::Nested(f.clone()),
            None => Ctx::Unreg,
        };
        if kw == "enum" {
            self.out.has_enum = true;
        }
        let kind = if kw == "enum" { 1 } else if i > 0 && self.p(i - 1, b'@') { 2 } else { 0 };
        let end = self.class_body(j + 1, inner, kind);
        if let Some(f) = fqn {
            self.out.classes.push((f, lo, self.out.members.len() as u32));
        }
        end
    }

    /// Members of a type body (after `{`). Returns the index after the closing `}`.
    fn class_body(&mut self, mut i: usize, ctx: Ctx, kind: u8) -> usize {
        let is_annotation = kind == 2;
        if kind == 1 {
            i = self.enum_constants(i);
        }
        while i < self.n() {
            if self.p(i, b'}') {
                return i + 1;
            }
            if self.p(i, b';') {
                i += 1;
                continue;
            }
            let start = i;
            let slot = self.out.members.len();
            let mut flags: u16 = 0;
            loop {
                if self.p(i, b'@') && !self.id(i + 1, "interface") {
                    i = self.skip_annotation(i);
                } else if self.id(i, "non") && self.p(i + 1, b'-') && self.id(i + 2, "sealed") {
                    i += 3;
                } else if self.is_id(i) {
                    match modifier_flag(self.txt(i)) {
                        Some(f) => {
                            flags |= f;
                            i += 1;
                        }
                        None => break,
                    }
                } else {
                    break;
                }
            }
            // nested type
            if self.id(i, "class") || self.id(i, "interface") || self.id(i, "enum") || (self.p(i, b'@') && self.id(i + 1, "interface")) || (self.id(i, "record") && self.is_id(i + 1) && (self.p(i + 2, b'(') || self.p(i + 2, b'<'))) {
                let at = if self.p(i, b'@') { i + 1 } else { i };
                i = self.type_decl(at, ctx.clone());
                continue;
            }
            if self.p(i, b'{') {
                i = self.scan(i + 1, b'}');
                continue;
            }
            // member
            let mut j = i;
            if self.p(j, b'<') {
                j = self.skip_group(j, b'<', b'>');
            }
            // constructor / compact constructor
            if self.is_id(j) && self.p(j + 1, b'(') {
                let name = self.txt(j).to_string();
                let close = self.skip_group(j + 1, b'(', b')');
                let params = self.params(j + 2, close - 1);
                let (k, end) = self.after_params(close);
                self.record(slot, start, end, flags, MemberRec { name, params: Some(params), ..Default::default() }, 1);
                i = k;
                continue;
            }
            if self.is_id(j) && self.p(j + 1, b'{') && matches!(ctx, Ctx::Nested(_) | Ctx::Unreg) {
                // record compact constructor
                i = self.scan(j + 2, b'}');
                continue;
            }
            let Some(ty_end) = self.parse_type(j) else {
                i = start.max(i) + 1;
                continue;
            };
            if !self.is_id(ty_end) {
                i = ty_end.max(start + 1);
                continue;
            }
            let name = self.txt(ty_end).to_string();
            let mut ty = self.type_str(j, ty_end, ",");
            let mut after = ty_end + 1;
            while self.p(after, b'[') && self.p(after + 1, b']') {
                ty.push_str("[]");
                after += 2;
            }
            if self.p(ty_end + 1, b'(') {
                let close = self.skip_group(ty_end + 1, b'(', b')');
                let params = self.params(ty_end + 2, close - 1);
                let (k, end) = self.after_params(close);
                if !is_annotation {
                    self.record(slot, start, end, flags, MemberRec { name, ty: Some(ty), params: Some(params), ..Default::default() }, 2);
                }
                i = k;
            } else {
                let k = self.scan(after, b';');
                let end = k.saturating_sub(1).max(ty_end);
                self.record(slot, start, end, flags | F_FIELD, MemberRec { name, ty: Some(ty), ..Default::default() }, 0);
                i = k;
            }
        }
        i
    }

    /// Tokens after `)` of a method/constructor: `[]`, `throws ...`, `default ...`, then `{...}` or `;`.
    /// Returns (index after, index of the last token of the declaration).
    fn after_params(&mut self, mut i: usize) -> (usize, usize) {
        while i < self.n() {
            if self.p(i, b'{') {
                let k = self.scan(i + 1, b'}');
                return (k, k.saturating_sub(1));
            }
            if self.p(i, b';') {
                return (i + 1, i);
            }
            if self.p(i, b'}') {
                return (i, i.saturating_sub(1));
            }
            if self.p(i, b'(') {
                i = self.skip_group(i, b'(', b')');
            } else {
                i += 1;
            }
        }
        (i, i.saturating_sub(1))
    }

    /// Enum constants up to `;` (members follow) or `}`. Returns the index of the first member token / `}`.
    fn enum_constants(&mut self, mut i: usize) -> usize {
        loop {
            let start = i;
            let slot = self.out.members.len();
            while self.p(i, b'@') {
                i = self.skip_annotation(i);
            }
            if !self.is_id(i) {
                break;
            }
            let name = self.txt(i).to_string();
            let mut last = i;
            i += 1;
            if self.p(i, b'(') {
                i = self.scan(i + 1, b')');
                last = i - 1;
            }
            if self.p(i, b'{') {
                i = self.class_body(i + 1, Ctx::Unreg, 0);
                last = i - 1;
            }
            self.record(slot, start, last, 0, MemberRec { name, ..Default::default() }, 3);
            if self.p(i, b',') {
                i += 1;
                continue;
            }
            break;
        }
        if self.p(i, b';') {
            i += 1;
        }
        i
    }

    /// Type starting at `i`: returns the index after it.
    fn parse_type(&self, mut i: usize) -> Option<usize> {
        while self.p(i, b'@') {
            i = self.skip_annotation(i);
        }
        if !self.is_id(i) {
            return None;
        }
        i += 1;
        loop {
            if self.p(i, b'<') {
                let k = self.skip_group(i, b'<', b'>');
                if k <= i || !self.p(k - 1, b'>') {
                    return None;
                }
                i = k;
            }
            if self.p(i, b'.') && self.is_id(i + 1) {
                i += 2;
                continue;
            }
            break;
        }
        while self.p(i, b'[') && self.p(i + 1, b']') {
            i += 2;
        }
        Some(i)
    }

    /// `Type.asString()` of tokens `a..b`.
    fn type_str(&self, a: usize, b: usize, comma: &str) -> String {
        let mut s = String::new();
        let mut i = a;
        while i < b {
            if self.p(i, b'@') {
                i = self.skip_annotation(i);
                continue;
            }
            let t = self.t[i];
            match t.k {
                K::Id => {
                    let w = self.txt(i);
                    if (w == "extends" || w == "super") && !s.is_empty() {
                        s.push(' ');
                        s.push_str(w);
                        s.push(' ');
                    } else {
                        s.push_str(w);
                    }
                }
                K::P(b',') => s.push_str(comma),
                K::P(b'&') => s.push_str(" & "),
                K::P(c) => s.push(c as char),
                K::Lit => s.push_str(self.txt(i)),
            }
            i += 1;
        }
        s
    }

    /// `Parameter.toString()` list for tokens `a..b` (inside the parentheses), joined by `\u{1f}`.
    fn params(&self, a: usize, b: usize) -> String {
        let mut lim = b;
        while lim < self.n() && !self.p(lim, b'{') && !self.p(lim, b';') {
            lim += 1;
        }
        let limit = self.t.get(lim).map_or(u32::MAX, |t| t.s);
        let mut out: Vec<String> = Vec::new();
        let mut i = a;
        let mut start = a;
        let (mut ad, mut pd) = (0i32, 0i32);
        let flush = |this: &P, s: usize, e: usize, out: &mut Vec<String>| {
            if s < e {
                if let Some(p) = this.one_param(s, e, limit) {
                    out.push(p);
                }
            }
        };
        while i < b {
            match self.t[i].k {
                K::P(b'<') => ad += 1,
                K::P(b'>') => ad -= 1,
                K::P(b'(') => pd += 1,
                K::P(b')') => pd -= 1,
                K::P(b',') if ad <= 0 && pd == 0 => {
                    flush(self, start, i, &mut out);
                    start = i + 1;
                }
                _ => {}
            }
            i += 1;
        }
        flush(self, start, b, &mut out);
        out.join("\u{1f}")
    }

    fn one_param(&self, a: usize, b: usize, limit: u32) -> Option<String> {
        let mut i = a;
        let mut pre = String::new();
        loop {
            if self.p(i, b'@') {
                let k = self.skip_annotation(i);
                for x in i..k {
                    pre.push_str(self.txt(x));
                }
                pre.push(' ');
                i = k;
            } else if self.id(i, "final") {
                pre.push_str("final ");
                i += 1;
            } else {
                break;
            }
        }
        // name = last identifier (C-style `x[]` arrays after it); `...` before it marks varargs
        let mut n = b;
        let mut dims = 0;
        while n > i && self.p(n - 1, b']') && self.p(n - 2, b'[') {
            n -= 2;
            dims += 1;
        }
        if n <= i || !self.is_id(n - 1) {
            return None;
        }
        let ni = n - 1;
        let name = self.txt(ni);
        let varargs = ni >= i + 3 && self.p(ni - 1, b'.') && self.p(ni - 2, b'.') && self.p(ni - 3, b'.');
        let te = if varargs { ni - 3 } else { ni };
        let ty = self.type_str(i, te, ", ") + &"[]".repeat(dims);
        let doc = self.param_doc(a, b - 1, limit);
        let cm = if doc.1 > doc.0 { format!("{}\n", &self.src[doc.0 as usize..doc.1 as usize]) } else { String::new() };
        Some(format!("{cm}{pre}{ty}{} {name}", if varargs { "..." } else { "" }))
    }

    fn record(&mut self, slot: usize, first: usize, last: usize, mut flags: u16, mut m: MemberRec, cat: u16) {
        if flags & PRIVATE_MARK != 0 {
            return;
        }
        flags &= !PRIVATE_MARK;
        if cat == 1 || cat == 2 {
            flags |= F_METHOD;
        }
        if cat == 3 {
            flags |= F_FIELD;
        }
        m.flags = flags | (cat << CAT_SHIFT);
        let s = self.t[first].s;
        let e = self.t[last.max(first)].e;
        let (r, c) = self.pos(s);
        let (er, ec) = self.pos(e.saturating_sub(1).max(s));
        m.row = r;
        m.col = c;
        m.end_row = er;
        m.end_col = ec;
        m.doc = self.doc_for(first, last.max(first));
        self.out.members.insert(slot, m);
    }

    fn line_of(&self, off: u32) -> u32 {
        self.pos(off).0
    }

    /// The comment right before the node starting at token `first` (after the previous token, no blank line between,
    /// not already attributed elsewhere).
    fn preceding(&self, first: usize) -> Option<(u32, u32)> {
        let node_s = self.t[first].s;
        let prev_e = if first > 0 { self.t[first - 1].e } else { 0 };
        let hi = self.comments.partition_point(|c| c.1 <= node_s);
        if hi == 0 {
            return None;
        }
        let c = self.comments[hi - 1];
        if c.0 < prev_e || self.taken.borrow().contains(&c.0) || self.line_of(c.1.saturating_sub(1)) + 1 < self.line_of(node_s) {
            return None;
        }
        Some((c.0, c.1))
    }

    /// Line comment after token `last` on the line where the node (starting at token `first`) begins.
    fn trailing(&self, first: usize, last: usize, limit: u32) -> Option<(u32, u32)> {
        let e = self.t[last].e;
        let ln = self.line_of(self.t[first].s);
        let i = self.comments.partition_point(|c| c.0 < e);
        let c = self.comments.get(i)?;
        (c.2 && c.0 < limit && self.line_of(c.0) == ln && !self.taken.borrow().contains(&c.0)).then_some((c.0, c.1))
    }

    /// Comment of a member (JavaParser, checked against the kondo oracle): a line comment after the member on the
    /// line it begins wins, else the comment right before it.
    fn doc_for(&self, first: usize, last: usize) -> (u32, u32) {
        let c = self.trailing(first, last, u32::MAX).or_else(|| self.preceding(first));
        match c {
            Some(c) => {
                self.taken.borrow_mut().insert(c.0);
                c
            }
            None => (0, 0),
        }
    }

    /// Comment of a parameter: a line comment on the parameter's first line before `limit` (the method body), which
    /// every parameter starting on that line shares; else the preceding comment.
    fn param_doc(&self, first: usize, last: usize, limit: u32) -> (u32, u32) {
        let e = self.t[last].e;
        let ln = self.line_of(self.t[first].s);
        let i = self.comments.partition_point(|c| c.0 < e);
        if let Some(c) = self.comments.get(i) {
            if c.2 && c.0 < limit && self.line_of(c.0) == ln {
                self.taken.borrow_mut().insert(c.0);
                return (c.0, c.1);
            }
        }
        match self.preceding(first) {
            Some(c) => {
                self.taken.borrow_mut().insert(c.0);
                c
            }
            None => (0, 0),
        }
    }

    fn compilation_unit(&mut self) {
        let mut i = 0;
        while i < self.n() {
            if self.id(i, "package") && self.pkg.is_empty() {
                let mut j = i + 1;
                let mut name = String::new();
                while j < self.n() && !self.p(j, b';') {
                    if self.is_id(j) {
                        name.push_str(self.txt(j));
                    } else if self.p(j, b'.') {
                        name.push('.');
                    }
                    j += 1;
                }
                self.pkg = name;
                i = j + 1;
                continue;
            }
            if self.id(i, "import") {
                while i < self.n() && !self.p(i, b';') {
                    i += 1;
                }
                i += 1;
                continue;
            }
            // modifiers / annotations
            let mut j = i;
            loop {
                if self.p(j, b'@') && !self.id(j + 1, "interface") {
                    j = self.skip_annotation(j);
                } else if self.id(j, "non") && self.p(j + 1, b'-') && self.id(j + 2, "sealed") {
                    j += 3;
                } else if self.is_id(j) && modifier_flag(self.txt(j)).is_some() {
                    j += 1;
                } else {
                    break;
                }
            }
            if self.id(j, "class") || self.id(j, "interface") || self.id(j, "enum") || (self.id(j, "record") && self.is_id(j + 1)) {
                i = self.type_decl(j, Ctx::Top);
            } else if self.p(j, b'@') && self.id(j + 1, "interface") {
                i = self.type_decl(j + 1, Ctx::Top);
            } else {
                i = j.max(i) + 1;
            }
        }
    }
}

/// Parse one `.java` file.
pub fn parse(src: &str) -> FileParse {
    let Lexed { toks, comments, lines } = lex(src);
    let mut p = P { src, t: toks, comments, lines, pkg: String::new(), out: FileParse::default(), taken: Default::default() };
    p.compilation_unit();
    p.out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = "package a.b;\n\npublic class Foo {\n    /** doc */\n    public static final int MAX = 1;\n    private int hidden;\n    int x, y;\n\n    public Foo(int a, final List<String> b) {\n    }\n\n    @Override\n    public <T> Map.Entry<K, V>[] get(String s, Object... rest) throws E {\n        return new Object() { public int anon() { return 1; } }.anon();\n    }\n    private static class Inner { public void m(); }\n    enum E { A, B(1) { public void f() {} }; public int g() { return 0; } }\n}\n";

    #[test]
    fn members_and_classes() {
        let f = parse(SRC);
        let names: Vec<&str> = f.classes.iter().map(|c| c.0.as_str()).collect();
        assert!(names.contains(&"a.b.Foo") && names.contains(&"a.b.Foo$Inner") && names.contains(&"a.b.Foo$E"), "{names:?}");
        let ms: Vec<(&str, Option<&str>, Option<&str>)> = f.members.iter().map(|m| (m.name.as_str(), m.ty.as_deref(), m.params.as_deref())).collect();
        assert!(ms.iter().any(|m| m.0 == "MAX" && m.1 == Some("int")));
        assert!(!ms.iter().any(|m| m.0 == "hidden"));
        assert!(ms.iter().any(|m| m.0 == "x" && m.1 == Some("int")));
        assert!(ms.iter().any(|m| m.0 == "Foo" && m.2 == Some("int a\u{1f}final List<String> b")));
        assert!(ms.iter().any(|m| m.0 == "get" && m.1 == Some("Map.Entry<K,V>[]") && m.2 == Some("String s\u{1f}Object... rest")));
        assert!(ms.iter().any(|m| m.0 == "anon") && ms.iter().any(|m| m.0 == "m") && ms.iter().any(|m| m.0 == "f") && ms.iter().any(|m| m.0 == "g"));
        assert!(ms.iter().any(|m| m.0 == "A" && m.1.is_none()));
        let max = f.members.iter().find(|m| m.name == "MAX").unwrap();
        assert_eq!((max.row, max.col, max.end_row, max.end_col), (5, 5, 5, 36));
        assert_eq!(&SRC[max.doc.0 as usize..max.doc.1 as usize], "/** doc */");
        let foo = f.classes.iter().find(|c| c.0 == "a.b.Foo").unwrap();
        let inner = f.classes.iter().find(|c| c.0 == "a.b.Foo$Inner").unwrap();
        assert!(foo.1 <= inner.1 && inner.2 <= foo.2 && inner.2 > inner.1);
    }
}
