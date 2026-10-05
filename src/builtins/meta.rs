//! S5 / M3: `meta` / `with-meta` / `vary-meta` / `alter-meta!` /
//! `reset-meta!`.
//!
//! # The two metadata systems
//!
//! Clojure has TWO, and conflating them is the single easiest way to get
//! this subsystem wrong -- `tests/conformance/pending/metadata.corpus`
//! calls the resulting confusion "the atom trap" and spends three forms
//! pinning it:
//!
//! - **`IObj` -- immutable, on the VALUE.** Collections, symbols, fns and
//!   lazy seqs. `with-meta` returns a NEW value; the old one is
//!   unchanged. Implemented as [`crate::value::Value::Meta`], a wrapper
//!   variant (see `crate::value::MetaObj` for why a wrapper and not a
//!   field).
//! - **`IReference` -- mutable, on the CELL.** Vars and atoms.
//!   `alter-meta!`/`reset-meta!` mutate in place, and every holder of the
//!   same var/atom sees the change. Implemented as a `meta` slot on
//!   [`crate::env::VarCell`] / [`crate::value::AtomCell`].
//!
//! They are disjoint, not a hierarchy: an atom is `IReference` and NOT
//! `IObj`, so `(with-meta (atom 1) {:a 1})` THROWS (measured:
//! ClassCastException, "class clojure.lang.Atom cannot be cast to class
//! clojure.lang.IObj") even though `(alter-meta! (atom 1) assoc :d 1)`
//! works fine. `meta` is the one fn that reads both.
//!
//! # `nil` is not `false` here
//!
//! `(with-meta nil {:a 1})` throws a **NullPointerException**, not the
//! ClassCastException every other non-`IObj` gets (measured -- the corpus
//! pins both, deliberately, as "a distinct case worth pinning"): Clojure
//! reaches `((IObj) x).withMeta(m)` and dereferences a null `x` before
//! any cast can fail. mova has no exception-class taxonomy yet
//! (`tests/pending_conformance_test.rs`'s `ErrBoth` doc), so both land as
//! a `TypeErr` here -- but the MESSAGES are kept distinct so the
//! eventual taxonomy pass has the distinction already made.

use crate::builtins::{reg, ArityHint};
use crate::error::RjError;
use crate::eval::Interp;
use crate::value::{Keyword, PMap, PVec, Str, Value};

/// `(meta x)` for BOTH metadata systems -- the `IObj` wrapper if `x` is
/// wrapped, the cell's mutable slot if `x` is a var or an atom, else
/// `nil`.
///
/// Measured: `(meta 1)`, `(meta :k)`, `(meta nil)` and `(meta (atom 1))`
/// are all `nil` -- asking a non-`IObj` for its metadata is NOT an error,
/// only trying to *attach* metadata to one is.
pub fn meta_of_value(v: &Value) -> Value {
    match v {
        // IReference: read the cell, not the value.
        Value::Var(cell) => crate::coredocs::with_docs(cell, cell.var_meta()),
        Value::Atom(cell) => crate::sync::lock_read(&cell.meta).clone(),
        // IObj: read the wrapper (`Nil` when there isn't one).
        other => other.obj_meta(),
    }
}

/// Rejects a metadata argument that isn't a map. Measured:
/// `(with-meta [1] [1 2])` throws ClassCastException ("cannot be cast to
/// class clojure.lang.IPersistentMap"); `nil` is accepted and STRIPS.
fn check_meta_map(m: &Value, op: &str) -> Result<(), RjError> {
    // Unwrap first: a metadata arg that itself carries meta (e.g. `(with-meta
    // {} (with-meta {:a 1} {:m 1}))`) must be judged on its data, not on the
    // `Value::Meta` wrapper variant -- same fix as `conj`'s map branch.
    match m.unmeta() {
        Value::Nil | Value::Map(_) | Value::SortedMap(_) | Value::HostStruct(_) | Value::LazyMap(_) | Value::StructMap(_) => Ok(()),
        other => Err(RjError::type_err(format!(
            "{op}: metadata must be a map, got {}",
            other.type_name()
        ))),
    }
}

/// Whether `v` can carry `IObj` metadata.
///
/// The measured membership list, from `metadata.corpus`'s header and
/// re-verified per type: collections (list/vector/map/set, plus mova's
/// sorted/typed variants, which are those same Clojure classes),
/// symbols, fns/macros, lazy seqs, and records. Everything else -- and
/// notably `nil`, numbers, strings, keywords, booleans, chars, atoms,
/// vars, and every host/cell-backed variant -- is not `IObj`.
///
/// Deliberately spelled as an explicit allow-list rather than a
/// `!matches!` deny-list: a variant added by a future milestone should
/// default to "cannot carry metadata" and be opted in by whoever measured
/// it, not silently inherit a semantics nobody checked.
///
/// C3c: `pub(crate)` (was private) -- `crate::types::builtin_classes`'s
/// `clojure.lang.IObj` row reuses this SAME allow-list rather than
/// duplicating it (measured: `(instance? clojure.lang.IObj x)` and
/// "does `with-meta` throw on `x`" are the exact same question on the
/// real JVM, so one predicate answers both). `Value::Queue` was ADDED to
/// this list by that same task (measured: `(instance? clojure.lang.IObj
/// (into clojure.lang.PersistentQueue/EMPTY [1 2 3]))` => `true` --
/// before this, `(with-meta queue {..})` threw "queue does not support
/// metadata", a real gap, not a deviation: `PersistentQueue` genuinely
/// implements `IObj` on the JVM).
pub(crate) fn is_iobj(v: &Value) -> bool {
    matches!(
        v,
        Value::List(_)
            | Value::Vector(_)
            | Value::Map(_)
            | Value::Set(_)
            | Value::SortedMap(_)
            | Value::SortedSet(_)
            | Value::TypedVec(_)
            | Value::HostStruct(_) | Value::LazyMap(_)
            | Value::StructMap(_)
            | Value::Sym(_)
            | Value::Fn(_)
            | Value::Native(_)
            | Value::Macro(_)
            | Value::Lazy(_)
            | Value::Inst(_)
            | Value::Meta(_)
            | Value::Queue(_)
            // W4C-NS (sequences.clj's `test-seqs-implements-iobj`):
            // `clojure.core.VecSeq`/`APersistentVector$RSeq` are both
            // ordinary `ISeq`s on the real JVM and both measured `IObj`
            // (`(instance? clojure.lang.IMeta (seq (vector-of :long 1 2
            // 3)))` => `true`, `(meta (with-meta (seq ...) {:a true}))` =>
            // `{:a true}`) -- same as every other seq shape already
            // listed here (`List`/`Lazy`).
            | Value::VecSeq(_)
    )
}

/// The shared `with-meta`/`vary-meta` attach step, including both
/// measured rejections. See this module's doc for why `nil` gets its own
/// message.
fn attach(target: &Value, meta: Value, op: &str) -> Result<Value, RjError> {
    check_meta_map(&meta, op)?;
    if matches!(target, Value::Nil) {
        return Err(RjError::type_err(format!(
            "{op}: cannot attach metadata to nil (Clojure: NullPointerException)"
        )));
    }
    if !is_iobj(target) {
        return Err(RjError::type_err(format!(
            "{op}: {} does not support metadata (Clojure: ClassCastException, not an IObj)",
            target.type_name()
        )));
    }
    Ok(Value::attach_meta(target.clone(), meta))
}

/// `alter-meta!`/`reset-meta!`'s receiver: the two `IReference` types.
/// Anything else is an error -- measured: `(alter-meta! [1] assoc :a 1)`
/// throws ClassCastException, an `IObj` value is NOT an acceptable
/// receiver just because it can hold metadata some other way.
fn reference_meta(v: &Value, op: &str) -> Result<Value, RjError> {
    match v {
        Value::Var(cell) => Ok(cell.var_meta()),
        Value::Atom(cell) => Ok(crate::sync::lock_read(&cell.meta).clone()),
        other => Err(RjError::type_err(format!(
            "{op}: expected a var or an atom, got {} (Clojure: ClassCastException, not an IReference)",
            other.type_name()
        ))),
    }
}

/// Writes an `IReference` receiver's metadata slot. Assumes
/// [`reference_meta`] already validated the receiver.
fn set_reference_meta(v: &Value, m: Value) {
    match v {
        Value::Var(cell) => cell.set_var_meta(m),
        Value::Atom(cell) => *crate::sync::lock_write(&cell.meta) = m,
        _ => unreachable!("reference_meta validated the receiver"),
    }
}

pub fn register(i: &mut Interp) {
    reg(i, "meta", ArityHint::Exact(1), |_i, args| {
        Ok(meta_of_value(&args[0]))
    });

    reg(i, "with-meta", ArityHint::Exact(2), |_i, args| {
        attach(&args[0], args[1].clone(), "with-meta")
    });

    // `(vary-meta obj f & args)` = `(with-meta obj (apply f (meta obj)
    // args))`. Note the `f` sees `nil` when there's no metadata yet, and
    // that this is exactly why `(meta (vary-meta [1] assoc :a 1))` is
    // `{:a 1}` and not an error: `(assoc nil :a 1)` is `{:a 1}`.
    reg(i, "vary-meta", ArityHint::Min(2), |interp, args| {
        let current = meta_of_value(&args[0]);
        let mut call_args = Vec::with_capacity(args.len() - 1);
        call_args.push(current);
        call_args.extend_from_slice(&args[2..]);
        let next = interp.call_owned(&args[1], call_args)?;
        attach(&args[0], next, "vary-meta")
    });

    // `(alter-meta! r f & args)` -> the NEW metadata map (measured: the
    // return value is the new map, not the reference).
    reg(i, "alter-meta!", ArityHint::Min(2), |interp, args| {
        let current = reference_meta(&args[0], "alter-meta!")?;
        let mut call_args = Vec::with_capacity(args.len() - 1);
        call_args.push(current);
        call_args.extend_from_slice(&args[2..]);
        let next = interp.call_owned(&args[1], call_args)?;
        check_meta_map(&next, "alter-meta!")?;
        set_reference_meta(&args[0], next.clone());
        Ok(next)
    });

    // `(reset-meta! r m)` -> `m` (measured).
    reg(i, "reset-meta!", ArityHint::Exact(2), |_i, args| {
        reference_meta(&args[0], "reset-meta!")?;
        check_meta_map(&args[1], "reset-meta!")?;
        set_reference_meta(&args[0], args[1].clone());
        Ok(args[1].clone())
    });

    // field1/W-EXPLAIN: `(compile-explain f)` -- the REPL-facing half of
    // tier-decision observability (`MOVA_EXPLAIN=1`'s eprintln lines are
    // the other). See `compile::explain`'s module doc for the data this
    // reads and `Interp::compile_explain`'s field doc for why it is a
    // name-keyed registry rather than a field on the fn value itself.
    reg(i, "compile-explain", ArityHint::Exact(1), |interp, args| {
        Ok(compile_explain_value(interp, &args[0]))
    });

    // field4/W-LENS-1: `(runtime-report)` -- the regret ledger as EDN.
    // `(runtime-report :reset)` additionally records a windowing baseline;
    // `:lens/events` stays MONOTONE either way (gate 4: windowing is the
    // CONSUMER's subtraction, and a reset never writes another thread's
    // counter page -- it records a baseline the report subtracts into
    // `:lens/window`). See `crate::lens` and docs/W-LENS-SCHEMA.md.
    //
    // Data out, nothing else (design-doc principle 2): no rendering, no
    // formatting, no transport. `embed::Engine::lens_report` returns the
    // identical map to an embedding host.
    reg(i, "runtime-report", ArityHint::Range(0, 1), |interp, args| {
        let reset = matches!(args.first(), Some(Value::Keyword(k)) if k.as_ref() == "reset");
        Ok(crate::lens::report(reset, interp.globals.retired_root_maps()))
    });
}

fn kw(name: &'static str) -> Value {
    Value::Keyword(Keyword::from(name))
}

/// `(compile-explain f)`'s body, split out for testability
/// (`compile::explain`'s own unit tests call this indirectly through
/// `eval_str`). Reads `Interp::compile_explain`, populated as a side
/// effect of every `compile::compile_fn` call -- but under lazy tier-up
/// (v0.6) that call may not have happened yet (a fn tree-walks until it
/// crosses the `MOVA_LAZY_TIER_N` call threshold), so this FORCES the
/// compile decision now, on demand, via `CompileSlot::on_call(0, ..)`, if
/// it has not settled already. A fn already settled (compiled, bailed, or
/// eager mode) short-circuits that call for free.
fn compile_explain_value(interp: &mut Interp, f: &Value) -> Value {
    let name = match f {
        Value::Fn(c) | Value::Macro(c) => {
            let _ = c.compiled.on_call(0, || {
                let saved_source = std::mem::replace(&mut interp.source_id, c.def_source_id.get());
                let r = crate::compile::compile_fn(interp, c.name.as_ref(), c.arities.as_slice(), &c.env, c.def_span);
                interp.source_id = saved_source;
                r
            });
            c.name.clone()
        }
        _ => {
            return meta_map_of([
                (kw("tier"), kw("unknown")),
                (
                    Value::Keyword(Keyword::from("error")),
                    Value::Str(Str::from(format!(
                        "compile-explain: expected a fn, got {}",
                        f.type_name()
                    ))),
                ),
            ]);
        }
    };
    let Some(name) = name else {
        return meta_map_of([
            (kw("tier"), kw("unknown")),
            (
                kw("reason"),
                Value::Str(Str::from(
                    "anonymous fn -- compile-explain only tracks fns bound to a name (def/defn)",
                )),
            ),
        ]);
    };
    let Some(explain) = interp.compile_explain.get(&name) else {
        return meta_map_of([
            (kw("tier"), kw("unknown")),
            (
                kw("reason"),
                Value::Str(Str::from(
                    "no compile decision recorded for this name (never went through eval_fn_form, or a later redefinition of the same name overwrote it)",
                )),
            ),
        ]);
    };
    match &explain.tier {
        crate::compile::explain::FnTier::TreeWalk { reason, span, source_id, preview } => {
            let mut m = vec![
                (kw("tier"), kw("tree-walk")),
                (kw("reason"), Value::Str(Str::from(reason.as_str()))),
                (kw("at"), Value::Str(Str::from(at_string(interp, *source_id, *span)))),
            ];
            // H1: only present when MOVA_EXPLAIN built it -- see
            // `FnTier::TreeWalk::preview`'s doc.
            if let Some(p) = preview {
                m.push((kw("preview"), Value::Str(Str::from(p.as_str()))));
            }
            meta_map_of(m)
        }
        crate::compile::explain::FnTier::Compiled { loops, escapes } => {
            let loop_vals: Vec<Value> = loops.iter().map(|l| loop_explain_value(interp, l)).collect();
            meta_map_of([
                (kw("tier"), kw("compiled")),
                (kw("loops"), Value::Vector(PVec::from_iter(loop_vals))),
                // field3/W-RESOLVE: how many interop forms inside this fn
                // compiled to an `Ir::Escape` (tree-walked in place)
                // instead of bailing the whole fn. `0` is the clean case.
                (kw("escapes"), Value::Int(*escapes as i64)),
            ])
        }
    }
}

fn loop_explain_value(interp: &Interp, l: &crate::compile::explain::LoopExplain) -> Value {
    let at = Value::Str(Str::from(at_string(interp, l.source_id, l.span)));
    match &l.decision {
        crate::compile::explain::LoopDecision::Specialized { lanes, superloop } => meta_map_of([
            (kw("numloop"), Value::Bool(true)),
            (kw("lanes"), Value::Bool(*lanes)),
            (kw("superloop"), Value::Bool(*superloop)),
            (kw("at"), at),
        ]),
        crate::compile::explain::LoopDecision::Generic { reason } => meta_map_of([
            (kw("numloop"), Value::Bool(false)),
            (kw("reason"), Value::Str(Str::from(*reason))),
            (kw("at"), at),
        ]),
    }
}

/// field5/W-SPAN: thin wrapper over `source_registry::render_at` -- see
/// that fn's doc for the fallback behavior when `source_id` is 0/unknown.
fn at_string(interp: &Interp, source_id: u32, span: crate::reader::Span) -> String {
    crate::source_registry::render_at(interp, source_id, span)
}

/// Builds a `{:key val, ...}` metadata map. Shared by `def`'s
/// symbol-metadata flow-through (`eval::special_forms`) so it doesn't
/// hand-roll a `PMap` per call site.
pub fn meta_map_of(pairs: impl IntoIterator<Item = (Value, Value)>) -> Value {
    let mut m = PMap::new();
    for (k, v) in pairs {
        m.insert(k, v);
    }
    Value::Map(m)
}
