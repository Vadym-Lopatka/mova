//! Buckets beyond the six core ones, registered exactly where kondo registers them:
//! - keywords: kondo `usages/analyze-keyword` + `analysis/reg-keyword-usage!` (`keyword_usage*`)
//! - symbols: `analysis/reg-symbol!` for quoted qualified symbols (`quoted_symbol`)
//! - protocol-impls: `analysis/reg-protocol-impl!` (`protocol_impl_hook`)
//! - instance-invocations: `analysis/reg-instance-invocation!` (`instance_invocation`)
//! - java-class-usages: `java/reg-class-usage!` (`java_*`, see `extras_java.rs`); definitions in `java.rs`.
use super::*;

/// Options of kondo `analyze-keyword` (`:keys-destructuring?`, `:keys-destructuring-ns-modifier?`, `:destructuring-expr`).
#[derive(Clone, Copy, Default)]
pub(crate) struct KwOpts {
    pub keys_destr: bool,
    pub ns_mod: bool,
    pub destr: Option<NodeId>,
}

/// Result of kondo `resolve-keyword`.
struct KwRes {
    name: SymId,
    ns: SymId,
    alias: SymId,
    from_prefix: bool,
}

/// "No expression" marker for `resolve_name` (kondo passes nil).
pub(crate) const NO_EXPR: NodeId = NodeId(u32::MAX);

impl<'a> Analyzer<'a> {
    /// kondo `usages/resolve-keyword`.
    fn resolve_keyword(&self, n: NodeId) -> KwRes {
        let aliased = self.c.kind(n) == Kind::Keyword && self.c.flags(n) & F_AUTO != 0;
        let alias_or_ns = self.c.ns(n);
        let prefix = self.ex.kw_prefix.get(&n).copied().unwrap_or(SymId::NONE);
        let unknown = syms().unknown_ns;
        let ns = if aliased && !alias_or_ns.is_none() {
            self.cur_ns().aliases.get(&alias_or_ns).copied().unwrap_or(unknown)
        } else if aliased {
            if self.ex.edn {
                SymId::NONE
            } else {
                self.cur_ns_name()
            }
        } else if !prefix.is_none() && alias_or_ns.as_str() == "_" {
            SymId::NONE
        } else if !prefix.is_none() && alias_or_ns.is_none() {
            prefix
        } else {
            alias_or_ns
        };
        let alias = if aliased && ns != unknown { alias_or_ns } else { SymId::NONE };
        KwRes { name: self.c.name(n), ns, alias, from_prefix: !prefix.is_none() && alias_or_ns.is_none() && !aliased }
    }

    /// kondo `usages/analyze-keyword`.
    pub(crate) fn analyze_keyword(&mut self, n: NodeId, o: KwOpts) {
        let resolved = self.resolve_keyword(n);
        let destructuring = o.destr.map(|d| self.resolve_keyword(d));
        let (d_ns, d_alias) = destructuring.as_ref().map_or((SymId::NONE, SymId::NONE), |d| (d.ns, d.alias));
        let aliased = self.c.kind(n) == Kind::Keyword && self.c.flags(n) & F_AUTO != 0;
        let mut flags = 0u8;
        if aliased {
            flags |= KW_AUTO;
        }
        if resolved.from_prefix {
            flags |= KW_PREFIX;
        }
        if o.keys_destr {
            flags |= KW_KEYS_DESTR;
        }
        if o.ns_mod {
            flags |= KW_NS_MOD;
        }
        let edn = self.ex.edn;
        let kw = Keyword {
            pos: self.pos(n),
            name: resolved.name,
            from: if edn { SymId::NONE } else { self.cur_ns_name() },
            from_var: self.ctx.in_def,
            ns: if d_ns.is_none() { resolved.ns } else { d_ns },
            alias: if d_alias.is_none() { resolved.alias } else { SymId::NONE },
            reg: self.ex.kw_reg.get(&n).copied().unwrap_or(SymId::NONE),
            lang: self.ltag,
            flags,
        };
        self.out.keywords.push(kw);
        if aliased && !edn {
            self.lint_keyword_ns(n);
        }
        if aliased && !self.c.ns(n).is_none() && !edn {
            // `::alias/foo`: the alias is a used namespace (or unresolved)
            let r = self.resolve_name(false, (self.c.ns(n), self.c.name(n)), NO_EXPR);
            if r.found && !r.unresolved && !r.ns.is_none() {
                self.note_used(r.ns);
            }
        }
    }

    /// A keyword token was analyzed.
    pub fn keyword_usage(&mut self, n: NodeId) {
        self.analyze_keyword(n, KwOpts::default());
    }

    /// A keyword / symbol in destructuring with kondo's `opts`.
    pub(crate) fn keyword_usage_opts(&mut self, n: NodeId, o: KwOpts) {
        self.analyze_keyword(n, o);
    }

    /// A qualified symbol inside a quoted form (kondo `analyze-expression**` token branch).
    pub fn quoted_symbol(&mut self, n: NodeId) {
        if self.opts.external || self.c.kind(n) != Kind::Symbol || self.c.ns(n).is_none() {
            return;
        }
        let (ns, name) = (self.c.ns(n), self.c.name(n));
        let symbol = intern(&format!("{}/{}", ns.as_str(), name.as_str()));
        let edn = self.ex.edn;
        let (mut to, mut nm) = (SymId::NONE, name);
        if !edn {
            let r = self.resolve_name(false, (ns, name), n);
            if !(r.unresolved || r.interop) && r.found {
                to = r.ns;
                nm = r.name;
            }
        }
        let lang = if edn {
            3
        } else if self.is_cljs() {
            2
        } else {
            1
        };
        let from = if edn { SymId::NONE } else { self.cur_ns_name() };
        self.out.symbols.push(SymbolUse { pos: self.pos(n), name: nm, symbol, to, from, lang });
    }

    /// EDN files: every token of the tree is analyzed (kondo `analyze-expression**` with `lang :edn`).
    pub(crate) fn analyze_edn(&mut self, n: NodeId) {
        match self.kind(n) {
            Kind::Keyword => self.keyword_usage(n),
            Kind::Symbol => self.quoted_symbol(n),
            Kind::Tagged => {
                for c in self.kids(n).into_iter().skip(1) {
                    self.analyze_edn(c);
                }
            }
            k if Cst::is_container(k) => {
                for c in self.kids(n) {
                    self.analyze_edn(c);
                }
            }
            _ => {}
        }
    }

    /// kondo `analyze-namespaced-map`: the resolved prefix is attached to the keys of the inner map.
    pub(crate) fn note_nsmap_keys(&mut self, m: NodeId, prefix: SymId) {
        let m = self.c.unwrap_meta(m);
        let kids = self.kids(m);
        let mut i = 0;
        while i < kids.len() {
            self.ex.kw_prefix.insert(kids[i], prefix);
            i += 2;
        }
    }

    /// `^Tag (...)`: the list keeps the tag as metadata (leaks into java-class-usages).
    pub(crate) fn note_list_meta(&mut self, orig: NodeId, target: NodeId) {
        if self.kind(target) == Kind::List {
            if let Some((m, _)) = self.c.meta(orig) {
                if self.kind(m) == Kind::Symbol && self.c.ns(m).is_none() {
                    self.ex.list_tag.insert(target, self.c.name(m));
                }
            }
        }
    }

    /// `s/def` name keyword gets `:reg` (kondo `spec/analyze-def`).
    pub(crate) fn note_reg(&mut self, n: NodeId, reg: SymId) {
        if self.c.kind(n) == Kind::Keyword {
            self.ex.kw_reg.insert(n, reg);
        }
    }

    /// kondo `reg-protocol-impl!`; `method` is the impl list `(foo [this] ...)`, `pns`/`pname` the resolved protocol.
    pub(crate) fn protocol_impl_hook(&mut self, method: NodeId, by: forms::DefBy, pns: SymId, pname: SymId) {
        let first = self.c.children(method).first().copied();
        let (name_pos, mname) = match first {
            Some(f) => {
                let f = self.c.unwrap_meta(f);
                let nm = if self.kind(f) == Kind::Symbol {
                    if self.c.ns(f).is_none() {
                        self.c.name(f)
                    } else {
                        intern(&format!("{}/{}", self.c.ns(f).as_str(), self.c.name(f).as_str()))
                    }
                } else {
                    SymId::NONE
                };
                (self.pos(f), nm)
            }
            None => (Pos { row: 0, col: 0, end_row: 0, end_col: 0 }, SymId::NONE),
        };
        self.out.protocol_impls.push(ProtocolImpl {
            pos: self.pos(method),
            name_pos,
            method_name: mname,
            protocol_name: pname,
            protocol_ns: pns,
            impl_ns: self.cur_ns_name(),
            defined_by: by.by,
            defined_by_lint_as: by.lint_as,
            derived: self.c.flags(method) & F_DERIVED != 0,
        });
    }

    /// `(.method obj)` / `(. obj method)`; `method` is the method name node.
    pub fn instance_invocation(&mut self, method: NodeId) {
        if self.opts.external {
            return;
        }
        let name = match self.kind(method) {
            Kind::Symbol => {
                if self.c.ns(method).is_none() {
                    self.c.name(method)
                } else {
                    intern(&format!("{}/{}", self.c.ns(method).as_str(), self.c.name(method).as_str()))
                }
            }
            _ => intern(&self.node_str(method)),
        };
        self.out.instance_invocations.push(InstanceInvocation { name_pos: self.pos(method), method_name: name, derived: self.c.flags(method) & F_DERIVED != 0, lang: self.ltag });
    }
}

/// Second pass after `DefsIndex::add_file*` (kondo shares namespace state between files): a protocol that did not
/// resolve but is defined in another file of the impl namespace (`in-ns` continuations) belongs to that namespace.
pub fn finish_extras(fa: &mut FileAnalysis, defs: &DefsIndex) {
    let unknown = syms().unknown_ns;
    let src = match fa.base_lang.unwrap_or(BaseLang::Clj) {
        BaseLang::Clj => Src::Clj,
        BaseLang::Cljs => Src::Cljs,
        BaseLang::Cljc => Src::CljcClj,
    };
    for p in fa.protocol_impls.iter_mut() {
        if p.protocol_ns == unknown && !p.protocol_name.is_none() && defs.get(src, p.impl_ns, p.protocol_name).is_some() {
            p.protocol_ns = p.impl_ns;
        }
    }
}
