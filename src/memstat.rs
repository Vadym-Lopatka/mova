//! M3: `(mova.mem/malloc-in-use)` -> [blocks bytes] from the default malloc zone (macOS; [0 0] elsewhere).
use std::sync::Arc;

use crate::eval::Interp;
use crate::value::{NativeFn, PVec, Symbol, Value};

#[cfg(target_os = "macos")]
fn in_use() -> (i64, i64) {
    #[repr(C)]
    #[derive(Default)]
    struct MStats {
        blocks_in_use: u32,
        size_in_use: usize,
        max_size_in_use: usize,
        size_allocated: usize,
    }
    unsafe extern "C" {
        fn malloc_zone_statistics(zone: *mut std::ffi::c_void, s: *mut MStats);
    }
    let mut s = MStats::default();
    unsafe { malloc_zone_statistics(std::ptr::null_mut(), &mut s) };
    (s.blocks_in_use as i64, s.size_in_use as i64)
}

#[cfg(not(target_os = "macos"))]
fn in_use() -> (i64, i64) {
    (0, 0)
}

pub(crate) fn register(i: &mut Interp) {
    let f = NativeFn::new("malloc-in-use", |_i: &mut Interp, _a: &[Value]| {
        let (b, n) = in_use();
        Ok(Value::Vector(PVec::from_iter([Value::Int(b), Value::Int(n)])))
    });
    i.globals.set_builtin(Symbol { ns: Some("mova.mem".into()), name: "malloc-in-use".into() }, Value::Native(Arc::new(f)));
    // M6: (mova.mem/pack v) / (mova.mem/pack v min-count) -> columnar vector, or v unchanged.
    let f = NativeFn::new("pack", |_i: &mut Interp, a: &[Value]| {
        let min = match a.get(1) {
            Some(Value::Int(n)) => (*n).max(1) as usize,
            _ => PACK_MIN,
        };
        Ok(match a.first() {
            Some(Value::Vector(pv)) => match crate::colvec::pack(pv, min) {
                Some(p) => Value::Vector(p),
                None => Value::Vector(pv.clone()),
            },
            Some(v) => v.clone(),
            None => Value::Nil,
        })
    });
    i.globals.set_builtin(Symbol { ns: Some("mova.mem".into()), name: "pack".into() }, Value::Native(Arc::new(f)));
    // M6: [packs materializations] since start.
    let f = NativeFn::new("colvec-stats", |_i: &mut Interp, _a: &[Value]| {
        use std::sync::atomic::Ordering::Relaxed;
        let p = crate::colvec::PACKS.load(Relaxed) as i64;
        let m = crate::colvec::MATS.load(Relaxed) as i64;
        Ok(Value::Vector(PVec::from_iter([Value::Int(p), Value::Int(m)])))
    });
    i.globals.set_builtin(Symbol { ns: Some("mova.mem".into()), name: "colvec-stats".into() }, Value::Native(Arc::new(f)));
    // M6: (mova.mem/colvec-bytes v) -> approx column bytes of a packed vector, else nil.
    let f = NativeFn::new("colvec-bytes", |_i: &mut Interp, a: &[Value]| {
        Ok(match a.first() {
            Some(Value::Vector(PVec::Col(c))) => Value::Int(c.data_bytes() as i64),
            _ => Value::Nil,
        })
    });
    i.globals.set_builtin(Symbol { ns: Some("mova.mem".into()), name: "colvec-bytes".into() }, Value::Native(Arc::new(f)));
    // M6 debug: (mova.mem/colvec-layout v) -> [[key int? table-len code-bytes] ...]
    let f = NativeFn::new("colvec-layout", |_i: &mut Interp, a: &[Value]| {
        Ok(match a.first() {
            Some(Value::Vector(PVec::Col(c))) => Value::Vector(
                c.col_keys()
                    .into_iter()
                    .zip(c.layout())
                    .map(|(k, (int, t, b))| Value::Vector(PVec::from_iter([k, Value::Bool(int), Value::Int(t as i64), Value::Int(b as i64)])))
                    .collect(),
            ),
            _ => Value::Nil,
        })
    });
    i.globals.set_builtin(Symbol { ns: Some("mova.mem".into()), name: "colvec-layout".into() }, Value::Native(Arc::new(f)));
    // M6: (mova.mem/packed? v) -> true when v is a columnar vector.
    let f = NativeFn::new("packed?", |_i: &mut Interp, a: &[Value]| {
        Ok(Value::Bool(matches!(a.first(), Some(Value::Vector(PVec::Col(_))))))
    });
    i.globals.set_builtin(Symbol { ns: Some("mova.mem".into()), name: "packed?".into() }, Value::Native(Arc::new(f)));
    // M8: (mova.mem/collect!) -> nil; frees retained allocator pages back to the OS.
    let f = NativeFn::new("collect!", |_i: &mut Interp, _a: &[Value]| {
        collect();
        // M10 debug (heap-prof builds): page census after each collect! to stderr.
        #[cfg(all(feature = "heap-prof", feature = "mimalloc-alloc"))]
        { let (a, b) = mi::census(); eprintln!("M10 collect! census {:?} {:?} heap {:?}", a, b, heap_stats()); }
        Ok(Value::Nil)
    });
    i.globals.set_builtin(Symbol { ns: Some("mova.mem".into()), name: "collect!".into() }, Value::Native(Arc::new(f)));
    // M8 debug: (mova.mem/page-census) -> [pages committed live abandoned-pages ab-committed ab-live] (racy).
    let f = NativeFn::new("page-census", |_i: &mut Interp, _a: &[Value]| {
        #[cfg(feature = "mimalloc-alloc")]
        {
            let (a, b) = mi::census();
            return Ok(Value::Vector(a.iter().chain(b.iter()).map(|&n| Value::Int(n as i64)).collect()));
        }
        #[allow(unreachable_code)]
        Ok(Value::Nil)
    });
    i.globals.set_builtin(Symbol { ns: Some("mova.mem".into()), name: "page-census".into() }, Value::Native(Arc::new(f)));
    // M8 debug: (mova.mem/abandoned-by-size) -> [[block-size pages committed live] ...]
    let f = NativeFn::new("abandoned-by-size", |_i: &mut Interp, _a: &[Value]| {
        #[cfg(feature = "mimalloc-alloc")]
        {
            let m = mi::by_size();
            return Ok(Value::Vector(m.into_iter().map(|(k, v)| Value::Vector([k, v[0], v[1], v[2]].into_iter().map(|n| Value::Int(n as i64)).collect())).collect()));
        }
        #[allow(unreachable_code)]
        Ok(Value::Nil)
    });
    i.globals.set_builtin(Symbol { ns: Some("mova.mem".into()), name: "abandoned-by-size".into() }, Value::Native(Arc::new(f)));
    // M8: (mova.mem/compact v) -> deep copy of v on fresh pages (see compact.rs).
    let f = NativeFn::new("compact", |_i: &mut Interp, a: &[Value]| Ok(a.first().map(crate::compact::compact).unwrap_or(Value::Nil)));
    i.globals.set_builtin(Symbol { ns: Some("mova.mem".into()), name: "compact".into() }, Value::Native(Arc::new(f)));
    // M8: (mova.mem/heap-stats) -> [commit peak-commit rss peak-rss] bytes.
    let f = NativeFn::new("deep-size", |_i: &mut Interp, a: &[Value]| {
        let mut w = dsize::W::default();
        // (deep-size v) or (deep-size v :key) -> also counts maps holding :key (3rd slot)
        w.key = a.get(1).cloned();
        if let Some(v) = a.first() { w.walk(v); }
        let mut top: Vec<_> = w.insts.into_iter().collect();
        top.sort_by(|x, y| y.1.cmp(&x.1));
        let insts: Vec<Value> = top.into_iter().take(10).map(|(n, c)| Value::Vector(PVec::from(vec![Value::Str(n.into()), Value::Int(c as i64)]))).collect();
        Ok(Value::Vector(PVec::from(vec![Value::Int(w.bytes as i64), Value::Vector(PVec::from(insts)), Value::Int(w.hits as i64)])))
    });
    i.globals.set_builtin(Symbol { ns: Some("mova.mem".into()), name: "deep-size".into() }, Value::Native(Arc::new(f)));
    let f = NativeFn::new("heap-stats", |_i: &mut Interp, _a: &[Value]| {
        Ok(Value::Vector(heap_stats().into_iter().map(Value::Int).collect()))
    });
    i.globals.set_builtin(Symbol { ns: Some("mova.mem".into()), name: "heap-stats".into() }, Value::Native(Arc::new(f)));
    // M10 debug: (mova.mem/dyn-census var) -> [ctx-entries total-frames] of its binding stacks.
    let f = NativeFn::new("dyn-census", |_i: &mut Interp, a: &[Value]| {
        let (n, t) = match a.first() { Some(Value::Var(c)) => c.dyn_census(), _ => (0, 0) };
        Ok(Value::Vector(PVec::from_iter([Value::Int(n as i64), Value::Int(t as i64)])))
    });
    i.globals.set_builtin(Symbol { ns: Some("mova.mem".into()), name: "dyn-census".into() }, Value::Native(Arc::new(f)));
    // M10 debug: (mova.mem/ab-paths v) -> [[path nodes] ...]: nodes of v that sit on abandoned pages, by map-key path.
    #[cfg(feature = "mimalloc-alloc")]
    {
        let f = NativeFn::new("ab-paths", |_i: &mut Interp, a: &[Value]| {
            let mut w = abpaths::W { ranges: mi::ab_ranges(), ..Default::default() };
            if let Some(v) = a.first() { w.walk(v, 0); }
            let mut top: Vec<_> = w.hits.into_iter().collect();
            top.sort_by(|x, y| y.1.cmp(&x.1));
            Ok(Value::Vector(top.into_iter().take(40).map(|(p, n)| Value::Vector(PVec::from(vec![Value::Str(p.into()), Value::Int(n as i64)]))).collect()))
        });
        i.globals.set_builtin(Symbol { ns: Some("mova.mem".into()), name: "ab-paths".into() }, Value::Native(Arc::new(f)));
    }
    // M10 debug (leak-probe): (mova.mem/leak-counters) -> [closures-created dropped frames-created dropped].
    #[cfg(feature = "leak-probe")]
    {
        let f = NativeFn::new("leak-counters", |_i: &mut Interp, _a: &[Value]| {
            use std::sync::atomic::Ordering::Relaxed;
            let v = [crate::value::CLOSURE_CREATED.load(Relaxed), crate::value::CLOSURE_DROPPED.load(Relaxed), crate::env::FRAME_CREATED.load(Relaxed), crate::env::FRAME_DROPPED.load(Relaxed)];
            Ok(Value::Vector(v.into_iter().map(|x| Value::Int(x as i64)).collect()))
        });
        i.globals.set_builtin(Symbol { ns: Some("mova.mem".into()), name: "leak-counters".into() }, Value::Native(Arc::new(f)));
    }
}

/// M8: return free mimalloc pages to the OS now (no-op without mimalloc).
pub(crate) fn collect() {
    #[cfg(feature = "mimalloc-alloc")]
    unsafe {
        libmimalloc_sys::mi_collect(true)
    }
}

/// M8: [current-commit peak-commit current-rss peak-rss] bytes from mimalloc ([0 0 0 0] without it).
fn heap_stats() -> [i64; 4] {
    #[cfg(feature = "mimalloc-alloc")]
    unsafe {
        let (mut e, mut u, mut s, mut rss, mut prss, mut c, mut pc, mut pf) = (0usize, 0usize, 0usize, 0usize, 0usize, 0usize, 0usize, 0usize);
        libmimalloc_sys::mi_process_info(&mut e, &mut u, &mut s, &mut rss, &mut prss, &mut c, &mut pc, &mut pf);
        return [c as i64, pc as i64, rss as i64, prss as i64];
    }
    #[allow(unreachable_code)]
    [0; 4]
}

#[cfg(feature = "mimalloc-alloc")]
mod mi {
    use std::ffi::c_void;
    #[repr(C)]
    pub struct Area {
        pub blocks: *mut c_void,
        pub reserved: usize,
        pub committed: usize,
        pub used: usize,
        pub block_size: usize,
        pub full_block_size: usize,
        pub reserved1: *mut c_void,
    }
    pub type Visit = extern "C" fn(*const c_void, *const Area, *mut c_void, usize, *mut c_void) -> bool;
    unsafe extern "C" {
        pub fn mi_heap_visit_blocks(heap: *mut c_void, visit_blocks: bool, f: Visit, arg: *mut c_void) -> bool;
        pub fn mi_heap_visit_abandoned_blocks(heap: *mut c_void, visit_blocks: bool, f: Visit, arg: *mut c_void) -> bool;
    }
    /// [pages page-committed-bytes live-bytes]
    extern "C" fn visit(_h: *const c_void, a: *const Area, _b: *mut c_void, _s: usize, arg: *mut c_void) -> bool {
        let acc = unsafe { &mut *(arg as *mut [usize; 3]) };
        let a = unsafe { &*a };
        acc[0] += 1;
        acc[1] += a.committed;
        acc[2] += a.used * a.block_size;
        true
    }
    extern "C" fn visit_bs(_h: *const c_void, a: *const Area, _b: *mut c_void, _s: usize, arg: *mut c_void) -> bool {
        let m = unsafe { &mut *(arg as *mut std::collections::BTreeMap<usize, [usize; 3]>) };
        let a = unsafe { &*a };
        let e = m.entry(a.block_size).or_default();
        e[0] += 1;
        e[1] += a.committed;
        e[2] += a.used * a.block_size;
        true
    }
    /// Abandoned pages by block size: block-size -> [pages committed live].
    pub fn by_size() -> std::collections::BTreeMap<usize, [usize; 3]> {
        let mut m = std::collections::BTreeMap::new();
        unsafe { mi_heap_visit_abandoned_blocks(std::ptr::null_mut(), false, visit_bs, &mut m as *mut _ as *mut c_void) };
        m
    }
    extern "C" fn visit_rng(_h: *const c_void, a: *const Area, _b: *mut c_void, _s: usize, arg: *mut c_void) -> bool {
        let v = unsafe { &mut *(arg as *mut Vec<(usize, usize)>) };
        let a = unsafe { &*a };
        if a.used > 0 { v.push((a.blocks as usize, a.blocks as usize + a.committed.max(a.reserved))); }
        true
    }
    /// M10 debug: sorted [start, end) of abandoned pages that still hold used blocks.
    pub fn ab_ranges() -> Vec<(usize, usize)> {
        let mut v = Vec::new();
        unsafe { mi_heap_visit_abandoned_blocks(std::ptr::null_mut(), false, visit_rng, &mut v as *mut _ as *mut c_void) };
        v.sort();
        v
    }
    /// Racy debug census of the main heap's pages: (all, abandoned-only).
    pub fn census() -> ([usize; 3], [usize; 3]) {
        let (mut all, mut ab) = ([0usize; 3], [0usize; 3]);
        unsafe {
            mi_heap_visit_blocks(std::ptr::null_mut(), false, visit, &mut all as *mut _ as *mut c_void);
            mi_heap_visit_abandoned_blocks(std::ptr::null_mut(), false, visit, &mut ab as *mut _ as *mut c_void);
        }
        (all, ab)
    }
}

/// M6: default minimum element count for `pack`.
const PACK_MIN: usize = 8;

/// M9 diagnostic: approximate deep bytes of values (shared nodes counted once) + record counts by type.
mod dsize {
    use crate::value::{PMap, PVec, Value};
    use std::collections::{HashMap, HashSet};
    #[derive(Default)]
    pub struct W { seen: HashSet<usize>, pub bytes: usize, pub insts: HashMap<String, usize>, pub key: Option<Value>, pub hits: usize }
    impl W {
        fn once(&mut self, a: usize) -> bool { self.seen.insert(a) }
        fn str(&mut self, s: &crate::value::Str) { if self.once(s.ptr_addr()) { self.bytes += 24 + s.as_contiguous().len(); } }
        pub fn walk(&mut self, v: &Value) {
            if !matches!(v, Value::Str(_) | Value::Uri(_) | Value::Sym(_) | Value::Vector(_) | Value::List(_) | Value::Map(_) | Value::Set(_) | Value::Meta(_) | Value::Nil | Value::Bool(_) | Value::Int(_) | Value::Float(_) | Value::Keyword(_) | Value::Char(_)) {
                *self.insts.entry(format!("<{}>", v.type_name())).or_default() += 1;
            }
            match v {
                Value::Str(s) | Value::Uri(s) => self.str(s),
                Value::Sym(s) => { if let Some(n) = &s.ns { self.str(n) } self.str(&s.name) }
                Value::Vector(p) | Value::List(p) => self.pvec(p),
                Value::Map(m) => self.pmap(m),
                Value::Set(s) => { self.bytes += 48 * s.len() + 32; for x in s.iter() { self.walk(x) } }
                Value::Meta(m) => { if self.once(std::sync::Arc::as_ptr(m) as usize) { self.bytes += 48; self.walk(&m.meta); self.walk(&m.inner) } }
                Value::Inst(i) => {
                    if self.once(std::sync::Arc::as_ptr(i) as usize) {
                        self.bytes += 112;
                        *self.insts.entry(i.tdef.name.as_contiguous().to_string()).or_default() += 1;
                        self.pmap(&i.data);
                        if let Some(m) = &i.meta { self.pmap(m) }
                    }
                }
                Value::Atom(a) => { if self.once(std::sync::Arc::as_ptr(a) as usize) { self.bytes += 96; let x = a.state.lock().unwrap().1.clone(); self.walk(&x) } }
                Value::Volatile(a) => { if self.once(std::sync::Arc::as_ptr(a) as usize) { self.bytes += 48; let x = a.read().unwrap().clone(); self.walk(&x) } }
                Value::Lazy(l) | Value::LazyTail(l) => { if self.once(std::sync::Arc::as_ptr(l) as usize) { self.bytes += 64; let x = l.realized.lock().unwrap().clone(); if let Some(x) = x { self.walk(&x) } else { *self.insts.entry("<unrealized>".into()).or_default() += 1 } } }
                _ => {}
            }
        }
        fn pvec(&mut self, p: &PVec) {
            let (a, b) = match p {
                PVec::Small(a) => (std::sync::Arc::as_ptr(a) as *const Value as usize, 32 + 16 * a.len()),
                PVec::Big(b) => (std::sync::Arc::as_ptr(b) as usize, 64 + 24 * b.len()),
                PVec::Col(c) => (std::sync::Arc::as_ptr(c) as usize, 64 + c.data_bytes()),
            };
            if self.once(a) { self.bytes += b; for x in p.iter_cloned() { self.walk(&x) } }
        }
        fn pmap(&mut self, m: &PMap) {
            let (a, b) = match m {
                PMap::Small(a) => (std::sync::Arc::as_ptr(a) as *const u8 as usize, 32 + 32 * a.len()),
                PMap::Big(b) => (std::sync::Arc::as_ptr(b) as usize, 64 + 64 * b.len()),
                PMap::Shaped(s) => (s.ptr(), 48 + 16 * s.vals().len()),
            };
            if self.once(a) {
                self.bytes += b;
                for (k, v) in m.iter() {
                    if self.key.as_ref() == Some(&k.clone()) { self.hits += 1 }
                    self.walk(&k.clone()); self.walk(&v.clone())
                }
            }
        }
    }
}

/// M10: champ's node pool is a per-thread free list with no Drop, so a thread exiting with parked nodes leaked them
/// (~90 KB per short future, MBs per kondo worker). Hold one of these for the life of every Mova-spawned thread.
pub(crate) struct ThreadPoolDrain;

impl Drop for ThreadPoolDrain {
    fn drop(&mut self) {
        champ::pool_drain();
    }
}

/// M10: `f` as a thread body that drains this thread's champ pool after `f` and everything it captured are dropped.
pub(crate) fn drained<R>(f: impl FnOnce() -> R) -> impl FnOnce() -> R {
    move || {
        let _g = ThreadPoolDrain;
        f()
    }
}

/// M10 debug: walk a value and count its Arc nodes that live on abandoned pages, keyed by path.
#[cfg(feature = "mimalloc-alloc")]
mod abpaths {
    use crate::value::{PMap, PVec, Value};
    use std::collections::{HashMap, HashSet};
    #[derive(Default)]
    pub struct W {
        pub ranges: Vec<(usize, usize)>,
        pub hits: HashMap<String, usize>,
        pub seen: HashSet<usize>,
        pub path: Vec<String>,
    }
    impl W {
        fn on_ab(&self, a: usize) -> bool {
            let i = self.ranges.partition_point(|r| r.0 <= a);
            i > 0 && a < self.ranges[i - 1].1
        }
        fn node(&mut self, a: usize) -> bool {
            if !self.seen.insert(a) { return false; }
            if self.on_ab(a) { *self.hits.entry(self.path.join(" ")).or_default() += 1; }
            true
        }
        pub fn walk(&mut self, v: &Value, depth: usize) {
            match v {
                Value::Str(s) | Value::Uri(s) => { self.node(s.ptr_addr()); }
                Value::Vector(p) | Value::List(p) => self.pvec(p, depth),
                Value::Map(m) => self.pmap(m, depth),
                Value::Set(s) => { for x in s.iter() { self.walk(x, depth) } }
                Value::Meta(m) => { if self.node(std::sync::Arc::as_ptr(m) as usize) { self.walk(&m.meta, depth); self.walk(&m.inner, depth) } }
                Value::Inst(i) => { if self.node(std::sync::Arc::as_ptr(i) as usize) { self.path.push(format!("#{}", i.tdef.name.as_contiguous())); self.pmap(&i.data, depth + 1); self.path.pop(); } }
                Value::Atom(a) => { if self.node(std::sync::Arc::as_ptr(a) as usize) { let x = a.state.lock().unwrap().1.clone(); self.path.push("@".into()); self.walk(&x, depth + 1); self.path.pop(); } }
                Value::Lazy(l) | Value::LazyTail(l) => { if self.node(std::sync::Arc::as_ptr(l) as usize) { let x = l.realized.lock().unwrap().clone(); if let Some(x) = x { self.path.push("lazy".into()); self.walk(&x, depth + 1); self.path.pop(); } } }
                Value::Fn(c) => { self.node(std::sync::Arc::as_ptr(c) as usize); }
                _ => {}
            }
        }
        fn pvec(&mut self, p: &PVec, depth: usize) {
            let a = match p { PVec::Small(a) => std::sync::Arc::as_ptr(a) as *const u8 as usize, PVec::Big(b) => std::sync::Arc::as_ptr(b) as usize, PVec::Col(c) => std::sync::Arc::as_ptr(c) as usize };
            if !self.node(a) { return; }
            if matches!(p, PVec::Col(_)) { self.path.push("col".into()); }
            for x in p.iter() { self.walk(&x, depth) }
            if matches!(p, PVec::Col(_)) { self.path.pop(); }
        }
        fn pmap(&mut self, m: &PMap, depth: usize) {
            let a = match m { PMap::Small(a) => std::sync::Arc::as_ptr(a) as *const u8 as usize, PMap::Big(b) => std::sync::Arc::as_ptr(b) as usize, PMap::Shaped(s) => s.ptr() };
            if !self.node(a) { return; }
            for (k, x) in m.iter() {
                let deep = depth < 3 && !matches!(k, Value::Str(_) | Value::Uri(_));
                if deep { self.path.push(match k { Value::Keyword(kw) => format!(":{kw:?}"), Value::Sym(s) => s.name.as_contiguous().to_string(), _ => "?".into() }); }
                self.walk(&x, depth + deep as usize);
                if deep { self.path.pop(); }
            }
        }
    }
}
