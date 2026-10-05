//! M6: `PVec::Col` -- a columnar packed vector of same-shape keyword-keyed maps.
//!
//! Built only by `(mova.mem/pack v)`. Storage: per-vector shape table (M3 `MapShape`
//! or a small-map key list) + one typed column per distinct key (ints as offsets from
//! a base, everything else as codes into a per-column deduped table), 1/2/4-byte codes.
//! Owned reads (`elem`, `into_iter`, `skip`/`take`/`slice`, `len`) build elements on
//! the fly and cache nothing. Reference-returning `PVec` ops go through ONE choke
//! point, [`ColVec::mat`] (cached `OnceLock`), like `host_struct::as_pmap`.
//! Mutating ops first swap `self` for that materialized `PVec`.

use crate::shaped_map::{MapShape, ShapedMap};
use crate::value::{MetaObj, PMap, PVec, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

pub static PACKS: AtomicU64 = AtomicU64::new(0);
pub static MATS: AtomicU64 = AtomicU64::new(0);

enum Codes {
    U8(Box<[u8]>),
    U16(Box<[u16]>),
    U32(Box<[u32]>),
}

impl Codes {
    fn build(raw: Vec<u32>, max: u32) -> Codes {
        if max < 256 {
            Codes::U8(raw.into_iter().map(|x| x as u8).collect())
        } else if max < 65536 {
            Codes::U16(raw.into_iter().map(|x| x as u16).collect())
        } else {
            Codes::U32(raw.into_boxed_slice())
        }
    }
    #[inline]
    fn get(&self, i: usize) -> u32 {
        match self {
            Codes::U8(a) => a[i] as u32,
            Codes::U16(a) => a[i] as u32,
            Codes::U32(a) => a[i],
        }
    }
    fn bytes(&self) -> usize {
        match self {
            Codes::U8(a) => a.len(),
            Codes::U16(a) => a.len() * 2,
            Codes::U32(a) => a.len() * 4,
        }
    }
}

enum Col {
    /// Every present value is `Value::Int(base + code)`.
    Int(i64, Codes),
    /// Codes index a per-column table (scalars deduped).
    Tab(Box<[Value]>, Codes),
    /// Every present value carries keyword-keyed map metadata: inner column + packed meta maps.
    Meta(Box<Col>, Arc<ColData>),
}

impl Codes {
    fn deep(&self) -> Codes {
        match self {
            Codes::U8(b) => Codes::U8(b.clone()),
            Codes::U16(b) => Codes::U16(b.clone()),
            Codes::U32(b) => Codes::U32(b.clone()),
        }
    }
}

impl Col {
    /// M8: copy onto fresh pages; table values go through `f`.
    fn deep(&self, f: &mut dyn FnMut(&Value) -> Value) -> Col {
        match self {
            Col::Int(b, c) => Col::Int(*b, c.deep()),
            Col::Tab(t, c) => Col::Tab(t.iter().map(|x| f(x)).collect(), c.deep()),
            Col::Meta(i, d) => Col::Meta(Box::new(i.deep(f)), Arc::new(d.deep(f))),
        }
    }

    #[inline]
    fn get(&self, i: usize) -> Value {
        match self {
            Col::Int(base, c) => Value::Int(base + c.get(i) as i64),
            Col::Tab(t, c) => t[c.get(i) as usize].clone(),
            Col::Meta(inner, m) => Value::Meta(Arc::new(MetaObj { meta: m.elem(i), inner: inner.get(i) })),
        }
    }
}

impl Col {
    fn bytes_of(c: &Col) -> usize {
        match c {
            Col::Int(_, c) => c.bytes(),
            Col::Tab(t, c) => c.bytes() + t.len() * 32,
            Col::Meta(inner, m) => Col::bytes_of(inner) + m.bytes(),
        }
    }
}

enum Kind {
    Shaped(Arc<MapShape>),
    /// Small map keys in insertion order.
    Small(Box<[Value]>),
}

struct ElemShape {
    kind: Kind,
    /// Column index of each key, in the shape's key order.
    cols: Box<[u16]>,
}

pub struct ColData {
    shapes: Box<[ElemShape]>,
    /// Shape index per element; empty when there is one shape.
    shape_of: Box<[u8]>,
    cols: Box<[Col]>,
}

impl ColData {
    /// M8: deep copy (shapes' interned `MapShape`s stay shared).
    fn deep(&self, f: &mut dyn FnMut(&Value) -> Value) -> ColData {
        ColData {
            shapes: self
                .shapes
                .iter()
                .map(|s| ElemShape {
                    kind: match &s.kind {
                        Kind::Shaped(sh) => Kind::Shaped(sh.clone()),
                        Kind::Small(k) => Kind::Small(k.iter().map(|x| f(x)).collect()),
                    },
                    cols: s.cols.clone(),
                })
                .collect(),
            shape_of: self.shape_of.clone(),
            cols: self.cols.iter().map(|c| c.deep(f)).collect(),
        }
    }

    #[inline]
    fn elem(&self, i: usize) -> Value {
        let s = &self.shapes[if self.shape_of.is_empty() { 0 } else { self.shape_of[i] as usize }];
        let vals = s.cols.iter().map(|&c| self.cols[c as usize].get(i));
        match &s.kind {
            Kind::Shaped(sh) => Value::Map(PMap::Shaped(ShapedMap::new(sh.clone(), vals))),
            Kind::Small(keys) => Value::Map(PMap::Small(Arc::new(keys.iter().cloned().zip(vals).collect()))),
        }
    }

    /// Approximate heap bytes (columns + tables' slots; excludes shared payloads).
    pub fn bytes(&self) -> usize {
        let cols: usize = self
            .cols
            .iter()
            .map(|c| match c {
                Col::Int(_, c) => c.bytes(),
                Col::Tab(t, c) => c.bytes() + t.len() * 32,
                Col::Meta(inner, m) => Col::bytes_of(inner) + m.bytes(),
            })
            .sum();
        cols + self.shape_of.len() + self.shapes.iter().map(|s| s.cols.len() * 2 + 48).sum::<usize>()
    }
}

pub struct ColVec {
    data: Arc<ColData>,
    lo: usize,
    len: usize,
    mat: OnceLock<PVec>,
}

impl ColVec {
    /// M8: deep copy of the whole column data; the materialize cache is dropped.
    pub fn deep_copy(&self, f: &mut dyn FnMut(&Value) -> Value) -> ColVec {
        ColVec { data: Arc::new(self.data.deep(f)), lo: self.lo, len: self.len, mat: OnceLock::new() }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Owned element `i` (built on the fly, or cloned from the cache when present).
    #[inline]
    pub fn elem(&self, i: usize) -> Option<Value> {
        if i >= self.len {
            return None;
        }
        if let Some(m) = self.mat.get() {
            return m.get(i).cloned();
        }
        Some(self.data.elem(self.lo + i))
    }

    /// THE materialize choke point: a plain `Small`/`Big` `PVec`, cached.
    pub fn mat(&self) -> &PVec {
        self.mat.get_or_init(|| {
            MATS.fetch_add(1, Ordering::Relaxed);
            if std::env::var_os("MOVA_COLVEC_TRACE").is_some() {
                let mut clj = [0u8; 160];
                crate::profile::clj_frames(&mut clj);
                eprintln!("colvec materialize {} len {} [{}]", std::thread::current().name().unwrap_or("?"), self.len, String::from_utf8_lossy(&clj).trim_end_matches('\0'));
            }
            (0..self.len).map(|i| self.data.elem(self.lo + i)).collect()
        })
    }

    pub fn is_mat(&self) -> bool {
        self.mat.get().is_some()
    }

    /// Sub-view `[a, b)` sharing the columns (bounds clamped).
    pub fn view(&self, a: usize, b: usize) -> PVec {
        let b = b.min(self.len);
        let a = a.min(b);
        if let Some(m) = self.mat.get() {
            return m.slice(a..b);
        }
        if b - a <= crate::value::PVEC_SMALL_MAX {
            return (a..b).map(|i| self.data.elem(self.lo + i)).collect();
        }
        PVec::Col(Arc::new(ColVec { data: self.data.clone(), lo: self.lo + a, len: b - a, mat: OnceLock::new() }))
    }

    /// Debug: per column [kind table-len code-bytes]; column order = first-seen key order.
    pub fn layout(&self) -> Vec<(bool, usize, usize)> {
        self.data
            .cols
            .iter()
            .map(|c| match c {
                Col::Int(_, c) => (true, 0, c.bytes()),
                Col::Tab(t, c) => (false, t.len(), c.bytes()),
                Col::Meta(..) => (false, 0, Col::bytes_of(c)),
            })
            .collect()
    }

    /// Debug: keys by column index.
    pub fn col_keys(&self) -> Vec<Value> {
        let mut ks = vec![Value::Nil; self.data.cols.len()];
        for s in self.data.shapes.iter() {
            let keys: Vec<Value> = match &s.kind {
                Kind::Shaped(sh) => sh.keys.to_vec(),
                Kind::Small(k) => k.to_vec(),
            };
            for (k, &c) in keys.into_iter().zip(s.cols.iter()) {
                ks[c as usize] = k;
            }
        }
        ks
    }

    pub fn data_bytes(&self) -> usize {
        self.data.bytes()
    }
}

fn kill() -> bool {
    static K: OnceLock<bool> = OnceLock::new();
    *K.get_or_init(|| std::env::var("MOVA_COLVEC").map(|v| v == "0").unwrap_or(false))
}

/// Tag for dedup keys: only variants whose `=` implies identical representation.
fn dedup_tag(v: &Value) -> Option<u8> {
    Some(match v {
        Value::Nil => 0,
        Value::Bool(_) => 1,
        Value::Int(_) => 2,
        Value::Str(_) => 3,
        Value::Keyword(_) => 4,
        Value::Sym(_) => 5,
        Value::Char(_) => 6,
        Value::Map(m) if m.is_empty() => 7,
        Value::Set(s) if s.iter().all(|x| matches!(x, Value::Int(_))) => 8,
        Value::Vector(v) if v.len() <= 8 && !v.is_col() && v.iter().all(|x| matches!(dedup_tag(x), Some(0..=6))) => 9,
        _ => return None,
    })
}

fn encode(vals: Vec<Value>, present: &[bool]) -> Col {
    if let Some(c) = encode_meta(&vals, present) {
        return c;
    }
    let mut lo = i64::MAX;
    let mut hi = i64::MIN;
    let mut all_int = true;
    for (v, &p) in vals.iter().zip(present) {
        if !p {
            continue;
        }
        match v {
            Value::Int(x) => {
                lo = lo.min(*x);
                hi = hi.max(*x);
            }
            _ => {
                all_int = false;
                break;
            }
        }
    }
    if all_int && lo <= hi && (hi as i128 - lo as i128) < u32::MAX as i128 {
        let raw: Vec<u32> = vals
            .iter()
            .zip(present)
            .map(|(v, &p)| match (v, p) {
                (Value::Int(x), true) => (x - lo) as u32,
                _ => 0,
            })
            .collect();
        return Col::Int(lo, Codes::build(raw, (hi - lo) as u32));
    }
    let mut table: Vec<Value> = Vec::new();
    let mut seen: HashMap<(u8, Value), u32> = HashMap::new();
    let mut raw = Vec::with_capacity(vals.len());
    for (v, &p) in vals.into_iter().zip(present) {
        if !p {
            raw.push(0);
            continue;
        }
        let code = match dedup_tag(&v) {
            Some(t) => *seen.entry((t, v.clone())).or_insert_with(|| {
                table.push(v);
                (table.len() - 1) as u32
            }),
            None => {
                table.push(v);
                (table.len() - 1) as u32
            }
        };
        raw.push(code);
    }
    if table.is_empty() {
        table.push(Value::Nil);
    }
    let max = (table.len() - 1) as u32;
    Col::Tab(table.into_boxed_slice(), Codes::build(raw, max))
}

/// All present values are `Meta` over a keyword-keyed map: pack inner and meta separately.
fn encode_meta(vals: &[Value], present: &[bool]) -> Option<Col> {
    let mut filler = None;
    for (v, &p) in vals.iter().zip(present) {
        if !p {
            continue;
        }
        match v {
            Value::Meta(m) if matches!(m.meta, Value::Map(_)) => {
                filler.get_or_insert_with(|| m.meta.clone());
            }
            _ => return None,
        }
    }
    let filler = filler?;
    let mut inner = Vec::with_capacity(vals.len());
    let mut metas = Vec::with_capacity(vals.len());
    for (v, &p) in vals.iter().zip(present) {
        match (v, p) {
            (Value::Meta(m), true) => {
                inner.push(m.inner.clone());
                metas.push(m.meta.clone());
            }
            _ => {
                inner.push(Value::Nil);
                metas.push(filler.clone());
            }
        }
    }
    let PVec::Col(cv) = pack_inner(&metas.into_iter().collect(), 1)? else { return None };
    Some(Col::Meta(Box::new(encode(inner, present)), cv.data.clone()))
}

/// `(mova.mem/pack v)`: `Some(PVec::Col)` when `v` has >= `min` elements, all plain
/// keyword-keyed maps (Shaped or Small), <= 256 distinct shapes; else `None`.
pub fn pack(v: &PVec, min: usize) -> Option<PVec> {
    if kill() {
        return None;
    }
    let r = pack_inner(v, min)?;
    PACKS.fetch_add(1, Ordering::Relaxed);
    Some(r)
}

fn pack_inner(v: &PVec, min: usize) -> Option<PVec> {
    if v.len() < min.max(1) || matches!(v, PVec::Col(_)) {
        return None;
    }
    let n = v.len();
    let mut shapes: Vec<ElemShape> = Vec::new();
    let mut shape_ix: HashMap<(usize, Vec<Value>), u8> = HashMap::new();
    let mut shape_of: Vec<u8> = Vec::with_capacity(n);
    let mut col_ix: HashMap<Value, u16> = HashMap::new();
    let mut cols: Vec<(Vec<Value>, Vec<bool>)> = Vec::new();
    for (i, e) in v.iter().enumerate() {
        let Value::Map(m) = e else { return None };
        let (key, keys): ((usize, Vec<Value>), Vec<Value>) = match m {
            PMap::Shaped(s) => ((Arc::as_ptr(s.shape()) as usize, Vec::new()), s.shape().keys.to_vec()),
            PMap::Small(a) => {
                let ks: Vec<Value> = a.iter().map(|(k, _)| k.clone()).collect();
                ((0, ks.clone()), ks)
            }
            PMap::Big(_) => return None,
        };
        if !keys.iter().all(|k| matches!(k, Value::Keyword(_))) {
            return None;
        }
        let si = match shape_ix.get(&key) {
            Some(&si) => si,
            None => {
                if shapes.len() >= 256 {
                    return None;
                }
                let mut cix = Vec::with_capacity(keys.len());
                for k in &keys {
                    let next = col_ix.len();
                    if next >= u16::MAX as usize {
                        return None;
                    }
                    let c = *col_ix.entry(k.clone()).or_insert_with(|| {
                        cols.push((vec![Value::Nil; n], vec![false; n]));
                        next as u16
                    });
                    cix.push(c);
                }
                let kind = match m {
                    PMap::Shaped(s) => Kind::Shaped(s.shape().clone()),
                    _ => Kind::Small(keys.clone().into_boxed_slice()),
                };
                shapes.push(ElemShape { kind, cols: cix.into_boxed_slice() });
                let si = (shapes.len() - 1) as u8;
                shape_ix.insert(key, si);
                si
            }
        };
        shape_of.push(si);
        let s = &shapes[si as usize];
        let vals: Vec<Value> = match m {
            PMap::Shaped(sm) => sm.vals().to_vec(),
            PMap::Small(a) => a.iter().map(|(_, v)| v.clone()).collect(),
            PMap::Big(_) => unreachable!(),
        };
        for (j, val) in vals.into_iter().enumerate() {
            let c = &mut cols[s.cols[j] as usize];
            c.0[i] = val;
            c.1[i] = true;
        }
    }
    let cols: Vec<Col> = cols.into_iter().map(|(vals, present)| encode(vals, &present)).collect();
    if shapes.len() == 1 {
        shape_of = Vec::new();
    }
    let data = ColData { shapes: shapes.into_boxed_slice(), shape_of: shape_of.into_boxed_slice(), cols: cols.into_boxed_slice() };
    Some(PVec::Col(Arc::new(ColVec { data: Arc::new(data), lo: 0, len: n, mat: OnceLock::new() })))
}
