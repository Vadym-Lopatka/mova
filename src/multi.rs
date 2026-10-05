//! S4: `defmulti`/`defmethod` and the derivation-hierarchy machinery
//! (`derive`/`underive`/`isa?`/`parents`/`ancestors`/`descendants`/
//! `make-hierarchy`). Measured on 1.13.0-alpha6 (see `compat/
//! multimethods-probe.clj` and `compat/multimethods-probe2.clj` and their
//! committed `.out` transcripts, plus `tests/clojure-suite/vendor/
//! multimethods.clj`, the ACTUAL suite this unblocks).
//!
//! ## Hierarchy representation
//!
//! A hierarchy is an ordinary Clojure value (a map) so it round-trips
//! through `def`/`=`/`vals`/`keys` exactly the way the vendored suite's own
//! `hierarchy-tags` helper needs (`(reduce into #{} (map keys (vals h)))`
//! walks `(vals h)`, i.e. requires `h` to literally be `{:parents {...}
//! :ancestors {...} :descendants {...}}`, not an opaque Rust struct) --
//! measured shape, matching real Clojure's own internal representation:
//! `{:parents {tag #{direct-parents}}, :ancestors {tag #{transitive
//! ancestors}}, :descendants {tag #{transitive descendants}}}`. A tag with
//! an empty set is simply ABSENT from the relevant sub-map (measured via
//! `compat/multimethods-probe2.clj`'s q09: a parent-less tag has no
//! `:parents`/`:ancestors` entry, a childless tag has no `:descendants`
//! entry) -- `parents`/`ancestors`/`descendants` return `nil`, never `#{}`,
//! for an absent tag.
//!
//! `:ancestors`/`:descendants` are fully recomputed (BFS over `:parents`)
//! on every `derive`/`underive` rather than incrementally patched --
//! correctness-first: hierarchies in test corpora are tiny, and incremental
//! maintenance is where real implementations grow subtle bugs. This keeps
//! `isa?`/`parents`/`ancestors`/`descendants` queries O(1) (map lookup),
//! paying the O(n) recompute only at mutation time.
//!
//! ## Global vs. custom hierarchy
//!
//! The GLOBAL hierarchy is an ordinary global VAR, `clojure.core/
//! global-hierarchy` (matching real Clojure's own var name exactly, not a
//! side field on `Interp`), bound to a PLAIN hierarchy MAP -- measured on
//! the real oracle: `(class (var-get #'clojure.core/global-hierarchy))`
//! is `clojure.lang.PersistentArrayMap`, NOT an atom, and the global
//! 2-arity `derive`/`underive` mutate it via `alter-var-root` directly on
//! the var. This is what makes the vendored suite's `global-hierarchy-
//! test` work unmodified: it wraps its body in `(with-var-roots
//! {#'clojure.core/global-hierarchy (make-hierarchy)} ...)`, which only
//! works at all if that var exists and is resolvable, and temporarily
//! swapping its ROOT is exactly what `with-var-roots`/`with-redefs` do
//! (and exactly why it must NOT be an atom: swapping the var's root to a
//! bare `(make-hierarchy)` map, as that call does, would silently stop
//! being visible to a write path that expected to find an atom to
//! `swap!` instead). Living in `globals` (shared across
//! `fork` already, exactly like `protocols`/`multimethods`) means no
//! separate `Interp` field or fork/snapshot wiring is needed for it at
//! all. A custom hierarchy passed as `defmulti`'s `:hierarchy` option is
//! stored as the RAW option value (an atom, a var, or -- measured in the
//! vendored suite's `indirect-3`/`indirect-4` tests -- a var pointing
//! directly at a plain hierarchy MAP, no atom at all) and re-dereferenced
//! on EVERY dispatch call via [`resolve_hierarchy_value`] -- no caching, so
//! a `swap!`/`alter-var-root` on the referenced hierarchy after `defmulti`
//! is immediately visible to dispatch (measured: `compat/multimethods-
//! probe.clj`'s p59-p64). The GLOBAL hierarchy's default (`defmulti` with
//! no `:hierarchy` option) is represented internally the same way: `Some(
//! Value::Var(global-hierarchy's cell))`, resolved through the identical
//! re-deref-every-call path -- no `None`/"use the global" special case.
//!
//! ## Multimethod registry
//!
//! Keyed by the dispatch-fn's own `Arc<NativeFn>` pointer identity (NOT the
//! var cell, unlike `Protocols`): `defmethod`/`methods`/`prefers`/
//! `remove-method`/... all receive the multimethod as an ordinary evaluated
//! `Value` (ordinarily `Value::Native`, since real Clojure code never binds
//! `#'the-multi` when calling these -- it passes the deref'd MultiFn
//! object), so the registry key must be recoverable from that bare value
//! alone. `defmulti`'s own defonce-like no-redefinition check (measured:
//! `compat/multimethods-probe.clj` p09/p10 -- a second `defmulti` on an
//! already-multimethod var is a silent no-op returning `nil`, keeping the
//! OLD dispatch-fn/hierarchy/default) reads the var's CURRENT value and
//! checks registry membership by that same pointer.
//!
//! ## Dispatch caching (W-MULTI)
//!
//! Real Clojure's `MultiFn` caches the best-method resolution per dispatch
//! value (`findAndCacheBestMethod`), invalidated on any method/hierarchy
//! change. Before this, every mova multimethod call paid TWO registry
//! `RwLock` reads plus an `O(methods)` linear `isa?`/`dominates` scan on
//! every call -- field-measured at 1.7x a plain fn (real Clojure's own
//! cached dispatch is close to protocol-IC parity). [`MultiCache`]
//! (per-`MultiDef`, see that type) closes this: generation-stamped so ANY
//! mutation -- `defmethod`, `remove-method`, `prefer-method`, or a global-
//! hierarchy `derive`/`underive` -- forces the next call back onto the
//! fresh path, INCLUDING a mutation the dispatch fn itself performs
//! mid-call (the measured contract this module's registry-read discipline
//! already had to honor: `defmethod` executed BY the dispatch fn must be
//! visible to that SAME call's method resolution, which is why the
//! methods/prefers snapshot -- "READ#2" in [`make_dispatch_native`] --
//! happens AFTER calling the dispatch fn, not before). The cache sits
//! entirely UNDER that contract: a cache hit is only ever taken when the
//! generation counter proves nothing moved between grabbing the pre-call
//! snapshot and the dispatch fn returning, at which point skipping the
//! registry re-read and the linear scan is provably safe, not merely
//! probably safe.
//!
//! **Scoping: global-hierarchy multimethods only** (`hierarchy_ref ==
//! None`). A custom `:hierarchy` option is commonly an ATOM (or a var
//! pointing at one), which script code can `swap!`/`alter-var-root` with
//! no signal this module can see (see [`resolve_hierarchy_value`]'s "no
//! caching" doc) -- today that mutation is immediately visible because
//! every dispatch re-derefs it fresh. Real Clojure's own cache has the
//! IDENTICAL blind spot (it also only invalidates on `derive`/`prefer`/
//! `defmethod`/etc., never on an arbitrary mutation of a custom hierarchy
//! value it doesn't own), so matching that limitation would be faithful
//! -- but faithful to a DIFFERENT baseline than what mova has shipped
//! and been measured against. Rather than change existing, already-
//! measured behavior for the (rare) custom-`:hierarchy` case, this cache
//! simply never engages for it: `hierarchy_ref.is_none()` is the runtime
//! check, cheap and exact, so custom-hierarchy multimethods keep the
//! uncached path byte-for-byte while the common case (`defmulti` with no
//! `:hierarchy` option -- what the field report's dispatch-ladder probe
//! uses) gets the win.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock};

use crate::error::{JvmClass, RjError};
use crate::eval::Interp;
use crate::value::{Keyword, NativeFn, PMap, Str, Value};

/// W-MULTI: crate-wide multimethod generation counter -- bumped by every
/// mutation that could change ANY multimethod's dispatch answer: a
/// `defmethod`/`.addMethod`, `remove-method`/`remove-all-methods`,
/// `prefer-method`, or a write to the GLOBAL hierarchy (`derive`/
/// `underive`'s 2-arity forms, and anything else going through
/// [`set_global_hierarchy_value`] -- a hierarchy change can flip `isa?`
/// answers for methods on EVERY multimethod that reads the global
/// hierarchy, not just the one that triggered it). Exactly `env.rs`'s
/// `GLOBAL_GENERATION` argument, transplanted here rather than shared with
/// it: "a single monotone counter is conservative by construction" --
/// enumerating precisely which multimethods a given mutation could affect
/// is exactly the kind of bookkeeping that goes subtly wrong later, and
/// mutations are rare next to the dispatch calls the cache exists for.
///
/// publication pattern: see ARCHITECTURE.md, "Publication patterns
/// (lock-free read paths)" -- generation counter.
static MULTI_GENERATION: AtomicU64 = AtomicU64::new(0);

pub(crate) fn bump_multi_generation() {
    MULTI_GENERATION.fetch_add(1, Ordering::Relaxed);
}

/// The current multimethod generation -- see [`MULTI_GENERATION`].
pub fn multi_generation() -> u64 {
    MULTI_GENERATION.load(Ordering::Relaxed)
}

/// W-MULTI: number of distinct dispatch VALUES a [`MultiCache`] will hold
/// before it stops accepting new ones for the current generation -- the
/// same "fixed-slot IC" graceful-degradation shape as `types::PROTO_IC_
/// SLOTS`, just sized for a hash map instead of a probe bank (dispatch
/// values are typically keywords/small enums, not user types, so the
/// realistic working set is tiny; 1024 is generous headroom). Once full,
/// FURTHER new dispatch values simply take the uncached path -- existing
/// entries are never evicted to make room.
const MULTI_CACHE_CAP: usize = 1024;

/// W-MULTI: one multimethod's best-method cache -- dispatch value ->
/// `(winning method key, method fn)`, exactly [`find_best_method`]'s
/// `Ok(Some(..))` payload. See the module doc's "Dispatch caching" section
/// for the invalidation argument this implements; in short: the WHOLE map
/// is stamped with the [`MULTI_GENERATION`] it was populated under, and a
/// stamp mismatch on either probe or install means "someone mutated
/// something since this map was built" -- probe treats that as a miss,
/// install clears the map before writing (so entries from two different
/// generations can never coexist). No per-entry ABA concern: entries are
/// content-keyed by `Value` (`Hash`/`Eq`, used as `PMap` keys crate-wide),
/// never by a pointer that could be recycled.
#[derive(Debug, Default)]
pub struct MultiCache {
    stamp: AtomicU64,
    map: RwLock<HashMap<Value, (Value, Value)>>,
}

impl MultiCache {
    /// A hit requires BOTH the map's stamp to match `gen` (the generation
    /// the caller observed as unchanged across its own dispatch-fn call)
    /// AND an entry for `dispatch_val`. A stamp mismatch alone is enough
    /// to call it a miss without even touching the map -- correctness
    /// never depends on which of two racing writers' clear/insert the
    /// reader happens to observe (see module doc): whichever it sees is a
    /// complete, self-consistent snapshot for SOME generation, and the
    /// stamp comparison against the reader's own `gen` is what decides
    /// whether that snapshot is usable.
    fn probe(&self, gen: u64, dispatch_val: &Value) -> Option<(Value, Value)> {
        if self.stamp.load(Ordering::Acquire) != gen {
            return None;
        }
        crate::sync::lock_read(&self.map).get(dispatch_val).cloned()
    }

    /// Records a resolved `(key, fn)` for `dispatch_val` under generation
    /// `gen`. If the map's current stamp differs from `gen` (a fresher --
    /// or, on a rare race, staler -- generation than what's cached), the
    /// whole map is wiped first: entries from two different generations
    /// must never coexist, since a probe only checks the MAP's stamp, not
    /// per-entry provenance. See [`MULTI_CACHE_CAP`] for the size cap.
    fn install(&self, gen: u64, dispatch_val: Value, entry: (Value, Value)) {
        let mut map = crate::sync::lock_write(&self.map);
        if self.stamp.swap(gen, Ordering::AcqRel) != gen {
            map.clear();
        }
        if map.len() >= MULTI_CACHE_CAP && !map.contains_key(&dispatch_val) {
            return;
        }
        map.insert(dispatch_val, entry);
    }
}

/// One `defmulti`'s live state.
#[derive(Debug)]
pub struct MultiDef {
    /// Bare name (for error messages: `"No method in multimethod '<name>'
    /// ..."` -- measured, no namespace prefix even though the var is
    /// namespace-qualified).
    pub name: Str,
    pub dispatch_fn: Value,
    /// The dispatch value that resolves to the `:default` method --
    /// `:default` unless overridden by `defmulti`'s `:default` option.
    pub default_val: Value,
    /// `None` = the global hierarchy; `Some(v)` = re-deref `v` (through
    /// `Var`/`Atom`, however deep) on every dispatch -- see module doc.
    pub hierarchy_ref: Option<Value>,
    pub methods: PMap,
    /// dispatch-value -> set of dispatch-values it's preferred over.
    pub prefers: PMap,
    /// W-MULTI: the best-method cache -- see [`MultiCache`] and the module
    /// doc's "Dispatch caching" section, including why it is only ever
    /// CONSULTED when `hierarchy_ref` is `None` (custom `:hierarchy`
    /// multimethods stay on the always-fresh path). `Arc` so a clone of
    /// this `MultiDef` (there isn't one today, but `Multimethods::0`'s
    /// registry could in principle be read and cloned) shares the same
    /// cache rather than forking it; freshly empty on every `defmulti`
    /// (re-registration builds a brand new `MultiDef`, cache included --
    /// correct, since a redefinition can change the dispatch fn itself).
    pub cache: Arc<MultiCache>,
}

impl MultiDef {
    /// Deep copy for `Interp::snapshot`'s FORK semantics -- see
    /// `Multimethods::snapshot`. `dispatch_fn`/`default_val`/
    /// `hierarchy_ref` are cheap `Value` clones that preserve pointer
    /// identity where it matters: `dispatch_fn` is (per `multi_key`) a
    /// `Value::Native` whose `Arc<NativeFn>` stays the SAME allocation on
    /// both sides -- `globals.snapshot()` re-wraps the VAR CELL but clones
    /// the `Value` INSIDE verbatim, so the registry key (`Arc::as_ptr` of
    /// that same fn) is still valid, unchanged, in the copied map below.
    /// `methods`/`prefers` are `PMap`s (persistent, structurally shared)
    /// so cloning is O(1) and a `defmethod`/`prefer-method` on either side
    /// after the snapshot allocates fresh nodes instead of mutating the
    /// other's view.
    ///
    /// `cache` is DELIBERATELY NOT `Arc`-shared, unlike the fields above:
    /// `MultiCache`'s stamp is checked against the process-wide
    /// `MULTI_GENERATION` counter (a `static`, shared by every `Interp` in
    /// the process, snapshot or not), so if both engines kept the same
    /// `Arc<MultiCache>`, a `defmethod` on EITHER side bumps that global
    /// stamp and the next dispatch on EITHER side could then install or
    /// read a cache entry naming a method the OTHER side's (independently
    /// deep-copied) `methods` map does not actually contain -- a silent
    /// cross-engine dispatch leak of exactly the shape this whole snapshot
    /// exists to prevent. A fresh, empty `MultiCache` per copy closes that:
    /// a miss just falls through to the registry-read + linear-scan slow
    /// path, same as any other cold multimethod.
    fn snapshot(&self) -> MultiDef {
        MultiDef {
            name: self.name.clone(),
            dispatch_fn: self.dispatch_fn.clone(),
            default_val: self.default_val.clone(),
            hierarchy_ref: self.hierarchy_ref.clone(),
            methods: self.methods.clone(),
            prefers: self.prefers.clone(),
            cache: Arc::new(MultiCache::default()),
        }
    }
}

/// The shared (fork-surviving) multimethod registry, keyed by the
/// dispatch-native's `Arc` pointer identity.
#[derive(Debug, Clone, Default)]
pub struct Multimethods(pub Arc<RwLock<HashMap<usize, MultiDef>>>);

impl Multimethods {
    pub fn new() -> Self {
        Self::default()
    }

    /// A deep, independent copy for `Interp::snapshot`'s FORK semantics --
    /// exactly `crate::ns::snapshot`'s contract: own `Arc<RwLock<..>>`, own
    /// `MultiDef` per entry (`MultiDef::snapshot`, including why its
    /// `cache` must NOT be shared), so a `defmethod`/`remove-method`/
    /// `prefer-method` on either engine after the snapshot mutates only
    /// that engine's copy. Keys stay valid across the copy without
    /// translation -- see `MultiDef::snapshot`'s doc on `dispatch_fn`.
    pub fn snapshot(&self) -> Multimethods {
        let src = crate::sync::lock_read(&self.0);
        let copied = src.iter().map(|(k, v)| (*k, v.snapshot())).collect();
        Multimethods(Arc::new(RwLock::new(copied)))
    }
}

/// The registry key for a `Value` that IS a multimethod's dispatch fn, or
/// `None` if it isn't (a plain fn, or anything else).
pub fn multi_key(v: &Value) -> Option<usize> {
    match v {
        Value::Native(arc) => Some(Arc::as_ptr(arc) as usize),
        _ => None,
    }
}

/// Bare name of the global hierarchy var -- registered bare like every
/// other core builtin (`crate::ns`'s "core interns bare" rule), so
/// `global-hierarchy`, `clojure.core/global-hierarchy`, AND
/// `#'clojure.core/global-hierarchy` (the exact spelling the vendored
/// suite's `global-hierarchy-test` uses) all resolve to the SAME cell via
/// `for_each_global_candidate`'s bare-name fallback.
pub const GLOBAL_HIERARCHY_VAR: &str = "global-hierarchy";

/// Interns `clojure.core/global-hierarchy`, bound to a PLAIN hierarchy MAP
/// (measured on the real oracle: `(class (var-get #'clojure.core/
/// global-hierarchy))` is `clojure.lang.PersistentArrayMap`, NOT an atom --
/// the global 2-arity `derive`/`underive` mutate it via `alter-var-root`
/// directly on the var, the same mechanism `with-redefs`/`with-var-roots`
/// use to swap it out for a test-local hierarchy. A custom `:hierarchy`
/// option MAY still be an atom, or a var pointing at one -- see module doc
/// and [`resolve_hierarchy_value`], which handles both shapes). Called
/// once, from `builtins::multi::install`.
pub fn install_global_hierarchy(i: &mut Interp) {
    i.globals.set(
        crate::value::Symbol::simple(GLOBAL_HIERARCHY_VAR),
        Value::Map(empty_hierarchy()),
    );
}

/// `Value::Var` pointing at the global hierarchy cell, resolved FRESH by
/// symbol lookup (never cached) -- used as `MultiDef.hierarchy_ref`'s
/// default so the global-hierarchy path and a custom `:hierarchy` value
/// share the exact same re-deref-every-call code in
/// [`resolve_hierarchy_value`], no `None`/"is it global" special case.
pub fn global_hierarchy_ref(interp: &Interp) -> Option<Value> {
    interp
        .globals
        .find_bound_cell(&crate::value::Symbol::simple(GLOBAL_HIERARCHY_VAR))
        .map(Value::Var)
}

/// The CURRENT global hierarchy map -- fresh lookup every call (module
/// doc: no caching, so `with-var-roots`/`with-redefs`/a plain `(swap!
/// (var-get #'clojure.core/global-hierarchy) derive ...)` are all
/// immediately visible). Delegates to [`resolve_hierarchy_value`] with
/// `None`, which itself falls back to [`global_hierarchy_ref`].
pub fn global_hierarchy_value(interp: &Interp) -> PMap {
    resolve_hierarchy_value(interp, &None)
}

/// Replaces the CURRENT global hierarchy map by writing the var's ROOT
/// directly (`alter-var-root`-style -- see [`install_global_hierarchy`]'s
/// doc for why: real Clojure's own var holds a plain map, not an atom) --
/// used by `derive`/`underive`'s 2-arity global forms. `store`'s
/// `pristine=false` matches every other ordinary var write in this crate
/// (`set!`, `with-redefs`'s restore, ...). A no-op if the var has
/// vanished (defensive; not reachable through this module's own API,
/// which installs it unconditionally at startup).
pub fn set_global_hierarchy_value(interp: &Interp, new_h: PMap) {
    if let Some(cell) = interp
        .globals
        .find_bound_cell(&crate::value::Symbol::simple(GLOBAL_HIERARCHY_VAR))
    {
        cell.store(Value::Map(new_h), false);
    }
    // W-MULTI: a hierarchy change can flip `isa?` answers for methods on
    // EVERY global-hierarchy multimethod, not just whichever one (if any)
    // triggered it -- see [`MULTI_GENERATION`]'s doc.
    bump_multi_generation();
}

/// `(make-hierarchy)` -- `{:parents {} :ancestors {} :descendants {}}`.
pub fn empty_hierarchy() -> PMap {
    let mut m = PMap::new();
    m.insert(Value::Keyword(Keyword::from("parents")), Value::Map(PMap::new()));
    m.insert(Value::Keyword(Keyword::from("ancestors")), Value::Map(PMap::new()));
    m.insert(Value::Keyword(Keyword::from("descendants")), Value::Map(PMap::new()));
    m
}

fn sub_map<'a>(h: &'a PMap, key: &str) -> Option<&'a PMap> {
    match h.get(&Value::Keyword(Keyword::from(key))) {
        Some(Value::Map(m)) => Some(m),
        _ => None,
    }
}

/// `(parents h tag)` -- direct parents, or `nil` if the tag has none.
pub fn parents_of(h: &PMap, tag: &Value) -> Option<Value> {
    sub_map(h, "parents").and_then(|m| m.get(tag).cloned())
}

/// `(ancestors h tag)` -- transitive parents (precomputed at derive time).
pub fn ancestors_of(h: &PMap, tag: &Value) -> Option<Value> {
    sub_map(h, "ancestors").and_then(|m| m.get(tag).cloned())
}

/// `(descendants h tag)` -- transitive children (precomputed).
pub fn descendants_of(h: &PMap, tag: &Value) -> Option<Value> {
    sub_map(h, "descendants").and_then(|m| m.get(tag).cloned())
}

fn as_set(v: Option<&Value>) -> champ::PersistentHashSet<Value> {
    match v {
        Some(Value::Set(s)) => s.clone(),
        _ => champ::PersistentHashSet::new(),
    }
}

/// BFS over `parents_map` starting at (but not including) `tag`.
fn transitive(parents_map: &PMap, tag: &Value) -> champ::PersistentHashSet<Value> {
    let mut seen: champ::PersistentHashSet<Value> = champ::PersistentHashSet::new();
    let mut stack: Vec<Value> = match parents_map.get(tag) {
        Some(Value::Set(s)) => s.iter().cloned().collect(),
        _ => Vec::new(),
    };
    while let Some(p) = stack.pop() {
        if seen.contains(&p) {
            continue;
        }
        seen = seen.insert(p.clone());
        if let Some(Value::Set(pp)) = parents_map.get(&p) {
            for gp in pp.iter() {
                if !seen.contains(gp) {
                    stack.push(gp.clone());
                }
            }
        }
    }
    seen
}

/// Rebuilds `:ancestors`/`:descendants` from `:parents` (see module doc for
/// why full recompute rather than incremental maintenance).
fn recompute(new_parents: PMap) -> PMap {
    let mut all_tags: champ::PersistentHashSet<Value> = champ::PersistentHashSet::new();
    for (tag, pset) in new_parents.iter() {
        all_tags = all_tags.insert(tag.clone());
        if let Value::Set(s) = pset {
            for p in s.iter() {
                all_tags = all_tags.insert(p.clone());
            }
        }
    }
    let mut ancestors_map = PMap::new();
    let mut descendants_map = PMap::new();
    for tag in all_tags.iter() {
        let anc = transitive(&new_parents, tag);
        if !anc.is_empty() {
            ancestors_map.insert(tag.clone(), Value::Set(anc));
        }
    }
    // Descendants: tag -> everyone whose ancestor set contains tag.
    for tag in all_tags.iter() {
        let mut desc: champ::PersistentHashSet<Value> = champ::PersistentHashSet::new();
        for other in all_tags.iter() {
            if other == tag {
                continue;
            }
            if let Some(Value::Set(oa)) = ancestors_map.get(other) {
                if oa.contains(tag) {
                    desc = desc.insert(other.clone());
                }
            }
        }
        if !desc.is_empty() {
            descendants_map.insert(tag.clone(), Value::Set(desc));
        }
    }
    let mut out = PMap::new();
    out.insert(Value::Keyword(Keyword::from("parents")), Value::Map(new_parents));
    out.insert(Value::Keyword(Keyword::from("ancestors")), Value::Map(ancestors_map));
    out.insert(Value::Keyword(Keyword::from("descendants")), Value::Map(descendants_map));
    out
}

/// Pure `(derive h tag parent)` core -- shared by the 2-arity global form
/// (which additionally asserts the namespace requirements before calling
/// this) and the 3-arity pure form (measured: `compat/multimethods-
/// probe2.clj` q09 -- the 3-arity form does NOT assert namespaces).
///
/// Measured error text (`tests/clojure-suite/vendor/multimethods.clj`'s
/// `cycles-are-forbidden` regexes it): self-derive is `"Assert failed:
/// (not= tag parent)"`; a cycle is `"Cyclic derivation: <parent> has <tag>
/// as ancestor"` where `<parent>` is the NEW parent and `<tag>` is the tag
/// being derived (measured: `(derive family ::ancestor-1 ::child)` with
/// tag=`ancestor-1` parent=`child` throws "... :child has :ancestor-1 as
/// ancestor").
pub fn derive_pure(h: &PMap, tag: &Value, parent: &Value, values_eq: bool) -> Result<PMap, RjError> {
    if values_eq {
        return Err(RjError::other("Assert failed: (not= tag parent)"));
    }
    let parents_map = sub_map(h, "parents").cloned().unwrap_or_default();
    let existing = as_set(parents_map.get(tag));
    if existing.contains(parent) {
        // Edge already present -- no-op (measured: matches real Clojure's
        // `when-not (contains? (tp tag) parent)` guard).
        return Ok(h.clone());
    }
    let ancestors_of_parent = transitive(&parents_map, parent);
    if ancestors_of_parent.contains(tag) {
        return Err(RjError::other(format!(
            "Cyclic derivation: {} has {} as ancestor",
            crate::printer::pr_str(parent),
            crate::printer::pr_str(tag)
        )));
    }
    let mut new_parents = parents_map;
    new_parents.insert(tag.clone(), Value::Set(existing.insert(parent.clone())));
    Ok(recompute(new_parents))
}

/// Pure `(underive h tag parent)` core. Removing a non-existent edge is a
/// silent no-op (measured: `compat/multimethods-probe.clj` p24).
pub fn underive_pure(h: &PMap, tag: &Value, parent: &Value) -> PMap {
    let parents_map = sub_map(h, "parents").cloned().unwrap_or_default();
    let existing = as_set(parents_map.get(tag));
    if !existing.contains(parent) {
        return h.clone();
    }
    let mut new_parents = parents_map;
    let remaining = existing.remove(parent);
    if !remaining.is_empty() {
        new_parents.insert(tag.clone(), Value::Set(remaining));
    } else {
        new_parents.remove(tag);
    }
    recompute(new_parents)
}

/// `(isa? [h] child parent)` -- measured rule: `=` first, then vectors
/// pairwise (same length only), then hierarchy ancestry, then (for two
/// classes only) the built-in Java-inheritance emulation
/// (`crate::types::class_isa`) as a fallback for pairs never explicitly
/// `derive`d (measured: `(isa? Long Number)` true with no `derive` in
/// sight; `(isa? String ::tag)` after `(derive String ::tag)` true via the
/// hierarchy path instead).
pub fn isa_values(interp: &mut Interp, h: &PMap, a: &Value, b: &Value) -> Result<bool, RjError> {
    if interp.values_equal(a, b)? {
        return Ok(true);
    }
    // S7: an entry is vector-shaped for pairwise `isa?` too -- the same
    // "a MapEntry IS a 2-vector" rule every other site follows.
    if let (Value::Vector(va) | Value::MapEntry(va), Value::Vector(vb) | Value::MapEntry(vb)) = (a, b) {
        if va.len() != vb.len() {
            return Ok(false);
        }
        for (x, y) in va.iter().zip(vb.iter()) {
            if !isa_values(interp, h, x, y)? {
                return Ok(false);
            }
        }
        return Ok(true);
    }
    if let Some(Value::Set(anc)) = ancestors_of(h, a) {
        if anc.contains(b) {
            return Ok(true);
        }
    }
    if let (Value::Class(ca), Value::Class(cb)) = (a, b) {
        let res = match (ca.as_ref(), cb.as_ref()) {
            // A user class isa? Object and itself (the `=` check above
            // already covers "itself"), nothing else -- preserved verbatim
            // from the original class-only 2-arity `isa?` this subsumes.
            (crate::types::ClassVal::User(_), crate::types::ClassVal::Builtin { name, .. }) => {
                *name == "java.lang.Object"
            }
            (crate::types::ClassVal::User(_), _) | (_, crate::types::ClassVal::User(_)) => false,
            // D5: `Builtin`/`Interface` in EITHER position. Both are named
            // classes and `class_isa` is a relation over names, so which
            // of the two tables a spelling happens to resolve through must
            // not change the answer -- before this, `clojure.lang.ISeq`
            // (registered in `builtin_interfaces`) answered `false` for
            // every seq class while `clojure.lang.IPersistentMap`
            // (registered in `builtin_classes`) answered correctly, purely
            // because of which table won the name. `defmulti`'s
            // class dispatch runs through here, which is how the vendored
            // `clojure.pprint`'s `(use-method simple-dispatch
            // clojure.lang.ISeq pprint-list)` finds its method.
            _ => crate::types::class_isa(ca.name(), cb.name()),
        };
        if res {
            return Ok(true);
        }
        // C3c: fall through to the derive-bridging walk below even for a
        // Class/Class pair -- `class_isa` alone doesn't see e.g. `isa?
        // java.util.HashMap ::map` (b isn't a class at all here, so this
        // arm didn't even fire for THAT pair), but it also doesn't chase
        // more than one hop for a class/class pair beyond what it's told
        // directly, so let `direct_supers` (below) retry transitively
        // before giving up.
    }
    // C3c (multimethods.clj's `derivation-world-bridges-to-java-
    // inheritance`/`isA-multimethod-test`): `a` is a builtin class whose
    // own hierarchy entry doesn't (yet) contain `b` -- try each of its
    // DIRECT java-inheritance superclasses in turn (`crate::types::
    // direct_supers`), recursively, so `(derive java.util.Map ::map)`
    // makes `(isa? java.util.HashMap ::map)` true via the one-hop chain
    // `HashMap -> Map`, and `ancestors_of`/`class_isa` at the `Map` level
    // (checked by the RECURSIVE call, not repeated here) picks up either
    // the derived tag or a further class-to-class relationship. Measured:
    // `(isa? java.util.Collection ::map)` stays `false` -- `Collection`
    // has no `direct_supers` entry pointing at `Map` (they're siblings,
    // not ancestor/descendant), so this walk never reaches it.
    if let Value::Class(ca) = a {
        if let crate::types::ClassVal::Builtin { name, .. } = ca.as_ref() {
            for super_name in crate::types::direct_supers(name) {
                if let Some(super_class) = crate::builtins::types::class_by_full_name(super_name) {
                    if isa_values(interp, h, &super_class, b)? {
                        return Ok(true);
                    }
                }
            }
        }
    }
    Ok(false)
}

/// Resolves a `:hierarchy` option value (or `None` for the global slot) to
/// its CURRENT hierarchy map, dereferencing through `Var`/`Atom` however
/// deep -- fresh every call, never cached (module doc).
pub fn resolve_hierarchy_value(interp: &Interp, href: &Option<Value>) -> PMap {
    let start = href.clone().or_else(|| global_hierarchy_ref(interp));
    let Some(mut cur) = start else {
        return empty_hierarchy();
    };
    loop {
        cur = match cur {
            Value::Var(cell) => cell.get().unwrap_or(Value::Nil),
            Value::Atom(a) => crate::sync::lock_mutex(&a.state).1.clone(),
            Value::Map(m) => return m,
            _ => return empty_hierarchy(),
        };
    }
}

/// `prefers(x, y)` per real `clojure.lang.MultiFn.prefers`: x directly
/// prefers y, OR x prefers some ancestor... no -- OR x prefers some PARENT
/// of y (recursively), OR some PARENT of x prefers y (recursively).
/// Measured against `tests/clojure-suite/vendor/multimethods.clj`'s
/// `indirect-preferences-mulitmethod-test` (both directions).
pub(crate) fn prefers_transitive(prefers: &PMap, h: &PMap, x: &Value, y: &Value) -> bool {
    if let Some(Value::Set(xs)) = prefers.get(x) {
        if xs.contains(y) {
            return true;
        }
    }
    if let Some(Value::Set(py)) = parents_of(h, y).and_then(|v| if let Value::Set(s) = v { Some(Value::Set(s)) } else { None }) {
        for p in py.iter() {
            if prefers_transitive(prefers, h, x, p) {
                return true;
            }
        }
    }
    if let Some(Value::Set(px)) = parents_of(h, x).and_then(|v| if let Value::Set(s) = v { Some(Value::Set(s)) } else { None }) {
        for p in px.iter() {
            if prefers_transitive(prefers, h, p, y) {
                return true;
            }
        }
    }
    false
}

/// `dominates(x, y)` -- x should win over y when both match: an explicit
/// (possibly indirect) preference, or x being strictly more specific
/// (`isa? x y`, e.g. an exact-derived tag over one of its own ancestors --
/// this is what lets `::rect` win over `::shape` with no `prefer-method`
/// at all when `::rect` derives `::shape`).
fn dominates(interp: &mut Interp, prefers: &PMap, h: &PMap, x: &Value, y: &Value) -> Result<bool, RjError> {
    if prefers_transitive(prefers, h, x, y) {
        return Ok(true);
    }
    isa_values(interp, h, x, y)
}

/// The measured dispatch-resolution algorithm (mirrors `clojure.lang.
/// MultiFn.findAndCacheBestMethod`): every registered key that `isa?`-
/// matches `dispatch_val` is a candidate; the best candidate must dominate
/// every OTHER candidate or the call is ambiguous. Returns `Ok(None)` for
/// "no match" (caller falls back to `:default`), `Err` for ambiguity.
pub fn find_best_method(
    interp: &mut Interp,
    name: &str,
    methods: &PMap,
    prefers: &PMap,
    h: &PMap,
    dispatch_val: &Value,
) -> Result<Option<(Value, Value)>, RjError> {
    let mut best: Option<(Value, Value)> = None;
    for (k, f) in methods.iter() {
        if isa_values(interp, h, dispatch_val, k)? {
            let take = match &best {
                None => true,
                Some((bk, _)) => dominates(interp, prefers, h, k, bk)?,
            };
            if take {
                best = Some((k.clone(), f.clone()));
            }
        }
    }
    let Some((best_key, best_fn)) = best else {
        return Ok(None);
    };
    for (k, _) in methods.iter() {
        if *k == best_key {
            continue;
        }
        if isa_values(interp, h, dispatch_val, k)? && !dominates(interp, prefers, h, &best_key, k)? {
            // W3a: measured -- both of `MultiFn`'s own failure modes are
            // `java.lang.IllegalArgumentException` on the JVM
            // (`MultiFn.java` throws it literally, for both this ambiguity
            // and the no-matching-method case below).
            return Err(RjError::other(format!(
                "Multiple methods in multimethod '{name}' match dispatch value: {} -> {} and {}, and neither is preferred",
                crate::printer::pr_str(dispatch_val),
                crate::printer::pr_str(&best_key),
                crate::printer::pr_str(k),
            ))
            .with_class(JvmClass::IllegalArgument));
        }
    }
    Ok(Some((best_key, best_fn)))
}

/// Builds the dispatch native fn stored as the multimethod's value, and
/// returns it together with the registry key the caller must insert it
/// under. The key IS the fn's own `Arc` pointer identity (module doc), so
/// it can't be known before the `Arc` exists -- resolved via a `OnceLock`
/// the closure reads lazily, set by [`Arc::as_ptr`] right after
/// construction, before the value is ever stored where script code (and
/// therefore a call) could reach it.
///
/// Reads the registry FRESH on every call -- so `defmethod`/`remove-method`/
/// `prefer-method`/`derive` mutations are visible on the very next call
/// (measured requirement, see module doc) -- UNLESS [`MultiCache`] can
/// prove (via [`MULTI_GENERATION`]) that nothing could have changed since
/// the pre-call snapshot, in which case the registry re-read ("READ#2"
/// below) and the linear `find_best_method` scan are both skipped. See the
/// module doc's "Dispatch caching" section for the full argument.
///
/// W-SNAP: the closure deliberately captures NO `Multimethods` handle of
/// its own -- both registry reads go through `interp.multimethods`, i.e.
/// whichever `Interp` is ACTUALLY EXECUTING this call (the first argument
/// every `NativeFn` closure receives), not a `Multimethods` `Arc` pinned at
/// `defmulti`-evaluation time. This matters because the same `Arc<NativeFn>`
/// (and thus the same registry KEY, `Arc::as_ptr` of it) is shared, by
/// design, into every `Interp::snapshot` clone -- `globals.snapshot()`
/// re-wraps the var CELL but clones the `Value` inside verbatim, so a
/// multimethod var defined before a snapshot resolves to the identical
/// `Value::Native` on both engines afterwards. If this closure captured a
/// fixed registry `Arc` instead, EVERY call through that shared native --
/// on the original, on the clone, on any later clone -- would permanently
/// read and write the ONE registry that happened to exist at `defmulti`
/// time, silently defeating `Multimethods::snapshot`'s whole point: a
/// `defmethod` added through the clone would land in the clone's own
/// (correctly deep-copied) `Interp::multimethods` field but never be seen
/// by this closure, which would still be consulting the original's. Reading
/// `interp.multimethods` fresh every call is exactly `lookup_method`'s
/// (`builtins::types.rs`) `interp.protocols` pattern for protocol dispatch,
/// which never had this bug for the same reason -- ported here so both
/// registries honor `Interp::snapshot`'s isolation contract identically.
/// `fork()`-spawned threads are unaffected: their `multimethods` field is
/// the SAME shared `Arc` as the parent's (`Interp::fork`'s doc), so reading
/// it fresh per call is indistinguishable from the old captured-`Arc`
/// behavior in that case.
pub fn make_dispatch_native(name: Str) -> (Arc<NativeFn>, usize) {
    let key_cell: Arc<OnceLock<usize>> = Arc::new(OnceLock::new());
    let key_cell_for_closure = key_cell.clone();
    let name_for_fn = name.clone();
    let arc = Arc::new(NativeFn::new(name.to_string(), move |interp, call_args| {
        let key = *key_cell_for_closure
            .get()
            .expect("multimethod key set immediately after construction, before any call is reachable");
        // READ#1 -- against THIS call's `interp.multimethods`, see the doc
        // above for why that must not be a captured registry handle.
        let (dispatch_fn, default_val, href, cache) = {
            let reg = crate::sync::lock_read(&interp.multimethods.0);
            let def = reg.get(&key).ok_or_else(|| {
                RjError::other(format!("multimethod '{name_for_fn}' vanished from the registry"))
            })?;
            (
                def.dispatch_fn.clone(),
                def.default_val.clone(),
                def.hierarchy_ref.clone(),
                def.cache.clone(),
            )
        };
        // W-MULTI: noted right after READ#1, so it reflects a generation
        // no older than the snapshot just taken. `cacheable` is the module
        // doc's scoping rule -- custom `:hierarchy` multimethods never
        // touch the cache at all, in either direction.
        let cacheable = href.is_none();
        let gen_before = multi_generation();
        let dispatch_val = interp.call(&dispatch_fn, call_args)?;
        if cacheable {
            // Reentrant mutations (including a `defmethod` the dispatch fn
            // itself just performed) bump the generation, so a match here
            // proves methods/prefers/hierarchy are exactly as READ#1 saw
            // them -- safe to trust a cached answer without ever touching
            // the registry lock or the linear scan again.
            let gen_after = multi_generation();
            if gen_after == gen_before {
                if let Some((_k, f)) = cache.probe(gen_after, &dispatch_val) {
                    // field4/W-LENS-1: the hit half of the rate; the miss
                    // half is counted just below, once every path that can
                    // still reach the cache has been exhausted.
                    crate::lens::event(crate::lens::Event::MultiCacheHit);
                    return interp.call(&f, call_args);
                }
            }
        }
        // field4/W-LENS-1: a real miss -- registry READ#2 plus the linear
        // `find_best_method` scan is about to run. Counted for every
        // dispatch that reaches here, INCLUDING the deliberately
        // uncacheable custom-`:hierarchy` case (which is exactly the
        // population a cache-shape wave would want to know the size of).
        crate::lens::event(crate::lens::Event::MultiCacheMiss);
        let h = resolve_hierarchy_value(interp, &href);
        // READ#2 -- deliberately AFTER the dispatch fn call (module doc:
        // the exotic same-call-visibility contract), against THIS call's
        // `interp.multimethods` for the same reason as READ#1.
        let (methods, prefers) = {
            let reg = crate::sync::lock_read(&interp.multimethods.0);
            let def = reg.get(&key).ok_or_else(|| {
                RjError::other(format!("multimethod '{name_for_fn}' vanished from the registry"))
            })?;
            (def.methods.clone(), def.prefers.clone())
        };
        let gen_for_read2 = multi_generation();
        let found = find_best_method(interp, &name_for_fn, &methods, &prefers, &h, &dispatch_val)?;
        if cacheable {
            if let Some((k, f)) = &found {
                // Errors/ambiguity/no-match are never cached (mirrors
                // `ProtoIc`'s "a no-impl result is never interned" rule);
                // only an actual resolved winner is. Stamped with the
                // generation observed around READ#2 -- if it turns out
                // stale by the time this lands (a rare race, not the
                // common case this optimizes), [`MultiCache::install`]'s
                // clear-on-mismatch makes it self-correcting rather than
                // wrong; see that method's doc.
                cache.install(gen_for_read2, dispatch_val.clone(), (k.clone(), f.clone()));
            }
        }
        if let Some((_k, f)) = found {
            return interp.call(&f, call_args);
        }
        if let Some(f) = methods.get(&default_val) {
            return interp.call(&f.clone(), call_args);
        }
        // W3a: measured -- `java.lang.IllegalArgumentException: No method
        // in multimethod 'nom' for dispatch value: 1`.
        Err(RjError::other(format!(
            "No method in multimethod '{name_for_fn}' for dispatch value: {}",
            crate::printer::pr_str(&dispatch_val)
        ))
        .with_class(JvmClass::IllegalArgument))
    }));
    let key = Arc::as_ptr(&arc) as usize;
    key_cell.set(key).ok();
    (arc, key)
}

// ---- heap-image gate-1 accessors (src/image.rs) ----
impl MultiDef {
    pub(crate) fn clone_for_image(&self) -> MultiDef {
        self.snapshot()
    }
    pub(crate) fn for_image(name: Str, dispatch_fn: Value, default_val: Value, hierarchy_ref: Option<Value>, methods: PMap, prefers: PMap) -> MultiDef {
        MultiDef { name, dispatch_fn, default_val, hierarchy_ref, methods, prefers, cache: Arc::new(MultiCache::default()) }
    }
}
