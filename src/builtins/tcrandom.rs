//! SPEC-W6a: `clojure.test.check.random` as a Rust-native veneer.
//!
//! The five fns `clojure.test.check.generators` and
//! `clojure.test.check` actually call -- `make-random` (0- and
//! 1-arity), `split`, `split-n`, `rand-long`, `rand-double`, plus
//! upstream's public `make-java-util-splittable-random` for
//! completeness -- registered ONLY under the
//! `clojure.test.check.random/` prefix. The arithmetic they run, and
//! the bit-exactness evidence for it, live in [`crate::splitrandom`].
//!
//! # Why native, in one number
//!
//! test.check splits the RNG ONCE PER GENERATED ELEMENT, so this
//! namespace is the entire per-element cost of generative testing.
//! Interpreted (the vendored `random.clj`, a `deftype` reached through
//! an `IRandom` protocol), 10 000 `split` + `rand-long` took **1.90 s**
//! on the merge base. That is what made
//! `tests/clojure-suite/transducers.clj`'s `(quick-check 200000 ...)`
//! a ~40-minute proposition and the sole census delta, and what would
//! have made `clojure.spec.test.alpha/check` -- 1000 trials per fn by
//! default -- unusable.
//!
//! # Wiring, and the precedence rule it relies on
//!
//! Registering a native under a QUALIFIED symbol is enough to make the
//! namespace `require`-able with no file behind it: `env::Globals::
//! interned_namespaces` reports every namespace that has an interned
//! qualified name, and `ns::Interp::seed_builtin_namespaces` marks all
//! of them `loaded` at `Interp::new`. `require_ns` returns early on
//! `ns_loaded`, BEFORE it consults the module path or
//! `crate::stdlib`'s embedded table -- exactly the mechanism
//! `clojure.string` and `clojure.math` already ride.
//!
//! The consequence is deliberate and worth stating out loud, because it
//! INVERTS `crate::stdlib`'s usual "disk wins" rule for this one
//! namespace: a `clojure/test/check/random.clj` on `--module-path` is
//! now ignored, since the `require` never gets as far as looking for a
//! file. `tests/clojure-suite`'s runner materializes exactly such a
//! copy (alongside the rest of test.check, which still loads from
//! disk), so this is not hypothetical -- it is how the census suite
//! gets the fast RNG. `crate::stdlib`'s embedded row for the namespace
//! was REMOVED in the same commit rather than left as unreachable
//! fallback: it could never be selected, and a dead row in a table
//! whose entire job is to say what the binary ships would be a lie.
//!
//! What did NOT change: `clojure.test.check.generators` and
//! `clojure.test.check` still `(:require [clojure.test.check.random :as
//! random])` verbatim and still call `random/split` etc. verbatim. The
//! `IRandom` protocol indirection simply is not there any more -- those
//! calls resolve straight to the natives below. No vendored byte was
//! touched.
//!
//! # What is NOT ported
//!
//! The `IRandom` PROTOCOL itself. Nothing in the embedded test.check
//! stack, in `clojure.spec.alpha`, or in the census suite does
//! `(satisfies? random/IRandom x)` or `extend`s it (checked by grep
//! over `core/lib` and `tests/clojure-suite`), and inventing a protocol
//! object whose only implementation is a native type would be JVM
//! emulation deeper than anything demands -- the owner rule for every
//! veneer in this engine. A program that genuinely wants to plug in its
//! own RNG is a feature request, not a regression: say so, rather than
//! shipping a protocol nobody can usefully extend.

use std::cell::Cell;

use crate::builtins::ArityHint;
use crate::error::RjError;
use crate::eval::Interp;
use crate::splitrandom::SplitRandom;
use crate::value::{NativeFn, PVec, Symbol, Value};
use std::sync::Arc;

/// The namespace every name below is interned under. Nothing here is
/// ever bound BARE: `split`/`split-n`/`rand-long`/`rand-double`/
/// `make-random` are not `clojure.core` names, and binding them
/// globally would shadow user code for no benefit (contrast
/// `clojure.string`, whose natives are bare-plus-aliased because the
/// bare spellings predate the namespace).
const NS: &str = "clojure.test.check.random";

/// `(rand-long rng)`'s receiver check. Upstream would raise
/// `IllegalArgumentException: No implementation of method: :rand-long
/// of protocol: #'clojure.test.check.random/IRandom found for class:
/// ...`; without the protocol there is nothing to phrase it in terms
/// of, so this is an ordinary type error naming the fn and what it got.
fn rng_arg(v: &Value, op: &str) -> Result<SplitRandom, RjError> {
    match v {
        Value::TcRandom(r) => Ok(*r),
        // Metadata is invisible to what a value IS, the same rule
        // `reg_reading_receiver_meta` encodes for collections -- but an
        // RNG is never `IObj` in practice, so this is one cheap
        // discriminant test, not a code path anyone exercises.
        Value::Meta(m) => rng_arg(&m.inner, op),
        other => Err(RjError::type_err(format!(
            "{NS}/{op}: expected a random number generator, got {}",
            other.type_name()
        ))),
    }
}

/// `(long n)` on `split-n`'s count argument, and `^long seed` on
/// `make-random`'s. Real Clojure compiles both into `RT.longCast`,
/// which accepts any `Number`, truncates a `double` toward zero and
/// throws when the value does not fit a `long`; this reproduces that
/// for the numeric `Value`s that can actually reach here (every caller
/// in the embedded stack passes a `count` or a `:seed`, i.e. a
/// `Value::Int`).
fn long_arg(v: &Value, op: &str, what: &str) -> Result<i64, RjError> {
    match v {
        Value::Int(n) => Ok(*n),
        Value::BigInt(b) | Value::BigInteger(b) => b.to_i64_exact().ok_or_else(|| {
            RjError::other(format!("{NS}/{op}: {what} out of range for long: {}", b.to_decimal_string()))
        }),
        Value::Float(f) if f.is_finite() && *f >= (i64::MIN as f64) && *f <= (i64::MAX as f64) => {
            Ok(*f as i64)
        }
        other => Err(RjError::type_err(format!(
            "{NS}/{op}: {what} must be an integer, got {}",
            other.type_name()
        ))),
    }
}

// -----------------------------------------------------------------
// `make-random`'s 0-arity: upstream's seedless, per-thread generator.
// -----------------------------------------------------------------
//
// Upstream (`random.clj`'s `next-rng`) is:
//
//     (let [a (atom (make-java-util-splittable-random
//                     (System/currentTimeMillis)))
//           thread-local (proxy [ThreadLocal] []
//                          (initialValue []
//                            (first (split (swap! a #(second (split %)))))))]
//       (fn [] (let [rng (.get thread-local)
//                    [rng1 rng2] (split rng)]
//                (.set thread-local rng2)
//                rng1)))
//
// i.e. ONE process-wide atom, time-seeded, from which each thread
// carves an independent starting RNG on first use; thereafter each
// thread splits its own local one and keeps the second half. That
// shape is reproduced exactly below -- a process-global `AtomicI64`
// pair standing in for the atom, and a `thread_local!` `Cell` for the
// `ThreadLocal` -- because the INDEPENDENCE property is the point:
// two threads generating concurrently must not walk the same stream.
//
// Nothing about it is deterministic (it is seeded from the clock, and
// deliberately so: `make-random` with no seed exists precisely to be
// unpredictable), so no golden pins it. `seedless_make_random_streams_
// are_independent` pins the property that does matter.

/// The process-wide `a` atom. A plain `Mutex`, not a lock-free pair:
/// it is touched exactly ONCE PER THREAD, ever (the per-thread slot
/// below absorbs every subsequent call), so there is nothing here for
/// atomics to buy. `None` until the first seedless `make-random`, so
/// a program that never calls one never reads the clock.
static SEEDLESS_ROOT: std::sync::Mutex<Option<SplitRandom>> = std::sync::Mutex::new(None);

thread_local! {
    /// This thread's `ThreadLocal` slot. `None` until first use, then
    /// always `Some` -- exactly `proxy [ThreadLocal]`'s
    /// `initialValue`-on-first-`get` contract.
    static THREAD_RNG: Cell<Option<SplitRandom>> = const { Cell::new(None) };
}

/// `(next-rng)`.
fn next_rng() -> SplitRandom {
    let current = THREAD_RNG.with(|slot| slot.get());
    let rng = match current {
        Some(r) => r,
        None => {
            // `(first (split (swap! a #(second (split %)))))`
            let mut root = SEEDLESS_ROOT.lock().unwrap_or_else(|e| e.into_inner());
            let seeded = root
                .get_or_insert_with(|| SplitRandom::from_seed(crate::clock::clock_epoch_ms() as i64));
            *seeded = seeded.split().1;
            seeded.split().0
        }
    };
    let (rng1, rng2) = rng.split();
    THREAD_RNG.with(|slot| slot.set(Some(rng2)));
    rng1
}

/// [`crate::builtins::reg`], but bound ONLY under `NS/name`. Same
/// shape as `builtins::strings`'s own `reg_ns` (and for the same
/// reason it is duplicated rather than shared: `reg` always ALSO binds
/// the bare name, which is precisely what must not happen here).
#[track_caller]
fn reg_ns(
    i: &mut Interp,
    name: &'static str,
    arity: ArityHint,
    f: impl Fn(&mut Interp, &[Value]) -> Result<Value, RjError> + Send + Sync + 'static,
) {
    let native = NativeFn::new(name, move |interp: &mut Interp, args: &[Value]| {
        if !arity.matches(args.len()) {
            return Err(RjError::arity(format!(
                "{NS}/{name}: expected {}, got {}",
                arity.expected_desc(),
                args.len()
            ))
            .with_stack(interp.stack_snapshot(), interp.source_id)
            .with_arity_actual(args.len() as i64));
        }
        f(interp, args)
    });
    i.globals
        .set_builtin(Symbol { ns: Some(NS.into()), name: name.into() }, Value::Native(Arc::new(native)));
}

pub fn register(i: &mut Interp) {
    // `(make-random)` / `(make-random seed)`.
    reg_ns(i, "make-random", ArityHint::Range(0, 1), |_i, args| {
        Ok(Value::TcRandom(match args.first() {
            None => next_rng(),
            Some(seed) => SplitRandom::from_seed(long_arg(seed, "make-random", "seed")?),
        }))
    });
    // Upstream's other public constructor, which `make-random`'s
    // 1-arity is a one-line wrapper around. Nothing in the embedded
    // stack calls it; it is here because it is public API and costs one
    // line, not because anything demands it.
    reg_ns(i, "make-java-util-splittable-random", ArityHint::Exact(1), |_i, args| {
        Ok(Value::TcRandom(SplitRandom::from_seed(long_arg(
            &args[0],
            "make-java-util-splittable-random",
            "seed",
        )?)))
    });
    reg_ns(i, "rand-long", ArityHint::Exact(1), |_i, args| {
        Ok(Value::Int(rng_arg(&args[0], "rand-long")?.rand_long()))
    });
    reg_ns(i, "rand-double", ArityHint::Exact(1), |_i, args| {
        Ok(Value::Float(rng_arg(&args[0], "rand-double")?.rand_double()))
    });
    // Returns a two-element VECTOR: every caller destructures it as
    // `[r1 r2]`, and upstream's `split` literally builds `[a b]`.
    reg_ns(i, "split", ArityHint::Exact(1), |_i, args| {
        let (a, b) = rng_arg(&args[0], "split")?.split();
        Ok(Value::Vector(PVec::pair(Value::TcRandom(a), Value::TcRandom(b))))
    });
    reg_ns(i, "split-n", ArityHint::Exact(2), |_i, args| {
        let rng = rng_arg(&args[0], "split-n")?;
        let n = long_arg(&args[1], "split-n", "n")?;
        let parts = rng.split_n(n).ok_or_else(|| {
            // See `SplitRandom::split_n`'s doc: upstream spins forever
            // here, so there is no behaviour to match, only a hang to
            // refuse.
            RjError::type_err(format!("{NS}/split-n: n must not be negative, got {n}"))
        })?;
        Ok(Value::Vector(parts.into_iter().map(Value::TcRandom).collect()))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn interp() -> Interp {
        Interp::new()
    }

    fn eval(i: &mut Interp, src: &str) -> String {
        let v = i.eval_str("test", src).unwrap_or_else(|e| panic!("{src}: {e:?}"));
        crate::printer::pr_str(&v)
    }

    /// The namespace is `require`-able with NO module path and NO file
    /// -- the whole point of the veneer's wiring (see this module's
    /// doc). If this breaks, `clojure.test.check.generators` stops
    /// loading, so it is worth asserting directly rather than only
    /// through a generator test.
    #[test]
    fn the_namespace_requires_with_no_file_behind_it() {
        let mut i = interp();
        assert_eq!(eval(&mut i, "(require 'clojure.test.check.random) :ok"), ":ok");
        assert_eq!(
            eval(&mut i, "(clojure.test.check.random/rand-long (clojure.test.check.random/make-random 42))"),
            "-4767286540954276203"
        );
    }

    /// The same JVM-printed values `crate::splitrandom`'s tests pin,
    /// reached through the SCRIPT surface -- so a wiring mistake
    /// (wrong arity, arguments swapped, `split` returning its halves
    /// the wrong way round) fails here even though the arithmetic is
    /// right.
    #[test]
    fn script_surface_matches_the_jvm() {
        let mut i = interp();
        eval(&mut i, "(require '[clojure.test.check.random :as r])");
        assert_eq!(eval(&mut i, "(r/rand-long (r/make-random 0))"), "-2152535657050944081");
        assert_eq!(eval(&mut i, "(r/rand-double (r/make-random 0))"), "0.8833108082136426");
        assert_eq!(
            eval(&mut i, "(mapv r/rand-long (r/split (r/make-random 42)))"),
            "[5139283748462763858 -7511033593127921611]"
        );
        assert_eq!(
            eval(&mut i, "(mapv r/rand-long (r/split-n (r/make-random 0) 5))"),
            "[1750893463095773485 -3696125781861132782 1132958324480006400 \
             -2753407341945129819 4532161160992623299]"
        );
        assert_eq!(eval(&mut i, "(r/split-n (r/make-random 0) 0)"), "[]");
        assert_eq!(
            eval(&mut i, "(= (r/split-n (r/make-random 7) 1) [(r/make-random 7)])"),
            "true"
        );
        // `make-java-util-splittable-random` is `make-random`'s 1-arity.
        assert_eq!(
            eval(&mut i, "(= (r/make-java-util-splittable-random 42) (r/make-random 42))"),
            "true"
        );
    }

    /// `rand-long` must answer a `java.lang.Long`, never a bignum --
    /// the whole reason `docs/SPEC-PORT-PATCHES.md` section D item 9
    /// exists is that the INTERPRETED version's `^long` field hints
    /// were ignored and bignums leaked into `shrink-long`.
    #[test]
    fn rand_long_is_a_long_and_rand_double_is_in_range() {
        let mut i = interp();
        eval(&mut i, "(require '[clojure.test.check.random :as r])");
        assert_eq!(
            eval(&mut i, "(every? #(instance? java.lang.Long %)
                            (map r/rand-long (r/split-n (r/make-random 1) 200)))"),
            "true"
        );
        assert_eq!(
            eval(&mut i, "(every? #(and (<= 0.0 %) (< % 1.0))
                            (map r/rand-double (r/split-n (r/make-random 1) 200)))"),
            "true"
        );
        assert_eq!(
            eval(&mut i, "(class (r/make-random 1))"),
            "clojure.test.check.random.JavaUtilSplittableRandom"
        );
        assert_eq!(eval(&mut i, "(pr-str (r/make-random 1))"), "\"#<random>\"");
    }

    /// The documented divergence from the JVM, asserted so it can never
    /// change by accident: see `Value::TcRandom`'s doc.
    #[test]
    fn tc_random_equality_is_structural_not_identity() {
        let mut i = interp();
        eval(&mut i, "(require '[clojure.test.check.random :as r])");
        // JVM: false (deftype identity equality). Here: true.
        assert_eq!(eval(&mut i, "(= (r/make-random 42) (r/make-random 42))"), "true");
        assert_eq!(eval(&mut i, "(= (r/make-random 42) (r/make-random 43))"), "false");
        assert_eq!(
            eval(&mut i, "(= (hash (r/make-random 42)) (hash (r/make-random 42)))"),
            "true"
        );
        // ... and a set of RNGs therefore dedupes, which it would not on
        // the JVM. Same trade, stated in the other direction. (Built
        // with `set`, not a `#{}` literal: the reader rejects a literal
        // whose two elements are the same FORM as a duplicate key,
        // before either is ever evaluated -- real Clojure's does too.)
        assert_eq!(eval(&mut i, "(count (set [(r/make-random 1) (r/make-random 1)]))"), "1");
    }

    /// Bad arguments are ordinary errors, not panics -- `rand-long` on
    /// a non-RNG, `split-n` on a negative count (which upstream turns
    /// into an infinite loop; see `SplitRandom::split_n`).
    #[test]
    fn bad_arguments_are_errors() {
        let mut i = interp();
        eval(&mut i, "(require '[clojure.test.check.random :as r])");
        for src in [
            "(r/rand-long 42)",
            "(r/rand-double :nope)",
            "(r/split \"x\")",
            "(r/split-n (r/make-random 1) -1)",
            "(r/make-random :not-a-seed)",
        ] {
            assert!(i.eval_str("test", src).is_err(), "{src} should have been an error");
        }
    }

    /// `make-random`'s 0-arity: two calls on one thread must not return
    /// the same generator, and a second thread must not walk the first
    /// thread's stream. Nondeterministic by design (clock-seeded), so
    /// what is pinned is INDEPENDENCE, not values.
    #[test]
    fn seedless_make_random_streams_are_independent() {
        let mut i = interp();
        eval(&mut i, "(require '[clojure.test.check.random :as r])");
        let a = eval(&mut i, "(r/rand-long (r/make-random))");
        let b = eval(&mut i, "(r/rand-long (r/make-random))");
        assert_ne!(a, b, "successive seedless make-random calls must be independent");
        let other = std::thread::spawn(|| {
            let mut i = Interp::new();
            eval(&mut i, "(require '[clojure.test.check.random :as r])");
            eval(&mut i, "(r/rand-long (r/make-random))")
        })
        .join()
        .unwrap();
        assert_ne!(a, other, "a second thread must not walk the first thread's stream");
    }
}
