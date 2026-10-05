//! SPEC-W5 (`docs/SPEC-ALPHA-CAMPAIGN.md`, wave 5): `clojure.spec.test.alpha`
//! -- the port, the `callstack*` veneer it is built on, and the
//! `clojure.core/defn` `:arglists` metadata `instrument` reads.
//!
//! The oracle-diffed acceptance instrument is
//! `tests/spec-smoke/stest-smoke.mova` (125 printed lines, byte-identical to
//! Clojure 1.13.0-alpha6 + test.check 1.1.1). This file is the `cargo test`
//! -resident half, so a regression fails the build instead of waiting for a
//! manual corpus run -- and it pins the things the corpus deliberately
//! cannot compare, above all the exact `::stest/caller` `:line` (see
//! `stest_caller_line_is_the_defn_head`).
//!
//! Every case runs through BOTH tiers -- compiled and tree-walked
//! (`Interp::with_compile_enabled(false)`) -- like `tests/ns_test.rs` and
//! `tests/spec_engine_test.rs`. That is not ceremony here: `callstack*`
//! reads `Interp::stack`, which `eval::apply`'s THREE frame-push sites
//! maintain, and the compiled and tree-walked bodies reach different ones.

use mova::internal::Interp;

/// Same reason as `ns_test.rs`'s `NS_TEST_STACK_SIZE`: a `cargo test`
/// thread's default stack cannot hold spec's (and test.check's) recursion
/// in a debug build.
const STEST_STACK_SIZE: usize = 64 * 1024 * 1024;

fn on_big_stack(f: fn()) {
    std::thread::Builder::new()
        .stack_size(STEST_STACK_SIZE)
        .spawn(f)
        .expect("failed to spawn the spec-test-alpha worker thread")
        .join()
        .unwrap_or_else(|payload| std::panic::resume_unwind(payload));
}

/// Evaluates `src` in a fresh interpreter of each tier with an EMPTY module
/// path (everything must come out of `crate::stdlib`), asserts the two
/// agree, and returns the shared `pr-str`.
fn ev(src: &str) -> String {
    fn one(mut interp: Interp, src: &str) -> String {
        interp.module_paths = vec![];
        match interp.eval_str("spec-test-alpha-test", src) {
            Ok(v) => match interp.realize_deep(&v) {
                Ok(r) => mova::internal::pr_str(&r),
                Err(e) => format!("ERR: {}", e.message),
            },
            Err(e) => format!("ERR: {}", e.message),
        }
    }
    let compiled = one(Interp::new(), src);
    let walked = one(Interp::with_compile_enabled(false), src);
    assert_eq!(compiled, walked, "tier divergence for: {src}");
    compiled
}

fn is(src: &str, expected: &str) {
    assert_eq!(ev(src), expected, "for: {src}");
}

// ---------------------------------------------------------------------------
// `callstack*` -- the one engine capability this wave owns
// ---------------------------------------------------------------------------

/// The shape contract: innermost frame FIRST, one 4-element vector per live
/// frame, in `clojure.core/StackTraceElement->vec`'s own `[class method file
/// line]` order, with `class` spelled `<defining-ns>$<fn-name>` and `method`
/// always `invoke` (which is what makes upstream's `(contains? '#{invoke
/// invokeStatic} method)` Clojure-frame test true).
#[test]
fn callstack_reports_frames_innermost_first_as_ns_dollar_name() {
    is(
        "(defn deepest [] (callstack*))\n\
         (defn mid [] (deepest))\n\
         (defn top [] (mid))\n\
         (mapv (fn [e] [(first e) (second e)]) (top))",
        "[[user$deepest invoke] [user$mid invoke] [user$top invoke]]",
    );
}

/// A frame's file is the source buffer name and its line is a real line in
/// it; an anonymous fn is named `anonymous-fn`, the same name a rendered
/// mova stack trace already gives it.
#[test]
fn callstack_names_anonymous_frames_and_carries_file_and_line() {
    is(
        "(defn probe [] (callstack*))\n\
         (defn run [f] (f))\n\
         (let [cs (run (fn [] (probe)))]\n\
        \x20 [(mapv first cs)\n\
        \x20  (every? (fn [e] (= \"spec-test-alpha-test\" (nth e 2))) cs)\n\
        \x20  (every? (fn [e] (pos-int? (nth e 3))) cs)])",
        "[[user$probe user$anonymous-fn user$run] true true]",
    );
}

/// A frame's namespace is the DEFINING namespace of the closure, not the
/// namespace that called it -- which is the whole reason `Frame` had to
/// grow an `ns` field rather than the caller reading `Interp::current_ns`.
#[test]
fn callstack_reports_the_defining_namespace_of_each_frame() {
    is(
        "(ns lib.a)\n\
         (defn from-a [] (callstack*))\n\
         (ns lib.b (:require [lib.a :as a]))\n\
         (defn from-b [] (a/from-a))\n\
         (mapv first (from-b))",
        "[lib.a$from-a lib.b$from-b]",
    );
}

/// At the top level there are no live closure frames at all -- mova does
/// not compile a top-level form into an `evalNNN` fn the way the JVM does,
/// so the stack is genuinely empty. This is what makes the
/// `::stest/caller` key ABSENT for a top-level bad call (documented in
/// docs/SPEC-PORT-PATCHES.md; the corpus routes around it).
#[test]
fn callstack_at_the_top_level_is_empty() {
    is("(callstack*)", "[]");
}

// ---------------------------------------------------------------------------
// `clojure.core/defn` computes `:arglists`
// ---------------------------------------------------------------------------

/// `instrument-1` reads `(->> v meta :arglists (sort-by count) seq)` and
/// nothing else decides whether a kwargs fn gets its kvs-forwarding thunk,
/// so this metadata is load-bearing, not cosmetic.
#[test]
fn defn_computes_arglists_metadata() {
    is(
        "(defn f ([a] a) ([a b] [a b]) ([a b & {:as m}] [a b m]))\n\
         (:arglists (meta #'f))",
        "([a] [a b] [a b & {:as m}])",
    );
    is("(defn g [x] x) (:arglists (meta #'g))", "([x])");
    is("(defn- h [x] x) (:arglists (meta #'h))", "([x])");
}

/// An EXPLICIT `:arglists` in an attr-map still wins, exactly as real
/// `defn`'s `(conj {:arglists ..} m)` makes it -- vendored
/// `clojure/test/check/clojure_test.cljc` relies on that.
#[test]
fn an_explicit_arglists_attr_map_wins_over_the_computed_one() {
    is(
        "(defn f {:arglists '([supplied])} ([a] a) ([a b] [a b]))\n\
         (:arglists (meta #'f))",
        "([supplied])",
    );
}

/// `alter-meta!` can take it away again -- instr.clj's `add10` shape.
#[test]
fn arglists_can_be_removed_with_alter_meta() {
    is(
        "(defn f [x] x) (alter-meta! #'f dissoc :arglists) (:arglists (meta #'f))",
        "nil",
    );
}

// ---------------------------------------------------------------------------
// The port itself, on a bare binary
// ---------------------------------------------------------------------------

/// THE acceptance shape for wave 5: no `--module-path` at all, and the
/// whole instrument/unstrument round trip.
#[test]
fn spec_test_alpha_requires_and_instruments_with_no_module_path() {
    on_big_stack(|| {
        is(
            "(ns main (:require [clojure.spec.alpha :as s]\n\
            \x20                  [clojure.spec.test.alpha :as stest]))\n\
             (defn adder [a b] (+ a b))\n\
             (s/fdef adder :args (s/cat :a number? :b number?) :ret number?)\n\
             [(stest/instrument `adder)\n\
            \x20 (adder 1 2)\n\
            \x20 (try (adder 1 :nope) (catch Throwable e (ex-message e)))\n\
            \x20 (stest/unstrument `adder)\n\
            \x20 (try (adder 1 :nope) (catch Throwable e :threw))]",
            "[[main/adder] 3 \"Call to main/adder did not conform to spec.\" \
             [main/adder] :threw]",
        );
    });
}

/// The ex-data an instrument failure carries: spec's own explain-data plus
/// `::s/fn`, `::s/args` and `::s/failure :instrument`.
#[test]
fn an_instrument_failure_carries_spec_failure_fn_and_args() {
    on_big_stack(|| {
        is(
            "(ns main (:require [clojure.spec.alpha :as s]\n\
            \x20                  [clojure.spec.test.alpha :as stest]))\n\
             (defn adder [a b] (+ a b))\n\
             (s/fdef adder :args (s/cat :a number? :b number?))\n\
             (stest/instrument `adder)\n\
             (try (adder 1 :nope)\n\
            \x20 (catch Throwable e\n\
            \x20   (let [d (ex-data e)]\n\
            \x20     [(:clojure.spec.alpha/failure d)\n\
            \x20      (:clojure.spec.alpha/fn d)\n\
            \x20      (:clojure.spec.alpha/args d)\n\
            \x20      (mapv :path (:clojure.spec.alpha/problems d))\n\
            \x20      (mapv :val (:clojure.spec.alpha/problems d))])))",
            "[:instrument main/adder (1 :nope) [[:b]] [:nope]]",
        );
    });
}

/// THE contract `clojure/test_clojure/instr.clj` asserts: `::stest/caller`
/// `:var-scope` is the var symbol of the plain `defn`'d helper that made
/// the failing call, with spec's own plumbing frames -- the checked thunk,
/// its `conform!` local, and (on the JVM) `clojure.core/apply` -- skipped.
#[test]
fn stest_caller_var_scope_is_the_calling_fn() {
    on_big_stack(|| {
        is(
            "(ns main (:require [clojure.spec.alpha :as s]\n\
            \x20                  [clojure.spec.test.alpha :as stest]))\n\
             (defn adder [a b] (+ a b))\n\
             (s/fdef adder :args (s/cat :a number? :b number?))\n\
             (stest/instrument `adder)\n\
             (defn fail-add [x y]\n\
            \x20 (try (adder x y)\n\
            \x20      (catch Throwable e (:clojure.spec.test.alpha/caller (ex-data e)))))\n\
             (defn outer [x y] (fail-add x y))\n\
             [(:var-scope (fail-add 1 :nope))\n\
            \x20 (:var-scope (outer 1 :nope))\n\
            \x20 (vec (sort (map str (keys (fail-add 1 :nope)))))]",
            "[main/fail-add main/fail-add [\":file\" \":line\" \":var-scope\"]]",
        );
    });
}

/// The same contract THROUGH the kwargs forwarding thunk -- the frame
/// upstream skips via its `--KVS--EMULATION--THUNK--` `:local-fn` marker and
/// this port skips via MOVA-PATCH P17's `var-scope` test, because mova
/// frames carry no enclosing-fn chain.
#[test]
fn stest_caller_is_found_through_the_kwargs_emulation_thunk() {
    on_big_stack(|| {
        is(
            "(ns main (:require [clojure.spec.alpha :as s]\n\
            \x20                  [clojure.spec.test.alpha :as stest]))\n\
             (s/def ::a any?)\n\
             (s/def ::b number?)\n\
             (defn kwargs-fn ([a] a) ([a b] [a b]) ([a b & {:as m}] [a b m]))\n\
             (s/fdef kwargs-fn\n\
            \x20 :args (s/alt :unary (s/cat :a ::a)\n\
            \x20              :binary (s/cat :a ::a :b ::b)\n\
            \x20              :variadic (s/cat :a ::a :b ::b\n\
            \x20                               :kwargs (s/keys* :opt-un [::a ::b]))))\n\
             (defn fail-kwargs [& args] (apply kwargs-fn args))\n\
             (stest/instrument `kwargs-fn)\n\
             [(kwargs-fn 1 2 :a 1 {:b 2})\n\
            \x20 (try (fail-kwargs 1 :not-num) nil\n\
            \x20      (catch Throwable e\n\
            \x20        (:var-scope (:clojure.spec.test.alpha/caller (ex-data e)))))\n\
            \x20 (try (fail-kwargs 1 2 :a 1 {:b :not-num}) nil\n\
            \x20      (catch Throwable e\n\
            \x20        (:var-scope (:clojure.spec.test.alpha/caller (ex-data e)))))]",
            "[[1 2 {:a 1, :b 2}] main/fail-kwargs main/fail-kwargs]",
        );
    });
}

/// The `::stest/caller` `:line` the corpus deliberately does NOT compare
/// against the oracle, pinned here so it cannot drift silently.
///
/// mova's macro expander rebuilds a macro's output carrying the macro CALL
/// form's span, so every form inside a `defn` body reports the `defn`'s own
/// position -- a general, pre-existing property of every stack trace mova
/// renders, not something this port introduced. The consequence: the caller
/// frame's line is the line of its `(defn ...)` head, where the JVM reports
/// the line of the call inside it. Here `fail-add`'s `defn` is on line 6 of
/// the source string and the `(adder x y)` call is on line 7.
#[test]
fn stest_caller_line_is_the_defn_head() {
    on_big_stack(|| {
        is(
            "(ns main (:require [clojure.spec.alpha :as s]\n\
            \x20                  [clojure.spec.test.alpha :as stest]))\n\
             (defn adder [a b] (+ a b))\n\
             (s/fdef adder :args (s/cat :a number? :b number?))\n\
             (stest/instrument `adder)\n\
             (defn fail-add [x y]\n\
            \x20 (try (adder x y)\n\
            \x20      (catch Throwable e (:clojure.spec.test.alpha/caller (ex-data e)))))\n\
             [(:file (fail-add 1 :nope)) (:line (fail-add 1 :nope))]",
            "[\"spec-test-alpha-test\" 6]",
        );
    });
}

/// `with-instrument-disabled` is dynamic-extent scoped, and the instrument
/// wrapper re-enables checking for the duration of the wrapped call so a
/// checked callee reached from a checked caller still throws.
#[test]
fn with_instrument_disabled_is_scoped_to_its_body() {
    on_big_stack(|| {
        is(
            "(ns main (:require [clojure.spec.alpha :as s]\n\
            \x20                  [clojure.spec.test.alpha :as stest]))\n\
             (defn f [x] x)\n\
             (s/fdef f :args (s/cat :x int?))\n\
             (stest/instrument `f)\n\
             [(try (f :bad) (catch Throwable e :threw))\n\
            \x20 (stest/with-instrument-disabled (f :bad))\n\
            \x20 (try (f :bad) (catch Throwable e :threw))]",
            "[:threw :bad :threw]",
        );
    });
}

/// The three opts `instrument` documents: `:spec` overrides the registered
/// fspec, `:stub` replaces the body with a `:ret` generator, and `:replace`
/// swaps in a fn of your own (with `:args` still checked first).
#[test]
fn instrument_honours_spec_stub_and_replace_overrides() {
    on_big_stack(|| {
        is(
            "(ns main (:require [clojure.spec.alpha :as s]\n\
            \x20                  [clojure.spec.test.alpha :as stest]))\n\
             (defn over [x] x)\n\
             (s/fdef over :args (s/cat :x int?))\n\
             (stest/instrument `over {:spec {`over (s/fspec :args (s/cat :x string?))}})\n\
             (def spec-result\n\
            \x20 [(over \"ok\") (try (over 1) (catch Throwable e :threw))])\n\
             (stest/unstrument `over)\n\
             (s/def ::big (s/and int? #(< 100 %)))\n\
             (defn stubbed [x] (throw (ex-info \"the real body must not run\" {})))\n\
             (s/fdef stubbed :args (s/cat :x int?) :ret ::big)\n\
             (stest/instrument `stubbed {:stub #{`stubbed}})\n\
             (def stub-result (s/valid? ::big (stubbed 1)))\n\
             (stest/unstrument `stubbed)\n\
             (defn repl-me [x] :original)\n\
             (s/fdef repl-me :args (s/cat :x int?))\n\
             (stest/instrument `repl-me {:replace {`repl-me (fn [x] :replaced)}})\n\
             (def replace-result\n\
            \x20 [(repl-me 1) (try (repl-me :bad) (catch Throwable e :threw))])\n\
             (stest/unstrument `repl-me)\n\
             [spec-result stub-result replace-result (over 1) (repl-me 1)]",
            "[[\"ok\" :threw] true [:replaced :threw] 1 :original]",
        );
    });
}

/// `check` drives test.check through `s/gen` on the `:args` spec and reports
/// `::stc/ret`. Assertions are on PROPERTIES (the seed varies), plus the
/// `:sym` and the summary map `summarize-results` returns.
#[test]
fn check_runs_generative_tests_and_summarizes_them() {
    on_big_stack(|| {
        is(
            "(ns main (:require [clojure.spec.alpha :as s]\n\
            \x20                  [clojure.spec.test.alpha :as stest]))\n\
             (defn ranged-rand [start end] (+ start (long (rand (- end start)))))\n\
             (s/fdef ranged-rand\n\
            \x20 :args (s/and (s/cat :start int? :end int?) #(< (:start %) (:end %)))\n\
            \x20 :ret int?\n\
            \x20 :fn (s/and #(>= (:ret %) (-> % :args :start))\n\
            \x20            #(< (:ret %) (-> % :args :end))))\n\
             (let [r (first (stest/check `ranged-rand\n\
            \x20                          {:clojure.spec.test.check/opts {:num-tests 8}}))\n\
            \x20      sink (atom nil)]\n\
            \x20 (with-out-str\n\
            \x20   (reset! sink (stest/summarize-results\n\
            \x20                 (stest/check `ranged-rand\n\
            \x20                              {:clojure.spec.test.check/opts {:num-tests 4}}))))\n\
            \x20 [(:sym r)\n\
            \x20  (-> r :clojure.spec.test.check/ret :pass?)\n\
            \x20  (-> r :clojure.spec.test.check/ret :num-tests)\n\
            \x20  (contains? r :failure)\n\
            \x20  (stest/abbrev-result r)\n\
            \x20  @sink])",
            "[main/ranged-rand true 8 false {:sym main/ranged-rand} \
             {:total 1, :check-passed 1}]",
        );
    });
}

/// A failing check: `:failure` is an exception carrying `::s/failure
/// :check-failed`, `abbrev-result` unwraps it and describes the spec, and
/// `summarize-results` counts it under that same key.
#[test]
fn a_failing_check_reports_check_failed_through_abbrev_result() {
    on_big_stack(|| {
        is(
            "(ns main (:require [clojure.spec.alpha :as s]\n\
            \x20                  [clojure.spec.test.alpha :as stest]))\n\
             (defn bad-ret [x] \"not an int\")\n\
             (s/fdef bad-ret :args (s/cat :x int?) :ret int?)\n\
             (let [r (first (stest/check `bad-ret\n\
            \x20                          {:clojure.spec.test.check/opts {:num-tests 4}}))\n\
            \x20      a (stest/abbrev-result r)\n\
            \x20      sink (atom nil)]\n\
            \x20 (with-out-str\n\
            \x20   (reset! sink (stest/summarize-results\n\
            \x20                 (stest/check `bad-ret\n\
            \x20                              {:clojure.spec.test.check/opts {:num-tests 4}}))))\n\
            \x20 [(:sym r)\n\
            \x20  (-> r :clojure.spec.test.check/ret :pass?)\n\
            \x20  (:clojure.spec.alpha/failure (ex-data (:failure r)))\n\
            \x20  (:spec a)\n\
            \x20  (:clojure.spec.alpha/failure (:failure a))\n\
            \x20  @sink])",
            "[main/bad-ret false :check-failed \
             (fspec :args (cat :x int?) :ret int? :fn nil) :check-failed \
             {:total 1, :check-failed 1}]",
        );
    });
}

/// The three set/enumeration fns, and the `:gen`-keys assertion both
/// `checkable-syms` and `instrumentable-syms` make.
#[test]
fn checkable_instrumentable_and_enumerate_namespace() {
    on_big_stack(|| {
        is(
            "(ns main (:require [clojure.spec.alpha :as s]\n\
            \x20                  [clojure.spec.test.alpha :as stest]))\n\
             (defn f [x] x)\n\
             (defn unspecd [x] x)\n\
             (s/fdef f :args (s/cat :x int?))\n\
             [(contains? (stest/checkable-syms) `f)\n\
            \x20 (contains? (stest/checkable-syms) `unspecd)\n\
            \x20 (contains? (stest/instrumentable-syms) `f)\n\
            \x20 (contains? (stest/instrumentable-syms {:stub #{'nowhere/nothing}})\n\
            \x20            'nowhere/nothing)\n\
            \x20 (contains? (stest/enumerate-namespace 'main) `f)\n\
            \x20 (contains? (stest/enumerate-namespace '[main clojure.string])\n\
            \x20            'clojure.string/join)\n\
            \x20 (try (stest/checkable-syms {:gen {\"not-an-ident\" nil}})\n\
            \x20      (catch Throwable e :asserted))]",
            "[true false true true true true :asserted]",
        );
    });
}

/// `->sym` is the one PUBLIC helper this namespace exposes besides the API
/// proper, and it is a thin wrapper over spec's own private `->sym`.
#[test]
fn stest_to_sym_names_a_var() {
    on_big_stack(|| {
        is(
            "(ns main (:require [clojure.spec.test.alpha :as stest]))\n\
             (defn f [x] x)\n\
             [(stest/->sym #'f) (stest/->sym `f)]",
            "[main/f main/f]",
        );
    });
}

// ---------------------------------------------------------------------------
// D12 (`docs/SPEC-PORT-PATCHES.md` item 12): a macro re-expanded at RUN
// time resolves against the LEXICAL namespace of its call site
// ---------------------------------------------------------------------------

/// The defect's spec-level face, and the reason it was ledgered at all:
/// `clojure.spec.alpha`'s `res` calls `resolve` on every `s/&`/`s/coll-of`/
/// `s/def` argument WHILE the macro is expanding, so a re-expansion under
/// the wrong `*ns*` silently rewrites the spec's own `s/form`.
///
/// The reproduction shape is the census harness's: the file's namespace is
/// `clojure.test-clojure.spec`, the deftest is entered with the dynamic
/// `*ns*` left at `user`, and the deftest body bails compilation (here a
/// `binding`, in the vendored file its sheer size) so the tree-walker
/// re-expands the `s/&` call site on every call.
///
/// Before the fix (measured, and the exact answer
/// `docs/SPEC-PORT-PATCHES.md` item 12 records):
/// `(clojure.spec.alpha/& (clojure.core/* clojure.core/keyword?) even-count?)`
/// -- `s/*` resolved to `clojure.core/*` and `even-count?` left unqualified.
#[test]
fn d12_spec_form_survives_a_run_time_re_expansion_under_a_foreign_ns() {
    on_big_stack(|| {
        is(
            "(ns clojure.test-clojure.spec (:require [clojure.spec.alpha :as s]))\n\
             (defn even-count? [x] (even? (count x)))\n\
             (def at-top (s/form (s/& (s/* keyword?) even-count?)))\n\
             (defn probe []\n\
            \x20 (binding [*warn-on-reflection* false]\n\
            \x20   (s/form (s/& (s/* keyword?) even-count?))))\n\
             (in-ns 'user)\n\
             (clojure.core/refer 'clojure.core)\n\
             [(str *ns*)\n\
            \x20 clojure.test-clojure.spec/at-top\n\
            \x20 (clojure.test-clojure.spec/probe)]",
            "[\"user\" \
             (clojure.spec.alpha/& (clojure.spec.alpha/* clojure.core/keyword?) clojure.test-clojure.spec/even-count?) \
             (clojure.spec.alpha/& (clojure.spec.alpha/* clojure.core/keyword?) clojure.test-clojure.spec/even-count?)]",
        );
    });
}
