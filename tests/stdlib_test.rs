//! Integration tests for P3a's stdlib: builtins/{numbers,collections,seq,
//! strings,predicates,atoms}.rs + core/core.mova's bootstrap macros.
//!
//! Note on laziness: `pr_str`/`=` don't auto-realize a lazy-seq chain (see
//! collections.rs's module doc for why) -- tests that produce a lazy result
//! wrap it in `doall`/`take`/`vec`/`into` (a materializing consumer) before
//! printing/comparing, exactly like real Clojure code routinely does.

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

// -------------------- collections: constructors + conj --------------------

#[test]
fn conj_list_prepends_vector_appends() {
    assert_eq!(ps("(conj '(1 2) 3)"), "(3 1 2)");
    assert_eq!(ps("(conj [1 2] 3)"), "[1 2 3]");
}

#[test]
fn conj_map_takes_pair_or_map() {
    assert_eq!(ps("(conj {:a 1} [:b 2])"), "{:a 1, :b 2}");
    assert_eq!(ps("(conj {:a 1} {:b 2 :c 3})"), "{:a 1, :b 2, :c 3}");
}

#[test]
fn conj_set_adds_element() {
    assert_eq!(ps("(conj #{1 2} 2 3)"), "#{1 2 3}");
}

#[test]
fn conj_nil_acts_like_empty_list() {
    assert_eq!(ps("(conj nil 1 2)"), "(2 1)");
}

// -------------------- collections: assoc/dissoc/get/nth --------------------

#[test]
fn assoc_on_map_and_vector() {
    assert_eq!(ps("(assoc {:a 1} :b 2)"), "{:a 1, :b 2}");
    assert_eq!(ps("(assoc [1 2 3] 1 99)"), "[1 99 3]");
    assert_eq!(ps("(assoc [1 2 3] 3 4)"), "[1 2 3 4]");
}

#[test]
fn assoc_nil_creates_a_map() {
    assert_eq!(ps("(assoc nil :a 1)"), "{:a 1}");
}

#[test]
fn dissoc_removes_keys() {
    assert_eq!(ps("(dissoc {:a 1 :b 2} :a)"), "{:b 2}");
}

#[test]
fn get_returns_default_for_missing_key() {
    assert_eq!(ps("(get {:a 1} :b)"), "nil");
    assert_eq!(ps("(get {:a 1} :b :default)"), ":default");
    assert_eq!(ps("(get [1 2] 5 :dflt)"), ":dflt");
}

#[test]
fn nth_out_of_bounds_errors_without_default() {
    let msg = eval_err("(nth [1 2] 5)");
    assert!(msg.contains("out of bounds"), "message: {msg}");
}

#[test]
fn nth_out_of_bounds_uses_default() {
    assert_eq!(ps("(nth [1 2] 5 :dflt)"), ":dflt");
}

#[test]
fn nth_negative_index_uses_default_but_still_throws_without_one() {
    // With a default, a negative index is "not found" too, same as
    // out-of-bounds -- real Clojure: `(nth [] -1 :d)` => `:d`.
    assert_eq!(ps("(nth [] -1 :dflt)"), ":dflt");
    assert_eq!(ps("(nth [1 2 3] -1 :dflt)"), ":dflt");
    // Without a default, negative still errors.
    let msg = eval_err("(nth [1 2] -1)");
    assert!(msg.contains("negative index"), "message: {msg}");
}

#[test]
fn nth_walks_a_seq() {
    assert_eq!(ps("(nth (range 10) 3)"), "3");
}

// -------------------- collections: count/contains?/keys/vals --------------------

#[test]
fn count_across_shapes() {
    assert_eq!(ps("(count [1 2 3])"), "3");
    assert_eq!(ps("(count {:a 1 :b 2})"), "2");
    assert_eq!(ps("(count nil)"), "0");
    assert_eq!(ps("(count \"abc\")"), "3");
}

#[test]
fn contains_checks_key_or_index_membership() {
    assert_eq!(ps("(contains? {:a 1} :a)"), "true");
    assert_eq!(ps("(contains? [1 2] 1)"), "true");
    assert_eq!(ps("(contains? [1 2] 5)"), "false");
    assert_eq!(ps("(contains? nil :a)"), "false");
}

#[test]
fn keys_and_vals() {
    assert_eq!(ps("(sort (keys {:a 1 :b 2}))"), "(:a :b)");
    assert_eq!(ps("(sort (vals {:a 1 :b 2}))"), "(1 2)");
    assert_eq!(ps("(keys {})"), "nil");
}

// -------------------- collections: first/rest/next/cons/seq --------------------

#[test]
fn first_rest_next_on_various_seqables() {
    assert_eq!(ps("(first [1 2 3])"), "1");
    assert_eq!(ps("(rest [1 2 3])"), "(2 3)");
    assert_eq!(ps("(rest [1])"), "()");
    assert_eq!(ps("(next [1])"), "nil");
    assert_eq!(ps("(next [1 2])"), "(2)");
    assert_eq!(ps("(first nil)"), "nil");
    assert_eq!(ps("(rest nil)"), "()");
}

#[test]
fn cons_prepends_and_seq_flattens() {
    assert_eq!(ps("(cons 0 [1 2])"), "(0 1 2)");
    assert_eq!(ps("(cons 0 nil)"), "(0)");
    assert_eq!(ps("(seq [1 2 3])"), "(1 2 3)");
    assert_eq!(ps("(seq [])"), "nil");
    assert_eq!(ps("(seq \"\")"), "nil");
}

// -------------------- collections: into/empty/peek/pop/subvec --------------------

#[test]
fn into_uses_conj_semantics_of_target() {
    assert_eq!(ps("(into [] '(1 2 3))"), "[1 2 3]");
    assert_eq!(ps("(into #{} [1 1 2])"), "#{1 2}");
    assert_eq!(ps("(into {} [[:a 1] [:b 2]])"), "{:a 1, :b 2}");
}

#[test]
fn empty_and_empty_q() {
    assert_eq!(ps("(empty [1 2])"), "[]");
    assert_eq!(ps("(empty? [])"), "true");
    assert_eq!(ps("(empty? [1])"), "false");
    assert_eq!(ps("(empty? nil)"), "true");
}

#[test]
fn peek_pop_stack_ends() {
    assert_eq!(ps("(peek '(1 2 3))"), "1");
    assert_eq!(ps("(peek [1 2 3])"), "3");
    assert_eq!(ps("(pop [1 2 3])"), "[1 2]");
    assert_eq!(ps("(pop '(1 2 3))"), "(2 3)");
}

#[test]
fn subvec_slices() {
    assert_eq!(ps("(subvec [1 2 3 4 5] 1 3)"), "[2 3]");
    assert_eq!(ps("(subvec [1 2 3 4 5] 3)"), "[4 5]");
}

// -------------------- collections: update / assoc-in / get-in / update-in --------------------

#[test]
fn update_applies_fn_to_current_value() {
    assert_eq!(ps("(update {:a 1} :a inc)"), "{:a 2}");
    assert_eq!(ps("(update {:a 1} :b (fn [v] (if v v 0)))"), "{:a 1, :b 0}");
}

#[test]
fn nested_assoc_get_update_in() {
    assert_eq!(ps("(assoc-in {:a {:b 1}} [:a :b] 99)"), "{:a {:b 99}}");
    assert_eq!(ps("(get-in {:a {:b 1}} [:a :b])"), "1");
    assert_eq!(ps("(get-in {:a {:b 1}} [:a :c] :dflt)"), ":dflt");
    assert_eq!(ps("(update-in {:a {:b 1}} [:a :b] + 10)"), "{:a {:b 11}}");
}

#[test]
fn assoc_in_nested_through_a_record() {
    // kondo-wave regression: a defrecord IS associative on the JVM, but
    // assoc-in's nested-lookup match lacked the record arm (same gap as
    // `update`'s, fixed earlier) and threw "not associative: record".
    assert_eq!(
        ps("(defrecord R [a]) (assoc-in (->R {:x 1}) [:a :x] 99)"),
        "#user.R{:a {:x 99}}"
    );
}

// -------------------- lazy seq composition --------------------

#[test]
fn take_from_infinite_range_is_safe() {
    assert_eq!(ps("(take 5 (map inc (range)))"), "(1 2 3 4 5)");
}

#[test]
fn filter_over_infinite_range() {
    assert_eq!(ps("(take 3 (filter even? (range)))"), "(0 2 4)");
}

#[test]
fn filter_with_a_long_run_of_misses_does_not_overflow_stack() {
    // All-odd input: filter has to skip ~50k consecutive non-matches
    // before finding none at all -- this is the case that would blow
    // MAX_CALL_DEPTH if `filter`'s skip loop weren't a real recur
    // trampoline instead of Rust-recursing through nested lazy-seq forces.
    assert_eq!(ps("(pr-str (doall (filter even? (range 1 100000 2))))"), "\"()\"");
}

#[test]
fn remove_is_filter_complement() {
    assert_eq!(ps("(take 3 (remove even? (range)))"), "(1 3 5)");
}

#[test]
fn take_while_and_drop_while() {
    assert_eq!(ps("(doall (take-while (fn [x] (< x 5)) (range)))"), "(0 1 2 3 4)");
    assert_eq!(ps("(take 3 (drop-while (fn [x] (< x 5)) (range 10)))"), "(5 6 7)");
}

// SPEC-W4: `mapcat` is `(apply concat (apply map f colls))`, and `concat`
// is lazy now, so this returns a `clojure.lang.LazySeq` -- which is what
// it is on the JVM too (measured, 1.13.0-alpha6). `ps` goes through
// `Display`, which deliberately does NOT realize (`#<lazy-seq>`), so the
// assertion realizes first, exactly like the `take-while`/`interpose`
// cases around it already do.
#[test]
fn mapcat_flattens() {
    assert_eq!(ps("(doall (mapcat (fn [x] [x x]) [1 2 3]))"), "(1 1 2 2 3 3)");
    assert_eq!(ps("(class (mapcat (fn [x] [x x]) [1 2 3]))"), "clojure.lang.LazySeq");
}

#[test]
fn interpose_inserts_separator() {
    assert_eq!(ps("(doall (interpose 0 [1 2 3]))"), "(1 0 2 0 3)");
}

#[test]
fn cycle_repeats_forever() {
    assert_eq!(ps("(take 7 (cycle [1 2 3]))"), "(1 2 3 1 2 3 1)");
}

// -------------------- range / repeat / iterate / concat --------------------

#[test]
fn range_arities() {
    assert_eq!(ps("(take 3 (range))"), "(0 1 2)");
    // `range` (like every lazy-seq value) prints as `#<lazy-seq>` until
    // something forces it -- `doall` here, `take` above/below -- pr-str
    // itself never auto-realizes a Lazy (see this file's module doc).
    assert_eq!(ps("(doall (range 5))"), "(0 1 2 3 4)");
    assert_eq!(ps("(doall (range 2 5))"), "(2 3 4)");
    assert_eq!(ps("(doall (range 0 10 3))"), "(0 3 6 9)");
    assert_eq!(ps("(doall (range 5 5))"), "()");
}

#[test]
fn repeat_and_iterate() {
    assert_eq!(ps("(doall (repeat 3 :x))"), "(:x :x :x)");
    assert_eq!(ps("(take 3 (repeat :y))"), "(:y :y :y)");
    assert_eq!(ps("(take 4 (iterate inc 0))"), "(0 1 2 3)");
}

// SPEC-W4: `concat` is LAZY now -- as it always was on the JVM: `(class
// (concat [1 2] '(3 4) [5]))` is `clojure.lang.LazySeq` for EVERY arity
// including `(concat)`, and `(concat)` prints `()`, not `nil` (all
// measured on 1.13.0-alpha6). `ps` goes through `Display`, which
// deliberately does not realize a lazy cell (`#<lazy-seq>`), so the
// realization is explicit here.
#[test]
fn concat_joins_finite_seqs() {
    assert_eq!(ps("(doall (concat [1 2] '(3 4) [5]))"), "(1 2 3 4 5)");
    assert_eq!(ps("(doall (concat [1 2]))"), "(1 2)");
    assert_eq!(ps("(class (concat [1 2] '(3 4) [5]))"), "clojure.lang.LazySeq");
    assert_eq!(ps("(class (concat))"), "clojure.lang.LazySeq");
    assert_eq!(ps("(pr-str (concat))"), "\"()\"");
    // Lazy for real: the un-consumed input is never forced, so its
    // element's division by zero never fires.
    assert_eq!(ps("(doall (take 2 (concat [1 2] (map (fn [x] (/ x 0)) [1]))))"), "(1 2)");
    // ... and an infinite tail is fine.
    assert_eq!(ps("(doall (take 5 (concat [1 2] (range))))"), "(1 2 0 1 2)");
}

// -------------------- reduce / doall / dorun / last / apply --------------------

#[test]
fn reduce_2_and_3_arity() {
    assert_eq!(ps("(reduce + [1 2 3 4])"), "10");
    assert_eq!(ps("(reduce + 100 [1 2 3])"), "106");
    assert_eq!(ps("(reduce + [])"), "0");
}

#[test]
fn reduce_over_100000_range_is_stack_safe() {
    assert_eq!(ps("(reduce + (range 100000))"), "4999950000");
}

// -------------------- W-REDUCE: fast paths (Part A arithmetic / Part B
// index-walk) differentially checked against a FORCED-generic twin --------
//
// `plus2`/`mul2`/`max2`/`min2` are ordinary mova closures wrapping `+`/`*`/
// `max`/`min` -- `Value::Fn`, never `Value::Native`, so `reduce`'s
// `recognized_arith` guard (pointer identity against the BOOT `+`/`*`/
// `min`/`max` registration -- see `builtins::seq::BootArith`'s doc) can
// never fire for them, forcing every one of these calls down the
// `ElemWalk`-driven `call_with_buf` loop that `(reduce + ...)` itself
// would have used before W-REDUCE. Comparing the two is a real
// differential test of the new fast paths against the pre-existing
// behavior, not merely "does `reduce` still work".

#[test]
fn reduce_fast_arith_matches_generic_across_chunk_boundaries() {
    // 1023/1024/1025 straddle `GEN_CHUNK` (seq.rs); 2048/2049 straddle the
    // boundary a second time -- the values most likely to expose an
    // off-by-one in `ElemWalk::Fast`'s chunk-boundary re-derivation.
    let src = r#"
      (defn plus2 [a b] (+ a b))
      (defn closed [n] (quot (* n (dec n)) 2))
      (vec (for [n [1 2 3 16 17 1023 1024 1025 2048 2049 5000]]
             [(= (reduce + (range n)) (reduce plus2 (range n)) (closed n))
              (= (reduce + 7 (range n)) (reduce plus2 7 (range n)) (+ 7 (closed n)))]))
    "#;
    let v = eval_ok(src);
    let s = v.to_string();
    assert!(!s.contains("false"), "a chunk-boundary N disagreed: {s}");
}

#[test]
fn reduce_fast_arith_empty_and_no_init() {
    assert_eq!(ps("(reduce + (range 0))"), "0");
    assert_eq!(ps("(reduce + 42 (range 0))"), "42");
    assert_eq!(ps("(reduce + 42 [])"), "42");
    assert_eq!(ps("(reduce * (range 1 1))"), "1");
}

#[test]
fn reduce_fast_arith_negative_and_custom_step() {
    let src = r#"
      (defn plus2 [a b] (+ a b))
      [(= (reduce + (range 10 0 -1)) (reduce plus2 (range 10 0 -1)))
       (= (reduce + (range 10 0 -2)) (reduce plus2 (range 10 0 -2)))
       (= (reduce + (range 0 20 3)) (reduce plus2 (range 0 20 3)))
       (reduce + (range 10 0 -1))
       (reduce + (range 10 0 -2))
       (reduce + (range 0 20 3))]
    "#;
    assert_eq!(ps(src), "[true true true 55 30 63]");
}

#[test]
fn reduce_fast_arith_float_range() {
    let src = r#"
      (defn plus2 [a b] (+ a b))
      [(= (reduce + (range 1.5 10.5)) (reduce plus2 (range 1.5 10.5)))
       (reduce + (range 1.5 10.5))]
    "#;
    assert_eq!(ps(src), "[true 49.5]");
}

#[test]
fn reduce_fast_arith_i64_overflow_matches_generic_throw() {
    // Plain `+` is CHECKED (throws on i64 overflow, does not bignum-
    // promote -- that's `+'`'s job); the fast path must throw the exact
    // same way the generic `call_with_buf`-driven loop does, not silently
    // wrap or diverge.
    let fast = eval_err("(reduce + Long/MAX_VALUE (range 5))");
    let generic = eval_err("(defn plus2 [a b] (+ a b)) (reduce plus2 Long/MAX_VALUE (range 5))");
    assert!(fast.contains("overflow"), "fast path: {fast}");
    assert!(generic.contains("overflow"), "generic path: {generic}");
}

#[test]
fn reduce_fast_arith_mul_and_min_max() {
    let src = r#"
      (defn mul2 [a b] (* a b))
      (defn max2 [a b] (max a b))
      (defn min2 [a b] (min a b))
      [(= (reduce * (range 1 15)) (reduce mul2 (range 1 15)))
       (reduce * (range 1 15))
       (= (reduce max [3 1 4 1 5 9 2 6]) (reduce max2 [3 1 4 1 5 9 2 6]))
       (= (reduce min [3 1 4 1 5 9 2 6]) (reduce min2 [3 1 4 1 5 9 2 6]))
       (reduce max [3 1 4 1 5 9 2 6])
       (reduce min [3 1 4 1 5 9 2 6])]
    "#;
    assert_eq!(ps(src), "[true 87178291200 true true 9 1]");
}

#[test]
fn reduce_fast_arith_shadowed_plus_falls_back() {
    // A lexically shadowed `+` is a DIFFERENT `Value::Fn`/`Value::Native`
    // by the time it reaches `reduce_coll` -- pointer identity against the
    // boot `+` naturally excludes it, no special-case needed.
    assert_eq!(ps("(let [+ -] (reduce + [10 1 2 3]))"), "4");
}

#[test]
fn reduce_fast_arith_redefined_plus_falls_back_and_restores() {
    let src = r#"
      (def orig-plus +)
      (def + (fn [a b] (* a b)))
      (def redefined-result (reduce + [1 2 3 4]))
      (def + orig-plus)
      (def restored-result (reduce + [1 2 3 4]))
      [redefined-result restored-result]
    "#;
    assert_eq!(ps(src), "[24 10]");
}

#[test]
fn reduce_fast_arith_pre_reduced_init_calls_f_once_then_throws() {
    // C10 (see `reduce_coll`'s own doc): a pre-`reduced` `init` does NOT
    // short-circuit before `f`'s first call -- the fast arithmetic path
    // must reproduce that exactly (one `add_step` call against the
    // `Reduced` wrapper, which is not numeric, so it throws on that first
    // step) rather than special-casing `Reduced` away.
    let e = eval_err("(reduce + (reduced 5) [1 2 3])");
    assert!(e.contains("reduced") || e.contains("number"), "{e}");
}

#[test]
fn reduce_user_reduced_short_circuit_unchanged_by_fast_paths() {
    // A user `f` that itself calls `reduced` is never `Value::Native`, so
    // it can never hit `recognized_arith` -- this exercises the
    // `ElemWalk`-driven generic loop's `Reduced` unwrap directly.
    assert_eq!(
        ps("(reduce (fn [a b] (if (> a 100) (reduced a) (+ a b))) (range 1000))"),
        "105"
    );
    assert_eq!(
        ps("(reduce (fn [_ a] (if (= a 5) (reduced \"foo\") a)) 0 [1 2 3 4 5 6 7])"),
        "\"foo\""
    );
}

#[test]
fn reduce_fast_index_walk_vector_matches_range() {
    // `ElemWalk::Fast`'s `Vector` arm against its chunked-`List` (range)
    // arm -- both should fold to the same total via the SAME fast
    // arithmetic path.
    assert_eq!(
        ps("(= (reduce + (vec (range 300000))) (reduce + (range 300000)))"),
        "true"
    );
}

#[test]
fn reduce_over_every_seqable_shape_is_unchanged() {
    let src = r#"
      [(reduce + [1 2 3 4 5])
       (reduce + (list 1 2 3 4 5))
       (reduce + #{1 2 3 4 5})
       (reduce (fn [acc [_ v]] (+ acc v)) 0 {:a 1 :b 2 :c 3})
       (reduce str "" "hello")
       (reduce + (map inc (range 5)))]
    "#;
    assert_eq!(ps(src), "[15 15 15 6 \"hello\" 15]");
}

#[test]
fn doall_and_dorun_realize_a_lazy_seq() {
    assert_eq!(ps("(doall (map inc [1 2 3]))"), "(2 3 4)");
    assert_eq!(ps("(dorun (map inc [1 2 3]))"), "nil");
}

#[test]
fn last_walks_to_the_end() {
    assert_eq!(ps("(last [1 2 3])"), "3");
    assert_eq!(ps("(last nil)"), "nil");
}

#[test]
fn apply_splices_the_final_arg() {
    assert_eq!(ps("(apply + 1 2 [3 4])"), "10");
    assert_eq!(ps("(apply str [\"a\" \"b\"])"), "\"ab\"");
}

/// W3e-3: `apply` onto a `& rest` fn realizes only what it must -- the JVM's
/// `RestFn.applyTo` walks `boundedLength(arglist, requiredArity)` nodes and
/// hands the REST of the seq to `doInvoke` untouched. Every row measured on
/// real Clojure 1.13.0-alpha6 (`compat/apply-lazy-probe.clj` /
/// `compat/apply-lazy-oracle-transcript.txt`).
#[test]
fn apply_does_not_realize_a_variadic_rest_arg() {
    // The `clojure.test-clojure.vars/test-vars-apply-lazily` shape: this
    // used to hang forever.
    assert_eq!(ps("(defn sample [& args] 0) (apply sample (range))"), "0");
    assert_eq!(ps("(defn sample [& args] 0) (apply (var sample) (range))"), "0");
    assert_eq!(ps("(apply (fn [& xs] (first xs)) (range))"), "0");
    assert_eq!(ps("(apply (fn [& xs] (vec (take 3 xs))) (range))"), "[0 1 2]");
    assert_eq!(ps("(apply (fn [a & xs] [a (vec (take 3 xs))]) :lead (range))"), "[:lead [0 1 2]]");
}

/// W3e-3: ...and everything a bounded seq did before is byte-identical,
/// including which arity gets selected. `select_arity_index` prefers an
/// exact fixed match, so the probe must never stop realizing while a fixed
/// arity could still be the answer.
#[test]
fn apply_arity_selection_is_unchanged_for_finite_seqs() {
    let f = "(defn one-plus [a & r] [a (count r)]) ";
    assert_eq!(ps(&format!("{f}(apply one-plus [1])")), "[1 0]");
    assert_eq!(ps(&format!("{f}(apply one-plus [1 2 3])")), "[1 2]");
    assert_eq!(ps(&format!("{f}(apply one-plus 1 2 [3 4])")), "[1 3]");
    assert_eq!(ps(&format!("{f}(apply one-plus 1 2 3 ())")), "[1 2]");
    assert_eq!(ps(&format!("{f}(apply one-plus 1 nil)")), "[1 0]");
    assert_eq!(ps(&format!("{f}(apply one-plus (take 3 (range)))")), "[0 2]");
    // An empty rest is `nil`, not `()` -- Clojure's own answer.
    assert_eq!(ps("(apply (fn [& xs] (nil? xs)) [])"), "true");
    // Multi-arity: the 3-arg body wins over the variadic one at 3 args, and
    // only a 4th argument settles it the other way.
    let m = "(defn multi ([] :zero) ([a] :one) ([a b c] :three) \
             ([a b c d & r] [:var a (count r)])) ";
    assert_eq!(ps(&format!("{m}(apply multi [])")), ":zero");
    assert_eq!(ps(&format!("{m}(apply multi [1])")), ":one");
    assert_eq!(ps(&format!("{m}(apply multi [1 2 3])")), ":three");
    assert_eq!(ps(&format!("{m}(apply multi 1 2 [3])")), ":three");
    assert_eq!(ps(&format!("{m}(apply multi [1 2 3 4])")), "[:var 1 0]");
    assert_eq!(ps(&format!("{m}(apply multi [1 2 3 4 5])")), "[:var 1 1]");
    // Natives and fixed-arity closures are untouched by the new path.
    assert_eq!(ps("(apply + (range 10))"), "45");
    assert_eq!(ps("(apply vector (take 3 (range)))"), "[0 1 2]");
    assert_eq!(ps("(apply (fn [{:keys [a]} & more] [a (vec more)]) {:a 1} [2 3])"), "[1 [2 3]]");
}

// -------------------- some / every? / not-every? / not-any? --------------------

#[test]
fn predicate_walkers() {
    assert_eq!(ps("(some even? [1 3 4])"), "true");
    assert_eq!(ps("(some even? [1 3 5])"), "nil");
    assert_eq!(ps("(every? even? [2 4 6])"), "true");
    assert_eq!(ps("(every? even? [2 3 6])"), "false");
    assert_eq!(ps("(not-every? even? [2 4 6])"), "false");
    assert_eq!(ps("(not-any? even? [1 3 5])"), "true");
}

// -------------------- sort / sort-by / distinct / group-by / frequencies --------------------

#[test]
fn sort_default_and_comparator() {
    assert_eq!(ps("(sort [3 1 2])"), "(1 2 3)");
    assert_eq!(ps("(sort > [3 1 2])"), "(3 2 1)");
    assert_eq!(ps("(sort [3 1.5 2])"), "(1.5 2 3)");
}

#[test]
fn sort_incomparable_types_errors() {
    let msg = eval_err("(sort [1 :a])");
    assert!(msg.contains("compare"), "message: {msg}");
}

#[test]
fn sort_by_key_fn_and_comparator() {
    assert_eq!(ps("(sort-by - [1 2 3])"), "(3 2 1)");
    assert_eq!(ps("(sort-by identity > [1 2 3])"), "(3 2 1)");
}

#[test]
fn distinct_removes_duplicates_by_type_strict_numeric_eq() {
    assert_eq!(ps("(distinct [1 1 2 2 3])"), "(1 2 3)");
    // S5 (SPEC-numtower): `distinct` is defined in terms of `=`, and `=`
    // is category-strict now -- an `Int` and a `Float` of the same
    // numeric value are two DISTINCT elements (measured:
    // `(distinct [1 1.0 1])` => `(1 1.0)`; this form is also now a
    // conformance corpus line, promoted out of the pending
    // equality-hash ledger). This test previously asserted the blend.
    assert_eq!(ps("(distinct [1 1.0 2])"), "(1 1.0 2)");
}

#[test]
fn group_by_buckets_values() {
    assert_eq!(ps("(group-by even? [1 2 3 4])"), "{false [1 3], true [2 4]}");
}

#[test]
fn frequencies_counts_occurrences() {
    assert_eq!(ps("(frequencies [:a :a :b])"), "{:a 2, :b 1}");
}

#[test]
fn partition_and_partition_all() {
    assert_eq!(ps("(partition 2 [1 2 3 4 5])"), "((1 2) (3 4))");
    assert_eq!(ps("(partition-all 2 [1 2 3 4 5])"), "((1 2) (3 4) (5))");
}

#[test]
fn reverse_and_vec() {
    assert_eq!(ps("(reverse [1 2 3])"), "(3 2 1)");
    assert_eq!(ps("(vec (range 5))"), "[0 1 2 3 4]");
}

// -------------------- numbers --------------------

#[test]
fn mod_is_floor_mod_rem_is_truncating() {
    assert_eq!(ps("(mod 7 3)"), "1");
    assert_eq!(ps("(mod -7 3)"), "2");
    assert_eq!(ps("(mod 7 -3)"), "-2");
    assert_eq!(ps("(rem -7 3)"), "-1");
    assert_eq!(ps("(rem 7 -3)"), "1");
}

#[test]
fn quot_truncates_toward_zero() {
    assert_eq!(ps("(quot 7 2)"), "3");
    assert_eq!(ps("(quot -7 2)"), "-3");
}

#[test]
fn div_by_zero_is_an_error() {
    let msg = eval_err("(mod 1 0)");
    assert!(msg.contains("zero"), "message: {msg}");
}

#[test]
fn inc_dec_abs() {
    assert_eq!(ps("(inc 5)"), "6");
    assert_eq!(ps("(dec 5)"), "4");
    assert_eq!(ps("(abs -5)"), "5");
    assert_eq!(ps("(abs 5.5)"), "5.5");
}

#[test]
fn min_max_variadic() {
    assert_eq!(ps("(max 1 5 3)"), "5");
    assert_eq!(ps("(min 1 5 3)"), "1");
    assert_eq!(ps("(max 1 2.5)"), "2.5");
}

#[test]
fn not_equal() {
    assert_eq!(ps("(not= 1 1)"), "false");
    assert_eq!(ps("(not= 1 2)"), "true");
}

// -------------------- predicates --------------------

#[test]
fn numeric_predicates() {
    assert_eq!(ps("[(even? 4) (odd? 3) (pos? 1) (neg? -1) (zero? 0)]"), "[true true true true true]");
}

#[test]
fn type_predicates() {
    let src = "[(nil? nil) (some? 1) (true? true) (false? false) (number? 1) (int? 1) \
                (float? 1.0) (string? \"a\") (keyword? :a) (symbol? 'a) (vector? [1]) \
                (map? {}) (set? #{}) (list? '(1)) (seq? '(1)) (seq? [1]) (fn? +) (coll? [1]) \
                (boolean 0) (not nil) (char? \\a)]";
    let expected = "[true true true true true true true true true true true \
                     true true true true false true true true true true]";
    let normalize = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    assert_eq!(normalize(&ps(src)), normalize(expected));
}

// -------------------- strings --------------------

#[test]
fn str_variadic_treats_nil_as_empty() {
    assert_eq!(ps("(str nil \"a\" 1 nil)"), "\"a1\"");
    assert_eq!(ps("(str)"), "\"\"");
}

#[test]
fn pr_str_and_name_keyword_symbol_subs() {
    assert_eq!(ps("(pr-str 1 \"a\")"), "\"1 \\\"a\\\"\"");
    assert_eq!(ps("(name :foo)"), "\"foo\"");
    assert_eq!(ps("(keyword \"bar\")"), ":bar");
    assert_eq!(ps("(keyword \"ns\" \"bar\")"), ":ns/bar");
    assert_eq!(ps("(symbol \"baz\")"), "baz");
    assert_eq!(ps("(subs \"hello\" 1 3)"), "\"el\"");
    assert_eq!(ps("(subs \"hello\" 2)"), "\"llo\"");
}

#[test]
fn clojure_string_ns_fns_work_bare_and_namespaced() {
    assert_eq!(ps("(split \"a,b,c\" \",\")"), "[\"a\" \"b\" \"c\"]");
    assert_eq!(ps("(clojure.string/split \"a,b,c\" \",\")"), "[\"a\" \"b\" \"c\"]");
    assert_eq!(ps("(string/upper-case \"hi\")"), "\"HI\"");
    assert_eq!(ps("(lower-case \"HI\")"), "\"hi\"");
    assert_eq!(ps("(trim \"  hi  \")"), "\"hi\"");
    assert_eq!(ps("(starts-with? \"hello\" \"he\")"), "true");
    assert_eq!(ps("(ends-with? \"hello\" \"lo\")"), "true");
    assert_eq!(ps("(includes? \"hello\" \"ell\")"), "true");
    assert_eq!(ps("(join \", \" [1 2 3])"), "\"1, 2, 3\"");
    assert_eq!(ps("(clojure.string/replace \"foo bar\" \"bar\" \"baz\")"), "\"foo baz\"");
}

// -------------------- atoms --------------------

#[test]
fn atom_deref_swap_reset() {
    assert_eq!(ps("(deref (atom 5))"), "5");
    assert_eq!(ps("@(atom 5)"), "5");
    assert_eq!(ps("(def a (atom 1)) (swap! a + 10) @a"), "11");
    assert_eq!(ps("(def a (atom 1)) (reset! a 99) @a"), "99");
    assert_eq!(ps("(def a (atom 1)) (swap! a (fn [v x y] (+ v x y)) 2 3) @a"), "6");
}

// -------------------- core.mova macros: defn, cond, and/or, threading --------------------

#[test]
fn defn_multi_arity() {
    let src = "(defn f ([x] x) ([x y] (+ x y))) [(f 1) (f 1 2)]";
    assert_eq!(ps(src), "[1 3]");
}

#[test]
fn defn_variadic() {
    let src = "(defn f [a & more] more) [(f 1) (f 1 2 3)]";
    assert_eq!(ps(src), "[nil (2 3)]");
}

#[test]
fn when_when_not_if_not() {
    assert_eq!(ps("(when true 1 2 3)"), "3");
    assert_eq!(ps("(when false 1)"), "nil");
    assert_eq!(ps("(when-not false 1 2)"), "2");
    assert_eq!(ps("(if-not false :t :f)"), ":t");
}

#[test]
fn cond_semantics() {
    assert_eq!(ps("(cond false 1 false 2 :else 3)"), "3");
    assert_eq!(ps("(cond false 1)"), "nil");
    assert_eq!(ps("(cond)"), "nil");
}

#[test]
fn and_or_semantics() {
    assert_eq!(ps("(and)"), "true");
    assert_eq!(ps("(or)"), "nil");
    assert_eq!(ps("(and 1 2 3)"), "3");
    assert_eq!(ps("(and 1 false 3)"), "false");
    assert_eq!(ps("(or false nil 3)"), "3");
    assert_eq!(ps("(or false nil)"), "nil");
}

#[test]
fn and_or_short_circuit() {
    // If `and`/`or` didn't short-circuit, this would divide by zero.
    assert_eq!(ps("(and false (/ 1 0))"), "false");
    assert_eq!(ps("(or true (/ 1 0))"), "true");
}

#[test]
fn threading_macros() {
    assert_eq!(ps("(-> 1 inc (+ 10))"), "12");
    assert_eq!(ps("(->> 1 inc (+ 10))"), "12");
    assert_eq!(ps("(-> {:a 1} (assoc :b 2) :b)"), "2");
    assert_eq!(ps("(-> 5)"), "5");
}

#[test]
fn when_let_and_if_let() {
    assert_eq!(ps("(when-let [x 5] (* x 2))"), "10");
    assert_eq!(ps("(when-let [x nil] (* x 2))"), "nil");
    assert_eq!(ps("(if-let [x 5] x :none)"), "5");
    assert_eq!(ps("(if-let [x nil] x :none)"), ":none");
}

#[test]
fn dotimes_side_effects_and_returns_nil() {
    let src = "(def a (atom 0)) (dotimes [i 5] (swap! a + i)) @a";
    assert_eq!(ps(src), "10");
}

#[test]
fn identity_second_complement_constantly() {
    assert_eq!(ps("(identity 5)"), "5");
    assert_eq!(ps("(second [1 2 3])"), "2");
    assert_eq!(ps("((complement even?) 3)"), "true");
    assert_eq!(ps("((constantly 5) 1 2 3)"), "5");
}

#[test]
fn comment_is_a_noop() {
    assert_eq!(ps("(comment (this is never evaluated) (/ 1 0)) :ok"), ":ok");
}

// -------------------- clojure-lsp campaign (mova/PLAN.md) --------------------

#[test]
fn sorted_map_realizes_lazy_values_and_answers_keyword_lookup() {
    // `realize_deep`'s missing `SortedMap` arm left a nested `Lazy` value
    // printing `#<lazy-seq>`; `named_lookup`'s missing `SortedMap` arm
    // made `(:kw sorted-map)` always answer `nil`/default regardless of
    // content.
    let m = "(into (sorted-map) {:langs (keep :lang [{:x 1}])})";
    assert_eq!(ps(&format!("(pr-str {m})")), "\"{:langs ()}\"");
    assert_eq!(ps(&format!("(pr-str (:langs {m}))")), "\"()\"");
}

#[test]
fn arrow_eduction_constructor_alias() {
    assert_eq!(ps("(doall (->Eduction (map inc) [1 2 3]))"), "(2 3 4)");
}

#[test]
fn namespace_munge_and_munge() {
    assert_eq!(ps("(namespace-munge \"clojure-lsp.core\")"), "\"clojure_lsp.core\"");
    assert_eq!(ps("(munge \"a-b?\")"), "\"a_b_QMARK_\"");
}

#[test]
fn symbol_sees_through_meta_and_accepts_symbol_ns_and_name() {
    assert_eq!(ps("(symbol (with-meta 'foo {:a 1}))"), "foo");
    assert_eq!(ps("(symbol 'ns-sym 'name-sym)"), "ns-sym/name-sym");
    assert_eq!(ps("(symbol nil \"foo\")"), "foo");
}

#[test]
fn with_meta_accepts_a_metadata_map_that_itself_carries_meta() {
    // `check_meta_map` used to match the raw `Value` and reject a
    // `Value::Meta`-wrapped map, even though it wraps a plain map and is a
    // perfectly valid metadata argument (measured on real Clojure: the meta
    // argument's OWN attached meta is irrelevant, only its map-ness is
    // checked). Same class of bug `conj`'s map branch was fixed for.
    assert_eq!(ps("(meta (with-meta {} (with-meta {:a 1} {:m 1})))"), "{:a 1}");
    // Same fix applies to vary-meta/alter-meta!/reset-meta!'s metadata arg.
    assert_eq!(
        ps("(meta (vary-meta {} (fn [_] (with-meta {:a 1} {:m 1}))))"),
        "{:a 1}"
    );
    assert_eq!(
        ps("(let [a (atom nil)] (alter-meta! a (fn [_] (with-meta {:a 1} {:m 1}))) (meta a))"),
        "{:a 1}"
    );
    assert_eq!(
        ps("(let [a (atom nil)] (reset-meta! a (with-meta {:a 1} {:m 1})) (meta a))"),
        "{:a 1}"
    );
}

#[test]
fn starts_ends_includes_coerce_non_string_via_tostring() {
    // Real `clojure.string/starts-with?`/`ends-with?`/`includes?` call
    // `(.toString s)` on their first arg (a type HINT, not a runtime
    // check) -- a bare symbol/keyword works on the JVM.
    assert_eq!(ps("(clojure.string/ends-with? 'foo.bar \".\")"), "false");
    assert_eq!(ps("(clojure.string/ends-with? 'foo.bar \"bar\")"), "true");
    assert_eq!(ps("(clojure.string/starts-with? :foo/bar \":foo\")"), "true");
    assert_eq!(ps("(clojure.string/includes? 42 \"4\")"), "true");
}

#[test]
fn transit_read_json_decodes_map_set_keyword_symbol_and_cache_refs() {
    // `mova.transit/read-json`: a minimal transit-json document
    // exercising the array-map form, a `~#set` tag, cache codes (`^0`),
    // and a `~i` integer -- same shapes clj-kondo's real built-in
    // `.transit.json` caches use (src/builtins/transit.rs's module doc).
    let json = r#"["^ ","~:flags",["~#set",["~:public","~:static"]],"~:arity",["^ ","~i0",["^ ","~:ret","~:nat-int"]]]"#;
    let src = format!("(mova.transit/read-json {json:?})");
    assert_eq!(ps(&src), "{:flags #{:public :static}, :arity {0 {:ret :nat-int}}}");
}
