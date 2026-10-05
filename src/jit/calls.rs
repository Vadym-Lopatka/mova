//! G2a (docs/JIT.md "Threaded tier (G1)" follow-up): the threaded tier's ONE
//! call helper, `jit_t_call`, for `Ir::CallGlobal` and an `Ir::Call` whose
//! callee is a `LoadSlot`/`LoadSlotTake`/`Const` (see `threaded::LowerCtx::
//! lower_call`). Kept in its own file (not `threaded.rs`) so a sibling
//! branch lowering leaf/arith nodes in `jit/leaf.rs` never touches the same
//! file.
//!
//! The win over the ordinary `exec_call`/`finish_call` path is NOT a
//! hand-rolled inline call: it is that the arguments already live in
//! consecutive slots of the SAME per-invocation `slots` array the whole
//! compiled body uses (lowered exactly like any other sub-expression, via
//! `LowerCtx::lower_into`), so this helper hands them to
//! `Interp::apply_value_slice` as a borrowed `&mut [Value]` -- no
//! `take_buf`/`push`/`put_buf` round trip through the interpreter's small-Vec
//! pool (`exec_args`'s own cost, per docs/JIT.md's profile note) for EITHER
//! a native call or a compiled-closure call. `apply_value_slice` is the
//! exact same seam `reduce`/`reduce-kv` already use for their stack-array
//! calls, so every check `apply_closure_buf` makes (arity, depth, fuel,
//! frame, ns swap, profile hooks, D9 coercion, the E1 int fast path) runs
//! unmodified -- this file never duplicates that logic, only skips the
//! Vec.
//!
//! `CallSiteIc` adds a SECOND, smaller win for `CallGlobal`: a resolved
//! non-dynamic global is cached (mirrors `jit::CallIc`/`jit_call_miss`'s own
//! epoch convention) so a repeat call skips `GlobalChain::get`'s `RwLock`
//! read and the macro check, as long as nothing has been `def`d anywhere
//! since (`DEF_EPOCH`). A `binding`-hinted var is NEVER cached (checked via
//! `is_dyn_hinted`, same guard `jit_call_miss` uses) -- it is re-resolved,
//! correctly, on every call.

use std::sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use super::{TCtx, DEF_EPOCH, PROTO_EPOCH, THREADED_OUT};
use crate::compile::ir::{GlobalChain, Ir};
use crate::error::RjError;
use crate::eval::Interp;
use crate::reader::Span;
use crate::types::TypeDef;
use crate::value::{NativeFn, Symbol, Value};

/// One cached global (K1): immutable `f` + a re-validatable epoch. Leaked
/// and never freed, so a running call may borrow `f` with no refcount; a
/// refill with the SAME fn only bumps `epoch` (no new leak).
struct Target {
    epoch: AtomicU64,
    f: Value,
    /// K3: resolved fast entry for this site's argc (null until first fast call).
    fast: AtomicPtr<FastInfo>,
}

/// K3: `fast_target`'s answer for (`f`, site argc), cached once; all fields borrow the leaked `f`.
struct FastInfo {
    rc: *const Arc<crate::value::Closure>,
    cc: *const crate::compile::CompiledClosure,
    entry: super::ThreadedEntry,
    n: usize,
    idx: usize,
}

/// One `CallGlobal` call site's cache (`Box::leak`ed at lowering time).
struct CallSiteIc {
    cur: AtomicPtr<Target>,
    /// K3: polymorphic protocol-dispatch IC (record type -> impl closure).
    proto: [AtomicPtr<ProtoTarget>; PROTO_WAYS],
    proto_fills: AtomicU32,
}

const PROTO_WAYS: usize = 4;
const PROTO_MAX_FILLS: u32 = 64;

/// K3: one cached protocol impl; leaked (a running call may borrow `f`), pins `tdef` against ABA.
struct ProtoTarget {
    epoch: u64,
    native: *const NativeFn,
    protos: usize,
    tdef: Arc<TypeDef>,
    f: Value,
    /// K3: `f`'s cached fast entry for this site's argc.
    fast: AtomicPtr<FastInfo>,
}

impl CallSiteIc {
    fn leak() -> &'static CallSiteIc {
        Box::leak(Box::new(CallSiteIc {
            cur: AtomicPtr::new(std::ptr::null_mut()),
            proto: Default::default(),
            proto_fills: AtomicU32::new(0),
        }))
    }
}

/// `threaded::LowerCtx::lower_call`'s hook: one leaked `CallSiteIc` per
/// `CallGlobal`/simple-callee `Call` site, as a bare address so `threaded.rs`
/// never needs this module's private type.
pub(super) fn leak_site_addr() -> i64 {
    let a = fresh_site();
    super::aot::note_fresh(a, fresh_site);
    a as i64
}

// S3: also the image restore's factory for this site kind.
fn fresh_site() -> usize {
    CallSiteIc::leak() as *const CallSiteIc as usize
}

enum Callee<'a> {
    Borrowed(&'a Value, &'a Target),
    Owned(Value),
}

/// The threaded tier's one call helper (docs/JIT.md). `slots[arg_base..
/// arg_base+argc]` are temps this site alone owns; for an `Ir::Call` the
/// callee's temp is `arg_base - 1`. K1: a closure with a fast entry is
/// entered directly (`Interp::fast_invoke`), result written straight to `d`.
pub(super) extern "C" fn jit_t_call(
    ctx: *mut TCtx,
    site: *const (),
    node: *const Ir,
    arg_base: u32,
    argc: u32,
    d: u32,
    borrowed: u32,
) -> u32 {
    unsafe {
        let ctxr = &mut *ctx;
        let interp = &mut *ctxr.interp;
        let ir = &*node;
        let site = &*(site as *const CallSiteIc);
        let args_ptr = ctxr.slots.add(arg_base as usize);
        let (callee, span) = match ir {
            Ir::CallGlobal { chain, sym, sym_span, span, .. } => {
                match resolve_global(interp, site, chain, sym, *sym_span, *span) {
                    Ok(c) => (c, *span),
                    Err(e) => {
                        forget_borrowed(args_ptr, borrowed);
                        ctxr.err = Some(e);
                        return 2;
                    }
                }
            }
            Ir::Call { span, .. } => {
                // A temp owned by this call site: MOVED, not cloned.
                let callee_slot = arg_base as usize - 1;
                (Callee::Owned(std::mem::replace(&mut *ctxr.slots.add(callee_slot), Value::Nil)), *span)
            }
            _ => unreachable!("jit_t_call only ever lowers CallGlobal/simple-callee Call nodes"),
        };
        let f: &Value = match &callee {
            Callee::Borrowed(f, _) => f,
            Callee::Owned(f) => f,
        };
        let argc = argc as usize;
        let dst: *mut Value = if d == THREADED_OUT { &mut ctxr.out } else { ctxr.slots.add(d as usize) };
        if borrowed != 0 {
            // K6: aliased local args -- a plain `&[Value]` native reads them in place; else own them now.
            if let Value::Native(nf) = f {
                if nf.consuming.is_none() && !(argc >= 1 && matches!(&*args_ptr, Value::Inst(_))) {
                    crate::profile::push(&nf.name);
                    let out = (nf.f)(interp, std::slice::from_raw_parts(args_ptr, argc));
                    crate::profile::pop();
                    forget_borrowed(args_ptr, borrowed);
                    return match out {
                        Ok(v) => {
                            *dst = v;
                            0
                        }
                        Err(e) => {
                            ctxr.err = Some(interp.decorate_native_err(e, span));
                            2
                        }
                    };
                }
            }
            promote_borrowed(args_ptr, borrowed);
        }
        if let Callee::Borrowed(_, t) = &callee {
            let fi = t.fast.load(Ordering::Acquire);
            if !fi.is_null() {
                if let Some(st) = enter_cached(interp, &*fi, args_ptr, argc, span, dst, &mut ctxr.err) {
                    return st;
                }
            } else if let Value::Fn(rc) = f {
                fill_fast(t, rc, argc);
            }
        }
        if let Value::Fn(rc) = f {
            if let Some(st) = try_fast(interp, rc, args_ptr, argc, span, dst, &mut ctxr.err) {
                return st;
            }
        }
        if let Value::Native(nf) = f {
            if argc >= 1 {
                if let Some(st) = try_proto(interp, site, nf, args_ptr, argc, span, dst, &mut ctxr.err) {
                    return st;
                }
            }
        }
        let args: &mut [Value] = std::slice::from_raw_parts_mut(args_ptr, argc);
        match interp.apply_value_slice(f, args, span) {
            Ok(v) => {
                *dst = v;
                0
            }
            Err(e) => {
                ctxr.err = Some(e);
                2
            }
        }
    }
}

/// K6: drop the aliases without touching refcounts (the locals still own them).
#[inline(always)]
unsafe fn forget_borrowed(args: *mut Value, mut m: u32) {
    while m != 0 {
        let i = m.trailing_zeros() as usize;
        std::ptr::write(args.add(i), Value::Nil);
        m &= m - 1;
    }
}

/// K6: turn each alias into a real owned clone in place.
#[inline(always)]
unsafe fn promote_borrowed(args: *mut Value, mut m: u32) {
    while m != 0 {
        let i = m.trailing_zeros() as usize;
        let v = (*args.add(i)).clone();
        std::ptr::write(args.add(i), v);
        m &= m - 1;
    }
}

/// K1 native -> native: arity select, fast gates, then `fast_invoke` with
/// the args MOVED from the caller's temps. `None` = use the old path (which
/// also owns every error: arity, overflow, E1 int entry).
#[inline(always)]
unsafe fn try_fast(
    interp: &mut Interp,
    rc: &Arc<crate::value::Closure>,
    args: *mut Value,
    argc: usize,
    span: Span,
    dst: *mut Value,
    err: &mut Option<RjError>,
) -> Option<u32> {
    let idx = crate::eval::apply::select_arity_index(&rc.arities, argc)?;
    let (cc, entry, n) = Interp::fast_target(rc, idx, argc)?;
    if interp.fuel.is_some() || interp.intr_armed || interp.call_depth() > interp.max_depth || !interp.moveargs_on() {
        return None;
    }
    // E1 (int entry) keeps priority exactly as in `apply_closure_buf`.
    let a = &cc.code.arities[idx];
    if !a.jit.is_disabled() && std::slice::from_raw_parts(args, argc).iter().all(|v| matches!(v, Value::Int(_))) {
        return None;
    }
    let fill = |base: *mut Value| {
        std::ptr::copy_nonoverlapping(args, base, argc);
        for i in 0..argc {
            std::ptr::write(args.add(i), Value::Nil);
        }
    };
    Some(interp.fast_invoke(rc, cc, entry, n, span, fill, dst, err))
}

/// K3: cache `fast_target` for a leaked target (process-constant gates only).
#[cold]
fn fill_fast(t: &Target, rc: &Arc<crate::value::Closure>, argc: usize) {
    let Some(idx) = crate::eval::apply::select_arity_index(&rc.arities, argc) else { return };
    let Some((cc, entry, n)) = Interp::fast_target(rc, idx, argc) else { return };
    let fi = Box::leak(Box::new(FastInfo { rc, cc, entry, n, idx }));
    t.fast.store(fi, Ordering::Release);
}

/// K3 direct call: the cached entry, only the per-call gates re-checked.
#[inline(always)]
unsafe fn enter_cached(
    interp: &mut Interp,
    fi: &FastInfo,
    args: *mut Value,
    argc: usize,
    span: Span,
    dst: *mut Value,
    err: &mut Option<RjError>,
) -> Option<u32> {
    if interp.fuel.is_some() || interp.intr_armed || interp.call_depth() > interp.max_depth || !interp.moveargs_on() {
        return None;
    }
    let cc = &*fi.cc;
    let a = &cc.code.arities[fi.idx];
    if !a.jit.is_disabled() && std::slice::from_raw_parts(args, argc).iter().all(|v| matches!(v, Value::Int(_))) {
        return None;
    }
    let fill = |base: *mut Value| {
        std::ptr::copy_nonoverlapping(args, base, argc);
        for i in 0..argc {
            std::ptr::write(args.add(i), Value::Nil);
        }
    };
    Some(interp.fast_invoke(&*fi.rc, cc, fi.entry, fi.n, span, fill, dst, err))
}

/// K3: protocol dispatch fn called on a named record/deftype: per-site IC
/// (PROTO_EPOCH, dispatch fn, registry, type) -> impl closure, entered via
/// `try_fast`. `None` = old path (reify, meta, non-Inst, miss not cacheable).
#[allow(clippy::too_many_arguments)]
#[inline(always)]
unsafe fn try_proto(
    interp: &mut Interp,
    site: &CallSiteIc,
    nf: &Arc<NativeFn>,
    args: *mut Value,
    argc: usize,
    span: Span,
    dst: *mut Value,
    err: &mut Option<RjError>,
) -> Option<u32> {
    let Value::Inst(inst) = &*args else { return None };
    let now = PROTO_EPOCH.load(Ordering::Acquire);
    let protos = Arc::as_ptr(&interp.protocols.0) as usize;
    let tdef = Arc::as_ptr(&inst.tdef);
    let mut hit: Option<&'static ProtoTarget> = None;
    for w in &site.proto {
        let p = w.load(Ordering::Acquire);
        if p.is_null() {
            break;
        }
        let t: &'static ProtoTarget = &*p;
        if t.epoch == now && Arc::as_ptr(&t.tdef) == tdef && t.native == Arc::as_ptr(nf) && t.protos == protos {
            hit = Some(t);
            break;
        }
    }
    let t = match hit {
        Some(t) => t,
        None => proto_miss(interp, site, nf, &*args, argc, now, protos)?,
    };
    let Value::Fn(rc) = &t.f else { return None };
    #[cfg(feature = "k2-count")]
    crate::k2count::PROTO_CALLS.fetch_add(1, Ordering::Relaxed);
    // Old path: native dispatch applies the impl with a zero span, then decorates errors.
    let zero = Span { start: 0, end: 0 };
    let fi = t.fast.load(Ordering::Acquire);
    let st = if fi.is_null() {
        if let Some(idx) = crate::eval::apply::select_arity_index(&rc.arities, argc) {
            if let Some((cc, entry, n)) = Interp::fast_target(rc, idx, argc) {
                t.fast.store(Box::leak(Box::new(FastInfo { rc, cc, entry, n, idx })), Ordering::Release);
            }
        }
        try_fast(interp, rc, args, argc, zero, dst, err)?
    } else {
        enter_cached(interp, &*fi, args, argc, zero, dst, err)?
    };
    if st == 2 {
        if let Some(e) = err.take() {
            *err = Some(interp.decorate_native_err(e, span));
        }
    }
    Some(st)
}

#[cold]
fn proto_miss(
    interp: &mut Interp,
    site: &CallSiteIc,
    nf: &Arc<NativeFn>,
    target: &Value,
    argc: usize,
    now: u64,
    protos: usize,
) -> Option<&'static ProtoTarget> {
    let Some(crate::image::Recipe::Proto { mname, arity, cell, epoch, midx, .. }) = nf.image_recipe.as_deref() else {
        return None;
    };
    let Value::Inst(inst) = target else { return None };
    if argc < arity.0 || argc > arity.1 || !inst.tdef.methods.is_empty() {
        return None;
    }
    let fills = site.proto_fills.load(Ordering::Relaxed);
    if fills >= PROTO_MAX_FILLS {
        return None;
    }
    let key = Arc::as_ptr(cell) as usize;
    // Cache only what `lookup_method` itself would cache (current-epoch protocol).
    let cur_epoch = crate::sync::lock_read(&interp.protocols.0).get(&key).map(|p| p.epoch);
    if cur_epoch != Some(*epoch) {
        return None;
    }
    let f = crate::builtins::types::lookup_method(&interp.protocols, key, *epoch, *midx, target, mname)?;
    if !matches!(f, Value::Fn(_)) {
        return None;
    }
    let t: &'static ProtoTarget = Box::leak(Box::new(ProtoTarget {
        epoch: now,
        native: Arc::as_ptr(nf),
        protos,
        tdef: inst.tdef.clone(),
        f,
        fast: AtomicPtr::new(std::ptr::null_mut()),
    }));
    site.proto_fills.store(fills + 1, Ordering::Relaxed);
    site.proto[fills as usize % PROTO_WAYS].store(t as *const ProtoTarget as *mut ProtoTarget, Ordering::Release);
    Some(t)
}

/// `CallGlobal`'s resolution: exactly `exec_call_global`/`finish_call`'s
/// head-first steps (same errors, same spans), plus the IC. A `binding`-
/// hinted var is never cached (`is_dyn_hinted`, as in `jit_call_miss`).
fn resolve_global<'a>(
    interp: &mut Interp,
    site: &'a CallSiteIc,
    chain: &GlobalChain,
    sym: &Symbol,
    sym_span: Span,
    span: Span,
) -> Result<Callee<'a>, RjError> {
    let now = DEF_EPOCH.load(Ordering::Acquire);
    let cur = site.cur.load(Ordering::Acquire);
    if !cur.is_null() {
        let t: &'a Target = unsafe { &*cur };
        if t.epoch.load(Ordering::Acquire) == now {
            return Ok(Callee::Borrowed(&t.f, t));
        }
    }
    let f = chain.get().ok_or_else(|| crate::compile::exec::unresolved(interp, sym, sym_span))?;
    if let Value::Macro(_) = f {
        return Err(crate::compile::exec::late_macro(interp, sym, span));
    }
    if chain.resolved().is_dyn_hinted() {
        return Ok(Callee::Owned(f));
    }
    if !cur.is_null() {
        let t: &'a Target = unsafe { &*cur };
        if same_value(&t.f, &f) {
            t.epoch.store(now, Ordering::Release);
            return Ok(Callee::Borrowed(&t.f, t));
        }
    }
    let t: &'static Target = Box::leak(Box::new(Target { epoch: AtomicU64::new(now), f, fast: AtomicPtr::new(std::ptr::null_mut()) }));
    site.cur.store(t as *const Target as *mut Target, Ordering::Release);
    Ok(Callee::Borrowed(&t.f, t))
}

/// Identity (not equality): the same fn object.
fn same_value(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Fn(x), Value::Fn(y)) => Arc::ptr_eq(x, y),
        (Value::Native(x), Value::Native(y)) => Arc::ptr_eq(x, y),
        _ => false,
    }
}
