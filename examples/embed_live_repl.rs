//! A REPL on a Unix socket into a *running app*: `AppState` holds live,
//! mutating process state (an uptime counter ticking on a background
//! thread), and every socket connection gets its own snapshot of a
//! template [`Engine`] with a zero-copy [`host::wrap_struct`] view of that
//! SAME state def'd in as `app` -- `(:queue-depth app)` reads the live
//! value with no serialization step. `*1`/`*2`/`*3` work per-connection,
//! exactly like `mova`'s own CLI REPL (`src/main.rs`).
//!
//! Run: `cargo run --release --example embed_live_repl -- --demo`
//! (interactive mode, the default with no `--demo`, listens forever --
//! connect with `nc -U <printed path>` and try `(:uptime-secs app)`).

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use mova::embed::host::{wrap_struct, Shape, ShapeBuilder};
use mova::embed::{Engine, Profile, Value};

/// The app's live state. Plain atomics (no `Mutex`) so the background
/// ticker and every REPL connection can read/update it concurrently.
struct AppState {
    uptime_secs: AtomicI64,
    connected_clients: AtomicI64,
    queue_depth: AtomicI64,
}

/// Registers `AppState`'s script-visible fields ONCE. Once
/// `#[derive(MovaStruct)]` lands (DESIGN-hoststruct-derive.md §5.4), this
/// whole function shrinks to a `#[derive(MovaStruct)]` on the struct
/// itself.
fn app_state_shape() -> Shape<AppState> {
    ShapeBuilder::<AppState>::new("AppState")
        .field("uptime-secs", |s| Value::from(s.uptime_secs.load(Ordering::Relaxed)))
        .field("connected-clients", |s| Value::from(s.connected_clients.load(Ordering::Relaxed)))
        .field("queue-depth", |s| Value::from(s.queue_depth.load(Ordering::Relaxed)))
        .field("version", |_| Value::from("0.6.0"))
        .build()
}

/// Serves one connection: a fresh snapshot, the live state wrapped in as
/// `app`, and a line-oriented `eval -> print result` loop with `*1`/`*2`/
/// `*3` history, same as `src/main.rs`'s `handle_input`/`record_result`
/// minus the line editor.
fn handle_connection(stream: UnixStream, template: &Engine, state: &Arc<AppState>, shape: &Shape<AppState>) {
    state.connected_clients.fetch_add(1, Ordering::Relaxed);
    let mut engine = template.snapshot();
    engine.def("app", wrap_struct(Arc::clone(state), shape));
    for var in ["*1", "*2", "*3"] {
        engine.def(var, Value::from(()));
    }

    let reader = BufReader::new(stream.try_clone().expect("clone socket for reading"));
    let mut writer = stream;
    for line in reader.lines() {
        let Ok(line) = line else { break };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line == "exit" {
            break;
        }
        match engine.eval_named("repl", line).and_then(|v| engine.realize(&v)) {
            Ok(v) => {
                let _ = writeln!(writer, "{v}");
                let star1 = engine.get("*1").unwrap_or_else(|| Value::from(()));
                let star2 = engine.get("*2").unwrap_or_else(|| Value::from(()));
                engine.def("*3", star2);
                engine.def("*2", star1);
                engine.def("*1", v);
            }
            Err(e) => {
                let _ = writeln!(writer, "ERR: {e}");
            }
        }
    }
    state.connected_clients.fetch_sub(1, Ordering::Relaxed);
}

fn socket_path() -> String {
    format!("/tmp/mova-embed-live-repl-{}.sock", std::process::id())
}

/// One request/reply pair a demo client sends over the socket.
fn run_demo_client(path: &str) {
    let mut client = UnixStream::connect(path).expect("connect demo client");
    let requests = ["(:queue-depth app)", "(:connected-clients app)", "(+ *1 1)", "(:version app)"];
    for req in requests {
        writeln!(client, "{req}").expect("send request");
    }
    client.shutdown(std::net::Shutdown::Write).ok();
    let mut reply = String::new();
    std::io::Read::read_to_string(&mut client, &mut reply).expect("read replies");
    println!("-- client sent {} forms, server replied: --", requests.len());
    for (req, resp) in requests.iter().zip(reply.lines()) {
        println!("  {req:<26} => {resp}");
    }
}

fn main() {
    let demo = std::env::args().any(|a| a == "--demo") || std::env::var("MOVA_DEMO").is_ok();

    let state = Arc::new(AppState {
        uptime_secs: AtomicI64::new(0),
        connected_clients: AtomicI64::new(0),
        queue_depth: AtomicI64::new(if demo { 42 } else { 0 }),
    });
    let shape = app_state_shape();
    let template = Engine::builder().profile(Profile::Pure).build();

    let stop = Arc::new(AtomicBool::new(false));
    let tick = if demo { Duration::from_millis(50) } else { Duration::from_secs(1) };
    let ticker = std::thread::spawn({
        let (state, stop) = (Arc::clone(&state), Arc::clone(&stop));
        move || {
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(tick);
                state.uptime_secs.fetch_add(1, Ordering::Relaxed);
            }
        }
    });

    let path = socket_path();
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).expect("bind socket");

    if !demo {
        println!("listening on {path} -- connect with `nc -U {path}`, try `(:uptime-secs app)`");
        for conn in listener.incoming() {
            let Ok(stream) = conn else { continue };
            let (template, state, shape) = (template.snapshot(), Arc::clone(&state), shape.clone());
            std::thread::spawn(move || handle_connection(stream, &template, &state, &shape));
        }
        return; // unreachable in practice: `incoming()` only stops on a fatal accept error
    }

    println!("-- background app running, REPL listening on {path} --");
    let server = std::thread::spawn({
        let (template, state, shape) = (template.snapshot(), Arc::clone(&state), shape.clone());
        // One connection is enough to prove the story end to end.
        move || {
            if let Ok((stream, _)) = listener.accept() {
                handle_connection(stream, &template, &state, &shape);
            }
        }
    });

    std::thread::sleep(Duration::from_millis(100)); // let the ticker tick a bit before connecting
    run_demo_client(&path);

    server.join().expect("server thread should finish after one connection");
    stop.store(true, Ordering::Relaxed);
    ticker.join().expect("ticker thread should stop cleanly");
    let _ = std::fs::remove_file(&path);
    println!("-- clean shutdown; final uptime_secs = {} --", state.uptime_secs.load(Ordering::Relaxed));
}
