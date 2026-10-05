//! Compact binary encoding of `FileAnalysis`. SymIds are process-local, so every name is written as an index
//! into a string table (`StrTab` on write, `Strs` on read; index 0 = absent, i+1 = string i).
//! Integers are LEB128 varints. A file blob = core fields in fixed order, then `n` extension sections
//! `(tag, len, bytes)` that older readers skip (room for new buckets without a format break).
use crate::analyzer::*;
use crate::cst::Pos;
use crate::intern::{intern, SymId};
use crate::io::cache::{Decode, Encode};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};

// ---------- primitives ----------

#[derive(Default)]
pub struct W {
    pub b: Vec<u8>,
}

impl W {
    #[inline]
    pub fn u(&mut self, mut v: u64) {
        while v >= 0x80 {
            self.b.push(v as u8 | 0x80);
            v >>= 7;
        }
        self.b.push(v as u8);
    }
    #[inline]
    pub fn byte(&mut self, v: u8) {
        self.b.push(v)
    }
    pub fn u32le(&mut self, v: u32) {
        self.b.extend_from_slice(&v.to_le_bytes())
    }
}

pub struct R<'a> {
    pub b: &'a [u8],
    pub p: usize,
}

impl<'a> R<'a> {
    pub fn new(b: &'a [u8]) -> R<'a> {
        R { b, p: 0 }
    }
    #[inline]
    pub fn u(&mut self) -> Option<u64> {
        let mut v = 0u64;
        let mut sh = 0;
        loop {
            let c = *self.b.get(self.p)?;
            self.p += 1;
            v |= ((c & 0x7f) as u64) << sh;
            if c < 0x80 {
                return Some(v);
            }
            sh += 7;
            if sh > 63 {
                return None;
            }
        }
    }
    #[inline]
    pub fn u32(&mut self) -> Option<u32> {
        self.u().map(|v| v as u32)
    }
    #[inline]
    pub fn byte(&mut self) -> Option<u8> {
        let c = *self.b.get(self.p)?;
        self.p += 1;
        Some(c)
    }
    pub fn bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        let s = self.b.get(self.p..self.p.checked_add(n)?)?;
        self.p += n;
        Some(s)
    }
}

// ---------- string tables ----------

/// Write side: SymId -> table index.
#[derive(Default)]
pub struct StrTab {
    ids: HashMap<u32, u32>,
    pub strs: Vec<&'static str>,
}

impl StrTab {
    /// 0 = absent (`SymId::NONE`), else index + 1.
    pub fn idx(&mut self, s: SymId) -> u64 {
        if s.is_none() {
            return 0;
        }
        self.idx_str(s.as_str(), Some(s.0))
    }
    /// Index of an arbitrary string (+1).
    pub fn str_idx(&mut self, s: &str) -> u64 {
        self.idx_str(intern(s).as_str(), None)
    }
    fn idx_str(&mut self, s: &'static str, id: Option<u32>) -> u64 {
        let id = id.unwrap_or_else(|| intern(s).0);
        let n = self.strs.len() as u32;
        let i = *self.ids.entry(id).or_insert_with(|| {
            self.strs.push(s);
            n
        });
        i as u64 + 1
    }
    /// `u32 n, (n+1) u32 offsets, bytes` (random access without scanning).
    pub fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&(self.strs.len() as u32).to_le_bytes());
        let mut off = 0u32;
        out.extend_from_slice(&off.to_le_bytes());
        for s in &self.strs {
            off += s.len() as u32;
            out.extend_from_slice(&off.to_le_bytes());
        }
        for s in &self.strs {
            out.extend_from_slice(s.as_bytes());
        }
    }
}

/// Read side: table index -> SymId, interned lazily and memoized.
pub struct Strs<'a> {
    n: usize,
    offs: &'a [u8],
    data: &'a [u8],
    memo: Vec<AtomicU32>,
}

impl<'a> Strs<'a> {
    pub fn parse(b: &'a [u8]) -> Option<Strs<'a>> {
        let n = u32::from_le_bytes(b.get(..4)?.try_into().ok()?) as usize;
        let offs = b.get(4..4 + (n + 1) * 4)?;
        let data = &b[4 + (n + 1) * 4..];
        let end = u32::from_le_bytes(offs[n * 4..].try_into().ok()?) as usize;
        if end > data.len() {
            return None;
        }
        Some(Strs { n, offs, data, memo: (0..n).map(|_| AtomicU32::new(u32::MAX)).collect() })
    }
    pub fn len(&self) -> usize {
        self.n
    }
    pub fn is_empty(&self) -> bool {
        self.n == 0
    }
    /// String `i` (0-based), not interned.
    pub fn str_at(&self, i: usize) -> Option<&'a str> {
        if i >= self.n {
            return None;
        }
        let a = u32::from_le_bytes(self.offs[i * 4..i * 4 + 4].try_into().unwrap()) as usize;
        let b = u32::from_le_bytes(self.offs[i * 4 + 4..i * 4 + 8].try_into().unwrap()) as usize;
        std::str::from_utf8(self.data.get(a..b)?).ok()
    }
    /// Table index as written by `StrTab::idx` (0 = NONE).
    pub fn sym(&self, idx: u64) -> Option<SymId> {
        if idx == 0 {
            return Some(SymId::NONE);
        }
        let i = (idx - 1) as usize;
        let m = self.memo.get(i)?;
        let v = m.load(Ordering::Relaxed);
        if v != u32::MAX {
            return Some(SymId(v));
        }
        let id = intern(self.str_at(i)?);
        m.store(id.0, Ordering::Relaxed);
        Some(id)
    }
}

// ---------- element encoders ----------

struct Enc<'a> {
    w: &'a mut W,
    t: &'a mut StrTab,
}

impl<'a> Enc<'a> {
    fn s(&mut self, v: SymId) {
        let i = self.t.idx(v);
        self.w.u(i)
    }
    fn val(&mut self, v: Val) {
        self.s(v.0)
    }
    fn pos(&mut self, p: Pos) {
        self.w.u(p.row as u64);
        self.w.u(p.col as u64);
        self.w.u(p.end_row.wrapping_sub(p.row) as u64);
        self.w.u(p.end_col as u64);
    }
    fn pair(&mut self, p: (SymId, SymId)) {
        self.s(p.0);
        self.s(p.1);
    }
    fn range(&mut self, p: (u32, u32)) {
        self.w.u(p.0 as u64);
        self.w.u(p.1 as u64);
    }
    fn vec<T>(&mut self, v: &[T], mut f: impl FnMut(&mut Self, &T)) {
        self.w.u(v.len() as u64);
        for x in v {
            f(self, x);
        }
    }
}

struct Dec<'a, 'b> {
    r: R<'a>,
    t: &'b Strs<'b>,
}

impl<'a, 'b> Dec<'a, 'b> {
    fn s(&mut self) -> Option<SymId> {
        let i = self.r.u()?;
        self.t.sym(i)
    }
    fn val(&mut self) -> Option<Val> {
        Some(Val(self.s()?))
    }
    fn pos(&mut self) -> Option<Pos> {
        let row = self.r.u32()?;
        let col = self.r.u32()?;
        let er = self.r.u32()?.wrapping_add(row);
        let ec = self.r.u32()?;
        Some(Pos { row, col, end_row: er, end_col: ec })
    }
    fn pair(&mut self) -> Option<(SymId, SymId)> {
        Some((self.s()?, self.s()?))
    }
    fn range(&mut self) -> Option<(u32, u32)> {
        Some((self.r.u32()?, self.r.u32()?))
    }
    fn vec<T>(&mut self, mut f: impl FnMut(&mut Self) -> Option<T>) -> Option<Vec<T>> {
        let n = self.r.u()? as usize;
        if n > self.r.b.len() {
            return None; // every element takes >= 1 byte
        }
        let mut v = Vec::with_capacity(n);
        for _ in 0..n {
            v.push(f(self)?);
        }
        Some(v)
    }
}

fn bits(v: &[bool]) -> u8 {
    v.iter().enumerate().fold(0, |a, (i, &b)| a | (b as u8) << i)
}

fn bit(b: u8, i: u32) -> bool {
    b >> i & 1 != 0
}

const EXT_NONE: u64 = 0;
const EXT_EXTRAS: u64 = 1;
const EXT_FINDINGS: u64 = 2;

/// Encode `fa` into `w`, names through `t`.
pub fn encode_fa(fa: &FileAnalysis, w: &mut W, t: &mut StrTab) {
    let mut e = Enc { w, t };
    let bl = match fa.base_lang {
        None => 0,
        Some(BaseLang::Clj) => 1,
        Some(BaseLang::Cljs) => 2,
        Some(BaseLang::Cljc) => 3,
    };
    e.w.byte(bl | (fa.has_callstack as u8) << 2);
    e.w.u(fa.next_local_id as u64);
    e.vec(&fa.namespace_definitions, |e, n| {
        e.pos(n.pos);
        e.pos(n.name_pos);
        e.s(n.name);
        e.s(n.doc);
        e.val(n.no_doc);
        e.val(n.deprecated);
        e.val(n.added);
        e.val(n.author);
        e.w.byte(n.in_ns as u8);
        e.w.byte(n.lang);
    });
    e.vec(&fa.namespace_usages, |e, n| {
        e.pos(n.name_pos);
        e.pos(n.alias_pos);
        e.s(n.from);
        e.s(n.to);
        e.s(n.alias);
        e.w.byte(n.lang);
    });
    e.vec(&fa.var_definitions, |e, d| {
        e.pos(d.pos);
        e.pos(d.name_pos);
        e.s(d.name);
        e.s(d.ns);
        e.pair(d.defined_by);
        e.pair(d.defined_by_lint_as);
        e.range(d.cs);
        e.s(d.doc);
        e.range(d.arglists);
        e.w.u(d.fixed.0);
        e.w.u(d.varargs_min as u64);
        e.val(d.deprecated);
        e.val(d.added);
        e.val(d.export);
        e.s(d.protocol_name);
        e.s(d.protocol_ns);
        e.val(d.meta);
        e.w.byte(d.lang);
        e.w.byte(bits(&[d.has_fixed, d.has_arglists, d.declared, d.private, d.macro_, d.test]));
    });
    e.vec(&fa.var_usages, |e, u| {
        e.pos(u.pos);
        e.pos(u.name_pos);
        e.s(u.name);
        e.s(u.from);
        e.s(u.from_var);
        e.w.u(u.arity as u64);
        e.s(u.alias);
        e.s(u.dispatch_val_str);
        e.s(u.ctx_testing);
        e.s(u.resolved_ns);
        e.w.byte(bits(&[u.refer, u.defmethod, u.derived, u.derived_name, u.unresolved, u.clojure_excluded, u.has_fixed, u.macro_]));
        e.w.byte(bits(&[u.private]));
        e.w.byte(u.lang);
        e.w.byte(u.call_lang);
        e.s(u.to);
        e.w.u(u.fixed.0);
        e.w.u(u.varargs_min as u64);
        e.val(u.deprecated);
    });
    e.vec(&fa.locals, |e, l| {
        e.w.u(l.id as u64);
        e.s(l.name);
        e.s(l.str_);
        e.pos(l.pos);
        e.w.u(l.scope_end_row as u64);
        e.w.u(l.scope_end_col as u64);
        e.w.byte(l.lang);
    });
    e.vec(&fa.local_usages, |e, l| {
        e.w.u(l.id as u64);
        e.s(l.name);
        e.pos(l.pos);
        e.pos(l.name_pos);
        e.w.byte(l.lang);
    });
    e.vec(&fa.callstacks, |e, c| e.pair(*c));
    e.vec(&fa.strs, |e, s| e.s(*s));
    e.vec(&fa.used_ns, |e, s| e.s(*s));
    e.vec(&fa.refer_alls, |e, (a, b, ex)| {
        e.s(*a);
        e.s(*b);
        e.vec(ex, |e, s| e.s(*s));
    });
    let mut x = W::default();
    let has_ext = !(fa.keywords.is_empty() && fa.symbols.is_empty() && fa.protocol_impls.is_empty() && fa.instance_invocations.is_empty() && fa.java_class_usages.is_empty() && fa.java_class_defs.is_empty());
    if has_ext {
        let mut e2 = Enc { w: &mut x, t: e.t };
        e2.vec(&fa.keywords, |e, k| {
            e.pos(k.pos);
            e.s(k.name);
            e.s(k.from);
            e.s(k.from_var);
            e.s(k.ns);
            e.s(k.alias);
            e.s(k.reg);
            e.w.byte(k.lang);
            e.w.byte(k.flags);
        });
        e2.vec(&fa.symbols, |e, k| {
            e.pos(k.pos);
            e.s(k.name);
            e.s(k.symbol);
            e.s(k.to);
            e.s(k.from);
            e.w.byte(k.lang);
        });
        e2.vec(&fa.protocol_impls, |e, p| {
            e.pos(p.pos);
            e.pos(p.name_pos);
            e.s(p.method_name);
            e.s(p.protocol_name);
            e.s(p.protocol_ns);
            e.s(p.impl_ns);
            e.pair(p.defined_by);
            e.pair(p.defined_by_lint_as);
            e.w.byte(p.derived as u8);
        });
        e2.vec(&fa.instance_invocations, |e, i| {
            e.pos(i.name_pos);
            e.s(i.method_name);
            e.w.byte(i.derived as u8);
            e.w.byte(i.lang);
        });
        e2.vec(&fa.java_class_usages, |e, j| {
            e.pos(j.pos);
            e.pos(j.name_pos);
            e.s(j.class);
            e.s(j.method);
            e.s(j.branch);
            e.s(j.tag);
            e.w.byte(j.call);
            e.w.byte(j.flags);
        });
        e2.vec(&fa.java_class_defs, |e, j| {
            e.s(j.class);
            e.w.u(j.flags as u64);
        });
    }
    // findings (project analysis only): level is baked in so the file is self-contained
    let mut xf = W::default();
    let has_f = !fa.findings.is_empty();
    if has_f {
        let mut e3 = Enc { w: &mut xf, t: e.t };
        e3.vec(&fa.findings, |e, f| {
            let level = if f.level != 0 { f.level } else { fa.lint_levels.get(f.ty as usize).copied().unwrap_or_else(|| f.ty.default_level()) };
            e.w.byte(f.ty as u8);
            e.w.byte(level);
            e.pos(f.pos);
            e.s(crate::intern::intern(&f.msg));
            e.w.byte(f.lang);
            e.w.byte(f.explicit_lang as u8 | (f.null_pos as u8) << 1);
            e.w.u(f.extra.len() as u64);
            for (k, v) in &f.extra {
                e.s(crate::intern::intern(k));
                e.s(crate::intern::intern(v));
            }
        });
    }
    let nblocks = has_ext as u64 + has_f as u64;
    if nblocks > 0 {
        e.w.u(nblocks);
        if has_ext {
            e.w.u(EXT_EXTRAS);
            e.w.u(x.b.len() as u64);
            e.w.b.extend_from_slice(&x.b);
        }
        if has_f {
            e.w.u(EXT_FINDINGS);
            e.w.u(xf.b.len() as u64);
            e.w.b.extend_from_slice(&xf.b);
        }
    } else {
        e.w.u(EXT_NONE);
    }
}

/// Decode a blob written by `encode_fa`.
pub fn decode_fa(b: &[u8], t: &Strs) -> Option<FileAnalysis> {
    let mut d = Dec { r: R::new(b), t };
    let mut fa = FileAnalysis::default();
    let f = d.r.byte()?;
    fa.base_lang = match f & 3 {
        0 => None,
        1 => Some(BaseLang::Clj),
        2 => Some(BaseLang::Cljs),
        _ => Some(BaseLang::Cljc),
    };
    fa.has_callstack = bit(f, 2);
    fa.next_local_id = d.r.u32()?;
    fa.namespace_definitions = d.vec(|d| {
        Some(NsDef { pos: d.pos()?, name_pos: d.pos()?, name: d.s()?, doc: d.s()?, no_doc: d.val()?, deprecated: d.val()?, added: d.val()?, author: d.val()?, in_ns: d.r.byte()? != 0, lang: d.r.byte()? })
    })?;
    fa.namespace_usages = d.vec(|d| Some(NsUsage { name_pos: d.pos()?, alias_pos: d.pos()?, from: d.s()?, to: d.s()?, alias: d.s()?, lang: d.r.byte()? }))?;
    fa.var_definitions = d.vec(|d| {
        let pos = d.pos()?;
        let name_pos = d.pos()?;
        let name = d.s()?;
        let ns = d.s()?;
        let defined_by = d.pair()?;
        let defined_by_lint_as = d.pair()?;
        let cs = d.range()?;
        let doc = d.s()?;
        let arglists = d.range()?;
        let fixed = Arities(d.r.u()?);
        let varargs_min = d.r.u()? as u16;
        let deprecated = d.val()?;
        let added = d.val()?;
        let export = d.val()?;
        let protocol_name = d.s()?;
        let protocol_ns = d.s()?;
        let meta = d.val()?;
        let lang = d.r.byte()?;
        let b = d.r.byte()?;
        Some(VarDef { pos, name_pos, name, ns, defined_by, defined_by_lint_as, cs, doc, arglists, fixed, has_fixed: bit(b, 0), has_arglists: bit(b, 1), declared: bit(b, 2), varargs_min, private: bit(b, 3), macro_: bit(b, 4), test: bit(b, 5), deprecated, added, export, protocol_name, protocol_ns, meta, imported: (SymId::NONE, SymId::NONE), lang })
    })?;
    fa.var_usages = d.vec(|d| {
        let pos = d.pos()?;
        let name_pos = d.pos()?;
        let name = d.s()?;
        let from = d.s()?;
        let from_var = d.s()?;
        let arity = d.r.u()? as u16;
        let alias = d.s()?;
        let dispatch_val_str = d.s()?;
        let ctx_testing = d.s()?;
        let resolved_ns = d.s()?;
        let b = d.r.byte()?;
        let b2 = d.r.byte()?;
        let lang = d.r.byte()?;
        let call_lang = d.r.byte()?;
        let to = d.s()?;
        let fixed = Arities(d.r.u()?);
        let varargs_min = d.r.u()? as u16;
        let deprecated = d.val()?;
        Some(VarUsage { pos, name_pos, name, from, from_var, arity, alias, refer: bit(b, 0), defmethod: bit(b, 1), derived: bit(b, 2), derived_name: bit(b, 3), dispatch_val_str, ctx_testing, resolved_ns, unresolved: bit(b, 4), clojure_excluded: bit(b, 5), lang, to, fixed, has_fixed: bit(b, 6), varargs_min, macro_: bit(b, 7), private: bit(b2, 0), deprecated, call_lang, synth: false })
    })?;
    fa.locals = d.vec(|d| Some(Local { id: d.r.u32()?, name: d.s()?, str_: d.s()?, pos: d.pos()?, scope_end_row: d.r.u32()?, scope_end_col: d.r.u32()?, lang: d.r.byte()? }))?;
    fa.local_usages = d.vec(|d| Some(LocalUsage { id: d.r.u32()?, name: d.s()?, pos: d.pos()?, name_pos: d.pos()?, lang: d.r.byte()? }))?;
    fa.callstacks = d.vec(|d| d.pair())?;
    fa.strs = d.vec(|d| d.s())?;
    fa.used_ns = d.vec(|d| d.s())?;
    fa.refer_alls = d.vec(|d| Some((d.s()?, d.s()?, d.vec(|d| d.s())?)))?;
    let n = d.r.u()?;
    for _ in 0..n {
        let tag = d.r.u()?;
        let len = d.r.u()? as usize;
        let body = d.r.bytes(len)?;
        if tag == EXT_EXTRAS {
            let mut x = Dec { r: R::new(body), t };
            fa.keywords = x.vec(|d| Some(Keyword { pos: d.pos()?, name: d.s()?, from: d.s()?, from_var: d.s()?, ns: d.s()?, alias: d.s()?, reg: d.s()?, lang: d.r.byte()?, flags: d.r.byte()? }))?;
            fa.symbols = x.vec(|d| Some(SymbolUse { pos: d.pos()?, name: d.s()?, symbol: d.s()?, to: d.s()?, from: d.s()?, lang: d.r.byte()? }))?;
            fa.protocol_impls = x.vec(|d| {
                Some(ProtocolImpl { pos: d.pos()?, name_pos: d.pos()?, method_name: d.s()?, protocol_name: d.s()?, protocol_ns: d.s()?, impl_ns: d.s()?, defined_by: d.pair()?, defined_by_lint_as: d.pair()?, derived: d.r.byte()? != 0 })
            })?;
            fa.instance_invocations = x.vec(|d| Some(InstanceInvocation { name_pos: d.pos()?, method_name: d.s()?, derived: d.r.byte()? != 0, lang: d.r.byte()? }))?;
            fa.java_class_usages = x.vec(|d| Some(JavaUsage { pos: d.pos()?, name_pos: d.pos()?, class: d.s()?, method: d.s()?, branch: d.s()?, tag: d.s()?, call: d.r.byte()?, flags: d.r.byte()? }))?;
            fa.java_class_defs = x.vec(|d| Some(JavaClassDef { class: d.s()?, flags: d.r.u()? as u16 }))?;
        }
        if tag == EXT_FINDINGS {
            let mut x = Dec { r: R::new(body), t };
            fa.findings = x.vec(|d| {
                let ty = crate::analyzer::lint::FTYPES.get(d.r.byte()? as usize)?.0;
                let level = d.r.byte()?;
                let pos = d.pos()?;
                let msg = d.s()?.as_str().to_owned();
                let lang = d.r.byte()?;
                let fl = d.r.byte()?;
                let n = d.r.u()? as usize;
                let mut extra = Vec::with_capacity(n);
                for _ in 0..n {
                    let k = d.s()?.as_str();
                    let v = d.s()?.as_str().to_owned();
                    extra.push((crate::analyzer::lint::static_key(k), v));
                }
                Some(crate::analyzer::lint::Finding { ty, pos, msg, lang, explicit_lang: fl & 1 != 0, level, null_pos: fl & 2 != 0, extra })
            })?;
        }
    }
    Some(fa)
}

/// Standalone value encoding (own string table): `u32 table_len, table, body`.
impl Encode for FileAnalysis {
    fn encode(&self, out: &mut Vec<u8>) {
        let (mut w, mut t) = (W::default(), StrTab::default());
        encode_fa(self, &mut w, &mut t);
        let mut tb = Vec::new();
        t.write(&mut tb);
        out.extend_from_slice(&(tb.len() as u32).to_le_bytes());
        out.extend_from_slice(&tb);
        out.extend_from_slice(&w.b);
    }
}

impl Decode for FileAnalysis {
    fn decode(b: &[u8]) -> Option<Self> {
        let tl = u32::from_le_bytes(b.get(..4)?.try_into().ok()?) as usize;
        let strs = Strs::parse(b.get(4..4 + tl)?)?;
        decode_fa(&b[4 + tl..], &strs)
    }
}
