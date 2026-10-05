//! Namespaces (v0.5 / R1): qualified interning, the ONE resolution order
//! both tiers obey, per-ns alias/refer tables, and multi-file loading.
//!
//! ## Where a var lives
//!
//! There is still exactly one globals frame, and a var still is an
//! `Arc<VarCell>` in it -- namespaces are a naming discipline over that one
//! frame, not a frame per namespace. A `def` of `name` inside namespace
//! `a.b` interns the symbol `a.b/name`; two files that both define `step`
//! therefore get two distinct cells and cannot clobber each other. The
//! bare (unqualified) names belong to ONE namespace, [`CORE_NS`]: the Rust
//! builtins and the `core/*.mova` bootstrap, which every namespace sees
//! without asking. That is what makes `(def resolve ...)` in a user
//! namespace harmless -- it creates `user.ns/resolve` and leaves the bare
//! builtin cell (and its `pristine_builtin` intrinsic gating) untouched.
//!
//! ## Resolution order
//!
//! [`Interp::for_each_global_candidate`] is the single source of truth, and
//! it exists as a callback walk rather than a `Vec` builder precisely so
//! the two tiers cannot drift: the tree-walker probes each candidate with
//! `Env::get_exact` and stops at the first hit; the compiled tier interns
//! each candidate's cell and stops at the first cell that is already bound
//! (see `compile::ir::GlobalChain`). Same order, same stopping rule.
//!
//! ```text
//!   qualified  q/n : (alias q -> full ns)/n , then bare n
//!   unqualified  n : current-ns/n , (refer ns)/n , then bare n
//! ```
//!
//! The trailing bare-`n` step is load-bearing twice over: it is how every
//! namespace reaches core, and it is how `flow/process` and
//! `clojure.string/blank?` reach natives/core fns that are only registered
//! under one of the two spellings (`builtins::strings::alias`,
//! `core/flow.mova`). Because `current-ns/n` is tried FIRST, a namespace's
//! own `def` shadows a core name for that namespace only -- which is why
//! `(:refer-clojure :exclude [...])` can be parsed and ignored.
//!
//! ## Which namespace is "current" when a fn body runs
//!
//! Resolution happens at ACCESS time in the tree-walker and at
//! closure-creation time in the compiled tier, so the two can only agree if
//! a fn body always runs in the namespace it was written in. Every
//! `Closure` therefore records its defining ns (`Closure::ns`) and
//! `apply_closure` makes it current for the duration of the call. Without
//! that, a fn defined in `a.b` and called from `c.d` would resolve its own
//! free names against `c.d` in the tree-walker.
//!
//! ## Threading
//!
//! The registry (per-ns tables + the loaded set) is an `Arc<RwLock<..>>`
//! SHARED by `Interp::fork`, exactly like the globals `Env` is: a namespace
//! loaded on one thread is loaded for every thread, which is the same
//! contract vars already have. `current_ns` and the `loading` stack are
//! per-interpreter (a fork gets a snapshot): the first is a property of the
//! code being run, the second of one thread's in-progress `require` chain.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use crate::env::VarCell;
use crate::error::RjError;
use crate::eval::Interp;
use crate::reader::Span;
use crate::sync::{lock_read, lock_write};
use crate::types::{InstVal, TypeDef};
use crate::value::{PMap, PVec, Str, Symbol, Value};

/// The namespace bare names live in: the Rust builtins and `core/*.mova`.
/// `def` inside it interns bare (that is what "core" MEANS here), and
/// resolution skips the redundant `clojure.core/n` candidate.
pub const CORE_NS: &str = "clojure.core";

/// The namespace a REPL session or a file without an `(ns ...)` form runs
/// in. Not special in any other way: its defs intern as `user/name` like
/// any other namespace's.
pub const USER_NS: &str = "user";

/// DESIGN-flow-namespace.md Part 1 point 3: mova's engine-owned default
/// alias table -- mova's analog of Clojure's implicit `clojure.core`
/// refer, a deliberate language stance ("async and flow are part of the
/// language"), not general resolution magic. `expand_alias` and
/// `Interp::reader_ns_context` both consult this, at the SAME precedence:
/// (a) the current ns's own `:as`/`:as-alias` table, (b) a literal
/// existing namespace of that name (a user's own `(ns flow)` always
/// wins), (c) this table, last resort. A static table needs no
/// `bump_global_generation` -- it never changes at runtime.
///
/// Deliberately NOT visible via `(ns-aliases)`: these belong to no
/// namespace, exactly like `clojure.core`'s own implicit refer has no
/// `:as` entry anywhere. `"string" -> "clojure.string"` is a documented
/// follow-up (DESIGN-flow-namespace.md), not added here.
const DEFAULT_ALIASES: &[(&str, &str)] = &[
    ("flow", "clojure.core.async.flow"),
    ("async", "clojure.core.async"),
];

/// [`DEFAULT_ALIASES`] for the source index (`mova --source-index`).
pub fn default_aliases() -> &'static [(&'static str, &'static str)] {
    DEFAULT_ALIASES
}

/// The default-alias target for `q`, or `None` if `q` isn't one of
/// [`DEFAULT_ALIASES`]'s keys. Callers apply this only as the LAST
/// resort -- see that table's doc for the full precedence.
fn default_alias_target(q: &str) -> Option<&'static str> {
    DEFAULT_ALIASES.iter().find(|(k, _)| *k == q).map(|(_, v)| *v)
}

/// How many namespaces have `NsInfo::no_core` set (a fast "none" test for resolution).
static NO_CORE_NS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// One namespace's tables. Created empty the first time the namespace is
/// mentioned (by an `ns` form or by a `require` of it).
#[derive(Default, Clone)]
pub struct NsInfo {
    /// `:as` aliases: local alias -> full namespace name.
    pub aliases: HashMap<Str, Str>,
    /// `:refer` mappings: unqualified LOCAL name -> (namespace that
    /// defines it, name it's defined UNDER there). The two names differ
    /// only for a `(refer ns :rename {...})` entry (W3f, `errors.clj`'s
    /// `assert-arg-messages`: `(refer 'clojure.core :rename '{with-open
    /// renamed-with-open})` must make the bare symbol `renamed-with-open`
    /// resolve to `clojure.core/with-open`) -- every OTHER caller
    /// (`add_refer`) still passes the same name for both, so an ordinary
    /// refer is exactly the pre-W3f behavior.
    pub refers: HashMap<Str, (Str, Str)>,
    /// A program made this namespace (`ns`, `in-ns`, `create-ns`), as opposed
    /// to the registry seeding it for natives. Tooling only.
    pub declared: bool,
    /// `in-ns` made this namespace: clojure.core is NOT referred into it
    /// (the JVM rule). `ns` and `refer-clojure` clear it.
    pub no_core: bool,
}

/// Every namespace's tables plus the loaded-file set, shared by every
/// forked `Interp` (see this module's doc).
#[derive(Default, Clone)]
pub struct NsRegistry {
    namespaces: HashMap<Str, NsInfo>,
    loaded: HashSet<Str>,
}

pub type Namespaces = Arc<RwLock<NsRegistry>>;

/// A deep, independent copy of `reg` -- its own `Arc<RwLock<..>>` around a
/// `Clone` of the current tables -- for `Interp::snapshot`'s FORK semantics.
/// Unlike `Interp::fork` (which clones the `Arc` itself, deliberately
/// SHARING the registry), this is used when a `require`/alias/refer done
/// after the snapshot must not become visible on the other side: without
/// this, both interpreters would still point at the same `RwLock` even
/// though their `globals` had already diverged, an inconsistency where
/// `def`s fork but namespace bookkeeping doesn't.
pub(crate) fn snapshot(reg: &Namespaces) -> Namespaces {
    Arc::new(RwLock::new(lock_read(reg).clone()))
}

/// S4: `*ns*`/`find-ns`/`the-ns`/`ns-name`/`ns-resolve`/`all-ns` need a
/// first-class namespace VALUE, not just the plain `Str` name this module
/// otherwise tracks internally. Reuses S3's `defrecord`/`deftype` machinery
/// (`crate::types`) rather than adding a new `Value` variant: one shared
/// deftype-shaped `TypeDef` named `clojure.lang.Namespace` (measured:
/// `(class *ns*)` prints that bare name) with a single basis field `name`
/// (so `(.name *ns*)` -- already-existing dot-field-access hook,
/// `eval::types_forms::eval_dot_form` -- returns it as a SYMBOL, matching
/// `(class (.name *ns*))` => `clojure.lang.Symbol`, measured). Deftype
/// shape (not record) is deliberate: measured `(map? *ns*)` is false in
/// real Clojure, and deftypes don't get folded into any map-wide op.
///
/// One canonical `Arc<InstVal>` is cached per namespace NAME (not
/// reconstructed per call) so `(= (the-ns 'user) (find-ns 'user))` is
/// `true` -- deftype-shaped `Value::Inst` equality is `Arc::ptr_eq`
/// (`value.rs`'s `PartialEq` impl), so two calls must hand back the exact
/// same `Arc` for that to hold.
fn ns_tdef() -> Arc<TypeDef> {
    static TDEF: OnceLock<Arc<TypeDef>> = OnceLock::new();
    TDEF.get_or_init(|| {
        Arc::new(TypeDef {
            name: Str::from("clojure.lang.Namespace"),
            basis: vec![Str::from("name")],
            is_record: false,
            // S5: a namespace value implements no interface.
            interfaces: Vec::new(),
            field_tags: Vec::new(),
            mutable: Vec::new(),
            methods: Default::default(),
            protocols: Vec::new(),
        })
    })
    .clone()
}

/// D12: the symbol `*ns*`, built once. `Symbol::simple("*ns*")` allocates
/// a fresh `Arc<StrInner>` every call, and since D12 the `*ns*` var is read
/// (and sometimes written) on the per-call macro-expansion path -- see
/// [`Interp::enter_expansion_ns`]. Cloning this is an `Arc` bump instead.
pub(crate) fn ns_sym() -> &'static Symbol {
    static SYM: OnceLock<Symbol> = OnceLock::new();
    SYM.get_or_init(|| Symbol::simple("*ns*"))
}

/// The namespace value for `name` -- same `Arc<InstVal>` every call (see
/// `ns_tdef`'s doc for why identity must be stable).
pub(crate) fn ns_value(name: &Str) -> Value {
    static CACHE: OnceLock<Mutex<HashMap<Str, Value>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = cache.lock().expect("namespace-value cache poisoned");
    guard
        .entry(name.clone())
        .or_insert_with(|| {
            let mut fields = PVec::new();
            fields.push_back(Value::Sym(Symbol::simple(name.clone())));
            Value::Inst(Arc::new(InstVal {
                tdef: ns_tdef(),
                data: PMap::new(),
                fields: Mutex::new(fields),
                meta: None,
            }))
        })
        .clone()
}

/// `Some(name)` iff `v` is one of `ns_value`'s namespace instances (by
/// `TypeDef` `Arc` identity, the same test every other S3 class check
/// uses); `None` for anything else, including a plain symbol -- callers
/// that also accept a bare symbol naming a namespace (`the-ns`/`find-ns`/
/// `ns-resolve`, all measured) layer that on top via `ns_name_arg`.
pub(crate) fn ns_value_name(v: &Value) -> Option<Str> {
    if let Value::Inst(inst) = v {
        if Arc::ptr_eq(&inst.tdef, &ns_tdef()) {
            if let Some(Value::Sym(s)) = crate::sync::lock_mutex(&inst.fields).get(0) {
                return Some(s.name.clone());
            }
        }
    }
    None
}

/// The namespace NAME `v` designates, accepting either shape every S4
/// namespace fn does (measured): a namespace value itself, or a bare
/// symbol naming one.
fn ns_name_arg(v: &Value) -> Option<Str> {
    ns_value_name(v).or_else(|| match v {
        Value::Sym(s) => Some(s.name.clone()),
        _ => None,
    })
}

/// C3h (clojure.repl surface): the symbol that IDENTIFIES the var interned
/// under `name` within `ns_name` -- bare (`ns: None`) in [`CORE_NS`], where
/// every def interns unqualified (see this module's doc), qualified
/// (`ns_name/name`) everywhere else. The inverse-lookup sibling of
/// [`Interp::qualify_def`] (which only ever answers for the CURRENT
/// namespace); this answers for an ARBITRARY target namespace, which is
/// what enumerating another namespace's vars (`ns-publics`, and anything
/// built on it -- `dir-fn`/`apropos`) needs: `Env::names_in_ns` hands back
/// bare NAMES, and the cell each one is actually interned under can only
/// be found back by reconstructing this same symbol spelling.
pub(crate) fn var_symbol_in(ns_name: &Str, name: &Str) -> Symbol {
    if ns_name.as_ref() == CORE_NS {
        Symbol::simple(name.clone())
    } else {
        Symbol { ns: Some(ns_name.clone()), name: name.clone() }
    }
}

impl Interp {
    /// Makes `ns` current, creating its (empty) tables if this is the first
    /// mention of it. Also refreshes the bare global `*ns*` to `ns`'s
    /// namespace value (S4) -- a plain global write, not a per-thread
    /// dynamic binding like `*out*`'s (M4b): `globals` is an `Arc`-shared
    /// `Env` across every `Interp::fork` (see that fn's doc), so a
    /// `set_current_ns` on one forked thread is, today, visible to every
    /// other one too. Documented, accepted gap -- none of this
    /// campaign's target suite files switch namespaces from inside a
    /// `future`/other thread, and wiring `*ns*` into the real per-thread
    /// dynamic-binding stack is a separate, larger piece of work than S4's
    /// scope.
    ///
    /// C3f, measured: real Clojure's `in-ns` is literally `(set! *ns*
    /// (the-ns name))` (`clojure.core/in-ns`'s own source) -- `set!`
    /// semantics, meaning it writes through the CURRENT THREAD'S innermost
    /// `binding` frame when `*ns*` has one pushed (`(binding [*ns* *ns*]
    /// (in-ns 'tmp.zzz) ...)` measured leaving `*ns*` back on the outer
    /// value once the `binding` body exits), and only falls back to a root
    /// write when no such frame is active (the ordinary top-level `(ns
    /// ...)`/`(in-ns ...)` case, unchanged from before this fix). This
    /// mirrors `VarCell::set_binding`'s own documented `set!` fallback
    /// rule. See `eval::special_forms::eval_binding`'s `*ns*` special case
    /// for the other half: `current_ns` itself (the fast-path field the
    /// tree-walker actually resolves symbols against) is restored there,
    /// since it -- unlike the var's displayed value -- has no dynamic-frame
    /// stack of its own.
    pub fn set_current_ns(&mut self, ns: Str) {
        self.set_dynamic_ns(ns.clone());
        self.current_ns = ns;
    }

    /// field2/W-NS: the DYNAMIC half of [`Self::set_current_ns`] on its own
    /// -- creates `ns`'s tables if absent and moves the `*ns*` var (through
    /// the innermost `binding` frame when one is live, exactly as before;
    /// see this fn's caller's C3f note above), WITHOUT touching
    /// `current_ns`, the lexical field the tree-walker resolves free
    /// symbols against. Used by [`Self::switch_ns`] for a `ns`/`in-ns`
    /// executed INSIDE a running fn body.
    pub(crate) fn set_dynamic_ns(&mut self, ns: Str) {
        lock_write(&self.namespaces)
            .namespaces
            .entry(ns.clone())
            .or_default()
            .declared = true;
        self.move_dynamic_ns(&ns);
    }

    /// D12: [`Self::set_dynamic_ns`] MINUS the "create `ns`'s tables if
    /// absent" step -- the write half on its own, for the two callers that
    /// can only ever name a namespace that demonstrably already exists
    /// (`enter_expansion_ns`/`leave_expansion_ns`, which move `*ns*` to
    /// `current_ns` and then back to a name they just READ off `*ns*`).
    /// Skipping the `namespaces` write lock matters because that pair sits
    /// on the per-call macro-expansion path of every tree-walked call site.
    fn move_dynamic_ns(&mut self, ns: &Str) {
        let cell = self.globals.intern(ns_sym());
        let val = ns_value(ns);
        // Bug-1 fix (mova/PLAN.md): no active frame yet on this thread ->
        // push ONE instead of writing the shared root (`*ns*`'s cell has
        // bare `Nil` meta, per `prefers_thread_local_set`'s doc, so this
        // always takes the push path). Establishes this thread's own
        // `*ns*` frame lazily, on first `ns`/`in-ns`, that later `set!`
        // calls on THIS thread mutate in place (`cell.set_binding` above)
        // and that `future*`/`thread`/`go` conveyance copies onto a
        // spawned thread as its own independent frame -- so `(future
        // (in-ns 'x) ...)` no longer changes the caller's `*ns*`.
        if !cell.set_binding(val.clone()) {
            cell.push_binding(val);
        }
    }

    /// field2/W-NS: what the `ns` SPECIAL FORM and the `in-ns` FUNCTION do
    /// -- the one namespace-switch entry point whose answer depends on
    /// WHERE it runs. This is the split that closes the last var-
    /// architecture conformance residue (`tests/conformance/DEVIATIONS.md`
    /// "W4 close" roots 2 and 3).
    ///
    /// * At **top level** (`closure_depth == 0`): moves both, exactly as
    ///   before. Real Clojure's `Compiler.load` reads and compiles one
    ///   top-level form at a time, so a `(ns a)` really does change the
    ///   namespace the NEXT form is compiled in.
    ///
    /// * **Inside a running fn/macro body** (`closure_depth > 0`): moves
    ///   only the dynamic `*ns*`. On the JVM the whole body was compiled,
    ///   in one pass, in the namespace it was WRITTEN in; a `(ns a)`
    ///   reached mid-body at RUN time changes `*ns*` (and therefore what a
    ///   later `eval`/`load`/`read-string`/`ns-publics` in that body sees)
    ///   but cannot retroactively un-resolve the free symbols after it.
    ///   mova's tree-walker resolves those symbols as it reaches them, so
    ///   without this the switch un-resolved them.
    ///
    /// Measured repro (both tiers -- an `ns` in a fn body forces the whole
    /// fn to bail out of compilation, `compile::resolve`'s `ns` rule, so
    /// the tree-walker answers for both):
    ///
    /// ```clojure
    /// (ns testns)
    /// (def helper 42)
    /// (defmacro switch [] (list 'do (list 'ns 'a) 1))
    /// (defn f [] (switch) helper)
    /// (f)   ;; real Clojure => 42; mova before this fix => "Unable to
    ///       ;; resolve symbol: helper"
    /// ```
    ///
    /// The vendored shape is `repl.clj`'s `test-dynamic-ns`, whose
    /// `(defmacro call-ns [] `(ns a#))` fires inside a `deftest` body and
    /// used to un-resolve the `testing` form after it.
    ///
    /// `def` is deliberately NOT part of this split: [`Self::qualify_def`]
    /// keeps reading the LEXICAL `current_ns`, which is what the JVM does
    /// too (a `def`'s target namespace is fixed when its enclosing unit is
    /// compiled). The one place that must re-base the lexical field on the
    /// dynamic one is a fresh compilation unit -- `eval`, `load`,
    /// `require` -- and each of those already brackets it (see
    /// `builtins::reflect::eval_native` and `load_ns_file`/`load_path`
    /// below, which also reset `closure_depth` so the loaded file's own
    /// top-level `(ns ...)`/`(in-ns ...)` forms take the depth-0 branch).
    pub fn switch_ns(&mut self, ns: Str) {
        if self.closure_depth == 0 {
            self.set_current_ns(ns);
        } else {
            self.set_dynamic_ns(ns);
        }
    }

    /// S6/Blocker-2: the name `*ns*` currently reads as -- set ONLY by
    /// `set_current_ns` (`ns`/`in-ns`/`require_ns`'s file-level switch),
    /// unlike `current_ns` itself, which `apply_closure` ALSO overwrites
    /// for the duration of every fn/macro call so the tree-walker can
    /// re-resolve that body's free symbols against the ns it was WRITTEN
    /// in (see this module's "which namespace is current" doc). Those are
    /// two different questions -- "what ns does this code's own symbols
    /// lexically belong to" vs. "what does the dynamic var `*ns*` read as
    /// right now" -- that real Clojure keeps separate (a `Var`, changed
    /// only by `in-ns`/`binding`) and mova's single `current_ns` field
    /// conflates. `eval` (`builtins::reflect::eval_native`) is the one
    /// place that needs the SECOND answer: Clojure's own docstring says it
    /// "operates in the current value of `*ns*`", and a macro that calls
    /// `eval` in its own body must see the CALLER's ns, not its own
    /// defining ns (measured: `clojure.test.generative`'s `defspec` calls
    /// `eval` on a `:tag` form naming `cgen/ednable`, a var reachable only
    /// through an alias `clojure.test-clojure.api` declares -- `defspec`
    /// itself has no such alias, so reading raw `current_ns` at that point
    /// resolved against the wrong namespace and failed with "Unable to
    /// resolve symbol").
    pub(crate) fn dynamic_ns_name(&self) -> Str {
        match self.globals.get_exact(ns_sym()) {
            Some(v) => ns_value_name(&v).unwrap_or_else(|| self.current_ns.clone()),
            None => self.current_ns.clone(),
        }
    }

    /// D12: opens the `*ns*` bracket every macro expansion runs inside.
    ///
    /// # The defect
    ///
    /// mova expands a macro call site as many times as it EVALUATES it: the
    /// compiled tier freezes one expansion into its IR, but a fn whose body
    /// bails compilation (`binding`, `set!`, ..) is tree-walked, and the
    /// tree-walker re-expands every macro in that body on every call. Real
    /// Clojure expands exactly once, in `Compiler`, with `*ns*` thread-bound
    /// to the namespace being compiled. So a macro that reads `*ns*` during
    /// its own expansion -- `clojure.spec.alpha`'s `res`, via `resolve`, on
    /// every `s/&`/`s/coll-of`/`s/def` argument -- saw the CALLER's dynamic
    /// `*ns*` on the second and later expansions instead of the namespace
    /// the code was written in, and resolved its symbols against the wrong
    /// namespace. Measured (`docs/SPEC-PORT-PATCHES.md` item 12, and pinned
    /// by `eval::tests::d12_tree_walked_reexpansion_sees_the_lexical_ns`):
    /// `my.lib/f6`'s marker macro reported `my.lib` at define time and
    /// `user` on every call, and `(resolve 'secret)` answered `nil` there.
    ///
    /// # The fix
    ///
    /// For the duration of ANY macro expansion, the dynamic `*ns*` reads as
    /// the LEXICAL namespace of the expansion SITE -- `current_ns`, which
    /// `apply_closure` has not yet swapped to the macro's own defining ns at
    /// the point [`crate::eval::Interp::apply_macro`] calls this. That is
    /// what the JVM's compiler effectively does, so this makes mova strictly
    /// MORE JVM-like: a macro reading `*ns*` at expansion time now sees the
    /// compiling file's namespace on the JVM and here alike (oracle, Clojure
    /// 1.13.0-alpha6: the repro above prints `my.lib` once and never `user`).
    ///
    /// # Perf
    ///
    /// The swap is SKIPPED entirely when the dynamic `*ns*` already equals
    /// the lexical one, which is the ordinary case: a file's own top-level
    /// `(ns foo)` moves both (see [`Self::set_current_ns`]), so code written
    /// and run in `foo` pays one `*ns*` var read and one `Str` compare. It is
    /// PAID only when the two have been driven apart -- `binding`/`in-ns`
    /// inside a body, or a harness that runs a file's fns under a different
    /// `*ns*`, which is exactly what the census runner does -- and there it
    /// costs two var writes per expansion, off the `namespaces` write lock
    /// ([`Self::move_dynamic_ns`]). Nothing is added to any non-macro path.
    ///
    /// Returns the value to hand back to [`Self::leave_expansion_ns`]:
    /// `None` when no swap was made.
    pub(crate) fn enter_expansion_ns(&mut self) -> Option<Str> {
        let dynamic = self.dynamic_ns_name();
        if dynamic == self.current_ns {
            return None;
        }
        let lexical = self.current_ns.clone();
        self.move_dynamic_ns(&lexical);
        Some(dynamic)
    }

    /// D12: closes [`Self::enter_expansion_ns`]'s bracket. Must run on EVERY
    /// exit path of the expansion, `Ok` and `Err` alike.
    ///
    /// The restore is CONDITIONAL, and deliberately so. Measured against the
    /// oracle (Clojure 1.13.0-alpha6): a macro body that itself executes
    /// `(in-ns 'zzz)` at expansion time leaves `*ns*` reading `zzz` after the
    /// expansion returns -- the JVM's `*ns*` binding is a per-LOAD bracket,
    /// not a per-expansion one, so a deliberate mid-expansion switch sticks.
    /// So this restores the saved value only when `*ns*` still reads as the
    /// namespace `enter_expansion_ns` parked there; a macro that moved it
    /// keeps its move. (The one case this cannot tell apart is a macro that
    /// deliberately `in-ns`es to the expansion site's OWN namespace, which
    /// is a no-op anyway.)
    ///
    /// `current_ns` is already back to the expansion site's namespace here:
    /// `apply_closure` restores it before returning, and a mid-body
    /// `ns`/`in-ns` runs at `closure_depth > 0` and so moves only the dynamic
    /// half ([`Self::switch_ns`]).
    pub(crate) fn leave_expansion_ns(&mut self, saved: Option<Str>) {
        let Some(saved) = saved else { return };
        if self.dynamic_ns_name() == self.current_ns {
            self.move_dynamic_ns(&saved);
        }
    }

    /// C3d: the reader's `::kw`/`::alias/kw` auto-resolution context
    /// (`crate::reader::NsContext`, see that type's doc) as of RIGHT NOW
    /// -- `dynamic_ns_name`'s "what `*ns*` reads as", for the same
    /// `eval`/`read-string` reason that fn's own doc gives (the CALLER's
    /// live namespace, not whatever ns this code happened to be defined
    /// in), plus that namespace's current alias table. Two callers:
    /// `eval_str`/`eval_str_allow_read_cond` (`eval/mod.rs`), fresh before
    /// every top-level form so a same-file `(require '[x :as y])` is
    /// visible to a later `::y/z`; and `builtins::reflect::read_string`,
    /// once per call.
    // See `Env::interned_namespaces`'s own `#[allow(clippy::mutable_key_type)]`
    // doc comment (and `seed_builtin_namespaces`'s below): `Str`'s cache
    // fields are irrelevant to its `Hash`/`Eq`, so the lint's premise
    // doesn't apply to a `HashMap<Str, _>` here either.
    #[allow(clippy::mutable_key_type)]
    pub(crate) fn reader_ns_context(&self) -> crate::reader::NsContext {
        let ns = self.dynamic_ns_name();
        let mut aliases: HashMap<Str, Str> = self.ns_aliases_of(&ns).into_iter().collect();
        // DESIGN-flow-namespace.md Part 1 point 3 ("reader parity"): the
        // `::alias/kw`/`#::alias{}` auto-resolve context must see the SAME
        // three-step precedence `expand_alias` uses -- per-ns `:as` table
        // (already collected above), then a literal LOADED namespace of
        // that name, then `DEFAULT_ALIASES` as the last resort. Folded in
        // here (rather than consulted lazily per keyword) because
        // `NsContext` is a plain owned snapshot with no back-reference to
        // the registry -- see that struct's doc. `reg.loaded`, mirroring
        // `expand_alias`'s own fix: a mere `reg.namespaces` entry (created
        // by `or_default` on ANY lookup, e.g. a transient `(in-ns 'flow)`
        // that never actually loads anything) must not be enough to shadow
        // the default alias -- see `expand_alias`'s comment on the same
        // check for the full story.
        let reg = lock_read(&self.namespaces);
        for (alias, full) in DEFAULT_ALIASES {
            let alias = Str::from(*alias);
            if !aliases.contains_key(&alias) && !reg.loaded.contains(&alias) {
                aliases.insert(alias, Str::from(*full));
            }
        }
        drop(reg);
        crate::reader::NsContext {
            current_ns: ns,
            aliases,
        }
    }

    /// The symbol a `def` of `sym` interns: `current-ns/name`, except in
    /// [`CORE_NS`] (bare) and for an already-qualified name (used as
    /// written).
    pub fn qualify_def(&self, sym: &Symbol) -> Symbol {
        if sym.ns.is_some() || self.current_ns.as_ref() == CORE_NS {
            return sym.clone();
        }
        Symbol {
            ns: Some(self.current_ns.clone()),
            name: sym.name.clone(),
        }
    }

    /// W3e-4: real Clojure's `Namespace.checkReplacement` warning line, or
    /// `None` when interning `name` in `ns_name` warrants no warning.
    ///
    /// `prior` is whatever `name` resolves to in `ns_name` TODAY (the
    /// caller supplies it, because `def` and `intern` reach that answer
    /// differently -- `def` always interns into the CURRENT namespace and
    /// can use the ordinary candidate order, while `intern` names its
    /// target namespace explicitly and must go through `ns_resolve_in`).
    ///
    /// The rule, transcribed from
    /// `.oracle/clojure-src/src/jvm/clojure/lang/Namespace.java`'s `intern`
    /// and `checkReplacement`: a name with no current mapping interns
    /// silently, a name already mapped to a var of THIS namespace returns
    /// that var silently, and anything else -- i.e. a name currently
    /// reaching some OTHER namespace's var -- warns and replaces. Measured
    /// (`compat/def-shadow-warning-probe.clj`): `(defn prefers [] :mine)`
    /// in ns `probe` prints `WARNING: prefers already refers to:
    /// #'clojure.core/prefers in namespace: probe, being replaced by:
    /// #'probe/prefers`, while redefining `probe/prefers` a second time, or
    /// defining a brand-new name, prints nothing at all.
    ///
    /// A BARE-interned prior cell reads as `clojure.core`'s -- that is what
    /// bare interning means in mova (see [`CORE_NS`] and
    /// `crate::env::Env::names_in_ns`), and it is why the oracle's
    /// `#'clojure.core/prefers` is the right spelling to print.
    /// Real `clojure.core` var names: `:present-vars` (present in mova)
    /// union `:missing-vars` (known-missing, tracked for the build queue),
    /// parsed once from `compat/core-var-inventory.edn`. Used by
    /// [`Self::shadow_warning`] to tell a real-Clojure-core name (worth a
    /// shadow warning) from a mova-only core extension (never warranted
    /// one). Each of those two EDN keys is a flat `[...]` vector of plain
    /// string literals (no nested brackets, no escaped quotes in any
    /// entry), so splitting the bracketed span on `"` and keeping the
    /// odd-indexed pieces recovers every entry without a full EDN reader.
    fn real_core_vars() -> &'static HashSet<&'static str> {
        static SET: OnceLock<HashSet<&'static str>> = OnceLock::new();
        SET.get_or_init(|| {
            const EDN: &str = include_str!("../compat/core-var-inventory.edn");
            let mut set = HashSet::new();
            for key in [":present-vars", ":missing-vars"] {
                // Anchored on `\n <key>\n` (line-start), not a bare
                // substring search: the file's own `:note` prose
                // mentions both key names inline ("... via git diff on
                // :missing-vars / :present-vars ...") well before either
                // key's real definition, and a bare `find` would lock
                // onto that prose instead.
                let anchor = format!("\n {key}\n");
                let start = EDN
                    .find(&anchor)
                    .unwrap_or_else(|| panic!("core-var-inventory.edn missing key {key}"));
                let open = start + EDN[start..].find('[').unwrap();
                let close = open + EDN[open..].find(']').unwrap();
                for (i, piece) in EDN[open + 1..close].split('"').enumerate() {
                    if i % 2 == 1 {
                        set.insert(piece);
                    }
                }
            }
            set
        })
    }

    pub(crate) fn shadow_warning(
        ns_name: &Str,
        name: &Str,
        prior: Option<&std::sync::Arc<VarCell>>,
    ) -> Option<String> {
        let prior = prior?;
        if prior.name.ns.as_ref() == Some(ns_name) {
            return None;
        }
        let prior_ns = prior.name.ns.as_deref().unwrap_or(CORE_NS);
        // W3e-4b: mova interns a handful of real Clojure's `clojure.
        // string`/`clojure.java.io` API bare into `clojure.core` too, as a
        // convenience (see `src/builtins/strings.rs`'s bare-then-alias
        // loop and `src/builtins/sys.rs`'s `delete-file`), AND adds
        // mova-only core extensions (`exit`, `rename`, `file-exists?`,
        // `directory?`, `special-doc-map`, ...). Real Clojure's OWN
        // `clojure.core` never defines any of these, so a user ns
        // defining its own var of one of these names is not shadowing
        // anything real Clojure would ever warn about, and that user def
        // unambiguously wins. General rule (replaces a static name list):
        // warn only when `name` is a REAL `clojure.core` var, per
        // `real_core_vars()` (`compat/core-var-inventory.edn`'s
        // `:present-vars` union `:missing-vars`).
        if prior_ns == CORE_NS && !Self::real_core_vars().contains(name.as_ref()) {
            return None;
        }
        Some(format!(
            "WARNING: {name} already refers to: #'{prior_ns}/{} in namespace: {ns_name}, \
             being replaced by: #'{ns_name}/{name}\n",
            prior.name.name,
        ))
    }

    /// Calls `probe` with each global candidate spelling for `sym`, in
    /// resolution order, and returns the first `Some` it answers. THE
    /// single definition of that order -- see this module's doc.
    pub fn for_each_global_candidate<T>(
        &self,
        sym: &Symbol,
        mut probe: impl FnMut(&Symbol) -> Option<T>,
    ) -> Option<T> {
        // ns: restrict qualified->bare fallback to clojure.core spellings
        // (DESIGN-flow-namespace.md Part 1 point 4's trap): the trailing
        // bare-name probe at the bottom of this fn is what used to let
        // `(a/merge chans)` -- `a` aliased to `clojure.core.async`, which
        // has no `merge` of its own -- silently fall through to bare
        // `merge`, i.e. `clojure.core/merge`, a SILENT WRONG ANSWER on
        // channels rather than a loud "Unable to resolve". Every
        // LEGITIMATE use of that fallback for a QUALIFIED symbol is a
        // core var (interned bare because `qualify_def` never qualifies
        // inside `CORE_NS`) reached through some other namespace's alias
        // (`clojure.core/inc` via `core/let`'s sibling case in
        // `is_bare_or_core_alias`'s own doc, or historically `flow/
        // process`/`clojure.string/blank?` before item 1-4 gave those a
        // real home) -- so the fallback now applies to a qualified symbol
        // ONLY when its EXPANDED namespace is [`CORE_NS`] itself. The
        // unqualified-symbol path (`None` arm below) is completely
        // untouched -- it never went through this trap, since an
        // unqualified symbol's trailing probe IS the bare-core lookup by
        // definition -- and so is the D3 unexpanded-spelling retry
        // (imported short class names), which stays exactly where it was.
        let mut qualified_bare_fallback_ok = true;
        match &sym.ns {
            Some(q) => {
                let full = self.expand_alias(q);
                qualified_bare_fallback_ok = full.as_ref() == CORE_NS;
                if let Some(v) = probe(&Symbol {
                    ns: Some(full.clone()),
                    name: sym.name.clone(),
                }) {
                    return Some(v);
                }
                // D3 (2026-08-21): `expand_alias` also fires for an
                // IMPORTED short class name (`(import '(java.lang
                // Boolean))` rebinds bare `Boolean` in the current ns to
                // its `java.lang.Boolean`-named class value, see
                // `expand_alias`'s own `ClassVal::Builtin` branch), which
                // then translates `Boolean/TRUE` to `java.lang.Boolean/
                // TRUE` -- but `builtins::statics` registers static
                // fields/methods under the SHORT class name only
                // (`Symbol{ns:"Boolean",..}`, see that module's doc).
                // Before this fallback, importing a class broke every
                // static member access spelled with its short name
                // (measured: vendored `evaluation.clj`'s `(import
                // '(java.lang Boolean)) ... Boolean/TRUE` -- fine
                // unimported, "Unable to resolve symbol" once imported).
                // Retrying the UNEXPANDED spelling here, only as a
                // fallback after the expanded one already missed, can
                // only ADD candidates a bare probe already wouldn't have
                // found some other way -- never shadow or reorder an
                // existing hit.
                if full != *q {
                    if let Some(v) = probe(&Symbol {
                        ns: Some(q.clone()),
                        name: sym.name.clone(),
                    }) {
                        return Some(v);
                    }
                }
            }
            None => {
                if self.current_ns.as_ref() != CORE_NS {
                    if let Some(v) = probe(&Symbol {
                        ns: Some(self.current_ns.clone()),
                        name: sym.name.clone(),
                    }) {
                        return Some(v);
                    }
                }
                if let Some((from, source_name)) = self.refer_source(&sym.name) {
                    // W3f: a refer FROM `clojure.core` (overwhelmingly the
                    // common case -- every plain, unaliased `use`/`refer`
                    // in this corpus) needs the BARE spelling here, not
                    // `clojure.core/source_name` -- every core builtin is
                    // interned bare (`Symbol{ns: None, ..}`, see
                    // `names_in_ns`'s own `core && sym.ns.is_none()`
                    // check), so a qualified probe would never find it.
                    // This went unnoticed before `:rename` existed only
                    // because `source_name == sym.name` for every ORDINARY
                    // refer, which the bare-name-last fallback a few lines
                    // below already covers by accident; `:rename`'s two
                    // names genuinely differ (`errors.clj`'s `assert-arg-
                    // messages`: `renamed-with-open` referring `with-open`
                    // under a new local name), so it needs a REAL probe
                    // under the source name specifically.
                    let probe_sym = if from.as_ref() == CORE_NS {
                        Symbol::simple(source_name)
                    } else {
                        Symbol { ns: Some(from), name: source_name }
                    };
                    if let Some(v) = probe(&probe_sym) {
                        return Some(v);
                    }
                }
            }
        }
        // `in-ns` into a new namespace does not refer clojure.core into it
        if sym.ns.is_none() && !self.ns_sees_core(&self.current_ns) {
            return None;
        }
        // The bare name last: core for an unqualified symbol (always), or
        // -- for a qualified one -- only when `qualified_bare_fallback_ok`
        // (its expanded namespace is `clojure.core`) said this is a
        // legitimate core-var reference, not an accidental cross-namespace
        // guess (see this fn's opening comment).
        if sym.ns.is_none() || qualified_bare_fallback_ok {
            probe(&Symbol::simple(sym.name.clone()))
        } else {
            None
        }
    }

    /// A global symbol's value under the resolution order above. Locals are
    /// NOT considered -- `eval_form_in` probes the lexical frames first.
    pub fn lookup_global(&self, sym: &Symbol) -> Option<Value> {
        self.for_each_global_candidate(sym, |cand| self.globals.get_exact(cand))
    }

    /// Resolves `sym` to its `Arc<VarCell>` (v0.5 / R2's `(var x)`/`#'x`),
    /// under the SAME candidate order `lookup_global` reads a value
    /// through -- probing for an already-BOUND cell first, so `#'foo`
    /// written before `(def foo ...)` in the same namespace still ends up
    /// holding the identical cell that `def` will later write through
    /// (`Env::set`'s "cell identity preserved across redefinition"
    /// contract makes that safe). If no candidate has a bound cell yet,
    /// interns -- and returns -- the QUALIFIED spelling `def` itself would
    /// write to (`qualify_def`), exactly mirroring how `eval_def` qualifies
    /// before it writes.
    pub fn resolve_var_cell(&self, sym: &Symbol) -> std::sync::Arc<crate::env::VarCell> {
        if let Some(cell) = self.for_each_global_candidate(sym, |cand| self.globals.find_bound_cell(cand)) {
            return cell;
        }
        self.globals.intern(&self.qualify_def(sym))
    }

    /// S4: `resolve`/`ns-resolve`'s cell lookup -- the SAME candidate
    /// order as `resolve_var_cell`, but `None` instead of auto-interning a
    /// fresh unbound cell when nothing is bound yet (measured: `(resolve
    /// 'no-such-var)` is `nil` with no side effect, unlike `#'no-such-var`
    /// which DOES intern a placeholder).
    ///
    /// field4 (f4/ns residue, closed here): the boundness gate is
    /// `Env::find_any_cell`, not `find_bound_cell` -- the JVM's
    /// `Compiler.maybeResolveIn`/`Namespace.getMapping` return the `Var`
    /// for ANY genuine mapping, bound or not (measured,
    /// `compat/w-decl-fix-ns-machinery-oracle-transcript.txt`:
    /// `(ns-resolve tns 'unbound-pub)` is `#'oracle.tmp/unbound-pub` on the
    /// JVM, and privacy does not gate it either -- `(ns-resolve tns
    /// 'hidden-var)` on a `^:private` unbound var still returns the Var,
    /// only `bound?` on the result reports `false`). `find_any_cell`
    /// already excludes a compiler-speculative placeholder
    /// ([`VarCell::speculative`]) the same way it does for
    /// `resolve_binding_pairs`, so a genuinely never-interned symbol still
    /// resolves to `None` here exactly as before.
    pub fn try_resolve_var_cell(&self, sym: &Symbol) -> Option<Arc<VarCell>> {
        self.for_each_global_candidate(sym, |cand| self.globals.find_any_cell(cand))
    }

    /// `(the-ns x)` -- `x` already a namespace value, returned as-is; else
    /// a symbol naming a LOADED namespace, resolved to its (cached, see
    /// `ns_value`) value; else the measured error shape ("No namespace:
    /// NAME found", a plain `Exception` on the real JVM -- mova has no
    /// exception-class taxonomy, matching every other S3/S4 error).
    pub fn the_ns(&self, v: &Value) -> Result<Value, RjError> {
        if ns_value_name(v).is_some() {
            return Ok(v.clone());
        }
        let Some(name) = ns_name_arg(v) else {
            return Err(RjError::type_err(format!(
                "the-ns: expected a namespace or a symbol, got {}",
                v.type_name()
            )));
        };
        if lock_read(&self.namespaces).namespaces.contains_key(&name) {
            Ok(ns_value(&name))
        } else {
            Err(RjError::other(format!("No namespace: {name} found")))
        }
    }

    /// `(find-ns x)` -- `the_ns`'s lookup, `nil` (not an error) when `x`
    /// doesn't name a loaded namespace (measured).
    pub fn find_ns_value(&self, v: &Value) -> Option<Value> {
        let name = ns_name_arg(v)?;
        if lock_read(&self.namespaces).namespaces.contains_key(&name) {
            Some(ns_value(&name))
        } else {
            None
        }
    }

    /// `(create-ns sym)` -- creates `sym`'s namespace (empty tables) if it
    /// doesn't exist yet and returns its namespace value; unlike
    /// `in-ns`/`ns` it never moves `*ns*`/`current_ns` (measured:
    /// `(create-ns 'zzz4) (= *ns* (the-ns 'zzz4))` is `false`). Was
    /// unresolved in mova (mova/PLAN.md bug 1 follow-up).
    pub fn create_ns(&mut self, v: &Value) -> Result<Value, RjError> {
        let Some(name) = ns_name_arg(v) else {
            return Err(RjError::type_err(format!(
                "create-ns: expected a namespace or a symbol, got {}",
                v.type_name()
            )));
        };
        lock_write(&self.namespaces).namespaces.entry(name.clone()).or_default().declared = true;
        Ok(ns_value(&name))
    }

    /// `(all-ns)` -- every namespace this `Interp` currently has tables
    /// for (bootstrap-seeded builtins plus every `ns`/`require`d one).
    /// Real Clojure returns a `LazySeq`; mova returns a plain `List` of the
    /// same namespace values `the-ns`/`find-ns` hand back (a documented
    /// shape deviation -- `(count (all-ns))`/`(map ns-name (all-ns))`/
    /// `(some pred (all-ns))`, the realistic suite-corpus surface, all
    /// work identically either way).
    pub fn all_ns_values(&self) -> Vec<Value> {
        lock_read(&self.namespaces)
            .namespaces
            .keys()
            .map(ns_value)
            .collect()
    }

    /// Names of the namespaces a user would call namespaces, for tooling
    /// (nREPL `completions`). The registry also holds pseudo namespaces the
    /// natives live under (class names such as `System`, short spellings such
    /// as `string`): those are left out. A namespace counts when a program
    /// declared it (`ns`, `in-ns`, `create-ns`) or when it is a dotted name
    /// whose last segment is not a class name.
    pub fn user_visible_ns_names(&self) -> Vec<Str> {
        let reg = lock_read(&self.namespaces);
        reg.namespaces
            .iter()
            .filter(|(name, info)| {
                info.declared
                    || name.as_ref() == CORE_NS
                    || name.as_ref() == USER_NS
                    || (name.contains('.') && !name.rsplit('.').next().is_some_and(|l| l.starts_with(|c: char| c.is_uppercase())))
            })
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// Whether the bare `clojure.core` names resolve in `ns`. They do in every
    /// namespace except one that `in-ns` made and nothing has referred core
    /// into (the JVM rule: a bare `(in-ns 'foo)` has no `str`, `map`, ...).
    pub fn ns_sees_core(&self, ns: &Str) -> bool {
        // the hot path (every global resolution): no such namespace exists
        if NO_CORE_NS.load(std::sync::atomic::Ordering::Relaxed) == 0 {
            return true;
        }
        !lock_read(&self.namespaces).namespaces.get(ns).is_some_and(|i| i.no_core)
    }

    /// Whether a program already made `ns` (`ns`, `in-ns`, `create-ns`).
    pub fn ns_declared(&self, ns: &Str) -> bool {
        lock_read(&self.namespaces).namespaces.get(ns).is_some_and(|i| i.declared)
    }

    /// Sets or clears "clojure.core is not referred into `ns`".
    pub fn set_ns_no_core(&self, ns: &Str, on: bool) {
        let mut reg = lock_write(&self.namespaces);
        let info = reg.namespaces.entry(ns.clone()).or_default();
        if info.no_core != on {
            info.no_core = on;
            if on {
                NO_CORE_NS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            } else {
                NO_CORE_NS.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
        drop(reg);
        crate::env::bump_global_generation();
    }

    /// `(ns-resolve ns-or-sym sym)` -- resolves `sym` as though the
    /// CURRENT namespace were `ns-or-sym` for the duration of the lookup
    /// (measured: `(ns-resolve 'user 'str)` => `#'clojure.core/str`, via
    /// `user`'s own bare-core fallback, exactly like typing `str` inside
    /// `user`). Errors the same "No namespace: NAME found" shape as
    /// `the-ns` when `ns-or-sym` doesn't resolve; returns `nil` (not an
    /// error) when the namespace exists but `sym` doesn't resolve in it.
    pub fn ns_resolve_in(&mut self, ns_arg: &Value, sym: &Symbol) -> Result<Option<Arc<VarCell>>, RjError> {
        let target = self.the_ns(ns_arg)?;
        let target_name = ns_value_name(&target).expect("the_ns always returns a namespace value");
        let saved = self.current_ns.clone();
        self.current_ns = target_name;
        let result = self.try_resolve_var_cell(sym);
        self.current_ns = saved;
        Ok(result)
    }

    /// SPEC-W3: `try_resolve_var_cell` as `clojure.core/resolve` means it
    /// -- against the DYNAMIC `*ns*` rather than the lexical
    /// `current_ns`. `(defn resolve [sym] (ns-resolve *ns* sym))` is
    /// upstream's whole definition, so this is `ns_resolve_in` above with
    /// `*ns*` already chosen; see the `resolve` builtin's own comment
    /// (`builtins::nsfns`) for the two measured cases that separate the
    /// two namespaces.
    ///
    /// Everything else keeps reading LEXICALLY (`expand_alias`,
    /// `refer_source`, and so every ordinary symbol in compiled or
    /// tree-walked code): that is the JVM's compile-time resolution, and
    /// this fn is the runtime one.
    pub fn resolve_var_cell_in_dynamic_ns(&mut self, sym: &Symbol) -> Option<Arc<VarCell>> {
        let dynamic = self.dynamic_ns_name();
        if dynamic == self.current_ns {
            return self.try_resolve_var_cell(sym);
        }
        let saved = std::mem::replace(&mut self.current_ns, dynamic);
        let result = self.try_resolve_var_cell(sym);
        self.current_ns = saved;
        result
    }

    /// S4: one `:require`/callable-`require` libspec, already a `Value`
    /// (a bare symbol, or a `[ns & opts]` vector/list) -- shared by
    /// `eval_ns`'s `:require` clause (which converts each clause Form to a
    /// Value first, `crate::reader::form_to_value`) and the callable
    /// `require` native (`builtins::nsfns`), so there is exactly one
    /// libspec parser. Delegates to the pre-existing `require_ns`/
    /// `add_alias`/`add_refer` machinery -- unchanged from before S4.
    pub fn require_spec_value(&mut self, spec: &Value, span: Span) -> Result<(), RjError> {
        // `PVec` has no `[1..]` sub-slicing (unlike the `&[Form]` the
        // `(ns ...)` clause path used to hand `eval_require_spec` before
        // S4's refactor), so option lists are collected into owned `Vec`s
        // throughout -- cheap (a handful of `Value` clones per libspec).
        match spec {
            Value::Sym(s) => self.require_one_spec(s.name.clone(), &[], span),
            Value::Vector(items) | Value::List(items) => {
                let Some(Value::Sym(first)) = items.get(0) else {
                    return Err(RjError::other(":require: expected a namespace symbol").with_span(span));
                };
                let rest: Vec<Value> = items.iter().skip(1).cloned().collect();
                // C3f, measured against the oracle: a PREFIX LIST --
                // `(require '[clojure [set :as cs] [walk :as cw]])`, real
                // Clojure's `(use '(clojure zip [set :as s]))`-style
                // grouping -- is told apart from an ordinary `[lib &
                // opts]` libspec by whether the SECOND element is a
                // keyword: an ordinary libspec's second element (when
                // present at all) is always an option key (`:as`/
                // `:refer`/...), while a prefix list's second element is
                // always another sub-libspec (bare symbol or `[sub &
                // opts]` vector), never a keyword. That single test also
                // makes `(require '[clojure :as bad-alias [set :as
                // cs]])` take the ORDINARY path (second element `:as` IS
                // a keyword) and fail on its malformed, odd-length option
                // tail -- measured: real Clojure rejects that exact spec
                // too (a stray vector where an option VALUE was expected).
                // Each sub-libspec resolves to `prefix.sub` and is
                // processed through the very same `require_one_spec` an
                // ordinary top-level libspec uses, so `:as`/`:as-alias`/
                // `:refer`/no-load-for-as-alias-only all apply per SUB,
                // independently (measured: `[not.a.real.ns [foo :as-alias
                // foo] [bar :as-alias bar]]` aliases both without
                // loading either, even though neither sub-namespace nor
                // the prefix itself exists on the module path).
                let is_prefix_list =
                    matches!(rest.first(), Some(Value::Sym(_)) | Some(Value::Vector(_)) | Some(Value::List(_)));
                if is_prefix_list {
                    for sub in &rest {
                        let (sub_name, sub_opts): (Str, Vec<Value>) = match sub {
                            Value::Sym(s) => (s.name.clone(), Vec::new()),
                            Value::Vector(sub_items) | Value::List(sub_items) => {
                                let Some(Value::Sym(s)) = sub_items.get(0) else {
                                    return Err(RjError::other(
                                        ":require: a prefix list entry must be a symbol or a [sub & opts] vector",
                                    )
                                    .with_span(span));
                                };
                                (s.name.clone(), sub_items.iter().skip(1).cloned().collect())
                            }
                            other => {
                                return Err(RjError::other(format!(
                                    ":require: a prefix list entry must be a symbol or a vector/list, got {}",
                                    other.type_name()
                                ))
                                .with_span(span))
                            }
                        };
                        let full_name = Str::from(format!("{}.{}", first.name, sub_name));
                        self.require_one_spec(full_name, &sub_opts, span)?;
                    }
                    Ok(())
                } else {
                    self.require_one_spec(first.name.clone(), &rest, span)
                }
            }
            other => Err(RjError::other(format!(
                ":require: expected a symbol or a [ns & opts] vector, got {}",
                other.type_name()
            ))
            .with_span(span)),
        }
    }

    /// C3f: the per-namespace body `require_spec_value` shared out of --
    /// one namespace name plus its (already-flattened) option list,
    /// whether that pair came directly off a top-level libspec or was
    /// synthesized from one entry of a prefix list. Everything below is
    /// unchanged behavior, just no longer duplicated per prefix-list
    /// entry.
    fn require_one_spec(&mut self, ns_name: Str, opts: &[Value], span: Span) -> Result<(), RjError> {
        if !opts.len().is_multiple_of(2) {
            return Err(RjError::other(format!(
                ":require: {ns_name}: option list must have an even number of forms"
            ))
            .with_span(span));
        }
        // S7 (tail wave), measured: `(require '[not.a.real.ns :as-alias
        // nra])` succeeds even though `not.a.real.ns` cannot be found on
        // the module path -- `:as-alias` alone records an alias WITHOUT
        // loading the namespace's file at all. `:as`/`:refer` (or no
        // options at all -- a bare `(require 'clojure.set)` always loads)
        // still load normally; a spec that combines `:as` and `:as-alias`
        // on the SAME namespace (`[clojure.set :as n1 :as-alias n2]`) DOES
        // load, because `:as` is present too. So: skip the load only when
        // every option pair present is `:as-alias`.
        let needs_load = opts.is_empty()
            || opts
                .chunks(2)
                .any(|pair| !matches!(pair.first(), Some(Value::Keyword(k)) if k.as_ref() == "as-alias"));
        if needs_load {
            self.require_ns(&ns_name, span)?;
        } else {
            // Alias-only: the namespace must still EXIST for `find-ns`/
            // `ns-name` (measured `(find-ns 'not.a.real.ns)` is non-`nil`
            // after an `:as-alias`-only require) -- declared, not loaded
            // (absent from `loaded`, so a later real `(require ns)` still
            // runs the file normally).
            lock_write(&self.namespaces).namespaces.entry(ns_name.clone()).or_default();
        }

        let mut i = 0;
        while i < opts.len() {
            let key = match &opts[i] {
                Value::Keyword(k) => Some(k.as_ref()),
                _ => None,
            };
            let value = opts.get(i + 1);
            match (key, value) {
                (Some("as"), Some(Value::Sym(alias))) | (Some("as-alias"), Some(Value::Sym(alias))) => {
                    self.add_alias(alias.name.clone(), ns_name.clone());
                }
                (Some("as"), Some(_)) => {
                    return Err(RjError::other(":as: expected an alias symbol").with_span(span));
                }
                // `:refer :all` -- every currently-interned name under the
                // required namespace, refer-tabled in one shot. Mirrors
                // `special_forms.rs`'s own `:refer :all` support for the
                // `(ns ...)` clause path (M2), including its `Env::
                // names_in_ns` "mova has no private vars yet" caveat.
                (Some("refer"), Some(Value::Keyword(k))) if k.as_ref() == "all" => {
                    for name in self.globals.names_in_ns(&ns_name) {
                        self.add_refer(name, ns_name.clone());
                    }
                }
                (Some("refer"), Some(Value::Vector(names)) | Some(Value::List(names))) => {
                    for n in names {
                        let Value::Sym(name_sym) = n else {
                            return Err(RjError::other(":refer: entries must be symbols").with_span(span));
                        };
                        self.add_refer(name_sym.name.clone(), ns_name.clone());
                    }
                }
                (Some("refer"), Some(_)) => {
                    return Err(RjError::other(
                        ":refer: expected :all or a vector/list of symbols",
                    )
                    .with_span(span));
                }
                // Anything else (`:reload`, `:refer-macros`, a stray flag)
                // is ignored, same as an unknown `ns` clause.
                _ => {}
            }
            i += 2;
        }
        Ok(())
    }

    /// S5: one `(:use ...)` libspec. Real Clojure's `use` is `require`
    /// plus `refer`, so that is literally what this is: load the
    /// namespace, then refer-table its names -- all of them for a bare
    /// symbol, the `:only` list where one is given, everything but the
    /// `:exclude` list where THAT is given. `:as` aliases too, which real
    /// `use` also accepts.
    ///
    /// TOLERANT, BUT NARROWLY SO -- this is the load-bearing detail, and it
    /// got MORE PRECISE in S6: a namespace that cannot be LOCATED on the
    /// module path (`find_ns_file` finds no backing file, and it isn't one
    /// of the builtins `seed_builtin_namespaces` pre-marks loaded, e.g.
    /// `clojure.string`) is SILENTLY SKIPPED rather than raising. Before
    /// S5, `(:use ...)` was ignored wholesale as an unknown `ns` clause,
    /// which is what let the vendored suite's many `(:use clojure.test
    /// [clojure.test.generative :exclude (is)] ...)` forms load at all --
    /// several of those namespaces genuinely do not exist on mova's module
    /// path. Making `use` strict about THAT would turn every one of those
    /// files from "runs, scores honestly" into ":blocked", i.e. it would
    /// DESTROY signal to gain nothing. Skipping keeps the exact pre-S5
    /// outcome for every unresolvable spec while letting the resolvable
    /// ones (`clojure.test-clojure.protocols.examples`, which
    /// `protocols.clj` gets its whole `ExampleProtocol`/`foo`/`bar`
    /// surface from) actually work.
    ///
    /// S6 fix: a namespace that IS found on the module path (or already
    /// loaded) but FAILS while loading -- a syntax error, a missing
    /// transitive dependency, a circular `:use`/`:require` cycle -- used to
    /// be swallowed by the exact same `is_err() { return }` that handles
    /// "not found", via `require_ns` folding both failure modes into one
    /// `Result`. That let a file silently continue with a `:use`d
    /// namespace only PARTIALLY defined (whatever ran before the error),
    /// instead of failing the way real Clojure does (measured: a `:use`d
    /// lib that exists but errors during load THROWS from the `ns` form,
    /// same as `:require`). The fix distinguishes the two cases up front
    /// -- `ns_loaded`/`find_ns_file` (both cheap, no I/O beyond a
    /// directory-entry check) decide "not found, skip" BEFORE calling
    /// `require_ns`, so by the time `require_ns` runs, any error it
    /// returns is a genuine load failure, not a location failure, and gets
    /// propagated via `?` -- exactly like `eval_require_spec` already does
    /// for `:require`.
    ///
    /// Note there is deliberately no split-brain risk with the suite
    /// runner's spliced `clojure.test` shim: the splice defines those
    /// same names in the test file's OWN namespace, which outranks the
    /// refer table in `candidate_order` -- so `(:use clojure.test)` now
    /// resolving for real refers exactly the name set the splice already
    /// shadows, a no-op by construction.
    pub fn use_spec_value(&mut self, spec: &Value, span: Span) -> Result<(), RjError> {
        let (ns_name, opts): (Str, Vec<Value>) = match spec {
            Value::Sym(s) => (s.name.clone(), Vec::new()),
            Value::Vector(items) | Value::List(items) => match items.get(0) {
                Some(Value::Sym(s)) => (s.name.clone(), items.iter().skip(1).cloned().collect()),
                _ => return Ok(()),
            },
            _ => return Ok(()),
        };
        // "Not found on the module path, and not a pre-loaded builtin" is
        // the ONLY case that stays silently skipped (see doc above). Once
        // either check passes, `require_ns` below is loading (or has
        // already loaded) a namespace that genuinely exists, so any error
        // it returns from here on is a real load failure, not a location
        // one -- propagate it.
        if !self.ns_loaded(&ns_name) && self.find_ns_file(&ns_name).is_none() {
            return Ok(()); // see doc above: unresolvable `:use` stays ignored
        }
        self.require_ns(&ns_name, span)?;

        // Option scan, same pair-wise shape as `require_spec_value`'s.
        let names_of = |v: Option<&Value>| -> Vec<Str> {
            match v {
                Some(Value::Vector(ns) | Value::List(ns)) => ns
                    .iter()
                    .filter_map(|n| match n {
                        Value::Sym(s) => Some(s.name.clone()),
                        _ => None,
                    })
                    .collect(),
                _ => Vec::new(),
            }
        };
        let mut only: Option<Vec<Str>> = None;
        let mut exclude: Vec<Str> = Vec::new();
        let mut i = 0;
        while i < opts.len() {
            let key = match &opts[i] {
                Value::Keyword(k) => Some(k.as_ref()),
                _ => None,
            };
            match (key, opts.get(i + 1)) {
                (Some("only"), v) => only = Some(names_of(v)),
                (Some("exclude"), v) => exclude = names_of(v),
                // C3f, measured: `(use '[clojure.walk :as-alias e1])`
                // registers the alias exactly like `:as` does -- unlike
                // `require`'s `:as-alias`, `use`'s ALWAYS loads regardless
                // (this fn's unconditional `require_ns` call above already
                // gives that; there is no alias-only skip-load branch
                // here to add).
                (Some("as"), Some(Value::Sym(alias))) | (Some("as-alias"), Some(Value::Sym(alias))) => {
                    self.add_alias(alias.name.clone(), ns_name.clone());
                }
                _ => {}
            }
            i += 2;
        }

        let referred = match only {
            Some(names) => names,
            None => self
                .globals
                .names_in_ns(&ns_name)
                .into_iter()
                .filter(|n| !exclude.contains(n))
                .collect(),
        };
        for name in referred {
            self.add_refer(name, ns_name.clone());
        }
        Ok(())
    }

    /// Full symbol resolution: the lexical frames of `env` (everything
    /// below the root), then the global order. This is what
    /// `eval_form_in` and every macro-ness test use, so "what does this
    /// symbol mean here" has exactly one answer.
    pub fn resolve_symbol(&self, env: &crate::env::Env, sym: &Symbol) -> Option<Value> {
        if let Some(v) = env.get_local(sym) {
            return Some(v);
        }
        // W4-SPECIAL: `&form` is real Clojure's implicit macro-arglist
        // param (see `Interp::macro_form_stack`'s own doc) -- checked here,
        // AFTER the ordinary lexical chain (so an actual local/global
        // named `&form` still wins, matching a real implicit param's
        // precedence) and BEFORE `lookup_global` (there is no genuine
        // `&form`/`clojure.core/&form` var to shadow). Only meaningful
        // while a macro's body is actually running (stack non-empty);
        // elsewhere `&form` is exactly as unresolved as it is in real
        // Clojure outside a macro.
        if sym.ns.is_none() && sym.name.as_ref() == "&form" {
            if let Some(v) = self.macro_form_stack.last() {
                return Some(v.clone());
            }
        }
        // kondo-wave: `&env` -- real Clojure's OTHER implicit macro-
        // arglist param, the compiler's local-bindings environment map.
        // Same "meaningful only while a macro's body is actually running"
        // gate as `&form` just above (reusing `macro_form_stack` as the
        // "inside a macro expansion" signal, rather than a second stack,
        // since both implicit params come and go together). UNLIKE
        // `&form`, this is an HONEST PARTIAL stand-in: real `&env` maps
        // each in-scope local symbol to compiler metadata (a
        // `LocalBinding`), which mova's tree-walker has no structured,
        // per-macro-call snapshot of to hand back -- this always answers
        // an EMPTY map, never the real locals. That is exactly enough for
        // every macro that only asks "am I expanding inside a `let`/`fn`
        // at all" via truthiness or a specific key lookup that's always
        // absent from a real top-level `&env` too -- e.g. edamame's
        // `deftime` macro (`(not (:ns &env))`): real JVM `&env` never has
        // an `:ns` key (that's a ClojureScript-analyzer-only key), so
        // `(:ns &env)` is unconditionally `nil` there regardless of what
        // locals are actually in scope, and an empty map answers exactly
        // the same `nil`. A macro that walks `(keys &env)` expecting the
        // real local set would see nothing here -- undetected until
        // something in scope actually does that.
        if sym.ns.is_none() && sym.name.as_ref() == "&env" && !self.macro_form_stack.is_empty() {
            return Some(Value::Map(PMap::new()));
        }
        self.lookup_global(sym)
    }

    /// W4-EVAL task 1: mirrors real Clojure's privacy gate on a
    /// namespace-QUALIFIED symbol reference (`clojure/lang/Compiler.java`'s
    /// `analyzeSymbol` -> `resolve(sym)` -> `resolveIn(currentNS(), sym,
    /// allowPrivate=false)`): `v.ns != currentNS() && !v.isPublic() &&
    /// !allowPrivate` throws `IllegalStateException("var: " + sym + " is
    /// not public")`. Measured on the oracle (`tests/clojure-suite/vendor/
    /// evaluation.clj`'s `SymbolResolution` deftest): a qualified read of a
    /// `^{:private true}` var from any OTHER namespace throws; the SAME var
    /// read unqualified or qualified from WITHIN its own namespace is fine
    /// (the `v.ns != currentNS()` guard never fires there); `#'other/priv`/
    /// `(var other/priv)` is ALSO fine because those resolve through
    /// `resolve_var_cell`/`eval_var`, which never calls this gate -- so
    /// this method is NEVER called from `(var ..)`/`#'`, in either tier.
    /// It IS called from every other place a qualified symbol gets
    /// resolved, in BOTH positions and BOTH tiers (W-VARS-PRIV): the
    /// tree-walker's ordinary symbol-VALUE read path
    /// (`eval::eval_form_in`'s `FormValue::Atom(Value::Sym(..))` arm) AND
    /// its call-head path (`eval::eval_list`, just before the head
    /// symbol's `resolve_symbol`); and the compiled tier's single
    /// resolver chokepoint (`compile::resolve::Resolver::resolve_symbol`,
    /// reached from both value and call position) plus its separate
    /// macro-detection branch in `compile_list` (which resolves the head
    /// through `Interp::resolve_symbol` directly and so needs its own
    /// call to this gate). The compiled-tier call sites never build the
    /// error themselves -- they `Bail` out of compiling the whole fn so
    /// the tree-walker re-runs it and raises the error from here, keeping
    /// exactly one wording. `refer` was also measured: it never
    /// even copies a private var's name into the referring ns's refer
    /// table (`builtins::nsfns::refer`'s own "is not public" check on an
    /// explicit `:only` name), so an unqualified read after `refer` fails
    /// as a plain "Unable to resolve symbol" instead of reaching this gate
    /// at all -- consistent with the oracle, where `refer` behaves the
    /// same way.
    ///
    /// Returns `Some(err)` only when the gate actually fires. `None`
    /// covers every other case on purpose: no bound cell yet (left for the
    /// ordinary "Unable to resolve symbol" fallback below this call),
    /// public, or same-namespace.
    ///
    /// W-VARS-PRIV follow-up: this resolves `sym` to a cell (one
    /// `for_each_global_candidate` walk) purely to CHECK it -- callers
    /// that also need the cell's VALUE right after (the tree-walker's
    /// read and call-head paths, previously) used to pay for a SECOND,
    /// separate walk immediately afterward via `resolve_symbol`, doubling
    /// the cost of every qualified reference (measured: ~8% extra user
    /// CPU on a qualified-call-heavy hot loop). `lookup_global_checked`
    /// below is the fused replacement those two runtime call sites now
    /// use; this fn remains, unchanged in shape, for the compiled tier's
    /// two compile-time-only bail sites, which only ever need the yes/no
    /// answer and never touch the value.
    pub(crate) fn check_qualified_private(&self, sym: &Symbol) -> Option<RjError> {
        if sym.ns.is_none() {
            return None;
        }
        let cell = self.try_resolve_var_cell(sym)?;
        self.private_var_violation(&cell)
    }

    /// The core privacy check on an ALREADY-RESOLVED cell -- factored out
    /// of `check_qualified_private` so it is the ONE place this logic and
    /// this error's wording/class/cause exist, shared by that fn and by
    /// `lookup_global_checked`'s fused walk. Never calls
    /// `try_resolve_var_cell`/`for_each_global_candidate` itself; every
    /// caller has already done that walk exactly once.
    fn private_var_violation(&self, cell: &Arc<VarCell>) -> Option<RjError> {
        if cell.name.ns.as_deref() == Some(self.current_ns.as_ref()) {
            return None;
        }
        // f4/ns: shared with `builtins::nsfns::refer`'s identical check and
        // `ns-publics`'s listing-time gate -- see `VarCell::is_private`'s doc.
        if cell.is_private() {
            // C3g/W3a's class-aware `catch`/`thrown?` machinery (see
            // `eval::special_forms::catch_class_matches`) means this must
            // be tagged, not just messaged, to satisfy `(thrown-with-
            // cause-msg? Compiler$CompilerException #"...is not public..."
            // ...)`: `.with_class` is what makes the shim's `(catch
            // Compiler$CompilerException e# ...)` clause fire at all.
            // `error_to_info_map` (special_forms.rs) then builds a REAL
            // `mk_compiler_exception` instance -- but only when
            // `arity_cause` is populated, so `.getMessage` on the caught
            // value is a genuine `Value::Inst` field read (`ex-message`'s
            // `(instance? Throwable e)` branch), giving a REAL regex match
            // against this message rather than an accidental message-
            // blind pass. The message here doubles as both the outer
            // exception's `.getMessage` AND the (measured) text real
            // Clojure's own cause carries -- a deliberate simplification:
            // the shim's `thrown-with-cause-msg?` branch only ever reads
            // the TOP caught exception's `ex-message` (documented in
            // `mova-test-shim.mova`: mova has no `.getCause`-walking
            // assert-expr), so real Clojure's outer "Syntax error
            // compiling..." wrapper text is never observed by anything in
            // this corpus -- putting the measured CAUSE text there
            // instead is what actually satisfies the regex, and
            // `mk_illegal_state_exception` below still gives `.getCause`
            // the right shape for any stricter FUTURE check.
            let msg = format!(
                "var: {} is not public",
                crate::printer::pr_str(&Value::Sym(cell.name.clone()))
            );
            let cause = crate::hostclass::mk_illegal_state_exception(msg.clone());
            Some(
                RjError::other(msg)
                    .with_class(crate::error::JvmClass::CompilerException)
                    .with_arity_cause(cause),
            )
        } else {
            None
        }
    }

    /// `lookup_global`, fused with the privacy gate into ONE
    /// `for_each_global_candidate` walk (W-VARS-PRIV follow-up). Resolves
    /// to the candidate's `VarCell` (same candidate ORDER `lookup_global`
    /// uses -- `find_bound_cell` and `get_exact` probe the identical
    /// chain, just returning the cell vs. its value), checks privacy on
    /// THAT cell when `sym` is qualified, then reads the value off the
    /// SAME cell -- no second walk, unlike calling
    /// `check_qualified_private` and `lookup_global`/`resolve_symbol`
    /// back to back. Unqualified symbols pay nothing extra beyond the one
    /// lookup `lookup_global` always needed: the privacy check is skipped
    /// outright (`sym.ns.is_some()` guards it), not merely made cheap.
    ///
    /// `Err` only when the gate fires (identical error to
    /// `check_qualified_private`); `Ok(None)` for "nothing resolved",
    /// exactly like `lookup_global`.
    pub(crate) fn lookup_global_checked(&self, sym: &Symbol) -> Result<Option<Value>, RjError> {
        let Some(cell) = self.for_each_global_candidate(sym, |cand| self.globals.find_bound_cell(cand)) else {
            return Ok(None);
        };
        if sym.ns.is_some() {
            if let Some(e) = self.private_var_violation(&cell) {
                return Err(e);
            }
        }
        Ok(cell.get())
    }

    /// `resolve_symbol`, privacy-checked: same lexical-frame ->
    /// `&form` -> global order, but the global half goes through
    /// `lookup_global_checked` instead of `lookup_global`, so a qualified
    /// reference to another ns's private var surfaces as `Err` here
    /// instead of silently resolving. This is what `eval::eval_form_in`'s
    /// symbol-VALUE arm and `eval::eval_list`'s call-head resolution both
    /// use now (W-VARS-PRIV follow-up) -- `resolve_symbol` itself is
    /// UNCHANGED and still used everywhere a privacy-blind resolve is
    /// correct (e.g. the compiled tier's macro-detection probe, which
    /// gates privacy itself, separately, at compile time).
    pub fn resolve_symbol_checked(&self, env: &crate::env::Env, sym: &Symbol) -> Result<Option<Value>, RjError> {
        if let Some(v) = env.get_local(sym) {
            return Ok(Some(v));
        }
        // See `resolve_symbol`'s identical `&form` step for why this is
        // safe ahead of the global lookup.
        if sym.ns.is_none() && sym.name.as_ref() == "&form" {
            if let Some(v) = self.macro_form_stack.last() {
                return Ok(Some(v.clone()));
            }
        }
        // See `resolve_symbol`'s identical `&env` step (kondo-wave) --
        // same empty-map partial stand-in, same gate.
        if sym.ns.is_none() && sym.name.as_ref() == "&env" && !self.macro_form_stack.is_empty() {
            return Ok(Some(Value::Map(PMap::new())));
        }
        self.lookup_global_checked(sym)
    }

    /// `q` expanded through the current namespace's `:as` table, or --
    /// S7 (tail wave), measured against the oracle: `(:import [java.util
    /// UUID]) (UUID/randomUUID)` resolves on the real JVM, an ordinary
    /// explicit-import case that was falling through to "Unable to
    /// resolve symbol: UUID/randomUUID" (`import` only ever bound the bare
    /// class VALUE under the short name, never taught qualified-symbol
    /// resolution to consult it) -- a class short name bound by `import`
    /// (`eval::types_forms::import_one` interns exactly the
    /// `Symbol{ns: current_ns, name: short}` cell this checks), expanded
    /// to that class's fully-qualified name -- or `q` itself when neither
    /// matches (already a full namespace/class name, or genuinely
    /// unknown).
    pub(crate) fn expand_alias(&self, q: &Str) -> Str {
        let reg = lock_read(&self.namespaces);
        if let Some(info) = reg.namespaces.get(&self.current_ns) {
            if let Some(full) = info.aliases.get(q) {
                return full.clone();
            }
        }
        // DESIGN-flow-namespace.md Part 1 point 3, precedence step (b): a
        // literal LOADED namespace named `q` (a user's own `(ns flow)`, a
        // completed `require`, or any namespace merely declared via
        // `:as-alias`) wins over the default table below -- checked here,
        // before that table, so the literal name is what `q` continues to
        // mean. `reg.loaded`, NOT `reg.namespaces.contains_key` -- a mere
        // registry entry (`or_default`'s side effect of ANY lookup that
        // touches a namespace, e.g. a transient `(in-ns 'flow)` that never
        // finishes loading anything) is not the same claim as an actually
        // loaded namespace. Before this fix, a bare `(in-ns 'flow)` typo
        // permanently shadowed the `flow` default alias program-wide --
        // the registry entry it created via `or_default` never goes away,
        // even after the caller immediately switches back out -- with no
        // real `clojure.core.async.flow` behind it to resolve anything.
        let is_literal_ns = reg.loaded.contains(q);
        drop(reg);
        if let Some(crate::value::Value::Class(c)) = self.globals.get_exact(&Symbol {
            ns: Some(self.current_ns.clone()),
            name: q.clone(),
        }) {
            if let crate::types::ClassVal::Builtin { name, .. } = c.as_ref() {
                return Str::from(*name);
            }
        }
        // Precedence step (c), last resort: the engine-owned default-alias
        // table (`DEFAULT_ALIASES`), skipped when step (b) already claimed
        // `q` as a real namespace name.
        if !is_literal_ns {
            if let Some(full) = default_alias_target(q) {
                return Str::from(full);
            }
        }
        q.clone()
    }

    /// S6/Blocker-1: true when `sym`'s head should dispatch through
    /// `eval_special`/`compile_special` -- bare, exactly like before, OR
    /// namespace-qualified with a namespace that (bare, or through the
    /// current ns's `:as` table) IS [`CORE_NS`].
    ///
    /// Special forms (`let`, `fn`, `if`, `do`, `def`, ...) are Rust-level
    /// structural dispatch, never entries in the global var table -- unlike
    /// an ordinary `core.mova`-bootstrapped macro (`when`, `->`, ...), which
    /// interns BARE (because `qualify_def` never qualifies inside
    /// `CORE_NS`) and is therefore already reachable through an alias via
    /// `for_each_global_candidate`'s trailing bare-name fallback. A
    /// structural special form has no such fallback: it is matched ONLY by
    /// this dispatch gate, so `(ns t (:require [clojure.core :as core]))
    /// (core/let [x 1] (core/inc x))` measured `core/inc` succeeding
    /// (bare-registered fn, found via fallback) while `core/let` failed
    /// with "Unable to resolve symbol" (no fallback exists for it) --
    /// exactly the asymmetry this closes. `(ns t (:require [clojure.core
    /// :as c])) (c/let [x 1] (c/inc x))` => `2`, matching the oracle.
    pub(crate) fn is_bare_or_core_alias(&self, sym: &Symbol) -> bool {
        match &sym.ns {
            None => true,
            Some(q) => q.as_ref() == CORE_NS || self.expand_alias(q).as_ref() == CORE_NS,
        }
    }

    /// The `(namespace, source-name)` `name` was `:refer`red from in the
    /// current namespace -- `source-name` differs from `name` only for a
    /// `:rename`d refer (see `NsInfo::refers`'s doc).
    fn refer_source(&self, name: &Str) -> Option<(Str, Str)> {
        let reg = lock_read(&self.namespaces);
        reg.namespaces
            .get(&self.current_ns)?
            .refers
            .get(name)
            .cloned()
    }

    /// `(ns-aliases ns)`'s data half: every `alias -> full-ns-name` pair
    /// recorded against `ns` (both `:as` and `:as-alias` land in the same
    /// table -- see `add_alias`'s doc). Empty (not an error) for a
    /// namespace with no entry yet.
    pub fn ns_aliases_of(&self, ns: &Str) -> Vec<(Str, Str)> {
        lock_read(&self.namespaces)
            .namespaces
            .get(ns)
            .map(|info| info.aliases.iter().map(|(a, f)| (a.clone(), f.clone())).collect())
            .unwrap_or_default()
    }

    /// f4/ns: `(ns-map ns)`'s refer half -- every `local -> (from,
    /// source_name)` entry `ns`'s own `:refer`/`refer` table currently
    /// holds (see [`NsInfo::refers`]'s doc for the `local == source_name`
    /// vs. a `:rename`d entry's differing pair). Empty (not an error) for a
    /// namespace with no entry yet. Deliberately does NOT also walk the
    /// IMPLICIT bare-core fallback `for_each_global_candidate` gives every
    /// non-core namespace for free (that fallback needs no table entry at
    /// all, so it has nothing to iterate) -- `ns-map`'s own doc flags that
    /// as a known, unmeasured-by-this-transcript gap rather than widening
    /// this accessor to fabricate one.
    pub fn ns_refers_of(&self, ns: &Str) -> Vec<(Str, (Str, Str))> {
        lock_read(&self.namespaces)
            .namespaces
            .get(ns)
            .map(|info| {
                info.refers
                    .iter()
                    .map(|(local, (from, source))| (local.clone(), (from.clone(), source.clone())))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Records `alias -> full` in the current namespace's `:as` table.
    ///
    /// field2/W-NS: "current" here is the DYNAMIC `*ns*`, not the lexical
    /// `current_ns` -- real Clojure's `alias`/`require :as` are ordinary
    /// RUNTIME functions that mutate `*ns*`'s alias map (`clojure.core/
    /// alias` is literally `(.addAlias *ns* ...)`), so `(binding [*ns* n]
    /// (require '[x :as y]))` aliases into `n`. Identical to the old
    /// behavior everywhere the two agree (every top-level `(ns ...)`/
    /// `require`, and every call from a body whose defining ns IS the
    /// current one); the split only shows up once a fn body runs with
    /// `*ns*` pointing somewhere else. READING the table
    /// (`expand_alias`/`refer_source`) stays LEXICAL, matching the JVM's
    /// compile-time symbol resolution.
    pub fn add_alias(&mut self, alias: Str, full: Str) {
        let cur = self.dynamic_ns_name();
        lock_write(&self.namespaces)
            .namespaces
            .entry(cur)
            .or_default()
            .aliases
            .insert(alias, full);
        // W3e2: the `:as` table is part of what syntax-quote resolution
        // reads (`expand_alias`), so a new alias invalidates cached
        // answers -- see `env::global_generation`.
        crate::env::bump_global_generation();
    }

    /// Records `name -> (from, name)` in the current namespace's `:refer`
    /// table -- the ordinary case, where the local and source names match.
    pub fn add_refer(&mut self, name: Str, from: Str) {
        self.add_refer_as(name.clone(), from, name);
    }

    /// W3f: records `local -> (from, source_name)` -- the general form
    /// `add_refer` is a thin `local == source_name` wrapper around, used
    /// directly by `(refer ns :rename {...})` (`builtins::nsfns::refer`)
    /// where the two names genuinely differ.
    ///
    /// field2/W-NS: targets the DYNAMIC `*ns*`, same reason and same
    /// measured shape as `add_alias`'s -- vendored `generators.clj` does
    /// `(doseq [ns nses] (binding [*ns* ns] (refer 'clojure.core)))`,
    /// which only means anything if `refer` writes into the BOUND
    /// namespace.
    pub fn add_refer_as(&mut self, local: Str, from: Str, source_name: Str) {
        let cur = self.dynamic_ns_name();
        lock_write(&self.namespaces)
            .namespaces
            .entry(cur)
            .or_default()
            .refers
            .insert(local, (from, source_name));
        // W3e2: the `:refer` table is the second candidate syntax-quote
        // resolution probes -- same invalidation reason as `add_alias`.
        crate::env::bump_global_generation();
    }

    /// Marks the namespaces the interpreter itself provides -- core, plus
    /// every namespace `register_all` interned a qualified builtin under
    /// (`clojure.string`, `string`, `flow`) -- as already loaded, so a
    /// `(:require [clojure.string :as str])` records its alias instead of
    /// failing to find a file that will never exist. Called once, at the
    /// end of the bootstrap.
    // See `Env::interned_namespaces`' own `#[allow(clippy::mutable_key_type)]`
    // doc comment: `Str`'s cache is irrelevant to its `Hash`/`Eq`, so the
    // lint's premise doesn't apply to a `HashSet<Str>` here either.
    #[allow(clippy::mutable_key_type)]
    pub(crate) fn seed_builtin_namespaces(&self) {
        let mut reg = lock_write(&self.namespaces);
        let mut provided = self.globals.interned_namespaces();
        provided.insert(Str::from(CORE_NS));
        for ns in provided {
            reg.namespaces.entry(ns.clone()).or_default();
            reg.loaded.insert(ns);
        }
    }

    /// field2/W-NS: records `ns` in the loaded set without loading
    /// anything -- what `clojure.core/ns`'s own trailing `(commute
    /// *loaded-libs* conj '<name>)` does. See `eval::special_forms::
    /// eval_ns`'s call site for the measured oracle transcript.
    pub(crate) fn mark_ns_loaded(&mut self, ns: Str) {
        lock_write(&self.namespaces).loaded.insert(ns);
    }

    /// True once `ns` has been loaded to completion (by any thread).
    pub fn ns_loaded(&self, ns: &Str) -> bool {
        lock_read(&self.namespaces).loaded.contains(ns)
    }

    /// Loads namespace `ns` from the module path unless it is already
    /// loaded. Idempotent; a cycle is a clear error rather than a hang.
    ///
    /// The file is evaluated with `current_ns` preset to `ns` (so a file
    /// without an `ns` form still defs into the namespace that was asked
    /// for) and with the interpreter's diagnostic source switched to it. On
    /// SUCCESS the caller's namespace and source are restored; on FAILURE
    /// the failing file's name and text are deliberately left in place, so
    /// the diagnostic the CLI renders points at the file the error is
    /// really in.
    pub fn require_ns(&mut self, ns: &Str, span: Span) -> Result<(), RjError> {
        if self.ns_loaded(ns) {
            return Ok(());
        }
        if let Some(pos) = self.loading.iter().position(|n| n == ns) {
            let mut chain: Vec<&str> = self.loading[pos..].iter().map(|n| n.as_ref()).collect();
            chain.push(ns.as_ref());
            return Err(RjError::other(format!(
                "circular namespace dependency: {}",
                chain.join(" -> ")
            ))
            .with_span(span)
            .with_stack(self.stack_snapshot(), self.source_id));
        }
        // SPEC-W1 task 2: DISK FIRST, embedded table second. A namespace
        // that exists on the module path always wins, so `--module-path`
        // can override anything shipped in the binary (the clojure-suite
        // runner depends on this: it materializes its own copy of the
        // very same test.check files and must keep scoring THAT copy).
        // See `crate::stdlib`.
        let (source_name, source) = match self.find_ns_file(ns) {
            Some(path) => {
                let text = std::fs::read_to_string(&path).map_err(|e| {
                    RjError::other(format!("couldn't read {}: {e}", path.display()))
                        .with_span(span)
                        .with_stack(self.stack_snapshot(), self.source_id)
                })?;
                (path.to_string_lossy().into_owned(), text)
            }
            None => match crate::stdlib::find_embedded(ns) {
                Some(m) => (m.file.to_string(), m.source.to_string()),
                // sci-shim/load-hook: neither on disk nor embedded -- give
                // an active `with-load-hook*` (see `builtins::nsfns`) a
                // chance to supply source before failing. This is what lets
                // the sci.core shim's `:load-fn` (hooks loading their own
                // `.clj-kondo/` code, custom linters loading from the
                // classpath) satisfy a plain `(require 'some.ns)` inside
                // evaluated code.
                None => match crate::builtins::nsfns::try_load_hook(self, ns)? {
                    Some((file, source)) => (file.to_string(), source.to_string()),
                    None => {
                        return Err(RjError::other(format!(
                            "could not locate namespace {ns} on the module path: no {} in [{}]",
                            ns_file_names(ns).join(" or "),
                            self.module_paths
                                .iter()
                                .map(|p| p.display().to_string())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ))
                        .with_span(span)
                        .with_stack(self.stack_snapshot(), self.source_id))
                    }
                },
            },
        };

        let prev_ns = self.current_ns.clone();
        // SPEC-PORT: the DYNAMIC `*ns*` is a separate question from the
        // lexical `current_ns` (see `dynamic_ns_name`'s doc), and a load
        // reached from INSIDE a running fn is exactly where they differ:
        // `apply_closure` has already parked `current_ns` on the callee's
        // DEFINING namespace, so restoring `*ns*` from it below left the
        // var reading that namespace instead of the caller's. Measured
        // fallout: `clojure.spec.gen.alpha/dynaload` calls `require` in a
        // fn body, and every `::keyword` the CALLER read after its first
        // generator call auto-resolved into `clojure.spec.gen.alpha`
        // (`(s/gen ::foo ..)` => "Unable to resolve spec:
        // :clojure.spec.gen.alpha/foo"). Real Clojure's `Compiler.load`
        // push/pops `CURRENT_NS` as a thread binding, restoring exactly
        // this value. At top level the two coincide, so the ordinary
        // multi-`:require` case (the S6/Blocker-2 note below) is
        // unchanged.
        let prev_dyn_ns = self.dynamic_ns_name();
        let prev_source_name = self.source_name.clone();
        let prev_source = self.source.clone();
        // field5/W-SPAN: kept in lock-step with source_name/source below --
        // an eval_str nested inside this one (about to happen a few lines
        // down) re-interns source_id for the CHILD buffer; without saving
        // and restoring it here too, source_id would keep pointing at the
        // required file even after source_name/source are restored to the
        // caller's, corrupting every span recorded by the caller's OWN
        // fns for the rest of this load.
        let prev_source_id = self.source_id;
        // field2/W-NS: a loaded file's forms are TOP-LEVEL forms, whichever
        // call happened to trigger the load (a `require` reached from
        // inside a running fn body is still a fresh compilation unit on the
        // JVM). Reset for the load, restored below, so the file's own
        // `(ns ...)`/`(in-ns ...)` take `switch_ns`'s depth-0 branch. See
        // `Interp::closure_depth`'s field doc.
        let prev_depth = std::mem::replace(&mut self.closure_depth, 0);
        self.loading.push(ns.clone());
        self.set_current_ns(ns.clone());
        // `.cljc` files (S5 / reader conditionals) get `#?`/`#?@` dispatch
        // turned ON for this one load, matching real Clojure's own
        // `.clj`-vs-`.cljc` split; every other extension (`.mova`, and
        // since SPEC-W1 task 1 `.clj`) keeps the default OFF, same as
        // `eval_str`'s doc comment explains. Keyed on the NAME, so an
        // embedded module (whose name is its upstream file name, see
        // `crate::stdlib`) gets exactly the same treatment as the same
        // bytes read off disk.
        let is_cljc = source_name.ends_with(".cljc");
        // PERF-PROBE (MOVA_LOAD_TRACE): per-namespace time + RSS delta.
        // `ns_enter` returns `None` (no-op) unless the env var is set.
        let trace_span = crate::load_trace::ns_enter(ns.as_ref(), self.loading.len() - 1, source.len());
        // The JVM's `load` binds these around a file, so a library may `set!` them.
        let load_bound: Vec<_> = ["*warn-on-reflection*", "*unchecked-math*"]
            .iter()
            .filter_map(|n| self.globals.find_any_cell(&Symbol::simple(*n)))
            .collect();
        for c in &load_bound {
            c.push_binding(c.current_binding().or_else(|| c.raw_root()).unwrap_or(Value::Nil));
        }
        let result = if is_cljc {
            self.eval_str_allow_read_cond(&source_name, &source)
        } else {
            self.eval_str(&source_name, &source)
        };
        for c in load_bound.iter().rev() {
            c.pop_binding();
        }
        crate::load_trace::ns_exit(trace_span);
        self.loading.pop();
        result?;
        lock_write(&self.namespaces).loaded.insert(ns.clone());
        // S6/Blocker-2: restore THROUGH `set_current_ns`, not a bare field
        // write -- `set_current_ns(ns.clone())` a few lines up ALSO
        // refreshed the global `*ns*` var to the child namespace being
        // loaded, and a bare `self.current_ns = prev_ns` here put the
        // `current_ns` field back without undoing that, leaving `*ns*`
        // stuck on the LAST namespace `require`d (measured: a file with
        // multiple `:require`s left `*ns*` reading the final required
        // namespace instead of the file's own, once `eval`
        // (`Interp::dynamic_ns_name`) started consulting it instead of
        // `current_ns`).
        self.current_ns = prev_ns;
        self.set_dynamic_ns(prev_dyn_ns);
        self.closure_depth = prev_depth;
        self.source_name = prev_source_name;
        self.source = prev_source;
        self.source_id = prev_source_id;
        Ok(())
    }

    /// D5: `(load "pprint/utilities")` -- real Clojure's multi-file
    /// namespace loader, and the ONLY way `clojure.pprint` is spelled
    /// upstream (`pprint.clj` is a 51-line `ns` form followed by seven
    /// `(load "pprint/<part>")` calls, each part starting with `(in-ns
    /// 'clojure.pprint)`; vendoring it byte-identically means supporting
    /// that spelling rather than rewriting it).
    ///
    /// Path resolution, matching real Clojure's `load` docstring exactly:
    /// a path beginning with `/` is module-path-relative as written;
    /// anything else is relative to "the root directory for the current
    /// namespace" -- i.e. the current namespace's own path with its LAST
    /// segment dropped (`clojure.pprint` -> `clojure/`), so `(load
    /// "pprint/utilities")` resolves to `clojure/pprint/utilities`.
    /// Extension search (`.mova` then `.cljc`) and the `.cljc`-only
    /// reader-conditional switch are shared with `require_ns` verbatim --
    /// `load` differs from `require` in exactly two ways: it takes a PATH
    /// not a namespace, and it does NOT consult or update the
    /// loaded-namespaces set (real `load` re-loads every call; that is the
    /// whole point of `require`'s memo living one level up).
    ///
    /// W4C-NS: `load` does NOT PROACTIVELY switch to a target namespace
    /// the way `require_ns` does (the loaded file inherits the caller's
    /// namespace and is free to `in-ns` itself elsewhere, which is
    /// precisely what pprint's parts do) -- but it still SAVE/RESTOREs
    /// `*ns*` around the whole load, exactly like `require_ns`, because
    /// this is a universal property of real Clojure's file-loading
    /// (`Compiler.load` binds `CURRENT_NS` as a thread-local around
    /// *every* load, `require`/`load`/`load-file`/the script entry point
    /// alike -- not something `require` adds on top). Measured directly
    /// (`compat/w4c-ns-load-restore-oracle-transcript.txt`): a two-file real-JVM
    /// probe where the loaded file does `(in-ns 'other)` internally shows
    /// the CALLER's `*ns*` is back to its own namespace once `(load
    /// "other")` returns, not left on `other`. A previous version of this
    /// fn had no such restore at all -- any loaded file (pprint's parts
    /// included, though none of them currently exploit this) that ended
    /// on a different `in-ns` than it started would leak that switch to
    /// the caller permanently, unlike real Clojure.
    pub fn load_path(&mut self, path: &str, span: Span) -> Result<(), RjError> {
        let rel = if let Some(stripped) = path.strip_prefix('/') {
            stripped.to_string()
        } else {
            let root = match self.current_ns.rfind('.') {
                Some(i) => format!("{}/", self.current_ns[..i].replace('.', "/").replace('-', "_")),
                None => String::new(),
            };
            format!("{root}{path}")
        };
        let rel = rel.replace('-', "_");
        let file = self
            .find_module_file(&path_file_names(&rel))
            .ok_or_else(|| {
                RjError::other(format!(
                    "load: could not locate {} on the module path [{}]",
                    path_file_names(&rel).join(" or "),
                    self.module_paths
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
                .with_span(span)
                .with_stack(self.stack_snapshot(), self.source_id)
            })?;
        let source = std::fs::read_to_string(&file).map_err(|e| {
            RjError::other(format!("couldn't read {}: {e}", file.display()))
                .with_span(span)
                .with_stack(self.stack_snapshot(), self.source_id)
        })?;
        let prev_ns = self.current_ns.clone();
        // SPEC-PORT: the DYNAMIC `*ns*` is a separate question from the
        // lexical `current_ns` (see `dynamic_ns_name`'s doc), and a load
        // reached from INSIDE a running fn is exactly where they differ:
        // `apply_closure` has already parked `current_ns` on the callee's
        // DEFINING namespace, so restoring `*ns*` from it below left the
        // var reading that namespace instead of the caller's. Measured
        // fallout: `clojure.spec.gen.alpha/dynaload` calls `require` in a
        // fn body, and every `::keyword` the CALLER read after its first
        // generator call auto-resolved into `clojure.spec.gen.alpha`
        // (`(s/gen ::foo ..)` => "Unable to resolve spec:
        // :clojure.spec.gen.alpha/foo"). Real Clojure's `Compiler.load`
        // push/pops `CURRENT_NS` as a thread binding, restoring exactly
        // this value. At top level the two coincide, so the ordinary
        // multi-`:require` case (the S6/Blocker-2 note below) is
        // unchanged.
        let prev_dyn_ns = self.dynamic_ns_name();
        let prev_source_name = self.source_name.clone();
        let prev_source = self.source.clone();
        // field5/W-SPAN: same lock-step reasoning as `require_ns`'s -- see
        // there.
        let prev_source_id = self.source_id;
        // field2/W-NS: same compilation-unit reset as `require_ns`'s -- see
        // there. Load-bearing here specifically: every vendored
        // `clojure.pprint` part-file opens with a top-level `(in-ns
        // 'clojure.pprint)`.
        let prev_depth = std::mem::replace(&mut self.closure_depth, 0);
        let is_cljc = file.extension().and_then(|e| e.to_str()) == Some("cljc");
        let result = if is_cljc {
            self.eval_str_allow_read_cond(&file.to_string_lossy(), &source)
        } else {
            self.eval_str(&file.to_string_lossy(), &source)
        };
        // Same failure convention as `require_ns`: on error the failing
        // file's name/text/ns stay installed so the rendered diagnostic
        // points into it (restore is skipped by the early `?` return,
        // exactly like `require_ns`'s own success-only restore).
        result?;
        self.current_ns = prev_ns;
        self.set_dynamic_ns(prev_dyn_ns);
        self.closure_depth = prev_depth;
        self.source_name = prev_source_name;
        self.source = prev_source;
        self.source_id = prev_source_id;
        Ok(())
    }

    /// The first `<module-path>/<a/b_c>.mova` that exists, module paths
    /// searched in order.
    fn find_ns_file(&self, ns: &str) -> Option<PathBuf> {
        self.find_module_file(&ns_file_names(ns))
    }

    /// The first of `names` that exists under any module path, module
    /// paths searched in order (shared by `find_ns_file` and `load_path`).
    fn find_module_file(&self, names: &[String]) -> Option<PathBuf> {
        for dir in &self.module_paths {
            for name in names {
                let candidate = dir.join(name);
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
        None
    }
}

/// Clojure's file mapping: dots are directory separators and hyphens become
/// underscores, so `oma.core.mode-line` is `oma/core/mode_line.mova`.
/// `.mova` is tried first (mova's native extension, the Mova override);
/// then `.clj`, then `.cljc` -- SPEC-W1 task 1, matching real Clojure's own
/// preference order (`clojure.core/load`'s `root-resource` tries `.clj`
/// before `.cljc`) with mova's native extension in front.
///
/// `.clj` joined the list because a portable Clojure library is allowed to
/// mix extensions inside one namespace tree and several do: vendored
/// `clojure.test.check` is `.cljc` throughout EXCEPT
/// `clojure/test/check/random.clj`, which is plain `.clj` -- so `(require
/// 'clojure.test.check)` against an unmodified upstream checkout failed on
/// that one file until this list grew a `.clj` entry (measured against the
/// b8201bb release binary: the whole stack loads and `quick-check` runs
/// once `random` resolves).
///
/// SPEC-W6a made `clojure.test.check.random` a Rust-native veneer
/// (`crate::builtins::tcrandom`), so THAT namespace never reaches this
/// function any more -- its `require` is satisfied before file
/// resolution is consulted. The `.clj` entry is not vestigial, though:
/// it is what lets `clojure.walk` (required by `clojure.spec.alpha`'s
/// own `ns` form, embedded by `crate::stdlib`) keep its upstream file
/// name, and it remains the general rule for any mixed-extension
/// library a user puts on the module path.
///
/// Extension is also the reader-conditional switch: `.cljc` gets `#?`/`#?@`
/// dispatch, `.clj` and `.mova` do NOT -- see `require_ns`'s own
/// `is_cljc` check, which is a `== "cljc"` test precisely so a new
/// extension here defaults to conditionals-off, the same split real
/// Clojure draws between `.clj` and `.cljc`.
fn ns_file_names(ns: &str) -> Vec<String> {
    let stem = ns.replace('.', "/").replace('-', "_");
    path_file_names(&stem)
}

/// The extension search for an already-munged module-relative path stem --
/// shared verbatim by `ns_file_names` (namespace-derived stems) and
/// `load_path` (path-derived stems), so `require` and `load` can never
/// disagree about which extensions exist. See `ns_file_names`'s doc for
/// the order and the reader-conditional consequence.
fn path_file_names(stem: &str) -> Vec<String> {
    vec![
        format!("{stem}.mova"),
        format!("{stem}.clj"),
        format!("{stem}.cljc"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// W3e-4b/task-1b: `real_core_vars()` (present-vars union missing-vars
    /// from `compat/core-var-inventory.edn`) must contain real
    /// `clojure.core` names -- including ones NOT yet implemented in mova
    /// (`:missing-vars`) -- and must NOT contain mova-only core
    /// extensions, so `shadow_warning` warns on the former and stays
    /// silent on the latter.
    #[test]
    fn real_core_vars_has_real_names_not_mova_extensions() {
        let set = Interp::real_core_vars();
        for real in ["distinct?", "promise", "resolve", "*1", "with-redefs"] {
            assert!(set.contains(real), "{real} should be a real core var");
        }
        for mova_ext in [
            "exit",
            "rename",
            "file-exists?",
            "directory?",
            "special-doc-map",
        ] {
            assert!(
                !set.contains(mova_ext),
                "{mova_ext} is a mova-only extension, not real clojure.core"
            );
        }
    }

    #[test]
    fn ns_file_names_munge_dots_and_hyphens() {
        assert_eq!(
            ns_file_names("oma.core.mode-line"),
            vec![
                "oma/core/mode_line.mova".to_string(),
                "oma/core/mode_line.clj".to_string(),
                "oma/core/mode_line.cljc".to_string()
            ]
        );
    }

    /// SPEC-W1 task 1: `.mova` stays FIRST (the Mova override), `.clj`
    /// before `.cljc` (real Clojure's own order) -- see `ns_file_names`.
    #[test]
    fn ns_file_names_prefers_mova_then_clj_then_cljc() {
        let names = ns_file_names("clojure.test.check.random");
        assert_eq!(
            names,
            vec![
                "clojure/test/check/random.mova".to_string(),
                "clojure/test/check/random.clj".to_string(),
                "clojure/test/check/random.cljc".to_string()
            ]
        );
        // `load`'s path-derived stems share the identical extension list.
        assert_eq!(path_file_names("clojure/pprint/utilities"), {
            let mut v = Vec::new();
            for ext in ["mova", "clj", "cljc"] {
                v.push(format!("clojure/pprint/utilities.{ext}"));
            }
            v
        });
    }

    #[test]
    fn defs_qualify_outside_core_and_stay_bare_inside_it() {
        let mut interp = Interp::new();
        assert_eq!(interp.current_ns.as_ref(), USER_NS);
        assert_eq!(
            interp.qualify_def(&Symbol::simple("x")),
            Symbol {
                ns: Some(Str::from(USER_NS)),
                name: Str::from("x")
            }
        );
        interp.set_current_ns(Str::from(CORE_NS));
        assert_eq!(interp.qualify_def(&Symbol::simple("x")), Symbol::simple("x"));
    }

    #[test]
    fn candidate_order_is_current_ns_then_refer_then_bare() {
        let mut interp = Interp::new();
        interp.set_current_ns(Str::from("a.b"));
        interp.add_refer(Str::from("x"), Str::from("c.d"));
        let mut seen: Vec<String> = Vec::new();
        let found: Option<()> = interp.for_each_global_candidate(&Symbol::simple("x"), |s| {
            seen.push(crate::printer::pr_str(&Value::Sym(s.clone())));
            None
        });
        assert!(found.is_none());
        assert_eq!(seen, vec!["a.b/x", "c.d/x", "x"]);
    }

    #[test]
    fn qualified_candidates_expand_aliases_then_fall_back_to_the_bare_name() {
        let mut interp = Interp::new();
        interp.set_current_ns(Str::from("a.b"));
        interp.add_alias(Str::from("s"), Str::from("clojure.string"));
        let mut seen: Vec<String> = Vec::new();
        let sym = Symbol {
            ns: Some(Str::from("s")),
            name: Str::from("join"),
        };
        let _: Option<()> = interp.for_each_global_candidate(&sym, |s| {
            seen.push(crate::printer::pr_str(&Value::Sym(s.clone())));
            None
        });
        // D3 (2026-08-21): `s/join`, the UNEXPANDED spelling, now also
        // tried (after the expanded `clojure.string/join`, before the
        // bare fallback) -- see `for_each_global_candidate`'s own doc for
        // why: an imported short class name's static members are
        // registered under that short spelling, and `expand_alias`
        // translating it away must not make them unreachable.
        //
        // ns: restrict qualified->bare fallback to clojure.core spellings
        // (DESIGN-flow-namespace.md Part 1 point 4): the trailing bare
        // "join" candidate is GONE -- `s`'s expanded namespace is
        // `clojure.string`, not [`CORE_NS`], so the fallback that used to
        // let ANY qualified miss retry the bare name (the exact shape that
        // would have let `(a/merge ...)` silently resolve to
        // `clojure.core/merge`) no longer applies here. Real resolution of
        // `clojure.string/join` is completely unaffected: `join` is
        // dual-interned (`strings.rs`'s bare-plus-qualified loop), so the
        // FIRST candidate this walk tries, `clojure.string/join`, already
        // hits -- this test's `probe` returning `None` unconditionally is
        // what makes the (now-shorter) full candidate list observable at
        // all.
        assert_eq!(seen, vec!["clojure.string/join", "s/join"]);
    }

    /// Parses `src` as a single form and hands `require_spec_value` the
    /// resulting `Value` -- the same conversion `eval_ns`'s `:require`
    /// clause and the callable `require` native both go through
    /// (`crate::reader::form_to_value`).
    fn require_spec(interp: &mut Interp, src: &str) -> Result<(), RjError> {
        let forms = crate::reader::read_all(src).expect("test spec must parse");
        let spec = crate::reader::form_to_value(&forms[0]);
        interp.require_spec_value(&spec, Span { start: 0, end: 0 })
    }

    /// `ns_aliases_of` returns a `Vec<(Str, Str)>` rather than a map (see
    /// that fn's own doc); a linear find over it here avoids collecting
    /// into a `HashMap<Str, _>` just for a one-off lookup, which clippy's
    /// `mutable_key_type` lint (correctly) flags given `Str`'s interior
    /// mutability.
    fn find_alias<'a>(aliases: &'a [(Str, Str)], name: &str) -> Option<&'a str> {
        aliases.iter().find(|(a, _)| a.as_ref() == name).map(|(_, full)| full.as_ref())
    }

    /// C3f: a prefix list (`[prefix [sub1 opts...] [sub2 opts...]]`)
    /// aliases each `prefix.sub` per its own options, WITHOUT attempting
    /// to load anything, when every sub's options are `:as-alias`-only --
    /// measured against the oracle (`ns_libs.clj`'s `require-as-alias`).
    /// Before this fix `require_spec_value` mistook the whole vector for
    /// an ordinary `[ns & opts]` libspec and tried (and failed) to locate
    /// `not.a.real.ns` itself on the module path.
    #[test]
    fn require_prefix_list_aliases_each_sub_without_loading() {
        let mut interp = Interp::new();
        require_spec(
            &mut interp,
            "[not.a.real.ns.prefix-unit [foo :as-alias pfoo] [bar :as-alias pbar]]",
        )
        .expect("an :as-alias-only prefix list must not try to load anything");
        let aliases = interp.ns_aliases_of(&Str::from(USER_NS));
        assert_eq!(find_alias(&aliases, "pfoo"), Some("not.a.real.ns.prefix-unit.foo"));
        assert_eq!(find_alias(&aliases, "pbar"), Some("not.a.real.ns.prefix-unit.bar"));
        // Declared (for `find-ns`), not loaded.
        assert!(!interp.ns_loaded(&Str::from("not.a.real.ns.prefix-unit.foo")));
    }

    /// C3f: `(require '[ns :as-alias a])` -- the single-spec (non-prefix-
    /// list) form -- must surface `a` from `(ns-aliases *ns*)`, the exact
    /// shape `ns_libs.clj`'s `require-as-alias-then-load-later` checks via
    /// `(contains? (ns-aliases *ns*) 'alias-now)`.
    #[test]
    fn require_as_alias_single_spec_surfaces_in_ns_aliases() {
        let mut interp = Interp::new();
        require_spec(&mut interp, "[not.a.real.ns.solo-unit :as-alias sole]")
            .expect(":as-alias alone must not try to load");
        let aliases = interp.ns_aliases_of(&Str::from(USER_NS));
        assert_eq!(find_alias(&aliases, "sole"), Some("not.a.real.ns.solo-unit"));
    }

    /// C3f: `[clojure :as bad-alias [set :as cs]]` is NOT a prefix list --
    /// its second element (`:as`) is a keyword, so it takes the ordinary
    /// `[lib & opts]` path (measured against the oracle: real Clojure
    /// rejects this exact spec too, for the same reason -- an odd-length,
    /// malformed option tail). This must error, not silently no-op.
    #[test]
    fn a_keyword_second_element_is_never_treated_as_a_prefix_list() {
        let mut interp = Interp::new();
        let err = require_spec(&mut interp, "[clojure :as bad-alias [set :as cs]]")
            .expect_err("a malformed option tail after :as must error, not silently succeed");
        assert!(err.message.contains("even number"), "message: {}", err.message);
    }
}

// ---- heap-image gate-1 accessors (src/image.rs) ----
pub(crate) fn img_dump(reg: &Namespaces) -> (Vec<(Str, NsInfo)>, Vec<Str>) {
    let g = crate::sync::lock_read(reg);
    let mut ns: Vec<(Str, NsInfo)> = g.namespaces.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    ns.sort_by(|a, b| (&*a.0).cmp(&*b.0));
    let mut loaded: Vec<Str> = g.loaded.iter().cloned().collect();
    loaded.sort_by(|a, b| (&**a).cmp(&**b));
    (ns, loaded)
}
pub(crate) fn img_load(reg: &Namespaces, ns: Vec<(Str, NsInfo)>, loaded: Vec<Str>) {
    let mut g = crate::sync::lock_write(reg);
    // the image does not store `declared`: every namespace it holds was made by a program
    g.namespaces = ns.into_iter().map(|(k, mut v)| { v.declared = true; (k, v) }).collect();
    g.loaded = loaded.into_iter().collect();
}
