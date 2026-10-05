//! S4: callable namespace machinery -- `require` (a plain function in
//! real Clojure, unlike `import`/`ns`; see `eval::special_forms`'s dispatch
//! comment for why `import` is a special form instead), `ns-name`,
//! `find-ns`, `the-ns`, `ns-resolve`, `resolve`, `all-ns`. Every semantic
//! here was measured on 1.13.0-alpha6 (`compat/nsfns-probe.clj`); the
//! actual namespace-value plumbing (`ns_value`/`the_ns`/`find_ns_value`/
//! `all_ns_values`/`ns_resolve_in`/`try_resolve_var_cell`/
//! `require_spec_value`) lives on `Interp` in `crate::ns`, shared with the
//! `(ns ...)` `:require` clause and the `*ns*` global.

use crate::builtins::{reg, reg_unmeta, ArityHint};
use crate::error::{JvmClass, RjError};
use crate::eval::Interp;
use crate::reader::Span;
use crate::value::{Str, Symbol, Value};

/// A synthetic zero-width span for natives that need to hand one to
/// `Interp` methods written for the tree-walker (`require_spec_value`/
/// `import_spec_value` take a `Span` so their error messages CAN carry a
/// source location when called from special-form position; a native fn
/// has no span of its own to give them).
const NATIVE_SPAN: Span = Span { start: 0, end: 0 };

// sci-shim/load-hook: a generic "custom require resolver" primitive, added
// per mova/PLAN.md rule 3 (missing standard-Clojure-shaped capability, no
// existing hook point). Real sci's `:load-fn` lets embedder code supply
// source for a namespace `require` can't find on disk/embedded; Mova's own
// `require`/`ns` has no such extension point, so this is a thread-local
// stack of "resolver" fns (each `(fn [{:keys [namespace]}] {:file .. :source
// ..} | nil)`), consulted by `Interp::require_ns` (see `ns.rs`) only AFTER
// its normal disk+embedded lookup fails, so it changes nothing for callers
// that never push a hook. `thread_local!` (not an `Interp` field) keeps the
// diff to this one file: `Interp` has many constructors that would all need
// a new field threaded through for no benefit (the hook is a dynamic,
// call-scoped resource, not per-interpreter state).
thread_local! {
    static LOAD_HOOKS: std::cell::RefCell<Vec<Value>> = std::cell::RefCell::new(Vec::new());
}

/// Consulted by `Interp::require_ns` when a namespace isn't found on disk
/// or embedded. Calls the innermost active hook (if any) with `{:namespace
/// sym}`; a `nil` result means "hook declines", falling through to the
/// caller's normal not-found error.
pub(crate) fn try_load_hook(interp: &mut Interp, ns: &str) -> Result<Option<(Str, Str)>, RjError> {
    let hook = LOAD_HOOKS.with(|h| h.borrow().last().cloned());
    let Some(hook) = hook else { return Ok(None) };
    let arg = Value::Map(crate::pmap![
        Value::Keyword(crate::value::Keyword::construct("namespace")) => Value::Sym(Symbol::simple(ns.to_string()))
    ]);
    let result = interp.call(&hook, &[arg])?;
    match result {
        Value::Map(m) => {
            let file = m.get(&Value::Keyword(crate::value::Keyword::construct("file"))).cloned();
            let source = m.get(&Value::Keyword(crate::value::Keyword::construct("source"))).cloned();
            match (file, source) {
                (Some(Value::Str(f)), Some(Value::Str(s))) => Ok(Some((f, s))),
                _ => Ok(None),
            }
        }
        _ => Ok(None),
    }
}

/// `Value::Keyword`'s `ns/name` split for `namespace`'s keyword arm.
/// Mirrors `builtins::strings::symbol_from_str`/`reader::parse_symbol`'s
/// splitting rule (split on the FIRST `/`, with `"/"` itself a bare
/// name) -- duplicated rather than shared, because that fn is private to
/// `strings.rs`, a file this task's file-ownership rules cannot touch
/// (see this crate's S6 compat-batch task brief). Three lines, so the
/// duplication cost is low; `namespace`'s own doc below is the single
/// call site.
fn keyword_ns(full: &str) -> Option<Str> {
    if full == "/" {
        return None;
    }
    let idx = full.find('/')?;
    (idx > 0 && idx + 1 < full.len()).then(|| full[..idx].into())
}

/// S7 (tail wave); W-EMBED (2026-08-22) added the bare-core-var lookup
/// (first arm below) and the real-stderr fallback (final `else`). Writes
/// `s` through whatever `*err*` is CURRENTLY capturing, in this order:
///
/// 1. **The bare core `*err*`** (`core/core.mova`'s `(def ^:dynamic *err*
///    nil)`, next to `*out*`) -- exactly the candidate `out_write` checks
///    first for `*out*`. A `binding`/`with-err-str` of THIS var, from
///    anywhere, wins over everything below: it is the one real,
///    interpreter-level `*err*` mova now has, and an embedder using
///    [`crate::embed::Engine::eval_capture`] or a script calling
///    `with-err-str` directly is bound through this cell specifically, so
///    it must be checked before any suite-shim fallback could steal the
///    write.
/// 2. **`<current_ns>/*err*`** -- the vendored suite's own `mova-test-shim.
///    mova`/`mova-test-helper-shim.mova` splice a shim-local `(def
///    ^:dynamic *err* nil)` + `with-err-string-writer` (built on
///    `binding`) into every test file's own namespace; this is how a
///    warning raised while that file's own namespace is current reaches
///    it. Only reached when (1) found no LIVE (atom-bound) core `*err*` --
///    core `*err*` defaults to `nil`, so an ordinary suite-file run (which
///    never binds the core var at all) falls straight through to this
///    arm, unchanged from before this var existed.
/// 3. **`Env::find_var_cells_named` fallback** -- `current_ns` names no
///    `*err*` of its own -- the common case is code running inside
///    `mova-test-helper-shim.mova`'s `eval-in-temp-ns`, which switches to
///    a freshly gensym'd namespace the shim was never spliced into
///    (measured: `warn_if_non_dynamic_earmuff`'s warning silently
///    vanished from `with-err-print-writer`'s capture until this fallback
///    existed -- see `Env::find_var_cells_named`'s own doc for the full
///    measured scenario). Fall back to whichever OTHER namespace's own
///    `*err*` is CURRENTLY bound to an atom (i.e. a real capture is live
///    right now) -- there is at most one at a time in every vendored file
///    this corpus runs (each file is its own mova process), so "the
///    first live one found" is unambiguous here, mirroring real Clojure's
///    single process-wide `*err*`.
/// 4. **Real process stderr** (`eprint!`, mirroring `out_write`'s own
///    `print!` fallback for `*out*`) -- nothing above found a live atom
///    capture, so `s` is never silently dropped: an embedder that never
///    binds `*err*` at all still sees every warning on the process's real
///    stderr, exactly like an unbound `*out*` still reaches real stdout.
pub(crate) fn write_shim_err(interp: &mut Interp, s: &str) {
    let core_sym = Symbol::simple("*err*");
    // a capture the code itself set up (`*err*` of this namespace bound to an atom) wins
    // over the process-wide stream below (the vendored suite's `with-err-string-writer`)
    let own_sym = Symbol { ns: Some(interp.current_ns.clone()), name: Str::from("*err*") };
    let own_atom = matches!(interp.globals.get(&own_sym), Some(Value::Atom(_)))
        || (interp.globals.get(&core_sym).is_some_and(|v| matches!(v, Value::HostInst(_)))
            && interp.globals.find_var_cells_named(&Str::from("*err*")).into_iter().any(|c| matches!(c.get(), Some(Value::Atom(_)))));
    // nREPL: `*err*` bound to a host stream (the session's `err` sink).
    if let (false, Some(Value::HostInst(h))) = (own_atom, interp.globals.get(&core_sym)) {
        if h.kind == crate::hostclass::HostKind::OutputStream
            && crate::hostclass::stream_write_str(&h, s).is_ok()
        {
            let _ = crate::hostclass::stream_flush(&h);
            return;
        }
    }
    let atom_cell = match interp.globals.get(&core_sym) {
        Some(Value::Atom(cell)) => Some(cell),
        _ => {
            let sym = Symbol { ns: Some(interp.current_ns.clone()), name: Str::from("*err*") };
            match interp.globals.get(&sym) {
                Some(Value::Atom(cell)) => Some(cell),
                _ => interp
                    .globals
                    .find_var_cells_named(&Str::from("*err*"))
                    .into_iter()
                    .find_map(|cell| match cell.get() {
                        Some(Value::Atom(a)) => Some(a),
                        _ => None,
                    }),
            }
        }
    };
    match atom_cell {
        Some(cell) => {
            let mut guard = crate::sync::lock_mutex(&cell.state);
            let mut appended = match &guard.1 {
                Value::Str(existing) => existing.to_string(),
                _ => String::new(),
            };
            appended.push_str(s);
            guard.0 = guard.0.wrapping_add(1);
            guard.1 = Value::Str(Str::from(appended));
        }
        None => eprint!("{s}"),
    }
}

/// C3c (rt.clj's `ns-intern-policies`): `(.refer ns sym var)` dot-surface
/// on a `clojure.lang.Namespace` value -- direct-maps `sym` to `var` in
/// `ns`'s mapping table, exactly `clojure.lang.Namespace.refer(Symbol,
/// Var)`'s real Java shape (`.oracle`'s `Namespace.java`, `checkReplacement`
/// specifically): unlike the `refer` FUNCTION (which refers a whole other
/// namespace's public vars, `unqualified -> owning-ns-name`, `crate::ns`'s
/// `refers` table), THIS maps `sym` to one SPECIFIC var value handed in --
/// which may already carry a completely different bare name (measured
/// call site: `(.refer ns 'flatten v1)` where `v1` is actually `#'ns/foo`)
/// -- so it rides `Env::bind_alias` (a direct `sym -> this exact cell`
/// binding), not `intern`'s get-or-create-a-cell-named-`sym` path.
///
/// Policy (measured against `Namespace.java`'s `checkReplacement`, the
/// exact table in that method's own comment):
/// - `sym` currently resolves (in `ns`) to NO var, or to a var that is
///   NOT natively interned in `ns` under this exact name (a REFERRED/
///   aliased mapping, e.g. the default `clojure.core` refer every fresh
///   namespace starts with, or a previous `.refer` call) -- replace it,
///   printing a `"WARNING: ..."` line through `*err*` when something WAS
///   there before (silent when nothing was).
/// - `sym` currently resolves to a var natively interned in `ns` itself
///   under that exact name (`cell.name == ns/sym`, `VarCell::name`'s own
///   "a cell's identity IS its name" doc) -- REJECTED, no replacement;
///   prints a `"REJECTED: ..."` line through `*err*` instead. Matches
///   `Namespace.java`'s own literal `"REJECTED: attempt to replace
///   interned var ..."` text closely (not verbatim -- message BODIES
///   aren't conformance-scored, only the `WARNING`/`REJECTED` PREFIX the
///   vendored deftest actually asserts on, per `CONFORMANCE-GUARANTEE.md`).
///
/// `None` when `target` isn't a namespace value at all (lets the caller's
/// generic "no field or interface method" error fire instead of silently
/// mishandling a non-namespace `.refer` receiver).
pub(crate) fn refer_dot_method(interp: &mut Interp, target: &Value, args: &[Value], span: Span) -> Option<Result<Value, RjError>> {
    let ns_name = crate::ns::ns_value_name(target)?;
    let Value::Sym(sym) = args[0].unmeta() else {
        return Some(Err(RjError::type_err(format!(
            ".refer: expected a symbol, got {}",
            args[0].type_name()
        ))
        .with_span(span)));
    };
    let Value::Var(new_cell) = &args[1] else {
        return Some(Err(RjError::type_err(format!(
            ".refer: expected a var, got {}",
            args[1].type_name()
        ))
        .with_span(span)));
    };
    let bare = Symbol::simple(sym.name.clone());
    let prior = match interp.ns_resolve_in(target, &bare) {
        Ok(p) => p,
        Err(e) => return Some(Err(e.with_span(span))),
    };
    let full = Symbol { ns: Some(ns_name.clone()), name: sym.name.clone() };
    if let Some(old_cell) = &prior {
        if std::sync::Arc::ptr_eq(old_cell, new_cell) {
            // Already refers to this exact var -- a true no-op, matching
            // `reference`'s own early-return when `o == val` (see the
            // Java source's `if(o == val) return o;`).
            return Some(Ok(args[1].clone()));
        }
        let natively_interned = old_cell.name == full;
        if natively_interned {
            write_shim_err(
                interp,
                &format!(
                    "REJECTED: attempt to replace interned var #'{} with #'{} in {ns_name}, you must ns-unmap first\n",
                    fmt_sym(&old_cell.name),
                    fmt_sym(&new_cell.name),
                ),
            );
            return Some(Ok(Value::Var(old_cell.clone())));
        }
        write_shim_err(
            interp,
            &format!(
                "WARNING: {} already refers to: #'{} in namespace: {ns_name}, being replaced by: #'{}\n",
                sym.name,
                fmt_sym(&old_cell.name),
                fmt_sym(&new_cell.name),
            ),
        );
    }
    interp.globals.bind_alias(full, new_cell.clone());
    Some(Ok(args[1].clone()))
}

/// `ns/name`-or-bare formatting for a `Symbol`, matching how a `Var`
/// prints (`#'ns/name`) -- `Symbol` itself has no `Display` impl (this
/// module's only caller that needs one), so this is a three-line local
/// helper rather than widening that type's own surface for one caller.
fn fmt_sym(s: &Symbol) -> String {
    match &s.ns {
        Some(ns) => format!("{ns}/{}", s.name),
        None => s.name.to_string(),
    }
}

pub fn register(i: &mut Interp) {
    // S6 (assert/namespace/uuid batch): `(namespace x)` -- `x` a keyword
    // or symbol (measured 1.13.0-alpha6: `(namespace :a/b)` => `"a"`,
    // `(namespace :b)` => `nil`, `(namespace 'c/d)` => `"c"`, `(namespace
    // 'd)` => `nil`); anything else throws `ClassCastException: class
    // java.lang.String cannot be cast to class clojure.lang.Named` on
    // the real JVM (real `namespace`'s body is literally `(.getNamespace
    // ^clojure.lang.Named x)`) -- reproduced here as an ordinary
    // `RjError::type_err` (message text is not conformance-scored, only
    // occurrence/coarse kind, per CONFORMANCE-GUARANTEE.md's comparison
    // rules). A `Value::Sym` already carries its `ns`/`name` split as
    // separate fields (unlike `Value::Keyword`'s flat `"ns/name"`
    // string), so the symbol arm needs no parsing at all.
    reg_unmeta(i, "namespace", ArityHint::Exact(1), |_i, args| match &args[0] {
        Value::Keyword(k) => Ok(match keyword_ns(k) {
            Some(ns) => Value::Str(ns),
            None => Value::Nil,
        }),
        Value::Sym(s) => Ok(match &s.ns {
            Some(ns) => Value::Str(ns.clone()),
            None => Value::Nil,
        }),
        other => Err(RjError::type_err(format!(
            "namespace: expected a symbol or keyword, got {}",
            other.type_name()
        ))),
    });

    // `(require spec...)` -- a plain function in real Clojure (`(defn
    // require [& args] ...)`), so ordinary evaluate-then-call semantics
    // are exactly right: `(require '[clojure.string :as s])` evaluates
    // the `quote` to a vector value before this native ever sees it, and
    // `(require ns)` (metadata.clj's `(doseq [ns public-namespaces]
    // (require ns))`) evaluates the bound local `ns` to whatever symbol it
    // holds -- both land here as an already-evaluated `Value` libspec, no
    // special-form quoting trick needed (unlike `import`).
    reg(i, "require", ArityHint::Min(1), |interp, args| {
        for spec in args {
            interp.require_spec_value(spec, NATIVE_SPAN)?;
        }
        Ok(Value::Nil)
    });

    // C3f, measured missing: `(use spec...)` as a standalone callable --
    // real Clojure's `use` is a plain function too (`(defn use [& args]
    // ...)`), same evaluate-then-call shape as `require` above. Before
    // this, `Interp::use_spec_value` was reachable only from the `(ns ...
    // (:use ...))` clause path (`eval::special_forms`); a bare top-level
    // `(use '[clojure.walk :as-alias e1])` (measured: `ns_libs.clj`'s
    // `require-as-alias`) had no handler at all ("Unable to resolve
    // symbol: use").
    reg(i, "use", ArityHint::Min(1), |interp, args| {
        for spec in args {
            // `use_spec_value` itself stays lenient about a well-SHAPED
            // spec naming a namespace it can't locate (see that fn's own
            // doc -- load-bearing for the vendored suite's many
            // `(:use clojure.test [x :exclude (...)] ...)` clauses whose
            // targets genuinely don't exist), but a spec that ISN'T even
            // a symbol or `[ns & opts]` vector/list is simply invalid,
            // same as `require_spec_value` already rejects it outright.
            // Measured: `(is (thrown? Exception (use :foo)))` --
            // `test-use` in ns_libs.clj.
            match spec {
                Value::Sym(_) | Value::Vector(_) | Value::List(_) => {}
                other => {
                    return Err(RjError::type_err(format!(
                        "use: expected a symbol or a [ns & opts] vector, got {}",
                        other.type_name()
                    )))
                }
            }
            interp.use_spec_value(spec, NATIVE_SPAN)?;
        }
        Ok(Value::Nil)
    });

    // D5: `(alter-var-root #'v f & args)` -- sets `v`'s ROOT value to
    // `(apply f <current root> args)` and returns it. Root, not
    // thread-binding: that distinction is the whole point of the fn (real
    // Clojure's own `with-redefs` is built on it), and `VarCell::store`
    // is the same root write `def` performs, so a var altered this way is
    // indistinguishable from one re-`def`d to the same value.
    //
    // The vendored `clojure.pprint` uses it for `set-pprint-dispatch` /
    // `with-pprint-dispatch`, where the point is precisely that the new
    // dispatch fn must be visible to code running on other threads too --
    // which a thread-binding would not give.
    reg(i, "alter-var-root", ArityHint::Min(2), |interp, args| {
        let Value::Var(cell) = args[0].unmeta() else {
            return Err(RjError::type_err(format!(
                "alter-var-root: expected a var, got {}",
                args[0].type_name()
            )));
        };
        let cell = cell.clone();
        let mut call_args = Vec::with_capacity(args.len() - 1);
        call_args.push(cell.get().unwrap_or(Value::Nil));
        call_args.extend_from_slice(&args[2..]);
        let next = interp.call_owned(&args[1], call_args)?;
        cell.store(next.clone(), false);
        Ok(next)
    });

    // D5: `(find-var 'ns/name)` -- the var a FULLY-QUALIFIED symbol names,
    // or nil. Unlike `resolve`, it does no alias/refer resolution and does
    // not consult the current namespace: real Clojure throws on an
    // unqualified symbol, and that check is reproduced here because
    // silently treating `foo` as `current-ns/foo` would make `find-var`
    // and `resolve` the same fn, which they are not.
    reg(i, "find-var", ArityHint::Exact(1), |interp, args| {
        let Value::Sym(sym) = args[0].unmeta() else {
            return Err(RjError::type_err(format!(
                "find-var: expected a symbol, got {}",
                args[0].type_name()
            )));
        };
        if sym.ns.is_none() {
            return Err(RjError::other(format!(
                "find-var: Symbol {} is not fully qualified",
                sym.name
            )));
        }
        // `clojure.core/x` names a var that mova interns BARE (see
        // `Interp::qualify_def` -- nothing qualifies inside `CORE_NS`),
        // so the exact-name lookup has to fall back to the bare spelling
        // for a core-namespace (or core-aliased) symbol, exactly like
        // `for_each_global_candidate`'s own trailing bare fallback does
        // for ordinary resolution. Without it `(find-var
        // 'clojure.core/map)` would be nil while `#'clojure.core/map`
        // works, which is incoherent.
        let bare = Symbol::simple(sym.name.clone());
        let target =
            if interp.globals.get_exact(sym).is_some() {
                Some(sym.clone())
            } else if interp.is_bare_or_core_alias(sym) && interp.globals.get_exact(&bare).is_some()
            {
                Some(bare)
            } else {
                None
            };
        Ok(match target {
            Some(t) => Value::Var(interp.globals.intern(&t)),
            None => Value::Nil,
        })
    });

    // D5: `(load "pprint/utilities" ...)` -- a plain function in real
    // Clojure too, so ordinary evaluate-then-call is right. All the
    // resolution/eval semantics live in `Interp::load_path`; this is only
    // the arg-shape check. Returns nil (real `load` returns the last
    // form's value, but every vendored call site is a statement).
    reg(i, "load", ArityHint::Any, |interp, args| {
        for spec in args {
            let Value::Str(path) = spec.unmeta() else {
                return Err(RjError::type_err(format!(
                    "load: expected a string path, got {}",
                    spec.type_name()
                )));
            };
            interp.load_path(path.as_ref(), NATIVE_SPAN)?;
        }
        Ok(Value::Nil)
    });

    // `(in-ns sym)` -- S7 (tail wave): a real FUNCTION, unlike `ns`
    // (special form, S4). Measured confirmed-missing (the test-helper
    // shim's `eval-in-temp-ns` macro doc documents working around its
    // absence with `(eval (list 'ns sym))`). `Interp::set_current_ns`
    // already does exactly what real `in-ns` does -- switches (creating if
    // absent) -- so this is a thin wrapper, not new namespace-switching
    // logic. Returns the switched-to namespace value (measured: real
    // `in-ns` returns the `Namespace` object, not `nil`).
    reg(i, "in-ns", ArityHint::Exact(1), |interp, args| {
        let Value::Sym(sym) = args[0].unmeta() else {
            return Err(RjError::type_err(format!(
                "in-ns: expected a symbol, got {}",
                args[0].type_name()
            )));
        };
        // field2/W-NS: `switch_ns`, same reason as the `ns` special form's
        // -- real `in-ns` is `(set! *ns* ...)`, a DYNAMIC-var write; it
        // does not recompile the body that called it.
        // A namespace made by `in-ns` has no clojure.core in it (JVM); `ns` and
        // `(refer 'clojure.core)` bring it in. Existing namespaces keep what they have.
        let is_new = !interp.ns_declared(&sym.name);
        interp.switch_ns(sym.name.clone());
        // (while core.mova itself loads, `clojure.core` is not declared yet: nothing to leave out)
        if is_new && interp.ns_declared(&Str::from(crate::ns::CORE_NS)) && sym.name.as_ref() != crate::ns::CORE_NS && sym.name.as_ref() != crate::ns::USER_NS {
            interp.set_ns_no_core(&sym.name, true);
            // the JVM resolves `in-ns` in any namespace (so `(in-ns 'a) (in-ns 'b)` works)
            interp.add_refer(Str::from("in-ns"), Str::from(crate::ns::CORE_NS));
        }
        Ok(crate::ns::ns_value(&sym.name))
    });

    // `(refer ns-sym & filters)` -- S7 (tail wave): a real FUNCTION (real
    // Clojure's `refer` is callable directly, distinct from the `(ns ...)`
    // `:use`-clause path `Interp::use_spec_value` already implements).
    // `:only`/`:exclude`/`:rename` are the filters this corpus exercises.
    // Measured error shape: `(refer ns :only '(nonexistent-var))` throws
    // "nonexistent-var does not exist"; referring a var whose meta has
    // `:private true` throws "hidden-var is not public" -- both checked
    // ONLY against the names an explicit `:only` list names (matching
    // this corpus's own two call sites; a bare `(refer ns)`/`:exclude`-
    // only refer, like `use_spec_value`'s existing behavior, does not
    // re-validate every public name it already enumerated from
    // `names_in_ns`).
    //
    // W3f (small-tail sweep): `:rename` -- `errors.clj`'s `assert-arg-
    // messages` needs `(refer 'clojure.core :rename '{with-open renamed-
    // with-open})` to make the bare symbol `renamed-with-open` resolve to
    // `clojure.core/with-open` (measured against 1.13.0-alpha6: a
    // `:rename`d var is bound under ONLY its new local name, not both --
    // this corpus never exercises the "still bound under its old name
    // too" question, so no attempt is made to guess that edge). Threaded
    // through `Interp::add_refer_as` (the general `local -> (from,
    // source_name)` form `add_refer` is now a thin wrapper around, see
    // its own doc) instead of the plain `add_refer` every other name in
    // `referred` still uses unchanged.
    reg(i, "refer", ArityHint::Min(1), |interp, args| {
        let ns_val = interp.the_ns(&args[0])?;
        let ns_name =
            crate::ns::ns_value_name(&ns_val).expect("the_ns always returns a namespace value");
        let names_of = |v: Option<&Value>| -> Vec<Str> {
            match v {
                Some(Value::Vector(ns) | Value::List(ns)) => ns
                    .iter()
                    .filter_map(|n| match n {
                        Value::Sym(s) => Some(s.name.clone()),
                        _ => None,
                    })
                    .collect(),
                _ => Vec::new(),
            }
        };
        let mut only: Option<Vec<Str>> = None;
        let mut exclude: Vec<Str> = Vec::new();
        // See `Env::interned_namespaces`'s own `#[allow(clippy::mutable_key_type)]`
        // doc (env.rs): `Str`'s cache fields are irrelevant to its `Hash`/`Eq`
        // (both derive purely from its immutable `text`), so the lint's
        // mutable-key premise doesn't apply to a `HashMap<Str, _>` here either.
        #[allow(clippy::mutable_key_type)]
        let mut rename: std::collections::HashMap<Str, Str> = std::collections::HashMap::new();
        let mut j = 1;
        while j < args.len() {
            if let Value::Keyword(k) = &args[j] {
                match k.as_ref() {
                    "only" => only = Some(names_of(args.get(j + 1))),
                    "exclude" => exclude = names_of(args.get(j + 1)),
                    "rename" => {
                        if let Some(Value::Map(m)) = args.get(j + 1) {
                            for (k, v) in m.iter() {
                                if let (Value::Sym(from), Value::Sym(to)) = (k, v) {
                                    rename.insert(from.name.clone(), to.name.clone());
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            j += 2;
        }
        // `(refer 'clojure.core)` with no filter: core is visible again
        if ns_name.as_ref() == crate::ns::CORE_NS && only.is_none() && exclude.is_empty() && rename.is_empty() {
            let cur = interp.dynamic_ns_name();
            interp.set_ns_no_core(&cur, false);
        }
        let checked = only.is_some();
        let referred: Vec<Str> = match only {
            Some(names) => names,
            None => interp
                .globals
                .names_in_ns(&ns_name)
                .into_iter()
                .filter(|n| !exclude.contains(n))
                .collect(),
        };
        for name in &referred {
            if checked {
                let sym = Symbol { ns: Some(ns_name.clone()), name: name.clone() };
                // field3 (W-DECL integration fix): EXISTENCE, not
                // boundness -- `find_any_cell`, not `find_bound_cell`.
                // Real `refer` walks `Namespace.getMappings`, which holds
                // every var some `def`/`declare`/`intern` put there
                // whether or not it has a root value. Before W-DECL a
                // 1-arg `(def x)` bound `nil`, so the two were the same
                // thing here; now that it interns a genuinely UNBOUND var
                // (matching the JVM), a boundness gate reported real vars
                // as absent. Measured against 1.13.0-alpha6
                // (compat/w-decl-fix-ns-machinery-oracle-transcript.txt):
                //
                //   (binding [*ns* tns] (eval '(def ^{:private true} hidden-var))
                //                       (eval '(def unbound-pub)))
                //   (refer 'oracle.tmp :only '(hidden-var))     ; IllegalAccessError
                //                                              ;   "hidden-var is not public"
                //   (refer 'oracle.tmp :only '(unbound-pub))    ; => nil, referred fine
                //   (refer 'oracle.tmp :only '(nonexistent-var)); IllegalAccessError
                //                                              ;   "... does not exist"
                //
                // i.e. an unbound var is REFERRABLE, and privacy is what
                // decides -- exactly what vendored `ns_libs.clj`'s
                // `refer-error-messages` asserts.
                //
                // Sibling surfaces, from the same transcript: `ns-interns`
                // and `ns-map` list an unbound interned var, `ns-publics`
                // lists it unless private, and `ns-resolve` returns it.
                // mova's `Env::names_in_ns` (which backs all four, plus
                // `:refer :all`/`dir-fn`/`apropos`) is still boundness-
                // gated and therefore still omits them. Deliberately NOT
                // changed here: no test in this corpus observes it, and
                // that gate is also what keeps mova's compiler-speculative
                // candidate placeholders (`VarCell::speculative`) out of
                // every ns listing. Recorded as a known, oracle-measured
                // divergence rather than fixed blind.
                match interp.globals.find_any_cell(&sym) {
                    None => {
                        // W3a: measured -- real `clojure.core/refer` throws
                        // `java.lang.IllegalAccessError` for both of these,
                        // which is an `Error`, NOT an `Exception` (real
                        // ancestry: `IllegalAccessError <:
                        // IncompatibleClassChangeError <: LinkageError <:
                        // Error <: Throwable`), so a `(catch Exception e ..)`
                        // correctly does not catch it. ns_libs.clj's
                        // `refer-error-messages` asserts both by class.
                        return Err(RjError::other(format!("{name} does not exist"))
                            .with_class(JvmClass::IllegalAccess));
                    }
                    Some(cell) => {
                        // f4/ns: shared with `Interp::private_var_violation`'s
                        // identical check -- see `VarCell::is_private`'s doc.
                        if cell.is_private() {
                            return Err(RjError::other(format!("{name} is not public"))
                                .with_class(JvmClass::IllegalAccess));
                        }
                    }
                }
            }
            match rename.get(name) {
                Some(local) => interp.add_refer_as(local.clone(), ns_name.clone(), name.clone()),
                None => interp.add_refer(name.clone(), ns_name.clone()),
            }
        }
        Ok(Value::Nil)
    });

    // `(intern ns sym)` / `(intern ns sym val)` -- S7 (tail wave), measured
    // (rt.clj's `ns-intern-policies`): interns `sym` in `ns` (creating the
    // var if absent), optionally sets its root to `val`, and returns the
    // `Var`. Prints a `"WARNING: ..."` line (through whatever `*err*`
    // currently resolves in `ns` -- the shim's own dynamic `*err*`/
    // `with-err-string-writer` writer, see `write_shim_err`'s doc) when
    // `sym` already resolved, in `ns`, to a var interned SOMEWHERE ELSE
    // (typically the `clojure.core` fallback every namespace sees) --
    // matches `def`'s own core-shadowing warning, checked BEFORE the
    // intern (a name already interned in `ns` itself resolves to ITS OWN
    // cell either way, so re-`intern`ing it is silent, same as real
    // Clojure).
    reg(i, "intern", ArityHint::Range(2, 3), |interp, args| {
        let ns_val = interp.the_ns(&args[0])?;
        let ns_name =
            crate::ns::ns_value_name(&ns_val).expect("the_ns always returns a namespace value");
        let Value::Sym(sym) = args[1].unmeta() else {
            return Err(RjError::type_err(format!(
                "intern: expected a symbol, got {}",
                args[1].type_name()
            )));
        };
        let target = Symbol { ns: Some(ns_name.clone()), name: sym.name.clone() };
        // SPEC-W3: the same `(:refer-clojure :exclude [...])` suppression
        // `def`'s own warning applies. Both routes are `Namespace.intern`
        // -> `checkReplacement` on the JVM, so they cannot differ here
        // either. See `Interp::refer_clojure_excludes`.
        if interp.globals.find_bound_cell(&target).is_none()
            && !interp.refer_clojure_excluded(&ns_name, &sym.name)
        {
            let prior = interp.ns_resolve_in(&args[0], &Symbol::simple(sym.name.clone()))?;
            // W3e-4: the message text (and the "bare cell means
            // clojure.core" spelling, which this used to render as the
            // namespace-less `#'prefers`) now comes from the ONE place
            // `def`'s identical warning comes from.
            if let Some(w) = Interp::shadow_warning(&ns_name, &sym.name, prior.as_ref()) {
                write_shim_err(interp, &w);
            }
        }
        let cell = interp.globals.intern(&target);
        if let Some(val) = args.get(2) {
            cell.store(val.clone(), false);
        }
        Ok(Value::Var(cell))
    });

    // `(ns-interns ns)` -- f4/ns (W-DECL integration fix, oracle transcript
    // `compat/w-decl-fix-ns-machinery-oracle-transcript.txt`): `{sym #'ns/sym
    // ...}` for every var interned DIRECTLY in `ns`, as a GENUINE namespace
    // mapping -- bound or not, but never a compiler-speculative resolution
    // placeholder (`Env::find_any_cell`'s gate, see its doc: a `def`/
    // `declare`/`intern`/`refer`-minted cell answers whether or not it has
    // a root value; a cell the compiler only ever PROBED for while
    // resolving some other candidate does not). Measured: `(ns-interns
    // tns)` lists BOTH a `^:private` unbound var and a plain unbound var --
    // unlike `ns-publics` below, `ns-interns` does NOT narrow by privacy at
    // all (real `Namespace.getMappings`' interned half hides nothing;
    // narrowing to public is `ns-publics`'s own, separate, contract).
    //
    // C3h (clojure.repl surface): the cell lookup uses `crate::ns::
    // var_symbol_in`, NOT a bare `Symbol{ns: Some(ns_name), ..}`, because
    // `names_in_ns` now also hands back every bare (`ns: None`) builtin
    // when `ns_name` is `clojure.core` (see that fn's doc) -- those cells
    // are interned BARE, so re-qualifying the name as `clojure.core/name`
    // before the lookup would miss every one of them (the same
    // `ns-publics`/`apropos`/`dir-fn`-on-`clojure.core` bug `ns-publics`'s
    // own history already measured).
    reg(i, "ns-interns", ArityHint::Exact(1), |interp, args| {
        let ns_val = interp.the_ns(&args[0])?;
        let ns_name =
            crate::ns::ns_value_name(&ns_val).expect("the_ns always returns a namespace value");
        let mut out = crate::value::PMap::new();
        for name in interp.globals.names_in_ns(&ns_name) {
            let sym = crate::ns::var_symbol_in(&ns_name, &name);
            if let Some(cell) = interp.globals.find_any_cell(&sym) {
                out.insert(Value::Sym(Symbol::simple(name)), Value::Var(cell));
            }
        }
        Ok(Value::Map(out))
    });

    // `(ns-publics ns)` -- S7 (tail wave); f4/ns (W-DECL integration fix)
    // widened the gate the same way `ns-interns` above did (GENUINE
    // mapping, not boundness -- see that fn's doc immediately above), and
    // ADDED the privacy narrowing `ns-interns` deliberately skips: a
    // `^{:private true}` var is filtered out here (`VarCell::is_private`,
    // shared with `refer`'s and `Interp::private_var_violation`'s identical
    // check). Measured: oracle `(ns-publics tns)` on the SAME two vars
    // `ns-interns` lists both of drops the private one, keeping only the
    // public-but-unbound one -- exactly `unbound-pub` present, `hidden-var`
    // absent.
    //
    // C3h (clojure.repl surface): see `ns-interns`'s identical note on
    // `var_symbol_in` vs. a bare `Symbol{ns: Some(ns_name), ..}` -- same
    // reason, same `clojure.core` case.
    reg(i, "ns-publics", ArityHint::Exact(1), |interp, args| {
        let ns_val = interp.the_ns(&args[0])?;
        let ns_name =
            crate::ns::ns_value_name(&ns_val).expect("the_ns always returns a namespace value");
        let mut out = crate::value::PMap::new();
        for name in interp.globals.names_in_ns(&ns_name) {
            let sym = crate::ns::var_symbol_in(&ns_name, &name);
            if let Some(cell) = interp.globals.find_any_cell(&sym) {
                if !cell.is_private() {
                    out.insert(Value::Sym(Symbol::simple(name)), Value::Var(cell));
                }
            }
        }
        Ok(Value::Map(out))
    });

    // `(ns-map ns)` -- f4/ns (W-DECL integration fix): `{sym #'ns/sym ...}`
    // (or `{sym (the-ns other-ns) ...}` for an alias -- not modeled here,
    // see below) for EVERY mapping `ns` currently has, real `Namespace.
    // getMappings`'s three parts:
    //
    //   1. vars interned DIRECTLY in `ns` -- identical loop and gate to
    //      `ns-interns` above (genuine mapping, no privacy narrowing:
    //      `getMappings` includes private vars too, privacy is a
    //      RESOLUTION-time gate on the JVM, never a listing-time one).
    //   2. vars `:refer`red (or bare `refer`red) INTO `ns` from elsewhere --
    //      `Interp::ns_refers_of`'s table, resolved through the SAME
    //      genuine-mapping gate on the SOURCE cell, so a referred-but-
    //      unbound var (measured: `(refer 'oracle.tmp :only '(unbound-pub))`
    //      succeeds, see the `refer` builtin's own doc above) shows up here
    //      too.
    //   3. imported classes -- mova has no per-namespace import table yet
    //      (`NsInfo` tracks `aliases`/`refers` only, see that struct's
    //      doc); `(import ...)`'s effect on `ns-map` is NOT modeled here.
    //      No test in this corpus and no line of the oracle transcript
    //      observes this half specifically -- flagged as a known,
    //      oracle-measured residue rather than fixed blind (matches the
    //      spirit of `refer`'s and `use_spec_value`'s own documented gaps).
    //
    // Also missing: `ns`'s own `:as`/`:as-alias` ALIASES do not appear as
    // `ns-map` entries on the real JVM either (an alias is a namespace-level
    // `Symbol -> Namespace` mapping tracked separately from `getMappings`),
    // so that omission is actually CORRECT, not a gap.
    //
    // Measured: `(contains? (ns-map tns) 'hidden-var)` is `true` -- part 1
    // above (a directly-interned private-and-unbound var) already covers
    // it; no refer/import case appears in the transcript.
    reg(i, "ns-map", ArityHint::Exact(1), |interp, args| {
        let ns_val = interp.the_ns(&args[0])?;
        let ns_name =
            crate::ns::ns_value_name(&ns_val).expect("the_ns always returns a namespace value");
        let mut out = crate::value::PMap::new();
        for name in interp.globals.names_in_ns(&ns_name) {
            let sym = crate::ns::var_symbol_in(&ns_name, &name);
            if let Some(cell) = interp.globals.find_any_cell(&sym) {
                out.insert(Value::Sym(Symbol::simple(name)), Value::Var(cell));
            }
        }
        for (local, (from, source_name)) in interp.ns_refers_of(&ns_name) {
            let source_sym = crate::ns::var_symbol_in(&from, &source_name);
            if let Some(cell) = interp.globals.find_any_cell(&source_sym) {
                out.insert(Value::Sym(Symbol::simple(local)), Value::Var(cell));
            }
        }
        Ok(Value::Map(out))
    });

    // `(ns-name x)` -- `x` a namespace value or a symbol naming one
    // (measured: both `(ns-name *ns*)` and `(ns-name 'user)` work,
    // `ns-name`'s real implementation is literally `(.name (the-ns ns))`).
    reg(i, "ns-name", ArityHint::Exact(1), |interp, args| {
        let ns = interp.the_ns(&args[0])?;
        let name = crate::ns::ns_value_name(&ns).expect("the_ns always returns a namespace value");
        Ok(Value::Sym(crate::value::Symbol::simple(name)))
    });

    // `(find-ns sym)` -- `nil` (not an error) when unresolvable (measured).
    reg(i, "find-ns", ArityHint::Exact(1), |interp, args| {
        Ok(interp.find_ns_value(&args[0]).unwrap_or(Value::Nil))
    });

    // `(create-ns sym)` -- creates (if absent) and returns the namespace,
    // WITHOUT changing `*ns*` (unlike `in-ns`/`ns`). Was unresolved.
    reg(i, "create-ns", ArityHint::Exact(1), |interp, args| interp.create_ns(&args[0]));

    // `(the-ns x)` -- `x` returned as-is if already a namespace value,
    // else the measured "No namespace: NAME found" error when `x` names
    // one that isn't loaded.
    reg(i, "the-ns", ArityHint::Exact(1), |interp, args| interp.the_ns(&args[0]));

    // `(all-ns)` -- see `Interp::all_ns_values`'s doc for the `LazySeq`
    // vs. plain-`List` shape deviation (not corpus-observable: no live
    // form asserts the container TYPE, only count/contents).
    reg(i, "all-ns", ArityHint::Exact(0), |interp, _args| {
        Ok(Value::List(interp.all_ns_values().into_iter().collect()))
    });

    // `(ns-resolve ns sym)` / `(ns-resolve ns env sym)` -- S7 (tail wave)
    // added the 3-arity form: `env` is a macro-expansion-time LOCAL
    // BINDINGS map (`{sym-in-scope value-or-placeholder ...}`); real
    // Clojure's own implementation is literally `(when-not (contains? env
    // sym) (ns-resolve ns sym))` -- if `sym` is a KEY in `env`, it names a
    // local, and there is no Var to resolve it to, so the answer is `nil`
    // regardless of whether a global of that name also exists (measured:
    // `(ns-resolve 'clojure.core {'first :local-first} 'first)` => `nil`,
    // even though `clojure.core/first` obviously exists) -- `env`'s VALUES
    // are never consulted, only its key set. `mova` has no compile-time
    // macro environment of its own to thread through automatically (same
    // scope note the old 2-arity-only doc made), but callers that already
    // HAVE such a map in hand (this is exactly what `ns_libs.clj`'s
    // `resolution` deftest does, by hand) can still pass it.
    reg(i, "ns-resolve", ArityHint::Range(2, 3), |interp, args| {
        let (ns_arg, env_arg, sym_arg) = if args.len() == 3 {
            (&args[0], Some(&args[1]), &args[2])
        } else {
            (&args[0], None, &args[1])
        };
        // S5/M3: `^:foo`-carrying symbols reach here through macros
        // (`defonce`/`declare` do exactly this), so look through.
        let Value::Sym(sym) = sym_arg.unmeta() else {
            return Err(RjError::type_err(format!(
                "ns-resolve: expected a symbol, got {}",
                sym_arg.type_name()
            )));
        };
        if let Some(Value::Map(env)) = env_arg {
            if env.contains_key(&Value::Sym(sym.clone())) {
                return Ok(Value::Nil);
            }
        }
        let sym = sym.clone();
        match interp.ns_resolve_in(ns_arg, &sym)? {
            Some(cell) => Ok(Value::Var(cell)),
            None => Ok(Value::Nil),
        }
    });

    // `(ns-aliases ns)` -- S7 (tail wave): `{alias-sym namespace-value
    // ...}` for every `:as`/`:as-alias` alias the-ns has recorded (both
    // land in the SAME table -- see `Interp::add_alias`'s doc and
    // `require_spec_value`'s `:as-alias` arm; real Clojure's own
    // `ns-aliases` doesn't distinguish them either, both show up in
    // `(ns-aliases *ns*)`).
    reg(i, "ns-aliases", ArityHint::Exact(1), |interp, args| {
        let ns = interp.the_ns(&args[0])?;
        let name = crate::ns::ns_value_name(&ns).expect("the_ns always returns a namespace value");
        let mut out = crate::value::PMap::new();
        for (alias, full) in interp.ns_aliases_of(&name) {
            out.insert(
                Value::Sym(crate::value::Symbol::simple(alias)),
                crate::ns::ns_value(&full),
            );
        }
        Ok(Value::Map(out))
    });

    // W4B-MESSAGES (ns_libs.clj's `test-alias`): `(alias alias-sym
    // ns-sym)` records `alias-sym` as an alias for `ns-sym` IN THE
    // CURRENT namespace (`require`'s own `:as` clause already builds on
    // this exact primitive -- see `Interp::add_alias`'s doc; this is the
    // first time it's reachable as the bare `clojure.core/alias` fn
    // itself). `ns-sym` must already name a LOADED namespace -- reusing
    // `the_ns` here (not a bespoke check) means the missing-namespace
    // error is the SAME measured "No namespace: NAME found" shape
    // `the-ns`/`ns-resolve` already give, not a fabricated one (measured
    // against the oracle: `(alias 'bogus 'epicfail)` => `java.lang.
    // Exception: "No namespace: epicfail found"`, transcript in
    // compat/w4b-alias-oracle-transcript.txt). Real Clojure returns `nil`.
    reg(i, "alias", ArityHint::Exact(2), |interp, args| {
        let Value::Sym(alias_sym) = &args[0] else {
            return Err(RjError::type_err(format!(
                "alias: expected a symbol, got {}",
                args[0].type_name()
            )));
        };
        let alias_name = alias_sym.name.clone();
        let target = interp.the_ns(&args[1])?;
        let target_name =
            crate::ns::ns_value_name(&target).expect("the_ns always returns a namespace value");
        interp.add_alias(alias_name, target_name);
        Ok(Value::Nil)
    });

    // `(resolve sym)` -- 1-arity only (see `ns-resolve`'s doc for the
    // omitted macro-environment 2-arity); measured: resolves `sym` in the
    // CURRENT namespace, `nil` (not an error) when it doesn't resolve.
    //
    // SPEC-W3: "current" is the DYNAMIC `*ns*`, because
    // `clojure.core/resolve` is LITERALLY `(defn resolve [sym] (ns-resolve
    // *ns* sym))` on the JVM -- a runtime function reading a runtime var,
    // not a compile-time lookup. mova used the LEXICAL `current_ns`
    // instead, i.e. the DEFINING namespace of whatever fn happened to be
    // running, and the two differ exactly when a fn body runs with `*ns*`
    // pointing elsewhere.
    //
    // Two measured victims:
    //   * `clojure.spec.alpha`'s `res` (`(-> form resolve ->sym)`) saw
    //     spec's OWN namespace instead of the caller's, so a user's alias
    //     did not resolve and `(s/form (s/or :n (s/and int? even?)))`
    //     reported `clojure.core/and` where the oracle says
    //     `clojure.spec.alpha/and`. The port worked around it as MOVA-PATCH
    //     P10, now reverted to upstream.
    //   * vendored `ns_libs.clj`'s `require-as-alias`: the suite harness
    //     runs `(run-tests)` with `*ns*` deliberately set to `user` (as
    //     `Compiler.load` leaves it), a `require :as` inside a deftest body
    //     therefore adds its alias to `user` (`add_alias` is dynamic, and
    //     is right to be -- `clojure.core/alias` is `(.addAlias *ns* ..)`),
    //     and `(resolve 'n1/union)` then had to read that same table to
    //     find it. Reading it lexically fell through to bare `union` and
    //     answered `#'clojure.core/union`.
    //
    // `ns-resolve` is unaffected: it is handed the namespace to look in.
    // clojure-lsp campaign (mova/PLAN.md): real `clojure.core/resolve` also
    // has a 2-arg `([env sym] ...)` form (macros pass `&env`, which this
    // ignores exactly like the 1-arg form -- `env` only matters for a
    // LOCAL binding, and `resolve` never returns those on the real JVM
    // either, only a Var). `taoensso.encore`'s `var-info` macro helper
    // calls it this way.
    reg_unmeta(i, "resolve", ArityHint::Range(1, 2), |interp, args| {
        let sym_arg = if args.len() == 2 { &args[1] } else { &args[0] };
        let Value::Sym(sym) = sym_arg else {
            return Err(RjError::type_err(format!(
                "resolve: expected a symbol, got {}",
                sym_arg.type_name()
            )));
        };
        match interp.resolve_var_cell_in_dynamic_ns(sym) {
            Some(cell) => Ok(Value::Var(cell)),
            None => Ok(Value::Nil),
        }
    });

    // `(mova.internal/with-load-hook* load-fn thunk)` -- pushes `load-fn`
    // (called by `require_ns` on a not-found namespace, see `try_load_hook`
    // above) for the dynamic extent of `(thunk)`, always popping it again
    // (even on error) so a failing eval never leaves a stale hook active.
    // This is the sci.core shim's `:load-fn` primitive (mova/PLAN.md's sci
    // shim wave): sci's own `:load-fn` semantics are exactly "consulted
    // when required code needs a namespace the embedder didn't provide".
    reg(i, "with-load-hook*", ArityHint::Exact(2), |interp, args| {
        LOAD_HOOKS.with(|h| h.borrow_mut().push(args[0].clone()));
        let result = interp.call(&args[1], &[]);
        LOAD_HOOKS.with(|h| {
            h.borrow_mut().pop();
        });
        result
    });
}

#[cfg(test)]
mod load_hook_tests {
    use super::Interp;

    fn eval_ok(src: &str) -> crate::value::Value {
        let mut interp = Interp::new();
        interp.eval_str("test", src).unwrap_or_else(|e| panic!("eval error for {src:?}: {e:?}"))
    }

    // sci-shim/load-hook: `with-load-hook*` supplies source for a
    // `require` that would otherwise fail (no file on disk, nothing
    // embedded) -- the exact shape the sci.core shim's `:load-fn` needs.
    #[test]
    fn with_load_hook_supplies_missing_namespace_source() {
        let v = eval_ok(
            r#"
            (with-load-hook*
              (fn [{:keys [namespace]}]
                (when (= namespace 'totally.fake.ns)
                  {:file "totally/fake/ns.clj" :source "(ns totally.fake.ns) (defn greet [] :hi)"}))
              (fn []
                (require 'totally.fake.ns)
                ((resolve 'totally.fake.ns/greet))))
            "#,
        );
        assert_eq!(v, crate::value::Value::Keyword("hi".into()));
    }

    // The hook is popped even when the thunk throws, and a namespace the
    // hook declines (returns nil for) still surfaces the ordinary
    // not-found error rather than hanging or panicking.
    #[test]
    fn with_load_hook_pops_on_error_and_can_decline() {
        let v = eval_ok(
            r#"
            (try
              (with-load-hook*
                (fn [_] nil)
                (fn [] (require 'still.totally.fake)))
              (catch e :not-found))
            "#,
        );
        assert_eq!(v, crate::value::Value::Keyword("not-found".into()));
    }
}
