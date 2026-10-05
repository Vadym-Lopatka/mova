//! flow-gold: replays every scenario in
//! `tests/conformance/flow-gold/scenarios/NN-name.mova` through the mova
//! BINARY and diffs its stdout against the committed
//! `tests/conformance/flow-gold/goldens/NN-name.golden` (real JVM
//! `clojure.core.async.flow` output, generated ahead of time by
//! `tools/gen-flow-gold.bb` -- see that script's module doc and
//! `tests/conformance/flow-gold/SPEC.md`'s "Comparison semantics").
//!
//! ## Binary resolution
//! This crate declares no explicit `[[bin]]` in Cargo.toml, but Cargo
//! infers one named after the package (`mova`) from the presence of
//! `src/main.rs` -- confirmed by `cargo build --release --bin mova`
//! succeeding in this worktree. Because that inferred target exists,
//! `cargo test` builds it automatically and exposes its path to every
//! integration test via the `CARGO_BIN_EXE_mova` env var (Cargo sets one
//! `CARGO_BIN_EXE_<name>` per binary target the package produces). We use
//! that -- no manual `target/release/mova` path-guessing needed, and it
//! stays correct across debug/release test profiles.
//!
//! ## Why serial, not parallel
//! This file is a single `#[test]` function that loops over every scenario
//! serially (mirroring `tests/conformance_test.rs`'s one-fn-loops-every-file
//! shape), rather than one `#[test]` fn per scenario left to the default
//! parallel test harness. Two reasons, per SPEC.md's own steer:
//!   1. Flow scenarios spin real OS threads inside the mova process
//!      (one per proc, plus `flow/inject`'s own per-call thread -- see
//!      `src/builtins/flow.rs`). Several such processes launched
//!      concurrently on a shared machine oversubscribe CPU, which perturbs
//!      the wall-clock TIMEOUT-MS budgets scenarios rely on to bound
//!      otherwise-hangable reads -- a scenario that's reliably fast in
//!      isolation can spuriously time out under parallel load, which is
//!      exactly the kind of flakiness this corpus exists to eliminate.
//!   2. Every scenario run is already an OS-process boundary (`Command`
//!      spawns a fresh `mova` process per run), so there's no shared
//!      in-process state to race on the Rust side either way -- the only
//!      thing parallelism would buy is wall-clock speed, which isn't worth
//!      the timing flakiness above for a corpus whose whole point is
//!      semantic-equality confidence, not throughput.
//!
//! ## Scenario body == scenario file, verbatim
//! Per SPEC.md's "Body rules", a scenario file contains ONLY the body (no
//! require/ns forms) plus optional `;;`-prefixed header directives at the
//! top. Those directives are themselves ordinary Clojure line comments, so
//! they need no stripping -- the mova-side source is the scenario file's
//! raw text, copied verbatim into a fresh temp file per scenario (a temp
//! copy, rather than pointing the binary at `scenarios/NN-name.mova`
//! directly, both matches SPEC.md's stated construction and insulates a
//! run from a sibling agent concurrently editing scenario files outside
//! this test's own 00-* namespace).
//!
//! ## DIVERGENT scenarios
//! A scenario marked `;;DIVERGENT` is compared against the expected mova
//! output recorded in `DIVERGENCES.md` instead of the golden (whose
//! `;;DIVERGENT` directive makes it a JVM-only artifact for that scenario).
//! See `parse_divergences` below for the exact whitelist format, and its
//! doc comment for the same honesty rule `tests/conformance_test.rs` uses
//! for `DEVIATIONS.md`: an entry whose scenario's mova output now equals
//! the golden anyway (no longer actually divergent) FAILS as stale, and an
//! entry with no corresponding `;;DIVERGENT` scenario also fails as stale.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn flow_gold_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/conformance/flow-gold")
}

struct Scenario {
    name: String,
    path: PathBuf,
    runs: u32,
    timeout_ms: u64,
    divergent: bool,
}

/// Parses `;;RUNS n` (default 5), `;;TIMEOUT-MS ms` (default 30000), and
/// `;;DIVERGENT` (a flag) out of a scenario's raw text. Must track
/// `tools/gen-flow-gold.bb`'s `parse-directives` exactly, or a scenario
/// could be generated with one RUNS/TIMEOUT-MS budget and tested with
/// another.
fn parse_directives(text: &str) -> (u32, u64, bool) {
    let mut runs = 5u32;
    let mut timeout_ms = 30_000u64;
    let mut divergent = false;
    for line in text.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix(";;RUNS ") {
            if let Ok(n) = rest.trim().parse() {
                runs = n;
            }
        } else if let Some(rest) = t.strip_prefix(";;TIMEOUT-MS ") {
            if let Ok(n) = rest.trim().parse() {
                timeout_ms = n;
            }
        } else if t == ";;DIVERGENT" {
            divergent = true;
        }
    }
    (runs, timeout_ms, divergent)
}

fn scenarios() -> Vec<Scenario> {
    let dir = flow_gold_dir().join("scenarios");
    let mut files: Vec<PathBuf> = fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("reading {dir:?}: {e}"))
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("mova"))
        .collect();
    files.sort();
    files
        .into_iter()
        .map(|path| {
            let text = fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("reading {path:?}: {e}"));
            let (runs, timeout_ms, divergent) = parse_directives(&text);
            let name = path.file_stem().unwrap().to_string_lossy().into_owned();
            Scenario { name, path, runs, timeout_ms, divergent }
        })
        .collect()
}

/// One `DIVERGENCES.md` entry: the exact bytes mova's stdout must produce
/// on EVERY run of a `;;DIVERGENT` scenario.
struct DivergenceEntry {
    expected_stdout: Vec<u8>,
}

/// Parses `DIVERGENCES.md`'s whitelist. Format (see that file's own header
/// for the authoritative doc authors write against):
///
/// ```text
/// <!-- DIVERGENCE: scenario-name -->
/// free-form prose, ignored by this parser ...
///
/// ```text
/// RESULT some-verdict true
/// ```
/// <!-- END DIVERGENCE -->
/// ```
///
/// Between the two HTML-comment markers, the FIRST fenced block opened by a
/// line that is exactly "```text" and closed by a line that is exactly
/// "```" holds the expected stdout, one real output line per fence line
/// (each reconstructed with a trailing `\n`, matching how `println` and
/// this corpus's own goldens are always newline-terminated).
fn parse_divergences(path: &Path) -> HashMap<String, DivergenceEntry> {
    let text = fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("reading {path:?}: {e}"));
    let mut out = HashMap::new();
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0usize;
    while i < lines.len() {
        let line = lines[i].trim();
        if let Some(rest) = line
            .strip_prefix("<!-- DIVERGENCE: ")
            .and_then(|s| s.strip_suffix(" -->"))
        {
            let name = rest.trim().to_string();
            let mut j = i + 1;
            let mut fence: Option<Vec<String>> = None;
            let mut closed = false;
            while j < lines.len() {
                let t = lines[j].trim();
                if t == "<!-- END DIVERGENCE -->" {
                    closed = true;
                    break;
                }
                if fence.is_none() && t == "```text" {
                    let mut body = Vec::new();
                    let mut k = j + 1;
                    while k < lines.len() && lines[k].trim() != "```" {
                        body.push(lines[k].to_string());
                        k += 1;
                    }
                    fence = Some(body);
                    j = k;
                }
                j += 1;
            }
            if !closed {
                panic!(
                    "{path:?}: DIVERGENCE entry {name:?} is missing its <!-- END DIVERGENCE --> marker"
                );
            }
            let body = fence.unwrap_or_else(|| {
                panic!("{path:?}: DIVERGENCE entry {name:?} has no ```text fenced expected-output block")
            });
            let mut expected_stdout = Vec::new();
            for l in &body {
                expected_stdout.extend_from_slice(l.as_bytes());
                expected_stdout.push(b'\n');
            }
            if out.insert(name.clone(), DivergenceEntry { expected_stdout }).is_some() {
                panic!("{path:?}: duplicate DIVERGENCE entry for {name:?}");
            }
            i = j + 1;
            continue;
        }
        i += 1;
    }
    out
}

struct RunResult {
    run: u32,
    timed_out: bool,
    exit_code: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Runs the mova binary on `scenario_file` once, with a wall-clock
/// `timeout` budget. stdout/stderr are redirected to their own temp files
/// (never piped in-process) so a chatty/blocked child can't deadlock this
/// harness on a full pipe buffer -- same technique
/// `tools/gen-flow-gold.bb`'s `run-once` uses on the JVM side, kept
/// symmetric on purpose. On timeout, the child is killed and whatever
/// partial output made it to the file is still captured for diagnostics.
fn run_once(mova_bin: &Path, scenario_file: &Path, timeout: Duration, run: u32) -> RunResult {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let uniq = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let stamp = format!(
        "flow-gold-{}-{}-{}",
        std::process::id(),
        scenario_file.file_stem().unwrap().to_string_lossy(),
        uniq
    );
    let out_path = std::env::temp_dir().join(format!("{stamp}.out"));
    let err_path = std::env::temp_dir().join(format!("{stamp}.err"));
    let out_file = fs::File::create(&out_path)
        .unwrap_or_else(|e| panic!("creating stdout capture file {out_path:?}: {e}"));
    let err_file = fs::File::create(&err_path)
        .unwrap_or_else(|e| panic!("creating stderr capture file {err_path:?}: {e}"));

    let mut child = Command::new(mova_bin)
        .arg(scenario_file)
        .stdout(Stdio::from(out_file))
        .stderr(Stdio::from(err_file))
        .spawn()
        .unwrap_or_else(|e| panic!("failed to spawn mova binary at {mova_bin:?}: {e}"));

    let start = Instant::now();
    let (timed_out, exit_code) = loop {
        match child.try_wait().expect("try_wait on mova child") {
            Some(status) => break (false, status.code()),
            None => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    break (true, None);
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    };

    let stdout = fs::read(&out_path).unwrap_or_default();
    let stderr = fs::read(&err_path).unwrap_or_default();
    let _ = fs::remove_file(&out_path);
    let _ = fs::remove_file(&err_path);
    RunResult { run, timed_out, exit_code, stdout, stderr }
}

fn preview(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    let lines: Vec<&str> = s.lines().collect();
    if lines.len() > 20 {
        format!("{}\n  ... ({} more line(s))", lines[..20].join("\n"), lines.len() - 20)
    } else {
        s.into_owned()
    }
}

/// Rich side-by-side line diff for a mismatch report: shows the first
/// differing line (1-indexed) plus a few lines of context either side.
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
            out.push_str(&format!("    line {}: expected: {e}\n", i + 1));
            out.push_str(&format!("    line {}: actual:   {a}\n", i + 1));
            shown += 1;
            if shown >= 5 {
                out.push_str("    ... (further differences omitted)\n");
                break;
            }
        }
    }
    if exp_lines.len() != act_lines.len() {
        out.push_str(&format!(
            "    line counts differ: expected {} line(s), actual {} line(s)\n",
            exp_lines.len(),
            act_lines.len()
        ));
    }
    out
}

#[test]
fn flow_gold_scenarios_match_golden() {
    let dir = flow_gold_dir();
    let goldens_dir = dir.join("goldens");
    let divergences_path = dir.join("DIVERGENCES.md");
    let divergences = parse_divergences(&divergences_path);
    let mut used_divergences: HashSet<String> = HashSet::new();

    let mova_bin = PathBuf::from(env!("CARGO_BIN_EXE_mova"));
    let scenario_list = scenarios();
    assert!(
        !scenario_list.is_empty(),
        "no scenario .mova files found under {:?}",
        dir.join("scenarios")
    );
    let all_names: HashSet<String> = scenario_list.iter().map(|s| s.name.clone()).collect();

    let mut failures: Vec<String> = Vec::new();
    let mut ok_count = 0usize;

    for scenario in &scenario_list {
        if scenario.divergent && !divergences.contains_key(&scenario.name) {
            failures.push(format!(
                "{}: marked ;;DIVERGENT but DIVERGENCES.md has no entry for it -- add one \
                 (see DIVERGENCES.md's header for the format)",
                scenario.name
            ));
        } else if !scenario.divergent && divergences.contains_key(&scenario.name) {
            failures.push(format!(
                "{}: has a DIVERGENCES.md entry but its scenario file has no ;;DIVERGENT \
                 directive -- either the divergence was fixed (remove the DIVERGENCES.md entry) \
                 or the directive was dropped by mistake (restore it)",
                scenario.name
            ));
        }

        let golden_path = goldens_dir.join(format!("{}.golden", scenario.name));
        if !golden_path.exists() {
            failures.push(format!(
                "{}: no golden at {golden_path:?} -- generate it with:\n    \
                 bb tools/gen-flow-gold.bb {}",
                scenario.name, scenario.name
            ));
            continue;
        }
        let golden_bytes = fs::read(&golden_path)
            .unwrap_or_else(|e| panic!("reading {golden_path:?}: {e}"));

        let temp_copy = std::env::temp_dir().join(format!(
            "flow-gold-mova-{}-{}.mova",
            scenario.name,
            std::process::id()
        ));
        let scenario_text = fs::read_to_string(&scenario.path)
            .unwrap_or_else(|e| panic!("reading {:?}: {e}", scenario.path));
        fs::write(&temp_copy, &scenario_text)
            .unwrap_or_else(|e| panic!("writing {temp_copy:?}: {e}"));

        let timeout = Duration::from_millis(scenario.timeout_ms);
        let results: Vec<RunResult> = (1..=scenario.runs)
            .map(|run| run_once(&mova_bin, &temp_copy, timeout, run))
            .collect();
        let _ = fs::remove_file(&temp_copy);

        let timeouts: Vec<&RunResult> = results.iter().filter(|r| r.timed_out).collect();
        if !timeouts.is_empty() {
            let mut msg = format!(
                "{}: {}/{} run(s) exceeded TIMEOUT-MS={}\n",
                scenario.name,
                timeouts.len(),
                scenario.runs,
                scenario.timeout_ms
            );
            for t in &timeouts {
                msg.push_str(&format!(
                    "  run {}: partial stdout ({}B):\n{}\n",
                    t.run,
                    t.stdout.len(),
                    preview(&t.stdout)
                ));
            }
            failures.push(msg);
            continue;
        }

        let nonzero: Vec<&RunResult> =
            results.iter().filter(|r| r.exit_code != Some(0)).collect();
        if !nonzero.is_empty() {
            let mut msg = format!(
                "{}: {}/{} run(s) exited nonzero\n",
                scenario.name,
                nonzero.len(),
                scenario.runs
            );
            for r in &nonzero {
                msg.push_str(&format!(
                    "  run {}: exit={:?}\n  stdout:\n{}\n  stderr:\n{}\n",
                    r.run,
                    r.exit_code,
                    preview(&r.stdout),
                    preview(&r.stderr)
                ));
            }
            failures.push(msg);
            continue;
        }

        if scenario.divergent {
            let Some(entry) = divergences.get(&scenario.name) else {
                // already reported above (missing entry); nothing more to check.
                continue;
            };
            used_divergences.insert(scenario.name.clone());
            let mismatched: Vec<&RunResult> = results
                .iter()
                .filter(|r| r.stdout != entry.expected_stdout)
                .collect();
            if !mismatched.is_empty() {
                let mut msg = format!(
                    "{}: {}/{} run(s) did not match DIVERGENCES.md's recorded mova output\n",
                    scenario.name,
                    mismatched.len(),
                    scenario.runs
                );
                for r in &mismatched {
                    msg.push_str(&format!("  run {}:\n{}\n", r.run, diff_lines(&entry.expected_stdout, &r.stdout)));
                }
                failures.push(msg);
                continue;
            }
            // Honesty check: if EVERY run's actual output now also equals
            // the JVM golden, the scenario no longer actually diverges --
            // the DIVERGENCES.md entry (and ;;DIVERGENT directive) are
            // stale, mirroring DEVIATIONS.md's staleness rule exactly.
            if results.iter().all(|r| r.stdout == golden_bytes) {
                failures.push(format!(
                    "{}: marked ;;DIVERGENT and matches its DIVERGENCES.md entry, but mova's \
                     actual output now ALSO equals the JVM golden -- this scenario no longer \
                     diverges; remove ;;DIVERGENT from the scenario and delete its \
                     DIVERGENCES.md entry",
                    scenario.name
                ));
                continue;
            }
            ok_count += 1;
        } else {
            let mismatched: Vec<&RunResult> =
                results.iter().filter(|r| r.stdout != golden_bytes).collect();
            if !mismatched.is_empty() {
                let mut msg = format!(
                    "{}: {}/{} run(s) did not match the golden\n",
                    scenario.name,
                    mismatched.len(),
                    scenario.runs
                );
                for r in &mismatched {
                    msg.push_str(&format!("  run {}:\n{}\n", r.run, diff_lines(&golden_bytes, &r.stdout)));
                }
                failures.push(msg);
                continue;
            }
            ok_count += 1;
        }
    }

    // Orphaned DIVERGENCES.md entries: a name that doesn't correspond to
    // any scenario file at all (the "not marked ;;DIVERGENT" case is
    // already caught per-scenario above).
    for name in divergences.keys() {
        if !all_names.contains(name) {
            failures.push(format!(
                "DIVERGENCES.md has an entry for {name:?} but no such scenario file exists -- \
                 remove the stale entry"
            ));
        }
    }

    if !failures.is_empty() {
        panic!(
            "{} flow-gold failure(s):\n\n{}",
            failures.len(),
            failures.join("\n")
        );
    }

    println!(
        "flow-gold: {ok_count}/{} scenario(s) green ({} documented divergence(s))",
        scenario_list.len(),
        used_divergences.len()
    );
}
