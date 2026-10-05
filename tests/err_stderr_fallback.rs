//! W-EMBED: verifies the CLI-level guarantee behind `write_shim_err`'s
//! real-stderr fallback (`src/builtins/nsfns.rs`) -- a warning raised with
//! nothing bound to `*err*` reaches the process's REAL stderr, not just an
//! in-process capture. `tests/embed_capture.rs` covers the in-process
//! capture side (`Engine::eval_capture`); this file is the one place that
//! spawns the actual `mova` binary and inspects its real stderr stream,
//! since that is not observable through the embed facade at all.

use std::process::Command;

#[test]
fn unbound_warning_reaches_real_process_stderr() {
    let bin = env!("CARGO_BIN_EXE_mova");
    let output = Command::new(bin)
        .arg("-e")
        .arg("(set! *warn-on-reflection* true) (defn foo [x] (.blah x))")
        .output()
        .expect("failed to spawn mova binary");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Reflection warning"),
        "expected a reflection warning on real stderr, got: {stderr:?}"
    );
}

#[test]
fn quiet_program_leaves_stderr_empty() {
    let bin = env!("CARGO_BIN_EXE_mova");
    let output = Command::new(bin)
        .arg("-e")
        .arg("(+ 1 2)")
        .output()
        .expect("failed to spawn mova binary");
    assert!(
        output.stderr.is_empty(),
        "expected no boot-time or eval-time stderr noise, got: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );
}
