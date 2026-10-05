//! Integration tests for A3's de-toying work: the destructuring engine
//! (`eval::special_forms::bind_pattern`, wired into `let`/`loop`/fn params),
//! `gensym` + auto-gensym syntax-quote hygiene, and the `core/core.mova`
//! additions built on top of them (`doseq`, `for`, `case`, `condp`, `while`,
//! `letfn`, `juxt`/`partial`/`comp`/`fnil`/`max-key`/`min-key`, `if-let`/
//! `when-let` patterns, `defn` docstrings).

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

// -------------------- sequential destructuring --------------------

#[test]
fn sequential_basic_binding() {
    assert_eq!(ps("(let [[a b] [1 2]] (+ a b))"), "3");
}

#[test]
fn sequential_nested_pattern() {
    assert_eq!(ps("(let [[[a b] c] [[1 2] 3]] [a b c])"), "[1 2 3]");
}

#[test]
fn sequential_rest_binding() {
    assert_eq!(ps("(let [[a & r] [1 2 3]] [a r])"), "[1 (2 3)]");
}

#[test]
fn sequential_rest_is_nil_when_empty() {
    assert_eq!(ps("(let [[a & r] [1]] [a r])"), "[1 nil]");
}

#[test]
fn sequential_as_binds_original_value() {
    assert_eq!(ps("(let [[a b :as all] [1 2 3]] [a b all])"), "[1 2 [1 2 3]]");
}

#[test]
fn sequential_missing_positions_bind_nil() {
    assert_eq!(ps("(let [[a b c] [1]] [a b c])"), "[1 nil nil]");
}

#[test]
fn sequential_destructure_of_string() {
    assert_eq!(ps("(let [[a b] \"xy\"] (= a \\x))"), "true");
    assert_eq!(ps("(let [[a b] \"xy\"] (str a b))"), "\"xy\"");
}

#[test]
fn sequential_destructure_of_lazy_seq() {
    assert_eq!(ps("(let [[a b c] (map inc (range 3))] [a b c])"), "[1 2 3]");
}

#[test]
fn sequential_destructure_of_nil_binds_all_nil() {
    assert_eq!(ps("(let [[a b] nil] [a b])"), "[nil nil]");
}

// -------------------- map destructuring --------------------

#[test]
fn map_keys_basic() {
    assert_eq!(ps("(let [{:keys [a b]} {:a 1 :b 2}] (+ a b))"), "3");
}

#[test]
fn map_strs_basic() {
    assert_eq!(ps("(let [{:strs [x y]} {\"x\" 1 \"y\" 2}] (+ x y))"), "3");
}

#[test]
fn map_or_default_only_applies_when_key_missing() {
    // Present-but-nil must NOT trigger :or -- only a genuinely missing key.
    assert_eq!(ps("(let [{:keys [a] :or {a :dflt}} {:a nil}] a)"), "nil");
    assert_eq!(ps("(let [{:keys [a] :or {a :dflt}} {}] a)"), ":dflt");
}

#[test]
fn map_as_binds_original_map() {
    assert_eq!(ps("(let [{:keys [a] :as m} {:a 1 :b 2}] [a m])"), "[1 {:a 1, :b 2}]");
}

#[test]
fn map_general_binding_and_nested_pattern_key() {
    assert_eq!(
        ps("(let [{a :a [x y] :point} {:a 1 :point [10 20]}] [a x y])"),
        "[1 10 20]"
    );
}

#[test]
fn map_destructure_of_nil_binds_all_missing() {
    assert_eq!(ps("(let [{:keys [a b] :or {b 99}} nil] [a b])"), "[nil 99]");
}

#[test]
fn map_malformed_or_value_errors() {
    let msg = eval_err("(let [{:keys [a] :or [a 1]} {}] a)");
    assert!(msg.contains(":or"), "message was: {msg}");
}

// -------------------- fn param destructuring --------------------

#[test]
fn fn_param_options_map_idiom_with_defaults() {
    assert_eq!(
        ps("(defn greet [{:keys [name greeting] :or {greeting \"Hello\"}}] (str greeting \", \" name \"!\")) (greet {:name \"World\"})"),
        "\"Hello, World!\""
    );
}

#[test]
fn fn_param_sequential_destructure() {
    assert_eq!(ps("(defn f [[a b]] (+ a b)) (f [1 2])"), "3");
}

#[test]
fn fn_variadic_destructure() {
    assert_eq!(ps("(defn f [& [a b]] [a b]) (f 1 2 3)"), "[1 2]");
}

// -------------------- let/loop patterns + recur rebinding --------------------

#[test]
fn loop_plain_pattern_still_works() {
    assert_eq!(ps("(loop [n 5 acc 1] (if (zero? n) acc (recur (dec n) (* acc n))))"), "120");
}

#[test]
fn loop_recur_rebinds_pattern_each_iteration() {
    // Fibonacci via a rebound [a b] pair each recur -- exercises the
    // internal __loopN indirection plus per-iteration re-destructuring.
    assert_eq!(
        ps("(loop [[a b] [0 1] n 5] (if (zero? n) a (recur [b (+ a b)] (dec n))))"),
        "5"
    );
}

// -------------------- doseq / for --------------------

#[test]
fn doseq_with_let_and_when_is_eager_and_returns_nil() {
    assert_eq!(
        ps("(let [acc (atom [])] (doseq [x (range 5) :let [y (* x x)] :when (even? y)] (swap! acc conj y)) @acc)"),
        "[0 4 16]"
    );
}

#[test]
fn doseq_destructures_each_element() {
    assert_eq!(
        ps("(let [acc (atom [])] (doseq [[a b] [[1 2] [3 4]]] (swap! acc conj (+ a b))) @acc)"),
        "[3 7]"
    );
}

// S7: `for` became a genuinely LAZY comprehension (previously an
// approved-but-documented eager deviation -- retired because eagerness
// made a correct `:while` fix undoable without a catastrophic-runtime
// bug, see `core/core.mova`'s `for` doc comment). `ps`'s `Display` calls
// `printer::pr_str` directly with no `&mut Interp` to force an unrealized
// lazy seq through (mirrors real Clojure: raw printing never forces an
// unbounded lazy seq -- only `println`/`pr-str`/`str`, mova's explicit
// *forcing* builtins, do that here too), so these wrap in `doall` to get
// a real, fully-realized `Value::List` back before comparing -- the
// SEQUENCE OF ELEMENTS these two tests exist to check is unaffected by
// laziness either way.
#[test]
fn for_basic_and_when() {
    assert_eq!(ps("(doall (for [x (range 5) :when (even? x)] x))"), "(0 2 4)");
}

#[test]
fn for_multiple_bindings_cross_product() {
    assert_eq!(ps("(doall (for [x [1 2] y [10 20]] (+ x y)))"), "(11 21 12 22)");
}

// -------------------- case / condp --------------------

#[test]
fn case_list_clause_and_default() {
    assert_eq!(
        ps("(defn describe [n] (case n (1 2 3) :small 10 :ten :other)) [(describe 2) (describe 10) (describe 99)]"),
        "[:small :ten :other]"
    );
}

#[test]
fn case_no_default_no_match_errors() {
    // `throw`'s `RjError::message` is always the fixed "user exception";
    // the actual thrown payload lives in `RjError::thrown` -- not part of
    // `embed::Error`'s surface, so this one test reaches through
    // `mova::internal` rather than the facade.
    //
    // Wave-C small sweep item 3: `case`'s no-match throw used to be a bare
    // `Value::Str`, so `display_str` on it WAS the message text verbatim.
    // It's now a real `(IllegalArgumentException. "No matching clause: 5")`
    // instance (`Value::Inst`, matching the oracle's own exception type --
    // see `core/core.mova`'s `case-step`), and `display_str`/`pr_str` on an
    // exception `Inst` print the generic `#object[...]` shape (same as
    // every other non-record `Inst`, no message text embedded) rather than
    // Java's own `Throwable.toString()` -- so this test now reaches into
    // the `Inst`'s one basis field directly (see `hostclass::mk_exception`'s
    // doc: a single field literally named `"getMessage"`) instead of
    // `display_str`-matching the whole thrown value.
    use mova::internal::Interp;
    let mut interp = Interp::new();
    let err = interp
        .eval_str("test", "(case 5 1 :one 2 :two)")
        .expect_err("expected case to throw on no match");
    let thrown = err.thrown.expect("thrown value present");
    let mova::internal::Value::Inst(inst) = &thrown else {
        panic!("thrown was not an exception instance: {thrown:?}");
    };
    assert_eq!(inst.tdef.name.as_ref(), "java.lang.IllegalArgumentException");
    let fields = inst.fields.lock().unwrap();
    let msg = fields.first().expect("exception has a message field");
    let mova::internal::Value::Str(s) = msg else {
        panic!("message field was not a string: {msg:?}");
    };
    assert!(s.to_string().contains("No matching clause"), "message was: {s}");
}

#[test]
fn condp_basic_and_default() {
    assert_eq!(ps("(condp = 2 1 :one 2 :two :dflt)"), ":two");
    assert_eq!(ps("(condp = 99 1 :one 2 :two :dflt)"), ":dflt");
}

// -------------------- while / letfn --------------------

#[test]
fn while_loops_until_false() {
    assert_eq!(
        ps("(let [a (atom 0)] (while (< @a 5) (swap! a inc)) @a)"),
        "5"
    );
}

#[test]
fn letfn_supports_mutual_recursion() {
    assert_eq!(
        ps("(letfn [(even2? [n] (if (zero? n) true (odd2? (dec n)))) (odd2? [n] (if (zero? n) false (even2? (dec n))))] (even2? 10))"),
        "true"
    );
}

// -------------------- higher-order utilities --------------------

#[test]
fn comp_composes_right_to_left() {
    assert_eq!(ps("((comp inc (fn [x] (* x 2))) 5)"), "11");
}

#[test]
fn partial_fixes_leading_args() {
    assert_eq!(ps("((partial + 1 2) 3 4)"), "10");
}

#[test]
fn juxt_collects_results_into_vector() {
    assert_eq!(ps("((juxt inc dec) 5)"), "[6 4]");
}

#[test]
fn fnil_substitutes_for_nil_args() {
    assert_eq!(ps("((fnil + 0) nil 5)"), "5");
}

#[test]
fn max_key_and_min_key() {
    assert_eq!(ps("(max-key count [1] [1 2 3] [1 2])"), "[1 2 3]");
    assert_eq!(ps("(min-key count [1] [1 2 3] [1 2])"), "[1]");
}

#[test]
fn not_empty_returns_nil_for_empty_coll() {
    assert_eq!(ps("(not-empty [])"), "nil");
    assert_eq!(ps("(not-empty [1])"), "[1]");
}

// -------------------- if-let / when-let with patterns --------------------

#[test]
fn if_let_with_destructuring_pattern() {
    assert_eq!(ps("(if-let [[a b] [1 2]] (+ a b) :none)"), "3");
    assert_eq!(ps("(if-let [[a b] nil] (+ a b) :none)"), ":none");
}

#[test]
fn when_let_with_map_pattern() {
    assert_eq!(ps("(when-let [{:keys [a]} {:a 42}] a)"), "42");
}

// -------------------- gensym / auto-gensym --------------------

#[test]
fn gensym_produces_unique_symbols() {
    assert_eq!(ps("(= (gensym) (gensym))"), "false");
}

#[test]
fn gensym_with_prefix() {
    assert_eq!(ps("(clojure.string/starts-with? (name (gensym \"foo\")) \"foo\")"), "true");
}

#[test]
fn auto_gensym_same_symbol_within_one_syntax_quote() {
    // If `x#` resolved to two DIFFERENT generated symbols, the second `x#`
    // in `(+ x# x#)` would be an unresolved-symbol error, not 2.
    assert_eq!(
        ps("(defmacro twice [] `(let [x# 1] (+ x# x#))) (twice)"),
        "2"
    );
}

#[test]
fn auto_gensym_differs_across_separate_expansions() {
    assert_eq!(
        ps("(defmacro gs [] `(quote x#)) (not= (gs) (gs))"),
        "true"
    );
}

// -------------------- defn docstring tolerance --------------------

#[test]
fn defn_docstring_is_accepted_and_discarded() {
    assert_eq!(ps("(defn add \"adds two numbers\" [a b] (+ a b)) (add 2 3)"), "5");
}

#[test]
fn defn_docstring_multi_arity() {
    assert_eq!(
        ps("(defn addn \"adds\" ([a] a) ([a b] (+ a b))) [(addn 5) (addn 2 3)]"),
        "[5 5]"
    );
}

// -------------------- §5/M2: Clojure 1.13.0-alpha6 destructuring --------------------
//
// `req!`, `:keys!`/`:strs!`/`:syms!` (required keys), `:foo/keys`/`:syms`
// (namespaced/symbol-keyed), `&` inside a directive vector, `:select`/
// `:all`/`:defaults`, and the `:or`-validation errors those trigger.
// compat/c12-destructure ported real Clojure's `destmap*`/`push1`
// ALGORITHM (`.oracle/clojure-src/src/clj/clojure/core.clj`) into
// `eval::special_forms::bind_map_pattern`, not its macroexpansion
// strategy. Every row below is oracle-measured: `OK` rows are the exact
// live-1.13.0-alpha6 `pr-str` result (see `compat/destructuring-113.golden`
// and `compat/notes-destructuring.md`'s per-row table, both regenerated
// together by `compat/destructuring-113-probe.clj`); `ERR` rows check the
// message text mova's own (untyped, no exception-taxonomy) `RjError`
// carries, matched as closely as that error type allows -- see
// `CLOJURE-COMPAT-PLAN.md` §5 for the seven verbatim compile-time error
// strings this ports.

#[test]
fn req_bang_returns_value_or_throws_on_missing() {
    // compat/destructuring-113.corpus's own `test-req!` ground truth,
    // also independently in the vendored suite (data_structures.clj).
    assert_eq!(ps("(let [m {:a 1, :b 2, :f nil, :g false, nil \"nil\"}] (req! m :a))"), "1");
    assert_eq!(ps("(let [m {:a 1, :b 2, :f nil, :g false, nil \"nil\"}] (req! m nil))"), "\"nil\"");
    assert_eq!(ps("(let [m {:a 1, :b 2, :f nil, :g false, nil \"nil\"}] (req! m :f))"), "nil");
    let msg = eval_err("(req! {:a 1} :missing)");
    assert!(msg.contains("Missing required key: :missing"), "message was: {msg}");
}

#[test]
fn seq_to_map_for_destructuring_matches_destructuring_coercion() {
    // 1.11 helper backing the same seq/singleton/trailing-map coercion
    // `bind_map_pattern` runs on a fn's `& {:keys [...]}` rest arg.
    assert_eq!(ps("(seq-to-map-for-destructuring (list :a 1 :b 2))"), "{:a 1, :b 2}");
    assert_eq!(ps("(seq-to-map-for-destructuring (list {:a 1}))"), "{:a 1}");
    assert_eq!(ps("(seq-to-map-for-destructuring (list))"), "{}");
}

#[test]
fn keys_bang_required_happy_path_and_throw() {
    assert_eq!(ps("(let [{:keys! [a]} {:a 1}] a)"), "1");
    let msg = eval_err("(let [{:keys! [a]} {:b 1}] a)");
    assert!(msg.contains("Missing required key: :a"), "message was: {msg}");
}

#[test]
fn strs_bang_required_key_message_is_pr_str_quoted() {
    // reqmsg's own asymmetry: a STRING key prints quoted.
    let msg = eval_err("(let [{:strs! [a]} {}] a)");
    assert!(msg.contains("Missing required key: \"a\""), "message was: {msg}");
}

#[test]
fn syms_bang_required_key_message_is_bare() {
    let msg = eval_err("(let [{:syms! [a]} {}] a)");
    assert!(msg.contains("Missing required key: a"), "message was: {msg}");
}

#[test]
fn foo_keys_bang_namespaced_required_key_message() {
    let msg = eval_err("(let [{:foo/keys! [a]} {}] a)");
    assert!(msg.contains("Missing required key: :foo/a"), "message was: {msg}");
}

#[test]
fn keys_bang_default_for_required_key_is_a_compile_time_style_error() {
    let msg = eval_err("(let [{:keys! [a] :or {a 1}} {}] a)");
    assert!(
        msg.contains("Can't supply default value for required key: :a"),
        "message was: {msg}"
    );
}

#[test]
fn amp_inside_keys_vector_declares_without_binding() {
    // Keys after `&` are declared (feed `:select`) but never bound as
    // locals -- and, for `:keys!` (not `:keys`), STILL required-checked.
    assert_eq!(
        ps("(let [{:keys [a b & :c :d] :select sel} {:a 1 :b 2 :c 3 :d 4}] [a b (into (sorted-map) sel)])"),
        "[1 2 {:a 1, :b 2, :c 3, :d 4}]"
    );
    assert_eq!(ps("(let [{:keys! [a & :b :c]} {:a 1 :b 2 :c 3}] a)"), "1");
    let msg = eval_err("(let [{:keys! [a & :b :c]} {:a 1 :c 3}] a)");
    assert!(msg.contains("Missing required key: :b"), "message was: {msg}");
}

#[test]
fn amp_can_only_appear_once_in_a_directive() {
    let msg = eval_err("(let [{:keys [a & :b & :c]} {:a 1}] a)");
    assert!(msg.contains("& can only appear once in :keys"), "message was: {msg}");
}

#[test]
fn symbols_after_amp_are_rejected() {
    let msg = eval_err("(let [{:keys [a b & c d]} {:a 1 :b 2 :c 3 :d 4}] [a b])");
    assert!(
        msg.contains("binding symbols can only appear before '&', use keys after"),
        "message was: {msg}"
    );
}

#[test]
fn select_directive_binds_declared_keys_map() {
    assert_eq!(ps("(let [{:select sel} {:a 1 :b 2}] sel)"), "{}");
    assert_eq!(ps("(let [{:keys [a] :select sel} {:a 1 :b 2}] sel)"), "{:a 1}");
}

#[test]
fn all_directive_binds_every_key() {
    assert_eq!(ps("(let [{:all m} {:a 1 :b 2}] (into (sorted-map) m))"), "{:a 1, :b 2}");
}

#[test]
fn defaults_directive_binds_the_or_map_and_requires_or() {
    assert_eq!(
        ps("(let [{:keys [a] :or {a 5} :defaults d} {}] [a (into (sorted-map) d)])"),
        "[5 {:a 5}]"
    );
    let msg = eval_err("(let [{:keys [a] :defaults d} {}] [a d])");
    assert!(msg.contains("Can't specify :defaults without :or"), "message was: {msg}");
    // `:or {}` (present but EMPTY) is still truthy -- only a genuinely
    // ABSENT `:or` triggers the error above.
    assert_eq!(ps("(empty? (let [{:defaults d :or {}} {}] d))"), "true");
}

#[test]
fn or_validation_only_active_alongside_select_all_defaults() {
    // Back-compat: a dangling `:or` key with none of :select/:all/:defaults
    // present stays silently ignored.
    assert_eq!(ps("(let [{:keys [a] :or {q 1}} {}] a)"), "nil");
    let msg = eval_err("(let [{:keys [a] :or {a 1 q 2} :select sel} {}] [a sel])");
    assert!(
        msg.contains("symbol q in :or does not refer to a binding"),
        "message was: {msg}"
    );
    let msg2 = eval_err("(let [{:keys [a] :or {a 1 :extra 2} :select sel} {}] [a sel])");
    assert!(msg2.contains("appear only in :or"), "message was: {msg2}");
    let msg3 = eval_err("(let [{x :a :or {x 1 :a 2}} {}] x)");
    assert!(
        msg3.contains("Multiple :or defaults for same key: :a 'x'"),
        "message was: {msg3}"
    );
}

#[test]
fn nested_select_all_propagate_from_a_submap_pattern() {
    // Nested :select FILTERS the parent's view of the sub-map (only the
    // declared keys survive); nested :all passes it through raw.
    assert_eq!(
        ps("(let [{{:keys [x] :select psel} :point :select sel} {:point {:x 1 :y 2}}] [(into (sorted-map) sel) psel])"),
        "[{:point {:x 1}} {:x 1}]"
    );
    assert_eq!(
        ps("(let [{{:keys [x] :all pall} :point :all sall} {:point {:x 1 :y 2}}] [(into (sorted-map) sall) pall])"),
        "[{:point {:x 1, :y 2}} {:x 1, :y 2}]"
    );
}

#[test]
fn required_keys_defer_validation_to_call_time_not_defn_time() {
    // Neither real Clojure nor mova validates a `:keys!` pattern at `defn`
    // time -- only when the fn is actually CALLED (§5: "required -- missing
    // key throws at runtime").
    assert_eq!(ps("(defn req-f [{:keys! [a]}] a) (req-f {:a 5})"), "5");
    let msg = eval_err("(defn req-f2 [{:keys! [a]}] a) (req-f2 {})");
    assert!(msg.contains("Missing required key: :a"), "message was: {msg}");
}

#[test]
fn classic_namespaced_keys_and_syms_directives() {
    // Pre-1.13 namespaced-key forms mova got wrong before this port (S9).
    assert_eq!(ps("(let [{:keys [foo/a]} {:foo/a 1}] a)"), "1");
    assert_eq!(ps("(let [{:foo/keys [a b]} {:foo/a 1 :foo/b 2}] [a b])"), "[1 2]");
    assert_eq!(ps("(let [{:syms [a b]} {'a 1 'b 2}] [a b])"), "[1 2]");
}

#[test]
fn general_entry_key_is_an_evaluated_expression_not_a_literal() {
    // `{a 'b}`'s key form is `(quote b)` -- real Clojure splices it into
    // generated code and evaluates it once that code runs, so the actual
    // lookup key is the SYMBOL `b`, not the literal list `(quote b)`.
    // Oracle-confirmed live; a bare var/local reference in key position is
    // evaluated the same way.
    assert_eq!(ps("(let [{a 'b} {'b 5}] a)"), "5");
    assert_eq!(ps("(def bvar :the-key) (let [{a bvar} {:the-key 7}] a)"), "7");
}

#[test]
fn symbol_as_fn_backs_the_syms_directive_family() {
    // §5 exposed this general (non-destructuring-specific) gap: symbols
    // implement IFn exactly like keywords, `(sym coll not-found)`.
    assert_eq!(ps("('b {'b 1 'c 2})"), "1");
    assert_eq!(ps("('b {} :default)"), ":default");
    assert_eq!(ps("('b nil)"), "nil");
}
