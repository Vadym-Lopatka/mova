//! S3: user types (`defrecord`/`deftype`), class values (`class`/
//! `instance?`), and protocols (`defprotocol`/`extend-type`/
//! `extend-protocol`/`satisfies?`) -- the measured 1.13.0-alpha6 semantics
//! live in scratchpad probe transcripts and are restated per item below.
//!
//! Design in one paragraph: a record instance is `Value::Inst` carrying an
//! `Arc<TypeDef>` (shared per `defrecord`/`deftype` evaluation) plus its
//! data; records follow `HostStruct`'s integration precedent (one
//! map-view choke point, arms added at every map-wide op), deftypes are
//! opaque (measured: `(count (T. 5))` throws UnsupportedOperationException
//! in real Clojure -- opaque here means "no collection nature", not
//! "hidden"). Classes are `Value::Class`: builtins carry a membership
//! predicate for `instance?`; user classes point at their `TypeDef`.
//! Protocol dispatch is a shared registry on the `Interp` (an
//! `Arc<RwLock<..>>` cloned by `Interp::fork`, like `namespaces`, so
//! methods keep working inside `future`s): per protocol, per class-key, a
//! method table. Lookup order (measured): exact class key, else `Object`,
//! else the IllegalArgumentException-shaped error real Clojure raises
//! ("No implementation of method: :m of protocol: #'user/P found for
//! class: java.lang.Long"). `nil` is its own dispatch key, never `Object`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use crate::value::{Keyword, PMap, PVec, Str, Value};

/// One `defrecord`/`deftype` evaluation. Re-evaluating the same name makes
/// a NEW `TypeDef` (matching the JVM, where re-`defrecord` makes a new
/// class and old instances answer `false` to `instance?` of the new one)
/// -- which is why class identity below is `Arc` pointer identity, not
/// name equality.
#[derive(Debug)]
pub struct TypeDef {
    /// Fully qualified, e.g. `user.R` (defining ns with `-` -> `_`? no --
    /// measured: `(class (->R 1 2))` prints `user.R`; munging only matters
    /// for namespaces containing `-`, which the suite's `clojure.
    /// test-clojure.*` files DO have: real class is
    /// `clojure.test_clojure.protocols.R` -- munge dots/dashes exactly:
    /// ns dots stay, ns dashes become underscores).
    pub name: Str,
    /// Basis field names in declaration order (drives `keys`/`seq` order
    /// on records, and field index for deftype access).
    pub basis: Vec<Str>,
    /// `true` for `defrecord` (map nature), `false` for `deftype`.
    pub is_record: bool,
    /// S5: the NAMES of the interfaces this type declared in its
    /// `defrecord`/`deftype` body (`definterface`-minted ones like
    /// `p1.IFoo`, or host interfaces from `builtin_interfaces()` like
    /// `java.util.Map$Entry`). Drives `instance?` against an interface
    /// class (measured: `(instance? java.util.Map$Entry (R. :a 1))` is
    /// true for `(defrecord R [k v] java.util.Map$Entry ...)`) and the
    /// `.method` interop lookup order in `eval::types_forms::
    /// eval_dot_form`. Interface NAMES, not identities: a re-evaluated
    /// `definterface` mints a fresh class value but the name is what an
    /// already-built type recorded, which matches the JVM closely enough
    /// for every shape the vendored suite expresses (no vendored form
    /// re-`definterface`s and then re-checks an old instance).
    pub interfaces: Vec<Str>,
    /// C14 (protocols): per-basis-field `^Tag` hint, PARALLEL to `basis`
    /// (same index), `Value::Nil` for an unhinted field -- what
    /// `RecordName/getBasis` (`test-statics`'s "sane generated basis
    /// method" sub-test) reads back via `(:tag (meta (rb idx)))`. Tags are
    /// resolved to their canonical class name the same way `defprotocol`
    /// method tags are (`^String` -> `'java.lang.String`, never the bare
    /// reader spelling) -- see `eval_deftype_like`'s own comment for the
    /// exact mechanism, shared with `eval_defprotocol`'s tag handling.
    /// Every non-`defrecord`/`deftype` `TypeDef` (the namespace-value and
    /// pseudo-reflection-`Method` internal types) simply leaves this
    /// empty; nothing reads it for those.
    pub field_tags: Vec<Value>,
    /// clojure-lsp campaign (mova/PLAN.md): per-basis-field mutability,
    /// PARALLEL to `basis` -- `true` where the field carried `^:unsynchron
    /// ized-mutable` or `^:volatile-mutable` metadata, `false` otherwise.
    /// `deftype`-only in real Clojure (`defrecord` fields can never be
    /// mutable -- a compile error mova does not reproduce, since nothing
    /// needs it rejected and field access is already `.data`-based for
    /// records, so a stray `true` here would simply never be consulted).
    /// Drives `set!` on a field local inside a method body -- see
    /// `wrap_fields_let`'s doc and `InstVal::fields`'s `Mutex` -- which is
    /// the ONLY reason this exists: `clojure.tools.reader.reader-types`
    /// (a transitive dependency of `rewrite-clj.reader`, itself needed by
    /// clojure-lsp's own `clojure-lsp.parser`) is built entirely on
    /// mutable-field `deftype`s (`StringReader`'s `s-pos`, `Indexing
    /// PushbackReader`'s line/column counters, ...); without real `set!`
    /// support on them, mova cannot read a single character of Clojure
    /// source through rewrite-clj's own reader.
    pub mutable: Vec<bool>,
    /// D1 (`reify`): the anonymous class's OWN method table. Empty for
    /// every `defrecord`/`deftype`/`definterface`/host-shim `TypeDef` --
    /// those register their impls in the shared `Interfaces`/`Protocols`
    /// registries, keyed by `Arc::as_ptr(tdef)`, because a named type is
    /// defined once and its class value outlives every instance.
    ///
    /// `reify` cannot use those registries: it mints a FRESH `TypeDef`
    /// per EVALUATION (real Clojure mints one anonymous class per `reify`
    /// FORM, but its methods close over the call site, so per-evaluation
    /// is the closure-correct granularity here), and those `TypeDef`s are
    /// dropped as soon as their instance is -- a registry keyed by a
    /// freed `Arc` address both leaks an entry per evaluation (`vectors.
    /// clj`'s `test-spliterator-trySplit` alone evaluates 257 of them)
    /// and invites stale-pointer aliasing. Hanging the table off the
    /// type itself makes lifetime and lookup trivially correct: the
    /// methods live and die with the anonymous class, exactly like the
    /// JVM's.
    ///
    /// ONE table per type, keyed by bare method name, shared by interface
    /// heads and protocol heads alike -- `reify`'s two dispatch paths
    /// (`.method` interop via `builtins::types::lookup_interface_method`
    /// and protocol-fn dispatch via `builtins::types::lookup_method`)
    /// both consult it. Cross-head method-name collisions are rejected at
    /// `reify` time (real Clojure: "Duplicate method name"), so one flat
    /// table can never lose an impl.
    pub methods: MethodTable,
    /// W3d2: the PROTOCOL registry keys (`Protocols`' `usize` var-cell
    /// addresses) an anonymous type named in its implements position.
    /// Empty for every named type, and for a `reify`/`proxy` that names
    /// only interfaces.
    ///
    /// `methods` above is keyed by bare method NAME and cannot say which
    /// head a name came from, which is all `lookup_method` needs (it
    /// already has the protocol key from the dispatch fn) but not enough
    /// for `satisfies?`, whose whole question is "does this value
    /// implement protocol P" with no method name in hand. Recording the
    /// keys is the smallest datum that answers it, and it is the same
    /// question real Clojure answers with `(instance? (:on-interface P) x)`
    /// -- a `reify` implements the protocol's generated interface
    /// DIRECTLY, which is also why it must never appear in `extenders`
    /// (nothing `extend`ed it). This closed the last row of
    /// `tests/conformance/pending/records.corpus`.
    pub protocols: Vec<usize>,
}

/// A record or deftype instance.
#[derive(Debug)]
pub struct InstVal {
    pub tdef: Arc<TypeDef>,
    /// Records: the FULL map view (basis fields + ext keys), keyword keys.
    /// Deftypes: field values by basis position (`data` empty).
    pub data: PMap,
    /// clojure-lsp campaign (mova/PLAN.md): `Mutex`, not a plain `PVec`,
    /// so a `deftype` with a `^:unsynchronized-mutable`/`^:volatile-
    /// mutable` field (see `TypeDef::mutable`) can actually be mutated in
    /// place through `set!` -- every holder of this `Arc<InstVal>` must
    /// see the write, which a persistent `PVec` swap on some OTHER `Arc`
    /// could never do. Always empty (`PVec::new()`) for `defrecord`
    /// (records live in `data`); for `deftype` this IS the field storage,
    /// locked only by `inst_field`'s read and `set!`'s write -- both
    /// single-index, uncontended in the tree-walker's one-thread-per-call
    /// shape, so the lock is a formality, not a bottleneck.
    pub fields: Mutex<PVec>,
    /// Records are IObj (measured: `(meta (with-meta r {:x 1}))` works and
    /// preserves class). Slot exists even though mova has no general
    /// `with-meta` yet -- record meta was measured as real suite surface.
    pub meta: Option<PMap>,
}

impl InstVal {
    /// Record key lookup honoring basis-then-ext ordering only where order
    /// matters (`keys`/`seq`); plain lookup is just the map view.
    pub fn lookup(&self, k: &Value) -> Option<Value> {
        self.data.get(k).cloned()
    }

    /// `keys`/`vals`/`seq` order (measured): basis declaration order
    /// first, then ext keys (ext order approximated by map iteration
    /// order -- documented, suite forms don't order-assert ext keys).
    pub fn ordered_entries(&self) -> Vec<(Value, Value)> {
        let mut out = Vec::with_capacity(self.data.len());
        for f in &self.tdef.basis {
            let k = Value::Keyword(Keyword::from(f));
            if let Some(v) = self.data.get(&k) {
                out.push((k, v.clone()));
            }
        }
        for (k, v) in self.data.iter() {
            if let Value::Keyword(n) = k {
                if self.tdef.basis.iter().any(|b| b == n.text_ref()) {
                    continue;
                }
            }
            out.push((k.clone(), v.clone()));
        }
        out
    }
}

/// What `class` returns and `instance?`/protocol dispatch key off.
#[derive(Debug)]
pub enum ClassVal {
    /// A named platform class (`java.lang.Long`, `clojure.lang.
    /// PersistentVector`, ...) with a membership predicate for
    /// `instance?`. Identity/equality by name.
    Builtin {
        name: &'static str,
        /// `instance?` membership. `None` for marker classes that exist
        /// only as `class`-return values or dispatch keys.
        pred: Option<fn(&Value) -> bool>,
    },
    /// A `defrecord`/`deftype` class. Identity by `Arc` pointer.
    User(Arc<TypeDef>),
    /// S5: an INTERFACE -- either `definterface`-minted (`p1.IFoo`) or one
    /// of the host interface names in `builtin_interfaces()`
    /// (`java.util.Map$Entry`). Identity/equality by NAME (like
    /// `Builtin`, unlike `User`): interface values are interned per name
    /// by `builtins::types::interface_class`, so `(= IFoo IFoo)` holds
    /// across every spelling that resolves to the same name. Carries no
    /// membership predicate -- `instance?` against an interface asks the
    /// candidate's `TypeDef::interfaces` list, which is the only way an
    /// interface can have members in a runtime with no JVM class
    /// hierarchy.
    Interface { name: Str },
}

impl ClassVal {
    pub fn name(&self) -> &str {
        match self {
            ClassVal::Builtin { name, .. } => name,
            ClassVal::User(t) => &t.name,
            ClassVal::Interface { name } => name,
        }
    }
}

/// Protocol dispatch key: the class of the first argument, flattened to
/// something hashable without holding `Value`s.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ClassKey {
    Nil,
    Builtin(&'static str),
    /// `Arc::as_ptr` of the `TypeDef` -- see `TypeDef` doc for why
    /// identity, not name.
    User(usize),
    /// S5: an interface, keyed by name (matching `ClassVal::Interface`'s
    /// name-based identity). Only reachable through an explicit
    /// `extend-type`/`extend` on an interface class value -- ordinary
    /// interface METHOD impls do not live in the protocol registry, they
    /// live in `Interfaces` (see below).
    Interface(Str),
    Object,
}

/// S5: interface method impls, keyed by `(interface name, implementing
/// `TypeDef` identity)`. Deliberately NOT folded into `Protocols`: a
/// protocol is a var-backed value with `:impls`/`extenders` introspection
/// and a per-method dispatch FN interned as a global, while an interface
/// has none of that -- its methods are reachable only through `.method`
/// interop on an instance. Shared/`Arc`-cloned exactly like `Protocols`
/// so impls survive `Interp::fork` (methods must keep working inside
/// `future`s), same reasoning as that type's doc.
#[derive(Debug, Clone, Default)]
pub struct Interfaces(pub Arc<RwLock<HashMap<(Str, usize), MethodTable>>>);

impl Interfaces {
    pub fn new() -> Self {
        Self::default()
    }

    /// A deep, independent copy for `Interp::snapshot`'s FORK semantics
    /// (`crate::ns::snapshot`'s contract, transplanted here): own
    /// `Arc<RwLock<..>>` around a `Clone` of the current table, so an
    /// `extend`/interface-method-impl done on either engine after the
    /// snapshot mutates only that engine's copy. Safe to clone wholesale
    /// (no per-entry surgery needed, unlike `Multimethods`/`Protocols`):
    /// `MethodTable` is a plain `HashMap<Str, Value>` with no interior
    /// `Arc<RwLock<..>>` mutable state to worry about sharing -- every
    /// `Value` a method fn closes over follows the same "atoms stay
    /// Arc-shared by reference-identity design" rule `Env::snapshot`
    /// documents for `globals`.
    pub fn snapshot(&self) -> Interfaces {
        Interfaces(Arc::new(RwLock::new(crate::sync::lock_read(&self.0).clone())))
    }
}

/// Per-protocol method tables: class-key -> method-name -> impl fn.
pub type MethodTable = HashMap<Str, Value>;

/// W-PROTO: number of inline-cache slots per declared protocol METHOD.
/// A call site is monomorphic (one record type) or lightly polymorphic in
/// practice; four slots cover that and keep one bank inside a cache line's
/// worth of pointers. A protocol method dispatched against more than four
/// distinct user types simply stops caching past the fourth -- every
/// further type falls back to the (still correct, only slower) `impls`/
/// `MethodTable` hash walk `lookup_method` always did.
pub const PROTO_IC_SLOTS: usize = 4;

/// W-PROTO: one resolved (user type -> impl fn) inline-cache entry.
///
/// `tdef_ptr` is the probe key, stored INLINE so a miss on a populated
/// slot costs one `usize` compare and never chases an `Arc`. `tdef` is the
/// ABA pin: exactly like `host_struct::Shape::ic_pins`, holding a STRONG
/// `Arc` clone of the `TypeDef` this entry names means that allocation can
/// never be freed while the entry is reachable, so its address can never
/// be recycled by a later `defrecord`/`deftype`/`reify` and matched by a
/// stale probe. Here the pin is not a side table at all -- it is a FIELD
/// of the published value, which makes the "pin before publish" ordering
/// requirement structural rather than a discipline (see [`ProtoIc`]).
///
/// publication pattern: see ARCHITECTURE.md, "Publication patterns
/// (lock-free read paths)" -- OnceLock inline cache.
#[derive(Debug)]
pub struct ProtoIcEntry {
    /// `Arc::as_ptr(&tdef) as usize`.
    pub tdef_ptr: usize,
    /// The strong ABA pin; see the struct doc. Never READ by design --
    /// its whole job is the refcount it holds, which is exactly why
    /// `dead_code` has to be silenced here rather than the field removed:
    /// dropping it would reintroduce the address-reuse hazard.
    #[allow(dead_code)]
    pub tdef: Arc<TypeDef>,
    /// The impl fn `lookup_method` resolved for this `(protocol, method,
    /// type)` triple -- the exact `Value` the slow path would clone out,
    /// including the case where it came from the `Object` fallback row.
    pub f: Value,
}

/// W-PROTO: the per-`ProtoDef` method inline cache -- one append-only bank
/// of [`PROTO_IC_SLOTS`] slots per DECLARED method, indexed by the method
/// index a dispatch closure was minted with (see [`ProtoDef::epoch`]).
///
/// # Why this shape, and not `host_struct`'s packed `AtomicU64`
///
/// `host_struct`'s IC can pack its whole payload -- a 16-bit field INDEX --
/// into the spare bits of the key word, so a hit there is literally one
/// relaxed atomic load. A protocol IC's payload is a `Value` (an impl fn),
/// which cannot be packed into a `u64`, so the payload has to live
/// somewhere a reader can reach without a lock. `OnceLock` is exactly that
/// storage: `get()` is an `Acquire` load of the state word plus a branch
/// (the same cost as the packed-word probe) and it hands back a `&T` that,
/// by construction, can never be mutated or dropped while the reference is
/// alive.
///
/// # Ordering argument
///
/// The `host_struct::install` argument ports directly, and gets SIMPLER,
/// because the pin is part of the published value instead of a side table:
///
/// 1. `OnceLock::set` publishes the fully-constructed [`ProtoIcEntry`]
///    with `Release`; `OnceLock::get` consumes it with `Acquire`. A thread
///    that observes `Some(entry)` therefore observes every write that
///    built that entry -- `tdef_ptr`, the `Arc` strong-count bump behind
///    `tdef`, and `f`'s own refcount bump -- with no window in which the
///    key is visible but the pin is not. There is nothing to get wrong in
///    the "pin before publish" direction here: publishing the key and
///    publishing the pin are the SAME store.
/// 2. Entries are append-only and never evicted or overwritten in place
///    (`OnceLock::set` on an already-initialized slot fails, and the
///    install loop just moves to the next slot). So a `&ProtoIcEntry` a
///    reader is holding can never be invalidated underneath it.
/// 3. The ONLY way an entry ever goes away is the whole `ProtoIc` being
///    REPLACED, and that happens exclusively behind the protocol
///    registry's `RwLock` WRITE guard (`Interp::register_protocol_impls_ex`
///    and `eval_defprotocol`). A write guard excludes every reader by
///    definition, so no probe can be in flight across a reset -- which is
///    also what makes invalidation free: `extend-type` does not have to
///    find and clear individual slots, it just drops the bank wholesale
///    and the next dispatch re-resolves and re-installs.
///
/// Installs run under the registry's READ guard (`OnceLock::set` needs
/// only `&self`), so two threads can race to fill the same slot. The loser
/// of `set` moves on to the next slot; the worst case is that the same
/// `(type -> fn)` pair occupies two slots, which is a wasted slot and
/// never a wrong answer -- both racers resolved against the same
/// write-locked-out `impls` snapshot.
#[derive(Debug, Default)]
pub struct ProtoIc {
    banks: Vec<[std::sync::OnceLock<ProtoIcEntry>; PROTO_IC_SLOTS]>,
}

impl ProtoIc {
    /// One empty bank per declared method.
    pub fn new(methods: usize) -> ProtoIc {
        ProtoIc {
            banks: (0..methods).map(|_| std::array::from_fn(|_| std::sync::OnceLock::new())).collect(),
        }
    }

    /// An empty cache with the SAME bank count as this one -- the
    /// invalidation primitive. Derived from `self` rather than recounted
    /// from `ProtoDef::declared_methods` so the bank layout a live epoch's
    /// dispatch fns index into can never drift out from under them.
    pub fn fresh(&self) -> ProtoIc {
        ProtoIc::new(self.banks.len())
    }

    /// Probe the bank for method index `midx` for user type `tdef_ptr`.
    /// `None` means "not cached" (either genuinely absent, or `midx` is
    /// out of range for this -- possibly redefined -- protocol), never
    /// "no such impl": a miss always falls through to the full lookup.
    #[inline]
    pub fn probe(&self, midx: usize, tdef_ptr: usize) -> Option<&Value> {
        // field4/W-LENS-1: hit AND miss, because a miss count without its
        // denominator is a number nobody can act on -- the cache-shape wave
        // needs the RATE. `crate::lens::event` is one TLS load plus one
        // uncontended relaxed load/store; it deliberately does NOT go
        // through the shared-atomic counter this IC exists to avoid.
        let found = (|| {
            for slot in self.banks.get(midx)? {
                // Append-only: the first empty slot ends the populated run,
                // so a cold/short bank exits after one `Acquire` load.
                let entry = slot.get()?;
                if entry.tdef_ptr == tdef_ptr {
                    return Some(&entry.f);
                }
            }
            None
        })();
        crate::lens::event(if found.is_some() {
            crate::lens::Event::ProtoIcHit
        } else {
            crate::lens::Event::ProtoIcMiss
        });
        found
    }

    /// Install `(tdef -> f)` into the first free slot of `midx`'s bank.
    /// Silently does nothing when the bank is full or `midx` is out of
    /// range -- an IC that cannot record a hit is only slower, never wrong.
    pub fn install(&self, midx: usize, tdef: &Arc<TypeDef>, f: &Value) {
        let Some(bank) = self.banks.get(midx) else { return };
        let mut entry = ProtoIcEntry {
            tdef_ptr: Arc::as_ptr(tdef) as usize,
            tdef: tdef.clone(),
            f: f.clone(),
        };
        for slot in bank {
            // `set` hands the value straight back on a lost race, so a
            // contended install never rebuilds (or leaks) the entry.
            match slot.set(entry) {
                Ok(()) => return,
                Err(back) => entry = back,
            }
        }
    }
}

#[derive(Debug, Default)]
pub struct ProtoDef {
    /// `#'user/P`-ish display name for the measured no-impl error message.
    pub var_name: Str,
    /// Per class key: the class VALUE it was registered under (so
    /// `extenders` can hand back the exact class values) and the method
    /// table.
    pub impls: HashMap<ClassKey, (Value, MethodTable)>,
    /// W4B-MESSAGES (protocols.clj's "you can redefine a protocol with
    /// different methods"): the method names THIS (possibly redefined)
    /// version of the protocol currently declares -- independent of
    /// `impls`, which only tracks what's been extended/reified so far.
    /// Rebuilt from scratch every `defprotocol` call (redefinition
    /// REPLACES the whole `ProtoDef`, this field included -- see
    /// `eval_defprotocol`), so a method dropped by a redefinition simply
    /// isn't in here anymore even though the protocol's VAR CELL (and
    /// thus every already-defined method dispatch fn's captured `key`)
    /// is unchanged. Lets a stale dispatch fn (built for a method the
    /// CURRENT protocol no longer has) distinguish "this protocol never
    /// had this method for ANY class" from "this protocol has the
    /// method, this class just doesn't implement it" -- the two report
    /// genuinely different real exceptions (measured:
    /// compat/w4b-protocols-oracle-transcript.txt's first probe).
    pub declared_methods: std::collections::HashSet<Str>,
    /// W-PROTO: which `defprotocol` EVALUATION minted this `ProtoDef`,
    /// from [`next_proto_epoch`]. A protocol's registry key is its var
    /// cell's address, which survives redefinition -- so a dispatch fn
    /// minted by an EARLIER `defprotocol` of the same name can still be
    /// called, and its `midx` (an index into THAT protocol's declared
    /// method order) would silently name a different method in the new
    /// `ic`. Every dispatch fn therefore carries the epoch it was minted
    /// with and touches `ic` only on an exact match; a stale fn just takes
    /// the uncached path, exactly as it did before this cache existed.
    /// `0` is the "never came from a `defprotocol`" epoch that `Default`
    /// (i.e. `impls`' `entry(..).or_default()`) produces, and no dispatch
    /// fn is ever minted with it.
    pub epoch: u64,
    /// W-PROTO: the per-method dispatch inline cache -- see [`ProtoIc`],
    /// including why replacing it wholesale under the registry WRITE lock
    /// is the complete invalidation story.
    pub ic: ProtoIc,
}

impl ProtoDef {
    /// Deep copy for `Interp::snapshot`'s FORK semantics -- see
    /// `Protocols::snapshot`. `impls`/`declared_methods` are plain
    /// `HashMap`/`HashSet` clones (no interior `Arc<RwLock<..>>>` state:
    /// `MethodTable`'s `Value`s follow the same shared-atom-by-design rule
    /// as `globals`), so an `extend-type`/`extend-protocol` on either
    /// engine after the snapshot only touches its own copy. `epoch` is
    /// copied VERBATIM, not re-minted: it identifies which `defprotocol`
    /// EVALUATION produced this `ProtoDef`, a property of the
    /// already-executed code being cloned, and every dispatch fn closed
    /// over from before the snapshot (shared, like any other `Value`, via
    /// `globals.snapshot()`'s per-cell re-wrap) still carries that same
    /// epoch on both sides -- re-minting here would desync it from those
    /// fns and force them onto the uncached path forever.
    ///
    /// `ic` deliberately starts FRESH (`ProtoIc::fresh`, same bank count)
    /// rather than being cloned: it is a pure re-derivable cache (a probe
    /// miss always falls through to the `impls` hash walk), so there is no
    /// correctness reason to carry entries across, and skipping it avoids
    /// needing `OnceLock`/`ProtoIcEntry` to implement `Clone` for a benefit
    /// that is only ever "warm cache after the first post-snapshot call".
    /// This is also why the two engines' caches can never cross-pollute
    /// even though both may hold `Arc<TypeDef>` pins for the SAME
    /// pre-snapshot type (fine per `ProtoIc`'s epoch-keyed doc: epochs are
    /// global and a stale/foreign entry just skips by mismatch) -- each
    /// engine's `ProtoDef` (and thus its `ic`) lives behind its OWN
    /// `RwLock`, so `extend-type`'s wholesale-replace-under-write-lock
    /// invalidation on one side can never observe or touch the other's
    /// bank.
    fn snapshot(&self) -> ProtoDef {
        ProtoDef {
            var_name: self.var_name.clone(),
            impls: self.impls.clone(),
            declared_methods: self.declared_methods.clone(),
            epoch: self.epoch,
            ic: self.ic.fresh(),
        }
    }
}

/// W-PROTO: mints a fresh [`ProtoDef::epoch`]. Starts at 1 so `0` stays
/// reserved for `Default`-constructed `ProtoDef`s (see that field's doc).
pub(crate) static PROTO_EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
pub fn next_proto_epoch() -> u64 {
    PROTO_EPOCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Fast, non-cryptographic hasher for [`ProtoMap`]'s `usize` (pointer-
/// derived) keys. SipHash's DoS resistance buys nothing for a process-
/// internal registry key, and this map is probed on EVERY protocol call
/// (`lookup_method`'s `reg.get(&key)`, cache hit or miss) -- measured as
/// `SipHash::write` samples inside `proto_dispatch_native`'s hot path.
/// FxHash-style rotate+multiply, no cryptographic mixing.
#[derive(Default)]
pub struct FastIdHasher(u64);

const FAST_ID_SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;

impl std::hash::Hasher for FastIdHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0.rotate_left(5) ^ b as u64).wrapping_mul(FAST_ID_SEED);
        }
    }
    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.0 = (self.0.rotate_left(5) ^ i as u64).wrapping_mul(FAST_ID_SEED);
    }
    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }
}

pub type FastIdBuildHasher = std::hash::BuildHasherDefault<FastIdHasher>;
pub type ProtoMap = HashMap<usize, ProtoDef, FastIdBuildHasher>;

/// The shared (fork-surviving) protocol registry, keyed by the protocol's
/// unique id minted at `defprotocol` time.
#[derive(Debug, Clone, Default)]
pub struct Protocols(pub Arc<RwLock<ProtoMap>>);

impl Protocols {
    pub fn new() -> Self {
        Self::default()
    }

    /// A deep, independent copy for `Interp::snapshot`'s FORK semantics --
    /// exactly `crate::ns::snapshot`'s contract: own `Arc<RwLock<..>>`, own
    /// `ProtoDef` per entry (`ProtoDef::snapshot`), so a `defprotocol`
    /// redefinition or `extend-type`/`extend-protocol` on either engine
    /// after the snapshot mutates only that engine's copy. Keys (the
    /// protocol var cell's `Arc` pointer identity) stay valid across the
    /// copy without translation: `globals.snapshot()` re-wraps the VAR
    /// CELL but clones the `Value` inside verbatim, so a pre-snapshot
    /// protocol var still resolves to the same registry key on both sides.
    pub fn snapshot(&self) -> Protocols {
        let src = crate::sync::lock_read(&self.0);
        let copied = src.iter().map(|(k, v)| (*k, v.snapshot())).collect();
        Protocols(Arc::new(RwLock::new(copied)))
    }
}

/// Class of a value, as a dispatch key. `Fn`/`Native`/... map to real
/// (if synthetic-looking) class-key names so `extend-protocol` on
/// `clojure.lang.Fn` etc. stays possible later without re-keying.
pub fn class_key(v: &Value) -> ClassKey {
    match v {
        Value::Nil => ClassKey::Nil,
        Value::Inst(inst) => ClassKey::User(Arc::as_ptr(&inst.tdef) as usize),
        // SPEC-PORT: metadata does not change a value's CLASS -- on the
        // JVM `(with-meta r {..})` hands back an object of the very same
        // class, so it still dispatches every protocol its type
        // implements. Without this arm a `with-meta`'d record/reify fell
        // through to `builtin_class_name`, whose `Value::Meta` row
        // unwraps straight into the `Value::Inst` arm that is
        // `unreachable!("Inst handled by class_key")` -- i.e. `(class
        // (with-meta (reify ..) {..}))`, `satisfies?` and every protocol
        // call on a named `clojure.spec.alpha` spec PANICKED the
        // evaluator thread. `builtin_class_name`'s own unwrap stays for
        // the builtin shapes it already served.
        Value::Meta(m) => class_key(&m.inner),
        other => ClassKey::Builtin(builtin_class_name(other)),
    }
}

/// The measured `(class x)` names for every builtin value shape --
/// each row verified on 1.13.0-alpha6 (scratchpad proto-probe run):
/// `(class 3)` java.lang.Long, `(class 1.5)` java.lang.Double, `(class
/// "s")` java.lang.String, `(class :k)` clojure.lang.Keyword, `(class
/// 'sym)` clojure.lang.Symbol, `(class [1])` clojure.lang.
/// PersistentVector, `(class '(1))` clojure.lang.PersistentList,
/// `(class {:a 1})` clojure.lang.PersistentArrayMap (small maps; CHAMP
/// backing means mova reports ArrayMap for len<=8 and HashMap above,
/// matching the JVM's 8-entry threshold), `(class #{1})` clojure.lang.
/// PersistentHashSet, `(class 3N)` clojure.lang.BigInt, `(class 1/2)`
/// clojure.lang.Ratio, `(class 3.0M)` java.math.BigDecimal, `(class \c)`
/// java.lang.Character, `(class true)` java.lang.Boolean, `(class (atom
/// 1))` clojure.lang.Atom, `(class (lazy-seq [1]))`/`(class (map inc
/// [1]))` clojure.lang.LazySeq.
pub fn builtin_class_name(v: &Value) -> &'static str {
    match v {
        Value::Nil => "nil",
        Value::Bool(_) => "java.lang.Boolean",
        Value::Int(_) => "java.lang.Long",
        Value::Float(_) => "java.lang.Double",
        Value::Str(_) => "java.lang.String",
        Value::Sym(_) => "clojure.lang.Symbol",
        Value::Keyword(_) => "clojure.lang.Keyword",
        Value::Char(_) => "java.lang.Character",
        Value::List(_) => "clojure.lang.PersistentList",
        Value::Vector(_) => "clojure.lang.PersistentVector",
        // W4-PRINTER: an `ex-info` result is a plain `Value::Map` under
        // `:ex/`-namespaced keys (see `core.mova`'s own `ex-info` doc) --
        // real Clojure's is a genuine `clojure.lang.ExceptionInfo`
        // instance, and `Throwable->map`'s cause-chain walk (`(class
        // cur)` at each link) needs THIS class name, not the generic
        // array/hash-map fallback below, for a mixed host-Throwable/
        // ex-info chain to report the right `:type` at an ex-info
        // midlink (`printer.clj`'s `print-throwable` deftest's "mixed"
        // row). One extra key lookup, gated on the map actually carrying
        // `:ex/message` -- every other map (the overwhelming majority)
        // falls through to the size-based check unchanged.
        Value::Map(m) if m.get(&Value::Keyword(Keyword::from("ex/message"))).is_some() => {
            "clojure.lang.ExceptionInfo"
        }
        Value::Map(m) => {
            if m.len() <= 8 {
                "clojure.lang.PersistentArrayMap"
            } else {
                "clojure.lang.PersistentHashMap"
            }
        }
        Value::Set(_) => "clojure.lang.PersistentHashSet",
        Value::Fn(_) | Value::Native(_) | Value::Macro(_) => "clojure.lang.AFunction",
        Value::Atom(_) => "clojure.lang.Atom",
        Value::Volatile(_) => "clojure.lang.Volatile",
        Value::Reduced(_) => "clojure.lang.Reduced",
        // C3e: the internal continuation marker never reaches user code
        // (`uncons` unwraps it on peel -- see `Value::LazyTail`'s doc), so
        // this arm exists only so the match stays exhaustive; if one ever
        // did leak, reporting the class of the `LazySeq` it wraps is the
        // only honest answer.
        Value::Lazy(_) | Value::LazyTail(_) => "clojure.lang.LazySeq",
        Value::Future(_) => "clojure.lang.Future",
        Value::Promise(_) => "clojure.lang.Promise",
        Value::Delay(_) => "clojure.lang.Delay",
        Value::Channel(_) => "clojure.core.async.impl.channels.ManyToManyChannel",
        Value::Flow(_) => "clojure.core.async.flow.Flow",
        // TIMER-CANCEL: a native name for a native type -- `timeout-put`
        // is mova surface (upstream core.async has no cancellable
        // timer), so unlike `Channel`/`Flow` above there is no JVM class
        // to mirror.
        Value::Timer(_) => "mova.async.Timer",
        // SPEC-W6a: the UPSTREAM name, not a native one -- unlike
        // `Timer` above, this type does exist on the JVM. `(class
        // (clojure.test.check.random/make-random 42))` is
        // `clojure.test.check.random.JavaUtilSplittableRandom` there
        // (a `deftype`'s generated class), and the veneer's whole
        // contract is to be indistinguishable from it.
        Value::TcRandom(_) => "clojure.test.check.random.JavaUtilSplittableRandom",
        Value::Regex(_) => "java.util.regex.Pattern",
        Value::Var(_) => "clojure.lang.Var",
        Value::HostStruct(_) => "clojure.lang.PersistentArrayMap",
        Value::LazyMap(m) if crate::lazy_map::count(m) <= crate::value::PMAP_SMALL_MAX => "clojure.lang.PersistentArrayMap",
        Value::LazyMap(_) => "clojure.lang.PersistentHashMap",
        Value::BigInt(_) => "clojure.lang.BigInt",
        // S5 (measured): `(class (biginteger 5))` =>
        // `java.math.BigInteger` -- a DIFFERENT class from
        // `clojure.lang.BigInt` directly above, which is the whole reason
        // `Value::BigInteger` exists as its own variant.
        Value::BigInteger(_) => "java.math.BigInteger",
        Value::Ratio(_) => "clojure.lang.Ratio",
        Value::BigDec(_) => "java.math.BigDecimal",
        Value::Class(_) => "java.lang.Class",
        Value::Inst(_) => unreachable!("Inst handled by class_key"),
        // S4 (everyday3): measured `(class (re-matcher #"a" "a"))`.
        Value::Matcher(_) => "java.util.regex.Matcher",
        Value::Array(arr) => array_jvm_name(&arr.kind),
        // S4: measured class names -- `(class (sorted-map 1 2))` =>
        // `clojure.lang.PersistentTreeMap`, `(class (sorted-set 1 2))` =>
        // `clojure.lang.PersistentTreeSet`, `(class (vector-of :int 1 2
        // 3))` => `clojure.core.Vec`.
        Value::SortedMap(_) => "clojure.lang.PersistentTreeMap",
        Value::SortedSet(_) => "clojure.lang.PersistentTreeSet",
        Value::TypedVec(_) => "clojure.core.Vec",
        // S7 (measured): `(class (first {:a 1}))`, `(class (find {:a 1}
        // :a))`, `(class (find [10 20] 1))` and `(class (first (->R 1 2)))`
        // are all `clojure.lang.MapEntry`. The ONE oracle row mova
        // deliberately does not reproduce is the SORTED map's, where the
        // JVM leaks its red-black tree node class instead
        // (`(class (first (sorted-map :a 1)))` =>
        // `clojure.lang.PersistentTreeMap$BlackVal`, a node that happens to
        // implement `IMapEntry`); `map-entry?` is `true` for it either way,
        // and that is the property `clojure.walk` and the vendored suite
        // actually dispatch on, while modelling PersistentTreeMap's node
        // colouring is exactly the JVM-internals emulation the design
        // directive rules out. See `compat/mapentry-oracle-transcript2.txt`
        // rows 009/067 and this branch's report.
        Value::MapEntry(_) => "clojure.lang.MapEntry",
        // C10: measured `(class clojure.lang.PersistentQueue/EMPTY)` and
        // `(class (conj EMPTY 1 2 3))` are both `clojure.lang.
        // PersistentQueue`, empty or not.
        Value::Queue(_) => "clojure.lang.PersistentQueue",
        // C2 (defstruct), measured: `(class (struct s 1 2))` =>
        // `clojure.lang.PersistentStructMap` -- uniform regardless of
        // WHICH `defstruct`/`create-struct` the struct's basis came from
        // (measured: two structs from different `defstruct`s report the
        // SAME class).
        Value::StructMap(_) => "clojure.lang.PersistentStructMap",
        Value::StructBasis(_) => "clojure.lang.PersistentStructMap$Def",
        // C7 (vecveneer): measured `(class (.rseq v))` =>
        // `clojure.lang.APersistentVector$RSeq`, `(class (.chunkedNext
        // (seq v)))` => `clojure.core.VecSeq` -- see `VecSeqKind`'s doc.
        Value::VecSeq(vs) => match vs.kind {
            crate::value::VecSeqKind::RSeq => "clojure.lang.APersistentVector$RSeq",
            crate::value::VecSeqKind::Chunked => "clojure.core.VecSeq",
        },
        // S5 (host-class shims): the real JVM class name per `HostKind` --
        // see `crate::hostclass`'s module doc for what each one models.
        Value::HostInst(h) => match h.kind {
            crate::hostclass::HostKind::Random => "java.util.Random",
            crate::hostclass::HostKind::Date => "java.util.Date",
            crate::hostclass::HostKind::Thread => "java.lang.Thread",
            crate::hostclass::HostKind::ThreadLocal => "java.lang.ThreadLocal",
            // S6 (predicates.clj batch, out-of-list edit -- same
            // exhaustive-match necessity as the `Value::Uuid`/`Value::Uri`
            // arms' own note below: `HostKind` grew two new variants in
            // `hostclass.rs`, and this match has no wildcard). Measured:
            // `(class (StringBuilder. "x"))` => `java.lang.StringBuilder`,
            // `(class (StringBuffer. "x"))` => `java.lang.StringBuffer`.
            crate::hostclass::HostKind::StringBuilder => "java.lang.StringBuilder",
            crate::hostclass::HostKind::StringBuffer => "java.lang.StringBuffer",
            // C7 (vecveneer): measured `(class (new java.util.ArrayList
            // [0 1 2]))` => `java.util.ArrayList`.
            crate::hostclass::HostKind::ArrayList => "java.util.ArrayList",
            // C10: measured `(class (java.util.HashMap.))` =>
            // `java.util.HashMap`, `(class (java.util.HashSet.))` =>
            // `java.util.HashSet`.
            crate::hostclass::HostKind::HashMap => "java.util.HashMap",
            crate::hostclass::HostKind::HashSet => "java.util.HashSet",
            // C10: `(class (new Object))` => `java.lang.Object`.
            crate::hostclass::HostKind::Object => "java.lang.Object",
            // C10: not oracle-matched (real Clojure mints a different
            // concrete `Iterator` class per source collection, e.g.
            // `PersistentVector$2`/`PersistentArrayMap$Iter` -- see
            // `HostKind::Iterator`'s doc; nothing in scope calls `class`
            // on an iterator, only `.hasNext`/`.next`).
            crate::hostclass::HostKind::Iterator => "clojure.lang.Iterator",
            // D1: same "not oracle-matched, nothing calls `class` on one"
            // note as `Iterator` above -- real Clojure hands back
            // per-source concrete classes here too
            // (`PersistentVector$VSeqSpliterator`,
            // `ReferencePipeline$Head`, `Collectors$CollectorImpl`). The
            // INTERFACE name is the honest answer for a veneer that
            // implements only the interface.
            crate::hostclass::HostKind::Spliterator => "java.util.Spliterator",
            crate::hostclass::HostKind::Stream => "java.util.stream.Stream",
            crate::hostclass::HostKind::Collector => "java.util.stream.Collector",
            // C3c: measured `(class (CyclicBarrier. 1))` =>
            // `java.util.concurrent.CyclicBarrier`.
            crate::hostclass::HostKind::CyclicBarrier => "java.util.concurrent.CyclicBarrier",
            // W4-veneer: measured `(class (java.io.File. "x"))` =>
            // `java.io.File` (`.toPath`'s result answers the SAME name --
            // a documented conflation, see `HostKind::JavaFile`'s doc).
            crate::hostclass::HostKind::JavaFile => "java.io.File",
            // W4-veneer: not oracle-matched (real
            // `Files.newBufferedReader` returns a JDK-internal
            // `sun.nio.fs.*` buffered-reader subclass, not the public
            // `java.io.BufferedReader` itself) -- same "nothing in scope
            // calls `class` on one, the interface-ish public name is the
            // honest stand-in" shape as `Iterator`/`Spliterator` above.
            crate::hostclass::HostKind::BufferedReader => "java.io.BufferedReader",
            // W4-veneer: measured shape -- `ReflectorTryCatchFixture` is
            // its own concrete class on the real JVM too.
            crate::hostclass::HostKind::ReflectorFixture => "clojure.test.ReflectorTryCatchFixture",
            // W4C-NS: `java.util.Locale` is its own concrete (final)
            // class on the real JVM too.
            crate::hostclass::HostKind::Locale => "java.util.Locale",
            // kondo-wave: its own concrete (final) class on the real JVM
            // too.
            crate::hostclass::HostKind::ReentrantLock => "java.util.concurrent.locks.ReentrantLock",
            // lsp/io (review round 2): real streams -- `System/in`/an
            // opened file/an in-memory source all report the same
            // interface-ish veneer name, same "nothing in scope calls
            // `class` on one to distinguish the concrete JDK subclass"
            // rationale as `Iterator`/`Spliterator` above. `instance?
            // java.io.File`/`instance? java.io.InputStream`-shaped checks
            // (the `clojure.java.io` shim's own dispatch) are the only
            // measured consumer of this name.
            crate::hostclass::HostKind::InputStream => "java.io.InputStream",
            crate::hostclass::HostKind::OutputStream => "java.io.OutputStream",
            // lsp/host: measured shape -- `java.time.Clock`/`Instant`
            // are both concrete (final) classes on the real JVM too.
            crate::hostclass::HostKind::Clock => "java.time.Clock",
            crate::hostclass::HostKind::Instant => "java.time.Instant",
            // lsp/kondo: its own concrete (final) class on the real JVM
            // too.
            crate::hostclass::HostKind::StringTokenizer => "java.util.StringTokenizer",
            // mova campaign: the public API type name every
            // `instance?`/`class` call site cares about.
            crate::hostclass::HostKind::MessageDigest => "java.security.MessageDigest",
            crate::hostclass::HostKind::JarFile => "java.util.jar.JarFile",
            crate::hostclass::HostKind::JarEntry => "java.util.jar.JarFile$JarFileEntry",
            crate::hostclass::HostKind::JarEntries => "java.util.Enumeration",
            crate::hostclass::HostKind::RandomAccessFile => "java.io.RandomAccessFile",
            crate::hostclass::HostKind::FileChannel => "sun.nio.ch.FileChannelImpl",
            crate::hostclass::HostKind::FileLock => "sun.nio.ch.FileLockImpl",
            crate::hostclass::HostKind::Url => "java.net.URL",
            crate::hostclass::HostKind::HttpConnection => "sun.net.www.protocol.https.HttpsURLConnectionImpl",
            // lsp/io: measured shape for a bare `(java.io.StringReader.
            // s)`. Deviation, deliberate: since `PushbackReader`/
            // `LineNumberingPushbackReader` are identity passthroughs
            // onto this same `HostKind` (see `hostclass::construct`'s
            // arms), `(class *in*)` inside `with-in-str` also answers
            // this name rather than `clojure.lang.
            // LineNumberingPushbackReader` -- undisclosed only because
            // nothing in scope calls `class`/`instance?` on `*in*`
            // itself (only `slurp`/`read-line`, which dispatch on
            // `HostKind` directly, never on this printed name).
            crate::hostclass::HostKind::StringReader => "java.io.StringReader",
        },
        // S6 (assert/namespace/uuid batch, out-of-list edit -- see this
        // task's own final report for why: `Value` grew two new variants
        // in `value.rs`, and this fn's match is exhaustive with no
        // wildcard, so the crate cannot compile without these two arms;
        // purely additive, touches no line another in-flight branch
        // owns). Measured: `(class (java.util.UUID/randomUUID))` =>
        // `java.util.UUID`, `(class (java.net.URI. "x"))` => `java.net.URI`.
        Value::Uuid(_) => "java.util.UUID",
        Value::Uri(_) => "java.net.URI",
        // S5/M3: metadata never changes a value's class -- measured,
        // `(class (with-meta [1] {:a 1}))` is still
        // `clojure.lang.PersistentVector`.
        Value::Meta(m) => builtin_class_name(&m.inner),
        // C13: `(reduced x)`'s class, matching real Clojure's
        // `clojure.lang.Reduced`. Never observed by this campaign's corpus
    }
}

/// S4/1D: the bracketed JVM class name (`.getName()` spelling) for a
/// [`crate::value::ArrayKind`] -- what `class`/`instance?`/printing use.
/// Primitive kinds are the fixed one-letter JVM descriptors (measured:
/// `(.getName (class (int-array 3)))` -> `"[I"`); `Object(component)`
/// wraps the measured `[L<component>;` shape around whatever
/// `builtin_class_name` says for a representative element -- this is a
/// literal re-spelling of that SAME finite set of strings (not a runtime
/// format!/leak) so the result stays `&'static str`, which is what lets
/// `class`'s cache (`builtins::types::class_of`'s `MARKERS`) key on it by
/// plain string equality without ever allocating one. An `Object`
/// component this table doesn't recognize (a class `builtin_class_name`
/// can name that this task's probe never exercised as an array element,
/// e.g. `Fn`/`Channel`/host types) falls back to the plain
/// `"[Ljava.lang.Object;"` spelling -- a documented, deliberately coarse
/// simplification (see tests/conformance/DEVIATIONS.md), never a panic.
pub fn array_jvm_name(kind: &crate::value::ArrayKind) -> &'static str {
    use crate::value::ArrayKind::*;
    match kind {
        Int => "[I",
        Long => "[J",
        Double => "[D",
        Float => "[F",
        Boolean => "[Z",
        Byte => "[B",
        Char => "[C",
        Short => "[S",
        Object(comp) => match *comp {
            "java.lang.Object" => "[Ljava.lang.Object;",
            "java.lang.Long" => "[Ljava.lang.Long;",
            "java.lang.Double" => "[Ljava.lang.Double;",
            "java.lang.String" => "[Ljava.lang.String;",
            "java.lang.Boolean" => "[Ljava.lang.Boolean;",
            "java.lang.Character" => "[Ljava.lang.Character;",
            "clojure.lang.Keyword" => "[Lclojure.lang.Keyword;",
            "clojure.lang.Symbol" => "[Lclojure.lang.Symbol;",
            "clojure.lang.PersistentVector" => "[Lclojure.lang.PersistentVector;",
            "clojure.lang.PersistentList" => "[Lclojure.lang.PersistentList;",
            "clojure.lang.PersistentArrayMap" => "[Lclojure.lang.PersistentArrayMap;",
            "clojure.lang.PersistentHashMap" => "[Lclojure.lang.PersistentHashMap;",
            "clojure.lang.PersistentHashSet" => "[Lclojure.lang.PersistentHashSet;",
            "clojure.lang.BigInt" => "[Lclojure.lang.BigInt;",
            "java.math.BigInteger" => "[Ljava.math.BigInteger;",
            "clojure.lang.Ratio" => "[Lclojure.lang.Ratio;",
            "java.math.BigDecimal" => "[Ljava.math.BigDecimal;",
            "clojure.lang.Atom" => "[Lclojure.lang.Atom;",
            "clojure.lang.Var" => "[Lclojure.lang.Var;",
            _ => "[Ljava.lang.Object;",
        },
    }
}

/// `array_jvm_name` plus `ArrayVal::dims` extra leading brackets for a
/// `dims > 1` "outer" array (measured: `(class (make-array String 2
/// 3))`'s `.getName`-equivalent internal name is `"[[Ljava.lang.String;"`,
/// TWO brackets). `dims <= 1` is the overwhelmingly common case and
/// returns the same `&'static str` `array_jvm_name` would, at zero extra
/// cost.
pub fn array_jvm_name_dims(kind: &crate::value::ArrayKind, dims: u32) -> std::borrow::Cow<'static, str> {
    let base = array_jvm_name(kind);
    if dims <= 1 {
        std::borrow::Cow::Borrowed(base)
    } else {
        std::borrow::Cow::Owned(format!("{}{}", "[".repeat((dims - 1) as usize), base))
    }
}

/// Real Clojure's `Class` print-method special-cases arrays: instead of
/// the raw JVM binary name (`"[Ljava.lang.Object;"`, `"[I"`, ...) every
/// OTHER consumer here uses (`array_jvm_name` above, `#object[...]`
/// array printing in `crate::printer`, the identity/equality key
/// `builtins::types::class_of` caches by), printing a `Class` VALUE
/// spells an array class `<component>/<dims>` (measured:
/// `(pr-str (class (into-array [])))` -> `"java.lang.Object/1"`,
/// `(pr-str (class (make-array String 2 3)))` -> `"java.lang.String/2"`).
/// Called from `crate::printer`'s `Value::Class` arm; returns `None` for
/// an ordinary (non-array) class name, which keeps printing that bare
/// name unchanged. NOTE: mova has no `.getName` method dispatch today
/// (unrelated pre-existing gap, confirmed absent from `src/`), so this is
/// the ONLY place any array class name is ever rendered friendly --
/// `array_jvm_name`'s raw spelling stays the single source of truth
/// everywhere else.
pub fn array_class_print_name(raw: &str) -> Option<String> {
    let dims = raw.chars().take_while(|&c| c == '[').count();
    if dims == 0 {
        return None;
    }
    let rest = &raw[dims..];
    let component = match rest {
        "I" => "int",
        "J" => "long",
        "D" => "double",
        "F" => "float",
        "Z" => "boolean",
        "B" => "byte",
        "C" => "char",
        "S" => "short",
        s if s.len() >= 2 && s.starts_with('L') && s.ends_with(';') => &s[1..s.len() - 1],
        other => other,
    };
    Some(format!("{component}/{dims}"))
}

/// The `instance?`-able builtin classes registered as bare global vars
/// (`Long`, `String`, `Number`, ...) plus their fully-qualified aliases
/// where real code spells them out. Membership predicates measured:
/// `(instance? Number 3)` true (Long/Double/BigInt/Ratio/BigDecimal all
/// Numbers), `(instance? Comparable "x")` true, `(isa? Long Number)`
/// true.
pub fn builtin_classes() -> Vec<(&'static [&'static str], &'static str, fn(&Value) -> bool)> {
    fn is_int(v: &Value) -> bool {
        matches!(v, Value::Int(_))
    }
    fn is_double(v: &Value) -> bool {
        matches!(v, Value::Float(_))
    }
    fn is_bool(v: &Value) -> bool {
        matches!(v, Value::Bool(_))
    }
    fn is_char(v: &Value) -> bool {
        matches!(v, Value::Char(_))
    }
    fn is_string(v: &Value) -> bool {
        matches!(v, Value::Str(_))
    }
    fn is_number(v: &Value) -> bool {
        matches!(
            v,
            Value::Int(_)
                | Value::Float(_)
                | Value::BigInt(_)
                // S5: `java.math.BigInteger` extends `java.lang.Number`
                // (measured: `(number? (biginteger 5))` is true).
                | Value::BigInteger(_)
                | Value::Ratio(_)
                | Value::BigDec(_)
        )
    }
    fn is_comparable(v: &Value) -> bool {
        // Ordered by what mova's `compare` accepts today. S4: a typed
        // vector is Comparable exactly like a plain one (both go through
        // `builtins::sorted::natural_compare`'s vector arm).
        matches!(
            v,
            Value::Int(_)
                | Value::Float(_)
                | Value::Str(_)
                | Value::Keyword(_)
                | Value::Sym(_)
                | Value::Char(_)
                | Value::Bool(_)
                | Value::Vector(_)
                // S7: measured `(compare (first {:a 1}) [:a 1])` => `0`
                // and `(compare (first {:a 1}) (first {:b 2}))` => `-1`.
                | Value::MapEntry(_)
                | Value::TypedVec(_)
                | Value::BigInt(_)
                | Value::Ratio(_)
                | Value::BigDec(_)
        )
    }
    fn is_keyword(v: &Value) -> bool {
        matches!(v, Value::Keyword(_))
    }
    fn is_symbol(v: &Value) -> bool {
        matches!(v, Value::Sym(_))
    }
    // S4: `clojure.lang.Named` -- keywords and symbols both implement it
    // (real JVM: `Keyword`/`Symbol` both `implements Named`). Added for
    // the vendored `multimethods.clj` suite's own `hierarchy-tags` helper
    // (`(instance? clojure.lang.Named %)`, used to filter a hierarchy's
    // keys down to actual tags) -- not otherwise exercised by this
    // workstream's own corpus.
    fn is_named(v: &Value) -> bool {
        matches!(v, Value::Keyword(_) | Value::Sym(_))
    }
    fn is_fn(v: &Value) -> bool {
        matches!(v, Value::Fn(_) | Value::Native(_) | Value::Keyword(_) | Value::Sym(_))
    }
    fn is_map(v: &Value) -> bool {
        matches!(v, Value::Map(_) | Value::HostStruct(_) | Value::LazyMap(_) | Value::SortedMap(_) | Value::StructMap(_))
            || matches!(v, Value::Inst(i) if i.tdef.is_record)
    }
    // S4: `(vector-of ...)` measured `instance? IPersistentVector` true.
    // S7: so is a map entry -- `clojure.lang.MapEntry extends AMapEntry
    // extends APersistentVector`, measured `(instance?
    // clojure.lang.IPersistentVector (first {:a 1}))` and `(instance?
    // clojure.lang.APersistentVector (first {:a 1}))` are both `true`
    // (and so is `(vector? (first {:a 1}))` -- see
    // `builtins::predicates`'s `vector?`).
    fn is_vector(v: &Value) -> bool {
        matches!(v, Value::Vector(_) | Value::MapEntry(_) | Value::TypedVec(_))
    }
    // S7 (was S6's honest-`false`): `clojure.lang.IMapEntry` is now
    // STRUCTURALLY exact. Until this branch, mova's `(first {:a 1})` was
    // a plain `Value::Vector`, byte-for-byte identical to a hand-typed
    // `[:a 1]`, so no predicate could say `true` for the real entry
    // without also lying about the literal (measured: `(instance?
    // clojure.lang.IMapEntry [:a 1])` => `false`) -- hence the
    // unconditional `false` and its `tests/conformance/DEVIATIONS.md`
    // row. `Value::MapEntry` is that missing structural signal, so both
    // oracle rows are now reproduced exactly, and the deviation is
    // RETIRED (see this branch's promotion diff).
    //
    // Note the variant, not "any 2-element vector": `[:a 1]` typed by
    // hand is still `false`, as measured.
    fn is_map_entry(v: &Value) -> bool {
        matches!(v, Value::MapEntry(_))
    }
    // S6: `clojure.lang.IRecord` -- the very next `cond` clause after
    // `IMapEntry` in `clojure/walk.clj`'s `walk`, so loading that file
    // needs both. Unlike `IMapEntry` this one IS structurally exact:
    // `TypeDef::is_record` is precisely the same field `record?` already
    // uses (`builtins/types.rs`), so this predicate is honest, not a
    // deviation. Measured: `(instance? clojure.lang.IRecord (map->R
    // {...}))` => `true`, `false` for a plain map or vector.
    fn is_irecord(v: &Value) -> bool {
        matches!(v, Value::Inst(inst) if inst.tdef.is_record)
    }
    // clojure-lsp campaign (mova/PLAN.md): mirrors `builtin_class_name`'s
    // own `:ex/message`-tagged-map special case (this file, above) --
    // see `clojure.lang.ExceptionInfo`'s `builtin_classes` row for why
    // the two must never drift apart.
    fn is_ex_info_map(v: &Value) -> bool {
        matches!(v, Value::Map(m) if m.get(&Value::Keyword(Keyword::from("ex/message"))).is_some())
    }
    fn is_set(v: &Value) -> bool {
        matches!(v, Value::Set(_) | Value::SortedSet(_))
    }
    fn is_list(v: &Value) -> bool {
        matches!(v, Value::List(_))
    }
    fn is_coll(v: &Value) -> bool {
        matches!(
            v,
            Value::List(_)
                | Value::Vector(_)
                // S7: measured `(coll? (first {:a 1}))` => `true`.
                | Value::MapEntry(_)
                | Value::Map(_)
                | Value::Set(_)
                | Value::Lazy(_)
                | Value::TypedVec(_)
                | Value::SortedMap(_)
                | Value::SortedSet(_)
                | Value::StructMap(_)
                // C10: measured `(coll? clojure.lang.PersistentQueue/EMPTY)` => `true`.
                | Value::Queue(_)
        )
    }
    // S4: `clojure.lang.Sorted` -- the marker interface `sorted-map`/
    // `sorted-set` implement (measured `(instance? clojure.lang.Sorted
    // (sorted-map 1 2))` true; a plain map/set is NOT `Sorted`).
    fn is_sorted(v: &Value) -> bool {
        matches!(v, Value::SortedMap(_) | Value::SortedSet(_))
    }
    // S4: the class `(vector-of ...)` actually returns (measured
    // `(class (vector-of :int 1 2 3))` -> `clojure.core.Vec`).
    fn is_typed_vec(v: &Value) -> bool {
        matches!(v, Value::TypedVec(_))
    }
    fn is_seq(v: &Value) -> bool {
        matches!(v, Value::List(_) | Value::Lazy(_))
    }
    // C10: `java.util.Collection` -- narrower than `IPersistentCollection`
    // (`is_coll` above): real `java.util.Map`/`Map$Entry` do NOT extend
    // `Collection` (measured: `(instance? java.util.Collection {})` and
    // `(instance? .. (first {:a 1}))` are both `false`), only the
    // ordered/set-ish shapes do. Used by `data_structures.clj`'s
    // `is-same-collection` helper (`(:import [java.util Collection])`) to
    // gate its `.size` call -- measured `true` for vector/list/lazy-seq/
    // set/sorted-set/typed-vec/queue, `false` for map/sorted-map/
    // struct-map/map-entry.
    //
    // W4C-NS (data_structures.clj phantom-pass purge): `VecSeq` is NOT
    // uniformly `Collection` -- the two `VecSeqKind`s measured DIFFERENT
    // real-JVM answers. `(instance? java.util.Collection (rseq [1 2 3]))`
    // (kind `RSeq`, real class `clojure.lang.APersistentVector$RSeq`) is
    // `true` (`ASeq`'s usual `List`/`Collection` lineage). But
    // `(instance? java.util.Collection (seq (vector-of :long 2 3 4)))`
    // (kind `Chunked`, real class `clojure.core.VecSeq`) is measured
    // `false` -- `clojure.core.VecSeq` (defined in `clojure/core.clj`'s
    // gvec support, not `clojure.lang.ASeq`) implements `ISeq`/`Sequential`
    // but NOT `java.util.Collection`. Both measured directly against the
    // oracle (`compat/w4c-collection-oracle-transcript.txt`). Treating
    // `VecSeq` as uniformly `true` (the previous shape) made
    // `data_structures.clj`'s `ordered-collection-equality-test` count 10
    // MORE assertions than the oracle (each of the 5 `long-colls` paired
    // against `(seq (vector-of :long 2 3 4))` skips one `.size` `is` on
    // the real JVM but mova ran and passed it) -- a phantom-pass inflation
    // of the North Star numerator, not a real capability gap.
    fn is_java_collection(v: &Value) -> bool {
        matches!(
            v,
            Value::List(_)
                | Value::Vector(_)
                | Value::Set(_)
                | Value::SortedSet(_)
                | Value::Lazy(_)
                | Value::TypedVec(_)
                // C10: measured `(instance? java.util.Collection
                // clojure.lang.PersistentQueue/EMPTY)` => `true`.
                | Value::Queue(_)
        ) || matches!(v, Value::VecSeq(vs) if vs.kind == crate::value::VecSeqKind::RSeq)
    }
    // C3c (multimethods.clj's `derivation-world-bridges-to-java-
    // inheritance`/`isA-multimethod-test`): `java.util.Map` -- a plain
    // Clojure map genuinely IS a `java.util.Map` on the real JVM
    // (`PersistentArrayMap`/`PersistentHashMap` both implement it), same
    // for the `java.util.HashMap` veneer; a vector/list/set is NOT
    // (measured: `(instance? java.util.Map {})`/`(instance? java.util.Map
    // (java.util.HashMap.))` => `true`, `(instance? java.util.Map [1
    // 2])` => `false`). Deliberately the SAME shape (union of the plain
    // map variants) as `is_map` above, plus the `HashMap` host-class
    // veneer `is_map` does NOT include (a plain Clojure map and a
    // `java.util.HashMap` are different mova `Value` shapes, but both
    // answer `instance? java.util.Map` true).
    fn is_java_map(v: &Value) -> bool {
        is_map(v) || matches!(v, Value::HostInst(h) if h.kind == crate::hostclass::HostKind::HashMap)
    }
    fn is_var(v: &Value) -> bool {
        matches!(v, Value::Var(_))
    }
    fn is_atom(v: &Value) -> bool {
        matches!(v, Value::Atom(_))
    }
    fn is_queue(v: &Value) -> bool {
        matches!(v, Value::Queue(_))
    }
    // S7 (tail wave), measured against the oracle: `clojure.lang.Atom` and
    // `clojure.lang.Delay` BOTH unconditionally implement every
    // `java.util.function.*Supplier` interface below -- structurally, at
    // the class level, regardless of the cell's current value (`(instance?
    // java.util.function.IntSupplier (delay "not a number"))` is still
    // `true` on the real JVM; only actually CALLING `.getAsInt` on it would
    // fail). See `builtins::conc::supplier_dot_method` for the `.get`/
    // `.getAsBoolean`/`.getAsInt`/`.getAsLong`/`.getAsDouble` methods
    // these back.
    fn is_supplier(v: &Value) -> bool {
        matches!(v, Value::Atom(_) | Value::Delay(_))
    }
    fn is_object(v: &Value) -> bool {
        !matches!(v, Value::Nil)
    }
    fn is_bigint(v: &Value) -> bool {
        matches!(v, Value::BigInt(_))
    }
    fn is_biginteger(v: &Value) -> bool {
        matches!(v, Value::BigInteger(_))
    }
    fn is_bigdec(v: &Value) -> bool {
        matches!(v, Value::BigDec(_))
    }
    fn is_ratio(v: &Value) -> bool {
        matches!(v, Value::Ratio(_))
    }
    fn is_pattern(v: &Value) -> bool {
        matches!(v, Value::Regex(_))
    }
    fn is_class(v: &Value) -> bool {
        matches!(v, Value::Class(_))
    }
    // S5 (host-class shims): one predicate per `HostKind` -- registering
    // these here (rather than a bespoke registration path) is what makes
    // `java.util.Random`/`java.util.Date`/`Thread`/`ThreadLocal` resolve
    // as BARE fully-qualified symbols with no `import` needed: `install`
    // below binds every alias in this table (short AND fully-qualified)
    // as a plain global var, exactly like `java.util.regex.Pattern`'s
    // existing entry above already did for one class.
    fn is_random(v: &Value) -> bool {
        matches!(v, Value::HostInst(h) if h.kind == crate::hostclass::HostKind::Random)
    }
    // lsp/io: real predicate for `java.io.StringReader` (see that row's
    // doc) -- same shape as `is_random` just above.
    fn is_string_reader(v: &Value) -> bool {
        matches!(v, Value::HostInst(h) if h.kind == crate::hostclass::HostKind::StringReader)
    }
    // S7 (tail wave): `java.util.UUID` was printable (`(class (UUID/
    // randomUUID))`, see `builtin_class_name` below) but not a resolvable
    // CLASS VALUE at all -- `(instance? java.util.UUID ..)` and `(import
    // '[java.util UUID])` both threw "unknown class" (measured: parse.clj's
    // `test-parse-uuid` does exactly that import). Fully-qualified only in
    // the table below, same reasoning as `java.util.Random`/`java.util.
    // Date` -- `UUID` is not `java.lang.*`, so nothing auto-resolves the
    // BARE name without an explicit `import` (which is what this row now
    // makes possible).
    fn is_uuid(v: &Value) -> bool {
        matches!(v, Value::Uuid(_))
    }
    fn is_date(v: &Value) -> bool {
        matches!(v, Value::HostInst(h) if h.kind == crate::hostclass::HostKind::Date)
    }
    // W4C-NS: `java.util.Locale` -- same construction-only shape as
    // `is_random`/`is_date` above.
    fn is_locale(v: &Value) -> bool {
        matches!(v, Value::HostInst(h) if h.kind == crate::hostclass::HostKind::Locale)
    }
    fn is_thread(v: &Value) -> bool {
        matches!(v, Value::HostInst(h) if h.kind == crate::hostclass::HostKind::Thread)
    }
    fn is_threadlocal(v: &Value) -> bool {
        matches!(v, Value::HostInst(h) if h.kind == crate::hostclass::HostKind::ThreadLocal)
    }
    // C3c: `java.util.concurrent.CyclicBarrier` -- see `hostclass`'s
    // `HostState::CyclicBarrier` doc.
    fn is_cyclic_barrier(v: &Value) -> bool {
        matches!(v, Value::HostInst(h) if h.kind == crate::hostclass::HostKind::CyclicBarrier)
    }
    // kondo-wave: same one-predicate-per-`HostKind` pattern as
    // `is_cyclic_barrier` above -- see `hostclass::HostKind::
    // ReentrantLock`'s doc.
    fn is_reentrant_lock(v: &Value) -> bool {
        matches!(v, Value::HostInst(h) if h.kind == crate::hostclass::HostKind::ReentrantLock)
    }
    // S6 (predicates.clj batch): same one-predicate-per-`HostKind` pattern
    // as `is_random`/`is_date`/... above.
    fn is_stringbuilder(v: &Value) -> bool {
        matches!(v, Value::HostInst(h) if h.kind == crate::hostclass::HostKind::StringBuilder)
    }
    fn is_stringbuffer(v: &Value) -> bool {
        matches!(v, Value::HostInst(h) if h.kind == crate::hostclass::HostKind::StringBuffer)
    }
    // C7 (vecveneer): same one-predicate-per-`HostKind` pattern as
    // `is_random`/`is_date`/`is_stringbuilder`/... above.
    fn is_arraylist(v: &Value) -> bool {
        matches!(v, Value::HostInst(h) if h.kind == crate::hostclass::HostKind::ArrayList)
    }
    // W4-veneer (sequences.clj's `test-iteration` file-IO row): same
    // one-predicate-per-`HostKind` pattern as `is_arraylist` above.
    fn is_java_file(v: &Value) -> bool {
        matches!(v, Value::HostInst(h) if h.kind == crate::hostclass::HostKind::JavaFile)
    }
    // lsp/io (review round 2): `clojure.java.io`'s shim dispatches on
    // `(instance? java.io.InputStream x)`/`(instance? java.io.
    // OutputStream x)` to tell an already-open stream apart from a bare
    // path/`File` it still needs to coerce -- same pattern as
    // `is_java_file` above.
    fn is_input_stream(v: &Value) -> bool {
        matches!(v, Value::HostInst(h) if h.kind == crate::hostclass::HostKind::InputStream)
    }
    fn is_output_stream(v: &Value) -> bool {
        matches!(v, Value::HostInst(h) if h.kind == crate::hostclass::HostKind::OutputStream)
    }
    // lsp/kondo: `java.util.jar.JarFile`/`java.util.zip.ZipFile` veneer --
    // same "fully-qualified only" shape as `File`/`InputStream` above.
    fn is_jar_file(v: &Value) -> bool {
        matches!(v, Value::HostInst(h) if h.kind == crate::hostclass::HostKind::JarFile)
    }
    fn is_random_access_file(v: &Value) -> bool {
        matches!(v, Value::HostInst(h) if h.kind == crate::hostclass::HostKind::RandomAccessFile)
    }
    fn is_file_channel(v: &Value) -> bool {
        matches!(v, Value::HostInst(h) if h.kind == crate::hostclass::HostKind::FileChannel)
    }
    fn is_file_lock(v: &Value) -> bool {
        matches!(v, Value::HostInst(h) if h.kind == crate::hostclass::HostKind::FileLock)
    }
    fn is_jar_entry(v: &Value) -> bool {
        matches!(v, Value::HostInst(h) if h.kind == crate::hostclass::HostKind::JarEntry)
    }
    // lsp/host: `mk_exception`-built instances (`Value::Inst`, a
    // synthetic `TypeDef` whose `name` IS the fully-qualified class
    // name) -- checked by name directly, same shape as `hostclass.rs`'s
    // own (private) `is_exception_named` helper, duplicated here rather
    // than exposed cross-module for one predicate apiece.
    fn is_cancellation_exception(v: &Value) -> bool {
        matches!(v, Value::Inst(inst) if inst.tdef.name.as_ref() == "java.util.concurrent.CancellationException")
    }
    fn is_timeout_exception(v: &Value) -> bool {
        matches!(v, Value::Inst(inst) if inst.tdef.name.as_ref() == "java.util.concurrent.TimeoutException")
    }
    // W4-veneer (try_catch.clj): the fixture object itself (`HostInst`,
    // same pattern as `is_arraylist`/`is_java_file` above) and its nested
    // `Cookies` throwable (`Value::Inst`, same "check `tdef.name`"
    // pattern `is_exception_named` uses in `hostclass.rs`, duplicated
    // rather than shared across the two modules' separate class tables).
    fn is_reflector_fixture(v: &Value) -> bool {
        matches!(v, Value::HostInst(h) if h.kind == crate::hostclass::HostKind::ReflectorFixture)
    }
    fn is_reflector_cookies(v: &Value) -> bool {
        matches!(v, Value::Inst(inst) if inst.tdef.name.as_ref() == "clojure.test.ReflectorTryCatchFixture$Cookies")
    }
    // C10: same pattern as `is_arraylist` above -- makes `java.util.
    // HashMap`/`java.util.HashSet` resolve as bare fully-qualified
    // symbols with NO `:import` needed (measured: `(java.util.HashMap.
    // {})`/`(java.util.HashSet. #{})` are both spelled fully-qualified,
    // unimported, everywhere the vendored suite uses them).
    fn is_java_hashmap(v: &Value) -> bool {
        matches!(v, Value::HostInst(h) if h.kind == crate::hostclass::HostKind::HashMap)
    }
    fn is_java_hashset(v: &Value) -> bool {
        matches!(v, Value::HostInst(h) if h.kind == crate::hostclass::HostKind::HashSet)
    }
    // lsp/kondo: `java.util.StringTokenizer` -- resolves via `:import
    // [java.util StringTokenizer]` (auto-derives the short alias) OR
    // fully-qualified, same shape as `ReentrantLock`/`CyclicBarrier`
    // above (a `java.util.*` class, not `java.lang.*`, so only the
    // fully-qualified row is registered here; the SHORT alias is what
    // `import`'s own `short_class_name` derivation adds at runtime).
    fn is_string_tokenizer(v: &Value) -> bool {
        matches!(v, Value::HostInst(h) if h.kind == crate::hostclass::HostKind::StringTokenizer)
    }
    // C7 (vecveneer): one predicate per `VecSeqKind`, same pattern as
    // `is_random`/`is_date`/... above -- makes `clojure.lang.
    // APersistentVector$RSeq`/`clojure.core.VecSeq` resolve as bare
    // fully-qualified symbols (`test-reversed-vec`'s `(= clojure.lang.
    // APersistentVector$RSeq (class reversed))`).
    fn is_rseq(v: &Value) -> bool {
        matches!(v, Value::VecSeq(vs) if vs.kind == crate::value::VecSeqKind::RSeq)
    }
    fn is_vecseq_chunked(v: &Value) -> bool {
        matches!(v, Value::VecSeq(vs) if vs.kind == crate::value::VecSeqKind::Chunked)
    }
    // D3 (2026-08-21, owner-approved veneer per binding directive: interop
    // targets Rust-native values, java.*/clojure.lang.* names are a thin
    // compatibility veneer, never JVM emulation): `clojure.lang.Compiler`/
    // `clojure.lang.Compiler$CompilerException` -- class ROWS only, so
    // `(import '(clojure.lang Compiler Compiler$CompilerException))`
    // (vendored `evaluation.clj`'s ns form) resolves through
    // `class_by_full_name` instead of unblocking the whole file. Nothing
    // mova builds is ever an instance of either: `Compiler` is never
    // instantiated on the real JVM either (a static-methods-only utility
    // class -- see `builtins::statics`'s `Compiler/eval` row, the one
    // static member the suite actually calls), and `Compiler$CompilerException`
    // is class-blind everywhere the vendored suite spells it (measured:
    // every use is `(thrown? Compiler$CompilerException ..)` or
    // `(thrown-with-cause-msg? Compiler$CompilerException ..)`, and the
    // test shim's `is` macro never evaluates or checks the class argument
    // for either form -- see `mova-test-shim.mova`'s own doc), so no
    // synthetic exception TYPE (unlike hostclass.rs's `IllegalArgumentException`-
    // family veneer) is measurably needed here -- start minimal, grow only
    // from a real blocker.
    fn is_never_instance(_v: &Value) -> bool {
        false
    }
    // C3c (sequences.clj's `test-seqs-implements-iobj`): `clojure.lang.
    // IObj` -- SAME allow-list `builtins::meta::is_iobj` already gates
    // `with-meta`/`vary-meta` against (see that fn's doc for why one
    // predicate answers both questions). Measured: every collection the
    // deftest doseqs over (vector, `vector-of`, map, sorted-map, set,
    // sorted-set, queue) plus every one of their `seq`/`rseq` results
    // (which realize to `Value::List`/already-`IObj` shapes in mova) is
    // `IObj`.
    fn is_iobj_class(v: &Value) -> bool {
        crate::builtins::meta::is_iobj(v)
    }
    // C3c: `clojure.lang.IMeta` -- the superset `IObj` (immutable,
    // `with-meta`) and `IReference` (mutable, `alter-meta!`) both extend
    // on the real JVM (measured: `(instance? clojure.lang.IMeta (atom
    // 1))` and `(instance? clojure.lang.IMeta (var *ns*))` are both
    // `true`, even though NEITHER is `IObj` -- `(instance? clojure.lang.
    // IObj (atom 1))` is `false`). `meta_of_value` (`builtins::meta`) is
    // exactly this same union already (`Var`/`Atom` read the mutable
    // cell, everything else reads the `IObj` wrapper), so this predicate
    // mirrors its match arms rather than inventing a separate list.
    fn is_imeta(v: &Value) -> bool {
        matches!(v, Value::Var(_) | Value::Atom(_)) || crate::builtins::meta::is_iobj(v)
    }
    vec![
        (&["Long", "java.lang.Long"][..], "java.lang.Long", is_int as fn(&Value) -> bool),
        (&["Integer", "java.lang.Integer"][..], "java.lang.Integer", is_int),
        // S6 (compat veneer for `(Short. x)`/`(Byte. x)` -- see
        // `hostclass::construct`'s module doc): mova has no boxed
        // `Short`/`Byte` value distinct from `Value::Int`, so -- exactly
        // like `Long`/`Integer` above -- both resolve `instance?`/`class`
        // membership via the same `is_int` predicate every plain integer
        // already uses. Added by S6 alongside the boxed-ctor arms below;
        // they did not exist before this task because nothing needed a
        // bare `Short`/`Byte` class VALUE until `(Short. Short/MAX_VALUE)`
        // (vendored `numbers.clj`) needed one to construct against.
        (&["Short", "java.lang.Short"][..], "java.lang.Short", is_int),
        (&["Byte", "java.lang.Byte"][..], "java.lang.Byte", is_int),
        (&["Double", "java.lang.Double"][..], "java.lang.Double", is_double),
        (&["Float", "java.lang.Float"][..], "java.lang.Float", is_double),
        (&["Boolean", "java.lang.Boolean"][..], "java.lang.Boolean", is_bool),
        (&["Character", "java.lang.Character"][..], "java.lang.Character", is_char),
        (&["String", "java.lang.String"][..], "java.lang.String", is_string),
        (&["CharSequence", "java.lang.CharSequence"][..], "java.lang.CharSequence", is_string),
        (&["Number", "java.lang.Number"][..], "java.lang.Number", is_number),
        (&["Comparable", "java.lang.Comparable"][..], "java.lang.Comparable", is_comparable),
        (&["Object", "java.lang.Object"][..], "java.lang.Object", is_object),
        (&["Class", "java.lang.Class"][..], "java.lang.Class", is_class),
        (&["BigDecimal", "java.math.BigDecimal"][..], "java.math.BigDecimal", is_bigdec),
        (&["clojure.lang.Keyword"][..], "clojure.lang.Keyword", is_keyword),
        (&["clojure.lang.Symbol"][..], "clojure.lang.Symbol", is_symbol),
        (&["clojure.lang.Named"][..], "clojure.lang.Named", is_named),
        (&["clojure.lang.IFn"][..], "clojure.lang.IFn", is_fn),
        (&["clojure.lang.Fn", "clojure.lang.AFunction"][..], "clojure.lang.AFunction", is_fn),
        (&["clojure.lang.IPersistentMap", "clojure.lang.APersistentMap"][..], "clojure.lang.IPersistentMap", is_map),
        (&["clojure.lang.PersistentArrayMap"][..], "clojure.lang.PersistentArrayMap", is_map),
        (&["clojure.lang.PersistentHashMap"][..], "clojure.lang.PersistentHashMap", is_map),
        (&["clojure.lang.IPersistentVector", "clojure.lang.PersistentVector", "clojure.lang.APersistentVector"][..], "clojure.lang.PersistentVector", is_vector),
        // S6: resolvable so `clojure/walk.clj`'s `(instance? clojure.lang.
        // IMapEntry form)` (its `walk`'s entry-vs-plain-vector branch)
        // stops being an unresolved symbol -- see `is_map_entry`'s doc
        // for why the predicate is unconditionally `false` (a documented
        // deviation, not a gap: mova has no representation that could
        // answer `true` honestly without also lying about a literal
        // 2-vector).
        // S7: `is_map_entry` is structural now (see its doc) -- these
        // two rows are what `(instance? clojure.lang.IMapEntry (first
        // {:a 1}))` and `(instance? java.util.Map$Entry (first {:a 1}))`
        // (both measured `true`) answer off. `java.util.Map$Entry` ALSO
        // stays in `builtin_interfaces` below, where a `defrecord` can
        // declare it (protocols.clj does); `builtins::types::instance_of`
        // consults `builtin_classes` first, so a native entry answers
        // here and a declaring record answers there -- see the
        // `ClassVal::Builtin` / `ClassVal::Interface` split in that fn.
        (&["clojure.lang.IMapEntry"][..], "clojure.lang.IMapEntry", is_map_entry),
        (&["clojure.lang.MapEntry"][..], "clojure.lang.MapEntry", is_map_entry),
        // clojure-lsp campaign (mova/PLAN.md): `clojure.lang.ExceptionInfo`
        // as a bare, resolvable symbol -- `builtin_class_name`/`class_of`
        // (this file, above) already special-cases an `ex-info` map's
        // reported CLASS NAME to this string, but that path never
        // installed the name as a global VAR, so `(instance? clojure.
        // lang.ExceptionInfo x)` (`clojure.tools.reader.impl.errors`'s
        // `ex-info?`, a transitive dependency of `rewrite-clj.reader`)
        // threw "Unable to resolve symbol" even though `(class (ex-info
        // "x" {}))` already printed the right name. Predicate mirrors
        // `class_of`'s own `:ex/message`-tagged-map check exactly, so the
        // two can never drift into disagreeing about the same value.
        (
            &["clojure.lang.ExceptionInfo"][..],
            "clojure.lang.ExceptionInfo",
            is_ex_info_map,
        ),
        (&["clojure.lang.IRecord"][..], "clojure.lang.IRecord", is_irecord),
        (&["clojure.lang.IPersistentSet", "clojure.lang.PersistentHashSet"][..], "clojure.lang.PersistentHashSet", is_set),
        (&["clojure.lang.IPersistentList", "clojure.lang.PersistentList"][..], "clojure.lang.PersistentList", is_list),
        (&["clojure.lang.IPersistentCollection"][..], "clojure.lang.IPersistentCollection", is_coll),
        // C10: `(:import [java.util Collection])` -- `import_one` looks
        // up the CANONICAL column only (`class_by_full_name`), so it must
        // read `java.util.Collection` exactly; the bare `Collection`
        // alias is what the import then binds as a namespace-local var.
        (&["java.util.Collection", "Collection"][..], "java.util.Collection", is_java_collection),
        // C3c: fully-qualified only, no bare alias -- same "not
        // java.lang.*, nothing here imports it" reasoning as `java.util.
        // Random`/`java.util.Date`/`java.util.HashMap` above (and unlike
        // `java.util.Collection`, the vendored suite never `:import`s
        // this one either, always spelling it out).
        (&["java.util.Map"][..], "java.util.Map", is_java_map),
        (&["clojure.lang.ISeq"][..], "clojure.lang.ISeq", is_seq),
        (&["clojure.lang.LazySeq"][..], "clojure.lang.LazySeq", |v| matches!(v, Value::Lazy(_))),
        (&["clojure.lang.Var"][..], "clojure.lang.Var", is_var),
        (&["clojure.lang.Atom"][..], "clojure.lang.Atom", is_atom),
        // D5 (`clojure.pprint`): `dispatch.clj` installs one
        // `simple-dispatch`/`code-dispatch` method per collection class,
        // by CLASS VALUE (`(use-method simple-dispatch
        // clojure.lang.PersistentQueue pprint-pqueue)`), so every class it
        // names has to resolve. This is the one it names that mova's table
        // did not already carry -- C10 gave `Value::Queue` its
        // `class`-name (`clojure.lang.PersistentQueue`, measured) but no
        // resolvable class row to go with it, which is exactly the gap
        // `use-method` trips over.
        (&["clojure.lang.PersistentQueue"][..], "clojure.lang.PersistentQueue", is_queue),
        // S7 (tail wave): fully-qualified only, no short alias -- like
        // `java.util.Random`/`java.util.Date` above, `java.util.function.*`
        // is not `java.lang.*` and nothing here `import`s it, so a bare
        // `Supplier` does not auto-resolve on the real JVM either.
        (
            &["java.util.function.Supplier"][..],
            "java.util.function.Supplier",
            is_supplier,
        ),
        (
            &["java.util.function.BooleanSupplier"][..],
            "java.util.function.BooleanSupplier",
            is_supplier,
        ),
        (
            &["java.util.function.IntSupplier"][..],
            "java.util.function.IntSupplier",
            is_supplier,
        ),
        (
            &["java.util.function.LongSupplier"][..],
            "java.util.function.LongSupplier",
            is_supplier,
        ),
        (
            &["java.util.function.DoubleSupplier"][..],
            "java.util.function.DoubleSupplier",
            is_supplier,
        ),
        (&["clojure.lang.BigInt"][..], "clojure.lang.BigInt", is_bigint),
        (&["BigInteger", "java.math.BigInteger"][..], "java.math.BigInteger", is_biginteger),
        (&["clojure.lang.Ratio"][..], "clojure.lang.Ratio", is_ratio),
        // kondo-wave: dropped the bare `"Pattern"` alias this row used to
        // carry alongside the fully-qualified spelling. `java.util.regex`
        // is not `java.lang`, so real JVM Clojure does NOT auto-import a
        // bare `Pattern` in every namespace (unlike `Thread`/
        // `StringBuilder`/... above, which really are `java.lang.*`) --
        // the extra alias was a Mova-only global overreach with no
        // measured need (grep-verified: no conformance corpus form uses
        // bare `Pattern`), and it broke a real, unrelated vendored
        // library on this task's path: `datalog-parser`'s `datalog.
        // parser.type` namespace defines its OWN `(defrecord Pattern
        // [source pattern])`, which real Clojure allows freely (no
        // `Pattern` import in scope, so no collision) but Mova's
        // `check_name_not_builtin_class` rejected with "Pattern already
        // refers to: class java.util.regex.Pattern" -- exactly the
        // `(definterface String)` shape that check exists for, firing on
        // a name that was never actually auto-imported for real.
        (&["java.util.regex.Pattern"][..], "java.util.regex.Pattern", is_pattern),
        // S4: `clojure.core.Vec` is `vector-of`'s OWN concrete class
        // (measured `(class (vector-of :int 1 2 3))`) -- deliberately its
        // own entry, separate from the `IPersistentVector`/
        // `PersistentVector` group above (which now ALSO answers `true`
        // for a typed vector via `is_vector`'s broadened match, a
        // documented coarse-grained approximation: real Clojure's
        // concrete `clojure.lang.PersistentVector`/`clojure.core.Vec`
        // classes are siblings, both implementing `IPersistentVector`,
        // and mova doesn't model that distinction for the shared
        // interface-name entry -- only this dedicated `clojure.core.Vec`
        // entry is exact).
        (&["clojure.core.Vec"][..], "clojure.core.Vec", is_typed_vec),
        // S4: `clojure.lang.Sorted` is the marker interface, not a
        // concrete class -- both `sorted-map` and `sorted-set` implement
        // it (measured), same shared-predicate-across-two-concrete-shapes
        // pattern `IPersistentCollection` etc. already use above.
        (&["clojure.lang.Sorted"][..], "clojure.lang.Sorted", is_sorted),
        (
            &["clojure.lang.PersistentTreeMap"][..],
            "clojure.lang.PersistentTreeMap",
            |v| matches!(v, Value::SortedMap(_)),
        ),
        (
            &["clojure.lang.PersistentTreeSet"][..],
            "clojure.lang.PersistentTreeSet",
            |v| matches!(v, Value::SortedSet(_)),
        ),
        // S5 (host-class shims). `java.util.Random`/`java.util.Date` get
        // ONLY their fully-qualified spelling -- measured: real Clojure
        // does NOT auto-resolve a bare `Random`/`Date` (neither is
        // `java.lang.*`, and nothing here `import`s them), so adding a
        // short alias would make mova MORE permissive than the JVM, a
        // real divergence, not a convenience. `Thread`/`ThreadLocal` DO
        // get a short alias below -- both really are `java.lang.*`, which
        // Clojure's compiler auto-imports short names for on every
        // platform (measured: bare `Thread`/`ThreadLocal` resolve on the
        // real JVM with no `import`).
        (&["java.util.Random"][..], "java.util.Random", is_random),
        (&["java.util.Date"][..], "java.util.Date", is_date),
        (&["java.util.UUID"][..], "java.util.UUID", is_uuid),
        // clojure-lsp campaign (mova/PLAN.md): `clojure.tools.reader`'s
        // vendored `default_data_readers.clj` (transitively required by
        // `rewrite-clj.reader`) `defmethod`s `print-method`/`print-dup`
        // onto these two alongside `java.util.Date`/`java.util.UUID`
        // above. mova has no `Calendar`/`Timestamp` value (no consumer
        // ever constructs one through this campaign's string-parsing
        // path), so `is_never_instance` -- same shape and same reasoning
        // as `java.io.StringWriter` above: exists so the dispatch-value
        // symbol resolves, not so `instance?` can answer honestly.
        (&["java.util.Calendar"][..], "java.util.Calendar", is_never_instance),
        (&["java.sql.Timestamp"][..], "java.sql.Timestamp", is_never_instance),
        // W4C-NS: fully-qualified only, same "no bare-alias auto-import"
        // reasoning as `java.util.Random`/`java.util.Date` above.
        (&["java.util.Locale"][..], "java.util.Locale", is_locale),
        (&["Thread", "java.lang.Thread"][..], "java.lang.Thread", is_thread),
        (
            &["ThreadLocal", "java.lang.ThreadLocal"][..],
            "java.lang.ThreadLocal",
            is_threadlocal,
        ),
        // C3c: fully-qualified only -- `java.util.concurrent.*` is not
        // `java.lang.*`, same "no bare-alias auto-import" reasoning as
        // `java.util.Random`/`java.util.Date` above.
        (
            &["java.util.concurrent.CyclicBarrier"][..],
            "java.util.concurrent.CyclicBarrier",
            is_cyclic_barrier,
        ),
        // kondo-wave: `java.util.concurrent.locks.ReentrantLock` --
        // clj-kondo's `impl/cache.clj` `:import`s it as `(java.util.
        // concurrent.locks ReentrantLock)` (short alias auto-derived by
        // `Interp::import_one`, same as `CyclicBarrier`'s), fully-
        // qualified only, same reasoning as `CyclicBarrier` above.
        (
            &["java.util.concurrent.locks.ReentrantLock"][..],
            "java.util.concurrent.locks.ReentrantLock",
            is_reentrant_lock,
        ),
        // lsp/kondo: `java.util.StringTokenizer` -- `(:import [java.util
        // StringTokenizer])` short-aliases it via `Interp::import_one`,
        // same as `ReentrantLock`/`CyclicBarrier` above; fully-qualified
        // also always resolves.
        (
            &["java.util.StringTokenizer"][..],
            "java.util.StringTokenizer",
            is_string_tokenizer,
        ),
        // S6 (predicates.clj batch): `StringBuilder`/`StringBuffer` are
        // BOTH `java.lang.*`, so -- like `Thread`/`ThreadLocal` above,
        // unlike `Random`/`Date` -- they get the short alias too (measured:
        // `(class (StringBuilder. "x"))` resolves with no `import` on the
        // real JVM).
        (
            &["StringBuilder", "java.lang.StringBuilder"][..],
            "java.lang.StringBuilder",
            is_stringbuilder,
        ),
        (
            &["StringBuffer", "java.lang.StringBuffer"][..],
            "java.lang.StringBuffer",
            is_stringbuffer,
        ),
        // C7 (vecveneer): `java.util.ArrayList` -- like `Random`/`Date`
        // above (NOT `java.lang.*`), only the fully-qualified spelling
        // auto-resolves on the real JVM, so only that one alias here.
        (&["java.util.ArrayList"][..], "java.util.ArrayList", is_arraylist),
        // W4-veneer (sequences.clj's `test-iteration` file-IO row):
        // `java.io.File` -- same "fully-qualified only" shape as
        // `ArrayList` above (`java.io.*` is not `java.lang.*`, no bare
        // alias auto-resolves on the real JVM either).
        (&["java.io.File"][..], "java.io.File", is_java_file),
        // lsp/io (review round 2): real streams -- same "fully-qualified
        // only" shape as `File` above.
        (&["java.io.InputStream"][..], "java.io.InputStream", is_input_stream),
        (&["java.io.OutputStream"][..], "java.io.OutputStream", is_output_stream),
        // lsp/kondo: jar/zip external-classpath reading -- fully-
        // qualified only, `ZipFile`/`JarFile` share one `HostKind`
        // (see `HostKind::JarFile`'s doc), so both spellings resolve to
        // the same canonical class predicate.
        (&["java.util.jar.JarFile"][..], "java.util.jar.JarFile", is_jar_file),
        // e2: kondo's cache lock (`with-cache`).
        (&["java.io.RandomAccessFile"][..], "java.io.RandomAccessFile", is_random_access_file),
        (&["java.nio.channels.FileChannel"][..], "java.nio.channels.FileChannel", is_file_channel),
        (&["java.nio.channels.FileLock"][..], "java.nio.channels.FileLock", is_file_lock),
        (&["java.net.URL"][..], "java.net.URL", |v| matches!(v, Value::HostInst(h) if h.kind == crate::hostclass::HostKind::Url)),
        (&["javax.net.ssl.HttpsURLConnection"][..], "javax.net.ssl.HttpsURLConnection", |v| matches!(v, Value::HostInst(h) if h.kind == crate::hostclass::HostKind::HttpConnection)),
        (&["java.net.HttpURLConnection"][..], "java.net.HttpURLConnection", |v| matches!(v, Value::HostInst(h) if h.kind == crate::hostclass::HostKind::HttpConnection)),
        (&["java.nio.channels.OverlappingFileLockException"][..], "java.nio.channels.OverlappingFileLockException", |_| false),
        (&["java.util.zip.ZipFile"][..], "java.util.jar.JarFile", is_jar_file),
        (
            &["java.util.jar.JarFile$JarFileEntry", "java.util.zip.ZipEntry", "java.util.jar.JarEntry"][..],
            "java.util.jar.JarFile$JarFileEntry",
            is_jar_entry,
        ),
        // W4-veneer (try_catch.clj's `catch-receives-checked-exception-
        // from-eval`): `java.io.FileReader` needs a resolvable class
        // symbol for `(java.io.FileReader. path)` to construct through
        // (`hostclass::construct`'s own arm always either throws
        // `FileNotFoundException` or errors "out of scope" -- see that
        // arm's doc -- so no `Value` of this class is ever actually
        // produced; `is_never_instance` is honest, matching `Math`/
        // `java.io.StringWriter`'s own marker-only rows above).
        (&["java.io.FileReader"][..], "java.io.FileReader", is_never_instance),
        // lsp/host (clojure-lsp-on-Mova campaign): `(java.io.
        // OutputStreamWriter. stream)` needs a resolvable class symbol
        // for `(new ...)`'s own `eval_form_in` of the class position to
        // succeed -- `hostclass::construct`'s arm returns the WRAPPED
        // stream/proxy unchanged (identity veneer, see that arm's own
        // doc), so no `Value` of this class is ever actually produced;
        // `is_never_instance` is honest, same rationale as `FileReader`
        // immediately above.
        (
            &["java.io.OutputStreamWriter"][..],
            "java.io.OutputStreamWriter",
            is_never_instance,
        ),
        // lsp/host: `java.util.concurrent.CancellationException`/
        // `TimeoutException` -- `promesa.core.mova`'s `cancel!` and
        // `jsonrpc4clj.server`'s `PendingRequest.get` construct these
        // directly (`(CancellationException.)`/`(TimeoutException.)`),
        // and `(catch CancellationException ..)`/`(instance?
        // CancellationException ..)` match the resulting `mk_exception`
        // instance by name -- see `hostclass::construct`'s two new
        // arms for the measured ancestor chains.
        (
            &["java.util.concurrent.CancellationException"][..],
            "java.util.concurrent.CancellationException",
            is_cancellation_exception,
        ),
        (
            &["java.util.concurrent.TimeoutException"][..],
            "java.util.concurrent.TimeoutException",
            is_timeout_exception,
        ),
        // W4-veneer (try_catch.clj's `catch-receives-checked-exception-
        // from-reflective-call`): `clojure.test.ReflectorTryCatchFixture`
        // + its nested `Cookies` throwable -- BOTH need a class-table row
        // here (not just a bare global var, unlike the exception-hierarchy
        // table in `hostclass::register`) because the vendored ns form
        // explicitly `(:import [clojure.test ReflectorTryCatchFixture
        // ReflectorTryCatchFixture$Cookies])`s them, and `import` only
        // resolves against THIS table (`class_by_full_name`, see
        // `eval::types_forms::import_one`'s doc) -- no bare alias for
        // either (like `ArrayList`/`File` above, `clojure.test.*` is not
        // `java.lang.*` and the vendored source always writes the
        // fully-qualified `(package Class)` import form, never a bare
        // symbol).
        (
            &["clojure.test.ReflectorTryCatchFixture"][..],
            "clojure.test.ReflectorTryCatchFixture",
            is_reflector_fixture,
        ),
        (
            &["clojure.test.ReflectorTryCatchFixture$Cookies"][..],
            "clojure.test.ReflectorTryCatchFixture$Cookies",
            is_reflector_cookies,
        ),
        // C10: same "fully-qualified only" shape as `ArrayList` above.
        (&["java.util.HashMap"][..], "java.util.HashMap", is_java_hashmap),
        (&["java.util.HashSet"][..], "java.util.HashSet", is_java_hashset),
        // C7 (vecveneer): `.rseq`/`.chunkedNext`'s two result classes --
        // neither is `java.lang.*`, so (like `Random`/`Date`/
        // `ArrayList` above) only the fully-qualified spelling resolves,
        // no short alias.
        (
            &["clojure.lang.APersistentVector$RSeq"][..],
            "clojure.lang.APersistentVector$RSeq",
            is_rseq,
        ),
        (&["clojure.core.VecSeq"][..], "clojure.core.VecSeq", is_vecseq_chunked),
        // W3e-3: `java.math.MathContext`, so `(set! *math-context*
        // (java.math.MathContext. 8))` -- `clojure.test-clojure.vars/
        // test-settable-math-context`, verbatim -- resolves at all. Not
        // `java.lang.*`, so (like `Random`/`Date`/`ArrayList` above) only
        // the fully-qualified spelling. `is_never_instance` for the same
        // reason `java.io.StringWriter` below has it: mova represents a
        // math context as the same plain `{:precision n :rounding-mode
        // "MODE"}` map `with-precision` has bound to `*math-context*`
        // since S5 (see `hostclass::construct`'s arm for why), and
        // answering `(instance? java.math.MathContext {:precision 1})`
        // with `true` would be a lie about every map that happens to have
        // those keys. Documented in `tests/conformance/DEVIATIONS.md`.
        (
            &["java.math.MathContext"][..],
            "java.math.MathContext",
            is_never_instance,
        ),
        // D3: see `is_never_instance`'s doc above -- these two exist only
        // so `import` resolves the vendored ns form; no short alias for
        // either (real Clojure does not auto-import `clojure.lang.*`
        // short names the way it does `java.lang.*`).
        // D3: `java.lang.Math` -- same "utility class, no instances"
        // shape as `Compiler` above, but `java.lang.*` (auto-imported on
        // the real JVM, like `Thread`/`StringBuilder` elsewhere in this
        // table), so it gets the short alias too. Needed for `(eval
        // 'java.lang.Math)` to resolve at all (vendored `evaluation.clj`'s
        // `SymbolResolution` deftest: `(is (= (eval 'java.lang.Math)
        // (class-for-name "java.lang.Math")))`) -- `Math/pow`-style
        // STATIC calls already worked with no class row at all
        // (`builtins::statics` keys those off the bare class-name string
        // directly), but resolving the bare CLASS symbol itself needed
        // this table to have an entry.
        (&["Math", "java.lang.Math"][..], "java.lang.Math", is_never_instance),
        // mova campaign (clojure-lsp): `System` -- same "static-methods-only
        // utility class, no instances" shape as `Math` above (`System/
        // currentTimeMillis`-style statics already worked with no class
        // row; resolving the bare/FQ class symbol itself, e.g. as a sci
        // `:classes {'System System}` map value, needed this row).
        (&["System", "java.lang.System"][..], "java.lang.System", is_never_instance),
        (&["clojure.lang.Compiler"][..], "clojure.lang.Compiler", is_never_instance),
        (
            &["clojure.lang.Compiler$CompilerException"][..],
            "clojure.lang.Compiler$CompilerException",
            is_never_instance,
        ),
        (&["clojure.lang.IObj"][..], "clojure.lang.IObj", is_iobj_class),
        (&["clojure.lang.IMeta"][..], "clojure.lang.IMeta", is_imeta),
        // C3c (test.clj's `clj-1102` deftest, `Class/forName "java.lang.
        // StackTraceElement"`; ALSO errors.clj's `Throwable->map-test`
        // "nil stack handled" sub-test, `(into-array StackTraceElement
        // [])`): a pure MARKER class, `pred: None` -- same shape as
        // `clojure.lang.MultiFn`'s row above (nothing mova builds is
        // ever genuinely a `StackTraceElement`; the class VALUE existing
        // at all is the whole measured need: `Class/forName` resolving
        // it without throwing, and `into-array`/`make-array`'s
        // `resolve_ref_kind` accepting it as an element-type hint --
        // both go through `ClassVal::Builtin{name,..}` generically,
        // needing no real membership predicate). Bare alias included:
        // `java.lang.StackTraceElement` auto-imports bare on the real
        // JVM (every `java.lang.*` class does), matching `Thread`/
        // `StringBuilder`/... above.
        (
            &["StackTraceElement", "java.lang.StackTraceElement"][..],
            "java.lang.StackTraceElement",
            |_| false,
        ),
        // D5 (`clojure.pprint`): `(java.io.StringWriter.)` -- the sink
        // `cl-format`/`write` build when handed a nil stream, then read
        // back with `.toString`. mova ALREADY has exactly that value:
        // `with-out-str` binds `*out*` to an `(atom "")` and
        // `strings::out_write` appends to it, so a StringWriter here IS
        // that atom (`hostclass::construct`), and the four methods
        // `cl_format` calls on one (`.write`/`.toString`/`.flush`/
        // `.append`) are Atom arms in `eval_dot_form`. No new value kind,
        // no second capture mechanism.
        //
        // `is_never_instance` because there is no way to answer
        // `(instance? java.io.StringWriter x)` honestly under that
        // representation -- an atom would have to claim to be one, and a
        // plain `(atom "")` is not. No vendored form asks (checked: zero
        // `instance?`/`isa?` sites naming StringWriter across pprint's
        // eight files and the two test files), so this row exists purely
        // to make the CONSTRUCTOR symbol resolve, exactly like the
        // `Math`/`Compiler` rows above.
        (&["java.io.StringWriter"][..], "java.io.StringWriter", is_never_instance),
        // kondo-wave: `java.io.InputStream`/`java.io.BufferedReader`/
        // `java.io.PushbackReader`/`clojure.lang.LineNumberingPushbackReader`
        // -- clj-kondo's vendored `clj-kondo.impl.toolsreader` (a fork of
        // `clojure.tools.reader`) `:import`s the first two and
        // `extend-type`s the `Reader` protocol onto `java.io.
        // PushbackReader` at namespace load time (`reader_types.clj`).
        // Same `is_never_instance`, construction/reference-only rationale
        // as `java.io.StringWriter` just above: nothing mova builds IS
        // one of these (mova has no real byte/char stream types), so the
        // whole measured need is that the CLASS SYMBOL resolves -- for
        // `:import` (`eval::special_forms`'s per-class-tolerant
        // `Some("import")` arm) and for `extend-type`'s target-class
        // position (`eval::types_forms::eval_extend_type`, which accepts
        // any `Value::Class`, not only `ClassVal::Interface` -- exactly
        // how `(extend-protocol Node Object ...)` already worked before
        // this task). No `.read`/`.close`/... method surface here:
        // clj-kondo's OWN TOOLSREADER lint path (parsing the slurped
        // source into forms) reads a plain string through its sibling
        // `push-back-reader`/`string-push-back-reader` constructors (pure
        // Clojure over a plain string+index, no real Reader), never these
        // real-stream classes. `java.io.PushbackReader`/`clojure.lang.
        // LineNumberingPushbackReader` are still constructible below
        // (`hostclass::construct`'s arms for them) for the SEPARATE,
        // narrower `*in*`/`with-in-str`/`(slurp *in*)` need -- see
        // `HostKind::StringReader`'s doc -- which is a real `Reader`, just
        // not the one clj-kondo's tokenizer itself uses.
        (&["java.io.InputStream"][..], "java.io.InputStream", is_never_instance),
        (&["java.io.BufferedReader"][..], "java.io.BufferedReader", is_never_instance),
        (&["java.io.PushbackReader"][..], "java.io.PushbackReader", is_never_instance),
        (&["java.io.Reader"][..], "java.io.Reader", is_never_instance),
        (
            &["clojure.lang.LineNumberingPushbackReader"][..],
            "clojure.lang.LineNumberingPushbackReader",
            is_never_instance,
        ),
        // lsp/io: `java.io.StringReader` -- see `HostKind::StringReader`'s
        // doc. Unlike the four rows just above (construction-only
        // passthroughs), this one is a REAL instance of something (`is_
        // string_reader`, not `is_never_instance`): `(instance? java.io.
        // StringReader (java.io.StringReader. "x"))` is genuinely true.
        (&["java.io.StringReader"][..], "java.io.StringReader", is_string_reader),
        // kondo-wave: `clj-kondo.impl.toolsreader`'s `impl/inspect.clj`
        // (a debug-formatting helper) dispatches a `defmethod` on these
        // four internal JDK collection-implementation classes directly
        // (`(defmethod inspect* clojure.lang.Cons [truncate x] ...)`) --
        // `defmethod` evaluates its dispatch-value argument eagerly at
        // namespace load, so each symbol must resolve to SOME class, real
        // membership or not. `clojure.lang.Cons` is genuinely never
        // produced by mova (`cons` builds an ordinary `Value::List`, see
        // `builtins::collections`), same `is_never_instance` rationale as
        // every other row here; the two inner `$`-named seq classes are
        // JDK implementation details nothing here could ever produce
        // either way.
        (&["clojure.lang.Cons"][..], "clojure.lang.Cons", is_never_instance),
        (
            &["clojure.lang.PersistentVector$ChunkedSeq"][..],
            "clojure.lang.PersistentVector$ChunkedSeq",
            is_never_instance,
        ),
        (
            &["clojure.lang.PersistentArrayMap$Seq"][..],
            "clojure.lang.PersistentArrayMap$Seq",
            is_never_instance,
        ),
        (
            &["clojure.lang.PersistentHashMap$NodeSeq"][..],
            "clojure.lang.PersistentHashMap$NodeSeq",
            is_never_instance,
        ),
        // kondo-wave: `default_data_readers.clj`'s `#inst` tagged-literal
        // support installs `print-method`/`print-dup` for these two on
        // top of `java.util.Date` -- again dispatch-value symbols
        // resolved eagerly at `defmethod` time. Neither is ever produced
        // by mova (`#inst` reads into `HostKind::Date`, see
        // `hostclass.rs`'s doc), so `is_never_instance`.
        (&["java.util.Calendar"][..], "java.util.Calendar", is_never_instance),
        (&["java.sql.Timestamp"][..], "java.sql.Timestamp", is_never_instance),
    ]
}

// ==================== S5 definterface: host interface table ====================
//
/// S5: host (JVM) INTERFACE names a `defrecord`/`deftype` body may name in
/// its implements position, and that `instance?`/`import` must resolve.
/// These are not classes with a membership predicate -- nothing mova
/// builds is natively one of them; the ONLY way a value is an instance is
/// by a type declaring it (see `TypeDef::interfaces`). Each entry is
/// installed as a global var under every listed spelling and interned in
/// `builtins::types::interface_class`'s table under the canonical (first)
/// spelling.
///
/// Measured on 1.13.0-alpha6 for the one row the vendored suite needs
/// (`protocols.clj`'s top-level `(defrecord MapEntry [k v]
/// java.util.Map$Entry (getKey [_] k) (getValue [_] v))`):
/// `(pr-str java.util.Map$Entry)` => `java.util.Map$Entry`,
/// `(instance? java.util.Map$Entry (MapEntry. :a 1))` => `true`,
/// `(.getKey (MapEntry. :a 1))` => `:a`.
///
/// W3d2: the method names a HOST type declares, for `reify`'s "you can't
/// define a method not on an interface/protocol/j.l.Object" rejection
/// (`protocols.clj`'s `reify-test`). `None` means "mova does not know this
/// type's method set", and `reify` then accepts any method name for it
/// rather than inventing a rejection.
///
/// Every list below was TRANSCRIBED FROM THE PINNED ORACLE, not recalled:
/// `(sort (distinct (map #(.getName %) (.getMethods java.util.List))))` on
/// real Clojure 1.13.0-alpha6 / its JDK. That matters -- a hand-written
/// list is a guess that can only ever REJECT working code, and a
/// first-draft one here was already wrong in both directions (it invented
/// `clone`/`finalize` on `Object`, which are protected and absent from
/// `getMethods`, and it missed `java.util.List`'s `SequencedCollection`
/// methods entirely). Being JDK-version-shaped is the honest cost of the
/// check; the alternative is not checking at all.
///
/// Deliberately only these three: they are exactly the host heads the
/// vendored corpus names in a `reify`/`proxy` implements position
/// (`java.util.List`, `java.util.Collection`, `Object`). Every other head
/// -- `clojure.lang.ISeq`, `clojure.lang.IReduceInit`,
/// `java.util.function.Consumer`, every protocol, every `definterface` --
/// falls through to `None` and stays permissive, so this can never reject
/// a method mova has no business judging.
pub fn host_interface_methods(name: &str) -> Option<&'static [&'static str]> {
    const OBJECT: &[&str] =
        &["equals", "getClass", "hashCode", "notify", "notifyAll", "toString", "wait"];
    const COLLECTION: &[&str] = &[
        "add", "addAll", "clear", "contains", "containsAll", "equals", "forEach", "hashCode",
        "isEmpty", "iterator", "parallelStream", "remove", "removeAll", "removeIf", "retainAll",
        "size", "spliterator", "stream", "toArray",
    ];
    const LIST: &[&str] = &[
        "add", "addAll", "addFirst", "addLast", "clear", "contains", "containsAll", "copyOf",
        "equals", "forEach", "get", "getFirst", "getLast", "hashCode", "indexOf", "isEmpty",
        "iterator", "lastIndexOf", "listIterator", "of", "parallelStream", "remove", "removeAll",
        "removeFirst", "removeIf", "removeLast", "replaceAll", "retainAll", "reversed", "set",
        "size", "sort", "spliterator", "stream", "subList", "toArray",
    ];
    // SPEC-W1 task 6: `clojure.lang.ILookup` declares exactly one method,
    // `valAt` (2- and 3-arity). Listed so a typo in a `reify`'s ILookup
    // clause is rejected at reify time rather than silently ignored --
    // same purpose the three rows above serve.
    const ILOOKUP: &[&str] = &["valAt"];
    match name {
        "java.lang.Object" => Some(OBJECT),
        "java.util.Collection" => Some(COLLECTION),
        "java.util.List" => Some(LIST),
        "clojure.lang.ILookup" => Some(ILOOKUP),
        _ => None,
    }
}

/// Deliberately minimal: an interface added here with no vendored-suite
/// call site would be unmeasured surface. Grow it from real blockers.
pub fn builtin_interfaces() -> &'static [&'static [&'static str]] {
    const MAP_ENTRY: &[&str] = &["java.util.Map$Entry", "java.util.Map.Entry"];
    // S6: `clojure.lang.Indexed` -- test.check's `rose_tree.cljc` deftype
    // target (`(deftype RoseTree [root children] clojure.lang.Indexed
    // (nth [this i] ...) (nth [this i not-found] ...))`). Measured on
    // 1.13.0-alpha6: only the one fully-qualified spelling is ever used in
    // the vendored suite, and there is no short alias to auto-import
    // (`Indexed` alone does not resolve on the real JVM either -- it's not
    // `java.lang.*`) -- so, same reasoning as `java.util.Random`/
    // `java.util.Date` above, exactly one spelling, no convenience alias.
    const INDEXED: &[&str] = &["clojure.lang.Indexed"];
    // D1 (`reify`): the six interface names the vendored suite names in
    // `reify`'s implements position (`protocols.clj`'s `reify-test`,
    // `vectors.clj`'s `remember`/`test-vec`, `sequences.clj`'s
    // `test-into-IReduceInit`). Registering a name here buys exactly two
    // things -- the symbol RESOLVES (so `(reify Consumer ...)` and
    // `(:import [java.util.function Consumer])` stop erroring) and
    // `instance?` against it consults `TypeDef::interfaces` -- and
    // nothing else: no method inventory, no JVM hierarchy, no native
    // membership (mova builds nothing that IS a `java.util.List`). The
    // BEHAVIOR each interface implies lives where the suite measures it:
    // `IReduceInit` in `Interp::seq_items`' `Inst` arm, `Consumer` in the
    // spliterator veneer, the rest only in `.method` dispatch.
    //
    // Spellings: the fully-qualified name is what `reify` heads and type
    // hints use; `import` binds the short alias per-namespace, so no
    // short spelling is listed here (same rule as `INDEXED` above).
    const LIST: &[&str] = &["java.util.List"];
    const COLLECTION: &[&str] = &["java.util.Collection"];
    const CONSUMER: &[&str] = &["java.util.function.Consumer"];
    const SPLITERATOR: &[&str] = &["java.util.Spliterator"];
    const ISEQ: &[&str] = &["clojure.lang.ISeq"];
    const IREDUCE_INIT: &[&str] = &["clojure.lang.IReduceInit"];
    // D5 (`clojure.pprint`): the two heads its five `proxy` sites name --
    // `[java.io.Writer]`, `[Writer IDeref]`, `[Writer IDeref
    // PrettyFlush]`. `java.io.Writer` is a CLASS on the JVM, not an
    // interface, but mova has no class hierarchy and `proxy` records
    // every head the same way (a name in `TypeDef::interfaces`), so the
    // distinction buys nothing here -- see `eval::types_forms::
    // eval_proxy`'s doc, difference 2. What registering them buys is
    // exactly what the other rows buy: the symbols RESOLVE, and
    // `instance?` against them asks `TypeDef::interfaces`.
    //
    // `clojure.lang.IDeref` deserves its own note, because on the JVM it
    // is implemented by atoms/refs/delays/vars/futures and here it is
    // implemented by nothing except a type that declares it. That is the
    // MEASURED-correct answer for every vendored use site: pprint's
    // `pretty-writer?` (`(and (instance? clojure.lang.IDeref x)
    // (:pretty-writer @@x))`) and `cl_format`'s `fresh-line` (`(instance?
    // clojure.lang.IDeref *out*)`) both ask it about a WRITER, to
    // distinguish "one of my own proxies, which carries its state in a
    // deref-able ref" from "a plain sink". A `java.io.StringWriter` (an
    // atom, here) must answer FALSE to both, and does -- on the JVM for
    // the same reason (a StringWriter is not IDeref). Making mova's atoms
    // answer true would break pprint, not fix it.
    //
    // `java.io.Writer` gets no short alias: it is not `java.lang.*`, so
    // real Clojure does not auto-resolve a bare `Writer` either -- both
    // vendored files that spell it bare `(import [java.io Writer])`
    // first, which binds the short name per-namespace the ordinary way.
    const WRITER: &[&str] = &["java.io.Writer"];
    const IDEREF: &[&str] = &["clojure.lang.IDeref"];
    // SPEC-W1 task 6: `clojure.lang.ILookup` -- the interface
    // `clojure.spec.alpha`'s `fspec-impl` puts in its `reify`'s implements
    // position so `(:args aspec)`/`(:ret aspec)` read the spec map off the
    // returned object. Registering the name here buys the two things every
    // other row buys (the symbol RESOLVES, and `instance?` consults
    // `TypeDef::interfaces`); the BEHAVIOR -- `get`/keyword lookup routing
    // to the declared `valAt` -- lives at the lookup sites, exactly as
    // `IReduceInit`'s lives in `seq_items`. Fully-qualified spelling only,
    // same rule as `INDEXED`/`ISEQ` above.
    const ILOOKUP: &[&str] = &["clojure.lang.ILookup"];
    // clojure-lsp campaign (mova/PLAN.md): `java.io.Closeable` -- vendored
    // `clojure.tools.reader.reader-types` (a transitive dependency of
    // `rewrite-clj.reader`, itself required by `rewrite-clj.parser`,
    // which clojure-lsp's own `clojure-lsp.parser` requires) `:import`s
    // it bare and declares it in a `deftype`'s implements position
    // (`PushbackReader`'s `close`). Same minimal treatment as every row
    // above: the symbol resolves and `instance?` consults `TypeDef::
    // interfaces`; nothing calls `.close` through the interface type in
    // any path this campaign's smoke exercises (string-based parsing
    // only), so no native behaviour is needed. No bare alias: real
    // Clojure does not auto-resolve `Closeable` either (not `java.lang.*`),
    // same rule as `WRITER`/`INDEXED` above -- the vendored file's own
    // `:import` is what binds the short name.
    const CLOSEABLE: &[&str] = &["java.io.Closeable"];
    // clojure-lsp campaign: the other three names the SAME `reader_types.
    // clj` `:import`s alongside `Closeable` (`BufferedReader`
    // bare in a `(java.io ...)` package-list; `LineNumberingPushbackReader`
    // as its own bare dotted `clojure.lang.*` spec). All three are used
    // ONLY as type hints, `instance?` targets (always false here -- mova's
    // string-based reader path, all `rewrite-clj.reader`/clojure-lsp ever
    // exercise, never constructs one) and, for `LineNumberingPushbackReader`
    // only, a bare `extend` target (`reader_types.clj:190`) registering
    // protocol methods nothing calls in that same path. No bare alias for
    // the `java.io` pair, same rule as `Closeable`/`Writer` above; the
    // `clojure.lang.*` one has no alias to begin with (its own `:import`
    // spec IS the fully-qualified name).
    const BUFFERED_READER: &[&str] = &["java.io.BufferedReader"];
    const LINE_NUMBERING_PUSHBACK_READER: &[&str] = &["clojure.lang.LineNumberingPushbackReader"];
    // Two more from the same file: `java.io.PushbackReader` is an
    // `extend-type` target (line 173, registering methods nothing calls
    // in mova's string-only reader path) and `java.io.Reader` a bare type
    // hint (`^Reader`). Fully-qualified only -- both files that mention
    // either always spell them qualified or via their own `:import`.
    const PUSHBACK_READER: &[&str] = &["java.io.PushbackReader"];
    const READER: &[&str] = &["java.io.Reader"];
    // lsp/host (clojure-lsp-on-Mova campaign): `jsonrpc4clj.server`'s
    // `PendingRequest`/`PendingReceivedRequest` records declare these
    // three heads (`clojure.lang.IBlockingDeref`/`IPending`, `java.util.
    // concurrent.Future`) alongside `clojure.lang.IDeref` -- same
    // "symbol resolves, `instance?` consults `TypeDef::interfaces`,
    // nothing else" contract as every other row here (no native
    // membership, no method inventory beyond what the `deftype`/
    // `defrecord` body itself supplies). See `atoms.rs`'s `deref`
    // dispatch note on `Value::Inst` for the one behavioral gap this
    // does NOT close (a 3-arg `(deref inst ms default)` call ignores
    // `ms`/`default` -- the record's OWN `deref-or-cancel`-style
    // protocol method, called directly rather than through generic
    // `deref`, is unaffected).
    const IBLOCKING_DEREF: &[&str] = &["clojure.lang.IBlockingDeref"];
    const IPENDING: &[&str] = &["clojure.lang.IPending"];
    const FUTURE: &[&str] = &["java.util.concurrent.Future"];
    // clojure-lsp campaign: data.priority-map's PersistentPriorityMap
    // deftype declares this head; same symbol-resolves/instance?-only
    // contract as the rows above.
    const IHASHEQ: &[&str] = &["clojure.lang.IHashEq"];
    // Same deftype, two more marker-interface heads (no methods of their
    // own): serialization + map-equivalence markers.
    const SERIALIZABLE: &[&str] = &["java.io.Serializable"];
    const MAP_EQUIVALENCE: &[&str] = &["clojure.lang.MapEquivalence"];
    // Same deftype: `Iterable` head, spelled bare (java.lang auto-import,
    // like `Object`/`Comparable` elsewhere) -- registers `.iterator`.
    const ITERABLE: &[&str] = &["Iterable", "java.lang.Iterable"];
    // Same deftype: `peek`/`pop`/`rseq` heads -- interop-only registry
    // rows, same contract as the rows above (mova's native `peek`/`pop`/
    // `rseq` fns dispatch on `Value` shape directly and do not consult
    // this registry; only `.peek`/`.pop`/`.rseq` interop would reach it).
    const IPERSISTENT_STACK: &[&str] = &["clojure.lang.IPersistentStack"];
    const REVERSIBLE: &[&str] = &["clojure.lang.Reversible"];
    // core.cache's `defcache` macro (`BasicCache`/`FIFOCache`/...
    // deftypes) declares this head; same interop-only registry rows as
    // the two above.
    const ASSOCIATIVE: &[&str] = &["clojure.lang.Associative"];
    // Same `defcache` macro: two more interop-only heads.
    const COUNTED: &[&str] = &["clojure.lang.Counted"];
    const SEQABLE: &[&str] = &["clojure.lang.Seqable"];
    // core.memoize's `check-args` macro: bare `(ancestors (class f))`
    // set-membership check against these three -- just needs the bare
    // symbol to resolve to SOME value, same rows as everywhere else.
    const RUNNABLE: &[&str] = &["java.lang.Runnable"];
    const CALLABLE: &[&str] = &["java.util.concurrent.Callable"];
    const AFN: &[&str] = &["clojure.lang.AFn"];
    const ALL: &[&[&str]] = &[
        MAP_ENTRY,
        INDEXED,
        LIST,
        COLLECTION,
        CONSUMER,
        SPLITERATOR,
        ISEQ,
        IREDUCE_INIT,
        WRITER,
        IDEREF,
        ILOOKUP,
        CLOSEABLE,
        BUFFERED_READER,
        LINE_NUMBERING_PUSHBACK_READER,
        PUSHBACK_READER,
        READER,
        IBLOCKING_DEREF,
        IPENDING,
        FUTURE,
        IHASHEQ,
        SERIALIZABLE,
        MAP_EQUIVALENCE,
        ITERABLE,
        IPERSISTENT_STACK,
        REVERSIBLE,
        ASSOCIATIVE,
        COUNTED,
        SEQABLE,
        RUNNABLE,
        CALLABLE,
        AFN,
    ];
    ALL
}
// ==================== end S5 definterface block ====================

/// Minimal `isa?` class hierarchy (measured rows only): concrete numeric
/// classes are Numbers; everything non-nil is an Object; a class isa?
/// itself.
pub fn class_isa(child: &str, parent: &str) -> bool {
    if child == parent || parent == "java.lang.Object" {
        return true;
    }
    let number_children = [
        "java.lang.Long",
        "java.lang.Integer",
        "java.lang.Short",
        "java.lang.Byte",
        "java.lang.Double",
        "java.lang.Float",
        "clojure.lang.BigInt",
        "java.math.BigInteger",
        "clojure.lang.Ratio",
        "java.math.BigDecimal",
    ];
    // D5 (`clojure.pprint`): the two `clojure.lang` collection-interface
    // rows a class-dispatching multimethod needs. `dispatch.clj` installs
    // its pretty-printing methods by INTERFACE (`(use-method
    // simple-dispatch clojure.lang.IPersistentMap pprint-map)`), and
    // `defmulti`'s `isa?`-based dispatch is what has to connect a concrete
    // `(class {:a 1})` to that interface -- without these rows every map
    // and every seq falls through to `pprint-simple-default` and prints as
    // `#object[...]` instead of being pretty-printed at all.
    //
    // Only these two, and only because they are the two mova's class table
    // does NOT already collapse: `IPersistentVector`/`IPersistentSet`
    // resolve to the concrete class name itself (`PersistentVector`/
    // `PersistentHashSet` -- see `builtin_classes()`), so `isa?` already
    // answers true for those by the identity check at the top of this fn.
    // Every child listed is one `builtin_class_name` can actually return.
    let map_children =
        ["clojure.lang.PersistentArrayMap", "clojure.lang.PersistentHashMap", "clojure.lang.PersistentTreeMap", "clojure.lang.PersistentStructMap"];
    let seq_children = [
        "clojure.lang.PersistentList",
        "clojure.lang.LazySeq",
        "clojure.lang.APersistentVector$RSeq",
        "clojure.core.VecSeq",
    ];
    match parent {
        "java.lang.Number" => number_children.contains(&child),
        "java.lang.CharSequence" => child == "java.lang.String",
        // C3c: `java.util.HashMap` extends `java.util.Map` -- NOT
        // `java.util.Collection` (measured: `Map` and `Collection` are
        // siblings under `Object`, `isa? java.util.Collection java.util.
        // Map` is `false` -- see `direct_supers`' doc for how this feeds
        // the derive-bridging isa? path too).
        "java.util.Map" => child == "java.util.HashMap",
        "clojure.lang.IPersistentMap" => map_children.contains(&child),
        "clojure.lang.ISeq" => seq_children.contains(&child),
        _ => false,
    }
}

/// C3c (multimethods.clj's `derivation-world-bridges-to-java-
/// inheritance`/`isA-multimethod-test`): the DIRECT (one-hop) builtin-
/// class superclass names for `child`, for `multi::isa_values`' derive-
/// bridging walk -- `(derive java.util.Map ::map)` then `(isa? h
/// java.util.HashMap ::map)` needs `HashMap`'s hierarchy-derived
/// ancestors checked TRANSITIVELY through its JAVA superclass chain
/// (`HashMap` -> `Map`), not just its own directly-`derive`d tags.
/// `class_isa` above only answers "is X a Y" for a fixed pair, which is
/// enough for the class-to-class case (`isa? java.util.HashMap java.util.
/// Map`) but not for walking upward to look up EACH ancestor's hierarchy
/// entry in turn -- this is that same information shaped as an
/// enumerable list instead, one level at a time (the caller recurses).
/// Deliberately minimal: only the one measured chain this task's corpus
/// needs (`HashMap` -> `Map`); every other builtin class answers `&[]`
/// (no bridging), which is honest -- growing this list is measuring a
/// new chain, not guessing one.
pub fn direct_supers(child: &str) -> &'static [&'static str] {
    match child {
        "java.util.HashMap" => &["java.util.Map"],
        // C3c (`isA-multimethod-test`'s `(foo [])` dispatching on
        // `::collection` via `(derive java.util.Collection
        // ::collection)`): `(class [])` is `clojure.lang.
        // PersistentVector`, which genuinely implements `java.util.
        // Collection` on the real JVM -- same "true" `is_java_collection`
        // above already answers for a plain vector instance, restated
        // here at the class-name level for the derive-bridging walk.
        "clojure.lang.PersistentVector" => &["java.util.Collection"],
        _ => &[],
    }
}

// ---- heap-image gate-1 accessors (src/image.rs) ----
impl ProtoDef {
    pub(crate) fn img_epoch(&self) -> u64 {
        self.epoch
    }
    pub(crate) fn img_banks(&self) -> usize {
        self.ic.banks.len()
    }
    pub(crate) fn for_image(
        var_name: Str,
        impls: HashMap<ClassKey, (Value, MethodTable)>,
        declared_methods: std::collections::HashSet<Str>,
        banks: usize,
        epoch: u64,
    ) -> ProtoDef {
        PROTO_EPOCH.fetch_max(epoch + 1, std::sync::atomic::Ordering::Relaxed);
        ProtoDef { var_name, impls, declared_methods, epoch, ic: ProtoIc::new(banks) }
    }
}
/// Throwaway: `ClassKey::Builtin` wants `&'static str`; leak the name.
pub(crate) fn static_builtin_name(n: &str) -> Option<&'static str> {
    Some(Box::leak(n.to_string().into_boxed_str()))
}
