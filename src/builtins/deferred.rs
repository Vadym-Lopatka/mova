//! lsp/host (clojure-lsp-on-Mova campaign): a manually-completable future
//! cell. `promesa.core`'s `Deferred` (`mova/shims/promesa/core.mova`,
//! clojure-lsp-io worktree) needs exactly this shape -- a `future`/
//! `promise`-like cell that ANY thread can settle, exactly once, with
//! either a success value OR an arbitrary "rejected with this reason"
//! value, not just a background-thread-computed result -- and Mova
//! doesn't have one: `future*` (`builtins::conc`) always spawns its OWN
//! thread to produce the result; `promise` has no error channel at all.
//!
//! Reuses `Value::Future`/`FutureCell` completely unchanged: `deref`
//! (`builtins::atoms`), timeout support (`conc::future_deref`), and the
//! `future?`/`realized?` predicates (`builtins::predicates`) all already
//! dispatch on `Value::Future` -- a deferred IS a future as far as every
//! one of those is concerned, it is simply resolved by hand instead of by
//! a computation running on its own thread. This module adds only the
//! missing "settle it yourself" entry points, on top of `builtins::conc`'s
//! new `try_resolve_future` (first-settlement-wins, since -- unlike every
//! existing `Value::Future` resolver, each the sole writer for its own
//! cell by construction -- two threads may race to settle the same
//! deferred).
//!
//! A rejected deferred's `deref` re-throws `value` VERBATIM (`RjError::
//! thrown`, the exact mechanism `(throw v)` itself uses), so `(catch
//! SomeClass e ...)` on the eventual `deref` binds `e` to whatever was
//! passed to `mova-deferred-reject!` -- real promesa's own contract
//! (`p/reject!`/`p/catch` round-trip the exception object unchanged).

use std::sync::Arc;

use crate::builtins::ArityHint;
use crate::error::RjError;
use crate::eval::Interp;
use crate::value::{FutureCell, FutureState, Symbol, Value};

/// [`reg`](crate::builtins::reg), but registered ONLY under `ns/name` --
/// never as a bare global. Exact duplicate of `builtins::io`/`builtins::
/// strings`'s own private `reg_ns` (see either's doc for why this isn't
/// factored out instead).
#[track_caller]
fn reg_ns(
    i: &mut Interp,
    ns: &'static str,
    name: &'static str,
    arity: ArityHint,
    f: impl Fn(&mut Interp, &[Value]) -> Result<Value, RjError> + Send + Sync + 'static,
) {
    let native = crate::value::NativeFn::new(name, move |interp: &mut Interp, args: &[Value]| {
        if !arity.matches(args.len()) {
            return Err(RjError::arity(format!("{ns}/{name}: wrong number of args ({})", args.len()))
                .with_stack(interp.stack_snapshot(), interp.source_id));
        }
        f(interp, args)
    });
    i.globals.set_builtin(
        Symbol { ns: Some(ns.into()), name: name.into() },
        Value::Native(Arc::new(native)),
    );
}

// clojure-lsp campaign (mova/PLAN.md): moved off the bare `mova-deferred`/
// `mova-deferred-resolve!`/`mova-deferred-reject!` globals into the
// `mova.deferred` namespace (`deferred`/`resolve!`/`reject!`), registered
// the same way `builtins::io`'s `mova.io`/`mova.json`/`mova.digest` are --
// a plain native is trivially callable from any script that never
// requires the namespace, which is exactly the bare-global smell this
// campaign's own `java.*`-veneer rule (mova/PLAN.md) argues against for
// everything else. `mova/shims/promesa/*.mova` updated to match.
pub fn register(i: &mut Interp) {
    reg_ns(i, "mova.deferred", "deferred", ArityHint::Exact(0), |_i, _args| {
        Ok(Value::Future(Arc::new(FutureCell::pending())))
    });
    reg_ns(i, "mova.deferred", "resolve!", ArityHint::Exact(2), |_i, args| settle(args, false));
    reg_ns(i, "mova.deferred", "reject!", ArityHint::Exact(2), |_i, args| settle(args, true));
}

/// Settles `args[0]` (a `Value::Future` from `mova-deferred`) with
/// `args[1]` -- only if it is still pending; a later settle (either
/// function, on an already-settled cell) is a silent no-op, matching
/// real promesa's `resolve!`/`reject!`/`p/resolve`/`p/reject` on an
/// already-settled deferred (first settlement wins). Returns `true` if
/// THIS call performed the settlement, `false` if it was already
/// settled -- `promesa.core.mova`'s Clojure-level wrapper doesn't need
/// the return value (it always hands back the deferred itself, matching
/// promesa's own contract), but exposing it costs nothing and is more
/// honest than discarding it silently.
fn settle(args: &[Value], rejected: bool) -> Result<Value, RjError> {
    let cell = match &args[0] {
        Value::Future(cell) => cell.clone(),
        other => {
            return Err(RjError::type_err(format!(
                "expected a deferred (from mova.deferred/deferred), got {}",
                other.type_name()
            )))
        }
    };
    let new_state = if rejected {
        FutureState::Failed(RjError::thrown(args[1].clone()))
    } else {
        FutureState::Done(args[1].clone())
    };
    let settled = crate::builtins::conc::try_resolve_future(&cell, new_state);
    Ok(Value::Bool(settled))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::Symbol;

    const SPAN: crate::reader::Span = crate::reader::Span { start: 0, end: 0 };

    fn fresh_interp() -> Interp {
        let mut i = Interp::new();
        crate::builtins::register_all(&mut i);
        i
    }

    fn call(i: &mut Interp, name: &str, args: &[Value]) -> Result<Value, RjError> {
        let sym = match name.split_once('/') {
            Some((ns, n)) => Symbol { ns: Some(ns.into()), name: n.into() },
            None => Symbol::simple(name),
        };
        let f = i.globals.get(&sym).unwrap().clone();
        i.apply_value(&f, args, SPAN)
    }

    #[test]
    fn resolve_then_deref_returns_the_value() {
        let mut i = fresh_interp();
        let d = call(&mut i, "mova.deferred/deferred", &[]).unwrap();
        call(&mut i, "mova.deferred/resolve!", &[d.clone(), Value::Int(42)]).unwrap();
        let realized = call(&mut i, "realized?", &[d.clone()]).unwrap();
        assert_eq!(realized, Value::Bool(true));
        let v = call(&mut i, "deref", &[d]).unwrap();
        assert_eq!(v, Value::Int(42));
    }

    #[test]
    fn reject_then_deref_rethrows_the_exact_value() {
        let mut i = fresh_interp();
        let d = call(&mut i, "mova.deferred/deferred", &[]).unwrap();
        let reason = Value::Str("boom".into());
        call(&mut i, "mova.deferred/reject!", &[d.clone(), reason.clone()]).unwrap();
        let err = call(&mut i, "deref", &[d]).expect_err("rejected deferred must throw on deref");
        assert_eq!(err.thrown.as_ref(), Some(&reason));
    }

    #[test]
    fn second_settlement_is_a_no_op() {
        let mut i = fresh_interp();
        let d = call(&mut i, "mova.deferred/deferred", &[]).unwrap();
        let first = call(&mut i, "mova.deferred/resolve!", &[d.clone(), Value::Int(1)]).unwrap();
        let second = call(&mut i, "mova.deferred/reject!", &[d.clone(), Value::Str("nope".into())]).unwrap();
        assert_eq!(first, Value::Bool(true));
        assert_eq!(second, Value::Bool(false));
        let v = call(&mut i, "deref", &[d]).unwrap();
        assert_eq!(v, Value::Int(1));
    }
}
