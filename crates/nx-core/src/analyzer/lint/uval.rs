//! unused-value: kondo `analyzer/lint-unused-value`, `usages/analyze-usages2` (tokens) and
//! `linters/reg-unused-value!` (calls of pure core fns).
use super::*;
use crate::analyzer::defs::{fast_map, FastSet};
use crate::cst::Kind;
use crate::analyzer::syms;
use crate::intern::{intern, SymId};
use std::sync::OnceLock;

/// `(ns, name)` of `var-info/unused-values` (cljs.core normalized to clojure.core).
pub fn unused_values() -> &'static FastSet<(SymId, SymId)> {
    static T: OnceLock<FastSet<(SymId, SymId)>> = OnceLock::new();
    T.get_or_init(|| {
        let txt = include_str!("tables.txt");
        let mut set = FastSet::default();
        let mut sec = "";
        for line in txt.lines() {
            if let Some(s) = line.strip_prefix('@') {
                sec = s;
                continue;
            }
            if sec == "unused-values" {
                if let Some((ns, nm)) = line.split_once('/') {
                    set.insert((intern(ns), intern(nm)));
                }
            }
        }
        set
    })
}

const CORE_PARENTS: &[&str] = &["do", "fn", "defn", "defn-", "let", "when-let", "loop", "binding", "with-open", "doseq", "try", "when", "when-not", "when-first", "when-some", "future"];

#[inline]
fn is_core(ns: SymId) -> bool {
    ns == syms().clojure_core || ns == syms().cljs_core
}

/// Whether a value at `idx` of `len` inside `parent` is discarded (call variant, `reg-unused-value!`).
pub fn call_unused(parent: (SymId, SymId), idx: u32, len: u32) -> bool {
    if idx == u32::MAX {
        return false;
    }
    let (pns, pname) = parent;
    let name = pname.as_str();
    if is_core(pns) && (name == "doseq" || idx + 1 < len) && CORE_PARENTS.contains(&name) {
        return true;
    }
    pns.as_str() == "clojure.test" && name == "deftest"
}

impl<'a> Analyzer<'a> {
    fn uv_ctx(&self) -> Option<((SymId, SymId), u32, u32)> {
        let (idx, len) = (self.ctx.idx, self.ctx.len);
        if idx == u32::MAX || len == u32::MAX || idx + 1 >= len {
            return None;
        }
        let parent = *self.cs.last()?;
        Some((parent, idx, len))
    }

    /// kondo `lint-unused-value` for non-token expressions (vector, map, set, quote, ...).
    pub fn lint_unused_value_expr(&mut self, n: NodeId) {
        if !self.lon || self.ctx.idx == u32::MAX || self.ctx.idx + 1 >= self.ctx.len || self.lc().level(FType::UnusedValue) == OFF {
            return;
        }
        let Some((parent, _, _)) = self.uv_ctx() else { return };
        // `(symbol? (ffirst callstack))`
        if parent.0.is_none() || !is_core(parent.0) || !CORE_PARENTS.contains(&parent.1.as_str()) {
            return;
        }
        let p = self.pos(n);
        if self.lint_is_gen(n) || self.ctx.gen_call || self.lt.cond_pos == (p.row, p.col) {
            return;
        }
        self.lint(FType::UnusedValue, p, "Unused value");
    }

    /// kondo `analyze-usages2`: unused value of a token (symbol, keyword, number, string, ...).
    pub fn lint_unused_value_token(&mut self, n: NodeId, symbol: bool) {
        // most tokens are the last child of their parent: cheap exit before anything else
        if !self.lon || self.ctx.idx == u32::MAX || self.ctx.idx + 1 >= self.ctx.len || self.lc().level(FType::UnusedValue) == OFF {
            return;
        }
        let Some((parent, _, _)) = self.uv_ctx() else { return };
        let core = is_core(parent.0);
        let name = parent.1.as_str();
        let test = !core && matches!(parent.0.as_str(), "clojure.test" | "cljs.test");
        let ok = if core {
            CORE_PARENTS.contains(&name) || matches!(name, "fn*" | "defmethod")
        } else {
            test && name == "deftest"
        };
        let t = self.c.unwrap_meta(n);
        let p = self.pos(t);
        // multi-line strings are a separate node type in kondo and are not linted
        if !ok || (self.kind(t) == Kind::String && p.row != p.end_row) || self.lint_is_gen(n) || self.ctx.gen_call || self.lt.cond_pos == (p.row, p.col) {
            return;
        }
        let _ = symbol;
        let text = self.token_text(t);
        let mut extra: Vec<(&'static str, String)> = Vec::new();
        if t != n {
            // kondo quirk: keyword metadata of the node is copied to the finding (`^:foo x`)
            if let Some((m, _)) = self.c.meta(n) {
                if self.kind(m) == Kind::Keyword && self.c.ns(m).is_none() {
                    let k: &'static str = Box::leak(self.c.name(m).as_str().to_owned().into_boxed_str());
                    extra.push((k, "true".into()));
                    extra.push(("user-meta", format!("[{{\"{}\":true}}]", k)));
                }
            }
        }
        if let Some(f) = self.lint(FType::UnusedValue, p, format!("Unused value: {}", text)) {
            f.extra = extra;
        }
    }

    /// `(str token)` of a token node.
    fn token_text(&self, t: NodeId) -> String {
        match self.kind(t) {
            Kind::Symbol | Kind::Keyword => {
                let s = self.c.text(t);
                if s.is_empty() {
                    self.node_str(t)
                } else {
                    s.to_owned()
                }
            }
            _ => self.c.text(t).to_owned(),
        }
    }
}

#[allow(dead_code)]
fn _f() {
    let _ = fast_map::<u8, u8>;
}

impl<'a> Analyzer<'a> {
    /// kondo `analyze-condition`: analyze `c` in condition position (result dropped).
    pub fn analyze_condition(&mut self, c: NodeId) {
        let saved = self.lt.cond_pos;
        let p = self.pos(self.c.unwrap_meta(c));
        self.lt.cond_pos = (p.row, p.col);
        self.dropped(|a| a.analyze_expression(c));
        self.lt.cond_pos = saved;
    }
    /// Like `analyze_condition` but keeps the result (conditional-let bindings).
    pub fn analyze_condition_nd(&mut self, c: NodeId) {
        let saved = self.lt.cond_pos;
        let p = self.pos(self.c.unwrap_meta(c));
        self.lt.cond_pos = (p.row, p.col);
        self.analyze_expression(c);
        self.lt.cond_pos = saved;
    }
}

impl<'a> Analyzer<'a> {
    /// kondo `:clj-kondo.impl/generated`: synthetic nodes and hook expansions.
    #[inline]
    pub fn lint_is_gen(&self, n: NodeId) -> bool {
        self.ex.gen || self.c.flags(n) & (crate::cst::F_GEN | crate::cst::F_DERIVED) != 0
    }
}
