//! nREPL phase P2 (`eval`): sessions, bindings, output chunking, errors, and
//! the thread model, through the real server over a TCP socket. Expected
//! texts and message orders are from the JVM nREPL 1.8.0 goldens
//! (`crates/mova-nrepl/oracle`, see `show.py <scenario>`).
//!
//! Tests share one process; each takes `SERIAL` so the idle-CPU and thread
//! count checks see only their own server.

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
    fn start() -> Fixture {
        let guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let l = Listeners::bind(&Endpoint::Tcp { host: "127.0.0.1".into(), port: 0 }).unwrap();
        let port = l.port().unwrap();
        let backend = MovaBackend::new(Config { errors: mova::nrepl::ErrorMode::Jvm, ..Config::default() });
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

fn int(n: i64) -> Value {
    Value::Int(n)
}

fn texts(msgs: &[Msg], key: &str) -> Vec<String> {
    msgs.iter().filter_map(|m| m.s(key)).collect()
}

/// A compact trace: `out:..`, `err:..`, `v:..`, `done`, `ex`, `status:..`.
fn trace(msgs: &[Msg]) -> Vec<String> {
    msgs.iter()
        .map(|m| {
            if let Some(o) = m.s("out") {
                format!("out:{o}")
            } else if let Some(e) = m.s("err") {
                format!("err:{e}")
            } else if let Some(v) = m.s("value") {
                format!("v:{v}")
            } else if m.s("ex").is_some() {
                "ex".to_string()
            } else {
                format!("status:{}", m.status().join(","))
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// eval semantics
// ---------------------------------------------------------------------------

#[test]
fn one_value_and_ns_per_form_then_done() {
    let f = Fixture::start();
    let mut c = f.client();
    let s = c.clone_session(None);
    let m = c.eval(&s, "1 2 (+ 1 2)");
    assert_eq!(texts(&m, "value"), ["1", "2", "3"]);
    assert_eq!(texts(&m, "ns"), ["user", "user", "user"]);
    assert_eq!(m.len(), 4);
    assert_eq!(m.last().unwrap().status(), ["done"]);
    // every reply carries the session
    assert!(m.iter().all(|m| m.s("session").as_deref() == Some(s.as_str())));
}

#[test]
fn empty_and_comment_only_code_gives_only_done() {
    let f = Fixture::start();
    let mut c = f.client();
    let s = c.clone_session(None);
    for code in ["", "   \n", "; just a comment", "#_(+ 1 2)"] {
        let m = c.eval(&s, code);
        assert_eq!(trace(&m), ["status:done"], "code {code:?}");
    }
}

#[test]
fn errors_are_three_messages_and_the_next_form_still_runs() {
    let f = Fixture::start();
    let mut c = f.client();
    let s = c.clone_session(None);
    let m = c.eval(&s, "(println 1) (/ 1 0) (println 2)");
    let t = trace(&m);
    assert_eq!(t[0], "out:1\n");
    assert_eq!(t[1], "v:nil");
    assert!(t[2].starts_with("err:Execution error (ArithmeticException) at user/eval"), "{t:?}");
    assert!(t[2].ends_with("(REPL:1).\nDivide by zero\n"), "{t:?}");
    assert_eq!(t[3], "ex");
    assert_eq!(m[3].s("ex").as_deref(), Some("class java.lang.ArithmeticException"));
    assert_eq!(m[3].s("root-ex").as_deref(), Some("class java.lang.ArithmeticException"));
    assert_eq!(m[3].status(), ["eval-error"]);
    assert_eq!(&t[4..], ["out:2\n", "v:nil", "status:done"]);
    // the err message and the ex message have no `done`; only the last does
    assert!(m.iter().take(m.len() - 1).all(|m| !m.has_status("done")));
}

#[test]
fn read_error_comes_after_the_forms_before_it_and_stops() {
    let f = Fixture::start();
    let mut c = f.client();
    let s = c.clone_session(None);
    let m = c.eval(&s, "(+ 1 2) (+ 1 (+ 3 4)");
    let t = trace(&m);
    assert_eq!(t[0], "v:3");
    assert_eq!(t[1], "err:Syntax error reading source at (REPL:2:1).\nEOF while reading, starting at line 1\n");
    assert_eq!(t[2], "ex");
    assert_eq!(m[2].s("ex").as_deref(), Some("class clojure.lang.ExceptionInfo"));
    assert_eq!(m[2].s("root-ex").as_deref(), Some("class java.lang.RuntimeException"));
    assert_eq!(t[3], "status:done");
    // nothing after the bad form is read
    let m = c.eval(&s, "(+ 1 2) ) (+ 3 4)");
    assert_eq!(trace(&m)[0], "v:3");
    assert!(trace(&m)[1].starts_with("err:Syntax error reading source at (REPL:1:10)"), "{:?}", trace(&m));
    assert_eq!(m.iter().filter(|m| m.s("value").is_some()).count(), 1);
}

#[test]
fn ns_param_is_for_this_request_only_and_unknown_is_an_error() {
    let f = Fixture::start();
    let mut c = f.client();
    let s = c.clone_session(None);
    let m = c.eval_in(Some(&s), "(str *ns*)", &[("ns", Value::str("clojure.string"))]);
    assert_eq!(texts(&m, "value"), ["\"clojure.string\""]);
    assert_eq!(texts(&m, "ns"), ["clojure.string"]);
    // not sticky
    assert_eq!(c.value(&s, "(str *ns*)"), "\"user\"");
    for bad in ["no.such.ns", ""] {
        let m = c.eval_in(Some(&s), "(+ 1 2)", &[("ns", Value::str(bad))]);
        assert_eq!(m.len(), 1, "{bad}");
        assert_eq!(m[0].status(), ["namespace-not-found", "done", "error"]);
        assert_eq!(m[0].s("ns").as_deref(), Some(bad));
    }
    // the session still works, and `in-ns` / `ns` stick
    c.eval(&s, "(in-ns 'foo.bar)");
    assert_eq!(c.value(&s, "(clojure.core/str clojure.core/*ns*)"), "\"foo.bar\"");
    // `in-ns` inside a request with `ns` ends with that request
    let m = c.eval_in(Some(&s), "(in-ns 'other.ns) (clojure.core/str clojure.core/*ns*)", &[("ns", Value::str("user"))]);
    assert_eq!(texts(&m, "ns").last().map(|s| s.as_str()), Some("other.ns"));
    assert_eq!(c.value(&s, "(clojure.core/str clojure.core/*ns*)"), "\"foo.bar\"");
    let m = c.eval(&s, "(def zz 1)");
    assert_eq!(texts(&m, "ns"), ["foo.bar"]);
}

#[test]
fn file_line_column_place_forms_and_errors() {
    let f = Fixture::start();
    let mut c = f.client();
    let s = c.clone_session(None);
    let extra = [("file", Value::str("foo/bar.clj")), ("line", int(10)), ("column", int(5))];
    let m = c.eval_in(Some(&s), "(def pv1 1)\n(def pv2 2)\n(select-keys (meta #'pv2) [:line :column :file])", &extra);
    assert_eq!(texts(&m, "value")[2], "{:line 11, :column 1, :file \"foo/bar.clj\"}");
    // first line is shifted by the column
    let m = c.eval_in(Some(&s), "(do (def pv9 1) (select-keys (meta #'pv9) [:line :column]))", &extra);
    assert_eq!(texts(&m, "value"), ["{:line 10, :column 9}"]);
    // error locations use the short file name
    let m = c.eval_in(Some(&s), "(+ 1 2)\n(/ 1 0)", &[("file", Value::str("foo/bar.clj")), ("line", int(20))]);
    let e = texts(&m, "err");
    assert!(e[0].ends_with("(bar.clj:21).\nDivide by zero\n"), "{e:?}");
    let m = c.eval_in(Some(&s), "(foo-unresolved)", &[("file", Value::str("foo/bar.clj")), ("line", int(30)), ("column", int(2))]);
    assert_eq!(
        texts(&m, "err")[0],
        "Syntax error compiling at (foo/bar.clj:30:2).\nUnable to resolve symbol: foo-unresolved in this context\n"
    );
    // `*file*` and `*source-path*` (short) are bound for the request, not after
    let m = c.eval_in(Some(&s), "[*file* *source-path*]", &[("file", Value::str("d/q.clj"))]);
    assert_eq!(texts(&m, "value"), ["[\"d/q.clj\" \"q.clj\"]"]);
    // the JVM session keeps the last `file` (a later def records it)
    assert_eq!(c.value(&s, "*file*"), "\"d/q.clj\"");
    // a `line` that is not an int: no reply at all (as on the JVM); the session lives on
    let id = c.send(&[("op", Value::str("eval")), ("code", Value::str("1")), ("session", Value::str(&s)), ("line", Value::str("12"))]);
    assert_eq!(c.value(&s, "2"), "2");
    let _ = id;
}

#[test]
fn read_cond_allow_default_and_refused() {
    let f = Fixture::start();
    let mut c = f.client();
    let s = c.clone_session(None);
    assert_eq!(c.value(&s, "#?(:clj 1 :cljs 2)"), "1");
    let m = c.eval_in(Some(&s), "#?(:clj 1 :cljs 2)", &[("read-cond", Value::str("allow"))]);
    assert_eq!(texts(&m, "value"), ["1"]);
    let m = c.eval_in(Some(&s), "#?(:cljs 2)", &[("read-cond", Value::str("allow"))]);
    assert_eq!(trace(&m), ["status:done"]);
    for bad in ["bogus", ""] {
        let m = c.eval_in(Some(&s), "#?(:clj 1)", &[("read-cond", Value::str(bad))]);
        assert!(texts(&m, "err")[0].ends_with("Conditional read not allowed\n"), "{:?}", trace(&m));
    }
}

#[test]
fn star_vars_update_on_success_and_e_on_error() {
    let f = Fixture::start();
    let mut c = f.client();
    let s = c.clone_session(None);
    for n in ["10", "20", "30"] {
        c.eval(&s, n);
    }
    assert_eq!(c.value(&s, "[*1 *2 *3]"), "[30 20 10]");
    assert_eq!(c.value(&s, "[*1 *2 *3]"), "[[30 20 10] 30 20]");
    c.eval(&s, "(/ 1 0)");
    assert_eq!(c.value(&s, "(.getMessage *e)"), "\"Divide by zero\"");
    // an error leaves *1 *2 *3 alone
    c.eval(&s, "1 2");
    c.eval(&s, "(do 5 (/ 1 0))");
    assert_eq!(c.value(&s, "[*1 *2]"), "[2 1]");
    // another session has its own
    let t = c.clone_session(None);
    assert_eq!(c.value(&t, "[*1 *2 *3 *e]"), "[nil nil nil nil]");
}

#[test]
fn set_bang_of_session_vars_sticks_and_does_not_leak() {
    let f = Fixture::start();
    let mut c = f.client();
    let s = c.clone_session(None);
    let t = c.clone_session(None);
    c.eval(&s, "(set! *print-length* 3)");
    assert_eq!(c.value(&s, "(range 10)"), "(0 1 2 ...)");
    assert_eq!(c.value(&t, "(range 10)"), "(0 1 2 3 4 5 6 7 8 9)");
    assert_eq!(c.value(&s, "*print-length*"), "3");
    assert_eq!(c.value(&t, "*print-length*"), "nil");
    // an ephemeral request does not see it either
    let m = c.eval_in(None, "*print-length*", &[]);
    assert_eq!(texts(&m, "value"), ["nil"]);
    c.eval(&s, "(set! *warn-on-reflection* true)");
    assert_eq!(c.value(&s, "*warn-on-reflection*"), "true");
    assert_eq!(c.value(&t, "*warn-on-reflection*"), "false");
    c.eval(&s, "(set! *print-level* 1)");
    assert_eq!(c.value(&s, "[[1 [2]] [3]]"), "[# #]");
    c.eval(&s, "(set! *unchecked-math* true)");
    assert_eq!(c.value(&s, "*unchecked-math*"), "true");
    assert_eq!(c.value(&t, "*unchecked-math*"), "false");
    // set! inside binding changes the binding, not the session value
    c.eval(&s, "(set! *print-level* nil) (set! *print-length* 2)");
    c.eval(&s, "(binding [*print-length* 1] (set! *print-length* 5))");
    assert_eq!(c.value(&s, "(range 10)"), "(0 1 ...)");
    // the ROOT value was never touched
    let m = c.eval_in(None, "[*print-length* *print-level* *warn-on-reflection* *unchecked-math*]", &[]);
    assert_eq!(texts(&m, "value"), ["[nil nil false false]"]);
    // *ns* by set!
    c.eval(&t, "(set! *ns* (the-ns 'clojure.string))");
    assert_eq!(c.value(&t, "(str *ns*)"), "\"clojure.string\"");
    assert_eq!(c.value(&s, "(str *ns*)"), "\"user\"");
}

#[test]
fn value_printing_honors_print_length_and_level() {
    let f = Fixture::start();
    let mut c = f.client();
    let s = c.clone_session(None);
    c.eval(&s, "(set! *print-length* 2) (set! *print-level* 2)");
    assert_eq!(c.value(&s, "[1 2 3 [4 [5]]]"), "[1 2 ...]");
    assert_eq!(c.value(&s, "[[1 [2]] 3]"), "[[1 #] 3]");
}

// ---------------------------------------------------------------------------
// sessions
// ---------------------------------------------------------------------------

#[test]
fn defs_are_global_across_sessions_and_ephemeral() {
    let f = Fixture::start();
    let mut c = f.client();
    let s = c.clone_session(None);
    let t = c.clone_session(None);
    c.eval(&s, "(def abc-p2 41)");
    assert_eq!(c.value(&s, "(inc abc-p2)"), "42");
    assert_eq!(c.value(&t, "abc-p2"), "41");
    let m = c.eval_in(None, "abc-p2", &[]);
    assert_eq!(texts(&m, "value"), ["41"]);
    c.eval_in(None, "(defn f-p2 [x] (* 2 x))", &[]);
    assert_eq!(c.value(&s, "(f-p2 4)"), "8");
}

#[test]
fn ephemeral_requests_get_fresh_bindings_and_a_fresh_session_id_each() {
    let f = Fixture::start();
    let mut c = f.client();
    let a = c.eval_in(None, "(set! *print-length* 1) (range 5)", &[]);
    assert_eq!(texts(&a, "value"), ["1", "(0 ...)"]);
    let b = c.eval_in(None, "(range 5)", &[]);
    assert_eq!(texts(&b, "value"), ["(0 1 2 3 4)"]);
    // *1 does not carry over
    let m = c.eval_in(None, "*1", &[]);
    assert_eq!(texts(&m, "value"), ["nil"]);
    // ids differ between requests, and are the same inside one request
    let ids = |m: &[Msg]| m.iter().filter_map(|m| m.s("session")).collect::<std::collections::HashSet<_>>();
    assert_eq!(ids(&a).len(), 1);
    assert_ne!(ids(&a), ids(&b));
}

#[test]
fn ephemeral_requests_keep_at_most_one_thread_and_sessions_make_one_each() {
    let f = Fixture::start();
    let mut c = f.client();
    for _ in 0..20 {
        c.eval_in(None, "(+ 1 2)", &[]);
    }
    assert_eq!(f.backend.session_threads(), 0, "ephemeral requests make no session thread");
    assert_eq!(f.backend.pool_threads(), 1, "one idle worker is kept");
    // a session has no thread until its first eval
    let s = c.clone_session(None);
    assert_eq!(f.backend.session_threads(), 0);
    c.eval(&s, "1");
    assert_eq!(f.backend.session_threads(), 1);
    let t = c.clone_session(Some(&s));
    c.eval(&t, "1");
    assert_eq!(f.backend.session_threads(), 2);
    // close ends the thread
    c.close(&s);
    wait_until(|| f.backend.session_threads() == 1);
    c.close(&t);
    wait_until(|| f.backend.session_threads() == 0);
    // a closed session is unknown
    let id = c.send(&[("op", Value::str("eval")), ("code", Value::str("1")), ("session", Value::str(&s))]);
    let m = c.until_done(&id);
    assert_eq!(m[0].status(), ["done", "unknown-session", "error"]);
}

#[test]
fn concurrent_ephemeral_requests_do_not_wait_for_each_other() {
    let f = Fixture::start();
    let mut slow = f.client();
    let mut fast = f.client();
    let sid = slow.send(&[("op", Value::str("eval")), ("code", Value::str("(do (Thread/sleep 1500) :slow)"))]);
    std::thread::sleep(Duration::from_millis(100));
    let t0 = Instant::now();
    let m = fast.eval_in(None, ":fast", &[]);
    assert_eq!(texts(&m, "value"), [":fast"]);
    assert!(t0.elapsed() < Duration::from_millis(1000), "fast request waited {:?}", t0.elapsed());
    let m = slow.until_done(&sid);
    assert_eq!(texts(&m, "value"), [":slow"]);
    // the extra worker goes away again; one stays
    wait_until(|| f.backend.pool_threads() == 1);
}

#[test]
fn clone_copies_bindings_but_not_ns() {
    let f = Fixture::start();
    let mut c = f.client();
    let s = c.clone_session(None);
    c.eval(&s, "(set! *print-length* 2)");
    c.eval(&s, "(def cl-a 5)");
    c.eval(&s, "(in-ns 'clone.ns)");
    let d = c.clone_session(Some(&s));
    // `*1` is copied too (it is one of the session bindings): the parent's last value was the ns
    assert!(c.value(&d, "(clojure.core/str clojure.core/*1)").contains("clone.ns"));
    assert_eq!(c.value(&d, "(range 10)"), "(0 1 ...)");
    assert_eq!(c.value(&d, "(clojure.core/str clojure.core/*ns*)"), "\"user\"");
    assert_eq!(c.value(&d, "cl-a"), "5");
    // the parent is untouched
    assert_eq!(c.value(&s, "(clojure.core/str clojure.core/*ns*)"), "\"clone.ns\"");
    // a clone of a session with no eval yet gets the parent's (default) bindings
    let e = c.clone_session(Some(&s));
    let g = c.clone_session(Some(&e));
    assert_eq!(c.value(&g, "(range 10)"), "(0 1 ...)");
    // clone with no parent: fresh defaults
    let h = c.clone_session(None);
    assert_eq!(c.value(&h, "(range 3)"), "(0 1 2)");
}

#[test]
fn clone_sees_the_bindings_of_the_last_finished_eval() {
    // The snapshot is published before `done`, so a clone sent right after
    // `done` always sees the `set!` (no race).
    let f = Fixture::start();
    let mut c = f.client();
    let s = c.clone_session(None);
    for n in 1..30 {
        c.eval(&s, &format!("(set! *print-length* {n})"));
        let d = c.clone_session(Some(&s));
        assert_eq!(c.value(&d, "*print-length*"), n.to_string());
        c.close(&d);
    }
}

#[test]
fn evals_in_one_session_run_in_order() {
    let f = Fixture::start();
    let mut c = f.client();
    let s = c.clone_session(None);
    // send three at once; the first is slow
    let a = c.send(&[("op", Value::str("eval")), ("code", Value::str("(Thread/sleep 300) (def ord [:a])")), ("session", Value::str(&s))]);
    let b = c.send(&[("op", Value::str("eval")), ("code", Value::str("(def ord (conj ord :b))")), ("session", Value::str(&s))]);
    let d = c.send(&[("op", Value::str("eval")), ("code", Value::str("ord")), ("session", Value::str(&s))]);
    let mut done = vec![];
    let mut last_value = None;
    while done.len() < 3 {
        let m = c.recv().unwrap();
        if m.has_status("done") {
            done.push(m.s("id").unwrap());
        }
        if m.s("id").as_deref() == Some(d.as_str()) {
            if let Some(v) = m.s("value") {
                last_value = Some(v);
            }
        }
    }
    assert_eq!(done, [a, b, d]);
    assert_eq!(last_value.as_deref(), Some("[:a :b]"));
}

#[test]
fn two_sessions_run_at_the_same_time() {
    let f = Fixture::start();
    let mut c = f.client();
    let s = c.clone_session(None);
    let t = c.clone_session(None);
    let slow = c.send(&[("op", Value::str("eval")), ("code", Value::str("(do (Thread/sleep 1000) :slow)")), ("session", Value::str(&s))]);
    std::thread::sleep(Duration::from_millis(100));
    let t0 = Instant::now();
    assert_eq!(c.value(&t, ":fast"), ":fast");
    assert!(t0.elapsed() < Duration::from_millis(700));
    let m = c.until_done(&slow);
    assert_eq!(texts(&m, "value"), [":slow"]);
}

// ---------------------------------------------------------------------------
// out / err
// ---------------------------------------------------------------------------

#[test]
fn out_basic_rules() {
    let f = Fixture::start();
    let mut c = f.client();
    let s = c.clone_session(None);
    let t = |c: &mut Client, code: &str| trace(&c.eval(&s, code));
    assert_eq!(t(&mut c, r#"(println "a")"#), ["out:a\n", "v:nil", "status:done"]);
    // print waits for the end of the form
    assert_eq!(t(&mut c, r#"(print "a")"#), ["out:a", "v:nil", "status:done"]);
    // ... and is flushed before each form's value
    assert_eq!(t(&mut c, r#"(print "a") (print "b")"#), ["out:a", "v:nil", "out:b", "v:nil", "status:done"]);
    // `(flush)` sends at once
    assert_eq!(
        t(&mut c, r#"(print "a") (flush) (print "b")"#),
        ["out:a", "v:nil", "v:nil", "out:b", "v:nil", "status:done"]
    );
    // within one form, print output is joined; println sends what was printed before it
    assert_eq!(t(&mut c, r#"(do (print "a") (print "b") (println "c"))"#), ["out:abc\n", "v:nil", "status:done"]);
    assert_eq!(t(&mut c, r#"(doseq [i (range 5)] (print i))"#), ["out:01234", "v:nil", "status:done"]);
    assert_eq!(
        t(&mut c, r#"(doseq [i (range 3)] (println i))"#),
        ["out:0\n", "out:1\n", "out:2\n", "v:nil", "status:done"]
    );
    assert_eq!(t(&mut c, r#"(prn "x" :y)"#), ["out:\"x\" :y\n", "v:nil", "status:done"]);
    assert_eq!(t(&mut c, r#"(printf "%d-%s\n" 1 "z")"#), ["out:1-z\n", "v:nil", "status:done"]);
    assert_eq!(t(&mut c, r#"(.write *out* "w")"#), ["out:w", "v:nil", "status:done"]);
    assert_eq!(t(&mut c, r#"(println "multi\nline\nout")"#), ["out:multi\nline\nout\n", "v:nil", "status:done"]);
    assert_eq!(t(&mut c, r#"(println "")"#), ["out:\n", "v:nil", "status:done"]);
    assert_eq!(t(&mut c, r#"(print "") 1"#), ["v:nil", "v:1", "status:done"]);
    assert_eq!(t(&mut c, r#"(println "é ü 日本 😀")"#), ["out:é ü 日本 😀\n", "v:nil", "status:done"]);
    // output before an error is flushed before the error text
    let m = t(&mut c, r#"(print "a") (/ 1 0)"#);
    assert_eq!(&m[..2], ["out:a", "v:nil"]);
    assert!(m[2].starts_with("err:Execution error (ArithmeticException)"));
    // no newline at the very end: flushed before done
    assert_eq!(t(&mut c, r#"(do (print "tail") 1)"#), ["out:tail", "v:1", "status:done"]);
    // with-out-str is not captured by the server
    assert_eq!(t(&mut c, r#"(with-out-str (print "hid"))"#), ["v:\"hid\"", "status:done"]);
}

#[test]
fn err_stream_is_its_own_message_kind() {
    let f = Fixture::start();
    let mut c = f.client();
    let s = c.clone_session(None);
    let t = |c: &mut Client, code: &str| trace(&c.eval(&s, code));
    assert_eq!(t(&mut c, r#"(binding [*out* *err*] (println "e"))"#), ["err:e\n", "v:nil", "status:done"]);
    assert_eq!(t(&mut c, r#"(binding [*out* *err*] (print "e"))"#), ["err:e", "v:nil", "status:done"]);
    assert_eq!(
        t(&mut c, r#"(println "a") (binding [*out* *err*] (println "e")) (println "b") 1"#),
        ["out:a\n", "v:nil", "err:e\n", "v:nil", "out:b\n", "v:nil", "v:1", "status:done"]
    );
}

fn chunk_sizes(msgs: &[Msg], key: &str) -> Vec<usize> {
    msgs.iter().filter_map(|m| m.s(key)).map(|s| s.len()).collect()
}

#[test]
fn out_chunk_rule_is_1024_bytes_never_splitting_a_char() {
    let f = Fixture::start();
    let mut c = f.client();
    let s = c.clone_session(None);
    let sizes = |c: &mut Client, code: &str, key: &str| chunk_sizes(&c.eval(&s, code), key);
    assert_eq!(sizes(&mut c, r#"(print (apply str (repeat 5000 "a")))"#, "out"), [1024, 1024, 1024, 1024, 904]);
    assert_eq!(sizes(&mut c, r#"(println (apply str (repeat 5000 "b")))"#, "out"), [1024, 1024, 1024, 1024, 905]);
    assert_eq!(sizes(&mut c, r#"(print (apply str (repeat 1024 "c")))"#, "out"), [1024]);
    assert_eq!(sizes(&mut c, r#"(print (apply str (repeat 1025 "d")))"#, "out"), [1024, 1]);
    assert_eq!(sizes(&mut c, r#"(print (apply str (repeat 1023 "e")))"#, "out"), [1023]);
    assert_eq!(sizes(&mut c, r#"(dotimes [i 300] (print "ab"))"#, "out"), [600]);
    assert_eq!(sizes(&mut c, r#"(dotimes [i 600] (print "abc"))"#, "out"), [1024, 776]);
    // 3-byte chars: 342 of them are 1026 bytes: the char that crosses 1024 is finished
    assert_eq!(
        sizes(&mut c, r#"(print (apply str (repeat 3000 "日")))"#, "out"),
        [1026, 1026, 1026, 1026, 1026, 1026, 1026, 1026, 792]
    );
    assert_eq!(sizes(&mut c, r#"(print (apply str (repeat 1500 "é")))"#, "out"), [1024, 1024, 952]);
    assert_eq!(sizes(&mut c, r#"(binding [*out* *err*] (print (apply str (repeat 3000 "f"))))"#, "err"), [1024, 1024, 952]);
    // explicit flushes
    assert_eq!(
        sizes(&mut c, r#"(dotimes [i 2000] (print "x") (when (zero? (mod i 500)) (flush)))"#, "out"),
        [1, 500, 500, 500, 499]
    );
    assert_eq!(sizes(&mut c, r#"(print (apply str (repeat 2047 "h")))"#, "out"), [1024, 1023]);
    // and the pieces add up
    let m = c.eval(&s, r#"(dotimes [i 3000] (print i " "))"#);
    let all: String = texts(&m, "out").concat();
    let expect: String = (0..3000).map(|i| format!("{i}  ")).collect();
    assert_eq!(all, expect);
    assert!(chunk_sizes(&m, "out").iter().rev().skip(1).all(|n| *n == 1024));
}

#[test]
fn long_error_text_is_chunked_like_err_output() {
    let f = Fixture::start();
    let mut c = f.client();
    let s = c.clone_session(None);
    let m = c.eval(&s, r#"(throw (Exception. (apply str (repeat 3000 "m"))))"#);
    let sizes = chunk_sizes(&m, "err");
    assert!(sizes.len() >= 3 && sizes[..sizes.len() - 1].iter().all(|n| *n == 1024), "{sizes:?}");
    assert!(texts(&m, "err").concat().ends_with("mmm\n"));
    assert_eq!(m[sizes.len()].s("ex").as_deref(), Some("class java.lang.Exception"));
}

#[test]
fn output_from_a_future_after_done_carries_the_evals_id() {
    let f = Fixture::start();
    let mut c = f.client();
    let s = c.clone_session(None);
    let id = c.send(&[
        ("op", Value::str("eval")),
        ("code", Value::str(r#"(println "bg") (future (Thread/sleep 300) (println "after done") (binding [*out* *err*] (println "late-err")))"#)),
        ("session", Value::str(&s)),
    ]);
    let m = c.until_done(&id);
    assert_eq!(texts(&m, "out"), ["bg\n"]);
    // the future still runs: its output comes later, on the same id
    let late1 = c.recv().unwrap();
    assert_eq!(late1.s("out").as_deref(), Some("after done\n"));
    assert_eq!(late1.s("id").as_deref(), Some(id.as_str()));
    assert_eq!(late1.s("session").as_deref(), Some(s.as_str()));
    let late2 = c.recv().unwrap();
    assert_eq!(late2.s("err").as_deref(), Some("late-err\n"));
    assert_eq!(late2.s("id").as_deref(), Some(id.as_str()));
    // the next eval is not mixed up with it
    let m = c.eval(&s, r#"(println "next")"#);
    assert_eq!(trace(&m), ["out:next\n", "v:nil", "status:done"]);
}

#[test]
fn slow_reader_slows_the_evaluator_and_loses_nothing() {
    // The client does not read for a while; the eval thread must block (bounded
    // memory) and everything must still arrive, in order, once it reads.
    let f = Fixture::start();
    let mut c = f.client();
    let s = c.clone_session(None);
    let n = 60_000;
    let id = c.send(&[
        ("op", Value::str("eval")),
        ("code", Value::str(&format!(r#"(dotimes [i {n}] (println "line-with-some-padding-to-make-it-bigger" i))"#))),
        ("session", Value::str(&s)),
    ]);
    std::thread::sleep(Duration::from_millis(1500));
    let mut seen = 0usize;
    loop {
        let m = c.recv().unwrap();
        if m.s("id").as_deref() != Some(id.as_str()) {
            continue;
        }
        if let Some(o) = m.s("out") {
            assert_eq!(o, format!("line-with-some-padding-to-make-it-bigger {seen}\n"));
            seen += 1;
        }
        if m.has_status("done") {
            break;
        }
    }
    assert_eq!(seen, n);
}

// ---------------------------------------------------------------------------
// threads and CPU
// ---------------------------------------------------------------------------

fn wait_until(mut f: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while !f() {
        assert!(t0.elapsed() < Duration::from_secs(5), "condition not reached");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn cpu_time() -> Duration {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    let tv = |t: libc::timeval| Duration::new(t.tv_sec as u64, t.tv_usec as u32 * 1000);
    tv(ru.ru_utime) + tv(ru.ru_stime)
}

#[test]
fn idle_server_uses_no_cpu() {
    let f = Fixture::start();
    let mut c = f.client();
    let sessions: Vec<String> = (0..5).map(|_| c.clone_session(None)).collect();
    for s in &sessions {
        c.eval(s, "(+ 1 2)");
    }
    c.eval_in(None, "(+ 1 2)", &[]);
    std::thread::sleep(Duration::from_millis(200));
    let t0 = cpu_time();
    std::thread::sleep(Duration::from_millis(1500));
    let used = cpu_time() - t0;
    // 5 session threads, a pool worker, the IO thread and the boot thread are all parked
    assert!(used < Duration::from_millis(15), "idle CPU over 1.5 s: {used:?}");
}

#[test]
fn evals_still_work_after_an_eval_panics_or_errors_deeply() {
    let f = Fixture::start();
    let mut c = f.client();
    let s = c.clone_session(None);
    // deep recursion ends in a stack-overflow error, not a dead session
    let m = c.eval(&s, "(defn f [] (+ 1 (f))) (f)");
    assert!(m.iter().any(|m| m.has_status("eval-error")), "{:?}", trace(&m));
    assert_eq!(c.value(&s, "(+ 1 2)"), "3");
}

// ===========================================================================
// P3: interrupt, stdin, load-file, print and caught options
// ===========================================================================

impl Client {
    /// Sends an eval and does not wait.
    fn start_eval(&mut self, session: &str, code: &str) -> String {
        self.send(&[("op", Value::str("eval")), ("code", Value::str(code)), ("session", Value::str(session))])
    }

    fn interrupt(&mut self, session: &str, id: Option<&str>) -> Vec<Msg> {
        let mut items = vec![("op", Value::str("interrupt")), ("session", Value::str(session))];
        if let Some(i) = id {
            items.push(("interrupt-id", Value::str(i)));
        }
        let iid = self.send(&items);
        // everything up to the interrupt's own `done` (the `interrupted` reply has the eval's id)
        let mut out = Vec::new();
        loop {
            let m = self.recv().expect("connection closed");
            let done = m.s("id").as_deref() == Some(iid.as_str()) && m.has_status("done");
            out.push(m);
            if done {
                return out;
            }
        }
    }

    /// Waits for a reply that matches `f` (other replies are dropped).
    fn wait_for(&mut self, f: impl Fn(&Msg) -> bool) -> Msg {
        loop {
            let m = self.recv().expect("connection closed");
            if f(&m) {
                return m;
            }
        }
    }

    fn need_input(&mut self, id: &str) {
        let id = id.to_string();
        self.wait_for(|m| m.s("id").as_deref() == Some(id.as_str()) && m.has_status("need-input"));
    }
}

fn pause() {
    std::thread::sleep(Duration::from_millis(300));
}

#[test]
fn interrupt_a_sleep_gives_interrupted_then_the_error_and_no_second_done() {
    let f = Fixture::start();
    let mut c = f.client();
    let s = c.clone_session(None);
    let id = c.start_eval(&s, "(Thread/sleep 100000)");
    pause();
    let m = c.interrupt(&s, Some(&id));
    // `interrupted` carries the eval's id; the interrupt itself is answered with done
    assert!(m.len() == 2, "{:?}", trace(&m));
    assert_eq!(m[0].s("id").as_deref(), Some(id.as_str()));
    assert_eq!(m[0].status(), ["done", "interrupted"]);
    let rest: Vec<Msg> = {
        // the eval's own replies: err, ex (no done)
        let a = c.wait_for(|m| m.s("id").as_deref() == Some(id.as_str()) && m.s("err").is_some());
        let b = c.recv().unwrap();
        vec![a, b]
    };
    assert!(rest[0].s("err").unwrap().contains("InterruptedException"), "{:?}", rest[0]);
    assert!(rest[0].s("err").unwrap().ends_with("sleep interrupted\n"));
    assert_eq!(rest[1].s("ex").as_deref(), Some("class java.lang.InterruptedException"));
    assert_eq!(rest[1].status(), ["eval-error"]);
    // the session is fine, and the eval never sent `done`
    let m = c.eval(&s, "(+ 1 2)");
    assert_eq!(trace(&m), ["v:3", "status:done"]);
}

#[test]
fn interrupt_replies_for_every_case() {
    let f = Fixture::start();
    let mut c = f.client();
    let s = c.clone_session(None);
    // idle: with and without an id
    assert_eq!(c.interrupt(&s, None)[0].status(), ["done", "session-idle"]);
    assert_eq!(c.interrupt(&s, Some("zzz"))[0].status(), ["done", "session-idle"]);
    // wrong id while running: nothing is interrupted
    let id = c.start_eval(&s, "(do (Thread/sleep 1500) :finished)");
    pause();
    assert_eq!(c.interrupt(&s, Some("nope"))[0].status(), ["done", "interrupt-id-mismatch", "error"]);
    let m = c.until_done(&id);
    assert_eq!(texts(&m, "value"), [":finished"]);
    // no session: ephemeral
    let iid = c.send(&[("op", Value::str("interrupt")), ("interrupt-id", Value::str("1"))]);
    let m = c.until_done(&iid);
    assert_eq!(m[0].status(), ["session-ephemeral", "done", "error"]);
    // unknown session is the router's answer
    let iid = c.send(&[("op", Value::str("interrupt")), ("session", Value::str("no-such")), ("interrupt-id", Value::str("1"))]);
    assert_eq!(c.until_done(&iid)[0].status(), ["done", "unknown-session", "error"]);
    // no id means the running eval
    let id = c.start_eval(&s, "(Thread/sleep 100000)");
    pause();
    let m = c.interrupt(&s, None);
    assert_eq!(m.len(), 2);
    c.wait_for(|m| m.s("id").as_deref() == Some(id.as_str()) && m.has_status("eval-error"));
    assert_eq!(c.value(&s, "1"), "1");
}

#[test]
fn interrupt_drops_the_evals_queued_behind_it() {
    let f = Fixture::start();
    let mut c = f.client();
    let s = c.clone_session(None);
    let a = c.start_eval(&s, "(Thread/sleep 100000)");
    pause();
    let b = c.start_eval(&s, "(+ 1 2)");
    let _d = c.start_eval(&s, "(+ 3 4)");
    pause();
    // an id that is not the running one is refused
    assert_eq!(c.interrupt(&s, Some(&b))[0].status(), ["done", "interrupt-id-mismatch", "error"]);
    c.interrupt(&s, Some(&a));
    c.wait_for(|m| m.s("id").as_deref() == Some(a.as_str()) && m.has_status("eval-error"));
    // the queued ones get no reply; a new eval works
    let m = c.eval(&s, "(+ 10 20)");
    assert_eq!(texts(&m, "value"), ["30"]);
    std::thread::sleep(Duration::from_millis(200));
    // nothing more arrives for b or d (the connection is quiet)
    c.s.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
    let mut buf = [0u8; 16];
    assert!(c.s.read(&mut buf).is_err(), "no reply expected for queued evals");
}

#[test]
fn interrupt_stops_loops_silently_and_catch_can_swallow_a_sleep_interrupt() {
    let f = Fixture::start();
    let mut c = f.client();
    let s = c.clone_session(None);
    for code in ["(loop [] (recur))", "(defn spin [n] (loop [i 0] (if (< i n) (recur (inc i)) i))) (spin 1000000000000)", "(dorun (iterate inc 0))"] {
        let id = c.start_eval(&s, code);
        pause();
        let m = c.interrupt(&s, Some(&id));
        assert_eq!(m.iter().filter(|m| m.has_status("done")).count(), 2, "{code}");
        // no err / ex: a loop ends like the JVM's ThreadDeath
        assert_eq!(c.value(&s, "(+ 1 2)"), "3", "{code}");
    }
    let id = c.start_eval(&s, "(try (Thread/sleep 100000) (catch InterruptedException e :caught))");
    pause();
    c.interrupt(&s, Some(&id));
    let v = c.wait_for(|m| m.s("id").as_deref() == Some(id.as_str()) && m.s("value").is_some());
    assert_eq!(v.s("value").as_deref(), Some(":caught"));
    // finally runs
    let id = c.start_eval(&s, r#"(try (Thread/sleep 100000) (finally (println "fin")))"#);
    pause();
    c.interrupt(&s, Some(&id));
    let o = c.wait_for(|m| m.s("id").as_deref() == Some(id.as_str()) && m.s("out").is_some());
    assert_eq!(o.s("out").as_deref(), Some("fin\n"));
}

#[test]
fn interrupt_stops_natives_that_consume_a_huge_range() {
    let f = Fixture::start();
    let mut c = f.client();
    let s = c.clone_session(None);
    for code in [
        "(vec (range 100000000))",
        "(count (range 1e12))",
        "(reduce + (range 100000000000))",
        "(doall (range 1e9))",
        "(into [] (range 1e9))",
        "(apply + (range 1e9))",
        "(sort (range 1e8))",
        "(clojure.string/join (range 1e8))",
        "(count (repeat 1000000000 1))",
        "(count (map inc (range 1e10)))",
    ] {
        let id = c.start_eval(&s, code);
        // 50 ms by hand; 150 ms here so a loaded CI box has started the eval
        std::thread::sleep(Duration::from_millis(150));
        let t = Instant::now();
        let m = c.interrupt(&s, Some(&id));
        assert!(m.iter().any(|m| m.has_status("interrupted")), "{code}: {m:?}");
        assert_eq!(c.value(&s, "(+ 1 2)"), "3", "{code}");
        assert!(t.elapsed() < Duration::from_millis(500), "{code}: session free after {:?}", t.elapsed());
    }
}

#[test]
fn interrupt_wakes_blocked_waits_without_polling() {
    let f = Fixture::start();
    let mut c = f.client();
    let s = c.clone_session(None);
    c.eval(&s, "(def prm (promise)) (def ch (chan)) (def full (chan 1)) (>!! full 1)");
    for code in ["@prm", "(<!! ch)", "(>!! full 2)", "@(future (Thread/sleep 100000))", "(locking :a (Thread/sleep 100000))"] {
        let id = c.start_eval(&s, code);
        pause();
        let t0 = Instant::now();
        c.interrupt(&s, Some(&id));
        let freed = c.value(&s, "(+ 1 2)");
        assert_eq!(freed, "3", "{code}");
        assert!(t0.elapsed() < Duration::from_millis(400), "{code} took {:?} to free the session", t0.elapsed());
    }
}

#[test]
fn interrupt_frees_an_eval_blocked_by_a_slow_client() {
    let f = Fixture::start();
    let mut slow = f.client();
    let mut ctl = f.client();
    let s = slow.clone_session(None);
    // lots of output, and this client does not read it
    slow.start_eval(&s, r#"(dotimes [i 2000000] (println "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"))"#);
    std::thread::sleep(Duration::from_millis(800));
    // the evaluator must be blocked by backpressure now; interrupt it from another connection
    let t0 = Instant::now();
    let m = ctl.interrupt(&s, None);
    assert!(m.iter().any(|m| m.has_status("interrupted")), "{:?}", trace(&m));
    // read everything: the stream ends and the session answers again
    let id = slow.start_eval(&s, "(+ 1 2)");
    let v = slow.wait_for(|m| m.s("id").as_deref() == Some(id.as_str()) && m.s("value").is_some());
    assert_eq!(v.s("value").as_deref(), Some("3"));
    assert!(t0.elapsed() < Duration::from_secs(10), "{:?}", t0.elapsed());
}

#[test]
fn interrupt_does_not_leave_a_stale_flag_for_the_next_eval() {
    let f = Fixture::start();
    let mut c = f.client();
    let s = c.clone_session(None);
    for _ in 0..5 {
        let id = c.start_eval(&s, "(Thread/sleep 100000)");
        pause();
        c.interrupt(&s, Some(&id));
        // an eval that is interrupted just as it ends must not poison the next one
        assert_eq!(c.value(&s, "(reduce + (range 1000))"), "499500");
    }
}

// ---- stdin ----

fn stdin_op(c: &mut Client, s: &str, data: Option<&str>) {
    let mut items = vec![("op", Value::str("stdin")), ("session", Value::str(s))];
    if let Some(d) = data {
        items.push(("stdin", Value::str(d)));
    }
    let id = c.send(&items);
    let m = c.until_done(&id);
    assert_eq!(m[0].status(), ["done"]);
}

#[test]
fn stdin_read_line_read_and_dot_read() {
    let f = Fixture::start();
    let mut c = f.client();
    let s = c.clone_session(None);
    let id = c.start_eval(&s, "(read-line)");
    c.need_input(&id);
    stdin_op(&mut c, &s, Some("hello\n"));
    let m = c.until_done(&id);
    assert_eq!(texts(&m, "value"), ["\"hello\""]);
    // (read) reads a form
    let id = c.start_eval(&s, "(read)");
    c.need_input(&id);
    stdin_op(&mut c, &s, Some("(1 2)\n"));
    assert_eq!(texts(&c.until_done(&id), "value"), ["(1 2)"]);
    // data sent before the read is buffered; no need-input then
    stdin_op(&mut c, &s, Some("buffered\n"));
    assert_eq!(c.value(&s, "(read-line)"), "\"buffered\"");
    // `.read`
    let id = c.start_eval(&s, "(.read *in*)");
    c.need_input(&id);
    stdin_op(&mut c, &s, Some("A"));
    assert_eq!(texts(&c.until_done(&id), "value"), ["65"]);
}

#[test]
fn stdin_partial_line_asks_again_and_two_lines_in_one_message() {
    let f = Fixture::start();
    let mut c = f.client();
    let s = c.clone_session(None);
    let id = c.start_eval(&s, "[(read-line) (read-line)]");
    c.need_input(&id);
    stdin_op(&mut c, &s, Some("l1\nl2\n"));
    assert_eq!(texts(&c.until_done(&id), "value"), ["[\"l1\" \"l2\"]"]);
    let id = c.start_eval(&s, "(read-line)");
    c.need_input(&id);
    stdin_op(&mut c, &s, Some("partial"));
    // a second need-input: the line is not finished
    c.need_input(&id);
    stdin_op(&mut c, &s, Some(" line\n"));
    assert_eq!(texts(&c.until_done(&id), "value"), ["\"partial line\""]);
}

#[test]
fn stdin_eof_is_read_once_and_slurp_closes_the_stream() {
    let f = Fixture::start();
    let mut c = f.client();
    let s = c.clone_session(None);
    // no data / empty data is end of input for exactly one read
    stdin_op(&mut c, &s, None);
    assert_eq!(c.value(&s, "(read-line)"), "nil");
    let id = c.start_eval(&s, "(read-line)");
    c.need_input(&id);
    stdin_op(&mut c, &s, Some("after\n"));
    assert_eq!(texts(&c.until_done(&id), "value"), ["\"after\""]);
    // slurp reads to EOF, then the stream is closed (as with `with-open`)
    let id = c.start_eval(&s, "(slurp *in*)");
    c.need_input(&id);
    stdin_op(&mut c, &s, Some(""));
    assert_eq!(texts(&c.until_done(&id), "value"), ["\"\""]);
    let m = c.eval(&s, "(read-line)");
    assert!(texts(&m, "err")[0].contains("Stream closed"), "{:?}", trace(&m));
    assert_eq!(c.value(&s, "(+ 1 2)"), "3");
    // an ephemeral request has no input: end of input at once
    let m = c.eval_in(None, "(read-line)", &[]);
    assert_eq!(texts(&m, "value"), ["nil"]);
    // another session has its own `*in*`
    let t = c.clone_session(None);
    let id = c.start_eval(&t, "(read-line)");
    c.need_input(&id);
    stdin_op(&mut c, &s, Some("for-s\n")); // s is not reading: buffered for s only
    stdin_op(&mut c, &t, Some("for-t\n"));
    assert_eq!(texts(&c.until_done(&id), "value"), ["\"for-t\""]);
}

#[test]
fn a_read_blocked_on_stdin_is_interruptible_and_idle_cpu_stays_zero() {
    let f = Fixture::start();
    let mut c = f.client();
    let s = c.clone_session(None);
    let t = c.clone_session(None);
    c.eval(&s, "(def prm (promise))");
    let a = c.start_eval(&s, "@prm");
    let b = c.start_eval(&t, "(read-line)");
    c.need_input(&b);
    std::thread::sleep(Duration::from_millis(300));
    let t0 = cpu_time();
    std::thread::sleep(Duration::from_millis(1500));
    let used = cpu_time() - t0;
    assert!(used < Duration::from_millis(15), "CPU while blocked on @promise and read-line: {used:?}");
    c.interrupt(&t, Some(&b));
    assert_eq!(c.value(&t, "(+ 1 2)"), "3");
    c.interrupt(&s, Some(&a));
    assert_eq!(c.value(&s, "(+ 1 2)"), "3");
}

// ---- load-file ----

fn load_file(c: &mut Client, session: Option<&str>, file: Option<&str>, extra: &[(&str, Value)]) -> Vec<Msg> {
    let mut items = vec![("op", Value::str("load-file"))];
    if let Some(s) = session {
        items.push(("session", Value::str(s)));
    }
    if let Some(f) = file {
        items.push(("file", Value::str(f)));
    }
    items.extend(extra.iter().cloned());
    let id = c.send(&items);
    c.until_done(&id)
}

#[test]
fn load_file_sends_one_value_without_ns_and_keeps_the_session_ns() {
    let f = Fixture::start();
    let mut c = f.client();
    let s = c.clone_session(None);
    let m = load_file(&mut c, Some(&s), Some("(ns lf.a)\n(defn f [] 1)\n(f)"), &[("file-name", Value::str("a.clj")), ("file-path", Value::str("/t/a.clj"))]);
    assert_eq!(trace(&m), ["v:1", "status:done"]);
    assert!(m[0].s("ns").is_none());
    assert_eq!(c.value(&s, "(lf.a/f)"), "1");
    assert_eq!(c.value(&s, "(str *ns*)"), "\"user\"");
    assert_eq!(c.value(&s, "(select-keys (meta #'lf.a/f) [:file :line])"), "{:file \"/t/a.clj\", :line 2}");
    // files with several forms, a print of the last value only, out before it
    let m = load_file(&mut c, Some(&s), Some("(println \"in-file\") (+ 1 2)"), &[("file-path", Value::str("/x/y/p.clj"))]);
    assert_eq!(trace(&m), ["out:in-file\n", "v:3", "status:done"]);
    // `in-ns` in a file does not move the session
    load_file(&mut c, Some(&s), Some("(in-ns 'lf.inns)"), &[]);
    assert_eq!(c.value(&s, "(str *ns*)"), "\"user\"");
    // no session: works, ephemeral
    let m = load_file(&mut c, None, Some("(+ 1 2)"), &[]);
    assert_eq!(texts(&m, "value"), ["3"]);
}

#[test]
fn load_file_stops_at_the_first_error_and_reports_places() {
    let f = Fixture::start();
    let mut c = f.client();
    let s = c.clone_session(None);
    let m = load_file(&mut c, Some(&s), Some("(ns lf.b)\n(def x 1)\n(/ 1 0)\n(def y 2)"), &[("file-name", Value::str("b.clj")), ("file-path", Value::str("/t/b.clj"))]);
    let t = trace(&m);
    assert!(t[0].starts_with("err:Execution error (ArithmeticException) at lf.b/eval") || t[0].starts_with("err:Execution error (ArithmeticException) at user/eval"), "{t:?}");
    assert!(t[0].contains("(b.clj:3)"), "{t:?}");
    assert_eq!(t[1], "ex");
    assert_eq!(t[2], "status:done");
    assert_eq!(m.len(), 3);
    assert_eq!(c.value(&s, "lf.b/x"), "1");
    assert_eq!(c.value(&s, "(resolve 'lf.b/y)"), "nil");
    // compile errors use the full path; read errors say REPL
    let m = load_file(&mut c, Some(&s), Some("(ns lf.c)\n(def v (foo-unresolved))"), &[("file-path", Value::str("/t/c.clj"))]);
    assert!(texts(&m, "err")[0].starts_with("Syntax error compiling at (/t/c.clj:2:"), "{:?}", texts(&m, "err"));
    let m = load_file(&mut c, Some(&s), Some("(+ 1"), &[]);
    assert!(texts(&m, "err")[0].starts_with("Syntax error reading source at (REPL:2:1)"), "{:?}", texts(&m, "err"));
    // no `file`: no-code
    let m = load_file(&mut c, Some(&s), None, &[]);
    assert_eq!(m[0].status(), ["done", "no-code", "error"]);
    // line / column meta
    let m = load_file(
        &mut c,
        Some(&s),
        Some("(def lfv 1)\n(def lfw 2)\n(map #(select-keys (meta %) [:file :line :column]) [#'lfv #'lfw])"),
        &[("file-path", Value::str("/p/d.clj"))],
    );
    assert_eq!(
        texts(&m, "value"),
        ["({:file \"/p/d.clj\", :line 1, :column 1} {:file \"/p/d.clj\", :line 2, :column 1})"]
    );
}

// ---- print and caught options ----

fn pk(k: &str) -> String {
    format!("nrepl.middleware.print/{k}")
}

#[test]
fn print_option_calls_the_fn_with_value_writer_options() {
    let f = Fixture::start();
    let mut c = f.client();
    let s = c.clone_session(None);
    c.eval(&s, "(defn my-print [v w o] (.write w (str \"<\" (pr-str v) \"|\" (pr-str o) \">\")))");
    let m = c.eval_in(Some(&s), "{:a 1}", &[("nrepl.middleware.print/print", Value::str("user/my-print")), ("nrepl.middleware.print/options", Value::dict([("width", int(20))]))]);
    assert_eq!(texts(&m, "value"), ["<{:a 1}|{:width 20}>"]);
    // pr-str / prn with 3 args print nothing into the writer: value is ""
    let m = c.eval_in(Some(&s), "{:a 1}", &[("nrepl.middleware.print/print", Value::str("clojure.core/pr-str"))]);
    assert_eq!(texts(&m, "value"), [""]);
    // unresolved: an error message first, then default printing
    let m = c.eval_in(Some(&s), "{:a 1}", &[("nrepl.middleware.print/print", Value::str("no.such/fn"))]);
    assert_eq!(m[0].s("nrepl.middleware.print/error").as_deref(), Some("Couldn't resolve var no.such/fn"));
    assert_eq!(m[0].status(), ["nrepl.middleware.print/error"]);
    assert_eq!(texts(&m, "value"), ["{:a 1}"]);
}

#[test]
fn print_stream_buffer_size_and_truthiness() {
    let f = Fixture::start();
    let mut c = f.client();
    let s = c.clone_session(None);
    let m = c.eval_in(Some(&s), "(range 50)", &[(&pk("stream?"), Value::str("1")), (&pk("buffer-size"), int(10))]);
    let chunks = texts(&m, "value");
    assert_eq!(&chunks[..3], ["(0 1 2 3 4", " 5 6 7 8 9", " 10 11 12 "]);
    assert_eq!(chunks.concat(), format!("({})", (0..50).map(|i| i.to_string()).collect::<Vec<_>>().join(" ")));
    // a lone {ns} after the chunks
    let n = m.len();
    assert_eq!(m[n - 2].s("ns").as_deref(), Some("user"));
    assert!(m[n - 2].s("value").is_none());
    // buffer-size 1 and 0 go byte by byte; a multi-byte char is never torn
    let m = c.eval_in(Some(&s), "\"é日本\"", &[(&pk("stream?"), Value::str("1")), (&pk("buffer-size"), int(3))]);
    assert_eq!(texts(&m, "value"), ["\"é", "日", "本\""]);
    // any string is true, even "" and "false"; only the empty list is false
    for v in ["", "0", "false"] {
        let m = c.eval_in(Some(&s), "(range 3)", &[(&pk("stream?"), Value::str(v))]);
        assert!(m.iter().any(|m| m.s("ns").is_some() && m.s("value").is_none()), "stream? {v:?}");
    }
    let m = c.eval_in(Some(&s), "(range 3)", &[(&pk("stream?"), Value::List(vec![]))]);
    assert_eq!(texts(&m, "value"), ["(0 1 2)"]);
}

#[test]
fn print_quota_truncates_and_says_so() {
    let f = Fixture::start();
    let mut c = f.client();
    let s = c.clone_session(None);
    let m = c.eval_in(Some(&s), "(range 100)", &[(&pk("quota"), int(20))]);
    assert_eq!(texts(&m, "value"), ["(0 1 2 3 4 5 6 7 8 9"]);
    assert_eq!(m[0].status(), ["nrepl.middleware.print/truncated"]);
    assert_eq!(m[0].0.get("nrepl.middleware.print/truncated-keys"), Some(&Value::List(vec![Value::str("value")])));
    // exactly the quota is not truncated; one more is
    let m = c.eval_in(Some(&s), "(range 5)", &[(&pk("quota"), int(11))]);
    assert_eq!(trace(&m), ["v:(0 1 2 3 4)", "status:done"]);
    let m = c.eval_in(Some(&s), "(range 5)", &[(&pk("quota"), int(10))]);
    assert_eq!(texts(&m, "value"), ["(0 1 2 3 4"]);
    // characters, not bytes
    let m = c.eval_in(Some(&s), "\"日本日本日本\"", &[(&pk("quota"), int(5))]);
    assert_eq!(texts(&m, "value"), ["\"日本日本"]);
    // streamed: the prefix, then a status message, then the {ns}
    let m = c.eval_in(Some(&s), "(range 100)", &[(&pk("quota"), int(20)), (&pk("stream?"), Value::str("1"))]);
    let t = trace(&m);
    assert_eq!(t.len(), 4, "{t:?}");
    assert_eq!(t[0], "v:(0 1 2 3 4 5 6 7 8 9");
    assert_eq!(t[1], "status:nrepl.middleware.print/truncated");
    assert_eq!(m[2].s("ns").as_deref(), Some("user"));
    // an invalid quota is a print error
    let m = c.eval_in(Some(&s), "(range 100)", &[(&pk("quota"), int(0))]);
    assert!(texts(&m, "err")[0].contains("Invalid quota: 0"), "{:?}", trace(&m));
    assert!(m.iter().any(|m| m.has_status("eval-error")));
}

#[test]
fn print_keys_that_are_not_a_list_get_no_reply() {
    let f = Fixture::start();
    let mut c = f.client();
    let s = c.clone_session(None);
    let _ = c.send(&[("op", Value::str("eval")), ("code", Value::str("{:a 1}")), ("session", Value::str(&s)), ("nrepl.middleware.print/keys", Value::str("value"))]);
    // the session is not stuck
    assert_eq!(c.value(&s, "(+ 1 2)"), "3");
}

#[test]
fn caught_option_custom_fn_and_print_flag() {
    let f = Fixture::start();
    let mut c = f.client();
    let s = c.clone_session(None);
    // print? true: the printed throwable rides on the ex message
    let m = c.eval_in(Some(&s), "(/ 1 0)", &[("nrepl.middleware.caught/print?", Value::str("1"))]);
    let ex = m.iter().find(|m| m.s("ex").is_some()).unwrap();
    assert!(ex.s("nrepl.middleware.caught/throwable").unwrap().contains("Divide by zero"));
    assert!(texts(&m, "err")[0].contains("Divide by zero"));
    // an empty list is false
    let m = c.eval_in(Some(&s), "(/ 1 0)", &[("nrepl.middleware.caught/print?", Value::List(vec![]))]);
    assert!(m.iter().find(|m| m.s("ex").is_some()).unwrap().s("nrepl.middleware.caught/throwable").is_none());
    // a custom fn replaces the err text
    c.eval(&s, "(defn my-caught [e] (println \"caught:\" (ex-message e)))");
    let m = c.eval_in(Some(&s), "(/ 1 0)", &[("nrepl.middleware.caught/caught", Value::str("user/my-caught"))]);
    assert_eq!(texts(&m, "err").len(), 0);
    assert_eq!(texts(&m, "out"), ["caught: Divide by zero\n"]);
    assert!(m.iter().any(|m| m.has_status("eval-error")));
    // unresolved: an error message, then the default text
    let m = c.eval_in(Some(&s), "(/ 1 0)", &[("nrepl.middleware.caught/caught", Value::str("no.such/fn"))]);
    assert_eq!(m[0].s("nrepl.middleware.caught/error").as_deref(), Some("Couldn't resolve var no.such/fn"));
    assert!(texts(&m, "err")[0].contains("Divide by zero"));
    // quota applies to the throwable
    let m = c.eval_in(Some(&s), "(/ 1 0)", &[("nrepl.middleware.caught/print?", Value::str("1")), (&pk("quota"), int(5))]);
    let ex = m.iter().find(|m| m.s("ex").is_some()).unwrap();
    assert_eq!(ex.s("nrepl.middleware.caught/throwable").unwrap().chars().count(), 5);
    assert!(ex.has_status("nrepl.middleware.print/truncated") && ex.has_status("eval-error"));
}

#[test]
fn eval_param_names_the_fn_that_evaluates_each_form() {
    let f = Fixture::start();
    let mut c = f.client();
    let s = c.clone_session(None);
    // identity "evaluates" to the form itself
    let m = c.eval_in(Some(&s), "(+ 1 2)", &[("eval", Value::str("clojure.core/identity"))]);
    assert_eq!(texts(&m, "value"), ["(+ 1 2)"]);
    // unresolved: plain eval
    let m = c.eval_in(Some(&s), "(+ 1 2)", &[("eval", Value::str("no.such/eval"))]);
    assert_eq!(texts(&m, "value"), ["3"]);
}
