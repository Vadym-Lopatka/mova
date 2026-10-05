//! The compiler: `&[Arity]` (already parsed by `parse_fn_like`) plus the
//! closure's creation `Env` -> `CompiledClosure`, or `None` = "tree-walk
//! this fn".
//!
//! ## Compile / fallback matrix (stages S2 .. S4)
//!
//! Compiled: literals (incl. vector/map/set literal forms), symbol
//! references (slot / self-name / capture / global candidate chain /
//! creation-env lookup), function calls, arithmetic and comparison
//! intrinsics, `if`, `do`, `let`, `loop`, `recur`, `quote`, `throw`,
//! multi-arity and variadic fns, named self-recursion, `()`, ALL
//! destructuring patterns (`[a b & r :as v]`, `{:keys/:strs [..] :or {..}
//! :as m}`, `{sym :key}`, nested, seq->map coercion) at every binding site,
//! nested `fn` forms (`Ir::MakeClosure`), `try`/`catch`/`finally`, `def`
//! in a body, and -- since fix/closure-env-cycles -- the `letfn` shape: a
//! run of consecutive `fn`-literal bindings that refer to each other
//! (`Ir::MakeRecGroup`, see "Recursive binding groups" below).
//!
//! v0.5: a `loop` whose whole state is scalars is additionally wrapped in an
//! `Ir::NumLoop` (see `specialize_num_loop` at the foot of this file, and
//! `ir::NumLoop` for the grammar, the deopt conditions and the invariants).
//! That is a pure addition -- the generic `Ir::Loop` is kept inside it and
//! is what runs whenever an entry guard says the specialization does not
//! apply -- so nothing in the matrix below changes. `MOVA_NO_NUMLOOP=1`
//! stops the node from being emitted at all.
//!
//! v0.5 also runs a second post-pass over each finished arity,
//! `compile::lastuse` (Perceus-lite phase 2): a backward liveness analysis
//! that rewrites a slot's provably-final `Ir::LoadSlot` into a MOVING
//! `Ir::LoadSlotTake`. Same relationship to the matrix as `NumLoop`'s --
//! it reads resolved IR in slot indices, cannot change what a node means,
//! and declining to rewrite is always correct -- and the same shape of
//! switch, `MOVA_NO_LASTUSE=1`, gating emission rather than execution. It
//! lives in its own module rather than at the foot of this file only
//! because it is a dataflow pass with ten stated safety conditions and
//! their proofs, which would bury the compiler proper.
//!
//! Falls back (whole fn tree-walks): `defmacro`/`ns`/`macroexpand`/
//! `macroexpand-1`/`quasiquote`/`var`/`set!` in the body, a `recur` whose
//! arg count doesn't match its target, any malformed special form (so the
//! tree-walker still raises that error at call time, with its original
//! message and timing), and the poison rule below.
//!
//! ## Macro expansion is FUSED into resolution
//!
//! COMPILE-TIER-DESIGN.md specifies a separate macroexpand-all pass
//! followed by resolve-and-emit. This implements the two fused into one
//! walk -- semantically identical (same dispatch order: special-form names
//! are matched before locals or macros, per `eval_list`; a bare-symbol head
//! that is not locally bound and whose binding currently holds a
//! `Value::Macro` is expanded via `apply_macro` + `value_to_form` and
//! re-walked; any expansion error aborts to fallback) but strictly cheaper:
//! a fn that will fall back usually discovers that within a few nodes and
//! never pays to expand the rest of its body.
//!
//! Freezing expansion at fn-creation time is the tier's one documented
//! semantic deviation (constraint 1 of the design doc): a macro defined, or
//! redefined, AFTER a fn that calls it is no longer picked up by that fn.
//! `exec.rs` raises a self-explanatory error if a global that compiled as a
//! call turns out to hold a macro at call time. S4 extends the freeze one
//! level: a nested `fn`'s body is expanded when the ENCLOSING fn is
//! compiled, not when the nested closure is created.
//!
//! ## Free-symbol rules
//!
//! Resolution is innermost-first, and crosses nested-`fn` boundaries:
//!
//! 1. this fn's own scopes -> `LoadSlot` / `SelfRef`;
//! 2. an ENCLOSING compiled fn's scopes -> `LoadCapture`, with a
//!    `CaptureSrc::Slot`/`Capture`/`SelfRef` recorded on this fn's capture
//!    list (transitively: a two-level-deep reference makes the middle fn
//!    capture it too). By-value is exact -- see `ir::CaptureSrc`;
//! 3. else, if the creation env IS the root -> the symbol's global
//!    candidate chain (`crate::ns`'s resolution order, interned as cells
//!    even where still unbound, so forward references late-bind);
//! 4. else -> `Ir::CreationEnvLookup`: re-resolved against the closure's own
//!    creation env chain on every access.
//!
//! Rule 4 is why `MakeClosure` gives every nested closure the ENCLOSING
//! closure's creation env (`exec.rs`): the compiled tier has no live env
//! chain of its own, and the only bindings that sit between the enclosing
//! fn's entry and the nested `fn` form -- its params, `let`s, `loop`s and
//! `catch` binding -- are exactly the ones rule 2 already captured by value.
//! So "enclosing slots, then the enclosing fn's own creation env" IS the
//! tree-walker's chain, with the middle (dead) segment replaced by a
//! snapshot that cannot differ from it.
//!
//! S2 instead snapshotted the creation frames by value and fell back
//! whenever a symbol wasn't in them yet. That fallback was a large class in
//! practice -- every free GLOBAL of a closure built under any intermediate
//! frame (a `future` thunk inside a `let`, a `lazy-seq` body, every `letfn`
//! sibling) dragged the whole fn back to the tree-walker -- and the
//! snapshot half was not even exact: a `let` frame is a single mutable
//! frame, so `(let [a 1 g (fn [] a) a 2] (g))` is `2`, not the snapshotted
//! `1`. `CreationEnvLookup` subsumes both halves and is exact for both.
//!
//! ## The poison rule
//!
//! While compiling binding *i* of a `let`/`loop`, every name introduced by
//! the patterns at *j >= i* is poison: none of them is bound yet.
//!
//! - Read *immediately* (the init itself, or anything it evaluates inline):
//!   safe to resolve outward iff the name has a value RIGHT NOW, because
//!   that is what the tree-walker's own `Interp::resolve_symbol` would find
//!   at that same moment. Otherwise fall back rather than freeze an "unresolved" that
//!   the tree-walker would raise at call time.
//!
//!   Two places can hold that value, and BOTH are asked (field3/W-PARSE):
//!   an enclosing COMPILED fn's binding, which the name may be SHADOWING
//!   (`(fn [s] (lazy-seq (loop [s s] ..)))` -- the shape `for` expands to,
//!   one gensym for the iterator fn's param and its loop binding), and only
//!   then globals / live creation frames. Asking the lexical chain first is
//!   what makes the shadow case a capture instead of a bail: the
//!   tree-walker's env walk finds no `s` in the loop's child env yet
//!   (`eval_loop` `set`s each name only after its init runs) and continues
//!   outward to the enclosing frame -- the very slot the capture reads.
//!   See docs/W-PARSE-poison-shadow-decision.md.
//! - Read from inside a nested `fn` (a DEFERRED read): never safe *on its
//!   own*, so it falls back. This is `letfn`'s shared-frame trick -- the
//!   sibling does exist by the time the closure runs, and the tree-walker
//!   finds it in the live `let` frame, but a compiled scope has no live
//!   frame to defer to, and resolving it outward (to a global of the same
//!   name, say) would silently call the WRONG function. Granularity is the
//!   whole enclosing fn, not just the nested one: `Bail` is the tier's only
//!   failure mode FOR THIS CASE, and a nested fn cannot fall back on its
//!   own while its `MakeClosure` node stays compiled. (`Ir::Escape`,
//!   field3/W-RESOLVE, is a per-NODE fallback, but it exists only for
//!   interop calls -- a form the tree-walker can evaluate standalone in a
//!   frame this module can reconstruct. A deferred read is not such a
//!   form: there is no frame to reconstruct, which is the whole problem.)
//!
//! ## Recursive binding groups: the shape that no longer bails
//!
//! The deferred read above is not resolvable *outward*, but it is
//! resolvable *sideways* -- if the compiler knows which closures the read
//! could possibly mean. `rec_group_run` detects exactly the case where it
//! does: a maximal run of two or more CONSECUTIVE bindings, each a bare
//! symbol bound to a literal `fn` form, at least one of which mentions
//! another run name. That run compiles to ONE `Ir::MakeRecGroup`
//! (`compile_rec_group`), and every intra-run reference -- forward AND
//! backward -- compiles to `Ir::SiblingRef`, resolved through the group's
//! member list at access time. That is the compiled tier's replacement for
//! the live `let` frame the tree-walker defers to.
//!
//! Two properties are non-negotiable and both fall out of "an intra-group
//! reference is never a capture":
//!
//! - **No `Arc` cycle.** mova is precise-RC with no cycle collector, and
//!   mutual recursion is inherently cyclic, so a by-value sibling capture
//!   in EITHER direction would simply relocate the leak into
//!   `CompiledClosure::captures` or into the group's own snapshot. The
//!   group holds its members only weakly; the frame slots own them. See
//!   `compile::RecGroup` for the full edge-by-edge argument.
//! - **No drift from the tree-walker.** The group answers the same closure
//!   the live frame would, for the whole life of the run -- which is only
//!   true because the run's names are proved unique in the vector
//!   (condition 3 of `rec_group_run`). A rebound member name would let the
//!   tree-walker's one mutable `let` frame move on while the group did not,
//!   so that shape is refused and keeps the bail above.
//!
//! What still bails, and why, in one place: a rebound member name, a
//! non-consecutive run, a run of one, a `loop` binding vector (a group
//! collapses N bindings into one entry and a `loop`'s binding count IS its
//! `recur` arity -- see `compile_binds`), and an `Ir::Escape` whose interop
//! form mentions a sibling (`compile_escape`: the escape's env bridge has
//! no frame to put a group member in, and putting one there would rebuild
//! the very cycle the group avoids).
//!
//! ## Capture-by-value vs. the tree-walker's one-frame `let`
//!
//! lsp/letfix: the tree-walker now opens a new frame on a rebind after a
//! closure (lexical, like the JVM), and `tree_walker_frames` mirrors that, so
//! the guard and the poison rule below only fire inside one such frame
//! (letfn runs). The text below describes the pre-fix one-frame behavior.
//!
//! `eval_let` evaluates every binding of one vector into a SINGLE child env,
//! by repeated `env.set` -- so re-binding a name mutates the frame a closure
//! built earlier in that same vector is still pointing at:
//!
//! ```clojure
//! (let [a 1 g (fn [] a) a 2] (g))   ; => 2, not 1
//! ```
//!
//! A compiled `let` gives the second `a` its OWN slot (which is what makes
//! capture-by-value exact everywhere else), so `g` would answer `1`. There
//! is no way to express the tree-walker's answer with by-value captures, so
//! `compile_binds` refuses: if a binding vector binds a name that a nested
//! fn created earlier in that same vector already captured, the fn falls
//! back. `FnCtx::captured_names` is what that guard reads. Everything else
//! about capture-by-value stays exact, because every other frame the
//! tree-walker builds (a fn call, each `loop` iteration, a `catch`) is
//! FRESH: it is only ever this one shape that mutates a frame under a live
//! closure. `loop`'s `recur` rebinding is not an instance of it -- the
//! tree-walker allocates a new env per iteration, so a closure created in
//! iteration *k* keeps iteration *k*'s values in both tiers.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use super::explain::{LoopDecision, LoopExplain};
use super::ir::{
    CaptureSrc, CatchArm, CompiledPattern, DynBind, Escape, FieldGet, FieldIc, FieldRecv, FnTemplate, NewInst,
    GlobalChain, IntrinOp, Ir, MapEntry, MapPattern, NumBin, NumBranch, NumCmp, NumLoad, NumLoop,
    NumOp, NumSeed, NumTest, RecMember, SeqStep, NUM_MAX_BINDS, NUM_REGS,
};
use super::{CompiledArity, CompiledClosure, CompiledFn};
use crate::builtins::numbers::Num;
use crate::env::Env;
use crate::eval::special_forms::{
    form_has_nested_fn, form_is_fn_literal, form_keyword_name, is_amp, is_as_kw, parse_catch_head, symbol_name_of_pattern,
};
use crate::eval::Interp;
use crate::reader::{Form, FormValue, Span};
use crate::value::{Arity, Keyword, PMap, PVec, Str, Symbol, Value};

/// Abort marker: any compile step may bail, and every bail means exactly
/// one thing -- "tree-walk this whole fn instead". `reason` is a
/// human-readable, specific explanation (field1/W-EXPLAIN) -- read by
/// `super::compile_fn` when the OUTERMOST `compile_arity`/`compile_form`
/// call in a fn fails, and surfaced through `MOVA_EXPLAIN=1` and the
/// `compile-explain` builtin. `span` is the source position closest to the
/// actual cause; `reason` is `Cow` rather than `&'static str` because the
/// nested-fn re-throw (`compile_fn_expr`) prefixes the inner reason with
/// `"nested fn: "` rather than discarding it, which is the one place this
/// module builds a reason string at compile time instead of borrowing a
/// literal.
pub(super) struct Bail {
    pub(super) reason: std::borrow::Cow<'static, str>,
    pub(super) span: Span,
}
type CResult<T> = Result<T, Bail>;

/// The poison rule's one bail reason, shared by the two arms that can raise
/// it (`resolve_lexical`'s deferred-read refusal and `poisoned_free`'s
/// genuinely-unbound tail) so the operator-facing text stays ONE string --
/// `MOVA_EXPLAIN` histograms and `compile-explain` records key on it.
const POISON_BAIL: &str = "reads a let/loop binding not yet bound at this point (the poison rule -- a letfn-style forward/deferred reference a compiled scope can't express)";

/// Builds a `Bail` from a `&'static str` reason -- the overwhelmingly common
/// case, at every site that isn't re-throwing another `Bail`'s reason.
fn bail<T>(reason: &'static str, span: Span) -> CResult<T> {
    Err(Bail {
        reason: std::borrow::Cow::Borrowed(reason),
        span,
    })
}

/// A `Span` for the handful of guards that have no meaningful source
/// position at hand (an internal allocator overflowing `u16`, an `FnCtx`
/// invariant that should be unreachable in practice) -- see this module's
/// W-EXPLAIN doc note on the tradeoff. Deliberately `(0, 0)` rather than a
/// `span` argument threaded through every allocator call: these are
/// pathological guards, not ordinary source-shape rejections, and
/// `error::line_col` reports `(0,0)` as line 1 col 1, which reads as "no
/// real position" without panicking.
const NO_SPAN: Span = Span { start: 0, end: 0 };

/// Guard against unbounded compile-time recursion (a self-expanding macro,
/// or pathologically nested source). The tree-walker has the same hazard at
/// *call* time; here we simply refuse to compile rather than risk blowing
/// the Rust stack at definition time.
const MAX_COMPILE_DEPTH: usize = 512;

/// `Ok` carries the compiled code AND every `loop`'s `NumLoop` decision
/// (field1/W-EXPLAIN) -- `super::compile_fn` is the one caller, and turns
/// both halves into an `explain::FnExplain`. `Err(Bail)` means the whole fn
/// tree-walks; `Bail`'s `reason`/`span` are exactly what that record needs
/// too. Recording the reason is purely a read of what this fn already
/// computes to make its actual (behavioral) decision -- nothing here is
/// gathered FOR the explain feature at the cost of changing what compiles.
pub(super) fn compile(
    interp: &mut Interp,
    name: Option<&Str>,
    arities: &[Arity],
    env: &Env,
    fn_span: Span,
) -> Result<(CompiledClosure, Vec<LoopExplain>, usize, u32), Bail> {
    let mut c = Compiler {
        globals: interp.globals.clone(),
        interp,
        env_is_root: env.is_root(),
        env: env.clone(),
        loop_explain: Vec::new(),
        n_escapes: 0,
        lens_name: name.cloned(),
        lens_span: fn_span,
        lens_site: crate::lens::NO_SITE,
    };
    let mut ctx = FnCtx::new(name.map(|n| Symbol::simple(n.clone())), 0, None);
    let mut compiled = Vec::with_capacity(arities.len());
    for arity in arities {
        match c.compile_arity(&mut ctx, arity) {
            Ok(a) => compiled.push(a),
            Err(bail) => return Err(bail),
        }
    }
    debug_assert!(
        ctx.captures.is_empty(),
        "the outermost compiled fn has no enclosing frame to capture from"
    );
    // field4/W-LENS-1: the escape site's reason, now that the count is
    // known. Only a fn that actually emitted an escape ever allocated one.
    if c.lens_site != crate::lens::NO_SITE {
        crate::lens::set_site_reason(
            c.lens_site,
            format!(
                "compiles, but {} interop form(s) escape to the tree-walker on every execution",
                c.n_escapes
            ),
        );
    }
    let escape_site = c.lens_site;
    Ok((
        CompiledClosure {
            code: Arc::new(CompiledFn {
                name: name.cloned(),
                arities: compiled,
                capture_syms: Vec::new(),
            }),
            // The outermost compiled fn captures nothing by value: it is
            // created by tree-walked code, whose frames are live and must be
            // re-probed (see `Ir::CreationEnvLookup`). Only `Ir::MakeClosure`
            // -- a closure built from inside a compiled frame, where a
            // snapshot IS exact -- ever fills this.
            captures: Vec::new(),
            // Only `RecGroup::materialize` builds a group member; the fn
            // being compiled here is created by tree-walked code.
            group: None,
        },
        c.loop_explain,
        c.n_escapes,
        escape_site,
    ))
}

struct Compiler<'a> {
    interp: &'a mut Interp,
    /// The creation env of the fn being compiled -- and, because
    /// `MakeClosure` hands every nested closure this same env, of every fn
    /// nested inside it too. Used for capture-free symbol resolution and for
    /// deciding macro-ness of a call head.
    env: Env,
    env_is_root: bool,
    /// The root env, for interning global cells.
    globals: Env,
    /// field1/W-EXPLAIN: one entry per `loop` form `compile_loop` has
    /// finished (specialized or not), across THIS fn and every `fn` nested
    /// inside it (`compile_fn_expr` shares the same `Compiler`). Read out by
    /// `compile::compile_fn` once the whole top-level fn compiles
    /// successfully; discarded (along with everything else `c` holds) if
    /// the fn ends up bailing.
    loop_explain: Vec<LoopExplain>,
    /// field3/W-RESOLVE: how many `Ir::Escape` nodes this fn (and every fn
    /// nested inside it, which shares this `Compiler`) has emitted. Read
    /// out by `compile::compile_fn` for the explain record, so
    /// `MOVA_EXPLAIN=1` says "compiles (N interop escapes)" instead of
    /// silently claiming a clean compile. Discarded with the rest of `c`
    /// if the fn bails anyway.
    n_escapes: usize,
    /// field4/W-LENS-1: identity of the fn being compiled, kept so
    /// [`Compiler::lens_escape_site`] can allocate a regret-ledger site
    /// LAZILY -- only if this fn actually emits an `Ir::Escape`. Eager
    /// allocation would burn one site id per `defn` in the bootstrap
    /// (well over a thousand, nearly all clean) for nothing.
    lens_name: Option<Str>,
    lens_span: Span,
    /// The site id, once [`Compiler::lens_escape_site`] has minted it, else
    /// `lens::NO_SITE`. Stamped into every `Ir::Escape` this compile emits
    /// (see `ir::Escape::lens_site`) and shared with nested fns exactly like
    /// `n_escapes`/`loop_explain`: a nested fn compiled as part of this one
    /// has no separate compile decision to attribute.
    lens_site: u32,
}

/// How a name resolves inside the fn being compiled.
#[derive(Clone, Copy)]
enum Binder {
    Slot(u16),
    /// The fn's own name. Pushed in its own scope ABOVE the params, so it
    /// shadows a same-named param -- matching `run_closure_body`, which
    /// binds the params first and the self-name second (constraint 4).
    SelfName,
    /// Member *i* of the RECURSIVE BINDING GROUP this fn is a member of
    /// (`ir::Ir::MakeRecGroup`) -- a `letfn` sibling, by index.
    ///
    /// Lives in `FnCtx::siblings` rather than in a `scopes` entry, for two
    /// reasons: `compile_arity` resets `scopes` per arity, and a group name
    /// must be visible in EVERY arity of the member; and it is the
    /// OUTERMOST binder of the member fn, shadowed by the fn's own params,
    /// self-name and every `let`/`loop`/`catch` inside it -- which is
    /// exactly what "consulted only after `scopes` misses" says.
    Sibling(u16),
}

struct RecurTarget {
    scratch_base: u16,
    arity: usize,
}

/// Per-FN compile state: the lexical scope stack, the slot allocator, the
/// `recur` target stack, the poisoned sibling names of any `let`/`loop`
/// binding list currently being compiled, this fn's capture list, and the
/// ENCLOSING fn's state (`parent`) for nested `fn` forms.
///
/// `scopes`/`next_slot`/`recur`/`poison` are reset per arity (each arity is
/// its own frame layout); `captures` is per fn, since every arity of one
/// closure instance shares one `captures` Vec.
struct FnCtx {
    scopes: Vec<Vec<(Symbol, Binder)>>,
    next_slot: u16,
    recur: Vec<RecurTarget>,
    /// (name, deferred read safe): safe when the tree-walker binds that name in a later frame than the reading closure's.
    poison: Vec<(Symbol, bool)>,
    depth: usize,
    self_name: Option<Symbol>,
    captures: Vec<CaptureSrc>,
    /// Every name a NESTED fn has captured out of this fn's scopes, in
    /// capture order. Read by `compile_binds`'s rebinding guard (see this
    /// module's doc, "Capture-by-value vs. the tree-walker's one-frame
    /// `let`"); never truncated, since a capture recorded while compiling an
    /// inner binding vector still matters to every vector enclosing it.
    capture_syms: Vec<Symbol>,
    captured_names: Vec<Symbol>,
    /// Non-empty exactly when the fn being compiled is a MEMBER of a
    /// recursive binding group: the run's names, in binding order, paired
    /// with the member index each one denotes. See [`Binder::Sibling`] for
    /// why this is a field rather than a `scopes` entry, and
    /// `Compiler::compile_rec_group` for what fills it.
    siblings: Vec<(Symbol, u16)>,
    parent: Option<Box<FnCtx>>,
}

impl FnCtx {
    fn new(self_name: Option<Symbol>, depth: usize, parent: Option<Box<FnCtx>>) -> Self {
        FnCtx {
            scopes: Vec::new(),
            next_slot: 0,
            recur: Vec::new(),
            poison: Vec::new(),
            depth,
            self_name,
            captures: Vec::new(),
            capture_syms: Vec::new(),
            captured_names: Vec::new(),
            siblings: Vec::new(),
            parent,
        }
    }

    /// An empty stand-in, used only to move a real `FnCtx` out of a `&mut`
    /// while it serves as a nested fn's `parent` (see `compile_fn_expr`).
    fn placeholder() -> Self {
        FnCtx::new(None, 0, None)
    }

    fn alloc_slot(&mut self) -> CResult<u16> {
        let s = self.next_slot;
        match self.next_slot.checked_add(1) {
            Some(n) => self.next_slot = n,
            None => return bail("too many local bindings -- slot allocator overflowed u16", NO_SPAN),
        }
        Ok(s)
    }

    fn lookup(&self, sym: &Symbol) -> Option<Binder> {
        // A namespace/alias-qualified symbol never binds to a slot: a
        // binding form is always a bare symbol, so `mode/state` must skip
        // every lexical scope and reach the global cell, never a `state`
        // param/let/loop slot that merely shares its bare name. Mirrors
        // `Env::get_local`'s identical rule in the tree-walker exactly --
        // see that fn's doc comment.
        if sym.ns.is_some() {
            return None;
        }
        for scope in self.scopes.iter().rev() {
            // Reverse within a scope too: a later `let` binding shadows an
            // earlier one of the same name, exactly like re-`set`ting it in
            // the tree-walker's single child env would.
            for (s, b) in scope.iter().rev() {
                if s == sym {
                    return Some(*b);
                }
            }
        }
        // A recursive binding group's member names bind OUTSIDE everything
        // above (see `Binder::Sibling`), so they are asked last -- a param,
        // a self-name or any inner `let` of the same spelling shadows them,
        // which is what the tree-walker's env chain does with the `letfn`
        // frame sitting outermost.
        self.siblings
            .iter()
            .rev()
            .find(|(s, _)| s == sym)
            .map(|(_, i)| Binder::Sibling(*i))
    }

    /// Is `sym` bound lexically anywhere in this fn or an enclosing one?
    /// Read-only (it must not intern a capture): used for the "is this call
    /// head a macro?" test, which `eval_list` answers with `env.get`, and a
    /// local always shadows a global macro there.
    fn bound_anywhere(&self, sym: &Symbol) -> bool {
        if self.lookup(sym).is_some() {
            return true;
        }
        match &self.parent {
            Some(p) => p.bound_anywhere(sym),
            None => false,
        }
    }

    fn bind(&mut self, sym: Symbol, b: Binder) -> CResult<()> {
        match self.scopes.last_mut() {
            // Unreachable in practice -- every caller pushes a scope before
            // binding into it -- kept as a bail rather than an `unwrap` so a
            // future caller mistake fails safe.
            None => return bail("internal: no open lexical scope to bind into", NO_SPAN),
            Some(scope) => scope.push((sym, b)),
        }
        Ok(())
    }

    /// Records a capture source, reusing an existing index for an identical
    /// one (two references to the same enclosing binding are one capture).
    fn intern_capture(&mut self, src: CaptureSrc, sym: &Symbol, span: Span) -> CResult<u16> {
        const TOO_MANY: &str = "too many distinct captures for u16 index";
        if let Some(i) = self.captures.iter().position(|c| same_src(c, &src)) {
            return u16::try_from(i).map_err(|_| ()).or_else(|()| bail(TOO_MANY, span));
        }
        let i = u16::try_from(self.captures.len()).map_err(|_| ()).or_else(|()| bail(TOO_MANY, span))?;
        self.captures.push(src);
        self.capture_syms.push(sym.clone());
        Ok(i)
    }
}

fn same_src(a: &CaptureSrc, b: &CaptureSrc) -> bool {
    match (a, b) {
        (CaptureSrc::Slot(x), CaptureSrc::Slot(y)) => x == y,
        (CaptureSrc::Capture(x), CaptureSrc::Capture(y)) => x == y,
        (CaptureSrc::SelfRef, CaptureSrc::SelfRef) => true,
        _ => false,
    }
}

impl Compiler<'_> {
    /// Compiles one arity into `ctx`, resetting its per-arity frame state.
    fn compile_arity(&mut self, ctx: &mut FnCtx, arity: &Arity) -> CResult<CompiledArity> {
        let n_params = arity.params.len();
        let variadic = arity.rest.is_some();
        let n_recur = n_params + usize::from(variadic);
        if n_recur > u16::MAX as usize / 2 {
            let span = arity.body.first().map_or(NO_SPAN, |f| f.span);
            return bail("too many parameters -- recur/scratch slot count would overflow u16", span);
        }
        let mut params: Vec<(Symbol, Binder)> = Vec::with_capacity(n_recur);
        for (i, p) in arity.params.iter().enumerate() {
            params.push((p.clone(), Binder::Slot(i as u16)));
        }
        if let Some(r) = &arity.rest {
            params.push((r.clone(), Binder::Slot(n_params as u16)));
        }
        ctx.scopes = vec![params];
        if let Some(name) = &ctx.self_name {
            ctx.scopes.push(vec![(name.clone(), Binder::SelfName)]);
        }
        let scratch_base = n_recur as u16;
        ctx.next_slot = scratch_base + n_recur as u16;
        ctx.recur = vec![RecurTarget {
            scratch_base,
            arity: n_recur,
        }];
        ctx.poison.clear();
        let mut body = self.compile_body(ctx, &arity.body)?;
        // Perceus-lite phase 2, a post-pass for the same reason
        // `specialize_num_loop` is one: it reads FINISHED IR, in slot
        // indices, so it cannot change what the code means -- it only
        // decides which slot reads may move instead of clone. See
        // `compile::lastuse` for the analysis and its safety conditions.
        if self.interp.lastuse_enabled() && !super::lastuse::disabled_by_env() {
            super::lastuse::analyze_arity(
                &mut body,
                ctx.next_slot as usize,
                n_recur,
                scratch_base,
            );
        }
        Ok(CompiledArity {
            n_params,
            variadic,
            n_recur,
            scratch_base,
            n_slots: ctx.next_slot as usize,
            body,
            jit: crate::jit::JitSlot::pending(),
            threaded: crate::jit::ThreadedSlot::pending(),
        })
    }

    fn compile_body(&mut self, ctx: &mut FnCtx, forms: &[Form]) -> CResult<Vec<Ir>> {
        forms.iter().map(|f| self.compile_form(ctx, f)).collect()
    }

    fn compile_form(&mut self, ctx: &mut FnCtx, form: &Form) -> CResult<Ir> {
        ctx.depth += 1;
        if ctx.depth > MAX_COMPILE_DEPTH {
            return bail("compile-time recursion depth exceeded MAX_COMPILE_DEPTH (self-expanding macro or pathologically nested source)", form.span);
        }
        let out = self.compile_form_inner(ctx, form);
        ctx.depth -= 1;
        out
    }

    fn compile_form_inner(&mut self, ctx: &mut FnCtx, form: &Form) -> CResult<Ir> {
        // S5/M3: `^{...}` on an evaluated form inside a compiled fn body.
        // BAIL to the tree-walker (which does implement it, see
        // `Interp::eval_form_with_meta`) rather than growing the `Ir`
        // enum with a variant that would also have to be taught to
        // `exec`, to `lastuse`'s liveness analysis, and to `lanes` --
        // three places where a subtle mistake is a miscompile, not a
        // missing feature.
        //
        // The trade is deliberate and small: `^meta` in *expression*
        // position inside a fn body is rare (the everyday spellings are
        // `with-meta` at runtime, and `^:private`/`^:dynamic`/type hints
        // at the TOP level or on a parameter -- none of which reach
        // here; a hinted parameter is handled by `compile_binding_pattern`
        // and never loses its binding, since metadata is a `Form` FIELD,
        // see `reader::Form::meta`). A symbol form is excluded for the
        // same reason `eval_form_in` excludes it: the reader's metadata
        // belongs to the symbol, not to the value it resolves to.
        if form.meta.is_some() && !matches!(&form.value, FormValue::Atom(Value::Sym(_))) {
            return bail("metadata on a non-symbol form -- ^meta in expression position runs only in the tree-walker", form.span);
        }
        match &form.value {
            FormValue::Atom(Value::Sym(sym)) => self.resolve_symbol(ctx, sym, form.span),
            FormValue::Atom(Value::Keyword(k)) => {
                // S5: compiled-tier counterpart of `eval::Interp::
                // eval_form_in`'s keyword-literal interning arm -- a
                // keyword literal baked into an `Ir::Const` here never
                // takes that tree-walk path at all, so `find-keyword`
                // needs this chokepoint too. Interns once per compile
                // (not per call), which is fine: the registry is a set.
                self.interp.keywords.intern(k.text_ref());
                Ok(Ir::Const(Value::Keyword(k.clone())))
            }
            FormValue::Atom(v) => Ok(Ir::Const(v.clone())),
            FormValue::Vector(items) => {
                let parts = self.compile_body(ctx, items)?;
                Ok(match const_values(&parts) {
                    Some(vs) => Ir::Const(Value::Vector(vs.into_iter().collect())),
                    None => Ir::VectorLit(parts),
                })
            }
            FormValue::Set(items) => {
                let parts = self.compile_body(ctx, items)?;
                Ok(match const_values(&parts) {
                    Some(vs) => Ir::Const(Value::Set(vs.into_iter().collect())),
                    None => Ir::SetLit(parts),
                })
            }
            FormValue::Map(pairs) => {
                let mut parts = Vec::with_capacity(pairs.len());
                for (k, v) in pairs {
                    let ki = self.compile_form(ctx, k)?;
                    let vi = self.compile_form(ctx, v)?;
                    parts.push((ki, vi));
                }
                let all_const = parts
                    .iter()
                    .all(|(k, v)| matches!(k, Ir::Const(_)) && matches!(v, Ir::Const(_)));
                if all_const {
                    let mut m = PMap::new();
                    for (k, v) in &parts {
                        if let (Ir::Const(kv), Ir::Const(vv)) = (k, v) {
                            m.insert(kv.clone(), vv.clone());
                        }
                    }
                    return Ok(Ir::Const(Value::Map(m)));
                }
                Ok(Ir::MapLit(parts))
            }
            FormValue::List(items) => self.compile_list(ctx, items, form.span),
        }
    }

    fn compile_list(&mut self, ctx: &mut FnCtx, items: &[Form], span: Span) -> CResult<Ir> {
        // `()` self-evaluates, exactly like `eval_list`'s is_empty branch.
        if items.is_empty() {
            return Ok(Ir::Const(Value::List(PVec::new())));
        }
        if let FormValue::Atom(Value::Sym(sym)) = &items[0].value {
            // S6/Blocker-1: alias-qualified `clojure.core` heads (`core/let`)
            // dispatch the same as bare -- see
            // `ns::Interp::is_bare_or_core_alias`'s doc comment; the
            // tree-walker's `eval_list` uses the SAME gate.
            if self.interp.is_bare_or_core_alias(sym) {
                // Special forms are matched BEFORE locals and macros
                // (constraint 6) -- a local named `if` cannot shadow `if`.
                if let Some(r) = self.compile_special(ctx, sym.name.as_ref(), items, span) {
                    return r;
                }
            }
            // A bare-symbol head that isn't locally bound and whose current
            // binding is a macro: expand now and re-walk the expansion.
            if !ctx.bound_anywhere(sym) {
                // W-VARS-PRIV: this branch resolves the head through
                // `Interp::resolve_symbol` directly (to detect a macro),
                // bypassing `Resolver::resolve_symbol` below and the
                // privacy gate now installed there -- so a private macro
                // referenced cross-ns would otherwise expand silently.
                // Bail here (compile-time only; the tree-walker then
                // raises the real, exactly-worded error via `eval_list`'s
                // own gate on this same symbol).
                if self.interp.check_qualified_private(sym).is_some() {
                    return bail(
                        "qualified reference to a private var from another ns -- privacy error is raised by the tree-walker",
                        span,
                    );
                }
                if let Some(Value::Macro(closure)) = self.interp.resolve_symbol(&self.env, sym) {
                    // `&form`: the WHOLE unevaluated call, head included --
                    // see `Interp::macro_form_stack`'s doc.
                    let raw_call_form = Value::List(items.iter().map(crate::reader::form_to_value).collect());
                    let mx_t0 = super::ircost::enabled().then(std::time::Instant::now);
                    let expansion = self
                        .interp
                        .apply_macro(&closure, &items[1..], raw_call_form, span)
                        .map_err(|_| Bail {
                            reason: std::borrow::Cow::Borrowed(
                                "macro expansion failed -- let the tree-walker raise the real error",
                            ),
                            span,
                        })?;
                    // SPEC-W4: a lazily-`concat`-built expansion (`->`,
                    // `doto`, ..) has to be realized into concrete `Form`
                    // shape before it can be compiled -- see
                    // `Interp::realize_form_value`. Realizing it can run
                    // arbitrary user thunks, so a failure bails to the
                    // tree-walker exactly like the expansion itself does.
                    let form = self.interp.value_to_form_realized(&expansion, span).map_err(|_| Bail {
                        reason: std::borrow::Cow::Borrowed(
                            "macro expansion could not be realized -- let the tree-walker raise the real error",
                        ),
                        span,
                    })?;
                    if let Some(t0) = mx_t0 {
                        super::ircost::MACRO_NS.fetch_add(t0.elapsed().as_nanos() as u64, std::sync::atomic::Ordering::Relaxed);
                    }
                    return self.compile_form(ctx, &form);
                }
            }
        }
        // Callee is evaluated before the args, same as `eval_list`.
        let callee = self.compile_form(ctx, &items[0])?;
        let args = self.compile_body(ctx, &items[1..])?;
        Ok(match callee {
            Ir::GlobalRef {
                chain,
                sym,
                span: sym_span,
            } => {
                // A still-pristine builtin of a known name and arity gets an
                // `Intrinsic` node instead; it carries every part of the
                // `CallGlobal` it would otherwise be, so if the name IS
                // redefined later -- or shadowed in this namespace -- the
                // guard in `exec` performs exactly that call
                // (COMPILE-TIER-DESIGN.md's runtime guard).
                match intrin_op(sym.name.as_ref(), args.len(), &chain) {
                    Some(op) => Ir::Intrinsic {
                        op,
                        chain,
                        sym,
                        sym_span,
                        args,
                        span,
                    },
                    None => Ir::CallGlobal {
                        chain,
                        sym,
                        sym_span,
                        args,
                        span,
                    },
                }
            }
            Ir::CreationEnvLookup {
                sym,
                chain,
                span: sym_span,
            } => Ir::CallCreationEnv {
                sym,
                chain,
                sym_span,
                args,
                span,
            },
            other => Ir::Call {
                callee: Box::new(other),
                args,
                span,
            },
        })
    }

    /// `Some(..)` if `name` is one of `eval_special`'s special forms (which
    /// then either compiles or bails); `None` if the caller should treat
    /// the list as an ordinary macro/function call.
    ///
    /// `items` is the WHOLE list form's items, head included; `args` below
    /// is its tail, which is what every arm but the interop one wants. The
    /// interop arm (field3/W-RESOLVE) needs the head back so it can rebuild
    /// the untouched source form for `Ir::Escape` to tree-walk.
    fn compile_special(
        &mut self,
        ctx: &mut FnCtx,
        name: &str,
        items: &[Form],
        span: Span,
    ) -> Option<CResult<Ir>> {
        let args = &items[1..];
        match name {
            "if" => Some(self.compile_if(ctx, args, span)),
            "do" => Some(self.compile_body(ctx, args).map(Ir::Do)),
            "let" => Some(self.compile_let(ctx, args, span, false)),
            "letfn*" => Some(self.compile_let(ctx, args, span, true)),
            "loop" => Some(self.compile_loop(ctx, args, span)),
            "recur" => Some(self.compile_recur(ctx, args, span)),
            "fn" | "fn*" => Some(self.compile_fn_expr(ctx, args, span)),
            "try" => Some(self.compile_try(ctx, args)),
            "def" => Some(self.compile_def(ctx, args, span)),
            "quote" => Some(if args.len() == 1 {
                Ok(Ir::Const(crate::reader::form_to_value(&args[0])))
            } else {
                bail("quote: expected exactly 1 argument (malformed special form)", span)
            }),
            "throw" => Some(if args.len() == 1 {
                self.compile_form(ctx, &args[0]).map(|v| Ir::Throw {
                    value: Box::new(v),
                    span,
                })
            } else {
                bail("throw: expected exactly 1 argument (malformed special form)", span)
            }),
            // Recognized special forms the tier doesn't compile: the whole
            // fn falls back so the tree-walker handles them unchanged.
            // `var` (R2): resolving to an `Arc<VarCell>` at compile time
            // would freeze the SAME candidate-order lookup `eval_var` does
            // at access time, and namespace resolution is exactly the risk
            // area the differential suite exists to catch -- bailing is
            // simple and safe, and a `(var x)` inside a hot compiled fn
            // body is not the case this tier exists to speed up.
            //
            // `set!` (SPEC-D, lsp/setf): a target that is a deftype's own
            // mutable-field local (the `wrap_fields_let` paired-local shape
            // -- see `compile_set_bang`) compiles to `Ir::SetMutField`,
            // reading/writing the SAME `Mutex<PVec>` field storage
            // `eval_set_bang` uses. Every other target (a Var, or a plain
            // local with no owner-marker sibling) bails exactly as before
            // -- it resolves `sym` through the identical candidate-order
            // global lookup, and a compile-time-frozen cell reference would
            // risk silently disagreeing with the tree-walker the moment
            // namespace candidates shift under it.
            "set!" => Some(self.compile_set_bang(ctx, args, span)),
            // H2: `quasiquote` no longer bails the WHOLE fn -- it escapes
            // per node instead, exactly like `.`/`new` interop below (see
            // `Ir::Escape`'s doc). The escape tree-walks the `(quasiquote
            // ...)` form through `eval_quasiquote_top` -- the SAME fn the
            // tree-walker itself calls (`special_forms.rs`'s dispatch) --
            // with the fn's OWN lexical captures bridged in via
            // `compile_escape`'s env bridge, so semantics (auto-gensym,
            // ns-qualification, nested qq, splicing, metadata) are
            // byte-for-byte identical: it is not a reimplementation, it is
            // the same code. This is what turns clj-kondo's
            // `clj_kondo.impl.utils/get-in` helper fn `#(if (keyword? %) %
            // `(get ~%))` from "tree-walks -- quasiquote" into "compiles
            // with 1 interop escape".
            "quasiquote" => Some(self.compile_escape(ctx, items, span)),
            // C1: `binding`/`with-redefs` compile to `Ir::DynBind`, which
            // resolves vars at run time via the tree-walker's own candidate
            // walk and shares its push/pop code (special_forms.rs).
            "binding" | "with-redefs" => Some(self.compile_dyn_bind(ctx, name == "with-redefs", args, span)),
            "defmacro" | "ns" | "macroexpand-1" | "macroexpand" | "var" => {
                Some(bail(
                    match name {
                        "defmacro" => "defmacro in a fn body -- macro bodies are never compiled",
                        "ns" => "ns form in a fn body -- namespace declarations run only in the tree-walker",
                        "macroexpand-1" | "macroexpand" => "macroexpand/macroexpand-1 in a fn body -- needs the tree-walker's live macro table",
                        _ => "(var x) -- resolving to a frozen VarCell would risk disagreeing with eval_var's candidate-order lookup",
                    },
                    span,
                ))
            }
            // S3 type system: definitions AND the two symbol-shaped hooks
            // (`(.field x)` / `(Ctor. args)`) live only in the tree-walker
            // (`eval::types_forms`) -- bail so a fn body containing them
            // tree-walks instead of freezing an unresolved-symbol error
            // the tree-walker would not raise.
            // S5: `proxy` (host-class shims) and `definterface` join the
            // S3 row -- tree-walk only, same reasoning (both live only in
            // `eval::types_forms`).
            // D1: `reify` joins that row -- it lives only in
            // `eval::types_forms` too, and a compiled fn body containing
            // one must bail to the tree-walker rather than freeze an
            // unresolved-symbol error.
            // D5: `.` (the longhand interop form the `.field`/`Ctor.`
            // sugar below already bails for) joins that row for exactly
            // the same reason -- it rewrites into `eval_dot_form`/a
            // static call inside the tree-walker and has no compiled
            // counterpart, so a compiled fn body containing one must bail
            // rather than freeze an unresolved-symbol error.
            //
            // field3/W-RESOLVE: the TYPE-DEFINITION half of that row still
            // bails the whole fn -- those forms install globals, protocol
            // tables and host classes, and a fn body containing one is
            // never a hot loop, so there is nothing to buy. The INTEROP
            // CALL half (`.`/`new`, plus the `.method`/`Ctor.` sugar
            // below) no longer bails: it escapes per node instead, so the
            // interop-free rest of the fn compiles. See `Ir::Escape`.
            "defprotocol" | "defrecord" | "deftype" | "definterface" | "extend-type"
            | "extend-protocol" | "proxy" | "reify" => Some(bail(
                "type system form (defprotocol/defrecord/deftype/definterface/extend-type/extend-protocol/proxy/reify) -- lives only in eval::types_forms",
                span,
            )),
            "new" => Some(self.compile_new(ctx, items, span)),
            "." => Some(self.compile_escape(ctx, items, span)),
            // S4: `import` is a special form too (see `eval::types_forms::
            // eval_import`'s doc for why) -- same bail-to-tree-walker
            // reasoning as the S3 row above. `require`/`ns-name`/`find-ns`/
            // `the-ns`/`ns-resolve`/`all-ns`/`resolve` need no entry here:
            // they're ordinary native fns (`builtins::nsfns`), resolved
            // through the normal global-cell path both tiers already share.
            "import" => Some(bail("import -- lives only in eval::types_forms", span)),
            // S4 multimethods (eval::multi_forms): definitions only, same
            // reasoning as the S3 heads directly above -- ordinary calls
            // to an already-defined multimethod are plain native-fn calls
            // and need no bail at all.
            "defmulti" | "defmethod" => Some(bail(
                "defmulti/defmethod -- multimethod definitions live only in eval::multi_forms",
                span,
            )),
            other
                if (other.len() > 1 && other.starts_with('.') && other != "..")
                    || (other.len() > 1 && other.ends_with('.')) =>
            {
                // W-FIELDGET: `(.-field local)` is the one interop shape
                // that is hot enough to compile rather than escape (see
                // `ir::FieldGet`). It still BUILDS the escape -- the node
                // keeps it as its verbatim fallback -- so this is a pure
                // addition to the row above, exactly as `specialize_num_loop`
                // is to `Ir::Loop`.
                Some(
                    self.compile_escape(ctx, items, span)
                        .map(|esc| specialize_field_get(other, items, esc)),
                )
            }
            _ => None,
        }
    }

    /// field3/W-RESOLVE: compile ONE interop form into an `Ir::Escape`
    /// rather than bailing the fn that contains it.
    ///
    /// The form itself is not compiled at all -- it is kept verbatim and
    /// tree-walked by `exec`, which is the whole safety argument: identical
    /// forms, identical spans, identical `eval_form_in`, therefore identical
    /// values, errors, side-effect order and dynamic-binding visibility.
    /// All this function does is decide (a) that the escape is legal here
    /// and (b) which enclosing locals the escaped form must be able to see.
    ///
    /// (a) is one refusal: a `recur` written inside the escaped subtree.
    /// The tree-walker signals `recur` by throwing an `RjError` with
    /// `ErrorKind::Recur`; the compiled tier signals it with `Flow::Recur`
    /// and a scratch block. An escape boundary between the `recur` and its
    /// target would put those two disciplines on opposite sides of the same
    /// unwind, so the whole fn falls back exactly as it did before this
    /// node existed. Note this scans the RAW source: a `dotimes` inside an
    /// escaped form is still spelled `dotimes` here, and expands (per
    /// evaluation, in the tree-walker) only once the escape runs -- which
    /// is precisely the tree-walk behaviour being preserved.
    ///
    /// (b) is the bridge. Every bare symbol MENTIONED anywhere in the
    /// subtree (quoted or not, binding site or reference -- an
    /// over-approximation, and safe in that direction) is put through
    /// `resolve_lexical`, the same resolver every ordinary symbol reference
    /// uses. Names that land on a slot / an enclosing compiled fn's capture
    /// / the self-name are recorded; names that resolve no further are left
    /// out, because the bridge frame's PARENT is the creation env, where
    /// the tree-walker would have found them anyway.
    /// field4/W-LENS-1: get-or-mint this fn's escape site id. Cold: at most
    /// once per fn compile, and (via `Interp::lens_site_for`'s per-interp
    /// cache) at most one global-registry lock per distinct source site per
    /// interpreter, so a `fn` form re-evaluated inside a tree-walked loop
    /// never touches the registry twice.
    fn lens_escape_site(&mut self) -> u32 {
        if self.lens_site == crate::lens::NO_SITE {
            let name = self.lens_name.clone();
            let span = self.lens_span;
            let (site, _) =
                self.interp
                    .lens_site_for(crate::lens::SiteKind::Escape, name.as_ref(), span);
            self.lens_site = site;
        }
        self.lens_site
    }

    /// K5: `(new C a..)` with a bare, non-local class symbol -> `Ir::New` (args compiled, Escape kept as fallback).
    fn compile_new(&mut self, ctx: &mut FnCtx, items: &[Form], span: Span) -> CResult<Ir> {
        let esc = self.compile_escape(ctx, items, span)?;
        let Some(class) = items.get(1) else { return Ok(esc) };
        let FormValue::Atom(Value::Sym(sym)) = &class.value else { return Ok(esc) };
        if class.meta.is_some() || new_inst_off() || self.resolve_lexical(ctx, sym, false, span)?.is_some() {
            return Ok(esc);
        }
        let mut args = Vec::with_capacity(items.len() - 2);
        for a in &items[2..] {
            args.push(self.compile_form(ctx, a)?);
        }
        Ok(Ir::New(Box::new(NewInst { class: class.clone(), args, fallback: esc, span })))
    }

    fn compile_escape(&mut self, ctx: &mut FnCtx, items: &[Form], span: Span) -> CResult<Ir> {
        if items.iter().any(mentions_recur) {
            return bail(
                "interop form containing `recur` -- recur unwinds as an RjError in the tree-walker and as Flow::Recur in the compiled tier, so an escape boundary between it and its target would cross signalling disciplines",
                span,
            );
        }
        let mut names: Vec<Symbol> = Vec::new();
        for it in items {
            collect_bare_symbols(it, &mut names);
        }
        let mut binds: Vec<(Symbol, CaptureSrc)> = Vec::with_capacity(names.len());
        for sym in names {
            let src = match self.resolve_lexical(ctx, &sym, false, span)? {
                Some(Ir::LoadSlot(i)) => CaptureSrc::Slot(i),
                Some(Ir::LoadCapture(i)) => CaptureSrc::Capture(i),
                Some(Ir::SelfRef) => CaptureSrc::SelfRef,
                // An escaped interop form inside a recursive-binding-group
                // member that mentions a SIBLING name. The bridge builds a
                // tree-walked child frame of `l.me.env` and writes `binds`
                // into it -- and the sibling is not in that env at all (a
                // compiled group has no `let` frame; that is the whole
                // point), so the escaped form would resolve the name
                // outward, to a global of the same spelling or to nothing.
                // Bail the whole fn, exactly as this shape did before
                // groups existed. Bridging a sibling INTO the frame would
                // also hand the tree-walker a strong member handle inside
                // an env, re-creating the very cycle the group avoids.
                Some(Ir::SiblingRef(_)) => {
                    return bail(
                        "an escaped interop form mentions a letfn sibling -- the escape's env bridge has no frame to put a recursive-binding-group member in",
                        span,
                    )
                }
                // `resolve_lexical` only ever returns those four.
                Some(_) => return bail("internal: resolve_lexical returned an unexpected Ir node", span),
                None => continue,
            };
            binds.push((sym, src));
        }
        self.n_escapes += 1;
        let lens_site = self.lens_escape_site();
        Ok(Ir::Escape(Box::new(Escape {
            lens_site,
            // The list form, rebuilt exactly as the reader produced it.
            // `meta: None` is not a loss: `compile_form_inner` bails on
            // metadata attached to any non-symbol form before this is ever
            // reached, so a list form arriving here provably had none.
            form: Form::bare(FormValue::List(items.to_vec()), span),
            binds,
            span,
        })))
    }

    fn compile_if(&mut self, ctx: &mut FnCtx, args: &[Form], span: Span) -> CResult<Ir> {
        if args.len() < 2 || args.len() > 3 {
            // malformed: let the tree-walker raise it
            return bail("if: expected 2 or 3 arguments (malformed special form)", span);
        }
        let test = Box::new(self.compile_form(ctx, &args[0])?);
        let then = Box::new(self.compile_form(ctx, &args[1])?);
        let els = match args.get(2) {
            Some(f) => Some(Box::new(self.compile_form(ctx, f)?)),
            None => None,
        };
        Ok(Ir::If { test, then, els })
    }

    /// Shared `let`/`loop` binding-vector compiler: validates the shape,
    /// compiles each init under the poison rule, then compiles that
    /// binding's PATTERN (allocating a slot per name it introduces and
    /// pushing them into `ctx`'s innermost scope as it goes, so a later init
    /// sees every earlier binding).
    ///
    /// The caller must have pushed the scope this binds into, and -- for
    /// `loop` -- must NOT yet have pushed the loop's own recur target: an
    /// init containing `recur` targets the ENCLOSING loop/fn, because
    /// `eval_loop` evaluates its inits before it ever starts trampolining.
    ///
    /// `rec_groups` enables the `letfn`-shaped RECURSIVE BINDING GROUP (see
    /// `rec_group_run` and `Compiler::compile_rec_group`) and is true only
    /// for `let`. A `loop` is excluded because a group collapses its whole
    /// run into ONE binding entry, and a `loop`'s binding COUNT is its
    /// `recur` arity -- changing it would change what `(recur ...)` means.
    /// `letfn` expands to a `let`, so nothing this feature exists for is
    /// lost; a mutually-recursive run written directly in a `loop` vector
    /// keeps today's bail.
    fn compile_binds(
        &mut self,
        ctx: &mut FnCtx,
        items: &[Form],
        span: Span,
        rec_groups: bool,
    ) -> CResult<Vec<(CompiledPattern, Ir)>> {
        if !items.len().is_multiple_of(2) {
            return bail("let/loop binding vector has an odd number of forms", span);
        }
        let n = items.len() / 2;
        // Every name of every not-yet-bound binding is poison, per pattern
        // (a compound pattern introduces several).
        let mut names: Vec<Vec<Symbol>> = Vec::with_capacity(n);
        for i in 0..n {
            names.push(pattern_names(&items[i * 2])?);
        }
        let mut binds = Vec::with_capacity(n);
        let (fi, fb) = self.tree_walker_frames(ctx, items, &names, rec_groups);
        // Start of the captures made by closures living in the current tree-walker frame.
        let mut frame_mark = ctx.captured_names.len();
        let mut i = 0;
        while i < n {
            // The `letfn` shape: a maximal run of >= 2 consecutive
            // plain-symbol bindings whose inits are `fn` literals, at least
            // one of which mentions another run name. Compiled as ONE
            // `Ir::MakeRecGroup` covering the whole run, which is what
            // replaces the deferred-read bail for this shape. Every other
            // shape -- and every binding outside a run -- takes the path
            // below, unchanged.
            if rec_groups {
                if let Some(run) = rec_group_run(items, &names, i) {
                    let poison_start = ctx.poison.len();
                    push_poison(ctx, &names, &fi, &fb, i);
                    let out = self.compile_rec_group(ctx, items, i, &run, frame_mark, span);
                    ctx.poison.truncate(poison_start);
                    binds.push(out?);
                    i += run.len();
                    continue;
                }
            }
            let poison_start = ctx.poison.len();
            push_poison(ctx, &names, &fi, &fb, i);
            // The poison window covers the pattern too, not just the init:
            // a `:or` default is an expression, and the names its own
            // pattern binds AFTER it are just as unbound as a later
            // binding's would be.
            let init = self.compile_form(ctx, &items[i * 2 + 1]);
            let init_span = items[i * 2 + 1].span;
            if fb[i] > fi[i] {
                // Tree-walker opens a new frame here: earlier closures keep the old binding, like by-value capture.
                frame_mark = ctx.captured_names.len();
            }
            let out = init.and_then(|init| {
                if names[i].iter().any(|nm| ctx.captured_names[frame_mark..].contains(nm)) {
                    // A closure made earlier in the SAME tree-walker frame
                    // captured this name by value, but the tree-walker would
                    // have it read the binding we are about to make (letfn-style
                    // shared frame). Fall back rather than disagree.
                    return bail(
                        "a nested fn captured this name earlier in the same binding vector, which is about to rebind it (letfn-style one-frame let)",
                        init_span,
                    );
                }
                let pat = self.compile_pattern(ctx, &items[i * 2])?;
                Ok((pat, init))
            });
            ctx.poison.truncate(poison_start);
            binds.push(out?);
            i += 1;
        }
        Ok(binds)
    }

    /// Mirrors `eval_let`/`eval_loop`'s frame splitting: `fi[k]` is the
    /// frame binding k's init runs in, `fb[k]` the frame its names land in.
    /// A new frame opens when a fn literal appeared since the last split, the
    /// binding shadows a resolvable name, and it is not in a letfn run.
    fn tree_walker_frames(&self, ctx: &FnCtx, items: &[Form], names: &[Vec<Symbol>], rec_groups: bool) -> (Vec<u32>, Vec<u32>) {
        let n = names.len();
        let fn_bound: Vec<bool> = (0..n)
            .map(|k| matches!(&items[k * 2].value, FormValue::Atom(Value::Sym(_))) && form_is_fn_literal(&items[k * 2 + 1]))
            .collect();
        let in_run = |k: usize| rec_groups && fn_bound[k];
        let (mut fi, mut fb) = (Vec::with_capacity(n), Vec::with_capacity(n));
        let (mut frame, mut saw_fn) = (0u32, false);
        for k in 0..n {
            fi.push(frame);
            let shadow = names[k].iter().any(|nm| {
                names[..k].iter().any(|g| g.contains(nm))
                    || ctx_chain_binds(ctx, nm)
                    || self.interp.resolve_symbol(&self.env, nm).is_some()
            });
            if saw_fn && shadow && !in_run(k) {
                frame += 1;
                saw_fn = false;
            }
            fb.push(frame);
            saw_fn |= form_has_nested_fn(&items[k * 2 + 1]);
        }
        (fi, fb)
    }

    /// One recursive binding group: `run.len()` consecutive bindings
    /// starting at binding index `start`, each a bare symbol bound to a `fn`
    /// literal, compiled into a single `Ir::MakeRecGroup`.
    ///
    /// Two phases, and the order is the whole trick:
    ///
    /// 1. every member's template is compiled with the run's names in scope
    ///    as [`Binder::Sibling`]s -- so a reference to any member, FORWARD
    ///    or BACKWARD, resolves to an `Ir::SiblingRef` rather than to a bail
    ///    or to a by-value slot capture. Nothing is bound in `ctx` yet, so a
    ///    backward reference cannot accidentally find an earlier member's
    ///    slot (which is the cycle trap: a sibling captured by value puts
    ///    the `Arc` cycle straight back).
    /// 2. only THEN are the slots allocated and the names bound as ordinary
    ///    `Binder::Slot`s, so every later init and the `let` BODY read the
    ///    members with a plain `LoadSlot` -- by then the slots are filled,
    ///    and capturing a materialized member by value from OUTSIDE the
    ///    group is cycle-free (outsider -> member -> group -> weak members).
    fn compile_rec_group(
        &mut self,
        ctx: &mut FnCtx,
        items: &[Form],
        start: usize,
        run: &[Symbol],
        frame_mark: usize,
        span: Span,
    ) -> CResult<(CompiledPattern, Ir)> {
        if u16::try_from(run.len()).is_err() {
            return bail("recursive binding group larger than u16 member indices", span);
        }
        let mut members: Vec<RecMember> = Vec::with_capacity(run.len());
        for k in 0..run.len() {
            let init = &items[(start + k) * 2 + 1];
            let fn_args = match &init.value {
                // `rec_group_run` already proved this shape.
                FormValue::List(l) => &l[1..],
                _ => return bail("internal: recursive binding group member is not a fn form", init.span),
            };
            let (template, caps) = self.compile_fn_template(ctx, fn_args, init.span, run)?;
            members.push(RecMember { template, caps });
        }
        // The same one-frame-`let` guard the ordinary path applies: a nested
        // fn created EARLIER in this very vector may have captured one of
        // these names by value, which the tree-walker would have read out of
        // the mutated frame instead.
        if run.iter().any(|nm| ctx.captured_names[frame_mark..].contains(nm)) {
            return bail(
                "a nested fn captured this name earlier in the same binding vector, which is about to rebind it (letfn-style one-frame let)",
                span,
            );
        }
        let mut slots = Vec::with_capacity(run.len());
        for name in run {
            let s = ctx.alloc_slot()?;
            ctx.bind(name.clone(), Binder::Slot(s))?;
            slots.push(s);
        }
        // The node writes every slot itself and evaluates to member 0, which
        // this pattern then writes into `slots[0]` a second time -- see
        // `Ir::MakeRecGroup`'s doc for why that beats a third binding-step
        // shape.
        Ok((
            CompiledPattern::Slot(slots[0]),
            Ir::MakeRecGroup { members, slots },
        ))
    }

    /// `(set! sym expr)`. Tries the deftype-mutable-field pattern first
    /// (see `compile_mut_field_set`); every other target -- a Var, a plain
    /// local, a namespace-qualified symbol -- bails with the same message
    /// this arm always gave, so `eval_set_bang` still runs it unchanged.
    fn compile_set_bang(&mut self, ctx: &mut FnCtx, args: &[Form], span: Span) -> CResult<Ir> {
        if let Some(ir) = self.compile_mut_field_set(ctx, args, span)? {
            return Ok(ir);
        }
        bail(
            "set! -- resolves its target through the tree-walker's candidate-order global lookup",
            span,
        )
    }

    /// `Some(ir)` iff `(set! sym expr)` targets a deftype's own mutable
    /// field local, recognized from `wrap_fields_let`'s paired-local shape:
    /// `sym` bound to a plain slot in THIS fn's own scope chain (never a
    /// capture -- `FnCtx::lookup` never crosses a nested-`fn` boundary,
    /// which is exactly the case this must NOT handle -- see
    /// `Ir::SetMutField`'s doc), plus a sibling local named
    /// `__mutfield_owner_<sym>` also bound to a plain slot in the same
    /// chain. `None` for every other shape (a Var, an ordinary local with
    /// no owner marker, a namespace-qualified symbol, wrong arity), which
    /// sends the caller to the unchanged bail.
    fn compile_mut_field_set(
        &mut self,
        ctx: &mut FnCtx,
        args: &[Form],
        span: Span,
    ) -> CResult<Option<Ir>> {
        if args.len() != 2 {
            return Ok(None);
        }
        let FormValue::Atom(Value::Sym(sym)) = &args[0].value else {
            return Ok(None);
        };
        if sym.ns.is_some() {
            return Ok(None);
        }
        let Some(Binder::Slot(field_slot)) = ctx.lookup(sym) else {
            return Ok(None);
        };
        let owner_sym = Symbol::simple(format!("__mutfield_owner_{}", sym.name));
        let Some(Binder::Slot(owner_slot)) = ctx.lookup(&owner_sym) else {
            return Ok(None);
        };
        let value = self.compile_form(ctx, &args[1])?;
        Ok(Some(Ir::SetMutField {
            owner_slot,
            field_slot,
            field_name: sym.name.clone(),
            ic: FieldIc::new(),
            value: Box::new(value),
            span,
        }))
    }

    fn compile_let(&mut self, ctx: &mut FnCtx, args: &[Form], span: Span, letfn: bool) -> CResult<Ir> {
        let items = match args.first().map(|f| &f.value) {
            Some(FormValue::Vector(v)) => v.as_slice(),
            _ => return bail("let: first argument isn't a binding vector (malformed special form)", span),
        };
        let binds_span = args[0].span;
        ctx.scopes.push(Vec::new());
        let out = (|| {
            let binds = self.compile_binds(ctx, items, binds_span, letfn)?;
            let body = self.compile_body(ctx, &args[1..])?;
            Ok(Ir::Let { binds, body })
        })();
        ctx.scopes.pop();
        out
    }

    fn compile_loop(&mut self, ctx: &mut FnCtx, args: &[Form], span: Span) -> CResult<Ir> {
        let items = match args.first().map(|f| &f.value) {
            Some(FormValue::Vector(v)) => v.as_slice(),
            _ => return bail("loop: first argument isn't a binding vector (malformed special form)", span),
        };
        let binds_span = args[0].span;
        ctx.scopes.push(Vec::new());
        let out = (|| {
            let binds = self.compile_binds(ctx, items, binds_span, false)?;
            // One scratch slot per binding PAIR -- `eval_loop`'s `__loopN`
            // raw-value holders, which is what makes `recur`'s arity the
            // binding count even when a binding is a compound pattern.
            let n = binds.len();
            let scratch_base = ctx.next_slot;
            for _ in 0..n {
                ctx.alloc_slot()?;
            }
            ctx.recur.push(RecurTarget {
                scratch_base,
                arity: n,
            });
            let body = self.compile_body(ctx, &args[1..]);
            ctx.recur.pop();
            // A scalar-arithmetic loop gets an `Ir::NumLoop` wrapper that
            // keeps this very node as its fallback. Anything else -- and
            // everything, with the specialization switched off -- is
            // returned unchanged.
            let generic = Ir::Loop {
                binds,
                scratch_base,
                body: body?,
            };
            Ok(if self.interp.numloop_enabled() {
                let (ir, decision) =
                    specialize_num_loop(generic, self.interp.lanes_enabled(), self.interp.superloop_enabled());
                self.loop_explain.push(LoopExplain {
                    span,
                    source_id: self.interp.source_id,
                    decision,
                });
                ir
            } else {
                self.loop_explain.push(LoopExplain {
                    span,
                    source_id: self.interp.source_id,
                    decision: LoopDecision::Generic {
                        reason: "this interpreter's numloop_enabled switch is off (differential-suite A/B, not MOVA_NO_NUMLOOP)",
                    },
                });
                generic
            })
        })();
        ctx.scopes.pop();
        out
    }

    fn compile_recur(&mut self, ctx: &mut FnCtx, args: &[Form], span: Span) -> CResult<Ir> {
        let (scratch_base, arity) = match ctx.recur.last() {
            Some(t) => (t.scratch_base, t.arity),
            None => return bail("recur outside any loop/fn tail position", span),
        };
        // An arg-count mismatch is a *runtime* error in the tree-walker,
        // with a message that names the enclosing form; falling back keeps
        // that error's wording and timing exactly as it is today.
        if args.len() != arity {
            return bail("recur: argument count doesn't match its target's arity (runtime error in the tree-walker)", span);
        }
        let args = self.compile_body(ctx, args)?;
        Ok(Ir::Recur { args, scratch_base })
    }

    /// A nested `(fn ...)` form -> `Ir::MakeClosure`. The template is
    /// compiled HERE, once, no matter how many closure instances the node
    /// later produces; only the capture snapshot is per instance.
    fn compile_fn_expr(&mut self, ctx: &mut FnCtx, args: &[Form], span: Span) -> CResult<Ir> {
        let (template, caps) = self.compile_fn_template(ctx, args, span, &[])?;
        Ok(Ir::MakeClosure { template, caps })
    }

    /// The body of [`Compiler::compile_fn_expr`], shared with
    /// `compile_rec_group`: compiles a nested `fn` form's template against
    /// `ctx` as its enclosing scope and returns it with the capture list the
    /// creating node must snapshot.
    ///
    /// `siblings` is empty for an ordinary nested fn and, for a member of a
    /// recursive binding group, is the run's names in binding order -- which
    /// become that member's [`Binder::Sibling`] binders, so a reference to
    /// any member of the run (its own name included, unless the `fn`'s own
    /// self-name or a closer binding shadows it) resolves to an
    /// `Ir::SiblingRef` instead of bailing (forward) or capturing by value
    /// (backward).
    fn compile_fn_template(
        &mut self,
        ctx: &mut FnCtx,
        args: &[Form],
        span: Span,
        siblings: &[Symbol],
    ) -> CResult<(Arc<FnTemplate>, Vec<CaptureSrc>)> {
        // A malformed `fn` must stay a *runtime* error with the
        // tree-walker's own wording, so a parse failure falls back.
        let (name, arities) = self.interp.parse_fn_like(args, span).map_err(|_| Bail {
            reason: std::borrow::Cow::Borrowed("malformed fn/fn* form (runtime error in the tree-walker)"),
            span,
        })?;
        let arities = Arc::new(arities);

        // Move the enclosing state in as the nested fn's parent scope, so
        // free symbols can resolve through it into captures.
        let enclosing = std::mem::replace(ctx, FnCtx::placeholder());
        let depth = enclosing.depth;
        let mut child = FnCtx::new(
            name.clone().map(Symbol::simple),
            depth,
            Some(Box::new(enclosing)),
        );
        child.siblings = siblings
            .iter()
            .enumerate()
            .map(|(i, s)| (s.clone(), i as u16))
            .collect();
        let mut compiled = Vec::with_capacity(arities.len());
        // field1/W-EXPLAIN: a nested fn cannot fall back on its own (its
        // `MakeClosure` node is already compiled into the parent), so `Bail`
        // propagates all the way out to whichever TOP-LEVEL fn is being
        // compiled -- but the INNER reason is what is actually useful to a
        // reader (the outer fn's own body may be perfectly ordinary; it is
        // the nested one that hit a dot-form/binding/whatever), so it is
        // carried through, span and all, with a "nested fn: " prefix rather
        // than being discarded and replaced with a generic `Bail`.
        let mut inner_bail: Option<Bail> = None;
        for arity in arities.iter() {
            match self.compile_arity(&mut child, arity) {
                Ok(a) => compiled.push(a),
                Err(b) => {
                    inner_bail = Some(b);
                    break;
                }
            }
        }
        // Put the enclosing state back before propagating anything.
        if let Some(p) = child.parent.take() {
            *ctx = *p;
        }
        if let Some(Bail { reason, span }) = inner_bail {
            return Err(Bail {
                reason: std::borrow::Cow::Owned(format!("nested fn: {reason}")),
                span,
            });
        }
        Ok((
            Arc::new(FnTemplate {
                code: Arc::new(CompiledFn {
                    name,
                    arities: compiled,
                    capture_syms: child.capture_syms,
                }),
                arities,
            }),
            child.captures,
        ))
    }

    /// C1: `(binding [v e ...] body)` / `(with-redefs ...)` -> `Ir::DynBind`.
    /// Every shape `resolve_binding_pairs` rejects at run time (no vector,
    /// odd count, non-symbol name) falls back, keeping that error's timing.
    fn compile_dyn_bind(&mut self, ctx: &mut FnCtx, redefs: bool, args: &[Form], span: Span) -> CResult<Ir> {
        let Some(FormValue::Vector(pairs)) = args.first().map(|f| &f.value) else {
            return bail("binding/with-redefs: malformed bindings vector (runtime error in the tree-walker)", span);
        };
        if pairs.len() % 2 != 0 {
            return bail("binding/with-redefs: odd bindings vector (runtime error in the tree-walker)", span);
        }
        let mut out = Vec::with_capacity(pairs.len() / 2);
        for pair in pairs.chunks(2) {
            let FormValue::Atom(Value::Sym(sym)) = &pair[0].value else {
                return bail("binding/with-redefs: non-symbol binding name (runtime error in the tree-walker)", pair[0].span);
            };
            // Inits run in the fn's own lexical scope (binding adds no locals).
            let init = self.compile_form(ctx, &pair[1])?;
            out.push((sym.clone(), pair[0].span, init));
        }
        let body = self.compile_body(ctx, &args[1..])?;
        Ok(Ir::DynBind(Box::new(DynBind {
            redefs,
            pairs: out,
            body,
            span,
        })))
    }

    /// `try` with any number of `catch` clauses (C3g: was at most one) and
    /// one `finally`, split exactly like `eval_try` splits it: ANY list
    /// whose head is the bare symbol `catch`/`finally` is a clause wherever
    /// it appears, everything else is body. Every shape `eval_try` reports
    /// as a runtime error (a catch with no binding, a non-symbol binding,
    /// two `finally`s) falls back instead, so that error keeps its wording
    /// and timing.
    fn compile_try(&mut self, ctx: &mut FnCtx, args: &[Form]) -> CResult<Ir> {
        let mut body: Vec<&Form> = Vec::new();
        let mut catches: Vec<(Option<Symbol>, Symbol, &[Form])> = Vec::new();
        let mut finally: Option<&[Form]> = None;
        for a in args {
            if let FormValue::List(items) = &a.value {
                if let Some(FormValue::Atom(Value::Sym(sym))) = items.first().map(|f| &f.value) {
                    if sym.ns.is_none() && sym.name.as_ref() == "catch" {
                        if items.len() < 2 {
                            return bail("try: catch clause has no binding (malformed, runtime error in the tree-walker)", a.span);
                        }
                        // Shared with `eval_try` (`crate::eval::special_forms`)
                        // so the two tiers cannot disagree about which shape
                        // a clause is -- see that fn's doc.
                        let Some((class, bind, consumed)) = parse_catch_head(&items[1..]) else {
                            return bail("try: catch clause has a non-symbol binding (malformed, runtime error in the tree-walker)", a.span);
                        };
                        catches.push((class, bind, &items[1 + consumed..]));
                        continue;
                    }
                    if sym.ns.is_none() && sym.name.as_ref() == "finally" {
                        if finally.is_some() {
                            return bail("try: more than one finally clause (malformed, runtime error in the tree-walker)", a.span);
                        }
                        finally = Some(&items[1..]);
                        continue;
                    }
                }
            }
            body.push(a);
        }
        let body = body
            .into_iter()
            .map(|f| self.compile_form(ctx, f))
            .collect::<CResult<Vec<Ir>>>()?;
        let mut compiled_catches = Vec::with_capacity(catches.len());
        for (class, bind, cbody) in catches {
            let slot = ctx.alloc_slot()?;
            ctx.scopes.push(vec![(bind, Binder::Slot(slot))]);
            let compiled = self.compile_body(ctx, cbody);
            ctx.scopes.pop();
            compiled_catches.push(CatchArm {
                class,
                slot,
                body: compiled?,
            });
        }
        let finally = match finally {
            None => None,
            // `finally`'s own child env introduces no bindings, so it needs
            // no scope of its own.
            Some(f) => Some(self.compile_body(ctx, f)?),
        };
        Ok(Ir::Try {
            body,
            catches: compiled_catches,
            finally,
        })
    }

    /// `def` in a fn body. W-DECL: the 1-argument form leaves the var
    /// genuinely UNBOUND (matching `eval_def`'s fix, special_forms.rs --
    /// this stale comment used to say the opposite, "binds `nil`", which
    /// was the bug `declare` had to route around entirely via its own
    /// bypass native before this task). `value: None` reaches
    /// `exec_def`'s own `None` arm (`compile/exec.rs`), which now does no
    /// root write at all rather than defaulting to `Value::Nil`. No var
    /// meta is published here either way (`declare`'s own `:declared`/
    /// `:dynamic`/etc. meta is a `def`-in-a-fn-body concern this compiled
    /// path has never handled, at top level OR nested -- out of this
    /// task's scope: `declare` almost always runs at top level, which
    /// stays on `eval_def`, the only place meta publishing has ever
    /// lived).
    fn compile_def(&mut self, ctx: &mut FnCtx, args: &[Form], span: Span) -> CResult<Ir> {
        // `(def name "doc" value)`: mirror eval_def's docstring-discard so
        // the tiers agree.
        let docless: [Form; 2];
        let args = if args.len() == 3
            && matches!(&args[1].value, FormValue::Atom(Value::Str(_)))
        {
            docless = [args[0].clone(), args[2].clone()];
            &docless[..]
        } else {
            args
        };
        if args.is_empty() || args.len() > 2 {
            return bail("def: expected a name and an optional value (malformed special form)", span);
        }
        let sym = match &args[0].value {
            FormValue::Atom(Value::Sym(s)) => s.clone(),
            _ => return bail("def: first argument isn't a symbol (malformed, runtime error in the tree-walker)", args[0].span),
        };
        let value = match args.get(1) {
            Some(f) => Some(Box::new(self.compile_form(ctx, f)?)),
            None => None,
        };
        Ok(Ir::Def {
            // Qualified exactly as `eval_def` would qualify it, in the
            // namespace this fn is being compiled in -- which is the
            // namespace it will run in (`crate::ns`).
            cell: self.globals.intern(&self.interp.qualify_def(&sym)),
            value,
        })
    }

    /// Compiles a binding-site pattern, allocating one slot per name it
    /// introduces and binding that name in `ctx`'s innermost scope AS IT
    /// GOES -- `bind_pattern` defines names into its `env` in the same
    /// order, which is observable both through `:or` defaults (they are
    /// evaluated in that accumulating env) and through which of two
    /// same-named bindings ends up winning.
    fn compile_pattern(&mut self, ctx: &mut FnCtx, form: &Form) -> CResult<CompiledPattern> {
        match &form.value {
            FormValue::Atom(Value::Sym(sym)) => {
                let slot = ctx.alloc_slot()?;
                ctx.bind(sym.clone(), Binder::Slot(slot))?;
                Ok(CompiledPattern::Slot(slot))
            }
            FormValue::Vector(items) => self.compile_seq_pattern(ctx, items),
            FormValue::Map(pairs) => self.compile_map_pattern(ctx, pairs),
            // Anything else is a runtime "invalid destructuring pattern"
            // error in the tree-walker; fall back so it stays one.
            _ => bail("invalid destructuring pattern (not a symbol/vector/map, runtime error in the tree-walker)", form.span),
        }
    }

    fn compile_seq_pattern(&mut self, ctx: &mut FnCtx, items: &[Form]) -> CResult<CompiledPattern> {
        let seq_span = items.first().map_or(NO_SPAN, |f| f.span);
        let mut steps = Vec::with_capacity(items.len());
        let mut i = 0;
        while i < items.len() {
            if is_amp(&items[i]) {
                i += 1;
                if i >= items.len() {
                    // runtime error in the tree-walker
                    return bail("destructuring pattern: & with no following binding", seq_span);
                }
                steps.push(SeqStep::Rest(self.compile_pattern(ctx, &items[i])?));
                i += 1;
                continue;
            }
            if is_as_kw(&items[i]) {
                i += 1;
                if i >= items.len() {
                    return bail("destructuring pattern: :as with no following binding", seq_span);
                }
                steps.push(SeqStep::As(self.compile_pattern(ctx, &items[i])?));
                i += 1;
                continue;
            }
            steps.push(SeqStep::Elem(self.compile_pattern(ctx, &items[i])?));
            i += 1;
        }
        Ok(CompiledPattern::Seq(steps))
    }

    /// The compile-time half of `bind_map_pattern`: `:keys`/`:strs` expand
    /// into ordinary entries in place, `:or` defaults are attached to the
    /// entry that will need them, `:as` is deferred to the end. The runtime
    /// half (`coerce_map_pattern_source` + `map_pattern_lookup`) is the
    /// tree-walker's own code, called from `exec.rs`.
    fn compile_map_pattern(
        &mut self,
        ctx: &mut FnCtx,
        pairs: &[(Form, Form)],
    ) -> CResult<CompiledPattern> {
        let mut or_defaults: Vec<(Str, &Form)> = Vec::new();
        let mut as_form: Option<&Form> = None;
        // (binding form, key value, required) in the tree-walker's order.
        let mut entries: Vec<(&Form, Value, bool)> = Vec::new();

        for (k, v) in pairs {
            match form_keyword_name(k) {
                Some("or") => {
                    let or_pairs = match &v.value {
                        FormValue::Map(p) => p,
                        _ => return bail("destructuring pattern: :or value isn't a map (malformed, runtime error in the tree-walker)", v.span),
                    };
                    for (osym, oval) in or_pairs {
                        match &osym.value {
                            FormValue::Atom(Value::Sym(s)) => {
                                or_defaults.push((s.name.clone(), oval))
                            }
                            _ => return bail("destructuring pattern: :or key isn't a symbol (malformed, runtime error in the tree-walker)", osym.span),
                        }
                    }
                }
                Some("as") => as_form = Some(v),
                // Wave-C small sweep item 5: `:syms` joins `:keys`/`:strs`
                // here too -- WITHOUT this arm, `:syms` fell into the
                // catch-all `_` arm below (it's neither `:or`/`:as`/
                // `:keys`/`:strs`), which treats an unrecognized map-
                // pattern key as an ordinary literal entry: the KEYWORD
                // `:syms` would be bound to `(get m [a b c])` (the vector
                // FORM misread as a literal lookup key), silently
                // producing a bogus binding instead of erroring -- a
                // latent correctness bug in the compiled tier, distinct
                // from (and worse than) the interpreter's own loud
                // "invalid destructuring pattern: :syms" (see
                // `eval::special_forms::bind_map_pattern`'s sibling `:syms`
                // arm, added alongside this one). Same key-shape split as
                // that arm: `:syms` looks up a bare (unqualified) SYMBOL
                // key, not a keyword or string.
                // `!`-suffixed (`:keys!`/`:strs!`/`:syms!`, S5/M2's
                // "required keys" directives, matched here as exact flat
                // strings -- this is deliberately narrower than the tree-
                // walker's `mkn.starts_with("keys")`-style prefix test,
                // because a NAMESPACED directive (`:foo/keys`, `:foo/keys!`)
                // must keep falling through to the `_` catch-all below and
                // `Bail` (see that arm's own comment and `item_ns_name`'s
                // sibling handling in the tree-walker, which this compiled
                // arm does not replicate): `form_keyword_name` already
                // returns the FULL flat `"foo/keys!"` string for those, which
                // never matches one of these six bare alternatives).
                Some(kind @ ("keys" | "strs" | "syms" | "keys!" | "strs!" | "syms!")) => {
                    let required = kind.ends_with('!');
                    let kind = kind.trim_end_matches('!');
                    let syms = match &v.value {
                        FormValue::Vector(items) => items,
                        _ => return bail("destructuring pattern: :keys/:strs/:syms value isn't a vector (malformed, runtime error in the tree-walker)", v.span),
                    };
                    for it in syms {
                        // Bug fix (W-DESTR): the tree-walker's
                        // `bind_directive_entries` treats `&` inside a
                        // `:keys`/`:strs`/`:syms` (bang or not) vector as a
                        // sequential-destructuring-style rest marker and
                        // THROWS on anything bound after it ("'b' - binding
                        // symbols can only appear before '&', use keys
                        // after") -- `&` has no meaning at all in a map
                        // directive otherwise. This arm used to compile `&`
                        // as an ordinary symbol item (binding a dead local
                        // under the bogus key `:&`) and every item after it
                        // as a normal binding, silently accepting what the
                        // tree-walker rejects. Bail the instant `&` appears
                        // -- the whole fn falls back to the tree-walker,
                        // which throws the exact right message.
                        if is_amp(it) {
                            return bail("destructuring pattern: & inside a :keys/:strs/:syms vector (malformed, runtime error in the tree-walker)", it.span);
                        }
                        let s = match &it.value {
                            FormValue::Atom(Value::Sym(s)) => s,
                            _ => return bail("destructuring pattern: :keys/:strs/:syms item isn't a symbol (malformed, runtime error in the tree-walker)", it.span),
                        };
                        // A namespaced item (`:keys [a/b]`) binds the
                        // BARE local name `b`, but looks the value up
                        // under the QUALIFIED key -- the tree-walker's
                        // `bind_directive_entries`/`item_ns_name` does
                        // the same split (the directive here is always
                        // BARE `:keys`/`:strs`/`:syms`, no `mkns`, since
                        // a namespaced directive's `form_keyword_name`
                        // doesn't match this arm at all and bails to the
                        // tree-walker instead -- see that fn's own CLJ-
                        // 2968 handling). Compiling `it` ITSELF as the
                        // binder (the pre-fix code) put the SLOT under
                        // the full `ns/name` symbol, which
                        // `FnCtx::lookup` then refuses to ever match
                        // (namespace-qualified symbols never resolve to
                        // a lexical slot -- see that fn's doc) --
                        // "Unable to resolve symbol: b" was the
                        // observable failure this produced.
                        let key = match kind {
                            "keys" => Value::Keyword(match &s.ns {
                                Some(ns) => Keyword::from(format!("{ns}/{}", s.name)),
                                None => Keyword::from(&s.name),
                            }),
                            "strs" => Value::Str(match &s.ns {
                                Some(ns) => format!("{ns}/{}", s.name).into(),
                                None => s.name.clone(),
                            }),
                            _ => Value::Sym(s.clone()),
                        };
                        let bind_form: &Form = if s.ns.is_some() {
                            // Deliberate, bounded `Box::leak` (precedent:
                            // `builtins::types`'s array-name interning) --
                            // one small synthetic `Form` per namespaced
                            // `:keys`/`:strs`/`:syms` item actually
                            // compiled, for the lifetime of the process;
                            // there is no arena on `Compiler` to allocate
                            // this from instead, and every OTHER call site
                            // needing a `&Form` here borrows straight out
                            // of the real (already-alive) source AST.
                            Box::leak(Box::new(Form::bare(
                                FormValue::Atom(Value::Sym(crate::value::Symbol::simple(s.name.as_ref()))),
                                it.span,
                            )))
                        } else {
                            it
                        };
                        entries.push((bind_form, key, required));
                    }
                }
                // K5: namespaced `:ns/keys [a]` -> key `:ns/a`; only simple symbols (else the tree-walker's CLJ-2968 error).
                Some(n) if n.len() > 5 && n.ends_with("/keys") => {
                    let ns = &n[..n.len() - 5];
                    let FormValue::Vector(items) = &v.value else {
                        return bail("destructuring pattern: :ns/keys value isn't a vector (malformed, runtime error in the tree-walker)", v.span);
                    };
                    for it in items {
                        match &it.value {
                            FormValue::Atom(Value::Sym(s)) if s.ns.is_none() && !is_amp(it) => {
                                entries.push((it, Value::Keyword(Keyword::from(format!("{ns}/{}", s.name))), false))
                            }
                            _ => return bail("destructuring pattern: :ns/keys item isn't a simple symbol (spec error in the tree-walker)", it.span),
                        }
                    }
                }
                _ => entries.push((k, crate::reader::form_to_value(v), false)),
            }
        }

        let mut out = Vec::with_capacity(entries.len());
        for (bind_form, key, required) in entries {
            // The default is compiled BEFORE the pattern binds its own name,
            // and after every earlier entry bound theirs -- exactly the env
            // `bind_map_pattern` evaluates it in.
            let default_form = symbol_name_of_pattern(bind_form).and_then(|name| {
                or_defaults
                    .iter()
                    .find(|(n, _)| n.as_ref() == name.as_ref())
                    .map(|(_, f)| *f)
            });
            if required && default_form.is_some() {
                // The tree-walker's `resolve_push_value` throws "Can't
                // supply default value for required key" for this exact
                // combination UNCONDITIONALLY -- before ever looking at the
                // map's contents, purely from the pattern's own shape (a
                // required entry naming itself in the enclosing `:or`).
                // Bail so that message comes from there, byte-for-byte,
                // rather than being duplicated here.
                return bail(
                    "destructuring pattern: a required (:keys! etc.) key also names a default in :or (runtime error in the tree-walker)",
                    bind_form.span,
                );
            }
            let default = match default_form {
                Some(f) => Some(self.compile_form(ctx, f)?),
                None => None,
            };
            let span = bind_form.span;
            let target = self.compile_pattern(ctx, bind_form)?;
            out.push(MapEntry {
                key,
                target,
                default,
                required,
                span,
            });
        }
        let as_pat = match as_form {
            Some(f) => Some(self.compile_pattern(ctx, f)?),
            None => None,
        };
        Ok(CompiledPattern::Map(Box::new(MapPattern {
            entries: out,
            as_pat,
        })))
    }

    /// Resolution order (COMPILE-TIER-DESIGN.md + this module's doc):
    /// innermost `let`/`loop`/`catch` slots -> fn self-name -> params ->
    /// enclosing compiled fns (as captures) -> creation-env lookup ->
    /// global cell.
    fn resolve_symbol(&mut self, ctx: &mut FnCtx, sym: &Symbol, span: Span) -> CResult<Ir> {
        if let Some(ir) = self.resolve_lexical(ctx, sym, false, span)? {
            return Ok(ir);
        }
        // W-VARS-PRIV: single chokepoint for BOTH value position
        // (`compile_form_inner`) and call position (`compile_list`'s
        // callee compiles through this same fn) -- see
        // `ns::Interp::check_qualified_private`'s doc for the full
        // measured contract. A qualified reference to another ns's
        // private var bails the WHOLE fn out of the compiled tier (per
        // this module's contract, `Bail` docs above) so the tree-walker
        // re-runs it and raises the real "var: ns/name is not public"
        // error itself -- this branch never constructs that error text,
        // it only refuses to compile past it. Compile-time-only cost:
        // `check_qualified_private` short-circuits on `sym.ns.is_none()`,
        // so an unqualified symbol (the overwhelming majority) pays one
        // extra field check and nothing more.
        if self.interp.check_qualified_private(sym).is_some() {
            return bail(
                "qualified reference to a private var from another ns -- privacy error is raised by the tree-walker",
                span,
            );
        }
        let chain = global_chain(self.interp, &self.globals, sym);
        if !self.env_is_root {
            // Under tree-walked frames: re-run the lookup against those very
            // frames on every access (see `Ir::CreationEnvLookup`). NOTHING
            // about them may be snapshotted here -- a frame can gain a
            // binding (letfn) or rebind one (`(let [a 1 g (fn [] a) a 2] ..)`)
            // after this closure exists.
            return Ok(Ir::CreationEnvLookup {
                chain,
                sym: sym.clone(),
                span,
            });
        }
        Ok(Ir::GlobalRef {
            chain,
            sym: sym.clone(),
            span,
        })
    }

    /// `Some(load node)` if `sym` is bound by this fn or an enclosing
    /// compiled one, `None` if it is free (the caller then decides between a
    /// global cell and a creation-env lookup). `crossed` is true once the
    /// search has passed through a nested-`fn` boundary, which is what makes
    /// the read a DEFERRED one for the poison rule (see this module's doc).
    fn resolve_lexical(
        &mut self,
        ctx: &mut FnCtx,
        sym: &Symbol,
        crossed: bool,
        span: Span,
    ) -> CResult<Option<Ir>> {
        if let Some(b) = ctx.lookup(sym) {
            if crossed {
                // A nested fn is about to snapshot this binding by value.
                // Record the name so `compile_binds` can refuse to compile a
                // later rebinding of it in a still-open binding vector.
                // `lookup` never matches a qualified symbol (see its doc
                // comment), so `sym` here is always the bare spelling that
                // was actually captured -- no second spelling to record.
                ctx.captured_names.push(sym.clone());
            }
            return Ok(Some(match b {
                Binder::Slot(i) => Ir::LoadSlot(i),
                Binder::SelfName => Ir::SelfRef,
                // A sibling of the recursive binding group THIS fn belongs
                // to. `crossed == false` means the read is in the member's
                // own body, where `l.me` is the member and the node runs as
                // written; `crossed == true` means a fn nested inside the
                // member read it, and the caller below turns this into a
                // `CaptureSrc::Sibling` -- the same read, taken in the
                // member's frame at that nested closure's creation time.
                // Either way it is NEVER a by-value sibling capture out of
                // a slot, which is what keeps the group acyclic (see
                // `ir::Ir::SiblingRef`).
                Binder::Sibling(i) => Ir::SiblingRef(i),
            }));
        }
        let poisoned = ctx.poison.iter().any(|(p, _)| p == sym);
        if crossed && ctx.poison.iter().any(|(p, safe)| p == sym && !safe) {
            // A DEFERRED read of a not-yet-bound name: `letfn`'s live-frame
            // trick, which a compiled scope has no frame to defer to. Always
            // refused, and refused HERE -- before the parent chain gets a
            // say -- because resolving it outward would silently read a
            // DIFFERENT binding than the one the closure will actually see
            // when it runs. (field3/W-PARSE left this arm untouched.)
            return bail(POISON_BAIL, span);
        }
        // field3/W-PARSE: an IMMEDIATE read of a poisoned name is safe iff
        // the name has a value RIGHT NOW, because that is what the
        // tree-walker's own `resolve_symbol` finds at this same moment. Two
        // places can hold that value, and the order they are probed in used
        // to be an accident: this arm checked ONLY globals/creation-env
        // (`interp.resolve_symbol`) and bailed if they missed -- never
        // reaching the parent chain below, which is where a SHADOWED
        // enclosing-fn binding lives.
        //
        // `(fn [s] (lazy-seq (loop [s s] ..)))` is that shape, and it is what
        // `core.mova`'s `for` expands to (one gensym for the iterator fn's
        // param AND its loop binding), so every `for` in the language was
        // tree-walking. The tree-walker evaluating that init walks the live
        // env chain, finds no `s` in the loop's child env yet (`eval_loop`
        // `set`s each name only after its init runs) and continues outward to
        // the enclosing fn's frame -- exactly the slot the capture below
        // reads. Same answer, so the bail is now DEFERRED past the parent
        // chain: capture if the chain has it, and only otherwise fall back on
        // the original global probe. See
        // docs/W-PARSE-poison-shadow-decision.md.
        let src = {
            let parent = match ctx.parent.as_deref_mut() {
                Some(p) => p,
                None => return self.poisoned_free(poisoned, sym, span),
            };
            match self.resolve_lexical(parent, sym, true, span)? {
                None => return self.poisoned_free(poisoned, sym, span),
                Some(Ir::LoadSlot(i)) => CaptureSrc::Slot(i),
                Some(Ir::LoadCapture(i)) => CaptureSrc::Capture(i),
                Some(Ir::SelfRef) => CaptureSrc::SelfRef,
                // The parent is a recursive-binding-group member and this is
                // a fn nested inside it reading a sibling name: capture the
                // sibling out of the PARENT's frame at creation time.
                // Acyclic, and exact -- see `ir::CaptureSrc::Sibling`.
                Some(Ir::SiblingRef(i)) => CaptureSrc::Sibling(i),
                // `resolve_lexical` only ever returns those four.
                Some(_) => return bail("internal: resolve_lexical returned an unexpected Ir node", span),
            }
        };
        let idx = ctx.intern_capture(src, sym, span)?;
        Ok(Some(Ir::LoadCapture(idx)))
    }

    /// field3/W-PARSE: the tail of `resolve_lexical`'s immediate-read arm --
    /// reached once the enclosing compiled fns have been asked and come back
    /// empty, so the name is genuinely FREE of every compiled scope.
    ///
    /// A name that was never poisoned is simply free: `None` lets the caller
    /// pick between a creation-env lookup and a global cell, as always.
    ///
    /// A POISONED one gets the original probe, unchanged: it is safe to
    /// resolve outward iff `interp.resolve_symbol` says it has a value right
    /// now (a global, or a live tree-walked creation frame) -- which is what
    /// the tree-walker would find at this same moment. If it has none, the
    /// only thing left that could ever answer is the binding being made,
    /// which does not exist yet in either tier, so fall back rather than
    /// freeze an "unresolved" the tree-walker would raise at call time.
    fn poisoned_free(&mut self, poisoned: bool, sym: &Symbol, span: Span) -> CResult<Option<Ir>> {
        if poisoned && self.interp.resolve_symbol(&self.env, sym).is_none() {
            return bail(POISON_BAIL, span);
        }
        Ok(None)
    }
}

/// Poisons every name bound at binding `i` or later; a deferred read is safe
/// iff the tree-walker binds the name in a newer frame than binding i's init.
fn push_poison(ctx: &mut FnCtx, names: &[Vec<Symbol>], fi: &[u32], fb: &[u32], i: usize) {
    for (j, group) in names.iter().enumerate().skip(i) {
        ctx.poison.extend(group.iter().map(|nm| (nm.clone(), fb[j] > fi[i])));
    }
}

/// True if `sym` is bound in `ctx` or any enclosing compiled fn.
fn ctx_chain_binds(ctx: &FnCtx, sym: &Symbol) -> bool {
    let mut c = Some(ctx);
    while let Some(x) = c {
        if x.lookup(sym).is_some() {
            return true;
        }
        c = x.parent.as_deref();
    }
    false
}

/// The `letfn` SHAPE detector: does a recursive binding group start at
/// binding index `start` of this binding vector, and if so what are its
/// member names, in order?
///
/// A run qualifies when ALL of the following hold. Every condition is a
/// refusal in the safe direction -- declining just leaves the run on the
/// path it took before groups existed, which for a genuinely mutual shape
/// is the whole-fn bail (correct, only slow).
///
/// 1. **Consecutive, and maximal.** Two or more adjacent bindings, each a
///    bare symbol (no destructuring, no metadata) bound to a literal `(fn
///    ...)`/`(fn* ...)` list form (again no metadata -- `compile_form_inner`
///    bails on metadata over a non-symbol form anyway). The run is extended
///    as far as it goes; a non-matching binding in the middle ends it, which
///    is what leaves `(let [e (fn [] (o)) x 5 o (fn [] x)] ..)` on the old
///    path. `fn`/`fn*` is matched by NAME, which is exact: `eval_list`
///    dispatches special forms ahead of locals and macros, so a `fn` head
///    cannot mean anything else.
/// 2. **Mutual.** Some member's init MENTIONS another member's name, as a
///    bare symbol anywhere in the form. Over-approximating is safe: a run
///    grouped without any actual sibling reference simply emits no
///    `SiblingRef` at all, and is then N `MakeClosure`s sharing one node.
///    Under-approximating is not an option -- a missed mutual reference is a
///    bail, not a miscompile, but it is the bail this feature exists to
///    remove.
/// 3. **No name drift.** The run's names are distinct from each other AND
///    from every name bound anywhere else in the same vector. A rebinding of
///    a member name -- `(let [e (fn [] (o)) o (fn [] 1) o (fn [] 2)] (e))`
///    -- would make `SiblingRef` keep answering the ORIGINAL member while
///    the tree-walker's single mutable `let` frame answers the later one.
///    That shape stays on the old path, where the deferred-read poison rule
///    bails the whole fn and the tree-walker gives its own answer.
fn rec_group_run(items: &[Form], names: &[Vec<Symbol>], start: usize) -> Option<Vec<Symbol>> {
    let n = names.len();
    let mut run: Vec<Symbol> = Vec::new();
    let mut i = start;
    while i < n {
        match binding_is_plain_fn(items, i) {
            Some(sym) => run.push(sym),
            None => break,
        }
        i += 1;
    }
    if run.len() < 2 {
        return None;
    }
    // (3) distinct within the run, and bound nowhere else in this vector.
    for (k, nm) in run.iter().enumerate() {
        if run[..k].contains(nm) {
            return None;
        }
        for (j, group) in names.iter().enumerate() {
            if (j < start || j >= start + run.len()) && group.contains(nm) {
                return None;
            }
        }
    }
    // (2) at least one genuine sibling mention.
    let mut mentioned: Vec<Symbol> = Vec::new();
    for (k, _) in run.iter().enumerate() {
        mentioned.clear();
        collect_bare_symbols(&items[(start + k) * 2 + 1], &mut mentioned);
        if run
            .iter()
            .enumerate()
            .any(|(j, other)| j != k && mentioned.contains(other))
        {
            return Some(run);
        }
    }
    None
}

/// Binding `i` of a binding vector, if it is exactly `<bare-symbol> (fn
/// ...)` -- the one shape a recursive binding group is built from. Returns
/// the bound name.
fn binding_is_plain_fn(items: &[Form], i: usize) -> Option<Symbol> {
    let pat = items.get(i * 2)?;
    let init = items.get(i * 2 + 1)?;
    if pat.meta.is_some() || init.meta.is_some() {
        return None;
    }
    let sym = match &pat.value {
        FormValue::Atom(Value::Sym(s)) if s.ns.is_none() => s.clone(),
        _ => return None,
    };
    let head = match &init.value {
        FormValue::List(l) => l.first()?,
        _ => return None,
    };
    match &head.value {
        FormValue::Atom(Value::Sym(h))
            if h.ns.is_none() && (h.name.as_ref() == "fn" || h.name.as_ref() == "fn*") =>
        {
            Some(sym)
        }
        _ => None,
    }
}

/// field3/W-RESOLVE: every distinct BARE symbol appearing anywhere in
/// `form`, in first-mention order -- the candidate set `compile_escape`
/// puts through `resolve_lexical` to build an escape's locals bridge.
///
/// Deliberately syntax-blind: it does not care whether a symbol is a
/// reference, a binding site, quoted, or the head of a call. That is an
/// over-approximation of what the escaped form actually reads, and
/// over-approximating is the SAFE direction here -- every extra name it
/// yields is one the tree-walker would also have had in scope at this
/// point, so binding it changes nothing, while a name it MISSED would be
/// one the escape could not see. (Missing one is not possible: the
/// tree-walker resolves free names by the same spelling that appears in
/// the source, and this collects every spelling in the source.)
///
/// Qualified symbols are skipped for the reason `FnCtx::lookup` gives: a
/// binding form is always a bare symbol, so `mode/state` can never name a
/// local and must reach a global.
///
/// `^` metadata is walked too (a `^Foo x` hint's tag symbol is harmless
/// noise here, but a metadata map can hold arbitrary forms).
///
/// Linear dedup rather than a set: escape candidate lists are a handful of
/// names, and this runs once per interop form at COMPILE time.
fn collect_bare_symbols(form: &Form, out: &mut Vec<Symbol>) {
    if let Some(m) = &form.meta {
        collect_bare_symbols(m, out);
    }
    match &form.value {
        FormValue::Atom(Value::Sym(s)) => {
            if s.ns.is_none() && !out.contains(s) {
                out.push(s.clone());
            }
        }
        FormValue::Atom(_) => {}
        FormValue::List(items) | FormValue::Vector(items) | FormValue::Set(items) => {
            for it in items {
                collect_bare_symbols(it, out);
            }
        }
        FormValue::Map(pairs) => {
            for (k, v) in pairs {
                collect_bare_symbols(k, out);
                collect_bare_symbols(v, out);
            }
        }
    }
}

/// field3/W-RESOLVE: does `form` contain a literal bare `recur` anywhere?
///
/// The one thing `Ir::Escape` refuses to bridge -- see `compile_escape`
/// for why (two incompatible unwind disciplines on opposite sides of the
/// boundary). Scans RAW source, so a `dotimes`/`doseq` inside an escaped
/// form does not trip it: those are still spelled as their macro names
/// here and only become `recur` when the tree-walker expands them, at
/// which point the whole expansion is on the tree-walk side of the
/// boundary and there is no boundary to cross.
fn mentions_recur(form: &Form) -> bool {
    if form.meta.as_deref().is_some_and(mentions_recur) {
        return true;
    }
    match &form.value {
        FormValue::Atom(Value::Sym(s)) => s.ns.is_none() && s.name.as_ref() == "recur",
        FormValue::Atom(_) => false,
        FormValue::List(items) | FormValue::Vector(items) | FormValue::Set(items) => {
            items.iter().any(mentions_recur)
        }
        FormValue::Map(pairs) => pairs.iter().any(|(k, v)| mentions_recur(k) || mentions_recur(v)),
    }
}

// ---------------------------------------------------------------------------
// W-FIELDGET: recognizing a compilable `.-field` read
//
// A post-pass on an ALREADY BUILT `Ir::Escape`, deliberately -- the same
// shape `specialize_num_loop` has, and for the same reason: it cannot change
// what the form means, it only decides whether a faster encoding exists, and
// the node it inspects is kept verbatim as the fallback so every refusal at
// run time lands back on today's code path. See `ir::FieldGet`.
// ---------------------------------------------------------------------------

/// True when `MOVA_NO_FIELDGET=1` was set at process start: the operator
/// switch that stops `Ir::FieldGet` from ever being EMITTED, leaving the
/// compiled tier otherwise untouched. Read exactly once, and compile-time
/// rather than a run-time branch, for exactly the reasons
/// [`num_loop_disabled_by_env`] gives -- with the node never built, an
/// `MOVA_NO_FIELDGET=1` run is a genuine A/B of the feature rather than of
/// one of its two code paths.
pub(super) fn field_get_disabled_by_env() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| std::env::var("MOVA_NO_FIELDGET").is_ok_and(|v| v == "1"))
}

/// `Ir::FieldGet` wrapping `esc` when the shape allows, `esc` unchanged
/// otherwise. Never fails: declining to specialize is always correct.
///
/// Three conditions, all syntactic:
///
/// 1. the head is `.-name` (field-only interop) with a non-empty name --
///    `.name` is NOT accepted, because a one-argument `(.name x)` is
///    field-access-then-method-dispatch and reaching the field half means
///    reproducing `eval_dot_form`'s universal-`Object`-method interception
///    (`.equals`/`.hashCode`/`.getName`/`.size`/`.iterator`/`.isArray`,
///    which fire BEFORE the field read for a type that declares them);
/// 2. exactly one argument, which is what `eval_dot_form` requires of a
///    `.-` form anyway (it errors on any other arity, and that error must
///    keep coming from the tree-walker);
/// 3. that argument is a bare symbol the escape's own bridge resolved to a
///    slot or a capture. Restricting the receiver to a pure frame read is
///    what makes re-running the fallback safe -- see `ir::FieldGet`.
fn specialize_field_get(head: &str, items: &[Form], esc: Ir) -> Ir {
    if field_get_disabled_by_env() {
        return esc;
    }
    let Some(field) = head.strip_prefix(".-").filter(|f| !f.is_empty()) else {
        return esc;
    };
    if items.len() != 2 {
        return esc;
    }
    let FormValue::Atom(Value::Sym(recv_sym)) = &items[1].value else {
        return esc;
    };
    // A metadata-carrying receiver (`^RoseTree rose`) is still just the
    // symbol here: `compile_form_inner` keeps hints on symbols, and the
    // bridge below resolves the same name either way.
    if recv_sym.ns.is_some() {
        return esc;
    }
    let Ir::Escape(e) = &esc else { return esc };
    // The receiver must be one of the names the escape bridged, and it must
    // have landed on a frame read. `CaptureSrc::SelfRef` is deliberately not
    // accepted: the running closure is never a `Value::Inst`, so the fast
    // path could not fire for it anyway.
    let recv = e.binds.iter().find(|(sym, _)| sym == recv_sym).map(|(_, src)| src);
    let recv = match recv {
        Some(CaptureSrc::Slot(i)) => FieldRecv::Slot(*i),
        Some(CaptureSrc::Capture(i)) => FieldRecv::Capture(*i),
        _ => return esc,
    };
    let field: Str = field.into();
    let lens_site = e.lens_site;
    Ir::FieldGet(Box::new(FieldGet {
        kw: Value::Keyword(Keyword::from(&field)),
        field,
        recv,
        ic: FieldIc::new(),
        fallback: esc,
        lens_site,
    }))
}

/// Every name a binding-site pattern introduces, for the poison rule. Bails
/// on any shape the tree-walker would reject at run time (the same shapes
/// `compile_pattern` bails on), so a malformed pattern falls back before it
/// can half-poison a binding list.
fn pattern_names(form: &Form) -> CResult<Vec<Symbol>> {
    let mut out = Vec::new();
    collect_pattern_names(form, &mut out)?;
    Ok(out)
}

fn collect_pattern_names(form: &Form, out: &mut Vec<Symbol>) -> CResult<()> {
    match &form.value {
        FormValue::Atom(Value::Sym(s)) => {
            out.push(s.clone());
            Ok(())
        }
        FormValue::Vector(items) => {
            for it in items {
                if is_amp(it) || is_as_kw(it) {
                    continue;
                }
                collect_pattern_names(it, out)?;
            }
            Ok(())
        }
        FormValue::Map(pairs) => {
            for (k, v) in pairs {
                match form_keyword_name(k) {
                    // `:or`'s keys name bindings introduced elsewhere in the
                    // same pattern (by `:keys`/`:strs`), never new ones.
                    Some("or") => {}
                    Some("as") => collect_pattern_names(v, out)?,
                    // Wave-C small sweep item 5: `:syms` joins `:keys`/
                    // `:strs` here too, for the same reason as this file's
                    // other `:syms` arm above -- without it, a `:syms`
                    // pattern's names never get collected, `Bail`s out of
                    // the compiled tier every time (safe, just a missed
                    // optimization) instead of participating normally.
                    Some(n) if n == "keys" || n == "strs" || n == "syms" || (n.len() > 5 && n.ends_with("/keys")) => match &v.value {
                        FormValue::Vector(items) => {
                            for it in items {
                                collect_pattern_names(it, out)?;
                            }
                        }
                        _ => return bail("destructuring pattern: :keys/:strs/:syms value isn't a vector (malformed, runtime error in the tree-walker)", v.span),
                    },
                    _ => collect_pattern_names(k, out)?,
                }
            }
            Ok(())
        }
        _ => bail("invalid destructuring pattern (not a symbol/vector/map, runtime error in the tree-walker)", form.span),
    }
}

/// Compile-time intrinsic selection (v0.3 / S3): `Some(op)` when `name` is
/// a builtin the executor can run directly at this argument count AND its
/// global cell is *still* the boot-registered native. The pristine test is
/// repeated at run time (that is the actual guard -- this one only decides
/// whether it is worth emitting the node at all), so a `def` between
/// compiling and calling is handled, and so is one that happens between two
/// calls.
///
/// Deliberately NOT here: any 0-argument or wrong-arity shape (`(+)`,
/// `(- x)`, `(< a b c)`) -- those keep the native's own folding/chaining
/// behavior via `CallGlobal`. `+` and `*` are the exceptions that accept
/// 2-or-more, because their intrinsic folds n arguments in ONE node exactly
/// as the native does (see `IntrinOp`).
fn intrin_op(name: &str, argc: usize, chain: &GlobalChain) -> Option<IntrinOp> {
    // The pristine test applies to the cell this chain currently RESOLVES
    // to, and only while nothing ahead of it in the chain has been defined
    // (`resolved_still_wins`) -- a namespace that has already defined its
    // own `+` never gets an intrinsic for it.
    if !chain.resolved_still_wins() || !chain.resolved().pristine_builtin.load(Ordering::Acquire) {
        return None;
    }
    Some(match (name, argc) {
        ("+", n) if n >= 2 => IntrinOp::Add,
        ("*", n) if n >= 2 => IntrinOp::Mul,
        ("-", 2) => IntrinOp::Sub2,
        ("/", 2) => IntrinOp::Div2,
        ("inc", 1) => IntrinOp::Inc,
        ("dec", 1) => IntrinOp::Dec,
        ("<", 2) => IntrinOp::Lt2,
        ("<=", 2) => IntrinOp::Le2,
        (">", 2) => IntrinOp::Gt2,
        (">=", 2) => IntrinOp::Ge2,
        ("=", 2) => IntrinOp::Eq2,
        ("zero?", 1) => IntrinOp::Zero,
        ("not", 1) => IntrinOp::Not,
        _ => return None,
    })
}

/// Interns `sym`'s global candidate chain (`crate::ns`'s resolution order,
/// as cells) for the node that will read it on every access.
///
/// The walk stops at the first candidate that is ALREADY BOUND, because a
/// var is never unbound again: every later candidate is unreachable
/// forever, so keeping them would only cost reads. Everything ahead of that
/// point is kept, unbound and all -- that is what makes a forward reference
/// (`a.b/helper` defined later in the file) and a namespace that shadows a
/// core name *after* this fn was compiled resolve exactly as the
/// tree-walker's per-access probe would.
///
/// field3: interned SPECULATIVELY (`Env::intern_speculative`), not
/// genuinely. The placeholders this leaves behind for candidates no `def`
/// ever reaches -- `my.ns/*ns*` in front of the real bare `*ns*`, one per
/// core name any compiled fn in `my.ns` happens to mention -- are a
/// resolution artifact, not namespace mappings, and W-DECL's new
/// interned-or-bound `binding` lookup (`Env::find_any_cell`) would
/// otherwise pick one of them over the genuine var. See
/// `VarCell::speculative` for the measured failure. A candidate that is
/// already a real var is returned unchanged, so nothing here can demote
/// one.
fn global_chain(interp: &Interp, globals: &Env, sym: &Symbol) -> GlobalChain {
    let mut cells: Vec<Arc<crate::env::VarCell>> = Vec::new();
    interp.for_each_global_candidate(sym, |cand| {
        let cell = globals.intern_speculative(cand);
        let bound = cell.is_bound();
        cells.push(cell);
        // `Some` stops the walk; the value itself is unused.
        if bound {
            Some(())
        } else {
            None
        }
    });
    GlobalChain::new(cells)
}

// ---------------------------------------------------------------------------
// Recognizing a scalar numeric loop (`ir::NumLoop`)
//
// A post-pass, deliberately: it reads an ALREADY RESOLVED `Ir::Loop`, so it
// works in slot indices and `IntrinOp`s rather than in source forms, and it
// cannot change what the loop means -- it only decides whether an equivalent
// register-machine encoding exists. When it does, the original node is kept
// verbatim as the `NumLoop`'s fallback, so every guard failure at run time
// lands back on the generic code path.
//
// `ir::NumLoop` states the grammar this accepts, the four deopt conditions
// and the invariants that make an entry-only guard sufficient; this file is
// only the recognizer. The recognizer is TOTAL: every path out of the
// grammar is a `None`, and `None` means "leave the generic loop alone",
// which is always correct.
// ---------------------------------------------------------------------------

/// True when `MOVA_NO_NUMLOOP=1` was set at process start: the operator
/// switch that stops `Ir::NumLoop` from ever being EMITTED, leaving the
/// compiled tier otherwise untouched. Read exactly once, like
/// `super::disabled_by_env`, so no compile-time path touches the
/// environment more than that.
///
/// Deliberately compile-time rather than a run-time branch in `exec.rs`:
/// with the node never built there is nothing left to be wrong about, so
/// `MOVA_NO_NUMLOOP=1 cargo test --release` is a genuine A/B of the feature
/// rather than of one of its two code paths. `MOVA_NO_COMPILE=1` subsumes
/// it (no fn is compiled at all, so no loop is ever inspected).
///
/// `Interp::numloop_enabled` is its per-interpreter sibling, checked at the
/// one call site in `compile_loop`, for the same reason `compile_enabled`
/// sits beside `MOVA_NO_COMPILE`.
pub(super) fn num_loop_disabled_by_env() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| std::env::var("MOVA_NO_NUMLOOP").is_ok_and(|v| v == "1"))
}

/// Register allocation + constant pool + guard collection while lowering one
/// loop.
struct NumCtx {
    /// Frame slot of binding *i*, i.e. register *i*. Entries are distinct:
    /// `FnCtx::alloc_slot` only ever hands out fresh indices, so even
    /// `(loop [a 0 a 1] ..)` -- legal, and `1` -- gives the two `a`s their
    /// own slots and the body's read resolves to the second.
    slots: Vec<u16>,
    next_reg: usize,
    /// `(register, value)`, in allocation order; see `NumLoop::consts`.
    consts: Vec<(u8, Num)>,
    /// See `NumLoop::loads`.
    loads: Vec<(u8, NumLoad)>,
    /// One entry per DISTINCT intrinsic cell absorbed (see `push_guard`).
    guards: Vec<GlobalChain>,
}

impl NumCtx {
    fn reg_of_slot(&self, slot: u16) -> Option<u8> {
        self.slots.iter().position(|s| *s == slot).map(|i| i as u8)
    }

    /// Records the guard for one absorbed intrinsic, deduplicated by CELL
    /// IDENTITY. A loop mentioning `+` five times absorbs one `+`, and
    /// `NumLoop`'s entry check is per guard, so the dedup is a real (if
    /// small) saving for a loop entered many times -- `bench/flow-gen-sink-
    /// w2000.mova` enters its `burn` loop once per message.
    ///
    /// Comparing the RESOLVED cell alone would be wrong: two chains can
    /// share a resolved cell and differ in what sits ahead of it, and
    /// `resolved_still_wins` reads exactly that prefix. So identity here is
    /// "same cells, in the same order".
    fn push_guard(&mut self, chain: &GlobalChain) {
        if !self.guards.iter().any(|g| g.same_candidates(chain)) {
            self.guards.push(chain.clone());
        }
    }

    /// A frame slot read: one of the loop's own bindings if it is one, else
    /// a loop-invariant load.
    fn slot_reg(&mut self, slot: u16) -> Option<u8> {
        match self.reg_of_slot(slot) {
            Some(r) => Some(r),
            None => self.load_reg(NumLoad::Slot(slot)),
        }
    }

    /// The register holding an invariant, allocating one on first use.
    fn load_reg(&mut self, src: NumLoad) -> Option<u8> {
        if let Some((r, _)) = self.loads.iter().find(|(_, s)| *s == src) {
            return Some(*r);
        }
        let r = self.temp()?;
        self.loads.push((r, src));
        Some(r)
    }

    /// A scratch register, or `None` when the loop needs more than
    /// `NUM_REGS` (the executor's fixed register file).
    fn temp(&mut self) -> Option<u8> {
        if self.next_reg >= NUM_REGS {
            return None;
        }
        let r = self.next_reg as u8;
        self.next_reg += 1;
        Some(r)
    }

    /// The register holding `n`, allocating one on first use. Dedup is by
    /// BIT PATTERN, never by numeric equality: `0.0` and `-0.0` compare
    /// equal but `add` distinguishes them, and `Int(1)`/`Float(1.0)` are
    /// different types to every op here.
    fn const_reg(&mut self, n: Num) -> Option<u8> {
        if let Some((r, _)) = self.consts.iter().find(|(_, c)| same_num_bits(*c, n)) {
            return Some(*r);
        }
        let r = self.temp()?;
        self.consts.push((r, n));
        Some(r)
    }
}

fn same_num_bits(a: Num, b: Num) -> bool {
    match (a, b) {
        (Num::I(x), Num::I(y)) => x == y,
        (Num::F(x), Num::F(y)) => x.to_bits() == y.to_bits(),
        _ => false,
    }
}

/// `Ir::NumLoop` wrapping `ir` when the shape allows, `ir` unchanged
/// otherwise -- paired with the [`LoopDecision`] explaining which happened
/// (field1/W-EXPLAIN). Never fails: not specializing is always correct, and
/// declining is itself a reportable decision, not an error.
fn specialize_num_loop(ir: Ir, lanes_enabled: bool, superloop_enabled: bool) -> (Ir, LoopDecision) {
    if num_loop_disabled_by_env() {
        return (
            ir,
            LoopDecision::Generic {
                reason: "MOVA_NO_NUMLOOP=1 -- NumLoop specialization is disabled process-wide",
            },
        );
    }
    let built = match &ir {
        Ir::Loop {
            binds,
            scratch_base,
            body,
        } => build_num_loop(binds, *scratch_base, body),
        // `specialize_num_loop`'s one call site (`compile_loop`) only ever
        // hands it the `Ir::Loop` it just built; kept as a reachable `Err`
        // rather than `unreachable!` so a future caller misuse is a bail,
        // not a panic.
        _ => Err("internal: specialize_num_loop called on a non-Loop node"),
    };
    match built {
        Ok(p) => {
            let mut nl = NumLoop {
                seeds: p.seeds,
                consts: p.consts,
                loads: p.loads,
                test: p.test,
                then: p.then,
                els: p.els,
                guards: p.guards,
                fallback: ir,
                lane_variants: Vec::new(),
            };
            let mut lanes = false;
            let mut superloop = false;
            // W1: the AOT tag-flow fixpoint, run once here at resolve time
            // (never per call, never per iteration) -- see `lanes::
            // build_lane_variants`. `MOVA_NO_LANES=1` is a pure EMISSION
            // gate, same discipline as `MOVA_NO_NUMLOOP` above: with no
            // variant ever built there is nothing left for `exec_num_loop`
            // to be wrong about, so that env var is a genuine A/B of the
            // feature rather than of one of its two code paths.
            if lanes_enabled && !lanes_disabled_by_env() {
                nl.lane_variants = super::lanes::build_lane_variants(&nl);
                lanes = !nl.lane_variants.is_empty();
                // W6: the superloop shape pass, run once here for the same
                // reason and with the same discipline -- a pure EMISSION
                // gate, so `MOVA_NO_SUPERLOOP=1` (or the per-interpreter
                // switch the 5-way differential needs) leaves no variant
                // carrying a shape and nothing in the hot path to be wrong
                // about.
                if superloop_enabled && !superloop_disabled_by_env() {
                    super::lanes::attach_superloops(&mut nl.lane_variants);
                    superloop = nl.lane_variants.iter().any(|v| v.sup.is_some());
                }
            }
            (Ir::NumLoop(Box::new(nl)), LoopDecision::Specialized { lanes, superloop })
        }
        Err(reason) => (ir, LoopDecision::Generic { reason }),
    }
}

/// True when `MOVA_NO_SUPERLOOP=1` was set at process start: W6's kill
/// switch (LATENCY-CAMPAIGN.md §7), stopping any lane variant from ever
/// carrying a shape-specialized superloop. `MOVA_NO_LANES=1` (and
/// everything that subsumes IT) subsumes this one: with no lane variant
/// there is nothing to attach a shape to.
pub(super) fn superloop_disabled_by_env() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| std::env::var("MOVA_NO_SUPERLOOP").is_ok_and(|v| v == "1"))
}

/// True when `MOVA_NO_LANES=1` was set at process start: W1's kill switch
/// (LATENCY-CAMPAIGN.md), stopping any `NumLoop` from ever carrying a lane
/// variant. `MOVA_NO_NUMLOOP=1` and `MOVA_NO_COMPILE=1` both subsume it
/// (no `NumLoop` at all, so nothing to attach a variant to).
pub(super) fn lanes_disabled_by_env() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| std::env::var("MOVA_NO_LANES").is_ok_and(|v| v == "1"))
}

/// `NumLoop`'s I7, proved rather than assumed: every register index the
/// finished node mentions is below `NUM_REGS`.
///
/// `NumCtx::temp` is already the only allocator and already refuses to go
/// past the bound, so this can only fail if that changes. It runs once per
/// compiled loop (never per iteration), and it is what lets `exec.rs` index
/// the register file with a mask instead of a bounds check without the mask
/// being load-bearing: a register file this walk has approved cannot be
/// indexed out of range, so the mask never changes an index.
fn validate_regs(p: &NumLoopParts) -> bool {
    fn ok(r: u8) -> bool {
        (r as usize) < NUM_REGS
    }
    fn ops_ok(ops: &[NumOp]) -> bool {
        ops.iter().all(|o| ok(o.dst) && ok(o.a) && ok(o.b))
    }
    fn branch_ok(b: &NumBranch) -> bool {
        match b {
            NumBranch::Recur { ops, next } => ops_ok(ops) && next.iter().copied().all(ok),
            NumBranch::Ret { ops, out } => ops_ok(ops) && ok(*out),
            // No ops, no output register: nothing to be out of range.
            NumBranch::RetNil => true,
        }
    }
    p.seeds.len() <= NUM_MAX_BINDS
        && p.consts.iter().all(|(r, _)| ok(*r))
        && p.loads.iter().all(|(r, _)| ok(*r))
        && ops_ok(&p.test.ops)
        && ok(p.test.a)
        && ok(p.test.b)
        && branch_ok(&p.then)
        && branch_ok(&p.els)
}

/// Everything a `NumLoop` needs except its fallback (which is the very node
/// being inspected, so it can only be moved in afterwards).
struct NumLoopParts {
    seeds: Vec<NumSeed>,
    consts: Vec<(u8, Num)>,
    loads: Vec<(u8, NumLoad)>,
    test: NumTest,
    then: NumBranch,
    els: NumBranch,
    guards: Vec<GlobalChain>,
}

/// The recognizer proper. `Err(reason)` at any step means "leave the generic
/// loop alone, and here is why" (field1/W-EXPLAIN); see `ir::NumLoop` for the
/// grammar this implements.
fn build_num_loop(
    binds: &[(CompiledPattern, Ir)],
    scratch_base: u16,
    body: &[Ir],
) -> Result<NumLoopParts, &'static str> {
    // A `loop` with no bindings has no state to keep in registers, and one
    // with more than `NUM_MAX_BINDS` overruns the executor's
    // simultaneous-rebind staging buffer. Both keep the generic node. The
    // upper bound is a REJECTION, never a truncation: a 9-binding loop is
    // not specialized at all.
    if binds.is_empty() {
        return Err("loop has no bindings -- nothing to keep in registers");
    }
    if binds.len() > NUM_MAX_BINDS {
        return Err("loop has more bindings than NumLoop's simultaneous-rebind buffer holds");
    }
    let mut slots = Vec::with_capacity(binds.len());
    for (pat, _) in binds {
        match pat {
            CompiledPattern::Slot(s) => slots.push(*s),
            // A destructuring binding is not a scalar.
            _ => return Err("a loop binding destructures instead of naming a plain scalar"),
        }
    }
    let mut seeds = Vec::with_capacity(binds.len());
    for (_, init) in binds {
        seeds.push(match init {
            Ir::Const(Value::Int(i)) => NumSeed::Const(Num::I(*i)),
            Ir::Const(Value::Float(f)) => NumSeed::Const(Num::F(*f)),
            // A slot read is pure, but NOT if it is one of this loop's own
            // bindings: `(loop [i 0 j i] ..)`'s second init reads the slot
            // the first init has just written, which only the generic
            // `Loop` does. (Same rule covers a shadowing rebind of the same
            // NAME, `(loop [a 0 a a] ..)`: each `a` has its own slot, and
            // the second init reads the first's.)
            Ir::LoadSlot(s) if !slots.contains(s) => NumSeed::Slot(*s),
            _ => return Err("a loop binding's seed isn't a numeric literal or an outer scalar slot"),
        });
    }
    // The body must be exactly one `if`. Its else may be ABSENT (W-NUMLOOP):
    // a 2-arg `if` yields `nil` on the missing branch, which `NumBranch::
    // RetNil` now encodes exactly -- that is the `when`-shaped loop, and it
    // is the whole reason this widening exists. A body of several forms, or
    // of anything else, is still out of the grammar. Nested `if`s in a
    // branch are rejected further down, by `build_num_branch` finding
    // neither a `recur`, nor `nil`, nor a scalar expression.
    let (test, then, els) = match body {
        [Ir::If { test, then, els }] => (test, then, els.as_deref()),
        _ => return Err("loop body isn't a single (if <test> ...) form"),
    };
    let mut ctx = NumCtx {
        slots,
        next_reg: binds.len(),
        consts: Vec::new(),
        loads: Vec::new(),
        guards: Vec::new(),
    };
    let test = build_num_test(test, &mut ctx)?;
    let then = build_num_branch(then, scratch_base, binds.len(), &mut ctx)?;
    // A MISSING else is `nil`, with no `Ir` to inspect -- the one branch
    // this matcher builds without lowering anything.
    let els = match els {
        Some(e) => build_num_branch(e, scratch_base, binds.len(), &mut ctx)?,
        None => NumBranch::RetNil,
    };
    // Neither branch iterating means this is not a loop at all; leave it to
    // the generic node rather than grow a second encoding of `if`. (This is
    // also what refuses the degenerate `(loop [i 0] (when <test>))`, whose
    // two branches are both `nil`.)
    if !matches!(then, NumBranch::Recur { .. }) && !matches!(els, NumBranch::Recur { .. }) {
        return Err("neither if-branch recurs -- not actually a loop");
    }
    let parts = NumLoopParts {
        seeds,
        consts: ctx.consts,
        loads: ctx.loads,
        test,
        then,
        els,
        guards: ctx.guards,
    };
    // I7, checked rather than trusted -- see `validate_regs`.
    if !validate_regs(&parts) {
        return Err("internal: a lowered register index escaped validate_regs's bound");
    }
    Ok(parts)
}

/// The loop's test: one comparison intrinsic over two scalar expressions.
///
/// Only these five ops, and only at exactly two arguments. `intrin_op`
/// already refuses to emit any of them at another arity (a chained `(< a b
/// c)` keeps the native's own short-circuiting), but the arity is re-checked
/// here rather than inherited: this matcher must be readable as a total
/// function of the `Ir` it is handed, not as a function of what some other
/// pass promises never to build.
fn build_num_test(ir: &Ir, ctx: &mut NumCtx) -> Result<NumTest, &'static str> {
    let Ir::Intrinsic {
        op, chain, args, ..
    } = ir
    else {
        return Err("loop test isn't a comparison intrinsic call");
    };
    let cmp = match op {
        IntrinOp::Lt2 => NumCmp::Lt,
        IntrinOp::Le2 => NumCmp::Le,
        IntrinOp::Gt2 => NumCmp::Gt,
        IntrinOp::Ge2 => NumCmp::Ge,
        // `=` is admissible only HERE, in test position, and only because
        // both operands are already known to be `Num`s: `numbers::num_eq`
        // reproduces `values_equal` for that case (see `NumCmp`). It is not
        // admissible as an operand, where it would produce a `Bool`.
        IntrinOp::Eq2 => NumCmp::Eq,
        _ => return Err("loop test intrinsic isn't one of < <= > >= = (unsupported test op)"),
    };
    if args.len() != 2 {
        return Err("loop test comparison isn't exactly 2-ary");
    }
    ctx.push_guard(chain);
    let mut ops = Vec::new();
    let a = build_num_expr(&args[0], ctx, &mut ops)?;
    let b = build_num_expr(&args[1], ctx, &mut ops)?;
    Ok(NumTest { ops, cmp, a, b })
}

/// One arm of the loop's `if`: a `recur` to this very loop, the scalar
/// expression the loop leaves with, or (W-NUMLOOP) `nil`.
///
/// A `recur` is recognized ONLY as the whole arm. Anywhere else -- inside an
/// operand (`(+ 1 (recur ..))`), inside a nested `if`, inside another
/// `recur`'s arguments -- it reaches `build_num_expr`, which has no arm for
/// `Ir::Recur` and returns `None`, so the whole loop stays generic. That is
/// deliberate and load-bearing: a non-tail `recur` ABANDONS the pending
/// expression (COMPILE-TIER-DESIGN.md constraint 2), and a register machine
/// with no unwind has no way to say so.
fn build_num_branch(
    ir: &Ir,
    scratch_base: u16,
    n_binds: usize,
    ctx: &mut NumCtx,
) -> Result<NumBranch, &'static str> {
    let mut ops = Vec::new();
    match ir {
        Ir::Recur {
            args,
            scratch_base: sb,
        } => {
            // A `recur` aimed at an ENCLOSING loop or at the fn body unwinds
            // past this node; the register machine has no way to express
            // that, so the whole loop stays generic.
            if *sb != scratch_base || args.len() != n_binds {
                return Err("recur in this branch targets an enclosing loop/fn, not this one");
            }
            let mut next = Vec::with_capacity(n_binds);
            for a in args {
                next.push(build_num_expr(a, ctx, &mut ops)?);
            }
            Ok(NumBranch::Recur { ops, next })
        }
        // W-NUMLOOP: an explicit `nil` literal. `build_num_expr` has no arm
        // for it (it is not a `Num`), so this arm must come first -- and it
        // is the only `Ir::Const` this fn treats specially: every other
        // non-numeric constant still falls through to `build_num_expr`'s
        // `Err` and keeps the whole loop generic.
        Ir::Const(Value::Nil) => Ok(NumBranch::RetNil),
        // W-NUMLOOP: a `do` in branch position, but ONLY at zero or one
        // form -- where it is a SHAPE around the branch, not an extra
        // evaluation step. `(do)` evaluates to exactly `Value::Nil`, and
        // `(do x)` evaluates exactly `x` and returns exactly its value
        // (including letting a `Flow::Recur` through untouched), so
        // unwrapping either is an identity, not a semantic decision. This
        // is what `when` expands to -- `(if <test> (do <body>..))` -- so
        // without it the missing-else rule above would never fire on the
        // shape it exists for.
        //
        // TWO OR MORE forms is declined: the leading forms are evaluated
        // for effect, and effects between the test and the `recur` are
        // exactly what `NumLoop`'s I2 says cannot happen here. That is why
        // `dotimes` specializes only with an EMPTY body.
        Ir::Do(forms) => match forms.as_slice() {
            [] => Ok(NumBranch::RetNil),
            [only] => build_num_branch(only, scratch_base, n_binds, ctx),
            _ => Err("branch is a (do ...) of more than one form -- effects between test and recur"),
        },
        other => {
            let out = build_num_expr(other, ctx, &mut ops)?;
            Ok(NumBranch::Ret { ops, out })
        }
    }
}

/// Lowers one scalar expression, appending its ops and returning the
/// REGISTER its value lands in. `None` = "not scalar arithmetic over this
/// loop's registers", which sends the whole loop back to the generic node.
///
/// Every arm is an explicit shape. There is no catch-all that could admit a
/// node by accident: a `Const` that is not `Int`/`Float`, an
/// `Ir::CreationEnvLookup` (re-probed per access against a live env this
/// loop does not own, so not hoistable), an `Ir::SelfRef`, a `Call`, a
/// `Do`/`Let`/`Loop`/`If`, a `Recur` in a non-tail position -- all fall to
/// the final `None`.
fn build_num_expr(ir: &Ir, ctx: &mut NumCtx, ops: &mut Vec<NumOp>) -> Result<u8, &'static str> {
    const REG_OVERFLOW: &str = "register overflow -- loop needs more scalar registers than NUM_REGS";
    match ir {
        Ir::Const(Value::Int(i)) => ctx.const_reg(Num::I(*i)).ok_or(REG_OVERFLOW),
        Ir::Const(Value::Float(f)) => ctx.const_reg(Num::F(*f)).ok_or(REG_OVERFLOW),
        // A slot or capture that is NOT one of this loop's bindings is
        // loop-invariant: the machine cannot write either, so reading it
        // once at entry is what the generic loop's per-iteration read
        // would have found every time. See `NumLoop::loads`.
        Ir::LoadSlot(s) => ctx.slot_reg(*s).ok_or(REG_OVERFLOW),
        Ir::LoadCapture(i) => ctx.load_reg(NumLoad::Capture(*i)).ok_or(REG_OVERFLOW),
        Ir::Intrinsic {
            op, chain, args, ..
        } => {
            // Arity is re-checked per op rather than inherited from
            // `intrin_op`'s emission rules, for the reason given on
            // `build_num_test`: silently ignoring an extra argument is the
            // one way this matcher could compute something the tree-walker
            // does not.
            match op {
                // The n-ary folds. Their identity element is part of the
                // arithmetic (`(+ -0.0 -0.0)` is `0.0` BECAUSE the fold
                // starts at `Int(0)`), so the lowering starts there too --
                // exactly `exec_intrinsic`'s chain of `add_step`s.
                IntrinOp::Add | IntrinOp::Mul => {
                    if args.len() < 2 {
                        return Err("internal: n-ary +/* intrinsic with fewer than 2 args");
                    }
                    ctx.push_guard(chain);
                    // Every argument is lowered before the first fold step,
                    // matching the native's "evaluate all, then fold".
                    let mut vals = Vec::with_capacity(args.len());
                    for a in args {
                        vals.push(build_num_expr(a, ctx, ops)?);
                    }
                    let (fold, bin) = match op {
                        IntrinOp::Add => (NumBin::AddFold, NumBin::Add),
                        _ => (NumBin::MulFold, NumBin::Mul),
                    };
                    // Allocated AFTER every argument, so no argument can
                    // name it and the fold may reuse it for every step (each
                    // step reads the accumulator before writing it).
                    let dst = ctx.temp().ok_or(REG_OVERFLOW)?;
                    // The first instruction carries the identity step (see
                    // `NumBin`); the rest are ordinary fold steps.
                    ops.push(NumOp {
                        dst,
                        op: fold,
                        a: vals[0],
                        b: vals[1],
                    });
                    for v in &vals[2..] {
                        ops.push(NumOp {
                            dst,
                            op: bin,
                            a: dst,
                            b: *v,
                        });
                    }
                    Ok(dst)
                }
                IntrinOp::Sub2 => {
                    if args.len() != 2 {
                        return Err("internal: 2-ary - intrinsic without 2 args");
                    }
                    ctx.push_guard(chain);
                    let a = build_num_expr(&args[0], ctx, ops)?;
                    let b = build_num_expr(&args[1], ctx, ops)?;
                    let dst = ctx.temp().ok_or(REG_OVERFLOW)?;
                    ops.push(NumOp {
                        dst,
                        op: NumBin::Sub,
                        a,
                        b,
                    });
                    Ok(dst)
                }
                // `inc`/`dec` ARE `add`/`sub` against `Int(1)` -- that is
                // literally how `numbers::inc1`/`dec1` are written.
                IntrinOp::Inc | IntrinOp::Dec => {
                    if args.len() != 1 {
                        return Err("internal: 1-ary inc/dec intrinsic without 1 arg");
                    }
                    ctx.push_guard(chain);
                    let a = build_num_expr(&args[0], ctx, ops)?;
                    let b = ctx.const_reg(Num::I(1)).ok_or(REG_OVERFLOW)?;
                    let dst = ctx.temp().ok_or(REG_OVERFLOW)?;
                    ops.push(NumOp {
                        dst,
                        op: match op {
                            IntrinOp::Inc => NumBin::Add,
                            _ => NumBin::Sub,
                        },
                        a,
                        b,
                    });
                    Ok(dst)
                }
                // Listed rather than caught by `_`, so that adding an
                // `IntrinOp` fails to compile until it is classified here.
                //
                // `/` is a deliberate NON-GOAL, not an oversight: it is the
                // one arithmetic op that can FAIL (`Int`/0 is a
                // `DivideByZero` error, which an infallible register machine
                // cannot raise at the right span and the right iteration)
                // and the one whose result type depends on the VALUES
                // (`(/ 4 2)` is `Int`, `(/ 3 2)` is `Float`). Both are
                // expressible, at the cost of a fallible op and a second
                // promotion rule to keep in step with `divide` -- and no
                // benchmarked workload here needs it. See
                // COMPILE-TIER-DESIGN.md's NumLoop section.
                //
                // `=`/`<`/`<=`/`>`/`>=` produce `Bool` and `zero?`/`not`
                // produce `Bool`: none is a `Num`, so none can be an
                // operand. (`=` and the ordered comparisons ARE admissible
                // in test position -- see `build_num_test`.)
                IntrinOp::Div2
                | IntrinOp::Lt2
                | IntrinOp::Le2
                | IntrinOp::Gt2
                | IntrinOp::Ge2
                | IntrinOp::Eq2
                | IntrinOp::Zero
                | IntrinOp::Not => Err("expression uses an intrinsic that isn't numeric-valued (/, comparison, zero?, not)"),
            }
        }
        _ => Err("expression node isn't scalar arithmetic (fn call, nth, nested if/let/loop, ...)"),
    }
}

/// `Some(values)` iff every node folded to a constant -- the only case in
/// which a collection literal may be pre-built at compile time, since
/// otherwise element evaluation order (and its errors) would be observable.
fn const_values(parts: &[Ir]) -> Option<Vec<Value>> {
    parts
        .iter()
        .map(|p| match p {
            Ir::Const(v) => Some(v.clone()),
            _ => None,
        })
        .collect()
}

/// K5 kill switch: `MOVA_NO_NEWINST=1` keeps every `(new C ..)` a plain `Ir::Escape`.
fn new_inst_off() -> bool {
    static OFF: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *OFF.get_or_init(|| std::env::var("MOVA_NO_NEWINST").is_ok_and(|v| v == "1"))
}
