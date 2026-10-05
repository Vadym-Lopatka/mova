//! mova CLI: no args -> REPL; `mova <file.mova>` runs a file; `mova -e
//! "<expr>"` evaluates one expression and prints its `pr_str`. See
//! ARCHITECTURE.md's "Crate layout" for the contract.
//!
//! The interpreter is a tree-walker (deep non-tail Rust recursion for deep
//! mova recursion), so every mode of operation -- including the REPL --
//! runs on a dedicated worker thread with a large stack rather than trusting
//! the OS-default main-thread stack; the main thread only spawns it, joins
//! it, and propagates its exit code.

use std::path::{Path, PathBuf};

use rustyline::error::ReadlineError;
use rustyline::DefaultEditor;

use mova::embed::{Engine, Profile, Value};

/// W4 bench-only configuration (`--features bench-mimalloc`): swap the
/// global allocator to price the allocator FRONT-END against the
/// allocation-count diet. Never enabled by default; see LATENCY-CAMPAIGN.md
/// §3 W4's step-2 kill-shot.
#[cfg(all(any(feature = "bench-mimalloc", feature = "mimalloc-alloc"), not(feature = "heap-prof")))]
#[global_allocator]
static BENCH_MIMALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

// M9 diagnostic: sampling heap profiler around mimalloc (MOVA_HEAPPROF=<dir>).
#[cfg(feature = "heap-prof")]
mod heapprof;
#[cfg(feature = "heap-prof")]
#[global_allocator]
static HEAP_PROF: heapprof::Prof = heapprof::Prof;

const EVAL_STACK_SIZE: usize = 512 * 1024 * 1024;

/// Raises `Interp::max_depth` (default 200, tuned for a modest OS-default
/// stack -- see `eval::Interp`'s field doc) now that every CLI mode runs on
/// the dedicated `mova-eval` thread above, which has room for much deeper
/// mova-level recursion.
const EVAL_MAX_CALL_DEPTH: usize = 10_000;

const HELP: &str = concat!(
    "mova ", env!("CARGO_PKG_VERSION"), " — a system-level Clojure on Rust\n",
    "\n",
    "USAGE:\n",
    "    mova                  start the REPL\n",
    "    mova <file.mova>      run a file\n",
    "    mova -e \"<expr>\"      evaluate an expression and print its result\n",
    "    mova -h, --help       print this help\n",
    "    mova --version        print the version\n",
    "    mova nrepl [options]  start an nREPL server; `mova nrepl --help` lists\n",
    "                          the options\n",
    "\n",
    "OPTIONS:\n",
    "    --module-path a:b:c    colon-separated directories searched, in order,\n",
    "                           for a required namespace's file (`(ns x (:require\n",
    "                           y.z))` looks for y/z.mova in each).\n",
    "                           Default: the directory of the file being run, or\n",
    "                           the working directory for -e and the REPL.\n",
);

fn main() {
    let t0 = std::time::Instant::now();
    // P0a: `mova nrepl` is dispatched before ANY interpreter work.
    // P0a probe: version without the 512 MB-stack worker thread
    if std::env::args_os().nth(1).is_some_and(|a| a == "--early-version") {
        println!("mova {}", env!("CARGO_PKG_VERSION"));
        std::process::exit(0);
    }
    // `--module-path` may come before or after `nrepl`; take it out first,
    // with the same parser the runner uses.
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.iter().any(|a| a == "nrepl") {
        match split_module_path(&argv) {
            Ok((module_paths, rest)) if rest.first().is_some_and(|a| a == "nrepl") => {
                #[cfg(feature = "heap-prof")]
                heapprof::init();
                std::process::exit(nrepl_main(t0, module_paths, &rest[1..]));
            }
            Ok(_) => {}
            Err(msg) => {
                eprintln!("mova: {msg}\n\n{HELP}");
                std::process::exit(2);
            }
        }
    }
    #[cfg(feature = "heap-prof")]
    heapprof::init();
    mova::load_trace::mark_start();
    let mut args: Vec<String> = std::env::args().collect();
    // MOVA-PATCH: --no-reflection-warnings force-disables reflwarn analysis
    // (same effect as MOVA_REFLECTION_WARNINGS=0) regardless of
    // *warn-on-reflection* in loaded code; set the env var before any
    // Interp exists so reflwarn's OnceLock reads it on first use.
    if let Some(pos) = args.iter().position(|a| a == "--no-reflection-warnings") {
        args.remove(pos);
        std::env::set_var("MOVA_REFLECTION_WARNINGS", "0");
    }
    let spawned = std::thread::Builder::new()
        .stack_size(EVAL_STACK_SIZE)
        .name("mova-eval".to_string())
        .spawn(move || run(&args[1..]));

    let handle = match spawned {
        Ok(h) => h,
        Err(e) => {
            eprintln!("mova: couldn't start the evaluator thread: {e}");
            std::process::exit(1);
        }
    };

    let code = match handle.join() {
        Ok(code) => code,
        Err(_) => {
            eprintln!("mova: internal error: the evaluator thread panicked");
            101
        }
    };
    // PERF-PROBE: totals print via the `atexit` hook `load_trace::enabled`
    // registers (covers both this path and a direct `process::exit` from
    // inside evaluated code, e.g. clojure-lsp's `--version`).
    std::process::exit(code);
}

/// Dispatches on hand-parsed argv (everything after argv[0]); runs entirely
/// on the large-stack worker thread spawned by `main`.
fn run(args: &[String]) -> i32 {
    let (module_paths, rest) = match split_module_path(args) {
        Ok(split) => split,
        Err(msg) => {
            eprintln!("mova: {msg}\n\n{HELP}");
            return 2;
        }
    };
    let args = &rest[..];
    if args.is_empty() {
        return repl(module_paths);
    }
    match args[0].as_str() {
        "-h" | "--help" => {
            print!("{HELP}");
            0
        }
        "--source-index" => {
            // no heap image needed: registration only, then print and exit
            std::env::remove_var("MOVA_IMAGE");
            mova::srcindex::enable();
            let _engine = new_engine(Vec::new());
            println!("{}", mova::srcindex::json());
            0
        }
        "--version" => {
            println!("mova {}", env!("CARGO_PKG_VERSION"));
            0
        }
        "-e" => match args.get(1) {
            Some(expr) if args.len() == 2 => run_eval(expr, module_paths),
            _ => {
                eprintln!("mova: -e requires exactly one expression argument\n\n{HELP}");
                2
            }
        },
        // MOVA-PATCH: trailing args after the file path are the script's
        // own argv, bound to *command-line-args* (see run_file), not an
        // error -- matches `clojure -M foo.clj a b c`.
        path => run_file(path, &args[1..], module_paths),
    }
}

/// Pulls `--module-path a:b:c` (or `--module-path=a:b:c`) out of argv
/// wherever it appears, leaving the rest of the arguments in order for the
/// mode dispatch above. `None` = the flag wasn't given, which each mode
/// turns into its own default.
fn split_module_path(args: &[String]) -> Result<(Option<Vec<PathBuf>>, Vec<String>), String> {
    let mut paths: Option<Vec<PathBuf>> = None;
    let mut rest: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        let spec = if arg == "--module-path" {
            i += 1;
            match args.get(i) {
                Some(v) => v.as_str(),
                None => return Err("--module-path requires a colon-separated list".to_string()),
            }
        } else if let Some(v) = arg.strip_prefix("--module-path=") {
            v
        } else {
            rest.push(args[i].clone());
            i += 1;
            continue;
        };
        paths = Some(
            spec.split(':')
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
                .collect(),
        );
        i += 1;
    }
    Ok((paths, rest))
}

/// What `-e` and the REPL search when `--module-path` wasn't given: the
/// working directory, so `mova -e '(ns u (:require app.core))'` behaves
/// like every other tool run from the project root.
fn default_module_paths() -> Vec<PathBuf> {
    vec![PathBuf::from(".")]
}

/// The engine every CLI mode runs on: the big-stack depth limit plus the
/// module path this mode resolves `(:require ...)` against.
///
/// `MOVA_FUEL=<n>` (read once per process, like `MOVA_NO_COMPILE`/
/// `MOVA_NO_NUMLOOP`) sets the per-eval fuel budget -- the CLI's own
/// opt-in surface for the embedding-fuel probe, purely so
/// `bench/fuel-lcg.mova` and friends can be driven through the ordinary
/// release binary for the A/B overhead measurement without a dedicated
/// harness binary. Unset (the default) leaves fuel unlimited -- ordinary
/// `mova` runs are completely unaffected.
fn new_engine(module_paths: Vec<PathBuf>) -> Engine {
    let mut builder = Engine::builder()
        .profile(Profile::Scripting)
        .max_depth(EVAL_MAX_CALL_DEPTH)
        .module_paths(module_paths);
    if let Some(fuel) = std::env::var("MOVA_FUEL").ok().and_then(|v| v.parse::<u64>().ok()) {
        builder = builder.fuel(fuel);
    }
    builder.build()
}

/// Runs `path`: reads the whole file, evaluates it top to bottom, prints
/// nothing itself (only what the program prints via `print`/`println`/
/// etc). A reader or runtime error renders as a labeled diagnostic to
/// stderr and the process exits 1.
fn run_file(path: &str, script_args: &[String], module_paths: Option<Vec<PathBuf>>) -> i32 {
    let source = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("mova: couldn't read {path:?}: {e}");
            return 1;
        }
    };
    // Without an explicit `--module-path`, a file's requires resolve
    // against its OWN directory -- `mova src/app.mova` finds `src/lib.mova`
    // the way running it from that directory would.
    let module_paths = module_paths.unwrap_or_else(|| {
        vec![Path::new(path)
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."))
            .to_path_buf()]
    });
    let mut engine = new_engine(module_paths);
    // MOVA-PATCH: *command-line-args* -- real Clojure's `clojure -M foo.clj
    // a b c` binds the trailing argv as a seq the script can read; nil
    // when there are none, same as real Clojure (not an empty seq).
    let cli_args = if script_args.is_empty() {
        Value::from(())
    } else {
        Value::vector(script_args.iter().map(|s| Value::from(s.as_str())))
    };
    engine.def("*command-line-args*", cli_args.clone());
    // MOVA-PATCH: `*file*` -- real Clojure binds this per-file at compile
    // time; Mova doesn't track that per-namespace, so give it real
    // Clojure's own REPL/non-compiling default ("NO_SOURCE_PATH") just so
    // the symbol resolves (logger.clj's info/warn/error/debug macros read
    // it unconditionally for log metadata, regardless of *logger* being set).
    engine.def("*file*", Value::from("NO_SOURCE_PATH"));
    // Heap-image gate-1 probe (docs/HEAP-IMAGE-DESIGN.md): hidden, env-gated.
    if let (Ok(img), Ok(pre)) = (std::env::var("MOVA_IMAGE"), std::env::var("MOVA_IMAGE_PRELOAD")) {
        let t_img = std::time::Instant::now();
        let res = mova::image::run_preload(engine.image_interp_mut(), &img, &pre);
        if mova::metrics::enabled() {
            let ns = t_img.elapsed().as_nanos() as u64;
            let hit = matches!(res, Ok(true));
            mova::metrics::startup(vec![
                ("image.hit", hit.into()),
                (if hit { "image.restore_ns" } else { "preload_ns" }, ns.into()),
                ("jit.enabled", mova::internal::jit_enabled().into()),
                ("jit.fns_restored", mova::internal::jit_aot_bound().into()),
            ]);
        }
        match res {
            Ok(true) => {
                // the image carries the WRITER's argv; rebind ours
                engine.def("*command-line-args*", cli_args.clone());
                engine.def("*file*", Value::from("NO_SOURCE_PATH"));
            }
            Ok(false) => {}
            Err(e) => eprintln!("mova: image: {e}"),
        }
    }
    let code = match engine.eval_named(path, &source) {
        Ok(_) => 0,
        Err(e) => {
            eprintln!("{}", e.render_plain());
            1
        }
    };
    // E3 (V05-PERF-PLAN) probe: MOVA_MAP_PROBE=1-gated report of
    // map-touching-builtin frequency/size histograms and flow proc state
    // identity, printed once here so it covers the whole run (including
    // any flow procs whose threads `flow/stop` already joined by the time
    // `eval_named` returns). No-op -- one atomic load -- when unset. Not
    // facade-worthy (a debug/perf-probe hook, not something a real
    // embedder needs), so this reaches through `mova::internal` rather
    // than `mova::embed`.
    mova::internal::map_probe::print_report();
    lens_at_exit(&mut engine);
    code
}

/// field4/W-LENS-1's batch/CLI surface. Silent unless `MOVA_LENS` asked for
/// output: `dump` prints the whole regret ledger as EDN, `warn` prints only
/// the threshold crossings. Unset -- the default, and every existing run --
/// prints nothing at all, which is the W-ADX noise discipline restated: the
/// counters are always on, the OUTPUT never is unless asked for.
///
/// Goes through the public `Engine::lens_report`, not a private path, so the
/// CLI is a consumer of the same embed API a host uses.
fn lens_at_exit(engine: &mut mova::embed::Engine) {
    match mova::internal::lens::mode() {
        mova::internal::lens::Mode::Off => {}
        mova::internal::lens::Mode::Dump => eprintln!("{}", engine.lens_report()),
        mova::internal::lens::Mode::Warn => {
            for line in mova::internal::lens::warning_lines() {
                eprintln!("lens: {line}");
            }
        }
    }
}

/// Evaluates `expr` and prints its `pr_str` to stdout. Errors render to
/// stderr and exit 1.
fn run_eval(expr: &str, module_paths: Option<Vec<PathBuf>>) -> i32 {
    let mut engine = new_engine(module_paths.unwrap_or_else(default_module_paths));
    let result = match engine.eval_named("cmdline", expr) {
        Ok(v) => engine.realize(&v),
        Err(e) => Err(e),
    };
    let code = match result {
        Ok(v) => {
            println!("{v}");
            0
        }
        Err(e) => {
            eprintln!("{}", e.render_plain());
            1
        }
    };
    lens_at_exit(&mut engine);
    code
}

/// Interactive REPL: rustyline-backed, multi-line aware (keeps reading
/// continuation lines while the reader reports an incomplete/unclosed
/// form), with persistent `*1`/`*2`/`*3` history vars and `~/.mova_history`
/// persistence. Runs until Ctrl-D (exit 0) or a fatal line-editor error.
fn repl(module_paths: Option<Vec<PathBuf>>) -> i32 {
    println!("mova {} — a system-level Clojure on Rust", env!("CARGO_PKG_VERSION"));
    println!("  precise Arc, no tracing GC, no L30-class bugs by construction");

    let mut engine = new_engine(module_paths.unwrap_or_else(default_module_paths));
    for var in ["*1", "*2", "*3"] {
        engine.def(var, Value::from(()));
    }
    // a `(set! *print-length* 5)` on one line holds for the next lines, as in `clojure.main`
    engine.keep_script_bindings();

    let mut editor = match DefaultEditor::new() {
        Ok(ed) => ed,
        Err(e) => {
            eprintln!("mova: couldn't start the line editor: {e}");
            return 1;
        }
    };

    let history_path = std::env::var("HOME")
        .ok()
        .map(|home| format!("{home}/.mova_history"));
    if let Some(path) = &history_path {
        let _ = editor.load_history(path);
    }

    let mut buffer = String::new();
    loop {
        let prompt = if buffer.is_empty() { "user=> " } else { "  ...=> " };
        match editor.readline(prompt) {
            Ok(line) => {
                if !buffer.is_empty() {
                    buffer.push('\n');
                }
                buffer.push_str(&line);
                handle_input(&mut engine, &mut editor, &mut buffer);
            }
            Err(ReadlineError::Interrupted) => {
                buffer.clear();
            }
            Err(ReadlineError::Eof) => {
                println!("bye!");
                break;
            }
            Err(e) => {
                eprintln!("mova: line editor error: {e}");
                break;
            }
        }
    }

    if let Some(path) = &history_path {
        let _ = editor.save_history(path);
    }
    0
}

/// Tries to evaluate the accumulated REPL `buffer` as a complete batch of
/// top-level forms (`Engine::eval_named` reads the whole buffer before
/// evaluating anything, so an incomplete-form error below is always a pure
/// read failure with no side effects yet). On an incomplete-form error,
/// leaves `buffer` alone so the next `readline` call appends a
/// continuation line. Otherwise records history and clears `buffer`: on
/// success, prints the (deep-realized) result and shifts `*1`/`*2`/`*3`;
/// on any other error (malformed syntax or a runtime failure), renders it.
fn handle_input(engine: &mut Engine, editor: &mut DefaultEditor, buffer: &mut String) {
    let result = engine.eval_named("repl", buffer);
    if let Err(e) = &result {
        if e.is_incomplete_input() {
            return;
        }
    }

    if !buffer.trim().is_empty() {
        let _ = editor.add_history_entry(buffer.as_str());
    }
    buffer.clear();

    match result {
        Ok(v) => {
            match engine.realize(&v) {
                Ok(realized) => println!("{realized}"),
                Err(e) => {
                    eprintln!("{}", e.render_plain());
                    return;
                }
            }
            record_result(engine, v);
        }
        Err(e) => eprintln!("{}", e.render_plain()),
    }
}

/// Shifts the `*1`/`*2`/`*3` REPL history vars after a successful eval:
/// `*1` is the most recent result, `*2`/`*3` the two before that.
fn record_result(engine: &mut Engine, result: Value) {
    let star1 = engine.get("*1").unwrap_or_else(|| Value::from(()));
    let star2 = engine.get("*2").unwrap_or_else(|| Value::from(()));
    engine.def("*3", star2);
    engine.def("*2", star1);
    engine.def("*1", result);
}

#[cfg(test)]
mod command_line_args_tests {
    use super::*;
    use mova::embed::ValueKind;

    #[test]
    fn bound_to_a_seqable_of_the_trailing_argv() {
        let mut engine = new_engine(default_module_paths());
        let args = vec!["a".to_string(), "b".to_string()];
        engine.def(
            "*command-line-args*",
            Value::vector(args.iter().map(|s| Value::from(s.as_str()))),
        );
        let raw = engine.eval_named("test", "*command-line-args*").unwrap();
        let v = engine.realize(&raw).unwrap();
        let items: Vec<String> = v.iter().map(|x| x.as_str().unwrap().to_string()).collect();
        assert_eq!(items, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn nil_when_no_trailing_argv() {
        let mut engine = new_engine(default_module_paths());
        engine.def("*command-line-args*", Value::from(()));
        let raw = engine.eval_named("test", "*command-line-args*").unwrap();
        let v = engine.realize(&raw).unwrap();
        assert_eq!(v.kind(), ValueKind::Nil);
    }
}

// ---------------------------------------------------------------------
// `mova nrepl`: the nREPL server (crates/mova-nrepl is the wire layer; this
// part is the Backend that reaches the interpreter, and the command line).
// ---------------------------------------------------------------------

/// Files to delete on exit: `.nrepl-port` and the Unix socket, if any.
static NREPL_CLEANUP: std::sync::OnceLock<Vec<std::ffi::CString>> = std::sync::OnceLock::new();

/// Async-signal-safe: `unlink` only.
fn nrepl_cleanup() {
    if let Some(paths) = NREPL_CLEANUP.get() {
        for p in paths {
            // SAFETY: `p` is a valid NUL-terminated path; unlink is async-signal-safe.
            unsafe { libc::unlink(p.as_ptr()) };
        }
    }
}

extern "C" fn nrepl_on_signal(sig: libc::c_int) {
    nrepl_cleanup();
    // SAFETY: _exit is async-signal-safe.
    unsafe { libc::_exit(128 + sig) };
}

extern "C" fn nrepl_at_exit() {
    nrepl_cleanup();
}

/// A message for stderr and exit status 2 (what `nrepl.cmdline/die` does).
fn nrepl_die(msg: &str) -> i32 {
    eprint!("{msg}");
    2
}

/// Where `--connect` / `--interactive` connect to.
struct NreplTarget {
    host: String,
    port: Option<u16>,
    socket: Option<String>,
    codec: mova_nrepl::Codec,
    tls: bool,
}

/// `-h` may be a URL the server advertises (`nrepl://h:p`, `nrepls://`,
/// `nrepl+edn://`, `nrepl+unix:/path`). Returns `None` for a plain host.
fn nrepl_parse_url(host: &str, o: &mova_nrepl::cmdline::Options) -> Option<Result<NreplTarget, String>> {
    use mova_nrepl::Codec;
    let (scheme, rest) = if let Some((s, r)) = host.split_once("://") {
        (s.to_ascii_lowercase(), r)
    } else if let Some((s, r)) = host.split_once(':') {
        // opaque form: nrepl+unix:/path
        if !s.ends_with("+unix") || !s.chars().all(|c| c.is_ascii_alphanumeric() || "+.-".contains(c)) {
            return None;
        }
        (s.to_ascii_lowercase(), r)
    } else {
        return None;
    };
    if !scheme.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    let conflict = |what: &str| Some(Err(format!("nREPL: --{what} can't be combined with the URL {host}.\n")));
    if o.socket.is_some() {
        return conflict("socket");
    }
    if o.port.is_some() {
        return conflict("port");
    }
    if o.transport.as_deref().is_some_and(|t| t != "nrepl.transport/bencode") {
        return Some(Err(format!(
            "nREPL: --transport can't be combined with the URL {host}; the transport comes from the scheme.\n"
        )));
    }
    let (base, unix, tls) = match scheme.strip_suffix("+unix") {
        Some(b) => (b.to_string(), true, false),
        None => match scheme.strip_suffix('s') {
            Some(b) if b == "nrepl" || b == "nrepl+edn" => (b.to_string(), false, true),
            _ => (scheme.clone(), false, false),
        },
    };
    let codec = match base.as_str() {
        "nrepl" => Codec::Bencode,
        "nrepl+edn" => Codec::Edn,
        "telnet" => return Some(Err("The built-in client does not support the tty transport. Consider using `nc` or `telnet`.\n".into())),
        "http" | "https" => {
            return Some(Err(format!("nREPL: connecting to {scheme} URLs requires the nrepl/drawbridge library on the classpath.\n")))
        }
        _ => return Some(Err(format!("nREPL: No matching clauses: {scheme}\n"))),
    };
    if unix {
        return Some(Ok(NreplTarget { host: String::new(), port: None, socket: Some(rest.to_string()), codec, tls: false }));
    }
    let hostport = rest.split('/').next().unwrap_or("");
    let (h, p) = match hostport.rsplit_once(':') {
        Some((h, p)) if !p.contains(']') => (h, p.parse::<u16>().ok()),
        _ => (hostport, None),
    };
    let h = h.trim_start_matches('[').trim_end_matches(']');
    if h.is_empty() {
        return Some(Err(format!("nREPL: Can't extract a host from the URL {host}.\n")));
    }
    Some(Ok(NreplTarget { host: h.to_string(), port: Some(p.filter(|p| *p > 0).unwrap_or(7888)), socket: None, codec, tls }))
}

#[cfg(feature = "tls")]
type NreplTls = std::sync::Arc<mova_nrepl::tls::TlsConfig>;
#[cfg(not(feature = "tls"))]
type NreplTls = ();

/// TLS contexts from `--tls-keys-file` / `--tls-keys-str`.
#[cfg(feature = "tls")]
fn nrepl_tls(o: &mova_nrepl::cmdline::Options) -> Result<Option<NreplTls>, String> {
    if o.tls_keys_file.is_none() && o.tls_keys_str.is_none() {
        return Ok(None);
    }
    mova_nrepl::tls::TlsConfig::load(o.tls_keys_file.as_deref(), o.tls_keys_str.as_deref()).map(|c| Some(std::sync::Arc::new(c)))
}

#[cfg(not(feature = "tls"))]
fn nrepl_tls(o: &mova_nrepl::cmdline::Options) -> Result<Option<NreplTls>, String> {
    if o.tls_keys_file.is_some() || o.tls_keys_str.is_some() {
        return Err("TLS support is not built in (rebuild with `cargo build --release --features tls`).".into());
    }
    Ok(None)
}

fn nrepl_intro() -> String {
    format!(
        "nREPL {}\nClojure {}\nmova {}\nInterrupt: Control+C\nExit:      Control+D or (exit) or (quit)",
        mova_nrepl::describe::NREPL_VERSION,
        mova_nrepl::describe::Versions::default().clojure.version_string,
        env!("CARGO_PKG_VERSION"),
    )
}

/// Runs the built-in REPL against `t` until end of input.
fn nrepl_repl(t: &NreplTarget, o: &mova_nrepl::cmdline::Options, tls: Option<&NreplTls>) -> i32 {
    let stream = if t.tls || (tls.is_some() && t.socket.is_none()) {
        #[cfg(feature = "tls")]
        {
            match tls {
                Some(c) => c.connect(&t.host, t.port.unwrap_or(7888)),
                None => return nrepl_die("nREPL: nrepls:// URLs require --tls-keys-file or --tls-keys-str.\n"),
            }
        }
        #[cfg(not(feature = "tls"))]
        {
            let _ = tls;
            return nrepl_die("nREPL: TLS support is not built in (rebuild with --features tls).\n");
        }
    } else {
        mova_nrepl::client::dial(&t.host, t.port, t.socket.as_deref())
    };
    let stream = match stream {
        Ok(s) => s,
        Err(e) => {
            eprintln!("nREPL: {e}");
            return 1;
        }
    };
    let opts = mova_nrepl::client::ReplOptions { codec: t.codec, color: o.color, intro: nrepl_intro() };
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut out = std::io::stdout().lock();
    match mova_nrepl::client::run_repl(stream, &opts, &mut input, &mut out) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("nREPL: {e}");
            1
        }
    }
}

fn nrepl_main(t0: std::time::Instant, module_paths: Option<Vec<PathBuf>>, argv: &[String]) -> i32 {
    use std::io::Write;
    let o = match mova_nrepl::cmdline::parse(argv) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("nREPL: {e}");
            return 1;
        }
    };
    // same order as `nrepl.cmdline/dispatch-commands`: help, version, connect, server
    if o.help {
        print!("{}", mova_nrepl::cmdline::HELP);
        return 0;
    }
    if o.version {
        println!("{}", mova_nrepl::describe::NREPL_VERSION);
        return 0;
    }
    let Some(codec) = mova_nrepl::Codec::from_symbol(o.transport_symbol()) else {
        return nrepl_die(&format!("nREPL transport: unable to resolve {}\n", o.transport_symbol()));
    };
    // `--middleware` / `--handler` run Mova-level code (the middleware lane); a custom REPL fn does not exist here.
    if o.repl_fn.is_some() {
        return nrepl_die("nREPL: --repl-fn is not supported by mova nrepl (the server is native; use --middleware or --handler).\n");
    }
    let tls = match nrepl_tls(&o) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    let port = match o.port_number() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let ack = match o.ack_number() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    if o.connect {
        let target = match o.host.as_deref().and_then(|h| nrepl_parse_url(h, &o)) {
            Some(Ok(t)) => t,
            Some(Err(e)) => return nrepl_die(&e),
            None => {
                if codec == mova_nrepl::Codec::Tty {
                    return nrepl_die("The built-in client does not support the tty transport. Consider using `nc` or `telnet`.\n");
                }
                if o.socket.is_none() && port.is_none() {
                    return nrepl_die("Must supply host/port, socket, or a URL.\n");
                }
                NreplTarget { host: o.host.clone().unwrap_or_else(|| "127.0.0.1".into()), port, socket: o.socket.clone(), codec, tls: false }
            }
        };
        return nrepl_repl(&target, &o, tls.as_ref());
    }
    if o.socket.is_some() && (port.is_some() || o.bind.is_some() || tls.is_some()) {
        return nrepl_die("Cannot listen on both port and filesystem socket\n");
    }
    if o.interactive && codec == mova_nrepl::Codec::Tty {
        return nrepl_die("The built-in client does not support the tty transport. Consider using `nc` or `telnet`.\n");
    }
    let bind = o.bind.clone().unwrap_or_else(|| "127.0.0.1".into());
    let errors = o
        .errors
        .clone()
        .or_else(|| std::env::var("MOVA_NREPL_ERRORS").ok())
        .and_then(|v| mova::nrepl::ErrorMode::parse(&v))
        .unwrap_or_default();
    let verbose = o.verbose;
    let us = move |t: std::time::Instant| t.duration_since(t0).as_secs_f64() * 1e6;
    if !o.no_core_image {
        mova::core_image::enable_cache();
    }
    // The backend (see `mova::nrepl`): session threads, ephemeral pool, boot latch.
    let backend = mova::nrepl::MovaBackend::new(mova::nrepl::Config {
        stack_size: std::env::var("MOVA_NREPL_STACK_MB").ok().and_then(|v| v.parse::<usize>().ok()).map(|m| m << 20).unwrap_or(EVAL_STACK_SIZE),
        max_depth: EVAL_MAX_CALL_DEPTH,
        module_paths: module_paths.unwrap_or_else(default_module_paths),
        errors,
        middleware: o.middleware.clone(),
        handler: o.handler.clone(),
        fatal_hook: Some(nrepl_cleanup),
    });
    // The boot thread builds the interpreter (from the core image when valid);
    // evals that arrive before it is ready wait for it.
    let boot = {
        let backend = backend.clone();
        move || {
            backend.boot_thread(move || {
                if verbose {
                    use mova::core_image as ci;
                    use std::sync::atomic::Ordering::Relaxed;
                    eprintln!(
                        "nrepl: main->interpreter ready {:.1} us (outcome {} [1=restored 2=miss+saved 3=failed 0=off], natives {} us, restore {} us, save-encode {} us)",
                        us(std::time::Instant::now()),
                        ci::OUTCOME.load(Relaxed),
                        ci::NATIVES_US.load(Relaxed),
                        ci::RESTORE_US.load(Relaxed),
                        ci::SAVE_ENCODE_US.load(Relaxed)
                    );
                }
            })
        }
    };
    let mut spawn_boot = Some(boot);
    let mut spawned = None;
    if o.boot_first {
        spawned = Some((spawn_boot.take().unwrap())());
    }

    // 1. bind + listen. Nothing of the interpreter is touched before the banner.
    let endpoint = match &o.socket {
        Some(path) => mova_nrepl::Endpoint::Unix(path.into()),
        None => mova_nrepl::Endpoint::Tcp { host: bind.clone(), port: port.unwrap_or(0) },
    };
    let listeners = match mova_nrepl::Listeners::bind(&endpoint) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("nrepl: bind failed: {e}");
            return 1;
        }
    };
    let port_bound = listeners.port();
    let t_listen = std::time::Instant::now();
    // --ack: like the JVM, tell the other server our port before the banner
    if let (Some(ack_port), Some(my_port)) = (ack, listeners.port()) {
        if let Err(e) = mova_nrepl::ack::send_ack(my_port, ack_port, codec) {
            eprintln!("nREPL: could not ack port {my_port} to the server on port {ack_port}: {e}");
            return 1;
        }
    }

    // cleanup of .nrepl-port (and the socket) on normal exit, SIGINT, SIGTERM
    let mut cleanup = Vec::new();
    if let Ok(c) = std::ffi::CString::new(".nrepl-port") {
        cleanup.push(c);
    }
    if let Some(Ok(c)) = o.socket.as_ref().map(|p| std::ffi::CString::new(p.as_str())) {
        cleanup.push(c);
    }
    let _ = NREPL_CLEANUP.set(cleanup);
    // SAFETY: registering plain C handlers; they only call async-signal-safe functions.
    unsafe {
        let handler = nrepl_on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
        libc::signal(libc::SIGINT, handler);
        libc::signal(libc::SIGTERM, handler);
        libc::atexit(nrepl_at_exit);
    }

    // 2. banner (flushed), then the port file: same order as the JVM nREPL
    {
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{}", listeners.banner_with(codec.uri_scheme(), tls.is_some()));
        let _ = out.flush();
    }
    let t_banner = std::time::Instant::now();
    // The JVM writes the port (an empty file for a socket server).
    let _ = std::fs::write(".nrepl-port", listeners.port().map(|p| p.to_string()).unwrap_or_default());
    let t_pf = std::time::Instant::now();
    if verbose {
        eprintln!(
            "nrepl: main->listening {:.1} us, ->banner flushed {:.1} us, ->port file {:.1} us",
            us(t_listen),
            us(t_banner),
            us(t_pf)
        );
    }

    // 3. boot the interpreter in parallel; the IO loop starts at once.
    if let Some(f) = spawn_boot.take() {
        spawned = Some(f());
    }
    if let Some(Err(e)) = spawned {
        eprintln!("nrepl: couldn't start the evaluator thread: {e}");
        nrepl_cleanup();
        return 1;
    }
    let server = match mova_nrepl::Server::new(listeners, backend, verbose) {
        Ok(s) => s.with_codec(codec),
        Err(e) => {
            eprintln!("nrepl: {e}");
            nrepl_cleanup();
            return 1;
        }
    };
    #[cfg(feature = "tls")]
    let server = match &tls {
        Some(c) => server.with_tls(c.clone()),
        None => server,
    };
    let code = if o.interactive {
        // the server runs on its own thread; this one is the REPL (like the JVM)
        let target = NreplTarget {
            host: if bind == "0.0.0.0" || bind == "::" { "127.0.0.1".into() } else { bind.clone() },
            port: port_bound,
            socket: o.socket.clone(),
            codec,
            tls: tls.is_some(),
        };
        let h = std::thread::Builder::new().name("mova-nrepl-io".into()).spawn(move || {
            if let Err(e) = server.run() {
                eprintln!("nrepl: {e}");
            }
        });
        if let Err(e) = h {
            eprintln!("nrepl: couldn't start the IO thread: {e}");
            nrepl_cleanup();
            return 1;
        }
        nrepl_repl(&target, &o, tls.as_ref())
    } else {
        match server.run() {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("nrepl: {e}");
                1
            }
        }
    };
    nrepl_cleanup();
    code
}
