//! Integration tests for `builtins::sys` (P3b): errno-aware system natives,
//! plus a CLI smoke test on the built `mova` binary.

use mova::embed::{Engine, Value, ValueKind};

fn engine() -> Engine {
    Engine::builder().build()
}

/// Evaluates `src` against a fresh interpreter, panicking (with the
/// rendered diagnostic) on error -- convenient for tests that only care
/// about the success path.
fn eval_ok(src: &str) -> Value {
    engine()
        .eval_named("test", src)
        .unwrap_or_else(|e| panic!("{}", e.render_plain()))
}

/// Evaluates `src`, returning the rendered diagnostic string of an expected
/// error (panics if evaluation unexpectedly succeeds).
fn eval_err(src: &str) -> String {
    match engine().eval_named("test", src) {
        Ok(v) => panic!("expected an error, got {v:?}"),
        Err(e) => e.render_plain(),
    }
}

fn unique_temp_path(tag: &str) -> std::path::PathBuf {
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("mova-sys-test-{tag}-{pid}-{nanos}"))
}

#[test]
fn slurp_spit_round_trip() {
    let path = unique_temp_path("roundtrip");
    let path_str = path.to_str().unwrap();
    let src = format!(
        r#"(spit "{p}" "hello from mova\n") (slurp "{p}")"#,
        p = path_str
    );
    let v = eval_ok(&src);
    assert_eq!(v, Value::from("hello from mova\n"));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn slurp_nonexistent_path_reports_errno_and_syscall() {
    let rendered = eval_err(r#"(slurp "/no/such/path/mova-does-not-exist")"#);
    assert!(
        rendered.contains("ENOENT") || rendered.contains("No such file"),
        "rendered error should mention ENOENT or its message, got: {rendered}"
    );
    assert!(
        rendered.contains("open"),
        "rendered error should name the failing syscall 'open', got: {rendered}"
    );
}

#[test]
fn spit_to_unwritable_directory_is_an_errno_error() {
    let rendered = eval_err(r#"(spit "/nonexistent-dir-xyz/f" "x")"#);
    assert!(
        rendered.contains("ENOENT") || rendered.contains("No such file"),
        "rendered error should mention ENOENT, got: {rendered}"
    );
}

#[test]
fn getenv_path_is_present() {
    let v = eval_ok(r#"(getenv "PATH")"#);
    match v.as_str() {
        Some(s) => assert!(!s.is_empty()),
        None => panic!("expected a string, got {v:?}"),
    }
}

#[test]
fn getenv_missing_var_is_nil() {
    let v = eval_ok(r#"(getenv "MOVA_DEFINITELY_UNSET_VAR_XYZ")"#);
    assert_eq!(v.kind(), ValueKind::Nil);
}

#[test]
fn sh_echo_round_trip() {
    let v = eval_ok(r#"(sh "echo" "hi-from-sh")"#);
    assert_eq!(v.kind(), ValueKind::Map, "expected a map, got {v:?}");
    let exit = v.entries().find(|(k, _)| *k == Value::keyword("exit")).map(|(_, val)| val);
    assert_eq!(exit, Some(Value::from(0i64)));
    let out = v.entries().find(|(k, _)| *k == Value::keyword("out")).map(|(_, val)| val);
    match out {
        Some(ref val) => match val.as_str() {
            Some(s) => assert!(s.contains("hi-from-sh")),
            None => panic!("expected :out to be a string, got {val:?}"),
        },
        None => panic!("expected :out to be present"),
    }
}

#[test]
fn file_exists_true_and_false() {
    let path = unique_temp_path("exists");
    let path_str = path.to_str().unwrap();
    let src_false = format!(r#"(file-exists? "{p}")"#, p = path_str);
    assert_eq!(eval_ok(&src_false), Value::from(false));

    std::fs::write(&path, b"x").unwrap();
    let src_true = format!(r#"(file-exists? "{p}")"#, p = path_str);
    assert_eq!(eval_ok(&src_true), Value::from(true));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn delete_file_lifecycle() {
    let path = unique_temp_path("delete");
    std::fs::write(&path, b"x").unwrap();
    let path_str = path.to_str().unwrap();
    let src = format!(r#"(delete-file "{p}")"#, p = path_str);
    assert_eq!(eval_ok(&src), Value::from(true));
    assert!(!path.exists());
}

#[test]
fn delete_file_missing_path_is_an_errno_error() {
    let rendered = eval_err(r#"(delete-file "/no/such/path/mova-does-not-exist")"#);
    assert!(
        rendered.contains("ENOENT") || rendered.contains("No such file"),
        "rendered error should mention ENOENT, got: {rendered}"
    );
    assert!(rendered.contains("unlink"));
}

#[test]
fn list_dir_returns_sorted_names() {
    let dir = unique_temp_path("listdir");
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join("b.mova"), b"").unwrap();
    std::fs::write(dir.join("a.mova"), b"").unwrap();
    let dir_str = dir.to_str().unwrap();
    let src = format!(r#"(list-dir "{p}")"#, p = dir_str);
    let v = eval_ok(&src);
    assert_eq!(v.kind(), ValueKind::Vector, "expected a vector, got {v:?}");
    let names: Vec<String> = v
        .iter()
        .map(|item| match item.as_str() {
            Some(s) => s.to_string(),
            None => panic!("expected string entries, got {item:?}"),
        })
        .collect();
    assert_eq!(names, vec!["a.mova".to_string(), "b.mova".to_string()]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn cwd_is_non_empty() {
    let v = eval_ok("(cwd)");
    match v.as_str() {
        Some(s) => assert!(!s.is_empty()),
        None => panic!("expected a string, got {v:?}"),
    }
}

#[test]
fn time_ms_is_a_recent_positive_int() {
    let v = eval_ok("(time-ms)");
    match v.as_i64() {
        Some(n) => assert!(n > 1_700_000_000_000, "expected a millisecond epoch timestamp, got {n}"),
        None => panic!("expected an int, got {v:?}"),
    }
}

// -----------------------------------------------------------------------
// CLI smoke tests (built binary via `env!("CARGO_BIN_EXE_mova")`)
// -----------------------------------------------------------------------

#[test]
fn cli_eval_flag_prints_result() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_mova"))
        .args(["-e", "(+ 1 2)"])
        .output()
        .expect("failed to run mova binary");
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout), "3\n");
}

#[test]
fn cli_eval_flag_type_error_exits_nonzero_with_labeled_diagnostic() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_mova"))
        .args(["-e", "(+ 1 :not-a-number)"])
        .output()
        .expect("failed to run mova binary");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains('^') || stderr.contains('|'),
        "expected a miette caret/label in stderr, got: {stderr}"
    );
}

#[test]
fn cli_version_flag() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_mova"))
        .arg("--version")
        .output()
        .expect("failed to run mova binary");
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("mova"));
}
