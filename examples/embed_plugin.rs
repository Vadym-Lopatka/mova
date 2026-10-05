//! Plugin/scripting hook pattern: the host exposes a handful of native
//! Rust functions (an inventory lookup, an order-placement fn that can
//! fail), and a small user-supplied script orchestrates them -- deciding
//! WHEN and IN WHAT ORDER to call the host's capabilities, while the host
//! keeps full control over what those capabilities actually do. Shows both
//! [`Engine::register_fn`] (unchecked arity) and
//! [`Engine::register_fn_with_arity`] (checked arity, generated error
//! message), plus error propagation in both directions: a native fn's
//! `Err` becoming a script-catchable exception, and an uncaught script
//! error propagating back out to the host as `Engine::eval`'s `Err`.
//!
//! Run: `cargo run --release --example embed_plugin`

use std::collections::HashMap;
use std::sync::Mutex;

use mova::embed::{Arity, Engine, Error, Profile, Value};

fn main() {
    let mut engine = Engine::builder().profile(Profile::Pure).build();

    // A tiny "inventory" the plugin script gets to query and mutate --
    // standing in for whatever real host-side capability (a database, a
    // device driver, a game entity table) a real plugin would orchestrate.
    let inventory: &'static Mutex<HashMap<String, i64>> =
        Box::leak(Box::new(Mutex::new(HashMap::from([("widget".to_string(), 3i64)]))));

    // Unchecked arity: `register_fn` accepts any argument count and leaves
    // validation to the closure body.
    engine.register_fn("host/stock-of", move |args| {
        let sku = args[0].as_str().ok_or_else(|| Error::other("host/stock-of: expected a string sku"))?;
        let qty = inventory.lock().unwrap().get(sku).copied().unwrap_or(0);
        Ok(Value::from(qty))
    });

    // Checked arity: a wrong-arity call gets a normal mova arity error
    // (naming "host/take!" and the expected count) BEFORE the closure ever
    // runs, instead of `take!` having to hand-roll its own `args.len()`
    // check.
    engine.register_fn_with_arity("host/take!", Arity::Exact(2), move |args| {
        let sku = args[0].as_str().ok_or_else(|| Error::other("host/take!: expected a string sku"))?;
        let n = args[1].as_i64().ok_or_else(|| Error::other("host/take!: expected an int count"))?;
        let mut stock = inventory.lock().unwrap();
        let have = stock.get(sku).copied().unwrap_or(0);
        if have < n {
            // A native fn's `Err` crosses into script as a normal
            // exception -- catchable with `try`/`catch`, same as any
            // script-raised error.
            return Err(Error::other(format!("host/take!: only {have} {sku} in stock, asked for {n}")));
        }
        stock.insert(sku.to_string(), have - n);
        Ok(Value::from(have - n))
    });

    println!("-- orchestration script: check stock, take what's available, catch the rest --");
    let script = r#"
        (defn fulfil [sku want]
          (let [have (host/stock-of sku)]
            (if (<= want have)
              (try
                (host/take! sku want)
                (catch e {:ok false :error (str e)}))
              {:ok false :error (str "not enough " sku ": have " have ", want " want)})))

        [(fulfil "widget" 2)
         (fulfil "widget" 5)]
    "#;
    match engine.eval(script) {
        Ok(v) => {
            for (i, result) in v.iter().enumerate() {
                println!("  fulfil #{i}: {result}");
            }
        }
        Err(e) => {
            eprintln!("unexpected script failure:\n{}", e.render_plain());
            std::process::exit(1);
        }
    }

    // Remaining stock proves the successful `take!` really mutated
    // host-side state, and the failed one didn't.
    let remaining = engine.eval("(host/stock-of \"widget\")").unwrap();
    println!("  remaining widget stock: {}", remaining.as_i64().unwrap());

    println!();
    println!("-- an UNCAUGHT script error propagates straight back to the host --");
    match engine.eval(r#"(host/take! "widget" 999)"#) {
        Ok(v) => {
            eprintln!("expected this call to fail, got: {v}");
            std::process::exit(1);
        }
        Err(e) => println!("  Engine::eval returned Err as expected: {e}"),
    }

    println!();
    println!("-- a wrong-arity call gets a generated arity error, not a panic --");
    match engine.eval(r#"(host/take! "widget")"#) {
        Ok(v) => {
            eprintln!("expected this call to fail, got: {v}");
            std::process::exit(1);
        }
        Err(e) => println!("  {e}"),
    }
}
