//! `:doc` / `:arglists` / `:added` for `clojure.core` vars, from a static
//! table (`core/core-docs.dat`, generated from real Clojure 1.13 by
//! `tools/gen-core-docs.sh`).
//!
//! Memory: the table is part of the binary's read-only data. Nothing is
//! parsed or allocated at boot; `(meta #'map)` looks the name up (a linear
//! scan of the table, no index) and builds the merged map on demand. The
//! result is not cached, so an idle process holds nothing.

use crate::env::VarCell;
use crate::keyword::Keyword;
use crate::value::{PMap, Str, Symbol, Value};

static DOCS: &str = include_str!("../core/core-docs.dat");

/// `(added, arglists-source, doc)` for a `clojure.core` var name.
pub fn lookup(name: &str) -> Option<(&'static str, &'static str, &'static str)> {
    let key_end = |rec: &str| rec.find('\u{1f}').unwrap_or(rec.len());
    for rec in DOCS.split('\u{1e}') {
        let k = &rec[..key_end(rec)];
        if k == name {
            let mut it = rec[k.len()..].split('\u{1f}').skip(1);
            let added = it.next().unwrap_or("");
            let arglists = it.next().unwrap_or("");
            let doc = it.next().unwrap_or("");
            return Some((added, arglists, doc));
        }
    }
    None
}

/// Names of `clojure.core` vars the docs table gives an `:arglists`, indexed
/// on first use (nREPL `completions` types a candidate `function` by it).
/// The index is a few KB and is built once, only if a client asks.
pub fn has_arglists(name: &str) -> bool {
    use std::collections::HashSet;
    use std::sync::OnceLock;
    static SET: OnceLock<HashSet<&'static str>> = OnceLock::new();
    SET.get_or_init(|| {
        let mut set = HashSet::new();
        for rec in DOCS.split('\u{1e}') {
            let mut it = rec.split('\u{1f}');
            if let (Some(k), Some(_added), Some(arglists)) = (it.next(), it.next(), it.next()) {
                if !arglists.is_empty() {
                    set.insert(k);
                }
            }
        }
        set
    })
    .contains(name)
}

fn kw(s: &str) -> Value {
    Value::Keyword(Keyword::from(s))
}

/// `meta` for a var cell: its own metadata plus, for a `clojure.core` var,
/// the real `:doc`, `:arglists`, `:added` (and `:name`, `:ns` when absent).
pub fn with_docs(cell: &VarCell, meta: Value) -> Value {
    let ns_ok = match &cell.name.ns {
        None => true,
        Some(ns) => ns.as_ref() == crate::ns::CORE_NS,
    };
    if !ns_ok {
        return meta;
    }
    let name: &str = cell.name.name.as_ref();
    let Some((added, arglists, doc)) = lookup(name) else {
        return meta;
    };
    let mut m = match &meta {
        Value::Map(m) => m.clone(),
        _ => PMap::new(),
    };
    if !doc.is_empty() {
        m.insert(kw("doc"), Value::Str(Str::from(doc)));
    }
    if !added.is_empty() {
        m.insert(kw("added"), Value::Str(Str::from(added)));
    }
    if !arglists.is_empty() {
        if let Ok(Some(form)) = crate::reader::Reader::new(arglists).next_form() {
            m.insert(kw("arglists"), crate::reader::form_to_value(&form));
        }
    }
    if m.get(&kw("name")).is_none() {
        m.insert(kw("name"), Value::Sym(Symbol::simple(name)));
    }
    if m.get(&kw("ns")).is_none() {
        m.insert(kw("ns"), crate::ns::ns_value(&Str::from(crate::ns::CORE_NS)));
    }
    Value::Map(m)
}
