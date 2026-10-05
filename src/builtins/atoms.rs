//! `atom deref swap! reset!`. `deref` also covers `future`/`promise`/`delay`
//! (v0.2 / A1) since Clojure's `deref`/`@` is the one polymorphic entry
//! point for all of `IDeref` -- see `builtins::conc` for the future/promise
//! blocking-wait machinery and delay's force-once machinery this dispatches
//! into.

use std::sync::{Arc, RwLock};

use crate::builtins::{conc, reg, ArityHint};
use crate::error::RjError;
use crate::eval::Interp;
use crate::pvec;
use crate::value::{AtomCell, Value};

/// `(push-thread-bindings {#'v1 val1 ...})`'s body, shared by the bare
/// `clojure.core` spelling and D5's `clojure.lang.Var/pushThreadBindings`
/// static spelling.
pub(crate) fn push_thread_bindings_native(args: &[Value]) -> Result<Value, RjError> {
    let Value::Map(m) = args[0].unmeta() else {
        return Err(RjError::type_err(format!(
            "push-thread-bindings: expected a map, got {}",
            args[0].type_name()
        )));
    };
    let mut pairs = Vec::with_capacity(m.len());
    for (k, v) in m.iter() {
        let Value::Var(cell) = k else {
            return Err(RjError::type_err(format!(
                "push-thread-bindings: expected a map of vars, got a key of type {}",
                k.type_name()
            )));
        };
        pairs.push((cell.clone(), v.clone()));
    }
    crate::env::push_thread_bindings(&pairs);
    Ok(Value::Nil)
}

/// kondo-wave: fires every real `add-watch` registration on `cell` --
/// `(f key atom-value old new)`, `IRef.notifyWatches`'s own call shape.
/// Clones the watch list out from under `cell.watches`'s lock FIRST,
/// then calls every `f` with NO lock held (not `watches`, not `state`)
/// -- a watch fn that derefs/`swap!`s the SAME atom, or adds/removes a
/// watch, must not deadlock or corrupt the list mid-iteration. Called
/// AFTER the commit, with the lock already dropped, by `reset!`/
/// `swap!`/`compare-and-set!`/`swap-vals!`/`reset-vals!`.
fn notify_watches(
    interp: &mut Interp,
    cell: &Arc<AtomCell>,
    atom_value: &Value,
    old: &Value,
    new: &Value,
) -> Result<(), RjError> {
    let watches = crate::sync::lock_mutex(&cell.watches).clone();
    for (key, f) in watches {
        interp.call_owned(&f, vec![key, atom_value.clone(), old.clone(), new.clone()])?;
    }
    Ok(())
}

pub fn register(i: &mut Interp) {
    reg(i, "atom", ArityHint::Exact(1), |_i, args| {
        Ok(Value::Atom(Arc::new(AtomCell::new(args[0].clone()))))
    });
    // `(--make-agent state)`: the cell `core.mova`'s `agent` builds on (an atom that prints as an Agent).
    reg(i, "--make-agent", ArityHint::Exact(1), |_i, args| {
        let mut cell = AtomCell::new(args[0].clone());
        cell.agent = true;
        Ok(Value::Atom(Arc::new(cell)))
    });

    // `(push-thread-bindings {#'v1 val1 ...})` / `(pop-thread-bindings)` --
    // S7 (tail wave): the LOW-LEVEL pair the `binding` special form itself
    // is built on, callable directly (rt.clj's `bare-rt-print` helper does
    // exactly this, below `binding`). See `env::push_thread_bindings`/
    // `env::pop_thread_bindings`'s docs for the frame-stack mechanics --
    // same per-cell `VarCell::push_binding`/`pop_binding` primitives
    // `eval_binding` uses, just callable outside a lexical `binding` form.
    reg(i, "push-thread-bindings", ArityHint::Exact(1), |_i, args| {
        push_thread_bindings_native(args)
    });

    reg(i, "pop-thread-bindings", ArityHint::Exact(0), |_i, _args| {
        crate::env::pop_thread_bindings();
        Ok(Value::Nil)
    });

    // D5: the SAME two, under the `clojure.lang.Var` static spellings the
    // vendored `clojure.pprint` writes them in (`pprint_base.clj`'s
    // `binding-map` macro, `(. clojure.lang.Var (pushThreadBindings
    // ~amap))` -- it needs a map computed at runtime, which the `binding`
    // special form's literal-symbol syntax cannot express). One body, two
    // spellings, exactly like `Class/forName`'s short and fully-qualified
    // rows in `builtins::statics`.
    crate::builtins::statics::reg_var_thread_binding_statics(i);

    // `(local-var* init)` -- S7 (tail wave): a fresh, UNINTERNED `Var`
    // holding `init` as its root -- see `VarCell::unbound`'s doc. The
    // `core.mova` macro `with-local-vars` (vars.clj's `test-with-local-
    // vars`) is built on this: `(with-local-vars [acc 1] body)` expands to
    // `(let [acc (local-var* 1)] body)`, giving `acc` an ordinary LEXICAL
    // binding to a real `Value::Var` that `@acc`/`(var-set acc ..)` work
    // on exactly like any other var, just never resolvable by name from
    // anywhere else (nothing ever `intern`s it).
    reg(i, "local-var*", ArityHint::Exact(1), |_i, args| {
        let cell = crate::env::VarCell::unbound(crate::value::Symbol::simple("local"));
        cell.store(args[0].clone(), false);
        Ok(Value::Var(cell))
    });

    // `(var-set v val)` -- S7 (tail wave): sets `v`'s ROOT value (like
    // `VarCell::store`, which every `def` also goes through). Real
    // Clojure's `var-set` requires `v` be thread-bound first (throws
    // otherwise); not reproduced here -- `with-local-vars`' cells are
    // never dynamically bound at all (see `local-var*`'s doc), and that
    // is the only caller in this task's corpus, so the stricter check is
    // unmeasured surface, not a gap this task's scope demands closing.
    reg(i, "var-set", ArityHint::Exact(2), |_i, args| match &args[0] {
        Value::Var(cell) => {
            cell.store(args[1].clone(), false);
            Ok(args[1].clone())
        }
        other => Err(RjError::type_err(format!("var-set: expected a var, got {}", other.type_name()))),
    });

    // `deref`/`@` covers atoms (instant), delays (force-once, runs in this
    // calling thread), and futures/promises (block, optionally with a
    // timeout: `(deref x timeout-ms default)`). Clojure's `deref` is
    // strictly 1- or 3-arity (never 2), so that's hand-checked here instead
    // of via `ArityHint`.
    reg(i, "deref", ArityHint::Any, |interp, args| {
        if args.len() != 1 && args.len() != 3 {
            return Err(RjError::arity(format!(
                "deref: expected 1 or 3 arguments, got {}",
                args.len()
            )));
        }
        let timeout_ms = if args.len() == 3 {
            match &args[1] {
                Value::Int(n) if *n >= 0 => Some(*n as u64),
                // lsp/host (clojure-lsp-on-Mova campaign): real Clojure's
                // 3-arg `deref` calls `(.deref ^IBlockingDeref ref
                // timeout-ms timeout-val)` -- a reflective method call
                // whose `long` parameter accepts a `double` ARGUMENT
                // EXPRESSION too (Clojure's reflective numeric coercion
                // widens/narrows to the target primitive type, it doesn't
                // require the caller to have written an integer literal).
                // Measured, load-bearing: `jsonrpc4clj.server/shutdown`
                // -- real, vendored, unmodified -- calls exactly `(deref
                // join 10e3 :timeout)`, a `Value::Float` timeout-ms; this
                // arm used to reject it outright ("must be a non-negative
                // int"), so EVERY `jsonrpc4clj.server/shutdown` call threw
                // immediately (caught and silently logged by
                // `receive-notification`'s blanket `catch Throwable`,
                // never surfacing) instead of blocking as intended -- see
                // this campaign's `mova/NOTES.md` for the full symptom.
                Value::Float(f) if *f >= 0.0 => Some(*f as u64),
                other => {
                    return Err(RjError::type_err(format!(
                        "deref: timeout-ms must be a non-negative number, got {}",
                        other.type_name()
                    )))
                }
            }
        } else {
            None
        };
        let default = args.get(2).cloned();
        match &args[0] {
            Value::Atom(cell) => Ok(crate::sync::lock_mutex(&cell.state).1.clone()),
            Value::Volatile(cell) => Ok(crate::sync::lock_read(cell).clone()),
            Value::Future(cell) => {
                let _g = crate::interrupt::WaitGuard::arm(&interp.intr);
                conc::future_deref(cell, timeout_ms, default).map_err(|e| take_intr(interp, e))
            }
            Value::Promise(cell) => {
                let _g = crate::interrupt::WaitGuard::arm(&interp.intr);
                conc::promise_deref(cell, timeout_ms, default).map_err(|e| take_intr(interp, e))
            }
            Value::Delay(cell) => conc::force_delay(interp, cell),
            // R2: `@#'x` reads the var's CURRENT value through its cell,
            // same polymorphic `deref` entry point as everything else here.
            // An unbound var (interned but never `def`d) derefs to `nil`
            // rather than erroring, matching this fn's overall leniency
            // (an atom/promise/future can never be "unbound" the same way,
            // so there's no precedent to match here beyond that spirit).
            Value::Var(cell) => Ok(cell.get().unwrap_or(Value::Nil)),
            // C13: real `Reduced` implements `IDeref` -- `@(reduced 5)` is
            // `5` (measured against the oracle).
            Value::Reduced(inner) => Ok((**inner).clone()),
            // D5: `IDeref` is an interface a `proxy`/`reify` may
            // implement, and `clojure.pprint` leans on that heavily --
            // its writers carry their whole mutable state behind
            // `(deref [] fields)`, read back as `@@this` in a dozen
            // places. `deref` is the ONE polymorphic entry point for
            // `IDeref` (this fn's module doc), so the instance's own
            // `deref` method belongs here rather than in a second
            // dispatcher. Uses the same `.method` lookup `(.deref x)`
            // would, so declaring the interface is not required -- only
            // having the method is, which matches how every other arm
            // here is structural rather than nominal.
            Value::Inst(inst) => {
                let f = crate::builtins::types::lookup_interface_method(
                    &interp.interfaces,
                    inst,
                    "deref",
                )
                .ok_or_else(|| {
                    RjError::type_err(format!(
                        "deref: {} has no deref method (does it implement clojure.lang.IDeref?)",
                        inst.tdef.name
                    ))
                })?;
                let recv = args[0].clone();
                interp.apply_value(&f, &[recv], crate::reader::Span { start: 0, end: 0 })
            }
            other => Err(RjError::type_err(format!(
                "deref: expected an atom, volatile, future, promise, delay, or var, got {}",
                other.type_name()
            ))),
        }
    });

    // C13 (sequences/transducers wave): `reduced`/`reduced?` -- the
    // early-termination primitive every stateful transducer this wave adds
    // (`take`, `take-nth`, `halt-when`, ...) needs. See [`crate::value::
    // Value::Reduced`]'s own doc for why this is a genuine `Value` variant
    // rather than a sentinel. `unreduced`/`ensure-reduced`/`completing`/
    // `transduce`/`halt-when` are plain mova (core.mova), built on these
    // two plus `deref` above -- no more Rust surface than this is needed.
    reg(i, "reduced", ArityHint::Exact(1), |_i, args| {
        Ok(Value::Reduced(Arc::new(args[0].clone())))
    });

    reg(i, "reduced?", ArityHint::Exact(1), |_i, args| {
        Ok(Value::Bool(matches!(&args[0], Value::Reduced(_))))
    });

    reg(i, "reset!", ArityHint::Exact(2), |interp, args| match &args[0] {
        Value::Atom(cell) => {
            let old = {
                let mut guard = crate::sync::lock_mutex(&cell.state);
                let old = guard.1.clone();
                guard.0 = guard.0.wrapping_add(1);
                guard.1 = args[1].clone();
                old
            };
            notify_watches(interp, cell, &args[0], &old, &args[1])?;
            Ok(args[1].clone())
        }
        other => Err(RjError::type_err(format!("reset!: expected an atom, got {}", other.type_name()))),
    });

    // CAS-retry, NOT a lock-free CAS loop in the classic sense (there's no
    // hardware CAS on `Value` -- it's arbitrary heap data), but the same
    // shape: never hold the atom's lock across the (interpreter-reentrant!)
    // call to `f`. If `f` itself derefs/swaps! this same atom (a realistic
    // pattern -- e.g. a fn that reads other atoms while computing), holding
    // the lock across that call would deadlock (same-thread re-lock) or, on
    // a naive per-call-fresh-lock design, silently corrupt the version
    // check. Instead:
    //   1. snapshot (version, value) under a *short* lock,
    //   2. compute `f(snapshot, ...extra-args)` with NO lock held,
    //   3. re-lock, and only commit if `version` hasn't moved (i.e. no
    //      other thread's swap!/reset! landed while we were computing);
    //      otherwise loop and retry from a fresh snapshot.
    // `version` is a plain `u64` counter (bumped on every successful
    // swap!/reset!) rather than comparing the old/new `Value`s for equality
    // -- cheap, exact (no false "unchanged" positives from a `Value` that
    // happens to `==` its predecessor), and needs no `Interp` (unlike
    // `values_equal`) to check.
    reg(i, "swap!", ArityHint::Min(2), |interp, args| {
        let cell = match &args[0] {
            Value::Atom(cell) => cell.clone(),
            other => return Err(RjError::type_err(format!("swap!: expected an atom, got {}", other.type_name()))),
        };
        let f = args[1].clone();
        let extra = &args[2..];
        loop {
            let (version, current) = {
                let guard = crate::sync::lock_mutex(&cell.state);
                (guard.0, guard.1.clone())
            };
            let mut call_args = Vec::with_capacity(1 + extra.len());
            call_args.push(current.clone());
            call_args.extend_from_slice(extra);
            // Handed over (phase 3): `call_args` is rebuilt per retry and
            // dead after the call. NOTE what this does *not* buy: the atom
            // cell still holds `current`, and must -- a version bump that
            // loses the race retries from a fresh snapshot -- so the compute
            // fn's receiver is never unique, by the same argument as
            // `flow`'s kept state. What it does buy is one clone per
            // argument per swap!.
            let new_val = interp.call_owned(&f, call_args)?;

            let committed = {
                let mut guard = crate::sync::lock_mutex(&cell.state);
                if guard.0 == version {
                    guard.0 = guard.0.wrapping_add(1);
                    guard.1 = new_val.clone();
                    true
                } else {
                    false
                }
            };
            if committed {
                notify_watches(interp, &cell, &args[0], &current, &new_val)?;
                return Ok(new_val);
            }
            // Another thread's swap!/reset! landed while `f` was computing;
            // drop the guard and retry from a fresh snapshot.
        }
    });

    // kondo-wave: real `compare-and-set!` -- `IRef.compareAndSet` (no
    // interpreter call, no retry loop: a plain one-shot equality check
    // under the atom's own lock). `oldv` compares by `Value`'s Rust
    // `PartialEq` (the same equality `=` itself resolves to for every
    // non-numeric-tower case relevant here), matching real Clojure's
    // `.equals`-based check.
    reg(i, "compare-and-set!", ArityHint::Exact(3), |interp, args| {
        let cell = match &args[0] {
            Value::Atom(cell) => cell.clone(),
            other => {
                return Err(RjError::type_err(format!(
                    "compare-and-set!: expected an atom, got {}",
                    other.type_name()
                )))
            }
        };
        let (old, new) = (args[1].clone(), args[2].clone());
        let swapped = {
            let mut guard = crate::sync::lock_mutex(&cell.state);
            if guard.1 == old {
                guard.0 = guard.0.wrapping_add(1);
                guard.1 = new.clone();
                true
            } else {
                false
            }
        };
        if swapped {
            notify_watches(interp, &cell, &args[0], &old, &new)?;
        }
        Ok(Value::Bool(swapped))
    });

    // kondo-wave: real `add-watch`/`remove-watch` -- see `AtomCell::
    // watches`'s doc and `notify_watches` above. Only `Value::Atom` gets
    // REAL firing (the measured need, `swap!`/`reset!`/`compare-and-
    // set!`); a `Var`/`Ref`/`Agent` reference (mova has no real `Ref`/
    // `Agent` distinct from an atom, and a `Var`'s root is a different
    // mechanism entirely) falls through as a harmless no-op, same as
    // the plain-mova stub this replaces -- `potemkin.link-vars`'s only
    // measured need is the INITIAL value, copied before `add-watch` is
    // even called. `add-watch` on an already-registered `key` REPLACES
    // that entry (measured real Clojure behavior), keyed by `Value`'s
    // own `PartialEq`.
    reg(i, "add-watch", ArityHint::Exact(3), |_i, args| {
        if let Value::Atom(cell) = &args[0] {
            let (key, f) = (args[1].clone(), args[2].clone());
            let mut watches = crate::sync::lock_mutex(&cell.watches);
            match watches.iter_mut().find(|(k, _)| *k == key) {
                Some(entry) => entry.1 = f,
                None => watches.push((key, f)),
            }
        }
        Ok(args[0].clone())
    });

    reg(i, "remove-watch", ArityHint::Exact(2), |_i, args| {
        if let Value::Atom(cell) = &args[0] {
            let key = &args[1];
            let mut watches = crate::sync::lock_mutex(&cell.watches);
            watches.retain(|(k, _)| k != key);
        }
        Ok(args[0].clone())
    });

    // `swap-vals!`/`reset-vals!` (measured on real Clojure 1.13.0-alpha6):
    // `[old new]`, a `clojure.lang.PersistentVector` -- `pvec![old, new]`
    // below produces exactly that shape. Same CAS-retry discipline as
    // `swap!` just above (never hold the lock across the reentrant call to
    // `f`); `reset-vals!` needs no retry loop at all since it never calls
    // back into the interpreter.
    reg(i, "swap-vals!", ArityHint::Min(2), |interp, args| {
        let cell = match &args[0] {
            Value::Atom(cell) => cell.clone(),
            other => return Err(RjError::type_err(format!("swap-vals!: expected an atom, got {}", other.type_name()))),
        };
        let f = args[1].clone();
        let extra = &args[2..];
        loop {
            let (version, current) = {
                let guard = crate::sync::lock_mutex(&cell.state);
                (guard.0, guard.1.clone())
            };
            let mut call_args = Vec::with_capacity(1 + extra.len());
            call_args.push(current.clone());
            call_args.extend_from_slice(extra);
            let new_val = interp.call_owned(&f, call_args)?;

            let committed = {
                let mut guard = crate::sync::lock_mutex(&cell.state);
                if guard.0 == version {
                    guard.0 = guard.0.wrapping_add(1);
                    guard.1 = new_val.clone();
                    true
                } else {
                    false
                }
            };
            if committed {
                notify_watches(interp, &cell, &args[0], &current, &new_val)?;
                return Ok(Value::Vector(pvec![current, new_val]));
            }
            // Retry from a fresh snapshot, same as `swap!`.
        }
    });

    reg(i, "reset-vals!", ArityHint::Exact(2), |interp, args| match &args[0] {
        Value::Atom(cell) => {
            let old = {
                let mut guard = crate::sync::lock_mutex(&cell.state);
                let old = guard.1.clone();
                guard.0 = guard.0.wrapping_add(1);
                guard.1 = args[1].clone();
                old
            };
            notify_watches(interp, cell, &args[0], &old, &args[1])?;
            Ok(Value::Vector(pvec![old, args[1].clone()]))
        }
        other => Err(RjError::type_err(format!("reset-vals!: expected an atom, got {}", other.type_name()))),
    });

    // ---- M4b: `volatile!`/`vswap!`/`vreset!`/`volatile?` ----
    // Measured on real Clojure 1.13.0-alpha6: `vswap!`'s macroexpansion is
    // `(. v reset (inc (.deref v)))` -- a bare read-then-write, no
    // compare-and-swap (`value.rs`'s `Value::Volatile` doc has the full
    // measurement). `volatile?` on an atom is `false` (measured); the two
    // reference types are unrelated as far as this predicate is concerned.
    reg(i, "volatile!", ArityHint::Exact(1), |_i, args| Ok(Value::Volatile(Arc::new(RwLock::new(args[0].clone())))));

    reg(i, "vreset!", ArityHint::Exact(2), |_i, args| match &args[0] {
        Value::Volatile(cell) => {
            *crate::sync::lock_write(cell) = args[1].clone();
            Ok(args[1].clone())
        }
        other => Err(RjError::type_err(format!("vreset!: expected a volatile, got {}", other.type_name()))),
    });

    // No CAS retry (unlike `swap!`/`swap-vals!` above): a `Volatile` is a
    // plain mutable field in real Clojure, so a racing writer here can
    // genuinely lose an update -- that IS the measured semantics, not a
    // shortcut. Still never holds the lock across the reentrant call to
    // `f`, for the same deadlock reason `swap!` doesn't (a `vswap!` inside
    // `f` on the SAME volatile must be able to read the pre-`f` value).
    reg(i, "vswap!", ArityHint::Min(2), |interp, args| {
        let cell = match &args[0] {
            Value::Volatile(cell) => cell.clone(),
            other => return Err(RjError::type_err(format!("vswap!: expected a volatile, got {}", other.type_name()))),
        };
        let f = args[1].clone();
        let extra = &args[2..];
        let current = crate::sync::lock_read(&cell).clone();
        let mut call_args = Vec::with_capacity(1 + extra.len());
        call_args.push(current);
        call_args.extend_from_slice(extra);
        let new_val = interp.call_owned(&f, call_args)?;
        *crate::sync::lock_write(&cell) = new_val.clone();
        Ok(new_val)
    });

    // `with-redefs-fn`: the map+thunk-shaped sibling of the `with-redefs`
    // special form (`eval_with_redefs`, `src/eval/special_forms.rs`) --
    // this one is an ordinary native since its bindings arrive as an
    // already-evaluated `Value::Map` of `#'var -> replacement` pairs, not
    // unevaluated `Form`s needing symbol resolution. Same discipline:
    // resolve+validate every entry before touching any root (an unbound or
    // non-var key must leave the world untouched), save every original
    // before storing any replacement, and restore in reverse order no
    // matter how the thunk exits.
    //
    // Measured divergence from real Clojure, deliberately kept, matching
    // `eval_with_redefs`'s own documented divergence: Clojure's
    // `with-redefs-fn` never errors on an unbound var (it round-trips
    // `.getRawRoot`'s `Unbound` sentinel, which mova has no equivalent of);
    // mova errors instead, the same choice `eval_with_redefs` already made.
    reg(i, "with-redefs-fn", ArityHint::Exact(2), |interp, args| {
        let map = match &args[0] {
            Value::Map(m) => m,
            other => {
                return Err(RjError::type_err(format!(
                    "with-redefs-fn: expected a map of vars to values, got {}",
                    other.type_name()
                )))
            }
        };
        let thunk = args[1].clone();
        let mut resolved = Vec::with_capacity(map.len());
        for (k, v) in map.iter() {
            let cell = match k {
                Value::Var(cell) => cell.clone(),
                other => {
                    return Err(RjError::type_err(format!(
                        "with-redefs-fn: expected a var key, got {}",
                        other.type_name()
                    )))
                }
            };
            resolved.push((cell, v.clone()));
        }
        let mut saved = Vec::with_capacity(resolved.len());
        for (cell, _) in &resolved {
            // W3e-3: `raw_root`, not `get` -- real `with-redefs-fn` is
            // `.getRawRoot` in and `.bindRoot` out, so a `with-redefs` under
            // a `binding` of the same var restores the ROOT rather than the
            // thread-local value that happened to be visible. See
            // `Interp::eval_with_redefs`'s doc for the oracle measurement.
            let original = cell.raw_root().ok_or_else(|| {
                RjError::other(format!(
                    "with-redefs-fn: var {} is unbound",
                    crate::printer::pr_str(&Value::Sym(cell.name.clone()))
                ))
            })?;
            saved.push((cell.clone(), original));
        }
        for (cell, replacement) in &resolved {
            cell.store(replacement.clone(), false);
        }
        let result = interp.call(&thunk, &[]);
        for (cell, original) in saved.iter().rev() {
            cell.store(original.clone(), false);
        }
        result
    });
}

/// P0c: a deref aborted by the interrupt flag consumes it, so the error kind
/// (soft/hard) follows the flag state.
fn take_intr(interp: &mut Interp, e: RjError) -> RjError {
    if e.kind == crate::error::ErrorKind::Interrupted {
        interp.intr.take_err().unwrap_or(e)
    } else {
        e
    }
}
