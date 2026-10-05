//! Minimal expression type tags (kondo `types/expr->tag`, `constant-verdict`): literals, `str`, collection
//! literals, quoted forms. Enough for constant-condition, redundant-str-call and locking checks.
use super::*;
use crate::cst::Kind;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tag {
    Unknown,
    Nil,
    Boolean,
    String,
    Number,
    Keyword,
    Symbol,
    Char,
    Regex,
    Vector,
    Map,
    Set,
    List,
    Fn,
    Var,
}

impl Tag {
    /// kondo `constant-verdict` = `:always-true`: a tag that is never nil and never boolean.
    pub fn always_true(self) -> bool {
        !matches!(self, Tag::Unknown | Tag::Nil | Tag::Boolean)
    }
}

impl<'a> Analyzer<'a> {
    /// Tag of an expression (`None` semantics = `Tag::Unknown`).
    pub fn lint_tag(&self, n: NodeId) -> Tag {
        let n = self.c.unwrap_meta(n);
        match self.kind(n) {
            Kind::Nil => Tag::Nil,
            Kind::True | Kind::False => Tag::Boolean,
            Kind::String => Tag::String,
            Kind::Number => Tag::Number,
            Kind::Keyword => Tag::Keyword,
            Kind::Char => Tag::Char,
            Kind::Regex => Tag::Regex,
            Kind::Vector => Tag::Vector,
            Kind::Map | Kind::NsMap => Tag::Map,
            Kind::Set => Tag::Set,
            Kind::AnonFn => Tag::Fn,
            Kind::Var => Tag::Var,
            Kind::Quote => match self.c.nth(n, 0) {
                Some(x) => match self.kind(x) {
                    Kind::Symbol => Tag::Symbol,
                    Kind::List => Tag::List,
                    Kind::Vector => Tag::Vector,
                    Kind::Map => Tag::Map,
                    Kind::Set => Tag::Set,
                    Kind::Keyword => Tag::Keyword,
                    Kind::String => Tag::String,
                    Kind::Number => Tag::Number,
                    Kind::Char => Tag::Char,
                    _ => Tag::Unknown,
                },
                None => Tag::Unknown,
            },
            Kind::List => {
                let Some(h) = self.c.nth(n, 0) else { return Tag::Unknown };
                if self.kind(h) != Kind::Symbol {
                    return Tag::Unknown;
                }
                let (ns, name) = (self.c.ns(h), self.c.name(h));
                if !ns.is_none() && ns.as_str() != "clojure.core" && ns.as_str() != "cljs.core" {
                    return Tag::Unknown;
                }
                // shadowed or renamed core fns resolve elsewhere
                if ns.is_none() && (self.find_binding(name).is_some() || self.cur_ns().vars.contains(&name) || self.cur_ns().referred.contains_key(&name) || self.cur_ns().clojure_excluded.contains(&name)) {
                    return Tag::Unknown;
                }
                match name.as_str() {
                    "str" | "subs" | "format" => Tag::String,
                    "list" => Tag::List,
                    _ => Tag::Unknown,
                }
            }
            _ => Tag::Unknown,
        }
    }
}
