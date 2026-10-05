//! W-PROTO: the protocol-method dispatch inline cache (`types::ProtoIc`,
//! consulted by `builtins::types::lookup_method`, fed the `epoch`/`midx`
//! coordinates `eval::types_forms::eval_defprotocol` mints).
//!
//! The cache is a pure performance shortcut, so every test here is a
//! BEHAVIOR test written against observable dispatch results only -- each
//! one first WARMS a call site (so the cache is populated and would be
//! consulted), then mutates the thing the cache must notice, then asserts
//! the answer real Clojure gives. A build with the cache ripped out must
//! pass this file byte-identically; that is the whole point.
//!
//! Same `eval_ok`/`ps` helper convention as `reify_test.rs` and every
//! other integration test file in this crate (no shared test-util module
//! exists).

use mova::embed::{Engine, Value};

fn engine() -> Engine {
    Engine::builder().build()
}

fn eval_ok(src: &str) -> Value {
    engine()
        .eval_named("test", src)
        .unwrap_or_else(|e| panic!("eval error for {src:?}: {}", e.render_plain()))
}

fn eval_err(src: &str) -> String {
    match engine().eval_named("test", src) {
        Ok(v) => panic!("expected an error evaluating {src:?}, got {v:?}"),
        Err(e) => e.to_string(),
    }
}

fn ps(src: &str) -> String {
    eval_ok(src).to_string()
}

// ==================== visibility of later extensions ====================

/// Rule (i): an `extend-type` that lands AFTER a call site has been warmed
/// must be visible on the very next call. `Object` is the sharpest version
/// of this -- it changes what an ALREADY-cached user type resolves to
/// without touching that type's own row, which is why the cache is dropped
/// wholesale on any `impls` write rather than per class key.
#[test]
fn object_extension_after_warm_is_visible_to_a_cached_type() {
    let src = "(defprotocol P (m [x])) \
               (defrecord R [a]) \
               (def r (->R 1)) \
               (extend-type R P (m [_] :exact)) \
               (dotimes [_ 200] (m r)) \
               (extend-type Object P (m [_] :obj)) \
               [(m r) (m 7) (m \"s\")]";
    // R keeps its exact row (it wins over Object); everything else now
    // reaches the freshly added Object row.
    assert_eq!(ps(src), "[:exact :obj :obj]");
}

/// The mirror image: warm against the `Object` row, then add an EXACT row
/// for the cached type. The exact row must win immediately -- a cache that
/// only invalidated the key that was written would still be serving
/// `:obj` here.
#[test]
fn exact_extension_after_warm_beats_a_cached_object_hit() {
    let src = "(defprotocol P (m [x])) \
               (defrecord R [a]) \
               (def r (->R 1)) \
               (extend-type Object P (m [_] :obj)) \
               (dotimes [_ 200] (m r)) \
               (extend-type R P (m [_] :exact)) \
               (m r)";
    assert_eq!(ps(src), ":exact");
}

/// A type first seen only AFTER the cache was warmed for a sibling type
/// still resolves normally (the install path must not poison the bank for
/// keys it has never seen).
#[test]
fn a_type_extended_after_warm_dispatches() {
    let src = "(defprotocol P (m [x])) \
               (defrecord R [a]) (defrecord S [a]) \
               (def r (->R 1)) (def s (->S 1)) \
               (extend-type R P (m [_] :r)) \
               (dotimes [_ 200] (m r)) \
               (extend-type S P (m [_] :s)) \
               [(m r) (m s) (m r) (m s)]";
    assert_eq!(ps(src), "[:r :s :r :s]");
}

// ==================== re-extension (silent wins) ====================

/// Rule: a SECOND `extend-type` on the same class replaces that class's
/// whole method table and silently wins (`register_protocol_impls_ex`'s
/// `impls.insert`). Warmed in between, so the cache is holding the first
/// impl when the second lands.
#[test]
fn re_extension_after_warm_silently_wins() {
    let src = "(defprotocol P (m [x])) \
               (defrecord R [a]) \
               (def r (->R 1)) \
               (extend-type R P (m [_] :one)) \
               (dotimes [_ 200] (m r)) \
               (extend-type R P (m [_] :two)) \
               (dotimes [_ 200] (m r)) \
               (m r)";
    assert_eq!(ps(src), ":two");
}

/// Three generations of the same `(protocol, class)` pair, each warmed --
/// the cache has to re-warm cleanly every time, not just the first.
#[test]
fn repeated_re_extension_each_generation_wins() {
    let src = "(defprotocol P (m [x])) \
               (defrecord R [a]) \
               (def r (->R 1)) \
               (extend-type R P (m [_] 1)) (dotimes [_ 50] (m r)) (def a (m r)) \
               (extend-type R P (m [_] 2)) (dotimes [_ 50] (m r)) (def b (m r)) \
               (extend-type R P (m [_] 3)) (dotimes [_ 50] (m r)) (def c (m r)) \
               [a b c]";
    assert_eq!(ps(src), "[1 2 3]");
}

// ==================== protocol redefinition ====================

/// Rule (ii): re-`defprotocol` REPLACES the whole `ProtoDef` (impls
/// included), so a previously-warmed dispatch now finds nothing and
/// reports the measured no-impl `IllegalArgumentException` -- identical to
/// pre-cache behavior.
#[test]
fn redefprotocol_after_warm_drops_impls() {
    let src = "(defprotocol P (m [x])) \
               (defrecord R [a] P (m [_] :first)) \
               (def r (->R 1)) \
               (dotimes [_ 200] (m r)) \
               (defprotocol P (m [x])) \
               (m r)";
    let e = eval_err(src);
    assert!(
        e.contains("No implementation of method: :m") && e.contains("user.R"),
        "expected the measured no-impl message, got {e:?}"
    );
}

/// The `epoch` guard, isolated. A dispatch fn captured BEFORE a
/// re-`defprotocol` that REORDERS the protocol's methods still carries the
/// old method index; without the epoch check it would probe the new
/// protocol's bank 0 -- which now belongs to a DIFFERENT method -- and
/// return that method's impl. `old-a` must still be `a`.
#[test]
fn stale_dispatch_fn_never_reads_the_new_epochs_banks() {
    let src = "(defprotocol P (a [x]) (b [x])) \
               (defrecord R [f]) \
               (def r (->R 1)) \
               (extend-type R P (a [_] :a1) (b [_] :b1)) \
               (def old-a a) \
               (dotimes [_ 200] (a r) (b r)) \
               (defprotocol P (b [x]) (a [x])) \
               (extend-type R P (a [_] :a2) (b [_] :b2)) \
               (dotimes [_ 200] (a r) (b r)) \
               [(a r) (b r) (old-a r) (old-a r)]";
    assert_eq!(ps(src), "[:a2 :b2 :a2 :a2]");
}

// ==================== reify is never cached ====================

/// Rule (iii): `reify` mints a fresh `TypeDef` per EVALUATION, so it must
/// never enter the cache -- and it must keep dispatching correctly while
/// sharing a protocol (and a call site) with a cached `defrecord`.
#[test]
fn reify_and_record_alternate_on_one_call_site() {
    let src = "(defprotocol P (m [x])) \
               (defrecord R [a] P (m [_] :rec)) \
               (def r (->R 1)) \
               (def y (reify P (m [_] :reify))) \
               (vec (map m (take 40 (cycle [r y]))))";
    let out = ps(src);
    assert_eq!(out.matches(":rec").count(), 20, "in {out:?}");
    assert_eq!(out.matches(":reify").count(), 20, "in {out:?}");
    assert!(out.starts_with("[:rec :reify :rec :reify"), "in {out:?}");
}

/// A FRESH `reify` per iteration -- each one a brand-new `TypeDef`, whose
/// allocation is freed as soon as the value is dropped. If those addresses
/// were ever interned, a later `TypeDef` landing on a recycled address
/// would hit a stale entry; they must simply never be cached, and the
/// record sharing the call site must keep answering correctly throughout.
#[test]
fn fresh_reifies_never_displace_a_cached_record() {
    let src = "(defprotocol P (m [x])) \
               (defrecord R [a] P (m [_] :rec)) \
               (def r (->R 1)) \
               (vec (for [i (range 25)] \
                      [(m r) (m (reify P (m [_] i)))]))";
    let out = ps(src);
    assert!(out.starts_with("[[:rec 0] [:rec 1] [:rec 2]"), "in {out:?}");
    assert!(out.ends_with("[:rec 24]]"), "in {out:?}");
    assert_eq!(out.matches(":rec").count(), 25, "in {out:?}");
}

/// A `reify` that satisfies the protocol but does NOT implement the
/// dispatched method falls through to the registry's `Object` row -- the
/// path that reaches `lookup_method`'s uncached walk with an `Inst` whose
/// `tdef.methods` is non-empty. It must not be interned there either.
#[test]
fn reify_without_the_method_falls_through_to_object() {
    let src = "(defprotocol P (m [x]) (n [x])) \
               (extend-type Object P (m [_] :obj) (n [_] :objn)) \
               (def y (reify P (n [_] :mine))) \
               (dotimes [_ 50] (m y) (n y)) \
               [(m y) (n y)]";
    assert_eq!(ps(src), "[:obj :mine]");
}

// ==================== polymorphic call sites ====================

/// Rule: two record types alternating on ONE call site -- both cached,
/// both correct, no aliasing between the banks' slots.
#[test]
fn two_record_types_alternate_on_one_call_site() {
    let src = "(defprotocol P (m [x])) \
               (defrecord A [a] P (m [_] :a)) \
               (defrecord B [b] P (m [_] :b)) \
               (def xs (vec (take 40 (cycle [(->A 1) (->B 2)])))) \
               (vec (map m xs))";
    let out = ps(src);
    assert_eq!(out.matches(":a").count(), 20, "in {out:?}");
    assert_eq!(out.matches(":b").count(), 20, "in {out:?}");
    assert!(out.starts_with("[:a :b :a :b"), "in {out:?}");
}

/// More distinct types than there are IC slots (`PROTO_IC_SLOTS` is 4):
/// the bank fills and everything past it takes the uncached walk. Every
/// answer must still be right -- an exhausted cache is only slower.
#[test]
fn more_types_than_slots_still_dispatch_correctly() {
    let src = "(defprotocol P (m [x])) \
               (defrecord T1 [x] P (m [_] 1)) (defrecord T2 [x] P (m [_] 2)) \
               (defrecord T3 [x] P (m [_] 3)) (defrecord T4 [x] P (m [_] 4)) \
               (defrecord T5 [x] P (m [_] 5)) (defrecord T6 [x] P (m [_] 6)) \
               (def xs [(->T1 0) (->T2 0) (->T3 0) (->T4 0) (->T5 0) (->T6 0)]) \
               (vec (map m (concat xs xs xs)))";
    assert_eq!(ps(src), "[1 2 3 4 5 6 1 2 3 4 5 6 1 2 3 4 5 6]");
}

/// Builtin class keys are deliberately NOT cached (see `lookup_method`) --
/// they share the protocol and the call site with a cached record, so this
/// pins that mixing the two routes keeps both right.
#[test]
fn builtin_and_record_keys_mix_on_one_call_site() {
    let src = "(defprotocol P (m [x])) \
               (defrecord R [a] P (m [_] :rec)) \
               (extend-type java.lang.Long P (m [_] :long)) \
               (extend-type nil P (m [_] :nil)) \
               (def r (->R 1)) \
               (vec (mapcat (fn [_] [(m r) (m 7) (m nil)]) (range 20)))";
    let out = ps(src);
    assert_eq!(out.matches(":rec").count(), 20, "in {out:?}");
    assert_eq!(out.matches(":long").count(), 20, "in {out:?}");
    assert_eq!(out.matches(":nil").count(), 20, "in {out:?}");
}

// ==================== one bank per method ====================

/// The banks are indexed by DECLARED METHOD ORDER, so a multi-method
/// protocol dispatched on one type must keep each method's answer in its
/// own bank.
#[test]
fn methods_of_one_protocol_do_not_share_a_bank() {
    let src = "(defprotocol P (m1 [x]) (m2 [x]) (m3 [x])) \
               (defrecord R [a] P (m1 [_] :one) (m2 [_] :two) (m3 [_] :three)) \
               (def r (->R 1)) \
               (dotimes [_ 50] (m1 r) (m2 r) (m3 r)) \
               [(m1 r) (m2 r) (m3 r) (m3 r) (m1 r) (m2 r)]";
    assert_eq!(ps(src), "[:one :two :three :three :one :two]");
}

/// A method a protocol declared but that is dropped by a redefinition
/// keeps its own (measured) message, warmed or not -- the cache must not
/// change which of the two no-impl conditions is reported.
#[test]
fn method_removed_by_redefinition_keeps_its_own_message() {
    let src = "(defprotocol P (m [x]) (n [x])) \
               (defrecord R [a]) \
               (def r (->R 1)) \
               (extend-type R P (m [_] :m) (n [_] :n)) \
               (def old-n n) \
               (dotimes [_ 100] (n r)) \
               (defprotocol P (m [x])) \
               (extend-type R P (m [_] :m2)) \
               (old-n r)";
    let e = eval_err(src);
    assert!(
        e.contains("may have been defined before and removed"),
        "expected the removed-method message, got {e:?}"
    );
}

// ==================== cache lives across a fork ====================

/// The registry (and with it the cache) is `Arc`-shared by `Interp::fork`,
/// so a `future` dispatches against the SAME banks the main thread warmed.
/// Exercises the `OnceLock` install/probe path from two threads at once.
#[test]
fn cached_dispatch_survives_a_fork() {
    let src = "(defprotocol P (m [x])) \
               (defrecord R [a] P (m [_] :rec)) \
               (def r (->R 1)) \
               (dotimes [_ 100] (m r)) \
               (let [fs (doall (for [_ (range 4)] \
                                 (future (vec (repeatedly 50 (fn [] (m r)))))))] \
                 (vec (map (fn [f] (count (filter (fn [v] (= v :rec)) @f))) fs)))";
    assert_eq!(ps(src), "[50 50 50 50]");
}

// ==================== defrecord field bindings ====================
//
// W-PROTO also narrowed `wrap_fields_let` to bind only the basis fields a
// method body actually NAMES (that wrapper, not the registry walk, was the
// dominant per-call cost -- see its doc). These pin the scoping rules that
// change must not disturb.

#[test]
fn method_body_sees_its_basis_fields_by_bare_name() {
    assert_eq!(
        ps("(defprotocol P (m [x])) \
            (defrecord R [a b] P (m [_] [a b])) \
            (m (->R 1 2))"),
        "[1 2]"
    );
}

#[test]
fn a_param_shadows_the_field_of_the_same_name() {
    assert_eq!(
        ps("(defprotocol P (m [x y])) \
            (defrecord R [a] P (m [_ a] a)) \
            (m (->R :field) :param)"),
        ":param"
    );
}

#[test]
fn unmentioned_fields_are_still_reachable_through_the_map_nature() {
    // The body names no field, so no field binding is generated -- the
    // record's map nature has to carry the read.
    assert_eq!(
        ps("(defprotocol P (m [x])) \
            (defrecord R [a b] P (m [this] [(:a this) (:b this) (get this :b)])) \
            (m (->R 1 2))"),
        "[1 2 2]"
    );
}

#[test]
fn a_field_named_only_in_a_nested_closure_is_still_bound() {
    assert_eq!(
        ps("(defprotocol P (m [x])) \
            (defrecord R [a] P (m [_] ((fn [] (+ a 1))))) \
            (m (->R 41))"),
        "42"
    );
}

#[test]
fn a_field_named_only_inside_a_recur_loop_is_still_bound() {
    assert_eq!(
        ps("(defprotocol P (m [x n])) \
            (defrecord R [step] P (m [_ n] (loop [i 0 acc 0] \
                                             (if (< i n) (recur (inc i) (+ acc step)) acc)))) \
            (m (->R 3) 4)"),
        "12"
    );
}

#[test]
fn deftype_fields_follow_the_same_rule() {
    assert_eq!(
        ps("(defprotocol P (m [x]) (n [x])) \
            (deftype T [a b] P (m [_] (+ a b)) (n [this] (.a this))) \
            (let [t (T. 3 4)] [(m t) (n t)])"),
        "[7 3]"
    );
}
