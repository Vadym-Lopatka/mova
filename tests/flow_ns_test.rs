//! Acceptance tests for DESIGN-flow-namespace.md Part 1 (`clojure.core.
//! async.flow` and `clojure.core.async` become real namespaces) and Part 3
//! items 1-2 (`ping`/`ping-proc` keyword opts, `process`'s `:describe`).
//! Style matches `tests/flow_test.rs`: a fresh `Interp` per test, `eval_str`
//! against a literal source string, `eval_ok`/`eval_err` helpers.
//!
//! Part 2 (`::flow/report`/`::flow/error` always-wired out-targets) and
//! Part 3 item 3 (post-stop report-chan close) already have their own
//! coverage in `tests/flow_test.rs`/`tests/flow_wake_test.rs` -- this file
//! is scoped to the NAMESPACE work only.

use mova::internal::{Interp, Value};

fn eval_ok(src: &str) -> Value {
    let mut interp = Interp::new();
    interp
        .eval_str("flow_ns_test", src)
        .unwrap_or_else(|e| panic!("eval error for {src:?}: {}", mova::internal::render(&e, "flow_ns_test", src)))
}

fn eval_err(src: &str) -> String {
    let mut interp = Interp::new();
    match interp.eval_str("flow_ns_test", src) {
        Ok(v) => panic!("expected an error evaluating {src:?}, got {v:?}"),
        Err(e) => e.message,
    }
}

// ---------------------------------------------------------------------------
// 1. `clojure.core.async.flow` is a real, `require`-able namespace.
// ---------------------------------------------------------------------------

#[test]
fn requiring_the_real_flow_namespace_and_aliasing_it_works_end_to_end() {
    let result = eval_ok(
        r#"(require '[clojure.core.async.flow :as flow])
           (let [step (flow/map->step
                       {:describe (fn [] {:ins {} :outs {:out {}}})
                        :transform (fn [s _ m] [s {:out [m]}])})
                 g (flow/create-flow {:procs {:a {:proc (flow/process step)}}
                                       :conns []})]
             (flow? g))"#,
    );
    assert_eq!(result, Value::Bool(true));
}

#[test]
fn the_real_flow_namespace_is_visible_to_find_ns() {
    // `require` must have actually created the namespace, not just
    // recorded an alias -- `find-ns` is the direct probe for that.
    let result = eval_ok(
        r#"(require '[clojure.core.async.flow :as flow])
           (some? (find-ns 'clojure.core.async.flow))"#,
    );
    assert_eq!(result, Value::Bool(true));
}

// ---------------------------------------------------------------------------
// 2. `clojure.core.async` is a real, `require`-able namespace whose public
//    surface is exactly what mova implements.
// ---------------------------------------------------------------------------

#[test]
fn requiring_the_real_async_namespace_and_using_chan_works() {
    let result = eval_ok(r#"(require '[clojure.core.async :as a]) (chan? (a/chan))"#);
    assert_eq!(result, Value::Bool(true));
}

#[test]
fn chan_predicate_is_reachable_through_the_async_namespace() {
    // `chan?` is registered by predicates.rs, not async.rs -- the one
    // async-surface name outside that module, so PUBLIC_NAMES has to carry
    // it explicitly. Both client apps' timer facades spell it
    // `async/chan?`; the restricted qualified->bare fallback (rightly)
    // refuses that spelling unless this row exists.
    let result = eval_ok(r#"(require '[clojure.core.async :as a]) (a/chan? (a/chan))"#);
    assert_eq!(result, Value::Bool(true));
}

#[test]
fn the_real_async_namespace_also_reaches_a_bootstrap_macro_with_macro_ness_intact() {
    // `go`/`go-loop`/`thread`/`>!`/`<!`/`alts!`/`onto-chan!` are defined by
    // `core/async.mova`, not natives -- this pins that `install_core_async_ns`
    // (`eval::Interp`) reaches those too, and that `go` is still usable as a
    // MACRO (not just a resolvable value) through the qualified spelling.
    let result = eval_ok(
        r#"(require '[clojure.core.async :as a])
           (let [ch (a/go 42)]
             (a/<! ch))"#,
    );
    assert_eq!(result, Value::Int(42));
}

#[test]
fn an_unimplemented_upstream_async_name_fails_loudly_not_silently_onto_clojure_core() {
    // DESIGN-flow-namespace.md Part 1 point 4's trap: `clojure.core.async`
    // has no `merge` of its own (mova doesn't implement it), so `a/merge`
    // must NOT silently resolve to `clojure.core/merge` -- it must fail.
    let msg = eval_err(r#"(require '[clojure.core.async :as a]) (a/merge {} {})"#);
    assert!(msg.contains("Unable to resolve"), "message was: {msg}");
}

// ---------------------------------------------------------------------------
// 3. Bare-corpus compat: `flow/...` keeps working with NO require, via the
//    engine-owned default alias (ns.rs's `DEFAULT_ALIASES`).
// ---------------------------------------------------------------------------

#[test]
fn flow_prefixed_create_flow_and_process_work_with_no_require() {
    let result = eval_ok(
        r#"(let [step (flow/map->step
                       {:describe (fn [] {:ins {} :outs {}})
                        :transform (fn [s _ m] [s {}])})
                 g (flow/create-flow {:procs {:a {:proc (flow/process step)}}
                                       :conns []})]
             (flow? g))"#,
    );
    assert_eq!(result, Value::Bool(true));
}

#[test]
fn async_prefixed_chan_works_with_no_require() {
    let result = eval_ok(r#"(chan? (async/chan))"#);
    assert_eq!(result, Value::Bool(true));
}

// ---------------------------------------------------------------------------
// 4. A user namespace literally named `flow` beats the default alias.
// ---------------------------------------------------------------------------

#[test]
fn a_user_namespace_named_flow_beats_the_default_alias() {
    let result = eval_ok(r#"(ns flow) (def marker 42) (in-ns 'user) flow/marker"#);
    assert_eq!(result, Value::Int(42));
}

#[test]
fn inside_the_user_flow_namespace_the_default_alias_spelling_is_shadowed() {
    // From WITHIN the user's own `(ns flow)`, an unqualified `marker`
    // resolves to the user's own def (current-ns candidate, unaffected by
    // any of this) -- and `flow/marker` still can't mean anything else
    // either, since `flow` no longer names a DIFFERENT namespace to alias
    // away to: `expand_alias`'s literal-namespace check means `flow` stays
    // `flow`.
    let result = eval_ok(r#"(ns flow) (def marker 42) flow/marker"#);
    assert_eq!(result, Value::Int(42));
}

// ---------------------------------------------------------------------------
// 5. Reader parity: `::flow/report` auto-resolves through the SAME
//    precedence as symbol resolution, with and without an explicit require.
// ---------------------------------------------------------------------------

#[test]
fn keyword_alias_auto_resolves_through_the_default_alias_with_no_require() {
    let result = eval_ok(r#"(= ::flow/report :clojure.core.async.flow/report)"#);
    assert_eq!(result, Value::Bool(true));
}

#[test]
fn keyword_alias_auto_resolves_the_same_way_after_an_explicit_require() {
    let result = eval_ok(
        r#"(require '[clojure.core.async.flow :as flow])
           (= ::flow/report :clojure.core.async.flow/report)"#,
    );
    assert_eq!(result, Value::Bool(true));
}

#[test]
fn keyword_alias_does_not_leak_the_default_table_once_flow_is_a_literal_namespace() {
    // Mirrors test 4's symbol-resolution case at the READER level, but the
    // observable shape differs because `::alias/kw` and a qualified SYMBOL
    // resolve completely differently in real Clojure: a qualified symbol
    // whose namespace portion isn't a registered alias still tries it as a
    // literal namespace name (that's `expand_alias`'s own fallback, and
    // what makes `flow/marker` keep working below), but `::alias/kw`
    // requires `alias` to be a REAL alias table entry in the reading ns --
    // it never falls back to treating the token as a bare namespace name,
    // with or without this design's default-alias table (matches upstream:
    // `(ns foo) (in-ns 'user) ::foo/bar` is "Invalid token" too, real
    // namespace or not).
    //
    // So the correct, PARITY-preserving behavior once `flow` is a literal
    // namespace (precedence step (b)) is that `reader_ns_context` must NOT
    // inject the default `flow -> clojure.core.async.flow` entry either --
    // `::flow/x` must fail to read at all here, the same "no entry" error
    // it would hit with no default-alias table in the picture, rather than
    // silently reading as `:clojure.core.async.flow/x`. A qualified SYMBOL
    // reference (`flow/marker`) is unaffected: it still resolves to the
    // literal `flow` namespace, exactly like test 4.
    let mut interp = Interp::new();
    interp.eval_str("t", "(ns flow) (def marker 42) (in-ns 'user)").expect("setup");
    let err = interp.eval_str("t2", "::flow/x").expect_err("::flow/x must not silently mean :clojure.core.async.flow/x");
    assert!(err.message.contains("Invalid token"), "message was: {}", err.message);

    // The literal namespace itself is still reachable, both as an ordinary
    // qualified symbol and via the default alias's OWN qualified symbol
    // spelling for the real flow namespace (unaffected, different mechanism):
    assert_eq!(interp.eval_str("t3", "flow/marker").unwrap(), Value::Int(42));
}

// ---------------------------------------------------------------------------
// 6. `ping`/`ping-proc`: both the legacy positional spelling and upstream's
//    keyword-opts spelling work (DESIGN-flow-namespace.md Part 3 item 1).
// ---------------------------------------------------------------------------

fn minimal_running_flow() -> &'static str {
    r#"(require '[clojure.core.async.flow :as flow])
       (def g (flow/create-flow
               {:procs {:p {:proc (flow/process
                                    (flow/map->step
                                     {:describe (fn [] {:ins {} :outs {}})
                                      :transform (fn [s _ m] [s {}])}))}}
                :conns []}))
       (flow/start g)"#
}

#[test]
fn ping_proc_accepts_the_positional_timeout_the_existing_corpus_uses() {
    let src = format!(
        r#"{}
           (some? (flow/ping-proc g :p 500))"#,
        minimal_running_flow()
    );
    assert_eq!(eval_ok(&src), Value::Bool(true));
}

#[test]
fn ping_proc_also_accepts_upstreams_keyword_opts_spelling() {
    let src = format!(
        r#"{}
           (some? (flow/ping-proc g :p :timeout-ms 500))"#,
        minimal_running_flow()
    );
    assert_eq!(eval_ok(&src), Value::Bool(true));
}

#[test]
fn ping_accepts_both_the_positional_and_keyword_opts_spellings() {
    let src = format!(
        r#"{}
           [(map? (flow/ping g 500)) (map? (flow/ping g :timeout-ms 500)) (map? (flow/ping g))]"#,
        minimal_running_flow()
    );
    let result = eval_ok(&src);
    let Value::Vector(v) = &result else { panic!("expected a vector, got {result:?}") };
    assert_eq!(v.len(), 3);
    for (i, item) in v.iter().enumerate() {
        assert_eq!(item, &Value::Bool(true), "element {i} was not true: {result:?}");
    }
}

// ---------------------------------------------------------------------------
// 7. `process`'s return map carries `:describe` (DESIGN-flow-namespace.md
//    Part 3 item 2) -- upstream's launcher shape, which client code (e.g. a
//    dangling-port scan) reads as `(:outs ((:describe proc)))`.
// ---------------------------------------------------------------------------

#[test]
fn process_return_map_carries_a_working_describe_fn() {
    let result = eval_ok(
        r#"(require '[clojure.core.async.flow :as flow])
           (let [step (flow/map->step
                       {:describe (fn [] {:ins {:in {}} :outs {:out {}}})
                        :transform (fn [s _ m] [s {}])})
                 proc (flow/process step)]
             (:outs ((:describe proc))))"#,
    );
    let Value::Map(m) = result else { panic!("expected a map") };
    assert!(m.get(&Value::Keyword("out".into())).is_some(), "describe's :outs lost :out: {m:?}");
}

// ---------------------------------------------------------------------------
// 8. Post-review hardening: orphaned interop spellings restored by item 5
//    (0775556) as REAL registrations, not the old coincidental bare-name
//    fallback -- `builtins::numbers`/`builtins::sys`/`builtins::statics`'
//    own fixes, plus the new `clojure.repl` namespace (`install_clojure_repl_ns`,
//    eval/mod.rs).
// ---------------------------------------------------------------------------

#[test]
fn math_max_and_min_resolve_under_both_class_spellings() {
    assert_eq!(eval_ok("(Math/max 1 2)"), Value::Int(2));
    assert_eq!(eval_ok("(java.lang.Math/max 1 2)"), Value::Int(2));
    assert_eq!(eval_ok("(Math/min 1 2)"), Value::Int(1));
    assert_eq!(eval_ok("(java.lang.Math/min 1 2)"), Value::Int(1));
}

#[test]
fn system_getenv_resolves_under_both_class_spellings() {
    // Doesn't assert a specific value (the test environment's `$HOME` isn't
    // this suite's business) -- only that the call resolves and returns a
    // string, i.e. it doesn't throw "Unable to resolve".
    assert!(matches!(eval_ok(r#"(System/getenv "HOME")"#), Value::Str(_) | Value::Nil));
    assert!(matches!(eval_ok(r#"(java.lang.System/getenv "HOME")"#), Value::Str(_) | Value::Nil));
}

#[test]
fn system_resolves_as_a_class_value_like_math() {
    // mova campaign (clojure-lsp): `System` -- statics (above) already
    // worked with no class row; `System` itself must ALSO resolve bare/FQ
    // as a real class VALUE (usable in `(class? ...)`, `=`, and as a sci
    // `:classes {'System System}` map value), same as `Math`.
    assert_eq!(eval_ok("(class? System)"), Value::Bool(true));
    assert_eq!(eval_ok("(class? java.lang.System)"), Value::Bool(true));
    assert_eq!(eval_ok("(= System java.lang.System)"), Value::Bool(true));
    assert_eq!(eval_ok("(map? {System 1})"), Value::Bool(true));
}

#[test]
fn string_format_and_long_integer_compare_max_min_resolve() {
    assert_eq!(eval_ok(r#"(String/format "%d" 42)"#), Value::Str("42".into()));
    assert_eq!(eval_ok("(Long/compare 1 2)"), Value::Int(-1));
    assert_eq!(eval_ok("(Integer/compare 2 1)"), Value::Int(1));
    assert_eq!(eval_ok("(Long/max 1 2)"), Value::Int(2));
    assert_eq!(eval_ok("(Long/min 1 2)"), Value::Int(1));
    assert_eq!(eval_ok("(Integer/max 1 2)"), Value::Int(2));
    assert_eq!(eval_ok("(Integer/min 1 2)"), Value::Int(1));
}

#[test]
fn clojure_repl_doc_source_apropos_dir_all_resolve() {
    // `apropos` is an ordinary fn -- exercised through a real call.
    let result = eval_ok(r#"(some? (clojure.repl/apropos "map"))"#);
    assert_eq!(result, Value::Bool(true));
    // `doc`/`source`/`dir` are macros -- `install_clojure_repl_ns` must
    // preserve macro-ness through `bind_alias` (same `Value::Macro` cell,
    // not a copy), so calling them in head position must not throw
    // "not callable"/"Unable to resolve".
    assert_eq!(eval_ok("(clojure.repl/doc doc)"), Value::Nil);
    assert_eq!(eval_ok("(clojure.repl/source dir)"), Value::Nil);
    assert_eq!(eval_ok("(clojure.repl/dir clojure.core)"), Value::Nil);
}

// ---------------------------------------------------------------------------
// 9. Post-review hardening: `expand_alias`/`reader_ns_context`'s literal-ns
//    precedence check consults `reg.loaded`, not a mere `reg.namespaces`
//    registry entry -- a transient `(in-ns 'flow)` typo must not
//    permanently shadow the `flow` default alias program-wide.
// ---------------------------------------------------------------------------

#[test]
fn a_transient_in_ns_typo_does_not_shadow_the_default_flow_alias() {
    // `(in-ns 'flow)` touches the registry (`or_default` creates an entry
    // for "flow") but never loads anything -- `flow/create-flow` must
    // still resolve through the default alias afterward, exactly as if
    // the in-ns typo had never happened.
    let result = eval_ok(
        r#"(in-ns 'flow)
           (clojure.core/in-ns 'user)
           (let [step (flow/map->step
                       {:describe (fn [] {:ins {} :outs {}})
                        :transform (fn [s _ m] [s {}])})
                 g (flow/create-flow {:procs {:a {:proc (flow/process step)}} :conns []})]
             (flow? g))"#,
    );
    assert_eq!(result, Value::Bool(true));
}
