//! Integration tests for R4's stdlib fill-in: the editor-used stdlib
//! surface mova was missing (see docs/mova-port.md's "R4 -- stdlib
//! fill-in" for the full list). Split from `stdlib_test.rs` (P3a's
//! original seed set) purely to keep each file's scope legible; same
//! `eval_ok`/`eval_err`/`ps` helper convention as every other integration
//! test file in this crate (no shared test-util module exists yet).
//!
//! Note on laziness (same caveat as `stdlib_test.rs`): a lazy-seq-producing
//! result is wrapped in `doall`/`take`/`vec` before printing/comparing.
//!
//! Note on map printing: `Value::Map` is backed by `imbl::HashMap`, whose
//! iteration (and therefore `pr_str`) order for a multi-key map is not
//! something these tests pin down -- assertions on multi-key map RESULTS
//! compare via `(= actual expected)` (order-independent structural
//! equality) rather than comparing `pr_str` output directly.

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

// ==================== merge / merge-with / select-keys ====================

#[test]
fn merge_overlays_maps_left_to_right() {
    assert_eq!(ps("(= (merge {:a 1 :b 2} {:b 3 :c 4}) {:a 1 :b 3 :c 4})"), "true");
}

#[test]
fn merge_with_no_maps_is_nil() {
    assert_eq!(ps("(merge)"), "nil");
    assert_eq!(ps("(merge nil nil)"), "nil");
}

#[test]
fn merge_with_combines_colliding_keys() {
    assert_eq!(ps("(= (merge-with + {:a 1 :b 2} {:a 10} {:a 100 :c 1}) {:a 111 :b 2 :c 1})"), "true");
}

#[test]
fn merge_with_multi_arg_maps_all_fold_through_f() {
    // Three maps, same key in all three -- f must be threaded across every
    // one of them, not just the first collision.
    assert_eq!(ps("(get (merge-with str {:k \"a\"} {:k \"b\"} {:k \"c\"}) :k)"), "\"abc\"");
}

#[test]
fn select_keys_drops_absent_keys() {
    assert_eq!(ps("(= (select-keys {:a 1 :b 2} [:a :z]) {:a 1})"), "true");
}

#[test]
fn select_keys_on_empty_keyseq_is_empty_map() {
    assert_eq!(ps("(select-keys {:a 1} [])"), "{}");
}

// ==================== keep / keep-indexed / map-indexed ====================

#[test]
fn keep_drops_nil_results_keeps_false() {
    assert_eq!(ps("(doall (keep #(when (odd? %) %) [1 2 3 4 5]))"), "(1 3 5)");
}

#[test]
fn keep_on_empty_coll_is_empty_seq() {
    assert_eq!(ps("(doall (keep identity []))"), "()");
}

#[test]
fn keep_indexed_sees_original_indices() {
    assert_eq!(ps("(doall (keep-indexed (fn [i x] (when (even? i) x)) [:a :b :c :d]))"), "(:a :c)");
}

#[test]
fn keep_indexed_on_empty_coll_is_empty_seq() {
    assert_eq!(ps("(doall (keep-indexed (fn [i x] x) []))"), "()");
}

#[test]
fn map_indexed_pairs_index_and_value() {
    assert_eq!(ps("(doall (map-indexed vector [:x :y :z]))"), "([0 :x] [1 :y] [2 :z])");
}

#[test]
fn map_indexed_on_empty_coll_is_empty_seq() {
    assert_eq!(ps("(doall (map-indexed vector []))"), "()");
}

// ==================== reduce-kv (maps AND vectors) ====================

#[test]
fn reduce_kv_over_map_sums_values() {
    assert_eq!(ps("(reduce-kv (fn [acc _ v] (+ acc v)) 0 {:a 1 :b 2 :c 3})"), "6");
}

#[test]
fn reduce_kv_over_vector_gives_indices() {
    assert_eq!(ps("(reduce-kv (fn [acc i v] (conj acc [i v])) [] [:x :y :z])"), "[[0 :x] [1 :y] [2 :z]]");
}

#[test]
fn reduce_kv_on_empty_returns_init_unchanged() {
    assert_eq!(ps("(reduce-kv (fn [acc k v] (conj acc k)) :init {})"), ":init");
    assert_eq!(ps("(reduce-kv (fn [acc i v] (conj acc i)) :init [])"), ":init");
}

// ==================== disj / butlast / filterv / mapv ====================

#[test]
fn disj_removes_multiple_elements() {
    assert_eq!(ps("(= (disj #{1 2 3 4} 2 4) #{1 3})"), "true");
}

#[test]
fn disj_on_nil_is_nil() {
    assert_eq!(ps("(disj nil 1)"), "nil");
}

#[test]
fn butlast_drops_the_last_element() {
    assert_eq!(ps("(butlast [1 2 3])"), "(1 2)");
}

#[test]
fn butlast_on_empty_or_singleton_is_nil() {
    assert_eq!(ps("(butlast [])"), "nil");
    assert_eq!(ps("(butlast [1])"), "nil");
}

#[test]
fn take_last_returns_the_final_n_elements() {
    assert_eq!(ps("(take-last 2 [1 2 3])"), "(2 3)");
    // n exceeds the collection's length: the whole thing comes back.
    assert_eq!(ps("(take-last 5 [1 2])"), "(1 2)");
}

#[test]
fn take_last_of_n_lte_0_or_empty_coll_is_nil() {
    assert_eq!(ps("(take-last 0 [1 2])"), "nil");
    assert_eq!(ps("(take-last -3 [1 2])"), "nil");
    assert_eq!(ps("(take-last 3 [])"), "nil");
}

#[test]
fn filterv_returns_a_vector() {
    assert_eq!(ps("(filterv even? [1 2 3 4 5 6])"), "[2 4 6]");
}

#[test]
fn filterv_on_empty_coll_is_empty_vector() {
    assert_eq!(ps("(filterv even? [])"), "[]");
}

#[test]
fn mapv_returns_a_vector() {
    assert_eq!(ps("(mapv inc [1 2 3])"), "[2 3 4]");
}

#[test]
fn mapv_multi_collection_stops_at_shortest() {
    assert_eq!(ps("(mapv + [1 2 3] [10 20])"), "[11 22]");
}

// ==================== flatten / zipmap / split-at / interleave ====================

// SPEC-W4: `flatten` is built on `mapcat`/`concat`, both lazy now, so it
// returns a `clojure.lang.LazySeq` -- which is what it is on the JVM too
// (measured, 1.13.0-alpha6). `ps` goes through `Display`, which
// deliberately does not realize a lazy cell (`#<lazy-seq>`).
#[test]
fn flatten_flattens_nested_sequentials() {
    assert_eq!(ps("(doall (flatten [1 [2 [3 [4]] 5] 6]))"), "(1 2 3 4 5 6)");
    assert_eq!(ps("(class (flatten [1 [2 [3 [4]] 5] 6]))"), "clojure.lang.LazySeq");
}

#[test]
fn flatten_non_sequential_top_level_is_empty() {
    assert_eq!(ps("(flatten 5)"), "()");
}

#[test]
fn zipmap_pairs_keys_and_vals() {
    assert_eq!(ps("(= (zipmap [:a :b :c] [1 2 3]) {:a 1 :b 2 :c 3})"), "true");
}

#[test]
fn zipmap_stops_at_shorter_coll() {
    assert_eq!(ps("(= (zipmap [:a :b :c] [1 2]) {:a 1 :b 2})"), "true");
}

#[test]
fn split_at_partitions_take_and_drop() {
    assert_eq!(ps("(split-at 2 [1 2 3 4 5])"), "[(1 2) (3 4 5)]");
}

#[test]
fn split_at_zero_is_empty_prefix() {
    assert_eq!(ps("(split-at 0 [1 2])"), "[() (1 2)]");
}

#[test]
fn interleave_zips_two_colls() {
    assert_eq!(ps("(doall (interleave [1 2 3] [:a :b :c]))"), "(1 :a 2 :b 3 :c)");
}

#[test]
fn interleave_stops_at_shortest() {
    assert_eq!(ps("(doall (interleave [1 2 3] [:a]))"), "(1 :a)");
}

// ==================== identical? / integer? / sequential? ====================

#[test]
fn identical_true_for_same_keyword_and_same_object() {
    assert_eq!(ps("(identical? :a :a)"), "true");
    assert_eq!(ps("(let [v [1 2]] (identical? v v))"), "true");
}

#[test]
fn identical_false_for_equal_but_distinct_vectors() {
    assert_eq!(ps("(identical? [1 2] [1 2])"), "false");
}

#[test]
fn integer_true_only_for_int_not_float() {
    assert_eq!(ps("(integer? 5)"), "true");
    assert_eq!(ps("(integer? 5.0)"), "false");
}

#[test]
fn sequential_true_for_lists_and_vectors_false_for_maps_and_nil() {
    assert_eq!(ps("(sequential? '(1 2))"), "true");
    assert_eq!(ps("(sequential? [1 2])"), "true");
    assert_eq!(ps("(sequential? {:a 1})"), "false");
    assert_eq!(ps("(sequential? nil)"), "false");
}

// ==================== parse-long / parse-double ====================

#[test]
fn parse_long_accepts_optional_sign() {
    assert_eq!(ps("(parse-long \"42\")"), "42");
    assert_eq!(ps("(parse-long \"-42\")"), "-42");
    assert_eq!(ps("(parse-long \"+42\")"), "42");
}

#[test]
fn parse_long_nil_on_non_numeric_or_partial_match() {
    assert_eq!(ps("(parse-long \"abc\")"), "nil");
    assert_eq!(ps("(parse-long \"12.5\")"), "nil");
    assert_eq!(ps("(parse-long \"12abc\")"), "nil");
}

#[test]
fn parse_double_accepts_decimals_and_sign() {
    assert_eq!(ps("(parse-double \"3.14\")"), "3.14");
    assert_eq!(ps("(parse-double \"-2.5\")"), "-2.5");
}

#[test]
fn parse_double_nil_on_non_numeric() {
    assert_eq!(ps("(parse-double \"abc\")"), "nil");
}

// ==================== bitwise ops ====================

#[test]
fn bitwise_and_or_xor() {
    assert_eq!(ps("(bit-and 12 10)"), "8");
    assert_eq!(ps("(bit-or 12 3)"), "15");
    assert_eq!(ps("(bit-xor 5 3)"), "6");
}

#[test]
fn bitwise_not_is_two_s_complement() {
    assert_eq!(ps("(bit-not 0)"), "-1");
    assert_eq!(ps("(bit-not -1)"), "0");
}

#[test]
fn bitwise_shifts() {
    assert_eq!(ps("(bit-shift-left 1 8)"), "256");
    assert_eq!(ps("(bit-shift-right 256 4)"), "16");
}

// ==================== int/long/double/float casts, char ====================

#[test]
fn int_and_long_truncate_toward_zero() {
    assert_eq!(ps("(int 3.7)"), "3");
    assert_eq!(ps("(int -3.7)"), "-3");
    assert_eq!(ps("(long -9.99)"), "-9");
}

#[test]
fn int_of_char_is_its_codepoint() {
    assert_eq!(ps("(int \\A)"), "65");
    assert_eq!(ps("(long \\a)"), "97");
}

#[test]
fn double_and_float_widen_ints_and_chars() {
    assert_eq!(ps("(double 3)"), "3.0");
    assert_eq!(ps("(float 3)"), "3.0");
    assert_eq!(ps("(double \\A)"), "65.0");
}

#[test]
fn char_of_int_is_the_char_and_round_trips() {
    assert_eq!(ps("(char 65)"), "\\A");
    assert_eq!(ps("(int (char 97))"), "97");
}

// ==================== some-> / some->> / cond-> / cond->> ====================

#[test]
fn some_arrow_short_circuits_on_nil() {
    assert_eq!(ps("(some-> {:a 1} :a inc)"), "2");
    assert_eq!(ps("(some-> {:a 1} :missing inc)"), "nil");
}

#[test]
fn some_arrow_last_short_circuits_on_nil_from_the_source() {
    assert_eq!(ps("(some-> nil :a)"), "nil");
}

#[test]
fn some_double_arrow_threads_as_last_arg() {
    assert_eq!(ps("(some->> 1 (+ 1) (+ 1))"), "3");
    assert_eq!(ps("(some->> nil (+ 1))"), "nil");
}

#[test]
fn cond_arrow_only_applies_truthy_steps() {
    assert_eq!(ps("(cond-> 1 true inc false (* 100) true (* 10))"), "20");
}

#[test]
fn cond_arrow_with_no_truthy_test_returns_input_unchanged() {
    assert_eq!(ps("(cond-> 5 false inc)"), "5");
}

#[test]
fn cond_double_arrow_threads_as_last_arg() {
    assert_eq!(ps("(cond->> 1 true (+ 10) true (* 2))"), "22");
}

// ==================== defonce / defn- / declare ====================

#[test]
fn defonce_sets_once() {
    assert_eq!(ps("(do (defonce once-a 1) once-a)"), "1");
}

#[test]
fn defonce_reeval_keeps_first_value() {
    assert_eq!(ps("(do (defonce once-b 1) (defonce once-b 2) once-b)"), "1");
}

#[test]
fn defn_minus_defines_a_callable_fn() {
    assert_eq!(ps("(do (defn- secret [x] (* x 2)) (secret 21))"), "42");
}

#[test]
fn declare_then_later_def_is_visible() {
    // Before the real `def`, the declared name is interned but unbound,
    // exactly like real Clojure -- referencing it directly is still an
    // error (no value exists yet). Once the real `def` runs, it resolves.
    assert_eq!(ps("(do (declare later-x) (def later-x 99) later-x)"), "99");
}

#[test]
fn declare_leaves_the_var_genuinely_unbound_until_def() {
    let msg = eval_err("(do (declare never-defd) never-defd)");
    assert!(msg.contains("never-defd") || msg.to_lowercase().contains("resolve"), "message: {msg}");
}

// ==================== ex-info / ex-data / ex-message ====================

#[test]
fn ex_info_round_trips_message_and_data() {
    assert_eq!(ps("(ex-message (ex-info \"boom\" {:code 42}))"), "\"boom\"");
    assert_eq!(ps("(= (ex-data (ex-info \"boom\" {:code 42})) {:code 42})"), "true");
}

#[test]
fn ex_message_and_ex_data_nil_on_non_ex_info() {
    assert_eq!(ps("(ex-message 5)"), "nil");
    assert_eq!(ps("(ex-data \"plain string\")"), "nil");
    assert_eq!(ps("(ex-data nil)"), "nil");
}

// ==================== transients (persistent aliases) ====================

#[test]
fn transient_persistent_round_trip_via_conj() {
    assert_eq!(ps("(persistent! (conj! (conj! (transient []) 1) 2))"), "[1 2]");
}

#[test]
fn assoc_bang_on_empty_map_matches_assoc() {
    assert_eq!(ps("(= (assoc! {} :a 1 :b 2) (assoc {} :a 1 :b 2))"), "true");
}

// ==================== clojure.set ====================

#[test]
fn set_difference_union_intersection() {
    assert_eq!(ps("(= (clojure.set/difference #{1 2 3} #{2 3}) #{1})"), "true");
    assert_eq!(ps("(= (set/union #{1 2} #{2 3}) #{1 2 3})"), "true");
    assert_eq!(ps("(= (clojure.set/intersection #{1 2 3} #{2 3 4}) #{2 3})"), "true");
    // Bare (unqualified) names too, mirroring clojure.string's precedent.
    assert_eq!(ps("(= (difference #{1 2} #{1}) #{2})"), "true");
}

#[test]
fn set_union_is_total_on_nil() {
    // Real `clojure.set/union`: 1-arg is `([s1] s1)` (no set check, nil
    // passes through); 2+-arg folds via `conj`/`into`, which treat nil as
    // an empty seq. Was: unconditional `require_set` threw "union: not a
    // set: nil" (hit live via clojure-lsp completion on Mova).
    assert_eq!(ps("(clojure.set/union nil)"), "nil");
    assert_eq!(ps("(= (clojure.set/union nil #{1 2}) #{1 2})"), "true");
    assert_eq!(ps("(= (clojure.set/union #{1 2} nil) #{1 2})"), "true");
}

#[test]
fn set_difference_with_no_overlap_is_unchanged() {
    assert_eq!(ps("(= (clojure.set/difference #{1 2} #{9}) #{1 2})"), "true");
}

#[test]
fn set_intersection_with_disjoint_sets_is_empty() {
    assert_eq!(ps("(clojure.set/intersection #{1 2} #{3 4})"), "#{}");
}

// ---- subset? / superset? ----

#[test]
fn subset_and_superset_basic() {
    assert_eq!(ps("(clojure.set/subset? #{1} #{1 2})"), "true");
    assert_eq!(ps("(clojure.set/subset? #{1 2} #{1})"), "false");
    assert_eq!(ps("(clojure.set/superset? #{1 2} #{1})"), "true");
    assert_eq!(ps("(set/superset? #{1} #{1 2})"), "false");
}

#[test]
fn subset_and_superset_are_total_on_nil() {
    // Measured against real Clojure (1.13.0-alpha6): neither is set-only --
    // both go through the generic `count`/`contains?`/`every?`, and `nil`
    // is total for both (counts as 0, iterates as empty).
    assert_eq!(ps("(clojure.set/subset? #{} nil)"), "true");
    assert_eq!(ps("(clojure.set/subset? nil #{})"), "true");
    assert_eq!(ps("(clojure.set/superset? #{} nil)"), "true");
}

// ---- select ----

#[test]
fn select_filters_a_set_by_predicate() {
    assert_eq!(ps("(= (clojure.set/select even? #{1 2 3 4}) #{2 4})"), "true");
    assert_eq!(ps("(clojure.set/select even? #{})"), "#{}");
}

#[test]
fn select_on_nil_is_nil_not_empty_set() {
    // Measured: `(select even? nil)` => `nil`, NOT `#{}` -- `select`'s
    // `reduce` never iterates over a `nil` seed, so the seed (`nil`) comes
    // back unchanged.
    assert_eq!(ps("(clojure.set/select even? nil)"), "nil");
}

// ---- project ----

#[test]
fn project_keeps_only_the_given_keys() {
    assert_eq!(ps("(= (clojure.set/project #{{:a 1 :b 2}} [:a]) #{{:a 1}})"), "true");
    assert_eq!(
        ps("(= (clojure.set/project #{{:a 1 :b 2} {:a 1 :b 3}} [:a]) #{{:a 1}})"),
        "true"
    );
}

#[test]
fn project_on_empty_or_nil_rel_is_empty_set() {
    assert_eq!(ps("(clojure.set/project #{} [:a])"), "#{}");
    assert_eq!(ps("(clojure.set/project nil [:a])"), "#{}");
}

#[test]
fn project_with_a_missing_key_drops_it_from_every_row() {
    assert_eq!(ps("(clojure.set/project #{{:a 1}} [:z])"), "#{{}}");
}

// ---- rename / rename-keys ----

#[test]
fn rename_renames_keys_across_every_row() {
    assert_eq!(ps("(= (clojure.set/rename #{{:a 1 :b 2}} {:a :x}) #{{:b 2 :x 1}})"), "true");
}

#[test]
fn rename_ignores_a_kmap_key_absent_from_the_row() {
    assert_eq!(ps("(= (clojure.set/rename #{{:a 1 :b 2}} {:z :x}) #{{:a 1 :b 2}})"), "true");
}

#[test]
fn rename_on_nil_rel_is_empty_set() {
    assert_eq!(ps("(clojure.set/rename nil {:a :x})"), "#{}");
}

#[test]
fn rename_keys_renames_a_single_map() {
    assert_eq!(ps("(= (clojure.set/rename-keys {:a 1 :b 2} {:a :x}) {:b 2 :x 1})"), "true");
}

#[test]
fn rename_keys_dissocs_the_target_key_up_front() {
    // Measured: the target key's ORIGINAL value never survives, even
    // though it's the same key the source is being renamed onto.
    assert_eq!(ps("(clojure.set/rename-keys {:a 1 :b 2} {:a :b})"), "{:b 1}");
}

#[test]
fn rename_keys_collision_winner_is_kmaps_own_iteration_order() {
    // Measured against real Clojure: `(rename-keys {:a 1 :b 2} {:a :c :b :c})`
    // => `{:c 2}` -- the LAST kmap pair visited (by kmap's own iteration
    // order) wins the collision on `:c`, decided against the ORIGINAL map,
    // not a naive left-to-right per-key walk.
    assert_eq!(ps("(clojure.set/rename-keys {:a 1 :b 2} {:a :c :b :c})"), "{:c 2}");
}

#[test]
fn rename_keys_on_nil_map_is_nil() {
    // Measured: total-on-nil `dissoc`/`contains?`/`get` collapse the whole
    // call to `nil` unchanged -- NOT `{}` (contrast with `rename`/
    // `project`/`index`, which all measure a `nil` xrel as zero rows).
    assert_eq!(ps("(clojure.set/rename-keys nil {:a :x})"), "nil");
}

// ---- map-invert ----

#[test]
fn map_invert_swaps_keys_and_values() {
    assert_eq!(ps("(= (clojure.set/map-invert {:a 1}) {1 :a})"), "true");
    assert_eq!(ps("(= (clojure.set/map-invert {:a 1 :b 2}) {1 :a 2 :b})"), "true");
    assert_eq!(ps("(clojure.set/map-invert {})"), "{}");
    assert_eq!(ps("(clojure.set/map-invert nil)"), "{}");
}

#[test]
fn map_invert_collision_keeps_exactly_one_winner() {
    // Measured against real Clojure: on a colliding value, the winner is
    // whichever key was visited LAST in the source map's own iteration
    // order (a "last write wins" fold, not a merge) -- pinned here only to
    // the RULE (exactly one winning entry survives), since mova's `PMap`
    // and Clojure's own hash-map don't share an iteration order.
    let result = ps("(clojure.set/map-invert {:a 1 :b 1})");
    assert!(result == "{1 :a}" || result == "{1 :b}", "unexpected: {result}");
}

// ---- index ----

#[test]
fn index_groups_rows_by_the_given_keys() {
    assert_eq!(
        ps("(= (clojure.set/index #{{:a 1 :b 2}} [:a]) {{:a 1} #{{:a 1 :b 2}}})"),
        "true"
    );
    assert_eq!(
        ps("(= (clojure.set/index #{{:a 1 :b 2} {:a 1 :b 3}} [:a]) {{:a 1} #{{:a 1 :b 2} {:a 1 :b 3}}})"),
        "true"
    );
}

#[test]
fn index_on_empty_or_nil_rel_is_empty_map() {
    assert_eq!(ps("(clojure.set/index #{} [:a])"), "{}");
    assert_eq!(ps("(clojure.set/index nil [:a])"), "{}");
}

#[test]
fn index_groups_a_row_missing_the_key_under_the_empty_map_key() {
    assert_eq!(ps("(= (clojure.set/index #{{:a 1}} [:z]) {{} #{{:a 1}}})"), "true");
}

// ---- join ----

#[test]
fn join_natural_merges_matching_rows() {
    assert_eq!(
        ps("(= (clojure.set/join #{{:a 1 :b 2}} #{{:a 1 :c 3}}) #{{:a 1 :b 2 :c 3}})"),
        "true"
    );
}

#[test]
fn join_natural_drops_non_matching_rows() {
    assert_eq!(ps("(clojure.set/join #{{:a 1 :b 2}} #{{:a 2 :c 3}})"), "#{}");
}

#[test]
fn join_with_keymap_matches_on_the_mapped_key_names() {
    assert_eq!(
        ps("(= (clojure.set/join #{{:a 1 :b 2}} #{{:x 1 :c 3}} {:a :x}) #{{:a 1 :b 2 :x 1 :c 3}})"),
        "true"
    );
}

#[test]
fn join_on_empty_or_nil_rel_is_empty_set() {
    assert_eq!(ps("(clojure.set/join #{} #{{:a 1}})"), "#{}");
    assert_eq!(ps("(clojure.set/join nil nil)"), "#{}");
}

#[test]
fn join_with_no_common_keys_is_a_cartesian_product() {
    // Measured against real Clojure: with no key correspondence at all,
    // EVERY row's `select-keys` projects to `{}`, so the natural join
    // degrades to a full cartesian product instead of an empty result.
    assert_eq!(ps("(= (clojure.set/join #{{:b 1}} #{{:c 2}}) #{{:b 1 :c 2}})"), "true");
}

// ==================== clojure.string additions ====================

#[test]
fn blank_true_for_nil_empty_and_whitespace() {
    assert_eq!(ps("(clojure.string/blank? \"\")"), "true");
    assert_eq!(ps("(clojure.string/blank? \"   \")"), "true");
    assert_eq!(ps("(clojure.string/blank? nil)"), "true");
}

#[test]
fn blank_false_for_non_whitespace() {
    assert_eq!(ps("(string/blank? \" a \")"), "false");
}

#[test]
fn index_of_finds_first_occurrence() {
    assert_eq!(ps("(clojure.string/index-of \"abcabc\" \"b\")"), "1");
}

#[test]
fn index_of_nil_when_absent() {
    assert_eq!(ps("(clojure.string/index-of \"abc\" \"z\")"), "nil");
}

#[test]
fn index_of_three_arity_searches_from_offset() {
    assert_eq!(ps("(clojure.string/index-of \"abcabc\" \"b\" 2)"), "4");
}

#[test]
fn last_index_of_finds_last_occurrence() {
    assert_eq!(ps("(clojure.string/last-index-of \"abcabc\" \"b\")"), "4");
}

#[test]
fn last_index_of_nil_when_absent() {
    assert_eq!(ps("(clojure.string/last-index-of \"abc\" \"z\")"), "nil");
}

#[test]
fn last_index_of_three_arity_searches_backward_from_offset() {
    assert_eq!(ps("(clojure.string/last-index-of \"abcabc\" \"b\" 3)"), "1");
}

#[test]
fn split_lines_splits_on_newlines() {
    assert_eq!(ps("(clojure.string/split-lines \"a\\nb\\nc\")"), "[\"a\" \"b\" \"c\"]");
}

#[test]
fn split_lines_on_empty_string_is_empty_vector() {
    assert_eq!(ps("(clojure.string/split-lines \"\")"), "[]");
}

#[test]
fn triml_trims_only_leading_whitespace() {
    assert_eq!(ps("(clojure.string/triml \"  a  \")"), "\"a  \"");
}

#[test]
fn trimr_trims_only_trailing_whitespace() {
    assert_eq!(ps("(clojure.string/trimr \"  a  \")"), "\"  a\"");
}

#[test]
fn replace_first_replaces_only_the_first_match() {
    assert_eq!(ps("(clojure.string/replace-first \"a.b.c\" \".\" \"-\")"), "\"a-b.c\"");
}

#[test]
fn replace_first_unchanged_when_pattern_absent() {
    assert_eq!(ps("(clojure.string/replace-first \"abc\" \"z\" \"-\")"), "\"abc\"");
}

// ==================== pr ====================

#[test]
fn pr_returns_nil_like_prn() {
    assert_eq!(eval_ok("(pr \"hi\")"), Value::from(()));
    assert_eq!(eval_ok("(pr 1 2 3)"), Value::from(()));
}

// ==================== memoize / repeatedly ====================

#[test]
fn memoize_calls_the_underlying_fn_once_per_distinct_args() {
    assert_eq!(
        ps("(let [calls (atom 0)
                  mf (memoize (fn [x] (swap! calls inc) (* x x)))]
              [(mf 5) (mf 5) (mf 5) (mf 6) @calls])"),
        "[25 25 25 36 2]"
    );
}

#[test]
fn repeatedly_calls_f_each_time_not_cached() {
    assert_eq!(
        ps("(let [n (atom 0)]
              (doall (repeatedly 3 (fn [] (swap! n inc)))))"),
        "(1 2 3)"
    );
}

#[test]
fn repeatedly_one_arity_is_infinite_take_bounds_it() {
    assert_eq!(ps("(doall (take 3 (repeatedly (fn [] 9))))"), "(9 9 9)");
}

// ==================== M3: everyday core fn/macro batch ====================
// Measured against real Clojure 1.13.0-alpha6 via `clojure -e` (see
// core.mova's M3 section header for the raw-probe pointer).

#[test]
fn as_arrow_threads_name_through_each_form() {
    assert_eq!(ps("(as-> 1 x (+ x 1) (* x 2))"), "4");
}

#[test]
fn as_arrow_zero_forms_returns_expr_unchanged() {
    assert_eq!(ps("(as-> 5 x)"), "5");
}

#[test]
fn when_some_runs_body_unless_nil_even_for_false() {
    assert_eq!(ps("(when-some [x 1] (+ x 1))"), "2");
    assert_eq!(ps("(when-some [x nil] 99)"), "nil");
    assert_eq!(ps(r#"(when-some [x false] (str "got " x))"#), "\"got false\"");
}

#[test]
fn if_some_branches_on_nil_not_truthiness() {
    assert_eq!(ps("(if-some [x 1] x :else)"), "1");
    assert_eq!(ps("(if-some [x nil] x :else)"), ":else");
    assert_eq!(ps("(if-some [x false] x :else)"), "false");
}

#[test]
fn when_first_binds_first_element_or_nil_on_empty_or_nil_coll() {
    assert_eq!(ps("(when-first [x [1 2 3]] (* x 10))"), "10");
    assert_eq!(ps("(when-first [x []] 99)"), "nil");
    assert_eq!(ps("(when-first [x nil] 99)"), "nil");
}

#[test]
fn some_fn_short_circuits_on_first_logical_true() {
    assert_eq!(ps("((some-fn even? odd?) 3)"), "true");
    // Multi-arity: tries the combined predicate against x, then y, then z.
    assert_eq!(ps("((some-fn nil? false?) 1 2 false)"), "true");
    assert_eq!(ps("((some-fn nil? false?) 1 2 3)"), "nil");
}

#[test]
fn every_pred_requires_all_args_to_pass_every_predicate() {
    assert_eq!(ps("((every-pred even? pos?) 2 4 6)"), "true");
    assert_eq!(ps("((every-pred even? pos?) 2 -4 6)"), "false");
}

#[test]
fn run_bang_applies_proc_for_side_effects_and_returns_nil() {
    assert_eq!(
        ps("(let [seen (atom [])] (run! (fn [x] (swap! seen conj x)) [1 2 3]) @seen)"),
        "[1 2 3]"
    );
    assert_eq!(ps("(run! identity [1 2 3])"), "nil");
}

#[test]
fn doto_returns_x_after_threading_it_through_each_form() {
    assert_eq!(
        ps("(let [a (atom [])] (doto a (swap! conj 1) (swap! conj 2)) @a)"),
        "[1 2]"
    );
    // A bare-symbol form gets `x` appended as its sole argument.
    assert_eq!(ps("(doto 5 (+ 0))"), "5");
}

#[test]
fn distinct_predicate_arities() {
    assert_eq!(ps("(distinct? 1)"), "true");
    assert_eq!(ps("(distinct? 1 2)"), "true");
    assert_eq!(ps("(distinct? 1 1)"), "false");
    assert_eq!(ps("(distinct? 1 2 3)"), "true");
    assert_eq!(ps("(distinct? 1 2 1)"), "false");
}

#[test]
fn drop_last_default_and_n_arity_are_lazy() {
    assert_eq!(ps("(doall (drop-last [1 2 3 4 5]))"), "(1 2 3 4)");
    assert_eq!(ps("(doall (drop-last 2 [1 2 3 4 5]))"), "(1 2 3)");
    // Measured: a negative n clamps to 0 (whole coll unchanged), matching
    // real Clojure rather than erroring like mova's native `drop` would.
    assert_eq!(ps("(doall (drop-last -1 [1 2 3]))"), "(1 2 3)");
}

#[test]
fn drop_last_is_lazy_not_eager() {
    // Measured (matches mova's actual `map`-based implementation): asking
    // for the first result only walks as far as the 2nd source element
    // (one to yield, one to confirm a 2nd element exists to drop) --
    // element 3 and 4's side effects never run.
    assert_eq!(
        ps("(let [log (atom [])]
              (first (drop-last (map (fn [x] (swap! log conj x) x) [1 2 3 4])))
              @log)"),
        "[1 2]"
    );
}

#[test]
fn ffirst_fnext_nnext_on_nested_and_short_colls() {
    assert_eq!(ps("(ffirst [[1 2] [3 4]])"), "1");
    assert_eq!(ps("(ffirst nil)"), "nil");
    assert_eq!(ps("(fnext [1 2 3])"), "2");
    assert_eq!(ps("(fnext [1])"), "nil");
    assert_eq!(ps("(doall (nnext [1 2 3 4]))"), "(3 4)");
    assert_eq!(ps("(nnext [1])"), "nil");
    assert_eq!(ps("(nnext nil)"), "nil");
}

#[test]
fn splitv_at_first_half_is_a_vector_second_stays_a_seq() {
    assert_eq!(ps("(= (splitv-at 2 [1 2 3 4 5]) [[1 2] '(3 4 5)])"), "true");
    assert_eq!(ps("(vector? (first (splitv-at 2 [1 2 3 4 5])))"), "true");
    assert_eq!(ps("(= (splitv-at 10 [1 2 3]) [[1 2 3] '()])"), "true");
    // Measured: negative n clamps to 0, same native-take/drop-negative
    // workaround as drop-last above.
    assert_eq!(ps("(= (splitv-at -1 [1 2 3]) [[] '(1 2 3)])"), "true");
}

#[test]
fn partitionv_returns_vectors_and_drops_a_short_remainder() {
    // `doall` first: mova's `=` does not yet realize an UNREALIZED lazy
    // seq against a list (pre-existing divergence, tracked in
    // tests/conformance/pending/equality-hash.corpus -- real Clojure says
    // true without the doall). These tests pin partitionv's SHAPE, not
    // that separate equality gap.
    assert_eq!(ps("(= (doall (partitionv 2 [1 2 3 4 5])) '([1 2] [3 4]))"), "true");
    assert_eq!(ps("(vector? (first (partitionv 2 [1 2 3 4 5])))"), "true");
    assert_eq!(ps("(= (doall (partitionv 2 3 [1 2 3 4 5 6 7])) '([1 2] [4 5]))"), "true");
}

#[test]
fn lazy_cat_concatenates_and_stays_lazy_until_forced() {
    assert_eq!(ps("(doall (lazy-cat [1 2] [3 4]))"), "(1 2 3 4)");
    assert_eq!(ps("(doall (lazy-cat [1 2] [3 4] [5 6]))"), "(1 2 3 4 5 6)");
    assert_eq!(ps("(doall (lazy-cat [1 2]))"), "(1 2)");
    assert_eq!(ps("(lazy-cat)"), "()");
    // Side-effect timing: the second coll's side effect must not run
    // before the first coll is exhausted.
    assert_eq!(
        ps("(let [log (atom [])]
              (first (lazy-cat (do (swap! log conj :a) [1 2])
                                (do (swap! log conj :b) [3 4])))
              @log)"),
        "[:a]"
    );
}
