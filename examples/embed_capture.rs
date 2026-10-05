//! `*out*`/`*err*` capture: a host that wants to SHOW a script's console
//! output somewhere other than the process's real stdout/stderr (a log
//! pane, a structured audit record, a test assertion) without losing any
//! of it -- not the script's normal `println` output, and not the
//! interpreter's own warnings (reflection, boxed-math, def-shadowing,
//! ...), which would otherwise go straight to the real process stderr.
//!
//! Run: `cargo run --release --example embed_capture`

use mova::embed::{Engine, Profile};

fn main() {
    let mut engine = Engine::builder().profile(Profile::Pure).build();

    // A well-behaved script: its `println` output and return value are
    // both captured, `stderr` stays empty.
    let (result, stdout, stderr) = engine.eval_capture(
        r#"(println "starting up")
           (println "step 1 done")
           (+ 1 2 3)"#,
    );
    println!("-- well-behaved script --");
    println!("result: {:?}", result.unwrap().as_i64());
    println!("captured stdout:\n{stdout}");
    println!("captured stderr: {stderr:?}");

    // A script that trips a reflection warning: the warning lands in
    // `stderr` instead of the real process stderr -- nothing is silently
    // dropped, and nothing leaks onto the host's own console.
    let (result, _stdout, stderr) = engine.eval_capture(
        r#"(set! *warn-on-reflection* true)
           (defn describe [x] (.blah x))"#,
    );
    println!("\n-- reflective script --");
    println!("result ok: {}", result.is_ok());
    println!("captured stderr:\n{stderr}");

    // Without `eval_capture`, the SAME warning would go straight to this
    // process's real stderr -- try it: `engine.eval(...)` on the same
    // source prints "Reflection warning, ..." to the terminal running
    // this example, not into a Rust `String` at all.
    println!("-- for comparison, via plain eval() (watch this terminal's stderr) --");
    let _ = engine.eval(r#"(defn describe2 [x] (.zap x))"#);
}
