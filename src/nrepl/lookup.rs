//! `lookup`: a port of `nrepl.util.lookup` over the interpreter's var tables,
//! and the writer that turns Mova data into bencode for `info` and for the
//! results of `complete-fn` / `lookup-fn`.
//!
//! The `info` map has the keys `nrepl.misc/safe-var-metadata` allows, with the
//! same conversions as `sanitize-meta`: `ns`, `name`, `protocol` (`""`) are
//! strings, `macro` / `special-form` are `"true"`, `arglists` is the printed
//! list and `arglists-str` always exists (`""` when there are no arglists).
//! Source paths are Mova's (`core/core.mova`), not jar URLs.

use crate::eval::Interp;
use crate::keyword::Keyword;
use crate::value::{PMap, Symbol, Value};
use mova_nrepl::bencode::{write_bytes, write_int};

/// Why a lookup failed: becomes `message` of a `lookup-error` reply.
pub(crate) type Failure = String;

fn kw(s: &str) -> Value {
    Value::Keyword(Keyword::from(s))
}

fn truthy(v: Option<&Value>) -> bool {
    matches!(v, Some(v) if !matches!(v, Value::Nil | Value::Bool(false)))
}

/// `(symbol s)`: `a/b` is namespace `a`, name `b`; `/` alone is the name `/`.
pub(crate) fn parse_symbol(s: &str) -> Symbol {
    match s.find('/') {
        Some(i) if s != "/" => Symbol { ns: Some(s[..i].into()), name: s[i + 1..].into() },
        _ => Symbol::simple(s),
    }
}

/// Bencode of `info` for `sym` seen from `ns`; `Ok(None)` for no such symbol.
pub(crate) fn lookup(interp: &mut Interp, ns: &str, sym: &str) -> Result<Option<Vec<u8>>, Failure> {
    let symbol = parse_symbol(sym);
    // special forms first; they do not need the namespace to exist
    if symbol.ns.is_none() && is_special(interp, &symbol) {
        return Ok(Some(special_info(interp, &symbol)));
    }
    let ns_val = Value::Sym(Symbol::simple(ns));
    interp.the_ns(&ns_val).map_err(|e| e.message.to_string())?;
    let cell = match &symbol.ns {
        Some(q) => {
            let Some(scope) = scope_of(interp, q, ns) else { return Ok(None) };
            interp.globals.find_any_cell(&crate::ns::var_symbol_in(&scope, &symbol.name))
        }
        None => {
            if symbol.name.is_empty() {
                return Err("Index 0 out of bounds for length 0".into());
            }
            if symbol.name.contains('.') || symbol.name.starts_with('[') {
                return Ok(None); // a class name: no var, no meta
            }
            interp.ns_resolve_in(&ns_val, &symbol).map_err(|e| e.message.to_string())?
        }
    };
    let Some(cell) = cell else { return Ok(None) };
    if matches!(cell.raw_root(), Some(Value::Class(_))) {
        return Ok(None);
    }
    let meta = crate::coredocs::with_docs(&cell, cell.var_meta());
    let map = match &meta {
        Value::Map(m) => m.clone(),
        _ => PMap::new(),
    };
    let home = cell.name.ns.as_deref().unwrap_or(crate::ns::CORE_NS).to_string();
    let mut map = map;
    // a core var that the interpreter registers natively has no source file of its own
    if cell.name.ns.is_none() && map.get(&kw("file")).is_none() {
        map.insert(kw("file"), Value::Str("core/core.mova".into()));
    }
    Ok(Some(info(&map, &cell.name.name, &home, false)))
}

/// A namespace a qualified symbol may name: an alias of `ns`, or a namespace.
fn scope_of(interp: &Interp, q: &str, ns: &str) -> Option<crate::value::Str> {
    if let Some((_, full)) = interp.ns_aliases_of(&crate::value::Str::from(ns)).into_iter().find(|(a, _)| a.as_ref() == q) {
        return Some(full);
    }
    let q = crate::value::Str::from(q);
    interp.user_visible_ns_names().into_iter().find(|n| *n == q)
}

fn is_special(interp: &mut Interp, sym: &Symbol) -> bool {
    let Some(f) = interp.lookup_global(&Symbol::simple("special-symbol?")) else { return false };
    matches!(interp.call(&f, &[Value::Sym(sym.clone())]), Ok(Value::Bool(true)))
}

fn special_info(interp: &Interp, sym: &Symbol) -> Vec<u8> {
    let mut map = PMap::new();
    if let Some(Value::Map(docs)) = interp.globals.get_exact(&Symbol::simple("special-doc-map")) {
        if let Some(Value::Map(entry)) = docs.get(&Value::Sym(sym.clone())) {
            map = entry.clone();
        }
    }
    map.insert(kw("file"), Value::Str("core/core.mova".into()));
    info(&map, &sym.name, crate::ns::CORE_NS, true)
}

/// Builds the bencode dict. `map` is the var's metadata (or the special-doc entry).
fn info(map: &PMap, name: &str, home_ns: &str, special: bool) -> Vec<u8> {
    let get = |k: &str| map.get(&kw(k));
    let string = |v: Option<&Value>| -> Option<String> {
        match v? {
            Value::Str(s) => Some(s.to_string()),
            Value::Nil => None,
            other => Some(crate::printer::pr_str(other)),
        }
    };
    let mut f: Vec<(&str, Vec<u8>)> = Vec::new();
    let put_str = |f: &mut Vec<(&str, Vec<u8>)>, k: &'static str, s: &str| {
        let mut b = Vec::new();
        write_bytes(&mut b, s.as_bytes());
        f.push((k, b));
    };
    let put_int = |f: &mut Vec<(&str, Vec<u8>)>, k: &'static str, n: i64| {
        let mut b = Vec::new();
        write_int(&mut b, n);
        f.push((k, b));
    };
    // ns: a namespace value, a symbol or a string; else the var's own
    let ns = match get("ns") {
        Some(v) => crate::ns::ns_value_name(v)
            .map(|s| s.to_string())
            .or_else(|| match v {
                Value::Sym(s) => Some(s.name.to_string()),
                Value::Str(s) => Some(s.to_string()),
                _ => None,
            })
            .unwrap_or_else(|| home_ns.to_string()),
        None => home_ns.to_string(),
    };
    put_str(&mut f, "ns", &ns);
    let nm = match get("name") {
        Some(Value::Sym(s)) => s.name.to_string(),
        Some(Value::Str(s)) => s.to_string(),
        _ => name.to_string(),
    };
    put_str(&mut f, "name", &nm);
    put_str(&mut f, "protocol", "");
    if let Some(d) = string(get("doc")) {
        put_str(&mut f, "doc", &d);
    }
    if let Some(file) = string(get("file")) {
        put_str(&mut f, "file", &file);
    }
    match get("arglists") {
        Some(v) if truthy(Some(v)) => {
            let s = crate::printer::pr_str(v);
            put_str(&mut f, "arglists", &s);
            put_str(&mut f, "arglists-str", &s);
        }
        _ => put_str(&mut f, "arglists-str", ""),
    }
    if let Some(v) = get("forms") {
        if truthy(Some(v)) {
            let mut b = Vec::new();
            write_plain(v, &mut b);
            f.push(("forms", b));
        }
    }
    if truthy(get("macro")) {
        put_str(&mut f, "macro", "true");
    }
    if special || truthy(get("special-form")) {
        put_str(&mut f, "special-form", "true");
    }
    for k in ["line", "column"] {
        if let Some(Value::Int(n)) = get(k) {
            put_int(&mut f, if k == "line" { "line" } else { "column" }, *n);
        }
    }
    for k in ["added", "deprecated", "resource"] {
        if truthy(get(k)) {
            if let Some(s) = string(get(k)) {
                put_str(&mut f, if k == "added" { "added" } else if k == "deprecated" { "deprecated" } else { "resource" }, &s);
            }
        }
    }
    f.sort_by(|a, b| a.0.cmp(b.0));
    let mut out = vec![b'd'];
    for (k, v) in f {
        write_bytes(&mut out, k.as_bytes());
        out.extend_from_slice(&v);
    }
    out.push(b'e');
    out
}

/// Bencode of already realized data: lists and vectors as lists, maps as
/// dicts (keys by name), keywords and symbols as their names, nil as `le`.
pub(crate) fn write_plain(v: &Value, out: &mut Vec<u8>) {
    match v.unmeta() {
        Value::Nil => out.extend_from_slice(b"le"),
        Value::Int(n) => write_int(out, *n),
        Value::Bool(b) => write_bytes(out, if *b { b"true" } else { b"false" }),
        Value::Str(s) => write_bytes(out, s.as_bytes()),
        Value::Keyword(k) => write_bytes(out, k.text_ref().as_bytes()),
        Value::Sym(s) => write_bytes(out, name_of(s).as_bytes()),
        Value::List(l) | Value::Vector(l) => {
            out.push(b'l');
            for x in l.iter_cloned() {
                write_plain(&x, out);
            }
            out.push(b'e');
        }
        Value::Set(s) => {
            out.push(b'l');
            for x in s.iter() {
                write_plain(x, out);
            }
            out.push(b'e');
        }
        Value::Map(m) => {
            let mut rows: Vec<(Vec<u8>, &Value)> = m.iter().map(|(k, v)| (key_bytes(k), v)).collect();
            rows.sort_by(|a, b| a.0.cmp(&b.0));
            out.push(b'd');
            for (k, v) in rows {
                write_bytes(out, &k);
                write_plain(v, out);
            }
            out.push(b'e');
        }
        other => write_bytes(out, crate::printer::pr_str(other).as_bytes()),
    }
}

fn name_of(s: &Symbol) -> String {
    match &s.ns {
        Some(ns) => format!("{ns}/{}", s.name),
        None => s.name.to_string(),
    }
}

pub(crate) fn key_bytes(k: &Value) -> Vec<u8> {
    match k {
        Value::Str(s) => s.as_bytes().to_vec(),
        Value::Keyword(k) => k.text_ref().as_bytes().to_vec(),
        Value::Sym(s) => name_of(s).into_bytes(),
        other => crate::printer::pr_str(other).into_bytes(),
    }
}
