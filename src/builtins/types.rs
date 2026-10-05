//! S3: `class`/`instance?`/`type`/`class?`/`record?`/`isa?` and the
//! protocol introspection surface (`satisfies?`/`extends?`/`extenders`/
//! `extend`) plus registration of the builtin class VARS (`Long`,
//! `String`, `Number`, `Object`, `java.lang.*` spellings, ...). The
//! special forms (`defprotocol`/`defrecord`/`deftype`/`extend-type`/
//! `extend-protocol`) live in `eval::types_forms`; the value/registry
//! types in `crate::types`. Every semantic row here was measured on
//! 1.13.0-alpha6 (see the scratchpad proto-probe transcripts referenced
//! in `crate::types`' module doc).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};

use crate::builtins::{reg, ArityHint};
use crate::error::RjError;
use crate::eval::Interp;
use crate::types::{class_key, ClassKey, ClassVal, Protocols};
use crate::value::{Keyword, Str, Symbol, Value};

/// Canonical class value per builtin class name, built once -- `class`
/// must return the SAME (`=`) value every call, and `extend-type Long`'s
/// registry entry must be that same value for `extenders` round-trips.
fn class_by_name() -> &'static HashMap<&'static str, Value> {
    static CACHE: OnceLock<HashMap<&'static str, Value>> = OnceLock::new();
    CACHE.get_or_init(|| {
        let mut m = HashMap::new();
        for (_aliases, canonical, pred) in crate::types::builtin_classes() {
            m.entry(canonical).or_insert_with(|| {
                Value::Class(Arc::new(ClassVal::Builtin {
                    name: canonical,
                    pred: Some(pred),
                }))
            });
        }
        m
    })
}

/// W4-veneer: the canonical class VALUE for one of `hostclass::
/// exception_class_rows()`'s registered exception classes, by its
/// CANONICAL (fully-qualified) name only -- `class_of`'s `Value::Inst`
/// arm is the one caller (see that fn's doc); `None` for every other
/// name, which callers fall back to `ClassVal::User` for. A separate
/// cache from `class_by_name()` below because `exception_class_rows()`
/// lives in `hostclass.rs`, registered directly as global vars by
/// `hostclass::register` rather than folded into `builtin_classes()`
/// (see that fn's own doc for why) -- this cache mirrors ONLY the
/// canonical-name half of that registration, for lookup without an
/// `Interp`.
fn exception_class_by_name(name: &str) -> Option<Value> {
    static CACHE: OnceLock<HashMap<&'static str, Value>> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            crate::hostclass::exception_class_rows()
                .iter()
                .map(|(_aliases, canonical, pred)| {
                    (*canonical, Value::Class(Arc::new(ClassVal::Builtin { name: canonical, pred: Some(*pred) })))
                })
                .collect()
        })
        .get(name)
        .cloned()
}

/// S4: the class VALUE for a full class name (`java.lang.Boolean`,
/// `Long`'s CANONICAL spelling `java.lang.Long`, ...), for `import` to
/// resolve against -- `import`'s spec always carries a fully-qualified
/// name (`(java.lang Boolean)`'s package + short name, or a bare already-
/// dotted symbol), never one of `builtin_classes`' short aliases, so this
/// looks up the CANONICAL column only, not every alias spelling `class_
/// by_name`'s cache also answers to.
pub(crate) fn class_by_full_name(name: &str) -> Option<Value> {
    class_by_name()
        .get(name)
        .cloned()
        // S5: `import`'s resolution ALSO sees interfaces -- both the host
        // rows in `types::builtin_interfaces()` and anything a
        // `definterface` has already minted (`(:import
        // [clojure.test_clojure.protocols.examples ExampleInterface])`
        // is exactly that shape in the vendored `protocols.clj`).
        .or_else(|| interface_class_if_known(name))
}

// ==================== S5 definterface: interface interning ====================

/// Every interface class VALUE ever minted, interned by full name.
/// Interning is what makes `ClassVal::Interface`'s name-based identity
/// and `Arc`-pointer identity agree, which in turn lets an interface be
/// used as a `HashMap` key / `extend-type` target without surprises.
fn interface_table() -> &'static std::sync::Mutex<HashMap<Str, Value>> {
    static TABLE: OnceLock<std::sync::Mutex<HashMap<Str, Value>>> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut m = HashMap::new();
        for spellings in crate::types::builtin_interfaces() {
            let canonical = Str::from(spellings[0]);
            let cv = Value::Class(Arc::new(ClassVal::Interface {
                name: canonical.clone(),
            }));
            m.insert(canonical, cv);
        }
        std::sync::Mutex::new(m)
    })
}

/// The interned interface class value for `name`, MINTING one if this is
/// the first time the name is seen (`definterface`'s path).
pub(crate) fn interface_class(name: &str) -> Value {
    let mut table = crate::sync::lock_mutex(interface_table());
    let key = Str::from(name);
    table
        .entry(key.clone())
        .or_insert_with(|| Value::Class(Arc::new(ClassVal::Interface { name: key })))
        .clone()
}

/// Like `interface_class` but never mints -- for resolution paths
/// (`import`) that must fail honestly on an unknown name rather than
/// conjure an interface nothing declared.
pub(crate) fn interface_class_if_known(name: &str) -> Option<Value> {
    crate::sync::lock_mutex(interface_table())
        .get(&Str::from(name))
        .cloned()
}

// ==================== C14 (protocols): generated-interface reflection ====================
//
// `protocols.clj`'s `method-names` helper (`(->> (.getMethods c) (map
// #(.getName %)) sort)`) reflects on a protocol's GENERATED interface --
// real Clojure compiles every `defprotocol` to an actual JVM interface
// (`ns.ProtocolName`, dashes in the ns munged to `_` exactly like
// `defrecord`/`deftype`'s own class names -- see `eval_deftype_like`'s
// doc), one method per SIGNATURE ARITY (`baz`'s two arglists become two
// distinct `Method` objects, both named "baz" -- measured:
// `["bar" "baz" "baz" "foo" "with_quux"]`, `baz` appearing twice),
// method names using `_` instead of `-` (`with-quux` -> `with_quux`,
// mirrors the class-name munge). mova has no JVM, hence no real
// `java.lang.reflect.Method` -- this table is the minimal veneer: at
// `defprotocol` time (`eval::types_forms::eval_defprotocol`), the full
// munged interface name is registered here against `(munged-method-name,
// arity-count)` pairs, purely so `.getMethods` (`eval_dot_form`'s
// `Value::Class` arm) can rebuild the right MULTISET of method names
// without needing any other reflection machinery.
fn protocol_iface_methods() -> &'static std::sync::Mutex<HashMap<Str, Vec<(Str, usize)>>> {
    static TABLE: OnceLock<std::sync::Mutex<HashMap<Str, Vec<(Str, usize)>>>> = OnceLock::new();
    TABLE.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

/// Records `iface_full`'s method table -- called once per `defprotocol`.
pub(crate) fn register_protocol_iface_methods(iface_full: Str, methods: Vec<(Str, usize)>) {
    crate::sync::lock_mutex(protocol_iface_methods()).insert(iface_full, methods);
}

/// `Some(methods)` iff `iface_full` names a known protocol-generated
/// interface (registered by `register_protocol_iface_methods` above);
/// `None` for anything else (a plain `definterface`, an unknown name, ...)
/// -- `eval_dot_form`'s `.getMethods` arm falls through to the ordinary
/// "no field or interface method" error in that case, same as every
/// other unimplemented dot-method.
pub(crate) fn protocol_iface_lookup(iface_full: &str) -> Option<Vec<(Str, usize)>> {
    crate::sync::lock_mutex(protocol_iface_methods())
        .get(&Str::from(iface_full))
        .cloned()
}

/// The pseudo-`java.lang.reflect.Method` `TypeDef` -- a tiny INTERNAL
/// record type never exposed for construction, existing purely so
/// `.getName` on one of `.getMethods`' results reaches the ordinary
/// generic record-field-read path (`eval::types_forms::inst_field`) for
/// free: the basis field is literally named `getName`, so `(.getName m)`
/// resolves as an everyday record field access, no new dot-method
/// dispatch needed at all.
pub(crate) fn reflect_method_tdef() -> Arc<crate::types::TypeDef> {
    static TDEF: OnceLock<Arc<crate::types::TypeDef>> = OnceLock::new();
    TDEF.get_or_init(|| {
        Arc::new(crate::types::TypeDef {
            name: Str::from("java.lang.reflect.Method"),
            basis: vec![Str::from("getName")],
            is_record: true,
            interfaces: Vec::new(),
            field_tags: Vec::new(),
            mutable: Vec::new(),
            methods: Default::default(),
            protocols: Vec::new(),
        })
    })
    .clone()
}

/// Builds `.getMethods`' PVec result for a known protocol interface:
/// one pseudo-`Method` instance per `(name, arity-count)` row, repeated
/// `arity-count` times (measured: `baz`'s two arglists are two SEPARATE
/// `Method` objects, both named "baz").
pub(crate) fn reflect_methods_of(iface_full: &str) -> Option<Value> {
    let rows = protocol_iface_lookup(iface_full)?;
    let tdef = reflect_method_tdef();
    let mut out = crate::value::PVec::new();
    for (name, arity_count) in rows {
        for _ in 0..arity_count {
            let mut data = crate::value::PMap::new();
            data.insert(Value::Keyword(Keyword::from("getName")), Value::Str(name.clone()));
            out.push_back(Value::Inst(Arc::new(crate::types::InstVal {
                tdef: tdef.clone(),
                data,
                fields: std::sync::Mutex::new(crate::value::PVec::new()),
                meta: None,
            })));
        }
    }
    Some(Value::Vector(out))
}

/// Does `v` implement the interface named `name`? The general membership
/// rule (mova has no JVM class hierarchy): the value is a `defrecord`/
/// `deftype` instance whose `TypeDef` listed the name in its implements
/// position.
///
/// S7 adds the one NATIVE membership row: `Value::MapEntry` implements
/// `java.util.Map$Entry` intrinsically, not by declaration -- measured,
/// `(instance? java.util.Map$Entry (first {:a 1}))` is `true`. It has to
/// live here rather than in `crate::types::builtin_classes` because
/// `java.util.Map$Entry` is registered as a `ClassVal::Interface` (a
/// `defrecord` may declare it -- `protocols.clj` does), and an interface
/// class carries no membership predicate by construction. The
/// `IMapEntry`/`MapEntry` spellings go the OTHER way, through
/// `builtin_classes`' `is_map_entry`, since nothing declares those.
// ==================== C14 (protocols): inline-vs-extended tracking ====================
//
// Real Clojure rejects `(extend SomeType SomeProtocol {...})` when
// `SomeType` already implements `SomeProtocol` INLINE (declared directly
// in its `deftype`/`defrecord` body, `IllegalArgumentException: class ..
// already directly implements interface .. for protocol:..`) --
// `protocols.clj`'s `illegal-extending` deftest. `eval::types_forms::
// register_protocol_impls` (used by `extend-type`/`extend-protocol`/the
// native `extend` fn) and `eval_deftype_like`'s own INLINE registration
// share one underlying impl table with no such distinction otherwise, so
// this side table records which `(protocol, class)` pairs came from the
// INLINE path, checked ONLY by the non-inline callers.
fn inline_protocol_marks() -> &'static std::sync::Mutex<HashSet<(usize, ClassKey)>> {
    static TABLE: OnceLock<std::sync::Mutex<HashSet<(usize, ClassKey)>>> = OnceLock::new();
    TABLE.get_or_init(|| std::sync::Mutex::new(HashSet::new()))
}

pub(crate) fn mark_inline_protocol_impl(proto_key: usize, ck: ClassKey) {
    crate::sync::lock_mutex(inline_protocol_marks()).insert((proto_key, ck));
}

pub(crate) fn is_inline_protocol_impl(proto_key: usize, ck: &ClassKey) -> bool {
    crate::sync::lock_mutex(inline_protocol_marks()).contains(&(proto_key, ck.clone()))
}

pub(crate) fn implements_interface(v: &Value, name: &Str) -> bool {
    if matches!(v, Value::MapEntry(_)) && (&**name == "java.util.Map$Entry" || &**name == "java.util.Map.Entry")
    {
        return true;
    }
    matches!(v, Value::Inst(inst) if inst.tdef.interfaces.iter().any(|i| i == name))
}

/// SPEC-W1 task 6: `clojure.lang.ILookup`'s lookup hook.
///
/// `Some(..)` iff `v` is an instance whose type DECLARED
/// `clojure.lang.ILookup` and supplied a `valAt` -- in which case the
/// result is that method applied to `(this k)` or `(this k not-found)`,
/// exactly as `RT.get`/`RT.getFrom` would call it on the JVM. `None` for
/// everything else, so each call site keeps its own existing behavior
/// untouched (records still read their own `data` map, a plain `deftype`
/// still falls to the default).
///
/// Both arities are the caller's to choose and neither is synthesized
/// from the other: `(get o k)` calls the 2-arity and `(get o k nf)` the
/// 3-arity, matching the interface, and matching what upstream code
/// provides (`clojure.spec.alpha`'s `fspec-impl` reifies BOTH, `(valAt
/// [this k] (get specs k))` and `(valAt [_ k not-found] (get specs k
/// not-found))`). A type that declares only one of them gets an ordinary
/// arity error from the reified fn itself, same as any other under-arity
/// call.
///
/// The `Value::Inst` shape check comes first and is one enum-tag test, so
/// the hot keyword/`get` paths (maps, host structs) pay nothing measurable
/// for this hook.
pub(crate) fn ilookup_val_at(
    interp: &mut Interp,
    v: &Value,
    key: &Value,
    not_found: Option<Value>,
) -> Option<Result<Value, RjError>> {
    // SPEC-W3: metadata is TRANSPARENT here, exactly as it already is to
    // class and protocol dispatch (`types::class_key`, `lookup_method`,
    // `satisfies?` -- the E2 fix the port wave landed). On the JVM
    // `(with-meta o m)` on a `reify` returns a new object of the SAME
    // class implementing the same interfaces, so `RT.get` reaches the
    // same `valAt`. mova models the metadata as a wrapper, so peeling it
    // for the shape test is what reproduces that. Measured before the
    // fix: `(get (with-meta r {}) :a)` worked while `(:a (with-meta r
    // {}))` answered `nil` -- and `clojure.spec.alpha`'s `with-name`
    // wraps EVERY registered spec, so `(:args (s/get-spec ::an-fspec))`
    // (upstream's own spelling, `fspec-impl`) read as `nil`.
    //
    // `this` stays the value as GIVEN, metadata and all: that is the
    // object the JVM would hand the method, and a `valAt` body that
    // reads `(meta this)` must see it.
    let Value::Inst(inst) = v.unmeta() else {
        return None;
    };
    if !inst
        .tdef
        .interfaces
        .iter()
        .any(|i| i.as_ref() == "clojure.lang.ILookup")
    {
        return None;
    }
    let f = lookup_interface_method(&interp.interfaces, inst, "valAt")?;
    let mut args = Vec::with_capacity(3);
    args.push(v.clone());
    args.push(key.clone());
    if let Some(nf) = not_found {
        args.push(nf);
    }
    Some(interp.apply_value(&f, &args, crate::reader::Span { start: 0, end: 0 }))
}

/// clojure-lsp campaign (mova/PLAN.md): a `defrecord`/`deftype`'s own
/// `Object (toString [this] ...)` override, if it declared one --
/// registered under the "java.lang.Object" pseudo-interface name (see
/// `eval::types_forms::eval_deftype_like`'s `is_object_class` handling,
/// which files such a group into the same interface-method registry
/// `definterface` impls use). `None` for every value with no such
/// override -- callers keep their existing fallback untouched.
///
/// `str`'s own call site (`builtins::strings::str_concat`) uses this so
/// `(str x)` matches real Clojure's `.toString()` semantics -- which
/// `str` calls directly, independent of `print-method` -- instead of
/// mova's generic `#ns.R{...}` record dump for any `Value::Inst` that
/// customizes it. Measured via rewrite-clj, whose ~20 node types all
/// override `toString` this way (`(toString [node] (node/string
/// node))`), and whose `borkdude.rewrite-edn.impl/recalc-positional-
/// metadata` calls bare `str` on a raw node (`(-> node str p/parse-
/// string-all)`).
///
/// Same metadata-transparency as `ilookup_val_at` just above: `with-meta`
/// on the JVM returns an object of the same class, so `(str (with-meta
/// node {}))` must reach the same override.
pub(crate) fn inst_to_string_override(interp: &mut Interp, v: &Value) -> Option<Result<Value, RjError>> {
    let unmeta = v.unmeta().clone();
    let Value::Inst(inst) = &unmeta else {
        return None;
    };
    if !inst.tdef.interfaces.iter().any(|i| i.as_ref() == "java.lang.Object") {
        return None;
    }
    let f = lookup_interface_method(&interp.interfaces, inst, "toString")?;
    Some(interp.apply_value(&f, &[unmeta], crate::reader::Span { start: 0, end: 0 }))
}

/// Records `methods` as `tdef`'s impl of the interface named
/// `iface_name`. Re-`defrecord`ing a type mints a fresh `TypeDef`, hence
/// a fresh key -- old instances keep dispatching to the old table,
/// matching the `Arc`-identity rule `TypeDef`'s own doc states.
pub(crate) fn register_interface_impls(
    interfaces: &crate::types::Interfaces,
    iface_name: &Str,
    tdef: &Arc<crate::types::TypeDef>,
    methods: crate::types::MethodTable,
) {
    crate::sync::lock_write(&interfaces.0)
        .insert((iface_name.clone(), Arc::as_ptr(tdef) as usize), methods);
}

/// The `.method` interop impl for `inst`, searched across the interfaces
/// its `TypeDef` declared, in DECLARATION order. No vendored form
/// declares one method name on two interfaces of a single type, so the
/// order is a stable, documented tie-break rather than a measured
/// semantic claim.
pub(crate) fn lookup_interface_method(
    interfaces: &crate::types::Interfaces,
    inst: &crate::types::InstVal,
    method: &str,
) -> Option<Value> {
    // D1: a `reify` instance carries its impls on its own (anonymous)
    // `TypeDef` rather than in the registry below -- see `types::
    // TypeDef::methods`' doc. Empty for every named type, so the guard
    // costs one `len` check on the ordinary `deftype` path.
    if !inst.tdef.methods.is_empty() {
        if let Some(f) = inst.tdef.methods.get(method) {
            return Some(f.clone());
        }
    }
    if inst.tdef.interfaces.is_empty() {
        return None; // overwhelmingly common: skip the lock entirely
    }
    let reg = crate::sync::lock_read(&interfaces.0);
    let tid = Arc::as_ptr(&inst.tdef) as usize;
    for iface in &inst.tdef.interfaces {
        if let Some(f) = reg.get(&(iface.clone(), tid)).and_then(|t| t.get(method)) {
            return Some(f.clone());
        }
    }
    None
}

// ==================== end S5 definterface block ====================

// ==================== S6: clojure.lang.MultiFn ====================
//
/// S6: `(defmulti m ...)`'s dispatch value is an ordinary `Value::Native`
/// -- the SAME representation every other native/builtin fn uses (`+`,
/// `map`, ...) -- so, unlike every other row in `builtin_classes()`,
/// membership can't be a pure `fn(&Value) -> bool`: nothing about the
/// `Value` itself says "I'm a multimethod's dispatch fn", only whether
/// its `Arc` pointer is a live key in `interp.multimethods`. That needs
/// `Interp` access, which the shared `pred: fn(&Value) -> bool` slot
/// can't carry -- so this is checked as a special case in `instance?`,
/// not wired through `builtin_classes()`'s generic table (see that
/// call site's comment for why the ordering matters).
///
/// Measured on 1.13.0-alpha6 (`vendor-libs/clojure/test/check/
/// clojure_test.cljc`'s `(instance? clojure.lang.MultiFn ct/report)`):
/// `(instance? clojure.lang.MultiFn (defmulti m :k))` => `true`,
/// `(instance? clojure.lang.MultiFn (fn [x] x))` => `false`, and
/// `(instance? clojure.lang.MultiFn +)` => `false` -- a core builtin is
/// `Value::Native` too, but it never lives in `interp.multimethods`, so
/// this pred stays honest rather than degenerating to "is this any
/// native at all".
pub(crate) const MULTIFN_CLASS_NAME: &str = "clojure.lang.MultiFn";

pub(crate) fn is_multifn(interp: &Interp, v: &Value) -> bool {
    match crate::multi::multi_key(v) {
        Some(key) => crate::sync::lock_read(&interp.multimethods.0).contains_key(&key),
        None => false,
    }
}
// ==================== end S6: clojure.lang.MultiFn ====================

/// S4/1D: cache of `Class` values for `dims > 1` arrays, keyed by
/// `(kind, dims)` -- see `class_of`'s `Value::Array` arm. Separate from
/// `class_by_name`/`MARKERS` above because a `dims > 1` name isn't a
/// literal `&'static str` anywhere in the source (`types::
/// array_jvm_name_dims` builds it at runtime); `Box::leak` mints one
/// exactly once per distinct `(kind, dims)` pair actually seen -- a
/// bounded set (finitely many `ArrayKind`s x realistic dims), so this
/// cannot leak unboundedly the way leaking on every `class` CALL would.
fn array_class_cache() -> &'static std::sync::Mutex<HashMap<(crate::value::ArrayKind, u32), Value>> {
    static CACHE: OnceLock<std::sync::Mutex<HashMap<(crate::value::ArrayKind, u32), Value>>> = OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

fn array_class_cached(kind: crate::value::ArrayKind, dims: u32) -> Value {
    let mut cache = crate::sync::lock_mutex(array_class_cache());
    cache
        .entry((kind, dims))
        .or_insert_with(|| {
            let name: &'static str = Box::leak(crate::types::array_jvm_name_dims(&kind, dims).into_owned().into_boxed_str());
            Value::Class(Arc::new(ClassVal::Builtin { name, pred: None }))
        })
        .clone()
}

/// `(class x)` -- measured names in `types::builtin_class_name`. Classes
/// `class` can return but no var spells (e.g. `clojure.lang.
/// PersistentList`) still come from the same canonical cache; a name
/// outside the `builtin_classes` table gets a pred-less marker class.
pub(crate) fn class_of(v: &Value) -> Value {
    match v {
        Value::Nil => Value::Nil,
        // W4-veneer (try_catch.clj's `catch-receives-checked-exception-
        // from-eval`, `(= java.io.FileNotFoundException (type e))`): a
        // `mk_exception`-built instance (`FileNotFoundException`,
        // `ArityException`, the five S6 exceptions, ...) is an ordinary
        // `Value::Inst` like a user `defrecord`/`deftype`, but its
        // `tdef.name` names one of `hostclass::exception_class_rows()`'s
        // REGISTERED classes -- check that table FIRST (measured gap:
        // before this, `(type (IllegalArgumentException. "x"))` minted a
        // FRESH `ClassVal::User` every call, `Arc`-identity-keyed, never
        // `=` to the canonical `ClassVal::Builtin` the bare
        // `IllegalArgumentException` var resolves to, even though both
        // print identically -- nothing exercised that literal-`=`-on-
        // class-value shape before this task; every existing consumer
        // goes through `instance?`/typed `catch`, which already matched
        // by NAME, not by this identity). Falls through to the ordinary
        // `ClassVal::User` mint for every other `Value::Inst` (a real
        // user `defrecord`/`deftype`, whose name will not coincidentally
        // collide with one of these dotted host-class names).
        Value::Inst(inst) => exception_class_by_name(inst.tdef.name.as_ref())
            .unwrap_or_else(|| Value::Class(Arc::new(ClassVal::User(inst.tdef.clone())))),
        // SPEC-PORT: metadata does not change a value's class -- measured,
        // `(class (with-meta (->R 1) {:a 1}))` is `user.R` on the JVM.
        // Before this, a `with-meta`'d record/reify fell through to
        // `builtin_class_name`, which unwraps `Value::Meta` straight into
        // its `unreachable!("Inst handled by class_key")` arm and
        // PANICKED the evaluator thread.
        Value::Meta(m) if matches!(m.inner, Value::Inst(_)) => class_of(&m.inner),
        // S4/1D: `builtin_class_name`'s own `Value::Array` arm is
        // dims-BLIND (always the 1-dim raw name, see its doc) -- correct
        // for the overwhelmingly common `dims == 1` case, which reuses
        // the ordinary `class_by_name`/`MARKERS` path below untouched via
        // the `other` fallthrough. A `dims > 1` array (only reachable via
        // `make-array`'s multi-dim form or `to-array-2d`) needs a
        // DIFFERENT, dims-aware name, cached separately so `class` still
        // returns the SAME (`=`) value every call for a given (kind,
        // dims) pair.
        Value::Array(arr) if arr.dims > 1 => array_class_cached(arr.kind, arr.dims),
        other => {
            let name = crate::types::builtin_class_name(other);
            class_by_name().get(name).cloned().unwrap_or_else(|| {
                static MARKERS: OnceLock<std::sync::Mutex<HashMap<&'static str, Value>>> =
                    OnceLock::new();
                let mut markers = MARKERS
                    .get_or_init(|| std::sync::Mutex::new(HashMap::new()))
                    .lock()
                    .expect("marker class cache poisoned");
                markers
                    .entry(name)
                    .or_insert_with(|| {
                        Value::Class(Arc::new(ClassVal::Builtin { name, pred: None }))
                    })
                    .clone()
            })
        }
    }
}

/// Resolves a protocol MAP value to its registry key (the `:var` cell's
/// address) or errors with the fn name -- shared by `satisfies?`/
/// `extends?`/`extenders`/`extend` and the dispatch path.
pub(crate) fn proto_key(proto: &Value, who: &str) -> Result<usize, RjError> {
    if let Value::Map(m) = proto {
        if let Some(Value::Var(cell)) = m.get(&Value::Keyword(Keyword::from("var"))) {
            return Ok(Arc::as_ptr(cell) as usize);
        }
    }
    // W3a/W4B-MESSAGES: measured -- `(extend SomeClass
    // java.lang.Comparable {..})` => `java.lang.IllegalArgumentException:
    // interface java.lang.Comparable is not a protocol` (measured again
    // this session, compat/w4b-protocols-oracle-transcript.txt's second
    // probe, against a `definterface`-minted interface rather than a
    // built-in one -- same shape). `proto` reaching here as a
    // `Value::Class` is EXACTLY this condition: `extend`'s second
    // argument was a real (interface) class, not a protocol map, so its
    // own qualified name is the interface real Clojure names. Any OTHER
    // non-protocol shape (nothing in this corpus reaches `extend` with
    // one) keeps the prior generic wording rather than guessing a real
    // message nothing measures.
    if let Value::Class(c) = proto {
        return Err(RjError::type_err(format!("interface {} is not a protocol", c.name()))
            .with_class(crate::error::JvmClass::IllegalArgument));
    }
    Err(RjError::type_err(format!(
        "{who}: expected a protocol, got {}",
        proto.type_name()
    ))
    .with_class(crate::error::JvmClass::IllegalArgument))
}

/// The measured dispatch rule: exact class key, else (for non-nil values
/// only) `Object`. `nil` is NEVER caught by `Object` (measured: `(m nil)`
/// with only an Object impl errors "for class: nil", and `(satisfies? P
/// nil)` is false with Object extended).
///
/// W-PROTO: `epoch`/`midx` are the dispatch fn's inline-cache coordinates
/// (`ProtoDef::epoch` and its index into that protocol's declared method
/// order, both captured when `eval_defprotocol` minted the fn). They only
/// ever select a FASTER route to the same answer -- pass a mismatched
/// epoch and every lookup simply takes the uncached path below. See
/// `types::ProtoIc` for the cache's shape and its ordering argument.
pub(crate) fn lookup_method(
    protocols: &Protocols,
    key: usize,
    epoch: u64,
    midx: usize,
    v: &Value,
    method: &str,
) -> Option<Value> {
    // SPEC-PORT: metadata is transparent to protocol dispatch.
    // `(with-meta x {..})` wraps `x` in a `Value::Meta` carrier, but on
    // the JVM the result is an object of the SAME class implementing the
    // SAME protocols -- so the wrapper is peeled here, once, ahead of
    // both the reify table below and the `class_key` walk. Without it a
    // named `clojure.spec.alpha` spec (`with-name` stores `::name` in a
    // reify's metadata) stopped being a spec the moment it was
    // registered, and `class_key` panicked on the wrapped `Inst`.
    let v = v.unmeta();
    // D1: a `reify` of a PROTOCOL registers nothing in `protocols` (its
    // impls close over the call site and live on its own anonymous
    // `TypeDef` -- see `types::TypeDef::methods`), so the instance's own
    // table is consulted first. It wins over any registered impl, which
    // is the right precedence: `reify` is as specific as a dispatch
    // target gets. Empty for every named type, so ordinary `deftype`/
    // `extend-type` dispatch pays one `len` check.
    if let Value::Inst(inst) = v {
        if !inst.tdef.methods.is_empty() {
            if let Some(f) = inst.tdef.methods.get(method) {
                return Some(f.clone());
            }
        }
    }
    let reg = crate::sync::lock_read(&protocols.0);
    let proto = reg.get(&key)?;
    // W-PROTO: the inline cache covers exactly the shape it can key
    // safely -- a NAMED user type (`defrecord`/`deftype`), identified by
    // its `TypeDef` address and pinned by a strong `Arc` in the entry.
    //
    // Deliberately NOT cached:
    //   * `reify` (`!inst.tdef.methods.is_empty()`) -- a fresh `TypeDef`
    //     per EVALUATION, so caching it would fill the bank with entries
    //     no second call can ever hit, and would keep every one of those
    //     anonymous types alive through its pin. Note the early-return
    //     block above already served the reify hit; only a reify that
    //     does NOT implement THIS method reaches here, and it still gets
    //     the uncached walk (its `Object`-row fallback, typically).
    //   * builtin/interface/`nil` class keys -- `ClassKey::Builtin` is a
    //     `&'static str`, so those WOULD be safe to intern (no ABA
    //     hazard at all). They are left out because the whole registry
    //     walk they pay for measured ~40ns/call on this machine
    //     (`(extend-type java.lang.Long P ..)` dispatch, 0.48s per 2M
    //     in-loop calls, against 0.40s for the same protocol answered
    //     from a `reify`'s own table) -- not enough to justify a second
    //     key space in every bank. They take the walk below.
    let cacheable = match v {
        Value::Inst(inst) if inst.tdef.methods.is_empty() && proto.epoch == epoch => Some(&inst.tdef),
        _ => None,
    };
    if let Some(tdef) = cacheable {
        if let Some(f) = proto.ic.probe(midx, Arc::as_ptr(tdef) as usize) {
            return Some(f.clone());
        }
    }
    let ck = class_key(v);
    // Same two-step the uncached path always did, restated as one
    // expression so the result can be interned before it is returned:
    // exact class key first, then (non-`nil` only) the `Object` row.
    let found = proto
        .impls
        .get(&ck)
        .and_then(|(_cls, table)| table.get(method).cloned())
        .or_else(|| {
            if matches!(v, Value::Nil) {
                return None;
            }
            proto
                .impls
                .get(&ClassKey::Object)
                .and_then(|(_cls, table)| table.get(method).cloned())
        });
    // A no-impl result is never interned: it is the error path, it is not
    // hot, and leaving it out keeps "the bank only ever holds answers
    // that were true under the current `impls`" true by construction.
    if let (Some(tdef), Some(f)) = (cacheable, &found) {
        proto.ic.install(midx, tdef, f);
    }
    found
}

/// Class-key for an evaluated `extend-type`/`extend`/`extends?` class
/// argument: a `Class` value, or literal `nil` (its own dispatch row).
pub(crate) fn key_for_class_value(cv: &Value, who: &str) -> Result<ClassKey, RjError> {
    match cv {
        Value::Nil => Ok(ClassKey::Nil),
        Value::Class(c) => Ok(match c.as_ref() {
            ClassVal::Builtin { name, .. } if *name == "java.lang.Object" => ClassKey::Object,
            ClassVal::Builtin { name, .. } => ClassKey::Builtin(name),
            ClassVal::User(t) => ClassKey::User(Arc::as_ptr(t) as usize),
            ClassVal::Interface { name } => ClassKey::Interface(name.clone()),
        }),
        other => Err(RjError::type_err(format!(
            "{who}: expected a class (or nil), got {}",
            other.type_name()
        ))),
    }
}

pub fn install(i: &mut Interp) {
    // The builtin class VARS: every alias spelling binds the same
    // canonical class value (`Long` and `java.lang.Long` are `=`).
    for (aliases, canonical, _pred) in crate::types::builtin_classes() {
        let cv = class_by_name()
            .get(canonical)
            .expect("canonical name always cached")
            .clone();
        for alias in aliases {
            i.globals.set(Symbol::simple(*alias), cv.clone());
        }
    }

    // ==================== S5 definterface block ====================
    // Host interface VARS -- same "every alias spelling binds the same
    // interned value" rule as the builtin classes directly above, so a
    // vendored file can name `java.util.Map$Entry` in a `defrecord`
    // implements position and have the symbol simply resolve.
    for spellings in crate::types::builtin_interfaces() {
        let cv = interface_class(spellings[0]);
        for alias in *spellings {
            i.globals.set(Symbol::simple(*alias), cv.clone());
        }
    }
    // ==================== end S5 definterface block ====================

    // S6: `clojure.lang.MultiFn` -- a class row usable with `instance?`
    // only (see `is_multifn`'s doc for why it can't join `builtin_classes()`
    // and ride the shared `pred: fn(&Value)->bool` table). `pred: None`
    // here is just the marker-class shape every other row uses for
    // "membership isn't a plain predicate" -- `instance?`'s own match
    // special-cases this name before it would ever reach the generic
    // `pred: None` fallback.
    i.globals.set(
        Symbol::simple(MULTIFN_CLASS_NAME),
        Value::Class(Arc::new(ClassVal::Builtin {
            name: MULTIFN_CLASS_NAME,
            pred: None,
        })),
    );

    reg(i, "class", ArityHint::Exact(1), |_i, args| Ok(class_of(&args[0])));
    // `(type x)` is `(:type (meta x))` else `(class x)` -- mova has no
    // value metadata yet, so `class` is the whole measured behavior for
    // every corpus-expressible input.
    reg(i, "type", ArityHint::Exact(1), |_i, args| Ok(class_of(&args[0])));

    reg(i, "class?", ArityHint::Exact(1), |_i, args| {
        Ok(Value::Bool(matches!(&args[0], Value::Class(_))))
    });

    reg(i, "record?", ArityHint::Exact(1), |_i, args| {
        Ok(Value::Bool(
            matches!(&args[0], Value::Inst(inst) if inst.tdef.is_record),
        ))
    });

    reg(i, "instance?", ArityHint::Exact(2), |i, args| {
        // S5/M3: the RECEIVER here is `args[1]`, not `args[0]` (which is
        // the class), so this can't use `reg_unmeta`. Metadata never
        // changes a value's class -- `(instance? clojure.lang.
        // IPersistentVector (with-meta [1] {:a 1}))` is `true`, measured
        // -- so the test looks through it.
        let subject = args[1].unmeta();
        let res = match (&args[0], subject) {
            (Value::Class(c), x) => match c.as_ref() {
                // S6: checked BEFORE the generic `pred: None` name-
                // comparison arm below, which it deliberately does NOT
                // fall through to -- a bare name comparison would call
                // every builtin `Value::Native` (`+`, `map`, ...) a
                // MultiFn too, which is exactly the dishonest result
                // `is_multifn`'s doc measures against.
                ClassVal::Builtin { name, .. } if *name == MULTIFN_CLASS_NAME => is_multifn(i, x),
                ClassVal::Builtin { pred: Some(p), .. } => p(x),
                ClassVal::Builtin { pred: None, name } => {
                    crate::types::builtin_class_name(x) == *name && !matches!(x, Value::Nil)
                }
                ClassVal::User(t) => {
                    matches!(x, Value::Inst(inst) if Arc::ptr_eq(&inst.tdef, t))
                }
                // S5 (measured): `(instance? java.util.Map$Entry (R. :a
                // 1))` => true for `(defrecord R [k v]
                // java.util.Map$Entry ...)`; `(instance? IBar t)` => false
                // for a `deftype T` that implements only `IFoo`.
                ClassVal::Interface { name } => implements_interface(x, name),
            },
            (other, _) => {
                return Err(RjError::type_err(format!(
                    "instance?: expected a class, got {}",
                    other.type_name()
                )))
            }
        };
        Ok(Value::Bool(res))
    });

    // `isa?`: 2-arity (measured: `(isa? Long Number)` true, and -- S4 --
    // consults the GLOBAL derivation hierarchy plus vector-pairwise
    // comparison) and 3-arity (S4: `(isa? h child parent)` against an
    // explicit hierarchy value, e.g. one built with `make-hierarchy`/
    // `derive`). ONE implementation, `crate::multi::isa_values` -- see
    // that module's doc; extended in place here rather than duplicated,
    // per this module's own doc header.
    reg(i, "isa?", ArityHint::Range(2, 3), |interp, args| {
        let (h, child, parent) = if args.len() == 3 {
            let Value::Map(h) = &args[0] else {
                return Err(RjError::type_err(format!(
                    "isa?: expected a hierarchy map, got {}",
                    args[0].type_name()
                )));
            };
            (h.clone(), &args[1], &args[2])
        } else {
            (
                crate::multi::global_hierarchy_value(interp),
                &args[0],
                &args[1],
            )
        };
        Ok(Value::Bool(crate::multi::isa_values(interp, &h, child, parent)?))
    });

    // `(satisfies? P x)` -- measured: true on exact impl OR (non-nil
    // values only) an Object impl; false for nil unless nil itself is
    // extended.
    reg(i, "satisfies?", ArityHint::Exact(2), |interp, args| {
        let key = proto_key(&args[0], "satisfies?")?;
        // W3d2: an anonymous `reify` type implements its protocols
        // DIRECTLY -- its impls live on the type (`TypeDef::methods`),
        // never in the extend registry this fn reads, so a `reify` used to
        // answer `false` here where real Clojure answers `true` (that was
        // the last row of `tests/conformance/pending/records.corpus`).
        // Real Clojure's own answer is `(instance? (:on-interface P) x)`,
        // i.e. "does this class implement the protocol's interface" --
        // which is exactly what `TypeDef::protocols` records. Costs one
        // `Value::Inst` discriminant test, and the `Vec` it scans is empty
        // for every value in the runtime that is not a `reify` of a
        // protocol.
        //
        // SPEC-PORT: `.unmeta()` for the same reason `lookup_method`
        // peels it -- `(with-meta (reify P ..) {..})` is still a `P` on
        // the JVM, and `clojure.spec.alpha`'s `spec?` is exactly this
        // question asked about a spec that `with-name` has named.
        let target = args[1].unmeta();
        if let Value::Inst(inst) = target {
            if inst.tdef.protocols.contains(&key) {
                return Ok(Value::Bool(true));
            }
        }
        let reg_guard = crate::sync::lock_read(&interp.protocols.0);
        let Some(proto) = reg_guard.get(&key) else {
            return Ok(Value::Bool(false));
        };
        let ck = class_key(target);
        let hit = proto.impls.contains_key(&ck)
            || (!matches!(target, Value::Nil) && proto.impls.contains_key(&ClassKey::Object));
        Ok(Value::Bool(hit))
    });

    // `(extends? P Class)` -- EXACT registration only (measured: an
    // Object impl does not make `(extends? P Long)` true; `extenders`
    // lists exactly the registered keys).
    reg(i, "extends?", ArityHint::Exact(2), |interp, args| {
        let key = proto_key(&args[0], "extends?")?;
        let ck = key_for_class_value(&args[1], "extends?")?;
        let reg_guard = crate::sync::lock_read(&interp.protocols.0);
        Ok(Value::Bool(
            reg_guard.get(&key).is_some_and(|p| p.impls.contains_key(&ck)),
        ))
    });

    reg(i, "extenders", ArityHint::Exact(1), |interp, args| {
        let key = proto_key(&args[0], "extenders")?;
        let reg_guard = crate::sync::lock_read(&interp.protocols.0);
        let Some(proto) = reg_guard.get(&key) else {
            return Ok(Value::Nil);
        };
        let items: crate::value::PVec =
            proto.impls.values().map(|(cls, _)| cls.clone()).collect();
        if items.is_empty() {
            Ok(Value::Nil)
        } else {
            Ok(Value::List(items))
        }
    });

    // `(extend Long P {:m (fn [x] ...)} Q {...} ...)` -- the fn-value
    // plane `extend-type`/`extend-protocol` macroexpand onto in real
    // Clojure; measured working with plain fn maps.
    reg(i, "extend", ArityHint::Min(1), |interp, args| {
        if args.len() < 3 || args.len() % 2 == 0 {
            return Err(RjError::arity(format!(
                "extend: expected a class followed by protocol/method-map pairs, got {} args",
                args.len()
            )));
        }
        for pair in args[1..].chunks(2) {
            let methods = match &pair[1] {
                Value::Map(m) => {
                    let mut table = HashMap::new();
                    for (k, f) in m.iter() {
                        let name = match k {
                            Value::Keyword(s) => s.text(),
                            other => {
                                return Err(RjError::type_err(format!(
                                    "extend: method key must be a keyword, got {}",
                                    other.type_name()
                                )))
                            }
                        };
                        table.insert(name, f.clone());
                    }
                    table
                }
                other => {
                    return Err(RjError::type_err(format!(
                        "extend: expected a method map, got {}",
                        other.type_name()
                    )))
                }
            };
            interp.register_protocol_impls(&pair[0], &args[0], methods)?;
        }
        Ok(Value::Nil)
    });
}

// ---- heap-image gate-1 accessors (src/image.rs) ----
pub(crate) fn img_builtin_class(name: &str) -> Option<Value> {
    class_by_name().get(name).cloned().or_else(|| exception_class_by_name(name))
}
pub(crate) fn img_iface_methods() -> Vec<(Str, Vec<(Str, usize)>)> {
    let g = crate::sync::lock_mutex(protocol_iface_methods());
    let mut v: Vec<_> = g.iter().map(|(k, x)| (k.clone(), x.clone())).collect();
    v.sort_by(|a, b| (&*a.0).cmp(&*b.0));
    v
}
pub(crate) fn img_inline_marks() -> Vec<(usize, ClassKey)> {
    crate::sync::lock_mutex(inline_protocol_marks()).iter().cloned().collect()
}
pub(crate) fn img_set_inline_marks(v: Vec<(usize, ClassKey)>) {
    *crate::sync::lock_mutex(inline_protocol_marks()) = v.into_iter().collect();
}
