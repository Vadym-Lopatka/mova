//! The "5-minute hello world" for embedding: an app that ships with a
//! user-editable `.mova` config file (feature flags, connection limits,
//! timeouts -- the kind of thing ops hand-edits in production), loads it
//! through a [`Profile::Pure`] engine (no filesystem/network access FROM
//! the script itself -- only the host reads the file), and pulls typed
//! Rust values back out. Also shows what happens when someone hand-edits
//! the file into something broken: the load fails with a rendered
//! diagnostic instead of a panic.
//!
//! Run: `cargo run --release --example embed_config`

use std::path::Path;

use mova::embed::{Engine, Error, Profile, Value};

/// What the host actually wants out of the config -- typed Rust data, not
/// a script `Value` the rest of the app would have to keep poking at.
#[derive(Debug)]
struct AppConfig {
    app_name: String,
    max_connections: i64,
    timeout_ms: i64,
    feature_flags: Vec<String>,
}

/// Reads and evaluates `path`, then extracts the fields `AppConfig` needs.
///
/// The config value itself is just a Clojure map -- `{:app-name "..." ...}`.
/// Rather than manually walking `Value::entries()` (map keys come back as
/// `Value`s with no accessor to read a keyword's text directly -- see
/// `bench/optimization-log.md`'s Paper cuts entry for this example), this
/// pulls each field by CALLING the keyword as a fn, exactly like Clojure
/// script itself would with `(:app-name config)` -- a keyword is callable
/// against a map through `Engine::call`, not just usable inside script.
fn load_config(engine: &mut Engine, path: &Path) -> Result<AppConfig, Error> {
    let src = std::fs::read_to_string(path)
        .map_err(|e| Error::other(format!("couldn't read config {path:?}: {e}")))?;
    let config = engine.eval_named(&path.to_string_lossy(), &src)?;

    let field = |engine: &mut Engine, name: &str| -> Result<Value, Error> {
        engine.call(&Value::keyword(name), std::slice::from_ref(&config))
    };

    let app_name = field(engine, "app-name")?
        .as_str()
        .ok_or_else(|| Error::other(":app-name must be a string"))?
        .to_string();
    let max_connections = field(engine, "max-connections")?
        .as_i64()
        .ok_or_else(|| Error::other(":max-connections must be an int"))?;
    let timeout_ms = field(engine, "timeout-ms")?
        .as_i64()
        .ok_or_else(|| Error::other(":timeout-ms must be an int"))?;

    let flags_val = field(engine, "feature-flags")?;
    let feature_flags: Vec<String> = flags_val
        .iter()
        // Keywords print as `:name` via `Display`/`pr_str`; stripping the
        // leading `:` is the workaround for the same missing-accessor gap
        // `field` above routes around by calling INTO the engine instead.
        .map(|kw| kw.to_string().trim_start_matches(':').to_string())
        .collect();

    Ok(AppConfig {
        app_name,
        max_connections,
        timeout_ms,
        feature_flags,
    })
}

const GOOD_CONFIG: &str = r#"
{:app-name "widgetd"
 :max-connections 64
 :timeout-ms 2500
 :feature-flags [:beta-ui :metrics]}
"#;

// Missing a closing brace -- what a hand-edit gone wrong actually looks
// like, not a contrived syntax error.
const BROKEN_CONFIG: &str = r#"
{:app-name "widgetd"
 :max-connections 64
"#;

fn main() {
    let dir = std::env::temp_dir().join("mova-embed-config-example");
    std::fs::create_dir_all(&dir).expect("create example scratch dir");
    let good_path = dir.join("app.mova");
    let broken_path = dir.join("app-broken.mova");
    std::fs::write(&good_path, GOOD_CONFIG).expect("write example config");
    std::fs::write(&broken_path, BROKEN_CONFIG).expect("write example config");

    let mut engine = Engine::builder().profile(Profile::Pure).build();

    println!("-- loading a well-formed config --");
    match load_config(&mut engine, &good_path) {
        Ok(cfg) => {
            println!("  app_name:        {}", cfg.app_name);
            println!("  max_connections: {}", cfg.max_connections);
            println!("  timeout_ms:      {}", cfg.timeout_ms);
            println!("  feature_flags:   {:?}", cfg.feature_flags);
        }
        Err(e) => {
            eprintln!("unexpected failure loading a valid config:\n{}", e.render_plain());
            std::process::exit(1);
        }
    }

    println!();
    println!("-- loading a hand-edited, broken config --");
    match load_config(&mut engine, &broken_path) {
        Ok(cfg) => {
            eprintln!("expected the broken config to fail, got: {cfg:?}");
            std::process::exit(1);
        }
        Err(e) => {
            println!("  load failed as expected (unclosed `{{`):");
            println!("  is_incomplete_input: {}", e.is_incomplete_input());
            for line in e.render_plain().lines() {
                println!("  | {line}");
            }
        }
    }
}
