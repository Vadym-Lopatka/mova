//! Real-process measurement harness for FLOW-IDLE-CPU-BUG.md's "Synthetic
//! (no host application needed)" repro recipe: builds a ~20-proc flow graph (a mix of
//! single- and multi-input procs, per the bug report's own arithmetic --
//! "9 multi-input flow procs... 12 single-input parkers"), starts and
//! resumes it, injects nothing, and then just sits idle so the process can
//! be measured from OUTSIDE with the same tools the bug report used:
//!
//!   top -l 2 -s 3 -pid <pid> -stats pid,cpu,th,csw
//!   sample <pid> 5 -file /tmp/flow_idle_probe.sample.txt
//!
//! This is NOT a `#[test]` -- `cargo test` can assert the wake mechanism's
//! internal behavior deterministically (see `tests/flow_wake_test.rs`'s
//! `Doorbell::safety_net_hits()`-based tests), but "how much CPU/how many
//! context switches does the real OS-level process burn" is a real-process
//! measurement, not something a unit test should try to shell out to
//! `top`/`sample` to assert on portably.
//!
//! Run: `cargo run --release --example flow_idle_probe`
//! (release matters here: a debug build's interpreter setup and per-proc
//! thread stacks are noisier to profile around than the steady-state idle
//! park loops this is actually trying to isolate.)
//!
//! Before this fix: ~1,000 wakeups/s per single-input proc + ~5,000/s per
//! multi-input proc (`PARK_TIMEOUT`=1ms / `MULTI_INPUT_BACKOFF`=200µs),
//! i.e. tens of thousands of context switches/s and double-digit percent
//! CPU for this exact graph, scaling linearly with proc count. After it:
//! acceptance criterion #1 says this whole process should sit under ~200
//! context switches/s and under 1% CPU, flat regardless of proc count
//! (criterion #2).

use mova::internal::Interp;
use std::time::Duration;

/// Builds `chain_count` independent chains of `chain_len` single-input
/// relay procs each (`c{N}-r{i}` -> ... -> `c{N}-sink`), plus
/// `multi_count` standalone multi-input procs (`m{i}`, 3 in-ports each,
/// wired to nothing -- idle by construction, exactly like a mounted
/// plugin's input ports nobody happens to be driving right now, which is
/// the common case FLOW-IDLE-CPU-BUG.md's field report describes).
fn program(chain_count: usize, chain_len: usize, multi_count: usize) -> String {
    format!(
        r#"
        (defn mk-relay [] (flow/map->step {{:describe (fn [] {{:ins {{:in {{}}}} :outs {{:out {{}}}}}})
                                             :transform (fn [s _ m] [s {{:out [m]}}])}}))
        (defn mk-sink [] (flow/map->step {{:describe (fn [] {{:ins {{:in {{}}}} :outs {{}}}})
                                            :transform (fn [s _ m] [s {{}}])}}))
        (defn mk-multi [] (flow/map->step {{:describe (fn [] {{:ins {{:a {{}} :b {{}} :c {{}}}} :outs {{}}}})
                                             :transform (fn [s _ m] [s {{}}])}}))

        (defn chain-relay-pids [chain-idx]
          (map (fn [i] (keyword (str "c" chain-idx "-r" i))) (range {chain_len})))
        (defn chain-sink-pid [chain-idx]
          (keyword (str "c" chain-idx "-sink")))

        (def chain-procs
          (into {{}}
                (mapcat (fn [chain-idx]
                          (map (fn [pid] [pid {{:proc (mk-relay)}}]) (chain-relay-pids chain-idx)))
                        (range {chain_count}))))
        (def sink-procs
          (into {{}}
                (map (fn [chain-idx] [(chain-sink-pid chain-idx) {{:proc (mk-sink)}}])
                     (range {chain_count}))))
        (def multi-procs
          (into {{}}
                (map (fn [i] [(keyword (str "m" i)) {{:proc (mk-multi)}}])
                     (range {multi_count}))))

        (def all-procs (merge chain-procs sink-procs multi-procs))

        (defn chain-conns [chain-idx]
          (let [pids (vec (chain-relay-pids chain-idx))
                pairs (map vector pids (rest pids))
                inner (map (fn [[a b]] [[a :out] [b :in]]) pairs)]
            (conj (vec inner) [[(last pids) :out] [(chain-sink-pid chain-idx) :in]])))

        (def all-conns (vec (mapcat chain-conns (range {chain_count}))))

        (def fl (flow/create-flow {{:procs all-procs :conns all-conns}}))
        (flow/start fl)
        (flow/resume fl)
        [(count all-procs) (count all-conns)]
        "#,
        chain_len = chain_len,
        chain_count = chain_count,
        multi_count = multi_count,
    )
}

fn main() {
    let pid = std::process::id();
    println!("flow_idle_probe: pid={pid}");
    println!("  measure with (from another shell):");
    println!("    top -l 2 -s 3 -pid {pid} -stats pid,cpu,th,csw");
    println!("    sample {pid} 5 -file /tmp/flow_idle_probe.sample.txt");
    println!();

    // 2 chains * 5 relays + 2 chain sinks = 12 single-input procs, +10
    // standalone multi-input procs = 22 total -- matches the bug report's
    // "~20 procs... roughly half single-input and half multi-input"
    // recipe closely enough to reproduce its arithmetic (12 single-input *
    // ~1kHz + 10 multi-input * ~5kHz was the pre-fix ~46k csw/s estimate).
    let src = program(2, 5, 10);

    let mut interp = Interp::new();
    let result = interp
        .eval_str("flow_idle_probe", &src)
        .unwrap_or_else(|e| panic!("flow_idle_probe: setup failed: {}", mova::internal::render(&e, "flow_idle_probe", &src)));
    println!("started: [proc-count conn-count] = {}", mova::internal::pr_str(&result));
    println!("resumed. sitting idle for 60s now -- zero injected traffic, nothing else running.");
    println!("(idle CPU should stay flat regardless of how many procs are above -- acceptance criterion #2.)");
    println!();
    println!(
        "NOTE: this topology's relay CHAINS have 1:1 unfused hops -- some of those are \
         transport (SPSC-lane)-backed. That tier is Doorbell-driven too now, same as every \
         other proc here (see builtins::flow's module doc, \"Composition with control/pause/ \
         stop\", and transport.rs's \"BOUNDED waits\" section for the mechanism; \
         tests/flow_wake_test.rs's idle_transport_backed_proc_does_not_fall_back_to_polling is \
         the regression test for it), so csw/CPU should be near-zero across the WHOLE process, \
         not just the standalone multi-input procs (m0..m9) and the chain heads -- see \
         examples/flow_idle_probe_transport.rs for a topology where EVERY hop is \
         transport-backed, isolating that tier specifically."
    );

    std::thread::sleep(Duration::from_secs(60));
    println!("done sleeping, exiting (no flow/stop -- process exit tears everything down).");
}
