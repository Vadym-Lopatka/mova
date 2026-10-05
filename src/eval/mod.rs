//! Evaluator: special forms, macroexpansion, function application, the
//! `recur` trampoline, and lazy-seq forcing. See ARCHITECTURE.md "Evaluator"
//! for the contract. Split into submodules to keep files manageable:
//!   - `special_forms` — def/fn/defmacro/let/if/do/loop/recur/try/throw/ns
//!   - `apply` — function application (closures, natives, callable colls)
//!   - `quasiquote` — quasiquote/unquote/unquote-splicing expansion

pub(crate) mod apply;
mod multi_forms;
pub(crate) mod quasiquote;
pub(crate) mod types_forms;
/// `pub(crate)` for the compiled-fn tier only: `compile::resolve`/`exec`
/// reuse this module's destructuring leaves (`map_pattern_lookup`,
/// `coerce_map_pattern_source`), its `try` catch-value builder
/// (`error_to_info_map`) and its `fn`-form parser (`parse_fn_like`) rather
/// than reimplementing them, so the two tiers cannot drift.
pub(crate) mod special_forms;

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use crate::builtins::map_probe;
use crate::env::Env;
use crate::error::{ErrorKind, JvmClass, RjError};
use crate::ns::{CORE_NS, USER_NS};
use crate::reader::{Form, FormValue, Span};
use crate::value::{Keyword, PMap, Str, Symbol, Value};

/// PERF-PROBE (MOVA_LOAD_TRACE): a readable key for a top-level form's
/// exclusive-eval-time bucket -- `<ns>/<head> <defined-name>` when the form
/// is `(head name ...)` (covers `def`/`defn`/`defprotocol`/`ns`/etc.),
/// falling back to just `<ns>/<head>` or `<ns>/<form>` so nothing panics on
/// an unusual shape.
fn describe_top_level_form(form: &Form, ns: &Str) -> String {
    if let FormValue::List(items) = &form.value {
        let head = items.first().and_then(|f| match &f.value {
            FormValue::Atom(Value::Sym(s)) => Some(s.name.to_string()),
            _ => None,
        });
        let second = items.get(1).and_then(|f| match &f.value {
            FormValue::Atom(Value::Sym(s)) => Some(s.name.to_string()),
            FormValue::Atom(Value::Keyword(k)) => Some(format!(":{k}")),
            _ => None,
        });
        return match (head, second) {
            (Some(h), Some(s)) => format!("{ns}/{h} {s}"),
            (Some(h), None) => format!("{ns}/{h}"),
            (None, _) => format!("{ns}/<form>"),
        };
    }
    format!("{ns}/<form>")
}

// H2/macro-expansion-cache: process-wide, gated behind a STRUCTURAL EQUALITY
// check against the actual cloned call form (see `eval_list`'s macro branch
// for the full rationale). `items.as_ptr()` is used only as a HashMap key
// for O(1)-ish lookup, never trusted alone: `value_to_form` (reader.rs)
// stamps the SAME `span` onto every node of a macro's expansion output, so
// two DIFFERENT sibling forms produced by one expansion (or two calls whose
// addresses alias after a free) can share span+len+macro-identity -- a
// prior version of this cache kept fingerprint fields only (macro ptr, call
// length, span, source id) and that was NOT enough: it mis-hit on a
// `.getClass`-shaped interop call nested in expanded code, proving the
// collision is real, not theoretical. Comparing the FULL call form
// (structurally, via `forms_equal`, ignoring nothing) closes that hole: a
// hit requires the current call's actual head/args to be identical in
// content to what was cached, so an aliased address just becomes a miss
// (always safe) instead of a wrong answer.
struct MacroExpansionCacheEntry {
    macro_ptr: usize,
    def_epoch: u64,
    call_form: Vec<Form>,
    expansion: std::sync::Arc<Form>,
}

// `macro_ptr` alone is NOT enough to detect redefinition: once the OLD
// `Arc<Closure>` for a redefined macro is dropped (nothing else references
// it), the allocator is free to hand the exact same address to the NEW
// closure -- measured, not theoretical (a `(defmacro m [] 1)` /
// `(defmacro m [] 2)` redefinition test hit this on the very first attempt:
// same small `Closure` struct size, freed then immediately reallocated,
// same address, cache silently kept serving the OLD expansion). Bumped
// once per `defmacro` evaluation (`eval_defmacro`, special_forms.rs) --
// coarser than "only this macro's cache entries", but simple, and pays no
// measurable cost here: every `defmacro` in a clj-kondo-scale run happens
// during LOADING, before the hot linting phase ever populates this cache.
static MACRO_DEF_EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub(crate) fn bump_macro_def_epoch() {
    MACRO_DEF_EPOCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

fn macro_def_epoch() -> u64 {
    MACRO_DEF_EPOCH.load(std::sync::atomic::Ordering::Relaxed)
}

// Bound: a long-lived LSP session could otherwise grow this unboundedly
// across many distinct call sites/files. Once full, new entries are simply
// not inserted -- always a safe fallback (re-expand that one call site
// every time), never a correctness issue.
const MACRO_EXPANSION_CACHE_CAP: usize = 100_000;

fn macro_expansion_cache() -> &'static Mutex<std::collections::HashMap<usize, MacroExpansionCacheEntry>> {
    static CACHE: OnceLock<Mutex<std::collections::HashMap<usize, MacroExpansionCacheEntry>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

/// Structural equality of two `Form` trees, ignoring `span` (position is
/// irrelevant to "is this the same code") but NOT ignoring `meta` (reader
/// metadata like `^:dynamic` can change evaluation).
fn form_equal(a: &Form, b: &Form) -> bool {
    let meta_eq = match (&a.meta, &b.meta) {
        (Some(am), Some(bm)) => form_equal(am, bm),
        (None, None) => true,
        _ => false,
    };
    meta_eq && form_value_equal(&a.value, &b.value)
}

fn form_value_equal(a: &FormValue, b: &FormValue) -> bool {
    match (a, b) {
        (FormValue::Atom(av), FormValue::Atom(bv)) => av == bv,
        (FormValue::List(a), FormValue::List(b)) => forms_equal(a, b),
        (FormValue::Vector(a), FormValue::Vector(b)) => forms_equal(a, b),
        (FormValue::Set(a), FormValue::Set(b)) => forms_equal(a, b),
        (FormValue::Map(a), FormValue::Map(b)) => {
            a.len() == b.len()
                && a.iter()
                    .zip(b.iter())
                    .all(|((ak, av), (bk, bv))| form_equal(ak, bk) && form_equal(av, bv))
        }
        _ => false,
    }
}

fn forms_equal(a: &[Form], b: &[Form]) -> bool {
    a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| form_equal(x, y))
}

fn macro_expansion_cache_get(
    call_ptr: usize,
    macro_ptr: usize,
    call_form: &[Form],
) -> Option<std::sync::Arc<Form>> {
    let guard = macro_expansion_cache().lock().ok()?;
    let entry = guard.get(&call_ptr)?;
    if entry.macro_ptr == macro_ptr
        && entry.def_epoch == macro_def_epoch()
        && forms_equal(&entry.call_form, call_form)
    {
        Some(entry.expansion.clone())
    } else {
        None
    }
}

fn macro_expansion_cache_put(
    call_ptr: usize,
    macro_ptr: usize,
    call_form: Vec<Form>,
    expansion: std::sync::Arc<Form>,
) {
    if let Ok(mut guard) = macro_expansion_cache().lock() {
        if guard.len() < MACRO_EXPANSION_CACHE_CAP || guard.contains_key(&call_ptr) {
            guard.insert(
                call_ptr,
                MacroExpansionCacheEntry { macro_ptr, def_epoch: macro_def_epoch(), call_form, expansion },
            );
            MACRO_SCOPE.with(|s| {
                if let Some(v) = s.borrow_mut().as_mut() {
                    v.push(call_ptr);
                }
            });
        }
    }
}

thread_local! {
    /// Keys this thread added to the expansion cache since [`macro_scope_begin`].
    static MACRO_SCOPE: std::cell::RefCell<Option<Vec<usize>>> = const { std::cell::RefCell::new(None) };
}

/// nREPL memory, like `special_forms::template_scope_begin`: an expansion cached
/// for a REPL form is dead weight once the form's AST is gone (a `defn` held
/// 5-7 KB). Opened per top-level form; what the form added is dropped at the end.
pub(crate) fn macro_scope_begin() {
    MACRO_SCOPE.with(|s| *s.borrow_mut() = Some(Vec::new()));
}

pub(crate) fn macro_scope_end() {
    let keys = MACRO_SCOPE.with(|s| s.borrow_mut().take()).unwrap_or_default();
    if keys.is_empty() {
        return;
    }
    if let Ok(mut guard) = macro_expansion_cache().lock() {
        for k in keys {
            guard.remove(&k);
        }
    }
}

pub struct Interp {
    pub globals: Env,       // root env, builtins + core.mova + user defs
    /// K7b: `*unchecked-math*` cell cache keyed on (globals env, published root map).
    um_cache: std::cell::RefCell<Option<(Env, usize, Option<std::sync::Arc<crate::env::VarCell>>)>>,
    pub stack: Vec<Frame>,  // call frames for error traces
    /// SPEC-W5: the namespace that was current when each live frame was
    /// ENTERED -- i.e. the DEFINING namespace of that frame's CALLER, not
    /// its own. Index-parallel to [`Interp::stack`]: `ns_stack[i]` belongs
    /// to `stack[i]`, and the two are pushed and popped together by
    /// `eval::apply`'s three closure brackets.
    ///
    /// # Why it is a value each frame's CALLER owns
    ///
    /// It is the `Str` `apply_closure`'s `std::mem::replace(&mut current_ns,
    /// rc.ns.clone())` displaces -- moved in here instead of being parked in
    /// a Rust local until the call returns, and moved back out to
    /// `current_ns` at the pop. So the whole mechanism costs exactly the ONE
    /// `rc.ns.clone()` that bracket always made: not one extra atomic
    /// refcount operation per call.
    ///
    /// A frame's OWN namespace is therefore `ns_stack[i + 1]`, and the
    /// innermost frame's is `current_ns` itself. That is the same one-place
    /// shift `Frame::span` already needs (a frame's span is its CALL SITE, a
    /// position inside its caller), so one rule covers both -- see
    /// `builtins::reflect::callstack_native`, the only reader.
    ///
    /// # Why NOT a field on `Frame`
    ///
    /// Measured, and the reason this is a separate `Vec` rather than the
    /// obvious `Frame { name, span, ns }`: `Frame` is CLONED wholesale by
    /// `RjError::with_stack` -- every raised error copies the entire live
    /// stack -- so a second `Str` there is a second atomic refcount bump per
    /// frame per error, not per call. On `bench`'s `delays` metric
    /// (vendored `delays.clj`, which raises and catches constantly) that
    /// measured 2.61s -> 3.13s, **+19%**, reproducibly, interleaved against
    /// a same-tree build of the merge base. A pure 16-byte widening of
    /// `Frame` with a dummy `[u64; 2]` field cost NOTHING (2.52s), which is
    /// what isolated the `Str` -- and its clone -- as the whole of it.
    /// Nothing on the error path reads or needs the namespace, so it lives
    /// where only the live-stack reader pays for it.
    ///
    /// Per-thread-of-execution state, exactly like `stack`: `fork`/
    /// `snapshot` start it empty.
    pub(crate) ns_stack: Vec<Str>,
    pub source_name: Str,   // current file/"repl" for diagnostics
    /// A host's answer to "what is `*file*`": the `:file` a `def` records, when
    /// that differs from `source_name` (nREPL: `NO_SOURCE_PATH` for an eval with no `file`).
    pub def_file: Option<Str>,
    /// Set while an embedder/script entry point (`Engine::eval_named`) runs: the clojure.main
    /// vars that `set!` may bind on first use (see `special_forms::set_script_var`); popped by the entry point.
    pub script_frame: Option<Vec<std::sync::Arc<crate::env::VarCell>>>,
    pub source: Str,        // current source text (for miette snippets)
    /// field5/W-SPAN: the [`crate::source_registry`] id for `source_name`/
    /// `source`'s CURRENT buffer -- [`crate::source_registry::UNKNOWN_SOURCE`]
    /// (0) until the first `eval_str`/`eval_str_allow_read_cond` call
    /// interns one. Set alongside `source_name`/`source` at every site that
    /// touches those two fields (`new`, `fork`, `snapshot`, the three
    /// bootstrap reset-to-repl points, `eval_str`/`eval_str_allow_read_cond`,
    /// and the save/restore pairs in `ns::require_ns`/`ns`'s `load`/
    /// `builtins::reflect::load_string_native`) so the three fields can
    /// never drift apart. `compile::compile_fn`/`compile::resolve` read this
    /// at the moment a `FnTier::TreeWalk`/`LoopExplain` is constructed and
    /// stamp it onto the record, which is what lets `explain::at`/
    /// `builtins::meta::at_string` render against the ORIGINAL buffer later
    /// instead of whatever is current at render time -- see
    /// `source_registry`'s module doc for the measured bug this replaces.
    pub source_id: u32,
    /// W-ADX (item 4c): `true` for the duration of `load_core`/
    /// `load_core_async`/`load_core_flow`, `false` otherwise. Gates
    /// `MOVA_EXPLAIN=1`'s two stderr emitters (`compile::explain::record`,
    /// `compile::explain::report_top_level_loop`) so a user turning on
    /// `MOVA_EXPLAIN` sees only THEIR OWN code's compile-tier bails, not
    /// every core-bootstrap fn's noise printed before their script even
    /// starts running. Deliberately does NOT gate `compile_explain`'s data
    /// registry insert in `record` -- `(compile-explain some-core-fn)`
    /// must keep working on core fns, this only silences the unconditional
    /// eprintln.
    pub(crate) suppress_explain: bool,
    /// The namespace `def` interns into and unqualified symbols resolve
    /// against (v0.5 / R1, see `crate::ns`). Never `None`: `user` for a
    /// REPL/script, `clojure.core` while the bootstrap loads, whatever the
    /// running fn's `ns` is inside a call.
    pub current_ns: Str,
    /// Per-namespace alias/refer tables + the loaded set, SHARED with every
    /// forked `Interp` (`crate::ns`).
    pub namespaces: crate::ns::Namespaces,
    /// Directories searched, in order, for a required namespace's file.
    pub module_paths: Vec<PathBuf>,
    /// The `require` chain currently in progress on THIS interpreter, for
    /// cycle detection (`crate::ns::Interp::require_ns`).
    pub(crate) loading: Vec<Str>,
    /// D5: `(namespace, name)` pairs where a namespace has `def`d its OWN
    /// binding for a name mova implements as a special form but real
    /// Clojure implements as an ordinary `clojure.core` MACRO -- see
    /// `special_forms::is_shadowable_special`. Empty for essentially every
    /// program (the motivating case is the vendored `clojure.pprint`,
    /// which does `(:refer-clojure :exclude (deftype))` and then defines
    /// its own legacy `deftype` macro over `defstruct`), so `eval_list`'s
    /// per-call check is a single `is_empty` on the overwhelmingly common
    /// path. Namespace-scoped on purpose: one namespace shadowing
    /// `deftype` must not change what `deftype` means anywhere else.
    pub(crate) special_shadows: std::collections::HashSet<(Str, Str)>,
    /// SPEC-W3 (defect ledger D7): `(namespace, name)` pairs a namespace
    /// listed in `(:refer-clojure :exclude [...])`.
    ///
    /// mova reaches `clojure.core` by a FALLBACK, not by a mapping table:
    /// a bare symbol is tried as `<current-ns>/n`, then through the refer
    /// table, then bare (see `crate::ns`'s module doc). Real Clojure
    /// instead POPULATES each new namespace's mapping table with every
    /// core var, and `:exclude` omits the listed ones -- so "un-refer" is
    /// a deletion there and would have to be a negative set here. That
    /// half is deliberately NOT implemented; see the ledger entry.
    ///
    /// What IS honoured is the observable mova got wrong: a name the
    /// namespace explicitly excluded must not then warn `WARNING: <name>
    /// already refers to #'clojure.core/<name>` when the namespace defines
    /// it. Excluding is the programmer SAYING they mean to replace it, and
    /// real Clojure prints nothing for exactly that reason -- mova
    /// printed one line per excluded name, so loading `clojure.spec.alpha`
    /// or `clojure.test.check.generators` produced a wall of warnings the
    /// oracle does not produce.
    ///
    /// Same shape and same reasoning as `special_shadows` above: empty for
    /// essentially every program, so the check is an `is_empty` on the
    /// common path, and namespace-scoped so one namespace's exclusion says
    /// nothing about any other's.
    pub(crate) refer_clojure_excludes: std::collections::HashSet<(Str, Str)>,
    /// W3e2: memo for `quasiquote`'s syntax-quote symbol resolution --
    /// `(reading ns, template symbol) -> resolved symbol`, valid only while
    /// `sq_cache_gen` still equals `env::global_generation()`.
    ///
    /// Not an optimisation looking for a problem: resolution reads the
    /// global candidate order and the namespace tables, i.e. an
    /// `RwLock<HashMap>` read (two, when the refer table is consulted) PER
    /// TEMPLATE SYMBOL PER EXPANSION. Single-threaded that is free; with a
    /// hundred threads expanding the vendored test shim's `is` macro two
    /// million times it is not. Measured on `clojure.test-clojure.delays`
    /// (100 threads x 10 000 iterations x 2 assertions, the corpus's most
    /// macro-expansion-heavy file): 310s before syntax-quote
    /// qualification, 390s after it, 311s after this cache -- i.e. the
    /// cache is what makes the feature free again rather than a 26%
    /// tax on macro-heavy multithreaded code.
    ///
    /// Per-`Interp` (so per thread after `fork`) ON PURPOSE: a shared cache
    /// would reintroduce exactly the cross-core contention it exists to
    /// remove.
    pub(crate) sq_cache: std::collections::HashMap<(Str, Symbol), Symbol>,
    pub(crate) sq_cache_gen: u64,
    pub max_depth: usize,   // mova-level call-depth guard, see DEFAULT_MAX_CALL_DEPTH
    /// Per-interpreter compiled-fn-tier switch (v0.3 / S2). `true` is the
    /// normal setting; `false` makes every `fn` tree-walk. Exists next to
    /// the process-wide `MOVA_NO_COMPILE=1` kill switch because a
    /// differential test needs BOTH tiers live in one process -- see
    /// `Interp::with_compile_enabled` and `crate::compile`'s module doc.
    compile_enabled: bool,
    /// Lazy tier-up (v0.6): `false` (default) defers a `fn`'s compile
    /// attempt to its first real CALL (`CompileSlot::on_call`, from
    /// `apply_closure`); `true` compiles at `fn`-creation time, exactly
    /// like every tier before this one. Read fresh from `MOVA_EAGER_
    /// COMPILE=1` at construction, and overridable per-`Interp`
    /// (`set_eager_compile`) for tests/tools that assert def-time compile
    /// state without ever calling the fn.
    eager_compile: bool,
    /// Per-interpreter numeric-loop-specialization switch (v0.5). `true` is
    /// the normal setting; `false` makes every `loop` compile to the generic
    /// `Ir::Loop`, with the rest of the tier untouched. Same reason for
    /// existing as `compile_enabled`: the process-wide `MOVA_NO_NUMLOOP=1`
    /// kill switch cannot express "specialized and unspecialized side by
    /// side in one process", which is exactly what the randomized
    /// differential suite needs.
    numloop_enabled: bool,
    /// Per-interpreter lane-variant switch (W1, LATENCY-CAMPAIGN.md). `true`
    /// is the normal setting; `false` makes every `Ir::NumLoop` carry no
    /// `lane_variants` at all, with the tagged register machine (and
    /// everything else about the tier) untouched. Meaningless when
    /// `numloop_enabled` is false (no `NumLoop` is built, so there is
    /// nothing to attach a lane variant to). Same reason for existing as
    /// `numloop_enabled`: the process-wide `MOVA_NO_LANES=1` kill switch
    /// cannot express "lanes and no-lanes side by side in one process",
    /// which the randomized differential needs for its 4-way suite.
    lanes_enabled: bool,
    /// Per-interpreter superloop switch (W6, LATENCY-CAMPAIGN.md §7).
    /// `true` is the normal setting; `false` makes every lane variant carry
    /// no shape-specialized superloop, so the interpreted lane op lists W1
    /// landed run instead. Meaningless when `lanes_enabled` is false (no
    /// variant, so nothing to attach a shape to). Same reason for existing
    /// as `lanes_enabled`: the process-wide `MOVA_NO_SUPERLOOP=1` kill
    /// switch cannot express "superloops and interpreted lanes side by side
    /// in one process", which is what the randomized differential's fifth
    /// leg needs.
    superloop_enabled: bool,
    /// Per-interpreter last-use-analysis switch (v0.5, Perceus-lite phase
    /// 2). `true` is the normal setting; `false` makes `compile::lastuse`
    /// leave every slot read a cloning `Ir::LoadSlot`. Same reason for
    /// existing as `numloop_enabled`: the process-wide `MOVA_NO_LASTUSE=1`
    /// kill switch cannot express "taking and non-taking tiers side by side
    /// in one process", which is what the randomized differential needs.
    lastuse_enabled: bool,
    /// Per-interpreter consuming-convention switch (Perceus-lite phase 1).
    /// Initialised from `builtins::reuse::enabled()` -- i.e. from
    /// `MOVA_NO_REUSE` -- and then ANDed with what the caller asked for, so
    /// `apply_value_owned`'s hot check is one plain field read rather than
    /// the `OnceLock`'s atomic load, and the differential can still run a
    /// reusing and a non-reusing interpreter in one process.
    reuse_enabled: bool,
    /// Per-interpreter argument-handover switch (Perceus-lite phase 3).
    /// Initialised from `MOVA_NO_MOVEARGS` via
    /// `builtins::reuse::moveargs_enabled()`. `true` makes
    /// `apply_value_owned` MOVE an owned args `Vec` into a closure's slots /
    /// bindings; `false` restores phases 1-2's clone-out-of-the-buffer, which
    /// is what the same-binary A/B and the subprocess differential compare
    /// against. Read as a plain field on the hot call path.
    moveargs_enabled: bool,
    /// Embedding-fuel budget: an instruction-ish counter for bounding
    /// untrusted scripts, independent of every tier switch above. `None`
    /// (the default) means unlimited -- no fuel bookkeeping happens
    /// anywhere, and the overhead matrix in `bench/fuel-lcg.mova` /
    /// `bench/optimization-log.md` exists to prove that costs ~nothing.
    /// `Some(n)` decrements by one at every checked back-edge -- a
    /// tree-walked or compiled `loop`'s `recur`, a tree-walked or compiled
    /// fn's self-`recur`, and (either tier) a fn CALL's entry, all of which
    /// share the two choke points `apply_closure`/`apply_closure_buf`
    /// (`eval::apply`) for the call-entry case -- and once it reaches zero
    /// the NEXT checked back-edge raises `RjError::fuel_exhausted` instead
    /// of proceeding (see `Interp::tick_fuel`). `Ir::NumLoop`'s inner
    /// register loop is the one exception: it takes an independent
    /// plain-`u64` copy of the budget at loop entry (not this `Option`
    /// field, to keep its own hot path a `Copy` local with no `Interp`
    /// borrow) and writes the remainder back on exit -- see
    /// `compile::exec::exec_num_loop`'s doc comment for why and the
    /// measured per-iteration cost of doing so.
    ///
    /// A plain `Option<u64>` rather than the `Option<Cell<u64>>` an
    /// embedder-facing sketch might reach for: every checked site already
    /// holds `&mut Interp` (or `&mut Locals`/`&mut Interp` in the compiled
    /// tier), so there is no shared/aliased access to a live counter to
    /// justify `Cell`'s interior mutability -- it would only add a layer of
    /// indirection-free-but-still-there runtime cost for nothing.
    pub fuel: Option<u64>,
    /// Every `Value::Flow` handle `flow/create-flow` (`builtins::flow`) has
    /// ever built on THIS interpreter, tracked WEAKLY -- `crate::embed`'s
    /// `Engine::shutdown` is the one reader, and it upgrades/prunes as it
    /// walks, so a flow the script itself drops (never `def`'d, no live
    /// reference anywhere) is neither kept alive by this list nor left
    /// piling up dead entries forever.
    ///
    /// Sharing semantics are deliberate, not incidental (see `fork`'s and
    /// `snapshot`'s doc comments for the mechanical reasoning): `fork`
    /// SHARES this `Arc` (a `future*`/flow-proc sibling belongs to the same
    /// engine as its parent, and a flow it creates must be visible to that
    /// engine's `shutdown`), while `snapshot` starts a FRESH, EMPTY
    /// registry (a snapshot is an independent engine; flows created before
    /// the snapshot are the ORIGINAL's to stop, not the snapshot's).
    pub(crate) flow_registry: std::sync::Arc<std::sync::Mutex<Vec<std::sync::Weak<crate::value::FlowCell>>>>,
    /// S3: the protocol dispatch registry (`crate::types::Protocols`) --
    /// SHARED across `fork` like `namespaces`, so a protocol method keeps
    /// dispatching inside a `future`; snapshotted content-free on
    /// `isolated_clone` (each isolated engine re-runs its own
    /// `defprotocol`s).
    pub(crate) protocols: crate::types::Protocols,
    /// S5: interface method impls (`crate::types::Interfaces`) -- the
    /// `.method` interop dispatch table for `definterface`/host-interface
    /// impls written in a `defrecord`/`deftype` body. SHARED across `fork`
    /// and snapshotted content-free on `isolated_clone`, exactly like
    /// `protocols` directly above and for the identical reason.
    pub(crate) interfaces: crate::types::Interfaces,
    /// S4: the multimethod registry (`crate::multi::Multimethods`) --
    /// SHARED across `fork` like `protocols`, for the same reason (a
    /// `defmethod`/`derive` mutation done inside a `future*`-spawned
    /// sibling must be visible on the other side); snapshotted
    /// content-free on `isolated_clone` (each isolated engine re-runs its
    /// own `defmulti`s).
    pub(crate) multimethods: crate::multi::Multimethods,
    /// S5: the keyword intern registry (`crate::value::KeywordRegistry`)
    /// backing `find-keyword` -- SHARED across `fork` like `protocols`/
    /// `multimethods` (a keyword minted on either side of a `future*`
    /// boundary must be findable from both), fresh-empty on `snapshot`
    /// (an independent engine reinterns whatever its own script
    /// constructs). See `KeywordRegistry`'s own doc for registration
    /// coverage and why this must never be a Rust `static`.
    pub(crate) keywords: crate::value::KeywordRegistry,
    /// W4 allocation diet: a bounded per-interpreter free list of emptied
    /// `Vec<Value>` buffers, recycled between the three hot per-call
    /// allocations the attribution census named
    /// (bench/RESULTS-w4-alloc-attrib.md): call-argument vectors
    /// (`compile::exec::exec_args` -> `apply_value_owned`, which owns their
    /// death site), compiled slot frames (`compile::exec`'s
    /// `compiled_call_body!`), and vector-literal staging buffers
    /// (`compile::exec::exec_vector`). Never observable: only ever holds
    /// buffers whose `Value`s have been moved out or dropped (`put_buf`
    /// clears), and error paths simply drop instead of returning to the
    /// pool. Bounded by [`Interp::POOL_MAX_BUFS`] x [`Interp::POOL_MAX_CAP`]
    /// elements for the RSS crown. Per-interpreter, so no locks and no
    /// cross-thread traffic; deliberately NOT shared by `fork`/`snapshot`.
    pub(crate) buf_pool: Vec<Vec<Value>>,
    /// field1/W-EXPLAIN: a bounded "last compile decision by fn name"
    /// registry, read by the `(compile-explain f)` builtin
    /// (`builtins::meta`). Populated by `compile::compile_fn`'s one call
    /// site (`eval_fn_form`) EVERY time it runs, whether the fn ended up
    /// compiled or tree-walking -- never perturbing what got compiled,
    /// only recording why. Keyed by fn name because that is what
    /// `compile-explain`'s caller hands it (a `Value::Fn`'s `Closure::name`)
    /// and because attaching the record to the `Closure`/`CompiledClosure`
    /// itself would mean growing those types and every constructor of them
    /// for a diagnostics-only payload -- see `compile::explain`'s module doc
    /// for the fuller tradeoff. An anonymous fn (`name: None`) is never
    /// recorded here (nothing to key it by) but still gets an
    /// `MOVA_EXPLAIN=1` line. A name later reused by a DIFFERENT `fn`
    /// overwrites the earlier entry -- "last compile decision", not a
    /// history -- which is why this is documented as a v1 tradeoff rather
    /// than the final word: a real per-instance store would live on the
    /// `Closure` itself. Not shared by `fork`/`snapshot` (like `buf_pool`):
    /// it is a developer diagnostic, not program-observable state.
    pub(crate) compile_explain: std::collections::HashMap<Str, crate::compile::explain::FnExplain>,
    /// field4/W-LENS-1: this interpreter's cache in front of the GLOBAL
    /// regret-ledger site registry (`crate::lens::alloc_site`), keyed by the
    /// source position of the `fn`/`defmacro` form and the kind of decision.
    ///
    /// It exists for one reason: `eval_fn_form` runs `compile_fn` EVERY time
    /// a `fn` form is evaluated, so a `fn` literal inside a tree-walked loop
    /// would otherwise take the registry's global mutex once per iteration
    /// -- a shared RMW on a path the ledger is supposed to observe, not
    /// create. A hit here is a plain `HashMap` probe on a per-interpreter,
    /// single-threaded map. Not shared by `fork`/`snapshot` (like `buf_pool`
    /// and `compile_explain`): it is a pure cache, and the global registry
    /// dedups across interpreters anyway.
    pub(crate) lens_sites: std::collections::HashMap<(String, u8), u32>,
    /// W4-SPECIAL (errors.clj's `assert-arg-messages`): real Clojure's
    /// `defmacro` silently prepends `&form`/`&env` params to every arity
    /// (a macro's compiled fn has 2 MORE params than its written arglist,
    /// always populated by the compiler regardless of whether the macro
    /// body names them) -- `&form` is the whole unevaluated call form
    /// `(head arg...)`, the thing `core.clj`'s own `assert-args` reads
    /// `(first &form)` off of to name the actual (possibly `refer
    /// :rename`d) invocation symbol in its thrown message. Mova's
    /// `defmacro` has no such implicit-param machinery (`apply_macro`
    /// hands a macro closure only its real, declared args, exactly like
    /// an ordinary fn call -- see that fn's doc), and adding one would
    /// mean threading 2 synthetic params through arity selection/counting
    /// for every macro ever defined, for a feature exactly one vendored
    /// macro (`with-open`, via `assert-args`-shaped messages) actually
    /// reads.
    ///
    /// This is the narrower stand-in: a plain dynamic-extent stack,
    /// pushed by `apply_macro` with the raw call form just before running
    /// the macro's (always tree-walked, never compiled -- see
    /// `eval_defmacro`) body, popped on every exit path (`Ok` or `Err`).
    /// `resolve_symbol`'s bare-`&form` special case (see that fn) reads
    /// the top of this stack as a FALLBACK, after the ordinary lexical/
    /// global lookup chain finds nothing -- so an actual local named
    /// `&form` still shadows it, same precedence a real implicit param
    /// would have. Correctly nested for a macro that itself expands
    /// another macro referencing `&form` internally: each nested
    /// `apply_macro` call pushes/pops its OWN frame, so the inner
    /// macro's body sees ITS OWN call form and the outer macro's `&form`
    /// is restored once the inner call returns -- indistinguishable in
    /// effect from true per-invocation lexical scoping for this
    /// non-recursive-macro-generating-macro case (which is everything
    /// the vendored corpus exercises). Deliberately NOT shared by `fork`/
    /// `snapshot`: a macro-expansion-in-progress is per-thread-of-
    /// execution state, exactly like `stack` itself.
    pub(crate) macro_form_stack: Vec<Value>,
    /// W4C: whether the closure CURRENTLY EXECUTING (innermost live
    /// `apply_closure`/`apply_closure_buf` frame) was compiled while
    /// `*unchecked-math*` was truthy -- see `Closure::unchecked_math`'s doc
    /// for the full mechanism. `apply_closure`/`apply_closure_buf` swap
    /// this in and out exactly like `current_ns` (same unconditional
    /// save/restore shape, same reason: dynamic-extent-scoped to the call,
    /// not lexically scoped, not inherited by nested calls -- a checked
    /// callee called FROM an unchecked caller must still throw, matching
    /// the JVM: each compiled body carries its OWN baked-in checked/
    /// unchecked-ness, independent of its caller's). Consulted ONLY by
    /// `builtins::numbers::wrap_or_throw`, on the overflow path itself --
    /// never on the non-overflow fast path, and never by anything else.
    /// Per-thread-of-execution state like `stack`/`macro_form_stack`:
    /// `fork`/`snapshot` both reset it to `false` (a freshly spawned or
    /// snapshotted interpreter starts with no closure frame live).
    pub(crate) current_unchecked: bool,
    /// field2/W-NS: how many `apply_closure*` frames are live on THIS
    /// thread of execution -- i.e. "is a fn/macro BODY currently running,
    /// rather than a top-level form". Bumped by the same three
    /// `apply_closure`/`apply_closure_buf`/`apply_closure_lazy_rest`
    /// brackets that already swap `current_ns` (a `u32` increment right
    /// next to an existing `Arc<str>` clone -- free), and reset to 0 for
    /// the duration of every fresh COMPILATION UNIT (`ns::Interp::
    /// load_ns_file`/`load_path`/`require_ns`, `builtins::reflect::
    /// eval_native`), since the top-level forms of a loaded file, or the
    /// form handed to `eval`, are top-level no matter which call happened
    /// to trigger the load.
    ///
    /// Read by exactly one thing: [`Interp::switch_ns`], the `ns`/`in-ns`
    /// path. At depth 0 a namespace switch is a real, sequential
    /// load-order switch and moves BOTH the lexical resolution field
    /// (`current_ns`) and the dynamic `*ns*` var; at depth > 0 it moves
    /// only `*ns*`, because the body it interrupts was, on the real JVM,
    /// COMPILED as a whole in its defining namespace -- a mid-body switch
    /// there cannot un-resolve the free symbols that follow it. See
    /// `crate::ns::Interp::switch_ns`'s own doc for the measured case.
    pub(crate) closure_depth: u32,
    /// K1: slots of fast (threaded) calls -- see `jit::fast`.
    pub(crate) slot_stack: crate::jit::fast::SlotStack,
    /// K1: live fast calls; merged into every `stack` reader.
    pub(crate) fast_frames: Vec<crate::jit::fast::FastFrame>,
    /// P0c: per-interpreter interrupt flag (see `crate::interrupt`). Fresh per
    /// interp/fork/snapshot (a `future` child is NOT interrupted by its parent).
    pub intr: std::sync::Arc<crate::interrupt::Interrupt>,
    /// P0c: when true the native JIT shortcuts (no back-edge check) are off,
    /// exactly as when `fuel` is set.
    pub intr_armed: bool,
}

#[derive(Clone, Debug)]
pub struct Frame {
    pub name: Str,
    pub span: Span,
    /// field5/W-SPAN rider: `Interp::source_id` at the moment this frame was
    /// pushed (i.e. whatever buffer `span` -- the call site -- was read
    /// from). Lets `error::render` resolve THIS frame's line:col against
    /// its own originating source instead of whatever buffer happens to be
    /// current when the error is finally rendered, exactly like
    /// `compile::explain::FnTier::TreeWalk`'s `source_id` -- see
    /// `source_registry`'s module doc.
    pub source_id: u32,
}

/// Rust-recursion depth guard for closure application. Keeps us from
/// blowing the real Rust stack.
///
/// Deviation from ARCHITECTURE.md's illustrative "> 10_000" figure,
/// documented here: each mova-level call recurses through several Rust
/// frames (`eval_list` -> `eval_form_in` -> `apply_value` ->
/// `apply_closure` -> `run_closure_body` -> `eval_do_body` -> ...), and in
/// **debug** builds (no inlining/register allocation — exactly what `cargo
/// build`/`cargo test` produce) that chain costs tens of KB of real stack
/// per level. Empirically, on an 8 MiB thread stack (macOS/Linux's default
/// *main*-thread budget — the one `mova`'s CLI actually runs on) debug
/// builds abort with a genuine SIGSEGV/SIGABRT stack overflow somewhere
/// around depth 250-300 for deep non-tail recursion. 200 leaves headroom
/// below that measured cliff. Release builds (optimized, `--release`)
/// tolerate far deeper recursion for the same code; a future phase could
/// raise this by having the CLI entry point run on an explicitly larger
/// worker thread (a standard pattern for tree-walking interpreters) rather
/// than trusting the OS-default main-thread stack.
const DEFAULT_MAX_CALL_DEPTH: usize = 200;

/// `MOVA_EAGER_COMPILE=1`, read FRESH (not cached) at every `Interp`
/// construction -- cheap (once per interpreter, not once per call or per
/// `defn`) and, unlike a process-wide `OnceLock`-cached read, safe to rely
/// on even if some other test in the same binary also touches this env
/// var, since each `Interp` only ever reads it once, at its own
/// construction. Lazy tier-up's opt-out: `true` restores the pre-lazy
/// always-compile-at-creation behavior.
fn eager_compile_from_env() -> bool {
    std::env::var("MOVA_EAGER_COMPILE").is_ok_and(|v| v == "1")
}

static CORE_IMAGE_FAILED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

impl Interp {
    /// K1: `moveargs_enabled` for `jit::calls`.
    #[inline(always)]
    pub(crate) fn moveargs_on(&self) -> bool {
        self.moveargs_enabled
    }

    /// Upper bound on how many spare buffers [`Interp::buf_pool`] retains.
    const POOL_MAX_BUFS: usize = 32;
    /// Every buffer the pool hands out or takes back has capacity in
    /// [`POOL_MIN_CAP`, `POOL_MAX_CAP`]. The floor is the load-bearing
    /// half: a pool mixing capacities (first cut of W4) made a popped
    /// 1-slot buffer RE-ALLOC under an 8-slot frame -- an alloc + free +
    /// copy + bookkeeping where the pre-pool code paid one plain alloc,
    /// measured as a -8.6% flow-sink regression. With a uniform floor
    /// covering every hot user (call args <= arity, compiled frames,
    /// literal staging -- all <= 16 in practice), a warm pool never
    /// reallocates. The ceiling keeps a briefly-huge frame from pinning
    /// memory: 32 bufs x 64 slots x `size_of::<Value>` bounds the whole
    /// pool at ~100KB (RSS crown gate re-measured in
    /// bench/optimization-log.md's W4 entry).
    const POOL_MIN_CAP: usize = 16;
    const POOL_MAX_CAP: usize = 64;

    /// Kill switch (house rule: every experiment behind one):
    /// `MOVA_NO_BUFPOOL=1` makes `take_buf` hand out `Vec::new()` (callers
    /// `reserve` exactly what they need, restoring the pre-W4 exact-size
    /// allocation per call) and `put_buf` drop -- so a same-binary,
    /// same-process interleaved A/B isolates the pool. Read once per
    /// process, same pattern as `builtins::reuse`.
    #[inline]
    fn bufpool_enabled() -> bool {
        static DISABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        !*DISABLED.get_or_init(|| std::env::var_os("MOVA_NO_BUFPOOL").is_some())
    }

    /// Pops a spare (always empty, capacity >= [`Interp::POOL_MIN_CAP`])
    /// `Vec<Value>` off the pool, or makes a fresh one at the floor
    /// capacity. See [`Interp::buf_pool`].
    #[inline]
    pub(crate) fn take_buf(&mut self) -> Vec<Value> {
        match self.buf_pool.pop() {
            Some(b) => b,
            None if Self::bufpool_enabled() => Vec::with_capacity(Self::POOL_MIN_CAP),
            None => Vec::new(),
        }
    }

    /// Returns a dead buffer to the pool (clearing it -- dropping any
    /// leftover `Value`s exactly as the old direct `drop` did). Oversized,
    /// undersized, or surplus buffers are simply dropped, which keeps the
    /// pool's RSS bounded and makes "forgot to put back" (e.g. on error
    /// paths) a missed reuse, never a leak or a correctness issue.
    #[inline]
    pub(crate) fn put_buf(&mut self, mut buf: Vec<Value>) {
        if buf.capacity() >= Self::POOL_MIN_CAP
            && buf.capacity() <= Self::POOL_MAX_CAP
            && self.buf_pool.len() < Self::POOL_MAX_BUFS
            && Self::bufpool_enabled()
        {
            clear_values(&mut buf);
            self.buf_pool.push(buf);
        }
    }

    pub fn new() -> Self {
        Self::with_compile_enabled(true)
    }

    /// Builds an interpreter with the compiled-fn tier explicitly on or
    /// off. Off means every `fn` -- including `core/*.mova`'s, since the
    /// bootstrap runs below -- is tree-walked, which is what
    /// `tests/differential_test.rs` compares the compiled tier against.
    pub fn with_compile_enabled(compile_enabled: bool) -> Self {
        Self::with_tiers(compile_enabled, true)
    }

    /// Builds an interpreter with the compiled-fn tier and its numeric-loop
    /// specialization switched independently. `(true, false)` is the
    /// compiled tier with every `loop` left generic -- the middle leg of the
    /// three-way randomized differential in `tests/differential_test.rs`.
    /// `numloop_enabled` is meaningless when `compile_enabled` is false (no
    /// fn is compiled, so no loop is inspected).
    pub fn with_tiers(compile_enabled: bool, numloop_enabled: bool) -> Self {
        Self::with_all_tiers(compile_enabled, numloop_enabled, true, true)
    }

    /// `with_tiers` plus an explicit lane-variant switch (W1) -- the fourth
    /// leg of `tests/differential_test.rs`'s randomized NumLoop suite:
    /// `(true, true, false)` is the tagged-only `NumLoop` machine (no lane
    /// ever selected), `(true, true, true)` is the normal, fully-specialized
    /// setting.
    pub fn with_tiers_and_lanes(compile_enabled: bool, numloop_enabled: bool, lanes_enabled: bool) -> Self {
        Self::with_tiers_lanes_and_superloop(compile_enabled, numloop_enabled, lanes_enabled, true)
    }

    /// `with_tiers_and_lanes` plus an explicit superloop switch (W6) -- the
    /// FIFTH leg of `tests/differential_test.rs`'s randomized NumLoop suite:
    /// `(true, true, true, false)` is the interpreted lane machine W1
    /// landed, `(true, true, true, true)` is the normal setting, in which a
    /// variant whose shape `lanes::build_superloop` recognizes runs with its
    /// loop-carried state in Rust locals.
    pub fn with_tiers_lanes_and_superloop(
        compile_enabled: bool,
        numloop_enabled: bool,
        lanes_enabled: bool,
        superloop_enabled: bool,
    ) -> Self {
        Self::with_all_tiers_and_caps(
            compile_enabled,
            numloop_enabled,
            lanes_enabled,
            superloop_enabled,
            true,
            true,
            crate::builtins::Capabilities::ALL,
            DEFAULT_MAX_CALL_DEPTH,
        )
    }

    /// Every optional-fast-path switch at once, for the differential guards
    /// that must run several of them side by side in ONE process (the env
    /// vars are process-wide and read once, so they cannot express that).
    ///
    /// `lastuse_enabled` gates the emission of `Ir::LoadSlotTake`
    /// (`compile::lastuse`) and `reuse_enabled` gates the consuming
    /// calling convention (`builtins::reuse`) -- the two halves of
    /// Perceus-lite, which compose: the first decides whether the frame
    /// gives up its handle, the second what the native does with the handle
    /// it receives. Both are additionally ANDed with their process-wide
    /// kill switches, so `MOVA_NO_LASTUSE=1`/`MOVA_NO_REUSE=1` still win
    /// over anything asked for here. `lane_enabled` (W1) is `true` here --
    /// use `with_tiers_and_lanes` to turn it off independently.
    pub fn with_all_tiers(
        compile_enabled: bool,
        numloop_enabled: bool,
        lastuse_enabled: bool,
        reuse_enabled: bool,
    ) -> Self {
        Self::with_all_tiers_and_caps(
            compile_enabled,
            numloop_enabled,
            true,
            true,
            lastuse_enabled,
            reuse_enabled,
            crate::builtins::Capabilities::ALL,
            DEFAULT_MAX_CALL_DEPTH,
        )
    }

    /// `crate::embed::Engine`'s entry point: every optional-fast-path switch
    /// left at its normal (fully-on) setting, but the builtin-capability
    /// group and the call-depth guard both explicit. `max_depth: None`
    /// means [`DEFAULT_MAX_CALL_DEPTH`], same as every other constructor
    /// here.
    pub(crate) fn with_capabilities(caps: crate::builtins::Capabilities, max_depth: Option<usize>) -> Self {
        Self::with_all_tiers_and_caps(
            true,
            true,
            true,
            true,
            true,
            true,
            caps,
            max_depth.unwrap_or(DEFAULT_MAX_CALL_DEPTH),
        )
    }

    /// The one real constructor every public/crate-visible constructor
    /// above funnels into. `caps` decides which builtin groups
    /// `builtins::register_*` registers AND, in lockstep, which
    /// `core/*.mova` bootstrap files get loaded at all -- see
    /// `builtins::Capabilities`' doc for why a bootstrap file must be
    /// skipped outright rather than loaded-and-left-to-fail when its
    /// native group is missing.
    // One `bool` per independently-switchable fast path, and W6 pushed the
    // count past clippy's default: this is the single constructor funnel
    // (`fork`/`snapshot` are the only other `Interp { .. }` literals), so
    // collapsing them into a struct would only move the same list one
    // indirection away while breaking every caller.
    #[allow(clippy::too_many_arguments)]
    fn with_all_tiers_and_caps(
        compile_enabled: bool,
        numloop_enabled: bool,
        lanes_enabled: bool,
        superloop_enabled: bool,
        lastuse_enabled: bool,
        reuse_enabled: bool,
        caps: crate::builtins::Capabilities,
        max_depth: usize,
    ) -> Self {
        let mut interp = Interp {
            lastuse_enabled,
            reuse_enabled: reuse_enabled && crate::builtins::reuse::enabled(),
            moveargs_enabled: crate::builtins::reuse::moveargs_enabled(),
            globals: Env::new_root(),
            um_cache: Default::default(),
            stack: Vec::new(),
            ns_stack: Vec::new(),
            source_name: Str::from("repl"),
            def_file: None,
            script_frame: None,
            source: Str::from(""),
            source_id: crate::source_registry::UNKNOWN_SOURCE,
            suppress_explain: false,
            // The bootstrap runs IN core: its defs intern bare, which is
            // what makes them the set every other namespace can reach
            // without asking (`crate::ns`).
            current_ns: Str::from(CORE_NS),
            namespaces: crate::ns::Namespaces::default(),
            module_paths: Vec::new(),
            loading: Vec::new(),
            special_shadows: std::collections::HashSet::new(),
            refer_clojure_excludes: std::collections::HashSet::new(),
            sq_cache: std::collections::HashMap::new(),
            sq_cache_gen: crate::env::global_generation(),
            max_depth,
            compile_enabled,
            eager_compile: eager_compile_from_env(),
            numloop_enabled,
            lanes_enabled,
            superloop_enabled,
            fuel: None,
            intr: crate::interrupt::Interrupt::new(),
            // SPIKE-ONLY knob so the CLI can bench the armed (JIT-shortcuts-off) mode.
            intr_armed: std::env::var_os("MOVA_P0C_ARMED").is_some(),
            flow_registry: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            protocols: crate::types::Protocols::new(),
            interfaces: crate::types::Interfaces::new(),
            multimethods: crate::multi::Multimethods::new(),
            keywords: crate::value::KeywordRegistry::new(),
            buf_pool: Vec::new(),
            compile_explain: std::collections::HashMap::new(),
            lens_sites: std::collections::HashMap::new(),
            macro_form_stack: Vec::new(),
            current_unchecked: false,
            closure_depth: 0,
            slot_stack: crate::jit::fast::SlotStack::new(),
            fast_frames: Vec::new(),
        };
        let t_start = std::time::Instant::now();
        let bt = std::env::var_os("MOVA_BOOT_TRACE").is_some();
        let mut t_ = std::time::Instant::now();
        let mut lap = |name: &str, t_: &mut std::time::Instant| {
            if bt {
                eprintln!("[boot] {name}: {:.3} ms", t_.elapsed().as_secs_f64() * 1e3);
            }
            *t_ = std::time::Instant::now();
        };
        if caps.sys && caps.conc && caps.flow {
            crate::builtins::register_all(&mut interp);
        } else {
            crate::builtins::register_core(&mut interp);
            if caps.sys {
                crate::builtins::register_sys(&mut interp);
            }
            if caps.conc {
                crate::builtins::register_conc(&mut interp);
            }
            if caps.flow {
                crate::builtins::register_flow(&mut interp);
            }
        }
        lap("register natives", &mut t_);
        if std::env::var_os("MOVA_BOOT_NATIVES_ONLY").is_some() {
            return interp;
        }
        if caps.sys && caps.conc && caps.flow {
            if let Some(path) = crate::core_image::image_path().filter(|_| !CORE_IMAGE_FAILED.load(std::sync::atomic::Ordering::Relaxed)) {
                use std::sync::atomic::Ordering::Relaxed;
                use crate::core_image as ci;
                ci::NATIVES_US.store(t_start.elapsed().as_micros() as u64, Relaxed);
                let hdr = ci::header();
                let t = std::time::Instant::now();
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| crate::image::restore(&mut interp, &hdr, path)));
                match r {
                    Ok(Ok(true)) => {
                        interp.image_post_restore();
                        ci::RESTORE_US.store(t.elapsed().as_micros() as u64, Relaxed);
                        ci::OUTCOME.store(1, Relaxed);
                        if bt {
                            eprintln!("[boot] core image restore: {:.3} ms", t.elapsed().as_secs_f64() * 1e3);
                        }
                        return interp;
                    }
                    Ok(Ok(false)) => {
                        // missing / truncated / corrupt / other build: nothing was
                        // mutated. Boot normally, then save for next time.
                        let pre = crate::image::pre_index(&interp);
                        interp.boot_rest(caps, true);
                        let t = std::time::Instant::now();
                        if let Ok((bytes, rep)) = crate::image::encode_image(&interp, &pre, &hdr, 0) {
                            ci::SAVE_ENCODE_US.store(t.elapsed().as_micros() as u64, Relaxed);
                            if rep.unsupported.is_empty() {
                                let path = path.clone();
                                // file I/O off the boot thread
                                let _ = std::thread::Builder::new().name("mova-core-image-save".into()).spawn(move || {
                                    if crate::image::write_atomic(&path, &bytes).is_ok() {
                                        ci::prune_stale(&path);
                                    }
                                });
                            }
                        }
                        interp.image_post_restore();
                        ci::OUTCOME.store(2, Relaxed);
                        return interp;
                    }
                    _ => {
                        // restore failed half-way (or panicked): state is dirty,
                        // boot from scratch without the image.
                        CORE_IMAGE_FAILED.store(true, Relaxed);
                        ci::OUTCOME.store(3, Relaxed);
                        return Self::with_all_tiers_and_caps(compile_enabled, numloop_enabled, lanes_enabled, superloop_enabled, lastuse_enabled, reuse_enabled, caps, max_depth);
                    }
                }
            }
        }
        interp.boot_rest(caps, false);
        interp
    }

    /// P0b: everything after native registration (core.mova, async, flow, ...).
    #[doc(hidden)]
    pub fn boot_rest_all(&mut self) {
        self.boot_rest(crate::builtins::Capabilities::ALL, false);
    }

    /// P0b: like `boot_rest_all`, minus the two steps the image cannot carry
    /// (the `*err*` host stream and the native-macro swap); a restorer runs
    /// `image_post_restore` to redo them.
    #[doc(hidden)]
    pub fn boot_rest_for_image(&mut self) {
        self.boot_rest(crate::builtins::Capabilities::ALL, true);
    }

    /// P0b: the boot steps an image restore must redo (see above).
    #[doc(hidden)]
    pub fn image_post_restore(&mut self) {
        let err_stream = crate::hostclass::mk_output_stream(Box::new(std::io::stderr()));
        self.globals.set(crate::value::Symbol::simple("*err*"), err_stream);
        self.install_native_macros();
        self.set_current_ns(Str::from(USER_NS));
        self.source_name = Str::from("repl");
        self.source = Str::from("");
        self.source_id = crate::source_registry::UNKNOWN_SOURCE;
    }

    fn boot_rest(&mut self, caps: crate::builtins::Capabilities, for_image: bool) {
        let bt = std::env::var_os("MOVA_BOOT_TRACE").is_some();
        let mut t_ = std::time::Instant::now();
        let mut lap_ = |name: &str| {
            if bt {
                eprintln!("[boot] {name}: {:.3} ms", t_.elapsed().as_secs_f64() * 1e3);
            }
            t_ = std::time::Instant::now();
        };
        self.load_core();
        lap_("load_core (read+expand+eval+compile)");
        if caps.sys && !for_image {
            // clojure-lsp campaign: real Clojure's `*err*` root-binds to
            // `(PrintWriter. System/err)`, never `nil` -- `core.mova`'s
            // `(def ^:dynamic *err* nil)` (next to `*out*`) is the
            // per-var placeholder every dynamic def needs before this
            // runs, but a bare `nil` root left `(binding [*out* *err*]
            // ...)` doing nothing (`*out*` became `nil` too, which
            // `strings::out_write` treats as "real stdout", not "real
            // stderr") -- exactly the reported bug. Only when `caps.sys`
            // registered `builtins::io` (which owns `System/err`/
            // `HostKind::OutputStream`) is there a real stream to bind
            // to; a `caps.sys == false` (e.g. `Profile::Pure`) embed
            // keeps the old `nil` default unchanged.
            let err_stream = crate::hostclass::mk_output_stream(Box::new(std::io::stderr()));
            self.globals.set(crate::value::Symbol::simple("*err*"), err_stream);
        }
        lap_("err stream");
        if !for_image {
            self.install_native_macros();
        }
        lap_("native macros");
        self.install_clojure_repl_ns();
        lap_("repl ns");
        if caps.conc {
            self.load_core_async();
        }
        lap_("core.async");
        if caps.flow {
            self.require_core_flow();
        }
        lap_("flow");
        self.seed_builtin_namespaces();
        self.set_current_ns(Str::from(USER_NS));
        lap_("seed ns");
    }

    /// Builds a cheap sibling `Interp` for a `future*`-spawned thread (or
    /// any other place that needs an independent evaluator sharing the same
    /// world): shares `globals` (an `Arc`-backed `Env` clone -- defs made on
    /// one thread are visible to the other, same as real Clojure vars),
    /// clones the two `Str` diagnostics fields (cheap `Arc<str>` clones) and
    /// `max_depth`, but starts with a **fresh** `stack` -- call-stack frames
    /// are per-thread-of-execution, not shared state.
    ///
    /// Namespace state splits the same way vars do (`crate::ns`): the
    /// registry `Arc` is SHARED (a namespace loaded on either thread is
    /// loaded for both), while `current_ns`, `module_paths` and the
    /// in-progress `loading` chain are snapshotted -- the spawned thread
    /// starts out running the same namespace's code, and its `require`
    /// chain is its own.
    pub fn fork(&self) -> Interp {
        Interp {
            globals: self.globals.clone(),
            um_cache: Default::default(),
            stack: Vec::new(),
            ns_stack: Vec::new(),
            source_name: self.source_name.clone(),
            def_file: self.def_file.clone(),
            script_frame: None,
            source: self.source.clone(),
            source_id: self.source_id,
            suppress_explain: self.suppress_explain,
            current_ns: self.current_ns.clone(),
            namespaces: self.namespaces.clone(),
            module_paths: self.module_paths.clone(),
            loading: self.loading.clone(),
            special_shadows: self.special_shadows.clone(),
            refer_clojure_excludes: self.refer_clojure_excludes.clone(),
            sq_cache: std::collections::HashMap::new(),
            sq_cache_gen: crate::env::global_generation(),
            max_depth: self.max_depth,
            compile_enabled: self.compile_enabled,
            eager_compile: self.eager_compile,
            numloop_enabled: self.numloop_enabled,
            lanes_enabled: self.lanes_enabled,
            superloop_enabled: self.superloop_enabled,
            lastuse_enabled: self.lastuse_enabled,
            reuse_enabled: self.reuse_enabled,
            moveargs_enabled: self.moveargs_enabled,
            // Snapshotted, not shared: `fuel` is a plain `u64`, not an
            // atomic, so a `future*`-spawned thread gets its OWN independent
            // budget starting from whatever the parent had left at fork
            // time, rather than racing the parent for a shared counter. A
            // host that wants one combined budget across every OS thread a
            // script spawns needs a coarser mechanism (e.g. wall-clock
            // deadline or an external atomic checked from a native) -- out
            // of scope for this probe, which targets the common case of one
            // `Interp` running one script single-threaded.
            fuel: self.fuel,
            intr: crate::interrupt::Interrupt::new(),
            intr_armed: self.intr_armed,
            // SHARED, not snapshotted -- see `flow_registry`'s own field
            // doc: a `future*`-spawned sibling creating a flow must be
            // tracked by the same registry `Engine::shutdown` reads.
            flow_registry: self.flow_registry.clone(),
            // SHARED like `namespaces`: a protocol extended on either
            // side of a `future*` boundary must dispatch on both.
            protocols: self.protocols.clone(),
            // SHARED like `protocols`, same reason: an interface method
            // impl must dispatch on both sides of a `future*` boundary.
            interfaces: self.interfaces.clone(),
            // SHARED like `protocols`: a `defmethod`/`derive` done on
            // either side of a `future*` boundary must dispatch on both
            // -- includes the global hierarchy, which lives as an ordinary
            // var (`clojure.core/global-hierarchy`) in the ALSO-shared
            // `globals`, so no separate field is needed for it.
            multimethods: self.multimethods.clone(),
            // SHARED like `protocols`/`multimethods`: a keyword minted on
            // either side of a `future*` boundary must be findable from
            // both (see `KeywordRegistry`'s doc).
            keywords: self.keywords.clone(),
            buf_pool: Vec::new(),
            compile_explain: std::collections::HashMap::new(),
            lens_sites: std::collections::HashMap::new(),
            macro_form_stack: Vec::new(),
            current_unchecked: false,
            closure_depth: 0,
            slot_stack: crate::jit::fast::SlotStack::new(),
            fast_frames: Vec::new(),
        }
    }

    /// Builds an independent, isolated clone of this interpreter's whole
    /// world -- the "one engine per thread" embedding primitive: a `def`,
    /// redefinition, or `require` done on the clone afterwards is invisible
    /// to `self` and vice versa, and the reverse. This is FORK semantics,
    /// the deliberate opposite of `fork`'s SHARING semantics (`fork` is for
    /// a `future*`-spawned sibling that must see the same live world; this
    /// is for two engines that must stop being the same world at all).
    ///
    /// Mechanics: `globals` and `namespaces` are both deep-copied
    /// (`Env::snapshot`, `crate::ns::snapshot`) rather than `Arc`-cloned --
    /// every global's `Arc<VarCell>` is re-wrapped around a **new** `Arc`
    /// holding a copy of its current value, so `def`/`set!` on either side
    /// writes through a cell the other side no longer shares. `protocols`/
    /// `interfaces`/`multimethods`/`keywords` get the SAME deep-copy
    /// treatment (`Protocols::snapshot`, `Interfaces::snapshot`,
    /// `Multimethods::snapshot`, `KeywordRegistry::snapshot`) rather than
    /// `fork`'s `Arc`-clone: a `defprotocol`/`extend-type`/
    /// `extend-protocol`/`defmulti`/`defmethod` evaluated BEFORE the
    /// snapshot dispatches correctly on BOTH engines afterwards (the
    /// registry entry is copied, not lost), while the same evaluated AFTER
    /// the snapshot, on either side, is invisible to the other -- matching
    /// `globals`/`namespaces`' own isolation contract exactly, rather than
    /// leaving these four registries reset to empty (which would silently
    /// break dispatch for anything defined pre-snapshot even though the
    /// var/keyword itself resolves fine). Everything else follows `fork`'s
    /// lead: a fresh `stack`, cheap `Str`/`Vec` clones for the diagnostic
    /// and namespace-loading fields, and the per-interpreter tier switches
    /// copied verbatim.
    ///
    /// What is deliberately NOT deep-copied: any `Value` reachable from a
    /// global -- collections, closures, and in particular `Value::Atom`
    /// (`Arc<Mutex<(u64, Value)>>`). An atom captured inside a def'd value
    /// stays `Arc`-shared between `self` and the clone after this call, on
    /// purpose: that is what makes it the SAME atom on both sides, matching
    /// real Clojure's reference-identity semantics (`(identical? a a)` where
    /// `a` was visible before the snapshot). If the two engines should stop
    /// sharing mutable state entirely, the caller must avoid capturing
    /// shared atoms in the pre-snapshot world (or explicitly reset them
    /// after cloning) -- this method only forks the VAR TABLE, not the
    /// heap.
    ///
    /// Caller contract: like `Env::snapshot`, this assumes no other thread
    /// is concurrently `def`ing into `self.globals` while the snapshot
    /// walks it. That's automatically true for the intended embedding
    /// pattern (snapshot happens before workers start, i.e. before any
    /// `Arc` to this `Interp`'s globals has been handed to another thread);
    /// concurrent use is not memory-unsafe (each `VarCell`'s own lock still
    /// protects it) but a `def` racing the walk may or may not appear in
    /// the result.
    ///
    /// `fuel` is copied verbatim, same as `fork`: the clone gets an
    /// independent budget starting from whatever `self` had remaining, not
    /// a shared counter.
    ///
    /// `flow_registry` gets a FRESH, EMPTY registry -- the opposite of
    /// `fork`'s sharing. A snapshot is an independent engine (its own
    /// `globals`/`namespaces` fork, per this method's own doc above), so a
    /// flow created through `self` before the snapshot is `self`'s to stop;
    /// the snapshot only starts tracking flows created through IT from this
    /// point on. See `Engine::snapshot` (`crate::embed`) for the
    /// embedder-facing statement of this: `original.shutdown()` still stops
    /// pre-snapshot flows, `snapshot.shutdown()` sees none of them.
    pub fn snapshot(&self) -> Interp {
        Interp {
            globals: self.globals.snapshot(),
            um_cache: Default::default(),
            stack: Vec::new(),
            ns_stack: Vec::new(),
            source_name: self.source_name.clone(),
            def_file: self.def_file.clone(),
            script_frame: None,
            source: self.source.clone(),
            source_id: self.source_id,
            suppress_explain: self.suppress_explain,
            current_ns: self.current_ns.clone(),
            namespaces: crate::ns::snapshot(&self.namespaces),
            module_paths: self.module_paths.clone(),
            loading: self.loading.clone(),
            special_shadows: self.special_shadows.clone(),
            refer_clojure_excludes: self.refer_clojure_excludes.clone(),
            sq_cache: std::collections::HashMap::new(),
            sq_cache_gen: crate::env::global_generation(),
            max_depth: self.max_depth,
            compile_enabled: self.compile_enabled,
            eager_compile: self.eager_compile,
            numloop_enabled: self.numloop_enabled,
            lanes_enabled: self.lanes_enabled,
            superloop_enabled: self.superloop_enabled,
            lastuse_enabled: self.lastuse_enabled,
            reuse_enabled: self.reuse_enabled,
            moveargs_enabled: self.moveargs_enabled,
            fuel: self.fuel,
            intr: crate::interrupt::Interrupt::new(),
            intr_armed: self.intr_armed,
            flow_registry: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            // DEEP-COPIED, not fresh-empty and not `Arc`-shared like
            // `fork`: own `Arc<RwLock<..>>` around a copy of every
            // pre-snapshot `defprotocol`/`extend-type`/`extend-protocol`
            // (`Protocols::snapshot`/`ProtoDef::snapshot`), so a protocol
            // extended before the snapshot dispatches correctly on BOTH
            // engines afterwards, while an `extend` done on either side
            // AFTER the snapshot stays invisible to the other -- the same
            // isolation contract `globals`/`namespaces` already have.
            protocols: self.protocols.snapshot(),
            // Same contract as `protocols`, see `Interfaces::snapshot`.
            interfaces: self.interfaces.snapshot(),
            // Same contract as `protocols`, see `Multimethods::snapshot`
            // for why its per-`MultiDef` dispatch cache must NOT be
            // shared (process-global generation counter would otherwise
            // let a `defmethod` on one engine leak a cache hit into the
            // other).
            multimethods: self.multimethods.snapshot(),
            // Same contract, see `KeywordRegistry::snapshot`: a keyword
            // minted (via the `keyword` builtin or literal evaluation)
            // before the snapshot stays `find-keyword`-able on both
            // engines; one minted after stays engine-local.
            keywords: self.keywords.snapshot(),
            buf_pool: Vec::new(),
            compile_explain: std::collections::HashMap::new(),
            lens_sites: std::collections::HashMap::new(),
            macro_form_stack: Vec::new(),
            current_unchecked: false,
            closure_depth: 0,
            slot_stack: crate::jit::fast::SlotStack::new(),
            fast_frames: Vec::new(),
        }
    }

    /// field4/W-LENS-1: get-or-allocate the regret-ledger site id for one
    /// decision point, going to the global registry only the first time this
    /// interpreter sees it (see [`Interp::lens_sites`]).
    ///
    /// Returns `(site, first_time_here)`. `first_time_here` gates writing
    /// the site's REASON string: a repeat compile of the same source form
    /// keeps the first reason recorded for it. That is a deliberate v1
    /// tradeoff and the mirror image of `compile_explain`'s "last decision
    /// wins" -- the COUNT, which is what regret is measured in, is exact
    /// either way; only the prose could go stale, and only if the same form
    /// compiles differently against two different envs.
    pub(crate) fn lens_site_for(
        &mut self,
        kind: crate::lens::SiteKind,
        name: Option<&Str>,
        span: crate::reader::Span,
    ) -> (u32, bool) {
        // IDENTITY first, rendered position only as a fallback: see
        // `lens::alloc_site`'s doc for the measured reason a span-derived
        // key cannot be trusted here (`explain::at` renders against the
        // interpreter's CURRENT source, not the form's own provenance).
        let ident = match name {
            Some(n) => format!("{}/{}", self.current_ns, n),
            None => format!("{}:{}", self.source_name, span.start),
        };
        let key = (ident, kind as u8);
        if let Some(site) = self.lens_sites.get(&key) {
            return (*site, false);
        }
        let loc = format!("{}:{}", self.source_name, span.start);
        let site = crate::lens::alloc_site(kind, &key.0, name.map(|n| n.as_ref()), &loc);
        self.lens_sites.insert(key, site);
        (site, true)
    }

    /// Whether this interpreter compiles the `fn`s it evaluates (the
    /// process-wide `MOVA_NO_COMPILE` kill switch is consulted separately,
    /// in `compile::compile_fn`).
    pub fn compile_enabled(&self) -> bool {
        self.compile_enabled
    }

    /// Lazy tier-up (v0.6): `true` means every `fn` compiles at CREATION
    /// time (the pre-lazy behavior); `false` (the default) defers the
    /// first compile attempt to the fn's first real call. See
    /// `Closure::compiled`/`CompileSlot`/`eval_fn_form`.
    pub fn eager_compile(&self) -> bool {
        self.eager_compile
    }

    /// Per-`Interp` override for [`Self::eager_compile`] -- for
    /// tests/tools that assert def-time compile state (`rc.compiled.
    /// compiled().is_some()` right after `eval_str`, with the fn never
    /// called) without relying on the process-wide `MOVA_EAGER_COMPILE=1`
    /// env var, which -- like `MOVA_NO_COMPILE` -- is read once and meant
    /// to be set from the shell, not mutated mid-process by a test running
    /// alongside others that want the opposite setting.
    pub fn set_eager_compile(&mut self, v: bool) {
        self.eager_compile = v;
    }

    /// W4C: the one dynamic-var read behind `Closure::unchecked_math` --
    /// `*unchecked-math*`'s truthiness AT THIS MOMENT, i.e. at
    /// closure-creation time. `true` for either `true` or `:warn-on-boxed`
    /// (both are truthy `Value`s, and both wrap on the real JVM -- measured,
    /// `compat/w4-parse-unchecked-math-probe.clj`'s transcript uses
    /// `:warn-on-boxed` itself). Called from `eval_fn_form` and
    /// `compile::exec::make_closure` ONLY -- never on any per-call path.
    pub(crate) fn unchecked_math_active(&self) -> bool {
        // K7b: same map (never mutated in place) = same cell; skips the per-call hash probe.
        let Some(id) = self.globals.root_map_id() else {
            return self.globals.get(&crate::value::Symbol::simple("*unchecked-math*")).is_some_and(|v| v.truthy());
        };
        let mut c = self.um_cache.borrow_mut();
        let hit = matches!(&*c, Some((e, k, _)) if *k == id && Env::ptr_eq(e, &self.globals));
        if !hit {
            let cell = self.globals.root_cell(&crate::value::Symbol::simple("*unchecked-math*"));
            *c = Some((self.globals.clone(), id, cell));
        }
        c.as_ref().and_then(|(_, _, cell)| cell.as_ref()).and_then(|cell| cell.get()).is_some_and(|v| v.truthy())
    }

    /// W4C: whether the closure currently executing was compiled while
    /// `*unchecked-math*` was truthy -- see `current_unchecked`'s own field
    /// doc. Consulted ONLY by `builtins::numbers::wrap_or_throw`, on the
    /// overflow path.
    pub(crate) fn current_unchecked(&self) -> bool {
        self.current_unchecked
    }

    /// Whether this interpreter's compiled `loop`s may specialize to
    /// `Ir::NumLoop` (the process-wide `MOVA_NO_NUMLOOP` kill switch is
    /// consulted separately, in `compile::resolve`).
    pub fn numloop_enabled(&self) -> bool {
        self.numloop_enabled
    }

    /// Whether this interpreter's `Ir::NumLoop`s may carry lane variants
    /// (W1). The process-wide `MOVA_NO_LANES` kill switch is consulted
    /// separately, in `compile::resolve`.
    pub fn lanes_enabled(&self) -> bool {
        self.lanes_enabled
    }

    /// Whether this interpreter's lane variants may carry shape-specialized
    /// superloops (W6). The process-wide `MOVA_NO_SUPERLOOP` kill switch is
    /// consulted separately, in `compile::resolve`.
    pub fn superloop_enabled(&self) -> bool {
        self.superloop_enabled
    }

    /// Whether this interpreter's compiled fns may emit moving slot reads
    /// (the process-wide `MOVA_NO_LASTUSE` kill switch is consulted
    /// separately, in `compile::resolve`).
    pub fn lastuse_enabled(&self) -> bool {
        self.lastuse_enabled
    }

    /// Whether this interpreter routes whitelisted natives through their
    /// consuming entry point (`builtins::reuse`). Already ANDed with the
    /// process-wide `MOVA_NO_REUSE` switch at construction.
    #[inline]
    pub fn reuse_enabled(&self) -> bool {
        self.reuse_enabled
    }

    /// Decrements the fuel budget by one and errors once it is exhausted;
    /// a no-op (one `Option` discriminant check) when fuel is `None`. Called
    /// at every checked back-edge -- see `fuel`'s field doc for the full
    /// list of call sites and `Ir::NumLoop`'s own copy of this logic for why
    /// that one path doesn't call through here.
    ///
    /// Checks BEFORE decrementing, so `Some(n)` buys exactly `n` more
    /// checked back-edges: the `n`th tick succeeds and leaves `0` behind,
    /// the `(n+1)`th finds `0` and errors without going negative.
    ///
    /// The error-construction half is deliberately its OWN `#[cold]
    /// #[inline(never)]` function (`fuel_exhausted_err`), not inlined here:
    /// measured on `bench/fuel-lcg.mova`'s compiled-without-NumLoop tier
    /// (the shape that checks fuel once per `recur` back-edge in
    /// `compile::exec::exec_loop`), inlining `RjError::fuel_exhausted(..)
    /// .with_stack(self.stack_snapshot(), self.source_id)` straight into this function cost
    /// ~18% even with `fuel: None` -- `RjError` is a deliberately rich,
    /// large struct (span/label/stack/errno/thrown value, see its own doc
    /// and `#![allow(clippy::result_large_err)]` in `lib.rs`), and the
    /// NEVER-TAKEN branch's construction code (a `Vec` clone, `String`
    /// alloc, several field writes) was bloating the loop body enough to
    /// hurt icache/branch-prediction on every iteration, not just the one
    /// that would actually exhaust. Outlining it into a real, uninlined
    /// call dropped that to noise (see `bench/optimization-log.md`'s fuel
    /// section for the full A/B). The lesson generalizes: a hot check
    /// guarding a large `Err` payload should call out to construct it, not
    /// inline the construction, even though the branch is "just" an `if`.
    #[inline(always)]
    pub(crate) fn tick_fuel(&mut self) -> Result<(), RjError> {
        if let Some(remaining) = &mut self.fuel {
            if *remaining == 0 {
                return Err(self.fuel_exhausted_err());
            }
            *remaining -= 1;
        }
        Ok(())
    }

    /// P0c: loop back-edge poll = fuel + interrupt flag (one Relaxed byte
    /// load). Call entry (`tick_fuel`) deliberately does NOT poll: unbounded
    /// non-tail recursion ends in "stack overflow", and every unbounded loop
    /// passes a back-edge, a lazy-seq force, or an interruptible native.
    #[inline(always)]
    pub(crate) fn tick_edge(&mut self) -> Result<(), RjError> {
        self.tick_fuel()?;
        if self.intr.pending() {
            return self.interrupt_check();
        }
        Ok(())
    }

    /// Cold half of the interrupt poll (kept out of line, same lesson as
    /// `fuel_exhausted_err`): consumes the pending state into an error.
    #[cold]
    #[inline(never)]
    pub(crate) fn interrupt_check(&self) -> Result<(), RjError> {
        match self.intr.take_err_loop() {
            Some(e) => Err(e.with_stack(self.stack_snapshot(), self.source_id)),
            None => Ok(()),
        }
    }

    #[cold]
    #[inline(never)]
    fn fuel_exhausted_err(&self) -> RjError {
        RjError::fuel_exhausted("fuel exhausted").with_stack(self.stack_snapshot(), self.source_id)
    }

    /// Evaluates the embedded `core/core.mova` bootstrap (defn, control-flow
    /// macros, lazy-seq composition fns, etc.) against `self.globals`. This
    /// is a build-time invariant, not a user-input path: `core/core.mova`
    /// ships with the crate and is covered by the stdlib test suite, so a
    /// failure here means mova itself is broken, not that the user did
    /// anything wrong. We still render the *full* miette diagnostic (with
    /// source snippet) to stderr before panicking, so a regression is
    /// immediately diagnosable instead of just showing a bare message.
    fn load_core(&mut self) {
        const CORE_SRC: &str = include_str!("../../core/core.mova");
        // W-ADX item 4c: suppress MOVA_EXPLAIN=1's per-fn/per-loop stderr
        // lines for the duration of the bootstrap -- 2731 lines of
        // `defn`s the user didn't write and can't act on. The
        // `compile_explain` DATA registry (`(compile-explain some-fn)`)
        // still populates normally either way; only the eprintln gates
        // check this flag (see `compile::explain::record`/
        // `report_top_level_loop`).
        let saved_suppress = std::mem::replace(&mut self.suppress_explain, true);
        if let Err(e) = self.eval_str("core/core.mova", CORE_SRC) {
            let rendered = crate::error::render(&e, "core/core.mova", CORE_SRC);
            eprintln!("{rendered}");
            panic!("mova core bootstrap failed: {}", e.message);
        }
        self.suppress_explain = saved_suppress;
        // Reset diagnostics state so the first *user* error doesn't show
        // stale core.mova source/name.
        self.source_name = Str::from("repl");
        self.source = Str::from("");
        self.source_id = crate::source_registry::UNKNOWN_SOURCE;
    }

    /// Swaps the handful of hottest `core.mova` macros' interpreted
    /// closures for a copy carrying a native (Rust) fast-path expander --
    /// see `crate::native_macros` and `Closure::native_macro`'s doc. Run
    /// once, right after `load_core`, so every one of `core.mova`'s own
    /// remaining top-level forms (defined AFTER `defn` in the file) is
    /// already macroexpanded by the time this runs and unaffected either
    /// way -- this only changes how FUTURE expansions of that macro name
    /// happen, never past ones. Each swap keeps the original interpreted
    /// closure around as the native expander's fallback, so any input
    /// shape the fast path isn't confident about still gets the exact old
    /// behavior (see `native_macros::native_defn`'s doc).
    fn install_native_macros(&mut self) {
        // Escape hatch for the differential test below (and for A/B
        // perf measurement against the exact same binary): skips every
        // native-macro swap, leaving every `core.mova` macro on its
        // plain interpreted path.
        if std::env::var("MOVA_NO_NATIVE_MACROS").is_ok() {
            return;
        }
        self.swap_native_macro("defn", crate::native_macros::native_defn);
    }

    /// One `install_native_macros` step: looks up `name` in `clojure.core`
    /// (where every `core.mova` `defmacro` lands), and -- if it is still a
    /// plain interpreted `Value::Macro` with no native expander yet --
    /// rebuilds it as an identical closure with `native_macro: Some(f)`,
    /// keeping the original as that new closure's fallback. A no-op if
    /// the lookup fails or the var is already something else (defensive;
    /// never true in practice at this fixed point in bootstrap).
    fn swap_native_macro(&mut self, name: &str, f: crate::value::NativeMacroFn) {
        // `qualify_def` never qualifies a `defmacro`/`def` written
        // inside `clojure.core` itself -- it interns BARE (see that
        // fn's doc) -- so that is the key `eval_defmacro` actually wrote
        // `defn` under, not `clojure.core/defn`. `Env::get`'s own
        // qualified->bare retry silently papered over probing the wrong
        // (qualified) key here; `Env::set` has no such retry, so writing
        // the qualified key would have interned a second, dead cell.
        let sym = crate::value::Symbol::simple(name);
        let Some(Value::Macro(old)) = self.globals.get(&sym) else {
            return;
        };
        if old.native_macro.is_some() {
            return;
        }
        let new_closure = std::sync::Arc::new(crate::value::Closure {
            name: old.name.clone(),
            arities: old.arities.clone(),
            env: old.env.clone(),
            ns: old.ns.clone(),
            compiled: crate::value::CompileSlot::settled(None, old.lens_site()),
            unchecked_math: old.unchecked_math,
            def_span: old.def_span,
            def_source_id: old.def_source_id.clone(),
            native_macro: Some(f),
        });
        self.globals.set(sym, Value::Macro(new_closure));
    }

    /// Evaluates the embedded `core/async.mova` bootstrap (v0.2 / A2's
    /// `go`/`go-loop`/`thread`/`>!`/`<!`/`alts!`/`onto-chan!`) against
    /// `self.globals`, right after `load_core` so every core.mova macro
    /// (`defn`, `let`, `loop`, `when`, `->`, ...) and every native from
    /// `src/builtins/async.rs` (`chan`, `>!!`, `<!!`, `close!`, `go*`, ...)
    /// are already available. Same build-time-invariant reasoning as
    /// `load_core`: a failure here means mova itself is broken.
    fn load_core_async(&mut self) {
        const CORE_ASYNC_SRC: &str = include_str!("../../core/async.mova");
        // W-ADX item 4c: same MOVA_EXPLAIN suppression as `load_core`.
        let saved_suppress = std::mem::replace(&mut self.suppress_explain, true);
        if let Err(e) = self.eval_str("core/async.mova", CORE_ASYNC_SRC) {
            let rendered = crate::error::render(&e, "core/async.mova", CORE_ASYNC_SRC);
            eprintln!("{rendered}");
            panic!("mova core.async bootstrap failed: {}", e.message);
        }
        self.suppress_explain = saved_suppress;
        self.source_name = Str::from("repl");
        self.source = Str::from("");
        self.source_id = crate::source_registry::UNKNOWN_SOURCE;
        self.install_core_async_ns();
    }

    /// DESIGN-flow-namespace.md Part 1 point 4: gives `clojure.core.async`
    /// a REAL namespace, right after `load_core_async` populates every
    /// bare spelling -- so `(require '[clojure.core.async :as a])`
    /// succeeds and `a/chan`, `a/go`, `a/timeout-put`, ... resolve to real
    /// vars, and an unimplemented upstream name (`a/merge`, `a/map`,
    /// `a/reduce`, ...) fails loudly with "Unable to resolve" rather than
    /// silently falling through to the `clojure.core` fn of the same name
    /// (the trap `for_each_global_candidate`'s trailing bare-name probe
    /// sets once item 5 narrows it -- see `ns.rs`'s doc on that change).
    ///
    /// Mechanism: `bind_alias`, not a second `reg`/`def` -- every name
    /// below already has a bound cell under its bare spelling (either a
    /// native from `src/builtins/async.rs`, or a macro/fn `core/async.mova`
    /// just defined), and `bind_alias` points `clojure.core.async/<name>`
    /// at that SAME `Arc<VarCell>` rather than creating an independent one.
    /// One var cell, two spellings -- `alter-var-root` (or a future
    /// `MOVA_GO_THREADS`-style redefinition) on either is seen through the
    /// other, which a value-copy dual-interning could never give. This
    /// also answers the "does a `(def go go)`-style alias preserve
    /// macro-ness" question the design raised: macro-ness lives on the
    /// `Value::Macro` the cell HOLDS, not on the cell or the symbol, so
    /// sharing the cell trivially preserves it -- no delegating
    /// `defmacro` shim needed.
    ///
    /// This is the ENTIRE public surface the design calls for -- exactly
    /// what mova implements, no stubs for upstream names mova lacks
    /// (`merge`/`map`/`mult`/`pipe`/...). Bare spellings are completely
    /// unaffected (deliberate mova stance: async is part of the language);
    /// this only ALSO gives every name a canonical home.
    ///
    /// Namespace bookkeeping: this function does not call
    /// `mark_ns_loaded` itself -- it doesn't need to. Every symbol
    /// `bind_alias` interns below is qualified under `clojure.core.async`,
    /// so that name shows up in `self.globals.interned_namespaces()`, and
    /// `seed_builtin_namespaces` (called once, at the very end of the
    /// bootstrap, after this) marks every such namespace loaded already --
    /// the same mechanism that already seeds `clojure.string`. No
    /// ordering trap here the way `clojure.core.async.flow` has (see
    /// `require_core_flow`'s doc): there is no embedded `(ns ...)` file
    /// for `seed_builtin_namespaces` to race against, since this function
    /// populates the namespace directly.
    fn install_core_async_ns(&mut self) {
        const ASYNC_NS: &str = "clojure.core.async";
        // Natives (`src/builtins/async.rs`'s `register`) plus the
        // macros/fns `core/async.mova` just defined -- the union is
        // exactly `clojure.core.async`'s public surface.
        const PUBLIC_NAMES: &[&str] = &[
            // src/builtins/async.rs
            "chan",
            "dropping-buffer",
            "sliding-buffer",
            ">!!",
            "<!!",
            "close!",
            "poll!",
            "offer!",
            "timeout",
            "timeout-put",
            "cancel-timer!",
            "timer-armed?",
            "put!",
            "take!",
            "alts!!",
            "go*",
            "thread*",
            // src/builtins/predicates.rs -- the one async-surface name
            // registered outside async.rs (a channel predicate is still
            // async surface; mova extra, upstream has no chan?). Found the
            // hard way: keyborda's timer facade spells it `async/chan?`,
            // which the restricted qualified->bare fallback correctly
            // refused until this row existed.
            "chan?",
            // core/async.mova
            "go",
            "go-loop",
            "thread",
            ">!",
            "<!",
            "alts!",
            "onto-chan!",
        ];
        for name in PUBLIC_NAMES {
            let Some(cell) = self.globals.find_bound_cell(&Symbol::simple(*name)) else {
                // Build-time invariant: every name above was just defined
                // by `register_conc`/`load_core_async`, so a miss means
                // this list has drifted from that surface -- loud, not
                // silently skipped, same "mova itself is broken" contract
                // as the rest of this bootstrap.
                panic!(
                    "install_core_async_ns: bare `{name}` has no bound cell -- \
                     PUBLIC_NAMES has drifted from src/builtins/async.rs/core/async.mova"
                );
            };
            crate::srcindex::note_var_alias(ASYNC_NS, name, name);
            self.globals.bind_alias(
                Symbol { ns: Some(Str::from(ASYNC_NS)), name: Str::from(*name) },
                cell,
            );
        }
    }

    /// ns: restrict qualified->bare fallback to clojure.core spellings
    /// (DESIGN-flow-namespace.md item 5, `clojure.repl` orphan fallout):
    /// `doc`/`source`/`apropos`/`dir` are C3h's real, transliterated
    /// `clojure.repl` surface (`core/core.mova` ~2586-2967), but they were
    /// only ever interned BARE -- `(clojure.repl/doc doc)` used to reach
    /// them purely through the old unconditional trailing bare-name probe,
    /// exactly like `flow/process` before item 1-4 and `Math/abs` before
    /// 9462893. Called right after `load_core` (not gated on `caps.conc`/
    /// `caps.flow` the way `install_core_async_ns`/`require_core_flow`
    /// are -- `clojure.repl`'s four names are pure `core.mova` surface,
    /// no channel/flow natives involved) so every name below is already
    /// bound by the time this runs.
    ///
    /// Mechanism: `bind_alias`, same as `install_core_async_ns` right
    /// below -- one `Arc<VarCell>` per name, shared under both the bare
    /// and the `clojure.repl/`-qualified spelling, so macro-ness
    /// (`doc`/`source`/`dir` are macros; `apropos` is a plain fn) carries
    /// over automatically: it lives on the `Value::Macro` the cell holds,
    /// not on the cell or the symbol.
    ///
    /// Namespace bookkeeping: same as `install_core_async_ns` -- this
    /// function does not call `mark_ns_loaded` itself. Every symbol
    /// `bind_alias` interns below is qualified under `clojure.repl`, so it
    /// shows up in `self.globals.interned_namespaces()`, and
    /// `seed_builtin_namespaces` (called once, at the very end of the
    /// bootstrap) marks that namespace loaded already.
    fn install_clojure_repl_ns(&mut self) {
        const REPL_NS: &str = "clojure.repl";
        // core/core.mova's C3h section -- the entire `clojure.repl`
        // surface mova ports today. No `pst`/`demunge`: neither is
        // defined bare anywhere in core.mova (checked), so there is
        // nothing orphaned to restore a binding for; inventing new fns
        // here would be a different, out-of-scope change.
        const PUBLIC_NAMES: &[&str] = &["doc", "source", "apropos", "dir", "pst"];
        for name in PUBLIC_NAMES {
            let Some(cell) = self.globals.find_bound_cell(&Symbol::simple(*name)) else {
                // Build-time invariant: every name above is defined by
                // `load_core` (core.mova's C3h section), just evaluated
                // right before this runs -- a miss means this list has
                // drifted from that surface, loud rather than silently
                // skipped, same contract as `install_core_async_ns`.
                panic!(
                    "install_clojure_repl_ns: bare `{name}` has no bound cell -- \
                     PUBLIC_NAMES has drifted from core/core.mova's clojure.repl section"
                );
            };
            crate::srcindex::note_var_alias(REPL_NS, name, name);
            self.globals.bind_alias(
                Symbol { ns: Some(Str::from(REPL_NS)), name: Str::from(*name) },
                cell,
            );
        }
    }

    /// DESIGN-flow-namespace.md Part 1 point 2: `clojure.core.async.flow`
    /// is now "literally a normal namespace of the language that happens
    /// to ship in the binary" -- no snowflake loader, an ordinary internal
    /// `(require 'clojure.core.async.flow)` through `require_ns`, the SAME
    /// path a user's own `require` takes: same `loaded` memo, same cycle
    /// detection, same `*ns*` save/restore as any user file. Replaces the
    /// old `load_core_flow`, which `eval_str`-ed `core/flow.mova` directly
    /// with no `(ns ...)` form, landing `process`/`map->step` as bare core
    /// globals instead of real vars in their own namespace.
    ///
    /// Called right after `load_core_async` (so the channel natives
    /// `clojure.core.async.flow.mova`'s `process`/`ping` wrappers need are
    /// already in scope) and, load-bearing, BEFORE
    /// `seed_builtin_namespaces` -- that fn marks every namespace any
    /// interned-builtin qualified symbol names as already `loaded`, and
    /// after item 1 moved the natives to intern directly under
    /// `clojure.core.async.flow`, that namespace IS one of those. Seeding
    /// first would make this `require_ns` call a silent no-op (the
    /// `loaded` memo short-circuits it before the embedded module's `(ns
    /// ...)` form ever runs), so `process`/`map->step`/`ping`/`ping-proc`
    /// would never get defined at all. `register_flow` has already interned
    /// every native this file's bare-name references need by the time this
    /// runs, so ordering the require before the seed is sufficient and
    /// needs no special-casing inside `seed_builtin_namespaces` itself.
    ///
    /// Same build-time-invariant reasoning as `load_core`/
    /// `load_core_async`: a failure here means mova itself is broken, so
    /// this panics rather than propagating -- with the same MOVA_EXPLAIN
    /// suppression discipline as those two.
    fn require_core_flow(&mut self) {
        let saved_suppress = std::mem::replace(&mut self.suppress_explain, true);
        let ns = Str::from("clojure.core.async.flow");
        let result = self.require_ns(&ns, Span { start: 0, end: 0 });
        self.suppress_explain = saved_suppress;
        if let Err(e) = result {
            // `require_ns` deliberately leaves `source_name`/`source`
            // pointing at the failing file on error (see its own doc), so
            // rendering against `self`'s current diagnostics state here
            // -- not a hardcoded "core/flow.mova" -- is what actually
            // names the file the error is really in.
            let rendered = crate::error::render(&e, &self.source_name, &self.source);
            eprintln!("{rendered}");
            panic!("mova clojure.core.async.flow bootstrap failed: {}", e.message);
        }
        self.source_name = Str::from("repl");
        self.source = Str::from("");
        self.source_id = crate::source_registry::UNKNOWN_SOURCE;
    }

    /// Evaluates one already-read top-level form against `self.globals`.
    ///
    /// W4B-WARNINGS: `crate::reflwarn::analyze_top_level` runs FIRST, once,
    /// on the raw form -- this is THE one non-recursive top-level entry
    /// every file-load/`eval`-native/REPL path already funnels through
    /// (`eval_form_in` is the recursive per-subform workhorse a running
    /// closure's body uses instead, so a `defn`'d fn's body is walked
    /// exactly once, at `defn`-eval time, never again on each call -- see
    /// that module's own doc). Its own first line is the cheap flag check
    /// that keeps the default (both vars false) path free.
    ///
    /// field1/W-EXPLAIN: `crate::compile::explain::report_top_level_loop`
    /// runs right after, on the same raw form -- a sibling top-level-only
    /// check for the OTHER cliff a top-level form can hide: a `loop`/
    /// `dotimes`/`while` that never sits inside a `fn` never reaches
    /// `compile::resolve` at all (only fn bodies ever attempt compilation),
    /// so it has no `FnExplain` for `report_fn`/`report_loops` to find. Its
    /// own gate is `MOVA_EXPLAIN`, independent of `*warn-on-reflection*`.
    pub fn eval_form(&mut self, form: &Form) -> Result<Value, RjError> {
        crate::reflwarn::analyze_top_level(self, form);
        let globals = self.globals.clone();
        crate::compile::explain::report_top_level_loop(self, &globals, form);
        // W4C: a bare top-level form is ITSELF a compiled unit on the real
        // JVM (an anonymous zero-arg invoke, compiled and immediately run),
        // not merely "code that happens to run before any defn exists" --
        // measured: `clojure -M -e '(set! *unchecked-math* true)' -e '(+
        // Long/MAX_VALUE 1)'` (two SEPARATE top-level forms, matching one
        // `eval_form` call each, exactly like a REPL) wraps to
        // `Long/MIN_VALUE`, even though NEITHER form is a `defn`. This is
        // the flag's value at compile time of THIS top-level form, exactly
        // the read `eval_fn_form` does for a closure -- restored
        // afterward, so it cannot leak into the NEXT top-level form (whose
        // own `eval_form` call re-reads the (possibly `set!`-mutated) var
        // fresh, same as the JVM re-compiling the next form fresh). Nested
        // closure CALLS within this form still override it with their own
        // captured flag via `apply_closure`'s save/restore, and restore it
        // back to THIS value on return -- so this is genuinely "the
        // current compiled unit's flag", not merely "the top-level
        // default".
        let this_form_unchecked = self.unchecked_math_active();
        let caller_unchecked = std::mem::replace(&mut self.current_unchecked, this_form_unchecked);
        let result = self.eval_form_in(form, &globals);
        self.current_unchecked = caller_unchecked;
        match result {
            // A `recur` that escaped every enclosing loop/fn is a user
            // error, not an internal signal; convert it in place so its
            // span/stack/message (already descriptive) survive intact.
            Err(mut e) if e.kind == ErrorKind::Recur => {
                e.kind = ErrorKind::Other;
                Err(e)
            }
            other => other,
        }
    }

    /// Evaluates every form in order, returning the last value (or `Nil`
    /// for an empty slice). Stops at the first error.
    pub fn eval_forms(&mut self, forms: &[Form]) -> Result<Value, RjError> {
        let mut result = Value::Nil;
        for form in forms {
            result = self.eval_form(form)?;
        }
        Ok(result)
    }

    /// Parses `source` and evaluates every form in it, returning the last
    /// value. Records `source_name`/`source` on `self` first so any error
    /// (reader or runtime) can be rendered with a miette snippet.
    ///
    /// C3d: reads and evaluates one top-level form at a time (rather than
    /// `reader::read_all` up front, then `eval_forms` over the whole
    /// `Vec<Form>`) -- refreshing the reader's `::kw`/`::alias/kw` context
    /// (`Interp::reader_ns_context`) before EACH read, exactly mirroring
    /// real Clojure's own compiler loop (`Compiler.load` reads one form,
    /// evaluates it, reads the next), so a `(require '[x :as y])` earlier
    /// in this same file is visible to a later `::y/z` (measured:
    /// `tests/clojure-suite/vendor/special.clj`'s
    /// `resolve-keyword-ns-alias-in-destructuring`, a top-level `(require
    /// '[clojure.string :as s])` followed by a separate `deftest` that
    /// reads `::s/x`). This is a strict conformance improvement, not a
    /// looser one: side effects from forms before a later syntax/eval
    /// error already ran under the old `eval_forms` loop too (it returns
    /// on the FIRST error, same as this one); the only thing that changes
    /// is that a syntax error later in the file no longer retroactively
    /// prevents forms before it from having been read/evaluated at all,
    /// matching the JVM.
    pub fn eval_str(&mut self, source_name: &str, source: &str) -> Result<Value, RjError> {
        self.source_name = Str::from(source_name);
        self.source = Str::from(source);
        // field5/W-SPAN: cold-path intern (once per `eval_str` call, never
        // per form) -- see `source_registry`'s module doc.
        self.source_id = crate::source_registry::intern(source_name, source);
        let mut reader = crate::reader::Reader::new(source);
        self.eval_reader(&mut reader)
    }

    /// `eval_str`, but with `#?`/`#?@` reader-conditional dispatch enabled
    /// (S5) -- the `.cljc` sibling `ns.rs::require_ns` calls when the
    /// resolved namespace file has a `.cljc` extension (see that method
    /// and `reader::Reader::allow_read_cond`'s doc comment for why every
    /// other loading path, including plain `.mova` files, keeps read-cond
    /// OFF by default, matching real Clojure's own `.clj`-vs-`.cljc`
    /// split).
    pub fn eval_str_allow_read_cond(&mut self, source_name: &str, source: &str) -> Result<Value, RjError> {
        self.source_name = Str::from(source_name);
        self.source = Str::from(source);
        // field5/W-SPAN: same one-intern-per-call discipline as `eval_str`.
        self.source_id = crate::source_registry::intern(source_name, source);
        let mut reader = crate::reader::Reader::new_allow_read_cond(source);
        self.eval_reader(&mut reader)
    }

    /// Shared interleaved read+eval loop for `eval_str`/
    /// `eval_str_allow_read_cond` -- see the former's doc for why this
    /// interleaves rather than reading the whole source up front.
    fn eval_reader(&mut self, reader: &mut crate::reader::Reader) -> Result<Value, RjError> {
        let mut result = Value::Nil;
        loop {
            reader.set_ns_ctx(self.reader_ns_context());
            // PERF-PROBE (MOVA_LOAD_TRACE): read/eval phase split, both
            // behind the same `enabled()` check `load_trace`'s counters
            // already pay for -- a no-op call when the env var is unset.
            let traced = crate::load_trace::enabled();
            let t0 = if traced { Some(std::time::Instant::now()) } else { None };
            let form = reader.next_form()?;
            if let Some(t0) = t0 {
                crate::load_trace::add_read_ns(t0.elapsed().as_nanos() as u64);
            }
            match form {
                Some(form) => {
                    let t1 = if traced { Some(std::time::Instant::now()) } else { None };
                    if traced {
                        crate::load_trace::push_top_level(describe_top_level_form(&form, &self.current_ns));
                    }
                    let evaled = self.eval_form(&form);
                    if traced {
                        crate::load_trace::pop_frame();
                    }
                    result = evaled?;
                    if let Some(t1) = t1 {
                        crate::load_trace::add_eval_ns(t1.elapsed().as_nanos() as u64);
                        crate::load_trace::inc_forms();
                    }
                }
                None => return Ok(result),
            }
        }
    }

    /// Core recursive evaluator: `form` against the lexical env `env`.
    ///
    /// `pub(crate)` since field3/W-RESOLVE: `compile::exec`'s `Ir::Escape`
    /// arm calls straight back into it for one interop form, which is what
    /// makes an escape provably identical to tree-walking that form -- it
    /// IS tree-walking that form. Nothing else in `compile` may use it.
    pub(crate) fn eval_form_in(&mut self, form: &Form, env: &Env) -> Result<Value, RjError> {
        match &form.value {
            // A SYMBOL form is listed FIRST, ahead of the `^{...}` guard
            // below, for two reasons. Semantically, `^:private x` in
            // expression position evaluates to whatever `x` is bound to:
            // Clojure attaches the reader's metadata to the *symbol*,
            // which the compiler then consumes (`def`) or ignores -- it
            // never reaches the resolved value (`(do (def ^:zz sv 5) sv)`
            // is `5`, measured, not a `5` wearing `{:zz true}`; `5`
            // couldn't wear it anyway). And practically, a symbol lookup
            // is the most frequent thing this evaluator ever does, so
            // this ordering means the common case never even loads the
            // `meta` field.
            FormValue::Atom(Value::Sym(sym)) => {
                // W4-EVAL task 1 / W-VARS-PRIV follow-up: qualified-symbol
                // privacy gate, fused into the SAME resolution walk as the
                // ordinary lookup (matching the oracle's compile-time
                // `resolveIn` check, which fires whether or not the read
                // would otherwise have succeeded). `resolve_symbol_checked`
                // (see `ns::Interp`'s doc) does the gate and the lookup
                // together over one `for_each_global_candidate` pass --
                // calling `check_qualified_private` and `resolve_symbol`
                // back to back here used to walk that candidate order
                // TWICE per qualified reference, measured at ~8% extra
                // user CPU on a qualified-call-heavy hot loop; an
                // unqualified symbol still costs exactly the one lookup
                // `resolve_symbol` always needed.
                match self.resolve_symbol_checked(env, sym) {
                // W3e2: a macro named in VALUE position is an error, exactly
                // as `Compiler.analyzeSymbol` makes it one ("Can't take
                // value of a macro: #'clojure.core/let", measured). HEAD
                // position is resolved by `eval_list` directly, without
                // coming through here, so macro dispatch is untouched --
                // see that fn.
                //
                // This became load-bearing with W3e-2's `clojure.core`
                // forwarding macro vars: before them `(eval 'let)` failed
                // with "Unable to resolve symbol" because no cell existed,
                // and `clojure.test-clojure.evaluation/SymbolResolution`
                // asserts (for 13 names, `let`/`fn`/`loop` among them) only
                // that evaluating the bare name THROWS. Giving those names
                // real vars without also implementing this rule turned
                // three of those assertions into silent passes-as-values.
                Ok(Some(Value::Macro(m))) => Err(RjError::other(format!(
                    "Can't take value of a macro: #'{}",
                    crate::printer::pr_str(&Value::Sym(Self::macro_var_name_of(&m, sym)))
                ))
                // `Compiler$CompilerException`, not `IllegalStateException`:
                // on the real JVM this is a COMPILE-time condition, so the
                // `IllegalStateException` is the CAUSE and what actually
                // reaches user code -- and what `(thrown? C ...)` matches --
                // is the compiler's wrapper. mova has no cause chain to
                // hang the inner class off (see
                // `special_forms`'s `ErrorKind::Unresolved` row, which
                // makes exactly this call for exactly this reason), and
                // `clojure.test-clojure.evaluation/SymbolResolution`
                // asserts `(thrown? Compiler$CompilerException (eval 'let))`
                // -- which C3g's class-aware `thrown?` now really checks.
                .with_class(crate::error::JvmClass::CompilerException)
                .with_span(form.span)
                .with_label("a macro, not a value")
                .with_stack(self.stack_snapshot(), self.source_id)),
                Ok(Some(v)) => Ok(v),
                Ok(None) => Err(RjError::unresolved(format!(
                    "Unable to resolve symbol: {}",
                    crate::printer::pr_str(&Value::Sym(sym.clone()))
                ))
                .with_span(form.span)
                .with_label("undefined here")
                .with_stack(self.stack_snapshot(), self.source_id)),
                Err(e) => Err(e.with_span(form.span).with_stack(self.stack_snapshot(), self.source_id)),
                }
            }
            FormValue::Atom(Value::Keyword(k)) => {
                // S5: mirrors real Clojure's reader interning every
                // keyword token it reads -- mova instead interns at
                // literal-EVALUATION time (`KeywordRegistry`'s doc
                // explains the coverage gap this leaves and why it's
                // accepted). Covers every keyword nested inside a
                // literal vector/map/set/list too, since those forms
                // recurse back through `eval_form_in` per element.
                // Listed ahead of the metadata guard below: a keyword
                // form can never carry `^` metadata (the reader rejects
                // it), so the guard's `is_some` load is skipped on this
                // hot arm too.
                self.keywords.intern(k.text_ref());
                Ok(Value::Keyword(k.clone()))
            }
            // S5/M3: `^{...}` on any other evaluated form. One `is_some`
            // test on a field that is `None` for every form in every
            // benchmark, reached only after the symbol arm above missed.
            _ if form.meta.is_some() => self.eval_form_with_meta(form, env),
            FormValue::Atom(v) => Ok(v.clone()),
            FormValue::Vector(items) => {
                // Single-shot `Vec` -> `PVec` conversion (see
                // `compile::exec::exec_vector`'s identical comment): avoids
                // `items.len()` incremental reallocs of the growing
                // `Small` array for a literal vector form like `[a b c]`.
                let mut out = Vec::with_capacity(items.len());
                for it in items {
                    out.push(self.eval_form_in(it, env)?);
                }
                Ok(Value::Vector(out.into()))
            }
            FormValue::Set(items) => {
                let mut out = champ::PersistentHashSet::new().transient();
                for it in items {
                    // C3e: hash-keyed position -- see `normalize_key`.
                    let el = self.eval_form_in(it, env)?;
                    let el = self.normalize_key(el)?;
                    out.insert(el);
                }
                Ok(Value::Set(out.persistent()))
            }
            FormValue::Map(pairs) => {
                let mut out = PMap::new();
                for (k, v) in pairs {
                    // C3e: hash-keyed position -- see `normalize_key`.
                    let kv = self.eval_form_in(k, env)?;
                    let kv = self.normalize_key(kv)?;
                    let vv = self.eval_form_in(v, env)?;
                    out.insert(kv, vv);
                }
                map_probe::record("map-literal", out.len());
                Ok(Value::Map(out))
            }
            FormValue::List(items) => self.eval_list(items, form.span, env),
        }
    }

    /// S5/M3: the cold half of `eval_form_in`'s metadata branch --
    /// evaluate the form, evaluate its metadata map, attach.
    ///
    /// C11: ...*unless* the form is a LIST being evaluated as code, in
    /// which case the metadata is a COMPILE-TIME annotation on the
    /// expression and never reaches the value it produces. Measured on
    /// the oracle as rows 200-213 of `compat/qq-probe.clj` (transcript:
    /// `compat/qq-oracle-transcript.txt`) -- those two files are the
    /// evidence, the summary below is only a reading aid:
    ///
    /// ```text
    /// (meta ^long (first (range 3)))           => nil
    /// (meta ^{:x 1} (list 1 2))                => nil
    /// (meta ^{:x 1} (with-meta [1] {:y 2}))    => {:y 2}   ; value's own meta wins
    /// (meta ^{:x 1} (let [] [1 2]))            => nil
    /// (meta ^{:x 1} (if true [1 2]))           => nil
    /// (meta ^{:x 1} (do [1 2]))                => nil
    /// (meta ^{:x 1} [1 2])                     => {:x 1}   ; COLLECTION LITERALS keep it
    /// (meta ^{:x 1} (fn [] 1))                 => {:x 1}   ; `fn` is the JVM's one exception
    /// ```
    ///
    /// and the metadata map itself is NOT evaluated for a list form
    /// (measured: a `(print ...)` inside `^{:a ...}` on a list never
    /// fires, while the same metadata on a vector literal does), which
    /// is why the early return happens before `eval_meta_form`.
    ///
    /// This was a real bug, not a nicety: attaching wrapped the call's
    /// result in `Value::Meta`, and the arithmetic ops do not see
    /// through that wrapper, so `(> ^long (first (range 3)) 0)` -- a
    /// plain type hint, i.e. exactly what these forms are in practice --
    /// failed with the nonsensical ">: expected a number, got int"
    /// (numbers.clj's `warn-on-boxed` deftest). Every occurrence of
    /// list-form metadata in the whole vendored suite is a type hint of
    /// that kind (`^long (first ...)`, `^Collection (subvec ...)`,
    /// `^clojure.lang.IReduce (list ...)`, `^bytes (byte-array ...)`),
    /// and hints must be inert on a non-JVM host -- see
    /// [`eval_meta_form`](Interp::eval_meta_form)'s doc for that policy.
    ///
    /// A SYMBOL form's metadata is dropped one arm earlier in
    /// `eval_form_in` for the same reason, and has been since S5/M3.
    fn eval_form_with_meta(&mut self, form: &Form, env: &Env) -> Result<Value, RjError> {
        let bare = Form {
            value: form.value.clone(),
            span: form.span,
            meta: None,
        };
        let value = self.eval_form_in(&bare, env)?;
        if let FormValue::List(items) = &form.value {
            // The JVM's one exception: `FnExpr` copies the form's
            // metadata onto the function object it builds, so
            // `(meta ^{:x 1} (fn [] 1))` IS `{:x 1}` (measured). Keep
            // that shape; drop it for every other list head.
            let is_fn = matches!(
                items.first().map(|f| &f.value),
                Some(FormValue::Atom(Value::Sym(s)))
                    if s.ns.is_none() && matches!(s.name.as_ref(), "fn" | "fn*")
            );
            if !is_fn {
                return Ok(value);
            }
        }
        let meta_form = form.meta.as_ref().expect("caller checked meta.is_some()");
        let meta = self.eval_meta_form(meta_form, env)?;
        Ok(Value::attach_meta(value, meta))
    }

    /// Evaluates a `^{...}` metadata map form.
    ///
    /// This is ordinary evaluation with ONE deliberate deviation, and it
    /// is narrower than the previous revision of this doc claimed: a
    /// metadata value written as a bare SYMBOL is resolved NORMALLY
    /// first (as a local, a var, ...), same as anywhere else in source,
    /// and only falls back to the literal symbol when that resolution
    /// fails.
    ///
    /// Measured on real Clojure: `(let [x 42] (meta ^{:a x} [1 2]))` is
    /// `{:a 42}` -- the map form is compiled and evaluated like any
    /// other map literal in that lexical scope, so a local/var symbol
    /// used as a metadata VALUE resolves to its bound value, not to
    /// itself. `clojure.zip/zipper` depends on exactly this: `^{:zip/
    /// branch? branch? ...} [root nil]` closes over the function's own
    /// params. Treating every bare symbol as inert (the old behaviour)
    /// silently turned `(:zip/branch? (meta loc))` into the symbol
    /// `branch?` instead of the predicate it names, breaking every
    /// zipper op that calls `branch?`/`children`/`make-node`.
    ///
    /// The type-hint case is the one place real Clojure's resolution
    /// target does not exist here: `(meta ^String [1 2])` is `{:tag
    /// java.lang.String}`, measured, a `Class` object, and `^Zork [1 2]`
    /// is a compile error ("Unable to resolve symbol: Zork") -- mova has
    /// no JVM class world to resolve `String`/`long`/`Object`/
    /// `Collection`/... INTO. So: try ordinary resolution; if and only
    /// if it fails with `ErrorKind::Unresolved` (a symbol naming no
    /// local, var or class mova knows), keep the bare symbol instead of
    /// propagating the error, which is what keeps type hints inert
    /// without regressing every corpus file that uses them. Any OTHER
    /// evaluation error (e.g. an arity error inside a var's value) still
    /// propagates, since that failure has nothing to do with hints.
    ///
    /// Every non-symbol metadata value evaluates normally, so
    /// `(meta ^{:a (+ 1 2)} [1])` is `{:a 3}` -- matching Clojure.
    fn eval_meta_form(&mut self, meta_form: &Form, env: &Env) -> Result<Value, RjError> {
        let FormValue::Map(pairs) = &meta_form.value else {
            // The reader only ever builds a `Map` here; anything else
            // came from `value_to_form` on an already-evaluated metadata
            // map, which needs no further evaluation.
            return Ok(crate::reader::form_to_value(meta_form));
        };
        let mut out = PMap::new();
        for (k, v) in pairs {
            let kv = self.eval_meta_entry(k, env)?;
            // W-tag: `:tag` is the reader/compiler's own slot for a
            // TYPE HINT -- `^String x`, `^long x`, `^{:tag Foo} x`, all
            // desugar to a `:tag` key here. Real Clojure resolves that
            // value to a `Class` object; mova has no such Class world
            // for `:tag` to mean anything TO (see this fn's doc for why
            // `eval_meta_entry` otherwise tries ordinary resolution
            // first). Since W4 (`clojure-suite` protocols.clj regression,
            // measured): `String`/`Long`/... ARE resolvable bare symbols
            // in mova (`java.lang.*` auto-import, so `(class String)` ->
            // `java.lang.Class` works) -- so the generic resolve-first
            // path in `eval_meta_entry` silently turned every `^String`
            // hint's `:tag` from the literal symbol `getBasis`/protocol
            // var meta must report (measured against real Clojure:
            // `(:tag (meta (var baz)))` is the symbol `java.lang.String`,
            // and a record's `getBasis` element carries `:tag` `String`
            // UNqualified, exactly as spelled) into a resolved `Value::
            // Class`, breaking both. `:tag`'s value is therefore ALWAYS
            // kept as the literal form, bypassing `eval_meta_entry`
            // entirely -- narrower than a blanket revert of W-zipper
            // (below), since every OTHER key (e.g. `clojure.zip/zipper`'s
            // `:zip/branch?`) still needs the resolve-first behavior.
            let is_tag_key = kv == Value::Keyword(Keyword::from("tag"));
            let vv = if is_tag_key {
                crate::reader::form_to_value(v)
            } else {
                self.eval_meta_entry(v, env)?
            };
            out.insert(kv, vv);
        }
        Ok(Value::Map(out))
    }

    /// One key or value inside a metadata map: a bare symbol resolves
    /// normally (local/var lookup) and only falls back to the literal
    /// symbol if resolution fails as `ErrorKind::Unresolved` -- see
    /// [`eval_meta_form`](Interp::eval_meta_form)'s doc. Everything else
    /// evaluates unconditionally. NOTE: `eval_meta_form` never calls this
    /// for a `:tag` value (see the W-tag comment there) -- this fn's
    /// resolve-first behavior is only reached for every OTHER key.
    fn eval_meta_entry(&mut self, form: &Form, env: &Env) -> Result<Value, RjError> {
        match &form.value {
            FormValue::Atom(Value::Sym(_)) if form.meta.is_none() => {
                match self.eval_form_in(form, env) {
                    Ok(v) => Ok(v),
                    Err(e) if e.kind == ErrorKind::Unresolved => {
                        Ok(crate::reader::form_to_value(form))
                    }
                    Err(e) => Err(e),
                }
            }
            _ => self.eval_form_in(form, env),
        }
    }

    /// W3e2: the `#'ns/name` spelling for the "Can't take value of a macro"
    /// message. `Closure::name` is the name the `defmacro` gave it, and
    /// `Closure::ns` the namespace it was written in -- except in
    /// `ns::CORE_NS`, where a macro interns BARE and `ns` is already
    /// `clojure.core`, which is the spelling the oracle prints
    /// (`#'clojure.core/let`). Falls back to the symbol as WRITTEN for an
    /// anonymous macro, which cannot normally be named at all.
    fn macro_var_name_of(m: &std::sync::Arc<crate::value::Closure>, as_written: &Symbol) -> Symbol {
        match &m.name {
            Some(n) => Symbol {
                ns: Some(m.ns.clone()),
                name: n.clone(),
            },
            None => as_written.clone(),
        }
    }

    fn eval_list(&mut self, items: &[Form], span: Span, env: &Env) -> Result<Value, RjError> {
        if items.is_empty() {
            // `()` is self-evaluating, like Clojure.
            return Ok(Value::List(crate::value::PVec::new()));
        }
        if let FormValue::Atom(Value::Sym(sym)) = &items[0].value {
            // S6/Blocker-1: `core/let` (an alias of `clojure.core`) must
            // dispatch exactly like bare `let` -- see
            // `ns::Interp::is_bare_or_core_alias`'s doc comment.
            // D5: a namespace that defined its OWN binding for a
            // shadowable special-form name (see `special_shadows`) means
            // that binding, not the special form -- so the special-form
            // dispatch is skipped and the ordinary macro/fn path below
            // runs. `is_empty` short-circuits this for every program that
            // never shadows anything, which is essentially all of them.
            if self.is_bare_or_core_alias(sym) && !self.shadows_special(sym) {
                if let Some(result) = self.eval_special(sym.name.as_ref(), &items[1..], span, env) {
                    return result;
                }
            }
        }
        // W3e2: a SYMBOL head is resolved directly rather than through
        // `eval_form_in`, whose symbol arm now refuses to hand back a macro
        // as a value ("Can't take value of a macro"). Head position is the
        // one place a macro value is exactly what is wanted, so it must not
        // go through that check. Anything that is not a bare symbol (a
        // nested call, a `^meta`-carrying head, a literal) still evaluates
        // normally.
        let head_val = match (&items[0].value, &items[0].meta) {
            (FormValue::Atom(Value::Sym(sym)), None) => {
                // W-VARS-PRIV / W-VARS-PRIV follow-up: the same fused
                // gate+lookup as the symbol-VALUE read arm above
                // (`eval_form_in`'s `FormValue::Atom(Value::Sym(..))`
                // case), applied to a symbol in CALL/head position too.
                // Real Clojure's `resolveIn` fires on ANY qualified
                // reference to a non-public var, read or call, fn or
                // macro -- there is no separate "head position"
                // carve-out on the oracle side. Before W-VARS-PRIV,
                // `(p1/priv 1)` on a private `p1/priv` silently resolved
                // and ran; before the follow-up, this called
                // `check_qualified_private` then `resolve_symbol`
                // separately, walking the candidate order twice for
                // every qualified call head. `resolve_symbol_checked`
                // does both in one walk -- see `ns::Interp`'s doc.
                match self.resolve_symbol_checked(env, sym) {
                    Ok(Some(v)) => v,
                    Ok(None) => {
                        return Err(RjError::unresolved(format!(
                            "Unable to resolve symbol: {}",
                            crate::printer::pr_str(&Value::Sym(sym.clone()))
                        ))
                        .with_span(items[0].span)
                        .with_label("undefined here")
                        .with_stack(self.stack_snapshot(), self.source_id));
                    }
                    Err(e) => return Err(e.with_span(items[0].span).with_stack(self.stack_snapshot(), self.source_id)),
                }
            }
            _ => self.eval_form_in(&items[0], env)?,
        };
        if let Value::Macro(closure) = &head_val {
            // H2/macro-expansion-cache: the tree-walker re-expands a macro
            // call EVERY time it is evaluated (~12% CPU on clj-kondo-scale
            // runs, e.g. `get-in`'s overlay macro expanding ~5800x). The
            // compiled tier already freezes expansion at compile time (see
            // COMPILE-TIER-DESIGN.md), so caching here just matches that
            // documented behavior instead of introducing a new one.
            //
            // `items.as_ptr()` is used only as a HashMap key for lookup
            // speed; the actual hit test is a full structural comparison of
            // the call form (`macro_expansion_cache_get`/`forms_equal`), so
            // an address that was freed and reused for an unrelated form,
            // or a macro-synthesized sibling sharing the same span (see
            // `value_to_form`'s doc: EVERY node of an expansion gets the
            // SAME span), is always just a cache miss, never a wrong
            // answer. Redefining the macro (a new `Arc<Closure>`) naturally
            // busts the cache via the identity check.
            //
            // DEVIATION (documented, matches compiled-tier behavior): a
            // macro using auto-gensym (`` sym# ``) gets the SAME gensym on
            // every cache hit at a given call site, like Clojure's
            // compile-once. A macro that legitimately expands differently
            // on each call (reads mutable state at expand time) would also
            // see the frozen first expansion -- no such macro is known in
            // this codebase's macros; if one is added, it must bypass this
            // cache (native_macro fast path).
            let call_ptr = items.as_ptr() as usize;
            let macro_ptr = std::sync::Arc::as_ptr(closure) as *const () as usize;
            if let Some(cached) = macro_expansion_cache_get(call_ptr, macro_ptr, items) {
                return self.eval_form_in(&cached, env);
            }
            // PERF-PROBE (MOVA_LOAD_TRACE): exclusive macro-expansion time,
            // keyed by the symbol as written at the call site. True
            // special forms never reach this branch at all (matched
            // earlier, no `Value::Macro`), so they're never in this table.
            let traced = crate::load_trace::enabled();
            if traced {
                let name = match &items[0].value {
                    FormValue::Atom(Value::Sym(s)) => s.name.to_string(),
                    _ => "<macro>".to_string(),
                };
                crate::load_trace::push_macro(&name);
            }
            let out = (|| -> Result<Form, RjError> {
                let expansion = if let Some(nf) = closure.native_macro {
                    // Native fast path: no `&form`/raw-call-form build at
                    // all -- see `Closure::native_macro`'s doc. Falls back
                    // to `apply_macro` internally (building its own
                    // `raw_call_form`) on any shape it isn't confident
                    // about.
                    nf(self, &items[1..], span, closure)?
                } else {
                    // `&form`: the WHOLE unevaluated call, head included --
                    // see `Interp::macro_form_stack`'s doc.
                    let raw_call_form =
                        Value::List(items.iter().map(crate::reader::form_to_value).collect());
                    self.apply_macro(closure, &items[1..], raw_call_form, span)?
                };
                let expansion_form = self.value_to_form_realized(&expansion, span)?;
                Ok(expansion_form)
            })();
            if traced {
                crate::load_trace::pop_frame();
            }
            let expansion_form = std::sync::Arc::new(out?);
            macro_expansion_cache_put(call_ptr, macro_ptr, items.to_vec(), expansion_form.clone());
            return self.eval_form_in(&expansion_form, env);
        }
        // W4 diet: pooled buffer (`apply_value_owned`'s terminal arms are
        // its one death site and return it to the pool); an error path
        // drops it -- a missed reuse, never a leak.
        let mut arg_values = self.take_buf();
        arg_values.reserve(items.len() - 1);
        for a in &items[1..] {
            arg_values.push(self.eval_form_in(a, env)?);
        }
        // `arg_values` is dead after this call, so hand it over rather than
        // lending it: `apply_value_owned` lets a whitelisted native move its
        // receiver out (Perceus-lite phase 1, `builtins::reuse`).
        self.apply_value_owned(&head_val, arg_values, span)
    }

    /// Evaluates a `do`-style body (used by `do`, `let`, `try`, fn/loop
    /// bodies, etc.): every form for effect, the last form's value returned
    /// (`Nil` for an empty body).
    fn eval_do_body(&mut self, body: &[Form], env: &Env) -> Result<Value, RjError> {
        let mut result = Value::Nil;
        for f in body {
            result = self.eval_form_in(f, env)?;
        }
        Ok(result)
    }

    /// SPEC-W4: turns a value that is about to be read as CODE into one
    /// `crate::reader::value_to_form` can express, by realizing every lazy
    /// seq inside it into a concrete `Value::List`. Returns `None` when
    /// nothing needed realizing, which is the overwhelmingly common case
    /// and costs one allocation-free read-only walk.
    ///
    /// Why it exists: `Form` has no lazy arm, so `value_to_form` maps a
    /// `Value::Lazy` (and a `List` whose last slot is the internal
    /// `LazyTail` cons-cell marker) to `FormValue::Atom` -- i.e. to a
    /// self-evaluating constant rather than to a call. On the JVM the
    /// question never arises: `Compiler.macroexpand1` hands its expansion
    /// straight to `analyze`, which walks it with `RT.seq`/`RT.first`/
    /// `RT.next`, so a macro returning a `LazySeq` (which is what every
    /// `concat`-built expansion -- `->`, `->>`, `doto`, `dotimes` -- IS,
    /// upstream and now here) compiles exactly like the equivalent
    /// `PersistentList`. Measured on 1.13.0-alpha6: `(defmacro m []
    /// (map identity '(+ 1 2))) (m)` => `3`; before this, mova answered
    /// `(+ 1 2)`.
    ///
    /// Applied at the three places a `Value` crosses back into code:
    /// macro expansion in both tiers (`eval_list`, `compile::resolve`),
    /// `macroexpand-1`'s own result (so `macroexpand`'s fixpoint loop can
    /// still see a `List` head to re-expand), and `(eval form)`.
    ///
    /// An infinite lazy seq embedded in a form hangs here -- as it does on
    /// the JVM, whose analyzer walks the whole expansion too.
    pub(crate) fn realize_form_value(&mut self, v: &Value) -> Result<Option<Value>, RjError> {
        match v {
            Value::Lazy(_) | Value::LazyTail(_) => Ok(Some(self.realize_form_seq(v)?)),
            Value::List(items) => {
                if crate::builtins::collections::lazy_tail_split(items).is_some() {
                    return Ok(Some(self.realize_form_seq(v)?));
                }
                Ok(self.realize_form_items(items)?.map(Value::List))
            }
            Value::Vector(items) => Ok(self.realize_form_items(items)?.map(Value::Vector)),
            Value::Map(m) => {
                let mut out: Option<crate::value::PMap> = None;
                for (k, val) in m.iter() {
                    let nk = self.realize_form_value(k)?;
                    let nv = self.realize_form_value(val)?;
                    if nk.is_some() || nv.is_some() {
                        let acc = out.get_or_insert_with(|| m.clone());
                        if nk.is_some() {
                            acc.remove(k);
                        }
                        acc.insert(nk.unwrap_or_else(|| k.clone()), nv.unwrap_or_else(|| val.clone()));
                    }
                }
                Ok(out.map(Value::Map))
            }
            Value::Set(s) => {
                let mut out: Option<champ::PersistentHashSet<Value>> = None;
                for e in s.iter() {
                    if let Some(ne) = self.realize_form_value(e)? {
                        let acc = out.get_or_insert_with(|| s.clone());
                        *acc = acc.remove(e).insert(ne);
                    }
                }
                Ok(out.map(Value::Set))
            }
            Value::Meta(m) => {
                let inner = self.realize_form_value(&m.inner)?;
                let meta = self.realize_form_value(&m.meta)?;
                if inner.is_none() && meta.is_none() {
                    return Ok(None);
                }
                Ok(Some(Value::Meta(std::sync::Arc::new(crate::value::MetaObj {
                    meta: meta.unwrap_or_else(|| m.meta.clone()),
                    inner: inner.unwrap_or_else(|| m.inner.clone()),
                }))))
            }
            _ => Ok(None),
        }
    }

    /// [`realize_form_value`](Self::realize_form_value)'s seq arm: walks a
    /// lazy seq (or a cons-cell-encoded `List`) to its end and rebuilds it
    /// as a plain `Value::List`, realizing each element in turn.
    fn realize_form_seq(&mut self, v: &Value) -> Result<Value, RjError> {
        let items = crate::builtins::collections::materialize(self, v)?;
        let mut out = crate::value::PVec::new();
        for it in items {
            match self.realize_form_value(&it)? {
                Some(n) => out.push_back(n),
                None => out.push_back(it),
            }
        }
        Ok(Value::List(out))
    }

    /// [`realize_form_value`](Self::realize_form_value)'s element walk:
    /// `None` unless some element actually changed, in which case the
    /// (structurally shared) clone carries the replacements.
    fn realize_form_items(&mut self, items: &crate::value::PVec) -> Result<Option<crate::value::PVec>, RjError> {
        let mut out: Option<crate::value::PVec> = None;
        for idx in 0..items.len() {
            let Some(cur) = items.get(idx) else { break };
            if let Some(n) = self.realize_form_value(&cur.clone())? {
                let acc = out.get_or_insert_with(|| items.clone());
                acc.set(idx, n);
            }
        }
        Ok(out)
    }

    /// [`realize_form_value`](Self::realize_form_value) fused with
    /// `value_to_form`: the one call every "this value is code now" site
    /// makes.
    pub(crate) fn value_to_form_realized(&mut self, v: &Value, span: Span) -> Result<Form, RjError> {
        Ok(match self.realize_form_value(v)? {
            Some(realized) => crate::reader::value_to_form(&realized, span),
            None => crate::reader::value_to_form(v, span),
        })
    }

    /// Forces a `Value::Lazy` cell: calls its 0-arity thunk (if not already
    /// realized), SEQS the result (`RT.seq`, see below), memoizes that, and
    /// recursively forces if the result is itself `Lazy`. Non-`Lazy`
    /// values are returned as a cheap clone.
    ///
    /// C3e: real Clojure's `LazySeq.sval`/`.seq` does not merely CHECK that
    /// the body's value is seqable -- it calls `RT.seq` on it and keeps the
    /// SEQ. mova used to check the value against a hand-maintained seqable
    /// whitelist and then memoize the raw value, which left the realized
    /// cell holding a set/string/array/sorted-set rather than a sequence.
    /// Everything downstream that compares or walks a realized cell without
    /// re-seqing it then disagreed with the oracle (all measured):
    ///
    /// ```text
    /// (= (lazy-seq #{}) ())            false  (want true)
    /// (= (lazy-seq "abc") '(\a \b \c)) false  (want true)
    /// (lazy-seq (sorted-set 1 2))      threw  (want (1 2))
    /// (= (lazy-seq (into-array [1 2])) '(1 2)) false (want true)
    /// ```
    ///
    /// Going through `seq_items` -- the ONE conversion every other seq
    /// entry point already uses -- deletes the whitelist outright, so a
    /// newly seqable variant can never again be seqable everywhere EXCEPT
    /// inside `lazy-seq`, and the "not seqable" error becomes `seq_items`'s
    /// own ("don't know how to create a seq from X"), which is also what
    /// the JVM says here ("Don't know how to create ISeq from: ...").
    pub fn force(&mut self, v: &Value) -> Result<Value, RjError> {
        match v {
            Value::Lazy(cell) => {
                if let Some(realized) = crate::sync::lock_mutex(&cell.realized).as_ref() {
                    return Ok(realized.clone());
                }
                // P0c: interrupt point for native lazy-seq walks (`reduce`,
                // `count`, `dorun` over `range`/`iterate`/`repeat`). BEFORE the
                // thunk is taken, so an interrupt never poisons the cell.
                if self.intr.pending() {
                    self.interrupt_check()?;
                }
                let thunk_val = crate::sync::lock_mutex(&cell.thunk).take();
                let Some(thunk_val) = thunk_val else {
                    // No thunk and nothing realized: treat as an empty seq.
                    let empty = Value::Nil;
                    *crate::sync::lock_mutex(&cell.realized) = Some(empty.clone());
                    return Ok(empty);
                };
                // No real call-site span is available for an internal
                // thunk invocation; errors raised *inside* the thunk's own
                // body still carry their own accurate spans.
                let placeholder = Span { start: 0, end: 0 };
                let result = self.apply_value(&thunk_val, &[], placeholder)?;
                let forced = self.force(&result)?;
                let seqd = match &forced {
                    // `RT.seq` of an `ISeq` is that same `ISeq`, and of
                    // `nil` is `nil` -- no conversion, no realization. The
                    // `List` half is load-bearing beyond mere efficiency:
                    // a `lazy-seq` body that returns `(cons x (lazy-seq
                    // ...))` produces the improper-list encoding, and
                    // handing THAT to `seq_items` would materialize the
                    // whole (possibly infinite) chain right here.
                    //
                    // S7: an entry is seqable exactly like the vector it
                    // is, so a `lazy-seq` body that returns one realizes
                    // fine (`(lazy-seq (first {:a 1}))` => `(:a 1)`). A
                    // `Vector`/`MapEntry` is not an `ISeq` on the JVM, but
                    // mova's own `seq` of one is a same-elements `List`
                    // clone, so keeping the receiver is observationally
                    // identical and skips the copy.
                    Value::Nil | Value::List(_) | Value::Vector(_) | Value::MapEntry(_) => forced,
                    // Everything else goes through THE seq conversion, and
                    // an empty one realizes to `Nil` -- `RT.seq` of an
                    // empty collection is `nil` (measured: `(= (lazy-seq
                    // #{}) ())` is true and `(= (lazy-seq #{}) nil)` is
                    // false; `values_equal` keeps the second half honest
                    // via its `a_was_lazy` flag).
                    other => match self.seq_items(other)? {
                        None => Value::Nil,
                        Some(items) => Value::List(items),
                    },
                };
                *crate::sync::lock_mutex(&cell.realized) = Some(seqd.clone());
                Ok(seqd)
            }
            other => Ok(other.clone()),
        }
    }

    /// Seqs out `v`: `List`/`Vector` → their items, `Map` → `MapEntry`
    /// pairs, `Set` → its elements, `Str` → chars, `Nil`/empty → `None`,
    /// `Lazy` → forced then recursed.
    ///
    /// S7: this fn is THE map-entry construction choke point. Every
    /// map-ish arm below (`Map`, `SortedMap`, `HostStruct`, and the
    /// record arm further down) yields `Value::MapEntry`, not the plain
    /// `Value::Vector` it yielded before -- and because `seq`/`first`/
    /// `rest`/`next`/`uncons`/`materialize`/`reduce`/`doseq`/`into` all
    /// bottom out here, that one change is what makes `(map-entry?
    /// (first {:a 1}))`, `(class (first (map identity {:a 1})))` and
    /// `(reduce (fn [_ e] (map-entry? e)) nil m)` all match the oracle
    /// (measured: the JVM materializes a real `MapEntry` on ALL of those
    /// paths, including plain `reduce` over a `PersistentHashMap` -- only
    /// `reduce-kv` skips it; see `compat/mapentry-oracle-transcript2.txt`
    /// rows 003/007/011 and the note in this branch's report).
    pub fn seq_items(&mut self, v: &Value) -> Result<Option<crate::value::PVec>, RjError> {
        match v {
            // M6: callers walk by reference; hand them an uncached plain copy of a packed vector
            Value::Vector(items) | Value::List(items) if items.is_col() => {
                Ok(if items.is_empty() { None } else { Some(items.clone().into_iter().collect()) })
            }
            // S5/M3: reading a collection's elements sees through
            // metadata -- the elements of `(with-meta [1 2] {:a 1})` are
            // `1` and `2`, the same as any other vector's.
            Value::Meta(m) => {
                let inner = m.inner.clone();
                self.seq_items(&inner)
            }
            Value::Nil => Ok(None),
            // C11: a `List` is the ONE variant that can carry mova's
            // improper-list encoding (`[..data.., LazyTail]`), so it
            // gets its own arm and applies the shared rule --
            // `builtins::lazy_tail_split`, the single definition also used
            // by `uncons` and `materialize`. Without it this fn returned
            // the RAW slots, i.e. told its callers that a two-slot
            // `[head, Lazy]` cons cell has exactly two ELEMENTS, the
            // second being the whole unforced remainder. That is the
            // measured `~@` double-wrap bug (`` `(do ~@(map f xs)) `` =>
            // `(do (f x0) ((f x1) (f x2)))`); `qq_expand_seq` is this fn's
            // only caller that consumes the result as elements rather
            // than re-wrapping it into a seq.
            //
            // Realizing here is safe -- and is what "seq out `v`" has
            // always meant, since the `Option<PVec>` return type cannot
            // represent "...and a lazy tail" in the first place. Every
            // OTHER caller (`uncons`, `seq_of`, `cons_builtin`)
            // matches `List`/`Lazy` in an earlier arm and can only reach
            // this fn with a `Map`/`Set`/`Str`/`Array`/record-shaped
            // value, none of which is ever an improper list; the one
            // caller that could (`builtins::seq`'s `drop`, canonicalizing
            // a possibly-still-lazy tail) was switched to `seq_of`
            // in C11 precisely so it keeps NOT realizing.
            Value::List(items) => {
                if items.is_empty() {
                    Ok(None)
                } else if crate::builtins::lazy_tail_split(items).is_some() {
                    let all = crate::builtins::materialize(self, v)?;
                    if all.is_empty() {
                        Ok(None)
                    } else {
                        Ok(Some(all.into_iter().collect()))
                    }
                } else {
                    Ok(Some(items.clone()))
                }
            }
            // S7: an entry seqs out as its own two elements (measured:
            // `(seq (first {:a 1}))` is `(:a 1)`), i.e. exactly like the
            // vector it is -- so it rides the existing arm. C7: a
            // `Vector` never carries the improper-list marker, so neither
            // of these two ever needs the rule above (a `List` DOES --
            // see the C11 arm above; c10's Queue never carries it either).
            // C10: a queue seqs front-to-back (front == index 0) --
            // measured, `(seq (conj EMPTY 1 2 3))` is `(1 2 3)`; the
            // result is a plain seq, never another `Queue`.
            Value::Vector(items) | Value::MapEntry(items) | Value::Queue(items) => {
                if items.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(items.clone()))
                }
            }
            Value::Map(m) => {
                if m.is_empty() {
                    return Ok(None);
                }
                let items: crate::value::PVec = m
                    .iter()
                    .map(|(k, v)| Value::MapEntry(crate::value::PVec::pair(k.clone(), v.clone())))
                    .collect();
                Ok(Some(items))
            }
            // W3: THE choke point for `seq`/`uncons`/`materialize`/`cons`'s
            // fallback/`first`/`rest`/`next` (everything downstream of
            // `seq_items`) -- one arm here covers all of them. Shape order
            // (touch-only: reads `get_field` per entry, never
            // `host_struct::as_pmap`'s materialize -- matches `keys`/
            // `vals`/print's ordering guarantee, see that module's doc).
            Value::HostStruct(hs) => {
                if hs.shape.fields.is_empty() {
                    return Ok(None);
                }
                let items: crate::value::PVec = (0..hs.shape.fields.len())
                    .map(|idx| {
                        let k = Value::Keyword(Keyword::from(&hs.shape.fields[idx].key));
                        let v = crate::host_struct::get_field(hs, idx);
                        Value::MapEntry(crate::value::PVec::pair(k, v))
                    })
                    .collect();
                Ok(Some(items))
            }
            Value::LazyMap(lm) => {
                let m = crate::lazy_map::as_pmap(lm);
                if m.is_empty() {
                    return Ok(None);
                }
                let items: crate::value::PVec =
                    m.iter().map(|(k, v)| Value::MapEntry(crate::value::PVec::pair(k.clone(), v.clone()))).collect();
                Ok(Some(items))
            }
            Value::Set(s) => {
                if s.is_empty() {
                    return Ok(None);
                }
                Ok(Some(s.iter().cloned().collect()))
            }
            // S4: entries are already comparator-sorted incrementally (see
            // `value::SortedMapVal`/`SortedSetVal`'s doc) -- seq in THAT
            // order, no re-sort/re-hash needed (measured: `(seq (sorted-map
            // 3 :c 1 :a))` is `([1 :a] [3 :c])`, ascending).
            Value::SortedMap(m) => {
                if m.entries.is_empty() {
                    return Ok(None);
                }
                let items: crate::value::PVec = m
                    .entries
                    .iter()
                    .map(|(k, v)| Value::MapEntry(crate::value::PVec::pair(k.clone(), v.clone())))
                    .collect();
                Ok(Some(items))
            }
            Value::SortedSet(s) => {
                if s.entries.is_empty() {
                    return Ok(None);
                }
                Ok(Some(s.entries.iter().cloned().collect()))
            }
            // C2 (defstruct): `entries` is already in basis-then-ext order
            // (see `value::StructMapVal`'s doc) -- seq in THAT order, no
            // re-sort needed (measured: `(seq (struct s 1 2))` is `([:a 1]
            // [:b 2])`, basis order).
            Value::StructMap(m) => {
                if m.entries.is_empty() {
                    return Ok(None);
                }
                // S7 x C2 merge: a struct-map's entries are map entries
                // like any other map's -- measured on the oracle,
                // `(map-entry? (first (struct s 1 2)))` is `true` and
                // `(class ..)` is `clojure.lang.MapEntry` (the struct-map
                // itself is a `PersistentStructMap`, but its ENTRIES are
                // ordinary `MapEntry`s, same as a record's).
                let items: crate::value::PVec = m
                    .entries
                    .iter()
                    .map(|(k, v)| Value::MapEntry(crate::value::PVec::pair(k.clone(), v.clone())))
                    .collect();
                Ok(Some(items))
            }
            // S4: like `List`/`Vector` above (a typed vector is a plain
            // sequence of its already-coerced elements).
            Value::TypedVec(v) => {
                if v.data.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(v.data.clone()))
                }
            }
            // C7 (vecveneer): like `List`/`Vector`/`TypedVec` above -- a
            // `VecSeq`'s remaining elements, in order.
            Value::VecSeq(v) => {
                if v.items.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(v.items.clone()))
                }
            }
            Value::Str(s) => {
                if s.is_empty() {
                    return Ok(None);
                }
                // M8: `Str::chars_iter` walks a `Rope`'s chunks directly
                // (no intermediate materialize-to-`String` allocation) --
                // this still has to visit and collect every char into a
                // `Vec<Value>` either way (full seq realization is
                // inherently O(n), representation-independent), but skips
                // the extra full-document copy `Deref` would add first.
                Ok(Some(s.chars_iter().map(Value::Char).collect()))
            }
            // S4/1D (measured): `(seq (into-array []))` -> `nil`, `(seq
            // (into-array [1 2 3]))` -> `(1 2 3)` -- a snapshot clone of
            // the array's CURRENT contents, not a live view (mova has no
            // lazy-array-backed seq type; a `for`/`doseq` walking the
            // realized list won't see a concurrent `aset`, a documented
            // v1 simplification -- "tree-walk only is fine" per this
            // task's brief).
            Value::Array(arr) => {
                let data = crate::sync::lock_mutex(&arr.data);
                if data.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(data.iter().cloned().collect()))
                }
            }
            // C10: `java.util.ArrayList`/`HashMap`/`HashSet` veneer rows
            // -- seqable like their mova-native counterparts (a snapshot,
            // not a live view, same v1 simplification as `Array` above).
            // Order is whatever the backing `PVec`/`PMap`/CHAMP-set
            // iterates in; nothing in the vendored suite asserts a
            // specific `java.util.HashMap`/`HashSet` iteration order
            // (measured: real Java's own hash-bucket order isn't asserted
            // on either, only membership/count/map-entry?-ness).
            Value::HostInst(h) => {
                let guard = crate::sync::lock_mutex(&h.state);
                match &*guard {
                    crate::hostclass::HostState::ArrayList(items) => {
                        if items.is_empty() {
                            Ok(None)
                        } else {
                            Ok(Some(items.clone()))
                        }
                    }
                    crate::hostclass::HostState::HashMap(m) => {
                        if m.is_empty() {
                            Ok(None)
                        } else {
                            let items: crate::value::PVec = m
                                .iter()
                                .map(|(k, v)| Value::MapEntry(crate::value::PVec::pair(k.clone(), v.clone())))
                                .collect();
                            Ok(Some(items))
                        }
                    }
                    crate::hostclass::HostState::HashSet(s) => {
                        if s.is_empty() {
                            Ok(None)
                        } else {
                            Ok(Some(s.iter().cloned().collect()))
                        }
                    }
                    _ => Err(RjError::type_err(format!(
                        "don't know how to create a seq from {}",
                        h.kind.diagnostic_name()
                    ))
                    .with_class(JvmClass::IllegalArgument)
                    .with_stack(self.stack_snapshot(), self.source_id)),
                }
            }
            // C3e: a `LazyTail` is forced exactly like the `Lazy` it
            // wraps -- see `builtins::uncons`'s matching arm.
            Value::Lazy(_) | Value::LazyTail(_) => {
                let forced = self.force(v)?;
                self.seq_items(&forced)
            }
            // S3: records seq as `[k v]` entry pairs in basis-then-ext
            // order (measured: `(seq (->R 1 2))` is `([:a 1] [:b 2])`);
            // deftypes fall through to the error arm (measured: real
            // Clojure throws on `(count (T. 5))` -- no collection nature).
            Value::Inst(inst) if inst.tdef.is_record => {
                if inst.data.is_empty() {
                    return Ok(None);
                }
                let items: crate::value::PVec = inst
                    .ordered_entries()
                    .into_iter()
                    .map(|(k, v)| Value::MapEntry(crate::value::PVec::pair(k, v)))
                    .collect();
                Ok(Some(items))
            }
            // D1: a `reify`d `clojure.lang.IReduceInit` -- the ONE way a
            // non-record `Inst` can hand back elements. Measured surface:
            // `vectors.clj`'s `test-vec` (`(vec (reify IReduceInit
            // (reduce [_ f start] ...)))`) and `sequences.clj`'s
            // `test-into-IReduceInit` (`(into [] iri)`).
            //
            // Wired HERE rather than in `vec`/`into` separately because
            // `seq_items` is the one choke point every element-consuming
            // builtin already funnels through -- so `vec`, `into`,
            // `count`, `map`, ... all get it from a single arm, and this
            // arm is on the path that used to be a straight error, so no
            // existing shape pays anything for it.
            //
            // Reduction is `(.reduce inst conj [])`: the real
            // `IReduceInit` contract is exactly "fold my elements with
            // the fn and seed you give me", and `conj`-onto-a-vector is
            // the identity fold for that contract. A `reduced` early exit
            // is honored the same way `reduce` itself honors it (the
            // method returns the unwrapped value).
            Value::Inst(inst) if !inst.tdef.methods.is_empty() => {
                let Some(f) = inst.tdef.methods.get("reduce").cloned() else {
                    return Err(RjError::type_err(format!(
                        "don't know how to create a seq from {}",
                        inst.tdef.name
                    ))
                    .with_class(JvmClass::IllegalArgument)
                    .with_stack(self.stack_snapshot(), self.source_id));
                };
                let conj = Value::Native(std::sync::Arc::new(crate::value::NativeFn::new(
                    "clojure.lang.IReduceInit/conj".to_string(),
                    |interp: &mut Interp, args: &[Value]| {
                        crate::builtins::collections::conj_one(interp, &args[0], &args[1])
                    },
                )));
                let seed = Value::Vector(crate::value::PVec::new());
                let out =
                    self.apply_value(&f, &[v.clone(), conj, seed], Span { start: 0, end: 0 })?;
                match out {
                    Value::Reduced(inner) => self.seq_items(&inner),
                    other => self.seq_items(&other),
                }
            }
            // W3a: measured -- real Clojure's `RT.seqFrom` raises
            // `java.lang.IllegalArgumentException("Don't know how to create
            // ISeq from: <class>")` for every non-seqable receiver
            // (`(first 1)`, `(next :k)`, `(cons 1 2)`, `(get-in {:a 1} 5)`
            // all measured), NOT the `ClassCastException` mova's
            // `ErrorKind::TypeErr` otherwise maps to.
            other => Err(RjError::type_err(format!(
                "don't know how to create a seq from {}",
                other.type_name()
            ))
            .with_class(JvmClass::IllegalArgument)
            .with_stack(self.stack_snapshot(), self.source_id)),
        }
    }

    /// Pulls the pending recur args back out of an `ErrorKind::Recur`
    /// signal (shared by `loop` and closure self-recur). `e` is only ever
    /// constructed by `eval_recur`, so the malformed-signal branch should
    /// be unreachable in practice.
    pub(crate) fn recur_args(&self, e: &RjError) -> Result<Vec<Value>, RjError> {
        match &e.thrown {
            Some(Value::Vector(v)) | Some(Value::List(v)) => Ok(v.iter_cloned().collect()),
            _ => Err(RjError::other("internal: malformed recur signal")
                .with_span(e.span.unwrap_or(Span { start: 0, end: 0 }))
                .with_stack(self.stack_snapshot(), self.source_id)),
        }
    }

    /// Deep-realizes `v` for printing (`pr-str`/`str`/`println`/`prn`/
    /// `print`, and the REPL result printer): forces any `Lazy` chain
    /// ITERATIVELY into a concrete `List` -- O(1) Rust stack per chained
    /// element, using the same `uncons` cons-cell walk as `seq.rs`'s
    /// `materialize` -- and recurses into nested collection elements so
    /// e.g. `(map inc [1 2 3])` prints `(2 3 4)` instead of `#<lazy-seq>`.
    /// Guards against an infinite/huge lazy seq with a hard 100_000-element
    /// cap (shared across the whole realized value, not per chain), erroring
    /// with a clear message rather than hanging.
    pub fn realize_deep(&mut self, v: &Value) -> Result<Value, RjError> {
        let mut budget: usize = 0;
        self.realize_deep_bounded(v, &mut budget)
    }

    fn realize_deep_bounded(&mut self, v: &Value, budget: &mut usize) -> Result<Value, RjError> {
        const MAX_REALIZE: usize = 100_000;
        // Two shapes mean "this is a sequence with an unrealized rest,
        // walk it": a bare `Lazy` cell, and an improper list -- a `List`
        // whose last slot carries the `LazyTail` continuation marker.
        //
        // C3e: the second half is NEW here, and it is the printing half of
        // the improper-list leak. Under the old encoding this entry point
        // could not tell a cons cell from a genuine user-built list whose
        // last element just happens to be an unrealized lazy seq (e.g.
        // `(list 1 (range 3))`), so it deliberately chose "data" -- which
        // made `(pr-str (cons 1 (range 3)))` print `"(1 (0 1 2))"` instead
        // of `"(1 0 1 2)"` (the tail's elements nested one level too
        // deep). The marker settles the question, so both shapes now print
        // as Clojure prints them.
        let improper = matches!(v, Value::List(items) if crate::builtins::lazy_tail_split(items).is_some());
        if improper || matches!(v, Value::Lazy(_)) {
            let mut items: crate::value::PVec = crate::value::PVec::new();
            let mut cur = v.clone();
            while let Some((h, t)) = crate::builtins::uncons(self, &cur)? {
                *budget += 1;
                if *budget > MAX_REALIZE {
                    return Err(RjError::other(
                        "realizing infinite/huge lazy seq for printing (capped at 100000 elements)",
                    )
                    .with_stack(self.stack_snapshot(), self.source_id));
                }
                items.push_back(self.realize_deep_bounded(&h, budget)?);
                cur = t;
            }
            return Ok(Value::List(items));
        }
        match v {
            Value::List(items) => {
                let mut out = crate::value::PVec::new();
                for it in items {
                    out.push_back(self.realize_deep_bounded(it, budget)?);
                }
                Ok(Value::List(out))
            }
            Value::Vector(items) if items.is_col() => {
                let mut out = crate::value::PVec::new();
                for it in items.clone() {
                    out.push_back(self.realize_deep_bounded(&it, budget)?);
                }
                Ok(Value::Vector(out))
            }
            Value::Vector(items) => {
                let mut out = crate::value::PVec::new();
                for it in items {
                    out.push_back(self.realize_deep_bounded(it, budget)?);
                }
                Ok(Value::Vector(out))
            }
            // S7: deep-realizing an entry's slots must NOT demote it to a
            // plain vector -- `(pr-str (seq {:a (map inc [1])}))` walks
            // through here, and the printed shape is the same either way,
            // but `(class (first (seq m)))` after a `doall` is not.
            // Rebuilt with `PVec::pair`, so the two-slot invariant holds
            // by construction.
            Value::MapEntry(items) => {
                let k = self.realize_deep_bounded(&items[0], budget)?;
                let val = self.realize_deep_bounded(&items[1], budget)?;
                Ok(Value::MapEntry(crate::value::PVec::pair(k, val)))
            }
            Value::Map(m) => {
                let mut out = PMap::new();
                for (k, val) in m.iter() {
                    let k2 = self.realize_deep_bounded(k, budget)?;
                    let v2 = self.realize_deep_bounded(val, budget)?;
                    out.insert(k2, v2);
                }
                Ok(Value::Map(out))
            }
            Value::Set(s) => {
                let mut out = champ::PersistentHashSet::new().transient();
                for it in s.iter() {
                    out.insert(self.realize_deep_bounded(it, budget)?);
                }
                Ok(Value::Set(out.persistent()))
            }
            // C3e: realize THROUGH metadata and re-attach it. Without this
            // arm a metadata-carrying lazy seq fell to the catch-all and
            // printed as `#<lazy-seq>` -- measured, `(with-meta (range 3)
            // {:a 1})` printed `#<lazy-seq>` where the oracle prints
            // `(0 1 2)`. Re-attaching (rather than dropping) keeps
            // `realize_deep` a pure realization: it is also what
            // `sorted::hash_value` runs on, and metadata is invisible to
            // `hash` either way.
            Value::Meta(m) => {
                let inner = self.realize_deep_bounded(&m.inner, budget)?;
                Ok(Value::attach_meta(inner, m.meta.clone()))
            }
            // clojure-lsp campaign (mova/PLAN.md): a `defrecord`/`deftype`
            // field holding an unrealized (or only single-step-cached)
            // lazy seq -- e.g. `rewrite-clj.parser`'s own `(->FormsNode
            // (->> (repeatedly ...) (take-while identity)))`, whose
            // `first`/`last` calls (computing the forms-node's position
            // metadata) partially force the seq WITHOUT flattening it.
            // Every other collection arm above realizes through its own
            // elements; a record/deftype's fields were the one shape that
            // fell to the catch-all below, leaving the printer's own
            // `Value::Lazy` branch (an "internal diagnostic path", not
            // meant for user-facing output -- see its doc) to render a
            // partially-forced chain as literally-nested cons pairs
            // instead of a flat list. Measured real bug: `(pr-str (->R
            // (take-while identity (repeatedly ...))))` printed
            // `{:children (1 (2 nil))}` instead of `{:children (1 2)}`.
            //
            // Builds a FRESH `InstVal` rather than mutating the original
            // in place (matching every other arm's "realize is a pure
            // copy, not a side effect on the live value" contract, and
            // keeping a `deftype`'s `Arc`-identity equality untouched for
            // the value the program actually holds -- only this
            // printing/hashing-only copy differs).
            Value::Inst(inst) => {
                let mut data = PMap::new();
                for (k, v) in inst.data.iter() {
                    data.insert(k.clone(), self.realize_deep_bounded(v, budget)?);
                }
                let mut fields = crate::value::PVec::new();
                for v in crate::sync::lock_mutex(&inst.fields).iter() {
                    fields.push_back(self.realize_deep_bounded(v, budget)?);
                }
                Ok(Value::Inst(std::sync::Arc::new(crate::types::InstVal {
                    tdef: inst.tdef.clone(),
                    data,
                    fields: std::sync::Mutex::new(fields),
                    meta: inst.meta.clone(),
                })))
            }
            // clojure-lsp campaign (mova/PLAN.md): `SortedMap`/`SortedSet`
            // fell to the catch-all like `Inst` used to (same bug class,
            // same fix) -- a sorted-map value holding an unforced `Lazy`
            // (e.g. clj-kondo's `(assoc finding :langs (keep :lang fs))`
            // built via `into (sorted-map) ...`) printed `#<lazy-seq>`
            // instead of realizing. Measured: `(pr-str (into (sorted-map)
            // {:langs (keep :lang [])}))` printed `{:langs #<lazy-seq>}`.
            Value::SortedMap(m) => {
                let mut entries = Vec::with_capacity(m.entries.len());
                for (k, v) in m.entries.iter() {
                    let k2 = self.realize_deep_bounded(k, budget)?;
                    let v2 = self.realize_deep_bounded(v, budget)?;
                    entries.push((k2, v2));
                }
                Ok(Value::SortedMap(std::sync::Arc::new(crate::value::SortedMapVal { cmp: m.cmp.clone(), entries })))
            }
            Value::SortedSet(s) => {
                let mut entries = Vec::with_capacity(s.entries.len());
                for it in s.entries.iter() {
                    entries.push(self.realize_deep_bounded(it, budget)?);
                }
                Ok(Value::SortedSet(std::sync::Arc::new(crate::value::SortedSetVal { cmp: s.cmp.clone(), entries })))
            }
            other => Ok(other.clone()),
        }
    }

    /// `=`-semantics equality: forces `Lazy` on both sides, blends
    /// `Int`/`Float` numeric equality, and recurses through `List`/
    /// `Vector`/`Map` so nested laziness/numeric mixing still compares
    /// correctly. Falls back to the pure `PartialEq` impl elsewhere (it
    /// already implements everything else: identity for fns/atoms,
    /// structural equality for sets/strings/etc).
    ///
    /// Sequence semantics (measured on 1.13.0-alpha6): a lazy chain --
    /// the `[e0.., Lazy-tail]` improper-list encoding described in
    /// collections.rs's module doc -- compares element-by-element against
    /// any sequential (list/vector/other chain), so `(= (map inc [1 2])
    /// '(2 3))` is true; an EMPTY lazy seq equals `()`/`[]` but NOT `nil`
    /// (`(= (lazy-seq nil) '())` true, `(= (lazy-seq nil) nil)` false),
    /// which is why forcing below must remember it started from `Lazy`
    /// rather than letting `force`'s Nil collapse erase the distinction.
    pub fn values_equal(&mut self, a: &Value, b: &Value) -> Result<bool, RjError> {
        // C3e: the `Meta(Lazy)` arms are NEW. This match tested the
        // WRAPPER for `Lazy`-ness, so a metadata-carrying lazy seq was
        // never forced and fell through to the structural fallback, which
        // compares a `List` against a `Meta` and says no. Measured:
        // `(= (range 10) (with-meta (range 10) {:a 1}))` was `false`
        // (want `true`), in both directions and for every lazy producer.
        // Metadata is invisible to `=` at every depth, so forcing THROUGH
        // it is simply what the non-meta arm already does.
        //
        // Deliberately an extra ARM rather than an `unmeta()` call on the
        // scrutinee: every value that is neither `Lazy` nor `Meta` --
        // i.e. every value on every hot `=` path -- still reaches the
        // same `other => (other.clone(), false)` after the same
        // discriminant dispatch, and a `Meta` wrapping a NON-lazy still
        // falls to `other` and keeps its wrapper, exactly as before (its
        // unwrapping stays `Value::PartialEq`'s job, unchanged).
        let (a, a_was_lazy) = match a {
            Value::Lazy(_) => (self.force(a)?, true),
            Value::Meta(m) if matches!(m.inner, Value::Lazy(_)) => (self.force(&m.inner)?, true),
            other => (other.clone(), false),
        };
        let (b, b_was_lazy) = match b {
            Value::Lazy(_) => (self.force(b)?, true),
            Value::Meta(m) if matches!(m.inner, Value::Lazy(_)) => (self.force(&m.inner)?, true),
            other => (other.clone(), false),
        };
        match (&a, &b) {
            // S5 (SPEC-numtower) RETIRED the Int/Float blend that used to
            // live here (`*x as f64 == *y`, the old DEVIATIONS.md
            // `numbers.corpus:29` entry). Real Clojure's `=` on two
            // numbers is CATEGORY-STRICT -- `clojure.lang.Numbers.equal`
            // returns false outright unless both operands are in the same
            // `Category` (INTEGER / FLOATING / DECIMAL / RATIO) -- so
            // `(= 1 1.0)` is false (measured), and the cross-category
            // numeric comparison lives in `==` instead. The one surviving
            // cross-TYPE numeric bridge is inside the INTEGER category
            // (`Int`/`BigInt`/`BigInteger`), and that one is in `Value`'s
            // own `eq_inner`, so nothing numeric needs an arm here at all
            // any more.
            // The FLOATING category's own rule, and it is NOT `Value`'s
            // `PartialEq` (which compares raw BITS): Clojure's
            // `DoubleOps.equiv` is `x.doubleValue() == y.doubleValue()`,
            // i.e. plain IEEE-754. Measured, and both halves are
            // surprising in opposite directions: `(= 0.0 -0.0)` is TRUE
            // (the two zeros are numerically equal even though their bits
            // differ) and `(= ##NaN ##NaN)` is FALSE (NaN is equal to
            // nothing, itself included). Because this arm recurses --
            // `values_equal` is what the `List`/`Vector`/`Map` arms below
            // call per element -- it also fixes `(= [0.0] [-0.0])` (true,
            // measured) and everything defined in terms of `=`, `distinct`
            // included.
            //
            // KEY LOOKUP still uses `Value`'s bitwise `PartialEq`/`Hash`:
            // `(contains? #{0.0} -0.0)` is true in Clojure and false in
            // mova. Closing that would mean giving `Value` a NON-REFLEXIVE
            // `Eq` (a NaN key must never find itself -- measured
            // `(get {##NaN :a} ##NaN)` is `nil` and `(count (set [##NaN
            // ##NaN]))` is `2`), which breaks the contract `imbl`'s
            // HashMap/HashSet are built on. See
            // tests/conformance/DEVIATIONS.md's "S5 float identity in
            // collection keys".
            (Value::Float(x), Value::Float(y)) => Ok(x == y),
            // An empty lazy seq forced to `Nil`: equal to an empty
            // sequential, never to a bare `nil` (and two empty lazies are
            // equal to each other). Plain `nil` vs `nil` stays true via
            // the same arm because both `_was_lazy` flags are false.
            (Value::Nil, Value::Nil) => Ok(a_was_lazy == b_was_lazy),
            // S7: `MapEntry` rides every sequence-equality arm here for
            // the same reason it rides `value::PartialEq`'s -- an entry is
            // `=` to the equivalent vector/list, measured both ways.
            (Value::Nil, Value::List(ys) | Value::Vector(ys) | Value::MapEntry(ys) | Value::Queue(ys)) => {
                Ok(a_was_lazy && ys.is_empty())
            }
            (Value::List(xs) | Value::Vector(xs) | Value::MapEntry(xs) | Value::Queue(xs), Value::Nil) => {
                Ok(b_was_lazy && xs.is_empty())
            }
            // C10: `TypedVec`'s payload is a `TypedVecVal{kind, data}`
            // struct, not a bare `PVec` like its four siblings above, so
            // it can't join their pattern directly -- separate arms,
            // same "was this side a forced-empty lazy" rule (measured:
            // `(= (lazy-seq) (vector-of :long))` is `true`, matching
            // `ordered-collection-equality-test`'s `empty-colls` set,
            // which mixes a `(vector-of :long)` in with `[]`/`'()`/
            // `(lazy-seq)`/a queue).
            (Value::Nil, Value::TypedVec(tv)) => Ok(a_was_lazy && tv.data.is_empty()),
            (Value::TypedVec(tv), Value::Nil) => Ok(b_was_lazy && tv.data.is_empty()),
            // C10: `Queue` rides every sequence-equality arm here too --
            // measured both ways, `(= (conj EMPTY 1 2 3) '(1 2 3))` and
            // `(= [1 2 3] (conj EMPTY 1 2 3))` are both `true`. Same for
            // `TypedVec` cross-comparisons (measured: `(= (vector-of
            // :long 1 2 3) (conj EMPTY 1 2 3))` is `true`) -- reuses
            // `Value`'s own `PartialEq` for the `TypedVec`-involving
            // pairs (already correct, see that impl's `(TypedVec(a),
            // List(b) | Vector(b) | MapEntry(b) | Queue(b))` arm) rather
            // than re-deriving elementwise comparison a third time.
            (Value::TypedVec(_), Value::List(_) | Value::Vector(_) | Value::MapEntry(_) | Value::Queue(_))
            | (Value::List(_) | Value::Vector(_) | Value::MapEntry(_) | Value::Queue(_), Value::TypedVec(_))
            | (Value::TypedVec(_), Value::TypedVec(_)) => Ok(a == b),
            (
                Value::List(xs) | Value::Vector(xs) | Value::MapEntry(xs) | Value::Queue(xs),
                Value::List(ys) | Value::Vector(ys) | Value::MapEntry(ys) | Value::Queue(ys),
            ) => {
                // A lazy tail means structural length is meaningless
                // (`[2, LazyTail]` may encode any logical length >= 1), so
                // peel both sides seq-wise instead of zipping. C3e: the
                // duplicated `has_lazy_tail` predicate this used to call
                // is gone -- `lazy_tail_split` is the one definition of
                // the rule, and it now answers on the MARKER rather than
                // on "last slot happens to be a `Lazy`", so
                // `(= (seq [:x (range 2 5)]) '(:x (2 3 4)))` compares two
                // 2-element sequences instead of peeling the range open.
                if crate::builtins::lazy_tail_split(xs).is_some()
                    || crate::builtins::lazy_tail_split(ys).is_some()
                {
                    return self.seqs_equal(&a, &b);
                }
                // M13 (SPEC-M13-EQWITH.md): when both sides are already
                // `PVec::Big`, route through champ's `try_eq_by`
                // instead of the manual `zip` walk below. `try_eq_by`
                // does its own `len` check (Ok(false) on mismatch) and,
                // more importantly, prunes any pointer-shared subtree --
                // the predicate (this same `values_equal`, recursing) is
                // never called on a shared subtree's elements. Measured
                // motivation (SPEC-M13-EQWITH.md): the generic `zip` walk
                // below is O(n) even when the two vectors are
                // 99.999% structurally shared (compare-after-edit, the
                // pervasive editor/reactive pattern) -- this is the ONLY
                // way to actually see that prune from script `=`.
                //
                // Small/mixed pairs fall through unchanged: `PVec::Small`
                // has no trie to prune, so the manual walk below is
                // already optimal for them.
                if let (Some(xb), Some(yb)) = (xs.as_big(), ys.as_big()) {
                    return xb.try_eq_by(yb, |x, y| self.values_equal(x, y));
                }
                if xs.len() != ys.len() {
                    return Ok(false);
                }
                // M6: packed vectors compare by owned elements (no materialize)
                if xs.is_col() || ys.is_col() {
                    for (x, y) in xs.clone().into_iter().zip(ys.clone()) {
                        if !self.values_equal(&x, &y)? {
                            return Ok(false);
                        }
                    }
                    return Ok(true);
                }
                for (x, y) in xs.iter().zip(ys.iter()) {
                    if !self.values_equal(x, y)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            (Value::Map(m1), Value::Map(m2)) => self.maps_equal(m1, m2),
            // W3: a `HostStruct` on either (or both) side gets the SAME
            // blended-numeric recursive comparison a `Map`/`Map` pair
            // gets above (delegating through `host_struct::as_pmap`), not
            // the plain structural `PartialEq` fallback below -- otherwise
            // `(= host-struct real-map)` could disagree with what
            // `(= (into {} host-struct) real-map)` reports whenever a
            // numeric field's type differs (`Int` vs `Float`) between the
            // two, purely because this arm was skipped. Kept as separate
            // arms (not folded into the `Map`/`Map` one above) so the
            // hot, non-embed `Map`/`Map` path never pays an extra
            // `match`/branch for a variant it can never see -- required
            // by the W3 tie-check (`bench/optimization-log.md`).
            (Value::Map(m1), Value::HostStruct(hs)) | (Value::HostStruct(hs), Value::Map(m1)) => {
                self.maps_equal(m1, crate::host_struct::as_pmap(hs))
            }
            (Value::HostStruct(hs1), Value::HostStruct(hs2)) => {
                self.maps_equal(crate::host_struct::as_pmap(hs1), crate::host_struct::as_pmap(hs2))
            }
            (Value::Map(m1), Value::LazyMap(lm)) | (Value::LazyMap(lm), Value::Map(m1)) => {
                self.maps_equal(m1, crate::lazy_map::as_pmap(lm))
            }
            (Value::LazyMap(l1), Value::LazyMap(l2)) => {
                self.maps_equal(crate::lazy_map::as_pmap(l1), crate::lazy_map::as_pmap(l2))
            }
            _ => Ok(a == b),
        }
    }

    /// C3e: canonicalizes a value on its way into (or up against) a
    /// hash-keyed position -- a `Map` key, a `Set` element, a `get`/
    /// `contains?`/`find`/`disj`/`dissoc` probe.
    ///
    /// # Why any normalization is needed
    ///
    /// Hash lookup runs on `Value`'s own `PartialEq`/`Hash`, which are pure
    /// trait impls with no `&mut Interp` and therefore cannot force a
    /// thunk. An UNREALIZED `Value::Lazy` consequently hashes by `Arc`
    /// POINTER (see that arm in `value::Hash`), so it never finds an
    /// `=`-equal list key -- measured, `(get {(repeat 1 :x) :z} '(:x))`
    /// was `nil` and `(= {(repeat 1 :x) :z} {'(:x) :z})` was `false`. Real
    /// Clojure has no such hole because `LazySeq.hashCode` realizes the
    /// seq; this is that realization, moved to the boundary where an
    /// `&mut Interp` is actually in hand.
    ///
    /// # Why it is exactly this narrow
    ///
    /// ONLY a bare `Lazy` is touched, and it is replaced by the very seq
    /// its own `hash`/`=` would report once realized -- so this changes
    /// nothing about which values are `=`, it only lets the hash agree.
    /// In particular it deliberately does NOT go near the `Float` half of
    /// the key-lookup gap flagged in `values_equal` (`(contains? #{0.0}
    /// -0.0)`, `##NaN` keys): closing THAT would require a non-reflexive
    /// `Eq` on `Value`, which would break the contract the backing
    /// HashMap/HashSet are built on. See DEVIATIONS.md.
    ///
    /// Cost: one discriminant compare on values that are not `Lazy`, which
    /// is every key on every hot path (keywords, strings, ints). The
    /// `get`/`contains?` probes go further and pay even that only after a
    /// MISS, so a successful lookup is untouched.
    /// `#[inline]` is load-bearing, not decoration: this sits on the
    /// map-literal insert path (`{:c c2}` per message in a flow step-fn --
    /// see `map_probe`'s "hash-map (literal)" touch site), and inlining is
    /// what makes the non-`Lazy` case a single discriminant compare next
    /// to the insert rather than a function CALL per key.
    #[inline]
    pub(crate) fn normalize_key(&mut self, k: Value) -> Result<Value, RjError> {
        if matches!(k, Value::Lazy(_)) {
            // `realize_deep`, not `force`: a lazy chain forces to an
            // improper list whose own tail is still a thunk, and it is the
            // fully-walked flat list that `hash`/`=` compare. This is the
            // same conversion `builtins::sorted::hash_value` (the `hash`
            // BUILTIN) already applies for the same reason.
            return self.realize_deep(&k);
        }
        Ok(k)
    }

    /// C3e: `Object.equals` semantics, as opposed to [`values_equal`]'s
    /// `clojure.core/=` (`Util.equiv`) semantics -- what `.equals` must
    /// use, and what mova's universal `.equals` dot-method veneer used to
    /// get wrong by delegating straight to `=`.
    ///
    /// The ONE difference is at a NUMERIC leaf, where the JVM's boxed
    /// numbers are class-strict (`Long.equals` returns false for anything
    /// that is not a `Long`) while Clojure's `=` bridges the whole INTEGER
    /// category. Measured:
    ///
    /// ```text
    /// (= 3 3N)                       true    (.equals 3 3N)          false
    /// (= (seq [3]) (seq [3N]))       true    (.equals .. ..)         false
    /// (.equals [3] [3])              true    (.equals [3] [3.0])     false
    /// ```
    ///
    /// Collection CLASSES are deliberately not strict: `APersistentVector.
    /// equals`/`ASeq.equals` accept any `Sequential`/`java.util.List` on
    /// the other side and then compare elements with `Util.equals`, so
    /// `(.equals [3] '(3))` is `true` (measured). Hence: recurse through
    /// the sequential spine, tighten only the leaves, and delegate
    /// everything else (strings, keywords, nil, maps, sets, identity-typed
    /// cells) to `values_equal`, which already gives the same answer for
    /// them.
    ///
    /// Known remaining laxity, both needing a representation mova does not
    /// have and neither measured by the vendored suite: `(int 3)` is a
    /// `Value::Int` like any other `Long`, so `(.equals (int 3) 3)` stays
    /// `true` where the JVM says `false` (`Integer` vs `Long`); and
    /// `BigDecimal.equals` is scale-SENSITIVE on the JVM
    /// (`1.5M.equals(1.50M)` is false) while `Value::BigDec` compares
    /// scale-insensitively.
    pub(crate) fn values_equal_strict(&mut self, a: &Value, b: &Value) -> Result<bool, RjError> {
        let (a, b) = (a.unmeta(), b.unmeta());
        // Numeric leaf: same boxed CLASS or nothing. `discriminant` is
        // exactly the right granularity here -- `Int`/`Float`/`BigInt`/
        // `BigInteger`/`Ratio`/`BigDec` are one variant per JVM class.
        if is_boxed_number(a) && is_boxed_number(b) {
            if std::mem::discriminant(a) != std::mem::discriminant(b) {
                return Ok(false);
            }
            return self.values_equal(a, b);
        }
        // Sequential spine: same walk `seqs_equal` does, with the strict
        // leaf comparison. Cheap pre-check first, so nothing but an
        // actual seq/seq pair pays for the peel.
        if is_sequential_for_equals(a) && is_sequential_for_equals(b) {
            let mut ca = a.clone();
            let mut cb = b.clone();
            loop {
                return match (
                    crate::builtins::uncons(self, &ca)?,
                    crate::builtins::uncons(self, &cb)?,
                ) {
                    (None, None) => Ok(true),
                    (Some((ha, ta)), Some((hb, tb))) => {
                        if !self.values_equal_strict(&ha, &hb)? {
                            return Ok(false);
                        }
                        ca = ta;
                        cb = tb;
                        continue;
                    }
                    _ => Ok(false),
                };
            }
        }
        self.values_equal(a, b)
    }

    /// Element-by-element sequential equality via `uncons` peeling: the
    /// only correct comparison once either side carries a lazy tail,
    /// because the structural `len()` of a `[e0.., Lazy]` chain says
    /// nothing about its logical length. O(1) Rust stack per element
    /// (loop, not recursion, on the spine); terminates as soon as either
    /// side ends, so a finite seq compares against an infinite one in
    /// finite time exactly like Clojure.
    fn seqs_equal(&mut self, a: &Value, b: &Value) -> Result<bool, RjError> {
        let mut ca = a.clone();
        let mut cb = b.clone();
        loop {
            match (
                crate::builtins::uncons(self, &ca)?,
                crate::builtins::uncons(self, &cb)?,
            ) {
                (None, None) => return Ok(true),
                (Some((ha, ta)), Some((hb, tb))) => {
                    if !self.values_equal(&ha, &hb)? {
                        return Ok(false);
                    }
                    ca = ta;
                    cb = tb;
                }
                _ => return Ok(false),
            }
        }
    }

    /// Shared blended-numeric recursive map comparison -- see
    /// `values_equal`'s `Map`/`Map` and `HostStruct`-involving arms, both
    /// of which delegate here so they can never drift against each other.
    fn maps_equal(&mut self, m1: &PMap, m2: &PMap) -> Result<bool, RjError> {
        if m1.len() != m2.len() {
            return Ok(false);
        }
        for (k, v) in m1.iter() {
            match m2.get(k) {
                Some(v2) => {
                    if !self.values_equal(v, v2)? {
                        return Ok(false);
                    }
                }
                None => return Ok(false),
            }
        }
        Ok(true)
    }
}

impl Default for Interp {
    fn default() -> Self {
        Self::new()
    }
}

/// C3e: the boxed-number leaves `Interp::values_equal_strict` compares
/// class-strictly. One variant per JVM class (`Long`/`Double`/
/// `clojure.lang.BigInt`/`BigInteger`/`Ratio`/`BigDecimal`), which is what
/// makes `discriminant` the right comparison there.
#[inline]
fn is_boxed_number(v: &Value) -> bool {
    matches!(
        v,
        Value::Int(_)
            | Value::Float(_)
            | Value::BigInt(_)
            | Value::BigInteger(_)
            | Value::Ratio(_)
            | Value::BigDec(_)
    )
}

/// C3e: the shapes whose `.equals` walks a spine of elements. Mirrors
/// `predicates`'s `sequential?` plus the seq-producing cells `uncons`
/// already accepts, and deliberately EXCLUDES maps/sets (their `.equals`
/// has its own contract -- see `values_equal_strict`'s doc).
#[inline]
fn is_sequential_for_equals(v: &Value) -> bool {
    matches!(
        v,
        Value::List(_)
            | Value::Vector(_)
            | Value::MapEntry(_)
            | Value::Queue(_)
            | Value::TypedVec(_)
            | Value::VecSeq(_)
            | Value::Lazy(_)
    )
}

/// True if `form` is a symbol atom (used pervasively for binding targets:
/// `let`/`loop`/`fn` params, `def`/`defmacro` names).
fn form_as_symbol(form: &Form) -> Option<&Symbol> {
    if let FormValue::Atom(Value::Sym(s)) = &form.value {
        Some(s)
    } else {
        None
    }
}

#[cfg(test)]
mod tests;

/// K1: `Vec::clear` minus the out-of-line drop call for scalar slots (moved-out args are `Nil`).
#[inline]
pub(crate) fn clear_values(buf: &mut Vec<Value>) {
    for v in buf.iter_mut() {
        if !matches!(v, Value::Nil | Value::Bool(_) | Value::Int(_) | Value::Float(_)) {
            // SAFETY: each non-scalar element is dropped exactly once; `set_len(0)` below forgets all.
            unsafe { std::ptr::drop_in_place(v) }
        }
    }
    // SAFETY: every element was either dropped above or is a scalar with no drop glue.
    unsafe { buf.set_len(0) }
}
