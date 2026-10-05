//! L3/W3 probes P4a and P4b (docs/L3-FLOW-PROCS-DESIGN.md §5,
//! docs/L3-LANDING-SPEC.md §W3). NOT gates: every function here is
//! `#[ignore]`d and produces NUMBERS, which the principal turns into the
//! G-10K / G-HOP / G-CTL bars of design §6. Nothing here asserts a bar --
//! bars come from measurements, never the other way round (the W5c/L2
//! precedent).
//!
//! Run one at a time, on as quiet a machine as you can get:
//!
//! ```text
//! cargo test --release --test l3_placement_probe -- --ignored --exact p4a_chain_scaling --nocapture
//! cargo test --release --test l3_placement_probe -- --ignored --exact p4a_placement_ab --nocapture
//! cargo test --release --test l3_placement_probe -- --ignored --exact p4a_hop_table --nocapture
//! cargo test --release --test l3_placement_probe -- --ignored --exact p4b_hot_shard_control_latency --nocapture
//! ```
//!
//! **Shape.** Child processes, like `tests/l3_task_procs_test.rs` and
//! `tests/l2_direct_switch_test.rs`: every switch these probes move
//! (`MOVA_FLOW_PLACEMENT`, `MOVA_FLOW_THREAD_PROCS`, `MOVA_NO_FUSION`)
//! is an `OnceLock` read once per process, so two worlds cannot coexist in
//! one run -- and RSS, the thing P4a is measuring, is a property of a
//! process, not of a test. Each child prints `NAME=value` lines; the parent
//! parses them, derives, and prints a table.
//!
//! **What the chain program is, and why.** `flow/step-passthrough` ->
//! ... -> `flow/step-sink-deliver`: NATIVE steps, under `MOVA_NO_FUSION=1`.
//! Both halves of that are deliberate. Native, because an interpreted
//! `map->step` transform costs ~1 µs of interpreter per message and would
//! bury the hop -- and the hop is the whole question. `NO_FUSION`, because
//! the default policy fuses exactly the chains built out of native steps
//! (`plan_fusion`'s `PromotedOnly` clause), which would turn a 10,000-proc
//! chain into ONE task and measure nothing. An interpreted variant is
//! measured too (`P4A_INTERP_*`), as the honest upper bound on what a real
//! flow's per-message cost looks like.
//!
//! **What `NS_PER_HOP` means.** Wall time for M messages end-to-end divided
//! by M * (N-1) proc-to-proc hops. That is a THROUGHPUT number, not a
//! latency: a chain segmented across 14 shards runs its stages
//! concurrently, so the aggregate cost per hop is well below any single
//! hop's latency. It is the right number for "what does a 10k-proc graph
//! deliver", and the wrong number for "what does one hop cost" -- which is
//! what [`p4a_hop_table`] measures separately, on a 2-proc chain where
//! there is no pipeline to hide behind.

use std::time::Instant;

use mova::internal::{render, Interp};
use mova::runtime;

// ===========================================================================
// Child-process plumbing (same shape as tests/l3_task_procs_test.rs)
// ===========================================================================

/// Run one `#[ignore]`d worker below in a fresh process with `envs` set, and
/// return its stdout. Panics with the child's full output on failure.
fn run_child(worker: &str, envs: &[(&str, String)]) -> String {
    let exe = std::env::current_exe().expect("current_exe");
    let mut cmd = std::process::Command::new(exe);
    cmd.args([worker, "--exact", "--ignored", "--nocapture"]);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let out = cmd.output().unwrap_or_else(|e| panic!("failed to spawn the W3 probe worker {worker}: {e}"));
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "the W3 probe worker {worker} failed (envs={envs:?}); status={:?}\nstdout={stdout}\nstderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    stdout
}

fn field<'a>(stdout: &'a str, name: &str) -> &'a str {
    stdout
        .lines()
        .find_map(|l| l.strip_prefix(name))
        .unwrap_or_else(|| panic!("the W3 probe worker did not print {name}\nstdout={stdout}"))
        .trim()
}

fn field_u64(stdout: &str, name: &str) -> u64 {
    field(stdout, name).parse().unwrap_or_else(|e| panic!("{name} was not an integer: {e}\nstdout={stdout}"))
}

/// `(k, v)` pairs, spelled once.
fn env(pairs: &[(&'static str, &str)]) -> Vec<(&'static str, String)> {
    pairs.iter().map(|(k, v)| (*k, (*v).to_string())).collect()
}

/// Resident set size of THIS process, in KiB, via `ps` -- the portable-
/// enough answer on macOS (`/proc` does not exist; `proc_pidinfo` would
/// mean a libc dependency this crate does not have). Reported as `0` if
/// `ps` is unavailable, so a probe on an exotic host degrades to "no RSS
/// number" instead of failing.
fn rss_kb() -> u64 {
    let pid = std::process::id().to_string();
    std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid])
        .output()
        .ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse::<u64>().ok())
        .unwrap_or(0)
}

// ===========================================================================
// Small stats, for the parents' tables
// ===========================================================================

fn median(mut v: Vec<u64>) -> u64 {
    v.sort_unstable();
    if v.is_empty() {
        0
    } else {
        v[v.len() / 2]
    }
}

// ===========================================================================
// P4a -- the 10k-proc bench
// ===========================================================================

/// `p4a_chain_child`'s knobs, all through the environment (an `#[ignore]`d
/// worker takes no arguments).
struct ChainCfg {
    /// Procs in the chain: `p0` .. `p(n-1)`, the last one the sink.
    n: u64,
    /// Messages injected at the head and counted at the sink.
    m: u64,
    /// Every in-port's `:buf-or-n`.
    buf: u64,
    /// `native` (passthrough/sink-deliver) or `interp` (`map->step`).
    step: &'static str,
    /// `io` gives every proc `:workload :io` (threads + transport lanes);
    /// anything else leaves them at the default (`:mixed` -> tasks).
    workload: &'static str,
}

impl ChainCfg {
    fn envs(&self, extra: &[(&'static str, &str)]) -> Vec<(&'static str, String)> {
        let mut v = vec![
            ("PROBE_N", self.n.to_string()),
            ("PROBE_M", self.m.to_string()),
            ("PROBE_BUF", self.buf.to_string()),
            ("PROBE_STEP", self.step.to_string()),
            ("PROBE_WORKLOAD", self.workload.to_string()),
            // See the module doc: the default policy would fuse a native
            // chain into one run and there would be no chain left to
            // measure.
            ("MOVA_NO_FUSION", "1".to_string()),
        ];
        v.extend(env(extra));
        v
    }
}

/// One chain run's raw numbers.
struct ChainRun {
    start_ns: u64,
    run_ns: u64,
    stop_ns: u64,
    rss_base_kb: u64,
    rss_start_kb: u64,
    tasks: u64,
    threads: u64,
}

fn chain_run(cfg: &ChainCfg, extra: &[(&'static str, &str)]) -> ChainRun {
    let out = run_child("p4a_chain_child", &cfg.envs(extra));
    ChainRun {
        start_ns: field_u64(&out, "START_NS="),
        run_ns: field_u64(&out, "RUN_NS="),
        stop_ns: field_u64(&out, "STOP_NS="),
        rss_base_kb: field_u64(&out, "RSS_BASE_KB="),
        rss_start_kb: field_u64(&out, "RSS_START_KB="),
        tasks: field_u64(&out, "TASKS="),
        threads: field_u64(&out, "PROC_THREADS="),
    }
}

fn ns_per_hop(run: &ChainRun, cfg: &ChainCfg) -> f64 {
    run.run_ns as f64 / (cfg.m as f64 * (cfg.n - 1) as f64)
}

/// **P4a, part 1: does a 10,000-proc flow start, fit, and flow?**
///
/// Start wall time, idle RSS after start, and steady-state ns/hop at
/// N = 100 / 1000 / 10000. `M` is picked per N so each run lands in the
/// 1-5 s window the spec asks for; if the machine turns out much faster or
/// slower than the sizing below assumes, the printed `RUN_NS` says so and
/// the constant is the one thing to move.
#[test]
#[ignore]
fn p4a_chain_scaling() {
    // (N, M): M * (N-1) total hops, sized off a ~100 ns/hop first guess.
    let sizes: [(u64, u64); 3] = [(100, 200_000), (1_000, 20_000), (10_000, 5_000)];
    for (n, m) in sizes {
        let cfg = ChainCfg { n, m, buf: 64, step: "native", workload: "" };
        let r = chain_run(&cfg, &[]);
        println!("P4A_N{n}_START_MS={:.1}", r.start_ns as f64 / 1e6);
        println!("P4A_N{n}_RSS_BASE_KB={}", r.rss_base_kb);
        println!("P4A_N{n}_RSS_AFTER_START_KB={}", r.rss_start_kb);
        println!("P4A_N{n}_RSS_DELTA_KB={}", r.rss_start_kb.saturating_sub(r.rss_base_kb));
        println!(
            "P4A_N{n}_RSS_PER_PROC_KB={:.2}",
            r.rss_start_kb.saturating_sub(r.rss_base_kb) as f64 / n as f64
        );
        println!("P4A_N{n}_TASKS={}", r.tasks);
        println!("P4A_N{n}_PROC_THREADS={}", r.threads);
        println!("P4A_N{n}_MSGS={m}");
        println!("P4A_N{n}_RUN_MS={:.1}", r.run_ns as f64 / 1e6);
        println!("P4A_N{n}_NS_PER_HOP={:.1}", ns_per_hop(&r, &cfg));
        println!("P4A_N{n}_MHOPS_PER_SEC={:.2}", (m * (n - 1)) as f64 / (r.run_ns as f64 / 1e3));
        println!("P4A_N{n}_STOP_MS={:.1}", r.stop_ns as f64 / 1e6);
    }

    // The interpreted upper bound, at one size, so the native numbers above
    // can be read in proportion to what a real `map->step` proc costs.
    let cfg = ChainCfg { n: 1_000, m: 2_000, buf: 64, step: "interp", workload: "" };
    let r = chain_run(&cfg, &[]);
    println!("P4A_INTERP_N1000_START_MS={:.1}", r.start_ns as f64 / 1e6);
    println!("P4A_INTERP_N1000_RSS_DELTA_KB={}", r.rss_start_kb.saturating_sub(r.rss_base_kb));
    println!("P4A_INTERP_N1000_NS_PER_HOP={:.1}", ns_per_hop(&r, &cfg));
}

/// **P4a, part 2: segments vs blind round-robin, interleaved.**
///
/// The §3.6 justification or its refutation, at three chain lengths --
/// because the segment model's only claim is about chains LONGER than the
/// machine (a graph with fewer runs than shards gets one run per segment,
/// which IS round-robin; see `segment_shards`' doc), and the interesting
/// variable is therefore segment LENGTH: N/14 procs deep on this box.
///
/// A third arm, `pin:0` (the whole chain on ONE shard), is measured at the
/// two smaller sizes as the degenerate endpoint of the same formula -- it
/// is what design §3.6's first sketch, "chain-affine, whole chain, one
/// shard", would have shipped.
///
/// Runs alternate seg / rr / (pin) so the box's drift lands on every arm
/// equally.
#[test]
#[ignore]
fn p4a_placement_ab() {
    for (n, m, with_pin) in [(100u64, 200_000u64, true), (1_000, 20_000, true), (10_000, 5_000, false)] {
        let cfg = ChainCfg { n, m, buf: 64, step: "native", workload: "" };
        let hops = (m * (n - 1)) as f64;
        let (mut seg, mut rr, mut pin): (Vec<u64>, Vec<u64>, Vec<u64>) = (vec![], vec![], vec![]);
        for round in 0..2 {
            let s = chain_run(&cfg, &[]);
            println!("P4A_AB_N{n}_SEG_RUN{round}_NS_PER_HOP={:.1}", ns_per_hop(&s, &cfg));
            seg.push(s.run_ns);
            let r = chain_run(&cfg, &[("MOVA_FLOW_PLACEMENT", "rr")]);
            println!("P4A_AB_N{n}_RR_RUN{round}_NS_PER_HOP={:.1}", ns_per_hop(&r, &cfg));
            rr.push(r.run_ns);
            if with_pin {
                let p = chain_run(&cfg, &[("MOVA_FLOW_PLACEMENT", "pin:0")]);
                println!("P4A_AB_N{n}_PIN1_RUN{round}_NS_PER_HOP={:.1}", ns_per_hop(&p, &cfg));
                pin.push(p.run_ns);
            }
        }
        let seg_med = median(seg) as f64 / hops;
        let rr_med = median(rr) as f64 / hops;
        println!("P4A_AB_N{n}_SEG_MEDIAN_NS_PER_HOP={seg_med:.1}");
        println!("P4A_AB_N{n}_RR_MEDIAN_NS_PER_HOP={rr_med:.1}");
        // What the segment length WOULD be with no cap; the shipped cap is
        // `MAX_SEGMENT_LEN` in src/builtins/flow.rs, and the sweep probe
        // above is where it came from.
        println!("P4A_AB_N{n}_SEG_LEN_UNCAPPED_PROCS={}", n.div_ceil(runtime::shard_count() as u64));
        // > 1 means segments win; < 1 means blind round-robin wins.
        println!("P4A_AB_N{n}_SEG_SPEEDUP_X={:.2}", rr_med / seg_med);
        if with_pin {
            let pin_med = median(pin) as f64 / hops;
            println!("P4A_AB_N{n}_PIN1_MEDIAN_NS_PER_HOP={pin_med:.1}");
            println!("P4A_AB_N{n}_SEG_VS_PIN1_X={:.2}", pin_med / seg_med);
        }
    }
    println!("P4A_AB_SHARDS={}", runtime::shard_count());
}

/// **P4a, part 2b: the segment-LENGTH sweep** -- where the crossover is.
///
/// Part 2 shows segments beating round-robin at segment lengths 8 and 72 and
/// LOSING at 715. The variable is therefore not "segments vs not" but how
/// long a serial stretch of one pipeline a single shard should own. This
/// sweeps `MOVA_FLOW_PLACEMENT=seg:<L>` over a 10,000-proc chain, with `rr`
/// interleaved between every L so the comparison is against a
/// contemporaneous baseline rather than one measured minutes earlier.
/// `seg:10000` is the whole-chain-per-14-shards shape part 2 measured.
#[test]
#[ignore]
fn p4a_segment_length_sweep() {
    let cfg = ChainCfg { n: 10_000, m: 5_000, buf: 64, step: "native", workload: "" };
    let hops = (cfg.m * (cfg.n - 1)) as f64;
    let mut rr: Vec<u64> = Vec::new();
    for l in [4u64, 8, 16, 32, 64, 128, 512, 10_000] {
        let seg = chain_run(&cfg, &[("MOVA_FLOW_PLACEMENT", &format!("seg:{l}"))]);
        println!("P4A_SWEEP_SEG{l}_NS_PER_HOP={:.1}", ns_per_hop(&seg, &cfg));
        rr.push(chain_run(&cfg, &[("MOVA_FLOW_PLACEMENT", "rr")]).run_ns);
    }
    println!("P4A_SWEEP_RR_MEDIAN_NS_PER_HOP={:.1}", median(rr) as f64 / hops);
}

/// **P4a, part 3: the three-way hop table** (design §3.2's claim, measured).
///
/// Three transports for one proc-to-proc hop:
///
/// - `CO_SHARD`: `MOVA_FLOW_PLACEMENT=pin:0`, every proc on shard 0, so
///   every hop is a same-shard task rendezvous. The segment placement does
///   NOT produce this for a short chain (a 2-run graph is 2 segments on 2
///   shards), which is why the `pin:` probe seam exists.
/// - `CROSS_SHARD`: `MOVA_FLOW_PLACEMENT=rr`, blind round-robin, so
///   consecutive procs land on consecutive shards and every hop crosses.
/// - `IO_TRANSPORT`: every proc `:workload :io`, so they are OS threads AND
///   the conns keep their `transport.rs` `SpscRing` lanes -- the pre-L3 best
///   case, and the thing task procs give up (design §3.2).
///
/// TWO numbers per row, because neither alone is honest:
///
/// - `_NS` is total wall time per hop on the chain. On a 2-proc chain that
///   is dominated by FIXED costs -- the injector thread's cross-thread put,
///   the sink's promise -- which are the same in all three rows, so only the
///   DIFFERENCES between rows mean anything.
/// - `_MARGINAL_NS` prices ONE hop by differencing two chain lengths under
///   the identical placement: `(t(N=6) - t(N=2)) / (4 * M)`. Every fixed
///   cost cancels. Caveat, stated rather than hidden: for `CROSS_SHARD` and
///   `IO_TRANSPORT` the longer chain also gets more CORES, so their marginal
///   number is a hop cost net of added parallelism and can read low (or
///   negative); only `CO_SHARD`'s marginal is a clean single-core hop price.
#[test]
#[ignore]
fn p4a_hop_table() {
    /// `(row label, `:workload`, extra environment)` for one transport.
    type Arm = (&'static str, &'static str, &'static [(&'static str, &'static str)]);
    let arms: [Arm; 3] = [
        ("CO_SHARD", "", &[("MOVA_FLOW_PLACEMENT", "pin:0")]),
        ("CROSS_SHARD", "", &[("MOVA_FLOW_PLACEMENT", "rr")]),
        ("IO_TRANSPORT", "io", &[]),
    ];
    for (label, n, m) in [("2PROC", 2u64, 1_000_000u64), ("16PROC", 16, 200_000)] {
        for (arm, workload, extra) in arms {
            let cfg = ChainCfg { n, m, buf: 64, step: "native", workload };
            // Two runs, and the arms are visited in the same order each
            // round, so drift lands on all three equally.
            let mut runs: Vec<u64> = Vec::new();
            for _ in 0..2 {
                runs.push(chain_run(&cfg, extra).run_ns);
            }
            println!("P4A_HOP_{label}_{arm}_NS={:.1}", median(runs) as f64 / (m * (n - 1)) as f64);
        }
    }

    // The fixed-cost-cancelling marginal hop, N=2 -> N=6.
    const MARGIN_M: u64 = 1_000_000;
    for (arm, workload, extra) in arms {
        let short = ChainCfg { n: 2, m: MARGIN_M, buf: 64, step: "native", workload };
        let long = ChainCfg { n: 6, m: MARGIN_M, buf: 64, step: "native", workload };
        let mut deltas: Vec<i64> = Vec::new();
        for _ in 0..2 {
            let s = chain_run(&short, extra).run_ns as i64;
            let l = chain_run(&long, extra).run_ns as i64;
            deltas.push(l - s);
        }
        deltas.sort_unstable();
        println!("P4A_HOP_{arm}_MARGINAL_NS={:.1}", deltas[deltas.len() / 2] as f64 / (4 * MARGIN_M) as f64);
    }
}

// ===========================================================================
// P4b -- control latency on a hot shard
// ===========================================================================

/// **P4b: `flow/pause` -> quiescent, with the flow's procs sharing a shard
/// with a saturating noise graph.**
///
/// The scenario, all inside one child process:
///
/// - a NOISE flow: a 12-proc interpreted ring with 24 messages circulating
///   forever, so every one of its procs is runnable at all times;
/// - the MEASURED flow: source -> relay -> sink, fed continuously from an
///   external chan by an OS thread, sink writing into `sink-ch`;
/// - `MOVA_FLOW_PLACEMENT=pin:0` puts BOTH flows' task procs on shard 0,
///   which is what makes the shard hot AND puts the measured graph on it
///   (spec §W3.4's "pin a flow graph's segment there too").
///
/// A round is: let it run, drain the backlog, then time
/// `(flow/pause) + drain-until-silent`. The drain's final `alts!!` timeout
/// is subtracted using a CALIBRATED constant measured on the same idle
/// expression, not its nominal 1 ms -- so the resolution is the timer
/// service's jitter rather than a whole millisecond.
///
/// The `MOVA_FLOW_THREAD_PROCS=1` arm is the baseline: identical program,
/// procs as OS threads, `pin:` inert (an OS thread's placement is the OS's).
#[test]
#[ignore]
fn p4b_hot_shard_control_latency() {
    for (world, extra) in [
        // The COLD control arm: no noise ring, so the hot numbers can be
        // read against an idle shard instead of against nothing.
        ("TASK_COLD", vec![("MOVA_FLOW_PLACEMENT", "pin:0"), ("PROBE_NOISE_PROCS", "0")]),
        ("TASK_HOT", vec![("MOVA_FLOW_PLACEMENT", "pin:0")]),
        ("THREAD_COLD", vec![("MOVA_FLOW_THREAD_PROCS", "1"), ("PROBE_NOISE_PROCS", "0")]),
        ("THREAD_HOT", vec![("MOVA_FLOW_PLACEMENT", "pin:0"), ("MOVA_FLOW_THREAD_PROCS", "1")]),
    ] {
        let out = run_child("p4b_hot_shard_child", &env(&extra));
        let nums = |name: &str| -> Vec<u64> {
            field(&out, name).split(',').filter(|s| !s.is_empty()).map(|s| s.parse().expect("ns")).collect()
        };
        let lat = nums("LAT_NS=");
        let cmd = nums("CMD_NS=");
        let mut sorted = lat.clone();
        sorted.sort_unstable();
        println!("P4B_{world}_ROUNDS={}", lat.len());
        println!("P4B_{world}_NOISE_PROCS={}", field_u64(&out, "NOISE_PROCS="));
        println!("P4B_{world}_NOISE_TASKS={}", field_u64(&out, "NOISE_TASKS="));
        println!("P4B_{world}_MEASURED_TASKS={}", field_u64(&out, "MEASURED_TASKS="));
        println!("P4B_{world}_NS_PER_POLL={}", field_u64(&out, "NS_PER_POLL="));
        println!("P4B_{world}_HOT_MSGS_PER_SEC={}", field_u64(&out, "HOT_MSGS_PER_SEC="));
        // The resolution floor: the sink emits one message every this many
        // ns, so a quiescent latency below it is "under one message", not a
        // measured zero.
        let mps = field_u64(&out, "HOT_MSGS_PER_SEC=").max(1);
        println!("P4B_{world}_MSG_PERIOD_NS={}", 1_000_000_000 / mps);
        println!("P4B_{world}_POST_PAUSE_MSGS={}", field_u64(&out, "POST_PAUSE_MSGS="));
        println!("P4B_{world}_QUIESCENT_MIN_NS={}", sorted.first().copied().unwrap_or(0));
        println!("P4B_{world}_QUIESCENT_MED_NS={}", median(lat.clone()));
        println!("P4B_{world}_QUIESCENT_MAX_NS={}", sorted.last().copied().unwrap_or(0));
        println!("P4B_{world}_PAUSE_CMD_MED_NS={}", median(cmd.clone()));
        println!("P4B_{world}_ALL_QUIESCENT_NS={}", field(&out, "LAT_NS="));
    }
}

// ===========================================================================
// The child workers
// ===========================================================================

fn env_str(k: &str, dflt: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| dflt.to_string())
}

fn env_u64(k: &str, dflt: u64) -> u64 {
    std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(dflt)
}

/// `eval_str` or die with a rendered error -- these programs are built by
/// `format!`, so a syntax slip has to point at itself.
fn ev(interp: &mut Interp, src: &str) -> mova::internal::Value {
    interp
        .eval_str("w3-probe", src)
        .unwrap_or_else(|e| panic!("W3 probe eval failed: {}\nsrc={src}", render(&e, "w3-probe", src)))
}

/// The N-proc chain, built PROGRAMMATICALLY (a `format!`ed 10,000-proc map
/// literal would be a megabyte of source and would measure the reader).
///
/// `p0 .. p(n-2)` relay, `p(n-1)` counts to `m` and delivers `done`.
fn chain_program(cfg: &ChainCfg) -> String {
    let (n, m, buf) = (cfg.n, cfg.m, cfg.buf);
    let opts = if cfg.workload == "io" { "{:workload :io}" } else { "{}" };
    let (relay, sink) = if cfg.step == "native" {
        (
            format!("(flow/process (flow/step-passthrough) {opts})"),
            format!("(flow/process (flow/step-sink-deliver {m} done) {opts})"),
        )
    } else {
        (
            format!(
                "(flow/process (flow/map->step {{:describe (fn [] {{:ins {{:in {{}}}} :outs {{:out {{}}}}}}) \
                 :transform (fn [s _ msg] [s {{:out [msg]}}])}}) {opts})"
            ),
            // Interpreted sinks still deliver through the native counter's
            // promise so both variants end the same way: an interpreted
            // `deliver` per message would be a second thing being measured.
            format!("(flow/process (flow/step-sink-deliver {m} done) {opts})"),
        )
    };
    format!(
        r#"(do
             (def done (promise))
             (def relay {relay})
             (def sink {sink})
             (def popts {{:chan-opts {{:in {{:buf-or-n {buf}}}}}}})
             (def procs
               (reduce (fn [acc i] (assoc acc (keyword (str "p" i)) (assoc popts :proc relay)))
                       {{(keyword (str "p" {n1})) (assoc popts :proc sink)}}
                       (range {n1})))
             (def conns
               (mapv (fn [i] [[(keyword (str "p" i)) :out] [(keyword (str "p" (inc i))) :in]])
                     (range {n1})))
             (def fl (flow/create-flow {{:procs procs :conns conns}}))
             (def msgs (vec (range {m})))
             :built)"#,
        n1 = n - 1
    )
}

/// P4a's worker: build, start (timed), run M messages (timed), stop
/// (timed), with RSS sampled around the start.
#[test]
#[ignore]
fn p4a_chain_child() {
    let cfg = ChainCfg {
        n: env_u64("PROBE_N", 100),
        m: env_u64("PROBE_M", 1000),
        buf: env_u64("PROBE_BUF", 64),
        step: if env_str("PROBE_STEP", "native") == "interp" { "interp" } else { "native" },
        workload: if env_str("PROBE_WORKLOAD", "") == "io" { "io" } else { "" },
    };
    let mut interp = Interp::new();
    // Warm the interpreter and the flow machinery on a throwaway 2-proc
    // flow, so N=100's numbers are not the first-touch numbers.
    let warm = ChainCfg { n: 2, m: 1, buf: 4, step: cfg.step, workload: cfg.workload };
    ev(&mut interp, &chain_program(&warm));
    ev(&mut interp, "(do (flow/start fl) (flow/resume fl) (flow/inject fl [:p0 :in] msgs) (deref done) (flow/stop fl))");

    let mut interp = Interp::new();
    ev(&mut interp, &chain_program(&cfg));

    let rss_base = rss_kb();
    let tasks_before = runtime::tasks_spawned();
    let t = Instant::now();
    ev(&mut interp, "(flow/start fl)");
    let start_ns = t.elapsed().as_nanos() as u64;
    let tasks = runtime::tasks_spawned() - tasks_before;
    ev(&mut interp, "(flow/resume fl)");
    // Sampled AFTER resume, so every proc has run its `init` and its
    // coroutine stack has been touched -- an idle-but-live graph, which is
    // what "idle RSS after start" means.
    let rss_start = rss_kb();

    let t = Instant::now();
    ev(&mut interp, "(do (flow/inject fl [:p0 :in] msgs) (deref done))");
    let run_ns = t.elapsed().as_nanos() as u64;

    let t = Instant::now();
    ev(&mut interp, "(flow/stop fl)");
    let stop_ns = t.elapsed().as_nanos() as u64;

    println!("RSS_BASE_KB={rss_base}");
    println!("RSS_START_KB={rss_start}");
    println!("START_NS={start_ns}");
    println!("RUN_NS={run_ns}");
    println!("STOP_NS={stop_ns}");
    println!("TASKS={tasks}");
    // Procs that stayed OS threads: N minus the task runs (fusion is off in
    // every probe run, so runs == procs).
    println!("PROC_THREADS={}", cfg.n.saturating_sub(tasks));
}

/// P4b's worker. See [`p4b_hot_shard_control_latency`] for the scenario.
///
/// **How the latency is measured, exactly.** The round times ONE eval:
/// `(do (flow/pause mfl) (drain-spin limit))`. `drain-spin` is a
/// `poll!` spin -- no timers, no parks -- that returns
/// `[last total]`: the poll index of the last message it saw, and how many
/// polls it did before `limit` consecutive empty ones convinced it the sink
/// had gone silent. With `ns_per_poll` calibrated on an idle spin, the
/// tail of empty polls is exactly `(total - last) * ns_per_poll` of the
/// measured wall time, so
///
/// ```text
/// pause -> quiescent  =  elapsed - (total - last) * ns_per_poll
/// ```
///
/// which keeps the pause command's own cost inside the number and takes the
/// silence detector's cost out of it. A `timeout`-chan poll (the obvious
/// first shape) cannot do this: its resolution is the timer service's
/// millisecond, three orders above the thing being measured.
#[test]
#[ignore]
fn p4b_hot_shard_child() {
    let noise_procs: u64 = env_u64("PROBE_NOISE_PROCS", 12);
    let noise_msgs: u64 = env_u64("PROBE_NOISE_MSGS", 24);
    let rounds: usize = env_u64("PROBE_ROUNDS", 20) as usize;
    /// How long the sink must stay empty before the spin calls it silent.
    /// 2 ms is ~40x the whole latency being measured on a good day and
    /// still only ~6% of a round.
    const SILENCE_NS: u64 = 2_000_000;

    let mut interp = Interp::new();

    // --- the noise ring: N interpreted relays with messages going round ---
    let relay = "(flow/process (flow/map->step {:describe (fn [] {:ins {:in {}} :outs {:out {}}}) \
                 :transform (fn [s _ msg] [s {:out [msg]}])}))";
    let noise = format!(
        r#"(do
             (def nrelay {relay})
             (def nopts {{:chan-opts {{:in {{:buf-or-n 64}}}}}})
             (def nprocs
               (reduce (fn [acc i] (assoc acc (keyword (str "n" i)) (assoc nopts :proc nrelay)))
                       {{}} (range {noise_procs})))
             (def nconns
               (mapv (fn [i] [[(keyword (str "n" i)) :out]
                              [(keyword (str "n" (mod (inc i) {noise_procs}))) :in]])
                     (range {noise_procs})))
             (def nfl (flow/create-flow {{:procs nprocs :conns nconns}}))
             :built)"#
    );
    // `PROBE_NOISE_PROCS=0` skips the ring entirely -- the COLD control arm,
    // so the hot-shard numbers can be read against an unloaded shard rather
    // than against nothing.
    let mut noise_tasks = 0;
    if noise_procs > 0 {
        ev(&mut interp, &noise);
        let before = runtime::tasks_spawned();
        ev(&mut interp, "(do (flow/start nfl) (flow/resume nfl))");
        noise_tasks = runtime::tasks_spawned() - before;
        ev(&mut interp, &format!("(flow/inject nfl [:n0 :in] (vec (range {noise_msgs})))"));
    }
    eprintln!("[p4b] noise ring: {noise_tasks} tasks, {noise_msgs} messages circulating");

    // --- the measured flow: source -> relay -> sink, fed continuously ---
    let measured = format!(
        r#"(do
             (def feed-ch (chan 256))
             ;; `dropping-buffer 1`: the sink must never BLOCK (that would be
             ;; a second backpressure story inside a control-latency probe)
             ;; and must never build a BACKLOG (a queue drained after the
             ;; pause would be miscounted as the flow still running). One
             ;; slot does both.
             (def sink-ch (chan (dropping-buffer 1)))
             (def emitted (atom 0))
             (def mrelay {relay})
             (def msink (flow/process (flow/map->step
               {{:describe (fn [] {{:ins {{:in {{}}}} :outs {{}}}})
                 :transform (fn [s _ msg] (swap! emitted inc) (>!! sink-ch msg) [s {{}}])}})))
             (def mfl (flow/create-flow
                        {{:procs {{:a {{:proc (flow/process (flow/step-source feed-ch))}}
                                  :b {{:proc mrelay :chan-opts {{:in {{:buf-or-n 64}}}}}}
                                  :c {{:proc msink :chan-opts {{:in {{:buf-or-n 64}}}}}}}}
                          :conns [[[:a :out] [:b :in]] [[:b :out] [:c :in]]]}}))
             ;; Returns [last total]: the poll index of the last message
             ;; seen, and the total polls done. Pure spin -- see this
             ;; worker's doc for why the timestamped arithmetic needs that.
             (defn drain-spin [limit]
               (loop [idle 0 total 0 last 0]
                 (if (= idle limit)
                   [last total]
                   (if (nil? (poll! sink-ch))
                     (recur (inc idle) (inc total) last)
                     (recur 0 (inc total) (inc total))))))
             :built)"#
    );
    ev(&mut interp, &measured);
    let before = runtime::tasks_spawned();
    ev(&mut interp, "(flow/start mfl)");
    let measured_tasks = runtime::tasks_spawned() - before;

    // CALIBRATION, with the measured flow started-but-not-resumed (so the
    // sink is silent) and the noise ring already hot (so the spin is priced
    // against the same loaded machine the rounds run on). All polls are
    // empty here, so elapsed/total IS the empty-poll cost.
    let mut per_poll: Vec<u64> = Vec::new();
    for _ in 0..11 {
        let t = Instant::now();
        let v = ev(&mut interp, "(drain-spin 20000)");
        let elapsed = t.elapsed().as_nanos() as u64;
        let (_, total) = as_pair(&v);
        per_poll.push(elapsed / total.max(1));
    }
    let ns_per_poll = median(per_poll).max(1);
    let limit = (SILENCE_NS / ns_per_poll).max(1000);
    eprintln!("[p4b] calibrated {ns_per_poll} ns/poll -> silence limit {limit} polls");

    // The feeder: ONE OS thread pushing into the source proc's external
    // chan until `stop` closes it. `flow/inject` spawns an injector thread
    // per call by design, which a continuous feed cannot use.
    ev(&mut interp, "(def feeder (future (loop [i 0] (if (>!! feed-ch i) (recur (inc i)) :done))))");
    ev(&mut interp, "(flow/resume mfl)");

    let mut lat: Vec<u64> = Vec::new();
    let mut cmd: Vec<u64> = Vec::new();
    let mut seen_total: u64 = 0;
    let mut emitted_total: u64 = 0;
    for round in 0..rounds {
        // Run hot, then time pause-to-silence.
        let before_emitted = as_int(&ev(&mut interp, "@emitted"));
        std::thread::sleep(std::time::Duration::from_millis(30));
        let hot_emitted = as_int(&ev(&mut interp, "@emitted"));

        // Two evals, timed separately: the command itself (how long
        // `flow/pause` takes to hand every proc its `:pause`), then the
        // spin (how long after that the sink keeps producing).
        let t = Instant::now();
        ev(&mut interp, "(flow/pause mfl)");
        let cmd_ns = t.elapsed().as_nanos() as u64;
        let v = ev(&mut interp, &format!("(drain-spin {limit})"));
        let (last, total) = as_pair(&v);
        debug_assert!(total >= last);
        // The last message the sink produced, in ns from the moment
        // `flow/pause` was called: the command, plus that message's poll
        // index priced at the calibrated empty-poll cost.
        let latency = cmd_ns + last * ns_per_poll;
        lat.push(latency);
        cmd.push(cmd_ns);
        seen_total += last;
        emitted_total += hot_emitted - before_emitted;
        eprintln!(
            "[p4b] round {round}: quiescent {latency} ns (cmd {cmd_ns} ns, last msg at poll {last}), \
             {} emitted in the hot 30 ms",
            hot_emitted - before_emitted
        );
        ev(&mut interp, "(flow/resume mfl)");
    }

    ev(&mut interp, "(flow/stop mfl)");
    if noise_procs > 0 {
        ev(&mut interp, "(flow/stop nfl)");
    }

    println!("NS_PER_POLL={ns_per_poll}");
    println!("SILENCE_LIMIT={limit}");
    // Both: in the THREAD world the ring's procs are OS threads, so
    // `NOISE_TASKS` is 0 while the ring is very much running -- the load is
    // `NOISE_PROCS`, and `HOT_MSGS_PER_SEC` is what shows it landing.
    println!("NOISE_PROCS={noise_procs}");
    println!("NOISE_TASKS={noise_tasks}");
    println!("MEASURED_TASKS={measured_tasks}");
    println!("POST_PAUSE_MSGS={seen_total}");
    println!("HOT_MSGS={emitted_total}");
    println!("HOT_MSGS_PER_SEC={}", emitted_total / (rounds as u64).max(1) * 1000 / 30);
    println!("LAT_NS={}", lat.iter().map(u64::to_string).collect::<Vec<_>>().join(","));
    println!("CMD_NS={}", cmd.iter().map(u64::to_string).collect::<Vec<_>>().join(","));
}

fn as_int(v: &mova::internal::Value) -> u64 {
    mova::internal::pr_str(v).parse::<i64>().unwrap_or(0).max(0) as u64
}

/// `[a b]` as printed by `pr_str` -- `drain-spin`'s return.
fn as_pair(v: &mova::internal::Value) -> (u64, u64) {
    let s = mova::internal::pr_str(v);
    let nums: Vec<u64> = s
        .trim_matches(|c| c == '[' || c == ']')
        .split_whitespace()
        .map(|t| t.parse().unwrap_or_else(|e| panic!("drain-spin returned {s}, which is not [last total]: {e}")))
        .collect();
    assert_eq!(nums.len(), 2, "drain-spin returned {s}, expected [last total]");
    (nums[0], nums[1])
}
