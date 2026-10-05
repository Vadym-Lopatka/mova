//! S4 (compat/s4-sorted-vecof): `sorted-map sorted-map-by sorted-set
//! sorted-set-by sorted? subseq rsubseq rseq compare vector-of` -- sorted
//! collections and typed (`vector-of`) vectors.
//!
//! ## Representation
//!
//! `Value::SortedMap`/`Value::SortedSet` (see `value::SortedMapVal`/
//! `SortedSetVal`'s docs) are a comparator-sorted `Vec` kept sorted
//! incrementally by binary search on every insert/remove -- "conformance
//! first, perf later" per the spec brief: O(n) `assoc`/`conj` (clone the
//! `Vec`, binary-search, insert) rather than a real persistent balanced
//! tree. `Value::TypedVec` (see `TypedVecVal`'s doc) stores its elements
//! already coerced to its `kind`'s `Value` representation, so every op that
//! doesn't need to know the kind (print, `=`, hash, `seq`, `reduce`, ...)
//! is a free ride on `PVec`'s own machinery.
//!
//! ## The two comparators, measured
//!
//! `sorted-map`/`sorted-set` (no `-by`) use `Comparator::Default`
//! (`natural_compare` below), which -- measured against 1.13.0-alpha6 --
//! eagerly rejects a key that isn't nil/a number/a string/keyword/symbol/
//! char/bool/vector on EVERY insert, even the very first into an
//! otherwise-empty collection (`(sorted-set {})` throws `ClassCastException`
//! despite never comparing two keys). `sorted-map-by`/`sorted-set-by` use
//! `Comparator::Fn`, which only ever throws when the comparator fn is
//! actually invoked to compare two keys -- a single-element `(sorted-set-by
//! compare #{})` succeeds. `cmp_via`'s `Comparator::Default` arm therefore
//! validates BOTH operands (via `sm_search`/`ss_search`, the one choke
//! point every mutating op goes through) before comparing; the `Fn` arm
//! never validates anything up front.
//!
//! ## Equality/hash need no comparator at all
//!
//! Both sorted types' `PartialEq`/`Hash` (in `value.rs`) compare/hash their
//! `entries` as unordered content, exactly like a plain `Map`/`Set` --
//! `cmp` is irrelevant to `=` (measured: two sorted maps built with
//! opposite comparators but the same content are `=`). That's what lets
//! those impls live in `value.rs` without an `&mut Interp` to call a custom
//! comparator fn with -- only INSERT/LOOKUP (which must find a comparator-
//! equal existing key) ever needs one, and every such op is a builtin
//! registration, which already threads `&mut Interp` through.

use std::cmp::Ordering;
use std::sync::Arc;

use crate::builtins::{reg, reg_unmeta, ArityHint};
use crate::error::{JvmClass, RjError};
use crate::eval::Interp;
use crate::value::{Comparator, PVec, SortedMapVal, SortedSetVal, TypedVecVal, Value, VecOfKind};

// ============================== comparator core ==============================

/// Whether `v` is eligible for `Comparator::Default` AT ALL (independent of
/// what it's being compared against) -- the eager per-insert check
/// `sm_search`/`ss_search` run before ever touching the tree. Numbers,
/// `nil`, strings, keywords, symbols, chars, bools, and vectors (both plain
/// and `vector-of`) are real Clojure `Comparable`s (or `nil`, handled
/// specially); everything else (lists, maps, sets, fns, ...) is not.
fn is_naturally_comparable(v: &Value) -> bool {
    // S5/M3: metadata never changes whether a value is `Comparable` --
    // `(sorted-map (with-meta 'k {:m 1}) 1)` is `{k 1}`, measured. Unwrap
    // first, matching `natural_compare`'s own unwrap: if this gate and
    // the comparator disagreed about what a value IS, a key could pass
    // the gate and then fail to order (or vice versa).
    let v = v.unmeta();
    matches!(
        v,
        Value::Nil
            | Value::Int(_)
            | Value::Float(_)
            | Value::BigInt(_)
            | Value::BigInteger(_)
            | Value::Ratio(_)
            | Value::BigDec(_)
            | Value::Str(_)
            | Value::Keyword(_)
            | Value::Sym(_)
            | Value::Char(_)
            | Value::Bool(_)
            | Value::Vector(_)
            // S7: measured `(compare (first {:a 1}) [:a 1])` => `0` --
            // an entry is `Comparable` exactly as the vector it is.
            | Value::MapEntry(_)
            | Value::TypedVec(_)
    )
}

fn cce_type(v: &Value) -> RjError {
    RjError::type_err(format!(
        "Default comparator requires nil, Number, or Comparable: {}",
        crate::printer::pr_str(v)
    ))
}

fn is_numeric(v: &Value) -> bool {
    matches!(
        v,
        Value::Int(_)
            | Value::Float(_)
            | Value::BigInt(_)
            // S5: `java.math.BigInteger` is `Comparable` and a `Number`.
            | Value::BigInteger(_)
            | Value::Ratio(_)
            | Value::BigDec(_)
    )
}

#[allow(dead_code)] // kept beside `is_numeric` as the lossy-widening
// counterpart; S5 moved `natural_compare` onto the exact tower ordering,
// so nothing calls it today.
fn numeric_f64(v: &Value) -> f64 {
    match v {
        Value::Int(n) => *n as f64,
        Value::Float(f) => *f,
        Value::BigInt(b) | Value::BigInteger(b) => b.to_f64(),
        Value::Ratio(r) => r.to_f64(),
        Value::BigDec(d) => d.to_f64(),
        _ => f64::NAN,
    }
}

fn vec_items(v: &Value) -> Option<&PVec> {
    match v {
        // S7: an entry compares elementwise like the vector it is, in
        // both directions (`(compare (first {:a 1}) [:a 1])` => `0`,
        // `(compare (first {:a 1}) (first {:b 2}))` => `-1`, measured).
        Value::Vector(items) | Value::MapEntry(items) => Some(items),
        Value::TypedVec(tv) => Some(&tv.data),
        _ => None,
    }
}

/// Clojure's "natural order" (`clojure.lang.Util.compare`-shaped): `nil` is
/// less than everything non-`nil` (equal to another `nil`); numbers compare
/// cross-type by value; same-shape `Str`/`Keyword`/`Sym`/`Char`/`Bool`
/// compare directly; vectors (plain or typed) compare elementwise then by
/// length; anything else -- or a cross-category pair, e.g. `Int` vs `Str`
/// -- throws (measured `(sorted-set 1 "a")`/`(compare 1 "a")` both throw
/// `ClassCastException`). This is `clojure.core/compare`'s own algorithm
/// too (measured identical on every mixed-type pair tried), so `compare`
/// below is a thin wrapper rather than a second implementation.
pub(crate) fn natural_compare(a: &Value, b: &Value) -> Result<Ordering, RjError> {
    use Value::*;
    // S5/M3: `compare` ignores metadata, exactly as `=` and `hash` do
    // (see `PartialEq for Value`) -- and it MUST, or a sorted collection
    // could order a value differently from an `=`-equal copy of it, which
    // would break `sorted-map`/`sorted-set` invariants outright. Unwrap
    // once at the top rather than adding cross-arms, same shape as `=`.
    if a.has_meta() || b.has_meta() {
        return natural_compare(a.unmeta(), b.unmeta());
    }
    match (a, b) {
        (Nil, Nil) => Ok(Ordering::Equal),
        (Nil, _) => Ok(Ordering::Less),
        (_, Nil) => Ok(Ordering::Greater),
        (Int(x), Int(y)) => Ok(x.cmp(y)),
        // S5: every other numeric pair goes through the tower's own
        // contagion ladder (`builtins::numbers::tower_ordering`) rather
        // than a blanket `f64` widening, so `BigInt`-vs-`BigInt`,
        // `Ratio`-vs-`Ratio` and `BigDec`-vs-`BigDec` compare EXACTLY
        // while an `Int`-vs-`Double` pair still widens (which is what
        // Clojure's own `Numbers.lt` does -- measured `(< 1N 1.5)` and
        // `(compare 1M 1.0)` => `0`). `None` (a NaN was involved) is
        // `Equal` here because `compare` is defined as
        // `if (lt x y) -1 else if (lt y x) 1 else 0`, and both `lt`s are
        // false against a NaN.
        _ if is_numeric(a) && is_numeric(b) => Ok(crate::builtins::numbers::tower_ordering(
            a, b, "compare",
        )?
        .unwrap_or(Ordering::Equal)),
        (Str(x), Str(y)) => Ok(x.cmp(y)),
        // W-GEO stage 1 (design doc §4's one "must NOT take the fast
        // path" audit item): CONTENT order, never interned-id order.
        // Sorted order is a content property (lexicographic by name,
        // matching real Clojure), but `Keyword`'s ids are handed out in
        // CONSTRUCTION order -- so comparing ids would silently scramble
        // `sorted-map`/`sorted-set` iteration the moment two keywords'
        // construction order disagreed with their text order, which is the
        // common case. `Keyword`'s own `Ord` is content-based for exactly
        // this reason; `as_ref()` here reaches `&str` through the same
        // `text_ref` chokepoint. Regression-tested below
        // (`sorted_map_keyword_order_is_content_not_construction_order`).
        (Keyword(x), Keyword(y)) => Ok(x.as_ref().cmp(y.as_ref())),
        (Sym(x), Sym(y)) => Ok((x.ns.as_deref(), x.name.as_ref()).cmp(&(y.ns.as_deref(), y.name.as_ref()))),
        (Char(x), Char(y)) => Ok(x.cmp(y)),
        (Bool(x), Bool(y)) => Ok(x.cmp(y)),
        _ if vec_items(a).is_some() && vec_items(b).is_some() => {
            let xa = vec_items(a).expect("checked above");
            let xb = vec_items(b).expect("checked above");
            for idx in 0..xa.len().min(xb.len()) {
                let o = natural_compare(
                    xa.get(idx).expect("in bounds"),
                    xb.get(idx).expect("in bounds"),
                )?;
                if o != Ordering::Equal {
                    return Ok(o);
                }
            }
            Ok(xa.len().cmp(&xb.len()))
        }
        // C10: a same-shape pair of EMPTY `List`/`Map`/`Set` compares
        // `Equal` even though none of the three is `Comparable` -- not a
        // structural-equality shortcut (that's exactly what got REMOVED
        // from the `compare` wrapper above), but a narrower, measured
        // fact about the real JVM: `clojure.lang.Util.compare`'s FIRST
        // check is reference identity (`k1 == k2`), and Clojure
        // canonicalizes the empty instance of each persistent collection
        // type as a single static singleton (`PersistentList/EMPTY`,
        // `PersistentArrayMap/EMPTY`, `PersistentHashSet/EMPTY`) -- so
        // `(list)` and `(list)` (or `{}`/`{}`, `#{}`/`#{}`) really are
        // `identical?` on the oracle (measured `true` all three ways),
        // and `compare` returns `0` on that identity hit alone, before
        // ever reaching the `instanceof Comparable` cast that would
        // otherwise throw. A NON-empty `List`/`Map`/`Set`, or a pair of
        // DIFFERENT types (even both empty -- `(compare (list) [])`
        // throws, measured), never hits this: two independently-built
        // non-empty instances are never the same JVM object, so the real
        // `compare` falls through to the throwing cast exactly as this
        // fallback still does for them.
        (List(a), List(b)) if a.is_empty() && b.is_empty() => Ok(Ordering::Equal),
        (Map(a), Map(b)) if a.is_empty() && b.is_empty() => Ok(Ordering::Equal),
        (Set(a), Set(b)) if a.is_empty() && b.is_empty() => Ok(Ordering::Equal),
        _ => {
            if !is_naturally_comparable(a) {
                Err(cce_type(a))
            } else if !is_naturally_comparable(b) {
                Err(cce_type(b))
            } else {
                Err(RjError::type_err(format!(
                    "compare: cannot compare {} and {}",
                    a.type_name(),
                    b.type_name()
                )))
            }
        }
    }
}

/// The `hash` builtin's whole body, extracted (C10) so `eval::
/// types_forms::eval_dot_form`'s generic `.hashCode` fallback can call the
/// EXACT same computation instead of re-deriving it -- see that arm's own
/// doc for why reusing `hash` (rather than a bit-exact port of Java's
/// `List.hashCode`/`Set.hashCode`/`Map.hashCode`) is enough for
/// `data_structures.clj`'s `is-same-collection` helper, which only ever
/// compares `.hashCode` RELATIVELY.
///
/// C10: `realize_deep`s `v` FIRST -- `Value`'s own `Hash` impl (what this
/// bottoms out to) is a pure trait fn with no `&mut Interp`, so it can
/// never force a `Lazy` tail buried inside an otherwise-realized `List`
/// (the internal "cons cell" continuation-marker shape, see `builtins::
/// collections`'s module doc) -- it can only fall back to that unforced
/// tail's IDENTITY, which measurably breaks `a == b => hash(a) ==
/// hash(b)` for exactly the shape `ordered-collection-equality-test`'s
/// `colls1` constructs: `(lazy-seq (cons -3 (lazy-seq (cons :a (lazy-seq
/// (cons "7th" nil))))))`, whose OUTER cell realizes to a 2-element `[-3,
/// Lazy]` "improper list" long before its nested tail is ever forced.
/// `realize_deep` (already used by `pr-str`/`str` for the identical
/// reason) walks and flattens the WHOLE chain first, so what actually
/// gets hashed is the same concrete `[-3 :a "7th"]` `str`/`=` already see
/// -- measured, this is what makes `(= (hash a) (hash b))` hold for that
/// pair against an equal-content vector/list/queue.
pub(crate) fn hash_value(interp: &mut Interp, v: &Value) -> Result<i64, RjError> {
    if let Some(h) = crate::builtins::numbers::numeric_hasheq(v) {
        return Ok(h as i64);
    }
    let realized = interp.realize_deep(v)?;
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    realized.hash(&mut h);
    Ok(h.finish() as i64)
}

/// The exact two-call bool/number protocol real Clojure's
/// `clojure.lang.AFunction.compare` uses for a `sorted-*-by`/`sort-by`
/// comparator fn: a `Boolean` result means "is `a` less than `b`" (called a
/// SECOND time with swapped args to distinguish `Equal` from `Greater` when
/// the first call is `false`); a numeric result is a Java-`Comparator`-
/// style sign. Widens `builtins::seq`'s `compare_via` (which only ever
/// needs `Less`-or-not for an insertion sort) to a real 3-way `Ordering`,
/// since a sorted collection needs `Equal` to detect "this key is already
/// present" the way `sort` never does.
fn fn_compare(interp: &mut Interp, f: &Value, a: &Value, b: &Value) -> Result<Ordering, RjError> {
    match interp.call(f, &[a.clone(), b.clone()])? {
        Value::Bool(true) => Ok(Ordering::Less),
        Value::Bool(false) => match interp.call(f, &[b.clone(), a.clone()])? {
            Value::Bool(true) => Ok(Ordering::Greater),
            _ => Ok(Ordering::Equal),
        },
        Value::Int(n) => Ok(n.cmp(&0)),
        Value::Float(n) => Ok(n.partial_cmp(&0.0).unwrap_or(Ordering::Equal)),
        _ => Ok(Ordering::Equal),
    }
}

pub(crate) fn cmp_via(interp: &mut Interp, cmp: &Comparator, a: &Value, b: &Value) -> Result<Ordering, RjError> {
    match cmp {
        Comparator::Default => natural_compare(a, b),
        Comparator::Fn(f) => fn_compare(interp, f, a, b),
    }
}

// ============================== search/insert helpers ==============================

/// Binary search `entries` (assumed already sorted by `cmp`) for `k`.
/// `Ok(idx)` = found at `idx`; `Err(idx)` = not found, `idx` is the sorted
/// insertion point. The ONE choke point that validates a `Comparator::
/// Default` key eagerly (see this module's doc) -- every mutating AND
/// read-only sorted-map op goes through this (or `ss_search`'s set twin).
fn sm_search(
    interp: &mut Interp,
    cmp: &Comparator,
    entries: &[(Value, Value)],
    k: &Value,
) -> Result<Result<usize, usize>, RjError> {
    if matches!(cmp, Comparator::Default) && !is_naturally_comparable(k) {
        return Err(cce_type(k));
    }
    let mut lo = 0usize;
    let mut hi = entries.len();
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        match cmp_via(interp, cmp, &entries[mid].0, k)? {
            Ordering::Equal => return Ok(Ok(mid)),
            Ordering::Less => lo = mid + 1,
            Ordering::Greater => hi = mid,
        }
    }
    Ok(Err(lo))
}

fn ss_search(
    interp: &mut Interp,
    cmp: &Comparator,
    entries: &[Value],
    x: &Value,
) -> Result<Result<usize, usize>, RjError> {
    if matches!(cmp, Comparator::Default) && !is_naturally_comparable(x) {
        return Err(cce_type(x));
    }
    let mut lo = 0usize;
    let mut hi = entries.len();
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        match cmp_via(interp, cmp, &entries[mid], x)? {
            Ordering::Equal => return Ok(Ok(mid)),
            Ordering::Less => lo = mid + 1,
            Ordering::Greater => hi = mid,
        }
    }
    Ok(Err(lo))
}

/// Insert-or-update-value into a growing sorted-map `Vec` (shared by
/// `sorted-map`/`sorted-map-by`'s construction loop and `assoc`/`conj`).
/// Measured (Java `TreeMap` semantics): a comparator-equal-but-not-`=`
/// existing key is KEPT -- only its value is replaced (`(assoc (sorted-map
/// 1 :a) 1.0 :b)` is `{1 :b}`, not `{1.0 :b}`).
fn insert_sorted_entry(
    interp: &mut Interp,
    cmp: &Comparator,
    entries: &mut Vec<(Value, Value)>,
    k: Value,
    v: Value,
) -> Result<(), RjError> {
    match sm_search(interp, cmp, entries, &k)? {
        Ok(idx) => entries[idx].1 = v,
        Err(ins) => entries.insert(ins, (k, v)),
    }
    Ok(())
}

/// Insert into a growing sorted-set `Vec`; a comparator-equal existing
/// element is left untouched (dup ignored, first-inserted wins).
fn insert_sorted_elem(
    interp: &mut Interp,
    cmp: &Comparator,
    entries: &mut Vec<Value>,
    x: Value,
) -> Result<(), RjError> {
    if let Err(ins) = ss_search(interp, cmp, entries, &x)? {
        entries.insert(ins, x);
    }
    Ok(())
}

// ============================== public mutation API ==============================
// Called from `builtins::collections`'s existing `get`/`assoc`/`conj`/
// `dissoc`/`disj`/`into`/... choke points once they see a `SortedMap`/
// `SortedSet` receiver -- see that module's added match arms.

pub(crate) fn sorted_map_assoc(interp: &mut Interp, m: &SortedMapVal, k: &Value, v: &Value) -> Result<Value, RjError> {
    let mut entries = m.entries.clone();
    insert_sorted_entry(interp, &m.cmp, &mut entries, k.clone(), v.clone())?;
    Ok(Value::SortedMap(Arc::new(SortedMapVal { cmp: m.cmp.clone(), entries })))
}

pub(crate) fn sorted_map_dissoc(interp: &mut Interp, m: &SortedMapVal, k: &Value) -> Result<Value, RjError> {
    let mut entries = m.entries.clone();
    if let Ok(idx) = sm_search(interp, &m.cmp, &entries, k)? {
        entries.remove(idx);
    }
    Ok(Value::SortedMap(Arc::new(SortedMapVal { cmp: m.cmp.clone(), entries })))
}

pub(crate) fn sorted_map_get(interp: &mut Interp, m: &SortedMapVal, k: &Value) -> Result<Option<Value>, RjError> {
    Ok(match sm_search(interp, &m.cmp, &m.entries, k) {
        Ok(Ok(idx)) => Some(m.entries[idx].1.clone()),
        _ => None,
    })
}

pub(crate) fn sorted_map_contains(interp: &mut Interp, m: &SortedMapVal, k: &Value) -> bool {
    matches!(sm_search(interp, &m.cmp, &m.entries, k), Ok(Ok(_)))
}

/// `conj` onto a sorted map: a `[k v]` pair, another map (plain or sorted),
/// same policy `builtins::collections::conj_one`'s `Map` arm uses.
pub(crate) fn sorted_map_conj(interp: &mut Interp, m: &SortedMapVal, item: &Value) -> Result<Value, RjError> {
    let mut entries = m.entries.clone();
    match item {
        // clojure-lsp campaign (mova/PLAN.md): `(conj m nil)` is a
        // measured no-op on every real Clojure map type, sorted included
        // -- same treatment `builtins::collections::conj_one`'s `Map` arm
        // gives it.
        Value::Nil => {}
        // S7: an entry conj'd onto a sorted map behaves exactly like
        // the `[k v]` 2-vector it is.
        Value::Vector(pair) | Value::MapEntry(pair) if pair.len() == 2 => {
            insert_sorted_entry(interp, &m.cmp, &mut entries, pair[0].clone(), pair[1].clone())?
        }
        Value::Map(other) => {
            for (k, v) in other.iter() {
                insert_sorted_entry(interp, &m.cmp, &mut entries, k.clone(), v.clone())?;
            }
        }
        Value::SortedMap(other) => {
            for (k, v) in other.entries.iter() {
                insert_sorted_entry(interp, &m.cmp, &mut entries, k.clone(), v.clone())?;
            }
        }
        other => {
            return Err(RjError::type_err(format!(
                "conj: map conj arg must be a [k v] pair or a map, got {}",
                other.type_name()
            )))
        }
    }
    Ok(Value::SortedMap(Arc::new(SortedMapVal { cmp: m.cmp.clone(), entries })))
}

pub(crate) fn sorted_set_conj(interp: &mut Interp, s: &SortedSetVal, item: &Value) -> Result<Value, RjError> {
    let mut entries = s.entries.clone();
    insert_sorted_elem(interp, &s.cmp, &mut entries, item.clone())?;
    Ok(Value::SortedSet(Arc::new(SortedSetVal { cmp: s.cmp.clone(), entries })))
}

pub(crate) fn sorted_set_disj(interp: &mut Interp, s: &SortedSetVal, x: &Value) -> Result<Value, RjError> {
    let mut entries = s.entries.clone();
    if let Ok(idx) = ss_search(interp, &s.cmp, &entries, x)? {
        entries.remove(idx);
    }
    Ok(Value::SortedSet(Arc::new(SortedSetVal { cmp: s.cmp.clone(), entries })))
}

pub(crate) fn sorted_set_contains(interp: &mut Interp, s: &SortedSetVal, x: &Value) -> bool {
    matches!(ss_search(interp, &s.cmp, &s.entries, x), Ok(Ok(_)))
}

pub(crate) fn sorted_set_get(interp: &mut Interp, s: &SortedSetVal, x: &Value) -> Option<Value> {
    match ss_search(interp, &s.cmp, &s.entries, x) {
        Ok(Ok(idx)) => Some(s.entries[idx].clone()),
        _ => None,
    }
}

// ============================== vector-of coercion ==============================

fn parse_vec_kind(v: &Value) -> Result<VecOfKind, RjError> {
    // W3a: measured -- real `clojure.core/vector-of` dispatches on the
    // type keyword through a `case` whose fallthrough is `(throw
    // (IllegalArgumentException. (str "Unrecognized type " t)))`:
    // `(vector-of nil)`, `(vector-of 'int)`, `(vector-of :integer)` and
    // `(vector-of "")` all raise `java.lang.IllegalArgumentException`, not
    // the `ClassCastException` `ErrorKind::TypeErr` otherwise maps to.
    // mova keeps its own (more specific) wording; only the class matches.
    let bad = || {
        RjError::type_err(format!(
            "vector-of: invalid type keyword: {}",
            crate::printer::pr_str(v)
        ))
        .with_class(JvmClass::IllegalArgument)
    };
    match v {
        Value::Keyword(k) => match k.as_ref() {
            "boolean" => Ok(VecOfKind::Boolean),
            "byte" => Ok(VecOfKind::Byte),
            "short" => Ok(VecOfKind::Short),
            "int" => Ok(VecOfKind::Int),
            "long" => Ok(VecOfKind::Long),
            "float" => Ok(VecOfKind::Float),
            "double" => Ok(VecOfKind::Double),
            "char" => Ok(VecOfKind::Char),
            _ => Err(bad()),
        },
        _ => Err(bad()),
    }
}

/// W3a: the name was already `npe` -- the CLASS now says so too. Measured:
/// `(vector-of :int nil)` => `java.lang.NullPointerException: Cannot invoke
/// "java.lang.Character.charValue()" because "x" is null` (the primitive
/// unboxing of a null element), for every arity vectors.clj exercises.
fn npe(kind: &str) -> RjError {
    RjError::type_err(format!("vector-of :{kind}: null is not a valid element"))
        .with_class(JvmClass::NullPointer)
}

/// The sibling of [`npe`]: a non-null element of the wrong type really is a
/// `ClassCastException` on the JVM (`ErrorKind::TypeErr`'s default), stated
/// explicitly here so the two live side by side.
fn cce_kind(kind: &str, v: &Value) -> RjError {
    RjError::type_err(format!(
        "vector-of :{kind}: cannot coerce {} ({})",
        v.type_name(),
        crate::printer::pr_str(v)
    ))
}

/// Truncating-narrowing integral coercion shared with `byte`/`short`/`int`/
/// `long` casts (measured: `(vector-of :int 1M 2.0 3.1)` is `[1 2 3]`,
/// truncated exactly like `(int 3.1)` would be) -- reuses
/// `builtins::numbers::cast_integral` rather than re-deriving the
/// accept-list/range table.
fn coerce_integral(v: &Value, kind: &str, min: i64, max: i64) -> Result<Value, RjError> {
    // `cast_integral` itself already rejects every non-numeric/non-Char
    // `Value` (falls to its own type-error arm) -- `Nil` is special-cased
    // here only for a more accurate message (measured: real Clojure throws
    // `NullPointerException` for a `nil` element, not `ClassCastException`,
    // though neither is distinguishable to mova's class-blind `catch`).
    if matches!(v, Value::Nil) {
        return Err(npe(kind));
    }
    crate::builtins::numbers::cast_integral(v, kind, min, max).map(Value::Int)
}

fn coerce_float(v: &Value, kind: &str) -> Result<Value, RjError> {
    match v {
        Value::Nil => Err(npe(kind)),
        Value::Int(n) => Ok(Value::Float(*n as f64)),
        Value::Float(f) => Ok(Value::Float(*f)),
        Value::Char(c) => Ok(Value::Float(*c as u32 as f64)),
        Value::BigInt(b) => Ok(Value::Float(b.to_f64())),
        Value::Ratio(r) => Ok(Value::Float(r.to_f64())),
        Value::BigDec(d) => Ok(Value::Float(d.to_f64())),
        _ => Err(cce_kind(kind, v)),
    }
}

/// Java `char` is a 16-bit UTF-16 code unit: an in-range `Int` coerces to
/// its code point (measured `(vector-of :char 65)` -> `[\A]`), out of
/// `0..=0xFFFF` is a range error mirroring `cast_integral`'s shape.
fn coerce_char(v: &Value) -> Result<Value, RjError> {
    match v {
        Value::Nil => Err(npe("char")),
        Value::Char(c) => Ok(Value::Char(*c)),
        Value::Int(n) => {
            if (0..=0xFFFF).contains(n) {
                Ok(Value::Char(char::from_u32(*n as u32).unwrap_or('\u{FFFD}')))
            } else {
                Err(RjError::type_err(format!("vector-of :char: value out of range: {n}")))
            }
        }
        _ => Err(cce_kind("char", v)),
    }
}

fn coerce_bool(v: &Value) -> Result<Value, RjError> {
    match v {
        Value::Nil => Err(npe("boolean")),
        Value::Bool(b) => Ok(Value::Bool(*b)),
        _ => Err(cce_kind("boolean", v)),
    }
}

pub(crate) fn coerce_for_kind(kind: VecOfKind, v: &Value) -> Result<Value, RjError> {
    match kind {
        VecOfKind::Boolean => coerce_bool(v),
        VecOfKind::Byte => coerce_integral(v, "byte", -128, 127),
        VecOfKind::Short => coerce_integral(v, "short", -32768, 32767),
        VecOfKind::Int => coerce_integral(v, "int", i32::MIN as i64, i32::MAX as i64),
        VecOfKind::Long => coerce_integral(v, "long", i64::MIN, i64::MAX),
        VecOfKind::Float => coerce_float(v, "float"),
        VecOfKind::Double => coerce_float(v, "double"),
        VecOfKind::Char => coerce_char(v),
    }
}

pub(crate) fn typed_vec_conj(tv: &TypedVecVal, item: &Value) -> Result<Value, RjError> {
    let coerced = coerce_for_kind(tv.kind, item)?;
    let mut data = tv.data.clone();
    data.push_back(coerced);
    Ok(Value::TypedVec(Arc::new(TypedVecVal { kind: tv.kind, data })))
}

pub(crate) fn typed_vec_assoc(tv: &TypedVecVal, idx: usize, item: &Value) -> Result<Value, RjError> {
    let coerced = coerce_for_kind(tv.kind, item)?;
    let mut data = tv.data.clone();
    if idx < data.len() {
        data.set(idx, coerced);
    } else if idx == data.len() {
        data.push_back(coerced);
    } else {
        return Err(RjError::other(format!(
            "assoc: index {idx} out of bounds for vector of length {}",
            tv.data.len()
        )));
    }
    Ok(Value::TypedVec(Arc::new(TypedVecVal { kind: tv.kind, data })))
}

// ============================== subseq/rsubseq ==============================

/// `(comparator, (sort-key, emitted-item) pairs in ascending stored order)`
/// -- a map emits `[k v]` pairs, a set emits the element itself (twice:
/// once to test, once to emit).
fn sorted_key_and_item(coll: &Value) -> Result<(Comparator, Vec<(Value, Value)>), RjError> {
    match coll {
        Value::SortedMap(m) => Ok((
            m.cmp.clone(),
            m.entries
                .iter()
                .map(|(k, v)| (k.clone(), Value::MapEntry(PVec::pair(k.clone(), v.clone()))))
                .collect(),
        )),
        Value::SortedSet(s) => Ok((s.cmp.clone(), s.entries.iter().map(|v| (v.clone(), v.clone())).collect())),
        other => Err(RjError::type_err(format!(
            "subseq: not a sorted collection: {}",
            other.type_name()
        ))),
    }
}

/// The 4 canonical bound tests `subseq`/`rsubseq` accept. MEASURED: real
/// Clojure's `subseq` does NOT call `test` as a function on the two keys at
/// all (which is why `(subseq (sorted-map :a 1 :b 2) > :a)` works even
/// though `(> :a :a)` itself would throw -- keywords aren't `Number`s) --
/// it dispatches on `test`'s IDENTITY against `clojure.core</>/<=/>=` and
/// then walks the tree with its OWN comparator. Reproduced here the same
/// way: recognize the 4 ops by the native's registered name, then answer
/// every bound check through `cmp_via` (this collection's actual
/// comparator, default or `-by`) instead of ever invoking `test` itself.
#[derive(Clone, Copy)]
enum BoundOp {
    Lt,
    Le,
    Gt,
    Ge,
}

fn bound_op(v: &Value) -> Option<BoundOp> {
    let Value::Native(n) = v else { return None };
    match n.name.as_ref() {
        "<" => Some(BoundOp::Lt),
        "<=" => Some(BoundOp::Le),
        ">" => Some(BoundOp::Gt),
        ">=" => Some(BoundOp::Ge),
        _ => None,
    }
}

fn bound_matches(op: BoundOp, ord: Ordering) -> bool {
    match op {
        BoundOp::Lt => ord == Ordering::Less,
        BoundOp::Le => ord != Ordering::Greater,
        BoundOp::Gt => ord == Ordering::Greater,
        BoundOp::Ge => ord != Ordering::Less,
    }
}

fn require_bound_op(v: &Value) -> Result<BoundOp, RjError> {
    bound_op(v).ok_or_else(|| {
        RjError::type_err(format!(
            "subseq: test must be one of </<=/>/>=, got {}",
            crate::printer::pr_str(v)
        ))
    })
}

fn subseq_impl(interp: &mut Interp, args: &[Value], reverse: bool) -> Result<Value, RjError> {
    let (cmp, pairs) = sorted_key_and_item(&args[0])?;
    let mut out: Vec<Value> = Vec::new();
    match args.len() {
        3 => {
            let op = require_bound_op(&args[1])?;
            let bound = &args[2];
            for (k, item) in &pairs {
                if bound_matches(op, cmp_via(interp, &cmp, k, bound)?) {
                    out.push(item.clone());
                }
            }
        }
        5 => {
            let op1 = require_bound_op(&args[1])?;
            let b1 = &args[2];
            let op2 = require_bound_op(&args[3])?;
            let b2 = &args[4];
            for (k, item) in &pairs {
                if bound_matches(op1, cmp_via(interp, &cmp, k, b1)?)
                    && bound_matches(op2, cmp_via(interp, &cmp, k, b2)?)
                {
                    out.push(item.clone());
                }
            }
        }
        n => {
            return Err(RjError::arity(format!(
                "subseq: expected 3 or 5 arguments, got {n}"
            )))
        }
    }
    if reverse {
        out.reverse();
    }
    if out.is_empty() {
        Ok(Value::Nil)
    } else {
        Ok(Value::List(out.into_iter().collect()))
    }
}

// ============================== registration ==============================

pub fn register(i: &mut Interp) {
    reg(i, "compare", ArityHint::Exact(2), |_i, args| {
        // C10: NOT a structural-`=` shortcut, measured -- real Clojure's
        // `compare` is `clojure.lang.Util.compare`, which checks Java
        // reference identity (`k1 == k2`), not `.equals`, before falling
        // to `.compareTo`. Two distinct-but-`=`-equal lists/maps/sets
        // still throw `ClassCastException` (`(compare (list 1 2) (list 1
        // 2))`, `(compare {:a 1} {:a 1})`, `(compare #{1} #{1})` all throw
        // on the oracle) because `IPersistentList`/`Map`/`Set` are not
        // `Comparable` at all -- an equal-value shortcut here would mask
        // that and wrongly return `0`. `natural_compare` below already
        // handles `Nil`/numeric/exact-type equality correctly on its own
        // (`is_naturally_comparable` gates the throw), so no shortcut is
        // needed for the types that ARE Comparable either.
        natural_compare(&args[0], &args[1]).map(|o| {
            Value::Int(match o {
                Ordering::Less => -1,
                Ordering::Equal => 0,
                Ordering::Greater => 1,
            })
        })
    });

    // S4: `hash` didn't exist as a builtin anywhere in mova before this
    // (measured: `Unable to resolve symbol: hash`) -- added here because
    // this spec's own probe explicitly needs it (`vectors.clj`'s
    // `test-vec-creation` asserts `(= (hash vec) (hash gvec))` for every
    // `vector-of` example, and the sorted-collection probe measures hash
    // equivalence with a plain map/set the same way). `Value` already
    // derives a full `Hash` impl (`value.rs`) that every collection choke
    // point (this module's `SortedMap`/`SortedSet`/`TypedVec` arms
    // included) is written to agree with `PartialEq` on -- this just
    // exposes it. NOT Java's `hashCode` algorithm (Rust's `DefaultHasher`
    // is a different, but equally deterministic-within-one-process,
    // function) -- conformance corpus lines use it only for RELATIVE
    // equality (`(= (hash a) (hash b))`), never comparing the raw
    // printed number against the JVM oracle's own hash, which would
    // never agree by construction.
    // S5 (SPEC-numtower) narrowed the "not Java's hashCode" caveat above:
    // for NUMBERS `hash` now reproduces `clojure.lang.Util.hasheq`
    // digit-for-digit (`builtins::numbers::numeric_hasheq` -- Murmur3 for
    // the integer category, `Double.hashCode`/`Ratio.hashCode`/
    // scale-stripped `BigDecimal.hashCode` for the rest), because the
    // numtower transcript records `(hash 7)` as the literal `-137604029`
    // rather than merely as "whatever `(hash 7N)` is". Everything else
    // still uses `Value`'s own structural hash and is only ever compared
    // RELATIVELY by the corpus.
    reg(i, "hash", ArityHint::Exact(1), |interp, args| Ok(Value::Int(hash_value(interp, &args[0])?)));

    // S4: `key`/`val`/`find` didn't exist as builtins anywhere in mova
    // either (measured, same as `hash` above) -- added here because this
    // spec's own probe bullet list explicitly calls out "(key/val of
    // entries)" for sorted-map/-set.
    //
    // S7 TIGHTENED both from "any 2-element `Value::Vector`" to
    // "`Value::MapEntry` only". `clojure.core/key` is
    // `(. ^java.util.Map$Entry e getKey)`, so on the oracle
    // `(key [:a 1])` is a ClassCastException, not `:a` -- mova answered
    // `:a` before this branch purely because it had no way to tell the two
    // apart. It does now, so it stops guessing. (`(key (find [10 20] 1))`
    // still works: `find` on a vector returns a real entry -- measured,
    // `(class (find [10 20] 1))` => `clojure.lang.MapEntry`.)
    reg(i, "key", ArityHint::Exact(1), |_i, args| match &args[0] {
        Value::MapEntry(items) => Ok(items[0].clone()),
        other => Err(RjError::type_err(format!("key: not a map entry: {}", other.type_name()))),
    });
    reg(i, "val", ArityHint::Exact(1), |_i, args| match &args[0] {
        Value::MapEntry(items) => Ok(items[1].clone()),
        other => Err(RjError::type_err(format!("val: not a map entry: {}", other.type_name()))),
    });
    reg_unmeta(i, "find", ArityHint::Exact(2), |interp, args| match &args[0] {
        Value::Nil => Ok(Value::Nil),
        Value::Map(m) => Ok(m
            .get(&args[1])
            .map(|v| Value::MapEntry(PVec::pair(args[1].clone(), v.clone())))
            .unwrap_or(Value::Nil)),
        Value::HostStruct(hs) => Ok(match &args[1] {
            Value::Keyword(kw) => crate::host_struct::lookup(hs, kw.text_ref()),
            _ => crate::host_struct::as_pmap(hs).get(&args[1]).cloned(),
        }
        .map(|v| Value::MapEntry(PVec::pair(args[1].clone(), v)))
        .unwrap_or(Value::Nil)),
        Value::LazyMap(hs) => Ok(match &args[1] {
            Value::Keyword(kw) => crate::lazy_map::lookup(hs, kw.text_ref()),
            _ => crate::lazy_map::as_pmap(hs).get(&args[1]).cloned(),
        }
        .map(|v| Value::MapEntry(PVec::pair(args[1].clone(), v)))
        .unwrap_or(Value::Nil)),
        Value::Inst(inst) if inst.tdef.is_record => Ok(inst
            .data
            .get(&args[1])
            .map(|v| Value::MapEntry(PVec::pair(args[1].clone(), v.clone())))
            .unwrap_or(Value::Nil)),
        Value::SortedMap(m) => Ok(sorted_map_get(interp, m, &args[1])?
            .map(|v| Value::MapEntry(PVec::pair(args[1].clone(), v)))
            .unwrap_or(Value::Nil)),
        // D3 (2026-08-21) / W4-EVAL task 2: `find` on a `StructMap` --
        // vendored `evaluation.clj`'s `Metadata` deftest does `(find s
        // 'k)` on a `(struct struct-with-symbols 1)` instance. UNLIKE
        // every arm above, the entry's KEY is the struct's OWN stored key
        // object (`struct_map_entry`), not `args[1]` -- measured, a
        // struct-map basis key can carry its own metadata (`(with-meta
        // 'k {:a "A"})`) that a bare lookup key like `'k` does not, and
        // `find` must hand that metadata back (`(meta (key (find (struct
        // s 1) 'k)))` => `{:a "A"}`), mirroring the oracle's `entryAt`'s
        // `MapEntry.create(e.getKey(), ...)`.
        Value::StructMap(sm) => Ok(crate::builtins::structmap::struct_map_entry(sm, &args[1])
            .map(|(k, v)| Value::MapEntry(PVec::pair(k.clone(), v.clone())))
            .unwrap_or(Value::Nil)),
        // S4 merge (1E + 1G): vectors are associative by index (measured:
        // `(find [10 20] 1)` is `[1 20]`, out-of-range/non-int key is nil);
        // a typed vector behaves identically (clojure.core.Vec is
        // `Associative` on the JVM too).
        Value::Vector(items) | Value::MapEntry(items) => Ok(match &args[1] {
            Value::Int(n) if *n >= 0 && (*n as usize) < items.len() => {
                Value::MapEntry(PVec::pair(args[1].clone(), items[*n as usize].clone()))
            }
            _ => Value::Nil,
        }),
        Value::TypedVec(tv) => Ok(match &args[1] {
            Value::Int(n) if *n >= 0 && (*n as usize) < tv.data.len() => {
                Value::MapEntry(PVec::pair(args[1].clone(), tv.data[*n as usize].clone()))
            }
            _ => Value::Nil,
        }),
        other => Err(RjError::type_err(format!("find: not a map: {}", other.type_name()))),
    });

    reg(i, "sorted?", ArityHint::Exact(1), |_i, args| {
        Ok(Value::Bool(matches!(&args[0], Value::SortedMap(_) | Value::SortedSet(_))))
    });

    reg(i, "sorted-map", ArityHint::Any, |interp, args| {
        if args.len() % 2 != 0 {
            return Err(RjError::arity("sorted-map: expected an even number of arguments"));
        }
        let mut entries: Vec<(Value, Value)> = Vec::new();
        for pair in args.chunks(2) {
            insert_sorted_entry(interp, &Comparator::Default, &mut entries, pair[0].clone(), pair[1].clone())?;
        }
        Ok(Value::SortedMap(Arc::new(SortedMapVal { cmp: Comparator::Default, entries })))
    });

    reg(i, "sorted-map-by", ArityHint::Min(1), |interp, args| {
        let cmp = Comparator::Fn(args[0].clone());
        let rest = &args[1..];
        if rest.len() % 2 != 0 {
            return Err(RjError::arity("sorted-map-by: expected an even number of arguments"));
        }
        let mut entries: Vec<(Value, Value)> = Vec::new();
        for pair in rest.chunks(2) {
            insert_sorted_entry(interp, &cmp, &mut entries, pair[0].clone(), pair[1].clone())?;
        }
        Ok(Value::SortedMap(Arc::new(SortedMapVal { cmp, entries })))
    });

    reg(i, "sorted-set", ArityHint::Any, |interp, args| {
        let mut entries: Vec<Value> = Vec::new();
        for x in args {
            insert_sorted_elem(interp, &Comparator::Default, &mut entries, x.clone())?;
        }
        Ok(Value::SortedSet(Arc::new(SortedSetVal { cmp: Comparator::Default, entries })))
    });

    reg(i, "sorted-set-by", ArityHint::Min(1), |interp, args| {
        let cmp = Comparator::Fn(args[0].clone());
        let mut entries: Vec<Value> = Vec::new();
        for x in &args[1..] {
            insert_sorted_elem(interp, &cmp, &mut entries, x.clone())?;
        }
        Ok(Value::SortedSet(Arc::new(SortedSetVal { cmp, entries })))
    });

    reg(i, "subseq", ArityHint::Range(3, 5), |interp, args| subseq_impl(interp, args, false));
    reg(i, "rsubseq", ArityHint::Range(3, 5), |interp, args| subseq_impl(interp, args, true));

    // `rseq` doesn't exist anywhere in mova yet (not just for the new
    // sorted/typed-vector types) -- real Clojure's `Reversible` is
    // implemented by vectors and sorted collections only (never lists or
    // hash maps/sets), which is exactly the arm set below.
    reg(i, "rseq", ArityHint::Exact(1), |_i, args| match &args[0] {
        // S7: measured `(rseq (first {:a 1}))` => `(1 :a)` -- an entry is
        // `Reversible`, like the vector it is.
        Value::Vector(items) | Value::MapEntry(items) => {
            if items.is_empty() {
                Ok(Value::Nil)
            } else {
                // M10.1: `PVecIter` no longer implements `DoubleEndedIterator`
                // (`champ::PVecIter` is a forward-only leaf-chunk
                // walk) -- reverse via a `Vec` round-trip instead of
                // `.iter().rev()`. `rseq` is cold, so this is not a
                // hot-path concern.
                let mut v: Vec<Value> = items.iter_cloned().collect();
                v.reverse();
                Ok(Value::List(v.into_iter().collect()))
            }
        }
        Value::TypedVec(tv) => {
            if tv.data.is_empty() {
                Ok(Value::Nil)
            } else {
                let mut v: Vec<Value> = tv.data.iter_cloned().collect();
                v.reverse();
                Ok(Value::List(v.into_iter().collect()))
            }
        }
        Value::SortedMap(m) => {
            if m.entries.is_empty() {
                Ok(Value::Nil)
            } else {
                Ok(Value::List(
                    m.entries
                        .iter()
                        .rev()
                        .map(|(k, v)| Value::MapEntry(PVec::pair(k.clone(), v.clone())))
                        .collect(),
                ))
            }
        }
        Value::SortedSet(s) => {
            if s.entries.is_empty() {
                Ok(Value::Nil)
            } else {
                Ok(Value::List(s.entries.iter().rev().cloned().collect()))
            }
        }
        other => Err(RjError::type_err(format!(
            "rseq: doesn't support random access: {}",
            other.type_name()
        ))),
    });

    reg(i, "vector-of", ArityHint::Min(1), |_i, args| {
        let kind = parse_vec_kind(&args[0])?;
        let mut data = PVec::new();
        for a in &args[1..] {
            data.push_back(coerce_for_kind(kind, a)?);
        }
        Ok(Value::TypedVec(Arc::new(TypedVecVal { kind, data })))
    });
}

#[cfg(test)]
mod tests {
    use crate::eval::Interp;
    use crate::printer::pr_str;

    fn eval(src: &str) -> String {
        let mut interp = Interp::new();
        let v = interp
            .eval_str("sorted-test", src)
            .unwrap_or_else(|e| panic!("eval error for {src:?}: {}", crate::error::render(&e, "sorted-test", src)));
        pr_str(&v)
    }

    /// W-GEO stage 1's named regression class (design doc §5): a keyword
    /// intern table hands ids out in CONSTRUCTION order, so any code path
    /// that ordered keywords by id instead of by text would scramble
    /// sorted-collection iteration. Every keyword below is constructed in
    /// deliberately anti-lexicographic order (`:zeta` first, `:alpha`
    /// last) and carries a `geo1-` prefix so these are genuinely FRESH
    /// intern-table entries in whatever order this test happens to run --
    /// the id order is therefore guaranteed to disagree with the text
    /// order, which is precisely the condition an id-compare bug needs to
    /// become visible.
    #[test]
    fn sorted_map_keyword_order_is_content_not_construction_order() {
        assert_eq!(
            eval("(pr-str (sorted-map :geo1-zeta 1 :geo1-mu 2 :geo1-delta 3 :geo1-alpha 4))"),
            "\"{:geo1-alpha 4, :geo1-delta 3, :geo1-mu 2, :geo1-zeta 1}\""
        );
        assert_eq!(
            eval("(vec (keys (sorted-map :geo1b-zulu 1 :geo1b-november 2 :geo1b-bravo 3)))"),
            "[:geo1b-bravo :geo1b-november :geo1b-zulu]"
        );
    }

    /// The same property for `sorted-set`, and for `compare` itself --
    /// `compare` is the user-visible surface of `natural_compare`'s keyword
    /// arm, so an id-order bug would show up here first.
    #[test]
    fn sorted_set_and_compare_agree_on_keyword_text_order() {
        assert_eq!(
            eval("(vec (sorted-set :geo1c-yankee :geo1c-oscar :geo1c-charlie))"),
            "[:geo1c-charlie :geo1c-oscar :geo1c-yankee]"
        );
        // Constructed newest-first: `:geo1d-zz` gets the LOWER id, so an
        // id compare would answer -1 where content order answers 1.
        assert_eq!(eval("(compare :geo1d-zz :geo1d-aa)"), "1");
        assert_eq!(eval("(compare :geo1d-aa :geo1d-zz)"), "-1");
        assert_eq!(eval("(compare :geo1d-aa :geo1d-aa)"), "0");
    }

    /// Namespaced keywords sort by the FLAT `"ns/name"` text (that is what
    /// `Value::Keyword` stores), unchanged by stage 1.
    #[test]
    fn namespaced_keywords_sort_by_flat_text() {
        assert_eq!(
            eval("(vec (keys (sorted-map :geo1e.z/a 1 :geo1e.a/z 2)))"),
            "[:geo1e.a/z :geo1e.z/a]"
        );
    }
}
