//! SPEC-W6b: the three spec smoke corpora, as a `cargo test` gate.
//!
//! `tests/spec-smoke/*.mova` were oracle-diffed BY HAND until now (the
//! recipe lives in `tests/spec-smoke/RUNNING.md`), which meant a
//! regression in `clojure.spec.alpha`, `clojure.spec.gen.alpha` or
//! `clojure.spec.test.alpha` only surfaced when somebody remembered to run
//! two shell commands. This file makes them a build gate instead: each
//! corpus is executed by the mova binary and its **stdout** is compared
//! BYTE-FOR-BYTE against the adjacent `*.golden`, which is the real
//! Clojure capture.
//!
//! | corpus | pins |
//! |---|---|
//! | `smoke.mova` (180 lines) | the `clojure.spec.alpha` + `clojure.spec.gen.alpha` API surface |
//! | `stest-smoke.mova` (125 lines) | `clojure.spec.test.alpha` -- instrument / unstrument / check |
//! | `seeded-gen.mova` (33 lines) | SEEDED generation as raw VALUES, on the bit-exact native `clojure.test.check.random` |
//!
//! ## Golden provenance
//! Every `.golden` is the byte-exact stdout of the SAME `.mova` file run
//! through real Clojure, captured by `tools/gen-spec-smoke-goldens.sh`:
//!
//! ```text
//! clojure -Sdeps '{:deps {org.clojure/clojure    {:mvn/version "1.13.0-alpha6"}
//!                         org.clojure/test.check {:mvn/version "1.1.1"}}}' \
//!         -M -i tests/spec-smoke/<name>.mova -e ':end' 2>/dev/null | sed '$d'
//! ```
//!
//! `org.clojure/clojure 1.13.0-alpha6` (which SHIPS `clojure.spec.alpha`)
//! and `org.clojure/test.check 1.1.1` -- the same pin as
//! `tests/conformance/CLOJURE_VERSION` and `RUNNING.md`. A golden must
//! NEVER be regenerated from mova's own output; that would turn this gate
//! into a tautology.
//!
//! ## Why a subprocess and not an in-process `Engine`
//! Same precedent as `tests/flow_gold_test.rs`: these corpora are FILES,
//! and what is being compared is a file execution's stdout. Running the
//! real binary keeps three things honest that an in-process
//! `Engine::eval_capture` over the whole source would blur -- per-form
//! read-then-eval (`stest-smoke.mova` uses `::alias/kw` keywords whose
//! alias is established by an earlier form in the same file), the
//! `(set! *print-namespace-maps* false)` a corpus does at file scope, and
//! the stdout/stderr split (mova prints `WARNING:` / `Reflection warning,`
//! to stderr exactly as Clojure does, and neither side's stderr is part of
//! the comparison). Cargo hands us the binary's path in
//! `CARGO_BIN_EXE_mova`, so this is correct in debug and release alike.
//!
//! ## No `--module-path`
//! The corpora are invoked exactly as `RUNNING.md` documents them: the
//! file path and nothing else. `main.rs`'s `run_file` then defaults the
//! module path to the corpus's OWN directory, `tests/spec-smoke/`, which
//! contains no library namespaces at all -- so every `require` in these
//! files is served by the embedded table in `src/stdlib.rs` (or, for
//! `clojure.test.check.random`, by the native veneer). That is the whole
//! point of the W3/W6a work and `assert_no_module_libraries_in_corpus_dir`
//! below keeps it true.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The three corpora, in the order `RUNNING.md` lists them, with the line
/// count each is documented to produce (asserted, so a corpus that
/// silently shrinks fails loudly rather than passing a shorter diff).
const CORPORA: &[(&str, usize)] = &[("smoke", 180), ("stest-smoke", 125), ("seeded-gen", 33)];

fn smoke_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/spec-smoke")
}

/// Runs the mova binary on `corpus` and returns its stdout bytes.
/// stdout/stderr go to their own temp files rather than pipes, like
/// `flow_gold_test.rs`'s `run_once` -- a chatty child cannot deadlock the
/// harness on a full pipe buffer that way. stderr is captured only so it
/// can be shown in a failure message; it is never compared.
fn run_corpus(mova_bin: &Path, corpus: &Path) -> (Vec<u8>, Vec<u8>, Option<i32>) {
    let stamp = format!(
        "spec-smoke-{}-{}",
        std::process::id(),
        corpus.file_stem().unwrap().to_string_lossy()
    );
    let out_path = std::env::temp_dir().join(format!("{stamp}.out"));
    let err_path = std::env::temp_dir().join(format!("{stamp}.err"));
    let out_file = fs::File::create(&out_path)
        .unwrap_or_else(|e| panic!("creating stdout capture file {out_path:?}: {e}"));
    let err_file = fs::File::create(&err_path)
        .unwrap_or_else(|e| panic!("creating stderr capture file {err_path:?}: {e}"));

    let status = Command::new(mova_bin)
        .arg(corpus)
        .stdout(Stdio::from(out_file))
        .stderr(Stdio::from(err_file))
        .status()
        .unwrap_or_else(|e| panic!("failed to run mova binary at {mova_bin:?}: {e}"));

    let stdout = fs::read(&out_path).unwrap_or_default();
    let stderr = fs::read(&err_path).unwrap_or_default();
    let _ = fs::remove_file(&out_path);
    let _ = fs::remove_file(&err_path);
    (stdout, stderr, status.code())
}

/// First few differing lines, 1-indexed, plus a line-count note -- the
/// same shape `flow_gold_test.rs::diff_lines` reports.
fn diff_lines(expected: &[u8], actual: &[u8]) -> String {
    let exp_s = String::from_utf8_lossy(expected);
    let act_s = String::from_utf8_lossy(actual);
    let exp_lines: Vec<&str> = exp_s.lines().collect();
    let act_lines: Vec<&str> = act_s.lines().collect();
    let max = exp_lines.len().max(act_lines.len());
    let mut out = String::new();
    let mut shown = 0;
    for i in 0..max {
        let e = exp_lines.get(i).copied().unwrap_or("<no line>");
        let a = act_lines.get(i).copied().unwrap_or("<no line>");
        if e != a {
            out.push_str(&format!("    line {}: clojure: {e}\n", i + 1));
            out.push_str(&format!("    line {}: mova:    {a}\n", i + 1));
            shown += 1;
            if shown >= 5 {
                out.push_str("    ... (further differences omitted)\n");
                break;
            }
        }
    }
    if exp_lines.len() != act_lines.len() {
        out.push_str(&format!(
            "    line counts differ: golden {} line(s), mova {} line(s)\n",
            exp_lines.len(),
            act_lines.len()
        ));
    }
    out
}

#[test]
fn spec_smoke_corpora_match_the_clojure_goldens() {
    let mova_bin = PathBuf::from(env!("CARGO_BIN_EXE_mova"));
    let dir = smoke_dir();
    let mut failures: Vec<String> = Vec::new();

    for (name, expected_lines) in CORPORA {
        let corpus = dir.join(format!("{name}.mova"));
        let golden_path = dir.join(format!("{name}.golden"));
        assert!(corpus.is_file(), "missing corpus {corpus:?}");
        let golden = fs::read(&golden_path)
            .unwrap_or_else(|e| panic!("reading golden {golden_path:?}: {e} -- regenerate with tools/gen-spec-smoke-goldens.sh"));

        assert_eq!(
            golden.iter().filter(|b| **b == b'\n').count(),
            *expected_lines,
            "{name}.golden is not the documented {expected_lines} lines -- \
             RUNNING.md and this test's CORPORA table both need updating if that is intentional"
        );

        let (stdout, stderr, code) = run_corpus(&mova_bin, &corpus);
        if code != Some(0) {
            failures.push(format!(
                "{name}.mova: mova exited with {code:?}\n  stderr:\n{}",
                String::from_utf8_lossy(&stderr)
            ));
            continue;
        }
        if stdout != golden {
            failures.push(format!(
                "{name}.mova: stdout differs from {name}.golden (real Clojure 1.13.0-alpha6):\n{}",
                diff_lines(&golden, &stdout)
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "{} spec smoke corpus mismatch(es):\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

/// The corpora are run with NO `--module-path`, so their requires fall
/// through to `src/stdlib.rs`'s embedded table (and the native
/// `clojure.test.check.random` veneer). `main.rs` defaults an
/// unqualified `mova <file>` to the file's own directory, so a stray
/// `.mova`/`.clj`/`.cljc` library dropped into `tests/spec-smoke/` beside
/// a corpus would quietly start serving those requires from DISK and the
/// gate would stop proving that spec ships in the binary.
#[test]
fn corpus_directory_holds_no_library_that_could_shadow_the_embedded_table() {
    let dir = smoke_dir();
    let allowed: Vec<String> = CORPORA.iter().map(|(n, _)| format!("{n}.mova")).collect();
    let mut strays: Vec<String> = Vec::new();
    for entry in fs::read_dir(&dir).unwrap_or_else(|e| panic!("reading {dir:?}: {e}")) {
        let path = entry.expect("dir entry").path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let is_code = matches!(
            path.extension().and_then(|s| s.to_str()),
            Some("mova") | Some("clj") | Some("cljc") | Some("cljs")
        );
        if is_code && !allowed.contains(&name) {
            strays.push(name);
        }
    }
    assert!(
        strays.is_empty(),
        "tests/spec-smoke/ holds code file(s) that are not corpora: {strays:?} -- \
         they would be on the default module path of every corpus run and could \
         shadow the embedded stdlib table this gate exists to prove"
    );
    // And every corpus that IS listed must exist, with a golden beside it.
    for (name, _) in CORPORA {
        assert!(dir.join(format!("{name}.mova")).is_file(), "missing corpus {name}.mova");
        assert!(dir.join(format!("{name}.golden")).is_file(), "missing golden {name}.golden");
    }
}
