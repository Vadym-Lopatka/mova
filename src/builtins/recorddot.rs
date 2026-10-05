//! C14 (compat/c14-protocols): `(.method rec arg...)`-shaped instance
//! methods on a RECORD (`Value::Inst` with `tdef.is_record`) -- the
//! `java.util.Map` surface plus `.equals`/`.cons` that `protocols.clj`'s
//! `defrecord-acts-like-a-map`/`degenerate-defrecord-test`/`defrecord-
//! interfaces-test` deftests call directly, oracle-measured
//! (`compat/proto2-probe.clj` + its oracle transcript). Mirrors
//! `builtins::vecdot`'s shape: a small MEASURED method table, `None` for
//! anything not in it (falls through to `eval_dot_form`'s ordinary
//! "unresolved symbol"/interface-method path, same as vectors).
//!
//! BINDING DESIGN DIRECTIVE (owner): interop targets Rust-native values;
//! this is a thin compatibility veneer over the record's own `data` map,
//! never JVM emulation deeper than the suite demands. Scope is exactly the
//! dot-methods the target deftests call -- `deftype` (which has no map-
//! like `data`) never reaches this table (`eval_dot_form` only calls it
//! for `is_record` instances).

use crate::error::RjError;
use crate::eval::Interp;
use crate::reader::Span;
use crate::value::Value;

fn bad_arity(op: &str, got: usize) -> RjError {
    RjError::arity(format!("{op}: wrong number of args ({got})"))
}

/// Measured JVM shape (`java.util.Collections.unmodifiableMap`-backed
/// records): mutation throws `UnsupportedOperationException`. mova has no
/// exception-class taxonomy (matches every other S3/S4 host error, and the
/// suite's own `is`/`thrown?` shim is class-blind besides), so this is an
/// ordinary catchable `RjError` carrying the same message text real
/// Clojure would.
fn unsupported(op: &str) -> RjError {
    RjError::other(format!(
        "{op}: UnsupportedOperationException (records are immutable maps)"
    ))
}

/// `target` is already-evaluated and known `is_record` (checked by the
/// caller); `args` are the already-evaluated extra call arguments (method
/// name and receiver excluded).
pub(crate) fn record_dot_method(
    interp: &mut Interp,
    field: &str,
    target: &Value,
    args: &[Value],
    span: Span,
) -> Option<Result<Value, RjError>> {
    let Value::Inst(inst) = target else { return None };
    match field {
        // ---------- java.util.Map ----------
        "size" => {
            if !args.is_empty() {
                return Some(Err(bad_arity("size", args.len())));
            }
            Some(Ok(Value::Int(inst.data.len() as i64)))
        }
        "isEmpty" => {
            if !args.is_empty() {
                return Some(Err(bad_arity("isEmpty", args.len())));
            }
            Some(Ok(Value::Bool(inst.data.is_empty())))
        }
        "containsKey" => {
            if args.len() != 1 {
                return Some(Err(bad_arity("containsKey", args.len())));
            }
            Some(Ok(Value::Bool(inst.data.contains_key(&args[0]))))
        }
        "containsValue" => {
            if args.len() != 1 {
                return Some(Err(bad_arity("containsValue", args.len())));
            }
            Some(Ok(Value::Bool(inst.data.values().any(|v| *v == args[0]))))
        }
        "get" => {
            if args.len() != 1 {
                return Some(Err(bad_arity("get", args.len())));
            }
            Some(Ok(inst.data.get(&args[0]).cloned().unwrap_or(Value::Nil)))
        }
        "put" => Some(Err(unsupported("put"))),
        "remove" => Some(Err(unsupported("remove"))),
        "putAll" => Some(Err(unsupported("putAll"))),
        "clear" => Some(Err(unsupported("clear"))),
        "keySet" => {
            if !args.is_empty() {
                return Some(Err(bad_arity("keySet", args.len())));
            }
            Some(Ok(Value::Set(inst.data.keys().cloned().collect())))
        }
        // Measured: `(class (.values rec))` isn't exercised (only `(set
        // (.values rec))` is) -- a plain `List` is enough to feed `set`.
        "values" => {
            if !args.is_empty() {
                return Some(Err(bad_arity("values", args.len())));
            }
            Some(Ok(Value::List(inst.data.values().cloned().collect())))
        }
        // Measured: `#{[:a 1] [:b 2]}` `=`-compares directly against this
        // -- built from `Value::MapEntry` pairs (S7 wired `MapEntry` into
        // the same sequence-equality/hash class as a plain 2-vector, so a
        // `Set` doesn't care which concrete variant its elements are).
        "entrySet" => {
            if !args.is_empty() {
                return Some(Err(bad_arity("entrySet", args.len())));
            }
            let entries = inst
                .data
                .iter()
                .map(|(k, v)| Value::MapEntry(crate::value::PVec::pair(k.clone(), v.clone())))
                .collect();
            Some(Ok(Value::Set(entries)))
        }
        // ---------- Object/IPersistentCollection ----------
        // Measured: `.equals` on a record is exactly `clojure.core/=`'s
        // rule (field-and-type equality) -- delegates to the SAME
        // `values_equal` the `=` builtin itself calls, never a separate
        // implementation.
        "equals" => {
            if args.len() != 1 {
                return Some(Err(bad_arity("equals", args.len())));
            }
            Some(interp.values_equal(target, &args[0]).map(Value::Bool))
        }
        "cons" => {
            if args.len() != 1 {
                return Some(Err(bad_arity("cons", args.len())));
            }
            let _ = span;
            Some(crate::builtins::collections::conj_one(interp, target, &args[0]))
        }
        _ => None,
    }
}
