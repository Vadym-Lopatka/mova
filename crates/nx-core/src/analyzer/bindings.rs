//! Binding extraction: kondo `extract-bindings` / `extract-map-bindings` (destructuring).
use super::extras::KwOpts;
use super::*;

#[derive(Clone, Copy, Default)]
pub(crate) struct BOpts {
    pub fn_args: bool,
    pub keys_destructuring: bool,
    /// `:keys`/`:keys!` => symbols in the vector are also keyword usages.
    pub keys_kind: bool,
    pub allow_amp: bool,
    pub namespaced_map: bool,
    /// `:destructuring-expr`: the `:keys`/`:syms`/`:strs` keyword node.
    pub destr: Option<NodeId>,
    /// Init tag of a `let` binding (kondo `{:tag tag}` opts), see `Binding::tag`.
    pub tag: u16,
    pub nil_lit: bool,
}

impl<'a> Analyzer<'a> {
    /// Register a new local (kondo `reg-binding!` with `:analyze-locals?`), return its binding.
    fn new_local(&mut self, key: SymId, tok: NodeId, scope_end: (u32, u32)) -> Binding {
        let skip = self.c.flags(tok) & F_SKIP != 0 && self.kind(tok) == Kind::Symbol;
        let gen = self.c.is_gen(tok);
        self.out.next_local_id += 1;
        let id = self.out.next_local_id;
        if !skip && !gen && self.opts.locals {
            let p = self.pos(tok);
            let s = intern(&self.node_str(tok));
            self.out.locals.push(Local { id, name: key, str_: s, pos: p, scope_end_row: scope_end.0, scope_end_col: scope_end.1, lang: self.ltag });
        }
        self.lint_reg_binding(id, key, tok, skip || self.ctx.mark_bindings_used);
        Binding { key, name: key, id, gen, mark_used: skip, ar: 0, tag: 0, nil_lit: false }
    }

    pub fn scope_end(&self, scoped: NodeId) -> (u32, u32) {
        let p = self.pos(scoped);
        (p.end_row, p.end_col)
    }

    /// kondo `extract-bindings`. New bindings are appended to `out` (not pushed to the scope).
    pub fn extract_bindings(&mut self, expr: NodeId, scoped: NodeId, opts: BOpts, out: &mut Vec<Binding>) {
        let se = self.scope_end(scoped);
        self.extract_bindings_se(expr, se, opts, out);
    }

    pub fn extract_bindings_se(&mut self, expr: NodeId, se: (u32, u32), opts: BOpts, out: &mut Vec<Binding>) {
        let orig = expr;
        let (expr, _) = self.lift_meta(expr);
        match self.kind(expr) {
            Kind::Symbol => {
                if opts.keys_kind {
                    self.keyword_usage_opts(expr, KwOpts { keys_destr: opts.keys_destructuring, ns_mod: false, destr: opts.destr });
                }
                let nm = self.c.name(expr);
                let ns = self.c.ns(expr);
                if self.lt.fn_dupes.is_some() {
                    self.lint_fn_dupe(expr, nm);
                }
                if ns.is_none() && nm == syms().amp {
                    if !opts.allow_amp {
                        let p = self.pos(expr);
                        self.lint(lint::FType::Syntax, p, "Invalid binding: &");
                    }
                    return;
                }
                if ns.is_none() || opts.keys_destructuring {
                    let mut b = self.new_local(nm, expr, se);
                    if self.lon {
                        b.tag = self.hint_code(orig).unwrap_or(if opts.keys_destructuring { 0 } else { opts.tag });
                        self.lt.tagged_any |= b.tag != 0;
                        b.nil_lit = opts.nil_lit;
                    }
                    out.push(b);
                } else {
                    let p = self.pos(expr);
                    let s = self.node_str(expr);
                    self.lint(lint::FType::Syntax, p, format!("unsupported binding form {}", s));
                }
            }
            Kind::Keyword => {
                self.keyword_usage_opts(expr, KwOpts { keys_destr: opts.keys_destructuring, ns_mod: false, destr: opts.destr });
                if opts.keys_destructuring {
                    let nm = self.c.name(expr);
                    let b = self.new_local(nm, expr, se);
                    out.push(b);
                } else if self.c.flags(expr) & F_AUTO != 0 || !self.is_kw_named(expr, "as") {
                    let p = self.pos(expr);
                    let s = self.node_str(expr);
                    self.lint(lint::FType::Syntax, p, format!("unsupported binding form {}", s));
                }
            }
            Kind::Vector => {
                let kids = self.kids(expr);
                let all_tokens = kids.iter().all(|&k| matches!(self.kind(k), Kind::Symbol | Kind::Keyword | Kind::String | Kind::Number | Kind::Nil | Kind::True | Kind::False | Kind::Char));
                let child_opts = BOpts { allow_amp: true, ..opts };
                let mark = self.bindings.len();
                let ncs = self.cs.len();
                self.cs.push((SymId::NONE, intern("vector")));
                for &k in &kids {
                    let start = out.len();
                    self.extract_bindings_se(k, se, child_opts, out);
                    if !all_tokens {
                        let newb: Vec<Binding> = out[start..].to_vec();
                        self.bindings.extend(newb);
                    }
                }
                self.bindings.truncate(mark);
                self.cs.truncate(ncs);
            }
            Kind::NsMap => {
                if let Some(m) = self.c.nth(expr, 1) {
                    self.extract_bindings_se(m, se, BOpts { namespaced_map: true, ..opts }, out);
                }
            }
            Kind::Map => self.extract_map_bindings(expr, se, opts, out),
            _ => {
                let p = self.pos(expr);
                let s = self.node_str(expr);
                self.lint(lint::FType::Syntax, p, format!("unsupported binding form {}", s));
            }
        }
    }

    fn plain_directive(&self, k: NodeId, kw: &str, opts: BOpts) -> bool {
        !opts.namespaced_map && self.is_kw_named(k, kw)
    }

    fn extract_map_bindings(&mut self, expr: NodeId, se: (u32, u32), opts: BOpts, out: &mut Vec<Binding>) {
        let kids = self.kids(expr);
        let opts = BOpts { allow_amp: false, namespaced_map: opts.namespaced_map, ..opts };
        let base = out.len();
        let mut or_pair: Option<(NodeId, NodeId)> = None;
        let mut i = 0;
        while i < kids.len() {
            let k0 = kids[i];
            let v = kids.get(i + 1).copied();
            i += 2;
            let (k, _) = self.lift_meta(k0);
            if self.kind(k) == Kind::Keyword {
                let key_name = self.c.name(k).as_str();
                let ns_modifier = matches!(key_name, "keys" | "syms" | "strs" | "keys!" | "syms!" | "strs!" | "flds");
                if ns_modifier {
                    self.keyword_usage_opts(k, KwOpts { keys_destr: opts.keys_destructuring, ns_mod: true, destr: opts.destr });
                    if let Some(v) = v {
                        let vk = self.kids(v);
                        let mut amp = false;
                        for &child in &vk {
                            if self.is_sym_named(child, "&") {
                                amp = true;
                                continue;
                            }
                            if amp {
                                if self.kind(child) == Kind::Keyword {
                                    self.keyword_usage_opts(child, KwOpts { keys_destr: true, ns_mod: false, destr: Some(k) });
                                }
                                continue;
                            }
                            let co = BOpts { keys_destructuring: true, keys_kind: matches!(key_name, "keys" | "keys!"), allow_amp: false, fn_args: opts.fn_args, namespaced_map: false, destr: Some(k), tag: 0, nil_lit: false };
                            self.extract_bindings_se(child, se, co, out);
                        }
                    }
                } else {
                    self.keyword_usage(k);
                    match key_name {
                        "or" if self.plain_directive(k, "or", opts) => {
                            if i < kids.len() {
                                // kondo re-queues a non-final `:or` and analyzes its keyword again
                                self.keyword_usage(k);
                            }
                            if or_pair.is_none() {
                                if let Some(v) = v {
                                    or_pair = Some((k, v));
                                }
                            }
                        }
                        "as" | "all" | "select" | "defaults" if self.plain_directive(k, key_name, opts) => {
                            if let Some(v) = v {
                                self.extract_bindings_se(v, se, BOpts { keys_kind: false, ..opts }, out);
                            }
                        }
                        _ => {}
                    }
                }
            } else {
                // k is a binding form, v its lookup key
                self.extract_bindings_se(k, se, BOpts { keys_kind: false, ..opts }, out);
                if let Some(v) = v {
                    self.analyze_expression(v);
                }
            }
        }
        if let Some((_, v)) = or_pair {
            let newb: Vec<Binding> = out[base..].to_vec();
            let mark = self.bindings.len();
            self.bindings.extend(newb);
            self.analyze_defaults(v, mark);
            self.bindings.truncate(mark);
        }
    }

    /// kondo `analyze-keys-destructuring-defaults`.
    fn analyze_defaults(&mut self, defaults: NodeId, mark: usize) {
        if self.kind(defaults) != Kind::Map {
            self.analyze_expression(defaults);
            return;
        }
        let kv = self.kids(defaults);
        let mut i = 0;
        while i + 1 < kv.len() {
            let (k, v) = (kv[i], kv[i + 1]);
            i += 2;
            if self.kind(k) == Kind::Symbol && self.kind(v) == Kind::Symbol && self.c.name(k) == self.c.name(v) && self.c.ns(k) == self.c.ns(v) {
                // `{:or {x x}}`: the value refers to the outer `x` (analyzed without the new bindings)
                let inner: Vec<Binding> = self.bindings.split_off(mark);
                self.dropped(|a| a.analyze_expression(v));
                self.bindings.extend(inner);
                continue;
            }
            if self.kind(k) == Kind::Symbol && self.c.ns(k).is_none() {
                let p = self.pos(k);
                match self.find_binding(self.c.name(k)) {
                    Some(b) => {
                        let was = self.lint_used(b.id);
                        self.reg_used_binding(b, p, p);
                        self.lint_set_used(b.id, was);
                        self.lint_default(p, self.c.name(k), mark);
                    }
                    None if !self.opts.locals => {}
                    None => self.out.local_usages.push(LocalUsage { id: 0, name: SymId::NONE, pos: p, name_pos: p, lang: self.ltag }),
                }
            }
            // kondo `:undefined-locals`: usages of the map's own bindings inside its defaults are flagged
            let names: Vec<SymId> = if self.lon { self.bindings[mark..].iter().filter(|b| b.id != 0 || b.key != syms().percent).map(|b| b.key).collect() } else { Vec::new() };
            let saved = std::mem::replace(&mut self.lt.undefined_locals, names);
            self.dropped(|a| a.analyze_expression(v));
            self.lt.undefined_locals = saved;
        }
    }
}
