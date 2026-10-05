//! L1 (docs/JIT.md "Leaf ops (L1)"): leaf nodes lowered natively in the
//! threaded tier instead of `jit_t_exec`. Inline code only reads a tag BYTE
//! (`jit::layout`) and moves raw words; every clone/drop/error runs in a
//! Rust helper below that uses the interpreter's own code.

use std::sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use cranelift_codegen::ir::{condcodes::FloatCC, condcodes::IntCC, types, InstBuilder, MemFlagsData};
use cranelift_codegen::ir::{FuncRef, Value as CVal};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{FuncId, Module};

use super::{declare_helper, LowerCtx};
use crate::builtins::{numbers, predicates};
use crate::compile::exec::native_err;
use crate::compile::ir::{GlobalChain, IntrinOp, Ir};
use crate::error::RjError;
use crate::jit::layout::{self, Layout};
use crate::jit::{TCtx, DEF_EPOCH, THREADED_OUT};
use crate::value::Value;

const I64: types::Type = types::I64;
const I32: types::Type = types::I32;
const I8: types::Type = types::I8;

fn fl() -> MemFlagsData {
    MemFlagsData::trusted()
}

#[inline(always)]
unsafe fn dst_of(ctx: &mut TCtx, d: u32) -> *mut Value {
    if d == THREADED_OUT {
        &mut ctx.out
    } else {
        ctx.slots.add(d as usize)
    }
}

/// Drops a heap value in place and leaves `Nil` (inline code then overwrites it).
extern "C" fn jit_t_drop(p: *mut Value) {
    unsafe {
        // K3: an Interned keyword owns nothing (no drop glue call).
        if !matches!(&*p, Value::Keyword(crate::keyword::Keyword::Interned(_))) {
            std::ptr::drop_in_place(p);
        }
        std::ptr::write(p, Value::Nil);
    }
}

/// `*dst = (*src).clone()` (old `*dst` dropped by the assignment).
extern "C" fn jit_t_clone(src: *const Value, dst: *mut Value) {
    unsafe { *dst = (*src).clone() };
}

/// `Ir::LoadCapture(i)`: exec's `l.caps[i].clone()`.
extern "C" fn jit_t_load_cap(ctx: *mut TCtx, i: u32, d: u32) {
    unsafe {
        let c = &mut *ctx;
        let v = (&*c.locals).caps[i as usize].clone();
        *dst_of(c, d) = v;
    }
}

/// `Ir::SelfRef`: exec's `Value::Fn(l.me.clone())`.
extern "C" fn jit_t_self_ref(ctx: *mut TCtx, d: u32) {
    unsafe {
        let c = &mut *ctx;
        let v = Value::Fn((&*c.locals).me.clone());
        *dst_of(c, d) = v;
    }
}

/// `exec_intrinsic`'s guard, re-checked on every evaluation.
extern "C" fn jit_t_armed(chain: *const GlobalChain, site: *const AtomicU64) -> u8 {
    // K3: epoch read BEFORE the check, so a racing `def` leaves a stale (missing) entry, never a wrong hit.
    let now = crate::jit::DEF_EPOCH.load(Ordering::Acquire);
    let armed = unsafe { (*chain).intrinsic_armed() };
    if now < (1 << 62) {
        unsafe { (*site).store((now << 1) | armed as u64, Ordering::Release) };
    }
    armed as u8
}

/// Post-guard `exec_intrinsic` on already-evaluated operands (moved out of
/// their temps, dropped here like exec's locals). `b` unused for unary ops.
extern "C" fn jit_t_intrin(ctx: *mut TCtx, node: *const Ir, a: *mut Value, b: *mut Value, d: u32, own: u32) -> u32 {
    unsafe {
        let c = &mut *ctx;
        let interp = &mut *c.interp;
        let (op, span, n) = match &*node {
            Ir::Intrinsic { op, span, args, .. } => (*op, *span, args.len()),
            _ => unreachable!("jit_t_intrin: not an Intrinsic"),
        };
        // K3: bit i of `own` clear = operand i is a borrowed local slot (clone, never take).
        let a = if own & 1 != 0 { std::mem::replace(&mut *a, Value::Nil) } else { (*a).clone() };
        let b = if n != 2 {
            Value::Nil
        } else if own & 2 != 0 {
            std::mem::replace(&mut *b, Value::Nil)
        } else {
            (*b).clone()
        };
        let out: Result<Value, RjError> = match op {
            IntrinOp::Add | IntrinOp::Mul => {
                let (init, step): (Value, numbers::NumStep) = match op {
                    IntrinOp::Add => (Value::Int(0), numbers::add_step),
                    _ => (Value::Int(1), numbers::mul_step),
                };
                if numbers::skips_identity(&a) {
                    step(interp, &a, &b)
                } else {
                    step(interp, &init, &a).and_then(|acc| step(interp, &acc, &b))
                }
            }
            IntrinOp::Inc => numbers::inc1(interp, &a),
            IntrinOp::Dec => numbers::dec1(interp, &a),
            IntrinOp::Zero => predicates::zero1(&a),
            IntrinOp::Not => Ok(predicates::not1(&a)),
            IntrinOp::Sub2 => numbers::sub2(interp, &a, &b),
            IntrinOp::Div2 => numbers::div2(&a, &b),
            IntrinOp::Lt2 => numbers::lt2(&a, &b),
            IntrinOp::Le2 => numbers::le2(&a, &b),
            IntrinOp::Gt2 => numbers::gt2(&a, &b),
            IntrinOp::Ge2 => numbers::ge2(&a, &b),
            IntrinOp::Eq2 => interp.values_equal(&a, &b).map(Value::Bool),
        };
        match out {
            Ok(v) => {
                *dst_of(c, d) = v;
                0
            }
            Err(e) => {
                c.err = Some(native_err(interp, e, span));
                2
            }
        }
    }
}

/// n-ary `+`/`*` (not 2 args): exec's `fold_nary` over the arg temps, then drops them.
extern "C" fn jit_t_intrin_n(ctx: *mut TCtx, node: *const Ir, base: *mut Value, argc: u32, d: u32) -> u32 {
    unsafe {
        let c = &mut *ctx;
        let interp = &mut *c.interp;
        let (op, span) = match &*node {
            Ir::Intrinsic { op, span, .. } => (*op, *span),
            _ => unreachable!("jit_t_intrin_n: not an Intrinsic"),
        };
        let (init, step): (Value, numbers::NumStep) = match op {
            IntrinOp::Add => (Value::Int(0), numbers::add_step),
            _ => (Value::Int(1), numbers::mul_step),
        };
        let args = std::slice::from_raw_parts_mut(base, argc as usize);
        let out = numbers::fold_nary(interp, args, init, step);
        for a in args.iter_mut() {
            *a = Value::Nil;
        }
        match out {
            Ok(v) => {
                *dst_of(c, d) = v;
                0
            }
            Err(e) => {
                c.err = Some(native_err(interp, e, span));
                2
            }
        }
    }
}

/// `(:k x)`: exec_call's `apply_value_owned(kw, [x])` is `apply_value(kw, &[x])`;
/// `x` is BORROWED from its slot, then dropped there iff `take` (a moving read/temp).
extern "C" fn jit_t_kw_call(ctx: *mut TCtx, node: *const Ir, kw: *const Value, arg: *mut Value, take: u32, d: u32) -> u32 {
    unsafe {
        let c = &mut *ctx;
        let interp = &mut *c.interp;
        let span = match &*node {
            Ir::Call { span, .. } => *span,
            _ => unreachable!("jit_t_kw_call: not a Call"),
        };
        // K3: plain map receiver -> direct borrowed lookup (named_lookup's Map arm), no apply.
        // K6: a record receiver too (named_lookup's record arm), skipping apply_value.
        let direct = match &*arg {
            Value::Map(m) => {
                crate::builtins::map_probe::record("keyword-lookup", m.len());
                Some(m)
            }
            // A record that declared ILookup keeps apply_value's valAt route.
            Value::Inst(inst) if inst.tdef.is_record && !inst.tdef.interfaces.iter().any(|i| i.as_ref() == "clojure.lang.ILookup") => Some(&inst.data),
            _ => None,
        };
        if let Some(m) = direct {
            let v = m.get(&*kw).cloned().unwrap_or(Value::Nil);
            if take != 0 {
                *arg = Value::Nil;
            }
            *dst_of(c, d) = v;
            return 0;
        }
        let out = interp.apply_value(&*kw, std::slice::from_ref(&*arg), span);
        if take != 0 {
            *arg = Value::Nil;
        }
        match out {
            Ok(v) => {
                *dst_of(c, d) = v;
                0
            }
            Err(e) => {
                c.err = Some(e);
                2
            }
        }
    }
}

/// One cached global value; leaked (a concurrent reader may be cloning it).
struct GTarget {
    epoch: AtomicU64,
    v: Value,
}

/// One `GlobalRef` site's IC; stops caching after `MAX_REFILLS` new targets.
struct GlobalSite {
    cur: AtomicPtr<GTarget>,
    refills: AtomicU32,
}
const MAX_REFILLS: u32 = 64;

fn chain_dyn(chain: &GlobalChain) -> bool {
    match chain {
        GlobalChain::One(c) => c.is_dyn_hinted(),
        GlobalChain::Two(a, b) => a.is_dyn_hinted() || b.is_dyn_hinted(),
        GlobalChain::Many(cs) => cs.iter().any(|c| c.is_dyn_hinted()),
    }
}

/// Identity for a refill that can reuse the current target (no new leak).
fn same_identity(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Fn(x), Value::Fn(y)) => Arc::ptr_eq(x, y),
        (Value::Native(x), Value::Native(y)) => Arc::ptr_eq(x, y),
        (Value::Atom(x), Value::Atom(y)) => Arc::ptr_eq(x, y),
        (Value::Nil, Value::Nil) => true,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Int(x), Value::Int(y)) => x == y,
        _ => false,
    }
}

/// `Ir::GlobalRef`: exec's `chain.get()` behind a `DEF_EPOCH` IC; a chain
/// with any `binding`-hinted cell is never cached.
extern "C" fn jit_t_global(ctx: *mut TCtx, site: *const (), node: *const Ir, d: u32) -> u32 {
    unsafe {
        let c = &mut *ctx;
        let site = &*(site as *const GlobalSite);
        let now = DEF_EPOCH.load(Ordering::Acquire);
        let cur = site.cur.load(Ordering::Acquire);
        if !cur.is_null() && (*cur).epoch.load(Ordering::Acquire) == now {
            *dst_of(c, d) = (*cur).v.clone();
            return 0;
        }
        let (chain, sym, span) = match &*node {
            Ir::GlobalRef { chain, sym, span } => (chain, sym, *span),
            _ => unreachable!("jit_t_global: not a GlobalRef"),
        };
        let v = match chain.get() {
            Some(v) => v,
            None => {
                c.err = Some(crate::compile::exec::unresolved(&*c.interp, sym, span));
                return 2;
            }
        };
        if !chain_dyn(chain) {
            if !cur.is_null() && same_identity(&(*cur).v, &v) {
                (*cur).epoch.store(now, Ordering::Release);
            } else if site.refills.fetch_add(1, Ordering::Relaxed) < MAX_REFILLS {
                let t = Box::leak(Box::new(GTarget { epoch: AtomicU64::new(now), v: v.clone() }));
                site.cur.store(t, Ordering::Release);
            } else if !cur.is_null() {
                site.cur.store(std::ptr::null_mut(), Ordering::Release);
            }
        }
        *dst_of(c, d) = v;
        0
    }
}

// S3: leaked per-site global cache; also the image restore's factory.
// Armed-IC cell; also the image restore factory (a fresh cell re-arms lazily).
fn fresh_armed_site() -> usize {
    Box::leak(Box::new(AtomicU64::new(0))) as *const AtomicU64 as usize
}

fn fresh_global_site() -> usize {
    Box::leak(Box::new(GlobalSite { cur: AtomicPtr::new(std::ptr::null_mut()), refills: AtomicU32::new(0) })) as *const GlobalSite as usize
}

pub(super) fn register(jb: &mut JITBuilder) {
    jb.symbol("jit_t_drop", jit_t_drop as *const u8);
    jb.symbol("jit_t_clone", jit_t_clone as *const u8);
    jb.symbol("jit_t_load_cap", jit_t_load_cap as *const u8);
    jb.symbol("jit_t_self_ref", jit_t_self_ref as *const u8);
    jb.symbol("jit_t_armed", jit_t_armed as *const u8);
    jb.symbol("jit_t_intrin", jit_t_intrin as *const u8);
    jb.symbol("jit_t_global", jit_t_global as *const u8);
    jb.symbol("jit_t_intrin_n", jit_t_intrin_n as *const u8);
    jb.symbol("jit_t_kw_call", jit_t_kw_call as *const u8);
    jb.symbol("jit_t_bind", jit_t_bind as *const u8);
    jb.symbol("jit_t_coll", jit_t_coll as *const u8);
    jb.symbol("jit_t_new_chk", jit_t_new_chk as *const u8);
    jb.symbol("jit_t_new_make", jit_t_new_make as *const u8);
    jb.symbol("jit_t_mk_fn", jit_t_mk_fn as *const u8);
    jb.symbol("jit_t_catch", jit_t_catch as *const u8);
}

/// K7b: native `try`'s handler (`exec_try`'s arm choice): `k + 3` = arm `k` matched (thrown value in
/// its slot, error consumed); 2 = not ours (recur/fuel/no match), `ctx.err` kept.
extern "C" fn jit_t_catch(ctx: *mut TCtx, node: *const Ir) -> u32 {
    unsafe {
        let Ir::Try { catches, .. } = &*node else { unreachable!("jit_t_catch: not a Try") };
        let c = &mut *ctx;
        let Some(e) = c.err.take() else { return 2 };
        if matches!(e.kind, crate::error::ErrorKind::Recur | crate::error::ErrorKind::FuelExhausted | crate::error::ErrorKind::InterruptedHard) {
            c.err = Some(e);
            return 2;
        }
        let hit = catches
            .iter()
            .position(|arm| arm.class.as_ref().is_none_or(|cl| crate::eval::special_forms::catch_class_matches(cl, &e)));
        match hit {
            Some(k) => {
                let thrown = if e.kind == crate::error::ErrorKind::Thrown {
                    e.thrown.clone().unwrap_or(Value::Nil)
                } else {
                    crate::eval::special_forms::error_to_info_map(&e)
                };
                *c.slots.add(catches[k].slot as usize) = thrown;
                k as u32 + 3
            }
            None => {
                c.err = Some(e);
                2
            }
        }
    }
}

/// K7b: `Ir::MakeClosure` straight to `make_closure` (no `exec` dispatch / `Flow`).
extern "C" fn jit_t_mk_fn(ctx: *mut TCtx, node: *const Ir, d: u32) -> u32 {
    unsafe {
        let Ir::MakeClosure { template, caps } = &*node else { unreachable!("jit_t_mk_fn: not a MakeClosure") };
        let c = &mut *ctx;
        match crate::compile::exec::make_closure(&*c.interp, template, caps, &*c.locals) {
            Ok(v) => {
                *dst_of(c, d) = v;
                0
            }
            Err(e) => {
                c.err = Some(e);
                2
            }
        }
    }
}

/// K3: `[..]`/`{..}` literal from `n` items (map: 2n k,v slots) already in temps at `base` (moved out).
extern "C" fn jit_t_coll(ctx: *mut TCtx, kind: u32, base: u32, n: u32, d: u32) {
    unsafe {
        let c = &mut *ctx;
        let take = |i: u32| std::mem::replace(&mut *c.slots.add((base + i) as usize), Value::Nil);
        let v = if kind == 0 {
            Value::Vector(crate::value::PVec::from((0..n).map(take).collect::<Vec<Value>>()))
        } else {
            let mut out = crate::value::PMap::new();
            for i in 0..n {
                let k = take(2 * i);
                out.insert(k, take(2 * i + 1));
            }
            crate::builtins::map_probe::record("map-literal", out.len());
            Value::Map(out)
        };
        *dst_of(c, d) = v;
    }
}

/// K3: destructuring `let` bind: moves `slots[t]` through `exec_pattern` (status like `jit_t_exec`).
extern "C" fn jit_t_bind(ctx: *mut TCtx, node: *const Ir, bi: u32, t: u32) -> u32 {
    unsafe {
        let binds = match &*node {
            Ir::Let { binds, .. } | Ir::Loop { binds, .. } => binds,
            _ => unreachable!("jit_t_bind: not a Let/Loop"),
        };
        let pat = &binds[bi as usize].0;
        let c = &mut *ctx;
        let v = std::mem::replace(&mut *c.slots.add(t as usize), Value::Nil);
        match crate::compile::exec::exec_pattern(&mut *c.interp, pat, v, &mut *c.locals) {
            Ok(crate::compile::exec::Flow::Val(_)) => 0,
            Ok(crate::compile::exec::Flow::Recur) => 1,
            Err(e) => {
                c.err = Some(e);
                2
            }
        }
    }
}

/// K5: `Ir::New` gate -- 1 and the class in `slots[t]` iff the fast path applies, else 0 (run the fallback).
extern "C" fn jit_t_new_chk(ctx: *mut TCtx, node: *const Ir, t: u32) -> u32 {
    unsafe {
        let Ir::New(n) = &*node else { unreachable!("jit_t_new_chk: not a New") };
        let c = &mut *ctx;
        let l = &*c.locals;
        let env = &l.me.env;
        match (*c.interp).new_fast_class(&n.class, env, n.args.len()) {
            Some(v) => {
                *c.slots.add(t as usize) = v;
                1
            }
            None => 0,
        }
    }
}

/// K5: `Ir::New` fast path -- class in `slots[t]`, args moved out of `slots[base..]`.
extern "C" fn jit_t_new_make(ctx: *mut TCtx, node: *const Ir, t: u32, base: u32, d: u32) -> u32 {
    unsafe {
        let Ir::New(n) = &*node else { unreachable!("jit_t_new_make: not a New") };
        let c = &mut *ctx;
        let class = std::mem::replace(&mut *c.slots.add(t as usize), Value::Nil);
        let vals: Vec<Value> = (0..n.args.len()).map(|i| std::mem::replace(&mut *c.slots.add(base as usize + i), Value::Nil)).collect();
        match (*c.interp).new_fast_make(&class, &vals, n.span) {
            Ok(v) => {
                *dst_of(c, d) = v;
                0
            }
            Err(e) => {
                c.err = Some(e);
                2
            }
        }
    }
}

pub(super) struct LeafIds([FuncId; 15]);
pub(super) struct LeafRefs {
    drop: FuncRef,
    clone: FuncRef,
    load_cap: FuncRef,
    self_ref: FuncRef,
    armed: FuncRef,
    intrin: FuncRef,
    global: FuncRef,
    intrin_n: FuncRef,
    kw_call: FuncRef,
    pub(super) bind: FuncRef,
    pub(super) coll: FuncRef,
    pub(super) new_chk: FuncRef,
    pub(super) new_make: FuncRef,
    mk_fn: FuncRef,
    pub(super) catch: FuncRef,
}

pub(super) fn declare(m: &mut JITModule, p: types::Type) -> LeafIds {
    LeafIds([
        declare_helper(m, "jit_t_drop", &[p], None),
        declare_helper(m, "jit_t_clone", &[p, p], None),
        declare_helper(m, "jit_t_load_cap", &[p, I32, I32], None),
        declare_helper(m, "jit_t_self_ref", &[p, I32], None),
        declare_helper(m, "jit_t_armed", &[p, p], Some(I8)),
        declare_helper(m, "jit_t_intrin", &[p, p, p, p, I32, I32], Some(I32)),
        declare_helper(m, "jit_t_global", &[p, p, p, I32], Some(I32)),
        declare_helper(m, "jit_t_intrin_n", &[p, p, p, I32, I32], Some(I32)),
        declare_helper(m, "jit_t_kw_call", &[p, p, p, p, I32, I32], Some(I32)),
        declare_helper(m, "jit_t_bind", &[p, p, I32, I32], Some(I32)),
        declare_helper(m, "jit_t_coll", &[p, I32, I32, I32, I32], None),
        declare_helper(m, "jit_t_new_chk", &[p, p, I32], Some(I32)),
        declare_helper(m, "jit_t_new_make", &[p, p, I32, I32, I32], Some(I32)),
        declare_helper(m, "jit_t_mk_fn", &[p, p, I32], Some(I32)),
        declare_helper(m, "jit_t_catch", &[p, p], Some(I32)),
    ])
}

pub(super) fn refs(m: &mut JITModule, ids: &LeafIds, f: &mut cranelift_codegen::ir::Function) -> LeafRefs {
    let r = |m: &mut JITModule, i: usize, f: &mut cranelift_codegen::ir::Function| m.declare_func_in_func(ids.0[i], f);
    LeafRefs {
        drop: r(m, 0, f),
        clone: r(m, 1, f),
        load_cap: r(m, 2, f),
        self_ref: r(m, 3, f),
        armed: r(m, 4, f),
        intrin: r(m, 5, f),
        global: r(m, 6, f),
        intrin_n: r(m, 7, f),
        kw_call: r(m, 8, f),
        bind: r(m, 9, f),
        coll: r(m, 10, f),
        new_chk: r(m, 11, f),
        new_make: r(m, 12, f),
        mk_fn: r(m, 13, f),
        catch: r(m, 14, f),
    }
}

/// `MOVA_JIT_LEAF_SLOT=0`: LoadSlot via the clone helper only (smaller code).
fn slot_inline() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("MOVA_JIT_LEAF_SLOT").as_deref() != Ok("0"))
}

/// Probed layout, or `None` (then every leaf goes through a helper).
fn lay() -> Option<Layout> {
    layout::probe()
}

/// Which immediate (no-drop) variant `v` is, as (tag word, payload word).
fn imm_bits(l: &Layout, v: &Value) -> Option<(u64, u64)> {
    match v {
        Value::Nil => Some((l.nil, 0)),
        Value::Bool(b) => Some((l.bool_tag, *b as u64)),
        Value::Int(i) => Some((l.int_tag, *i as u64)),
        Value::Float(f) => Some((l.float_tag, f.to_bits())),
        _ => None,
    }
}

impl<'a> LowerCtx<'a> {
    fn dst_addr(&mut self, d: u32) -> CVal {
        if d == THREADED_OUT {
            let off = std::mem::offset_of!(TCtx, out) as i64;
            self.b.ins().iadd_imm_s(self.ctx_val, off)
        } else {
            self.slot_addr(d)
        }
    }

    fn tag8(&mut self, l: &Layout, addr: CVal) -> CVal {
        self.b.ins().uload8(I32, fl(), addr, l.tag_off as i32)
    }

    /// 1 iff `*addr` owns nothing: Nil/Bool/Int/Float or an Interned keyword (K3).
    fn is_pod(&mut self, l: &Layout, addr: CVal) -> CVal {
        let acc = self.is_imm(l, addr);
        let t = self.tag8(l, addr);
        let is_kw = self.b.ins().icmp_imm_s(IntCC::Equal, t, l.keyword_tag as i64);
        let disc = self.b.ins().uload8(I32, fl(), addr, l.kw_disc_off as i32);
        let interned = self.b.ins().icmp_imm_s(IntCC::Equal, disc, l.kw_interned as i64);
        let kw_pod = self.b.ins().band(is_kw, interned);
        self.b.ins().bor(acc, kw_pod)
    }

    /// 1 iff the tag byte at `addr` is Nil/Bool/Int/Float.
    fn is_imm(&mut self, l: &Layout, addr: CVal) -> CVal {
        let t = self.tag8(l, addr);
        let lo = l.nil.min(l.bool_tag).min(l.int_tag).min(l.float_tag);
        let mut tags = [l.nil, l.bool_tag, l.int_tag, l.float_tag];
        tags.sort_unstable();
        if tags.windows(2).all(|w| w[1] == w[0] + 1) {
            // Contiguous tags: one unsigned range check.
            let off = self.b.ins().iadd_imm_s(t, -(lo as i64));
            return self.b.ins().icmp_imm_s(IntCC::UnsignedLessThanOrEqual, off, 3);
        }
        let mut acc = self.b.ins().icmp_imm_s(IntCC::Equal, t, l.nil as i64);
        for tag in [l.bool_tag, l.int_tag, l.float_tag] {
            let e = self.b.ins().icmp_imm_s(IntCC::Equal, t, tag as i64);
            acc = self.b.ins().bor(acc, e);
        }
        acc
    }

    /// Drops `*addr`'s old value unless its tag is immediate.
    fn drop_old(&mut self, l: &Layout, addr: CVal) {
        let imm = self.is_imm(l, addr);
        let drop_blk = self.b.create_block();
        let cont = self.b.create_block();
        self.b.ins().brif(imm, cont, &[], drop_blk, &[]);
        self.b.seal_block(drop_blk);
        self.b.switch_to_block(drop_blk);
        self.b.ins().call(self.lh.drop, &[addr]);
        self.b.ins().jump(cont, &[]);
        self.b.seal_block(cont);
        self.b.switch_to_block(cont);
    }

    fn store_imm(&mut self, l: &Layout, addr: CVal, tag: u64, payload: CVal) {
        let t = self.b.ins().iconst(I64, tag as i64);
        self.b.ins().store(fl(), t, addr, l.tag_off as i32);
        self.b.ins().store(fl(), payload, addr, l.payload_off as i32);
    }

    fn call_clone(&mut self, src: CVal, dst: CVal) {
        self.b.ins().call(self.lh.clone, &[src, dst]);
    }

    pub(super) fn lower_const(&mut self, v: &Value, d: u32) {
        self.n_native += 1;
        let dst = self.dst_addr(d);
        match lay().and_then(|l| imm_bits(&l, v).map(|b| (l, b))) {
            Some((l, (tag, payload))) => {
                self.drop_old(&l, dst);
                let p = self.b.ins().iconst(I64, payload as i64);
                self.store_imm(&l, dst, tag, p);
            }
            None if lay().is_some() && matches!(v, Value::Keyword(crate::keyword::Keyword::Interned(_))) => {
                // K3: an Interned keyword owns nothing: raw 32-byte copy of the const.
                let l = lay().unwrap();
                self.drop_old(&l, dst);
                let src = self.b.ins().iconst(self.ptr_ty, v as *const Value as i64);
                for w in 0..4 {
                    let x = self.b.ins().load(I64, fl(), src, w * 8);
                    self.b.ins().store(fl(), x, dst, w * 8);
                }
            }
            None => {
                let src = self.b.ins().iconst(self.ptr_ty, v as *const Value as i64);
                self.call_clone(src, dst);
            }
        }
    }

    pub(super) fn lower_load_slot(&mut self, i: u16, d: u32) {
        self.n_native += 1;
        let src = self.slot_addr(i as u32);
        let dst = self.dst_addr(d);
        let Some(l) = lay().filter(|_| slot_inline()) else {
            self.call_clone(src, dst);
            return;
        };
        let imm = self.is_pod(&l, src);
        let fast = self.b.create_block();
        let slow = self.b.create_block();
        let merge = self.b.create_block();
        self.b.ins().brif(imm, fast, &[], slow, &[]);
        self.b.seal_block(fast);
        self.b.seal_block(slow);
        self.b.switch_to_block(fast);
        self.drop_old(&l, dst);
        for w in 0..4 {
            let x = self.b.ins().load(I64, fl(), src, w * 8);
            self.b.ins().store(fl(), x, dst, w * 8);
        }
        self.b.ins().jump(merge, &[]);
        self.b.switch_to_block(slow);
        self.call_clone(src, dst);
        self.b.ins().jump(merge, &[]);
        self.b.seal_block(merge);
        self.b.switch_to_block(merge);
    }

    /// K6: raw 32-byte alias of slot `i` into temp `d` (no refcount); `jit_t_call` forgets or promotes it.
    /// K7b: fn literal via `jit_t_mk_fn`.
    pub(super) fn lower_mk_fn(&mut self, node: &Ir, d: u32) {
        self.n_native += 1;
        let node_c = self.b.ins().iconst(self.ptr_ty, node as *const Ir as i64);
        let d_c = self.b.ins().iconst(I32, d as i64);
        let call = self.b.ins().call(self.lh.mk_fn, &[self.ctx_val, node_c, d_c]);
        let st = self.b.inst_results(call)[0];
        self.check_status(st);
    }

    pub(super) fn lower_borrow_arg(&mut self, i: u16, d: u32) -> bool {
        let Some(l) = lay() else { return false };
        let src = self.slot_addr(i as u32);
        let dst = self.dst_addr(d);
        self.drop_old(&l, dst);
        for w in 0..4 {
            let x = self.b.ins().load(I64, fl(), src, w * 8);
            self.b.ins().store(fl(), x, dst, w * 8);
        }
        true
    }

    /// Raw move (no refcount): read src's words, leave Nil, drop old dst, write.
    pub(super) fn lower_load_take(&mut self, i: u16, d: u32) {
        self.n_native += 1;
        let src = self.slot_addr(i as u32);
        let dst = self.dst_addr(d);
        let Some(l) = lay() else {
            // No layout: move via the clone helper + nil is not a move; keep exec.
            let node = Box::leak(Box::new(Ir::LoadSlotTake(i)));
            self.n_native -= 1;
            self.call_exec(node as *const Ir, d);
            return;
        };
        let words: Vec<CVal> = (0..4).map(|w| self.b.ins().load(I64, fl(), src, w * 8)).collect();
        let nil = self.b.ins().iconst(I64, l.nil as i64);
        self.b.ins().store(fl(), nil, src, l.tag_off as i32);
        self.drop_old(&l, dst);
        for (w, x) in words.into_iter().enumerate() {
            self.b.ins().store(fl(), x, dst, (w * 8) as i32);
        }
    }

    pub(super) fn lower_load_cap(&mut self, i: u16, d: u32) {
        self.n_native += 1;
        let i_c = self.b.ins().iconst(I32, i as i64);
        let d_c = self.b.ins().iconst(I32, d as i64);
        self.b.ins().call(self.lh.load_cap, &[self.ctx_val, i_c, d_c]);
    }

    pub(super) fn lower_self_ref(&mut self, d: u32) {
        self.n_native += 1;
        let d_c = self.b.ins().iconst(I32, d as i64);
        self.b.ins().call(self.lh.self_ref, &[self.ctx_val, d_c]);
    }

    pub(super) fn lower_global(&mut self, node: &Ir, d: u32) {
        self.n_native += 1;
        let site = fresh_global_site();
        super::super::aot::note_fresh(site, fresh_global_site);
        let site_c = self.b.ins().iconst(self.ptr_ty, site as i64);
        let node_c = self.b.ins().iconst(self.ptr_ty, node as *const Ir as i64);
        let d_c = self.b.ins().iconst(I32, d as i64);
        let call = self.b.ins().call(self.lh.global, &[self.ctx_val, site_c, node_c, d_c]);
        let st = self.b.inst_results(call)[0];
        self.check_status(st);
    }

    /// `(:k x)` with a keyword `Const` callee and one arg (see `jit_t_kw_call`).
    pub(super) fn lower_kw_call(&mut self, node: &Ir, kw: &Value, arg: &Ir, d: u32) {
        self.n_call += 1;
        let (slot, take) = match arg {
            Ir::LoadSlot(i) => (*i as u32, 0),
            Ir::LoadSlotTake(i) => (*i as u32, 1),
            other => {
                let t = self.alloc_temp();
                self.lower_into(other, t);
                (t, 1)
            }
        };
        let node_c = self.b.ins().iconst(self.ptr_ty, node as *const Ir as i64);
        let kw_c = self.b.ins().iconst(self.ptr_ty, kw as *const Value as i64);
        let a = self.slot_addr(slot);
        let take_c = self.b.ins().iconst(I32, take);
        let d_c = self.b.ins().iconst(I32, d as i64);
        let call = self.b.ins().call(self.lh.kw_call, &[self.ctx_val, node_c, kw_c, a, take_c, d_c]);
        let st = self.b.inst_results(call)[0];
        self.check_status(st);
    }

    /// Truthiness of the value at slot `t`: falsy iff Nil or Bool(false).
    pub(super) fn truthy_inline(&mut self, t: u32) -> Option<CVal> {
        let l = lay()?;
        let a = self.slot_addr(t);
        let tag = self.tag8(&l, a);
        let is_nil = self.b.ins().icmp_imm_s(IntCC::Equal, tag, l.nil as i64);
        let is_bool = self.b.ins().icmp_imm_s(IntCC::Equal, tag, l.bool_tag as i64);
        let byte = self.b.ins().uload8(I32, fl(), a, l.payload_off as i32);
        let is_zero = self.b.ins().icmp_imm_s(IntCC::Equal, byte, 0);
        let is_false = self.b.ins().band(is_bool, is_zero);
        let falsy = self.b.ins().bor(is_nil, is_false);
        Some(self.b.ins().bxor_imm_u(falsy, 1))
    }

    /// `Ir::Intrinsic`: armed check, args into temps, int fast path, else exact helper.
    pub(super) fn lower_intrinsic(&mut self, node: &Ir, op: IntrinOp, chain: &GlobalChain, args: &[Ir], d: u32) {
        let unary = matches!(op, IntrinOp::Inc | IntrinOp::Dec | IntrinOp::Zero | IntrinOp::Not);
        let n = if unary { 1 } else { 2 };
        let nary = matches!(op, IntrinOp::Add | IntrinOp::Mul) && args.len() != 2;
        if args.len() != n && !nary {
            self.call_exec(node as *const Ir, d);
            return;
        }
        self.n_native += 1;
        let armed = self.armed_ic(chain);
        let armed_blk = self.b.create_block();
        let exec_blk = self.b.create_block();
        let merge = self.b.create_block();
        self.b.ins().brif(armed, armed_blk, &[], exec_blk, &[]);
        self.b.seal_block(armed_blk);
        self.b.seal_block(exec_blk);

        self.b.switch_to_block(exec_blk);
        self.call_exec(node as *const Ir, d);
        self.b.ins().jump(merge, &[]);

        self.b.switch_to_block(armed_blk);
        if nary {
            // All temps reserved before any arg is lowered (see `lower_call`).
            let base = self.n_slots;
            for _ in args {
                self.alloc_temp();
            }
            for (i, a) in args.iter().enumerate() {
                self.lower_into(a, base + i as u32);
            }
            let node_c = self.b.ins().iconst(self.ptr_ty, node as *const Ir as i64);
            let base_a = self.slot_addr(base);
            let argc = self.b.ins().iconst(I32, args.len() as i64);
            let d_c = self.b.ins().iconst(I32, d as i64);
            let call = self.b.ins().call(self.lh.intrin_n, &[self.ctx_val, node_c, base_a, argc, d_c]);
            let st = self.b.inst_results(call)[0];
            self.check_status(st);
            self.b.ins().jump(merge, &[]);
            self.b.seal_block(merge);
            self.b.switch_to_block(merge);
            return;
        }
        // K3: a `LoadSlot` operand is read in place (borrowed), not cloned into a temp.
        let borrow = |a: &Ir| if let Ir::LoadSlot(i) = a { Some(*i as u32) } else { None };
        // arg0 is read after arg1 runs: borrow only if arg1 cannot move it out of its slot.
        let a0 = borrow(&args[0]).filter(|&i| {
            n == 1 || matches!(&args[1], Ir::Const(_) | Ir::LoadSlot(_)) || matches!(&args[1], Ir::LoadSlotTake(j) if *j as u32 != i)
        });
        let ta = a0.unwrap_or_else(|| self.alloc_temp());
        let tb = if n == 2 { borrow(&args[1]).unwrap_or_else(|| self.alloc_temp()) } else { ta };
        let own = (a0.is_none() as i64) | (((n == 2 && borrow(&args[1]).is_none()) as i64) << 1);
        if own & 1 != 0 {
            self.lower_into(&args[0], ta);
        }
        if own & 2 != 0 {
            self.lower_into(&args[1], tb);
        }
        let slow = self.b.create_block();
        let fast_ops = matches!(
            op,
            IntrinOp::Add
                | IntrinOp::Sub2
                | IntrinOp::Mul
                | IntrinOp::Inc
                | IntrinOp::Dec
                | IntrinOp::Lt2
                | IntrinOp::Le2
                | IntrinOp::Gt2
                | IntrinOp::Ge2
                | IntrinOp::Zero
        );
        match lay() {
            Some(l) if fast_ops => self.int_fast(&l, op, ta, tb, n, d, slow, merge),
            Some(l) if matches!(op, IntrinOp::Eq2) => self.eq_fast(&l, ta, tb, d, slow, merge),
            _ => {
                self.b.ins().jump(slow, &[]);
            }
        }
        self.b.seal_block(slow);
        self.b.switch_to_block(slow);
        let node_c = self.b.ins().iconst(self.ptr_ty, node as *const Ir as i64);
        let a = self.slot_addr(ta);
        let b = self.slot_addr(tb);
        let d_c = self.b.ins().iconst(I32, d as i64);
        let own_c = self.b.ins().iconst(I32, own);
        let call = self.b.ins().call(self.lh.intrin, &[self.ctx_val, node_c, a, b, d_c, own_c]);
        let st = self.b.inst_results(call)[0];
        self.check_status(st);
        self.b.ins().jump(merge, &[]);
        self.b.seal_block(merge);
        self.b.switch_to_block(merge);
    }

    /// K3: `intrinsic_armed` cached per site as `(DEF_EPOCH << 1) | armed`; helper only on epoch change.
    fn armed_ic(&mut self, chain: &GlobalChain) -> CVal {
        let site = fresh_armed_site();
        super::super::aot::note_fresh(site, fresh_armed_site);
        let site_c = self.b.ins().iconst(self.ptr_ty, site as i64);
        let ep_c = self.b.ins().iconst(self.ptr_ty, &crate::jit::DEF_EPOCH as *const AtomicU64 as i64);
        let cached = self.b.ins().load(I64, fl(), site_c, 0);
        let now = self.b.ins().load(I64, fl(), ep_c, 0);
        let cep = self.b.ins().ushr_imm_u(cached, 1);
        let hit = self.b.ins().icmp(IntCC::Equal, cep, now);
        let hit_blk = self.b.create_block();
        let miss_blk = self.b.create_block();
        let out = self.b.create_block();
        self.b.append_block_param(out, I8);
        self.b.ins().brif(hit, hit_blk, &[], miss_blk, &[]);
        self.b.seal_block(hit_blk);
        self.b.seal_block(miss_blk);
        self.b.switch_to_block(hit_blk);
        let bit = self.b.ins().band_imm_u(cached, 1);
        let bit = self.b.ins().ireduce(I8, bit);
        self.b.ins().jump(out, &[bit.into()]);
        self.b.switch_to_block(miss_blk);
        let chain_c = self.b.ins().iconst(self.ptr_ty, chain as *const GlobalChain as i64);
        let call = self.b.ins().call(self.lh.armed, &[chain_c, site_c]);
        let a = self.b.inst_results(call)[0];
        self.b.ins().jump(out, &[a.into()]);
        self.b.seal_block(out);
        self.b.switch_to_block(out);
        self.b.block_params(out)[0]
    }

    /// K3 `=`: Int/Int payload compare, Interned-kw/Interned-kw id compare; else `slow`.
    fn eq_fast(&mut self, l: &Layout, ta: u32, tb: u32, d: u32, slow: cranelift_codegen::ir::Block, merge: cranelift_codegen::ir::Block) {
        let aa = self.slot_addr(ta);
        let ba = self.slot_addr(tb);
        let at = self.tag8(l, aa);
        let bt = self.tag8(l, ba);
        let ai = self.b.ins().icmp_imm_s(IntCC::Equal, at, l.int_tag as i64);
        let bi = self.b.ins().icmp_imm_s(IntCC::Equal, bt, l.int_tag as i64);
        let both_int = self.b.ins().band(ai, bi);
        let int_blk = self.b.create_block();
        let kw_test = self.b.create_block();
        let kw_blk = self.b.create_block();
        let wr = self.b.create_block();
        self.b.append_block_param(wr, I8);
        self.b.ins().brif(both_int, int_blk, &[], kw_test, &[]);
        self.b.seal_block(int_blk);
        self.b.seal_block(kw_test);
        self.b.switch_to_block(int_blk);
        let x = self.b.ins().load(I64, fl(), aa, l.payload_off as i32);
        let y = self.b.ins().load(I64, fl(), ba, l.payload_off as i32);
        let r = self.b.ins().icmp(IntCC::Equal, x, y);
        self.b.ins().jump(wr, &[r.into()]);
        self.b.switch_to_block(kw_test);
        let ak = self.b.ins().icmp_imm_s(IntCC::Equal, at, l.keyword_tag as i64);
        let bk = self.b.ins().icmp_imm_s(IntCC::Equal, bt, l.keyword_tag as i64);
        let ad = self.b.ins().uload8(I32, fl(), aa, l.kw_disc_off as i32);
        let bd = self.b.ins().uload8(I32, fl(), ba, l.kw_disc_off as i32);
        let ad = self.b.ins().icmp_imm_s(IntCC::Equal, ad, l.kw_interned as i64);
        let bd = self.b.ins().icmp_imm_s(IntCC::Equal, bd, l.kw_interned as i64);
        let k1 = self.b.ins().band(ak, bk);
        let k2 = self.b.ins().band(ad, bd);
        let both_kw = self.b.ins().band(k1, k2);
        self.b.ins().brif(both_kw, kw_blk, &[], slow, &[]);
        self.b.seal_block(kw_blk);
        self.b.switch_to_block(kw_blk);
        let x = self.b.ins().load(I32, fl(), aa, l.kw_id_off as i32);
        let y = self.b.ins().load(I32, fl(), ba, l.kw_id_off as i32);
        let r = self.b.ins().icmp(IntCC::Equal, x, y);
        self.b.ins().jump(wr, &[r.into()]);
        self.b.seal_block(wr);
        self.b.switch_to_block(wr);
        let r = self.b.block_params(wr)[0];
        let dst = self.dst_addr(d);
        self.drop_old(l, dst);
        let p = self.b.ins().uextend(I64, r);
        self.store_imm(l, dst, l.bool_tag, p);
        self.b.ins().jump(merge, &[]);
    }

    /// Both operands Int: compute; overflow/non-Int jumps to `slow` (exact helper).
    #[allow(clippy::too_many_arguments)]
    fn int_fast(
        &mut self,
        l: &Layout,
        op: IntrinOp,
        ta: u32,
        tb: u32,
        n: usize,
        d: u32,
        slow: cranelift_codegen::ir::Block,
        merge: cranelift_codegen::ir::Block,
    ) {
        let aa = self.slot_addr(ta);
        let ba = self.slot_addr(tb);
        let at = self.tag8(l, aa);
        let mut ok = self.b.ins().icmp_imm_s(IntCC::Equal, at, l.int_tag as i64);
        if n == 2 {
            let bt = self.tag8(l, ba);
            let e = self.b.ins().icmp_imm_s(IntCC::Equal, bt, l.int_tag as i64);
            ok = self.b.ins().band(ok, e);
        }
        let ints = self.b.create_block();
        self.b.ins().brif(ok, ints, &[], slow, &[]);
        self.b.seal_block(ints);
        self.b.switch_to_block(ints);
        let a = self.b.ins().load(I64, fl(), aa, l.payload_off as i32);
        let b = if n == 2 { self.b.ins().load(I64, fl(), ba, l.payload_off as i32) } else { a };
        // (result, overflow flag, result is Bool?)
        let (r, of, is_bool) = match op {
            IntrinOp::Add => {
                let r = self.b.ins().iadd(a, b);
                let x = self.b.ins().bxor(a, r);
                let y = self.b.ins().bxor(b, r);
                let z = self.b.ins().band(x, y);
                (r, Some(self.b.ins().icmp_imm_s(IntCC::SignedLessThan, z, 0)), false)
            }
            IntrinOp::Sub2 => {
                let r = self.b.ins().isub(a, b);
                let x = self.b.ins().bxor(a, b);
                let y = self.b.ins().bxor(a, r);
                let z = self.b.ins().band(x, y);
                (r, Some(self.b.ins().icmp_imm_s(IntCC::SignedLessThan, z, 0)), false)
            }
            IntrinOp::Mul => {
                let r = self.b.ins().imul(a, b);
                let hi = self.b.ins().smulhi(a, b);
                let sign = self.b.ins().sshr_imm_u(r, 63);
                (r, Some(self.b.ins().icmp(IntCC::NotEqual, hi, sign)), false)
            }
            IntrinOp::Inc => {
                let r = self.b.ins().iadd_imm_s(a, 1);
                (r, Some(self.b.ins().icmp_imm_s(IntCC::Equal, a, i64::MAX)), false)
            }
            IntrinOp::Dec => {
                let r = self.b.ins().iadd_imm_s(a, -1);
                (r, Some(self.b.ins().icmp_imm_s(IntCC::Equal, a, i64::MIN)), false)
            }
            IntrinOp::Zero => (self.b.ins().icmp_imm_s(IntCC::Equal, a, 0), None, true),
            // numbers::cmp2 compares Int/Int AS f64 (`as_f64`): do the same.
            _ => {
                let fa = self.b.ins().fcvt_from_sint(types::F64, a);
                let fb = self.b.ins().fcvt_from_sint(types::F64, b);
                let cc = match op {
                    IntrinOp::Lt2 => FloatCC::LessThan,
                    IntrinOp::Le2 => FloatCC::LessThanOrEqual,
                    IntrinOp::Gt2 => FloatCC::GreaterThan,
                    _ => FloatCC::GreaterThanOrEqual,
                };
                (self.b.ins().fcmp(cc, fa, fb), None, true)
            }
        };
        if let Some(of) = of {
            let wr = self.b.create_block();
            self.b.ins().brif(of, slow, &[], wr, &[]);
            self.b.seal_block(wr);
            self.b.switch_to_block(wr);
        }
        let dst = self.dst_addr(d);
        self.drop_old(l, dst);
        if is_bool {
            let p = self.b.ins().uextend(I64, r);
            self.store_imm(l, dst, l.bool_tag, p);
        } else {
            self.store_imm(l, dst, l.int_tag, r);
        }
        self.b.ins().jump(merge, &[]);
    }
}
