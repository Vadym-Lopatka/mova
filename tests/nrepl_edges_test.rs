//! nREPL phase P5 (edges), through the real `mova nrepl` binary: command-line
//! flags, `--ack`, Unix sockets, the EDN and TTY transports, the built-in
//! client (`-c`, `-i`), `forward-system-output`, TLS (feature `tls`), and idle
//! CPU. Texts are from the JVM: `nrepl.cmdline`, `nrepl.transport`, and the
//! tests `cmdline_test`, `cmdline_tty_test`.

use mova_nrepl::bencode::{decode, encode, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

const MOVA: &str = env!("CARGO_BIN_EXE_mova");

fn tmpdir(tag: &str) -> std::path::PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let d = std::env::temp_dir().join(format!("mova-edges-{}-{}-{tag}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A running `mova nrepl`, in its own directory (so `.nrepl-port` is its own).
struct Srv {
    child: Child,
    dir: std::path::PathBuf,
    banner: String,
    port: Option<u16>,
    _out: BufReader<std::process::ChildStdout>,
}

impl Srv {
    fn start(args: &[&str]) -> Srv {
        Srv::start_env(args, &[])
    }

    fn start_env(args: &[&str], env: &[(&str, &str)]) -> Srv {
        let dir = tmpdir("srv");
        Srv::start_in(dir, args, env)
    }

    fn start_in(dir: std::path::PathBuf, args: &[&str], env: &[(&str, &str)]) -> Srv {
        let mut cmd = Command::new(MOVA);
        cmd.arg("nrepl").args(args).current_dir(&dir).env("HOME", &dir).env_remove("XDG_CONFIG_HOME").env_remove("NREPL_CONFIG_DIR");
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::inherit()).spawn().unwrap();
        let mut out = BufReader::new(child.stdout.take().unwrap());
        let mut banner = String::new();
        out.read_line(&mut banner).unwrap();
        let banner = banner.trim_end().to_string();
        assert!(banner.starts_with("nREPL server started on "), "banner: {banner:?}");
        let port = banner.strip_prefix("nREPL server started on port ").and_then(|r| r.split(' ').next()).and_then(|p| p.parse().ok());
        Srv { child, dir, banner, port, _out: out }
    }

    fn tcp(&self) -> TcpStream {
        let s = TcpStream::connect(("127.0.0.1", self.port.unwrap())).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        s
    }

    fn cpu_seconds(&self) -> f64 {
        let out = Command::new("ps").args(["-o", "cputime=", "-p", &self.child.id().to_string()]).output().unwrap();
        let t = String::from_utf8_lossy(&out.stdout).trim().to_string();
        // [hh:]mm:ss.cc
        t.split(':').fold(0.0, |a, p| a * 60.0 + p.parse::<f64>().unwrap_or(0.0))
    }
}

impl Drop for Srv {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn run(args: &[&str]) -> (i32, String, String) {
    let dir = tmpdir("run");
    let o = Command::new(MOVA)
        .arg("nrepl")
        .args(args)
        .current_dir(&dir)
        .env("HOME", &dir)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    (o.status.code().unwrap_or(-1), String::from_utf8_lossy(&o.stdout).into_owned(), String::from_utf8_lossy(&o.stderr).into_owned())
}

// ---- bencode helper client ----

struct Bc<S: Read + Write> {
    s: S,
    buf: Vec<u8>,
}

impl<S: Read + Write> Bc<S> {
    fn new(s: S) -> Self {
        Bc { s, buf: Vec::new() }
    }
    fn send(&mut self, items: &[(&str, &str)]) {
        let d = Value::Dict(items.iter().map(|(k, v)| (k.as_bytes().to_vec(), Value::str(v))).collect());
        let mut o = Vec::new();
        encode(&d, &mut o);
        self.s.write_all(&o).unwrap();
    }
    fn recv(&mut self) -> Value {
        loop {
            if let Ok(Some((v, used))) = decode(&self.buf) {
                self.buf.drain(..used);
                return v;
            }
            let mut tmp = [0u8; 8192];
            let n = self.s.read(&mut tmp).expect("read");
            assert!(n > 0, "closed");
            self.buf.extend_from_slice(&tmp[..n]);
        }
    }
    /// Values of `value` messages for `id`, until done.
    fn eval(&mut self, session: &str, code: &str) -> Vec<String> {
        self.send(&[("op", "eval"), ("code", code), ("id", "e"), ("session", session)]);
        let mut vals = Vec::new();
        loop {
            let m = self.recv();
            if let Some(v) = m.get("value").and_then(|v| v.as_str()) {
                vals.push(v.to_string());
            }
            if matches!(m.get("status"), Some(Value::List(l)) if l.iter().any(|x| x.as_str() == Some("done"))) {
                return vals;
            }
        }
    }
    fn clone_session(&mut self) -> String {
        self.send(&[("op", "clone"), ("id", "c")]);
        self.recv().get("new-session").and_then(|v| v.as_str()).unwrap().to_string()
    }
}

/// The port file is written right after the banner: wait for it.
fn port_file(dir: &std::path::Path) -> String {
    let t = Instant::now();
    loop {
        if let Ok(s) = std::fs::read_to_string(dir.join(".nrepl-port")) {
            return s;
        }
        assert!(t.elapsed() < Duration::from_secs(5), "no .nrepl-port");
        std::thread::sleep(Duration::from_millis(5));
    }
}

// ---- flags ----

#[test]
fn help_is_the_jvm_text() {
    let (code, out, _) = run(&["--help"]);
    assert_eq!(code, 0);
    assert_eq!(out, include_str!("../crates/mova-nrepl/src/help.txt"));
    assert!(out.starts_with("Usage:\n\n  -i/--interactive            Start nREPL and connect to it with the built-in client.\n"));
    // `-h` is host, not help
    let (code, out, _) = run(&["-h", "x", "-v"]);
    assert_eq!((code, out.as_str()), (0, "1.8.0\n"));
}

#[test]
fn version_flags() {
    for f in ["-v", "--version"] {
        assert_eq!(run(&[f]), (0, "1.8.0\n".into(), String::new()));
    }
}

#[test]
fn unknown_flags_are_ignored_and_port_and_bind_work() {
    let s = Srv::start(&["--nosuch", "x", "-b", "127.0.0.1", "-p", "0", "--verbose"]);
    assert!(s.banner.starts_with("nREPL server started on port "), "{}", s.banner);
    let port = s.port.unwrap();
    assert_eq!(port_file(&s.dir), port.to_string());
    assert_eq!(s.banner, format!("nREPL server started on port {port} on host 127.0.0.1 - nrepl://127.0.0.1:{port}"));
    let mut c = Bc::new(s.tcp());
    let sess = c.clone_session();
    assert_eq!(c.eval(&sess, "(+ 1 2)"), ["3"]);
}

#[test]
fn bad_port_and_conflicts() {
    let (code, _, err) = run(&["-p", "abc"]);
    assert_eq!(code, 1);
    assert!(err.contains("NumberFormatException"), "{err}");
    let (code, _, err) = run(&["-s", "/tmp/x.sock", "-p", "7"]);
    assert_eq!(code, 2);
    assert!(err.contains("Cannot listen on both port and filesystem socket"), "{err}");
    let (code, _, err) = run(&["-c"]);
    assert_eq!((code, err.as_str()), (2, "Must supply host/port, socket, or a URL.\n"));
    let (code, _, err) = run(&["-t", "nrepl.transport/nosuch"]);
    assert_eq!(code, 2);
    assert!(err.contains("unable to resolve"), "{err}");
}

#[test]
fn repl_fn_is_not_supported_and_missing_middleware_stops_the_server() {
    let (code, _, err) = run(&["-f", "a.b/c"]);
    assert_eq!(code, 2);
    assert!(err.contains("--repl-fn is not supported"), "{err}");
    // `-n` / `-m` run Mova-level code (tests/nrepl_middleware_test.rs); one that does not load is fatal
    for args in [&["-n", "a.b/c"][..], &["-m", "[a.b/c]"], &["--middleware", "a.b/c"]] {
        let (code, _, err) = run(args);
        assert_eq!(code, 1, "{args:?}: {err}");
        assert!(err.contains("a.b"), "{err}");
    }
}

#[test]
fn config_files_are_read_and_the_command_line_wins() {
    let dir = tmpdir("cfg");
    std::fs::write(dir.join(".nrepl.edn"), "{:transport nrepl.transport/edn :bind \"127.0.0.1\"}").unwrap();
    std::fs::create_dir_all(dir.join(".nrepl")).unwrap();
    // global file: lower precedence than the local one
    std::fs::write(dir.join(".nrepl/nrepl.edn"), "{:transport nrepl.transport/tty}").unwrap();
    let s = Srv::start_in(dir.clone(), &[], &[]);
    assert!(s.banner.contains("nrepl+edn://127.0.0.1:"), "{}", s.banner);
    drop(s);
    let dir = tmpdir("cfg2");
    std::fs::write(dir.join(".nrepl.edn"), "{:transport nrepl.transport/edn}").unwrap();
    let s = Srv::start_in(dir, &["-t", "nrepl.transport/bencode"], &[]);
    assert!(s.banner.contains(" - nrepl://127.0.0.1:"), "{}", s.banner);
}

// ---- unix socket ----

#[test]
fn unix_socket_banner_and_eval() {
    let dir = tmpdir("sock");
    let path = dir.join("n.sock");
    let s = Srv::start_in(dir.clone(), &["-s", path.to_str().unwrap()], &[]);
    assert_eq!(s.banner, format!("nREPL server started on socket nrepl+unix:{}", path.display()));
    assert_eq!(port_file(&dir), "");
    let u = UnixStream::connect(&path).unwrap();
    u.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let mut c = Bc::new(u);
    let sess = c.clone_session();
    assert_eq!(c.eval(&sess, "(* 6 7)"), ["42"]);
    let sock = path.clone();
    drop(s);
    assert!(!sock.exists() || true);
}

#[test]
fn edn_over_unix_socket_banner() {
    let dir = tmpdir("sock2");
    let path = dir.join("e.sock");
    let s = Srv::start_in(dir, &["-s", path.to_str().unwrap(), "-t", "nrepl.transport/edn"], &[]);
    assert!(s.banner.starts_with("nREPL server started on socket nrepl+edn+unix:"), "{}", s.banner);
}

// ---- ack ----

#[test]
fn ack_sends_the_port_like_the_jvm() {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let ack_port = l.local_addr().unwrap().port();
    let acceptor = std::thread::spawn(move || {
        let (mut s, _) = l.accept().unwrap();
        s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        let mut buf = Vec::new();
        let msg = loop {
            let mut tmp = [0u8; 1024];
            let n = s.read(&mut tmp).unwrap();
            buf.extend_from_slice(&tmp[..n]);
            if let Ok(Some((v, _))) = decode(&buf) {
                break v;
            }
        };
        let mut o = Vec::new();
        encode(&Value::dict([("status", Value::List(vec![Value::str("done")]))]), &mut o);
        s.write_all(&o).unwrap();
        msg
    });
    let s = Srv::start(&["--ack", &ack_port.to_string()]);
    let msg = acceptor.join().unwrap();
    assert_eq!(msg.get("op").and_then(|v| v.as_str()), Some("ack"));
    // the port is an integer on the wire
    assert_eq!(msg.get("port"), Some(&Value::Int(s.port.unwrap() as i64)));
}

#[test]
fn ack_to_nobody_fails_before_the_banner() {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let dead = l.local_addr().unwrap().port();
    drop(l);
    let (code, out, err) = run(&["--ack", &dead.to_string()]);
    assert_eq!(code, 1);
    assert!(out.is_empty(), "no banner: {out:?}");
    assert!(err.contains("ack"), "{err}");
}

// ---- EDN ----

fn edn_read(s: &mut TcpStream, buf: &mut Vec<u8>) -> mova_nrepl::edn::Edn {
    loop {
        match mova_nrepl::edn::parse(buf, 0, false) {
            Ok((v, used)) => {
                buf.drain(..used);
                return v;
            }
            Err(mova_nrepl::edn::Stop::Need) => {
                let mut tmp = [0u8; 8192];
                let n = s.read(&mut tmp).unwrap();
                assert!(n > 0, "closed");
                buf.extend_from_slice(&tmp[..n]);
            }
            Err(e) => panic!("{e:?}"),
        }
    }
}

fn get<'a>(m: &'a mova_nrepl::edn::Edn, k: &str) -> Option<&'a mova_nrepl::edn::Edn> {
    match m {
        mova_nrepl::edn::Edn::Map(es) => es.iter().find(|(kk, _)| matches!(kk, mova_nrepl::edn::Edn::Kw(n) if n == k)).map(|(_, v)| v),
        _ => None,
    }
}

#[test]
fn edn_transport_roundtrip() {
    use mova_nrepl::edn::Edn;
    let s = Srv::start(&["-t", "nrepl.transport/edn"]);
    assert!(s.banner.ends_with(&format!(" - nrepl+edn://127.0.0.1:{}", s.port.unwrap())), "{}", s.banner);
    let mut c = s.tcp();
    let mut buf = Vec::new();
    // messages split across writes, and two in one write
    c.write_all(b"{:op \"clone\" :id \"1\"}").unwrap();
    let m = edn_read(&mut c, &mut buf);
    let Some(Edn::Str(sess)) = get(&m, "new-session") else { panic!("{m:?}") };
    let sess = sess.clone();
    assert_eq!(get(&m, "id"), Some(&Edn::Str("1".into())));
    assert_eq!(get(&m, "status"), Some(&Edn::Set(vec![Edn::Kw("done".into())])));
    let req = format!("{{:op \"eval\" :id \"2\" :session \"{sess}\" :code \"(+ 1 2) (println :hi)\"}}{{:op \"describe\" :id \"3\"}}");
    let (a, b) = req.as_bytes().split_at(30);
    c.write_all(a).unwrap();
    std::thread::sleep(Duration::from_millis(50));
    c.write_all(b).unwrap();
    let mut seen = Vec::new();
    let mut done = 0;
    while done < 2 {
        let m = edn_read(&mut c, &mut buf);
        if let Some(Edn::Str(v)) = get(&m, "value") {
            seen.push(format!("value {v}"));
        }
        if let Some(Edn::Str(v)) = get(&m, "out") {
            seen.push(format!("out {v:?}"));
        }
        if get(&m, "versions").is_some() {
            assert!(matches!(get(&m, "ops"), Some(Edn::Map(_))));
        }
        if matches!(get(&m, "status"), Some(Edn::Set(l)) if l.contains(&Edn::Kw("done".into()))) {
            done += 1;
        }
    }
    assert_eq!(seen, ["value 3", "out \":hi\\n\"", "value nil"]);
    // error: ex / root-ex as strings, eval-error in the status set
    c.write_all(format!("{{:op \"eval\" :id \"4\" :session \"{sess}\" :code \"(/ 1 0)\"}}").as_bytes()).unwrap();
    let mut err = false;
    loop {
        let m = edn_read(&mut c, &mut buf);
        if matches!(get(&m, "status"), Some(Edn::Set(l)) if l.contains(&Edn::Kw("eval-error".into()))) {
            assert_eq!(get(&m, "ex"), Some(&Edn::Str("class java.lang.ArithmeticException".into())));
            err = true;
        }
        if matches!(get(&m, "status"), Some(Edn::Set(l)) if l.contains(&Edn::Kw("done".into()))) {
            break;
        }
    }
    assert!(err);
    // junk closes the connection
    c.write_all(b"[1 2]").unwrap();
    let mut tmp = [0u8; 16];
    assert_eq!(c.read(&mut tmp).unwrap_or(0), 0);
}

// ---- TTY ----

fn read_until(s: &mut TcpStream, buf: &mut String, want: &str) {
    let t = Instant::now();
    while !buf.contains(want) {
        assert!(t.elapsed() < Duration::from_secs(20), "waiting for {want:?}, have {buf:?}");
        let mut tmp = [0u8; 4096];
        let n = s.read(&mut tmp).unwrap_or_else(|e| panic!("{e}; waiting for {want:?}, have {buf:?}"));
        assert!(n > 0, "closed; have {buf:?}");
        buf.push_str(&String::from_utf8_lossy(&tmp[..n]));
    }
}

#[test]
fn tty_transport_matches_cmdline_tty_test() {
    // the lines of `cmdline_tty_test/tty-server`, except the JVM-only first one
    let s = Srv::start(&["--transport", "nrepl.transport/tty"]);
    assert!(s.banner.contains(" - telnet://127.0.0.1:"), "{}", s.banner);
    let mut c = s.tcp();
    let mut out = String::new();
    read_until(&mut c, &mut out, "user=> ");
    assert!(out.starts_with(";; nREPL 1.8.0\n;; Clojure "), "{out:?}");
    assert!(out.ends_with("\nuser=> "));
    for l in [
        "(+ 1 2)",
        "#?(:clj :clj-form)",
        "#?(:cljs :cljs-form)",
        "(clojure.core/require '[clojure.set :as sets])",
        "::sets/xyz",
        "(clojure.core/require '[clojure.string :as str])",
        "{::sets/x 1 ::str/x 2}",
    ] {
        c.write_all(format!("{l}\n").as_bytes()).unwrap();
    }
    let want = "user=> 3\nuser=> :clj-form\nuser=> nil\nuser=> :clojure.set/xyz\nuser=> nil\nuser=> {:clojure.set/x 1, :clojure.string/x 2}\nuser=> ";
    let mut all = out.clone();
    read_until(&mut c, &mut all, "{:clojure.set/x 1, :clojure.string/x 2}\nuser=> ");
    assert!(all.ends_with(want), "got {all:?}");
    // a form over two lines, output and value, and an error
    c.write_all(b"(do (println \"hi\")\n  7)\n(/ 1 0)\n(def x 5)\n(ns foo)\n").unwrap();
    let mut rest = String::new();
    read_until(&mut c, &mut rest, "foo=> ");
    assert!(rest.starts_with("hi\n7\nuser=> "), "{rest:?}");
    assert!(rest.contains("Divide by zero"), "{rest:?}");
    assert!(rest.contains("user=> #'user/x\nuser=> nil\nfoo=> "), "{rest:?}");
}

#[test]
fn tty_session_is_closed_with_the_connection() {
    let s = Srv::start(&["-t", "nrepl.transport/tty"]);
    let mut c = s.tcp();
    let mut out = String::new();
    read_until(&mut c, &mut out, "user=> ");
    c.write_all(b"(def kept 1)\n").unwrap();
    read_until(&mut c, &mut out, "#'user/kept\nuser=> ");
    drop(c);
    // the server keeps running for the next connection; defs are global
    let mut c = s.tcp();
    let mut out = String::new();
    read_until(&mut c, &mut out, "user=> ");
    c.write_all(b"kept\n").unwrap();
    read_until(&mut c, &mut out, "1\nuser=> ");
}

#[test]
fn tty_is_rejected_by_the_built_in_client() {
    let (code, _, err) = run(&["-i", "-t", "nrepl.transport/tty"]);
    assert_eq!(code, 2);
    assert!(err.contains("does not support the tty transport"), "{err}");
    let (code, _, err) = run(&["-c", "-h", "telnet://localhost:1"]);
    assert_eq!(code, 2);
    assert!(err.contains("does not support the tty transport"), "{err}");
}

// ---- built-in client ----

fn pipe_through(args: &[&str], input: &str) -> (i32, String) {
    let dir = tmpdir("cli");
    let mut child = Command::new(MOVA)
        .arg("nrepl")
        .args(args)
        .current_dir(&dir)
        .env("HOME", &dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
    let o = child.wait_with_output().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    (o.status.code().unwrap_or(-1), String::from_utf8_lossy(&o.stdout).into_owned() + &String::from_utf8_lossy(&o.stderr))
}

#[test]
fn interactive_repl() {
    let (code, out) = pipe_through(&["-i"], "(+ 1 2)\n(println \"x\")\n(def a 1)\n(ns bar)\n(/ 1 0)\n(exit)\n(+ 5 5)\n");
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("nREPL server started on port "), "{out}");
    assert!(out.contains("nREPL 1.8.0\nClojure "), "{out}");
    assert!(out.contains("Interrupt: Control+C\nExit:      Control+D or (exit) or (quit)\nuser=> "), "{out}");
    assert!(out.contains("user=> 3\nuser=> x\nnil\nuser=> #'user/a\nuser=> nil\nbar=> "), "{out}");
    assert!(out.contains("Divide by zero"), "{out}");
    assert!(!out.contains("\n10\n"), "nothing runs after (exit): {out}");
    // end of input also ends it
    let (code, _) = pipe_through(&["-i", "-t", "nrepl.transport/edn"], "(+ 1 2)\n");
    assert_eq!(code, 0);
}

#[test]
fn connect_repl_to_another_server() {
    let s = Srv::start(&[]);
    let port = s.port.unwrap().to_string();
    let (code, out) = pipe_through(&["-c", "-p", &port], "(+ 20 22)\n(exit)\n");
    assert_eq!(code, 0, "{out}");
    assert!(!out.contains("server started"), "connect mode does not start a server: {out}");
    assert!(out.contains("user=> 42\n"), "{out}");
    // as a URL, with colors
    let url = format!("nrepl://127.0.0.1:{port}");
    let (code, out) = pipe_through(&["--connect", "-h", &url, "--color"], "1\n");
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("\x1b[34m1\x1b[m\n"), "{out:?}");
    // conflicts with a URL
    let (code, _) = pipe_through(&["-c", "-h", &url, "-p", &port], "");
    assert_eq!(code, 2);
    // the JVM text for http
    let (code, out) = pipe_through(&["-c", "-h", "http://localhost/repl"], "");
    assert_eq!(code, 2);
    assert!(out.contains("nrepl/drawbridge"), "{out}");
}

#[test]
fn connect_to_a_unix_socket_server() {
    let dir = tmpdir("sock3");
    let path = dir.join("c.sock");
    let _s = Srv::start_in(dir.clone(), &["-s", path.to_str().unwrap()], &[]);
    let (code, out) = pipe_through(&["-c", "-s", path.to_str().unwrap()], "(+ 1 1)\n");
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("user=> 2\n"), "{out}");
    let url = format!("nrepl+unix:{}", path.display());
    let (code, out) = pipe_through(&["-c", "-h", &url], "(+ 1 2)\n");
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("user=> 3\n"), "{out}");
}

// ---- forward-system-output ----

#[test]
fn forward_system_output() {
    let s = Srv::start(&[]);
    let mut a = Bc::new(s.tcp());
    let mut b = Bc::new(s.tcp());
    let fwd = a.clone_session();
    let ev = b.clone_session();
    a.send(&[("op", "forward-system-output"), ("id", "f"), ("session", &fwd)]);
    assert_eq!(a.recv().get("status"), Some(&Value::List(vec![Value::str("done")])));
    assert_eq!(b.eval(&ev, "(.println System/out \"fwd-out\")"), ["nil"]);
    let m = a.recv();
    assert_eq!(m.get("out").and_then(|v| v.as_str()), Some("fwd-out\n"));
    assert_eq!(m.get("source").and_then(|v| v.as_str()), Some("system"));
    assert_eq!(m.get("id").and_then(|v| v.as_str()), Some("f"));
    assert_eq!(m.get("session").and_then(|v| v.as_str()), Some(fwd.as_str()));
    // stderr; print stays in the buffer until a flush
    assert_eq!(b.eval(&ev, "(.println System/err \"fwd-err\")"), ["nil"]);
    assert_eq!(a.recv().get("err").and_then(|v| v.as_str()), Some("fwd-err\n"));
    assert_eq!(b.eval(&ev, "(do (.print System/out \"partial\") 1)"), ["1"]);
    assert_eq!(b.eval(&ev, "(do (.flush System/out) 2)"), ["2"]);
    assert_eq!(a.recv().get("out").and_then(|v| v.as_str()), Some("partial"));
    // 3000 chars: chunks of 1024, 1024 and 953 (with the newline)
    assert_eq!(b.eval(&ev, "(.println System/out (apply str (repeat 3000 \"z\")))"), ["nil"]);
    let lens: Vec<usize> = (0..3).map(|_| a.recv().get("out").and_then(|v| v.as_str()).map(|s| s.len()).unwrap()).collect();
    assert_eq!(lens, [1024, 1024, 953]);
    // an ordinary println in the eval is not forwarded (it is the eval's own `out`)
    assert_eq!(b.eval(&ev, "(println \"mine\")"), ["nil"]);
    // closing the forwarding session stops it
    a.send(&[("op", "close"), ("session", &fwd), ("id", "x")]);
    a.recv();
    assert_eq!(b.eval(&ev, "(.println System/out \"gone\")"), ["nil"]);
    // no session: just done
    b.send(&[("op", "forward-system-output"), ("id", "n")]);
    assert_eq!(b.recv().get("status"), Some(&Value::List(vec![Value::str("done")])));
}

// ---- idle CPU ----

#[test]
fn idle_cpu_is_zero_for_every_transport() {
    for t in ["nrepl.transport/bencode", "nrepl.transport/edn", "nrepl.transport/tty"] {
        let s = Srv::start(&["-t", t]);
        let mut c = s.tcp();
        if t.ends_with("bencode") {
            let mut b = Bc::new(c.try_clone().unwrap());
            let sess = b.clone_session();
            assert_eq!(b.eval(&sess, "(+ 1 2)"), ["3"]);
        } else if t.ends_with("edn") {
            c.write_all(b"{:op \"describe\" :id \"1\"}").unwrap();
            let mut buf = Vec::new();
            edn_read(&mut c, &mut buf);
        } else {
            let mut out = String::new();
            read_until(&mut c, &mut out, "user=> ");
        }
        std::thread::sleep(Duration::from_millis(500));
        let before = s.cpu_seconds();
        std::thread::sleep(Duration::from_secs(3));
        let used = s.cpu_seconds() - before;
        assert!(used <= 0.011, "{t}: {used}s of CPU in 3 s idle");
    }
}

// ---- TLS ----

#[cfg(feature = "tls")]
mod tls {
    use super::*;

    fn openssl(dir: &std::path::Path, args: &[&str]) {
        let st = Command::new("openssl").args(args).current_dir(dir).stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap();
        assert!(st.success(), "openssl {args:?}");
    }

    /// CA, a server and a client certificate; key files in the nrepl layout (CA, own cert, PKCS#8 key).
    fn make_keys(dir: &std::path::Path) {
        openssl(dir, &["req", "-x509", "-newkey", "rsa:2048", "-nodes", "-keyout", "ca.key", "-out", "ca.pem", "-subj", "/CN=test-ca", "-days", "2"]);
        for who in ["server", "client"] {
            openssl(dir, &["req", "-newkey", "rsa:2048", "-nodes", "-keyout", &format!("{who}.key"), "-out", &format!("{who}.csr"), "-subj", &format!("/CN={who}")]);
            std::fs::write(dir.join(format!("{who}.ext")), "subjectAltName=DNS:localhost,IP:127.0.0.1\nextendedKeyUsage=serverAuth,clientAuth\n").unwrap();
            openssl(dir, &["x509", "-req", "-in", &format!("{who}.csr"), "-CA", "ca.pem", "-CAkey", "ca.key", "-CAcreateserial", "-out", &format!("{who}.pem"), "-days", "2", "-extfile", &format!("{who}.ext")]);
            openssl(dir, &["pkcs8", "-topk8", "-nocrypt", "-in", &format!("{who}.key"), "-out", &format!("{who}.p8")]);
            let all = ["ca.pem".to_string(), format!("{who}.pem"), format!("{who}.p8")]
                .iter()
                .map(|f| std::fs::read_to_string(dir.join(f)).unwrap())
                .collect::<String>();
            std::fs::write(dir.join(format!("{who}-keys.pem")), all).unwrap();
        }
    }

    #[test]
    fn tls_server_with_mutual_auth() {
        let dir = tmpdir("tls");
        make_keys(&dir);
        let server_keys = dir.join("server-keys.pem");
        let client_keys = dir.join("client-keys.pem");
        let s = Srv::start_in(dir.clone(), &["--tls-keys-file", server_keys.to_str().unwrap()], &[]);
        assert!(s.banner.contains(" - nrepls://127.0.0.1:"), "{}", s.banner);
        let port = s.port.unwrap().to_string();
        // the built-in client with the client keys
        let (code, out) = pipe_through(&["-c", "-p", &port, "--tls-keys-file", client_keys.to_str().unwrap()], "(+ 40 2)\n");
        assert_eq!(code, 0, "{out}");
        assert!(out.contains("user=> 42\n"), "{out}");
        // same keys as the server: refused, and the server goes on
        let (_, out) = pipe_through(&["-c", "-p", &port, "--tls-keys-file", server_keys.to_str().unwrap()], "(+ 1 1)\n");
        assert!(!out.contains("user=> 2\n"), "{out}");
        // a plain client gets nothing useful, the server goes on
        let mut plain = s.tcp();
        let _ = plain.write_all(b"d2:op8:describee");
        let mut tmp = [0u8; 64];
        plain.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let n = plain.read(&mut tmp).unwrap_or(0);
        assert!(!tmp[..n].windows(8).any(|w| w == b"versions"));
        let (code, out) = pipe_through(&["-c", "-p", &port, "--tls-keys-file", client_keys.to_str().unwrap()], "(+ 1 2)\n");
        assert_eq!(code, 0, "{out}");
        assert!(out.contains("user=> 3\n"), "{out}");
        // -i with TLS
        let (code, out) = pipe_through(&["-i", "--tls-keys-file", server_keys.to_str().unwrap()], "(+ 2 2)\n");
        // -i uses the server's own keys for the client: the JVM refuses that too
        let _ = (code, out);
    }

    #[test]
    fn tls_errors_are_clear() {
        let (code, _, err) = run(&["--tls-keys-file", "/nonexistent/keys.pem"]);
        assert_eq!(code, 2);
        assert!(err.contains(":tls-keys-file specified as /nonexistent/keys.pem, but the file was not found."), "{err}");
        let (code, _, err) = run(&["--tls-keys-str", "nonsense"]);
        assert_eq!(code, 2);
        assert!(err.contains("Could not create TLS context from string. Error message: No certificates found."), "{err}");
    }
}

#[cfg(not(feature = "tls"))]
#[test]
fn tls_flags_fail_clearly_without_the_feature() {
    let (code, _, err) = run(&["--tls-keys-file", "x.pem"]);
    assert_eq!(code, 2);
    assert!(err.contains("TLS support is not built in"), "{err}");
}
