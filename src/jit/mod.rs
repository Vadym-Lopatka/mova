//! E1a spike (docs/JIT.md): Cranelift-codegen a small scalar subset of
//! compiled-tier fns to native code, opt-in via `MOVA_JIT=1`. With the env
//! var unset (the default), every call here is a no-op and behaviour is
//! byte-identical to a build without this module. See docs/JIT.md for the
//! ABI, the lowered subset, and the bail rule.

#[cfg(feature = "jit")]
mod aot;
#[cfg(feature = "jit")]
mod calls;
#[cfg(feature = "jit")]
mod lower;
#[cfg(feature = "jit")]
mod threaded;
pub mod fast;
pub mod layout;

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use crate::compile::exec::{Flow, Locals};
use crate::compile::ir::Ir;
use crate::compile::CompiledFn;
use crate::error::RjError;
use crate::eval::Interp;
use crate::value::Value;

/// E1b (docs/JIT.md): bumped at every root-value-changing write to any
/// `VarCell` (`VarCell::store` -- def/set!/with-redefs/alter-var-root/boot)
/// and the first time any cell ever becomes dynamically bound
/// (`VarCell::push_binding`). A direct-call inline cache
/// ([`CallIc`]) is valid only while its own cached epoch still matches this
/// counter, so any such write invalidates every outstanding cache at once.
/// Plain `AtomicU64`/`Relaxed` reads from JIT-generated code (not a fenced
/// atomic load): a torn read can only cause a spurious miss (safe, just
/// slow), never a wrong hit, because the miss path re-resolves through the
/// same `GlobalChain::get()` the interpreter uses.
pub static DEF_EPOCH: AtomicU64 = AtomicU64::new(1);

/// K3: bumped on every protocol-registry write; guards the JIT protocol-dispatch IC.
pub static PROTO_EPOCH: AtomicU64 = AtomicU64::new(1);

#[inline]
pub fn bump_def_epoch() {
    DEF_EPOCH.fetch_add(1, Ordering::Release);
}

/// One per `CallGlobal` JIT call site, leaked (`'static`) exactly like the
/// generated native code itself -- see [`lower`]'s module doc. `entry == 0`
/// means "empty/never filled". A hit compares `epoch` against [`DEF_EPOCH`]
/// and, on match, calls `entry` directly; a miss calls [`jit_call_miss`].
#[repr(C)]
pub struct CallIc {
    pub epoch: AtomicU64,
    pub entry: AtomicUsize,
}

impl CallIc {
    pub(super) fn leak() -> &'static CallIc {
        Box::leak(Box::new(CallIc { epoch: AtomicU64::new(0), entry: AtomicUsize::new(0) }))
    }
}

/// `MOVA_JIT=1`, read once (same shape as `compile::disabled_by_env`).
pub fn enabled() -> bool {
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| {
        cfg!(feature = "jit") && std::env::var("MOVA_JIT").is_ok_and(|v| v == "1")
    })
}

/// E3a (docs/NATIVE-TIER-DESIGN.md): the generic (any-`Value`) tier, on top
/// of `enabled()`. `MOVA_JIT_GENERIC=0` limits the JIT to E1's int-only
/// subset even when `MOVA_JIT=1` -- read once, like `enabled()`.
/// G1: default OFF now that the threaded tier exists -- `MOVA_JIT_GENERIC=1`
/// opts back in. The generic tier's code is unchanged, just no longer tried
/// by default.
pub fn generic_enabled() -> bool {
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| {
        enabled() && std::env::var("MOVA_JIT_GENERIC").is_ok_and(|v| v == "1")
    })
}

/// F1: `MOVA_JIT_EXPLAIN=1` -- one stderr line per (fn, arity) the generic
/// tier is asked to lower, saying whether it lowered or the first `Ir` node
/// that stopped it (see `lower::explain_generic`). Off the hot path: read
/// once, checked only from `get_or_lower_generic`'s cold (`OnceLock::get_or_init`)
/// path, same shape as `enabled()`.
pub fn explain_enabled() -> bool {
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| std::env::var("MOVA_JIT_EXPLAIN").is_ok_and(|v| v == "1"))
}

/// Passed to every native entry point: the one piece of mutable state the
/// generated code touches. `depth` is a remaining-recursion-calls budget
/// (min of the interpreter's own remaining call depth and a 10_000 hard
/// cap), decremented around a self-call and restored after it returns, so
/// sibling recursive calls see the same budget a real call stack would give
/// them. Reaching zero bails instead of recursing further natively.
#[repr(C)]
pub struct JitCtx {
    pub depth: i64,
    /// E3a: the running `Interp`, for generic-tier helpers that must call
    /// back into it (`apply_value_owned`, `numbers::*`, frame/error
    /// plumbing). Null for an E1 int-only entry, which never reads it.
    /// `depth` MUST stay the first field: E1's existing Cranelift codegen
    /// loads/stores it at offset 0.
    pub interp: *mut Interp,
    /// `interp.stack.len()` at the OUTERMOST native entry for this call
    /// chain (docs/NATIVE-TIER-DESIGN.md #4) -- where a failing native fn
    /// inserts its own `Frame`, innermost-last.
    pub base: usize,
    /// Set by a helper on failure (status 2); taken by the entry hook.
    pub pending: Option<Box<RjError>>,
    /// F1: `Value::Fn(rc.clone())` for the closure currently running --
    /// BORROWED from a local the caller (`apply_closure_buf`) keeps alive
    /// for the whole call, exactly like `args`. Backs `Ir::SelfRef` (a
    /// named fn's body referring to its own name), which must be the
    /// ACTUAL running closure, not a fresh global lookup -- those can
    /// differ under redefinition-during-recursion. Null for an E1 entry.
    pub self_val: *const Value,
}

/// One arity's native entry, by parameter count (0-4, no variadic --
/// see docs/JIT.md). `extern "C" fn(ctx, a0..an, out) -> status`: `status
/// == 0` means `*out` holds the `i64` result, `1` means BAIL (the subset
/// has no side effects, so the caller just re-runs the interpreter with
/// the same, untouched arguments).
#[derive(Clone, Copy)]
pub enum NativeEntry {
    A0(extern "C" fn(*mut JitCtx, *mut i64) -> u32),
    A1(extern "C" fn(*mut JitCtx, i64, *mut i64) -> u32),
    A2(extern "C" fn(*mut JitCtx, i64, i64, *mut i64) -> u32),
    A3(extern "C" fn(*mut JitCtx, i64, i64, i64, *mut i64) -> u32),
    A4(extern "C" fn(*mut JitCtx, i64, i64, i64, i64, *mut i64) -> u32),
}

/// E3a: one arity's GENERIC native entry, by parameter count (0-4). ABI:
/// `extern "C" fn(ctx, a0..an: *const Value, out: *mut Value) -> u32`.
/// `0` = ok (`*out` initialised, owned by the caller); `2` = error (an
/// `RjError` is in `(*ctx).pending`, `*out` untouched) -- see
/// docs/NATIVE-TIER-DESIGN.md #3/#4. Unlike [`NativeEntry`], there is no
/// `1` (bail): generic code has side effects, so every op here is total.
#[derive(Clone, Copy)]
pub enum GenericEntry {
    G0(extern "C" fn(*mut JitCtx, *mut Value) -> u32),
    G1(extern "C" fn(*mut JitCtx, *const Value, *mut Value) -> u32),
    G2(extern "C" fn(*mut JitCtx, *const Value, *const Value, *mut Value) -> u32),
    G3(extern "C" fn(*mut JitCtx, *const Value, *const Value, *const Value, *mut Value) -> u32),
    G4(extern "C" fn(*mut JitCtx, *const Value, *const Value, *const Value, *const Value, *mut Value) -> u32),
}

/// Per-`CompiledArity` JIT state: lowered at most once (like `CompileSlot`),
/// plus a runtime "never again" flag a bail sets -- see docs/JIT.md's bail
/// rule. Exists (as a stub) even when the `jit` feature is off, so
/// `compile::mod`/`eval::apply` never need a `#[cfg(feature = "jit")]`.
pub struct JitSlot {
    entry: OnceLock<Option<NativeEntry>>,
    disabled: AtomicBool,
    /// E1b: bails no longer disable on the first one -- a direct-call miss
    /// just means the callee hasn't been JIT'd yet, which a few more calls
    /// usually fix. See `record_bail`.
    bails: AtomicU32,
    /// E3a: the generic entry. No bail counter -- generic lowering either
    /// succeeds once (cached forever, like `entry`) or is permanently `None`
    /// for this arity (unsupported shape); nothing here ever re-runs from
    /// scratch at runtime.
    gentry: OnceLock<Option<GenericEntry>>,
    /// L1: `DEF_EPOCH` at which every `Intrinsic` in the body was last seen armed.
    armed_epoch: AtomicU64,
}

/// L1: E1 inlines intrinsics as armed at lowering time; true iff all still are.
fn e1_all_armed(ir: &Ir) -> bool {
    use crate::compile::ir::Ir as I;
    let all = |xs: &[Ir]| xs.iter().all(e1_all_armed);
    match ir {
        I::Intrinsic { chain, args, .. } => chain.intrinsic_armed() && all(args),
        I::Const(_) | I::LoadSlot(_) | I::LoadSlotTake(_) | I::SelfRef | I::GlobalRef { .. } => true,
        I::If { test, then, els } => e1_all_armed(test) && e1_all_armed(then) && els.as_deref().map_or(true, e1_all_armed),
        I::Do(b) => all(b),
        I::Let { binds, body } | I::Loop { binds, body, .. } => binds.iter().all(|(_, e)| e1_all_armed(e)) && all(body),
        I::Recur { args, .. } | I::CallGlobal { args, .. } => all(args),
        I::Call { callee, args, .. } => e1_all_armed(callee) && all(args),
        _ => false,
    }
}

/// E1b (docs/JIT.md): how many bails a single arity tolerates before it is
/// disabled for good. Was 1 in E1a; a direct call's miss path can itself
/// cause a bail while the callee warms up, so a lone caller-side bail is no
/// longer proof the arity is permanently unfit for native code.
const MAX_BAILS: u32 = 8;

impl JitSlot {
    pub fn pending() -> Self {
        Self {
            entry: OnceLock::new(),
            disabled: AtomicBool::new(false),
            bails: AtomicU32::new(0),
            gentry: OnceLock::new(),
            armed_epoch: AtomicU64::new(0),
        }
    }

    /// A permanently-disabled arity never re-enters native code -- checked
    /// by the call site before `get_or_lower`.
    #[inline]
    pub fn is_disabled(&self) -> bool {
        self.disabled.load(Ordering::Relaxed)
    }

    /// Records a runtime bail; disables the arity once `MAX_BAILS` have
    /// accumulated (docs/JIT.md's bail policy). `Relaxed` throughout: a
    /// missed increment under a race just means one extra native attempt
    /// before disabling, never a correctness issue (a bail is always safe
    /// to re-run through the interpreter).
    #[inline]
    pub fn record_bail(&self) {
        if self.bails.fetch_add(1, Ordering::Relaxed) + 1 >= MAX_BAILS {
            self.disabled.store(true, Ordering::Relaxed);
        }
    }

    /// Lowers arity `idx` of `code` on first use, caching the outcome
    /// (`None` -- outside the subset -- just as durably as `Some`, exactly
    /// like `CompileSlot::on_call`). Only called when `enabled()` is true.
    #[inline]
    pub fn get_or_lower(&self, code: &Arc<CompiledFn>, idx: usize) -> Option<&NativeEntry> {
        let e = self.lower_e1(code, idx)?;
        // L1: a later `(def + ..)` disarms an intrinsic E1 baked in -- re-check once per DEF_EPOCH.
        let now = DEF_EPOCH.load(Ordering::Acquire);
        if self.armed_epoch.load(Ordering::Acquire) != now {
            if !code.arities[idx].body.iter().all(e1_all_armed) {
                self.disabled.store(true, Ordering::Relaxed);
                return None;
            }
            self.armed_epoch.store(now, Ordering::Release);
        }
        Some(e)
    }

    fn lower_e1(&self, code: &Arc<CompiledFn>, idx: usize) -> Option<&NativeEntry> {
        self.entry
            .get_or_init(|| {
                #[cfg(feature = "jit")]
                {
                    lower::lower_arity(code, idx)
                }
                #[cfg(not(feature = "jit"))]
                {
                    None
                }
            })
            .as_ref()
    }

    /// E3a twin of [`Self::get_or_lower`] for the generic tier. Only called
    /// when [`generic_enabled`] is true.
    #[inline]
    pub fn get_or_lower_generic(&self, code: &Arc<CompiledFn>, idx: usize) -> Option<&GenericEntry> {
        self.gentry
            .get_or_init(|| {
                #[cfg(feature = "jit")]
                {
                    let entry = lower::lower_arity_generic(code, idx);
                    if explain_enabled() {
                        let name = code.name.as_deref().unwrap_or("<anonymous>");
                        let arity = &code.arities[idx];
                        match &entry {
                            Some(_) => eprintln!("jit-explain: fn {name} arity {idx} -> lowered"),
                            None => match lower::explain_generic(&arity.body, arity.n_params, arity.scratch_base) {
                                Ok(()) => eprintln!("jit-explain: fn {name} arity {idx} -> not lowered (variadic/arity>4/unprobeable layout)"),
                                Err(reason) => eprintln!("jit-explain: fn {name} arity {idx} -> not lowered: {reason}"),
                            },
                        }
                    }
                    entry
                }
                #[cfg(not(feature = "jit"))]
                {
                    None
                }
            })
            .as_ref()
    }
}

impl Default for JitSlot {
    fn default() -> Self {
        Self::pending()
    }
}

/// Calls a lowered arity's native entry with `args` (already checked by the
/// caller to be exactly `Value::Int`s, one per parameter). `remaining_depth`
/// seeds `JitCtx::depth`. `None` means BAIL: the caller falls back to the
/// interpreter with `args` untouched, and disables this slot.
pub fn call_native(entry: &NativeEntry, args: &[Value], remaining_depth: i64) -> Option<Value> {
    let mut a = [0i64; 4];
    for (slot, v) in a.iter_mut().zip(args) {
        *slot = match v {
            Value::Int(n) => *n,
            _ => return None,
        };
    }
    let mut ctx = JitCtx {
        depth: remaining_depth.min(10_000),
        interp: std::ptr::null_mut(),
        base: 0,
        pending: None,
        self_val: std::ptr::null(),
    };
    let mut out: i64 = 0;
    let status = match entry {
        NativeEntry::A0(f) => f(&mut ctx, &mut out),
        NativeEntry::A1(f) => f(&mut ctx, a[0], &mut out),
        NativeEntry::A2(f) => f(&mut ctx, a[0], a[1], &mut out),
        NativeEntry::A3(f) => f(&mut ctx, a[0], a[1], a[2], &mut out),
        NativeEntry::A4(f) => f(&mut ctx, a[0], a[1], a[2], a[3], &mut out),
    };
    if status == 0 {
        Some(Value::Int(out))
    } else {
        None
    }
}

/// E3a twin of [`call_native`] for the generic ABI (docs/NATIVE-TIER-DESIGN.md
/// #3/#4): `args` are BORROWED for the call's duration (never cloned here --
/// the callee clones only what escapes). Never bails: `Ok` or a real `Err`.
pub fn call_native_generic(
    entry: &GenericEntry,
    interp: &mut Interp,
    args: &[Value],
    remaining_depth: i64,
    self_val: &Value,
) -> Result<Value, RjError> {
    let mut ctx = JitCtx {
        depth: remaining_depth.min(10_000),
        interp: interp as *mut Interp,
        base: interp.stack.len(),
        pending: None,
        self_val: self_val as *const Value,
    };
    let mut out = std::mem::MaybeUninit::<Value>::uninit();
    let out_ptr = out.as_mut_ptr();
    let a: Vec<*const Value> = args.iter().map(|v| v as *const Value).collect();
    let status = match entry {
        GenericEntry::G0(f) => f(&mut ctx, out_ptr),
        GenericEntry::G1(f) => f(&mut ctx, a[0], out_ptr),
        GenericEntry::G2(f) => f(&mut ctx, a[0], a[1], out_ptr),
        GenericEntry::G3(f) => f(&mut ctx, a[0], a[1], a[2], out_ptr),
        GenericEntry::G4(f) => f(&mut ctx, a[0], a[1], a[2], a[3], out_ptr),
    };
    if status == 0 {
        // SAFETY: status 0 means the callee initialised `*out_ptr`.
        Ok(unsafe { out.assume_init() })
    } else {
        Err(*ctx.pending.take().expect("status 2 must set JitCtx::pending"))
    }
}

// ---------------------------------------------------------------------------
// G1: the threaded tier (docs/JIT.md "Threaded tier (G1)"). Rust owns every
// `Value`; Cranelift-generated code (in `threaded`, feature-gated) only
// sequences calls to the `extern "C" fn`s below and branches on their
// integer status. These types/fns are unconditional (like `JitSlot` et al
// above) so `compile::mod`/`exec.rs` never need `#[cfg(feature = "jit")]`.
// ---------------------------------------------------------------------------

/// Passed to a threaded-tier entry. `slots` is `locals.slots.as_mut_ptr()`,
/// cached so generated code can index it without going through `locals`;
/// `locals` itself is kept so `jit_t_exec` can re-enter the real
/// interpreter's `exec` (captures/self-ref/anything not natively lowered).
/// The `'static` is a lie erased for the FFI boundary -- sound because a
/// `TCtx` never outlives the `call_threaded` stack frame that built it.
#[repr(C)]
pub struct TCtx {
    pub interp: *mut Interp,
    pub locals: *mut Locals<'static>,
    pub slots: *mut Value,
    pub out: Value,
    pub err: Option<RjError>,
}

/// One arity's threaded-tier entry: `status == 0` means the value is in
/// `ctx.out`, `1` means `Flow::Recur` (fn-level), `2` means `ctx.err`.
#[derive(Clone, Copy)]
pub struct ThreadedEntry(pub(crate) extern "C" fn(*mut TCtx) -> u32);

/// `node`'s value is computed by the real interpreter (`compile::exec::exec`)
/// and written to `slots[d]`, or to `ctx.out` when `d == OUT`. This is the
/// catch-all every node the threaded lowering doesn't natively structure
/// falls back to -- see `threaded::LowerCtx::lower_into`.
pub const THREADED_OUT: u32 = u32::MAX;

/// `MOVA_JIT_EXECSTATS=1`: per-`Ir`-kind count of `jit_t_exec` fallbacks, printed at exit.
fn exec_stats_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let on = std::env::var("MOVA_JIT_EXECSTATS").map(|v| v == "1").unwrap_or(false);
        if on {
            extern "C" fn dump() {
                let mut v: Vec<(String, u64)> = EXEC_STATS.lock().unwrap().iter().map(|(k, n)| (k.clone(), *n)).collect();
                v.sort_by(|a, b| b.1.cmp(&a.1));
                for (k, n) in v {
                    eprintln!("jit_t_exec {k}: {n}");
                }
            }
            unsafe { libc::atexit(dump) };
        }
        on
    })
}
static EXEC_STATS: std::sync::Mutex<std::collections::BTreeMap<String, u64>> = std::sync::Mutex::new(std::collections::BTreeMap::new());

fn exec_kind(ir: &Ir) -> String {
    match ir {
        Ir::Intrinsic { op, .. } => format!("Intrinsic::{op:?}"),
        Ir::Const(v) => format!("Const::{}", v.type_name()),
        Ir::Call { callee, .. } => format!("Call(callee={})", exec_kind(callee)),
        other => {
            let s = match other {
                Ir::LoadSlot(_) => "LoadSlot", Ir::LoadSlotTake(_) => "LoadSlotTake", Ir::LoadCapture(_) => "LoadCapture",
                Ir::SelfRef => "SelfRef", Ir::GlobalRef { .. } => "GlobalRef", Ir::CreationEnvLookup { .. } => "CreationEnvLookup",
                Ir::SetMutField { .. } => "SetMutField", Ir::If { .. } => "If", Ir::Do(_) => "Do", Ir::Let { .. } => "Let",
                Ir::Loop { .. } => "Loop", Ir::Recur { .. } => "Recur", Ir::NumLoop(_) => "NumLoop", Ir::CallGlobal { .. } => "CallGlobal",
                Ir::CallCreationEnv { .. } => "CallCreationEnv", Ir::VectorLit(_) => "VectorLit", Ir::MapLit(_) => "MapLit",
                Ir::SetLit(_) => "SetLit", Ir::Throw { .. } => "Throw", Ir::MakeClosure { .. } => "MakeClosure",
                Ir::MakeRecGroup { .. } => "MakeRecGroup", Ir::SiblingRef(_) => "SiblingRef", Ir::Try { .. } => "Try",
                Ir::Def { .. } => "Def", Ir::DynBind(_) => "DynBind", Ir::Escape(_) => "Escape", Ir::FieldGet(_) => "FieldGet", Ir::New(_) => "New",
                _ => "?",
            };
            s.to_string()
        }
    }
}

/// K5 census key: kind, plus the first child kind for structural nodes.
#[cfg(feature = "k2-count")]
pub(crate) fn census_kind(ir: &Ir) -> String {
    match ir {
        Ir::Let { binds, body } => format!("Let(binds={}, body0={})", binds.len(), body.first().map(exec_kind).unwrap_or_default()),
        Ir::Escape(e) => format!("Escape({})", { let t = format!("{:?}", e.form).replace("Form { value: ", "").split("span").filter(|x| x.contains("Atom")).map(|x| x.to_string()).collect::<Vec<_>>().join(""); t.chars().take(120).collect::<String>() }),
        Ir::Loop { binds, .. } => format!("Loop(plain={})", binds.iter().all(|(p, _)| matches!(p, crate::compile::ir::CompiledPattern::Slot(_)))),
        other => exec_kind(other),
    }
}

extern "C" fn jit_t_exec(ctx: *mut TCtx, node: *const Ir, d: u32) -> u32 {
    unsafe {
        if exec_stats_on() {
            *EXEC_STATS.lock().unwrap().entry(exec_kind(&*node)).or_insert(0) += 1;
        }
        let ctxr = &mut *ctx;
        let interp = &mut *ctxr.interp;
        let l = &mut *ctxr.locals;
        #[cfg(feature = "k2-count")]
        let tok = crate::k2count::census::on().then(crate::k2count::census::enter);
        let r = crate::compile::exec::exec(interp, &*node, l);
        #[cfg(feature = "k2-count")]
        if let Some(t) = tok {
            crate::k2count::census::exit(t, || census_kind(&*node));
        }
        match r {
            Ok(Flow::Val(v)) => {
                if d == THREADED_OUT {
                    ctxr.out = v;
                } else {
                    *ctxr.slots.add(d as usize) = v;
                }
                0
            }
            Ok(Flow::Recur) => 1,
            Err(e) => {
                ctxr.err = Some(e);
                2
            }
        }
    }
}

/// Writes `Value::Nil` into `slots[d]`/`ctx.out` -- an `If` with no `else`,
/// lowered natively, needs this without a real `Ir` node to hand `jit_t_exec`.
extern "C" fn jit_t_nil(ctx: *mut TCtx, d: u32) {
    unsafe {
        let ctxr = &mut *ctx;
        if d == THREADED_OUT {
            ctxr.out = Value::Nil;
        } else {
            *ctxr.slots.add(d as usize) = Value::Nil;
        }
    }
}

/// `(*slot).truthy()`, for a natively-lowered `If`'s test.
extern "C" fn jit_t_truthy(slot: *const Value) -> u8 {
    unsafe { (*slot).truthy() as u8 }
}

/// Moves `*src` into `*dst` (`Nil` left behind), dropping `*dst`'s old value
/// exactly as `std::mem::replace` does -- the native `Loop` rebind's twin of
/// `exec_loop`'s `std::mem::replace(&mut l.slots[base + i], Value::Nil)`.
extern "C" fn jit_t_move(dst: *mut Value, src: *mut Value) {
    unsafe {
        *dst = std::mem::replace(&mut *src, Value::Nil);
    }
}

/// `interp.tick_fuel()`, for a natively-lowered `Loop`'s back-edge.
extern "C" fn jit_t_tick_fuel(ctx: *mut TCtx) -> u32 {
    unsafe {
        let ctxr = &mut *ctx;
        let interp = &mut *ctxr.interp;
        match interp.tick_edge() {
            Ok(()) => 0,
            Err(e) => {
                ctxr.err = Some(e);
                2
            }
        }
    }
}

/// G1 twin of [`JitSlot`]: lowered at most once, correct by construction (no
/// runtime bail -- unlike the value-shape-speculating tiers, this one never
/// guesses, so there is nothing to fall back FROM at runtime).
pub struct ThreadedSlot {
    entry: OnceLock<Option<(ThreadedEntry, u32)>>,
    // K2: calls seen before lowering (hot-only JIT, `MOVA_JIT_HOT_N`).
    calls: std::sync::atomic::AtomicU32,
    // S3: `native_recur` the entry was lowered with (0 = none, 1 = false, 2 = true).
    recur: std::sync::atomic::AtomicU8,
    // S3: relocatable native code from the heap image, bound on first call.
    aot: OnceLock<AotBlob>,
}

/// S3: one arity's persisted code record (see `jit::aot`): `len` bytes at
/// `off` of the image file, read with `pread` on bind so the file's pages
/// never count toward RSS.
pub struct AotBlob {
    pub file: Arc<std::fs::File>,
    pub off: u64,
    pub len: usize,
}

/// K2: lower an arity only after this many calls (`MOVA_JIT_HOT_N`). S3:
/// without the env var, 0 (first call) -- except in a heap-image process
/// (`MOVA_IMAGE`): 1000 while booting and after a restore that brought
/// native code (training-hot fns are bound already), 0 when writing one.
fn jit_hot_n() -> u32 {
    static N: OnceLock<Option<u32>> = OnceLock::new();
    static IMG: OnceLock<bool> = OnceLock::new();
    N.get_or_init(|| std::env::var("MOVA_JIT_HOT_N").ok().and_then(|v| v.parse().ok())).unwrap_or_else(|| {
        match HOT_DEFAULT.load(Ordering::Relaxed) {
            u32::MAX if *IMG.get_or_init(|| std::env::var("MOVA_IMAGE").is_ok_and(|v| !v.is_empty())) => 1000,
            u32::MAX => 0,
            n => n,
        }
    })
}

static HOT_DEFAULT: AtomicU32 = AtomicU32::new(u32::MAX);

/// S3: called by the heap image once it knows: restored (`restored`) or about to write.
pub fn image_decided(restored: bool) {
    let n = if restored && AOT_ATTACHED.load(Ordering::Relaxed) { 1000 } else { 0 };
    HOT_DEFAULT.store(n, Ordering::Relaxed);
}

/// S3: set once any arity got image code attached.
static AOT_ATTACHED: AtomicBool = AtomicBool::new(false);

/// S4: the restored image carries native code (attached lazily, on IR decode).
pub fn note_image_code() {
    AOT_ATTACHED.store(true, Ordering::Relaxed);
}

/// metrics: (threaded lowerings, their ns, AOT arities bound).
pub fn stats() -> (u64, u64, u64) {
    (THREADED_LOWER_COUNT.load(Ordering::Relaxed) as u64, THREADED_LOWER_NS.load(Ordering::Relaxed), AOT_BOUND_COUNT.load(Ordering::Relaxed) as u64)
}

/// S3: arities bound from image code / total ns spent binding them.
pub static AOT_BOUND_COUNT: AtomicUsize = AtomicUsize::new(0);
pub static AOT_BOUND_NS: AtomicU64 = AtomicU64::new(0);

impl ThreadedSlot {
    pub fn pending() -> Self {
        Self {
            entry: OnceLock::new(),
            calls: std::sync::atomic::AtomicU32::new(0),
            recur: std::sync::atomic::AtomicU8::new(0),
            aot: OnceLock::new(),
        }
    }

    /// Lowers arity `idx` of `code` on first use. The `u32` is how many temp
    /// slots past `arity.n_slots` the lowering reserved (`If` test temps).
    #[inline]
    /// K1: `native_recur` (no `^long`/`^double` coercion) makes a fn-level
    /// `recur` a native jump, so status 1 never leaves the entry; it must be
    /// the same on every call for a given code (it is: coercion is per fn form).
    pub fn get_or_lower(&self, code: &Arc<CompiledFn>, idx: usize, native_recur: bool) -> Option<&(ThreadedEntry, u32)> {
        if let Some(e) = self.entry.get() {
            return e.as_ref();
        }
        let n = jit_hot_n();
        if n > 0 && self.aot.get().is_none() && self.calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < n {
            return None;
        }
        self.entry
            .get_or_init(|| {
                #[cfg(feature = "jit")]
                {
                    let r = self
                        .aot
                        .get()
                        .and_then(|b| {
                            use std::os::unix::fs::FileExt;
                            let mut buf = vec![0u8; b.len];
                            b.file.read_exact_at(&mut buf, b.off).ok()?;
                            aot::bind(code, idx, native_recur, &buf)
                        })
                        .or_else(|| threaded::lower_arity(code, idx, native_recur));
                    if r.is_some() {
                        self.recur.store(1 + native_recur as u8, Ordering::Relaxed);
                    }
                    r
                }
                #[cfg(not(feature = "jit"))]
                {
                    let _ = (code, idx, native_recur);
                    None
                }
            })
            .as_ref()
    }

    /// S3: relocatable code for the heap image, iff this arity has a threaded entry.
    pub fn image_code(&self, code: &Arc<CompiledFn>, idx: usize) -> Option<Vec<u8>> {
        let r = self.recur.load(Ordering::Relaxed);
        if r == 0 || !matches!(self.entry.get(), Some(Some(_))) {
            return None;
        }
        #[cfg(feature = "jit")]
        {
            threaded::lower_for_image(code, idx, r == 2)
        }
        #[cfg(not(feature = "jit"))]
        {
            let _ = (code, idx);
            None
        }
    }

    /// S3: true iff lowered (or bound) in this process.
    pub fn has_entry(&self) -> bool {
        matches!(self.entry.get(), Some(Some(_)))
    }

    /// S3: attach image code; bound (no lowering, no hot-N wait) on first call.
    pub fn attach_image_code(&self, b: AotBlob) {
        AOT_ATTACHED.store(true, Ordering::Relaxed);
        let _ = self.aot.set(b);
    }
}

impl Default for ThreadedSlot {
    fn default() -> Self {
        Self::pending()
    }
}

/// Total time spent lowering (Cranelift compile cost) and how many arities
/// got a threaded entry -- printed cumulatively (see `threaded::lower_arity`)
/// under `MOVA_JIT_EXPLAIN=1`.
pub static THREADED_LOWER_NS: AtomicU64 = AtomicU64::new(0);
pub static THREADED_LOWER_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Runs a threaded-tier entry. `l` is BORROWED for the call's duration; on
/// return its `slots` hold whatever the native code (and the `jit_t_exec`
/// calls it made back into the interpreter) left there, exactly like a
/// direct `exec_body` call would.
pub(crate) fn call_threaded(
    entry: &ThreadedEntry,
    interp: &mut Interp,
    l: &mut Locals,
) -> Result<Flow, RjError> {
    let mut ctx = TCtx {
        interp: interp as *mut Interp,
        locals: l as *mut Locals as *mut Locals<'static>,
        slots: l.slots.as_mut_ptr(),
        out: Value::Nil,
        err: None,
    };
    #[cfg(feature = "k2-count")]
    let tok = crate::k2count::census::on().then(crate::k2count::census::enter);
    let status = (entry.0)(&mut ctx as *mut TCtx);
    #[cfg(feature = "k2-count")]
    if let Some(t) = tok {
        crate::k2count::census::exit(t, || "Native".to_string());
    }
    match status {
        0 => Ok(Flow::Val(std::mem::replace(&mut ctx.out, Value::Nil))),
        1 => Ok(Flow::Recur),
        2 => Err(ctx.err.take().expect("status 2 must set TCtx::err")),
        _ => unreachable!("bad threaded status {status}"),
    }
}
