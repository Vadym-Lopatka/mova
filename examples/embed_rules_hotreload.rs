//! Hot-reloading a rules file while the host keeps running: a background
//! poll loop watches a `.mova` rules file's mtime (plain `fs::metadata`
//! polling -- no `notify` dependency), and on change re-evaluates it
//! against a FRESH [`Engine::snapshot`] taken from a template engine, not
//! the currently-live one. A broken edit (bad syntax, or a form that just
//! errors) never corrupts the rules already serving traffic -- the old
//! engine keeps answering `classify` calls exactly as before, and the host
//! is told the reload failed instead of silently running on half-loaded
//! state.
//!
//! Run: `cargo run --release --example embed_rules_hotreload -- --demo`
//! (interactive mode, the default with no `--demo`, watches a real file
//! path given as the first argument and polls until Ctrl-C).

use std::path::PathBuf;
use std::time::{Duration, Instant};

use mova::embed::{Engine, Profile};

/// Loads `src` into a fresh snapshot of `template` and confirms `classify`
/// is defined and callable before handing the snapshot back -- a rules
/// file that reads fine but forgot to define the entry point is just as
/// much a "broken reload" as a syntax error.
fn try_load(template: &Engine, src: &str) -> Result<Engine, String> {
    let mut candidate = template.snapshot();
    candidate
        .eval_named("rules", src)
        .map_err(|e| e.render_plain())?;
    candidate
        .call_by_name("classify", &[mova::embed::Value::from(0i64)])
        .map_err(|e| format!("rules loaded but `classify` is broken: {}", e.render_plain()))?;
    Ok(candidate)
}

/// One poll tick: if `path`'s mtime advanced past `last_seen`, try to
/// reload. On success, returns the new engine + mtime and prints what
/// changed; on failure, prints the error and returns `current` UNTOUCHED
/// -- the whole point of snapshotting into a candidate first.
fn poll_once(
    path: &PathBuf,
    template: &Engine,
    current: Engine,
    last_seen: std::time::SystemTime,
) -> (Engine, std::time::SystemTime) {
    let mtime = match std::fs::metadata(path).and_then(|m| m.modified()) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("  [poll] couldn't stat rules file: {e} (keeping current rules)");
            return (current, last_seen);
        }
    };
    if mtime <= last_seen {
        return (current, last_seen);
    }
    let src = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("  [poll] couldn't read rules file: {e} (keeping current rules)");
            return (current, last_seen);
        }
    };
    match try_load(template, &src) {
        Ok(reloaded) => {
            println!("  [poll] rules file changed -- reload OK, new rules are live");
            (reloaded, mtime)
        }
        Err(msg) => {
            println!("  [poll] rules file changed -- reload FAILED, keeping old rules live:");
            for line in msg.lines() {
                println!("    | {line}");
            }
            (current, mtime)
        }
    }
}

const V1: &str = "(defn classify [n] (if (< n 10) :small :big))";
const BROKEN: &str = "(defn classify [n] (if (< n 10) :small"; // unclosed
const V2: &str = "(defn classify [n] (cond (< n 10) :small (< n 100) :medium :else :big))";

fn run_demo() {
    let template = Engine::builder().profile(Profile::Pure).build();
    let dir = std::env::temp_dir().join("mova-embed-hotreload-example");
    std::fs::create_dir_all(&dir).expect("create example scratch dir");
    let path = dir.join("rules.mova");

    std::fs::write(&path, V1).unwrap();
    let mut engine = try_load(&template, V1).expect("v1 rules should load");
    let mut last_seen = std::fs::metadata(&path).unwrap().modified().unwrap();
    println!("-- v1 rules live: classify(5) = {}", call_classify(&mut engine, 5));

    for (label, src, probe) in [("v2 (valid upgrade)", V2, 50), ("broken edit", BROKEN, 50), ("v2 again (recovery)", V2, 500)] {
        std::thread::sleep(Duration::from_millis(20)); // ensure mtime advances
        std::fs::write(&path, src).unwrap();
        let (next, seen) = poll_once(&path, &template, engine, last_seen);
        engine = next;
        last_seen = seen;
        println!("-- after \"{label}\": classify({probe}) = {}", call_classify(&mut engine, probe));
    }
}

fn call_classify(engine: &mut Engine, n: i64) -> String {
    engine
        .call_by_name("classify", &[mova::embed::Value::from(n)])
        .map(|v| v.to_string())
        .unwrap_or_else(|e| format!("<error: {e}>"))
}

/// Interactive mode: `embed_rules_hotreload <path-to-rules.mova>` polls the
/// given file every 500ms until Ctrl-C. For humans, not CI.
fn run_interactive(path: PathBuf) {
    let template = Engine::builder().profile(Profile::Pure).build();
    let src = std::fs::read_to_string(&path).expect("read initial rules file");
    let mut engine = try_load(&template, &src).expect("initial rules file should load");
    let mut last_seen = std::fs::metadata(&path).and_then(|m| m.modified()).unwrap_or_else(|_| std::time::SystemTime::now());
    println!("watching {path:?} -- edit it and save to see reloads (Ctrl-C to quit)");
    let start = Instant::now();
    loop {
        std::thread::sleep(Duration::from_millis(500));
        let (next, seen) = poll_once(&path, &template, engine, last_seen);
        engine = next;
        last_seen = seen;
        let _ = start.elapsed();
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--demo") || std::env::var("MOVA_DEMO").is_ok() {
        run_demo();
        return;
    }
    match args.first() {
        Some(path) => run_interactive(PathBuf::from(path)),
        None => {
            eprintln!("usage: embed_rules_hotreload <path-to-rules.mova>   (or --demo for a scripted run)");
            std::process::exit(2);
        }
    }
}
