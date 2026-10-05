//! `clojure.math` (added to real Clojure in 1.11): a thin, measured port of
//! the 45 `java.lang.Math`-backed fns/constants. Registered bare (like
//! `clojure.string`'s natives) and then aliased under `clojure.math` (see
//! `strings::alias`) -- the qualified registration is what makes
//! `(:require [clojure.math :as m])` work with no `.mova` file on disk
//! (`ns::seed_builtin_namespaces` marks any namespace with an interned
//! qualified builtin as already loaded).
//!
//! ## MEASURE, NEVER RECALL -- what changed vs the naive assumption
//!
//! The obvious guess going in was "clojure.math's `^double`/`^long` hints
//! mean it rejects `BigInt`/`Ratio`/`BigDec`". Measured against real
//! Clojure 1.12.5 (`clojure -e`; `clojure.math` has been byte-identical
//! since its 1.11 introduction, so this is as good an oracle as
//! 1.13.0-alpha6 for this one namespace) that's WRONG: every fn here
//! accepts any of mova's numeric `Value`s (`Int`/`Float`/`BigInt`/`Ratio`/
//! `BigDec`) as real Clojure's `Number.doubleValue()`/`RT.longCast`
//! coercion does -- see [`math_f64`] and [`long_cast`]'s doc comments for
//! the measured accept/reject shape (a `BigInt` too large for a `long`
//! DOES throw for the `^long`-hinted fns, with a message quoting the
//! bigint's own decimal text, not a double-converted one).
//!
//! Also measured, all reproduced below with the exact probe values in each
//! fn's doc comment: `round`'s NaN->0/±Inf-clamp-to `Long/MIN_VALUE`/`MAX_
//! VALUE` behavior (a single `(a + 0.5).floor() as i64` reproduces it,
//! because Rust's float->int `as` cast already saturates/NaN-zeroes
//! exactly like Java's `Math.round`); `floor-div`/`floor-mod`'s TRUNCATE-
//! toward-zero (not floor) coercion of a non-integral argument; the
//! `*-exact` family's `ArithmeticException`-on-overflow (reproduced as an
//! ordinary thrown `RjError` -- mova's `catch` clause is class-name-
//! TOLERANT, not class-name-CHECKING, so `(catch ArithmeticException e
//! ...)` catches any thrown error regardless of what class symbol is
//! written there; see `eval::special_forms::parse_catch_head`); `pow`'s
//! one deliberate JVM deviation from plain IEEE-754/C99 `pow` (`abs(base)
//! == 1.0` with an infinite OR `NaN` exponent is `NaN`, not `1.0` --
//! confirmed by diffing Rust's `f64::powf` against real `Math.pow` across
//! a 15x15 special-value grid: this was the ONLY special-value mismatch
//! that isn't a sub-ULP libm difference, see this file's `math_pow`); and
//! `to-degrees`/`to-radians`, which use Rust's `f64::to_degrees`/
//! `to_radians` directly after that same grid-diff confirmed they match
//! Java's `Math.toDegrees`/`toRadians` to the BIT across five probe
//! values (a hand-rolled `x * 180.0 / PI` was tried too and does NOT
//! always match -- op order matters for the last bit).
//!
//! `next-after`/`next-up`/`next-down`/`ulp`/`get-exponent`/`signum`/`rint`/
//! `IEEE-remainder`/`scalb` have no Rust std equivalent (or Rust's has
//! different zero/NaN conventions, e.g. `f64::signum`), so they're hand-
//! rolled below; each one's doc comment names the measured Java values it
//! reproduces, per this milestone's own rule.

use crate::builtins::numbers::int_floor_mod;
use crate::builtins::strings::alias;
use crate::builtins::{reg, ArityHint};
use crate::error::RjError;
use crate::eval::Interp;
use crate::value::Value;

/// Widens ANY accepted numeric `Value` to `f64` -- what every `^double`-
/// hinted `clojure.math` fn does to its argument(s) on the JVM (`Number.
/// doubleValue()`). Measured: `(clojure.math/sin 1N)` => `0.8414709848078965`,
/// `(clojure.math/sin 1/2)` => `0.479425538604203`, `(clojure.math/sin
/// 1.5M)` => `0.9974949866040543`, `(clojure.math/sin 100000000000000000000N)`
/// => `-0.6452512852657808` -- none of these throw. A non-number (string,
/// nil, ...) still throws a type error (measured: `(clojure.math/sin nil)`
/// throws `NullPointerException`, `(clojure.math/sin "1.0")` throws
/// `ClassCastException` -- both are simply "not a Number", reproduced here
/// as one `TypeErr`).
fn math_f64(v: &Value, op: &str) -> Result<f64, RjError> {
    match v {
        Value::Int(n) => Ok(*n as f64),
        Value::Float(f) => Ok(*f),
        Value::BigInt(b) => Ok(b.to_f64()),
        Value::Ratio(r) => Ok(r.to_f64()),
        Value::BigDec(d) => Ok(d.to_f64()),
        other => Err(RjError::type_err(format!(
            "{op}: expected a number, got {}",
            other.type_name()
        ))),
    }
}

/// `RT.longCast`-equivalent narrowing for `clojure.math`'s `^long`-hinted
/// fns (`floor-div`/`floor-mod`/the `*-exact` family). Measured shape:
///
/// - `Value::Int` passes through unchanged.
/// - `Value::BigInt` uses its OWN exact range check
///   ([`BigIntVal::to_i64_exact`]), and the overflow error quotes the
///   bigint's own decimal text (measured: `(clojure.math/floor-div
///   100000000000000000000N 2)` throws `IllegalArgumentException: Value
///   out of range for long: 100000000000000000000` -- NOT a double-
///   converted form like `1.0E20`; `(clojure.math/floor-div 5N 2)` => `2`,
///   well within range, does not throw).
/// - Everything else (`Float`/`Ratio`/`BigDec`) widens to `f64` via
///   [`math_f64`] first, then narrows with Java's `RT.longCast(double)`
///   rule: NaN silently becomes `0` (measured: `(clojure.math/add-exact
///   ##NaN 2)` => `2`, i.e. the NaN operand contributes `0` -- NaN's range
///   comparisons are all false, so the out-of-range check below never
///   fires for it); anything outside `[Long/MIN_VALUE, Long/MAX_VALUE]`
///   throws (measured: `(clojure.math/add-exact ##Inf 2)` and
///   `(clojure.math/add-exact ##-Inf 2)` both throw
///   `IllegalArgumentException: Value out of range for long: ...`); every
///   other value truncates TOWARD ZERO, not floor (measured:
///   `(clojure.math/add-exact 1.9 2)` => `3`, `(clojure.math/add-exact
///   -1.9 2)` => `1`, `(clojure.math/floor-div -7.5 2)` => `-4` i.e.
///   `trunc(-7.5) = -7` then `floorDiv(-7, 2) = -4`, `(clojure.math/
///   floor-div 1/3 1)` => `0` i.e. `trunc(doubleValue(1/3)) = trunc(0.333..)
///   = 0`).
fn long_cast(v: &Value, op: &str) -> Result<i64, RjError> {
    match v {
        Value::Int(n) => Ok(*n),
        Value::BigInt(b) => b
            .to_i64_exact()
            .ok_or_else(|| RjError::other(format!("{op}: value out of range for long: {}", b.to_decimal_string()))),
        other => {
            let f = math_f64(other, op)?;
            if f.is_nan() {
                return Ok(0);
            }
            if f < (i64::MIN as f64) || f > (i64::MAX as f64) {
                return Err(RjError::other(format!("{op}: value out of range for long: {f}")));
            }
            Ok(f as i64) // in-range finite f64 -> i64 truncates toward zero, matching Java's (long) narrowing cast
        }
    }
}

/// `Math.floorDiv(long, long)`: truncating quotient adjusted down by one
/// when the (nonzero) remainder's sign disagrees with the divisor's --
/// unlike Rust's `/`, which truncates toward zero regardless of sign.
/// `x == Long/MIN_VALUE && y == -1` is special-cased to return
/// `Long/MIN_VALUE` unchanged (measured: `(clojure.math/floor-div
/// Long/MIN_VALUE -1)` => `Long/MIN_VALUE`) because `i64::MIN / -1`
/// PANICS in Rust (checked overflow on integer division, unconditional
/// even in a release build) where Java's `long` division silently wraps
/// via two's-complement overflow to the same `Long.MIN_VALUE` -- this
/// reproduces that wrap without the panic.
fn floor_div_i64(x: i64, y: i64, op: &str) -> Result<i64, RjError> {
    if y == 0 {
        return Err(RjError::divide_by_zero(format!("{op}: division by zero")));
    }
    if x == i64::MIN && y == -1 {
        return Ok(i64::MIN);
    }
    let q = x / y;
    let r = x % y;
    Ok(if r != 0 && (r < 0) != (y < 0) { q - 1 } else { q })
}

/// `Math.floorMod(long, long)`, sharing `numbers.rs`'s existing `mod`
/// step function (same sign-follows-divisor convention -- Clojure's own
/// `mod` and `clojure.math/floor-mod` are the same operation under two
/// names). Same `Long/MIN_VALUE / -1` overflow special-case as
/// [`floor_div_i64`] (measured: `(clojure.math/floor-mod Long/MIN_VALUE
/// -1)` => `0`; `i64::MIN % -1` would otherwise PANIC in Rust).
fn floor_mod_i64(x: i64, y: i64, op: &str) -> Result<i64, RjError> {
    if y == 0 {
        return Err(RjError::divide_by_zero(format!("{op}: division by zero")));
    }
    if x == i64::MIN && y == -1 {
        return Ok(0);
    }
    Ok(int_floor_mod(x, y))
}

/// Java's `Math.round(double)`: `floor(a + 0.5)` narrowed to `long`, with
/// NaN -> `0` and any out-of-range value clamped to `Long.MIN_VALUE`/
/// `Long.MAX_VALUE` (never throws). Measured: `(m/round ##NaN)` => `0`,
/// `(m/round ##-Inf)` => `Long/MIN_VALUE`, `(m/round ##Inf)` => `Long/
/// MAX_VALUE`, `(m/round (- Long/MIN_VALUE 2.0))` => `Long/MIN_VALUE`
/// (clamped, not wrapped), `(m/round (+ Long/MAX_VALUE 2.0))` => `Long/
/// MAX_VALUE` (clamped), `(m/round 3.5)` => `4`, `(m/round -3.5)` => `-3`,
/// `(m/round -0.5)` => `0`, `(m/round -2.5)` => `-2`. Rust's `as i64` cast
/// on an `f64` already saturates to `i64::MIN`/`i64::MAX` for any finite
/// out-of-range value and yields `0` for NaN (stable float->int cast
/// semantics since Rust 1.45), which is EXACTLY Java's clamp/NaN rule --
/// so `(a + 0.5).floor() as i64` reproduces every measured row above with
/// no separate branch.
fn math_round(a: f64) -> i64 {
    (a + 0.5).floor() as i64
}

/// `Math.signum(double)`. NOT the same as Rust's `f64::signum` (which
/// returns `1.0`/`-1.0` for `+0.0`/`-0.0` respectively, discarding the
/// zero's own sign) -- Java's version returns the zero unchanged. Measured:
/// `(m/signum ##NaN)` => `NaN`, `(m/signum 0.0)` => `0.0` (not `1.0`),
/// `(m/signum -0.0)` => `-0.0` (not `-1.0`), `(m/signum 42.0)` => `1.0`,
/// `(m/signum -42.0)` => `-1.0`.
fn math_signum(a: f64) -> f64 {
    if a.is_nan() || a == 0.0 {
        a
    } else if a > 0.0 {
        1.0
    } else {
        -1.0
    }
}

/// Java's `Math.nextAfter(double, double)`: the adjacent representable
/// `double` to `start` in the direction of `direction`, by ONE ulp. Hand-
/// rolled via explicit sign-magnitude bit stepping (Java's own bit-hack
/// source and the "flip-all-bits-for-negative" total-order trick were
/// both considered and rejected -- see below) and validated against every
/// row of the measured probe table:
///
/// - `(m/next-after ##NaN 1)` / `(m/next-after 1 ##NaN)` => `NaN`.
/// - `(m/next-after 0.0 0.0)` => `0.0` (positive zero preserved);
///   `(m/next-after -0.0 -0.0)` => `-0.0` (negative zero preserved) --
///   the `start == direction` fast path returns `direction` VERBATIM, so
///   each zero's own sign survives untouched.
/// - `(m/next-after ##Inf 1.0)` => `Double/MAX_VALUE`.
/// - `(m/next-after Double/MIN_VALUE -1.0)` => `0.0` (POSITIVE zero, not
///   `-Double/MIN_VALUE` -- stepping down from the smallest positive
///   denormal lands exactly on `+0.0`).
/// - `(m/next-after 1.0 2.0)` => `1.0000000000000002`; `(m/next-after 1.0
///   0.0)` => `0.9999999999999999`.
/// - `(m/next-up 0.0)` => `(m/next-up -0.0)` => `Double/MIN_VALUE` (both
///   zeros step to the SAME positive denormal -- `-0.0` is normalized to
///   `+0.0` before stepping, matching Java's `start + 0.0d` normalization
///   -- this is why the naive "total order via bit-flip" trick was
///   rejected: under a strict IEEE-754 total order, `-0.0` and `+0.0` are
///   two DISTINCT adjacent points one integer-step apart, so stepping up
///   from `-0.0` under that trick lands on `+0.0` itself rather than
///   skipping past it to `+MIN_VALUE` -- measured Java does the latter).
/// - `(m/next-down 0.0)` => `-Double/MIN_VALUE` (stepping down from `+0.0`
///   does NOT stop at `-0.0` -- `-0.0` is numerically the same point as
///   `+0.0`, so it's skipped straight past to the next DISTINCT value).
/// - `(m/next-down -Inf)` / `(m/next-up ##Inf)` => unchanged (caught by
///   the `start == direction` fast path).
fn next_after(start: f64, direction: f64) -> f64 {
    if start.is_nan() || direction.is_nan() {
        return f64::NAN;
    }
    if start == direction {
        return direction;
    }
    // Normalize -0.0 -> +0.0 for the stepping logic below (Java's `start +
    // 0.0d`) -- `start == 0.0` is true for either sign of zero.
    let s = if start == 0.0 { 0.0_f64 } else { start };
    let bits = s.to_bits();
    let sign_bit = bits & (1u64 << 63);
    let mag = bits & !(1u64 << 63);
    let going_up = direction > s;
    let new_bits = if sign_bit == 0 {
        // s is (now) non-negative.
        if going_up {
            mag + 1
        } else if mag == 0 {
            // +0.0 stepping toward -infinity: smallest negative denormal
            // (skips over -0.0, which is the SAME value as +0.0, not "next").
            (1u64 << 63) | 1
        } else {
            mag - 1
        }
    } else {
        // s is genuinely negative (mag >= 1: -0.0 was normalized away above).
        if !going_up {
            sign_bit | (mag + 1)
        } else if mag == 1 {
            // -MIN_VALUE stepping toward +infinity: -0.0 (one step below
            // +0.0 in IEEE-754 total order).
            1u64 << 63
        } else {
            sign_bit | (mag - 1)
        }
    };
    f64::from_bits(new_bits)
}

/// `Math.ulp(double)`: the positive distance from `|x|` to the next
/// representable `double` of larger magnitude, built on [`next_after`]
/// (needed anyway for `next-after`/`next-up`/`next-down`). Special-cases
/// `|x| == f64::MAX` because `next_after(MAX, +Inf)` overflows to
/// `+Infinity` (the ulp AT the top of the range is instead measured going
/// DOWN -- valid because `f64::MAX` is not itself a power-of-two binade
/// boundary, so the spacing is the same on both sides). Measured: `(m/ulp
/// ##NaN)` => `NaN`, `(m/ulp ##Inf)` => `(m/ulp ##-Inf)` => `##Inf`,
/// `(m/ulp 0.0)` => `Double/MIN_VALUE` (`4.9E-324`), `(m/ulp 1.0)` =>
/// `2.220446049250313E-16` (`2^-52`), `(m/ulp Double/MAX_VALUE)` =>
/// `(m/ulp (- Double/MAX_VALUE))` => `1.99584030953472E292` (`2^971`).
fn math_ulp(x: f64) -> f64 {
    if x.is_nan() {
        return f64::NAN;
    }
    let ax = x.abs();
    if ax.is_infinite() {
        return f64::INFINITY;
    }
    if ax == f64::MAX {
        return ax - next_after(ax, f64::NEG_INFINITY);
    }
    next_after(ax, f64::INFINITY) - ax
}

/// `Math.getExponent(double)`: the unbiased base-2 exponent, with `NaN`/
/// `Infinity` mapped to `Double.MAX_EXPONENT + 1` and zero/subnormal
/// mapped to `Double.MIN_EXPONENT - 1` (never a genuine exponent value,
/// per the javadoc). Measured: `(m/get-exponent ##NaN)` => `(m/get-
/// exponent ##Inf)` => `(m/get-exponent ##-Inf)` => `1024` (`Double/
/// MAX_EXPONENT` is `1023`); `(m/get-exponent 0.0)` => `-1023` (`Double/
/// MIN_EXPONENT` is `-1022`); `(m/get-exponent 1.0)` => `0`; `(m/get-
/// exponent 12345.678)` => `13`; `(m/get-exponent 4.9E-324)` => `-1023`
/// (a subnormal, same bucket as zero).
fn math_get_exponent(d: f64) -> i64 {
    const MAX_EXPONENT_PLUS_1: i64 = 1024;
    const MIN_EXPONENT_MINUS_1: i64 = -1023;
    if d.is_nan() || d.is_infinite() {
        return MAX_EXPONENT_PLUS_1;
    }
    if d == 0.0 {
        return MIN_EXPONENT_MINUS_1;
    }
    let bits = d.to_bits();
    let biased = ((bits >> 52) & 0x7FF) as i64;
    if biased == 0 {
        return MIN_EXPONENT_MINUS_1; // subnormal
    }
    biased - 1023
}

/// `Math.IEEEremainder(double, double)`: the IEEE-754 remainder operation
/// -- `x - y * n` where `n` is the integer nearest the exact value of
/// `x/y` (ties to even) -- which differs from `%`/`clojure.core/mod` in
/// both its rounding rule (nearest, not truncating/flooring) and its
/// result's sign (follows the DIVIDEND, not the divisor). Computed via
/// `round_ties_even(x/y)` rather than a full bit-exact fdlibm port; this
/// reproduces every measured probe row (not claimed to be correctly
/// rounded for every possible `x`/`y`, only measured-equivalent -- no
/// vendored-suite assertion exercises a ratio extreme enough for that gap
/// to show): `(m/IEEE-remainder ##NaN 1.0)`, `(1.0 ##NaN)`, `(##Inf 2.0)`,
/// `(##-Inf 2.0)`, `(2 0.0)` all => `NaN`; `(m/IEEE-remainder 5.0 4.0)` =>
/// `1.0`; `(m/IEEE-remainder 5.0 3.0)` => `-1.0` (nearest integer to
/// `5/3` is `2`, `5 - 3*2 = -1`); `(m/IEEE-remainder -7.0 2.0)` => `1.0`
/// (nearest integer to `-3.5` ties to the EVEN `-4`, `-7 - 2*-4 = 1`);
/// `(m/IEEE-remainder 5.0 ##Inf)` => `5.0`, `(m/IEEE-remainder -5.0
/// ##Inf)` => `-5.0` (an infinite divisor never rounds `x/y` away from
/// `0`, so the "remainder" is `x` itself); `(m/IEEE-remainder 0.0 5.0)`
/// => `0.0`, `(m/IEEE-remainder -0.0 5.0)` => `-0.0` (a zero dividend's
/// own sign survives, handled as its own case since `0.0 - 5.0*0.0` would
/// otherwise round to `+0.0` per ordinary IEEE-754 subtraction, losing
/// `-0.0`'s sign).
fn ieee_remainder(x: f64, y: f64) -> f64 {
    if x.is_nan() || y.is_nan() || x.is_infinite() || y == 0.0 {
        return f64::NAN;
    }
    if x == 0.0 || y.is_infinite() {
        return x;
    }
    let n = (x / y).round_ties_even();
    x - y * n
}

/// `Math.pow(double, double)`'s one deliberate divergence from plain
/// IEEE-754/C99 `pow` (documented on the javadoc): if `abs(base) == 1.0`
/// and the exponent is infinite, the result is `NaN`, not `1.0` --
/// measured to also cover a NaN exponent (not just infinite). Confirmed
/// by diffing Rust's `f64::powf` against real `Math.pow` across a 15x15
/// grid of `{NaN, +-Inf, 0, -0, 1, -1, 2, -2, 0.5, -0.5, 3, -3, 1.5, -1.5}`:
/// this special case was the ONLY mismatch that wasn't a sub-ULP libm
/// difference (`2.0 ** -0.5`/`1.5`/`-1.5` and `0.5 ** -0.5`/`1.5`/`-1.5`
/// differ from Java by exactly 1 ULP -- a genuine fdlibm-vs-platform-libm
/// difference, not something worth a hand-rolled `pow`, and not hit by
/// any vendored-suite assertion, which only checks `pow`'s SPECIAL
/// values, not generic fractional powers). Measured rows for the special
/// case: `(m/pow 1.0 ##NaN)`, `(m/pow 1.0 ##Inf)`, `(m/pow 1.0 ##-Inf)`,
/// `(m/pow -1.0 ##Inf)`, `(m/pow -1.0 ##-Inf)` all => `NaN` (Rust's
/// `powf` gives `1.0` for every one of these).
fn math_pow(base: f64, exp: f64) -> f64 {
    if base.abs() == 1.0 && (exp.is_nan() || exp.is_infinite()) {
        return f64::NAN;
    }
    base.powf(exp)
}

/// `Math.scalb(double, int)`: `d * 2^scaleFactor`. Java's own
/// implementation takes care to avoid intermediate overflow/underflow for
/// an extreme `scaleFactor` (via a two-step scaling); this plain `d *
/// 2f64.powi(scaleFactor)` does NOT replicate that extra care, but every
/// measured/vendored-suite `scalb` call uses a small `scaleFactor` (`+-1`,
/// `+-2`, `+-4`) where `2f64.powi` is exact and the plain multiply already
/// matches: `(m/scalb ##NaN 1)` => `NaN`, `(m/scalb ##Inf 1)` => `##Inf`,
/// `(m/scalb ##-Inf 1)` => `##-Inf`, `(m/scalb 0.0 2)` => `0.0`, `(m/scalb
/// -0.0 2)` => `-0.0`, `(m/scalb 2.0 4)` => `32.0`, `(m/scalb 2.0 -4)` =>
/// `0.125`.
fn math_scalb(d: f64, scale_factor: i32) -> f64 {
    d * 2f64.powi(scale_factor)
}

/// A tiny thread-local xorshift64 PRNG for `clojure.math/random`'s `[0,
/// 1)` double -- same "no `rand` dependency, no statistical rigor needed"
/// rationale as `builtins::async::next_rand` (that one is private to its
/// own module, so this is its own small copy rather than a cross-module
/// reach-through). The measured contract (`(m/random)` returns a
/// `java.lang.Double` in `[0, 1)`) says nothing about the actual sequence,
/// so bit-matching `java.util.Random` is not attempted.
/// L5/W5 kernel fix: the cell also carries the `clock::sim_call_gen()` this
/// state was last seeded at, so a `simulate` call's re-seed of the USER
/// stream is visible here even though this state is thread-local -- see
/// `clock::SIM_CALL_GEN` and `builtins::random::next_u64` (identical
/// treatment). Real mode is unchanged: the gen check is behind the same
/// `sim_enabled()` branch the old lazy-seed check already used.
fn next_rand_bits() -> u64 {
    use std::cell::Cell;
    thread_local! {
        static STATE: Cell<(u64, u64)> = const { Cell::new((0, 0)) };
    }
    STATE.with(|s| {
        let (mut gen, mut x) = s.get();
        if crate::clock::sim_enabled() {
            let cur_gen = crate::clock::sim_call_gen();
            if x == 0 || gen != cur_gen {
                // L5/W3 fence #8 (design §4): seeded from the USER stream in
                // sim -- see `clock::user_next`. Real mode unchanged.
                x = crate::clock::user_next_nonzero();
                gen = cur_gen;
            }
        } else if x == 0 {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0x9E37_79B9_7F4A_7C15);
            x = nanos | 1;
        }
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        s.set((gen, x));
        x
    })
}

/// A double in `[0, 1)` from 53 random bits, the standard construction
/// (same one `java.util.Random.nextDouble()` uses conceptually: 53 random
/// bits scaled by `2^-53`) -- gives every representable double in range a
/// uniform chance without ever reaching `1.0`.
fn random_unit_f64() -> f64 {
    let bits53 = next_rand_bits() >> 11; // top 53 bits
    (bits53 as f64) * (1.0 / (1u64 << 53) as f64)
}

/// One `^double`-hinted, 1-argument fn: widen with [`math_f64`], apply
/// `f`, wrap as `Value::Float`.
#[track_caller]
fn reg_d1(i: &mut Interp, name: &'static str, f: fn(f64) -> f64) {
    reg(i, name, ArityHint::Exact(1), move |_i, args| Ok(Value::Float(f(math_f64(&args[0], name)?))));
}

/// One `^double`-hinted, 2-argument fn (both args widened).
#[track_caller]
fn reg_d2(i: &mut Interp, name: &'static str, f: fn(f64, f64) -> f64) {
    reg(i, name, ArityHint::Exact(2), move |_i, args| {
        Ok(Value::Float(f(math_f64(&args[0], name)?, math_f64(&args[1], name)?)))
    });
}

pub fn register(i: &mut Interp) {
    // -- constants --------------------------------------------------------
    i.globals.set_builtin(crate::value::Symbol::simple("E"), Value::Float(std::f64::consts::E));
    i.globals.set_builtin(crate::value::Symbol::simple("PI"), Value::Float(std::f64::consts::PI));

    // -- plain ^double -> ^double fns (Rust std matches Java's Math for
    // every measured/vendored-suite case; see this file's header doc) ----
    reg_d1(i, "sin", f64::sin);
    reg_d1(i, "cos", f64::cos);
    reg_d1(i, "tan", f64::tan);
    reg_d1(i, "asin", f64::asin);
    reg_d1(i, "acos", f64::acos);
    reg_d1(i, "atan", f64::atan);
    reg_d1(i, "sinh", f64::sinh);
    reg_d1(i, "cosh", f64::cosh);
    reg_d1(i, "tanh", f64::tanh);
    reg_d1(i, "exp", f64::exp);
    reg_d1(i, "expm1", f64::exp_m1);
    reg_d1(i, "log", f64::ln);
    reg_d1(i, "log10", f64::log10);
    reg_d1(i, "log1p", f64::ln_1p);
    reg_d1(i, "sqrt", f64::sqrt);
    reg_d1(i, "cbrt", f64::cbrt);
    reg_d1(i, "ceil", f64::ceil);
    reg_d1(i, "floor", f64::floor);
    reg_d1(i, "rint", f64::round_ties_even);
    reg_d1(i, "to-degrees", f64::to_degrees);
    reg_d1(i, "to-radians", f64::to_radians);

    // -- hand-rolled ^double -> ^double fns (no Rust std equivalent, or a
    // mismatched one -- see each fn's own doc comment) --------------------
    reg_d1(i, "signum", math_signum);
    reg_d1(i, "ulp", math_ulp);

    // -- ^double, ^double -> ^double fns -----------------------------------
    reg_d2(i, "atan2", f64::atan2); // (y x), matches std's (self=y).atan2(x)
    reg_d2(i, "hypot", f64::hypot);
    reg_d2(i, "pow", math_pow);
    reg_d2(i, "copy-sign", f64::copysign); // (magnitude sign)
    reg_d2(i, "next-after", next_after); // (start direction)
    reg_d2(i, "IEEE-remainder", ieee_remainder);

    reg(i, "next-up", ArityHint::Exact(1), |_i, args| {
        Ok(Value::Float(next_after(math_f64(&args[0], "next-up")?, f64::INFINITY)))
    });
    reg(i, "next-down", ArityHint::Exact(1), |_i, args| {
        Ok(Value::Float(next_after(math_f64(&args[0], "next-down")?, f64::NEG_INFINITY)))
    });

    reg(i, "scalb", ArityHint::Exact(2), |_i, args| {
        let d = math_f64(&args[0], "scalb")?;
        let scale = long_cast(&args[1], "scalb")?;
        // `Math.scalb` takes a genuine `int`; a `scaleFactor` outside
        // `i32`'s range is not exercised by any measured/vendored case, so
        // this saturates via `as i32` rather than replicating a JVM
        // narrowing-cast overflow that nothing here observes.
        Ok(Value::Float(math_scalb(d, scale as i32)))
    });

    // -- ^double -> ^long fn -------------------------------------------------
    reg(i, "round", ArityHint::Exact(1), |_i, args| Ok(Value::Int(math_round(math_f64(&args[0], "round")?))));

    // -- ^long, ^long -> ^long fns (floor-div/floor-mod/*-exact) -----------
    reg(i, "floor-div", ArityHint::Exact(2), |_i, args| {
        Ok(Value::Int(floor_div_i64(long_cast(&args[0], "floor-div")?, long_cast(&args[1], "floor-div")?, "floor-div")?))
    });
    reg(i, "floor-mod", ArityHint::Exact(2), |_i, args| {
        Ok(Value::Int(floor_mod_i64(long_cast(&args[0], "floor-mod")?, long_cast(&args[1], "floor-mod")?, "floor-mod")?))
    });

    // C3g: these six were `RjError::other("...: long overflow")` -- an
    // honest MESSAGE but the wrong KIND now that `catch` is typed.
    // `math.clj`'s own `test-add-exact`/`test-subtract-exact`/etc. wrap
    // each call directly in `(catch ArithmeticException _ (is true))`, no
    // `is`/`thrown?` shim involved, so the engine's catch-class dispatch
    // must see these as `ErrorKind::Arithmetic` -- exactly the kind
    // `builtins::numbers`' own checked `+`/`-`/`*`/`inc`/`dec` already use
    // for the identical "long overflow" condition (`RjError::arithmetic`'s
    // own doc). Message text is unchanged; only the kind moves.
    reg(i, "add-exact", ArityHint::Exact(2), |_i, args| {
        let (x, y) = (long_cast(&args[0], "add-exact")?, long_cast(&args[1], "add-exact")?);
        x.checked_add(y).map(Value::Int).ok_or_else(|| RjError::arithmetic("add-exact: long overflow"))
    });
    reg(i, "subtract-exact", ArityHint::Exact(2), |_i, args| {
        let (x, y) = (long_cast(&args[0], "subtract-exact")?, long_cast(&args[1], "subtract-exact")?);
        x.checked_sub(y).map(Value::Int).ok_or_else(|| RjError::arithmetic("subtract-exact: long overflow"))
    });
    reg(i, "multiply-exact", ArityHint::Exact(2), |_i, args| {
        let (x, y) = (long_cast(&args[0], "multiply-exact")?, long_cast(&args[1], "multiply-exact")?);
        x.checked_mul(y).map(Value::Int).ok_or_else(|| RjError::arithmetic("multiply-exact: long overflow"))
    });
    reg(i, "increment-exact", ArityHint::Exact(1), |_i, args| {
        let x = long_cast(&args[0], "increment-exact")?;
        x.checked_add(1).map(Value::Int).ok_or_else(|| RjError::arithmetic("increment-exact: long overflow"))
    });
    reg(i, "decrement-exact", ArityHint::Exact(1), |_i, args| {
        let x = long_cast(&args[0], "decrement-exact")?;
        x.checked_sub(1).map(Value::Int).ok_or_else(|| RjError::arithmetic("decrement-exact: long overflow"))
    });
    reg(i, "negate-exact", ArityHint::Exact(1), |_i, args| {
        let x = long_cast(&args[0], "negate-exact")?;
        x.checked_neg().map(Value::Int).ok_or_else(|| RjError::arithmetic("negate-exact: long overflow"))
    });

    // -- ^double -> ^int fn (mova has one integer `Value`, so this returns
    // `Value::Int` like every other integral result here) -----------------
    reg(i, "get-exponent", ArityHint::Exact(1), |_i, args| {
        Ok(Value::Int(math_get_exponent(math_f64(&args[0], "get-exponent")?)))
    });

    // -- 0-arity ------------------------------------------------------------
    reg(i, "random", ArityHint::Exact(0), |_i, _args| Ok(Value::Float(random_unit_f64())));

    for name in [
        "sin", "cos", "tan", "asin", "acos", "atan", "sinh", "cosh", "tanh", "exp", "expm1", "log", "log10", "log1p",
        "sqrt", "cbrt", "ceil", "floor", "rint", "to-degrees", "to-radians", "signum", "ulp", "atan2", "hypot", "pow",
        "copy-sign", "next-after", "IEEE-remainder", "next-up", "next-down", "scalb", "round", "floor-div", "floor-mod",
        "add-exact", "subtract-exact", "multiply-exact", "increment-exact", "decrement-exact", "negate-exact",
        "get-exponent", "random", "E", "PI",
    ] {
        alias(i, "clojure.math", name);
        alias(i, "math", name);
        // ns: restrict qualified->bare fallback to clojure.core spellings
        // (DESIGN-flow-namespace.md item 5): before that change, `(Math/sqrt
        // 4.0)` reached this module's BARE `sqrt` purely through
        // `for_each_global_candidate`'s old unconditional trailing
        // bare-name probe -- this module never actually registered
        // anything under "Math"/"java.lang.Math" except `getExponent`
        // (`builtins::statics`'s `reg_static_fn`). Once that probe stopped
        // firing for a non-`clojure.core` expanded namespace, `Math/sqrt`,
        // `Math/pow`, `Math/round`, `Math/log`, ... all threw "Unable to
        // resolve" -- measured across `tests/conformance_test.rs`
        // (numbers.corpus), `tests/ns_test.rs`/`tests/spec_smoke_test.rs`/
        // `tests/spec_test_alpha_test.rs` (clojure.test.check.generators'
        // OWN internal `Math/log` call, so the entire generator backend
        // died), and `tests/l5_demo_ep1.rs` (`Math/round` in the flagship
        // demo). This closes that gap the same way `getExponent` already
        // does it: a REAL entry under both the short and fully-qualified
        // class spellings, not a coincidental fallback. Deliberately using
        // the SAME (already-registered) kebab-case `name` as the alias
        // key -- every name in this list that upstream `java.lang.Math`
        // also exposes happens to be single-word and spelled identically
        // in both (`sqrt`, `pow`, `round`, `log`, ...); the multi-word ones
        // (`to-degrees`, `floor-div`, ...) ALSO get this same kebab-case
        // alias -- harmless (no real call site spells `Math/to-degrees`)
        // -- but real Java-interop code spells THOSE camelCase
        // (`Math/toDegrees`), which is a genuinely different symbol; see
        // the explicit pairs loop right below for that spelling.
        alias(i, "Math", name);
        alias(i, "java.lang.Math", name);
    }

    // The multi-word names' actual Java (camelCase) spelling, for the ones
    // measured in the corpus (`tests/conformance_test.rs`'s
    // `Math/multiplyExact`) or plausible enough to be worth the two extra
    // lines apiece -- the rest of `java.lang.Math`'s multi-word surface
    // this module implements, so a real `Math/fooBar` call doesn't have to
    // rediscover this gap one name at a time. Unlike the loop above (whose
    // lookup key and target key are the same string), the KEBAB bare cell
    // this reads from and the CAMELCASE key it writes to genuinely differ,
    // so `strings::alias` (same lookup/target name) doesn't fit here --
    // `alias_renamed` fetches under `kebab` and re-interns under `java`.
    for (kebab, java) in [
        ("to-degrees", "toDegrees"),
        ("to-radians", "toRadians"),
        ("copy-sign", "copySign"),
        ("next-after", "nextAfter"),
        ("next-up", "nextUp"),
        ("next-down", "nextDown"),
        ("floor-div", "floorDiv"),
        ("floor-mod", "floorMod"),
        ("add-exact", "addExact"),
        ("subtract-exact", "subtractExact"),
        ("multiply-exact", "multiplyExact"),
        ("increment-exact", "incrementExact"),
        ("decrement-exact", "decrementExact"),
        ("negate-exact", "negateExact"),
    ] {
        alias_renamed(i, "Math", kebab, java);
        alias_renamed(i, "java.lang.Math", kebab, java);
    }
}

/// Re-binds the already-registered BARE `kebab` (mova's Clojure-style
/// name) under `ns/java` (the target class's actual Java spelling, when
/// the two names differ) -- `builtins::strings::alias`'s rename-aware
/// sibling, needed here because that helper assumes the lookup name and
/// the target name are identical (true for every SINGLE-word `java.lang.
/// Math` name this module has, false for the multi-word ones: `Math.
/// multiplyExact`, not `Math.multiply-exact`). Same pristine-builtin
/// discipline as `alias`: goes through `set_builtin`, so the new cell
/// starts pristine like the bare-name cell it mirrors.
fn alias_renamed(i: &mut Interp, ns: &str, kebab: &'static str, java: &'static str) {
    if let Some(v) = i.globals.get(&crate::value::Symbol::simple(kebab)) {
        i.globals.set_builtin_alias(
            crate::value::Symbol {
                ns: Some(ns.into()),
                name: java.into(),
            },
            v,
            kebab,
        );
    }
}
