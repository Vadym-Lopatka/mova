//! SPEC-W6a: the arithmetic core of `clojure.test.check.random` --
//! Gary Fredericks' immutable port of Java 8's
//! `java.util.SplittableRandom` (splitmix64).
//!
//! # Why this is Rust and not the vendored `.clj`
//!
//! `tests/clojure-suite/vendor-libs/clojure/test/check/random.clj` is a
//! `deftype` with two `^long` fields and ~30 unchecked 64-bit mixing ops
//! per `split`, reached through an `IRandom` protocol dispatch.
//! `clojure.test.check` calls `split` ONCE PER GENERATED ELEMENT, so
//! that file is the whole per-element cost of generative testing.
//! Measured on the SPEC-W4 binary (`spec/lazy-concat`@2117713):
//! 10 000 `split` + `rand-long` = **1.90 s**, i.e. 0.19 ms each, against
//! a JVM that does the same work in the tens of nanoseconds. It is also
//! the visible face of the ledgered `^long`-parameter-hint gap
//! (`docs/SPEC-PORT-PATCHES.md` section D item 9): mova does not honour
//! primitive hints, so every one of those ops is boxed generic
//! arithmetic dispatched through the numeric tower.
//!
//! Rewriting it as a native builtin is the approved mova veneer pattern
//! (`clojure.string`, `clojure.math`): natives registered under a
//! `clojure.*` namespace prefix, which `ns::seed_builtin_namespaces`
//! then marks as already-loaded so `require` of the namespace is a
//! no-op. See `crate::builtins::tcrandom` for the wiring.
//!
//! # Bit-exactness is the contract
//!
//! This is NOT "a splitmix64" -- it is *that* splitmix64. Seeded
//! generation (`(gen/generate g size seed)`, `(quick-check n prop :seed
//! s)`, `(s/exercise …)` under a fixed seed) must produce byte-identical
//! output to the JVM, or every seeded corpus, golden and bug report
//! silently diverges. Every constant and every shift below is
//! transcribed from the vendored `random.clj` -- the `longify` macro's
//! job (turning `0x9e3779b97f4a7c15`, which Clojure reads as a bigint,
//! into the `long` with those bits) is just Rust's `as i64` here.
//!
//! The transcription is pinned by [`tests`] below, whose expected values
//! came from the ORACLE: real `org.clojure/test.check` on the JVM,
//! printing `(.-gamma r)`/`(.-state r)`/`rand-long`/`rand-double` for
//! eight seeds plus three thousand-deep split chains. Verified identical
//! at test.check **1.1.1 and 1.1.3** (the version
//! `tests/clojure-suite/MANIFEST-LIBS.sha256` pins), so the goldens are
//! not version-sensitive.

/// One `clojure.test.check.random/JavaUtilSplittableRandom` -- the pair
/// of `^long` fields the `deftype` carries, and nothing else.
///
/// `Copy` and 16 bytes, so it rides INLINE in [`crate::value::Value`]
/// (which stays 32 bytes wide -- pinned by
/// `size_of_value_unchanged_by_bignum_variants`) with no `Arc`, no
/// allocation and no refcount traffic on the hottest path in generative
/// testing. That is the whole reason for a bespoke variant rather than,
/// say, an `Arc`-boxed `HostInst` cell: `split` produces TWO of these per
/// generated element, and an allocation apiece would be the only cost
/// left worth measuring.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SplitRandom {
    pub gamma: i64,
    pub state: i64,
}

/// `(longify 0x9e3779b97f4a7c15)` -- upstream's `golden-gamma`, the
/// gamma every seeded RNG starts with.
pub const GOLDEN_GAMMA: i64 = 0x9e37_79b9_7f4a_7c15_u64 as i64;

/// `double-unit`: `(/ 1.0 (double (bit-set 0 53)))` = 2^-53. Exactly
/// representable, so the multiply below is exact for every input (which
/// is < 2^53 by construction).
const DOUBLE_UNIT: f64 = 1.0 / ((1u64 << 53) as f64);

/// `(bxoubsr x n)`: `(-> x (unsigned-bit-shift-right n) (bit-xor x))`.
///
/// The shift is UNSIGNED -- Clojure's `unsigned-bit-shift-right`, i.e.
/// Rust's `>>` on `u64`, not on `i64`.
#[inline(always)]
fn bxoubsr(x: i64, n: u32) -> i64 {
    (((x as u64) >> n) as i64) ^ x
}

/// `(mix-64 n)` -- upstream's `mix-64` macro, verbatim.
#[inline(always)]
pub fn mix64(n: i64) -> i64 {
    let z = bxoubsr(n, 30);
    let z = z.wrapping_mul(0xbf58_476d_1ce4_e5b9_u64 as i64);
    let z = bxoubsr(z, 27);
    let z = z.wrapping_mul(0x94d0_49bb_1331_11eb_u64 as i64);
    bxoubsr(z, 31)
}

/// `(mix-gamma n)` -- upstream's `mix-gamma` macro, verbatim, including
/// the final `cond->`: if `(Long/bitCount (bxoubsr z 1))` is under 24,
/// xor with `0xaaaaaaaaaaaaaaaa`. (`Long/bitCount` counts bits in the
/// two's-complement pattern, which is what `i64::count_ones` does.)
#[inline(always)]
pub fn mix_gamma(n: i64) -> i64 {
    let z = bxoubsr(n, 33);
    let z = z.wrapping_mul(0xff51_afd7_ed55_8ccd_u64 as i64);
    let z = bxoubsr(z, 33);
    let z = z.wrapping_mul(0xc4ce_b9fe_1a85_ec53_u64 as i64);
    let z = bxoubsr(z, 33);
    let z = z | 1;
    if bxoubsr(z, 1).count_ones() < 24 {
        z ^ (0xaaaa_aaaa_aaaa_aaaa_u64 as i64)
    } else {
        z
    }
}

impl SplitRandom {
    /// `(make-java-util-splittable-random seed)`.
    #[inline]
    pub fn from_seed(seed: i64) -> Self {
        SplitRandom { gamma: GOLDEN_GAMMA, state: seed }
    }

    /// `(rand-long [_] (-> state (+ gamma) (mix-64)))`. The `+` is under
    /// `(set! *unchecked-math* :warn-on-boxed)`, i.e. wrapping.
    #[inline]
    pub fn rand_long(&self) -> i64 {
        mix64(self.state.wrapping_add(self.gamma))
    }

    /// `(rand-double [this] (* double-unit (unsigned-bit-shift-right
    /// (long (rand-long this)) 11)))` -- a value in `[0.0, 1.0)`.
    #[inline]
    pub fn rand_double(&self) -> f64 {
        (((self.rand_long() as u64) >> 11) as f64) * DOUBLE_UNIT
    }

    /// `(split [this])` -> `[rng1 rng2]`.
    #[inline]
    pub fn split(&self) -> (SplitRandom, SplitRandom) {
        let state1 = self.gamma.wrapping_add(self.state);
        let state2 = self.gamma.wrapping_add(state1);
        let gamma1 = mix_gamma(state2);
        (
            SplitRandom { gamma: self.gamma, state: state2 },
            SplitRandom { gamma: gamma1, state: mix64(state1) },
        )
    }

    /// `(split-n [this n])` -- upstream's own loop, which imitates a
    /// particular series of 2-way splits without the intermediate
    /// allocation. `n == 0` -> empty, `n == 1` -> `[this]`.
    ///
    /// Negative `n` is the one place this deliberately does NOT match the
    /// JVM: upstream's `(if (= n-dec (count ret)) …)` can never terminate
    /// when `n-dec` is negative, so real test.check spins until it runs
    /// out of memory. Callers get [`None`] here and the builtin turns it
    /// into an ordinary error -- see `crate::builtins::tcrandom`.
    pub fn split_n(&self, n: i64) -> Option<Vec<SplitRandom>> {
        if n < 0 {
            return None;
        }
        if n == 0 {
            return Some(Vec::new());
        }
        if n == 1 {
            return Some(vec![*self]);
        }
        let n_dec = (n - 1) as usize;
        let mut out: Vec<SplitRandom> = Vec::with_capacity(n as usize);
        let mut state = self.state;
        while out.len() != n_dec {
            let state1 = self.gamma.wrapping_add(state);
            let state2 = self.gamma.wrapping_add(state1);
            let gamma1 = mix_gamma(state2);
            out.push(SplitRandom { gamma: gamma1, state: mix64(state1) });
            state = state2;
        }
        out.push(SplitRandom { gamma: self.gamma, state });
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every number below was PRINTED BY THE JVM, not derived here: the
    /// probe is `(require '[clojure.test.check.random :as r])` plus
    /// `(.-gamma rng)` / `(.-state rng)` / `(r/rand-long rng)` /
    /// `(Double/doubleToRawLongBits (r/rand-double rng))`, run under
    /// `clojure -Sdeps '{:deps {org.clojure/test.check {:mvn/version
    /// "1.1.3"}}}'` (and re-run at 1.1.1 -- byte-identical output).
    ///
    /// Each row: `(seed, rand-long, rand-double-raw-bits,
    /// split-first-fields, split-second-fields)`.
    #[allow(clippy::type_complexity)]
    const ORACLE: &[(i64, i64, u64, (i64, i64), (i64, i64))] = &[
        (
            0,
            -2152535657050944081,
            4606131375998723001,
            (-7046029254386353131, 4354685564936845354),
            (-3239489724241199657, -2152535657050944081),
        ),
        (
            1,
            -7995527694508729151,
            4603278352542933067,
            (-7046029254386353131, 4354685564936845355),
            (-1706819287460639109, -7995527694508729151),
        ),
        (
            -1,
            -1956407806741107680,
            4606227141550632101,
            (-7046029254386353131, 4354685564936845353),
            (5036830254254310389, -1956407806741107680),
        ),
        (
            42,
            -4767286540954276203,
            4604854642168692077,
            (-7046029254386353131, 4354685564936845396),
            (540350159304224773, -4767286540954276203),
        ),
        (
            -42,
            2847773986881678254,
            4594730078858663700,
            (-7046029254386353131, 4354685564936845312),
            (-8451329845678729425, 2847773986881678254),
        ),
        (
            123456789,
            2466975172287755897,
            4593986331173909944,
            (-7046029254386353131, 4354685565060302143),
            (8650543683675089775, 2466975172287755897),
        ),
        (
            i64::MAX,
            3055647633038352039,
            4595136082073813452,
            (-7046029254386353131, -4868686471917930455),
            (2574394869760534423, 3055647633038352039),
        ),
        (
            i64::MIN,
            5196802822362493915,
            4598746622674119292,
            (-7046029254386353131, -4868686471917930454),
            (-5825544741555725513, 5196802822362493915),
        ),
    ];

    #[test]
    fn golden_gamma_is_javas() {
        // `(longify 0x9e3779b97f4a7c15)` printed by the JVM.
        assert_eq!(GOLDEN_GAMMA, -7046029254386353131);
    }

    #[test]
    fn rand_long_rand_double_and_split_match_the_jvm() {
        for &(seed, long, dbits, a, b) in ORACLE {
            let r = SplitRandom::from_seed(seed);
            assert_eq!(r.gamma, GOLDEN_GAMMA, "seed {seed}: gamma");
            assert_eq!(r.state, seed, "seed {seed}: state");
            assert_eq!(r.rand_long(), long, "seed {seed}: rand-long");
            assert_eq!(
                r.rand_double().to_bits(),
                dbits,
                "seed {seed}: rand-double (raw bits, so this is exact, \
                 not within-epsilon)"
            );
            assert!((0.0..1.0).contains(&r.rand_double()), "seed {seed}: rand-double range");
            let (r1, r2) = r.split();
            assert_eq!((r1.gamma, r1.state), a, "seed {seed}: (first (split r))");
            assert_eq!((r2.gamma, r2.state), b, "seed {seed}: (second (split r))");
        }
    }

    /// `(mapv (juxt #(.-gamma %) #(.-state %)) (r/split-n (r/make-random
    /// 0) n))` for n = 0/1/3, plus the rand-longs of a 5-way split, from
    /// the same JVM probe. n=3 is the smallest size that exercises the
    /// loop AND the trailing `(conj! ret (JavaUtilSplittableRandom. gamma
    /// state))`.
    #[test]
    fn split_n_matches_the_jvm() {
        let r = SplitRandom::from_seed(0);
        assert_eq!(r.split_n(0).unwrap(), vec![]);
        assert_eq!(r.split_n(1).unwrap(), vec![r]);
        let three: Vec<(i64, i64)> = r.split_n(3).unwrap().iter().map(|x| (x.gamma, x.state)).collect();
        assert_eq!(
            three,
            vec![
                (-3239489724241199657, -2152535657050944081),
                (7702497325448497535, 487617019471545679),
                (-7046029254386353131, 8709371129873690708),
            ]
        );
        let five: Vec<i64> = r.split_n(5).unwrap().iter().map(SplitRandom::rand_long).collect();
        assert_eq!(
            five,
            vec![
                1750893463095773485,
                -3696125781861132782,
                1132958324480006400,
                -2753407341945129819,
                4532161160992623299,
            ]
        );
        // The one deliberate deviation: the JVM loops forever here.
        assert!(r.split_n(-1).is_none());
    }

    /// Long chains are what generative testing actually does (one split
    /// per generated element), and they are where a single wrong shift
    /// would show up even if the first split happened to agree. All three
    /// finals are JVM-printed.
    #[test]
    fn thousand_deep_split_chains_match_the_jvm() {
        let mut r = SplitRandom::from_seed(42);
        for _ in 0..1000 {
            r = r.split().1;
        }
        assert_eq!((r.gamma, r.state), (7237899839683079931, 8690781334327564323));
        assert_eq!(r.rand_long(), -3484428716367203475);

        let mut r = SplitRandom::from_seed(42);
        for _ in 0..1000 {
            r = r.split().0;
        }
        assert_eq!((r.gamma, r.state), (-7046029254386353131, 1253963541391172666));
        assert_eq!(r.rand_long(), 6465557642177153689);

        let mut r = SplitRandom::from_seed(7);
        for _ in 0..100 {
            r = r.split_n(4).unwrap()[2];
        }
        assert_eq!((r.gamma, r.state), (2191236318260742075, 5467679010503001826));
        assert_eq!(r.rand_long(), 1659176598505418994);
    }

    /// `split-n`'s docstring claims equivalence to a particular series of
    /// 2-way splits; upstream tests it with `split-n-spec`. Cheap to
    /// re-check here, and it is what makes `gen/tuple` (which uses
    /// `split-n`) and `gen/vector` (which uses `split`) agree.
    #[test]
    fn split_n_agrees_with_repeated_two_way_splits() {
        for seed in [0i64, 1, -1, 42, i64::MAX] {
            let r = SplitRandom::from_seed(seed);
            for n in 1..8usize {
                let mut by_two = Vec::new();
                let mut cur = r;
                for _ in 0..n - 1 {
                    let (a, b) = cur.split();
                    by_two.push(b);
                    cur = a;
                }
                by_two.push(cur);
                assert_eq!(r.split_n(n as i64).unwrap(), by_two, "seed {seed}, n {n}");
            }
        }
    }
}
