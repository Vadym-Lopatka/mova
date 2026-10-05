//! field1/W-EXPLAIN: `MOVA_EXPLAIN=1` is a process-wide, checked-once
//! `OnceLock` (see `compile::explain::explain_enabled`), so the only way to
//! prove it is genuinely zero-cost-when-off AND genuinely wired up when on
//! is to spawn the real binary twice with different environments -- exactly
//! the pattern `tests/err_stderr_fallback.rs` uses for the sibling
//! `*warn-on-reflection*` diagnostic.

use std::process::Command;

/// `reify` is a type-system form that lives only in `eval::types_forms`
/// (see `compile::resolve::compile_special`), so the whole `reifier` fn
/// tree-walks and `compile_fn` records/reports exactly one explain line
/// for it.
///
/// field3/W-RESOLVE: this used to be `(defn dotty [x] (.toString x))`.
/// An interop CALL is no longer a whole-fn cliff -- it compiles to an
/// `Ir::Escape` -- so the "fn tree-walks" line needs a form that still
/// genuinely bails. The dot-form's new line is pinned by
/// [`escape_line_appears_with_env_var`] below.
const CLIFFY_SRC: &str = "(defn reifier [x] (reify Object (toString [_] x)))";

/// field3/W-RESOLVE: one interop call inside an otherwise-compilable fn.
const ESCAPE_SRC: &str = "(defn dotty [x] (.toString x))";

#[test]
fn explain_line_appears_with_env_var() {
    let bin = env!("CARGO_BIN_EXE_mova");
    let output = Command::new(bin)
        .arg("-e")
        .arg(CLIFFY_SRC)
        .env("MOVA_EXPLAIN", "1")
        .output()
        .expect("failed to spawn mova binary");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("explain: fn 'reifier' tree-walks"),
        "expected an explain line under MOVA_EXPLAIN=1, got: {stderr:?}"
    );
    assert!(
        stderr.contains("type system form"),
        "expected the type-system reason on the explain line, got: {stderr:?}"
    );
}

/// field3/W-RESOLVE, the honesty condition on `Ir::Escape`: a fn that
/// compiles with escapes must SAY so -- it did not compile cleanly, and
/// EXPLAIN silently claiming it did would be the dishonest option. Exactly
/// one line, and only for a fn that actually has an escape (a clean
/// compile stays silent, so the W-ADX 18->1 bootstrap-noise cut holds).
#[test]
fn escape_line_appears_with_env_var() {
    let bin = env!("CARGO_BIN_EXE_mova");
    let output = Command::new(bin)
        .arg("-e")
        .arg(ESCAPE_SRC)
        .env("MOVA_EXPLAIN", "1")
        .output()
        .expect("failed to spawn mova binary");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("explain: fn 'dotty' compiles with 1 interop escape"),
        "expected an escape explain line under MOVA_EXPLAIN=1, got: {stderr:?}"
    );
    assert!(
        !stderr.contains("tree-walks"),
        "an escaped interop call must NOT be reported as a whole-fn bail, got: {stderr:?}"
    );
    // W-ADX item 4c must not regress: the core bootstrap is full of
    // interop, and none of it may print.
    assert_eq!(
        stderr.matches("explain:").count(),
        1,
        "expected exactly one explain line (bootstrap stays silent), got: {stderr:?}"
    );
}

/// A clean fn -- no interop, no declined loop -- prints nothing at all,
/// even with the env var on.
#[test]
fn clean_fn_prints_no_explain_line() {
    let bin = env!("CARGO_BIN_EXE_mova");
    let output = Command::new(bin)
        .arg("-e")
        .arg("(defn clean [x] (+ x 1))")
        .env("MOVA_EXPLAIN", "1")
        .output()
        .expect("failed to spawn mova binary");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("explain:"),
        "expected NO explain line for a clean fn, got: {stderr:?}"
    );
}

#[test]
fn explain_line_absent_without_env_var() {
    let bin = env!("CARGO_BIN_EXE_mova");
    let output = Command::new(bin)
        .arg("-e")
        .arg(CLIFFY_SRC)
        .env_remove("MOVA_EXPLAIN")
        .output()
        .expect("failed to spawn mova binary");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("explain:"),
        "expected NO explain line without MOVA_EXPLAIN=1, got: {stderr:?}"
    );
}

/// Same idea for the loop-decline half: `(max i 1)` inside a `recur` arg is
/// not one of the arithmetic intrinsics the `NumLoop` grammar accepts, so
/// the loop stays generic and (under `MOVA_EXPLAIN=1`) reports why.
#[test]
fn explain_line_reports_a_declined_loop() {
    let bin = env!("CARGO_BIN_EXE_mova");
    let src = "(defn top-loop [n] (loop [i 0] (if (< i n) (recur (max i 1)) i)))";
    let output = Command::new(bin)
        .arg("-e")
        .arg(src)
        .env("MOVA_EXPLAIN", "1")
        .output()
        .expect("failed to spawn mova binary");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("explain: loop in 'top-loop' stays generic"),
        "expected a loop-decline explain line, got: {stderr:?}"
    );
}

/// field1/W-EXPLAIN follow-up: a bare TOP-LEVEL `loop` (outside any `fn`)
/// never reaches `compile::resolve` at all -- only fn bodies ever attempt
/// compilation -- so it had zero observable symptom until
/// `report_top_level_loop` (see `compile::explain`).
const TOP_LEVEL_LOOP_SRC: &str = "(loop [i 0] (if (< i 3) (recur (inc i)) i))";

#[test]
fn top_level_loop_explain_line_appears_with_env_var() {
    let bin = env!("CARGO_BIN_EXE_mova");
    let output = Command::new(bin)
        .arg("-e")
        .arg(TOP_LEVEL_LOOP_SRC)
        .env("MOVA_EXPLAIN", "1")
        .output()
        .expect("failed to spawn mova binary");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("top-level loop"),
        "expected a top-level-loop explain line under MOVA_EXPLAIN=1, got: {stderr:?}"
    );
}

#[test]
fn top_level_loop_explain_line_absent_without_env_var() {
    let bin = env!("CARGO_BIN_EXE_mova");
    let output = Command::new(bin)
        .arg("-e")
        .arg(TOP_LEVEL_LOOP_SRC)
        .env_remove("MOVA_EXPLAIN")
        .output()
        .expect("failed to spawn mova binary");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("top-level loop"),
        "expected NO top-level-loop explain line without MOVA_EXPLAIN=1, got: {stderr:?}"
    );
}

/// The SAME loop, but wrapped in a `defn` -- it now specializes cleanly
/// (a plain `inc`-driven counting loop is exactly the `NumLoop` grammar's
/// bread and butter), so neither the top-level-loop line NOR a decline line
/// should appear: this loop is not a cliff at all once it's inside a fn.
#[test]
fn loop_inside_defn_has_no_top_level_loop_line() {
    let bin = env!("CARGO_BIN_EXE_mova");
    let src = "(defn counter [] (loop [i 0] (if (< i 3) (recur (inc i)) i)))";
    let output = Command::new(bin)
        .arg("-e")
        .arg(src)
        .env("MOVA_EXPLAIN", "1")
        .output()
        .expect("failed to spawn mova binary");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("top-level loop"),
        "expected NO top-level-loop explain line for a loop inside a defn, got: {stderr:?}"
    );
    // NOTE: the core.mova bootstrap itself runs under `eval_form` too, so
    // `stderr` legitimately carries a pile of UNRELATED "stays generic"
    // lines for core's own loops (`filter`, `zipmap`, ...) even here --
    // asserting "no decline line anywhere in stderr" would be wrong. What
    // this test actually promises is that `counter`'s OWN loop specializes
    // cleanly (a plain `inc`-driven counting loop is exactly the `NumLoop`
    // grammar's bread and butter), so no decline line NAMES `counter`.
    assert!(
        !stderr.contains("loop in 'counter' stays generic"),
        "expected counter's own loop to specialize, not decline, got: {stderr:?}"
    );
}

/// `dotimes` is a `core.mova` macro that expands to `loop`, but this
/// module's walk is deliberately PRE-expansion (see
/// `compile::explain::find_top_level_loop`'s doc) and matches the raw
/// `dotimes` head directly, so a bare top-level `dotimes` is caught the
/// same way a bare top-level `loop` is.
#[test]
fn top_level_dotimes_explain_line_appears() {
    let bin = env!("CARGO_BIN_EXE_mova");
    let src = "(dotimes [i 3] i)";
    let output = Command::new(bin)
        .arg("-e")
        .arg(src)
        .env("MOVA_EXPLAIN", "1")
        .output()
        .expect("failed to spawn mova binary");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("top-level loop"),
        "expected a top-level-loop explain line for top-level dotimes, got: {stderr:?}"
    );
}

/// field5/W-SPAN: the actual regression this wave's fix targets -- two
/// SEPARATE, non-nested `eval_str` calls on the same `Interp` (exactly
/// what an embedder's `Engine::eval_named` gives, and what the REPL does
/// per submitted line): buffer A defines a fn that tree-walks, buffer B is
/// a DIFFERENT name and DIFFERENT (much shorter) content that becomes
/// current next, and a THIRD call reads back `(compile-explain ...)` for
/// A's fn. Pre-fix, `compile::explain::at`/`builtins::meta::at_string`
/// rendered the persisted span against WHATEVER `Interp::source`/
/// `source_name` were current at THAT THIRD call -- buffer B's, or the
/// query buffer's, never A's -- since a `Span`'s byte offsets carry no
/// buffer identity of their own (see `source_registry`'s module doc for
/// the full mechanism this replaces).
///
/// This is deliberately NOT a `Command::new(bin)` subprocess like every
/// other test in this file: those spawn the real binary because
/// `MOVA_EXPLAIN`'s `OnceLock` is checked-once-per-process, so proving it
/// genuinely responds to the env var needs a fresh process per case. This
/// test instead exercises `compile-explain`'s DATA return value (not an
/// env-gated stderr line), which the public `embed::Engine` API can drive
/// directly and assert on precisely -- an exact `name:line:col` equality,
/// not a substring search over captured stderr -- so it uses that instead
/// of a process spawn.
#[test]
fn compile_explain_survives_a_later_unrelated_eval_str_call() {
    use mova::embed::{Engine, Profile};

    let mut engine = Engine::builder().profile(Profile::Pure).build();

    // Buffer A: a fn that bails to the tree-walker (`reify`, the same
    // type-system bail `explain_line_appears_with_env_var` above uses),
    // padded so its `(defn ...)` call -- which is where macroexpansion
    // stamps every synthesized sub-form's span, same reasoning as
    // `compile::mod::tests::DEF_PAD` -- sits at a DISTINCTIVE, non-1:1
    // line:col instead of the trivial line 1 column 1 a bare one-liner
    // would always report regardless of this fix.
    let buffer_a_src = "\n\n   (defn cliffy [x] (reify Object (toString [_] x)))";
    engine
        .eval_named("buffer-a.mova", buffer_a_src)
        .unwrap_or_else(|e| panic!("buffer A: {e}"));

    // Buffer B: a DIFFERENT name and DIFFERENT (much shorter) content --
    // becomes `Interp::source`/`source_name` next. Pre-fix, THIS is what
    // `at`/`at_string` would have rendered buffer A's fn's span against.
    engine
        .eval_named("buffer-b.mova", "(+ 1 2)")
        .unwrap_or_else(|e| panic!("buffer B: {e}"));

    // A THIRD eval_str call: querying compile-explain is itself a fresh
    // buffer too, so a fix that only carried the id through ONE hop
    // wouldn't be enough -- the persisted record has to carry its own
    // identity all the way through to this call.
    let result = engine
        .eval_named("compile-explain-query.mova", "(compile-explain cliffy)")
        .unwrap_or_else(|e| panic!("compile-explain query: {e}"));

    let tier = result
        .get_kw("tier")
        .expect("expected compile-explain to return a :tier key");
    assert_eq!(
        tier.as_keyword().expect(":tier should be a keyword"),
        "tree-walk",
        "expected cliffy to have bailed to the tree-walker"
    );

    let at = result
        .get_kw("at")
        .expect("expected compile-explain to return an :at key");
    assert_eq!(
        at.as_str().expect(":at should be a string"),
        "buffer-a.mova:3:4",
        "expected buffer A's own name:line:col, not buffer B's or the compile-explain query buffer's"
    );
}
