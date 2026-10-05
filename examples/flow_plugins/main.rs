//! `flow_plugins` -- separation of concerns on top of `core.async.flow`,
//! with a live-CRUD plugin registry wired through the flow itself.
//!
//! Four `.mova` files make up the running program (see this folder):
//!
//!   procs/ingest.mova        logic: stamp {:seq n :value v}
//!   procs/plugin_stage.mova  logic: plugin registry + applier
//!   procs/sink.mova          logic: forward results to out-ch
//!   topology.mova            communication ONLY: wires the three procs
//!
//! None of the procs know how they're wired together, and topology.mova
//! contains no business logic at all -- see README.md for the full
//! architecture explainer. THIS file (main.rs) is lifecycle ONLY: load
//! the four scripts in order, start the flow, watch a plugins directory,
//! inject data + control messages, drain output, shut down. It never
//! contains a line of pipeline logic itself.
//!
//! Run the scripted self-demo (terminates on its own):
//! ```text
//! cargo run --release --example flow_plugins -- --demo
//! ```
//! Or run it live against a real directory and edit plugins yourself:
//! ```text
//! cargo run --release --example flow_plugins my-plugins
//! # then, from another terminal:
//! echo '(fn [ev] (assoc ev :squared (* (:value ev) (:value ev))))' > my-plugins/stats.mova
//! rm my-plugins/stats.mova
//! ```

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use mova::embed::{Engine, Profile, Value};

/// Where the four fixed pipeline files live, independent of the process's
/// current working directory -- this example must run the same way from
/// anywhere `cargo run` is invoked from.
fn pipeline_dir() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/examples/flow_plugins"))
}

/// A [`Value`] shaped like `{:seq 0 :value 1}` -- the smoke-test input
/// every candidate plugin fn is called with once before it's trusted with
/// real traffic. Matches the shape `procs/ingest.mova` actually produces,
/// so a plugin that blows up on real events blows up here too, not three
/// ticks later on the live pipeline.
fn sample_event() -> Value {
    Value::map([(Value::keyword("seq"), Value::from(0i64)), (Value::keyword("value"), Value::from(1i64))])
}

// ---------------------------------------------------------------------
// System: owns the engine + the running flow. Loads the four scripts in
// the load order the plugin_stage/topology split requires (both procs
// exist before topology.mova references their step vars), starts the
// flow paused-then-resumed, and offers the two operations the rest of
// this file needs: inject a data event, inject a control message.
// ---------------------------------------------------------------------
struct System {
    engine: Engine,
    pipeline: Value,
}

impl System {
    fn boot() -> Self {
        // `flow`/`chan`/`>!!`/`<!!` need `Profile::Scripting` -- `Pure`
        // spawns no threads at all, and a flow's procs each need one.
        let mut engine = Engine::builder().profile(Profile::Scripting).build();
        let dir = pipeline_dir();

        // Load order matters: both proc files must be evaluated (their
        // `*-step` vars defined) before topology.mova runs, since
        // topology.mova references them by name.
        for rel in ["procs/ingest.mova", "procs/plugin_stage.mova", "procs/sink.mova", "topology.mova"] {
            let path = dir.join(rel);
            let src = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
            if let Err(e) = engine.eval_named(rel, &src) {
                eprintln!("failed loading {rel}:\n{}", e.render_plain());
                std::process::exit(1);
            }
        }

        let pipeline = engine.get("pipeline").expect("topology.mova defines `pipeline`");
        engine.eval("(flow/start pipeline)").expect("flow/start pipeline");
        engine.eval("(flow/resume pipeline)").expect("flow/resume pipeline");
        println!("  loaded 4 files (3 procs + topology), flow started and resumed");

        System { engine, pipeline }
    }

    /// Injects `n` into `[:ingest :in]` as a one-message batch, drains the
    /// single result it produces off `out-ch`, and prints it.
    fn pump(&mut self, n: i64) {
        let port = Value::vector([Value::keyword("ingest"), Value::keyword("in")]);
        let batch = Value::vector([Value::from(n)]);
        self.engine
            .call_by_name("flow/inject", &[self.pipeline.clone(), port, batch])
            .expect("inject data event");
        let out = self.engine.eval("(<!! out-ch)").expect("drain out-ch");
        println!("  pump {n:>2} -> {out}");
    }

    /// Injects one control message into `[:plugin-stage :ctl]` and drains
    /// exactly one ack off `out-ch` before returning -- the ordering
    /// discipline the whole example hinges on. Separate `flow/inject`
    /// calls into DIFFERENT ports race against each proc's own thread, so
    /// draining the ack here, before the caller injects anything else,
    /// is what keeps "register landed" and "next data event sees it"
    /// externally observable in order. See README.md.
    fn inject_ctl(&mut self, msg: Value) -> Result<Value, String> {
        let port = Value::vector([Value::keyword("plugin-stage"), Value::keyword("ctl")]);
        let batch = Value::vector([msg]);
        self.engine
            .call_by_name("flow/inject", &[self.pipeline.clone(), port, batch])
            .map_err(|e| format!("flow/inject failed: {}", e.render_plain()))?;
        self.engine.eval("(<!! out-ch)").map_err(|e| format!("drain ack failed: {}", e.render_plain()))
    }

    /// Loads `src` under name `name`, requiring its last form to evaluate
    /// to a one-argument fn (the plugin file convention -- see README.md),
    /// then SMOKE-TESTS it against [`sample_event`] before it's ever
    /// wired into the live registry. On any failure the previously
    /// registered version (if any) stays live -- this call simply never
    /// happens.
    fn try_register(&mut self, name: &str, src: &str) -> Result<Value, String> {
        let f = self.engine.eval_named(name, src).map_err(|e| e.render_plain())?;
        self.engine
            .call(&f, &[sample_event()])
            .map_err(|e| format!("loaded, but smoke test call failed:\n{}", e.render_plain()))?;
        let ctl = Value::map([
            (Value::keyword("op"), Value::keyword("register")),
            (Value::keyword("name"), Value::keyword(name)),
            (Value::keyword("fn"), f),
        ]);
        self.inject_ctl(ctl)
    }

    fn deregister(&mut self, name: &str) -> Result<Value, String> {
        let ctl = Value::map([
            (Value::keyword("op"), Value::keyword("deregister")),
            (Value::keyword("name"), Value::keyword(name)),
        ]);
        self.inject_ctl(ctl)
    }
}

// ---------------------------------------------------------------------
// PluginWatcher: the entire directory-CRUD story. Diffs a directory of
// `.mova` files against what it last saw and drives `System::try_register`
// / `System::deregister` accordingly. Same mtime-polling shape as
// `embed_plugin_manager.rs` -- no `notify` dependency, just
// `std::fs::metadata(..).modified()` compared to what was recorded last
// sync.
// ---------------------------------------------------------------------
struct PluginWatcher {
    dir: PathBuf,
    /// Every FILE this watcher has seen, mapped to the mtime it was last
    /// (successfully OR rejectedly) processed at -- recording the mtime
    /// even on rejection is what stops a broken file from being re-loaded
    /// and re-printed every single tick until its NEXT edit.
    mtimes: BTreeMap<String, SystemTime>,
}

impl PluginWatcher {
    fn new(dir: PathBuf) -> Self {
        PluginWatcher { dir, mtimes: BTreeMap::new() }
    }

    /// One reconciliation pass: create/update/delete, in that order.
    fn sync(&mut self, system: &mut System) {
        let mut seen = Vec::new();
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("  [plugins] can't read {:?}: {e}", self.dir);
                return;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "clj") {
                continue;
            }
            let Some(name) = path.file_stem().and_then(|s| s.to_str()).map(String::from) else {
                continue;
            };
            let Ok(mtime) = std::fs::metadata(&path).and_then(|m| m.modified()) else {
                continue;
            };
            seen.push(name.clone());

            let action = match self.mtimes.get(&name) {
                None => "registered",
                Some(m) if *m != mtime => "re-registered",
                _ => continue, // unchanged since last sync -- the common case
            };
            let src = match std::fs::read_to_string(&path) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("  [plugins] can't read {path:?}: {e}");
                    continue;
                }
            };
            self.mtimes.insert(name.clone(), mtime);
            match system.try_register(&name, &src) {
                Ok(ack) => println!("  [plugins] {name} {action}, ack: {ack}"),
                Err(msg) => {
                    println!("  [plugins] REJECTED \"{name}\" (previous version, if any, stays live):");
                    for line in msg.lines() {
                        println!("    | {line}");
                    }
                }
            }
        }
        // Deletions: any previously-seen file that's now gone. Always
        // sends a deregister -- harmless if the name was never actually
        // live in the registry (e.g. it was rejected every time).
        let gone: Vec<String> = self.mtimes.keys().filter(|k| !seen.contains(k)).cloned().collect();
        for name in gone {
            self.mtimes.remove(&name);
            match system.deregister(&name) {
                Ok(ack) => println!("  [plugins] {name} removed, ack: {ack}"),
                Err(msg) => eprintln!("  [plugins] deregister \"{name}\" failed: {msg}"),
            }
        }
    }
}

// ---------------------------------------------------------------------
// Demo script -- the full CRUD lifecycle, scripted and self-terminating.
// ---------------------------------------------------------------------

const STATS_V1: &str = "(fn [ev] (assoc ev :squared (* (:value ev) (:value ev))))";
const SHOUT_V1: &str = r#"(fn [ev] (assoc ev :shout (str "value " (:value ev) "!")))"#;
const STATS_V2: &str = "(fn [ev] (assoc (assoc ev :squared (* (:value ev) (:value ev))) :cubed (* (:value ev) (:value ev) (:value ev))))";
const BROKEN: &str = "(fn [ev] (assoc ev :oops (no-such-fn ev)))";

fn run_demo() {
    let dir = std::env::temp_dir().join("mova-embed-flow-plugins-example");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create demo plugins dir");
    let settle = || std::thread::sleep(Duration::from_millis(20));

    println!("== boot: load 4 files, start the flow =====================");
    let mut system = System::boot();
    let mut watcher = PluginWatcher::new(dir.clone());

    println!();
    println!("== 1. pump with NO plugins registered ======================");
    system.pump(1);
    system.pump(2);

    println!();
    println!("== 2. CREATE stats.mova -- adds :squared ====================");
    std::fs::write(dir.join("stats.mova"), STATS_V1).unwrap();
    watcher.sync(&mut system);
    system.pump(3);

    println!();
    println!("== 3. CREATE shout.mova -- adds :shout, after stats =========");
    settle();
    std::fs::write(dir.join("shout.mova"), SHOUT_V1).unwrap();
    watcher.sync(&mut system);
    system.pump(4);

    println!();
    println!("== 4. UPDATE stats.mova -- also adds :cubed, re-registers ===");
    settle();
    std::fs::write(dir.join("stats.mova"), STATS_V2).unwrap();
    watcher.sync(&mut system);
    system.pump(5);

    println!();
    println!("== 5. CREATE broken.mova -- rejected at the smoke test ======");
    settle();
    std::fs::write(dir.join("broken.mova"), BROKEN).unwrap();
    watcher.sync(&mut system);
    system.pump(6); // unaffected -- broken.mova never made it into the registry

    println!();
    println!("== 6. DELETE shout.mova -- deregisters, drains the ack ======");
    std::fs::remove_file(dir.join("shout.mova")).unwrap();
    watcher.sync(&mut system);
    system.pump(7);

    println!();
    println!("== shutdown =================================================");
    let report = system.engine.shutdown();
    println!("  ShutdownReport {{ flows_stopped: {}, flows_failed: {} }}", report.flows_stopped, report.flows_failed);

    let _ = std::fs::remove_dir_all(&dir);
    println!();
    println!("done -- the program never restarted; only .mova files under {:?} changed.", dir);
}

/// Live mode: watch a real directory, tick once a second, Ctrl-C to quit.
/// Seeds a starter plugin if the directory is empty so there's something
/// to open and edit immediately.
fn run_interactive(dir: PathBuf) {
    std::fs::create_dir_all(&dir).expect("create plugins dir");
    let has_plugins = std::fs::read_dir(&dir)
        .map(|mut d| d.any(|e| e.is_ok_and(|e| e.path().extension().is_some_and(|x| x == "clj"))))
        .unwrap_or(false);
    if !has_plugins {
        std::fs::write(dir.join("stats.mova"), STATS_V1).expect("seed starter plugin");
        println!("seeded starter plugin {:?} -- open it in an editor", dir.join("stats.mova"));
    }
    println!("watching {dir:?} -- create/edit/delete .mova files to change this program live (Ctrl-C quits)");

    let mut system = System::boot();
    let mut watcher = PluginWatcher::new(dir);
    let mut n = 0i64;
    loop {
        watcher.sync(&mut system);
        n += 1;
        system.pump(n);
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--demo") || std::env::var("MOVA_DEMO").is_ok() {
        run_demo();
        return;
    }
    match args.first() {
        Some(dir) => run_interactive(PathBuf::from(dir)),
        None => {
            eprintln!("usage: flow_plugins <plugins-dir>   (or --demo for a scripted run)");
            eprintln!("  e.g.: cargo run --release --example flow_plugins my-plugins");
            std::process::exit(2);
        }
    }
}
