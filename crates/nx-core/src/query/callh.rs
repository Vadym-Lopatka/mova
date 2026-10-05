//! callHierarchy prepare / incoming / outgoing (feature/call_hierarchy.clj).
use super::hover::uri_filename;
use super::rename::internal_error;
use super::text::Doc;
use super::*;
use crate::cst::{Cst, Kind, NodeId};
use std::collections::HashMap;

/// Source text of a file: stored text, else disk / jar entry.
pub fn file_text(q: &Q, f: FileId) -> Option<String> {
    if let Some(t) = &q.entry(f).text() {
        return Some(t.to_string());
    }
    uri_text(q.uri(f))
}

/// Text of a disk file or jar entry uri.
pub fn uri_text(uri: &str) -> Option<String> {
    let dec = |s: &str| {
        let b = s.as_bytes();
        let mut o = Vec::with_capacity(b.len());
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'%' {
                if let Some(v) = s.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    o.push(v);
                    i += 3;
                    continue;
                }
            }
            o.push(b[i]);
            i += 1;
        }
        String::from_utf8_lossy(&o).into_owned()
    };
    let (jar, entry) = if let Some(r) = uri.strip_prefix("jar:file://") {
        let (j, e) = r.split_once("!/")?;
        (dec(j), dec(e))
    } else if let Some(r) = uri.strip_prefix("zipfile://") {
        let (j, e) = r.split_once("::")?;
        (dec(j), dec(e))
    } else {
        return std::fs::read_to_string(crate::engine::scan::uri_to_path(uri)?).ok();
    };
    let mut j = crate::io::jar::Jar::open(&jar).ok()?;
    let mut buf = Vec::new();
    j.read(&entry, &mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// Clojure `(name sym)` of a symbol written `ns/name`.
pub fn simple_name(s: &str) -> &str {
    match s.find('/') {
        Some(i) if s != "/" => &s[i + 1..],
        _ => s,
    }
}

fn is_by(d: &VarDef, names: &[&str]) -> bool {
    names.contains(&d.defined_by.1.as_str()) || names.contains(&d.defined_by_lint_as.1.as_str())
}

impl<'a> Q<'a> {
    /// `element->symbol-kind` as the LSP SymbolKind number.
    pub fn symbol_kind(&self, e: El) -> u8 {
        match e.b {
            B::NsUsage | B::NsDef => 3,
            B::VarDef => {
                let d = &self.fa(e.f).var_definitions[e.i as usize];
                if d.has_fixed || d.varargs_min != NO_ARITY || d.macro_ {
                    12
                } else if is_by(d, &["defprotocol", "definterface", "defmulti"]) {
                    11
                } else if is_by(d, &["defrecord", "deftype"]) {
                    5
                } else {
                    13
                }
            }
            B::VarUsage => {
                if self.fa(e.f).var_usages[e.i as usize].defmethod {
                    12
                } else {
                    13
                }
            }
            B::KwDef => 12,
            B::KwUsage => 8,
            _ => 21,
        }
    }

    /// `element-by-uri->call-hierarchy-item`; Err = the JVM would throw (element without `:name`).
    fn ch_item(&self, uri: &str, parent: El, usage: El) -> Result<String, ()> {
        let name = self.name(parent);
        if name.is_none() {
            return Err(());
        }
        let fa = self.fa(parent.f);
        let mut label = simple_name(name.as_str()).to_string();
        let mut deprecated = false;
        let mut ns = SymId::NONE;
        match parent.b {
            B::VarDef => {
                let d = &fa.var_definitions[parent.i as usize];
                if d.has_arglists {
                    label.push(' ');
                    let v: Vec<&str> = (0..d.arglists.1).map(|k| fa.strs[(d.arglists.0 + k) as usize].as_str()).collect();
                    label.push_str(&v.join(" "));
                }
                deprecated = !d.deprecated.is_none();
                ns = d.ns;
            }
            B::KwDef | B::KwUsage => ns = fa.keywords[parent.i as usize].ns,
            _ => {}
        }
        let detail = if ns.is_none() { uri_filename(self.uri(parent.f)) } else { ns.as_str().to_string() };
        Ok(format!(
            "{{\"name\":{},\"kind\":{},\"tags\":{},\"detail\":{},\"uri\":{},\"range\":{},\"selectionRange\":{}}}",
            json_str(&label),
            self.symbol_kind(parent),
            if deprecated { "[1]" } else { "[]" },
            json_str(&detail),
            json_str(uri),
            range_json(self.name_pos(parent)),
            range_json(self.name_pos(usage))
        ))
    }

    /// `parent-var-def`: the var definition (or defmethod usage) of the top-level form around (row, col).
    fn parent_var_def(&self, doc: &Doc, f: FileId, row: u32, col: u32) -> Option<El> {
        let n = doc.find_at(row, col)?;
        let mut top = n;
        loop {
            let p = doc.parent(top)?;
            if doc.cst.kind(p) == Kind::Root {
                break;
            }
            top = p;
        }
        if !Cst::is_container(doc.cst.kind(top)) {
            return None;
        }
        let kids: Vec<NodeId> = doc.cst.sig_children(top).collect();
        let n1 = *kids.get(1)?;
        let name: Option<NodeId> = match doc.cst.kind(n1) {
            Kind::Map => kids.get(2).copied(),
            Kind::Meta => {
                let mk: Vec<NodeId> = doc.cst.sig_children(n1).collect();
                if mk.first().is_some_and(|m| doc.cst.kind(*m) == Kind::Map) {
                    mk.last().copied()
                } else {
                    mk.get(1).copied()
                }
            }
            _ => Some(n1),
        };
        let p = doc.cst.pos(name?);
        let e = self.first_under_cursor(self.uri(f), p.row, p.col)?;
        match e.b {
            B::VarDef => Some(e),
            B::VarUsage if self.fa(e.f).var_usages[e.i as usize].defmethod => Some(e),
            _ => None,
        }
    }
}

pub fn prepare(q: &Q, at: At) -> String {
    let Some(e) = q.first_under_cursor(at.uri, at.row(), at.col()) else { return internal_error() };
    match q.ch_item(at.uri, e, e) {
        Ok(i) => format!("[{i}]"),
        Err(()) => internal_error(),
    }
}

pub fn incoming(q: &Q, at: At) -> String {
    let refs = q.references_from_cursor(at, false, false).unwrap_or_default();
    let mut docs: HashMap<FileId, Option<Doc>> = HashMap::new();
    let mut out: Vec<String> = Vec::new();
    for r in refs {
        let doc = docs.entry(r.f).or_insert_with(|| file_text(q, r.f).map(|t| Doc::new(&t)));
        let Some(doc) = doc.as_ref() else { continue };
        let p = q.name_pos(r);
        let Some(parent) = q.parent_var_def(doc, r.f, p.row, p.col) else { continue };
        match q.ch_item(q.uri(r.f), parent, r) {
            Ok(i) => out.push(format!("{{\"from\":{i},\"fromRanges\":[]}}")),
            Err(()) => return internal_error(),
        }
    }
    format!("[{}]", out.join(","))
}

pub fn outgoing(q: &Q, at: At) -> String {
    let Some(f) = q.s.id(at.uri) else { return "null".into() };
    let Some(text) = file_text(q, f) else { return "null".into() };
    let doc = Doc::new(&text);
    let Some(parent) = q.parent_var_def(&doc, f, at.row(), at.col()) else { return "null".into() };
    let ps = q.name_pos(parent);
    let end = q.form_pos(parent);
    let (er, ec) = (end.end_row, end.end_col);
    let mut out: Vec<String> = Vec::new();
    for (i, u) in q.fa(f).var_usages.iter().enumerate() {
        let n = u.name_pos;
        if n.row == 0 || n.col == 0 || n.end_row == 0 || n.end_col == 0 {
            continue;
        }
        let starts = ps.row < n.row || (ps.row == n.row && ps.col <= n.col);
        let ends = n.row < er || (n.row == er && n.col <= ec);
        if !(starts && ends) {
            continue;
        }
        let ue = El { f, b: B::VarUsage, i: i as u32 };
        let Some(def) = q.find_definition(ue) else { continue };
        match q.ch_item(q.uri(def.f), def, ue) {
            Ok(it) => out.push(format!("{{\"to\":{it},\"fromRanges\":[]}}")),
            Err(()) => return internal_error(),
        }
    }
    format!("[{}]", out.join(","))
}
