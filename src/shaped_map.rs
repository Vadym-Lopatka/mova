//! M3 shaped maps (hidden classes): a map of 9..=SHAPED_MAX interned-keyword keys stores one shared,
//! interned `Arc<MapShape>` (key order = CHAMP's canonical order for that key set, so iteration,
//! printing and seq order are identical to `PMap::Big`) plus only the values.
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use triomphe::ThinArc;

use crate::keyword::Keyword;
use crate::value::Value;

/// Max keys a shaped map holds; a new key past this goes to `PMap::Big`.
pub const SHAPED_MAX: usize = 32;
/// Max distinct shapes process-wide (shapes are never freed); past it maps stay `Big`.
const SHAPES_CAP: usize = 8192;
/// Marker in a transition remap: slot takes the newly assoc'ed value.
const NEW: u8 = u8::MAX;

pub struct MapShape {
    pub keys: Box<[Value]>,
    pub ids: Box<[u32]>,
    // assoc-new-key transitions: (key id, child shape, child slot -> parent slot or NEW)
    trans: Mutex<Vec<(u32, Arc<MapShape>, Arc<[u8]>)>>,
}

impl MapShape {
    #[inline]
    pub fn index_of_id(&self, id: u32) -> Option<usize> {
        self.ids.iter().position(|&x| x == id)
    }

    /// Exact slot lookup for any key `Value`.
    #[inline]
    pub fn index_of(&self, k: &Value) -> Option<usize> {
        match k {
            Value::Keyword(kw) => match kw.interned_id() {
                Some(id) => self.index_of_id(id),
                None => self.keys.iter().position(|k2| k2 == k),
            },
            _ => None,
        }
    }

    /// Child shape for assoc of new key `id`, plus the slot remap; cached per shape.
    pub fn child(&self, id: u32) -> Option<(Arc<MapShape>, Arc<[u8]>)> {
        if self.ids.len() >= SHAPED_MAX {
            return None;
        }
        let mut t = self.trans.lock().unwrap();
        if let Some((_, s, r)) = t.iter().find(|(i, _, _)| *i == id) {
            return Some((s.clone(), r.clone()));
        }
        let mut ids: Vec<u32> = self.ids.to_vec();
        ids.push(id);
        let s = shape_for(&ids)?;
        let r: Arc<[u8]> = s.ids.iter().map(|&c| if c == id { NEW } else { self.index_of_id(c).unwrap() as u8 }).collect();
        t.push((id, s.clone(), r.clone()));
        Some((s, r))
    }
}

/// Shape pointer + values in ONE allocation (triomphe `ThinArc`, 8-byte handle).
#[derive(Clone)]
pub struct ShapedMap(ThinArc<Arc<MapShape>, Value>);

impl ShapedMap {
    /// metrics: sole handle to this allocation.
    #[inline]
    pub(crate) fn is_unique(&self) -> bool {
        triomphe::ThinArc::strong_count(&self.0) == 1
    }

    pub(crate) fn new(shape: Arc<MapShape>, vals: impl ExactSizeIterator<Item = Value>) -> ShapedMap {
        ShapedMap(ThinArc::from_header_and_iter(shape, vals))
    }

    #[inline]
    pub fn shape(&self) -> &Arc<MapShape> {
        &self.0.header.header
    }

    #[inline]
    pub fn vals(&self) -> &[Value] {
        &self.0.slice
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.0.slice.len()
    }

    pub fn ptr(&self) -> usize {
        self.0.ptr() as usize
    }

    pub fn strong_count(&self) -> usize {
        ThinArc::strong_count(&self.0)
    }

    pub fn ptr_eq(&self, o: &ShapedMap) -> bool {
        self.0.ptr() == o.0.ptr()
    }

    #[inline]
    pub fn get(&self, k: &Value) -> Option<&Value> {
        self.shape().index_of(k).map(|i| &self.0.slice[i])
    }

    /// Replaces slot `i` (in place when uniquely held, else copy); returns the old value.
    pub fn set(&mut self, i: usize, v: Value) -> Value {
        let mut v = Some(v);
        let done = self.0.with_arc_mut(|a| triomphe::Arc::get_mut(a).map(|hs| std::mem::replace(&mut hs.slice_mut()[i], v.take().unwrap())));
        if let Some(old) = done {
            return old;
        }
        let v = v.unwrap();
        let old = self.0.slice[i].clone();
        let shape = self.shape().clone();
        let vals: Vec<Value> = self.0.slice.iter().enumerate().map(|(j, x)| if j == i { v.clone() } else { x.clone() }).collect();
        *self = ShapedMap::new(shape, vals.into_iter());
        old
    }

    /// Builds from `(key, value)` pairs with distinct keys; `None` unless every key is an interned keyword and a shape exists.
    pub fn from_pairs<'a>(pairs: impl Iterator<Item = (&'a Value, &'a Value)> + Clone) -> Option<ShapedMap> {
        if !enabled() {
            return None;
        }
        let mut ids = Vec::with_capacity(SHAPED_MAX);
        for (k, _) in pairs.clone() {
            match k {
                Value::Keyword(kw) => ids.push(kw.interned_id()?),
                _ => return None,
            }
            if ids.len() > SHAPED_MAX {
                return None;
            }
        }
        let shape = shape_for(&ids)?;
        let mut vals: Vec<Value> = vec![Value::Nil; ids.len()];
        for ((_, v), id) in pairs.zip(ids.iter()) {
            vals[shape.index_of_id(*id).unwrap()] = v.clone();
        }
        Some(ShapedMap::new(shape, vals.into_iter()))
    }

    /// Assoc of a key NOT in the map; `None` when no child shape (caller falls back to `Big`).
    pub fn with_new(&self, k: &Value, v: Value) -> Option<ShapedMap> {
        let id = match k {
            Value::Keyword(kw) => kw.interned_id()?,
            _ => return None,
        };
        let (shape, remap) = self.shape().child(id)?;
        let mut v = Some(v);
        let old = self.vals();
        let vals = remap.iter().map(|&r| if r == NEW { v.take().unwrap() } else { old[r as usize].clone() });
        Some(ShapedMap::new(shape, vals))
    }

    /// Dissoc of slot `i`; `None` when no shape for the smaller key set.
    pub fn without_slot(&self, i: usize) -> Option<ShapedMap> {
        let me = self.shape();
        let ids: Vec<u32> = me.ids.iter().enumerate().filter(|(j, _)| *j != i).map(|(_, &x)| x).collect();
        let shape = shape_for(&ids)?;
        let vals: Vec<Value> = shape.ids.iter().map(|&c| self.vals()[me.index_of_id(c).unwrap()].clone()).collect();
        Some(ShapedMap::new(shape, vals.into_iter()))
    }
}

/// Kill switch: `MOVA_SHAPED=0` keeps every map on the pre-M3 Small/Big path.
fn enabled() -> bool {
    static E: OnceLock<bool> = OnceLock::new();
    *E.get_or_init(|| std::env::var("MOVA_SHAPED").map_or(true, |v| v != "0"))
}

type Table = HashMap<Box<[u32]>, Option<Arc<MapShape>>>;

fn table() -> &'static Mutex<Table> {
    static T: OnceLock<Mutex<Table>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Interned shape for a set of distinct keyword ids (any order). `None` if the set is refused
/// (its CHAMP order depends on insertion order, i.e. a hash collision) or the table is full.
pub fn shape_for(ids: &[u32]) -> Option<Arc<MapShape>> {
    let mut sorted: Vec<u32> = ids.to_vec();
    sorted.sort_unstable();
    let mut t = table().lock().unwrap();
    if let Some(s) = t.get(sorted.as_slice()) {
        return s.clone();
    }
    if t.len() >= SHAPES_CAP {
        return None;
    }
    let kw = |id: u32| Value::Keyword(Keyword::Interned(id));
    let order = |it: &mut dyn Iterator<Item = u32>| -> Vec<Value> {
        let mut m = champ::PersistentHashMap::<Value, Value>::new().transient();
        for id in it {
            m.assoc(kw(id), Value::Nil);
        }
        m.persistent().iter().map(|(k, _)| k.clone()).collect()
    };
    let fwd = order(&mut sorted.iter().copied());
    let rev = order(&mut sorted.iter().rev().copied());
    let shape = if fwd == rev && fwd.len() == sorted.len() {
        let ids: Box<[u32]> = fwd.iter().map(|k| if let Value::Keyword(k) = k { k.interned_id().unwrap() } else { 0 }).collect();
        Some(Arc::new(MapShape { keys: fwd.into_boxed_slice(), ids, trans: Mutex::new(Vec::new()) }))
    } else {
        None
    };
    t.insert(sorted.into_boxed_slice(), shape.clone());
    shape
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::PMap;

    fn kw(s: &str) -> Value {
        Value::Keyword(Keyword::construct(s))
    }

    #[test]
    fn shaped_matches_big_order_and_one_allocation() {
        let ks: Vec<Value> = (0..16).map(|i| kw(&format!("m3k{i}"))).collect();
        let mut m = PMap::new();
        for (i, k) in ks.iter().enumerate() {
            m.insert(k.clone(), Value::Int(i as i64));
        }
        let PMap::Shaped(s) = &m else { panic!("expected Shaped") };
        let big: champ::PersistentHashMap<Value, Value> = ks.iter().cloned().map(|k| (k, Value::Nil)).collect();
        let bk: Vec<&Value> = big.iter().map(|(k, _)| k).collect();
        let sk: Vec<&Value> = m.iter().map(|(k, _)| k).collect();
        assert_eq!(bk, sk);
        #[cfg(target_os = "macos")]
        {
            unsafe extern "C" {
                fn malloc_size(p: *const std::ffi::c_void) -> usize;
            }
            let n = unsafe { malloc_size(s.0.heap_ptr()) };
            assert!(n <= 640, "shaped map block {n} B"); // 536 B request; xzone malloc bins it at 640
        }
        let mut m2 = m.clone();
        assert_eq!(m2.remove(&ks[3]), Some(Value::Int(3)));
        assert_eq!(m2.len(), 15);
        assert!(m2.get(&ks[3]).is_none());
        assert_eq!(m2.insert(ks[3].clone(), Value::Int(3)), None);
        assert!(m2 == m);
        assert!(matches!(m2, PMap::Shaped(_)));
    }
}
