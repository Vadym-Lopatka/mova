//! A small nREPL client: what `--ack`, `--connect` and `--interactive` need.
//!
//! Blocking, one connection, no threads. It speaks bencode or EDN. It is not
//! a library for tools; it is the "proof of concept" client of `nrepl.cmdline`.

use crate::bencode::{decode, encode, Value};
use crate::edn::{self, Edn};
use crate::transport::{scan_form, Codec};
use std::collections::BTreeMap;
use std::io::{self, BufRead, Read, Write};
use std::net::TcpStream;
use std::os::unix::net::UnixStream;
use std::time::Duration;

/// Anything a client can talk over.
pub trait Stream: Read + Write + Send {
    fn set_read_timeout(&mut self, d: Option<Duration>) -> io::Result<()>;
}

impl Stream for TcpStream {
    fn set_read_timeout(&mut self, d: Option<Duration>) -> io::Result<()> {
        TcpStream::set_read_timeout(self, d)
    }
}
impl Stream for UnixStream {
    fn set_read_timeout(&mut self, d: Option<Duration>) -> io::Result<()> {
        UnixStream::set_read_timeout(self, d)
    }
}

/// Connects to `host:port`, or to a Unix socket at `socket`.
pub fn dial(host: &str, port: Option<u16>, socket: Option<&str>) -> io::Result<Box<dyn Stream>> {
    if let Some(p) = socket {
        return Ok(Box::new(UnixStream::connect(p)?));
    }
    let port = port.ok_or_else(|| io::Error::other("no port"))?;
    let s = TcpStream::connect((host, port))?;
    let _ = s.set_nodelay(true);
    Ok(Box::new(s))
}

/// One reply, the fields a REPL cares about.
#[derive(Debug, Default, Clone)]
pub struct Msg {
    pub id: Option<String>,
    pub out: Option<String>,
    pub err: Option<String>,
    pub value: Option<String>,
    pub ns: Option<String>,
    pub new_session: Option<String>,
    pub status: Vec<String>,
}

impl Msg {
    pub fn done(&self) -> bool {
        self.status.iter().any(|s| s == "done")
    }
}

/// A client over one connection.
pub struct Client {
    s: Box<dyn Stream>,
    codec: Codec,
    buf: Vec<u8>,
}

impl Client {
    pub fn new(s: Box<dyn Stream>, codec: Codec) -> Client {
        Client { s, codec, buf: Vec::new() }
    }

    /// Sends a message of string fields (an `i64` for `port`-like fields is
    /// written when the value parses and the key is in `ints`).
    pub fn send(&mut self, fields: &[(&str, &str)]) -> io::Result<()> {
        let mut out = Vec::new();
        match self.codec {
            Codec::Edn => {
                out.push(b'{');
                for (i, (k, v)) in fields.iter().enumerate() {
                    if i > 0 {
                        out.push(b' ');
                    }
                    out.extend_from_slice(format!(":{k} ").as_bytes());
                    if *k == "port" {
                        out.extend_from_slice(v.as_bytes());
                    } else {
                        edn::write_string(&mut out, v.as_bytes());
                    }
                }
                out.push(b'}');
            }
            _ => {
                let d: BTreeMap<Vec<u8>, Value> = fields
                    .iter()
                    .map(|(k, v)| {
                        let val = if *k == "port" { v.parse().map(Value::Int).unwrap_or_else(|_| Value::str(v)) } else { Value::str(v) };
                        (k.as_bytes().to_vec(), val)
                    })
                    .collect();
                encode(&Value::Dict(d), &mut out);
            }
        }
        self.s.write_all(&out)?;
        self.s.flush()
    }

    pub fn set_timeout(&mut self, d: Option<Duration>) -> io::Result<()> {
        self.s.set_read_timeout(d)
    }

    /// The next reply. `Ok(None)` at end of stream.
    pub fn recv(&mut self) -> io::Result<Option<Msg>> {
        loop {
            if let Some(m) = self.parse()? {
                return Ok(Some(m));
            }
            let mut tmp = [0u8; 8192];
            let n = self.s.read(&mut tmp)?;
            if n == 0 {
                return Ok(None);
            }
            self.buf.extend_from_slice(&tmp[..n]);
        }
    }

    fn parse(&mut self) -> io::Result<Option<Msg>> {
        let bad = |e: &dyn std::fmt::Display| io::Error::new(io::ErrorKind::InvalidData, e.to_string());
        match self.codec {
            Codec::Edn => match edn::parse(&self.buf, 0, false) {
                Err(edn::Stop::Need) => Ok(None),
                Err(edn::Stop::Bad(w)) => Err(bad(&w)),
                Ok((Edn::Map(m), used)) => {
                    self.buf.drain(..used);
                    let mut msg = Msg::default();
                    let text = |v: &Edn| match v {
                        Edn::Str(s) | Edn::Kw(s) | Edn::Sym(s) => Some(s.clone()),
                        Edn::Int(n) => Some(n.to_string()),
                        _ => None,
                    };
                    for (k, v) in &m {
                        let Edn::Kw(k) = k else { continue };
                        match k.as_str() {
                            "id" => msg.id = text(v),
                            "out" => msg.out = text(v),
                            "err" => msg.err = text(v),
                            "value" => msg.value = text(v),
                            "ns" => msg.ns = text(v),
                            "new-session" => msg.new_session = text(v),
                            "status" => {
                                if let Edn::Set(l) | Edn::Vec(l) | Edn::List(l) = v {
                                    msg.status = l.iter().filter_map(text).collect();
                                }
                            }
                            _ => {}
                        }
                    }
                    Ok(Some(msg))
                }
                Ok(_) => Err(bad(&"reply is not a map")),
            },
            _ => match decode(&self.buf).map_err(|e| bad(&e))? {
                None => Ok(None),
                Some((Value::Dict(m), used)) => {
                    self.buf.drain(..used);
                    let s = |k: &str| match m.get(k.as_bytes()) {
                        Some(Value::Bytes(b)) => Some(String::from_utf8_lossy(b).into_owned()),
                        _ => None,
                    };
                    Ok(Some(Msg {
                        id: s("id"),
                        out: s("out"),
                        err: s("err"),
                        value: s("value"),
                        ns: s("ns"),
                        new_session: s("new-session"),
                        status: match m.get(b"status".as_slice()) {
                            Some(Value::List(l)) => l
                                .iter()
                                .filter_map(|x| match x {
                                    Value::Bytes(b) => Some(String::from_utf8_lossy(b).into_owned()),
                                    _ => None,
                                })
                                .collect(),
                            _ => vec![],
                        },
                    }))
                }
                Some(_) => Err(bad(&"reply is not a dict")),
            },
        }
    }
}

/// `--ack`: tells the server on `ack_port` that this server listens on `my_port`.
/// Like `nrepl.ack/send-ack`: send `{op ack, port N}`, read the answer (up to 1 s each).
pub fn send_ack(my_port: u16, ack_port: u16, codec: Codec) -> io::Result<()> {
    let stream = dial("localhost", Some(ack_port), None)?;
    let mut c = Client::new(stream, codec);
    c.set_timeout(Some(Duration::from_millis(1000)))?;
    c.send(&[("op", "ack"), ("port", &my_port.to_string())])?;
    // consume the answer so the other side ends cleanly
    loop {
        match c.recv() {
            Ok(Some(m)) if !m.done() => continue,
            _ => return Ok(()),
        }
    }
}

/// What the REPL prints and how.
pub struct ReplOptions {
    pub codec: Codec,
    pub color: bool,
    /// The lines printed first (version information).
    pub intro: String,
}

/// The built-in REPL (`nrepl.cmdline/run-repl-with-transport`): clone a
/// session, then prompt, read a form from `input`, evaluate it, print `out`,
/// `err` and `value` as they come. Ends at end of input or `exit`/`quit`/
/// `(exit)`/`(quit)`. Returns when done; the caller exits the process.
pub fn run_repl(stream: Box<dyn Stream>, opts: &ReplOptions, input: &mut dyn BufRead, out: &mut dyn Write) -> io::Result<()> {
    let mut c = Client::new(stream, opts.codec);
    writeln!(out, "{}", opts.intro)?;
    c.send(&[("op", "clone"), ("id", "nrepl.cmdline-clone")])?;
    let session = loop {
        match c.recv()? {
            Some(m) if m.new_session.is_some() => break m.new_session.unwrap_or_default(),
            Some(_) => {}
            None => return Err(io::Error::other("connection closed")),
        }
    };
    let mut ns = "user".to_string();
    let mut text: Vec<u8> = Vec::new();
    let mut n = 0u64;
    loop {
        write!(out, "{ns}=> ")?;
        out.flush()?;
        // read until one whole form is in `text`
        let code = loop {
            let (start, end) = scan_form(&text);
            if let Some(end) = end {
                let code = String::from_utf8_lossy(&text[start..end]).into_owned();
                text.drain(..end);
                break Some(code);
            }
            let mut line = Vec::new();
            if input.read_until(b'\n', &mut line)? == 0 {
                break None;
            }
            text.extend_from_slice(&line);
        };
        let Some(code) = code else { return Ok(()) };
        if matches!(code.as_str(), "exit" | "quit" | "(exit)" | "(quit)") {
            return Ok(());
        }
        n += 1;
        let id = format!("nrepl.cmdline-{n}");
        c.send(&[("op", "eval"), ("code", &code), ("id", &id), ("session", &session)])?;
        loop {
            let Some(m) = c.recv()? else { return Err(io::Error::other("connection closed")) };
            if let Some(o) = &m.out {
                write!(out, "{o}")?;
            }
            if let Some(e) = &m.err {
                if opts.color {
                    write!(out, "\x1b[31m{e}\x1b[m")?;
                } else {
                    write!(out, "{e}")?;
                }
            }
            if let (Some(v), Some(mid)) = (&m.value, &m.id) {
                if mid.starts_with("nrepl.cmdline-") {
                    if opts.color {
                        writeln!(out, "\x1b[34m{v}\x1b[m")?;
                    } else {
                        writeln!(out, "{v}")?;
                    }
                }
            }
            if let Some(x) = &m.ns {
                ns = x.clone();
            }
            out.flush()?;
            if m.done() && m.id.as_deref() == Some(id.as_str()) {
                break;
            }
        }
    }
}
