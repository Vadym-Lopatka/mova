//! `vector list hash-map hash-set conj assoc dissoc get nth count contains?
//! keys vals first rest cons seq into empty? peek pop subvec update
//! assoc-in get-in update-in` (+ `empty`, `next`). `list` itself is already
//! registered as part of numbers.rs's P2 seed set, so it isn't repeated here.
//!
//! R4 adds the hot-path collection ops (`merge merge-with select-keys disj
//! mapv filterv reduce-kv`, the last over BOTH maps and vectors) plus
//! `clojure.set`'s `difference union intersection` (registered bare, and
//! aliased under `clojure.set/`+`set/` via strings.rs's `alias` helper --
//! same registration precedent `clojure.string`'s aliases use).
//!
//! M3 completes `clojure.set`: `subset? superset? select project rename
//! rename-keys map-invert index join` (both `join` arities), same bare +
//! `clojure.set/`/`set/`-aliased registration. Every function was checked
//! against the pinned 1.13.0-alpha6 `clojure/set.clj` source rather than
//! recalled/idealized -- see each registration's own comment for the exact
//! ground-truth clause and the measured non-set/non-map edge case it
//! drives (`select`'s nil-passthrough, `rename-keys`'s nil collapse,
//! `map-invert`'s iteration-order collision winner, `join`'s cartesian-
//! product-on-no-common-keys degeneration).
//!
//! ## The lazy/eager seq protocol (read this before touching `uncons`)
//!
//! mova's `Value` has no dedicated cons-cell type — lazy sequences are
//! encoded as a realized prefix plus a marked continuation slot. The
//! convention used throughout `collections.rs` and `seq.rs` (and by
//! extension, every `lazy-seq`/`cons` combination in `core/core.mova`) is:
//!
//! - A **`Value::List` whose LAST slot is `Value::LazyTail`** encodes a
//!   cons cell (or, for the native chunk generators, a whole realized
//!   chunk): `[e0 .. en, <rest>]` where `<rest>` is still unforced. This
//!   is exactly what `cons_builtin` produces when its second argument is
//!   `Value::Lazy` (see below) and what `seq.rs`'s `gen_chunk` produces,
//!   and it is exactly what `Interp::force`'s memoization contract allows
//!   (nested lazy slots inside a returned `List` are *not* forced by
//!   `force` itself — it only forces the top-level value — so they stay
//!   lazy until something consumes them via `uncons`).
//! - Anything else (an ordinary flat `List`/`Vector`/`Map`/`Set`/`Str`, or
//!   `Nil`) is eager data, consumed head-by-head via `pop_front`. **A
//!   plain `Value::Lazy` sitting in a list slot is DATA**, never a
//!   continuation: `(list 1 (range 3))` and `(seq [:x (range 2 5)])` are
//!   two-element sequences whose second element happens to be a lazy seq.
//!
//! `lazy_tail_split` is the single definition of that rule and `uncons`
//! the single place that peels it; every iterative consumer
//! (`first`/`rest`/`next`/`seq`/`count`/`into`, and all of `seq.rs`'s
//! `reduce`/`take`/`drop`/`doall`/`sort`/etc.) is built as a Rust
//! `while`/`loop` around `uncons`, which is what keeps per-element Rust
//! stack usage O(1) even for sequences far longer than `MAX_CALL_DEPTH`.
//!
//! C3e retired the v0 tradeoff this convention used to carry (the
//! continuation slot held a bare `Value::Lazy`, so a list whose last
//! element genuinely was a lazy seq got misread as a cons cell, and
//! conversely a genuine cons cell had to be misprinted as data because
//! `realize_deep` could not tell which it was holding). The marker now
//! has its own discriminant; see `Value::LazyTail`'s doc.

use crate::builtins::strings::alias;
use crate::builtins::{
    map_probe, reg, reg_consuming_preserving_meta, reg_preserving_meta, reg_unmeta,
    ArityHint,
};
use crate::error::{JvmClass, RjError};
use crate::eval::Interp;
use crate::pvec;
use crate::value::{Keyword, PMap, PVec, Value};

/// **THE improper-list rule**, defined in exactly ONE place so that all
/// three seq walkers in the codebase agree on it: [`uncons`] (single-step
/// peel), [`materialize`] (full realization), and `eval::Interp::
/// seq_items` (shape conversion). Given the slots of a `Value::List`, it
/// answers: *does this list end in a lazy continuation, and if so how many
/// leading slots are real data?*
///
/// The rule: **a `Value::List` whose LAST slot is [`Value::LazyTail`]
/// carries that slot as the REST OF THE SEQUENCE, not as an element.**
/// Everything else is plain data -- and, since C3e, that includes every
/// list whose last slot is an ordinary `Value::Lazy`, which is exactly
/// what `(seq [:x (range 2 5)])`/`(list 1 (range 3))` build.
///
/// C11: before this helper existed the rule was open-coded in `uncons`
/// (as the narrower `len == 2` case its incremental peel reduces to) and
/// in `materialize`, while `seq_items` did not implement it at ALL --
/// it returned the raw slots, so every caller that asked it for the
/// ELEMENTS of a lazy seq got `[head, <unforced tail>]`: exactly two
/// items, the second of which prints as a nested list of everything
/// after the head. That is the measured `unquote-splicing` double-wrap
/// (`` `(do ~@(map f xs)) `` => `(do (f x0) ((f x1) (f x2)))`) this
/// helper's single definition fixes; see `compat/qq-probe.clj`.
///
/// C3e made the marker a distinct discriminant instead of an overloaded
/// `Value::Lazy` slot (see [`Value::LazyTail`]'s doc for the full
/// argument and the measured leaks it retires). Two accidental
/// complications died with the overloading:
///
/// * the `>= 2 slots` guard is now a pure construction invariant, not a
///   disambiguation device (nothing builds a 1-slot `[LazyTail]`), and
/// * the C7 `Value::Vector` exemption is gone -- a vector can no more
///   contain a `LazyTail` than a map can, so `Vector` needs no rule of
///   its own and `seq`'s Vector->List conversion no longer destroys the
///   guarantee.
pub(crate) fn lazy_tail_split(items: &PVec) -> Option<usize> {
    let n = items.len();
    if n >= 2 && !items.is_col() && matches!(&items[n - 1], Value::LazyTail(_)) {
        Some(n - 1)
    } else {
        None
    }
}

/// Peels the internal [`Value::LazyTail`] marker back off a continuation
/// slot, yielding the ordinary `Value::Lazy` that user code sees as "the
/// rest of the sequence". A pointer clone, exactly what cloning the slot
/// cost before C3e. Non-marker values pass through untouched, so this is
/// safe to apply to any tail slot.
#[inline]
fn untag_tail(v: &Value) -> Value {
    match v {
        Value::LazyTail(cell) => Value::Lazy(cell.clone()),
        other => other.clone(),
    }
}

/// C3e: the LOOKUP half of [`Interp::normalize_key`], shaped so a hash
/// probe pays for it only after a MISS. `Some(k)` is a realized key worth
/// re-probing with; `None` means "the probe was not lazy, so the miss you
/// already have is final". Every hot lookup (keyword, string, int keys)
/// returns `None` after a single discriminant compare -- `#[inline]` so
/// that compare lands at the call site instead of behind a call.
#[inline]
fn normalized_probe(interp: &mut Interp, k: &Value) -> Result<Option<Value>, RjError> {
    if matches!(k, Value::Lazy(_)) {
        return Ok(Some(interp.normalize_key(k.clone())?));
    }
    Ok(None)
}

/// Wraps a continuation in the internal [`Value::LazyTail`] marker before
/// it is parked in a list's last slot. The inverse of [`untag_tail`], and
/// the ONLY constructor of the marker outside `builtins::seq::gen_chunk`.
#[inline]
pub(crate) fn tag_tail(v: Value) -> Value {
    match v {
        Value::Lazy(cell) => Value::LazyTail(cell),
        other => other,
    }
}

/// Single-step sequence peel: `Nil` -> `None`; otherwise `Some((head,
/// tail))`. Forces at most one `Lazy` cell per call (bounded, since
/// `Interp::force` never itself returns a bare unforced `Lazy` — see the
/// module doc), so callers driving this from a Rust `while`/`loop` get O(1)
/// stack per consumed element regardless of how many elements they walk.
pub(crate) fn uncons(interp: &mut Interp, v: &Value) -> Result<Option<(Value, Value)>, RjError> {
    match v {
        // S5/M3: unconsing is a READ -- it sees through metadata, and the
        // TAIL it produces carries none (measured: `(meta (rest (with-meta
        // [1 2] {:a 1})))` is `nil`). This one arm is what makes every
        // seq-walking builtin in the codebase accept a metadata-carrying
        // collection, since they all bottom out here.
        Value::Meta(m) => uncons(interp, &m.inner),
        Value::Nil => Ok(None),
        // C3e: a `LazyTail` reaching here as a whole value (rather than
        // in a list's last slot) is not a shape any construction site
        // produces, but forcing it exactly like the `Lazy` it wraps is
        // both the only sensible reading and free -- one more pattern on
        // an arm that already exists.
        Value::Lazy(_) | Value::LazyTail(_) => {
            let forced = interp.force(v)?;
            uncons(interp, &forced)
        }
        // S7: a map entry gets its OWN arm rather than joining the
        // `List` one directly below, and the difference is
        // load-bearing: that arm reads a 2-element collection whose second
        // slot is a `Lazy` as a CONS CELL (mova's lazy-seq representation),
        // which for an entry is simply wrong -- `(first {:a (lazy-seq
        // [1])})` is the two-element entry `[:a (1)]`, not a cons of `:a`
        // onto a lazy tail. `PVec::pair`'s invariant guarantees len 2, so
        // there is no empty case either.
        Value::MapEntry(items) => {
            let mut rest = items.clone();
            let head = rest.pop_front().expect("MapEntry always holds exactly two elements");
            Ok(Some((head, Value::List(rest))))
        }
        // C3e: the `[.., LazyTail]` improper-list shape is an INTERNAL
        // encoding -- `cons_builtin`/`gen_chunk` (this file /
        // `builtins::seq`) are the only two places that ever construct
        // it, and both always build a `Value::List`. Nothing a reader,
        // a literal, or any other builtin produces can carry the marker,
        // which is why `Vector`/`MapEntry`/`Queue` below need no rule of
        // their own any more (before C3e the marker was an untagged
        // `Value::Lazy` slot, so `Vector` had to be exempted BY HAND to
        // keep `(count [1 (range 5)])` at `2` -- and the exemption was
        // lost the moment `seq` turned that vector into a `List`).
        Value::List(items) => {
            if items.is_empty() {
                Ok(None)
            } else if lazy_tail_split(items) == Some(1) {
                // C11: the shared rule at `len == 2` -- the shape every
                // longer improper list reduces to as the `pop_front`
                // branch below peels it down, so peeling incrementally
                // and applying `lazy_tail_split` at every step agree.
                // C3e: the marker comes OFF here -- what the caller gets
                // back as "the rest" is an ordinary `Value::Lazy`, so the
                // marker never escapes into user-visible territory.
                Ok(Some((items[0].clone(), untag_tail(&items[1]))))
            } else {
                let mut rest = items.clone();
                let head = rest.pop_front().expect("checked non-empty above");
                Ok(Some((head, Value::List(rest))))
            }
        }
        Value::Vector(items) => {
            if items.is_empty() {
                Ok(None)
            } else {
                let mut rest = items.clone();
                let head = rest.pop_front().expect("checked non-empty above");
                Ok(Some((head, Value::List(rest))))
            }
        }
        // S4: a typed vector unconses exactly like a plain `Vector` -- the
        // TAIL is an ordinary untyped `List` (measured: `(next (vector-of
        // :int 1 2 3))` is a plain seq, not another typed vector; only the
        // whole-vector value itself stays typed).
        Value::TypedVec(tv) => {
            if tv.data.is_empty() {
                Ok(None)
            } else {
                let mut rest = tv.data.clone();
                let head = rest.pop_front().expect("checked non-empty above");
                Ok(Some((head, Value::List(rest))))
            }
        }
        // C7 (vecveneer): unconses like a plain `List` -- but the TAIL,
        // while non-empty, stays a `VecSeq` of the SAME `kind` (measured:
        // real `(rest (.rseq v))` stays an `RSeq` for as long as it has
        // elements; only the truly-empty tail collapses to `Nil`, since
        // there is no such thing as an empty `RSeq`/`VecSeq` on the real
        // JVM -- `ISeq`s are never empty, `nil` stands in for "no more").
        Value::VecSeq(vs) => {
            if vs.items.is_empty() {
                Ok(None)
            } else {
                let mut rest = vs.items.clone();
                let head = rest.pop_front().expect("checked non-empty above");
                if rest.is_empty() {
                    Ok(Some((head, Value::Nil)))
                } else {
                    Ok(Some((
                        head,
                        Value::VecSeq(std::sync::Arc::new(crate::value::VecSeqVal {
                            kind: vs.kind,
                            items: rest,
                        })),
                    )))
                }
            }
        }
        Value::Map(_)
        | Value::Set(_)
        | Value::Str(_)
        | Value::HostStruct(_) | Value::LazyMap(_)
        | Value::Inst(_)
        | Value::SortedMap(_)
        | Value::SortedSet(_)
        | Value::StructMap(_)
        | Value::Array(_)
        // C10: `java.util.ArrayList`/`HashMap`/`HashSet` -- see
        // `Interp::seq_items`'s own `Value::HostInst` arm, which does the
        // actual dispatch on `HostState`.
        | Value::HostInst(_)
        // C10: a queue unconses front-to-back, tail collapses to a plain
        // `List` -- measured, `(rest (conj EMPTY 1 2 3))` is `(2 3)`, NOT
        // another queue.
        | Value::Queue(_) => match interp.seq_items(v)? {
            None => Ok(None),
            Some(items) => {
                let mut rest = items;
                let head = rest.pop_front().expect("seq_items only returns Some for non-empty");
                Ok(Some((head, Value::List(rest))))
            }
        },
        // W3a: `IllegalArgumentException`, measured -- see the matching
        // note on `Interp::seq_items`'s own arm in `eval::mod`.
        other => Err(RjError::type_err(format!(
            "don't know how to create a seq from {}",
            other.type_name()
        ))
        .with_class(JvmClass::IllegalArgument)),
    }
}

/// The `count` builtin's whole body, extracted (C10) so `.size` -- the
/// `is-same-collection` dot-method veneer's own name for the same
/// operation, `data_structures.clj`'s `test-count`/`test-map-entry?`-
/// adjacent helper -- calls the EXACT same logic instead of re-deriving
/// it in `eval::types_forms::eval_dot_form`.
pub(crate) fn count_value(interp: &mut Interp, v: &Value) -> Result<i64, RjError> {
    Ok(match v {
        Value::Nil => 0,
        // C7 (vecveneer): a `Vector` never carries the internal lazy-tail
        // marker (see `uncons`'s doc) -- always O(1) `.len()`.
        Value::Vector(items) => items.len() as i64,
        // S7: unconditionally 2 -- an entry is never a cons cell.
        // Measured: `(count (first {:a 1}))` => `2`.
        Value::MapEntry(_) => 2,
        // C10: `(count clojure.lang.PersistentQueue/EMPTY)` => `0`,
        // `(count (into EMPTY [:a :b]))` => `2` -- O(1), same as `Vector`.
        Value::Queue(items) => items.len() as i64,
        Value::Map(m) => m.len() as i64,
        // O(1), no materialize -- see `host_struct::count`'s doc.
        Value::HostStruct(hs) => crate::host_struct::count(hs) as i64,
        Value::LazyMap(hs) => crate::lazy_map::count(hs) as i64,
        Value::Set(s) => s.len() as i64,
        // Cached (see `value::Str::char_count_cached`) -- ASCII strings
        // count in O(1) via len, non-ASCII strings compute once and
        // reuse the cached count on every subsequent `count` call.
        Value::Str(s) => s.char_count_cached() as i64,
        // S3: records count their full map view; a deftype falls to
        // the uncons walk below, whose seq_items error matches the
        // measured "count throws" (kind-blind until M8).
        Value::Inst(inst) if inst.tdef.is_record => inst.data.len() as i64,
        // S4/1D: `alength`'s O(1) length, not a seq walk (measured:
        // `(count (into-array []))` 0, `(count (into-array [1 2 3]))`
        // 3).
        Value::Array(arr) => crate::sync::lock_mutex(&arr.data).len() as i64,
        // S4
        Value::SortedMap(m) => m.entries.len() as i64,
        Value::SortedSet(s) => s.entries.len() as i64,
        Value::TypedVec(tv) => tv.data.len() as i64,
        // C2 (defstruct): full entry count (basis + ext), like a plain
        // map's `.len()`.
        Value::StructMap(m) => m.entries.len() as i64,
        // C10: `java.util.ArrayList`/`HashMap`/`HashSet` -- O(1),
        // straight off the backing store, same shape as `Array` above.
        Value::HostInst(h) => {
            let guard = crate::sync::lock_mutex(&h.state);
            match &*guard {
                crate::hostclass::HostState::ArrayList(items) => items.len() as i64,
                crate::hostclass::HostState::HashMap(m) => m.len() as i64,
                crate::hostclass::HostState::HashSet(s) => s.len() as i64,
                _ => {
                    return Err(RjError::type_err(format!(
                        "count not supported on this type: {}",
                        h.kind.diagnostic_name()
                    )))
                }
            }
        }
        _ => {
            let mut n = 0i64;
            let mut cur = v.clone();
            while let Some((_, t)) = uncons(interp, &cur)? {
                n += 1;
                cur = t;
            }
            n
        }
    })
}

/// Fully walks `v` (iteratively, via [`uncons`]) into a flat `Vec<Value>`.
/// Used by anything that needs the whole (necessarily finite) collection at
/// once: `sort`/`sort-by`, `partition`/`partition-all`, `distinct`,
/// `group-by`, `frequencies`, `vec`, `join`.
// C11: `pub(crate)` (was `pub(super)`) -- `eval::Interp::seq_items` now
// realizes improper lists through this exact walker rather than
// re-deriving one, so all three seq walkers share one implementation of
// the rule (see `lazy_tail_split`).
pub(crate) fn materialize(interp: &mut Interp, v: &Value) -> Result<Vec<Value>, RjError> {
    let mut out = Vec::new();
    let mut cur = v.clone();
    loop {
        // W4 diet: the moment the walk reaches a PLAIN concrete
        // `List`/`Vector` tail (including the very first step, e.g.
        // `flow/inject`'s messages), bulk-copy the remaining elements by
        // iteration instead of continuing the `uncons` walk. `uncons` on a
        // vector-backed tail is `clone` + `pop_front` while `cur` still
        // holds the un-popped value, so the two handles force `imbl`'s
        // `make_mut` to COPY the front chunk (~4.5KB) on EVERY element --
        // the W4 census measured exactly 1.0 such >4K alloc + memcpy per
        // element on `bench/flow-sink.mova`'s 1.6M-message injection (that
        // bench's actual bottleneck). Iteration allocates nothing.
        //
        // "PLAIN" means not ending in a `LazyTail`. The engine's improper-
        // seq convention (see `lazy_tail_split`, the one definition of the
        // rule -- e.g. a forced `range` chunk is
        // `List[e0..e1023, LazyTail]`) is: a `Value::List` whose LAST slot
        // is the marker carries that slot as the rest of the sequence, not
        // as data. So: bulk-copy every proper element, and if a marked
        // tail ends the segment, continue the walk from it.
        match &cur {
            // A `Vector` can no more hold the marker than a map can (C3e:
            // the marker has its own discriminant and only `cons_builtin`/
            // `gen_chunk` build it, both into `List`s) -- bulk-copy every
            // element unconditionally.
            Value::Vector(items) => {
                out.reserve(items.len());
                out.extend(items.iter_cloned());
                return Ok(out);
            }
            // C3e: the `!(len == 1 && Lazy)` guard this arm used to carry
            // is gone -- it existed only to route a single-slot `[Lazy]`
            // list (data, not a continuation, but indistinguishable from
            // one under the old encoding) to the `uncons` fallback, which
            // produced the identical answer anyway. A `Lazy` slot is now
            // unambiguously an element and bulk-copies like any other.
            Value::List(items) => {
                let n = items.len();
                // C11: the shared rule -- see `lazy_tail_split`'s doc.
                let split = lazy_tail_split(items);
                let proper = split.unwrap_or(n);
                out.reserve(proper);
                out.extend(items.iter_cloned().take(proper));
                if split.is_none() {
                    return Ok(out);
                }
                cur = untag_tail(&items[n - 1]);
                continue;
            }
            _ => {}
        }
        match uncons(interp, &cur)? {
            Some((h, t)) => {
                out.push(h);
                cur = t;
            }
            None => return Ok(out),
        }
    }
}

/// `(cons x coll)`: if `coll` is `Nil`, a fresh single-element list; if
/// `coll` is `Lazy`, a lazy 2-element cons cell (preserves laziness -- this
/// is how idiomatic `(lazy-seq (cons x (more-lazy-stuff)))` patterns in
/// `core/core.mova` stay genuinely lazy); otherwise `coll` is eager, so it's
/// seq'd and flattened immediately (matches Clojure: `(cons 1 [2 3])` =>
/// `(1 2 3)`).
///
/// v0.5 / perf: `List`/`Vector` get their own arm -- `items.clone()` is O(1)
/// structural sharing (`imbl`), and `push_front` only touches the front
/// chunk, so prepending one element is O(1)-ish regardless of `coll`'s
/// length. The old code routed every case through `seq_items` + `extend`,
/// which drains the WHOLE collection element-by-element
/// (`pop_front`/`Arc::make_mut`, cloning chunks) to rebuild it one element
/// longer -- fine for `Map`/`Set`/`Str` (still routed through `seq_items`
/// below, unchanged), but `cons` onto an already-materialized list is a
/// hot path in `core.mova`'s lazy `map`/`filter` expansion, where it was
/// contributing to the O(n^2) file-open wall alongside `seq`'s identical
/// mistake (see that builtin's own doc comment).
// C7 (vecveneer): `pub(crate)` (not private) -- `builtins::vecdot`'s
// `.cons` dot-method delegates here directly, same "one implementation,
// two call shapes" rationale `str_dot_method`/`vec_dot_method` follow
// elsewhere rather than re-deriving `cons`'s prepend logic.
pub(crate) fn cons_builtin(interp: &mut Interp, head: Value, coll: Value) -> Result<Value, RjError> {
    match &coll {
        Value::Nil => Ok(Value::List(pvec![head])),
        // C3e: the cons cell's tail slot carries the `LazyTail` MARKER,
        // not the bare `Value::Lazy` -- that one wrap (a pointer clone,
        // see `tag_tail`) is what tells every later reader that this slot
        // is the rest of the sequence rather than an element that happens
        // to be a lazy seq. `(cons 1 (range 3))` => `(1 0 1 2)`;
        // `(list 1 (range 3))` => `(1 (0 1 2))`.
        Value::Lazy(_) => Ok(Value::List(pvec![head, tag_tail(coll.clone())])),
        // S7: `(cons x (first {:a 1}))` is a plain 3-element seq, exactly
        // as `(cons x [:a 1])` is.
        Value::List(items) | Value::Vector(items) | Value::MapEntry(items) => {
            let mut v = items.clone();
            v.push_front(head);
            Ok(Value::List(v))
        }
        _ => match interp.seq_items(&coll)? {
            None => Ok(Value::List(pvec![head])),
            Some(items) => {
                let mut v = pvec![head];
                v.extend(items);
                Ok(Value::List(v))
            }
        },
    }
}

/// `(seq coll)`: `nil`/empty -> `nil`; otherwise a seq over `coll`'s
/// elements. `List`/`Vector` -- including the `[head Lazy]` 2-element
/// pair-list form `cons_builtin`/`uncons` use to represent a realized head
/// in front of a still-lazy tail -- are handled WITHOUT `uncons`: a
/// non-empty `List` already IS a seq (Clojure's own `(seq lst)` returns
/// `lst` itself), and a `Vector`'s `imbl::Vector` becomes a `List`'s in O(1)
/// via structural-sharing `clone()`. `Map`/`Set`/`Str` keep going through
/// `seq_items` (their own uncons-shaped conversion), unchanged.
///
/// v0.5 / perf: the previous implementation always went through `uncons`
/// (which pops the head off, chunk-cloning the front) and then rebuilt the
/// list by re-prepending that same head via `extend` -- a full O(remaining)
/// rebuild on every call for no reason once the collection is already a
/// `List`/`Vector`. `core.mova`'s lazy `map`/`filter` (`(lazy-seq (when-let
/// [s (seq coll)] (cons (f (first s)) (map f (rest s)))))`) calls `seq` at
/// EVERY step of the expansion, so that O(remaining) rebuild was O(n) per
/// step, O(n^2) total over an n-element source -- the dominant cost (per
/// `sample`) of the file-open scaling wall alongside `cons_builtin`'s
/// identical mistake (see that fn's own doc comment).
/// C11: `pub(crate)` and named `seq_of` (was the private `seq_builtin`) --
/// `builtins::seq`'s `drop` canonicalizes through it rather than through
/// `Interp::seq_items`, which now realizes improper lists. See the call
/// site's comment.
pub(crate) fn seq_of(interp: &mut Interp, v: &Value) -> Result<Value, RjError> {
    match v {
        // S5/M3: `seq` normally DROPS metadata, because it builds a fresh
        // seq -- `(meta (seq (with-meta [1 2] {:a 1})))` is `nil`,
        // measured. The exception is a non-empty LIST, which in Clojure
        // already IS an `ISeq`, so `seq` hands back the identical object
        // and its metadata rides along: `(meta (seq (with-meta (list 1 2)
        // {:a 1})))` is `{:a 1}`, also measured. The arm below reproduces
        // that by re-attaching exactly in the case where the inner call
        // returned the receiver unchanged.
        Value::Meta(m) => {
            let seqd = seq_of(interp, &m.inner)?;
            Ok(if seqd == m.inner && matches!(&m.inner, Value::List(_)) {
                v.clone()
            } else {
                seqd
            })
        }
        Value::Nil => Ok(Value::Nil),
        Value::List(items) => {
            if items.is_empty() {
                Ok(Value::Nil)
            } else {
                Ok(v.clone())
            }
        }
        Value::Vector(items) => {
            if items.is_empty() {
                Ok(Value::Nil)
            } else {
                Ok(Value::List(items.clone()))
            }
        }
        // S7 (measured): `(seq (first {:a 1}))` is `(:a 1)` -- an entry
        // seqs out as its two slots, never empty.
        Value::MapEntry(items) => Ok(Value::List(items.clone())),
        Value::Lazy(_) => {
            let forced = interp.force(v)?;
            seq_of(interp, &forced)
        }
        // W4C-NS (data_structures.clj phantom-pass purge): a typed
        // vector's seq is a REAL `clojure.core.Vec` (`Chunked` `VecSeq`),
        // not a plain list -- measured `(class (seq (vector-of :long 2 3
        // 4)))` => `clojure.core.VecSeq` (`compat/w4c-collection-oracle-
        // transcript.txt`), and (unlike every other seqable shape here)
        // `(instance? java.util.Collection ...)` on that specific class is
        // `false` (`clojure.core.VecSeq` implements `ISeq`/`Sequential`
        // but not `java.util.Collection`), while a plain `Value::List`
        // answers `true`. Returning `Value::List` here (the previous
        // shape) made `is_java_collection` misclassify it, inflating
        // `data_structures.clj`'s `ordered-collection-equality-test` by
        // 10 phantom-passing assertions. `Chunked`-kind `VecSeq` is
        // already a fully general seq value elsewhere in this module
        // (`uncons`'s own arm just above preserves `kind` through
        // `rest`/`next`), so this is a representation correction, not a
        // new capability.
        Value::TypedVec(tv) => {
            if tv.data.is_empty() {
                Ok(Value::Nil)
            } else {
                Ok(Value::VecSeq(std::sync::Arc::new(crate::value::VecSeqVal {
                    kind: crate::value::VecSeqKind::Chunked,
                    items: tv.data.clone(),
                })))
            }
        }
        // C7 (vecveneer): `(seq (seq x))` is `(seq x)` for a real `ISeq`
        // (measured, and matches this fn's own doc: a non-empty `List`
        // "already IS a seq") -- a non-empty `VecSeq` is likewise handed
        // back unchanged; an empty one (`items` drained down by `rest`
        // until nothing is left) is `Nil`, though in practice `uncons`
        // above never LEAVES a `VecSeq` empty (it collapses straight to
        // `Nil` once its last element is popped), so this arm only ever
        // sees a non-empty `VecSeq` -- the emptiness check is here purely
        // for defensiveness, matching every other arm's shape.
        Value::VecSeq(vs) => {
            if vs.items.is_empty() {
                Ok(Value::Nil)
            } else {
                Ok(v.clone())
            }
        }
        Value::Map(_)
        | Value::Set(_)
        | Value::Str(_)
        | Value::HostStruct(_) | Value::LazyMap(_)
        | Value::Inst(_)
        | Value::SortedMap(_)
        | Value::SortedSet(_)
        | Value::StructMap(_)
        | Value::Array(_)
        // C10: `java.util.ArrayList`/`HashMap`/`HashSet` -- see
        // `Interp::seq_items`'s own `Value::HostInst` arm, which does the
        // actual dispatch on `HostState`.
        | Value::HostInst(_)
        // C10: `(seq (conj EMPTY 1 2 3))` is `(1 2 3)`, a plain `List`.
        | Value::Queue(_) => match interp.seq_items(v)? {
            None => Ok(Value::Nil),
            Some(items) => Ok(Value::List(items)),
        },
        // W3a: `IllegalArgumentException`, measured -- see the matching
        // note on `Interp::seq_items`'s own arm in `eval::mod`.
        other => Err(RjError::type_err(format!(
            "don't know how to create a seq from {}",
            other.type_name()
        ))
        .with_class(JvmClass::IllegalArgument)),
    }
}

/// `conj` semantics for a single extra item, dispatched on the collection
/// shape being conj'd onto (`nil` behaves like an empty list, per Clojure).
/// Takes `&mut Interp` (not just for consistency: S4's `SortedMap`/
/// `SortedSet` arms need it to call through a `sorted-*-by` comparator fn --
/// see `builtins::sorted::cmp_via`).
pub(crate) fn conj_one(interp: &mut Interp, coll: &Value, item: &Value) -> Result<Value, RjError> {
    match coll {
        // S5/M3: `conj` is an UPDATE on the receiver, so the result keeps
        // the receiver's metadata (measured: `(meta (conj (with-meta [1]
        // {:a 1}) 2))` is `{:a 1}`). `into` and `merge` inherit this for
        // free -- both are conj folds over this same function.
        Value::Meta(m) => Ok(conj_one(interp, &m.inner, item)?.with_meta_of(coll)),
        Value::Nil => Ok(Value::List(pvec![item.clone()])),
        // S4
        Value::SortedMap(m) => crate::builtins::sorted::sorted_map_conj(interp, m, item),
        Value::SortedSet(s) => crate::builtins::sorted::sorted_set_conj(interp, s, item),
        // C2 (defstruct): stays a StructMap, unlike `HostStruct`'s v1
        // widen-to-Map policy (measured).
        Value::StructMap(sm) => crate::builtins::structmap::struct_map_conj(sm, item),
        Value::TypedVec(tv) => crate::builtins::sorted::typed_vec_conj(tv, item),
        Value::List(items) => {
            let mut v = items.clone();
            v.push_front(item.clone());
            Ok(Value::List(v))
        }
        // SPEC-PORT: a SEQ is a conj-able collection on the JVM and, like
        // a list, grows at the FRONT -- measured on 1.13.0-alpha6:
        // `(conj (map inc [1 2]) 0)` => `(0 2 3)`, `(into (map inc [1 2])
        // [9])` => `(9 2 3)`. `cons_builtin` is exactly that prepend and
        // it stays LAZY (`(take 3 (conj (range) :x))` must not realize an
        // infinite source), so this arm delegates rather than
        // materializing. Before it, every seq-shaped receiver fell
        // through to the "not a collection" error at the bottom --
        // `clojure.spec.alpha`'s `s/keys` generator is `(into reqs opts)`
        // over two `map` results and so could not generate at all. The
        // one measured difference left is the RESULT class
        // (`clojure.lang.Cons` on the JVM), which mova's own `cons`
        // already spells `PersistentList`; this arm inherits that
        // pre-existing, documented spelling rather than adding a second.
        Value::Lazy(_) | Value::LazyTail(_) | Value::VecSeq(_) => {
            cons_builtin(interp, item.clone(), coll.clone())
        }
        // S7: `Value::Vector(..)` on the RESULT side is the promotion
        // rule, not an oversight -- measured, `(class (conj (first {:a 1})
        // :x))` is `clojure.lang.PersistentVector`, never `MapEntry`. Every
        // other growing/shrinking op below (`assoc`, `pop`, `subvec`,
        // `update`) promotes for the same measured reason.
        Value::Vector(items) | Value::MapEntry(items) => {
            crate::metrics::count(crate::metrics::CONJ, false);
            let mut v = items.clone();
            v.push_back(item.clone());
            Ok(Value::Vector(v))
        }
        // C10: FIFO -- `conj` grows a queue at the BACK (opposite end from
        // `List`'s `push_front` above; same end as `Vector`'s, but stays a
        // `Queue`, no promotion -- measured, `(class (conj EMPTY 1))` is
        // still `clojure.lang.PersistentQueue`).
        Value::Queue(items) => {
            let mut v = items.clone();
            v.push_back(item.clone());
            Ok(Value::Queue(v))
        }
        // S3: `conj` onto a record stays a record (measured: `(conj r
        // [:c 3])` prints as the record with :c riding in ext).
        Value::Inst(inst) if inst.tdef.is_record => {
            let mut data = inst.data.clone();
            match item {
                // C14 (protocols): `(.cons rec nil)` is a measured no-op
                // (real `APersistentMap.cons` short-circuits on `null`),
                // exercised by `defrecord-interfaces-test`'s `.cons`
                // sub-test alongside the `{}` (empty-map, already a no-op
                // through the `Value::Map` arm below) row.
                Value::Nil => {}
                // S7: an entry conj'd onto a map/record/sorted-map behaves
                // exactly like the `[k v]` 2-vector it is (measured:
                // `(conj {} (first {:a 1}))` => `{:a 1}`, and
                // `(into {} (seq {:a 1 :b 2}))` round-trips).
                Value::Vector(pair) | Value::MapEntry(pair) if pair.len() == 2 => {
                    data.insert(pair[0].clone(), pair[1].clone());
                }
                Value::Map(other) => {
                    for (k, v) in other.iter() {
                        data.insert(k.clone(), v.clone());
                    }
                }
                // C14 (protocols): a user `defrecord`/`deftype` that
                // DECLARES `java.util.Map$Entry` (`protocols.clj`'s local
                // `MapEntry` type, `(getKey [_] k) (getValue [_] v)`) is a
                // single k/v PAIR, not a whole map to merge -- checked
                // BEFORE the generic "record merges as a whole map" arm
                // below, which would otherwise wrongly fold `MapEntry`'s
                // own `:k`/`:v` basis fields in as literal keys.
                Value::Inst(other)
                    if crate::builtins::types::implements_interface(
                        item,
                        &crate::value::Str::from("java.util.Map$Entry"),
                    ) =>
                {
                    let span = crate::reader::Span { start: 0, end: 0 };
                    let get_key = crate::builtins::types::lookup_interface_method(
                        &interp.interfaces,
                        other,
                        "getKey",
                    )
                    .ok_or_else(|| RjError::other("cons: Map$Entry has no getKey method"))?;
                    let get_val = crate::builtins::types::lookup_interface_method(
                        &interp.interfaces,
                        other,
                        "getValue",
                    )
                    .ok_or_else(|| RjError::other("cons: Map$Entry has no getValue method"))?;
                    let k = interp.apply_value(&get_key, &[item.clone()], span)?;
                    let v = interp.apply_value(&get_val, &[item.clone()], span)?;
                    data.insert(k, v);
                }
                Value::Inst(other) if other.tdef.is_record => {
                    for (k, v) in other.data.iter() {
                        data.insert(k.clone(), v.clone());
                    }
                }
                other => {
                    return Err(RjError::type_err(format!(
                        "conj: cannot conj {} onto a record",
                        other.type_name()
                    )))
                }
            }
            return Ok(Value::Inst(std::sync::Arc::new(crate::types::InstVal {
                tdef: inst.tdef.clone(),
                data,
                fields: std::sync::Mutex::new(crate::value::PVec::new()),
                meta: inst.meta.clone(),
            })));
        }
        Value::Map(m) => {
            crate::metrics::count(crate::metrics::CONJ, false);
            map_probe::record("conj-map", m.len());
            let mut m2 = m.clone();
            // MOVA-PATCH: `.unmeta()` -- a metadata-carrying conj item
            // (e.g. `(conj {} (with-meta {:a 1} {}))`) reports `type_name()
            // "map"` yet didn't match the `Value::Map` arm below, erroring
            // even though it plainly IS one.
            match item.unmeta() {
                // clojure-lsp campaign (mova/PLAN.md): `(conj m nil)` is a
                // measured no-op (real `APersistentMap.cons` short-
                // circuits on `null`) -- same treatment the record conj
                // arm above already gives it. `rewrite-clj.reader`'s
                // `read-with-meta` relies on exactly this: `(conj {:row
                // ... } (meta entry))` with `entry` carrying no reader
                // metadata yet (the overwhelmingly common case).
                Value::Nil => {}
                // S7: an entry conj'd onto a map/record/sorted-map behaves
                // exactly like the `[k v]` 2-vector it is (measured:
                // `(conj {} (first {:a 1}))` => `{:a 1}`, and
                // `(into {} (seq {:a 1 :b 2}))` round-trips).
                Value::Vector(pair) | Value::MapEntry(pair) if pair.len() == 2 => {
                    // C3e: hash-keyed position -- see `normalize_key`.
                    m2.insert(interp.normalize_key(pair[0].clone())?, pair[1].clone());
                }
                Value::Map(other) => {
                    // Keys already normalized when they entered `other`.
                    for (k, v) in other.iter() {
                        m2.insert(k.clone(), v.clone());
                    }
                }
                other => {
                    return Err(RjError::type_err(format!(
                        "conj: map conj arg must be a [k v] pair or a map, got {}",
                        other.type_name()
                    )))
                }
            }
            Ok(Value::Map(m2))
        }
        // W3: v1 policy -- `conj` onto a `HostStruct` widens to a plain
        // `Map` (materialize-once via `host_struct::as_pmap`, then the
        // same merge logic the `Map` arm above uses), exactly like
        // `assoc`/`dissoc` below. See `crate::embed::host`'s module doc,
        // "v1 scope": no overlay representation in v1.
        Value::HostStruct(hs) => {
            let m = crate::host_struct::as_pmap(hs);
            map_probe::record("conj-map", m.len());
            let mut m2 = m.clone();
            // MOVA-PATCH: `.unmeta()` -- see the `Value::Map` receiver arm's
            // identical comment just above.
            match item.unmeta() {
                // clojure-lsp campaign (mova/PLAN.md): `(conj m nil)` is a
                // measured no-op (real `APersistentMap.cons` short-
                // circuits on `null`) -- same treatment the record conj
                // arm above already gives it. `rewrite-clj.reader`'s
                // `read-with-meta` relies on exactly this: `(conj {:row
                // ... } (meta entry))` with `entry` carrying no reader
                // metadata yet (the overwhelmingly common case).
                Value::Nil => {}
                // S7: an entry conj'd onto a map/record/sorted-map behaves
                // exactly like the `[k v]` 2-vector it is (measured:
                // `(conj {} (first {:a 1}))` => `{:a 1}`, and
                // `(into {} (seq {:a 1 :b 2}))` round-trips).
                Value::Vector(pair) | Value::MapEntry(pair) if pair.len() == 2 => {
                    m2.insert(pair[0].clone(), pair[1].clone());
                }
                Value::Map(other) => {
                    for (k, v) in other.iter() {
                        m2.insert(k.clone(), v.clone());
                    }
                }
                Value::HostStruct(other_hs) => {
                    for (k, v) in crate::host_struct::as_pmap(other_hs).iter() {
                        m2.insert(k.clone(), v.clone());
                    }
                }
                other => {
                    return Err(RjError::type_err(format!(
                        "conj: map conj arg must be a [k v] pair or a map, got {}",
                        other.type_name()
                    )))
                }
            }
            Ok(Value::Map(m2))
        }
        Value::LazyMap(hs) => {
            let m = crate::lazy_map::as_pmap(hs);
            map_probe::record("conj-map", m.len());
            let mut m2 = m.clone();
            // MOVA-PATCH: `.unmeta()` -- see the `Value::Map` receiver arm's
            // identical comment just above.
            match item.unmeta() {
                // clojure-lsp campaign (mova/PLAN.md): `(conj m nil)` is a
                // measured no-op (real `APersistentMap.cons` short-
                // circuits on `null`) -- same treatment the record conj
                // arm above already gives it. `rewrite-clj.reader`'s
                // `read-with-meta` relies on exactly this: `(conj {:row
                // ... } (meta entry))` with `entry` carrying no reader
                // metadata yet (the overwhelmingly common case).
                Value::Nil => {}
                // S7: an entry conj'd onto a map/record/sorted-map behaves
                // exactly like the `[k v]` 2-vector it is (measured:
                // `(conj {} (first {:a 1}))` => `{:a 1}`, and
                // `(into {} (seq {:a 1 :b 2}))` round-trips).
                Value::Vector(pair) | Value::MapEntry(pair) if pair.len() == 2 => {
                    m2.insert(pair[0].clone(), pair[1].clone());
                }
                Value::Map(other) => {
                    for (k, v) in other.iter() {
                        m2.insert(k.clone(), v.clone());
                    }
                }
                Value::LazyMap(other_hs) => {
                    for (k, v) in crate::lazy_map::as_pmap(other_hs).iter() {
                        m2.insert(k.clone(), v.clone());
                    }
                }
                other => {
                    return Err(RjError::type_err(format!(
                        "conj: map conj arg must be a [k v] pair or a map, got {}",
                        other.type_name()
                    )))
                }
            }
            Ok(Value::Map(m2))
        }
        // C3e: hash-keyed position -- see `Interp::normalize_key`.
        Value::Set(s) => Ok(Value::Set(s.insert(interp.normalize_key(item.clone())?))),
        other => Err(RjError::type_err(format!(
            "conj: not a collection: {}",
            other.type_name()
        ))),
    }
}

/// S3: `assoc` on a record stays a record (measured: assoc of a new key
/// rides in the ext map, class unchanged).
fn record_assoc(inst: &std::sync::Arc<crate::types::InstVal>, k: &Value, v: &Value) -> Value {
    let mut data = inst.data.clone();
    data.insert(k.clone(), v.clone());
    Value::Inst(std::sync::Arc::new(crate::types::InstVal {
        tdef: inst.tdef.clone(),
        data,
        fields: std::sync::Mutex::new(crate::value::PVec::new()),
        meta: inst.meta.clone(),
    }))
}

/// S3: `dissoc` of a BASIS field degrades the record to a plain map;
/// `dissoc` of an ext key keeps the record (both measured).
fn record_dissoc(
    inst: &std::sync::Arc<crate::types::InstVal>,
    keys: &[Value],
) -> Value {
    let mut data = inst.data.clone();
    let mut basis_hit = false;
    for k in keys {
        if let Value::Keyword(name) = k {
            if inst.tdef.basis.iter().any(|b| b == name.text_ref()) {
                basis_hit = true;
            }
        }
        data.remove(k);
    }
    if basis_hit {
        Value::Map(data)
    } else {
        Value::Inst(std::sync::Arc::new(crate::types::InstVal {
            tdef: inst.tdef.clone(),
            data,
            fields: std::sync::Mutex::new(crate::value::PVec::new()),
            meta: inst.meta.clone(),
        }))
    }
}

/// Takes `&mut Interp` for the same reason `conj_one` does -- S4's
/// `SortedMap`/`TypedVec` arms may need to call through a comparator fn or
/// (respectively) never need one, but sharing one signature keeps both
/// consumers uniform.
fn assoc_one(interp: &mut Interp, coll: &Value, k: &Value, v: &Value) -> Result<Value, RjError> {
    match coll {
        // S5/M3: UPDATE -- keeps the receiver's metadata (measured:
        // `(meta (assoc (with-meta {:x 1} {:a 1}) :y 2))` is `{:a 1}`).
        // `assoc-in`/`update-in` bottom out here, which is why they
        // preserve too (also measured).
        Value::Meta(m) => Ok(assoc_one(interp, &m.inner, k, v)?.with_meta_of(coll)),
        Value::Nil => {
            let mut m = PMap::new();
            m.insert(k.clone(), v.clone());
            Ok(Value::Map(m))
        }
        // S4
        Value::SortedMap(m) => crate::builtins::sorted::sorted_map_assoc(interp, m, k, v),
        // C2 (defstruct): stays a StructMap for both a basis key and a
        // brand-new extension key (measured).
        Value::StructMap(sm) => Ok(crate::builtins::structmap::struct_map_assoc(sm, k, v)),
        Value::TypedVec(tv) => {
            let idx = require_index(k, "assoc")?;
            crate::builtins::sorted::typed_vec_assoc(tv, idx, v)
        }
        Value::Map(m) => {
            crate::metrics::count(crate::metrics::ASSOC, false);
            map_probe::record("assoc", m.len());
            let mut m2 = m.clone();
            // C3e: hash-keyed position -- see `Interp::normalize_key`.
            m2.insert(interp.normalize_key(k.clone())?, v.clone());
            Ok(Value::Map(m2))
        }
        // W3: `assoc` on a `HostStruct` ALWAYS produces a plain `Map` --
        // v1 has no overlay representation (see `crate::embed::host`'s
        // module doc). The `HostStruct` itself (and therefore anything
        // else still holding it) is untouched; this only ever builds a
        // new value from the materialized snapshot.
        Value::HostStruct(hs) => {
            let m = crate::host_struct::as_pmap(hs);
            map_probe::record("assoc", m.len());
            let mut m2 = m.clone();
            m2.insert(k.clone(), v.clone());
            Ok(Value::Map(m2))
        }
        Value::LazyMap(hs) => {
            let m = crate::lazy_map::as_pmap(hs);
            map_probe::record("assoc", m.len());
            let mut m2 = m.clone();
            m2.insert(k.clone(), v.clone());
            Ok(Value::Map(m2))
        }
        // S3: records stay records under assoc (measured).
        Value::Inst(inst) if inst.tdef.is_record => Ok(record_assoc(inst, k, v)),
        // S7: promotes -- measured, `(class (assoc (first {:a 1}) 0 :z))`
        // is `clojure.lang.PersistentVector` even for an IN-RANGE index.
        Value::Vector(items) | Value::MapEntry(items) => {
            let idx = require_index(k, "assoc")?;
            crate::metrics::count(crate::metrics::ASSOC, false);
            let mut v2 = items.clone();
            if idx < v2.len() {
                v2.set(idx, v.clone());
            } else if idx == v2.len() {
                v2.push_back(v.clone());
            } else {
                return Err(RjError::other(format!(
                    "assoc: index {idx} out of bounds for vector of length {}",
                    items.len()
                )));
            }
            Ok(Value::Vector(v2))
        }
        other => Err(RjError::type_err(format!(
            "assoc: not associative: {}",
            other.type_name()
        ))),
    }
}

// --- the consuming calling convention (Perceus-lite phase 1) ------------
//
// `conj_owned`/`assoc_owned` are the by-value twins of `conj_one`/
// `assoc_one` above. They are NOT a second implementation of the
// semantics: each mutates through the very same `PMap`/`PVec` method its
// borrowing twin calls, and differs only in *where the handle came from* --
// moved out of an args `Vec` the caller gave up (see `builtins::reuse` and
// `Interp::apply_value_owned`) instead of cloned out of a borrowed slice.
//
// THE INVARIANT, restated where the take happens: mutating through an
// exclusively-owned handle is safe *regardless of whether anything else
// still points at the same structure*, because `Arc::make_mut`/
// `Arc::get_mut` (Small) and imbl's/champ's own node-level copy-on-write
// (Big -- `PVec`'s `imbl::Vector`, `PMap`'s `champ::PersistentHashMap`)
// both copy exactly when another handle exists. Ownership of the handle is what
// this convention buys; uniqueness of the structure is what it merely
// makes *possible*. Persistent semantics are therefore unchanged by
// construction -- only allocation traffic moves.

/// Moves a `Value` out of an args slot the callee owns, leaving a cheap
/// `Nil` behind (`Value` has no `Default`, so this is `mem::take` spelled
/// out). The vacated slot is never read again: the args `Vec` is dropped
/// as soon as the native returns.
#[inline]
fn take_arg(slot: &mut Value) -> Value {
    std::mem::replace(slot, Value::Nil)
}

fn conj_owned(interp: &mut Interp, coll: Value, item: Value) -> Result<Value, RjError> {
    // S5/M3: the consuming twin of `conj_one` must agree with it
    // observably (see `NativeFn::consuming`'s contract), so it preserves
    // the receiver's metadata the same way. Handled before the match
    // because this entry point OWNS `coll` and unwrapping moves out of it.
    if let Value::Meta(m) = &coll {
        let meta = m.meta.clone();
        let inner = coll.into_unmeta();
        return Ok(Value::attach_meta(conj_owned(interp, inner, item)?, meta));
    }
    match coll {
        Value::List(mut items) => {
            map_probe::record_reuse("conj-list", || items.small_is_unique());
            items.push_front(item);
            Ok(Value::List(items))
        }
        Value::Vector(mut items) => {
            crate::metrics::count(crate::metrics::CONJ, items.is_unique());
            map_probe::record_reuse("conj-vector", || items.small_is_unique());
            items.push_back(item);
            Ok(Value::Vector(items))
        }
        Value::Map(mut m) => {
            crate::metrics::count(crate::metrics::CONJ, m.is_unique());
            map_probe::record("conj-map", m.len());
            map_probe::record_reuse("conj-map", || m.small_is_unique());
            // MOVA-PATCH: `.unmeta()` -- see the borrowing twin's identical comment.
            match item.unmeta() {
                // clojure-lsp campaign (mova/PLAN.md): `(conj m nil)` is a
                // measured no-op (real `APersistentMap.cons` short-
                // circuits on `null`) -- same treatment the record conj
                // arm above already gives it. `rewrite-clj.reader`'s
                // `read-with-meta` relies on exactly this: `(conj {:row
                // ... } (meta entry))` with `entry` carrying no reader
                // metadata yet (the overwhelmingly common case).
                Value::Nil => {}
                // S7: an entry conj'd onto a map/record/sorted-map behaves
                // exactly like the `[k v]` 2-vector it is (measured:
                // `(conj {} (first {:a 1}))` => `{:a 1}`, and
                // `(into {} (seq {:a 1 :b 2}))` round-trips).
                Value::Vector(pair) | Value::MapEntry(pair) if pair.len() == 2 => {
                    // `pair` is owned here, but its two elements still sit
                    // behind its `Arc`; cloning them is what the borrowing
                    // twin does too, and is just two refcount bumps.
                    // C3e: hash-keyed position -- see `normalize_key`.
                    m.insert(interp.normalize_key(pair[0].clone())?, pair[1].clone());
                }
                Value::Map(other) => {
                    for (k, v) in other.iter() {
                        m.insert(k.clone(), v.clone());
                    }
                }
                other => {
                    return Err(RjError::type_err(format!(
                        "conj: map conj arg must be a [k v] pair or a map, got {}",
                        other.type_name()
                    )))
                }
            }
            Ok(Value::Map(m))
        }
        // `nil` and sets have nothing to reuse (a fresh list; sets have no
        // owned/mutate-in-place `insert` in champ's `PersistentHashSet`
        // -- only the persistent `&self` flavor, see `require_set`'s
        // callers below), so they defer to the borrowing twin rather than
        // duplicating its rules.
        other => conj_one(interp, &other, &item),
    }
}

fn assoc_owned(interp: &mut Interp, coll: Value, k: Value, v: Value) -> Result<Value, RjError> {
    match coll {
        Value::Map(mut m) => {
            crate::metrics::count(crate::metrics::ASSOC, m.is_unique());
            map_probe::record("assoc", m.len());
            map_probe::record_reuse("assoc", || m.small_is_unique());
            // C3e: hash-keyed position -- see `Interp::normalize_key`.
            m.insert(interp.normalize_key(k)?, v);
            Ok(Value::Map(m))
        }
        Value::Vector(mut items) => {
            let idx = require_index(&k, "assoc")?;
            crate::metrics::count(crate::metrics::ASSOC, items.is_unique());
            map_probe::record_reuse("assoc-vector", || items.small_is_unique());
            if idx < items.len() {
                items.set(idx, v);
            } else if idx == items.len() {
                items.push_back(v);
            } else {
                return Err(RjError::other(format!(
                    "assoc: index {idx} out of bounds for vector of length {}",
                    items.len()
                )));
            }
            Ok(Value::Vector(items))
        }
        other => assoc_one(interp, &other, &k, &v),
    }
}

/// The one body behind both of `update`'s entry points. Everything it
/// needs is already owned, so it can hand the receiver straight to
/// `assoc_owned`.
fn update_body(
    interp: &mut Interp,
    coll: Value,
    k: Value,
    f: Value,
    extra: Vec<Value>,
) -> Result<Value, RjError> {
    let current = match &coll {
        Value::Map(m) => {
            map_probe::record("update", m.len());
            m.get(&k).cloned().unwrap_or(Value::Nil)
        }
        // Touch-only fast path for the common keyword-key case (no
        // materialize); any other key type falls back to `as_pmap`.
        Value::HostStruct(hs) => match &k {
            Value::Keyword(kw) => crate::host_struct::lookup(hs, kw.text_ref()).unwrap_or(Value::Nil),
            _ => crate::host_struct::as_pmap(hs).get(&k).cloned().unwrap_or(Value::Nil),
        },
        Value::LazyMap(hs) => match &k {
            Value::Keyword(kw) => crate::lazy_map::lookup(hs, kw.text_ref()).unwrap_or(Value::Nil),
            _ => crate::lazy_map::as_pmap(hs).get(&k).cloned().unwrap_or(Value::Nil),
        },
        // S7: reads like a vector; the `assoc_owned` tail then promotes
        // the RESULT (measured: `(class (update (first {:a 1}) 1 inc))` is
        // `clojure.lang.PersistentVector`).
        Value::Vector(items) | Value::MapEntry(items) => {
            let idx = require_index(&k, "update")?;
            items.get_owned(idx).unwrap_or(Value::Nil)
        }
        Value::Nil => Value::Nil,
        // kondo-wave: a defrecord IS associative on the real JVM (it
        // implements `IPersistentMap`) -- `assoc`/`get`/keyword lookup
        // already treat it that way (`assoc_one`'s `record_assoc` arm,
        // `ilookup_val_at`'s record arm above); `update` had its own
        // separate `current`-lookup match that never grew the matching
        // arm, so `(update a-record :k f)` fell through to the generic
        // "not associative" error despite `(assoc a-record :k v)` on the
        // SAME record working fine one line above it.
        Value::Inst(inst) if inst.tdef.is_record => inst.data.get(&k).cloned().unwrap_or(Value::Nil),
        other => return Err(RjError::type_err(format!("update: not associative: {}", other.type_name()))),
    };
    let mut call_args = Vec::with_capacity(1 + extra.len());
    call_args.push(current);
    call_args.extend(extra);
    // Handed over (phase 3): freshly built, dead after the call. `current`
    // was read out of `coll` by value, so unless `coll` itself still holds
    // the same value (the common case -- `update` is not `dissoc`-then-call)
    // the callee's parameter slot is the only handle on it.
    let new_val = interp.call_owned(&f, call_args)?;
    assoc_owned(interp, coll, k, new_val)
}

fn require_index(v: &Value, op: &str) -> Result<usize, RjError> {
    match v {
        Value::Int(n) if *n >= 0 => Ok(*n as usize),
        Value::Int(n) => Err(RjError::other(format!("{op}: negative index {n}"))),
        other => Err(RjError::type_err(format!(
            "{op}: index must be an int, got {}",
            other.type_name()
        ))),
    }
}

/// W3a: which JVM index-error class real `nth` raises for a given receiver,
/// measured against the 1.13.0-alpha6 oracle (transcript in the W3a landing
/// commit): `(nth "abc" -1)` => `StringIndexOutOfBoundsException`,
/// `(nth (into-array [1 2 3]) -1)` => `ArrayIndexOutOfBoundsException`,
/// and every other `Indexed`/`Sequential` receiver (list, vector,
/// `java.util.List`, seq) => plain `IndexOutOfBoundsException`.
///
/// A `Matcher` is the one receiver whose class depends on its STATE, not
/// its type: `RT.nth` calls `Matcher.group(i)`, which checks "has a match
/// been found?" BEFORE it checks the group index. Measured on both sides of
/// that fork (this is exactly what `sequences.clj`'s two adjacent
/// `re-matcher` `let` blocks assert):
///   `(let [m (re-matcher #"(a)(b)" "ababaa")] (re-find m) (nth m 3))`
///     => `IndexOutOfBoundsException: No group 3`  (and `-1` likewise)
///   `(let [m (re-matcher #"c" "ababaa")] (re-find m) (nth m 0))`
///     => `IllegalStateException: No match found`  (and `2`, `-1` likewise)
///
/// Shared by `nth`'s negative-index branch (which runs BEFORE the per-type
/// fast paths and so cannot use their individually-tagged errors) and its
/// generic walk-the-seq fallback.
fn nth_index_class(receiver: &Value) -> JvmClass {
    match receiver {
        Value::Str(_) => JvmClass::StringIndexOutOfBounds,
        Value::Array(_) => JvmClass::ArrayIndexOutOfBounds,
        Value::Matcher(m) => matcher_nth_class(m),
        _ => JvmClass::IndexOutOfBounds,
    }
}

/// The state-dependent half of [`nth_index_class`], factored out because
/// `nth`'s own `Value::Matcher` fast path already holds the lock and needs
/// the same answer. See that fn's doc for the oracle transcript.
fn matcher_nth_class(m: &std::sync::Arc<std::sync::Mutex<crate::value::MatcherState>>) -> JvmClass {
    if crate::sync::lock_mutex(m).last_match.is_some() {
        JvmClass::IndexOutOfBounds
    } else {
        JvmClass::IllegalState
    }
}

pub fn register(i: &mut Interp) {
    reg(i, "vector", ArityHint::Any, |_i, args| {
        Ok(Value::Vector(args.iter().cloned().collect()))
    });

    reg(i, "hash-map", ArityHint::Any, |interp, args| {
        if args.len() % 2 != 0 {
            return Err(RjError::arity("hash-map: expected an even number of arguments"));
        }
        let mut m = PMap::new();
        for pair in args.chunks(2) {
            // C3e: hash-keyed position -- see `Interp::normalize_key`.
            m.insert(interp.normalize_key(pair[0].clone())?, pair[1].clone());
        }
        // `{...}` reader literals and `(hash-map ...)` calls both go
        // through this native: every `{:c c2}`-shaped per-message state
        // literal in a flow step-fn is a fresh HAMT root built from
        // scratch here, not an `assoc` mutation -- worth its own
        // touch-site so (A)'s histogram doesn't miss that mass.
        map_probe::record("hash-map (literal)", m.len());
        Ok(Value::Map(m))
    });

    // C2 (defstruct) side quest: the vendored `data_structures.clj` file
    // this task unblocks calls `array-map` directly (its own struct-vs-
    // other-map-classes `are` test, plus `test-array-map-arity`/
    // `map-equality-test`/`test-seq-iter-match`) -- a pre-existing,
    // unrelated-to-defstruct gap (mova had NO `array-map` builtin at all
    // before this), but one the suite measurably demands to even LOAD the
    // file, so in scope per this task's own "never build deeper than the
    // suite demands, but exactly that deep" brief. Real Clojure's own
    // `array-map` (`.oracle/clojure-src/src/clj/clojure/core.clj:4402`):
    // 0-arity is the empty map, odd `keyvals` throws
    // `IllegalArgumentException: "No value supplied for key: <(str
    // (last keyvals))>"` (measured: the trailing key is `str`'d, NOT
    // `pr-str`'d), duplicate keys keep the LAST value (same "handled as
    // if by repeated `assoc`" policy `hash-map` above already
    // implements via its own insert loop). mova's `Value::Map` already
    // reports `clojure.lang.PersistentArrayMap` for any map with `<= 8`
    // entries (`types::builtin_class_name`'s existing, pre-defstruct
    // threshold) -- ALL of the vendored file's `array-map` calls stay
    // under that threshold, so no new representation is needed: this is
    // the exact same `PMap` `hash-map` builds, just under a name whose
    // arity error matches the real one.
    reg(i, "array-map", ArityHint::Any, |interp, args| {
        if args.len() % 2 != 0 {
            let last = args.last().expect("odd length is never 0, so there is a last element");
            // W3a: the doc above already recorded the measured class
            // (`IllegalArgumentException`); it is carried now, not just
            // quoted.
            return Err(RjError::other(format!(
                "No value supplied for key: {}",
                crate::printer::display_str(last)
            ))
            .with_class(JvmClass::IllegalArgument));
        }
        let mut m = PMap::new();
        for pair in args.chunks(2) {
            // C3e: hash-keyed position -- see `Interp::normalize_key`.
            m.insert(interp.normalize_key(pair[0].clone())?, pair[1].clone());
        }
        Ok(Value::Map(m))
    });

    reg(i, "hash-set", ArityHint::Any, |interp, args| {
        // C3e: hash-keyed position -- see `Interp::normalize_key`.
        let mut out = champ::PersistentHashSet::new().transient();
        for a in args.iter() {
            out.insert(interp.normalize_key(a.clone())?);
        }
        Ok(Value::Set(out.persistent()))
    });

    reg(i, "set", ArityHint::Exact(1), |interp, args| {
        let items = materialize(interp, &args[0])?;
        // C3e: hash-keyed position -- see `Interp::normalize_key`.
        let mut out = champ::PersistentHashSet::new().transient();
        for it in items {
            out.insert(interp.normalize_key(it)?);
        }
        Ok(Value::Set(out.persistent()))
    });

    reg_consuming_preserving_meta(
        i,
        "conj",
        // C13: `Min(0)` -- real Clojure's `(conj)` (0-arity) is `[]`
        // (measured), the base case `into`/`transduce`'s "`conj` as a base
        // reducing fn" pattern needs (`(f)` init arity). `(conj coll)`
        // (1-arity) already fell out for free below (the loop over `args[1..]`
        // / `rest` is simply empty).
        ArityHint::Min(0),
        |interp, args| {
            if args.is_empty() {
                return Ok(Value::Vector(PVec::new()));
            }
            let mut coll = args[0].clone();
            for item in &args[1..] {
                coll = conj_one(interp, &coll, item)?;
            }
            Ok(coll)
        },
        |interp, args| {
            if args.is_empty() {
                return Ok(Value::Vector(PVec::new()));
            }
            let (first, rest) = args.split_at_mut(1);
            let mut coll = take_arg(&mut first[0]);
            for slot in rest {
                let item = take_arg(slot);
                coll = conj_owned(interp, coll, item)?;
            }
            Ok(coll)
        },
    );

    reg_consuming_preserving_meta(
        i,
        "assoc",
        ArityHint::Min(3),
        |interp, args| {
            if (args.len() - 1) % 2 != 0 {
                return Err(RjError::arity("assoc: expected key/value pairs"));
            }
            let mut coll = args[0].clone();
            let mut idx = 1;
            while idx < args.len() {
                coll = assoc_one(interp, &coll, &args[idx], &args[idx + 1])?;
                idx += 2;
            }
            Ok(coll)
        },
        |interp, args| {
            if (args.len() - 1) % 2 != 0 {
                return Err(RjError::arity("assoc: expected key/value pairs"));
            }
            let mut coll = take_arg(&mut args[0]);
            let mut idx = 1;
            while idx < args.len() {
                let k = take_arg(&mut args[idx]);
                let v = take_arg(&mut args[idx + 1]);
                // After the first pair `coll` is a temporary this frame is
                // the sole owner of, so a multi-pair `(assoc m :a 1 :b 2)`
                // reuses on every pair but the first no matter what the
                // caller does with `m`.
                coll = assoc_owned(interp, coll, k, v)?;
                idx += 2;
            }
            Ok(coll)
        },
    );

    reg_consuming_preserving_meta(
        i,
        "dissoc",
        ArityHint::Min(1),
        |interp, args| match &args[0] {
            Value::Nil => Ok(Value::Nil),
            Value::Map(m) => {
                map_probe::record("dissoc", m.len());
                let mut m2 = m.clone();
                for k in &args[1..] {
                    m2.remove(k);
                }
                Ok(Value::Map(m2))
            }
            // W3: same v1 widen-to-Map policy as `assoc`/`conj` above.
            Value::HostStruct(hs) => {
                let m = crate::host_struct::as_pmap(hs);
                map_probe::record("dissoc", m.len());
                let mut m2 = m.clone();
                for k in &args[1..] {
                    m2.remove(k);
                }
                Ok(Value::Map(m2))
            }
            Value::LazyMap(hs) => {
                let m = crate::lazy_map::as_pmap(hs);
                map_probe::record("dissoc", m.len());
                let mut m2 = m.clone();
                for k in &args[1..] {
                    m2.remove(k);
                }
                Ok(Value::Map(m2))
            }
            // S3: basis-key dissoc degrades to a plain map (measured).
            Value::Inst(inst) if inst.tdef.is_record => Ok(record_dissoc(inst, &args[1..])),
            // S4
            Value::SortedMap(m) => {
                let mut cur = Value::SortedMap(m.clone());
                for k in &args[1..] {
                    let Value::SortedMap(m) = &cur else { unreachable!() };
                    cur = crate::builtins::sorted::sorted_map_dissoc(interp, m, k)?;
                }
                Ok(cur)
            }
            // C2 (defstruct): a basis key throws (measured exact message);
            // an extension key dissocs in place, staying a StructMap.
            Value::StructMap(m) => {
                let mut cur = Value::StructMap(m.clone());
                for k in &args[1..] {
                    let Value::StructMap(m) = &cur else { unreachable!() };
                    cur = crate::builtins::structmap::struct_map_dissoc(m, k)?;
                }
                Ok(cur)
            }
            other => Err(RjError::type_err(format!("dissoc: not a map: {}", other.type_name()))),
        },
        |interp, args| match take_arg(&mut args[0]) {
            Value::Nil => Ok(Value::Nil),
            Value::Map(mut m) => {
                map_probe::record("dissoc", m.len());
                map_probe::record_reuse("dissoc", || m.small_is_unique());
                for k in &args[1..] {
                    m.remove(k);
                }
                Ok(Value::Map(m))
            }
            Value::HostStruct(hs) => {
                let m = crate::host_struct::as_pmap(&hs);
                map_probe::record("dissoc", m.len());
                let mut m2 = m.clone();
                for k in &args[1..] {
                    m2.remove(k);
                }
                Ok(Value::Map(m2))
            }
            Value::LazyMap(hs) => {
                let m = crate::lazy_map::as_pmap(&hs);
                map_probe::record("dissoc", m.len());
                let mut m2 = m.clone();
                for k in &args[1..] {
                    m2.remove(k);
                }
                Ok(Value::Map(m2))
            }
            Value::Inst(inst) if inst.tdef.is_record => Ok(record_dissoc(&inst, &args[1..])),
            // S4
            Value::SortedMap(m) => {
                let mut cur = Value::SortedMap(m);
                for k in &args[1..] {
                    let Value::SortedMap(m) = &cur else { unreachable!() };
                    cur = crate::builtins::sorted::sorted_map_dissoc(interp, m, k)?;
                }
                Ok(cur)
            }
            // C2 (defstruct)
            Value::StructMap(m) => {
                let mut cur = Value::StructMap(m);
                for k in &args[1..] {
                    let Value::StructMap(m) = &cur else { unreachable!() };
                    cur = crate::builtins::structmap::struct_map_dissoc(m, k)?;
                }
                Ok(cur)
            }
            other => Err(RjError::type_err(format!("dissoc: not a map: {}", other.type_name()))),
        },
    );

    reg_unmeta(i, "get", ArityHint::Range(2, 3), |interp, args| {
        let default = args.get(2).cloned().unwrap_or(Value::Nil);
        Ok(match &args[0] {
            Value::Map(m) => {
                map_probe::record("get", m.len());
                match m.get(&args[1]) {
                    Some(hit) => hit.clone(),
                    // C3e: an unrealized `Lazy` probe hashes by pointer, so
                    // a miss is not yet proof of absence -- retry ONCE with
                    // the realized seq (see `Interp::normalize_key`). Cost
                    // is paid only on a miss with a lazy key, never on the
                    // hot keyword/string/int lookup path.
                    None => match normalized_probe(interp, &args[1])? {
                        Some(k) => m.get(&k).cloned().unwrap_or(default),
                        None => default,
                    },
                }
            }
            // Touch-only fast path for a keyword key (no materialize);
            // any other key type falls back to `as_pmap`.
            Value::HostStruct(hs) => match &args[1] {
                Value::Keyword(kw) => crate::host_struct::lookup(hs, kw.text_ref()).unwrap_or(default),
                _ => crate::host_struct::as_pmap(hs).get(&args[1]).cloned().unwrap_or(default),
            },
            Value::LazyMap(hs) => match &args[1] {
                Value::Keyword(kw) => crate::lazy_map::lookup(hs, kw.text_ref()).unwrap_or(default),
                _ => crate::lazy_map::as_pmap(hs).get(&args[1]).cloned().unwrap_or(default),
            },
            Value::Set(s) => {
                if s.contains(&args[1]) {
                    args[1].clone()
                } else if let Some(k) = normalized_probe(interp, &args[1])? {
                    // C3e: same retry-after-miss shape as the `Map` arm.
                    // `get` on a set returns the STORED element, which is
                    // the normalized one.
                    if s.contains(&k) {
                        k
                    } else {
                        default
                    }
                } else {
                    default
                }
            }
            // S3: records `get` like maps (measured); deftypes fall to
            // the nil default like any non-associative value.
            Value::Inst(inst) if inst.tdef.is_record => {
                inst.data.get(&args[1]).cloned().unwrap_or(default)
            }
            // SPEC-W1 task 6: an instance that DECLARED
            // `clojure.lang.ILookup` answers through its own `valAt`, the
            // route `RT.get`/`RT.getFrom` take on the JVM. Checked after
            // the record arm (a record is associative natively) and
            // before the default fallthrough, so a `deftype`/`reify` that
            // declares nothing still falls through exactly as before.
            // See `builtins::types::ilookup_val_at`.
            Value::Inst(_) => match crate::builtins::types::ilookup_val_at(
                interp,
                &args[0],
                &args[1],
                args.get(2).cloned(),
            ) {
                Some(r) => r?,
                None => default,
            },
            // S7 (measured): `(get (first {:a 1}) 0)` => `:a`,
            // `(get .. 2)` => `nil`, `(get .. :missing :dflt)` => `:dflt`
            // -- index lookup exactly like the vector it is.
            Value::Vector(items) | Value::MapEntry(items) => match &args[1] {
                Value::Int(n) if *n >= 0 => items.get_owned(*n as usize).unwrap_or(default),
                _ => default,
            },
            // S4
            Value::SortedMap(m) => {
                crate::builtins::sorted::sorted_map_get(interp, m, &args[1])?.unwrap_or(default)
            }
            Value::SortedSet(s) => crate::builtins::sorted::sorted_set_get(interp, s, &args[1]).unwrap_or(default),
            // C2 (defstruct)
            Value::StructMap(sm) => crate::builtins::structmap::struct_map_get(sm, &args[1]).cloned().unwrap_or(default),
            Value::TypedVec(tv) => match &args[1] {
                Value::Int(n) if *n >= 0 => tv.data.get_owned(*n as usize).unwrap_or(default),
                _ => default,
            },
            // C10: `java.util.HashMap`/`HashSet` -- `get` works on both,
            // same as `contains?` above.
            Value::HostInst(h) if h.kind == crate::hostclass::HostKind::HashMap => {
                let guard = crate::sync::lock_mutex(&h.state);
                let crate::hostclass::HostState::HashMap(m) = &*guard else {
                    unreachable!("HostKind::HashMap always holds HostState::HashMap");
                };
                m.get(&args[1]).cloned().unwrap_or(default)
            }
            Value::HostInst(h) if h.kind == crate::hostclass::HostKind::HashSet => {
                let guard = crate::sync::lock_mutex(&h.state);
                let crate::hostclass::HostState::HashSet(s) = &*guard else {
                    unreachable!("HostKind::HashSet always holds HostState::HashSet");
                };
                if s.contains(&args[1]) {
                    args[1].clone()
                } else {
                    default
                }
            }
            _ => default,
        })
    });

    // §5/M2 (1.13, `:added "1.13"`): `req!` is `get`'s 2-arity sibling
    // that THROWS instead of defaulting to `nil` when `key` is missing --
    // `clojure.lang.RT/req` (`.oracle/clojure-src/src/jvm/clojure/lang/
    // RT.java`'s `req`/`reqmsg`). Backs `:keys!`/`:strs!`/`:syms!`
    // required-key destructuring, though the destructuring path itself
    // (`eval::special_forms::resolve_push_value`) reimplements this same
    // presence/absence check directly rather than calling through this
    // builtin, so a destructuring throw carries the destructuring SITE's
    // span/stack instead of `req!`'s own (bare) call site -- both stay in
    // agreement by construction (same `Missing required key: <pr_str
    // key>` format), checked independently by `tests/destructure_test.rs`
    // AND `compat/destructuring-113.corpus`. Same collection-type
    // breadth as `get` just above (kept in sync by hand -- `get` isn't
    // itself a reusable Rust fn to delegate to, it's an inline closure).
    // `reqmsg`'s own asymmetry falls out of `pr_str` for free: a STRING
    // key prints quoted (`Missing required key: "a"`), everything else
    // (keyword/symbol/nil/...) prints bare (`:a`/`a`/`nil`).
    reg_unmeta(i, "req!", ArityHint::Exact(2), |interp, args| {
        let key = &args[1];
        let found: Option<Value> = match &args[0] {
            Value::Map(m) => {
                map_probe::record("req!", m.len());
                m.get(key).cloned()
            }
            Value::HostStruct(hs) => match key {
                Value::Keyword(kw) => crate::host_struct::lookup(hs, kw.text_ref()),
                _ => crate::host_struct::as_pmap(hs).get(key).cloned(),
            },
            Value::LazyMap(hs) => match key {
                Value::Keyword(kw) => crate::lazy_map::lookup(hs, kw.text_ref()),
                _ => crate::lazy_map::as_pmap(hs).get(key).cloned(),
            },
            Value::Set(s) => {
                if s.contains(key) {
                    Some(key.clone())
                } else {
                    None
                }
            }
            Value::Inst(inst) if inst.tdef.is_record => inst.data.get(key).cloned(),
            Value::Vector(items) | Value::MapEntry(items) => match key {
                Value::Int(n) if *n >= 0 => items.get_owned(*n as usize),
                _ => None,
            },
            Value::SortedMap(m) => crate::builtins::sorted::sorted_map_get(interp, m, key)?,
            Value::SortedSet(s) => crate::builtins::sorted::sorted_set_get(interp, s, key),
            Value::StructMap(sm) => crate::builtins::structmap::struct_map_get(sm, key).cloned(),
            Value::TypedVec(tv) => match key {
                Value::Int(n) if *n >= 0 => tv.data.get_owned(*n as usize),
                _ => None,
            },
            _ => None,
        };
        match found {
            Some(v) => Ok(v),
            None => Err(RjError::other(format!(
                "Missing required key: {}",
                crate::printer::pr_str(key)
            ))),
        }
    });

    // §5/M2 (1.13, `:added "1.13"`): "Returns a map with only the non-nil
    // values of map m. Returns nil if m has no non-nil vals." Backs
    // `destmap*`'s `:select`/`:all`/`:defaults` merges in real Clojure;
    // `eval::special_forms::bind_map_pattern` reimplements the same
    // nil-filter inline (for the same reason `req!`'s destructuring use
    // doesn't call through the builtin above either) rather than calling
    // this, so this registration exists purely so user code -- and any
    // future direct spec-conformance probe -- can call `some-vals` by
    // name, matching `:added "1.13"`'s status as a real `clojure.core` var.
    reg_unmeta(i, "some-vals", ArityHint::Exact(1), |_interp, args| match &args[0] {
        Value::Nil => Ok(Value::Nil),
        Value::Map(m) => {
            let mut out = PMap::new();
            for (k, v) in m.iter() {
                if !matches!(v, Value::Nil) {
                    out.insert(k.clone(), v.clone());
                }
            }
            Ok(if out.is_empty() { Value::Nil } else { Value::Map(out) })
        }
        other => Err(RjError::type_err(format!("some-vals: not a map: {}", other.type_name()))),
    });

    // §5/M2 (1.11, `:added "1.11"`): builds a map from a seq the same way
    // `destmap*`'s `& {:keys [...]}` kwargs coercion does -- see
    // `eval::special_forms::seq_to_map_for_destructuring`'s doc for the
    // 0/1/N-element and trailing-map-merge rules. Thin wrapper so the two
    // call sites (the language's OWN destructuring coercion, and user
    // code calling this fn by name -- `tests/clojure-suite/vendor/
    // data_structures.clj`'s `trailing-map-destructuring` deftest does
    // both) can never disagree.
    reg_unmeta(i, "seq-to-map-for-destructuring", ArityHint::Exact(1), |interp, args| {
        crate::eval::special_forms::seq_to_map_for_destructuring(interp, &args[0])
    });

    // S4 (everyday3), measured: `find` is `get`'s "give me the whole
    // MapEntry" sibling -- maps/records/vectors (by index) return `[k v]`/
    // `[idx val]` on a hit, `nil` on a miss (out-of-bounds, negative
    // index, or missing key -- never the 3-arity `get`'s default, `find`
    // has no default arity). `(find nil _)` is `nil` (total, like `get`).
    // Sets and lists are NOT associative -- real Clojure throws
    // `IllegalArgumentException: find not supported on type: ...` rather
    // than silently returning `nil`, so those fall to the same `type_err`
    // catch-all `get`/`contains?`/`assoc` use for a non-associative
    // receiver. A `Value::Vector` entry is `[idx val]`, NOT `[val val]` --
    // `(find [10 20 30] 1)` is `[1 20]`.

    // S4 (everyday3): a uniform fold-right `cons`, matching real
    // Clojure's `list*` for every arity at once -- `(list* args)` is
    // `(seq args)` (1-arity only: `(list* [])` is `nil`, not `()`), and
    // `(list* a .. z tail)` (2+ arity) is `(cons a (cons .. (cons z
    // tail)))`, i.e. every LEADING argument becomes its own head, cons'd
    // in order onto the LAST argument (seq'd if it isn't already a list --
    // `cons_builtin` already does that for any coll shape). Real Clojure
    // spells this as five hand-unrolled arities (`a`, `a b`, `a b c`, `a b
    // c d`, `a b c d & more` via `spread`) purely to avoid an `apply`-style
    // list allocation per call; behaviorally that unrolling is exactly
    // this fold for every N, so one Rust loop covers it without porting
    // `spread` itself.
    reg(i, "list*", ArityHint::Min(1), |interp, args| {
        if args.len() == 1 {
            return seq_of(interp, &args[0]);
        }
        let (leading, tail) = args.split_at(args.len() - 1);
        let mut result = tail[0].clone();
        for head in leading.iter().rev() {
            result = cons_builtin(interp, head.clone(), result)?;
        }
        Ok(result)
    });

    reg(i, "nth", ArityHint::Range(2, 3), |interp, args| {
        let n_signed = match &args[1] {
            Value::Int(n) => *n,
            other => {
                return Err(RjError::type_err(format!(
                    "nth: index must be an int, got {}",
                    other.type_name()
                )))
            }
        };
        // S6: a `deftype` implementing `clojure.lang.Indexed` dispatches
        // to its OWN `nth` impl -- checked FIRST, before the negative-
        // index short-circuit below, because real Clojure's `nth` on an
        // `Indexed` receiver does no bounds/sign checking of its own at
        // all; it calls straight through to `.nth(i)`/`.nth(i,
        // not-found)` and lets the implementation decide (measured:
        // `(nth t -1)` throws whatever the user's own `nth` method
        // throws for a non-zero index, not a "negative index" error, and
        // `(nth t -1 :nf)` returns whatever that same method's 3-arity
        // clause returns -- so a deftype never reaches the generic
        // vector/string/seq arms below at all). `(get t i)`/`(seq t)` on
        // an Indexed-only deftype do NOT dispatch here -- measured `(get
        // t 0)` => `nil` (mova's existing "not associative" default) and
        // `(seq t)` throws, both unchanged, per this task's oracle
        // measurement (no extra dispatch beyond `nth` itself).
        if let Value::Inst(inst) = &args[0] {
            if inst.tdef.interfaces.iter().any(|iface| &**iface == "clojure.lang.Indexed") {
                if let Some(f) =
                    crate::builtins::types::lookup_interface_method(&interp.interfaces, inst, "nth")
                {
                    let mut call_args = vec![args[0].clone(), Value::Int(n_signed)];
                    if let Some(default) = args.get(2) {
                        call_args.push(default.clone());
                    }
                    return interp.call(&f, &call_args);
                }
            }
        }
        // C3b (measured): a map or set is NOT `Indexed` on the JVM, and
        // real `RT.nth` throws for it UNCONDITIONALLY -- even the 3-arity
        // default-value form still throws (`(nth {:a 1} 0 :dflt)` throws
        // too, it does NOT return `:dflt`) -- so this has to preempt the
        // negative-index/default handling below, not fall through to it.
        // Oracle transcript: `(nth {:a 1} 0)` and `(nth #{1} 0)` both
        // throw `UnsupportedOperationException: nth not supported on this
        // type: PersistentArrayMap` / `PersistentHashSet` respectively
        // (real Clojure names the concrete receiver class; mova has no
        // `Small`/`Big`-map split to preserve here for `Set`, but does for
        // `Map`, hence the two-way match below).
        let unsupported_nth_class = match &args[0] {
            Value::Map(PMap::Small(_)) => Some("PersistentArrayMap"),
            Value::Map(PMap::Big(_) | PMap::Shaped(_)) => Some("PersistentHashMap"),
            Value::SortedMap(_) => Some("PersistentTreeMap"),
            Value::Set(_) => Some("PersistentHashSet"),
            Value::SortedSet(_) => Some("PersistentTreeSet"),
            _ => None,
        };
        if let Some(class_name) = unsupported_nth_class {
            // W3a: measured -- real `RT.nth` on a map/set raises
            // `java.lang.UnsupportedOperationException` (this exact
            // message), not a bare `RuntimeException`.
            return Err(RjError::other(format!("nth not supported on this type: {class_name}"))
                .with_class(JvmClass::UnsupportedOperation));
        }
        // A negative index is "not found", same as an out-of-bounds one:
        // the 3-arity default covers it too (`(nth [] -1 :d)` => `:d`),
        // and only the 2-arity form still throws (`(nth [] -1)` throws).
        if n_signed < 0 {
            return match args.get(2) {
                Some(default) => Ok(default.clone()),
                None => Err(RjError::other(format!("nth: negative index {n_signed}"))
                    .with_class(nth_index_class(&args[0]))),
            };
        }
        let n = n_signed as usize;
        // S4: a typed vector indexes exactly like a plain `Vector` --
        // `.data` is already `PVec`.
        let vector_items = match &args[0] {
            // S7 (measured): `(nth (first {:a 1}) 0)` => `:a`,
            // `(nth .. 2)` throws, `(nth .. 2 :dflt)` => `:dflt`.
            Value::Vector(items) | Value::MapEntry(items) => Some(items),
            Value::TypedVec(tv) => Some(&tv.data),
            _ => None,
        };
        if let Some(items) = vector_items {
            if let Some(v) = items.get_owned(n) {
                return Ok(v);
            }
            return match args.get(2) {
                Some(default) => Ok(default.clone()),
                None => Err(RjError::other(format!(
                    "nth: index {n} out of bounds for vector of length {}",
                    items.len()
                ))
                .with_class(JvmClass::IndexOutOfBounds)),
            };
        }
        // A string's generic path below would go through `uncons` ->
        // `Interp::seq_items`, which materializes the WHOLE string into an
        // `imbl::Vector<Value>` of `Value::Char`s before ever looking at
        // index `n` -- expensive on its own, and callers that walk a
        // string char-by-char via repeated single-index `nth` (e.g.
        // omawrite's `oma.core.layout/wrap-line`, one `(nth line i)` per
        // column while word-wrapping) pay that FULL materialization on
        // EVERY call, O(line length) allocation per character. `str::nth`
        // walks the underlying `char_indices` iterator directly with no
        // allocation at all.
        if let Value::Str(s) = &args[0] {
            // M8: `Str::char_at` is rope-native (`PText::char_at`, `O(log
            // n)`, no materialization) for a `Rope` source; for `Flat`,
            // the same ASCII-fast-path-then-scan logic as pre-M8. This is
            // omawrite's `oma.core.layout/wrap-line` hot loop (one `(nth
            // line i)` per column while word-wrapping) -- see the comment
            // above for the O(n^2) history this fixed originally.
            return match s.char_at(n) {
                Some(c) => Ok(Value::Char(c)),
                None => match args.get(2) {
                    Some(default) => Ok(default.clone()),
                    None => Err(RjError::other(format!("nth: index {n} out of bounds"))
                        .with_class(JvmClass::StringIndexOutOfBounds)),
                },
            };
        }
        // S7 (tail wave), measured: real `clojure.lang.RT/nth` special-cases
        // `java.util.regex.Matcher` to index straight into its LAST match's
        // groups (`.group(i)`), same shape `re-groups` already returns here
        // (index 0 = whole match, 1..N = capture groups) -- `(nth m i)`
        // does NOT go through `seq`/`uncons` at all on the JVM (a bare
        // `Matcher` has no `Iterable`/`Seqable` there either), which is
        // exactly what other_functions.clj's `test-regex-matcher` caught
        // ("don't know how to create a seq from matcher" -- `nth` was
        // falling all the way through to the generic `uncons` path below).
        if let Value::Matcher(m) = &args[0] {
            let guard = crate::sync::lock_mutex(m);
            let group = match &guard.last_match {
                Some(Value::Vector(items)) => items.get_owned(n),
                Some(other) if n == 0 => Some(other.clone()),
                _ => None,
            };
            // W3a: "no match found yet" and "no such group in the match
            // there IS" are DIFFERENT JVM classes -- see
            // `nth_index_class`/`matcher_nth_class`. Read while the lock is
            // still held rather than re-locking below.
            let class = if guard.last_match.is_some() {
                JvmClass::IndexOutOfBounds
            } else {
                JvmClass::IllegalState
            };
            drop(guard);
            return match group {
                Some(v) => Ok(v),
                None => match args.get(2) {
                    Some(default) => Ok(default.clone()),
                    // W3a: measured -- real `RT.nth` on a `Matcher` calls
                    // `.group(i)`, which raises
                    // `java.lang.IllegalStateException("No match found")`
                    // whenever the group is not available. mova keeps its
                    // own (more specific) wording; only the class matches.
                    None => Err(RjError::other(format!("nth: index {n} out of bounds for matcher"))
                        .with_class(class)),
                },
            };
        }
        // S4/1D: direct index, same shape as the `Vector`/`Str` fast paths
        // above -- an array's generic `uncons` path (`Interp::seq_items`)
        // would materialize the WHOLE array into a `PVec` first.
        if let Value::Array(arr) = &args[0] {
            let data = crate::sync::lock_mutex(&arr.data);
            if let Some(v) = data.get(n) {
                return Ok(v.clone());
            }
            return match args.get(2) {
                Some(default) => Ok(default.clone()),
                None => Err(RjError::other(format!(
                    "nth: index {n} out of bounds for array of length {}",
                    data.len()
                ))
                .with_class(JvmClass::ArrayIndexOutOfBounds)),
            };
        }
        let mut cur = args[0].clone();
        let mut i = 0usize;
        loop {
            match uncons(interp, &cur)? {
                Some((h, t)) => {
                    if i == n {
                        return Ok(h);
                    }
                    cur = t;
                    i += 1;
                }
                None => {
                    return match args.get(2) {
                        Some(default) => Ok(default.clone()),
                        None => Err(RjError::other(format!("nth: index {n} out of bounds"))
                            .with_class(nth_index_class(&args[0]))),
                    }
                }
            }
        }
    });

    reg(i, "count", ArityHint::Exact(1), |interp, args| Ok(Value::Int(count_value(interp, &args[0])?)));

    reg_unmeta(i, "contains?", ArityHint::Exact(2), |interp, args| {
        Ok(Value::Bool(match &args[0] {
            Value::Nil => false,
            // C3e: retry-after-miss with a realized lazy probe -- same
            // shape (and same "hot lookups pay nothing" property) as
            // `get`'s own arms. See `normalized_probe`.
            Value::Map(m) => {
                m.contains_key(&args[1])
                    || match normalized_probe(interp, &args[1])? {
                        Some(k) => m.contains_key(&k),
                        None => false,
                    }
            }
            Value::HostStruct(hs) => match &args[1] {
                Value::Keyword(kw) => crate::host_struct::shape_field_index(&hs.shape, kw.text_ref()).is_some(),
                _ => crate::host_struct::as_pmap(hs).contains_key(&args[1]),
            },
            Value::LazyMap(hs) => match &args[1] {
                Value::Keyword(kw) => crate::lazy_map::contains_key(hs, kw.text_ref()),
                _ => false,
            },
            Value::Set(s) => {
                s.contains(&args[1])
                    || match normalized_probe(interp, &args[1])? {
                        Some(k) => s.contains(&k),
                        None => false,
                    }
            }
            // S3: records answer key membership on their full map view.
            Value::Inst(inst) if inst.tdef.is_record => inst.data.contains_key(&args[1]),
            // S7 (measured): `(contains? (first {:a 1}) 0)` => `true`,
            // `(contains? .. 2)` => `false`.
            Value::Vector(items) | Value::MapEntry(items) => {
                matches!(&args[1], Value::Int(n) if *n >= 0 && (*n as usize) < items.len())
            }
            // S4/1D: index membership, same shape as `Vector` above
            // (measured: `(contains? (into-array [1]) 0)` true, `(contains?
            // (into-array [1]) 1)` false).
            Value::Array(arr) => {
                matches!(&args[1], Value::Int(n) if *n >= 0 && (*n as usize) < crate::sync::lock_mutex(&arr.data).len())
            }
            // S4
            Value::SortedMap(m) => crate::builtins::sorted::sorted_map_contains(interp, m, &args[1]),
            Value::SortedSet(s) => crate::builtins::sorted::sorted_set_contains(interp, s, &args[1]),
            Value::TypedVec(tv) => matches!(&args[1], Value::Int(n) if *n >= 0 && (*n as usize) < tv.data.len()),
            // C2 (defstruct)
            Value::StructMap(m) => crate::builtins::structmap::struct_map_get(m, &args[1]).is_some(),
            // C10: `java.util.HashMap`/`HashSet` -- measured, `contains?`
            // works on both (same as their mova-native `Map`/`Set`
            // counterparts); `java.util.ArrayList` is NOT associative
            // (measured on the oracle: throws same as the catch-all
            // below), so it deliberately falls through un-matched here.
            Value::HostInst(h) if h.kind == crate::hostclass::HostKind::HashMap => {
                let guard = crate::sync::lock_mutex(&h.state);
                let crate::hostclass::HostState::HashMap(m) = &*guard else {
                    unreachable!("HostKind::HashMap always holds HostState::HashMap");
                };
                m.contains_key(&args[1])
            }
            Value::HostInst(h) if h.kind == crate::hostclass::HostKind::HashSet => {
                let guard = crate::sync::lock_mutex(&h.state);
                let crate::hostclass::HostState::HashSet(s) = &*guard else {
                    unreachable!("HostKind::HashSet always holds HostState::HashSet");
                };
                s.contains(&args[1])
            }
            other => {
                return Err(RjError::type_err(format!(
                    "contains?: not associative: {}",
                    other.type_name()
                )))
            }
        }))
    });

    reg_unmeta(i, "keys", ArityHint::Exact(1), |_interp, args| match &args[0] {
        // `(keys nil)` is nil in Clojure (total over "no map"), like vals.
        Value::Nil => Ok(Value::Nil),
        Value::Map(m) if m.is_empty() => Ok(Value::Nil),
        // C3e: the raw stored keys/values, verbatim. Up to C3e both `keys`
        // and `vals` ran their result through a `defuse_trailing_lazy`
        // helper that FORCED a trailing `Value::Lazy` element, because a
        // stored lazy value landing in the last slot (`{:a (range 1 100)}`)
        // made the result indistinguishable from the internal continuation
        // marker and got spliced open by the next walker (measured on
        // `test-vec-compare`'s `(zipmap ... (vals num-seqs))`). The marker
        // has its own discriminant now, so a `Lazy` slot is unambiguously
        // one opaque element and the whole helper (plus its 8 call sites)
        // is gone -- along with the eager realization it forced on values
        // nobody asked for.
        Value::Map(m) => Ok(Value::List(m.keys().cloned().collect())),
        // Shape order, touch-only (no `as_pmap` materialize) -- matches
        // `seq`/`vals`/print's ordering guarantee.
        Value::HostStruct(hs) if hs.shape.fields.is_empty() => Ok(Value::Nil),
        Value::HostStruct(hs) => Ok(Value::List(
            hs.shape.fields.iter().map(|f| Value::Keyword(Keyword::from(&f.key))).collect(),
        )),
        Value::LazyMap(lm) if lm_empty(lm) => Ok(Value::Nil),
        Value::LazyMap(lm) => Ok(Value::List(crate::lazy_map::as_pmap(lm).keys().cloned().collect())),
        // S3: basis declaration order first, then ext keys (measured).
        Value::Inst(inst) if inst.tdef.is_record => {
            if inst.data.is_empty() {
                Ok(Value::Nil)
            } else {
                let ks = inst.ordered_entries().into_iter().map(|(k, _)| k).collect();
                Ok(Value::List(ks))
            }
        }
        // S4: already comparator-sorted (see `SortedMapVal`'s doc).
        Value::SortedMap(m) if m.entries.is_empty() => Ok(Value::Nil),
        Value::SortedMap(m) => {
            let ks = m.entries.iter().map(|(k, _)| k.clone()).collect();
            Ok(Value::List(ks))
        }
        // C2 (defstruct): basis order first, then ext keys (measured) --
        // exactly `entries`' own layout, no re-sort needed.
        Value::StructMap(m) if m.entries.is_empty() => Ok(Value::Nil),
        Value::StructMap(m) => {
            let ks = m.entries.iter().map(|(k, _)| k.clone()).collect();
            Ok(Value::List(ks))
        }
        // C10: `keys`/`vals` on a NON-map are total over the EMPTY case
        // only -- measured, `(keys ())`/`(keys [])`/`(keys #{})`/`(keys
        // "")` are all `nil` (real `RT.keys` is `(seq (map key coll))`-
        // shaped: `seq` on an empty collection short-circuits to `nil`
        // BEFORE ever reaching the per-element `Map$Entry` cast that a
        // NON-empty non-map collection would fail -- `(keys (list 1 2))`
        // throws `ClassCastException` on the real JVM, not covered here
        // since the vendored suite never exercises it).
        Value::List(items) | Value::Vector(items) if items.is_empty() => Ok(Value::Nil),
        Value::Set(s) if s.is_empty() => Ok(Value::Nil),
        Value::Str(s) if s.is_empty() => Ok(Value::Nil),
        other => Err(RjError::type_err(format!("keys: not a map: {}", other.type_name()))),
    });

    reg_unmeta(i, "vals", ArityHint::Exact(1), |_interp, args| match &args[0] {
        Value::Nil => Ok(Value::Nil),
        Value::Map(m) if m.is_empty() => Ok(Value::Nil),
        Value::Map(m) => Ok(Value::List(m.values().cloned().collect())),
        Value::HostStruct(hs) if hs.shape.fields.is_empty() => Ok(Value::Nil),
        Value::HostStruct(hs) => Ok(Value::List(
            (0..hs.shape.fields.len()).map(|idx| crate::host_struct::get_field(hs, idx)).collect(),
        )),
        Value::LazyMap(lm) if lm_empty(lm) => Ok(Value::Nil),
        Value::LazyMap(lm) => Ok(Value::List(crate::lazy_map::as_pmap(lm).values().cloned().collect())),
        // S3: same order as `keys` above.
        Value::Inst(inst) if inst.tdef.is_record => {
            if inst.data.is_empty() {
                Ok(Value::Nil)
            } else {
                let vs = inst.ordered_entries().into_iter().map(|(_, v)| v).collect();
                Ok(Value::List(vs))
            }
        }
        // S4
        Value::SortedMap(m) if m.entries.is_empty() => Ok(Value::Nil),
        Value::SortedMap(m) => {
            let vs = m.entries.iter().map(|(_, v)| v.clone()).collect();
            Ok(Value::List(vs))
        }
        // C2 (defstruct)
        Value::StructMap(m) if m.entries.is_empty() => Ok(Value::Nil),
        Value::StructMap(m) => {
            let vs = m.entries.iter().map(|(_, v)| v.clone()).collect();
            Ok(Value::List(vs))
        }
        // C10: see `keys`'s own identical arms just above for why this is
        // total over the EMPTY case only.
        Value::List(items) | Value::Vector(items) if items.is_empty() => Ok(Value::Nil),
        Value::Set(s) if s.is_empty() => Ok(Value::Nil),
        Value::Str(s) if s.is_empty() => Ok(Value::Nil),
        other => Err(RjError::type_err(format!("vals: not a map: {}", other.type_name()))),
    });

    reg(i, "first", ArityHint::Exact(1), |interp, args| {
        Ok(uncons(interp, &args[0])?.map(|(h, _)| h).unwrap_or(Value::Nil))
    });

    reg(i, "rest", ArityHint::Exact(1), |interp, args| {
        Ok(uncons(interp, &args[0])?
            .map(|(_, t)| t)
            .unwrap_or_else(|| Value::List(PVec::new())))
    });

    // v0.5 / perf: once the first element is peeled off, a `List` tail's
    // emptiness is answered directly from its own length -- no second
    // `uncons` call needed. `uncons` on a `List` tail never forces
    // anything anyway (see `uncons`'s own `items.len() == 2 && Lazy` arm,
    // which just clones the pair without forcing), so this is a pure cost
    // cut, not a laziness change; a `Lazy` tail still goes through
    // `uncons` (which forces exactly one element) to answer correctly.
    reg(i, "next", ArityHint::Exact(1), |interp, args| match uncons(interp, &args[0])? {
        None => Ok(Value::Nil),
        Some((_, tail)) => match &tail {
            Value::List(items) if items.is_empty() => Ok(Value::Nil),
            Value::List(_) => Ok(tail),
            _ => {
                if uncons(interp, &tail)?.is_none() {
                    Ok(Value::Nil)
                } else {
                    Ok(tail)
                }
            }
        },
    });

    reg(i, "cons", ArityHint::Exact(2), |interp, args| {
        cons_builtin(interp, args[0].clone(), args[1].clone())
    });

    reg(i, "seq", ArityHint::Exact(1), |interp, args| seq_of(interp, &args[0]));

    // C3b (measured): `iterator-seq` on the ONE `java.util.Iterator`
    // representation mova actually has -- `.iterator`'s own
    // `HostKind::Iterator` (see its doc in `hostclass.rs`), which snapshots
    // its whole source into a `HostState::Iterator(Value::List(..) |
    // Value::Nil)` at construction time rather than pulling lazily. That
    // snapshot means there is no further host-side computation for THIS
    // representation's `iterator-seq` to defer -- unwrapping straight to
    // `seq_of` on the held remainder is observably identical to walking
    // `.hasNext`/`.next` one at a time, since both terminate on the same
    // already-fully-realized data. Any other Iterator source doesn't
    // exist in mova today, so this is deliberately narrow (measured against
    // `(reduce + (iterator-seq (.iterator (range 100))))` => `4950`).
    reg(i, "iterator-seq", ArityHint::Exact(1), |interp, args| match &args[0] {
        Value::HostInst(h) if h.kind == crate::hostclass::HostKind::Iterator => {
            let remaining = {
                let guard = crate::sync::lock_mutex(&h.state);
                let crate::hostclass::HostState::Iterator(remaining) = &*guard else {
                    unreachable!("HostKind::Iterator always holds HostState::Iterator");
                };
                remaining.clone()
            };
            seq_of(interp, &remaining)
        }
        other => Err(RjError::type_err(format!(
            "iterator-seq: expected a java.util.Iterator, got {}",
            other.type_name()
        ))),
    });

    // C13: renamed from `into` -- mova's `core.mova` now owns that name,
    // adding the missing 0-arity (`[]`) and the 3-arity TRANSDUCER form
    // (`(into to xform from)`, routed through `transduce`/`conj`) on top
    // of this native's unchanged 1/2-arity behavior. Same rename-and-wrap
    // shape as `take-coll*`/`drop-coll*`/`partition-all-coll*`/
    // `distinct-coll*` above.
    reg_preserving_meta(i, "into-coll*", ArityHint::Range(1, 2), |interp, args| {
        let mut to = args[0].clone();
        if args.len() == 1 {
            return Ok(to);
        }
        let mut cur = args[1].clone();
        while let Some((h, t)) = uncons(interp, &cur)? {
            to = conj_one(interp, &to, &h)?;
            cur = t;
        }
        Ok(to)
    });

    reg(i, "empty?", ArityHint::Exact(1), |interp, args| {
        // M8: a string's generic path goes through `uncons` -> `Interp::
        // seq_items`, which -- Rope or Flat -- materializes the WHOLE
        // string into a seq of `Value::Char`s just to check whether that
        // seq is empty (pre-existing behavior, not new here). `Str::
        // is_empty` is O(1) for both representations and semantically
        // identical (`uncons` returns `None` iff the string is empty), so
        // special-case it rather than let a `blank?`/`empty?` check on a
        // big `:editor/text` (the spec's own named "must be rope-native"
        // op) pay a full-document allocation.
        if let Value::Str(s) = &args[0] {
            return Ok(Value::Bool(s.is_empty()));
        }
        Ok(Value::Bool(uncons(interp, &args[0])?.is_none()))
    });

    reg_preserving_meta(i, "empty", ArityHint::Exact(1), |_i, args| match &args[0] {
        Value::Nil => Ok(Value::Nil),
        Value::List(_) | Value::Lazy(_) => Ok(Value::List(PVec::new())),
        Value::Vector(_) => Ok(Value::Vector(PVec::new())),
        // S7: `nil`, NOT an empty vector -- measured,
        // `(empty (first {:a 1}))` is `nil` and `(class ..)` is `nil` too
        // (real `MapEntry.empty()` throws `UnsupportedOperationException`,
        // which `clojure.core/empty`'s `(when (coll? coll) ...)`-free
        // `IPersistentCollection` path turns into `nil` for this one
        // class). This is why `clojure.walk` cannot round-trip an entry
        // through `(into (empty form) ..)` and dispatches on `IMapEntry`
        // first instead.
        Value::MapEntry(_) => Ok(Value::Nil),
        Value::Map(_) => Ok(Value::Map(PMap::new())),
        // `(empty host-struct)` widens to a plain empty `Map` -- there is
        // no meaningful "empty HostStruct" (its shape is fixed at
        // registration), matching `assoc`/`dissoc`/`conj`'s v1 policy.
        Value::HostStruct(_) => Ok(Value::Map(PMap::new())),
        Value::LazyMap(_) => Ok(Value::Map(PMap::new())),
        Value::Set(_) => Ok(Value::Set(champ::PersistentHashSet::new())),
        // S4: `(empty (sorted-map-by > ...))`/`(empty (vector-of :int ...))`
        // KEEP their comparator/kind (measured: conj-ing back into an
        // emptied sorted-by-`>` map still sorts descending; conj-ing a
        // float back into an emptied `:int` vec still truncates it) --
        // see this module's doc + `SortedMapVal`/`TypedVecVal`'s own.
        Value::SortedMap(m) => Ok(Value::SortedMap(std::sync::Arc::new(crate::value::SortedMapVal {
            cmp: m.cmp.clone(),
            entries: Vec::new(),
        }))),
        Value::SortedSet(s) => Ok(Value::SortedSet(std::sync::Arc::new(crate::value::SortedSetVal {
            cmp: s.cmp.clone(),
            entries: Vec::new(),
        }))),
        Value::TypedVec(tv) => Ok(Value::TypedVec(std::sync::Arc::new(crate::value::TypedVecVal {
            kind: tv.kind,
            data: PVec::new(),
        }))),
        // C2 (defstruct), measured: `(empty s)` is NOT `{}` -- it's a
        // fresh struct on the SAME basis with every basis value reset to
        // `nil` (matching `struct`'s own no-vals-supplied default), same
        // "keep my identity, reset my contents" policy `SortedMap`/
        // `TypedVec` use above for their comparator/kind.
        Value::StructMap(sm) => Ok(Value::StructMap(std::sync::Arc::new(crate::value::StructMapVal {
            basis: sm.basis.clone(),
            entries: crate::builtins::structmap::new_entries(&sm.basis),
        }))),
        // S4 (everyday3), measured: a record IS an `IPersistentCollection`
        // (it behaves like a map everywhere else -- `get`/`assoc`/`seq`/...)
        // but its own `.empty()` override is the ONE place it deliberately
        // does NOT act like a plain map -- real Clojure throws
        // `UnsupportedOperationException: Can't create empty: <class>`
        // rather than widening to `{}` (there is no way to rebuild a
        // record's required basis fields from nothing). A deftype instance
        // isn't a collection at all and falls through to the catch-all
        // below, same as any other non-collection value.
        Value::Inst(inst) if inst.tdef.is_record => Err(RjError::other(format!(
            "empty: can't create empty: {}",
            inst.tdef.name
        ))),
        // Real Clojure's `empty`: `(if (instance? IPersistentCollection
        // coll) (.empty coll) (when (nil? coll) nil))` -- for anything that
        // ISN'T an `IPersistentCollection` (a number, string, keyword, a
        // deftype instance, ...) that `when` returns `nil` unconditionally
        // (its `nil?` test only ever gates which branch computes the SAME
        // `nil`), not an exception. Measured: `(empty "abc")`/`(empty 5)`/
        // `(empty :a)` are all `nil`.
        _ => Ok(Value::Nil),
    });

    reg_unmeta(i, "peek", ArityHint::Exact(1), |_i, args| match &args[0] {
        Value::Nil => Ok(Value::Nil),
        Value::List(items) => Ok(items.front().cloned().unwrap_or(Value::Nil)),
        // S7 (measured): `(peek (first {:a 1}))` => `1`, the val slot.
        Value::Vector(items) | Value::MapEntry(items) => Ok(items.len().checked_sub(1).and_then(|i| items.get_owned(i)).unwrap_or(Value::Nil)),
        Value::TypedVec(tv) => Ok(tv.data.back().cloned().unwrap_or(Value::Nil)),
        // C10: a queue peeks its FRONT (index 0) -- FIFO, opposite end
        // from `Vector`'s `.back()` above (measured: `(peek (conj EMPTY 1
        // 2 3))` is `1`, the first-conj'd element, not `3`).
        Value::Queue(items) => Ok(items.front().cloned().unwrap_or(Value::Nil)),
        other => Err(RjError::type_err(format!("peek: not a stack: {}", other.type_name()))),
    });

    reg_preserving_meta(i, "pop", ArityHint::Exact(1), |_i, args| match &args[0] {
        // C10: measured `(= (pop nil) nil)` -- `pop` is TOTAL over `nil`,
        // same as `peek` above (`data_structures.clj`'s `test-pop`).
        Value::Nil => Ok(Value::Nil),
        Value::List(items) if !items.is_empty() => {
            let mut v = items.clone();
            v.pop_front();
            Ok(Value::List(v))
        }
        // S7: promotes -- measured, `(pop (first {:a 1}))` is `[:a]` and
        // `(class ..)` is `clojure.lang.PersistentVector`.
        Value::Vector(items) | Value::MapEntry(items) if !items.is_empty() => {
            let mut v = items.clone();
            v.pop_back();
            Ok(Value::Vector(v))
        }
        Value::TypedVec(tv) if !tv.data.is_empty() => {
            let mut data = tv.data.clone();
            data.pop_back();
            Ok(Value::TypedVec(std::sync::Arc::new(crate::value::TypedVecVal { kind: tv.kind, data })))
        }
        // C10: FIFO -- pops the FRONT (opposite end from `conj`'s back-
        // push below), and -- UNLIKE `List`/`Vector`/`TypedVec` above --
        // popping an ALREADY-EMPTY queue is not an error at all (measured:
        // `(pop clojure.lang.PersistentQueue/EMPTY)` returns another empty
        // queue, `=` to the original; real `List`/`Vector` throw instead).
        Value::Queue(items) => {
            let mut v = items.clone();
            v.pop_front();
            Ok(Value::Queue(v))
        }
        // W3a/W4B-MESSAGES: measured -- `(pop ())` => `java.lang.
        // IllegalStateException: "Can't pop empty list"`, `(pop [])` =>
        // `IllegalStateException: "Can't pop empty vector"`
        // (data_structures.clj's `test-stack` asserts both by class;
        // transients.clj's `popping-off` also checks the vector wording
        // by message, via `pop!` -- core.mova's `pop!` is a thin
        // `check-transient!` + `pop` wrapper, so this one site covers
        // both). Real Clojure's own wording verbatim now (capital "Can't
        // pop", no "pop: " prefix) -- ex-message on this class is
        // genuinely non-nil, so the old lowercase wording was a real,
        // checked mismatch, not a rubber-stamped pass.
        Value::List(_) => {
            Err(RjError::other("Can't pop empty list").with_class(JvmClass::IllegalState))
        }
        Value::Vector(_) | Value::TypedVec(_) => {
            Err(RjError::other("Can't pop empty vector").with_class(JvmClass::IllegalState))
        }
        other => Err(RjError::type_err(format!("pop: not a stack: {}", other.type_name()))),
    });

    reg(i, "subvec", ArityHint::Range(2, 3), |_i, args| {
        // S4: `subvec` of a typed vector degrades to a plain untyped
        // `Vector` (measured: `(class (subvec (vector-of :int 1 2 3 4 5) 1
        // 3))` is `clojure.lang.APersistentVector$SubVector`, never
        // `clojure.core.Vec` -- mova doesn't model the `SubVector` class
        // distinctly from `PersistentVector` either way, so this just
        // reuses the plain-`Vector` arm's slicing).
        let items = match &args[0] {
            // S7: promotes (the `Ok(Value::Vector(..))` below) -- measured,
            // `(subvec (first {:a 1}) 1)` is `[1]`.
            Value::Vector(items) | Value::MapEntry(items) => items,
            Value::TypedVec(tv) => &tv.data,
            other => return Err(RjError::type_err(format!("subvec: not a vector: {}", other.type_name()))),
        };
        let start = require_index(&args[1], "subvec")?;
        let end = match args.get(2) {
            Some(v) => require_index(v, "subvec")?,
            None => items.len(),
        };
        if start > end || end > items.len() {
            return Err(RjError::other(format!(
                "subvec: range [{start}, {end}) out of bounds for vector of length {}",
                items.len()
            )));
        }
        let sliced = items.clone();
        Ok(Value::Vector(sliced.slice(start..end)))
    });

    // `update` is on the whitelist only because it came free: its final act
    // is already an assoc through values it already owns, so pointing that
    // at `assoc_owned` (and moving, rather than cloning, the receiver out of
    // the args) is a substitution, not new machinery. Both entry points
    // share `update_body` so the two can't drift.
    reg_consuming_preserving_meta(
        i,
        "update",
        ArityHint::Min(3),
        |interp, args| {
            let extra: Vec<Value> = args[3..].to_vec();
            update_body(interp, args[0].clone(), args[1].clone(), args[2].clone(), extra)
        },
        |interp, args| {
            let extra: Vec<Value> = args[3..].to_vec();
            let coll = take_arg(&mut args[0]);
            let k = take_arg(&mut args[1]);
            let f = take_arg(&mut args[2]);
            update_body(interp, coll, k, f, extra)
        },
    );

    reg_preserving_meta(i, "assoc-in", ArityHint::Exact(3), |interp, args| {
        let path = match &args[1] {
            Value::Vector(items) | Value::MapEntry(items) => items.clone(),
            other => return Err(RjError::type_err(format!("assoc-in: path must be a vector, got {}", other.type_name()))),
        };
        assoc_in(interp, &args[0], &path, args[2].clone())
    });

    reg(i, "get-in", ArityHint::Range(2, 3), |interp, args| {
        let path = get_in_path(interp, &args[1])?;
        let default = args.get(2).cloned().unwrap_or(Value::Nil);
        Ok(get_in(interp, &args[0], &path)?.unwrap_or(default))
    });

    reg_preserving_meta(i, "update-in", ArityHint::Min(3), |interp, args| {
        let path = get_in_path(interp, &args[1])?;
        let f = args[2].clone();
        let current = get_in(interp, &args[0], &path)?.unwrap_or(Value::Nil);
        let mut call_args = vec![current];
        call_args.extend_from_slice(&args[3..]);
        let new_val = interp.call_owned(&f, call_args)?;
        assoc_in(interp, &args[0], &path, new_val)
    });

    reg_preserving_meta(i, "merge", ArityHint::Any, |_i, args| {
        // S3: the accumulator keeps the FIRST non-nil argument's identity
        // (merge is conj-folding in real Clojure), so `(merge record m)`
        // stays a record (measured) while plain-map merging is unchanged.
        fn pairs_of(v: &Value) -> Result<Vec<(Value, Value)>, RjError> {
            match v {
                // S5/M3: reading a later argument's ENTRIES is a read, so
                // it sees through metadata -- and crucially does not carry
                // that metadata anywhere. This is what makes `merge`
                // asymmetric: `(meta (merge (with-meta {:a 1} {:m 1})
                // {:b 2}))` is `{:m 1}` but `(meta (merge {:b 2}
                // (with-meta {:a 1} {:m 1})))` is `nil` (both measured).
                // The first map's metadata survives because `merge` is a
                // conj-fold onto it -- `reg_preserving_meta` above puts it
                // back -- while a later map only ever contributes entries.
                Value::Meta(m) => pairs_of(&m.inner),
                Value::Map(m) => Ok(m.iter().map(|(k, v)| (k.clone(), v.clone())).collect()),
                Value::HostStruct(hs) => Ok(crate::host_struct::as_pmap(hs)
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect()),
                Value::LazyMap(hs) => Ok(crate::lazy_map::as_pmap(hs)
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect()),
                Value::Inst(inst) if inst.tdef.is_record => {
                    Ok(inst.data.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                }
                other => Err(RjError::type_err(format!(
                    "merge: not a map: {}",
                    other.type_name()
                ))),
            }
        }
        let mut result: Option<Value> = None;
        for a in args {
            if matches!(a, Value::Nil) {
                continue;
            }
            match &mut result {
                None => {
                    // Validate the seed is map-ish, then adopt it whole.
                    pairs_of(a)?;
                    let seed = match a {
                        Value::HostStruct(hs) => Value::Map(crate::host_struct::as_pmap(hs).clone()),
                        Value::LazyMap(hs) => Value::Map(crate::lazy_map::as_pmap(hs).clone()),
                        other => other.clone(),
                    };
                    result = Some(seed);
                }
                Some(acc) => {
                    let pairs = pairs_of(a)?;
                    match acc {
                        Value::Map(m) => {
                            map_probe::record("merge", m.len());
                            for (k, v) in pairs {
                                m.insert(k, v);
                            }
                        }
                        Value::Inst(inst) => {
                            let mut data = inst.data.clone();
                            for (k, v) in pairs {
                                data.insert(k, v);
                            }
                            *acc = Value::Inst(std::sync::Arc::new(crate::types::InstVal {
                                tdef: inst.tdef.clone(),
                                data,
                                fields: std::sync::Mutex::new(crate::value::PVec::new()),
                                meta: inst.meta.clone(),
                            }));
                        }
                        _ => unreachable!("seed is always Map or record Inst"),
                    }
                }
            }
        }
        // `(merge)`/`(merge nil nil)` => `nil`, matching Clojure -- only a
        // real map-ish argument ever produces a result.
        Ok(result.unwrap_or(Value::Nil))
    });

    // S5/M3: `merge-with`'s receiver is `args[1]`, not `args[0]` (which
    // is the combining fn), so it can't use `reg_preserving_meta` --
    // `preserve_from` below plays that role by hand. Measured: `(meta
    // (merge-with + (with-meta {:x 1} {:a 1}) {:z 1}))` is `{:a 1}`, the
    // FIRST map's metadata, exactly like `merge`.
    reg(i, "merge-with", ArityHint::Min(1), |interp, args| {
        let f = args[0].clone();
        let preserve_from = args.get(1).cloned().unwrap_or(Value::Nil);
        let mut result: Option<PMap> = None;
        for a in &args[1..] {
            match a {
                Value::Nil => {}
                Value::Map(m) => {
                    let acc = result.get_or_insert_with(PMap::new);
                    map_probe::record("merge-with", acc.len());
                    for (k, v) in m.iter() {
                        let merged = match acc.get(k) {
                            Some(existing) => interp.call(&f, &[existing.clone(), v.clone()])?,
                            None => v.clone(),
                        };
                        acc.insert(k.clone(), merged);
                    }
                }
                Value::HostStruct(hs) => {
                    let m = crate::host_struct::as_pmap(hs);
                    let acc = result.get_or_insert_with(PMap::new);
                    map_probe::record("merge-with", acc.len());
                    for (k, v) in m.iter() {
                        let merged = match acc.get(k) {
                            Some(existing) => interp.call(&f, &[existing.clone(), v.clone()])?,
                            None => v.clone(),
                        };
                        acc.insert(k.clone(), merged);
                    }
                }
                Value::LazyMap(hs) => {
                    let m = crate::lazy_map::as_pmap(hs);
                    let acc = result.get_or_insert_with(PMap::new);
                    map_probe::record("merge-with", acc.len());
                    for (k, v) in m.iter() {
                        let merged = match acc.get(k) {
                            Some(existing) => interp.call(&f, &[existing.clone(), v.clone()])?,
                            None => v.clone(),
                        };
                        acc.insert(k.clone(), merged);
                    }
                }
                // C14 (protocols): `defrecord-acts-like-a-map`'s own
                // `(merge-with + rec {:a 10 :c 10})` -- read the record's
                // `data` map like the `Map`/`HostStruct` arms above.
                // Output is always a plain `Value::Map` (same as those
                // two), never a record -- `.equals`'s own class-agnostic
                // contract (this file's `eval_dot_form` `Value::Map`
                // arm) is what makes the result still compare `true`
                // against the test's plain map literal, so nothing here
                // needs to preserve record-ness.
                Value::Inst(inst) if inst.tdef.is_record => {
                    let acc = result.get_or_insert_with(PMap::new);
                    map_probe::record("merge-with", acc.len());
                    for (k, v) in inst.data.iter() {
                        let merged = match acc.get(k) {
                            Some(existing) => interp.call(&f, &[existing.clone(), v.clone()])?,
                            None => v.clone(),
                        };
                        acc.insert(k.clone(), merged);
                    }
                }
                other => {
                    // S5/M3: see through a metadata-carrying map argument
                    // (a READ of its entries), then fall into the arms
                    // above via the unwrapped value.
                    if let Value::Meta(_) = other {
                        let inner = other.unmeta().clone();
                        let Value::Map(m) = &inner else {
                            return Err(RjError::type_err(format!(
                                "merge-with: not a map: {}",
                                inner.type_name()
                            )));
                        };
                        let acc = result.get_or_insert_with(PMap::new);
                        for (k, v) in m.iter() {
                            let merged = match acc.get(k) {
                                Some(existing) => interp.call(&f, &[existing.clone(), v.clone()])?,
                                None => v.clone(),
                            };
                            acc.insert(k.clone(), merged);
                        }
                        continue;
                    }
                    return Err(RjError::type_err(format!("merge-with: not a map: {}", other.type_name())));
                }
            }
        }
        Ok(result
            .map(Value::Map)
            .unwrap_or(Value::Nil)
            .with_meta_of(&preserve_from))
    });

    reg_preserving_meta(i, "select-keys", ArityHint::Exact(2), |interp, args| {
        // MOVA-PATCH: records are maps too -- share coerce_map_like (also handles Meta).
        let m = coerce_map_like(&args[0], "select-keys")?;
        let ks = materialize(interp, &args[1])?;
        let mut out = PMap::new();
        for k in ks {
            if let Some(v) = m.get(&k) {
                out.insert(k, v.clone());
            }
        }
        Ok(Value::Map(out))
    });

    reg_preserving_meta(i, "disj", ArityHint::Min(1), |interp, args| match &args[0] {
        Value::Nil => Ok(Value::Nil),
        Value::Set(s) => {
            let mut s2 = s.clone();
            for k in &args[1..] {
                s2 = s2.remove(k);
            }
            Ok(Value::Set(s2))
        }
        // S4
        Value::SortedSet(s) => {
            let mut cur = Value::SortedSet(s.clone());
            for x in &args[1..] {
                let Value::SortedSet(s) = &cur else { unreachable!() };
                cur = crate::builtins::sorted::sorted_set_disj(interp, s, x)?;
            }
            Ok(cur)
        }
        other => Err(RjError::type_err(format!("disj: not a set: {}", other.type_name()))),
    });

    // `(mapv f c1 c2 ... cn)`: eager `map`, stepping every collection in
    // lockstep and stopping as soon as any one is exhausted (same
    // multi-collection contract as core.mova's lazy `map`, minus the
    // laziness), collected straight into a `Vector` instead of round-
    // tripping through a lazy-seq chain -- the point of having a Rust-native
    // `mapv` at all in an editor's hot paths.
    reg(i, "mapv", ArityHint::Min(2), |interp, args| {
        let f = args[0].clone();
        let mut currs: Vec<Value> = args[1..].to_vec();
        let mut out = PVec::new();
        loop {
            let mut heads = Vec::with_capacity(currs.len());
            let mut nexts = Vec::with_capacity(currs.len());
            let mut exhausted = false;
            for c in &currs {
                match uncons(interp, c)? {
                    Some((h, t)) => {
                        heads.push(h);
                        nexts.push(t);
                    }
                    None => {
                        exhausted = true;
                        break;
                    }
                }
            }
            if exhausted {
                break;
            }
            // `heads` is rebuilt per element and dead after the call, so it
            // is handed over (phase 3).
            out.push_back(interp.call_owned(&f, heads)?);
            currs = nexts;
        }
        Ok(Value::Vector(out))
    });

    reg(i, "filterv", ArityHint::Exact(2), |interp, args| {
        let pred = args[0].clone();
        let items = materialize(interp, &args[1])?;
        let mut out = PVec::new();
        for item in items {
            if interp.call(&pred, std::slice::from_ref(&item))?.truthy() {
                out.push_back(item);
            }
        }
        Ok(Value::Vector(out))
    });

    // `reduce-kv` over a `Map` calls `f` with each `[k v]` pair (map
    // iteration order, like everywhere else a `PMap::Big` is iterated);
    // over a `Vector` it calls `f` with each `[index v]` pair instead --
    // Clojure's actual dual contract (`IKVReduce`), not a map-only
    // approximation.
    reg(i, "reduce-kv", ArityHint::Exact(3), |interp, args| {
        let f = args[0].clone();
        let mut acc = args[1].clone();
        // S5/M3: receiver is `args[2]` (see `instance?` for the same
        // shape) -- walking a map's entries is a READ, so it sees through.
        match args[2].unmeta() {
            Value::Nil => {}
            // The accumulator is handed over through one reused buffer,
            // exactly as in `reduce` (phase 3): the temporary args array used
            // to outlive the call and keep `acc` non-unique for the whole of
            // the callee's body.
            Value::Map(m) => {
                let mut buf = [Value::Nil, Value::Nil, Value::Nil];
                for (k, v) in m.iter() {
                    buf[0] = std::mem::replace(&mut acc, Value::Nil);
                    buf[1] = k.clone();
                    buf[2] = v.clone();
                    acc = interp.call_with_buf(&f, &mut buf)?;
                }
            }
            // S3: records reduce-kv like maps (measured), basis-then-ext
            // entry order.
            Value::Inst(inst) if inst.tdef.is_record => {
                let mut buf = [Value::Nil, Value::Nil, Value::Nil];
                for (k, v) in inst.ordered_entries() {
                    buf[0] = std::mem::replace(&mut acc, Value::Nil);
                    buf[1] = k;
                    buf[2] = v;
                    acc = interp.call_with_buf(&f, &mut buf)?;
                }
            }
            // S7: `reduce-kv` over an entry walks it by INDEX, like the
            // vector it is (`IKVReduce`'s vector contract), not as one
            // `(k v)` call.
            Value::Vector(items) | Value::MapEntry(items) => {
                let mut buf = [Value::Nil, Value::Nil, Value::Nil];
                for (idx, v) in items.iter().enumerate() {
                    buf[0] = std::mem::replace(&mut acc, Value::Nil);
                    buf[1] = Value::Int(idx as i64);
                    buf[2] = v.clone();
                    acc = interp.call_with_buf(&f, &mut buf)?;
                }
            }
            // Shape order, touch-only (no `as_pmap` materialize) --
            // consistent with `seq`/`keys`/`vals`/print's ordering.
            Value::LazyMap(lm) => {
                let mut buf = [Value::Nil, Value::Nil, Value::Nil];
                for (k, v) in crate::lazy_map::as_pmap(lm).iter() {
                    buf[0] = std::mem::replace(&mut acc, Value::Nil);
                    buf[1] = k.clone();
                    buf[2] = v.clone();
                    acc = interp.call_with_buf(&f, &mut buf)?;
                }
            }
            Value::HostStruct(hs) => {
                let mut buf = [Value::Nil, Value::Nil, Value::Nil];
                for idx in 0..hs.shape.fields.len() {
                    buf[0] = std::mem::replace(&mut acc, Value::Nil);
                    buf[1] = Value::Keyword(Keyword::from(&hs.shape.fields[idx].key));
                    buf[2] = crate::host_struct::get_field(hs, idx);
                    acc = interp.call_with_buf(&f, &mut buf)?;
                }
            }
            // S4
            Value::SortedMap(m) => {
                let mut buf = [Value::Nil, Value::Nil, Value::Nil];
                for (k, v) in m.entries.iter() {
                    buf[0] = std::mem::replace(&mut acc, Value::Nil);
                    buf[1] = k.clone();
                    buf[2] = v.clone();
                    acc = interp.call_with_buf(&f, &mut buf)?;
                }
            }
            Value::TypedVec(tv) => {
                let mut buf = [Value::Nil, Value::Nil, Value::Nil];
                for (idx, v) in tv.data.iter().enumerate() {
                    buf[0] = std::mem::replace(&mut acc, Value::Nil);
                    buf[1] = Value::Int(idx as i64);
                    buf[2] = v.clone();
                    acc = interp.call_with_buf(&f, &mut buf)?;
                }
            }
            // C14 (protocols): `reduce-kv` over a plain SEQ (measured,
            // `test-base-reduce-kv`'s `(reduce-kv f {} (seq {:a 1 :b
            // 2}))`) -- real Clojure's `reduce-kv` falls back to ordinary
            // `reduce` for anything that isn't `IKVReduce` (a map/vector/
            // record/...), calling `f` with each element destructured as
            // a `[k v]` PAIR, not by index. A non-pair element is a
            // measured `ClassCastException`-shaped error on the JVM;
            // mova has no exception-class taxonomy, so this stays an
            // ordinary `RjError` like every other S3/S4 host error.
            Value::List(items) => {
                let mut buf = [Value::Nil, Value::Nil, Value::Nil];
                for item in items.iter() {
                    let (k, v) = match item {
                        Value::Vector(p) | Value::MapEntry(p) if p.len() == 2 => {
                            (p[0].clone(), p[1].clone())
                        }
                        Value::List(p) if p.len() == 2 => {
                            (p[0].clone(), p[1].clone())
                        }
                        other => {
                            return Err(RjError::type_err(format!(
                                "reduce-kv: cannot treat {} as a [k v] pair",
                                other.type_name()
                            )))
                        }
                    };
                    buf[0] = std::mem::replace(&mut acc, Value::Nil);
                    buf[1] = k;
                    buf[2] = v;
                    acc = interp.call_with_buf(&f, &mut buf)?;
                }
            }
            other => return Err(RjError::type_err(format!("reduce-kv: not a map or vector: {}", other.type_name()))),
        }
        Ok(acc)
    });

    // difference/union/intersection all rebuild from scratch (rather than
    // mutating a cloned receiver in place, imbl::HashSet-style): champ's
    // `PersistentHashSet` has no `&mut self` mutator, only the persistent
    // `&self` `insert`/`remove` (one path-copy per call) and the owned
    // `TransientSet` builder (mutates in place, since it's the sole owner of
    // whatever it was seeded with) -- `TransientSet` is the cheaper of the
    // two for accumulating many elements, so all three go through it.
    reg(i, "difference", ArityHint::Min(1), |_i, args| {
        let mut t = require_set(&args[0], "difference")?.transient();
        for a in &args[1..] {
            for x in require_set(a, "difference")?.iter() {
                t.remove(x);
            }
        }
        Ok(Value::Set(t.persistent()))
    });

    reg(i, "union", ArityHint::Any, |_i, args| {
        // Parity fix: real `clojure.set/union` 1-arg is `([s1] s1)` (no
        // validation, nil-passthrough) and 2+-arg folds via `conj`/`into`,
        // which tolerate nil as an empty seq -- unlike this fn's old
        // unconditional `require_set`, which threw on any nil arg (seen live:
        // clojure-lsp completion -> `queries.clj` `set/union` with a nil
        // settings-derived arg -> textDocument/completion 500s as -32603).
        if args.len() == 1 {
            return Ok(args[0].clone());
        }
        let mut t = champ::PersistentHashSet::new().transient();
        for a in args {
            if matches!(a, Value::Nil) {
                continue;
            }
            for x in require_set(a, "union")?.iter() {
                t.insert(x.clone());
            }
        }
        Ok(Value::Set(t.persistent()))
    });

    reg(i, "intersection", ArityHint::Min(1), |_i, args| {
        let first = require_set(&args[0], "intersection")?;
        let mut rest = Vec::with_capacity(args.len() - 1);
        for a in &args[1..] {
            rest.push(require_set(a, "intersection")?);
        }
        let mut t = champ::PersistentHashSet::new().transient();
        for x in first.iter() {
            if rest.iter().all(|other| other.contains(x)) {
                t.insert(x.clone());
            }
        }
        Ok(Value::Set(t.persistent()))
    });

    for name in ["difference", "union", "intersection"] {
        alias(i, "clojure.set", name);
        alias(i, "set", name);
    }

    // M3: the rest of `clojure.set` -- `subset? superset? select project
    // rename rename-keys map-invert index join` (both `join` arities).
    // Every function below was checked against the ACTUAL 1.13.0-alpha6
    // `clojure/set.clj` source (not recalled from memory -- see the
    // per-function comments for the exact clauses that drove each Rust
    // shape), because several of these are surprisingly non-total on
    // non-set/non-map inputs in ways an idealized reimplementation would
    // miss entirely (`select`'s nil-passthrough, `rename-keys`'s nil
    // collapse, `map-invert`'s collision-winner-is-iteration-order-last).
    //
    // Ground truth (1.13.0-alpha6's clojure/set.clj):
    //   (defn subset? [set1 set2]
    //     (and (<= (count set1) (count set2)) (every? #(contains? set2 %) set1)))
    //   (defn superset? [set1 set2]
    //     (and (>= (count set1) (count set2)) (every? #(contains? set1 %) set2)))
    // Neither is set-only in real Clojure -- both go through the GENERIC
    // `count`/`contains?`/`every?`, not a set-specific path, so e.g.
    // `(subset? #{1} [1 2])` measures `true` (not a type error): `contains?`
    // on a VECTOR checks INDEX membership, not value membership, and `1`
    // happens to be a valid index into a 2-element vector. Measured:
    // `(subset? nil #{})` => `true` and `(subset? #{} nil)` => `true` (both
    // operands are total on `nil`: it counts as 0 and iterates as empty).
    // Reproduced by mirroring `count`/`contains?`'s own match arms (see
    // `coll_count`/`coll_contains_generic` below) instead of requiring
    // `Value::Set`.
    reg(i, "subset?", ArityHint::Exact(2), |interp, args| {
        if coll_count(interp, &args[0])? > coll_count(interp, &args[1])? {
            return Ok(Value::Bool(false));
        }
        for x in materialize(interp, &args[0])? {
            if !coll_contains_generic(&args[1], &x)? {
                return Ok(Value::Bool(false));
            }
        }
        Ok(Value::Bool(true))
    });

    reg(i, "superset?", ArityHint::Exact(2), |interp, args| {
        if coll_count(interp, &args[0])? < coll_count(interp, &args[1])? {
            return Ok(Value::Bool(false));
        }
        for x in materialize(interp, &args[1])? {
            if !coll_contains_generic(&args[0], &x)? {
                return Ok(Value::Bool(false));
            }
        }
        Ok(Value::Bool(true))
    });

    // Ground truth: `(reduce (fn [s k] (if (pred k) s (disj s k))) xset
    // xset)`. Both the reduce's SEED and its walked collection are `xset`
    // itself, so `nil` short-circuits: `reduce` over an empty seq just
    // returns its seed unchanged, with `disj` never reached. Measured:
    // `(select even? nil)` => `nil` (NOT `#{}`) -- there's no set-only
    // requirement to violate when nothing iterates. `(select even? #{})`
    // does iterate zero elements but still returns its `#{}` seed, so it
    // matches too. A genuinely non-set, non-nil `xset` (e.g. a vector) only
    // blows up once `disj` actually runs on it in real Clojure; mova's scope
    // is "pure functions over mova's existing Set/Map values" so that
    // partial-per-element nuance isn't reproduced -- non-nil non-`Set` is a
    // `type_err` up front here.
    reg(i, "select", ArityHint::Exact(2), |interp, args| {
        let pred = args[0].clone();
        match &args[1] {
            Value::Nil => Ok(Value::Nil),
            Value::Set(s) => {
                let mut out = s.clone();
                for x in s.iter() {
                    if !interp.call(&pred, std::slice::from_ref(x))?.truthy() {
                        out = out.remove(x);
                    }
                }
                Ok(Value::Set(out))
            }
            other => Err(RjError::type_err(format!("select: not a set: {}", other.type_name()))),
        }
    });

    // Ground truth: `(with-meta (set (map #(select-keys % ks) xrel)) (meta
    // xrel))`. mova has no rel/set metadata to preserve, so `with-meta` is
    // a no-op here. `xrel` is walked via `materialize` (nil-safe: `nil`
    // measures as zero rows, same as `map` treating `nil` as an empty seq
    // everywhere else in this file), and each row goes through the same
    // Map/HostStruct/Nil coercion `select-keys` itself uses (`coerce_map_
    // like`), so a key absent from a row just drops out of the projected
    // row instead of erroring. Measured: `(project #{{:a 1}} [:z])` =>
    // `#{{}}` (every row projects to the same empty map, deduped by `set`).
    reg(i, "project", ArityHint::Exact(2), |interp, args| {
        let ks = materialize(interp, &args[1])?;
        let rows = materialize(interp, &args[0])?;
        let mut t = champ::PersistentHashSet::new().transient();
        for row in &rows {
            let m = coerce_map_like(row, "project")?;
            t.insert(Value::Map(select_keys_slice(&m, &ks)));
        }
        Ok(Value::Set(t.persistent()))
    });

    // Ground truth: `(set (map #(rename-keys % kmap) xrel))` -- `rename` is
    // just `rename-keys` mapped over every row of a rel and re-set-ified
    // (deduping rows that collide after renaming, same as `project`).
    reg(i, "rename", ArityHint::Exact(2), |interp, args| {
        let kmap = coerce_map_like(&args[1], "rename")?;
        let rows = materialize(interp, &args[0])?;
        let mut t = champ::PersistentHashSet::new().transient();
        for row in &rows {
            let m = coerce_map_like(row, "rename")?;
            t.insert(Value::Map(rename_keys_map(&m, &kmap)));
        }
        Ok(Value::Set(t.persistent()))
    });

    // Ground truth: `(reduce (fn [m [old new]] (if (contains? map old)
    // (assoc m new (get map old)) m)) (apply dissoc map (keys kmap)) kmap)`.
    // The subtlety that makes this NOT a simple per-key rename: the
    // accumulator starts as `map` with EVERY `kmap` key dissoc'd UP FRONT
    // (not incrementally, one dissoc per fold step), and every fold step
    // tests `contains?`/`get` against the ORIGINAL `map`, never the
    // accumulator. That's why a two-old-keys-collide-on-one-new-key rename
    // is decided by `kmap`'s OWN iteration order, not by which pair a naive
    // per-key `assoc`/`dissoc` walk would visit first -- measured:
    // `(rename-keys {:a 1 :b 2} {:a :c :b :c})` => `{:c 2}` (`:b`'s pair
    // landed last in `kmap`'s iteration order and clobbered `:a`'s). Also
    // measured: `(rename-keys {:a 1 :b 2} {:a :b})` => `{:b 1}` (the
    // up-front dissoc removes `:b`'s ORIGINAL value 2 before `:a`'s renamed
    // value 1 lands on `:b`, so nothing of the original `:b` entry
    // survives) -- see `rename_keys_map` for the shared implementation
    // `rename` (the set-of-rows version) also uses.
    //
    // Special-cased here (not folded into `coerce_map_like`'s usual
    // "`nil` -> empty map" contract): `map`'s `dissoc`/`contains?`/`get` are
    // ALL total-on-nil in real Clojure, so `nil`'s dissoc is `nil`, its
    // `contains?` is always false (every fold step keeps the accumulator ==
    // the dissoc'd `nil`), and the accumulator never becomes a real map --
    // the whole call collapses to `nil` unchanged. Measured:
    // `(rename-keys nil {:a :x})` => `nil` (not `{}`).
    reg(i, "rename-keys", ArityHint::Exact(2), |_i, args| {
        if matches!(&args[0], Value::Nil) {
            return Ok(Value::Nil);
        }
        let m = coerce_map_like(&args[0], "rename-keys")?;
        let kmap = coerce_map_like(&args[1], "rename-keys")?;
        Ok(Value::Map(rename_keys_map(&m, &kmap)))
    });

    // Ground truth: `(persistent! (reduce-kv (fn [m k v] (assoc! m v k))
    // (transient {}) m))`. On a value collision, the LAST `(k, v)` pair
    // visited in `m`'s own iteration order wins (its `assoc!` runs last and
    // overwrites) -- reproduced by folding forward over `PMap::iter()` in
    // the same order every other iteration site in this file uses
    // (`merge`/`reduce-kv`), so the winner here is whichever entry mova's
    // own `PMap` iterates last, exactly mirroring the "last write wins"
    // RULE even though the concrete winner for a given multi-key map can
    // differ from real Clojure's own hash-iteration order (different HAMT
    // implementation, not a semantic gap). Sanity-checked no-collision case
    // (gate): `(map-invert {:a 1})` => `{1 :a}`.
    reg(i, "map-invert", ArityHint::Exact(1), |_i, args| {
        let m = coerce_map_like(&args[0], "map-invert")?;
        Ok(Value::Map(map_invert_map(&m)))
    });

    // Ground truth: `(reduce (fn [m x] (let [ik (select-keys x ks)] (assoc
    // m ik (conj (get m ik #{}) x)))) {} xrel)`. `nil` `xrel` iterates as
    // zero rows (same `materialize` nil-safety as `project`/`rename`
    // above), so `(index nil [:a])` => `{}` (reduce over an empty seq
    // returns its `{}` seed). A row missing one of `ks` still groups fine
    // -- `select-keys` just omits the missing key, so e.g. a row with no
    // `:a` at all groups under key `{}` alongside every other row that also
    // projects to `{}` (measured: `(index #{{:a 1 :b 2} {:b 3}} [:a])` =>
    // `{{:a 1} #{{:a 1, :b 2}}, {} #{{:b 3}}}`).
    reg(i, "index", ArityHint::Exact(2), |interp, args| {
        let ks = materialize(interp, &args[1])?;
        let rows = materialize(interp, &args[0])?;
        Ok(Value::Map(build_index(&rows, &ks)?))
    });

    // Ground truth (both arities):
    //   ([xrel yrel] ;natural join
    //    (if (and (seq xrel) (seq yrel))
    //      (let [ks (intersection (set (keys (first xrel))) (set (keys (first yrel))))
    //            [r s] (if (<= (count xrel) (count yrel)) [xrel yrel] [yrel xrel])
    //            idx (index r ks)]
    //        (reduce (fn [ret x]
    //                  (let [found (idx (select-keys x ks))]
    //                    (if found (reduce #(conj %1 (merge %2 x)) ret found) ret)))
    //                #{} s))
    //      #{}))
    //   ([xrel yrel km] ;arbitrary key mapping
    //    (let [[r s k] (if (<= (count xrel) (count yrel))
    //                    [xrel yrel (map-invert km)]
    //                    [yrel xrel km])
    //          idx (index r (vals k))]
    //      (reduce (fn [ret x]
    //                (let [found (idx (rename-keys (select-keys x (keys k)) k))]
    //                  (if found (reduce #(conj %1 (merge %2 x)) ret found) ret)))
    //              #{} s)))
    // Two measured subtleties worth calling out (both reproduced exactly
    // below, not idealized away):
    // - `ks`/`k` are derived from `xrel`/`yrel` in CALLER order (`(keys
    //   (first xrel))`); only the `r`/`s` roles (which side gets indexed vs.
    //   walked) swap on a `count` comparison, favoring `xrel` on a tie.
    // - When there's no real key correspondence at all (arity-2: no common
    //   key names; arity-3: `km` names keys that don't exist on either side
    //   the way it implies), EVERY row's `select-keys` projects to `{}`, so
    //   the whole thing degrades to a full cartesian product rather than an
    //   empty result -- measured: `(join #{{:b 1}} #{{:c 2}})` =>
    //   `#{{:b 1, :c 2}}` (one `{} -> #{{:b 1}}` index bucket, matched by
    //   every row on the other side).
    reg(i, "join", ArityHint::Range(2, 3), |interp, args| {
        let xrows = materialize(interp, &args[0])?;
        let yrows = materialize(interp, &args[1])?;
        if xrows.is_empty() || yrows.is_empty() {
            return Ok(Value::Set(champ::PersistentHashSet::new()));
        }
        let mut ret = champ::PersistentHashSet::new().transient();
        if args.len() == 2 {
            let x0 = coerce_map_like(&xrows[0], "join")?;
            let y0 = coerce_map_like(&yrows[0], "join")?;
            let ks: Vec<Value> = pmap_keys(&x0).into_iter().filter(|k| y0.contains_key(k)).collect();
            let (r, s) = if xrows.len() <= yrows.len() { (&xrows, &yrows) } else { (&yrows, &xrows) };
            let idx = build_index(r, &ks)?;
            for x in s {
                let xm = coerce_map_like(x, "join")?;
                let probe = Value::Map(select_keys_slice(&xm, &ks));
                if let Some(Value::Set(found)) = idx.get(&probe) {
                    for el in found.iter() {
                        ret.insert(merge_two(el, x)?);
                    }
                }
            }
        } else {
            let km = coerce_map_like(&args[2], "join")?;
            let (r, s, k) = if xrows.len() <= yrows.len() {
                (&xrows, &yrows, map_invert_map(&km))
            } else {
                (&yrows, &xrows, km)
            };
            let idx = build_index(r, &pmap_vals(&k))?;
            let k_keys = pmap_keys(&k);
            for x in s {
                let xm = coerce_map_like(x, "join")?;
                let sub = select_keys_slice(&xm, &k_keys);
                let probe = Value::Map(rename_keys_map(&sub, &k));
                if let Some(Value::Set(found)) = idx.get(&probe) {
                    for el in found.iter() {
                        ret.insert(merge_two(el, x)?);
                    }
                }
            }
        }
        Ok(Value::Set(ret.persistent()))
    });

    for name in [
        "subset?",
        "superset?",
        "select",
        "project",
        "rename",
        "rename-keys",
        "map-invert",
        "index",
        "join",
    ] {
        alias(i, "clojure.set", name);
        alias(i, "set", name);
    }
}

fn require_set(v: &Value, op: &str) -> Result<champ::PersistentHashSet<Value>, RjError> {
    match v {
        // S5/M3: READ (`disj` re-attaches at its own call site).
        Value::Meta(m) => require_set(&m.inner, op),
        Value::Set(s) => Ok(s.clone()),
        // W3f (small-tail sweep): `clojure_set.clj`'s `test-union`/
        // `test-intersection` pass a `sorted-set`/`sorted-set-by` alongside
        // plain `hash-set`s in the same call (measured: real
        // `clojure.set/union`/`intersection` are generic over any
        // `IPersistentSet`, sorted or not -- only `contains?`/iteration are
        // used, never anything comparator-specific). Widened to a plain
        // hash set here; the caller always wraps the result back in
        // `Value::Set` regardless, which is fine because `=` between a
        // `Set` and a `SortedSet` is already content-equal (see
        // `value.rs`'s `(SortedSet(a), Set(b))` arm) -- no in-scope caller
        // ever asserts the RESULT stays sorted.
        Value::SortedSet(s) => Ok(s.entries.iter().cloned().collect()),
        other => Err(RjError::type_err(format!("{op}: not a set: {}", other.type_name()))),
    }
}

/// `count`'s exact match arms, for `subset?`/`superset?` (which are
/// generic over ANY counted collection in real Clojure, not set-only --
/// see the ground-truth comment on `subset?`'s registration). C10: now a
/// thin alias for `count_value` -- the two were a hand-duplicated pair
/// (see this fn's git history) until `count_value` was extracted as a
/// standalone fn for `eval::types_forms::eval_dot_form`'s `.size` arm,
/// which removed the "nothing exposes it as a plain Rust fn" reason the
/// duplication existed for in the first place.
fn coll_count(interp: &mut Interp, v: &Value) -> Result<i64, RjError> {
    count_value(interp, v)
}

/// `contains?`'s exact match arms (see `coll_count`'s doc for why this is a
/// deliberate duplicate rather than a shared entry point). Note the
/// `Vector` arm: it's INDEX membership, not value membership -- exactly
/// `contains?`'s own real-Clojure contract, which is why e.g. `(subset?
/// #{1} [1 2])` measures `true` (index `1` is valid in a 2-element vector,
/// coincidentally the same answer value membership would have given here).
fn coll_contains_generic(coll: &Value, k: &Value) -> Result<bool, RjError> {
    Ok(match coll {
        // S5/M3: READ -- `(contains? (with-meta {:x 1} {:a 1}) :x)` is
        // `true`, measured.
        Value::Meta(m) => return coll_contains_generic(&m.inner, k),
        Value::Nil => false,
        Value::Map(m) => m.contains_key(k),
        Value::HostStruct(hs) => match k {
            Value::Keyword(kw) => crate::host_struct::shape_field_index(&hs.shape, kw.text_ref()).is_some(),
            _ => crate::host_struct::as_pmap(hs).contains_key(k),
        },
        Value::LazyMap(hs) => match k {
            Value::Keyword(kw) => crate::lazy_map::contains_key(hs, kw.text_ref()),
            _ => false,
        },
        Value::Set(s) => s.contains(k),
        Value::Vector(items) | Value::MapEntry(items) => {
            matches!(k, Value::Int(n) if *n >= 0 && (*n as usize) < items.len())
        }
        Value::Array(arr) => matches!(k, Value::Int(n) if *n >= 0 && (*n as usize) < crate::sync::lock_mutex(&arr.data).len()),
        other => return Err(RjError::type_err(format!("contains?: not associative: {}", other.type_name()))),
    })
}

/// `select-keys`'s own Map/HostStruct/Nil coercion (see the `select-keys`
/// builtin above), factored out so every `clojure.set` function that walks
/// a "row" (a rel element) shares the exact same non-map-input contract:
/// `nil` coerces to an empty map (never errors), a `HostStruct` flattens
/// via `as_pmap`, anything else is a `type_err`.
fn coerce_map_like(v: &Value, op: &str) -> Result<PMap, RjError> {
    match v {
        // S5/M3: READ -- callers that need to PRESERVE metadata re-attach
        // it themselves from the original receiver (`merge`/`merge-with`/
        // `select-keys`), because only they know which argument the
        // result's metadata should come from.
        Value::Meta(m) => coerce_map_like(&m.inner, op),
        Value::Map(m) => Ok(m.clone()),
        Value::HostStruct(hs) => Ok(crate::host_struct::as_pmap(hs).clone()),
        Value::LazyMap(hs) => Ok(crate::lazy_map::as_pmap(hs).clone()),
        // C14 (protocols): `defrecord-acts-like-a-map`'s own
        // `set/rename-keys` call -- measured, `clojure.set/rename-keys`
        // on a record returns a PLAIN `PersistentArrayMap` (its output is
        // always built fresh via `assoc`/`dissoc` on an EMPTY accumulator
        // in real `clojure.set`, never on the record itself), so reading
        // a record's `data` here and letting every `clojure.set` fn's own
        // `Value::Map(..)` wrapping handle the result is exactly right.
        Value::Inst(inst) if inst.tdef.is_record => Ok(inst.data.clone()),
        Value::Nil => Ok(PMap::new()),
        other => Err(RjError::type_err(format!("{op}: not a map: {}", other.type_name()))),
    }
}

/// `(select-keys m ks)` over an already-materialized `ks` slice (every
/// `clojure.set` caller already has `ks` as a `Vec<Value>` from
/// `materialize`, so this skips re-walking it per row).
fn select_keys_slice(m: &PMap, ks: &[Value]) -> PMap {
    let mut out = PMap::new();
    for k in ks {
        if let Some(v) = m.get(k) {
            out.insert(k.clone(), v.clone());
        }
    }
    out
}

/// `PMap`'s keys/vals as owned `Vec<Value>`, in the map's own iteration
/// order (matching `(keys m)`/`(vals m)`'s own convention elsewhere in this
/// file).
fn pmap_keys(m: &PMap) -> Vec<Value> {
    m.iter().map(|(k, _)| k.clone()).collect()
}
fn pmap_vals(m: &PMap) -> Vec<Value> {
    m.iter().map(|(_, v)| v.clone()).collect()
}

/// `clojure.set/rename-keys`'s core transform, shared by the `rename-keys`
/// builtin (single map) and `rename` (mapped over every row of a rel) --
/// see the ground-truth comment on the `rename-keys` registration for why
/// this is NOT a simple per-key walk: `kmap`'s keys are all dissoc'd from
/// `m` UP FRONT, and every fold step tests `contains?`/`get` against the
/// ORIGINAL `m`, not the accumulator being built.
fn rename_keys_map(m: &PMap, kmap: &PMap) -> PMap {
    let mut out = m.clone();
    for (old, _) in kmap.iter() {
        out.remove(old);
    }
    for (old, new) in kmap.iter() {
        if let Some(v) = m.get(old) {
            out.insert(new.clone(), v.clone());
        }
    }
    out
}

/// `clojure.set/map-invert`'s core transform: fold `m` forward, later
/// entries winning ties on a colliding value (see the ground-truth comment
/// on the `map-invert` registration).
fn map_invert_map(m: &PMap) -> PMap {
    let mut out = PMap::new();
    for (k, v) in m.iter() {
        out.insert(v.clone(), k.clone());
    }
    out
}

/// `clojure.set/index`'s core transform, shared by the `index` builtin and
/// `join` (both arities build an index over the smaller relation before
/// probing it from the larger one). Mirrors `(reduce (fn [m x] (assoc m ik
/// (conj (get m ik #{}) x))) {} xrel)` exactly, including the "a row
/// missing one of `ks` groups under key `{}`" behavior `select_keys_slice`
/// already gives for free.
fn build_index(rows: &[Value], ks: &[Value]) -> Result<PMap, RjError> {
    let mut m = PMap::new();
    for row in rows {
        let rm = coerce_map_like(row, "index")?;
        let ik = Value::Map(select_keys_slice(&rm, ks));
        let group = match m.get(&ik) {
            Some(Value::Set(s)) => s.insert(row.clone()),
            _ => champ::PersistentHashSet::new().insert(row.clone()),
        };
        m.insert(ik, Value::Set(group));
    }
    Ok(m)
}

/// `(merge %2 x)` from `join`'s reducer: `b`'s keys win over `a`'s on
/// collision, matching `merge`'s own left-to-right-overlay contract
/// (the `merge` builtin above).
fn merge_two(a: &Value, b: &Value) -> Result<Value, RjError> {
    let am = coerce_map_like(a, "join")?;
    let bm = coerce_map_like(b, "join")?;
    let mut out = am;
    for (k, v) in bm.iter() {
        out.insert(k.clone(), v.clone());
    }
    Ok(Value::Map(out))
}

/// `get-in`/`update-in`'s path argument: real Clojure's is `(reduce get m
/// ks)`-shaped -- `ks` can be ANY seqable, not just a vector (measured:
/// `(get-in {:a {:c 4}} (list :a :c))` and `(get-in m (seq [:a :b]))` both
/// work on the oracle). `Vector`/`MapEntry` stay a cheap `PVec` clone (the
/// overwhelmingly common case, no allocation); anything else materializes
/// through the ordinary `uncons` walk, same as any other seqable-argument
/// builtin -- a non-seqable path (a keyword, a number, ...) surfaces
/// `materialize`'s own "don't know how to create a seq from ..." error,
/// which is an honest failure shape for a caller that passed something
/// that was never path-like at all.
fn get_in_path(interp: &mut Interp, path: &Value) -> Result<PVec, RjError> {
    match path {
        Value::Vector(items) | Value::MapEntry(items) => Ok(items.clone()),
        other => Ok(materialize(interp, other)?.into_iter().collect()),
    }
}

fn get_in(interp: &mut Interp, coll: &Value, path: &PVec) -> Result<Option<Value>, RjError> {
    let mut cur = coll.clone();
    for k in path.iter() {
        cur = match &cur {
            Value::Map(m) => {
                map_probe::record("get-in", m.len());
                match m.get(k) {
                    Some(v) => v.clone(),
                    None => return Ok(None),
                }
            }
            Value::HostStruct(hs) => match k {
                Value::Keyword(kw) => match crate::host_struct::lookup(hs, kw.text_ref()) {
                    Some(v) => v,
                    None => return Ok(None),
                },
                _ => match crate::host_struct::as_pmap(hs).get(k) {
                    Some(v) => v.clone(),
                    None => return Ok(None),
                },
            },
            Value::LazyMap(hs) => match k {
                Value::Keyword(kw) => match crate::lazy_map::lookup(hs, kw.text_ref()) {
                    Some(v) => v,
                    None => return Ok(None),
                },
                _ => match crate::lazy_map::as_pmap(hs).get(k) {
                    Some(v) => v.clone(),
                    None => return Ok(None),
                },
            },
            Value::Vector(items) | Value::MapEntry(items) => match k {
                Value::Int(n) if *n >= 0 => match items.get(*n as usize) {
                    Some(v) => v.clone(),
                    None => return Ok(None),
                },
                _ => return Ok(None),
            },
            // S4
            Value::SortedMap(m) => match crate::builtins::sorted::sorted_map_get(interp, m, k)? {
                Some(v) => v,
                None => return Ok(None),
            },
            Value::TypedVec(tv) => match k {
                Value::Int(n) if *n >= 0 => match tv.data.get(*n as usize) {
                    Some(v) => v.clone(),
                    None => return Ok(None),
                },
                _ => return Ok(None),
            },
            _ => return Ok(None),
        };
    }
    Ok(Some(cur))
}

fn assoc_in(interp: &mut Interp, coll: &Value, path: &PVec, v: Value) -> Result<Value, RjError> {
    match path.len() {
        0 => Ok(v),
        1 => assoc_one(interp, coll, &path[0], &v),
        _ => {
            let k = &path[0];
            let rest = path.skip(1);
            let nested = match coll {
                Value::Map(m) => m.get(k).cloned().unwrap_or(Value::Nil),
                Value::HostStruct(hs) => match k {
                    Value::Keyword(kw) => crate::host_struct::lookup(hs, kw.text_ref()).unwrap_or(Value::Nil),
                    _ => crate::host_struct::as_pmap(hs).get(k).cloned().unwrap_or(Value::Nil),
                },
                Value::LazyMap(hs) => match k {
                    Value::Keyword(kw) => crate::lazy_map::lookup(hs, kw.text_ref()).unwrap_or(Value::Nil),
                    _ => crate::lazy_map::as_pmap(hs).get(k).cloned().unwrap_or(Value::Nil),
                },
                Value::Vector(items) | Value::MapEntry(items) => match k {
                    Value::Int(n) if *n >= 0 => items.get_owned(*n as usize).unwrap_or(Value::Nil),
                    other => return Err(RjError::type_err(format!("assoc-in: bad index {}", other.type_name()))),
                },
                // S4
                Value::SortedMap(m) => crate::builtins::sorted::sorted_map_get(interp, m, k)?.unwrap_or(Value::Nil),
                Value::TypedVec(tv) => match k {
                    Value::Int(n) if *n >= 0 => tv.data.get_owned(*n as usize).unwrap_or(Value::Nil),
                    other => return Err(RjError::type_err(format!("assoc-in: bad index {}", other.type_name()))),
                },
                Value::Nil => Value::Nil,
                // kondo-wave: same record gap as `update` above -- a
                // defrecord is associative on the JVM; assoc-in's nested
                // lookup lacked the record arm.
                Value::Inst(inst) if inst.tdef.is_record => inst.data.get(k).cloned().unwrap_or(Value::Nil),
                other => return Err(RjError::type_err(format!("assoc-in: not associative: {}", other.type_name()))),
            };
            let updated = assoc_in(interp, &nested, &rest, v)?;
            assoc_one(interp, coll, k, &updated)
        }
    }
}

fn lm_empty(lm: &crate::lazy_map::LazyMapInner) -> bool {
    crate::lazy_map::count(lm) == 0
}
