//! Clojure-exact `Ratio` / `BigInt` / `BigDecimal` value types.
//!
//! Clojure's numeric tower has three types Rust's own numeric types have no
//! equivalent for: an always-reduced arbitrary-precision rational
//! (`clojure.lang.Ratio`), an arbitrary-precision integer that nonetheless
//! prints differently from -- and must cross-equal -- a machine `long`
//! (`clojure.lang.BigInt`), and Java's unscaled-integer-plus-scale decimal
//! (`java.math.BigDecimal`, with its own idiosyncratic `toString` and a
//! scale-INSENSITIVE notion of equality). None of that is `num-bigint`'s
//! job -- it gives us the arbitrary-precision integer primitive
//! (`BigInt`/gcd) and nothing else -- so this module hand-rolls the
//! Clojure/JVM semantics on top of it. We deliberately do NOT depend on
//! `num-rational` or `bigdecimal`: neither crate's normalization rule
//! (reduce-then-maybe-collapse-to-an-integer for `Ratio`; scale-insensitive
//! `=`/`hash` for `BigDecimal`) matches Clojure's, so pulling one in would
//! just mean fighting its invariants instead of encoding ours directly.
//!
//! `BigDecVal::to_java_string` is a line-by-line transcription of the
//! algorithm documented on `java.math.BigDecimal.toString()` (see the
//! comment on that method for the derivation of `adjusted`/plain-vs-
//! scientific notation). The test table for it below is not invented --
//! every row is measured output from running real Clojure 1.13.0-alpha6 on
//! JDK 21 (see SPEC-A-bignum.md); if a row here ever stops matching this
//! code, the code is wrong, not the row.
//!
//! This module started self-contained and not yet wired into `Value` --
//! see `SPEC-A-bignum.md`. `SPEC-B-bignum-wiring.md` did that wiring:
//! `Value::BigInt`/`Value::Ratio`/`Value::BigDec` (in `value.rs`), the
//! reader (`reader.rs::parse_number`), the printer (`printer.rs::
//! write_value`), and the cross-type `=`/`hash` bridging (`(= 1N 1)`,
//! `(hash 7N) == (hash 7)`, etc., using `BigIntVal::to_i64_exact` as the
//! bridge) all now live outside this module and its own `#[cfg(test)]`
//! unit tests -- the types here are load-bearing production code, not
//! designed-but-dormant.

//! S5 (SPEC-numtower): the types below stopped being merely *readable* and
//! became *arithmetic*. `BigDecVal` grew Java-`BigDecimal`-exact `add`/
//! `sub`/`mul`/`divide`/`quot`/`rem` plus the `MathContext` rounding
//! `with-precision` needs, and `RatioVal` grew the reduce-on-every-result
//! rational algebra `clojure.lang.Ratio` performs. Every scale rule here
//! (`+`/`-` take `max(s1,s2)`, `*` takes `s1+s2`, exact `/` takes the
//! smallest scale >= `s1-s2` that represents the quotient exactly,
//! `divideToIntegralValue` takes `s1-s2`) is measured, not guessed -- see
//! `compat/numtower-oracle-transcript.txt` and the unit tables below.

use std::cmp::Ordering;
use std::hash::{Hash, Hasher};

use num_bigint::BigInt;
use num_integer::Integer;
use num_traits::{One, Signed, ToPrimitive, Zero};

/// Java `java.math.RoundingMode`, the subset `with-precision`'s
/// `:rounding` argument can name. `HALF_UP` is `MathContext`'s own default
/// and therefore `with-precision`'s (measured: `(with-precision 4 (/ 1M
/// 3M))` => `0.3333M`, `(with-precision 4 :rounding CEILING (/ 1M 3M))` =>
/// `0.3334M`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoundingMode {
    Up,
    Down,
    Ceiling,
    Floor,
    HalfUp,
    HalfDown,
    HalfEven,
    Unnecessary,
}

impl RoundingMode {
    /// `None` for a name Java's enum doesn't have -- the caller turns that
    /// into the same `IllegalArgumentException` shape Java's
    /// `RoundingMode.valueOf` would.
    pub fn parse(name: &str) -> Option<RoundingMode> {
        Some(match name {
            "UP" => RoundingMode::Up,
            "DOWN" => RoundingMode::Down,
            "CEILING" => RoundingMode::Ceiling,
            "FLOOR" => RoundingMode::Floor,
            "HALF_UP" => RoundingMode::HalfUp,
            "HALF_DOWN" => RoundingMode::HalfDown,
            "HALF_EVEN" => RoundingMode::HalfEven,
            "UNNECESSARY" => RoundingMode::Unnecessary,
            _ => return None,
        })
    }
}

/// `n` clamped into `i64` range: exact when it fits, `i64::MAX`/`i64::MIN`
/// (by sign) otherwise. See [`BigIntVal::to_i64_saturating`], its main
/// caller.
pub(crate) fn bigint_to_i64_saturating(n: &BigInt) -> i64 {
    n.to_i64().unwrap_or(if n.sign() == num_bigint::Sign::Minus { i64::MIN } else { i64::MAX })
}

/// Number of decimal digits in `|n|` -- `1` for zero, matching Java's
/// `BigDecimal.precision()` convention for a zero unscaled value.
fn digit_count(n: &BigInt) -> u64 {
    if n.is_zero() {
        1
    } else {
        // `to_string` on a `BigInt` is sign + plain digits, so the digit
        // count is the length minus a possible leading `-`. Going through
        // the decimal rendering rather than a log10 estimate keeps this
        // exact for every magnitude (a float log10 is off by one near
        // powers of ten).
        let s = n.magnitude().to_str_radix(10);
        s.len() as u64
    }
}

fn ten_pow(k: u32) -> BigInt {
    BigInt::from(10).pow(k)
}

/// Round `q` (a truncated-toward-zero quotient) up in magnitude or not,
/// given the dropped remainder `r` over `divisor` and the sign of the true
/// quotient. `r` is the remainder of the truncating division, `divisor`
/// its (positive) divisor, and `sticky` records whether anything was ALSO
/// dropped before this step (so `0.4999...` never looks like an exact
/// `0.5`). Returns the rounded magnitude-adjusted quotient.
fn apply_rounding(
    q: BigInt,
    r: &BigInt,
    divisor: &BigInt,
    sticky: bool,
    negative: bool,
    mode: RoundingMode,
) -> Result<BigInt, ArithError> {
    let exact = r.is_zero() && !sticky;
    if exact {
        return Ok(q);
    }
    // `2*|r|` vs `divisor` is the half test without any division.
    let twice = r.abs() * 2u32;
    let cmp_half = twice.cmp(divisor);
    let round_away = match mode {
        RoundingMode::Up => true,
        RoundingMode::Down => false,
        RoundingMode::Ceiling => !negative,
        RoundingMode::Floor => negative,
        RoundingMode::HalfUp => cmp_half != Ordering::Less,
        RoundingMode::HalfDown => cmp_half == Ordering::Greater,
        RoundingMode::HalfEven => match cmp_half {
            Ordering::Greater => true,
            Ordering::Less => false,
            // Exactly half: round to the even neighbour.
            Ordering::Equal => (&q % 2u32) != BigInt::zero(),
        },
        RoundingMode::Unnecessary => {
            return Err(ArithError::RoundingNecessary);
        }
    };
    if round_away {
        // `q` is truncated toward zero, so "away from zero" is +1 for a
        // non-negative quotient and -1 for a negative one.
        Ok(if negative { q - 1u32 } else { q + 1u32 })
    } else {
        Ok(q)
    }
}

/// The arithmetic failures this module can raise. Deliberately a small
/// enum rather than the interpreter's `RjError`: `bignum.rs` stays free of
/// the evaluator, and `builtins::numbers` maps each variant to the exact
/// measured JVM message (`"Divide by zero"`, `"Non-terminating decimal
/// expansion; no exact representable decimal result."`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArithError {
    DivideByZero,
    NonTerminating,
    RoundingNecessary,
}

/// Clojure's `clojure.lang.BigInt`. Distinct from `Value::Int` because
/// `(pr-str 7N)` is `"7N"` while `(pr-str 7)` is `"7"` -- they must print
/// differently while still comparing `=` and hashing the same (measured:
/// `(= 1N 1)` is true, `(hash 7N) == (hash 7)`). This type owns only the
/// numeric value and the `i64`-fitting query the integration layer needs
/// for that bridging; the `N` suffix and the cross-type equality itself
/// belong to the printer and to `value.rs` respectively.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BigIntVal(pub BigInt);

impl BigIntVal {
    pub fn from_i64(v: i64) -> Self {
        BigIntVal(BigInt::from(v))
    }

    /// `Some(n)` iff the value fits exactly in an `i64`. The integration
    /// layer uses this so `(= 1N 1)` is true and `(hash 1N) == (hash 1)`
    /// -- both only make sense when the BigInt actually fits in the
    /// machine-`long` domain Clojure's own `Long`/`BigInt` bridging uses.
    pub fn to_i64_exact(&self) -> Option<i64> {
        self.0.to_i64()
    }

    /// Like [`Self::to_i64_exact`], but a magnitude past the `i64` range
    /// saturates to `i64::MAX`/`i64::MIN` (by sign) instead of returning
    /// `None` -- for callers that want "clamp to a machine int" rather
    /// than "must fit exactly" (e.g. `take`/`drop`'s count argument,
    /// which never needs to distinguish "huge" from "bigger huge").
    pub fn to_i64_saturating(&self) -> i64 {
        bigint_to_i64_saturating(&self.0)
    }

    /// Plain decimal digits, no `N` suffix -- the printer appends that.
    /// `BigInt`'s own `Display` is already exactly this (sign then plain
    /// digits, no separators, no leading zeros), so this is a thin
    /// re-export rather than a reimplementation.
    pub fn to_decimal_string(&self) -> String {
        self.0.to_string()
    }

    pub fn is_zero(&self) -> bool {
        self.0.is_zero()
    }

    /// Widening `f64` view -- `clojure.math`'s `^double`-hinted fns accept
    /// ANY `java.lang.Number` and widen via `.doubleValue()` (measured:
    /// `(clojure.math/sin 100000000000000000000N)` succeeds and returns
    /// `-0.6452512852657808`; real Clojure does NOT reject a `BigInt`
    /// argument to a double-hinted `clojure.math` fn, contrary to the
    /// naive assumption that a `^double` hint implies a JVM `Double` cast).
    /// Mirrors [`RatioVal::to_f64`]'s own `unwrap_or(f64::NAN)` fallback
    /// (in practice `BigInt -> f64` never actually fails -- `ToPrimitive`
    /// saturates to `+-Infinity` for a magnitude past `f64::MAX` -- the
    /// fallback exists only so this can't panic if that ever changes).
    pub fn to_f64(&self) -> f64 {
        self.0.to_f64().unwrap_or(f64::NAN)
    }

    /// Java `BigInteger.hashCode()`: `h = 31*h + word` over the
    /// big-endian 32-bit magnitude words, then multiplied by the signum.
    /// Reproduced exactly (not approximated) because `clojure.lang.Ratio`
    /// hashes as `numerator.hashCode() ^ denominator.hashCode()` and
    /// `BigInt`'s own `hasheq` falls back to it once the value no longer
    /// fits a `long` -- both are values `(hash ..)` must reproduce
    /// digit-for-digit (measured: `(hash 1/3)` => `2`, `(hash
    /// 12345678901234567890N)` => `-1436577082`).
    pub fn java_hash_code(&self) -> i32 {
        java_bigint_hash(&self.0)
    }
}

/// Java `java.math.BigInteger.hashCode()` for an arbitrary `BigInt`.
pub fn java_bigint_hash(n: &BigInt) -> i32 {
    let mut h: i32 = 0;
    // `to_u32_digits` is little-endian; Java walks the magnitude
    // big-endian, so iterate in reverse.
    let (sign, digits) = n.to_u32_digits();
    for w in digits.iter().rev() {
        h = h.wrapping_mul(31).wrapping_add(*w as i32);
    }
    let signum: i32 = match sign {
        num_bigint::Sign::Minus => -1,
        num_bigint::Sign::NoSign => 0,
        num_bigint::Sign::Plus => 1,
    };
    h.wrapping_mul(signum)
}

/// Clojure's `clojure.lang.Ratio`. INVARIANTS, upheld by construction (the
/// fields are private and the only constructor is [`RatioVal::reduce`]):
/// `den > 0`, `gcd(|num|, den) == 1`, and `den != 1` -- a denominator of 1
/// means the value is an integer, not a Ratio (measured: `4/2` reads as
/// the Long `2`, not a Ratio) -- see [`Reduced`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RatioVal {
    num: BigInt,
    den: BigInt,
}

/// What reducing `num/den` produced: Clojure collapses `4/2` to the Long
/// `2` and `0/5` to the Long `0`, but keeps `1/3` as a genuine Ratio.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reduced {
    Int(BigIntVal),
    Ratio(RatioVal),
}

/// Reader-time error for `n/0` -- Clojure throws `ArithmeticException:
/// Divide by zero` when the literal is read, not lazily when the value is
/// later used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DivideByZero;

impl RatioVal {
    /// Reduce `num/den` to lowest terms with a positive denominator.
    /// `Err(DivideByZero)` when `den == 0` (measured: `1/0` throws
    /// `ArithmeticException: Divide by zero` at read time).
    ///
    /// Sign lives entirely on the numerator: if `den` arrives negative
    /// (this module's own callers only, never the reader -- `1/-2` is
    /// rejected by the tokenizer before it gets here, per
    /// SPEC-A-bignum.md) both are negated first so the invariant `den > 0`
    /// holds unconditionally afterward.
    pub fn reduce(num: BigInt, den: BigInt) -> Result<Reduced, DivideByZero> {
        if den.is_zero() {
            return Err(DivideByZero);
        }
        let (mut num, mut den) = if den.sign() == num_bigint::Sign::Minus {
            (-num, -den)
        } else {
            (num, den)
        };
        // `Integer::gcd` on `BigInt` returns a non-negative divisor even
        // when one operand is negative or zero (`gcd(0, n) == n`), so this
        // also correctly folds `0/5` down to `0/1` in one step.
        let g = num.gcd(&den);
        if !g.is_zero() && !g.is_one() {
            num /= &g;
            den /= &g;
        }
        if den.is_one() {
            Ok(Reduced::Int(BigIntVal(num)))
        } else {
            Ok(Reduced::Ratio(RatioVal { num, den }))
        }
    }

    pub fn numer(&self) -> &BigInt {
        &self.num
    }

    pub fn denom(&self) -> &BigInt {
        &self.den
    }

    /// `"1/3"`, `"-1/2"` -- sign lives on the numerator (upheld by
    /// [`RatioVal::reduce`]), so this is just `numerator/denominator`.
    pub fn to_ratio_string(&self) -> String {
        format!("{}/{}", self.num, self.den)
    }

    /// Best-effort; used by `==`/coercion later. Not correctly-rounded for
    /// numerators/denominators too large to survive the round-trip through
    /// `f64` individually -- Clojure's own `Ratio.doubleValue()` does a
    /// proper `BigDecimal` division, which a later integration pass can
    /// switch to if that gap ever matters for a real program.
    pub fn to_f64(&self) -> f64 {
        let n = self.num.to_f64().unwrap_or(f64::NAN);
        let d = self.den.to_f64().unwrap_or(f64::NAN);
        n / d
    }

    /// `numerator / denominator`, truncated toward zero -- the integer
    /// half of `SPEC-C-casts.md`'s truncation rule ("Ratio truncates
    /// toward zero (numer/denom integer division truncated)"), used by
    /// the `byte`/`short`/`int`/`long`/`bigint` casts. `num_bigint::
    /// BigInt`'s `Div` is already round-toward-zero (matching Rust's own
    /// integer division, not floor division), so this is a direct
    /// division, not a hand-rolled truncation -- e.g. `1/2` -> `0`,
    /// `-1/2` -> `0` (not `-1`, which floor division would give).
    pub fn trunc_to_bigint(&self) -> BigInt {
        &self.num / &self.den
    }

    /// Attempts an EXACT decimal expansion of `self` as a `BigDecVal` --
    /// `Some` iff the reduced denominator's only prime factors are 2 and
    /// 5 (the standard "does this fraction terminate in base 10" test).
    /// Measured: `(bigdec 1/2)` -> `0.5M` (terminates); `(bigdec 1/3)`
    /// throws `ArithmeticException: Non-terminating decimal expansion; no
    /// exact representable decimal result.` (`den == 3` has a factor
    /// other than 2/5, so this returns `None` and the caller raises that
    /// error). `RatioVal::reduce`'s invariants (`den > 0`, `gcd(num, den)
    /// == 1`) are exactly what make "no factors left after dividing out
    /// 2s and 5s" a valid termination test -- a non-reduced fraction
    /// could carry spurious non-2/5 factors that actually cancel against
    /// the numerator.
    ///
    /// When it terminates, the exact scale is `max(count_2, count_5)`
    /// (the number of decimal digits needed is bounded by whichever
    /// prime needs more factors-of-ten to clear), and the unscaled value
    /// is `numerator * 2^(scale - count_2) * 5^(scale - count_5)` --
    /// always an exact integer multiplication, never a lossy division,
    /// since `10^scale = 2^scale * 5^scale` is divisible by `den =
    /// 2^count_2 * 5^count_5` by construction.
    pub fn to_exact_bigdec(&self) -> Option<BigDecVal> {
        let mut d = self.den.clone();
        let two = BigInt::from(2);
        let five = BigInt::from(5);
        let mut count2: u32 = 0;
        while (&d % &two).is_zero() {
            d /= &two;
            count2 += 1;
        }
        let mut count5: u32 = 0;
        while (&d % &five).is_zero() {
            d /= &five;
            count5 += 1;
        }
        if !d.is_one() {
            return None;
        }
        let scale = count2.max(count5);
        let unscaled = &self.num * two.pow(scale - count2) * five.pow(scale - count5);
        Some(BigDecVal::new(unscaled, scale as i32))
    }

    /// Java `clojure.lang.Ratio.hashCode()`:
    /// `numerator.hashCode() ^ denominator.hashCode()`. `Ratio` is a
    /// `Number` but NOT `IHashEq`, so `clojure.lang.Util.hasheq` falls all
    /// the way through `Numbers.hasheq` to this (measured: `(hash 1/3)` =>
    /// `2` == `1 ^ 3`, `(hash -1/3)` => `-4` == `-1 ^ 3`).
    pub fn java_hash_code(&self) -> i32 {
        java_bigint_hash(&self.num) ^ java_bigint_hash(&self.den)
    }
}

/// The rational algebra `clojure.lang.RatioOps` performs, expressed once
/// over raw `(numerator, denominator)` pairs so every caller
/// (`+`/`-`/`*`/`/`, and the `Int`/`BigInt` operands those promote to
/// `n/1`) shares one reduce-on-every-result implementation. Each returns a
/// [`Reduced`], because Clojure collapses an integral result back out of
/// `Ratio` (measured: `(+ 1/3 1/6)` => `1/2` stays a Ratio, but `(* 1/3
/// 3)` => `1N` becomes a BigInt).
pub mod ratio_ops {
    use super::{BigInt, DivideByZero, RatioVal, Reduced};

    pub fn add(n1: &BigInt, d1: &BigInt, n2: &BigInt, d2: &BigInt) -> Reduced {
        RatioVal::reduce(n1 * d2 + n2 * d1, d1 * d2).expect("d1*d2 != 0 by Ratio's own invariant")
    }

    pub fn sub(n1: &BigInt, d1: &BigInt, n2: &BigInt, d2: &BigInt) -> Reduced {
        RatioVal::reduce(n1 * d2 - n2 * d1, d1 * d2).expect("d1*d2 != 0 by Ratio's own invariant")
    }

    pub fn mul(n1: &BigInt, d1: &BigInt, n2: &BigInt, d2: &BigInt) -> Reduced {
        RatioVal::reduce(n1 * n2, d1 * d2).expect("d1*d2 != 0 by Ratio's own invariant")
    }

    /// `(n1/d1) / (n2/d2) == (n1*d2) / (d1*n2)` -- `Err(DivideByZero)`
    /// exactly when `n2` is zero (measured: `(/ 1/2 0)` throws
    /// `ArithmeticException: Divide by zero`).
    pub fn div(
        n1: &BigInt,
        d1: &BigInt,
        n2: &BigInt,
        d2: &BigInt,
    ) -> Result<Reduced, DivideByZero> {
        RatioVal::reduce(n1 * d2, d1 * n2)
    }
}

/// Java's `java.math.BigDecimal`: value = `unscaled * 10^(-scale)`. Both
/// fields are private; [`BigDecVal::new`] takes them as-is (unlike
/// `Ratio`, Java's `BigDecimal` performs NO normalization on construction
/// -- `1.50M` and `1.5M` are genuinely different unscaled/scale pairs that
/// only become equal through the scale-insensitive `PartialEq` below).
#[derive(Clone, Debug)]
pub struct BigDecVal {
    unscaled: BigInt,
    scale: i32,
}

impl BigDecVal {
    pub fn new(unscaled: BigInt, scale: i32) -> Self {
        BigDecVal { unscaled, scale }
    }

    pub fn unscaled(&self) -> &BigInt {
        &self.unscaled
    }

    pub fn scale(&self) -> i32 {
        self.scale
    }

    /// Parse the numeric BODY of a Clojure BigDecimal literal -- i.e. the
    /// token with its trailing `M` already removed. Grammar (hand-rolled,
    /// not regex, so every rejection is a named branch rather than a
    /// pattern to reverse-engineer):
    ///
    /// ```text
    /// body       := sign? significand exponent?
    /// significand:= digits ('.' digits?)?      -- but a bare trailing '.'
    ///             | '.' digits                    with nothing after it
    ///                                              is REJECTED (Java does
    ///                                              too: measured `"1."` ->
    ///                                              None)
    /// exponent   := ('e' | 'E') sign? digits
    /// digits     := ['0'-'9']+
    /// ```
    ///
    /// At least one digit must appear somewhere in the significand (so
    /// `"."`, `"+"`, `""` are all rejected too).
    pub fn parse(body: &str) -> Option<BigDecVal> {
        let bytes = body.as_bytes();
        let mut i = 0usize;
        let negative = match bytes.first() {
            Some(b'-') => {
                i += 1;
                true
            }
            Some(b'+') => {
                i += 1;
                false
            }
            _ => false,
        };

        let int_start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        let int_digits = &body[int_start..i];

        let mut saw_dot = false;
        let mut frac_digits = "";
        if i < bytes.len() && bytes[i] == b'.' {
            saw_dot = true;
            i += 1;
            let frac_start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            frac_digits = &body[frac_start..i];
            // Java rejects a trailing bare `.` -- there must be at least
            // one digit after the point (measured: `"1."` -> None).
            if frac_digits.is_empty() {
                return None;
            }
        }

        if int_digits.is_empty() && (!saw_dot || frac_digits.is_empty()) {
            // No digits anywhere in the significand: "", "+", ".", "-." ...
            return None;
        }

        let mut exponent: i32 = 0;
        if i < bytes.len() && (bytes[i] == b'e' || bytes[i] == b'E') {
            i += 1;
            let exp_negative = match bytes.get(i) {
                Some(b'-') => {
                    i += 1;
                    true
                }
                Some(b'+') => {
                    i += 1;
                    false
                }
                _ => false,
            };
            let exp_start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            if i == exp_start {
                // "e"/"E" with no digits following at all.
                return None;
            }
            let magnitude: i32 = body[exp_start..i].parse().ok()?;
            exponent = if exp_negative { -magnitude } else { magnitude };
        }

        if i != bytes.len() {
            // Trailing garbage after the exponent (or after the
            // significand, if there was no exponent at all).
            return None;
        }

        let mut unscaled_digits = String::with_capacity(int_digits.len() + frac_digits.len());
        unscaled_digits.push_str(int_digits);
        unscaled_digits.push_str(frac_digits);
        let magnitude = BigInt::parse_bytes(unscaled_digits.as_bytes(), 10)?;
        let unscaled = if negative { -magnitude } else { magnitude };
        let scale = frac_digits.len() as i32 - exponent;
        Some(BigDecVal { unscaled, scale })
    }

    /// EXACTLY `java.math.BigDecimal.toString()`. Transcribed from the
    /// javadoc algorithm (see the module doc): let `d` be the number of
    /// decimal digits in `|unscaled|` (`d = 1` for zero) and
    /// `adjusted = d - 1 - scale`.
    ///
    /// - `scale == 0`: plain digits, no decimal point.
    /// - `scale > 0 && adjusted >= -6`: plain notation with a decimal
    ///   point inserted (or leading `0.00…` zeros if there aren't enough
    ///   digits).
    /// - otherwise (`scale < 0`, or `adjusted < -6`): scientific notation,
    ///   `d.dddEsNN` -- and Java DOES emit the `+` sign on a non-negative
    ///   exponent (`1E+3`, not `1E3`).
    pub fn to_java_string(&self) -> String {
        let full = self.unscaled.to_string();
        let (neg, digits) = match full.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, full.as_str()),
        };
        let d = digits.len() as i32;
        let s = self.scale;
        let adjusted = d - 1 - s;

        let body = if s == 0 {
            digits.to_string()
        } else if s > 0 && adjusted >= -6 {
            if d > s {
                let point = (d - s) as usize;
                format!("{}.{}", &digits[..point], &digits[point..])
            } else {
                let zeros = "0".repeat((s - d) as usize);
                format!("0.{zeros}{digits}")
            }
        } else {
            let mut mantissa = String::from(&digits[..1]);
            if digits.len() > 1 {
                mantissa.push('.');
                mantissa.push_str(&digits[1..]);
            }
            let exp_sign = if adjusted >= 0 { '+' } else { '-' };
            format!("{mantissa}E{exp_sign}{}", adjusted.abs())
        };

        if neg {
            format!("-{body}")
        } else {
            body
        }
    }

    /// Canonical form with trailing zeros stripped from `unscaled` (each
    /// stripped zero decrements `scale` to keep the numeric value fixed:
    /// `u * 10^-s == (u/10) * 10^-(s-1)`). Zero canonicalizes to
    /// `(unscaled=0, scale=0)` regardless of the input scale -- Java 9+
    /// behaviour; deliberately NOT the Java 8 `stripTrailingZeros` bug
    /// where `0.00` stripped to a *non-zero* scale.
    ///
    /// Equality and hashing go through this so `1M`, `1.0M`, `1.00M` are
    /// `=` and hash identically (measured), matching Clojure/Java.
    pub fn canonical(&self) -> BigDecVal {
        if self.unscaled.is_zero() {
            return BigDecVal {
                unscaled: BigInt::zero(),
                scale: 0,
            };
        }
        let ten = BigInt::from(10);
        let mut u = self.unscaled.clone();
        let mut s = self.scale;
        loop {
            let r = &u % &ten;
            if r.is_zero() {
                u /= &ten;
                s -= 1;
            } else {
                break;
            }
        }
        BigDecVal { unscaled: u, scale: s }
    }

    pub fn to_f64(&self) -> f64 {
        let base = self.unscaled.to_f64().unwrap_or(f64::NAN);
        base * 10f64.powi(-self.scale)
    }

    /// Truncates toward zero to an integer `BigInt` -- the "drop the
    /// fraction, keep the sign" half of `SPEC-C-casts.md`'s truncation
    /// rule ("BigDec truncates (drop fraction)"), used by the
    /// `byte`/`short`/`int`/`long`/`bigint` casts (measured: `(long
    /// 1.5M)` -> `1`, `(bigint 1.5M)` -> `1N`). `scale <= 0` means the
    /// value is already an integer (possibly with trailing zeros the
    /// scale encodes, e.g. `1E+3` is `unscaled=1, scale=-3`), so
    /// widening by `10^-scale` is exact, not a truncation. `scale > 0`
    /// divides by `10^scale` using `BigInt`'s own round-toward-zero
    /// `Div` (see `RatioVal::trunc_to_bigint`'s doc for why that's
    /// exactly the truncation this needs, sign included).
    pub fn trunc_to_bigint(&self) -> BigInt {
        if self.scale <= 0 {
            &self.unscaled * BigInt::from(10).pow((-self.scale) as u32)
        } else {
            &self.unscaled / BigInt::from(10).pow(self.scale as u32)
        }
    }

    /// `self` and `other` re-expressed at a shared scale (the larger of
    /// the two, which is always reachable from both by multiplying by a
    /// non-negative power of ten) so their `unscaled` integers become
    /// directly comparable. Shared by `Ord`/`PartialOrd`; `PartialEq`
    /// instead goes through [`Self::canonical`] since it only needs
    /// equality, not a total order, and canonicalizing is cheaper when the
    /// two scales already happen to match.
    pub fn from_i64(v: i64) -> Self {
        BigDecVal::new(BigInt::from(v), 0)
    }

    pub fn from_bigint(v: BigInt) -> Self {
        BigDecVal::new(v, 0)
    }

    pub fn is_zero(&self) -> bool {
        self.unscaled.is_zero()
    }

    pub fn signum(&self) -> i32 {
        match self.unscaled.sign() {
            num_bigint::Sign::Minus => -1,
            num_bigint::Sign::NoSign => 0,
            num_bigint::Sign::Plus => 1,
        }
    }

    /// Java `BigDecimal.precision()`: the number of digits in the
    /// unscaled value (`1` for zero).
    pub fn precision(&self) -> u64 {
        digit_count(&self.unscaled)
    }

    pub fn negate(&self) -> BigDecVal {
        BigDecVal::new(-&self.unscaled, self.scale)
    }

    pub fn abs(&self) -> BigDecVal {
        BigDecVal::new(self.unscaled.abs(), self.scale)
    }

    /// Java `BigDecimal.hashCode()`: `31 * unscaled.hashCode() + scale`.
    /// Clojure's `Numbers.hasheq` first `stripTrailingZeros()` (with an
    /// explicit zero special-case, since Java 8's strip left `0.00M` at a
    /// non-zero scale) so numerically-equal decimals hash equal --
    /// [`Self::canonical`] is exactly that strip, so this hashes the
    /// canonical form (measured: `(hash 1.5M)` == `(hash 1.50M)` == `466`
    /// == `31*15 + 1`; `(hash 0M)` => `0`; `(hash 1M)` => `31`).
    pub fn clojure_hash(&self) -> i32 {
        let c = self.canonical();
        java_bigint_hash(&c.unscaled)
            .wrapping_mul(31)
            .wrapping_add(c.scale)
    }

    /// Java `BigDecimal.add`: the result's scale is `max(s1, s2)`
    /// (measured: `(+ 1.50M 1.5M)` => `3.00M`).
    pub fn add(&self, other: &BigDecVal) -> BigDecVal {
        let s = self.scale.max(other.scale);
        BigDecVal::new(self.scaled_to(s) + other.scaled_to(s), s)
    }

    /// Java `BigDecimal.subtract`; same `max(s1, s2)` scale rule as
    /// [`Self::add`] (measured: `(- 1M 1.000M)` => `0.000M`).
    pub fn sub(&self, other: &BigDecVal) -> BigDecVal {
        let s = self.scale.max(other.scale);
        BigDecVal::new(self.scaled_to(s) - other.scaled_to(s), s)
    }

    /// Java `BigDecimal.multiply`: unscaled values multiply and the
    /// scales ADD (measured: `(* 1.5M 2M)` => `3.0M` -- scale 1+0 -- and
    /// `(* 1.5M 1.5M)` => `2.25M` -- scale 1+1).
    pub fn mul(&self, other: &BigDecVal) -> BigDecVal {
        BigDecVal::new(&self.unscaled * &other.unscaled, self.scale + other.scale)
    }

    /// Java `BigDecimal.divide(BigDecimal)` -- the EXACT, no-`MathContext`
    /// division. The quotient is computed as a reduced rational; if its
    /// denominator has any prime factor other than 2 or 5 the exact
    /// decimal expansion does not terminate and Java throws
    /// `ArithmeticException: Non-terminating decimal expansion; no exact
    /// representable decimal result.` (measured: `(/ 1M 3M)`).
    ///
    /// When it DOES terminate, Java returns the quotient at the smallest
    /// scale that represents it exactly, but never SMALLER than the
    /// "preferred scale" `s1 - s2` -- which is why `(/ 0.00M 3M)` is
    /// `0.00M` (preferred scale 2, exact value 0 needs scale 0, so it is
    /// padded back up) while `(/ 1M 8M)` is `0.125M` (preferred scale 0,
    /// but 3 digits are genuinely needed). Both measured.
    pub fn divide_exact(&self, other: &BigDecVal) -> Result<BigDecVal, ArithError> {
        if other.unscaled.is_zero() {
            return Err(ArithError::DivideByZero);
        }
        // value = (u1/u2) * 10^(s2 - s1)
        let reduced = RatioVal::reduce(self.unscaled.clone(), other.unscaled.clone())
            .map_err(|_| ArithError::DivideByZero)?;
        let (base_unscaled, base_scale) = match reduced {
            Reduced::Int(n) => (n.0, 0i32),
            Reduced::Ratio(r) => match r.to_exact_bigdec() {
                Some(d) => (d.unscaled, d.scale),
                None => return Err(ArithError::NonTerminating),
            },
        };
        // Shift by 10^(s2 - s1): multiplying the VALUE by 10^-k means
        // adding k to the scale.
        let shifted = BigDecVal::new(base_unscaled, base_scale + self.scale - other.scale);
        let preferred = self.scale - other.scale;
        Ok(shifted.with_min_scale(preferred))
    }

    /// Java `BigDecimal.divide(BigDecimal, MathContext)`: the exact
    /// quotient when it terminates within `precision` significant digits,
    /// otherwise the quotient rounded to exactly `precision` digits
    /// (measured: `(with-precision 4 (/ 1M 2M))` => `0.5M` -- exact, NOT
    /// padded to `0.5000M`; `(with-precision 4 (/ 1M 3M))` => `0.3333M`).
    pub fn divide_with_precision(
        &self,
        other: &BigDecVal,
        precision: u64,
        mode: RoundingMode,
    ) -> Result<BigDecVal, ArithError> {
        if other.unscaled.is_zero() {
            return Err(ArithError::DivideByZero);
        }
        if precision == 0 {
            return self.divide_exact(other);
        }
        // Java prefers the exact quotient whenever it exists AND fits the
        // requested precision -- that (not a rounding step) is what makes
        // `(with-precision 4 (/ 1M 2M))` print `0.5M`.
        if let Ok(exact) = self.divide_exact(other) {
            if exact.canonical().precision() <= precision {
                return Ok(exact);
            }
            return exact.round_to_precision(precision, mode);
        }
        // Non-terminating: generate `precision + 1` significant digits by
        // pre-scaling the dividend, then round the last one off. `k` is
        // chosen so the truncated quotient lands at `precision + 1` digits
        // (+/- one, which the loop below normalizes).
        let dx = digit_count(&self.unscaled) as i64;
        let dy = digit_count(&other.unscaled) as i64;
        let k = (precision as i64 + 1 + dy - dx).max(0) as u32;
        let scaled_dividend = &self.unscaled * ten_pow(k);
        let (q, r) = scaled_dividend.div_rem(&other.unscaled);
        let negative = (self.signum() * other.signum()) < 0;
        let dq = digit_count(&q);
        let mut unscaled = q;
        let mut scale = k as i32 + self.scale - other.scale;
        let sticky = !r.is_zero();
        if dq > precision {
            let drop = (dq - precision) as u32;
            let divisor = ten_pow(drop);
            let (q2, r2) = unscaled.div_rem(&divisor);
            unscaled = apply_rounding(q2, &r2, &divisor, sticky, negative, mode)?;
            scale -= drop as i32;
            // Rounding can carry into an extra digit (`0.999 -> 1.00`),
            // which would leave `precision + 1` digits; renormalize.
            if digit_count(&unscaled) > precision {
                let (q3, r3) = unscaled.div_rem(&BigInt::from(10));
                debug_assert!(r3.is_zero(), "carry can only ever add a trailing zero");
                unscaled = q3;
                scale -= 1;
            }
        }
        Ok(BigDecVal::new(unscaled, scale))
    }

    /// Round to at most `precision` significant digits, Java
    /// `BigDecimal.round(MathContext)`. Used by every `MathContext`-aware
    /// op (`with-precision` makes `+`/`-`/`*` round too -- measured:
    /// `(with-precision 2 (+ 1234M 1M))` => `1.2E+3M`).
    pub fn round_to_precision(
        &self,
        precision: u64,
        mode: RoundingMode,
    ) -> Result<BigDecVal, ArithError> {
        if precision == 0 || self.unscaled.is_zero() {
            return Ok(self.clone());
        }
        let d = digit_count(&self.unscaled);
        if d <= precision {
            return Ok(self.clone());
        }
        let drop = (d - precision) as u32;
        let divisor = ten_pow(drop);
        let (q, r) = self.unscaled.div_rem(&divisor);
        let negative = self.signum() < 0;
        let mut unscaled = apply_rounding(q, &r, &divisor, false, negative, mode)?;
        let mut scale = self.scale - drop as i32;
        if digit_count(&unscaled) > precision {
            unscaled /= BigInt::from(10);
            scale -= 1;
        }
        Ok(BigDecVal::new(unscaled, scale))
    }

    /// Java `BigDecimal.divideToIntegralValue`: the quotient truncated
    /// toward zero, at the preferred scale `s1 - s2` (measured: `(quot 7M
    /// 2M)` => `3M`, `(quot 7.5M 2M)` => `3.0M`, `(quot -7M 2M)` =>
    /// `-3M`).
    pub fn quot(&self, other: &BigDecVal) -> Result<BigDecVal, ArithError> {
        if other.unscaled.is_zero() {
            return Err(ArithError::DivideByZero);
        }
        let s = self.scale.max(other.scale);
        let q = self.scaled_to(s) / other.scaled_to(s);
        Ok(BigDecVal::new(q, 0).with_min_scale(self.scale - other.scale))
    }

    /// Java `BigDecimal.remainder`: `this - (this quot other) * other`,
    /// i.e. the sign follows the DIVIDEND (measured: `(rem -7M 2M)` =>
    /// `-1M`, `(rem 7.5M 2M)` => `1.5M`).
    pub fn rem(&self, other: &BigDecVal) -> Result<BigDecVal, ArithError> {
        let q = self.quot(other)?;
        Ok(self.sub(&q.mul(other)))
    }

    /// `self` re-expressed at `target` when that needs MORE digits than
    /// its own canonical form, otherwise unchanged. This is the
    /// "preferred scale" padding Java's exact `divide`/
    /// `divideToIntegralValue` perform; the canonicalization first is what
    /// keeps `(/ 1M 8M)` at `0.125M` rather than dragging along the
    /// trailing zeros an intermediate computation happened to produce.
    fn with_min_scale(&self, target: i32) -> BigDecVal {
        let c = self.canonical();
        if c.scale >= target {
            return c;
        }
        let diff = (target - c.scale) as u32;
        BigDecVal::new(c.unscaled * ten_pow(diff), target)
    }

    fn scaled_to(&self, target_scale: i32) -> BigInt {
        let diff = target_scale - self.scale;
        debug_assert!(diff >= 0, "target_scale must be >= self.scale");
        if diff == 0 {
            self.unscaled.clone()
        } else {
            &self.unscaled * BigInt::from(10).pow(diff as u32)
        }
    }
}

impl PartialEq for BigDecVal {
    fn eq(&self, other: &Self) -> bool {
        let a = self.canonical();
        let b = other.canonical();
        a.unscaled == b.unscaled && a.scale == b.scale
    }
}

impl Eq for BigDecVal {}

impl Hash for BigDecVal {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let c = self.canonical();
        c.unscaled.hash(state);
        c.scale.hash(state);
    }
}

impl PartialOrd for BigDecVal {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for BigDecVal {
    fn cmp(&self, other: &Self) -> Ordering {
        let target = self.scale.max(other.scale);
        self.scaled_to(target).cmp(&other.scaled_to(target))
    }
}

/// Parse `digits` in `radix` (2..=36, case-insensitive) into a BigInt.
/// `None` if `digits` is empty or has a character invalid for the radix --
/// the reader uses this for `#x...` style non-decimal integer literals and
/// relies on both rejections (an empty run of digits after a radix prefix
/// is exactly as malformed as a bad digit character).
pub fn parse_bigint_radix(digits: &str, radix: u32, negative: bool) -> Option<BigIntVal> {
    if digits.is_empty() {
        return None;
    }
    let magnitude = BigInt::parse_bytes(digits.as_bytes(), radix)?;
    let value = if negative { -magnitude } else { magnitude };
    Some(BigIntVal(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bi(v: i64) -> BigInt {
        BigInt::from(v)
    }

    // --- Ratio -------------------------------------------------------

    #[test]
    fn ratio_reduces_and_collapses_to_int() {
        match RatioVal::reduce(bi(1), bi(3)) {
            Ok(Reduced::Ratio(r)) => assert_eq!(r.to_ratio_string(), "1/3"),
            other => panic!("1/3 expected Ratio, got {other:?}"),
        }
        match RatioVal::reduce(bi(2), bi(4)) {
            Ok(Reduced::Ratio(r)) => assert_eq!(r.to_ratio_string(), "1/2"),
            other => panic!("2/4 expected reduced Ratio 1/2, got {other:?}"),
        }
        match RatioVal::reduce(bi(-1), bi(2)) {
            Ok(Reduced::Ratio(r)) => assert_eq!(r.to_ratio_string(), "-1/2"),
            other => panic!("-1/2 expected Ratio, got {other:?}"),
        }
        match RatioVal::reduce(bi(4), bi(2)) {
            Ok(Reduced::Int(n)) => assert_eq!(n.to_decimal_string(), "2"),
            other => panic!("4/2 expected Long 2, got {other:?}"),
        }
        match RatioVal::reduce(bi(-4), bi(2)) {
            Ok(Reduced::Int(n)) => assert_eq!(n.to_decimal_string(), "-2"),
            other => panic!("-4/2 expected Long -2, got {other:?}"),
        }
        match RatioVal::reduce(bi(0), bi(5)) {
            Ok(Reduced::Int(n)) => assert_eq!(n.to_decimal_string(), "0"),
            other => panic!("0/5 expected Long 0, got {other:?}"),
        }
        match RatioVal::reduce(bi(10), bi(5)) {
            Ok(Reduced::Int(n)) => assert_eq!(n.to_decimal_string(), "2"),
            other => panic!("10/5 expected Long 2, got {other:?}"),
        }
        // Numerator exceeds i64 -- exercises real bignum division, not
        // just the small-int fast path.
        let big_num: BigInt = "1000000000000000000000".parse().expect("valid decimal");
        match RatioVal::reduce(big_num, bi(3)) {
            Ok(Reduced::Ratio(r)) => {
                assert_eq!(r.to_ratio_string(), "1000000000000000000000/3");
            }
            other => panic!("big/3 expected Ratio, got {other:?}"),
        }
    }

    #[test]
    fn ratio_divide_by_zero() {
        assert_eq!(RatioVal::reduce(bi(1), bi(0)), Err(DivideByZero));
    }

    // --- BigInt --------------------------------------------------------

    #[test]
    fn bigint_i64_roundtrip() {
        assert_eq!(BigIntVal::from_i64(7).to_i64_exact(), Some(7));
        assert_eq!(BigIntVal::from_i64(0).to_i64_exact(), Some(0));
        assert_eq!(BigIntVal::from_i64(-7).to_i64_exact(), Some(-7));
        assert_eq!(
            BigIntVal::from_i64(9_007_199_254_740_993).to_i64_exact(),
            Some(9_007_199_254_740_993)
        );

        let huge: BigInt = "170141183460469231731687303715884105728"
            .parse()
            .expect("valid decimal");
        assert_eq!(BigIntVal(huge).to_i64_exact(), None);
    }

    #[test]
    fn bigint_prints_plain_digits() {
        assert_eq!(BigIntVal::from_i64(7).to_decimal_string(), "7");
        assert_eq!(BigIntVal::from_i64(0).to_decimal_string(), "0");
        assert_eq!(BigIntVal::from_i64(-7).to_decimal_string(), "-7");
        let huge: BigInt = "170141183460469231731687303715884105728"
            .parse()
            .expect("valid decimal");
        assert_eq!(
            BigIntVal(huge).to_decimal_string(),
            "170141183460469231731687303715884105728"
        );
    }

    #[test]
    fn radix_parse() {
        assert_eq!(
            parse_bigint_radix("ff", 16, false).map(|v| v.to_decimal_string()),
            Some("255".to_string())
        );
        assert_eq!(
            parse_bigint_radix("FF", 16, false).map(|v| v.to_decimal_string()),
            Some("255".to_string())
        );
        assert_eq!(
            parse_bigint_radix("ff", 16, true).map(|v| v.to_decimal_string()),
            Some("-255".to_string())
        );
        assert_eq!(
            parse_bigint_radix("101", 2, false).map(|v| v.to_decimal_string()),
            Some("5".to_string())
        );
        assert_eq!(parse_bigint_radix("", 16, false), None);
        assert_eq!(parse_bigint_radix("g", 16, false), None); // 'g' invalid in base 16
        assert_eq!(
            parse_bigint_radix("z", 36, false).map(|v| v.to_decimal_string()),
            Some("35".to_string())
        );
    }

    // --- BigDecimal parse ------------------------------------------------

    #[test]
    fn bigdec_parse_matches_oracle() {
        let cases: &[(&str, i64, i32)] = &[
            ("1.5", 15, 1),
            ("1e3", 1, -3),
            ("1.23E+5", 123, -3),
            ("-1.50", -150, 2),
            ("0.0000001", 1, 7),
            ("100", 100, 0),
            (".5", 5, 1),
        ];
        for (body, unscaled, scale) in cases {
            let parsed = BigDecVal::parse(body)
                .unwrap_or_else(|| panic!("expected {body:?} to parse"));
            assert_eq!(
                parsed.unscaled(),
                &bi(*unscaled),
                "unscaled mismatch for {body:?}"
            );
            assert_eq!(parsed.scale(), *scale, "scale mismatch for {body:?}");
        }
        assert_eq!(BigDecVal::parse("1."), None, "Java rejects a trailing bare '.'");
        assert_eq!(BigDecVal::parse(""), None);
        assert_eq!(BigDecVal::parse("."), None);
        assert_eq!(BigDecVal::parse("+"), None);
        assert_eq!(BigDecVal::parse("e5"), None);
        assert_eq!(BigDecVal::parse("1.5.6"), None);
    }

    // --- BigDecimal toString ---------------------------------------------

    #[test]
    fn java_tostring_matches_oracle() {
        // (unscaled, scale, expected `str` -- the `M` suffix is the
        // printer's job, not this module's).
        let cases: &[(i64, i32, &str)] = &[
            (0, 2, "0.00"),
            (1, -3, "1E+3"),
            (10, 11, "1.0E-10"),
            (12300, 0, "12300"),
            (123, -3, "1.23E+5"),
            (1, 6, "0.000001"),
            (1, 7, "1E-7"),
            (15, 1, "1.5"),
            (-150, 2, "-1.50"),
            (100, 0, "100"),
            (0, 0, "0"),
        ];
        for (unscaled, scale, expected) in cases {
            let v = BigDecVal::new(bi(*unscaled), *scale);
            assert_eq!(
                v.to_java_string(),
                *expected,
                "unscaled={unscaled} scale={scale}"
            );
        }

        // The one row too large for an i64 literal: 123456789012345678901234567890.123M
        let unscaled: BigInt = "123456789012345678901234567890123"
            .parse()
            .expect("valid decimal");
        let v = BigDecVal::new(unscaled, 3);
        assert_eq!(v.to_java_string(), "123456789012345678901234567890.123");
    }

    // --- BigDecimal equality / hashing ------------------------------------

    #[test]
    fn bigdec_equality_ignores_scale() {
        let one_m = BigDecVal::new(bi(1), 0);
        let one_0_m = BigDecVal::new(bi(10), 1);
        let one_00_m = BigDecVal::new(bi(100), 2);
        assert_eq!(one_m, one_0_m, "1M should equal 1.0M");
        assert_eq!(one_0_m, one_00_m, "1.0M should equal 1.00M");
        assert_eq!(one_m, one_00_m, "1M should equal 1.00M");

        // Zero canonicalizes regardless of input scale (Java 9+, not the
        // Java 8 stripTrailingZeros zero bug).
        let zero_a = BigDecVal::new(bi(0), 2);
        let zero_b = BigDecVal::new(bi(0), 0);
        assert_eq!(zero_a, zero_b);
        assert_eq!(zero_a.canonical().scale(), 0);
        assert_eq!(zero_a.canonical().unscaled(), &bi(0));

        // Different numeric value must NOT be equal.
        let one_5 = BigDecVal::new(bi(15), 1);
        assert_ne!(one_m, one_5);
    }

    #[test]
    fn bigdec_hash_agrees_with_eq() {
        use std::collections::hash_map::DefaultHasher;
        fn hash_of(v: &BigDecVal) -> u64 {
            let mut h = DefaultHasher::new();
            v.hash(&mut h);
            h.finish()
        }

        let one_m = BigDecVal::new(bi(1), 0);
        let one_0_m = BigDecVal::new(bi(10), 1);
        let one_00_m = BigDecVal::new(bi(100), 2);
        assert_eq!(hash_of(&one_m), hash_of(&one_0_m));
        assert_eq!(hash_of(&one_0_m), hash_of(&one_00_m));
    }

    #[test]
    fn bigdec_ord_is_scale_insensitive() {
        let one_m = BigDecVal::new(bi(1), 0);
        let one_00_m = BigDecVal::new(bi(100), 2);
        let two_m = BigDecVal::new(bi(2), 0);
        assert_eq!(one_m.cmp(&one_00_m), Ordering::Equal);
        assert_eq!(one_m.cmp(&two_m), Ordering::Less);
        assert_eq!(two_m.cmp(&one_m), Ordering::Greater);
    }
}
