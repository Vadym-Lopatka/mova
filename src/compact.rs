//! M8: `(mova.mem/compact v)` -- deep copy of a long-lived value onto fresh,
//! densely packed allocator pages. After a big transient phase (clojure-lsp's
//! background analysis), survivors are scattered one-or-two per page across
//! pages the phase's worker threads filled; copying them and dropping the old
//! graph empties those pages so the allocator can return them to the OS.
//! Sharing is kept: every Arc-backed node is copied once (memo by address).
//! Only plain data is copied (strings, symbols, vectors/lists, maps, sets,
//! metadata, packed columnar vectors); everything else is shared as is.
use std::collections::HashMap;
use std::sync::Arc;

use crate::shaped_map::ShapedMap;
use crate::value::{MetaObj, PMap, PVec, Str, Symbol, Value};

/// Deep copy with an address memo so shared nodes stay shared.
pub struct Compactor {
    memo: HashMap<usize, Value>,
    keep: Vec<Value>,
}

impl Default for Compactor {
    fn default() -> Self {
        Self::new()
    }
}

impl Compactor {
    pub fn new() -> Self {
        Compactor { memo: HashMap::new(), keep: Vec::new() }
    }

    fn memo(&mut self, addr: usize, v: &Value, f: impl FnOnce(&mut Self) -> Value) -> Value {
        if let Some(c) = self.memo.get(&addr) {
            return c.clone();
        }
        let c = f(self);
        // keep the original alive while memoized so its address can't be reused
        self.memo.insert(addr, c.clone());
        self.keep.push(v.clone());
        c
    }

    pub fn str(&mut self, s: &Str) -> Str {
        match self.memo_str(s) {
            Value::Str(c) => c,
            _ => s.clone(),
        }
    }

    fn memo_str(&mut self, s: &Str) -> Value {
        let v = Value::Str(s.clone());
        self.memo(s.ptr_addr(), &v, |_| Value::Str(Str::from(&*s.as_contiguous())))
    }

    fn sym(&mut self, s: &Symbol) -> Symbol {
        Symbol { ns: s.ns.as_ref().map(|n| self.str(n)), name: self.str(&s.name) }
    }

    pub fn copy(&mut self, v: &Value) -> Value {
        match v {
            Value::Str(s) => self.memo_str(s),
            Value::Uri(s) => Value::Uri(self.str(s)),
            Value::Sym(s) => Value::Sym(self.sym(s)),
            Value::Vector(p) => self.pvec(p, v, Value::Vector),
            Value::List(p) => self.pvec(p, v, Value::List),
            Value::Map(m) => self.pmap(m, v),
            Value::Set(s) => Value::Set(s.iter().map(|x| self.copy(x)).collect()),
            Value::Meta(m) => {
                let addr = Arc::as_ptr(m) as usize;
                self.memo(addr, v, |c| Value::Meta(Arc::new(MetaObj { meta: c.copy(&m.meta), inner: c.copy(&m.inner) })))
            }
            _ => v.clone(),
        }
    }

    fn pvec(&mut self, p: &PVec, v: &Value, wrap: fn(PVec) -> Value) -> Value {
        match p {
            PVec::Small(a) => {
                let addr = Arc::as_ptr(a) as *const Value as usize;
                self.memo(addr, v, |c| wrap(PVec::Small(a.iter().map(|x| c.copy(x)).collect())))
            }
            PVec::Big(b) => {
                let addr = Arc::as_ptr(b) as usize;
                self.memo(addr, v, |c| wrap(PVec::Big(Arc::new(b.iter().map(|x| c.copy(x)).collect()))))
            }
            PVec::Col(cv) => {
                let addr = Arc::as_ptr(cv) as usize;
                self.memo(addr, v, |c| wrap(PVec::Col(Arc::new(cv.deep_copy(&mut |x| c.copy(x))))))
            }
        }
    }

    fn pmap(&mut self, m: &PMap, v: &Value) -> Value {
        match m {
            PMap::Small(a) => {
                let addr = Arc::as_ptr(a) as usize;
                self.memo(addr, v, |c| Value::Map(PMap::Small(Arc::new(a.iter().map(|(k, x)| (c.copy(k), c.copy(x))).collect()))))
            }
            PMap::Big(b) => {
                let addr = Arc::as_ptr(b) as usize;
                self.memo(addr, v, |c| Value::Map(PMap::Big(Arc::new(b.iter().map(|(k, x)| (c.copy(k), c.copy(x))).collect()))))
            }
            PMap::Shaped(s) => self.memo(s.ptr(), v, |c| {
                let vals: Vec<Value> = s.vals().iter().map(|x| c.copy(x)).collect();
                Value::Map(PMap::Shaped(ShapedMap::new(s.shape().clone(), vals.into_iter())))
            }),
        }
    }
}

/// One-shot deep copy (see module doc).
pub fn compact(v: &Value) -> Value {
    Compactor::new().copy(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(x: &str) -> Value {
        Value::Str(Str::from(x))
    }

    #[test]
    fn compact_is_equal_fresh_and_keeps_sharing() {
        let shared = s("file:///a/b.clj");
        let el: Value = Value::Map([(s("uri"), shared.clone()), (s("n"), Value::Int(1))].into_iter().collect());
        let v = Value::Vector([el.clone(), el.clone(), shared.clone()].into_iter().collect());
        let c = compact(&v);
        assert!(c == v);
        let (Value::Vector(a), Value::Vector(b)) = (&v, &c) else { panic!() };
        let (Some(Value::Str(s0)), Some(Value::Str(s1))) = (a.get(2), b.get(2)) else { panic!() };
        // fresh allocation, not the original
        assert_ne!(s0.ptr_addr(), s1.ptr_addr());
        // one copy per shared node
        let (Some(Value::Map(m0)), Some(Value::Map(m1))) = (b.get(0), b.get(1)) else { panic!() };
        let (Some(Value::Str(u0)), Some(Value::Str(u1))) = (m0.get(&s("uri")), m1.get(&s("uri"))) else { panic!() };
        assert_eq!(u0.ptr_addr(), u1.ptr_addr());
        assert_eq!(u0.ptr_addr(), s1.ptr_addr());
        // the copy outlives the original
        drop(v);
        drop(el);
        drop(shared);
        assert!(c == compact(&c));
    }
}
