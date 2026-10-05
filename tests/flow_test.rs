//! Integration tests for Phase F1's `core.async.flow` engine
//! (`src/builtins/flow.rs` + `core/flow.mova`). See FLOW-DESIGN.md for the
//! contract, and FLOW-IDLE-CPU-BUG.md for the `Doorbell` wake-primitive fix
//! (see `value.rs`'s `Doorbell` doc and `builtins::flow`'s module doc,
//! "Control-priority wait design") that changed the numbers this comment
//! used to calibrate against -- corrected below; `tests/flow_wake_test.rs`
//! is the DEDICATED regression suite for that fix specifically (idle procs
//! must not fall back to polling, injected/control events must wake a
//! parked proc promptly), with tighter, more direct assertions than the
//! "generous margin" style here.
//!
//! Timing-sensitive assertions in THIS file still use generous margins
//! (matching `tests/conc_test.rs`/`tests/async_test.rs`'s convention), but
//! what they're generous RELATIVE TO changed with the fix:
//!
//! - Single-input park, multi-input's scan-then-park tail, `run_fused`'s
//!   head-of-chain reads, AND the transport (SPSC) tier's data-direction
//!   read are ALL `Doorbell`-driven now -- no tier is the exception
//!   anymore (an earlier pass here left the transport tier on a short,
//!   fixed, non-`Doorbell` poll interval; that gap has since closed, see
//!   `builtins::flow`'s module doc, "Composition with control/pause/stop",
//!   and `transport.rs`'s "BOUNDED waits" section for the mechanism). Every
//!   one of them is woken instantly by a real event in the overwhelmingly
//!   common case, with `PARK_TIMEOUT` (2s) surviving only as a defensive
//!   safety net, NOT a live mechanism (see that constant's doc). A margin
//!   only needs to outlast genuine processing time now, not a poll
//!   interval -- but must still stay comfortably under 2s, since a
//!   regression back to poll-only behavior (on ANY tier) is exactly what
//!   `flow_wake_test.rs` exists to catch, and a test in THIS file with a
//!   multi-second margin would silently paper over that regression
//!   instead. `PING_POLL` (500µs) is the one timing constant in this
//!   module genuinely unaffected by any of this -- it's `flow/ping`'s own
//!   reply-collection poll, unrelated to a proc's park primitive.
//! - "Verify nothing arrived yet" checks still always pause a proc (or
//!   simply never inject) rather than racing an `inject` against a
//!   `pause-proc` -- that race is now narrowed, on EVERY tier, to the
//!   nanosecond-scale gap `value.rs`'s `Doorbell` doc calls out (between a
//!   non-blocking control check and the following park's generation
//!   snapshot), rather than the old up-to-one-message slop a condvar wait
//!   on the data chan's OWN condvar had no way to close. Avoiding the race
//!   entirely remains simplest either way.

use mova::internal::Interp;
use mova::internal::Value;

fn eval_ok(src: &str) -> Value {
    let mut interp = Interp::new();
    interp
        .eval_str("test", src)
        .unwrap_or_else(|e| panic!("eval error for {src:?}: {}", mova::internal::render(&e, "test", src)))
}

fn eval_err(src: &str) -> String {
    let mut interp = Interp::new();
    match interp.eval_str("test", src) {
        Ok(v) => panic!("expected an error evaluating {src:?}, got {v:?}"),
        Err(e) => e.message,
    }
}

fn ps(src: &str) -> String {
    mova::internal::pr_str(&eval_ok(src))
}

// ---------------------------------------------------------------------------
// map->step* (N1 native step tier, see NATIVE-STEP-DESIGN.md): validation
// errors and shell-arity dispatch. `core/flow.mova`'s `map->step` is now
// `(def map->step flow/map->step*)`, so calling `map->step` here exercises
// the native directly -- every OTHER test in this file already exercises it
// implicitly (every flow program goes through `map->step`), which is why
// "all 31 pre-existing tests still pass unchanged" is itself a differential
// check for this stage.
// ---------------------------------------------------------------------------

#[test]
fn map_to_step_missing_describe_throws_the_same_catchable_string_as_before() {
    // `catch`'s binding sees the raw thrown VALUE for a `(throw v)`
    // (`ErrorKind::Thrown` -> `e.thrown.clone()`, not an error-info map --
    // see `eval_try`), so this pins the exact observable value, not just a
    // substring of some rendered message.
    let result = eval_ok(
        r#"(try
             (flow/map->step {:transform (fn [s _ m] [s {}])})
             (catch e e))"#,
    );
    assert_eq!(result, Value::Str("map->step: :describe is required".into()));
}

#[test]
fn map_to_step_missing_transform_throws_the_same_catchable_string_as_before() {
    let result = eval_ok(
        r#"(try
             (flow/map->step {:describe (fn [] {:ins {} :outs {}})})
             (catch e e))"#,
    );
    assert_eq!(result, Value::Str("map->step: :transform is required".into()));
}

#[test]
fn map_to_step_missing_both_reports_describe_first() {
    // Matches the interpreted version's `(when (nil? describe) ...)` then
    // `(when (nil? transform) ...)` ordering: :describe is checked first,
    // so an entirely empty map's error names :describe, not :transform.
    let result = eval_ok(r#"(try (flow/map->step {}) (catch e e))"#);
    assert_eq!(result, Value::Str("map->step: :describe is required".into()));
}

#[test]
fn map_to_step_star_returns_a_native() {
    let v = eval_ok(r#"(flow/map->step {:describe (fn [] {:ins {} :outs {}}) :transform (fn [s _ m] [s {}])})"#);
    assert!(matches!(v, Value::Native(_)), "expected map->step to build a Value::Native shell, got {v:?}");
}

#[test]
fn map_to_step_star_shell_dispatches_arities_0_through_3_to_the_right_fn() {
    let result = eval_ok(
        r#"(let [sf (flow/map->step
                      {:describe (fn [] {:ins {:in {}} :outs {:out {}}})
                       :init (fn [args] (assoc args :inited true))
                       :transition (fn [s t] (assoc s :last-transition t))
                       :transform (fn [s cid m] [(assoc s :last-msg m) {:out [[cid m]]}])})]
              {:d (sf)
               :i (sf {:pid :p})
               :t (sf {:n 1} :clojure.core.async.flow/pause)
               :tr (sf {:n 2} :in :hello)})"#,
    );
    let Value::Map(m) = &result else { panic!("expected a map, got {result:?}") };
    let get = |k: &str| m.get(&Value::Keyword(k.into())).cloned().unwrap();

    // arity 0 -> describe()
    let Value::Map(d) = get("d") else { panic!("describe result must be a map") };
    assert_eq!(d.get(&Value::Keyword("ins".into())), Some(&Value::Map({
        let mut m = mova::pmap!();
        m.insert(Value::Keyword("in".into()), Value::Map(mova::pmap!()));
        m
    })));

    // arity 1 -> init(arg-map)
    let Value::Map(i) = get("i") else { panic!("init result must be a map") };
    assert_eq!(i.get(&Value::Keyword("pid".into())), Some(&Value::Keyword("p".into())));
    assert_eq!(i.get(&Value::Keyword("inited".into())), Some(&Value::Bool(true)));

    // arity 2 -> transition(state, transition-kw)
    let Value::Map(t) = get("t") else { panic!("transition result must be a map") };
    assert_eq!(t.get(&Value::Keyword("n".into())), Some(&Value::Int(1)));
    assert_eq!(
        t.get(&Value::Keyword("last-transition".into())),
        Some(&Value::Keyword("clojure.core.async.flow/pause".into()))
    );

    // arity 3 -> transform(state, cid, msg) -> [state' {out-id [msgs]}]
    let Value::Vector(tr) = get("tr") else { panic!("transform result must be a vector") };
    assert_eq!(tr.len(), 2);
    let Value::Map(new_state) = &tr[0] else { panic!("transform state' must be a map") };
    assert_eq!(new_state.get(&Value::Keyword("last-msg".into())), Some(&Value::Keyword("hello".into())));
    assert_eq!(new_state.get(&Value::Keyword("n".into())), Some(&Value::Int(2)));
    let expected_out = Value::Vector(mova::pvec![Value::Keyword("in".into()), Value::Keyword("hello".into())]);
    assert_eq!(
        tr[1],
        Value::Map({
            let mut m = mova::pmap!();
            m.insert(Value::Keyword("out".into()), Value::Vector(mova::pvec![expected_out]));
            m
        })
    );
}

#[test]
fn map_to_step_star_shell_default_init_and_transition_need_no_user_fn() {
    // :init and :transition are OMITTED here -- the shell must fall back
    // to the same defaults the interpreted version's inline `(or init (fn
    // [_] {}))` / `(or transition (fn [state _] state))` produced: an
    // empty-map init (ignoring the arg-map) and an identity transition.
    let result = eval_ok(
        r#"(let [sf (flow/map->step
                      {:describe (fn [] {:ins {} :outs {}})
                       :transform (fn [s _ m] [s {}])})]
              [(sf {:whatever 1}) (sf {:n 5} :clojure.core.async.flow/resume)])"#,
    );
    let Value::Vector(v) = &result else { panic!("expected a vector, got {result:?}") };
    assert_eq!(v[0], Value::Map(mova::pmap!()));
    let mut expected_state = mova::pmap!();
    expected_state.insert(Value::Keyword("n".into()), Value::Int(5));
    assert_eq!(v[1], Value::Map(expected_state));
}

#[test]
fn map_to_step_star_shell_wrong_arity_errors_like_the_old_anonymous_fn_did() {
    // The old interpreted shell was `(fn ([] ...) ([arg-map] ...) ([state
    // t] ...) ([state cid msg] ...))` -- no leading name symbol, so
    // `apply_closure`'s arity error named it "anonymous-fn". Reproduced
    // verbatim by the native shell for any argc outside 0..=3.
    let msg = eval_err(
        r#"(let [sf (flow/map->step {:describe (fn [] {:ins {} :outs {}}) :transform (fn [s _ m] [s {}])})]
              (sf 1 2 3 4 5))"#,
    );
    assert!(msg.contains("anonymous-fn"), "message was: {msg}");
    assert!(msg.contains("expects 0 or 1 or 2 or 3"), "message was: {msg}");
    assert!(msg.contains("called with 5 arguments"), "message was: {msg}");
}

// ---------------------------------------------------------------------------
// create-flow: eager validation
// ---------------------------------------------------------------------------

#[test]
fn create_flow_rejects_unknown_pid_in_conn() {
    let msg = eval_err(
        r#"(flow/create-flow
             {:procs {:a {:proc (flow/process
                                  (flow/map->step
                                   {:describe (fn [] {:ins {} :outs {:out {}}})
                                    :transform (fn [s _ m] [s {}])}))}}
              :conns [[[:a :out] [:ghost :in]]]})"#,
    );
    assert!(msg.contains("unknown pid"), "message was: {msg}");
    assert!(msg.contains(":ghost"), "message was: {msg}");
}

#[test]
fn create_flow_rejects_port_not_declared_in_describe() {
    let msg = eval_err(
        r#"(flow/create-flow
             {:procs {:a {:proc (flow/process
                                  (flow/map->step
                                   {:describe (fn [] {:ins {} :outs {:out {}}})
                                    :transform (fn [s _ m] [s {}])}))}
                       :b {:proc (flow/process
                                  (flow/map->step
                                   {:describe (fn [] {:ins {:in {}} :outs {}})
                                    :transform (fn [s _ m] [s {}])}))}}
              :conns [[[:a :nope] [:b :in]]]})"#,
    );
    assert!(msg.contains("no out port"), "message was: {msg}");
    assert!(msg.contains(":nope"), "message was: {msg}");
}

#[test]
fn create_flow_rejects_port_declared_as_both_in_and_out() {
    let msg = eval_err(
        r#"(flow/create-flow
             {:procs {:a {:proc (flow/process
                                  (flow/map->step
                                   {:describe (fn [] {:ins {:x {}} :outs {:x {}}})
                                    :transform (fn [s _ m] [s {}])}))}}
              :conns []})"#,
    );
    assert!(msg.contains("both an in and an out"), "message was: {msg}");
}

#[test]
fn create_flow_rejects_malformed_procs_shape() {
    let msg = eval_err(r#"(flow/create-flow {:procs [1 2 3] :conns []})"#);
    assert!(msg.contains("expected a map"), "message was: {msg}");
}

#[test]
fn create_flow_rejects_empty_procs_map() {
    let msg = eval_err(r#"(flow/create-flow {:procs {} :conns []})"#);
    assert!(msg.contains("non-empty"), "message was: {msg}");
}

// ---------------------------------------------------------------------------
// start: report/error chans, paused-by-default
// ---------------------------------------------------------------------------

#[test]
fn start_returns_report_and_error_chans_and_procs_start_paused() {
    let result = eval_ok(
        r#"(let [relay (flow/map->step
                         {:describe (fn [] {:ins {:in {}} :outs {:out {}}})
                          :transform (fn [s _ m] [s {:out [m]}])})
                  sink-ch (chan 10)
                  sink (flow/map->step
                        {:describe (fn [] {:ins {:in {}} :outs {}})
                         :transform (fn [s _ m] (>!! sink-ch m) [s {}])})
                  fl (flow/create-flow
                      {:procs {:r {:proc (flow/process relay)}
                               :s {:proc (flow/process sink)}}
                       :conns [[[:r :out] [:s :in]]]})
                  chans (flow/start fl)
                  has-report (chan? (:report-chan chans))
                  has-error (chan? (:error-chan chans))]
              (flow/inject fl [:r :in] [1 2 3])
              (sleep-ms 100)
              [has-report has-error (poll! sink-ch)])"#,
    );
    assert_eq!(mova::internal::pr_str(&result), "[true true nil]");
}

// ---------------------------------------------------------------------------
// Design Part 2: `::flow/report`/`::flow/error` are always-wired
// out-targets -- a transform's `{:clojure.core.async.flow/report [...]}` /
// `{:clojure.core.async.flow/error [...]}` entries reach `:report-chan` /
// `:error-chan` exactly like any other declared out port, alongside
// whatever else the SAME return map sends to a real port. `::flow/report`
// hasn't been given a reader-level alias yet (that's Part 1, a later
// phase), so these use the fully-qualified keyword literal directly --
// valid Clojure/mova syntax with no namespace lookup involved (only the
// `::flow/x` shorthand would need one).
// ---------------------------------------------------------------------------

#[test]
fn a_report_entry_reaches_report_chan_alongside_the_normal_out() {
    let result = ps(
        r#"(let [relay (flow/map->step
                         {:describe (fn [] {:ins {:in {}} :outs {:out {}}})
                          :transform (fn [s _ m]
                                       [s {:out [m] :clojure.core.async.flow/report [m]}])})
                  sink-ch (chan 10)
                  sink (flow/map->step
                        {:describe (fn [] {:ins {:in {}} :outs {}})
                         :transform (fn [s _ m] (>!! sink-ch m) [s {}])})
                  fl (flow/create-flow
                      {:procs {:r {:proc (flow/process relay)}
                               :s {:proc (flow/process sink)}}
                       :conns [[[:r :out] [:s :in]]]})
                  chans (flow/start fl)
                  _ (flow/resume fl)
                  _ (flow/inject fl [:r :in] [42])
                  reported (<!! (:report-chan chans))
                  delivered (<!! sink-ch)]
              (flow/stop fl)
              [reported delivered])"#,
    );
    assert_eq!(result, "[42 42]");
}

#[test]
fn an_error_entry_reaches_error_chan_alongside_the_normal_out() {
    let result = ps(
        r#"(let [relay (flow/map->step
                         {:describe (fn [] {:ins {:in {}} :outs {:out {}}})
                          ;; `::flow/error [msgs...]` -- ONE message
                          ;; here, the two-element vector `[:boom m]`
                          ;; itself (matching `::flow/report [m]`'s
                          ;; single-message shape elsewhere in this
                          ;; file, just non-scalar).
                          :transform (fn [s _ m]
                                       [s {:out [m] :clojure.core.async.flow/error [[:boom m]]}])})
                  sink-ch (chan 10)
                  sink (flow/map->step
                        {:describe (fn [] {:ins {:in {}} :outs {}})
                         :transform (fn [s _ m] (>!! sink-ch m) [s {}])})
                  fl (flow/create-flow
                      {:procs {:r {:proc (flow/process relay)}
                               :s {:proc (flow/process sink)}}
                       :conns [[[:r :out] [:s :in]]]})
                  chans (flow/start fl)
                  _ (flow/resume fl)
                  _ (flow/inject fl [:r :in] [7])
                  errored (<!! (:error-chan chans))
                  delivered (<!! sink-ch)]
              (flow/stop fl)
              [errored delivered])"#,
    );
    assert_eq!(result, "[[:boom 7] 7]");
}

/// A sink proc -- no declared out ports at all -- can still report:
/// `flow/start` wires the reserved keys into EVERY proc's outs map
/// regardless of what `:outs` the proc itself declares (design Part 2:
/// "not just procs that declare them").
#[test]
fn a_sink_proc_with_no_declared_outs_can_still_report() {
    let result = ps(
        r#"(let [sink-only (flow/map->step
                             {:describe (fn [] {:ins {:in {}} :outs {}})
                              :transform (fn [s _ m] [s {:clojure.core.async.flow/report [m]}])})
                  fl (flow/create-flow {:procs {:p {:proc (flow/process sink-only)}} :conns []})
                  chans (flow/start fl)
                  _ (flow/resume fl)
                  _ (flow/inject fl [:p :in] [:hello])
                  reported (<!! (:report-chan chans))]
              (flow/stop fl)
              reported)"#,
    );
    assert_eq!(result, ":hello");
}

// ---------------------------------------------------------------------------
// Post-review hardening: reserved-key guards (DESIGN-flow-namespace.md
// Part 2's two holes a code review found -- `create-flow` accepted a proc/
// conn that declared the reserved keys itself, and a proc's `:init` could
// silently hijack them via `::flow/out-ports`).
// ---------------------------------------------------------------------------

/// `create-flow` must REJECT a proc that declares `::flow/report` (or
/// `::flow/error`) among its own `:outs` -- those two are engine-reserved,
/// always wired by `flow/start` itself; letting a proc declare them too
/// would collide with that wiring invisibly.
#[test]
fn create_flow_rejects_a_proc_declaring_the_reserved_report_key_in_outs() {
    let msg = eval_err(
        r#"(flow/create-flow
             {:procs {:p {:proc (flow/process
                                  (flow/map->step
                                   {:describe (fn [] {:ins {} :outs {:clojure.core.async.flow/report {}}})
                                    :transform (fn [s _ m] [s {}])}))}}
              :conns []})"#,
    );
    assert!(msg.contains("engine-reserved"), "message was: {msg}");
}

/// Same guard, for a `:conns` entry naming a reserved key as either
/// endpoint -- a conn can't wire a normal port to/from a target that isn't
/// really a proc's own port at all.
#[test]
fn create_flow_rejects_a_conn_endpoint_naming_a_reserved_key() {
    let msg = eval_err(
        r#"(flow/create-flow
             {:procs {:r {:proc (flow/process
                                  (flow/map->step
                                   {:describe (fn [] {:ins {} :outs {:out {}}})
                                    :transform (fn [s _ m] [s {}])}))}
                      :s {:proc (flow/process
                                 (flow/map->step
                                  {:describe (fn [] {:ins {:in {}} :outs {}})
                                   :transform (fn [s _ m] [s {}])}))}}
              :conns [[[:r :clojure.core.async.flow/report] [:s :in]]]})"#,
    );
    assert!(msg.contains("engine-reserved"), "message was: {msg}");
}

/// A proc whose `:init` returns `{::flow/out-ports {::flow/report my-ch}}`
/// must NOT hijack report routing: the engine's own wiring wins after the
/// merge, so a transform's `{::flow/report [...]}` entries still land on
/// the flow's real `:report-chan`, and the user-supplied `my-ch` receives
/// nothing at all.
#[test]
fn init_time_out_ports_cannot_hijack_the_reserved_report_key() {
    let result = ps(
        r#"(let [my-ch (chan 10)
                  hijacker (flow/map->step
                            {:describe (fn [] {:ins {:in {}} :outs {}})
                             :init (fn [_] {:clojure.core.async.flow/out-ports
                                            {:clojure.core.async.flow/report my-ch}})
                             :transform (fn [s _ m] [s {:clojure.core.async.flow/report [m]}])})
                  fl (flow/create-flow {:procs {:p {:proc (flow/process hijacker)}} :conns []})
                  chans (flow/start fl)
                  _ (flow/resume fl)
                  _ (flow/inject fl [:p :in] [:hi])
                  reported (<!! (:report-chan chans))
                  hijacked (poll! my-ch)]
              (flow/stop fl)
              [reported hijacked])"#,
    );
    assert_eq!(result, "[:hi nil]", "engine wiring must win: real report-chan gets the message, my-ch gets nothing");
}

/// `ping`'s reported `:clojure.core.async.flow/outs` excludes both
/// engine-reserved keys (design Part 2, upstream `impl.clj:274`'s
/// `(dissoc outs ::flow/error ::flow/report)`) -- a caller inspecting a
/// proc's ports via `ping` must see only what the proc itself declared.
#[test]
fn ping_reply_outs_excludes_both_reserved_out_keys() {
    let result = ps(
        r#"(let [relay (flow/map->step
                         {:describe (fn [] {:ins {:in {}} :outs {:out {}}})
                          :transform (fn [s _ m] [s {:out [m]}])})
                  sink-ch (chan 10)
                  sink (flow/map->step
                        {:describe (fn [] {:ins {:in {}} :outs {}})
                         :transform (fn [s _ m] (>!! sink-ch m) [s {}])})
                  fl (flow/create-flow
                      {:procs {:r {:proc (flow/process relay)}
                               :s {:proc (flow/process sink)}}
                       :conns [[[:r :out] [:s :in]]]})
                  _ (flow/start fl)
                  _ (flow/resume fl)
                  reply (flow/ping fl 1000)
                  r-outs (get-in reply [:r :clojure.core.async.flow/outs])
                  s-outs (get-in reply [:s :clojure.core.async.flow/outs])]
              (flow/stop fl)
              [r-outs s-outs])"#,
    );
    assert_eq!(result, "[[:out] []]");
}

/// The Capture-arm fix (design Part 2's "fused loop" adjustment): a
/// MID-chain fused member's transform can still emit
/// `::flow/report`/`::flow/error`, and it must reach the real chans even
/// though the member's own output is captured into the run's internal
/// buffer rather than sent to a `Chan`. Run BOTH fused (`MOVA_FUSE_ALL=1`,
/// which fuses this all-interpreted chain) and unfused, and require the
/// report to arrive identically either way -- the same differential
/// discipline `fused_vs_unfused_interpreted_pipeline_is_observably_identical`
/// uses, because a missing Capture-arm route would only ever show up as a
/// FUSED-only difference (the unfused/generic loop routes reserved keys
/// with zero extra code, as it always has).
#[test]
fn a_mid_chain_fused_members_report_entry_still_reaches_report_chan() {
    const PROGRAM: &str = r#"(let [out-ch (chan 10)
             a (flow/map->step
                {:describe (fn [] {:ins {:in {}} :outs {:out {}}})
                 :transform (fn [s _ m] [s {:out [m]}])})
             b (flow/map->step
                {:describe (fn [] {:ins {:in {}} :outs {:out {}}})
                 :transform (fn [s _ m]
                              [s {:out [m] :clojure.core.async.flow/report [m]}])})
             c (flow/map->step
                {:describe (fn [] {:ins {:in {}} :outs {}})
                 :transform (fn [s _ m] (>!! out-ch m) [s {}])})
             fl (flow/create-flow
                 {:procs {:a {:proc (flow/process a)}
                          :b {:proc (flow/process b)}
                          :c {:proc (flow/process c)}}
                  :conns [[[:a :out] [:b :in]] [[:b :out] [:c :in]]]})
             chans (flow/start fl)
             _ (flow/resume fl)
             _ (flow/inject fl [:a :in] [99])
             reported (<!! (:report-chan chans))
             delivered (<!! out-ch)]
         (flow/stop fl)
         (println (pr-str [reported delivered])))"#;

    let fused = run_flow_program(PROGRAM, FUSE_ALL);
    let unfused = run_flow_program(PROGRAM, UNFUSED);
    // `-e`'s trailing "\nnil" is the auto-echoed top-level return value of
    // `(println ...)` itself (see `fused_vs_unfused_interpreted_pipeline_is_observably_identical`'s
    // identical trailing "\nnil" for the same reason).
    assert_eq!(
        fused,
        "[99 99]\nnil",
        "a fused mid-chain member's ::flow/report must still reach report-chan"
    );
    assert_eq!(unfused, "[99 99]\nnil");
}

// ---------------------------------------------------------------------------
// resume: ordered delivery, exact
// ---------------------------------------------------------------------------

#[test]
fn resume_delivers_100_values_in_order_exact() {
    let result = ps(
        r#"(let [doubler (flow/map->step
                           {:describe (fn [] {:ins {:in {}} :outs {:out {}}})
                            :transform (fn [s _ m] [s {:out [(* 2 m)]}])})
                  sink-ch (chan 200)
                  sink (flow/map->step
                        {:describe (fn [] {:ins {:in {}} :outs {}})
                         :transform (fn [s _ m] (>!! sink-ch m) [s {}])})
                  fl (flow/create-flow
                      {:procs {:d {:proc (flow/process doubler)}
                               :s {:proc (flow/process sink)}}
                       :conns [[[:d :out] [:s :in]]]})
                  _ (flow/start fl)
                  _ (flow/resume fl)
                  _ (flow/inject fl [:d :in] (range 100))
                  results (loop [n 100 acc []]
                            (if (zero? n)
                              acc
                              (recur (dec n) (conj acc (<!! sink-ch)))))
                  expected (vec (map #(* 2 %) (range 100)))]
              (flow/stop fl)
              (= results expected))"#,
    );
    assert_eq!(result, "true");
}

// ---------------------------------------------------------------------------
// R2: `(flow/process #'proc)` -- an invocable Var launcher
// ---------------------------------------------------------------------------

/// `(flow/process #'doubler)` must deliver the identical 100-value ordered
/// stream `resume_delivers_100_values_in_order_exact` gets from the bare
/// `(flow/process doubler)` -- the R2 mission's requirement that a var
/// launcher "behave identically to `(flow/process my-proc)`". The critical
/// wiring point is `Interp::apply_value`'s `Value::Var` arm (`apply.rs`):
/// `run_proc`'s every `interp.call(&step_fn, ...)` (describe at spawn,
/// init, and each message's transform) goes through it unmodified.
#[test]
fn flow_process_accepts_a_var_launcher_and_behaves_like_the_bare_fn() {
    let result = ps(
        r#"(let [doubler (flow/map->step
                           {:describe (fn [] {:ins {:in {}} :outs {:out {}}})
                            :transform (fn [s _ m] [s {:out [(* 2 m)]}])})
                  sink-ch (chan 200)
                  sink (flow/map->step
                        {:describe (fn [] {:ins {:in {}} :outs {}})
                         :transform (fn [s _ m] (>!! sink-ch m) [s {}])})]
              (def r2-doubler doubler)
              (def r2-sink sink)
              (let [fl (flow/create-flow
                        {:procs {:d {:proc (flow/process #'r2-doubler)}
                                 :s {:proc (flow/process #'r2-sink)}}
                         :conns [[[:d :out] [:s :in]]]})
                    _ (flow/start fl)
                    _ (flow/resume fl)
                    _ (flow/inject fl [:d :in] (range 100))
                    results (loop [n 100 acc []]
                              (if (zero? n)
                                acc
                                (recur (dec n) (conj acc (<!! sink-ch)))))
                    expected (vec (map #(* 2 %) (range 100)))]
                (flow/stop fl)
                (= results expected)))"#,
    );
    assert_eq!(result, "true");
}

/// The one thing a var launcher does DIFFERENTLY from a bare fn: it is
/// late-bound. `flow/process #'r2-late` captures the launcher map once, at
/// `create-flow` time, holding the VAR (not a snapshot of its
/// then-current value) -- so redefining `r2-late` any time before the proc
/// actually runs changes what runs, exactly like a real Clojure var-backed
/// step would and exactly UNLIKE a bare-fn launcher (which closes over the
/// value it had at `flow/process` call time and can never see a later
/// redefinition).
#[test]
fn flow_process_var_launcher_is_late_bound_through_redefinition() {
    let result = ps(
        r#"(def r2-late (flow/map->step
                          {:describe (fn [] {:ins {:in {}} :outs {:out {}}})
                           :transform (fn [s _ m] [s {:out [(* 2 m)]}])}))
           (def sink-ch2 (chan 10))
           (def r2-sink2 (flow/map->step
                          {:describe (fn [] {:ins {:in {}} :outs {}})
                           :transform (fn [s _ m] (>!! sink-ch2 m) [s {}])}))
           (let [fl (flow/create-flow
                     {:procs {:d {:proc (flow/process #'r2-late)}
                              :s {:proc (flow/process r2-sink2)}}
                      :conns [[[:d :out] [:s :in]]]})]
             ;; Redefine AFTER the launcher was captured by create-flow but
             ;; BEFORE the proc ever runs: a bare-fn launcher could not
             ;; observe this at all.
             (def r2-late (flow/map->step
                           {:describe (fn [] {:ins {:in {}} :outs {:out {}}})
                            :transform (fn [s _ m] [s {:out [(* 10 m)]}])}))
             (flow/start fl)
             (flow/resume fl)
             (flow/inject fl [:d :in] [5])
             (let [v (<!! sink-ch2)]
               (flow/stop fl)
               v))"#,
    );
    assert_eq!(result, "50");
}

// ---------------------------------------------------------------------------
// transform error: error-chan shape, KEEP previous state, proc continues
// ---------------------------------------------------------------------------

#[test]
fn transform_error_emits_shaped_map_keeps_state_and_continues() {
    let result = eval_ok(
        r#"(let [step (flow/map->step
                        {:describe (fn [] {:ins {:in {}} :outs {:out {}}})
                         :init (fn [_] {:n 0})
                         :transform (fn [s _ m]
                                      (if (= m :boom)
                                        (throw "kaboom")
                                        (let [s2 (update s :n inc)]
                                          [s2 {:out [(:n s2)]}])))})
                  sink-ch (chan 10)
                  sink (flow/map->step
                        {:describe (fn [] {:ins {:in {}} :outs {}})
                         :transform (fn [s _ m] (>!! sink-ch m) [s {}])})
                  fl (flow/create-flow
                      {:procs {:e {:proc (flow/process step)}
                               :s {:proc (flow/process sink)}}
                       :conns [[[:e :out] [:s :in]]]})
                  chans (flow/start fl)
                  _ (flow/resume fl)
                  _ (flow/inject fl [:e :in] [1 :boom 2])
                  v1 (<!! sink-ch)
                  err (<!! (:error-chan chans))
                  v2 (<!! sink-ch)]
              (flow/stop fl)
              {:v1 v1
               :v2 v2
               :has-pid (= :e (:clojure.core.async.flow/pid err))
               :has-cid (= :in (:clojure.core.async.flow/cid err))
               :has-msg (= :boom (:clojure.core.async.flow/msg err))
               :has-ex (map? (:clojure.core.async.flow/ex err))})"#,
    );
    let Value::Map(m) = &result else { panic!("expected a map, got {result:?}") };
    let get = |k: &str| m.get(&Value::Keyword(k.into())).cloned().unwrap();
    assert_eq!(get("v1"), Value::Int(1));
    // v2 is 2: state (:n) was kept at 1 across the error, so the next
    // successful message increments from 1 -> 2, not restarted from 0.
    assert_eq!(get("v2"), Value::Int(2));
    assert_eq!(get("has-pid"), Value::Bool(true));
    assert_eq!(get("has-cid"), Value::Bool(true));
    assert_eq!(get("has-msg"), Value::Bool(true));
    assert_eq!(get("has-ex"), Value::Bool(true));
}

// ---------------------------------------------------------------------------
// pause / resume: stops consumption, then continues losslessly
// ---------------------------------------------------------------------------

#[test]
fn pause_mid_stream_stops_consumption_then_resume_continues_losslessly() {
    let result = ps(
        r#"(let [relay (flow/map->step
                         {:describe (fn [] {:ins {:in {}} :outs {:out {}}})
                          :transform (fn [s _ m] [s {:out [m]}])})
                  sink-ch (chan 50)
                  sink (flow/map->step
                        {:describe (fn [] {:ins {:in {}} :outs {}})
                         :transform (fn [s _ m] (>!! sink-ch m) [s {}])})
                  fl (flow/create-flow
                      {:procs {:r {:proc (flow/process relay)}
                               :s {:proc (flow/process sink)}}
                       :conns [[[:r :out] [:s :in]]]})
                  _ (flow/start fl)
                  _ (flow/resume fl)
                  _ (flow/inject fl [:r :in] [1 2 3])
                  first-batch [(<!! sink-ch) (<!! sink-ch) (<!! sink-ch)]
                  ;; Proc is now idle (no in-flight take racing an arriving
                  ;; message) -- pause here is unambiguous, not raced.
                  _ (flow/pause fl)
                  _ (sleep-ms 50)
                  _ (flow/inject fl [:r :in] [4 5 6])
                  _ (sleep-ms 100)
                  blocked (poll! sink-ch)
                  _ (flow/resume fl)
                  second-batch [(<!! sink-ch) (<!! sink-ch) (<!! sink-ch)]]
              (flow/stop fl)
              [first-batch blocked second-batch])"#,
    );
    assert_eq!(result, "[[1 2 3] nil [4 5 6]]");
}

// ---------------------------------------------------------------------------
// ping / ping-proc
// ---------------------------------------------------------------------------

#[test]
fn ping_returns_status_and_count_for_both_procs_after_known_messages() {
    let result = eval_ok(
        r#"(let [relay (flow/map->step
                         {:describe (fn [] {:ins {:in {}} :outs {:out {}}})
                          :transform (fn [s _ m] [s {:out [m]}])})
                  sink-ch (chan 10)
                  sink (flow/map->step
                        {:describe (fn [] {:ins {:in {}} :outs {}})
                         :transform (fn [s _ m] (>!! sink-ch m) [s {}])})
                  fl (flow/create-flow
                      {:procs {:r {:proc (flow/process relay)}
                               :s {:proc (flow/process sink)}}
                       :conns [[[:r :out] [:s :in]]]})
                  _ (flow/start fl)
                  _ (flow/resume fl)
                  _ (flow/inject fl [:r :in] [1 2 3 4 5])
                  _ (do (<!! sink-ch) (<!! sink-ch) (<!! sink-ch) (<!! sink-ch) (<!! sink-ch))
                  reply (flow/ping fl 1000)
                  r-status (get-in reply [:r :clojure.core.async.flow/status])
                  r-count (get-in reply [:r :clojure.core.async.flow/count])
                  s-status (get-in reply [:s :clojure.core.async.flow/status])
                  s-count (get-in reply [:s :clojure.core.async.flow/count])]
              (flow/stop fl)
              {:r-status r-status :r-count r-count :s-status s-status :s-count s-count})"#,
    );
    let Value::Map(m) = &result else { panic!("expected a map, got {result:?}") };
    let get = |k: &str| m.get(&Value::Keyword(k.into())).cloned().unwrap();
    assert_eq!(get("r-status"), Value::Keyword("running".into()));
    assert_eq!(get("r-count"), Value::Int(5));
    assert_eq!(get("s-status"), Value::Keyword("running".into()));
    assert_eq!(get("s-count"), Value::Int(5));
}

#[test]
fn ping_proc_returns_single_procs_status() {
    let result = eval_ok(
        r#"(let [relay (flow/map->step
                         {:describe (fn [] {:ins {:in {}} :outs {:out {}}})
                          :transform (fn [s _ m] [s {:out [m]}])})
                  sink-ch (chan 10)
                  sink (flow/map->step
                        {:describe (fn [] {:ins {:in {}} :outs {}})
                         :transform (fn [s _ m] (>!! sink-ch m) [s {}])})
                  fl (flow/create-flow
                      {:procs {:r {:proc (flow/process relay)}
                               :s {:proc (flow/process sink)}}
                       :conns [[[:r :out] [:s :in]]]})
                  _ (flow/start fl)
                  _ (flow/resume fl)
                  _ (flow/inject fl [:r :in] [1 2])
                  _ (do (<!! sink-ch) (<!! sink-ch))
                  reply (flow/ping-proc fl :r 1000)]
              (flow/stop fl)
              reply)"#,
    );
    let Value::Map(m) = &result else { panic!("expected a map, got {result:?}") };
    assert_eq!(m.get(&Value::Keyword("clojure.core.async.flow/pid".into())), Some(&Value::Keyword("r".into())));
    assert_eq!(m.get(&Value::Keyword("clojure.core.async.flow/count".into())), Some(&Value::Int(2)));
    assert_eq!(
        m.get(&Value::Keyword("clojure.core.async.flow/status".into())),
        Some(&Value::Keyword("running".into()))
    );
}

// ---------------------------------------------------------------------------
// stop: transition observed, threads exit, idempotent
// ---------------------------------------------------------------------------

#[test]
fn stop_runs_the_stop_transition_observably() {
    // The step-fn's transition (arity-2) writes a marker to an out-port
    // side-channel when it sees ::flow/stop -- the only externally
    // observable proof `stop` actually ran the lifecycle transition (as
    // opposed to just killing the thread).
    let result = ps(
        r#"(let [marker-ch (chan 10)
                  step (flow/map->step
                        {:describe (fn [] {:ins {:in {}} :outs {}})
                         :transition (fn [s t]
                                       (when (= t :clojure.core.async.flow/stop)
                                         (>!! marker-ch :stopped))
                                       s)
                         :transform (fn [s _ m] [s {}])})
                  fl (flow/create-flow {:procs {:p {:proc (flow/process step)}} :conns []})
                  _ (flow/start fl)
                  _ (flow/resume fl)]
              (flow/stop fl)
              (<!! marker-ch))"#,
    );
    assert_eq!(result, ":stopped");
}

#[test]
fn stop_is_idempotent_and_flow_prints_stopped() {
    let result = ps(
        r#"(let [step (flow/map->step
                        {:describe (fn [] {:ins {} :outs {}})
                         :transform (fn [s _ m] [s {}])})
                  fl (flow/create-flow {:procs {:p {:proc (flow/process step)}} :conns []})]
              (flow/start fl)
              (flow/stop fl)
              (flow/stop fl)
              (pr-str fl))"#,
    );
    assert_eq!(result, "\"#<flow stopped>\"");
}

/// Design Part 3 item 3: `stop` closes `report_chan`/`error_chan` as its
/// LAST act (`stop_flow_cell`'s step 5, `src/builtins/flow.rs`), reversing
/// the earlier documented "leave them open" deviation now that Part 2
/// makes both chans always-wired out-targets. `<!!` on a closed, drained
/// `Chan` returns `nil` immediately (never a hang) -- flow-gold scenario
/// 13's exact contract (`tests/conformance/flow-gold/SPEC.md:152`).
#[test]
fn stop_closes_both_report_and_error_chans() {
    let result = ps(
        r#"(let [step (flow/map->step
                        {:describe (fn [] {:ins {} :outs {}})
                         :transform (fn [s _ m] [s {}])})
                  fl (flow/create-flow {:procs {:p {:proc (flow/process step)}} :conns []})
                  chans (flow/start fl)]
              (flow/stop fl)
              [(<!! (:report-chan chans)) (<!! (:error-chan chans))])"#,
    );
    assert_eq!(result, "[nil nil]");
}

// ---------------------------------------------------------------------------
// fan-out / mult
// ---------------------------------------------------------------------------

#[test]
fn fan_out_delivers_all_messages_to_both_sinks_in_order() {
    let result = ps(
        r#"(let [src (flow/map->step
                       {:describe (fn [] {:ins {:in {}} :outs {:out {}}})
                        :transform (fn [s _ m] [s {:out [m]}])})
                  a-ch (chan 20) b-ch (chan 20)
                  sink-a (flow/map->step
                          {:describe (fn [] {:ins {:in {}} :outs {}})
                           :transform (fn [s _ m] (>!! a-ch m) [s {}])})
                  sink-b (flow/map->step
                          {:describe (fn [] {:ins {:in {}} :outs {}})
                           :transform (fn [s _ m] (>!! b-ch m) [s {}])})
                  fl (flow/create-flow
                      {:procs {:src {:proc (flow/process src)}
                               :a {:proc (flow/process sink-a)}
                               :b {:proc (flow/process sink-b)}}
                       :conns [[[:src :out] [:a :in]]
                               [[:src :out] [:b :in]]]})
                  _ (flow/start fl)
                  _ (flow/resume fl)
                  _ (flow/inject fl [:src :in] [1 2 3 4 5])
                  a-results (loop [n 5 acc []] (if (zero? n) acc (recur (dec n) (conj acc (<!! a-ch)))))
                  b-results (loop [n 5 acc []] (if (zero? n) acc (recur (dec n) (conj acc (<!! b-ch)))))]
              (flow/stop fl)
              [a-results b-results])"#,
    );
    assert_eq!(result, "[[1 2 3 4 5] [1 2 3 4 5]]");
}

#[test]
fn mult_with_a_slow_consumer_still_eventually_delivers_to_the_fast_one() {
    // sink-b never reads while 15 messages (> the default buf-or-n of 10)
    // are injected, forcing the mult thread to block on B's full buffer;
    // sink-a's own fast-pass try_puts still land as far as the mult thread
    // gets before stalling. Once B is drained, both sinks eventually
    // receive the full, correctly-ordered sequence -- proof B being slow
    // never causes a drop, a deadlock, or A being starved forever.
    let result = ps(
        r#"(let [src (flow/map->step
                       {:describe (fn [] {:ins {:in {}} :outs {:out {}}})
                        :transform (fn [s _ m] [s {:out [m]}])})
                  a-ch (chan 20) b-ch (chan 20)
                  sink-a (flow/map->step
                          {:describe (fn [] {:ins {:in {}} :outs {}})
                           :transform (fn [s _ m] (>!! a-ch m) [s {}])})
                  sink-b (flow/map->step
                          {:describe (fn [] {:ins {:in {}} :outs {}})
                           :transform (fn [s _ m] (>!! b-ch m) [s {}])})
                  fl (flow/create-flow
                      {:procs {:src {:proc (flow/process src)}
                               :a {:proc (flow/process sink-a)}
                               :b {:proc (flow/process sink-b)}}
                       :conns [[[:src :out] [:a :in]]
                               [[:src :out] [:b :in]]]})
                  _ (flow/start fl)
                  _ (flow/resume fl)
                  _ (flow/inject fl [:src :in] (range 15))
                  _ (sleep-ms 100)
                  b-results (loop [n 15 acc []] (if (zero? n) acc (recur (dec n) (conj acc (<!! b-ch)))))
                  a-results (loop [n 15 acc []] (if (zero? n) acc (recur (dec n) (conj acc (<!! a-ch)))))
                  expected (vec (range 15))]
              (flow/stop fl)
              [(= a-results expected) (= b-results expected)])"#,
    );
    assert_eq!(result, "[true true]");
}

// ---------------------------------------------------------------------------
// self-loop
// ---------------------------------------------------------------------------

#[test]
fn self_loop_proc_feeds_itself_n_times_then_emits() {
    let result = ps(
        r#"(let [out-ch (chan 10)
                  step (flow/map->step
                        {:describe (fn [] {:ins {:in {} :self-in {}} :outs {:self-out {} :out {}}})
                         :transform (fn [s cid m]
                                      (if (= cid :self-in)
                                        (if (< m 5)
                                          [s {:self-out [(inc m)]}]
                                          [s {:out [m]}])
                                        [s {:self-out [m]}]))})
                  sink (flow/map->step
                        {:describe (fn [] {:ins {:in {}} :outs {}})
                         :transform (fn [s _ m] (>!! out-ch m) [s {}])})
                  fl (flow/create-flow
                      {:procs {:l {:proc (flow/process step)} :s {:proc (flow/process sink)}}
                       :conns [[[:l :self-out] [:l :self-in]]
                               [[:l :out] [:s :in]]]})
                  _ (flow/start fl)
                  _ (flow/resume fl)
                  _ (flow/inject fl [:l :in] [0])
                  result (<!! out-ch)]
              (flow/stop fl)
              result)"#,
    );
    assert_eq!(result, "5");
}

// ---------------------------------------------------------------------------
// input-filter
// ---------------------------------------------------------------------------

#[test]
fn input_filter_restricts_reads_to_one_of_two_ins() {
    let result = ps(
        r#"(let [out-ch (chan 10)
                  step (flow/map->step
                        {:describe (fn [] {:ins {:a {} :b {}} :outs {}})
                         :init (fn [_] {:clojure.core.async.flow/input-filter (fn [cid] (= cid :a))})
                         :transform (fn [s cid m] (>!! out-ch [cid m]) [s {}])})
                  fl (flow/create-flow {:procs {:f {:proc (flow/process step)}} :conns []})
                  _ (flow/start fl)
                  _ (flow/resume fl)
                  _ (flow/inject fl [:f :a] [1 2])
                  _ (flow/inject fl [:f :b] [99])
                  v1 (<!! out-ch)
                  v2 (<!! out-ch)
                  _ (sleep-ms 100)
                  blocked (poll! out-ch)]
              (flow/stop fl)
              [v1 v2 blocked])"#,
    );
    assert_eq!(result, "[[:a 1] [:a 2] nil]");
}

// ---------------------------------------------------------------------------
// external in-ports / out-ports
// ---------------------------------------------------------------------------

#[test]
fn external_in_ports_and_out_ports_from_init_are_wired() {
    let result = ps(
        r#"(let [external-in (chan 10)
                  external-out (chan 10)
                  step (flow/map->step
                        {:describe (fn [] {:ins {:in {}} :outs {:out {}}})
                         :init (fn [_] {:clojure.core.async.flow/in-ports {:in external-in}
                                        :clojure.core.async.flow/out-ports {:out external-out}})
                         :transform (fn [s _ m] [s {:out [(* 10 m)]}])})
                  fl (flow/create-flow {:procs {:p {:proc (flow/process step)}} :conns []})
                  _ (flow/start fl)
                  _ (flow/resume fl)
                  _ (>!! external-in 7)
                  result (<!! external-out)]
              (flow/stop fl)
              result)"#,
    );
    assert_eq!(result, "70");
}

// ---------------------------------------------------------------------------
// deep pipeline
// ---------------------------------------------------------------------------

#[test]
fn ten_hop_deep_pipeline_delivers_1000_messages_in_order() {
    let result = ps(
        r#"(let [relay (flow/map->step
                         {:describe (fn [] {:ins {:in {}} :outs {:out {}}})
                          :transform (fn [s _ m] [s {:out [m]}])})
                  pids (map #(keyword (str "r" %)) (range 10))
                  mkproc (fn [_] {:proc (flow/process relay)})
                  base-procs (into {} (map (fn [p] [p (mkproc p)]) pids))
                  pairs (map vector pids (rest pids))
                  base-conns (map (fn [[a b]] [[a :out] [b :in]]) pairs)
                  out-ch (chan 2000)
                  sink (flow/map->step
                        {:describe (fn [] {:ins {:in {}} :outs {}})
                         :transform (fn [s _ m] (>!! out-ch m) [s {}])})
                  procs (assoc base-procs :sink {:proc (flow/process sink)})
                  conns (conj (vec base-conns) [[(last pids) :out] [:sink :in]])
                  fl (flow/create-flow {:procs procs :conns conns})
                  _ (flow/start fl)
                  _ (flow/resume fl)
                  _ (flow/inject fl [(first pids) :in] (range 1000))
                  results (loop [n 1000 acc []]
                            (if (zero? n) acc (recur (dec n) (conj acc (<!! out-ch)))))
                  expected (vec (range 1000))]
              (flow/stop fl)
              (= results expected))"#,
    );
    assert_eq!(result, "true");
}

// ---------------------------------------------------------------------------
// unconnected output
// ---------------------------------------------------------------------------

#[test]
fn unconnected_output_silently_drops() {
    let result = ps(
        r#"(let [out-ch (chan 10)
                  step (flow/map->step
                        {:describe (fn [] {:ins {:in {}} :outs {:out {} :dead-end {}}})
                         :transform (fn [s _ m] [s {:out [m] :dead-end [:nobody-home]}])})
                  sink (flow/map->step
                        {:describe (fn [] {:ins {:in {}} :outs {}})
                         :transform (fn [s _ m] (>!! out-ch m) [s {}])})
                  fl (flow/create-flow
                      {:procs {:u {:proc (flow/process step)} :s {:proc (flow/process sink)}}
                       :conns [[[:u :out] [:s :in]]]})
                  _ (flow/start fl)
                  _ (flow/resume fl)
                  _ (flow/inject fl [:u :in] [:hi])
                  result (<!! out-ch)]
              (flow/stop fl)
              result)"#,
    );
    assert_eq!(result, ":hi");
}

// ---------------------------------------------------------------------------
// misc: predicate, inject's future
// ---------------------------------------------------------------------------

#[test]
fn flow_predicate_distinguishes_flow_values() {
    let result = ps(
        r#"(let [step (flow/map->step {:describe (fn [] {:ins {} :outs {}}) :transform (fn [s _ m] [s {}])})
                  fl (flow/create-flow {:procs {:p {:proc (flow/process step)}} :conns []})]
              [(flow? fl) (flow? 42) (flow? nil) (flow? (chan))])"#,
    );
    assert_eq!(result, "[true false false false]");
}

#[test]
fn inject_returns_a_future_that_resolves_after_the_puts_land() {
    let result = ps(
        r#"(let [step (flow/map->step
                        {:describe (fn [] {:ins {:in {}} :outs {}})
                         :transform (fn [s _ m] [s {}])})
                  fl (flow/create-flow {:procs {:p {:proc (flow/process step)}} :conns []})
                  _ (flow/start fl)
                  _ (flow/resume fl)
                  fut (flow/inject fl [:p :in] [1 2 3])
                  resolved (deref fut 2000 :timed-out)]
              (flow/stop fl)
              [(future? fut) resolved])"#,
    );
    assert_eq!(result, "[true nil]");
}

// ---------------------------------------------------------------------------
// Phase F2 hardening (see PLAN.md's F2 packet): 8 tests probing edges the
// conformance corpus (deterministic, JVM-portable scenarios only) can't
// reach -- backpressure timing, thread-join boundedness, and mova-only
// API surface (positional ping timeouts, chan-opts, restart semantics).
// ---------------------------------------------------------------------------

#[test]
fn control_priority_pauses_within_bounded_messages_under_backpressure_then_resumes_losslessly() {
    // sink's `:in` is deliberately sized to 1 (chan-opts belongs to the
    // DOWNSTREAM reader per FLOW-DESIGN.md), so a 200-message flood via
    // `inject` immediately backpressures the whole 1:1 pipeline: relay can
    // have at most one message "in hand" past what sink has already
    // buffered. `pause` is issued almost immediately after `inject`
    // returns (inject itself is async -- the flood is still in flight on
    // its own thread) -- proving control priority actually interrupts an
    // in-progress flood rather than "eventually noticing" only once
    // everything has already drained. After a generous settle window we
    // assert strictly fewer than all 200 messages arrived (pause held),
    // then `resume` and confirm the REST arrive, and the concatenation of
    // both phases is the exact, in-order 0..200 sequence (lossless: pause
    // never drops or reorders anything already in flight).
    let result = ps(
        r#"(let [relay (flow/map->step
                         {:describe (fn [] {:ins {:in {}} :outs {:out {}}})
                          :transform (fn [s _ m] [s {:out [m]}])})
                  sink-ch (chan 300)
                  sink (flow/map->step
                        {:describe (fn [] {:ins {:in {}} :outs {}})
                         :transform (fn [s _ m] (>!! sink-ch m) [s {}])})
                  fl (flow/create-flow
                      {:procs {:r {:proc (flow/process relay)}
                               :s {:proc (flow/process sink) :chan-opts {:in {:buf-or-n 1}}}}
                       :conns [[[:r :out] [:s :in]]]})
                  _ (flow/start fl)
                  _ (flow/resume fl)
                  _ (flow/inject fl [:r :in] (range 200))
                  _ (flow/pause fl)
                  _ (sleep-ms 300)
                  before (loop [acc []]
                           (let [v (poll! sink-ch)]
                             (if (nil? v) acc (recur (conj acc v)))))
                  bounded (< (count before) 200)
                  _ (flow/resume fl)
                  after (loop [acc [] n (- 200 (count before))]
                          (if (zero? n) acc (recur (conj acc (<!! sink-ch)) (dec n))))
                  all (into before after)
                  expected (vec (range 200))]
              (flow/stop fl)
              [bounded (= all expected)])"#,
    );
    assert_eq!(result, "[true true]");
}

#[test]
fn fan_out_keeps_delivering_to_live_dests_when_another_dest_is_permanently_stuck() {
    // mova's mult (fan-out) dest channels are fully engine-owned -- there
    // is no public API to close one from outside a running flow, so this
    // exercises the SAME resilience guarantee (`run_mult_thread`'s "never
    // crash broadcast, untap only the dest that stopped accepting" -- see
    // flow.rs's module doc) the only way reachable from the public API:
    // pausing one of three fan-out dests FOREVER (never resumed) so its
    // buffer fills and the mult thread permanently blocks trying to
    // deliver to it. The other two dests must still receive every message,
    // in order, undisturbed by :stuck's predicament, and `stop` (which
    // force-closes every engine-owned chan, including :stuck's full
    // buffer) must still unstick the mult thread and return promptly
    // rather than hang -- the real-world trigger for the `TryPut::Closed`
    // arm this test's name refers to.
    let result = ps(
        r#"(let [src (flow/map->step
                       {:describe (fn [] {:ins {:in {}} :outs {:out {}}})
                        :transform (fn [s _ m] [s {:out [m]}])})
                  a-ch (chan 20) c-ch (chan 20)
                  sink-a (flow/map->step
                          {:describe (fn [] {:ins {:in {}} :outs {}})
                           :transform (fn [s _ m] (>!! a-ch m) [s {}])})
                  sink-c (flow/map->step
                          {:describe (fn [] {:ins {:in {}} :outs {}})
                           :transform (fn [s _ m] (>!! c-ch m) [s {}])})
                  stuck (flow/map->step
                         {:describe (fn [] {:ins {:in {}} :outs {}})
                          :transform (fn [s _ m] [s {}])})
                  fl (flow/create-flow
                      {:procs {:src {:proc (flow/process src)}
                               :a {:proc (flow/process sink-a)}
                               :c {:proc (flow/process sink-c)}
                               :stuck {:proc (flow/process stuck)}}
                       :conns [[[:src :out] [:a :in]]
                               [[:src :out] [:c :in]]
                               [[:src :out] [:stuck :in]]]})
                  _ (flow/start fl)
                  _ (flow/resume fl)
                  _ (flow/pause-proc fl :stuck)
                  _ (flow/inject fl [:src :in] (range 10))
                  a-results (loop [n 10 acc []] (if (zero? n) acc (recur (dec n) (conj acc (<!! a-ch)))))
                  c-results (loop [n 10 acc []] (if (zero? n) acc (recur (dec n) (conj acc (<!! c-ch)))))
                  t0 (time-ms)
                  _ (flow/stop fl)
                  stop-elapsed (- (time-ms) t0)
                  expected (vec (range 10))]
              [(= a-results expected) (= c-results expected) (< stop-elapsed 4000)])"#,
    );
    assert_eq!(result, "[true true true]");
}

#[test]
fn stop_during_heavy_traffic_joins_within_a_bounded_time() {
    // Every relay stage's `:in` is sized to 5 (chan-opts belongs to the
    // DOWNSTREAM reader) -- with 3000 messages flooded into a 5-hop
    // pipeline, every one of those ENGINE-internal buffers fills almost
    // instantly, so every proc thread is genuinely mid-blocked-send (the
    // control-priority-aware retry loop in `process_message`) when `stop`
    // is called, not idly parked. `sink-ch` itself (the side-channel
    // sink's `transform` observes into) is sized to hold every message
    // that could possibly reach it, so sink's OWN interpreted step-fn call
    // never blocks -- that would be a block inside USER code the engine
    // has no visibility into at all (a real, documented architectural
    // boundary, not something `stop` could ever preempt), which is a
    // different failure mode than the one this test targets: the ENGINE's
    // own internal backpressure. `stop` must return well under its own
    // STOP_JOIN_TIMEOUT (5s, see flow.rs) rather than hang or silently
    // detach any proc -- "joins cleanly" means every proc thread actually
    // observes the control-priority stop and exits, not just "the test
    // process didn't deadlock forever".
    let result = ps(
        r#"(let [relay (flow/map->step
                         {:describe (fn [] {:ins {:in {}} :outs {:out {}}})
                          :transform (fn [s _ m] [s {:out [m]}])})
                  pids [:r1 :r2 :r3 :r4]
                  sink-ch (chan 3000)
                  sink (flow/map->step
                        {:describe (fn [] {:ins {:in {}} :outs {}})
                         :transform (fn [s _ m] (>!! sink-ch m) [s {}])})
                  procs (assoc (into {} (map (fn [p] [p {:proc (flow/process relay) :chan-opts {:in {:buf-or-n 5}}}]) pids))
                               :s {:proc (flow/process sink) :chan-opts {:in {:buf-or-n 5}}})
                  conns (conj (vec (map (fn [[a b]] [[a :out] [b :in]]) (map vector pids (rest pids))))
                              [[(last pids) :out] [:s :in]])
                  fl (flow/create-flow {:procs procs :conns conns})
                  _ (flow/start fl)
                  _ (flow/resume fl)
                  _ (flow/inject fl [(first pids) :in] (range 3000))
                  _ (sleep-ms 30)
                  t0 (time-ms)
                  _ (flow/stop fl)
                  elapsed (- (time-ms) t0)]
              (< elapsed 4000))"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn inject_future_only_resolves_once_a_large_backpressured_batch_fully_lands() {
    // sink's `:in` is sized to 2 -- far smaller than the 50-message batch
    // -- so `inject`'s blocking-put loop genuinely backpressures against
    // sink's own consumption speed. This strengthens
    // `inject_returns_a_future_that_resolves_after_the_puts_land` (a
    // 3-message batch that fits the DEFAULT buf-or-n of 10 outright,
    // resolving near-instantly with no real backpressure at all): here the
    // future must NOT resolve until every one of the 50 puts has actually
    // landed, which is only possible once sink has drained enough of them.
    let result = ps(
        r#"(let [big-ch (chan 100)
                  sink (flow/map->step
                        {:describe (fn [] {:ins {:in {}} :outs {}})
                         :transform (fn [s _ m] (>!! big-ch m) [s {}])})
                  fl (flow/create-flow
                      {:procs {:s {:proc (flow/process sink) :chan-opts {:in {:buf-or-n 2}}}}
                       :conns []})
                  _ (flow/start fl)
                  _ (flow/resume fl)
                  fut (flow/inject fl [:s :in] (range 50))
                  resolved (deref fut 3000 :timed-out)
                  results (loop [n 50 acc []] (if (zero? n) acc (recur (dec n) (conj acc (<!! big-ch)))))
                  expected (vec (range 50))]
              (flow/stop fl)
              [(future? fut) resolved (= results expected)])"#,
    );
    assert_eq!(result, "[true nil true]");
}

#[test]
fn starting_a_stopped_flow_again_errors_restart_is_not_supported() {
    // Pins F1's actual (documented) behavior: `native_start` only accepts
    // a flow in `FlowPhase::Created`; after `stop` the phase is
    // `FlowPhase::Stopped`, never back to `Created`, so a second `start`
    // always errors rather than silently restarting stopped procs.
    let msg = eval_err(
        r#"(let [step (flow/map->step {:describe (fn [] {:ins {} :outs {}}) :transform (fn [s _ m] [s {}])})
                  fl (flow/create-flow {:procs {:p {:proc (flow/process step)}} :conns []})]
              (flow/start fl)
              (flow/stop fl)
              (flow/start fl))"#,
    );
    assert!(msg.contains("already been started"), "message was: {msg}");
}

#[test]
fn ping_on_a_stopped_flow_errors_rather_than_returning_a_reply() {
    // Also pins actual F1 behavior (the counterpart to the restart test
    // above): `flow/ping`/`flow/ping-proc` require `FlowPhase::Running`,
    // so pinging a stopped flow errors outright -- there is no "empty
    // reply map" case for a fully-stopped flow (that shape only exists
    // for individual pids that didn't answer a ping in time on a flow
    // that IS still running -- see the next test).
    let msg = eval_err(
        r#"(let [step (flow/map->step {:describe (fn [] {:ins {} :outs {}}) :transform (fn [s _ m] [s {}])})
                  fl (flow/create-flow {:procs {:p {:proc (flow/process step)}} :conns []})]
              (flow/start fl)
              (flow/stop fl)
              (flow/ping fl))"#,
    );
    assert!(msg.contains("not running"), "message was: {msg}");
}

#[test]
fn ping_proc_times_out_absent_reply_while_target_is_busy_in_a_slow_transform() {
    // :slow's transform blocks synchronously for 500ms; a ping-proc issued
    // 50ms into that window with a 100ms timeout cannot possibly be
    // answered (control commands are only checked at specific
    // checkpoints -- never preemptively mid-`transform`-call, see flow.rs's
    // module doc), so it must come back `nil` (ping-proc's documented
    // timeout sentinel) rather than block until the transform finishes.
    // `flow/ping` (the all-procs form) must likewise simply omit :slow's
    // entry from the reply map (documented: "no :timeout sentinel in v0 --
    // pids that didn't answer in time are simply absent").
    let result = eval_ok(
        r#"(let [slow (flow/map->step
                        {:describe (fn [] {:ins {:in {}} :outs {}})
                         :transform (fn [s _ m] (sleep-ms 500) [s {}])})
                  fl (flow/create-flow {:procs {:slow {:proc (flow/process slow)}} :conns []})
                  _ (flow/start fl)
                  _ (flow/resume fl)
                  _ (flow/inject fl [:slow :in] [1])
                  _ (sleep-ms 50)
                  proc-reply (flow/ping-proc fl :slow 100)
                  all-reply (flow/ping fl 100)]
              (flow/stop fl)
              [proc-reply (contains? all-reply :slow)])"#,
    );
    assert_eq!(mova::internal::pr_str(&result), "[nil false]");
}

#[test]
fn error_chan_sliding_overflow_never_blocks_the_proc_from_processing_real_data() {
    // error-chan is a sliding buffer of 100 (see flow.rs's DIAG_BUF) that
    // this test NEVER drains -- 300 back-to-back transform errors (3x its
    // capacity) must silently drop the oldest ones rather than ever block
    // `chan_put`, so a real message injected right after the flood still
    // gets processed promptly. State (:n) is untouched by every errored
    // message (KEEP-previous-state-on-error, already covered by the
    // conformance corpus's error-chan scenario), so the final successful
    // message increments from 0 straight to 1.
    let result = ps(
        r#"(let [step (flow/map->step
                        {:describe (fn [] {:ins {:in {}} :outs {:out {}}})
                         :init (fn [_] {:n 0})
                         :transform (fn [s _ m]
                                      (if (= m :boom)
                                        (throw "boom")
                                        (let [s2 (update s :n inc)] [s2 {:out [(:n s2)]}])))})
                  sink-ch (chan 10)
                  sink (flow/map->step
                        {:describe (fn [] {:ins {:in {}} :outs {}})
                         :transform (fn [s _ m] (>!! sink-ch m) [s {}])})
                  fl (flow/create-flow
                      {:procs {:e {:proc (flow/process step)} :s {:proc (flow/process sink)}}
                       :conns [[[:e :out] [:s :in]]]})
                  _ (flow/start fl)
                  _ (flow/resume fl)
                  _ (flow/inject fl [:e :in] (repeat 300 :boom))
                  _ (flow/inject fl [:e :in] [:go])
                  result (<!! sink-ch)]
              (flow/stop fl)
              result)"#,
    );
    assert_eq!(result, "1");
}

#[test]
fn diamond_topology_fan_out_then_fan_in_delivers_every_message_via_both_paths() {
    // a -> {b c} -> d: :a's :out fans out (native mult) to :b and :c, each
    // of which forwards 1:1 to one of :d's TWO declared in-ports. :d has
    // no control over which of its two inputs is read first on any given
    // pass (module doc: "multi-input: non-blocking-fair scan"), so the
    // interleaving of the b-path vs c-path deliveries at :d is NOT
    // guaranteed -- the assertion sorts the combined, tagged-by-value
    // result instead of comparing raw arrival order. Every one of the 5
    // injected messages must arrive at :d via BOTH paths (10 total).
    let result = ps(
        r#"(let [a-step (flow/map->step
                          {:describe (fn [] {:ins {:in {}} :outs {:out {}}})
                           :transform (fn [s _ m] [s {:out [m]}])})
                  pass (fn [in out] (flow/map->step
                                      {:describe (fn [] {:ins {in {}} :outs {out {}}})
                                       :transform (fn [s _ m] [s {out [m]}])}))
                  b-step (pass :in :out)
                  c-step (pass :in :out)
                  d-ch (chan 20)
                  d-step (flow/map->step
                          {:describe (fn [] {:ins {:in-b {} :in-c {}} :outs {}})
                           :transform (fn [s _ m] (>!! d-ch m) [s {}])})
                  fl (flow/create-flow
                      {:procs {:a {:proc (flow/process a-step)}
                               :b {:proc (flow/process b-step)}
                               :c {:proc (flow/process c-step)}
                               :d {:proc (flow/process d-step)}}
                       :conns [[[:a :out] [:b :in]]
                               [[:a :out] [:c :in]]
                               [[:b :out] [:d :in-b]]
                               [[:c :out] [:d :in-c]]]})
                  _ (flow/start fl)
                  _ (flow/resume fl)
                  _ (flow/inject fl [:a :in] [1 2 3 4 5])
                  results (loop [n 10 acc []] (if (zero? n) acc (recur (dec n) (conj acc (<!! d-ch)))))
                  expected (sort (concat (range 1 6) (range 1 6)))]
              (flow/stop fl)
              (= (sort results) expected))"#,
    );
    assert_eq!(result, "true");
}

// ---------------------------------------------------------------------------
// N2 native step tier (NATIVE-STEP-DESIGN.md): the 4-arity shells, the
// fast-path differentials, and the MOVA_NO_FASTSTEP kill switch.
//
// NOTE on the missing error-shape differential: N2's catalog
// (step-passthrough / step-source / step-sink-deliver) is CLOSED -- no
// user fn runs inside any of their transforms, and none of them has a
// fallible operation at all, so there is no way to make a promoted
// `transform` fail from mova code in this stage. The fast path's error
// machinery (`report_transform_error_fast`, sharing
// `report_error_params`'s builder with the generic path, with `:state`
// from a post-failure `snapshot()`) is therefore exercised for the first
// time by N3's `flow/step-map`, where the error-shape differential
// against a `map->step` twin belongs.
// ---------------------------------------------------------------------------

#[test]
fn native_step_shell_arities_dispatch_describe_init_transition_transform() {
    // Every arity of every N2 step, called DIRECTLY (no engine): this is
    // the "shell = fast" contract -- the same `FastStep` code the promoted
    // loop runs, reached through the ordinary 4-arity step-fn interface.
    let result = ps(
        r#"(let [pt (flow/step-passthrough)
                 src (flow/step-source (chan 1))
                 snk (flow/step-sink-deliver 2 (promise))]
             [(pt) (pt {:pid :p}) (pt {:a 1} :clojure.core.async.flow/pause) (pt {:a 1} :in 7)
              (src) (src {})
              (snk) (snk {}) (snk {:count 1} :in :m)])"#,
    );
    assert_eq!(
        result,
        concat!(
            // passthrough: declared ports, empty init state, state threaded
            // through a transition unchanged, message straight to :out
            "[{:ins {:in {}}, :outs {:out {}}} {} {:a 1} [{:a 1} {:out [7]}] ",
            // source: NO declared :ins (its only in-port is the external
            // chan its init supplies via ::flow/in-ports), one :out
            "{:ins {}, :outs {:out {}}} {:clojure.core.async.flow/in-ports {:in #<chan open>}} ",
            // sink-deliver: one :in, no outs, counter state, no output
            "{:ins {:in {}}, :outs {}} {:count 0} [{:count 2} {}]]"
        )
    );
}

#[test]
fn native_step_shell_reports_a_bad_arity_by_name() {
    let msg = eval_err(r#"((flow/step-passthrough) 1 2 3 4)"#);
    assert_eq!(msg, "flow/step-passthrough: called with 4 arguments but expects 0 or 1 or 2 or 3");
}

#[test]
fn native_step_constructors_validate_their_arguments() {
    assert_eq!(eval_err("(flow/step-source 7)"), "flow/step-source: expected a channel, got int");
    assert_eq!(
        eval_err("(flow/step-sink-deliver 1 2)"),
        "flow/step-sink-deliver: expected a promise, got int"
    );
    assert_eq!(eval_err("(flow/feed-range! (chan 1) :x)"), "flow/feed-range!: expected an int n, got keyword");
}

/// The differential: a promoted native pipeline (source -> passthrough ->
/// sink-deliver, every proc running `run_proc_fast`) against the
/// semantically-identical `map->step` pipeline (every proc running the
/// generic interpreted loop), driven by the SAME feeder and compared on
/// everything the engine makes observable: delivered count, per-proc ping
/// `::flow/count`, `::flow/state`, `::flow/ins`/`::flow/outs`, and status.
#[test]
fn native_pipeline_is_observably_identical_to_the_map_to_step_pipeline() {
    let result = eval_ok(
        r#"(let [n 300
                 drive (fn [gen mid snk tick done]
                         (let [fl (flow/create-flow
                                   {:procs {:gen {:proc (flow/process gen)}
                                            :mid {:proc (flow/process mid)}
                                            :sink {:proc (flow/process snk)}}
                                    :conns [[[:gen :out] [:mid :in]]
                                            [[:mid :out] [:sink :in]]]})
                               _ (flow/start fl)
                               _ (flow/resume fl)
                               _ (flow/feed-range! tick n)
                               c (deref done 5000 :timeout)
                               _ (sleep-ms 100)
                               reply (flow/ping fl 2000)
                               at (fn [pid k] (get-in reply [pid k]))]
                           (flow/stop fl)
                           {:delivered c
                            :counts [(at :gen :clojure.core.async.flow/count)
                                     (at :mid :clojure.core.async.flow/count)
                                     (at :sink :clojure.core.async.flow/count)]
                            :statuses [(at :gen :clojure.core.async.flow/status)
                                       (at :mid :clojure.core.async.flow/status)
                                       (at :sink :clojure.core.async.flow/status)]
                            :states [(at :mid :clojure.core.async.flow/state)
                                     (at :sink :clojure.core.async.flow/state)]
                            :ports [(at :gen :clojure.core.async.flow/ins)
                                    (at :gen :clojure.core.async.flow/outs)
                                    (at :mid :clojure.core.async.flow/ins)
                                    (at :sink :clojure.core.async.flow/ins)
                                    (at :sink :clojure.core.async.flow/outs)]}))
                 native (let [tick (chan 64) done (promise)]
                          (drive (flow/step-source tick)
                                 (flow/step-passthrough)
                                 (flow/step-sink-deliver n done)
                                 tick done))
                 interp (let [tick (chan 64) done (promise)]
                          (drive (flow/map->step
                                  {:describe (fn [] {:ins {} :outs {:out {}}})
                                   :init (fn [_] {:clojure.core.async.flow/in-ports {:in tick}})
                                   :transition (fn [s t]
                                                 (when (= t :clojure.core.async.flow/stop) (close! tick))
                                                 s)
                                   :transform (fn [s _ m] [s {:out [m]}])})
                                 (flow/map->step
                                  {:describe (fn [] {:ins {:in {}} :outs {:out {}}})
                                   :init (fn [_] {})
                                   :transform (fn [s _ m] [s {:out [m]}])})
                                 (flow/map->step
                                  {:describe (fn [] {:ins {:in {}} :outs {}})
                                   :init (fn [_] {:count 0})
                                   :transform (fn [s _ m]
                                                (let [c (inc (:count s))]
                                                  (when (= c n) (deliver done c))
                                                  [{:count c} {}]))})
                                 tick done))]
             {:native native :interp interp :same (= native interp)})"#,
    );
    let Value::Map(m) = &result else { panic!("expected a map, got {result:?}") };
    let get = |k: &str| m.get(&Value::Keyword(k.into())).cloned().unwrap();
    // Pin the actual values too, not just "they agree" -- two identically
    // broken pipelines would also agree.
    // W4D-TIERS: expected string was stale sorted-key order, from before a
    // W4 printer fix made small-map INSERTION order visible. The source
    // map literal above is built `{:delivered ... :counts ... :statuses
    // ... :states ... :ports ...}`, so that's the order it prints in now.
    assert_eq!(
        mova::internal::pr_str(&get("native")),
        concat!(
            "{:delivered 300, :counts [300 300 300], ",
            ":statuses [:running :running :running], ",
            ":states [{} {:count 300}], ",
            ":ports [[:in] [:out] [:in] [:in] []]}"
        )
    );
    assert_eq!(get("same"), Value::Bool(true), "native: {:?}\ninterp: {:?}", get("native"), get("interp"));
}

/// Same two pipelines, this time exercising pause/resume against a
/// promoted proc: a paused fast loop must park on control only (consuming
/// nothing), then resume losslessly -- and must do so indistinguishably
/// from the generic loop.
#[test]
fn native_pipeline_pause_resume_matches_the_map_to_step_pipeline() {
    let result = eval_ok(
        r#"(let [n 20
                 drive (fn [gen snk tick done]
                         (let [fl (flow/create-flow
                                   {:procs {:gen {:proc (flow/process gen)}
                                            :sink {:proc (flow/process snk)}}
                                    :conns [[[:gen :out] [:sink :in]]]})
                               _ (flow/start fl)
                               _ (flow/resume fl)
                               _ (flow/feed-range! tick 10)
                               _ (sleep-ms 250)
                               running (flow/ping fl 2000)
                               _ (flow/pause fl)
                               _ (sleep-ms 50)
                               _ (flow/feed-range! tick 10)
                               _ (sleep-ms 250)
                               paused (flow/ping fl 2000)
                               _ (flow/resume fl)
                               delivered (deref done 5000 :timeout)]
                           (flow/stop fl)
                           [(get-in running [:sink :clojure.core.async.flow/count])
                            (get-in paused [:sink :clojure.core.async.flow/count])
                            (get-in paused [:sink :clojure.core.async.flow/status])
                            delivered]))
                 native (let [tick (chan 64) done (promise)]
                          (drive (flow/step-source tick) (flow/step-sink-deliver n done) tick done))
                 interp (let [tick (chan 64) done (promise)]
                          (drive (flow/map->step
                                  {:describe (fn [] {:ins {} :outs {:out {}}})
                                   :init (fn [_] {:clojure.core.async.flow/in-ports {:in tick}})
                                   :transition (fn [s t]
                                                 (when (= t :clojure.core.async.flow/stop) (close! tick))
                                                 s)
                                   :transform (fn [s _ m] [s {:out [m]}])})
                                 (flow/map->step
                                  {:describe (fn [] {:ins {:in {}} :outs {}})
                                   :init (fn [_] {:count 0})
                                   :transform (fn [s _ m]
                                                (let [c (inc (:count s))]
                                                  (when (= c n) (deliver done c))
                                                  [{:count c} {}]))})
                                 tick done))]
             [native interp])"#,
    );
    // 10 consumed while running; still 10 after 10 more were fed to a
    // PAUSED flow; status :paused; all 20 delivered after resume.
    assert_eq!(mova::internal::pr_str(&result), "[[10 10 :paused 20] [10 10 :paused 20]]");
}

#[test]
fn step_source_stop_transition_closes_its_chan_when_the_flow_stops() {
    // The STEP closes its own chan (the engine never touches a
    // user-supplied ::flow/in-ports chan -- see FlowRuntime's doc), which
    // is what releases a producer blocked on a full buffer after `stop`.
    let result = ps(
        r#"(let [tick (chan 1)
                 fl (flow/create-flow
                     {:procs {:gen {:proc (flow/process (flow/step-source tick))}}
                      :conns []})
                 _ (flow/start fl)
                 _ (flow/feed-range! tick 100)
                 _ (sleep-ms 50)
                 before (pr-str tick)
                 _ (flow/stop fl)]
             [before (pr-str tick)])"#,
    );
    assert_eq!(result, r##"["#<chan open>" "#<chan closed>"]"##);
}

/// The kill-switch differential. `MOVA_NO_FASTSTEP` is read ONCE per
/// process (an `OnceLock`, mirroring `MOVA_NO_COMPILE`), so the only
/// honest way to compare the two engine paths is two processes running the
/// same script -- one promoted, one forced onto the generic loop through
/// the very same steps' 4-arity shells.
#[test]
fn fast_path_and_killswitch_subprocesses_print_identical_results() {
    const PROGRAM: &str = r#"(let [n 200
             done (promise)
             tick (chan 64)
             fl (flow/create-flow
                 {:procs {:gen {:proc (flow/process (flow/step-source tick))}
                          :mid {:proc (flow/process (flow/step-passthrough))}
                          :sink {:proc (flow/process (flow/step-sink-deliver n done))}}
                  :conns [[[:gen :out] [:mid :in]] [[:mid :out] [:sink :in]]]})
             _ (flow/start fl)
             _ (flow/resume fl)
             _ (flow/feed-range! tick n)
             delivered (deref done 5000 :timeout)
             _ (sleep-ms 100)
             reply (flow/ping fl 2000)
             at (fn [pid k] (get-in reply [pid k]))]
         (flow/stop fl)
         (println (pr-str [delivered
                           [(at :gen :clojure.core.async.flow/count)
                            (at :mid :clojure.core.async.flow/count)
                            (at :sink :clojure.core.async.flow/count)]
                           [(at :gen :clojure.core.async.flow/state)
                            (at :mid :clojure.core.async.flow/state)
                            (at :sink :clojure.core.async.flow/state)]
                           [(at :gen :clojure.core.async.flow/status)
                            (at :sink :clojure.core.async.flow/ins)
                            (at :sink :clojure.core.async.flow/outs)]])))"#;

    let run = |killswitch: bool| -> String {
        let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_mova"));
        cmd.arg("-e").arg(PROGRAM);
        if killswitch {
            cmd.env("MOVA_NO_FASTSTEP", "1");
        } else {
            cmd.env_remove("MOVA_NO_FASTSTEP");
        }
        let out = cmd.output().expect("failed to run the mova binary");
        assert!(
            out.status.success(),
            "mova exited with {:?}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };

    let fast = run(false);
    let generic = run(true);
    assert_eq!(
        fast,
        concat!(
            "[200 [200 200 200] ",
            // `#<chan closed>`: the printed state is rendered AFTER
            // `flow/stop`, whose ::flow/stop transition is exactly what
            // closes a step-source's own chan (both runs agree, which is
            // the point -- the fast loop's `transition(Stop)` and the
            // shell's arity-2 are the same code).
            "[#:clojure.core.async.flow{:in-ports {:in #<chan closed>}} {} {:count 200}] ",
            "[:running [:in] []]]\nnil"
        ),
        "promoted (fast) run printed something unexpected"
    );
    assert_eq!(fast, generic, "MOVA_NO_FASTSTEP=1 changed observable behavior");
}

// ---------------------------------------------------------------------------
// N3 (NATIVE-STEP-DESIGN.md): the rest of the catalog run through a REAL
// promoted flow (not just the 4-arity shell -- flow_steps.rs's own unit
// tests already cover the shell-level semantics; these confirm the same
// steps behave identically once `run_proc_fast` is actually driving them).
// ---------------------------------------------------------------------------

#[test]
fn native_step_count_full_flow_passes_through_and_counts_every_message() {
    let result = ps(
        r#"(let [sink-ch (chan 10)
                 sink (flow/map->step
                       {:describe (fn [] {:ins {:in {}} :outs {}})
                        :transform (fn [s _ m] (>!! sink-ch m) [s {}])})
                 fl (flow/create-flow
                     {:procs {:c {:proc (flow/process (flow/step-count))}
                              :s {:proc (flow/process sink)}}
                      :conns [[[:c :out] [:s :in]]]})
                 _ (flow/start fl)
                 _ (flow/resume fl)
                 _ (flow/inject fl [:c :in] [10 20 30])
                 results [(<!! sink-ch) (<!! sink-ch) (<!! sink-ch)]
                 reply (flow/ping fl 1000)
                 count (get-in reply [:c :clojure.core.async.flow/count])
                 state (get-in reply [:c :clojure.core.async.flow/state])]
             (flow/stop fl)
             [results count state])"#,
    );
    assert_eq!(result, "[[10 20 30] 3 {:count 3}]");
}

#[test]
fn native_step_sum_full_flow_emits_a_running_total() {
    let result = ps(
        r#"(let [sink-ch (chan 10)
                 sink (flow/map->step
                       {:describe (fn [] {:ins {:in {}} :outs {}})
                        :transform (fn [s _ m] (>!! sink-ch m) [s {}])})
                 fl (flow/create-flow
                     {:procs {:sum {:proc (flow/process (flow/step-sum))}
                              :s {:proc (flow/process sink)}}
                      :conns [[[:sum :out] [:s :in]]]})
                 _ (flow/start fl)
                 _ (flow/resume fl)
                 _ (flow/inject fl [:sum :in] [1 2 3])
                 results [(<!! sink-ch) (<!! sink-ch) (<!! sink-ch)]]
             (flow/stop fl)
             results)"#,
    );
    assert_eq!(result, "[1 3 6]");
}

#[test]
fn native_step_take_full_flow_stops_forwarding_after_n() {
    let result = ps(
        r#"(let [sink-ch (chan 10)
                 sink (flow/map->step
                       {:describe (fn [] {:ins {:in {}} :outs {}})
                        :transform (fn [s _ m] (>!! sink-ch m) [s {}])})
                 fl (flow/create-flow
                     {:procs {:t {:proc (flow/process (flow/step-take 2))}
                              :s {:proc (flow/process sink)}}
                      :conns [[[:t :out] [:s :in]]]})
                 _ (flow/start fl)
                 _ (flow/resume fl)
                 _ (flow/inject fl [:t :in] [1 2 3])
                 first-two [(<!! sink-ch) (<!! sink-ch)]
                 _ (sleep-ms 100)
                 blocked (poll! sink-ch)
                 reply (flow/ping fl 1000)
                 count (get-in reply [:t :clojure.core.async.flow/count])]
             (flow/stop fl)
             [first-two blocked count])"#,
    );
    assert_eq!(result, "[[1 2] nil 3]");
}

#[test]
fn native_step_drop_full_flow_swallows_the_first_n_then_passes_through() {
    let result = ps(
        r#"(let [sink-ch (chan 10)
                 sink (flow/map->step
                       {:describe (fn [] {:ins {:in {}} :outs {}})
                        :transform (fn [s _ m] (>!! sink-ch m) [s {}])})
                 fl (flow/create-flow
                     {:procs {:d {:proc (flow/process (flow/step-drop 2))}
                              :s {:proc (flow/process sink)}}
                      :conns [[[:d :out] [:s :in]]]})
                 _ (flow/start fl)
                 _ (flow/resume fl)
                 _ (flow/inject fl [:d :in] [1 2 3])
                 result (<!! sink-ch)
                 _ (sleep-ms 100)
                 blocked (poll! sink-ch)]
             (flow/stop fl)
             [result blocked])"#,
    );
    assert_eq!(result, "[3 nil]");
}

#[test]
fn native_step_scan_full_flow_matches_step_sum_when_scanning_with_plus() {
    let result = ps(
        r#"(let [sink-ch (chan 10)
                 sink (flow/map->step
                       {:describe (fn [] {:ins {:in {}} :outs {}})
                        :transform (fn [s _ m] (>!! sink-ch m) [s {}])})
                 fl (flow/create-flow
                     {:procs {:sc {:proc (flow/process (flow/step-scan + 0))}
                              :s {:proc (flow/process sink)}}
                      :conns [[[:sc :out] [:s :in]]]})
                 _ (flow/start fl)
                 _ (flow/resume fl)
                 _ (flow/inject fl [:sc :in] [1 2 3])
                 results [(<!! sink-ch) (<!! sink-ch) (<!! sink-ch)]]
             (flow/stop fl)
             results)"#,
    );
    assert_eq!(result, "[1 3 6]");
}

#[test]
fn native_step_map_and_filter_full_flow() {
    let result = ps(
        r#"(let [sink-ch (chan 10)
                 sink (flow/map->step
                       {:describe (fn [] {:ins {:in {}} :outs {}})
                        :transform (fn [s _ m] (>!! sink-ch m) [s {}])})
                 fl (flow/create-flow
                     {:procs {:m {:proc (flow/process (flow/step-map inc))}
                              :f {:proc (flow/process (flow/step-filter even?))}
                              :s {:proc (flow/process sink)}}
                      :conns [[[:m :out] [:f :in]] [[:f :out] [:s :in]]]})
                 _ (flow/start fl)
                 _ (flow/resume fl)
                 ;; 1->2 (even, kept) 2->3 (odd, dropped) 3->4 (even, kept)
                 _ (flow/inject fl [:m :in] [1 2 3])
                 results [(<!! sink-ch) (<!! sink-ch)]
                 _ (sleep-ms 100)
                 blocked (poll! sink-ch)]
             (flow/stop fl)
             [results blocked])"#,
    );
    assert_eq!(result, "[[2 4] nil]");
}

// ---------------------------------------------------------------------------
// The deferred error-shape differential (N3 scope item 2): a flow whose
// step is `flow/step-map` with a throwing f, run through the REAL promoted
// fast path (`process_message_fast` / `report_transform_error_fast`),
// compared against the semantically-identical `map->step` pipeline running
// the generic interpreted path. Both must produce the SAME error-chan map
// shape (`:op :step`, previous `:state` kept, `:cid`, `:msg`, `:ex`), and
// both must keep delivering messages after the error.
// ---------------------------------------------------------------------------

#[test]
fn native_step_map_error_shape_matches_the_map_to_step_twin_and_both_continue() {
    let drive = |step_expr: &str| -> Value {
        eval_ok(&format!(
            r#"(let [f (fn [m] (if (= m :boom) (throw "kaboom") (inc m)))
                     step {step_expr}
                     sink-ch (chan 10)
                     sink (flow/map->step
                           {{:describe (fn [] {{:ins {{:in {{}}}} :outs {{}}}})
                            :transform (fn [s _ m] (>!! sink-ch m) [s {{}}])}})
                     fl (flow/create-flow
                         {{:procs {{:e {{:proc (flow/process step)}}
                                  :s {{:proc (flow/process sink)}}}}
                          :conns [[[:e :out] [:s :in]]]}})
                     chans (flow/start fl)
                     _ (flow/resume fl)
                     _ (flow/inject fl [:e :in] [1 :boom 2])
                     v1 (<!! sink-ch)
                     err (<!! (:error-chan chans))
                     v2 (<!! sink-ch)]
                 (flow/stop fl)
                 {{:v1 v1
                   :v2 v2
                   :state (:clojure.core.async.flow/state err)
                   :op (:clojure.core.async.flow/op err)
                   :has-pid (= :e (:clojure.core.async.flow/pid err))
                   :has-cid (= :in (:clojure.core.async.flow/cid err))
                   :has-msg (= :boom (:clojure.core.async.flow/msg err))
                   :has-ex (map? (:clojure.core.async.flow/ex err))}})"#
        ))
    };

    let native = drive("(flow/step-map f)");
    let interp = drive(
        r#"(flow/map->step
             {:describe (fn [] {:ins {:in {}} :outs {:out {}}})
              :transform (fn [s _ m] [s {:out [(f m)]}])})"#,
    );

    for (label, result) in [("native", &native), ("map->step", &interp)] {
        let Value::Map(m) = result else { panic!("{label}: expected a map, got {result:?}") };
        let get = |k: &str| m.get(&Value::Keyword(k.into())).cloned().unwrap();
        assert_eq!(get("v1"), Value::Int(2), "{label}: v1 (inc 1)");
        assert_eq!(get("v2"), Value::Int(3), "{label}: v2 (inc 2, proc kept running past the error)");
        assert_eq!(get("op"), Value::Keyword("step".into()), "{label}: :op");
        assert_eq!(get("has-pid"), Value::Bool(true), "{label}: :pid");
        assert_eq!(get("has-cid"), Value::Bool(true), "{label}: :cid");
        assert_eq!(get("has-msg"), Value::Bool(true), "{label}: :msg");
        assert_eq!(get("has-ex"), Value::Bool(true), "{label}: :ex");
    }
    // Both steps carry no state of their own (`step-map`'s is opaque, always
    // `{}`; the `map->step` twin's `:init` defaults to `{}` and its
    // `:transform` never touches `s`) -- so "previous state kept" is `{}`
    // in BOTH runs, and that agreement is itself the point: the fast path's
    // `snapshot()`-after-failed-transform and the generic path's untouched
    // `ctx.state` produce the exact same observable value.
    let Value::Map(nm) = &native else { unreachable!() };
    let Value::Map(im) = &interp else { unreachable!() };
    assert_eq!(
        nm.get(&Value::Keyword("state".into())),
        im.get(&Value::Keyword("state".into())),
        "native vs map->step :state after the error must match: native={native:?} interp={interp:?}"
    );
    assert_eq!(mova::internal::pr_str(nm.get(&Value::Keyword("state".into())).unwrap()), "{}");
}

// ---------------------------------------------------------------------------
// Native-vs-map->step differential for a count -> take pipeline (N3 scope
// item 2's second differential): same driving sequence through TWO procs
// chained together, native catalog steps on one side, a hand-written
// `map->step` twin with identical semantics on the other.
// ---------------------------------------------------------------------------

#[test]
fn native_count_then_take_pipeline_matches_its_map_to_step_twin() {
    let drive = |count_step: &str, take_step: &str| -> Value {
        eval_ok(&format!(
            r#"(let [sink-ch (chan 10)
                     sink (flow/map->step
                           {{:describe (fn [] {{:ins {{:in {{}}}} :outs {{}}}})
                            :transform (fn [s _ m] (>!! sink-ch m) [s {{}}])}})
                     fl (flow/create-flow
                         {{:procs {{:c {{:proc (flow/process {count_step})}}
                                  :t {{:proc (flow/process {take_step})}}
                                  :s {{:proc (flow/process sink)}}}}
                          :conns [[[:c :out] [:t :in]] [[:t :out] [:s :in]]]}})
                     _ (flow/start fl)
                     _ (flow/resume fl)
                     _ (flow/inject fl [:c :in] [10 20 30 40 50])
                     results [(<!! sink-ch) (<!! sink-ch) (<!! sink-ch)]
                     _ (sleep-ms 100)
                     blocked (poll! sink-ch)
                     reply (flow/ping fl 1000)
                     c-count (get-in reply [:c :clojure.core.async.flow/count])
                     c-state (get-in reply [:c :clojure.core.async.flow/state])
                     t-state (get-in reply [:t :clojure.core.async.flow/state])]
                 (flow/stop fl)
                 {{:results results :blocked blocked :c-count c-count :c-state c-state :t-state t-state}})"#
        ))
    };

    let native = drive("(flow/step-count)", "(flow/step-take 3)");
    let interp = drive(
        r#"(flow/map->step
             {:describe (fn [] {:ins {:in {}} :outs {:out {}}})
              :init (fn [_] {:count 0})
              :transform (fn [s _ m]
                           (let [s2 (update s :count inc)] [s2 {:out [m]}]))})"#,
        r#"(flow/map->step
             {:describe (fn [] {:ins {:in {}} :outs {:out {}}})
              :init (fn [_] {:remaining 3})
              :transform (fn [s _ m]
                           (if (pos? (:remaining s))
                             [(update s :remaining dec) {:out [m]}]
                             [s {}]))})"#,
    );

    assert_eq!(native, interp, "native pipeline diverged from its map->step twin");
    // W4D-TIERS: expected string was stale sorted-key order, from before a
    // W4 printer fix made small-map INSERTION order visible. The source
    // map literal above is built `{:results ... :blocked ... :c-count ...
    // :c-state ... :t-state ...}`, so that's the order it prints in now.
    assert_eq!(
        mova::internal::pr_str(&native),
        concat!(
            "{:results [10 20 30], :blocked nil, :c-count 5, ",
            ":c-state {:count 5}, :t-state {:remaining 0}}"
        )
    );
}

// ---------------------------------------------------------------------------
// N4: step-comp (fusion) -- full-flow differential against the unfused
// chain (same 3 members as separate procs vs one proc wrapping the comp),
// and a smoke test that a fused pipeline actually promotes onto the fast
// path (single in-chan, single out, no input-filter).
// ---------------------------------------------------------------------------

#[test]
fn step_comp_full_flow_matches_the_unfused_3_proc_chain() {
    let drive_unfused = || -> Value {
        eval_ok(
            r#"(let [sink-ch (chan 10)
                     sink (flow/map->step
                           {:describe (fn [] {:ins {:in {}} :outs {}})
                            :transform (fn [s _ m] (>!! sink-ch m) [s {}])})
                     fl (flow/create-flow
                         {:procs {:c {:proc (flow/process (flow/step-count))}
                                  :t {:proc (flow/process (flow/step-take 2))}
                                  :m {:proc (flow/process (flow/step-map inc))}
                                  :s {:proc (flow/process sink)}}
                          :conns [[[:c :out] [:t :in]] [[:t :out] [:m :in]] [[:m :out] [:s :in]]]})
                     _ (flow/start fl)
                     _ (flow/resume fl)
                     _ (flow/inject fl [:c :in] [10 20 30])
                     results (loop [n 2 acc []] (if (zero? n) acc (recur (dec n) (conj acc (<!! sink-ch)))))
                     _ (sleep-ms 100)
                     blocked (poll! sink-ch)]
                 (flow/stop fl)
                 [results blocked])"#,
        )
    };
    let drive_fused = || -> Value {
        eval_ok(
            r#"(let [sink-ch (chan 10)
                     sink (flow/map->step
                           {:describe (fn [] {:ins {:in {}} :outs {}})
                            :transform (fn [s _ m] (>!! sink-ch m) [s {}])})
                     comp (flow/step-comp (flow/step-count) (flow/step-take 2) (flow/step-map inc))
                     fl (flow/create-flow
                         {:procs {:fused {:proc (flow/process comp)}
                                  :s {:proc (flow/process sink)}}
                          :conns [[[:fused :out] [:s :in]]]})
                     _ (flow/start fl)
                     _ (flow/resume fl)
                     _ (flow/inject fl [:fused :in] [10 20 30])
                     results (loop [n 2 acc []] (if (zero? n) acc (recur (dec n) (conj acc (<!! sink-ch)))))
                     _ (sleep-ms 100)
                     blocked (poll! sink-ch)]
                 (flow/stop fl)
                 [results blocked])"#,
        )
    };

    let unfused = drive_unfused();
    let fused = drive_fused();
    assert_eq!(unfused, fused, "fused step-comp diverged from the unfused 3-proc chain");
    assert_eq!(mova::internal::pr_str(&fused), "[[11 21] nil]");
}

#[test]
fn step_comp_promotes_onto_the_fast_path_like_any_other_single_native_step() {
    // Same kill-switch differential shape as
    // `fast_path_and_killswitch_subprocesses_print_identical_results`,
    // but for a FUSED step: proves `try_promote_fast` treats a
    // `ComposedFactory`-carrying native exactly like any other (single
    // in-chan, <=1 out, no input-filter -- `step-comp`'s describe never
    // declares an input-filter), so the fast run and the kill-switched
    // (generic-shell) run must print identically.
    const PROGRAM: &str = r#"(let [n 50
             done (promise)
             comp (flow/step-comp (flow/step-count) (flow/step-take n))
             fl (flow/create-flow
                 {:procs {:fused {:proc (flow/process comp)}
                          :sink {:proc (flow/process (flow/step-sink-deliver n done))}}
                  :conns [[[:fused :out] [:sink :in]]]})
             _ (flow/start fl)
             _ (flow/resume fl)
             _ (flow/inject fl [:fused :in] (range n))
             delivered (deref done 5000 :timeout)]
         (flow/stop fl)
         (println (pr-str delivered)))"#;

    let run = |killswitch: bool| -> String {
        let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_mova"));
        cmd.arg("-e").arg(PROGRAM);
        if killswitch {
            cmd.env("MOVA_NO_FASTSTEP", "1");
        } else {
            cmd.env_remove("MOVA_NO_FASTSTEP");
        }
        let out = cmd.output().expect("failed to run the mova binary");
        assert!(
            out.status.success(),
            "mova exited with {:?}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };

    let fast = run(false);
    let generic = run(true);
    // `-e` also prints the script's own top-level result (`println`'s own
    // return value, `nil`) after the line the script printed itself --
    // same shape as `fast_path_and_killswitch_subprocesses_print_identical_results` above.
    assert_eq!(fast, "50\nnil");
    assert_eq!(fast, generic, "MOVA_NO_FASTSTEP=1 changed observable behavior for a fused step-comp");
}

// ---------------------------------------------------------------------------
// P3: ENGINE-LEVEL 1:1 chain fusion (FLOW-DESIGN.md's "Fusion" section).
//
// Every test here is a SUBPROCESS test, and that is deliberate: the fusion
// policy is read from the environment exactly once per process (an
// `OnceLock`, like `MOVA_NO_FASTSTEP`), so an in-process test would
// inherit whatever the suite was launched with -- and the whole suite must
// stay green under `MOVA_NO_FUSION=1` as well as by default. Spawning
// `mova` with an EXPLICIT fusion environment is the only honest way to
// assert fusion-specific behavior without breaking that sweep.
//
// Three environments matter:
//   - default             -> fuse runs whose every member promotes onto a
//                            FastStep (the measured default policy)
//   - MOVA_FUSE_ALL=1    -> fuse every topologically-fusable run,
//                            interpreted members included
//   - MOVA_NO_FUSION=1   -> the pre-P3 thread-per-proc engine
// ---------------------------------------------------------------------------

/// Runs `program` in a fresh `mova` process with an EXPLICIT fusion +
/// native-step environment (never inherited from the test runner's own),
/// returning its trimmed stdout.
fn run_flow_program(program: &str, env: &[(&str, &str)]) -> String {
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_mova"));
    cmd.arg("-e").arg(program);
    for k in ["MOVA_NO_FUSION", "MOVA_FUSE_ALL", "MOVA_NO_FASTSTEP", "MOVA_NO_SPSC"] {
        cmd.env_remove(k);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("failed to run the mova binary");
    assert!(out.status.success(), "mova exited with {:?}: {}", out.status, String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

const FUSED: &[(&str, &str)] = &[];
const FUSE_ALL: &[(&str, &str)] = &[("MOVA_FUSE_ALL", "1")];
const UNFUSED: &[(&str, &str)] = &[("MOVA_NO_FUSION", "1")];

/// The headline differential: a 4-proc INTERPRETED chain (`map->step`
/// members, which the default policy deliberately does NOT fuse -- see
/// `FusionPolicy` -- so `MOVA_FUSE_ALL=1` is what drives the fused side
/// here), including a mid-chain transform error, compared on everything the
/// engine makes observable: delivered messages and their order, the
/// error-chan map's every stable field, and per-pid ping
/// count/status/state/ins/outs.
#[test]
fn fused_vs_unfused_interpreted_pipeline_is_observably_identical() {
    const PROGRAM: &str = r#"(let [out-ch (chan 100)
             mk (fn [f] (flow/map->step
                          {:describe (fn [] {:ins {:in {}} :outs {:out {}}})
                           :init (fn [_] {:n 0})
                           :transform (fn [s _ m] [(update s :n inc) {:out [(f m)]}])}))
             boom (fn [m] (if (= m :boom) (throw "kaboom") m))
             sink (flow/map->step
                    {:describe (fn [] {:ins {:in {}} :outs {}})
                     :init (fn [_] {:n 0})
                     :transform (fn [s _ m] (>!! out-ch m) [(update s :n inc) {}])})
             fl (flow/create-flow
                 {:procs {:a {:proc (flow/process (mk (fn [m] m)))}
                          :b {:proc (flow/process (mk boom))}
                          :c {:proc (flow/process (mk inc))}
                          :s {:proc (flow/process sink)}}
                  :conns [[[:a :out] [:b :in]] [[:b :out] [:c :in]] [[:c :out] [:s :in]]]})
             chans (flow/start fl)
             _ (flow/resume fl)
             _ (flow/inject fl [:a :in] [1 2 :boom 3])
             vals [(<!! out-ch) (<!! out-ch) (<!! out-ch)]
             err (<!! (:error-chan chans))
             _ (sleep-ms 150)
             reply (flow/ping fl 2000)
             at (fn [pid k] (get-in reply [pid k]))
             pids [:a :b :c :s]]
         (flow/stop fl)
         (println (pr-str
                   [vals
                    [(:clojure.core.async.flow/pid err)
                     (:clojure.core.async.flow/cid err)
                     (:clojure.core.async.flow/msg err)
                     (:clojure.core.async.flow/op err)
                     (:clojure.core.async.flow/state err)
                     (:clojure.core.async.flow/count err)
                     (:clojure.core.async.flow/status err)
                     (get (:clojure.core.async.flow/ex err) :message)]
                    (vec (map (fn [p] (at p :clojure.core.async.flow/count)) pids))
                    (vec (map (fn [p] (at p :clojure.core.async.flow/status)) pids))
                    (vec (map (fn [p] (at p :clojure.core.async.flow/state)) pids))
                    (vec (map (fn [p] [(at p :clojure.core.async.flow/ins)
                                       (at p :clojure.core.async.flow/outs)]) pids))])))"#;

    let fused = run_flow_program(PROGRAM, FUSE_ALL);
    let unfused = run_flow_program(PROGRAM, UNFUSED);
    assert_eq!(
        fused,
        concat!(
            // :b swallows :boom, :c increments the three survivors
            "[[2 3 4] ",
            // the error map: reported with :b's OWN pid/cid/state/count,
            // :op :step, previous state kept (2 messages had succeeded)
            "[:b :in :boom :step {:n 2} 2 :running \"kaboom\"] ",
            "[4 3 3 3] [:running :running :running :running] ",
            "[{:n 4} {:n 3} {:n 3} {:n 3}] ",
            "[[[:in] [:out]] [[:in] [:out]] [[:in] [:out]] [[:in] []]]]\nnil"
        ),
        "the fused interpreted pipeline printed something unexpected"
    );
    assert_eq!(fused, unfused, "fusion changed observable behavior on an interpreted pipeline");
}

/// `flow/ping-proc` on a MID-CHAIN pid: answered from that member's own
/// count/status/state/ports, and `flow/ping` (flow-wide) must be
/// indistinguishable in shape from the unfused run. This chain is all
/// native steps, so it fuses under the DEFAULT policy.
#[test]
fn ping_proc_mid_chain_fused_matches_the_unfused_reply_exactly() {
    const PROGRAM: &str = r#"(let [n 40
             done (promise)
             fl (flow/create-flow
                 {:procs {:head {:proc (flow/process (flow/step-count))}
                          :mid {:proc (flow/process (flow/step-passthrough))}
                          :tail {:proc (flow/process (flow/step-sink-deliver n done))}}
                  :conns [[[:head :out] [:mid :in]] [[:mid :out] [:tail :in]]]})
             _ (flow/start fl)
             _ (flow/resume fl)
             _ (flow/inject fl [:head :in] (range n))
             delivered (deref done 5000 :timeout)
             _ (sleep-ms 150)
             mid-reply (flow/ping-proc fl :mid 2000)
             all (flow/ping fl 2000)]
         (flow/stop fl)
         (println (pr-str [delivered
                           [(:clojure.core.async.flow/pid mid-reply)
                            (:clojure.core.async.flow/status mid-reply)
                            (:clojure.core.async.flow/count mid-reply)
                            (:clojure.core.async.flow/state mid-reply)
                            (:clojure.core.async.flow/ins mid-reply)
                            (:clojure.core.async.flow/outs mid-reply)]
                           (vec (map (fn [p] [(get-in all [p :clojure.core.async.flow/count])
                                              (get-in all [p :clojure.core.async.flow/status])
                                              (get-in all [p :clojure.core.async.flow/state])
                                              (get-in all [p :clojure.core.async.flow/ins])
                                              (get-in all [p :clojure.core.async.flow/outs])])
                                     [:head :mid :tail]))])))"#;

    let fused = run_flow_program(PROGRAM, FUSED);
    let unfused = run_flow_program(PROGRAM, UNFUSED);
    assert_eq!(
        fused,
        concat!(
            "[40 [:mid :running 40 {} [:in] [:out]] ",
            "[[40 :running {:count 40} [:in] [:out]] ",
            "[40 :running {} [:in] [:out]] ",
            "[40 :running {:count 40} [:in] []]]]\nnil"
        )
    );
    assert_eq!(fused, unfused, "a fused run's ping replies differ from the unfused ones");
}

/// `flow/pause-proc` on a mid-chain member. THIS is the documented
/// deviation, asserted in both directions: fused, pausing any member parks
/// the WHOLE run (the head stops consuming at the next message boundary, so
/// its count stops advancing); unfused, the head keeps running until the
/// inter-proc buffer fills. Both deliver everything after `resume-proc`.
#[test]
fn pause_proc_mid_chain_pauses_the_whole_fused_run_and_resumes_losslessly() {
    const PROGRAM: &str = r#"(let [done (promise)
             fl (flow/create-flow
                 {:procs {:head {:proc (flow/process (flow/step-count))}
                          :mid {:proc (flow/process (flow/step-passthrough))}
                          :tail {:proc (flow/process (flow/step-sink-deliver 10 done))}}
                  :conns [[[:head :out] [:mid :in]] [[:mid :out] [:tail :in]]]})
             _ (flow/start fl)
             _ (flow/resume fl)
             _ (flow/inject fl [:head :in] (range 5))
             _ (sleep-ms 250)
             before (flow/ping fl 2000)
             _ (flow/pause-proc fl :mid)
             _ (sleep-ms 100)
             _ (flow/inject fl [:head :in] (range 5 10))
             _ (sleep-ms 250)
             paused (flow/ping fl 2000)
             _ (flow/resume-proc fl :mid)
             delivered (deref done 5000 :timeout)]
         (flow/stop fl)
         (println (pr-str [(get-in before [:head :clojure.core.async.flow/count])
                           (get-in before [:tail :clojure.core.async.flow/count])
                           (get-in paused [:head :clojure.core.async.flow/count])
                           (get-in paused [:head :clojure.core.async.flow/status])
                           (get-in paused [:mid :clojure.core.async.flow/status])
                           delivered])))"#;

    // Fused: :head never sees messages 5..9 while :mid is paused (the run
    // has ONE read loop), yet its own status stays :running -- only :mid
    // was paused. Everything is delivered once :mid resumes.
    assert_eq!(run_flow_program(PROGRAM, FUSED), "[5 5 5 :running :paused 10]\nnil");
    // Unfused: :head has its own thread and its own downstream buffer, so
    // it consumes all 10 regardless of :mid's status -- the count
    // difference this deviation is documented for.
    assert_eq!(run_flow_program(PROGRAM, UNFUSED), "[5 5 10 :running :paused 10]\nnil");
}

/// `flow/inject` into a MID-CHAIN in-port: the chan is still wired exactly
/// as it would be unfused, and the fused loop drains it at batch
/// boundaries, so the injected message flows through that member and every
/// member after it.
#[test]
fn inject_mid_chain_fused_flows_through_the_rest_of_the_chain() {
    const PROGRAM: &str = r#"(let [done (promise)
             fl (flow/create-flow
                 {:procs {:head {:proc (flow/process (flow/step-count))}
                          :mid {:proc (flow/process (flow/step-count))}
                          :tail {:proc (flow/process (flow/step-sink-deliver 5 done))}}
                  :conns [[[:head :out] [:mid :in]] [[:mid :out] [:tail :in]]]})
             _ (flow/start fl)
             _ (flow/resume fl)
             _ (flow/inject fl [:head :in] [10 20 30])
             _ (flow/inject fl [:mid :in] [70 80])
             delivered (deref done 5000 :timeout)
             _ (sleep-ms 150)
             reply (flow/ping fl 2000)]
         (flow/stop fl)
         (println (pr-str [delivered
                           (vec (map (fn [p] (get-in reply [p :clojure.core.async.flow/count]))
                                     [:head :mid :tail]))])))"#;

    let fused = run_flow_program(PROGRAM, FUSED);
    // :head saw 3, :mid saw those 3 PLUS the 2 injected straight into it,
    // :tail saw all 5 -- identical either way.
    assert_eq!(fused, "[5 [3 5 5]]\nnil");
    assert_eq!(fused, run_flow_program(PROGRAM, UNFUSED));
}

/// A mid-chain member whose `init` returns an extra `::flow/in-ports`
/// breaks the single-in assumption the wiring-time plan was built on -- so
/// the whole run DEMOTES to ordinary procs at init time (before a single
/// message moves) and everything keeps working, including the extra port.
#[test]
fn demotion_on_init_ports_falls_back_to_ordinary_procs_and_still_delivers() {
    const PROGRAM: &str = r#"(let [extra (chan 10)
             out-ch (chan 100)
             relay (flow/map->step
                     {:describe (fn [] {:ins {:in {}} :outs {:out {}}})
                      :init (fn [_] {:n 0})
                      :transform (fn [s _ m] [(update s :n inc) {:out [m]}])})
             widened (flow/map->step
                       {:describe (fn [] {:ins {:in {}} :outs {:out {}}})
                        :init (fn [_] {:n 0 :clojure.core.async.flow/in-ports {:extra extra}})
                        :transform (fn [s _ m] [(update s :n inc) {:out [m]}])})
             sink (flow/map->step
                    {:describe (fn [] {:ins {:in {}} :outs {}})
                     :init (fn [_] {:n 0})
                     :transform (fn [s _ m] (>!! out-ch m) [(update s :n inc) {}])})
             fl (flow/create-flow
                 {:procs {:a {:proc (flow/process relay)}
                          :b {:proc (flow/process widened)}
                          :c {:proc (flow/process sink)}}
                  :conns [[[:a :out] [:b :in]] [[:b :out] [:c :in]]]})
             _ (flow/start fl)
             _ (flow/resume fl)
             _ (flow/inject fl [:a :in] [1 2 3])
             _ (>!! extra 99)
             vals (loop [n 4 acc []] (if (zero? n) acc (recur (dec n) (conj acc (<!! out-ch)))))
             _ (sleep-ms 150)
             reply (flow/ping fl 2000)]
         (flow/stop fl)
         (println (pr-str [(sort vals)
                           (vec (map (fn [p] (get-in reply [p :clojure.core.async.flow/count])) [:a :b :c]))
                           (get-in reply [:b :clojure.core.async.flow/ins])])))"#;

    // MOVA_FUSE_ALL would otherwise have fused a->b->c; :b's init widening
    // it to two in-ports demotes the run instead.
    let demoted = run_flow_program(PROGRAM, FUSE_ALL);
    assert_eq!(demoted, "[(1 2 3 99) [3 4 4] [:extra :in]]\nnil");
    assert_eq!(demoted, run_flow_program(PROGRAM, UNFUSED), "demotion diverged from the never-fused run");
}

/// A MIXED chain -- native `step-passthrough`, then a `step-map` whose `f`
/// is ordinary interpreted mova code (one interpreter entry per message
/// INSIDE the fused loop), then a native sink -- fuses under the default
/// policy and stays observably identical to the unfused run.
#[test]
fn mixed_native_and_interpreted_chain_fuses_and_stays_identical() {
    const PROGRAM: &str = r#"(let [n 25
             done (promise)
             fl (flow/create-flow
                 {:procs {:head {:proc (flow/process (flow/step-passthrough))}
                          :mid {:proc (flow/process (flow/step-map (fn [m] (* 2 m))))}
                          :tail {:proc (flow/process (flow/step-sum))}
                          :sink {:proc (flow/process (flow/step-sink-deliver n done))}}
                  :conns [[[:head :out] [:mid :in]] [[:mid :out] [:tail :in]] [[:tail :out] [:sink :in]]]})
             _ (flow/start fl)
             _ (flow/resume fl)
             _ (flow/inject fl [:head :in] (range n))
             delivered (deref done 5000 :timeout)
             _ (sleep-ms 150)
             reply (flow/ping fl 2000)
             at (fn [p k] (get-in reply [p k]))]
         (flow/stop fl)
         (println (pr-str [delivered
                           (vec (map (fn [p] (at p :clojure.core.async.flow/count)) [:head :mid :tail :sink]))
                           (at :tail :clojure.core.async.flow/state)])))"#;

    let fused = run_flow_program(PROGRAM, FUSED);
    // sum of 2*0..2*24 = 2 * (24*25/2) = 600
    assert_eq!(fused, "[25 [25 25 25 25] {:sum 600}]\nnil");
    assert_eq!(fused, run_flow_program(PROGRAM, UNFUSED));
}

/// A transform error at a MID-CHAIN member of a fused run: reported with
/// THAT member's pid/cid/state/count, the message consumed there (nothing
/// goes downstream for it), the chain continuing with the next message --
/// all byte-identical to the unfused run's error.
#[test]
fn error_mid_chain_fused_keeps_state_and_the_run_continues() {
    const PROGRAM: &str = r#"(let [done (promise)
             f (fn [m] (if (= m :boom) (throw "kaboom") (inc m)))
             fl (flow/create-flow
                 {:procs {:head {:proc (flow/process (flow/step-passthrough))}
                          :mid {:proc (flow/process (flow/step-map f))}
                          :tail {:proc (flow/process (flow/step-sink-deliver 2 done))}}
                  :conns [[[:head :out] [:mid :in]] [[:mid :out] [:tail :in]]]})
             chans (flow/start fl)
             _ (flow/resume fl)
             _ (flow/inject fl [:head :in] [1 :boom 2])
             delivered (deref done 5000 :timeout)
             err (<!! (:error-chan chans))
             _ (sleep-ms 150)
             reply (flow/ping fl 2000)]
         (flow/stop fl)
         (println (pr-str [delivered
                           [(:clojure.core.async.flow/pid err)
                            (:clojure.core.async.flow/cid err)
                            (:clojure.core.async.flow/msg err)
                            (:clojure.core.async.flow/op err)
                            (:clojure.core.async.flow/state err)
                            (:clojure.core.async.flow/count err)
                            (get (:clojure.core.async.flow/ex err) :message)]
                           (vec (map (fn [p] (get-in reply [p :clojure.core.async.flow/count]))
                                     [:head :mid :tail]))])))"#;

    let fused = run_flow_program(PROGRAM, FUSED);
    assert_eq!(fused, "[2 [:mid :in :boom :step {} 1 \"kaboom\"] [3 2 2]]\nnil");
    assert_eq!(fused, run_flow_program(PROGRAM, UNFUSED), "a fused mid-chain error differs from the unfused one");
}

/// The kill switch is a true no-op switch, not a feature flag that changes
/// behavior: the N2 native-step differential program (a fully promotable
/// 3-proc chain, i.e. exactly what the default policy DOES fuse) must print
/// identically under all three fusion environments.
#[test]
fn fusion_environments_all_print_identical_results_for_a_promoted_chain() {
    const PROGRAM: &str = r#"(let [n 200
             done (promise)
             tick (chan 64)
             fl (flow/create-flow
                 {:procs {:gen {:proc (flow/process (flow/step-source tick))}
                          :mid {:proc (flow/process (flow/step-passthrough))}
                          :sink {:proc (flow/process (flow/step-sink-deliver n done))}}
                  :conns [[[:gen :out] [:mid :in]] [[:mid :out] [:sink :in]]]})
             _ (flow/start fl)
             _ (flow/resume fl)
             _ (flow/feed-range! tick n)
             delivered (deref done 5000 :timeout)
             _ (sleep-ms 100)
             reply (flow/ping fl 2000)
             at (fn [pid k] (get-in reply [pid k]))]
         (flow/stop fl)
         (println (pr-str [delivered
                           [(at :gen :clojure.core.async.flow/count)
                            (at :mid :clojure.core.async.flow/count)
                            (at :sink :clojure.core.async.flow/count)]
                           [(at :gen :clojure.core.async.flow/state)
                            (at :mid :clojure.core.async.flow/state)
                            (at :sink :clojure.core.async.flow/state)]
                           [(at :gen :clojure.core.async.flow/status)
                            (at :sink :clojure.core.async.flow/ins)
                            (at :sink :clojure.core.async.flow/outs)]])))"#;

    // The generator declares NO :ins and gains :tick... no: it gains :in at
    // init via ::flow/in-ports, which becomes the fused RUN's input -- the
    // head-gains-in-ports case fusion has to get right.
    let fused = run_flow_program(PROGRAM, FUSED);
    assert_eq!(
        fused,
        concat!(
            "[200 [200 200 200] ",
            "[#:clojure.core.async.flow{:in-ports {:in #<chan closed>}} {} {:count 200}] ",
            "[:running [:in] []]]\nnil"
        )
    );
    assert_eq!(fused, run_flow_program(PROGRAM, FUSE_ALL), "MOVA_FUSE_ALL=1 changed observable behavior");
    assert_eq!(fused, run_flow_program(PROGRAM, UNFUSED), "MOVA_NO_FUSION=1 changed observable behavior");
}

#[test]
fn fan_in_delivers_every_writers_messages_to_the_shared_in_port() {
    // Regression: conn wiring used to key destination-chan creation by
    // SOURCE, so with two conns into the same [pid port] the last-wired
    // writer's chan overwrote the first's in `ins_by_pid` and every
    // earlier writer's messages were silently orphaned. JVM flow fan-in:
    // all writers share the destination's one in-chan.
    let out = ps(r#"
      (do
        (def got (chan 16))
        (defn a-proc
          ([] {:ins {:in "trigger"} :outs {:out "x"}})
          ([_] {})
          ([s _t] s)
          ([s _in m] [s {:out [[:via-a m]]}]))
        (defn b-proc
          ([] {:ins {:in "trigger"} :outs {:out "x"}})
          ([_] {})
          ([s _t] s)
          ([s _in m] [s {:out [[:via-b m]]}]))
        (defn sink-proc
          ([] {:ins {:in "y"}})
          ([_] {})
          ([s _t] s)
          ([s _in m] (>!! got m) [s {}]))
        (def g (flow/create-flow
                {:procs {:a {:proc (flow/process a-proc)}
                         :b {:proc (flow/process b-proc)}
                         :sink {:proc (flow/process sink-proc)}}
                 :conns [[[:a :out] [:sink :in]]
                         [[:b :out] [:sink :in]]]}))
        (flow/start g)
        (flow/resume g)
        (flow/inject g [:a :in] ["ping"])
        (def first-msg (first (alts!! [got (timeout 3000)])))
        (flow/inject g [:b :in] ["pong"])
        (def second-msg (first (alts!! [got (timeout 3000)])))
        (flow/stop g)
        [first-msg second-msg])"#);
    assert_eq!(out, "[[:via-a \"ping\"] [:via-b \"pong\"]]");
}

// ---------------------------------------------------------------------------
// T2: the 1:1 TRANSPORT tier (FLOW-DESIGN.md's "Transport selection"
// section, `src/transport.rs`'s kernel).
//
// Subprocess tests for the same reason the fusion block above is: the
// `MOVA_NO_SPSC` kill switch is read once per process (an `OnceLock`), and
// the whole suite has to stay green under `MOVA_NO_SPSC=1` too -- so
// spawning `mova` with an EXPLICIT transport environment is the only way
// to assert transport-specific behavior without breaking that sweep.
//
// Every test here also pins `MOVA_NO_FUSION=1`. Fusion outranks the
// transport by design (a fused run has no channel hop left to accelerate,
// so `plan_transport_links` refuses every conn touching one); pinning
// fusion off is what makes these topologies actually take the transport
// path, so the differential tests the thing it claims to test.
//
// The DECISION -- which conns convert and, more importantly, which never do
// (fan-out, fan-in, self-loop, multi-port, `:buf-or-n 0`) -- is asserted
// separately and directly in `src/builtins/flow.rs`'s own unit tests,
// because a behavioral differential passes trivially when the transport is
// silently never selected.
// ---------------------------------------------------------------------------

/// Unfused (so conns really do convert) with the transport ON.
const SPSC_ON: &[(&str, &str)] = &[("MOVA_NO_FUSION", "1")];
/// The same topology with the transport OFF -- every conn back on the
/// general `Chan`, i.e. the exact pre-T2 engine.
const SPSC_OFF: &[(&str, &str)] = &[("MOVA_NO_FUSION", "1"), ("MOVA_NO_SPSC", "1")];

/// The headline differential, deliberately the SAME program the fusion
/// block's headline uses: a 4-proc interpreted chain (every conn 1:1,
/// single-in/single-out, so all three convert) including a mid-chain
/// transform error, compared on everything the engine makes observable --
/// delivered messages and their order, the error-chan map's every stable
/// field, and per-pid ping count/status/state/ins/outs.
#[test]
fn transport_vs_chan_interpreted_pipeline_is_observably_identical() {
    const PROGRAM: &str = r#"(let [out-ch (chan 100)
             mk (fn [f] (flow/map->step
                          {:describe (fn [] {:ins {:in {}} :outs {:out {}}})
                           :init (fn [_] {:n 0})
                           :transform (fn [s _ m] [(update s :n inc) {:out [(f m)]}])}))
             boom (fn [m] (if (= m :boom) (throw "kaboom") m))
             sink (flow/map->step
                    {:describe (fn [] {:ins {:in {}} :outs {}})
                     :init (fn [_] {:n 0})
                     :transform (fn [s _ m] (>!! out-ch m) [(update s :n inc) {}])})
             fl (flow/create-flow
                 {:procs {:a {:proc (flow/process (mk (fn [m] m)))}
                          :b {:proc (flow/process (mk boom))}
                          :c {:proc (flow/process (mk inc))}
                          :s {:proc (flow/process sink)}}
                  :conns [[[:a :out] [:b :in]] [[:b :out] [:c :in]] [[:c :out] [:s :in]]]})
             chans (flow/start fl)
             _ (flow/resume fl)
             _ (flow/inject fl [:a :in] [1 2 :boom 3])
             vals [(<!! out-ch) (<!! out-ch) (<!! out-ch)]
             err (<!! (:error-chan chans))
             _ (sleep-ms 150)
             reply (flow/ping fl 2000)
             at (fn [pid k] (get-in reply [pid k]))
             pids [:a :b :c :s]]
         (flow/stop fl)
         (println (pr-str
                   [vals
                    [(:clojure.core.async.flow/pid err)
                     (:clojure.core.async.flow/cid err)
                     (:clojure.core.async.flow/msg err)
                     (:clojure.core.async.flow/op err)
                     (:clojure.core.async.flow/state err)
                     (:clojure.core.async.flow/count err)
                     (:clojure.core.async.flow/status err)
                     (get (:clojure.core.async.flow/ex err) :message)]
                    (vec (map (fn [p] (at p :clojure.core.async.flow/count)) pids))
                    (vec (map (fn [p] (at p :clojure.core.async.flow/status)) pids))
                    (vec (map (fn [p] (at p :clojure.core.async.flow/state)) pids))
                    (vec (map (fn [p] [(at p :clojure.core.async.flow/ins)
                                       (at p :clojure.core.async.flow/outs)]) pids))])))"#;

    let on = run_flow_program(PROGRAM, SPSC_ON);
    let off = run_flow_program(PROGRAM, SPSC_OFF);
    assert_eq!(
        on,
        concat!(
            "[[2 3 4] ",
            "[:b :in :boom :step {:n 2} 2 :running \"kaboom\"] ",
            "[4 3 3 3] [:running :running :running :running] ",
            "[{:n 4} {:n 3} {:n 3} {:n 3}] ",
            "[[[:in] [:out]] [[:in] [:out]] [[:in] [:out]] [[:in] []]]]\nnil"
        ),
        "the transport-backed interpreted pipeline printed something unexpected"
    );
    assert_eq!(on, off, "MOVA_NO_SPSC=1 changed observable behavior");
}

/// The whole lifecycle surface over a transport-backed chain in ONE
/// program: procs start paused (nothing flows before `resume`),
/// `pause`/`resume` are lossless across the converted hops,
/// `pause-proc`/`resume-proc` reach a single mid-chain proc, a transform
/// error keeps previous state and lets the proc continue, `stop` runs every
/// stop transition observably, `stop` is idempotent, and restarting or
/// pinging a stopped flow still errors. Every one of those crosses at least
/// one converted conn.
#[test]
fn transport_lifecycle_surface_matches_the_chan_path() {
    const PROGRAM: &str = r#"(let [out-ch (chan 200)
             stopped (atom [])
             relay (flow/map->step
                     {:describe (fn [] {:ins {:in {}} :outs {:out {}}})
                      :init (fn [_] {:n 0})
                      :transition (fn [s t]
                                    (when (= t :clojure.core.async.flow/stop)
                                      (swap! stopped conj :relay))
                                    s)
                      :transform (fn [s _ m]
                                   (if (= m :boom)
                                     (throw "kaboom")
                                     [(update s :n inc) {:out [m]}]))})
             sink (flow/map->step
                    {:describe (fn [] {:ins {:in {}} :outs {}})
                     :init (fn [_] {:n 0})
                     :transition (fn [s t]
                                   (when (= t :clojure.core.async.flow/stop)
                                     (swap! stopped conj :sink))
                                   s)
                     :transform (fn [s _ m] (>!! out-ch m) [(update s :n inc) {}])})
             fl (flow/create-flow
                 {:procs {:a {:proc (flow/process relay)}
                          :b {:proc (flow/process relay)}
                          :s {:proc (flow/process sink)}}
                  :conns [[[:a :out] [:b :in]] [[:b :out] [:s :in]]]})
             chans (flow/start fl)
             _ (flow/inject fl [:a :in] [1 2 3])
             _ (sleep-ms 150)
             before-resume (poll! out-ch)
             _ (flow/resume fl)
             early [(<!! out-ch) (<!! out-ch) (<!! out-ch)]
             _ (flow/pause fl)
             _ (sleep-ms 80)
             _ (flow/inject fl [:a :in] [4 5 6])
             _ (sleep-ms 150)
             during-pause (poll! out-ch)
             paused-status (get-in (flow/ping fl 2000) [:b :clojure.core.async.flow/status])
             _ (flow/resume fl)
             after-resume [(<!! out-ch) (<!! out-ch) (<!! out-ch)]
             _ (flow/pause-proc fl :b)
             _ (sleep-ms 80)
             one-paused (vec (map (fn [p] (get-in (flow/ping fl 2000) [p :clojure.core.async.flow/status]))
                                  [:a :b :s]))
             _ (flow/resume-proc fl :b)
             _ (flow/inject fl [:a :in] [7 :boom 8])
             after-error [(<!! out-ch) (<!! out-ch)]
             err (<!! (:error-chan chans))
             _ (sleep-ms 200)
             counts (vec (map (fn [p] (get-in (flow/ping fl 2000) [p :clojure.core.async.flow/count]))
                              [:a :b :s]))
             _ (flow/stop fl)
             _ (flow/stop fl)
             restart-err (try (flow/start fl) :no-error (catch e (str e)))
             ping-err (try (flow/ping fl 100) :no-error (catch e (str e)))]
         (println (pr-str [before-resume early during-pause paused-status after-resume
                           one-paused after-error
                           [(:clojure.core.async.flow/pid err)
                            (:clojure.core.async.flow/msg err)
                            (:clojure.core.async.flow/state err)
                            (:clojure.core.async.flow/op err)]
                           counts
                           (vec (sort @stopped))
                           restart-err ping-err])))"#;

    let on = run_flow_program(PROGRAM, SPSC_ON);
    let off = run_flow_program(PROGRAM, SPSC_OFF);
    assert_eq!(on, off, "MOVA_NO_SPSC=1 changed the lifecycle surface");
    // W4D-TIERS: the two error strings' key order was stale (`:message`
    // before `:type`), from before a W4 printer fix made small-map
    // INSERTION order visible; the internal untyped-catch error map is
    // built `:type`-first (matches `differential_test.rs`'s
    // `try_catch_finally_in_compiled_code`, measured the same way there).
    assert_eq!(
        on,
        concat!(
            // procs start paused: nothing before resume, then all 3 in order
            "[nil [1 2 3] ",
            // nothing while paused; :b reports :paused; then all 3 in order
            "nil :paused [4 5 6] ",
            // pause-proc reaches exactly one proc
            "[:running :paused :running] ",
            // the error is :a's (it threw), and 7/8 still arrive
            "[7 8] [:a :boom {:n 7} :step] ",
            // counts: :a saw 9 and failed 1 -> 8; :b and :s saw the 8 survivors
            "[8 8 8] ",
            // both stop transitions ran; restart and ping both error
            "[:relay :relay :sink] ",
            "\"java.lang.RuntimeException: flow/start: flow has already been started\" ",
            "\"java.lang.RuntimeException: flow/ping: flow is not running\"]\nnil"
        ),
        "the transport-backed lifecycle surface printed something unexpected"
    );
}

/// `flow/inject` into a TRANSPORT-BACKED in-port. This is the one place the
/// transport genuinely needs help: an injection is a third writer arriving
/// on its own thread, which a strictly-1:1 ring cannot carry, so that port
/// keeps its `Chan` as an injection side channel and its proc drains both.
/// Both sources must be delivered, and each must keep its OWN FIFO order
/// (the interleaving between them is unspecified -- it is lap-granular, the
/// same deviation a fused run documents for mid-chain inject).
#[test]
fn inject_into_a_transport_backed_mid_chain_in_port_delivers_both_sources() {
    const PROGRAM: &str = r#"(let [out-ch (chan 400)
             relay (flow/map->step
                     {:describe (fn [] {:ins {:in {}} :outs {:out {}}})
                      :transform (fn [s _ m] [s {:out [m]}])})
             sink (flow/map->step
                    {:describe (fn [] {:ins {:in {}} :outs {}})
                     :transform (fn [s _ m] (>!! out-ch m) [s {}])})
             fl (flow/create-flow
                 {:procs {:a {:proc (flow/process relay)}
                          :b {:proc (flow/process relay)}
                          :s {:proc (flow/process sink)}}
                  :conns [[[:a :out] [:b :in]] [[:b :out] [:s :in]]]})
             _ (flow/start fl)
             _ (flow/resume fl)
             f1 (flow/inject fl [:a :in] (map (fn [i] [:stream i]) (range 100)))
             f2 (flow/inject fl [:b :in] (map (fn [i] [:injected i]) (range 100)))
             _ (deref f1 5000 :timeout)
             _ (deref f2 5000 :timeout)
             got (loop [acc [] k 0]
                   (if (= k 200) acc (recur (conj acc (<!! out-ch)) (inc k))))
             tag (fn [t] (vec (map second (filter (fn [m] (= (first m) t)) got))))]
         (flow/stop fl)
         (println (pr-str [(count got)
                           (= (tag :stream) (vec (range 100)))
                           (= (tag :injected) (vec (range 100)))])))"#;

    let on = run_flow_program(PROGRAM, SPSC_ON);
    let off = run_flow_program(PROGRAM, SPSC_OFF);
    assert_eq!(on, "[200 true true]\nnil", "an injection into a transport-backed in-port was lost or reordered");
    assert_eq!(on, off, "MOVA_NO_SPSC=1 changed inject behavior");
}

/// A converted conn must backpressure where its `Chan` did. `:buf-or-n 2`
/// on the destination means the ring holds exactly 2 (the kernel enforces
/// the requested capacity verbatim, not its rounded-up slot count), so a
/// producer facing a wedged consumer blocks after a bounded number of
/// messages -- and, critically, still notices `pause` while blocked, which
/// is the control-priority contract raced against every blocked send.
#[test]
fn a_converted_conn_backpressures_and_still_honours_control_while_blocked() {
    const PROGRAM: &str = r#"(let [gate (chan 1)
             out-ch (chan 200)
             src (flow/map->step
                   {:describe (fn [] {:ins {:in {}} :outs {:out {}}})
                    :init (fn [_] {:n 0})
                    :transform (fn [s _ m] [(update s :n inc) {:out [m]}])})
             slow (flow/map->step
                    {:describe (fn [] {:ins {:in {}} :outs {}})
                     :init (fn [_] {:n 0})
                     :transform (fn [s _ m] (<!! gate) (>!! out-ch m) [(update s :n inc) {}])})
             fl (flow/create-flow
                 {:procs {:a {:proc (flow/process src)}
                          :s {:proc (flow/process slow) :chan-opts {:in {:buf-or-n 2}}}}
                  :conns [[[:a :out] [:s :in]]]})
             _ (flow/start fl)
             _ (flow/resume fl)
             _ (flow/inject fl [:a :in] (range 50))
             _ (sleep-ms 250)
             blocked-count (get-in (flow/ping fl 2000) [:a :clojure.core.async.flow/count])
             _ (flow/pause fl)
             _ (sleep-ms 60)
             _ (>!! gate :go)
             _ (sleep-ms 200)
             paused (get-in (flow/ping fl 2000) [:a :clojure.core.async.flow/status])]
         (flow/stop fl)
         (println (pr-str [(and (> blocked-count 0) (< blocked-count 40)) paused])))"#;

    let on = run_flow_program(PROGRAM, SPSC_ON);
    let off = run_flow_program(PROGRAM, SPSC_OFF);
    assert_eq!(on, "[true :paused]\nnil", "a converted conn lost its backpressure or its control priority");
    assert_eq!(on, off, "MOVA_NO_SPSC=1 changed backpressure/control behavior");
}

/// [`run_flow_program`] under a WATCHDOG: a hang is a test failure, not a
/// wedged CI run.
///
/// The failure mode the transport tier has to be guarded against is a lost
/// wake -- both sides parked forever at 0% CPU (that is exactly how the
/// reverted E2 wake-elision fast path failed, see
/// `bench/optimization-log.md`). A plain `cmd.output()` would simply block
/// on such a child until the harness's own timeout, reported as nothing in
/// particular; killing it and panicking with a diagnostic reports it as the
/// deadlock class it is. Same policy as `src/transport/tests.rs`'s own
/// watchdog, adapted to a subprocess (which CAN be killed, so there is no
/// need for `abort()` here).
fn run_flow_program_watchdogged(program: &str, env: &[(&str, &str)], limit: std::time::Duration) -> String {
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_mova"));
    cmd.arg("-e").arg(program);
    cmd.stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    for k in ["MOVA_NO_FUSION", "MOVA_FUSE_ALL", "MOVA_NO_FASTSTEP", "MOVA_NO_SPSC"] {
        cmd.env_remove(k);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().expect("failed to spawn the mova binary");
    let deadline = std::time::Instant::now() + limit;
    loop {
        match child.try_wait().expect("try_wait") {
            Some(status) => {
                let out = child.wait_with_output().expect("wait_with_output");
                assert!(
                    status.success(),
                    "mova exited with {:?}: {}",
                    status,
                    String::from_utf8_lossy(&out.stderr)
                );
                return String::from_utf8_lossy(&out.stdout).trim().to_string();
            }
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                panic!(
                    "flow chaos stress did not finish within {limit:?} under env {env:?} -- \
                     this is the lost-wake/deadlock class the transport's wake protocol exists \
                     to rule out (src/transport.rs's invariants, bench/optimization-log.md's E2 \
                     entry). Env: {env:?}"
                );
            }
            None => std::thread::sleep(std::time::Duration::from_millis(20)),
        }
    }
}

/// The deep-pipeline chaos stress, run in BOTH transport modes.
///
/// 12 procs in an unbranching chain -- so under `MOVA_NO_FUSION=1` every
/// one of its 11 conns is transport-backed, and under `MOVA_NO_SPSC=1`
/// none is -- driven through 25 rounds of "deliver everything through a
/// pause/resume storm" plus 25 rounds of "stop mid-stream and join
/// promptly", half of those while the flow is paused (every proc parked on
/// control) and half while it is running.
///
/// What it is actually guarding:
/// - a `pause` landing while a proc is parked on a transport, or blocked
///   sending into a full one, must still be noticed -- control priority is
///   the property the whole bounded-wait composition exists to preserve;
/// - `resume` after that storm must be LOSSLESS and ORDER-PRESERVING
///   across every converted hop (the rounds compare against `(range N)`
///   exactly, not just by count);
/// - `stop` must wake and join every proc within seconds no matter which
///   wait each one happened to be sitting in.
#[test]
fn deep_pipeline_chaos_stress_survives_pause_resume_stop_storms_in_both_transport_modes() {
    const PROGRAM: &str = r#"
(def HOPS 12)
(def N 1500)
(def ROUNDS 25)

(def seed (atom 20260815))
(defn nxt! [] (swap! seed (fn [s] (rem (+ (* s 1103515245) 12345) 2147483647))))
(defn jitter [k] (rem (nxt!) k))

(def relay
  (flow/map->step
   {:describe (fn [] {:ins {:in {}} :outs {:out {}}})
    :init (fn [_] {:n 0})
    :transform (fn [s _ m] [(update s :n inc) {:out [m]}])}))

(defn mk-flow [out-ch]
  (let [sink (flow/map->step
              {:describe (fn [] {:ins {:in {}} :outs {}})
               :init (fn [_] {:n 0})
               :transform (fn [s _ m] (>!! out-ch m) [(update s :n inc) {}])})
        pids (vec (map (fn [i] (keyword (str "r" i))) (range (dec HOPS))))
        procs (assoc (into {} (map (fn [p] [p {:proc (flow/process relay)}]) pids))
                     :sink {:proc (flow/process sink)})
        chain (conj pids :sink)
        conns (vec (map (fn [a b] [[a :out] [b :in]]) chain (rest chain)))]
    [(flow/create-flow {:procs procs :conns conns}) (first chain)]))

;; Rounds that must deliver EVERYTHING, in order, despite a pause/resume storm.
(def full-ok
  (vec
   (map
    (fn [r]
      (let [out-ch (chan (inc N))
            [fl head] (mk-flow out-ch)]
        (flow/start fl)
        (flow/resume fl)
        (flow/inject fl [head :in] (range N))
        (let [chaos (future
                      (loop [i 0]
                        (when (< i 5)
                          (sleep-ms (jitter 6))
                          (flow/pause fl)
                          (sleep-ms (jitter 4))
                          (flow/resume fl)
                          (recur (inc i))))
                      :done)
              got (loop [acc [] k 0]
                    (if (= k N) acc (recur (conj acc (<!! out-ch)) (inc k))))]
          (deref chaos 20000 :timeout)
          (flow/stop fl)
          (= got (vec (range N))))))
    (range ROUNDS))))

;; Rounds that STOP mid-stream: nothing may hang and stop must return fast.
(def stop-ok
  (vec
   (map
    (fn [r]
      (let [out-ch (chan (inc N))
            [fl head] (mk-flow out-ch)]
        (flow/start fl)
        (flow/resume fl)
        (flow/inject fl [head :in] (range N))
        (sleep-ms (jitter 12))
        ;; half the rounds stop a RUNNING flow mid-stream, half stop a
        ;; PAUSED one (every proc parked on control, waiting for a command)
        (when (= 0 (rem r 2)) (flow/pause fl))
        (let [t0 (time-ms)
              _ (flow/stop fl)
              elapsed (- (time-ms) t0)]
          (< elapsed 3000))))
    (range ROUNDS))))

(println (pr-str [(count full-ok) (every? true? full-ok)
                  (count stop-ok) (every? true? stop-ok)]))
"#;

    let limit = std::time::Duration::from_secs(120);
    let on = run_flow_program_watchdogged(PROGRAM, SPSC_ON, limit);
    assert_eq!(on, "[25 true 25 true]\nnil", "chaos stress failed with the transport ON");
    let off = run_flow_program_watchdogged(PROGRAM, SPSC_OFF, limit);
    assert_eq!(off, "[25 true 25 true]\nnil", "chaos stress failed with the transport OFF");
    // ...and with FUSION on too, so the third tier's own chaos path (a run
    // that fuses, and therefore has no transport at all) is covered by the
    // same storm rather than assumed.
    let fused = run_flow_program_watchdogged(PROGRAM, FUSE_ALL, limit);
    assert_eq!(fused, "[25 true 25 true]\nnil", "chaos stress failed with the chain FUSED");
}
