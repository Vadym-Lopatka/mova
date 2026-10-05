//! Transports: how bytes on a connection become requests and replies.
//!
//! The core of the server speaks one thing: a bencode request in, bencode
//! replies out (`Responder`, `Outbox` and the router never change). A
//! [`Codec`] sits at the edge, on the IO thread, per connection:
//!
//! | codec | bytes in | bytes out |
//! |---|---|---|
//! | `Bencode` | parsed as they are | written as they are (zero extra cost) |
//! | `Edn` | EDN maps, converted to a bencode request | each bencode reply printed as an EDN map |
//! | `Tty` | text lines; one form at a time becomes an `eval` | `out`, `err`, `value` text and a `ns=> ` prompt |
//!
//! Replies from any thread reach the connection as bencode bytes through the
//! shared inbox (one `Vec` per message, one wake-up per burst, as before).
//! Only the IO thread translates, when it appends them to the write buffer, so
//! the bencode path has one `match` on a `Copy` enum and nothing else. A later
//! change to a per-connection shared buffer needs only that append site.

use crate::bencode::{decode, encode, Value};
use crate::edn::{self, Edn};
use std::collections::BTreeMap;

/// The `-t/--transport` choices.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Codec {
    #[default]
    Bencode,
    Edn,
    Tty,
}

impl Codec {
    /// `nrepl.transport/bencode` and friends. `None` for anything else.
    pub fn from_symbol(s: &str) -> Option<Codec> {
        match s {
            "nrepl.transport/bencode" => Some(Codec::Bencode),
            "nrepl.transport/edn" => Some(Codec::Edn),
            "nrepl.transport/tty" => Some(Codec::Tty),
            _ => None,
        }
    }

    /// The URI scheme in the startup banner (`transport/uri-scheme`).
    pub fn uri_scheme(self) -> &'static str {
        match self {
            Codec::Bencode => "nrepl",
            Codec::Edn => "nrepl+edn",
            Codec::Tty => "telnet",
        }
    }
}

// ---------------------------------------------------------------------------
// EDN
// ---------------------------------------------------------------------------

/// Parses the EDN map at the front of `buf` and appends it to `out` as a
/// bencode request. Returns the bytes used, or `Ok(None)` if the map is not
/// complete yet.
///
/// Keys become strings (a leading colon is dropped). `nil` and `false` values
/// leave the key out (the JVM would see a falsy value); `true` is `"true"`;
/// keywords and symbols become strings; collections become lists / dicts.
pub fn edn_request_to_bencode(buf: &[u8], out: &mut Vec<u8>) -> Result<Option<usize>, &'static str> {
    match edn::parse(buf, 0, false) {
        Err(edn::Stop::Need) => Ok(None),
        Err(edn::Stop::Bad(why)) => Err(why),
        Ok((Edn::Map(m), used)) => {
            let mut d = BTreeMap::new();
            for (k, v) in &m {
                let Some(k) = key_name(k) else { return Err("message keys must be keywords or strings") };
                if let Some(v) = to_value(v) {
                    d.insert(k.into_bytes(), v);
                }
            }
            encode(&Value::Dict(d), out);
            Ok(Some(used))
        }
        Ok(_) => Err("a message must be a map"),
    }
}

fn key_name(k: &Edn) -> Option<String> {
    match k {
        Edn::Kw(s) | Edn::Str(s) | Edn::Sym(s) => Some(s.clone()),
        _ => None,
    }
}

fn to_value(e: &Edn) -> Option<Value> {
    Some(match e {
        Edn::Nil | Edn::Bool(false) => return None,
        Edn::Bool(true) => Value::str("true"),
        Edn::Int(n) => Value::Int(*n),
        Edn::Float(s) | Edn::Str(s) | Edn::Kw(s) | Edn::Sym(s) => Value::str(s),
        Edn::List(l) | Edn::Vec(l) | Edn::Set(l) => Value::List(l.iter().filter_map(to_value).collect()),
        Edn::Map(m) => {
            let mut d = BTreeMap::new();
            for (k, v) in m {
                if let (Some(k), Some(v)) = (key_name(k), to_value(v)) {
                    d.insert(k.into_bytes(), v);
                }
            }
            Value::Dict(d)
        }
    })
}

/// Appends every bencode reply in `bencode` to `out` as an EDN map. The reply
/// is a map with keyword keys; `status` is a set of keywords. Keys come out
/// sorted (the JVM prints in hash order; EDN readers do not care).
/// Returns the number of messages.
pub fn bencode_replies_to_edn(bencode: &[u8], out: &mut Vec<u8>) -> usize {
    let mut pos = 0;
    let mut n = 0;
    while pos < bencode.len() {
        match decode(&bencode[pos..]) {
            Ok(Some((v, used))) => {
                print_edn(&v, out, Ctx::Top);
                pos += used;
                n += 1;
            }
            _ => break,
        }
    }
    n
}

#[derive(Clone, Copy, PartialEq)]
enum Ctx {
    Top,
    /// Under `ops`: the keys stay strings, as in the JVM.
    Plain,
    Nested,
    StatusSet,
}

fn print_edn(v: &Value, out: &mut Vec<u8>, ctx: Ctx) {
    match v {
        Value::Int(n) => out.extend_from_slice(n.to_string().as_bytes()),
        Value::Bytes(b) => {
            if ctx == Ctx::StatusSet {
                out.push(b':');
                out.extend_from_slice(b);
            } else {
                edn::write_string(out, b);
            }
        }
        Value::List(l) => {
            let (open, close): (&[u8], u8) = if ctx == Ctx::StatusSet { (b"#{", b'}') } else { (b"[", b']') };
            out.extend_from_slice(open);
            for (i, x) in l.iter().enumerate() {
                if i > 0 {
                    out.push(b' ');
                }
                print_edn(x, out, ctx);
            }
            out.push(close);
        }
        Value::Dict(m) => {
            out.push(b'{');
            for (i, (k, x)) in m.iter().enumerate() {
                if i > 0 {
                    out.extend_from_slice(b", ");
                }
                if ctx == Ctx::Plain {
                    edn::write_string(out, k);
                } else {
                    out.push(b':');
                    out.extend_from_slice(k);
                }
                let sub = match (ctx, k.as_slice()) {
                    (Ctx::Top, b"status") => Ctx::StatusSet,
                    (_, b"ops") => Ctx::Plain,
                    (Ctx::Plain, _) => Ctx::Plain,
                    _ => Ctx::Nested,
                };
                out.push(b' ');
                print_edn(x, out, sub);
            }
            out.push(b'}');
        }
    }
}

// ---------------------------------------------------------------------------
// TTY
// ---------------------------------------------------------------------------

/// Per-connection state of the TTY transport (`nrepl.transport/tty`).
///
/// The server side mimics the JVM: a greeting, an implicit `clone`, then one
/// `eval` per form read from the input, only after the previous one is done.
/// Replies are written as plain text: `out`, `err`, `value`, and after the
/// eval's `done` the prompt `"\n<ns>=> "`.
///
/// A form that produced no value and no error (a reader conditional that
/// reads as nothing, `#_x`) gets no prompt, as on the JVM, where the reader
/// goes on to the next form.
pub(crate) struct Tty {
    /// Input not yet turned into a request.
    pub inbuf: Vec<u8>,
    pub session: Option<Vec<u8>>,
    pub ns: String,
    awaiting: Option<Vec<u8>>,
    output: bool,
    counter: u64,
    /// Set by `reply`: the connection should try `next_request` again.
    pub need_pump: bool,
}

impl Tty {
    pub fn new() -> Tty {
        Tty { inbuf: Vec::new(), session: None, ns: "user".into(), awaiting: None, output: false, counter: 0, need_pump: true }
    }

    /// The greeting `tty-greeting` sends.
    pub fn greeting(clojure_version: &str) -> Vec<u8> {
        format!(";; nREPL {}\n;; Clojure {}\nuser=> ", crate::describe::NREPL_VERSION, clojure_version).into_bytes()
    }

    /// The `clone` request that starts the transport.
    pub fn clone_request() -> Vec<u8> {
        b"d2:op5:clonee".to_vec()
    }

    /// Handles every bencode reply in `bencode`: appends text to `out`, keeps state.
    pub fn replies(&mut self, bencode: &[u8], out: &mut Vec<u8>) {
        let mut pos = 0;
        while pos < bencode.len() {
            let Ok(Some((v, used))) = decode(&bencode[pos..]) else { break };
            pos += used;
            self.reply(&v, out);
        }
    }

    fn reply(&mut self, v: &Value, out: &mut Vec<u8>) {
        let Value::Dict(m) = v else { return };
        let get = |k: &str| m.get(k.as_bytes());
        if let Some(Value::Bytes(s)) = get("new-session") {
            self.session = Some(s.clone());
        }
        if let Some(Value::Bytes(s)) = get("ns") {
            self.ns = String::from_utf8_lossy(s).into_owned();
        }
        for k in ["out", "err", "value"] {
            if let Some(Value::Bytes(s)) = get(k) {
                out.extend_from_slice(s);
                if k != "out" {
                    self.output = true;
                }
            }
        }
        if get("ex").is_some() {
            self.output = true;
        }
        let done = matches!(get("status"), Some(Value::List(l)) if l.iter().any(|x| matches!(x, Value::Bytes(b) if b == b"done")));
        if done {
            let id = match get("id") {
                Some(Value::Bytes(b)) => Some(b),
                _ => None,
            };
            if id.is_some() && id == self.awaiting.as_ref() {
                self.awaiting = None;
                if self.output {
                    out.extend_from_slice(format!("\n{}=> ", self.ns).as_bytes());
                }
                self.output = false;
            }
            self.need_pump = true;
        }
    }

    /// The next `eval` request, if the previous eval is done and `inbuf`
    /// holds a whole form.
    pub fn next_request(&mut self) -> Option<Vec<u8>> {
        self.need_pump = false;
        if self.awaiting.is_some() {
            return None;
        }
        let session = self.session.clone()?;
        let (start, end) = scan_form(&self.inbuf);
        let Some(end) = end else {
            // only white space and comments so far: forget them
            self.inbuf.drain(..start);
            return None;
        };
        let code = self.inbuf[start..end].to_vec();
        self.inbuf.drain(..end);
        self.counter += 1;
        let id = format!("eval{}-{}", crate::session::SessionId::new().as_str(), self.counter).into_bytes();
        self.awaiting = Some(id.clone());
        self.output = false;
        let mut d = BTreeMap::new();
        d.insert(b"op".to_vec(), Value::str("eval"));
        d.insert(b"code".to_vec(), Value::Bytes(code));
        d.insert(b"id".to_vec(), Value::Bytes(id));
        d.insert(b"ns".to_vec(), Value::str(&self.ns));
        d.insert(b"session".to_vec(), Value::Bytes(session));
        let mut out = Vec::new();
        encode(&Value::Dict(d), &mut out);
        Some(out)
    }

    /// The request that closes the session when the connection goes away.
    pub fn close_request(&self) -> Option<Vec<u8>> {
        let s = self.session.clone()?;
        let mut d = BTreeMap::new();
        d.insert(b"op".to_vec(), Value::str("close"));
        d.insert(b"session".to_vec(), Value::Bytes(s));
        let mut out = Vec::new();
        encode(&Value::Dict(d), &mut out);
        Some(out)
    }
}

/// Finds the first form in `buf`. Returns `(start, end)`: where it starts
/// (after white space and comments) and `Some(end)` if it is complete.
///
/// This only finds the boundary, it does not read the form; the interpreter
/// reads it. It knows lists, vectors, maps, sets, strings, character
/// literals, comments, and the reader prefixes (`'` `` ` `` `~` `~@` `@` `#'`
/// `#_` `#?` `#?@` `#:ns` `#tag`, and `^` which takes two forms).
pub fn scan_form(buf: &[u8]) -> (usize, Option<usize>) {
    let start = skip_ws(buf, 0);
    if start >= buf.len() {
        return (start, None);
    }
    match form_end(buf, start, 0) {
        Some(end) => (start, Some(end)),
        None => (start, None),
    }
}

fn skip_ws(buf: &[u8], mut p: usize) -> usize {
    loop {
        match buf.get(p) {
            Some(b' ' | b'\t' | b'\n' | b'\r' | b',' | 0x0c) => p += 1,
            Some(b';') => {
                while buf.get(p).is_some_and(|&b| b != b'\n') {
                    p += 1;
                }
            }
            _ => return p,
        }
    }
}

fn is_delim(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | b',' | 0x0c | b'(' | b')' | b'[' | b']' | b'{' | b'}' | b'"' | b';')
}

fn string_end(buf: &[u8], mut p: usize) -> Option<usize> {
    // p is just after the opening quote
    loop {
        match buf.get(p)? {
            b'\\' => p += 2,
            b'"' => return Some(p + 1),
            _ => p += 1,
        }
    }
}

/// End of the form that starts at `p` (which is not white space), or `None` if incomplete.
fn form_end(buf: &[u8], p: usize, depth: usize) -> Option<usize> {
    if depth > 512 {
        return Some(p + 1); // let the reader complain
    }
    let next = |q: usize| -> Option<usize> {
        let q = skip_ws(buf, q);
        if q >= buf.len() {
            None
        } else {
            form_end(buf, q, depth + 1)
        }
    };
    match *buf.get(p)? {
        b'"' => string_end(buf, p + 1),
        b'(' | b'[' | b'{' => {
            let mut q = p + 1;
            loop {
                q = skip_ws(buf, q);
                match buf.get(q)? {
                    b')' | b']' | b'}' => return Some(q + 1),
                    _ => q = form_end(buf, q, depth + 1)?,
                }
            }
        }
        // a stray closer is a form of its own: the reader reports it
        b')' | b']' | b'}' => Some(p + 1),
        b'\'' | b'`' | b'@' => next(p + 1),
        b'~' => next(if buf.get(p + 1) == Some(&b'@') { p + 2 } else { p + 1 }),
        b'^' => {
            let q = next(p + 1)?;
            next(q)
        }
        b'\\' => {
            let mut q = p + 1;
            if q >= buf.len() {
                return None;
            }
            q += match buf[q] {
                0..=0x7f => 1,
                0xc0..=0xdf => 2,
                0xe0..=0xef => 3,
                _ => 4,
            };
            while buf.get(q).is_some_and(|&b| !is_delim(b)) {
                q += 1;
            }
            Some(q.min(buf.len()))
        }
        b'#' => match buf.get(p + 1)? {
            b'"' => string_end(buf, p + 2),
            b'\'' | b'_' => next(p + 2),
            b'{' | b'(' | b'[' => form_end(buf, p + 1, depth + 1),
            _ => {
                // #? #?@ #:ns #tag : a prefix token, then a form
                let mut q = p + 1;
                while buf.get(q).is_some_and(|&b| !is_delim(b)) {
                    q += 1;
                }
                if q >= buf.len() {
                    return None;
                }
                next(q)
            }
        },
        _ => {
            let mut q = p;
            while buf.get(q).is_some_and(|&b| !is_delim(b)) {
                q += 1;
            }
            // a token at the very end may go on: wait for the line end
            if q >= buf.len() {
                None
            } else {
                Some(q)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form(s: &str) -> Option<&str> {
        let (st, e) = scan_form(s.as_bytes());
        e.map(|e| &s[st..e])
    }

    #[test]
    fn scans_forms() {
        assert_eq!(form("(+ 1 2)\n"), Some("(+ 1 2)"));
        assert_eq!(form("  ; c\n  :a "), Some(":a"));
        assert_eq!(form("(+ 1"), None);
        assert_eq!(form("foo"), None);
        assert_eq!(form("foo\n"), Some("foo"));
        assert_eq!(form("\"a)b\" x"), Some("\"a)b\""));
        assert_eq!(form("(a \"x)\" \\) ;)\n b)"), Some("(a \"x)\" \\) ;)\n b)"));
        assert_eq!(form("#?(:clj 1 :cljs 2)\n"), Some("#?(:clj 1 :cljs 2)"));
        assert_eq!(form("#?(:cljs :x)\n"), Some("#?(:cljs :x)"));
        assert_eq!(form("'(1 2)\n"), Some("'(1 2)"));
        assert_eq!(form("^:foo bar\n"), Some("^:foo bar"));
        assert_eq!(form("#'x\n"), Some("#'x"));
        assert_eq!(form("{::io/x 1 ::sets/x 2}\n"), Some("{::io/x 1 ::sets/x 2}"));
        assert_eq!(form("#inst \"2020\"\n"), Some("#inst \"2020\""));
        assert_eq!(form("#:a{:b 1}\n"), Some("#:a{:b 1}"));
        assert_eq!(form(")\n"), Some(")"));
        assert_eq!(form("\\newline "), Some("\\newline"));
        assert_eq!(form("   \n"), None);
    }

    #[test]
    fn edn_roundtrip() {
        let mut out = Vec::new();
        let n = edn_request_to_bencode(b"{:op \"eval\" :code \"(+ 1 2)\" :id 1 :x nil} {", &mut out).unwrap();
        assert_eq!(n, Some(41));
        let (v, _) = decode(&out).unwrap().unwrap();
        assert_eq!(v.get("op").unwrap().as_str(), Some("eval"));
        assert!(v.get("x").is_none());
        assert_eq!(edn_request_to_bencode(b"{:op \"ev", &mut Vec::new()), Ok(None));

        let mut reply = Vec::new();
        let m = Value::dict([
            ("id", Value::str("1")),
            ("status", Value::List(vec![Value::str("done"), Value::str("eval-error")])),
            ("value", Value::str("a\"b")),
        ]);
        encode(&m, &mut reply);
        let mut o = Vec::new();
        assert_eq!(bencode_replies_to_edn(&reply, &mut o), 1);
        assert_eq!(String::from_utf8(o).unwrap(), r#"{:id "1", :status #{:done :eval-error}, :value "a\"b"}"#);
    }

    #[test]
    fn tty_prompt_and_elision() {
        let mut t = Tty::new();
        let mut out = Vec::new();
        let mut clone = Vec::new();
        encode(&Value::dict([("new-session", Value::str("s1")), ("status", Value::List(vec![Value::str("done")]))]), &mut clone);
        t.replies(&clone, &mut out);
        assert!(t.next_request().is_none(), "no input yet");
        t.inbuf.extend_from_slice(b"#?(:cljs 1)\n(+ 1 2)\n");
        let req = t.next_request().unwrap();
        let (r, _) = decode(&req).unwrap().unwrap();
        let id = r.get("id").unwrap().as_str().unwrap().to_string();
        assert_eq!(r.get("code").unwrap().as_str(), Some("#?(:cljs 1)"));
        assert!(t.next_request().is_none(), "waits for done");
        // done without value: no prompt, next form is issued
        let mut done = Vec::new();
        encode(&Value::dict([("id", Value::str(&id)), ("status", Value::List(vec![Value::str("done")]))]), &mut done);
        t.replies(&done, &mut out);
        assert!(out.is_empty());
        let (r, _) = decode(&t.next_request().unwrap()).unwrap().unwrap();
        assert_eq!(r.get("code").unwrap().as_str(), Some("(+ 1 2)"));
        let id = r.get("id").unwrap().as_str().unwrap().to_string();
        let mut v = Vec::new();
        encode(&Value::dict([("id", Value::str(&id)), ("ns", Value::str("user")), ("value", Value::str("3"))]), &mut v);
        encode(&Value::dict([("id", Value::str(&id)), ("status", Value::List(vec![Value::str("done")]))]), &mut v);
        t.replies(&v, &mut out);
        assert_eq!(String::from_utf8(out).unwrap(), "3\nuser=> ");
    }
}
