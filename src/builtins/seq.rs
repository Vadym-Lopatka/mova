//! `lazy-seq*` (the native laziness primitive) plus the seq library that
//! isn't naturally a macro/lazy-seq-in-core.mova concern: `reduce take drop
//! range repeat iterate concat apply doall dorun take-while drop-while some
//! every? not-every? not-any? sort sort-by distinct group-by frequencies
//! vec partition partition-all last`, plus R4's `butlast`.
//!
//! `map filter remove mapcat interpose cycle` are LAZY compositions and live
//! in `core/core.mova`, built on `lazy-seq`/`cons`/`first`/`rest`/`seq` (see
//! collections.rs's module doc for the lazy-seq cons-cell protocol those
//! rely on). Everything *here* either walks its input iteratively via
//! [`super::collections::uncons`] (never Rust-recursing per element, so
//! `(reduce + (range 100000))` costs O(1) Rust stack) or is a small
//! self-contained native lazy generator (`range`/`repeat`/`iterate`/
//! `concat`) built the same way `cons`-based core.mova laziness is.
//!
//! SPEC-W4 retired this file's one documented deviation: `concat` used to
//! be **eager** (one iterative loop over every input, returning a flat
//! list). It is now fully lazy, exactly like `clojure.core/concat` -- see
//! [`concat_lazy`]. The reason the eager version had survived was that it
//! kept lazy cons-cell markers out of MACRO-EXPANSION data (`->`/`->>`/
//! `doto`/`dotimes` all build their expansion with `concat`); that is now
//! handled where the JVM handles it, in the compiler rather than in
//! `concat` -- `Interp::realize_form_value` (eval/mod.rs) seqs a macro
//! expansion (and `eval`'s argument) into concrete `Form` shape the way
//! `Compiler.macroexpand1`'s `RT.seq`-driven analysis does.

use std::sync::{Arc, Mutex, OnceLock};

use crate::builtins::collections::{lazy_tail_split, materialize, seq_of, uncons};
use crate::builtins::numbers;
use crate::builtins::{reg, ArityHint};
use crate::error::RjError;
use crate::eval::Interp;
use crate::value::{LazySeq, NativeFn, PMap, PVec, Symbol, Value};

/// Wraps a 0-arity Rust closure as a `Value::Lazy` cell, exactly like a
/// user-level `(lazy-seq* (fn [] ...))` would, but without going through
/// the evaluator. Used by the native generators below.
fn make_lazy(f: impl Fn(&mut Interp, &[Value]) -> Result<Value, RjError> + Send + Sync + 'static) -> Value {
    let native = Value::Native(Arc::new(NativeFn::new("lazy-thunk", f)));
    Value::Lazy(Arc::new(LazySeq {
        thunk: Mutex::new(Some(native)),
        realized: Mutex::new(None),
    }))
}

/// Number of elements each of `range`/`repeat`/`iterate`'s native
/// generators packs into a single lazy node (a "chunk", after real
/// Clojure's chunked seqs -- same idea, different reason for existing
/// here). This is *not* an optimization -- it's load-bearing correctness:
/// see the big comment below.
const GEN_CHUNK: usize = 1024;

/// Builds a chunk's result for one of the internal generators below:
/// `items` are the elements just produced (already realized, up to
/// `GEN_CHUNK` of them), `tail` is whatever the recursive `*_lazy` call for
/// "the rest" returned. Terminal (`tail` is `Value::Nil`, i.e. the
/// generator has reached its bound and there is nothing left to defer): a
/// flat list of just `items`. Otherwise (`tail` is `Value::Lazy`): `items`
/// with `tail` appended as the trailing slot, wrapped in the
/// `Value::LazyTail` continuation marker by `collections::tag_tail` --
/// `uncons` peels flat prefixes one element at a time and recognizes a
/// final marked slot as a cons cell, so a "many heads + one lazy tail"
/// list works with zero changes there.
///
/// Why chunk at all: every `Value::Lazy` cell, once forced, permanently
/// holds a strong `Rc` to whatever it produced (that's the point of
/// memoization -- the same seq can be walked more than once). A seq built
/// one element per lazy node is therefore a genuine Rc-linked list once
/// realized, and `Value`/`LazySeq` have no custom `Drop` (nor can this
/// crate add one -- `value.rs`'s types are exact-per-ARCHITECTURE.md and
/// out of scope here): dropping a 100,000-node Rc chain recurses 100,000
/// stack frames through the default derived `Drop` and overflows, even
/// though every *forward* walk in this file is a flat Rust loop over
/// `uncons`. Packing `GEN_CHUNK` elements per node turns a 100,000-element
/// `(range 100000)` into ~100 chunk nodes instead of 100,000 -- the walk
/// stays O(1)-stack-per-element (chunks are plain `imbl::Vector`s, which
/// drop iteratively, not recursively) and the *chain* itself is shallow
/// enough that dropping it is a non-event. This is what makes `(reduce +
/// (range 100000))` stack-safe both to compute *and* to eventually drop.
fn gen_chunk(items: PVec, tail: Value) -> Value {
    match tail {
        Value::Nil => Value::List(items),
        other => {
            let mut v = items;
            v.push_back(crate::builtins::collections::tag_tail(other));
            Value::List(v)
        }
    }
}

/// C13 (sequences.clj 912/1148 gaps): `take`/`drop`/`repeat`'s `n` clamps a
/// NEGATIVE count to zero rather than erroring -- measured against the
/// oracle: `(take -1 [1 2 3])` => `()`, `(drop -2 [1 2 3])` => `(1 2 3)`
/// (unaffected -- see each call site), `(repeat -1 :x)` => `()`. This is
/// deliberately a NEW, separate helper rather than a behavior change to
/// the old strict `require_nonneg_int` it replaced at these 3 call sites
/// (since removed as dead code -- these were its only callers) --
/// narrowest possible fix, no risk to any call site this wave didn't
/// touch.
///
/// SPEC-W3 (defect ledger D3), the numeric-tower half: `repeat`'s `n`
/// reaches `clojure.lang.Repeat/create(long, Object)`, whose argument is
/// `RT.longCast`ed -- so any number is accepted and TRUNCATED toward zero.
/// mova used to raise `repeat: expected an int, got float` here instead,
/// which is the whole of that ledger entry.
///
/// Oracle transcript (1.13.0-alpha6), which is why this is truncation and
/// NOT [`clamp_nonneg_count_ceil`]'s ceiling -- the two rules genuinely
/// differ and must stay separate functions:
///   `(repeat 2.0 :x)`  => `(:x :x)`
///   `(repeat 2.7 :x)`  => `(:x :x)`     (truncate, not ceil)
///   `(repeat 5/2 :x)`  => `(:x :x)`     (`longCast` of a Ratio goes via
///                                        `bigIntegerValue`, truncating)
///   `(repeat -1.5 :x)` => `()`          (negative clamps to empty)
/// `NaN`/`±Infinity` fall out of the saturating, NaN-zeroing `as i64`
/// cast, matching `longCast`'s own range clamp closely enough for a count
/// that is then clamped to >= 0 anyway.
///
/// What this unblocks: `clojure.test.check.generators/size-bounded-bignat`
/// hands `(Math/ceil (/ ... 32))` -- a DOUBLE -- down to a `repeat`, so
/// `gen/simple-type` could not generate at all, and with it `s/gen` for
/// `any?`, `coll?`, `vector?`, `map?`, `set?`, `seq?` and `associative?`.
fn clamp_nonneg_int(v: &Value, op: &str) -> Result<i64, RjError> {
    match v {
        Value::Int(n) => Ok((*n).max(0)),
        Value::Float(f) => Ok((*f as i64).max(0)),
        Value::BigInt(b) | Value::BigInteger(b) => Ok(b.to_i64_saturating().max(0)),
        // Truncating division: `den > 0` is a `Ratio` invariant, so Rust's
        // `/` on `BigInt` (which truncates toward zero) is exactly
        // `BigInteger.longValue()`'s rule for both signs.
        Value::Ratio(r) => {
            let q = r.numer() / r.denom();
            Ok(crate::bignum::bigint_to_i64_saturating(&q).max(0))
        }
        Value::BigDec(d) => Ok((d.to_f64() as i64).max(0)),
        other => Err(RjError::type_err(format!(
            "{op}: expected a number, got {}",
            other.type_name()
        ))),
    }
}

/// `take`/`drop`'s own `n` clamp (and, transitively, `nthnext`/`nthrest`'s
/// family, which delegate to `drop` -- see core.mova). Unlike
/// [`clamp_nonneg_int`] above (still used by `repeat`, which truncates),
/// this ACCEPTS the whole numeric tower and rounds a positive fraction UP,
/// because real Clojure's `take`/`drop` never cast `n` to an integer at
/// all -- both are `(pos? n) ... (dec n)` decrement loops running over
/// whatever numeric type `n` already is (see `.oracle/clojure-src/src/clj/
/// clojure/core.clj`'s definitions), so a fractional `n` gets checked
/// AGAIN after each `dec`, and the loop only stops once the running value
/// drops to <= 0 -- one element later than truncation would predict.
///
/// Oracle transcript (this is `max(0, ceil(n))`, NOT truncation):
///   `(take 1.5 [1 2 3 4 5])`  => `(1 2)`      (ceil 1.5 = 2)
///   `(take 1.9 [1 2 3 4 5])`  => `(1 2)`      (ceil 1.9 = 2)
///   `(take 3/2 [1 2 3 4 5])`  => `(1 2)`      (ceil 3/2 = 2)
///   `(take 2.0 [1 2 3 4 5])`  => `(1 2)`      (integral float, unchanged)
///   `(take -1.5 [1 2 3 4 5])` => `()`         (n <= 0 => 0, any rounding)
///   `(drop 1.9 [1 2 3 4 5])`  => `(3 4 5)`    (ceil 1.9 = 2 dropped)
///   `(drop 3/2 [1 2 3 4 5])`  => `(3 4 5)`
///   `(drop -1.5 [1 2 3 4 5])` => `(1 2 3 4 5)` (no drop)
/// Real Clojure's `drop`'s IDrop fast path (vectors) makes the intent
/// explicit even where the generic loop doesn't run:
/// `(.drop coll (if (int? n) n (Math/ceil n)))` -- `Math/ceil` on exactly
/// the non-integer branch, confirming this is by design, not an accident
/// of `pos?`/`dec`. `NaN`/`+-Infinity` all fall out of the plain `as i64`
/// saturating-and-NaN-zeroing cast (`(take Double/NaN coll)` => `()`,
/// matching Java's `(pos? NaN)` being false).
fn clamp_nonneg_count_ceil(v: &Value, op: &str) -> Result<i64, RjError> {
    match v {
        Value::Int(n) => Ok((*n).max(0)),
        Value::Float(f) => Ok((f.ceil() as i64).max(0)),
        Value::BigInt(b) | Value::BigInteger(b) => Ok(b.to_i64_saturating().max(0)),
        Value::Ratio(r) => {
            let (num, den) = (r.numer(), r.denom()); // den > 0 invariant
            let q = if num.sign() == num_bigint::Sign::Plus {
                // Ceiling division for a positive fraction: floor((num +
                // den - 1) / den), exact since den > 0.
                (num + den - num_bigint::BigInt::from(1)) / den
            } else {
                // num <= 0 => quotient <= 0, gets clamped to 0 below
                // regardless of floor vs. ceil -- truncating division is
                // fine here.
                num / den
            };
            Ok(crate::bignum::bigint_to_i64_saturating(&q).max(0))
        }
        Value::BigDec(d) => Ok((d.to_f64().ceil() as i64).max(0)),
        other => Err(RjError::type_err(format!(
            "{op}: expected an int, got {}",
            other.type_name()
        ))),
    }
}

fn require_pos_int(v: &Value, op: &str) -> Result<usize, RjError> {
    match v {
        Value::Int(n) if *n > 0 => Ok(*n as usize),
        Value::Int(n) => Err(RjError::other(format!("{op}: expected a positive int, got {n}"))),
        other => Err(RjError::type_err(format!(
            "{op}: expected an int, got {}",
            other.type_name()
        ))),
    }
}

/// Every rank `default_compare`'s numeric arm accepts.
fn is_numeric_value(v: &Value) -> bool {
    matches!(
        v,
        Value::Int(_)
            | Value::Float(_)
            | Value::BigInt(_)
            | Value::BigInteger(_)
            | Value::Ratio(_)
            | Value::BigDec(_)
    )
}

#[allow(dead_code)] // superseded by `tower_ordering` in `default_compare`;
// kept as the lossy-widening counterpart for reference.
fn as_f64_of(v: &Value) -> f64 {
    match v {
        Value::Int(i) => *i as f64,
        Value::Float(f) => *f,
        _ => f64::NAN,
    }
}

/// `sort`/`sort-by`'s comparator when the caller supplies none.
///
/// S5: the numeric arm delegates to the tower's own
/// `builtins::numbers::tower_ordering`, so a `sort` over MIXED ranks works
/// at all -- `(sort [3/2 1 2N 0.5 1M])` used to die with `compare: cannot
/// compare ratio and int` because this match only knew `Int`/`Float`. It
/// also makes big values sort EXACTLY rather than through a lossy `f64`
/// widening, and keeps this function in step with
/// `builtins::sorted::natural_compare`, the OTHER default comparator in
/// the tree (`compare`, `sorted-map`, `sorted-set`), which routes to the
/// same place. A `None` ordering (a NaN was involved) is `Equal`, matching
/// `compare`'s own `if (lt x y) -1 else if (lt y x) 1 else 0` definition.
/// C10: was an independent, INCOMPLETE re-implementation of `compare`'s
/// algorithm (missing the `Vector`/`TypedVec`/`MapEntry` elementwise-
/// compare arm, `Nil`, and the empty-`List`/`Map`/`Set` identity case --
/// measured root cause of `test-duplicates`' `(sort [x1 z3a])` on two
/// `with-meta`'d vectors throwing "compare: cannot compare vector and
/// vector" even though `(compare x1 z3a)` itself worked fine). `sort`/
/// `sort-by`'s no-comparator-arg path now just delegates to
/// `builtins::sorted::natural_compare` -- the SAME fn the `compare`
/// builtin and `sorted-map`/`sorted-set`'s default ordering already use --
/// so this can never drift from them again.
fn default_compare(a: &Value, b: &Value) -> Result<std::cmp::Ordering, RjError> {
    crate::builtins::sorted::natural_compare(a, b)
}

/// A comparator fn may return a number (Java-`Comparator`-style: negative /
/// zero / positive) *or* a boolean (Clojure also accepts `<`/`>` directly as
/// a `sort`/`sort-by` comparator, treating `true` as "less than").
fn compare_via(interp: &mut Interp, comparator: &Value, a: &Value, b: &Value) -> Result<std::cmp::Ordering, RjError> {
    use std::cmp::Ordering;
    let r = interp.call(comparator, &[a.clone(), b.clone()])?;
    Ok(match r {
        Value::Int(n) => n.cmp(&0),
        Value::Float(f) => f.partial_cmp(&0.0).unwrap_or(Ordering::Equal),
        Value::Bool(true) => Ordering::Less,
        _ => Ordering::Greater,
    })
}

/// Plain insertion sort over an in-memory `Vec`. O(n^2), which is fine for
/// v0/test-sized inputs; chosen deliberately over `Vec::sort_by` so the
/// fallible, `&mut Interp`-threading comparator doesn't need to be smuggled
/// through a `FnMut(&T,&T) -> Ordering` closure.
fn sort_in_place(interp: &mut Interp, items: &mut [Value], comparator: Option<&Value>) -> Result<(), RjError> {
    for i in 1..items.len() {
        let mut j = i;
        while j > 0 {
            let ord = match comparator {
                Some(f) => compare_via(interp, f, &items[j - 1], &items[j])?,
                None => default_compare(&items[j - 1], &items[j])?,
            };
            if ord == std::cmp::Ordering::Greater {
                items.swap(j - 1, j);
                j -= 1;
            } else {
                break;
            }
        }
    }
    Ok(())
}

// -------------------- native lazy generators --------------------

/// C3b (measured, docstring confirmed): `step == 0` is its OWN case, not
/// folded into the `step >= 0` ("ascending") branch it used to share --
/// `(range 9 3 0)` must be the INFINITE `(9 9 9 ...)`, not `()`, because a
/// zero step never moves `current` toward (or away from) `end` at all;
/// the only thing that terminates a zero-step range is `current` already
/// EQUALING `end` at the start (`(range 5 5 0)` => `()`, matching
/// `clojure.core/range`'s own docstring: "When start is equal to end,
/// returns empty list" -- checked before the "when step is 0" rule).
fn range_continues(current: i64, end: Option<i64>, step: i64) -> bool {
    match end {
        Some(e) if step == 0 => current != e,
        Some(e) if step > 0 => current < e,
        Some(e) => current > e,
        None => true,
    }
}

/// nREPL interrupt poll for chunked generators: one relaxed load per chunk
/// (GEN_CHUNK elements), so `(vec (range 1e9))` can be stopped. Every consumer
/// (vec, count, reduce, doall, into, apply, sort, str/join) realizes through a
/// generator thunk, so this covers them.
#[inline(always)]
fn poll_interrupt(interp: &Interp) -> Result<(), RjError> {
    if interp.intr.pending() {
        if let Some(e) = interp.intr.take_err_loop() {
            return Err(e);
        }
    }
    Ok(())
}

fn range_lazy(current: i64, end: Option<i64>, step: i64) -> Value {
    if !range_continues(current, end, step) {
        return Value::Nil;
    }
    make_lazy(move |interp, _args| {
        poll_interrupt(interp)?;
        let mut items = PVec::new();
        let mut cur = current;
        // C3c (sequences.clj's `test-longrange-corners`): `cur += step`
        // used to be a plain wrapping `i64` add -- once `cur` got near
        // `Long/MAX_VALUE`/`MIN_VALUE`, adding a large `step` silently
        // WRAPPED to the opposite end of the `i64` range instead of
        // stopping, turning a 3-element range into an infinite one (a
        // real hang, caught by this task's own `[Long/MIN_VALUE
        // Long/MAX_VALUE Long/MAX_VALUE]` oracle probe: real Clojure's
        // `LongRange` stops the moment the NEXT step would overflow --
        // `(range Long/MIN_VALUE Long/MAX_VALUE Long/MAX_VALUE)` is
        // measured `(-9223372036854775808 -1 9223372036854775806)`,
        // exactly 3 elements, not a wrapped-around fourth). `checked_add`
        // makes that overflow observable instead of silent: the element
        // that triggered it is already pushed (it's a valid, in-bounds
        // element on its own), but the range ends there -- no further
        // chunk is generated past an overflowing step.
        let mut overflowed = false;
        for _ in 0..GEN_CHUNK {
            if !range_continues(cur, end, step) {
                break;
            }
            items.push_back(Value::Int(cur));
            match cur.checked_add(step) {
                Some(next) => cur = next,
                None => {
                    overflowed = true;
                    break;
                }
            }
        }
        let tail = if overflowed { Value::Nil } else { range_lazy(cur, end, step) };
        Ok(gen_chunk(items, tail))
    })
}

/// The general (non-`i64`) path for `range`: taken whenever `start`, `end`,
/// or `step` is a `Float`/`BigInt`/`BigInteger`/`Ratio`/`BigDec` -- an
/// all-`Int` call always stays on [`range_lazy`]'s `i64` fast path above,
/// unchanged. Element type is decided purely by `start`/`step` through the
/// numeric tower's own step ([`add_step_promoting`]) -- `end`'s type is
/// used ONLY for the continuation compare, never blended into the
/// elements. Measured against the oracle: `(range 1 3.5)` is `(1 2 3)`,
/// all `Long` (the `Double` end never widens the `Long` start/step);
/// `(range 2N)` is `(0 1)`, also `Long` (the `BigInt` end never widens the
/// default `Long` 0/1 start/step); ratio addition that lands on a whole
/// number promotes to `BigInt` rather than falling back to `Long`
/// (`(range 1/2 3 1/2)` is `(1/2 1N 3/2 2N 5/2)`, matching `(+' 1/2 1/2)`
/// => `1N`).
///
/// C3b: deliberately [`add_step_promoting`], NOT [`add_step`] (plain `+`'s
/// own fold step) -- real Clojure's `clojure.lang.Range` (this path's
/// analogue) increments with auto-promoting arithmetic, so an `i64`-range
/// `start` walking off the end of `i64` (`(range Long/MAX_VALUE ##Inf)`)
/// promotes to `BigInt` mid-sequence instead of throwing, even though
/// plain `(+ Long/MAX_VALUE 1)` itself throws. See `add_step_promoting`'s
/// own doc for the exact oracle transcript.
///
/// Direction (`<` vs `>`) is decided ONCE, up front, from `step`'s sign
/// against `0` -- matching the `i64` fast path's own three-way branch
/// (see [`range_continues`]'s doc), INCLUDING its `step == 0` case: a zero
/// step is its own branch, `current != end` (not merely "not yet reached
/// by the ascending compare") -- `(range 9 3 0)` is the INFINITE `(9 9 9
/// ...)`, not `()` (`9 < 3` would say "stop immediately", but real
/// Clojure's `range` docstring is unconditional: "When step is equal to 0,
/// returns an infinite sequence of start", checked only after "When start
/// is equal to end, returns empty list" -- so `(range 5 5 0)` is still
/// `()`). Both cases measured against the oracle.
fn range_lazy_tower(current: Value, end: Option<Value>, step: Value) -> Value {
    use crate::builtins::numbers::{add_step_promoting, gt2, lt2};
    let step_is_zero = !matches!(lt2(&step, &Value::Int(0)), Ok(Value::Bool(true)))
        && !matches!(gt2(&step, &Value::Int(0)), Ok(Value::Bool(true)));
    let step_negative = matches!(lt2(&step, &Value::Int(0)), Ok(Value::Bool(true)));
    let continues = move |cur: &Value, end: Option<&Value>| -> bool {
        match end {
            None => true,
            Some(e) if step_is_zero => {
                // Continue iff `cur` and `e` are NOT equal (either `<`
                // or `>` holds) -- see this fn's doc.
                matches!(lt2(cur, e), Ok(Value::Bool(true))) || matches!(gt2(cur, e), Ok(Value::Bool(true)))
            }
            Some(e) => {
                let cmp = if step_negative { gt2(cur, e) } else { lt2(cur, e) };
                matches!(cmp, Ok(Value::Bool(true)))
            }
        }
    };
    if !continues(&current, end.as_ref()) {
        return Value::Nil;
    }
    make_lazy(move |interp, _args| {
        poll_interrupt(interp)?;
        let mut items = PVec::new();
        let mut cur = current.clone();
        for _ in 0..GEN_CHUNK {
            if !continues(&cur, end.as_ref()) {
                break;
            }
            items.push_back(cur.clone());
            cur = add_step_promoting(&cur, &step)?;
        }
        Ok(gen_chunk(items, range_lazy_tower(cur, end.clone(), step.clone())))
    })
}

fn repeat_lazy(x: Value) -> Value {
    make_lazy(move |interp, _args| {
        poll_interrupt(interp)?;
        let items: PVec = std::iter::repeat_n(x.clone(), GEN_CHUNK).collect();
        Ok(gen_chunk(items, repeat_lazy(x.clone())))
    })
}

fn repeat_n_lazy(n: i64, x: Value) -> Value {
    if n <= 0 {
        return Value::Nil;
    }
    make_lazy(move |interp, _args| {
        poll_interrupt(interp)?;
        let take_n = (n as usize).min(GEN_CHUNK);
        let items: PVec = std::iter::repeat_n(x.clone(), take_n).collect();
        let remaining = n - take_n as i64;
        Ok(gen_chunk(items, repeat_n_lazy(remaining, x.clone())))
    })
}

/// `range_lazy`/`repeat_n_lazy` return a bare `Value::Nil` to mean "no more
/// elements" -- an internal sentinel `gen_chunk` relies on when they're
/// used as its recursive `tail` argument (see `gen_chunk`'s doc). But an
/// *immediately*-empty `(range 0)`/`(repeat 0 x)` call returns that same
/// bare `Nil` directly as the native fn's own result, which is wrong at
/// that boundary: Clojure's `range`/`repeat` always return a (possibly
/// empty) seq, printing as `()`, never bare `nil`. Used only at the two
/// top-level call sites below -- never passed back into `gen_chunk` -- so
/// it doesn't disturb the sentinel's internal meaning.
fn nil_to_empty_list(v: Value) -> Value {
    match v {
        Value::Nil => Value::List(PVec::new()),
        other => other,
    }
}

/// Shared, append-only backing store for one `(iterate f seed)` call.
/// `computed[0]` is always `seed` (free -- no call to `f`); `computed[i]`
/// for `i > 0` is `f` applied `i` times, filled in ON DEMAND the first
/// time any [`iterate_cursor`] asks for index `i`, and cached forever
/// after -- exactly `clojure.lang.Iterate`'s `_next` cache, just held in
/// one flat growable buffer instead of one field per node. `err` freezes
/// the first failure `f` ever raised (and the index it happened at): once
/// set, every cursor at or past that index re-raises the SAME error
/// instead of re-invoking `f` (which would double-fire side effects) if a
/// LATER cursor at or past the failure point is ever forced independently
/// of the one that first hit it.
struct IterateState {
    f: Value,
    computed: Mutex<Vec<Value>>,
    err: Mutex<Option<(usize, RjError)>>,
}

/// Real Clojure's `iterate` (`clojure.lang.Iterate`, measured against
/// JVM 1.12): `f` is called exactly once per NEWLY realized element, on
/// demand, cached -- never ahead of what the caller actually asked for.
/// The OLD implementation here called `f` eagerly to fill a `GEN_CHUNK`
/// (1024)-element buffer the moment ANY element of a chunk was demanded,
/// which is a correctness bug (not just a perf one): `(first (drop-while
/// #(< % 3) (take-while identity (iterate f 0))))` needs exactly 3 calls
/// to `f` on the JVM, but called it 1024 times here, because asking for
/// element 0 pulled 1023 more `f` calls nobody asked for yet.
///
/// This version calls `f` exactly `k` times to reveal element `k` (`k =
/// 0` costs nothing -- the seed is free), matching the JVM call-for-call,
/// while staying stack-safe for 100k+-element consumption WITHOUT
/// resurrecting the eager-chunk bug: rather than one `Value::Lazy` Arc
/// chained to the next (which, once the whole seq is realized and its
/// head finally drops, recurses one Rust stack frame per element via the
/// derived `Drop` glue -- the exact hazard `gen_chunk`'s chunking exists
/// to avoid), every element lives at a plain `usize` INDEX into one
/// shared, flat, iteratively-dropped `Vec` (`IterateState::computed`).
/// [`iterate_cursor`]'s per-index `Value::Lazy` cells therefore never
/// reference each other -- only the one shared `Arc<IterateState>` -- so
/// dropping a fully-realized 200,000-element `iterate` seq drops 200,000
/// independent, unlinked cells (each O(1) to drop) plus one `Vec` (which
/// drops iteratively, like any `Vec`), never a 200,000-deep Arc chain.
fn iterate_lazy(f: Value, x: Value) -> Value {
    let state = Arc::new(IterateState {
        f,
        computed: Mutex::new(vec![x]),
        err: Mutex::new(None),
    });
    iterate_cursor(state, 0)
}

/// One position in an `iterate` seq: forcing it yields `(nth-element,
/// tail)` where `tail` is the cursor for `index + 1`, extending
/// `state.computed` by exactly the calls to `f` needed to reach `index`
/// (0 if some earlier cursor sharing this `state` already reached that
/// far -- e.g. re-walking a `def`-bound seq a second time costs nothing,
/// same as the JVM's cached `_next`).
fn iterate_cursor(state: Arc<IterateState>, index: usize) -> Value {
    make_lazy(move |interp, _args| {
        loop {
            if let Some((fail_at, e)) = crate::sync::lock_mutex(&state.err).clone() {
                if index >= fail_at {
                    return Err(e);
                }
            }
            let have = crate::sync::lock_mutex(&state.computed).len();
            if have > index {
                break;
            }
            let last = crate::sync::lock_mutex(&state.computed)[have - 1].clone();
            match interp.call(&state.f, std::slice::from_ref(&last)) {
                Ok(next) => crate::sync::lock_mutex(&state.computed).push(next),
                Err(e) => {
                    *crate::sync::lock_mutex(&state.err) = Some((have, e.clone()));
                    return Err(e);
                }
            }
        }
        let elem = crate::sync::lock_mutex(&state.computed)[index].clone();
        let mut cell = PVec::new();
        cell.push_back(elem);
        cell.push_back(crate::builtins::collections::tag_tail(iterate_cursor(
            state.clone(),
            index + 1,
        )));
        Ok(Value::List(cell))
    })
}

/// SPEC-W4: `concat`'s lazy engine. State is exactly real Clojure's `cat`
/// loop variable pair -- `cur` is the seq currently being drained and
/// `colls[idx..]` is everything queued behind it -- and one force step
/// performs exactly one of `clojure.core/concat`'s three arms:
///
/// * nothing is queued behind `cur` (`idx == colls.len()`): hand `cur`
///   straight back, which is `(lazy-seq x)` for the 1-arity and the
///   `(if s (cons ..) y)` `y` arm for the 2-arity. `Interp::force` seqs
///   whatever comes back, so no element is touched and no cons cell is
///   built -- `(concat [1 2 3])` costs O(1), same as on the JVM.
/// * `cur` has an element: emit ONE cons cell whose tail is the next
///   state, deferred. Deliberately UNCHUNKED, unlike `range`/`repeat`/
///   `iterate` above: chunking would force `GEN_CHUNK` elements of a lazy
///   input the caller never asked for, and `concat`-over-a-lazy-input is
///   exactly the shape (`clojure.test.check.rose-tree/remove`) whose
///   whole point is that the un-consumed half is never computed. The
///   resulting per-element `Lazy` chain is the same shape `core.mova`'s
///   `map`/`filter` already produce and is walked/dropped the same way.
/// * `cur` is empty: drop to the next queued collection and retry, in a
///   Rust `loop` rather than by returning a new thunk -- a run of empty
///   inputs therefore costs no extra lazy nodes.
fn concat_lazy(cur: Value, colls: Arc<[Value]>, idx: usize) -> Value {
    make_lazy(move |interp, _args| {
        let mut cur = cur.clone();
        let mut i = idx;
        loop {
            if i >= colls.len() {
                return Ok(cur);
            }
            match uncons(interp, &cur)? {
                Some((h, t)) => {
                    let mut cell = PVec::new();
                    cell.push_back(h);
                    cell.push_back(crate::builtins::collections::tag_tail(concat_lazy(
                        t,
                        colls.clone(),
                        i,
                    )));
                    return Ok(Value::List(cell));
                }
                None => {
                    cur = colls[i].clone();
                    i += 1;
                }
            }
        }
    })
}

/// Lazy-rest twin of [`concat_lazy`] for `(apply concat <seq>)` where
/// `<seq>` may itself be infinite (mova's `mapcat` -- core.mova's
/// `(apply concat (apply map f colls))` -- is exactly this shape over an
/// `iterate`d/`range`less source). Real Clojure's `concat` is a plain
/// variadic Clojure fn, so `apply concat` there hits `RestFn.applyTo`,
/// which binds `& colls` to the LAZY tail rather than realizing it --
/// `apply`'s own native fallback (the plain `while uncons ...` drain
/// below `apply`'s registration) cannot do that for a Rust native like
/// `concat` (natives take an already-realized `&[Value]`), so `apply`
/// special-cases `concat` by name and comes here instead of draining.
///
/// Same per-step shape as `concat_lazy`, except the queue of remaining
/// collections (`colls_seq`) is itself walked ONE `uncons` at a time
/// rather than indexed off a pre-realized `Arc<[Value]>` -- so probing
/// for the next collection never forces further than the single element
/// needed to keep going.
fn concat_lazy_seq(cur: Value, colls_seq: Value) -> Value {
    make_lazy(move |interp, _args| {
        let mut cur = cur.clone();
        let mut colls_seq = colls_seq.clone();
        loop {
            match uncons(interp, &cur)? {
                Some((h, t)) => {
                    let mut cell = PVec::new();
                    cell.push_back(h);
                    cell.push_back(crate::builtins::collections::tag_tail(concat_lazy_seq(
                        t, colls_seq,
                    )));
                    return Ok(Value::List(cell));
                }
                None => match uncons(interp, &colls_seq)? {
                    Some((next, rest)) => {
                        cur = next;
                        colls_seq = rest;
                    }
                    None => return Ok(Value::Nil),
                },
            }
        }
    })
}

/// W-REDUCE Part B: an element source for [`reduce_coll`]'s walk that
/// avoids `uncons`'s pop_front-per-element cost whenever `coll`'s shape
/// allows it, while staying behaviorally IDENTICAL (same elements, same
/// order, same forcing of lazy tails, same errors) to the plain
/// `uncons`-driven `while` loop this replaces.
///
/// - `Fast`: built for a `Value::Vector` or a `Value::List` -- including
///   the internal range/repeat/iterate chunk-generator shape (a `List`
///   whose LAST slot is the `LazyTail` marker, see this module's and
///   `collections.rs`'s doc comments). Elements `0..data_len` are read by
///   INDEX off the (cheaply-`clone`d, persistent) chunk `PVec` -- no
///   per-element `pop_front` mutation/rebalancing. Once `idx` reaches
///   `data_len`, ONE `uncons` call on `tail` both forces the next chunk
///   (if any) and yields the single element `uncons` peels off; the
///   NEW cursor state is then re-derived from whatever `uncons` handed
///   back as "the rest" (`lazy_tail_split`'d the same way `ElemWalk::new`
///   itself parses `coll`), so the fast path resumes for the remaining
///   `data_len - 1` elements of that chunk. This makes `uncons` run once
///   per CHUNK boundary (every `GEN_CHUNK` elements for a native
///   generator, or once total for a plain `Vector`/flat `List`) instead
///   of once per element.
/// - `Generic`: exactly the original `uncons`-per-element walk, used for
///   every shape `Fast` doesn't recognize (`Map`/`Set`/`Str`/etc.) AND as
///   the automatic fallback if a `Fast` cursor's chunk boundary ever
///   forces to something other than `Nil`/`List` (nothing this codebase
///   builds does that today -- every native chunk generator's `tail`
///   forces to one of those two shapes -- but `next` degrades to calling
///   `uncons` every element rather than mis-happening if it ever does).
enum ElemWalk {
    Fast {
        items: PVec,
        idx: usize,
        data_len: usize,
        tail: Value,
    },
    Generic(Value),
}

impl ElemWalk {
    fn new(v: Value) -> ElemWalk {
        match &v {
            Value::Vector(items) => ElemWalk::Fast {
                items: items.clone(),
                idx: 0,
                data_len: items.len(),
                tail: Value::Nil,
            },
            Value::List(items) => match lazy_tail_split(items) {
                Some(data_len) => {
                    let tail = match &items[data_len] {
                        Value::LazyTail(cell) => Value::Lazy(cell.clone()),
                        _ => unreachable!("lazy_tail_split guarantees the marker at data_len"),
                    };
                    ElemWalk::Fast {
                        items: items.clone(),
                        idx: 0,
                        data_len,
                        tail,
                    }
                }
                None => ElemWalk::Fast {
                    items: items.clone(),
                    idx: 0,
                    data_len: items.len(),
                    tail: Value::Nil,
                },
            },
            _ => ElemWalk::Generic(v),
        }
    }

    fn next(&mut self, interp: &mut Interp) -> Result<Option<Value>, RjError> {
        match self {
            ElemWalk::Generic(cur) => match uncons(interp, cur)? {
                None => Ok(None),
                Some((h, t)) => {
                    *cur = t;
                    Ok(Some(h))
                }
            },
            ElemWalk::Fast {
                items,
                idx,
                data_len,
                tail,
            } => {
                if *idx < *data_len {
                    let v = items.get_owned(*idx).expect("idx < data_len");
                    *idx += 1;
                    return Ok(Some(v));
                }
                // Chunk boundary (or a non-Fast-shaped `tail`, which is
                // `Value::Nil` forever after the first pass through this
                // arm below -- see the `other` arm's comment): peel
                // exactly one element off `tail` via the shared `uncons`,
                // then re-derive this cursor's state from whatever it
                // handed back as "the rest".
                let boundary = tail.clone();
                match uncons(interp, &boundary)? {
                    None => Ok(None),
                    Some((h, new_tail)) => {
                        match new_tail {
                            Value::List(rest_items) => match lazy_tail_split(&rest_items) {
                                Some(dl) => {
                                    let m = match &rest_items[dl] {
                                        Value::LazyTail(cell) => Value::Lazy(cell.clone()),
                                        _ => unreachable!("lazy_tail_split guarantees the marker at dl"),
                                    };
                                    *items = rest_items;
                                    *idx = 0;
                                    *data_len = dl;
                                    *tail = m;
                                }
                                None => {
                                    *data_len = rest_items.len();
                                    *items = rest_items;
                                    *idx = 0;
                                    *tail = Value::Nil;
                                }
                            },
                            Value::Nil => {
                                *items = PVec::new();
                                *idx = 0;
                                *data_len = 0;
                                *tail = Value::Nil;
                            }
                            // Not a shape `Fast` recognizes -- fall back to
                            // calling `uncons` on it every subsequent
                            // element (same cost/behavior as `Generic`),
                            // by parking it as `tail` with an already-
                            // exhausted `[idx, data_len)` window.
                            other => {
                                *items = PVec::new();
                                *idx = 0;
                                *data_len = 0;
                                *tail = other;
                            }
                        }
                        Ok(Some(h))
                    }
                }
            }
        }
    }
}

/// W-REDUCE Part A: which boot arithmetic native (if any) `f` is,
/// recognized by pointer identity against the ORIGINAL boot registration
/// -- see [`BootArith`]/[`recognized_arith`].
#[derive(Clone, Copy)]
enum ArithOp {
    Add,
    Mul,
    Min,
    Max,
}

/// The boot `+`/`*`/`min`/`max` natives' `Arc<NativeFn>` handles, captured
/// ONCE at the end of [`register`] (after `numbers::register` has already
/// installed them -- see `builtins::mod::register_all`'s fixed order).
/// [`recognized_arith`] pointer-compares a `reduce` call's `f` against
/// these: `f` arrives at `reduce_coll` ALREADY RESOLVED (the evaluator
/// looked `+`/`*`/`min`/`max` up as a symbol before calling `reduce`), so
/// this is not a stand-in for the compiled tier's `GlobalChain::
/// intrinsic_armed` var-cell guard (`compile::ir.rs`) -- it doesn't need
/// to be, because there is no cell left to re-check: a shadowing `(let
/// [+ -] (reduce + ...))` or a redefining `(def + ...)` both change what
/// VALUE the `+` symbol evaluates to, so `f` is simply a different
/// `Arc<NativeFn>` (or not a `Value::Native` at all) by the time it gets
/// here, which this pointer compare naturally rejects.
struct BootArith {
    add: Arc<NativeFn>,
    mul: Arc<NativeFn>,
    min: Arc<NativeFn>,
    max: Arc<NativeFn>,
}

static BOOT_ARITH: OnceLock<BootArith> = OnceLock::new();

/// Called once from [`register`]. `numbers::register` (which installs
/// `+`/`*`/`min`/`max`) always runs before `seq::register` -- see
/// `builtins::mod::register_all` -- so every lookup here is expected to
/// hit; if any doesn't (e.g. a future refactor changes registration
/// order), `BOOT_ARITH` is simply left unset and [`recognized_arith`]
/// always returns `None`, i.e. Part A silently never engages rather than
/// panicking or miscompiling.
fn cache_boot_arith(i: &Interp) {
    let native_of = |name: &str| -> Option<Arc<NativeFn>> {
        match i.globals.get(&Symbol::simple(name)) {
            Some(Value::Native(n)) => Some(n),
            _ => None,
        }
    };
    if let (Some(add), Some(mul), Some(min), Some(max)) =
        (native_of("+"), native_of("*"), native_of("min"), native_of("max"))
    {
        let _ = BOOT_ARITH.set(BootArith { add, mul, min, max });
    }
}

/// `Some(op)` iff `f` is EXACTLY the boot registration of `+`/`*`/`min`/
/// `max` (pointer identity, not merely "a native named `+`") -- see
/// [`BootArith`]'s doc for why that's a sufficient shadowing/redefinition
/// guard here.
fn recognized_arith(f: &Value) -> Option<ArithOp> {
    let Value::Native(n) = f else { return None };
    let boot = BOOT_ARITH.get()?;
    if Arc::ptr_eq(n, &boot.add) {
        Some(ArithOp::Add)
    } else if Arc::ptr_eq(n, &boot.mul) {
        Some(ArithOp::Mul)
    } else if Arc::ptr_eq(n, &boot.min) {
        Some(ArithOp::Min)
    } else if Arc::ptr_eq(n, &boot.max) {
        Some(ArithOp::Max)
    } else {
        None
    }
}

/// W-REDUCE Part A: the unboxed fold for a recognized boot arithmetic
/// native. Walks `cur` via the SAME [`ElemWalk`] Part B uses (fast index
/// path for `Vector`/`List`/chunked-range shapes, `uncons`-per-element
/// otherwise), but folds each element straight through `numbers::
/// add_step`/`mul_step`/`min_max_reduce_step` -- the EXACT step fns `+`/
/// `*`/`min`/`max`'s own natives fold with (see each's doc: overflow
/// promotion, `*unchecked-math*` wrapping, NaN short-circuit, tower
/// widening are all untouched) -- instead of going through
/// `Value::Native` call dispatch, the arity-check wrapper `reg` installs,
/// and the `[acc, x]` 2-slot buf handover every element. The win is
/// entirely from deleting that per-element ceremony, not from any new
/// arithmetic.
///
/// No `Reduced` check: none of `add_step`/`mul_step`/`min_max_reduce_step`
/// can ever produce a `Value::Reduced` (only a user-supplied `f` calling
/// `reduced` can), so this loop is shaped as simply as the arithmetic
/// allows while staying result-identical to the generic path it replaces.
fn reduce_fast_arith(interp: &mut Interp, op: ArithOp, mut acc: Value, cur: Value) -> Result<Value, RjError> {
    let mut walk = ElemWalk::new(cur);
    while let Some(h) = walk.next(interp)? {
        acc = match op {
            ArithOp::Add => numbers::add_step(interp, &acc, &h)?,
            ArithOp::Mul => numbers::mul_step(interp, &acc, &h)?,
            ArithOp::Min => numbers::min_max_reduce_step(&acc, &h, "<", "min")?,
            ArithOp::Max => numbers::min_max_reduce_step(&acc, &h, ">", "max")?,
        };
    }
    Ok(acc)
}

/// The `reduce` builtin's whole body, extracted (C10) so `eval::
/// types_forms::eval_dot_form`'s `.reduce` dot-method veneer
/// (`ireduce-reduced`'s `(.reduce ^clojure.lang.IReduce (list 1 2 3 4 5)
/// f)`, real `IReduce.reduce(f)`/`IReduce.reduce(f, init)`) can call the
/// EXACT same logic -- honoring `reduced` short-circuit -- instead of a
/// second, driftable implementation. `init = None` is the 1-arity shape
/// (`f`'s own first call seeds off `coll`'s first element, `uncons`'d
/// here); `init = Some(_)` is the 2-arity shape (measured: a
/// pre-`reduced` `init` does NOT short-circuit before `f`'s first call,
/// see the inline comment below).
///
/// W-REDUCE: Part A (`reduce_fast_arith`) and Part B (`ElemWalk`) both
/// live above this fn. Part A's check runs first and, if it fires,
/// bypasses the generic walk ENTIRELY (including its own `ElemWalk`
/// construction happens inside `reduce_fast_arith` instead) -- so a
/// recognized `+`/`*`/`min`/`max` never pays for the `[acc, x]` buf setup
/// below at all.
pub(crate) fn reduce_coll(interp: &mut Interp, f: &Value, init: Option<Value>, coll: &Value) -> Result<Value, RjError> {
    let (acc, cur) = match init {
        Some(init) => (init, coll.clone()),
        None => match uncons(interp, coll)? {
            None => return interp.call(f, &[]),
            Some((h, t)) => (h, t),
        },
    };
    // C10: measured -- a pre-`reduced` INIT does NOT short-circuit before
    // `f`'s first call (`(reduce (fn [a b] ..) (reduced 5) [1 2 3])`
    // calls `f` once, then throws `ClassCastException` trying to use the
    // `Reduced` as a number, real JVM behavior) -- so there is
    // deliberately NO pre-loop check here, only the post-call one below.
    // (A recognized arithmetic `f` can never BE a pre-`reduced` value in
    // the first place -- `recognized_arith` only matches `Value::Native`
    // -- so `reduce_fast_arith` below has no such case to reproduce.)
    if let Some(op) = recognized_arith(f) {
        return reduce_fast_arith(interp, op, acc, cur);
    }
    // PHASE 3 (Perceus-lite): the accumulator is HANDED OVER, not lent.
    // `interp.call(f, &[acc, h])` moved `acc` into a temporary array --
    // but that array outlived the call, so `acc` had a live second handle
    // throughout the callee's body and phase 2's last-use take could
    // never make it unique (measured: 0.00% on every `reuse-assoc-*`
    // bench, before AND after phase 2). Handing the buffer over instead
    // takes that handle away; ONE two-slot STACK array serves the whole
    // reduction, so the shapes that cannot benefit (`(reduce + ..)`: a
    // native with no consuming entry point) pay no allocation for it.
    // The accumulator LIVES in slot 0 of that array between calls, so a
    // round trip is two writes and no shuffling: the callee takes slot 0
    // (leaving `nil`), and the result is written straight back over it.
    let mut walk = ElemWalk::new(cur);
    let mut buf = [acc, Value::Nil];
    while let Some(h) = walk.next(interp)? {
        buf[1] = h;
        buf[0] = interp.call_with_buf(f, &mut buf)?;
        // C10: short-circuit -- measured, `(reduce (fn [_ a] (if (= a 5)
        // (reduced "foo") a)) 0 [1 2 3 4 5 6 7])` is `"foo"`, `f` never
        // called again once it returns a `Reduced`, and the RESULT is
        // unwrapped (never a bare `#<reduced ..>`).
        if let Value::Reduced(v) = &buf[0] {
            return Ok((**v).clone());
        }
    }
    let [acc, _] = buf;
    Ok(acc)
}

/// `range`'s whole body, pulled out to a standalone `pub(crate)` fn (was
/// an inline closure until C3c) so `builtins::statics`'s `clojure.lang.
/// Range/create` and `clojure.lang.LongRange/create` statics
/// (`sequences.clj`'s `test-longrange-corners`/`unlimited-range-create`
/// helper spells BOTH class statics, comparing them against each other
/// and against plain `(range ...)`) can delegate to the EXACT SAME
/// implementation rather than re-deriving range semantics a second time
/// -- measured: real `clojure.lang.Range/create`/`LongRange/create` and
/// `clojure.core/range` agree on every corner this task's oracle probed
/// (see `test-longrange-corners`'s own `Long/MAX_VALUE`/`MIN_VALUE`
/// overflow cases), so one body serving all three call sites is not an
/// approximation, it's the same fn Clojure's own `range` calls into.
pub(crate) fn range_impl(_i: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    let (start, end, step) = match args.len() {
        0 => (Value::Int(0), None, Value::Int(1)),
        1 => (Value::Int(0), Some(require_range_num(&args[0])?), Value::Int(1)),
        2 => (
            require_range_num(&args[0])?,
            Some(require_range_num(&args[1])?),
            Value::Int(1),
        ),
        _ => (
            require_range_num(&args[0])?,
            Some(require_range_num(&args[1])?),
            require_range_num(&args[2])?,
        ),
    };
    // The `i64` fast path stays exactly as it was pre-S7 -- taken
    // whenever every SUPPLIED bound is an `Int` (unsupplied bounds
    // default to `Int`s too, so a bare `(range)`/`(range 5)` never
    // leaves it). Anything else (`Float`/`BigInt`/`BigInteger`/
    // `Ratio`/`BigDec` in any position) routes to the general tower
    // path instead -- see `range_lazy_tower`'s doc for the measured
    // element-type and direction semantics.
    let all_int = matches!(start, Value::Int(_))
        && matches!(step, Value::Int(_))
        && end.as_ref().map_or(true, |e| matches!(e, Value::Int(_)));
    let r = if all_int {
        let s = match start {
            Value::Int(n) => n,
            _ => unreachable!("all_int guarantees start is Int"),
        };
        let st = match step {
            Value::Int(n) => n,
            _ => unreachable!("all_int guarantees step is Int"),
        };
        let e = match end {
            None => None,
            Some(Value::Int(n)) => Some(n),
            Some(_) => unreachable!("all_int guarantees end is Int"),
        };
        range_lazy(s, e, st)
    } else {
        range_lazy_tower(start, end, step)
    };
    Ok(nil_to_empty_list(r))
}

pub fn register(i: &mut Interp) {
    reg(i, "lazy-seq*", ArityHint::Exact(1), |_i, args| {
        Ok(Value::Lazy(Arc::new(LazySeq {
            thunk: Mutex::new(Some(args[0].clone())),
            realized: Mutex::new(None),
        })))
    });

    reg(i, "range", ArityHint::Range(0, 3), range_impl);

    reg(i, "repeat", ArityHint::Range(1, 2), |_i, args| {
        if args.len() == 1 {
            Ok(repeat_lazy(args[0].clone()))
        } else {
            let r = repeat_n_lazy(clamp_nonneg_int(&args[0], "repeat")?, args[1].clone());
            Ok(nil_to_empty_list(r))
        }
    });

    // S6: `replicate` -- deprecated-but-present core fn, `(defn replicate
    // [n x] (take n (repeat x)))` upstream. Measured: `(replicate 3 :a)`
    // => `(:a :a :a)`, `(replicate 0 :a)`/`(replicate -2 :a)` => `()`
    // (negative clamps to empty like `take`, it does NOT error the way
    // `repeat`'s own 2-arity does -- so this deliberately does NOT reuse
    // `require_nonneg_int`), class `clojure.lang.LazySeq` on a non-empty
    // result. `repeat_n_lazy` already treats `n <= 0` as "no elements" for
    // its `gen_chunk` sentinel use, which is exactly the clamp needed
    // here too -- `nil_to_empty_list` turns that sentinel into a real
    // `()` at this top-level boundary the same way `repeat`'s own 2-arity
    // does above.
    reg(i, "replicate", ArityHint::Exact(2), |_i, args| {
        let n = match &args[0] {
            Value::Int(n) => *n,
            other => {
                return Err(RjError::type_err(format!(
                    "replicate: expected an int, got {}",
                    other.type_name()
                )))
            }
        };
        Ok(nil_to_empty_list(repeat_n_lazy(n, args[1].clone())))
    });

    reg(i, "iterate", ArityHint::Exact(2), |_i, args| Ok(iterate_lazy(args[0].clone(), args[1].clone())));

    // SPEC-W4: LAZY, like `clojure.core/concat` -- see `concat_lazy`. The
    // result is always a `Value::Lazy` cell, so `(class (concat ..))` is
    // `clojure.lang.LazySeq` for every arity including `(concat)` (which
    // realizes to `nil` and prints `()`, measured against 1.13.0-alpha6).
    reg(i, "concat", ArityHint::Any, |_i, args| {
        let (first, rest) = match args.split_first() {
            Some((f, r)) => (f.clone(), r.to_vec()),
            None => (Value::Nil, Vec::new()),
        };
        Ok(concat_lazy(first, Arc::from(rest), 0))
    });

    // C10: `reduced`/`reduced?` -- `reduce`'s early-termination protocol
    // (measured, previously unresolvable ANYWHERE in mova: `Unable to
    // resolve symbol: reduced`). See `Value::Reduced`'s own doc for the
    // representation; `deref`/`@` unwrapping lives in `builtins::atoms`.
    reg(i, "reduced", ArityHint::Exact(1), |_i, args| {
        Ok(Value::Reduced(std::sync::Arc::new(args[0].clone())))
    });
    reg(i, "reduced?", ArityHint::Exact(1), |_i, args| {
        Ok(Value::Bool(matches!(args[0], Value::Reduced(_))))
    });

    reg(i, "reduce", ArityHint::Range(2, 3), |interp, args| {
        let init = if args.len() == 3 { Some(args[1].clone()) } else { None };
        let coll = if args.len() == 3 { &args[2] } else { &args[1] };
        reduce_coll(interp, &args[0], init, coll)
    });

    // C13: renamed from `take` -- mova's `core.mova` now owns that name,
    // dispatching its 1-arity (TRANSDUCER, needs `reduced`/`volatile!`) to
    // a mova closure and its 2-arity (this native, unchanged) straight
    // through to here. Same rename-and-wrap shape as `partition-all-coll*`
    // above.
    reg(i, "take-coll*", ArityHint::Exact(2), |interp, args| {
        let n = clamp_nonneg_count_ceil(&args[0], "take")?;
        let mut out = PVec::new();
        let mut cur = args[1].clone();
        for _ in 0..n {
            match uncons(interp, &cur)? {
                Some((h, t)) => {
                    out.push_back(h);
                    cur = t;
                }
                None => break,
            }
        }
        Ok(Value::List(out))
    });

    // C13: same rename-and-wrap as `take-coll*` above.
    reg(i, "drop-coll*", ArityHint::Exact(2), |interp, args| {
        let n = clamp_nonneg_count_ceil(&args[0], "drop")?;
        let mut cur = args[1].clone();
        for _ in 0..n {
            match uncons(interp, &cur)? {
                Some((_, t)) => cur = t,
                None => {
                    cur = Value::Nil;
                    break;
                }
            }
        }
        // `drop` always returns a *seq* (never bare `nil`, and never the
        // original collection's own type -- e.g. `(drop 0 [1 2 3])` prints
        // `(1 2 3)`, not `[1 2 3]`), matching Clojure. `n == 0` is the one
        // case the loop above never touches `cur`, so it still needs
        // canonicalizing here.
        //
        // C11: canonicalize through `seq_of` (`seq`'s own implementation),
        // NOT `Interp::seq_items`. `cur` here is the only place in the
        // codebase where `seq_items` could be handed an improper list
        // (`[..data.., <lazy rest>]`) or a bare `Lazy` -- and `seq_items`
        // now REALIZES those (that is the `~@` double-wrap fix; see its
        // doc), which would make `(take 3 (drop 2 (range)))` walk an
        // infinite range. `seq_of` hands a non-empty `List`/`Lazy` back
        // shape-preserved, so `drop` stays exactly as lazy as it was and
        // behaves identically on every other input (`Vector` -> `List`,
        // `Map`/`Set`/`Str`/... -> realized `List`, as before).
        // `.unmeta()` keeps the pre-C11 metadata behaviour exactly:
        // `seq_items` saw through `Value::Meta` and rebuilt a bare
        // `List`, whereas `seq_of`'s own `Meta` arm deliberately hands a
        // metadata-carrying LIST back untouched (that is `seq`'s
        // identity rule, not `drop`'s -- measured, `(meta (drop 0
        // (with-meta (list 1 2) {:a 1})))` is `nil` on the oracle).
        let cur = cur.unmeta().clone();
        Ok(match seq_of(interp, &cur)? {
            Value::Nil => Value::List(PVec::new()),
            seqd => seqd,
        })
    });

    reg(i, "doall", ArityHint::Exact(1), |interp, args| {
        Ok(Value::List(materialize(interp, &args[0])?.into_iter().collect()))
    });

    reg(i, "dorun", ArityHint::Exact(1), |interp, args| {
        let mut cur = args[0].clone();
        while let Some((_, t)) = uncons(interp, &cur)? {
            cur = t;
        }
        Ok(Value::Nil)
    });

    reg(i, "last", ArityHint::Exact(1), |interp, args| {
        let mut result = Value::Nil;
        let mut cur = args[0].clone();
        while let Some((h, t)) = uncons(interp, &cur)? {
            result = h;
            cur = t;
        }
        Ok(result)
    });

    // W3e-3: `apply` realizes only as much of its trailing seq as it must.
    // Real Clojure's `RestFn.applyTo` walks `boundedLength(arglist,
    // requiredArity)` nodes and then hands the REST of the seq straight to
    // `doInvoke` -- a variadic fn's `& rest` parameter is a seq, and `apply`
    // never had any business forcing it. mova used to drain the whole thing,
    // so `(defn sample [& args] 0)` + `(apply sample (range))` -- `0` on the
    // oracle -- hung forever
    // (`clojure.test-clojure.vars/test-vars-apply-lazily`).
    //
    // Nothing else changes: when the seq turns out to be finite and short,
    // or the callee is a native / a fixed-arity closure (`variadic_apply_
    // shape` answers `None`), this is the same full drain it always was.
    reg(i, "apply", ArityHint::Min(2), |interp, args| {
        let f = args[0].clone();
        let mut call_args: Vec<Value> = args[1..args.len() - 1].to_vec();
        let mut cur = args[args.len() - 1].clone();
        // W-CONCAT-LAZY: `concat` is a Rust native (`ArityHint::Any`), not
        // a variadic Clojure closure, so `variadic_apply_shape` below never
        // fires for it and `apply` would otherwise fall to the plain
        // drain-everything loop at the bottom of this native -- exactly
        // the shape that hangs `(apply concat (repeat [1]))`/mova's own
        // `mapcat` over an infinite source (real Clojure's `concat` IS a
        // plain variadic fn, so `apply concat` there rides the same lazy
        // `RestFn.applyTo` path as any other user variadic fn). Special-
        // cased by name, ahead of the generic paths: fold the already-
        // realized `call_args` (ordinary `apply` arguments before the
        // trailing seq, always few and finite) onto the front of `cur`
        // with the lazy-preserving `cons_builtin`, then hand the whole
        // lazy chain to `concat_lazy_seq` instead of touching `cur` at all.
        if let Value::Native(nf) = &f {
            if &*nf.name == "concat" {
                let mut colls_seq = cur;
                for v in call_args.into_iter().rev() {
                    colls_seq = crate::builtins::collections::cons_builtin(interp, v, colls_seq)?;
                }
                return match uncons(interp, &colls_seq)? {
                    Some((first, rest)) => Ok(concat_lazy_seq(first, rest)),
                    // `(apply concat [])`/`(apply concat)`-shaped: no
                    // collections at all. Route through the SAME `Lazy`
                    // wrapper the bare `(concat)` 0-arity above builds
                    // (`concat_lazy(Nil, [], 0)`), not a raw `Value::Nil`
                    // -- a bare `Nil` prints `nil`, but `(apply concat [])`
                    // must print `()` like `(concat)` itself does
                    // (`(class (apply concat []))` is `LazySeq` on the
                    // oracle, same as `(class (concat))`).
                    None => Ok(concat_lazy(Value::Nil, Arc::from(Vec::new()), 0)),
                };
            }
        }
        if let Some((rc, required, probe_limit)) = interp.variadic_apply_shape(&f) {
            while call_args.len() < probe_limit {
                match uncons(interp, &cur)? {
                    Some((h, t)) => {
                        call_args.push(h);
                        cur = t;
                    }
                    // Short enough to settle the arity outright: fall
                    // through to the ordinary handover below.
                    None => return interp.call_owned(&f, call_args),
                }
            }
            // `probe_limit` arguments in hand AND the seq is not exhausted,
            // so the total exceeds every fixed arity: the variadic one is
            // what `select_arity_index` would pick. Anything realized past
            // `required` belongs to the rest arg, so cons it back on (in
            // reverse, innermost last) instead of dropping it.
            let extra: Vec<Value> = call_args.drain(required..).collect();
            let mut rest = cur;
            for v in extra.into_iter().rev() {
                // `cons_builtin` is the lazy-preserving prepend: a `Lazy`
                // tail becomes the `[head <lazy>]` pair-list `uncons`
                // already understands, never a realized copy.
                rest = crate::builtins::collections::cons_builtin(interp, v, rest)?;
            }
            return interp.apply_closure_lazy_rest(&rc, call_args, rest, crate::reader::Span { start: 0, end: 0 });
        }
        while let Some((h, t)) = uncons(interp, &cur)? {
            call_args.push(h);
            cur = t;
        }
        // `call_args` is this native's own vector, dead after the call
        // (phase 3), so `(apply f xs)` hands its arguments to `f` rather
        // than lending them.
        interp.call_owned(&f, call_args)
    });

    reg(i, "some", ArityHint::Exact(2), |interp, args| {
        let f = args[0].clone();
        let mut cur = args[1].clone();
        while let Some((h, t)) = uncons(interp, &cur)? {
            let r = interp.call(&f, &[h])?;
            if r.truthy() {
                return Ok(r);
            }
            cur = t;
        }
        Ok(Value::Nil)
    });

    reg(i, "every?", ArityHint::Exact(2), |interp, args| {
        let f = args[0].clone();
        let mut cur = args[1].clone();
        while let Some((h, t)) = uncons(interp, &cur)? {
            if !interp.call(&f, &[h])?.truthy() {
                return Ok(Value::Bool(false));
            }
            cur = t;
        }
        Ok(Value::Bool(true))
    });

    reg(i, "not-every?", ArityHint::Exact(2), |interp, args| {
        let f = args[0].clone();
        let mut cur = args[1].clone();
        while let Some((h, t)) = uncons(interp, &cur)? {
            if !interp.call(&f, &[h])?.truthy() {
                return Ok(Value::Bool(true));
            }
            cur = t;
        }
        Ok(Value::Bool(false))
    });

    reg(i, "not-any?", ArityHint::Exact(2), |interp, args| {
        let f = args[0].clone();
        let mut cur = args[1].clone();
        while let Some((h, t)) = uncons(interp, &cur)? {
            if interp.call(&f, &[h])?.truthy() {
                return Ok(Value::Bool(false));
            }
            cur = t;
        }
        Ok(Value::Bool(true))
    });

    reg(i, "sort", ArityHint::Range(1, 2), |interp, args| {
        let (comparator, coll) = if args.len() == 2 {
            (Some(args[0].clone()), &args[1])
        } else {
            (None, &args[0])
        };
        let mut items = materialize(interp, coll)?;
        sort_in_place(interp, &mut items, comparator.as_ref())?;
        // S5/M3: `sort` PRESERVES the collection's metadata even though it
        // returns a freshly-built seq -- measured, `(meta (sort (with-meta
        // [3 1 2] {:a 1})))` is `{:a 1}`. That looks like an exception to
        // the "a rebuild drops metadata" rule, and it is: `clojure.core/
        // sort` is literally written as `(with-meta (seq a) (meta coll))`,
        // putting the metadata back by hand. `sort-by` does the same.
        Ok(Value::List(items.into_iter().collect()).with_meta_of(coll))
    });

    reg(i, "sort-by", ArityHint::Range(2, 3), |interp, args| {
        let keyfn = args[0].clone();
        let (comparator, coll) = if args.len() == 3 {
            (Some(args[1].clone()), &args[2])
        } else {
            (None, &args[1])
        };
        let items = materialize(interp, coll)?;
        let mut pairs: Vec<(Value, Value)> = Vec::with_capacity(items.len());
        for it in items {
            let k = interp.call(&keyfn, std::slice::from_ref(&it))?;
            pairs.push((k, it));
        }
        for idx in 1..pairs.len() {
            let mut j = idx;
            while j > 0 {
                let ord = match &comparator {
                    Some(f) => compare_via(interp, f, &pairs[j - 1].0, &pairs[j].0)?,
                    None => default_compare(&pairs[j - 1].0, &pairs[j].0)?,
                };
                if ord == std::cmp::Ordering::Greater {
                    pairs.swap(j - 1, j);
                    j -= 1;
                } else {
                    break;
                }
            }
        }
        // S5/M3: same hand-written metadata carry-over as `sort` above.
        Ok(Value::List(pairs.into_iter().map(|(_, v)| v).collect()).with_meta_of(coll))
    });

    // C13 (sequences.clj 912/1148): `partition` grew its real 3-arity
    // (`n step coll`, offset-not-equal-to-n windows) and 4-arity (`n step
    // pad coll`, pad the trailing under-sized window) -- both measured
    // present in the vendored corpus (test-partition), previously
    // Exact(2)-only. `n <= 0` (measured: real Clojure's own `(partition -1
    // ..)`/`(partition -2 ..)` are `()`, `(partition 0 ..)` is an
    // INFINITE seq of `nil` -- oracle-confirmed by hand, commented out in
    // the vendored test itself) stays an eager `()` for `n < 0` here (safe
    // -- matches the tested rows) and is left UNHANDLED for `n == 0`
    // (falls out of `require_pos_int` as an error, same as before this
    // change) rather than risk this eager implementation looping forever
    // on the one row real Clojure itself only gets away with via
    // laziness.
    reg(i, "partition", ArityHint::Range(2, 4), |interp, args| {
        let n_val = &args[0];
        if let Value::Int(n) = n_val {
            if *n <= 0 {
                return Ok(Value::List(PVec::new()));
            }
        }
        let n = require_pos_int(n_val, "partition")?;
        let (step, pad, coll) = match args.len() {
            2 => (n, None, &args[1]),
            3 => (require_pos_int(&args[1], "partition")?, None, &args[2]),
            _ => (
                require_pos_int(&args[1], "partition")?,
                Some(materialize(interp, &args[2])?),
                &args[3],
            ),
        };
        let items = materialize(interp, coll)?;
        let mut out = PVec::new();
        let mut idx = 0;
        while idx + n <= items.len() {
            out.push_back(Value::List(items[idx..idx + n].iter().cloned().collect()));
            idx += step;
        }
        // 4-arity only: a trailing under-sized window gets padded (up to
        // `n` total) from `pad`, itself possibly shorter than needed --
        // "return a partition with less than n items" (measured).
        if let Some(pad) = pad {
            if idx < items.len() {
                let mut last: Vec<Value> = items[idx..].to_vec();
                last.extend(pad.into_iter().take(n - last.len()));
                out.push_back(Value::List(last.into_iter().collect()));
            }
        }
        Ok(Value::List(out))
    });

    // C13: renamed from `partition-all` (mova's `core.mova` now owns that
    // name -- see its own `defn partition-all` doc for why: the 1-arity
    // TRANSDUCER form this wave adds needs a `volatile!`-buffered mova
    // closure, and `into`/`take`/`conj`'s own precedent in this same wave
    // is "grow `sequence`'s existing rf-wrapping architecture", not bolt a
    // second one onto a Rust-only collection arity). This native now
    // covers ONLY the collection-realizing arities real Clojure's own
    // `([n coll])`/`([n step coll])` do (`([n step coll])` == `(partition-all
    // n n coll)` when `step` is omitted, handled by mova's dispatcher).
    reg(i, "partition-all-coll*", ArityHint::Range(2, 3), |interp, args| {
        let n = require_pos_int(&args[0], "partition-all")?;
        let (step, coll) = if args.len() == 3 {
            (require_pos_int(&args[1], "partition-all")?, &args[2])
        } else {
            (n, &args[1])
        };
        let items = materialize(interp, coll)?;
        let mut out = PVec::new();
        let mut idx = 0;
        while idx < items.len() {
            let end = (idx + n).min(items.len());
            out.push_back(Value::List(items[idx..end].iter().cloned().collect()));
            idx += step;
        }
        Ok(Value::List(out))
    });

    reg(i, "reverse", ArityHint::Exact(1), |interp, args| {
        let mut items = materialize(interp, &args[0])?;
        items.reverse();
        Ok(Value::List(items.into_iter().collect()))
    });

    // C13: renamed from `distinct` -- mova's `core.mova` now owns that
    // name, adding the missing 0-arity TRANSDUCER form and dispatching
    // 1-arity straight through to here, unchanged. Same shape as
    // `take-coll*`/`drop-coll*` above.
    reg(i, "distinct-coll*", ArityHint::Exact(1), |interp, args| {
        let items = materialize(interp, &args[0])?;
        let mut out: Vec<Value> = Vec::new();
        'items: for item in items {
            for seen in &out {
                if interp.values_equal(seen, &item)? {
                    continue 'items;
                }
            }
            out.push(item);
        }
        Ok(Value::List(out.into_iter().collect()))
    });

    // W4-PRINTER (collections.corpus:138, uncovered by the print-
    // namespace-maps ordering fix in `printer.rs`): builds the result
    // directly in a `PMap`, walking `items` in order and `insert`ing each
    // NEW key as it's first seen -- NOT via an `imbl::HashMap`
    // intermediate (the pre-existing shape, sorted arbitrarily by hash
    // bucket), because `printer.rs`'s `Value::Map` arm now preserves a
    // `PMap::Small` map's genuine INSERTION order when printing (matching
    // real Clojure's own `PersistentArrayMap`) rather than silently
    // re-sorting it -- so an accumulator that scrambles insertion order
    // now prints wrong, where it used to be masked by that resort.
    // Measured: `(group-by identity [1 1.0 2])` is `{1 [1], 1.0 [1.0], 2
    // [2]}`, first-occurrence order, not hash order.
    reg(i, "group-by", ArityHint::Exact(2), |interp, args| {
        let f = args[0].clone();
        let items = materialize(interp, &args[1])?;
        let mut m = PMap::new();
        for item in items {
            let k = interp.call(&f, std::slice::from_ref(&item))?;
            let mut bucket = match m.get(&k) {
                Some(Value::Vector(v)) => v.clone(),
                _ => PVec::new(),
            };
            bucket.push_back(item);
            m.insert(k, Value::Vector(bucket));
        }
        Ok(Value::Map(m))
    });

    // W4-PRINTER: same fix, same reason as `group-by` just above --
    // `imbl::HashMap` -> `PMap` directly, preserving first-occurrence
    // insertion order. Measured: `(frequencies [1 1.0 1])` is `{1 2, 1.0
    // 1}` (key `1` counted first, `1.0` a later, DIFFERENT key -- real
    // Clojure's map keys are NOT unified across numeric types the way
    // `=` is), not hash order.
    reg(i, "frequencies", ArityHint::Exact(1), |interp, args| {
        let items = materialize(interp, &args[0])?;
        let mut m = PMap::new();
        for item in items {
            let c = match m.get(&item) {
                Some(Value::Int(c)) => *c,
                _ => 0,
            };
            m.insert(item, Value::Int(c + 1));
        }
        Ok(Value::Map(m))
    });

    reg(i, "vec", ArityHint::Exact(1), |interp, args| {
        Ok(Value::Vector(materialize(interp, &args[0])?.into_iter().collect()))
    });

    // `(butlast coll)` => `nil` for an empty or single-element coll,
    // otherwise every element except the last, as a `List` (matches
    // Clojure: always a seq, never the input's own collection type).
    reg(i, "butlast", ArityHint::Exact(1), |interp, args| {
        let items = materialize(interp, &args[0])?;
        if items.len() <= 1 {
            return Ok(Value::Nil);
        }
        Ok(Value::List(items[..items.len() - 1].iter().cloned().collect()))
    });

    reg(i, "take-last", ArityHint::Exact(2), |interp, args| {
        let n = match &args[0] {
            Value::Int(n) => *n,
            other => {
                return Err(RjError::type_err(format!(
                    "take-last: expected an int, got {}",
                    other.type_name()
                )))
            }
        };
        // Clojure: `n <= 0` (or an empty coll) is `nil`, not `()` -- unlike
        // `take`, which always returns a (possibly empty) seq.
        let items = materialize(interp, &args[1])?;
        if n <= 0 || items.is_empty() {
            return Ok(Value::Nil);
        }
        let n = (n as usize).min(items.len());
        Ok(Value::List(items[items.len() - n..].iter().cloned().collect()))
    });

    // W-REDUCE Part A: capture the boot `+`/`*`/`min`/`max` natives'
    // `Arc<NativeFn>` identities now, while they are guaranteed to still
    // be the pristine boot registration -- `numbers::register` (which
    // installs all four) already ran, per `builtins::mod::register_all`'s
    // fixed order, and no user code has had a chance to `def`/shadow them
    // yet. See `cache_boot_arith`/`BootArith`'s own docs.
    cache_boot_arith(i);
}

/// `range`'s bound-checking: any rank `is_numeric_value` recognizes
/// (`Int`/`Float`/`BigInt`/`BigInteger`/`Ratio`/`BigDec`) is accepted and
/// handed back unchanged -- the `range` registration above decides
/// separately whether the whole call stays on the `i64` fast path or
/// falls to [`range_lazy_tower`].
fn require_range_num(v: &Value) -> Result<Value, RjError> {
    if is_numeric_value(v) {
        Ok(v.clone())
    } else {
        Err(RjError::type_err(format!(
            "range: expected a number, got {}",
            v.type_name()
        )))
    }
}
