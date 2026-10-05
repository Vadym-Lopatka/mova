//! D5 (`proxy` generalized + the `clojure.pprint` support surface):
//! integration tests for `src/eval/types_forms.rs`'s `eval_proxy` -- the
//! implicit `this`, multi-arity method clauses, interface membership,
//! `IDeref`-through-`deref`, the `*out*` writer bridge, and the
//! `java.io.StringWriter` veneer -- plus the D5 pieces that exist only
//! because the vendored `clojure.pprint` needs them (`ref`/`dosync`/
//! `alter`, the longhand `.` interop form, special-form shadowing).
//!
//! Same `eval_ok`/`eval_err`/`ps` helper convention as `reify_test.rs`
//! and every other integration test file in this crate. Every assertion
//! mirrors a shape the vendored `clojure.pprint` sources actually
//! express -- see each section's own comment for which file.

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

// ==================== proxy: the implicit `this` ====================

// The defining difference from `reify`: a proxy method's parameter vector
// does NOT name the receiver, but the body may still refer to `this`.
// `pretty_writer.clj`'s `getf`/`setf` macros expand to `(~sym @@~'this)`
// and `(alter @~'this assoc ...)`, i.e. an unqualified captured `this`.
#[test]
fn proxy_binds_this_implicitly() {
    assert_eq!(
        ps("(let [p (proxy [java.util.List] [] (size [] 7))] (.size p))"),
        "7"
    );
    // `this` in the body IS the receiver: calling another of its own
    // methods through it must dispatch back into the same proxy.
    let src = "(let [p (proxy [java.util.List] [] \
               (size [] 3) \
               (isEmpty [] (= 0 (.size this))))] \
               [(.size p) (.isEmpty p)])";
    assert_eq!(ps(src), "[3 false]");
}

#[test]
fn proxy_method_params_come_after_this() {
    assert_eq!(
        ps("(let [p (proxy [java.util.List] [] (contains [o] (= :foo o)))] \
            [(.contains p :foo) (.contains p :bar)])"),
        "[true false]"
    );
}

// `column_writer.clj`'s `write` override is written in the multi-arity
// spelling -- `(write ([cbuf off len] ...) ([x] ...))` -- so BOTH arities
// have to gain the implicit `this`.
#[test]
fn proxy_multi_arity_method() {
    let src = "(let [p (proxy [java.io.Writer] [] \
               (write ([x] [:one x]) ([x off len] [:three x off len])))] \
               [(.write p :a) (.write p :a 1 2)])";
    assert_eq!(ps(src), "[[:one :a] [:three :a 1 2]]");
}

// Methods close over the lexical environment, exactly like `reify`'s.
#[test]
fn proxy_methods_close_over_the_call_site() {
    assert_eq!(
        ps("(let [n 41 p (proxy [java.util.List] [] (size [] (inc n)))] (.size p))"),
        "42"
    );
}

// ==================== proxy: heads and membership ====================

// Every head -- nominal superclass first, then interfaces -- is recorded
// the same way, so `instance?` answers for all of them.
#[test]
fn proxy_is_an_instance_of_every_head() {
    let src = "(let [p (proxy [java.io.Writer clojure.lang.IDeref] [] (deref [] :x))] \
               [(instance? java.io.Writer p) (instance? clojure.lang.IDeref p) \
                (instance? java.util.List p)])";
    assert_eq!(ps(src), "[true true false]");
}

// Ctor args are evaluated (left to right, for effect) and discarded --
// there is no superclass constructor to hand them to.
#[test]
fn proxy_ctor_args_are_evaluated_then_dropped() {
    let src = "(let [log (atom []) \
                     p (proxy [java.util.List] [(swap! log conj 1) (swap! log conj 2)] \
                         (size [] 0))] \
               [(.size p) @log])";
    assert_eq!(ps(src), "[0 [1 2]]");
}

#[test]
fn proxy_rejects_a_non_class_head() {
    let e = eval_err("(proxy [17] [] (size [] 0))");
    assert!(e.contains("proxy"), "unexpected error: {e:?}");
}

// The pre-D5 `[ThreadLocal]` keyhole still produces its hand-written host
// value (a real JVM class with real inherited behavior -- see
// `eval_proxy`'s doc for why it is not folded into the general path).
#[test]
fn proxy_threadlocal_keyhole_still_works() {
    assert_eq!(ps("(.get (proxy [ThreadLocal] [] (initialValue [] :seeded)))"), ":seeded");
}

// ==================== proxy: IDeref through `deref`/`@` ====================

// `clojure.pprint`'s writers keep all their state behind `(deref []
// fields)` and read it back as `@@this`.
#[test]
fn proxy_deref_routes_to_the_deref_method() {
    assert_eq!(
        ps("(let [p (proxy [clojure.lang.IDeref] [] (deref [] {:a 1}))] (:a @p))"),
        "1"
    );
    assert_eq!(
        ps("(let [p (proxy [clojure.lang.IDeref] [] (deref [] (atom {:a 2})))] (:a @@p))"),
        "2"
    );
}

#[test]
fn deref_of_an_instance_without_a_deref_method_errors() {
    let e = eval_err("@(proxy [java.util.List] [] (size [] 0))");
    assert!(e.contains("deref"), "unexpected error: {e:?}");
}

// ==================== the `*out*` writer bridge ====================

// `pprint` rebinds `*out*` to a writer proxy and then calls ordinary
// `print`/`pr`; those characters must arrive at the proxy's own `write`.
#[test]
fn print_writes_through_an_out_proxy() {
    let src = "(let [seen (atom []) \
                     w (proxy [java.io.Writer] [] (write [s] (swap! seen conj s)))] \
               (binding [*out* w] (print \"a\") (pr :b) (println \"c\")) \
               @seen)";
    assert_eq!(ps(src), "[\"a\" \":b\" \"c\\n\"]");
}

// The whole string arrives in ONE `write` call, never a char at a time --
// that is what real `clojure.core/pr` does on the JVM too.
#[test]
fn out_proxy_receives_whole_strings() {
    let src = "(let [n (atom 0) \
                     w (proxy [java.io.Writer] [] (write [_] (swap! n inc)))] \
               (binding [*out* w] (print \"hello world\")) \
               @n)";
    assert_eq!(ps(src), "1");
}

// A throw from inside the writer is NOT swallowed -- see `out_write`'s doc.
#[test]
fn an_out_proxy_throwing_propagates() {
    let e = eval_err(
        "(let [w (proxy [java.io.Writer] [] (write [_] (throw (ex-info \"boom\" {}))))] \
         (binding [*out* w] (print \"x\")))",
    );
    assert!(
        e.contains("boom") || e.contains("user exception"),
        "expected the writer's throw to propagate, got {e:?}"
    );
}

// An `Inst` with no `write` method is not a writer; `*out*` falls through
// to stdout rather than erroring.
#[test]
fn a_non_writer_inst_bound_to_out_is_not_an_error() {
    eval_ok("(binding [*out* (proxy [java.util.List] [] (size [] 0))] (print \"x\") :ok)");
}

// ==================== java.io.StringWriter ====================

#[test]
fn string_writer_accumulates_and_reads_back() {
    let src = "(let [sw (java.io.StringWriter.)] \
               (.write sw \"ab\") (.write sw 99) (.append sw \\d) (.flush sw) (.toString sw))";
    assert_eq!(ps(src), "\"abcd\"");
}

// The same object works as an `*out*` sink -- one accumulation, in call
// order, whichever way it was written to (`out_write`'s atom arm and
// `.write` are the same append).
#[test]
fn string_writer_is_an_out_sink() {
    let src = "(let [sw (java.io.StringWriter.)] \
               (.write sw \"[\") (binding [*out* sw] (print \"mid\")) (.write sw \"]\") \
               (.toString sw))";
    assert_eq!(ps(src), "\"[mid]\"");
}

// A StringWriter must NOT answer `instance?` for `clojure.lang.IDeref` --
// `clojure.pprint`'s `pretty-writer?` distinguishes its own proxies from
// a plain sink exactly that way, on the JVM too.
#[test]
fn string_writer_is_not_ideref() {
    assert_eq!(ps("(instance? clojure.lang.IDeref (java.io.StringWriter.))"), "false");
}

// ==================== the ref veneer ====================

// `clojure.pprint` keeps every writer's field map in a ref.
#[test]
fn refs_alter_and_ref_set() {
    assert_eq!(ps("(let [r (ref {:a 1})] (dosync (alter r assoc :b 2)) @r)"), "{:a 1, :b 2}");
    assert_eq!(ps("(let [r (ref 0)] (dosync (ref-set r 9)) @r)"), "9");
    assert_eq!(ps("(let [r (ref 1)] (dosync (commute r + 4)) @r)"), "5");
    assert_eq!(ps("(let [r (ref :v)] (dosync (ensure r)))"), ":v");
}

// `dosync` returns its body's last value, like `do`.
#[test]
fn dosync_returns_the_last_body_value() {
    assert_eq!(ps("(dosync 1 2 3)"), "3");
}

// ==================== the longhand `.` interop form ====================

#[test]
fn dot_special_form_instance_call() {
    assert_eq!(ps("(. \"abc\" toUpperCase)"), "\"ABC\"");
    assert_eq!(ps("(. \"abc\" (toUpperCase))"), "\"ABC\"");
    assert_eq!(ps("(. \"abcb\" indexOf \"b\")"), "1");
    assert_eq!(ps("(. \"abcb\" (indexOf \"b\"))"), "1");
}

// `(. Class (method args))` is a STATIC call -- `pprint_base.clj`'s
// `binding-map` macro is `(. clojure.lang.Var (pushThreadBindings ~amap))`.
#[test]
fn dot_special_form_static_call() {
    assert_eq!(ps("(. Character isDigit \\7)"), "true");
    assert_eq!(ps("(. Character (isDigit \\x))"), "false");
}

#[test]
fn dot_special_form_reaches_a_proxy_method() {
    assert_eq!(ps("(. (proxy [java.util.List] [] (size [] 5)) size)"), "5");
}

// ==================== special-form shadowing ====================

// `pprint.clj` does `(:refer-clojure :exclude (deftype))` and
// `pretty_writer.clj` then defines its own legacy `deftype` macro over
// `defstruct`; every `(deftype buffer-blob :data ...)` in that file must
// reach the macro, not mova's `deftype` special form.
#[test]
fn a_namespace_may_shadow_a_macro_shaped_special_form() {
    let src = "(defmacro deftype [n & fields] (list 'def n (vec (map keyword fields)))) \
               (deftype tagged a b) \
               tagged";
    assert_eq!(ps(src), "[:a :b]");
}

// A shadow is namespace-scoped: another namespace still sees the special
// form.
#[test]
fn shadowing_a_special_form_does_not_leak_across_namespaces() {
    let src = "(ns one) \
               (defmacro deftype [n & fields] (list 'def n :shadowed)) \
               (ns two) \
               (deftype Rec [a b]) \
               (.-a (Rec. 1 2))";
    assert_eq!(ps(src), "1");
}

// Clojure's TRUE special forms cannot be shadowed on any platform, so
// they are not shadowable here either.
#[test]
fn a_true_special_form_is_not_shadowable() {
    // `if` stays `if` even with a same-named def in scope.
    assert_eq!(ps("(def if :nope) (if true :yes :no)"), ":yes");
}

// ==================== lookaround regexes ====================

// A `java.util.regex.Pattern` supports lookaround; before D5 mova's
// reader rejected such a literal outright. `cl_format.clj:1621` is the
// measured case.
#[test]
fn lookahead_patterns_compile_and_match() {
    // Three capture groups (the outer alternation, `('.)`, `([+-]?\d+)`)
    // -- a `(?=,)` lookahead captures nothing.
    assert_eq!(ps(r#"(re-find #"^([vV]|#|('.)|([+-]?\d+)|(?=,))" "12,")"#), "[\"12\" \"12\" nil \"12\"]");
    assert_eq!(ps(r#"(re-find #"foo(?=bar)" "foobar")"#), "\"foo\"");
    assert_eq!(ps(r#"(re-find #"foo(?=bar)" "foobaz")"#), "nil");
    assert_eq!(ps(r#"(re-find #"(?<=a)b" "ab")"#), "\"b\"");
}

// Ordinary patterns keep their exact previous behavior.
#[test]
fn plain_patterns_are_unchanged() {
    assert_eq!(ps(r#"(re-find #"\d+" "abc123")"#), "\"123\"");
    assert_eq!(ps(r#"(re-matches #"(a)(b)" "ab")"#), "[\"ab\" \"a\" \"b\"]");
    assert_eq!(ps(r#"(re-seq #"\d" "a1b2")"#), "(\"1\" \"2\")");
    assert_eq!(ps(r#"(clojure.string/split "a1b2c" #"\d")"#), "[\"a\" \"b\" \"c\"]");
    assert_eq!(ps(r#"(clojure.string/replace "a1b" #"\d" "X")"#), "\"aXb\"");
}

// ==================== matcher .start/.end ====================

#[test]
fn matcher_start_and_end() {
    let src = "(let [m (re-matcher #\"b+\" \"abbc\")] [(re-find m) (.start m) (.end m)])";
    assert_eq!(ps(src), "[\"bb\" 1 3]");
}

#[test]
fn matcher_start_before_any_find_errors() {
    let e = eval_err("(.end (re-matcher #\"a\" \"a\"))");
    assert!(e.contains("No match found"), "unexpected error: {e:?}");
}

// ==================== identical? on the rest of the enum ====================

// `(identical? x x)` was FALSE for every `Arc`-backed variant this fn had
// no arm for -- the bug that made `clojure.pprint` break lines in the
// wrong places (`pretty_writer.clj`'s `ancestor?` walks a chain of
// `defstruct` blocks with `identical?`).
#[test]
fn identical_is_true_for_a_value_and_itself() {
    let src = "(defstruct s :parent :x) \
               (let [a (struct s nil 1) b (struct s a 2)] \
                 [(identical? a a) (identical? a (:parent b)) (identical? a (struct s nil 1))])";
    assert_eq!(ps(src), "[true true false]");
    assert_eq!(ps("(let [q (conj clojure.lang.PersistentQueue/EMPTY 1)] (identical? q q))"), "true");
    assert_eq!(ps("(let [m (sorted-map :a 1)] (identical? m m))"), "true");
    assert_eq!(ps("(let [v (volatile! 1)] (identical? v v))"), "true");
    assert_eq!(ps("(identical? #'map #'map)"), "true");
}

// ==================== format width/justification ====================

// `print_table.clj` builds its column formats as `(str "%" width "s")`.
#[test]
fn format_supports_minimum_width_and_left_justify() {
    assert_eq!(ps(r#"(format "|%5s|" "ab")"#), "\"|   ab|\"");
    assert_eq!(ps(r#"(format "|%-5s|" "ab")"#), "\"|ab   |\"");
    assert_eq!(ps(r#"(format "|%3d|" 7)"#), "\"|  7|\"");
    assert_eq!(ps(r#"(format "|%2s|" "abcd")"#), "\"|abcd|\"");
    assert_eq!(ps(r#"(format "%s%%" 1)"#), "\"1%\"");
}

// ==================== *print-namespace-maps* ====================

#[test]
fn print_namespace_maps_lifts_a_single_shared_namespace() {
    let on = |m: &str| format!("(binding [*print-namespace-maps* true] (pr-str {m}))");
    assert_eq!(ps(&on("{:user/a 1}")), "\"#:user{:a 1}\"");
    assert_eq!(ps(&on("{:user/a 1, :user/b 2}")), "\"#:user{:a 1, :b 2}\"");
    // Mixed keyword/symbol keys lift together when the namespace agrees.
    assert_eq!(ps(&on("{:user/a 1, 'user/b 2}")), "\"#:user{:a 1, b 2}\"");
    // Not liftable: two namespaces, an unqualified key, a non-ident key,
    // or an empty map.
    // W4D-TIERS: the three multi-key expectations below were stale sorted-
    // key order, from before a W4 printer fix made small-map INSERTION
    // order visible (the comment they replace documented that sorting as
    // a "pre-existing deviation from Clojure's insertion order" -- it no
    // longer is one; mova now prints these in the same insertion order
    // the map literal was built in, matching Clojure).
    assert_eq!(ps(&on("{:user/a 1, :foo/b 2}")), "\"{:user/a 1, :foo/b 2}\"");
    assert_eq!(ps(&on("{:user/a 1, :b 2}")), "\"{:user/a 1, :b 2}\"");
    assert_eq!(ps(&on("{:user/a 1, 100 200}")), "\"{:user/a 1, 100 200}\"");
    assert_eq!(ps(&on("{}")), "\"{}\"");
}

// On by default (as in Clojure 1.13); bound off nothing lifts.
#[test]
fn print_namespace_maps_defaults_off() {
    assert_eq!(ps("(pr-str {:user/a 1})"), "\"#:user{:a 1}\"");
    assert_eq!(
        ps("(binding [*print-namespace-maps* false] (pr-str {:user/a 1}))"),
        "\"{:user/a 1}\""
    );
}

// ==================== load ====================

// `(load "...")` with no such file is a clear error naming the path it
// looked for; the successful path is exercised end to end by the
// vendored `clojure.pprint` in the clojure-suite run.
#[test]
fn load_reports_a_missing_path() {
    let e = eval_err("(load \"no/such/thing\")");
    assert!(e.contains("no/such/thing"), "unexpected error: {e:?}");
}

// ==================== alter-var-root / find-var ====================

#[test]
fn alter_var_root_sets_the_root() {
    assert_eq!(ps("(def x 1) (alter-var-root #'x inc) x"), "2");
    assert_eq!(ps("(def x 1) (alter-var-root #'x + 10 100)"), "111");
}

#[test]
fn find_var_needs_a_qualified_symbol() {
    assert_eq!(ps("(= #'clojure.core/map (find-var 'clojure.core/map))"), "true");
    assert_eq!(ps("(find-var 'clojure.core/definitely-not-a-var)"), "nil");
    let e = eval_err("(find-var 'map)");
    assert!(e.contains("not fully qualified"), "unexpected error: {e:?}");
}

// ==================== .addMethod ====================

// `dispatch.clj`'s `use-method` installs a multimethod method under a
// computed dispatch value, which `defmethod`'s literal syntax cannot do.
#[test]
fn multifn_add_method() {
    let src = "(defmulti m class) \
               (.addMethod m java.lang.Long (fn [_] :long)) \
               (. m addMethod java.lang.String (fn [_] :string)) \
               [(m 1) (m \"s\")]";
    assert_eq!(ps(src), "[:long :string]");
}

// ==================== isa? over the collection interfaces ====================

// `defmulti`'s class dispatch has to connect a concrete map/seq class to
// the interface `use-method` registered under.
#[test]
fn collection_classes_isa_their_interfaces() {
    assert_eq!(ps("(isa? (class {:a 1}) clojure.lang.IPersistentMap)"), "true");
    assert_eq!(ps("(isa? (class (sorted-map :a 1)) clojure.lang.IPersistentMap)"), "true");
    assert_eq!(ps("(isa? (class '(1)) clojure.lang.ISeq)"), "true");
    assert_eq!(ps("(isa? (class (map inc [1])) clojure.lang.ISeq)"), "true");
    assert_eq!(ps("(isa? (class [1]) clojure.lang.IPersistentVector)"), "true");
    assert_eq!(ps("(isa? (class [1]) clojure.lang.IPersistentMap)"), "false");
}
