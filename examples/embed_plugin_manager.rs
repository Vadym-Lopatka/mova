//! A Rust program whose behavior IS a directory of `.mova` plugins, with
//! full CRUD on that directory while the program keeps running:
//!
//! * **Create** -- drop `foo.mova` into the plugins dir and plugin `foo`
//!   is live on the next tick.
//! * **Update** -- save an edit to `foo.mova` and the new behavior swaps
//!   in atomically; a BROKEN edit is rejected and the old version keeps
//!   running (same candidate-snapshot trick as `embed_rules_hotreload`).
//! * **Delete** -- remove `foo.mova` and the plugin is gone next tick
//!   (its whole interpreter is simply dropped).
//!
//! The contract a plugin must meet is one function:
//!
//! ```clojure
//! (defn process [n] ...)   ; called every tick with the tick number
//! ```
//!
//! Each plugin lives in its OWN [`Engine::snapshot`] of a shared template
//! engine, so plugins can't see each other's state, a misbehaving plugin
//! can't corrupt its neighbors, and loading one costs microseconds, not a
//! full bootstrap (see `embed_snapshot_pool` for that cost measured).
//!
//! Run the scripted self-demo (terminates on its own):
//! ```text
//! cargo run --release --example embed_plugin_manager -- --demo
//! ```
//! Or run it live against a real directory and edit plugins yourself:
//! ```text
//! cargo run --release --example embed_plugin_manager my-plugins
//! # then, from another terminal:
//! echo '(defn process [n] (* n n))' > my-plugins/square.mova   # create
//! echo '(defn process [n] (* n n n))' > my-plugins/square.mova # update
//! rm my-plugins/square.mova                                    # delete
//! ```

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use mova::embed::{Engine, Profile, Value};

/// One live plugin: its own isolated interpreter plus the file mtime it
/// was loaded from (so we only reload when the file actually changes).
struct Plugin {
    engine: Engine,
    mtime: SystemTime,
}

/// The whole plugin system. The template engine pays the bootstrap cost
/// once; every plugin is a cheap snapshot of it.
struct PluginManager {
    template: Engine,
    plugins: BTreeMap<String, Plugin>,
    dir: PathBuf,
}

impl PluginManager {
    fn new(dir: PathBuf) -> Self {
        PluginManager {
            template: Engine::builder().profile(Profile::Pure).build(),
            plugins: BTreeMap::new(),
            dir,
        }
    }

    /// Loads `src` into a FRESH candidate snapshot and smoke-tests the
    /// `process` entry point before handing the engine back. A plugin that
    /// reads fine but forgot `process` (or whose `process` blows up
    /// immediately) is rejected here, and whatever was live stays live.
    fn try_load(&self, name: &str, src: &str) -> Result<Engine, String> {
        let mut candidate = self.template.snapshot();
        candidate.eval_named(name, src).map_err(|e| e.render_plain())?;
        candidate
            .call_by_name("process", &[Value::from(0i64)])
            .map_err(|e| format!("loaded, but (process 0) failed: {}", e.render_plain()))?;
        Ok(candidate)
    }

    /// One reconciliation pass: diff the directory against the loaded set
    /// and apply creates, updates, and deletes. This is the entire CRUD
    /// story -- called once per tick, cheap when nothing changed.
    fn sync(&mut self) {
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

            let action = match self.plugins.get(&name) {
                None => "loaded",
                Some(p) if p.mtime != mtime => "reloaded",
                Some(_) => continue, // unchanged -- the common case
            };
            let src = match std::fs::read_to_string(&path) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("  [plugins] can't read {path:?}: {e}");
                    continue;
                }
            };
            match self.try_load(&name, &src) {
                Ok(engine) => {
                    self.plugins.insert(name.clone(), Plugin { engine, mtime });
                    println!("  [plugins] {action} \"{name}\"");
                }
                Err(msg) => {
                    // Remember the mtime even on failure so we don't retry
                    // (and re-print the error) every tick until the NEXT
                    // edit. An already-live old version stays live.
                    if let Some(p) = self.plugins.get_mut(&name) {
                        p.mtime = mtime;
                    }
                    println!("  [plugins] REJECTED \"{name}\" (previous version, if any, stays live):");
                    for line in msg.lines() {
                        println!("    | {line}");
                    }
                }
            }
        }
        // Delete: any loaded plugin whose file vanished. Dropping the
        // `Plugin` drops its whole Engine -- that's the entire teardown.
        self.plugins.retain(|name, _| {
            let keep = seen.contains(name);
            if !keep {
                println!("  [plugins] removed \"{name}\"");
            }
            keep
        });
    }

    /// The host's actual work: hand the tick number to every plugin and
    /// show what each one made of it. A plugin erroring mid-tick is
    /// reported inline and does not disturb the others.
    fn tick(&mut self, n: i64) {
        let mut line = format!("tick {n:>3} |");
        if self.plugins.is_empty() {
            line.push_str(&format!(" (no plugins -- drop a .mova file into {:?})", self.dir));
        }
        for (name, plugin) in self.plugins.iter_mut() {
            match plugin.engine.call_by_name("process", &[Value::from(n)]) {
                Ok(v) => line.push_str(&format!(" {name}: {v} ")),
                Err(e) => line.push_str(&format!(" {name}: <error: {e}> ")),
            }
        }
        println!("{line}");
    }
}

const GREET_V1: &str = r#"(defn process [n] (str "hello #" n))"#;
const DOUBLE_V1: &str = "(defn process [n] (* 2 n))";
const DOUBLE_V2: &str = "(defn process [n] {:tick n :doubled (* 2 n) :squared (* n n)})";
const GREET_BROKEN: &str = "(defn process [n] (str \"hi \" (no-such-fn n)))";

/// Scripted walk through the full CRUD lifecycle. Every step is the same
/// two calls the interactive loop makes -- `sync()` then `tick()` -- only
/// the file edits between them are automated.
fn run_demo() {
    let dir = std::env::temp_dir().join("mova-embed-plugin-manager-example");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create demo plugins dir");
    let mut mgr = PluginManager::new(dir.clone());
    // File writes need a tiny gap so mtimes actually differ.
    let settle = || std::thread::sleep(Duration::from_millis(20));

    println!("== CREATE: greet.mova appears =============================");
    std::fs::write(dir.join("greet.mova"), GREET_V1).unwrap();
    mgr.sync();
    mgr.tick(1);

    println!("== CREATE: double.mova appears alongside it ===============");
    settle();
    std::fs::write(dir.join("double.mova"), DOUBLE_V1).unwrap();
    mgr.sync();
    mgr.tick(2);

    println!("== UPDATE: double.mova edited to return a map =============");
    settle();
    std::fs::write(dir.join("double.mova"), DOUBLE_V2).unwrap();
    mgr.sync();
    mgr.tick(3);

    println!("== UPDATE (broken): greet.mova calls an undefined fn ======");
    settle();
    std::fs::write(dir.join("greet.mova"), GREET_BROKEN).unwrap();
    mgr.sync();
    mgr.tick(4); // greet still answers with its OLD, working version

    println!("== DELETE: double.mova removed ============================");
    std::fs::remove_file(dir.join("double.mova")).unwrap();
    mgr.sync();
    mgr.tick(5);

    let _ = std::fs::remove_dir_all(&dir);
    println!("done -- the program never restarted; only .mova files changed.");
}

/// Live mode: watch a real directory, tick once a second, Ctrl-C to quit.
/// If the directory is empty it gets a starter plugin so there's something
/// to open and edit immediately.
fn run_interactive(dir: PathBuf) {
    std::fs::create_dir_all(&dir).expect("create plugins dir");
    let has_plugins = std::fs::read_dir(&dir)
        .map(|mut d| d.any(|e| e.is_ok_and(|e| e.path().extension().is_some_and(|x| x == "clj"))))
        .unwrap_or(false);
    if !has_plugins {
        std::fs::write(dir.join("greet.mova"), GREET_V1).expect("seed starter plugin");
        println!("seeded starter plugin {:?} -- open it in an editor", dir.join("greet.mova"));
    }
    println!("watching {dir:?} -- create/edit/delete .mova files to change this program live (Ctrl-C quits)");
    let mut mgr = PluginManager::new(dir);
    let mut n = 0i64;
    loop {
        mgr.sync();
        n += 1;
        mgr.tick(n);
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
            eprintln!("usage: embed_plugin_manager <plugins-dir>   (or --demo for a scripted run)");
            eprintln!("  e.g.: cargo run --release --example embed_plugin_manager my-plugins");
            std::process::exit(2);
        }
    }
}
