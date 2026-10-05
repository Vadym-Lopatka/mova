//! Heap-image GATE-1 kill-probe (docs/HEAP-IMAGE-DESIGN.md). THROWAWAY.
//!
//! Delta image: the restoring process boots core normally (`Interp::new`),
//! re-derives an index of every identity-bearing boot object by a
//! deterministic walk (`PreIndex`), then decodes the post-load delta. New
//! objects are written inline (post-order, so a reader can build them
//! bottom-up); every mutable cell (var, atom, volatile, lazy seq, delay,
//! env frame) is written as a SHELL first and filled later by a FIX record,
//! which is what breaks every Arc cycle. Every boot-time mutable cell is
//! re-FIXed with its post-load content, so load-time mutation of core state
//! is carried over. Registries (namespaces, protocols, interfaces,
//! multimethods, keyword registry, source registry, gensym counters) are
//! written wholesale with pointer keys remapped through object ids.
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, RwLock};

use crate::env::{Env, VarCell};
use crate::eval::Interp;
use crate::reader::{Form, FormValue, Span};
use crate::types::{ClassKey, ClassVal, InstVal, TypeDef};
use crate::value::*;

#[path = "image_ir.rs"]
mod image_ir;

const MAGIC: &[u8; 8] = b"MOVAIMG1";

/// How to re-create a native minted at load time (see `NativeFn::image_recipe`).
pub enum Recipe {
    Proto { mname: Str, var_display: String, iface: Str, pname: Str, arity: (usize, usize), cell: Arc<VarCell>, epoch: u64, midx: usize },
    Ctor { tdef: Arc<TypeDef>, name: String },
    Basis { tdef: Arc<TypeDef> },
    MapCtor { tdef: Arc<TypeDef>, name: String },
}
const T_RECIPE: u8 = 32;
const T_NUMTXT: u8 = 35;
const T_SORTEDMAP: u8 = 36;
const T_NS: u8 = 33;
const T_CLASS_BUILTIN: u8 = 34;

#[derive(Clone)]
enum Obj {
    V(Value),
    Env(Env),
    Ar(Arc<Vec<Arity>>),
    Td(Arc<TypeDef>),
    S(Str),
    Vec(PVec),
    Map(PMap),
}

fn vptr(v: &Value) -> Option<usize> {
    Some(match v {
        Value::Fn(a) | Value::Macro(a) => Arc::as_ptr(a) as usize,
        Value::Native(a) => Arc::as_ptr(a) as *const u8 as usize,
        Value::Var(a) => Arc::as_ptr(a) as usize,
        Value::Atom(a) => Arc::as_ptr(a) as usize,
        Value::Volatile(a) => Arc::as_ptr(a) as usize,
        Value::Lazy(a) | Value::LazyTail(a) => Arc::as_ptr(a) as usize,
        Value::Delay(a) => Arc::as_ptr(a) as usize,
        Value::Promise(a) => Arc::as_ptr(a) as usize,
        Value::Future(a) => Arc::as_ptr(a) as usize,
        Value::Channel(a) => Arc::as_ptr(a) as usize,
        Value::Regex(a) => Arc::as_ptr(a) as usize,
        Value::Class(a) => Arc::as_ptr(a) as usize,
        Value::Inst(a) => Arc::as_ptr(a) as usize,
        Value::HostInst(a) => Arc::as_ptr(a) as usize,
        Value::HostStruct(a) => Arc::as_ptr(a) as usize,
        Value::Array(a) => Arc::as_ptr(a) as usize,
        Value::Timer(a) => Arc::as_ptr(a) as usize,
        Value::Flow(a) => Arc::as_ptr(a) as usize,
        _ => return None,
    })
}

fn is_mutable_cell(v: &Value) -> bool {
    matches!(v, Value::Var(_) | Value::Atom(_) | Value::Volatile(_) | Value::Lazy(_) | Value::LazyTail(_) | Value::Delay(_))
}

// ------------------------------------------------------------ pre-index
pub struct PreIndex {
    ptr2id: HashMap<usize, u32>,
    objs: Vec<Obj>,
    checksum: u64,
}

struct PreWalk {
    ptr2id: HashMap<usize, u32>,
    objs: Vec<Obj>,
    h: u64,
}
impl PreWalk {
    fn mix(&mut self, x: u64) {
        self.h = (self.h ^ x).wrapping_mul(0x100000001b3);
    }
    fn reg(&mut self, p: usize, o: Obj, tag: u64) -> bool {
        if self.ptr2id.contains_key(&p) {
            return false;
        }
        self.ptr2id.insert(p, self.objs.len() as u32);
        self.objs.push(o);
        self.mix(tag);
        true
    }
    fn val(&mut self, v: &Value) {
        // LazyMap: image stores the materialized map (write-time cost only)
        if let Value::LazyMap(lm) = v {
            return self.val(&Value::Map(crate::lazy_map::as_pmap(lm).clone()));
        }
        if let Some(p) = vptr(v) {
            if !self.reg(p, Obj::V(v.clone()), skey(v)) {
                return;
            }
        }
        match v {
            Value::List(p) | Value::Vector(p) | Value::MapEntry(p) | Value::Queue(p) => p.iter().for_each(|x| self.val(x)),
            Value::Map(PMap::Small(m)) => m.iter().for_each(|(k, x)| {
                self.val(k);
                self.val(x)
            }),
            Value::Map(m) => {
                // CHAMP order can depend on pointer hashes: sort by a stable key
                let mut es: Vec<(u64, &Value, &Value)> = m.iter().map(|(k, x)| (skey(k), k, x)).collect();
                es.sort_by_key(|e| e.0);
                for (_, k, x) in es {
                    self.val(k);
                    self.val(x)
                }
            }
            Value::Set(s) => {
                let mut es: Vec<(u64, &Value)> = s.iter().map(|x| (skey(x), x)).collect();
                es.sort_by_key(|e| e.0);
                es.into_iter().for_each(|(_, x)| self.val(x))
            }
            Value::Meta(m) => {
                self.val(&m.meta);
                self.val(&m.inner)
            }
            Value::Fn(c) | Value::Macro(c) => self.closure(c),
            Value::Var(c) => {
                if let Some(x) = c.raw_root() {
                    self.val(&x)
                }
                self.val(&c.img_meta())
            }
            Value::Atom(a) => {
                let s = crate::sync::lock_mutex(&a.state).1.clone();
                self.val(&s)
            }
            Value::Volatile(a) => {
                let s = crate::sync::lock_read(a).clone();
                self.val(&s)
            }
            Value::Lazy(l) | Value::LazyTail(l) => {
                let t = crate::sync::lock_mutex(&l.thunk).clone();
                let r = crate::sync::lock_mutex(&l.realized).clone();
                t.iter().chain(r.iter()).for_each(|x| self.val(x))
            }
            Value::Delay(d) => {
                let f = crate::sync::lock_mutex(&d.f).clone();
                f.iter().for_each(|x| self.val(x))
            }
            Value::Class(c) => {
                if let ClassVal::User(t) = &**c {
                    self.tdef(t)
                }
            }
            Value::Inst(i) => {
                self.tdef(&i.tdef);
                i.data.iter().for_each(|(k, x)| {
                    self.val(k);
                    self.val(x)
                })
            }
            _ => {}
        }
    }
    fn tdef(&mut self, t: &Arc<TypeDef>) {
        if self.reg(Arc::as_ptr(t) as usize, Obj::Td(t.clone()), 90 ^ skey(&Value::Str(t.name.clone()))) {
            let mut ms: Vec<_> = t.methods.iter().collect();
            ms.sort_by(|a, b| (&**a.0).cmp(&**b.0));
            for (_, m) in ms {
                self.val(m)
            }
        }
    }
    fn closure(&mut self, c: &Arc<Closure>) {
        if self.reg(Arc::as_ptr(&c.arities) as usize, Obj::Ar(c.arities.clone()), 91 ^ c.arities.len() as u64) {
            for a in c.arities.iter() {
                a.body.iter().for_each(|f| self.form(f))
            }
        }
        self.env(&c.env)
    }
    fn form(&mut self, f: &Form) {
        if let Some(m) = &f.meta {
            self.form(m)
        }
        match &f.value {
            FormValue::Atom(v) => self.val(v),
            FormValue::List(v) | FormValue::Vector(v) | FormValue::Set(v) => v.iter().for_each(|x| self.form(x)),
            FormValue::Map(v) => v.iter().for_each(|(k, x)| {
                self.form(k);
                self.form(x)
            }),
        }
    }
    fn env(&mut self, e: &Env) {
        if e.img_is_root() {
            return;
        }
        let sig = e.img_frame().map_or(0, |(vs, _)| vs.iter().fold(92u64, |a, (k, _)| a.rotate_left(5) ^ skey(&Value::Sym(k.clone()))));
        if self.reg(e.img_ptr(), Obj::Env(e.clone()), sig) {
            if let Some((vars, parent)) = e.img_frame() {
                vars.iter().for_each(|(_, v)| self.val(v));
                self.env(&parent)
            }
        }
    }
}

/// Address-independent key: used to order unordered collections in the
/// pre-walk and as the per-object checksum contribution.
fn skey(v: &Value) -> u64 {
    fn h(tag: u64, s: &str) -> u64 {
        let mut x = 0xcbf29ce484222325u64 ^ tag;
        for b in s.as_bytes() {
            x = (x ^ *b as u64).wrapping_mul(0x100000001b3);
        }
        x
    }
    match v {
        Value::Keyword(k) => h(1, k),
        Value::Str(s) => h(2, s),
        Value::Sym(s) => h(3, &s.name) ^ s.ns.as_ref().map_or(0, |n| h(33, n)),
        Value::Int(i) => h(4, "") ^ (*i as u64),
        Value::Class(c) => h(5, c.name()),
        Value::Var(c) => h(6, &c.name.name) ^ c.name.ns.as_ref().map_or(0, |n| h(66, n)),
        Value::Fn(c) | Value::Macro(c) => h(7, c.name.as_deref().unwrap_or("")) ^ ((c.def_span.start as u64) << 20) ^ c.def_span.end as u64,
        Value::Native(n) => h(8, &n.name),
        Value::Nil => 9,
        Value::Bool(b) => 10 + *b as u64,
        _ => 12 + value_tag(v) as u64,
    }
}

fn value_tag(v: &Value) -> u8 {
    // stable small discriminant for the checksum / census
    match v {
        Value::Fn(_) => 1,
        Value::Macro(_) => 2,
        Value::Native(_) => 3,
        Value::Var(_) => 4,
        Value::Atom(_) => 5,
        Value::Class(_) => 6,
        Value::Inst(_) => 7,
        Value::HostInst(_) => 8,
        _ => 9,
    }
}

/// Deterministic walk of the (freshly booted) interpreter state.
pub fn pre_index(interp: &Interp) -> PreIndex {
    let mut w = PreWalk { ptr2id: HashMap::new(), objs: Vec::new(), h: 0xcbf29ce484222325 };
    for (sym, cell) in interp.globals.img_root_entries() {
        w.mix(sym.name.len() as u64);
        w.val(&Value::Var(cell));
    }
    // registries' values, in a deterministic order
    let mut multis: Vec<(Str, Vec<Value>)> = crate::sync::lock_read(&interp.multimethods.0)
        .values()
        .map(|m| (m.name.clone(), vec![m.dispatch_fn.clone(), m.default_val.clone(), Value::Map(m.methods.clone())]))
        .collect();
    multis.sort_by(|a, b| (&*a.0).cmp(&*b.0));
    for (_, vs) in multis {
        vs.iter().for_each(|v| w.val(v))
    }
    PreIndex { ptr2id: w.ptr2id, objs: w.objs, checksum: w.h }
}

// ------------------------------------------------------------ writer
struct W<'a> {
    o: Vec<u8>,
    pre: &'a PreIndex,
    seen: HashMap<usize, u32>,
    objs: Vec<Obj>,
    deferred: Vec<u32>,
    unsupported: BTreeMap<String, u64>,
    census: BTreeMap<&'static str, u64>,
    kw: HashMap<String, u32>,
    /// Encoded fn bodies (see `LazyBody`), appended after `END!`.
    blob: Vec<u8>,
    /// S3: native code records (`jit::aot`), appended after `blob`.
    code: Vec<u8>,
    /// While a body is written into `blob`: its table refs, remapped to a
    /// dense per-body extern list (global id -> local index, local -> global).
    body_ext: Option<(HashMap<u32, u32>, Vec<u32>)>,
    /// Inside a lazy body (prelude or body): strings go inline, never registered.
    in_body: bool,
    /// Persisted IR: shared CompiledFn / FnTemplate ids (see `image_ir`).
    cfn_seen: HashMap<usize, u32>,
    tpl_seen: HashMap<usize, u32>,
    /// S4: lazy-IR prelude pass: IR values/arities collected, not written.
    collect: Option<Vec<Pre>>,
}

/// S4: a table object an IR unit refs; registered eagerly in the main stream.
enum Pre {
    V(Value),
    Ar(Arc<Vec<Arity>>),
}

// value tags
const T_NIL: u8 = 0;
const T_FALSE: u8 = 1;
const T_TRUE: u8 = 2;
const T_INT: u8 = 3;
const T_FLOAT: u8 = 4;
const T_CHAR: u8 = 5;
const T_STR: u8 = 6;
const T_SYM: u8 = 7;
const T_KW: u8 = 8;
const T_LIST: u8 = 9;
const T_VEC: u8 = 10;
const T_MAPENTRY: u8 = 11;
const T_QUEUE: u8 = 12;
const T_MAP: u8 = 13;
const T_SET: u8 = 14;
const T_FN: u8 = 15;
const T_MACRO: u8 = 16;
const T_MULTI: u8 = 17;
const T_VAR: u8 = 18;
const T_ATOM: u8 = 19;
const T_VOLATILE: u8 = 20;
const T_LAZY: u8 = 21;
const T_LAZYTAIL: u8 = 22;
const T_DELAY: u8 = 23;
const T_REGEX: u8 = 24;
const T_CLASS_USER: u8 = 25;
const T_CLASS_IFACE: u8 = 26;
const T_INST: u8 = 27;
const T_HOST_OBJECT: u8 = 28;
const T_HOST_TL: u8 = 29;
const T_META: u8 = 30;
const T_UUID: u8 = 31;
const T_REF: u8 = 40;
const T_ENV_ROOT: u8 = 41;
const T_ENV_FRAME: u8 = 42;
const T_ARITIES: u8 = 43;
const T_TDEF: u8 = 44;
const T_UNSUP: u8 = 45;
const T_PVEC: u8 = 46;
const T_PMAP_SMALL: u8 = 47;
const T_PMAP_BIG: u8 = 48;
const REGF: u8 = 0x80; // "register this object in the table"

impl<'a> W<'a> {
    fn u(&mut self, mut v: u64) {
        loop {
            let b = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                self.o.push(b);
                return;
            }
            self.o.push(b | 0x80);
        }
    }
    fn b(&mut self, x: u8) {
        self.o.push(x)
    }
    fn raw(&mut self, s: &str) {
        self.u(s.len() as u64);
        self.o.extend_from_slice(s.as_bytes());
    }
    fn id_of(&self, p: usize) -> Option<u32> {
        self.pre.ptr2id.get(&p).copied().or_else(|| self.seen.get(&p).copied())
    }
    fn assign(&mut self, p: usize, o: Obj) -> u32 {
        let id = (self.pre.objs.len() + self.objs.len()) as u32;
        self.seen.insert(p, id);
        self.objs.push(o);
        id
    }
    fn emit_ref(&mut self, id: u32) {
        let id = match &mut self.body_ext {
            None => id,
            Some((m, v)) => *m.entry(id).or_insert_with(|| {
                v.push(id);
                (v.len() - 1) as u32
            }),
        };
        self.b(T_REF);
        self.u(id as u64);
    }
    fn try_ref(&mut self, p: usize) -> bool {
        if let Some(id) = self.id_of(p) {
            self.emit_ref(id);
            true
        } else {
            false
        }
    }
    fn str_(&mut self, s: &Str) {
        let p = s.ptr_addr();
        if self.try_ref(p) {
            return;
        }
        // Registering a body string would make it an eager table object kept alive by every LazyBody.ext.
        let shared = !self.in_body && s.strong_count() > 1;
        self.b(T_STR | if shared { REGF } else { 0 });
        self.raw(s);
        if shared {
            self.assign(p, Obj::S(s.clone()));
        }
    }
    fn sym(&mut self, s: &Symbol) {
        match &s.ns {
            Some(ns) => {
                self.b(1);
                self.str_(ns)
            }
            None => self.b(0),
        }
        self.str_(&s.name)
    }
    fn pvec(&mut self, p: &PVec) {
        let (ptr, cnt) = match p {
            PVec::Small(a) => (Arc::as_ptr(a) as *const u8 as usize, Arc::strong_count(a)),
            PVec::Big(a) => (Arc::as_ptr(a) as usize, Arc::strong_count(a)),
            PVec::Col(a) => (Arc::as_ptr(a) as usize, Arc::strong_count(a)),
        };
        if self.try_ref(ptr) {
            return;
        }
        let shared = cnt > 1;
        self.b(T_PVEC | if shared { REGF } else { 0 });
        let big = match p {
            PVec::Col(_) => p.len() > crate::value::PVEC_SMALL_MAX,
            _ => matches!(p, PVec::Big(_)),
        };
        self.b(big as u8);
        self.u(p.len() as u64);
        // M6: a packed vector is written as a plain vector (elements built on the fly, not cached)
        for x in p.clone() {
            self.val(&x)
        }
        if shared {
            self.assign(ptr, Obj::Vec(p.clone()));
        }
    }
    fn pmap(&mut self, m: &PMap) {
        let (ptr, cnt, big) = match m {
            PMap::Small(a) => (Arc::as_ptr(a) as usize, Arc::strong_count(a), false),
            PMap::Big(a) => (Arc::as_ptr(a) as usize, Arc::strong_count(a), true),
            PMap::Shaped(a) => (a.ptr(), a.strong_count(), true),
        };
        if self.try_ref(ptr) {
            return;
        }
        let shared = cnt > 1;
        self.b(if big { T_PMAP_BIG } else { T_PMAP_SMALL } | if shared { REGF } else { 0 });
        self.u(m.len() as u64);
        for (k, x) in m.iter() {
            self.val(k);
            self.val(x)
        }
        if shared {
            self.assign(ptr, Obj::Map(m.clone()));
        }
    }
    fn unsup(&mut self, what: String) {
        *self.unsupported.entry(what).or_default() += 1;
        self.b(T_UNSUP);
    }
    fn val(&mut self, v: &Value) {
        if let Some(p) = vptr(v) {
            if self.try_ref(p) {
                return;
            }
            // shells: register now, fill via FIX later
            if is_mutable_cell(v) {
                let tag = match v {
                    Value::Var(_) => T_VAR,
                    Value::Atom(_) => T_ATOM,
                    Value::Volatile(_) => T_VOLATILE,
                    Value::Lazy(_) => T_LAZY,
                    Value::LazyTail(_) => T_LAZYTAIL,
                    _ => T_DELAY,
                };
                self.b(tag | REGF);
                if let Value::Var(c) = v {
                    let n = c.name.clone();
                    self.sym(&n);
                }
                let id = self.assign(p, Obj::V(v.clone()));
                self.deferred.push(id);
                *self.census.entry("mutable-cell").or_default() += 1;
                return;
            }
        }
        match v {
            Value::Nil => self.b(T_NIL),
            Value::Bool(b) => self.b(if *b { T_TRUE } else { T_FALSE }),
            Value::Int(i) => {
                self.b(T_INT);
                self.u(((*i << 1) ^ (*i >> 63)) as u64)
            }
            Value::Float(f) => {
                self.b(T_FLOAT);
                self.o.extend_from_slice(&f.to_le_bytes())
            }
            Value::Char(c) => {
                self.b(T_CHAR);
                self.u(*c as u64)
            }
            Value::Str(s) => self.str_(s),
            Value::Sym(s) => {
                self.b(T_SYM);
                self.sym(s)
            }
            Value::Keyword(k) => {
                if let Some(id) = self.kw.get(&**k) {
                    let id = *id;
                    self.emit_ref(id);
                    return;
                }
                self.b(T_KW | REGF);
                self.raw(k);
                let id = (self.pre.objs.len() + self.objs.len()) as u32;
                self.objs.push(Obj::V(v.clone()));
                self.kw.insert(k.to_string(), id);
            }
            Value::List(p) | Value::Vector(p) | Value::MapEntry(p) | Value::Queue(p) => {
                self.b(match v {
                    Value::List(_) => T_LIST,
                    Value::Vector(_) => T_VEC,
                    Value::MapEntry(_) => T_MAPENTRY,
                    _ => T_QUEUE,
                });
                self.pvec(p)
            }
            Value::Map(m) => {
                self.b(T_MAP);
                self.pmap(m)
            }
            Value::Set(s) => {
                self.b(T_SET);
                self.u(s.len() as u64);
                for x in s.iter() {
                    self.val(x)
                }
            }
            Value::Meta(m) => {
                self.b(T_META);
                self.val(&m.meta);
                self.val(&m.inner)
            }
            Value::Uuid(u) => {
                self.b(T_UUID);
                self.o.extend_from_slice(&u.to_le_bytes())
            }
            Value::Fn(c) | Value::Macro(c) => {
                *self.census.entry("closure").or_default() += 1;
                if c.compiled.compiled().is_some() {
                    *self.census.entry("closure:ir-at-write").or_default() += 1;
                }
                if c.native_macro.is_some() {
                    return self.unsup("new closure with native_macro".into());
                }
                self.b(if matches!(v, Value::Fn(_)) { T_FN } else { T_MACRO } | REGF);
                match &c.name {
                    Some(n) => {
                        self.b(1);
                        self.str_(n)
                    }
                    None => self.b(0),
                }
                self.arities(&c.arities);
                // A MakeClosure-made closure keeps its free locals in the
                // compiled tier's capture vector, not in `env`: rebuild them
                // as a synthetic frame so the (pending) tree-walker sees them.
                match c.compiled.compiled() {
                    Some(cc) if !cc.captures.is_empty() || cc.group.is_some() => {
                        if cc.group.is_some() || cc.code.capture_syms.len() != cc.captures.len() {
                            return self.unsup("compiled closure with rec group / unnamed captures".into());
                        }
                        *self.census.entry("closure:compiled-captures").or_default() += 1;
                        let fr = Env::img_new_frame(c.env.clone());
                        fr.img_fill_frame(cc.code.capture_syms.iter().cloned().zip(cc.captures.iter().cloned()).collect(), c.env.clone());
                        self.env(&fr);
                    }
                    _ => self.env(&c.env),
                }
                self.str_(&c.ns);
                self.u(c.def_span.start as u64);
                self.u(c.def_span.end as u64);
                self.u(c.def_source_id.get() as u64);
                self.b(c.unchecked_math as u8);
                // persisted IR: restored fn comes back compiled (T_FN only)
                match c.compiled.compiled() {
                    Some(cc) if matches!(v, Value::Fn(_)) && !image_ir::disabled() && image_ir::persistable(cc) => {
                        *self.census.entry("closure:ir-persisted").or_default() += 1;
                        self.b(2);
                        self.lazy_cfn(&cc.code)
                    }
                    _ => self.b(0),
                }
                self.assign(vptr(v).unwrap(), Obj::V(v.clone()));
            }
            Value::Native(n) => {
                // the only load-time native kind carried so far: defmulti's
                // dispatch fn (its state lives in the multimethod registry)
                let key = Arc::as_ptr(n) as *const u8 as usize;
                let _ = key;
                let is_multi = crate::multi::multi_key(v)
                    .is_some_and(|k| MULTI_KEYS.with(|m| m.borrow().contains(&k)));
                if let Some(rc) = &n.image_recipe {
                    *self.census.entry("native:recipe").or_default() += 1;
                    self.b(T_RECIPE | REGF);
                    match &**rc {
                        Recipe::Proto { mname, var_display, iface, pname, arity, cell, epoch, midx } => {
                            self.b(0);
                            self.str_(mname);
                            self.raw(var_display);
                            self.str_(iface);
                            self.str_(pname);
                            self.u(arity.0 as u64);
                            self.u(arity.1 as u64);
                            self.val(&Value::Var(cell.clone()));
                            self.u(*epoch);
                            self.u(*midx as u64);
                        }
                        Recipe::Ctor { tdef, name } | Recipe::MapCtor { tdef, name } => {
                            self.b(if matches!(&**rc, Recipe::Ctor { .. }) { 1 } else { 3 });
                            self.tdef(tdef);
                            self.raw(name);
                        }
                        Recipe::Basis { tdef } => {
                            self.b(2);
                            self.tdef(tdef);
                        }
                    }
                    self.assign(vptr(v).unwrap(), Obj::V(v.clone()));
                } else if is_multi {
                    *self.census.entry("native:multi").or_default() += 1;
                    self.b(T_MULTI | REGF);
                    self.raw(&n.name);
                    self.assign(vptr(v).unwrap(), Obj::V(v.clone()));
                } else {
                    self.unsup(format!("native {}", n.name))
                }
            }
            Value::Regex(r) => {
                self.b(T_REGEX | REGF);
                self.raw(r.as_str());
                self.assign(vptr(v).unwrap(), Obj::V(v.clone()));
            }
            Value::Class(c) => match &**c {
                ClassVal::User(t) => {
                    self.b(T_CLASS_USER | REGF);
                    self.tdef(t);
                    self.assign(vptr(v).unwrap(), Obj::V(v.clone()));
                }
                ClassVal::Interface { name } => {
                    self.b(T_CLASS_IFACE | REGF);
                    self.str_(name);
                    self.assign(vptr(v).unwrap(), Obj::V(v.clone()));
                }
                ClassVal::Builtin { name, .. } => {
                    self.b(T_CLASS_BUILTIN);
                    self.raw(name)
                }
            },
            Value::Inst(_) if crate::ns::ns_value_name(v).is_some() => {
                let n = crate::ns::ns_value_name(v).unwrap();
                self.b(T_NS);
                self.str_(&n)
            }
            Value::Inst(i) => {
                *self.census.entry("inst").or_default() += 1;
                self.b(T_INST | REGF);
                self.tdef(&i.tdef);
                self.pmap(&i.data);
                let f = crate::sync::lock_mutex(&i.fields).clone();
                self.pvec(&f);
                match &i.meta {
                    Some(m) => {
                        self.b(1);
                        self.pmap(m)
                    }
                    None => self.b(0),
                }
                self.assign(vptr(v).unwrap(), Obj::V(v.clone()));
            }
            Value::HostInst(h) => {
                let st = crate::sync::lock_mutex(&h.state);
                match &*st {
                    crate::hostclass::HostState::Object => {
                        drop(st);
                        self.b(T_HOST_OBJECT | REGF);
                        self.assign(vptr(v).unwrap(), Obj::V(v.clone()));
                    }
                    crate::hostclass::HostState::ThreadLocal(tl) => {
                        let (a, b) = (tl.value.clone(), tl.init_fn.clone());
                        drop(st);
                        self.b(T_HOST_TL | REGF);
                        self.opt(&a);
                        self.opt(&b);
                        self.assign(vptr(v).unwrap(), Obj::V(v.clone()));
                    }
                    _ => {
                        drop(st);
                        self.unsup(format!("hostinst {:?}", h.kind))
                    }
                }
            }
            Value::SortedMap(sm) => {
                self.b(T_SORTEDMAP);
                match &sm.cmp {
                    Comparator::Default => self.b(0),
                    Comparator::Fn(f) => {
                        self.b(1);
                        self.val(f)
                    }
                }
                self.u(sm.entries.len() as u64);
                for (k, x) in &sm.entries {
                    self.val(k);
                    self.val(x)
                }
            }
            Value::BigInt(_) | Value::Ratio(_) | Value::BigDec(_) => {
                self.b(T_NUMTXT);
                self.raw(&crate::printer::pr_str(v))
            }
            other => self.unsup(format!("value {} {:?}", crate::printer::pr_str(other).chars().take(30).collect::<String>(), std::mem::discriminant(other))),
        }
    }
    fn opt(&mut self, v: &Option<Value>) {
        match v {
            Some(x) => {
                self.b(1);
                self.val(x)
            }
            None => self.b(0),
        }
    }
    fn tdef(&mut self, t: &Arc<TypeDef>) {
        let p = Arc::as_ptr(t) as usize;
        if self.try_ref(p) {
            return;
        }
        *self.census.entry("typedef").or_default() += 1;
        self.b(T_TDEF | REGF);
        self.str_(&t.name);
        self.u(t.basis.len() as u64);
        for s in &t.basis {
            self.str_(s)
        }
        self.b(t.is_record as u8);
        self.u(t.interfaces.len() as u64);
        for s in &t.interfaces {
            self.str_(s)
        }
        self.u(t.field_tags.len() as u64);
        for x in &t.field_tags {
            self.val(x)
        }
        self.u(t.mutable.len() as u64);
        for m in &t.mutable {
            self.b(*m as u8)
        }
        self.u(t.methods.len() as u64);
        for (k, x) in &t.methods {
            self.str_(k);
            self.val(x)
        }
        // protocol ids are VarCell pointers
        let ps = t.protocols.clone();
        self.u(ps.len() as u64);
        for p in ps {
            match self.id_of(p) {
                Some(id) => self.u(id as u64 + 1),
                None => {
                    *self.unsupported.entry("tdef protocol id not yet written".into()).or_default() += 1;
                    self.u(0)
                }
            }
        }
        self.assign(p, Obj::Td(t.clone()));
    }
    fn arities(&mut self, a: &Arc<Vec<Arity>>) {
        let p = Arc::as_ptr(a) as usize;
        if self.try_ref(p) {
            return;
        }
        self.b(T_ARITIES | REGF);
        self.u(a.len() as u64);
        for ar in a.iter() {
            self.u(ar.params.len() as u64);
            for s in &ar.params {
                self.sym(s)
            }
            match &ar.rest {
                Some(s) => {
                    self.b(1);
                    self.sym(s)
                }
                None => self.b(0),
            }
            match &ar.coerce {
                Some(c) => {
                    self.b(1);
                    self.u(c.len() as u64);
                    for x in c.iter() {
                        self.b(match x {
                            None => 0,
                            Some(PrimCast::Long) => 1,
                            Some(PrimCast::Double) => 2,
                        })
                    }
                }
                None => self.b(0),
            }
            self.lazy_body(&ar.body);
        }
        self.assign(p, Obj::Ar(a.clone()));
    }
    /// Body forms go to `blob`, decoded on first call (`LazyBody`). The body
    /// must not register table objects (a lazy decode can't replay them in
    /// order), so every atom that WOULD register is first written in the
    /// main stream as a `1 <val>` prelude record; the body then only refs.
    fn lazy_body(&mut self, body: &[Form]) {
        fn atoms<'f>(f: &'f Form, out: &mut Vec<&'f Value>) {
            if let Some(m) = &f.meta {
                atoms(m, out)
            }
            match &f.value {
                FormValue::Atom(v) => out.push(v),
                FormValue::List(v) | FormValue::Vector(v) | FormValue::Set(v) => v.iter().for_each(|x| atoms(x, out)),
                FormValue::Map(v) => v.iter().for_each(|(k, x)| {
                    atoms(k, out);
                    atoms(x, out)
                }),
            }
        }
        let mut av = Vec::new();
        body.iter().for_each(|f| atoms(f, &mut av));
        let outer = std::mem::replace(&mut self.in_body, true);
        for v in av {
            let mark = self.o.len();
            let n = self.objs.len();
            self.b(1);
            self.val(v);
            if self.objs.len() == n {
                self.o.truncate(mark)
            }
        }
        self.b(0);
        let main = std::mem::replace(&mut self.o, std::mem::take(&mut self.blob));
        let off = self.o.len();
        let n = self.objs.len();
        self.body_ext = Some((HashMap::new(), Vec::new()));
        body.iter().for_each(|f| self.form(f));
        let (_, ext) = self.body_ext.take().unwrap();
        self.in_body = outer;
        self.blob = std::mem::replace(&mut self.o, main);
        if self.objs.len() != n {
            self.unsup("lazy body registers an object".into())
        }
        self.u(body.len() as u64);
        self.u(off as u64);
        self.u(ext.len() as u64);
        ext.into_iter().for_each(|id| self.u(id as u64));
    }
    /// S4: IR goes to `blob`, decoded on first call (`LazyIr`). Same rule
    /// as `lazy_body`: table objects it refs are registered first in a
    /// main-stream prelude (`1 <val>` / `2 <arities>`); the unit only refs.
    fn lazy_cfn(&mut self, f: &Arc<crate::compile::CompiledFn>) {
        let outer = std::mem::replace(&mut self.in_body, true);
        let (mo, mc, census) = (self.o.len(), self.code.len(), self.census.clone());
        self.cfn_seen.clear();
        self.tpl_seen.clear();
        self.collect = Some(Vec::new());
        self.cfn(f);
        let items = self.collect.take().unwrap();
        self.o.truncate(mo);
        self.code.truncate(mc);
        self.census = census;
        for it in &items {
            let mark = self.o.len();
            let n = self.objs.len();
            match it {
                Pre::V(v) => {
                    self.b(1);
                    self.val(v)
                }
                Pre::Ar(a) => {
                    self.b(2);
                    self.arities(a)
                }
            }
            if self.objs.len() == n {
                self.o.truncate(mark)
            }
        }
        self.b(0);
        self.cfn_seen.clear();
        self.tpl_seen.clear();
        let main = std::mem::replace(&mut self.o, std::mem::take(&mut self.blob));
        let off = self.o.len();
        let n = self.objs.len();
        let outer_ext = self.body_ext.replace((HashMap::new(), Vec::new()));
        self.cfn(f);
        let (_, ext) = std::mem::replace(&mut self.body_ext, outer_ext).unwrap();
        self.blob = std::mem::replace(&mut self.o, main);
        self.in_body = outer;
        self.cfn_seen.clear();
        self.tpl_seen.clear();
        if self.objs.len() != n {
            self.unsup("lazy IR registers an object".into())
        }
        self.u(off as u64);
        self.u(ext.len() as u64);
        ext.into_iter().for_each(|id| self.u(id as u64));
    }
    fn form(&mut self, f: &Form) {
        *self.census.entry("form-node").or_default() += 1;
        let (tag, n) = match &f.value {
            FormValue::Atom(_) => (0u8, 0),
            FormValue::List(v) => (1, v.len()),
            FormValue::Vector(v) => (2, v.len()),
            FormValue::Map(v) => (3, v.len()),
            FormValue::Set(v) => (4, v.len()),
        };
        self.b(tag | if f.meta.is_some() { 0x80 } else { 0 });
        self.u(f.span.start as u64);
        self.u(f.span.end as u64);
        if let Some(m) = &f.meta {
            self.form(m)
        }
        match &f.value {
            FormValue::Atom(v) => self.val(v),
            FormValue::List(v) | FormValue::Vector(v) | FormValue::Set(v) => {
                self.u(n as u64);
                v.iter().for_each(|x| self.form(x))
            }
            FormValue::Map(v) => {
                self.u(n as u64);
                v.iter().for_each(|(k, x)| {
                    self.form(k);
                    self.form(x)
                })
            }
        }
    }
    fn env(&mut self, e: &Env) {
        if e.img_is_root() {
            return self.b(T_ENV_ROOT);
        }
        if self.try_ref(e.img_ptr()) {
            return;
        }
        self.b(T_ENV_FRAME | REGF);
        let id = self.assign(e.img_ptr(), Obj::Env(e.clone()));
        self.deferred.push(id);
    }
    /// FIX record for mutable object `id`.
    fn fix(&mut self, id: u32) {
        let o = if (id as usize) < self.pre.objs.len() {
            self.pre.objs[id as usize].clone()
        } else {
            self.objs[id as usize - self.pre.objs.len()].clone()
        };
        self.u(id as u64 + 1);
        match o {
            Obj::Env(e) => {
                let (vars, parent) = e.img_frame().expect("frame");
                self.env(&parent);
                self.u(vars.len() as u64);
                for (k, x) in vars {
                    self.sym(&k);
                    self.val(&x)
                }
            }
            Obj::V(Value::Var(c)) => {
                let x = c.raw_root();
                self.opt(&x);
                let (a, b, s) = c.img_flags();
                self.b(a as u8 | (b as u8) << 1 | (s as u8) << 2);
                self.val(&c.img_meta());
            }
            Obj::V(Value::Atom(a)) => {
                let (ver, x) = crate::sync::lock_mutex(&a.state).clone();
                self.u(ver);
                self.val(&x);
                let m = crate::sync::lock_read(&a.meta).clone();
                self.val(&m);
                let ws = crate::sync::lock_mutex(&a.watches).clone();
                self.u(ws.len() as u64);
                for (k, f) in ws {
                    self.val(&k);
                    self.val(&f)
                }
            }
            Obj::V(Value::Volatile(a)) => {
                let x = crate::sync::lock_read(&a).clone();
                self.val(&x)
            }
            Obj::V(Value::Lazy(l)) | Obj::V(Value::LazyTail(l)) => {
                let t = crate::sync::lock_mutex(&l.thunk).clone();
                let r = crate::sync::lock_mutex(&l.realized).clone();
                self.opt(&t);
                self.opt(&r);
            }
            Obj::V(Value::Delay(d)) => {
                let f = crate::sync::lock_mutex(&d.f).clone();
                self.opt(&f);
                match d.result.get() {
                    None => self.b(0),
                    Some(Ok(x)) => {
                        let x = x.clone();
                        self.b(1);
                        self.val(&x)
                    }
                    Some(Err(_)) => {
                        self.b(0);
                        *self.unsupported.entry("delay realized with error".into()).or_default() += 1
                    }
                }
            }
            // S3: boot fn hot at write time: its IR + native code
            Obj::V(Value::Fn(c)) => self.lazy_cfn(&c.compiled.compiled().expect("pre fn with code").code),
            _ => unreachable!("fix on non-mutable"),
        }
    }
}

thread_local! {
    static MULTI_KEYS: std::cell::RefCell<std::collections::HashSet<usize>> = Default::default();
}

pub struct WriteReport {
    pub bytes: usize,
    pub objects: usize,
    pub unsupported: BTreeMap<String, u64>,
    pub census: BTreeMap<&'static str, u64>,
}

/// Writes the delta image of `interp` (booted, pre-indexed with `pre`,
/// then loaded) to `path`. `header` is the caller's validity key.
pub fn write_image(interp: &Interp, pre: &PreIndex, header: &str, src_base: usize, path: &std::path::Path) -> Result<WriteReport, String> {
    let (bytes, rep) = encode_image(interp, pre, header, src_base)?;
    if rep.unsupported.is_empty() {
        write_atomic(path, &bytes).map_err(|e| e.to_string())?;
    }
    Ok(rep)
}

/// Fast non-cryptographic 64-bit content hash (4 independent multiply lanes,
/// 8 bytes per step). Corruption detection only, not adversarial. A single
/// changed word always changes its lane state (odd multiply is a bijection).
pub fn content_hash(b: &[u8]) -> u64 {
    const K: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut acc = [0x243F_6A88_85A3_08D3u64, 0x1319_8A2E_0370_7344, 0xA409_3822_299F_31D0, 0x082E_FA98_EC4E_6C89];
    let mut chunks = b.chunks_exact(32);
    for c in &mut chunks {
        for (l, a) in acc.iter_mut().enumerate() {
            let w = u64::from_le_bytes(c[l * 8..l * 8 + 8].try_into().unwrap());
            *a = (*a ^ w).wrapping_mul(K).rotate_left(29);
        }
    }
    let mut h = b.len() as u64;
    for a in acc {
        h = (h ^ a).wrapping_mul(K).rotate_left(31);
    }
    for &x in chunks.remainder() {
        h = (h ^ x as u64).wrapping_mul(K).rotate_left(7);
    }
    h ^ (h >> 32)
}

const FOOT_MAGIC: &[u8; 4] = b"MCHK";
/// Footer: magic(4) + total file length incl. footer (8) + content hash of everything before the footer (8).
const FOOT_LEN: usize = 20;

/// Checks the footer: exact length and content hash. Returns the image body.
fn verify_footer(all: &[u8]) -> Option<&[u8]> {
    if all.len() < 8 + 16 + FOOT_LEN {
        return None;
    }
    let (body, foot) = all.split_at(all.len() - FOOT_LEN);
    if &foot[..4] != FOOT_MAGIC || u64::from_le_bytes(foot[4..12].try_into().unwrap()) != all.len() as u64 {
        return None;
    }
    (u64::from_le_bytes(foot[12..20].try_into().unwrap()) == content_hash(body)).then_some(body)
}

/// Temp file in the same directory + rename: readers never see a partial
/// file, concurrent writers each use their own temp name (pid + counter).
pub fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = path.with_extension(format!("tmp{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
    let r = std::fs::write(&tmp, bytes).and_then(|_| std::fs::rename(&tmp, path));
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r
}

/// Encodes the image (with integrity footer) without touching the disk.
pub fn encode_image(interp: &Interp, pre: &PreIndex, header: &str, src_base: usize) -> Result<(Vec<u8>, WriteReport), String> {
    MULTI_KEYS.with(|m| *m.borrow_mut() = crate::sync::lock_read(&interp.multimethods.0).keys().copied().collect());
    let mut w = W {
        o: Vec::with_capacity(16 << 20),
        pre,
        seen: HashMap::new(),
        objs: Vec::new(),
        deferred: Vec::new(),
        unsupported: BTreeMap::new(),
        census: BTreeMap::new(),
        kw: HashMap::new(),
        blob: Vec::new(),
        code: Vec::new(),
        body_ext: None,
        in_body: false,
        cfn_seen: HashMap::new(),
        tpl_seen: HashMap::new(),
        collect: None,
    };
    w.o.extend_from_slice(MAGIC);
    w.raw(header);
    w.u(pre.objs.len() as u64);
    w.u(pre.checksum);
    let kw_at = w.o.len();
    // 1. root map
    let roots = interp.globals.img_root_entries();
    w.u(roots.len() as u64);
    for (sym, cell) in &roots {
        w.sym(sym);
        w.val(&Value::Var(cell.clone()));
    }
    // 2. every boot-time mutable cell gets its post-load content
    for (i, o) in pre.objs.iter().enumerate() {
        if matches!(o, Obj::V(v) if is_mutable_cell(v) || image_ir::pre_fn_with_code(v)) {
            w.deferred.push(i as u32);
        }
    }
    // 3. drain FIX records (they can discover more shells)
    w.b(1);
    while let Some(id) = w.deferred.pop() {
        w.fix(id);
    }
    w.u(0); // end of fixes
    // 4. registries
    let (nss, loaded) = crate::ns::img_dump(&interp.namespaces);
    w.u(nss.len() as u64);
    for (n, info) in &nss {
        w.str_(n);
        let mut al: Vec<_> = info.aliases.iter().collect();
        al.sort_by(|a, b| (&**a.0).cmp(&**b.0));
        w.u(al.len() as u64);
        for (a, t) in al {
            w.str_(a);
            w.str_(t)
        }
        let mut rf: Vec<_> = info.refers.iter().collect();
        rf.sort_by(|a, b| (&**a.0).cmp(&**b.0));
        w.u(rf.len() as u64);
        for (a, (x, y)) in rf {
            w.str_(a);
            w.str_(x);
            w.str_(y)
        }
    }
    w.u(loaded.len() as u64);
    for l in &loaded {
        w.str_(l)
    }
    // multimethods: key = the dispatch native's object
    let multis: Vec<(usize, crate::multi::MultiDef)> =
        crate::sync::lock_read(&interp.multimethods.0).iter().map(|(k, m)| (*k, m.clone_for_image())).collect();
    w.u(multis.len() as u64);
    for (k, m) in &multis {
        match w.id_of(*k) {
            Some(id) => w.u(id as u64 + 1),
            None => {
                *w.unsupported.entry("multimethod key not reachable".into()).or_default() += 1;
                w.u(0)
            }
        }
        w.str_(&m.name);
        w.val(&m.dispatch_fn);
        w.val(&m.default_val);
        w.opt(&m.hierarchy_ref);
        w.pmap(&m.methods);
        w.pmap(&m.prefers);
    }
    // protocols: key = the protocol's VarCell
    let protos: Vec<(usize, Str, Vec<(ClassKey, Value, crate::types::MethodTable)>, Vec<Str>, usize, u64)> = crate::sync::lock_read(&interp.protocols.0)
        .iter()
        .map(|(k, p)| {
            (
                *k,
                p.var_name.clone(),
                p.impls.iter().map(|(ck, (v, mt))| (ck.clone(), v.clone(), mt.clone())).collect(),
                p.declared_methods.iter().cloned().collect(),
                p.img_banks(),
                p.img_epoch(),
            )
        })
        .collect();
    w.u(protos.len() as u64);
    for (k, name, impls, decl, banks, epoch) in &protos {
        w.u(*banks as u64);
        w.u(*epoch);
        match w.id_of(*k) {
            Some(id) => w.u(id as u64 + 1),
            None => {
                *w.unsupported.entry("protocol key not reachable".into()).or_default() += 1;
                w.u(0)
            }
        }
        w.str_(name);
        w.u(decl.len() as u64);
        for d in decl {
            w.str_(d)
        }
        w.u(impls.len() as u64);
        for (ck, v, mt) in impls {
            w.class_key(ck);
            w.val(v);
            w.mtable(mt);
        }
    }
    let ifaces: Vec<((Str, usize), crate::types::MethodTable)> =
        crate::sync::lock_read(&interp.interfaces.0).iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    w.u(ifaces.len() as u64);
    for ((n, t), mt) in &ifaces {
        w.str_(n);
        match w.id_of(*t) {
            Some(id) => w.u(id as u64 + 1),
            None => {
                *w.unsupported.entry("interface tdef not reachable".into()).or_default() += 1;
                w.u(0)
            }
        }
        w.mtable(mt);
    }
    let im = crate::builtins::types::img_iface_methods();
    w.u(im.len() as u64);
    for (k, ms) in &im {
        w.str_(k);
        w.u(ms.len() as u64);
        for (m, n) in ms {
            w.str_(m);
            w.u(*n as u64)
        }
    }
    let marks = crate::builtins::types::img_inline_marks();
    w.u(marks.len() as u64);
    for (k, ck) in &marks {
        match w.id_of(*k) {
            Some(id) => w.u(id as u64 + 1),
            None => {
                *w.unsupported.entry("inline mark key not reachable".into()).or_default() += 1;
                w.u(0)
            }
        }
        w.class_key(ck);
    }
    let kws = interp.keywords.img_all();
    w.u(kws.len() as u64);
    for k in &kws {
        w.str_(k)
    }
    let mut ss: Vec<(Str, Str)> = interp.special_shadows.iter().cloned().collect();
    ss.sort_by(|a, b| (&*a.0, &*a.1).cmp(&(&*b.0, &*b.1)));
    let mut rx: Vec<(Str, Str)> = interp.refer_clojure_excludes.iter().cloned().collect();
    rx.sort_by(|a, b| (&*a.0, &*a.1).cmp(&(&*b.0, &*b.1)));
    for set in [ss, rx] {
        w.u(set.len() as u64);
        for (a, b) in set {
            w.str_(&a);
            w.str_(&b)
        }
    }
    w.str_(&interp.current_ns);
    w.u(crate::builtins::strings::GENSYM_COUNTER.load(Ordering::Relaxed));
    w.u(crate::eval::quasiquote::AUTOGENSYM_COUNTER.load(Ordering::Relaxed));
    let srcs = crate::source_registry::img_entries_from(src_base);
    w.u(src_base as u64);
    w.u(srcs.len() as u64);
    for (n, payload) in &srcs {
        w.raw(n);
        match payload {
            crate::source_registry::EntryPayload::Disk(hash) => {
                w.b(0);
                w.u(*hash);
            }
            crate::source_registry::EntryPayload::Mem(text) => {
                w.b(1);
                w.raw(text);
            }
        }
    }
    w.o.extend_from_slice(b"END!");
    // S4: every keyword text, spliced before the graph: bulk-interned first
    let mut kws: Vec<(String, u32)> = std::mem::take(&mut w.kw).into_iter().collect();
    kws.sort_by_key(|x| x.1);
    let main = std::mem::take(&mut w.o);
    w.u(kws.len() as u64);
    for (k, _) in kws {
        w.raw(&k)
    }
    let kwb = std::mem::replace(&mut w.o, main);
    w.o.splice(kw_at..kw_at, kwb);
    // lazy fn-body blob, then its start offset as the last 8 bytes
    let blob_base = w.o.len() as u64;
    let blob = std::mem::take(&mut w.blob);
    w.o.extend_from_slice(&blob);
    // S3: code section, then [code_base u64][blob_base u64]
    let code_base = w.o.len() as u64;
    let code = std::mem::take(&mut w.code);
    w.o.extend_from_slice(&code);
    w.o.extend_from_slice(&code_base.to_le_bytes());
    w.o.extend_from_slice(&blob_base.to_le_bytes());
    if std::env::var("MOVA_IMAGE_TRACE").is_ok_and(|v| v == "1") {
        let mut h: BTreeMap<String, u64> = BTreeMap::new();
        for o in &w.objs {
            let k = match o {
                Obj::V(v) => format!("V:{}", v.type_name()),
                Obj::Env(_) => "Env".into(),
                Obj::Ar(_) => "Ar".into(),
                Obj::Td(_) => "Td".into(),
                Obj::S(_) => "S".into(),
                Obj::Vec(_) => "Vec".into(),
                Obj::Map(_) => "Map".into(),
            };
            *h.entry(k).or_default() += 1;
        }
        eprintln!("[image] new-obj kinds {h:?}");
    }
    let rep = WriteReport {
        bytes: w.o.len(),
        objects: w.objs.len(),
        unsupported: std::mem::take(&mut w.unsupported),
        census: std::mem::take(&mut w.census),
    };
    let mut out = std::mem::take(&mut w.o);
    let total = (out.len() + FOOT_LEN) as u64;
    let h = content_hash(&out);
    out.extend_from_slice(FOOT_MAGIC);
    out.extend_from_slice(&total.to_le_bytes());
    out.extend_from_slice(&h.to_le_bytes());
    Ok((out, rep))
}

impl<'a> W<'a> {
    fn class_key(&mut self, ck: &ClassKey) {
        match ck {
            ClassKey::Nil => self.b(0),
            ClassKey::Builtin(n) => {
                self.b(1);
                self.raw(n)
            }
            ClassKey::User(p) => {
                self.b(2);
                match self.id_of(*p) {
                    Some(id) => self.u(id as u64 + 1),
                    None => {
                        *self.unsupported.entry("class key tdef not reachable".into()).or_default() += 1;
                        self.u(0)
                    }
                }
            }
            ClassKey::Interface(s) => {
                self.b(3);
                self.str_(s)
            }
            ClassKey::Object => self.b(4),
        }
    }
    fn mtable(&mut self, mt: &crate::types::MethodTable) {
        self.u(mt.len() as u64);
        for (k, x) in mt {
            self.str_(k);
            self.val(x)
        }
    }
}

// ------------------------------------------------------------ reader
struct R<'a> {
    b: &'a [u8],
    i: usize,
    tab: Vec<Obj>,
    /// The mapped image and the lazy-body blob's start offset.
    blob: Option<(Arc<Mapped>, usize)>,
    cfns: Vec<Arc<crate::compile::CompiledFn>>,
    tpls: Vec<Arc<crate::compile::ir::FnTemplate>>,
    /// S3: the image file and its code section's start offset.
    code: Option<(Arc<std::fs::File>, usize)>,
}

/// An image-restored fn body still in encoded form: `n` forms at absolute
/// byte `off` of the mapped image; its table refs index `ext`.
pub struct LazyBody {
    map: Arc<Mapped>,
    off: usize,
    n: usize,
    ext: Box<[Obj]>,
}

static BODIES_DECODED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
/// Set iff MOVA_IMAGE_TRACE=1: restore end time, for the decode trace.
static TRACE_T0: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
static BODIES_LAZY: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

extern "C" fn trace_bodies_at_exit() {
    eprintln!(
        "[image] at exit (+{:.1} ms after restore): lazy fn bodies decoded {} of {}; lazy IR {} of {}",
        TRACE_T0.get().map_or(0.0, |t| t.elapsed().as_secs_f64() * 1e3),
        BODIES_DECODED.load(Ordering::Relaxed),
        BODIES_LAZY.load(Ordering::Relaxed),
        IR_DECODED.load(Ordering::Relaxed),
        IR_LAZY.load(Ordering::Relaxed)
    );
}

/// S4: an image-restored fn's compiled IR still in encoded form (see
/// `W::lazy_cfn`); decoded into its `CompileSlot` on first use.
pub struct LazyIr {
    map: Arc<Mapped>,
    off: usize,
    ext: Box<[Obj]>,
    code: Option<(Arc<std::fs::File>, usize)>,
}

static IR_DECODED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static IR_LAZY: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

impl LazyIr {
    pub fn decode(&self) -> crate::compile::CompiledClosure {
        IR_DECODED.fetch_add(1, Ordering::Relaxed);
        let mut r = R { b: self.map.bytes_from(self.off), i: 0, tab: self.ext.to_vec(), blob: None, cfns: Vec::new(), tpls: Vec::new(), code: self.code.clone() };
        crate::compile::CompiledClosure { code: r.cfn(), captures: Vec::new(), group: None }
    }
}

impl LazyBody {
    pub fn decode(&self) -> Vec<Form> {
        let k = BODIES_DECODED.fetch_add(1, Ordering::Relaxed) + 1;
        if k % 64 == 0 {
            if let Some(t0) = TRACE_T0.get() {
                eprintln!("[image] lazy fn bodies decoded: {k} at +{:.0} ms", t0.elapsed().as_secs_f64() * 1e3);
            }
        }
        let mut r = R { b: self.map.bytes_from(self.off), i: 0, tab: self.ext.to_vec(), blob: None, cfns: Vec::new(), tpls: Vec::new(), code: None };
        (0..self.n).map(|_| r.form()).collect()
    }
}

impl<'a> R<'a> {
    fn u(&mut self) -> u64 {
        let mut v = 0u64;
        let mut sh = 0;
        loop {
            let x = self.b[self.i];
            self.i += 1;
            v |= ((x & 0x7f) as u64) << sh;
            if x & 0x80 == 0 {
                return v;
            }
            sh += 7;
        }
    }
    fn byte(&mut self) -> u8 {
        let x = self.b[self.i];
        self.i += 1;
        x
    }
    fn raw(&mut self) -> &'a str {
        let n = self.u() as usize;
        let s = unsafe { std::str::from_utf8_unchecked(&self.b[self.i..self.i + n]) };
        self.i += n;
        s
    }
    fn reg(&mut self, o: Obj) {
        self.tab.push(o)
    }
    fn get(&self, id: u64) -> &Obj {
        &self.tab[id as usize]
    }
    fn str_(&mut self) -> Str {
        let t = self.byte();
        match t & 0x7f {
            T_REF => {
                let id = self.u();
                match self.get(id) {
                    Obj::S(s) => s.clone(),
                    _ => panic!("image: str ref to non-str"),
                }
            }
            _ => {
                let s = Str::from(self.raw());
                if t & REGF != 0 {
                    self.reg(Obj::S(s.clone()))
                }
                s
            }
        }
    }
    fn sym(&mut self) -> Symbol {
        let ns = if self.byte() == 1 { Some(self.str_()) } else { None };
        Symbol { ns, name: self.str_() }
    }
    fn pvec(&mut self) -> PVec {
        let t = self.byte();
        if t == T_REF {
            let id = self.u();
            return match self.get(id) {
                Obj::Vec(p) => p.clone(),
                _ => panic!("image: pvec ref"),
            };
        }
        let big = self.byte() == 1;
        let n = self.u() as usize;
        let mut v = Vec::with_capacity(n);
        for _ in 0..n {
            v.push(self.val())
        }
        let p = if big { PVec::Big(Arc::new(champ::PVector::from_vec(v))) } else { PVec::Small(Arc::from(v)) };
        if t & REGF != 0 {
            self.reg(Obj::Vec(p.clone()))
        }
        p
    }
    fn pmap(&mut self) -> PMap {
        let t = self.byte();
        if t == T_REF {
            let id = self.u();
            return match self.get(id) {
                Obj::Map(p) => p.clone(),
                _ => panic!("image: pmap ref"),
            };
        }
        let n = self.u() as usize;
        let mut v = Vec::with_capacity(n);
        for _ in 0..n {
            let k = self.val();
            let x = self.val();
            v.push((k, x))
        }
        let m = if t & 0x7f == T_PMAP_BIG { PMap::Big(Arc::new(v.into_iter().collect())) } else { PMap::Small(Arc::new(v)) };
        if t & REGF != 0 {
            self.reg(Obj::Map(m.clone()))
        }
        m
    }
    fn opt(&mut self) -> Option<Value> {
        if self.byte() == 1 {
            Some(self.val())
        } else {
            None
        }
    }
    fn val(&mut self) -> Value {
        let t = self.byte();
        let v = match t & 0x7f {
            T_NIL => Value::Nil,
            T_FALSE => Value::Bool(false),
            T_TRUE => Value::Bool(true),
            T_INT => {
                let z = self.u();
                Value::Int(((z >> 1) as i64) ^ -((z & 1) as i64))
            }
            T_FLOAT => {
                let mut a = [0u8; 8];
                a.copy_from_slice(&self.b[self.i..self.i + 8]);
                self.i += 8;
                Value::Float(f64::from_le_bytes(a))
            }
            T_CHAR => Value::Char(char::from_u32(self.u() as u32).unwrap_or('?')),
            T_STR => {
                self.i -= 1;
                return Value::Str(self.str_());
            }
            T_SYM => Value::Sym(self.sym()),
            T_KW => Value::Keyword(crate::keyword::Keyword::construct(self.raw())),
            T_LIST => Value::List(self.pvec()),
            T_VEC => Value::Vector(self.pvec()),
            T_MAPENTRY => Value::MapEntry(self.pvec()),
            T_QUEUE => Value::Queue(self.pvec()),
            T_MAP => Value::Map(self.pmap()),
            T_SET => {
                let n = self.u() as usize;
                let mut v = Vec::with_capacity(n);
                for _ in 0..n {
                    v.push(self.val())
                }
                Value::Set(v.into_iter().collect())
            }
            T_META => {
                let meta = self.val();
                let inner = self.val();
                Value::Meta(Arc::new(MetaObj { meta, inner }))
            }
            T_UUID => {
                let mut a = [0u8; 16];
                a.copy_from_slice(&self.b[self.i..self.i + 16]);
                self.i += 16;
                Value::Uuid(Arc::new(u128::from_le_bytes(a)))
            }
            T_REF => {
                let id = self.u();
                return match self.get(id) {
                    Obj::V(v) => v.clone(),
                    Obj::S(s) => Value::Str(s.clone()),
                    o => panic!("image: value ref to non-value id {id} kind {} tab {} at byte {}", match o { Obj::Env(_) => "env", Obj::Ar(_) => "arities", Obj::Td(_) => "tdef", Obj::S(_) => "str", Obj::Vec(_) => "vec", Obj::Map(_) => "map", Obj::V(_) => "v" }, self.tab.len(), self.i),
                };
            }
            T_FN | T_MACRO => {
                let name = if self.byte() == 1 { Some(self.str_()) } else { None };
                let arities = self.arities();
                let env = self.env();
                let ns = self.str_();
                let def_span = Span { start: self.u() as usize, end: self.u() as usize };
                let def_source_id = self.u() as u32;
                let unchecked_math = self.byte() == 1;
                // Macro bodies are never compiled (see `defmacro`): the compiled tier has no `&form`/`&env`.
                let compiled = if self.byte() == 2 {
                    let s = CompileSlot::pending();
                    s.img_lazy(self.lazy_ir());
                    s
                } else if t & 0x7f == T_FN {
                    CompileSlot::pending()
                } else {
                    CompileSlot::settled(None, crate::lens::NO_SITE)
                };
                let c = Arc::new(Closure {
                    name,
                    arities,
                    env,
                    ns,
                    compiled,
                    def_span,
                    def_source_id: crate::source_registry::SrcRef::new(def_source_id),
                    unchecked_math,
                    native_macro: None,
                });
                if t & 0x7f == T_FN {
                    Value::Fn(c)
                } else {
                    Value::Macro(c)
                }
            }
            T_SORTEDMAP => {
                let cmp = if self.byte() == 1 { Comparator::Fn(self.val()) } else { Comparator::Default };
                let n = self.u() as usize;
                let mut entries = Vec::with_capacity(n);
                for _ in 0..n {
                    let k = self.val();
                    let x = self.val();
                    entries.push((k, x))
                }
                Value::SortedMap(Arc::new(SortedMapVal { cmp, entries }))
            }
            T_NUMTXT => {
                let t = self.raw();
                crate::reader::parse_number(t).unwrap_or_else(|_| panic!("image: number {t}"))
            }
            T_NS => {
                let n = self.str_();
                return crate::ns::ns_value(&n);
            }
            T_CLASS_BUILTIN => {
                let n = self.raw();
                return crate::builtins::types::img_builtin_class(n).unwrap_or_else(|| panic!("image: builtin class {n}"));
            }
            T_RECIPE => {
                let nf = match self.byte() {
                    0 => {
                        let mname = self.str_();
                        let var_display = self.raw().to_string();
                        let iface = self.str_();
                        let pname = self.str_();
                        let arity = (self.u() as usize, self.u() as usize);
                        let Value::Var(cell) = self.val() else { panic!("image: proto cell") };
                        let epoch = self.u();
                        let midx = self.u() as usize;
                        crate::eval::types_forms::proto_dispatch_native(mname, var_display, iface, pname, arity, cell, epoch, midx)
                    }
                    k @ (1 | 3) => {
                        let td = self.tdef();
                        let name = self.raw().to_string();
                        if k == 1 {
                            crate::eval::types_forms::record_ctor_native(td, name)
                        } else {
                            crate::eval::types_forms::map_ctor_native(td, name)
                        }
                    }
                    _ => {
                        let td = self.tdef();
                        crate::eval::types_forms::get_basis_native(td)
                    }
                };
                Value::Native(Arc::new(nf))
            }
            T_MULTI => {
                let name = Str::from(self.raw());
                let (arc, _key) = crate::multi::make_dispatch_native(name);
                Value::Native(arc)
            }
            T_VAR => {
                let s = self.sym();
                Value::Var(VarCell::unbound(s))
            }
            T_ATOM => Value::Atom(Arc::new(AtomCell::new(Value::Nil))),
            T_VOLATILE => Value::Volatile(Arc::new(RwLock::new(Value::Nil))),
            T_LAZY | T_LAZYTAIL => {
                let l = Arc::new(LazySeq { thunk: Mutex::new(None), realized: Mutex::new(None) });
                if t & 0x7f == T_LAZY {
                    Value::Lazy(l)
                } else {
                    Value::LazyTail(l)
                }
            }
            T_DELAY => Value::Delay(Arc::new(DelayCell { f: Mutex::new(None), result: std::sync::OnceLock::new() })),
            T_REGEX => {
                let s = self.raw();
                Value::Regex(Arc::new(crate::value::LazyRegex::lazy(s)))
            }
            T_CLASS_USER => {
                let td = self.tdef();
                Value::Class(Arc::new(ClassVal::User(td)))
            }
            T_CLASS_IFACE => {
                let name = self.str_();
                crate::builtins::types::interface_class(&name)
            }
            T_INST => {
                let tdef = self.tdef();
                let data = self.pmap();
                let fields = Mutex::new(self.pvec());
                let meta = if self.byte() == 1 { Some(self.pmap()) } else { None };
                Value::Inst(Arc::new(InstVal { tdef, data, fields, meta }))
            }
            T_HOST_OBJECT => Value::HostInst(Arc::new(crate::hostclass::HostInstVal {
                kind: crate::hostclass::HostKind::Object,
                state: Mutex::new(crate::hostclass::HostState::Object),
            })),
            T_HOST_TL => {
                let value = self.opt();
                let init_fn = self.opt();
                Value::HostInst(Arc::new(crate::hostclass::HostInstVal {
                    kind: crate::hostclass::HostKind::ThreadLocal,
                    state: Mutex::new(crate::hostclass::HostState::ThreadLocal(crate::hostclass::ThreadLocalState { value, init_fn })),
                }))
            }
            x => panic!("image: bad value tag {x} at {}", self.i),
        };
        if t & REGF != 0 {
            self.reg(Obj::V(v.clone()))
        }
        v
    }
    fn tdef(&mut self) -> Arc<TypeDef> {
        let t = self.byte();
        if t == T_REF {
            let id = self.u();
            return match self.get(id) {
                Obj::Td(x) => x.clone(),
                _ => panic!("image: tdef ref"),
            };
        }
        let name = self.str_();
        let basis = (0..self.u()).map(|_| self.str_()).collect();
        let is_record = self.byte() == 1;
        let interfaces = (0..self.u()).map(|_| self.str_()).collect();
        let field_tags = (0..self.u()).map(|_| self.val()).collect();
        let mutable = (0..self.u()).map(|_| self.byte() == 1).collect();
        let mut methods = crate::types::MethodTable::new();
        for _ in 0..self.u() {
            let k = self.str_();
            let x = self.val();
            methods.insert(k, x);
        }
        let protocols = (0..self.u())
            .map(|_| {
                let id = self.u();
                self.obj_ptr(id - 1)
            })
            .collect();
        let td = Arc::new(TypeDef { name, basis, is_record, interfaces, field_tags, mutable, methods, protocols });
        self.reg(Obj::Td(td.clone()));
        td
    }
    fn obj_ptr(&self, id: u64) -> usize {
        match self.get(id) {
            Obj::V(v) => vptr(v).unwrap_or(0),
            Obj::Td(t) => Arc::as_ptr(t) as usize,
            Obj::Env(e) => e.img_ptr(),
            _ => 0,
        }
    }
    fn arities(&mut self) -> Arc<Vec<Arity>> {
        let t = self.byte();
        if t == T_REF {
            let id = self.u();
            return match self.get(id) {
                Obj::Ar(x) => x.clone(),
                _ => panic!("image: arities ref"),
            };
        }
        let n = self.u() as usize;
        let mut v = Vec::with_capacity(n);
        for _ in 0..n {
            let params = (0..self.u()).map(|_| self.sym()).collect();
            let rest = if self.byte() == 1 { Some(self.sym()) } else { None };
            let coerce = if self.byte() == 1 {
                let k = self.u() as usize;
                Some(
                    (0..k)
                        .map(|_| match self.byte() {
                            1 => Some(PrimCast::Long),
                            2 => Some(PrimCast::Double),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .into_boxed_slice(),
                )
            } else {
                None
            };
            while self.byte() == 1 {
                self.val();
            }
            let n = self.u() as usize;
            let (map, base) = self.blob.clone().expect("image: lazy body outside restore");
            let off = base + self.u() as usize;
            let ext: Box<[Obj]> = (0..self.u()).map(|_| {
                let id = self.u();
                self.get(id).clone()
            }).collect();
            BODIES_LAZY.fetch_add(1, Ordering::Relaxed);
            let body = crate::value::Body::lazy(LazyBody { map, off, n, ext });
            v.push(Arity { params, rest, body, coerce });
        }
        let a = Arc::new(v);
        self.reg(Obj::Ar(a.clone()));
        a
    }
    fn form(&mut self) -> Form {
        let t = self.byte();
        let span = Span { start: self.u() as usize, end: self.u() as usize };
        let meta = if t & 0x80 != 0 { Some(Box::new(self.form())) } else { None };
        let value = match t & 0x7f {
            0 => FormValue::Atom(self.val()),
            3 => {
                let n = self.u() as usize;
                let mut v = Vec::with_capacity(n);
                for _ in 0..n {
                    let k = self.form();
                    let x = self.form();
                    v.push((k, x))
                }
                FormValue::Map(v)
            }
            tag => {
                let n = self.u() as usize;
                let mut v = Vec::with_capacity(n);
                for _ in 0..n {
                    v.push(self.form())
                }
                match tag {
                    1 => FormValue::List(v),
                    2 => FormValue::Vector(v),
                    _ => FormValue::Set(v),
                }
            }
        };
        Form { value, span, meta }
    }
    fn env(&mut self) -> Env {
        let t = self.byte();
        match t & 0x7f {
            T_ENV_ROOT => ROOT.with(|r| r.borrow().clone().expect("root")),
            T_REF => {
                let id = self.u();
                match self.get(id) {
                    Obj::Env(e) => e.clone(),
                    _ => panic!("image: env ref"),
                }
            }
            T_ENV_FRAME => {
                let root = ROOT.with(|r| r.borrow().clone().expect("root"));
                let e = Env::img_new_frame(root);
                self.reg(Obj::Env(e.clone()));
                e
            }
            x => panic!("image: bad env tag {x}"),
        }
    }
    fn fix(&mut self, id: u64) {
        let o = self.get(id).clone();
        match o {
            Obj::Env(e) => {
                let parent = self.env();
                let n = self.u() as usize;
                let mut vars = Vec::with_capacity(n);
                for _ in 0..n {
                    let k = self.sym();
                    let x = self.val();
                    vars.push((k, x))
                }
                e.img_fill_frame(vars, parent)
            }
            Obj::V(Value::Fn(c)) => {
                let l = self.lazy_ir();
                c.compiled.img_lazy(l);
            }
            Obj::V(Value::Var(c)) => {
                let x = self.opt();
                let fl = self.byte();
                let meta = self.val();
                c.img_fill(x, (fl & 1 != 0, fl & 2 != 0, fl & 4 != 0), meta)
            }
            Obj::V(Value::Atom(a)) => {
                let ver = self.u();
                let x = self.val();
                *crate::sync::lock_mutex(&a.state) = (ver, x);
                *crate::sync::lock_write(&a.meta) = self.val();
                let n = self.u() as usize;
                let mut ws = Vec::with_capacity(n);
                for _ in 0..n {
                    let k = self.val();
                    let f = self.val();
                    ws.push((k, f))
                }
                *crate::sync::lock_mutex(&a.watches) = ws;
            }
            Obj::V(Value::Volatile(a)) => *crate::sync::lock_write(&a) = self.val(),
            Obj::V(Value::Lazy(l)) | Obj::V(Value::LazyTail(l)) => {
                *crate::sync::lock_mutex(&l.thunk) = self.opt();
                *crate::sync::lock_mutex(&l.realized) = self.opt();
            }
            Obj::V(Value::Delay(d)) => {
                *crate::sync::lock_mutex(&d.f) = self.opt();
                if self.byte() == 1 {
                    let x = self.val();
                    let _ = d.result.set(Ok(x));
                }
            }
            _ => panic!("image: fix on non-mutable"),
        }
    }
}

thread_local! {
    static ROOT: std::cell::RefCell<Option<Env>> = const { std::cell::RefCell::new(None) };
}

/// Restores the image at `path` into `interp` (freshly booted, NOT yet
/// loaded). `Ok(false)` = image absent/stale (caller does a normal load).
pub fn restore(interp: &mut Interp, header: &str, path: &std::path::Path) -> Result<bool, String> {
    let map = match Mapped::open(path) {
        Some(m) => Arc::new(m),
        None => return Ok(false),
    };
    let Some(bytes) = verify_footer(map.bytes()) else { return Ok(false) };
    if &bytes[..8] != MAGIC {
        return Ok(false);
    }
    let blob_base = u64::from_le_bytes(bytes[bytes.len() - 8..].try_into().unwrap()) as usize;
    let code_base = u64::from_le_bytes(bytes[bytes.len() - 16..bytes.len() - 8].try_into().unwrap()) as usize;
    // S3: `MOVA_AOT=0` ignores the image's native code (A/B switch)
    let aot_on = crate::jit::enabled() && std::env::var("MOVA_AOT").map_or(true, |v| v != "0");
    let code = aot_on.then(|| std::fs::File::open(path).ok()).flatten().map(|f| (Arc::new(f), code_base));
    if code.is_some() && bytes.len() - 16 > code_base {
        crate::jit::note_image_code();
    }
    let mut r = R { b: bytes, i: 8, tab: Vec::new(), blob: Some((map.clone(), blob_base)), cfns: Vec::new(), tpls: Vec::new(), code };
    let h = r.raw();
    if h != header {
        if std::env::var("MOVA_IMAGE_TRACE").is_ok_and(|v| v == "1") {
            eprintln!("[image] header mismatch: {h} vs {header}");
        }
        return Ok(false);
    }
    if std::env::var("MOVA_IMAGE_TRACE").is_ok_and(|v| v == "1") {
        eprintln!("[image] sections: main {:.2} MB, blob {:.2} MB, code {:.2} MB; maxrss before restore {:.1} MB", blob_base as f64 / 1e6, (code_base - blob_base) as f64 / 1e6, (bytes.len() - 16 - code_base) as f64 / 1e6, crate::load_trace::maxrss_kb() as f64 * 1024.0 / 1e6);
    }
    let npre = r.u() as usize;
    let sum = r.u();
    let tp = std::time::Instant::now();
    let pre = pre_index(interp);
    let pre_ms = tp.elapsed().as_secs_f64() * 1e3;
    if pre.objs.len() != npre || pre.checksum != sum {
        if std::env::var("MOVA_IMAGE_TRACE").is_ok_and(|v| v == "1") {
            eprintln!("[image] pre-index mismatch: {} vs {npre}", pre.objs.len());
        }
        return Ok(false);
    }
    let kws: Vec<&str> = (0..r.u()).map(|_| r.raw()).collect();
    crate::keyword::intern_bulk(&kws);
    drop(kws);
    r.tab = pre.objs;
    r.tab.reserve(80_000);
    ROOT.with(|x| *x.borrow_mut() = Some(interp.globals.clone()));
    let n = r.u() as usize;
    let mut roots = Vec::with_capacity(n);
    for _ in 0..n {
        let s = r.sym();
        match r.val() {
            Value::Var(c) => roots.push((s, c)),
            _ => return Err("image: root entry not a var".into()),
        }
    }
    let _ = r.byte();
    loop {
        let id = r.u();
        if id == 0 {
            break;
        }
        r.fix(id - 1);
    }
    let t_graph = tp.elapsed().as_secs_f64() * 1e3;
    interp.globals.img_set_root(roots);
    let t_root = tp.elapsed().as_secs_f64() * 1e3;
    // namespaces
    let n = r.u() as usize;
    let mut nss = Vec::with_capacity(n);
    for _ in 0..n {
        let name = r.str_();
        let mut info = crate::ns::NsInfo::default();
        for _ in 0..r.u() {
            let a = r.str_();
            let t = r.str_();
            info.aliases.insert(a, t);
        }
        for _ in 0..r.u() {
            let a = r.str_();
            let x = r.str_();
            let y = r.str_();
            info.refers.insert(a, (x, y));
        }
        nss.push((name, info))
    }
    let loaded = (0..r.u()).map(|_| r.str_()).collect();
    crate::ns::img_load(&interp.namespaces, nss, loaded);
    // multimethods
    let mut multis = HashMap::new();
    for _ in 0..r.u() {
        let id = r.u();
        let key = r.obj_ptr(id - 1);
        let name = r.str_();
        let dispatch_fn = r.val();
        let default_val = r.val();
        let hierarchy_ref = r.opt();
        let methods = r.pmap();
        let prefers = r.pmap();
        multis.insert(key, crate::multi::MultiDef::for_image(name, dispatch_fn, default_val, hierarchy_ref, methods, prefers));
    }
    *crate::sync::lock_write(&interp.multimethods.0) = multis;
    // protocols
    let mut protos: crate::types::ProtoMap = Default::default();
    for _ in 0..r.u() {
        let banks = r.u() as usize;
        let epoch = r.u();
        let id = r.u();
        let key = r.obj_ptr(id - 1);
        let var_name = r.str_();
        let declared_methods = (0..r.u()).map(|_| r.str_()).collect();
        let mut impls = HashMap::new();
        for _ in 0..r.u() {
            let ck = r.class_key();
            let v = r.val();
            let mt = r.mtable();
            impls.insert(ck, (v, mt));
        }
        protos.insert(key, crate::types::ProtoDef::for_image(var_name, impls, declared_methods, banks, epoch));
    }
    *crate::sync::lock_write(&interp.protocols.0) = protos;
    crate::jit::PROTO_EPOCH.fetch_add(1, std::sync::atomic::Ordering::Release);
    let mut ifaces = HashMap::new();
    for _ in 0..r.u() {
        let n = r.str_();
        let id = r.u();
        let t = r.obj_ptr(id - 1);
        let mt = r.mtable();
        ifaces.insert((n, t), mt);
    }
    *crate::sync::lock_write(&interp.interfaces.0) = ifaces;
    for _ in 0..r.u() {
        let k = r.str_();
        let ms = (0..r.u()).map(|_| (r.str_(), r.u() as usize)).collect();
        crate::builtins::types::register_protocol_iface_methods(k, ms);
    }
    let mut marks = Vec::new();
    for _ in 0..r.u() {
        let id = r.u();
        let k = r.obj_ptr(id - 1);
        marks.push((k, r.class_key()));
    }
    crate::builtins::types::img_set_inline_marks(marks);
    for _ in 0..r.u() {
        let k = r.str_();
        interp.keywords.intern(&k);
    }
    for which in 0..2 {
        let n = r.u();
        let mut set = std::collections::HashSet::new();
        for _ in 0..n {
            let a = r.str_();
            let b = r.str_();
            set.insert((a, b));
        }
        if which == 0 {
            interp.special_shadows = set
        } else {
            interp.refer_clojure_excludes = set
        }
    }
    interp.current_ns = r.str_();
    crate::builtins::strings::GENSYM_COUNTER.fetch_max(r.u(), Ordering::Relaxed);
    crate::eval::quasiquote::AUTOGENSYM_COUNTER.fetch_max(r.u(), Ordering::Relaxed);
    let t_reg = tp.elapsed().as_secs_f64() * 1e3;
    let base = r.u() as usize;
    // P0b: tolerate an already-populated (process-global) registry: entries
    // that an earlier restore/boot of the same image already pushed at the
    // same ids (same name) are skipped, not re-pushed.
    if crate::source_registry::img_len() < base {
        return Err(format!("image: source registry base {} < {}", crate::source_registry::img_len(), base));
    }
    let mut idx = base;
    for _ in 0..r.u() {
        let n = Str::from(r.raw());
        let tag = r.byte();
        let payload = if tag == 0 {
            crate::source_registry::EntryPayload::Disk(r.u())
        } else {
            crate::source_registry::EntryPayload::Mem(Str::from(r.raw()))
        };
        if idx < crate::source_registry::img_len() {
            if crate::source_registry::img_name_at(idx).as_deref() != Some(n.as_ref()) {
                return Err(format!("image: source registry id {idx} name mismatch"));
            }
        } else {
            crate::source_registry::img_push(n, payload);
        }
        idx += 1;
    }
    if &r.b[r.i..r.i + 4] != b"END!" {
        return Err("image: trailer mismatch".into());
    }
    crate::multi::bump_multi_generation();
    ROOT.with(|x| *x.borrow_mut() = None);
    if std::env::var("MOVA_IMAGE_TRACE").is_ok_and(|v| v == "1") {
        unsafe { libc::atexit(trace_bodies_at_exit) };
        let _ = TRACE_T0.set(std::time::Instant::now());
        eprintln!("[image] cumulative: graph {t_graph:.1} root-map {t_root:.1} registries {t_reg:.1} sources {:.1} ms", tp.elapsed().as_secs_f64() * 1e3);
        eprintln!("[image] pre-index {pre_ms:.1} ms, table {} objs, maxrss after restore {:.1} MB", r.tab.len(), crate::load_trace::maxrss_kb() as f64 * 1024.0 / 1e6);
    }
    drop(r);
    map.drop_head(blob_base);
    Ok(true)
}

impl<'a> R<'a> {
    fn lazy_ir(&mut self) -> Box<LazyIr> {
        loop {
            match self.byte() {
                0 => break,
                1 => {
                    self.val();
                }
                _ => {
                    self.arities();
                }
            }
        }
        let (map, base) = self.blob.clone().expect("image: lazy IR outside restore");
        let off = base + self.u() as usize;
        let ext: Box<[Obj]> = (0..self.u()).map(|_| {
            let id = self.u();
            self.get(id).clone()
        }).collect();
        IR_LAZY.fetch_add(1, Ordering::Relaxed);
        Box::new(LazyIr { map, off, ext, code: self.code.clone() })
    }
    fn class_key(&mut self) -> ClassKey {
        match self.byte() {
            0 => ClassKey::Nil,
            1 => {
                let n = self.raw();
                ClassKey::Builtin(crate::types::static_builtin_name(n).expect("image: builtin class name"))
            }
            2 => {
                let id = self.u();
                ClassKey::User(self.obj_ptr(id - 1))
            }
            3 => ClassKey::Interface(self.str_()),
            _ => ClassKey::Object,
        }
    }
    fn mtable(&mut self) -> crate::types::MethodTable {
        let mut mt = crate::types::MethodTable::new();
        for _ in 0..self.u() {
            let k = self.str_();
            let x = self.val();
            mt.insert(k, x);
        }
        mt
    }
}

// ------------------------------------------------------------ driver
fn stat_key(p: &std::path::Path) -> Option<(u64, u64)> {
    let m = std::fs::metadata(p).ok()?;
    let t = m.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos() as u64;
    Some((m.len(), t))
}

/// Static validity key: binary identity, module path, preload, load flags.
pub fn header(module_paths: &[std::path::PathBuf], preload: &str) -> String {
    let exe = std::env::current_exe().ok().and_then(|p| stat_key(&p)).unwrap_or((0, 0));
    let mp: Vec<String> = module_paths.iter().map(|p| p.display().to_string()).collect();
    let flags: Vec<String> = ["MOVA_LAZY_TIER_N", "MOVA_EAGER_COMPILE", "MOVA_REFLECTION_WARNINGS", "MOVA_FUEL", "MOVA_NO_COMPILE", "MOVA_IMAGE_TRAIN", "MOVA_IMAGE_NO_IR"]
        .iter()
        .map(|k| format!("{k}={}", std::env::var(k).unwrap_or_default()))
        .collect();
    format!("v4|exe:{}:{}|mp:{}|pre:{}|{}", exe.0, exe.1, mp.join(":"), preload, flags.join(","))
}

/// P0b: validity key of a CORE image (natives-only base). No exe stat: the
/// caller embeds/keys the image to the binary itself.
pub fn core_header() -> String {
    let flags: Vec<String> = ["MOVA_LAZY_TIER_N", "MOVA_EAGER_COMPILE", "MOVA_REFLECTION_WARNINGS", "MOVA_FUEL", "MOVA_NO_COMPILE"]
        .iter()
        .map(|k| format!("{k}={}", std::env::var(k).unwrap_or_default()))
        .collect();
    format!("p0b-core-v1|{}", flags.join(","))
}

fn loaded_files(interp: &Interp) -> Vec<(String, u64, u64)> {
    let (_, loaded) = crate::ns::img_dump(&interp.namespaces);
    let mut out = Vec::new();
    for n in loaded {
        let stem = n.replace('-', "_").replace('.', "/");
        'f: for d in &interp.module_paths {
            for ext in ["mova", "clj", "cljc"] {
                let p = d.join(format!("{stem}.{ext}"));
                if let Some((len, t)) = stat_key(&p) {
                    out.push((p.display().to_string(), len, t));
                    break 'f;
                }
            }
        }
    }
    out
}

/// `MOVA_IMAGE=<path> MOVA_IMAGE_PRELOAD=<ns>`: restore the post-`require`
/// state from `<path>` if it is valid, else boot normally, `require` the
/// preload ns, and (re)write the image. Returns true if restored.
pub fn run_preload(interp: &mut Interp, path: &str, preload: &str) -> Result<bool, String> {
    let trace = std::env::var("MOVA_IMAGE_TRACE").is_ok_and(|v| v == "1");
    let path = std::path::Path::new(path);
    let hdr = header(&interp.module_paths, preload);
    let t0 = std::time::Instant::now();
    if files_ok(path) {
        match restore(interp, &hdr, path)? {
            true => {
                crate::jit::image_decided(true);
                if trace {
                    eprintln!("[image] restored {} in {:.1} ms", path.display(), t0.elapsed().as_secs_f64() * 1e3);
                }
                return Ok(true);
            }
            false if trace => eprintln!("[image] stale/absent: normal load"),
            false => {}
        }
    }
    crate::jit::image_decided(false);
    let src_base = crate::source_registry::img_len();
    let pre = pre_index(interp);
    interp.eval_str("image-preload", &format!("(require '{preload})")).map_err(|e| format!("{e:?}"))?;
    // MOVA_IMAGE_TRAIN=<file>: warm-up run so hot fns hold IR when the image is written
    let train = std::env::var("MOVA_IMAGE_TRAIN").ok().filter(|t| !t.is_empty());
    if let Some(t) = &train {
        let t2 = std::time::Instant::now();
        let src = std::fs::read_to_string(t).map_err(|e| format!("MOVA_IMAGE_TRAIN {t}: {e}"))?;
        if let Err(e) = interp.eval_str(t, &src) {
            eprintln!("mova: image: training {t} failed: {e:?}");
        } else if trace {
            eprintln!("[image] trained {t} in {:.1} ms", t2.elapsed().as_secs_f64() * 1e3);
        }
    }
    let t1 = std::time::Instant::now();
    let mut files = loaded_files(interp);
    if let Some(t) = &train {
        let p = std::path::Path::new(t);
        if let Some((l, m)) = stat_key(p) {
            files.push((p.canonicalize().unwrap_or(p.to_path_buf()).display().to_string(), l, m));
        }
    }
    let rep = write_image(interp, &pre, &hdr, src_base, path)?;
    let mut list = String::new();
    for (p, l, t) in &files {
        list.push_str(&format!("{p}\t{l}\t{t}\n"));
    }
    if rep.unsupported.is_empty() {
        std::fs::write(path.with_extension("files"), list).map_err(|e| e.to_string())?;
    }
    if trace || !rep.unsupported.is_empty() {
        eprintln!(
            "[image] wrote {} ({:.2} MB, {} new objs, {} pre, {} files) in {:.1} ms; census {:?}; UNSUPPORTED {:?}",
            path.display(),
            rep.bytes as f64 / 1e6,
            rep.objects,
            pre.objs.len(),
            files.len(),
            t1.elapsed().as_secs_f64() * 1e3,
            rep.census,
            rep.unsupported
        );
    }
    Ok(false)
}

/// Every source file the image was built from still has the same size+mtime.
fn files_ok(path: &std::path::Path) -> bool {
    let Ok(list) = std::fs::read_to_string(path.with_extension("files")) else { return false };
    list.lines().all(|l| {
        let mut it = l.split('\t');
        let (Some(p), Some(len), Some(t)) = (it.next(), it.next(), it.next()) else { return false };
        stat_key(std::path::Path::new(p)) == Some((len.parse().unwrap_or(0), t.parse().unwrap_or(0)))
    })
}

/// Read-only private mapping of the image file, unmapped on drop.
struct Mapped {
    p: *mut libc::c_void,
    len: usize,
    /// Bytes already unmapped from the front (page-aligned); see `drop_head`.
    head: std::sync::atomic::AtomicUsize,
}
impl Mapped {
    fn open(path: &std::path::Path) -> Option<Mapped> {
        use std::os::unix::io::AsRawFd;
        let f = std::fs::File::open(path).ok()?;
        let len = f.metadata().ok()?.len() as usize;
        if len == 0 {
            return None;
        }
        let p = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ, libc::MAP_PRIVATE, f.as_raw_fd(), 0) };
        if p == libc::MAP_FAILED {
            return None;
        }
        Some(Mapped { p, len, head: std::sync::atomic::AtomicUsize::new(0) })
    }
    /// The whole mapping. Only valid before `drop_head`.
    fn bytes(&self) -> &[u8] {
        assert_eq!(self.head.load(Ordering::Acquire), 0);
        unsafe { std::slice::from_raw_parts(self.p as *const u8, self.len) }
    }
    fn bytes_from(&self, from: usize) -> &[u8] {
        assert!(from >= self.head.load(Ordering::Acquire) && from <= self.len);
        unsafe { std::slice::from_raw_parts((self.p as *const u8).add(from), self.len - from) }
    }
    /// Unmap `[0, upto)` (rounded down to a page) once the eager part of the
    /// image is decoded, so its pages leave RSS; lazy bodies live past it.
    fn drop_head(&self, upto: usize) {
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
        let n = upto / page * page;
        if n > 0 && self.head.load(Ordering::Acquire) == 0 {
            unsafe { libc::munmap(self.p, n) };
            self.head.store(n, Ordering::Release);
        }
    }
}
// Read-only (PROT_READ) mapping: shared immutable bytes.
unsafe impl Send for Mapped {}
unsafe impl Sync for Mapped {}
impl Drop for Mapped {
    fn drop(&mut self) {
        let h = *self.head.get_mut();
        unsafe { libc::munmap((self.p as *mut u8).add(h) as *mut libc::c_void, self.len - h) };
    }
}
