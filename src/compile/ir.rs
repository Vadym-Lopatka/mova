//! The compiled-tier instruction tree (`Ir`): a *resolved* mirror of the
//! subset of mova's forms stage S2 compiles.
//!
//! Invariants every node here relies on:
//!
//! - **Symbols are already resolved.** A local is a `u16` slot index into
//!   the call's `slots` Vec; a captured free variable is an index into the
//!   instance's `captures`; a global is a `GlobalChain` of `Arc<VarCell>`s
//!   interned ONCE at compile time (so a later `def` writing *through* one
//!   of those cells is picked up, and a forward reference compiled before
//!   its `def` still late-binds correctly -- that is exactly why `env.rs`
//!   keeps cell identity stable across redefinition).
//! - **Spans are carried on every node that can fail**, and are the same
//!   spans the tree-walker would have attached, so error output (message,
//!   caret position, label, stack) is byte-identical between tiers.
//! - **Evaluation order matches the tree-walker exactly**: callee before
//!   args, `let` inits in order, map literal key before value, and so on.
//!   `exec.rs` must not "optimize" any of that.
//! - **Nothing here allocates a binding environment.** `Let`/`Loop` write
//!   into pre-allocated slots of the current frame; `Recur` writes into the
//!   scratch block its target reserved (`scratch_base`).
//!
//! Stage S4 completes the node set: `CompiledPattern` (destructuring),
//! `MakeClosure` (nested `fn`), `Try` and `Def`. What still makes a whole fn
//! fall back is listed in `resolve.rs`'s compile/fallback matrix.

use std::sync::Arc;

use super::CompiledFn;
use crate::builtins::numbers::Num;
use crate::env::VarCell;
use crate::reader::Span;
use crate::value::{Arity, Str, Symbol, Value};

/// One symbol's global resolution, resolved to CELLS at compile time
/// (v0.5 / R1). `crate::ns` defines the candidate order -- typically
/// `current-ns/name` then the bare core name -- and this is that order,
/// interned: read `head`, and while it is unbound try each of `rest` in
/// turn. Identical to what the tree-walker's `Interp::lookup_global` does
/// per access, because both walk `Interp::for_each_global_candidate`.
///
/// The walk stops at the first candidate that was ALREADY BOUND when the fn
/// was compiled: a var is never unbound again, so no later candidate could
/// ever be reached. `One` is therefore what a namespace calling its own
/// fns (and everything compiled in `clojure.core`) gets -- exactly one cell
/// read, as before namespaces existed. `Two` is the shadowed shape -- any
/// namespace calling a core fn -- and costs one extra lock-free `is_bound`
/// check. Both are stored INLINE rather than behind the `Many` slice: an
/// extra dependent load in front of the arithmetic intrinsics' guard cost
/// ~13% on a tight compiled loop.
#[derive(Clone)]
pub enum GlobalChain {
    One(Arc<VarCell>),
    Two(Arc<VarCell>, Arc<VarCell>),
    /// Three or more candidates (a `:refer`red name that resolves past both
    /// its own namespace and the refer target). Never empty, and never
    /// shorter than three.
    Many(Box<[Arc<VarCell>]>),
}

impl GlobalChain {
    /// Builds the chain from the candidate cells, in resolution order.
    /// `cells` must be non-empty (`Interp::for_each_global_candidate`
    /// always probes at least the bare name).
    pub fn new(mut cells: Vec<Arc<VarCell>>) -> GlobalChain {
        match cells.len() {
            0 => unreachable!("the candidate walk always probes at least the bare name"),
            1 => GlobalChain::One(cells.pop().expect("len == 1")),
            2 => {
                let second = cells.pop().expect("len == 2");
                GlobalChain::Two(cells.pop().expect("len == 2"), second)
            }
            _ => GlobalChain::Many(cells.into_boxed_slice()),
        }
    }

    #[inline]
    pub fn get(&self) -> Option<Value> {
        match self {
            GlobalChain::One(c) => c.get(),
            GlobalChain::Two(a, b) => a.get().or_else(|| b.get()),
            GlobalChain::Many(cs) => cs.iter().find_map(|c| c.get()),
        }
    }

    /// MT2: borrowing sibling of [`get`] -- same resolution order, but
    /// returns a [`crate::env::RootRead`] instead of cloning the `Value`
    /// out. Used on the hot call path (`compile::exec::exec_call_global`)
    /// so calling a global (native or closure) costs one atomic load
    /// instead of a lock-free-but-still-cloning read plus an `Arc<Value>`
    /// refcount bump.
    ///
    /// [`get`]: Self::get
    #[inline]
    pub fn read(&self) -> Option<crate::env::RootRead> {
        match self {
            GlobalChain::One(c) => c.read_root(),
            GlobalChain::Two(a, b) => a.read_root().or_else(|| b.read_root()),
            GlobalChain::Many(cs) => cs.iter().find_map(|c| c.read_root()),
        }
    }

    /// The cell an `Intrinsic` node would run: the last candidate, which is
    /// the one that was bound at compile time (or, if none was, the bare
    /// name -- an unbound cell is never pristine, so no intrinsic gets
    /// emitted for it anyway).
    pub fn resolved(&self) -> &Arc<VarCell> {
        match self {
            GlobalChain::One(c) => c,
            GlobalChain::Two(_, b) => b,
            GlobalChain::Many(cs) => cs.last().expect("Many is never empty"),
        }
    }

    /// True while every candidate AHEAD of `resolved` is still unbound,
    /// i.e. while `resolved` really is what this symbol names. A `def` that
    /// shadows a core name inside the running namespace flips this to
    /// false, and the intrinsic degrades to the ordinary call it carries
    /// all the parts for.
    #[inline]
    pub fn resolved_still_wins(&self) -> bool {
        match self {
            GlobalChain::One(_) => true,
            GlobalChain::Two(a, _) => !a.is_bound(),
            GlobalChain::Many(cs) => cs[..cs.len() - 1].iter().all(|c| !c.is_bound()),
        }
    }

    /// Same candidates, in the same order, by cell IDENTITY -- i.e. the two
    /// chains are interchangeable for both `get` and `resolved_still_wins`.
    /// Used to deduplicate the guard list a `NumLoop` collects; comparing
    /// only `resolved()` would not do, since two chains can share a resolved
    /// cell and differ in the prefix `resolved_still_wins` reads.
    pub fn same_candidates(&self, other: &GlobalChain) -> bool {
        use GlobalChain::*;
        match (self, other) {
            (One(a), One(b)) => Arc::ptr_eq(a, b),
            (Two(a1, a2), Two(b1, b2)) => Arc::ptr_eq(a1, b1) && Arc::ptr_eq(a2, b2),
            (Many(a), Many(b)) => {
                a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| Arc::ptr_eq(x, y))
            }
            _ => false,
        }
    }

    /// The whole intrinsic guard in one place: this chain must still name
    /// `resolved`, and `resolved` must still be the untouched
    /// boot-registered native. `Ir::Intrinsic` checks it per node, per
    /// evaluation; `Ir::NumLoop` checks it once per loop ENTRY for every
    /// intrinsic it absorbed, which is the only reason it may keep the
    /// arithmetic out of `Value` for the whole loop.
    #[inline]
    pub fn intrinsic_armed(&self) -> bool {
        self.resolved_still_wins()
            && self
                .resolved()
                .pristine_builtin
                .load(std::sync::atomic::Ordering::Acquire)
    }
}

pub enum Ir {
    /// A literal, a `quote`d form, or a fully-constant collection literal
    /// that was folded at compile time (folding is only ever applied when
    /// EVERY element folded, so evaluation order can't be observed).
    Const(Value),
    LoadSlot(u16),
    /// A slot read that MOVES: `std::mem::replace(slot, Value::Nil)` instead
    /// of `slot.clone()` (Perceus-lite phase 2). Emitted by
    /// `compile::lastuse` ONLY where a backward liveness analysis has proven
    /// that no path from this read reaches another read of the same slot, so
    /// the `Nil` left behind is unobservable.
    ///
    /// The point is not the saved refcount bump: it is that the value
    /// arriving in a call's owned `argv` may then be the ONLY handle in
    /// existence, which is what turns `builtins::reuse`'s consuming
    /// convention from "the callee owns its handle" into "the callee owns
    /// the only handle" -- and an `imbl`/`Arc` mutation through the only
    /// handle does not copy. See `compile::lastuse` for the analysis and
    /// for every condition under which this node is NOT emitted.
    LoadSlotTake(u16),
    LoadCapture(u16),
    /// The running closure itself (a named `fn`'s self-reference). Binds
    /// *after* the params, so it shadows a same-named param exactly like
    /// `run_closure_body` does.
    SelfRef,
    /// A global resolved to its namespace candidate chain at compile time.
    /// A chain that answers nothing produces the tree-walker's exact
    /// "Unable to resolve symbol: x".
    GlobalRef {
        chain: GlobalChain,
        sym: Symbol,
        span: Span,
    },
    /// A free symbol of a closure whose creation `Env` is NOT the root: it
    /// is looked up, on EVERY access, in that closure's own creation env
    /// chain -- the very chain the tree-walker would consult at call time.
    ///
    /// This is what makes a closure created under live tree-walked frames
    /// compilable at all (stage S2.5): an intermediate frame can still gain
    /// a binding after the closure was built (`letfn`'s mutual recursion is
    /// exactly that), and it can even REBIND a name the closure already
    /// read (`(let [a 1 g (fn [] a) a 2] (g))` is `2`), so nothing about
    /// those frames may be snapshotted at creation time.
    ///
    /// `chain` is the interned candidate chain for the global half of that
    /// probe, letting it skip the root frame's `HashMap` (and its
    /// process-wide lock) once the intermediate frames have missed: a root
    /// binding is ALWAYS a `Binding::Var` (`Env::set`/`set_builtin` intern
    /// one), so "walk the non-root frames, then read the chain" is
    /// `Interp::resolve_symbol` exactly.
    CreationEnvLookup {
        sym: Symbol,
        chain: GlobalChain,
        span: Span,
    },
    /// `set!` on a deftype's own mutable field, recognized in `resolve.rs`
    /// from `wrap_fields_let`'s paired-local shape: a field's plain slot
    /// (`f`) plus its hidden owner-marker slot (`__mutfield_owner_f` ->
    /// `this`), both bound in THIS fn's own scope chain (never across a
    /// nested `fn` boundary -- `FnCtx::lookup` doesn't cross one, so that
    /// case still falls through to the `set!` bail, unchanged). Mirrors
    /// `eval_set_bang`'s mutable-field arm exactly: writes `inst.fields
    /// [idx]` under lock (visible to every `Arc<InstVal>` holder) AND
    /// updates `field_slot` in this frame so a later bare read of `f` here
    /// sees the new value, matching `env.set_local_in_place`. `ic` caches
    /// the (type -> basis index) answer exactly like `FieldGet::ic` --
    /// same non-problem invalidation argument: a field's basis POSITION
    /// never changes, only its VALUE mutates.
    SetMutField {
        owner_slot: u16,
        field_slot: u16,
        field_name: Str,
        ic: FieldIc,
        value: Box<Ir>,
        span: Span,
    },
    If {
        test: Box<Ir>,
        then: Box<Ir>,
        els: Option<Box<Ir>>,
    },
    Do(Vec<Ir>),
    /// `let`: each init is evaluated and destructured into its pattern's
    /// slots in order, so a later init sees the earlier bindings (matching
    /// `eval_let`'s single accumulating child env).
    Let {
        binds: Vec<(CompiledPattern, Ir)>,
        body: Vec<Ir>,
    },
    /// `loop`: inits behave exactly like `Let`'s; `body` re-runs whenever it
    /// unwinds with `Flow::Recur`, after each raw value in the scratch block
    /// at `scratch_base` is re-destructured through its binding's pattern.
    /// The scratch block has one slot per binding PAIR (not per name a
    /// pattern introduces) -- which is precisely what `eval_loop`'s internal
    /// `__loopN` symbols are, and what keeps `recur`'s arity keyed to the
    /// binding count rather than the name count.
    Loop {
        binds: Vec<(CompiledPattern, Ir)>,
        scratch_base: u16,
        body: Vec<Ir>,
    },
    /// Evaluates its args into `scratch_base..` and unwinds with
    /// `Flow::Recur`. The arg count is checked at COMPILE time against the
    /// target's binding count (a mismatch makes the fn fall back, so the
    /// tree-walker keeps producing that runtime error with its original
    /// message and timing).
    Recur { args: Vec<Ir>, scratch_base: u16 },
    /// A `Loop` whose every binding, test and `recur` argument is scalar
    /// arithmetic over its own bindings and numeric constants, lowered to a
    /// flat op list over a register file of unboxed `Num`s. Always carries
    /// the generic `Loop` it was built from, and runs it whenever an entry
    /// guard says the specialization does not apply. See [`NumLoop`].
    NumLoop(Box<NumLoop>),
    Call {
        callee: Box<Ir>,
        args: Vec<Ir>,
        span: Span,
    },
    /// `(f a b)` where `f` resolved to a global: folds the `GlobalRef` into
    /// the call so the common case is one cell read plus the args. Two
    /// spans, because the folded-away node had its own: `sym_span` (the
    /// head symbol alone) is what an unresolved-callee error points at,
    /// `span` (the whole call form) is what the call itself reports --
    /// exactly the split `eval_list` gets for free by evaluating the head
    /// as a separate form.
    CallGlobal {
        chain: GlobalChain,
        sym: Symbol,
        sym_span: Span,
        args: Vec<Ir>,
        span: Span,
    },
    /// `(f a b)` where `f` resolved to a `CreationEnvLookup`: the same fold
    /// `CallGlobal` is, with the same two spans, and the same late-macro
    /// diagnostic if the name turns out to hold a macro at call time.
    CallCreationEnv {
        sym: Symbol,
        chain: GlobalChain,
        sym_span: Span,
        args: Vec<Ir>,
        span: Span,
    },
    /// A call to a builtin that was still `pristine_builtin` at compile
    /// time: `exec` re-checks that flag (one relaxed-ordering atomic load)
    /// and, while it holds, runs the builtin's own Rust helper directly on
    /// the evaluated arguments -- no argument `Vec` for the 1- and 2-arg
    /// shapes, and no `NativeFn` dyn call. The instant a user `def`s over
    /// the name -- either over the builtin itself (the flag clears) or over
    /// the name IN THE RUNNING NAMESPACE, which shadows it
    /// (`GlobalChain::resolved_still_wins` goes false) -- this node behaves
    /// as the `CallGlobal` it carries all the parts for, so a redefinition
    /// is picked up with the ordinary semantics.
    Intrinsic {
        op: IntrinOp,
        chain: GlobalChain,
        sym: Symbol,
        sym_span: Span,
        args: Vec<Ir>,
        span: Span,
    },
    VectorLit(Vec<Ir>),
    MapLit(Vec<(Ir, Ir)>),
    SetLit(Vec<Ir>),
    Throw { value: Box<Ir>, span: Span },
    /// A nested `fn` form. `template` is compiled ONCE, when the *enclosing*
    /// fn is compiled; executing this node only snapshots `caps` out of the
    /// running frame and wraps them in a fresh `Closure`.
    MakeClosure {
        template: Arc<FnTemplate>,
        caps: Vec<CaptureSrc>,
    },
    /// A RECURSIVE BINDING GROUP: the `letfn` shape -- a run of two or more
    /// CONSECUTIVE `let` bindings, each a bare symbol bound to a literal
    /// `fn` form, where at least one member mentions another member's name.
    ///
    /// Before this node, one such run bailed the WHOLE enclosing fn to the
    /// tree-walker (`resolve::POISON_BAIL`, the deferred half of the poison
    /// rule): a member reading a name bound LATER in the same vector has no
    /// live frame to defer to in a compiled scope. This node gives it one --
    /// not a frame, but a [`super::RecGroup`]: every member of the run is
    /// built at once, from one shared group object, and an intra-group
    /// reference compiles to [`Ir::SiblingRef`] rather than to a capture.
    ///
    /// Executing it snapshots each member's OUTER captures out of the
    /// running frame (exactly as `MakeClosure` does -- `caps` here can never
    /// name a member of this same run, see `SiblingRef`), builds the group,
    /// materializes every member, and writes each into its slot. The SLOTS
    /// are the strong owners of the members; the group itself holds them
    /// only weakly. See [`super::RecGroup`] for the whole cycle argument.
    ///
    /// `slots[i]` is where member `i` is written. The node's own VALUE is
    /// member 0's closure, so the enclosing `Ir::Let` binding pair
    /// (`CompiledPattern::Slot(slots[0])`, this node) re-writes slot 0 with
    /// the value it already holds -- one `Arc` bump, and the price of
    /// keeping `Ir::Let`'s `binds` a plain `(pattern, init)` list rather
    /// than growing a third binding-step shape that every walker in the
    /// tier would have to learn.
    ///
    /// Collapsing N binding steps into one is unobservable: the run is by
    /// construction N consecutive `fn` LITERALS, and evaluating a `fn`
    /// literal has no effect other than reading `*unchecked-math*` (see
    /// `RecGroup::unchecked_math` for the one documented consequence).
    MakeRecGroup {
        members: Vec<RecMember>,
        slots: Vec<u16>,
    },
    /// A reference from inside one member of a recursive binding group to
    /// ANOTHER member of the same group (or to itself by a name other than
    /// its `fn` self-name), by index into the group's member list.
    ///
    /// **This is deliberately not a capture, in either direction.** Mutual
    /// recursion is inherently cyclic, so a by-value sibling capture would
    /// move the `Arc` cycle into `CompiledClosure::captures` (e -> o -> e)
    /// or into the group's own snapshot (group.caps[o] holds e, e holds the
    /// group). Resolving through the group on each access -- forward
    /// references and BACKWARD ones alike -- is what keeps every strong edge
    /// acyclic. See [`super::RecGroup`].
    ///
    /// Valid only in a frame whose running closure IS a group member;
    /// `resolve.rs` emits it only there, and `exec.rs` treats a missing
    /// group as an internal error rather than a panic. A fn nested INSIDE a
    /// member reaches a sibling through [`CaptureSrc::Sibling`] instead,
    /// which is the same read taken one frame out, at closure-creation time.
    SiblingRef(u16),
    /// `try`, with any number of `catch` clauses (C3g: was at most one) and
    /// at most one `finally` -- `eval_try`'s exact shape. See
    /// `exec::exec_try` for the four-way interaction between the body's
    /// outcome, `Flow::Recur` and the always-run `finally`, and
    /// `CatchArm`'s own doc for how a clause's class is matched.
    Try {
        body: Vec<Ir>,
        catches: Vec<CatchArm>,
        finally: Option<Vec<Ir>>,
    },
    /// C1: `binding` / `with-redefs`. Boxed to keep `Ir` narrow; see [`DynBind`].
    DynBind(Box<DynBind>),
    /// `def` inside a fn body: writes through the interned root cell of the
    /// symbol's NAMESPACE-QUALIFIED name (which is what `eval_def` does,
    /// minus the re-intern), and evaluates to the value it wrote. `value: None` is
    /// the 1-argument `(def x)` form, which binds `nil` -- NOT a "declare"
    /// that leaves the cell unbound (`eval_def`'s own behavior).
    Def {
        cell: Arc<VarCell>,
        value: Option<Box<Ir>>,
    },
    /// field3/W-RESOLVE: **the one node that is not compiled.** An interop
    /// form (`(.method x)`, `(Ctor. a)`, longhand `(. x m)`, `(new C a)`)
    /// inside an otherwise-compilable fn, kept as its UNMODIFIED source
    /// `Form` and handed straight back to `Interp::eval_form_in` at run
    /// time. Boxed so this variant -- by far the largest -- does not widen
    /// every other `Ir` in every body.
    ///
    /// Before this node existed, one such form bailed the WHOLE fn to the
    /// tree-walker (`compile::resolve`'s `Bail` was the tier's only
    /// granularity), which is what made the assembled `delays.clj` cost
    /// 128.40s instead of 2.75s: 100 worker fns whose 10 000-iteration,
    /// interop-FREE `dotimes` tree-walked -- and therefore re-expanded the
    /// `is` macro on every single iteration -- purely because each fn also
    /// contained two `(.await barrier)` calls. See
    /// `docs/W-RESOLVE-interop-escape-decision.md`.
    ///
    /// Semantics are not re-implemented, they are *delegated*: the escaped
    /// form is the same form, with the same spans, evaluated by the same
    /// `eval_form_in` the tree-walk tier would have used, so return values,
    /// error kind/message/span, side-effect order and dynamic-binding
    /// visibility cannot drift. The only thing this node must get right is
    /// the ENV it evaluates in -- see [`Escape`].
    Escape(Box<Escape>),
    /// `(.-field recv)` where `recv` is a pure local read: the one interop
    /// shape hot enough to be worth compiling rather than escaping. See
    /// [`FieldGet`].
    FieldGet(Box<FieldGet>),
    /// K5: `(new C a..)` with compiled args; `fallback` (the Escape) runs when C is not a matching user type.
    New(Box<NewInst>),
}

/// How many shapes ONE `.-field` site caches before further shapes take the
/// slow path. Four, matching `types::PROTO_IC_SLOTS`, for the same reason:
/// real sites are overwhelmingly monomorphic, a handful of shapes covers
/// the polymorphic stragglers, and an append-only bank needs no eviction
/// policy -- which is the only part of an inline cache that can be *wrong*.
pub const FIELD_IC_SLOTS: usize = 4;

/// One cached (type -> basis index) answer. Laid out exactly like
/// `types::ProtoIcEntry`, and for the same two reasons.
pub struct FieldIcEntry {
    /// `Arc::as_ptr(&tdef) as usize` -- the comparison key, stored inline
    /// so a probe is one `usize` compare and never chases the `Arc`.
    pub tdef_ptr: usize,
    /// The strong ABA pin. NEVER read. A `TypeDef` is identified by its
    /// allocation address (`types::TypeDef`'s own doc: re-evaluating a
    /// `deftype` mints a NEW one), so without a strong handle a dropped
    /// type's address could be recycled by a later, unrelated type and a
    /// stale entry would answer for it. Holding the `Arc` makes that
    /// address un-reusable for as long as the entry can be read.
    #[allow(dead_code)]
    pub tdef: Arc<crate::types::TypeDef>,
    /// The field's position in `tdef.basis`, i.e. its index into
    /// `InstVal::fields`.
    pub idx: u32,
}

/// A monomorphic-ish inline cache for one `.-field` site.
///
/// `OnceLock` rather than the packed `AtomicU64` the `host_struct` shape
/// cache uses, because the payload here must carry the strong `Arc` pin
/// alongside the key and that does not fit in a word. This is the
/// `types::ProtoIc` discipline verbatim: append-only, write-once per slot,
/// lock-free to read, and never invalidated -- because it never needs to be.
///
/// INVALIDATION IS A NON-PROBLEM HERE, which is worth stating outright
/// since it is the usual way an inline cache goes wrong:
///
/// - A `TypeDef`'s `basis` is fixed at construction and never mutated, so a
///   (type, field) -> index answer cannot go stale while that type lives.
/// - Redefining a `deftype` does not mutate the old `TypeDef`; it mints a
///   NEW one at a NEW address. Instances built before the redefinition keep
///   pointing at the old one and must keep reading their old layout --
///   which is precisely what a pointer-keyed cache does. The new type
///   simply misses, then installs its own entry beside the old one.
/// - `InstVal::fields` is immutable after construction: mova's `deftype`
///   has no mutable fields and `set!` is vars-only, so no instance can
///   change shape underneath a cached index.
pub struct FieldIc {
    slots: [std::sync::OnceLock<FieldIcEntry>; FIELD_IC_SLOTS],
}

impl Default for FieldIc {
    fn default() -> Self {
        Self::new()
    }
}

impl FieldIc {
    pub fn new() -> Self {
        FieldIc {
            slots: [const { std::sync::OnceLock::new() }; FIELD_IC_SLOTS],
        }
    }

    /// The hit path: at most `FIELD_IC_SLOTS` inline `usize` compares. The
    /// bank is append-only, so the first EMPTY slot ends the run and a cold
    /// site pays one load rather than four.
    #[inline]
    pub fn probe(&self, tdef_ptr: usize) -> Option<u32> {
        for slot in &self.slots {
            let entry = slot.get()?;
            if entry.tdef_ptr == tdef_ptr {
                return Some(entry.idx);
            }
        }
        None
    }

    /// The miss path: publish this answer into the first free slot. Losing
    /// the `set` race, or finding the bank full, just means this shape stays
    /// uncached -- correct, only slower, which is the whole safety argument
    /// for an inline cache.
    pub fn install(&self, tdef_ptr: usize, tdef: Arc<crate::types::TypeDef>, idx: u32) {
        let mut entry = FieldIcEntry {
            tdef_ptr,
            tdef,
            idx,
        };
        for slot in &self.slots {
            match slot.set(entry) {
                Ok(()) => return,
                Err(back) => entry = back,
            }
        }
    }
}

/// The body of [`Ir::FieldGet`]: a compiled `(.-field recv)`.
///
/// ## Why this node exists
///
/// Every interop form compiles to an `Ir::Escape` (see [`Escape`]), which
/// per EXECUTION builds a child `Env` (an `Arc` + `RwLock` + `HashMap`
/// allocation), writes each bridged local into it (a hash insert behind an
/// `RwLock` acquire), and re-enters the tree-walker to re-parse the
/// `.-`-prefixed head and walk `eval_dot_form`'s whole receiver chain.
/// Measured on this machine: ~1.1 microseconds for one field read.
///
/// That is the single largest source of wasted work the regret ledger
/// records. On the census's heaviest file (`transducers.clj`, 138 s) the
/// lens counts 83,752,818 escape executions against 4,299 macro expansions
/// and 4,351 tier bails, and the dominant sites are all this one shape:
/// `test.check`'s `root` (`(.-root rose)`, 27.6M executions) and `children`
/// (15.9M). A `deftype` field read is physically an indexed load out of a
/// vector -- single-digit nanoseconds.
///
/// ## What it does
///
/// Reads the receiver out of the frame; if it is a `Value::Inst` whose type
/// declares `field`, returns that field. Everything else -- a non-`Inst`
/// receiver, a type without the field, a record whose map lacks the key --
/// runs `fallback`, which is the `Ir::Escape` this node was built from,
/// kept verbatim. Every shape the fast path does not handle therefore
/// behaves exactly as it did before this node existed, error messages and
/// spans included.
///
/// ## Why re-running the fallback is safe
///
/// `recv` is restricted to a slot or capture read (`resolve.rs` refuses to
/// build this node otherwise). Reading a frame slot is pure and total, so
/// the fast path having already looked at it is unobservable and the
/// fallback re-reading it cannot double a side effect. That is the same
/// restriction, for the same reason, `NumSeed` puts on a `NumLoop`'s seeds.
///
/// ## Why the fast path is exactly the slow path's answer
///
/// For `field_only` (`.-`) with a `Value::Inst` receiver and exactly one
/// argument, `eval::types_forms::eval_dot_form` reaches its `Inst` arm
/// unimpeded: every earlier arm is either gated on `!field_only` or
/// excludes `Inst` targets explicitly (the universal-`Object`-method
/// fallback does both). That arm's entire answer is `inst_field(inst,
/// field)` -- `data.get(Keyword::from(field))` for a record,
/// `fields.get(basis.position(field))` for a deftype. This node computes
/// those two, with the keyword built once at compile time instead of per
/// call and the basis scan cached per type. A `None` from either is not an
/// answer: it falls through to `fallback`, which lets the tree-walker raise
/// its own error at its own span.
pub struct FieldGet {
    /// The field name, with the `.-` prefix already stripped.
    pub field: crate::value::Str,
    /// `Value::Keyword(field)`, built once here rather than per call: the
    /// record path looks the field up in `InstVal::data` by keyword, and
    /// `inst_field` mints that keyword on EVERY call today.
    pub kw: Value,
    /// Where the receiver lives. Pure reads only -- see the doc above.
    pub recv: FieldRecv,
    pub ic: FieldIc,
    /// The `Ir::Escape` this node was built from, run verbatim whenever the
    /// fast path does not apply.
    pub fallback: Ir,
    /// The owning fn's regret-ledger site, copied in exactly as
    /// [`Escape::lens_site`] is.
    pub lens_site: u32,
}

/// K5: body of [`Ir::New`]. `class` is the bare class symbol form (never a local).
pub struct NewInst {
    pub class: crate::reader::Form,
    pub args: Vec<Ir>,
    pub fallback: Ir,
    pub span: Span,
}

/// Where a [`FieldGet`]'s receiver is read from. Both are pure, total frame
/// reads, which is what makes re-running the fallback safe.
#[derive(Clone, Copy)]
pub enum FieldRecv {
    Slot(u16),
    Capture(u16),
}

/// The body of [`Ir::Escape`]: what to tree-walk, and the locals bridge
/// that makes tree-walking it faithful.
///
/// `binds` is the bridge. At run time `exec` builds ONE child frame of the
/// closure's creation env (`l.me.env`) and writes `binds` into it, so the
/// escaped form sees the enclosing compiled fn's locals under the same
/// names a tree-walked frame would have shown. Four cases, all covered:
///
/// 1. this fn's own slots -> `CaptureSrc::Slot`,
/// 2. an enclosing COMPILED fn's locals -> `CaptureSrc::Capture` (the same
///    by-value snapshot `Ir::MakeClosure` takes, exact for the same reason:
///    a slot is never rebound under a live closure),
/// 3. the fn's self-name -> `CaptureSrc::SelfRef`,
/// 4. everything further out -> not bound here at all; the frame's PARENT
///    is the live creation-env chain, which is exactly what
///    `Ir::CreationEnvLookup` probes per access. Live frames stay live.
///
/// `binds` is built by `Compiler::resolve_lexical` -- the very function
/// that resolves every ordinary symbol reference -- so a name reaches the
/// bridge through the resolution order the two tiers already agree on, and
/// crossing a nested-fn boundary records the name in `ctx.captured_names`,
/// putting the escape under `compile_binds`' existing rebinding guard for
/// free.
///
/// Only names actually MENTIONED in the escaped subtree are bridged. That
/// is an over-approximation of use (every bare symbol in the subtree
/// counts, quoted or not) and therefore safe; the visible set it draws
/// from is the full lexical scope, which is what the tree-walker would
/// have shown.
/// F1 (native tier): `Clone` so `jit::lower`'s generic tier can leak an
/// independent, `'static` copy at codegen time -- same convention as
/// `CallGlobalSite` (never hold a raw pointer INTO the live `Ir` tree).
#[derive(Clone)]
pub struct Escape {
    /// The interop form, byte-for-byte as written (spans included).
    pub form: crate::reader::Form,
    /// Name -> where its current value lives in the running frame.
    pub binds: Vec<(Symbol, CaptureSrc)>,
    pub span: Span,
    /// field4/W-LENS-1: the regret-ledger site of the fn that owns this
    /// escape (or `lens::NO_SITE`). Copied in at compile time so
    /// `exec::exec_escape` counts the execution with a field read and no
    /// indirection through `l.me` -- an escape is by definition a slow-path
    /// decision, and its EXECUTION count is the only number that says how
    /// much that decision costs.
    pub lens_site: u32,
}

/// One compiled `catch` clause of an `Ir::Try` (C3g). `class` is the RAW
/// class-name symbol as written at the catch site (`None` for an untyped
/// `(catch e ...)` clause) -- NEVER resolved/evaluated at compile time, for
/// the same reason `eval::special_forms::parse_catch_head`'s doc gives:
/// several names this corpus catches by (`ArithmeticException`,
/// `clojure.lang.ArityException`, ...) are not registered `ClassVal`s at
/// all. Matching happens at CATCH time, in `exec::exec_try`, via
/// `eval::special_forms::catch_class_matches` -- shared with the
/// tree-walker's `eval_try` so both tiers agree about which clause a given
/// thrown error hits. `slot` is the binding's local slot, `body` its
/// compiled clause body.
pub struct CatchArm {
    pub class: Option<Symbol>,
    pub slot: u16,
    pub body: Vec<Ir>,
}

/// Everything `Ir::MakeClosure` needs to build a full, legacy-capable
/// `Closure` at run time, shared by every instance the nested `fn` form
/// produces: the compiled code AND the `parse_fn_like` products the
/// tree-walker's own machinery reads (`arities` drives arity selection and
/// arity-error messages in BOTH tiers -- see `eval::apply::apply_closure`).
pub struct FnTemplate {
    pub code: Arc<CompiledFn>,
    pub arities: Arc<Vec<Arity>>,
}

/// One member of an [`Ir::MakeRecGroup`]: the same two things a
/// `MakeClosure` carries, and for the same reasons -- a template compiled
/// ONCE with the enclosing fn, plus where to read this instance's captures
/// out of the running frame.
///
/// The member's NAME is not stored separately: it is `template.code.name`,
/// which `parse_fn_like` filled in from the `(fn <name> ..)` form
/// `resolve.rs` synthesizes/accepts for every member of a run (`letfn`'s
/// expansion names every sibling `fn`). One spelling, one place.
///
/// `caps` can never name a member of this same group, which is the whole
/// no-cycle argument (see [`super::RecGroup`]). The mechanism, in one line:
/// a capture is interned by `resolve::resolve_lexical` only when the search
/// CROSSED out of the member's own `FnCtx`, and a run name is answered by
/// that very `FnCtx` (its `siblings`) without crossing -- so it becomes an
/// [`Ir::SiblingRef`] and never reaches the capture list. A
/// [`CaptureSrc::Sibling`] CAN appear here, but only for an OUTER group's
/// member (a `letfn` nested inside a `letfn` member), which is a value that
/// already existed before this group did.
pub struct RecMember {
    pub template: Arc<FnTemplate>,
    pub caps: Vec<CaptureSrc>,
}

/// Where one of a nested closure's captured values comes from, read out of
/// the ENCLOSING compiled frame at `MakeClosure` time.
///
/// Capture-by-value is exact here (unlike the tree-walked creation env,
/// which `Ir::CreationEnvLookup` must re-probe on every access): a compiled
/// `let`/`loop`/param binding owns its own slot for its whole lifetime and
/// is never rebound in place -- a shadowing `let` of the same name allocates
/// a NEW slot, and a `loop` iteration rebinds only after the body returned.
/// So no enclosing binding can change under a closure that captured it.
///
/// `SelfRef` is an addition to COMPILE-TIER-DESIGN.md's two-variant sketch:
/// the enclosing fn's own name is not in a slot (it is `Ir::SelfRef`, read
/// from the running closure), but it is just as immutable, so a nested fn
/// referring to it captures it by value too.
#[derive(Clone, Copy)]
pub enum CaptureSrc {
    Slot(u16),
    Capture(u16),
    SelfRef,
    /// Member `i` of the group the CREATING frame's closure belongs to --
    /// i.e. [`Ir::SiblingRef`] taken one frame out, at closure-creation
    /// time. Emitted only for a fn nested INSIDE a group member that
    /// mentions a sibling name: the nested closure's own `l.me` is not a
    /// member and has no group, so it cannot run a `SiblingRef` itself, but
    /// the member's frame -- which is where its `MakeClosure` runs -- can.
    ///
    /// By-value here is exact AND acyclic: a group member is immutable once
    /// materialized (identity included, see `RecGroup::materialize`), and
    /// the resulting edge runs nested-closure -> member -> group -> *weak*
    /// members, which closes no loop. It can never appear in an
    /// `Ir::MakeRecGroup`'s own member `caps`, because a sibling read from
    /// inside a member resolves at the member's own level as a `SiblingRef`
    /// and never crosses out as a capture.
    Sibling(u16),
}

/// A binding-site pattern, compiled: `special_forms::bind_pattern`'s
/// recursive shape with `env.set(sym, v)` replaced by a frame-slot write.
/// The leaves that actually inspect values (`uncons`,
/// `coerce_map_pattern_source`, `map_pattern_lookup`) are the tree-walker's
/// own functions, called from `exec.rs` -- only the *binding* half is
/// reimplemented here, so no lookup/coercion semantics can drift.
pub enum CompiledPattern {
    /// A plain symbol: the overwhelmingly common case, one slot write.
    Slot(u16),
    /// `[a b & rest :as all]`, executed step by step in source order --
    /// `bind_seq_pattern` keeps walking after `&`/`:as` rather than
    /// stopping, and `Rest`/`As` deliberately do not advance the cursor.
    Seq(Vec<SeqStep>),
    Map(Box<MapPattern>),
}

pub enum SeqStep {
    /// Bind the next `uncons`ed element (missing => `nil`).
    Elem(CompiledPattern),
    /// `& rest`: the remaining seq, or `nil` when nothing is left.
    Rest(CompiledPattern),
    /// `:as`: the ORIGINAL value, not the walked-down cursor.
    As(CompiledPattern),
}

/// `{:keys [a b] :strs [c] {d :k} :or {a 1} :as m}`, flattened at compile
/// time into ordered lookups. `:keys`/`:strs` expand IN PLACE (their entries
/// keep the position the `:keys` pair had), `:or` defaults are attached to
/// the entry they belong to, and `:as` always binds last -- `bind_map_pattern`'s
/// exact order, which is observable through a default's side effects and
/// through which of two same-named bindings wins.
pub struct MapPattern {
    pub entries: Vec<MapEntry>,
    pub as_pat: Option<CompiledPattern>,
}

pub struct MapEntry {
    pub key: Value,
    pub target: CompiledPattern,
    /// The `:or` default for this entry's name, compiled. Evaluated ONLY
    /// when the key is absent, in the scope that exists at this point of the
    /// pattern (bindings from earlier entries of this same pattern included
    /// -- exactly the accumulating `env` the tree-walker evaluates it in).
    pub default: Option<Ir>,
    /// `:keys!`/`:strs!`/`:syms!` (the tree-walker's `req`, `bind_directive_entries`):
    /// when the key is absent AND there is no `default`, throw "Missing
    /// required key" instead of binding `nil`. `resolve.rs`'s
    /// `compile_map_pattern` never lets a required entry carry a `default`
    /// (that combination -- "Can't supply default value for required key"
    /// -- always throws regardless of the map's contents, so it `Bail`s to
    /// the tree-walker instead of being represented here).
    pub required: bool,
    /// The entry's binding-form span, used only to attribute the "Missing
    /// required key" error above.
    pub span: Span,
}

/// How many `Num` registers a [`NumLoop`] may use: the loop's own bindings,
/// one per distinct numeric constant it mentions, and the temporaries its op
/// list needs. A power of two so the executor can index the register file
/// with a mask instead of a bounds check -- `resolve.rs` allocates every
/// register below this bound AND re-validates every emitted index against it
/// before the node is built (`validate_regs`), so the mask can never be the
/// thing that keeps an index in range.
pub const NUM_REGS: usize = 16;

/// At most this many loop bindings, so the executor's simultaneous-rebind
/// staging buffer always fits alongside them.
pub const NUM_MAX_BINDS: usize = 8;

/// The staging buffer is indexed by binding number and the register file by
/// register number, and binding *i* IS register *i* -- so the binds bound
/// must not exceed the register bound.
const _: () = assert!(NUM_MAX_BINDS <= NUM_REGS);

/// A specialized numeric loop: the compiled tier's generic `Loop` reduced to
/// a register machine over unboxed `Num`s.
///
/// The generic `Loop` pays, per operation, for a boxed `Value` round trip
/// through a frame slot, a `GlobalChain` guard re-check
/// (`resolved_still_wins` + one atomic load), and a `Result<Flow, RjError>`
/// return. For a loop whose entire state is scalars that is all overhead:
/// this node instead keeps the loop's bindings in a register file of unboxed
/// `Num`s and runs a flat three-address op list over it, with NOTHING in
/// the inner loop that can allocate, fail, call, or observe the frame.
///
/// Constants live in registers too (`consts`, loaded once at loop entry),
/// which is what lets [`NumOp`] be four bytes of register indices: the inner
/// loop then decodes no operands, and a whole loop body's op stream fits in
/// one cache line.
///
/// ## Grammar (what `resolve::specialize_num_loop` will accept)
///
/// The matcher is data-driven, not aspirational: exactly this and nothing
/// else, because every widening is a new way to disagree with the
/// tree-walker.
///
/// ```text
/// loop     := (loop [b1 s1 .. bn sn] (if <test> <branch> <branch>))
///           | (loop [b1 s1 .. bn sn] (if <test> <branch>))
///             1 <= n <= NUM_MAX_BINDS, every bi a plain symbol (no
///             destructuring), at least one branch a `recur`. The 2-arg
///             `if` is the `when`-shaped loop: its MISSING else is the
///             nil-terminal branch below.
/// seed si  := <int-const> | <float-const> | <slot read that is not one of
///             THIS loop's own binding slots>
/// test     := (<cmp> <expr> <expr>)     cmp in { < <= > >= = }
/// branch   := (recur <expr> x n)        aimed at THIS loop, arity n
///           | <expr>                    the loop's result
///           | nil                       the loop's result is `Value::Nil`
///           | (do)                      likewise -- an empty `do` IS `nil`
///           | (do <branch>)             a ONE-form `do` IS that form
///           | <absent>                  a 2-arg `if`'s missing else: `nil`
/// expr     := <int-const> | <float-const>
///           | <slot read>               binding register, else loop-invariant
///           | <capture read>            loop-invariant
///           | (+ <expr> <expr> ..)      n-ary, 2+
///           | (* <expr> <expr> ..)      n-ary, 2+
///           | (- <expr> <expr>)
///           | (inc <expr>) | (dec <expr>)
/// ```
///
/// Every operator must additionally have compiled to an `Ir::Intrinsic`,
/// which already requires that it was the pristine boot-registered builtin
/// at compile time and is not shadowed in the running namespace.
///
/// ### The nil-terminal branch (W-NUMLOOP)
///
/// A branch that produces `nil` is admitted as [`NumBranch::RetNil`]: it has
/// no expression to lower, so it carries no ops and no output register, and
/// the executor leaves the loop with `Value::Nil` instead of
/// `numbers::num_to_value(reg)`. All four spellings above -- an explicit
/// `nil` literal, a missing else, an empty `do`, and a one-form `do` around
/// any of them -- reach the same variant, because each is exactly `nil` to
/// the tree-walker as well. This is what puts `(when ..)`-shaped counting
/// loops (and therefore `dotimes` with an EMPTY body) on the ladder; it is
/// also the shape lane variants and superloops already prefer, since a
/// nil-terminal branch can never be a `Recur` and so can never trip
/// `lanes::feasible_worlds`'s both-branches-`Recur` refusal.
///
/// The `do` rule is a shape identity, not an evaluation rule: `Ir::Do` with
/// one element runs exactly that element and returns exactly its value, and
/// with zero elements returns exactly `Value::Nil`. A `do` of TWO OR MORE
/// forms is still declined -- which is why `dotimes` over a NON-empty body
/// (`(do <body> .. (recur ..))`) does not specialize.
///
/// Anything else outside the grammar -- a nested `loop`/`let`/multi-form
/// `do`, a call, a `recur` aimed at an enclosing loop or nested inside an
/// operand, a nested `if` in a branch, `/`, `zero?`, `not`, a non-numeric
/// non-`nil` constant, an `Ir::CreationEnvLookup`, an `Ir::SelfRef` -- is
/// simply not specialized. Declining is always correct, which is why the
/// node-shape test in `compile::tests` exists at all: no differential test
/// can tell a declined loop from a specialized one.
///
/// ## Deopt: every dynamic condition that can invalidate the specialization
///
/// All four are checked ONCE, at loop entry, before a single register is
/// used; any of them failing runs `fallback`, which is the very `Ir::Loop`
/// this node was built from, kept verbatim for exactly that purpose. There
/// is no in-loop deopt, because I2 below proves none is reachable.
///
/// - **D1** an absorbed intrinsic's cell is no longer the untouched
///   boot-registered native (`(def + ..)`) -- `guards`, via
///   [`GlobalChain::intrinsic_armed`];
/// - **D2** an absorbed intrinsic's name is shadowed by a `def` in the
///   running namespace, so the chain no longer resolves to that native --
///   the `resolved_still_wins` half of the same guard;
/// - **D3** a `NumSeed::Slot` seed holds something that is not `Int`/`Float`
///   at entry;
/// - **D4** a `loads` entry (a loop-invariant slot or capture) holds
///   something that is not `Int`/`Float` at entry.
///
/// D3/D4 are how a type error keeps its exact wording, span and timing: the
/// fallback re-runs the ordinary loop, which raises the tree-walker's own
/// error from the tree-walker's own code.
///
/// ## Invariants that make the entry-only guard sufficient
///
/// - **I1 -- exactness by sharing, not by re-derivation.** Every op is
///   executed by the very `builtins::numbers` function the corresponding
///   builtin folds with (`add`/`sub`/`mul`, the `f64` comparisons, and
///   `num_eq` for `=`), so overflow promotion (`i64` -> `f64`), `Int`/
///   `Float` blending, NaN behavior and `-0.0` handling are shared code, not
///   a second copy. The n-ary `+`/`*` folds are lowered starting from the
///   operator's IDENTITY (`Int(0)` / `Int(1)`), which is the exact shape
///   `exec_intrinsic` folds -- and load-bearing, since `(+ -0.0 -0.0)` is
///   `0.0` only because the fold starts at `0`.
/// - **I2 -- nothing in the loop can run user code.** The grammar admits no
///   call, and every op is total arithmetic over `Num`. So between entry and
///   exit no `def` can happen, no side effect can be observed, no error can
///   be raised and the stack cannot grow. That is what makes hoisting D1/D2
///   from per-operation to per-entry *equivalent* rather than merely
///   cheaper. (Two deviations: a `def` on ANOTHER thread, racing a running
///   loop, is not observed until the loop's next entry -- the tree-walker
///   offers no ordering guarantee there either, see COMPILE-TIER-DESIGN.md's
///   NumLoop section; and the embedding-fuel probe's `Interp::fuel`, an
///   orthogonal per-iteration budget check via an independent `Copy` local
///   rather than a call back into `Interp` -- see `compile::exec::
///   exec_num_loop`/`run_num_loop` -- which CAN raise `FuelExhausted`
///   mid-loop when a budget is set, without otherwise touching I2: no user
///   code runs and no OTHER side effect happens, only the fuel-local
///   counter and the loop's own exit path are affected.)
/// - **I3 -- loop-invariant reads really are invariant.** No op writes a
///   frame slot or a capture, and by I2 nothing else runs, so reading them
///   once at entry finds what the generic loop's per-iteration read would
///   have found every iteration.
/// - **I4 -- entering `fallback` is indistinguishable from never having
///   looked.** A seed is only ever `Const` or `LoadSlot` (see [`NumSeed`])
///   and a load only ever a slot or capture read, so the guards' reads are
///   pure and re-evaluating them in the fallback cannot double anything.
/// - **I5 -- not writing the loop's slots back is unobservable.** The
///   generic loop leaves its binding slots (and its `recur` scratch block)
///   holding the last iteration's values; this node leaves them `Nil`. Those
///   slots are loop-scoped, `resolve.rs` never reuses a slot index, and a
///   specialized body cannot contain an `Ir::MakeClosure` to capture one --
///   so nothing can read them after the loop.
/// - **I6 -- `Flow::Recur` can never escape.** A `recur` in a branch must
///   name THIS loop's scratch block at this loop's arity, or the loop is not
///   specialized; so the node always returns `Flow::Val`.
/// - **I7 -- every register index is in range.** `resolve.rs` allocates only
///   below `NUM_REGS` and then re-walks the finished op list to prove it
///   (`validate_regs`), refusing the node otherwise. The executor's mask is
///   therefore a formality, not the check.
pub struct NumLoop {
    /// Registers `0..seeds.len()` are the loop's bindings, in binding order.
    pub seeds: Vec<NumSeed>,
    /// `(register, value)` pairs written once at loop entry and never again:
    /// every numeric constant the loop mentions, deduplicated by BIT PATTERN
    /// (so `0.0` and `-0.0` keep separate registers).
    pub consts: Vec<(u8, Num)>,
    /// Loop-INVARIANT registers, read once at entry and checked for
    /// `Int`/`Float` exactly like a seed. Nothing a `NumLoop` executes can
    /// write a frame slot or a capture, so re-reading them per iteration (as
    /// the generic loop does) could not observe a different value.
    pub loads: Vec<(u8, NumLoad)>,
    /// The `if` test: an op list to run, then one comparison.
    pub test: NumTest,
    /// What to do when the test is truthy, and when it is falsey.
    pub then: NumBranch,
    pub els: NumBranch,
    /// One entry per intrinsic this loop absorbed; all checked at entry.
    pub guards: Vec<GlobalChain>,
    /// The generic `Ir::Loop` this was built from -- run verbatim whenever a
    /// guard or a seed check says the specialization does not apply.
    pub fallback: Ir,
    /// W1 (LATENCY-CAMPAIGN.md): precompiled unboxed-register lane variants,
    /// one per feasible stable per-binding tag vector (`compile::lanes::
    /// feasible_worlds`), built once here at resolve time. Empty when the
    /// loop's own transition admits none (a `Recur` in both branches, see
    /// `lanes::LaneVariant`'s doc) or under `MOVA_NO_LANES=1`
    /// (`resolve::lanes_disabled_by_env`). `exec::exec_num_loop` runs ONE
    /// iteration in the tagged machine below, then -- ONLY if this is
    /// non-empty -- checks whether the resulting register tags match one of
    /// these; a match switches into `exec::run_lane_variant` for the rest of
    /// the loop, an `I`-lane overflow deopts back into `run_num_loop` with
    /// the tagged registers reconstructed from the lane's current values
    /// (sound by `NumLoop`'s I2: total arithmetic, no observation between
    /// entry and exit, so resuming the tagged machine mid-loop is
    /// indistinguishable from having run it the whole time).
    pub lane_variants: Vec<super::lanes::LaneVariant>,
}

/// A loop binding's initial value. Restricted to the two shapes that are
/// pure, total and cheap to re-evaluate, so entering `fallback` after
/// reading them is indistinguishable from never having looked.
pub enum NumSeed {
    Const(Num),
    /// A frame slot, checked for `Int`/`Float` at loop entry. Never one of
    /// this loop's OWN binding slots (`resolve.rs` rejects that: the generic
    /// `Loop` would have written it first).
    Slot(u16),
}

/// Where a loop-invariant register's value comes from. Both are values the
/// running frame already holds: a slot of an enclosing binding (a param, an
/// outer `let`) or one of this closure's by-value captures.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum NumLoad {
    Slot(u16),
    Capture(u16),
}

pub struct NumTest {
    pub ops: Vec<NumOp>,
    pub cmp: NumCmp,
    pub a: u8,
    pub b: u8,
}

/// The comparisons admissible in a loop's test position.
///
/// `Lt`/`Le`/`Gt`/`Ge` are `builtins::numbers`'s own `lt`/`le`/`gt`/`ge`
/// over `as_f64` of both operands -- which is what `cmp2` does, INCLUDING
/// comparing two `Int`s as `f64` (so `(< 9223372036854775806
/// 9223372036854775807)` is false: both round to the same `f64`).
///
/// `Eq` is deliberately NOT that. `=` goes through `Interp::values_equal`,
/// which blends `Int`/`Float` but compares `Int`/`Int` exactly and
/// `Float`/`Float` BY BITS -- so `=` and `<` genuinely disagree on adjacent
/// huge integers, `(= 0.0 -0.0)` is false and `(= NaN NaN)` is true. It runs
/// `numbers::num_eq`, which mirrors `values_equal`'s two relevant arms
/// line for line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NumCmp {
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
}

pub enum NumBranch {
    /// `(recur ...)` to this very loop: run `ops`, then assign the binding
    /// registers from `next` SIMULTANEOUSLY (they may read each other).
    Recur { ops: Vec<NumOp>, next: Vec<u8> },
    /// Leave the loop with this value.
    Ret { ops: Vec<NumOp>, out: u8 },
    /// W-NUMLOOP: leave the loop with `Value::Nil`.
    ///
    /// Deliberately payload-free. A nil-producing branch has NO expression
    /// to lower -- there is nothing to compute, no register to name, and
    /// `Value::Nil` is not a [`Num`], so it could not be named by one
    /// anyway. Every tier therefore treats this variant as "stop, and the
    /// loop's value is nil": `exec::run_num_loop_one_iter`, the interpreted
    /// lane variant (`lanes::LaneBranch::RetNil`) and the superloop all
    /// return without touching a register file.
    ///
    /// See this type's parent [`NumLoop`] for which source shapes reach
    /// here (an explicit `nil`, a missing else, an empty or one-form `do`
    /// around either).
    RetNil,
}

/// One three-address instruction: `regs[dst] = op(regs[a], regs[b])`. Four
/// bytes, deliberately -- see [`NumLoop`] on why constants are registers.
pub struct NumOp {
    pub dst: u8,
    pub op: NumBin,
    pub a: u8,
    pub b: u8,
}

/// The binary arithmetic the executor can run.
///
/// `inc`/`dec` are lowered to `Add`/`Sub` against a register holding
/// `Int(1)`, which is literally how `inc1`/`dec1` are written.
///
/// `AddFold`/`MulFold` are the FIRST step of an n-ary `+`/`*`, with the
/// operator's identity folded in: `AddFold(a, b)` is `add(add(Int(0), a),
/// b)`, which is exactly what `exec_intrinsic` computes for the 2-argument
/// shape it special-cases. Keeping that as one instruction rather than two
/// is not an algebraic simplification -- the identity step is still
/// performed, on the same values, by the same function -- it just keeps the
/// intermediate in a register instead of round-tripping it through the
/// register file. On the LCG loop that is worth ~2x, because the identity
/// steps sit on the loop's serial dependency chain and a `Num` store
/// followed by a `Num` load of the same slot costs more than the arithmetic
/// does. Arguments beyond the second continue with plain `Add`/`Mul`.
#[derive(Clone, Copy)]
pub enum NumBin {
    Add,
    Sub,
    Mul,
    AddFold,
    MulFold,
}

/// The builtins the compiled tier can run without going through their
/// `NativeFn`. Each op's executor calls the SAME `builtins::numbers` /
/// `builtins::predicates` helper the corresponding native calls, so
/// overflow promotion (`i64` -> `f64`), `Int`/`Float` blending, lazy
/// forcing inside `=`, and every error string are shared code, not
/// reimplementations.
///
/// `Add`/`Mul` are n-ary (2+) and fold in ONE node rather than compiling
/// `(+ a b c)` into a chain of 2-arg nodes: a chain would fold in pairs
/// through a user's redefined `+` too (`(+ (+ a b) c)`), which is a
/// different call than `(+ a b c)`. Every other op is fixed-arity, so its
/// node is only ever emitted for that exact argument count.
#[derive(Clone, Copy, Debug)]
pub enum IntrinOp {
    Add,
    Sub2,
    Mul,
    Div2,
    Inc,
    Dec,
    Lt2,
    Le2,
    Gt2,
    Ge2,
    Eq2,
    Zero,
    Not,
}

/// C1: body of [`Ir::DynBind`]. Mirrors `eval_binding`/`eval_with_redefs`
/// step for step: per pair, resolve the var at RUN time through
/// `Interp::resolve_dyn_var_cell` (the tree-walker's candidate walk), then
/// evaluate its init; then `Interp::enter_binding`/`enter_with_redefs`,
/// the body, and `leave_*` on every exit (value, error, `Flow::Recur`).
pub struct DynBind {
    /// `true` for `with-redefs` (root swap), `false` for `binding`.
    pub redefs: bool,
    /// (var symbol, its span, compiled init), in source order.
    pub pairs: Vec<(Symbol, Span, Ir)>,
    pub body: Vec<Ir>,
    /// The whole form's span (dynamic-check / unbound errors).
    pub span: Span,
}
