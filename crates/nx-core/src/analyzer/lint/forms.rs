//! Linters of special forms: recur / loop, do, let, when, if, cond, try, case, ... (kondo `analyzer.clj`).
use super::*;
use crate::analyzer::{syms, Arities, Binding, Name, NO_ARITY};
use crate::cst::Kind;
use crate::intern::SymId;
use super::tags::Tag;
use crate::analyzer::resolve::Resolved;

impl<'a> Analyzer<'a> {
    pub fn lint_new_seen(&mut self) -> u32 {
        self.lt.seen_recur.push(false);
        (self.lt.seen_recur.len() - 1) as u32
    }

    /// Frame of the enclosing call (`(second callstack)`; own frame is the last entry).
    pub fn lint_parent_frame(&self) -> (SymId, SymId) {
        let n = self.cs.len();
        if n >= 2 {
            self.cs[n - 2]
        } else {
            (SymId::NONE, SymId::NONE)
        }
    }

    /// kondo `analyze-recur` (lint part; children are analyzed by the caller).
    pub fn lint_recur(&mut self, expr: NodeId, nargs: u32) {
        if !self.lon {
            return;
        }
        if (self.ctx.seen as usize) < self.lt.seen_recur.len() {
            let i = self.ctx.seen as usize;
            self.lt.seen_recur[i] = true;
        }
        if self.ctx.off & uses::OFF_ARITY != 0 || self.skip_arity_pub() {
            return;
        }
        let p = self.pos(expr);
        let recur = self.ctx.recur;
        if recur == R_NONTAIL {
            self.lint(FType::UnexpectedRecur, p, "Recur can only be used in tail position.");
            return;
        }
        let mut expected: Option<u32> = if recur == R_NONE || recur == R_NIL { None } else { Some(recur) };
        if self.ctx.protocol_fn {
            expected = expected.map(|e| e.saturating_sub(1));
        }
        let (len, idx) = (self.ctx.len, self.ctx.idx);
        if len != u32::MAX && idx != u32::MAX && idx + 1 != len {
            // parent = name of the first enclosing frame that is not a threading macro
            let mut parent = "";
            let n = self.cs.len();
            for i in (0..n.saturating_sub(1)).rev() {
                let nm = self.cs[i].1.as_str();
                if !matches!(nm, "->" | "->>" | "some->" | "some->>" | "doto" | "cond->" | "alt!" | "alt!!") {
                    parent = nm;
                    break;
                }
            }
            if !matches!(parent, "if" | "case" | "cond" | "if-let" | "if-not" | "if-some" | "condp") {
                self.lint(FType::UnexpectedRecur, p, "Recur can only be used in tail position.");
            }
        }
        match expected {
            None => {
                self.lint(FType::UnexpectedRecur, p, "Unexpected usage of recur.");
            }
            Some(e) if e != nargs => {
                self.lint(FType::InvalidArity, p, format!("recur argument count mismatch (expected {}, got {})", e, nargs));
            }
            _ => {}
        }
    }

    /// kondo `analyze-loop`.
    pub fn analyze_loop(&mut self, expr: NodeId) {
        let kids = self.kids(expr);
        let Some(&bv) = kids.get(1) else { return };
        if self.kind(bv) != Kind::Vector {
            return;
        }
        let c = self.c.children(bv).len();
        let seen = self.lint_new_seen();
        self.scope(|a| {
            a.ctx.seen = seen;
            a.ctx.protocol_fn = false;
            a.ctx.recur = if c % 2 == 0 { (c / 2) as u32 } else { R_NIL };
            a.analyze_like_let(expr);
        });
        if !self.lt.seen_recur[seen as usize] {
            let p = self.pos(expr);
            self.lint(FType::LoopWithoutRecur, p, "Loop without recur.");
        }
    }
}

fn is_core_ns(ns: SymId) -> bool {
    ns == syms().clojure_core || ns == syms().cljs_core
}

impl<'a> Analyzer<'a> {
    /// kondo `lint-inline-def!`.
    pub fn lint_inline_def(&mut self, expr: NodeId) {
        if !self.lon || self.ctx.in_comment {
            return;
        }
        let parent = self.lint_parent_frame();
        let inline = !self.ctx.in_def.is_none() || (!self.ctx.top_level && is_core_ns(parent.0) && matches!(parent.1.as_str(), "fn" | "defmethod"));
        if inline {
            let p = self.pos(expr);
            self.lint(FType::InlineDef, p, "inline def");
        }
    }

    /// Pre-analysis linters of a resolved core call, dispatched by the resolved (not lint-as) name
    /// (kondo `lint-specific-calls!`) and by the analyzed-as name (analyze-* functions).
    pub fn lint_core_call(&mut self, expr: NodeId, args: &[NodeId], resolved_core: bool, rname: &str, name: &str, lint_as: bool) {
        if !self.lon {
            return;
        }
        self.lt.lint_as_call = lint_as;
        let p = self.pos(expr);
        let gen = self.lint_is_gen(expr);
        // resolved-name linters (lint-specific-calls!)
        if resolved_core {
            match rname {
                "cond" => self.lint_cond(expr, args),
                "if-let" | "if-not" | "if-some" => {
                    if args.len() == 2 {
                        self.lint(FType::MissingElseBranch, p, "Missing else branch.");
                    }
                }
                "if" => {
                    if args.len() == 2 {
                        self.lint(FType::MissingElseBranch, p, "Missing else branch.");
                    }
                }
                _ => {}
            }
        }
        // analyze-* linters, by name the form is analyzed as
        match name {
            "if" => {
                if args.len() < 2 {
                    self.lint(FType::Syntax, p, "Too few arguments to if.");
                } else if args.len() > 3 {
                    self.lint(FType::Syntax, p, "Too many arguments to if.");
                }
            }
            "when" | "when-not" => {
                if args.len() <= 1 {
                    self.lint(FType::MissingBodyInWhen, p, "Missing body in when");
                }
            }
            "=" | "not=" => self.lint_equals_position(args),
            "do" => self.lint_do(expr, args, gen),
            "let" if resolved_core && rname == "let" => self.lint_let(expr, args, gen),
            "try" => {
                let has = args.iter().any(|&c| matches!(self.symbol_call_name_pub(c), Some("catch") | Some("finally")));
                if !has {
                    self.lint(FType::MissingClauseInTry, p, "Missing catch or finally in try");
                }
            }
            _ => {}
        }
    }

    pub fn symbol_call_name_pub(&self, n: NodeId) -> Option<&'static str> {
        self.symbol_call_name(n)
    }

    /// kondo `analyze-do`: redundant `do`.
    fn lint_do(&mut self, expr: NodeId, args: &[NodeId], gen: bool) {
        let parent = self.lint_parent_frame();
        let core = is_core_ns(parent.0);
        let pname = parent.1.as_str();
        let (pns, _) = parent;
        let _ = pns;
        if gen || matches!(pname, "fn*" | "let*") && core {
            return;
        }
        let ep = self.pos(expr);
        let redundant = args.len() < 2
            || (core
                && self.lt.cond_pos != (ep.row, ep.col)
                && !self.lt.parent_gen
                && matches!(pname, "do" | "fn" | "defn" | "defn-" | "let" | "when-let" | "loop" | "binding" | "with-open" | "doseq" | "try" | "when" | "when-not" | "when-first" | "when-some" | "future" | "catch"));
        if redundant {
            self.lint(FType::RedundantDo, ep, "redundant do");
        }
    }

    /// kondo `analyze-like-let` redundant-let (current call is `let`).
    fn lint_let(&mut self, expr: NodeId, args: &[NodeId], gen: bool) {
        if gen {
            return;
        }
        let parent = self.lint_parent_frame();
        let parent_let = is_core_ns(parent.0) && parent.1.as_str() == "let";
        let bv_empty = args.first().map_or(false, |&b| self.kind(b) == Kind::Vector && self.c.children(b).is_empty());
        if (parent_let && self.ctx.let_parent && !self.lt.parent_gen) || bv_empty {
            let p = self.pos(expr);
            self.lint(FType::RedundantLet, p, "Redundant let expression.");
        }
    }

    /// kondo `linters/lint-cond`.
    fn lint_cond(&mut self, expr: NodeId, args: &[NodeId]) {
        if args.len() % 2 == 1 {
            let p = self.pos(expr);
            self.lint(FType::Syntax, p, "cond requires even number of forms");
            return;
        }
        let conds: Vec<NodeId> = args.iter().step_by(2).copied().collect();
        for (i, &c) in conds.iter().enumerate() {
            if self.constant_truthy(c) {
                let cp = self.pos(c);
                if !self.is_kw_named(c, "else") {
                    self.lint(FType::CondElse, cp, "use :else as the catch-all test expression in cond");
                }
                if let Some(&next) = conds.get(i + 1) {
                    let np = self.pos(next);
                    self.lint(FType::ConstantCondition, np, "Unreachable code");
                }
            }
        }
    }

    /// kondo `utils/constant?` + truthy value.
    fn constant_truthy(&self, n: NodeId) -> bool {
        if !self.is_constant(n) {
            return false;
        }
        let t = self.c.unwrap_meta(n);
        !matches!(self.kind(t), Kind::Nil | Kind::False)
    }

    fn is_constant(&self, n: NodeId) -> bool {
        let n = self.c.unwrap_meta(n);
        match self.kind(n) {
            Kind::Symbol => false,
            Kind::Number | Kind::String | Kind::Char | Kind::Keyword | Kind::Nil | Kind::True | Kind::False | Kind::Regex | Kind::Symbolic => true,
            Kind::Quote => true,
            Kind::Vector | Kind::Set | Kind::Map => self.c.children(n).iter().all(|&c| self.is_constant(c)),
            Kind::NsMap => self.c.nth(n, 1).map_or(false, |m| self.c.children(m).iter().all(|&c| self.is_constant(c))),
            _ => false,
        }
    }

    /// kondo `analyze-condition`: constant-condition verdict of the condition expression.
    pub fn lint_condition(&mut self, c: NodeId, nil_test: bool) {
        if !self.lon || self.lc().level(FType::ConstantCondition) == OFF || self.lint_is_gen(c) || self.lt.cond_arrow.contains(&c) {
            return;
        }
        let t = match self.ty_of(c) {
            Some(t) => t,
            None => match self.rt_of(c) {
                Some(rets::Rt::Ty(t)) => t,
                Some(rt @ (rets::Rt::Call { .. } | rets::Rt::Map { .. })) => {
                    if let rets::Rt::Call { .. } = rt {
                        let p = self.pos(self.c.unwrap_meta(c));
                        self.lt.deferred.push(rets::Deferred { pos: p, rt, nil_test, lang: self.ltag });
                    }
                    return;
                }
                None => return,
            },
        };
        let ks: Vec<types::Kw> = match &t {
            types::Ty::K(k) => vec![*k],
            types::Ty::U(v) => v.clone(),
        };
        let any = types::Kw(types::KNOWN.iter().position(|x| *x == "any").unwrap_or(0) as u8, false);
        let nil = types::Kw(types::KNOWN.iter().position(|x| *x == "nil").unwrap_or(0) as u8, false);
        let boolean = types::Kw(types::KNOWN.iter().position(|x| *x == "boolean").unwrap_or(0) as u8, false);
        let falsy = |k: &types::Kw| !k.1 && (k.0 == nil.0 || types::KNOWN.get(k.0 as usize) == Some(&"false"));
        let tr = types::kw_named("truthy").0;
        let tru = types::kw_named("true").0;
        let non_nil = |k: &types::Kw| !k.1 && (k.0 == tr || (k.0 != any.0 && !types::match_kw(*k, nil)));
        let truthy = |k: &types::Kw| !k.1 && (k.0 == tr || k.0 == tru || (non_nil(k) && !types::match_kw(*k, boolean)));
        let (then, els): (Box<dyn Fn(&types::Kw) -> bool>, Box<dyn Fn(&types::Kw) -> bool>) = if nil_test { (Box::new(|k| non_nil(k)), Box::new(|k| !k.1 && k.0 == nil.0)) } else { (Box::new(truthy), Box::new(falsy)) };
        if ks.iter().all(|k| els(k)) {
            // a literal nil (or a local bound to one) is an intentional way to disable a branch
            let c0 = self.c.unwrap_meta(c);
            let nil_lit_local = self.kind(c0) == Kind::Symbol && self.c.ns(c0).is_none() && self.find_binding(self.c.name(c0)).map_or(false, |b| b.nil_lit);
            if self.kind(c0) != Kind::Nil && !nil_lit_local {
                let p = self.pos(c0);
                self.lint(FType::ConstantCondition, p, "Condition always false");
            }
        } else if ks.iter().all(|k| then(k)) {
            let p = self.pos(self.c.unwrap_meta(c));
            self.lint(FType::ConstantCondition, p, "Condition always true");
        }
    }

    /// kondo `key-linter/lint-map-keys` (duplicate-map-key, missing-map-value) for a map / namespaced-map node.
    pub fn lint_map_keys(&mut self, n: NodeId) {
        if !self.lon {
            return;
        }
        let m = if self.kind(n) == Kind::NsMap { self.c.nth(n, 1) } else { Some(n) };
        let Some(m) = m else { return };
        let mut dups: Vec<NodeId> = Vec::new();
        let (nk, last) = {
            let kids = self.c.children(m);
            let nk = kids.len();
            if nk > 32 {
                // large literal: linear pass; simple tokens hash by text, composite keys by canonical form
                let mut seen_tok: std::collections::HashSet<(u32, u32, &str)> = std::collections::HashSet::new();
                let mut seen_canon: std::collections::HashSet<String> = std::collections::HashSet::new();
                let mut j = 0;
                while j < nk {
                    let k = self.c.unwrap_meta(kids[j]);
                    let dup = match self.kind(k) {
                        Kind::Keyword => !seen_tok.insert((self.c.ns(k).0, self.c.name(k).0 | ((self.c.flags(k) & crate::cst::F_AUTO) as u32) << 31, "")),
                        Kind::Symbol | Kind::Number | Kind::String | Kind::Char | Kind::Nil | Kind::True | Kind::False | Kind::Regex | Kind::Symbolic => !seen_tok.insert((u32::MAX, 0, self.c.text(k))),
                        _ => match self.key_value(k, false) {
                            Some(kv) => !seen_canon.insert(kv),
                            None => false,
                        },
                    };
                    if dup {
                        dups.push(kids[j]);
                    }
                    j += 2;
                }
            } else if nk >= 4 {
                // keys are every other child; compare against earlier keys without allocating
                let mut j = 2;
                while j < nk {
                    let k = kids[j];
                    let mut i = 0;
                    let mut dup = false;
                    if self.key_supported(k) {
                        while i < j {
                            if self.key_supported(kids[i]) && self.key_pair_eq(kids[i], k) {
                                dup = true;
                                break;
                            }
                            i += 2;
                        }
                    }
                    if dup {
                        dups.push(k);
                    }
                    j += 2;
                }
            }
            (nk, kids.last().copied())
        };
        for k in dups {
            let p = self.pos(k);
            let s = self.node_str(k);
            self.lint(FType::DuplicateMapKey, p, format!("duplicate key {}", s));
        }
        if nk % 2 == 1 {
            if let Some(last) = last {
                let p = self.pos(last);
                let s = self.node_str(last);
                self.lint(FType::MissingMapValue, p, format!("missing value for key {}", s));
            }
        }
    }

    /// kondo `key-linter/lint-set`.
    pub fn lint_set_keys(&mut self, n: NodeId) {
        if !self.lon {
            return;
        }
        let mut dups: Vec<NodeId> = Vec::new();
        {
            let kids = self.c.children(n);
            if kids.len() > 16 {
                let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
                for &k in kids {
                    if let Some(kv) = self.key_value(k, false) {
                        if !seen.insert(kv) {
                            dups.push(k);
                        }
                    }
                }
            } else {
                for j in 1..kids.len() {
                    let k = kids[j];
                    if self.key_supported(k) && kids[..j].iter().any(|&p| self.key_supported(p) && self.key_pair_eq(p, k)) {
                        dups.push(k);
                    }
                }
            }
        }
        for k in dups {
            let p = self.pos(k);
            let s = self.key_display(k);
            self.lint(FType::DuplicateSetKey, p, format!("duplicate set element {}", s));
        }
    }

    /// Equality of two supported keys (kondo `key-value`), falling back to the canonical form.
    fn key_pair_eq(&self, a: NodeId, b: NodeId) -> bool {
        match self.key_eq(a, b) {
            Some(r) => r,
            None => match (self.key_value(a, false), self.key_value(b, false)) {
                (Some(x), Some(y)) => x == y,
                _ => false,
            },
        }
    }

    fn key_supported(&self, n: NodeId) -> bool {
        let n = self.c.unwrap_meta(n);
        match self.kind(n) {
            Kind::Symbol | Kind::Number | Kind::String | Kind::Char | Kind::Nil | Kind::True | Kind::False | Kind::Regex | Kind::Symbolic | Kind::Keyword => true,
            Kind::Vector | Kind::List | Kind::Set => self.c.children(n).iter().all(|&c| self.key_supported(c)),
            Kind::Map => {
                let k = self.c.children(n);
                k.len() % 2 == 0 && k.iter().all(|&c| self.key_supported(c))
            }
            Kind::Quote => self.c.nth(n, 0).map_or(false, |x| self.key_supported(x)),
            _ => false,
        }
    }

    /// Allocation-free equality of two supported keys; `None` = needs the canonical form.
    fn key_eq(&self, a: NodeId, b: NodeId) -> Option<bool> {
        let (a, b) = (self.c.unwrap_meta(a), self.c.unwrap_meta(b));
        let (ka, kb) = (self.kind(a), self.kind(b));
        match (ka, kb) {
            (Kind::Keyword, Kind::Keyword) => Some(self.c.name(a) == self.c.name(b) && self.c.ns(a) == self.c.ns(b) && (self.c.flags(a) & crate::cst::F_AUTO) == (self.c.flags(b) & crate::cst::F_AUTO)),
            (Kind::Vector | Kind::List, Kind::Vector | Kind::List) => {
                let (ca, cb) = (self.c.children(a), self.c.children(b));
                if ca.len() != cb.len() {
                    return Some(false);
                }
                for (&x, &y) in ca.iter().zip(cb.iter()) {
                    match self.key_eq(x, y) {
                        Some(true) => {}
                        other => return other,
                    }
                }
                Some(true)
            }
            (Kind::Set, Kind::Set) | (Kind::Map, Kind::Map) | (Kind::Quote, _) | (_, Kind::Quote) => None,
            (Kind::Set | Kind::Map | Kind::Vector | Kind::List | Kind::Keyword, _) | (_, Kind::Set | Kind::Map | Kind::Vector | Kind::List | Kind::Keyword) => Some(false),
            _ => Some(self.c.text(a) == self.c.text(b)),
        }
    }

    /// `(str k)` of a key-value in the set message: keyword/symbol/str as printed by Clojure.
    fn key_display(&self, k: NodeId) -> String {
        let n = self.c.unwrap_meta(k);
        match self.kind(n) {
            Kind::Keyword => {
                let ns = self.c.ns(n);
                if self.c.flags(n) & crate::cst::F_AUTO != 0 {
                    self.node_str(n)
                } else if ns.is_none() {
                    format!(":{}", self.c.name(n).as_str())
                } else {
                    format!(":{}/{}", ns.as_str(), self.c.name(n).as_str())
                }
            }
            Kind::Vector | Kind::List => {
                let v: Vec<String> = self.c.children(n).iter().map(|&c| self.key_display(c)).collect();
                format!("[{}]", v.join(" "))
            }
            _ => self.node_str(n),
        }
    }

    /// kondo `key-linter/key-value`: canonical text of a constant key, `None` when unsupported.
    fn key_value(&self, n: NodeId, in_quote: bool) -> Option<String> {
        let n = self.c.unwrap_meta(n);
        match self.kind(n) {
            Kind::Symbol | Kind::Number | Kind::String | Kind::Char | Kind::Nil | Kind::True | Kind::False | Kind::Regex | Kind::Symbolic => {
                let quote = in_quote && !self.is_constant(n);
                Some(format!("S{}{}", if quote { "'" } else { "" }, self.node_str(n)))
            }
            Kind::Keyword => Some(format!("K{}", self.key_display(n))),
            Kind::Vector | Kind::List => {
                let mut v = Vec::new();
                for &c in self.c.children(n) {
                    v.push(self.key_value(c, in_quote)?);
                }
                Some(format!("L[{}]", v.join("\u{1}")))
            }
            Kind::Set => {
                let mut v = Vec::new();
                for &c in self.c.children(n) {
                    v.push(self.key_value(c, in_quote)?);
                }
                v.sort();
                v.dedup();
                Some(format!("T{{{}}}", v.join("\u{1}")))
            }
            Kind::Map => {
                let kids = self.c.children(n);
                if kids.len() % 2 != 0 {
                    return None;
                }
                let mut v = Vec::new();
                for ch in kids.chunks(2) {
                    let a = self.key_value(ch[0], in_quote)?;
                    let b = self.key_value(ch[1], in_quote)?;
                    v.push(format!("{}\u{2}{}", a, b));
                }
                v.sort();
                Some(format!("M{{{}}}", v.join("\u{1}")))
            }
            Kind::Quote => self.c.nth(n, 0).and_then(|x| self.key_value(x, true)),
            _ => None,
        }
    }

    /// kondo `analyze-case`: duplicate / quoted test constants.
    pub fn lint_case_tests(&mut self, tests: &[NodeId], seen: &mut Vec<String>) {
        if !self.lon {
            return;
        }
        for &constant in tests {
            let c0 = self.c.unwrap_meta(constant);
            let list_const = self.kind(c0) == Kind::List;
            let cands: Vec<NodeId> = if list_const { self.kids(c0) } else { vec![constant] };
            if self.kind(c0) == Kind::Quote {
                let p = self.pos(c0);
                self.lint(FType::CaseQuotedTest, p, "Case test is compile time constant and should not be quoted.");
            }
            let mut local = seen.clone();
            for (i, &d) in cands.iter().enumerate() {
                let d0 = self.c.unwrap_meta(d);
                if self.kind(d0) == Kind::Quote {
                    let p = self.pos(d0);
                    self.lint(FType::CaseQuotedTest, p, "Case test is compile time constant and should not be quoted.");
                }
                let s = self.node_str(d);
                if local.contains(&s) {
                    let p = self.pos(d0);
                    self.lint(FType::CaseDuplicateTest, p, format!("Duplicate case test constant: {}", s));
                }
                if i + 1 < cands.len() {
                    local.push(s);
                }
            }
            for &d in &cands {
                seen.push(self.node_str(d));
            }
        }
    }

    /// kondo `analyze-format`: format / printf with a literal format string.
    pub fn lint_format(&mut self, args: &[NodeId]) {
        if !self.lon {
            return;
        }
        let Some(&f) = args.first() else { return };
        let Some(fs) = self.string_of(f) else { return };
        let (percent_count, _) = format_percents(&fs);
        let arg_count = args.len() - 1;
        let counts_match = percent_count == arg_count;
        let p = self.pos(f);
        if !counts_match {
            self.lint(FType::Format, p, format!("Format string expects {} arguments instead of {}.", percent_count, arg_count));
        }
        if percent_count == 0 && counts_match {
            self.lint(FType::RedundantFormat, p, "Format string contains no format specifiers");
        }
    }
}

/// `analyze-format-string`: number of arguments the format string expects.
fn format_percents(s: &str) -> (usize, usize) {
    let s = s.replace("%%", "").replace("%n", "");
    let b: Vec<char> = s.chars().collect();
    let (mut indexed, mut unindexed) = (0usize, 0usize);
    let mut i = 0;
    while i < b.len() {
        if b[i] == '%' && i + 1 < b.len() {
            // regex `%.[^\s%]*`
            let mut j = i + 2;
            while j < b.len() && !b[j].is_whitespace() && b[j] != '%' {
                j += 1;
            }
            let pct: String = b[i..j].iter().collect();
            // `%(\d+)\$`
            let digits: String = pct[1..].chars().take_while(|c| c.is_ascii_digit()).collect();
            if !digits.is_empty() && pct[1 + digits.len()..].starts_with('$') {
                indexed = indexed.max(digits.parse().unwrap_or(0));
            } else if b[i + 1] != '<' {
                unindexed += 1;
            }
            i = j;
        } else {
            i += 1;
        }
    }
    (indexed.max(unindexed), 0)
}

impl<'a> Analyzer<'a> {
    fn skip_not_a_function(&self) -> bool {
        let Some(c) = self.lc().linter_cfg(FType::NotAFunction) else { return false };
        c.skip_args.iter().any(|fq| {
            let (ns, nm) = fq.as_str().split_once('/').unwrap_or(("", fq.as_str()));
            self.cs.iter().any(|&(cns, cname)| !cns.is_none() && cns.as_str() == ns && cname.as_str() == nm)
        })
    }

    /// `(lint-map-call! / lint-vector-call! / lint-set-call!)` for a collection literal in call position.
    pub fn lint_coll_call(&mut self, expr: NodeId, head_kind: Kind, arg_count: usize) {
        if !self.lon || self.skip_arity_pub() {
            return;
        }
        let p = self.pos(expr);
        match head_kind {
            Kind::Map => {
                if arg_count == 0 || arg_count > 2 {
                    self.lint(FType::InvalidArity, p, format!("map is called with {} args but expects 1 or 2", arg_count));
                }
            }
            Kind::Vector => {
                if arg_count != 1 {
                    self.lint(FType::InvalidArity, p, format!("Vector can only be called with 1 arg but was called with: {}", arg_count));
                }
            }
            _ => {
                let cljs = self.is_cljs();
                let ok = arg_count == 1 || (cljs && arg_count == 2);
                if !ok {
                    self.lint(FType::InvalidArity, p, format!("Set can only be called with {} but was called with: {}", if cljs { "1 or 2 args" } else { "1 arg" }, arg_count));
                }
            }
        }
    }

    /// `lint-keyword-call!`.
    pub fn lint_keyword_call(&mut self, expr: NodeId, kw: NodeId, arg_count: usize) {
        if !self.lon || self.skip_arity_pub() {
            return;
        }
        if arg_count == 0 || arg_count > 2 {
            let (ns, name) = (self.c.ns(kw), self.c.name(kw));
            let auto = self.c.flags(kw) & crate::cst::F_AUTO != 0;
            let kw_str = if auto {
                let rns = if ns.is_none() { self.cur_ns_name() } else { self.cur_ns().qualify.get(&ns).copied().unwrap_or(ns) };
                format!("{}/{}", rns.as_str(), name.as_str())
            } else if ns.is_none() {
                name.as_str().to_owned()
            } else {
                format!("{}/{}", ns.as_str(), name.as_str())
            };
            let p = self.pos(expr);
            self.lint(FType::InvalidArity, p, format!("keyword :{} is called with {} args but expects 1 or 2", kw_str, arg_count));
        }
    }

    /// `reg-not-a-function!` (literal in call position) / `lint-symbol-call!`.
    pub fn lint_literal_call(&mut self, n: NodeId, typ: &str) {
        if !self.lon || self.skip_not_a_function() {
            return;
        }
        let p = self.pos(n);
        self.lint(FType::NotAFunction, p, format!("a {} is not a function", typ));
    }

    pub fn lint_symbol_call(&mut self, expr: NodeId, arg_count: usize) {
        if !self.lon || self.skip_arity_pub() {
            return;
        }
        if arg_count == 0 || arg_count > 2 {
            let p = self.pos(expr);
            self.lint(FType::InvalidArity, p, format!("symbol is called with {} args but expects 1 or 2", arg_count));
        }
    }

    /// Head of a list that is not a symbol (kondo `analyze-expression**` :list, non-symbol head).
    pub fn lint_head_call(&mut self, expr: NodeId, function: NodeId, arg_count: usize) {
        if !self.lon {
            return;
        }
        match self.kind(function) {
            Kind::Map | Kind::Vector | Kind::Set => self.lint_coll_call(expr, self.kind(function), arg_count),
            Kind::Keyword => self.lint_keyword_call(expr, function, arg_count),
            Kind::Quote => {
                if let Some(q) = self.c.nth(function, 0) {
                    let q = self.c.unwrap_meta(q);
                    match self.kind(q) {
                        Kind::Symbol => self.lint_symbol_call(expr, arg_count),
                        Kind::List => self.lint_literal_call(q, "list"),
                        Kind::True | Kind::False => self.lint_literal_call(q, "boolean"),
                        Kind::String => self.lint_literal_call(q, "string"),
                        Kind::Char => self.lint_literal_call(q, "character"),
                        Kind::Number => self.lint_literal_call(q, "number"),
                        _ => {}
                    }
                }
            }
            Kind::True | Kind::False => self.lint_literal_call(function, "boolean"),
            Kind::String => self.lint_literal_call(function, "string"),
            Kind::Char => self.lint_literal_call(function, "character"),
            Kind::Number => self.lint_literal_call(function, "number"),
            _ => {}
        }
    }

    /// kondo `analyze-def` argument checks.
    pub fn lint_def_args(&mut self, expr: NodeId, kids: &[NodeId]) {
        if !self.lon {
            return;
        }
        let children: &[NodeId] = if kids.len() > 2 { &kids[2..] } else { &[] };
        let mut rest = children;
        if children.len() > 1 && self.string_of(children[0]).is_some() {
            rest = &children[1..];
        }
        let core_def = matches!(self.cs.last(), Some(&(ns, n)) if is_core_ns(ns) && n.as_str() == "def");
        let p = self.pos(expr);
        if core_def && rest.len() > 1 {
            self.lint(FType::InvalidArity, p, "Too many arguments to def");
        }
        if rest.is_empty() {
            self.lint(FType::UninitializedVar, p, "Uninitialized var");
        }
    }

    /// kondo `constant?`: compile-time constant expression.
    fn lint_constant(&self, n: NodeId) -> bool {
        match self.kind(n) {
            Kind::Nil | Kind::True | Kind::False | Kind::String | Kind::Number | Kind::Keyword | Kind::Char | Kind::Quote => true,
            Kind::Vector | Kind::Set | Kind::Map => self.c.sig_children(n).all(|x| self.lint_constant(x)),
            Kind::NsMap => self.c.sig_children(n).last().map_or(false, |m| self.c.sig_children(m).all(|x| self.lint_constant(x))),
            _ => false,
        }
    }

    /// kondo `analyze-=-not=`: `:equals-expected-position`.
    fn lint_equals_position(&mut self, args: &[NodeId]) {
        let level = self.lc().level(FType::EqualsExpectedPosition);
        if level == OFF || args.len() != 2 {
            return;
        }
        let (lhs, rhs) = (args[0], args[1]);
        let last = self.lc().linter_cfg(FType::EqualsExpectedPosition).map_or(false, |c| c.kws.iter().any(|(k, v)| k == "position" && v == "last"));
        let only_test = self.lc().lint_bool(FType::EqualsExpectedPosition, "only-in-test-assertion");
        if only_test {
            let n = self.cs.len();
            let in_is = n >= 2 && matches!(self.cs.get(n - 2), Some(&(ns, nm)) if nm.as_str() == "is" && matches!(ns.as_str(), "clojure.test" | "cljs.test"));
            if !in_is {
                return;
            }
        }
        let (want, other) = if last { (lhs, rhs) } else { (rhs, lhs) };
        if self.lint_constant(want) && !self.lint_constant(other) && !self.lint_is_gen(want) {
            let p = self.pos(want);
            self.lint(FType::EqualsExpectedPosition, p, format!("Write expected value {}", if last { "last" } else { "first" }));
        }
    }

    /// kondo `def-fn?` in `analyze-fn`: `(def x (fn ..))` or `(def x (let [..] (fn ..)))`.
    pub fn lint_def_fn(&mut self, expr: NodeId) {
        if !self.lon || self.lc().level(FType::DefFn) == OFF {
            return;
        }
        let n = self.cs.len();
        let is = |f: Option<&Name>, nm: &str| f.map_or(false, |&(ns, name)| is_core_ns(ns) && name.as_str() == nm);
        let parent = if n >= 2 { self.cs.get(n - 2) } else { None };
        let extra = if n >= 3 { self.cs.get(n - 3) } else { None };
        if is(parent, "def") || (is(parent, "let") && is(extra, "def")) {
            let p = self.pos(expr);
            self.lint(FType::DefFn, p, "Use defn instead of def + fn");
        }
    }

    /// `analyze-locking`.
    pub fn lint_locking(&mut self, args: &[NodeId]) {
        if !self.lon {
            return;
        }
        let Some(&obj) = args.first() else { return };
        let only_object = args.len() == 1;
        let o = self.c.unwrap_meta(obj);
        let no_symbol = self.kind(o) != Kind::Symbol && self.kind(o) == Kind::List;
        let t = self.lint_tag(obj);
        let interned = matches!(t, Tag::Keyword | Tag::String | Tag::Boolean | Tag::Number);
        if only_object || no_symbol || interned {
            let msg = if only_object {
                "no body provided"
            } else if interned {
                "use of interned object"
            } else {
                "object is local to locking scope"
            };
            let p = self.pos(o);
            self.lint(FType::LockingSuspiciousLock, p, format!("Suspicious lock object: {}", msg));
        }
    }

    /// `lint-valid-call!` linters that only need the call expression and its resolved core name.
    pub fn lint_call_post(&mut self, expr: NodeId, r: &Resolved, arg_count: u32) {
        if !self.lon || !r.found || r.unresolved || !is_core_ns(r.ns) {
            return;
        }
        let name = r.name.as_str();
        let p = self.pos(expr);
        if arg_count == 1 {
            if matches!(name, "=" | ">" | "<" | ">=" | "<=" | "==" | "not=") {
                self.lint(FType::SingleOperandComparison, p, format!("Single operand use of {}/{} is always {}", r.ns.as_str(), name, name != "not="));
            }
            if matches!(name, "and" | "or") {
                self.lint(FType::SingleLogicalOperand, p, format!("Single arg use of {} always returns the arg itself", name));
            }
        }
        if arg_count == 1 && name == "str" && !self.lint_is_gen(expr) {
            if let Some(&a) = self.c.children(expr).get(1) {
                if self.lint_tag(a) == Tag::String {
                    self.lint(FType::RedundantStrCall, p, "Single argument to str already is a string");
                }
            }
        }
        if matches!(name, "*" | "*'" | "+" | "+'" | "and" | "or" | "lazy-cat" | "max" | "merge" | "min" | "str") {
            let n = self.cs.len();
            // own frame is pushed by the caller right after this check: the parent call is the last frame
            if n >= 1 && self.cs[n - 1] == (r.ns, r.name) && true {
                let (prow, pcol) = self.lt.call_pos;
                if self.c.flags(expr) & crate::cst::F_DERIVED == 0 && prow != 0 && p.row != 0 && prow <= p.row && pcol < p.col {
                    self.lint(FType::RedundantNestedCall, p, format!("Redundant nested call: {}", name));
                }
            }
        }
    }
}

/// State of a var for `redefined-var` (kondo namespace `:vars` / `:var-counts`).
#[derive(Clone, Copy)]
pub struct VarRec {
    pub temp: bool,
    pub declared: bool,
    pub in_comment: bool,
    pub count: u32,
}

impl<'a> Analyzer<'a> {
    /// kondo `namespace/reg-var!` linters: redefined-var.
    pub fn lint_reg_var(&mut self, name: SymId, expr: NodeId, temp: bool, declared: bool, by: Name) {
        if !self.lon || name.is_none() {
            return;
        }
        let ns = self.cur_ns_name();
        let prev = self.lt.vars.get(&(ns, name)).copied();
        let in_comment = self.ctx.in_comment;
        if !(temp && prev.is_none()) {
            let hard = !declared && !in_comment;
            let curr_count = prev.map_or(0, |p| p.count);
            if self.ctx.top_level && hard && !(by.0.as_str() == "clojure.core" && by.1.as_str() == "definterface") {
                let redefined: Option<SymId> = if prev.map_or(false, |p| !p.temp && !p.declared) {
                    Some(ns)
                } else if let Some(&(rns, _)) = self.cur_ns().referred.get(&name) {
                    Some(rns)
                } else {
                    let core = self.core_ns();
                    if ns != core && !self.cur_ns().clojure_excluded.contains(&name) && crate::analyzer::defs::core_sym(self.is_cljs(), name) {
                        Some(core)
                    } else {
                        None
                    }
                };
                if let Some(rns) = redefined {
                    if curr_count > 0 || rns != ns {
                        let p = self.pos(expr);
                        let msg = if rns == ns { format!("redefined var #'{}/{}", rns.as_str(), name.as_str()) } else { format!("{} already refers to #'{}/{}", name.as_str(), rns.as_str(), name.as_str()) };
                        self.lint(FType::RedefinedVar, p, msg);
                    }
                }
            }
        }
        // update the record
        let keep_prev = in_comment && prev.map_or(false, |p| !p.in_comment && !p.temp);
        let hard_def = !temp && !declared && !in_comment;
        let e = self.lt.vars.entry((ns, name)).or_insert(VarRec { temp, declared, in_comment, count: 0 });
        if !keep_prev {
            e.temp = temp;
            e.declared = declared;
            e.in_comment = in_comment;
        }
        if hard_def {
            e.count += 1;
        }
    }
}

impl<'a> Analyzer<'a> {
    /// kondo `lint-unused-private-vars!`.
    pub fn lint_unused_private_vars(&mut self) {
        if self.lc().level(FType::UnusedPrivateVar) == OFF {
            return;
        }
        let mut used: crate::analyzer::defs::FastSet<(SymId, SymId)> = Default::default();
        for u in &self.out.var_usages {
            if u.lang == self.ltag && u.from_var != u.name {
                used.insert((u.from, u.name));
            }
        }
        let defs: Vec<(SymId, SymId, Pos, bool, Name)> = self.out.var_definitions.iter().filter(|d| d.lang == self.ltag && d.private).map(|d| (d.ns, d.name, d.name_pos, false, d.defined_by_lint_as)).collect();
        for (ns, name, pos, _, by) in defs {
            let in_comment = self.lt.vars.get(&(ns, name)).map_or(false, |v| v.in_comment);
            if in_comment || used.contains(&(ns, name)) || name.as_str().starts_with('_') {
                continue;
            }
            if matches!((by.0.as_str(), by.1.as_str()), ("clojure.core" | "cljs.core", "defrecord" | "deftype" | "definterface")) {
                continue;
            }
            let full = format!("{}/{}", ns.as_str(), name.as_str());
            if self.lc().excluded(FType::UnusedPrivateVar, &full) {
                continue;
            }
            self.lint(FType::UnusedPrivateVar, pos, format!("Unused private var {}", full));
        }
    }
}

impl<'a> Analyzer<'a> {
    /// kondo `extract-bindings` `:fn-dupes`: duplicate param names in one arglist.
    pub fn lint_fn_dupe(&mut self, tok: NodeId, name: SymId) {
        let Some(seen) = self.lt.fn_dupes.as_ref() else { return };
        let nm = name.as_str();
        let full = if self.c.ns(tok).is_none() { None } else { Some(self.node_str(tok)) };
        if seen.contains(&name) && nm != "_" && nm != "&" {
            let p = self.pos(tok);
            self.lint(FType::ShadowedFnParam, p, format!("Shadowed fn param: {}", full.unwrap_or_else(|| nm.to_owned())));
        }
        if let Some(seen) = self.lt.fn_dupes.as_mut() {
            seen.push(name);
        }
    }
}

impl<'a> Analyzer<'a> {
    /// kondo `analyze-like-let`: `assert-vector` + `lint-even-forms-bindings!`.
    pub fn lint_let_binding_vector(&mut self, bv: NodeId) {
        if !self.lon {
            return;
        }
        let call = self.cs.last().map_or("", |c| c.1.as_str());
        if self.kind(bv) != Kind::Vector {
            let p = self.pos(bv);
            self.lint(FType::Syntax, p, format!("{} requires a vector for its binding", call));
        } else if self.c.children(bv).len() % 2 == 1 {
            let p = self.pos(bv);
            self.lint(FType::Syntax, p, format!("{} binding vector requires even number of forms", call));
        }
    }
}

impl<'a> Analyzer<'a> {
    /// Register the arity info of a local fn, returning the `Binding::ar` handle (0 when not linting).
    pub fn lint_arity_info(&mut self, fixed: Arities, varargs_min: Option<u32>) -> u32 {
        if !self.lon {
            return 0;
        }
        self.lt.arities.push(ArInfo { fixed, varargs_min });
        self.lt.arities.len() as u32
    }

    /// kondo `arity-match?`.
    fn arity_ok(info: &ArInfo, n: usize) -> bool {
        info.fixed.has(n as u32) || info.varargs_min.map_or(false, |m| n as u32 >= m)
    }

    /// `analyze-binding-call` arity check.
    pub fn lint_binding_call(&mut self, expr: NodeId, b: Binding, nargs: usize) {
        // kondo analyze-binding-call: a local with a known non-ifn tag cannot be called
        if self.lon && b.tag != 0 && self.ctx.off & uses::OFF_TYPE == 0 && self.lc().level(FType::TypeMismatch) != OFF {
            let id = (b.tag >> 1) - 1;
            if id < types::UNION_BASE as u16 && b.tag & 1 == 0 {
                let kw = types::Kw(id as u8, false);
                if !types::match_kw(kw, types::kw_named("ifn")) {
                    let l = types::label(kw);
                    let mut cs = l.chars();
                    let cap: String = cs.next().map(|c| c.to_uppercase().collect::<String>()).unwrap_or_default() + cs.as_str();
                    let p = self.pos(expr);
                    self.lint(FType::TypeMismatch, p, format!("{} cannot be called as a function.", cap));
                }
            }
        }
        if !self.lon || b.ar == 0 || self.skip_arity_pub() {
            return;
        }
        let info = self.lt.arities[(b.ar - 1) as usize];
        if !Self::arity_ok(&info, nargs) {
            let p = self.pos(expr);
            let msg = format!("{} is called with {} {} but expects {}", b.key.as_str(), nargs, if nargs == 1 { "arg" } else { "args" }, uses::show_arities_pub(info.fixed, info.varargs_min.map_or(NO_ARITY, |m| m as u16)));
            self.lint(FType::InvalidArity, p, msg);
        }
    }

    /// `analyze-hof` arity check of a fn argument with known arity.
    pub fn lint_hof_arity(&mut self, f: NodeId, name: &str, info: ArInfo, nargs: usize) {
        if !self.lon || self.skip_arity_pub() {
            return;
        }
        if !Self::arity_ok(&info, nargs) {
            let p = self.pos(self.c.unwrap_meta(f));
            let msg = format!("{} is called with {} {} but expects {}", name, nargs, if nargs == 1 { "arg" } else { "args" }, uses::show_arities_pub(info.fixed, info.varargs_min.map_or(NO_ARITY, |m| m as u16)));
            self.lint(FType::InvalidArity, p, msg);
        }
    }
}

impl<'a> Analyzer<'a> {
    /// kondo `expand-do-template` findings; `pos` = position of the node (none for `are`).
    pub fn lint_do_template(&mut self, pos: Option<Pos>, argc: usize, values: usize) {
        if !self.lon {
            return;
        }
        let p = pos.unwrap_or(Pos { row: 0, col: 0, end_row: 0, end_col: 0 });
        if argc == 0 {
            self.lint(FType::DoTemplate, p, "No args defined. Expected at least 1.");
        } else if values == 0 {
            self.lint(FType::DoTemplate, p, format!("No values provided. Expected: multiple of {}.", argc));
        } else if values % argc != 0 {
            self.lint(FType::DoTemplate, p, format!("Incorrect number of values provided. Expected: multiple of {}.", argc));
        }
    }
}

impl<'a> Analyzer<'a> {
    /// `key-linter/lint-map-keys` on a synthetic map of `children` with a known key set (spec fdef / keys).
    pub fn lint_known_keys(&mut self, children: &[NodeId], known: &[&str]) {
        if !self.lon {
            return;
        }
        let mut seen: Vec<String> = Vec::new();
        let mut i = 0;
        while i < children.len() {
            let k = children[i];
            i += 2;
            if let Some(kv) = self.key_value(k, false) {
                if seen.contains(&kv) {
                    let p = self.pos(k);
                    let s = self.node_str(k);
                    self.lint(FType::DuplicateMapKey, p, format!("duplicate key {}", s));
                }
                let is_known = self.kind(self.c.unwrap_meta(k)) == Kind::Keyword && self.c.ns(self.c.unwrap_meta(k)).is_none() && known.contains(&self.c.name(self.c.unwrap_meta(k)).as_str());
                if !is_known {
                    let p = self.pos(k);
                    let s = self.node_str(k);
                    self.lint(FType::Syntax, p, format!("unknown option {}", s));
                }
                seen.push(kv);
            }
        }
        if children.len() % 2 == 1 {
            if let Some(&last) = children.last() {
                let p = self.pos(last);
                let s = self.node_str(last);
                self.lint(FType::MissingMapValue, p, format!("missing value for key {}", s));
            }
        }
    }
}

impl<'a> Analyzer<'a> {
    /// `meta/meta-node->map`: map metadata is key-linted.
    pub fn lint_meta_keys(&mut self, m: NodeId) {
        if self.lon && self.kind(m) == Kind::Map {
            self.lint_map_keys(m);
        }
    }
}

impl<'a> Analyzer<'a> {
    /// kondo `analyze-defn`: `(defn ^String f [] ..)` return type hint on the name (clj only).
    pub fn lint_return_type_hint(&mut self, name_node: NodeId) {
        if !self.lon || self.lang != crate::analyzer::Lang::Clj || self.kind(name_node) != Kind::Meta {
            return;
        }
        // the tag is the last symbol metadata; the reported node is the first metadata node printing the same
        let mut metas: Vec<NodeId> = Vec::new();
        let mut cur = name_node;
        while let Some((m, t)) = self.c.meta(cur) {
            metas.push(m);
            cur = t;
        }
        metas.reverse();
        let Some(tag) = metas.iter().rev().find(|&&m| self.kind(m) == Kind::Symbol).map(|&m| self.node_str(m)) else { return };
        if let Some(&m) = metas.iter().find(|&&m| self.node_str(m) == tag) {
            let p = self.pos(m);
            self.lint(FType::NonArgVecReturnTypeHint, p, format!("Prefer placing return type hint on arg vector: {}", tag));
        }
    }
}
