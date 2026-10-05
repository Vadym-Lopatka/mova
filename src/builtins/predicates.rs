//! `nil? some? true? false? number? int? float? string? keyword? symbol?
//! vector? map? set? list? seq? fn? even? odd? pos? neg? zero? coll?
//! boolean char? not`, plus R4's `identical? integer? sequential?`.
//!
//! Deviation: `empty?` is listed in both this module's and collections.rs's
//! header comment in ARCHITECTURE.md; it's implemented once, in
//! collections.rs (it needs the lazy-aware `uncons` helper that lives
//! there).

use std::sync::Arc;

use crate::builtins::{reg, reg_unmeta, ArityHint};
use crate::error::{JvmClass, RjError};
use crate::eval::Interp;
use crate::types::ClassVal;
use crate::value::Value;

/// `identical?`: reference identity, matched per-variant. Primitives that
/// have no meaningful separate "identity" from their value (`Nil`/`Bool`/
/// `Int`/`Float`/`Char`) and interned-style scalars (`Keyword`/`Sym`, which
/// mova -- like real Clojure keywords -- treats as value-equal rather than
/// pointer-tracked) compare by value; every `Arc`-backed reference type
/// (including `Str`, an `Arc`-backed handle with its own `ptr_eq`) compares
/// by pointer identity; the persistent collections (`List`/`Vector`/`Map`/`Set`)
/// delegate to `imbl`'s own `ptr_eq` (structural-sharing-aware: true iff
/// the two collections share their root node, exactly analogous to two
/// object references naming the same JVM collection instance). Different
/// variants are never identical.
fn identical(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Nil, Value::Nil) => true,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Int(x), Value::Int(y)) => x == y,
        (Value::Float(x), Value::Float(y)) => x == y,
        (Value::Char(x), Value::Char(y)) => x == y,
        (Value::Keyword(x), Value::Keyword(y)) => x == y,
        (Value::Sym(x), Value::Sym(y)) => x == y,
        (Value::Str(x), Value::Str(y)) => crate::value::Str::ptr_eq(x, y),
        (Value::List(x), Value::List(y)) => x.ptr_eq(y),
        (Value::Vector(x), Value::Vector(y)) => x.ptr_eq(y),
        // S7: same-allocation identity, like every other structural
        // variant. NOT paired with `Vector` -- `identical?` is reference
        // identity, and an entry and a vector are different objects even
        // when `=`.
        (Value::MapEntry(x), Value::MapEntry(y)) => x.ptr_eq(y),
        (Value::Map(x), Value::Map(y)) => x.ptr_eq(y),
        // W3: identity of the underlying HOST object (the `obj` Arc), not
        // the per-`wrap_struct`-call wrapper cell -- two independent
        // `wrap_struct` calls over the SAME `Arc<T>` are `identical?`,
        // matching every other cell-backed variant's "same allocation"
        // contract from the host's point of view (`wrap_struct` is cheap
        // and expected to be called freely, e.g. once per `Engine::call`,
        // not memoized by the host -- see `crate::embed::host`'s doc).
        (Value::HostStruct(x), Value::HostStruct(y)) => Arc::ptr_eq(&x.obj, &y.obj),
        (Value::LazyMap(x), Value::LazyMap(y)) => Arc::ptr_eq(x, y),
        (Value::Set(x), Value::Set(y)) => x.ptr_eq(y),
        (Value::Fn(x), Value::Fn(y)) => Arc::ptr_eq(x, y),
        (Value::Native(x), Value::Native(y)) => Arc::ptr_eq(x, y),
        (Value::Macro(x), Value::Macro(y)) => Arc::ptr_eq(x, y),
        (Value::Atom(x), Value::Atom(y)) => Arc::ptr_eq(x, y),
        (Value::Lazy(x), Value::Lazy(y)) => Arc::ptr_eq(x, y),
        (Value::Future(x), Value::Future(y)) => Arc::ptr_eq(x, y),
        (Value::Promise(x), Value::Promise(y)) => Arc::ptr_eq(x, y),
        (Value::Delay(x), Value::Delay(y)) => Arc::ptr_eq(x, y),
        (Value::Channel(x), Value::Channel(y)) => Arc::ptr_eq(x, y),
        // S7 (tail wave): exception/record instances and host-shim
        // instances are `Arc`-backed like every other reference type above
        // -- measured missing (`(let [e (Exception. "x")] (identical? e
        // e))` was `false`), which is what surfaced this: delays.clj's
        // `saves-exceptions` caches the SAME `Value::Inst` and expects
        // `identical?` to see it, exactly like the JVM's object identity
        // does for a caught `Exception` instance.
        (Value::Inst(x), Value::Inst(y)) => Arc::ptr_eq(x, y),
        (Value::HostInst(x), Value::HostInst(y)) => Arc::ptr_eq(x, y),
        (Value::Flow(x), Value::Flow(y)) => Arc::ptr_eq(x, y),
        // D5: the `Arc`-backed variants this fn had simply never grown an
        // arm for, so `(identical? x x)` was FALSE for every one of them
        // -- a plain bug, not a design choice, and the same one the S7
        // comment above records fixing for `Inst`.
        //
        // Surfaced by the vendored `clojure.pprint`, whose pretty-printer
        // decides where to break a line with `ancestor?`, an
        // `identical?`-walk up a chain of `defstruct` logical blocks
        // (`pretty_writer.clj`). With `StructMap` missing here that walk
        // always answered false, so every buffered token counted toward
        // the current section and long collections broke in the wrong
        // places -- e.g. a 20-entry map printed its key and value on
        // separate lines. Measured against the oracle both before and
        // after.
        //
        // Every arm below is the same "same allocation" rule the arms
        // above use, applied to the rest of the enum. `Queue`/`MapEntry`
        // are `PVec`-backed, so they get `imbl`'s structural `ptr_eq`
        // like `Vector` does; the rest are plain `Arc`s.
        (Value::StructMap(x), Value::StructMap(y)) => Arc::ptr_eq(x, y),
        (Value::StructBasis(x), Value::StructBasis(y)) => Arc::ptr_eq(x, y),
        (Value::SortedMap(x), Value::SortedMap(y)) => Arc::ptr_eq(x, y),
        (Value::SortedSet(x), Value::SortedSet(y)) => Arc::ptr_eq(x, y),
        (Value::TypedVec(x), Value::TypedVec(y)) => Arc::ptr_eq(x, y),
        (Value::VecSeq(x), Value::VecSeq(y)) => Arc::ptr_eq(x, y),
        (Value::Queue(x), Value::Queue(y)) => x.ptr_eq(y),
        (Value::Volatile(x), Value::Volatile(y)) => Arc::ptr_eq(x, y),
        (Value::Var(x), Value::Var(y)) => Arc::ptr_eq(x, y),
        (Value::Regex(x), Value::Regex(y)) => Arc::ptr_eq(x, y),
        (Value::Matcher(x), Value::Matcher(y)) => Arc::ptr_eq(x, y),
        (Value::Timer(x), Value::Timer(y)) => Arc::ptr_eq(x, y),
        (Value::Array(x), Value::Array(y)) => Arc::ptr_eq(x, y),
        (Value::Reduced(x), Value::Reduced(y)) => Arc::ptr_eq(x, y),
        // S6: `Class` values -- mirrors `Value`'s own `=` impl exactly
        // (see `value.rs`'s `(Class(a), Class(b))` arm), which is the
        // normative statement of per-`ClassVal`-variant identity here:
        // builtins/interfaces are interned per name (so name equality
        // AND `Arc::ptr_eq` agree for them -- name is the honest
        // statement of the rule, same reasoning as `=`'s own comment),
        // user classes are `Arc`-pointer identity (re-`defrecord`/
        // `deftype` mints a fresh `TypeDef`). Measured: `(identical?
        // (Class/forName "java.lang.Long") Long)` => `true` (both
        // resolve through the same interned builtin-class cache);
        // `(identical? Long Double)` => `false`.
        (Value::Class(a), Value::Class(b)) => match (a.as_ref(), b.as_ref()) {
            (
                ClassVal::Builtin { name: n1, .. },
                ClassVal::Builtin { name: n2, .. },
            ) => n1 == n2,
            (ClassVal::User(t1), ClassVal::User(t2)) => Arc::ptr_eq(t1, t2),
            (
                ClassVal::Interface { name: n1 },
                ClassVal::Interface { name: n2 },
            ) => n1 == n2,
            _ => false,
        },
        _ => false,
    }
}

/// The sign test `zero?`/`pos?`/`neg?` share, as an `Ordering` against
/// zero. S5 widened it to the whole tower (measured: `(zero? 0N)`,
/// `(zero? 0M)`, `(pos? 1M)`, `(pos? 1/2)`, `(neg? -1/2)` are all true,
/// and were all TYPE ERRORS in mova before this) -- but it stays a SIGN
/// rather than an `f64` comparison, because a BigInt past 2^53 or a
/// BigDec with a huge scale must not have its sign decided by a lossy
/// widening.
///
/// `NaN` is not this function's problem: it is neither zero, positive nor
/// negative, which no single `Ordering` can express, so all three callers
/// test [`is_nan`] first and never get here with one.
fn sign_checked(v: &Value, op: &str) -> Result<std::cmp::Ordering, RjError> {
    use std::cmp::Ordering;
    fn of_sign(s: num_bigint::Sign) -> Ordering {
        match s {
            num_bigint::Sign::Minus => Ordering::Less,
            num_bigint::Sign::NoSign => Ordering::Equal,
            num_bigint::Sign::Plus => Ordering::Greater,
        }
    }
    Ok(match v {
        Value::Int(i) => i.cmp(&0),
        Value::Float(f) => f
            .partial_cmp(&0.0)
            .expect("callers reject NaN before calling sign_checked"),
        Value::BigInt(b) | Value::BigInteger(b) => of_sign(b.0.sign()),
        // A `Ratio`'s sign lives entirely on its numerator (`RatioVal`
        // keeps `den > 0` by construction).
        Value::Ratio(r) => of_sign(r.numer().sign()),
        Value::BigDec(d) => d.signum().cmp(&0),
        other => {
            return Err(RjError::type_err(format!(
                "{op}: expected a number, got {}",
                other.type_name()
            )))
        }
    })
}

/// True iff `v` is a `NaN`. `zero?`/`pos?`/`neg?` are all false on one
/// (measured, and what Java's `==`/`>`/`<` do), so each tests this before
/// consulting [`sign_checked`].
fn is_nan(v: &Value) -> bool {
    matches!(v, Value::Float(f) if f.is_nan())
}

/// The INTEGER category: `Long`, `clojure.lang.BigInt`, and
/// `java.math.BigInteger` (measured: `(integer? 7N)` and
/// `(integer? (biginteger 5))` are both true; `(integer? 3.0)`,
/// `(integer? 1/2)` and `(integer? 1M)` are all false).
pub(crate) fn is_integer(v: &Value) -> bool {
    matches!(v, Value::Int(_) | Value::BigInt(_) | Value::BigInteger(_))
}

/// The low bit of an integer-category value, or the exact
/// `IllegalArgumentException` message real Clojure's `even?`/`odd?` throw
/// for everything else (measured: `(even? 1/2)` => `Argument must be an
/// integer: 1/2`; `(even? 4M)` => `Argument must be an integer: 4` -- note
/// the BigDecimal renders through `str`, hence without its `M`).
fn int_parity(v: &Value) -> Result<u32, RjError> {
    match v {
        Value::Int(n) => Ok((n & 1) as u32),
        Value::BigInt(b) | Value::BigInteger(b) => Ok(u32::from(b.0.bit(0))),
        // W3a: the doc above already recorded that this is real Clojure's
        // `IllegalArgumentException` message; the CLASS is now carried too,
        // instead of defaulting to `ErrorKind::TypeErr`'s
        // `ClassCastException`.
        other => Err(RjError::type_err(format!(
            "Argument must be an integer: {}",
            crate::printer::display_str(other)
        ))
        .with_class(JvmClass::IllegalArgument)),
    }
}

/// `zero?`, shared verbatim with `compile::exec`'s `IntrinOp::Zero` (v0.3 /
/// S3) so the compiled tier cannot drift on the float blend (`(zero? 0.0)`
/// is `true`) or on the error string.
pub(crate) fn zero1(v: &Value) -> Result<Value, RjError> {
    if is_nan(v) {
        return Ok(Value::Bool(false));
    }
    Ok(Value::Bool(sign_checked(v, "zero?")? == std::cmp::Ordering::Equal))
}

/// `not`, shared with `IntrinOp::Not`. Truthiness is `nil`/`false` only,
/// per `Value::truthy`.
pub(crate) fn not1(v: &Value) -> Value {
    Value::Bool(!v.truthy())
}

/// S4/1D: shared by the `seqable?` registration below AND
/// `builtins::arrays`'s typed-factory 2-arity discrimination (`(int-array 5
/// [1 2 3])` fills from a seq vs. `(int-array 5 7)` broadcasts a scalar --
/// telling those apart needs the exact same "is this seqable" answer
/// `seqable?` itself gives). Kept in one place so the two callers can never
/// silently disagree about what counts as seqable.
pub(crate) fn is_seqable_value(v: &Value) -> bool {
    matches!(
        v,
        Value::Nil
            | Value::List(_)
            | Value::Vector(_)
            // S7: measured `(seqable? (first {:a 1}))` => `true`.
            | Value::MapEntry(_)
            | Value::Map(_)
            | Value::Set(_)
            | Value::Str(_)
            | Value::Lazy(_)
            | Value::HostStruct(_) | Value::LazyMap(_)
            | Value::StructMap(_)
            | Value::Array(_)
            // Wave-C small sweep: `coll?`/`map?`/`set?`/`seqable?` in this
            // module never learned about `SortedMap`/`SortedSet`/`TypedVec`,
            // even though `types.rs`'s own `is_map`/`is_set`/`is_coll` (used
            // by `instance?`) already did. Measured on the oracle:
            // `(seqable? (sorted-set))` and `(seqable? (vector-of :int 1))`
            // are both `true`.
            | Value::SortedMap(_)
            | Value::SortedSet(_)
            | Value::TypedVec(_)
    ) || matches!(v, Value::Inst(inst) if inst.tdef.is_record)
}

pub fn register(i: &mut Interp) {
    reg(i, "nil?", ArityHint::Exact(1), |_i, args| Ok(Value::Bool(matches!(args[0], Value::Nil))));
    reg(i, "some?", ArityHint::Exact(1), |_i, args| Ok(Value::Bool(!matches!(args[0], Value::Nil))));
    reg(i, "true?", ArityHint::Exact(1), |_i, args| Ok(Value::Bool(matches!(args[0], Value::Bool(true)))));
    reg(i, "false?", ArityHint::Exact(1), |_i, args| Ok(Value::Bool(matches!(args[0], Value::Bool(false)))));
    // SPEC-B-bignum-wiring.md §5: `number?` is true for all of Clojure's
    // numeric tower, which now includes the three bignum variants (M5
    // arithmetic on them is out of scope here -- only the type predicate).
    reg(i, "number?", ArityHint::Exact(1), |_i, args| {
        Ok(Value::Bool(matches!(
            args[0],
            Value::Int(_)
                | Value::Float(_)
                | Value::BigInt(_)
                // S5: `java.math.BigInteger` is a `java.lang.Number` too
                // (measured `(number? (biginteger 5))` => `true`).
                | Value::BigInteger(_)
                | Value::Ratio(_)
                | Value::BigDec(_)
        )))
    });
    reg(i, "int?", ArityHint::Exact(1), |_i, args| Ok(Value::Bool(matches!(args[0], Value::Int(_)))));
    reg(i, "float?", ArityHint::Exact(1), |_i, args| Ok(Value::Bool(matches!(args[0], Value::Float(_)))));
    reg(i, "string?", ArityHint::Exact(1), |_i, args| Ok(Value::Bool(matches!(args[0], Value::Str(_)))));
    reg(i, "keyword?", ArityHint::Exact(1), |_i, args| Ok(Value::Bool(matches!(args[0], Value::Keyword(_)))));
    reg_unmeta(i, "symbol?", ArityHint::Exact(1), |_i, args| Ok(Value::Bool(matches!(args[0], Value::Sym(_)))));
    // S7: a map entry answers `true` -- measured, `(vector? (first {:a
    // 1}))` is `true` on the oracle, because `clojure.lang.MapEntry
    // extends AMapEntry extends APersistentVector`. (A `vector-of` typed
    // vector still answers `false` here, matching the oracle's own
    // `(vector? (vector-of :int 1))` => `false`; that asymmetry is
    // pre-existing and unrelated.)
    reg_unmeta(i, "vector?", ArityHint::Exact(1), |_i, args| {
        Ok(Value::Bool(matches!(args[0], Value::Vector(_) | Value::MapEntry(_))))
    });
    reg_unmeta(i, "map?", ArityHint::Exact(1), |_i, args| {
        Ok(Value::Bool(
            // Wave-C small sweep: `SortedMap` was missing here even though
            // `types.rs`'s `is_map` (used by `instance?`) already had it.
            // Measured: `(map? (sorted-map))` => `true`.
            matches!(&args[0], Value::Map(_) | Value::HostStruct(_) | Value::LazyMap(_) | Value::StructMap(_) | Value::SortedMap(_))
                // S3 (measured): records ARE maps to `map?`.
                || matches!(&args[0], Value::Inst(inst) if inst.tdef.is_record),
        ))
    });
    // Wave-C small sweep: `SortedSet` was missing here even though
    // `types.rs`'s `is_set` (used by `instance?`) already had it.
    // Measured: `(set? (sorted-set))` => `true`.
    reg_unmeta(i, "set?", ArityHint::Exact(1), |_i, args| {
        Ok(Value::Bool(matches!(args[0], Value::Set(_) | Value::SortedSet(_))))
    });
    // S7 (was S6's honest-`false`): `map-entry?` is now STRUCTURALLY
    // exact, answering off `Value::MapEntry` -- the representation S6
    // did not have. Measured on the oracle, all four rows now match:
    // `(map-entry? (first {:a 1}))` and `(map-entry? (first (sorted-map
    // :a 1)))` are `true`, `(map-entry? [:a 1])` and `(map-entry? nil)`
    // are `false`. The `tests/conformance/DEVIATIONS.md` row this
    // predicate carried is RETIRED by this branch.
    //
    // Answers off the VARIANT, never off "is a 2-element vector": that
    // distinction is the whole point, and is what `(map-entry? [:a 1])`
    // => `false` measures. Kept a one-liner in step with
    // `crate::types::is_map_entry` (which `instance?` uses); the two must
    // agree, and both are `matches!` on the same variant so they cannot
    // drift.
    reg_unmeta(i, "map-entry?", ArityHint::Exact(1), |_i, args| {
        Ok(Value::Bool(matches!(args[0], Value::MapEntry(_))))
    });
    reg_unmeta(i, "list?", ArityHint::Exact(1), |_i, args| Ok(Value::Bool(matches!(args[0], Value::List(_)))));
    // Clojure: lists and lazy seqs are seqs; vectors/maps/sets are not.
    reg_unmeta(i, "seq?", ArityHint::Exact(1), |_i, args| {
        Ok(Value::Bool(matches!(args[0], Value::List(_) | Value::Lazy(_))))
    });
    reg_unmeta(i, "fn?", ArityHint::Exact(1), |_i, args| {
        Ok(Value::Bool(matches!(args[0], Value::Fn(_) | Value::Native(_))))
    });
    // `ifn?`: "implements IFn" -- broader than `fn?`. Oracle-measured
    // (1.13.0-alpha6, 2026-08-20): true for fns/natives, keywords, symbols,
    // maps (plain AND sorted), vectors, sets (plain AND sorted), and vars;
    // false for strings, numbers, nil, and atoms. This mirrors mova's own
    // `apply_value` invocable set (`Value::Fn`/`Native`/`Keyword`/`Map`/
    // `Set`/`Vector`/`Var`, `Meta` transparent -- see eval/apply.rs) PLUS
    // `Sym`/`SortedMap`/`SortedSet`, which the oracle table says are
    // invokable even though mova's `apply_value` doesn't dispatch them yet
    // -- the oracle table is normative per the task spec, so it wins here.
    // Records (`Value::HostStruct` / record `Value::Inst`) are maps to
    // `map?` but measured `false` for `ifn?` on the JVM (plain `defrecord`
    // does not implement `IFn`), so they're deliberately excluded.
    reg_unmeta(i, "ifn?", ArityHint::Exact(1), |_i, args| {
        Ok(Value::Bool(matches!(
            args[0],
            Value::Fn(_)
                | Value::Native(_)
                | Value::Keyword(_)
                | Value::Sym(_)
                | Value::Map(_)
                | Value::SortedMap(_)
                // C2 (defstruct), measured: `(s :a)` works, so `ifn?` must too.
                | Value::StructMap(_)
                | Value::Vector(_)
                // S7: measured `(ifn? (first {:a 1}))` => `true` (and
                // `eval::apply` really does invoke it by index).
                | Value::MapEntry(_)
                | Value::Set(_)
                | Value::SortedSet(_)
                | Value::Var(_)
        )))
    });
    reg(i, "char?", ArityHint::Exact(1), |_i, args| Ok(Value::Bool(matches!(args[0], Value::Char(_)))));
    // S6: `var?` -- true only for a `Value::Var` (`#'sym`/`(var sym)`),
    // false for everything else including the value the var HOLDS
    // (measured: `(var? #'+)` => `true`, `(var? +)` => `false`).
    reg(i, "var?", ArityHint::Exact(1), |_i, args| Ok(Value::Bool(matches!(args[0], Value::Var(_)))));
    reg_unmeta(i, "coll?", ArityHint::Exact(1), |interp, args| {
        // W3f (small-tail sweep): checked BEFORE forcing -- real
        // `coll?` is a pure `instance? clojure.lang.IPersistentCollection`
        // check (`ISeq extends IPersistentCollection`, and `LazySeq`
        // implements `ISeq` REGARDLESS of what its body evaluates to), so
        // it never forces its argument at all. Forcing first (the
        // pre-existing code below, kept for every other shape) silently
        // broke this for a `Value::Lazy` whose body realizes to `Nil`
        // (measured: `(coll? (lazy-seq nil))` was `false`, want `true` --
        // `predicates.clj`'s `test-type-preds` matrix catches exactly this
        // cell; `seq?`'s neighboring registration already gets this right
        // by matching `Value::Lazy` directly, unforced). A `Lazy` whose
        // body has SIDE EFFECTS must not run them just to answer `coll?`,
        // which forcing-first would also do wrong.
        if matches!(args[0], Value::Lazy(_)) {
            return Ok(Value::Bool(true));
        }
        let v = interp.force(&args[0])?;
        Ok(Value::Bool(matches!(
            v,
            // S7: measured `(coll? (first {:a 1}))` => `true`.
            // Union merge with C2's `StructMap` arm -- independent adds.
            Value::List(_)
                | Value::Vector(_)
                | Value::MapEntry(_)
                | Value::Map(_)
                | Value::Set(_)
                | Value::HostStruct(_) | Value::LazyMap(_)
                | Value::StructMap(_)
                // Wave-C small sweep: `SortedMap`/`SortedSet`/`TypedVec` were
                // missing here even though `types.rs`'s `is_coll` (used by
                // `instance?`) already had them. Measured: `(coll? (sorted-set))`,
                // `(coll? (sorted-map))`, `(coll? (vector-of :int 1))` all `true`.
                | Value::SortedMap(_)
                | Value::SortedSet(_)
                | Value::TypedVec(_)
                // C10: measured `(coll? clojure.lang.PersistentQueue/EMPTY)`
                // => `true`. NOTE: this `coll?` match and `types::is_coll`
                // (which backs `instance? clojure.lang.IPersistentCollection`)
                // are two SEPARATE, pre-existing implementations that already
                // disagree on SortedMap/SortedSet/TypedVec/Lazy -- a
                // documented gap (see tests/conformance/pending/
                // collections.corpus's "S7 (MapEntry branch...)" note),
                // NOT introduced or widened here; only `Queue` is added to
                // both, matching each other for the one type this wave owns.
                | Value::Queue(_)
        )))
    });
    reg(i, "boolean", ArityHint::Exact(1), |_i, args| Ok(Value::Bool(args[0].truthy())));
    reg(i, "not", ArityHint::Exact(1), |_i, args| Ok(not1(&args[0])));

    reg(i, "identical?", ArityHint::Exact(2), |_i, args| Ok(Value::Bool(identical(&args[0], &args[1]))));
    // JVM Clojure's `integer?` is true for exact whole-number types (Long/
    // Integer/BigInt) only -- `Float` is never `integer?` even at a whole
    // value like `3.0` (that's `(== (int? ...))` territory, not this).
    // SPEC-B-bignum-wiring.md §5: measured `(integer? 7N)` is true, so
    // `Value::BigInt` joins `Value::Int` here. `int?` (a separate,
    // pre-existing predicate above) is left untouched -- the spec is
    // silent on it, so per the "don't guess beyond the spec" rule this
    // deliberately does NOT extend `int?` to `BigInt` even though real
    // Clojure's `int?` likely also covers `clojure.lang.BigInt`.
    reg(i, "integer?", ArityHint::Exact(1), |_i, args| {
        Ok(Value::Bool(is_integer(&args[0])))
    });
    // S5 (transcript rows 57-62, 90; plus the measured negative rows
    // `(ratio? 1N)` => false, `(rational? 1.5)` => false,
    // `(decimal? 1)` => false):
    //
    //   ratio?     -- exactly `clojure.lang.Ratio`.
    //   decimal?   -- exactly `java.math.BigDecimal`.
    //   rational?  -- `integer? or ratio? or decimal?`, which is
    //                 `clojure.core/rational?` verbatim; note it IS true
    //                 for a BigDecimal (measured `(rational? 1.5M)` =>
    //                 `true`) and false for every Double.
    reg(i, "ratio?", ArityHint::Exact(1), |_i, args| {
        Ok(Value::Bool(matches!(args[0], Value::Ratio(_))))
    });
    reg(i, "decimal?", ArityHint::Exact(1), |_i, args| {
        Ok(Value::Bool(matches!(args[0], Value::BigDec(_))))
    });
    reg(i, "rational?", ArityHint::Exact(1), |_i, args| {
        Ok(Value::Bool(matches!(
            args[0],
            Value::Ratio(_) | Value::BigDec(_)
        ) || is_integer(&args[0])))
    });
    // Lists and lazy seqs are sequential (ordered, steppable); vectors are
    // too (ordered, indexed); maps/sets/strings/nil are not.
    reg_unmeta(i, "sequential?", ArityHint::Exact(1), |_i, args| {
        // S7: measured `(sequential? (first {:a 1}))` => `true`
        // (`clojure.lang.Sequential` is on `MapEntry`'s supers list).
        // C10: measured `(sequential? clojure.lang.PersistentQueue/EMPTY)`
        // => `true`.
        Ok(Value::Bool(matches!(
            args[0],
            Value::List(_) | Value::Vector(_) | Value::MapEntry(_) | Value::Lazy(_) | Value::Queue(_)
        )))
    });

    // S4/1D: `seqable?` -- true for `nil` and anything `seq`/`Interp::
    // seq_items` accepts without erroring (measured: `(seqable? (into-array
    // [1 2 3]))` true, matching every other collection-ish shape here).
    // `Fn`/`Native`/numbers/keywords/etc. all correctly fall to `false` by
    // not appearing in this list, same shape as `coll?`/`sequential?`
    // above.
    reg_unmeta(i, "seqable?", ArityHint::Exact(1), |_i, args| Ok(Value::Bool(is_seqable_value(&args[0]))));

    // S4/1D: `indexed?` -- `clojure.lang.Indexed` membership. Only
    // `Vector` implements it in mova today (measured: `(indexed? (into-array
    // [1 2 3]))` false -- a JVM array is NOT `clojure.lang.Indexed`, even
    // though `aget`/`alength` give it O(1) random access by a different
    // route; lists/lazy-seqs/strings/maps/sets aren't `Indexed` either).
    reg(i, "indexed?", ArityHint::Exact(1), |_i, args| {
        // S7: measured `(indexed? (first {:a 1}))` => `true`.
        Ok(Value::Bool(matches!(args[0], Value::Vector(_) | Value::MapEntry(_))))
    });

    // C3c (sequences.clj's `test-seqs-implements-iobj`): `clojure.lang.
    // Reversible` membership -- `rseq`'s gate (`reversible?` didn't exist
    // at all before this task; the deftest's `(when (reversible? coll)
    // ...)` unconditionally threw "Unable to resolve symbol" for every
    // `coll`, aborting the whole `doseq` after the FIRST iteration).
    // Measured on 1.13.0-alpha6: `true` for a plain vector, a map entry
    // (`(reversible? (first {:a 1}))` -- `MapEntry` extends
    // `APersistentVector`, same "a MapEntry IS a 2-vector" precedent
    // `is_vector`/`sequential?` above already follow), `vector-of`
    // (`clojure.core.Vec`), `sorted-map`, `sorted-set`; `false` for a
    // plain map/set/queue/list (none of those implement `Reversible`).
    reg_unmeta(i, "reversible?", ArityHint::Exact(1), |_i, args| {
        Ok(Value::Bool(matches!(
            args[0],
            Value::Vector(_) | Value::MapEntry(_) | Value::TypedVec(_) | Value::SortedMap(_) | Value::SortedSet(_)
        )))
    });

    // S4/1D: `counted?` -- `clojure.lang.Counted` membership (O(1)
    // `count`, as opposed to `count` walking a seq). Measured: `(counted?
    // (into-array [1 2 3]))` is false in real Clojure (arrays answer
    // `alength` in O(1) but don't implement `Counted`); `Vector`/`Map`/
    // `Set`/`Str` all have mova's own O(1) `count` path (see
    // `collections.rs`'s `count` registration) and are `Counted` on the
    // JVM too.
    reg_unmeta(i, "counted?", ArityHint::Exact(1), |_i, args| {
        Ok(Value::Bool(matches!(
            args[0],
            // S7: measured `(counted? (first {:a 1}))` => `true`; mova's
            // `count` answers it in O(1) (a constant `2`) too.
            Value::Vector(_) | Value::MapEntry(_) | Value::Map(_) | Value::Set(_) | Value::Str(_) | Value::List(_)
        )))
    });

    // S4/1D: `bytes?` -- true ONLY for a `byte-array` (measured: `(bytes?
    // (byte-array 0))` true, `(bytes? (int-array 0))` false -- every other
    // array kind, including every non-array `Value`, is false).
    reg(i, "bytes?", ArityHint::Exact(1), |_i, args| {
        Ok(Value::Bool(
            matches!(&args[0], Value::Array(arr) if arr.kind == crate::value::ArrayKind::Byte),
        ))
    });

    // S5: `even?`/`odd?` are `clojure.core`'s
    // `(if (integer? n) (zero? (bit-and n 1)) (throw (IllegalArgumentException.
    // (str "Argument must be an integer: " n))))` -- so they accept the
    // WHOLE integer category, `BigInt`/`BigInteger` included (transcript
    // rows 28-29), and reject a Ratio/BigDec/Double with that exact
    // message (measured `(even? 1/2)` => `IllegalArgumentException:
    // Argument must be an integer: 1/2`, `(even? 4M)` => `... : 4` -- note
    // the BigDecimal renders through `str`, hence without the `M`).
    reg(i, "even?", ArityHint::Exact(1), |_i, args| Ok(Value::Bool(int_parity(&args[0])? == 0)));
    reg(i, "odd?", ArityHint::Exact(1), |_i, args| Ok(Value::Bool(int_parity(&args[0])? != 0)));
    reg(i, "pos?", ArityHint::Exact(1), |_i, args| {
        if is_nan(&args[0]) {
            return Ok(Value::Bool(false));
        }
        Ok(Value::Bool(sign_checked(&args[0], "pos?")? == std::cmp::Ordering::Greater))
    });
    reg(i, "neg?", ArityHint::Exact(1), |_i, args| {
        if is_nan(&args[0]) {
            return Ok(Value::Bool(false));
        }
        Ok(Value::Bool(sign_checked(&args[0], "neg?")? == std::cmp::Ordering::Less))
    });
    // S5: measured `(pos-int? 1N)`, `(nat-int? 1N)`, `(int? 1N)` are ALL
    // false -- this family is `java.lang.Long`/`Integer`-only and does NOT
    // extend to `clojure.lang.BigInt` (unlike `integer?`, which does).
    // `double?` is likewise exactly `java.lang.Double`, so
    // `(double? 1.5M)` is false.
    reg(i, "pos-int?", ArityHint::Exact(1), |_i, args| {
        Ok(Value::Bool(matches!(args[0], Value::Int(n) if n > 0)))
    });
    reg(i, "nat-int?", ArityHint::Exact(1), |_i, args| {
        Ok(Value::Bool(matches!(args[0], Value::Int(n) if n >= 0)))
    });
    reg(i, "neg-int?", ArityHint::Exact(1), |_i, args| {
        Ok(Value::Bool(matches!(args[0], Value::Int(n) if n < 0)))
    });
    reg(i, "double?", ArityHint::Exact(1), |_i, args| {
        Ok(Value::Bool(matches!(args[0], Value::Float(_))))
    });
    // S5: `NaN?`/`infinite?` are `^double`-hinted in `clojure.core`, but
    // the hint WIDENS rather than rejects -- measured, every one of
    // `(NaN? 1)`, `(infinite? 1)` and `(NaN? 1M)` answers `false` instead
    // of throwing. So both accept any number and are simply false for
    // every exact rank, which has no NaN and no infinity to have.
    reg(i, "NaN?", ArityHint::Exact(1), |_i, args| {
        Ok(Value::Bool(match &args[0] {
            Value::Float(f) => f.is_nan(),
            other => {
                crate::builtins::numbers::widen_to_f64(other, "NaN?")?;
                false
            }
        }))
    });
    reg(i, "infinite?", ArityHint::Exact(1), |_i, args| {
        Ok(Value::Bool(match &args[0] {
            Value::Float(f) => f.is_infinite(),
            other => {
                crate::builtins::numbers::widen_to_f64(other, "infinite?")?;
                false
            }
        }))
    });
    reg(i, "zero?", ArityHint::Exact(1), |_i, args| zero1(&args[0]));

    reg(i, "future?", ArityHint::Exact(1), |_i, args| Ok(Value::Bool(matches!(args[0], Value::Future(_)))));
    reg(i, "promise?", ArityHint::Exact(1), |_i, args| Ok(Value::Bool(matches!(args[0], Value::Promise(_)))));
    reg(i, "delay?", ArityHint::Exact(1), |_i, args| Ok(Value::Bool(matches!(args[0], Value::Delay(_)))));
    // M4b: measured `(volatile? (atom 1))` => `false` -- unrelated to
    // `atom?`, an ordinary variant-tag predicate like its neighbors above.
    reg(i, "volatile?", ArityHint::Exact(1), |_i, args| Ok(Value::Bool(matches!(args[0], Value::Volatile(_)))));
    // `chan?` (v0.2 / A2): lives here alongside the other type predicates
    // rather than in `builtins::async`, matching `future?`/`promise?`/
    // `delay?`'s placement above (pure state inspection, no channel logic).
    reg(i, "chan?", ArityHint::Exact(1), |_i, args| Ok(Value::Bool(matches!(args[0], Value::Channel(_)))));

    // S6 (assert/namespace/uuid predicate batch): `predicates.clj`'s own
    // `pred-val-table` truth table (oracle-measured, 2026-08-20) for the
    // five predicates below. `boolean?` -- exactly `Value::Bool`, unlike
    // the pre-existing `boolean` (a coercion, not a predicate) above.
    reg(i, "boolean?", ArityHint::Exact(1), |_i, args| Ok(Value::Bool(matches!(args[0], Value::Bool(_)))));
    // `ident?` -- `clojure.lang.Symbol` OR `clojure.lang.Keyword`
    // (measured: `(ident? :foo)`/`(ident? 'foo)` true, `(ident? "foo")`
    // false, `(ident? 5)`/`(ident? nil)` false).
    reg_unmeta(i, "ident?", ArityHint::Exact(1), |_i, args| {
        Ok(Value::Bool(matches!(args[0], Value::Sym(_) | Value::Keyword(_))))
    });
    // `uuid?` -- exactly `Value::Uuid` (see that variant's own doc).
    reg(i, "uuid?", ArityHint::Exact(1), |_i, args| Ok(Value::Bool(matches!(args[0], Value::Uuid(_)))));
    // `uri?` -- exactly `Value::Uri` (see that variant's own doc; mova's
    // ONLY producer is `(java.net.URI. s)`, `hostclass::construct`'s
    // narrow one-arg arm).
    reg(i, "uri?", ArityHint::Exact(1), |_i, args| Ok(Value::Bool(matches!(args[0], Value::Uri(_)))));
    // `inst?` -- measured against the table's one live case,
    // `java.util.Date`; real Clojure's `inst?` is protocol-based
    // (`Inst`, anything with `.getTime`-ish semantics -- `Instant` on a
    // real JVM too), but mova has only the one host-shimmed instant
    // shape (S5's `Value::HostInst` `HostKind::Date`), so this is
    // narrower-but-honest: true for a real Date shim, false for
    // everything else (never a false positive on anything mova can
    // actually construct).
    reg(i, "inst?", ArityHint::Exact(1), |_i, args| {
        Ok(Value::Bool(
            matches!(&args[0], Value::HostInst(h) if h.kind == crate::hostclass::HostKind::Date),
        ))
    });
    // SPEC-W1 task 4: `inst-ms` -- real Clojure's `(inst-ms inst)` is the
    // `Inst` protocol's one method, "the number of milliseconds since the
    // epoch". Same scope as `inst?` directly above (mova has exactly one
    // instant shape, `HostKind::Date`, whose entire state IS that number),
    // and the same error real Clojure raises for a non-instant: its
    // `inst-ms` is a protocol fn, so a `String`/number argument is an
    // "IllegalArgumentException: No implementation of method" -- reported
    // here as an ordinary type error naming the same condition.
    // `clojure.spec.alpha`'s `inst-in`/`inst-in-range?` are built on
    // exactly this pair.
    reg(i, "inst-ms", ArityHint::Exact(1), |_i, args| {
        match crate::hostclass::date_millis(&args[0]) {
            Some(ms) => Ok(Value::Int(ms)),
            None => Err(RjError::type_err(format!(
                "inst-ms: no implementation of method :inst-ms for {}",
                args[0].type_name()
            ))),
        }
    });

    // `realized?` (v0.2 / A1): pure state inspection, never blocks/forces --
    // matches Clojure's `IPending` contract (`future`/`promise`/`delay`),
    // extended here to `lazy-seq` too (mova's fourth "might not be computed
    // yet" cell shape).
    reg(i, "realized?", ArityHint::Exact(1), |_i, args| {
        Ok(Value::Bool(match &args[0] {
            Value::Future(cell) => !matches!(&*crate::sync::lock_mutex(&cell.state), crate::value::FutureState::Pending),
            Value::Promise(cell) => !matches!(&*crate::sync::lock_mutex(&cell.state), crate::value::PromiseState::Pending),
            Value::Delay(cell) => cell.result.get().is_some(),
            Value::Lazy(cell) => crate::sync::lock_mutex(&cell.realized).is_some(),
            other => {
                return Err(RjError::type_err(format!(
                    "realized?: expected a future, promise, delay, or lazy-seq, got {}",
                    other.type_name()
                )))
            }
        }))
    });
}
