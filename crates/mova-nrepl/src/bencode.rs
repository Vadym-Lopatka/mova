//! Bencode: a zero-copy request parser, a sorted-key encoder, and a small
//! owned `Value` for clients and tests.
//!
//! Parser rules follow the JVM (`nrepl.bencode`): a message is a top-level
//! dict; keys are byte strings; values are ints, byte strings, lists, dicts.
//! A later duplicate key wins. Anything the JVM would throw on (top-level
//! value that is not a dict, non-string key, bad digits, a dict key without a
//! value) is a `DecodeError`; the server closes the connection, like the JVM
//! does. Nesting deeper than `MAX_DEPTH` is also an error.

use std::collections::BTreeMap;

/// Nesting limit for lists and dicts (the JVM would overflow its stack long before).
pub const MAX_DEPTH: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodeError(pub &'static str);

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "bencode: {}", self.0)
    }
}

/// Why a scan stopped: more bytes are needed (with a lower bound on the
/// total buffer length that makes a retry worthwhile), or the input is bad.
#[derive(Debug, Clone, Copy)]
enum Stop {
    Need(usize),
    Bad(&'static str),
}

type Scan<T> = Result<T, Stop>;

fn bad<T>(why: &'static str) -> Scan<T> {
    Err(Stop::Bad(why))
}

// ---------------------------------------------------------------------------
// scanning
// ---------------------------------------------------------------------------

/// Parses `digits` followed by `delim` starting at `pos` (no sign).
/// Returns (value, position after the delimiter).
fn scan_uint(buf: &[u8], mut pos: usize, delim: u8) -> Scan<(usize, usize)> {
    let start = pos;
    let mut n: usize = 0;
    loop {
        let Some(&b) = buf.get(pos) else {
            return Err(Stop::Need(pos + 1));
        };
        if b == delim {
            if pos == start {
                return bad("empty number");
            }
            return Ok((n, pos + 1));
        }
        if !b.is_ascii_digit() {
            return bad("bad digit");
        }
        n = match n.checked_mul(10).and_then(|n| n.checked_add((b - b'0') as usize)) {
            Some(n) => n,
            None => return bad("number too large"),
        };
        pos += 1;
    }
}

/// `pos` is just after the `i`. Returns (value, position after `e`).
fn scan_int(buf: &[u8], pos: usize) -> Scan<(i64, usize)> {
    let (neg, p) = match buf.get(pos) {
        None => return Err(Stop::Need(pos + 1)),
        Some(b'-') => (true, pos + 1),
        Some(_) => (false, pos),
    };
    let (n, next) = scan_uint(buf, p, b'e')?;
    let n = n as u64;
    let v = if neg {
        if n == 1 << 63 {
            i64::MIN
        } else {
            match i64::try_from(n) {
                Ok(n) => -n,
                Err(_) => return bad("int out of range"),
            }
        }
    } else {
        match i64::try_from(n) {
            Ok(n) => n,
            Err(_) => return bad("int out of range"),
        }
    };
    Ok((v, next))
}

/// Byte string at `pos`. Returns the payload range (start, end).
fn scan_bytes(buf: &[u8], pos: usize) -> Scan<(usize, usize)> {
    let (len, p) = scan_uint(buf, pos, b':')?;
    let end = match p.checked_add(len) {
        Some(e) => e,
        None => return bad("length too large"),
    };
    if buf.len() < end {
        return Err(Stop::Need(end));
    }
    Ok((p, end))
}

/// Skips (and validates) one value. Returns the position after it.
fn skip_value(buf: &[u8], pos: usize, depth: usize) -> Scan<usize> {
    match buf.get(pos) {
        None => Err(Stop::Need(pos + 1)),
        Some(b'i') => scan_int(buf, pos + 1).map(|(_, p)| p),
        Some(b'0'..=b'9') => scan_bytes(buf, pos).map(|(_, e)| e),
        Some(b'l') => {
            if depth >= MAX_DEPTH {
                return bad("nested too deep");
            }
            let mut p = pos + 1;
            loop {
                match buf.get(p) {
                    None => return Err(Stop::Need(p + 1)),
                    Some(b'e') => return Ok(p + 1),
                    Some(_) => p = skip_value(buf, p, depth + 1)?,
                }
            }
        }
        Some(b'd') => {
            if depth >= MAX_DEPTH {
                return bad("nested too deep");
            }
            let mut p = pos + 1;
            loop {
                match buf.get(p) {
                    None => return Err(Stop::Need(p + 1)),
                    Some(b'e') => return Ok(p + 1),
                    Some(b'0'..=b'9') => {
                        let (_, kend) = scan_bytes(buf, p)?;
                        p = skip_value(buf, kend, depth + 1)?;
                    }
                    Some(_) => return bad("dict key must be a string"),
                }
            }
        }
        Some(_) => bad("unexpected byte"),
    }
}

// ---------------------------------------------------------------------------
// borrowed values and the request struct
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Int,
    Bytes,
    List,
    Dict,
}

/// One validated bencode value, borrowed from the read buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Val<'a> {
    raw: &'a [u8],
    kind: Kind,
    /// For byte strings: the payload. Otherwise empty.
    payload: &'a [u8],
}

impl<'a> Val<'a> {
    /// `buf[pos..end]` must be one value that `skip_value` accepted.
    fn at(buf: &'a [u8], pos: usize, end: usize) -> Val<'a> {
        let raw = &buf[pos..end];
        match raw[0] {
            b'i' => Val { raw, kind: Kind::Int, payload: &[] },
            b'l' => Val { raw, kind: Kind::List, payload: &[] },
            b'd' => Val { raw, kind: Kind::Dict, payload: &[] },
            _ => {
                // validated: digits, ':', payload
                let colon = raw.iter().position(|&b| b == b':').unwrap_or(0);
                Val { raw, kind: Kind::Bytes, payload: &raw[(colon + 1).min(raw.len())..] }
            }
        }
    }

    /// The value exactly as it is on the wire. Used to echo `id`, `session`, `op`.
    pub fn raw(&self) -> &'a [u8] {
        self.raw
    }
    pub fn kind(&self) -> Kind {
        self.kind
    }
    pub fn as_bytes(&self) -> Option<&'a [u8]> {
        (self.kind == Kind::Bytes).then_some(self.payload)
    }
    pub fn as_str(&self) -> Option<&'a str> {
        self.as_bytes().and_then(|b| std::str::from_utf8(b).ok())
    }
    pub fn as_int(&self) -> Option<i64> {
        if self.kind != Kind::Int {
            return None;
        }
        scan_int(self.raw, 1).ok().map(|(n, _)| n)
    }
}

macro_rules! known_keys {
    ($( $idx:ident, $name:literal, $method:ident; )*) => {
        #[derive(Clone, Copy)]
        enum K { $( $idx, )* N }
        const KEYS: [&[u8]; K::N as usize] = [ $( $name.as_bytes(), )* ];
        impl<'a> Request<'a> {
            $(
                #[doc = concat!("The `", $name, "` key, if present.")]
                pub fn $method(&self) -> Option<Val<'a>> {
                    self.known[K::$idx as usize]
                }
            )*
        }
    };
}

known_keys! {
    Op, "op", op;
    Id, "id", id;
    Session, "session", session;
    Code, "code", code;
    Ns, "ns", ns;
    File, "file", file;
    FileName, "file-name", file_name;
    FilePath, "file-path", file_path;
    Line, "line", line;
    Column, "column", column;
    Stdin, "stdin", stdin;
    InterruptId, "interrupt-id", interrupt_id;
    Prefix, "prefix", prefix;
    Sym, "sym", sym;
    Verbose, "verbose?", verbose;
    Eval, "eval", eval;
    ReadCond, "read-cond", read_cond;
    Options, "options", options;
    CompleteFn, "complete-fn", complete_fn;
    LookupFn, "lookup-fn", lookup_fn;
}

const INLINE_EXTRAS: usize = 6;

type Extra<'a> = (&'a [u8], Val<'a>);

/// One decoded request. Known keys are in a fixed table (no allocation);
/// every other key is kept as (key, raw value) in a small inline array that
/// spills to a `Vec` only past 6 entries.
#[derive(Clone)]
pub struct Request<'a> {
    /// The whole message, `d...e`.
    raw: &'a [u8],
    known: [Option<Val<'a>>; K::N as usize],
    inline: [Option<Extra<'a>>; INLINE_EXTRAS],
    n_inline: usize,
    more: Vec<Extra<'a>>,
}

impl<'a> Request<'a> {
    fn new(raw: &'a [u8]) -> Self {
        Request {
            raw,
            known: [None; K::N as usize],
            inline: [None; INLINE_EXTRAS],
            n_inline: 0,
            more: Vec::new(),
        }
    }

    /// The message bytes.
    pub fn raw(&self) -> &'a [u8] {
        self.raw
    }

    /// Any key, known or not. The last duplicate wins.
    pub fn get(&self, key: &[u8]) -> Option<Val<'a>> {
        if let Some(i) = KEYS.iter().position(|k| *k == key) {
            return self.known[i];
        }
        self.extras().filter(|(k, _)| *k == key).map(|(_, v)| v).last()
    }

    /// Keys that are not in the known table, in wire order.
    pub fn extras(&self) -> impl Iterator<Item = Extra<'a>> + '_ {
        self.inline[..self.n_inline].iter().flatten().copied().chain(self.more.iter().copied())
    }

    fn push_extra(&mut self, key: &'a [u8], v: Val<'a>) {
        if self.n_inline < INLINE_EXTRAS {
            self.inline[self.n_inline] = Some((key, v));
            self.n_inline += 1;
        } else {
            self.more.push((key, v));
        }
    }

    /// Copies the message so another thread can keep it.
    pub fn to_owned_request(&self) -> OwnedRequest {
        OwnedRequest { buf: self.raw.into() }
    }
}

/// A request that owns its bytes. Use `request()` to get the borrowed view.
#[derive(Clone, Debug)]
pub struct OwnedRequest {
    buf: Box<[u8]>,
}

impl OwnedRequest {
    pub fn request(&self) -> Request<'_> {
        match parse_request(&self.buf) {
            Ok(Parsed::Message(r, _)) => r,
            // Cannot happen: `buf` was a validated message.
            _ => Request::new(&[]),
        }
    }
}

/// Result of `parse_request`.
pub enum Parsed<'a> {
    /// One message and the number of bytes it used.
    Message(Request<'a>, usize),
    /// Not complete. Do not call again before the buffer holds at least this many bytes.
    Need(usize),
}

/// Parses one request from the front of `buf`.
pub fn parse_request(buf: &[u8]) -> Result<Parsed<'_>, DecodeError> {
    match parse_inner(buf) {
        Ok((req, used)) => Ok(Parsed::Message(req, used)),
        Err(Stop::Need(n)) => Ok(Parsed::Need(n)),
        Err(Stop::Bad(why)) => Err(DecodeError(why)),
    }
}

fn parse_inner(buf: &[u8]) -> Scan<(Request<'_>, usize)> {
    match buf.first() {
        None => return Err(Stop::Need(1)),
        Some(b'd') => {}
        Some(_) => return bad("message must be a dict"),
    }
    let mut pos = 1;
    let mut req = Request::new(&[]);
    loop {
        match buf.get(pos) {
            None => return Err(Stop::Need(pos + 1)),
            Some(b'e') => {
                pos += 1;
                break;
            }
            Some(b'0'..=b'9') => {}
            Some(_) => return bad("dict key must be a string"),
        }
        // Scanning a byte string is O(1) however long it is, so a rescan
        // after a short read costs little.
        let (ks, ke) = scan_bytes(buf, pos)?;
        let key = &buf[ks..ke];
        let vend = skip_value(buf, ke, 1)?;
        let val = Val::at(buf, ke, vend);
        pos = vend;
        match KEYS.iter().position(|k| *k == key) {
            Some(i) => req.known[i] = Some(val),
            None => req.push_extra(key, val),
        }
    }
    req.raw = &buf[..pos];
    Ok((req, pos))
}

// ---------------------------------------------------------------------------
// encoder
// ---------------------------------------------------------------------------

/// A value to encode. `Copy`, so a reply is built on the stack.
#[derive(Clone, Copy, Debug)]
pub enum V<'a> {
    Int(i64),
    Str(&'a str),
    Bytes(&'a [u8]),
    /// Already encoded bencode, copied as is.
    Raw(&'a [u8]),
    /// A list of byte strings.
    Strs(&'a [&'a str]),
    List(&'a [V<'a>]),
    /// Keys are sorted on output; the input order does not matter.
    Dict(&'a [(&'a str, V<'a>)]),
}

pub fn write_int(out: &mut Vec<u8>, n: i64) {
    out.push(b'i');
    if n < 0 {
        out.push(b'-');
    }
    write_uint(out, n.unsigned_abs());
    out.push(b'e');
}

pub fn write_bytes(out: &mut Vec<u8>, b: &[u8]) {
    write_uint(out, b.len() as u64);
    out.push(b':');
    out.extend_from_slice(b);
}

fn write_uint(out: &mut Vec<u8>, mut u: u64) {
    let mut tmp = [0u8; 20];
    let mut i = tmp.len();
    loop {
        i -= 1;
        tmp[i] = b'0' + (u % 10) as u8;
        u /= 10;
        if u == 0 {
            break;
        }
    }
    out.extend_from_slice(&tmp[i..]);
}

pub fn write_value(out: &mut Vec<u8>, v: V<'_>) {
    match v {
        V::Int(n) => write_int(out, n),
        V::Str(s) => write_bytes(out, s.as_bytes()),
        V::Bytes(b) => write_bytes(out, b),
        V::Raw(r) => out.extend_from_slice(r),
        V::Strs(l) => {
            out.push(b'l');
            for s in l {
                write_bytes(out, s.as_bytes());
            }
            out.push(b'e');
        }
        V::List(l) => {
            out.push(b'l');
            for x in l {
                write_value(out, *x);
            }
            out.push(b'e');
        }
        V::Dict(d) => write_dict(out, d),
    }
}

type Entry<'a> = (&'a str, V<'a>);

fn emit_dict(out: &mut Vec<u8>, entries: &[Entry<'_>]) {
    out.push(b'd');
    for (k, v) in entries {
        write_bytes(out, k.as_bytes());
        write_value(out, *v);
    }
    out.push(b'e');
}

/// Writes a dict with keys in byte order. Up to 24 entries are sorted on the
/// stack; more use a `Vec`.
pub fn write_dict(out: &mut Vec<u8>, entries: &[Entry<'_>]) {
    if entries.windows(2).all(|w| w[0].0.as_bytes() < w[1].0.as_bytes()) {
        return emit_dict(out, entries);
    }
    const STACK: usize = 24;
    if entries.len() <= STACK {
        let mut tmp: [Entry<'_>; STACK] = [("", V::Int(0)); STACK];
        tmp[..entries.len()].copy_from_slice(entries);
        let s = &mut tmp[..entries.len()];
        s.sort_unstable_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
        emit_dict(out, s);
    } else {
        let mut v = entries.to_vec();
        v.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
        emit_dict(out, &v);
    }
}

/// Writes a reply: `fields` plus `id` (if the request had one, echoed as it
/// came on the wire) and `session`. Keys come out sorted.
pub fn write_reply(out: &mut Vec<u8>, id: Option<&[u8]>, session: V<'_>, fields: &[Entry<'_>]) {
    const STACK: usize = 16;
    if fields.len() + 2 <= STACK {
        let mut tmp: [Entry<'_>; STACK] = [("", V::Int(0)); STACK];
        let mut n = fields.len();
        tmp[..n].copy_from_slice(fields);
        if let Some(id) = id {
            tmp[n] = ("id", V::Raw(id));
            n += 1;
        }
        tmp[n] = ("session", session);
        n += 1;
        write_dict(out, &tmp[..n]);
    } else {
        let mut v: Vec<Entry<'_>> = fields.to_vec();
        if let Some(id) = id {
            v.push(("id", V::Raw(id)));
        }
        v.push(("session", session));
        write_dict(out, &v);
    }
}

// ---------------------------------------------------------------------------
// owned values (clients, tests, tools)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Int(i64),
    Bytes(Vec<u8>),
    List(Vec<Value>),
    Dict(BTreeMap<Vec<u8>, Value>),
}

impl Value {
    pub fn str(s: &str) -> Value {
        Value::Bytes(s.as_bytes().to_vec())
    }
    pub fn dict<const N: usize>(items: [(&str, Value); N]) -> Value {
        Value::Dict(items.into_iter().map(|(k, v)| (k.as_bytes().to_vec(), v)).collect())
    }
    pub fn get(&self, k: &str) -> Option<&Value> {
        match self {
            Value::Dict(m) => m.get(k.as_bytes()),
            _ => None,
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Bytes(b) => std::str::from_utf8(b).ok(),
            _ => None,
        }
    }
}

/// Decodes one value (any type) from the front of `buf`.
/// `Ok(None)` = need more bytes. `Ok(Some((v, used)))` = done.
pub fn decode(buf: &[u8]) -> Result<Option<(Value, usize)>, DecodeError> {
    let end = match skip_value(buf, 0, 0) {
        Ok(end) => end,
        Err(Stop::Need(_)) => return Ok(None),
        Err(Stop::Bad(why)) => return Err(DecodeError(why)),
    };
    // validated above, so `build` cannot fail
    match build(buf, 0) {
        Ok((v, _)) => Ok(Some((v, end))),
        Err(_) => Err(DecodeError("internal")),
    }
}

fn build(buf: &[u8], pos: usize) -> Scan<(Value, usize)> {
    match buf.get(pos) {
        None => Err(Stop::Need(pos + 1)),
        Some(b'i') => scan_int(buf, pos + 1).map(|(n, p)| (Value::Int(n), p)),
        Some(b'l') => {
            let mut items = Vec::new();
            let mut p = pos + 1;
            while buf.get(p) != Some(&b'e') {
                let (v, np) = build(buf, p)?;
                items.push(v);
                p = np;
            }
            Ok((Value::List(items), p + 1))
        }
        Some(b'd') => {
            let mut m = BTreeMap::new();
            let mut p = pos + 1;
            while buf.get(p) != Some(&b'e') {
                let (ks, ke) = scan_bytes(buf, p)?;
                let (v, np) = build(buf, ke)?;
                m.insert(buf[ks..ke].to_vec(), v);
                p = np;
            }
            Ok((Value::Dict(m), p + 1))
        }
        Some(_) => scan_bytes(buf, pos).map(|(s, e)| (Value::Bytes(buf[s..e].to_vec()), e)),
    }
}

pub fn encode(v: &Value, out: &mut Vec<u8>) {
    match v {
        Value::Int(n) => write_int(out, *n),
        Value::Bytes(b) => write_bytes(out, b),
        Value::List(l) => {
            out.push(b'l');
            for x in l {
                encode(x, out);
            }
            out.push(b'e');
        }
        Value::Dict(m) => {
            out.push(b'd');
            for (k, x) in m {
                write_bytes(out, k);
                encode(x, out);
            }
            out.push(b'e');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(buf: &[u8]) -> (Request<'_>, usize) {
        match parse_request(buf) {
            Ok(Parsed::Message(r, n)) => (r, n),
            Ok(Parsed::Need(n)) => panic!("incomplete, need {n}"),
            Err(e) => panic!("{e}"),
        }
    }

    #[test]
    fn known_and_unknown_keys() {
        let b = b"d2:op4:eval2:id1:73:foo3:bar4:codei5ee";
        let (r, used) = msg(b);
        assert_eq!(used, b.len());
        assert_eq!(r.op().unwrap().as_str(), Some("eval"));
        assert_eq!(r.id().unwrap().raw(), b"1:7");
        assert_eq!(r.code().unwrap().as_int(), Some(5));
        assert_eq!(r.get(b"foo").unwrap().as_str(), Some("bar"));
        assert!(r.session().is_none());
        assert_eq!(r.extras().count(), 1);
    }

    #[test]
    fn extras_spill_past_inline() {
        let mut b = b"d".to_vec();
        for i in 0..20 {
            let k = format!("k{i}");
            b.extend_from_slice(format!("{}:{}i{}e", k.len(), k, i).as_bytes());
        }
        b.push(b'e');
        let (r, _) = msg(&b);
        assert_eq!(r.extras().count(), 20);
        assert_eq!(r.get(b"k19").unwrap().as_int(), Some(19));
        assert_eq!(r.get(b"k0").unwrap().as_int(), Some(0));
    }

    #[test]
    fn duplicate_key_last_wins() {
        let (r, _) = msg(b"d2:op1:a2:op1:be");
        assert_eq!(r.op().unwrap().as_str(), Some("b"));
        let (r, _) = msg(b"d1:xi1e1:xi2ee");
        assert_eq!(r.get(b"x").unwrap().as_int(), Some(2));
    }

    #[test]
    fn nested_values_are_kept_raw() {
        let b = b"d2:id1:12:opld1:ai1eel1:beee";
        let (r, _) = msg(b);
        assert_eq!(r.op().unwrap().kind(), Kind::List);
        assert_eq!(r.op().unwrap().raw(), b"ld1:ai1eel1:bee");
    }

    #[test]
    fn many_messages_in_one_buffer() {
        let one = b"d2:op8:describe2:id1:1e";
        let mut buf = Vec::new();
        for _ in 0..5 {
            buf.extend_from_slice(one);
        }
        let mut pos = 0;
        let mut n = 0;
        while pos < buf.len() {
            let (r, used) = msg(&buf[pos..]);
            assert_eq!(r.op().unwrap().as_str(), Some("describe"));
            pos += used;
            n += 1;
        }
        assert_eq!(n, 5);
    }

    #[test]
    fn split_at_every_byte() {
        let b = b"d4:code7:(+ 1 2)2:id1:12:op4:evale";
        for cut in 0..b.len() {
            match parse_request(&b[..cut]) {
                Ok(Parsed::Need(n)) => assert!(n > cut && n <= b.len(), "cut {cut} need {n}"),
                _ => panic!("cut {cut}: not Need"),
            }
        }
        let (_, used) = msg(b);
        assert_eq!(used, b.len());
    }

    #[test]
    fn need_hint_skips_to_end_of_big_string() {
        let mut b = b"d4:code1000000:".to_vec();
        b.extend_from_slice(&[b'a'; 1000]);
        match parse_request(&b) {
            Ok(Parsed::Need(n)) => assert_eq!(n, 15 + 1_000_000),
            _ => panic!("expected Need"),
        }
    }

    #[test]
    fn one_megabyte_string() {
        let code = vec![b'x'; 1 << 20];
        let mut b = b"d4:code1048576:".to_vec();
        b.extend_from_slice(&code);
        b.extend_from_slice(b"2:op4:evale");
        let (r, used) = msg(&b);
        assert_eq!(used, b.len());
        assert_eq!(r.code().unwrap().as_bytes().unwrap().len(), 1 << 20);
    }

    #[test]
    fn binary_payload_is_fine() {
        let (r, _) = msg(b"d4:code4:\x00\xff\r\ne");
        assert_eq!(r.code().unwrap().as_bytes(), Some(&b"\x00\xff\r\n"[..]));
        assert_eq!(r.code().unwrap().as_str(), None);
    }

    #[test]
    fn malformed() {
        let cases: &[&[u8]] = &[
            b"x",                            // not a dict
            b"i5e",                          // not a dict
            b"\n",                           // telnet newline
            b"di1ei2ee",                     // int key
            b"d2:opee",                      // 'e' where a value is expected
            b"d2:op-1:ae",                   // negative length
            b"d2:op1x:ae",                   // bad digit in length
            b"d2:op:ae",                     // empty length
            b"d2:opiXee",                    // bad int
            b"d2:opiee",                     // empty int
            b"d2:opi99999999999999999999ee", // int overflow
            b"d2:op99999999999999999999:e",  // length overflow
        ];
        for c in cases {
            assert!(parse_request(c).is_err(), "{:?}", String::from_utf8_lossy(c));
        }
        // incomplete is not malformed
        assert!(matches!(parse_request(b"d2:opl"), Ok(Parsed::Need(_))));
    }

    #[test]
    fn nesting_limit() {
        let deep = |n: usize| {
            let mut b = b"d1:x".to_vec();
            b.extend(std::iter::repeat(b'l').take(n));
            b.extend(std::iter::repeat(b'e').take(n));
            b.push(b'e');
            b
        };
        assert!(parse_request(&deep(MAX_DEPTH + 10)).is_err());
        assert!(matches!(parse_request(&deep(50)), Ok(Parsed::Message(..))));
    }

    #[test]
    fn int_edges() {
        let (r, _) = msg(b"d1:ai-5e1:bi0e1:ci9223372036854775807e1:di-9223372036854775808ee");
        assert_eq!(r.get(b"a").unwrap().as_int(), Some(-5));
        assert_eq!(r.get(b"b").unwrap().as_int(), Some(0));
        assert_eq!(r.get(b"c").unwrap().as_int(), Some(i64::MAX));
        assert_eq!(r.get(b"d").unwrap().as_int(), Some(i64::MIN));
        assert!(parse_request(b"d1:ai9223372036854775808ee").is_err());
    }

    #[test]
    fn encoder_sorts_keys() {
        let mut out = Vec::new();
        write_dict(
            &mut out,
            &[
                ("status", V::Strs(&["done"])),
                ("id", V::Str("1")),
                ("new-session", V::Str("x")),
                ("n", V::Int(-7)),
            ],
        );
        assert_eq!(out, b"d2:id1:11:ni-7e11:new-session1:x6:statusl4:doneee");
    }

    #[test]
    fn encoder_many_keys_and_nesting() {
        let names: Vec<String> = (0..40).rev().map(|i| format!("k{i:02}")).collect();
        let entries: Vec<(&str, V)> = names.iter().map(|n| (n.as_str(), V::Int(1))).collect();
        let mut out = Vec::new();
        write_dict(&mut out, &entries);
        assert!(out.starts_with(b"d3:k00i1e3:k01i1e"));
        let (v, used) = decode(&out).unwrap().unwrap();
        assert_eq!(used, out.len());
        let Value::Dict(m) = v else { panic!() };
        assert_eq!(m.len(), 40);

        let inner = [("b", V::Int(1)), ("a", V::List(&[V::Str("x"), V::Dict(&[])]))];
        let mut out = Vec::new();
        write_value(&mut out, V::Dict(&inner));
        assert_eq!(out, b"d1:al1:xdee1:bi1ee");
    }

    #[test]
    fn reply_adds_id_and_session() {
        let mut out = Vec::new();
        write_reply(&mut out, Some(b"1:7"), V::Str("S"), &[("status", V::Strs(&["done"]))]);
        assert_eq!(out, b"d2:id1:77:session1:S6:statusl4:doneee");
        let mut out = Vec::new();
        write_reply(&mut out, None, V::Raw(b"i5e"), &[]);
        assert_eq!(out, b"d7:sessioni5ee");
    }

    #[test]
    fn owned_roundtrip() {
        let b = b"d2:op4:eval2:id1:1e";
        let (r, _) = msg(b);
        let o = r.to_owned_request();
        drop(r);
        assert_eq!(o.request().op().unwrap().as_str(), Some("eval"));
    }

    #[test]
    fn owned_value_codec() {
        let v = Value::dict([("a", Value::List(vec![Value::Int(1), Value::str("x")])), ("b", Value::Int(-3))]);
        let mut out = Vec::new();
        encode(&v, &mut out);
        let (back, used) = decode(&out).unwrap().unwrap();
        assert_eq!(used, out.len());
        assert_eq!(back, v);
        assert!(decode(&out[..out.len() - 1]).unwrap().is_none());
    }
}
