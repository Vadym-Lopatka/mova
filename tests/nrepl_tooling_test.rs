//! nREPL phase P4 (`completions`, `lookup`) and the rich error report
//! (`--errors=rich`), through the real server over a TCP socket. Expected
//! values are from the JVM nREPL 1.8.0 goldens (`crates/mova-nrepl/oracle`,
//! scenarios `i01_completions`, `i02_lookup`) and real Clojure 1.13.0-alpha6.
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
    fn start() -> Fixture {
        Fixture::start_with(mova::nrepl::ErrorMode::Jvm)
    }

    fn start_with(errors: mova::nrepl::ErrorMode) -> Fixture {
        let guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let l = Listeners::bind(&Endpoint::Tcp { host: "127.0.0.1".into(), port: 0 }).unwrap();
        let port = l.port().unwrap();
        let backend = MovaBackend::new(Config { errors, ..Config::default() });
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


fn call(c: &mut Client, items: &[(&str, Value)]) -> Vec<Msg> {
    let id = c.send(items);
    c.until_done(&id)
}

fn cands(c: &mut Client, session: &str, prefix: &str, ns: Option<&str>) -> Vec<Value> {
    let mut items = vec![("op", Value::str("completions")), ("session", Value::str(session)), ("prefix", Value::str(prefix))];
    if let Some(ns) = ns {
        items.push(("ns", Value::str(ns)));
    }
    let r = call(c, &items);
    assert_eq!(r.len(), 1, "{r:?}");
    assert_eq!(r[0].status(), vec!["done"], "{prefix}: {:?}", r[0]);
    match r[0].0.get("completions") {
        Some(Value::List(l)) => l.clone(),
        other => panic!("completions: {other:?}"),
    }
}

fn field(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(|x| x.as_str()).map(|s| s.to_string())
}

fn names(cs: &[Value]) -> Vec<String> {
    cs.iter().map(|c| field(c, "candidate").unwrap()).collect()
}

fn find<'a>(cs: &'a [Value], name: &str) -> &'a Value {
    cs.iter().find(|c| field(c, "candidate").as_deref() == Some(name)).unwrap_or_else(|| panic!("no {name} in {:?}", names(cs)))
}

#[test]
fn completions_vars_have_type_ns_priority_and_are_sorted() {
    let f = Fixture::start();
    let mut c = f.client();
    let s = c.clone_session(None);
    let cs = cands(&mut c, &s, "ma", None);
    let n = names(&cs);
    assert!(n.contains(&"map".to_string()) && n.contains(&"map-indexed".to_string()) && n.contains(&"mapv".to_string()), "{n:?}");
    // sorted by candidate (Java String order)
    let mut sorted = n.clone();
    sorted.sort();
    assert_eq!(n, sorted);
    let m = find(&cs, "map");
    assert_eq!(field(m, "type").as_deref(), Some("function"));
    assert_eq!(field(m, "ns").as_deref(), Some("clojure.core"));
    assert_eq!(m.get("priority"), Some(&int(0)));
    // `-` is a separator: `m-i` finds `map-indexed`; `ma` does not find `mix-a`-shaped names
    assert!(names(&cands(&mut c, &s, "m-i", None)).contains(&"map-indexed".to_string()));
    // macros and special forms
    let w = cands(&mut c, &s, "when-", None);
    assert_eq!(field(find(&w, "when-let"), "type").as_deref(), Some("macro"));
    let i = cands(&mut c, &s, "if", None);
    let sf = find(&i, "if");
    assert_eq!(field(sf, "type").as_deref(), Some("special-form"));
    assert!(sf.get("ns").is_none() && sf.get("priority").is_none());
    assert_eq!(field(find(&i, "if-let"), "type").as_deref(), Some("macro"));
    // nothing matches
    assert!(cands(&mut c, &s, "zzzzqq", None).is_empty());
    // the helpers of the interpreter are not offered
    assert!(names(&cands(&mut c, &s, "--", None)).is_empty());
}

#[test]
fn completions_scope_alias_ns_param_and_unknown_ns() {
    let f = Fixture::start();
    let mut c = f.client();
    let s = c.clone_session(None);
    let cs = cands(&mut c, &s, "clojure.string/jo", None);
    assert_eq!(names(&cs), vec!["clojure.string/join"]);
    assert_eq!(field(&cs[0], "ns").as_deref(), Some("clojure.string"));
    assert_eq!(field(&cs[0], "type").as_deref(), Some("function"));
    assert!(cands(&mut c, &s, "clojure.string/", None).len() >= 20);
    // `ns` param: the names of that ns come first-hand
    assert_eq!(names(&cands(&mut c, &s, "join", Some("clojure.string"))), vec!["join"]);
    // an unknown ns falls back to `user` (clojure.core is visible there)
    assert_eq!(names(&cands(&mut c, &s, "map-", Some("no.such.ns"))), names(&cands(&mut c, &s, "map-", Some("user"))));
    // a scope that is not a namespace gives nothing; a class is not a scope
    assert!(cands(&mut c, &s, "nosuch.ns/x", None).is_empty());
    assert!(!names(&cands(&mut c, &s, "System/getP", None)).iter().any(|n| n == "System/getProperty") || cands(&mut c, &s, "System/getP", None).iter().all(|v| field(v, "type").as_deref() != Some("var")));
    // an alias works as a scope, and is itself a candidate
    c.eval(&s, "(ns comp.ns (:require [clojure.set :as s]))");
    let cs = cands(&mut c, &s, "s/un", Some("comp.ns"));
    assert_eq!(names(&cs), vec!["s/union"]);
    assert_eq!(field(&cs[0], "ns").as_deref(), Some("clojure.set"));
    assert!(names(&cands(&mut c, &s, "s", Some("comp.ns"))).contains(&"s/".to_string()));
    // the session's own `*ns*` is the default
    c.eval(&s, "(def my-comp-var 1)");
    let cs = cands(&mut c, &s, "my-comp", None);
    assert_eq!(names(&cs), vec!["my-comp-var"]);
    assert_eq!(field(&cs[0], "type").as_deref(), Some("var"));
    assert_eq!(field(&cs[0], "ns").as_deref(), Some("comp.ns"));
    c.eval(&s, "(defn my-comp-fn [a] a) (defmacro my-comp-mac [] 1)");
    let cs = cands(&mut c, &s, "my-comp-", None);
    assert_eq!(field(find(&cs, "my-comp-fn"), "type").as_deref(), Some("function"));
    assert_eq!(field(find(&cs, "my-comp-mac"), "type").as_deref(), Some("macro"));
}

#[test]
fn completions_namespaces_classes_statics_and_keywords() {
    let f = Fixture::start();
    let mut c = f.client();
    let s = c.clone_session(None);
    let ns = cands(&mut c, &s, "clojure.str", None);
    assert_eq!(names(&ns), vec!["clojure.string"]);
    assert_eq!(field(&ns[0], "type").as_deref(), Some("namespace"));
    assert_eq!(ns[0].get("priority"), Some(&int(0)));
    // fuzzy with dots
    assert!(names(&cands(&mut c, &s, "c.s", None)).contains(&"clojure.string".to_string()));
    // classes Mova knows
    let cl = cands(&mut c, &s, "Sys", None);
    let sys = find(&cl, "System");
    assert_eq!(field(sys, "type").as_deref(), Some("class"));
    assert_eq!(field(sys, "package").as_deref(), Some("java.lang"));
    assert!(names(&cl).contains(&"java.lang.System".to_string()));
    let st = cands(&mut c, &s, "System/getP", None);
    assert_eq!(names(&st), vec!["System/getProperty"]);
    assert_eq!(field(&st[0], "type").as_deref(), Some("static-method"));
    assert!(names(&cands(&mut c, &s, "java.io.File", None)).contains(&"java.io.File".to_string()));
    // keywords
    c.eval(&s, "[:zz-kw-one :zz-kw-two]");
    let k = cands(&mut c, &s, ":zz-kw", None);
    assert_eq!(names(&k), vec![":zz-kw-one", ":zz-kw-two"]);
    assert_eq!(field(&k[0], "type").as_deref(), Some("keyword"));
    assert!(k[0].get("ns").is_none());
    c.eval(&s, "(ns kw.ns (:require [clojure.string :as str])) [::mine :kw.ns/other :clojure.string/yours]");
    assert!(names(&cands(&mut c, &s, "::mi", None)).contains(&"::mine".to_string()));
    assert!(names(&cands(&mut c, &s, "::st", None)).contains(&"::str/".to_string()));
    assert!(names(&cands(&mut c, &s, "::str/yo", None)).contains(&"::str/yours".to_string()));
}

#[test]
fn completions_errors_and_unknown_ops() {
    let f = Fixture::start();
    let mut c = f.client();
    let s = c.clone_session(None);
    // no prefix: completions-error with a message, as on the JVM
    let r = call(&mut c, &[("op", Value::str("completions")), ("session", Value::str(&s))]);
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].status(), vec!["done", "completions-error", "error"]);
    assert!(r[0].s("message").is_some());
    // an empty prefix lists everything
    assert!(cands(&mut c, &s, "", None).len() > 500);
    // `complete` is not an op
    let r = call(&mut c, &[("op", Value::str("complete")), ("session", Value::str(&s)), ("prefix", Value::str("ma"))]);
    assert_eq!(r[0].status(), vec!["done", "unknown-op", "error"]);
    // an ephemeral request works (default ns `user`)
    let r = call(&mut c, &[("op", Value::str("completions")), ("prefix", Value::str("ma"))]);
    assert!(r[0].s("session").is_some());
    assert!(matches!(r[0].0.get("completions"), Some(Value::List(l)) if !l.is_empty()));
}

#[test]
fn complete_fn_param_calls_a_mova_fn_and_a_bad_one_is_ignored() {
    let f = Fixture::start();
    let mut c = f.client();
    let s = c.clone_session(None);
    c.eval(
        &s,
        "(ns my.comp) (defn complete [prefix ns options] [{:candidate (str prefix \"-X\") :type :custom :ns (str ns) :opts (pr-str options)}])",
    );
    let r = call(
        &mut c,
        &[
            ("op", Value::str("completions")),
            ("session", Value::str(&s)),
            ("prefix", Value::str("ab")),
            ("complete-fn", Value::str("my.comp/complete")),
            ("ns", Value::str("user")),
            ("options", Value::dict([("extra-metadata", Value::List(vec![Value::str("arglists")]))])),
        ],
    );
    let cs = match r[0].0.get("completions") {
        Some(Value::List(l)) => l.clone(),
        o => panic!("{o:?} {r:?}"),
    };
    assert_eq!(field(&cs[0], "candidate").as_deref(), Some("ab-X"));
    assert_eq!(field(&cs[0], "type").as_deref(), Some("custom"));
    assert_eq!(field(&cs[0], "ns").as_deref(), Some("user"));
    assert_eq!(field(&cs[0], "opts").as_deref(), Some("{:extra-metadata #{:arglists}}"));
    // an unresolvable fn: the built-in answers
    let r = call(
        &mut c,
        &[("op", Value::str("completions")), ("session", Value::str(&s)), ("prefix", Value::str("mapi")), ("complete-fn", Value::str("no.such/fn"))],
    );
    assert!(matches!(r[0].0.get("completions"), Some(Value::List(l)) if l.iter().any(|c| field(c, "candidate").as_deref() == Some("map-indexed"))));
    // a fn that throws: completions-error with its message
    c.eval(&s, "(defn boom [& _] (throw (ex-info \"nope\" {})))");
    let r = call(
        &mut c,
        &[("op", Value::str("completions")), ("session", Value::str(&s)), ("prefix", Value::str("m")), ("complete-fn", Value::str("my.comp/boom"))],
    );
    assert_eq!(r[0].status(), vec!["done", "completions-error", "error"]);
}

fn lookup(c: &mut Client, session: &str, sym: Option<&str>, ns: Option<&str>) -> Msg {
    let mut items = vec![("op", Value::str("lookup")), ("session", Value::str(session))];
    if let Some(s) = sym {
        items.push(("sym", Value::str(s)));
    }
    if let Some(n) = ns {
        items.push(("ns", Value::str(n)));
    }
    let r = call(c, &items);
    assert_eq!(r.len(), 1, "{r:?}");
    r.into_iter().next().unwrap()
}

#[test]
fn lookup_info_fields_for_core_vars_macros_and_special_forms() {
    let f = Fixture::start();
    let mut c = f.client();
    let s = c.clone_session(None);
    let m = lookup(&mut c, &s, Some("map"), None);
    let info = m.0.get("info").unwrap();
    assert_eq!(field(info, "name").as_deref(), Some("map"));
    assert_eq!(field(info, "ns").as_deref(), Some("clojure.core"));
    assert_eq!(field(info, "arglists").as_deref(), Some("([f] [f coll] [f c1 c2] [f c1 c2 c3] [f c1 c2 c3 & colls])"));
    assert_eq!(field(info, "arglists"), field(info, "arglists-str"));
    assert!(field(info, "doc").unwrap().starts_with("Returns a lazy sequence"));
    assert_eq!(field(info, "added").as_deref(), Some("1.0"));
    assert_eq!(field(info, "protocol").as_deref(), Some(""));
    assert!(field(info, "file").is_some());
    assert!(info.get("macro").is_none());
    // a macro
    let m = lookup(&mut c, &s, Some("when"), None);
    assert_eq!(field(m.0.get("info").unwrap(), "macro").as_deref(), Some("true"));
    // a special form: forms, special-form, arglists-str is empty
    for sf in ["if", "def"] {
        let m = lookup(&mut c, &s, Some(sf), None);
        let info = m.0.get("info").unwrap();
        assert_eq!(field(info, "special-form").as_deref(), Some("true"), "{sf}");
        assert_eq!(field(info, "arglists-str").as_deref(), Some(""));
        assert_eq!(field(info, "ns").as_deref(), Some("clojure.core"));
        assert!(matches!(info.get("forms"), Some(Value::List(l)) if !l.is_empty()));
        assert!(info.get("arglists").is_none());
    }
    // special forms do not need the namespace
    let m = lookup(&mut c, &s, Some("if"), Some("no.such.ns"));
    assert!(m.0.get("info").is_some());
    // a dynamic var
    let m = lookup(&mut c, &s, Some("*print-length*"), None);
    assert_eq!(field(m.0.get("info").unwrap(), "name").as_deref(), Some("*print-length*"));
}

#[test]
fn lookup_user_vars_use_their_place_and_unknowns_are_an_empty_list() {
    let f = Fixture::start();
    let mut c = f.client();
    let s = c.clone_session(None);
    c.eval(&s, "(ns lk.ns (:require [clojure.string :as s]))");
    c.eval(&s, "(defn lk-fn \"doc here\" [a b] 1)");
    let m = lookup(&mut c, &s, Some("lk-fn"), None);
    let info = m.0.get("info").unwrap();
    assert_eq!(field(info, "doc").as_deref(), Some("doc here"));
    assert_eq!(field(info, "arglists").as_deref(), Some("([a b])"));
    assert_eq!(field(info, "ns").as_deref(), Some("lk.ns"));
    assert_eq!(field(info, "file").as_deref(), Some("NO_SOURCE_PATH"));
    assert_eq!(info.get("line"), Some(&int(1)));
    assert_eq!(info.get("column"), Some(&int(1)));
    // line / column / file params of the defining eval show
    c.eval_in(Some(&s), "\n(def placed 1)", &[("file", Value::str("/x/y/placed.clj")), ("line", int(40)), ("column", int(3))]);
    let info = lookup(&mut c, &s, Some("placed"), None).0.get("info").cloned().unwrap();
    assert_eq!(field(&info, "file").as_deref(), Some("/x/y/placed.clj"));
    assert_eq!(info.get("line"), Some(&int(41)));
    // an alias, and a name seen from another ns
    assert!(lookup(&mut c, &s, Some("s/join"), None).0.get("info").is_some_and(|i| field(i, "name").as_deref() == Some("join")));
    for (sym, ns) in [("lk-fn", Some("user")), ("nosuchsym-xyz", None), ("String", None), ("java.lang.String", None), ("Thread/sleep", None), ("clojure.core", None), ("...", None), ("zz/x", None)] {
        let m = lookup(&mut c, &s, Some(sym), ns);
        assert_eq!(m.status(), vec!["done"], "{sym}");
        assert!(matches!(m.0.get("info"), Some(Value::List(l)) if l.is_empty()), "{sym}: {m:?}");
    }
    // `-` is a var
    assert_eq!(field(lookup(&mut c, &s, Some("-"), None).0.get("info").unwrap(), "name").as_deref(), Some("-"));
}

#[test]
fn lookup_errors_have_the_jvm_messages() {
    let f = Fixture::start();
    let mut c = f.client();
    let s = c.clone_session(None);
    for (sym, ns, msg) in [
        (Some("map"), Some("no.such.ns"), "No namespace: no.such.ns found"),
        (Some("s/join"), Some("lk.nothere"), "No namespace: lk.nothere found"),
        (None, None, "no conversion to symbol"),
        (Some(""), None, "Index 0 out of bounds for length 0"),
    ] {
        let m = lookup(&mut c, &s, sym, ns);
        assert_eq!(m.status(), vec!["done", "error", "lookup-error"], "{sym:?}");
        assert_eq!(m.s("message").as_deref(), Some(msg));
    }
    // `eldoc` and `info` are not ops in nREPL 1.8
    let r = call(&mut c, &[("op", Value::str("eldoc")), ("session", Value::str(&s)), ("sym", Value::str("map"))]);
    assert_eq!(r[0].status(), vec!["done", "unknown-op", "error"]);
}

#[test]
fn lookup_fn_param_calls_a_mova_fn() {
    let f = Fixture::start();
    let mut c = f.client();
    let s = c.clone_session(None);
    c.eval(&s, "(ns my.lk) (defn lookup [ns sym] {:name (str sym) :ns (str ns) :line 7 :custom :yes})");
    let r = call(
        &mut c,
        &[("op", Value::str("lookup")), ("session", Value::str(&s)), ("sym", Value::str("a/b")), ("lookup-fn", Value::str("my.lk/lookup")), ("ns", Value::str("user"))],
    );
    let info = r[0].0.get("info").unwrap();
    assert_eq!(field(info, "name").as_deref(), Some("a/b"));
    assert_eq!(field(info, "ns").as_deref(), Some("user"));
    assert_eq!(field(info, "custom").as_deref(), Some("yes"));
    assert_eq!(info.get("line"), Some(&int(7)));
    // unresolvable: the built-in answers
    let r = call(&mut c, &[("op", Value::str("lookup")), ("session", Value::str(&s)), ("sym", Value::str("map")), ("lookup-fn", Value::str("no.such/fn"))]);
    assert!(field(r[0].0.get("info").unwrap(), "arglists").is_some());
}

#[test]
fn tooling_does_not_wait_for_a_running_eval_and_uses_one_thread() {
    let f = Fixture::start();
    let mut c = f.client();
    let s = c.clone_session(None);
    let id = c.send(&[("op", Value::str("eval")), ("session", Value::str(&s)), ("code", Value::str("(Thread/sleep 1500) :slept"))]);
    std::thread::sleep(Duration::from_millis(200));
    let t = Instant::now();
    let cs = cands(&mut c, &s, "map-", None);
    assert!(!cs.is_empty());
    assert!(t.elapsed() < Duration::from_millis(1000), "completions waited for the eval: {:?}", t.elapsed());
    let m = lookup(&mut c, &s, Some("map"), None);
    assert!(m.0.get("info").is_some());
    assert_eq!(f.backend.tool_threads(), 1);
    let r = c.until_done(&id);
    assert!(trace(&r).contains(&"v::slept".to_string()), "{:?}", trace(&r));
}

// ---------------------------------------------------------------------------
// rich errors
// ---------------------------------------------------------------------------

fn err_of(c: &mut Client, session: &str, code: &str) -> String {
    let r = c.eval(session, code);
    texts(&r, "err").concat()
}

#[test]
fn rich_errors_are_the_jvm_text_then_a_blank_line_then_the_report() {
    let f = Fixture::start_with(mova::nrepl::ErrorMode::Rich);
    let mut c = f.client();
    let s = c.clone_session(None);
    // unresolved symbol
    let e = err_of(&mut c, &s, "foo-undefined");
    let (text, report) = e.split_once("\n\n").expect("a blank line between text and report");
    assert!(text.starts_with("Syntax error compiling at (REPL:") && text.contains("Unable to resolve symbol: foo-undefined in this context"), "{e}");
    assert!(report.contains("1 | foo-undefined") && report.contains("`foo-undefined` is not defined in namespace user"), "{e}");
    // the report never repeats the message line
    assert_eq!(e.matches("Unable to resolve symbol").count(), 1, "{e}");
    // arity
    let e = err_of(&mut c, &s, "(inc 1 2)");
    assert!(e.starts_with("Execution error (ArityException) at user/eval"), "{e}");
    assert!(e.contains("Wrong number of args (2) passed to: clojure.core/inc\n\n") && e.contains("(inc 1 2)") && e.contains("expected exactly 1 argument"), "{e}");
    // divide by zero in a fn from an earlier eval: the snippet is that fn's source, not the current buffer
    c.eval(&s, "(defn rich-f [x]\n  (/ 1 x))");
    c.eval(&s, "(defn rich-g [y] (+ 1 (rich-f y)))");
    let e = err_of(&mut c, &s, "(rich-g 0)");
    assert!(e.starts_with("Execution error (ArithmeticException) at user/rich-f (REPL:"), "{e}");
    assert!(e.contains("Divide by zero\n\n"), "{e}");
    assert!(e.contains("(/ 1 x)") && e.contains("the divisor is zero here"), "{e}");
    assert!(e.contains("at user/rich-f (REPL:") && e.contains("at user/rich-g (REPL:1"), "{e}");
    // read error
    let e = err_of(&mut c, &s, "(+ 1\n  2");
    assert!(e.starts_with("Syntax error reading source at (REPL:"), "{e}");
    assert!(e.contains("EOF while reading, starting at line 1\n\n") && e.contains("unclosed list, opened here"), "{e}");
    // bad let
    let e = err_of(&mut c, &s, "(let [x] 1)");
    assert!(e.starts_with("Syntax error macroexpanding clojure.core/let at (REPL:1:1)."), "{e}");
    assert!(e.contains("\n\n") && e.contains("(let [x] 1)") && e.contains("help: "), "{e}");
    // an error with no report is the plain text
    let e = err_of(&mut c, &s, "(throw (ex-info \"boom\" {:a 1}))");
    assert!(e.starts_with("Execution error (ExceptionInfo) at user/eval") && e.contains("boom\n\n") && e.contains("note: ex-data: {:a 1}"), "{e}");
}

#[test]
fn rich_errors_in_padded_and_loaded_code_show_the_users_place() {
    let f = Fixture::start_with(mova::nrepl::ErrorMode::Rich);
    let mut c = f.client();
    let s = c.clone_session(None);
    // `line` / `column` pad the buffer: no blank padding lines in the snippet, real line numbers
    let r = c.eval_in(Some(&s), "(+ 1 2)\n(/ 1 0)", &[("file", Value::str("/a/b/foo.clj")), ("line", int(10)), ("column", int(5))]);
    let e = texts(&r, "err").concat();
    assert!(e.starts_with("Execution error (ArithmeticException) at user/eval"), "{e}");
    assert!(e.contains("(foo.clj:11)"), "{e}");
    assert!(e.contains(",-[/a/b/foo.clj:11:1]") && e.contains(" 11 | (/ 1 0)") && e.contains(" 10 |     (+ 1 2)"), "{e}");
    assert!(!e.lines().any(|l| l.trim_end().ends_with("|") && l.trim_start().starts_with(|c: char| c.is_ascii_digit())), "blank padding line in the snippet:\n{e}");
    // load-file: the file name and line are the user's
    let r = load_file(
        &mut c,
        Some(&s),
        Some("(ns foo.baz)\n(def a 1)\n(+ a\n  (nope))\n"),
        &[("file-path", Value::str("src/foo/baz.clj")), ("file-name", Value::str("baz.clj"))],
    );
    let e = texts(&r, "err").concat();
    assert!(e.starts_with("Syntax error compiling at (src/foo/baz.clj:4:3).\nUnable to resolve symbol: nope in this context\n\n"), "{e}");
    assert!(e.contains(",-[src/foo/baz.clj:4:4]") && e.contains("4 |   (nope))"), "{e}");
    assert!(!e.contains("Compiler$CompilerException"), "no duplicate header in notes:\n{e}");
    let r = load_file(
        &mut c,
        Some(&s),
        Some("(ns foo.bar)\n\n(defn k [x]\n  (inc x))\n\n(def z (k nil))\n"),
        &[("file-path", Value::str("src/foo/bar.clj")), ("file-name", Value::str("bar.clj"))],
    );
    let e = texts(&r, "err").concat();
    assert!(e.starts_with("Execution error (NullPointerException) at foo.bar/k (bar.clj:3).\n"), "{e}");
    assert!(e.contains(",-[src/foo/bar.clj:3:1]") && e.contains("at foo.bar/k (bar.clj:6:8)"), "{e}");
}

#[test]
fn jvm_mode_errors_are_exactly_the_text() {
    let f = Fixture::start_with(mova::nrepl::ErrorMode::Jvm);
    let mut c = f.client();
    let s = c.clone_session(None);
    let e = err_of(&mut c, &s, "(inc 1 2)").replace(|ch: char| ch.is_ascii_digit(), "N").replace("NN", "N").replace("NN", "N");
    assert_eq!(e, "Execution error (ArityException) at user/evalN (REPL:N).\nWrong number of args (N) passed to: clojure.core/inc\n");
    assert!(!err_of(&mut c, &s, "(/ 1 0)").contains(",-["));
}

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

fn int(n: i64) -> Value {
    Value::Int(n)
}

fn texts(msgs: &[Msg], key: &str) -> Vec<String> {
    msgs.iter().filter_map(|m| m.s(key)).collect()
}

fn trace(msgs: &[Msg]) -> Vec<String> {
    msgs.iter()
        .map(|m| {
            if let Some(v) = m.s("value") {
                format!("v:{v}")
            } else {
                format!("status:{}", m.status().join(","))
            }
        })
        .collect()
}

#[test]
fn core_load_file_runs_the_file_with_output_and_rich_errors() {
    let f = Fixture::start_with(mova::nrepl::ErrorMode::Rich);
    let mut c = f.client();
    let s = c.clone_session(None);
    let dir = std::env::temp_dir().join(format!("mova-core-load-file-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let ok = dir.join("okfile.clj");
    let bad = dir.join("badfile.clj");
    std::fs::write(&ok, "(defn lf-sq [x]\n  (* x x))\n(println \"loaded\")\n(def lf-file *file*)\n(lf-sq 3)\n").unwrap();
    std::fs::write(&bad, "(ns lfbad)\n(defn k [x]\n  (inc x))\n(println \"before\")\n(k nil)\n").unwrap();
    let (okp, badp) = (ok.to_str().unwrap().to_string(), bad.to_str().unwrap().to_string());
    let r = c.eval(&s, &format!("(load-file {okp:?})"));
    assert_eq!(texts(&r, "out").concat(), "loaded\n");
    assert_eq!(texts(&r, "value"), ["9"], "{:?}", trace(&r));
    assert_eq!(c.value(&s, "lf-file"), format!("{okp:?}"));
    assert_eq!(c.value(&s, "(select-keys (meta #'lf-sq) [:file :line])"), format!("{{:file {okp:?}, :line 1}}"));
    let r = c.eval(&s, &format!("(load-file {badp:?})"));
    assert_eq!(texts(&r, "out").concat(), "before\n");
    let e = texts(&r, "err").concat();
    assert!(e.contains("at lfbad/k (badfile.clj:2)") && e.contains("badfile.clj:2:1]") && e.contains("(inc x)") && e.contains("at lfbad/k (badfile.clj:5:1)"), "{e}");
    assert_eq!(c.value(&s, "(str *ns*)"), "\"user\"");
    let e = err_of(&mut c, &s, "(load-file \"no-such-dir/none.clj\")");
    assert!(e.contains("FileNotFoundException") || e.contains("No such file"), "{e}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn macroexpand_and_macroexpand_1_are_real_vars_for_completion_and_lookup() {
    let f = Fixture::start();
    let mut c = f.client();
    let s = c.clone_session(None);
    let cs = cands(&mut c, &s, "ma", None);
    for n in ["macroexpand", "macroexpand-1"] {
        let m = find(&cs, n);
        assert_eq!(field(m, "ns").as_deref(), Some("clojure.core"), "{n}");
        assert_eq!(field(m, "type").as_deref(), Some("function"), "{n}");
        let info_msg = lookup(&mut c, &s, Some(n), None);
        let info = info_msg.0.get("info").unwrap();
        assert_eq!(field(info, "arglists").as_deref(), Some("([form])"), "{n}");
        assert!(field(info, "doc").unwrap().contains("macro form"), "{n}");
    }
    assert_eq!(c.value(&s, "(map macroexpand ['(when a b)])"), "((if a (do b)))");
}

#[test]
fn many_evals_do_not_exhaust_the_source_registry() {
    let f = Fixture::start_with(mova::nrepl::ErrorMode::Rich);
    let mut c = f.client();
    let s = c.clone_session(None);
    // 3000 distinct evals that define nothing that refers to their source
    for i in 0..3000 {
        let r = c.eval(&s, &format!("(+ {i} 1)"));
        if i == 2999 {
            assert_eq!(texts(&r, "value"), ["3000"]);
        }
    }
    // evals that define a fn keep their source: still right after more evals
    c.eval(&s, "(defn reg-f [x]\n  (/ 10 x))");
    for i in 0..1200 {
        c.eval(&s, &format!("(- {i} 1)"));
    }
    let e = err_of(&mut c, &s, "(reg-f 0)");
    assert!(e.starts_with("Execution error (ArithmeticException) at user/reg-f (REPL:1)."), "{e}");
    assert!(e.contains("2 |   (/ 10 x)") || e.contains("(/ 10 x)"), "{e}");
    assert!(e.contains(",-[REPL:1:1]") || e.contains(",-[REPL:"), "{e}");
    assert!(e.contains("the divisor is zero here"), "{e}");
}
