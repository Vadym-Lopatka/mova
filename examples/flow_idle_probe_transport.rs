//! Companion to `examples/flow_idle_probe.rs`, built to close the specific
//! gap that probe's first run measured: an ALL-transport-eligible
//! topology, so `top -stats csw`/`sample` against THIS process isolates
//! the transport (SPSC-lane) tier specifically, with none of the
//! "some hops are transport, some aren't, which is which" ambiguity the
//! original probe's relay CHAINS had (only their middle hops were
//! transport-backed; each chain's head has no wired predecessor at all,
//! so it was never a hop of any kind).
//!
//! 10 independent 2-proc `relay -> sink` pairs, each `flow/map->step`
//! (interpreted) -- `plan_fusion` only ever groups a conn into a fusable
//! run when BOTH endpoints carry a native `StepFactory`
//! (`carries_step_factory`, checked at PLANNING time), so an interpreted
//! pair is never a fusion candidate and its ONE conn is transport-eligible
//! by default: no `MOVA_NO_FUSION` env var needed. That gives 10
//! transport-backed hops total, all idle, and nothing else -- no
//! standalone multi-input procs and no unconnected chain heads diluting
//! the measurement this time.
//!
//! Run: `cargo run --release --example flow_idle_probe_transport`
//!
//! Before the transport-tier `Doorbell` fix (see `builtins::flow`'s module
//! doc, "Composition with control/pause/stop", and `transport.rs`'s
//! "BOUNDED waits" section): each of these 10 hops polled at roughly 1kHz
//! even fully idle (`TRANSPORT_PARK_TIMEOUT` = 1ms, no live wake at all),
//! for ~10,000 wakeups/s process-wide. After it: each hop's proc parks on
//! the SAME long `PARK_TIMEOUT` safety net every other tier uses, woken
//! genuinely by `Doorbell::ring` rather than polling -- idle CPU/context
//! switches for this process should now be near-zero, the same as
//! `flow_idle_probe`'s non-transport procs already were.

use mova::internal::{Doorbell, Interp};
use std::time::Duration;

fn program(pair_count: usize) -> String {
    format!(
        r#"
        (defn mk-relay [] (flow/map->step {{:describe (fn [] {{:ins {{:in {{}}}} :outs {{:out {{}}}}}})
                                             :transform (fn [s _ m] [s {{:out [m]}}])}}))
        (defn mk-sink [] (flow/map->step {{:describe (fn [] {{:ins {{:in {{}}}} :outs {{}}}})
                                            :transform (fn [s _ m] [s {{}}])}}))

        (def relay-pid (fn [i] (keyword (str "relay" i))))
        (def sink-pid (fn [i] (keyword (str "sink" i))))

        (def all-procs
          (into {{}}
                (mapcat (fn [i] [[(relay-pid i) {{:proc (mk-relay)}}] [(sink-pid i) {{:proc (mk-sink)}}]])
                        (range {pair_count}))))
        (def all-conns
          (vec (map (fn [i] [[(relay-pid i) :out] [(sink-pid i) :in]]) (range {pair_count}))))

        (def fl (flow/create-flow {{:procs all-procs :conns all-conns}}))
        (flow/start fl)
        (flow/resume fl)
        [(count all-procs) (count all-conns)]
        "#,
        pair_count = pair_count,
    )
}

fn main() {
    let pid = std::process::id();
    println!("flow_idle_probe_transport: pid={pid}");
    println!("  measure with (from another shell):");
    println!("    top -l 2 -s 3 -pid {pid} -stats pid,cpu,th,csw");
    println!("    sample {pid} 5 -file /tmp/flow_idle_probe_transport.sample.txt");
    println!();

    let src = program(10);
    let mut interp = Interp::new();
    let result = interp.eval_str("flow_idle_probe_transport", &src).unwrap_or_else(|e| {
        panic!("flow_idle_probe_transport: setup failed: {}", mova::internal::render(&e, "flow_idle_probe_transport", &src))
    });
    println!("started: [proc-count conn-count] = {}", mova::internal::pr_str(&result));
    println!("resumed. sitting idle for 60s now -- zero injected traffic, nothing else running.");
    println!("every one of the 10 conns above should be a transport (SPSC-lane) hop.");

    // Also report the in-process `Doorbell::safety_net_hits()` delta over
    // the same window -- doesn't need an external tool, and folds in the
    // transport tier's own safety-net counter (transport.rs's
    // `Doorbell::note_external_safety_net_hit`) alongside the general
    // path's, so a near-zero delta here is corroborating evidence for
    // whatever `top`/`sample` report externally.
    let before = Doorbell::safety_net_hits();
    std::thread::sleep(Duration::from_secs(60));
    let after = Doorbell::safety_net_hits();
    println!("done sleeping. Doorbell::safety_net_hits() delta over 60s = {} (before={before}, after={after})", after - before);
    println!("(no flow/stop -- process exit tears everything down.)");
}
