//! The compiled-fn tier (COMPILE-TIER-DESIGN.md, stage S2): a
//! resolved-IR "closure compilation" pass that runs once, at `fn`-closure
//! *creation* time, and replaces per-call symbol lookups / form dispatch
//! with direct slot indices and pre-interned global `VarCell`s.
//!
//! ## Contract with the tree-walker
//!
//! The compiled tier is an OPTIONAL fast path, never a semantic change:
//! every `Closure` still carries its full legacy `arities` and its creation
//! `env`, and `Closure::compiled == None` means "tree-walk this exactly like
//! before". Compilation is all-or-nothing per **fn** with exactly ONE
//! exception: the moment `resolve` meets anything it doesn't handle it
//! returns `None` and the whole fn falls back to `run_closure_body`.
//! See `resolve.rs` for the exact compile/fallback matrix.
//!
//! The exception is `Ir::Escape` (field3/W-RESOLVE): an INTEROP CALL
//! (`(.m x)`, `(C. a)`, `(. x m)`, `(new C a)`) compiles to a node that
//! hands that one form, unmodified, back to `Interp::eval_form_in`, so the
//! interop-free rest of the fn still compiles. It was added because
//! whole-fn granularity was costing 46x on the real thing -- one
//! `(.await barrier)` was dragging a 10 000-iteration interop-free loop
//! onto the tree-walker with it (assembled `delays.clj`: 128.40s -> 2.69s,
//! same 25 assertions). It is deliberately the ONLY mixed-mode node, and
//! it is not "bridging machinery to get subtly wrong" in the sense this
//! paragraph warned about: it does not re-implement interop on a second
//! path, it delegates to the first one. The only thing it has to get right
//! is the env the escaped form sees -- see `ir::Escape` for that argument,
//! and `docs/W-RESOLVE-interop-escape-decision.md` for the whole case.
//!
//! One consequence worth stating: a NESTED fn cannot fall back on its own,
//! because its `Ir::MakeClosure` node has already been compiled into its
//! parent. So anything that would make a nested fn fall back makes the
//! whole enclosing fn fall back instead -- `Bail` propagates all the way
//! out. That is why a nested closure built by `MakeClosure` always has
//! `compiled: Some(..)`; its legacy `arities` are still fully populated,
//! but only arity selection and arity-error messages read them.
//!
//! ## What is shared and what is per-instance
//!
//! `CompiledFn` is the immutable *code* (one per compiled fn form) and
//! `CompiledClosure` pairs it with that particular closure instance's
//! `captures` -- by-value snapshots of free symbols.
//!
//! Which of the two mechanisms a free symbol uses depends on who creates the
//! closure:
//!
//! - created by TREE-WALKED code (`eval_fn_form` -> `compile_fn`): its free
//!   symbols are resolved through the live creation env on every access
//!   (`Ir::CreationEnvLookup`, S2.5), because those frames can still gain or
//!   rebind a name after the closure exists. `captures` stays empty, and the
//!   code is compiled once per creation.
//! - created by COMPILED code (`Ir::MakeClosure`, S4): the enclosing frame's
//!   bindings are slots, which are never rebound under a live closure, so
//!   they are snapshotted by value into `captures`; anything further out
//!   keeps the enclosing fn's own resolution (global cells, or a
//!   `CreationEnvLookup` against the enclosing closure's creation env, which
//!   `MakeClosure` therefore hands to the new closure as its `env`). The
//!   `Arc<CompiledFn>` template is compiled ONCE, when the enclosing fn is,
//!   and shared by every instance the nested `fn` form ever produces --
//!   creating one costs a capture snapshot and two `Arc` bumps, not a
//!   compile.
//! - created as a MEMBER of a recursive binding group (`Ir::MakeRecGroup`,
//!   the `letfn` shape): the same by-value snapshot for everything OUTSIDE
//!   the run, plus a shared [`RecGroup`] through which the members reach
//!   each other. That third route exists because mutual recursion cannot be
//!   expressed with by-value captures without building an `Arc` cycle --
//!   see `RecGroup`'s doc for the whole argument, and `resolve.rs`'s
//!   "Recursive binding groups" section for what compiles into one.
//!
//! `CompiledArity` mirrors `value::Arity` 1:1 and IN THE SAME ORDER, which
//! is load-bearing: `eval::apply` selects the arity ONCE, from the legacy
//! `arities` (so arity-selection and arity-error messages can never drift
//! between the two tiers), and then indexes `code.arities` with that same
//! index.
//!
//! ## Kill switches
//!
//! `MOVA_NO_COMPILE=1` (read once, cached in a `OnceLock`) disables the
//! tier process-wide. Because that is process-wide it is useless for a
//! differential test running both tiers in one process, so `Interp` also
//! carries a per-interpreter `compile_enabled` flag
//! (`Interp::with_compile_enabled`) -- `tests/differential_test.rs` builds
//! one session of each kind side by side. Both are consulted here, at the
//! single entry point.
//!
//! `MOVA_NO_LASTUSE=1` is the same shape of switch for the last-use
//! analysis (`compile::lastuse`, Perceus-lite phase 2): the tier stays on,
//! but no slot read is ever rewritten into a moving one, so every local read
//! clones exactly as it did before that landing. It composes with
//! `MOVA_NO_REUSE=1` (which decides what a native does with the handle it
//! is given, rather than whether the frame gives its handle up), and
//! `Interp::with_all_tiers` is its per-interpreter sibling.
//!
//! `MOVA_NO_NUMLOOP=1` (also read once) is the narrower switch: the tier
//! stays on, but `resolve.rs` never emits an `Ir::NumLoop`, so every `loop`
//! runs the generic node. It exists so the numeric-loop specialization can
//! be A/B'd -- and bisected against -- without turning off compilation
//! itself. `MOVA_NO_COMPILE=1` subsumes it. Both are gates on EMISSION, not
//! on execution: with the switch on there is no specialized node anywhere in
//! the process to be wrong about. See `resolve::num_loop_disabled_by_env`.

#[cfg(test)]
mod bench;
pub mod exec;
pub(crate) mod explain;
pub mod ir;
pub mod lanes;
pub(crate) mod lastuse;
mod resolve;

use std::sync::{Arc, OnceLock};

use crate::env::Env;
use crate::eval::Interp;
use crate::value::{Arity, Str, Symbol, Value};

/// One compiled closure *instance*: shared code plus this instance's
/// by-value captures (indexed by `Ir::LoadCapture`).
///
/// H2/fn-template-cache: `Clone` is cheap (an `Arc` bump + a `Vec<Value>`
/// clone, empty for every closure `eval_fn_form` creates -- see
/// `resolve::compile`'s `debug_assert!(ctx.captures.is_empty())` for the
/// outermost-compiled-fn case, the ONLY case reached from there) and is
/// what lets `eval_fn_form` share one compiled result across every
/// instance of the SAME syntactic fn literal.
#[derive(Clone)]
pub struct CompiledClosure {
    pub code: Arc<CompiledFn>,
    pub captures: Vec<Value>,
    /// `Some` iff this closure is a member of a RECURSIVE BINDING GROUP (the
    /// `letfn` shape -- see [`RecGroup`] and `ir::Ir::MakeRecGroup`), which
    /// is what `Ir::SiblingRef` / `CaptureSrc::Sibling` resolve against.
    /// `None` everywhere else, which is everywhere but `RecGroup::
    /// materialize`: `resolve::compile` and `exec::make_closure` both set it
    /// to `None`, so `value::Closure` and its three construction sites are
    /// untouched by this feature.
    pub group: Option<Arc<RecGroup>>,
}

/// The runtime half of `ir::Ir::MakeRecGroup`: everything a `letfn`-shaped
/// run of mutually-referring closures needs, held ONCE for the whole run.
///
/// ## Why a group at all
///
/// The tree-walker gets mutual recursion for free: every sibling closes over
/// the same live `let` frame, and `Env::get` re-reads it at CALL time, so a
/// forward reference resolves once the frame has been filled in. A compiled
/// scope has no live frame -- which is exactly why the deferred half of the
/// poison rule bailed the whole enclosing fn (see `resolve.rs`). This is the
/// frame's replacement: an immutable object, shared by every member of one
/// run, that can answer "member *i*" at any time after the run was built.
///
/// ## THE CYCLE ARGUMENT (this is the load-bearing part)
///
/// mova is precise-RC (`Arc`) with NO cycle collector, so any strong
/// reference cycle is a permanent leak. Mutual recursion is inherently
/// cyclic, so the cycle has to be broken by construction. Every strong edge
/// this design creates, in full:
///
/// ```text
///   let frame slot i  --strong-->  member closure i     (the OWNER)
///   member closure i  --strong-->  Arc<RecGroup>        (via CompiledClosure::group)
///   RecGroup          --strong-->  Arc<FnTemplate>      (immutable code, no Value in it)
///   RecGroup          --strong-->  outer capture Values (snapshotted from the
///                                  enclosing frame; every one of them existed
///                                  BEFORE the group did, and none is a member)
///   RecGroup          --WEAK---->  member closure i     (`cache`, below)
/// ```
///
/// No edge runs group -> member strongly, and no member ever holds another
/// member: an intra-group reference is `Ir::SiblingRef`, resolved through
/// the group on access, never a capture (see that node's doc for why a
/// by-value sibling capture -- in EITHER direction -- just relocates the
/// cycle). So the strong graph is a DAG rooted at whatever owns the member
/// closures, and dropping those roots drops the group and the members with
/// it. That is what makes an escaped `letfn` closure reclaimable, which the
/// tree-walked shape (closure <-> frame) is not.
///
/// ## The weak cache, and why identity is still stable
///
/// `cache[i]` is a `Weak` handle to member *i*'s closure. `materialize`
/// upgrades it when it can and rebuilds only when every strong handle is
/// gone, so repeated `SiblingRef` reads answer the SAME `Arc<Closure>` while
/// any handle is alive -- which is what makes `(identical? (e) (e))` true,
/// as it is in the tree-walker where both reads find one frame binding. Once
/// nothing holds a member any more, its identity is unobservable by
/// definition, so rebuilding is not a semantic difference; it is also
/// unreachable in practice while the run's own `let` slots are live, since
/// those are strong owners.
pub struct RecGroup {
    /// The creation env every member is given -- the ENCLOSING closure's
    /// creation env at `MakeRecGroup` time, i.e. the exact same env
    /// `make_closure` hands a nested fn, for the same reason (see
    /// `ir::Ir::CreationEnvLookup`).
    env: Env,
    /// The enclosing closure's namespace, by `make_closure`'s rule: a member
    /// was written in the same namespace as the fn whose body holds the
    /// `letfn`, not in whatever namespace is current when the node runs.
    ns: Str,
    /// `*unchecked-math*` as it stood when the GROUP was created, shared by
    /// every member.
    ///
    /// Deviation, stated rather than hidden: the tree-walker reads the flag
    /// once per `fn` FORM, so it could in principle differ between two
    /// siblings; the group reads it once for the whole run. The only way to
    /// observe that is to toggle `*unchecked-math*` from inside the binding
    /// vector BETWEEN two sibling `fn` forms -- which the run's own shape
    /// forbids, since every binding in a run is a bare symbol bound to a
    /// `fn` literal and a `fn` literal cannot `set!` anything. Accepted.
    unchecked_math: bool,
    /// The members' shared code, in binding order -- the very
    /// `Arc<FnTemplate>`s `Ir::MakeRecGroup` carries, compiled once with the
    /// enclosing fn. Immutable, and holds no `Value`, so this edge can never
    /// participate in a cycle.
    templates: Vec<Arc<ir::FnTemplate>>,
    /// Per-member snapshot of the outer captures, taken once out of the
    /// creating frame (`Vec<Value>`, 1:1 with `templates`). Never contains a
    /// member of this group -- see the cycle argument above.
    captures: Vec<Vec<Value>>,
    /// The weak per-member identity cache. `Mutex` rather than an atomic
    /// because a `Weak` is two words; it is only ever taken on a
    /// `SiblingRef`/`CaptureSrc::Sibling` read, never on the arithmetic or
    /// call fast paths.
    cache: Vec<std::sync::Mutex<std::sync::Weak<crate::value::Closure>>>,
}

impl RecGroup {
    /// Builds the group and materializes every member. The returned
    /// closures are the run's members in binding order; the CALLER (i.e.
    /// `exec`'s `Ir::MakeRecGroup` arm) writes them into their frame slots,
    /// which is what makes the slots their strong owners.
    pub(crate) fn build(
        templates: Vec<Arc<ir::FnTemplate>>,
        captures: Vec<Vec<Value>>,
        env: Env,
        ns: Str,
        unchecked_math: bool,
    ) -> (Arc<RecGroup>, Vec<Value>) {
        let n = templates.len();
        let mut cache = Vec::with_capacity(n);
        for _ in 0..n {
            cache.push(std::sync::Mutex::new(std::sync::Weak::new()));
        }
        let group = Arc::new(RecGroup {
            env,
            ns,
            unchecked_math,
            templates,
            captures,
            cache,
        });
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            // Every index is in range by construction, so the `None` arm is
            // unreachable; `Value::Nil` rather than a panic keeps a future
            // caller mistake a wrong answer instead of a crash, and
            // `materialize`'s own callers surface the real error.
            out.push(match group.materialize(i) {
                Some(c) => Value::Fn(c),
                None => Value::Nil,
            });
        }
        (group, out)
    }

    /// Member `i`, as the SAME `Arc<Closure>` every other live handle names
    /// (upgrade-or-build against the weak cache -- see this type's doc).
    /// `None` only for an out-of-range index, which `resolve.rs` cannot
    /// emit.
    pub(crate) fn materialize(self: &Arc<Self>, i: usize) -> Option<Arc<crate::value::Closure>> {
        let t = self.templates.get(i)?;
        let caps = self.captures.get(i)?;
        let cell = self.cache.get(i)?;
        // A poisoned lock would mean a panic inside this tiny critical
        // section (which holds no user code at all); recovering the handle
        // is strictly better than propagating the poison into unrelated
        // code.
        let mut slot = cell.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = slot.upgrade() {
            return Some(existing);
        }
        #[cfg(feature = "leak-probe")]
        crate::value::CLOSURE_CREATED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let c = Arc::new(crate::value::Closure {
            name: t.code.name.clone(),
            arities: t.arities.clone(),
            env: self.env.clone(),
            ns: self.ns.clone(),
            // A group member is compiled BY CONSTRUCTION (its template was
            // compiled with the enclosing fn), exactly like a `MakeClosure`
            // instance -- no bail to regret. See `make_closure`.
            compiled: crate::value::CompileSlot::settled(
                Some(CompiledClosure {
                    code: t.code.clone(),
                    captures: caps.clone(),
                    group: Some(self.clone()),
                }),
                crate::lens::NO_SITE,
            ),
            unchecked_math: self.unchecked_math,
            // Never consulted: `compiled` is already settled, so `on_call`
            // never reaches this fn's `def_span`.
            def_span: crate::reader::Span { start: 0, end: 0 },
            def_source_id: crate::source_registry::SrcRef::NONE,
            native_macro: None,
        });
        *slot = Arc::downgrade(&c);
        Some(c)
    }
}

/// The immutable compiled code for one `fn` form.
pub struct CompiledFn {
    /// Only used for diagnostics; the *live* name for stack frames still
    /// comes from `Closure::name` (identical value).
    pub name: Option<Str>,
    /// 1:1 with, and in the same order as, the closure's legacy `arities`.
    pub arities: Vec<CompiledArity>,
    /// Heap-image gate-1: the local each capture slot snapshots (parallel to
    /// `CompiledClosure::captures`), so an image can rebuild a tree-walk env.
    pub capture_syms: Vec<Symbol>,
}

/// One arity's frame layout + body.
///
/// Slot layout (all indices are into the single per-call `slots` Vec):
///
/// ```text
///   0 .. n_params            positional params
///   n_params                 the `& rest` param, if `variadic`
///   scratch_base .. +n_recur recur scratch (see exec.rs)
///   ..                       every other binding, in compile order: one
///                            slot per NAME a let/loop pattern introduces,
///                            one per loop's raw-value scratch block, one
///                            per `catch` binding
/// ```
///
/// `scratch_base == n_recur == n_params + variadic`: `recur` writes its
/// evaluated arguments into the scratch block and unwinds; the fn-body
/// trampoline copies scratch back over the param slots and re-runs. Nested
/// `loop`s get their own scratch blocks, so they can never collide.
pub struct CompiledArity {
    pub n_params: usize,
    pub variadic: bool,
    /// `n_params + variadic`: how many values a `recur` to this fn carries.
    pub n_recur: usize,
    pub scratch_base: u16,
    pub n_slots: usize,
    pub body: Vec<ir::Ir>,
    /// E1a (docs/JIT.md): lowered lazily, at most once, the first time
    /// `MOVA_JIT=1` and `eval::apply::apply_closure_buf` reach this arity.
    pub jit: crate::jit::JitSlot,
    /// G1 (docs/JIT.md "Threaded tier"): the threaded-tier twin of `jit`,
    /// lowered lazily and cached the same way.
    pub threaded: crate::jit::ThreadedSlot,
}

/// True when `MOVA_NO_COMPILE=1` was set in the environment at process
/// start. Read exactly once (COMPILE-TIER-DESIGN.md's "checked once"
/// requirement) so the hot creation path never touches the environment.
fn disabled_by_env() -> bool {
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| std::env::var("MOVA_NO_COMPILE").is_ok_and(|v| v == "1"))
}

/// Lazy tier-up's `N`: how many calls a fn takes before `CompileSlot::
/// on_call` fires its one compile attempt. `MOVA_LAZY_TIER_N`, default `1`
/// (compile on the first call) -- read once, like `MOVA_NO_COMPILE`, since
/// it is a whole-process tuning knob, not a per-test override.
pub(crate) fn lazy_tier_n() -> u32 {
    static N: OnceLock<u32> = OnceLock::new();
    *N.get_or_init(|| {
        std::env::var("MOVA_LAZY_TIER_N")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|&n: &u32| n >= 1)
            .unwrap_or(1)
    })
}

/// The one entry point: attempts to compile a `fn` form's already-parsed
/// arities against the closure's creation `env`. `None` means "tree-walk
/// this fn" and is always a correct answer.
///
/// Called from `eval_fn_form` only -- `defmacro` bodies are deliberately
/// never compiled (a macro runs at expansion time against unevaluated
/// forms, and the compiled tier freezes macro expansion, so compiling
/// macros would compound the one documented deviation of this tier).
///
/// field4/W-LENS-1: also returns the fn's regret-ledger site id (see
/// `crate::lens` and `value::Closure::lens_site`), or `lens::NO_SITE` when
/// this fn has nothing to regret. Sites are allocated LAZILY -- only a fn
/// that bails, or one that emits an `Ir::Escape`, ever consumes one -- so a
/// clean bootstrap (well over a thousand `defn`s, nearly all of which
/// compile cleanly) spends no site ids at all and the fast tier never
/// touches a counter.
// SPIKE/IRCOST (throwaway, `spike/ircost` branch only): env-gated
// (`MOVA_IRCOST=1`) counters for "how much wall time does `resolve::compile`
// (the whole IR-compile pass, incl. its internal macroexpand-all) cost per
// arity-group compiled, at clojure-lsp scale". Answers "can AOT just
// re-run this at restore instead of serializing IR". Same
// atexit-hook-on-first-use pattern as `load_trace::enabled`.
pub(crate) mod ircost {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::OnceLock;

    static ENABLED: OnceLock<bool> = OnceLock::new();
    pub static COUNT: AtomicU64 = AtomicU64::new(0);
    pub static TOTAL_NS: AtomicU64 = AtomicU64::new(0);
    pub static MAX_NS: AtomicU64 = AtomicU64::new(0);
    pub static MACRO_NS: AtomicU64 = AtomicU64::new(0);
    pub static OUTER_NS: AtomicU64 = AtomicU64::new(0);
    pub static SEEN: std::sync::Mutex<Option<std::collections::HashMap<(usize, usize, usize), (u32, u64)>>> = std::sync::Mutex::new(None);
    pub fn site(k: (usize, usize, usize), ns: u64) {
        let mut g = SEEN.lock().unwrap();
        let e = g.get_or_insert_with(Default::default).entry(k).or_default();
        e.0 += 1;
        e.1 += ns;
    }
    thread_local!(pub static DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) });

    pub fn enabled() -> bool {
        *ENABLED.get_or_init(|| {
            let on = std::env::var("MOVA_IRCOST").map(|v| v == "1").unwrap_or(false);
            if on {
                unsafe {
                    libc::atexit(atexit_print);
                }
            }
            on
        })
    }

    pub fn record(ns: u64) {
        COUNT.fetch_add(1, Ordering::Relaxed);
        let tot = TOTAL_NS.fetch_add(ns, Ordering::Relaxed) + ns;
        MAX_NS.fetch_max(ns, Ordering::Relaxed);
        let n = COUNT.load(Ordering::Relaxed);
        if n % 100 == 0 {
            eprintln!("[ircost] n={n} cum_ms={:.1} at +{:.0} ms", tot as f64 / 1e6, crate::load_trace::since_start_ms());
        }
    }

    extern "C" fn atexit_print() {
        let count = COUNT.load(Ordering::Relaxed);
        let total_ns = TOTAL_NS.load(Ordering::Relaxed);
        let max_ns = MAX_NS.load(Ordering::Relaxed);
        if let Some(m) = SEEN.lock().unwrap().as_ref() {
            let dup: Vec<_> = m.values().filter(|e| e.0 > 1).collect();
            let dup_ms: f64 = dup.iter().map(|e| e.1 as f64 * (e.0 - 1) as f64 / e.0 as f64).sum::<f64>() / 1e6;
            eprintln!("[ircost] unique sites {} dup-sites {} dup-compiles {} dup_ms {dup_ms:.1}", m.len(), dup.len(), dup.iter().map(|e| e.0 - 1).sum::<u32>());
        }
        let mean_us = if count > 0 { (total_ns as f64 / count as f64) / 1e3 } else { 0.0 };
        eprintln!(
            "[ircost] arities_compiled={count} outer_ms={:.1} macro_ms={:.1} total_ms={:.3} mean_us={:.2} max_us={:.2}",
            OUTER_NS.load(Ordering::Relaxed) as f64 / 1e6,
            MACRO_NS.load(Ordering::Relaxed) as f64 / 1e6,
            total_ns as f64 / 1e6,
            mean_us,
            max_ns as f64 / 1e3,
        );
    }
}

pub(crate) fn compile_fn(
    interp: &mut Interp,
    name: Option<&Str>,
    arities: &[Arity],
    env: &Env,
    span: crate::reader::Span,
) -> (Option<CompiledClosure>, u32) {
    if !interp.compile_enabled() || disabled_by_env() {
        // `MOVA_NO_COMPILE=1` (or a compile-disabled interp): every fn
        // tree-walks by CONFIGURATION, not by a bail the compiler chose.
        // Counting those would misreport the DEFAULT build, which is the
        // build the ledger exists to describe.
        return (None, crate::lens::NO_SITE);
    }
    let ircost_t0 = ircost::enabled().then(std::time::Instant::now);
    if ircost_t0.is_some() {
        ircost::DEPTH.with(|d| d.set(d.get() + 1));
    }
    // field1/W-EXPLAIN: `resolve::compile` now reports WHY, alongside
    // WHETHER, every time -- `explain::record` is the one place that turns
    // that into an `MOVA_EXPLAIN=1` line and/or a `compile-explain`
    // registry entry. Never perturbs the `Some`/`None` this fn returns: the
    // match below is exactly the shape it was before this landed.
    let ircost_result = resolve::compile(interp, name, arities, env, span);
    if let Some(t0) = ircost_t0 {
        let ns = t0.elapsed().as_nanos() as u64;
        if ircost::DEPTH.with(|d| {
            d.set(d.get() - 1);
            d.get() == 0
        }) {
            ircost::OUTER_NS.fetch_add(ns, std::sync::atomic::Ordering::Relaxed);
        }
        ircost::site((arities.as_ptr() as usize, span.start, span.end), ns);
        ircost::record(ns);
    }
    match ircost_result {
        Ok((closure, loops, escapes, escape_site)) => {
            explain::record(
                interp,
                explain::FnExplain {
                    name: name.cloned(),
                    tier: explain::FnTier::Compiled { loops, escapes },
                },
            );
            (Some(closure), escape_site)
        }
        Err(bail) => {
            let reason = bail.reason.into_owned();
            let (site, first) =
                interp.lens_site_for(crate::lens::SiteKind::TierBail, name, span);
            if first {
                crate::lens::set_site_reason(site, reason.clone());
            }
            // H1: only render the preview when MOVA_EXPLAIN is on -- a
            // `pr_str` walk of the fn body on every bail would tax the hot
            // path (the ~6000 syntax-quote anon fns recompiled per
            // instance are exactly this case).
            let preview = explain::explain_enabled().then(|| explain::fn_preview(arities));
            explain::record(
                interp,
                explain::FnExplain {
                    name: name.cloned(),
                    tier: explain::FnTier::TreeWalk {
                        reason,
                        span: bail.span,
                        source_id: interp.source_id,
                        preview,
                    },
                },
            );
            (None, site)
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::eval::Interp;
    use crate::value::Value;

    /// Evaluates `src` (whose LAST form must produce a fn) and reports
    /// whether that fn got compiled.
    fn compiles(src: &str) -> bool {
        let mut interp = Interp::new();
        // Lazy tier-up (v0.6): this helper asserts a DEF-TIME outcome, with
        // the fn never called -- force eager compilation so the assertion
        // still means what it always meant. See `Interp::set_eager_compile`.
        interp.set_eager_compile(true);
        let v = interp
            .eval_str("compile-test", src)
            .unwrap_or_else(|e| panic!("{src}: {}", e.message));
        // `def`/`defmacro` return the var, as in Clojure.
        let v = match v {
            Value::Var(cell) => cell.get().expect("def'd var is bound"),
            other => other,
        };
        match v {
            Value::Fn(rc) | Value::Macro(rc) => rc.compiled.compiled().is_some(),
            other => panic!("{src}: expected a fn, got {}", other.type_name()),
        }
    }

    /// The compile column of COMPILE-TIER-DESIGN.md's matrix. Every one of
    /// these must be `compiled: Some` or the tier is silently doing nothing
    /// at all -- which no differential test can detect, since falling back
    /// is always *correct*.
    #[test]
    fn compiles_its_documented_surface() {
        if super::disabled_by_env() {
            // `MOVA_NO_COMPILE=1 cargo test` is a supported mode (it
            // proves the whole suite still passes with the tier off); this
            // one test asserts the tier IS on, so it has nothing to say
            // there.
            return;
        }
        for src in [
            "(fn [x] (+ x 1))",                          // params, global call
            "(fn [] 42)",                                // constant
            "(fn [] ())",                                // empty list self-evaluates
            "(fn f [n] (if (= n 0) 1 (* n (f (dec n)))))", // self-reference + if
            "(fn ([a] a) ([a b] b) ([a b & r] r))",      // multi-arity + variadic
            "(fn [n] (loop [i 0 acc n] (if (= i 3) acc (recur (inc i) (inc acc)))))",
            "(fn [x] (let [a (inc x) b (* a 2)] (do a b)))",
            "(fn [x] (when x [x {:k x} #{x}]))", // macro expansion + literals
            "(fn [] (quote (a b)))",
            "(fn [] (throw :boom))",
            // H2: quasiquote no longer bails the whole fn -- it escapes per
            // node (see `compile_special`'s "quasiquote" arm), so this now
            // compiles just like a `.`/`new` interop form does.
            "(fn [x] `(a ~x))",
            "(fn [x] (some.ns/thing x))", // ns-qualified -> alias/bare chain
            // S2.5: a closure created under live tree-walked frames, whose
            // free symbols are a frame binding AND a global. Before S2.5
            // the global dragged the whole fn back to the tree-walker.
            "(let [n 1] (fn [] n))",
            "(let [n 1] (fn [] (inc n)))",
            "(let [n 1] (fn [] (not-defined-anywhere n)))",
            "(let [n 1] (fn [] (some.ns/thing n)))",
            // S4: destructuring at every binding site ...
            "(fn [x] (let [[a b & r :as v] x] [a b r v]))",
            "(fn [x] (let [{:keys [a b] :or {a 1} :as m} x] [a b m]))",
            "(fn [x] (let [{:strs [a] c :k [d] :v} x] [a c d]))",
            "(fn [[a b] {:keys [c]}] [a b c])",
            "(fn [& {:keys [a b]}] [a b])",
            "(fn [x] (loop [[a & r] x acc 0] (if a (recur r (+ acc a)) acc)))",
            // ... nested fns (MakeClosure) ...
            "(fn [] (fn [] 1))",
            "(fn [x] (fn [y] (fn [] (+ x y))))",
            "(fn f [x] (fn [] (f x)))",
            "(fn [x] (map (fn [y] (+ x y)) [1 2 3]))",
            "(let [n 1] (fn [] (fn [] (inc n))))",
            // ... try/catch/finally ...
            "(fn [] (try 1 (catch e e)))",
            "(fn [] (try 1 (catch e e) (finally 2)))",
            "(fn [] (try 1 (finally 2)))",
            // ... and def in a body.
            "(fn [x] (def captured x))",
            "(fn [] (def declared))",
            // field3/W-PARSE: an IMMEDIATE read of a poisoned name that
            // SHADOWS an enclosing compiled fn's binding is a capture, not a
            // bail. This first one is what `core.mova`'s `for` macro expands
            // to (one gensym for the iterator fn's param AND its loop
            // binding), so before W-PARSE every `for` in the language
            // tree-walked.
            "(fn [s] (lazy-seq (loop [s s] (when (seq s) (first s)))))",
            // The same class hand-written, one fn boundary in: test.check's
            // `sized` shadows the enclosing fn's param in a `let` whose init
            // reads that very param.
            "(fn [g] (fn [x] (let [g (g x)] g)))",
            // The control: rename the inner binding and it always compiled.
            "(fn [s] (lazy-seq (loop [t s] (when (seq t) (first t)))))",
            // fix/closure-env-cycles: the `letfn` shape is a RECURSIVE
            // BINDING GROUP (`Ir::MakeRecGroup`), not a bail. Both of these
            // used to be in the fallback list below, as the poison rule's
            // deferred arm; the group is what a compiled scope can now
            // defer to. `tests/rec_group_test.rs` pins the node shape and
            // the semantics, `tests/leak_cycle_probe.rs` the leak.
            "(fn [] (letfn [(ev? [n] (od? n)) (od? [n] (ev? n))] (ev? 1)))",
            // ... and the macro that motivated the whole wave.
            "(fn [n] (for [x (range n)] (* x x)))",
            "(fn [n] (for [x (range n) :let [y (* x 2)] :when (odd? x)] y))",
            "(fn [n] (for [x (range n) y (range x)] [x y]))",
            // lsp/setf: `set!` on a deftype's own mutable field compiles
            // to `Ir::SetMutField` when the `wrap_fields_let` paired-local
            // shape is present -- the field's own local plus its hidden
            // `__mutfield_owner_<name>` marker, both plain slots (a param
            // and a `let` binding here, standing in for what
            // `wrap_fields_let` actually generates for a deftype method).
            "(fn [this f] (let [__mutfield_owner_f this] (set! f 1)))",
            // ... and from inside nested `let`/`loop`/`if` -- same FnCtx,
            // no nested-`fn` boundary crossed, which is exactly what
            // `IndexingPushbackReader.read-char`'s `when`/`let` nesting is.
            "(fn [this f] (let [__mutfield_owner_f this] (loop [i 0] (when (< i 3) (let [_ 1] (set! f i)) (recur (inc i))))))",
        ] {
            assert!(compiles(src), "expected the tier to compile: {src}");
        }
    }

    /// The fallback column. These are not *failures* -- each is a
    /// deliberate "later stage" -- but each must keep falling back rather
    /// than compiling something half-understood.
    #[test]
    fn falls_back_where_documented() {
        for src in [
            "(fn [x] (macroexpand-1 x))", // macroexpand in body
            "(fn [x] (recur x x))",       // recur arity mismatch: runtime error
            "(fn [x] (if x))",            // malformed special form: runtime error
            "(fn [x] (let [[a 5] x] a))", // malformed pattern: runtime error
            "(fn [x] (try 1 (catch 5 x)))", // malformed catch: runtime error
            "(fn [x] (def 5 x))",         // malformed def: runtime error
            // The poison rule's deferred arm, on a shape that is NOT a
            // recursive binding group: `g` is a lone `fn` binding (a run of
            // one), so the sibling reference has no group to resolve
            // through and still bails.
            "(fn [] (let [f (fn [] (g)) x 5 g (fn [] 1)] (f)))",
            // lsp/setf: a plain local with no `__mutfield_owner_<name>`
            // sibling is not the deftype-mutable-field shape -- still
            // bails, same message as before this task.
            "(fn [x] (set! x 1))",
            // ... a Var target, unaffected by this task at all.
            "(def g 1) (fn [] (set! g 2))",
            // ... and a field-shaped local whose owner marker crosses a
            // nested-`fn` boundary: `FnCtx::lookup` never sees it (it
            // never crosses that boundary), so this still bails to the
            // tree-walker rather than mutating the wrong frame.
            "(fn [this f] (let [__mutfield_owner_f this] (fn [] (set! f 1))))",
        ] {
            assert!(!compiles(src), "expected the tier to fall back: {src}");
        }
    }

    /// lsp/letfix: a let/loop rebind after a closure no longer bails -- the
    /// tree-walker opens a new frame there, matching by-value capture.
    #[test]
    fn let_rebind_after_closure_compiles() {
        for src in [
            "(fn [a] (let [g (fn [] a) a 2] (g)))",
            "(fn [] (let [a 1 g (fn [] a) a 2] (g)))",
            "(fn [] (let [a 1 g (fn [] (let [a a] a)) a 2] [(g) a]))",
            "(fn [] (loop [a 1 g (fn [] a) a 2] (g)))",
        ] {
            assert!(compiles(src), "expected the tier to compile: {src}");
        }
    }

    /// Falling back to `CallGlobal` is always *correct*, so no differential
    /// test can tell whether an intrinsic was ever emitted (S3). Assert the
    /// node shape directly: `(fn [x] <form>)`'s single body node.
    fn body_is_intrinsic(src: &str) -> bool {
        let mut interp = Interp::new();
        // Lazy tier-up: force the def-time answer, see `compiles`'s doc.
        interp.set_eager_compile(true);
        let v = interp
            .eval_str("compile-test", src)
            .unwrap_or_else(|e| panic!("{src}: {}", e.message));
        let Value::Fn(rc) = v else {
            panic!("{src}: expected a fn")
        };
        let cc = rc.compiled.compiled().unwrap_or_else(|| panic!("{src}: not compiled"));
        matches!(cc.code.arities[0].body[0], super::ir::Ir::Intrinsic { .. })
    }

    #[test]
    fn pristine_builtin_calls_compile_to_intrinsics() {
        if super::disabled_by_env() {
            return;
        }
        for src in [
            "(fn [x] (+ x 1))",
            "(fn [x] (+ x x x))", // n-ary: still ONE node
            "(fn [x] (- x 1))",
            "(fn [x] (* x 2))",
            "(fn [x] (/ x 2))",
            "(fn [x] (inc x))",
            "(fn [x] (dec x))",
            "(fn [x] (< x 1))",
            "(fn [x] (<= x 1))",
            "(fn [x] (> x 1))",
            "(fn [x] (>= x 1))",
            "(fn [x] (= x 1))",
            "(fn [x] (zero? x))",
            "(fn [x] (not x))",
        ] {
            assert!(body_is_intrinsic(src), "expected an intrinsic for: {src}");
        }
        for src in [
            "(fn [x] (+))",        // 0-ary identity: the native's job
            "(fn [x] (+ x))",      // 1-ary
            "(fn [x] (- x))",      // unary negation is a different shape
            "(fn [x] (/ x))",      // ... as is unary reciprocal
            "(fn [x] (< 1 x 3))",  // chained compare: no short-circuit here
            "(fn [x] (= 1 x 3))",
            "(fn [x] (inc x x))",  // arity error: the native must raise it
            "(fn [x] (max x 1))",  // not an intrinsic at all
            // A redefined builtin is not pristine, so no node is emitted in
            // the first place (the run-time guard covers the other order).
            "(do (def + str) (fn [x] (+ x 1)))",
        ] {
            assert!(!body_is_intrinsic(src), "expected NO intrinsic for: {src}");
        }
    }

    /// Same problem as `body_is_intrinsic`: declining to specialize a loop
    /// is always *correct*, so no differential test can tell whether
    /// `Ir::NumLoop` was ever built. The node shape is therefore asserted
    /// directly -- otherwise the differential coverage in
    /// `tests/differential_test.rs` could all be running the generic loop
    /// and passing for the wrong reason.
    /// Whether `src`'s compiled fn contains an `Ir::NumLoop` ANYWHERE --
    /// nested `fn` templates and `:or` defaults included -- rather than only
    /// at `body[0]`. The whole-tree walk is what lets the matrix below state
    /// a loop's shape directly (`(let [m k] (loop ..))`, `(fn [] (loop ..))`)
    /// instead of contorting every case into a fn whose first body node is
    /// the loop.
    fn body_is_num_loop(src: &str) -> bool {
        has_num_loop(&mut Interp::new(), src)
    }

    /// Compiles `src` (whose last form must be a fn) in `interp` and reports
    /// whether the resulting code contains an `Ir::NumLoop`.
    fn has_num_loop(interp: &mut Interp, src: &str) -> bool {
        // Lazy tier-up: force the def-time answer, see `compiles`'s doc.
        interp.set_eager_compile(true);
        let v = interp
            .eval_str("compile-test", src)
            .unwrap_or_else(|e| panic!("{src}: {}", e.message));
        let Value::Fn(rc) = v else {
            panic!("{src}: expected a fn")
        };
        let cc = rc.compiled.compiled().unwrap_or_else(|| panic!("{src}: not compiled"));
        cc.code
            .arities
            .iter()
            .any(|a| a.body.iter().any(contains_num_loop))
    }

    /// Every `Ir` variant is listed, so a new node that can hold a `loop`
    /// fails to compile here until it is classified -- the same discipline
    /// `exec_intrinsic`'s match uses.
    fn contains_num_loop(ir: &super::ir::Ir) -> bool {
        use super::ir::Ir::*;
        fn any(irs: &[super::ir::Ir]) -> bool {
            irs.iter().any(contains_num_loop)
        }
        fn binds(bs: &[(super::ir::CompiledPattern, super::ir::Ir)]) -> bool {
            bs.iter().any(|(p, i)| contains_num_loop(i) || in_pattern(p))
        }
        // A `:or` default is an ordinary expression and can hold a loop.
        fn in_pattern(p: &super::ir::CompiledPattern) -> bool {
            use super::ir::{CompiledPattern as P, SeqStep};
            match p {
                P::Slot(_) => false,
                P::Seq(steps) => steps.iter().any(|s| match s {
                    SeqStep::Elem(q) | SeqStep::Rest(q) | SeqStep::As(q) => in_pattern(q),
                }),
                P::Map(m) => {
                    m.entries.iter().any(|e| {
                        in_pattern(&e.target)
                            || e.default.as_ref().is_some_and(contains_num_loop)
                    }) || m.as_pat.as_ref().is_some_and(in_pattern)
                }
            }
        }
        match ir {
            NumLoop(_) => true,
            New(n) => n.args.iter().any(contains_num_loop),
            // field3/W-RESOLVE: `Escape` carries a source `Form`, not
            // `Ir` -- no compiled subtree, so no `NumLoop` under it.
            Const(_) | LoadSlot(_) | LoadSlotTake(_) | LoadCapture(_) | SelfRef
            | SiblingRef(_) | GlobalRef { .. } | CreationEnvLookup { .. } | Escape(_) => false,
            // W-FIELDGET: likewise. Its fallback is the very `Escape` it was
            // built from, and its receiver is a bare frame read -- there is
            // no compiled subtree under it either.
            FieldGet(_) => false,
            If { test, then, els } => {
                contains_num_loop(test)
                    || contains_num_loop(then)
                    || els.as_deref().is_some_and(contains_num_loop)
            }
            Do(irs) | VectorLit(irs) | SetLit(irs) => any(irs),
            Let { binds: bs, body } | Loop { binds: bs, body, .. } => binds(bs) || any(body),
            Recur { args, .. } => any(args),
            Call { callee, args, .. } => contains_num_loop(callee) || any(args),
            CallGlobal { args, .. } | CallCreationEnv { args, .. } | Intrinsic { args, .. } => {
                any(args)
            }
            MapLit(kvs) => kvs
                .iter()
                .any(|(k, v)| contains_num_loop(k) || contains_num_loop(v)),
            Throw { value, .. } => contains_num_loop(value),
            SetMutField { value, .. } => contains_num_loop(value),
            MakeClosure { template, .. } => template
                .code
                .arities
                .iter()
                .any(|a| any(&a.body)),
            MakeRecGroup { members, .. } => members
                .iter()
                .any(|m| m.template.code.arities.iter().any(|a| any(&a.body))),
            Try {
                body,
                catches,
                finally,
            } => {
                any(body)
                    || catches.iter().any(|arm| any(&arm.body))
                    || finally.as_ref().is_some_and(|b| any(b))
            }
            Def { value, .. } => value.as_deref().is_some_and(contains_num_loop),
            DynBind(d) => d.pairs.iter().any(|(_, _, i)| contains_num_loop(i)) || any(&d.body),
        }
    }

    const LCG: &str = "(fn [seed] \
                         (loop [i 0 acc seed] \
                           (if (< i 2000) \
                             (recur (inc i) (+ (* acc 6364136223846793005) 1442695040888963407)) \
                             acc)))";

    /// Every loop shape inside `ir::NumLoop`'s stated grammar. Each must
    /// specialize; a regression here is the specialization silently doing
    /// nothing, which nothing else can detect.
    const MUST_SPECIALIZE: &[&str] = &[
        LCG,
        // All five comparisons in test position, `recur` in each branch.
        "(fn [] (loop [i 0] (if (< i 10) (recur (inc i)) i)))",
        "(fn [] (loop [i 10] (if (> i 0) (recur (dec i)) i)))",
        "(fn [] (loop [i 0] (if (>= i 5) i (recur (inc i)))))",
        "(fn [] (loop [i 0] (if (<= i 5) (recur (inc i)) i)))",
        "(fn [] (loop [i 0] (if (= i 5) i (recur (inc i)))))",
        // Seeds: an int const, a float const, a param of either type.
        "(fn [n] (loop [i n] (if (< i 5) (recur (inc i)) i)))",
        "(fn [] (loop [i 0.5] (if (< i 5.0) (recur (+ i 1.0)) i)))",
        // Loop-invariant reads: an enclosing slot, and a capture.
        "(fn [k] (loop [i 0] (if (< i k) (recur (inc i)) i)))",
        "(fn [k] (let [m k] (loop [i 0] (if (< i m) (recur (inc i)) i))))",
        "(fn [k] (fn [] (loop [i 0] (if (< i k) (recur (inc i)) i))))",
        // Every arithmetic op the grammar admits, n-ary folds included.
        "(fn [] (loop [i 0 a 1.5] (if (< i 3) (recur (inc i) (- a 0.5)) a)))",
        "(fn [] (loop [i 0 a 0] (if (< i 3) (recur (inc i) (+ a 1 2 3)) a)))",
        "(fn [] (loop [i 0 a 1] (if (< i 3) (recur (inc i) (* a 2 3 4)) a)))",
        "(fn [] (loop [i 0 a 1] (if (< i 3) (recur (dec i) (* a 2)) (- a 1))))",
        // Simultaneous rebinding, and the 3+-binding staging path.
        "(fn [] (loop [i 0 a 1 b 2] (if (< i 3) (recur (inc i) b a) (- a b))))",
        // Exactly NUM_MAX_BINDS bindings -- the accepted side of the
        // threshold (the rejected side is in MUST_NOT_SPECIALIZE).
        "(fn [] (loop [a 0 b 0 c 0 d 0 e 0 f 0 g 0 h 0] \
                  (if (< a 5) (recur (inc a) b c d e f g h) (+ a b c d e f g h))))",
        // A shadowing rebind whose second init is a plain constant: each
        // `a` gets its own slot, so this is still all-scalar.
        "(fn [] (loop [a 0 a 1] (if (< a 5) (recur a (inc a)) a)))",
        // --- W-NUMLOOP: nil-terminal exits -------------------------------
        // A 2-arg `if`: the missing else IS the nil branch. This is the
        // `when`-shaped loop the widening exists for.
        "(fn [n] (loop [i 0] (if (< i n) (recur (inc i)))))",
        // ... and the same loop written with `when`, which macroexpands to
        // `(if <test> (do <body>))` -- so this one also pins the one-form
        // `do` unwrap the expansion needs.
        "(fn [n] (loop [i 0] (when (< i n) (recur (inc i)))))",
        // An EXPLICIT `nil` else, in both branch positions.
        "(fn [n] (loop [i 0] (if (< i n) (recur (inc i)) nil)))",
        "(fn [n] (loop [i 0] (if (>= i n) nil (recur (inc i)))))",
        // Two bindings with a nil exit: the accumulating shape whose result
        // is discarded (the client's measured cliff).
        "(fn [n] (loop [i 0 acc 0] (if (< i n) (recur (inc i) (+ acc i)) nil)))",
        // A Float-lane loop with a nil exit.
        "(fn [] (loop [i 0.5] (when (< i 5.0) (recur (+ i 1.0)))))",
        // `dotimes` with an EMPTY body: `(do (recur (inc i)))`, i.e. the
        // one-form `do` again, under the missing else again.
        "(fn [n] (dotimes [i n]))",
        // An empty `do` in else position is exactly `nil`.
        "(fn [n] (loop [i 0] (if (< i n) (recur (inc i)) (do))))",
        // A one-form `do` wrapping the `recur`, with a NUMERIC else -- the
        // `do` rule is independent of the nil rule. (This used to be in the
        // MUST_NOT list, as "nor is a `do`"; the one-form case is now a
        // shape identity, see `ir::NumLoop`'s grammar.)
        "(fn [] (loop [i 0] (if (< i 5) (do (recur (inc i))) i)))",
        // A nested loop specializes on its own terms. `recur` always names
        // the INNERMOST target, so this one belongs to the inner loop, and
        // the outer loop's `n` is genuinely invariant for the inner loop's
        // whole run (nothing a NumLoop executes can write a frame slot).
        // The OUTER loop stays generic -- its branch is a whole `Loop` node
        // -- which is why this entry proves the inner one, not both.
        "(fn [] (loop [n 0] (if (< n 3) (recur (loop [k n] (if (< k 9) (recur (inc k)) k))) n)))",
    ];

    /// Whether `src`'s compiled fn contains an `Ir::FieldGet` anywhere in
    /// any arity body. Same problem `body_is_num_loop` exists for: declining
    /// to specialize a `.-field` read is always *correct* (the escape still
    /// answers), so no differential test can tell whether the node was ever
    /// built, and the differential coverage in `tests/differential_test.rs`
    /// could all be passing for the wrong reason.
    ///
    /// The walk covers the node kinds the matrix below actually nests a
    /// field read inside (`let`, `do`, `if`, a loop body, a call argument).
    /// It does not need `contains_num_loop`'s exhaustiveness: a MISSED node
    /// makes a positive case fail, never a negative case pass.
    fn body_is_field_get(src: &str) -> bool {
        fn has(ir: &super::ir::Ir) -> bool {
            use super::ir::Ir::*;
            match ir {
                FieldGet(_) => true,
                If { test, then, els } => {
                    has(test) || has(then) || els.as_deref().is_some_and(has)
                }
                Do(irs) | VectorLit(irs) | SetLit(irs) => irs.iter().any(has),
                Let { binds, body } | Loop { binds, body, .. } => {
                    binds.iter().any(|(_, i)| has(i)) || body.iter().any(has)
                }
                Recur { args, .. }
                | CallGlobal { args, .. }
                | CallCreationEnv { args, .. }
                | Intrinsic { args, .. } => args.iter().any(has),
                Call { callee, args, .. } => has(callee) || args.iter().any(has),
                Throw { value, .. } => has(value),
                Def { value, .. } => value.as_deref().is_some_and(has),
                _ => false,
            }
        }
        let mut interp = Interp::new();
        // Lazy tier-up: force the def-time answer, see `compiles`'s doc.
        interp.set_eager_compile(true);
        let v = interp
            .eval_str("compile-test", src)
            .unwrap_or_else(|e| panic!("{src}: {}", e.message));
        let Value::Fn(rc) = v else {
            panic!("{src}: expected a fn")
        };
        let cc = rc
            .compiled
            .compiled()
            .unwrap_or_else(|| panic!("{src}: not compiled"));
        cc.code.arities.iter().any(|a| a.body.iter().any(has))
    }

    /// W-FIELDGET: the shapes that must reach `Ir::FieldGet`, and the ones
    /// that must stay an ordinary `Ir::Escape`.
    #[test]
    fn field_reads_compile_to_the_fieldget_specialization() {
        if super::disabled_by_env() || super::resolve::field_get_disabled_by_env() {
            return;
        }
        let types = "(deftype RoseTree [root children]) (defrecord Gen [gen]) ";
        for src in [
            // The shape the node exists for, hinted and unhinted, on a
            // param slot and on a capture.
            "(fn [^RoseTree r] (.-root r))",
            "(fn [r] (.-root r))",
            "(fn [r] (.-children r))",
            "(fn [^Gen g] (.-gen g))",
            // A `let`-bound local is a slot too.
            "(fn [x] (let [r x] (.-root r)))",
        ] {
            let src = &format!("{types}{src}");
            assert!(body_is_field_get(src), "expected a FieldGet for: {src}");
        }
        for src in [
            // `.method`, not `.-field`: one-argument method dispatch has a
            // field-access half, but reaching it means reproducing
            // `eval_dot_form`'s universal-Object-method interception.
            "(fn [r] (.root r))",
            // Not a bare local receiver -- re-running the fallback would
            // re-evaluate it.
            "(fn [r] (.-root (identity r)))",
            "(fn [] (.-root (RoseTree. 1 2)))",
            // Wrong arity for a `.-` form: the tree-walker's error to raise.
            "(fn [r] (.-root r r))",
            // A constructor, not a field read (the named follow-up).
            "(fn [a b] (RoseTree. a b))",
            // An empty field name is not a field read.
            "(fn [r] (.- r))",
        ] {
            let src = &format!("{types}{src}");
            assert!(!body_is_field_get(src), "expected NO FieldGet for: {src}");
        }
    }

    /// The kill switch really does stop the node being emitted.
    #[test]
    fn fieldget_kill_switch_is_honoured() {
        if super::disabled_by_env() {
            return;
        }
        let src = "(deftype T [root]) (fn [^T r] (.-root r))";
        assert_eq!(
            body_is_field_get(src),
            !super::resolve::field_get_disabled_by_env(),
            "MOVA_NO_FIELDGET must decide whether the node is built"
        );
    }

    #[test]
    fn scalar_loops_compile_to_the_numeric_specialization() {
        if super::disabled_by_env() || super::resolve::num_loop_disabled_by_env() {
            // Both kill switches are supported modes; this test asserts the
            // specialization IS on, so it has nothing to say under either.
            return;
        }
        for src in MUST_SPECIALIZE {
            assert!(body_is_num_loop(src), "expected a NumLoop for: {src}");
        }
        for src in [
            // --- rejected operators -------------------------------------
            // `/` can fail (Int/0) and its result type depends on the
            // VALUES: a deliberate non-goal, not an oversight.
            "(fn [] (loop [i 1.0] (if (< i 10.0) (recur (/ i 2.0)) i)))",
            "(fn [] (loop [i 8] (if (< i 5) (recur (/ i 2)) i)))",
            // Ops that produce a Bool, in operand position.
            "(fn [] (loop [i 0] (if (< i 5) (recur (zero? i)) i)))",
            "(fn [] (loop [i 0] (if (< i 5) (recur (not i)) i)))",
            "(fn [] (loop [i 0] (if (< i 5) (recur (= i 1)) i)))",
            "(fn [] (loop [i 0] (if (< i 5) (recur (< i 1)) i)))",
            // Numeric builtins that are not intrinsics at all.
            "(fn [] (loop [i 0] (if (< i 5) (recur (max i 1)) i)))",
            "(fn [] (loop [i 0] (if (< i 5) (recur (mod i 3)) i)))",
            "(fn [] (loop [i 0] (if (< i 5) (recur (bit-and i 3)) i)))",
            // --- rejected test positions --------------------------------
            "(fn [] (loop [i 0] (if (zero? i) i (recur (dec i)))))",
            "(fn [] (loop [i 0] (if (not i) i (recur (dec i)))))",
            "(fn [] (loop [i 5] (if i (recur (dec i)) i)))",
            // A CHAINED comparison is not a 2-arg intrinsic: the native
            // short-circuits, so it keeps its own node.
            "(fn [] (loop [i 0] (if (< 0 i 5) (recur (inc i)) i)))",
            // --- rejected loop shapes -----------------------------------
            // A call, a non-numeric result, a non-scalar binding, a
            // non-constant seed, and a body that is not a single `if`.
            "(fn [f] (loop [i 0] (if (< i 5) (recur (f i)) i)))",
            "(fn [] (loop [i 0] (if (< i 5) (recur (inc i)) [i])))",
            "(fn [x] (loop [[a b] x i 0] (if (< i 5) (recur x (inc i)) a)))",
            "(fn [x] (loop [i (count x)] (if (< i 5) (recur (inc i)) i)))",
            "(fn [] (loop [i 0] (if (< i 5) (recur (inc i)) i) 7))",
            "(fn [] (loop [] (if (< 1 5) (recur) 7)))", // no bindings
            // A nested `if` in a branch: the arm is neither a `recur`, nor
            // `nil`, nor a scalar expression.
            "(fn [] (loop [i 0] (if (< i 9) (if (< i 5) (recur (inc i)) (recur (+ i 2))) i)))",
            // ... nor is a `let`.
            "(fn [] (loop [i 0] (if (< i 5) (let [j 1] (recur (+ i j))) i)))",
            // W-NUMLOOP: a `do` of TWO OR MORE forms stays out. The leading
            // forms run for effect, and effects between the test and the
            // `recur` are exactly what invariant I2 forbids -- which is why
            // `dotimes` over a NON-empty body does not specialize.
            "(fn [] (loop [i 0] (if (< i 5) (do 1 (recur (inc i))) i)))",
            "(fn [n] (dotimes [i n] (+ i 1)))",
            // Both branches nil is not a loop at all (no `recur` anywhere),
            // so the "at least one branch iterates" rule still refuses it.
            "(fn [n] (loop [i 0] (when (< i n))))",
            "(fn [n] (loop [i 0] (if (< i n) nil nil)))",
            // A NON-TAIL `recur`: the pending `(+ 1 _)` must be abandoned
            // when it unwinds, which the register machine has no way to say.
            "(fn [] (loop [i 0] (if (< i 5) (+ 1 (recur (inc i))) i)))",
            "(fn [] (loop [i 0] (if (< (+ 1 (recur (inc i))) 5) i i)))",
            // Nested loops. `recur` always names the INNERMOST target, so a
            // recur that escapes this loop can only come from a nested
            // loop's own INIT (`eval_loop` evaluates inits before
            // trampolining, so that one targets the enclosing loop). Here
            // neither loop specializes: the inner one's seed is a `recur`,
            // and the outer one's branch is a whole `Loop` node.
            "(fn [] (loop [n 0] (if (< n 3) (loop [k (recur (inc n))] k) n)))",
            // Same for a nested loop the matcher rejects for its own
            // reasons (a call in its seed): rejecting it also costs the
            // outer loop, whose branch is then an unrepresentable node.
            "(fn [x] (loop [n 0] (if (< n 3) (loop [k (count x)] (if (< k 2) (recur (inc k)) k)) n)))",
            // One binding too many (NUM_MAX_BINDS + 1): a REJECTION, never
            // a truncation.
            "(fn [] (loop [a 0 b 0 c 0 d 0 e 0 f 0 g 0 h 0 j 0] \
                      (if (< a 5) (recur (inc a) b c d e f g h j) a)))",
            // More registers than the file holds (bindings + one per
            // distinct constant + temporaries > NUM_REGS).
            "(fn [] (loop [i 0] \
                      (if (< i 5) (recur (+ i 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16)) i)))",
            // A second init that reads the slot the first one just wrote --
            // only the generic loop does that. Same rule catches a
            // shadowing rebind that reads the binding it shadows.
            "(fn [] (loop [i 0 j i] (if (< i 5) (recur (inc i) j) j)))",
            "(fn [] (loop [a 0 a (inc a)] (if (< a 5) (recur a (inc a)) a)))",
            // A non-numeric constant, in operand and in operator position.
            "(fn [] (loop [i 0] (if (< i 5) (recur (+ i :x)) i)))",
            "(fn [] (loop [i :x] (if (< 1 5) (recur i) i)))",
            "(fn [] (loop [i 0] (if (< i 5) (recur (1 i)) i)))",
            "(fn [] (loop [i 0] (if (< i 5) (recur (\"s\" i)) i)))",
            // A free symbol of a closure created under LIVE tree-walked
            // frames is an `Ir::CreationEnvLookup`, re-probed on every
            // access against an env this loop does not own -- not something
            // it may hoist to loop entry.
            "(let [n 5] (fn [] (loop [i 0] (if (< i n) (recur (inc i)) i))))",
            // A redefined operator: no intrinsic is emitted, so no scalar
            // op list can be built either. (The other order -- redefined
            // AFTER compiling -- is the run-time guard, pinned in
            // tests/differential_test.rs.)
            "(do (def + str) (fn [] (loop [i 0] (if (< i 5) (recur (+ i 1)) i))))",
            "(do (def < str) (fn [] (loop [i 0] (if (< i 5) (recur (inc i)) i))))",
            "(do (def inc str) (fn [] (loop [i 0] (if (< i 5) (recur (inc i)) i))))",
            "(do (def = str) (fn [] (loop [i 0] (if (= i 5) i (recur (inc i))))))",
        ] {
            assert!(!body_is_num_loop(src), "expected NO NumLoop for: {src}");
        }
    }

    /// The other half of the kill switch: under `MOVA_NO_NUMLOOP=1` the
    /// node must never be emitted, INCLUDING for the shapes that otherwise
    /// always specialize. Without this, "green under the kill switch" would
    /// only prove the suite passes, not that the switch does anything.
    #[test]
    fn the_numloop_kill_switch_suppresses_every_specialization() {
        if super::disabled_by_env() || !super::resolve::num_loop_disabled_by_env() {
            return;
        }
        for src in MUST_SPECIALIZE {
            assert!(!body_is_num_loop(src), "kill switch ignored for: {src}");
        }
    }

    /// The per-interpreter sibling of the same switch, which -- unlike the
    /// env var -- can be exercised in an ordinary test run.
    #[test]
    fn the_per_interpreter_numloop_switch_leaves_every_loop_generic() {
        if super::disabled_by_env() {
            return;
        }
        for src in MUST_SPECIALIZE {
            let mut interp = Interp::with_tiers(true, false);
            assert!(
                !has_num_loop(&mut interp, src),
                "per-interpreter switch ignored for: {src}"
            );
        }
    }

    #[test]
    fn macros_are_never_compiled() {
        assert!(!compiles("(defmacro m [x] x)"));
    }

    #[test]
    fn the_per_interpreter_switch_disables_the_tier() {
        let mut interp = Interp::with_compile_enabled(false);
        let v = interp.eval_str("compile-test", "(fn [x] (+ x 1))").unwrap();
        match v {
            Value::Fn(rc) => assert!(rc.compiled.compiled().is_none()),
            _ => panic!("expected a fn"),
        }
    }

    // -------------------------------------------------------------------
    // field1/W-EXPLAIN: tier-decision observability
    // -------------------------------------------------------------------

    use crate::value::{Keyword, PMap};

    fn get_kw<'a>(m: &'a PMap, key: &str) -> &'a Value {
        m.get(&Value::Keyword(Keyword::from(key)))
            .unwrap_or_else(|| panic!("expected map to have key :{key}"))
    }

    fn as_kw_name(v: &Value) -> &str {
        match v {
            Value::Keyword(s) => s.as_ref(),
            other => panic!("expected a keyword, got {}", other.type_name()),
        }
    }

    fn as_str(v: &Value) -> &str {
        match v {
            Value::Str(s) => s.as_ref(),
            other => panic!("expected a string, got {}", other.type_name()),
        }
    }

    fn as_bool(v: &Value) -> bool {
        match v {
            Value::Bool(b) => *b,
            other => panic!("expected a bool, got {}", other.type_name()),
        }
    }

    /// `def_src` prefixed with two throwaway lines and three leading spaces
    /// -- so the `(defn ...)` form itself starts at line 3, column 4,
    /// rather than line 1 column 1. `def_src` is always a one-liner in
    /// every caller below, and macroexpansion (`defn`'s own macro) stamps
    /// every form it synthesizes with the ORIGINAL macro CALL's span (see
    /// `eval::special_forms::publish_var_meta`'s doc for the same rule) --
    /// so this is what makes the `:at` position DISTINGUISHABLE from the
    /// trivial `1:1` a bare one-liner buffer would otherwise always report,
    /// regardless of which sub-form inside it actually bailed/declined.
    const DEF_PAD: &str = "\n;; padding so line:col is distinguishable, not always 1:1\n   ";

    /// `defn`s `def_src` (padded via [`DEF_PAD`], which must bind `name`)
    /// in a `"compile-test"` buffer, then evaluates `(compile-explain
    /// name)` and returns the resulting map.
    ///
    /// field5/W-SPAN: the query eval_str below deliberately uses a
    /// DIFFERENT source_name AND a substantially different (longer,
    /// multi-line) buffer than `def_src`'s own `"compile-test"` -- this is
    /// exactly the stale-buffer scenario `explain::at`/`builtins::meta::
    /// at_string` used to get wrong: `Interp::source`/`source_name` are
    /// overwritten at every `eval_str` entry, so a span recorded against
    /// `def_src`'s buffer used to render against WHATEVER buffer was
    /// current at query time (here, the query buffer itself) instead of
    /// the one it was actually read from. Every `:at` assertion built on
    /// this helper is an EXACT `name:line:col` match, not a `starts_with`
    /// prefix check, so a regression -- rendering against the query
    /// buffer's identity OR position instead of `def_src`'s -- fails
    /// loudly.
    fn explain_of(interp: &mut Interp, def_src: &str, name: &str) -> PMap {
        interp
            .eval_str("compile-test", &format!("{DEF_PAD}{def_src}"))
            .unwrap_or_else(|e| panic!("{def_src}: {}", e.message));
        let v = interp
            .eval_str(
                "compile-explain-query",
                &format!("\n\n;; a differently-named, differently-shaped buffer\n(compile-explain {name})"),
            )
            .unwrap_or_else(|e| panic!("compile-explain {name}: {}", e.message));
        match v {
            Value::Map(m) => m,
            other => panic!("expected compile-explain to return a map, got {}", other.type_name()),
        }
    }

    /// A bailing fn (a `reify` -- a type-system form, which still lives
    /// only in `eval::types_forms`) yields a `:tree-walk` compile-explain
    /// record naming the bail and pointing at the form's own line -- the
    /// exact "silent cliff" this whole feature exists to turn into a
    /// five-minute fix.
    ///
    /// field3/W-RESOLVE: this test used to use `(.toString x)`, which no
    /// longer bails -- see
    /// `compile_explain_reports_interop_escape_not_a_bail` directly below,
    /// which pins the behaviour that replaced it.
    #[test]
    fn compile_explain_reports_tree_walk_reason_and_position() {
        let mut interp = Interp::new();
        let m = explain_of(&mut interp, "(defn reifier [x] (reify Object (toString [_] x)))", "reifier");
        assert_eq!(as_kw_name(get_kw(&m, "tier")), "tree-walk");
        let reason = as_str(get_kw(&m, "reason"));
        assert!(reason.contains("type system form"), "reason should name the type-system bail, got: {reason}");
        let at = as_str(get_kw(&m, "at"));
        // field5/W-SPAN regression check: EXACT match (not `starts_with`),
        // so a render against the LATER "compile-explain-query" buffer
        // (which `explain_of` deliberately makes current before this map
        // is even built) fails loudly instead of silently passing a
        // prefix check. `3:4` (not the `(reify ...)` sub-form's own
        // position) because `defn`'s macroexpansion stamps every
        // synthesized sub-form with the ORIGINAL macro CALL's span (see
        // `eval::special_forms::publish_var_meta`'s doc for the same
        // stamping rule) -- `DEF_PAD` puts that call on line 3, column 4.
        // Pre-fix, this would have rendered as
        // "compile-explain-query:1:1" (the LATER query buffer's own
        // identity and position) instead.
        assert_eq!(
            at, "compile-test:3:4",
            "expected the ORIGINAL def buffer's name/position, not the later compile-explain query buffer's, got: {at}"
        );
    }

    /// field3/W-RESOLVE: an interop CALL no longer bails the fn that holds
    /// it. It compiles to one `Ir::Escape`, and `compile-explain` says so
    /// -- `:tier :compiled` with a non-zero `:escapes`, which is the
    /// honesty condition on the node (a fn with escapes did not compile
    /// cleanly, and the record must not imply it did).
    #[test]
    fn compile_explain_reports_interop_escape_not_a_bail() {
        let mut interp = Interp::new();
        let m = explain_of(&mut interp, "(defn dotty [x] (.toString x))", "dotty");
        assert_eq!(as_kw_name(get_kw(&m, "tier")), "compiled");
        assert_eq!(get_kw(&m, "escapes"), &Value::Int(1));
        // Two interop forms in one fn are two escapes, not one -- the count
        // is per NODE, which is the whole point of the per-node fallback.
        let m2 = explain_of(
            &mut interp,
            "(defn dotty2 [x y] (.toString x) (.toString y))",
            "dotty2",
        );
        assert_eq!(get_kw(&m2, "escapes"), &Value::Int(2));
        // A fn with no interop at all still reports zero, so `:escapes` is
        // always present and always readable.
        let m3 = explain_of(&mut interp, "(defn clean [x] (+ x 1))", "clean");
        assert_eq!(get_kw(&m3, "escapes"), &Value::Int(0));
    }

    /// H2: `quasiquote` joins the interop-escape family -- the
    /// `clj_kondo.impl.utils/get-in` helper shape (`#(if (keyword? %) %
    /// `(get ~%))`) compiles with exactly one escape instead of bailing the
    /// whole fn, which used to be the single largest tree-walk bucket in a
    /// clj-kondo run (~6000 instances of this exact fn).
    #[test]
    fn quasiquote_compiles_as_an_escape() {
        let mut interp = Interp::new();
        let m = explain_of(
            &mut interp,
            "(defn qq [x] (if (keyword? x) x `(get ~x)))",
            "qq",
        );
        assert_eq!(as_kw_name(get_kw(&m, "tier")), "compiled");
        assert_eq!(get_kw(&m, "escapes"), &Value::Int(1));
    }

    /// The measured claim of the whole wave, as a compile-time assertion:
    /// the `delays.clj` worker shape -- two interop calls wrapped around a
    /// hot, interop-FREE numeric loop -- now COMPILES, escapes and all, and
    /// its loop still reaches the `NumLoop` specialization. Before
    /// `Ir::Escape` this fn tree-walked in its entirety, which is what made
    /// the assembled file cost 128.40s instead of 2.69s.
    #[test]
    fn the_delays_worker_shape_compiles_with_its_hot_loop_specialized() {
        if super::disabled_by_env() || super::resolve::num_loop_disabled_by_env() {
            return;
        }
        let mut interp = Interp::new();
        let m = explain_of(
            &mut interp,
            "(defn worker [] \
               (.toUpperCase \"a\") \
               (loop [i 0] (if (< i 10000) (recur (inc i)) i)) \
               (.toUpperCase \"b\"))",
            "worker",
        );
        assert_eq!(as_kw_name(get_kw(&m, "tier")), "compiled");
        assert_eq!(get_kw(&m, "escapes"), &Value::Int(2));
        let Value::Vector(loops) = get_kw(&m, "loops") else {
            panic!("expected :loops to be a vector");
        };
        assert_eq!(loops.len(), 1, "expected exactly one loop record");
        let Value::Map(l0) = loops.get(0).unwrap() else {
            panic!("expected a loop record map");
        };
        assert_eq!(
            get_kw(l0, "numloop"),
            &Value::Bool(true),
            "the interop-free loop inside a fn with escapes must still specialize"
        );
    }

    /// field3/W-RESOLVE, the refusal: a `recur` written INSIDE an interop
    /// form cannot be escaped (the tree-walker unwinds `recur` as an
    /// `RjError`, the compiled tier as `Flow::Recur`), so the whole fn
    /// bails exactly as it did before the escape node existed.
    #[test]
    fn interop_form_containing_recur_still_bails_the_whole_fn() {
        let mut interp = Interp::new();
        let m = explain_of(
            &mut interp,
            "(defn r [x] (loop [i 0] (if (< i 1) (.toString (recur (inc i))) x)))",
            "r",
        );
        assert_eq!(as_kw_name(get_kw(&m, "tier")), "tree-walk");
        let reason = as_str(get_kw(&m, "reason"));
        assert!(reason.contains("recur"), "reason should name the recur refusal, got: {reason}");
    }

    /// A clean scalar loop reports `:compiled` with a `:numloop true` loop
    /// entry.
    #[test]
    fn compile_explain_reports_specialized_numloop() {
        if super::disabled_by_env() || super::resolve::num_loop_disabled_by_env() {
            return;
        }
        let mut interp = Interp::new();
        let m = explain_of(
            &mut interp,
            "(defn count-up [n] (loop [i 0] (if (< i n) (recur (inc i)) i)))",
            "count-up",
        );
        assert_eq!(as_kw_name(get_kw(&m, "tier")), "compiled");
        let Value::Vector(loops) = get_kw(&m, "loops") else {
            panic!("expected :loops to be a vector");
        };
        assert_eq!(loops.len(), 1, "expected exactly one loop record");
        let Value::Map(loop0) = &loops[0] else {
            panic!("expected a loop record map");
        };
        assert!(as_bool(get_kw(loop0, "numloop")), "expected the loop to specialize");
        let at = as_str(get_kw(loop0, "at"));
        // field5/W-SPAN regression check: same "3:4 via macroexpansion
        // span-collapse (see `DEF_PAD`'s doc)" reasoning as
        // `compile_explain_reports_tree_walk_reason_and_position` above.
        assert_eq!(
            at, "compile-test:3:4",
            "expected the ORIGINAL def buffer's name/position, not the later compile-explain query buffer's, got: {at}"
        );
    }

    /// W-NUMLOOP: the nil-terminal (`when`-shaped) grammar also reports as
    /// specialized, not just the classic if/else shape.
    #[test]
    fn compile_explain_reports_specialized_numloop_for_when_shaped_loop() {
        if super::disabled_by_env() || super::resolve::num_loop_disabled_by_env() {
            return;
        }
        let mut interp = Interp::new();
        let m = explain_of(
            &mut interp,
            "(defn tick [n] (loop [i 0] (when (< i n) (recur (inc i)))))",
            "tick",
        );
        assert_eq!(as_kw_name(get_kw(&m, "tier")), "compiled");
        let Value::Vector(loops) = get_kw(&m, "loops") else {
            panic!("expected :loops to be a vector");
        };
        assert_eq!(loops.len(), 1);
        let Value::Map(loop0) = &loops[0] else {
            panic!("expected a loop record map");
        };
        assert!(
            as_bool(get_kw(loop0, "numloop")),
            "expected the when-shaped loop to specialize"
        );
    }

    /// A loop that stays generic (an unsupported op in the grammar) reports
    /// `:numloop false` with a reason, inside an otherwise `:compiled` fn.
    #[test]
    fn compile_explain_reports_generic_loop_with_reason() {
        if super::disabled_by_env() {
            return;
        }
        let mut interp = Interp::new();
        let m = explain_of(
            &mut interp,
            "(defn top-loop [n] (loop [i 0] (if (< i n) (recur (max i 1)) i)))",
            "top-loop",
        );
        assert_eq!(as_kw_name(get_kw(&m, "tier")), "compiled");
        let Value::Vector(loops) = get_kw(&m, "loops") else {
            panic!("expected :loops to be a vector");
        };
        assert_eq!(loops.len(), 1);
        let Value::Map(loop0) = &loops[0] else {
            panic!("expected a loop record map");
        };
        assert!(!as_bool(get_kw(loop0, "numloop")), "expected the loop to stay generic");
        let reason = as_str(get_kw(loop0, "reason"));
        assert!(!reason.is_empty(), "expected a non-empty decline reason");
    }

    /// `(compile-explain f)` on an anonymous fn and on a non-fn both report
    /// `:unknown` rather than panicking or throwing.
    #[test]
    fn compile_explain_is_graceful_on_anonymous_fns_and_non_fns() {
        let mut interp = Interp::new();
        let v = interp
            .eval_str("compile-test", "(compile-explain (fn [x] x))")
            .unwrap();
        let Value::Map(m) = v else { panic!("expected a map") };
        assert_eq!(as_kw_name(get_kw(&m, "tier")), "unknown");

        let v = interp.eval_str("compile-test", "(compile-explain 5)").unwrap();
        let Value::Map(m) = v else { panic!("expected a map") };
        assert_eq!(as_kw_name(get_kw(&m, "tier")), "unknown");
    }
}
