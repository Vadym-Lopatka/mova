//! Tiny EDN value view over the nx CST reader (enough for deps.edn, bb.edn, project.clj, config.edn).
use crate::cst::{Cst, Kind, NodeId};

#[derive(Clone, Debug, PartialEq)]
pub enum Edn {
    Map(Vec<(Edn, Edn)>),
    /// Vector, list or set (order kept).
    Seq(Vec<Edn>),
    Kw(String),
    Sym(String),
    Str(String),
    Nil,
    Bool(bool),
    Other,
}

impl Edn {
    pub fn get(&self, key: &str) -> Option<&Edn> {
        match self {
            Edn::Map(kv) => kv.iter().find(|(k, _)| matches!(k, Edn::Kw(s) if s == key)).map(|(_, v)| v),
            _ => None,
        }
    }
    pub fn get_str(&self, key: &str) -> Option<&Edn> {
        match self {
            Edn::Map(kv) => kv.iter().find(|(k, _)| matches!(k, Edn::Str(s) if s == key)).map(|(_, v)| v),
            _ => None,
        }
    }
    pub fn entries(&self) -> &[(Edn, Edn)] {
        match self {
            Edn::Map(kv) => kv,
            _ => &[],
        }
    }
    pub fn items(&self) -> &[Edn] {
        match self {
            Edn::Seq(v) => v,
            _ => &[],
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Edn::Str(s) | Edn::Kw(s) | Edn::Sym(s) => Some(s),
            _ => None,
        }
    }
    /// Strings (or keyword/symbol names) of a sequence.
    pub fn strs(&self) -> Vec<String> {
        self.items().iter().filter_map(|e| e.as_str().map(String::from)).collect()
    }
}

fn conv(c: &Cst, id: NodeId) -> Edn {
    let id = c.unwrap_meta(id);
    match c.kind(id) {
        Kind::Map => {
            let ch: Vec<Edn> = sig(c, id).map(|x| conv(c, x)).collect();
            Edn::Map(ch.chunks(2).filter(|p| p.len() == 2).map(|p| (p[0].clone(), p[1].clone())).collect())
        }
        Kind::Vector | Kind::List | Kind::Set => Edn::Seq(sig(c, id).map(|x| conv(c, x)).collect()),
        Kind::Keyword => Edn::Kw(c.text(id).trim_start_matches(':').to_string()),
        Kind::Symbol => Edn::Sym(c.text(id).to_string()),
        Kind::String => Edn::Str(unescape(c.string_content(id))),
        Kind::Nil => Edn::Nil,
        Kind::True => Edn::Bool(true),
        Kind::False => Edn::Bool(false),
        _ => Edn::Other,
    }
}

fn sig<'a>(c: &'a Cst, id: NodeId) -> impl Iterator<Item = NodeId> + 'a {
    c.sig_children(id).filter(move |&x| c.kind(x) != Kind::Uneval)
}

/// First top-level form (project.clj `(defproject ...)` comes back as `Seq`).
pub fn read_first(src: &str) -> Option<Edn> {
    let c = crate::reader::parse(src);
    let f = sig(&c, c.root()).next()?;
    Some(conv(&c, f))
}

pub fn read_file(p: &std::path::Path) -> Option<Edn> {
    read_first(&std::fs::read_to_string(p).ok()?)
}

/// EDN string escapes (`\\`, `\"`, `\n`, `\t`); unknown escapes are kept.
fn unescape(s: &str) -> String {
    if !s.contains('\\') {
        return s.to_string();
    }
    let mut o = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(ch) = it.next() {
        if ch != '\\' {
            o.push(ch);
            continue;
        }
        match it.next() {
            Some('n') => o.push('\n'),
            Some('t') => o.push('\t'),
            Some(c @ ('\\' | '"')) => o.push(c),
            Some(c) => {
                o.push('\\');
                o.push(c);
            }
            None => o.push('\\'),
        }
    }
    o
}
