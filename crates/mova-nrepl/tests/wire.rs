//! End-to-end tests of the wire layer with a mock backend, over real sockets.

use mova_nrepl::bencode::{decode, encode, Value};
use mova_nrepl::{status, Backend, Call, Endpoint, Listeners, Server, ServerHandle, V};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

/// `eval` answers `value` = length of `code`, from another thread, then done.
/// `flood` sends `n` `out` messages of 1000 bytes (n = the `line` int).
struct Mock;

impl Backend for Mock {
    fn dispatch(&self, call: Call<'_>) {
        let reply = call.reply.clone();
        match call.req.op().and_then(|o| o.as_str()) {
            Some("eval") => {
                let n = call.req.code().and_then(|c| c.as_bytes()).map(|b| b.len()).unwrap_or(0);
                std::thread::spawn(move || {
                    reply.send(&[("ns", V::Str("user")), ("value", V::Str(&n.to_string()))]);
                    reply.send_status(status::DONE);
                });
            }
            Some("completions") => {
                // reply on the IO thread, then wake another thread that replies:
                // the wire order must be this one (the thread cannot have run before)
                let r2 = reply.clone();
                let (tx, rx) = std::sync::mpsc::channel::<()>();
                let h = std::thread::spawn(move || {
                    rx.recv().unwrap();
                    r2.send(&[("out", V::Str("from-thread"))]);
                });
                reply.send(&[("out", V::Str("from-io"))]);
                tx.send(()).unwrap();
                // the thread's reply is queued before this handler returns
                h.join().unwrap();
            }
            Some("forward-system-output") => {
                // from many threads, each message tagged with its thread and number
                let n = call.req.line().and_then(|l| l.as_int()).unwrap_or(0);
                let t = call.req.code().and_then(|c| c.as_bytes()).map(|b| b.len()).unwrap_or(1);
                let mut hs = Vec::new();
                for k in 0..t {
                    let r = reply.clone();
                    hs.push(std::thread::spawn(move || {
                        for i in 0..n {
                            r.send(&[("out", V::Str(&format!("{k}:{i}")))]);
                            if i % 7 == 0 {
                                std::thread::sleep(Duration::from_micros(300));
                            }
                        }
                    }));
                }
                let r = reply.clone();
                std::thread::spawn(move || {
                    for h in hs {
                        h.join().unwrap();
                    }
                    r.send_status(status::DONE);
                });
            }
            Some("stdin") => {
                // answered on the IO thread itself
                reply.send_status(status::DONE);
            }
            Some("lookup") => {
                let n = call.req.line().and_then(|l| l.as_int()).unwrap_or(0);
                std::thread::spawn(move || {
                    let chunk = "x".repeat(1000);
                    for _ in 0..n {
                        if !reply.send(&[("out", V::Str(&chunk))]) {
                            return;
                        }
                    }
                    reply.send_status(status::DONE);
                });
            }
            _ => {}
        }
    }
}

struct Fixture {
    port: u16,
    handle: ServerHandle,
    join: Option<std::thread::JoinHandle<()>>,
}

impl Fixture {
    fn start() -> Fixture {
        let l = Listeners::bind(&Endpoint::Tcp { host: "127.0.0.1".into(), port: 0 }).unwrap();
        let port = l.port().unwrap();
        let server = Server::new(l, Arc::new(Mock), false).unwrap();
        let handle = server.handle();
        let join = std::thread::spawn(move || server.run().unwrap());
        Fixture { port, handle, join: Some(join) }
    }
    fn connect(&self) -> Client {
        let s = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        Client { s, buf: Vec::new() }
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
}

fn msg<const N: usize>(items: [(&str, Value); N]) -> Vec<u8> {
    let mut out = Vec::new();
    encode(&Value::dict(items), &mut out);
    out
}

impl Client {
    fn send(&mut self, bytes: &[u8]) {
        self.s.write_all(bytes).unwrap();
    }
    /// Next reply, or `None` on EOF.
    fn recv(&mut self) -> Option<Value> {
        loop {
            if let Ok(Some((v, used))) = decode(&self.buf) {
                self.buf.drain(..used);
                return Some(v);
            }
            let mut tmp = [0u8; 65536];
            match self.s.read(&mut tmp) {
                Ok(0) => return None,
                Ok(n) => self.buf.extend_from_slice(&tmp[..n]),
                Err(e) => panic!("read: {e}"),
            }
        }
    }
    fn call(&mut self, bytes: &[u8]) -> Value {
        self.send(bytes);
        self.recv().expect("reply")
    }
}

fn is_done(v: &Value) -> bool {
    matches!(v.get("status"), Some(Value::List(l)) if l.iter().any(|s| s.as_str() == Some("done")))
}

#[test]
fn describe_and_clone() {
    let f = Fixture::start();
    let mut c = f.connect();
    let d = c.call(&msg([("op", Value::str("describe")), ("id", Value::str("1"))]));
    assert!(d.get("ops").is_some() && is_done(&d));
    let cl = c.call(&msg([("op", Value::str("clone")), ("id", Value::str("2"))]));
    assert!(cl.get("new-session").is_some());
}

#[test]
fn two_messages_in_one_write_and_split_writes() {
    let f = Fixture::start();
    let mut c = f.connect();
    let mut both = msg([("op", Value::str("describe")), ("id", Value::str("1"))]);
    both.extend(msg([("op", Value::str("clone")), ("id", Value::str("2"))]));
    c.send(&both);
    assert_eq!(c.recv().unwrap().get("id").unwrap().as_str(), Some("1"));
    assert_eq!(c.recv().unwrap().get("id").unwrap().as_str(), Some("2"));

    // one message, one byte at a time
    let one = msg([("op", Value::str("clone")), ("id", Value::str("3"))]);
    for b in &one {
        c.send(std::slice::from_ref(b));
        std::thread::sleep(Duration::from_micros(200));
    }
    assert_eq!(c.recv().unwrap().get("id").unwrap().as_str(), Some("3"));
}

#[test]
fn one_megabyte_code_and_reply_from_another_thread() {
    let f = Fixture::start();
    let mut c = f.connect();
    let code = "a".repeat(1 << 20);
    let m = msg([("op", Value::str("eval")), ("id", Value::str("7")), ("code", Value::str(&code))]);
    c.send(&m);
    let v = c.recv().unwrap();
    assert_eq!(v.get("value").unwrap().as_str(), Some("1048576"));
    assert_eq!(v.get("id").unwrap().as_str(), Some("7"));
    assert!(is_done(&c.recv().unwrap()));
    // two of them back to back, in one write
    let mut two = m.clone();
    two.extend(&m);
    c.send(&two);
    for _ in 0..2 {
        assert_eq!(c.recv().unwrap().get("value").unwrap().as_str(), Some("1048576"));
        assert!(is_done(&c.recv().unwrap()));
    }
}

#[test]
fn ephemeral_session_is_fresh_for_each_message_and_stable_within_one() {
    let f = Fixture::start();
    let mut c = f.connect();
    let m = msg([("op", Value::str("eval")), ("code", Value::str("12"))]);
    let a = c.call(&m);
    let a_done = c.recv().unwrap();
    assert_eq!(a.get("session"), a_done.get("session"));
    let b = c.call(&m);
    let _ = c.recv();
    assert_ne!(a.get("session"), b.get("session"));
    let ls = c.call(&msg([("op", Value::str("ls-sessions"))]));
    assert_eq!(ls.get("sessions"), Some(&Value::List(vec![])));
}

#[test]
fn backend_reply_on_the_io_thread() {
    let f = Fixture::start();
    let mut c = f.connect();
    let v = c.call(&msg([("op", Value::str("stdin")), ("id", Value::str("9")), ("stdin", Value::str("x"))]));
    assert!(is_done(&v));
    assert_eq!(v.get("id").unwrap().as_str(), Some("9"));
}

#[test]
fn malformed_input_closes_the_connection_but_not_the_server() {
    let f = Fixture::start();
    for junk in [&b"hello\n"[..], b"i5e", b"d1:ai1e1:b", b"di1ei2ee", b"d2:op-3:abce"] {
        let mut c = f.connect();
        c.send(junk);
        if junk.ends_with(b"1:b") {
            // incomplete: the server waits; closing our side ends it
            c.s.shutdown(std::net::Shutdown::Write).unwrap();
        }
        assert!(c.recv().is_none(), "connection should close for {junk:?}");
    }
    let mut c = f.connect();
    assert!(is_done(&c.call(&msg([("op", Value::str("describe"))]))));
}

#[test]
fn good_message_before_junk_is_answered_then_closed() {
    let f = Fixture::start();
    let mut c = f.connect();
    let mut bytes = msg([("op", Value::str("clone")), ("id", Value::str("1"))]);
    bytes.extend(b"junk");
    c.send(&bytes);
    assert_eq!(c.recv().unwrap().get("id").unwrap().as_str(), Some("1"));
    assert!(c.recv().is_none());
}

#[test]
fn slow_reader_gets_everything_in_order() {
    // 20 MB of `out` replies pushed from another thread to a client that
    // starts reading late: exercises partial writes and write interest.
    let f = Fixture::start();
    let mut c = f.connect();
    c.send(&msg([("op", Value::str("lookup")), ("id", Value::str("1")), ("line", Value::Int(20_000))]));
    std::thread::sleep(Duration::from_millis(300));
    let mut n = 0;
    loop {
        let v = c.recv().unwrap();
        if is_done(&v) {
            break;
        }
        assert_eq!(v.get("out").unwrap().as_str().unwrap().len(), 1000);
        n += 1;
    }
    assert_eq!(n, 20_000);
}

#[test]
fn closed_connection_drops_its_queue_and_server_survives() {
    let f = Fixture::start();
    {
        let mut c = f.connect();
        c.send(&msg([("op", Value::str("lookup")), ("line", Value::Int(50_000))]));
        // drop without reading: server must close it and stop the sender
    }
    std::thread::sleep(Duration::from_millis(200));
    let mut c2 = f.connect();
    assert!(is_done(&c2.call(&msg([("op", Value::str("describe"))]))));
}

#[test]
fn many_connections() {
    let f = Fixture::start();
    let mut cs: Vec<Client> = (0..200).map(|_| f.connect()).collect();
    for (i, c) in cs.iter_mut().enumerate() {
        c.send(&msg([("op", Value::str("clone")), ("id", Value::str(&i.to_string()))]));
    }
    for (i, c) in cs.iter_mut().enumerate() {
        assert_eq!(c.recv().unwrap().get("id").unwrap().as_str(), Some(i.to_string().as_str()));
    }
    let ls = cs[0].call(&msg([("op", Value::str("ls-sessions"))]));
    let Some(Value::List(l)) = ls.get("sessions") else { panic!() };
    assert_eq!(l.len(), 200);
}

#[test]
fn unix_socket_transport() {
    use std::os::unix::net::UnixStream;
    let path = std::env::temp_dir().join(format!("mova-nrepl-wire-{}.sock", std::process::id()));
    let l = Listeners::bind(&Endpoint::Unix(path.clone())).unwrap();
    let server = Server::new(l, Arc::new(Mock), false).unwrap();
    let handle = server.handle();
    let j = std::thread::spawn(move || server.run().unwrap());
    let mut s = UnixStream::connect(&path).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(&msg([("op", Value::str("describe")), ("id", Value::str("1"))])).unwrap();
    let mut buf = vec![0u8; 100_000];
    let mut got = 0;
    let v = loop {
        got += s.read(&mut buf[got..]).unwrap();
        if let Ok(Some((v, _))) = decode(&buf[..got]) {
            break v;
        }
    };
    assert!(is_done(&v));
    handle.shutdown();
    j.join().unwrap();
    assert!(!path.exists(), "socket file is removed when the server is dropped");
}

// ---- EDN and TTY transports (the codec at the edge) ----

use mova_nrepl::Codec;

fn start_with(codec: Codec) -> Fixture {
    let l = Listeners::bind(&Endpoint::Tcp { host: "127.0.0.1".into(), port: 0 }).unwrap();
    let port = l.port().unwrap();
    let server = Server::new(l, Arc::new(Mock), false).unwrap().with_codec(codec);
    let handle = server.handle();
    let join = std::thread::spawn(move || server.run().unwrap());
    Fixture { port, handle, join: Some(join) }
}

fn read_until(s: &mut TcpStream, want: &str) -> String {
    let mut got = String::new();
    while !got.contains(want) {
        let mut tmp = [0u8; 4096];
        let n = s.read(&mut tmp).expect("read");
        assert!(n > 0, "closed, have {got:?}");
        got.push_str(&String::from_utf8_lossy(&tmp[..n]));
    }
    got
}

#[test]
fn edn_requests_and_replies() {
    let fx = start_with(Codec::Edn);
    let mut c = fx.connect().s;
    // a request split in two, then two in one write; replies come from another thread
    c.write_all(b"{:op \"eval\" :id \"1\" :code \"(+ 1").unwrap();
    std::thread::sleep(Duration::from_millis(30));
    c.write_all(b" 2)\"}{:op \"nosuch\" :id \"2\"}{:op \"ls-sessions\"}").unwrap();
    let mut got = read_until(&mut c, ":sessions");
    if !got.contains(":value") {
        got.push_str(&read_until(&mut c, ":value"));
    }
    if !got.contains(":status #{:done}}") {
        got.push_str(&read_until(&mut c, "}"));
    }
    assert!(got.contains(r#"{:id "1", :ns "user", :session ""#), "{got}");
    assert!(got.contains(r#":value "7"#), "{got}");
    assert!(got.contains(r#":id "2", :op "nosuch", :session ""#) && got.contains(":status #{:done :unknown-op :error}"), "{got}");
    assert!(got.contains(":sessions []"), "{got}");
}

#[test]
fn tty_greeting_prompt_and_elision() {
    let fx = start_with(Codec::Tty);
    let mut c = fx.connect().s;
    let g = read_until(&mut c, "user=> ");
    assert!(g.starts_with(";; nREPL 1.8.0\n;; Clojure "), "{g:?}");
    // the mock answers `value` = length of the code
    c.write_all(b"(+ 1 2)\n").unwrap();
    assert_eq!(read_until(&mut c, "\nuser=> "), "7\nuser=> ");
    // two forms in one write are answered one after the other
    c.write_all(b"[1]\n:abc\n").unwrap();
    assert_eq!(read_until(&mut c, "4\nuser=> "), "3\nuser=> 4\nuser=> ");
}

#[test]
fn io_thread_reply_precedes_a_reply_the_handler_provoked_from_another_thread() {
    // `order`: the handler answers on the IO thread, then a thread it woke
    // answers too. Both are queued when the handler returns; the first one
    // must be written first (this is what `interrupt` relies on).
    let f = Fixture::start();
    let mut c = f.connect();
    for _ in 0..200 {
        c.send(&msg([("op", Value::str("completions"))]));
        assert_eq!(c.recv().unwrap().get("out").unwrap().as_str(), Some("from-io"));
        assert_eq!(c.recv().unwrap().get("out").unwrap().as_str(), Some("from-thread"));
    }
}

#[test]
fn many_threads_send_while_the_io_thread_idles_and_nothing_is_lost() {
    // Every message sent from a non-IO thread must arrive, in order per
    // thread, though the IO thread goes to sleep between the bursts (a lost
    // wake-up shows as a read timeout).
    let f = Fixture::start();
    let mut c = f.connect();
    let (threads, per) = (16usize, 300i64);
    for round in 0..5 {
        c.send(&msg([("op", Value::str("forward-system-output")), ("id", Value::str("b")), ("code", Value::str(&"x".repeat(threads))), ("line", Value::Int(per))]));
        let mut next = vec![0i64; threads];
        loop {
            let v = c.recv().unwrap();
            if is_done(&v) {
                break;
            }
            let s = v.get("out").unwrap().as_str().unwrap().to_string();
            let (k, i) = s.split_once(':').unwrap();
            let (k, i): (usize, i64) = (k.parse().unwrap(), i.parse().unwrap());
            assert_eq!(next[k], i, "round {round}: thread {k} out of order");
            next[k] += 1;
        }
        assert!(next.iter().all(|&n| n == per), "round {round}: {next:?}");
        std::thread::sleep(Duration::from_millis(30));
    }
    // single message from a thread after a long idle
    std::thread::sleep(Duration::from_millis(200));
    let v = c.call(&msg([("op", Value::str("eval")), ("code", Value::str("ab"))]));
    assert_eq!(v.get("value").unwrap().as_str(), Some("2"));
}
