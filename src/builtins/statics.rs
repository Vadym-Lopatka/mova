//! S3: `java.lang`-boxed-numeric-class STATIC FIELDS and the STATIC METHODS
//! the vendored conformance suite calls in head position (`Long/MAX_VALUE`,
//! `(Double/isNaN x)`, ...). Scope was derived by grepping
//! `tests/clojure-suite/vendor/*.clj` for `[A-Z][A-Za-z0-9]*/[A-Za-z_0-9]+`
//! and keeping the high-frequency subset (see the branch's own commit
//! message for the full frequency table) plus the "guaranteed-needed"
//! fields named in the S3 brief. Exotic reflection-y statics the suite
//! only touches once or twice for unrelated Java-object-construction
//! mechanics mova has no analogue for (`RecordToTestStatics*/create`,
//! `Range/create`, `Collectors/counting`, `Clojure/var`, `UUID/randomUUID`,
//! `PersistentQueue/EMPTY`, ...) are deliberately skipped. The `*/TYPE`
//! class-token fields (`Integer/TYPE`, `Long/TYPE`, ...) WERE one of these
//! until W3f, which registers them as primitive-named `ClassVal::Builtin`
//! markers (`"int"`, `"long"`, ...) that `builtins::arrays::resolve_ref_kind`
//! recognizes -- `into-array`'s explicit-component-type form
//! (`transducers.clj`/`sequences.clj`'s `(into-array Integer/TYPE ...)`)
//! measurably needs them to build a primitive rather than `Object[]` array.
//! `Class/forName` was ONE of these until S6, which
//! added it as a thin name-resolution veneer -- see `class_for_name`'s own
//! doc for why it stopped counting as "exotic": it needed no new
//! machinery, just a lookup through the builtin-class table `import`
//! already resolves through.
//!
//! ## Registration mechanics
//!
//! `src/env.rs`'s `Env::get`/`for_each_global_candidate` (`src/ns.rs`)
//! resolve a namespace-qualified symbol by EXACT `{ns, name}` match FIRST,
//! before ever trying a bare-name fallback -- see `ns.rs`'s module doc.
//! `clojure.math`'s `PI`/`E` (`math.rs`) lean on the *fallback* path (bare
//! registration only, reached because nothing claims the exact `Math/PI`
//! spelling), which is fine for them because "PI"/"E" are unique bare
//! names. It would NOT be fine here: `MAX_VALUE` alone is shared by
//! `Long`/`Integer`/`Double`/`Float`/`Byte`/`Short`, each with a different
//! value, so every field/method below is registered under its EXACT
//! qualified `Symbol { ns: Some(class), name }` via `reg_static_value`/
//! `reg_static_fn`, never bare -- `Long/MAX_VALUE` and `Byte/MAX_VALUE`
//! must NOT resolve to the same cell.
//!
//! ## Value-type collapse
//!
//! mova has exactly one integral `Value::Int(i64)` and one floating
//! `Value::Float(f64)` -- there is no `Integer`/`Byte`/`Short`/`Float`
//! distinct from `Long`/`Double`. Every field below stores the JVM value
//! widened into whichever of those two mova already has (measured against
//! real Clojure 1.13.0-alpha6, see each constant's own comment for the
//! exact probe). Print-parity holds for every integral field (an `i64`
//! prints identically regardless of what JVM box it came from) and for
//! `Double/MAX_VALUE`/every `##Inf`/`##-Inf`/`##NaN` field (measured: mova
//! prints all three exactly as `pr-str` does on the JVM). It does NOT hold
//! for `Double/MIN_VALUE` (a pre-existing, already-pending-ledgered
//! Rust-vs-Java shortest-float-digits divergence on subnormals -- see
//! `tests/conformance/pending/numerics.corpus`'s trailing `4.9E-324`
//! entry) or for `Float/MAX_VALUE` (mova has no real `f32`, so it stores
//! `f32::MAX` WIDENED to `f64` -- numerically the exact right value, but
//! printed with full `f64` shortest-round-trip precision
//! (`3.4028234663852886E38`) rather than Java's `Float.toString` digits
//! (`3.4028235E38`)). Both are registered as VALUES (so arithmetic/
//! comparison forms that use them still conform) but neither is given a
//! bare-printing form in `tests/conformance/corpus/interop-statics.corpus`
//! -- see that file's own comments, and the new pending-ledger line added
//! alongside the pre-existing subnormal entry.

use crate::builtins::{ArityHint, ArityHint::Exact};
use crate::error::RjError;
use crate::eval::Interp;
use crate::value::{ArrayVal, NativeFn, Symbol, Value};
use std::sync::{Arc, Mutex};

/// Widens any of mova's numeric `Value`s to `f64`, matching `Number.
/// doubleValue()` -- the same coercion `clojure.math`'s `math_f64`
/// (`math.rs`) does for its own `^double`-hinted fns, duplicated here
/// (rather than exported) because this module has a different error-
/// message prefix convention (`Class/method`, not a bare fn name).
fn as_f64(v: &Value, op: &str) -> Result<f64, RjError> {
    match v {
        Value::Int(n) => Ok(*n as f64),
        Value::Float(f) => Ok(*f),
        Value::BigInt(b) => Ok(b.to_f64()),
        Value::Ratio(r) => Ok(r.to_f64()),
        Value::BigDec(d) => Ok(d.to_f64()),
        other => Err(RjError::type_err(format!("{op}: expected a number, got {}", other.type_name()))),
    }
}

/// A borrowed `&str` out of a `Value::Str`, or a type error under `op`'s
/// name -- the shape every `parseXxx`/`valueOf` static below needs for its
/// string-argument overload.
fn as_str<'a>(v: &'a Value, op: &str) -> Result<&'a str, RjError> {
    match v {
        Value::Str(s) => Ok(s.as_ref()),
        other => Err(RjError::type_err(format!("{op}: expected a string, got {}", other.type_name()))),
    }
}

/// Looks up the already-registered bare global `bare_name` and calls it
/// with `args` -- the "restore exactly that binding" primitive `String/
/// format`, `Long/compare`/`Integer/compare` and the `Long`/`Integer`
/// `max`/`min` rows use (see their registration site's own comment): those
/// six rows must behave IDENTICALLY to `clojure.core`'s own `format`/
/// `compare`/`max`/`min`, which are closures registered elsewhere in
/// `register_all`'s call order (this module runs after `numbers`/`sorted`/
/// `strings`, so the lookup always hits). `.expect` is deliberate, not a
/// user-facing error path: a miss here means `register_all`'s order broke,
/// a programming bug this module's own tests would catch immediately, not
/// a condition a caller of `String/format` could ever trigger.
fn bare_delegate(interp: &mut Interp, bare_name: &str, args: &[Value]) -> Result<Value, RjError> {
    let f = interp
        .globals
        .get(&Symbol::simple(bare_name))
        .unwrap_or_else(|| panic!("statics::bare_delegate: bare `{bare_name}` not registered yet"));
    interp.call(&f, args)
}

/// Registers a VALUE (not a fn) under the exact qualified symbol
/// `class/name` -- the static-FIELD sibling of `reg_static_fn` below, and
/// this module's namesake use of `Env::set_builtin`'s ns-qualified form
/// (see the module doc's "Registration mechanics" section).
///
/// W4C: ALSO registers the identical `Value` under the fully-qualified
/// spelling (`java.lang.Long/MAX_VALUE`, not just `Long/MAX_VALUE`), when
/// that differs from `class` itself -- see `reg_static_fn`'s doc for why
/// this second alias exists (same reasoning, field side).
#[track_caller]
fn reg_static_value(i: &mut Interp, class: &'static str, name: &'static str, val: Value) {
    let fq_class = fq_class_name(class);
    i.globals.set_builtin(Symbol { ns: Some(class.into()), name: name.into() }, val.clone());
    if fq_class != class {
        i.globals.set_builtin(Symbol { ns: Some(fq_class.into()), name: name.into() }, val);
    }
}

/// W3f (small-tail sweep): builds the primitive `Class` token a boxed
/// numeric/`Character`/`Boolean` class's `TYPE` field holds on the real
/// JVM (`int.class`, `long.class`, ...) -- a plain `ClassVal::Builtin`
/// marker named after the primitive's own `Class.getName()` spelling
/// (`"int"`, `"long"`, ...), which `builtins::arrays::resolve_ref_kind`
/// recognizes by that exact name and maps to the matching `ArrayKind`
/// instead of falling through to the generic reference-type
/// `Object(name)` arm. `pred: None` matches every other marker-only
/// `ClassVal::Builtin` row in this codebase (e.g. `PersistentQueue/EMPTY`'s
/// class) -- nothing calls `instance?` against a primitive `Class` token
/// in the vendored suite.
fn primitive_type_class(name: &'static str) -> Value {
    Value::Class(Arc::new(crate::types::ClassVal::Builtin { name, pred: None }))
}

/// Registers a native fn under the exact qualified symbol `class/name`,
/// with the same generated arity check `builtins::reg` gives every bare
/// native -- duplicated (not delegated to `reg`) purely because `reg`
/// always interns `Symbol::simple`, which would collide across classes
/// for shared field/method names (see the module doc).
///
/// W4C: ALSO registers the identical native under the fully-qualified
/// spelling (`java.lang.Long/bitCount`, not just `Long/bitCount`), when
/// that differs from `class` itself -- both are the exact SAME method on
/// the real JVM (`Long/bitCount` and `java.lang.Long/bitCount` are one
/// call, not two), and mova's own syntax-quote qualification
/// (`crate::quasiquote`) can produce the fully-qualified spelling from
/// source that only ever WROTE the bare one: a `defmacro`'s syntax-quoted
/// body resolves every symbol at macro-DEFINITION time, including a
/// class-namespaced one like `Long/bitCount`, to its canonical
/// fully-qualified form -- measured, `tests/clojure-suite/vendor-libs/
/// clojure/test/check/random.clj`'s `mix-gamma` macro (SplitMix64's
/// gamma-mixing step -- SPEC-W6a moved that arithmetic into
/// `crate::splitrandom`, so the example is now HISTORY rather than a
/// live consumer; the resolution rule it demonstrates is unchanged and
/// still applies to every `defmacro` that syntax-quotes a static call)
/// writes bare `(Long/bitCount ...)`
/// inside its own syntax-quoted template, which mova's macroexpansion
/// turns into `java.lang.Long/bitCount` by the time it actually runs --
/// "Unable to resolve symbol: java.lang.Long/bitCount" otherwise, even
/// though `Long/bitCount` itself was already fully implemented and
/// tested. Registering under both spellings, ONCE, here -- rather than
/// teaching symbol resolution to special-case "did qualification touch
/// this" -- fixes this for every static field/method this module ever
/// registers or will register, not just `bitCount`.
#[track_caller]
fn reg_static_fn(
    i: &mut Interp,
    class: &'static str,
    name: &'static str,
    arity: ArityHint,
    f: impl Fn(&mut Interp, &[Value]) -> Result<Value, RjError> + Send + Sync + 'static,
) {
    let full = format!("{class}/{name}");
    let fq_class = fq_class_name(class);
    let err_fq_class = fq_class.clone();
    let native = NativeFn::new(full.clone(), move |interp: &mut Interp, args: &[Value]| {
        if !arity_matches(arity, args.len()) {
            // W4B-MESSAGES (errors.clj's `compile-error-examples`): real
            // Clojure resolves `Class/staticMethod` via reflection at
            // compile time, and an arity that matches no overload comes
            // back as `IllegalArgumentException: "No matching method
            // <name> found taking <n> args for class <fq-class>"`
            // (measured against the oracle for `Long/parseLong` called
            // with 0 and 3 args, compat/w4b-method-arity-oracle-
            // transcript.txt) -- not mova's own "expected N, got M"
            // wording, which used to be strictly MORE informative (it
            // states the arity `reg_static_fn` actually implements) but
            // didn't match the real text. `type_err` (not `arity`)
            // because the real exception is `IllegalArgumentException`,
            // not `ArityException` -- reflective overload resolution on
            // the JVM never constructs an `ArityException` at all, it's
            // a pure `IllegalArgumentException` from `Reflector`.
            return Err(RjError::type_err(format!(
                "No matching method {name} found taking {} args for class {err_fq_class}",
                args.len()
            ))
            .with_stack(interp.stack_snapshot(), interp.source_id));
        }
        f(interp, args)
    });
    let native = Value::Native(Arc::new(native));
    i.globals.set_builtin(Symbol { ns: Some(class.into()), name: name.into() }, native.clone());
    if fq_class != class {
        i.globals.set_builtin(Symbol { ns: Some(fq_class.clone().into()), name: name.into() }, native.clone());
    }
    // kondo-wave: the REVERSE direction -- a class registered under its
    // own fully-qualified spelling as `class` (e.g. `clojure.lang.RT`)
    // also gets its bare short alias (`RT`) registered, same flat-
    // global-table precedent as the short-primary classes above (this
    // module already registers `Long`'s statics under BOTH `Long` and
    // `java.lang.Long` unconditionally, regardless of any `:import` in
    // the calling namespace) -- needed because vendored `clojure.tools.
    // reader` source spells static calls on `clojure.lang.RT`/`PersistentList`/
    // etc as the BARE name after its own `(:import (clojure.lang RT
    // ...))`.
    if let Some(short) = class.rsplit('.').next() {
        if short != class && short != fq_class {
            i.globals.set_builtin(Symbol { ns: Some(short.into()), name: name.into() }, native);
        }
    }
}

/// `ArityHint::matches` is private to `builtins::mod`; this free fn
/// reproduces its exact behavior locally rather than widen that module's
/// visibility for one caller.
fn arity_matches(arity: ArityHint, n: usize) -> bool {
    match arity {
        ArityHint::Exact(k) => n == k,
        ArityHint::Min(k) => n >= k,
        ArityHint::Range(lo, hi) => n >= lo && n <= hi,
        ArityHint::Any => true,
    }
}

/// The fully-qualified class name real Clojure's reflective error
/// mentions ("for class <fq-class>") for each SHORT class name this
/// module registers statics under -- every one of them is `java.lang.*`
/// except `Compiler`, which is `clojure.lang.Compiler`. Anything already
/// containing a `.` (this module also registers a few classes under
/// their fully-qualified spelling directly, e.g. `clojure.lang.RT`,
/// `java.lang.Class`) is already fully qualified and passed through
/// unchanged.
fn fq_class_name(class: &str) -> String {
    if class.contains('.') {
        return class.to_string();
    }
    match class {
        "Compiler" => "clojure.lang.Compiler".to_string(),
        other => format!("java.lang.{other}"),
    }
}

pub fn register(i: &mut Interp) {
    // -- static FIELDS -----------------------------------------------------
    // Every value below measured via:
    //   clojure -Sdeps '{:deps {org.clojure/clojure {:mvn/version
    //   "1.13.0-alpha6"}}}' -M -e '(prn Long/MAX_VALUE) ...'
    // (S3 branch scratchpad probe.clj); the class column is the probed
    // `(class ...)` alongside each, kept here only as provenance -- mova
    // collapses every one into `Value::Int`/`Value::Float` per the module
    // doc's "Value-type collapse" section.

    // Long (java.lang.Long) -- both ends of mova's own native i64 range.
    reg_static_value(i, "Long", "MAX_VALUE", Value::Int(i64::MAX)); // measured: 9223372036854775807
    reg_static_value(i, "Long", "MIN_VALUE", Value::Int(i64::MIN)); // measured: -9223372036854775808

    // C10: `clojure.lang.PersistentQueue/EMPTY` -- `data_structures.clj`'s
    // (and `sequences.clj`'s) ONE entry point into the whole `Value::
    // Queue` type; a fully-qualified DOTTED class name (not a short alias
    // like `Long` above) is the exact spelling every call site uses, and
    // `reg_static_value` keys off whatever `class` string is passed here
    // verbatim, so this is the only registration `Value::Queue` needs to
    // become reachable from source.
    reg_static_value(i, "clojure.lang.PersistentQueue", "EMPTY", Value::Queue(crate::value::PVec::new()));

    // Integer (java.lang.Integer) -- widened into mova's one Int; print
    // parity holds (2147483647 / -2147483648 print identically either way).
    reg_static_value(i, "Integer", "MAX_VALUE", Value::Int(2_147_483_647)); // measured: 2147483647
    reg_static_value(i, "Integer", "MIN_VALUE", Value::Int(-2_147_483_648)); // measured: -2147483648

    // Double (java.lang.Double).
    reg_static_value(i, "Double", "MAX_VALUE", Value::Float(f64::MAX)); // measured: 1.7976931348623157E308 (print-conforms)
    reg_static_value(i, "Double", "MIN_VALUE", Value::Float(f64::from_bits(1))); // measured: 4.9E-324 (smallest positive subnormal; print does NOT conform, see module doc)
    reg_static_value(i, "Double", "POSITIVE_INFINITY", Value::Float(f64::INFINITY)); // measured: ##Inf
    reg_static_value(i, "Double", "NEGATIVE_INFINITY", Value::Float(f64::NEG_INFINITY)); // measured: ##-Inf
    reg_static_value(i, "Double", "NaN", Value::Float(f64::NAN)); // measured: ##NaN
    reg_static_value(i, "Double", "MAX_EXPONENT", Value::Int(1023)); // measured: 1023 (java.lang.Integer on the JVM)
    reg_static_value(i, "Double", "MIN_EXPONENT", Value::Int(-1022)); // measured: -1022

    // Float (java.lang.Float) -- mova has no real f32; every value below is
    // the f32 constant WIDENED to f64 (numerically exact), never a bare f64
    // literal typed by hand (see module doc re: Float/MAX_VALUE's print
    // divergence, which follows from this widening, not from a wrong value).
    reg_static_value(i, "Float", "MAX_VALUE", Value::Float(f32::MAX as f64)); // measured (JVM Float.toString): 3.4028235E38; mova widened value prints 3.4028234663852886E38 -- print does NOT conform, see module doc
    reg_static_value(i, "Float", "POSITIVE_INFINITY", Value::Float(f64::INFINITY)); // measured: ##Inf
    reg_static_value(i, "Float", "NEGATIVE_INFINITY", Value::Float(f64::NEG_INFINITY)); // measured: ##-Inf
    reg_static_value(i, "Float", "NaN", Value::Float(f64::NAN)); // measured: ##NaN

    // Byte / Short (java.lang.Byte / java.lang.Short) -- widened into Int.
    reg_static_value(i, "Byte", "MAX_VALUE", Value::Int(127)); // measured: 127
    reg_static_value(i, "Byte", "MIN_VALUE", Value::Int(-128)); // measured: -128
    reg_static_value(i, "Short", "MAX_VALUE", Value::Int(32767)); // measured: 32767
    reg_static_value(i, "Short", "MIN_VALUE", Value::Int(-32768)); // measured: -32768

    // Boolean (java.lang.Boolean).
    reg_static_value(i, "Boolean", "TRUE", Value::Bool(true)); // measured: true
    reg_static_value(i, "Boolean", "FALSE", Value::Bool(false)); // measured: false

    // W3f (small-tail sweep): the `*/TYPE` primitive `Class` tokens --
    // `transducers.clj`'s `test-transduce` and `sequences.clj`'s array-
    // typed reduce/seq coverage both build primitive arrays via
    // `(into-array Integer/TYPE ...)` etc (see `primitive_type_class`'s
    // own doc + `builtins::arrays::resolve_ref_kind`, which is the other
    // half of this fix).
    reg_static_value(i, "Integer", "TYPE", primitive_type_class("int"));
    reg_static_value(i, "Long", "TYPE", primitive_type_class("long"));
    reg_static_value(i, "Float", "TYPE", primitive_type_class("float"));
    reg_static_value(i, "Double", "TYPE", primitive_type_class("double"));
    reg_static_value(i, "Byte", "TYPE", primitive_type_class("byte"));
    reg_static_value(i, "Short", "TYPE", primitive_type_class("short"));
    reg_static_value(i, "Character", "TYPE", primitive_type_class("char"));
    reg_static_value(i, "Boolean", "TYPE", primitive_type_class("boolean"));

    // C7 (vecveneer): `clojure.lang.PersistentList/EMPTY` -- the ONE
    // shared instance every real `ASeq.empty()` returns (measured
    // `(identical? clojure.lang.PersistentList/EMPTY (.empty (seq v)))`,
    // `vectors.clj`'s `test-vecseq`). mova's `Value::List` has no
    // identity beyond structural equality (`imbl` vectors, no pointer
    // tag), so `identical?` on two SEPARATELY-built empty lists is
    // already true here regardless (measured: `(identical? (list) (list))`
    // is true in mova, unlike the JVM) -- this binding just needs to BE
    // an empty `Value::List` for that existing `identical?` behavior to
    // carry the assertion, not a literal singleton object.
    reg_static_value(
        i,
        "clojure.lang.PersistentList",
        "EMPTY",
        crate::value::empty_list_singleton(),
    );

    // -- static METHODS ------------------------------------------------
    reg_static_fn(i, "Double", "isNaN", Exact(1), double_is_nan);
    reg_static_fn(i, "Float", "isNaN", Exact(1), double_is_nan); // same impl: NaN-ness doesn't depend on precision
    reg_static_fn(i, "Double", "isInfinite", Exact(1), double_is_infinite);
    reg_static_fn(i, "Float", "isInfinite", Exact(1), double_is_infinite); // same impl, same reason as isNaN
    reg_static_fn(i, "Double", "compare", Exact(2), double_compare);
    reg_static_fn(i, "Double", "parseDouble", Exact(1), double_parse_double);
    reg_static_fn(i, "Float", "parseFloat", Exact(1), float_parse_float);
    reg_static_fn(i, "Long", "parseLong", Exact(1), long_parse_long);
    reg_static_fn(i, "Long", "valueOf", Exact(1), long_value_of);
    reg_static_fn(i, "Integer", "valueOf", Exact(1), integer_value_of);
    // S5 stragglers -- see each fn's own doc comment for its oracle probe.
    reg_static_fn(i, "Long", "bitCount", Exact(1), long_bit_count);
    reg_static_fn(i, "System", "currentTimeMillis", Exact(0), system_current_time_millis);
    reg_static_fn(i, "System", "nanoTime", Exact(0), system_nano_time);
    // lsp/host (clojure-lsp-on-Mova campaign): `jsonrpc4clj.server`'s
    // `chan-server` defaults `:clock` to this -- a REAL `java.time.
    // Clock` type-hinted field on the JVM (measured: a `proxy [Object]
    // []` stand-in throws `ClassCastException`, silently, on an async
    // thread). See `hostclass::mk_clock`'s doc.
    reg_static_fn(i, "java.time.Clock", "systemDefaultZone", Exact(0), |_i, _args| {
        Ok(crate::hostclass::mk_clock())
    });
    // mova campaign (clojure-lsp): `clj-kondo.impl.core/config-hash`
    // (SHA-256) and `clojure-lsp.shared/md5` (MD5) both reach for this --
    // see `hostclass::mk_message_digest`'s doc.
    reg_static_fn(i, "java.security.MessageDigest", "getInstance", Exact(1), |_i, args| match &args[0] {
        Value::Str(algorithm) => crate::hostclass::mk_message_digest(algorithm),
        other => Err(RjError::type_err(format!(
            "MessageDigest/getInstance: expected a String, got {}",
            other.type_name()
        ))),
    });
    // S6 -- see `class_for_name`'s own doc.
    reg_static_fn(i, "Class", "forName", Exact(1), class_for_name);
    // D3 (2026-08-21): the SAME static, also under its fully-qualified
    // spelling -- vendored `evaluation.clj`'s own `class-for-name` helper
    // spells it `(java.lang.Class/forName name)`, fully qualified, not
    // the short `Class/forName` every other call site in this module was
    // measured against. `reg_static_fn` keys on an EXACT `{ns, name}`
    // pair (see this module's doc's "Registration mechanics" section),
    // so the short registration above doesn't also answer the qualified
    // spelling -- two rows, same body, not a new fn.
    reg_static_fn(i, "java.lang.Class", "forName", Exact(1), class_for_name);
    // S6/libstatics -- see each fn's own doc comment for its oracle probe.
    reg_static_fn(i, "Math", "getExponent", Exact(1), math_get_exponent);
    reg_static_fn(i, "Long", "numberOfLeadingZeros", Exact(1), long_number_of_leading_zeros);
    reg_static_fn(i, "Character", "isDigit", Exact(1), character_is_digit);
    reg_static_fn(i, "Long", "reverse", Exact(1), long_reverse);
    // mova/PLAN.md interop-census batch (generic gaps, not clj-kondo-
    // specific): `Character/digit` (char+radix -> int, -1 if not a digit
    // in that radix -- tools.reader's number parser), `Character/
    // valueOf` (boxing no-op on mova, same as `Integer/valueOf` above),
    // `Integer/toString(int, radix)` (edamame/tools.reader print
    // non-decimal literals), `Boolean/parseBoolean` (babashka.cli),
    // `Thread/sleep` (real sleep; clj-kondo's core.clj retry loop),
    // `System/gc` (no-op -- mova has no GC to trigger; db.clj calls it
    // best-effort before a file lock retry).
    reg_static_fn(i, "Character", "digit", Exact(2), |_i, args| {
        let c = char_arg("digit", &args[0])?;
        let radix = match &args[1] {
            Value::Int(n) => *n,
            other => {
                return Err(RjError::type_err(format!(
                    "Character/digit: expected an int radix, got {}",
                    other.type_name()
                )))
            }
        };
        Ok(Value::Int(c.to_digit(radix as u32).map(|d| d as i64).unwrap_or(-1)))
    });
    reg_static_fn(i, "Character", "valueOf", Exact(1), |_i, args| {
        char_arg("valueOf", &args[0]).map(Value::Char)
    });
    reg_static_fn(i, "Integer", "toString", Exact(2), |_i, args| {
        let n = match &args[0] {
            Value::Int(n) => *n,
            other => {
                return Err(RjError::type_err(format!(
                    "Integer/toString: expected an int, got {}",
                    other.type_name()
                )))
            }
        };
        let radix = match &args[1] {
            Value::Int(r) => *r,
            other => {
                return Err(RjError::type_err(format!(
                    "Integer/toString: expected an int radix, got {}",
                    other.type_name()
                )))
            }
        };
        Ok(Value::Str(radix_to_string(n, radix as u32).into()))
    });
    reg_static_fn(i, "Boolean", "parseBoolean", Exact(1), |_i, args| match &args[0] {
        Value::Str(s) => Ok(Value::Bool(s.eq_ignore_ascii_case("true"))),
        other => Err(RjError::type_err(format!(
            "Boolean/parseBoolean: expected a string, got {}",
            other.type_name()
        ))),
    });
    reg_static_fn(i, "Thread", "sleep", Exact(1), |i, args| {
        let ms = match &args[0] {
            Value::Int(n) => *n,
            // the JVM finds no `sleep` overload for such an argument
            _ => {
                return Err(RjError::type_err("No matching method sleep found taking 1 args").with_class(crate::error::JvmClass::IllegalArgument))
            }
        };
        if ms > 0 {
            i.intr.sleep(std::time::Duration::from_millis(ms as u64))?;
        }
        Ok(Value::Nil)
    });
    reg_static_fn(i, "System", "gc", Exact(0), |_i, _args| Ok(Value::Nil));
    // mova/PLAN.md interop-census batch: `java.util.regex.Pattern/
    // compile`+`quote`, `Matcher/quoteReplacement` -- see
    // `builtins::regex::compile_pattern` (same engine `re-pattern` uses)
    // and `regex::escape`/manual `\`/`$` escaping for the quoting pair
    // (mova has no `\Q...\E` literal-quoting support in `fancy_regex`, so
    // `Pattern/quote` escapes each metacharacter individually instead --
    // functionally equivalent when the result is spliced into another
    // pattern string, which is the only way the vendored call sites use it).
    reg_static_fn(i, "Pattern", "compile", Exact(1), |_i, args| match &args[0] {
        Value::Str(s) => Ok(Value::Regex(crate::builtins::regex::compile_pattern(
            s,
            "Pattern/compile",
        )?)),
        other => Err(RjError::type_err(format!(
            "Pattern/compile: expected a string, got {}",
            other.type_name()
        ))),
    });
    reg_static_fn(i, "Pattern", "quote", Exact(1), |_i, args| match &args[0] {
        Value::Str(s) => Ok(Value::Str(regex::escape(s).into())),
        other => Err(RjError::type_err(format!(
            "Pattern/quote: expected a string, got {}",
            other.type_name()
        ))),
    });
    reg_static_fn(i, "Matcher", "quoteReplacement", Exact(1), |_i, args| match &args[0] {
        Value::Str(s) => {
            let mut out = std::string::String::with_capacity(s.len());
            for ch in s.chars() {
                if ch == '\\' || ch == '$' {
                    out.push('\\');
                }
                out.push(ch);
            }
            Ok(Value::Str(out.into()))
        }
        other => Err(RjError::type_err(format!(
            "Matcher/quoteReplacement: expected a string, got {}",
            other.type_name()
        ))),
    });
    // mova/PLAN.md interop-census batch: `URLDecoder/decode` (2-arg,
    // charset name ignored -- mova strings are always UTF-8) --
    // `clojure-lsp.shared/unescape-uri` percent-decodes a URI path.
    reg_static_fn(i, "URLDecoder", "decode", Exact(2), |_i, args| match &args[0] {
        Value::Str(s) => url_decode(s).map(|s| Value::Str(s.into())),
        other => Err(RjError::type_err(format!(
            "URLDecoder/decode: expected a string, got {}",
            other.type_name()
        ))),
    });
    // D5 (`clojure.pprint`): the `java.lang.Character`/`Integer`/`System`
    // statics the vendored sources call, and only those -- `cl_format`'s
    // four case-converting writer proxies (`~(`/`~:(`/`~@(`) are built
    // entirely out of `toUpperCase`/`toLowerCase`/`isLetter`/
    // `isWhitespace`, its parameter parser out of `Integer/parseInt` and
    // `Integer/valueOf`, and `dispatch`'s `#object[...]` fallback out of
    // `System/identityHashCode`. `pp-newline` needs `System/getProperty
    // "line.separator"`. Registered under the short spelling only:
    // `Character`/`Integer`/`System` are all `java.lang.*`, which real
    // Clojure auto-imports, and every vendored call site spells them
    // short (same rule the `Math`/`Long`/`Character/isDigit` rows above
    // follow).
    //
    // Char-vs-codepoint: real `Character/toUpperCase` is OVERLOADED on
    // `char` and `int` and returns the same kind it was given; the
    // vendored sources use BOTH (`(Character/toUpperCase (char c))` in a
    // `(.write writer (int ...))`, and `(Character/toUpperCase ^Character
    // (nth s offset))` on a char from a string). So these preserve the
    // argument's kind rather than picking one, which is what the JVM
    // overload set does.
    reg_static_fn(i, "Character", "toUpperCase", Exact(1), |_i, args| {
        char_case_convert("toUpperCase", &args[0], true)
    });
    reg_static_fn(i, "Character", "toLowerCase", Exact(1), |_i, args| {
        char_case_convert("toLowerCase", &args[0], false)
    });
    reg_static_fn(i, "Character", "isLetter", Exact(1), |_i, args| {
        Ok(Value::Bool(char_arg("isLetter", &args[0])?.is_alphabetic()))
    });
    reg_static_fn(i, "Character", "isWhitespace", Exact(1), |_i, args| {
        Ok(Value::Bool(char_arg("isWhitespace", &args[0])?.is_whitespace()))
    });
    // MOVA-PATCH: rest of the java.lang.Character static veneer clj-kondo calls.
    reg_static_fn(i, "Character", "isUpperCase", Exact(1), |_i, args| {
        Ok(Value::Bool(char_arg("isUpperCase", &args[0])?.is_uppercase()))
    });
    reg_static_fn(i, "Character", "isLowerCase", Exact(1), |_i, args| {
        Ok(Value::Bool(char_arg("isLowerCase", &args[0])?.is_lowercase()))
    });
    reg_static_fn(i, "Character", "isLetterOrDigit", Exact(1), |_i, args| {
        Ok(Value::Bool(char_arg("isLetterOrDigit", &args[0])?.is_alphanumeric()))
    });
    // `Integer/valueOf` and `Integer/parseInt` differ on the JVM only in
    // boxing (`Integer` vs `int`), which mova does not model -- both are
    // `Value::Int` here. Both accept a string (the vendored use) and, for
    // `valueOf`, a number (`cl_format`'s exponent arithmetic).
    reg_static_fn(i, "Integer", "parseInt", Exact(1), |_i, args| {
        parse_int_static("parseInt", &args[0])
    });
    reg_static_fn(i, "Integer", "valueOf", Exact(1), |_i, args| {
        parse_int_static("valueOf", &args[0])
    });
    // Only the properties mova can answer truthfully. `line.separator`/
    // `file.separator`/`path.separator` are the ones the vendored sources
    // read (`pprint`'s `pp-newline`; kondo-wave: clj-kondo's `impl/
    // core.clj` builds a path-separator regex from `file.separator` at
    // namespace load time, and `System.getProperty("path.separator")` is
    // ALSO how `clojure-lsp.kondo`'s own `config-for-paths` joins
    // multiple lint paths into kondo's one `:lint` string); every other
    // key returns nil, which is exactly what the JVM does for an unset
    // property -- no invented values.
    reg_static_fn(i, "System", "getProperty", Exact(1), |_i, args| {
        let Value::Str(k) = args[0].unmeta() else {
            return Err(RjError::type_err(format!(
                "System/getProperty: expected a string, got {}",
                args[0].type_name()
            )));
        };
        Ok(match k.as_ref() {
            // mova targets Unix-likes; `\n` is the honest answer here,
            // and `printer.clj`'s own `platform-newlines` helper asserts
            // against exactly this.
            "line.separator" => Value::Str(crate::value::Str::from("\n")),
            // kondo-wave: Unix-likes only, same stance as `line.separator`
            // above -- `/` and `:` are the real, honest answers there, not
            // invented ones (every Unix/Linux/macOS JVM answers exactly
            // these two).
            "file.separator" => Value::Str(crate::value::Str::from("/")),
            "path.separator" => Value::Str(crate::value::Str::from(":")),
            // kondo-wave: `user.dir` -- another honest, answerable one
            // (`std::env::current_dir()`), not invented. `clojure-lsp-
            // kondo`'s own smoke harness (`kondo_smoke.clj`'s `find-
            // repo-root`) reads it via `(System/getProperty "user.dir")`
            // to seed a real filesystem walk; falling through to `nil`
            // here (as if unset) broke that walk before a File was even
            // constructed. Falls back to `nil` (matching the JVM's own
            // "can't determine" case) only if the OS call itself fails.
            "user.dir" => std::env::current_dir()
                .ok()
                .map(|p| Value::Str(crate::value::Str::from(p.to_string_lossy().into_owned())))
                .unwrap_or(Value::Nil),
            // kondo-wave: `user.home` -- same honest-`std::env` answer as
            // `user.dir` above. clj-kondo's own `impl/core.clj`'s `home-
            // config` reads it directly (`(io/file (System/getProperty
            // "user.home") ".config" "clj-kondo")`) to find the user's
            // global config dir; falling through to `nil` made that
            // `io/file` call try to coerce `nil` to a path and throw.
            "user.home" => std::env::var("HOME")
                .ok()
                .map(|p| Value::Str(crate::value::Str::from(p)))
                .unwrap_or(Value::Nil),
            // clojure-lsp warm-start campaign: `java.io.tmpdir` -- another
            // honest `std::env` answer (`std::env::temp_dir()`, which
            // itself checks `TMPDIR`/falls back to `/tmp` on Unix, exactly
            // mirroring the JVM's own resolution). Previously fell through
            // to `nil` (as if unset), which clj-kondo's/clojure-lsp's own
            // tmp-file plumbing then reads as blank rather than `/tmp`.
            "java.io.tmpdir" => Value::Str(crate::value::Str::from(
                std::env::temp_dir().to_string_lossy().into_owned(),
            )),
            _ => Value::Nil,
        })
    });
    // clojure-lsp campaign (mova/PLAN.md): `System/lineSeparator()` --
    // the JDK 7+ zero-arg static equivalent of `System/getProperty
    // "line.separator"` just above (same "mova targets Unix-likes, `\n`
    // is the honest answer" reasoning). `cljfmt.core`'s own `default-
    // line-separator` reads this directly: `#?(:clj (System/
    // lineSeparator) :cljs \newline)`.
    reg_static_fn(i, "System", "lineSeparator", Exact(0), |_i, _args| {
        Ok(Value::Str(crate::value::Str::from("\n")))
    });
    // `System/identityHashCode` -- on the JVM, a hash that distinguishes
    // two EQUAL but non-identical objects. mova has no object header to
    // read one out of, so this is the value's ordinary hash, and the
    // difference is stated rather than hidden: two `=` values get the
    // same answer here where the JVM would (usually) give two. The one
    // vendored caller is `dispatch.clj`'s `pprint-simple-default`,
    // rendering `#object[Class 0x<hex> <value>]`; it never compares two
    // of them, and printer.clj/metadata.clj never reach that branch, so
    // nothing in scope observes the difference. Deliberately NOT faked
    // with an address: `Value` is a copy-on-write enum, and several of
    // its arms have no stable allocation to take the address of at all.
    reg_static_fn(i, "System", "identityHashCode", Exact(1), |interp, args| {
        Ok(Value::Int(crate::builtins::sorted::hash_value(interp, &args[0])?))
    });
    reg_static_fn(i, "String", "valueOf", Exact(1), string_value_of);
    // ns: restrict qualified->bare fallback to clojure.core spellings
    // (DESIGN-flow-namespace.md item 5): `(String/format ...)`, `(Long/
    // compare ...)`, `(Integer/compare ...)` and the `Long`/`Integer`
    // `max`/`min` pairs used to reach `clojure.core`'s bare `format`/
    // `compare`/`max`/`min` purely through the old unconditional trailing
    // bare-name probe -- this module never claimed any of those six exact
    // `{class, name}` pairs. Each row below is a REAL registration
    // (through `reg_static_fn`, same as every other row in this file, so
    // an arity miss gets the same `IllegalArgumentException`-shaped
    // message every other static method here does) whose body -- via
    // `bare_delegate` -- calls the SAME bare cell the old fallback used
    // to reach, not a reimplementation: `clojure.core/format`/`compare`/
    // `max`/`min` are ordinary closures (`strings.rs`/`sorted.rs`/
    // `numbers.rs`), not standalone Rust fn items, so they can't be
    // reused by value the way `Compiler/eval` reuses `reflect::eval_native`
    // right below -- a fresh `interp.call` lookup each invocation is the
    // equivalent for a closure-backed var, and (being dynamic) also keeps
    // faith with a hypothetical `(def compare ...)` redefinition, exactly
    // as real Java-static-method-calling-a-Var indirection would.
    reg_static_fn(i, "String", "format", ArityHint::Min(1), |interp, args| bare_delegate(interp, "format", args));
    reg_static_fn(i, "Long", "compare", Exact(2), |interp, args| bare_delegate(interp, "compare", args));
    reg_static_fn(i, "Integer", "compare", Exact(2), |interp, args| bare_delegate(interp, "compare", args));
    reg_static_fn(i, "Long", "max", ArityHint::Min(1), |interp, args| bare_delegate(interp, "max", args));
    reg_static_fn(i, "Long", "min", ArityHint::Min(1), |interp, args| bare_delegate(interp, "min", args));
    reg_static_fn(i, "Integer", "max", ArityHint::Min(1), |interp, args| bare_delegate(interp, "max", args));
    reg_static_fn(i, "Integer", "min", ArityHint::Min(1), |interp, args| bare_delegate(interp, "min", args));
    // D3 (2026-08-21, owner-approved veneer per binding directive):
    // `clojure.lang.Compiler/eval` -- vendored `evaluation.clj`'s `Eval`
    // deftest calls `(Compiler/eval '(+ 1 2 3))` directly (after `(import
    // '(clojure.lang Compiler Compiler$CompilerException))`) and compares
    // it against `(eval '(+ 1 2 3))`. mova has no separate compiler-vs-
    // interpreter split -- `clojure.core/eval` IS the whole evaluation
    // path here -- so `Compiler/eval` reuses `reflect::eval_native`'s
    // exact body rather than a bespoke reimplementation; see that fn's
    // own doc for why one body backing both spellings is exact, not an
    // approximation.
    reg_static_fn(i, "Compiler", "eval", Exact(1), crate::builtins::reflect::eval_native);

    // D1: `(Collectors/counting)` -- this module's doc listed it among
    // the "exotic reflection-y statics deliberately skipped"; D1 unskips
    // exactly it, because `vectors.clj`'s `test-vector-parallel-stream`
    // (1024 assertions) calls nothing else on a stream. Returns the ONE
    // collector marker `.collect` accepts -- see
    // `hostclass::HostKind::Collector`. Registered under both the short
    // (`:import`ed) spelling the suite writes and the fully-qualified
    // one, since either can appear in head position.
    for class in ["Collectors", "java.util.stream.Collectors"] {
        reg_static_fn(i, class, "counting", Exact(0), |_i, _args| {
            Ok(crate::hostclass::mk_counting_collector())
        });
    }
    // S7 (wave-C item 8): `clojure.lang.MapEntry/create` -- the ONE static
    // factory `Value::MapEntry` gets, and it is genuinely suite-demanded
    // rather than speculative surface. `tests/clojure-suite/vendor-libs/
    // clojure/walk.clj:46` is `(outer (clojure.lang.MapEntry/create (inner
    // (key form)) (inner (val form))))`, and that branch is reached only
    // once `(instance? clojure.lang.IMapEntry form)` starts answering
    // `true` -- which is exactly what this branch made happen. The
    // regression check caught it: `clojure_walk.clj` went 25 -> 18 without
    // this row (every walk over a map now enters the entry branch and died
    // on an unresolved symbol).
    //
    // Measured on 1.13.0-alpha6 (`compat/mapentry-oracle-transcript2.txt`
    // rows 029-032): `(clojure.lang.MapEntry/create :a 1)` prints
    // `[:a 1]`, `(class ..)` is `clojure.lang.MapEntry`, `(map-entry? ..)`
    // is `true`, `(= .. [:a 1])` is `true`. Fully-qualified spelling only,
    // same reasoning as the `java.util.UUID` statics just below.
    reg_static_fn(i, "clojure.lang.MapEntry", "create", Exact(2), |_i, args| {
        Ok(Value::MapEntry(crate::value::PVec::pair(args[0].clone(), args[1].clone())))
    });

    // S6 (assert/namespace/uuid batch): `java.util.UUID` statics -- ONLY
    // the fully-qualified spelling (measured, same reasoning as `java.
    // util.Random`/`java.util.Date` in `hostclass.rs`: `UUID` alone is
    // not `java.lang.*` and does not auto-resolve on the real JVM, so no
    // bare `UUID/...` alias is added). `predicates.clj`'s own truth-table
    // fixture calls exactly `(java.util.UUID/randomUUID)`.
    reg_static_fn(i, "java.util.UUID", "randomUUID", Exact(0), uuid_random_uuid);
    reg_static_fn(i, "java.util.UUID", "fromString", Exact(1), uuid_from_string);

    // lsp/host: `java.io.File/createTempFile` -- clojure-lsp.server's
    // log-path fallback (`server.clj`) calls exactly the 2-arg
    // (prefix, suffix) form. Real JVM creates an empty file on disk.
    reg_static_fn(i, "java.io.File", "createTempFile", Exact(2), file_create_temp_file);

    // SPEC-W1 task 7: the two statics `clojure.test.check.generators`'
    // gen-builtins call, both onto backing types mova ALREADY has (there
    // is no new value shape here -- `Value::BigDec` and `Value::Uri` both
    // predate this task; only the static-factory SPELLING was missing).
    //
    // `(BigDecimal/valueOf x)` -- `gen/big-decimal`'s `#(BigDecimal/valueOf
    // %)`. NOT the same as `(BigDecimal. x)` for a double: see
    // `builtins::numbers::bigdecimal_value_of`'s doc. Bare AND
    // fully-qualified, same "real Clojure auto-imports every `java.lang.*`
    // class bare" reasoning the boxed-numeric statics above use --
    // `java.math.BigDecimal` is NOT `java.lang.*`, but `BigDecimal` IS one
    // of the classes `clojure.core` itself imports into every namespace
    // (measured: bare `BigDecimal/valueOf` resolves on the oracle), and
    // `hostclass::construct` already accepts both ctor spellings.
    reg_static_fn(i, "BigDecimal", "valueOf", Exact(1), |_i, args| {
        crate::builtins::numbers::bigdecimal_value_of(&args[0])
    });
    reg_static_fn(i, "java.math.BigDecimal", "valueOf", Exact(1), |_i, args| {
        crate::builtins::numbers::bigdecimal_value_of(&args[0])
    });

    // `(java.net.URI/create s)` -- `gen/uri`'s `#(java.net.URI/create (str
    // "http://" % ".com"))`. Real `URI.create` differs from `new URI(s)`
    // only in the exception it throws on malformed input
    // (IllegalArgumentException rather than the checked
    // URISyntaxException); mova's `Value::Uri` is the narrow "the text the
    // constructor was given" shape (see that variant's own doc) and
    // performs no syntax validation on either path, so the two spellings
    // are the same operation here. Fully-qualified only, like the
    // `java.net.URI` CLASS row in `hostclass::exception_class_rows`.
    reg_static_fn(i, "java.net.URI", "create", Exact(1), |_i, args| match &args[0] {
        Value::Str(s) => Ok(Value::Uri(s.clone())),
        other => Err(RjError::type_err(format!(
            "java.net.URI/create: expected a string, got {}",
            other.type_name()
        ))),
    });

    // `(clojure.lang.RT/print x writer)` -- S7 (tail wave), measured:
    // rt.clj's `bare-rt-print` helper calls this directly (below `pr`,
    // which is itself `(.write *out* (pr-str x))`-shaped) to observe what
    // printing looks like before `clojure.core/print-initialized` flips
    // true. mova has no such staged-initialization boundary at all (`pr`
    // always prints the same way), so this is exactly `pr`'s own body --
    // the SECOND arg (`writer`) is accepted but ignored, same as every
    // other narrow veneer here: the one measured call site always passes
    // the dynamically-bound `*out*` itself, which `out_write` already
    // writes through regardless.
    reg_static_fn(i, "clojure.lang.RT", "print", Exact(2), |interp, args| {
        let s = crate::printer::pr_str(&args[0]);
        crate::builtins::strings::out_write(interp, &s)?;
        Ok(Value::Nil)
    });

    // kondo-wave: `(clojure.lang.RT/map entries)` -- builds a persistent
    // map from an array/seq of alternating key/value entries. Real
    // clj-kondo/tools.reader's OWN vendored map-literal reader
    // (`edn.clj`/`reader.clj`'s `read-map`) builds EVERY `{...}` literal
    // it reads through exactly this call (`(RT/map (to-array coll))`) --
    // unresolved before this, so any file read through the vendored
    // `tools.reader` path (not mova's own reader), including clj-kondo's
    // own `.clj-kondo/config.edn`, failed on its first map literal.
    // Duplicate-key policy matches `hash-map` above (last value wins).
    reg_static_fn(i, "clojure.lang.RT", "map", Exact(1), |interp, args| {
        // `seq_items` answers `Ok(None)` for a genuinely EMPTY array/seq
        // too (`(seq [])` is `nil`), not only for "not seqable" -- an
        // empty entries array is a real, valid (empty) map, so this
        // must NOT error on `None`.
        let items: Vec<Value> = match interp.seq_items(&args[0])? {
            Some(seq) => seq.into_iter().collect(),
            None => Vec::new(),
        };
        if items.len() % 2 != 0 {
            return Err(RjError::other(format!(
                "No value supplied for key: {}",
                crate::printer::pr_str(items.last().unwrap())
            )));
        }
        let mut m = crate::value::PMap::new();
        for pair in items.chunks(2) {
            m.insert(interp.normalize_key(pair[0].clone())?, pair[1].clone());
        }
        Ok(Value::Map(m))
    });

    // e2: `(clojure.lang.PersistentList/create coll)` -- tools.reader's read-list (kondo reading config.edn with a list).
    reg_static_fn(i, "clojure.lang.PersistentList", "create", Exact(1), |interp, args| {
        let items: crate::value::PVec = match interp.seq_items(&args[0])? {
            Some(seq) => seq.into_iter().collect(),
            None => crate::value::PVec::new(),
        };
        if items.is_empty() {
            return Ok(crate::value::empty_list_singleton());
        }
        Ok(Value::List(items))
    });

    // C3c (sequences.clj's `test-longrange-corners`, via its own
    // `unlimited-range-create` helper): `clojure.lang.Range/create` /
    // `clojure.lang.LongRange/create` -- the two static factories real
    // Clojure's OWN `range` delegates to internally (an int-overflow-safe
    // `LongRange` when every bound fits a `long`, a lazy generic `Range`
    // otherwise). mova's `range` builtin (`builtins::seq::range_impl`)
    // already reproduces both paths' observable behavior in ONE fn (the
    // Long-overflow corners this deftest exercises -- `Long/MAX_VALUE`/
    // `MIN_VALUE` arithmetic wraparound -- were fixed in `range_lazy`
    // just before this task), so both statics delegate to that exact
    // same implementation rather than re-deriving range semantics a
    // second time: measured, `Range/create`/`LongRange/create`/`range`
    // agree on every corner the oracle probes. 1, 2, or 3 args only (`(end)`
    // / `(start end)` / `(start end step)`) -- unlike `range` itself,
    // neither static has a 0-arg overload on the real JVM, so this
    // deliberately does NOT accept `Exact(0)`.
    reg_static_fn(i, "clojure.lang.Range", "create", crate::builtins::ArityHint::Range(1, 3), crate::builtins::seq::range_impl);
    reg_static_fn(i, "clojure.lang.LongRange", "create", crate::builtins::ArityHint::Range(1, 3), crate::builtins::seq::range_impl);

    // C3c (sequences.clj's `test-ArrayIter`): `clojure.lang.ArrayIter/
    // createFromObject` -- builds the SAME `HostKind::Iterator` cursor
    // `.iterator` already builds for any seqable target (`eval::
    // types_forms::universal_object_dot_method`'s `"iterator"` arm,
    // this fn's only difference is being reachable as a class STATIC
    // rather than an instance method): `Interp::seq_items` already walks
    // every mova array kind (object/boolean/byte/short/int/long/float/
    // double/char) into correctly-typed elements, and `nil` -- measured,
    // `(clojure.lang.ArrayIter/createFromObject nil)` -- becomes an
    // immediately-exhausted iterator (`seq_items` returns `None` for
    // `nil`), matching this deftest's own `nil` -> `[]` row.
    reg_static_fn(i, "clojure.lang.ArrayIter", "createFromObject", Exact(1), |interp, args| {
        let items = interp.seq_items(&args[0])?;
        let remaining = items.map(Value::List).unwrap_or(Value::Nil);
        Ok(crate::hostclass::mk_iterator(remaining))
    });
}

/// `(java.util.UUID/randomUUID)` -- see `Value::random_uuid_bits`'s own
/// doc for the entropy source and RFC 4122 version/variant bit-twiddle.
fn uuid_random_uuid(_interp: &mut Interp, _args: &[Value]) -> Result<Value, RjError> {
    Ok(Value::Uuid(Arc::new(Value::random_uuid_bits())))
}

/// `(java.util.UUID/fromString s)` -- measured: a malformed string
/// throws `IllegalArgumentException: Invalid UUID string: <s>`; matched
/// here by an ordinary `RjError::type_err` (message text is not
/// conformance-scored, only occurrence/coarse kind, per
/// CONFORMANCE-GUARANTEE.md's comparison rules). See `Value::parse_uuid`
/// for the exact accepted shape (and its documented narrowing versus the
/// real ctor's more permissive per-dash-group parse).
fn uuid_from_string(_interp: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    let s = as_str(&args[0], "java.util.UUID/fromString")?;
    Value::parse_uuid(s)
        .map(|bits| Value::Uuid(Arc::new(bits)))
        .ok_or_else(|| RjError::type_err(format!("java.util.UUID/fromString: Invalid UUID string: {s}")))
}

/// `(java.io.File/createTempFile prefix suffix)` -- creates an empty file
/// in the system temp dir named `<prefix><random><suffix>`, same shape as
/// the real ctor. Only the 2-arg overload (no explicit dir) is used by
/// clojure-lsp's `server.clj`.
fn file_create_temp_file(_interp: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    let prefix = as_str(&args[0], "java.io.File/createTempFile")?;
    let suffix = as_str(&args[1], "java.io.File/createTempFile")?;
    let unique = Value::random_uuid_bits();
    let path = std::env::temp_dir().join(format!("{prefix}{unique:032x}{suffix}"));
    std::fs::File::create(&path)
        .map_err(|e| RjError::type_err(format!("java.io.File/createTempFile: {e}")))?;
    Ok(crate::hostclass::mk_java_file(path.to_string_lossy().into_owned().into()))
}

/// `(Math/getExponent x)`: the unbiased base-2 exponent of `x`'s IEEE-754
/// bit pattern, `java.lang.Math.getExponent`'s exact algorithm -- measured
/// `(Math/getExponent 8.0)` => `3`, `(Math/getExponent 1.0)` => `0`,
/// `(Math/getExponent 0.0)` => `-1023` (`Double/MIN_EXPONENT - 1`, the
/// same answer zero/subnormals share -- their biased-exponent field is
/// all-zero), `(Math/getExponent -8.0)` => `3` (sign is irrelevant, the
/// exponent bits don't encode it), `(Math/getExponent
/// Double/POSITIVE_INFINITY)` / `(Math/getExponent Double/NaN)` => `1024`
/// (`Double/MAX_EXPONENT + 1`, both share the all-ones biased-exponent
/// field).
fn math_get_exponent(_interp: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    let f = as_f64(&args[0], "Math/getExponent")?;
    if f.is_nan() || f.is_infinite() {
        return Ok(Value::Int(1024));
    }
    let raw_exp = ((f.to_bits() >> 52) & 0x7FF) as i64;
    Ok(Value::Int(if raw_exp == 0 { -1023 } else { raw_exp - 1023 }))
}

/// `(Long/numberOfLeadingZeros x)`: count of leading zero bits in `x`'s
/// two's-complement 64-bit representation -- measured `(Long/
/// numberOfLeadingZeros 1)` => `63`, `(Long/numberOfLeadingZeros 0)` =>
/// `64` (the JVM's documented edge case: an all-zero value counts as 64,
/// matching `u64::leading_zeros`' own all-zero answer), `(Long/
/// numberOfLeadingZeros -1)` => `0`, `(Long/numberOfLeadingZeros
/// Long/MAX_VALUE)` => `1`.
///
/// SPEC-W3: an integer BIGNUM argument is accepted too, and that is the
/// ORACLE'S OWN behaviour, not a loosening. `Long.numberOfLeadingZeros`
/// takes a primitive `long`, so a `clojure.lang.BigInt` argument reaches
/// it through `Reflector.boxArg`'s `((Number) arg).longValue()` --
/// measured on 1.13.0-alpha6: `(Long/numberOfLeadingZeros (bigint
/// 1099511627776))` => `23`, while a value too large for a long throws
/// there instead. `to_i64_exact` reproduces exactly that split.
///
/// Reached from `clojure.test.check.generators/shrink-long`, which
/// `gen/simple-type` -- and so `s/gen` for `coll?`/`vector?`/`map?`/
/// `set?`/`seq?`/`associative?`/`any?` -- goes through. On the JVM that
/// fn's `^long` parameter hint has already coerced the value long before
/// this call; mova does not yet honour primitive param hints, which is
/// its own ledger entry in docs/SPEC-PORT-PATCHES.md.
fn long_number_of_leading_zeros(_interp: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    match &args[0] {
        Value::Int(n) => Ok(Value::Int((*n as u64).leading_zeros() as i64)),
        Value::BigInt(b) | Value::BigInteger(b) => match b.to_i64_exact() {
            Some(n) => Ok(Value::Int((n as u64).leading_zeros() as i64)),
            None => Err(RjError::type_err(format!(
                "Long/numberOfLeadingZeros: value out of range for long: {}",
                crate::printer::pr_str(&args[0])
            ))),
        },
        other => Err(RjError::type_err(format!(
            "Long/numberOfLeadingZeros: expected an int, got {}",
            other.type_name()
        ))),
    }
}

/// `Integer/toString(int, radix)`: real Java is sign-then-magnitude in
/// the given radix, lowercase digits (measured: `(Integer/toString -255
/// 16)` => `"-ff"`). `radix` 10 is the common case (`i64::to_string`);
/// others go through manual digit accumulation since `i64` has no
/// built-in non-decimal formatter.
fn radix_to_string(n: i64, radix: u32) -> std::string::String {
    if radix == 10 {
        return n.to_string();
    }
    let neg = n < 0;
    let mut mag = n.unsigned_abs();
    if mag == 0 {
        return "0".to_string();
    }
    let mut digits = Vec::new();
    while mag > 0 {
        let d = (mag % radix as u64) as u32;
        digits.push(std::char::from_digit(d, radix).unwrap_or('0'));
        mag /= radix as u64;
    }
    if neg {
        digits.push('-');
    }
    digits.iter().rev().collect()
}

/// `URLDecoder/decode(s, charset)`: percent-decoding, `+` -> space (real
/// `application/x-www-form-urlencoded` semantics, which is what real
/// Java's `URLDecoder` implements -- NOT plain percent-decoding, which is
/// `URI`'s job). Charset arg is ignored: mova strings are always UTF-8.
fn url_decode(s: &str) -> Result<std::string::String, RjError> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut idx = 0;
    while idx < bytes.len() {
        match bytes[idx] {
            b'+' => {
                out.push(b' ');
                idx += 1;
            }
            b'%' => {
                let hex = s.get(idx + 1..idx + 3).ok_or_else(|| {
                    RjError::type_err("URLDecoder/decode: incomplete % escape".to_string())
                })?;
                let byte = u8::from_str_radix(hex, 16).map_err(|_| {
                    RjError::type_err(format!("URLDecoder/decode: invalid % escape {hex:?}"))
                })?;
                out.push(byte);
                idx += 3;
            }
            b => {
                out.push(b);
                idx += 1;
            }
        }
    }
    std::string::String::from_utf8(out)
        .map_err(|_| RjError::type_err("URLDecoder/decode: invalid UTF-8 result".to_string()))
}

/// `(Character/isDigit c)`: measured `(Character/isDigit \5)` => `true`,
/// `(Character/isDigit \a)` => `false`, `(Character/isDigit \space)` =>
/// `false`. Real `java.lang.Character.isDigit` is a full-Unicode
/// (category `Nd`) predicate; this covers the ASCII `0`-`9` subset only
/// -- the whole measured, corpus-expressible set for this task (`char::
/// is_ascii_digit`) -- and deliberately does not chase non-ASCII decimal
/// digits (Devanagari, full-width, ...) with no oracle-measured corpus
/// row driving them.
/// D5: `clojure.lang.Var/pushThreadBindings` / `popThreadBindings` -- see
/// the call site in `builtins::atoms::register` for why these two exist.
/// Lives here rather than there purely because `reg_static_fn` (the
/// qualified-symbol registrar) is private to this module.
pub(crate) fn reg_var_thread_binding_statics(i: &mut Interp) {
    reg_static_fn(i, "clojure.lang.Var", "pushThreadBindings", Exact(1), |_i, args| {
        crate::builtins::atoms::push_thread_bindings_native(args)
    });
    reg_static_fn(i, "clojure.lang.Var", "popThreadBindings", Exact(0), |_i, _args| {
        crate::env::pop_thread_bindings();
        Ok(Value::Nil)
    });
}

/// D5: the char a `java.lang.Character` static was handed, whether spelled
/// as a `\c` char literal or as an `int` codepoint (the JVM overloads
/// every one of these on both; see the `Character` registrations' comment).
fn char_arg(who: &str, v: &Value) -> Result<char, RjError> {
    match v.unmeta() {
        Value::Char(c) => Ok(*c),
        Value::Int(n) => u32::try_from(*n).ok().and_then(char::from_u32).ok_or_else(|| {
            RjError::type_err(format!("Character/{who}: {n} is not a character codepoint"))
        }),
        other => Err(RjError::type_err(format!(
            "Character/{who}: expected a char or codepoint, got {}",
            other.type_name()
        ))),
    }
}

/// D5: `Character/toUpperCase`/`toLowerCase`, returning the SAME kind it
/// was given (char in, char out; int in, int out) -- the JVM's own
/// overload behavior. Uses `char::to_uppercase`'s first mapped char:
/// `java.lang.Character.toUpperCase(char)` is likewise a 1:1 char mapping
/// and cannot express the multi-char expansions (e.g. `ß` -> `SS`) that
/// `String.toUpperCase` can, so taking the first is the matching
/// behavior, not a shortcut.
fn char_case_convert(who: &str, v: &Value, upper: bool) -> Result<Value, RjError> {
    let c = char_arg(who, v)?;
    let converted =
        if upper { c.to_uppercase().next() } else { c.to_lowercase().next() }.unwrap_or(c);
    Ok(match v.unmeta() {
        Value::Int(_) => Value::Int(converted as i64),
        _ => Value::Char(converted),
    })
}

/// D5: `Integer/parseInt`/`Integer/valueOf` -- see their registrations.
fn parse_int_static(who: &str, v: &Value) -> Result<Value, RjError> {
    match v.unmeta() {
        Value::Str(s) => s.as_ref().trim().parse::<i64>().map(Value::Int).map_err(|_| {
            RjError::other(format!("For input string: \"{}\"", s.as_ref())).with_class(crate::error::JvmClass::NumberFormat)
        }),
        Value::Int(n) => Ok(Value::Int(*n)),
        Value::Char(c) => Ok(Value::Int(*c as i64)),
        other => Err(RjError::type_err(format!(
            "Integer/{who}: expected a string or int, got {}",
            other.type_name()
        ))),
    }
}

fn character_is_digit(_interp: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    match &args[0] {
        Value::Char(c) => Ok(Value::Bool(c.is_ascii_digit())),
        other => Err(RjError::type_err(format!(
            "Character/isDigit: expected a char, got {}",
            other.type_name()
        ))),
    }
}

/// `(Long/reverse x)`: reverses the ORDER of the 64 bits in `x`'s two's-
/// complement representation (bit 63 <-> bit 0, ..., NOT byte-order/
/// endianness) -- `java.lang.Long.reverse`'s exact algorithm, measured
/// `(Long/reverse 1)` => `-9223372036854775808` (`Long/MIN_VALUE`: the
/// lone set bit moves from position 0 to position 63), `(Long/reverse
/// 0)` => `0`, `(Long/reverse -1)` => `-1` (every bit set, reversal is a
/// no-op), `(Long/reverse Long/MAX_VALUE)` => `-2`, `(Long/reverse
/// Long/MIN_VALUE)` => `1`. `u64::reverse_bits` is exactly this
/// operation.
///
/// BUG this fn fixes (S6/libstatics): before this registration,
/// `Long/reverse` had no EXACT `{ns: "Long", name: "reverse"}` global
/// entry, so `env::Env::get`'s bare-name FALLBACK path (see this
/// module's own doc, "Registration mechanics") resolved it to the
/// unrelated bare `reverse` seq builtin instead -- `(Long/reverse 1)`
/// errored `"can't create seq from int"` rather than doing bit
/// reversal. Registering the exact qualified symbol here (same
/// mechanism every other `Long/*` static in this file already uses)
/// makes the exact-match path win, so the fallback is never reached for
/// this symbol -- no change needed to the resolution code itself.
fn long_reverse(_interp: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    match &args[0] {
        Value::Int(n) => Ok(Value::Int((*n as u64).reverse_bits() as i64)),
        other => Err(RjError::type_err(format!(
            "Long/reverse: expected an int, got {}",
            other.type_name()
        ))),
    }
}

/// `(String/valueOf x)`: `java.lang.String.valueOf`'s "stringify anything"
/// overload set -- measured `(String/valueOf 5)` => `"5"`, `(String/
/// valueOf true)` => `"true"`, `(String/valueOf \a)` => `"a"`. Every
/// scalar shape reuses mova's own display rendering (`crate::printer::
/// display_str`, the same one `str` itself uses), which already agrees
/// with Java's `String.valueOf` there. `nil` is a deliberate SURPRISE,
/// not guessed: measured `(String/valueOf nil)` => `Execution error
/// (NullPointerException) at java.lang.String/<init>` -- Clojure's
/// reflective overload pick for an untyped `nil` argument across
/// `String/valueOf`'s many overloads lands on `valueOf(char[])`, which
/// calls `new String(char[])` and NPEs on a null array, NOT the
/// `valueOf(Object)` overload's `"null"` string a naive reading of the
/// javadoc would predict. Reproduced here as an ordinary catchable
/// `RjError` (message text is out of conformance scope; propagating some
/// error, not returning a string, is the measured contract to match).
fn string_value_of(_interp: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    match &args[0] {
        Value::Nil => Err(RjError::other(
            "String/valueOf: null argument (matches real Clojure's ambiguous \
             reflective dispatch to String.valueOf(char[]), which NPEs on null)"
                .to_string(),
        )),
        other => Ok(Value::Str(crate::printer::display_str(other).into())),
    }
}

/// `(Class/forName name)`: a COMPAT VENEER, not JVM reflection -- resolves
/// a fully-qualified class name (or one of the 8 primitive JVM array
/// binary names, `"[Z" "[B" "[C" "[S" "[F" "[D" "[I" "[J"`) to the SAME
/// `Class` value a bare class-name symbol/`class`/`import` would produce,
/// riding the EXISTING builtin-class table rather than adding any JVM
/// class-hierarchy machinery. Measured: `(identical? (Class/forName
/// "java.lang.Long") Long)` => `true`, `(= (Class/forName "[D") (class
/// (double-array 1)))` => `true`.
///
/// Ordinary names go through `builtins::types::class_by_full_name` -- the
/// SAME interning table `import` resolves through (see that fn's own
/// doc), which is what makes the `identical?` claim hold: both paths
/// return the identical cached `Value::Class`, not two separately-minted
/// look-alikes. A bracket name has no entry in that table (arrays aren't
/// named classes -- see `types::array_jvm_name`'s doc), so
/// `array_class_for_binary_name` below builds a throwaway ZERO-LENGTH
/// array of the matching `ArrayKind` and asks `builtins::types::class_of`
/// for ITS class -- the exact same call a real `(double-array 1)` makes,
/// which is what makes `=` (not just name equality) hold against a real
/// array's class. Multi-dimension (`"[[D"`) and reference-element
/// (`"[Ljava.lang.String;"`) binary names are out of scope: not in this
/// task's oracle-measured set, and no vendored suite form calls them.
///
/// Unknown name: real Clojure throws `ClassNotFoundException` naming the
/// class (measured: `(Class/forName "nope.Nope")` => `THROW
/// ClassNotFoundException nope.Nope`); mova has no exception-class
/// taxonomy (message TEXT is out of conformance scope, see
/// `CONFORMANCE-GUARANTEE.md`), so this is an ordinary catchable
/// `RjError` whose message still names the class.
fn class_for_name(_interp: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    let name = as_str(&args[0], "Class/forName")?;
    if let Some(cv) = crate::builtins::types::class_by_full_name(name) {
        return Ok(cv);
    }
    if let Some(cv) = array_class_for_binary_name(name) {
        return Ok(cv);
    }
    Err(RjError::other(format!("Class/forName: class not found: {name}")))
}

/// Builds a zero-length array `Value` of the `ArrayKind` a primitive JVM
/// array binary name (`"[D"`, ...) names, then defers to
/// `builtins::types::class_of` for the actual `Class` value -- see
/// `class_for_name`'s doc for why this indirection (rather than a
/// bespoke name-keyed cache) is what makes the identity claim hold.
fn array_class_for_binary_name(name: &str) -> Option<Value> {
    use crate::value::ArrayKind::*;
    let kind = match name {
        "[Z" => Boolean,
        "[B" => Byte,
        "[C" => Char,
        "[S" => Short,
        "[F" => Float,
        "[D" => Double,
        "[I" => Int,
        "[J" => Long,
        _ => return None,
    };
    let arr = Value::Array(Arc::new(ArrayVal { kind, dims: 1, data: Mutex::new(Vec::new()) }));
    Some(crate::builtins::types::class_of(&arr))
}

/// `(Double/isNaN x)` / `(Float/isNaN x)`: measured `(Double/isNaN 1.0)` =>
/// `false`, `(Double/isNaN Double/NaN)` => `true`, `(Float/isNaN
/// Float/NaN)` => `true`. `f64::is_nan` is bit-for-bit the same predicate
/// IEEE-754 defines, so it needs no precision-specific variant.
fn double_is_nan(_interp: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    Ok(Value::Bool(as_f64(&args[0], "Double/isNaN")?.is_nan()))
}

/// SPEC-PORT: `(Double/isInfinite x)` / `(Float/isInfinite x)` -- the
/// twin of `isNaN` above, and `clojure.spec.alpha/double-in`'s
/// `:infinite? false` option is written directly in terms of it
/// (`#(not (Double/isInfinite %))`). Measured on the JVM:
/// `(Double/isInfinite 1.0)` => `false`, `(Double/isInfinite (/ 1.0 0.0))`
/// => `true`, `(Double/isInfinite Double/NaN)` => `false`. `f64::
/// is_infinite` is exactly that predicate (NaN is not infinite), and like
/// `isNaN` it needs no precision-specific variant.
fn double_is_infinite(_interp: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    Ok(Value::Bool(as_f64(&args[0], "Double/isInfinite")?.is_infinite()))
}

/// `(Double/compare a b)`: reproduces `java.lang.Double.compare`'s TOTAL
/// order exactly -- not `<`/`>`, which treat `NaN` as incomparable and
/// `0.0 == -0.0`. Measured: `(Double/compare 1.0 2.0)` => `-1`,
/// `(Double/compare 2.0 1.0)` => `1`, `(Double/compare 1.0 1.0)` => `0`,
/// `(Double/compare 0.0 -0.0)` => `1`, `(Double/compare -0.0 0.0)` =>
/// `-1`, `(Double/compare Double/NaN 1.0)` => `1`, `(Double/compare 1.0
/// Double/NaN)` => `-1`, `(Double/compare Double/NaN Double/NaN)` => `0`.
/// The algorithm (matching `java.lang.Double`'s own source): ordinary `<`/
/// `>` first (handles every non-NaN, non-zero-sign case), then falls back
/// to comparing `to_bits()` as SIGNED `i64` -- which is exactly how Java
/// breaks the `0.0`-vs-`-0.0` tie (sign bit set => very negative as i64)
/// and defines `NaN` as strictly greatest (its bit pattern is a large
/// positive i64).
fn double_compare(_interp: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    let a = as_f64(&args[0], "Double/compare")?;
    let b = as_f64(&args[1], "Double/compare")?;
    if a < b {
        return Ok(Value::Int(-1));
    }
    if a > b {
        return Ok(Value::Int(1));
    }
    let ab = a.to_bits() as i64;
    let bb = b.to_bits() as i64;
    Ok(Value::Int(match ab.cmp(&bb) {
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Greater => 1,
    }))
}

/// `(Double/parseDouble s)`: measured `(Double/parseDouble "NaN")` =>
/// `##NaN`, `(Double/parseDouble "3.14")` => `3.14`. Rust's `f64::from_str`
/// accepts the same `"NaN"`/`"Infinity"`/signed-decimal/exponent grammar
/// (case-insensitively) real Clojure's `Double/parseDouble` does for every
/// probed shape, so it's used directly with no hand-rolled grammar.
fn double_parse_double(_interp: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    let s = as_str(&args[0], "Double/parseDouble")?;
    s.trim().parse::<f64>().map(Value::Float).map_err(|_| RjError::type_err(format!("Double/parseDouble: invalid input {s:?}")))
}

/// `(Float/parseFloat s)`: measured `(Float/parseFloat "NaN")` => `##NaN`,
/// `(Float/parseFloat "3.14")` => `3.14` (Float's own, shorter, `toString`
/// digits on the JVM). Parses as a real `f32` (not `f64`) FIRST, then
/// widens -- the JVM rounds to float precision before ever widening back
/// to double for arithmetic, and mova's single `Value::Float` needs the
/// same rounding to store the numerically-correct widened value (matches
/// this module's `Float/MAX_VALUE` field, which is `f32::MAX as f64` for
/// the identical reason).
fn float_parse_float(_interp: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    let s = as_str(&args[0], "Float/parseFloat")?;
    s.trim().parse::<f32>().map(|f| Value::Float(f as f64)).map_err(|_| RjError::type_err(format!("Float/parseFloat: invalid input {s:?}")))
}

/// `(Long/parseLong s)`: measured `(Long/parseLong "123")` => `123`,
/// `(Long/parseLong "-123")` => `-123`.
fn long_parse_long(_interp: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    let s = as_str(&args[0], "Long/parseLong")?;
    s.trim().parse::<i64>().map(Value::Int).map_err(|_| RjError::type_err(format!("Long/parseLong: invalid input {s:?}")))
}

/// `(Long/valueOf x)`: measured `(Long/valueOf Long/MAX_VALUE)` =>
/// `9223372036854775807`, `(Long/valueOf 1)` => `1`, `(Long/valueOf
/// "42")` => `42` -- both the boxing overload (`Long/valueOf(long)`, an
/// identity here since mova has one Int) and the string-parsing overload
/// (`Long/valueOf(String)`, identical to `Long/parseLong`) matter: the
/// vendored `numbers.clj` overflow-wraparound tests (`unchecked-add`/
/// `unchecked-multiply`/...) call this 27 times, always on an int arg, to
/// force a boxed (as opposed to compile-time-constant-folded) operand --
/// though those particular vendored forms stay unresolved regardless of
/// this fn (mova has no `unchecked-*` family yet; see
/// `tests/conformance/pending/numerics.corpus`), so `Long/valueOf` alone
/// does not newly unblock them.
fn long_value_of(_interp: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    match &args[0] {
        Value::Int(n) => Ok(Value::Int(*n)),
        Value::Str(s) => s.trim().parse::<i64>().map(Value::Int).map_err(|_| RjError::type_err(format!("Long/valueOf: invalid input {s:?}"))),
        other => Err(RjError::type_err(format!("Long/valueOf: expected a number or string, got {}", other.type_name()))),
    }
}

/// `(Integer/valueOf x)`: measured `(Integer/valueOf 5)` => `5`,
/// `(Integer/valueOf "5")` => `5`, and (the one vendored call site,
/// `rt.clj`) `(Integer/valueOf #"boom")` throws `IllegalArgumentException`
/// on the JVM -- reproduced here as an ordinary catchable `RjError` (mova's
/// `catch` is class-name-tolerant, not class-name-checking, so this still
/// satisfies a `(catch ...)` around it even though the concrete Rust error
/// carries no Java class name). The string overload parses as a genuine
/// 32-bit `i32` (not `i64`, unlike `Long/valueOf`'s string overload) --
/// `Integer/valueOf("99999999999")` really does throw
/// `NumberFormatException` on the JVM, a real int/long overload difference
/// worth keeping even though the vendored suite doesn't exercise it.
fn integer_value_of(_interp: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    match &args[0] {
        Value::Int(n) => Ok(Value::Int(*n)),
        Value::Str(s) => s.trim().parse::<i32>().map(|n| Value::Int(n as i64)).map_err(|_| RjError::type_err(format!("Integer/valueOf: invalid input {s:?}"))),
        other => Err(RjError::type_err(format!("Integer/valueOf: expected a number or string, got {}", other.type_name()))),
    }
}

/// `(Long/bitCount x)`: population count of `x`'s two's-complement 64-bit
/// representation -- measured `(Long/bitCount -1)` => `64` (every bit
/// set), `(Long/bitCount 0)` => `0`, `(Long/bitCount 255)` => `8`.
/// `i64::count_ones` (via the `u64` reinterpretation, to avoid sign-
/// extension surprises in the cast -- though `count_ones` is defined
/// identically on `i64` and `u64` since it only looks at the bit
/// pattern) is exactly `java.lang.Long.bitCount`'s algorithm.
fn long_bit_count(_interp: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    match &args[0] {
        Value::Int(n) => Ok(Value::Int((*n as u64).count_ones() as i64)),
        other => Err(RjError::type_err(format!("Long/bitCount: expected an int, got {}", other.type_name()))),
    }
}

/// `(System/currentTimeMillis)`: wall-clock epoch milliseconds, as a
/// `Long` -- measured `(class (System/currentTimeMillis))` =>
/// `java.lang.Long`. Deliberately excluded from
/// `tests/conformance/corpus/bit-ops.corpus` (not deterministic); only
/// its VALUE TYPE (`Value::Int`, matching `java.lang.Long`) is anything
/// this session can conform against the oracle. `UNIX_EPOCH` is always in
/// the past on any real clock, and mova has no 32-bit target, so the
/// `as i64` truncation of the `u128` millisecond count never actually
/// truncates in practice.
fn system_current_time_millis(_interp: &mut Interp, _args: &[Value]) -> Result<Value, RjError> {
    Ok(Value::Int(crate::clock::clock_epoch_ms() as i64))
}

/// lsp/host: `(System/nanoTime)` -- `clojure-lsp.shared/format-time-delta-ms`
/// and its `time`/`> stopwatch macros (`shared.clj`) call this to measure
/// `format`/`clean-ns` elapsed time; missing this static was a hard load
/// blocker for those namespaces.
fn system_nano_time(_interp: &mut Interp, _args: &[Value]) -> Result<Value, RjError> {
    Ok(Value::Int(crate::clock::clock_nano_time()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eval_ok(src: &str) -> Value {
        let mut interp = Interp::new();
        interp.eval_str("test", src).unwrap_or_else(|e| panic!("eval error for {src:?}: {e:?}"))
    }

    fn eval_err(src: &str) -> RjError {
        let mut interp = Interp::new();
        interp.eval_str("test", src).expect_err("expected an error")
    }

    /// mova/PLAN.md interop-census batch: `Character/digit`,
    /// `Character/valueOf`, `Integer/toString(radix)`,
    /// `Boolean/parseBoolean`, `Pattern/compile`+`quote`,
    /// `Matcher/quoteReplacement`, `URLDecoder/decode` -- one probe per
    /// static, each measured against real Java/Clojure semantics.
    #[test]
    fn interop_census_batch_matches_real_jvm() {
        assert_eq!(eval_ok(r#"(Character/digit \f 16)"#), Value::Int(15));
        assert_eq!(eval_ok(r#"(Character/digit \z 10)"#), Value::Int(-1));
        assert_eq!(eval_ok(r#"(Character/valueOf \a)"#), Value::Char('a'));
        assert_eq!(eval_ok("(Integer/toString -255 16)"), Value::Str("-ff".to_string().into()));
        assert_eq!(eval_ok("(Integer/toString 42 10)"), Value::Str("42".to_string().into()));
        assert_eq!(eval_ok(r#"(Boolean/parseBoolean "TRUE")"#), Value::Bool(true));
        assert_eq!(eval_ok(r#"(Boolean/parseBoolean "no")"#), Value::Bool(false));
        assert_eq!(eval_ok("(nil? (Thread/sleep 0))"), Value::Bool(true));
        assert_eq!(eval_ok("(nil? (System/gc))"), Value::Bool(true));
        assert_eq!(eval_ok(r#"(Pattern/quote "a.b*c")"#), Value::Str("a\\.b\\*c".to_string().into()));
        assert_eq!(
            eval_ok(r#"(Matcher/quoteReplacement "a$b\\c")"#),
            Value::Str("a\\$b\\\\c".to_string().into())
        );
        assert_eq!(eval_ok(r#"(regex? (Pattern/compile "a+"))"#), Value::Bool(true));
        assert_eq!(
            eval_ok(r#"(URLDecoder/decode "a%20b+c" "UTF-8")"#),
            Value::Str("a b c".to_string().into())
        );
    }

    #[test]
    fn long_fields_match_i64_extremes() {
        assert_eq!(eval_ok("Long/MAX_VALUE"), Value::Int(i64::MAX));
        assert_eq!(eval_ok("Long/MIN_VALUE"), Value::Int(i64::MIN));
    }

    #[test]
    fn distinct_classes_do_not_collide_on_shared_field_names() {
        assert_eq!(eval_ok("Long/MAX_VALUE"), Value::Int(9_223_372_036_854_775_807));
        assert_eq!(eval_ok("Integer/MAX_VALUE"), Value::Int(2_147_483_647));
        assert_eq!(eval_ok("Byte/MAX_VALUE"), Value::Int(127));
        assert_eq!(eval_ok("Short/MAX_VALUE"), Value::Int(32767));
    }

    #[test]
    fn double_special_values() {
        assert!(matches!(eval_ok("Double/POSITIVE_INFINITY"), Value::Float(f) if f == f64::INFINITY));
        assert!(matches!(eval_ok("Double/NEGATIVE_INFINITY"), Value::Float(f) if f == f64::NEG_INFINITY));
        assert!(matches!(eval_ok("Double/NaN"), Value::Float(f) if f.is_nan()));
    }

    #[test]
    fn double_min_value_is_numerically_exact_even_though_it_does_not_print_conformingly() {
        assert!(matches!(eval_ok("Double/MIN_VALUE"), Value::Float(f) if f.to_bits() == 1));
    }

    #[test]
    fn float_max_value_widens_f32_max_exactly() {
        assert!(matches!(eval_ok("Float/MAX_VALUE"), Value::Float(f) if f == f32::MAX as f64));
    }

    #[test]
    fn double_is_nan_and_float_is_nan() {
        assert_eq!(eval_ok("(Double/isNaN 1.0)"), Value::Bool(false));
        assert_eq!(eval_ok("(Double/isNaN Double/NaN)"), Value::Bool(true));
        assert_eq!(eval_ok("(Float/isNaN Float/NaN)"), Value::Bool(true));
    }

    #[test]
    fn double_compare_total_order() {
        assert_eq!(eval_ok("(Double/compare 1.0 2.0)"), Value::Int(-1));
        assert_eq!(eval_ok("(Double/compare 2.0 1.0)"), Value::Int(1));
        assert_eq!(eval_ok("(Double/compare 1.0 1.0)"), Value::Int(0));
        assert_eq!(eval_ok("(Double/compare 0.0 -0.0)"), Value::Int(1));
        assert_eq!(eval_ok("(Double/compare -0.0 0.0)"), Value::Int(-1));
        assert_eq!(eval_ok("(Double/compare Double/NaN 1.0)"), Value::Int(1));
        assert_eq!(eval_ok("(Double/compare 1.0 Double/NaN)"), Value::Int(-1));
        assert_eq!(eval_ok("(Double/compare Double/NaN Double/NaN)"), Value::Int(0));
    }

    #[test]
    fn parse_methods() {
        assert!(matches!(eval_ok(r#"(Double/parseDouble "NaN")"#), Value::Float(f) if f.is_nan()));
        assert_eq!(eval_ok(r#"(Double/parseDouble "2.5")"#), Value::Float(2.5));
        assert!(matches!(eval_ok(r#"(Float/parseFloat "NaN")"#), Value::Float(f) if f.is_nan()));
        assert_eq!(eval_ok(r#"(Long/parseLong "123")"#), Value::Int(123));
        assert_eq!(eval_ok(r#"(Long/parseLong "-123")"#), Value::Int(-123));
    }

    #[test]
    fn value_of_boxes_or_parses() {
        assert_eq!(eval_ok("(Long/valueOf Long/MAX_VALUE)"), Value::Int(i64::MAX));
        assert_eq!(eval_ok("(Long/valueOf 1)"), Value::Int(1));
        assert_eq!(eval_ok(r#"(Long/valueOf "42")"#), Value::Int(42));
        assert_eq!(eval_ok("(Integer/valueOf 5)"), Value::Int(5));
        assert_eq!(eval_ok(r#"(Integer/valueOf "5")"#), Value::Int(5));
    }

    #[test]
    fn integer_value_of_rejects_a_non_numeric_non_string_arg() {
        eval_err(r#"(Integer/valueOf #"boom")"#);
    }

    #[test]
    fn boolean_fields() {
        assert_eq!(eval_ok("Boolean/TRUE"), Value::Bool(true));
        assert_eq!(eval_ok("Boolean/FALSE"), Value::Bool(false));
    }

    #[test]
    fn long_value_of_composes_with_ordinary_arithmetic() {
        // `unchecked-add`/`unchecked-multiply` (tests/clojure-suite/vendor/
        // numbers.clj:782-813's actual pattern) aren't implemented in mova
        // yet (tests/conformance/pending/numerics.corpus), so this checks
        // `Long/valueOf`'s boxing/identity behavior via plain `+` instead.
        assert_eq!(eval_ok("(+ (Long/valueOf Long/MAX_VALUE) 0)"), Value::Int(i64::MAX));
        assert_eq!(eval_ok("(= (Long/valueOf 1) 1)"), Value::Bool(true));
    }

    #[test]
    fn long_bit_count_matches_oracle() {
        assert_eq!(eval_ok("(Long/bitCount -1)"), Value::Int(64));
        assert_eq!(eval_ok("(Long/bitCount 0)"), Value::Int(0));
        assert_eq!(eval_ok("(Long/bitCount 255)"), Value::Int(8));
    }

    #[test]
    fn current_time_millis_returns_a_long() {
        // Not deterministic (see the fn's own doc comment) -- only the
        // TYPE and rough sanity of the value are checked here.
        assert!(matches!(eval_ok("(System/currentTimeMillis)"), Value::Int(n) if n > 0));
    }
}
