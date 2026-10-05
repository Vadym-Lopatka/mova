//! C2 (defstruct): legacy struct-maps -- `create-struct`/`struct`/
//! `struct-map`/`accessor`. `defstruct` itself is a `core/core.mova`
//! macro (`(def name (create-struct keys...))`, real Clojure's own
//! expansion, `.oracle/clojure-src/src/clj/clojure/core.clj:4068`).
//!
//! Ground truth: `compat/structmap-probe.clj` replayed against
//! `.oracle` Clojure 1.13.0-alpha6 (`compat/structmap-oracle-transcript.txt`).
//! Representation: [`crate::value::Value::StructMap`]/
//! `Value::StructBasis` -- see `value::StructMapVal`'s doc for the
//! basis-then-ext layout every helper below maintains.
//!
//! Measured semantics this module implements:
//! - `create-struct`: variadic keyword args -> a `Value::StructBasis`,
//!   `Arc`-identity only (`(= (create-struct :a :b) (create-struct :a
//!   :b))` is `false`).
//! - `struct`: `[basis & vals]`, positional -- missing trailing vals
//!   default to `nil`; MORE vals than basis keys throws
//!   `IllegalArgumentException: "Too many arguments to struct
//!   constructor"` (message-exact, kind untyped like every other mova
//!   error today).
//! - `struct-map`: `[basis & kvs]` -- any subset of basis keys plus
//!   arbitrary extension keys; a later duplicate key (basis OR ext)
//!   overwrites the earlier one in place, never adds a second slot.
//! - `accessor`: `[basis key]` -> a fn. Applying it: `ClassCastException`-
//!   flavored rejection for a non-`StructMap` receiver, `RuntimeException:
//!   "Accessor/struct mismatch"` for a `StructMap` whose basis is a
//!   DIFFERENT `Arc` (even with byte-identical keys) from the one the
//!   accessor closed over, else the looked-up value (defaulting to `nil`
//!   for an unset -- always-present, per `struct`'s nil-fill -- basis
//!   slot).
//!
//! `assoc`/`dissoc`/`conj`/`get`/`count`/`contains?`/`keys`/`vals`/
//! `empty`/`seq`/map-as-fn/keyword-lookup arms live at their existing
//! generic choke points (`builtins::collections`, `eval::apply`,
//! `eval::mod::seq_items`) -- this module only owns the shared entry-
//! mutation helper ([`set_entry`]) those call sites share, plus the four
//! natives above.

use std::sync::Arc;

use crate::builtins::{reg, ArityHint};
use crate::error::RjError;
use crate::eval::Interp;
use crate::value::{Keyword, NativeFn, Str, StructMapVal, Value};

fn require_basis<'a>(v: &'a Value, op: &str) -> Result<&'a Arc<Vec<Value>>, RjError> {
    match v {
        Value::StructBasis(b) => Ok(b),
        other => Err(RjError::type_err(format!("{op}: not a struct basis: {}", other.type_name()))),
    }
}

/// `accessor`'s second argument only -- NOT `create-struct`'s keys (see
/// that native's own doc: real `create-struct` never type-checks its
/// args at all). Kept here for the one caller that still wants a literal
/// keyword.
fn require_keyword(v: &Value, op: &str) -> Result<Str, RjError> {
    match v.unmeta() {
        Value::Keyword(k) => Ok(k.text()),
        other => Err(RjError::type_err(format!(
            "{op}: struct keys must be keywords, got {}",
            other.type_name()
        ))),
    }
}

/// A fresh basis-order entry vec, every basis slot defaulted to `nil` --
/// the starting point `struct`/`struct-map` both build from (measured:
/// `(struct basis)` with no vals is `{:a nil, :b nil}`). The basis key
/// VALUE itself becomes the entry's key, unchanged (W4-EVAL task 2) -- no
/// coercion to `Value::Keyword`, since the basis can hold any key object
/// now, metadata included.
pub(crate) fn new_entries(basis: &Arc<Vec<Value>>) -> Vec<(Value, Value)> {
    basis.iter().map(|k| (k.clone(), Value::Nil)).collect()
}

/// THE shared mutation choke point every struct-map write op (`assoc`,
/// `conj`, `struct-map`'s own kv-pair loop) goes through -- see
/// `value::StructMapVal`'s doc for the layout invariant this maintains:
/// a `k` matching a basis keyword updates that FIXED slot's value only
/// (the slot's position and key never move); an already-present
/// extension key updates in place; anything else is appended as a new
/// extension entry, in first-introduction order (measured: call-order
/// among extension keys given BEFORE a basis key in a `struct-map` call
/// still prints/iterates basis-first -- `basis.len()` is exactly where
/// the fixed prefix ends, always).
///
/// W4-EVAL task 2: a plain `slot.0 == k` (`Value`'s own `PartialEq`,
/// which already recurses through a meta-carrying key on EITHER side)
/// replaces the old keyword-only special case now that a basis key can
/// be any `Value`, not just `Value::Keyword`. Matching a basis slot no
/// longer requires `k` itself to be a keyword -- it requires `k` to be
/// `=` to whatever that slot's ORIGINAL key was, exactly the oracle's
/// `Def.keyslots` lookup (an ordinary structural-equality map keyed by
/// the same key objects `entryAt` hands back).
pub(crate) fn set_entry(entries: &mut Vec<(Value, Value)>, basis_len: usize, k: Value, v: Value) {
    for slot in entries.iter_mut().take(basis_len) {
        if slot.0 == k {
            slot.1 = v;
            return;
        }
    }
    for slot in entries.iter_mut().skip(basis_len) {
        if slot.0 == k {
            slot.1 = v;
            return;
        }
    }
    entries.push((k, v));
}

/// `assoc` on a struct-map -- ALWAYS stays a `StructMap` (measured, both
/// for a basis key and a brand-new extension key), unlike `HostStruct`'s
/// v1 widen-to-`Map` policy.
pub(crate) fn struct_map_assoc(sm: &StructMapVal, k: &Value, v: &Value) -> Value {
    let mut entries = sm.entries.clone();
    set_entry(&mut entries, sm.basis.len(), k.clone(), v.clone());
    Value::StructMap(Arc::new(StructMapVal {
        basis: sm.basis.clone(),
        entries,
    }))
}

pub(crate) fn struct_map_get<'a>(sm: &'a StructMapVal, k: &Value) -> Option<&'a Value> {
    sm.entries.iter().find(|(ek, _)| ek == k).map(|(_, v)| v)
}

/// `find`'s struct-map entry lookup (W4-EVAL task 2): unlike
/// [`struct_map_get`], which every OTHER caller needs, `find` must hand
/// back the ORIGINAL stored key object -- not the query key `k` -- because
/// that key may carry metadata (`Value::Meta`) `k` itself does not
/// (measured: `(meta (key (find (struct s 1) 'k)))` is `{:a "A"}` even
/// when `find` was called with a bare, unmeta'd `'k`; the oracle's
/// `entryAt` is `MapEntry.create(e.getKey(), ...)`, `e.getKey()` being the
/// key `createSlotMap` originally closed over, not the lookup argument).
pub(crate) fn struct_map_entry<'a>(sm: &'a StructMapVal, k: &Value) -> Option<(&'a Value, &'a Value)> {
    sm.entries.iter().find(|(ek, _)| ek == k).map(|(ek, v)| (ek, v))
}

/// `dissoc` -- a basis key can NEVER be removed (measured exact message:
/// `RuntimeException: "Can't remove struct key"`); an extension key
/// dissocs in place and the result stays a `StructMap`; a key that was
/// never present (basis or ext) is a no-op returning an equivalent
/// struct.
pub(crate) fn struct_map_dissoc(sm: &StructMapVal, k: &Value) -> Result<Value, RjError> {
    let basis_len = sm.basis.len();
    // W4-EVAL task 2: basis keys are no longer necessarily `Value::Keyword`
    // (see `set_entry`'s doc) -- compare `k` directly against each basis
    // key's VALUE, same generalization.
    if sm.basis.iter().any(|b| b == k) {
        return Err(RjError::other("Can't remove struct key"));
    }
    let mut entries = sm.entries.clone();
    if let Some(pos) = entries.iter().skip(basis_len).position(|(ek, _)| ek == k) {
        entries.remove(basis_len + pos);
    }
    Ok(Value::StructMap(Arc::new(StructMapVal {
        basis: sm.basis.clone(),
        entries,
    })))
}

/// `conj` -- a `[k v]` pair or a map/struct-map merges via [`set_entry`]
/// one pair at a time (matching `assoc`'s "always stays a StructMap"
/// policy, measured for both a basis-key and an extension-key pair).
pub(crate) fn struct_map_conj(sm: &StructMapVal, item: &Value) -> Result<Value, RjError> {
    match item {
        // clojure-lsp campaign (mova/PLAN.md): `(conj m nil)` is a
        // measured no-op on every real Clojure map type -- same
        // treatment `builtins::collections::conj_one`'s `Map` arm gives
        // it.
        Value::Nil => Ok(Value::StructMap(Arc::new(StructMapVal {
            basis: sm.basis.clone(),
            entries: sm.entries.clone(),
        }))),
        // S7 x C2 merge: a `Value::MapEntry` IS a `[k v]` 2-vector, so
        // `(conj a-struct-map (first other-map))` and `(into a-struct-map
        // (seq other-map))` behave like the vector-pair spelling.
        Value::Vector(pair) | Value::MapEntry(pair) if pair.len() == 2 => {
            Ok(struct_map_assoc(sm, &pair[0], &pair[1]))
        }
        Value::Map(m) => {
            let mut entries = sm.entries.clone();
            for (k, v) in m.iter() {
                set_entry(&mut entries, sm.basis.len(), k.clone(), v.clone());
            }
            Ok(Value::StructMap(Arc::new(StructMapVal {
                basis: sm.basis.clone(),
                entries,
            })))
        }
        Value::StructMap(other) => {
            let mut entries = sm.entries.clone();
            for (k, v) in other.entries.iter() {
                set_entry(&mut entries, sm.basis.len(), k.clone(), v.clone());
            }
            Ok(Value::StructMap(Arc::new(StructMapVal {
                basis: sm.basis.clone(),
                entries,
            })))
        }
        other => Err(RjError::type_err(format!(
            "conj: map conj arg must be a [k v] pair or a map, got {}",
            other.type_name()
        ))),
    }
}

pub fn register(i: &mut Interp) {
    // W4-EVAL task 2, measured (`PersistentStructMap.createSlotMap`, which
    // takes an `ISeq` of arbitrary key objects with NO type check at all):
    // `create-struct` keys are not restricted to keywords -- every real
    // corpus caller happens to use keywords, but `evaluation.clj`'s
    // `Metadata` deftest builds a basis from `(with-meta 'k {:a "A"})`, a
    // SYMBOL, specifically to prove `find` hands its metadata back later.
    // `args.to_vec()` keeps each key exactly as evaluated -- `Value::Meta`
    // wrapper included, since this `reg` (not `reg_unmeta`) registration
    // never unwraps them.
    reg(i, "create-struct", ArityHint::Any, |_i, args| {
        Ok(Value::StructBasis(Arc::new(args.to_vec())))
    });

    reg(i, "struct", ArityHint::Min(1), |_i, args| {
        let basis = require_basis(&args[0], "struct")?;
        let vals = &args[1..];
        if vals.len() > basis.len() {
            return Err(RjError::other("Too many arguments to struct constructor"));
        }
        let mut entries = new_entries(basis);
        for (idx, v) in vals.iter().enumerate() {
            entries[idx].1 = v.clone();
        }
        Ok(Value::StructMap(Arc::new(StructMapVal {
            basis: basis.clone(),
            entries,
        })))
    });

    reg(i, "struct-map", ArityHint::Min(1), |_i, args| {
        let basis = require_basis(&args[0], "struct-map")?;
        let kvs = &args[1..];
        if kvs.len() % 2 != 0 {
            return Err(RjError::arity("struct-map: expected key/value pairs"));
        }
        let basis_len = basis.len();
        let mut entries = new_entries(basis);
        for pair in kvs.chunks(2) {
            set_entry(&mut entries, basis_len, pair[0].clone(), pair[1].clone());
        }
        Ok(Value::StructMap(Arc::new(StructMapVal {
            basis: basis.clone(),
            entries,
        })))
    });

    reg(i, "accessor", ArityHint::Exact(2), |_i, args| {
        let basis = require_basis(&args[0], "accessor")?.clone();
        let key = require_keyword(&args[1], "accessor")?;
        let f = move |_interp: &mut Interp, cargs: &[Value]| -> Result<Value, RjError> {
            if cargs.len() != 1 {
                return Err(RjError::arity(format!(
                    "struct accessor: called with {} argument(s), expects 1",
                    cargs.len()
                )));
            }
            match &cargs[0] {
                Value::StructMap(sm) if Arc::ptr_eq(&sm.basis, &basis) => {
                    Ok(struct_map_get(sm, &Value::Keyword(Keyword::from(&key))).cloned().unwrap_or(Value::Nil))
                }
                Value::StructMap(_) => Err(RjError::other("Accessor/struct mismatch")),
                other => Err(RjError::type_err(format!(
                    "accessor: {} cannot be cast to a struct-map",
                    other.type_name()
                ))),
            }
        };
        Ok(Value::Native(Arc::new(NativeFn::new("struct-accessor", f))))
    });
}
