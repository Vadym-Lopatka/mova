//! Differential conformance harness: replays every form in
//! `tests/conformance/corpus/*.corpus` through mova and diffs the result
//! against the adjacent `*.golden` file (real Clojure output -- generated
//! ahead of time by `tools/gen-golden.bb`, which shells out to a real JVM
//! `clojure` process pinned to `tests/conformance/CLOJURE_VERSION`
//! (1.13.0-alpha6), one `clojure` process per corpus *file* -- see
//! `tools/jvm-runner.clj`/`tools/jvm-flow-runner.clj`, and that latter
//! file's own module doc for why babashka/SCI is no longer the oracle).
//! Session model: one `Interp` (mova `Engine`) per corpus *file*, so
//! forms may build on `def`s made earlier in the same file, matching the
//! JVM runner's own one-`clojure`-process-per-file session model.
//!
//! Golden line format: `OK<TAB><pr-str of the result>`, or
//! `ERR<TAB><ExceptionSimpleName>` for a form that threw (a bare `ERR`
//! with no kind is also still accepted, see `parse_golden`, so a stale or
//! hand-written golden doesn't panic the parser). The recorded kind is
//! Clojure's own exception class simple name (e.g. `ArithmeticException`,
//! `ExceptionInfo`), RAW/outer and never the message text -- per
//! CONFORMANCE-GUARANTEE.md's canonicalization rule 6, exceptions compare
//! by occurrence AND coarse kind, never by message. Comparison here is
//! currently OCCURRENCE ONLY (mova threw <=> Clojure threw): mova has no
//! exception-class taxonomy yet (milestone M8), so there is no mova-side
//! kind to compare against Clojure's recorded one. The golden's kind is
//! still parsed and carried as data, and surfaced in mismatch messages, so
//! no goldens need regenerating when M8 lands -- see
//! `COMPARE_EXCEPTION_KIND` below for the flag that turns kind comparison
//! on.
//!
//! Mismatches are checked against `tests/conformance/DEVIATIONS.md`: a
//! whitelisted `file:line` passes (and is counted); anything else fails
//! with a rich message (form, expected, got). Whitelist entries that no
//! longer actually mismatch also fail, so the file can't go stale.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use mova::embed::Engine;

/// One line from a `.corpus` file that survived comment/blank filtering,
/// with its original 1-indexed line number in that file (used to key
/// DEVIATIONS.md entries and for readable failure messages).
struct CorpusForm {
    line: usize,
    src: String,
}

/// One line from a `.golden` file: `OK\t<pr-str>` or `ERR` / `ERR\t<kind>`.
/// `Err`'s payload is the Clojure-side exception's simple class name when
/// the golden recorded one, or `None` for a bare `ERR` line (either a
/// stale golden from before this format widened, or -- on the mova side,
/// see `eval_form` -- because mova has no exception-class taxonomy yet).
#[derive(Clone, PartialEq, Eq, Debug)]
enum Golden {
    Ok(String),
    Err(Option<String>),
}

impl std::fmt::Display for Golden {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Golden::Ok(s) => write!(f, "OK\t{s}"),
            Golden::Err(Some(kind)) => write!(f, "ERR ({kind})"),
            Golden::Err(None) => write!(f, "ERR"),
        }
    }
}

/// Whether a mismatch comparison also requires the exception *kind* to
/// match, not just that both sides threw. Currently `false`: mova has no
/// exception-class taxonomy yet (that's milestone M8's job, per
/// CLOJURE-COMPAT-PLAN.md), so `eval_form` below can only ever produce
/// `Golden::Err(None)` on the mova side -- there is no mova-side kind to
/// compare Clojure's recorded one against. Until then, `Golden::matches`
/// treats any two `Err` goldens as equal regardless of kind (occurrence
/// only: "mova threw <=> Clojure threw"), while still parsing and
/// carrying Clojure's kind as data (see `parse_golden`) and surfacing it
/// in mismatch messages. When M8 lands a typed-error taxonomy, `eval_form`
/// should start returning `Golden::Err(Some(kind))` for a caught mova
/// error, and this flag flips to `true` to make kind comparison live --
/// no golden regeneration or format change needed at that point, since
/// the data has been recorded since M0.
const COMPARE_EXCEPTION_KIND: bool = false;

impl Golden {
    /// Whether `self` (actual, from mova) matches `expected` (from the
    /// golden file) under the currently-staged comparison rule -- see
    /// `COMPARE_EXCEPTION_KIND`'s own doc comment.
    fn matches(&self, expected: &Golden) -> bool {
        match (self, expected) {
            (Golden::Ok(a), Golden::Ok(b)) => a == b,
            (Golden::Err(actual_kind), Golden::Err(expected_kind)) => {
                !COMPARE_EXCEPTION_KIND || actual_kind == expected_kind
            }
            _ => false,
        }
    }
}

struct Deviation {
    file: String,
    line: usize,
}

fn conformance_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/conformance")
}

/// A corpus line is a form unless it's blank or a `;;`-prefixed comment
/// (after leading whitespace) -- must match `tools/gen-golden.bb`'s
/// `skip-line?` exactly, or corpus/golden line counts drift apart.
fn is_skippable(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.is_empty() || trimmed.starts_with(";;")
}

fn parse_corpus(path: &Path) -> Vec<CorpusForm> {
    let text = fs::read_to_string(path).unwrap_or_else(|e| panic!("reading {path:?}: {e}"));
    text.lines()
        .enumerate()
        .filter(|(_, line)| !is_skippable(line))
        .map(|(i, line)| CorpusForm {
            line: i + 1,
            src: line.to_string(),
        })
        .collect()
}

fn parse_golden(path: &Path) -> Vec<Golden> {
    let text = fs::read_to_string(path).unwrap_or_else(|e| {
        panic!("reading {path:?}: {e} -- run `bb tools/gen-golden.bb` to (re)generate goldens")
    });
    text.lines()
        .map(|line| match line.strip_prefix("OK\t") {
            Some(rest) => Golden::Ok(rest.to_string()),
            None if line == "ERR" => Golden::Err(None),
            None => match line.strip_prefix("ERR\t") {
                Some(kind) => Golden::Err(Some(kind.to_string())),
                None => panic!("{path:?}: malformed golden line: {line:?}"),
            },
        })
        .collect()
}

/// Parses DEVIATIONS.md's whitelist table. Deliberately dumb per the
/// file's own doc: any line starting with `|` is split on `|`; a row
/// counts as a deviation entry only when its second column parses as a
/// plain `usize` (the header/separator rows don't, so they're skipped for
/// free without any markdown-table-structure awareness).
fn parse_deviations() -> Vec<Deviation> {
    let path = conformance_dir().join("DEVIATIONS.md");
    let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {path:?}: {e}"));
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if !line.starts_with('|') {
            continue;
        }
        let cols: Vec<&str> = line.trim_matches('|').split('|').map(str::trim).collect();
        if cols.len() < 2 {
            continue;
        }
        let Ok(line_no) = cols[1].parse::<usize>() else {
            continue;
        };
        out.push(Deviation {
            file: cols[0].to_string(),
            line: line_no,
        });
    }
    out
}

/// Evaluates one corpus form's source against `engine` (the file's shared
/// session) and renders it exactly like `tools/gen-golden.bb`'s
/// `eval-form` does: `Ok` deep-realizes the result (see
/// `Engine::realize`) before `pr_str`ing it, so a lazy-seq result
/// compares against Clojure's already-realized printed form; any error
/// (reader or runtime) collapses to `Golden::Err(None)` -- always `None`
/// on the mova side, since mova has no exception-class taxonomy yet (see
/// `COMPARE_EXCEPTION_KIND`'s doc comment) -- matching `eval-form`'s
/// blanket `catch Throwable`.
fn eval_form(engine: &mut Engine, src: &str) -> Golden {
    match engine.eval_named("conformance", src) {
        Ok(v) => match engine.realize(&v) {
            Ok(realized) => Golden::Ok(realized.to_string()),
            Err(_) => Golden::Err(None),
        },
        Err(_) => Golden::Err(None),
    }
}

fn corpus_files() -> Vec<PathBuf> {
    let dir = conformance_dir().join("corpus");
    let mut files: Vec<PathBuf> = fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("reading {dir:?}: {e}"))
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("corpus"))
        .collect();
    files.sort();
    files
}

/// `cargo test` runs every `#[test]` fn on a freshly spawned thread with a
/// far smaller default stack than the OS-default *main*-thread budget
/// `eval::DEFAULT_MAX_CALL_DEPTH`'s doc comment tunes against (see
/// `src/eval/mod.rs`) -- and, unlike that main thread, `src/main.rs`'s CLI
/// never runs actual mova code on it directly either (it dedicates its own
/// large-stack `mova-eval` worker thread for exactly this reason). A
/// corpus form doing real non-tail mova-level recursion (e.g.
/// `higher-order.corpus`'s `letfn`-defined self-recursive `sum-to`, depth
/// 100 -- safely under the 200 call-depth guard by that guard's own
/// documented budget) can still blow the SMALL test-thread stack and abort
/// the whole process with an uncatchable SIGSEGV/SIGABRT before the guard
/// ever trips. Running the corpus body on an explicitly large worker thread
/// here sidesteps the test-thread-specific stack budget entirely, matching
/// `src/main.rs`'s own `EVAL_STACK_SIZE` convention (scaled down: this
/// harness's forms are far shallower than an interactive session's worst
/// case, so 64 MiB -- `builtins::async`/`conc`'s per-thread convention --
/// is ample headroom).
const CONFORMANCE_STACK_SIZE: usize = 64 * 1024 * 1024;

#[test]
fn conformance_corpus_matches_real_clojure() {
    std::thread::Builder::new()
        .stack_size(CONFORMANCE_STACK_SIZE)
        .spawn(run_conformance_corpus)
        .expect("failed to spawn conformance test worker thread")
        .join()
        .unwrap_or_else(|payload| std::panic::resume_unwind(payload));
}

fn run_conformance_corpus() {
    let deviations = parse_deviations();
    let mut used_deviations: HashSet<(String, usize)> = HashSet::new();
    let mut unexpected: Vec<String> = Vec::new();
    let mut total_forms = 0usize;
    let mut whitelisted_count = 0usize;

    let files = corpus_files();
    assert!(!files.is_empty(), "no *.corpus files found under tests/conformance/corpus");

    for corpus_path in &files {
        let file_name = corpus_path.file_name().unwrap().to_string_lossy().into_owned();
        let golden_path = corpus_path.with_extension("golden");
        let forms = parse_corpus(corpus_path);
        let golden = parse_golden(&golden_path);
        assert_eq!(
            forms.len(),
            golden.len(),
            "{file_name}: corpus has {} form(s) but golden has {} line(s) -- \
             re-run `bb tools/gen-golden.bb` to regenerate goldens",
            forms.len(),
            golden.len()
        );

        // One Engine per corpus file: the file IS the session, so earlier
        // `def`s in this file stay visible to later forms in it, and never
        // leak into the next file.
        let mut engine = Engine::builder().build();

        for (form, expected) in forms.iter().zip(golden.iter()) {
            total_forms += 1;
            let actual = eval_form(&mut engine, &form.src);
            if actual.matches(expected) {
                continue;
            }
            if deviations.iter().any(|d| d.file == file_name && d.line == form.line) {
                used_deviations.insert((file_name.clone(), form.line));
                whitelisted_count += 1;
                continue;
            }
            unexpected.push(format!(
                "{file_name}:{}: {}\n    clojure: {expected}\n    mova:   {actual}",
                form.line, form.src
            ));
        }
    }

    // Keep DEVIATIONS.md honest: every whitelisted file:line must
    // correspond to an actual mismatch, or a fixed bug could silently
    // leave a stale (and now-misleading) entry behind forever.
    let stale: Vec<String> = deviations
        .iter()
        .filter(|d| !used_deviations.contains(&(d.file.clone(), d.line)))
        .map(|d| format!("{}:{} is whitelisted but no longer mismatches -- remove it from DEVIATIONS.md", d.file, d.line))
        .collect();

    if !unexpected.is_empty() || !stale.is_empty() {
        let mut msg = String::new();
        if !unexpected.is_empty() {
            msg.push_str(&format!(
                "{} UNWHITELISTED conformance mismatch(es) (fix mova, or add a DEVIATIONS.md entry):\n\n",
                unexpected.len()
            ));
            msg.push_str(&unexpected.join("\n\n"));
            msg.push('\n');
        }
        if !stale.is_empty() {
            if !msg.is_empty() {
                msg.push('\n');
            }
            msg.push_str(&format!("{} STALE DEVIATIONS.md entr(ies):\n", stale.len()));
            msg.push_str(&stale.join("\n"));
            msg.push('\n');
        }
        panic!("{msg}");
    }

    println!(
        "conformance: {total_forms} forms across {} corpus files, {whitelisted_count} documented deviations, all green",
        files.len()
    );
}
