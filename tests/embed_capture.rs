//! W-EMBED: `Engine::eval_capture` -- `*out*`/`*err*` capture surface.
//! Like `embed_api.rs`, everything here uses ONLY `mova::embed::*`.

use mova::embed::{Engine, Profile};

#[test]
fn pure_code_captures_nothing() {
    let mut engine = Engine::builder().profile(Profile::Pure).build();
    let (result, stdout, stderr) = engine.eval_capture("(+ 1 2 3)");
    assert_eq!(result.unwrap().as_i64(), Some(6));
    assert_eq!(stdout, "");
    assert_eq!(stderr, "");
}

#[test]
fn captures_println_output() {
    let mut engine = Engine::builder().profile(Profile::Pure).build();
    let (result, stdout, stderr) =
        engine.eval_capture(r#"(println "hello") (println "world") :done"#);
    assert_eq!(result.unwrap().as_keyword(), Some("done"));
    assert_eq!(stdout, "hello\nworld\n");
    assert_eq!(stderr, "");
}

#[test]
fn captures_reflection_warning_into_stderr() {
    let mut engine = Engine::builder().profile(Profile::Pure).build();
    let (result, _stdout, stderr) = engine.eval_capture(
        "(set! *warn-on-reflection* true) (defn foo [x] (.blah x))",
    );
    assert!(result.is_ok(), "eval should succeed even though it warns");
    assert!(
        stderr.contains("Reflection warning"),
        "expected a reflection warning in stderr, got: {stderr:?}"
    );
}

#[test]
fn errors_still_propagate_with_captures_returned() {
    let mut engine = Engine::builder().profile(Profile::Pure).build();
    let (result, stdout, _stderr) = engine.eval_capture(r#"(println "before") (throw "boom")"#);
    assert!(result.is_err());
    // The partial capture up to the throw is still returned, exactly like
    // `with-out-str` around a throwing body keeps its partial capture.
    assert_eq!(stdout, "before\n");
}

#[test]
fn capture_bindings_do_not_leak_into_a_later_eval() {
    let mut engine = Engine::builder().profile(Profile::Pure).build();
    let _ = engine.eval_capture(r#"(println "captured")"#);
    // A later plain `eval` must not still be bound to the (now-dropped)
    // capture atom from the previous call -- *out* must be back to
    // whatever it was before (nil, by default), so this eval doesn't
    // silently redirect into a stale buffer.
    let v = engine.eval("*out*").unwrap();
    assert_eq!(v.kind(), mova::embed::ValueKind::Nil);
}
