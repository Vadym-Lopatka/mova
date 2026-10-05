//! Script-defined flow topology: the SCRIPT builds a small
//! `core.async.flow` pipeline (`:gen` ingests host-injected events,
//! `:xform` transforms them, `:sink` forwards results to a channel), and
//! the HOST drives it from Rust -- injecting input, draining output, and
//! finally calling [`Engine::shutdown`] to stop every flow the script
//! created and reading back the [`ShutdownReport`]. This is the "topology
//! lives in script, lifecycle is owned by the host" split a real embedder
//! wiring user-defined data pipelines would want.
//!
//! Run: `cargo run --release --example embed_pipeline`

use mova::embed::{Engine, Profile, Value};

fn main() {
    // `flow`/`chan`/`>!!`/`<!!` all live in the `conc`+`flow` capability
    // groups, so this needs `Profile::Scripting` (Pure has no thread
    // spawning at all -- see `crate::embed`'s module doc).
    let mut engine = Engine::builder().profile(Profile::Scripting).build();

    println!("-- script builds the topology: gen -> xform -> sink --");
    engine
        .eval(
            r#"
            (def out-ch (chan 32))

            (def gen
              (flow/map->step
               {:describe (fn [] {:ins {:in {}} :outs {:out {}}})
                :transform (fn [s _ m] [s {:out [m]}])}))

            (def xform
              (flow/map->step
               {:describe (fn [] {:ins {:in {}} :outs {:out {}}})
                :init (fn [_] {:count 0})
                :transform (fn [s _ m]
                             [(update s :count inc) {:out [(* m m)]}])}))

            (def sink
              (flow/map->step
               {:describe (fn [] {:ins {:in {}} :outs {}})
                :transform (fn [s _ m] (>!! out-ch m) [s {}])}))

            (def pipeline
              (flow/create-flow
               {:procs {:gen {:proc (flow/process gen)}
                        :xform {:proc (flow/process xform)}
                        :sink {:proc (flow/process sink)}}
                :conns [[[:gen :out] [:xform :in]]
                        [[:xform :out] [:sink :in]]]}))
            "#,
        )
        .expect("script should build the pipeline");
    println!("  3 procs (gen -> xform -> sink), all paused by default");

    println!();
    println!("-- host drives it: start, inject, drain --");
    engine.eval("(flow/start pipeline)").expect("start pipeline");
    engine.eval("(flow/resume pipeline)").expect("resume pipeline");

    // One `flow/inject` call with the whole batch, exactly like
    // `flow-tour.mova` does -- this is what keeps the batch's relative
    // order deterministic through the pipeline (separate inject calls
    // race against each proc's own thread instead of queuing FIFO).
    let events = [1i64, 2, 3, 4, 5];
    let messages = Value::vector(events.iter().map(|&n| Value::from(n)));
    let port = Value::vector([Value::keyword("gen"), Value::keyword("in")]);
    engine
        .call_by_name("flow/inject", &[engine.get("pipeline").unwrap(), port, messages])
        .expect("inject events");

    // `<!!` is a macro (not a plain callable native), so draining from Rust
    // goes through a one-line `eval` per read rather than `Engine::call` --
    // the ordinary way a host reaches a script-level macro form.
    let mut squares = Vec::new();
    for _ in &events {
        let v = engine.eval("(<!! out-ch)").expect("drain out-ch");
        squares.push(v.as_i64().unwrap());
    }
    println!("  injected {events:?}, drained squares {squares:?}");

    println!();
    println!("-- host shuts the pipeline down --");
    let report = engine.shutdown();
    println!("  ShutdownReport {{ flows_stopped: {}, flows_failed: {} }}", report.flows_stopped, report.flows_failed);

    // Idempotent: a second call finds nothing left to stop.
    let second = engine.shutdown();
    println!("  second shutdown() call: {{ flows_stopped: {}, flows_failed: {} }} (idempotent)", second.flows_stopped, second.flows_failed);
}
