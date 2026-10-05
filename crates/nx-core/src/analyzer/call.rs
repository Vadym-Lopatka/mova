//! `analyze-call` and `analyze-known-call` dispatch.
use super::expr::*;
use super::forms::*;
use super::resolve::Resolved;
use super::*;

fn is_core(ns: SymId) -> bool {
    ns == syms().clojure_core || ns == syms().cljs_core
}

impl<'a> Analyzer<'a> {
    /// kondo `analyze-call`. `full` is the (possibly normalized) head symbol.
    pub fn analyze_call(&mut self, expr: NodeId, head: NodeId, full: Name, arg_count: u32) -> Ret {
        let s = syms();
        let fs = full.1.as_str();
        let simple = full.0.is_none();
        let is_dot = simple && (fs == "." || fs == "..");
        if !is_dot && simple {
            if fs.ends_with('.') && fs.len() > 1 {
                let cur = self.cur_ns();
                if !cur.referred.contains_key(&full.1) && !cur.vars.contains(&full.1) {
                    let e2 = self.expand_dot_constructor(expr);
                    let h2 = self.c.nth(e2, 0).unwrap();
                    return self.analyze_call(e2, h2, (SymId::NONE, s.new_), arg_count + 1);
                }
            }
            if fs.starts_with('.') && fs.len() > 1 {
                let e2 = self.expand_method_invocation(expr);
                let h2 = self.c.nth(e2, 0).unwrap();
                return self.analyze_call(e2, h2, (SymId::NONE, s.dot), arg_count + 1);
            }
        }
        let children = self.kids(expr);
        let args: Vec<NodeId> = children[1..].to_vec();
        let from_ns = self.cur_ns_name();
        let r = self.resolve_name(true, full, expr);
        if !r.unresolved_ns.is_none() {
            let fname = intern(full.1.as_str());
            if !self.c.is_gen(expr) {
                let p = self.pos(head);
                self.lint_unresolved_ns(r.unresolved_ns, fname, p);
            }
            self.scope(|a| {
                a.cs.push((s.unknown_ns, fname));
                a.analyze_children(&args);
            });
            return None;
        }
        // :analyze-call hook
        if r.found {
            if let Some(h) = self.cfg.hook(r.ns, r.name) {
                if let Some(new) = self.expand_hook(h, expr) {
                    let pos = self.pos(expr);
                    let name_pos = self.pos(head);
                    let name = r.name;
                    let derived = self.c.flags(expr) & F_DERIVED != 0;
                    let derived_name = self.c.flags(head) & F_DERIVED != 0;
                    // the original call is registered (without from-var) since the expansion is another call
                    self.reg_var_usage(VarUsageArgs { name, pos, name_pos, arity: arg_count.min(0xfffe) as u16, r, refer: false, defmethod: false, dispatch_val: SymId::NONE, testing: SymId::NONE, derived, derived_name, no_from_var: true, from: from_ns, written: full });
                    if !self.ctx.dropped {
                        self.note_used(r.ns);
                    }
                    self.lint_use_ns(r.ns);
                    return self.analyze_expression(new);
                }
            }
        }
        // lint-as
        let (as_ns, as_name, lint_as) = match if r.found { self.cfg.lint_as(r.ns, r.name) } else { None } {
            Some((n, m)) => (n, m, true),
            // Mova: core.async forms are part of the language (bare `go-loop`, `go`, `thread`, `alt!`)
            None if self.opts.mova && r.unresolved && r.unresolved_ns.is_none() && r.name.as_str() == "go-loop" => (s.clojure_core, intern("loop"), true),
            None if self.opts.mova && r.unresolved && r.unresolved_ns.is_none() && matches!(r.name.as_str(), "go" | "thread" | "alt!" | "alt!!") => (intern("clojure.core.async"), r.name, false),
            None => (r.ns, r.name, false),
        };
        // clojure.test/testing context (kondo test/testing-hook)
        let mut testing = SymId::NONE;
        if r.found && r.name.as_str() == "testing" && matches!(r.ns.as_str(), "clojure.test" | "cljs.test") {
            if let Some(&t) = args.first() {
                testing = intern(&self.sexpr_json(t));
            }
        }
        let unknown = r.ns == s.unknown_ns;
        let ns_star = if unknown { self.cur_ns_name() } else { r.ns };
        let top_level = self.ctx.top_level;
        self.lint_call_post(expr, &r, arg_count);
        let saved = self.ctx;
        let ncs = self.cs.len();
        let nb = self.bindings.len();
        if r.found && !(r.ns == s.clojure_core && r.name.as_str() == "doto") {
            self.cs.push((ns_star, r.name));
        } else {
            self.cs.push((SymId::NONE, SymId::NONE));
        }
        self.lt.written_head = Some(full);
        self.lt.parent_gen = self.ctx.gen_call;
        self.ctx.gen_call = self.lint_is_gen(expr);
        let saved_cp = self.lt.call_pos;
        if self.lon {
            let p = self.pos(expr);
            self.lt.call_pos = (p.row, p.col);
        }
        let as_core_name = if is_core(as_ns) { as_name } else { SymId::NONE };
        if !as_core_name.is_none() {
            self.ctx.resolved_as_core = as_core_name;
        }
        let by = DefBy {
            by: if r.found { (r.ns, r.name) } else { (SymId::NONE, SymId::NONE) },
            lint_as: (as_ns, as_name),
        };
        let ret = self.analyze_known_call(expr, &args, &r, (as_ns, as_name), as_core_name, lint_as, top_level, by);
        let skip = self.c.flags(expr) & F_SKIP != 0 && self.kind(expr) == Kind::List;
        let is_ns_form = as_core_name.as_str() == "ns";
        if !saved.dropped && !is_ns_form && !r.interop && r.found {
            self.note_used(r.ns);
        }
        if !is_ns_form && r.found && !r.unresolved {
            self.lint_use_ns(r.ns);
        }
        // usage of the called var (registered even for unresolved names)
        if !is_ns_form && !r.interop && !skip {
            let name = if r.found && !r.name.is_none() {
                r.name
            } else if full.0.is_none() {
                full.1
            } else {
                intern(&format!("{}/{}", full.0.as_str(), full.1.as_str()))
            };
            let rr = if r.found { r } else { Resolved::none() };
            let pos = self.pos(expr);
            let name_pos = self.pos(head);
            let derived = self.c.flags(expr) & F_DERIVED != 0 && self.kind(expr) == Kind::List;
            let derived_name = self.c.flags(head) & F_DERIVED != 0;
            self.ctx = saved;
            self.lt.call_pos = saved_cp;
            self.cs.truncate(ncs);
            self.bindings.truncate(nb);
            if self.lon && r.found && !r.unresolved {
                self.lint_arg_types(expr, head, arg_count);
                self.lint_arg_extras(expr, &r, arg_count);
            }
            self.reg_var_usage(VarUsageArgs { name, pos, name_pos, arity: arg_count.min(0xfffe) as u16, r: rr, refer: false, defmethod: false, dispatch_val: SymId::NONE, testing, derived, derived_name, no_from_var: false, from: from_ns, written: full });
        } else {
            self.ctx = saved;
            self.lt.call_pos = saved_cp;
            self.cs.truncate(ncs);
            self.bindings.truncate(nb);
        }
        ret
    }

    #[allow(clippy::too_many_arguments)]
    fn analyze_known_call(&mut self, expr: NodeId, args: &[NodeId], r: &Resolved, as_: Name, core_name: SymId, lint_as: bool, top_level: bool, by: DefBy) -> Ret {
        if self.lon {
            let rc = r.found && is_core(r.ns);
            self.lint_core_call(expr, args, rc, r.name.as_str(), core_name.as_str(), lint_as);
        }
        if !core_name.is_none() {
            let name = core_name.as_str();
            match name {
                "assoc" | "assoc!" | "sorted-map-by" | "struct-map" | "dissoc" | "dissoc!" | "disj" | "disj!" | "sorted-set-by" | "array-map" | "hash-map" | "sorted-map" | "hash-set" | "sorted-set" | "create-struct" => {
                    self.analyze_children(args);
                    None
                }
                "ns" => {
                    if top_level {
                        self.analyze_ns_decl(expr);
                    }
                    None
                }
                "in-ns" => {
                    if top_level {
                        self.analyze_in_ns(expr);
                    } else {
                        self.analyze_children(args);
                    }
                    None
                }
                "alias" => {
                    self.analyze_alias(expr);
                    None
                }
                "declare" => {
                    self.analyze_declare(expr, by);
                    None
                }
                "def" | "defonce" | "defmulti" | "goog-define" => {
                    self.lint_inline_def(expr);
                    self.analyze_def(expr, by)
                }
                "defn" | "defn-" | "defmacro" | "definline" => {
                    self.lint_inline_def(expr);
                    self.analyze_defn(expr, by, false)
                }
                "defmethod" => {
                    self.analyze_defmethod(expr);
                    None
                }
                "definterface" | "defprotocol" => {
                    self.analyze_defprotocol(expr, by);
                    None
                }
                "defrecord" | "deftype" => {
                    self.analyze_defrecord(expr, by);
                    None
                }
                "defstruct" => {
                    if let Some(&sn) = args.first() {
                        let sn = self.c.unwrap_meta(sn);
                        if self.kind(sn) == Kind::Symbol {
                            let nm = self.c.name(sn);
                            let none = (SymId::NONE, SymId::NONE);
                            self.reg_var(nm, expr, VarMeta::new(Pos { row: 0, col: 0, end_row: 0, end_col: 0 }, DefBy { by: none, lint_as: none }));
                        }
                    }
                    self.analyze_children(&args[1.min(args.len())..]);
                    None
                }
                "comment" => {
                    self.ctx.in_comment = true;
                    self.analyze_children(args);
                    None
                }
                "->" | "->>" => {
                    if let Some(e) = self.expand_thread(expr, name == "->>") {
                        self.analyze_expression(e);
                    }
                    None
                }
                "some->" | "some->>" => {
                    if let Some(e) = self.expand_some_arrow(expr, name == "some->>") {
                        self.analyze_expression(e);
                    }
                    None
                }
                "cond->" | "cond->>" => {
                    if let Some(e) = self.expand_cond_arrow(expr, name == "cond->>") {
                        self.analyze_expression(e);
                    }
                    None
                }
                "doto" => {
                    if let Some(e) = self.expand_doto(expr) {
                        self.analyze_expression(e);
                    }
                    None
                }
                ".." => {
                    if let Some(e) = self.expand_double_dot(expr) {
                        self.analyze_expression(e);
                    }
                    None
                }
                "." => {
                    self.analyze_instance_invocation(args);
                    None
                }
                "reify" | "extend-protocol" | "extend-type" => {
                    self.analyze_protocol_impls(by, args);
                    None
                }
                "specify!" => {
                    if let Some(&f) = args.first() {
                        self.analyze_expression(f);
                    }
                    self.analyze_protocol_impls(by, args);
                    None
                }
                "proxy-super" => {
                    // kondo `analyze-proxy-super`: the `this` binding counts as used (no local-usage)
                    if let Some(b) = self.find_binding(intern("this")) {
                        self.lint_use_binding(b.id);
                    }
                    self.analyze_children(&args[1.min(args.len())..]);
                    None
                }
                "amap" => {
                    if args.len() >= 4 {
                        let mut out = Vec::new();
                        self.extract_bindings(args[1], expr, bindings::BOpts::default(), &mut out);
                        self.extract_bindings(args[2], expr, bindings::BOpts::default(), &mut out);
                        self.scope(|a| {
                            a.bindings.extend(out);
                            a.analyze_children(&[args[0], args[3]]);
                        });
                    }
                    None
                }
                "proxy" | "defcurried" => {
                    self.scope(|a| {
                        a.ctx.off |= lint::uses::OFF_SYM | lint::uses::OFF_ARITY | lint::uses::OFF_TYPE;
                        a.analyze_children(args);
                    });
                    None
                }
                "gen-interface" => {
                    self.scope(|a| {
                        a.ctx.off |= lint::uses::OFF_SYM;
                        a.analyze_children(args);
                    });
                    None
                }
                "loop" => {
                    self.analyze_loop(expr);
                    None
                }
                "let" | "let*" | "for" | "doseq" | "dotimes" | "with-open" | "with-local-vars" => {
                    self.analyze_like_let_bindings_form(expr, name);
                    None
                }
                "letfn" => {
                    self.analyze_letfn(expr);
                    None
                }
                "if-let" | "if-some" | "when-let" | "when-some" | "when-first" => {
                    self.analyze_conditional_let(name, expr);
                    None
                }
                "fn" | "fn*" | "bound-fn" => self.analyze_fn(expr),
                "case" => {
                    self.analyze_case(expr);
                    None
                }
                "recur" | "do" | "if" | "if-not" | "new" | "when" | "when-not" | "cond" | "and" | "or" | "condp" | "=" | "not=" | "+" | "-" | "set!" | "format" | "printf" | "rest" | "await" | "memfn" | "locking" | "with-precision" => {
                    self.analyze_new_if_etc(expr, name, args);
                    None
                }
                "with-redefs" | "binding" => {
                    self.analyze_with_redefs(args);
                    None
                }
                "quote" => None,
                "try" => {
                    self.analyze_try(expr);
                    None
                }
                "as->" => {
                    self.analyze_as_arrow(expr);
                    None
                }
                "areduce" => {
                    self.analyze_areduce(expr);
                    None
                }
                "this-as" => {
                    self.analyze_this_as(expr);
                    None
                }
                "use" | "require" => {
                    if top_level {
                        self.analyze_require(expr);
                    } else {
                        self.analyze_children(args);
                    }
                    None
                }
                "import" => {
                    if top_level {
                        self.analyze_import(expr);
                    } else {
                        self.analyze_children(args);
                    }
                    None
                }
                "map" | "mapv" | "filter" | "filterv" | "remove" | "reduce" | "every?" | "not-every?" | "some" | "not-any?" | "mapcat" | "iterate" | "max-key" | "min-key" | "group-by" | "partition-by" | "map-indexed" | "keep" | "keep-indexed" | "update" | "update-in" | "swap!" | "swap-vals!" | "send" | "send-off" | "send-via" => {
                    self.analyze_hof(expr, args, name, r.ns, r.name);
                    None
                }
                "ns-unmap" => {
                    self.analyze_ns_unmap(args);
                    None
                }
                "gen-class" => None,
                "exists?" => {
                    self.scope(|b| {
                        b.ctx.off |= lint::uses::OFF_SYM | lint::uses::OFF_NS;
                        for &a in args {
                            b.analyze_usages2(a, false, false);
                        }
                    });
                    None
                }
                "var" => {
                    let c = self.ctx;
                    self.ctx.private_access = true;
                    self.analyze_children(args);
                    self.ctx = c;
                    None
                }
                _ => {
                    self.analyze_unknown_ns_call(args, r);
                    None
                }
            }
        } else {
            self.analyze_known_ns_call(expr, args, r, as_, by)
        }
    }

    fn analyze_like_let_bindings_form(&mut self, expr: NodeId, _name: &str) {
        self.analyze_like_let(expr);
    }

    /// Forms that only analyze their arguments (conditions, branches, constructor, ...).
    fn analyze_new_if_etc(&mut self, expr: NodeId, name: &str, args: &[NodeId]) {
        match name {
            "new" => {
                if let Some(&c) = args.first() {
                    let saved = self.ex.ctor.replace(expr);
                    self.dropped(|a| a.analyze_expression(c));
                    self.ex.ctor = saved;
                }
                self.analyze_children(&args[1.min(args.len())..]);
            }
            "if" | "if-not" | "when" | "when-not" => {
                // the condition's analysis result is discarded by kondo
                if let Some(&c) = args.first() {
                    if !self.lt.lint_as_call && !self.lint_is_gen(expr) {
                        self.lint_condition(c, false);
                    }
                    self.analyze_condition(c);
                    self.analyze_children(&args[1..]);
                }
            }
            "set!" if self.is_cljs() && args.len() == 3 => {
                let v = vec![args[0], args[2]];
                self.analyze_children(&v);
            }
            "recur" => {
                self.lint_recur(expr, args.len() as u32);
                self.analyze_children(args);
            }
            "format" | "printf" => {
                self.lint_format(args);
                self.analyze_children(args);
            }
            "locking" => {
                self.analyze_children(args);
                self.lint_locking(args);
            }
            "memfn" => self.scope(|a| {
                a.ctx.off |= lint::uses::OFF_SYM;
                a.analyze_children(args);
            }),
            _ => self.analyze_children(args),
        }
    }

    fn analyze_with_redefs(&mut self, args: &[NodeId]) {
        let Some(&bv) = args.first() else { return };
        if self.kind(bv) == Kind::Vector {
            let kv = self.kids(bv);
            self.scope(|a| {
                a.cs.push((SymId::NONE, intern("vector")));
                let lhs: Vec<NodeId> = kv.iter().step_by(2).copied().collect();
                let rhs: Vec<NodeId> = kv.iter().skip(1).step_by(2).copied().collect();
                a.dropped(|a| {
                    a.scope(|b| {
                        b.ctx.off |= lint::uses::OFF_PRIV;
                        b.analyze_children(&lhs);
                    });
                    a.analyze_children(&rhs);
                });
            });
        }
        self.analyze_children(&args[1..]);
    }

    fn analyze_ns_unmap(&mut self, args: &[NodeId]) {
        if args.len() >= 2 && self.is_sym_named(args[0], "*ns*") && self.kind(args[1]) == Kind::Quote {
            if let Some(s) = self.c.nth(args[1], 0) {
                if self.kind(s) == Kind::Symbol && self.c.ns(s).is_none() {
                    let nm = self.c.name(s);
                    let cur = self.cur_ns_mut();
                    cur.clojure_excluded.insert(nm);
                    cur.vars.remove(&nm);
                }
            }
        }
        self.analyze_children(args);
    }

    fn analyze_instance_invocation(&mut self, args: &[NodeId]) {
        let instance = args.first().copied();
        let meth = args.get(1).copied();
        if let Some(i) = instance {
            self.dropped(|a| a.analyze_expression(i));
        }
        if let Some(m) = meth {
            if self.kind(m) == Kind::List && args.len() == 2 {
                let mk = self.kids(m);
                if let Some(&mn) = mk.first() {
                    self.instance_invocation(mn);
                }
                self.dropped(|a| a.analyze_children(&mk[1.min(mk.len())..]));
            } else {
                self.instance_invocation(m);
            }
        }
        if args.len() > 2 {
            self.analyze_children(&args[2..]);
        }
    }

    /// kondo `analyze-hof`.
    fn analyze_hof(&mut self, expr: NodeId, args: &[NodeId], as_name: &str, hof_ns: SymId, hof_name: SymId) {
        let head = self.lt.written_head.unwrap_or((SymId::NONE, SymId::NONE));
        let core_ns = is_core(hof_ns);
        let hn = hof_name.as_str();
        let (prepending_n, f_pos, f_args_n) = if core_ns && matches!(hn, "update" | "update-in" | "send-via") {
            (2, 2, 3)
        } else if core_ns && matches!(hn, "swap!" | "swap-vals!" | "send" | "send-off") {
            (1, 1, 2)
        } else {
            (0, 0, 1)
        };
        let prepending: Vec<NodeId> = args.iter().take(prepending_n).copied().collect();
        let f = args.get(f_pos).copied();
        let f_args: Vec<NodeId> = args.iter().skip(f_args_n).copied().collect();
        self.dropped(|a| a.analyze_children(&prepending));
        let Some(f) = f else { return };
        let fret = self.analyze_expression(f);
        let fsym = if self.kind(f) == Kind::Symbol { Some((self.c.ns(f), self.c.name(f))) } else { None };
        let binding = match fsym {
            Some((ns, nm)) if ns.is_none() => self.find_binding(nm),
            _ => None,
        };
        let var = fsym.is_some() && binding.is_none();
        let mut rr = Resolved::none();
        if var {
            rr = self.resolve_name(true, fsym.unwrap(), NodeId(u32::MAX));
        }
        let nf = f_args.len() as i64;
        let mut arg_count: Option<i64> = Some(match as_name {
            "map" | "mapv" | "mapcat" => nf,
            "update" | "update-in" | "send" | "send-off" | "send-via" | "swap!" | "swap-vals!" => nf + 1,
            "reduce" | "map-indexed" | "keep-indexed" => 2,
            _ => 1,
        });
        let eligible = matches!(as_name, "map" | "filter" | "remove" | "mapcat" | "map-indexed" | "keep" | "keep-indexed");
        if eligible && arg_count == Some(0) {
            arg_count = if core_ns && (hn == "map" || hn == "mapcat") { None } else { Some(1) };
        }
        if var && rr.found && !rr.interop && rr.unresolved_ns.is_none() {
            let (fs_ns, fs_nm) = fsym.unwrap();
            let name = if !rr.name.is_none() {
                rr.name
            } else if fs_ns.is_none() {
                fs_nm
            } else {
                intern(&format!("{}/{}", fs_ns.as_str(), fs_nm.as_str()))
            };
            let p = self.pos(f);
            self.lt.hof_head = Some(head);
            self.reg_var_usage(VarUsageArgs { name, pos: p, name_pos: Pos { row: 0, col: 0, end_row: 0, end_col: 0 }, arity: arg_count.map_or(NO_ARITY, |a| a as u16), r: rr, refer: false, defmethod: false, dispatch_val: SymId::NONE, testing: SymId::NONE, derived: false, derived_name: false, no_from_var: false, from: SymId::NONE, written: (fs_ns, fs_nm) });
        } else if var && !rr.found {
            // nil resolution (cljs js/...): usage without target
            let (fs_ns, fs_nm) = fsym.unwrap();
            let name = if fs_ns.is_none() { fs_nm } else { intern(&format!("{}/{}", fs_ns.as_str(), fs_nm.as_str())) };
            let p = self.pos(f);
            self.reg_var_usage(VarUsageArgs { name, pos: p, name_pos: Pos { row: 0, col: 0, end_row: 0, end_col: 0 }, arity: arg_count.map_or(NO_ARITY, |a| a as u16), r: Resolved::none(), refer: false, defmethod: false, dispatch_val: SymId::NONE, testing: SymId::NONE, derived: false, derived_name: false, no_from_var: false, from: SymId::NONE, written: (fs_ns, fs_nm) });
        }
        if !var && self.lon {
            let info = match binding {
                Some(b) if b.ar != 0 => Some(self.lt.arities[(b.ar - 1) as usize]),
                Some(_) => None,
                None => fret.as_ref().map(|r| lint::ArInfo { fixed: r.fixed, varargs_min: r.varargs_min }),
            };
            if let (Some(info), Some(n)) = (info, arg_count) {
                let fname = match fsym {
                    Some((_, nm)) => nm.as_str().to_owned(),
                    None if self.kind(f) == Kind::AnonFn => "fn*".to_owned(),
                    None => "fn".to_owned(),
                };
                self.lint_hof_arity(f, &fname, info, n as usize);
            } else if let Some(n) = arg_count {
                let (label, ok) = match self.kind(self.c.unwrap_meta(f)) {
                    Kind::Map => ("Map", n == 1 || n == 2),
                    Kind::Set => ("Set", n == 1),
                    Kind::Vector => ("Vector", n == 1),
                    _ => ("", true),
                };
                if !ok && !self.skip_arity_pub() {
                    let p = self.pos(self.c.unwrap_meta(f));
                    let exp = if label == "Map" { "1 or 2" } else { "1" };
                    self.lint(lint::FType::InvalidArity, p, format!("{} is called with {} {} but expects {}", label, n, if n == 1 { "arg" } else { "args" }, exp));
                }
            }
        }
        // reduce-without-init (kondo analyze-hof tail)
        if self.lon && hn == "reduce" && core_ns && args.len() == 2 && self.lc().level(lint::FType::ReduceWithoutInit) != lint::OFF {
            let (rn, rname) = if var && rr.found { (rr.ns, rr.name) } else { (SymId::NONE, SymId::NONE) };
            let plus_times = matches!((rn.as_str(), rname.as_str()), ("clojure.core" | "cljs.core", "+" | "*"));
            let excluded = self.lc().linter_cfg(lint::FType::ReduceWithoutInit).map_or(false, |c| !c.exclude.is_empty() && c.excl_vars.contains(&(rn, rname)));
            if !plus_times && !excluded {
                let p = self.pos(expr);
                self.lint(lint::FType::ReduceWithoutInit, p, "Reduce called without explicit initial value.".to_owned());
            }
        }
        self.scope(|a| {
            a.cs.push((if rr.found { rr.ns } else { SymId::NONE }, if rr.found { rr.name } else { SymId::NONE }));
            a.analyze_children(&f_args);
        });
    }

    fn analyze_unknown_ns_call(&mut self, args: &[NodeId], r: &Resolved) {
        // kondo `analyze-unknown-ns-call`: bodies of thread/dosync/future/lazy-seq/lazy-cat get a fresh recur context
        let reset = r.found
            && match (r.ns.as_str(), r.name.as_str()) {
                ("clojure.core.async", "thread") => true,
                ("clojure.core", "dosync" | "future" | "lazy-seq" | "lazy-cat") => true,
                _ => false,
            };
        if reset {
            let seen = self.lint_new_seen();
            self.scope(|a| {
                a.ctx.recur = 0;
                a.ctx.seen = seen;
                a.ctx.protocol_fn = false;
                a.analyze_children(args);
            });
        } else {
            self.analyze_children(args);
        }
    }

    /// kondo `analyze-known-ns-call` for non-core namespaces.
    fn analyze_known_ns_call(&mut self, expr: NodeId, args: &[NodeId], r: &Resolved, as_: Name, by: DefBy) -> Ret {
        let key = (as_.0.as_str(), as_.1.as_str());
        match key {
            ("clojure.test", "deftest") | ("clojure.test", "deftest-") | ("cljs.test", "deftest") => {
                let kids = self.kids(expr);
                self.lint_inline_def(expr);
                let defn_t = self.c.push_token(Kind::Symbol, None, intern("defn"), intern("clojure.core"), 0);
                let vec = self.c.push_container(Kind::Vector, None, &[], 0);
                let mut v = vec![defn_t];
                if let Some(&n) = kids.get(1) {
                    v.push(n);
                }
                v.push(vec);
                v.extend_from_slice(&kids[2.min(kids.len())..]);
                let e2 = self.c.push_container(Kind::List, Some(expr), &v, 0);
                self.analyze_defn(e2, by, true)
            }
            ("clojure.test.check.clojure-test", "defspec") => {
                let kids = self.kids(expr);
                self.lint_inline_def(expr);
                let defn_t = self.c.push_token(Kind::Symbol, None, intern("defn"), intern("clojure.core"), 0);
                let amp = self.c.push_token(Kind::Symbol, None, syms().amp, SymId::NONE, 0);
                let a2 = self.c.push_token(Kind::Symbol, None, intern("_args"), SymId::NONE, 0);
                let vec = self.c.push_container(Kind::Vector, None, &[amp, a2], 0);
                let mut v = vec![defn_t];
                if let Some(&n) = kids.get(1) {
                    v.push(n);
                }
                v.push(vec);
                v.extend_from_slice(&kids[2.min(kids.len())..]);
                let e2 = self.c.push_container(Kind::List, Some(expr), &v, 0);
                self.analyze_defn(e2, by, true)
            }
            ("clojure.test", "are") | ("cljs.test", "are") => {
                if let Some(e) = self.expand_are(expr, r.ns) {
                    let saved = std::mem::replace(&mut self.ex.gen, true);
                    self.analyze_expression(e);
                    self.ex.gen = saved;
                }
                None
            }
            ("cljs.test", "async") => {
                let kids = self.kids(expr);
                if let Some(&b) = kids.get(1) {
                    let mut out = Vec::new();
                    let _ = b;
                    if self.kind(b) == Kind::Symbol {
                        out.push(Binding { key: self.c.name(b), name: SymId::NONE, id: 0, gen: false, mark_used: false, ar: 0, tag: 0, nil_lit: false });
                    }
                    self.scope(|a| {
                        a.bindings.extend(out);
                        a.analyze_children(&kids[2..]);
                    });
                }
                None
            }
            ("clojure.test", "is") | ("cljs.test", "is") => {
                if let Some(&c) = args.first() {
                    if !self.lt.lint_as_call && !self.lint_is_gen(expr) {
                        self.lint_condition(c, false);
                    }
                    self.analyze_condition(c);
                    self.analyze_children(&args[1..]);
                }
                None
            }
            ("potemkin", "import-vars") => {
                self.analyze_import_vars(expr, DefBy { by: (intern("potemkin"), intern("import-vars")), lint_as: by.lint_as });
                None
            }
            ("potemkin", "import-fn") | ("potemkin", "import-macro") | ("potemkin", "import-def") => {
                self.analyze_import_fn(expr, by);
                None
            }
            ("clojure.string", "replace") => {
                self.analyze_children(args);
                self.lint_string_replace(args);
                None
            }
            ("clojure.test.check.properties", "for-all") => {
                self.analyze_like_let(expr);
                None
            }
            ("schema.core", "fn") | ("schema.core", "def") | ("schema.core", "defn") | ("schema.core", "defmethod") | ("schema.core", "defrecord") | ("schema.core", "defprotocol") => self.analyze_schema(expr, as_.1.as_str(), by),
            ("clojure.spec.alpha", "def") | ("cljs.spec.alpha", "def") => {
                // kondo `spec/analyze-def`: the name keyword carries `:reg`
                let fq = intern(&format!("{}/def", as_.0.as_str()));
                if let Some(&n) = args.first() {
                    self.note_reg(n, fq);
                    self.scope(|a| {
                        a.ctx.off |= lint::uses::OFF_SYM;
                        a.analyze_expression(n);
                    });
                    self.analyze_children(&args[1..]);
                }
                None
            }
            ("clojure.spec.alpha", "fdef") | ("cljs.spec.alpha", "fdef") => {
                // kondo `spec/analyze-fdef`: the fn symbol only marks its namespace used, no usage is registered
                if let Some((&sym, body)) = args.split_first() {
                    self.lint_known_keys(body, &["args", "ret", "fn"]);
                    let s = self.c.unwrap_meta(sym);
                    if self.kind(s) == Kind::Symbol {
                        let r = self.resolve_name(true, (self.c.ns(s), self.c.name(s)), extras::NO_EXPR);
                        if r.found && !r.ns.is_none() {
                            self.lint_use_ns(r.ns);
                        }
                    } else {
                        let p = self.pos(s);
                        self.lint(lint::FType::Syntax, p, "expected symbol");
                    }
                    self.analyze_children(body);
                }
                None
            }
            ("clojure.spec.alpha", "keys") | ("cljs.spec.alpha", "keys") => {
                self.lint_known_keys(args, &["req", "opt", "req-un", "opt-un", "gen"]);
                self.analyze_children(args);
                None
            }
            ("clojure.spec.gen.alpha", "lazy-combinators") | ("clojure.spec.gen.alpha", "lazy-prims") | ("cljs.spec.gen.alpha", "lazy-combinators") | ("cljs.spec.gen.alpha", "lazy-prims") => {
                self.analyze_declare(expr, by);
                None
            }
            ("clojure.core.async", "defblockingop") | ("clojure.core.async", "defparkingop") | ("clojure.core.reducers", "defcurried") => self.analyze_defn(expr, by, false),
            ("clojure.template", "do-template") => {
                if args.len() >= 2 {
                    let argc = self.c.children(args[0]).len();
                    let p = self.pos(expr);
                    self.lint_do_template(Some(p), argc, args.len() - 2);
                }
                self.analyze_children(args);
                None
            }
            ("cljs.core", "simple-benchmark") => {
                self.analyze_like_let(expr);
                None
            }
            ("clojure.core.async", "alt!") | ("clojure.core.async", "alt!!") | ("cljs.core.async", "alt!") | ("cljs.core.async", "alt!!") => {
                // kondo core-async/analyze-alt!: pairs [k v]; `([v ch] body)` binds the vector in the body
                for pair in args.chunks(2) {
                    self.analyze_expression(pair[0]);
                    let Some(&v) = pair.get(1) else { continue };
                    let vk = self.c.unwrap_meta(v);
                    let first = if self.kind(vk) == Kind::List { self.c.children(vk).first().copied() } else { None };
                    match first {
                        Some(f) if self.kind(self.c.unwrap_meta(f)) == Kind::Vector => {
                            let f = self.c.unwrap_meta(f);
                            let rest: Vec<NodeId> = self.c.children(vk)[1..].to_vec();
                            let mut out = Vec::new();
                            self.extract_bindings(f, f, bindings::BOpts::default(), &mut out);
                            self.scope(|a| {
                                a.bindings.extend(out);
                                for r in rest {
                                    a.analyze_expression(r);
                                }
                            });
                        }
                        _ => {
                            self.analyze_expression(v);
                        }
                    }
                }
                None
            }
            _ => {
                self.analyze_unknown_ns_call(args, r);
                None
            }
        }
    }
}
