//! D1 (`reify` + the vector spliterator/stream veneer): integration tests
//! for `src/eval/types_forms.rs`'s `eval_reify`, the two dispatch paths a
//! `reify` instance answers on (`.method` interop and protocol-fn
//! dispatch), the `clojure.lang.IReduceInit` element source wired into
//! `Interp::seq_items`, and `src/builtins/vecdot.rs`'s
//! `.spliterator`/`.stream`/`.parallelStream` rows.
//!
//! Same `eval_ok`/`eval_err`/`ps` helper convention as every other
//! integration test file in this crate (no shared test-util module
//! exists). Every assertion below mirrors a shape the vendored suite
//! actually expresses -- see each section's own comment for which file.

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

// ==================== reify: interface heads ====================

// `protocols.clj`'s `reify-test` "of an interface" / "of two interfaces".
#[test]
fn reify_interface_method_dispatch() {
    assert_eq!(ps("(let [r (reify java.util.List (contains [_ o] (= :foo o)))] (.contains r :foo))"), "true");
    assert_eq!(ps("(let [r (reify java.util.List (contains [_ o] (= :foo o)))] (.contains r :bar))"), "false");
}

#[test]
fn reify_two_interfaces_share_one_table() {
    let src = "(let [r (reify java.util.List (contains [_ o] (= :foo o)) \
               java.util.Collection (isEmpty [_] false))] [(.contains r :foo) (.isEmpty r)])";
    assert_eq!(ps(src), "[true false]");
}

// A method the reify declared an interface for but did not implement is
// an error (real Clojure: `AbstractMethodError`; the vendored suite's
// `thrown?` is class-blind, so only "throws" is observable).
#[test]
fn reify_unimplemented_method_errors() {
    let e = eval_err("(.add (reify java.util.List (contains [_ o] true)) :baz)");
    assert!(e.contains("add"), "expected the missing method name in {e:?}");
}

// `protocols.clj`'s "you can't define a method twice" -- same name under
// two heads. (The SAME name under ONE head is the multi-arity spelling,
// covered by `reify_protocol_multi_arity` below.)
#[test]
fn reify_rejects_a_duplicate_method_name_across_heads() {
    let e = eval_err(
        "(reify java.util.List (size [_] 10) java.util.Collection (size [_] 20))",
    );
    assert!(e.contains("duplicate method name"), "unexpected error: {e:?}");
}

#[test]
fn reify_instance_of_each_declared_interface_only() {
    let src = "(let [r (reify java.util.List (contains [_ o] true))] \
               [(instance? java.util.List r) (instance? java.util.Collection r)])";
    assert_eq!(ps(src), "[true false]");
}

// A `reify` overriding neither `equals` nor `hashCode` has JVM identity
// semantics -- two structurally identical ones are never `=`.
#[test]
fn reify_instances_are_identity_equal_only() {
    assert_eq!(ps("(let [r (reify java.util.List (contains [_ o] true))] (= r r))"), "true");
    let mk = "(reify java.util.List (contains [_ o] true))";
    assert_eq!(ps(&format!("(= {mk} {mk})")), "false");
}

// ==================== reify: protocol heads ====================

// `protocols.clj`'s `reify-test` "of a protocol": one head, two arities of
// `baz`, reachable BOTH as `.method` interop and as a protocol fn call.
#[test]
fn reify_protocol_multi_arity() {
    let src = "(do (defprotocol P (bar [this o]) (baz [this] [this o])) \
               (let [r (reify P (bar [this o] o) (baz [this] 1) (baz [this o] 2))] \
                 [(.bar r :foo) (.baz r) (.baz r nil) (bar r :x) (baz r) (baz r nil)]))";
    assert_eq!(ps(src), "[:foo 1 2 :x 1 2]");
}

// A `reify`d impl wins over an `extend-type` impl registered for every
// other class -- `reify` is as specific as a dispatch target gets.
#[test]
fn reify_protocol_impl_beats_a_registered_object_impl() {
    let src = "(do (defprotocol P (m [this])) (extend-type java.lang.Object P (m [_] :object)) \
               [(m (reify P (m [_] :reified))) (m 42)])";
    assert_eq!(ps(src), "[:reified :object]");
}

// `protocols.clj`'s "you can implement just part of a protocol if you
// want" -- the unimplemented arity throws rather than silently answering.
#[test]
fn reify_partial_protocol_impl_throws_on_the_missing_arity() {
    let src = "(do (defprotocol P (baz [this] [this o])) \
               [(baz (reify P (baz [a b] :two-arg)) nil)])";
    assert_eq!(ps(src), "[:two-arg]");
    let e = eval_err("(do (defprotocol P (baz [this] [this o])) (baz (reify P (baz [a b] :two-arg))))");
    assert!(!e.is_empty());
}

// ==================== reify: closures, destructuring, recur ====================

// Methods close over the enclosing locals -- and over the locals of THIS
// evaluation, which is why each evaluation mints its own anonymous type.
#[test]
fn reify_methods_close_over_call_site_locals() {
    let src = "(let [mk (fn [n] (reify java.util.List (get [_ _] n))) \
                     a (mk 1) b (mk 2)] [(.get a 0) (.get b 0)])";
    assert_eq!(ps(src), "[1 2]");
}

#[test]
fn reify_method_params_destructure() {
    let src = "(do (defprotocol P (bar [this o])) \
               (.bar (reify P (bar [this [_ _ item]] item)) [:a :b :c]))";
    assert_eq!(ps(src), ":c");
}

// `protocols.clj`'s "methods can recur": `recur` rebinds the method's
// arguments but NOT `this`, so a two-parameter method recurs with ONE
// argument (see `wrap_method_recur`).
#[test]
fn reify_method_recur_excludes_this() {
    let src = "(let [r (reify java.util.List \
                        (get [_ index] (if (zero? index) :done (recur (dec index)))))] \
               [(.get r 0) (.get r 3)])";
    assert_eq!(ps(src), "[:done :done]");
}

// The same rule for a `deftype` method (real Clojure treats both the same
// way; the vendored suite only measures `reify`).
#[test]
fn deftype_method_recur_excludes_this() {
    let src = "(do (definterface ICount (down [n])) \
               (deftype T [] ICount (down [_ n] (if (zero? n) :zero (recur (dec n))))) \
               (.down (T.) 4))";
    assert_eq!(ps(src), ":zero");
}

// ==================== reify: clojure.lang.IReduceInit ====================

// `vectors.clj`'s `test-vec` and `sequences.clj`'s `test-into-IReduceInit`:
// one `seq_items` arm feeds every element-consuming builtin.
#[test]
fn reify_ireduceinit_is_an_element_source() {
    let iri = "(reify clojure.lang.IReduceInit (reduce [_ f start] (reduce f start (range 4))))";
    assert_eq!(ps(&format!("(vec {iri})")), "[0 1 2 3]");
    assert_eq!(ps(&format!("(into [] {iri})")), "[0 1 2 3]");
    assert_eq!(ps(&format!("(count {iri})")), "4");
    assert_eq!(ps(&format!("(doall (map inc {iri}))")), "(1 2 3 4)");
}

// A `reify` with no element source is still opaque, exactly like a
// `deftype` (measured: real Clojure throws on `(seq (T.))`).
#[test]
fn reify_without_ireduceinit_has_no_seq_nature() {
    let e = eval_err("(seq (reify java.util.List (contains [_ o] true)))");
    assert!(e.contains("seq"), "unexpected error: {e:?}");
}

// ==================== reify: rejected heads ====================

#[test]
fn reify_rejects_a_non_class_non_protocol_head() {
    let e = eval_err("(reify 42 (m [_] 1))");
    assert!(e.contains("reify"), "unexpected error: {e:?}");
}

// ==================== spliterator veneer ====================

// `vectors.clj`'s `test-empty-vector-spliterator`.
#[test]
fn empty_vector_spliterator() {
    let src = "(let [s (.spliterator []) seen (atom []) \
                     c (reify java.util.function.Consumer (accept [_ v] (swap! seen conj v)))] \
               [(.estimateSize s) (.getExactSizeIfKnown s) (.trySplit s) (.tryAdvance s c) @seen])";
    assert_eq!(ps(src), "[0 0 nil false []]");
}

// `test-spliterator-tryadvance-then-forEach`: `tryAdvance` walks one
// element at a time, `forEachRemaining` drains the rest, and the two
// together visit every element exactly once, in order.
#[test]
fn spliterator_tryadvance_then_for_each_remaining() {
    let src = "(let [v (vec (range 6)) s (.spliterator v) seen (atom []) \
                     c (reify java.util.function.Consumer (accept [_ x] (swap! seen conj x)))] \
               (.tryAdvance s c) (.tryAdvance s c) \
               (let [left (.estimateSize s)] \
                 (.forEachRemaining s c) \
                 [left @seen (.tryAdvance s c)]))";
    assert_eq!(ps(src), "[4 [0 1 2 3 4 5] false]");
}

// `test-spliterator-trySplit`: recursively split, then walk every split --
// the union is the whole vector, each element seen exactly once. Split
// SHAPE is deliberately unspecified (the vendored test sorts a set of
// what it saw); only the union is a promise.
#[test]
fn spliterator_splits_union_to_the_whole_vector() {
    let src = "(let [n 33 v (vec (range n)) seen (atom []) \
                     c (reify java.util.function.Consumer (accept [_ x] (swap! seen conj x))) \
                     splits (loop [ss [(.spliterator v)]] \
                              (let [ss' (doall (map #(.trySplit %) ss))] \
                                (if (every? nil? ss') ss (recur (into ss (remove nil? ss'))))))] \
               (doseq [s splits] (.forEachRemaining s c)) \
               [(count @seen) (= v (sort @seen))])";
    assert_eq!(ps(src), "[33 true]");
}

#[test]
fn spliterator_of_a_single_element_never_splits() {
    assert_eq!(ps("(.trySplit (.spliterator [:only]))"), "nil");
    assert_eq!(ps("(some? (.trySplit (.spliterator [:a :b])))"), "true");
}

// The parent SHRINKS when it splits -- the two halves are disjoint.
#[test]
fn spliterator_split_shrinks_its_parent() {
    let src = "(let [s (.spliterator (vec (range 8))) t (.trySplit s)] \
               [(.estimateSize t) (.estimateSize s)])";
    assert_eq!(ps(src), "[4 4]");
}

// A `subvec` receiver is a vector like any other (the vendored
// `test-spliterator-tryadvance-then-forEach`/`test-vector-parallel-stream`
// both use one).
#[test]
fn spliterator_over_a_subvec() {
    let src = "(let [s (.spliterator (subvec (vec (range 10)) 2 5)) seen (atom []) \
                     c (reify java.util.function.Consumer (accept [_ x] (swap! seen conj x)))] \
               (.forEachRemaining s c) @seen)";
    assert_eq!(ps(src), "[2 3 4]");
}

// ==================== stream veneer ====================

// `vectors.clj`'s `test-vector-parallel-stream` -- counting is the whole
// of the stream surface (see `builtins::vecdot`'s module doc).
#[test]
fn stream_collect_counting() {
    let src = "(let [v (vec (range 7))] \
               [(.collect (.stream v) (java.util.stream.Collectors/counting)) \
                (.collect (.parallelStream v) (Collectors/counting)) \
                (.collect (.stream (subvec v 0 3)) (Collectors/counting)) \
                (.collect (.stream []) (Collectors/counting))])";
    assert_eq!(ps(src), "[7 7 3 0]");
}

#[test]
fn stream_collect_rejects_an_unknown_collector() {
    let e = eval_err("(.collect (.stream [1 2 3]) :not-a-collector)");
    assert!(e.contains("collect"), "unexpected error: {e:?}");
}

// ==================== W3d2: same-arity overloads (^Tag dispatch) ====================
//
// `protocols.clj`'s `reify-test` "disambiguating with type hints". Two
// clauses sharing a name AND an arity are JVM method OVERLOADS, not the
// multi-arity spelling -- a Clojure `fn` dispatches on arity alone, so a
// straight `eval_fn_form` merge left the second clause unreachable. They
// resolve by parameter `^Tag` against the runtime argument type and
// collapse back into ONE ordinary `MethodTable` value, so no consumer
// learns that overloads exist (see `collect_methods`' own comment).

#[test]
fn reify_same_arity_overloads_dispatch_on_tags() {
    let src = r#"(do (definterface I (hinted [^int i]) (hinted [^String s]))
                     (let [r (reify I (hinted [_ ^int i] (inc i))
                                      (hinted [_ ^String s] (str s s)))]
                       [(.hinted r 1) (.hinted r "xo")]))"#;
    assert_eq!(ps(src), r#"[2 "xoxo"]"#);
}

#[test]
fn reify_overload_reports_when_no_variant_matches_the_argument_types() {
    let src = r#"(do (definterface I (hinted [^int i]) (hinted [^String s]))
                     (let [r (reify I (hinted [_ ^int i] i) (hinted [_ ^String s] s))]
                       (.hinted r :a-keyword)))"#;
    let e = eval_err(src);
    assert!(e.contains("no overload"), "unexpected error: {e:?}");
}

// A name whose clauses all have DISTINCT arities keeps the plain
// multi-arity closure -- the overload path must not capture it.
#[test]
fn reify_multi_arity_is_not_treated_as_an_overload_set() {
    let src = "(do (defprotocol P (baz [this] [this o])) \
                   (let [r (reify P (baz [_] 1) (baz [_ o] 2))] [(.baz r) (.baz r nil)]))";
    assert_eq!(ps(src), "[1 2]");
}

// ==================== W3d2: AbstractMethodError ====================
//
// Oracle-measured on 1.13.0-alpha6: an unimplemented method on an
// anonymous type is `AbstractMethodError` (an `Error`, NOT a
// `RuntimeException`), both when the NAME is missing and when only that
// ARITY is. Carried by the `error::JvmClass::AbstractMethod` tag, so
// these assert through a TYPED `catch` -- the same way the vendored
// corpus asserts it, and the same convention `src/eval/tests.rs`'s W3a
// per-class rows use.

/// `(catch <class> _ :caught)` around `src` -- `":caught"` iff the class
/// matched, and a panic (from `eval_ok`) iff it did not, since an
/// unmatched typed catch lets the error propagate.
fn caught_as(src: &str, class: &str) -> String {
    ps(&format!("(try {src} (catch {class} _ :caught))"))
}

#[test]
fn reify_missing_method_name_is_an_abstract_method_error() {
    let src = "(.add (reify java.util.List (contains [_ o] true)) :baz)";
    assert_eq!(caught_as(src, "AbstractMethodError"), ":caught");
    assert_eq!(caught_as(src, "java.lang.AbstractMethodError"), ":caught");
    // It is an `Error`, so `Error`/`Throwable` catch it ...
    assert_eq!(caught_as(src, "Error"), ":caught");
    // ... and `Exception` deliberately does NOT.
    let e = eval_err(&format!("(try {src} (catch Exception _ :wrong))"));
    assert!(e.contains("does not define or inherit"), "unexpected error: {e:?}");
}

#[test]
fn reify_missing_protocol_arity_is_an_abstract_method_error() {
    let src = "(do (defprotocol P (baz [this] [this o])) (baz (reify P (baz [_ o] :two))))";
    assert_eq!(caught_as(src, "AbstractMethodError"), ":caught");
}

#[test]
fn a_named_deftype_keeps_the_generic_missing_method_message() {
    // The AbstractMethodError phrasing is for ANONYMOUS types only -- real
    // Clojure rejects `(.nope t)` on a named type at compile time, a
    // different condition this arm has never modeled.
    let e = eval_err("(do (deftype T []) (.nope (T.)))");
    assert!(e.contains("no field or interface method"), "unexpected error: {e:?}");
}

// ==================== W3d2: undeclared method names ====================
//
// `reify-test`'s "you can't define a method not on an interface/protocol/
// j.l.Object". Only heads whose method set mova KNOWS (oracle-transcribed
// -- `types::host_interface_methods`) are checked; everything else stays
// permissive so nothing legal can be rejected.

#[test]
fn reify_rejects_a_method_the_host_head_does_not_declare() {
    let e = eval_err("(reify java.util.List (nosuchmethod [_] 1))");
    assert!(e.contains("nosuchmethod"), "unexpected error: {e:?}");
}

#[test]
fn reify_accepts_every_measured_object_method() {
    assert_eq!(ps(r#"(.toString (reify Object (toString [_] "hi")))"#), r#""hi""#);
    assert_eq!(ps("(.hashCode (reify Object (hashCode [_] 42)))"), "42");
}

#[test]
fn reify_stays_permissive_for_a_head_mova_has_no_inventory_for() {
    // `clojure.lang.IReduceInit` is a real vendored head (vectors.clj) and
    // is deliberately NOT in the inventory -- any method name is accepted.
    let src = "(vec (reify clojure.lang.IReduceInit (reduce [_ f start] (f start 1))))";
    assert_eq!(ps(src), "[1]");
}

// ==================== W3d2: satisfies? of a reify'd protocol ====================
//
// Real Clojure answers `(instance? (:on-interface P) x)`: a `reify`
// implements the protocol's generated interface DIRECTLY. It is therefore
// `satisfies?` yet must never appear in `extenders` (nothing `extend`ed
// it). Closed the last row of tests/conformance/pending/records.corpus.

#[test]
fn a_reify_satisfies_the_protocol_it_names() {
    assert_eq!(ps("(do (defprotocol P (m [x])) (satisfies? P (reify P (m [_] 1))))"), "true");
    assert_eq!(ps("(do (defprotocol P (m [x])) (satisfies? P 1))"), "false");
}

#[test]
fn a_reify_is_never_an_extender() {
    assert_eq!(ps("(do (defprotocol P (m [x])) (reify P (m [_] 1)) (extenders P))"), "nil");
}

#[test]
fn a_reify_of_one_protocol_does_not_satisfy_another() {
    let src = "(do (defprotocol P (pm [x])) (defprotocol Q (qm [x])) \
                   (satisfies? Q (reify P (pm [_] 1))))";
    assert_eq!(ps(src), "false");
}
