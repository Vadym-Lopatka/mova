//! `read-string` and `eval` (v0.5 / R2): the two natives `init.edn` loading
//! and an in-process nREPL need to turn text into data and data into a
//! running value, without the interpreter process ever dying on either
//! step -- both a malformed `read-string` argument and a failing `eval`
//! form surface as ordinary catchable `RjError`s, never a panic.
//!
//! S6 also puts `cast` here: a COMPAT VENEER over the vendored suite's
//! Java-interop idiom, not a step toward JVM-style reflection. mova's
//! real type system is Rust-native; `cast` rides the EXISTING builtin
//! class table (`src/types.rs::builtin_classes()`) purely as a
//! name-level alias resolver, going no deeper than
//! `tests/clojure-suite/vendor/numbers.clj`'s own `cast` call sites
//! measurably demand -- see `cast_native`'s own doc.

use crate::builtins::{reg, ArityHint};
use crate::error::RjError;
use crate::eval::Interp;
use crate::reader::{Form, Span};
use crate::types::ClassVal;
use crate::value::{NativeFn, Symbol, Value};
use std::sync::Arc;

/// clojure-lsp campaign cleanup: `edn-fast-read-string*` was a bare
/// global (this file's only such native); every other fast-path native
/// (`mova.io`/`mova.json`/`mova.transit`) registers under a QUALIFIED
/// `mova.<ns>/name` symbol instead, same `reg_ns` convention as those
/// modules' own (each a private near-duplicate of this one, by design --
/// see `builtins::transit`'s copy of this same fn for why it isn't
/// factored out further).
#[track_caller]
fn reg_ns(
    i: &mut Interp,
    ns: &'static str,
    name: &'static str,
    arity: ArityHint,
    f: impl Fn(&mut Interp, &[Value]) -> Result<Value, RjError> + Send + Sync + 'static,
) {
    let native = NativeFn::new(name, move |interp: &mut Interp, args: &[Value]| {
        if !arity.matches(args.len()) {
            return Err(RjError::arity(format!("{ns}/{name}: wrong number of args ({})", args.len()))
                .with_stack(interp.stack_snapshot(), interp.source_id));
        }
        f(interp, args)
    });
    i.globals.set_builtin(Symbol { ns: Some(ns.into()), name: name.into() }, Value::Native(Arc::new(native)));
}

pub fn register(i: &mut Interp) {
    reg(i, "read-string", ArityHint::Range(1, 2), read_string);
    // mova/PERF-LSP.md task 1: `clojure.edn/read-string`'s native fast
    // path -- exposes `edn_fast::try_read_edn` to `edn.mova` as a 2-elem
    // `[ok? value]` pair (never a bare `nil`, which would be ambiguous
    // with a genuine `nil` EDN literal) so the shim can fall back to the
    // interpreted `clojure.tools.reader.edn` path on bail (any construct
    // outside edn_fast's supported subset: tagged literals, `#_`, etc.)
    // without ever guessing.
    reg_ns(i, "mova.edn", "edn-fast-read-string*", ArityHint::Exact(1), edn_fast_read_string);
    // Track M1: like `read-string`, but a top-level map stays lazy (`Value::LazyMap`).
    reg_ns(i, "mova.edn", "read-string-lazy", ArityHint::Exact(1), read_string_lazy);
    reg(i, "eval", ArityHint::Exact(1), eval_native);
    reg(i, "load-string", ArityHint::Range(1, 2), load_string_native);
    reg(i, "load-file", ArityHint::Exact(1), load_file_native);
    reg(i, "cast", ArityHint::Exact(2), cast_native);
    // C3h (clojure.repl surface): `source-fn` needs real file I/O
    // (`std::fs::read_to_string`) and a from-arbitrary-offset reader
    // (`reader::read_one`), same toolbox `read-string` already uses --
    // see `source_fn_native`'s own doc for the full algorithm. `source`
    // itself (the printing macro real Clojure defines over `source-fn`)
    // is a plain `core/core.mova` macro, not a native -- it's just
    // `(println (or (source-fn 'n) "Source not found"))`, nothing this
    // fn needs to help with.
    reg(i, "source-fn", ArityHint::Exact(1), source_fn_native);
    // SPEC-W5: `clojure.spec.test.alpha`'s caller introspection -- see
    // `callstack_native`'s own doc for the shape and for why it is a
    // dedicated native rather than a real `(.getStackTrace
    // (Thread/currentThread))`.
    reg(i, "callstack*", ArityHint::Exact(0), callstack_native);
}

/// `(callstack*)` -- the LIVE mova call stack, innermost frame first, in
/// the exact shape `clojure.core/StackTraceElement->vec` produces on the
/// JVM: a vector of `[class-sym method-sym file-string line-int]`.
///
/// # Why this exists
///
/// `clojure.spec.test.alpha/instrument` is contractually required to say
/// WHO made a non-conforming call: the exception it throws carries
/// `::stest/caller {:var-scope 'calling-ns/calling-fn, :file .., :line ..}`,
/// and the official suite (`clojure/test_clojure/instr.clj`) asserts that
/// `:var-scope` is the var symbol of the plain `defn`'d helper that called
/// the instrumented fn -- skipping spec's own plumbing frames. Upstream
/// gets that by walking `(.getStackTrace (Thread/currentThread))` and
/// demunging JVM class names back into `ns$fn` pairs.
///
/// # Why NOT `(.getStackTrace (Thread/currentThread))`
///
/// That method already exists here (`hostclass::call_thread_method`) and
/// deliberately answers an ALWAYS-EMPTY array, which is load-bearing:
/// `tests/clojure-suite`'s `clojure.test` shim runs vendored
/// `file-and-line*`/`test-context-stacktrace` over it and depends on both
/// taking their empty-stack branch (`shim-selftest.mova` asserts exactly
/// that, and `SHIM-LIMITS.md` documents the degradation). Making it return
/// real frames would silently re-route every `clojure.test` report map in
/// the census through a code path this build has never scored. So the new
/// capability gets its own name and the old stub keeps its meaning --
/// the same `*`-suffixed "runtime half of a core form" convention
/// `locking*`/`delay*` already use.
///
/// # Shape
///
/// * `class` is `<defining-ns>$<fn-name>`, UNMUNGED. mova fn names are not
///   munged in the first place (the same fact that made `clojure.spec.
///   alpha`'s port drop `Compiler/demunge` -- see `MOVA-PATCH P3`), so the
///   port's `demunge` step becomes `identity` rather than a munging table
///   invented purely to be undone. `<ns>$<name>` (rather than
///   `<ns>/<name>`) is deliberate: it is what upstream's own
///   `interpret-stack-trace-element` splits on, so that fn stays verbatim.
/// * `method` is always `invoke`, which is what makes upstream's
///   `(contains? '#{invoke invokeStatic} method)` "is this a Clojure
///   frame?" test true. Every frame here IS a mova fn call; mova has no
///   host frames to distinguish.
/// * An anonymous fn is named `anonymous-fn` -- the same name it already
///   carries in a rendered mova stack trace (`Interp::anon_frame_name`).
///   mova records no enclosing-fn chain, so there is never a third
///   `$`-segment and therefore never a `:local-fn`; the port compensates
///   in its `plumbing?` predicate (ledgered).
///
/// # ONE shift: a frame is described by the frame just inside it
///
/// Both facts this needs about frame F -- the namespace F ran in and the
/// line F was executing -- are recorded by the frame F called.
///
/// * NAMESPACE. `Interp::ns_stack` is index-parallel to `Interp::stack` and
///   holds, for each live frame, the namespace that was CURRENT when it was
///   entered -- i.e. its caller's; see that field's doc for why it is
///   stored that way, and why it is a separate `Vec` rather than a third
///   `Frame` field. So F's own namespace is `ns_stack[F + 1]`, and the
///   INNERMOST frame's is `Interp::current_ns` itself.
/// * LINE. `Frame::span` is a frame's CALL SITE, a position inside its
///   caller's body. A JVM `StackTraceElement` instead reports the line
///   inside the frame it names. Same shift: F's line is `stack[F+1].span`.
///   The innermost frame has no frame inside it and a native is not handed
///   its own call span, so element 0 falls back to that frame's own
///   call-site span. Documented rather than hidden: for the one consumer
///   this exists for, element 0 is always spec's own `conform!` closure and
///   is always dropped by `stacktrace-relevant-to-instrument`'s plumbing
///   filter before anything reads its `:file`/`:line`.
///
/// Spans are resolved against `Interp::source`/`source_name` -- the buffer
/// currently being evaluated -- exactly as `error::render` already resolves
/// the frames of a raised error (`Span` carries no source id; see
/// `source_registry`'s module doc for why). A frame from another buffer
/// therefore gets a best-effort line, which again only ever affects
/// plumbing frames in practice. Note also that mova's macro expander
/// rebuilds a macro's output carrying the macro CALL form's span, so a call
/// inside a `defn` body reports the `defn`'s own line -- a pre-existing
/// property of every stack trace mova renders, not of this fn.
fn callstack_native(interp: &mut Interp, _args: &[Value]) -> Result<Value, RjError> {
    let source_name = interp.source_name.clone();
    let source = interp.source.clone();
    // K1: live fast-call frames merged back in (`Interp::merged_stack`).
    let merged = interp.merged_stack();
    let n = merged.len();
    let mut out = crate::value::PVec::new();
    for k in 0..n {
        let frame = &merged[n - 1 - k].0;
        // k == 0 is the innermost frame: no frame inward of it, so its own
        // namespace is the live `current_ns` and its line falls back to its
        // own call site (see this fn's doc).
        let (ns, span) = if k == 0 {
            (&interp.current_ns, frame.span)
        } else {
            (merged[n - k].1, merged[n - k].0.span)
        };
        let (line, _col) = crate::error::line_col(&source, span.start);
        let elem = crate::value::PVec::from_slice(&[
            Value::Sym(Symbol::simple(format!("{}${}", ns, frame.name))),
            Value::Sym(Symbol::simple("invoke")),
            Value::Str(source_name.clone()),
            Value::Int(line as i64),
        ]);
        out.push_back(Value::Vector(elem));
    }
    Ok(Value::Vector(out))
}

/// `(cast c x)`: measured against `.oracle` (this task's scratchpad
/// probe) rather than assumed from `Class.cast`'s javadoc. `nil` casts
/// to ANY class without checking membership (measured: `(cast String
/// nil)` would succeed just like `(cast Long nil)` => `nil` -- real
/// `Class.cast(null)` never calls `isInstance`), and `c` itself must be
/// a `Value::Class` (measured: `(cast nil 3)` throws, real Clojure's
/// NPE on calling `.cast` through a null class reference). Otherwise:
/// `(instance? c x)` true means `x` unchanged; false means the JVM's
/// exact `ClassCastException` message SHAPE, `"Cannot cast <actual> to
/// <target>"` (message TEXT is out of conformance scope per
/// `CONFORMANCE-GUARANTEE.md`'s comparison rules -- only occurrence/
/// coarse kind is ever compared -- but the shape is cheap to match and
/// was oracle-measured anyway: `(cast String 3)` =>
/// `"Cannot cast java.lang.Long to java.lang.String"`).
///
/// The membership check duplicates `builtins::types::install`'s
/// `instance?` closure (that fn is private to its own module and not
/// reachable from here -- `src/builtins/types.rs` is outside this
/// task's file ownership) rather than factoring out a shared helper;
/// see `is_instance` below for the one-to-one mapping between the two.
/// `x` is looked through `unmeta()` first, matching `instance?`'s own
/// "metadata never changes a value's class" rule.
fn cast_native(_interp: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    let Value::Class(c) = &args[0] else {
        return Err(RjError::type_err(format!("cast: expected a class, got {}", args[0].type_name())));
    };
    if matches!(&args[1], Value::Nil) {
        return Ok(Value::Nil);
    }
    let subject = args[1].unmeta();
    if is_instance(c.as_ref(), subject) {
        return Ok(args[1].clone());
    }
    Err(RjError::type_err(format!("Cannot cast {} to {}", actual_class_name(subject), c.name())))
}

/// `(instance? c x)`'s membership test, restated here (see `cast_native`'s
/// doc for why this isn't shared with `builtins::types::install`'s
/// private closure that implements the SAME rule for the `instance?`
/// builtin itself) -- keep the two in sync by hand if either changes.
fn is_instance(c: &ClassVal, x: &Value) -> bool {
    match c {
        ClassVal::Builtin { pred: Some(p), .. } => p(x),
        ClassVal::Builtin { pred: None, name } => crate::types::builtin_class_name(x) == *name && !matches!(x, Value::Nil),
        ClassVal::User(t) => matches!(x, Value::Inst(inst) if std::sync::Arc::ptr_eq(&inst.tdef, t)),
        ClassVal::Interface { name } => crate::builtins::types::implements_interface(x, name),
    }
}

/// The class NAME `cast`'s error message names as "actual" -- the same
/// data `(class x)` would report (`builtins::types::class_of`'s own
/// naming, restated locally: a record/deftype instance's `TypeDef` name,
/// else `types::builtin_class_name`), without needing that module's
/// interned `Class` VALUE (only the name string is needed for the
/// message).
fn actual_class_name(x: &Value) -> std::borrow::Cow<'static, str> {
    match x {
        Value::Inst(inst) => std::borrow::Cow::Owned(inst.tdef.name.to_string()),
        other => std::borrow::Cow::Borrowed(crate::types::builtin_class_name(other)),
    }
}

/// `(read-string s)` / `(read-string opts s)`: reads exactly the FIRST
/// form out of `s` (Clojure semantics -- trailing text after that form is
/// ignored, not an error), via `reader::read_one_with_ns`/
/// `read_one_allow_cond_with_ns` rather than `read_all` (which demands
/// every top-level form in the string parse) -- the `_with_ns` variants
/// also resolve `::kw`/`::alias/kw` (C3d) against `interp`'s live `*ns*`
/// (`Interp::reader_ns_context`), matching real Clojure's own
/// `read-string`, which reads against the CALLER's current namespace, not
/// a fixed default. A blank/empty string reads as `nil`, matching
/// `read_one_with_ns`'s own "true EOF" case. A malformed form is a normal
/// `Err(RjError::Reader)`, which propagates through `native.f`'s existing
/// span/stack wiring (`apply.rs::apply_value`) exactly like any other
/// builtin's error -- so `(try (read-string "(") (catch e ...))` catches
/// it instead of the process dying.
///
/// The 2-arity `opts` map is ONLY inspected for `:read-cond` (S5 / reader
/// conditionals): `{:read-cond :allow}` turns on `#?`/`#?@` dispatch for
/// this one call, matching the JVM's own `read-string` option (measured:
/// `(read-string "#?(:clj 1)")` throws "Conditional read not allowed",
/// `(read-string {:read-cond :allow} "#?(:clj 1)")` => `1`). Every other
/// key real Clojure's `read-string` accepts (`:eof`, `:features`) is
/// unimplemented here -- `:features` in particular measured as a NO-OP
/// even on the real JVM reader (the active feature set is hardcoded to
/// `{:clj :default}` there, not configurable via `read-string`'s opts
/// map), so there is nothing to wire up for it.
/// `(edn-fast-read-string* s)`: `clojure.edn/read-string`'s native fast
/// path (mova/PERF-LSP.md task 1). Only ever called by `edn.mova` when
/// the caller's opts map has none of `:eof`/`:readers`/`:default` set
/// (the fast scanner never produces tagged literals or a genuine EOF
/// distinct from a `nil` literal, so those opts would never have anything
/// to act on -- but the shim checks their ABSENCE up front rather than
/// this native trying to detect "opts that don't matter"). Returns
/// `[true value]` on success, `[false nil]` on bail (any construct
/// outside `edn_fast`'s supported EDN subset), never a bare value, so the
/// Mova-side caller can never mistake a bail for a read `nil`.
fn edn_fast_read_string(_interp: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    let s = match &args[0] {
        Value::Str(s) => s.as_ref(),
        other => {
            return Err(RjError::type_err(format!(
                "edn-fast-read-string*: expected a string, got {}",
                other.type_name()
            )))
        }
    };
    let pair = match crate::edn_fast::try_read_edn(s) {
        Some(v) => [Value::Bool(true), v],
        None => [Value::Bool(false), Value::Nil],
    };
    Ok(Value::Vector(crate::value::PVec::from_slice(&pair)))
}

fn read_string_lazy(interp: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    if let Value::Str(s) = &args[0] {
        if let Some(inner) = crate::lazy_map::build(s) {
            return Ok(Value::LazyMap(Arc::new(inner)));
        }
    }
    read_string(interp, args)
}

fn read_string(interp: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    let (opts, s_val) = match args {
        [s] => (None, s),
        [opts, s] => (Some(opts), s),
        _ => unreachable!("ArityHint::Range(1, 2) guarantees 1 or 2 args"),
    };
    let s = match s_val {
        Value::Str(s) => s.as_ref(),
        other => {
            return Err(RjError::type_err(format!(
                "read-string: expected a string, got {}",
                other.type_name()
            )))
        }
    };
    // edn/fast: the 1-arity path ONLY (no `:read-cond` opts, no ctor-
    // literal/tagged-literal post-processing to worry about since the fast
    // path never produces either) tries the direct-to-`Value` byte scanner
    // BEFORE building an `NsContext` at all -- it has no namespace state
    // and never resolves `::kw` (that's one of its bail triggers). `Some`
    // means it fully handled `s`; `None` ("bail", the overwhelming common
    // case for any construct outside its supported EDN subset) falls
    // through to the general reader below exactly as before.
    if opts.is_none() {
        if let Some(v) = crate::edn_fast::try_read_edn(s) {
            return Ok(v);
        }
    }
    let allow_read_cond = match opts {
        Some(Value::Map(m)) => matches!(
            m.get(&Value::Keyword("read-cond".into())),
            Some(Value::Keyword(k)) if k.as_ref() == "allow"
        ),
        Some(other) => {
            return Err(RjError::type_err(format!(
                "read-string: expected an options map, got {}",
                other.type_name()
            )))
        }
        None => false,
    };
    // C3d: `::kw`/`::alias/kw` inside `s` resolve against the CALLER's
    // live `*ns*` (measured: `(binding [*ns* ...] (read-string "::bar"))`
    // picks up the rebinding), so this must consult `interp` fresh on
    // every call rather than reading against a fixed default.
    let ctx = interp.reader_ns_context();
    let (form, ctor_starts, tag_starts) = crate::reader::read_one_with_ns_ctors(s, ctx, allow_read_cond)?;
    match form {
        Some(form) => {
            let form = if ctor_starts.is_empty() {
                form
            } else {
                eval_ctor_literals(interp, &form, &ctor_starts)?
            };
            // W4-PRINTER (print-throwable): resolve every generic tagged
            // literal the reader flagged against the caller's live
            // `*data-readers*` binding -- see `apply_data_readers`'s doc.
            // Empty for the overwhelming common case (no `#tag` at all in
            // `s`), so this is a no-op check, not a walk, for every OTHER
            // `read-string` call in the corpus.
            let form = if tag_starts.is_empty() {
                form
            } else {
                apply_data_readers(interp, &form, &tag_starts)?
            };
            Ok(crate::reader::form_to_value(&form))
        }
        None => Ok(Value::Nil),
    }
}

/// W4-PRINTER (print-throwable's `(binding [*data-readers* {'error
/// identity}] ...)`): resolves each tagged literal the reader flagged
/// (`tag_starts`, one `(span-start, tag)` per generic `#tag form` --
/// see `reader::Reader::tag_literal_starts`' doc) against the CALLER's
/// live `*data-readers*` map, walking the read form innermost-first (same
/// shape as `eval_ctor_literals` above) and replacing each matched
/// sub-form with `(reader-fn payload)`'s result. A tag with NO entry in
/// `*data-readers*` -- the default, `{}` (`core.mova`'s own doc) -- is
/// left exactly as read: `read-string`'s existing "discard the tag, keep
/// the payload" pass-through (measured: `(read-string "#cpp 300")` =>
/// `300`) is UNCHANGED for every tag `*data-readers*` doesn't know about.
fn apply_data_readers(
    interp: &mut Interp,
    form: &Form,
    tag_starts: &[(usize, String)],
) -> Result<Form, RjError> {
    use crate::reader::FormValue;
    let rebuilt_value = match &form.value {
        FormValue::List(items) => Some(FormValue::List(
            items
                .iter()
                .map(|f| apply_data_readers(interp, f, tag_starts))
                .collect::<Result<Vec<_>, _>>()?,
        )),
        FormValue::Vector(items) => Some(FormValue::Vector(
            items
                .iter()
                .map(|f| apply_data_readers(interp, f, tag_starts))
                .collect::<Result<Vec<_>, _>>()?,
        )),
        FormValue::Set(items) => Some(FormValue::Set(
            items
                .iter()
                .map(|f| apply_data_readers(interp, f, tag_starts))
                .collect::<Result<Vec<_>, _>>()?,
        )),
        FormValue::Map(pairs) => {
            let mut out = Vec::with_capacity(pairs.len());
            for (k, v) in pairs {
                out.push((
                    apply_data_readers(interp, k, tag_starts)?,
                    apply_data_readers(interp, v, tag_starts)?,
                ));
            }
            Some(FormValue::Map(out))
        }
        _ => None,
    };
    let rebuilt = match rebuilt_value {
        Some(value) => Form { meta: form.meta.clone(), value, span: form.span },
        None => form.clone(),
    };
    if let Some((_, tag)) = tag_starts.iter().find(|(start, _)| *start == form.span.start) {
        if let Some(reader_fn) = lookup_data_reader(interp, tag) {
            let v = interp.call(&reader_fn, &[crate::reader::form_to_value(&rebuilt)])?;
            return Ok(crate::reader::value_to_form(&v, form.span));
        }
    }
    Ok(rebuilt)
}

/// Looks `tag` (`"error"`, or a namespaced `"my.ns/tag"`) up in the
/// CALLER's live `*data-readers*` dynamic var (default `{}`, same
/// "innermost `binding` frame on this thread" read `builtins::strings::
/// dynamic_var_on` uses for `*print-meta*` et al). `None` when the var is
/// unbound, not a map, or has no entry for this exact tag symbol.
fn lookup_data_reader(interp: &mut Interp, tag: &str) -> Option<Value> {
    let sym = match tag.split_once('/') {
        Some((ns, name)) => Symbol { ns: Some(ns.into()), name: name.into() },
        None => Symbol::simple(tag),
    };
    match interp.globals.get(&Symbol::simple("*data-readers*")) {
        Some(Value::Map(m)) => m.get(&Value::Sym(sym)).cloned(),
        _ => None,
    }
}

/// W3d2: real Clojure's reader CONSTRUCTS a `#pkg.Class[..]` /
/// `#pkg.Class{..}` constructor literal while reading, so `(read-string
/// "#user.R{:a 42}")` hands back the record itself and a literal naming a
/// class that cannot be constructed (`#java.util.Locale[(str 'en)]`)
/// throws from `read-string`, not later. mova's reader desugars such a
/// literal to the equivalent form instead, and has to -- see
/// `reader::Reader::ctor_literal_starts`' doc for why (whole-FILE read
/// before any evaluation).
///
/// `read-string` is the one caller that can close the gap honestly: it has
/// an interpreter, and its input is a single self-contained string, so
/// every type it names is either already defined or genuinely absent. This
/// walks the read form INNERMOST-FIRST and evaluates exactly the sub-forms
/// the reader flagged as constructor literals, substituting each one's
/// value -- which is what makes `exercise-literals`' "ctors can have
/// whitespace after class name", its "only work with constants or statics"
/// rows, and `hinting-test`'s three `(read-string "#..LongHint[...]")`
/// rows all behave like the JVM. Forms the reader did NOT flag are never
/// evaluated, so `(read-string "(+ 1 2)")` still returns the list `(+ 1 2)`
/// unevaluated.
fn eval_ctor_literals(
    interp: &mut Interp,
    form: &Form,
    ctor_starts: &[(usize, bool)],
) -> Result<Form, RjError> {
    use crate::reader::FormValue;
    let rebuilt_value = match &form.value {
        FormValue::List(items) => Some(FormValue::List(
            items
                .iter()
                .map(|f| eval_ctor_literals(interp, f, ctor_starts))
                .collect::<Result<Vec<_>, _>>()?,
        )),
        FormValue::Vector(items) => Some(FormValue::Vector(
            items
                .iter()
                .map(|f| eval_ctor_literals(interp, f, ctor_starts))
                .collect::<Result<Vec<_>, _>>()?,
        )),
        FormValue::Set(items) => Some(FormValue::Set(
            items
                .iter()
                .map(|f| eval_ctor_literals(interp, f, ctor_starts))
                .collect::<Result<Vec<_>, _>>()?,
        )),
        FormValue::Map(pairs) => {
            let mut out = Vec::with_capacity(pairs.len());
            for (k, v) in pairs {
                out.push((
                    eval_ctor_literals(interp, k, ctor_starts)?,
                    eval_ctor_literals(interp, v, ctor_starts)?,
                ));
            }
            Some(FormValue::Map(out))
        }
        _ => None,
    };
    let rebuilt = match rebuilt_value {
        Some(value) => Form { meta: form.meta.clone(), value, span: form.span },
        None => form.clone(),
    };
    // The LIST guard is load-bearing, not defensive: `read_ctor_*_literal`
    // gives the desugared call AND its synthesized head symbol the SAME
    // `full_span`, so a bare `ctor_starts.contains(start)` test also fires
    // on the head -- which would evaluate `clojure.test_clojure.protocols.
    // Plain.` (trailing dot and all) as a standalone symbol and fail with
    // "Unable to resolve symbol". Only the desugared LIST is ever the
    // constructor literal.
    if matches!(rebuilt.value, FormValue::List(_)) {
        if let Some((_, positional)) =
            ctor_starts.iter().find(|(start, _)| *start == form.span.start)
        {
            let v = interp.eval_form(&rebuilt).map_err(|e| {
                // Measured (`hinting-test`): the POSITIONAL `#R[..]`
                // spelling fails with `IllegalArgumentException` -- real
                // Clojure builds it through `Reflector.invokeConstructor`,
                // which reports "no matching ctor" rather than casting --
                // while the `#R{..}` map spelling keeps the
                // `ClassCastException` its `create`/`map->` path raises.
                // Both spellings reach the SAME `check_field_tags` inside
                // mova, so the distinction has to be made here, at the one
                // place that still knows which literal was written.
                if *positional && e.kind == crate::error::ErrorKind::TypeErr {
                    RjError::other(e.message.clone())
                        .with_class(crate::error::JvmClass::IllegalArgument)
                } else {
                    e
                }
            })?;
            return Ok(crate::reader::value_to_form(&v, form.span));
        }
    }
    Ok(rebuilt)
}

/// `(eval form)`: evaluates a data form (as produced by `read-string`, a
/// quoted list, etc.) through the NORMAL evaluation pipeline --
/// `Interp::eval_form`, the same entry point `eval_str` uses per top-level
/// form -- against `self.globals` in whatever namespace `*ns*` CURRENTLY
/// reads as (Clojure's own docstring: eval "operates in the current value
/// of `*ns*`"). A `(def ...)` form therefore interns into that namespace,
/// same as if it had been typed there directly.
///
/// S6/Blocker-2: this is deliberately `Interp::dynamic_ns_name()`, NOT the
/// raw `current_ns` field -- see that fn's doc comment for the measured
/// case (`clojure.test.generative`'s `defspec` macro calling `eval` on a
/// `:tag` form) that made the distinction load-bearing. `current_ns` is
/// what the tree-walker re-resolves the CALLING code's own free symbols
/// against, and it tracks whichever closure is executing; `*ns*` is the
/// separate, rarely-changing "namespace of the file being compiled" that
/// real `eval` actually reads, restored unconditionally afterward so the
/// swap doesn't leak into the caller's own subsequent symbol resolution.
///
/// No real call-site span exists for the synthesized top-level form
/// (`eval`'s own argument already carries one, from being read/evaluated
/// normally); errors raised *inside* the evaluated form still carry their
/// own accurate spans, same convention as `Interp::call`'s placeholder
/// span.
// D3 (2026-08-21, owner-approved veneer per binding directive: interop
// targets Rust-native values, java.*/clojure.lang.* names are a thin
// compatibility veneer, never JVM emulation): `pub(crate)` so
// `builtins::statics` can register the SAME body under `Compiler/eval`
// -- vendored `evaluation.clj`'s `Eval` deftest calls `(Compiler/eval
// '(+ 1 2 3))` and compares it against `(eval '(+ 1 2 3))`; real
// `clojure.lang.Compiler.eval` and `clojure.core/eval` are two spellings
// of the same operation there, so one body backing both spellings here
// is exact, not an approximation.
pub(crate) fn eval_native(interp: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    let placeholder = Span { start: 0, end: 0 };
    // SPEC-W4: `(eval (concat '(+) [1 2]))` hands `eval` a LAZY seq now
    // that `concat` is lazy, and `Form` has no lazy arm -- realize it into
    // concrete form shape first, exactly as macro expansion does. See
    // `Interp::realize_form_value`.
    let form = interp.value_to_form_realized(&args[0], placeholder)?;
    let dynamic_ns = interp.dynamic_ns_name();
    let saved = std::mem::replace(&mut interp.current_ns, dynamic_ns);
    // field2/W-NS: `eval`'s argument is a fresh COMPILATION UNIT -- a
    // top-level form, whichever fn body called `eval`. Resetting the depth
    // (restored below, on every path this fn has) is what makes an `(eval
    // '(ns foo))` inside a running body move the LEXICAL namespace too for
    // the remainder of that eval, which is exactly what the vendored
    // test-helper shim's `eval-in-temp-ns` is built out of. See
    // `Interp::closure_depth`'s field doc and `ns::Interp::switch_ns`.
    let saved_depth = std::mem::replace(&mut interp.closure_depth, 0);
    let result = interp.eval_form(&form);
    interp.closure_depth = saved_depth;
    interp.current_ns = saved;
    result
}

/// `(load-string s)` / `(load-string s name)` -- wishlist #11. Sequentially
/// reads and evaluates EVERY top-level form in `s`, returning the LAST
/// form's value -- unlike `eval` above (one already-read data form) or
/// `read-string` (one form, never evaluated), this is a whole-string
/// mini-file load, backed by the SAME `Interp::eval_str` entry point
/// `Engine::eval_named`/`Reentry::eval_named` (`src/embed/engine.rs`) give
/// embedders, now reachable from script.
///
/// 1-arity is Clojure-faithful: real `clojure.core/load-string`
/// (`.oracle`'s `core.clj`, `load-string` -> `load-reader` over a
/// `StringReader`, i.e. `Compiler.load` with no bound `*source-path*`)
/// leaves every `def`/`defn` inside with `:file "NO_SOURCE_FILE"`
/// metadata -- measured directly against a live `clojure` CLI:
/// `(load-string "(defn f []) (:file (meta (var f)))")` => `
/// "NO_SOURCE_FILE"`. This native reproduces that exactly by defaulting
/// `name` to the literal string `"NO_SOURCE_FILE"`, which is what
/// `special_forms::publish_var_meta` stamps onto `:file` (it reads
/// `self.source_name` verbatim, see that fn's doc).
///
/// 2-arity `(load-string s name)` is an mova EXTENSION: `name` becomes
/// the loaded forms' `:file`/diagnostic source name instead of
/// `"NO_SOURCE_FILE"`. This is what turns a script-side plugin loader
/// (`(load-string plugin-src plugin-path)`) into vars `clojure.repl/
/// source`/`source-fn` can read back -- IF `name` is a real, currently
/// on-disk-readable file path: `source-fn` re-reads `:file` FRESH off
/// disk (see its own doc above) and returns `nil`, not an error, for a
/// synthetic name (`"plugin:foo"`) that isn't one. Argument order (string
/// first, name second) matches `read-string`'s own 2-arity convention
/// above (options/context second) rather than `eval_named`'s
/// (name-first): the pending-corpus row this promotes
/// (`tests/conformance/pending/namespaces.corpus`, `(load-string "(+ 1
/// 2)")`) only exercises 1-arity, so nothing outside this fn's own tests
/// pins the 2-arity order either way.
///
/// SAVES/RESTORES `interp.source_name`/`interp.source` around the
/// `eval_str` call: `eval_str` unconditionally OVERWRITES both fields (it
/// has no nested callers today, see its own doc), so without this a
/// `load-string` called partway through evaluating a real file would
/// corrupt `:file`/`:line` on every `def` AFTER it in that same file --
/// `publish_var_meta` reads `self.source_name`/`self.source` fresh, per
/// `def`, not once at file-open time.
fn load_string_native(interp: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    let s = match &args[0] {
        Value::Str(s) => s.as_ref(),
        other => {
            return Err(RjError::type_err(format!(
                "load-string: expected a string, got {}",
                other.type_name()
            )))
        }
    };
    let name: &str = match args.get(1) {
        Some(Value::Str(n)) => n.as_ref(),
        Some(other) => {
            return Err(RjError::type_err(format!(
                "load-string: expected a string name, got {}",
                other.type_name()
            )))
        }
        None => "NO_SOURCE_FILE",
    };
    let saved_name = interp.source_name.clone();
    let saved_source = interp.source.clone();
    // field5/W-SPAN: kept in lock-step with source_name/source above -- see
    // `ns::require_ns`'s matching save/restore for the same reasoning.
    let saved_source_id = interp.source_id;
    let result = interp.eval_str(name, s);
    interp.source_name = saved_name;
    interp.source = saved_source;
    interp.source_id = saved_source_id;
    result
}

/// `(load-file name)`: reads the file (a relative path resolves against the
/// process cwd), evaluates every form in order, returns the last value.
/// `*file*` and the `:file` meta of vars point at `name` as given. `*ns*` is
/// restored afterwards, even on error. Errors carry the file name and line.
fn load_file_native(interp: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    let name: String = match args[0].unmeta() {
        Value::Str(s) => s.to_string(),
        other => {
            return Err(RjError::type_err(format!(
                "load-file: expected a string, got {}",
                other.type_name()
            )))
        }
    };
    let text = std::fs::read_to_string(&name).map_err(|e| {
        let msg = if e.kind() == std::io::ErrorKind::NotFound {
            format!("{name} (No such file or directory)")
        } else {
            format!("{name}: {e}")
        };
        RjError::other(msg)
            .with_class(crate::error::JvmClass::FileNotFound)
            .with_stack(interp.stack_snapshot(), interp.source_id)
    })?;
    let saved_name = interp.source_name.clone();
    let saved_source = interp.source.clone();
    let saved_source_id = interp.source_id;
    let saved_def_file = interp.def_file.replace(crate::value::Str::from(name.as_str()));
    let saved_ns = interp.current_ns.clone();
    let saved_dyn_ns = interp.dynamic_ns_name();
    let saved_depth = std::mem::replace(&mut interp.closure_depth, 0);
    let file_cell = interp.globals.find_bound_cell(&Symbol::simple("*file*"));
    if let Some(c) = &file_cell {
        c.push_binding(Value::Str(crate::value::Str::from(name.as_str())));
    }
    let is_cljc = name.ends_with(".cljc");
    let result = if is_cljc { interp.eval_str_allow_read_cond(&name, &text) } else { interp.eval_str(&name, &text) };
    if let Some(c) = &file_cell {
        c.pop_binding();
    }
    interp.set_dynamic_ns(saved_dyn_ns);
    interp.closure_depth = saved_depth;
    interp.def_file = saved_def_file;
    // On error the failing file stays installed so the diagnostic points into
    // it (same convention as `load_path`/`require_ns`).
    if result.is_ok() {
        interp.current_ns = saved_ns;
        interp.source_name = saved_name;
        interp.source = saved_source;
        interp.source_id = saved_source_id;
    }
    result
}

/// `(source-fn sym)` -- C3h (clojure.repl surface). Transliterated from
/// real `clojure.repl/source-fn`'s own algorithm (`.oracle/clojure-src/
/// src/clj/clojure/repl.clj`): resolve `sym` to a Var, read its `:file`/
/// `:line` metadata (published by `def`/`defn` -- see
/// `eval::special_forms::publish_var_meta`'s C3h doc for where those two
/// keys come from and why `self.source`/`self.source_name` already being
/// tracked per-file made this a small addition rather than the "needs a
/// span-to-line/column mapping" gap that fn's OLD doc comment predicted),
/// re-read that file FRESH off disk (matching real semantics exactly:
/// this is not a def-time cache -- a file edited and re-slurped after the
/// def would show the new text here too), seek to the line the form
/// starts on, and read exactly ONE form from there via `reader::read_one`
/// -- the same "stop at the first complete form, ignore what follows"
/// behavior real `source-fn`'s proxied-`PushbackReader` trick achieves by
/// hand, reused from `read-string`'s own toolbox (see that fn's doc).
///
/// The returned STRING is the exact original slice `line_start..form_end`
/// of the file's bytes, not a re-print of the parsed value, so
/// formatting/comments/internal whitespace are preserved byte-for-byte --
/// exactly what real `source-fn` returns (measured:
/// `(source-fn 'clojure.test-clojure.repl.example/foo)` =>
/// `"(defn foo [])"`).
///
/// `nil` (not an error) whenever a step doesn't pan out -- unresolvable
/// symbol, no `:file` meta, the file no longer readable, or the reader
/// finding nothing there -- same as real `source-fn`'s `when-let` chain
/// (measured: `(source-fn 'non-existent-fn)` => `nil`).
///
/// `*read-eval*` (`core/core.mova`'s `(def *read-eval* true)`): real
/// `source-fn`'s ONE observable check, `(= :unknown *read-eval*)` ->
/// throws, sits INSIDE its `when-let` chain -- gated on a `:file` already
/// having been found -- which is why a prior pass here gated it the same
/// way and left `(binding [*read-eval* :unknown] (source reduce))`
/// unthrown as an honest, documented blocker: `reduce` is a Rust native
/// with no `:file` metadata to begin with, so that nested check is
/// structurally unreachable for it, matching real semantics' own ordering
/// exactly.
///
/// W4-EVAL task 3 (measured on the oracle, `repl.clj`'s
/// `test-source-read-eval-unknown` deftest): moved the check to the very
/// TOP instead, ahead of resolving `sym` at all. This is a deliberate,
/// narrow divergence, not a reversion of the reasoning above: real
/// `source-fn`'s check never fires AT ALL for `reduce` specifically
/// (nothing upstream of it can make `reduce` grow `:file` metadata), so
/// nested-vs-hoisted is unobservable for every var that already has NO
/// `:file` meta -- hoisting only changes behavior for exactly that case
/// (throw instead of silently returning `nil`), never for a var that DOES
/// have `:file` meta and would have thrown anyway either order. Strictly
/// more cases throw, never fewer, so nothing the corpus expects to
/// silently return `nil` under `*read-eval*` `:unknown` can regress.
fn source_fn_native(interp: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    if let Some(re_cell) = interp.globals.find_bound_cell(&Symbol::simple("*read-eval*")) {
        if let Some(Value::Keyword(k)) = re_cell.get() {
            if k.as_ref() == "unknown" {
                // C3g/W3a class-aware `catch`/`thrown?`: `.with_class` is
                // what makes `(thrown? IllegalStateException ...)` (repl.clj's
                // `test-source-read-eval-unknown`) actually match -- an
                // untagged `RjError::other` falls through
                // `error_kind_class_chain`'s generic `Other` bucket
                // (`Exception`/`RuntimeException`/`Throwable` only), which
                // a class-aware `catch IllegalStateException` does NOT
                // match.
                return Err(RjError::other("Unable to read source while *read-eval* is :unknown.")
                    .with_class(crate::error::JvmClass::IllegalState));
            }
        }
    }
    let Value::Sym(sym) = args[0].unmeta() else {
        return Err(RjError::type_err(format!(
            "source-fn: expected a symbol, got {}",
            args[0].type_name()
        )));
    };
    let Some(cell) = interp.try_resolve_var_cell(sym) else {
        return Ok(Value::Nil);
    };
    let Value::Map(meta) = cell.var_meta() else {
        return Ok(Value::Nil);
    };
    let Some(Value::Str(filepath)) = meta.get(&Value::Keyword("file".into())) else {
        return Ok(Value::Nil);
    };
    let Some(Value::Int(line)) = meta.get(&Value::Keyword("line".into())) else {
        return Ok(Value::Nil);
    };
    let line = (*line).max(1) as usize;

    let Ok(source) = std::fs::read_to_string(filepath.as_ref()) else {
        return Ok(Value::Nil);
    };
    let start = byte_offset_of_line(&source, line);
    let Some(rest) = source.get(start..) else {
        return Ok(Value::Nil);
    };
    // `.cljc` companions get `#?`/`#?@` dispatch turned on for this one
    // read, matching real `source-fn`'s own `(if (.endsWith filepath
    // "cljc") {:read-cond :allow} {})` -- same split every other loading
    // path in this crate makes. Reads against the caller's live ns
    // context (C3d) so any `::kw` in the sliced source resolves the same
    // way it did when the file was loaded.
    let ctx = interp.reader_ns_context();
    let read = if filepath.as_ref().ends_with("cljc") {
        crate::reader::read_one_allow_cond_with_ns(rest, ctx)
    } else {
        crate::reader::read_one_with_ns(rest, ctx)
    };
    match read {
        Ok(Some(form)) => Ok(Value::Str(crate::value::Str::from(&rest[..form.span.end]))),
        _ => Ok(Value::Nil),
    }
}

/// 1-indexed line -> byte offset of that line's first character, by
/// counting newlines -- the same "skip (line - 1) lines" positioning real
/// `source-fn`'s `(dotimes [_ (dec line)] (.readLine rdr))` achieves by
/// consuming a `LineNumberReader` line at a time.
fn byte_offset_of_line(source: &str, line: usize) -> usize {
    if line <= 1 {
        return 0;
    }
    let mut seen = 1usize;
    for (i, c) in source.char_indices() {
        if c == '\n' {
            seen += 1;
            if seen == line {
                return i + 1;
            }
        }
    }
    source.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eval_ok(src: &str) -> Value {
        let mut interp = Interp::new();
        interp.eval_str("test", src).unwrap_or_else(|e| panic!("eval error for {src:?}: {e:?}"))
    }

    #[test]
    fn read_string_reads_one_form_and_stops() {
        assert_eq!(eval_ok(r#"(read-string "(+ 1 2) 3")"#), Value::List(crate::pvec![Value::Sym(crate::value::Symbol::simple("+")), Value::Int(1), Value::Int(2)]));
        assert_eq!(eval_ok(r#"(read-string "")"#), Value::Nil);
    }

    #[test]
    fn read_string_of_tagged_literal_passes_through() {
        assert_eq!(eval_ok(r##"(read-string "#cpp 300")"##), Value::Int(300));
    }

    #[test]
    fn eval_of_read_string_runs_the_form() {
        assert_eq!(eval_ok(r#"(eval (read-string "(+ 1 2)"))"#), Value::Int(3));
    }

    #[test]
    fn eval_errors_are_catchable() {
        assert_eq!(
            eval_ok(r#"(try (eval (read-string "(no-such-fn)")) (catch e :caught))"#),
            Value::Keyword("caught".into())
        );
    }

    #[test]
    fn read_string_reader_errors_are_catchable() {
        assert_eq!(
            eval_ok(r#"(try (read-string "(") (catch e :caught))"#),
            Value::Keyword("caught".into())
        );
    }

    #[test]
    fn eval_of_def_interns_into_the_current_namespace() {
        assert_eq!(eval_ok("(eval (read-string \"(def r2x 9)\")) r2x"), Value::Int(9));
    }

    // --- load-string (wishlist #11) ---------------------------------------

    #[test]
    fn load_string_multi_form_returns_last_value() {
        assert_eq!(eval_ok(r#"(load-string "1 2 3")"#), Value::Int(3));
    }

    #[test]
    fn load_string_1arity_default_name_matches_oracle() {
        // Measured against a live `clojure` CLI: `(load-string "(defn f
        // []) (:file (meta (var f)))")` => `"NO_SOURCE_FILE"`.
        assert_eq!(
            eval_ok(r#"(load-string "(defn f27182 []) (:file (meta (var f27182)))")"#),
            Value::Str(crate::value::Str::from("NO_SOURCE_FILE"))
        );
    }

    #[test]
    fn load_string_error_inside_propagates() {
        assert_eq!(
            eval_ok(r#"(try (load-string "(no-such-fn-zzz)") (catch e :caught))"#),
            Value::Keyword("caught".into())
        );
    }

    #[test]
    fn load_string_2arity_carries_file_meta_and_source_fn_round_trips() {
        let path = std::env::temp_dir()
            .join(format!("mova-load-string-test-{}.mova", std::process::id()));
        let content = "(defn plugin-fn-27182 [] 42)";
        std::fs::write(&path, content).unwrap();
        let path_str = path.to_str().unwrap();

        let file_meta_src =
            format!(r#"(load-string "{content}" "{path_str}") (:file (meta (var plugin-fn-27182)))"#);
        let file_meta = eval_ok(&file_meta_src);

        let source_src =
            format!(r#"(load-string "{content}" "{path_str}") (source-fn 'plugin-fn-27182)"#);
        let source_text = eval_ok(&source_src);

        std::fs::remove_file(&path).ok();

        assert_eq!(file_meta, Value::Str(crate::value::Str::from(path_str)));
        assert_eq!(source_text, Value::Str(crate::value::Str::from(content)));
    }

    #[test]
    fn load_string_does_not_corrupt_enclosing_file_attribution() {
        // A `load-string` mid-file must not leak its (possibly synthetic)
        // name into `:file` metadata for defs AFTER it in the SAME file
        // -- see `load_string_native`'s save/restore doc.
        let mut interp = Interp::new();
        let src = r#"
            (load-string "(+ 1 2)" "synthetic-name")
            (defn after-load-string-27182 [])
            (:file (meta (var after-load-string-27182)))
        "#;
        let result = interp
            .eval_str("real-enclosing-file.mova", src)
            .unwrap_or_else(|e| panic!("eval error: {e:?}"));
        assert_eq!(result, Value::Str(crate::value::Str::from("real-enclosing-file.mova")));
    }
}
