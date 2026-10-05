//! The compiled-tier executor: walks an `Ir` tree against a flat slot
//! frame. No `Env` is allocated, no `HashMap` is touched for a local, and
//! `recur` costs zero allocations.
//!
//! ## `Flow::Recur` must unwind exactly like the tree-walker's error signal
//!
//! In the tree-walker `recur` raises an `ErrorKind::Recur` "error" that
//! `?`-propagates out of any partially-evaluated expression up to the
//! nearest enclosing `loop`/fn body. That is observable: `(loop [i 0] (if
//! (< i 3) (+ 1 (recur (inc i))) i))` returns 3, because the pending `(+ 1
//! _)` is simply abandoned. `Flow::Recur` reproduces that: EVERY composite
//! node here must abandon its own work and return `Ok(Flow::Recur)` the
//! moment a subexpression yields it (the `val!` macro below does this), and
//! only `Ir::Loop` and the fn-body trampoline may absorb it.
//!
//! Because a `recur`'s arguments are written straight into the target's
//! scratch slots as they are evaluated (never into a temporary), a `recur`
//! nested inside another `recur`'s arguments naturally wins -- the inner
//! one writes the scratch block and unwinds, and the outer one abandons
//! itself on the way out, leaving precisely the inner one's values behind.
//! That is what the tree-walker does with its nested error signals too.
//!
//! ## Everything else routes through the tree-walker's own code
//!
//! Calls dispatch through `Interp::apply_value`, so natives, keywords/maps/
//! sets/vectors-as-fns, arity errors, and legacy (uncompiled) closures all
//! behave bit-identically; error constructors and message strings are the
//! ones `eval/mod.rs` and `eval/apply.rs` use.

use std::sync::Arc;

use super::ir::{
    CaptureSrc, CatchArm, CompiledPattern, DynBind, Escape, FieldGet, FieldRecv, NewInst, FnTemplate, GlobalChain,
    IntrinOp, Ir, MapEntry, MapPattern, NumBin, NumBranch, NumCmp, NumLoad, NumLoop, NumOp,
    NumSeed, SeqStep, NUM_MAX_BINDS, NUM_REGS,
};
use super::lanes;
use super::{CompiledArity, CompiledClosure};
use crate::builtins::numbers::Num;
use crate::builtins::{map_probe, numbers, predicates};
use crate::env::VarCell;
use crate::error::{ErrorKind, RjError};
use crate::eval::special_forms::{catch_class_matches, error_to_info_map, map_pattern_lookup};
use crate::eval::Interp;
use crate::reader::Span;
use crate::value::{Closure, PMap, PVec, Symbol, Value};

/// A node's result: a value, or "a `recur` unwound past me".
pub enum Flow {
    Val(Value),
    Recur,
}

/// The compiled frame: the call's slots, this instance's captures, and the
/// running closure itself (for a named fn's self-reference).
pub(crate) struct Locals<'a> {
    pub(crate) slots: &'a mut [Value],
    pub(crate) caps: &'a [Value],
    pub(crate) me: &'a Arc<Closure>,
}

/// Evaluate a subexpression, propagating a `recur` unwind immediately.
macro_rules! val {
    ($e:expr) => {
        match $e? {
            Flow::Val(v) => v,
            Flow::Recur => return Ok(Flow::Recur),
        }
    };
}

/// Run a binding step (which produces no value), propagating a `recur`
/// unwind out of a `:or` default form immediately.
macro_rules! bind {
    ($e:expr) => {
        match $e? {
            Flow::Val(_) => {}
            Flow::Recur => return Ok(Flow::Recur),
        }
    };
}

/// `bind!`, but with the plain-symbol pattern -- the overwhelmingly common
/// one, and the one a `loop` re-runs on EVERY iteration -- kept inline as
/// the single slot write it is. Routing it through `exec_pattern`'s outlined
/// call + `Result<Flow, _>` round trip instead measured ~4% on the compiled
/// LCG loop of `bench/flow-gen-sink-w2000.mova`.
macro_rules! bind_pat {
    ($interp:expr, $pat:expr, $v:expr, $l:expr) => {
        match $pat {
            CompiledPattern::Slot(i) => $l.slots[*i as usize] = $v,
            pat => bind!(exec_pattern($interp, pat, $v, $l)),
        }
    };
}

/// One compiled call, from the parameter slots to the `recur` trampoline.
///
/// A MACRO, not a function taking a binder, because both spellings of "share
/// this body" measured as a regression on `bench_call_dispatch`'s compiled
/// arm -- whose entire subject is this code's per-call overhead:
///
/// | shape | vs baseline |
/// |---|---|
/// | generic `#[inline] fn ..(get: impl FnMut(usize) -> Value)` | ~4% slower |
/// | non-generic `fn run_compiled_slots(.., slots: Vec<Value>)` (one extra call frame per mova-level call, even with the binding half `#[inline]`) | ~4% slower |
/// | this macro (one source of truth, two fully independent bodies) | tie |
///
/// So the two entry points below are two complete, separately optimised
/// functions that cannot drift, since there is only one copy of the source.
/// W3e-3: `$bind` is the parameter binder's NAME, pasted as an ident, so a
/// third entry point (`run_compiled_body_lazy_rest`) can bind an
/// already-formed rest seq without walking it while the two hot entry points
/// keep the exact code they measured. It is an ident and not a closure or a
/// generic parameter for the reason in the table above -- both of those
/// shapes cost ~4% here. `$a`/`$b` are that binder's own last two arguments,
/// whatever they are for it.
macro_rules! compiled_call_body {
    ($interp:expr, $rc:expr, $cc:expr, $arity_idx:expr, $bind:ident, $a:expr, $b:expr) => {{
        let arity = &$cc.code.arities[$arity_idx];
        // W4 diet: the slot frame is a pooled buffer (`take_buf` hands back
        // an EMPTY vec, `resize` nil-fills it), returned to the pool on both
        // normal exits below. Error paths (`break Err`/fuel `?`) drop it --
        // a missed reuse on a cold path, never a leak.
        // G1: the threaded native tier, tried before the interpreter loop
        // below. Lowering is a one-shot, correct-by-construction compile
        // (no runtime bail), so `threaded` is `Some` for the rest of the
        // process once lowered. `n_extra` are temp slots the lowering
        // reserved past `n_slots` (see `jit::threaded`).
        let threaded = if crate::jit::enabled() {
            arity.threaded.get_or_lower(&$cc.code, $arity_idx, $rc.arities[$arity_idx].coerce.is_none())
        } else {
            None
        };
        let mut slots = $interp.take_buf();
        let n_extra = threaded.map(|(_, n)| *n as usize).unwrap_or(0);
        slots.resize_with(arity.n_slots + n_extra, || Value::Nil); // K1: no per-slot Clone call
        $bind(&mut slots, arity, $a, $b);
        let mut l = Locals {
            slots: &mut slots[..],
            caps: &$cc.captures,
            me: $rc,
        };
        let base = arity.scratch_base as usize;
        let result = loop {
            let step = match threaded {
                Some((entry, _)) => crate::jit::call_threaded(entry, $interp, &mut l),
                #[cfg(feature = "k2-count")]
                None if crate::jit::enabled() && crate::k2count::census::on() => {
                    let t = crate::k2count::census::enter();
                    let r = exec_body($interp, &arity.body, &mut l);
                    crate::k2count::census::exit(t, || {
                        let k = if arity.body.len() == 1 { crate::jit::census_kind(&arity.body[0]) } else { format!("Do{}", arity.body.len()) };
                        format!("NoEntry[{}]", k)
                    });
                    r
                }
                None => exec_body($interp, &arity.body, &mut l),
            };
            match step {
                Err(e) => break Err(e),
                Ok(Flow::Val(v)) => break Ok(v),
                Ok(Flow::Recur) => {
                    // Fuel: fn SELF-RECUR back-edge (compiled tier). See
                    // `eval::apply::run_closure_trampoline`'s identical
                    // check for the tree-walker twin of this site.
                    $interp.tick_edge()?;
                    // The scratch block sits entirely above the param block,
                    // so one split hands us "the recur args" and "the params
                    // to rebind" as disjoint slices -- and rebinding then goes
                    // through the very same `bind_param_slots` the initial
                    // call used, which is what `run_closure_body` does too (it
                    // calls `bind_params` again per iteration). That matters
                    // for a variadic fn: re-binding WRAPS the recur'd rest
                    // value in a fresh one-element list every iteration, so
                    // `(recur n r)` nests `r` one level deeper each time. Odd,
                    // but it is the tree-walker's long-standing behavior and
                    // the tiers must not disagree about it.
                    let (params, scratch) = l.slots[..arity.n_slots].split_at_mut(base);
                    // K1: only the recur scratch, never G1/G2a temps past `n_slots`.
                    // D9: the compiled twin of
                    // `eval::apply::run_closure_trampoline`'s identical
                    // coercion -- a self-recur is still a call to this arity,
                    // so its arguments need the same `^long`/`^double`
                    // coercion the initial call gets from `apply_closure`.
                    // `casts` is parallel to the FIXED params only and
                    // `$rc.arities` is 1:1 with `$cc.code.arities` (see
                    // `CompiledFn`'s doc), so `$arity_idx` addresses the
                    // same arity in both; the zip stops at `casts.len()`,
                    // leaving a trailing rest value in `scratch` untouched.
                    if let Some(casts) = &$rc.arities[$arity_idx].coerce {
                        numbers::coerce_prim_params_in_place(casts, scratch)?;
                    }
                    // MOVED out of the scratch block, not cloned. A scratch
                    // slot is written only by `Ir::Recur` and read only here,
                    // and the next `recur` rewrites it before it can be read
                    // again -- so the `Nil` left behind is unobservable,
                    // exactly as for `Ir::LoadSlotTake` (`compile::lastuse`).
                    // This is not bookkeeping: leaving the value behind here
                    // would keep a SECOND live handle on a `recur`'d
                    // accumulator for the whole of the next iteration, which
                    // is precisely the handle that makes `(loop [m {}]
                    // .. (recur (assoc m k v)))` fail to be unique no matter
                    // what the analysis proves about `m`'s slot.
                    // mova campaign (clojure-lsp): `bind_param_slots_recur`,
                    // NOT `bind_param_slots` -- see that fn's doc, and its
                    // tree-walker twin `eval::apply::bind_params_recur`,
                    // for why a self-`recur` to a variadic arity must bind
                    // its trailing scratch value to the `& rest` slot
                    // AS-IS rather than wrapped in a fresh one-element
                    // `List`.
                    bind_param_slots_recur(
                        params,
                        arity,
                        scratch.iter_mut().map(|v| std::mem::replace(v, Value::Nil)),
                    );
                }
            }
        };
        drop(l);
        $interp.put_buf(slots);
        result
    }};
}

/// Runs one call of a compiled closure. The caller (`apply_closure`) has
/// already selected the arity index, checked the depth guard, and pushed
/// the stack frame -- exactly as it does for the tree-walked path.
pub(crate) fn run_compiled_body(
    interp: &mut Interp,
    rc: &Arc<Closure>,
    cc: &CompiledClosure,
    arity_idx: usize,
    args: &[Value],
) -> Result<Value, RjError> {
    // The initial call CLONES: `args` is borrowed from a caller that still
    // owns it (`apply_closure` takes `&[Value]`).
    compiled_call_body!(
        interp,
        rc,
        cc,
        arity_idx,
        bind_param_slots,
        args.len(),
        args.iter().cloned()
    )
}

/// W3e-3: [`run_compiled_body`] for `apply`'s lazy-rest path -- `fixed` goes
/// into the positional slots and `rest` into the `& rest` slot AS IS, never
/// walked. See `eval::apply::Interp::apply_closure_lazy_rest` (the tier-
/// neutral caller) for why `apply` must not realize a variadic rest arg, and
/// `bind_param_slots_lazy_rest` for the binding rule.
pub(crate) fn run_compiled_body_lazy_rest(
    interp: &mut Interp,
    rc: &Arc<Closure>,
    cc: &CompiledClosure,
    arity_idx: usize,
    fixed: Vec<Value>,
    rest: Value,
) -> Result<Value, RjError> {
    compiled_call_body!(interp, rc, cc, arity_idx, bind_param_slots_lazy_rest, fixed, rest)
}

/// [`run_compiled_body`] for a caller that has handed its arguments over
/// (Perceus-lite phase 3, `eval::apply::apply_closure_buf`): each argument is
/// MOVED into its parameter slot, so the caller's buffer stops holding a
/// second handle on it for the duration of the body. That second handle is
/// exactly what phase 2 measured as the remaining pin on `reduce`'s
/// accumulator -- with it gone, a `LoadSlotTake` of the parameter really is
/// the only handle, and the receiver mutates in place.
///
/// The buffer is borrowed rather than taken by value so a loop can reuse it:
/// the `Value`s move out, the backing store stays with the caller. (Taking a
/// `Vec` by value cost a malloc/free pair per element -- a ~7% regression on
/// `(reduce + 0 (range N))`, a shape that gains nothing from the handover
/// because `+` is a native.) What is left behind is `Value::Nil`s, never the
/// arguments. This is the hotter of the two entry points: after phase 3
/// every compiled call site (`finish_call`) reaches a closure through here.
pub(crate) fn run_compiled_body_buf(
    interp: &mut Interp,
    rc: &Arc<Closure>,
    cc: &CompiledClosure,
    arity_idx: usize,
    args: &mut [Value],
) -> Result<Value, RjError> {
    let n = args.len();
    compiled_call_body!(
        interp,
        rc,
        cc,
        arity_idx,
        bind_param_slots,
        n,
        args.iter_mut().map(|v| std::mem::replace(v, Value::Nil))
    )
}

/// W3e-3: [`bind_param_slots`] for `apply`'s lazy-rest path. `fixed` has
/// exactly `arity.n_params` values (`variadic_apply_shape` guarantees it)
/// and `rest` is already the seq the `& rest` param should see, so this
/// binder does no wrapping and no walking at all -- the whole point being
/// that `rest` may be infinite.
#[inline]
fn bind_param_slots_lazy_rest(
    slots: &mut [Value],
    arity: &CompiledArity,
    fixed: Vec<Value>,
    rest: Value,
) {
    debug_assert!(arity.variadic, "lazy-rest binding is only ever used on a variadic arity");
    for (slot, v) in slots.iter_mut().take(arity.n_params).zip(fixed) {
        *slot = v;
    }
    slots[arity.n_params] = rest;
}

/// Writes the `n_args` arguments into an arity's parameter slots,
/// byte-for-byte like `eval::apply::bind_params`: positional params by
/// index, and the `& rest` param as `nil` when there are no extras (NOT an
/// empty list), otherwise a `List` of them.
///
/// The arguments arrive as an ITERATOR consumed strictly in order, which is
/// how the ONE copy of this rule serves both the initial-call callers: the
/// initial call clones out of a borrowed slice, a handed-over call MOVES
/// out of the caller's buffer. `impl Iterator` rather than `&mut dyn` (or
/// an index getter): this runs on every compiled call, and both an
/// indirect call and a bounds check per argument are exactly the kind of
/// cost `bench_call_dispatch` exists to catch.
///
/// NOT used for the `recur` back-edge any more -- see
/// `bind_param_slots_recur`'s doc for why a self-recur to a variadic
/// arity needs different (non-wrapping) rest-binding semantics than a
/// genuine external call with one extra positional argument.
#[inline]
fn bind_param_slots(
    slots: &mut [Value],
    arity: &CompiledArity,
    n_args: usize,
    mut vals: impl Iterator<Item = Value>,
) {
    for slot in slots.iter_mut().take(arity.n_params.min(n_args)) {
        *slot = vals.next().unwrap_or(Value::Nil);
    }
    if arity.variadic {
        let start = arity.n_params.min(n_args);
        slots[arity.n_params] = if start >= n_args {
            Value::Nil
        } else {
            Value::List(vals.take(n_args - start).collect())
        };
    }
}

/// S8 (mova campaign, real bug fix): [`bind_param_slots`]'s twin for the
/// `recur` back-edge only -- see `eval::apply::bind_params_recur`'s doc
/// for the oracle-verified reasoning (both tiers must agree, and now
/// agree with the JVM too). The caller (`compiled_call_body!`'s
/// `Flow::Recur` arm) already sized `scratch` to exactly `arity.n_recur
/// == arity.n_params + variadic` slots, so this always has exactly
/// enough values: no `n_args`/`min`/`take` bookkeeping needed, and the
/// trailing value (when variadic) is written to the rest slot AS-IS,
/// never re-wrapped in a `List`.
#[inline]
fn bind_param_slots_recur(slots: &mut [Value], arity: &CompiledArity, mut vals: impl Iterator<Item = Value>) {
    for slot in slots.iter_mut().take(arity.n_params) {
        *slot = vals.next().unwrap_or(Value::Nil);
    }
    if arity.variadic {
        slots[arity.n_params] = vals.next().unwrap_or(Value::Nil);
    }
}

/// A `do`-style body: every form for effect, the last form's value (`nil`
/// for an empty body) -- same contract as `eval_do_body`.
pub(crate) fn exec_body(interp: &mut Interp, body: &[Ir], l: &mut Locals) -> Result<Flow, RjError> {
    let mut out = Value::Nil;
    for ir in body {
        out = val!(exec(interp, ir, l));
    }
    Ok(Flow::Val(out))
}

/// The dispatcher deliberately keeps NOTHING but the match in its own
/// frame, delegating every arm that needs locals to an outlined helper.
/// `exec` recurses once per nested expression, so its frame size is
/// multiplied by the mova-level call depth; an unoptimized build gives a
/// fat match arm's locals their own stack slots whether or not that arm
/// runs, and letting all of them accumulate here made a compiled fn blow
/// the real Rust stack *before* `apply_closure`'s `max_depth` guard could
/// trip (which is the whole point of that guard). Keep it this way.
pub(crate) fn exec(interp: &mut Interp, ir: &Ir, l: &mut Locals) -> Result<Flow, RjError> {
    #[cfg(feature = "k2-count")]
    {
        crate::k2count::init();
        crate::k2count::node(match ir {
            Ir::Const{..} => 0,
            Ir::LoadSlot{..} => 1,
            Ir::LoadSlotTake{..} => 2,
            Ir::LoadCapture{..} => 3,
            Ir::SelfRef{..} => 4,
            Ir::GlobalRef{..} => 5,
            Ir::CreationEnvLookup{..} => 6,
            Ir::If{..} => 7,
            Ir::Do{..} => 8,
            Ir::Let{..} => 9,
            Ir::Loop{..} => 10,
            Ir::NumLoop{..} => 11,
            Ir::MakeClosure{..} => 12,
            Ir::MakeRecGroup{..} => 13,
            Ir::SiblingRef{..} => 14,
            Ir::Try{..} => 15,
            Ir::Def{..} => 16,
            Ir::DynBind{..} => 17,
            Ir::Recur{..} => 18,
            Ir::Call { callee, .. } if matches!(&**callee, Ir::Const(Value::Keyword(_))) => 30,
            Ir::Call { callee, .. } if matches!(&**callee, Ir::LoadSlot(_) | Ir::LoadSlotTake(_) | Ir::LoadCapture(_)) => 31,
            Ir::Call{..} => 19,
            Ir::CallGlobal{..} => 20,
            Ir::CallCreationEnv{..} => 21,
            Ir::Intrinsic{..} => 22,
            Ir::VectorLit{..} => 23,
            Ir::SetLit{..} => 24,
            Ir::MapLit{..} => 25,
            Ir::Throw{..} => 26,
            Ir::Escape{..} => 27,
            Ir::FieldGet{..} => 28,
            Ir::SetMutField{..} => 29,
            Ir::New{..} => 27,
        });
    }
    match ir {
        Ir::Const(v) => Ok(Flow::Val(v.clone())),
        Ir::LoadSlot(i) => Ok(Flow::Val(l.slots[*i as usize].clone())),
        // The moving read (Perceus-lite phase 2). `compile::lastuse` proved
        // no path from here reaches another read of this slot, so leaving
        // `Nil` behind is unobservable -- and the handle handed on may now
        // be the only one, which is what lets `builtins::reuse`'s consuming
        // natives mutate in place instead of copying. Cheaper than the
        // clone it replaces for EVERY value type, not just collections: one
        // move plus one `Nil` store, versus a refcount bump now and a
        // decrement when the frame is dropped.
        Ir::LoadSlotTake(i) => Ok(Flow::Val(std::mem::replace(
            &mut l.slots[*i as usize],
            Value::Nil,
        ))),
        Ir::LoadCapture(i) => Ok(Flow::Val(l.caps[*i as usize].clone())),
        Ir::SelfRef => Ok(Flow::Val(Value::Fn(l.me.clone()))),
        Ir::GlobalRef { chain, sym, span } => match chain.get() {
            Some(v) => Ok(Flow::Val(v)),
            None => Err(unresolved(interp, sym, *span)),
        },
        Ir::CreationEnvLookup { sym, chain, span } => match creation_env_get(l, sym, chain) {
            Some(v) => Ok(Flow::Val(v)),
            None => Err(unresolved(interp, sym, *span)),
        },
        Ir::If { test, then, els } => {
            if val!(exec(interp, test, l)).truthy() {
                exec(interp, then, l)
            } else if let Some(e) = els {
                exec(interp, e, l)
            } else {
                Ok(Flow::Val(Value::Nil))
            }
        }
        Ir::Do(body) => exec_body(interp, body, l),
        Ir::Let { binds, body } => exec_let(interp, binds, body, l),
        Ir::Loop {
            binds,
            scratch_base,
            body,
        } => exec_loop(interp, binds, *scratch_base, body, l),
        Ir::NumLoop(nl) => exec_num_loop(interp, nl, l),
        Ir::MakeClosure { template, caps } => {
            Ok(Flow::Val(make_closure(interp, template, caps, l)?))
        }
        Ir::MakeRecGroup { members, slots } => exec_make_rec_group(interp, members, slots, l),
        Ir::SiblingRef(i) => Ok(Flow::Val(Value::Fn(sibling(interp, *i, l)?))),
        Ir::Try {
            body,
            catches,
            finally,
        } => exec_try(interp, body, catches, finally.as_deref(), l),
        Ir::Def { cell, value } => exec_def(interp, cell, value.as_deref(), l),
        Ir::DynBind(d) => exec_dyn_bind(interp, d, l),
        Ir::Recur { args, scratch_base } => exec_recur(interp, args, *scratch_base, l),
        Ir::Call { callee, args, span } => exec_call(interp, callee, args, *span, l),
        Ir::CallGlobal {
            chain,
            sym,
            sym_span,
            args,
            span,
        } => exec_call_global(interp, chain, sym, *sym_span, args, *span, l),
        Ir::CallCreationEnv {
            sym,
            chain,
            sym_span,
            args,
            span,
        } => exec_call_creation_env(interp, sym, chain, *sym_span, args, *span, l),
        Ir::Intrinsic {
            op,
            chain,
            sym,
            sym_span,
            args,
            span,
        } => exec_intrinsic(interp, *op, chain, sym, *sym_span, args, *span, l),
        Ir::VectorLit(items) => exec_vector(interp, items, l),
        Ir::SetLit(items) => exec_set(interp, items, l),
        Ir::MapLit(pairs) => exec_map(interp, pairs, l),
        Ir::Throw { value, span } => exec_throw(interp, value, *span, l),
        Ir::Escape(e) => exec_escape(interp, e, l),
        Ir::FieldGet(fg) => exec_field_get(interp, fg, l),
        Ir::New(n) => exec_new(interp, n, l),
        Ir::SetMutField { owner_slot, field_slot, field_name, ic, value, span } => {
            exec_set_mut_field(interp, *owner_slot, *field_slot, field_name, ic, value, *span, l)
        }
    }
}

/// `Ir::SetMutField` (lsp/setf): a compiled `set!` on a deftype's own
/// mutable field. Mirrors `eval_set_bang`'s mutable-field arm exactly:
/// write `inst.fields[idx]` under lock (visible through every `Arc
/// <InstVal>` handle, `owner`'s clone included), then mirror the new
/// value into THIS frame's `field_slot` so a later bare read of the field
/// in this call sees it -- matching `env.set_local_in_place`.
///
/// `owner_slot`/`field_slot` are always plain frame slots: `resolve.rs`
/// only builds this node from the `wrap_fields_let` paired-local shape
/// (`f` + `__mutfield_owner_f`), which `FnCtx::lookup` never resolves
/// across a nested-`fn` capture boundary -- that shape still bails to the
/// tree-walker, unchanged.
///
/// The three `return Err(..)` arms below are unreachable in practice --
/// the paired-local shape only ever exists because `wrap_fields_let`
/// built it for a field this exact type declares mutable -- and exist
/// only so a future change to that invariant fails loudly instead of
/// silently mutating the wrong slot.
#[allow(clippy::too_many_arguments)]
#[inline(never)] // K1: keep exec()'s frame small, no TLS in its prologue
fn exec_set_mut_field(
    interp: &mut Interp,
    owner_slot: u16,
    field_slot: u16,
    field_name: &crate::value::Str,
    ic: &super::ir::FieldIc,
    value: &Ir,
    span: Span,
    l: &mut Locals,
) -> Result<Flow, RjError> {
    let v = val!(exec(interp, value, l));
    let inst = match &l.slots[owner_slot as usize] {
        Value::Inst(inst) => inst.clone(),
        _ => return Err(internal_set_mut_field_err(interp, span)),
    };
    let ptr = Arc::as_ptr(&inst.tdef) as usize;
    let idx = match ic.probe(ptr) {
        Some(idx) => idx as usize,
        None => {
            let Some(idx) = inst.tdef.basis.iter().position(|b| b.as_ref() == field_name.as_ref())
            else {
                return Err(internal_set_mut_field_err(interp, span));
            };
            if !inst.tdef.mutable.get(idx).copied().unwrap_or(false) {
                return Err(internal_set_mut_field_err(interp, span));
            }
            ic.install(ptr, inst.tdef.clone(), idx as u32);
            idx
        }
    };
    crate::sync::lock_mutex(&inst.fields).set(idx, v.clone());
    l.slots[field_slot as usize] = v.clone();
    Ok(Flow::Val(v))
}

fn internal_set_mut_field_err(interp: &Interp, span: Span) -> RjError {
    RjError::other("internal: compiled set!'s mutable-field invariant violated")
        .with_span(span)
        .with_stack(interp.stack_snapshot(), interp.source_id)
}

/// K5: `Ir::New` -- class checked BEFORE args run (as `eval_new`); a miss runs the Escape verbatim.
#[inline(never)]
fn exec_new(interp: &mut Interp, n: &NewInst, l: &mut Locals) -> Result<Flow, RjError> {
    let Some(tdef) = interp.new_fast_class(&n.class, &l.me.env, n.args.len()) else {
        return exec(interp, &n.fallback, l);
    };
    let mut vals = Vec::with_capacity(n.args.len());
    for a in &n.args {
        vals.push(val!(exec(interp, a, l)));
    }
    interp.new_fast_make(&tdef, &vals, n.span).map(Flow::Val)
}

/// `Ir::FieldGet` (W-FIELDGET): a compiled `(.-field local)`.
///
/// The fast path is what `eval::types_forms::inst_field` computes, with the
/// keyword built at compile time and the basis scan cached per type -- see
/// `ir::FieldGet` for why that is the tree-walker's answer exactly, and why
/// the cache never needs invalidating. Anything it does not answer runs the
/// `Ir::Escape` this node was built from, unchanged.
#[inline(never)] // K1: keep exec()'s frame small, no TLS in its prologue
fn exec_field_get(interp: &mut Interp, fg: &FieldGet, l: &mut Locals) -> Result<Flow, RjError> {
    // Scoped so the frame borrow ends before `exec` takes `l` mutably.
    let hit = {
        let recv = match fg.recv {
            FieldRecv::Slot(i) => &l.slots[i as usize],
            FieldRecv::Capture(i) => &l.caps[i as usize],
        };
        // K1: see through `with-meta` like `eval_dot_form`'s `into_unmeta` (kondo nodes carry meta).
        match recv.unmeta() {
            Value::Inst(inst) if inst.tdef.is_record => {
                // A record keeps its fields in the map view (basis fields
                // AND any `assoc`ed extension keys), keyed by keyword --
                // per INSTANCE, so there is no per-type index to cache. What
                // the compiled node saves is the keyword `inst_field` mints
                // on every single call.
                inst.data.get(&fg.kw).cloned()
            }
            Value::Inst(inst) => {
                let ptr = Arc::as_ptr(&inst.tdef) as usize;
                match fg.ic.probe(ptr) {
                    Some(idx) => {
                        crate::lens::event(crate::lens::Event::FieldIcHit);
                        crate::sync::lock_mutex(&inst.fields).get_owned(idx as usize)
                    }
                    None => {
                        crate::lens::event(crate::lens::Event::FieldIcMiss);
                        match inst
                            .tdef
                            .basis
                            .iter()
                            .position(|b| b.as_ref() == fg.field.as_ref())
                        {
                            Some(idx) => {
                                fg.ic.install(ptr, inst.tdef.clone(), idx as u32);
                                crate::sync::lock_mutex(&inst.fields).get_owned(idx)
                            }
                            // Not a field of this type at all. Nothing to
                            // cache (a negative answer would have to be
                            // invalidated); the escape raises the
                            // tree-walker's own error at its own span.
                            None => None,
                        }
                    }
                }
            }
            _ => None,
        }
    };
    match hit {
        Some(v) => Ok(Flow::Val(v)),
        None => exec(interp, &fg.fallback, l),
    }
}

/// `Ir::Escape` (field3/W-RESOLVE): the one node that hands control back to
/// the tree-walker, for exactly one interop form.
///
/// Everything semantic about the escaped form -- its value, the kind /
/// message / span / stack of anything it throws, the order of its side
/// effects, which dynamic bindings (`*out*`, `*err*`, ...) it sees -- comes
/// from `eval_form_in` evaluating the SAME form the tree-walk tier would
/// have evaluated, so none of it can drift. This function's whole job is
/// the env.
///
/// The env is one child frame of `l.me.env`, the closure's creation env:
///
/// - the frame itself carries this fn's (and any enclosing compiled fn's)
///   locals, read out of the running slots/captures under the names they
///   were written with -- see `ir::Escape` for why that set is exact;
/// - its PARENT is the live creation-env chain, so everything outside the
///   compiled region resolves exactly as `Ir::CreationEnvLookup` would
///   resolve it, re-probed now rather than snapshotted.
///
/// One frame per execution, not one per closure: it must not outlive the
/// call... except when it legitimately does, because the escaped form
/// built a closure over it (`(Thread. (fn [] .. barrier ..))`). `Env` is
/// `Arc`-backed, so that case just keeps the frame alive, and the values it
/// holds are the ones the tree-walker would have closed over -- the
/// tree-walk `loop` allocates a fresh child env per iteration
/// (`eval_loop`), which is the same by-value snapshot this frame is.
///
/// `Flow::Recur` is unreachable here by construction: `compile_escape`
/// refuses to escape any subtree containing a literal `recur`, so a
/// `recur` unwind can never originate inside one.
#[inline(never)] // K1: keep exec()'s frame small, no TLS in its prologue
fn exec_escape(interp: &mut Interp, e: &Escape, l: &mut Locals) -> Result<Flow, RjError> {
    // field4/W-LENS-1: THE regret event this node exists to make visible.
    // `MOVA_EXPLAIN` already says "this fn has N escapes"; this says how
    // many times they actually ran, which is the half session 12 was
    // missing. One TLS load + one uncontended `Relaxed` load/store, against
    // a node that is about to allocate an env frame and re-enter the
    // tree-walker -- immeasurable next to what follows it.
    crate::lens::event_at(crate::lens::Event::EscapeExec, e.lens_site);
    let env = l.me.env.child();
    for (sym, src) in &e.binds {
        let v = match src {
            CaptureSrc::Slot(i) => l.slots[*i as usize].clone(),
            CaptureSrc::Capture(i) => l.caps[*i as usize].clone(),
            CaptureSrc::SelfRef => Value::Fn(l.me.clone()),
            // Unreachable: `compile_escape` bails the whole fn rather than
            // bridge a recursive-binding-group sibling into a tree-walked
            // frame (which is where the cycle this feature avoids would come
            // straight back). Kept as an explicit refusal rather than a
            // `todo!` so a future widening of the bridge fails safe.
            CaptureSrc::Sibling(_) => {
                return Err(internal_sibling_error(
                    interp,
                    "in an interop escape's env bridge",
                ))
            }
        };
        env.set(sym.clone(), v);
    }
    interp.eval_form_in(&e.form, &env).map(Flow::Val)
}

#[inline(never)] // K1: keep exec()'s frame small, no TLS in its prologue
fn exec_let(
    interp: &mut Interp,
    binds: &[(CompiledPattern, Ir)],
    body: &[Ir],
    l: &mut Locals,
) -> Result<Flow, RjError> {
    for (pat, init) in binds {
        let v = val!(exec(interp, init, l));
        bind_pat!(interp, pat, v, l);
    }
    exec_body(interp, body, l)
}

#[inline(never)] // K1: keep exec()'s frame small, no TLS in its prologue
fn exec_loop(
    interp: &mut Interp,
    binds: &[(CompiledPattern, Ir)],
    scratch_base: u16,
    body: &[Ir],
    l: &mut Locals,
) -> Result<Flow, RjError> {
    for (pat, init) in binds {
        // A `recur` inside an init targets the ENCLOSING loop/fn (the
        // compiler emitted it against that scratch block), so it must
        // escape this loop rather than restart it -- exactly how
        // `eval_loop`'s `?` on the init lets the signal past.
        let v = val!(exec(interp, init, l));
        bind_pat!(interp, pat, v, l);
    }
    let base = scratch_base as usize;
    loop {
        match exec_body(interp, body, l)? {
            Flow::Val(v) => return Ok(Flow::Val(v)),
            Flow::Recur => {
                // Fuel: `loop` back-edge (compiled tier). See
                // `eval::special_forms::eval_loop`'s identical check for the
                // tree-walker twin of this site.
                interp.tick_edge()?;
                // The scratch block holds one RAW value per binding pair;
                // each is re-destructured through its pattern, exactly like
                // `eval_loop` re-runs `bind_pattern` against its `__loopN`
                // values every iteration. (For the plain-symbol case that is
                // one slot copy, as before.)
                for (i, (pat, _)) in binds.iter().enumerate() {
                    // MOVED, not cloned -- see the identical note in
                    // `run_compiled_body`'s trampoline. A scratch slot is
                    // written only by `Ir::Recur` and read only here, and
                    // the next `recur` rewrites it before any read, so the
                    // `Nil` left behind is unobservable; leaving the value
                    // there instead would keep a second handle alive on a
                    // recur'd accumulator for the whole next iteration.
                    let v = std::mem::replace(&mut l.slots[base + i], Value::Nil);
                    bind_pat!(interp, pat, v, l);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The specialized numeric loop (`ir::NumLoop`)
//
// See `ir::NumLoop` for the grammar, the four deopt conditions D1-D4 and the
// invariants I1-I7 that make an entry-only guard sufficient. The rule for
// this section is that the inner loop touches NOTHING but a stack-local
// register file: no `Value`, no frame slot, no `Interp`, no atomic, no
// `Result`, no call. Every condition that could make it wrong has already
// been checked at entry, and I2 is why that is enough.
// ---------------------------------------------------------------------------

/// Reads one register. Masked rather than bounds-checked: `resolve.rs`
/// allocates every register below `NUM_REGS`, which is a power of two, AND
/// re-walks the finished node to prove no index escaped that bound
/// (`validate_regs`, invariant I7). So the mask never changes an index; it
/// only saves the compiler from having to prove that itself.
#[inline(always)]
fn reg(i: u8, regs: &[Num; NUM_REGS]) -> Num {
    debug_assert!((i as usize) < NUM_REGS);
    regs[(i as usize) & (NUM_REGS - 1)]
}

/// Runs a straight-line op list against the register file. Each op is the
/// `builtins::numbers` function its builtin folds with, so overflow
/// promotion is shared code.
///
/// S5 (SPEC-numtower): the step fns became fallible when checked `i64`
/// arithmetic started THROWING on overflow instead of promoting to `f64`,
/// so this returns `Result<(), Overflow>` -- a zero-sized error, and the
/// only failure the register machine has besides fuel exhaustion. The
/// success path did not gain any work: `checked_add` & co already branched
/// on the overflow bit, and the `?` here rides that same branch.
#[inline(always)]
fn run_num_ops(ops: &[NumOp], regs: &mut [Num; NUM_REGS]) -> Result<(), numbers::Overflow> {
    for op in ops {
        let a = reg(op.a, regs);
        let b = reg(op.b, regs);
        let v = match op.op {
            NumBin::Add => numbers::add(a, b)?,
            NumBin::Sub => numbers::sub(a, b)?,
            NumBin::Mul => numbers::mul(a, b)?,
            // The identity step is performed, not elided -- see `NumBin`.
            NumBin::AddFold => numbers::add(numbers::add(Num::I(0), a)?, b)?,
            NumBin::MulFold => numbers::mul(numbers::mul(Num::I(1), a)?, b)?,
        };
        debug_assert!((op.dst as usize) < NUM_REGS);
        regs[(op.dst as usize) & (NUM_REGS - 1)] = v;
    }
    Ok(())
}

/// `Ir::NumLoop`: check every deopt condition ONCE, seed the registers, then
/// run.
///
/// `ir::NumLoop` enumerates D1-D4; this is all four of them, and there is no
/// other exit from the specialization. Every one of them lands on
/// `nl.fallback`, which IS the generic `Ir::Loop` this node was built from,
/// so a deopt costs one extra pure read of each seed/invariant (I4) and then
/// behaves exactly as an unspecialized build would -- same value, same
/// error, same wording, same span, same iteration.
#[inline(never)] // K1: keep exec()'s frame small, no TLS in its prologue
fn exec_num_loop(interp: &mut Interp, nl: &NumLoop, l: &mut Locals) -> Result<Flow, RjError> {
    let mut regs = [Num::I(0); NUM_REGS];
    // D1 + D2: every builtin this loop absorbed must still be the pristine
    // native it compiled against, and must still be what its name resolves
    // to. This is the same test `exec_intrinsic` makes per operation,
    // hoisted to loop entry -- equivalent, not merely cheaper, because the
    // loop calls nothing (I2).
    if !nl.guards.iter().all(GlobalChain::intrinsic_armed) {
        return exec(interp, &nl.fallback, l);
    }
    // D3: every seed must actually be a number. Registers `0..seeds.len()`
    // are the bindings, in binding order; `build_num_loop` caps that at
    // `NUM_MAX_BINDS <= NUM_REGS`, so the direct index is in range.
    for (i, s) in nl.seeds.iter().enumerate() {
        regs[i] = match s {
            NumSeed::Const(n) => *n,
            NumSeed::Slot(idx) => match numbers::num_of(&l.slots[*idx as usize]) {
                Some(n) => n,
                None => return exec(interp, &nl.fallback, l),
            },
        };
    }
    // D4: the loop-invariant reads, which are numbers-or-fallback too. Read
    // once here and never again (I3).
    for (r, src) in &nl.loads {
        let v = match src {
            NumLoad::Slot(i) => &l.slots[*i as usize],
            NumLoad::Capture(i) => &l.caps[*i as usize],
        };
        match numbers::num_of(v) {
            Some(n) => regs[(*r as usize) & (NUM_REGS - 1)] = n,
            None => return exec(interp, &nl.fallback, l),
        }
    }
    // The constant pool, once per entry rather than per operand read. Its
    // registers are disjoint from the bindings' and the invariants' (one
    // monotone allocator, `NumCtx::temp`), so the write order above and here
    // is not load-bearing.
    for (r, n) in &nl.consts {
        regs[(*r as usize) & (NUM_REGS - 1)] = *n;
    }
    // Past this point nothing can fail and nothing can unwind for VALUE
    // computation (I2, I6) -- fuel exhaustion is the one deliberate
    // exception. Rather than thread `&mut Interp` (or even just `&mut
    // Option<u64>` pointing at `interp.fuel`) through the register-only
    // inner loop, take an independent `Copy` local of the budget here and
    // write the remainder back on exit: that keeps `run_num_loop`'s hot
    // path a check-and-branch against a stack local with no `Interp`
    // borrow, no `Result` in its signature for the common (`None`) case's
    // sake, and no possibility of a native call sneaking in through an
    // `&mut Interp` some future edit might be tempted to use for something
    // else. When `fuel` is `None` this is one predicted-not-taken branch
    // per iteration; `bench/fuel-lcg.mova` measures exactly that cost (see
    // `bench/optimization-log.md`'s fuel section for the verdict).
    // P0c: unlimited fuel is seeded as `Some(u64::MAX)` so the one existing
    // per-iteration branch also polls the interrupt flag every 65536 steps.
    let unlimited = interp.fuel.is_none();
    let mut fuel_local = interp.fuel.or(Some(u64::MAX));
    // SAFETY: `interp.intr`'s Arc outlives this call; nothing in the register
    // loop can replace it (no `&mut Interp` is reachable from there).
    let intr_ref: &crate::interrupt::Interrupt = unsafe { &*std::sync::Arc::as_ptr(&interp.intr) };
    match run_num_loop_lane_aware(nl, &mut regs, &mut fuel_local, intr_ref) {
        NumLoopExit::Val(n) => {
            if !unlimited { interp.fuel = fuel_local; }
            Ok(Flow::Val(numbers::num_to_value(n)))
        }
        // W-NUMLOOP: the loop took a nil-terminal branch (`NumBranch::
        // RetNil`). Exactly the `Value::Nil` the generic `Ir::Loop` would
        // have produced for the same `if`, with the same fuel accounting as
        // the numeric exit above.
        NumLoopExit::Nil => {
            if !unlimited { interp.fuel = fuel_local; }
            Ok(Flow::Val(Value::Nil))
        }
        NumLoopExit::FuelExhausted if fuel_local != Some(0) => {
            // P0c: stopped by the interrupt poll, not by an empty budget.
            if !unlimited { interp.fuel = fuel_local; }
            Err(numloop_interrupt_err(interp))
        }
        NumLoopExit::FuelExhausted => {
            // `fuel_local` was `Some(0)` on the iteration that stopped the
            // loop (checked-before-decrement, same rule as `Interp::
            // tick_fuel`); write that back so a subsequent checked back-edge
            // (there won't be one -- this error propagates straight out --
            // but a future caller reusing `interp` after catching it at the
            // host level should still see an exhausted, not silently
            // reset, budget) sees zero rather than the pre-loop value.
            interp.fuel = Some(0);
            Err(RjError::fuel_exhausted("fuel exhausted").with_stack(interp.stack_snapshot(), interp.source_id))
        }
        // S5: an `i64` overflow DEOPTS rather than raising here. Raising
        // directly would produce the right message with the wrong span
        // (the register machine has no per-op span to attach), and
        // `tests/differential_test.rs` compares the two tiers' errors by
        // kind + span + message, not just message.
        //
        // Re-running `nl.fallback` -- the very `Ir::Loop` this node was
        // built from -- from the START is sound for exactly the reason
        // every other `NumLoop` deopt is (invariant I2: the specialized
        // body is total and observation-free, so "some iterations already
        // ran in registers" is indistinguishable from "none did"), and it
        // reproduces the unspecialized error exactly: same span, same
        // stack, same wording, same iteration. `interp.fuel` was never
        // written during the specialized run -- only the `fuel_local`
        // copy was -- so the fallback re-consumes the identical budget
        // and cannot see a double charge. The cost is re-running the loop
        // up to the overflow, paid once, on a path that is about to
        // abort anyway.
        NumLoopExit::Overflow => exec(interp, &nl.fallback, l),
    }
}

/// What the inner numeric-loop trampoline exited with: an ordinary
/// computed value, or "the fuel-local counter reached zero" -- the latter
/// only possible when `exec_num_loop` seeded `run_num_loop` with `Some(_)`.
enum NumLoopExit {
    Val(Num),
    /// W-NUMLOOP: a nil-terminal branch ran; the loop's value is
    /// `Value::Nil`. A separate variant rather than a `Value` payload on
    /// `Val` so the numeric path keeps carrying a bare `Num`.
    Nil,
    FuelExhausted,
    /// S5: a checked `i64` op overflowed. `NumLoop`'s invariant I2 ("total
    /// arithmetic, nothing can fail between entry and exit") now has
    /// exactly TWO deliberate exceptions rather than one -- fuel, and
    /// this. Both are still observation-free: the loop stops at the op
    /// that would have been wrong, and `exec_num_loop` raises the very
    /// same `ArithmeticException: long overflow` the untiered evaluator
    /// would have raised at the same point.
    Overflow,
}

/// The inner loop itself: allocation-free and entirely in registers except
/// for the one `fuel` local, which is `None` (a single predicted-not-taken
/// branch, no counter touched) unless the embedder set a budget.
const INTR_MASK: u64 = 0xFFFF;

/// Cold: build the interrupt error for a NumLoop stopped by the poll.
#[cold]
#[inline(never)]
fn numloop_interrupt_err(interp: &mut Interp) -> RjError {
    match interp.intr.take_err_loop() {
        Some(e) => e.with_stack(interp.stack_snapshot(), interp.source_id),
        None => RjError::other("numloop: spurious stop"),
    }
}

fn run_num_loop(nl: &NumLoop, regs: &mut [Num; NUM_REGS], fuel: &mut Option<u64>, intr: &crate::interrupt::Interrupt) -> NumLoopExit {
    let n = nl.seeds.len();
    loop {
        match run_num_loop_one_iter(nl, regs, fuel, n, intr) {
            NumLoopIter::Recurred => {}
            NumLoopIter::Val(v) => return NumLoopExit::Val(v),
            NumLoopIter::Nil => return NumLoopExit::Nil,
            NumLoopIter::FuelExhausted => return NumLoopExit::FuelExhausted,
            NumLoopIter::Overflow => return NumLoopExit::Overflow,
        }
    }
}

/// One outcome of [`run_num_loop_one_iter`]: it rebound the loop's own
/// bindings and should be called again, or the loop is over (a value, or
/// fuel ran out).
enum NumLoopIter {
    Recurred,
    Val(Num),
    /// W-NUMLOOP: the iteration selected a `NumBranch::RetNil`.
    Nil,
    FuelExhausted,
    Overflow,
}

/// Exactly ONE trip through the tagged register machine: the fuel check,
/// the test, and whichever branch it selects. Pure extraction from
/// `run_num_loop`'s own `loop {}` body (W1, LATENCY-CAMPAIGN.md) -- same
/// code, same order, same values -- so that `run_num_loop_lane_aware` can
/// run the SAME "one tagged iteration" the design calls the tag
/// OBSERVATION step without a second copy of this logic to keep in sync.
/// `n` is `nl.seeds.len()`, threaded in rather than recomputed so a caller
/// looping this (`run_num_loop`) pays for it once, as before.
#[inline(always)]
fn run_num_loop_one_iter(nl: &NumLoop, regs: &mut [Num; NUM_REGS], fuel: &mut Option<u64>, n: usize, intr: &crate::interrupt::Interrupt) -> NumLoopIter {
    if let Some(remaining) = fuel {
        if *remaining & INTR_MASK == 0 && (*remaining == 0 || intr.pending()) {
            return NumLoopIter::FuelExhausted;
        }
        *remaining -= 1;
    }
    if run_num_ops(&nl.test.ops, regs).is_err() {
        return NumLoopIter::Overflow;
    }
    let x = reg(nl.test.a, regs);
    let y = reg(nl.test.b, regs);
    // Spelled out per comparison, with no shared prelude: the four
    // ordered ones go through `as_f64` (what `cmp2` does, two `Int`s
    // included) and `=` must NOT (see `ir::NumCmp`), so an arm that
    // fell through to a neighbour would be a silent semantic change.
    let f = numbers::as_f64;
    let t = match nl.test.cmp {
        NumCmp::Lt => numbers::lt(f(x), f(y)),
        NumCmp::Le => numbers::le(f(x), f(y)),
        NumCmp::Gt => numbers::gt(f(x), f(y)),
        NumCmp::Ge => numbers::ge(f(x), f(y)),
        NumCmp::Eq => numbers::num_eq(x, y),
    };
    match if t { &nl.then } else { &nl.els } {
        NumBranch::Recur { ops, next } => {
            if run_num_ops(ops, regs).is_err() {
                return NumLoopIter::Overflow;
            }
            // Simultaneous rebind: `next` may read the very registers it
            // is about to overwrite (`(recur j i)`), so every value is
            // read before any is stored. The one- and two-binding shapes
            // are spelled out because the general path's staging buffer
            // is `NUM_REGS` wide and zeroing it per iteration costs more
            // than the whole loop body does.
            match next.as_slice() {
                [a] => {
                    let x = reg(*a, regs);
                    regs[0] = x;
                }
                [a, b] => {
                    let x = reg(*a, regs);
                    let y = reg(*b, regs);
                    regs[0] = x;
                    regs[1] = y;
                }
                rest => {
                    let mut staged = [Num::I(0); NUM_MAX_BINDS];
                    for (i, a) in rest.iter().enumerate() {
                        staged[i] = reg(*a, regs);
                    }
                    regs[..n].copy_from_slice(&staged[..n]);
                }
            }
            NumLoopIter::Recurred
        }
        NumBranch::Ret { ops, out } => {
            if run_num_ops(ops, regs).is_err() {
                return NumLoopIter::Overflow;
            }
            NumLoopIter::Val(reg(*out, regs))
        }
        // W-NUMLOOP: no ops to run and no register to read -- a nil branch
        // has no expression at all (see `ir::NumBranch::RetNil`).
        NumBranch::RetNil => NumLoopIter::Nil,
    }
}

// ---------------------------------------------------------------------------
// W1 (LATENCY-CAMPAIGN.md): lane variants -- unboxed i64/f64 register files,
// selected once per loop CALL rather than checked per operation.
//
// The mechanism: run exactly ONE iteration in the tagged machine above (the
// "tag observation" step -- registers 0..seeds.len() now hold whatever the
// loop's OWN transition actually produced), then check those tags against
// `nl.lane_variants` (built at resolve time by `compile::lanes::
// build_lane_variants`, one per feasible stable tag vector). A match hands
// off to `run_lane_variant`, which runs UNBOXED `i64`/`f64` arrays with no
// per-op tag dispatch until it exits, runs out of fuel, or an `i64` op
// overflows (`checked_add`/`sub`/`mul` returning `None`) -- the one dynamic
// event a lane variant cannot absorb, since it would change a register's
// TAG mid-loop, which the variant is typed to assume never happens. That
// deopts: reconstruct the tagged register file's binding slice from the
// lane's current values and resume `run_num_loop`, which is sound by
// `NumLoop`'s I2 (total arithmetic, no observation between entry and exit,
// so "some iterations ran in the lane" is indistinguishable from "all of
// them ran tagged" to anything outside this fn).
// ---------------------------------------------------------------------------

/// Lane-aware entry point, called exactly where `run_num_loop` used to be
/// called directly. `nl.lane_variants.is_empty()` (kill switch off, or the
/// tag-flow fixpoint found no feasible world for this loop) routes straight
/// to `run_num_loop` with NO extra branch beyond the one `is_empty` check --
/// this is the MOVA_NO_LANES / lanes-off parity path the campaign doc's
/// kill bar (`bench/optimization-log.md`) demands stay a tie against
/// baseline.
fn run_num_loop_lane_aware(nl: &NumLoop, regs: &mut [Num; NUM_REGS], fuel: &mut Option<u64>, intr: &crate::interrupt::Interrupt) -> NumLoopExit {
    if nl.lane_variants.is_empty() {
        return run_num_loop(nl, regs, fuel, intr);
    }
    let n = nl.seeds.len();
    match run_num_loop_one_iter(nl, regs, fuel, n, intr) {
        NumLoopIter::Val(v) => NumLoopExit::Val(v),
        NumLoopIter::Nil => NumLoopExit::Nil,
        NumLoopIter::FuelExhausted => NumLoopExit::FuelExhausted,
        NumLoopIter::Overflow => NumLoopExit::Overflow,
        NumLoopIter::Recurred => match match_lane_variant(nl, regs) {
            Some(lv) => {
                let (mut ir, mut fr) = tagged_to_lane(regs);
                match run_lane_variant(lv, &mut ir, &mut fr, fuel, intr) {
                    LaneExit::Val(v) => NumLoopExit::Val(v),
                    LaneExit::Nil => NumLoopExit::Nil,
                    LaneExit::FuelExhausted => NumLoopExit::FuelExhausted,
                    LaneExit::Deopt => {
                        // Reconstruct the tagged binding registers from the
                        // lane's CURRENT values (the world's own `binds`
                        // tags say which array each one lives in) and
                        // resume the tagged machine for the rest of the
                        // loop -- see this section's module doc for
                        // soundness.
                        for (i, t) in lv.world.binds.iter().enumerate() {
                            regs[i] = match t {
                                lanes::Tag::I => Num::I(ir[i]),
                                lanes::Tag::F => Num::F(fr[i]),
                            };
                        }
                        run_num_loop(nl, regs, fuel, intr)
                    }
                }
            }
            // No precompiled world matches the tags this loop's OWN
            // transition actually produced -- e.g. a shape `feasible_
            // worlds` declined (both branches `Recur`). `regs` already
            // holds the correct post-iteration state, so resuming
            // `run_num_loop` here continues the loop exactly as if this
            // whole fn had never looked.
            None => run_num_loop(nl, regs, fuel, intr),
        },
    }
}

/// Finds the (at most one, by construction -- see `lanes::feasible_worlds`)
/// lane variant whose world matches `regs`'s CURRENT tags: every binding
/// register's tag, and every load register a variant's ops actually read
/// (`LaneWorld::loads` already omits any load that doesn't -- see its doc).
fn match_lane_variant<'a>(nl: &'a NumLoop, regs: &[Num; NUM_REGS]) -> Option<&'a lanes::LaneVariant> {
    nl.lane_variants.iter().find(|lv| {
        lv.world.binds.iter().enumerate().all(|(i, t)| tag_matches(*t, regs[i]))
            && lv
                .world
                .loads
                .iter()
                .all(|(r, t)| tag_matches(*t, regs[(*r as usize) & (NUM_REGS - 1)]))
    })
}

fn tag_matches(t: lanes::Tag, n: Num) -> bool {
    matches!((t, n), (lanes::Tag::I, Num::I(_)) | (lanes::Tag::F, Num::F(_)))
}

/// Converts the tagged register file to the two typed arrays a lane variant
/// reads/writes. Every slot the lane's ops ever reference is a const, a
/// load, a binding, or an earlier op's `dst` written within the SAME
/// iteration before it is read (`resolve::NumCtx` never reuses a register
/// across the loop's disjoint op lists) -- so converting EVERY slot
/// wholesale, including ones a given world's ops never touch, is always
/// safe: an untouched slot's converted value simply never gets read.
fn tagged_to_lane(regs: &[Num; NUM_REGS]) -> ([i64; NUM_REGS], [f64; NUM_REGS]) {
    let mut ir = [0i64; NUM_REGS];
    let mut fr = [0.0f64; NUM_REGS];
    for i in 0..NUM_REGS {
        match regs[i] {
            Num::I(x) => ir[i] = x,
            Num::F(f) => fr[i] = f,
        }
    }
    (ir, fr)
}

/// One lane variant's own outcome.
enum LaneExit {
    Val(Num),
    /// W-NUMLOOP: the variant took its nil-terminal branch
    /// (`lanes::LaneBranch::RetNil`); the loop's value is `Value::Nil`.
    Nil,
    FuelExhausted,
    /// An `I`-lane op's `checked_*` overflowed. The registers are left
    /// exactly as they were BEFORE the op list that overflowed ran (`run_
    /// lane_ops` returns `false` the instant one op fails, having written
    /// nothing beyond that point in ITS OWN list -- but ops before it in
    /// the SAME list already committed; that's fine, because the CALLER
    /// reconstructs the tagged registers from `binds`-tagged slots ONLY,
    /// and no `NumOp` list ever writes a binding register except through
    /// the simultaneous-rebind step at the very end of an iteration, which
    /// is never reached when an earlier op in that same rebind step
    /// deopts).
    Deopt,
}

/// Runs `lv` from `ir`/`fr`'s current values until it exits, deopts, or
/// runs out of fuel -- unboxed, no per-op tag dispatch, no allocation. The
/// two arrays are `[_; NUM_REGS]` values on the CALLER's stack (not a
/// `Vec`, not a `Box`), so this function's own locals never spill through a
/// heap pointer per op -- the death mode the campaign doc names explicitly.
#[inline(never)]
fn run_lane_variant(lv: &lanes::LaneVariant, ir: &mut [i64; NUM_REGS], fr: &mut [f64; NUM_REGS], fuel: &mut Option<u64>, intr: &crate::interrupt::Interrupt) -> LaneExit {
    // W6: a variant whose shape is in the superloop set runs with its
    // loop-carried state in Rust LOCALS instead of the register-file
    // arrays. One branch per loop CALL, not per iteration.
    if let Some(sl) = &lv.sup {
        return run_superloop(lv, sl, ir, fr, fuel, intr);
    }
    loop {
        if let Some(remaining) = fuel {
            if *remaining & INTR_MASK == 0 && (*remaining == 0 || intr.pending()) {
                return LaneExit::FuelExhausted;
            }
            *remaining -= 1;
        }
        if !run_lane_ops(&lv.test.ops, ir, fr) {
            return LaneExit::Deopt;
        }
        let x = lane_to_num(lv.test.a, ir, fr);
        let y = lane_to_num(lv.test.b, ir, fr);
        let f = numbers::as_f64;
        let t = match lv.test.cmp {
            NumCmp::Lt => numbers::lt(f(x), f(y)),
            NumCmp::Le => numbers::le(f(x), f(y)),
            NumCmp::Gt => numbers::gt(f(x), f(y)),
            NumCmp::Ge => numbers::ge(f(x), f(y)),
            NumCmp::Eq => numbers::num_eq(x, y),
        };
        match if t { &lv.then } else { &lv.els } {
            lanes::LaneBranch::Recur { ops, next } => {
                if !run_lane_ops(ops, ir, fr) {
                    return LaneExit::Deopt;
                }
                // Simultaneous rebind, staged through `Num` (cheap: at most
                // `NUM_MAX_BINDS` of them, and every value the world admits
                // is already known to land in the SAME array `binds[i]`
                // names, by construction of `feasible_worlds`'s fixed
                // point) -- mirrors `run_num_loop_one_iter`'s 1-/2-/rest
                // shape split for the same reason.
                match next.as_slice() {
                    [a] => {
                        let v = lane_to_num(*a, ir, fr);
                        write_lane_bind(0, v, ir, fr);
                    }
                    [a, b] => {
                        let x = lane_to_num(*a, ir, fr);
                        let y = lane_to_num(*b, ir, fr);
                        write_lane_bind(0, x, ir, fr);
                        write_lane_bind(1, y, ir, fr);
                    }
                    rest => {
                        let mut staged = [Num::I(0); NUM_MAX_BINDS];
                        for (i, a) in rest.iter().enumerate() {
                            staged[i] = lane_to_num(*a, ir, fr);
                        }
                        for (i, v) in staged[..rest.len()].iter().enumerate() {
                            write_lane_bind(i, *v, ir, fr);
                        }
                    }
                }
            }
            lanes::LaneBranch::Ret { ops, out } => {
                if !run_lane_ops(ops, ir, fr) {
                    return LaneExit::Deopt;
                }
                return LaneExit::Val(lane_to_num(*out, ir, fr));
            }
            // W-NUMLOOP: nothing to run, nothing to read -- see
            // `lanes::LaneBranch::RetNil`.
            lanes::LaneBranch::RetNil => return LaneExit::Nil,
        }
    }
}

#[inline(always)]
fn lane_to_num(r: lanes::LaneReg, ir: &[i64; NUM_REGS], fr: &[f64; NUM_REGS]) -> Num {
    match r {
        lanes::LaneReg::I(i) => Num::I(ir[(i as usize) & (NUM_REGS - 1)]),
        lanes::LaneReg::F(i) => Num::F(fr[(i as usize) & (NUM_REGS - 1)]),
    }
}

#[inline(always)]
fn write_lane_bind(i: usize, v: Num, ir: &mut [i64; NUM_REGS], fr: &mut [f64; NUM_REGS]) {
    match v {
        Num::I(x) => ir[i] = x,
        Num::F(f) => fr[i] = f,
    }
}

/// Runs one lane-typed op list. `false` (deopt) the instant a `checked_*`
/// `I*` op overflows; every `F*` op is total (`f64` arithmetic never
/// fails), so it can never be the op that returns `false`.
///
/// `FAddFoldFF`/`FAddFoldFI` perform the identity step (`0.0 + a`)
/// EXPLICITLY rather than skipping straight to `a + b`: see `lanes::LaneOp`
/// and `ir::NumBin`'s docs on why that step is value-observable (`-0.0`)
/// even though it never changes a TAG.
#[inline(always)]
fn run_lane_ops(ops: &[lanes::LaneOp], ir: &mut [i64; NUM_REGS], fr: &mut [f64; NUM_REGS]) -> bool {
    use lanes::LaneOp::*;
    for op in ops {
        match *op {
            IAdd { dst, a, b } => match ir[a as usize & (NUM_REGS - 1)].checked_add(ir[b as usize & (NUM_REGS - 1)]) {
                Some(v) => ir[dst as usize & (NUM_REGS - 1)] = v,
                None => return false,
            },
            ISub { dst, a, b } => match ir[a as usize & (NUM_REGS - 1)].checked_sub(ir[b as usize & (NUM_REGS - 1)]) {
                Some(v) => ir[dst as usize & (NUM_REGS - 1)] = v,
                None => return false,
            },
            IMul { dst, a, b } => match ir[a as usize & (NUM_REGS - 1)].checked_mul(ir[b as usize & (NUM_REGS - 1)]) {
                Some(v) => ir[dst as usize & (NUM_REGS - 1)] = v,
                None => return false,
            },
            FAdd { dst, a, b } => fr[dst as usize & (NUM_REGS - 1)] = fr[a as usize & (NUM_REGS - 1)] + fr[b as usize & (NUM_REGS - 1)],
            FSub { dst, a, b } => fr[dst as usize & (NUM_REGS - 1)] = fr[a as usize & (NUM_REGS - 1)] - fr[b as usize & (NUM_REGS - 1)],
            FMul { dst, a, b } => fr[dst as usize & (NUM_REGS - 1)] = fr[a as usize & (NUM_REGS - 1)] * fr[b as usize & (NUM_REGS - 1)],
            FAddIF { dst, a, b } => {
                fr[dst as usize & (NUM_REGS - 1)] = (ir[a as usize & (NUM_REGS - 1)] as f64) + fr[b as usize & (NUM_REGS - 1)]
            }
            FSubIF { dst, a, b } => {
                fr[dst as usize & (NUM_REGS - 1)] = (ir[a as usize & (NUM_REGS - 1)] as f64) - fr[b as usize & (NUM_REGS - 1)]
            }
            FMulIF { dst, a, b } => {
                fr[dst as usize & (NUM_REGS - 1)] = (ir[a as usize & (NUM_REGS - 1)] as f64) * fr[b as usize & (NUM_REGS - 1)]
            }
            FAddFI { dst, a, b } => {
                fr[dst as usize & (NUM_REGS - 1)] = fr[a as usize & (NUM_REGS - 1)] + (ir[b as usize & (NUM_REGS - 1)] as f64)
            }
            FSubFI { dst, a, b } => {
                fr[dst as usize & (NUM_REGS - 1)] = fr[a as usize & (NUM_REGS - 1)] - (ir[b as usize & (NUM_REGS - 1)] as f64)
            }
            FMulFI { dst, a, b } => {
                fr[dst as usize & (NUM_REGS - 1)] = fr[a as usize & (NUM_REGS - 1)] * (ir[b as usize & (NUM_REGS - 1)] as f64)
            }
            FAddFoldFF { dst, a, b } => {
                let corrected = 0.0 + fr[a as usize & (NUM_REGS - 1)];
                fr[dst as usize & (NUM_REGS - 1)] = corrected + fr[b as usize & (NUM_REGS - 1)];
            }
            FAddFoldFI { dst, a, b } => {
                let corrected = 0.0 + fr[a as usize & (NUM_REGS - 1)];
                fr[dst as usize & (NUM_REGS - 1)] = corrected + (ir[b as usize & (NUM_REGS - 1)] as f64);
            }
        }
    }
    true
}

// ---------------------------------------------------------------------------
// W6 (LATENCY-CAMPAIGN.md §7): the superloop runtime.
//
// `lanes::build_superloop` recognises, at RESOLVE time, the closed set of
// lane-variant shapes whose whole loop-carried state fits in one or two
// scalars. This runs them: the bindings live in Rust `i64`/`f64` LOCALS for
// the entire call, the invariants are read out of the register file ONCE at
// entry, and the register-file arrays are touched again only on the two
// exits that hand back to generic code (a `Ret` branch, or a deopt).
//
// Everything observable is unchanged from `run_lane_variant`:
//   * FUEL is checked-then-decremented at the top of every iteration, from a
//     LOCAL copy written back on every exit -- so a deopting iteration
//     charges fuel twice (once here, once when `run_num_loop` re-runs it),
//     exactly as the interpreted path does.
//   * A `checked_*` OVERFLOW leaves the bindings at their ITERATION-START
//     values (chains commit only after every chain has succeeded) and
//     returns `Deopt`, which is what `run_num_loop_lane_aware` reconstructs
//     the tagged registers from. Chain order does not matter to that: an
//     overflow anywhere in an iteration deopts the whole iteration, and the
//     tagged machine then re-runs it from the same bindings.
//   * The `-0.0` identity step of `AddFold` is preserved -- see
//     `resolve_f`'s note for the ONE algebraic simplification applied to it
//     and why it is exact.
// ---------------------------------------------------------------------------

/// One `i64`-lane step with its loop-invariant operand already read out of
/// the register file. `*X` steps name another BINDING, whose value changes
/// per iteration and so stays a local, not an immediate.
#[derive(Clone, Copy)]
enum RI {
    AddK(i64),
    SubK(i64),
    RsubK(i64),
    MulK(i64),
    AddX(u8),
    SubX(u8),
    RsubX(u8),
    MulX(u8),
}

/// One `f64`-lane step, same idea. The reversed forms are separate arms
/// rather than an operand swap so that each one is a straight jump-table
/// target with nothing on the value's dependency chain but the arithmetic.
#[derive(Clone, Copy)]
enum RF {
    AddK(f64),
    RaddK(f64),
    SubK(f64),
    RsubK(f64),
    MulK(f64),
    RmulK(f64),
    AddFoldK(f64),
    AddX(u8),
    RaddX(u8),
    SubX(u8),
    RsubX(u8),
    MulX(u8),
    RmulX(u8),
    AddFoldX(u8),
    RaddFoldX(u8),
}

#[inline(always)]
fn super_k_i(r: lanes::LaneReg, ir: &[i64; NUM_REGS]) -> i64 {
    // An `i64`-lane chain's operands are `i64`-lane by construction (every
    // `IAdd`/`ISub`/`IMul` reads the `I` file), so this only ever sees
    // `LaneReg::I`; the `F` arm is a total fallback, never taken.
    ir[(r.idx() as usize) & (NUM_REGS - 1)]
}

#[inline(always)]
fn super_k_f(r: lanes::LaneReg, ir: &[i64; NUM_REGS], fr: &[f64; NUM_REGS]) -> f64 {
    match r {
        lanes::LaneReg::I(i) => ir[(i as usize) & (NUM_REGS - 1)] as f64,
        lanes::LaneReg::F(i) => fr[(i as usize) & (NUM_REGS - 1)],
    }
}

/// Reads one `i64` chain's invariants out of the register file, once, at
/// lane entry.
fn resolve_i(steps: &[lanes::IStep], ir: &[i64; NUM_REGS]) -> ([RI; lanes::MAX_STEPS], usize) {
    use lanes::{IStep, Src};
    let mut out = [RI::AddK(0); lanes::MAX_STEPS];
    let n = steps.len().min(lanes::MAX_STEPS);
    for (o, s) in out.iter_mut().zip(steps.iter().take(n)) {
        *o = match *s {
            IStep::Add(Src::K(r)) => RI::AddK(super_k_i(r, ir)),
            IStep::Add(Src::X(b)) => RI::AddX(b),
            IStep::Sub(Src::K(r)) => RI::SubK(super_k_i(r, ir)),
            IStep::Sub(Src::X(b)) => RI::SubX(b),
            IStep::Rsub(Src::K(r)) => RI::RsubK(super_k_i(r, ir)),
            IStep::Rsub(Src::X(b)) => RI::RsubX(b),
            IStep::Mul(Src::K(r)) => RI::MulK(super_k_i(r, ir)),
            IStep::Mul(Src::X(b)) => RI::MulX(b),
        };
    }
    (out, n)
}

/// Reads one `f64` chain's invariants out of the register file, once, at
/// lane entry -- and performs the one algebraic simplification a superloop
/// is allowed to make that the interpreted machine cannot:
///
/// `AddFold`'s identity step exists because `(0.0 + t) != t` when `t` is
/// `-0.0` (see `lanes::LaneOp`). But the step is immediately followed by
/// `+ k`, and `(0.0 + t) + k == t + k` for EVERY `t` unless `k` is `-0.0`:
/// the two disagree only at `t == -0.0`, where they become `0.0 + k` versus
/// `-0.0 + k`, and those differ only for `k == -0.0`. `k` is a loop
/// invariant, known here, so the identity add is dropped exactly when it is
/// provably unobservable -- which is what takes the LCG body's carried
/// dependency chain from three `f64` ops to two. `RaddFold` (`(0.0 + k) +
/// x`) folds unconditionally: its identity step touches only the invariant.
/// The `*X` folds keep theirs -- their operand is a per-iteration binding.
fn resolve_f(steps: &[lanes::FStep], ir: &[i64; NUM_REGS], fr: &[f64; NUM_REGS]) -> ([RF; lanes::MAX_STEPS], usize) {
    use lanes::{FStep, Src};
    const NEG_ZERO: u64 = 0x8000_0000_0000_0000;
    let mut out = [RF::AddK(0.0); lanes::MAX_STEPS];
    let n = steps.len().min(lanes::MAX_STEPS);
    for (o, s) in out.iter_mut().zip(steps.iter().take(n)) {
        *o = match *s {
            FStep::Add(Src::K(r)) => RF::AddK(super_k_f(r, ir, fr)),
            FStep::Add(Src::X(b)) => RF::AddX(b),
            FStep::Radd(Src::K(r)) => RF::RaddK(super_k_f(r, ir, fr)),
            FStep::Radd(Src::X(b)) => RF::RaddX(b),
            FStep::Sub(Src::K(r)) => RF::SubK(super_k_f(r, ir, fr)),
            FStep::Sub(Src::X(b)) => RF::SubX(b),
            FStep::Rsub(Src::K(r)) => RF::RsubK(super_k_f(r, ir, fr)),
            FStep::Rsub(Src::X(b)) => RF::RsubX(b),
            FStep::Mul(Src::K(r)) => RF::MulK(super_k_f(r, ir, fr)),
            FStep::Mul(Src::X(b)) => RF::MulX(b),
            FStep::Rmul(Src::K(r)) => RF::RmulK(super_k_f(r, ir, fr)),
            FStep::Rmul(Src::X(b)) => RF::RmulX(b),
            FStep::AddFold(Src::K(r)) => {
                let k = super_k_f(r, ir, fr);
                if k.to_bits() == NEG_ZERO {
                    RF::AddFoldK(k)
                } else {
                    RF::AddK(k)
                }
            }
            FStep::AddFold(Src::X(b)) => RF::AddFoldX(b),
            FStep::RaddFold(Src::K(r)) => RF::RaddK(0.0 + super_k_f(r, ir, fr)),
            FStep::RaddFold(Src::X(b)) => RF::RaddFoldX(b),
        };
    }
    (out, n)
}

/// One `i64` step. `None` is a `checked_*` overflow -- the caller's deopt,
/// on exactly the iteration the interpreted path would have taken it.
#[inline(always)]
fn step_i(s: RI, x: i64, x0: i64, x1: i64) -> Option<i64> {
    Some(match s {
        RI::AddK(k) => x.checked_add(k)?,
        RI::SubK(k) => x.checked_sub(k)?,
        RI::RsubK(k) => k.checked_sub(x)?,
        RI::MulK(k) => x.checked_mul(k)?,
        RI::AddX(b) => x.checked_add(if b == 0 { x0 } else { x1 })?,
        RI::SubX(b) => x.checked_sub(if b == 0 { x0 } else { x1 })?,
        RI::RsubX(b) => (if b == 0 { x0 } else { x1 }).checked_sub(x)?,
        RI::MulX(b) => x.checked_mul(if b == 0 { x0 } else { x1 })?,
    })
}

/// One `f64` step. Total: `f64` arithmetic never fails, so an `f64` chain
/// can never be the reason a superloop deopts.
#[inline(always)]
fn step_f(s: RF, x: f64, x0: f64, x1: f64) -> f64 {
    match s {
        RF::AddK(k) => x + k,
        RF::RaddK(k) => k + x,
        RF::SubK(k) => x - k,
        RF::RsubK(k) => k - x,
        RF::MulK(k) => x * k,
        RF::RmulK(k) => k * x,
        RF::AddFoldK(k) => (0.0 + x) + k,
        RF::AddX(b) => x + if b == 0 { x0 } else { x1 },
        RF::RaddX(b) => (if b == 0 { x0 } else { x1 }) + x,
        RF::SubX(b) => x - if b == 0 { x0 } else { x1 },
        RF::RsubX(b) => (if b == 0 { x0 } else { x1 }) - x,
        RF::MulX(b) => x * if b == 0 { x0 } else { x1 },
        RF::RmulX(b) => (if b == 0 { x0 } else { x1 }) * x,
        RF::AddFoldX(b) => (0.0 + x) + if b == 0 { x0 } else { x1 },
        RF::RaddFoldX(b) => (0.0 + if b == 0 { x0 } else { x1 }) + x,
    }
}

/// Runs one `i64` chain over locals.
///
/// The `len` match is not decoration: `steps` is loop-invariant for the
/// whole call, so this shape lets LLVM unswitch the length out of the
/// superloop's own `loop {}` and leave a straight-line body with the step
/// records hoisted into registers. A rolled `for` over a runtime-length
/// slice cannot be unswitched and measured materially slower (see W6's
/// entry in `bench/optimization-log.md`).
#[inline(always)]
fn chain_i(steps: &[RI], seed: i64, x0: i64, x1: i64) -> Option<i64> {
    match steps.len() {
        0 => Some(seed),
        1 => step_i(steps[0], seed, x0, x1),
        2 => step_i(steps[1], step_i(steps[0], seed, x0, x1)?, x0, x1),
        _ => {
            let mut x = seed;
            for s in steps {
                x = step_i(*s, x, x0, x1)?;
            }
            Some(x)
        }
    }
}

/// Runs one `f64` chain over locals -- see [`chain_i`] on the `len` match.
#[inline(always)]
fn chain_f(steps: &[RF], seed: f64, x0: f64, x1: f64) -> f64 {
    match steps.len() {
        0 => seed,
        1 => step_f(steps[0], seed, x0, x1),
        2 => step_f(steps[1], step_f(steps[0], seed, x0, x1), x0, x1),
        _ => {
            let mut x = seed;
            for s in steps {
                x = step_f(*s, x, x0, x1);
            }
            x
        }
    }
}

/// The superloop proper, monomorphized on each binding's LANE and on
/// whether there are one or two of them. The const parameters are what make
/// the unused half of each binding's `(i64, f64)` local pair vanish: with
/// `F0 == true`, `x0i` is written by no reachable code and constant
/// propagation deletes it (and vice versa). This is the whole trick -- a
/// runtime-typed register still has to live in memory; a statically typed
/// one lives in a machine register.
#[inline(never)]
fn run_super_impl<const F0: bool, const F1: bool, const TWO: bool>(
    lv: &lanes::LaneVariant,
    sl: &lanes::SuperLoop,
    ir: &mut [i64; NUM_REGS],
    fr: &mut [f64; NUM_REGS],
    fuel: &mut Option<u64>,
    intr: &crate::interrupt::Interrupt,
) -> LaneExit {
    let (b0i, n0i) = resolve_i(&sl.c[0].i, ir);
    let (b0f, n0f) = resolve_f(&sl.c[0].f, ir, fr);
    let (b1i, n1i) = resolve_i(&sl.c[1].i, ir);
    let (b1f, n1f) = resolve_f(&sl.c[1].f, ir, fr);
    let (c0i, c0f) = (&b0i[..n0i], &b0f[..n0f]);
    let (c1i, c1f) = (&b1i[..n1i], &b1f[..n1f]);
    // The test's right operand is loop-invariant (`build_superloop` checked
    // it), so both the `f64` form the four ordered comparisons need and the
    // `Num` form `=` needs are read once, here.
    let bnum = lane_to_num(sl.test_inv, ir, fr);
    let bf = numbers::as_f64(bnum);
    let mut x0i = ir[0];
    let mut x0f = fr[0];
    let mut x1i = ir[1];
    let mut x1f = fr[1];
    let mut fuel_l = *fuel;
    // Hoisted out of the iteration by hand rather than by hope: every one
    // of these is a field read through `&SuperLoop`, i.e. exactly the kind
    // of per-iteration re-read of variant state that W5's autopsy named as
    // the interpretive frame's cost.
    let recur_then = sl.recur_then;
    let cmp = sl.cmp;
    let ret = if recur_then { &lv.els } else { &lv.then };
    macro_rules! writeback {
        () => {{
            if F0 {
                fr[0] = x0f;
            } else {
                ir[0] = x0i;
            }
            if TWO {
                if F1 {
                    fr[1] = x1f;
                } else {
                    ir[1] = x1i;
                }
            }
            *fuel = fuel_l;
        }};
    }
    loop {
        // Fuel, from a local: identical rule to `run_lane_variant`'s
        // (checked before decrement), and with `None` it is one
        // predicted-not-taken branch on a register.
        if let Some(rem) = &mut fuel_l {
            if *rem & INTR_MASK == 0 && (*rem == 0 || intr.pending()) {
                *fuel = fuel_l;
                return LaneExit::FuelExhausted;
            }
            *rem -= 1;
        }
        let af = if F0 { x0f } else { x0i as f64 };
        let t = match cmp {
            NumCmp::Lt => numbers::lt(af, bf),
            NumCmp::Le => numbers::le(af, bf),
            NumCmp::Gt => numbers::gt(af, bf),
            NumCmp::Ge => numbers::ge(af, bf),
            // `=` must NOT go through `as_f64` -- see `ir::NumCmp`.
            NumCmp::Eq => numbers::num_eq(if F0 { Num::F(x0f) } else { Num::I(x0i) }, bnum),
        };
        if t != recur_then {
            // The `Ret` branch: hand the exit expression back to the
            // interpreted machine, which runs it ONCE. Its op list may
            // itself overflow, which is a deopt with the bindings already
            // written back -- the same thing `run_lane_variant` does.
            writeback!();
            match ret {
                lanes::LaneBranch::Ret { ops, out } => {
                    if !run_lane_ops(ops, ir, fr) {
                        return LaneExit::Deopt;
                    }
                    return LaneExit::Val(lane_to_num(*out, ir, fr));
                }
                // W-NUMLOOP: the nil-terminal exit. No op list to run and
                // no register to read, so there is not even a deopt to
                // consider here -- the bindings have already been written
                // back above, exactly as for the `Ret` case.
                lanes::LaneBranch::RetNil => return LaneExit::Nil,
                // `build_superloop` only ever picks a non-`Recur` branch
                // as `ret`; a `Recur` here would mean the shape pass and
                // this runtime disagree, so decline rather than guess.
                lanes::LaneBranch::Recur { .. } => return LaneExit::Deopt,
            }
        }
        // The `Recur`: every chain reads the ITERATION-START values, and
        // nothing commits until all of them have succeeded, so a deopt
        // leaves the bindings exactly where the interpreted path would.
        let cx0 = if F0 { x0f } else { x0i as f64 };
        let cx1 = if F1 { x1f } else { x1i as f64 };
        let (nx0i, nx0f) = if F0 {
            (x0i, chain_f(c0f, x0f, cx0, cx1))
        } else {
            match chain_i(c0i, x0i, x0i, x1i) {
                Some(v) => (v, x0f),
                None => {
                    writeback!();
                    return LaneExit::Deopt;
                }
            }
        };
        let (nx1i, nx1f) = if !TWO {
            (x1i, x1f)
        } else if F1 {
            (x1i, chain_f(c1f, x1f, cx0, cx1))
        } else {
            match chain_i(c1i, x1i, x0i, x1i) {
                Some(v) => (v, x1f),
                None => {
                    writeback!();
                    return LaneExit::Deopt;
                }
            }
        };
        x0i = nx0i;
        x0f = nx0f;
        x1i = nx1i;
        x1f = nx1f;
    }
}

/// Picks the monomorphization for this shape's lanes. One branch per loop
/// CALL, never per iteration.
fn run_superloop(
    lv: &lanes::LaneVariant,
    sl: &lanes::SuperLoop,
    ir: &mut [i64; NUM_REGS],
    fr: &mut [f64; NUM_REGS],
    fuel: &mut Option<u64>,
    intr: &crate::interrupt::Interrupt,
) -> LaneExit {
    match (sl.f0, sl.f1, sl.n_binds == 2) {
        (false, _, false) => run_super_impl::<false, false, false>(lv, sl, ir, fr, fuel, intr),
        (true, _, false) => run_super_impl::<true, false, false>(lv, sl, ir, fr, fuel, intr),
        (false, false, true) => run_super_impl::<false, false, true>(lv, sl, ir, fr, fuel, intr),
        (false, true, true) => run_super_impl::<false, true, true>(lv, sl, ir, fr, fuel, intr),
        (true, false, true) => run_super_impl::<true, false, true>(lv, sl, ir, fr, fuel, intr),
        (true, true, true) => run_super_impl::<true, true, true>(lv, sl, ir, fr, fuel, intr),
    }
}

/// The compiled half of `bind_pattern`: recursively writes `value` into the
/// pattern's slots. Returns `Flow::Recur` when a `:or` default form unwound
/// with one (it is an ordinary expression, evaluated in the frame, so it can
/// contain a `recur` targeting an enclosing loop -- the tree-walker lets
/// that signal out of `bind_pattern` the same way).
pub(crate) fn exec_pattern(
    interp: &mut Interp,
    pat: &CompiledPattern,
    value: Value,
    l: &mut Locals,
) -> Result<Flow, RjError> {
    match pat {
        CompiledPattern::Slot(i) => {
            l.slots[*i as usize] = value;
            Ok(Flow::Val(Value::Nil))
        }
        CompiledPattern::Seq(steps) => exec_seq_pattern(interp, steps, value, l),
        CompiledPattern::Map(m) => exec_map_pattern(interp, m, value, l),
    }
}

/// `bind_seq_pattern`, slot-flavored: one `uncons` cursor walked by `Elem`
/// steps only -- `Rest` reads the cursor without advancing it and `As` binds
/// the ORIGINAL value, and neither ends the walk.
fn exec_seq_pattern(
    interp: &mut Interp,
    steps: &[SeqStep],
    value: Value,
    l: &mut Locals,
) -> Result<Flow, RjError> {
    let mut cur = value.clone();
    for step in steps {
        match step {
            SeqStep::Elem(p) => {
                let elem = match crate::builtins::uncons(interp, &cur)? {
                    Some((h, t)) => {
                        cur = t;
                        h
                    }
                    None => Value::Nil,
                };
                bind!(exec_pattern(interp, p, elem, l));
            }
            SeqStep::Rest(p) => {
                let rest = match crate::builtins::uncons(interp, &cur)? {
                    None => Value::Nil,
                    Some(_) => cur.clone(),
                };
                bind!(exec_pattern(interp, p, rest, l));
            }
            SeqStep::As(p) => bind!(exec_pattern(interp, p, value.clone(), l)),
        }
    }
    Ok(Flow::Val(Value::Nil))
}

/// `bind_map_pattern`, slot-flavored. The value coercion
/// (`coerce_map_pattern_source`: a seq becomes a map, which is what makes
/// `(fn [& {:keys [a b]}] ..)` kwargs work) and the per-key probe
/// (`map_pattern_lookup`, including its "absent" vs "present but nil"
/// distinction, which is what `:or` keys off) are the tree-walker's own
/// functions, called here unchanged.
fn exec_map_pattern(
    interp: &mut Interp,
    m: &MapPattern,
    value: Value,
    l: &mut Locals,
) -> Result<Flow, RjError> {
    let value = interp.coerce_map_pattern_source(&value)?;
    for MapEntry {
        key,
        target,
        default,
        required,
        span,
    } in &m.entries
    {
        let (present, found) = map_pattern_lookup(&value, key);
        let v = if present {
            found
        } else if let Some(d) = default {
            val!(exec(interp, d, l))
        } else if *required {
            // `:keys!`/`:strs!`/`:syms!` (`resolve_push_value`'s `req`
            // branch): matches the tree-walker's "Missing required key"
            // wording exactly -- `resolve.rs`'s `compile_map_pattern` never
            // lets a required entry reach here WITH a `default` also set
            // (that always-throwing combination bails to the tree-walker
            // instead), so this arm only ever fires on a genuinely absent
            // key.
            return Err(RjError::other(format!(
                "Missing required key: {}",
                crate::printer::pr_str(key)
            ))
            .with_span(*span)
            .with_stack(interp.stack_snapshot(), interp.source_id));
        } else {
            Value::Nil
        };
        bind!(exec_pattern(interp, target, v, l));
    }
    // `:as` binds the COERCED value (matching Clojure), and binds last.
    if let Some(p) = &m.as_pat {
        bind!(exec_pattern(interp, p, value.clone(), l));
    }
    Ok(Flow::Val(Value::Nil))
}

/// `Ir::MakeClosure`: snapshot the captures out of the running frame and
/// wrap the shared template in a fresh `Closure`.
///
/// The new closure's `env` is the ENCLOSING closure's creation env, not a
/// frame of its own -- the compiled tier has no live env chain, and it does
/// not need one: every binding between the enclosing fn's entry and this
/// node is a slot, and `caps` has just snapshotted the ones this fn refers
/// to (by value, which `ir::CaptureSrc` explains is exact). What remains for
/// `Ir::CreationEnvLookup` to probe is precisely the enclosing creation env
/// chain, which is what it gets. `arities` is shared (an `Arc`), so a
/// closure created in a hot loop costs one `Arc` bump rather than a deep
/// clone of its body forms -- and it is fully populated, so arity selection
/// and arity-error messages come from the same place in both tiers.
#[inline(never)] // K1: keep exec()'s frame small, no TLS in its prologue
pub(crate) fn make_closure(
    interp: &Interp,
    template: &Arc<FnTemplate>,
    caps: &[CaptureSrc],
    l: &Locals,
) -> Result<Value, RjError> {
    let captures = snapshot_caps(interp, caps, l)?;
    #[cfg(feature = "leak-probe")]
    crate::value::CLOSURE_CREATED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Ok(Value::Fn(Arc::new(Closure {
        name: template.code.name.clone(),
        arities: template.arities.clone(),
        env: l.me.env.clone(),
        // The nested fn was written in the same namespace as the enclosing
        // one (its template was compiled as part of it), so it inherits
        // that namespace rather than reading whatever is current at the
        // moment this node runs.
        ns: l.me.ns.clone(),
        // field4/W-LENS-1: a closure built by `Ir::MakeClosure` is compiled
        // BY CONSTRUCTION -- its template was compiled as part of the
        // enclosing fn, and a nested fn can never fall back on its own (see
        // this module's doc). So it never tree-walks and has no bail to
        // regret; any escape it does carry counts through that escape node's
        // own `lens_site`, not through the closure.
        compiled: crate::value::CompileSlot::settled(
            Some(CompiledClosure {
                code: template.code.clone(),
                captures,
                // An ordinary nested fn is not a member of any recursive
                // binding group -- only `RecGroup::materialize` ever fills
                // this.
                group: None,
            }),
            crate::lens::NO_SITE,
        ),
        // W4C: this `fn` literal is being evaluated (i.e. created) RIGHT
        // NOW, at the moment `Ir::MakeClosure` runs -- the same
        // "definition-time" read `eval_fn_form` does, just reached from
        // the compiled tier's own creation site instead of the
        // tree-walker's. See `Closure::unchecked_math`'s doc.
        unchecked_math: interp.unchecked_math_active(),
        // Never consulted: `compiled` is already settled, so `on_call`
        // never reaches this fn's `def_span`.
        def_span: crate::reader::Span { start: 0, end: 0 },
        def_source_id: crate::source_registry::SrcRef::NONE,
        native_macro: None,
    })))
}

/// One closure's capture snapshot, read out of the running frame. Shared by
/// `Ir::MakeClosure` and by every member of an `Ir::MakeRecGroup`, so the
/// two node kinds cannot drift about what a `CaptureSrc` means.
fn snapshot_caps(
    interp: &Interp,
    caps: &[CaptureSrc],
    l: &Locals,
) -> Result<Vec<Value>, RjError> {
    let mut out = Vec::with_capacity(caps.len());
    for c in caps {
        out.push(match c {
            CaptureSrc::Slot(i) => l.slots[*i as usize].clone(),
            CaptureSrc::Capture(i) => l.caps[*i as usize].clone(),
            CaptureSrc::SelfRef => Value::Fn(l.me.clone()),
            // The one capture kind that is not a plain read: member `i` of
            // the group the CREATING closure belongs to (see
            // `CaptureSrc::Sibling`). `resolve.rs` emits it only inside a
            // group member's body, so `l.me` is a member here.
            CaptureSrc::Sibling(i) => Value::Fn(sibling(interp, *i, l)?),
        });
    }
    Ok(out)
}

/// Member `i` of the running closure's recursive binding group -- the whole
/// of `Ir::SiblingRef`, and the odd arm of [`snapshot_caps`].
///
/// A missing group is a COMPILER bug, not a user error: `resolve.rs` emits
/// neither node outside a group member. It is reported as an internal error
/// rather than a panic, on this module's standing "a future caller mistake
/// must fail safe" rule.
#[inline(never)] // K1: keep exec()'s frame small, no TLS in its prologue
fn sibling(interp: &Interp, i: u16, l: &Locals) -> Result<Arc<Closure>, RjError> {
    let group = l
        .me
        .compiled
        .compiled()
        .and_then(|c| c.group.as_ref())
        .ok_or_else(|| internal_sibling_error(interp, "outside a recursive binding group"))?;
    group
        .materialize(i as usize)
        .ok_or_else(|| internal_sibling_error(interp, "with an out-of-range member index"))
}

fn internal_sibling_error(interp: &Interp, what: &str) -> RjError {
    RjError::type_err(format!(
        "internal: compiled sibling reference evaluated {what}"
    ))
    .with_stack(interp.stack_snapshot(), interp.source_id)
}

/// `Ir::MakeRecGroup`: build one recursive binding group, materialize every
/// member, and write each into its slot.
///
/// Order mirrors `MakeClosure`'s: every member's OUTER captures are
/// snapshotted out of the running frame FIRST (none of them can name a
/// member of this run -- see `Ir::SiblingRef`), and only then is the group
/// built. The frame slots become the members' strong owners; the group holds
/// them weakly, which is the whole point (`super::RecGroup`).
///
/// The node's value is member 0's closure, which the enclosing `Ir::Let`
/// binding pair writes into `slots[0]` a second time -- see
/// `Ir::MakeRecGroup`'s doc for why that redundancy is the cheap option.
#[inline(never)] // K1: keep exec()'s frame small, no TLS in its prologue
fn exec_make_rec_group(
    interp: &mut Interp,
    members: &[super::ir::RecMember],
    slots: &[u16],
    l: &mut Locals,
) -> Result<Flow, RjError> {
    let mut captures = Vec::with_capacity(members.len());
    for m in members {
        captures.push(snapshot_caps(interp, &m.caps, l)?);
    }
    let (_group, built) = super::RecGroup::build(
        members.iter().map(|m| m.template.clone()).collect(),
        captures,
        l.me.env.clone(),
        l.me.ns.clone(),
        // W4C: read ONCE for the whole run, at the moment the run is
        // created -- see `RecGroup::unchecked_math` for the one deviation
        // that implies and why it is unobservable.
        interp.unchecked_math_active(),
    );
    let mut first = Value::Nil;
    for (i, slot) in slots.iter().enumerate() {
        let v = built.get(i).cloned().unwrap_or(Value::Nil);
        if i == 0 {
            first = v.clone();
        }
        l.slots[*slot as usize] = v;
    }
    Ok(Flow::Val(first))
}

/// `try`, mirroring `eval_try` decision for decision:
///
/// - a `recur` unwinding out of the body is NOT caught (in either tier: the
///   tree-walker special-cases `ErrorKind::Recur` before its catch clause,
///   and here it is a `Flow`, not an error at all) -- but `finally` still
///   runs before it continues on its way;
/// - C3g: `catches` are tried IN ORDER and the first whose `class` matches
///   (`catch_class_matches` -- `None` matches unconditionally, same as the
///   pre-C3g single untyped catch) wins; a throw matching no clause
///   propagates, exactly like `eval_try`'s identical dispatch;
/// - a caught error binds the THROWN value for `throw`, and
///   `error_to_info_map`'s `{:type :error/... :message "..."}` for every
///   other error kind (the tree-walker's own function);
/// - `finally` always runs, on every path, and if it fails or itself
///   `recur`s, THAT outcome wins and the body's/catch's is discarded --
///   which is exactly what `eval_try`'s `?` on its finally body does.
/// C1: `binding`/`with-redefs` -- the tree-walker's exact sequence (see
/// `ir::DynBind`); the `leave_*` runs whatever the body returned, `Flow::Recur` included.
#[inline(never)] // K1: keep exec()'s frame small, no TLS in its prologue
fn exec_dyn_bind(interp: &mut Interp, d: &DynBind, l: &mut Locals) -> Result<Flow, RjError> {
    let mut resolved = Vec::with_capacity(d.pairs.len());
    for (sym, sym_span, init) in &d.pairs {
        let cell = interp.resolve_dyn_var_cell(sym, *sym_span, !d.redefs)?;
        let v = val!(exec(interp, init, l));
        resolved.push((cell, v));
    }
    if d.redefs {
        let saved = interp.enter_with_redefs(&resolved, d.span)?;
        let result = exec_body(interp, &d.body, l);
        Interp::leave_with_redefs(&saved);
        result
    } else {
        let frame = interp.enter_binding(&resolved, d.span)?;
        let result = exec_body(interp, &d.body, l);
        interp.leave_binding(&resolved, frame);
        result
    }
}

#[inline(never)] // K1: keep exec()'s frame small, no TLS in its prologue
fn exec_try(
    interp: &mut Interp,
    body: &[Ir],
    catches: &[CatchArm],
    finally: Option<&[Ir]>,
    l: &mut Locals,
) -> Result<Flow, RjError> {
    let result = match exec_body(interp, body, l) {
        Ok(f) => Ok(f),
        // A `recur` signal raised by a TREE-WALKED callee (an uncompiled fn
        // called from here) arrives as an error; it is not ours to catch.
        Err(e) if e.kind == ErrorKind::Recur => Err(e),
        // Fuel exhaustion is not catchable by script-level `try`, in either
        // tier -- see `eval::special_forms::eval_try`'s identical exclusion
        // for the reasoning.
        Err(e) if e.kind == ErrorKind::FuelExhausted || e.kind == ErrorKind::InterruptedHard => Err(e),
        Err(e) => {
            match catches
                .iter()
                .find(|arm| arm.class.as_ref().is_none_or(|c| catch_class_matches(c, &e)))
            {
                Some(arm) => {
                    let thrown = if e.kind == ErrorKind::Thrown {
                        e.thrown.clone().unwrap_or(Value::Nil)
                    } else {
                        error_to_info_map(&e)
                    };
                    l.slots[arm.slot as usize] = thrown;
                    exec_body(interp, &arm.body, l)
                }
                None => Err(e),
            }
        }
    };
    if let Some(fin) = finally {
        match exec_body(interp, fin, l)? {
            Flow::Val(_) => {}
            Flow::Recur => return Ok(Flow::Recur),
        }
    }
    result
}

/// `def` in a fn body. Writing through the pre-interned cell is what
/// `Env::set` does for the root frame (`intern` would hand back this very
/// cell), minus the root map's lock and hash -- including clearing
/// `pristine_builtin`, so `(def + str)` inside a fn disarms every compiled
/// `+` intrinsic exactly like one at top level.
///
/// W-DECL: a bare `(def name)` -- no init form at all -- is NOT `(def
/// name nil)`. `cell` is already interned (`compile_def` calls
/// `self.globals.intern` at compile time, unbound if new), so the `None`
/// arm does nothing further: no root write, leaving a fresh cell
/// genuinely unbound and an already-bound one untouched -- mirrors
/// `eval_def`'s identical fix (special_forms.rs). Before this fix
/// `exec_def` unconditionally `store`d (defaulting the missing init to
/// `Value::Nil`), which was this exact bug reaching the compiled tier:
/// caught by `tests/differential_test.rs`'s `def_in_a_fn_body`
/// (`(defn f [] (def d)) (f) [d (f)]` used to read `[nil nil]` here while
/// the tree-walker already (correctly) errored "Unable to resolve
/// symbol: d" -- a real tier divergence, not just a stale test).
#[inline(never)] // K1: keep exec()'s frame small, no TLS in its prologue
fn exec_def(
    interp: &mut Interp,
    cell: &Arc<VarCell>,
    value: Option<&Ir>,
    l: &mut Locals,
) -> Result<Flow, RjError> {
    match value {
        Some(ir) => {
            let v = val!(exec(interp, ir, l));
            cell.store(v, false);
            Ok(Flow::Val(Value::Var(cell.clone())))
        }
        None => Ok(Flow::Val(Value::Var(cell.clone()))),
    }
}

#[inline(never)] // K1: keep exec()'s frame small, no TLS in its prologue
fn exec_recur(
    interp: &mut Interp,
    args: &[Ir],
    scratch_base: u16,
    l: &mut Locals,
) -> Result<Flow, RjError> {
    let base = scratch_base as usize;
    for (i, a) in args.iter().enumerate() {
        let v = val!(exec(interp, a, l));
        l.slots[base + i] = v;
    }
    Ok(Flow::Recur)
}

#[inline(never)] // K1: keep exec()'s frame small, no TLS in its prologue
fn exec_call(
    interp: &mut Interp,
    callee: &Ir,
    args: &[Ir],
    span: Span,
    l: &mut Locals,
) -> Result<Flow, RjError> {
    let f = val!(exec(interp, callee, l));
    let argv = match exec_args(interp, args, l)? {
        Some(v) => v,
        None => return Ok(Flow::Recur),
    };
    // `argv` is dead after this call -- see `finish_call`'s note.
    interp.apply_value_owned(&f, argv, span).map(Flow::Val)
}

#[allow(clippy::too_many_arguments)] // one span each for the head and the call
#[inline(never)] // K1: keep exec()'s frame small, no TLS in its prologue
fn exec_call_global(
    interp: &mut Interp,
    chain: &GlobalChain,
    sym: &Symbol,
    sym_span: Span,
    args: &[Ir],
    span: Span,
    l: &mut Locals,
) -> Result<Flow, RjError> {
    // Head first, then args -- `eval_list`'s order, so an unbound callee
    // errors before any argument runs, and points at the head symbol
    // itself rather than at the whole call form.
    // MT2: `chain.read()` borrows the root `Value` (arc-swap `Guard`,
    // no clone, no refcount bump) and keeps it alive for the whole call
    // (`finish_call` runs arg eval + apply while still borrowing) --
    // safe because the `Guard` keeps the old value alive even if some
    // reentrant arg evaluation `def`s a new one into this same cell.
    match chain.read() {
        Some(r) => finish_call(interp, r.value(), sym, args, span, l),
        None => Err(unresolved(interp, sym, sym_span)),
    }
}

#[allow(clippy::too_many_arguments)] // one span each for the head and the call
#[inline(never)] // K1: keep exec()'s frame small, no TLS in its prologue
fn exec_call_creation_env(
    interp: &mut Interp,
    sym: &Symbol,
    chain: &GlobalChain,
    sym_span: Span,
    args: &[Ir],
    span: Span,
    l: &mut Locals,
) -> Result<Flow, RjError> {
    let f = match creation_env_get(l, sym, chain) {
        Some(v) => v,
        None => return Err(unresolved(interp, sym, sym_span)),
    };
    finish_call(interp, &f, sym, args, span, l)
}

/// The shared tail of `CallGlobal`/`CallCreationEnv`: the head value is in
/// hand, so check it isn't a late-defined macro, evaluate the args, apply.
/// `#[inline]`: `Value` is a wide enum, and this is on the hot path of
/// every call a compiled fn makes.
#[inline]
fn finish_call(
    interp: &mut Interp,
    f: &Value,
    sym: &Symbol,
    args: &[Ir],
    span: Span,
    l: &mut Locals,
) -> Result<Flow, RjError> {
    if let Value::Macro(_) = f {
        return Err(late_macro(interp, sym, span));
    }
    let argv = match exec_args(interp, args, l)? {
        Some(v) => v,
        None => return Ok(Flow::Recur),
    };
    // `argv` is dead after this call, so it is handed over by value: the
    // consuming seam (`builtins::reuse`) lives in `apply_value_owned`, which
    // both tiers reach from here and from `eval::eval_list` -- one seam,
    // both tiers, no per-tier duplication of the whitelist.
    interp.apply_value_owned(f, argv, span).map(Flow::Val)
}

/// `Ir::CreationEnvLookup`'s resolution step: the closure's own creation env
/// chain, probed exactly as `Interp::resolve_symbol` would -- the non-root
/// frames first (`get_local`), then the global candidate chain interned at
/// compile time.
///
/// The frames must be re-probed every time: they are live, and one of them
/// may have gained or rebound this name since the closure was created.
#[inline(never)] // K1: keep exec()'s frame small, no TLS in its prologue
fn creation_env_get(l: &Locals, sym: &Symbol, chain: &GlobalChain) -> Option<Value> {
    if let Some(v) = l.me.env.get_local(sym) {
        return Some(v);
    }
    chain.get()
}

/// The intrinsic guard (COMPILE-TIER-DESIGN.md, S3). One atomic load
/// decides between "run the builtin's own Rust helper on the evaluated
/// arguments" and "this name has been redefined, so do the ordinary call".
///
/// The load happens BEFORE the arguments are evaluated, which is what makes
/// it exact: `eval_list` reads the head symbol's value before evaluating a
/// single argument, so an argument that redefines `+` mid-call does not
/// affect that call in either tier.
///
/// Argument evaluation still completes for ALL arguments before any of them
/// is inspected, because that is when a native would first see them -- the
/// fold shapes below never short-circuit on a type error before evaluating
/// a later argument's side effects.
#[allow(clippy::too_many_arguments)] // one span each for the head and the call
#[inline(never)] // K1: keep exec()'s frame small, no TLS in its prologue
fn exec_intrinsic(
    interp: &mut Interp,
    op: IntrinOp,
    chain: &GlobalChain,
    sym: &Symbol,
    sym_span: Span,
    args: &[Ir],
    span: Span,
    l: &mut Locals,
) -> Result<Flow, RjError> {
    // Two guards, both lock-free: the builtin must still be the untouched
    // native, AND nothing defined since must shadow it in this namespace
    // (`crate::ns`). Either one failing means "do the ordinary call".
    if !chain.intrinsic_armed() {
        return exec_call_global(interp, chain, sym, sym_span, args, span, l);
    }
    let out = match op {
        // n-ary folds: the identity element and the per-step function are
        // the native's own (`builtins::numbers`), so `(+ a b c)` is one
        // node and still folds exactly as `+` does.
        IntrinOp::Add | IntrinOp::Mul => {
            let (init, step): (Value, numbers::NumStep) = match op {
                IntrinOp::Add => (Value::Int(0), numbers::add_step),
                _ => (Value::Int(1), numbers::mul_step),
            };
            // The 2-argument shape (overwhelmingly the common one) folds
            // straight off the stack: no argument Vec, hence no malloc per
            // evaluation of `(+ a b)`.
            if args.len() == 2 {
                let a = val!(exec(interp, &args[0], l));
                let b = val!(exec(interp, &args[1], l));
                // S5: a TOWER first operand skips the identity step, for
                // the same measured reason the native does -- see
                // `numbers::fold_nary`. `skips_identity` is false for
                // every `Int`/`Float`, so the hot 2-argument shape adds
                // one predictable, already-warm variant test and no
                // allocation.
                if numbers::skips_identity(&a) {
                    step(interp, &a, &b)
                } else {
                    step(interp, &init, &a).and_then(|acc| step(interp, &acc, &b))
                }
            } else {
                let argv = match exec_args(interp, args, l)? {
                    Some(v) => v,
                    None => return Ok(Flow::Recur),
                };
                let folded = numbers::fold_nary(interp, &argv, init, step);
                interp.put_buf(argv);
                folded
            }
        }
        IntrinOp::Inc | IntrinOp::Dec | IntrinOp::Zero | IntrinOp::Not => {
            let a = val!(exec(interp, &args[0], l));
            match op {
                IntrinOp::Inc => numbers::inc1(interp, &a),
                IntrinOp::Dec => numbers::dec1(interp, &a),
                IntrinOp::Zero => predicates::zero1(&a),
                _ => Ok(predicates::not1(&a)),
            }
        }
        // The 2-argument ops, listed rather than caught by `_` so that
        // adding an `IntrinOp` fails to compile until it is classified.
        IntrinOp::Sub2
        | IntrinOp::Div2
        | IntrinOp::Lt2
        | IntrinOp::Le2
        | IntrinOp::Gt2
        | IntrinOp::Ge2
        | IntrinOp::Eq2 => {
            let a = val!(exec(interp, &args[0], l));
            let b = val!(exec(interp, &args[1], l));
            match op {
                IntrinOp::Sub2 => numbers::sub2(interp, &a, &b),
                IntrinOp::Div2 => numbers::div2(&a, &b),
                IntrinOp::Lt2 => numbers::lt2(&a, &b),
                IntrinOp::Le2 => numbers::le2(&a, &b),
                IntrinOp::Gt2 => numbers::gt2(&a, &b),
                IntrinOp::Ge2 => numbers::ge2(&a, &b),
                // `=` is the one intrinsic that needs the interpreter: it
                // forces lazy operands, so it must be the very same
                // `values_equal` the `=` native calls.
                _ => interp.values_equal(&a, &b).map(Value::Bool),
            }
        }
    };
    out.map(Flow::Val)
        .map_err(|e| native_err(interp, e, span))
}

/// A native's error gets its call-site span and the current stack filled in
/// by `apply_value`; an intrinsic bypasses `apply_value`, so it does the
/// same here (and in the same order) rather than emitting a span-less error.
pub(crate) fn native_err(interp: &Interp, mut e: RjError, span: Span) -> RjError {
    if e.span.is_none() {
        e.span = Some(span);
    }
    if e.stack.is_empty() {
        e.stack = interp.stack_snapshot();
    }
    e
}

#[inline(never)] // K1: keep exec()'s frame small, no TLS in its prologue
fn exec_vector(interp: &mut Interp, items: &[Ir], l: &mut Locals) -> Result<Flow, RjError> {
    // Build a flat staging buffer first, convert to `PVec` in one shot at
    // the end (sizing the single `Arc<[Value]>` allocation exactly once for
    // the common <=16-element case) -- rather than `PVec::push_back`-ing
    // `items.len()` times, which would reallocate-and-copy the growing
    // array on every single push while still `Small` (O(n^2) allocator
    // churn for a hot literal-vector path like `[a b c ...]` evaluated in a
    // loop).
    //
    // W4 diet: the staging buffer is POOLED (`take_buf`/`put_buf`) and the
    // conversion clones out of it (`PVec::from_slice` -- per-element
    // refcount bumps) instead of consuming a fresh `Vec` per literal: only
    // the `Arc<[Value]>` backing actually escapes into the result, so only
    // it is allocated. The census priced the old per-literal staging `Vec`
    // at ~3 allocs/msg on flow-gen-sink (bench/RESULTS-w4-alloc-attrib.md).
    let mut out = interp.take_buf();
    out.reserve(items.len());
    for it in items {
        match exec(interp, it, l)? {
            Flow::Val(v) => out.push(v),
            Flow::Recur => {
                interp.put_buf(out);
                return Ok(Flow::Recur);
            }
        }
    }
    let pv = PVec::from_slice(&out);
    interp.put_buf(out);
    Ok(Flow::Val(Value::Vector(pv)))
}

#[inline(never)] // K1: keep exec()'s frame small, no TLS in its prologue
fn exec_set(interp: &mut Interp, items: &[Ir], l: &mut Locals) -> Result<Flow, RjError> {
    let mut out = champ::PersistentHashSet::new().transient();
    for it in items {
        out.insert(val!(exec(interp, it, l)));
    }
    Ok(Flow::Val(Value::Set(out.persistent())))
}

#[inline(never)] // K1: keep exec()'s frame small, no TLS in its prologue
fn exec_map(interp: &mut Interp, pairs: &[(Ir, Ir)], l: &mut Locals) -> Result<Flow, RjError> {
    // W4 diet: `{}` (sink transforms return `[state {}]` on every message
    // that emits nothing) is one shared persistent empty map, cloned per
    // literal (a refcount bump) instead of a fresh `Arc`-boxed empty `Vec`
    // per evaluation. Persistent semantics make the sharing unobservable:
    // any growing op on a non-unique handle copies (builtins::reuse's
    // invariant), paying exactly what the old fresh-allocation path paid.
    if pairs.is_empty() {
        static EMPTY: std::sync::OnceLock<PMap> = std::sync::OnceLock::new();
        map_probe::record("map-literal", 0);
        return Ok(Flow::Val(Value::Map(EMPTY.get_or_init(PMap::new).clone())));
    }
    let mut out = PMap::new();
    for (k, v) in pairs {
        let kv = val!(exec(interp, k, l));
        let vv = val!(exec(interp, v, l));
        out.insert(kv, vv);
    }
    map_probe::record("map-literal", out.len());
    Ok(Flow::Val(Value::Map(out)))
}

#[inline(never)] // K1: keep exec()'s frame small, no TLS in its prologue
fn exec_throw(
    interp: &mut Interp,
    value: &Ir,
    span: Span,
    l: &mut Locals,
) -> Result<Flow, RjError> {
    let v = val!(exec(interp, value, l));
    Err(RjError::thrown(v)
        .with_span(span)
        .with_stack(interp.stack_snapshot(), interp.source_id))
}

/// Evaluates a call's arguments; `None` means a `recur` unwound through one
/// of them, so the call itself must be abandoned.
fn exec_args(
    interp: &mut Interp,
    args: &[Ir],
    l: &mut Locals,
) -> Result<Option<Vec<Value>>, RjError> {
    // W4 diet: the args buffer comes from (and, via `apply_value_owned`'s
    // terminal arms, returns to) the interpreter's buffer pool -- this was
    // the census's single largest line item (~5 allocs/msg on
    // flow-gen-sink, bench/RESULTS-w4-alloc-attrib.md). An error path
    // drops the buffer instead of pooling it; that is a missed reuse, not
    // a leak.
    let mut out = interp.take_buf();
    out.reserve(args.len());
    for a in args {
        match exec(interp, a, l)? {
            Flow::Val(v) => out.push(v),
            Flow::Recur => {
                interp.put_buf(out);
                return Ok(None);
            }
        }
    }
    Ok(Some(out))
}

/// Byte-identical to `eval_form_in`'s unresolved-symbol error (message,
/// span, label and stack), so error output can't drift between tiers.
pub(crate) fn unresolved(interp: &Interp, sym: &Symbol, span: Span) -> RjError {
    RjError::unresolved(format!(
        "Unable to resolve symbol: {}",
        crate::printer::pr_str(&Value::Sym(sym.clone()))
    ))
    .with_span(span)
    .with_label("undefined here")
    .with_stack(interp.stack_snapshot(), interp.source_id)
}

/// The compiled tier's one documented deviation, surfaced explicitly: this
/// call site compiled as a function call because the symbol was not a macro
/// when the enclosing fn was defined, and it is one now.
pub(crate) fn late_macro(interp: &Interp, sym: &Symbol, span: Span) -> RjError {
    RjError::type_err(format!(
        "{} is a macro, but the compiled fn calling it was defined before that macro existed \
         (macro expansion is frozen at fn-definition time; define the macro first)",
        crate::printer::pr_str(&Value::Sym(sym.clone()))
    ))
    .with_span(span)
    .with_stack(interp.stack_snapshot(), interp.source_id)
}
