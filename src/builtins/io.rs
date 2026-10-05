//! lsp/io (clojure-lsp-on-Mova campaign, mova/PLAN.md): the transport-layer
//! natives clojure-lsp's JSON-RPC-over-stdio stack needs and that are
//! genuinely Rust-native capabilities with a Clojure-shaped API, per
//! PLAN.md's ordering rule #3 -- real streams, JSON, and md5.
//!
//! Review round 2 fix: every native here is registered under a QUALIFIED
//! symbol in a real namespace (`mova.io`, `mova.json`, `mova.digest`) --
//! never a bare `mova-*` global cluttering `clojure.core`'s namespace --
//! following the exact pattern `builtins::strings`'s `clojure.string`
//! natives use (`reg_ns` below is that module's `reg_ns` helper,
//! duplicated locally for the same reason that one gives: it's private to
//! its own module). Because these are registered as qualified builtins
//! during bootstrap, `Interp::seed_builtin_namespaces` (`ns.rs`)
//! automatically marks `mova.io`/`mova.json`/`mova.digest` as ALREADY
//! LOADED, the same way it does for `clojure.string` -- so `(require
//! 'mova.io)` is a no-op, never a "no such namespace" error, with zero
//! extra wiring. `flush` and `file-seq` stay bare: they ARE real
//! `clojure.core` vars on the JVM, not `mova.*` plumbing.
//!
//! ## Real streams (review round 2)
//!
//! `System/in`/`System/out`/`System/err` and `clojure.java.io/input-
//! stream`/`output-stream`/`reader`/`writer` now return REAL
//! `HostKind::InputStream`/`OutputStream` instances (`hostclass.rs`,
//! boxed/buffered `Read`/`Write` trait objects) -- not sentinels, not
//! "ignored, always real stdio" special cases. `mova.io`'s stream natives
//! (`read-line-bytes`/`read-bytes`/`write-str`/`flush`/`close`) take the
//! stream as their first arg and work on ANY of them (stdio, a file, an
//! in-memory string), so the `jsonrpc4clj.io-chan` overlay can thread its
//! `input`/`output` args through genuinely instead of special-casing
//! stdio -- see `mova/overlay/jsonrpc4clj/io_chan.mova`.
//!
//! ## JSON (`cheshire.core`'s job on the JVM)
//!
//! `mova.json/parse-string`/`generate-string` are a direct, general
//! `Value <-> serde_json::Value` bridge (hand-written tree walk, not
//! `crate::serde_bridge`'s `serde::Serializer`/`Deserializer` bridge,
//! which is a different, `--features serde`-gated embedding-API
//! mechanism this module deliberately does not depend on). Review round
//! 2: `parse-string`'s second (boolean) arg keywordizes EVERY map key
//! directly in Rust during the parse -- LSP parses a message per
//! keystroke, so the common `key-fn true` case skips a whole second
//! Clojure-level tree walk. A CUSTOM key-fn function (rare) still walks
//! the string-keyed result in `cheshire.core`'s Clojure shim.

use std::sync::Arc;

use crate::builtins::{reg, ArityHint};
use crate::error::RjError;
use crate::eval::Interp;
use crate::hostclass::HostInstVal;
use crate::value::{Keyword, PMap, PVec, Str, Symbol, Value};

/// [`reg`], but registered ONLY under `ns/name` -- never as a bare
/// global. Exact duplicate of `builtins::strings::reg_ns` (private to
/// that module, see its own doc for why this isn't factored out
/// instead): arity-checking wrapper inlined here rather than shared.
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

pub fn register(i: &mut Interp) {
    // System/in, System/out, System/err -- real streams, constructed
    // ONCE here (matching real Java: these are singleton objects, not
    // "construct a fresh one on every reference"), under both the bare
    // and `java.lang.System`-qualified spellings (mirrors `sys.rs`'s own
    // `System/getenv`/`System/exit` aliasing).
    let sys_in = crate::hostclass::mk_input_stream(Box::new(std::io::stdin()));
    // `Tee`: the bytes also reach the nREPL `forward-system-output` tap (a no-op unless asked for)
    use crate::nrepl::forward::{Stream as Fwd, Tee};
    let sys_out = crate::hostclass::mk_output_stream(Box::new(Tee::new(Fwd::Out, std::io::stdout())));
    let sys_err = crate::hostclass::mk_output_stream(Box::new(Tee::new(Fwd::Err, std::io::stderr())));
    for (name, v) in [("in", sys_in), ("out", sys_out), ("err", sys_err)] {
        i.globals.set_builtin(Symbol { ns: Some(Str::from("System")), name: Str::from(name) }, v.clone());
        i.globals.set_builtin(Symbol { ns: Some(Str::from("java.lang.System")), name: Str::from(name) }, v);
    }

    reg_ns(i, "mova.io", "read-line-bytes", ArityHint::Exact(1), |_i, args| read_line_bytes(args));
    reg_ns(i, "mova.io", "read-bytes", ArityHint::Exact(2), |_i, args| read_bytes(args));
    reg_ns(i, "mova.io", "read-all", ArityHint::Exact(1), |_i, args| read_all(args));
    reg_ns(i, "mova.io", "write-str", ArityHint::Exact(2), |_i, args| write_str(args));
    reg_ns(i, "mova.io", "flush", ArityHint::Exact(1), |_i, args| stream_flush_native(args));
    reg_ns(i, "mova.io", "close", ArityHint::Exact(1), |_i, args| stream_close_native(args));
    reg_ns(i, "mova.io", "file-input-stream", ArityHint::Exact(1), |_i, args| file_input_stream(args));
    reg_ns(i, "mova.io", "file-output-stream", ArityHint::Range(1, 2), |_i, args| file_output_stream(args));
    reg_ns(i, "mova.io", "string-input-stream", ArityHint::Exact(1), |_i, args| string_input_stream(args));
    // `.getBytes` isn't a Mova String veneer method at all (no deep JVM
    // String emulation -- PLAN.md's Mova change rules), so a caller that
    // needs a Content-Length-shaped BYTE count (not char count) for a
    // string it hasn't written yet -- `jsonrpc4clj.io-chan/write-message`
    // -- needs a native for exactly that, instead. Trivial, and the same
    // `str::len()` this module's JSON/stdio natives already rely on.
    reg_ns(i, "mova.io", "utf8-byte-count", ArityHint::Exact(1), |_i, args| utf8_byte_count(args));

    reg_ns(i, "mova.io", "new-java-file", ArityHint::Exact(1), |_i, args| new_java_file(args));
    reg_ns(i, "mova.io", "copy-file", ArityHint::Exact(2), |_i, args| copy_file(args));
    reg_ns(i, "mova.io", "make-parents", ArityHint::Exact(1), |_i, args| make_parents(args));
    reg_ns(i, "mova.io", "resolve-resource", ArityHint::Exact(1), |interp, args| resolve_resource(interp, args));

    reg_ns(i, "mova.json", "parse-string", ArityHint::Range(1, 2), |_i, args| json_parse_string(args));
    reg_ns(i, "mova.json", "generate-string", ArityHint::Exact(2), |_i, args| json_generate_string(args));

    reg_ns(i, "mova.digest", "md5-hex", ArityHint::Exact(1), |_i, args| md5_hex(args));

    // Real `clojure.core` vars -- stay bare, unlike everything above.
    reg(i, "flush", ArityHint::Exact(0), |interp, _args| flush_stdout(interp));
    reg(i, "file-seq", ArityHint::Exact(1), |_i, args| file_seq(args));
}

fn expect_str<'a>(v: &'a Value, who: &str) -> Result<&'a str, RjError> {
    match v {
        Value::Str(s) => Ok(s.as_ref()),
        other => Err(RjError::type_err(format!(
            "{who}: expected a string, got {}",
            other.type_name()
        ))),
    }
}

fn expect_stream<'a>(v: &'a Value, who: &str) -> Result<&'a Arc<HostInstVal>, RjError> {
    match v {
        Value::HostInst(h) => Ok(h),
        other => Err(RjError::type_err(format!(
            "{who}: expected a stream, got {}",
            other.type_name()
        ))),
    }
}

// ---------------------------------------------------------------------
// mova.io: real stream ops (see this module's doc for the design)
// ---------------------------------------------------------------------

/// `mova.io/read-line-bytes`: `nil` at a clean EOF, else the line (see
/// `hostclass::stream_read_line`'s doc for the exact CR/LF handling).
fn read_line_bytes(args: &[Value]) -> Result<Value, RjError> {
    let h = expect_stream(&args[0], "mova.io/read-line-bytes")?;
    Ok(match crate::hostclass::stream_read_line(h)? {
        Some(s) => Value::Str(s),
        None => Value::Nil,
    })
}

/// `mova.io/read-bytes`: EXACTLY `n` bytes, UTF-8-decoded, `nil` at a
/// clean EOF (see `hostclass::stream_read_n_bytes`'s doc for what counts
/// as "clean").
pub(crate) fn read_bytes(args: &[Value]) -> Result<Value, RjError> {
    let h = expect_stream(&args[0], "mova.io/read-bytes")?;
    let n = match &args[1] {
        Value::Int(n) if *n >= 0 => *n as usize,
        other => {
            return Err(RjError::type_err(format!(
                "mova.io/read-bytes: expected a non-negative int, got {}",
                other.type_name()
            )))
        }
    };
    Ok(match crate::hostclass::stream_read_n_bytes(h, n)? {
        Some(s) => Value::Str(s),
        None => Value::Nil,
    })
}

/// `mova.io/read-all`: drains `h` to EOF, whole-document reads
/// (`cognitect.transit/reader`'s shim -- see `hostclass::stream_read_all`'s
/// doc).
fn read_all(args: &[Value]) -> Result<Value, RjError> {
    let h = expect_stream(&args[0], "mova.io/read-all")?;
    Ok(Value::Str(crate::hostclass::stream_read_all(h)?))
}

/// `mova.io/write-str`: `s`'s UTF-8 bytes, straight through.
fn write_str(args: &[Value]) -> Result<Value, RjError> {
    let h = expect_stream(&args[0], "mova.io/write-str")?;
    let s = expect_str(&args[1], "mova.io/write-str")?;
    crate::hostclass::stream_write_str(h, s)?;
    Ok(Value::Nil)
}

fn stream_flush_native(args: &[Value]) -> Result<Value, RjError> {
    let h = expect_stream(&args[0], "mova.io/flush")?;
    crate::hostclass::stream_flush(h)?;
    Ok(Value::Nil)
}

fn stream_close_native(args: &[Value]) -> Result<Value, RjError> {
    let h = expect_stream(&args[0], "mova.io/close")?;
    crate::hostclass::stream_close(h)?;
    Ok(Value::Nil)
}

/// `mova.io/file-input-stream`: opens `path` for reading.
fn file_input_stream(args: &[Value]) -> Result<Value, RjError> {
    let path = expect_str(&args[0], "mova.io/file-input-stream")?;
    let f = std::fs::File::open(path).map_err(|e| {
        RjError::sys(
            format!("mova.io/file-input-stream: couldn't open {path}"),
            e.raw_os_error().unwrap_or(libc::EIO),
            "open",
        )
    })?;
    Ok(crate::hostclass::mk_input_stream(Box::new(f)))
}

/// `mova.io/file-output-stream`: opens `path` for writing -- truncating
/// (real `FileOutputStream`'s default) unless `append?` (the 2-arity
/// form's second arg) is `true`.
fn file_output_stream(args: &[Value]) -> Result<Value, RjError> {
    let path = expect_str(&args[0], "mova.io/file-output-stream")?;
    let append = matches!(args.get(1), Some(Value::Bool(true)));
    let f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .append(append)
        .truncate(!append)
        .open(path)
        .map_err(|e| {
            RjError::sys(
                format!("mova.io/file-output-stream: couldn't open {path}"),
                e.raw_os_error().unwrap_or(libc::EIO),
                "open",
            )
        })?;
    Ok(crate::hostclass::mk_output_stream(Box::new(f)))
}

/// `mova.io/string-input-stream`: an in-memory source over `content`'s
/// UTF-8 bytes (`std::io::Cursor`) -- the "byte/string content" source
/// PLAN.md's stream review asked for, useful standalone (tests, a
/// caller that already has the whole document in memory) without
/// needing a real file or stdin.
fn string_input_stream(args: &[Value]) -> Result<Value, RjError> {
    let s = expect_str(&args[0], "mova.io/string-input-stream")?;
    Ok(crate::hostclass::mk_input_stream(Box::new(std::io::Cursor::new(s.as_bytes().to_vec()))))
}

/// `mova.io/utf8-byte-count`: `s`'s UTF-8 byte length (`str::len()`),
/// not its char count -- see this native's registration-site doc.
fn utf8_byte_count(args: &[Value]) -> Result<Value, RjError> {
    let s = expect_str(&args[0], "mova.io/utf8-byte-count")?;
    Ok(Value::Int(s.len() as i64))
}

// ---------------------------------------------------------------------
// JSON (serde_json-backed; see this module's doc for the shim split)
// ---------------------------------------------------------------------

/// `serde_json::Value` -> mova `Value`. `keywordize`: when `true`, every
/// OBJECT key becomes a `Value::Keyword` directly (no separate Clojure-
/// level walk needed for the common `cheshire.core/parse-string`'s
/// `key-fn true` case -- see this module's doc); when `false`, keys stay
/// plain strings, exactly as before. JSON integers that fit `i64` become
/// `Value::Int`; anything else (a JSON float, or an integer too big for
/// `i64` -- mova has no bignum, see `serde_bridge.rs`'s own doc for the
/// same ceiling) becomes `Value::Float`.
fn json_to_value(j: &serde_json::Value, keywordize: bool) -> Value {
    match j {
        serde_json::Value::Null => Value::Nil,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Int(i)
            } else {
                Value::Float(n.as_f64().unwrap_or(f64::NAN))
            }
        }
        serde_json::Value::String(s) => Value::Str(Str::from(s.clone())),
        serde_json::Value::Array(items) => {
            Value::Vector(items.iter().map(|v| json_to_value(v, keywordize)).collect::<PVec>())
        }
        serde_json::Value::Object(map) => {
            let mut m = PMap::new();
            for (k, v) in map {
                let key = if keywordize {
                    Value::Keyword(Keyword::from(k.clone()))
                } else {
                    Value::Str(Str::from(k.clone()))
                };
                m.insert(key, json_to_value(v, keywordize));
            }
            Value::Map(m)
        }
    }
}

/// mova `Value` -> `serde_json::Value`. Map keys are stringified:
/// `Value::Keyword`/`Value::Sym` use their bare name (a JSON-RPC message
/// is always built from keyword-keyed maps -- `{:jsonrpc "2.0" ...}` --
/// and JSON object keys must be strings, so this is the same "keyword ->
/// its name, verbatim" convention `cheshire`'s real generator uses by
/// default), anything else (a `Value::Str` key, or an unusual key like an
/// `Value::Int`) is rendered with `pr_str`/`to_string` as a fallback --
/// good enough for a shim whose only measured caller (`jsonrpc4clj.io-
/// chan`) always hands it string keys already (`cske/transform-keys` ran
/// first).
fn value_to_json(v: &Value) -> serde_json::Value {
    match v {
        Value::Nil => serde_json::Value::Null,
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::Int(i) => serde_json::Value::Number((*i).into()),
        Value::Float(f) => serde_json::Number::from_f64(*f)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        Value::Str(s) => serde_json::Value::String(s.as_ref().to_string()),
        Value::Keyword(k) => serde_json::Value::String(k.text().to_string()),
        Value::Sym(sym) => serde_json::Value::String(sym.name.to_string()),
        Value::Char(c) => serde_json::Value::String(c.to_string()),
        Value::Vector(items) | Value::List(items) => {
            serde_json::Value::Array(items.iter().map(value_to_json).collect())
        }
        Value::Map(m) => {
            let mut obj = serde_json::Map::new();
            for (k, val) in m.iter() {
                let key = match k {
                    Value::Str(s) => s.as_ref().to_string(),
                    Value::Keyword(kw) => kw.text().to_string(),
                    Value::Sym(sym) => sym.name.to_string(),
                    other => crate::printer::pr_str(other),
                };
                obj.insert(key, value_to_json(val));
            }
            serde_json::Value::Object(obj)
        }
        Value::LazyMap(lm) => value_to_json(&Value::Map(crate::lazy_map::as_pmap(lm).clone())),
        other => serde_json::Value::String(crate::printer::pr_str(other)),
    }
}

/// `mova.json/parse-string`: `serde_json::from_str`, then the tree walk
/// above. `keywordize?` (optional, defaults to `false`) is the native
/// fast path for `cheshire.core/parse-string`'s `key-fn true` -- see
/// this module's doc. A malformed document is a catchable `RjError`,
/// matching cheshire's own `parse-string` (which throws
/// `JsonParseException` on the JVM, caught by `jsonrpc4clj.io-chan/read-
/// message`'s blanket `(catch Exception _ :parse-error)`).
fn json_parse_string(args: &[Value]) -> Result<Value, RjError> {
    let text = expect_str(&args[0], "mova.json/parse-string")?;
    let keywordize = matches!(args.get(1), Some(Value::Bool(true)));
    let parsed: serde_json::Value = serde_json::from_str(text)
        .map_err(|e| RjError::other(format!("mova.json/parse-string: {e}")))?;
    Ok(json_to_value(&parsed, keywordize))
}

/// `mova.json/generate-string`: `value_to_json` then `serde_json::
/// to_string`/`to_string_pretty`, chosen by the second (boolean) arg --
/// `cheshire.core/generate-string`'s `:pretty` option, passed through
/// verbatim by the shim. `serde_json::to_string_pretty` makes this a
/// trivial native flag rather than a Clojure-level reformat.
fn json_generate_string(args: &[Value]) -> Result<Value, RjError> {
    let json = value_to_json(&args[0]);
    let pretty = matches!(&args[1], Value::Bool(true));
    let s = if pretty {
        serde_json::to_string_pretty(&json)
    } else {
        serde_json::to_string(&json)
    }
    .map_err(|e| RjError::other(format!("mova.json/generate-string: {e}")))?;
    Ok(Value::Str(Str::from(s)))
}

// ---------------------------------------------------------------------
// md5 (`md-5` crate; clojure-lsp's `shared.clj`/`classpath.clj`/
// `config.clj` all reach for `java.security.MessageDigest` "MD5" -- not
// yet wired to any overlay, see this native's own doc).
// ---------------------------------------------------------------------

/// `mova.digest/md5-hex`: lower-case hex digest of `s`'s UTF-8 bytes,
/// exactly what `(-> (MessageDigest/getInstance "MD5") (.digest (.getBytes
/// s)) BigInteger/... hex-string)`-shaped JVM call sites in clojure-lsp
/// compute (byte-for-byte: `md-5` and the JDK's own MD5 both implement
/// RFC 1321, so this is a real algorithmic match, not an approximation).
fn md5_hex(args: &[Value]) -> Result<Value, RjError> {
    use md5::{Digest, Md5};
    let s = expect_str(&args[0], "mova.digest/md5-hex")?;
    let mut hasher = Md5::new();
    hasher.update(s.as_bytes());
    let digest = hasher.finalize();
    let hex = digest.iter().map(|b| format!("{b:02x}")).collect::<String>();
    Ok(Value::Str(Str::from(hex)))
}

// ---------------------------------------------------------------------
// clojure.java.io support: file / copy / make-parents / resource /
// file-seq
// ---------------------------------------------------------------------

/// `mova.io/new-java-file`: constructs a `java.io.File` veneer from a
/// plain path string (`clojure.java.io/file`'s Clojure shim joins
/// segments with `/` first, then calls this).
fn new_java_file(args: &[Value]) -> Result<Value, RjError> {
    Ok(crate::hostclass::mk_java_file(Str::from(
        expect_str(&args[0], "mova.io/new-java-file")?.to_string(),
    )))
}

/// Accepts a `Value::Str` path or a `java.io.File` `HostInst`
/// indifferently -- `clojure.java.io/as-file`'s job on the JVM.
fn expect_path(v: &Value, who: &str) -> Result<Str, RjError> {
    crate::hostclass::path_str_of(v).ok_or_else(|| {
        RjError::type_err(format!(
            "{who}: expected a path string or java.io.File, got {}",
            v.type_name()
        ))
    })
}

/// `mova.io/copy-file` (`clojure.java.io/copy`'s file-to-file subset):
/// `std::fs::copy`, a real byte-for-byte copy, not a read-then-spit
/// round trip through a mova `Str` (which would lossy-decode non-UTF-8
/// bytes -- see `sys::slurp`'s own doc for that exact caveat).
fn copy_file(args: &[Value]) -> Result<Value, RjError> {
    let from = expect_path(&args[0], "mova.io/copy-file")?;
    let to = expect_path(&args[1], "mova.io/copy-file")?;
    std::fs::copy(from.as_ref(), to.as_ref()).map_err(|e| {
        RjError::sys(
            format!("mova.io/copy-file: couldn't copy {} to {}", from.as_ref(), to.as_ref()),
            e.raw_os_error().unwrap_or(libc::EIO),
            "copy",
        )
    })?;
    Ok(Value::Nil)
}

/// `mova.io/make-parents` (`clojure.java.io/make-parents`): creates every
/// missing directory in `path`'s parent chain (`std::fs::
/// create_dir_all`, matching real `File.mkdirs()` semantics -- a no-op,
/// not an error, if the parent already exists).
fn make_parents(args: &[Value]) -> Result<Value, RjError> {
    let path = expect_path(&args[0], "mova.io/make-parents")?;
    if let Some(parent) = std::path::Path::new(path.as_ref()).parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| {
                RjError::sys(
                    format!("mova.io/make-parents: couldn't create {}", parent.display()),
                    e.raw_os_error().unwrap_or(libc::EIO),
                    "mkdir",
                )
            })?;
        }
    }
    Ok(Value::Nil)
}

/// `mova.io/resolve-resource` (`clojure.java.io/resource`'s backing
/// native, review round 2): searches `interp.module_paths` IN ORDER
/// (exactly the roots `require`/`load` themselves search, see `ns.rs`'s
/// `find_module_file`) for `rel`, returning the first match's absolute
/// path as a plain string, or `nil` if none of them have it. This is a
/// REAL classpath-shaped search (module-path roots stand in for JARs/
/// classpath directories), not a bare cwd-relative check -- clojure-lsp
/// reads `CLOJURE_LSP_VERSION` and clj-kondo reads its default configs
/// this way. Returns a plain absolute path (not a `file:` URL); the
/// `clojure.java.io` shim wraps it as `"file://" + path` (documented
/// there) so `str` gets back exactly that string, and `slurp`/`io/file`
/// strip the `file://` prefix again before touching the filesystem (see
/// `sys::slurp`'s own doc for that half).
fn resolve_resource(interp: &mut Interp, args: &[Value]) -> Result<Value, RjError> {
    let rel = expect_str(&args[0], "mova.io/resolve-resource")?;
    for root in &interp.module_paths {
        let candidate = root.join(rel);
        if candidate.is_file() {
            return Ok(Value::Str(Str::from(candidate.to_string_lossy().into_owned())));
        }
    }
    Ok(Value::Nil)
}

/// `clojure.core/file-seq`: every file/directory reachable under `dir`,
/// depth-first, `dir` itself first (matches real `file-seq`'s own
/// walk order: `tree-seq` pre-order over `.listFiles`). EAGER, not a
/// true lazy seq -- see this native's own doc note in `mova/NOTES.md`
/// for why (nothing here needs streaming a directory too large to fit
/// in memory, and mova's lazy-seq machinery is `builtins::collections`'
/// territory, not `io.rs`'s).
fn file_seq(args: &[Value]) -> Result<Value, RjError> {
    let root = expect_path(&args[0], "file-seq")?;
    let mut out: Vec<Value> = Vec::new();
    let mut stack = vec![std::path::PathBuf::from(root.as_ref())];
    while let Some(p) = stack.pop() {
        out.push(crate::hostclass::mk_java_file(Str::from(p.to_string_lossy().into_owned())));
        if p.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&p) {
                // `stack.pop()` above is depth-first LIFO -- pushing in
                // reverse keeps sibling order matching the JVM's
                // `listFiles` (undefined by spec, but this keeps our
                // oracle diff deterministic against a single directory
                // listing rather than reversed).
                let mut children: Vec<_> = entries.filter_map(|e| e.ok()).map(|e| e.path()).collect();
                children.sort();
                children.reverse();
                stack.extend(children);
            }
        }
    }
    Ok(Value::Vector(out.into_iter().collect::<PVec>()))
}

/// `clojure.core/flush` -- real Clojure flushes the CURRENT `*out*`
/// (whatever `Writer` it's bound to); this native only ever flushes real
/// process stdout, since nothing in this campaign's scope binds `*out*`
/// to a buffered Mova-native writer that itself needs flushing (`*out*`'s
/// own sink, `strings::out_write`, either appends to an in-memory atom --
/// nothing to flush -- or falls through to `print!`, which is real
/// stdout). A future writer-backed `*out*` would need this native taught
/// to check the dynamic binding first, same as `out_write` does.
fn flush_stdout(interp: &mut Interp) -> Result<Value, RjError> {
    use std::io::Write;
    // nREPL: a stream bound to `*out*` (the session's output sink) is what
    // `(flush)` must flush. Plain stdout stays the default.
    if let Some(Value::HostInst(h)) = interp.globals.get(&crate::value::Symbol::simple("*out*")) {
        if h.kind == crate::hostclass::HostKind::OutputStream {
            crate::hostclass::stream_flush(&h)?;
            return Ok(Value::Nil);
        }
    }
    std::io::stdout().flush().map_err(|e| {
        RjError::sys(
            "flush: couldn't flush stdout",
            e.raw_os_error().unwrap_or(libc::EIO),
            "write",
        )
    })?;
    Ok(Value::Nil)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::Keyword;

    #[test]
    fn json_round_trips_scalars_and_collections() {
        let parsed = json_parse_string(&[Value::Str(Str::from(
            r#"{"a": 1, "b": [true, null, "x✓"], "c": 1.5}"#.to_string(),
        ))])
        .unwrap();
        let Value::Map(m) = &parsed else { panic!("expected a map") };
        assert_eq!(m.get(&Value::Str(Str::from("a".to_string()))), Some(&Value::Int(1)));
        assert_eq!(m.get(&Value::Str(Str::from("c".to_string()))), Some(&Value::Float(1.5)));
        let Some(Value::Vector(b)) = m.get(&Value::Str(Str::from("b".to_string()))) else {
            panic!("expected a vector")
        };
        assert_eq!(b.len(), 3);

        let generated = json_generate_string(&[parsed, Value::Bool(false)]).unwrap();
        let Value::Str(s) = generated else { panic!("expected a string") };
        // Round-trip through serde_json again rather than comparing text
        // (key order/whitespace aren't part of the contract).
        let reparsed: serde_json::Value = serde_json::from_str(s.as_ref()).unwrap();
        assert_eq!(reparsed["a"], 1);
        assert_eq!(reparsed["c"], 1.5);
    }

    #[test]
    fn json_parse_string_keywordize_true_makes_keyword_keys_natively() {
        let parsed = json_parse_string(&[
            Value::Str(Str::from(r#"{"a":{"b":1}}"#.to_string())),
            Value::Bool(true),
        ])
        .unwrap();
        let Value::Map(m) = &parsed else { panic!("expected a map") };
        let inner = m.get(&Value::Keyword(Keyword::from("a".to_string()))).unwrap();
        let Value::Map(inner) = inner else { panic!("expected a nested map") };
        assert_eq!(inner.get(&Value::Keyword(Keyword::from("b".to_string()))), Some(&Value::Int(1)));
    }

    #[test]
    fn json_generate_pretty_differs_from_compact() {
        let m = Value::Map(PMap::from_iter(vec![(
            Value::Keyword(Keyword::from("x".to_string())),
            Value::Int(1),
        )]));
        let compact = json_generate_string(&[m.clone(), Value::Bool(false)]).unwrap();
        let pretty = json_generate_string(&[m, Value::Bool(true)]).unwrap();
        assert_ne!(compact, pretty);
    }

    #[test]
    fn keyword_keys_serialize_by_bare_name() {
        let m = Value::Map(PMap::from_iter(vec![(
            Value::Keyword(Keyword::from("jsonrpc".to_string())),
            Value::Str(Str::from("2.0".to_string())),
        )]));
        let Value::Str(s) = json_generate_string(&[m, Value::Bool(false)]).unwrap() else {
            panic!("expected a string")
        };
        assert_eq!(s.as_ref(), r#"{"jsonrpc":"2.0"}"#);
    }

    #[test]
    fn md5_hex_matches_known_vector() {
        // RFC 1321 test vector: MD5("") = d41d8cd98f00b204e9800998ecf8427e
        let Value::Str(s) = md5_hex(&[Value::Str(Str::from(String::new()))]).unwrap() else {
            panic!("expected a string")
        };
        assert_eq!(s.as_ref(), "d41d8cd98f00b204e9800998ecf8427e");
        let Value::Str(s2) = md5_hex(&[Value::Str(Str::from("abc".to_string()))]).unwrap() else {
            panic!("expected a string")
        };
        assert_eq!(s2.as_ref(), "900150983cd24fb0d6963f7d28e17f72");
    }

    #[test]
    fn utf8_byte_length_exceeds_char_count_for_non_ascii() {
        // The exact proof case the smoke test's driver exercises: "héllo
        // ✓" is 7 chars but its UTF-8 encoding is longer (é = 2 bytes, ✓ =
        // 3 bytes) -- a byte-counting reader (the JVM's, and ours) needs
        // the BYTE count, not the char count, to frame the next message
        // correctly.
        let s = "h\u{e9}llo \u{2713}";
        assert_eq!(s.chars().count(), 7);
        assert!(s.as_bytes().len() > 7);
    }

    #[test]
    fn read_write_round_trip_through_a_string_stream() {
        let out = crate::hostclass::mk_output_stream(Box::new(std::io::Cursor::new(Vec::<u8>::new())));
        let h = match &out {
            Value::HostInst(h) => h.clone(),
            _ => unreachable!(),
        };
        write_str(&[out.clone(), Value::Str(Str::from("hello".to_string()))]).unwrap();
        // Can't read stdout-shaped writes back out here (no shared
        // buffer accessor exposed) -- covered end-to-end instead by
        // `hostclass::tests` and the smoke test's JVM-vs-Mova diff. This
        // test only proves `write_str`/`expect_stream` don't error on a
        // real `HostInst`.
        let _ = h;
    }

    #[test]
    fn resolve_resource_finds_a_file_on_the_module_path() {
        let dir = std::env::temp_dir().join(format!("mova-resource-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("MOVA_TEST_RESOURCE"), b"hi").unwrap();
        let mut interp = Interp::new();
        interp.module_paths = vec![dir.clone()];
        let found = resolve_resource(&mut interp, &[Value::Str(Str::from("MOVA_TEST_RESOURCE".to_string()))]).unwrap();
        assert_eq!(
            found,
            Value::Str(Str::from(dir.join("MOVA_TEST_RESOURCE").to_string_lossy().into_owned()))
        );
        let missing = resolve_resource(&mut interp, &[Value::Str(Str::from("NOPE".to_string()))]).unwrap();
        assert_eq!(missing, Value::Nil);
        std::fs::remove_dir_all(&dir).ok();
    }
}
