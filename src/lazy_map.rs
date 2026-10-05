//! `Value::LazyMap`: a read-only top-level EDN map that stays as its source
//! text plus a sorted key index. Values are parsed from their byte span on
//! each `get`. Map-wide ops go through ONE materialize choke point,
//! [`as_pmap`] (cached in `OnceLock`), like `host_struct`.
//!
//! [`build`] does one byte pass over the top-level map only. It returns
//! `None` ("bail") on anything outside a simple subset (non-keyword keys,
//! non-ASCII outside strings, char literals, tagged/meta/quote forms,
//! duplicate keys, odd count); the caller then reads eagerly.

use crate::keyword::Keyword;
use crate::reader::DELIM_TABLE;
use crate::value::{PMap, Str, Value};
use std::sync::OnceLock;

#[derive(Clone, Copy)]
struct Entry {
    ks: u32,
    ke: u32,
    vs: u32,
    ve: u32,
}

pub struct LazyMapInner {
    src: Str,
    /// Entries in source order (used by `as_pmap`).
    entries: Vec<Entry>,
    /// (key hash, index into `entries`), sorted.
    index: Vec<(u64, u32)>,
    snap: OnceLock<PMap>,
}

#[inline]
fn fnv(b: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &c in b {
        h = (h ^ c as u64).wrapping_mul(0x100000001b3);
    }
    h
}

fn skip_trivia(b: &[u8], mut p: usize) -> usize {
    loop {
        match b.get(p) {
            Some(&c) if matches!(c, 0x09..=0x0D | 0x20 | b',') => p += 1,
            Some(&b';') => {
                while p < b.len() && b[p] != b'\n' {
                    p += 1;
                }
            }
            _ => return p,
        }
    }
}

/// Skips one string starting at the opening quote; returns index after the closing quote.
fn skip_string(b: &[u8], mut p: usize) -> Option<usize> {
    p += 1;
    while p < b.len() {
        match b[p] {
            b'"' => return Some(p + 1),
            b'\\' => p += 2,
            _ => p += 1,
        }
    }
    None
}

fn at_token_start(b: &[u8], p: usize) -> bool {
    p == 0 || b[p - 1] >= 0x80 || DELIM_TABLE[b[p - 1] as usize]
}

/// Skips one value starting at `p` (trivia already skipped); returns its end.
fn skip_value(b: &[u8], mut p: usize) -> Option<usize> {
    let c = *b.get(p)?;
    match c {
        b'"' => skip_string(b, p),
        b'(' | b'[' | b'{' | b'#' => {
            let mut depth = 0usize;
            if c == b'#' {
                if b.get(p + 1) != Some(&b'{') {
                    return None;
                }
                p += 1;
            }
            loop {
                match *b.get(p)? {
                    b'(' | b'[' | b'{' => {
                        depth += 1;
                        p += 1;
                    }
                    b')' | b']' | b'}' => {
                        depth -= 1;
                        p += 1;
                        if depth == 0 {
                            return Some(p);
                        }
                    }
                    b'"' => p = skip_string(b, p)?,
                    b';' => p = skip_trivia(b, p),
                    // `#`, quote-ish macros: only special at a token start (`u#`, `kw'` are plain tokens)
                    b'#' if at_token_start(b, p) => {
                        if b.get(p + 1) != Some(&b'{') {
                            return None;
                        }
                        p += 1;
                    }
                    b'\'' | b'`' | b'~' | b'@' | b'^' if at_token_start(b, p) => return None,
                    b'\\' => return None,
                    x if x >= 0x80 => return None,
                    _ => p += 1,
                }
            }
        }
        b')' | b']' | b'}' | b'\\' | b'\'' | b'`' | b'~' | b'@' | b'^' | b';' => None,
        x if x >= 0x80 => None,
        _ => {
            // atom or keyword token
            let start = p;
            while let Some(&x) = b.get(p) {
                if x >= 0x80 {
                    return None;
                }
                if DELIM_TABLE[x as usize] {
                    break;
                }
                p += 1;
            }
            if p == start {
                None
            } else {
                Some(p)
            }
        }
    }
}

/// Builds the lazy index for a top-level `{...}` in `s`, or `None` to bail.
pub fn build(s: &Str) -> Option<LazyMapInner> {
    let src: &str = s;
    let b = src.as_bytes();
    if b.len() >= u32::MAX as usize {
        return None;
    }
    let mut p = skip_trivia(b, 0);
    if b.get(p) != Some(&b'{') {
        return None;
    }
    p += 1;
    let mut entries: Vec<Entry> = Vec::with_capacity(b.len() / 200 + 8);
    loop {
        p = skip_trivia(b, p);
        match b.get(p)? {
            b'}' => break,
            b':' => {}
            _ => return None,
        }
        // key: `:` token, ASCII, no `::`
        let ks = p + 1;
        if b.get(ks) == Some(&b':') {
            return None;
        }
        let ke = skip_value(b, p)?;
        if ke <= ks {
            return None;
        }
        p = skip_trivia(b, ke);
        let vs = p;
        let ve = skip_value(b, vs)?;
        entries.push(Entry { ks: ks as u32, ke: ke as u32, vs: vs as u32, ve: ve as u32 });
        p = ve;
    }
    let mut index: Vec<(u64, u32)> =
        entries.iter().enumerate().map(|(i, e)| (fnv(&b[e.ks as usize..e.ke as usize]), i as u32)).collect();
    index.sort_unstable();
    // duplicate keys -> bail (general reader raises the canonical error)
    for w in index.windows(2) {
        if w[0].0 == w[1].0 {
            let (a, c) = (entries[w[0].1 as usize], entries[w[1].1 as usize]);
            if b[a.ks as usize..a.ke as usize] == b[c.ks as usize..c.ke as usize] {
                return None;
            }
        }
    }
    Some(LazyMapInner { src: s.clone(), entries, index, snap: OnceLock::new() })
}

fn parse_span(src: &str, s: u32, e: u32) -> Value {
    let text = &src[s as usize..e as usize];
    if let Some(v) = crate::edn_fast::try_read_edn(text) {
        return v;
    }
    match crate::reader::read_one_with_ns(text, Default::default()) {
        Ok(Some(f)) => crate::reader::form_to_value(&f),
        _ => Value::Nil,
    }
}

fn key_value(inner: &LazyMapInner, e: Entry) -> Value {
    Value::Keyword(Keyword::from(&inner.src[e.ks as usize..e.ke as usize]))
}

pub fn count(inner: &LazyMapInner) -> usize {
    inner.entries.len()
}

fn find(inner: &LazyMapInner, key: &str) -> Option<Entry> {
    let h = fnv(key.as_bytes());
    let mut i = inner.index.partition_point(|x| x.0 < h);
    let b = inner.src.as_bytes();
    while let Some(&(hh, idx)) = inner.index.get(i) {
        if hh != h {
            return None;
        }
        let e = inner.entries[idx as usize];
        if &b[e.ks as usize..e.ke as usize] == key.as_bytes() {
            return Some(e);
        }
        i += 1;
    }
    None
}

/// Keyword lookup by its flat `ns/name` text; parses the value span.
pub fn lookup(inner: &LazyMapInner, kw: &Str) -> Option<Value> {
    let e = find(inner, kw)?;
    Some(parse_span(&inner.src, e.vs, e.ve))
}

pub fn contains_key(inner: &LazyMapInner, kw: &Str) -> bool {
    find(inner, kw).is_some()
}

/// Generic key lookup: only keyword keys can be present.
pub fn get(inner: &LazyMapInner, k: &Value) -> Option<Value> {
    match k {
        Value::Keyword(kw) => lookup(inner, kw.text_ref()),
        _ => None,
    }
}

/// THE materialize choke point: builds (once) a real `PMap`.
pub fn as_pmap(inner: &LazyMapInner) -> &PMap {
    inner.snap.get_or_init(|| {
        let pairs: Vec<(Value, Value)> =
            inner.entries.iter().map(|e| (key_value(inner, *e), parse_span(&inner.src, e.vs, e.ve))).collect();
        PMap::from_unique_pairs(pairs)
    })
}
