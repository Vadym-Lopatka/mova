//! nREPL phase P6: the middleware lane (design 5.7). Plain-Clojure middleware
//! from `crates/mova-nrepl/oracle/middleware` (the same files the JVM runs in
//! `oracle/middleware-check.py`) loaded with `--middleware`, through the real
//! server over a TCP socket.
//!
//! Tests share one process; each takes `SERIAL`.

use mova::nrepl::{Config, MovaBackend};
use mova_nrepl::bencode::{decode, encode, Value};
use mova_nrepl::{Endpoint, Listeners, Server, ServerHandle};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

static SERIAL: Mutex<()> = Mutex::new(());

struct Fixture {
    port: u16,
    backend: Arc<MovaBackend>,
    handle: ServerHandle,
    join: Option<std::thread::JoinHandle<()>>,
    _guard: MutexGuard<'static, ()>,
}

impl Fixture {
    /// A server whose `--middleware` is `mw` (fixtures of `oracle/middleware`, plain Clojure).
    fn start(mw: &[&str]) -> Fixture {
        let guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let l = Listeners::bind(&Endpoint::Tcp { host: "127.0.0.1".into(), port: 0 }).unwrap();
        let port = l.port().unwrap();
        let backend = MovaBackend::new(Config {
            errors: mova::nrepl::ErrorMode::Jvm,
            module_paths: vec![fixtures()],
            middleware: mw.iter().map(|s| s.to_string()).collect(),
            ..Config::default()
        });
        // Boot after the server exists, like the binary: the first eval waits for it.
        let server = Server::new(l, backend.clone(), false).unwrap();
        let handle = server.handle();
        let join = std::thread::spawn(move || server.run().unwrap());
        backend.boot_thread(|| {}).unwrap();
        Fixture { port, backend, handle, join: Some(join), _guard: guard }
    }

    fn client(&self) -> Client {
        let s = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        s.set_nodelay(true).unwrap();
        Client { s, buf: Vec::new(), next_id: 1 }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.handle.shutdown();
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

struct Client {
    s: TcpStream,
    buf: Vec<u8>,
    next_id: u32,
}

/// One reply, easier to match on.
#[derive(Debug, Clone)]
struct Msg(Value);

impl Msg {
    fn s(&self, k: &str) -> Option<String> {
        self.0.get(k).and_then(|v| v.as_str()).map(|s| s.to_string())
    }
    fn status(&self) -> Vec<String> {
        match self.0.get("status") {
            Some(Value::List(l)) => l.iter().filter_map(|v| v.as_str()).map(|s| s.to_string()).collect(),
            _ => vec![],
        }
    }
    fn has_status(&self, st: &str) -> bool {
        self.status().iter().any(|s| s == st)
    }
}

impl Client {
    fn send(&mut self, items: &[(&str, Value)]) -> String {
        let id = self.next_id.to_string();
        self.next_id += 1;
        let mut d: Vec<(&str, Value)> = items.to_vec();
        d.push(("id", Value::str(&id)));
        let m = Value::Dict(d.into_iter().map(|(k, v)| (k.as_bytes().to_vec(), v)).collect());
        let mut out = Vec::new();
        encode(&m, &mut out);
        self.s.write_all(&out).unwrap();
        id
    }

    fn recv(&mut self) -> Option<Msg> {
        loop {
            if let Ok(Some((v, used))) = decode(&self.buf) {
                self.buf.drain(..used);
                return Some(Msg(v));
            }
            let mut tmp = [0u8; 65536];
            match self.s.read(&mut tmp) {
                Ok(0) => return None,
                Ok(n) => self.buf.extend_from_slice(&tmp[..n]),
                Err(e) => panic!("read: {e} (waiting for a reply)"),
            }
        }
    }

    /// Replies for `id` up to and including the one with `done`.
    fn until_done(&mut self, id: &str) -> Vec<Msg> {
        let mut out = Vec::new();
        loop {
            let m = self.recv().expect("connection closed");
            let mine = m.s("id").as_deref() == Some(id);
            if mine {
                let done = m.has_status("done");
                out.push(m);
                if done {
                    return out;
                }
            }
        }
    }

    fn eval_in(&mut self, session: Option<&str>, code: &str, extra: &[(&str, Value)]) -> Vec<Msg> {
        let mut items = vec![("op", Value::str("eval")), ("code", Value::str(code))];
        if let Some(s) = session {
            items.push(("session", Value::str(s)));
        }
        items.extend(extra.iter().cloned());
        let id = self.send(&items);
        self.until_done(&id)
    }

    fn eval(&mut self, session: &str, code: &str) -> Vec<Msg> {
        self.eval_in(Some(session), code, &[])
    }

    fn clone_session(&mut self, from: Option<&str>) -> String {
        let mut items = vec![("op", Value::str("clone"))];
        if let Some(f) = from {
            items.push(("session", Value::str(f)));
        }
        let id = self.send(&items);
        self.until_done(&id)[0].s("new-session").expect("new-session")
    }

    fn close(&mut self, session: &str) {
        let id = self.send(&[("op", Value::str("close")), ("session", Value::str(session))]);
        self.until_done(&id);
    }

    /// The values of the `value` messages.
    fn values(&mut self, session: &str, code: &str) -> Vec<String> {
        self.eval(session, code).iter().filter_map(|m| m.s("value")).collect()
    }

    fn value(&mut self, session: &str, code: &str) -> String {
        let v = self.values(session, code);
        assert_eq!(v.len(), 1, "one value for {code}: {v:?}");
        v.into_iter().next().unwrap()
    }
}



fn fixtures() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("crates/mova-nrepl/oracle/middleware")
}

fn call(c: &mut Client, items: &[(&str, Value)]) -> Vec<Msg> {
    let id = c.send(items);
    c.until_done(&id)
}

fn strings(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::List(l)) => l.iter().filter_map(|x| x.as_str()).map(|s| s.to_string()).collect(),
        _ => vec![],
    }
}

fn describe(c: &mut Client, verbose: bool) -> Msg {
    let mut items = vec![("op", Value::str("describe"))];
    if verbose {
        items.push(("verbose?", Value::str("1")));
    }
    call(c, &items).remove(0)
}

fn ops(m: &Msg) -> Vec<String> {
    match m.0.get("ops") {
        Some(Value::Dict(d)) => d.keys().map(|k| String::from_utf8_lossy(k).into_owned()).collect(),
        _ => vec![],
    }
}

#[test]
fn an_op_added_by_middleware_is_answered_by_it() {
    let f = Fixture::start(&["fx.add-op/wrap-add-op"]);
    let mut c = f.client();
    let r = call(&mut c, &[("op", Value::str("fx/add")), ("a", Value::str("40")), ("b", Value::str("2"))]);
    assert_eq!(r.len(), 1, "{r:?}");
    assert_eq!(r[0].s("value").as_deref(), Some("42"));
    assert_eq!(r[0].status(), vec!["done"]);
    // describe lists it, with the middleware, short and verbose
    let d = describe(&mut c, false);
    assert!(ops(&d).contains(&"fx/add".to_string()), "{:?}", ops(&d));
    assert_eq!(ops(&d).len(), 12);
    let mw = strings(d.0.get("middleware"));
    assert_eq!(mw.first().map(|s| s.as_str()), Some("#'nrepl.middleware/wrap-describe"));
    assert_eq!(mw.last().map(|s| s.as_str()), Some("#'fx.add-op/wrap-add-op"));
    assert_eq!(mw.len(), 11);
    let d = describe(&mut c, true);
    let Some(Value::Dict(o)) = d.0.get("ops") else { panic!() };
    let add = &o[&b"fx/add".to_vec()];
    assert_eq!(add.get("doc").and_then(|x| x.as_str()), Some("Adds two integers."));
    assert!(add.get("requires").and_then(|r| r.get("a")).is_some());
    assert!(o[&b"eval".to_vec()].get("doc").is_some(), "native ops keep their docs");
    // the other ops still work
    let s = c.clone_session(None);
    assert_eq!(c.value(&s, "(+ 1 2)"), "3");
    // a session that does not exist is answered by the router, before any middleware
    let r = call(&mut c, &[("op", Value::str("eval")), ("code", Value::str("1")), ("session", Value::str("nope"))]);
    assert_eq!(r[0].status(), vec!["done", "unknown-session", "error"]);
    // an unknown op reaches the end of the stack
    let r = call(&mut c, &[("op", Value::str("nosuchop"))]);
    assert_eq!(r[0].status(), vec!["done", "unknown-op", "error"]);
    assert_eq!(r[0].s("op").as_deref(), Some("nosuchop"));
}

#[test]
fn a_wrapped_transport_adds_a_key_to_every_eval_reply() {
    let f = Fixture::start(&["fx.tag-eval/wrap-tag-eval"]);
    let mut c = f.client();
    let s = c.clone_session(None);
    let r = c.eval(&s, "(do (println \"hi\") (print \"x\") 7)");
    // out, out, value, done: all tagged
    assert_eq!(r.len(), 4, "{r:?}");
    assert!(r.iter().all(|m| m.s("tag").as_deref() == Some("fx")), "{r:?}");
    assert_eq!(r[0].s("out").as_deref(), Some("hi\n"));
    assert_eq!(r[2].s("value").as_deref(), Some("7"));
    // errors too
    let r = c.eval(&s, "(/ 1 0)");
    assert!(r.len() >= 3 && r.iter().all(|m| m.s("tag").as_deref() == Some("fx")), "{r:?}");
    assert!(r.iter().any(|m| m.has_status("eval-error")));
    // other ops are not tagged
    let d = describe(&mut c, false);
    assert!(d.s("tag").is_none());
    let r = call(&mut c, &[("op", Value::str("ls-sessions"))]);
    assert!(r[0].s("tag").is_none());
    // the session keeps its state across requests that went through the lane
    c.eval(&s, "(def zz 41)");
    assert_eq!(c.value(&s, "(inc zz)"), "42");
}

#[test]
fn descriptors_order_the_stack() {
    for mw in [&["fx.ordered/wrap-audit", "fx.ordered/wrap-log"], &["fx.ordered/wrap-log", "fx.ordered/wrap-audit"]] {
        let f = Fixture::start(mw);
        let mut c = f.client();
        let r = c.eval_in(None, "(+ 1 1)", &[]);
        assert!(!r.is_empty());
        // the order the JVM gives for these descriptors (middleware-check.py runs the same on both)
        for m in &r {
            assert_eq!(strings(m.0.get("trail")), ["audit", "log"], "{r:?}");
        }
        let names = strings(describe(&mut c, false).0.get("middleware"));
        let at = |n: &str| names.iter().position(|x| x == n).unwrap_or_else(|| panic!("{n} in {names:?}"));
        assert!(at("#'fx.ordered/wrap-audit") < at("#'fx.ordered/wrap-log"), "{names:?}");
        assert!(at("#'fx.ordered/wrap-log") < at("#'nrepl.middleware.session/session"), "{names:?}");
    }
}

#[test]
fn middleware_can_change_the_message() {
    let f = Fixture::start(&["fx.rewrite/wrap-rewrite", "fx.identity/wrap-identity"]);
    let mut c = f.client();
    let s = c.clone_session(None);
    assert_eq!(c.value(&s, "(rewrite-me)"), "42");
    assert_eq!(c.value(&s, "(+ 1 2)"), "3");
    // ephemeral request (no session) too
    let r = c.eval_in(None, "(rewrite-me)", &[]);
    assert_eq!(r.iter().filter_map(|m| m.s("value")).collect::<Vec<_>>(), ["42"]);
    // ... and the id and session of the request come back on every reply
    assert!(r.iter().all(|m| m.s("id").is_some() && m.s("session").is_some()));
}

#[test]
fn interrupt_and_stdin_go_through_the_lane() {
    let f = Fixture::start(&["fx.identity/wrap-identity"]);
    let mut c = f.client();
    let s = c.clone_session(None);
    let id = c.send(&[("op", Value::str("eval")), ("session", Value::str(&s)), ("code", Value::str("(Thread/sleep 30000)"))]);
    std::thread::sleep(Duration::from_millis(300));
    let started = Instant::now();
    let iid = c.send(&[("op", Value::str("interrupt")), ("session", Value::str(&s)), ("interrupt-id", Value::str(&id))]);
    let (mut got_interrupted, mut got_interrupt_done) = (false, false);
    while !(got_interrupted && got_interrupt_done) {
        let m = c.recv().expect("closed");
        if m.s("id").as_deref() == Some(&id) && m.has_status("interrupted") {
            got_interrupted = true;
        }
        if m.s("id").as_deref() == Some(&iid) && m.has_status("done") {
            got_interrupt_done = true;
        }
    }
    assert!(started.elapsed() < Duration::from_secs(5));
    // the session still works, and `*in*` reads what `stdin` sent
    assert_eq!(c.value(&s, "(+ 2 3)"), "5");
    let sid = c.send(&[("op", Value::str("stdin")), ("session", Value::str(&s)), ("stdin", Value::str("hello\n"))]);
    c.until_done(&sid);
    assert_eq!(c.value(&s, "(read-line)"), "\"hello\"");
}

#[test]
fn concurrent_requests_do_not_block_each_other() {
    let f = Fixture::start(&["fx.identity/wrap-identity"]);
    let mut a = f.client();
    let mut b = f.client();
    let sa = a.clone_session(None);
    let _slow = a.send(&[("op", Value::str("eval")), ("session", Value::str(&sa)), ("code", Value::str("(Thread/sleep 3000)"))]);
    std::thread::sleep(Duration::from_millis(200));
    let t = Instant::now();
    let r = b.eval_in(None, "(+ 1 1)", &[]);
    assert_eq!(r.iter().filter_map(|m| m.s("value")).collect::<Vec<_>>(), ["2"]);
    assert!(t.elapsed() < Duration::from_secs(2), "took {:?}", t.elapsed());
    assert!(f.backend.lane_threads() >= 2, "a busy worker makes room for another");
}

#[test]
fn no_middleware_means_no_lane() {
    let f = Fixture::start(&[]);
    let mut c = f.client();
    let s = c.clone_session(None);
    assert_eq!(c.value(&s, "(+ 1 2)"), "3");
    assert_eq!(ops(&describe(&mut c, false)).len(), 11);
    assert_eq!(f.backend.lane_threads(), 0);
}

fn mova_nrepl(args: &[&str]) -> std::process::Output {
    let dir = std::env::temp_dir().join(format!("mova-mw-test-{}-{}", std::process::id(), args.len()));
    std::fs::create_dir_all(&dir).unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_mova"))
        .arg("nrepl")
        .args(args)
        .current_dir(&dir)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    out
}

#[test]
fn a_middleware_that_does_not_load_stops_the_server() {
    let out = mova_nrepl(&["-p", "0", "-m", "[no.such.ns/wrap-x]"]);
    assert_eq!(out.status.code(), Some(1), "{}", String::from_utf8_lossy(&out.stderr));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("no.such.ns"), "{err}");
}

#[test]
fn repl_fn_is_refused() {
    let out = mova_nrepl(&["-p", "0", "-f", "a.b/c"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("--repl-fn"));
}
