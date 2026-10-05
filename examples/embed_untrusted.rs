//! Running script text you did not write: [`Profile::Untrusted`] plus
//! fuel. A hostile/looping script gets cut off by [`Error::is_fuel_exhausted`]
//! instead of hanging the host thread forever, and [`Engine::set_fuel`]
//! shows how a host might grant a bigger retry budget for a script that
//! legitimately needs more steps -- while still refusing to run it
//! unbounded.
//!
//! **Fuel is not a sandbox**: fuel bounds interpreter steps, not
//! native-call time or memory — pair with a process watchdog for
//! untrusted hosting. See [`Profile::Untrusted`]'s own doc for the full
//! caveat; this example only demonstrates the fuel half.
//!
//! Run: `cargo run --release --example embed_untrusted`

use mova::embed::{Engine, Profile};

fn main() {
    // `Profile::Untrusted` = `Pure`'s capability surface (no filesystem,
    // no network, no thread spawning) PLUS fuel treated as required: if
    // `.fuel(..)` is never called, `build()` applies a documented default
    // of 10,000,000 steps rather than leaving the engine unbounded.
    let mut engine = Engine::builder().profile(Profile::Untrusted).fuel(200_000).build();

    println!("-- a well-behaved script runs fine under a modest budget --");
    match engine.eval("(reduce + (range 1000))") {
        Ok(v) => println!("  (reduce + (range 1000)) = {v}"),
        Err(e) => {
            eprintln!("unexpected failure: {}", e.render_plain());
            std::process::exit(1);
        }
    }

    println!();
    println!("-- a hostile infinite loop gets cut off, not run forever --");
    let hostile = "(loop [n 0] (recur (inc n)))";
    match engine.eval(hostile) {
        Ok(v) => {
            eprintln!("expected fuel exhaustion, got a value instead: {v}");
            std::process::exit(1);
        }
        Err(e) => {
            println!("  eval returned Err, is_fuel_exhausted = {}", e.is_fuel_exhausted());
            assert!(e.is_fuel_exhausted(), "the infinite loop should fail specifically on fuel, not some other error");
        }
    }

    println!();
    println!("-- try/catch CANNOT swallow fuel exhaustion -- the host always regains control --");
    match engine.eval("(try (loop [n 0] (recur (inc n))) (catch e :caught-it))") {
        Ok(v) => {
            eprintln!("a script should never be able to catch FuelExhausted, got: {v}");
            std::process::exit(1);
        }
        Err(e) => println!("  still Err at the host boundary, is_fuel_exhausted = {}", e.is_fuel_exhausted()),
    }

    println!();
    println!("-- a legitimately expensive script fails on the same budget --");
    // A real script-level `loop`/`recur` (not a native like `reduce`/
    // `range`, which iterate internally in Rust and tick no fuel at all --
    // see this example's Paper cuts entry in bench/optimization-log.md).
    let heavier = "(loop [n 0 acc 0] (if (< n 1000000) (recur (inc n) (+ acc n)) acc))";
    let failed_first = match engine.eval(heavier) {
        Ok(v) => {
            eprintln!("expected the 200k-fuel budget to be too small for this, got: {v}");
            std::process::exit(1);
        }
        Err(e) => {
            println!("  first attempt: fuel exhausted (is_fuel_exhausted = {})", e.is_fuel_exhausted());
            e.is_fuel_exhausted()
        }
    };
    assert!(failed_first);

    println!("-- host grants a bigger retry budget via Engine::set_fuel and tries again --");
    engine.set_fuel(Some(50_000_000));
    match engine.eval(heavier) {
        Ok(v) => println!("  retry with 50M fuel succeeded: {v}"),
        Err(e) => {
            eprintln!("unexpected failure even at 50M fuel: {}", e.render_plain());
            std::process::exit(1);
        }
    }

    println!();
    println!("REMINDER: fuel bounds interpreter steps, not native-call time or memory --");
    println!("pair Profile::Untrusted with a process watchdog for real untrusted hosting.");
    println!("(Profile::Untrusted never registers sys/thread-spawning natives at all, so");
    println!(" there is no `slurp`/`sh`/`future` to abuse in the first place -- but a");
    println!(" pathological native like a regex blowup inside `re-matches` still spends");
    println!(" zero fuel while it runs.)");
}
