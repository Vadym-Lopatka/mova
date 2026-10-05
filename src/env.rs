//! Lexical environment chain: `Arc<...>` parent links (v0.2 / A1: promoted
//! from `Rc<RefCell<...>>` so `Env` is `Send + Sync` -- a
//! `future*`-spawned thread's forked `Interp` shares the very same globals
//! `Env` `Arc` with the thread that spawned it). See `src/sync.rs` for the
//! poisoned-lock policy `lock_read`/`lock_write` implement.
//!
//! ## Two frame kinds (v0.5 / W-ENV)
//!
//! An `Env` is either the ROOT globals frame or a lexical frame, and the
//! two have genuinely different concurrency shapes, so they have different
//! reprs (see [`Repr`]). A lexical frame is per-call and effectively
//! thread-private: an ordinary `RwLock<HashMap<Symbol, Value>>`, unchanged.
//! The root is shared by every thread in the process and read on every
//! single symbol resolution, so it holds no lock on the read path at all --
//! an immutable [`RootMap`] published through an `AtomicPtr`. The full
//! rationale, ordering argument and reclamation story live on
//! [`RootGlobals`]; the short version is that a read lock is still two
//! atomic read-modify-writes on one shared cacheline, which made the
//! globals table scale BACKWARDS with thread count.
//!
//! ## Two binding kinds (v0.3 / S1)
//!
//! Every frame stores `Binding::Local(Value)` for ordinary lexical bindings
//! (fn params, `let`/`loop` locals) -- cheap, no allocation beyond the
//! `HashMap` entry itself, exactly like before this migration. The ROOT
//! frame is different: `Env::set` on the root instead interns a
//! `Binding::Var(Arc<VarCell>)` and writes the new value *through* that
//! cell rather than replacing the map entry. This preserves the cell's
//! identity across redefinition, which is the whole point: the future
//! compiled-fn tier will resolve a global once (at compile time) to its
//! `Arc<VarCell>` and read through that `Arc` on every call forever after,
//! the same way real Clojure vars work. If `set` just overwrote the
//! `HashMap` entry with a fresh `Value` on every `def`, a compiled fn that
//! captured the *old* cell would keep seeing the *old* definition after a
//! REPL redefinition -- exactly the staleness bug this store exists to
//! avoid. Child (non-root) frames never allocate a cell: tree-walker
//! per-call binding cost is unchanged by this migration.
//!
//! `VarCell::value` is `Option<Value>` so a symbol can be *interned but
//! unbound* (`Env::intern`, for the compiler to resolve forward references
//! before the corresponding `def` has run); an unbound cell is treated as
//! "not found" by `get`, falling through to the same ns-qualified/parent
//! lookup chain as an absent binding. `VarCell::pristine_builtin` tracks
//! whether the cell still holds the exact native Rust fn it was
//! boot-registered with (`Env::set_builtin`, used only by
//! `builtins::register_all`'s boot-time wiring) -- `false` the instant any
//! ordinary `def`/`set` (including a `core.mova`-defined fn, or a user
//! redefinition) writes through the cell. Nothing reads
//! `pristine_builtin` yet; it exists for a later phase that wants to tell
//! "still the untouched builtin" apart from "user has redefined this" (e.g.
//! to safely inline a builtin's behavior at compile time only while it's
//! still pristine).

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use arc_swap::{ArcSwapOption, Guard};

use crate::ctx::current_ctx;
use crate::sync::{lock_mutex, lock_read, lock_write};
use crate::value::{Symbol, Value};

/// W3e2: bumped by every change to the GLOBAL name->cell world -- a fresh
/// intern, or any `VarCell::store`. Read by `quasiquote`'s per-interpreter
/// syntax-quote resolution cache, which throws itself away whenever this
/// moves, so a cached answer can never outlive the mapping it was computed
/// from.
///
/// Why a counter and not finer-grained invalidation: syntax-quote
/// resolution reads the whole candidate order (`current-ns/name`, the
/// refer table, then the bare name) plus, for a class hit, the cell's
/// VALUE. Anything that could change any of those has to invalidate, and
/// enumerating them precisely is exactly the kind of thing that goes
/// subtly wrong later. A single monotone counter is conservative by
/// construction: a `def` anywhere clears every cached answer everywhere,
/// and `def`s are rare next to the macro expansions the cache exists for.
///
/// publication pattern: see ARCHITECTURE.md, "Publication patterns
/// (lock-free read paths)" -- generation counter.
static GLOBAL_GENERATION: AtomicU64 = AtomicU64::new(0);

pub(crate) fn bump_global_generation() {
    GLOBAL_GENERATION.fetch_add(1, Ordering::Relaxed);
}

/// The current global-mapping generation -- see [`GLOBAL_GENERATION`].
pub fn global_generation() -> u64 {
    GLOBAL_GENERATION.load(Ordering::Relaxed)
}

/// A global var's storage cell. Held behind an `Arc` so anyone who resolved
/// a symbol to its cell (the compiled-fn tier; `Env::intern`'s caller)
/// keeps seeing redefinitions written through `Env::set`/`Env::set_builtin`,
/// without re-resolving the symbol.
pub struct VarCell {
    /// The exact symbol this cell was interned under (`current-ns/name`,
    /// or bare in [`crate::ns::CORE_NS`]) -- carried so `Value::Var`
    /// (v0.5 / R2's `#'x`/`(var x)`) can print as `#'name` and so
    /// diagnostics can name the var without threading its symbol through
    /// separately. Set once, at `VarCell::unbound` (creation time), and
    /// never changes: a cell's identity IS its name.
    pub name: Symbol,
    /// `None` = interned but unbound (forward reference not yet `def`d).
    /// lsp/varcell: `ArcSwapOption`, not `RwLock` -- `get` is an atomic
    /// pointer load + `Arc` clone, no lock acquire/release pair. `store`
    /// swaps the pointer; the old `Arc` is freed once no reader still
    /// holds a load guard on it (arc-swap's own reclamation, not ours).
    pub value: ArcSwapOption<Value>,
    /// Lock-free mirror of `value.is_some()`, and MONOTONE: interning
    /// creates an unbound cell, `def` binds it, and nothing ever unbinds a
    /// var again. That is what lets `is_bound` answer "definitely unbound"
    /// without taking the lock -- the hot half of namespace resolution,
    /// where a name's `current-ns/name` candidate is usually an unbound
    /// placeholder in front of the bare core cell that actually holds the
    /// value (see `crate::ns`).
    ///
    /// `Relaxed` on both sides, deliberately: this flag is a HINT, and the
    /// `RwLock` around `value` is the real synchronization edge. `store`
    /// publishes the value (releasing the write lock) BEFORE flipping the
    /// flag, so a reader that sees `true` and then takes the read lock is
    /// guaranteed to find `Some`; a reader that sees a stale `false` simply
    /// behaves as though it had probed one instant earlier, which is the
    /// same race any concurrent `def` already has with `Env::get`.
    bound: AtomicBool,
    /// `true` only while this cell still holds the exact native fn it was
    /// boot-registered with; cleared by any subsequent root `set`.
    pub pristine_builtin: AtomicBool,
    /// field3 (W-DECL integration fix): `true` while this cell exists ONLY
    /// because the COMPILER speculatively interned it as a resolution
    /// candidate. `compile::resolve::global_chain` interns every candidate
    /// ahead of the first bound one (so a `def` of `my.ns/foo` reached
    /// AFTER the fn was compiled still re-resolves through the same cell --
    /// see that fn's own doc), which means the globals table is full of
    /// `current-ns/name` placeholders for names that actually live in
    /// `clojure.core`. Such a placeholder is NOT a namespace mapping on the
    /// JVM: real `Namespace.getMapping` only ever answers for a var some
    /// `def`/`declare`/`intern`/`refer` genuinely put there.
    ///
    /// Nothing could tell the difference until W-DECL taught `binding` to
    /// accept an interned-but-UNBOUND cell ([`Env::find_any_cell`]). From
    /// that point a plain `(binding [*ns* ...] ...)` reached inside a
    /// COMPILED fn body pushed its frame onto the speculative
    /// `current.ns/*ns*` placeholder, while `in-ns`/`Interp::
    /// set_dynamic_ns` kept writing the real, bare `*ns*` cell -- two
    /// different vars for one name. Measured fallout: the `binding` looked
    /// like a no-op to `dynamic_ns_name` AND leaked its `in-ns` past the
    /// frame's end (vendored `ns_libs.clj`'s `refer-error-messages`
    /// leaking its gensym'd ns into the NEXT deftest's `defrecord` error
    /// message -- "G__52.MyRecord" for "user.MyRecord" -- and `repl.clj`'s
    /// `test-dir` losing the `str` alias it binds `*ns*` to reach).
    ///
    /// Set ONLY by [`Env::intern_speculative`], and only on a cell it
    /// CREATES; cleared unconditionally by [`Env::intern`] (the genuine
    /// `def`/`declare`/`intern` path), so a forward reference the compiler
    /// interned first and a `def` reached later ends up genuine, as it
    /// must. Only ever consulted for an UNBOUND cell -- a value in the cell
    /// is proof enough that a real `def` happened. `Relaxed`, like `bound`
    /// above and for the same reason: a hint, monotone in the direction
    /// that matters (speculative -> genuine), never the synchronization
    /// edge.
    ///
    /// [`Env::find_any_cell`]: Env::find_any_cell
    /// [`Env::intern_speculative`]: Env::intern_speculative
    speculative: AtomicBool,
    /// M4b (dynamic vars): `true` while ANY thread currently has a
    /// `binding` frame pushed on this cell. This is the hot-path gate: the
    /// overwhelming majority of cells are never dynamically bound, and for
    /// them `get` pays exactly one extra `Relaxed` load before taking the
    /// same path it always took. Maintained under `dyn_bindings`' write
    /// lock (set on first push, cleared when the last thread's stack
    /// empties), so a reader that sees `false` while a push is mid-flight
    /// merely behaves as though it probed an instant earlier -- the same
    /// benign race `bound` already documents above.
    dyn_hint: AtomicBool,
    /// M4b: per-CONTEXT stacks of dynamic bindings, keyed by
    /// [`crate::ctx::current_ctx`]. Clojure's model exactly: a var has one
    /// root value plus a per-context stack of `binding` frames; reads see
    /// the top of the current context's stack, else the root. Kept per-CELL
    /// (not one global table) so an unbound read never touches any other
    /// cell's traffic. The map is only ever consulted behind `dyn_hint`.
    ///
    /// L1/W1: the key was `ThreadId` until go blocks became tasks. It is a
    /// `u64` ctx id now because two tasks multiplexed onto one shard thread
    /// share a `ThreadId` and must NOT share this stack -- see `ctx.rs`.
    /// One thread with no tasks in play is still one stable id, so the
    /// per-thread semantics this map had are unchanged.
    dyn_bindings: RwLock<HashMap<u64, Vec<Value>>>,
    /// S5 / M3: the var's MUTABLE metadata (`IReference`), which is a
    /// different thing from `IObj` metadata and must not be confused with
    /// it. `Value::Meta` (see `crate::value::MetaObj`) is an immutable
    /// wrapper around a value; this is a slot on the CELL, so
    /// `alter-meta!`/`reset-meta!` change what `(meta #'x)` answers for
    /// every holder of `#'x` without producing a new var. That is exactly
    /// Clojure's split: a var is `IReference` (mutable meta) but NOT
    /// `IObj`, which is why `(with-meta #'x {})` has no meaning while
    /// `(alter-meta! #'x assoc :doc "hi")` does.
    ///
    /// `Value::Nil` while the var has no metadata, otherwise a map. Not
    /// wrapped in `Option` because `meta`'s answer for "no metadata" is
    /// `nil` anyway, so `Option<Value>` would just add a layer every
    /// reader has to flatten.
    ///
    /// Root-only, deliberately: metadata is a property of the var, not of
    /// a `binding` frame (measured -- `(binding [*x* 1] (alter-meta!
    /// #'*x* assoc :a 1))` mutates the var itself and the change outlives
    /// the frame), so this sits beside `name` rather than inside
    /// `dyn_bindings`.
    meta: RwLock<Value>,
}

thread_local! {
    /// M4b: the cells this thread currently holds `binding` frames on, in
    /// push order (a cell appears once PER active frame, so nested
    /// bindings of the same var occupy two slots). This exists for
    /// conveyance: `snapshot_thread_bindings` reads it to reproduce the
    /// spawning thread's dynamic environment inside a `future*` thread
    /// (measured Clojure: `(binding [*a* 42] @(future *a*))` is `42`).
    static ACTIVE_BINDINGS: RefCell<Vec<Arc<VarCell>>> = const { RefCell::new(Vec::new()) };
}

/// The spawning thread's live dynamic environment: each currently-bound
/// cell paired with its CURRENT top-of-stack value, deduplicated to the
/// visible (innermost) frame, in first-bound order. Feed to
/// [`BindingConveyance::install`] on the spawned thread.
pub fn snapshot_thread_bindings() -> Vec<(Arc<VarCell>, Value)> {
    ACTIVE_BINDINGS.with(|ab| {
        let cells = ab.borrow();
        let mut seen: Vec<(Arc<VarCell>, Value)> = Vec::new();
        for cell in cells.iter() {
            if seen.iter().any(|(c, _)| Arc::ptr_eq(c, cell)) {
                continue; // innermost frame already captured via current_binding
            }
            if let Some(v) = cell.current_binding() {
                seen.push((cell.clone(), v));
            }
        }
        seen
    })
}

thread_local! {
    /// S7 (tail wave): frame boundaries for the LOW-LEVEL
    /// `push-thread-bindings`/`pop-thread-bindings` functions (rt.clj's
    /// `bare-rt-print` calls them directly, below the `binding` macro).
    /// Each entry is the [`ACTIVE_BINDINGS`] length at the matching
    /// `push-thread-bindings` call -- `pop-thread-bindings` pops back to
    /// it, calling `VarCell::pop_binding` (which does its OWN
    /// `ACTIVE_BINDINGS` bookkeeping -- see that fn's doc) for each cell
    /// pushed in that frame. Reuses the exact same per-cell push/pop
    /// primitives `binding` itself uses, just frame-scoped instead of
    /// lexically-scoped.
    static PUSH_FRAMES: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
}

/// `(push-thread-bindings {#'v1 val1 ...})` -- pushes one dynamic frame per
/// `(cell, value)` pair and records the frame boundary for the next
/// `pop_thread_bindings`.
pub fn push_thread_bindings(pairs: &[(Arc<VarCell>, Value)]) {
    let start = ACTIVE_BINDINGS.with(|ab| ab.borrow().len());
    for (cell, v) in pairs {
        cell.push_binding(v.clone());
    }
    PUSH_FRAMES.with(|f| f.borrow_mut().push(start));
}

/// `(pop-thread-bindings)` -- pops the most recent `push_thread_bindings`
/// frame. A stray call with no matching push is a silent no-op (matches
/// this fn's narrow, single-measured-call-site scope; real Clojure throws
/// `IllegalStateException: Pop without matching push`, not reproduced here
/// since nothing in this task's corpus exercises that error path).
pub fn pop_thread_bindings() {
    let Some(start) = PUSH_FRAMES.with(|f| f.borrow_mut().pop()) else {
        return;
    };
    loop {
        let cell = ACTIVE_BINDINGS.with(|ab| {
            let ab = ab.borrow();
            if ab.len() > start {
                ab.last().cloned()
            } else {
                None
            }
        });
        match cell {
            Some(c) => c.pop_binding(),
            None => break,
        }
    }
}

/// L1/W1: the whole of a context's binding bookkeeping, lifted out of
/// thread-local storage so a scheduler can put a different context's copy
/// there. [`ACTIVE_BINDINGS`] and [`PUSH_FRAMES`] are two halves of one
/// state -- `PUSH_FRAMES` stores indices INTO `ACTIVE_BINDINGS` -- so they
/// are swapped as a unit or not at all.
///
/// Nothing outside a scheduler switch may take these: a context that loses
/// its `ACTIVE_BINDINGS` still owns live frames in every `VarCell::
/// dyn_bindings` it pushed on, and only reinstalling them (or unwinding
/// through the matching `pop_binding`s first) leaves the two consistent.
#[derive(Default)]
pub struct BindingLocals {
    active: Vec<Arc<VarCell>>,
    frames: Vec<usize>,
}

/// Moves the running context's binding bookkeeping OUT of this thread,
/// leaving empty vectors behind (i.e. the state of a thread that has never
/// bound anything). Pairs with [`install_binding_locals`]; the scheduler
/// calls this on switch-out, alongside saving the ctx id.
pub fn take_binding_locals() -> BindingLocals {
    BindingLocals {
        active: ACTIVE_BINDINGS.with(|ab| std::mem::take(&mut *ab.borrow_mut())),
        frames: PUSH_FRAMES.with(|f| std::mem::take(&mut *f.borrow_mut())),
    }
}

/// Reverse of [`take_binding_locals`]: makes `locals` the running context's
/// bookkeeping, DISCARDING whatever was there. Correct only because the one
/// legal caller (a scheduler switch-in) has just taken the outgoing
/// context's copy; the two Vec moves are the entire per-switch cost §3.4
/// budgets for.
pub fn install_binding_locals(locals: BindingLocals) {
    let BindingLocals { active, frames } = locals;
    ACTIVE_BINDINGS.with(|ab| *ab.borrow_mut() = active);
    PUSH_FRAMES.with(|f| *f.borrow_mut() = frames);
}

/// [`take_binding_locals`] and [`install_binding_locals`] as ONE operation:
/// installs `locals` and hands back what was there.
///
/// L2/W2 item 1. A scheduler switch is always a swap -- it never installs
/// without first taking, or takes without installing -- and spelling it as
/// two calls charges FOUR thread-local accesses where two will do. On
/// aarch64-darwin each of those is a call through the TLV descriptor, and
/// the four here were a quarter of the ~14 a task hop was paying
/// (docs/L2-PROBE-RESULTS.md §6 item 1). The per-binding-op access pattern
/// of [`ACTIVE_BINDINGS`]/[`PUSH_FRAMES`] is deliberately untouched: this
/// collapses SWITCH traffic only, and `binding` from a plain OS thread runs
/// exactly the code it ran before.
///
/// It is also strictly safer than the pair it replaces:
/// [`install_binding_locals`] DISCARDS the outgoing context's bookkeeping
/// and is correct only because its one caller had just taken it. This
/// returns it, so there is nothing to get wrong.
pub fn swap_binding_locals(locals: BindingLocals) -> BindingLocals {
    let BindingLocals { active, frames } = locals;
    BindingLocals {
        active: ACTIVE_BINDINGS.with(|ab| std::mem::replace(&mut *ab.borrow_mut(), active)),
        frames: PUSH_FRAMES.with(|f| std::mem::replace(&mut *f.borrow_mut(), frames)),
    }
}

/// RAII conveyance for a spawned thread: pushes every snapshotted binding
/// on construction (making the child see the parent's dynamic env) and
/// pops them all when dropped -- including on panic/error unwind, which is
/// the entire reason this is a guard and not a pair of function calls.
pub struct BindingConveyance {
    cells: Vec<Arc<VarCell>>,
}

impl BindingConveyance {
    pub fn install(snapshot: Vec<(Arc<VarCell>, Value)>) -> Self {
        let mut cells = Vec::with_capacity(snapshot.len());
        for (cell, v) in snapshot {
            cell.push_binding(v);
            cells.push(cell);
        }
        BindingConveyance { cells }
    }
}

impl Drop for BindingConveyance {
    fn drop(&mut self) {
        // Reverse order, same as `binding` itself unwinds.
        for cell in self.cells.drain(..).rev() {
            cell.pop_binding();
        }
    }
}

/// MT2: the result of [`VarCell::read_root`] -- a value borrow that avoids
/// cloning the root `Value` on the hot call path. `Bound` holds the
/// arc-swap `Guard` alive (no lock, no refcount churn on the shared
/// pointer); `Dyn` is a per-context `binding` frame value, which was
/// already an owned clone out of the ctx map (see `current_binding`) so
/// there is nothing left to save there.
pub enum RootRead {
    Bound(Guard<Option<Arc<Value>>>),
    Dyn(Value),
}

impl RootRead {
    #[inline]
    pub fn value(&self) -> &Value {
        match self {
            RootRead::Bound(g) => g.as_ref().expect("RootRead::Bound only built when Some"),
            RootRead::Dyn(v) => v,
        }
    }
}

impl VarCell {
    /// `pub(crate)` (not private): S7's `local-var*` builtin
    /// (`builtins::atoms`) mints a fresh, UNINTERNED cell this same way
    /// for `with-local-vars` -- a local var is exactly an ordinary
    /// `VarCell`, just never `Env::intern`ed into any namespace's table,
    /// so nothing else can ever resolve it by name (only the lexical
    /// binding `with-local-vars` itself creates can reach it).
    pub(crate) fn unbound(name: Symbol) -> Arc<Self> {
        Arc::new(VarCell {
            name,
            value: ArcSwapOption::empty(),
            bound: AtomicBool::new(false),
            pristine_builtin: AtomicBool::new(false),
            // Genuine by default: every OTHER minting path (`with-local-
            // vars`, `Env::intern`) is a real var. Only
            // `Env::intern_speculative` flips this on, right after it
            // creates the cell.
            speculative: AtomicBool::new(false),
            dyn_hint: AtomicBool::new(false),
            dyn_bindings: RwLock::new(HashMap::new()),
            meta: RwLock::new(Value::Nil),
        })
    }

    /// field3: is this cell nothing but a compiler-interned resolution
    /// candidate? See the [`speculative`](VarCell::speculative) field's
    /// doc. Always `false` for a bound cell, whatever the flag says -- a
    /// root value can only have come from a real `def`/`set`.
    pub(crate) fn is_speculative(&self) -> bool {
        !self.is_bound() && self.speculative.load(Ordering::Relaxed)
    }

    /// field3: mark this cell as a genuine namespace mapping (idempotent).
    /// Called by [`Env::intern`], the `def`/`declare`/`intern`/`refer`
    /// path, on every hit -- including one the compiler interned
    /// speculatively earlier (a forward reference), which is exactly the
    /// case that must be promoted.
    pub(crate) fn mark_genuine(&self) {
        self.speculative.store(false, Ordering::Relaxed);
    }

    /// S5/M3: this var's `IReference` metadata (`(meta #'x)`), or `Nil`.
    /// See the [`meta`](VarCell::meta) field's doc for why a var has a
    /// mutable metadata slot while an ordinary value gets an immutable
    /// `Value::Meta` wrapper instead.
    pub fn var_meta(&self) -> Value {
        crate::sync::lock_read(&self.meta).clone()
    }

    /// f4/ns: `true` iff `^{:private true}` metadata is on this cell --
    /// factored out of `Interp::private_var_violation`'s and
    /// `builtins::nsfns::refer`'s previously-duplicated `matches!` (both
    /// checked the exact same `:private` key by hand), and now also the
    /// gate `ns-publics` uses to hide a private var from its listing (the
    /// oracle transcript's `hidden-var` case,
    /// `compat/w-decl-fix-ns-machinery-oracle-transcript.txt`). `ns-interns`/
    /// `ns-map` do NOT call this -- both list every genuine mapping
    /// regardless of privacy, matching the JVM's own `Namespace.
    /// getMappings` (privacy is a RESOLUTION-time gate on the JVM, not a
    /// listing-time one; only `ns-publics`'s own contract narrows to
    /// public).
    pub fn is_private(&self) -> bool {
        matches!(
            self.var_meta(),
            Value::Map(m) if matches!(
                m.get(&Value::Keyword(crate::keyword::Keyword::from("private"))),
                Some(Value::Bool(true))
            )
        )
    }

    /// True when the var was declared `^:dynamic` (a real metadata map with a truthy `:dynamic`).
    pub fn var_meta_is_dynamic(&self) -> bool {
        matches!(
            crate::sync::lock_read(&self.meta).clone(),
            Value::Map(m) if matches!(
                m.get(&Value::Keyword(crate::keyword::Keyword::from("dynamic"))),
                Some(v) if !matches!(v, Value::Nil | Value::Bool(false))
            )
        )
    }

    /// S5/M3: replaces this var's `IReference` metadata wholesale --
    /// `reset-meta!`, the write half of `alter-meta!`, and how `def`
    /// publishes a definition's `^{...}` into `(meta #'x)`.
    pub fn set_var_meta(&self, m: Value) {
        *crate::sync::lock_write(&self.meta) = m;
    }

    /// Bug-1 fix (mova/PLAN.md): whether a `set!` that finds no active
    /// binding frame should push a FRESH per-thread frame instead of
    /// writing the shared root. Mirrors `check_dynamic_or_err`'s
    /// permissive reading of metadata: `Nil` (bootstrap cells like `*ns*`
    /// that bypass `eval_def`'s meta pipeline, per that fn's doc) counts
    /// as eligible; a real meta map only qualifies with a truthy
    /// `:dynamic`; anything else (an ordinary non-dynamic var) keeps the
    /// old shared root write so plain top-level mutable state is
    /// unaffected. Without this, `*ns*`/`*warn-on-reflection*`/etc. wrote
    /// through `globals` (`Arc`-shared across every `Interp::fork`), so a
    /// `(future (in-ns 'x) ...)` or a clj-kondo hook thread's `in-ns`
    /// mutated the CALLER's `*ns*` too -- unlike the JVM, where `set!`
    /// always writes the current thread's own binding frame.
    pub fn prefers_thread_local_set(&self) -> bool {
        match self.var_meta() {
            Value::Nil => true,
            Value::Map(m) => m
                .get(&Value::Keyword(crate::keyword::Keyword::from("dynamic")))
                .is_some_and(Value::truthy),
            _ => false,
        }
    }

    /// This cell's value: the current thread's innermost `binding` frame
    /// if one exists (M4b), else the root value, or `None` while unbound.
    /// The `dyn_hint` gate keeps the never-dynamically-bound fast path at
    /// one extra `Relaxed` load -- see the field's doc.
    #[inline]
    /// MT2: borrowing sibling of [`get`] -- same dyn_hint/root logic, but
    /// the bound-root case returns an arc-swap `Guard` (a lock-free borrow,
    /// no refcount bump on the shared `Arc<Value>`, no `Value::clone`)
    /// instead of an owned clone. The `Guard` keeps the old value alive
    /// even if another thread `store`s a new one mid-borrow, so callers may
    /// hold a [`RootRead`] across a whole call (arg eval + apply) without
    /// risking use-after-free -- see `compile::exec::exec_call_global`.
    ///
    /// [`get`]: Self::get
    pub fn read_root(&self) -> Option<RootRead> {
        if self.dyn_hint.load(Ordering::Relaxed) {
            crate::lens::event(crate::lens::Event::DynBindingWalk);
            if let Some(v) = self.current_binding() {
                return Some(RootRead::Dyn(v));
            }
        }
        if !self.is_bound() {
            return None;
        }
        let guard = self.value.load();
        if guard.is_some() {
            Some(RootRead::Bound(guard))
        } else {
            None
        }
    }

    pub fn get(&self) -> Option<Value> {
        if self.dyn_hint.load(Ordering::Relaxed) {
            // field4/W-LENS-1: the SLOW arm only -- an `RwLock` read plus a
            // ctx-id hash plus a `Vec` tail read, per var read, for
            // every var any thread has ever `binding`-ed. `VarCell::get`
            // measured ~4.9% of delays.clj post-W-RESOLVE, and this counter
            // is what would tell a future session whether that is this walk
            // or the root path. The never-dynamically-bound fast path
            // (`dyn_hint` false, the overwhelming majority of cells) is
            // untouched: the counter lives INSIDE the branch.
            crate::lens::event(crate::lens::Event::DynBindingWalk);
            if let Some(v) = self.current_binding() {
                return Some(v);
            }
        }
        if !self.is_bound() {
            return None;
        }
        self.value.load_full().map(|v| (*v).clone())
    }

    /// This cell's ROOT value, ignoring any `binding` frame the current
    /// thread has pushed -- the JVM's `Var.getRawRoot`. `None` while
    /// unbound.
    ///
    /// W3e-3: `with-redefs` needs exactly this and not [`get`]. Real
    /// `with-redefs-fn` reads `(.getRawRoot v)` before swapping and writes
    /// it back with `.bindRoot` in its `finally`, so a `with-redefs` nested
    /// inside a `binding` of the same var restores the var to its ROOT, not
    /// to the thread-local value that was merely VISIBLE at entry.
    /// `clojure.test-clojure.vars/test-with-redefs-inside-binding` measures
    /// precisely that: `(binding [dynamic-var 2] (with-redefs
    /// [dynamic-var 3] ...))` must leave `dynamic-var` reading `1` -- its
    /// root -- once the `binding` also exits. Reading through `get` saved
    /// `2` and made the trailing `(is (= 1 dynamic-var))` see `2`.
    ///
    /// [`get`]: Self::get
    pub fn raw_root(&self) -> Option<Value> {
        if !self.is_bound() {
            return None;
        }
        self.value.load_full().map(|v| (*v).clone())
    }

    /// M10 debug: [ctx-entries total-frames] of this var's per-ctx binding stacks.
    pub fn dyn_census(&self) -> (usize, usize) {
        let map = lock_read(&self.dyn_bindings);
        (map.len(), map.values().map(|v| v.len()).sum())
    }

    /// The current context's innermost `binding` frame value, if any.
    /// Deliberately does NOT check `dyn_hint` itself: `get` gates on the
    /// hint for speed, while conveyance/`set!` call this directly for
    /// correctness (they already know they care).
    pub fn current_binding(&self) -> Option<Value> {
        let map = lock_read(&self.dyn_bindings);
        map.get(&current_ctx())
            .and_then(|stack| stack.last())
            .cloned()
    }

    /// Pushes a `binding` frame for the current context and registers it in
    /// the context's [`ACTIVE_BINDINGS`] (conveyance bookkeeping). Callers
    /// are responsible for the matching [`pop_binding`] -- the `binding`
    /// special form and [`BindingConveyance`] both guarantee it on unwind.
    pub fn push_binding(self: &Arc<Self>, v: Value) {
        {
            let mut map = lock_write(&self.dyn_bindings);
            map.entry(current_ctx()).or_default().push(v);
            // E1b: the FIRST time this cell ever becomes dynamically bound,
            // bump the JIT direct-call epoch -- an inline cache that had
            // already cached this var's (until-now-static) value must not
            // keep using it once a `binding` frame can shadow it. Later
            // pushes/pops on an already-dynamic cell do not bump again;
            // `jit_call_miss` instead refuses to (re-)cache while
            // `dyn_hint` reads true, so a live `binding` always re-resolves.
            if !self.dyn_hint.swap(true, Ordering::Relaxed) {
                crate::jit::bump_def_epoch();
            }
        }
        ACTIVE_BINDINGS.with(|ab| ab.borrow_mut().push(self.clone()));
    }

    /// E1b (docs/JIT.md): whether a `binding` frame currently (or ever)
    /// shadows this cell's root value -- `jit_call_miss` must not cache a
    /// direct-call entry while this is true, since the visible value can
    /// then change per push/pop without a `DEF_EPOCH` bump.
    #[inline]
    pub fn is_dyn_hinted(&self) -> bool {
        self.dyn_hint.load(Ordering::Relaxed)
    }

    /// Pops the current context's innermost frame (reverse of
    /// [`push_binding`]). Clears `dyn_hint` when the whole map empties so
    /// the fast path goes back to one relaxed load and a miss.
    pub fn pop_binding(&self) {
        {
            let mut map = lock_write(&self.dyn_bindings);
            let ctx = current_ctx();
            if let Some(stack) = map.get_mut(&ctx) {
                stack.pop();
                if stack.is_empty() {
                    map.remove(&ctx);
                }
            }
            if map.is_empty() {
                self.dyn_hint.store(false, Ordering::Relaxed);
            }
        }
        ACTIVE_BINDINGS.with(|ab| {
            let mut cells = ab.borrow_mut();
            // Remove the LAST occurrence of this cell (stack discipline;
            // nested bindings of one var registered it twice).
            if let Some(pos) = cells.iter().rposition(|c| std::ptr::eq(c.as_ref(), self)) {
                cells.remove(pos);
            }
        });
    }

    /// `set!` support (M4b): overwrites the current context's innermost
    /// `binding` frame, returning `false` when the context has none (the
    /// caller then falls back to a root write -- mova's documented
    /// divergence until the full Clojure "set! requires a thread binding"
    /// rule can land).
    pub fn set_binding(&self, v: Value) -> bool {
        if !self.dyn_hint.load(Ordering::Relaxed) {
            return false;
        }
        let mut map = lock_write(&self.dyn_bindings);
        match map
            .get_mut(&current_ctx())
            .and_then(|stack| stack.last_mut())
        {
            Some(slot) => {
                *slot = v;
                true
            }
            None => false,
        }
    }

    #[inline]
    pub fn is_bound(&self) -> bool {
        self.bound.load(Ordering::Relaxed)
    }

    /// The one writer: publishes `val` and then flips `bound`, so a reader
    /// that sees `bound` also sees the value. `pristine` is `true` only for
    /// `Env::set_builtin`'s boot-time native registration.
    pub fn store(&self, val: Value, pristine: bool) {
        // lsp/varcell: swap the pointer -- publishes `val` atomically;
        // the old `Arc<Value>` (if any) is reclaimed once every in-flight
        // `get`/`raw_root` load guard on it has dropped.
        self.value.store(Some(Arc::new(val)));
        self.bound.store(true, Ordering::Relaxed);
        self.pristine_builtin.store(pristine, Ordering::Release);
        // W3e2: a cell going from unbound to bound, or from a class to a
        // non-class, changes what `quasiquote`'s symbol resolution answers
        // -- see `global_generation`. `store` is only ever reached from
        // `def`/`intern`/`set!`/`with-redefs`/`alter-var-root`/boot-time
        // registration, all of which are rare next to the macro expansions
        // the cache exists for.
        bump_global_generation();
        // E1b: same choke point invalidates every JIT direct-call inline
        // cache that had resolved through this cell (docs/JIT.md).
        crate::jit::bump_def_epoch();
    }

    /// A fresh, independent cell holding a snapshot of this one's current
    /// state (same name/value/bound/pristine), used by [`Env::snapshot`] --
    /// see that method's doc for why a snapshot needs a brand-new `Arc` per
    /// cell rather than cloning the `Arc` itself (`fork`'s sharing
    /// strategy). The two locks are taken and released independently (not
    /// atomically with each other), which is fine: at worst a concurrent
    /// `def` racing the snapshot lands its write on either side of the
    /// value read, exactly the same race `Env::get` already tolerates.
    fn snapshot(&self) -> Arc<Self> {
        Arc::new(VarCell {
            name: self.name.clone(),
            value: ArcSwapOption::new(self.value.load_full()),
            bound: AtomicBool::new(self.is_bound()),
            pristine_builtin: AtomicBool::new(self.pristine_builtin.load(Ordering::Acquire)),
            // field3: carried, like `pristine_builtin` -- a snapshot of a
            // world in which `my.ns/foo` is only a compiler-interned
            // resolution candidate must not promote it to a real var.
            speculative: AtomicBool::new(self.speculative.load(Ordering::Relaxed)),
            // M4b: dynamic frames are PER-THREAD, live-execution state --
            // they belong to the threads that pushed them, not to a
            // point-in-time world copy, so a snapshot starts with none
            // (same reasoning as `Interp::fork` starting with a fresh call
            // stack). `Engine::snapshot` is documented as a
            // quiescent-world operation; mid-`binding` snapshots would be
            // torn state either way.
            dyn_hint: AtomicBool::new(false),
            dyn_bindings: RwLock::new(HashMap::new()),
            // S5/M3: metadata IS part of the world a snapshot copies
            // (unlike `dyn_bindings` above) -- it's root state on the
            // var, set by `def` and `alter-meta!`, not per-thread
            // execution state.
            meta: RwLock::new(lock_read(&self.meta).clone()),
        })
    }
}

/// W-ENV: a cell as stored in the ROOT map.
///
/// `champ::PersistentHashMap::assoc` requires `V: PartialEq` (it
/// returns a pointer-identical map when a write would change nothing), and
/// `Arc<VarCell>` has none, deliberately: a var's equality IS its identity
/// -- two cells interned under the same name are still two different vars,
/// which is the whole premise of the cell-identity contract in this
/// module's doc. `Arc::ptr_eq` is therefore not a shortcut here, it is the
/// only correct answer.
#[derive(Clone)]
struct RootCell(Arc<VarCell>);

impl PartialEq for RootCell {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// The globals table. A persistent CHAMP (the same one behind
/// `Value::Map`/`Value::Set`) rather than a `std::HashMap`, because the
/// repr below publishes a whole new map per write: a persistent map pays
/// one path-copy per intern (measured 211 ns at n=800, see
/// `tests/globals_snapshot_bench.rs`) where cloning a `std::HashMap` pays
/// the entire table (8-80 us at the same size, i.e. 6-64 ms added to boot
/// -- disqualifying for a latency-first project).
type RootMap = champ::PersistentHashMap<Symbol, RootCell>;

/// W-ENV: the ROOT globals frame, read WITHOUT taking any lock.
///
/// publication pattern: see ARCHITECTURE.md, "Publication patterns
/// (lock-free read paths)" -- AtomicPtr + retire-list snapshot.
///
/// # Why this exists
///
/// Every symbol the tree-walker resolves ends in
/// `Interp::for_each_global_candidate` (`crate::ns`) probing 1-3 candidate
/// spellings, and every probe used to be one acquire+release of a single
/// process-wide `RwLock`. A read lock does not serialize the critical
/// section, but each acquire and each release is an atomic
/// read-modify-write on ONE cacheline, and an RMW needs that line
/// exclusive -- so the cost is ~6 coherence round trips per symbol no
/// matter how short the body is. Measured (`tests/globals_snapshot_bench.
/// rs`, 13 readers on an M4 Pro): the locked repr SCALES BACKWARDS -- 13
/// threads resolve fewer symbols per second in total than 1 thread does.
/// That is the `vendor/delays.clj` collapse (100 threads x 10k tree-walked
/// `(is (= 1 @d))`) in isolation.
///
/// # The repr
///
/// The map is immutable once published, and the pointer to it is swapped
/// wholesale. A reader does ONE `Acquire` load and dereferences it: no
/// RMW, so the pointer's cacheline stays Shared in every core's cache and
/// reads scale flat. Measured 14-25x contended AND ~+3% single-threaded
/// against the lock it replaces (the probe's `D_atom` row) -- the
/// thread-local-snapshot alternative won contention just as decisively but
/// cost 10-17% single-threaded on macOS, where a TLS access is a call.
///
/// # Ordering argument
///
/// 1. A writer builds the ENTIRE next map off to the side and installs it
///    with a single `Release` swap, so a reader's `Acquire` load observes
///    either the old map or a fully-constructed new one. There is no
///    window in which the pointer is visible but the nodes it names are
///    not -- publishing the pointer IS publishing the map (the same
///    "the pin is part of the published value" property
///    `types::ProtoIcEntry` documents for the protocol IC).
/// 2. Writers are serialized by `writer`, so the read-modify-write a
///    publish performs (probe the current map, add one key, swap) cannot
///    lose an update. Writes are RARE by construction: `Env::intern`'s
///    slow path fires only for a genuinely NEW name and `bind_alias` only
///    at ns-load time, while ordinary `def`/redefinition writes THROUGH
///    the existing `Arc<VarCell>` (`VarCell::store`) and never touches
///    this map at all.
/// 3. A reader can hold a `&RootMap` while a writer publishes over it, so
///    the superseded map must outlive that borrow. It does: instead of
///    being dropped, it is moved onto the `writer` mutex's retire list and
///    freed in `Drop`, at which point the last `Arc<Repr>` is gone and no
///    `&RootMap` derived from `self` can exist (the borrow is tied to
///    `&self`). This bounds the cost by the ENV's lifetime rather than the
///    process's, which matters because an embedder may create and drop
///    many `Engine`s. What it does NOT do is reclaim during a long life:
///    a still-live env retains one path-copy per new global name ever
///    interned (~a few nodes each; the `Arc<VarCell>` that intern also
///    allocates, and which likewise lives as long as the env, is the
///    larger permanent allocation). That is this design's one real
///    liability, and it is priced rather than hidden.
struct RootGlobals {
    published: AtomicPtr<RootMap>,
    /// Serializes publishes AND owns the retired maps -- one lock, because
    /// retiring is part of publishing and nothing else may touch either.
    // `clippy::vec_box` is wrong here, and dangerously so: the `Box` is not
    // a removable indirection over `Vec<RootMap>`, it is the SAME
    // allocation a reader may still be inside, reconstructed from the raw
    // pointer we published. Storing the map by value would MOVE it,
    // invalidating exactly the pointers this list exists to keep valid.
    #[allow(clippy::vec_box)]
    writer: Mutex<Vec<Box<RootMap>>>,
}

impl RootGlobals {
    fn with_map(m: RootMap) -> Self {
        RootGlobals {
            published: AtomicPtr::new(Box::into_raw(Box::new(m))),
            writer: Mutex::new(Vec::new()),
        }
    }

    /// The currently published map. See the ordering argument above.
    #[inline]
    fn map(&self) -> &RootMap {
        // SAFETY: `published` is only ever set from `Box::into_raw` of a
        // live `RootMap` (here and in `publish`), is never null, and a
        // superseded map is retired rather than freed until `Drop` -- so
        // the pointee outlives `&self`, which is exactly the lifetime the
        // returned reference claims.
        unsafe { &*self.published.load(Ordering::Acquire) }
    }

    /// Installs `next` and retires the map it replaces. Caller must hold
    /// `writer` (it passes the guard in, so that cannot be forgotten).
    #[allow(clippy::vec_box)] // see the `writer` field
    fn publish(&self, retired: &mut Vec<Box<RootMap>>, next: RootMap) {
        let old = self
            .published
            .swap(Box::into_raw(Box::new(next)), Ordering::Release);
        // SAFETY: `old` is the pointer a previous `Box::into_raw` produced
        // and no other thread can be swapping concurrently (we hold
        // `writer`), so reclaiming ownership of it here is sound; it is
        // parked on the retire list rather than dropped because readers
        // may still be inside it.
        retired.push(unsafe { Box::from_raw(old) });
    }

    /// `Env::intern`'s root half: get-or-create, with the fast path taking
    /// no lock at all (the overwhelmingly common case -- every symbol the
    /// compiled tier resolves is already interned).
    ///
    /// field3: `speculative` says what to do about
    /// [`VarCell::speculative`] -- `false` (the `def`/`declare`/`intern`
    /// path, `Env::intern`) promotes whatever it finds to a genuine
    /// namespace mapping; `true` ([`Env::intern_speculative`], the
    /// compiler's candidate walk) marks a cell it CREATES as a mere
    /// placeholder and leaves an already-existing one exactly as it found
    /// it -- a candidate probe must never demote a real var.
    fn get_or_intern(&self, sym: &Symbol, speculative: bool) -> Arc<VarCell> {
        if let Some(c) = self.map().get(sym) {
            if !speculative {
                c.0.mark_genuine();
            }
            return c.0.clone();
        }
        let mut retired = lock_mutex(&self.writer);
        // Re-check under the writer lock: another thread may have interned
        // `sym` between the probe above and here (this is the root globals
        // table, reachable from any `future*` thread).
        if let Some(c) = self.map().get(sym) {
            if !speculative {
                c.0.mark_genuine();
            }
            return c.0.clone();
        }
        let cell = VarCell::unbound(sym.clone());
        if speculative {
            cell.speculative.store(true, Ordering::Relaxed);
        }
        let next = self.map().assoc(sym.clone(), RootCell(cell.clone()));
        self.publish(&mut retired, next);
        bump_global_generation();
        cell
    }

    fn bind_alias(&self, sym: Symbol, cell: Arc<VarCell>) {
        let mut retired = lock_mutex(&self.writer);
        let next = self.map().assoc(sym, RootCell(cell));
        self.publish(&mut retired, next);
    }
}

impl Drop for RootGlobals {
    fn drop(&mut self) {
        // Exclusive access by construction (`&mut self`), so every
        // outstanding `&RootMap` borrow is provably over -- point 3 of the
        // ordering argument. Both the published map and the retire list
        // are freed here; nothing survives the env that owned them.
        let p = *self.published.get_mut();
        if !p.is_null() {
            // SAFETY: as in `publish` -- `p` came from `Box::into_raw`, and
            // this is the last possible use of it.
            drop(unsafe { Box::from_raw(p) });
        }
        self.writer
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }
}

/// A non-root (lexical) frame: `let`/`loop`/fn-param bindings only.
///
/// Root bindings live in [`RootGlobals`] instead, which is why this holds
/// a plain `Value` and not the pre-W-ENV `Binding::{Local, Var}` enum:
/// `Env::set` interns on the root and inserts a local on a frame,
/// `intern`/`bind_alias` walk to the root first, and nothing else writes a
/// frame -- so a frame could never hold a `Var` even before this split.
/// Dropping the enum takes a branch off the lexical read path.
struct EnvInner {
    vars: HashMap<Symbol, Value>,
    parent: Env,
}

/// fix/closure-env-cycles Step 1 (`tests/leak_cycle_probe.rs`): raw
/// create/drop counters, gated behind `leak-probe` so they never ship.
#[cfg(feature = "leak-probe")]
pub static FRAME_CREATED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
#[cfg(feature = "leak-probe")]
pub static FRAME_DROPPED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(feature = "leak-probe")]
impl Drop for EnvInner {
    fn drop(&mut self) {
        FRAME_DROPPED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// The two frame kinds, which have genuinely different concurrency shapes:
/// the root is shared by every thread in the process and is read
/// constantly, while a lexical frame is per-call and effectively
/// thread-private (only a closure escaping to a `future*` thread shares
/// one, and then read-only).
enum Repr {
    Root(RootGlobals),
    Frame(RwLock<EnvInner>),
}

#[derive(Clone)]
pub struct Env(Arc<Repr>);

/// Owns a `let`/`loop` frame for its lifetime and runs
/// [`Env::break_frame_cycles`] when it drops -- on EVERY exit path,
/// including the `?` unwinds that a hand-placed call at the end of
/// `eval_let` would miss.
///
/// The guard must hold the *sole* persistent `Env` handle to its frame:
/// that is precisely the assumption `break_frame_cycles`' `1 + backrefs`
/// arithmetic rests on. A transient clone that dies before the guard does
/// is fine; one that outlives the body would silently defeat the check (by
/// inflating the count) rather than corrupt anything -- but it would also
/// bring the leak back, so callers must not keep one.
pub(crate) struct FrameGuard(pub(crate) Env);

impl Drop for FrameGuard {
    fn drop(&mut self) {
        self.0.break_frame_cycles()
    }
}

impl std::ops::Deref for FrameGuard {
    type Target = Env;
    fn deref(&self) -> &Env {
        &self.0
    }
}

impl Env {
    /// K7b: identity of the published root map (`None` for a frame env); same id = same bindings.
    #[inline]
    pub fn root_map_id(&self) -> Option<usize> {
        match &*self.0 {
            Repr::Root(g) => Some(g.map() as *const RootMap as usize),
            Repr::Frame(_) => None,
        }
    }

    /// K7b: the root map's cell for `sym` exactly as `get` probes it (no bare-name retry).
    pub fn root_cell(&self, sym: &Symbol) -> Option<Arc<VarCell>> {
        match &*self.0 {
            Repr::Root(g) => g.map().get(sym).map(|c| c.0.clone()),
            Repr::Frame(_) => None,
        }
    }

    pub fn ptr_eq(a: &Env, b: &Env) -> bool {
        Arc::ptr_eq(&a.0, &b.0)
    }

    pub fn new_root() -> Self {
        Env(Arc::new(Repr::Root(RootGlobals::with_map(RootMap::new()))))
    }

    pub fn child(&self) -> Self {
        #[cfg(feature = "leak-probe")]
        FRAME_CREATED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Env(Arc::new(Repr::Frame(RwLock::new(EnvInner {
            vars: HashMap::new(),
            parent: self.clone(),
        }))))
    }

    /// Breaks the `Closure`<->`Env` strong-`Arc` cycle a retiring `let`/
    /// `loop` frame would otherwise leak forever.
    ///
    /// `eval_let`/`eval_loop` create ONE child frame, evaluate every
    /// binding value in it, and bind the results back into it. A `fn` in
    /// binding position therefore captures (via `Closure::env`) the very
    /// frame its own `Value::Fn` lands in: `frame.vars[f] -> Arc<Closure>`
    /// and `Closure.env -> Arc<Repr::Frame>` close a strong reference
    /// cycle, and this interpreter is precise-RC with no cycle collector
    /// (`ARCHITECTURE.md`), so neither end is ever reclaimed. That is the
    /// whole leak; `tests/leak_cycle_probe.rs` measures it.
    ///
    /// The fix is a conservative *island* check run when the frame is
    /// retired (see [`FrameGuard`], which owns the sole `Env` handle at
    /// that moment -- that is what makes the arithmetic below exact):
    ///
    /// * every direct `Value::Fn`/`Value::Macro` in `vars` whose `env` IS
    ///   this frame is a back-reference edge. Tallied by `Arc` identity,
    ///   because one closure may legitimately sit in the map more than
    ///   once -- `eval_loop` binds each value under BOTH its user-facing
    ///   name and an internal `__loopN` symbol. A closure whose
    ///   `strong_count` exceeds its occurrences here is also held
    ///   somewhere outside this map -- it escaped, and we do nothing;
    /// * the frame itself must then have exactly `1 + backrefs` handles
    ///   (`backrefs` = DISTINCT such closures, each contributing one `env`
    ///   edge): the caller's one, plus one per back-referencing closure.
    ///
    /// When both hold, the frame and those closures form a cluster nothing
    /// else in the process can reach: no other value, no global, no other
    /// thread. Clearing `vars` is therefore invisible to the program and
    /// severs every cycle edge at once, letting ordinary `Arc` drops
    /// reclaim the lot. Any escape at all -- the closure returned as the
    /// body's result, stored into a global or an atom, captured
    /// transitively inside a collection or a `delay`, reachable through a
    /// nested frame's parent edge, or shared with another thread --
    /// inflates one of the two counts, the match fails, and we leave
    /// everything exactly as it is today (the leak persists only for
    /// genuinely escaped cycles). Deliberately out of scope: closures
    /// buried inside collections, which the count mismatch skips safely.
    ///
    /// On `strong_count` racing: a count that MATCHES proves exclusivity
    /// (any concurrent holder would only inflate it), and a stale-high read
    /// can only cause a safe skip -- so there is no window in which this
    /// clears a frame someone else can see.
    pub(crate) fn break_frame_cycles(&self) {
        let Repr::Frame(l) = &*self.0 else { return };
        // Fast path: nobody else holds this frame, so plain RC reclaims it.
        if Arc::strong_count(&self.0) == 1 {
            return;
        }
        // (closure identity, times it appears in `vars`, its strong count).
        // Frames are tiny, so a linear scan beats allocating a hash map --
        // and the `strong_count == 1` fast path above already skipped every
        // frame that binds no self-capturing closure at all.
        let mut seen: Vec<(*const crate::value::Closure, usize, usize)> = Vec::new();
        {
            let inner = lock_read(l);
            for v in inner.vars.values() {
                let c = match v {
                    Value::Fn(c) | Value::Macro(c) => c,
                    _ => continue,
                };
                if !Arc::ptr_eq(&c.env.0, &self.0) {
                    continue;
                }
                let p = Arc::as_ptr(c);
                match seen.iter_mut().find(|(q, _, _)| *q == p) {
                    Some(e) => e.1 += 1,
                    None => seen.push((p, 1, Arc::strong_count(c))),
                }
            }
            for &(_, occurrences, strong) in &seen {
                // Held anywhere besides this frame's own vars map ->
                // escaped; the island argument does not apply.
                if strong != occurrences {
                    return;
                }
            }
        }
        let backrefs = seen.len();
        if backrefs == 0 {
            return;
        }
        if Arc::strong_count(&self.0) != 1 + backrefs {
            return;
        }
        // Take the map out under the lock and let it drop AFTER the write
        // guard is released (the guard is a temporary of this `let`, so it
        // dies first): clearing in place would run every value's destructor
        // -- arbitrary user data -- while still holding this frame's lock.
        let severed = std::mem::take(&mut lock_write(l).vars);
        drop(severed);
    }

    /// field4/W-LENS-1 gauge: how many superseded root maps are parked on
    /// the retire list, freed only in `RootGlobals::drop` (see the
    /// publication-pattern doc above -- this is the design's ONE real
    /// liability, and a liability you cannot observe in production is only
    /// half-priced). Walks to the root, so any frame answers. `0` for an
    /// env whose root has never been re-published.
    pub fn retired_root_maps(&self) -> u64 {
        match &*self.0 {
            Repr::Root(g) => lock_mutex(&g.writer).len() as u64,
            Repr::Frame(f) => lock_read(f).parent.retired_root_maps(),
        }
    }

    /// True for the globals frame (the one `set` interns cells in). The
    /// compiled-fn tier's capture rule keys off this: a closure created
    /// directly under the root has no intermediate frames to capture from,
    /// so every free symbol of its body is a global.
    pub fn is_root(&self) -> bool {
        matches!(*self.0, Repr::Root(_))
    }

    /// "Is this name bound by some enclosing `let`/fn frame?", ignoring
    /// globals. Used by the compiled-fn tier to snapshot a closure's
    /// captures at creation time, and by `Interp::resolve_symbol` as the
    /// tree-walker's lexical probe before it falls through to
    /// `lookup_global`.
    ///
    /// A namespace/alias-qualified symbol never matches here, full stop: a
    /// binding form is always a bare symbol (Clojure has no syntax for
    /// destructuring into a qualified name), so `mode/state` inside
    /// `(defn f [state] (mode/state state :x))` must skip the `state`
    /// param entirely and reach the global `mode/state` -- exactly like
    /// real Clojure's `(let [state 1] some.ns/state)`, which never sees the
    /// local either. `Scope::lookup` in `compile/resolve.rs` enforces the
    /// identical rule at compile time, so the two tiers cannot drift here.
    pub fn get_local(&self, sym: &Symbol) -> Option<Value> {
        if sym.ns.is_some() {
            return None;
        }
        let Repr::Frame(l) = &*self.0 else {
            // The root holds globals, which by definition are not locals.
            return None;
        };
        let inner = lock_read(l);
        if let Some(v) = inner.vars.get(sym) {
            return Some(v.clone());
        }
        // Recursing while still holding this frame's guard is safe for the
        // same reason `get` documents below: a distinct `Env` with its own
        // lock, always in the child->parent direction, so no cycle exists.
        inner.parent.get_local(sym)
    }

    /// clojure-lsp campaign (mova/PLAN.md): the write-side sibling of
    /// [`get_local`](Self::get_local) -- walks the lexical (Frame) chain
    /// for the frame that ALREADY binds `sym` and overwrites it there IN
    /// PLACE, returning `true`. Returns `false` without touching
    /// anything once the walk reaches the root (globals are `set!`'s
    /// OTHER path, `eval_set_bang`'s existing var-cell lookup -- this
    /// method never allocates a fresh binding in a frame that lacks one,
    /// which is what would happen if it were built on `Env::set`
    /// instead).
    ///
    /// Exists for exactly one caller: `set!` on a mutable `deftype`
    /// field's local (`wrap_fields_let`'s `f -> (.f this)` binding).
    /// Real Clojure has no OTHER local `set!` target (an ordinary `let`/
    /// fn-param local is never assignable -- "Cannot assign to non-
    /// mutable local" -- so this is deliberately never exposed as a
    /// general local-mutation primitive; `eval_set_bang` calls it only
    /// after confirming `sym` names a mutable field via `TypeDef::
    /// mutable`).
    pub(crate) fn set_local_in_place(&self, sym: &Symbol, val: Value) -> bool {
        let Repr::Frame(l) = &*self.0 else {
            return false;
        };
        let mut inner = lock_write(l);
        if inner.vars.contains_key(sym) {
            inner.vars.insert(sym.clone(), val);
            return true;
        }
        // Drop the write guard before recursing: the parent is a distinct
        // `Env`/lock in the child->parent direction, so holding this one
        // is not required for safety, and releasing it early keeps this
        // frame lock-free for the (overwhelmingly common) recursive case.
        let parent = inner.parent.clone();
        drop(inner);
        parent.set_local_in_place(sym, val)
    }

    /// Walks the parent chain looking up `sym` by its full (ns-qualified)
    /// name; if `sym` has a namespace, also tries the bare name at each
    /// level. Namespace-aware GLOBAL resolution (alias expansion, the
    /// current ns, refers) is `Interp::lookup_global`'s job, not this
    /// method's -- `get` is the plain lexical-chain probe the evaluator uses
    /// for locals, and the bare-name retry survives only because a
    /// qualified name that resolves to a bare cell is exactly how `flow/*`
    /// and `clojure.string/*` are wired (see `crate::ns`).
    /// A `Binding::Var` whose cell is unbound (`None`) is treated the same
    /// as an absent entry, so lookup falls through exactly as if the
    /// binding weren't there at all.
    pub fn get(&self, sym: &Symbol) -> Option<Value> {
        match &*self.0 {
            Repr::Root(g) => {
                // An interned-but-unbound cell (`VarCell::get` -> `None`)
                // is treated exactly like an absent entry, so lookup falls
                // through to the bare-name retry just as it always did.
                let m = g.map();
                if let Some(v) = m.get(sym).and_then(|c| c.0.get()) {
                    return Some(v);
                }
                if sym.ns.is_some() {
                    let bare = Symbol::simple(sym.name.clone());
                    if let Some(v) = m.get(&bare).and_then(|c| c.0.get()) {
                        return Some(v);
                    }
                }
                None
            }
            Repr::Frame(l) => {
                let inner = lock_read(l);
                if let Some(v) = inner.vars.get(sym) {
                    return Some(v.clone());
                }
                if sym.ns.is_some() {
                    let bare = Symbol::simple(sym.name.clone());
                    if let Some(v) = inner.vars.get(&bare) {
                        return Some(v.clone());
                    }
                }
                // A distinct `Env` (its own lock), so recursing here while
                // still holding `inner`'s read guard cannot deadlock.
                inner.parent.get(sym)
            }
        }
    }

    /// Looks `sym` up by its EXACT spelling only -- no bare-name retry for a
    /// qualified symbol, which is `get`'s v0 stand-in for real namespaces.
    /// This is the primitive `crate::ns`'s resolution order is built out of:
    /// it decides the candidate spellings itself and probes each one here,
    /// so no probe may quietly widen into another.
    pub fn get_exact(&self, sym: &Symbol) -> Option<Value> {
        match &*self.0 {
            // THE hot read of the whole interpreter: one `Acquire` load,
            // one CHAMP probe, one cell read. No lock.
            Repr::Root(g) => g.map().get(sym).and_then(|c| c.0.get()),
            Repr::Frame(l) => {
                let inner = lock_read(l);
                if let Some(v) = inner.vars.get(sym) {
                    return Some(v.clone());
                }
                inner.parent.get_exact(sym)
            }
        }
    }

    /// Defines/rebinds `sym` in *this* env frame (not the parent chain).
    ///
    /// On the ROOT frame this get-or-interns the symbol's `Arc<VarCell>`
    /// and writes `val` *through* that cell (preserving its identity across
    /// redefinition — see the module doc comment), clearing
    /// `pristine_builtin` since this is an ordinary (non-builtin) write. On
    /// any other frame this is a plain, allocation-cheap `Local` insert,
    /// unchanged from pre-S1 behavior.
    pub fn set(&self, sym: Symbol, val: Value) {
        match &*self.0 {
            Repr::Root(g) => {
                // Genuine (`speculative: false`): a root write is a `def`
                // by any other name -- and the `store` below would make
                // `is_speculative` answer `false` regardless.
                g.get_or_intern(&sym, false).store(val, false);
            }
            Repr::Frame(l) => {
                lock_write(l).vars.insert(sym, val);
            }
        }
    }

    /// Root-only: get-or-creates `sym`'s `Arc<VarCell>`, unbound (`None`)
    /// if newly created. Called on a non-root env, walks up to the root
    /// first. This is the hook the future compiler uses to resolve a
    /// global symbol to its cell once and hold the `Arc` from then on
    /// (including resolving a forward reference before its `def` runs).
    ///
    /// field3: this is the GENUINE path -- `def`/`declare`/`intern`/
    /// `refer`/boot registration -- so it also promotes a cell the
    /// compiler had only interned speculatively
    /// ([`VarCell::speculative`]).
    pub fn intern(&self, sym: &Symbol) -> Arc<VarCell> {
        match &*self.0 {
            Repr::Root(g) => g.get_or_intern(sym, false),
            Repr::Frame(l) => {
                let parent = lock_read(l).parent.clone();
                parent.intern(sym)
            }
        }
    }

    /// field3: [`Self::intern`] for the COMPILER's candidate walk
    /// (`compile::resolve::global_chain`) -- a cell this creates is marked
    /// [`speculative`](VarCell::speculative), i.e. "a resolution candidate
    /// that no `def` has ever reached", so `binding`'s
    /// interned-or-bound lookup ([`Self::find_any_cell`]) can tell it apart
    /// from a genuine namespace mapping. An ALREADY-interned cell comes
    /// back untouched: probing for a candidate must never demote a real
    /// var.
    pub fn intern_speculative(&self, sym: &Symbol) -> Arc<VarCell> {
        match &*self.0 {
            Repr::Root(g) => g.get_or_intern(sym, true),
            Repr::Frame(l) => {
                let parent = lock_read(l).parent.clone();
                parent.intern_speculative(sym)
            }
        }
    }

    /// Like `get_exact`, but returns the CELL itself (not a snapshot of its
    /// value), and only when the cell is actually bound -- an
    /// interned-but-unbound placeholder (a forward reference, or a
    /// namespace candidate no `def` has reached yet) is treated as absent,
    /// so callers fall through to the next candidate exactly as
    /// `get_exact`'s value-returning cousin does. No bare-name retry for a
    /// qualified symbol, matching `get_exact`. Used by `(var x)`/`#'x`
    /// resolution (`crate::ns::Interp::resolve_var_cell`), which needs the
    /// `Arc<VarCell>` identity itself, not a value read through it.
    /// C3c (rt.clj's `ns-intern-policies`, `.refer`'s dot-surface):
    /// binds `sym` DIRECTLY to an already-existing `cell` -- unlike
    /// `intern` (which get-or-CREATES a fresh cell for `sym`), this
    /// aliases `sym` to a cell that may already carry a completely
    /// different name (real `Namespace.refer(Symbol, Var)`'s exact
    /// shape: any var, from anywhere, can be mapped under any symbol in
    /// any namespace's mapping table -- e.g. `(.refer ns 'flatten v1)`
    /// where `v1` is actually `#'ns/foo`, an existing var with a
    /// DIFFERENT bare name than `flatten`). Root-only (recurses to the
    /// root first, like `intern`/`set`), and unconditional: whatever
    /// `sym` used to map to (nothing, a core refer, another alias) is
    /// simply overwritten -- callers needing "reject if already
    /// interned"/"warn if replacing a refer" (`.refer`'s own policy)
    /// decide that BEFORE calling this, by consulting `find_bound_cell`/
    /// `ns_resolve_in` first; this method itself has no policy at all.
    pub fn bind_alias(&self, sym: Symbol, cell: Arc<VarCell>) {
        match &*self.0 {
            Repr::Root(g) => g.bind_alias(sym, cell),
            Repr::Frame(l) => {
                let parent = lock_read(l).parent.clone();
                parent.bind_alias(sym, cell)
            }
        }
    }

    pub fn find_bound_cell(&self, sym: &Symbol) -> Option<Arc<VarCell>> {
        match &*self.0 {
            // W-DECL: "bound" here means "currently resolves to a value"
            // (`cell.get().is_some()`), matching the JVM's own
            // `Var.isBound()` (`hasRoot() || (dynamic &&
            // getThreadBinding() != null)`), NOT just "has a root value"
            // (the old `c.0.is_bound()` check, which is root-only). Before
            // this task the two were indistinguishable: `binding` itself
            // required a root-bound cell to push a frame at all
            // (`resolve_binding_pairs` used this SAME method), so
            // "unbound root, live dynamic frame" was unreachable. Now
            // that `binding` can push a frame onto a `declare`d-but-never-
            // `def`d dynamic var (`Env::find_any_cell`), an ordinary
            // value read of that var WHILE the frame is live -- e.g.
            // `def.clj`'s `nested-dynamic-declaration`'s `(defn q [] @p)`
            // -- must see it too: `find_bound_cell` backs `resolve`,
            // `lookup_global`/`lookup_global_checked` (bare symbol reads),
            // and `resolve_var_cell`'s first probe, so leaving this on
            // root-only would have made `binding` a write-only op for a
            // var this shape -- readable through a compiled closure's own
            // pre-resolved `VarCell` handle (which never re-probes this
            // method), but "Unable to resolve symbol" from a plain
            // top-level symbol read in the very same scope. Cannot
            // regress anything: the state this widens (unbound root +
            // live dynamic frame) could not exist before `find_any_cell`.
            Repr::Root(g) => g.map().get(sym).and_then(|c| {
                if c.0.get().is_some() {
                    Some(c.0.clone())
                } else {
                    None
                }
            }),
            // A lexical frame holds no cells at all (see `EnvInner`), so a
            // local named `sym` neither answers nor SHADOWS here -- the
            // walk continues to the root exactly as it did when the frame
            // could only ever contain a `Binding::Local` that this
            // method's `Binding::Var` pattern declined to match.
            Repr::Frame(l) => {
                let inner = lock_read(l);
                inner.parent.find_bound_cell(sym)
            }
        }
    }

    /// W-DECL: like [`find_bound_cell`], but answers for an
    /// interned-but-UNBOUND cell too -- the JVM's `Namespace.getMapping`
    /// (any existing `Var`, bound or not, is a legitimate `binding`
    /// target: real Clojure's `Var.pushThreadBindings` only ever checks
    /// `v.dynamic`, never `v.hasRoot()`). `(declare ^:dynamic p) (binding
    /// [p 1] p)` needs exactly this: `p`'s cell exists (interned by
    /// `declare`'s `(def p)`) but has no root value, so `find_bound_cell`
    /// -- which treats "unbound" as "absent", the right call for an
    /// ordinary VALUE read -- made `binding` see no candidate at all and
    /// fail with "Unable to resolve symbol". `binding`'s own
    /// `resolve_binding_pairs` is the ONLY caller: an ordinary symbol
    /// VALUE read (`lookup_global_checked`/`resolve_var_cell`) must keep
    /// using `find_bound_cell`, since reading a genuinely unbound var is
    /// its own (documented, `src/builtins/atoms.rs:346`) unimplemented-
    /// sentinel gap, not this one.
    ///
    /// field3 (W-DECL integration fix): "interned" here means interned by
    /// something that is actually a namespace MAPPING -- a
    /// compiler-speculative candidate placeholder
    /// ([`VarCell::speculative`], see that field for the measured
    /// `*ns*`-splitting fallout) is skipped, exactly as it was before
    /// W-DECL, when `find_bound_cell`'s boundness gate happened to filter
    /// it out for free. Everything a `def`/`declare`/`intern`/`refer`
    /// created still answers, bound or not, which is the whole point of
    /// this method.
    ///
    /// field4 (f4/ns residue, closed): a second caller besides `binding`'s
    /// `resolve_binding_pairs` now reads through this gate --
    /// `try_resolve_var_cell`, which backs `resolve`/`ns-resolve` (see its
    /// doc for the oracle-measured "returns the Var for any genuine
    /// mapping, bound or not, private or not" contract).
    ///
    /// [`find_bound_cell`]: Self::find_bound_cell
    pub fn find_any_cell(&self, sym: &Symbol) -> Option<Arc<VarCell>> {
        match &*self.0 {
            Repr::Root(g) => g
                .map()
                .get(sym)
                .filter(|c| !c.0.is_speculative())
                .map(|c| c.0.clone()),
            Repr::Frame(l) => {
                let inner = lock_read(l);
                inner.parent.find_any_cell(sym)
            }
        }
    }

    /// Calls `f` for every genuinely interned cell (not a compiler-speculative
    /// placeholder), across the parent chain. One pass over the table, no
    /// allocation: what the nREPL `completions` op scans.
    pub fn for_each_cell(&self, f: &mut dyn FnMut(&Symbol, &Arc<VarCell>)) {
        match &*self.0 {
            Repr::Root(g) => {
                for (sym, cell) in g.map().iter() {
                    if !cell.0.is_speculative() {
                        f(sym, &cell.0);
                    }
                }
            }
            Repr::Frame(l) => lock_read(l).parent.for_each_cell(f),
        }
    }

    /// Every namespace name that appears in one of this env's bindings --
    /// i.e. the namespaces the interpreter provides NATIVELY
    /// (`clojure.string`, `flow`, ...), since only `register_all` interns
    /// qualified symbols at boot. `crate::ns` seeds its loaded-set from
    /// this, so `(:require [clojure.string :as str])` records the alias
    /// instead of hunting the module path for a file that cannot exist.
    // `Str`'s cache fields (ASCII/char-count) are `Atomic*`, which trips
    // clippy's `mutable_key_type` lint on any `HashSet<Str>`/`HashMap<Str,
    // _>` -- but `Str`'s `Hash`/`Eq` are derived purely from its immutable
    // `text` (value.rs), never from the cache, so the lint's premise
    // (mutation could invalidate a key's hash bucket) doesn't apply here.
    #[allow(clippy::mutable_key_type)]
    pub fn interned_namespaces(&self) -> std::collections::HashSet<crate::value::Str> {
        match &*self.0 {
            Repr::Root(g) => g.map().keys().filter_map(|sym| sym.ns.clone()).collect(),
            Repr::Frame(l) => {
                let inner = lock_read(l);
                let mut out: std::collections::HashSet<crate::value::Str> = inner
                    .vars
                    .keys()
                    .filter_map(|sym| sym.ns.clone())
                    .collect();
                out.extend(inner.parent.interned_namespaces());
                out
            }
        }
    }

    /// Every var name interned under namespace `ns`, across the whole
    /// parent chain. Backs `:refer :all` (special_forms.rs): the target
    /// namespace is always fully loaded *before* its refer options are
    /// applied (`eval_require_spec` requires first), so by the time this
    /// runs, "everything interned under `ns`" IS the namespace's def set.
    /// mova has no private vars yet (`^:private` metadata is parsed and
    /// discarded by the reader), so all defs are public -- when privacy
    /// lands (M3 metadata), this must learn to skip private vars.
    ///
    /// C3h (clojure.repl surface): [`crate::ns::CORE_NS`] is special --
    /// every Rust-native builtin and every `core/*.mova` bootstrap def
    /// interns BARE (`sym.ns == None`, see `crate::ns`'s module doc: "the
    /// bare (unqualified) names belong to ONE namespace, `CORE_NS`"), not
    /// qualified as `clojure.core/name`. A plain `sym.ns == Some(ns)`
    /// filter therefore saw ZERO names for `clojure.core` no matter how
    /// many hundreds of builtins exist (measured: `(ns-publics 'clojure.
    /// core)` was `{}`), which is what made `apropos`/`dir-fn` -- both
    /// built on `ns-publics` -- silently empty for the one namespace most
    /// worth introspecting. Folding bare vars in for that one namespace
    /// name fixes the enumeration; `nsfns::var_symbol_in` is the matching
    /// fix on the LOOKUP side (a name found this way must be looked back
    /// up by its bare spelling too, not `clojure.core/name`).
    pub fn names_in_ns(&self, ns: &crate::value::Str) -> Vec<crate::value::Str> {
        let core = ns.as_ref() == crate::ns::CORE_NS;
        let pick = |sym: &Symbol| sym.ns.as_ref() == Some(ns) || (core && sym.ns.is_none());
        match &*self.0 {
            // clojure-lsp campaign (mova/PLAN.md): `RootCell::0.is_
            // speculative()` excludes a cell that only exists because
            // the COMPILER's own resolution walk speculatively interned
            // it as a candidate (`Env::intern_speculative`'s doc) --
            // never a genuine `def`/`declare`/`refer` in `ns`. Measured
            // real bug this fixes: a namespace `y` whose body merely
            // CALLS a name it refers in from elsewhere (`(defn helper []
            // (read-char nil))`, `read-char` refer'd from `clojure.tools.
            // reader.reader-types`) got a speculative `y/read-char` cell
            // interned during resolution, and without this filter
            // `names_in_ns("y")` reported `read-char` as one of `y`'s OWN
            // names -- so `(:require [z :refer :all])`-ing `y` from a
            // THIRD namespace silently clobbered that third namespace's
            // OWN, unrelated, already-correct `read-char` refer with a
            // second (harmless-looking but wrong) one, and separately
            // corrupted `ns-publics`/`ns-interns`/`clojure.tools.reader.
            // impl.commons`'s own `:refer :all` (this campaign's actual
            // trigger: `clojure.tools.reader.edn` -> `clojure.tools.
            // reader.impl.commons`, which itself only refers `read-char`/
            // `peek-char`/`numeric?` from elsewhere).
            Repr::Root(g) => g
                .map()
                .iter()
                .filter(|(sym, cell)| pick(sym) && !cell.0.is_speculative())
                .map(|(sym, _)| sym.name.clone())
                .collect(),
            Repr::Frame(l) => {
                let inner = lock_read(l);
                let mut out: Vec<crate::value::Str> = inner
                    .vars
                    .keys()
                    .filter(|sym| pick(sym))
                    .map(|sym| sym.name.clone())
                    .collect();
                out.extend(inner.parent.names_in_ns(ns));
                out
            }
        }
    }

    /// Every currently-INTERNED var cell whose bare name (ignoring
    /// namespace entirely) equals `name`, across every namespace, in
    /// unspecified but deterministic-per-run order.
    ///
    /// W4-SHIM (2026-08-21): backs `builtins::nsfns::write_shim_err`'s
    /// fallback path. That fn's ordinary lookup is namespace-qualified
    /// (`<current-ns>/*err*`), which breaks for a warning raised from
    /// INSIDE `mova-test-helper-shim.mova`'s `eval-in-temp-ns`: that macro
    /// switches `current_ns` to a freshly gensym'd, completely bare
    /// namespace that was never spliced with the shim at all, so it has
    /// no `*err*` cell of its own under any name -- measured directly
    /// (`(with-err-print-writer (eval-in-temp-ns (def *hello* "hi"))))`
    /// returned `""` instead of the real warning text, even though the
    /// SAME probe with no `eval-in-temp-ns` wrapper correctly captured
    /// it). The dynamic BINDING itself is not the problem -- `binding`
    /// pushes onto a specific `Arc<VarCell>` by identity
    /// (`eval_binding`/`resolve_binding_pairs`), which stays pushed for
    /// the whole dynamic extent regardless of which namespace is
    /// "current" -- the problem is that nothing at the write site had a
    /// way to REACH that cell once `current_ns` no longer names the
    /// namespace it lives in. This walks every namespace's own `*err*`
    /// cell (the shim splices one per namespace, module doc in
    /// `mova-test-shim.mova`) so the caller can pick whichever one is
    /// CURRENTLY bound to something (i.e. genuinely being captured right
    /// now) -- mirroring real Clojure, where there is only ONE `*err*` in
    /// the whole process, reachable from anywhere, so "whichever one is
    /// live" is never ambiguous there either.
    pub fn find_var_cells_named(&self, name: &crate::value::Str) -> Vec<Arc<VarCell>> {
        match &*self.0 {
            Repr::Root(g) => g
                .map()
                .values()
                .filter(|c| &c.0.name.name == name)
                .map(|c| c.0.clone())
                .collect(),
            // Lexical frames hold no cells (see `EnvInner`) -- the
            // pre-W-ENV `Binding::Var` filter here could only ever match on
            // the root, so walking straight past a frame is the same
            // answer, not a narrowed one.
            Repr::Frame(l) => {
                let inner = lock_read(l);
                inner.parent.find_var_cells_named(name)
            }
        }
    }

    /// Boot-time-only sibling of `set`: like a root `set`, but marks the
    /// cell `pristine_builtin = true` instead of clearing it. Used
    /// exclusively by `builtins::register_all`'s native-fn registration
    /// path (`builtins::reg`, `builtins::flow::reg_flow`, `strings::alias`)
    /// so only an untouched Rust native is ever "pristine" -- a
    /// `core.mova`-defined fn (loaded via ordinary `def` right after
    /// `register_all`) goes through `set`, correctly landing as non-
    /// pristine, and a user `def` that shadows a builtin correctly clears
    /// pristine on the *same* cell (see the "cell identity preserved
    /// across redefinition" test).
    #[track_caller]
    pub fn set_builtin(&self, sym: Symbol, val: Value) {
    crate::srcindex::note(&sym);
        self.intern(&sym).store(val, true);
    }

    /// `set_builtin` for an alias of the bare native `from`: the source index reports the original's location.
    pub fn set_builtin_alias(&self, sym: Symbol, val: Value, from: &str) {
        crate::srcindex::note_alias(&sym, from);
        self.intern(&sym).store(val, true);
    }

    /// A deep, independent copy of this env frame for `Interp::snapshot`'s
    /// FORK semantics (as opposed to `Interp::fork`'s deliberate SHARING):
    /// every `Binding::Var(Arc<VarCell>)` is re-wrapped around a **new**
    /// `Arc<VarCell>` holding a copy of the cell's current value (see
    /// `VarCell::snapshot`), so a `def`/redefinition written through the
    /// clone's cell afterwards does not reach the original's cell (they no
    /// longer share the `Arc`), and vice versa. `Binding::Local` values are
    /// cloned too (cheap -- every `Value` variant is `Copy` or `Arc`-backed)
    /// for completeness, though in practice this is only ever called on the
    /// root globals frame, which `Env::set`'s doc guarantees holds nothing
    /// but `Binding::Var` entries. Only intended to be called when nothing
    /// is concurrently running against the `Interp` being snapshotted (see
    /// `Interp::snapshot`'s doc) -- a concurrent `def` mid-snapshot is not a
    /// memory-safety hazard (each cell's own lock still protects it) but its
    /// visibility in the snapshot is unspecified: it may or may not make it
    /// in, per cell.
    ///
    /// Parent chains are never expected here (the globals frame is always
    /// root, `parent: None`); a non-root caller still gets a working
    /// snapshot by recursing, but that path is untested since nothing in
    /// this crate takes it today.
    // Same non-issue as `interned_namespaces`' own allow: `Symbol`/`Str`'s
    // `Hash`/`Eq` never read the interior-mutable cache fields, so
    // rebuilding a `HashMap<Symbol, _>` via `collect` here is safe despite
    // the lint.
    #[allow(clippy::mutable_key_type)]
    pub fn snapshot(&self) -> Self {
        match &*self.0 {
            Repr::Root(g) => {
                let mut m = RootMap::new();
                for (sym, cell) in g.map().iter() {
                    m = m.assoc_owned(sym.clone(), RootCell(cell.0.snapshot()));
                }
                Env(Arc::new(Repr::Root(RootGlobals::with_map(m))))
            }
            Repr::Frame(l) => {
                let inner = lock_read(l);
                let vars = inner.vars.clone();
                let parent = inner.parent.snapshot();
                #[cfg(feature = "leak-probe")]
                FRAME_CREATED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Env(Arc::new(Repr::Frame(RwLock::new(EnvInner {
                    vars,
                    parent,
                }))))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::Str;

    #[test]
    fn child_sees_parent_bindings() {
        let root = Env::new_root();
        root.set(Symbol::simple("x"), Value::Int(1));
        let child = root.child();
        assert_eq!(child.get(&Symbol::simple("x")), Some(Value::Int(1)));
    }

    #[test]
    fn set_defines_in_this_env_only() {
        let root = Env::new_root();
        let child = root.child();
        child.set(Symbol::simple("x"), Value::Int(1));
        assert_eq!(child.get(&Symbol::simple("x")), Some(Value::Int(1)));
        assert_eq!(root.get(&Symbol::simple("x")), None);
    }

    #[test]
    fn shadowing_prefers_innermost() {
        let root = Env::new_root();
        root.set(Symbol::simple("x"), Value::Int(1));
        let child = root.child();
        child.set(Symbol::simple("x"), Value::Int(2));
        assert_eq!(child.get(&Symbol::simple("x")), Some(Value::Int(2)));
        assert_eq!(root.get(&Symbol::simple("x")), Some(Value::Int(1)));
    }

    // clojure-lsp campaign (mova/PLAN.md): `names_in_ns` must not report a
    // SPECULATIVE cell (the compiler's own forward-reference resolution
    // candidate, `intern_speculative`) as one of a namespace's real
    // names. Measured real bug: a namespace `y` whose body merely calls
    // a name it refers in from elsewhere got a speculative `y/read-char`
    // cell, and a THIRD namespace's `(:require [y :refer :all])` picked
    // it up as if `y` genuinely defined `read-char` -- silently
    // clobbering that third namespace's own, unrelated, correct
    // `read-char` refer (`clojure.tools.reader.impl.commons`'s own
    // `:refer :all`, reached transitively from `rewrite-clj.reader`, hit
    // exactly this).
    #[test]
    fn names_in_ns_excludes_a_speculative_cell_but_includes_a_genuine_one() {
        let root = Env::new_root();
        let y = Str::from("y");
        let speculative_sym = Symbol { ns: Some(y.clone()), name: Str::from("read-char") };
        let genuine_sym = Symbol { ns: Some(y.clone()), name: Str::from("helper") };
        // Mirrors what the compiler's own resolution walk does for a free
        // symbol it has not yet proven resolves elsewhere: intern a
        // candidate cell under the CURRENT namespace, unbound.
        root.intern_speculative(&speculative_sym);
        // A real `defn`/`def` in `y`: genuinely bound.
        root.set(genuine_sym.clone(), Value::Int(1));
        let names = root.names_in_ns(&y);
        assert!(
            !names.contains(&Str::from("read-char")),
            "speculative cell leaked into names_in_ns: {names:?}"
        );
        assert!(
            names.contains(&Str::from("helper")),
            "genuine binding missing from names_in_ns: {names:?}"
        );
    }

    #[test]
    fn namespaced_lookup_falls_back_to_bare_name() {
        let root = Env::new_root();
        root.set(Symbol::simple("y"), Value::Int(42));
        let looked_up = Symbol {
            ns: Some(Str::from("some.ns")),
            name: Str::from("y"),
        };
        assert_eq!(root.get(&looked_up), Some(Value::Int(42)));
    }

    #[test]
    fn cell_identity_preserved_across_redefinition() {
        let root = Env::new_root();
        let sym = Symbol::simple("z");
        // Forward-intern before any `def`.
        let cell = root.intern(&sym);
        assert!(cell.raw_root().is_none());

        root.set(sym.clone(), Value::Int(1));
        assert_eq!(cell.raw_root(), Some(Value::Int(1)));
        assert_eq!(root.get(&sym), Some(Value::Int(1)));

        // Redefine -- the SAME Arc<VarCell> must observe the new value.
        root.set(sym.clone(), Value::Int(2));
        assert_eq!(cell.raw_root(), Some(Value::Int(2)));
        assert_eq!(root.get(&sym), Some(Value::Int(2)));

        // intern() again returns the identical cell.
        let cell2 = root.intern(&sym);
        assert!(Arc::ptr_eq(&cell, &cell2));
    }

    #[test]
    fn unbound_interned_cell_invisible_to_get() {
        let root = Env::new_root();
        let sym = Symbol::simple("forward-ref");
        root.intern(&sym);
        assert_eq!(root.get(&sym), None);
    }

    /// field3 (W-DECL integration fix): the compiler's candidate-chain
    /// intern must not manufacture a namespace mapping. `find_any_cell`
    /// (`binding`'s interned-or-bound lookup) answers for a genuine
    /// unbound var -- `(declare ^:dynamic p)` -- and declines a
    /// speculative placeholder, which is what kept `(binding [*ns* ...])`
    /// from splitting off onto a `current.ns/*ns*` phantom. See
    /// `VarCell::speculative`.
    #[test]
    fn speculative_intern_is_not_a_namespace_mapping() {
        let root = Env::new_root();
        let sym = Symbol::simple("candidate-only");

        // Compiler-interned: exists in the table, but is not a mapping.
        let cell = root.intern_speculative(&sym);
        assert!(cell.is_speculative());
        assert!(root.find_any_cell(&sym).is_none());
        assert!(root.find_bound_cell(&sym).is_none());

        // A genuine `def`/`declare` through the SAME name promotes the
        // SAME cell (forward reference: identity must be preserved, or
        // every compiled read of it would go on seeing an empty phantom).
        let genuine = root.intern(&sym);
        assert!(Arc::ptr_eq(&cell, &genuine));
        assert!(!cell.is_speculative());
        assert!(root.find_any_cell(&sym).is_some());
        // Still unbound: `declare` interns without binding, and
        // `find_any_cell` is exactly the lookup that must see it anyway.
        assert!(root.find_bound_cell(&sym).is_none());

        // A speculative probe of an already-genuine cell must not demote
        // it back.
        let reprobed = root.intern_speculative(&sym);
        assert!(Arc::ptr_eq(&cell, &reprobed));
        assert!(!cell.is_speculative());
        assert!(root.find_any_cell(&sym).is_some());
    }

    /// field3: a bound cell is never speculative, whatever the flag says
    /// -- a root value can only have come from a real `def`/`set`.
    #[test]
    fn a_bound_cell_is_never_speculative() {
        let root = Env::new_root();
        let sym = Symbol::simple("later-defd");
        let cell = root.intern_speculative(&sym);
        assert!(cell.is_speculative());
        root.set(sym.clone(), Value::Int(7));
        assert!(!cell.is_speculative());
        assert!(root.find_any_cell(&sym).is_some());
    }

    #[test]
    fn set_builtin_marks_pristine_and_root_set_clears_it() {
        let root = Env::new_root();
        let sym = Symbol::simple("+");
        root.set_builtin(sym.clone(), Value::Int(0));
        let cell = root.intern(&sym);
        assert!(cell.pristine_builtin.load(Ordering::Acquire));

        root.set(sym.clone(), Value::Int(1));
        assert!(!cell.pristine_builtin.load(Ordering::Acquire));
        assert_eq!(root.get(&sym), Some(Value::Int(1)));
    }

    #[test]
    fn intern_on_child_delegates_to_root() {
        let root = Env::new_root();
        let child = root.child();
        let sym = Symbol::simple("w");
        let cell = child.intern(&sym);
        root.set(sym, Value::Int(9));
        assert_eq!(cell.raw_root(), Some(Value::Int(9)));
    }
}

// ---- heap-image gate-1 accessors (src/image.rs; docs/HEAP-IMAGE-DESIGN.md) ----
impl Env {
    pub(crate) fn img_ptr(&self) -> usize {
        Arc::as_ptr(&self.0) as *const u8 as usize
    }
    pub(crate) fn img_is_root(&self) -> bool {
        matches!(&*self.0, Repr::Root(_))
    }
    /// A frame's (vars sorted by symbol text, parent), or `None` for the root.
    pub(crate) fn img_frame(&self) -> Option<(Vec<(Symbol, Value)>, Env)> {
        match &*self.0 {
            Repr::Frame(l) => {
                let g = lock_read(l);
                let mut vars: Vec<(Symbol, Value)> = g.vars.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                vars.sort_by(|a, b| (a.0.ns.as_deref(), &*a.0.name).cmp(&(b.0.ns.as_deref(), &*b.0.name)));
                Some((vars, g.parent.clone()))
            }
            Repr::Root(_) => None,
        }
    }
    pub(crate) fn img_new_frame(parent: Env) -> Env {
        Env(Arc::new(Repr::Frame(RwLock::new(EnvInner { vars: HashMap::new(), parent }))))
    }
    pub(crate) fn img_fill_frame(&self, vars: Vec<(Symbol, Value)>, parent: Env) {
        if let Repr::Frame(l) = &*self.0 {
            let mut g = lock_write(l);
            g.vars = vars.into_iter().collect();
            g.parent = parent;
        }
    }
    /// Root map entries sorted by symbol text (deterministic walk order).
    pub(crate) fn img_root_entries(&self) -> Vec<(Symbol, Arc<VarCell>)> {
        let Repr::Root(r) = &*self.0 else { return Vec::new() };
        let mut v: Vec<(Symbol, Arc<VarCell>)> = r.map().iter().map(|(k, c)| (k.clone(), c.0.clone())).collect();
        v.sort_by(|a, b| (a.0.ns.as_deref(), &*a.0.name).cmp(&(b.0.ns.as_deref(), &*b.0.name)));
        v
    }
    pub(crate) fn img_set_root(&self, entries: Vec<(Symbol, Arc<VarCell>)>) {
        let Repr::Root(r) = &*self.0 else { return };
        let next: RootMap = entries.into_iter().map(|(k, c)| (k, RootCell(c))).collect();
        let mut retired = lock_mutex(&r.writer);
        r.publish(&mut retired, next);
        bump_global_generation();
    }
}

impl VarCell {
    /// (bound, pristine_builtin, speculative)
    pub(crate) fn img_flags(&self) -> (bool, bool, bool) {
        (
            self.bound.load(Ordering::Relaxed),
            self.pristine_builtin.load(Ordering::Acquire),
            self.speculative.load(Ordering::Relaxed),
        )
    }
    pub(crate) fn img_meta(&self) -> Value {
        lock_read(&self.meta).clone()
    }
    pub(crate) fn img_fill(&self, value: Option<Value>, flags: (bool, bool, bool), meta: Value) {
        self.value.store(value.map(Arc::new));
        self.bound.store(flags.0, Ordering::Relaxed);
        self.pristine_builtin.store(flags.1, Ordering::Release);
        self.speculative.store(flags.2, Ordering::Relaxed);
        *lock_write(&self.meta) = meta;
    }
}
