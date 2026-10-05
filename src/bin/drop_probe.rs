//! Design-spike probe for `embed/probe-snapshot` Part 2 (drop lifecycle):
//! one short-lived process per subcommand, each spawning a background
//! thing (future/go-loop/flow) that would still be "live" (sleeping /
//! parked / running) when the `Interp` that created it is dropped, then
//! measuring (a) how long the `drop` itself takes and (b) letting `main`
//! return immediately after -- an external `time` wrapper (see
//! `tests/embed_drop_test.rs`, which drives this via
//! `CARGO_BIN_EXE_drop_probe`) then observes total process wall time to
//! answer "does the process wait for the background thread, or does it
//! get killed at exit like any other OS thread would".
//!
//! NOT part of the public crate surface -- a throwaway measurement
//! harness, same spirit as `snapshot_bench`.

use std::time::Instant;

use mova::internal::Interp;

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_default();
    match mode.as_str() {
        "future" => run_future(),
        "go-loop" => run_go_loop(),
        "flow" => run_flow(),
        "baseline" => {} // no background thing at all: process-overhead control
        other => {
            eprintln!("usage: drop_probe <future|go-loop|flow|baseline>, got {other:?}");
            std::process::exit(2);
        }
    }
    println!("main: returning normally");
}

fn run_future() {
    let mut interp = Interp::new();
    interp
        .eval_str("probe", "(def f (future (sleep-ms 5000) :done))")
        .expect("future def failed");
    println!("future: spawned, dropping Interp now");
    let start = Instant::now();
    drop(interp);
    println!("future: Interp::drop took {:?}", start.elapsed());
}

fn run_go_loop() {
    let mut interp = Interp::new();
    // An unbuffered/never-closed channel: `<!` blocks forever, so the
    // go-loop's OS thread is parked indefinitely at drop time.
    interp
        .eval_str("probe", "(def ch (chan)) (def g (go-loop [] (<! ch) (recur)))")
        .expect("go-loop def failed");
    println!("go-loop: spawned (parked on empty chan), dropping Interp now");
    let start = Instant::now();
    drop(interp);
    println!("go-loop: Interp::drop took {:?}", start.elapsed());
}

fn run_flow() {
    let mut interp = Interp::new();
    interp
        .eval_str(
            "probe",
            r#"(def relay (flow/map->step
                             {:describe (fn [] {:ins {:in {}} :outs {:out {}}})
                              :transform (fn [s _ m] [s {:out [m]}])}))
               (def fl (flow/create-flow
                        {:procs {:r {:proc (flow/process relay)}}
                         :conns []}))
               (flow/start fl)
               (flow/resume fl)"#,
        )
        .expect("flow start failed");
    println!("flow: started and resumed (never stopped), dropping Interp now");
    let start = Instant::now();
    drop(interp);
    println!("flow: Interp::drop took {:?}", start.elapsed());
}
