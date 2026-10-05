//! kondo `schema.clj` (`expand-schema`) and `analyzer/analyze-schema`: plumatic `schema.core` forms carry
//! `:- Schema` annotations; they are removed from the forms and the schema expressions analyzed afterwards.
use super::forms::DefBy;
use super::*;

impl<'a> Analyzer<'a> {
    fn is_schema_kw(&self, n: NodeId) -> bool {
        self.kind(n) == Kind::Keyword && self.c.ns(n).is_none() && self.c.flags(n) & F_AUTO == 0 && self.c.name(n).as_str() == "-"
    }

    /// kondo `remove-schemas-from-children`: a copy of `expr` without `:- x` pairs (vectors recursively).
    fn remove_schemas_from_children(&mut self, expr: NodeId, schemas: &mut Vec<NodeId>) -> NodeId {
        let kids = self.kids(expr);
        let mut new = Vec::with_capacity(kids.len());
        let mut i = 0;
        while i < kids.len() {
            let c = kids[i];
            if self.is_schema_kw(c) {
                if let Some(&s) = kids.get(i + 1) {
                    schemas.push(s);
                }
                i += 2;
                continue;
            }
            if self.kind(c) == Kind::Vector {
                new.push(self.remove_schemas_from_children(c, schemas));
            } else {
                new.push(c);
            }
            i += 1;
        }
        self.c.push_container(self.c.kind(expr), Some(expr), &new, 0)
    }

    /// kondo `expand-schema` (findings for misplaced return schemas are not produced).
    fn expand_schema(&mut self, fn_sym: &str, expr: NodeId) -> (NodeId, Vec<NodeId>) {
        let children = self.kids(expr);
        let mut new: Vec<NodeId> = Vec::with_capacity(children.len());
        let mut schemas: Vec<NodeId> = Vec::new();
        let mut past_arg_schemas = false;
        let mut index = 0usize;
        let mut i = 0usize;
        while i < children.len() {
            let c = children[i];
            let kind = self.kind(c);
            if fn_sym == "defprotocol" {
                if kind == Kind::List {
                    let e = self.remove_schemas_from_children(c, &mut schemas);
                    new.push(e);
                } else {
                    new.push(c);
                }
                i += 1;
                continue;
            }
            if past_arg_schemas {
                if fn_sym == "defrecord" && kind == Kind::Map {
                    new.extend_from_slice(&children[i + 1..]);
                    schemas.push(c);
                } else {
                    new.extend_from_slice(&children[i..]);
                }
                break;
            }
            if self.is_schema_kw(c) {
                if let Some(&s) = children.get(i + 1) {
                    schemas.push(s);
                }
                i += 2;
                index += 1;
                continue;
            }
            if kind == Kind::Vector && !(fn_sym == "defmethod" && index == 2) {
                let e = self.remove_schemas_from_children(c, &mut schemas);
                new.push(e);
                past_arg_schemas = true;
                index += 1;
                i += 1;
                continue;
            }
            if kind == Kind::List && !self.kids(c).is_empty() {
                let ck = self.kids(c);
                let params = ck[0];
                let e = if self.kind(params) == Kind::Vector {
                    let p = self.remove_schemas_from_children(params, &mut schemas);
                    let mut v = vec![p];
                    v.extend_from_slice(&ck[1..]);
                    self.c.push_container(Kind::List, Some(c), &v, 0)
                } else {
                    c
                };
                new.push(e);
                index += 1;
                i += 1;
                continue;
            }
            new.push(c);
            index += 1;
            i += 1;
        }
        (self.c.push_container(Kind::List, Some(expr), &new, 0), schemas)
    }

    /// kondo `analyze-schema`.
    pub(crate) fn analyze_schema(&mut self, expr: NodeId, fn_sym: &str, by: DefBy) -> Option<expr::ArityInfo> {
        let (e2, schemas) = self.expand_schema(fn_sym, expr);
        let by = DefBy { by: (intern("schema.core"), intern(fn_sym)), lint_as: by.lint_as };
        let ret = match fn_sym {
            "fn" => self.analyze_fn(e2),
            "def" => self.analyze_def(e2, by),
            "defn" => self.analyze_defn(e2, by, false),
            "defmethod" => {
                self.analyze_defmethod(e2);
                None
            }
            "defrecord" => {
                self.analyze_defrecord(e2, by);
                None
            }
            _ => {
                self.analyze_defprotocol(e2, by);
                None
            }
        };
        self.analyze_children(&schemas);
        ret
    }
}
