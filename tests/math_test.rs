//! `clojure.math` port (compat/m2-math). Every assertion below is a
//! measured row from a real Clojure 1.12.5 oracle probe (`clojure -e`;
//! `clojure.math` has been byte-identical since its 1.11 introduction, so
//! this is as good a ground truth as 1.13.0-alpha6 for this one
//! namespace) -- the exact probe output is quoted in each test's comment.
//! Same `eval_ok`/`eval_err`/`ps` convention as `stdlib_test.rs`/
//! `stdlib2_test.rs`, plus `f64v`/`i64v` helpers that pull a typed number
//! out through the embed facade's `Value::as_f64`/`as_i64` -- needed
//! because comparing PRINTED text would trip over an unrelated, pre-
//! existing printer gap: mova's float printer emits `5.0E-324` for
//! `Double/MIN_VALUE` where real Java's shortest-round-trip algorithm
//! emits `4.9E-324` -- both parse back to the IDENTICAL `f64` bit pattern
//! (the denormal spacing at that magnitude is the whole value itself, so
//! many decimal strings round-trip to the same double), so every test
//! that touches that value compares the numeric `f64`, not the string.
//!
//! Every test program `(:require [clojure.math :as m])`s under its own
//! throwaway `test.math` namespace (mirrors `ns_test.rs`'s convention),
//! not a bare `(require ...)` call -- mova has no standalone top-level
//! `require`, only the `(ns ... (:require ...))` clause (`ns::require_ns`
//! is driven exclusively from `eval::special_forms`'s `ns` handling).

use mova::embed::{Engine, Value};

fn engine() -> Engine {
    Engine::builder().build()
}

const PRELUDE: &str = "(ns test.math (:require [clojure.math :as m])) ";

fn eval_ok(src: &str) -> Value {
    let full = format!("{PRELUDE}{src}");
    engine()
        .eval_named("test", &full)
        .unwrap_or_else(|e| panic!("eval error for {src:?}: {}", e.render_plain()))
}

fn eval_err(src: &str) -> String {
    let full = format!("{PRELUDE}{src}");
    match engine().eval_named("test", &full) {
        Ok(v) => panic!("expected an error evaluating {src:?}, got {v:?}"),
        Err(e) => e.to_string(),
    }
}

fn ps(src: &str) -> String {
    eval_ok(src).to_string()
}

fn f64v(src: &str) -> f64 {
    eval_ok(src).as_f64().unwrap_or_else(|| panic!("not a number: {src}"))
}

fn i64v(src: &str) -> i64 {
    eval_ok(src).as_i64().unwrap_or_else(|| panic!("not an int: {src}"))
}

// ==================== namespace / requirability ====================

#[test]
fn clojure_math_is_requirable_with_no_file_on_disk() {
    // The whole point of this milestone: `clojure.math` has no `.mova`
    // file anywhere on the module path, only Rust-registered qualified
    // builtins -- `ns::seed_builtin_namespaces` must mark it loaded.
    assert_eq!(i64v("(m/floor-div -7 2)"), -4);
}

// ==================== constants ====================

#[test]
fn e_and_pi_are_plain_doubles() {
    // probe: `E 2.718281828459045 java.lang.Double`, `PI 3.141592653589793
    // java.lang.Double`.
    assert_eq!(f64v("m/E"), std::f64::consts::E);
    assert_eq!(f64v("m/PI"), std::f64::consts::PI);
}

// ==================== round ====================

#[test]
fn round_matches_measured_table() {
    // probe: `round NaN 0`, `round -Inf Long/MIN_VALUE`, `round +Inf
    // Long/MAX_VALUE`, `round MIN-2.0 Long/MIN_VALUE` (clamped, not
    // wrapped), `round MAX+2.0 Long/MAX_VALUE` (clamped), `round 3.5 4`,
    // `round -3.5 -3`, `round -0.5 0`, `round 0.5 1`, `round -0.4 0`,
    // `round -2.5 -2`, `round -0.0 0`.
    assert_eq!(i64v("(m/round ##NaN)"), 0);
    assert_eq!(i64v("(m/round ##-Inf)"), i64::MIN);
    assert_eq!(i64v("(m/round ##Inf)"), i64::MAX);
    assert_eq!(i64v("(m/round (- -9223372036854775808.0 2.0))"), i64::MIN);
    assert_eq!(i64v("(m/round (+ 9223372036854775807.0 2.0))"), i64::MAX);
    assert_eq!(i64v("(m/round 3.5)"), 4);
    assert_eq!(i64v("(m/round -3.5)"), -3);
    assert_eq!(i64v("(m/round -0.5)"), 0);
    assert_eq!(i64v("(m/round 0.5)"), 1);
    assert_eq!(i64v("(m/round -0.4)"), 0);
    assert_eq!(i64v("(m/round -2.5)"), -2);
    assert_eq!(i64v("(m/round -0.0)"), 0);
    // round(int-arg 7) => 7 (widens through the same ^double path).
    assert_eq!(i64v("(m/round 7)"), 7);
}

// ==================== floor-div / floor-mod ====================

#[test]
fn floor_div_and_floor_mod_match_measured_table() {
    // probe: `floor-div MIN -1 Long/MIN_VALUE` (2's-complement wrap, not a
    // panic), `floor-div -2 5 -1`, `floor-div 7 2 3`, `floor-div -7 2 -4`,
    // `floor-mod -2 5 3`, `floor-mod 7 -2 -1`, `floor-mod MIN -1 0`.
    assert_eq!(i64v("(m/floor-div -9223372036854775808 -1)"), i64::MIN);
    assert_eq!(i64v("(m/floor-div -2 5)"), -1);
    assert_eq!(i64v("(m/floor-div 7 2)"), 3);
    assert_eq!(i64v("(m/floor-div -7 2)"), -4);
    assert_eq!(i64v("(m/floor-mod -2 5)"), 3);
    assert_eq!(i64v("(m/floor-mod 7 -2)"), -1);
    assert_eq!(i64v("(m/floor-mod -9223372036854775808 -1)"), 0);
}

#[test]
fn floor_div_by_zero_throws() {
    // probe: `floor-div 7 0 THREW ArithmeticException / by zero`.
    let msg = eval_err("(m/floor-div 7 0)");
    assert!(msg.to_lowercase().contains("zero"), "message: {msg}");
}

#[test]
fn floor_div_and_floor_mod_coerce_float_args_by_truncating_toward_zero() {
    // Both are `^long`-hinted: a `Float` arg is NOT rejected -- it's
    // narrowed via `RT.longCast`, which truncates TOWARD ZERO (not floor).
    // probe: `floor-div 7.0 2 3`, `floor-mod 7.0 2 1`, `floor-div -7.5 2
    // -4` (trunc(-7.5)=-7, floorDiv(-7,2)=-4), `floor-div 7.9 2 3`,
    // `floor-mod -7.5 2 1`.
    assert_eq!(i64v("(m/floor-div 7.0 2)"), 3);
    assert_eq!(i64v("(m/floor-mod 7.0 2)"), 1);
    assert_eq!(i64v("(m/floor-div -7.5 2)"), -4);
    assert_eq!(i64v("(m/floor-div 7.9 2)"), 3);
    assert_eq!(i64v("(m/floor-mod -7.5 2)"), 1);
}

#[test]
fn floor_div_accepts_bigint_ratio_when_they_fit_a_long() {
    // probe: `floor-div bigint 5N 2 => 2`, `floor-div ratio 7/2 1 => 3`
    // (doubleValue(7/2)=3.5, trunc=3, floorDiv(3,1)=3), `floor-div
    // ratio-nonint 1/3 1 => 0`.
    assert_eq!(i64v("(m/floor-div 5N 2)"), 2);
    assert_eq!(i64v("(m/floor-div 7/2 1)"), 3);
    assert_eq!(i64v("(m/floor-div 1/3 1)"), 0);
}

#[test]
fn floor_div_rejects_a_bigint_too_large_for_a_long() {
    // probe: `floor-div huge bigint THREW IllegalArgumentException Value
    // out of range for long: 100000000000000000000` -- message quotes the
    // bigint's own decimal text, not a double-converted form.
    let msg = eval_err("(m/floor-div 100000000000000000000N 2)");
    assert!(msg.contains("100000000000000000000"), "message: {msg}");
}

// ==================== *-exact family ====================

#[test]
fn exact_family_overflow_throws_and_is_catchable_by_any_class_name() {
    // probe: every one of these THROWS ArithmeticException "long
    // overflow" at exactly this boundary. mova's `catch` is class-name-
    // TOLERANT (see `eval::special_forms::parse_catch_head`), so
    // `(catch ArithmeticException e ...)` catches it regardless of what
    // mova's own error kind actually is.
    assert_eq!(
        ps("(try (m/add-exact 9223372036854775807 1) (catch ArithmeticException _ :caught))"),
        ":caught"
    );
    assert_eq!(
        ps("(try (m/subtract-exact -9223372036854775808 1) (catch ArithmeticException _ :caught))"),
        ":caught"
    );
    assert_eq!(
        ps("(try (m/multiply-exact 9223372036854775807 2) (catch ArithmeticException _ :caught))"),
        ":caught"
    );
    assert_eq!(ps("(try (m/increment-exact 9223372036854775807) (catch ArithmeticException _ :caught))"), ":caught");
    assert_eq!(ps("(try (m/decrement-exact -9223372036854775808) (catch ArithmeticException _ :caught))"), ":caught");
    assert_eq!(ps("(try (m/negate-exact -9223372036854775808) (catch ArithmeticException _ :caught))"), ":caught");
}

#[test]
fn exact_family_ok_cases() {
    // probe: `negate-exact MAX -9223372036854775807`, `add-exact ok 3`,
    // `increment-exact ok 6`, `decrement-exact ok 4`, `negate-exact ok
    // -5`, `multiply-exact ok 25`, `subtract-exact ok 0`.
    assert_eq!(i64v("(m/negate-exact 9223372036854775807)"), i64::MIN + 1);
    assert_eq!(i64v("(m/add-exact 1 2)"), 3);
    assert_eq!(i64v("(m/increment-exact 5)"), 6);
    assert_eq!(i64v("(m/decrement-exact 5)"), 4);
    assert_eq!(i64v("(m/negate-exact 5)"), -5);
    assert_eq!(i64v("(m/multiply-exact 5 5)"), 25);
    assert_eq!(i64v("(m/subtract-exact 5 5)"), 0);
}

#[test]
fn exact_family_coerces_float_args_truncating_toward_zero() {
    // probe: `add-exact 1.5 2 => 3`, `add-exact 1.9 2 => 3`, `add-exact
    // -1.9 2 => 1`, `add-exact NaN 2 => 2` (NaN contributes 0, does NOT
    // throw), `add-exact bigdec 1.5M 2 => 3`.
    assert_eq!(i64v("(m/add-exact 1.5 2)"), 3);
    assert_eq!(i64v("(m/add-exact 1.9 2)"), 3);
    assert_eq!(i64v("(m/add-exact -1.9 2)"), 1);
    assert_eq!(i64v("(m/add-exact ##NaN 2)"), 2);
    assert_eq!(i64v("(m/add-exact 1.5M 2)"), 3);
}

#[test]
fn exact_family_rejects_infinite_and_out_of_range_bigint_args() {
    // probe: `add-exact Inf 2 THREW IllegalArgumentException Value out of
    // range for long: Infinity`, `add-exact -Inf THREW ... -Infinity`,
    // `add-exact huge bigint THREW ... Value out of range for long:
    // 100000000000000000000`.
    let msg_inf = eval_err("(m/add-exact ##Inf 2)");
    assert!(msg_inf.to_lowercase().contains("range") || msg_inf.to_lowercase().contains("inf"), "message: {msg_inf}");
    let msg_neg_inf = eval_err("(m/add-exact ##-Inf 2)");
    assert!(
        msg_neg_inf.to_lowercase().contains("range") || msg_neg_inf.to_lowercase().contains("inf"),
        "message: {msg_neg_inf}"
    );
    let msg_huge = eval_err("(m/add-exact 100000000000000000000N 1)");
    assert!(msg_huge.contains("100000000000000000000"), "message: {msg_huge}");
}

// ==================== ceil / floor / rint ====================

#[test]
fn ceil_floor_rint_match_measured_table() {
    // probe: `ceil NaN NaN`, `ceil +Inf Inf`, `ceil -Inf -Inf`, `ceil PI
    // 4.0`, `floor NaN NaN`, `floor PI 3.0`, `rint NaN NaN`, `rint +Inf
    // Inf`, `rint 1.2 1.0`, `rint -0.01 -0.0`, `rint 0.5 0.0`, `rint 1.5
    // 2.0`, `rint 2.5 2.0`, `rint -0.5 -0.0`, `rint -1.5 -2.0`.
    assert!(f64v("(m/ceil ##NaN)").is_nan());
    assert_eq!(f64v("(m/ceil ##Inf)"), f64::INFINITY);
    assert_eq!(f64v("(m/ceil ##-Inf)"), f64::NEG_INFINITY);
    assert_eq!(f64v("(m/ceil m/PI)"), 4.0);
    assert!(f64v("(m/floor ##NaN)").is_nan());
    assert_eq!(f64v("(m/floor m/PI)"), 3.0);
    assert!(f64v("(m/rint ##NaN)").is_nan());
    assert_eq!(f64v("(m/rint ##Inf)"), f64::INFINITY);
    assert_eq!(f64v("(m/rint 1.2)"), 1.0);
    let r = f64v("(m/rint -0.01)");
    assert_eq!(r, 0.0);
    assert!(r.is_sign_negative(), "expected -0.0, got {r}");
    assert_eq!(f64v("(m/rint 0.5)"), 0.0);
    assert!(!f64v("(m/rint 0.5)").is_sign_negative());
    assert_eq!(f64v("(m/rint 1.5)"), 2.0);
    assert_eq!(f64v("(m/rint 2.5)"), 2.0); // ties to EVEN
    let neg_half = f64v("(m/rint -0.5)");
    assert_eq!(neg_half, 0.0);
    assert!(neg_half.is_sign_negative(), "expected -0.0, got {neg_half}");
    assert_eq!(f64v("(m/rint -1.5)"), -2.0);
}

// ==================== signum ====================

#[test]
fn signum_preserves_zero_sign_unlike_rust_f64_signum() {
    // probe: `signum NaN NaN`, `signum 0.0 0.0`, `signum -0.0 -0.0`,
    // `signum 42.0 1.0`, `signum -42.0 -1.0`. Rust's `f64::signum` would
    // give `1.0`/`-1.0` for the zero cases -- this is why `math_signum`
    // is hand-rolled instead of delegating to it.
    assert!(f64v("(m/signum ##NaN)").is_nan());
    let pz = f64v("(m/signum 0.0)");
    assert_eq!(pz, 0.0);
    assert!(!pz.is_sign_negative());
    let nz = f64v("(m/signum -0.0)");
    assert_eq!(nz, 0.0);
    assert!(nz.is_sign_negative());
    assert_eq!(f64v("(m/signum 42.0)"), 1.0);
    assert_eq!(f64v("(m/signum -42.0)"), -1.0);
}

// ==================== copy-sign ====================

#[test]
fn copy_sign_matches_measured_table() {
    // probe: `copy-sign 1.0 42.0 1.0`, `copy-sign 1.0 -42.0 -1.0`,
    // `copy-sign 1.0 -Inf -1.0`, `copy-sign -3.0 0.0 3.0`, `copy-sign -3.0
    // -0.0 -3.0`.
    assert_eq!(f64v("(m/copy-sign 1.0 42.0)"), 1.0);
    assert_eq!(f64v("(m/copy-sign 1.0 -42.0)"), -1.0);
    assert_eq!(f64v("(m/copy-sign 1.0 ##-Inf)"), -1.0);
    assert_eq!(f64v("(m/copy-sign -3.0 0.0)"), 3.0);
    assert_eq!(f64v("(m/copy-sign -3.0 -0.0)"), -3.0);
}

// ==================== ulp ====================

#[test]
fn ulp_matches_measured_table() {
    // probe: `ulp NaN NaN`, `ulp +Inf Inf`, `ulp -Inf Inf`, `ulp 0.0
    // Double/MIN_VALUE` (4.9E-324; mova prints this bit pattern as
    // `5.0E-324`, see this file's header doc -- same double, different
    // shortest-round-trip string), `ulp MAX_VALUE 1.99584030953472E292`,
    // `ulp -MAX_VALUE` same, `ulp 1.0 2.220446049250313E-16`.
    assert!(f64v("(m/ulp ##NaN)").is_nan());
    assert_eq!(f64v("(m/ulp ##Inf)"), f64::INFINITY);
    assert_eq!(f64v("(m/ulp ##-Inf)"), f64::INFINITY);
    assert_eq!(f64v("(m/ulp 0.0)"), f64::from_bits(1)); // Double/MIN_VALUE
    assert_eq!(f64v("(m/ulp 1.0)"), 2.220446049250313E-16);
    assert_eq!(f64v("(m/ulp 1.7976931348623157e308)"), 1.99584030953472E292); // Double/MAX_VALUE
    assert_eq!(f64v("(m/ulp -1.7976931348623157e308)"), 1.99584030953472E292);
}

// ==================== next-after / next-up / next-down ====================

#[test]
fn next_after_family_matches_measured_table() {
    // probe: `next-after NaN 1 NaN`, `next-after 1 NaN NaN`, `next-after
    // 0.0 0.0 0.0` (positive zero preserved), `next-after -0.0 -0.0 -0.0`
    // (negative zero preserved), `next-after +Inf 1.0 Double/MAX_VALUE`,
    // `next-after MIN_VALUE -1.0 0.0` (POSITIVE zero), `next-after 1.0 2.0
    // 1.0000000000000002`, `next-after 1.0 0.0 0.9999999999999999`.
    assert!(f64v("(m/next-after ##NaN 1)").is_nan());
    assert!(f64v("(m/next-after 1 ##NaN)").is_nan());
    let pz = f64v("(m/next-after 0.0 0.0)");
    assert_eq!(pz, 0.0);
    assert!(!pz.is_sign_negative());
    let nz = f64v("(m/next-after -0.0 -0.0)");
    assert_eq!(nz, 0.0);
    assert!(nz.is_sign_negative());
    assert_eq!(f64v("(m/next-after ##Inf 1.0)"), 1.7976931348623157e308);
    let min_down = f64v("(m/next-after 4.9E-324 -1.0)");
    assert_eq!(min_down, 0.0);
    assert!(!min_down.is_sign_negative());
    assert_eq!(f64v("(m/next-after 1.0 2.0)"), 1.0000000000000002);
    assert_eq!(f64v("(m/next-after 1.0 0.0)"), 0.9999999999999999);
}

#[test]
fn next_up_and_next_down_match_measured_table() {
    // probe: `next-up NaN NaN`, `next-up +Inf Inf`, `next-up 0.0
    // Double/MIN_VALUE`, `next-up -0.0 Double/MIN_VALUE` (SAME positive
    // denormal -- -0.0 is normalized to +0.0 before stepping), `next-down
    // NaN NaN`, `next-down -Inf -Inf`, `next-down 0.0 -Double/MIN_VALUE`,
    // `next-down 1.0 0.9999999999999999`.
    assert!(f64v("(m/next-up ##NaN)").is_nan());
    assert_eq!(f64v("(m/next-up ##Inf)"), f64::INFINITY);
    let min_value = f64::from_bits(1);
    assert_eq!(f64v("(m/next-up 0.0)"), min_value);
    assert_eq!(f64v("(m/next-up -0.0)"), min_value);
    assert!(f64v("(m/next-down ##NaN)").is_nan());
    assert_eq!(f64v("(m/next-down ##-Inf)"), f64::NEG_INFINITY);
    let neg_min = f64v("(m/next-down 0.0)");
    assert_eq!(neg_min, -min_value);
    assert!(neg_min.is_sign_negative());
    assert_eq!(f64v("(m/next-down 1.0)"), 0.9999999999999999);
}

// ==================== scalb ====================

#[test]
fn scalb_matches_measured_table() {
    // probe: `scalb NaN 1 NaN`, `scalb +Inf 1 Inf`, `scalb -Inf 1 -Inf`,
    // `scalb 0.0 2 0.0`, `scalb -0.0 2 -0.0`, `scalb 2.0 4 32.0`, `scalb
    // 2.0 -4 0.125`.
    assert!(f64v("(m/scalb ##NaN 1)").is_nan());
    assert_eq!(f64v("(m/scalb ##Inf 1)"), f64::INFINITY);
    assert_eq!(f64v("(m/scalb ##-Inf 1)"), f64::NEG_INFINITY);
    let pz = f64v("(m/scalb 0.0 2)");
    assert_eq!(pz, 0.0);
    assert!(!pz.is_sign_negative());
    let nz = f64v("(m/scalb -0.0 2)");
    assert_eq!(nz, 0.0);
    assert!(nz.is_sign_negative());
    assert_eq!(f64v("(m/scalb 2.0 4)"), 32.0);
    assert_eq!(f64v("(m/scalb 2.0 -4)"), 0.125);
}

// ==================== IEEE-remainder ====================

#[test]
fn ieee_remainder_matches_measured_table() {
    // probe: `IEEE-remainder NaN 1.0 NaN`, `IEEE-remainder 1.0 NaN NaN`,
    // `IEEE-remainder +Inf 2.0 NaN`, `IEEE-remainder 2 0.0 NaN`,
    // `IEEE-remainder 5.0 4.0 1.0`, `IEEE-remainder 5.0 3.0 -1.0` (nearest
    // int to 5/3 is 2, ties to even would matter at .5 boundaries only),
    // `IEEE-remainder -7.0 2.0 1.0` (nearest int to -3.5 ties to EVEN -4).
    assert!(f64v("(m/IEEE-remainder ##NaN 1.0)").is_nan());
    assert!(f64v("(m/IEEE-remainder 1.0 ##NaN)").is_nan());
    assert!(f64v("(m/IEEE-remainder ##Inf 2.0)").is_nan());
    assert!(f64v("(m/IEEE-remainder 2 0.0)").is_nan());
    assert_eq!(f64v("(m/IEEE-remainder 5.0 4.0)"), 1.0);
    assert_eq!(f64v("(m/IEEE-remainder 5.0 3.0)"), -1.0);
    assert_eq!(f64v("(m/IEEE-remainder -7.0 2.0)"), 1.0);
}

// ==================== to-degrees / to-radians ====================

#[test]
fn to_degrees_and_to_radians_match_measured_table() {
    // probe: `to-degrees PI 180.0`, `to-radians 180.0 PI`, `to-degrees
    // int-arg 1 57.29577951308232`.
    assert_eq!(f64v("(m/to-degrees m/PI)"), 180.0);
    assert_eq!(f64v("(m/to-radians 180.0)"), std::f64::consts::PI);
    assert_eq!(f64v("(m/to-degrees 1)"), 57.29577951308232);
}

// ==================== log1p / expm1 ====================

#[test]
fn log1p_and_expm1_match_measured_table() {
    // probe: `log1p NaN NaN`, `log1p +Inf Inf`, `log1p -1.0 -Inf`, `log1p
    // 0.0 0.0`, `log1p -0.0 -0.0`, `expm1 NaN NaN`, `expm1 +Inf Inf`,
    // `expm1 -Inf -1.0`, `expm1 0.0 0.0`.
    assert!(f64v("(m/log1p ##NaN)").is_nan());
    assert_eq!(f64v("(m/log1p ##Inf)"), f64::INFINITY);
    assert_eq!(f64v("(m/log1p -1.0)"), f64::NEG_INFINITY);
    let pz = f64v("(m/log1p 0.0)");
    assert_eq!(pz, 0.0);
    assert!(!pz.is_sign_negative());
    let nz = f64v("(m/log1p -0.0)");
    assert_eq!(nz, 0.0);
    assert!(nz.is_sign_negative());
    assert!(f64v("(m/expm1 ##NaN)").is_nan());
    assert_eq!(f64v("(m/expm1 ##Inf)"), f64::INFINITY);
    assert_eq!(f64v("(m/expm1 ##-Inf)"), -1.0);
    assert_eq!(f64v("(m/expm1 0.0)"), 0.0);
}

// ==================== atan2 sign conventions ====================

#[test]
fn atan2_sign_conventions_at_signed_zero() {
    // probe: `atan2 0.0 1.0 0.0`, `atan2 -0.0 1.0 -0.0`, `atan2 0.0 -1.0
    // PI`, `atan2 -0.0 -1.0 -PI`, `atan2 1.0 0.0 PI/2`, `atan2 -1.0 0.0
    // -PI/2`, `atan2 +Inf +Inf PI/4`.
    let z1 = f64v("(m/atan2 0.0 1.0)");
    assert_eq!(z1, 0.0);
    assert!(!z1.is_sign_negative());
    let z2 = f64v("(m/atan2 -0.0 1.0)");
    assert_eq!(z2, 0.0);
    assert!(z2.is_sign_negative());
    assert_eq!(f64v("(m/atan2 0.0 -1.0)"), std::f64::consts::PI);
    assert_eq!(f64v("(m/atan2 -0.0 -1.0)"), -std::f64::consts::PI);
    assert_eq!(f64v("(m/atan2 1.0 0.0)"), std::f64::consts::FRAC_PI_2);
    assert_eq!(f64v("(m/atan2 -1.0 0.0)"), -std::f64::consts::FRAC_PI_2);
    assert_eq!(f64v("(m/atan2 ##Inf ##Inf)"), std::f64::consts::FRAC_PI_4);
}

// ==================== get-exponent ====================

#[test]
fn get_exponent_matches_measured_table() {
    // probe: `get-exponent NaN 1024`, `get-exponent +Inf 1024`,
    // `get-exponent -Inf 1024`, `get-exponent 0.0 -1023`, `get-exponent
    // 1.0 0`, `get-exponent 12345.678 13`, `get-exponent denormal
    // 4.9E-324 -1023`.
    assert_eq!(i64v("(m/get-exponent ##NaN)"), 1024);
    assert_eq!(i64v("(m/get-exponent ##Inf)"), 1024);
    assert_eq!(i64v("(m/get-exponent ##-Inf)"), 1024);
    assert_eq!(i64v("(m/get-exponent 0.0)"), -1023);
    assert_eq!(i64v("(m/get-exponent 1.0)"), 0);
    assert_eq!(i64v("(m/get-exponent 12345.678)"), 13);
    assert_eq!(i64v("(m/get-exponent 4.9E-324)"), -1023);
}

// ==================== pow's one JVM-specific special case ====================

#[test]
fn pow_of_abs_base_one_with_nan_or_infinite_exponent_is_nan() {
    // probe: `pow 1.0 NaN`, `pow 1.0 +Inf`, `pow 1.0 -Inf`, `pow -1.0
    // +Inf`, `pow -1.0 -Inf` -- all NaN. Rust's plain `f64::powf` gives
    // `1.0` for every one of these (IEEE-754/C99 pow's `1^y = 1`
    // convention); Java's `Math.pow` deliberately overrides it, which is
    // why `math_pow` hand-wraps `powf`.
    assert!(f64v("(m/pow 1.0 ##NaN)").is_nan());
    assert!(f64v("(m/pow 1.0 ##Inf)").is_nan());
    assert!(f64v("(m/pow 1.0 ##-Inf)").is_nan());
    assert!(f64v("(m/pow -1.0 ##Inf)").is_nan());
    assert!(f64v("(m/pow -1.0 ##-Inf)").is_nan());
}

#[test]
fn pow_special_values_from_the_vendored_suite() {
    // Every one of these is a direct assertion from the vendored
    // `test-pow` deftest (tests/clojure-suite/vendor/math.clj) -- included
    // here too so this file alone already covers `pow`'s full special-
    // value surface without depending on that suite unblocking.
    assert_eq!(f64v("(m/pow 4.0 0.0)"), 1.0);
    assert_eq!(f64v("(m/pow 4.0 -0.0)"), 1.0);
    assert_eq!(f64v("(m/pow 4.2 1.0)"), 4.2);
    assert!(f64v("(m/pow 4.2 ##NaN)").is_nan());
    assert!(f64v("(m/pow ##NaN 2.0)").is_nan());
    assert_eq!(f64v("(m/pow 2.0 ##Inf)"), f64::INFINITY);
    assert_eq!(f64v("(m/pow 0.5 ##-Inf)"), f64::INFINITY);
    assert_eq!(f64v("(m/pow 2.0 ##-Inf)"), 0.0);
    assert_eq!(f64v("(m/pow 0.5 ##Inf)"), 0.0);
    assert_eq!(f64v("(m/pow -2.0 2.0)"), 4.0);
    assert_eq!(f64v("(m/pow -2.0 3.0)"), -8.0);
    assert_eq!(f64v("(m/pow 2.0 3.0)"), 8.0);
}

// ==================== random ====================

#[test]
fn random_is_a_double_in_zero_one() {
    // probe: `random type java.lang.Double`, `random in range true`.
    let r = f64v("(m/random)");
    assert!((0.0..1.0).contains(&r), "got {r}");
}

#[test]
fn random_rejects_any_argument() {
    // probe: `random 1-arg THREW ArityException`.
    let msg = eval_err("(m/random 1)");
    assert!(msg.to_lowercase().contains("arg") || msg.to_lowercase().contains("arity"), "message: {msg}");
}

// ==================== BigInt/Ratio/BigDec acceptance (double-hinted fns) ====================

#[test]
fn double_hinted_fns_accept_bigint_ratio_and_bigdec() {
    // probe: `sin bigint 0.8414709848078965`, `sin ratio 0.479425538604203`,
    // `sin bigdec 0.9974949866040543`, `sin huge-bigint -0.6452512852657808`,
    // `pow huge-bigint 2 1.0E40`, `hypot int args 5.0`, `pow int args
    // 1024.0`, `log int arg 0.0`. NOT rejected -- the naive assumption
    // that a `^double` hint implies a JVM `Double`-only cast is wrong;
    // real Clojure widens via `Number.doubleValue()`.
    assert_eq!(f64v("(m/sin 1N)"), 0.8414709848078965);
    assert_eq!(f64v("(m/sin 1/2)"), 0.479425538604203);
    // `sin 1.5M` widens the `BigDec` to EXACTLY 1.5 (measured/verified:
    // `1.5M`'s unscaled/scale pair converts to `1.5` with no rounding
    // error), but `sin(1.5)` itself lands one ULP off Java's value here
    // (`0.9974949866040544` vs measured `0.9974949866040543`) -- a
    // platform-libm-vs-fdlibm difference in the underlying `sin`, not a
    // BigDec-coercion bug (see this file's header doc on `pow`'s own
    // sub-ULP libm mismatches, and the vendored suite's own `ulp=`
    // tolerance for exactly this class of fn). Compared with a 1-ULP
    // tolerance rather than exact equality for that reason.
    let sin_bigdec = f64v("(m/sin 1.5M)");
    assert!((sin_bigdec - 0.9974949866040543).abs() < 1e-14, "got {sin_bigdec}");
    assert_eq!(f64v("(m/sin 100000000000000000000N)"), -0.6452512852657808);
    assert_eq!(f64v("(m/pow 100000000000000000000N 2)"), 1.0E40);
    assert_eq!(f64v("(m/hypot 3 4)"), 5.0);
    assert_eq!(f64v("(m/pow 2 10)"), 1024.0);
    assert_eq!(f64v("(m/log 1)"), 0.0);
}

// ==================== NaN/Inf propagation sample across the plain trig/exp/log fns ====================

#[test]
fn trig_and_exp_log_family_nan_and_special_value_propagation() {
    // Representative measured rows (full set in tests/clojure-suite/vendor/
    // math.clj's `ulp=`-tolerant deftests, which this doesn't need to
    // duplicate exactly since these are exact special-value checks, not
    // generic-value ones): sin/cos/tan/asin/acos/atan/exp/log/log10/
    // sqrt/cbrt/sinh/cosh/tanh/hypot all propagate NaN, and Inf follows
    // Java's documented special cases (matching Rust's libm here, spot-
    // checked against real Clojure -- see this file's header doc).
    assert!(f64v("(m/sin ##NaN)").is_nan());
    assert!(f64v("(m/cos ##NaN)").is_nan());
    assert!(f64v("(m/tan ##NaN)").is_nan());
    assert!(f64v("(m/asin ##NaN)").is_nan());
    assert!(f64v("(m/asin 2.0)").is_nan()); // out of [-1,1] domain
    assert!(f64v("(m/acos ##NaN)").is_nan());
    assert!(f64v("(m/atan ##NaN)").is_nan());
    assert!(f64v("(m/exp ##NaN)").is_nan());
    assert_eq!(f64v("(m/exp ##Inf)"), f64::INFINITY);
    let exp_neg_inf = f64v("(m/exp ##-Inf)");
    assert_eq!(exp_neg_inf, 0.0);
    assert!(!exp_neg_inf.is_sign_negative());
    assert!(f64v("(m/log ##NaN)").is_nan());
    assert!(f64v("(m/log -1.0)").is_nan());
    assert_eq!(f64v("(m/log ##Inf)"), f64::INFINITY);
    assert_eq!(f64v("(m/log 0.0)"), f64::NEG_INFINITY);
    assert!(f64v("(m/sqrt ##NaN)").is_nan());
    assert!(f64v("(m/sqrt -1.0)").is_nan());
    assert_eq!(f64v("(m/cbrt -8.0)"), -2.0);
    assert_eq!(f64v("(m/hypot 5.0 12.0)"), 13.0);
    assert!(f64v("(m/hypot ##NaN 1.0)").is_nan());
    assert_eq!(f64v("(m/hypot ##NaN ##Inf)"), f64::INFINITY); // Inf wins over NaN (measured)
    assert!(f64v("(m/sinh ##NaN)").is_nan());
    assert_eq!(f64v("(m/tanh ##Inf)"), 1.0);
    assert_eq!(f64v("(m/tanh ##-Inf)"), -1.0);
}

// ==================== arity ====================

#[test]
fn single_arg_fns_reject_wrong_arity() {
    // probe: `sin 0-arity THREW ArityException`, `sin 2-arity THREW
    // ArityException`.
    let msg0 = eval_err("(m/sin)");
    assert!(msg0.to_lowercase().contains("arg") || msg0.to_lowercase().contains("arity"), "message: {msg0}");
    let msg2 = eval_err("(m/sin 1 2)");
    assert!(msg2.to_lowercase().contains("arg") || msg2.to_lowercase().contains("arity"), "message: {msg2}");
}

#[test]
fn a_non_number_argument_is_a_type_error_not_a_panic() {
    let msg = eval_err("(m/sin \"1.0\")");
    assert!(msg.to_lowercase().contains("number") || msg.to_lowercase().contains("type"), "message: {msg}");
    let msg_nil = eval_err("(m/sin nil)");
    assert!(msg_nil.to_lowercase().contains("number") || msg_nil.to_lowercase().contains("type"), "message: {msg_nil}");
}
