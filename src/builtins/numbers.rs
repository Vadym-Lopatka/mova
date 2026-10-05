//! Seed numeric builtins so the evaluator is testable in P2: `+`, `-`, `*`,
//! `/`, `=`, `<`, `<=`, `>`, `>=`, `list`. P3a extends this file with the
//! rest of ARCHITECTURE.md's numbers.rs list (inc/dec/mod/quot/rem/abs/
//! min/max/not=). R4 adds `parse-long parse-double bit-and bit-or bit-xor
//! bit-not bit-shift-left bit-shift-right` and the `int/long/double/float`
//! casts. SPEC-C-casts.md adds `byte/short/num/bigint/bigdec` and gives
//! `byte/short/int/long/char` real range checks against a table measured
//! from real Clojure 1.13.0-alpha6 -- see `cast_integral`'s doc for the
//! shape every one of those five casts now shares.

use std::cmp::Ordering;
use std::sync::Arc;

use num_bigint::BigInt;
use num_traits::{FromPrimitive, Signed, ToPrimitive, Zero};

use crate::bignum::{
    ratio_ops, ArithError, BigDecVal, BigIntVal, RatioVal, Reduced, RoundingMode,
};
use crate::builtins::{reg, ArityHint};
use crate::error::{JvmClass, RjError};
use crate::eval::Interp;
use crate::value::Value;

/// The unboxed shape of a number. Exported so the compiled tier's
/// numeric-loop specialization (`compile::ir::NumLoop`) can hold loop-local
/// scalars in registers rather than in `Value`s, and still do its
/// arithmetic with the very functions `+`/`-`/`*`/`inc`/`dec` fold with --
/// which is what makes the specialization exact rather than merely similar.
/// Nothing outside this file and `compile::{ir,resolve,exec}` builds one.
#[derive(Clone, Copy)]
pub enum Num {
    I(i64),
    F(f64),
}

/// M8 slice 1 / defect D13: the JVM class real Clojure raises when an
/// arithmetic/comparison op is handed a non-number. Measured on the
/// 1.13.0-alpha6 oracle, all four rows through `clojure.lang.Numbers`:
///
///   (> nil 5) => NullPointerException
///               `Cannot invoke "Object.getClass()" because "x" is null`
///   (< nil 10) => NullPointerException  (same message)
///   (+ nil 1)  => NullPointerException  (same message)
///   (> :k 5)   => ClassCastException
///               `class clojure.lang.Keyword cannot be cast to
///                class java.lang.Number`
///   (- :k 1)   => ClassCastException    (same shape)
///
/// i.e. `Numbers.ops(x)` dereferences the argument to read its class, so
/// `nil` faults BEFORE any cast is attempted, and every other non-`Number`
/// fails the cast itself. Exactly the split `builtins::strings`'
/// `char_seq_reject_class` already makes for `clojure.string`'s
/// `^CharSequence` arguments, for the identical reason.
///
/// `ClassCast` is what `ErrorKind::TypeErr` already defaults to in
/// `error_kind_class_chain`, so tagging it changes no `catch` MATCH; it is
/// named explicitly (rather than left an unstated fallthrough) because
/// `error_to_info_map` now reads the tag to decide what the `catch` clause
/// BINDS, and the `:k` half of `spec.clj`'s `conform-explain` asserts
/// `"java.lang.ClassCastException"` verbatim.
fn number_reject_class(v: &Value) -> crate::error::JvmClass {
    match v {
        Value::Nil => crate::error::JvmClass::NullPointer,
        _ => crate::error::JvmClass::ClassCast,
    }
}

fn to_num(v: &Value, op: &str) -> Result<Num, RjError> {
    num_of(v).ok_or_else(|| {
        RjError::type_err(format!("{op}: expected a number, got {}", v.type_name()))
            .with_class(number_reject_class(v))
    })
}

/// The type test half of [`to_num`], without the error string: `Some` for
/// exactly the two `Value`s the arithmetic ops accept.
pub(crate) fn num_of(v: &Value) -> Option<Num> {
    match v {
        Value::Int(i) => Some(Num::I(*i)),
        Value::Float(f) => Some(Num::F(*f)),
        _ => None,
    }
}

pub(crate) fn as_f64(n: Num) -> f64 {
    match n {
        Num::I(i) => i as f64,
        Num::F(f) => f,
    }
}

pub(crate) fn num_to_value(n: Num) -> Value {
    match n {
        Num::I(i) => Value::Int(i),
        Num::F(f) => Value::Float(f),
    }
}

/// An `i64` op overflowed. Deliberately a ZERO-SIZED marker rather than a
/// full [`RjError`]: `add`/`sub`/`mul` are what the compiled tier's
/// register machine (`compile::exec::run_num_ops`) calls once per op in
/// its innermost loop, and an `RjError` is a `String` plus five more
/// fields -- returning one by value from the hot path would widen every
/// op's return from a register pair to a multi-word struct even on the
/// success side. `Result<Num, Overflow>` is instead niche-free but tiny,
/// and the error side does NO work at all: the message is built once, at
/// the boundary, by [`overflow_err`].
///
/// This is also why the hot path did not get slower in S5 even though
/// arithmetic became fallible: `checked_add` ALREADY branched on the
/// overflow bit (the pre-S5 code used that branch to promote to `f64`);
/// all that changed is which arm the taken-branch runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Overflow;

/// The zero-sized-ness of [`Overflow`] is load-bearing, not incidental:
/// `Num`'s discriminant only uses two of its 256 tag values, so
/// `Result<Num, Overflow>` niche-packs the error into a spare tag and is
/// the SAME SIZE as a bare `Num`. That is what makes the step functions'
/// new fallibility free on the success path -- no wider return value, no
/// extra register, no spill. If a future edit gives `Overflow` a payload
/// (a span, a message, an operand) this assertion fires, and the fix is to
/// keep the payload out of the hot path rather than to delete the
/// assertion.
const _: () = assert!(
    std::mem::size_of::<Result<Num, Overflow>>() == std::mem::size_of::<Num>(),
    "Result<Num, Overflow> must niche-pack into a bare Num -- see compile::exec::run_num_ops"
);

/// The measured JVM message for a checked-arithmetic overflow (transcript
/// rows 1-6: `(+ 9223372036854775807 1)` =>
/// `java.lang.ArithmeticException: long overflow`).
///
/// `*unchecked-math*` is NOT consulted here, deliberately -- but as of W4C
/// this is a NARROWER claim than it used to be. It reads like a runtime
/// switch, but it is a COMPILE-time one on the JVM -- measured directly:
/// `(binding [*unchecked-math* true] (+ Long/MAX_VALUE 1))` and even `(set!
/// *unchecked-math* true)` followed by evaluating `(+ Long/MAX_VALUE 1)` in
/// the SAME top-level form both still throw `ArithmeticException: long
/// overflow`; only a fn whose BODY was *compiled* while the flag was set
/// wraps for its entire lifetime, independent of the flag's value at any
/// later call site (`compat/w4-parse-unchecked-math-probe.clj`'s
/// transcript). That per-closure compile-time distinction is now real in
/// mova (`Closure::unchecked_math`, `Interp::current_unchecked`) -- this fn
/// itself still only ever produces the checked message; [`wrap_or_throw`]
/// is the W4C decision point that picks between calling this and wrapping,
/// and top-level (non-closure) arithmetic still always reaches this
/// unconditionally, matching the oracle's own top-level-`binding`/`set!`
/// case above (no closure frame is live, so `current_unchecked` is `false`
/// by construction). `unchecked-add` & friends remain the explicit,
/// always-available wrapping ops, untouched by any of this.
pub(crate) fn overflow_err() -> RjError {
    RjError::arithmetic("long overflow")
}

/// W4C: the one op-shape distinction [`wrap_or_throw`] needs to compute the
/// wrapped result -- `add_step`/`mul_step`/`sub2`/`inc1`/`dec1` each pass
/// their own operator through.
#[derive(Clone, Copy)]
pub(crate) enum WrapOp {
    Add,
    Sub,
    Mul,
}

/// W4C: the ONE consultation point for `Interp::current_unchecked` in the
/// entire arithmetic path -- called ONLY after `add`/`sub`/`mul` has ALREADY
/// returned `Err(Overflow)`, i.e. only on the cold path an `i64` overflow
/// takes. The non-overflow fast path (`add`/`sub`/`mul`'s `checked_*` calls)
/// never runs this fn and is completely unaware it exists.
///
/// `a`/`b` are the ORIGINAL operands (both guaranteed `Num::I` -- only an
/// `Int`/`Int` pair can overflow, same invariant `promoting` relies on)
/// rather than anything re-derived, so the wrapped result is exact: real
/// Clojure's unchecked ops are literally `x +/-/* y` at `i64`/`long` width
/// with silent 2's-complement wraparound, which is precisely what Rust's
/// `wrapping_add`/`wrapping_sub`/`wrapping_mul` compute.
pub(crate) fn wrap_or_throw(interp: &Interp, a: Num, b: Num, op: WrapOp) -> Result<Value, RjError> {
    let (Num::I(x), Num::I(y)) = (a, b) else {
        unreachable!("only Int/Int arithmetic can overflow");
    };
    if interp.current_unchecked() {
        Ok(Value::Int(match op {
            WrapOp::Add => x.wrapping_add(y),
            WrapOp::Sub => x.wrapping_sub(y),
            WrapOp::Mul => x.wrapping_mul(y),
        }))
    } else {
        Err(overflow_err())
    }
}

/// The measured JVM message for every division by an exact zero --
/// `/`, `quot`, `rem` and `mod` alike (transcript rows 63, 66, 67;
/// measured also for `(mod 5 0)`, `(quot 5.0 0.0)`, `(rem 5 0.0)`,
/// `(/ 1M 0M)`, `(/ 1/2 0)`). Float division proper is the ONE exception
/// and stays IEEE (`(/ 1.0 0)` => `##Inf`, `(/ 0 0.0)` => `##NaN`).
fn div_zero_err() -> RjError {
    RjError::divide_by_zero("Divide by zero")
}

fn non_terminating_err() -> RjError {
    RjError::arithmetic(
        "Non-terminating decimal expansion; no exact representable decimal result.",
    )
}

fn arith_err(e: ArithError) -> RjError {
    match e {
        ArithError::DivideByZero => div_zero_err(),
        ArithError::NonTerminating => non_terminating_err(),
        ArithError::RoundingNecessary => RjError::arithmetic("Rounding necessary"),
    }
}

pub(crate) fn add(a: Num, b: Num) -> Result<Num, Overflow> {
    match (a, b) {
        (Num::I(x), Num::I(y)) => match x.checked_add(y) {
            Some(r) => Ok(Num::I(r)),
            None => Err(Overflow),
        },
        _ => Ok(Num::F(as_f64(a) + as_f64(b))),
    }
}

pub(crate) fn sub(a: Num, b: Num) -> Result<Num, Overflow> {
    match (a, b) {
        (Num::I(x), Num::I(y)) => match x.checked_sub(y) {
            Some(r) => Ok(Num::I(r)),
            None => Err(Overflow),
        },
        _ => Ok(Num::F(as_f64(a) - as_f64(b))),
    }
}

pub(crate) fn mul(a: Num, b: Num) -> Result<Num, Overflow> {
    match (a, b) {
        (Num::I(x), Num::I(y)) => match x.checked_mul(y) {
            Some(r) => Ok(Num::I(r)),
            None => Err(Overflow),
        },
        _ => Ok(Num::F(as_f64(a) * as_f64(b))),
    }
}

// ---------------------------------------------------------------------------
// S5 (SPEC-numtower): the TOWER slow path.
//
// `Num` above stays exactly what it was -- the unboxed `i64`/`f64` fast
// domain the compiled tier holds in registers. A `BigInt`/`BigInteger`/
// `Ratio`/`BigDec` operand NEVER becomes a `Num`; it takes the `Value`-level
// path below instead. That split is the whole performance story of this
// spec: the tower is arbitrary-precision and allocating, and none of it is
// reachable from a loop whose operands are machine numbers.
//
// CONTAGION ORDER (measured, `compat/numtower-oracle-transcript.txt` plus
// the cross-pair probes): Long < BigInt/BigInteger < Ratio < BigDec <
// Double. The result type is the MAX of the two operands' ranks, with two
// wrinkles the probes pinned down:
//
//   * A rank-1-or-above INTEGER result is a `BigInt`, never a `Long`, even
//     when it would fit (`(+ 1N 2)` => `3N`).
//   * A `Ratio` result that reduces to an integer collapses to `BigInt`,
//     not `Ratio` and not `Long` (`(* 1/3 3)` => `1N`, `(+ 1/3 1/6)` =>
//     `1/2`).
// ---------------------------------------------------------------------------

/// One operand, promoted into the tower. `Rat` carries an ALREADY-REDUCED
/// `(numerator, denominator)` pair with `denominator > 0` -- an integer
/// operand enters as `n/1` -- so `bignum::ratio_ops` never has to
/// re-establish that invariant.
enum Tw {
    Int(BigInt),
    Rat(BigInt, BigInt),
    Dec(BigDecVal),
    Flt(f64),
}

/// Both operands re-expressed at their COMMON rank, ready for the actual
/// arithmetic. Producing this is the only place contagion is decided.
enum Pair {
    Int(BigInt, BigInt),
    Rat(BigInt, BigInt, BigInt, BigInt),
    Dec(BigDecVal, BigDecVal),
    Flt(f64, f64),
}

fn tower_of(v: &Value) -> Option<Tw> {
    Some(match v {
        Value::Int(n) => Tw::Int(BigInt::from(*n)),
        Value::BigInt(b) | Value::BigInteger(b) => Tw::Int(b.0.clone()),
        Value::Ratio(r) => Tw::Rat(r.numer().clone(), r.denom().clone()),
        Value::BigDec(d) => Tw::Dec(BigDecVal::new(d.unscaled().clone(), d.scale())),
        Value::Float(f) => Tw::Flt(*f),
        _ => return None,
    })
}

fn tw_f64(t: &Tw) -> f64 {
    match t {
        Tw::Int(n) => n.to_f64().unwrap_or(f64::NAN),
        Tw::Rat(n, d) => n.to_f64().unwrap_or(f64::NAN) / d.to_f64().unwrap_or(f64::NAN),
        Tw::Dec(d) => d.to_f64(),
        Tw::Flt(f) => *f,
    }
}

/// A `Ratio` reaching the BigDec rank must have an EXACT decimal
/// expansion; when it doesn't, Java's `Numbers.toBigDecimal(Ratio)` throws
/// the very same non-terminating error an inexact `BigDecimal.divide`
/// does (measured: `(== 1/3 1M)` throws `ArithmeticException:
/// Non-terminating decimal expansion; ...`).
fn tw_dec(t: Tw) -> Result<BigDecVal, RjError> {
    Ok(match t {
        Tw::Int(n) => BigDecVal::from_bigint(n),
        Tw::Rat(n, d) => match RatioVal::reduce(n, d) {
            Ok(Reduced::Int(b)) => BigDecVal::from_bigint(b.0),
            Ok(Reduced::Ratio(r)) => r.to_exact_bigdec().ok_or_else(non_terminating_err)?,
            Err(_) => return Err(div_zero_err()),
        },
        Tw::Dec(d) => d,
        Tw::Flt(_) => unreachable!("Float outranks BigDec; promote() never asks for this"),
    })
}

fn tw_rat(t: Tw) -> (BigInt, BigInt) {
    match t {
        Tw::Int(n) => (n, BigInt::from(1)),
        Tw::Rat(n, d) => (n, d),
        Tw::Dec(_) | Tw::Flt(_) => {
            unreachable!("BigDec/Float outrank Ratio; promote() never asks for this")
        }
    }
}

fn promote(a: Tw, b: Tw) -> Result<Pair, RjError> {
    // Highest rank wins, checked from the top down so each arm can assume
    // neither operand outranks it.
    if matches!(a, Tw::Flt(_)) || matches!(b, Tw::Flt(_)) {
        return Ok(Pair::Flt(tw_f64(&a), tw_f64(&b)));
    }
    if matches!(a, Tw::Dec(_)) || matches!(b, Tw::Dec(_)) {
        return Ok(Pair::Dec(tw_dec(a)?, tw_dec(b)?));
    }
    if matches!(a, Tw::Rat(..)) || matches!(b, Tw::Rat(..)) {
        let (n1, d1) = tw_rat(a);
        let (n2, d2) = tw_rat(b);
        return Ok(Pair::Rat(n1, d1, n2, d2));
    }
    match (a, b) {
        (Tw::Int(x), Tw::Int(y)) => Ok(Pair::Int(x, y)),
        _ => unreachable!("every non-Int rank was handled above"),
    }
}

fn pair_of(a: &Value, b: &Value, op: &str) -> Result<Pair, RjError> {
    let ta = tower_of(a).ok_or_else(|| not_a_number(a, op))?;
    let tb = tower_of(b).ok_or_else(|| not_a_number(b, op))?;
    promote(ta, tb)
}

fn not_a_number(v: &Value, op: &str) -> RjError {
    RjError::type_err(format!("{op}: expected a number, got {}", v.type_name()))
        .with_class(number_reject_class(v))
}

pub(crate) fn bigint_value(n: BigInt) -> Value {
    Value::BigInt(Arc::new(BigIntVal(n)))
}

fn biginteger_value(n: BigInt) -> Value {
    Value::BigInteger(Arc::new(BigIntVal(n)))
}

/// A `Reduced` (the output of every rational op) as the `Value` Clojure
/// actually produces: `Ratio` when the denominator survived reduction,
/// `BigInt` -- never `Long` -- when it collapsed to 1.
fn reduced_value(r: Reduced) -> Value {
    match r {
        Reduced::Int(b) => Value::BigInt(Arc::new(b)),
        Reduced::Ratio(r) => Value::Ratio(Arc::new(r)),
    }
}

// --- *math-context* / with-precision --------------------------------------

thread_local! {
    /// The innermost `with-precision` in effect on THIS thread, as
    /// `(precision, rounding mode)`. A thread-local rather than a threaded
    /// `&mut Interp` parameter on purpose: it is read ONLY inside the
    /// BigDec arms of the tower slow path (never by `Num` arithmetic, never
    /// by the compiled tier's register machine), so the hot path cannot
    /// pay for it even in principle, and `add_step`/`div2`/... keep the
    /// `&Value`-only signatures the compiled tier calls them with.
    ///
    /// Set/restored as a strict stack by `with-precision*` (see
    /// [`with_precision_scope`]), which is what makes it correct under
    /// nesting and under an error unwinding out of the body.
    static MATH_CONTEXT: std::cell::Cell<Option<(u64, RoundingMode)>> =
        const { std::cell::Cell::new(None) };
}

fn math_context() -> Option<(u64, RoundingMode)> {
    MATH_CONTEXT.with(|c| c.get())
}

/// W3e-3: the `(precision, mode)` a `*math-context*` VALUE stands for.
///
/// mova represents a math context as `{:precision n :rounding-mode "MODE"}`
/// -- what `core.mova`'s `with-precision` binds, and (since W3e-3) what
/// `(java.math.MathContext. n)` constructs. `None` for `nil` or anything
/// else, which is also what an unset `*math-context*` means.
fn math_context_of(v: &Value) -> Option<(u64, RoundingMode)> {
    let Value::Map(m) = v.unmeta() else { return None };
    let precision = match m.get(&Value::Keyword("precision".into()))? {
        Value::Int(n) if *n >= 0 => *n as u64,
        _ => return None,
    };
    let mode = match m.get(&Value::Keyword("rounding-mode".into())) {
        Some(Value::Str(s)) => RoundingMode::parse(s)?,
        None => RoundingMode::HalfUp,
        _ => return None,
    };
    Some((precision, mode))
}

/// W3e-3: re-derive the ambient `MathContext` from `*math-context*`'s
/// current value.
///
/// [`MATH_CONTEXT`] is a thread-local because the tower's BigDec arms take
/// `&Value`s only and cannot reach the interpreter (see its own doc). That
/// is fine for `with-precision`, which owns both halves -- but it left
/// `(set! *math-context* ...)` writing a var nothing read.
/// `clojure.test-clojure.vars/test-settable-math-context` is exactly that
/// shape: `(clojure.main/with-bindings (set! *math-context*
/// (java.math.MathContext. 8)) (+ 3.55555555555555M 1))` must round to 8
/// digits. So `set!` and `binding` -- the only two forms that can move that
/// var -- call this, keeping the thread-local a pure cache of the var
/// rather than a second, independent source of truth.
pub(crate) fn sync_math_context(v: &Value) {
    MATH_CONTEXT.with(|c| c.set(math_context_of(v)));
}

/// W3e-3: true for the one var name [`sync_math_context`] has to track.
/// Bare or `clojure.core/`-qualified, since `core.mova` interns it bare
/// (`Interp::qualify_def` does not qualify inside `ns::CORE_NS`) and a
/// caller may still have written the long spelling.
pub(crate) fn is_math_context_var(sym: &crate::value::Symbol) -> bool {
    sym.name.as_ref() == "*math-context*"
        && sym.ns.as_deref().is_none_or(|q| q == crate::ns::CORE_NS)
}

/// A finished `BigDecimal` result, with the ambient `MathContext` (if any)
/// applied. Every BigDec-producing op goes through here, because Java's
/// `BigDecimalOps` consults `*math-context*` in `add`/`multiply`/... too,
/// not only in `divide` (measured: `(with-precision 2 (+ 1234M 1M))` =>
/// `1.2E+3M`).
fn dec_value(d: BigDecVal) -> Result<Value, RjError> {
    let d = match math_context() {
        Some((p, mode)) => d.round_to_precision(p, mode).map_err(arith_err)?,
        None => d,
    };
    Ok(Value::BigDec(Arc::new(d)))
}

// --- the four arithmetic ops over a promoted pair --------------------------

fn tower_add(a: &Value, b: &Value, op: &str) -> Result<Value, RjError> {
    match pair_of(a, b, op)? {
        Pair::Int(x, y) => Ok(bigint_value(x + y)),
        Pair::Rat(n1, d1, n2, d2) => Ok(reduced_value(ratio_ops::add(&n1, &d1, &n2, &d2))),
        Pair::Dec(x, y) => dec_value(x.add(&y)),
        Pair::Flt(x, y) => Ok(Value::Float(x + y)),
    }
}

fn tower_sub(a: &Value, b: &Value, op: &str) -> Result<Value, RjError> {
    match pair_of(a, b, op)? {
        Pair::Int(x, y) => Ok(bigint_value(x - y)),
        Pair::Rat(n1, d1, n2, d2) => Ok(reduced_value(ratio_ops::sub(&n1, &d1, &n2, &d2))),
        Pair::Dec(x, y) => dec_value(x.sub(&y)),
        Pair::Flt(x, y) => Ok(Value::Float(x - y)),
    }
}

fn tower_mul(a: &Value, b: &Value, op: &str) -> Result<Value, RjError> {
    match pair_of(a, b, op)? {
        Pair::Int(x, y) => Ok(bigint_value(x * y)),
        Pair::Rat(n1, d1, n2, d2) => Ok(reduced_value(ratio_ops::mul(&n1, &d1, &n2, &d2))),
        Pair::Dec(x, y) => dec_value(x.mul(&y)),
        Pair::Flt(x, y) => Ok(Value::Float(x * y)),
    }
}

/// `/` across the whole tower. Two rules make this different from every
/// other op, both measured:
///
/// - `Long / Long` that does NOT divide exactly produces a `Ratio`, not a
///   `Double` (`(/ 1 3)` => `1/3`) -- and when it DOES divide exactly the
///   result stays a `Long` (`(/ 6 3)` => `2`), unlike every other rank
///   where an integral result is a `BigInt` (`(/ 4N 2)` => `2N`).
/// - `BigDec / BigDec` is EXACT or an error: without a `with-precision`
///   context, a non-terminating quotient throws rather than rounding
///   (`(/ 1M 3M)`).
fn tower_div(a: &Value, b: &Value) -> Result<Value, RjError> {
    // The `Long`/`Long` shape is spelled out here (rather than falling
    // into `Pair::Int`) purely because its result type differs: `Long`,
    // not `BigInt`.
    if let (Value::Int(x), Value::Int(y)) = (a, b) {
        if *y == 0 {
            return Err(div_zero_err());
        }
        return Ok(
            match RatioVal::reduce(BigInt::from(*x), BigInt::from(*y)).expect("y != 0 checked") {
                // A reduced integral quotient of two `i64`s always fits an
                // `i64` -- |n/gcd| <= |n| -- except for the single
                // `i64::MIN / -1` pair, which `to_i64_exact` catches and
                // which correctly promotes rather than wrapping.
                Reduced::Int(n) => match n.to_i64_exact() {
                    Some(i) => Value::Int(i),
                    None => Value::BigInt(Arc::new(n)),
                },
                Reduced::Ratio(r) => Value::Ratio(Arc::new(r)),
            },
        );
    }
    match pair_of(a, b, "/")? {
        Pair::Int(x, y) => {
            if x.is_zero() && y.is_zero() {
                return Err(div_zero_err());
            }
            match RatioVal::reduce(x, y) {
                Ok(r) => Ok(reduced_value(r)),
                Err(_) => Err(div_zero_err()),
            }
        }
        Pair::Rat(n1, d1, n2, d2) => match ratio_ops::div(&n1, &d1, &n2, &d2) {
            Ok(r) => Ok(reduced_value(r)),
            Err(_) => Err(div_zero_err()),
        },
        Pair::Dec(x, y) => {
            let q = match math_context() {
                Some((p, mode)) => x.divide_with_precision(&y, p, mode),
                None => x.divide_exact(&y),
            };
            // Already rounded to the context if there was one, so this
            // must NOT round again -- hence the direct construction
            // rather than `dec_value`.
            Ok(Value::BigDec(Arc::new(q.map_err(arith_err)?)))
        }
        // IEEE: division by a float zero is `##Inf`/`##NaN`, never an
        // error (transcript rows 64-65).
        Pair::Flt(x, y) => Ok(Value::Float(x / y)),
    }
}

// ---------------------------------------------------------------------------
// Shared with the compiled tier (v0.3 / S3)
//
// `compile::exec`'s `IntrinOp` arms call the functions below DIRECTLY, so a
// compiled `(+ a b)` never builds an argument slice or goes through the
// `NativeFn` dyn call. Every one of them is also what the corresponding
// native folds with, which is the point: overflow promotion, the `Int`/
// `Float` blend and the exact error strings are one implementation, not two
// that have to be kept in sync. The n-ary natives are therefore written as
// folds over the SAME step functions the intrinsics use -- including their
// identity element, which is load-bearing: `(+ -0.0 -0.0)` is `0.0`, not
// `-0.0`, precisely because `+` starts its fold at `0`.
// ---------------------------------------------------------------------------

/// The n-ary `+`/`*` fold.
///
/// The identity element is PERFORMED, not elided, whenever the fold stays
/// in the `Num` domain -- `(+ -0.0 -0.0)` is `0.0` in mova precisely
/// because `+` starts at `0`, and `compile::ir::NumBin::AddFold`/`MulFold`
/// exist to reproduce that step in the compiled tier. (Measured, and
/// recorded as a known divergence: real Clojure's 2-arity `+` does NOT
/// fold through the identity, so it answers `-0.0`. Correcting that means
/// changing `AddFold`/`MulFold` and the float-lane specializations built
/// on them -- see tests/conformance/DEVIATIONS.md's "S5 n-ary identity
/// fold" note.)
///
/// A TOWER first argument skips the identity step, because there the step
/// is not a no-op and not merely a sign question: under `with-precision`
/// it is an extra ROUNDING. Measured, `(with-precision 2 (* 123M 456M))`
/// is `5.6E+4M` -- one rounding of `56088` -- while
/// `(with-precision 2 (* 1 123M 456M))` is `5.5E+4M`, because real
/// Clojure's own 3-arity fold rounds `1 * 123M` first. Starting a
/// tower fold at `args[0]` is what makes mova's 2-arity agree with
/// Clojure's 2-arity instead of with Clojure's 3-arity. The compiled tier
/// is unaffected: a tower value can never enter a `Num` register.
pub(crate) fn fold_nary(
    interp: &Interp,
    args: &[Value],
    identity: Value,
    step: NumStep,
) -> Result<Value, RjError> {
    let skip = args.first().is_some_and(skips_identity);
    let (mut acc, rest) = if skip {
        (args[0].clone(), &args[1..])
    } else {
        (identity, args)
    };
    for a in rest {
        acc = step(interp, &acc, a)?;
    }
    Ok(acc)
}

/// One step of an n-ary arithmetic fold ([`add_step`] or [`mul_step`] --
/// the same ones `+` and `*` themselves fold with, and the same ones
/// `compile::exec`'s `IntrinOp::Add`/`Mul` intrinsic dispatch folds with).
pub(crate) type NumStep = fn(&Interp, &Value, &Value) -> Result<Value, RjError>;

/// Whether an n-ary `+`/`*` fold starting at `first` must SKIP its
/// identity step. True for exactly the tower values -- see
/// [`fold_nary`]'s doc for the measured `with-precision` reason. Shared
/// with `compile::exec`'s `IntrinOp::Add`/`Mul` arms so the compiled tier
/// folds identically; a compiled `(* 123M 456M)` would otherwise round
/// once more than the native one does.
pub(crate) fn skips_identity(first: &Value) -> bool {
    // A pure variant test, NOT `tower_of(..).is_some()`: `tower_of` clones
    // the operand's `BigInt` to build a `Tw`, and this only needs to know
    // WHICH variant it is. On the hot path (`Int`/`Float`) the first arm
    // matches and returns false immediately, so a compiled `(+ a b)` pays
    // one variant test and nothing else.
    matches!(
        first,
        Value::BigInt(_) | Value::BigInteger(_) | Value::Ratio(_) | Value::BigDec(_)
    )
}

/// One fold step of `+`, starting from `Value::Int(0)`.
///
/// The shape every step function below shares: try the `Num` fast domain
/// first (`num_of` is `Some` for exactly `Int`/`Float`), and only if
/// EITHER operand is outside it fall into the tower. An `i64` overflow in
/// the fast domain is a throw BY DEFAULT -- transcript rows 1-5 -- unless
/// the currently-executing closure was compiled under `*unchecked-math*`
/// (W4C, [`wrap_or_throw`]), in which case it wraps instead. The
/// non-overflow arm (`add(a, b)`'s `Ok` case) is completely unaffected
/// either way: this is the "only the overflow path changes" design
/// (`compat/w4-parse-unchecked-math-probe.clj`).
pub(crate) fn add_step(interp: &Interp, acc: &Value, x: &Value) -> Result<Value, RjError> {
    match (num_of(acc), num_of(x)) {
        (Some(a), Some(b)) => match add(a, b) {
            Ok(n) => Ok(num_to_value(n)),
            Err(Overflow) => wrap_or_throw(interp, a, b, WrapOp::Add),
        },
        _ => tower_add(acc, x, "+"),
    }
}

/// The ALWAYS-checked twin of [`add_step`], for callers that are mova's OWN
/// fixed internal implementation of a `clojure.core` fn rather than a
/// user's closure -- [`tower_mod`]'s fallback nudge is the one caller.
/// Real Clojure's `mod` is itself ordinary precompiled `clojure.core` code,
/// compiled ONCE (with the default checked semantics) long before any
/// user's `*unchecked-math*` setting exists; a later `binding`/`set!`
/// cannot retroactively change what `mod`'s own already-compiled `+` does,
/// so mova's `mod` must not consult `current_unchecked` either -- doing so
/// would make `mod`'s internal arithmetic depend on the CALLER's
/// unchecked-ness, which nothing on the real JVM does.
fn add_step_checked(acc: &Value, x: &Value) -> Result<Value, RjError> {
    match (num_of(acc), num_of(x)) {
        (Some(a), Some(b)) => add(a, b).map(num_to_value).map_err(|_| overflow_err()),
        _ => tower_add(acc, x, "+"),
    }
}

/// C3b (measured): `range`'s own step-add, used only by `range_lazy_tower`
/// (the non-`i64`-fast-path branch, taken e.g. whenever `end` is
/// `##Inf`). Real Clojure's `clojure.lang.Range` (as opposed to the
/// `i64`-only `LongRange`) increments with auto-PROMOTING arithmetic, not
/// throwing `+` -- measured: `(take 3 (range Long/MAX_VALUE ##Inf))` =>
/// `(9223372036854775807 9223372036854775808N 9223372036854775809N)`,
/// even though plain `(+ Long/MAX_VALUE 1)` itself throws
/// `ArithmeticException: long overflow`. [`add_step`] above is
/// deliberately unchanged (it backs `+` itself); this is [`promoting`]
/// under the hood, same as the `+'`/`inc'` family.
pub(crate) fn add_step_promoting(acc: &Value, x: &Value) -> Result<Value, RjError> {
    promoting(acc, x, PromOp::Add, "+")
}

/// One fold step of `*`, starting from `Value::Int(1)`. See [`add_step`]'s
/// doc for the overflow-path-only W4C consultation this shares.
pub(crate) fn mul_step(interp: &Interp, acc: &Value, x: &Value) -> Result<Value, RjError> {
    match (num_of(acc), num_of(x)) {
        (Some(a), Some(b)) => match mul(a, b) {
            Ok(n) => Ok(num_to_value(n)),
            Err(Overflow) => wrap_or_throw(interp, a, b, WrapOp::Mul),
        },
        _ => tower_mul(acc, x, "*"),
    }
}

/// One fold step of the 2-or-more-argument `-` (the 1-argument negation is
/// a different shape and stays in the native, reusing this via `(- 0 x)`).
/// See [`add_step`]'s doc for the overflow-path-only W4C consultation this
/// shares -- this is also what makes `(- Long/MIN_VALUE)` wrap rather than
/// throw inside an unchecked-compiled closure (`0 - Long/MIN_VALUE`
/// overflows the same as any other `Int`/`Int` subtraction).
pub(crate) fn sub2(interp: &Interp, a: &Value, b: &Value) -> Result<Value, RjError> {
    match (num_of(a), num_of(b)) {
        (Some(x), Some(y)) => match sub(x, y) {
            Ok(n) => Ok(num_to_value(n)),
            Err(Overflow) => wrap_or_throw(interp, x, y, WrapOp::Sub),
        },
        _ => tower_sub(a, b, "-"),
    }
}

/// The ALWAYS-checked twin of [`mul_step`] -- see [`add_step_checked`]'s
/// doc. Used ONLY by `unchecked_int32`/`unchecked`'s non-`Int`/`Int`
/// fallback (the `unchecked-*` family's own tower/float case, which can
/// never actually overflow, so this is behaviorally inert there either
/// way; it exists so the `unchecked-*` ops stay completely decoupled from
/// `current_unchecked`, per the W4C mandate: "`unchecked-*` named ops...
/// are UNTOUCHED").
fn mul_step_checked(acc: &Value, x: &Value) -> Result<Value, RjError> {
    match (num_of(acc), num_of(x)) {
        (Some(a), Some(b)) => mul(a, b).map(num_to_value).map_err(|_| overflow_err()),
        _ => tower_mul(acc, x, "*"),
    }
}

/// The ALWAYS-checked twin of [`sub2`] -- see [`add_step_checked`]'s doc
/// and [`mul_step_checked`]'s doc for why `unchecked`'s fallback uses this
/// rather than the `current_unchecked`-consulting public `sub2`.
fn sub2_checked(a: &Value, b: &Value) -> Result<Value, RjError> {
    match (num_of(a), num_of(b)) {
        (Some(x), Some(y)) => sub(x, y).map(num_to_value).map_err(|_| overflow_err()),
        _ => tower_sub(a, b, "-"),
    }
}

/// One fold step of the 2-or-more-argument `/`. NOTE the native type-checks
/// EVERY argument before dividing any of them, so `(/ 1 2 :x)` is a type
/// error rather than a division; with exactly two arguments -- the only
/// shape compiled to an intrinsic -- that ordering is unobservable.
pub(crate) fn div2(a: &Value, b: &Value) -> Result<Value, RjError> {
    tower_div(a, b)
}

/// See [`add_step`]'s doc for the overflow-path-only W4C consultation this
/// shares -- `inc` on `Long/MAX_VALUE` inside an unchecked-compiled
/// closure wraps to `Long/MIN_VALUE`, matching the oracle.
pub(crate) fn inc1(interp: &Interp, v: &Value) -> Result<Value, RjError> {
    match num_of(v) {
        Some(n) => match add(n, Num::I(1)) {
            Ok(r) => Ok(num_to_value(r)),
            Err(Overflow) => wrap_or_throw(interp, n, Num::I(1), WrapOp::Add),
        },
        None => tower_add(v, &Value::Int(1), "inc"),
    }
}

pub(crate) fn dec1(interp: &Interp, v: &Value) -> Result<Value, RjError> {
    match num_of(v) {
        Some(n) => match sub(n, Num::I(1)) {
            Ok(r) => Ok(num_to_value(r)),
            Err(Overflow) => wrap_or_throw(interp, n, Num::I(1), WrapOp::Sub),
        },
        None => tower_sub(v, &Value::Int(1), "dec"),
    }
}

/// The `'`-suffixed promoting family (`+' -' *' inc' dec'`): identical to
/// the checked ops EXCEPT that an `i64` overflow promotes to `BigInt`
/// instead of throwing (transcript rows 8-11). Both operands are `Int`
/// whenever `Overflow` can happen, so the promotion is a straight
/// `BigInt` redo of the same op rather than a re-entry into the tower.
fn promoting(a: &Value, b: &Value, op: PromOp, name: &str) -> Result<Value, RjError> {
    if let (Some(x), Some(y)) = (num_of(a), num_of(b)) {
        let fast = match op {
            PromOp::Add => add(x, y),
            PromOp::Sub => sub(x, y),
            PromOp::Mul => mul(x, y),
        };
        match fast {
            Ok(n) => return Ok(num_to_value(n)),
            Err(Overflow) => {
                // Only an `I`/`I` pair can overflow, so both sides are
                // exactly representable as `BigInt`.
                let (Num::I(xi), Num::I(yi)) = (x, y) else {
                    unreachable!("only Int/Int arithmetic can overflow");
                };
                let (bx, by) = (BigInt::from(xi), BigInt::from(yi));
                return Ok(bigint_value(match op {
                    PromOp::Add => bx + by,
                    PromOp::Sub => bx - by,
                    PromOp::Mul => bx * by,
                }));
            }
        }
    }
    match op {
        PromOp::Add => tower_add(a, b, name),
        PromOp::Sub => tower_sub(a, b, name),
        PromOp::Mul => tower_mul(a, b, name),
    }
}

/// The 32-bit half of the `unchecked-*` family. Wraps at `i32` width
/// (Java's `int`), then widens back to mova's single integer type.
fn unchecked_int32(a: &Value, b: &Value, op: PromOp) -> Result<Value, RjError> {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => {
            let (x, y) = (*x as i32, *y as i32);
            Ok(Value::Int(match op {
                PromOp::Add => x.wrapping_add(y),
                PromOp::Sub => x.wrapping_sub(y),
                PromOp::Mul => x.wrapping_mul(y),
            } as i64))
        }
        _ => unchecked(a, b, op),
    }
}

#[derive(Clone, Copy)]
enum PromOp {
    Add,
    Sub,
    Mul,
}

/// `unchecked-add`/`-subtract`/`-multiply` & co: two's-complement
/// wraparound for `Int`/`Int` (transcript rows 12-17), and ORDINARY
/// arithmetic for everything else -- real Clojure's
/// `Numbers.unchecked_add(Object, Object)` is literally `add(x, y)`
/// (measured: `(unchecked-add 1.5 2)` => `3.5`, no wraparound involved).
fn unchecked(a: &Value, b: &Value, op: PromOp) -> Result<Value, RjError> {
    if let (Value::Int(x), Value::Int(y)) = (a, b) {
        return Ok(Value::Int(match op {
            PromOp::Add => x.wrapping_add(*y),
            PromOp::Sub => x.wrapping_sub(*y),
            PromOp::Mul => x.wrapping_mul(*y),
        }));
    }
    match op {
        PromOp::Add => add_step_checked(a, b),
        PromOp::Sub => sub2_checked(a, b),
        PromOp::Mul => mul_step_checked(a, b),
    }
}

// The four comparisons, as named fns rather than inline closures, so the
// chained natives and the 2-argument intrinsics literally share them.
pub(crate) fn lt(a: f64, b: f64) -> bool {
    a < b
}
pub(crate) fn le(a: f64, b: f64) -> bool {
    a <= b
}
pub(crate) fn gt(a: f64, b: f64) -> bool {
    a > b
}
pub(crate) fn ge(a: f64, b: f64) -> bool {
    a >= b
}

/// Ordered comparison across the WHOLE tower. `None` means "unordered" --
/// reachable only through a `NaN`, and exactly what makes every one of
/// `< <= > >=` false on it.
///
/// Note the deliberate asymmetry with `=`: the ordered comparisons are NOT
/// category-strict. They promote through the very same contagion ladder
/// arithmetic does, which is why an `Int`-vs-`Double` comparison is a
/// plain `f64` comparison (measured, and the pre-S5 behavior this keeps)
/// while `BigInt`-vs-`BigInt` is exact -- `(< 12345678901234567890N 1.0)`
/// is `false` because the BigInt widens to a Double, but
/// `(< 12345678901234567890N 12345678901234567891N)` is exact.
pub(crate) fn tower_ordering(a: &Value, b: &Value, op: &str) -> Result<Option<Ordering>, RjError> {
    Ok(match pair_of(a, b, op)? {
        Pair::Int(x, y) => Some(x.cmp(&y)),
        // `n1/d1 <=> n2/d2` with both denominators positive (a `Ratio`
        // invariant) is `n1*d2 <=> n2*d1` -- exact, no division.
        Pair::Rat(n1, d1, n2, d2) => Some((n1 * d2).cmp(&(n2 * d1))),
        Pair::Dec(x, y) => Some(x.cmp(&y)),
        Pair::Flt(x, y) => x.partial_cmp(&y),
    })
}

/// The 2-argument case of [`cmp_chain`]: an `Int`/`Float` pair compares as
/// `f64` (the fast path, and what the compiled tier's register machine
/// does inline); anything with a tower operand goes through
/// [`tower_ordering`].
fn cmp2(a: &Value, b: &Value, op: &str, cmp: fn(f64, f64) -> bool) -> Result<Value, RjError> {
    if let (Some(x), Some(y)) = (num_of(a), num_of(b)) {
        return Ok(Value::Bool(cmp(as_f64(x), as_f64(y))));
    }
    let ord = tower_ordering(a, b, op)?;
    Ok(Value::Bool(ordering_matches(op, ord)))
}

/// Which orderings each of `< <= > >=` accepts. `None` (a `NaN` was
/// involved) is false for all four.
fn ordering_matches(op: &str, ord: Option<Ordering>) -> bool {
    match ord {
        None => false,
        Some(o) => match op {
            "<" => o == Ordering::Less,
            "<=" => o != Ordering::Greater,
            ">" => o == Ordering::Greater,
            ">=" => o != Ordering::Less,
            _ => unreachable!("ordering_matches is only ever called with one of < <= > >="),
        },
    }
}

/// `==`: numeric-value equality that IGNORES category, the exact
/// complement of `=` (which is category-strict). It promotes through the
/// same contagion ladder as arithmetic, so a `Double` operand makes it a
/// `f64` comparison -- measured, and genuinely inexact for big values:
/// `(== 9007199254740993 9.007199254740992E15)` is TRUE on the JVM even
/// though the two are different numbers, because `DoubleOps.equiv` widens
/// both through `doubleValue()`. Away from `Double` it is exact
/// (`(== 1M 1N)`, `(== 1/2 0.5M)`), and a `Ratio` promoted to the BigDec
/// rank can even throw (`(== 1/3 1M)` -- see [`tw_dec`]).
fn num_equiv(a: &Value, b: &Value) -> Result<bool, RjError> {
    Ok(match pair_of(a, b, "==")? {
        Pair::Int(x, y) => x == y,
        Pair::Rat(n1, d1, n2, d2) => n1 * d2 == n2 * d1,
        Pair::Dec(x, y) => x.cmp(&y) == Ordering::Equal,
        // NOT `partial_cmp == Equal`: that would also have to special-case
        // NaN, and `f64`'s own `==` already returns false for it
        // (measured: `(== ##NaN ##NaN)` is false).
        Pair::Flt(x, y) => x == y,
    })
}

pub(crate) fn lt2(a: &Value, b: &Value) -> Result<Value, RjError> {
    cmp2(a, b, "<", lt)
}
pub(crate) fn le2(a: &Value, b: &Value) -> Result<Value, RjError> {
    cmp2(a, b, "<=", le)
}
pub(crate) fn gt2(a: &Value, b: &Value) -> Result<Value, RjError> {
    cmp2(a, b, ">", gt)
}
pub(crate) fn ge2(a: &Value, b: &Value) -> Result<Value, RjError> {
    cmp2(a, b, ">=", ge)
}

/// `=` restricted to two numbers, for `compile::ir::NumLoop`'s test
/// position. This is NOT `lt`/`le`/`gt`/`ge`'s `as_f64` comparison: `=` runs
/// through [`Interp::values_equal`](crate::eval::Interp::values_equal),
/// whose behavior on two numbers is exactly two of its arms --
///
/// - the `Int`/`Float` blend arm, `*x as f64 == *y`;
/// - the catch-all, `Value`'s own `PartialEq`, which is exact for
///   `Int`/`Int` and BIT equality for `Float`/`Float`;
///
/// -- and that is what the two arms below are, in the same order. Nothing is
/// re-derived: the second delegates to the same `PartialEq` impl. No forcing
/// step is needed, because a `Num` can never be a `Lazy`.
///
/// The distinction is observable in three places, all covered by tests:
/// `(= 0.0 -0.0)` is false while `(< 0.0 -0.0)`'s operands are equal;
/// `(= NaN NaN)` is true while every ordered comparison on NaN is false; and
/// `=` separates two adjacent i64s that `as_f64` rounds together.
pub(crate) fn num_eq(a: Num, b: Num) -> bool {
    match (a, b) {
        // S5: `=` is CATEGORY-STRICT (see `Interp::values_equal`'s own
        // note) -- an `Int` is never `=` to a `Float`, measured
        // `(= 1 1.0)` => `false`. This arm used to be the `x as f64 == y`
        // blend; it is now the constant `false`, kept as an explicit arm
        // rather than deleted so the contrast with `lt`/`le`/`gt`/`ge`
        // (which DO widen through `f64`) stays visible right here.
        (Num::I(_), Num::F(_)) | (Num::F(_), Num::I(_)) => false,
        (Num::I(x), Num::I(y)) => x == y,
        // IEEE, not bits -- `(= 0.0 -0.0)` is true and `(= ##NaN ##NaN)`
        // is false (both measured). See `Interp::values_equal`'s
        // `Float`/`Float` arm, which this mirrors for the compiled tier's
        // `NumCmp::Eq`; the two must agree, since `ir::NumCmp` exists
        // precisely to be indistinguishable from the native `=`.
        (Num::F(x), Num::F(y)) => x == y,
    }
}

fn cmp_chain(args: &[Value], op: &str, cmp: fn(f64, f64) -> bool) -> Result<Value, RjError> {
    // Fast path: every argument is an `Int`/`Float`, so the whole chain is
    // an `f64` comparison with one small allocation, exactly as before S5.
    if args.iter().all(|a| num_of(a).is_some()) {
        let mut nums = Vec::with_capacity(args.len());
        for a in args {
            nums.push(as_f64(to_num(a, op)?));
        }
        for w in nums.windows(2) {
            if !cmp(w[0], w[1]) {
                return Ok(Value::Bool(false));
            }
        }
        return Ok(Value::Bool(true));
    }
    // Tower path: type-check every argument first (so `(< 1 :x)` is a type
    // error rather than a short-circuited `false`), then compare windows.
    for a in args {
        if tower_of(a).is_none() {
            return Err(not_a_number(a, op));
        }
    }
    for w in args.windows(2) {
        if !ordering_matches(op, tower_ordering(&w[0], &w[1], op)?) {
            return Ok(Value::Bool(false));
        }
    }
    Ok(Value::Bool(true))
}

// --- quot / rem / mod across the tower -------------------------------------

/// `quot`: truncate the quotient toward zero. Rank rules, all measured:
/// `Long/Long` stays `Long`; anything else integral is a `BigInt`
/// (`(quot 10N 3)` => `3N`, and even `(quot 1/2 1)` => `0N`); `BigDec`
/// operands give a `BigDec` at the preferred scale (`(quot 7.5M 2M)` =>
/// `3.0M`); a `Double` operand gives a truncated `Double`. Division by an
/// exact zero throws in EVERY rank, the float one included (measured:
/// `(quot 5.0 0.0)` throws, unlike `(/ 5.0 0.0)` which is `##Inf`).
fn tower_quot(a: &Value, b: &Value) -> Result<Value, RjError> {
    if let (Value::Int(x), Value::Int(y)) = (a, b) {
        if *y == 0 {
            return Err(div_zero_err());
        }
        // `i64::MIN / -1` is the one overflowing integer division; Java
        // throws `ArithmeticException` for it too.
        return x.checked_div(*y).map(Value::Int).ok_or_else(overflow_err);
    }
    match pair_of(a, b, "quot")? {
        Pair::Int(x, y) => {
            if y.is_zero() {
                return Err(div_zero_err());
            }
            Ok(bigint_value(x / y))
        }
        Pair::Rat(n1, d1, n2, d2) => {
            if n2.is_zero() {
                return Err(div_zero_err());
            }
            Ok(bigint_value((n1 * d2) / (d1 * n2)))
        }
        Pair::Dec(x, y) => dec_value(x.quot(&y).map_err(arith_err)?),
        Pair::Flt(x, y) => {
            if y == 0.0 {
                return Err(div_zero_err());
            }
            Ok(Value::Float((x / y).trunc()))
        }
    }
}

/// `rem`: the remainder whose sign follows the DIVIDEND, i.e.
/// `x - (quot x y) * y` (measured `(rem -10N 3)` => `-1N`).
fn tower_rem(a: &Value, b: &Value) -> Result<Value, RjError> {
    if let (Value::Int(x), Value::Int(y)) = (a, b) {
        if *y == 0 {
            return Err(div_zero_err());
        }
        return x.checked_rem(*y).map(Value::Int).ok_or_else(overflow_err);
    }
    match pair_of(a, b, "rem")? {
        Pair::Int(x, y) => {
            if y.is_zero() {
                return Err(div_zero_err());
            }
            Ok(bigint_value(x % y))
        }
        Pair::Rat(n1, d1, n2, d2) => {
            if n2.is_zero() {
                return Err(div_zero_err());
            }
            let q = (&n1 * &d2) / (&d1 * &n2);
            // x - q*y, kept in exact rational form.
            let (qn, qd) = (q * &n2, d2.clone());
            Ok(reduced_value(ratio_ops::sub(&n1, &d1, &qn, &qd)))
        }
        Pair::Dec(x, y) => dec_value(x.rem(&y).map_err(arith_err)?),
        Pair::Flt(x, y) => {
            if y == 0.0 {
                return Err(div_zero_err());
            }
            Ok(Value::Float(x % y))
        }
    }
}

/// `mod`: `rem`, then nudged by one divisor when the remainder's sign
/// disagrees with the DIVISOR's -- floor-mod. Written as the same
/// `(rem, +)` composition `clojure.core/mod` itself is, so it can never
/// drift from `tower_rem` (measured `(mod -5N 3)` => `1N`,
/// `(mod 10N -3)` => `-2N`, `(mod -7/2 1/3)` => `1/6`).
fn tower_mod(a: &Value, b: &Value) -> Result<Value, RjError> {
    if let (Value::Int(x), Value::Int(y)) = (a, b) {
        if *y == 0 {
            return Err(div_zero_err());
        }
        return Ok(Value::Int(int_floor_mod(*x, *y)));
    }
    let m = tower_rem(a, b)?;
    if value_signum(&m)? == 0 {
        return Ok(m);
    }
    if value_signum(&m)? == value_signum(b)? {
        return Ok(m);
    }
    add_step_checked(&m, b)
}

/// `-1`/`0`/`1` for any numeric `Value` -- `NaN` reports `0`, matching
/// what the `mod` composition above needs (a `NaN` remainder must not be
/// "nudged").
fn value_signum(v: &Value) -> Result<i32, RjError> {
    Ok(match v {
        Value::Int(n) => (*n).signum() as i32,
        Value::Float(f) => {
            if *f > 0.0 {
                1
            } else if *f < 0.0 {
                -1
            } else {
                0
            }
        }
        Value::BigInt(b) | Value::BigInteger(b) => match b.0.sign() {
            num_bigint::Sign::Minus => -1,
            num_bigint::Sign::NoSign => 0,
            num_bigint::Sign::Plus => 1,
        },
        Value::Ratio(r) => match r.numer().sign() {
            num_bigint::Sign::Minus => -1,
            num_bigint::Sign::NoSign => 0,
            num_bigint::Sign::Plus => 1,
        },
        Value::BigDec(d) => d.signum(),
        other => return Err(not_a_number(other, "mod")),
    })
}

/// Floor-mod for integers: sign of the result follows the *divisor* `y`
/// (Clojure `mod`, unlike Rust's truncating `%`). `pub(crate)` so
/// `builtins::math`'s `clojure.math/floor-mod` (Java `Math.floorMod`,
/// same sign convention) can share this instead of re-deriving it.
pub(crate) fn int_floor_mod(x: i64, y: i64) -> i64 {
    let r = x % y;
    if r != 0 && (r < 0) != (y < 0) {
        r + y
    } else {
        r
    }
}

/// One `min`/`max` fold step: `(if (<cmp> acc x) acc x)`, plus Java's
/// `Numbers.min`/`max` NaN short-circuit (`if x is NaN return x; if y is
/// NaN return y`) -- measured `(min ##NaN 1)` => `##NaN` and
/// `(max 1 ##NaN)` => `##NaN`, which the plain comparison alone would get
/// wrong in both directions since every comparison against NaN is false.
fn min_max_step(acc: &Value, x: &Value, cmp: &str, op: &str) -> Result<Value, RjError> {
    if tower_of(x).is_none() {
        return Err(not_a_number(x, op));
    }
    min_max_step_body(acc, x, cmp)
}

/// W-REDUCE: the exact per-step body `min`/`max`'s own native runs, minus
/// the `min_max_step` `x`-validation wrapper -- pulled out so
/// [`min_max_reduce_step`] can add its OWN pre-step validation (of `acc`,
/// not `x`) without duplicating the NaN/tower-ordering logic.
fn min_max_step_body(acc: &Value, x: &Value, cmp: &str) -> Result<Value, RjError> {
    if matches!(acc, Value::Float(f) if f.is_nan()) {
        return Ok(acc.clone());
    }
    if matches!(x, Value::Float(f) if f.is_nan()) {
        return Ok(x.clone());
    }
    let ord = tower_ordering(acc, x, cmp)?;
    Ok(if ordering_matches(cmp, ord) {
        acc.clone()
    } else {
        x.clone()
    })
}

/// W-REDUCE: `builtins::seq::reduce_coll`'s fast arithmetic path calls this
/// directly for a recognized-boot `min`/`max`, in place of going through
/// `Value::Native` call dispatch + arity check + the `[acc, x]` 2-arg buf
/// ceremony every single element. Reproduces `min`/`max`'s own native body
/// EXACTLY: `reduce` re-invokes that native fresh on every `(acc, x)`
/// pair (`args.len() == 2` always, on this path), so its `tower_of(&best)`
/// upfront check runs on `acc` every step too -- not just once at the very
/// first pair -- and this mirrors that rather than "optimizing" it away,
/// so an error mid-reduction (e.g. a non-numeric `acc` that somehow made
/// it into the accumulator slot) still throws at exactly the same step
/// with exactly the same message.
pub(crate) fn min_max_reduce_step(acc: &Value, x: &Value, cmp: &str, op: &str) -> Result<Value, RjError> {
    tower_of(acc).ok_or_else(|| not_a_number(acc, op))?;
    min_max_step_body(acc, x, cmp)
}

/// A JVM `ClassCastException` message, reproduced down to the module
/// clause: `v`'s class is always mova's own boot-class naming (`java.*`
/// classes live in `java.base`/`'bootstrap'`, everything else -- Ratio
/// included -- in the unnamed module of loader `'app'`), and `target` gets
/// the same treatment via `target_is_boot`. The four loader combinations
/// this produces are exactly `checkcast`'s own four wordings; shared by
/// [`ratio_cast_err`] (`target` = `clojure.lang.Ratio`, app-loaded) and
/// `prim_param_cce` below (`target` = `java.lang.Number`, boot-loaded).
fn class_cast_err(v: &Value, target: &str, target_is_boot: bool) -> RjError {
    let cls = crate::types::builtin_class_name(v);
    let src_is_boot = cls.starts_with("java.");
    let where_clause = match (src_is_boot, target_is_boot) {
        (true, true) => format!("({cls} and {target} are in module java.base of loader 'bootstrap')"),
        (true, false) => {
            format!("({cls} is in module java.base of loader 'bootstrap'; {target} is in unnamed module of loader 'app')")
        }
        (false, true) => {
            format!("({cls} is in unnamed module of loader 'app'; {target} is in module java.base of loader 'bootstrap')")
        }
        (false, false) => format!("({cls} and {target} are in unnamed module of loader 'app')"),
    };
    RjError::type_err(format!("class {cls} cannot be cast to class {target} {where_clause}"))
}

/// The `numerator`/`denominator` non-Ratio error. Real Clojure's message
/// is a full JVM `ClassCastException` text naming both classes and their
/// modules; reproduced here down to the module clause because transcript
/// row 89 records it verbatim.
fn ratio_cast_err(v: &Value) -> RjError {
    class_cast_err(v, "clojure.lang.Ratio", false)
}

/// `clojure.lang.Numbers.rationalize`, transcribed:
///
/// - `Double` -> the `BigDecimal` of its SHORTEST round-trip decimal
///   rendering, then the `BigDecimal` case (so `(rationalize 0.1)` is
///   `1/10`, not the exact binary expansion of the double `0.1`).
/// - `BigDecimal` with a negative scale -> a `BigInt` (`(rationalize
///   1.0E10)` => `10000000000N`).
/// - `BigDecimal` otherwise -> `unscaled / 10^scale`, reduced -- which
///   collapses to a `BigInt` when it divides out (`(rationalize 1.0)` =>
///   `1N`, NOT the Long `1`).
/// - anything else (Long, BigInt, BigInteger, Ratio) -> itself, unchanged.
fn rationalize(v: &Value) -> Result<Value, RjError> {
    let dec = match v {
        Value::Float(f) => {
            if !f.is_finite() {
                return Err(RjError::type_err(format!(
                    "rationalize: {} has no exact decimal representation",
                    crate::printer::display_str(v)
                )));
            }
            let mut s = String::new();
            crate::printer::write_finite_float_java(*f, &mut s);
            BigDecVal::parse(&s)
                .ok_or_else(|| RjError::other(format!("rationalize: could not parse {s:?}")))?
        }
        Value::BigDec(d) => BigDecVal::new(d.unscaled().clone(), d.scale()),
        Value::Int(_) | Value::BigInt(_) | Value::BigInteger(_) | Value::Ratio(_) => {
            return Ok(v.clone())
        }
        other => return Err(not_a_number(other, "rationalize")),
    };
    let scale = dec.scale();
    if scale < 0 {
        return Ok(bigint_value(dec.trunc_to_bigint()));
    }
    let den = BigInt::from(10).pow(scale as u32);
    match RatioVal::reduce(dec.unscaled().clone(), den) {
        Ok(r) => Ok(reduced_value(r)),
        Err(_) => Err(div_zero_err()),
    }
}

/// Run `thunk` with `(precision, mode)` installed as the ambient
/// `MathContext`, restoring the previous one on the way out -- INCLUDING
/// when the body errors, which is why the restore is not written after the
/// call but around it.
fn with_precision_scope(
    interp: &mut Interp,
    precision: u64,
    mode: RoundingMode,
    thunk: &Value,
) -> Result<Value, RjError> {
    let saved = MATH_CONTEXT.with(|c| c.replace(Some((precision, mode))));
    let out = interp.call(thunk, &[]);
    MATH_CONTEXT.with(|c| c.set(saved));
    out
}

// --- (hash n): Clojure's own numeric hasheq ---------------------------------

/// `clojure.lang.Murmur3.hashLong` -- the finalized 8-byte Murmur3 hash
/// Clojure uses for EVERY integer-category value. Transcribed from the
/// JVM source rather than approximated, because `(hash 7)` is a concrete
/// number the transcript records (`-137604029`), not merely something that
/// has to agree with `(hash 7N)`.
fn murmur3_hash_long(input: i64) -> i32 {
    const C1: i32 = 0xcc9e2d51u32 as i32;
    const C2: i32 = 0x1b873593;
    fn mix_k1(mut k1: i32) -> i32 {
        k1 = k1.wrapping_mul(C1);
        k1 = k1.rotate_left(15);
        k1.wrapping_mul(C2)
    }
    fn mix_h1(mut h1: i32, k1: i32) -> i32 {
        h1 ^= k1;
        h1 = h1.rotate_left(13);
        h1.wrapping_mul(5).wrapping_add(0xe6546b64u32 as i32)
    }
    fn fmix(mut h1: i32, length: i32) -> i32 {
        h1 ^= length;
        h1 ^= ((h1 as u32) >> 16) as i32;
        h1 = h1.wrapping_mul(0x85ebca6bu32 as i32);
        h1 ^= ((h1 as u32) >> 13) as i32;
        h1 = h1.wrapping_mul(0xc2b2ae35u32 as i32);
        h1 ^ (((h1 as u32) >> 16) as i32)
    }
    if input == 0 {
        return 0;
    }
    let low = input as i32;
    let high = ((input as u64) >> 32) as i32;
    let h1 = mix_h1(0, mix_k1(low));
    let h1 = mix_h1(h1, mix_k1(high));
    fmix(h1, 8)
}

/// `clojure.lang.Util.hasheq` restricted to numbers -- `None` for anything
/// that isn't one, so the `hash` builtin can fall back to mova's own
/// (deterministic but not JVM-compatible) structural hash for every other
/// shape. Measured values this reproduces exactly: `(hash 7)` and
/// `(hash 7N)` both `-137604029`; `(hash (biginteger 5))` == `(hash 5)` ==
/// `1740791543`; `(hash 12345678901234567890N)` `-1436577082`;
/// `(hash 1/3)` `2`; `(hash 1.5M)` == `(hash 1.50M)` == `466`;
/// `(hash 0.5)` `1071644672`.
pub(crate) fn numeric_hasheq(v: &Value) -> Option<i32> {
    Some(match v {
        Value::Int(n) => murmur3_hash_long(*n),
        // An integer that FITS a long hashes as that long (which is what
        // makes `(hash 7N) == (hash 7)`); one that doesn't falls back to
        // `BigInteger.hashCode`, exactly as `clojure.lang.BigInt.hasheq`
        // and `Numbers.hasheq`'s BigInteger arm do.
        Value::BigInt(b) | Value::BigInteger(b) => match b.to_i64_exact() {
            Some(n) => murmur3_hash_long(n),
            None => b.java_hash_code(),
        },
        Value::Ratio(r) => r.java_hash_code(),
        Value::BigDec(d) => d.clojure_hash(),
        // `Double.hashCode` -- the raw bits folded in half -- EXCEPT for
        // zero: `Numbers.hasheq` sends both `0.0` and `-0.0` to `0` so
        // that two values its own `equiv` calls equal cannot hash apart
        // (measured: `(hash 0.0)` and `(hash -0.0)` are both `0`, where
        // the raw `Double.hashCode(-0.0)` would be `-2147483648`).
        Value::Float(f) => {
            if *f == 0.0 {
                0
            } else {
                let bits = f.to_bits();
                (bits ^ (bits >> 32)) as i32
            }
        }
        _ => return None,
    })
}

/// The one host-interop method this spec needs: `(.toBigInteger 5N)`.
/// `Some` when `field` names a method these numeric types have, `None`
/// otherwise (so `eval_dot_form` falls through to its ordinary
/// record/deftype field lookup and its unresolved-symbol error).
pub(crate) fn numeric_dot_method(field: &str, target: &Value) -> Option<Value> {
    match (field, target) {
        ("toBigInteger", Value::BigInt(b) | Value::BigInteger(b)) => {
            Some(biginteger_value(b.0.clone()))
        }
        ("toBigInteger", Value::BigDec(d)) => Some(biginteger_value(d.trunc_to_bigint())),
        // D5: `(.toString n)` on a number -- the same text `str`/`pr-str`
        // give (every numeric variant prints identically either way; a
        // Double's `3.14159`, a BigInt's `5N`), because there is exactly
        // one number-to-text rendering in mova and this is a second
        // SPELLING of it, not a second policy. Vendored
        // `clojure.pprint`'s `cl_format.clj` calls it while decomposing a
        // float into mantissa/exponent (`(.toLowerCase (.toString f))`).
        (
            "toString",
            Value::Int(_)
            | Value::Float(_)
            | Value::BigInt(_)
            | Value::BigInteger(_)
            | Value::Ratio(_)
            | Value::BigDec(_),
        ) => Some(Value::Str(crate::printer::pr_str(target).into())),
        // clojure-lsp campaign (mova/PLAN.md): `.bitLength`/`.negate`/
        // `.longValue` -- the three remaining arg-free `java.math.
        // BigInteger` methods `clojure.tools.reader.impl.commons`'s
        // number parser calls (a transitive dependency of `rewrite-clj.
        // reader`): `(if (< (.bitLength bn) 64) (.longValue bn)
        // (BigInt/fromBigInteger bn))`, `bn` itself possibly `(.negate
        // bn)`'d first for a leading `-`.
        //
        // `bitLength`: real Java semantics, not `.bits()`'s plain
        // magnitude -- `BigInteger.bitLength()` is defined over the
        // MINIMAL two's-complement representation (excluding the sign
        // bit), which for a negative `v` is `bitLength(-v - 1)`, not
        // `bitLength(-v)` (measured identity: `Long.MIN_VALUE.
        // bitLength()` is `63`, matching `-2^63` fitting exactly in a
        // signed 64-bit long). For `v >= 0` the two coincide with plain
        // magnitude bit count (`0` for `v = 0`, matching `num_bigint`'s
        // own `.bits()`).
        ("bitLength", Value::BigInteger(b)) => {
            let n = &b.0;
            let bits = if num_bigint::Sign::Minus == n.sign() {
                (-(n + 1_i32)).bits()
            } else {
                n.bits()
            };
            Some(Value::Int(bits as i64))
        }
        ("negate", Value::BigInteger(b)) => {
            Some(Value::BigInteger(Arc::new(BigIntVal(-&b.0))))
        }
        // `.longValue`: real Java narrows with wrapping (low 64 bits),
        // but every call site this campaign reaches already checked
        // `.bitLength bn) < 64` first, so the value always fits exactly
        // -- `to_i64_exact` is therefore total in practice; the
        // `unwrap_or` fallback (wrapping via the low 64 bits) exists
        // only so an out-of-contract call degrades instead of panicking.
        ("longValue", Value::BigInt(b) | Value::BigInteger(b)) => Some(Value::Int(
            b.to_i64_exact().unwrap_or_else(|| {
                let (_, bytes) = b.0.to_bytes_le();
                let mut buf = [0u8; 8];
                let n = bytes.len().min(8);
                buf[..n].copy_from_slice(&bytes[..n]);
                let mag = i64::from_le_bytes(buf);
                if num_bigint::Sign::Minus == b.0.sign() { -mag } else { mag }
            }),
        )),
        _ => None,
    }
}

/// SPEC-W3: `(.shiftLeft (biginteger 1) exp)` -- the one ARGUMENT-taking
/// host method the numeric tower needs, so it cannot ride in
/// [`numeric_dot_method`] (arg-free by construction) and gets its own arm
/// in `eval_dot_form`.
///
/// `clojure.test.check.generators/two-pow` is literally `(bigint
/// (.shiftLeft (biginteger 1) exp))`, and `two-pow` is on the only path
/// that reaches `size-bounded-bigint` -- one of the twelve branches of
/// `gen/simple-type`, which is in turn what `s/gen` uses for `coll?`,
/// `vector?`, `map?`, `set?`, `seq?`, `associative?` and (via
/// `any-printable`) `any?`. Because `one-of` picks a branch at RANDOM,
/// its absence did not fail those generators deterministically; it failed
/// them about one run in twelve, which is worse. This is the second half
/// of defect-ledger D3: fixing `repeat`'s float count got
/// `size-bounded-bignat` as far as here.
///
/// `java.math.BigInteger.shiftLeft` is defined for a NEGATIVE distance too
/// (it shifts the other way), so that case is honoured rather than
/// rejected. The receiver may be either integer-bignum spelling; the
/// result is a `java.math.BigInteger`, exactly as on the JVM (the
/// `bigint` at the call site is what turns it back into a
/// `clojure.lang.BigInt`).
pub(crate) fn biginteger_shift_left(
    target: &Value,
    distance: &Value,
) -> Option<Result<Value, RjError>> {
    let b = match target {
        Value::BigInt(b) | Value::BigInteger(b) => &b.0,
        _ => return None,
    };
    let n = match distance {
        Value::Int(n) => *n,
        other => {
            return Some(Err(RjError::type_err(format!(
                ".shiftLeft: expected an int distance, got {}",
                other.type_name()
            ))))
        }
    };
    let shifted = if n >= 0 {
        b << (n as u64)
    } else {
        b >> ((-n) as u64)
    };
    Some(Ok(biginteger_value(shifted)))
}

/// Strict int requirement for the bitwise ops -- unlike [`to_num`]'s
/// generic arithmetic blend, Clojure's `bit-*` ops reject `Float` outright
/// rather than silently truncating it.
fn req_int(v: &Value, _op: &str) -> Result<i64, RjError> {
    match v {
        Value::Int(n) => Ok(*n),
        // Measured: `(bit-and 1N 1)` => `IllegalArgumentException: bit
        // operation not supported for: class clojure.lang.BigInt`. The
        // `{op}: ` prefix real mova errors usually carry is deliberately
        // absent -- Clojure's message names only the offending CLASS.
        // W3a: same measurement the comment above already records --
        // `IllegalArgumentException`, carried as the class now, not just
        // quoted in the message.
        other => Err(RjError::type_err(format!(
            "bit operation not supported for: class {}",
            crate::types::builtin_class_name(other)
        ))
        .with_class(JvmClass::IllegalArgument)),
    }
}

/// Whole-string integer grammar for `parse-long`: an optional leading
/// `+`/`-` followed by one or more ASCII digits, nothing else (no
/// whitespace, no partial matches) -- matches JVM Clojure's `parse-long`
/// contract.
fn looks_like_long(s: &str) -> bool {
    let bytes = s.as_bytes();
    let digits = match bytes.first() {
        Some(b'+' | b'-') => &bytes[1..],
        _ => bytes,
    };
    !digits.is_empty() && digits.iter().all(u8::is_ascii_digit)
}

/// `f.trunc()`, EXCEPT `NaN` truncates to `0.0` rather than staying `NaN` --
/// the JLS narrowing double-to-integral-type conversion rule (JLS 5.1.3),
/// measured here as `(long ##NaN)` => `0`, NOT a throw the way every other
/// out-of-range float is. Infinities are deliberately NOT special-cased:
/// `f64::trunc` leaves `##Inf`/`##-Inf` infinite, so [`cast_integral`]'s own
/// range check (comparing against `min`/`max` as `f64`) rejects them the
/// same way it rejects an ordinary too-large finite float -- measured
/// `(long ##Inf)` => `THROW ... "Value out of range for long: Infinity"`.
pub(crate) fn trunc_float_for_cast(f: f64) -> f64 {
    if f.is_nan() {
        0.0
    } else {
        f.trunc()
    }
}

/// Shared truncating-narrowing-cast core for `byte`/`short`/`int`/`long`
/// (SPEC-C-casts.md), extending the pre-SPEC-C `truncate_to_i64` (which had
/// no range check at all -- `int`/`long` used to just truncate and hope)
/// with the range check real Clojure's `RT.byteCast`/`shortCast`/
/// `intCast`/`longCast` all perform: truncate first, THEN range-check the
/// truncated value against `[min, max]`, throwing `Value out of range for
/// <ty>: <ORIGINAL argument, printed Java-style>` on failure -- the error
/// message names the value the caller passed in, not the (possibly
/// meaningless, e.g. saturated) truncated one. `long`'s call site passes
/// `i64::MIN..=i64::MAX`, i.e. every truncated value is in range UNLESS the
/// source was a float/BigInt/Ratio/BigDec too big to fit an `i64` at all --
/// measured `(long 9.3e18)` throws for exactly that reason.
///
/// Accepts `Int`, `Float`, `Char`, `BigInt`, `Ratio`, `BigDec` (measured
/// table's "byte/short/int/long accept" list); every other `Value` --
/// notably `Str`, which is NEVER castable this way even when it looks like
/// a number (measured `(byte "1")` throws `ClassCastException`) -- falls
/// through to the type-error arm.
// pub(crate), not private: S4's `vector-of` (`builtins::sorted::
// coerce_integral`) reuses this EXACT truncate-then-range-check table for
// `:byte`/`:short`/`:int`/`:long` elements (measured: `(vector-of :int 1M
// 2.0 3.1)` truncates exactly like `(int 3.1)` would) instead of
// re-deriving it.
pub(crate) fn cast_integral(v: &Value, ty: &str, min: i64, max: i64) -> Result<i64, RjError> {
    let range_err = || {
        RjError::type_err(format!("Value out of range for {ty}: {}", crate::printer::display_str(v)))
    };
    // S5 (measured): `(int 12345678901234567890N)` reports `Value out of
    // range for LONG`, not `for int` -- real Clojure's `RT.intCast(Object)`
    // is `intCast(longCast(x))`, so the inner `longCast` throws first for
    // anything that does not fit an `i64` at all. Only that case renames
    // the type; an in-`i64`-but-out-of-`i32` value still says `int`.
    let long_range_err = || {
        RjError::type_err(format!(
            "Value out of range for long: {}",
            crate::printer::display_str(v)
        ))
    };
    let check = |n: i64| if n < min || n > max { Err(range_err()) } else { Ok(n) };
    match v {
        Value::Int(n) => check(*n),
        Value::Char(c) => check(*c as i64),
        Value::Float(f) => {
            let t = trunc_float_for_cast(*f);
            if t < min as f64 || t > max as f64 {
                Err(range_err())
            } else {
                Ok(t as i64)
            }
        }
        // A `BigInt` too large for an `i64` at all is out of range for
        // EVERY integral cast, `long` included -- there is no truncation
        // step for `BigInt` (it's already an integer), only the fit check.
        Value::BigInt(b) | Value::BigInteger(b) => {
            b.to_i64_exact().map_or_else(|| Err(long_range_err()), check)
        }
        // `RatioVal::trunc_to_bigint` truncates toward zero (see its doc);
        // `.to_i64()` is the same "must fit exactly" fit check as `BigInt`
        // above, just on the truncated quotient rather than the raw value.
        Value::Ratio(r) => r.trunc_to_bigint().to_i64().map_or_else(|| Err(range_err()), check),
        Value::BigDec(d) => d.trunc_to_bigint().to_i64().map_or_else(|| Err(range_err()), check),
        other => Err(RjError::type_err(format!(
            "{ty}: expected a number or char, got {}",
            other.type_name()
        ))),
    }
}

/// The `unchecked-byte`/`-short`/`-int`/`-long`/`-char` casts: the same
/// truncate-toward-zero conversion [`cast_integral`] performs, then a
/// two's-complement WRAP to the target width instead of a range check.
/// Measured: `(unchecked-byte 200)` => `-56`, `(unchecked-short 40000)` =>
/// `-25536`, `(unchecked-int 3000000000)` => `-1294967296`,
/// `(unchecked-int 9223372036854775807)` => `-1`, `(unchecked-char 65536)`
/// => the NUL char.
///
/// Two conversions are deliberately not "wrap": a `Float` source saturates
/// at ITS INTERMEDIATE bounds (see below) and sends `NaN` to `0` before any
/// wrapping, which is the JLS narrowing rule and exactly what Rust's `as`
/// does (measured `(unchecked-long 9.3E18)` => `9223372036854775807`, i.e.
/// saturated, not wrapped); and a `BigInt` source keeps its LOW 64 bits,
/// because Java's `BigInteger.longValue()` truncates rather than
/// saturating.
///
/// S7 (measured, `compat/casts-oracle-transcript.txt`): a `Float` source's
/// intermediate width is NOT always 64 bits. JLS 5.1.3's narrowing
/// conversion from a floating type to `byte`/`short`/`char`/`int` is a
/// TWO-STEP conversion -- float/double to a 32-bit `int` FIRST (saturating,
/// same as the `bits == 64` case below but at 32 bits instead of 64), and
/// only THEN (for `byte`/`short`/`char`) a further two's-complement narrow
/// of that 32-bit result. Only `long` (`bits == 64`) widens the
/// intermediate to a full 64 bits. Going through a 64-bit intermediate
/// unconditionally (this function's pre-S7 shape) gives the WRONG answer
/// whenever the source overflows `i32` but the two paths' saturated bit
/// patterns diverge -- concretely, `(unchecked-int Float/MAX_VALUE)`:
/// saturating to `i64::MAX` (`0x7FFF_FFFF_FFFF_FFFF`) and then keeping the
/// low 32 bits gives `-1` (all those bits are `1`), but real Clojure's
/// answer is `Integer/MAX_VALUE` (`2147483647`, i.e. `0x7FFF_FFFF`) --
/// saturating DIRECTLY to `i32` first. The two only coincide by accident
/// for MAX-magnitude inputs at 8/16-bit widths (both saturated patterns
/// are all-`1`s in their low bits there); they diverge for any float that
/// overflows `i32` but not `i64` (e.g. `3e9` unchecked-short-cast: the old
/// code's wide-then-wrap path used the exact, non-saturated `i64` value
/// `3000000000` and wrapped THAT to 16 bits, a different answer than
/// wrapping the correctly-saturated `i32::MAX`).
fn cast_unchecked_integral(v: &Value, ty: &str, bits: u32) -> Result<i64, RjError> {
    let wide: i64 = match v {
        Value::Int(n) => *n,
        Value::Char(c) => *c as i64,
        Value::Float(f) => {
            let t = trunc_float_for_cast(*f);
            if bits == 64 {
                t as i64
            } else {
                // Saturate to the 32-bit `int` intermediate JLS 5.1.3
                // mandates for every non-`long` target, THEN let the
                // shift-based wrap below (which is width-agnostic once the
                // value is already in `i64`) do the second narrowing step.
                (t as i32) as i64
            }
        }
        Value::BigInt(b) | Value::BigInteger(b) => low_64_bits(&b.0),
        Value::Ratio(r) => low_64_bits(&r.trunc_to_bigint()),
        Value::BigDec(d) => low_64_bits(&d.trunc_to_bigint()),
        other => {
            return Err(RjError::type_err(format!(
                "{ty}: expected a number or char, got {}",
                other.type_name()
            )))
        }
    };
    Ok(if bits == 64 {
        wide
    } else {
        // Sign-extend the low `bits` bits: shift the wanted bits up to the
        // top of the word and back down arithmetically.
        let shift = 64 - bits;
        (wide << shift) >> shift
    })
}

/// A `BigInt`'s low 64 bits as an `i64`, two's complement -- Java's
/// `BigInteger.longValue()`. `to_i64` would return `None` for anything
/// that doesn't fit; the `unchecked-*` casts specifically must not care.
fn low_64_bits(n: &BigInt) -> i64 {
    let (sign, digits) = n.to_u32_digits();
    let mut mag: u64 = 0;
    for (i, d) in digits.iter().take(2).enumerate() {
        mag |= (*d as u64) << (32 * i);
    }
    let v = mag as i64;
    if sign == num_bigint::Sign::Minus {
        v.wrapping_neg()
    } else {
        v
    }
}

/// Widening numeric-or-char -> `f64` conversion shared by the `double`/
/// `float` casts (mova has no separate `f32` value type, so both casts
/// land on the same `Value::Float`). SPEC-C-casts.md extends this to also
/// accept `BigInt`/`Ratio`/`BigDec` (measured: `(double 1/2)` => `0.5`,
/// `(double 5N)` => `5.0`, `(double 1.5M)` => `1.5`) via each type's own
/// best-effort `to_f64` -- `double`/`float` never range-check `Int`/`Char`/
/// `BigInt` inputs (an `f64` can represent any `i64` losslessly-enough for
/// this purpose), only `float` adds an f32-overflow check on top, in its
/// own registration below.
pub(crate) fn widen_to_f64(v: &Value, op: &str) -> Result<f64, RjError> {
    match v {
        Value::Int(n) => Ok(*n as f64),
        Value::Float(f) => Ok(*f),
        Value::Char(c) => Ok(*c as u32 as f64),
        Value::BigInt(b) | Value::BigInteger(b) => Ok(b.0.to_f64().unwrap_or(f64::INFINITY)),
        Value::Ratio(r) => Ok(r.to_f64()),
        Value::BigDec(d) => Ok(d.to_f64()),
        other => Err(RjError::type_err(format!(
            "{op}: expected a number or char, got {}",
            other.type_name()
        ))),
    }
}

/// D9: the `^long`/`^double` PARAMETER coercion, i.e. what the JVM runs at
/// the call boundary of a fn with a primitive parameter hint. See
/// [`crate::value::PrimCast`] for where the hint is stored and
/// `eval::apply` for where this is invoked.
///
/// This is deliberately NOT `(long x)`/`(double x)`. Real Clojure's
/// `Compiler$HostExpr.emitUnboxArg` emits **`checkcast java/lang/Number`
/// first**, and only then `RT.longCast(Object)`/`RT.doubleCast(Object)` --
/// so the boundary is strictly narrower than the `clojure.core` casts on
/// exactly one input, `Character`:
///
/// ```text
/// (long \a)                 => 97     ; RT.longCast HAS a Character branch
/// ((fn [^long x] x) \a)     => THROW java.lang.ClassCastException
/// ```
///
/// (measured on 1.13.0-alpha6; `(double \a)` throws the same CCE, because
/// `RT.doubleCast` is a bare `((Number)x).doubleValue()` with no Character
/// branch at all -- the two casts only differ for the `long` side.)
///
/// The full measured grid, all rows reproduced here:
///
/// ```text
/// ^long   <- 5 / (int 7) / (byte 7)      => 5 / 7 / 7        (java.lang.Long)
/// ^long   <- (bigint 5) / (biginteger 5) => 5
/// ^long   <- (bigint 10^23)              => IAE "Value out of range for long: 99999999999999999999999"
/// ^long   <- 1.5 / -1.9 / (float 1.5)    => 1 / -1 / 1       (truncate toward zero)
/// ^long   <- ##NaN                       => 0                (JLS 5.1.3)
/// ^long   <- 1e30 / ##Inf                => IAE "Value out of range for long: 1.0E30" / "... Infinity"
/// ^long   <- 3/2 / -7/2                  => 1 / -3
/// ^long   <- 1M / 1.5M / 1.9M            => 1
/// ^long   <- (bigdec 1e23)               => IAE "Value out of range for long: 1.0E23"  (the DOUBLE, not the bigdec)
/// ^double <- 1 / (bigint 5) / 3/2 / 1.5M => 1.0 / 5.0 / 1.5 / 1.5
/// ^double <- (bigint 10^23)              => 1.0E23           (lossy, never throws)
/// ^double <- (bigdec 1e400M)             => ##Inf
/// both    <- "s" / \a / true / []        => ClassCastException (the checkcast)
/// both    <- nil                         => NullPointerException
/// ```
///
/// The `Number` gate is what makes `cast_integral`'s `Char` arm and its
/// non-number arm both unreachable from here, which is why every remaining
/// `Err` it can return is a range failure and can be re-tagged
/// `IllegalArgument` wholesale.
pub(crate) fn prim_param_cast(v: &Value, cast: crate::value::PrimCast) -> Result<Value, RjError> {
    // `checkcast java/lang/Number`, transcribed. `Char`/`Bool`/`Str`/every
    // collection is NOT a `java.lang.Number` and dies here; `nil` passes the
    // checkcast (null casts to anything) and dies one line later inside
    // `RT.longCast`/`doubleCast`'s `((Number)x).doubleValue()`.
    match v {
        Value::Int(_)
        | Value::Float(_)
        | Value::BigInt(_)
        | Value::BigInteger(_)
        | Value::Ratio(_)
        | Value::BigDec(_) => {}
        Value::Nil => return Err(prim_param_npe()),
        other => return Err(prim_param_cce(other)),
    }
    match cast {
        // `RT.longCast(Object)` dispatches per class: `Long`/`Integer`/
        // `Byte`/`Short` -> `longValue()`, `BigInt`/`BigInteger` -> an
        // exact-fit check, `Ratio` -> `longCast(bigIntegerValue())` -- all
        // four of which `cast_integral` already transcribes. `BigDecimal`
        // matches NONE of them and lands in the trailing
        // `longCast(((Number)x).doubleValue())`, so it is converted to a
        // double FIRST here. That is not cosmetic in either direction:
        // `(bigdec 1e23)` must report `Value out of range for long: 1.0E23`
        // (the double, measured) rather than the bigdec's own 24-digit
        // rendering, and a `BigDecimal` carrying more precision than an
        // `f64` can hold must round to the double BEFORE the range check,
        // not truncate exactly and squeak in under `Long/MAX_VALUE`.
        crate::value::PrimCast::Long => {
            let via_double;
            let src = match v {
                Value::BigDec(d) => {
                    via_double = Value::Float(d.to_f64());
                    &via_double
                }
                _ => v,
            };
            cast_integral(src, "long", i64::MIN, i64::MAX)
                .map(Value::Int)
                // `RT.longCast`'s out-of-range throw is an
                // `IllegalArgumentException`, not the `ClassCastException`
                // `RjError::type_err` defaults to (measured above).
                .map_err(|e| e.with_class(crate::error::JvmClass::IllegalArgument))
        }
        crate::value::PrimCast::Double => {
            // `((Number)x).doubleValue()` is total over every `Number`:
            // lossy for a huge `BigInt`/`BigDec` (saturating to `##Inf`),
            // never a throw. `widen_to_f64`'s error arms are unreachable
            // after the gate above.
            widen_to_f64(v, "double").map(Value::Float)
        }
    }
}

/// D9: rewrite each hinted parameter slot with its `^long`/`^double`
/// coercion, in place. `casts` is parallel to an arity's FIXED parameters
/// only, and `args` may legitimately be longer (a variadic arity's rest
/// slot, or a recur's trailing rest value) -- the `zip`, which stops at the
/// shorter, is load-bearing here, not defensive.
///
/// The single copy every call boundary in EVERY tier goes through: the
/// tree-walker's three entry points (`apply_closure`/`_buf`/
/// `_lazy_rest`) and its self-recur trampoline, and the compiled tier's
/// self-recur back-edge, all call this -- so a call and a recur into the
/// same hinted param can no more disagree about the cast than the tiers can
/// disagree about which arity ran. See `value::Arity::coerce`.
///
/// `#[inline(never)]` is a deliberate code-size guard for the hottest
/// call-dispatch functions in the interpreter: neither tier's call/recur
/// path may grow a loop body for a feature its callers overwhelmingly do
/// not use.
#[inline(never)]
pub(crate) fn coerce_prim_params_in_place(
    casts: &[Option<crate::value::PrimCast>],
    args: &mut [Value],
) -> Result<(), RjError> {
    for (slot, cast) in args.iter_mut().zip(casts) {
        if let Some(c) = cast {
            *slot = prim_param_cast(slot, *c)?;
        }
    }
    Ok(())
}

/// [`coerce_prim_params_in_place`] for a caller that only has a borrowed
/// `&[Value]` (`apply_closure`). Reached only when the selected arity
/// actually carries hints, so the copy it makes is paid for by exactly the
/// fns that asked for primitive parameters and by nobody else.
#[inline(never)]
pub(crate) fn coerce_prim_params(
    casts: &[Option<crate::value::PrimCast>],
    args: &[Value],
) -> Result<Vec<Value>, RjError> {
    let mut out = args.to_vec();
    coerce_prim_params_in_place(casts, &mut out)?;
    Ok(out)
}

/// The `checkcast java/lang/Number` failure, reproduced down to the JVM's
/// module clause -- [`class_cast_err`] with `java.lang.Number` as the
/// (boot-loaded) target.
fn prim_param_cce(v: &Value) -> RjError {
    class_cast_err(v, "java.lang.Number", true).with_class(crate::error::JvmClass::ClassCast)
}

/// The `nil` arm: a helpful-NPE message naming `RT.longCast`/`doubleCast`'s
/// own parameter, which is literally called `x` in `RT.java` -- so this text
/// is constant, not derived from the mova parameter's name (measured
/// identical for `^long` and `^double`, and for every parameter name).
fn prim_param_npe() -> RjError {
    RjError::type_err(
        "Cannot invoke \"java.lang.Number.doubleValue()\" because \"x\" is null".to_string(),
    )
    .with_class(crate::error::JvmClass::NullPointer)
}

/// `char` cast (SPEC-C-casts.md): accepts `Char` (identity), `Int`, and
/// `Float` (truncated toward zero, `NaN` -> `0` -- same [`trunc_float_for_cast`]
/// rule the integral casts use; measured `(char 65.5)` => `\A`). Valid
/// range is Java's `char` width, `0..=0xFFFF` (measured `(char 1114112)`
/// throws even though `1114112` is a valid Unicode scalar value -- Clojure's
/// `char` cast is a 16-bit Java `char`, not a full Unicode codepoint cast).
///
/// KNOWN DIVERGENCE (documented, not a bug): real Java's `char` is a bare
/// 16-bit code unit, so it CAN hold a lone surrogate (`0xD800..=0xDFFF`)
/// that isn't part of a valid UTF-16 pair. Rust's `char` cannot -- it's a
/// Unicode scalar value by construction, and `char::from_u32` returns
/// `None` for that whole range. So mova's `char` cast is narrower than
/// real Clojure's on exactly the surrogate range: a form real Clojure would
/// accept (`(char 0xD800)`) throws here instead. There is no `Value` this
/// cast could return in that case even if it wanted to -- mova has no
/// lone-surrogate representation -- so the divergence is structural, not a
/// missed range check.
pub(crate) fn cast_char(v: &Value) -> Result<Value, RjError> {
    let code: i64 = match v {
        Value::Char(c) => return Ok(Value::Char(*c)),
        Value::Int(n) => *n,
        Value::Float(f) => trunc_float_for_cast(*f) as i64,
        other => {
            return Err(RjError::type_err(format!(
                "char: expected an int, float, or char, got {}",
                other.type_name()
            )))
        }
    };
    if !(0..=0xFFFF).contains(&code) {
        return Err(RjError::type_err(format!(
            "Value out of range for char: {}",
            crate::printer::display_str(v)
        )));
    }
    char::from_u32(code as u32).map(Value::Char).ok_or_else(|| {
        RjError::type_err(format!("Value out of range for char: {}", crate::printer::display_str(v)))
    })
}

/// `codepoint-str` (wishlist #9, owner-ruled: core fn, Clojure-faithful
/// spelling): mova-native EXTENSION, no real-Clojure equivalent. Turns a
/// full Unicode scalar codepoint into its one-character string, closing
/// the astral gap `cast_char` (above) deliberately leaves open: `char`
/// stays clamped to `0..=0xFFFF` to match real Clojure's 16-bit Java
/// `char` cast, so there is NO core spelling today that can turn
/// `0x1F600` (an emoji, 😀) into a mova value at all -- even though
/// `Value::Char` is a Rust `char` and has been astral-capable since day
/// one. This matters concretely for a client whose editor host receives
/// emoji/astral codepoints one keystroke at a time: with only `char`
/// available, that codepoint has no way to become a mova value.
///
/// Real Clojure has no equivalent: the JVM path is `(String.
/// (Character/toChars cp))`, which round-trips through a `char[]` (Java's
/// UTF-16 surrogate-pair encoding) and works only because Java strings
/// ARE UTF-16. mova strings are Rust `String`s (UTF-8, one `char` per
/// Unicode scalar value, no surrogates), so the direct spelling here --
/// `char::from_u32` straight into a one-`char` `String` -- is the honest
/// mova-native shape, not a veneer over a JVM idiom that doesn't apply.
/// Per the interop directive (CLOJURE-COMPAT-STARTER.md:145: "interop
/// targets Rust types/ecosystem; `java.*` names are a compatibility
/// veneer over mova-native values, never JVM emulation deeper than the
/// vendored suite measurably demands"), this ships the direct spelling
/// rather than a `Character/toChars`-shaped veneer nothing needs.
///
/// Accepts `Value::Int` (the codepoint as a number) or `Value::Char`
/// (already a scalar value, trivially in range -- returned as its
/// one-character string). Does NOT accept `Float`, unlike `cast_char`: a
/// codepoint is a discrete identity, not a truncatable magnitude, and
/// there's no plausible call site handing this a float.
///
/// Valid range is `0..=0x10FFFF` (do NOT clamp to `0xFFFF` -- that's
/// `char`'s narrower range, not this fn's) EXCLUDING the surrogate range
/// `0xD800..=0xDFFF`: those code points are UTF-16 plumbing with no
/// scalar-value meaning of their own, and mova (like Rust) has no
/// representation for a lone surrogate -- same structural gap
/// `cast_char`'s KNOWN DIVERGENCE note describes.
pub(crate) fn codepoint_str(v: &Value) -> Result<Value, RjError> {
    let code: i64 = match v {
        Value::Char(c) => return Ok(Value::Str(crate::value::Str::from(c.to_string()))),
        Value::Int(n) => *n,
        other => {
            return Err(RjError::type_err(format!(
                "codepoint-str: expected an int or char, got {}",
                other.type_name()
            )))
        }
    };
    if !(0..=0x0010_FFFF).contains(&code) {
        return Err(RjError::type_err(format!(
            "codepoint-str: {} is out of range for a Unicode codepoint (valid range 0..=0x10FFFF)",
            code
        )));
    }
    char::from_u32(code as u32)
        .map(|c| Value::Str(crate::value::Str::from(c.to_string())))
        .ok_or_else(|| {
            RjError::type_err(format!(
                "codepoint-str: {} is a lone surrogate (0xD800..=0xDFFF), which has no \
                 Unicode scalar value and cannot be represented",
                code
            ))
        })
}

/// `bigint` cast (SPEC-C-casts.md): ALWAYS returns `Value::BigInt`, unlike
/// `byte`/`short`/`int`/`long` which stay `Value::Int` (there is no
/// range check -- a `BigInt` can hold any size). Accepts everything the
/// integral casts do (`Int`, `Float` truncated, `BigInt`, `Ratio`
/// truncated, `BigDec` truncated) PLUS `Str` (measured `(bigint "123")` =>
/// `123N`) -- the one type `byte`/`short`/`int`/`long` explicitly reject
/// (measured `(byte "1")` throws `ClassCastException`). This asymmetry is
/// real Clojure's, not an inconsistency in this file: `bigint` and
/// `biginteger` both accept numeric strings; the fixed-width casts never
/// do.
fn cast_bigint(v: &Value) -> Result<BigIntVal, RjError> {
    match v {
        Value::Int(n) => Ok(BigIntVal::from_i64(*n)),
        Value::Float(f) => {
            let t = trunc_float_for_cast(*f);
            BigInt::from_f64(t).map(BigIntVal).ok_or_else(|| {
                RjError::type_err(format!(
                    "bigint: {} has no finite integer representation",
                    crate::printer::display_str(v)
                ))
            })
        }
        Value::BigInt(b) | Value::BigInteger(b) => Ok(BigIntVal(b.0.clone())),
        Value::Ratio(r) => Ok(BigIntVal(r.trunc_to_bigint())),
        Value::BigDec(d) => Ok(BigIntVal(d.trunc_to_bigint())),
        // Measured: `(bigint "123")` => `123N`; an invalid string is a
        // NumberFormatException on the JVM -- this driver doesn't need to
        // match that message text, just route to SOME error (per
        // SPEC-C-casts.md: "check nothing, just route to an error").
        Value::Str(s) => {
            s.parse::<BigInt>().map(BigIntVal).map_err(|_| RjError::type_err(format!("bigint: invalid number: {s}")))
        }
        other => Err(RjError::type_err(format!("bigint: expected a number or string, got {}", other.type_name()))),
    }
}

/// `bigdec` cast (SPEC-C-casts.md): ALWAYS returns `Value::BigDec`.
///
/// - `Int`/`BigInt` -> scale `0` (exact, no rounding possible).
/// - `Float` -> parsed from `f`'s OWN Java-style `Double.toString`
///   rendering (`printer::write_finite_float_java`, reused rather than
///   reimplemented -- see that function's doc), matching Clojure's
///   `(bigdec 1.5)` going through `BigDecimal.valueOf(double)` ==
///   `new BigDecimal(Double.toString(1.5))`, NOT `new BigDecimal(1.5)`
///   (which would round-trip the double's exact binary value and produce
///   a very different, very long decimal). `NaN`/infinite floats have no
///   such string (Java's `BigDecimal(double)` constructor itself throws
///   `NumberFormatException` for them) -- rejected up front rather than
///   guessed at, since SPEC-C-casts.md's measured table has no row for
///   this case.
/// - `Ratio` -> [`RatioVal::to_exact_bigdec`] (exact decimal expansion, or
///   an `ArithmeticException`-shaped error when the fraction doesn't
///   terminate in base 10 -- measured `(bigdec 1/3)` throws exactly the
///   message this uses).
/// - `BigDec` -> itself (already the target type).
/// - `Str` -> `BigDecVal::parse`, which preserves scale from the string's
///   own digits (measured `(bigdec "1.50")` => `1.50M`, not `1.5M`).
fn cast_bigdec(v: &Value) -> Result<BigDecVal, RjError> {
    match v {
        Value::Int(n) => Ok(BigDecVal::new(BigInt::from(*n), 0)),
        Value::Float(f) => {
            if !f.is_finite() {
                return Err(RjError::type_err(format!(
                    "bigdec: {} has no exact decimal representation",
                    crate::printer::display_str(v)
                )));
            }
            let mut s = String::new();
            crate::printer::write_finite_float_java(*f, &mut s);
            BigDecVal::parse(&s).ok_or_else(|| RjError::other(format!("bigdec: could not parse {s:?}")))
        }
        Value::BigInt(b) | Value::BigInteger(b) => Ok(BigDecVal::new(b.0.clone(), 0)),
        Value::Ratio(r) => r.to_exact_bigdec().ok_or_else(|| {
            RjError::other(
                "Non-terminating decimal expansion; no exact representable decimal result.".to_string(),
            )
        }),
        Value::BigDec(d) => Ok(BigDecVal::new(d.unscaled().clone(), d.scale())),
        Value::Str(s) => {
            BigDecVal::parse(s).ok_or_else(|| RjError::type_err(format!("bigdec: invalid number: {s}")))
        }
        other => Err(RjError::type_err(format!("bigdec: expected a number or string, got {}", other.type_name()))),
    }
}

/// SPEC-W1 task 7: `(BigDecimal/valueOf x)` -- the STATIC factory, which
/// is NOT `(BigDecimal. x)`. For a `double` real Java's `valueOf` is
/// documented as `new BigDecimal(Double.toString(val))`, i.e. the SHORTEST
/// round-tripping decimal (`BigDecimal/valueOf 0.1` => `0.1M`), where the
/// `double` CONSTRUCTOR takes the exact binary value
/// (`0.1000000000000000055511151231257827021181583404541015625M`) -- see
/// `hostclass::bigdecimal_ctor`'s doc for that side. `cast_bigdec`'s
/// `Float` arm already computes exactly `Double.toString` (via
/// `printer::write_finite_float_java`) and its `Int` arm is the exact
/// integer, so the two `valueOf` overloads mova can be handed are this
/// one function.
///
/// Needed by `clojure.test.check.generators`' own gen-builtins
/// (`#(BigDecimal/valueOf %)` behind `gen/big-decimal`); registered here
/// rather than in `builtins::statics` so it sits next to the `cast_bigdec`
/// it delegates to.
pub(crate) fn bigdecimal_value_of(v: &Value) -> Result<Value, RjError> {
    Ok(Value::BigDec(Arc::new(cast_bigdec(v)?)))
}

pub fn register(i: &mut Interp) {
    reg(i, "+", ArityHint::Any, |i, args| fold_nary(i, args, Value::Int(0), add_step));
    reg(i, "*", ArityHint::Any, |i, args| fold_nary(i, args, Value::Int(1), mul_step));

    reg(i, "-", ArityHint::Min(1), |i, args| {
        if args.len() == 1 {
            // Unary `-` is `0 - x` at every rank -- `(- 5N)` => `-5N`,
            // `(- 1/2)` => `-1/2`, `(- 1.5M)` => `-1.5M`, all measured --
            // and it CHECKS BY DEFAULT (transcript row 6), same
            // overflow-path-only W4C consultation as `sub2` itself.
            return sub2(i, &Value::Int(0), &args[0]);
        }
        let mut acc = args[0].clone();
        for a in &args[1..] {
            acc = sub2(i, &acc, a)?;
        }
        Ok(acc)
    });

    reg(i, "/", ArityHint::Min(1), |_i, args| {
        // Every argument is type-checked BEFORE the first division, so a
        // trailing non-number wins over a leading division by zero.
        for a in args {
            if tower_of(a).is_none() {
                return Err(not_a_number(a, "/"));
            }
        }
        if args.len() == 1 {
            // `(/ x)` is `1/x` -- measured `(/ 3/2)` => `2/3`,
            // `(/ 4M)` => `0.25M`, `(/ 1/2)` => `2`.
            return tower_div(&Value::Int(1), &args[0]);
        }
        let mut acc = args[0].clone();
        for a in &args[1..] {
            acc = div2(&acc, a)?;
        }
        Ok(acc)
    });

    reg(i, "<", ArityHint::Min(1), |_i, args| cmp_chain(args, "<", lt));
    reg(i, ">", ArityHint::Min(1), |_i, args| cmp_chain(args, ">", gt));
    reg(i, "<=", ArityHint::Min(1), |_i, args| cmp_chain(args, "<=", le));
    reg(i, ">=", ArityHint::Min(1), |_i, args| cmp_chain(args, ">=", ge));

    reg(i, "=", ArityHint::Min(1), |interp, args| {
        for w in args.windows(2) {
            if !interp.values_equal(&w[0], &w[1])? {
                return Ok(Value::Bool(false));
            }
        }
        Ok(Value::Bool(true))
    });

    reg(i, "list", ArityHint::Any, |_i, args| {
        Ok(Value::List(args.iter().cloned().collect()))
    });

    reg(i, "not=", ArityHint::Min(1), |interp, args| {
        for w in args.windows(2) {
            if !interp.values_equal(&w[0], &w[1])? {
                return Ok(Value::Bool(true));
            }
        }
        Ok(Value::Bool(false))
    });

    reg(i, "inc", ArityHint::Exact(1), |i, args| inc1(i, &args[0]));

    reg(i, "dec", ArityHint::Exact(1), |i, args| dec1(i, &args[0]));

    reg(i, "quot", ArityHint::Exact(2), |_i, args| tower_quot(&args[0], &args[1]));
    reg(i, "rem", ArityHint::Exact(2), |_i, args| tower_rem(&args[0], &args[1]));
    reg(i, "mod", ArityHint::Exact(2), |_i, args| tower_mod(&args[0], &args[1]));

    // `abs` preserves the operand's type across the whole tower (measured:
    // `(abs -3N)` => `3N`, `(abs -1/2)` => `1/2`, `(abs -1.5M)` => `1.5M`).
    // The one surprise is `Long`: `(abs -9223372036854775808)` is
    // `-9223372036854775808`, NOT a throw and NOT a promotion (transcript
    // row 7) -- `Math.abs` on `Long.MIN_VALUE` simply wraps, so this
    // deliberately uses `wrapping_abs` rather than `checked_abs`.
    reg(i, "abs", ArityHint::Exact(1), |_i, args| match &args[0] {
        Value::Int(x) => Ok(Value::Int(x.wrapping_abs())),
        Value::Float(f) => Ok(Value::Float(f.abs())),
        Value::BigInt(b) | Value::BigInteger(b) => Ok(bigint_value(b.0.abs())),
        Value::Ratio(r) => Ok(Value::Ratio(Arc::new(
            match RatioVal::reduce(r.numer().abs(), r.denom().clone()) {
                Ok(Reduced::Ratio(x)) => x,
                // |n|/d with d > 1 and gcd(n,d) == 1 can never collapse to
                // an integer, so this is unreachable in practice; falling
                // back to the original keeps it total rather than
                // panicking.
                _ => (**r).clone(),
            },
        ))),
        Value::BigDec(d) => Ok(Value::BigDec(Arc::new(d.abs()))),
        other => Err(not_a_number(other, "abs")),
    });
    // ns: restrict qualified->bare fallback to clojure.core spellings
    // (DESIGN-flow-namespace.md item 5): `(Math/abs -3)` used to reach this
    // bare `abs` purely through `for_each_global_candidate`'s old
    // unconditional trailing bare-name probe -- this module never
    // registered anything under "Math"/"java.lang.Math" at all. Once that
    // probe stopped firing for a non-`clojure.core` expanded namespace,
    // `Math/abs` started throwing "Unable to resolve" (measured:
    // `tests/ns_test.rs`'s embedded-namespace-requires case, reached via
    // `clojure.test.check.generators`' own internal `Math/abs` call). Same
    // fix shape as `builtins::math`'s identical comment: a real entry
    // under both class spellings, matching `abs`'s exact Java name.
    crate::builtins::strings::alias(i, "Math", "abs");
    crate::builtins::strings::alias(i, "java.lang.Math", "abs");

    // `clojure.core/max`/`min` are `(if (> x y) x y)` / `(if (< x y) x y)`
    // folds, and that exact shape is observable: the comparison decides,
    // but the RESULT is always one of the original operand objects, tie
    // included -- so `(min 1.0 1)` is the Long `1` (the `y` of a
    // non-`<` pair) while `(max 1 1.0)` is the Double `1.0`, both
    // measured. Writing it as a fold over `Numbers.min`/`max` rather than
    // "track the best f64 seen" is what makes that fall out.
    reg(i, "max", ArityHint::Min(1), |_i, args| {
        let mut best = args[0].clone();
        tower_of(&best).ok_or_else(|| not_a_number(&best, "max"))?;
        for a in &args[1..] {
            best = min_max_step(&best, a, ">", "max")?;
        }
        Ok(best)
    });

    reg(i, "min", ArityHint::Min(1), |_i, args| {
        let mut best = args[0].clone();
        tower_of(&best).ok_or_else(|| not_a_number(&best, "min"))?;
        for a in &args[1..] {
            best = min_max_step(&best, a, "<", "min")?;
        }
        Ok(best)
    });
    // ns: restrict qualified->bare fallback to clojure.core spellings
    // (DESIGN-flow-namespace.md item 5): same orphaned-spelling shape as
    // this fn's own `abs` alias two blocks up -- `(Math/max 1 2)` used to
    // reach this bare `max`/`min` purely through the old unconditional
    // trailing bare-name probe, and this module never registered anything
    // under "Math"/"java.lang.Math" for them. Real entries under both
    // class spellings, matching `Math.max`/`Math.min`'s exact Java names
    // (same-spelling single words, so no camelCase rename is needed).
    crate::builtins::strings::alias(i, "Math", "max");
    crate::builtins::strings::alias(i, "java.lang.Math", "max");
    crate::builtins::strings::alias(i, "Math", "min");
    crate::builtins::strings::alias(i, "java.lang.Math", "min");

    // `==` -- numeric-value equality across every rank, the complement of
    // the category-strict `=` (transcript rows 32, 52, 76).
    reg(i, "==", ArityHint::Min(1), |_i, args| {
        for a in args {
            if tower_of(a).is_none() {
                return Err(not_a_number(a, "=="));
            }
        }
        for w in args.windows(2) {
            if !num_equiv(&w[0], &w[1])? {
                return Ok(Value::Bool(false));
            }
        }
        Ok(Value::Bool(true))
    });

    // The `'`-suffixed promoting family (transcript rows 8-11). Same
    // identities and same folds as the checked ops -- only the overflow
    // branch differs.
    reg(i, "+'", ArityHint::Any, |_i, args| {
        let mut acc = Value::Int(0);
        for a in args {
            acc = promoting(&acc, a, PromOp::Add, "+")?;
        }
        Ok(acc)
    });
    reg(i, "*'", ArityHint::Any, |_i, args| {
        let mut acc = Value::Int(1);
        for a in args {
            acc = promoting(&acc, a, PromOp::Mul, "*")?;
        }
        Ok(acc)
    });
    reg(i, "-'", ArityHint::Min(1), |_i, args| {
        if args.len() == 1 {
            return promoting(&Value::Int(0), &args[0], PromOp::Sub, "-");
        }
        let mut acc = args[0].clone();
        for a in &args[1..] {
            acc = promoting(&acc, a, PromOp::Sub, "-")?;
        }
        Ok(acc)
    });
    reg(i, "inc'", ArityHint::Exact(1), |_i, args| {
        promoting(&args[0], &Value::Int(1), PromOp::Add, "inc")
    });
    reg(i, "dec'", ArityHint::Exact(1), |_i, args| {
        promoting(&args[0], &Value::Int(1), PromOp::Sub, "dec")
    });

    // `unchecked-*` (transcript rows 12-17): two's-complement wraparound.
    reg(i, "unchecked-add", ArityHint::Exact(2), |_i, args| {
        unchecked(&args[0], &args[1], PromOp::Add)
    });
    reg(i, "unchecked-subtract", ArityHint::Exact(2), |_i, args| {
        unchecked(&args[0], &args[1], PromOp::Sub)
    });
    reg(i, "unchecked-multiply", ArityHint::Exact(2), |_i, args| {
        unchecked(&args[0], &args[1], PromOp::Mul)
    });
    reg(i, "unchecked-inc", ArityHint::Exact(1), |_i, args| {
        unchecked(&args[0], &Value::Int(1), PromOp::Add)
    });
    reg(i, "unchecked-dec", ArityHint::Exact(1), |_i, args| {
        unchecked(&args[0], &Value::Int(1), PromOp::Sub)
    });
    reg(i, "unchecked-negate", ArityHint::Exact(1), |_i, args| {
        unchecked(&Value::Int(0), &args[0], PromOp::Sub)
    });
    // `unchecked-divide-int`/`unchecked-remainder-int` do NOT wrap: they
    // are plain integer division (measured `(unchecked-divide-int 7 2)` =>
    // `3`), so they route to the ordinary `quot`/`rem`.
    // The `-int` suffixed family: real Clojure's are 32-bit ops returning
    // a `java.lang.Integer` (measured `(unchecked-add-int 1 2)` => `3`).
    // mova has one integer width, so the WRAPAROUND is done at 32 bits --
    // which is the observable part -- and the result is an ordinary
    // `Value::Int`; see the int-width note in
    // tests/conformance/DEVIATIONS.md for the class-name half.
    reg(i, "unchecked-add-int", ArityHint::Exact(2), |_i, args| {
        unchecked_int32(&args[0], &args[1], PromOp::Add)
    });
    reg(i, "unchecked-subtract-int", ArityHint::Exact(2), |_i, args| {
        unchecked_int32(&args[0], &args[1], PromOp::Sub)
    });
    reg(i, "unchecked-multiply-int", ArityHint::Exact(2), |_i, args| {
        unchecked_int32(&args[0], &args[1], PromOp::Mul)
    });
    reg(i, "unchecked-inc-int", ArityHint::Exact(1), |_i, args| {
        unchecked_int32(&args[0], &Value::Int(1), PromOp::Add)
    });
    reg(i, "unchecked-dec-int", ArityHint::Exact(1), |_i, args| {
        unchecked_int32(&args[0], &Value::Int(1), PromOp::Sub)
    });
    reg(i, "unchecked-negate-int", ArityHint::Exact(1), |_i, args| {
        unchecked_int32(&Value::Int(0), &args[0], PromOp::Sub)
    });
    reg(i, "unchecked-divide-int", ArityHint::Exact(2), |_i, args| {
        tower_quot(&args[0], &args[1])
    });
    reg(i, "unchecked-remainder-int", ArityHint::Exact(2), |_i, args| {
        tower_rem(&args[0], &args[1])
    });

    // `numerator`/`denominator` are Ratio-ONLY and return a
    // `java.math.BigInteger`, not a `clojure.lang.BigInt` -- which is why
    // `(numerator 1/3)` prints `1` and not `1N` (transcript rows 42-43).
    // A non-Ratio argument is a ClassCastException on the JVM; mova has no
    // exception-class taxonomy, so the message text is reproduced and the
    // kind is an ordinary type error (transcript row 89).
    reg(i, "numerator", ArityHint::Exact(1), |_i, args| match &args[0] {
        Value::Ratio(r) => Ok(biginteger_value(r.numer().clone())),
        other => Err(ratio_cast_err(other)),
    });
    reg(i, "denominator", ArityHint::Exact(1), |_i, args| match &args[0] {
        Value::Ratio(r) => Ok(biginteger_value(r.denom().clone())),
        other => Err(ratio_cast_err(other)),
    });

    // `biginteger`: same accepted inputs as `bigint` (numbers of every
    // rank, plus a numeric string), different result TYPE.
    reg(i, "biginteger", ArityHint::Exact(1), |_i, args| {
        Ok(biginteger_value(cast_bigint(&args[0])?.0))
    });

    // `rationalize` (transcript rows 98-99): exact for a `Double`, because
    // it goes through the double's own SHORTEST decimal rendering
    // (`BigDecimal.valueOf`), not its exact binary value -- measured
    // `(rationalize 0.1)` => `1/10`, not the 55-digit binary truth.
    // Integers pass through untouched (`(rationalize 3)` => the Long `3`).
    reg(i, "rationalize", ArityHint::Exact(1), |_i, args| rationalize(&args[0]));

    // `with-precision`'s runtime half -- see `core/core.mova` for the
    // macro that calls it and [`MATH_CONTEXT`] for why the context is a
    // thread-local rather than a threaded parameter.
    reg(i, "with-precision*", ArityHint::Exact(3), |interp, args| {
        let precision = match &args[0] {
            Value::Int(n) if *n >= 0 => *n as u64,
            other => {
                return Err(RjError::type_err(format!(
                    "with-precision: precision must be a non-negative integer, got {}",
                    crate::printer::display_str(other)
                )))
            }
        };
        let mode = match &args[1] {
            Value::Str(s) => RoundingMode::parse(s).ok_or_else(|| {
                RjError::type_err(format!("with-precision: no such rounding mode: {s}"))
            })?,
            other => {
                return Err(RjError::type_err(format!(
                    "with-precision: :rounding must name a RoundingMode, got {}",
                    other.type_name()
                )))
            }
        };
        with_precision_scope(interp, precision, mode, &args[2])
    });

    // `nil` on anything that isn't a whole-string valid integer/float
    // literal -- never an error -- matching JVM Clojure's `parse-long`/
    // `parse-double` contract (they're the safe alternative to a bare
    // `Long/parseLong` that throws).
    reg(i, "parse-long", ArityHint::Exact(1), |_i, args| Ok(match &args[0] {
        Value::Str(s) if looks_like_long(s) => s.parse::<i64>().map(Value::Int).unwrap_or(Value::Nil),
        _ => Value::Nil,
    }));

    reg(i, "parse-double", ArityHint::Exact(1), |_i, args| Ok(match &args[0] {
        // `str::parse::<f64>` already rejects leading/trailing whitespace
        // and requires the WHOLE string to match, so no separate grammar
        // check is needed here the way `parse-long` needs `looks_like_long`
        // (Rust's integer parser is stricter about leading `+` than we
        // want, which is why that one gets its own check).
        Value::Str(s) if !s.is_empty() => s.parse::<f64>().map(Value::Float).unwrap_or(Value::Nil),
        _ => Value::Nil,
    }));

    // `parse-uuid` (Clojure 1.11), S7 tail wave: `nil` on a syntactically
    // invalid UUID string (measured: `(parse-uuid "BOGUS")` => `nil`, NOT
    // an error -- same "safe alternative to a throwing parse" contract as
    // `parse-long`/`parse-double` above), but a non-`Str` ARGUMENT throws
    // (measured: `(parse-uuid 123)`/`(parse-uuid nil)` both throw) -- unlike
    // `parse-long`/`parse-double`, which quietly return `nil` for a
    // non-string argument too. `Value::parse_uuid` is the same accept
    // grammar `java.util.UUID/fromString` already uses (see that static's
    // doc in `builtins::statics`).
    reg(i, "parse-uuid", ArityHint::Exact(1), |_i, args| match &args[0] {
        Value::Str(s) => Ok(Value::parse_uuid(s)
            .map(|bits| Value::Uuid(Arc::new(bits)))
            .unwrap_or(Value::Nil)),
        other => Err(RjError::type_err(format!(
            "parse-uuid: expected a string, got {}",
            other.type_name()
        ))),
    });

    // `parse-boolean` (Clojure 1.11), S7 tail wave: exactly `"true"`/
    // `"false"` parse to the matching boolean (measured `identical?` --
    // i.e. the real singleton `true`/`false`, not merely `=`-equal, which
    // `Value::Bool` already gives for free); any other string is `nil`
    // (measured: `"TRUE"`, `"FALSE"`, `" true "` all `nil` -- case- and
    // whitespace-sensitive, no `trim`); a non-`Str` argument throws
    // (measured: `(parse-boolean nil)`, `(parse-boolean false)`,
    // `(parse-boolean true)`, `(parse-boolean 100)` all throw --
    // `Boolean/parseBoolean` only overloads on `String`, there is no
    // identity/no-op passthrough for an already-boolean argument).
    reg(i, "parse-boolean", ArityHint::Exact(1), |_i, args| match &args[0] {
        Value::Str(s) => Ok(match s.as_ref() {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            _ => Value::Nil,
        }),
        other => Err(RjError::type_err(format!(
            "parse-boolean: expected a string, got {}",
            other.type_name()
        ))),
    });

    // `bit-and`/`bit-or`/`bit-xor` are variadic in real Clojure (`clojure.
    // core`'s own defs delegate to a Java-side varargs reduce), min arity
    // 2 -- measured: `(bit-and 7 3 1)` => `1`, `(bit-and 7)` => ArityException
    // ("Wrong number of args (1)"), `(bit-and)` => ArityException too.
    // `ArityHint::Min(2)` + a left-to-right fold over `args[1..]` gives
    // both the arity floor and the variadic reduction in one shot.
    reg(i, "bit-and", ArityHint::Min(2), |_i, args| {
        let mut acc = req_int(&args[0], "bit-and")?;
        for a in &args[1..] {
            acc &= req_int(a, "bit-and")?;
        }
        Ok(Value::Int(acc))
    });
    reg(i, "bit-or", ArityHint::Min(2), |_i, args| {
        let mut acc = req_int(&args[0], "bit-or")?;
        for a in &args[1..] {
            acc |= req_int(a, "bit-or")?;
        }
        Ok(Value::Int(acc))
    });
    reg(i, "bit-xor", ArityHint::Min(2), |_i, args| {
        let mut acc = req_int(&args[0], "bit-xor")?;
        for a in &args[1..] {
            acc ^= req_int(a, "bit-xor")?;
        }
        Ok(Value::Int(acc))
    });
    reg(i, "bit-not", ArityHint::Exact(1), |_i, args| Ok(Value::Int(!req_int(&args[0], "bit-not")?)));
    reg(i, "bit-shift-left", ArityHint::Exact(2), |_i, args| {
        Ok(Value::Int(req_int(&args[0], "bit-shift-left")?.wrapping_shl(req_int(&args[1], "bit-shift-left")? as u32)))
    });
    reg(i, "bit-shift-right", ArityHint::Exact(2), |_i, args| {
        Ok(Value::Int(req_int(&args[0], "bit-shift-right")?.wrapping_shr(req_int(&args[1], "bit-shift-right")? as u32)))
    });
    // S5 bit-op stragglers -- each measured against the pinned oracle
    // (`tests/conformance/corpus/bit-ops.corpus`) before being written.
    // All five are fixed 2-arity (`unsigned-bit-shift-right` in
    // particular is NOT variadic like `bit-and`/`bit-or`/`bit-xor` --
    // measured `(unsigned-bit-shift-right 1 2 3)` => ArityException).
    //
    // Shift/mask semantics all match `bit-shift-left`/`-right` above:
    // `wrapping_shl`/`wrapping_shr`'s shift-amount argument is masked to
    // the operand's bit width (64 for `i64`/`u64`) by Rust itself, and
    // the `as u32` truncation of a negative or >=64 `i64` shift distance
    // preserves exactly its low 6 bits -- the same two-step reduction the
    // JVM's own `<<`/`>>`/`>>>` bytecodes perform (`shiftDistance &
    // 0x3f`). Measured: `(bit-shift-left 1 64)` => `1`, `(bit-shift-left 1
    // -1)` => `Long/MIN_VALUE`, `(unsigned-bit-shift-right 1 64)` => `1` --
    // all reproduced by this same masking, so `unsigned-bit-shift-right`
    // needs no extra range-checking logic here.
    reg(i, "unsigned-bit-shift-right", ArityHint::Exact(2), |_i, args| {
        let x = req_int(&args[0], "unsigned-bit-shift-right")? as u64;
        let n = req_int(&args[1], "unsigned-bit-shift-right")? as u32;
        Ok(Value::Int(x.wrapping_shr(n) as i64))
    });
    // `bit-set`/`bit-clear`/`bit-flip`: `x <op> (1L << n)`, matching
    // Clojure's own Java-side `Numbers`/`clojure.core` defs bit-for-bit
    // (signed shift, same masking as `bit-shift-left` above -- measured
    // `(bit-set -1 3)` => `-1` (every bit already set) and `(bit-set 0
    // 3)` => `8`).
    reg(i, "bit-set", ArityHint::Exact(2), |_i, args| {
        let x = req_int(&args[0], "bit-set")?;
        let n = req_int(&args[1], "bit-set")? as u32;
        Ok(Value::Int(x | 1i64.wrapping_shl(n)))
    });
    reg(i, "bit-clear", ArityHint::Exact(2), |_i, args| {
        let x = req_int(&args[0], "bit-clear")?;
        let n = req_int(&args[1], "bit-clear")? as u32;
        Ok(Value::Int(x & !1i64.wrapping_shl(n)))
    });
    reg(i, "bit-flip", ArityHint::Exact(2), |_i, args| {
        let x = req_int(&args[0], "bit-flip")?;
        let n = req_int(&args[1], "bit-flip")? as u32;
        Ok(Value::Int(x ^ 1i64.wrapping_shl(n)))
    });
    // `bit-test`: `((x >>> n) & 1) != 0` -- UNSIGNED shift (Clojure's own
    // Java source uses `>>>` here, not `>>`), so a negative `x`'s test
    // bit N is read correctly instead of being sign-extended into every
    // high bit.
    reg(i, "bit-test", ArityHint::Exact(2), |_i, args| {
        let x = req_int(&args[0], "bit-test")? as u64;
        let n = req_int(&args[1], "bit-test")? as u32;
        Ok(Value::Bool((x.wrapping_shr(n) & 1) != 0))
    });

    // Ranges per SPEC-C-casts.md's measured table: `byte` [-128,127],
    // `short` [-32768,32767], `int` = i32's range, `long` = i64's full
    // range (see `cast_integral`'s doc for why `long` can still throw:
    // a float/BigInt/Ratio/BigDec too big to fit ANY i64 at all).
    reg(i, "byte", ArityHint::Exact(1), |_i, args| Ok(Value::Int(cast_integral(&args[0], "byte", -128, 127)?)));
    reg(i, "short", ArityHint::Exact(1), |_i, args| {
        Ok(Value::Int(cast_integral(&args[0], "short", -32768, 32767)?))
    });
    reg(i, "int", ArityHint::Exact(1), |_i, args| {
        Ok(Value::Int(cast_integral(&args[0], "int", i32::MIN as i64, i32::MAX as i64)?))
    });
    reg(i, "long", ArityHint::Exact(1), |_i, args| {
        Ok(Value::Int(cast_integral(&args[0], "long", i64::MIN, i64::MAX)?))
    });

    reg(i, "double", ArityHint::Exact(1), |_i, args| Ok(Value::Float(widen_to_f64(&args[0], "double")?)));
    // mova has one float width (`Value::Float` = `f64`), so `float` is
    // `double`'s near-twin PLUS an actual f32 round-trip -- an EARLIER
    // version of this comment claimed the round-trip "buys nothing" (Java's
    // `(float 1.1)` prints `1.1` too), which is true for PRINTING but wrong
    // for numeric EQUALITY: S7 (measured, `compat/casts-oracle-transcript.
    // txt`, `float`/`Integer/MAX_VALUE` cell) `(float Integer/MAX_VALUE)`
    // is `2.147483648E9` on real Clojure -- 2^31, the nearest value an
    // actual 24-bit-mantissa float can represent -- NOT `2.147483647E9`
    // (the exact input, unrounded). `test-expected-casts` compares by `=`,
    // which is exact, so skipping the f32 round-trip was a genuine value
    // bug, not just a cosmetic one: it silently kept the unrounded f64
    // wherever a real `float` cast would have lost precision. `(f as f32)
    // as f64` is that round-trip -- Rust's `as f32` on an in-range f64
    // rounds to nearest exactly like the JVM's narrowing conversion (JLS
    // 5.1.3), and on an OUT-of-f32-range finite `f` saturates to +-infinity,
    // which is what makes the overflow check below able to reuse the same
    // cast rather than needing a separate `f32::MAX` constant.
    reg(i, "float", ArityHint::Exact(1), |_i, args| {
        let f = widen_to_f64(&args[0], "float")?;
        if f.is_finite() && (f as f32).is_infinite() {
            return Err(RjError::type_err(format!(
                "Value out of range for float: {}",
                crate::printer::display_str(&args[0])
            )));
        }
        Ok(Value::Float(f as f32 as f64))
    });

    reg(i, "char", ArityHint::Exact(1), |_i, args| cast_char(&args[0]));

    reg(i, "codepoint-str", ArityHint::Exact(1), |_i, args| codepoint_str(&args[0]));

    // The `unchecked-<type>` casts: `byte`/`short`/`int`/`long`/`char`'s
    // range check replaced by a two's-complement wrap (see
    // `cast_unchecked_integral`). `unchecked-float`/`-double` are just the
    // widening cast -- there is no narrower float to wrap to, and mova has
    // one float width anyway.
    reg(i, "unchecked-byte", ArityHint::Exact(1), |_i, args| {
        Ok(Value::Int(cast_unchecked_integral(&args[0], "unchecked-byte", 8)?))
    });
    reg(i, "unchecked-short", ArityHint::Exact(1), |_i, args| {
        Ok(Value::Int(cast_unchecked_integral(&args[0], "unchecked-short", 16)?))
    });
    reg(i, "unchecked-int", ArityHint::Exact(1), |_i, args| {
        Ok(Value::Int(cast_unchecked_integral(&args[0], "unchecked-int", 32)?))
    });
    reg(i, "unchecked-long", ArityHint::Exact(1), |_i, args| {
        Ok(Value::Int(cast_unchecked_integral(&args[0], "unchecked-long", 64)?))
    });
    // Java's `char` is UNSIGNED 16-bit, so this wraps into `0..=0xFFFF`
    // rather than sign-extending like its siblings (measured:
    // `(unchecked-char 65536)` is the NUL char, not `￿`). The
    // lone-surrogate range is still unrepresentable in mova -- see
    // `cast_char`'s KNOWN DIVERGENCE note -- and lands on the same error.
    //
    // `bits` is 32, not 64: per JLS 5.1.3 a float/double source's
    // intermediate width for a `char` target is the same 32-bit `int` as
    // `byte`/`short` (see `cast_unchecked_integral`'s S7 doc) -- `long` is
    // the only target that widens the intermediate to 64 bits. For every
    // non-`Float` source this is a no-op either way: the low 16 bits taken
    // by the `as u16` below are identical whether `cast_unchecked_integral`
    // wrapped at 32 or 64 bits first (a wider wrap only touches bits ABOVE
    // the ones kept), so this change is float-only, matching
    // `compat/casts-oracle-transcript.txt`'s `unchecked-char` rows exactly
    // either way.
    reg(i, "unchecked-char", ArityHint::Exact(1), |_i, args| {
        let code = cast_unchecked_integral(&args[0], "unchecked-char", 32)? as u16;
        char::from_u32(code as u32).map(Value::Char).ok_or_else(|| {
            RjError::type_err(format!(
                "unchecked-char: {} is a lone surrogate, which mova cannot represent",
                code
            ))
        })
    });
    reg(i, "unchecked-double", ArityHint::Exact(1), |_i, args| {
        Ok(Value::Float(widen_to_f64(&args[0], "unchecked-double")?))
    });
    // Same f32 round-trip as `float` above (measured: `(unchecked-float
    // Integer/MAX_VALUE)` is `2.147483648E9`, not the unrounded input) --
    // but with the overflow check DROPPED instead of erroring, since
    // `unchecked-*` never range-checks: `(f as f32)` on a finite `f` too
    // big for f32 (e.g. `Double/MAX_VALUE`) itself saturates to +-infinity,
    // exactly matching real Clojure's `unchecked-float` (measured:
    // `(unchecked-float Double/MAX_VALUE)` => `Float/POSITIVE_INFINITY`,
    // no exception).
    reg(i, "unchecked-float", ArityHint::Exact(1), |_i, args| {
        Ok(Value::Float(widen_to_f64(&args[0], "unchecked-float")? as f32 as f64))
    });

    // `num`: identity on any numeric `Value` (measured: `(num 5)` => `5`,
    // `(num 1.5)` => `1.5`, unchanged including `BigInt`/`Ratio`/`BigDec`);
    // a type error on everything else, INCLUDING `Char` (measured `(num
    // \a)` throws `ClassCastException` -- unlike the integral casts, `num`
    // does NOT treat a char as a numeric codepoint).
    reg(i, "num", ArityHint::Exact(1), |_i, args| match &args[0] {
        v @ (Value::Int(_)
        | Value::Float(_)
        | Value::BigInt(_)
        | Value::BigInteger(_)
        | Value::Ratio(_)
        | Value::BigDec(_)) => {
            Ok(v.clone())
        }
        other => Err(RjError::type_err(format!("num: expected a number, got {}", other.type_name()))),
    });

    reg(i, "bigint", ArityHint::Exact(1), |_i, args| Ok(Value::BigInt(Arc::new(cast_bigint(&args[0])?))));

    reg(i, "bigdec", ArityHint::Exact(1), |_i, args| Ok(Value::BigDec(Arc::new(cast_bigdec(&args[0])?))));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eval_ok(src: &str) -> Value {
        let mut interp = Interp::new();
        interp
            .eval_str("test", src)
            .unwrap_or_else(|e| panic!("eval error for {src:?}: {}", crate::error::render(&e, "test", src)))
    }

    fn ps(src: &str) -> String {
        crate::printer::pr_str(&eval_ok(src))
    }

    fn eval_err(src: &str) -> String {
        let mut interp = Interp::new();
        match interp.eval_str("test", src) {
            Ok(v) => panic!("expected an error evaluating {src:?}, got {v:?}"),
            Err(e) => e.message,
        }
    }

    // --- byte -----------------------------------------------------------
    // SPEC-C-casts.md measured table, every row a separate assertion so a
    // future regression names exactly which one broke.

    #[test]
    fn byte_in_range_ints_pass_through() {
        assert_eq!(ps("(byte 1)"), "1");
        assert_eq!(ps("(byte -128)"), "-128");
        assert_eq!(ps("(byte 127)"), "127");
    }

    #[test]
    fn byte_out_of_range_ints_throw() {
        eval_err("(byte 128)");
        eval_err("(byte 300)");
    }

    #[test]
    fn byte_floats_truncate_toward_zero_then_range_check() {
        assert_eq!(ps("(byte 1.7)"), "1");
        assert_eq!(ps("(byte -1.7)"), "-1");
    }

    #[test]
    fn byte_char_is_its_codepoint() {
        assert_eq!(ps(r"(byte \a)"), "97");
    }

    #[test]
    fn byte_string_never_castable() {
        eval_err(r#"(byte "1")"#);
    }

    #[test]
    fn byte_bigint_ratio_in_range() {
        assert_eq!(ps("(byte 1N)"), "1");
        // Ratio truncates toward zero: 1/2 -> 0.
        assert_eq!(ps("(byte 1/2)"), "0");
    }

    // --- short ------------------------------------------------------------

    #[test]
    fn short_out_of_range_throws() {
        eval_err("(short 40000)");
    }

    #[test]
    fn short_float_truncates() {
        assert_eq!(ps("(short 1.9)"), "1");
    }

    // --- int ----------------------------------------------------------

    #[test]
    fn int_out_of_range_float_throws_with_java_style_message() {
        // Measured: the message renders the ORIGINAL (pre-truncation)
        // value, Java `Double.toString`-style -- "1.0E10", not "10000000000".
        let msg = eval_err("(int 1e10)");
        assert!(msg.contains("1.0E10"), "message was: {msg:?}");
    }

    #[test]
    fn int_float_truncates_toward_zero() {
        assert_eq!(ps("(int 3.99)"), "3");
        assert_eq!(ps("(int -3.99)"), "-3");
    }

    #[test]
    fn int_char_is_codepoint() {
        assert_eq!(ps(r"(int \A)"), "65");
    }

    #[test]
    fn int_ratio_bigint_bigdec() {
        assert_eq!(ps("(int 1/2)"), "0");
        assert_eq!(ps("(int 5N)"), "5");
        assert_eq!(ps("(int 1.5M)"), "1");
    }

    // --- long ---------------------------------------------------------

    #[test]
    fn long_bigdec_and_char() {
        assert_eq!(ps("(long 1.5M)"), "1");
        assert_eq!(ps(r"(long \A)"), "65");
    }

    #[test]
    fn long_infinite_float_throws() {
        let msg = eval_err("(long ##Inf)");
        assert!(msg.contains("Infinity"), "message was: {msg:?}");
    }

    #[test]
    fn long_nan_float_casts_to_zero_not_a_throw() {
        // !! Measured surprise: NaN does NOT throw, it casts to 0 (JLS
        // narrowing double->long conversion rule).
        assert_eq!(ps("(long ##NaN)"), "0");
    }

    #[test]
    fn long_float_beyond_i64_throws() {
        let msg = eval_err("(long 9.3e18)");
        assert!(msg.contains("9.3E18"), "message was: {msg:?}");
    }

    // --- num ------------------------------------------------------------

    #[test]
    fn num_is_identity_on_numbers() {
        assert_eq!(ps("(num 5)"), "5");
        assert_eq!(ps("(num 1.5)"), "1.5");
    }

    #[test]
    fn num_rejects_char() {
        // Unlike the integral casts, `num` does NOT accept a char.
        eval_err(r"(num \a)");
    }

    // --- bigint -----------------------------------------------------------

    #[test]
    fn bigint_from_int_float_ratio() {
        assert_eq!(ps("(bigint 5)"), "5N");
        assert_eq!(ps("(bigint 1.7)"), "1N");
        assert_eq!(ps("(bigint 1/2)"), "0N");
    }

    #[test]
    fn bigint_from_string() {
        // Unlike byte/short/int/long, `bigint` DOES accept strings.
        assert_eq!(ps(r#"(bigint "123")"#), "123N");
    }

    #[test]
    fn bigint_from_bigdec_truncates() {
        assert_eq!(ps("(bigint 1.5M)"), "1N");
    }

    // --- bigdec -----------------------------------------------------------

    #[test]
    fn bigdec_from_int_float_ratio() {
        assert_eq!(ps("(bigdec 5)"), "5M");
        assert_eq!(ps("(bigdec 1.5)"), "1.5M");
        assert_eq!(ps("(bigdec 1/2)"), "0.5M");
    }

    #[test]
    fn bigdec_non_terminating_ratio_throws() {
        let msg = eval_err("(bigdec 1/3)");
        assert!(msg.contains("Non-terminating decimal expansion"), "message was: {msg:?}");
    }

    #[test]
    fn bigdec_from_string_preserves_scale() {
        // Measured: scale is preserved from the string's own digits --
        // "1.50" stays 2 decimal places, not canonicalized to "1.5".
        assert_eq!(ps(r#"(bigdec "1.50")"#), "1.50M");
    }

    #[test]
    fn bigdec_from_bigint() {
        assert_eq!(ps("(bigdec 5N)"), "5M");
    }

    // --- float / double -------------------------------------------------

    #[test]
    fn float_overflow_throws_with_java_style_message() {
        let msg = eval_err("(float 1e300)");
        assert!(msg.contains("1.0E300"), "message was: {msg:?}");
    }

    #[test]
    fn double_accepts_ratio_bigint_bigdec() {
        assert_eq!(ps("(double 1/2)"), "0.5");
        assert_eq!(ps("(double 5N)"), "5.0");
        assert_eq!(ps("(double 1.5M)"), "1.5");
    }

    // --- char -----------------------------------------------------------

    #[test]
    fn char_float_truncates() {
        assert_eq!(ps("(char 65.5)"), r"\A");
    }

    #[test]
    fn char_negative_throws() {
        let msg = eval_err("(char -1)");
        assert!(msg.contains("-1"), "message was: {msg:?}");
    }

    #[test]
    fn char_above_0xffff_throws() {
        let msg = eval_err("(char 1114112)");
        assert!(msg.contains("1114112"), "message was: {msg:?}");
    }

    // --- codepoint-str (wishlist #9) --------------------------------------

    #[test]
    fn codepoint_str_bmp_char() {
        assert_eq!(ps("(codepoint-str 65)"), "\"A\"");
    }

    #[test]
    fn codepoint_str_char_arg_passthrough() {
        assert_eq!(ps(r"(codepoint-str \A)"), "\"A\"");
    }

    #[test]
    fn codepoint_str_astral_emoji() {
        // 0x1F600 is 😀 -- outside the BMP, above cast_char's 0xFFFF
        // ceiling. Must round-trip as exactly one Unicode scalar value /
        // one Rust char, with the correct UTF-8 bytes.
        let v = eval_ok("(codepoint-str 0x1F600)");
        match &v {
            Value::Str(s) => {
                let text: &str = s;
                assert_eq!(text.chars().count(), 1);
                assert_eq!(text.chars().next().unwrap(), '\u{1F600}');
                assert_eq!(text.as_bytes(), "\u{1F600}".as_bytes());
            }
            other => panic!("expected Str, got {other:?}"),
        }
    }

    #[test]
    fn codepoint_str_max_valid_codepoint() {
        let v = eval_ok("(codepoint-str 0x10FFFF)");
        match &v {
            Value::Str(s) => {
                let text: &str = s;
                assert_eq!(text.chars().next().unwrap(), '\u{10FFFF}');
            }
            other => panic!("expected Str, got {other:?}"),
        }
    }

    #[test]
    fn codepoint_str_above_max_throws() {
        let msg = eval_err("(codepoint-str 0x110000)");
        assert!(msg.contains("1114112"), "message was: {msg:?}");
    }

    #[test]
    fn codepoint_str_lone_surrogate_throws() {
        let msg = eval_err("(codepoint-str 0xD800)");
        assert!(msg.contains("55296") || msg.contains("surrogate"), "message was: {msg:?}");
    }

    #[test]
    fn codepoint_str_negative_throws() {
        let msg = eval_err("(codepoint-str -1)");
        assert!(msg.contains("-1"), "message was: {msg:?}");
    }

    // --- Gates section sanity check (SPEC-C-casts.md) --------------------

    #[test]
    fn gate_sanity_vector() {
        assert_eq!(ps(r"[(byte 1/2) (long ##NaN) (int \A)]"), "[0 0 65]");
    }
}
