//! lsp/io (clojure-lsp-on-Mova campaign, mova/PLAN.md): `mova.uri` --
//! percent-encode/decode primitives for clojure-lsp's file: URI handling
//! (`clojure_lsp.shared`'s `filename->uri`/`uri->filename`/`uri->path`),
//! replacing deep `java.net.URI`/`URLDecoder` emulation per PLAN.md's
//! "reuse Rust crates" rule -- hand-rolled here (RFC 3986 unreserved set,
//! byte-wise over UTF-8) since the job is two small pure functions, not
//! worth a new Cargo dependency.

use std::sync::Arc;

use crate::builtins::ArityHint;
use crate::error::RjError;
use crate::eval::Interp;
use crate::value::{Str, Symbol, Value};

/// [`crate::builtins::reg`], but registered under a qualified `ns/name`
/// symbol -- exact duplicate of `builtins::io`'s own `reg_ns` (private to
/// that module; see its doc for why this isn't factored out instead).
#[track_caller]
fn reg_ns(
    i: &mut Interp,
    ns: &'static str,
    name: &'static str,
    arity: ArityHint,
    f: impl Fn(&mut Interp, &[Value]) -> Result<Value, RjError> + Send + Sync + 'static,
) {
    let native = crate::value::NativeFn::new(name, move |interp: &mut Interp, args: &[Value]| {
        if !arity.matches(args.len()) {
            return Err(RjError::arity(format!("{ns}/{name}: wrong number of args ({})", args.len()))
                .with_stack(interp.stack_snapshot(), interp.source_id));
        }
        f(interp, args)
    });
    i.globals.set_builtin(
        Symbol { ns: Some(ns.into()), name: name.into() },
        Value::Native(Arc::new(native)),
    );
}

fn as_str<'a>(v: &'a Value, who: &str) -> Result<&'a str, RjError> {
    match v {
        Value::Str(s) => Ok(s.as_ref()),
        other => Err(RjError::other(format!(
            "{who}: expected a string, got {}",
            crate::printer::pr_str(other)
        ))),
    }
}

/// Percent-encodes `s` byte-wise: RFC 3986 unreserved (`A-Za-z0-9-_.~`)
/// plus any char in the optional `safe` string (e.g. `"/"` for a path)
/// pass through verbatim; every other byte -- including each byte of a
/// multi-byte UTF-8 sequence -- becomes `%XX` uppercase hex. Matches what
/// `java.net.URI`'s constructor does to a path component (`escape-uri`'s
/// `.toASCIIString` on the JVM side of this same call site).
fn percent_encode(args: &[Value]) -> Result<Value, RjError> {
    let s = as_str(&args[0], "mova.uri/percent-encode")?;
    let safe = if args.len() > 1 { as_str(&args[1], "mova.uri/percent-encode")? } else { "" };
    let safe_bytes = safe.as_bytes();
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') || safe_bytes.contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    Ok(Value::Str(Str::from(out)))
}

/// Reverses [`percent_encode`]: `%XX` groups become the raw byte, then
/// the whole byte sequence is decoded as UTF-8. An invalid `%XX` (bad hex,
/// or truncated at the end) passes through literally, same laxness as
/// `java.net.URLDecoder`; invalid UTF-8 after decoding falls back to the
/// original string rather than erroring.
fn percent_decode(args: &[Value]) -> Result<Value, RjError> {
    let s = as_str(&args[0], "mova.uri/percent-decode")?;
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 3 <= bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    match String::from_utf8(out) {
        Ok(decoded) => Ok(Value::Str(Str::from(decoded))),
        Err(_) => Ok(Value::Str(Str::from(s.to_string()))),
    }
}

pub fn register(i: &mut Interp) {
    reg_ns(i, "mova.uri", "percent-encode", ArityHint::Range(1, 2), |_i, args| percent_encode(args));
    reg_ns(i, "mova.uri", "percent-decode", ArityHint::Exact(1), |_i, args| percent_decode(args));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_space_and_non_ascii_leaves_slash_safe() {
        let v = percent_encode(&[Value::Str(Str::from("/a b/wörld".to_string())), Value::Str(Str::from("/".to_string()))]).unwrap();
        assert_eq!(v, Value::Str(Str::from("/a%20b/w%C3%B6rld".to_string())));
    }

    #[test]
    fn decode_reverses_encode() {
        let v = percent_decode(&[Value::Str(Str::from("/a%20b/w%C3%B6rld".to_string()))]).unwrap();
        assert_eq!(v, Value::Str(Str::from("/a b/wörld".to_string())));
    }
}
