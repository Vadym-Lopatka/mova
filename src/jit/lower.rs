//! Cranelift lowering for the E1a/E1b subset (docs/JIT.md). One global
//! `JITModule` behind a `Mutex` -- compiling a fn is rare, so contention
//! never matters; the generated code (and every `Box::leak`ed inline cache
//! or `GlobalChain` clone a call site needs) is kept alive forever
//! (`static`/`'static`), so raw addresses embedded in generated code as
//! `iconst` never dangle.
//!
//! Two passes: [`supported`] walks the already-compiled `Ir` tree in plain
//! Rust and answers yes/no with NO side effects on the module, so a
//! rejected arity never touches Cranelift at all. Only when it says yes
//! does [`lower_arity`] build the function, and that pass may assume every
//! node it sees is one `supported` already approved.

use std::sync::atomic::Ordering;
use std::sync::{Mutex, OnceLock};

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{
    AbiParam, Block, Function, InstBuilder, MemFlagsData, Signature, StackSlotData, StackSlotKind,
    UserFuncName, Value as ClifValue,
};
use cranelift_codegen::isa::CallConv;
use cranelift_codegen::Context;
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{FuncId, Linkage, Module};

use super::{layout, CallIc, GenericEntry, JitCtx, NativeEntry, DEF_EPOCH};
use crate::compile::ir::{CaptureSrc, CompiledPattern, GlobalChain, IntrinOp, Ir};
use crate::compile::CompiledFn;
use crate::error::RjError;
use crate::reader::Span;
use crate::value::{Symbol, Value};

const I64: cranelift_codegen::ir::Type = cranelift_codegen::ir::types::I64;
const I32: cranelift_codegen::ir::Type = cranelift_codegen::ir::types::I32;

/// `a op b -> *out`, `0` ok / `1` overflow -- the exact `i64::checked_*`
/// mova's own `builtins::numbers::add/sub/mul` use for the `Int,Int` case
/// (the only case this subset ever sees; a non-Int operand never reaches
/// here because the arity is never entered unless every arg was
/// `Value::Int`, and the subset only ever combines params/consts/prior
/// results of these same ops).
extern "C" fn jit_checked_add(a: i64, b: i64, out: *mut i64) -> i32 {
    match a.checked_add(b) {
        Some(r) => {
            unsafe { *out = r };
            0
        }
        None => 1,
    }
}
extern "C" fn jit_checked_sub(a: i64, b: i64, out: *mut i64) -> i32 {
    match a.checked_sub(b) {
        Some(r) => {
            unsafe { *out = r };
            0
        }
        None => 1,
    }
}
extern "C" fn jit_checked_mul(a: i64, b: i64, out: *mut i64) -> i32 {
    match a.checked_mul(b) {
        Some(r) => {
            unsafe { *out = r };
            0
        }
        None => 1,
    }
}

/// E1b direct-call miss helper (docs/JIT.md): resolves `chain` through the
/// SAME `GlobalChain::get` the interpreter uses, accepts only a `Value::Fn`
/// whose compiled arity for `argc` has a native entry, no captures and no
/// param coercion, and returns its address (`0` => caller bails). Fills
/// `ic` with the current epoch UNLESS the resolved cell is currently
/// dynamically bound (`is_dyn_hinted`): a live `binding` can change the
/// visible value on every push/pop without a `DEF_EPOCH` bump, so such a
/// cell is resolved fresh on every call instead of being cached.
extern "C" fn jit_call_miss(ic: *const CallIc, chain: *const GlobalChain, argc: u32) -> usize {
    // SAFETY: both are `Box::leak`ed at lower time (`Lower::lower_call_global`)
    // and never freed -- see this module's doc.
    let ic = unsafe { &*ic };
    let chain = unsafe { &*chain };
    let entry = resolve_entry(chain, argc as usize);
    if entry != 0 && !chain.resolved().is_dyn_hinted() {
        ic.entry.store(entry, Ordering::Relaxed);
        ic.epoch.store(DEF_EPOCH.load(Ordering::Acquire), Ordering::Release);
    }
    entry
}

/// May lower the callee now (locks `state()`) if it is compiled but not yet
/// JIT'd -- safe because native code never itself holds that lock (by the
/// time it runs, its own `lower_arity` call has already returned and
/// dropped the guard).
fn resolve_entry(chain: &GlobalChain, argc: usize) -> usize {
    let Some(Value::Fn(rc)) = chain.get() else { return 0 };
    let Some(idx) = crate::eval::apply::select_arity_index(&rc.arities, argc) else { return 0 };
    // D9: a `^long`/`^double` hint coerces at the call boundary; the native
    // entry never lowered that cast (see docs/JIT.md's subset).
    if rc.arities[idx].coerce.is_some() {
        return 0;
    }
    let Some(cc) = rc.compiled.compiled() else { return 0 };
    if !cc.captures.is_empty() {
        return 0;
    }
    let arity = &cc.code.arities[idx];
    if arity.jit.is_disabled() {
        return 0;
    }
    match arity.jit.get_or_lower(&cc.code, idx) {
        Some(e) => entry_addr(e),
        None => 0,
    }
}

fn entry_addr(e: &NativeEntry) -> usize {
    match e {
        NativeEntry::A0(f) => *f as usize,
        NativeEntry::A1(f) => *f as usize,
        NativeEntry::A2(f) => *f as usize,
        NativeEntry::A3(f) => *f as usize,
        NativeEntry::A4(f) => *f as usize,
    }
}

/// E3a (docs/NATIVE-TIER-DESIGN.md #1/#2): clones `*src` into `*dst`,
/// establishing independent ownership (a real `Arc::clone` for a heap
/// variant, a plain copy for everything else) -- the ONE place the generic
/// tier turns a borrow into an owned value.
extern "C" fn jit_g_clone(src: *const Value, dst: *mut Value) {
    // SAFETY: `src` is a live `Value` for the call's duration (a param, a
    // leaked const, or an already-owned local slot); `dst` is
    // caller-provided, uninitialised, sized-for-`Value` memory.
    unsafe { std::ptr::write(dst, (*src).clone()) }
}

/// E3a: drops the `Value` at `v` in place. Cheap (no-op-ish, one branch) for
/// every immediate variant; a real `Arc` decrement only for a heap one.
extern "C" fn jit_g_drop(v: *mut Value) {
    // SAFETY: `v` points at a `Value` this arity's own bookkeeping has
    // proven independently owned (established via `jit_g_clone`, a helper's
    // fresh result, or a loop-carried slot's previous iteration).
    unsafe { std::ptr::drop_in_place(v) }
}

/// E3a op codes for [`jit_g_arith`] -- one dispatch helper instead of 13
/// separate `extern "C" fn`s, to keep the module-state/declare boilerplate
/// small. Order matches `IntrinOp` for `intrin_op_code`'s convenience only;
/// nothing outside this file depends on the numbering.
const OP_ADD: i32 = 0;
const OP_SUB2: i32 = 1;
const OP_MUL: i32 = 2;
const OP_DIV2: i32 = 3;
const OP_INC: i32 = 4;
const OP_DEC: i32 = 5;
const OP_LT2: i32 = 6;
const OP_LE2: i32 = 7;
const OP_GT2: i32 = 8;
const OP_GE2: i32 = 9;
const OP_EQ2: i32 = 10;
const OP_ZERO: i32 = 11;
const OP_NOT: i32 = 12;

/// E3a: the generic tier's ONE arithmetic/compare slow path -- calls the
/// EXACT SAME `builtins::numbers`/`predicates` fn the tree-walk interpreter
/// (`compile::exec::exec_intrinsic`) uses, wrapped in the same
/// `native_err` (span + stack snapshot), for byte-identical results and
/// errors. No inline Int fast path in this round (deviation, see
/// docs/JIT.md) -- every generic Intrinsic goes through here; the E1
/// int-only tier remains the fast path for pure-`Int` arities.
extern "C" fn jit_g_arith(
    ctx: *mut JitCtx,
    op: i32,
    a: *const Value,
    b: *const Value,
    span_start: i64,
    span_end: i64,
    out: *mut Value,
) -> u32 {
    use crate::builtins::{numbers, predicates};
    let ctx = unsafe { &mut *ctx };
    let interp = unsafe { &mut *ctx.interp };
    let a = unsafe { &*a };
    let b = unsafe { &*b };
    let span = Span { start: span_start as usize, end: span_end as usize };
    let result: Result<Value, RjError> = match op {
        OP_ADD => {
            if numbers::skips_identity(a) {
                numbers::add_step(interp, a, b)
            } else {
                numbers::add_step(interp, &Value::Int(0), a).and_then(|acc| numbers::add_step(interp, &acc, b))
            }
        }
        OP_MUL => {
            if numbers::skips_identity(a) {
                numbers::mul_step(interp, a, b)
            } else {
                numbers::mul_step(interp, &Value::Int(1), a).and_then(|acc| numbers::mul_step(interp, &acc, b))
            }
        }
        OP_SUB2 => numbers::sub2(interp, a, b),
        OP_DIV2 => numbers::div2(a, b),
        OP_INC => numbers::inc1(interp, a),
        OP_DEC => numbers::dec1(interp, a),
        OP_LT2 => numbers::lt2(a, b),
        OP_LE2 => numbers::le2(a, b),
        OP_GT2 => numbers::gt2(a, b),
        OP_GE2 => numbers::ge2(a, b),
        OP_EQ2 => interp.values_equal(a, b).map(Value::Bool),
        OP_ZERO => predicates::zero1(a),
        OP_NOT => Ok(predicates::not1(a)),
        _ => unreachable!("op codes are generated by this same file"),
    };
    match result.map_err(|e| crate::compile::exec::native_err(interp, e, span)) {
        Ok(v) => {
            unsafe { std::ptr::write(out, v) };
            0
        }
        Err(e) => {
            ctx.pending = Some(Box::new(e));
            2
        }
    }
}

/// One `CallGlobal` JIT call site's fixed (non-cached -- see this fn's
/// caller doc) resolution data, `Box::leak`ed at lower time exactly like
/// E1b's `GlobalChain`/`CallIc` leaks.
struct CallGlobalSite {
    chain: GlobalChain,
    sym: Symbol,
    sym_span: Span,
    span: Span,
}

/// E3a `CallGlobal`/generic-call helper (docs/NATIVE-TIER-DESIGN.md #5,
/// deviation noted in docs/JIT.md): resolves `site.chain` FRESH on every
/// call (no inline cache -- always correct under redefinition, including a
/// live `binding`, at the cost of the interpreter's own resolution cost)
/// and applies through `Interp::apply_value_owned`, the SAME function the
/// interpreter itself calls from `compile::exec::finish_call` -- so nested
/// frames/errors below this point are byte-identical to a fully
/// interpreted call by construction.
extern "C" fn jit_g_call_global(
    ctx: *mut JitCtx,
    site: *const CallGlobalSite,
    args: *const *const Value,
    argc: u32,
    out: *mut Value,
) -> u32 {
    let ctx = unsafe { &mut *ctx };
    let site = unsafe { &*site };
    let interp = unsafe { &mut *ctx.interp };
    let f = match site.chain.get() {
        Some(v) => v,
        None => {
            ctx.pending = Some(Box::new(crate::compile::exec::unresolved(interp, &site.sym, site.sym_span)));
            return 2;
        }
    };
    if let Value::Macro(_) = f {
        ctx.pending = Some(Box::new(crate::compile::exec::late_macro(interp, &site.sym, site.span)));
        return 2;
    }
    // SAFETY: `args[0..argc]` are live `*const Value` for the call's
    // duration; cloning is this path's own documented cost (docs/
    // NATIVE-TIER-DESIGN.md #5: "this path is already the slow class").
    let argv: Vec<Value> = (0..argc as usize).map(|i| unsafe { (*(*args.add(i))).clone() }).collect();
    match interp.apply_value_owned(&f, argv, site.span) {
        Ok(v) => {
            unsafe { std::ptr::write(out, v) };
            0
        }
        Err(e) => {
            ctx.pending = Some(Box::new(e));
            2
        }
    }
}

/// F1: `Ir::Call`'s (computed-callee call, e.g. `(:k m)`/`((if a f g) x)`)
/// generic-tier slow path -- exactly `compile::exec::exec_call`'s own body
/// (`f = callee value, argv = args, apply_value_owned`), no macro check
/// (matching `exec_call`, which has none either: a computed callee is never
/// a `Value::Macro` in practice, only a resolved global symbol can be).
extern "C" fn jit_g_call(
    ctx: *mut JitCtx,
    callee: *const Value,
    args: *const *const Value,
    argc: u32,
    span_start: i64,
    span_end: i64,
    out: *mut Value,
) -> u32 {
    let ctx = unsafe { &mut *ctx };
    let interp = unsafe { &mut *ctx.interp };
    let f = unsafe { (*callee).clone() };
    let span = Span { start: span_start as usize, end: span_end as usize };
    // SAFETY: same as `jit_g_call_global`'s identical loop.
    let argv: Vec<Value> = (0..argc as usize).map(|i| unsafe { (*(*args.add(i))).clone() }).collect();
    match interp.apply_value_owned(&f, argv, span) {
        Ok(v) => {
            unsafe { std::ptr::write(out, v) };
            0
        }
        Err(e) => {
            ctx.pending = Some(Box::new(e));
            2
        }
    }
}

/// F1: `Ir::Throw`'s generic-tier slow path -- exactly `exec_throw`'s own
/// body (`RjError::thrown(v).with_span(..).with_stack(..)`). Always fails
/// (status 2); there is no ok path, matching `throw`'s own type (`Ir` has
/// no separate "never returns" marker, so the caller just treats this like
/// any other fallible helper).
extern "C" fn jit_g_throw(ctx: *mut JitCtx, value: *const Value, span_start: i64, span_end: i64) -> u32 {
    let ctx = unsafe { &mut *ctx };
    let interp = unsafe { &mut *ctx.interp };
    let v = unsafe { (*value).clone() };
    let span = Span { start: span_start as usize, end: span_end as usize };
    let e = RjError::thrown(v).with_span(span).with_stack(interp.stack_snapshot(), interp.source_id);
    ctx.pending = Some(Box::new(e));
    2
}

/// F1: one `Ir::Escape` site's leaked bridge data -- same leak convention
/// as `CallGlobalSite`, a fresh `Escape` clone rather than a pointer into
/// the live `Ir` tree.
struct EscapeSite(crate::compile::ir::Escape);

/// F1: one `Ir::GlobalRef` site's leaked resolution data (a bare global
/// reference, e.g. a fn passed BY VALUE rather than called).
struct GlobalRefSite {
    chain: GlobalChain,
    sym: Symbol,
    span: Span,
}

/// F1: `Ir::GlobalRef`'s generic-tier slow path -- exactly `exec.rs`'s own
/// arm (`chain.get()`, or the same `unresolved` error), so the resulting
/// `Value` (typically a `Value::Fn`, handed onward as an ordinary arg) is
/// byte-identical.
extern "C" fn jit_g_global_ref(ctx: *mut JitCtx, site: *const GlobalRefSite, out: *mut Value) -> u32 {
    let ctx = unsafe { &mut *ctx };
    let site = unsafe { &*site };
    let interp = unsafe { &mut *ctx.interp };
    match site.chain.get() {
        Some(v) => {
            unsafe { std::ptr::write(out, v) };
            0
        }
        None => {
            ctx.pending = Some(Box::new(crate::compile::exec::unresolved(interp, &site.sym, site.span)));
            2
        }
    }
}

/// F1 (docs/NATIVE-TIER-DESIGN.md #2, deviation noted in docs/JIT.md):
/// `Ir::Escape`'s generic-tier slow path -- tree-walks the SAME verbatim
/// form `compile::exec::exec_escape` does, via the SAME `Interp::eval_form_in`,
/// so results/errors/side effects are byte-identical. The bridge env's
/// parent is `interp.globals` (the root env) rather than the real creation
/// env `exec_escape` uses: exact for every escape `supported_g_expr` admits
/// (`CaptureSrc::Slot` binds only), because any name `resolve_lexical`
/// could NOT resolve lexically inside this fn is -- by the same argument
/// `compile_escape`'s own doc gives -- a global, findable from any env
/// whose chain reaches root.
extern "C" fn jit_g_escape(
    ctx: *mut JitCtx,
    site: *const EscapeSite,
    slot_ptrs: *const *const Value,
    out: *mut Value,
) -> u32 {
    let ctx = unsafe { &mut *ctx };
    let site = unsafe { &(*site).0 };
    let interp = unsafe { &mut *ctx.interp };
    let env = interp.globals.child();
    for (i, (sym, _)) in site.binds.iter().enumerate() {
        // SAFETY: `slot_ptrs[0..binds.len()]` are live slot addresses for
        // the call's duration (`lower_g_escape` fills them from this same
        // arity's own stack slots).
        let v = unsafe { (**slot_ptrs.add(i)).clone() };
        env.set(sym.clone(), v);
    }
    match interp.eval_form_in(&site.form, &env) {
        Ok(v) => {
            unsafe { std::ptr::write(out, v) };
            0
        }
        Err(e) => {
            ctx.pending = Some(Box::new(e));
            2
        }
    }
}

struct JitModuleState {
    module: JITModule,
    add_id: FuncId,
    sub_id: FuncId,
    mul_id: FuncId,
    call_miss_id: FuncId,
    counter: u64,
    /// E3a generic-tier helpers -- see their doc comments below.
    g_arith_id: FuncId,
    g_call_global_id: FuncId,
    g_clone_id: FuncId,
    g_drop_id: FuncId,
    g_escape_id: FuncId,
    g_global_ref_id: FuncId,
    g_call_id: FuncId,
    g_throw_id: FuncId,
}

static STATE: OnceLock<Mutex<JitModuleState>> = OnceLock::new();

fn state() -> &'static Mutex<JitModuleState> {
    STATE.get_or_init(|| {
        let mut builder = JITBuilder::new(cranelift_module::default_libcall_names())
            .expect("host machine not supported by cranelift-native");
        builder.symbol("mova_jit_add", jit_checked_add as *const u8);
        builder.symbol("mova_jit_sub", jit_checked_sub as *const u8);
        builder.symbol("mova_jit_mul", jit_checked_mul as *const u8);
        builder.symbol("mova_jit_call_miss", jit_call_miss as *const u8);
        builder.symbol("mova_jit_g_arith", jit_g_arith as *const u8);
        builder.symbol("mova_jit_g_call_global", jit_g_call_global as *const u8);
        builder.symbol("mova_jit_g_clone", jit_g_clone as *const u8);
        builder.symbol("mova_jit_g_drop", jit_g_drop as *const u8);
        builder.symbol("mova_jit_g_escape", jit_g_escape as *const u8);
        builder.symbol("mova_jit_g_global_ref", jit_g_global_ref as *const u8);
        builder.symbol("mova_jit_g_call", jit_g_call as *const u8);
        builder.symbol("mova_jit_g_throw", jit_g_throw as *const u8);
        let mut module = JITModule::new(builder);
        let ptr_ty = module.target_config().pointer_type();
        let mut helper_sig = module.make_signature();
        helper_sig.params.push(AbiParam::new(I64));
        helper_sig.params.push(AbiParam::new(I64));
        helper_sig.params.push(AbiParam::new(ptr_ty));
        helper_sig.returns.push(AbiParam::new(I32));
        let add_id = module.declare_function("mova_jit_add", Linkage::Import, &helper_sig).expect("declare mova_jit_add");
        let sub_id = module.declare_function("mova_jit_sub", Linkage::Import, &helper_sig).expect("declare mova_jit_sub");
        let mul_id = module.declare_function("mova_jit_mul", Linkage::Import, &helper_sig).expect("declare mova_jit_mul");

        let mut miss_sig = module.make_signature();
        miss_sig.params.push(AbiParam::new(ptr_ty)); // ic*
        miss_sig.params.push(AbiParam::new(ptr_ty)); // chain*
        miss_sig.params.push(AbiParam::new(I32)); // argc
        miss_sig.returns.push(AbiParam::new(ptr_ty)); // entry (0 = bail)
        let call_miss_id = module
            .declare_function("mova_jit_call_miss", Linkage::Import, &miss_sig)
            .expect("declare mova_jit_call_miss");

        // E3a helper signatures (docs/NATIVE-TIER-DESIGN.md #2/#5).
        let mut arith_sig = module.make_signature();
        arith_sig.params.push(AbiParam::new(ptr_ty)); // ctx*
        arith_sig.params.push(AbiParam::new(I32)); // op
        arith_sig.params.push(AbiParam::new(ptr_ty)); // a*
        arith_sig.params.push(AbiParam::new(ptr_ty)); // b*
        arith_sig.params.push(AbiParam::new(I64)); // span.start
        arith_sig.params.push(AbiParam::new(I64)); // span.end
        arith_sig.params.push(AbiParam::new(ptr_ty)); // out*
        arith_sig.returns.push(AbiParam::new(I32));
        let g_arith_id = module.declare_function("mova_jit_g_arith", Linkage::Import, &arith_sig).expect("declare mova_jit_g_arith");

        let mut cg_sig = module.make_signature();
        cg_sig.params.push(AbiParam::new(ptr_ty)); // ctx*
        cg_sig.params.push(AbiParam::new(ptr_ty)); // site*
        cg_sig.params.push(AbiParam::new(ptr_ty)); // args: *const *const Value
        cg_sig.params.push(AbiParam::new(I32)); // argc
        cg_sig.params.push(AbiParam::new(ptr_ty)); // out*
        cg_sig.returns.push(AbiParam::new(I32));
        let g_call_global_id = module
            .declare_function("mova_jit_g_call_global", Linkage::Import, &cg_sig)
            .expect("declare mova_jit_g_call_global");

        let mut clone_sig = module.make_signature();
        clone_sig.params.push(AbiParam::new(ptr_ty)); // src*
        clone_sig.params.push(AbiParam::new(ptr_ty)); // dst*
        let g_clone_id = module.declare_function("mova_jit_g_clone", Linkage::Import, &clone_sig).expect("declare mova_jit_g_clone");

        let mut drop_sig = module.make_signature();
        drop_sig.params.push(AbiParam::new(ptr_ty)); // v*
        let g_drop_id = module.declare_function("mova_jit_g_drop", Linkage::Import, &drop_sig).expect("declare mova_jit_g_drop");

        let mut escape_sig = module.make_signature();
        escape_sig.params.push(AbiParam::new(ptr_ty)); // ctx*
        escape_sig.params.push(AbiParam::new(ptr_ty)); // site*
        escape_sig.params.push(AbiParam::new(ptr_ty)); // slot_ptrs: *const *const Value
        escape_sig.params.push(AbiParam::new(ptr_ty)); // out*
        escape_sig.returns.push(AbiParam::new(I32));
        let g_escape_id = module.declare_function("mova_jit_g_escape", Linkage::Import, &escape_sig).expect("declare mova_jit_g_escape");

        let mut gref_sig = module.make_signature();
        gref_sig.params.push(AbiParam::new(ptr_ty)); // ctx*
        gref_sig.params.push(AbiParam::new(ptr_ty)); // site*
        gref_sig.params.push(AbiParam::new(ptr_ty)); // out*
        gref_sig.returns.push(AbiParam::new(I32));
        let g_global_ref_id = module.declare_function("mova_jit_g_global_ref", Linkage::Import, &gref_sig).expect("declare mova_jit_g_global_ref");

        let mut call_sig = module.make_signature();
        call_sig.params.push(AbiParam::new(ptr_ty)); // ctx*
        call_sig.params.push(AbiParam::new(ptr_ty)); // callee*
        call_sig.params.push(AbiParam::new(ptr_ty)); // args: *const *const Value
        call_sig.params.push(AbiParam::new(I32)); // argc
        call_sig.params.push(AbiParam::new(I64)); // span.start
        call_sig.params.push(AbiParam::new(I64)); // span.end
        call_sig.params.push(AbiParam::new(ptr_ty)); // out*
        call_sig.returns.push(AbiParam::new(I32));
        let g_call_id = module.declare_function("mova_jit_g_call", Linkage::Import, &call_sig).expect("declare mova_jit_g_call");

        let mut throw_sig = module.make_signature();
        throw_sig.params.push(AbiParam::new(ptr_ty)); // ctx*
        throw_sig.params.push(AbiParam::new(ptr_ty)); // value*
        throw_sig.params.push(AbiParam::new(I64)); // span.start
        throw_sig.params.push(AbiParam::new(I64)); // span.end
        throw_sig.returns.push(AbiParam::new(I32));
        let g_throw_id = module.declare_function("mova_jit_g_throw", Linkage::Import, &throw_sig).expect("declare mova_jit_g_throw");

        Mutex::new(JitModuleState {
            module,
            add_id,
            sub_id,
            mul_id,
            call_miss_id,
            counter: 0,
            g_arith_id,
            g_call_global_id,
            g_clone_id,
            g_drop_id,
            g_escape_id,
            g_global_ref_id,
            g_call_id,
            g_throw_id,
        })
    })
}

/// A `recur` target: `scratch_base` identifies it (same value `Ir::Recur`
/// carries), `head_block` is the Cranelift loop head to jump back to.
struct LoopCtx {
    scratch_base: u16,
    head_block: Block,
}

/// Whether `ir` is one of the boolean-PRODUCING intrinsics -- the only
/// place a real two-way branch is generated. Shared by `supported`'s check
/// and lowering so both agree on which `If`s are real conditionals versus
/// bare-Int tests (see `supported_expr`'s `If` arm).
fn is_bool_test(ir: &Ir) -> bool {
    matches!(ir, Ir::Intrinsic { op, chain, .. }
        if chain.intrinsic_armed()
            && matches!(op, IntrinOp::Lt2 | IntrinOp::Le2 | IntrinOp::Gt2 | IntrinOp::Ge2 | IntrinOp::Eq2 | IntrinOp::Zero | IntrinOp::Not))
}

/// Pass 1: is this arity entirely inside the E1a/E1b subset (docs/JIT.md)?
/// Checked with NO Cranelift touched, so "no" costs nothing but a tree walk.
fn supported(body: &[Ir], n_params: usize, scratch_base: u16) -> bool {
    let mut targets = vec![(scratch_base, n_params)];
    !body.is_empty() && supported_tail_seq(body, n_params, &mut targets)
}

/// A statement sequence where only the LAST element may be in tail
/// position (an ordinary `Recur` never appears anywhere else -- a non-tail
/// `recur` is out of scope for this JIT, see docs/JIT.md point 1).
fn supported_tail_seq(exprs: &[Ir], n_params: usize, targets: &mut Vec<(u16, usize)>) -> bool {
    let Some((last, init)) = exprs.split_last() else { return false };
    init.iter().all(|e| supported_expr(e, n_params)) && supported_tail(last, n_params, targets)
}

fn supported_tail(ir: &Ir, n_params: usize, targets: &mut Vec<(u16, usize)>) -> bool {
    match ir {
        Ir::If { test, then, els } => {
            if is_bool_test(test) {
                let Some(els) = els else { return false };
                supported_bool(test, n_params) && supported_tail(then, n_params, targets) && supported_tail(els, n_params, targets)
            } else {
                // Bare Int test: every value here is an Int and every Int
                // is truthy in Mova, so this `If` always takes `then` --
                // `els` is unreachable and need not even be checked.
                supported_expr(test, n_params) && supported_tail(then, n_params, targets)
            }
        }
        Ir::Do(exprs) => supported_tail_seq(exprs, n_params, targets),
        Ir::Let { binds, body } => {
            binds.iter().all(|(pat, init)| matches!(pat, CompiledPattern::Slot(_)) && supported_expr(init, n_params))
                && supported_tail_seq(body, n_params, targets)
        }
        Ir::Loop { binds, scratch_base, body } => {
            if !binds.iter().all(|(pat, init)| matches!(pat, CompiledPattern::Slot(_)) && supported_expr(init, n_params)) {
                return false;
            }
            targets.push((*scratch_base, binds.len()));
            let ok = supported_tail_seq(body, n_params, targets);
            targets.pop();
            ok
        }
        Ir::Recur { args, scratch_base } => {
            targets.iter().rev().any(|(sb, arity)| sb == scratch_base && *arity == args.len())
                && args.iter().all(|a| supported_expr(a, n_params))
        }
        _ => supported_expr(ir, n_params),
    }
}

fn supported_expr(ir: &Ir, n_params: usize) -> bool {
    match ir {
        Ir::Const(Value::Int(_)) => true,
        Ir::Const(_) => false,
        Ir::LoadSlot(_) | Ir::LoadSlotTake(_) => true,
        Ir::If { test, then, els } => {
            if is_bool_test(test) {
                let Some(els) = els else { return false };
                supported_bool(test, n_params) && supported_expr(then, n_params) && supported_expr(els, n_params)
            } else {
                // Bare Int test -- see `supported_tail`'s identical arm.
                supported_expr(test, n_params) && supported_expr(then, n_params)
            }
        }
        Ir::Do(exprs) => !exprs.is_empty() && exprs.iter().all(|e| supported_expr(e, n_params)),
        Ir::Let { binds, body } => {
            binds.iter().all(|(pat, init)| {
                matches!(pat, CompiledPattern::Slot(_)) && supported_expr(init, n_params)
            }) && !body.is_empty()
                && body.iter().all(|e| supported_expr(e, n_params))
        }
        Ir::Intrinsic { op, chain, args, .. } => {
            if !chain.intrinsic_armed() {
                return false;
            }
            match op {
                IntrinOp::Add | IntrinOp::Mul => {
                    args.len() >= 2 && args.iter().all(|a| supported_expr(a, n_params))
                }
                IntrinOp::Sub2 => args.len() == 2 && args.iter().all(|a| supported_expr(a, n_params)),
                IntrinOp::Inc | IntrinOp::Dec => {
                    args.len() == 1 && supported_expr(&args[0], n_params)
                }
                // Boolean-only ops are not valid in a plain value position
                // (the subset speculates every value is Int) -- only as an
                // `If` test, via `supported_bool`.
                IntrinOp::Div2
                | IntrinOp::Lt2
                | IntrinOp::Le2
                | IntrinOp::Gt2
                | IntrinOp::Ge2
                | IntrinOp::Eq2
                | IntrinOp::Zero
                | IntrinOp::Not => false,
            }
        }
        // Self-recursion only, same arity (no cross-arity -- mutual/global
        // recursion goes through `CallGlobal` below instead).
        Ir::Call { callee, args, .. } => {
            matches!(**callee, Ir::SelfRef)
                && args.len() == n_params
                && args.iter().all(|a| supported_expr(a, n_params))
        }
        // E1b: a direct call to a global, fixed argc 0-4 -- resolved through
        // a per-call-site inline cache at lower time (see `lower_call_global`).
        Ir::CallGlobal { args, .. } => args.len() <= 4 && args.iter().all(|a| supported_expr(a, n_params)),
        _ => false,
    }
}

/// An `If` test position: the ONLY place a boolean-producing intrinsic may
/// appear, since the subset has no boolean/nil VALUE (see `supported_expr`).
fn supported_bool(ir: &Ir, n_params: usize) -> bool {
    match ir {
        Ir::Intrinsic { op, chain, args, .. } if chain.intrinsic_armed() => match op {
            IntrinOp::Lt2 | IntrinOp::Le2 | IntrinOp::Gt2 | IntrinOp::Ge2 | IntrinOp::Eq2 => {
                args.len() == 2
                    && supported_expr(&args[0], n_params)
                    && supported_expr(&args[1], n_params)
            }
            IntrinOp::Zero => args.len() == 1 && supported_expr(&args[0], n_params),
            IntrinOp::Not => args.len() == 1 && supported_bool(&args[0], n_params),
            _ => false,
        },
        _ => false,
    }
}

/// Lowers arity `idx` of `code`, or `None` if it falls outside the subset.
pub(super) fn lower_arity(code: &std::sync::Arc<CompiledFn>, idx: usize) -> Option<NativeEntry> {
    let arity = &code.arities[idx];
    if arity.variadic || arity.n_params > 4 {
        return None;
    }
    if !supported(&arity.body, arity.n_params, arity.scratch_base) {
        return None;
    }

    let mut guard = state().lock().unwrap_or_else(|e| e.into_inner());
    let st = &mut *guard;
    let target_config = st.module.target_config();
    let ptr_ty = target_config.pointer_type();
    let call_conv = target_config.default_call_conv;

    let mut sig = st.module.make_signature();
    sig.params.push(AbiParam::new(ptr_ty)); // JitCtx*
    for _ in 0..arity.n_params {
        sig.params.push(AbiParam::new(I64));
    }
    sig.params.push(AbiParam::new(ptr_ty)); // out*
    sig.returns.push(AbiParam::new(I32));

    st.counter += 1;
    let name = format!("mova_jit_fn_{}", st.counter);
    let func_id = st.module.declare_function(&name, Linkage::Export, &sig).ok()?;

    let mut ctx = Context::new();
    ctx.func = Function::with_name_signature(UserFuncName::user(0, func_id.as_u32()), sig);
    let mut fbctx = FunctionBuilderContext::new();
    {
        let mut builder = FunctionBuilder::new(&mut ctx.func, &mut fbctx);
        let add_ref = st.module.declare_func_in_func(st.add_id, builder.func);
        let sub_ref = st.module.declare_func_in_func(st.sub_id, builder.func);
        let mul_ref = st.module.declare_func_in_func(st.mul_id, builder.func);
        let self_ref = st.module.declare_func_in_func(func_id, builder.func);
        let call_miss_ref = st.module.declare_func_in_func(st.call_miss_id, builder.func);

        let entry_block = builder.create_block();
        builder.append_block_params_for_function_params(entry_block);
        builder.switch_to_block(entry_block);
        builder.seal_block(entry_block);

        let params = builder.block_params(entry_block).to_vec();
        let ctx_ptr = params[0];
        let out_ptr = *params.last().unwrap();

        let vars: Vec<Variable> = (0..arity.n_slots)
            .map(|_| builder.declare_var(I64))
            .collect();
        let zero = builder.ins().iconst(I64, 0);
        for v in &vars {
            builder.def_var(*v, zero);
        }

        let bail_block = builder.create_block();

        // E1b: an implicit loop head for a tail `Recur` that targets the FN
        // ITSELF (no explicit `loop`) -- unifies that case with an explicit
        // `Ir::Loop` (see `LoopCtx`/`lower_recur`). One extra jump for a
        // non-recursive fn, negligible next to a hot loop's own body.
        let fn_head = builder.create_block();
        for _ in 0..arity.n_params {
            builder.append_block_param(fn_head, I64);
        }
        let init_args: Vec<_> = (0..arity.n_params).map(|i| params[1 + i].into()).collect();
        builder.ins().jump(fn_head, &init_args);
        builder.switch_to_block(fn_head);
        let head_params = builder.block_params(fn_head).to_vec();
        for i in 0..arity.n_params {
            builder.def_var(vars[i], head_params[i]);
        }

        let mut lower = Lower {
            builder,
            vars,
            bail_block,
            add_ref,
            sub_ref,
            mul_ref,
            self_ref,
            call_miss_ref,
            ctx_ptr,
            ptr_ty,
            call_conv,
            loops: vec![LoopCtx { scratch_base: arity.scratch_base, head_block: fn_head }],
        };
        let result = lower.lower_tail_seq(&arity.body);
        lower.loops.pop();
        lower.builder.seal_block(fn_head);
        if let Some(result) = result {
            lower.builder.ins().store(MemFlagsData::trusted(), result, out_ptr, 0);
            let ok = lower.builder.ins().iconst(I32, 0);
            lower.builder.ins().return_(&[ok]);
        }

        lower.builder.switch_to_block(bail_block);
        lower.builder.seal_block(bail_block);
        let bail = lower.builder.ins().iconst(I32, 1);
        lower.builder.ins().return_(&[bail]);

        lower.builder.finalize(target_config);
    }

    st.module.define_function(func_id, &mut ctx).ok()?;
    st.module.clear_context(&mut ctx);
    st.module.finalize_definitions().ok()?;
    let code_ptr = st.module.get_finalized_function(func_id);
    Some(match arity.n_params {
        0 => NativeEntry::A0(unsafe { std::mem::transmute(code_ptr) }),
        1 => NativeEntry::A1(unsafe { std::mem::transmute(code_ptr) }),
        2 => NativeEntry::A2(unsafe { std::mem::transmute(code_ptr) }),
        3 => NativeEntry::A3(unsafe { std::mem::transmute(code_ptr) }),
        _ => NativeEntry::A4(unsafe { std::mem::transmute(code_ptr) }),
    })
}

/// Pass 2's running state -- one per arity being lowered.
struct Lower<'a> {
    builder: FunctionBuilder<'a>,
    vars: Vec<Variable>,
    /// No params: every bail path jumps here empty-handed and it just
    /// returns `1`. Sealed once, at the very end (its predecessor set only
    /// closes once the whole body is lowered).
    bail_block: Block,
    add_ref: cranelift_codegen::ir::FuncRef,
    sub_ref: cranelift_codegen::ir::FuncRef,
    mul_ref: cranelift_codegen::ir::FuncRef,
    self_ref: cranelift_codegen::ir::FuncRef,
    call_miss_ref: cranelift_codegen::ir::FuncRef,
    ctx_ptr: ClifValue,
    ptr_ty: cranelift_codegen::ir::Type,
    call_conv: CallConv,
    /// Open `recur` targets, innermost last -- the fn's own implicit target
    /// is always element 0 (see `lower_arity`).
    loops: Vec<LoopCtx>,
}

impl<'a> Lower<'a> {
    fn lower_seq(&mut self, body: &[Ir]) -> ClifValue {
        let mut last = None;
        for ir in body {
            last = Some(self.lower_expr(ir));
        }
        last.expect("supported() guarantees a non-empty body")
    }

    /// After a fallible op: `is_bail` (nonzero => bail) branches straight to
    /// the shared bail block; otherwise control continues in a fresh block
    /// carrying `ok_value` as its one param, which is what this returns.
    fn guard(&mut self, is_bail: ClifValue, ok_value: ClifValue) -> ClifValue {
        let ok_block = self.builder.create_block();
        let p = self.builder.append_block_param(ok_block, I64);
        self.builder.ins().brif(is_bail, self.bail_block, &[], ok_block, &[ok_value.into()]);
        self.builder.switch_to_block(ok_block);
        self.builder.seal_block(ok_block);
        p
    }

    fn checked_binop(&mut self, callee: cranelift_codegen::ir::FuncRef, a: ClifValue, b: ClifValue) -> ClifValue {
        let slot = self
            .builder
            .create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 3));
        let addr = self.builder.ins().stack_addr(self.ptr_ty, slot, 0);
        let call = self.builder.ins().call(callee, &[a, b, addr]);
        let status = self.builder.inst_results(call)[0];
        let zero = self.builder.ins().iconst(I32, 0);
        let is_bail = self.builder.ins().icmp(IntCC::NotEqual, status, zero);
        let loaded = self.builder.ins().load(I64, MemFlagsData::trusted(), addr, 0);
        self.guard(is_bail, loaded)
    }

    /// Ordinary (non-tail) value position: never diverges (`supported_expr`
    /// never admits `Recur`/`Loop` here), always yields a `ClifValue`.
    fn lower_expr(&mut self, ir: &Ir) -> ClifValue {
        match ir {
            Ir::Const(Value::Int(n)) => self.builder.ins().iconst(I64, *n),
            Ir::LoadSlot(slot) | Ir::LoadSlotTake(slot) => self.builder.use_var(self.vars[*slot as usize]),
            Ir::If { test, then, els } => {
                if !is_bool_test(test) {
                    // Bare Int test: evaluate for its (overflow-bail)
                    // effects only, then always take `then` -- see
                    // `supported_expr`'s identical comment.
                    self.lower_expr(test);
                    return self.lower_expr(then);
                }
                self.lower_if(test, then, els.as_deref().unwrap())
            }
            Ir::Do(exprs) => self.lower_seq(exprs),
            Ir::Let { binds, body } => {
                for (pat, init) in binds {
                    let CompiledPattern::Slot(slot) = pat else {
                        unreachable!("supported() only admits Slot patterns")
                    };
                    let v = self.lower_expr(init);
                    self.builder.def_var(self.vars[*slot as usize], v);
                }
                self.lower_seq(body)
            }
            Ir::Intrinsic { op, args, .. } => match op {
                IntrinOp::Add => {
                    let mut acc = self.lower_expr(&args[0]);
                    for a in &args[1..] {
                        let v = self.lower_expr(a);
                        acc = self.checked_binop(self.add_ref, acc, v);
                    }
                    acc
                }
                IntrinOp::Mul => {
                    let mut acc = self.lower_expr(&args[0]);
                    for a in &args[1..] {
                        let v = self.lower_expr(a);
                        acc = self.checked_binop(self.mul_ref, acc, v);
                    }
                    acc
                }
                IntrinOp::Sub2 => {
                    let a = self.lower_expr(&args[0]);
                    let b = self.lower_expr(&args[1]);
                    self.checked_binop(self.sub_ref, a, b)
                }
                IntrinOp::Inc => {
                    let a = self.lower_expr(&args[0]);
                    let one = self.builder.ins().iconst(I64, 1);
                    self.checked_binop(self.add_ref, a, one)
                }
                IntrinOp::Dec => {
                    let a = self.lower_expr(&args[0]);
                    let one = self.builder.ins().iconst(I64, 1);
                    self.checked_binop(self.sub_ref, a, one)
                }
                _ => unreachable!("supported_expr() rejects boolean-only intrinsics"),
            },
            Ir::Call { callee, args, .. } if matches!(**callee, Ir::SelfRef) => self.lower_self_call(args),
            Ir::CallGlobal { chain, args, .. } => self.lower_call_global(chain, args),
            _ => unreachable!("supported() rejected everything else"),
        }
    }

    fn lower_if(&mut self, test: &Ir, then: &Ir, els: &Ir) -> ClifValue {
        let cond = self.lower_bool(test, false);
        let then_block = self.builder.create_block();
        let else_block = self.builder.create_block();
        let merge_block = self.builder.create_block();
        let merge_param = self.builder.append_block_param(merge_block, I64);
        self.builder.ins().brif(cond, then_block, &[], else_block, &[]);

        self.builder.switch_to_block(then_block);
        self.builder.seal_block(then_block);
        let tv = self.lower_expr(then);
        self.builder.ins().jump(merge_block, &[tv.into()]);

        self.builder.switch_to_block(else_block);
        self.builder.seal_block(else_block);
        let ev = self.lower_expr(els);
        self.builder.ins().jump(merge_block, &[ev.into()]);

        self.builder.switch_to_block(merge_block);
        self.builder.seal_block(merge_block);
        merge_param
    }

    /// One `icmp`, complementing the condition code for each `not` wrapper
    /// instead of computing and re-testing a boolean value -- sidesteps the
    /// whole "what type is a Cranelift boolean" question.
    fn lower_bool(&mut self, ir: &Ir, negate: bool) -> ClifValue {
        let Ir::Intrinsic { op, args, .. } = ir else {
            unreachable!("supported_bool() only admits Intrinsic")
        };
        match op {
            IntrinOp::Not => self.lower_bool(&args[0], !negate),
            IntrinOp::Zero => {
                let a = self.lower_expr(&args[0]);
                let z = self.builder.ins().iconst(I64, 0);
                let cc = if negate { IntCC::NotEqual } else { IntCC::Equal };
                self.builder.ins().icmp(cc, a, z)
            }
            _ => {
                let a = self.lower_expr(&args[0]);
                let b = self.lower_expr(&args[1]);
                let base = match op {
                    IntrinOp::Lt2 => IntCC::SignedLessThan,
                    IntrinOp::Le2 => IntCC::SignedLessThanOrEqual,
                    IntrinOp::Gt2 => IntCC::SignedGreaterThan,
                    IntrinOp::Ge2 => IntCC::SignedGreaterThanOrEqual,
                    IntrinOp::Eq2 => IntCC::Equal,
                    _ => unreachable!(),
                };
                use cranelift_codegen::ir::condcodes::CondCode;
                let cc = if negate { base.complement() } else { base };
                self.builder.ins().icmp(cc, a, b)
            }
        }
    }

    fn lower_self_call(&mut self, args: &[Ir]) -> ClifValue {
        let arg_vals: Vec<ClifValue> = args.iter().map(|a| self.lower_expr(a)).collect();
        let depth = self
            .builder
            .ins()
            .load(I64, MemFlagsData::trusted(), self.ctx_ptr, 0);
        let zero = self.builder.ins().iconst(I64, 0);
        let exhausted = self.builder.ins().icmp(IntCC::Equal, depth, zero);
        let call_block = self.builder.create_block();
        self.builder.ins().brif(exhausted, self.bail_block, &[], call_block, &[]);
        self.builder.switch_to_block(call_block);
        self.builder.seal_block(call_block);

        let one = self.builder.ins().iconst(I64, 1);
        let dec = self.builder.ins().isub(depth, one);
        self.builder.ins().store(MemFlagsData::trusted(), dec, self.ctx_ptr, 0);

        let slot = self
            .builder
            .create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 3));
        let out_addr = self.builder.ins().stack_addr(self.ptr_ty, slot, 0);
        let mut call_args = Vec::with_capacity(arg_vals.len() + 2);
        call_args.push(self.ctx_ptr);
        call_args.extend(arg_vals);
        call_args.push(out_addr);
        let call = self.builder.ins().call(self.self_ref, &call_args);
        let status = self.builder.inst_results(call)[0];

        // Restore the budget for the NEXT sibling call at this same depth
        // (e.g. `fib`'s second recursive call), mirroring a real call
        // stack popping back to this frame.
        self.builder.ins().store(MemFlagsData::trusted(), depth, self.ctx_ptr, 0);

        let zero32 = self.builder.ins().iconst(I32, 0);
        let is_bail = self.builder.ins().icmp(IntCC::NotEqual, status, zero32);
        let loaded = self.builder.ins().load(I64, MemFlagsData::trusted(), out_addr, 0);
        self.guard(is_bail, loaded)
    }

    /// E1b: `(f a b)` to a resolved global, through a per-call-site inline
    /// cache (docs/JIT.md). Both the `GlobalChain` (cloned -- cheap, just
    /// `Arc<VarCell>`s) and the `CallIc` are `Box::leak`ed so their
    /// addresses embedded below as `iconst`s never dangle, independent of
    /// whatever `Arc<CompiledFn>` this call site's own IR tree lives in.
    fn lower_call_global(&mut self, chain: &GlobalChain, args: &[Ir]) -> ClifValue {
        let argc = args.len();
        let arg_vals: Vec<ClifValue> = args.iter().map(|a| self.lower_expr(a)).collect();

        let chain_static: &'static GlobalChain = Box::leak(Box::new(chain.clone()));
        let ic: &'static CallIc = CallIc::leak();
        let chain_addr = self.builder.ins().iconst(self.ptr_ty, chain_static as *const GlobalChain as i64);
        let ic_addr = self.builder.ins().iconst(self.ptr_ty, ic as *const CallIc as i64);
        let epoch_addr = self.builder.ins().iconst(self.ptr_ty, &DEF_EPOCH as *const _ as i64);

        // Hit check: plain (non-fenced) loads of two `Atomic*` fields --
        // see `super::DEF_EPOCH`'s doc for why a torn read is harmless here.
        let cur_epoch = self.builder.ins().load(I64, MemFlagsData::trusted(), epoch_addr, 0);
        let ic_epoch = self.builder.ins().load(I64, MemFlagsData::trusted(), ic_addr, 0);
        let hit = self.builder.ins().icmp(IntCC::Equal, ic_epoch, cur_epoch);

        let hit_block = self.builder.create_block();
        let miss_block = self.builder.create_block();
        let merge_block = self.builder.create_block();
        let entry_param = self.builder.append_block_param(merge_block, self.ptr_ty);
        self.builder.ins().brif(hit, hit_block, &[], miss_block, &[]);

        self.builder.switch_to_block(hit_block);
        self.builder.seal_block(hit_block);
        let entry_off = std::mem::offset_of!(CallIc, entry) as i32;
        let hit_entry = self.builder.ins().load(self.ptr_ty, MemFlagsData::trusted(), ic_addr, entry_off);
        self.builder.ins().jump(merge_block, &[hit_entry.into()]);

        self.builder.switch_to_block(miss_block);
        self.builder.seal_block(miss_block);
        let argc_const = self.builder.ins().iconst(I32, argc as i64);
        let miss_call = self.builder.ins().call(self.call_miss_ref, &[ic_addr, chain_addr, argc_const]);
        let miss_entry = self.builder.inst_results(miss_call)[0];
        self.builder.ins().jump(merge_block, &[miss_entry.into()]);

        self.builder.switch_to_block(merge_block);
        self.builder.seal_block(merge_block);

        let zero_ptr = self.builder.ins().iconst(self.ptr_ty, 0);
        let bad = self.builder.ins().icmp(IntCC::Equal, entry_param, zero_ptr);
        let ok_block = self.builder.create_block();
        self.builder.ins().brif(bad, self.bail_block, &[], ok_block, &[]);
        self.builder.switch_to_block(ok_block);
        self.builder.seal_block(ok_block);

        // Every native call (self or global) spends from the same depth
        // budget (docs/JIT.md point 5) -- identical to `lower_self_call`.
        let depth = self.builder.ins().load(I64, MemFlagsData::trusted(), self.ctx_ptr, 0);
        let zero = self.builder.ins().iconst(I64, 0);
        let exhausted = self.builder.ins().icmp(IntCC::Equal, depth, zero);
        let call_block = self.builder.create_block();
        self.builder.ins().brif(exhausted, self.bail_block, &[], call_block, &[]);
        self.builder.switch_to_block(call_block);
        self.builder.seal_block(call_block);
        let one = self.builder.ins().iconst(I64, 1);
        let dec = self.builder.ins().isub(depth, one);
        self.builder.ins().store(MemFlagsData::trusted(), dec, self.ctx_ptr, 0);

        let mut sig = Signature::new(self.call_conv);
        sig.params.push(AbiParam::new(self.ptr_ty));
        for _ in 0..argc {
            sig.params.push(AbiParam::new(I64));
        }
        sig.params.push(AbiParam::new(self.ptr_ty));
        sig.returns.push(AbiParam::new(I32));
        let sig_ref = self.builder.import_signature(sig);

        let slot = self
            .builder
            .create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 3));
        let out_addr = self.builder.ins().stack_addr(self.ptr_ty, slot, 0);
        let mut call_args = Vec::with_capacity(argc + 2);
        call_args.push(self.ctx_ptr);
        call_args.extend(arg_vals);
        call_args.push(out_addr);
        let call = self.builder.ins().call_indirect(sig_ref, entry_param, &call_args);
        let status = self.builder.inst_results(call)[0];

        self.builder.ins().store(MemFlagsData::trusted(), depth, self.ctx_ptr, 0);

        let zero32 = self.builder.ins().iconst(I32, 0);
        let is_bail = self.builder.ins().icmp(IntCC::NotEqual, status, zero32);
        let loaded = self.builder.ins().load(I64, MemFlagsData::trusted(), out_addr, 0);
        self.guard(is_bail, loaded)
    }

    /// A tail-position statement sequence: all but the last are lowered for
    /// effect only, the last may diverge (`Recur`) -- see `lower_tail`.
    fn lower_tail_seq(&mut self, body: &[Ir]) -> Option<ClifValue> {
        let (last, init) = body.split_last().expect("supported() guarantees a non-empty body");
        for e in init {
            self.lower_expr(e);
        }
        self.lower_tail(last)
    }

    /// Lowers `ir` in tail position: `Some(v)` means control falls through
    /// to the CURRENT block (builder cursor) holding `v`, still needing a
    /// terminator from the caller; `None` means `ir` already terminated its
    /// own block (a `recur` jump), and the caller must add nothing more.
    fn lower_tail(&mut self, ir: &Ir) -> Option<ClifValue> {
        match ir {
            Ir::If { test, then, els } => self.lower_tail_if(test, then, els.as_deref()),
            Ir::Do(exprs) => self.lower_tail_seq(exprs),
            Ir::Let { binds, body } => {
                for (pat, init) in binds {
                    let CompiledPattern::Slot(slot) = pat else {
                        unreachable!("supported() only admits Slot patterns")
                    };
                    let v = self.lower_expr(init);
                    self.builder.def_var(self.vars[*slot as usize], v);
                }
                self.lower_tail_seq(body)
            }
            Ir::Loop { binds, scratch_base, body } => self.lower_loop(binds, *scratch_base, body),
            Ir::Recur { args, scratch_base } => self.lower_recur(args, *scratch_base),
            _ => Some(self.lower_expr(ir)),
        }
    }

    fn lower_tail_if(&mut self, test: &Ir, then: &Ir, els: Option<&Ir>) -> Option<ClifValue> {
        if !is_bool_test(test) {
            // Bare Int test -- always truthy, see `supported_tail`'s comment.
            self.lower_expr(test);
            return self.lower_tail(then);
        }
        let els = els.expect("supported() requires els for a bool test");
        let cond = self.lower_bool(test, false);
        let then_block = self.builder.create_block();
        let else_block = self.builder.create_block();
        let merge_block = self.builder.create_block();
        let merge_param = self.builder.append_block_param(merge_block, I64);
        self.builder.ins().brif(cond, then_block, &[], else_block, &[]);

        self.builder.switch_to_block(then_block);
        self.builder.seal_block(then_block);
        let tv = self.lower_tail(then);
        if let Some(v) = tv {
            self.builder.ins().jump(merge_block, &[v.into()]);
        }

        self.builder.switch_to_block(else_block);
        self.builder.seal_block(else_block);
        let ev = self.lower_tail(els);
        if let Some(v) = ev {
            self.builder.ins().jump(merge_block, &[v.into()]);
        }

        if tv.is_none() && ev.is_none() {
            self.builder.seal_block(merge_block);
            return None;
        }
        self.builder.switch_to_block(merge_block);
        self.builder.seal_block(merge_block);
        Some(merge_param)
    }

    fn lower_loop(&mut self, binds: &[(CompiledPattern, Ir)], scratch_base: u16, body: &[Ir]) -> Option<ClifValue> {
        let init_vals: Vec<ClifValue> = binds.iter().map(|(_, init)| self.lower_expr(init)).collect();
        let head_block = self.builder.create_block();
        for _ in binds {
            self.builder.append_block_param(head_block, I64);
        }
        let jump_args: Vec<_> = init_vals.iter().map(|v| (*v).into()).collect();
        self.builder.ins().jump(head_block, &jump_args);
        self.builder.switch_to_block(head_block);
        let head_params = self.builder.block_params(head_block).to_vec();
        for ((pat, _), p) in binds.iter().zip(head_params.iter()) {
            let CompiledPattern::Slot(slot) = pat else {
                unreachable!("supported() only admits Slot patterns")
            };
            self.builder.def_var(self.vars[*slot as usize], *p);
        }
        self.loops.push(LoopCtx { scratch_base, head_block });
        let result = self.lower_tail_seq(body);
        self.loops.pop();
        // All predecessors (the initial jump above plus every `recur` jump
        // lowered while processing `body`) are now known.
        self.builder.seal_block(head_block);
        result
    }

    fn lower_recur(&mut self, args: &[Ir], scratch_base: u16) -> Option<ClifValue> {
        let head_block = self
            .loops
            .iter()
            .rev()
            .find(|c| c.scratch_base == scratch_base)
            .map(|c| c.head_block)
            .expect("supported() matched this recur to an open loop target");
        let vals: Vec<ClifValue> = args.iter().map(|a| self.lower_expr(a)).collect();
        let jump_args: Vec<_> = vals.iter().map(|v| (*v).into()).collect();
        self.builder.ins().jump(head_block, &jump_args);
        None
    }
}

// ========================= E3a: generic (any-`Value`) tier =========================
//
// Deviations from docs/NATIVE-TIER-DESIGN.md, documented here and in the E3a
// report (docs/JIT.md has the short version):
//  - No inline Int fast path for `Intrinsic` -- every arithmetic/compare op
//    goes through `jit_g_arith` (the E1 int-only tier is still tried FIRST
//    by `eval::apply::apply_closure_buf`, so pure-int arities keep E1's
//    speed; this tier is the generic fallback).
//  - `CallGlobal` always resolves fresh through `jit_g_call_global` ->
//    `apply_value_owned` -- no inline cache / native-to-native fast path
//    yet (always correct under redefinition; E3b's speed work).
//  - `Call` to a non-self, non-global callee Value (local fn value,
//    keyword/map-as-fn) and non-tail self-recursion are NOT lowered this
//    round -- such an arity simply stays interpreted (always safe).
//  - F1: `Let` IS now lowered (plain-symbol/`Slot` patterns only, like E1's
//    int tier) -- see `lower_g_let_tail`/`lower_g_let_expr`/`let_scopes`.
//    Deviation: a `recur` to an OUTER loop from inside a `Let` does not
//    drop that `Let`'s own bound slots (mirrors `lower_g_loop`'s existing
//    behavior for a `Let`/inner-`Loop` nested the same way -- a pre-existing
//    gap, not new here; unreachable in `kondo-walk.clj`).

fn intrin_op_code(op: IntrinOp) -> i32 {
    match op {
        IntrinOp::Add => OP_ADD,
        IntrinOp::Sub2 => OP_SUB2,
        IntrinOp::Mul => OP_MUL,
        IntrinOp::Div2 => OP_DIV2,
        IntrinOp::Inc => OP_INC,
        IntrinOp::Dec => OP_DEC,
        IntrinOp::Lt2 => OP_LT2,
        IntrinOp::Le2 => OP_LE2,
        IntrinOp::Gt2 => OP_GT2,
        IntrinOp::Ge2 => OP_GE2,
        IntrinOp::Eq2 => OP_EQ2,
        IntrinOp::Zero => OP_ZERO,
        IntrinOp::Not => OP_NOT,
    }
}

/// Pass 1 (generic tier): mirrors `supported`/`supported_expr`/`supported_tail`
/// above, generalized to any `Value` and requiring a probeable layout (the
/// tier's one inline fast path: generic truthiness in `If`).
fn supported_generic(body: &[Ir], n_params: usize, scratch_base: u16) -> bool {
    layout::probe().is_some() && !body.is_empty() && {
        let mut targets = vec![(scratch_base, n_params)];
        supported_g_tail_seq(body, n_params, &mut targets)
    }
}

fn supported_g_tail_seq(exprs: &[Ir], n_params: usize, targets: &mut Vec<(u16, usize)>) -> bool {
    let Some((last, init)) = exprs.split_last() else { return false };
    init.iter().all(|e| supported_g_expr(e, n_params)) && supported_g_tail(last, n_params, targets)
}

fn supported_g_tail(ir: &Ir, n_params: usize, targets: &mut Vec<(u16, usize)>) -> bool {
    match ir {
        Ir::If { test, then, els } => {
            supported_g_expr(test, n_params)
                && supported_g_tail(then, n_params, targets)
                && els.as_deref().is_none_or(|e| supported_g_tail(e, n_params, targets))
        }
        Ir::Do(exprs) => supported_g_tail_seq(exprs, n_params, targets),
        Ir::Let { binds, body } => {
            binds.iter().all(|(pat, init)| matches!(pat, CompiledPattern::Slot(_)) && supported_g_expr(init, n_params))
                && supported_g_tail_seq(body, n_params, targets)
        }
        Ir::Loop { binds, scratch_base, body } => {
            if !binds.iter().all(|(pat, init)| matches!(pat, CompiledPattern::Slot(_)) && supported_g_expr(init, n_params)) {
                return false;
            }
            targets.push((*scratch_base, binds.len()));
            let ok = supported_g_tail_seq(body, n_params, targets);
            targets.pop();
            ok
        }
        Ir::Recur { args, scratch_base } => {
            targets.iter().rev().any(|(sb, arity)| sb == scratch_base && *arity == args.len())
                && args.iter().all(|a| supported_g_expr(a, n_params))
        }
        _ => supported_g_expr(ir, n_params),
    }
}

fn supported_g_expr(ir: &Ir, n_params: usize) -> bool {
    match ir {
        Ir::Const(_) => true,
        Ir::LoadSlot(_) | Ir::LoadSlotTake(_) => true,
        // F1: a bare global reference (e.g. `analyze` passed BY VALUE to
        // `reduce`, not called) -- pure lookup, no locals/captures needed,
        // so always safe (`jit_g_global_ref` mirrors `Ir::GlobalRef`'s own
        // `exec.rs` arm exactly: `chain.get()` or the same unresolved err).
        Ir::GlobalRef { .. } => true,
        // F1: the running closure, borrowed from `JitCtx::self_val` (see
        // `lower_g_expr`'s arm) -- exact under redefinition-during-recursion
        // because the CALLER (`apply_closure_buf`) fills it from its own
        // `rc`, never from a fresh global lookup.
        Ir::SelfRef => true,
        Ir::If { test, then, els } => {
            supported_g_expr(test, n_params)
                && supported_g_expr(then, n_params)
                && els.as_deref().is_none_or(|e| supported_g_expr(e, n_params))
        }
        Ir::Do(exprs) => !exprs.is_empty() && exprs.iter().all(|e| supported_g_expr(e, n_params)),
        Ir::Let { binds, body } => {
            binds.iter().all(|(pat, init)| matches!(pat, CompiledPattern::Slot(_)) && supported_g_expr(init, n_params))
                && !body.is_empty()
                && body.iter().all(|e| supported_g_expr(e, n_params))
        }
        Ir::Intrinsic { op, chain, args, .. } => {
            chain.intrinsic_armed()
                && match op {
                    IntrinOp::Add | IntrinOp::Mul => args.len() == 2,
                    IntrinOp::Inc | IntrinOp::Dec | IntrinOp::Zero | IntrinOp::Not => args.len() == 1,
                    _ => args.len() == 2,
                }
                && args.iter().all(|a| supported_g_expr(a, n_params))
        }
        Ir::CallGlobal { args, .. } => args.len() <= 4 && args.iter().all(|a| supported_g_expr(a, n_params)),
        // F1: `Ir::Call` (computed callee -- keyword/map-as-fn) is
        // IMPLEMENTED (`lower_g_call`/`jit_g_call`) but NOT armed: measured
        // a real data-corruption bug -- a fn that both takes a `Value::Fn`
        // reference to ITSELF (`Ir::SelfRef`, e.g. passed to `reduce`) and
        // makes a keyword/map-as-fn call (`Ir::Call`) in its own body, then
        // recurses through that reference over a large (1000+-node) shared
        // structure, silently corrupts that structure: correct on the
        // first traversal, wrong (then stably wrong) from the 2nd on.
        // Reproduced minimally (no `case`/`let`/`Escape` needed -- see the
        // F1 report); root cause not isolated within this round's budget.
        // Matches this tier's own pre-existing documented deviation ("Call
        // to a non-self, non-global callee Value ... NOT lowered this
        // round -- always safe"), so this is a return to that documented
        // baseline, not a new restriction.
        Ir::Call { .. } => false,
        // F1: an escape whose bridge only needs THIS fn's own slots (no
        // capture/self-ref/sibling -- those would need a closure/creation-
        // env this ABI doesn't carry, see `lower_g_escape`).
        Ir::Escape(e) => e.binds.iter().all(|(_, src)| matches!(src, CaptureSrc::Slot(_))),
        // F1: `throw` -- always errors, so this expr position's "value" is
        // never actually produced; see `lower_g_throw`.
        Ir::Throw { value, .. } => supported_g_expr(value, n_params),
        _ => false,
    }
}

/// F1 (`MOVA_JIT_EXPLAIN=1`): first-unsupported-node diagnostic, mirroring
/// `supported_generic` exactly but returning WHY on the first rejection
/// instead of a bare `bool`. Never on the hot path -- only called from
/// `JitSlot::get_or_lower_generic` when the env var is set.
pub(super) fn explain_generic(body: &[Ir], n_params: usize, scratch_base: u16) -> Result<(), String> {
    if layout::probe().is_none() {
        return Err("Value layout not probeable".to_string());
    }
    if body.is_empty() {
        return Err("empty body".to_string());
    }
    let mut targets = vec![(scratch_base, n_params)];
    explain_g_tail_seq(body, n_params, &mut targets)
}

fn explain_g_tail_seq(exprs: &[Ir], n_params: usize, targets: &mut Vec<(u16, usize)>) -> Result<(), String> {
    let Some((last, init)) = exprs.split_last() else { return Err("empty seq".to_string()) };
    for e in init {
        explain_g_expr(e, n_params)?;
    }
    explain_g_tail(last, n_params, targets)
}

fn explain_g_tail(ir: &Ir, n_params: usize, targets: &mut Vec<(u16, usize)>) -> Result<(), String> {
    match ir {
        Ir::If { test, then, els } => {
            explain_g_expr(test, n_params)?;
            explain_g_tail(then, n_params, targets)?;
            if let Some(e) = els.as_deref() {
                explain_g_tail(e, n_params, targets)?;
            }
            Ok(())
        }
        Ir::Do(exprs) => explain_g_tail_seq(exprs, n_params, targets),
        Ir::Let { binds, body } => {
            for (pat, init) in binds {
                if !matches!(pat, CompiledPattern::Slot(_)) {
                    return Err("Let: non-Slot pattern".to_string());
                }
                explain_g_expr(init, n_params)?;
            }
            explain_g_tail_seq(body, n_params, targets)
        }
        Ir::Loop { binds, scratch_base, body } => {
            for (pat, init) in binds {
                if !matches!(pat, CompiledPattern::Slot(_)) {
                    return Err("Loop: non-Slot pattern".to_string());
                }
                explain_g_expr(init, n_params)?;
            }
            targets.push((*scratch_base, binds.len()));
            let r = explain_g_tail_seq(body, n_params, targets);
            targets.pop();
            r
        }
        Ir::Recur { args, scratch_base } => {
            if !targets.iter().rev().any(|(sb, arity)| sb == scratch_base && *arity == args.len()) {
                return Err("Recur: no matching open loop/fn target at this arity".to_string());
            }
            for a in args {
                explain_g_expr(a, n_params)?;
            }
            Ok(())
        }
        _ => explain_g_expr(ir, n_params),
    }
}

fn explain_g_expr(ir: &Ir, n_params: usize) -> Result<(), String> {
    match ir {
        Ir::Const(_) | Ir::LoadSlot(_) | Ir::LoadSlotTake(_) | Ir::GlobalRef { .. } | Ir::SelfRef => Ok(()),
        Ir::If { test, then, els } => {
            explain_g_expr(test, n_params)?;
            explain_g_expr(then, n_params)?;
            if let Some(e) = els.as_deref() {
                explain_g_expr(e, n_params)?;
            }
            Ok(())
        }
        Ir::Do(exprs) => {
            if exprs.is_empty() {
                return Err("Do: empty".to_string());
            }
            exprs.iter().try_for_each(|e| explain_g_expr(e, n_params))
        }
        Ir::Let { binds, body } => {
            for (pat, init) in binds {
                if !matches!(pat, CompiledPattern::Slot(_)) {
                    return Err("Let: non-Slot pattern".to_string());
                }
                explain_g_expr(init, n_params)?;
            }
            if body.is_empty() {
                return Err("Let: empty body".to_string());
            }
            body.iter().try_for_each(|e| explain_g_expr(e, n_params))
        }
        Ir::Intrinsic { op, chain, args, .. } => {
            if !chain.intrinsic_armed() {
                return Err("Intrinsic: builtin not armed (redefined)".to_string());
            }
            let ok_argc = match op {
                IntrinOp::Add | IntrinOp::Mul => args.len() == 2,
                IntrinOp::Inc | IntrinOp::Dec | IntrinOp::Zero | IntrinOp::Not => args.len() == 1,
                _ => args.len() == 2,
            };
            if !ok_argc {
                return Err(format!("Intrinsic: unsupported argc {}", args.len()));
            }
            args.iter().try_for_each(|a| explain_g_expr(a, n_params))
        }
        Ir::CallGlobal { args, sym, .. } => {
            if args.len() > 4 {
                return Err(format!("CallGlobal {}: argc>4", sym.name));
            }
            args.iter().try_for_each(|a| explain_g_expr(a, n_params))
        }
        Ir::Call { .. } => Err("Call: computed callee -- reverted, see F1 report (data-corruption bug)".to_string()),
        Ir::Escape(e) => {
            if !e.binds.iter().all(|(_, src)| matches!(src, CaptureSrc::Slot(_))) {
                return Err("Escape: bridges a capture/self-ref/sibling, not just this fn's own slots".to_string());
            }
            Ok(())
        }
        Ir::Throw { value, .. } => explain_g_expr(value, n_params),
        other => Err(format!("unsupported node: {}", ir_kind_name(other))),
    }
}

fn ir_kind_name(ir: &Ir) -> &'static str {
    match ir {
        Ir::Const(_) => "Const",
        Ir::LoadSlot(_) => "LoadSlot",
        Ir::LoadSlotTake(_) => "LoadSlotTake",
        Ir::LoadCapture(_) => "LoadCapture",
        Ir::SelfRef => "SelfRef",
        Ir::GlobalRef { .. } => "GlobalRef",
        Ir::CreationEnvLookup { .. } => "CreationEnvLookup",
        Ir::SetMutField { .. } => "SetMutField",
        Ir::If { .. } => "If",
        Ir::Do(_) => "Do",
        Ir::Let { .. } => "Let",
        Ir::Loop { .. } => "Loop",
        Ir::Recur { .. } => "Recur",
        Ir::NumLoop(_) => "NumLoop",
        Ir::Call { .. } => "Call",
        Ir::CallGlobal { .. } => "CallGlobal",
        Ir::CallCreationEnv { .. } => "CallCreationEnv",
        Ir::Intrinsic { .. } => "Intrinsic",
        Ir::VectorLit(_) => "VectorLit",
        Ir::MapLit(_) => "MapLit",
        Ir::SetLit(_) => "SetLit",
        Ir::Throw { .. } => "Throw",
        Ir::MakeClosure { .. } => "MakeClosure",
        Ir::MakeRecGroup { .. } => "MakeRecGroup",
        Ir::SiblingRef(_) => "SiblingRef",
        Ir::Try { .. } => "Try",
        Ir::Def { .. } => "Def",
        Ir::DynBind(_) => "DynBind",
        Ir::Escape(_) => "Escape",
        Ir::FieldGet(_) => "FieldGet",
        Ir::New(_) => "New",
    }
}

/// An address (`*const`/`*mut Value`), tagged with whether the CALLER of
/// `lower_g_expr` independently owns it (docs/NATIVE-TIER-DESIGN.md #1/#2):
/// `Borrow` came from a param/const/slot and must never be dropped by the
/// reader; `Owned` is a fresh single-use result (a helper call's `out`, or
/// an `If`'s merged result) that the reader must eventually move or drop.
#[derive(Clone, Copy)]
enum GVal {
    Borrow(ClifValue),
    Owned(ClifValue),
}

impl GVal {
    fn addr(self) -> ClifValue {
        match self {
            GVal::Borrow(a) | GVal::Owned(a) => a,
        }
    }
}

/// One active `Loop` (explicit or the implicit fn-level one for a bare
/// top-level `recur`, see `lower_arity_generic`): its bound values live in
/// `slots[slot_base..slot_base + n]`, all independently owned (established
/// by `jit_g_clone` at init/recur time) -- dropped at this scope's own exit
/// (`lower_g_tail_seq` falling through) or before being overwritten by a
/// matching `Recur`.
struct LoopCtxG {
    scratch_base: u16,
    head_block: Block,
    slot_base: usize,
    n: usize,
}

/// Lowers arity `idx` of `code` for the generic tier, or `None` outside the
/// E3a subset (`supported_generic`) or if the arity's own `Value` layout
/// isn't probeable.
pub(super) fn lower_arity_generic(code: &std::sync::Arc<CompiledFn>, idx: usize) -> Option<GenericEntry> {
    let arity = &code.arities[idx];
    let dbg = std::env::var("MOVA_JIT_EXPLAIN").is_ok();
    if arity.variadic || arity.n_params > 4 {
        if dbg {
            eprintln!("jit-explain: DEBUG variadic={} n_params={}", arity.variadic, arity.n_params);
        }
        return None;
    }
    if !supported_generic(&arity.body, arity.n_params, arity.scratch_base) {
        if dbg {
            eprintln!("jit-explain: DEBUG supported_generic()=false");
        }
        return None;
    }
    let Some(layout) = layout::probe() else {
        if dbg {
            eprintln!("jit-explain: DEBUG layout::probe()=None");
        }
        return None;
    };

    let mut guard = state().lock().unwrap_or_else(|e| e.into_inner());
    let st = &mut *guard;
    let target_config = st.module.target_config();
    let ptr_ty = target_config.pointer_type();

    let mut sig = st.module.make_signature();
    sig.params.push(AbiParam::new(ptr_ty)); // JitCtx*
    for _ in 0..arity.n_params {
        sig.params.push(AbiParam::new(ptr_ty)); // *const Value (borrow)
    }
    sig.params.push(AbiParam::new(ptr_ty)); // out*
    sig.returns.push(AbiParam::new(I32));

    st.counter += 1;
    let name = format!("mova_jit_gfn_{}", st.counter);
    let func_id = st.module.declare_function(&name, Linkage::Export, &sig).ok()?;

    let mut cctx = Context::new();
    cctx.func = Function::with_name_signature(UserFuncName::user(0, func_id.as_u32()), sig);
    let mut fbctx = FunctionBuilderContext::new();
    {
        let mut builder = FunctionBuilder::new(&mut cctx.func, &mut fbctx);
        let g_arith_ref = st.module.declare_func_in_func(st.g_arith_id, builder.func);
        let g_call_global_ref = st.module.declare_func_in_func(st.g_call_global_id, builder.func);
        let g_clone_ref = st.module.declare_func_in_func(st.g_clone_id, builder.func);
        let g_drop_ref = st.module.declare_func_in_func(st.g_drop_id, builder.func);
        let g_escape_ref = st.module.declare_func_in_func(st.g_escape_id, builder.func);
        let g_global_ref_ref = st.module.declare_func_in_func(st.g_global_ref_id, builder.func);
        let g_call_ref = st.module.declare_func_in_func(st.g_call_id, builder.func);
        let g_throw_ref = st.module.declare_func_in_func(st.g_throw_id, builder.func);

        let entry_block = builder.create_block();
        builder.append_block_params_for_function_params(entry_block);
        builder.switch_to_block(entry_block);
        builder.seal_block(entry_block);

        let params = builder.block_params(entry_block).to_vec();
        let ctx_ptr = params[0];
        let param_ptrs: Vec<ClifValue> = params[1..1 + arity.n_params].to_vec();
        let out_ptr = *params.last().unwrap();

        // One 32-byte slot per local (params included -- a bare top-level
        // `recur` rebinds them, see below); `supported_generic` rejects
        // `Let`, so every slot index is either a param or a `Loop` bind.
        let slots: Vec<cranelift_codegen::ir::StackSlot> = (0..arity.n_slots.max(arity.n_params))
            .map(|_| builder.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 32, 3)))
            .collect();

        let error_block = builder.create_block();
        let fn_head = builder.create_block();

        let mut lower = LowerG {
            builder,
            param_ptrs,
            slots,
            ptr_ty,
            layout,
            ctx_ptr,
            error_block,
            g_arith_ref,
            g_call_global_ref,
            g_clone_ref,
            g_drop_ref,
            g_escape_ref,
            g_global_ref_ref,
            g_call_ref,
            g_throw_ref,
            loops: Vec::new(),
            let_scopes: Vec::new(),
        };

        // Establish every param's OWN slot (clone from the borrowed
        // incoming pointer) before any branching, so a bare top-level
        // `recur` (no explicit `loop`) has somewhere to rebind to.
        for i in 0..arity.n_params {
            lower.clone_into_slot(lower.param_ptrs[i], i);
        }
        lower.builder.ins().jump(fn_head, &[]);
        lower.builder.switch_to_block(fn_head);
        lower.loops.push(LoopCtxG { scratch_base: arity.scratch_base, head_block: fn_head, slot_base: 0, n: arity.n_params });

        let result = lower.lower_g_tail_seq(&arity.body);
        lower.builder.seal_block(fn_head);
        if let Some(gval) = result {
            let scope = lower.loops.pop().expect("pushed above");
            lower.move_into(gval, out_ptr);
            for i in 0..scope.n {
                lower.drop_slot(scope.slot_base + i);
            }
            let ok = lower.builder.ins().iconst(I32, 0);
            lower.builder.ins().return_(&[ok]);
        } else {
            lower.loops.pop();
        }

        lower.builder.switch_to_block(error_block);
        lower.builder.seal_block(error_block);
        let err = lower.builder.ins().iconst(I32, 2);
        lower.builder.ins().return_(&[err]);

        lower.builder.finalize(target_config);
    }

    if let Err(e) = st.module.define_function(func_id, &mut cctx) {
        if std::env::var("MOVA_JIT_EXPLAIN").is_ok() {
            eprintln!("jit-explain: define_function failed: {e:?}");
        }
        return None;
    }
    st.module.clear_context(&mut cctx);
    st.module.finalize_definitions().ok()?;
    let code_ptr = st.module.get_finalized_function(func_id);
    Some(match arity.n_params {
        0 => GenericEntry::G0(unsafe { std::mem::transmute(code_ptr) }),
        1 => GenericEntry::G1(unsafe { std::mem::transmute(code_ptr) }),
        2 => GenericEntry::G2(unsafe { std::mem::transmute(code_ptr) }),
        3 => GenericEntry::G3(unsafe { std::mem::transmute(code_ptr) }),
        _ => GenericEntry::G4(unsafe { std::mem::transmute(code_ptr) }),
    })
}

struct LowerG<'a> {
    builder: FunctionBuilder<'a>,
    /// Borrowed incoming argument pointers (call-duration only).
    param_ptrs: Vec<ClifValue>,
    /// One 32-byte stack slot per local (param or `Loop` bind).
    slots: Vec<cranelift_codegen::ir::StackSlot>,
    ptr_ty: cranelift_codegen::ir::Type,
    layout: layout::Layout,
    ctx_ptr: ClifValue,
    /// Shared: every failing helper jumps here (after this call site's own
    /// drop sequence -- see `guard_g`), which just returns `2`.
    error_block: Block,
    g_arith_ref: cranelift_codegen::ir::FuncRef,
    g_call_global_ref: cranelift_codegen::ir::FuncRef,
    g_clone_ref: cranelift_codegen::ir::FuncRef,
    g_drop_ref: cranelift_codegen::ir::FuncRef,
    g_escape_ref: cranelift_codegen::ir::FuncRef,
    g_global_ref_ref: cranelift_codegen::ir::FuncRef,
    g_call_ref: cranelift_codegen::ir::FuncRef,
    g_throw_ref: cranelift_codegen::ir::FuncRef,
    /// Active `Loop` scopes, innermost last; element 0 is always the
    /// implicit fn-level scope over the params (see `lower_arity_generic`).
    loops: Vec<LoopCtxG>,
    /// F1: active `Let` scopes, innermost last -- each a list of the slot
    /// indices that `Let` form bound (not necessarily contiguous, unlike a
    /// `Loop`'s scratch block, so tracked by index rather than base+n).
    let_scopes: Vec<Vec<u16>>,
}

impl<'a> LowerG<'a> {
    fn slot_addr(&mut self, idx: usize) -> ClifValue {
        let slot = self.slots[idx];
        self.builder.ins().stack_addr(self.ptr_ty, slot, 0)
    }

    fn clone_into_slot(&mut self, src: ClifValue, idx: usize) {
        let dst = self.slot_addr(idx);
        self.builder.ins().call(self.g_clone_ref, &[src, dst]);
    }

    fn drop_slot(&mut self, idx: usize) {
        let addr = self.slot_addr(idx);
        self.builder.ins().call(self.g_drop_ref, &[addr]);
    }

    /// Ownership transfer into `dst` (an `out*`/slot address): clone a
    /// `Borrow`, or raw-copy (move, no separate clone+drop) an `Owned`
    /// single-use result -- see [`GVal`].
    fn move_into(&mut self, v: GVal, dst: ClifValue) {
        match v {
            GVal::Borrow(src) => {
                self.builder.ins().call(self.g_clone_ref, &[src, dst]);
            }
            GVal::Owned(src) => {
                // Raw 32-byte copy: `src` is a single-use scratch (an `If`
                // merge slot or a helper's `out`) never read again after
                // this move, so this is a genuine ownership MOVE, not an
                // alias -- no clone, and `src` must NOT be separately
                // dropped after this call.
                for off in [0, 8, 16, 24] {
                    let w = self.builder.ins().load(I64, MemFlagsData::trusted(), src, off);
                    self.builder.ins().store(MemFlagsData::trusted(), w, dst, off);
                }
            }
        }
    }

    /// Drops every currently-live owned slot (every active `Loop`/`Let`
    /// scope, innermost and outermost) -- called right before jumping to
    /// `error_block` from ANY fallible call site, since that one shared
    /// block cannot itself know which scopes were live at the failing site.
    fn drop_all_active(&mut self) {
        let scopes: Vec<(usize, usize)> = self.loops.iter().map(|l| (l.slot_base, l.n)).collect();
        for (base, n) in scopes {
            for i in 0..n {
                self.drop_slot(base + i);
            }
        }
        let let_idxs: Vec<u16> = self.let_scopes.iter().flatten().copied().collect();
        for idx in let_idxs {
            self.drop_slot(idx as usize);
        }
    }

    /// After a helper call returning mova's own status convention (`0` ok,
    /// `2` error): on error, drops every live scope's slots then jumps to
    /// `error_block`; otherwise falls through to a fresh block.
    fn guard_g(&mut self, status: ClifValue) {
        let zero = self.builder.ins().iconst(I32, 0);
        let is_err = self.builder.ins().icmp(IntCC::NotEqual, status, zero);
        let ok_block = self.builder.create_block();
        let err_block = self.builder.create_block();
        self.builder.ins().brif(is_err, err_block, &[], ok_block, &[]);
        self.builder.switch_to_block(err_block);
        self.builder.seal_block(err_block);
        self.drop_all_active();
        self.builder.ins().jump(self.error_block, &[]);
        self.builder.switch_to_block(ok_block);
        self.builder.seal_block(ok_block);
    }

    /// Non-tail sequence: all but the last for effect only (dropped if
    /// owned -- the leak check's "build/drop a map" shape), last is the
    /// result.
    fn lower_g_seq(&mut self, body: &[Ir]) -> GVal {
        let (last, init) = body.split_last().expect("supported_generic guarantees non-empty");
        for e in init {
            let v = self.lower_g_expr(e);
            if let GVal::Owned(addr) = v {
                self.builder.ins().call(self.g_drop_ref, &[addr]);
            }
        }
        self.lower_g_expr(last)
    }

    fn lower_g_expr(&mut self, ir: &Ir) -> GVal {
        match ir {
            Ir::Const(v) => {
                let leaked: &'static Value = Box::leak(Box::new(v.clone()));
                let addr = self.builder.ins().iconst(self.ptr_ty, leaked as *const Value as i64);
                GVal::Borrow(addr)
            }
            Ir::LoadSlot(slot) | Ir::LoadSlotTake(slot) => GVal::Borrow(self.slot_addr(*slot as usize)),
            Ir::If { test, then, els } => self.lower_g_if(test, then, els.as_deref()),
            Ir::Do(exprs) => self.lower_g_seq(exprs),
            Ir::Let { binds, body } => self.lower_g_let_expr(binds, body),
            Ir::Intrinsic { op, args, span, .. } => self.lower_g_intrinsic(*op, args, *span),
            Ir::CallGlobal { chain, sym, sym_span, args, span } => {
                self.lower_g_call_global(chain, sym, *sym_span, args, *span)
            }
            Ir::Escape(e) => self.lower_g_escape(e),
            Ir::GlobalRef { chain, sym, span } => self.lower_g_global_ref(chain, sym, *span),
            Ir::Call { callee, args, span } => self.lower_g_call(callee, args, *span),
            Ir::Throw { value, span } => self.lower_g_throw(value, *span),
            Ir::SelfRef => {
                let off = std::mem::offset_of!(JitCtx, self_val) as i32;
                let addr = self.builder.ins().load(self.ptr_ty, MemFlagsData::trusted(), self.ctx_ptr, off);
                GVal::Borrow(addr)
            }
            _ => unreachable!("supported_generic() rejected everything else"),
        }
    }

    fn lower_g_if(&mut self, test: &Ir, then: &Ir, els: Option<&Ir>) -> GVal {
        let t = self.lower_g_expr(test);
        let truthy = self.lower_truthy(t.addr());
        if let GVal::Owned(a) = t {
            self.builder.ins().call(self.g_drop_ref, &[a]);
        }
        let result_slot = self.fresh_scratch();
        let then_block = self.builder.create_block();
        let else_block = self.builder.create_block();
        let merge_block = self.builder.create_block();
        self.builder.ins().brif(truthy, then_block, &[], else_block, &[]);

        self.builder.switch_to_block(then_block);
        self.builder.seal_block(then_block);
        let tv = self.lower_g_expr(then);
        self.move_into(tv, result_slot);
        self.builder.ins().jump(merge_block, &[]);

        self.builder.switch_to_block(else_block);
        self.builder.seal_block(else_block);
        let ev = match els {
            Some(e) => self.lower_g_expr(e),
            None => {
                let leaked: &'static Value = Box::leak(Box::new(Value::Nil));
                let addr = self.builder.ins().iconst(self.ptr_ty, leaked as *const Value as i64);
                GVal::Borrow(addr)
            }
        };
        self.move_into(ev, result_slot);
        self.builder.ins().jump(merge_block, &[]);

        self.builder.switch_to_block(merge_block);
        self.builder.seal_block(merge_block);
        GVal::Owned(result_slot)
    }

    /// Reads `Value`'s tag (and, for `Bool`, its payload) via `jit::layout`'s
    /// probed offsets: falsy iff `Nil` or `Bool(false)` -- Mova/Clojure's
    /// only two falsy values. Returns an `i8` usable directly in `brif`.
    fn lower_truthy(&mut self, addr: ClifValue) -> ClifValue {
        let l = self.layout;
        let tag = self.builder.ins().load(I64, MemFlagsData::trusted(), addr, l.tag_off as i32);
        let nil_tag = self.builder.ins().iconst(I64, l.nil as i64);
        let is_nil = self.builder.ins().icmp(IntCC::Equal, tag, nil_tag);
        let bool_tag = self.builder.ins().iconst(I64, l.bool_tag as i64);
        let is_bool = self.builder.ins().icmp(IntCC::Equal, tag, bool_tag);
        let payload = self.builder.ins().load(I64, MemFlagsData::trusted(), addr, l.payload_off as i32);
        let zero = self.builder.ins().iconst(I64, 0);
        let is_false_payload = self.builder.ins().icmp(IntCC::Equal, payload, zero);
        let is_false = self.builder.ins().band(is_bool, is_false_payload);
        let falsy = self.builder.ins().bor(is_nil, is_false);
        // F1: this same-width `uextend(I8, <i8>)` fails Cranelift's verifier
        // (pre-existing bug -- unreachable before this round, since no fn
        // with a non-Int `If` had cleared every other `supported_generic`
        // gate yet, so `lower_arity_generic` always returned `None` here
        // and every such fn silently fell back to the interpreter). Fixing
        // it (drop the uextend) let `analyze` lower for the first time, but
        // also let several PRE-EXISTING generic-tier fns run real native
        // code for the first time instead of their proven-safe interpreter
        // fallback, which surfaced an unrelated correctness regression in
        // `jit_test` (a dynamic-var callee call argument came back `nil`)
        // not isolated within this round's budget. Reverted to the
        // broken-but-safe form until that is root-caused -- see the F1
        // report.
        let zero8 = self.builder.ins().iconst(cranelift_codegen::ir::types::I8, 0);
        let falsy8 = self.builder.ins().uextend(cranelift_codegen::ir::types::I8, falsy);
        self.builder.ins().icmp(IntCC::Equal, falsy8, zero8)
    }

    fn fresh_scratch(&mut self) -> ClifValue {
        let slot = self.builder.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 32, 3));
        self.builder.ins().stack_addr(self.ptr_ty, slot, 0)
    }

    fn lower_g_intrinsic(&mut self, op: IntrinOp, args: &[Ir], span: Span) -> GVal {
        let a = self.lower_g_expr(&args[0]);
        let b = if args.len() > 1 { self.lower_g_expr(&args[1]) } else { a };
        let out = self.fresh_scratch();
        let op_c = self.builder.ins().iconst(I32, intrin_op_code(op) as i64);
        let start = self.builder.ins().iconst(I64, span.start as i64);
        let end = self.builder.ins().iconst(I64, span.end as i64);
        let call = self.builder.ins().call(self.g_arith_ref, &[self.ctx_ptr, op_c, a.addr(), b.addr(), start, end, out]);
        let status = self.builder.inst_results(call)[0];
        if let GVal::Owned(addr) = a {
            self.builder.ins().call(self.g_drop_ref, &[addr]);
        }
        if args.len() > 1 {
            if let GVal::Owned(addr) = b {
                self.builder.ins().call(self.g_drop_ref, &[addr]);
            }
        }
        self.guard_g(status);
        GVal::Owned(out)
    }

    fn lower_g_call_global(&mut self, chain: &GlobalChain, sym: &Symbol, sym_span: Span, args: &[Ir], span: Span) -> GVal {
        let arg_gvals: Vec<GVal> = args.iter().map(|a| self.lower_g_expr(a)).collect();
        let argc = args.len();
        let argv_slot = self.builder.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, (argc.max(1) * 8) as u32, 3));
        let argv_addr = self.builder.ins().stack_addr(self.ptr_ty, argv_slot, 0);
        for (i, g) in arg_gvals.iter().enumerate() {
            self.builder.ins().store(MemFlagsData::trusted(), g.addr(), argv_addr, (i * 8) as i32);
        }
        let site = Box::leak(Box::new(CallGlobalSite { chain: chain.clone(), sym: sym.clone(), sym_span, span }));
        let site_addr = self.builder.ins().iconst(self.ptr_ty, site as *const CallGlobalSite as i64);
        let out = self.fresh_scratch();
        let argc_c = self.builder.ins().iconst(I32, argc as i64);
        let call = self.builder.ins().call(self.g_call_global_ref, &[self.ctx_ptr, site_addr, argv_addr, argc_c, out]);
        let status = self.builder.inst_results(call)[0];
        for g in &arg_gvals {
            if let GVal::Owned(addr) = g {
                self.builder.ins().call(self.g_drop_ref, &[*addr]);
            }
        }
        self.guard_g(status);
        GVal::Owned(out)
    }

    /// F1: `Ir::Call` (computed callee, e.g. `(:k m)`) -- same shape as
    /// `lower_g_call_global`, but the callee is a `GVal` from `lower_g_expr`
    /// (no `GlobalChain`/site to leak) instead of a resolved global.
    fn lower_g_call(&mut self, callee: &Ir, args: &[Ir], span: Span) -> GVal {
        let callee_gval = self.lower_g_expr(callee);
        let arg_gvals: Vec<GVal> = args.iter().map(|a| self.lower_g_expr(a)).collect();
        let argc = args.len();
        let argv_slot = self.builder.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, (argc.max(1) * 8) as u32, 3));
        let argv_addr = self.builder.ins().stack_addr(self.ptr_ty, argv_slot, 0);
        for (i, g) in arg_gvals.iter().enumerate() {
            self.builder.ins().store(MemFlagsData::trusted(), g.addr(), argv_addr, (i * 8) as i32);
        }
        let out = self.fresh_scratch();
        let argc_c = self.builder.ins().iconst(I32, argc as i64);
        let start = self.builder.ins().iconst(I64, span.start as i64);
        let end = self.builder.ins().iconst(I64, span.end as i64);
        let call = self.builder.ins().call(self.g_call_ref, &[self.ctx_ptr, callee_gval.addr(), argv_addr, argc_c, start, end, out]);
        let status = self.builder.inst_results(call)[0];
        if let GVal::Owned(addr) = callee_gval {
            self.builder.ins().call(self.g_drop_ref, &[addr]);
        }
        for g in &arg_gvals {
            if let GVal::Owned(addr) = g {
                self.builder.ins().call(self.g_drop_ref, &[*addr]);
            }
        }
        self.guard_g(status);
        GVal::Owned(out)
    }

    /// F1: `Ir::Throw` -- always fails (status 2 unconditionally), so the
    /// `GVal` returned past `guard_g` (which already diverted control to
    /// `error_block`) is dead: a fresh scratch slot, never read.
    fn lower_g_throw(&mut self, value: &Ir, span: Span) -> GVal {
        let v = self.lower_g_expr(value);
        let start = self.builder.ins().iconst(I64, span.start as i64);
        let end = self.builder.ins().iconst(I64, span.end as i64);
        let call = self.builder.ins().call(self.g_throw_ref, &[self.ctx_ptr, v.addr(), start, end]);
        let status = self.builder.inst_results(call)[0];
        if let GVal::Owned(addr) = v {
            self.builder.ins().call(self.g_drop_ref, &[addr]);
        }
        self.guard_g(status);
        GVal::Owned(self.fresh_scratch())
    }

    /// F1: `Ir::Escape` -- leaks a fresh `EscapeSite` (the form + bind list,
    /// `supported_g_expr` already checked every bind is `CaptureSrc::Slot`),
    /// gathers those slots' addresses into a scratch array exactly like
    /// `lower_g_call_global` gathers argv, and calls `jit_g_escape`.
    fn lower_g_escape(&mut self, e: &crate::compile::ir::Escape) -> GVal {
        let n = e.binds.len();
        let argv_slot = self.builder.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, (n.max(1) * 8) as u32, 3));
        let argv_addr = self.builder.ins().stack_addr(self.ptr_ty, argv_slot, 0);
        for (i, (_, src)) in e.binds.iter().enumerate() {
            let CaptureSrc::Slot(idx) = src else { unreachable!("supported_g_expr admits only Slot binds") };
            let addr = self.slot_addr(*idx as usize);
            self.builder.ins().store(MemFlagsData::trusted(), addr, argv_addr, (i * 8) as i32);
        }
        let site: &'static EscapeSite = Box::leak(Box::new(EscapeSite(e.clone())));
        let site_addr = self.builder.ins().iconst(self.ptr_ty, site as *const EscapeSite as i64);
        let out = self.fresh_scratch();
        let call = self.builder.ins().call(self.g_escape_ref, &[self.ctx_ptr, site_addr, argv_addr, out]);
        let status = self.builder.inst_results(call)[0];
        self.guard_g(status);
        GVal::Owned(out)
    }

    /// F1: `Ir::GlobalRef` -- a bare global reference (not a call). Leaks a
    /// `GlobalRefSite` and calls `jit_g_global_ref`.
    fn lower_g_global_ref(&mut self, chain: &GlobalChain, sym: &Symbol, span: Span) -> GVal {
        let site: &'static GlobalRefSite = Box::leak(Box::new(GlobalRefSite { chain: chain.clone(), sym: sym.clone(), span }));
        let site_addr = self.builder.ins().iconst(self.ptr_ty, site as *const GlobalRefSite as i64);
        let out = self.fresh_scratch();
        let call = self.builder.ins().call(self.g_global_ref_ref, &[self.ctx_ptr, site_addr, out]);
        let status = self.builder.inst_results(call)[0];
        self.guard_g(status);
        GVal::Owned(out)
    }

    fn lower_g_tail_seq(&mut self, body: &[Ir]) -> Option<GVal> {
        let (last, init) = body.split_last().expect("supported_generic guarantees non-empty");
        for e in init {
            let v = self.lower_g_expr(e);
            if let GVal::Owned(addr) = v {
                self.builder.ins().call(self.g_drop_ref, &[addr]);
            }
        }
        self.lower_g_tail(last)
    }

    fn lower_g_tail(&mut self, ir: &Ir) -> Option<GVal> {
        match ir {
            Ir::If { test, then, els } => self.lower_g_tail_if(test, then, els.as_deref()),
            Ir::Do(exprs) => self.lower_g_tail_seq(exprs),
            Ir::Let { binds, body } => self.lower_g_let_tail(binds, body),
            Ir::Loop { binds, scratch_base, body } => self.lower_g_loop(binds, *scratch_base, body),
            Ir::Recur { args, scratch_base } => self.lower_g_recur(args, *scratch_base),
            _ => Some(self.lower_g_expr(ir)),
        }
    }

    fn lower_g_tail_if(&mut self, test: &Ir, then: &Ir, els: Option<&Ir>) -> Option<GVal> {
        let t = self.lower_g_expr(test);
        let truthy = self.lower_truthy(t.addr());
        if let GVal::Owned(a) = t {
            self.builder.ins().call(self.g_drop_ref, &[a]);
        }
        let then_block = self.builder.create_block();
        let else_block = self.builder.create_block();
        let merge_block = self.builder.create_block();
        let result_slot = self.fresh_scratch();
        self.builder.ins().brif(truthy, then_block, &[], else_block, &[]);

        self.builder.switch_to_block(then_block);
        self.builder.seal_block(then_block);
        let tv = self.lower_g_tail(then);
        if let Some(v) = tv {
            self.move_into(v, result_slot);
            self.builder.ins().jump(merge_block, &[]);
        }

        self.builder.switch_to_block(else_block);
        self.builder.seal_block(else_block);
        let ev = match els {
            Some(e) => self.lower_g_tail(e),
            None => {
                let leaked: &'static Value = Box::leak(Box::new(Value::Nil));
                let addr = self.builder.ins().iconst(self.ptr_ty, leaked as *const Value as i64);
                Some(GVal::Borrow(addr))
            }
        };
        if let Some(v) = ev {
            self.move_into(v, result_slot);
            self.builder.ins().jump(merge_block, &[]);
        }

        if tv.is_none() && ev.is_none() {
            self.builder.seal_block(merge_block);
            return None;
        }
        self.builder.switch_to_block(merge_block);
        self.builder.seal_block(merge_block);
        Some(GVal::Owned(result_slot))
    }

    /// F1: evaluates each `Let`/`Loop`-style bind's init and moves it into
    /// its own slot (an `Owned` init raw-copies in, a `Borrow`ed one -- a
    /// param/outer slot read back -- clones), returning the bound indices
    /// for the caller's scope bookkeeping.
    fn lower_g_let_binds(&mut self, binds: &[(CompiledPattern, Ir)]) -> Vec<u16> {
        let mut idxs = Vec::with_capacity(binds.len());
        for (pat, init) in binds {
            let CompiledPattern::Slot(idx) = pat else {
                unreachable!("supported_g_expr/supported_g_tail admit only Slot patterns")
            };
            let v = self.lower_g_expr(init);
            let dst = self.slot_addr(*idx as usize);
            self.move_into(v, dst);
            idxs.push(*idx);
        }
        idxs
    }

    /// F1: `Ir::Let` in value position -- binds, evaluates `body` for its
    /// value, moves that value into a fresh scratch slot (so it survives
    /// past this `Let`'s own slots, which may alias it -- same reason the
    /// fn entry moves its tail result into `out*` before dropping params),
    /// then drops the `Let`'s own bindings.
    fn lower_g_let_expr(&mut self, binds: &[(CompiledPattern, Ir)], body: &[Ir]) -> GVal {
        let idxs = self.lower_g_let_binds(binds);
        self.let_scopes.push(idxs.clone());
        let v = self.lower_g_seq(body);
        self.let_scopes.pop();
        let out = self.fresh_scratch();
        self.move_into(v, out);
        for idx in idxs {
            self.drop_slot(idx as usize);
        }
        GVal::Owned(out)
    }

    /// F1: `Ir::Let` in tail position -- same as `lower_g_let_expr` but the
    /// body may end in a `Recur` (`None`, control already jumped elsewhere;
    /// nothing to move or drop on this path -- see this file's E3a
    /// deviations note on `Let`+outer-`Recur`).
    fn lower_g_let_tail(&mut self, binds: &[(CompiledPattern, Ir)], body: &[Ir]) -> Option<GVal> {
        let idxs = self.lower_g_let_binds(binds);
        self.let_scopes.push(idxs.clone());
        let result = self.lower_g_tail_seq(body);
        self.let_scopes.pop();
        match result {
            Some(gval) => {
                let out = self.fresh_scratch();
                self.move_into(gval, out);
                for idx in idxs {
                    self.drop_slot(idx as usize);
                }
                Some(GVal::Owned(out))
            }
            None => None,
        }
    }

    fn lower_g_loop(&mut self, binds: &[(CompiledPattern, Ir)], scratch_base: u16, body: &[Ir]) -> Option<GVal> {
        let slot_base = scratch_base as usize;
        let init_gvals: Vec<GVal> = binds.iter().map(|(_, init)| self.lower_g_expr(init)).collect();
        for (i, g) in init_gvals.into_iter().enumerate() {
            let dst = self.slot_addr(slot_base + i);
            self.move_into(g, dst);
        }
        let head_block = self.builder.create_block();
        self.builder.ins().jump(head_block, &[]);
        self.builder.switch_to_block(head_block);
        self.loops.push(LoopCtxG { scratch_base, head_block, slot_base, n: binds.len() });
        let result = self.lower_g_tail_seq(body);
        let scope = self.loops.pop().expect("pushed above");
        self.builder.seal_block(head_block);
        match result {
            Some(gval) => {
                let out = self.fresh_scratch();
                self.move_into(gval, out);
                for i in 0..scope.n {
                    self.drop_slot(scope.slot_base + i);
                }
                Some(GVal::Owned(out))
            }
            None => None,
        }
    }

    fn lower_g_recur(&mut self, args: &[Ir], scratch_base: u16) -> Option<GVal> {
        let (slot_base, head_block) = self
            .loops
            .iter()
            .rev()
            .find(|c| c.scratch_base == scratch_base)
            .map(|c| (c.slot_base, c.head_block))
            .expect("supported_generic() matched this recur to an open loop target");
        // Evaluate ALL args against the OLD slot values first, THEN rebind
        // -- `(recur (dec n) (inc acc))` must see the pre-recur `n`/`acc`.
        let vals: Vec<GVal> = args.iter().map(|a| self.lower_g_expr(a)).collect();
        for (i, v) in vals.into_iter().enumerate() {
            self.drop_slot(slot_base + i); // always previously initialised
            let dst = self.slot_addr(slot_base + i);
            self.move_into(v, dst);
        }
        self.builder.ins().jump(head_block, &[]);
        None
    }
}
