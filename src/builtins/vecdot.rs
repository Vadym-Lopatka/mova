//! C7 (compat/c7-vectors-veneer): `(.method v arg...)`-shaped instance
//! methods on vectors (`Value::Vector`/`Value::TypedVec`), on the SEQ of a
//! vector (`(seq v)`, an ordinary `Value::List` -- `.chunkedNext`/`.empty`/
//! `.cons`/`.equiv` all get called on one in `vectors.clj`), and on the two
//! vector-derived `Value::VecSeq` shapes (`.rseq`'s `RSeq`, `.chunkedNext`'s
//! `Chunked` -- see `value::VecSeqKind`'s doc). Mirrors
//! `builtins::strings::str_dot_method`'s shape: a small, MEASURED method
//! table (every row's exact return/exception shape was checked against the
//! real JVM first -- see `compat/vecveneer-probe.clj` +
//! `compat/vecveneer-oracle-transcript.txt`), `None` for anything not in
//! that table (falls through to `eval_dot_form`'s ordinary "unresolved
//! symbol" error, same as an unimplemented string method). Unlike
//! `str_dot_method`, several rows here need `&mut Interp` (`.equiv`/`.cons`
//! delegate to the SAME `values_equal`/`cons_builtin` the plain `=`/`cons`
//! builtins use, never a second implementation), so this fn takes `interp`
//! up front.
//!
//! BINDING DESIGN DIRECTIVE (owner, this task's brief): interop targets
//! Rust-native values; these dot-methods are a thin compatibility veneer,
//! never JVM emulation deeper than `vectors.clj` measurably demands. Two
//! consequences visible in the table below: (1) `clojure.lang.MapEntry.`
//! (the constructor `test-vec-associative`'s `entryAt` sub-test compares
//! against) is NOT implemented here -- a real `MapEntry` value type is a
//! separate, owner-approved, not-yet-landed campaign with its own gates
//! (see the session memory), so those 3 `are` rows stay honestly failing
//! ("unresolved symbol" on the ctor call) while `.entryAt` itself returns
//! the right SHAPE (a plain `[idx val]` 2-vector, which IS `=` to a real
//! `MapEntry` on the JVM) for the other 5 nil-checking rows and anything
//! else that reads it structurally; (2) spliterators/streams/`reify`
//! (`test-empty-vector-spliterator` and its three siblings) are simply not
//! in this table at all -- owner-gated, out of scope, they stay failing on
//! "Unable to resolve symbol: .spliterator"/"reify" exactly as before.

use std::sync::Arc;

use crate::builtins::collections::cons_builtin;
use crate::error::{JvmClass, RjError};
use crate::eval::Interp;
use crate::reader::Span;
use crate::value::{PVec, TypedVecVal, Value, VecSeqKind, VecSeqVal};

fn bad_arity(op: &str, got: usize) -> RjError {
    RjError::arity(format!("{op}: wrong number of args ({got})"))
}

/// `Vector`/`TypedVec`'s shared element view -- mirrors
/// `builtins::sorted::vec_items` (private to that module, so restated here
/// rather than widening its visibility for one more caller).
fn vec_items(v: &Value) -> Option<&PVec> {
    match v {
        Value::Vector(items) => Some(items),
        Value::TypedVec(tv) => Some(&tv.data),
        _ => None,
    }
}

/// Rebuilds a `Vector`/`TypedVec` receiver's "empty version of itself"
/// (measured: `IPersistentCollection.empty()` on a real vector returns an
/// empty instance of the SAME concrete class -- `clojure.core.Vec` stays
/// `clojure.core.Vec`, unlike a SEQ's `.empty()`, which always collapses to
/// the generic `clojure.lang.PersistentList$EmptyList` marker regardless of
/// concrete seq subtype; see the `"empty"` arm below for that half).
fn empty_like(v: &Value) -> Value {
    match v {
        Value::TypedVec(tv) => Value::TypedVec(Arc::new(TypedVecVal { kind: tv.kind, data: PVec::new() })),
        _ => Value::Vector(PVec::new()),
    }
}

/// C7: `.rseq`/`.index`/`.first`/`.next`(dot)/`.count`(dot)/`.empty`/
/// `.cons`/`.equiv`/`.chunkedNext`/`.compareTo`/`.containsKey`/`.entryAt`/
/// `.length`/`.equals` -- see this module's doc for the overall shape and
/// scope. `target` is already-evaluated (same convention as
/// `eval_dot_form`'s `HostInst`/`Str` arms); `args` are the already-
/// evaluated call arguments (method name excluded).
pub(crate) fn vec_dot_method(
    interp: &mut Interp,
    field: &str,
    target: &Value,
    args: &[Value],
    span: Span,
) -> Option<Result<Value, RjError>> {
    match field {
        // ---------- Vector/TypedVec only ----------
        "rseq" => {
            let items = vec_items(target)?;
            if !args.is_empty() {
                return Some(Err(bad_arity("rseq", args.len())));
            }
            if items.is_empty() {
                return Some(Ok(Value::Nil));
            }
            // M10.1: `PVecIter` no longer implements `DoubleEndedIterator`
            // (`champ::PVecIter` is a forward-only leaf-chunk walk)
            // -- reverse via a `Vec` round-trip instead of `.iter().rev()`.
            let mut v: Vec<Value> = items.iter_cloned().collect();
            v.reverse();
            let reversed: PVec = v.into_iter().collect();
            Some(Ok(Value::VecSeq(Arc::new(VecSeqVal { kind: VecSeqKind::RSeq, items: reversed }))))
        }
        // Measured (`compat/vecveneer-oracle-transcript.txt`): `nil`
        // throws (real JVM: NPE, from `Numbers.compare`/`Util.compare`
        // dereferencing a null `Counted.count()`); any non-vector
        // Comparable (list/map/set/int/...) throws too (real JVM: CCE,
        // `IPersistentVector` cast failure); another vector (plain or
        // typed) compares via the SAME element-then-length rule `compare`
        // itself uses (`builtins::sorted::natural_compare`) -- measured
        // identical results on every `compareTo`-vs-`compare` pair tried.
        // Exact message TEXT is out of conformance scope (same rule
        // `reflect::cast_native`'s doc cites). The CLASS is not: this
        // sentence used to end "the vendored suite's own `thrown?` checks
        // are class-blind, so only 'did it throw at all' is ever observed
        // here" -- true when it was written, false since C3g made
        // `mova-test-shim.mova`'s `is` match the written class for real.
        // See the `Value::Nil` arm below.
        "compareTo" => {
            if vec_items(target).is_none() {
                return None;
            }
            if args.len() != 1 {
                return Some(Err(bad_arity("compareTo", args.len())));
            }
            match &args[0] {
                // W3a: measured -- `(.compareTo (vector-of :int 1 2) nil)`
                // => `java.lang.NullPointerException: Cannot invoke
                // "clojure.lang.Counted.count()" because "v" is null`. The
                // message already SAID NullPointerException (it was the
                // only channel available pre-W3a); now the class carries it.
                Value::Nil => Some(Err(RjError::other(
                    "compareTo: NullPointerException (cannot compare against nil)",
                )
                .with_class(JvmClass::NullPointer)
                .with_span(span))),
                other if vec_items(other).is_some() => {
                    match crate::builtins::sorted::natural_compare(target, other) {
                        Ok(std::cmp::Ordering::Less) => Some(Ok(Value::Int(-1))),
                        Ok(std::cmp::Ordering::Equal) => Some(Ok(Value::Int(0))),
                        Ok(std::cmp::Ordering::Greater) => Some(Ok(Value::Int(1))),
                        Err(e) => Some(Err(e.with_span(span))),
                    }
                }
                other => Some(Err(RjError::type_err(format!(
                    "compareTo: ClassCastException (cannot cast {} to a vector)",
                    other.type_name()
                ))
                .with_span(span))),
            }
        }
        // Measured: `true` iff `x` is an `Int` in `[0, count)` -- every
        // other shape (negative, out of range, `nil`, a non-Int key at
        // all) is `false`, never an exception (real `Associative.
        // containsKey` on a vector is total: it never throws for a bad
        // key, only `nth`/`valAt` do that).
        "containsKey" => {
            let items = vec_items(target)?;
            if args.len() != 1 {
                return Some(Err(bad_arity("containsKey", args.len())));
            }
            let ok = matches!(&args[0], Value::Int(n) if *n >= 0 && (*n as usize) < items.len());
            Some(Ok(Value::Bool(ok)))
        }
        // Measured: an in-range `Int` key returns a `[idx val]` 2-vector
        // (real class `clojure.lang.MapEntry`, but PRINTS/`=`s exactly
        // like a plain 2-vector, and there is no dedicated `MapEntry`
        // value type here yet -- see this module's doc); anything else is
        // `nil`, never an exception (matches `containsKey`'s totality).
        "entryAt" => {
            let items = vec_items(target)?;
            if args.len() != 1 {
                return Some(Err(bad_arity("entryAt", args.len())));
            }
            let entry = match &args[0] {
                Value::Int(n) if *n >= 0 && (*n as usize) < items.len() => {
                    let v = items.get(*n as usize).expect("checked in bounds").clone();
                    Value::Vector(crate::pvec![args[0].clone(), v])
                }
                _ => Value::Nil,
            };
            Some(Ok(entry))
        }
        // D1: `.spliterator`/`.stream`/`.parallelStream` -- the three
        // entry points into `crate::hostclass`'s Rust-native
        // spliterator/stream cursors (see `HostKind::Spliterator`/
        // `HostKind::Stream` for the exact, deliberately tiny method
        // scope on the other side, and this module's own doc, which
        // named these as out-of-scope until D1 unlocked them).
        //
        // Elements are SNAPSHOTTED here (a `PVec` clone is O(1) on a
        // persistent vector, and the receiver can't mutate anyway) --
        // real `java.util.List.spliterator()` on an immutable list has
        // the same late-binding-irrelevant property. `parallelStream`
        // deliberately shares `stream`'s arm: parallelism is a hint on
        // the real JVM, and `test-vector-parallel-stream` asserts the two
        // agree, never that they differ.
        "spliterator" | "stream" | "parallelStream" => {
            let items = vec_items(target)?;
            if !args.is_empty() {
                return Some(Err(bad_arity(field, args.len())));
            }
            Some(Ok(if field == "spliterator" {
                crate::hostclass::mk_spliterator(items.clone())
            } else {
                crate::hostclass::mk_stream(items.clone())
            }))
        }
        // Measured: `.length` (real `clojure.core.Vec.length()`/
        // `Counted.count()`) is plain element count, same value `count`
        // returns.
        "length" => {
            let items = vec_items(target)?;
            if !args.is_empty() {
                return Some(Err(bad_arity("length", args.len())));
            }
            Some(Ok(Value::Int(items.len() as i64)))
        }
        // ---------- Vector/TypedVec/List/VecSeq (anything sequence-`=`-able) ----------
        // Measured: `.equals` on a vector is `clojure.core/=`'s
        // sequence-equality rule (a `TypedVec`/plain `Vector`/`List` with
        // the same elements are all `.equals`, `nil` never is) with ONE
        // tightening -- `Util.equals`, not `Util.equiv`, compares the
        // ELEMENTS, so a numeric element is class-strict: `(.equals [3]
        // [3])` is true but `(.equals (seq [3]) (seq [3N]))` is FALSE
        // even though `(= (seq [3]) (seq [3N]))` is true. C3e: delegates
        // to `values_equal_strict`, which is `values_equal` plus exactly
        // that leaf rule (see its doc), so this still cannot drift from
        // `=` anywhere the two agree.
        "equals" if vec_items(target).is_some() || matches!(target, Value::List(_) | Value::VecSeq(_)) => {
            if args.len() != 1 {
                return Some(Err(bad_arity("equals", args.len())));
            }
            Some(interp.values_equal_strict(target, &args[0]).map(Value::Bool))
        }
        // `IPersistentCollection.equiv` -- measured identical to `=` on
        // every pair `test-vecseq` exercises (including a plain `range`
        // on one side), so same delegation as `.equals` above.
        "equiv" if vec_items(target).is_some() || matches!(target, Value::List(_) | Value::VecSeq(_)) => {
            if args.len() != 1 {
                return Some(Err(bad_arity("equiv", args.len())));
            }
            Some(interp.values_equal(target, &args[0]).map(Value::Bool))
        }
        // clojure-lsp campaign (mova/PLAN.md): `java.util.List.indexOf`
        // -- the first `=`-matching element's index, `-1` when absent
        // (real JVM contract; `clojure_lsp.feature.completion` relies on
        // the `-1` exactly, via `(inc (or (.indexOf v x) 0))`). `Vector`/
        // `TypedVec` only (`vec_items`) -- `List`/`VecSeq` never spelled
        // `.indexOf` in any measured caller, so left out rather than
        // guessed at.
        "indexOf" => {
            let items = vec_items(target)?;
            if args.len() != 1 {
                return Some(Err(bad_arity("indexOf", args.len())));
            }
            let mut found: i64 = -1;
            for (idx, item) in items.iter().enumerate() {
                match interp.values_equal(item, &args[0]) {
                    Ok(true) => {
                        found = idx as i64;
                        break;
                    }
                    Ok(false) => {}
                    Err(e) => return Some(Err(e)),
                }
            }
            Some(Ok(Value::Int(found)))
        }
        // C10: `IReduce.reduce(f)`/`IReduce.reduce(f, init)` -- measured,
        // `data_structures.clj`'s `ireduce-reduced` calls the 1-arg form
        // on a `List` (`(.reduce ^clojure.lang.IReduce (list 1 2 3 4 5)
        // f)`) and on the seq of a `long-array` (also a `List` once
        // `seq`'d). Delegates to the SAME `reduce_coll` the `reduce`
        // builtin itself uses (honors `reduced` short-circuit) rather
        // than a second implementation -- see that fn's own doc.
        // `vec_items`-having targets (`Vector`/`TypedVec`) get the same
        // dot-method for free, even though the vendored suite only
        // exercises `List`.
        "reduce" if vec_items(target).is_some() || matches!(target, Value::List(_) | Value::VecSeq(_)) => {
            match args {
                [f] => Some(crate::builtins::seq::reduce_coll(interp, f, None, target)),
                [f, init] => Some(crate::builtins::seq::reduce_coll(interp, f, Some(init.clone()), target)),
                _ => Some(Err(bad_arity("reduce", args.len()))),
            }
        }
        // `IPersistentCollection.empty()` -- measured: a genuine VECTOR
        // (`Vector`/`TypedVec`) returns an empty instance of its OWN
        // concrete class (`empty_like`, above); a SEQ (`List`/`VecSeq`)
        // always collapses to the one shared `PersistentList$EmptyList`
        // marker, regardless of concrete seq subtype (measured on `vs`/
        // `vs-1`/`vs-32` alike, three different concrete seq shapes, same
        // result AND `identical?` to `clojure.lang.PersistentList/EMPTY`
        // -- `Value::empty_list_singleton()`'s doc explains why a fresh
        // `Value::List(PVec::new())` would NOT be `identical?` to that
        // static binding, and must not be used here instead).
        "empty" if vec_items(target).is_some() => {
            if !args.is_empty() {
                return Some(Err(bad_arity("empty", args.len())));
            }
            Some(Ok(empty_like(target)))
        }
        "empty" if matches!(target, Value::List(_) | Value::VecSeq(_)) => {
            if !args.is_empty() {
                return Some(Err(bad_arity("empty", args.len())));
            }
            Some(Ok(crate::value::empty_list_singleton()))
        }
        // `IPersistentCollection.cons` -- `(.cons coll x)` prepends `x`,
        // receiver-then-element (opposite argument order from the `cons`
        // FUNCTION, `(cons x coll)`) -- delegates to the exact same
        // `cons_builtin` the `cons` builtin calls, just with the two
        // arguments swapped back.
        "cons" if vec_items(target).is_some() || matches!(target, Value::List(_) | Value::VecSeq(_)) => {
            if args.len() != 1 {
                return Some(Err(bad_arity("cons", args.len())));
            }
            Some(cons_builtin(interp, args[0].clone(), target.clone()))
        }
        // ---------- the SEQ of a vector (`(seq v)`, a plain `List`) ----------
        // `.chunkedNext` -- measured: skips the FIRST 32-element chunk
        // (real `PersistentVector`'s chunk size) and returns a `Chunked`
        // `VecSeq` over everything past it, or `Nil` if there is no next
        // chunk (32 or fewer elements total -- not exercised by
        // `vectors.clj`, which only calls this on a 100-element seq, but
        // handled the same "no more" -> `Nil` way as `VecSeq`'s own
        // `.next` for consistency).
        // W4C-NS: a `Chunked`-kind `VecSeq` (now what `(seq (vector-of
        // ...))` itself returns -- see `builtins::collections::seq_of`'s
        // own doc) needs the SAME `.chunkedNext` behavior a plain `List`
        // seq gets here -- same "skip 32, `Nil` once nothing's left"
        // logic, just reading `vs.items` instead of a `List`'s items.
        "chunkedNext" if matches!(target, Value::List(_) | Value::VecSeq(_)) => {
            let items: &PVec = match target {
                Value::List(items) => items,
                Value::VecSeq(vs) => &vs.items,
                _ => unreachable!("checked above"),
            };
            if !args.is_empty() {
                return Some(Err(bad_arity("chunkedNext", args.len())));
            }
            const CHUNK: usize = 32;
            if items.len() <= CHUNK {
                return Some(Ok(Value::Nil));
            }
            let rest: PVec = items.iter().skip(CHUNK).cloned().collect();
            Some(Ok(Value::VecSeq(Arc::new(VecSeqVal { kind: VecSeqKind::Chunked, items: rest }))))
        }
        // ---------- VecSeq only (`.rseq`/`.chunkedNext`'s own results) ----------
        "index" => {
            let Value::VecSeq(vs) = target else { return None };
            if !args.is_empty() {
                return Some(Err(bad_arity("index", args.len())));
            }
            Some(Ok(Value::Int(vs.items.len() as i64 - 1)))
        }
        "first" if matches!(target, Value::VecSeq(_)) => {
            let Value::VecSeq(vs) = target else { unreachable!("checked above") };
            if !args.is_empty() {
                return Some(Err(bad_arity("first", args.len())));
            }
            Some(Ok(vs.items.front().cloned().unwrap_or(Value::Nil)))
        }
        // `.next` (the DOT method, distinct from the `next` FUNCTION):
        // measured `(class (.next (.rseq v)))` stays the SAME `RSeq`
        // class as long as elements remain -- matches `uncons`'s own
        // `VecSeq` tail rule (`builtins::collections::uncons`'s doc), so
        // this just IS that same peel, restated as a 0-arg dot-method.
        "next" if matches!(target, Value::VecSeq(_)) => {
            if !args.is_empty() {
                return Some(Err(bad_arity("next", args.len())));
            }
            Some(crate::builtins::uncons(interp, target).map(|r| r.map(|(_, t)| t).unwrap_or(Value::Nil)))
        }
        // `.count` -- `VecSeq` answers in O(1) from `items.len()`; a plain
        // `List` (measured on `(seq vs)`/`(seq vs-1)`, `vs`/`vs-1` from
        // `test-vecseq`) walks via `uncons` like the `count` BUILTIN's own
        // fallback (`builtins::collections`'s `count`/`coll_count`) --
        // needed because a `List` MAY carry the internal lazy-tail marker
        // (see `uncons`'s doc), so `.len()` alone could underreport.
        "count" if matches!(target, Value::VecSeq(_)) => {
            let Value::VecSeq(vs) = target else { unreachable!("checked above") };
            if !args.is_empty() {
                return Some(Err(bad_arity("count", args.len())));
            }
            Some(Ok(Value::Int(vs.items.len() as i64)))
        }
        "count" if matches!(target, Value::List(_)) => {
            if !args.is_empty() {
                return Some(Err(bad_arity("count", args.len())));
            }
            let mut n = 0i64;
            let mut cur = target.clone();
            loop {
                match crate::builtins::uncons(interp, &cur) {
                    Ok(Some((_, t))) => {
                        n += 1;
                        cur = t;
                    }
                    Ok(None) => break,
                    Err(e) => return Some(Err(e.with_span(span))),
                }
            }
            Some(Ok(Value::Int(n)))
        }
        _ => None,
    }
}
