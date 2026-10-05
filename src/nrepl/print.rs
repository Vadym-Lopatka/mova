//! Print and caught options (design 5.3; JVM `nrepl.middleware.print` and
//! `nrepl.middleware.caught`), and the one function that turns a value into
//! reply messages.
//!
//! Request options (all optional):
//!
//! | key | meaning |
//! |---|---|
//! | `nrepl.middleware.print/print` | symbol of a var `(fn [value writer options])`; unresolved: a `.../error` message, then default printing |
//! | `.../options` | map handed to that fn as `options` (dict keys become keywords) |
//! | `.../stream?` | true for anything but an empty list (even `""`, `"0"`, `"false"`): send the text as `value` chunks, then a lone `{ns}` message |
//! | `.../buffer-size` | chunk size in bytes for streaming (default 1024; the JVM `CallbackBufferedOutputStream` rule, ported) |
//! | `.../quota` | characters printed per value; more: `truncated` status + `truncated-keys`; `<= 0` is an error |
//! | `.../keys` | list of reply keys to print (see `Keys` below) |
//! | `nrepl.middleware.caught/caught` | symbol of a fn that gets the throwable instead of the `err` text |
//! | `.../print?` | send the printed throwable as `nrepl.middleware.caught/throwable` |
//!
//! The printed text of a value without a `print` var is Mova's `pr-str`
//! (honouring `*print-length*` etc.). With a var the fn is called as
//! `(f value writer options)` on a `java.io.StringWriter`; the text is what
//! the writer holds afterwards (so `prn` and `pr-str` give "" there, exactly
//! as on the JVM). The quota cuts the finished text: an endless print stops at
//! Mova's realize cap, not at the quota.

use crate::error::RjError;
use crate::eval::Interp;
use crate::value::{Str, Symbol, Value};
use mova_nrepl::bencode::Value as Bv;
use mova_nrepl::{Responder, V};

pub(crate) const PRINT_ERROR: &str = "nrepl.middleware.print/error";
pub(crate) const CAUGHT_ERROR: &str = "nrepl.middleware.caught/error";
const TRUNCATED: &str = "nrepl.middleware.print/truncated";
const TRUNCATED_KEYS: &str = "nrepl.middleware.print/truncated-keys";
const THROWABLE: &str = "nrepl.middleware.caught/throwable";

/// What the client asked for. `PrintOpts::default()` is "plain `pr`, one message".
#[derive(Clone, Debug, Default)]
pub(crate) struct PrintOpts {
    pub print: Option<String>,
    pub options: Option<Bv>,
    pub stream: bool,
    pub buffer_size: Option<i64>,
    pub quota: Option<i64>,
    /// `Some` when the client gave `keys` (a list of strings).
    pub keys: Option<Vec<String>>,
    pub caught: Option<String>,
    pub caught_print: bool,
}

impl PrintOpts {
    /// True when nothing was asked for: the cheap path.
    pub(crate) fn is_plain(&self) -> bool {
        self.print.is_none()
            && !self.stream
            && self.quota.is_none()
            && self.keys.is_none()
            && self.buffer_size.is_none()
            && self.options.is_none()
    }
}

fn truthy(v: &mova_nrepl::Val<'_>) -> bool {
    // bencode has no booleans: only the empty list is false (JVM `booleanize-bencode-val`)
    v.raw() != b"le"
}

/// Reads the options off a request. `Err(())`: the JVM answers nothing at all
/// (a `keys` that is not a list).
pub(crate) fn parse(req: &mova_nrepl::Request<'_>) -> Result<PrintOpts, ()> {
    let mut o = PrintOpts::default();
    let s = |k: &str| req.get(k.as_bytes()).and_then(|v| v.as_str()).map(|s| s.to_string());
    o.print = s("nrepl.middleware.print/print");
    if let Some(v) = req.get(b"nrepl.middleware.print/options") {
        if let Ok(Some((b, _))) = mova_nrepl::bencode::decode(v.raw()) {
            o.options = Some(b);
        }
    }
    o.stream = req.get(b"nrepl.middleware.print/stream?").map(|v| truthy(&v)).unwrap_or(false);
    o.buffer_size = req.get(b"nrepl.middleware.print/buffer-size").and_then(|v| v.as_int());
    o.quota = req.get(b"nrepl.middleware.print/quota").and_then(|v| v.as_int());
    if let Some(v) = req.get(b"nrepl.middleware.print/keys") {
        match mova_nrepl::bencode::decode(v.raw()) {
            Ok(Some((Bv::List(l), _))) => o.keys = Some(l.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect()),
            _ => return Err(()),
        }
    }
    o.caught = s("nrepl.middleware.caught/caught");
    o.caught_print = req.get(b"nrepl.middleware.caught/print?").map(|v| truthy(&v)).unwrap_or(false);
    Ok(o)
}

// ---------------------------------------------------------------------------
// resolving and calling
// ---------------------------------------------------------------------------

/// `requiring-resolve`: the value of `ns/name`, loading `ns` first if needed.
pub(crate) fn resolve_fn(interp: &mut Interp, sym: &str) -> Option<Value> {
    let s = crate::reader::parse_symbol(sym);
    s.ns.as_ref()?;
    if let Some(f) = interp.lookup_global(&s) {
        return Some(f);
    }
    let ns = s.ns.clone()?;
    let req = interp.lookup_global(&Symbol::simple("require"))?;
    interp.call(&req, &[Value::Sym(Symbol::simple(ns))]).ok()?;
    interp.lookup_global(&s)
}

/// Converts a decoded request value to a Mova value (dict keys: keywords).
pub(crate) fn to_mova(b: &Bv) -> Value {
    use crate::value::{PMap, PVec};
    match b {
        Bv::Int(n) => Value::Int(*n),
        Bv::Bytes(_) => Value::Str(Str::from(b.as_str().unwrap_or(""))),
        Bv::List(l) => Value::Vector(l.iter().map(to_mova).collect::<PVec>()),
        Bv::Dict(d) => {
            let mut m = PMap::new();
            for (k, v) in d {
                m.insert(Value::Keyword(crate::keyword::Keyword::from(String::from_utf8_lossy(k).as_ref())), to_mova(v));
            }
            Value::Map(m)
        }
    }
}

fn text_of_writer(interp: &mut Interp, w: &Value) -> Result<String, RjError> {
    let f = interp.lookup_global(&Symbol::simple("str")).ok_or_else(|| RjError::other("str is not defined"))?;
    match interp.call(&f, &[w.clone()])? {
        Value::Str(s) => Ok(s.to_string()),
        other => Ok(crate::printer::display_str(&other)),
    }
}

fn illegal_arg(msg: &str) -> RjError {
    let chain: Vec<String> = ["java.lang.IllegalArgumentException", "java.lang.RuntimeException", "java.lang.Exception", "java.lang.Throwable"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    RjError::thrown(crate::errinfo::mk_exception(&chain, Some(msg.to_string()), Value::Nil, Value::Nil))
}

/// Prints `value` to text: the `print` fn if it resolved (`fnv`), else `pr`.
fn print_text(interp: &mut Interp, fnv: Option<&Value>, opts: &PrintOpts, value: &Value) -> Result<String, RjError> {
    match fnv {
        None => Ok(crate::builtins::strings::realize_all_pr_str(interp, std::slice::from_ref(value))?.pop().unwrap_or_default()),
        Some(f) => {
            let w = crate::hostclass::construct(interp, "java.io.StringWriter", &[], crate::reader::Span { start: 0, end: 0 })?;
            let o = opts.options.as_ref().map(to_mova).unwrap_or(Value::Nil);
            interp.call(f, &[value.clone(), w.clone(), o])?;
            text_of_writer(interp, &w)
        }
    }
}

// ---------------------------------------------------------------------------
// JVM CallbackBufferedOutputStream, ported (streaming chunks)
// ---------------------------------------------------------------------------

struct Chunker<'a> {
    buf: Vec<u8>,
    size: i64,
    emit: &'a mut dyn FnMut(&str),
}

fn single(b: u8) -> bool {
    b & 0x80 == 0
}
fn starts_char(b: u8) -> bool {
    single(b) || b & 0xC0 == 0xC0
}

impl Chunker<'_> {
    fn write(&mut self, b: &[u8]) {
        let mut off = 0usize;
        let mut len = b.len();
        let end = b.len();
        while off < end {
            let mut write_len = (self.size - self.buf.len() as i64).clamp(1, len as i64) as usize;
            let mut can_flush = false;
            if single(b[off + write_len - 1]) {
                can_flush = true;
            } else {
                while write_len + 1 < len {
                    if starts_char(b[off + write_len]) {
                        can_flush = true;
                        break;
                    }
                    write_len += 1;
                }
            }
            self.buf.extend_from_slice(&b[off..off + write_len]);
            if can_flush {
                self.maybe_flush(false);
            }
            off += write_len;
            len -= write_len;
        }
    }

    fn maybe_flush(&mut self, force: bool) {
        let size = self.buf.len();
        if size == 0 {
            return;
        }
        let content = String::from_utf8_lossy(&self.buf).into_owned();
        let length = content.chars().count();
        let last_nl = content.rfind('\n').map(|i| content[..i].chars().count() as i64).unwrap_or(-1);
        let to_flush = if force || size as i64 >= self.size { length } else { (last_nl + 1) as usize };
        if to_flush > 0 {
            let cut: usize = content.char_indices().nth(to_flush).map(|(i, _)| i).unwrap_or(content.len());
            (self.emit)(&content[..cut]);
            self.buf.clear();
            if to_flush < length {
                self.buf.extend_from_slice(content[cut..].as_bytes());
            }
        }
    }
}

/// Streams `text` as the JVM would: the encoder hands over at most 8192 bytes
/// at a time, each call goes through the chunk rule above, close flushes.
fn stream_chunks(text: &str, size: i64, emit: &mut dyn FnMut(&str)) {
    let mut c = Chunker { buf: Vec::new(), size, emit };
    let bytes = text.as_bytes();
    let mut pos = 0;
    while pos < bytes.len() {
        let mut end = (pos + 8192).min(bytes.len());
        while end < bytes.len() && (bytes[end] & 0xC0) == 0x80 {
            end -= 1;
        }
        c.write(&bytes[pos..end]);
        pos = end;
    }
    c.maybe_flush(true);
}

// ---------------------------------------------------------------------------
// the seam: a value (or a throwable) becomes reply messages
// ---------------------------------------------------------------------------

/// Which character prefix survives a quota, and whether the text was cut.
fn apply_quota(text: String, quota: Option<i64>) -> Result<(String, bool), RjError> {
    match quota {
        None => Ok((text, false)),
        Some(q) if q <= 0 => Err(illegal_arg(&format!("Invalid quota: {q}"))),
        Some(q) => {
            let q = q as usize;
            match text.char_indices().nth(q) {
                Some((i, _)) => Ok((text[..i].to_string(), true)),
                None => Ok((text, false)),
            }
        }
    }
}

/// The fields every message of a request carries because of `keys` (each key
/// prints as `nil`: on the JVM the client's string keys never match the
/// keyword keys of the reply, so `nil` is what gets printed).
pub(crate) fn keys_fields(opts: &PrintOpts) -> Vec<(String, String)> {
    opts.keys.iter().flatten().map(|k| (k.clone(), "nil".to_string())).collect()
}

/// A Mova value as bencode, for the `keys` case where the reply carries the
/// raw value (nil is the empty list, a map a dict, as the JVM transport does).
fn raw_bencode(v: &Value) -> Vec<u8> {
    fn go(v: &Value) -> Bv {
        match v {
            Value::Nil => Bv::List(vec![]),
            Value::Int(n) => Bv::Int(*n),
            Value::Str(s) => Bv::str(s.as_ref()),
            Value::Keyword(_) | Value::Sym(_) => Bv::str(crate::printer::display_str(v).trim_start_matches(':')),
            Value::Vector(x) | Value::List(x) => Bv::List(x.iter_cloned().map(|e| go(&e)).collect()),
            Value::Map(m) => {
                let mut d = std::collections::BTreeMap::new();
                for (k, val) in m.iter() {
                    let key = match &k.clone() {
                        Value::Str(s) => s.to_string(),
                        other => crate::printer::display_str(other).trim_start_matches(':').to_string(),
                    };
                    d.insert(key.into_bytes(), go(&val.clone()));
                }
                Bv::Dict(d)
            }
            other => Bv::str(&crate::printer::pr_str(other)),
        }
    }
    let mut out = Vec::new();
    mova_nrepl::bencode::encode(&go(v), &mut out);
    out
}

/// What `reply_value` needs from the request.
pub(crate) struct Printer {
    pub opts: PrintOpts,
    /// The `print` fn when the request named one and it resolved.
    pub fnv: Option<Value>,
}

impl Printer {
    /// Resolves the `print` var. An unresolved one gets the `.../error` message.
    pub(crate) fn new(interp: &mut Interp, opts: PrintOpts, reply: &Responder) -> Printer {
        let mut fnv = None;
        if let Some(p) = &opts.print {
            fnv = resolve_fn(interp, p);
            if fnv.is_none() {
                reply.send(&[
                    (PRINT_ERROR, V::Str(&format!("Couldn't resolve var {p}"))),
                    ("status", V::Strs(&[PRINT_ERROR])),
                ]);
            }
        }
        Printer { opts, fnv }
    }

    /// Sends the `{ns, value}` reply for one evaluated value.
    pub(crate) fn reply_value(&self, interp: &mut Interp, reply: &Responder, value: &Value, ns: &str) -> Result<(), RjError> {
        let o = &self.opts;
        if o.is_plain() {
            let text = print_text(interp, None, o, value)?;
            reply.send(&[("ns", V::Str(ns)), ("value", V::Str(&text))]);
            return Ok(());
        }
        let (text, truncated) = if o.keys.is_some() {
            (String::new(), false)
        } else {
            let t = print_text(interp, self.fnv.as_ref(), o, value);
            // a bad quota is checked before the print result is used, as the JVM does
            let t = t?;
            apply_quota(t, o.quota)?
        };
        if o.keys.is_some() {
            // client `keys`: the named reply keys print as "nil"; the value goes out raw
            let raw = raw_bencode(value);
            let kf = keys_fields(o);
            let mut f: Vec<(&str, V<'_>)> = vec![("ns", V::Str(ns)), ("value", V::Raw(&raw))];
            for (k, v) in &kf {
                f.retain(|(n, _)| n != k);
                f.push((k.as_str(), V::Str(v)));
            }
            reply.send(&f);
            return Ok(());
        }
        if o.stream {
            let size = o.buffer_size.unwrap_or(1024);
            stream_chunks(&text, size, &mut |c| {
                reply.send(&[("value", V::Str(c))]);
            });
            if truncated {
                reply.send(&[("status", V::Strs(&[TRUNCATED]))]);
            }
            reply.send(&[("ns", V::Str(ns))]);
        } else if truncated {
            reply.send(&[
                ("ns", V::Str(ns)),
                ("value", V::Str(&text)),
                (TRUNCATED_KEYS, V::Strs(&["value"])),
                ("status", V::Strs(&[TRUNCATED])),
            ]);
        } else {
            reply.send(&[("ns", V::Str(ns)), ("value", V::Str(&text))]);
        }
        Ok(())
    }

    /// `load-file`: the last value, without `ns`.
    pub(crate) fn reply_value_no_ns(&self, interp: &mut Interp, reply: &Responder, value: &Value) -> Result<(), RjError> {
        let o = &self.opts;
        if o.is_plain() || o.keys.is_some() {
            let text = print_text(interp, None, o, value)?;
            reply.send(&[("value", V::Str(&text))]);
            return Ok(());
        }
        let t = print_text(interp, self.fnv.as_ref(), o, value)?;
        let (text, truncated) = apply_quota(t, o.quota)?;
        if truncated {
            reply.send(&[("value", V::Str(&text)), (TRUNCATED_KEYS, V::Strs(&["value"])), ("status", V::Strs(&[TRUNCATED]))]);
        } else {
            reply.send(&[("value", V::Str(&text))]);
        }
        Ok(())
    }

    /// Prints the throwable for `caught/print?`. Streamed: chunks are sent now
    /// and the result has no text. Not streamed: the text (and the truncated
    /// flag) go into the `ex` message.
    pub(crate) fn throwable(&self, interp: &mut Interp, reply: &Responder, exception: &Value) -> Throwable {
        let o = &self.opts;
        let printed = print_text(interp, self.fnv.as_ref(), o, exception).and_then(|t| apply_quota(t, o.quota));
        let Ok((text, truncated)) = printed else { return Throwable { text: None, truncated: false } };
        if o.stream {
            let size = o.buffer_size.unwrap_or(1024);
            stream_chunks(&text, size, &mut |c| {
                reply.send(&[(THROWABLE, V::Str(c))]);
            });
            if truncated {
                reply.send(&[("status", V::Strs(&[TRUNCATED]))]);
            }
            Throwable { text: None, truncated: false }
        } else {
            Throwable { text: Some(text), truncated }
        }
    }
}

pub(crate) struct Throwable {
    pub text: Option<String>,
    pub truncated: bool,
}

pub(crate) const THROWABLE_KEY: &str = THROWABLE;
pub(crate) const TRUNCATED_KEYS_KEY: &str = TRUNCATED_KEYS;
pub(crate) const TRUNCATED_STATUS: &str = TRUNCATED;
