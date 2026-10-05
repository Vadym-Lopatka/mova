//! `mova nrepl` and `--module-path`: the flag works before or after `nrepl`,
//! and means what it means for the script runner.

use mova_nrepl::bencode::{decode, encode, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

const MOVA: &str = env!("CARGO_BIN_EXE_mova");

/// A temp dir with two roots: `demo.core` (in `a`) requires `demo.util` (in `b`).
fn project() -> std::path::PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let d = std::env::temp_dir().join(format!("mova-modpath-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
    std::fs::create_dir_all(d.join("a/demo")).unwrap();
    std::fs::create_dir_all(d.join("b/demo")).unwrap();
    std::fs::write(d.join("a/demo/core.mova"), "(ns demo.core (:require [demo.util :as u]))\n(defn hello [] (u/twice 21))\n").unwrap();
    std::fs::write(d.join("b/demo/util.mova"), "(ns demo.util)\n(defn twice [x] (* 2 x))\n").unwrap();
    d
}

struct Srv {
    child: Child,
    dir: std::path::PathBuf,
    port: u16,
    _out: BufReader<std::process::ChildStdout>,
}

impl Srv {
    /// `args` are everything after the binary name, `nrepl` included.
    fn start(dir: &std::path::Path, args: &[&str]) -> Srv {
        let mut child = Command::new(MOVA)
            .args(args)
            .current_dir(dir)
            .env("HOME", dir)
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("NREPL_CONFIG_DIR")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut out = BufReader::new(child.stdout.take().unwrap());
        let mut banner = String::new();
        out.read_line(&mut banner).unwrap();
        let port = banner
            .strip_prefix("nREPL server started on port ")
            .and_then(|r| r.split(' ').next())
            .and_then(|p| p.parse().ok())
            .unwrap_or_else(|| panic!("banner: {banner:?}"));
        Srv { child, dir: dir.to_path_buf(), port, _out: out }
    }
}

impl Drop for Srv {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

struct Client {
    s: TcpStream,
    buf: Vec<u8>,
}

impl Client {
    fn new(port: u16) -> Client {
        let s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        Client { s, buf: Vec::new() }
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

    /// All messages of one request, until `done`.
    fn call(&mut self, items: &[(&str, &str)]) -> Vec<Value> {
        self.send(items);
        let mut all = Vec::new();
        loop {
            let m = self.recv();
            let done = matches!(m.get("status"), Some(Value::List(l)) if l.iter().any(|x| x.as_str() == Some("done")));
            all.push(m);
            if done {
                return all;
            }
        }
    }

    /// The `value`s, or the `err` text if the eval failed.
    fn eval(&mut self, session: Option<&str>, code: &str) -> Result<Vec<String>, String> {
        let mut msg = vec![("op", "eval"), ("code", code), ("id", "e")];
        if let Some(s) = session {
            msg.push(("session", s));
        }
        let (mut vals, mut err) = (Vec::new(), String::new());
        for m in self.call(&msg) {
            if let Some(v) = m.get("value").and_then(|v| v.as_str()) {
                vals.push(v.to_string());
            }
            if let Some(e) = m.get("err").and_then(|v| v.as_str()) {
                err.push_str(e);
            }
        }
        if err.is_empty() {
            Ok(vals)
        } else {
            Err(err)
        }
    }

    fn clone_session(&mut self) -> String {
        let r = self.call(&[("op", "clone"), ("id", "c")]);
        r[0].get("new-session").and_then(|v| v.as_str()).unwrap().to_string()
    }
}

fn mp(dir: &std::path::Path) -> String {
    format!("{}:{}", dir.join("a").display(), dir.join("b").display())
}

/// Require and call in no session, a clone and a later clone; then `load-file`
/// of a changed module (its own file, and one that requires through the path).
/// (`require` has no `:reload` flag in Mova, so a reload is a `load-file`.)
fn check_all_sessions(port: u16, dir: &std::path::Path) {
    let mut c = Client::new(port);
    assert_eq!(c.eval(None, "(require 'demo.core) (demo.core/hello)"), Ok(vec!["nil".into(), "42".into()]));
    let s1 = c.clone_session();
    assert_eq!(c.eval(Some(&s1), "(demo.core/hello)"), Ok(vec!["42".into()]));
    let s2 = c.clone_session();
    assert_eq!(c.eval(Some(&s2), "(require 'demo.core) (demo.core/hello)"), Ok(vec!["nil".into(), "42".into()]));
    let load = |c: &mut Client, session: &str, f: &std::path::Path| {
        let code = std::fs::read_to_string(f).unwrap();
        let name = f.file_name().unwrap().to_str().unwrap();
        let msgs = c.call(&[("op", "load-file"), ("file", &code), ("file-name", name), ("file-path", f.to_str().unwrap()), ("session", session), ("id", "l")]);
        assert!(msgs.iter().all(|m| m.get("err").is_none()), "{msgs:?}");
    };
    // a changed demo.util, loaded again, is seen by demo.core
    let util = dir.join("b/demo/util.mova");
    std::fs::write(&util, "(ns demo.util)\n(defn twice [x] (* 3 x))\n").unwrap();
    load(&mut c, &s2, &util);
    assert_eq!(c.eval(Some(&s1), "(demo.core/hello)"), Ok(vec!["63".into()]));
    // a loaded file whose `require` goes through the module path
    let f = dir.join("load_me.mova");
    std::fs::write(&f, "(ns demo.loaded (:require [demo.util :as u]))\n(defn five [] (u/twice 5))\n").unwrap();
    load(&mut c, &s1, &f);
    assert_eq!(c.eval(Some(&s1), "(demo.loaded/five)"), Ok(vec!["15".into()]));
}

#[test]
fn flag_before_nrepl() {
    let dir = project();
    let m = mp(&dir);
    let s = Srv::start(&dir, &["--module-path", &m, "nrepl", "-p", "0"]);
    check_all_sessions(s.port, &dir);
}

#[test]
fn flag_after_nrepl() {
    let dir = project();
    let m = mp(&dir);
    let s = Srv::start(&dir, &["nrepl", "-p", "0", "--module-path", &m]);
    check_all_sessions(s.port, &dir);
}

#[test]
fn flag_with_equals_sign() {
    let dir = project();
    let m = format!("--module-path={}", mp(&dir));
    let s = Srv::start(&dir, &["nrepl", &m, "-p", "0"]);
    let mut c = Client::new(s.port);
    assert_eq!(c.eval(None, "(require 'demo.core) (demo.core/hello)"), Ok(vec!["nil".into(), "42".into()]));
}

#[test]
fn without_the_flag_the_require_fails() {
    let dir = project();
    let s = Srv::start(&dir, &["nrepl", "-p", "0"]);
    let mut c = Client::new(s.port);
    let err = c.eval(None, "(require 'demo.core)").unwrap_err();
    assert!(err.contains("demo/core.mova") && err.contains("[.]"), "{err}");
    // the default is the working directory: a file there is found
    std::fs::create_dir_all(dir.join("demo")).unwrap();
    std::fs::write(dir.join("demo/util.mova"), "(ns demo.util)\n(defn twice [x] (* 2 x))\n").unwrap();
    assert_eq!(c.eval(None, "(require 'demo.util) (demo.util/twice 4)"), Ok(vec!["nil".into(), "8".into()]));
}

#[test]
fn flag_without_a_value_is_an_error() {
    let dir = project();
    let o = Command::new(MOVA).args(["nrepl", "--module-path"]).current_dir(&dir).env("HOME", &dir).stdin(Stdio::null()).output().unwrap();
    assert_eq!(o.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&o.stderr).contains("--module-path requires"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn main_version_and_help() {
    let run = |args: &[&str]| Command::new(MOVA).args(args).stdin(Stdio::null()).output().unwrap();
    let o = run(&["--version"]);
    assert_eq!(String::from_utf8_lossy(&o.stdout), format!("mova {}\n", env!("CARGO_PKG_VERSION")));
    let o = run(&["--help"]);
    let help = String::from_utf8_lossy(&o.stdout);
    assert!(help.starts_with(&format!("mova {} ", env!("CARGO_PKG_VERSION"))), "{help}");
    assert!(help.contains("mova nrepl") && help.contains("mova nrepl --help"), "{help}");
    // the nREPL version stays the JVM one
    assert_eq!(String::from_utf8_lossy(&run(&["nrepl", "--version"]).stdout), "1.8.0\n");
    assert!(String::from_utf8_lossy(&run(&["nrepl", "--help"]).stdout).contains("--module-path"));
}
