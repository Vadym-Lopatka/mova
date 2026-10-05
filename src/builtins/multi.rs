//! S4: the plain-function half of multimethods/hierarchies --
//! `remove-method`/`remove-all-methods`/`prefer-method`/`prefers`/
//! `methods`/`get-method`/`derive`/`underive`/`make-hierarchy`/`parents`/
//! `ancestors`/`descendants`. `defmulti`/`defmethod` themselves are special
//! forms (`eval::multi_forms`, mirroring `defprotocol`'s split); `isa?`'s
//! hierarchy-aware/3-arity extension lives in `builtins::types` (it already
//! owns the class-only 2-arity form -- extended there, not duplicated
//! here). Every semantic row was measured on 1.13.0-alpha6 -- see
//! `compat/multimethods-probe.clj`/`multimethods-probe2.clj` and
//! `crate::multi`'s module doc.

use crate::builtins::{reg, ArityHint};
use crate::error::RjError;
use crate::eval::Interp;
use crate::multi::{self, MultiDef};
use crate::value::{PMap, Value};

/// D5: `(.addMethod multifn dispatch-val f)` -- the INSTANCE method
/// `clojure.lang.MultiFn` exposes, and the one `defmethod` itself
/// macroexpands into on the JVM. Vendored `clojure.pprint`'s
/// `dispatch.clj` calls it directly (`use-method`, its own helper for
/// installing a dispatch method under a computed class value, which
/// `defmethod`'s literal-dispatch-value syntax cannot express). Performs
/// the same registry write `eval_defmethod` does, so a method installed
/// either way is indistinguishable afterward. Returns the multimethod,
/// matching `MultiFn.addMethod`'s `return this`.
///
/// Reachable two ways, deliberately: as the `.addMethod` interop arm in
/// `eval::types_forms::eval_dot_form` (the vendored spelling), and as the
/// internal `--multifn-add-method` native (so the `.`-form rewrite and
/// anything else needing it has an ordinary callable, with no second
/// implementation).
pub(crate) fn multifn_add_method(interp: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    let key = multi_key_or_err(interp, &args[0], ".addMethod")?;
    crate::sync::lock_write(&interp.multimethods.0)
        .get_mut(&key)
        .expect("membership just checked by multi_key_or_err")
        .methods
        .insert(args[1].clone(), args[2].clone());
    // W-MULTI: same cache-invalidating write `eval_defmethod` performs.
    multi::bump_multi_generation();
    Ok(args[0].clone())
}

/// SPEC-W1 task 5: `(.dispatchFn mm)` -- the fn `defmulti` was given,
/// verbatim (`MultiDef::dispatch_fn`). `clojure.spec.alpha`'s
/// `multi-spec-impl` needs it to compute "the dispatch value THIS argument
/// would take" without invoking the multimethod, which no existing
/// core builtin exposes. Reached only from `eval_dot_form`'s interop arm
/// -- the upstream spelling -- since nothing else asks for it.
pub(crate) fn multifn_dispatch_fn(interp: &Interp, mm: &Value) -> Result<Value, RjError> {
    let key = multi_key_or_err(interp, mm, ".dispatchFn")?;
    Ok(crate::sync::lock_read(&interp.multimethods.0)
        .get(&key)
        .expect("membership just checked by multi_key_or_err")
        .dispatch_fn
        .clone())
}

/// SPEC-W1 task 5: `(.getMethod mm dispatch-val)` -- byte-for-byte the
/// `get-method` builtin's contract (best method for the dispatch value
/// under the multimethod's hierarchy and preferences, else the `:default`
/// method, else `nil`), reached through the interop spelling
/// `clojure.spec.alpha`'s `multi-spec-impl` uses.
pub(crate) fn multifn_get_method(
    interp: &mut Interp,
    who: &str,
    mm: &Value,
    dispatch_val: &Value,
) -> Result<Value, RjError> {
    let key = multi_key_or_err(interp, mm, who)?;
    let (name, methods, prefers, default_val, href) = {
        let reg_guard = crate::sync::lock_read(&interp.multimethods.0);
        let def = reg_guard
            .get(&key)
            .expect("membership just checked by multi_key_or_err");
        (
            def.name.clone(),
            def.methods.clone(),
            def.prefers.clone(),
            def.default_val.clone(),
            def.hierarchy_ref.clone(),
        )
    };
    let h = multi::resolve_hierarchy_value(interp, &href);
    if let Some((_k, f)) = multi::find_best_method(interp, &name, &methods, &prefers, &h, dispatch_val)? {
        return Ok(f);
    }
    Ok(methods.get(&default_val).cloned().unwrap_or(Value::Nil))
}

/// Resolves a "multimethod" argument to its registry key, erroring like a
/// type mismatch if `v` isn't a currently-registered multimethod.
fn multi_key_or_err(interp: &Interp, v: &Value, who: &str) -> Result<usize, RjError> {
    multi::multi_key(v)
        .filter(|k| crate::sync::lock_read(&interp.multimethods.0).contains_key(k))
        .ok_or_else(|| RjError::type_err(format!("{who}: expected a multimethod, got {}", v.type_name())))
}

/// `(namespace v)`-shaped check used only for `derive`/`underive`'s 2-arity
/// (global) namespace assertions -- measured: a `Value::Sym`'s namespace
/// lives in its own `ns` field, but `Value::Keyword` has no such field (a
/// namespaced keyword's reader token is one flat `"ns/name"` string, see
/// `builtins::strings`' `name` builtin doc), so a keyword needs the same
/// `/`-split rule the reader itself uses.
///
/// ALSO accepts a keyword whose stored text starts with `:` (i.e. TWO
/// colons at the read site, `::foo`): mova's reader has no `::`
/// namespace-alias resolution (documented gap, see `builtins::flow`'s
/// module doc and `tests/conformance/corpus/flow.corpus`'s header comment
/// -- out of scope for this workstream, a reader-level fix) and reads
/// `::foo` as a keyword literally named `:foo`, one leading colon short of
/// real Clojure's `current-ns/foo`. Rejecting that token here as
/// "unnamespaced" would make EVERY `::tag` in idiomatic Clojure code
/// (including the entire vendored `multimethods.clj` suite) fail this
/// assert before hierarchy logic ever runs. The token is still a stable,
/// unique `Value` (opaque hashable identity is all `derive`/`isa?`/
/// dispatch actually need) -- what's lost is only the COSMETIC namespace
/// text a `pr-str` of it would show, already a divergence this file's
/// forms live with as ordinary `DIVERGE`s.
fn is_namespaced(v: &Value) -> bool {
    match v {
        Value::Sym(s) => s.ns.is_some(),
        Value::Keyword(k) => {
            let s = k.as_ref();
            s.starts_with(':') || matches!(s.find('/'), Some(idx) if idx > 0 && idx + 1 < s.len())
        }
        _ => false,
    }
}

/// Measured (`compat/multimethods-probe2.clj` q01/q02): only the 2-arity
/// (global) `derive` asserts these -- `underive` does not (no assert calls
/// in real Clojure's `underive` source), and neither does 3-arity `derive`
/// (q09). Order matters -- `(namespace parent)` is checked FIRST (q01:
/// namespaced tag + bare parent fails on the PARENT message), then the tag
/// check (q02: bare tag + namespaced parent fails on the TAG message).
fn assert_derive_namespaces(tag: &Value, parent: &Value) -> Result<(), RjError> {
    if !is_namespaced(parent) {
        return Err(RjError::other("Assert failed: (namespace parent)"));
    }
    if !(matches!(tag, Value::Class(_)) || is_namespaced(tag)) {
        return Err(RjError::other(
            "Assert failed: (or (class? tag) (and (instance? clojure.lang.Named tag) (namespace tag)))",
        ));
    }
    Ok(())
}

pub fn install(i: &mut Interp) {
    multi::install_global_hierarchy(i);

    // `(make-hierarchy)` -- `{:parents {} :ancestors {} :descendants {}}`.
    reg(i, "make-hierarchy", ArityHint::Exact(0), |_i, _args| {
        Ok(Value::Map(multi::empty_hierarchy()))
    });

    // `derive`: 2-arity mutates the GLOBAL hierarchy (with the extra
    // namespace asserts above); 3-arity is pure, operating on the given
    // hierarchy value and returning a NEW one (measured: `compat/
    // multimethods-probe2.clj` q09 -- no namespace asserts on this arity).
    reg(i, "derive", ArityHint::Range(2, 3), |interp, args| {
        if args.len() == 2 {
            let (tag, parent) = (&args[0], &args[1]);
            assert_derive_namespaces(tag, parent)?;
            let eq = interp.values_equal(tag, parent)?;
            let cur = multi::global_hierarchy_value(interp);
            let next = multi::derive_pure(&cur, tag, parent, eq)?;
            multi::set_global_hierarchy_value(interp, next);
            Ok(Value::Nil)
        } else {
            let Value::Map(h) = &args[0] else {
                return Err(RjError::type_err(format!(
                    "derive: expected a hierarchy map, got {}",
                    args[0].type_name()
                )));
            };
            let (tag, parent) = (&args[1], &args[2]);
            let eq = interp.values_equal(tag, parent)?;
            Ok(Value::Map(multi::derive_pure(h, tag, parent, eq)?))
        }
    });

    // `underive`: measured -- neither arity asserts namespaces (no assert
    // calls in real Clojure's `underive` source).
    reg(i, "underive", ArityHint::Range(2, 3), |interp, args| {
        if args.len() == 2 {
            let (tag, parent) = (&args[0], &args[1]);
            let cur = multi::global_hierarchy_value(interp);
            let next = multi::underive_pure(&cur, tag, parent);
            multi::set_global_hierarchy_value(interp, next);
            Ok(Value::Nil)
        } else {
            let Value::Map(h) = &args[0] else {
                return Err(RjError::type_err(format!(
                    "underive: expected a hierarchy map, got {}",
                    args[0].type_name()
                )));
            };
            Ok(Value::Map(multi::underive_pure(h, &args[1], &args[2])))
        }
    });

    reg(i, "parents", ArityHint::Range(1, 2), |interp, args| {
        if args.len() == 1 {
            let h = multi::global_hierarchy_value(interp);
            Ok(multi::parents_of(&h, &args[0]).unwrap_or(Value::Nil))
        } else {
            let Value::Map(h) = &args[0] else {
                return Err(RjError::type_err(format!(
                    "parents: expected a hierarchy map, got {}",
                    args[0].type_name()
                )));
            };
            Ok(multi::parents_of(h, &args[1]).unwrap_or(Value::Nil))
        }
    });

    reg(i, "ancestors", ArityHint::Range(1, 2), |interp, args| {
        if args.len() == 1 {
            let h = multi::global_hierarchy_value(interp);
            Ok(multi::ancestors_of(&h, &args[0]).unwrap_or(Value::Nil))
        } else {
            let Value::Map(h) = &args[0] else {
                return Err(RjError::type_err(format!(
                    "ancestors: expected a hierarchy map, got {}",
                    args[0].type_name()
                )));
            };
            Ok(multi::ancestors_of(h, &args[1]).unwrap_or(Value::Nil))
        }
    });

    reg(i, "descendants", ArityHint::Range(1, 2), |interp, args| {
        if args.len() == 1 {
            let h = multi::global_hierarchy_value(interp);
            Ok(multi::descendants_of(&h, &args[0]).unwrap_or(Value::Nil))
        } else {
            let Value::Map(h) = &args[0] else {
                return Err(RjError::type_err(format!(
                    "descendants: expected a hierarchy map, got {}",
                    args[0].type_name()
                )));
            };
            Ok(multi::descendants_of(h, &args[1]).unwrap_or(Value::Nil))
        }
    });

    reg(i, "methods", ArityHint::Exact(1), |interp, args| {
        let key = multi_key_or_err(interp, &args[0], "methods")?;
        let reg_guard = crate::sync::lock_read(&interp.multimethods.0);
        Ok(Value::Map(reg_guard.get(&key).expect("checked above").methods.clone()))
    });

    reg(i, "prefers", ArityHint::Exact(1), |interp, args| {
        let key = multi_key_or_err(interp, &args[0], "prefers")?;
        let reg_guard = crate::sync::lock_read(&interp.multimethods.0);
        Ok(Value::Map(reg_guard.get(&key).expect("checked above").prefers.clone()))
    });

    // SPEC-W1 task 5: ONE implementation, shared verbatim with the
    // `.getMethod` interop spelling -- see `multifn_get_method`. `who`
    // keeps each spelling's own name in the not-a-multimethod error.
    reg(i, "get-method", ArityHint::Exact(2), |interp, args| {
        let dispatch_val = args[1].clone();
        multifn_get_method(interp, "get-method", &args[0], &dispatch_val)
    });

    // D5: `(.addMethod multifn dispatch-val f)` -- the INSTANCE method
    // `clojure.lang.MultiFn` exposes, and the one `defmethod` itself
    // macroexpands into on the JVM. Vendored `clojure.pprint`'s
    // `dispatch.clj` calls it directly (`use-method`, its own helper for
    // installing a dispatch method by class rather than by literal
    // syntax) because `defmethod` cannot take a computed dispatch value.
    // Same registry write `eval_defmethod` performs, so a method
    // installed either way is indistinguishable afterward. Returns the
    // multimethod, matching `MultiFn.addMethod`'s `return this`.
    reg(i, "--multifn-add-method", ArityHint::Exact(3), multifn_add_method);

    reg(i, "remove-method", ArityHint::Exact(2), |interp, args| {
        let key = multi_key_or_err(interp, &args[0], "remove-method")?;
        let mut reg_guard = crate::sync::lock_write(&interp.multimethods.0);
        reg_guard
            .get_mut(&key)
            .expect("checked above")
            .methods
            .remove(&args[1]);
        drop(reg_guard);
        multi::bump_multi_generation();
        Ok(args[0].clone())
    });

    reg(i, "remove-all-methods", ArityHint::Exact(1), |interp, args| {
        let key = multi_key_or_err(interp, &args[0], "remove-all-methods")?;
        let mut reg_guard = crate::sync::lock_write(&interp.multimethods.0);
        let def = reg_guard.get_mut(&key).expect("checked above");
        def.methods = PMap::new();
        def.prefers = PMap::new();
        drop(reg_guard);
        multi::bump_multi_generation();
        Ok(args[0].clone())
    });

    // `(prefer-method mm x y)` -- x is preferred over y. Measured
    // (`clojure.lang.MultiFn.preferMethod`): rejects a conflicting REVERSE
    // preference (`y` already, possibly indirectly, preferred over `x`).
    reg(i, "prefer-method", ArityHint::Exact(3), |interp, args| {
        let key = multi_key_or_err(interp, &args[0], "prefer-method")?;
        let (x, y) = (args[1].clone(), args[2].clone());
        let (name, prefers, href) = {
            let reg_guard = crate::sync::lock_read(&interp.multimethods.0);
            let def = reg_guard.get(&key).expect("checked above");
            (def.name.clone(), def.prefers.clone(), def.hierarchy_ref.clone())
        };
        let h = multi::resolve_hierarchy_value(interp, &href);
        if multi::prefers_transitive(&prefers, &h, &y, &x) {
            return Err(RjError::other(format!(
                "Preference conflict in multimethod '{name}': {} is already preferred to {}",
                crate::printer::pr_str(&y),
                crate::printer::pr_str(&x),
            )));
        }
        let mut reg_guard = crate::sync::lock_write(&interp.multimethods.0);
        let def: &mut MultiDef = reg_guard.get_mut(&key).expect("checked above");
        let cur = match def.prefers.get(&x) {
            Some(Value::Set(s)) => s.clone(),
            _ => champ::PersistentHashSet::new(),
        };
        def.prefers.insert(x, Value::Set(cur.insert(y)));
        drop(reg_guard);
        multi::bump_multi_generation();
        Ok(args[0].clone())
    });
}
