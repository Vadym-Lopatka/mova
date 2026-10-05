//! `ns` form and `require`: kondo `analyzer/namespace.clj`.
use super::defs::FastMap;
use super::expr::*;
use super::forms::*;
use super::lint::FType;
use super::*;
use std::collections::HashSet;

/// A normalized lib spec: `[name-node option...]`.
#[derive(Clone)]
struct Lib {
    /// The libspec (symbol or vector) the name belongs to.
    spec: NodeId,
    node: NodeId,
    name: SymId,
    opts: Vec<NodeId>,
    bare: bool,
    prefix: Option<SymId>,
}

/// Result of analyzing one lib spec.
struct Analyzed {
    ns: SymId,
    node: NodeId,
    as_: Option<NodeId>,
    excluded: Vec<SymId>,
    /// (local name, (ns, original name))
    referred: Vec<(SymId, (SymId, SymId))>,
    refer_all: bool,
    has_as: bool,
    // lint data
    group: usize,
    spec: NodeId,
    as_alias: bool,
    /// (local, original name, pos) of referred vars
    lref: Vec<(SymId, SymId, Pos)>,
    /// node reported by refer-all / use (the `:all` value or the `:use` keyword)
    ra_node: Option<NodeId>,
    use_: bool,
    req_macros: bool,
    prefix: Option<SymId>,
}

#[derive(Default)]
struct Clauses {
    analyzed: Vec<Analyzed>,
}

impl<'a> Analyzer<'a> {
    /// Lint registration of analyzed libspecs (kondo `analyze-require-clauses` + `reg-required-namespaces!`).
    fn lint_clauses(&mut self, idx: usize, cs: &Clauses, top_level: bool) {
        if !self.lon {
            return;
        }
        // duplicate-require: per clause; top-level `require` also against earlier requires of the ns
        let mut seen: Vec<(usize, Vec<SymId>)> = Vec::new();
        if top_level {
            let init: Vec<SymId> = self.lns_at(idx).required.iter().map(|r| r.ns).collect();
            seen.push((usize::MAX, init));
        }
        // unsorted-required-namespaces / conflicting-alias: per clause (ns form) or per `require` call
        if self.lc().level(FType::UnsortedRequiredNamespaces) != lint::OFF || self.lc().level(FType::ConflictingAlias) != lint::OFF {
            let mut groups: Vec<usize> = cs.analyzed.iter().map(|a| if top_level { 0 } else { a.group }).collect();
            groups.dedup();
            groups.sort_unstable();
            groups.dedup();
            for g in groups {
                let items: Vec<&Analyzed> = cs.analyzed.iter().filter(|a| (if top_level { 0 } else { a.group }) == g).collect();
                // conflicting alias
                let mut aliases: Vec<SymId> = Vec::new();
                for a in &items {
                    if let Some(al) = a.as_ {
                        let nm = self.c.name(al);
                        if aliases.contains(&nm) {
                            let p = self.pos(al);
                            self.lint(FType::ConflictingAlias, p, format!("Conflicting alias for {}", a.ns.as_str()));
                        }
                        aliases.push(nm);
                    }
                }
                // unsorted: the first out-of-order name is reported, then the check stops
                if self.lc().level(FType::UnsortedRequiredNamespaces) != lint::OFF {
                    let case_sensitive = self.lc().lint_sort_case_sensitive(FType::UnsortedRequiredNamespaces);
                    let mut last: Option<Vec<u16>> = None;
                    for a in &items {
                        if self.ex.branch.contains(&a.spec) {
                            continue;
                        }
                        // kondo: prefix lists give `<prefix>.<full name>` (the full name already carries the prefix)
                        let raw = if let Some(p) = a.prefix { format!("{}.{}", p.as_str(), a.ns.as_str()) } else if self.kind(a.node) == Kind::String { format!("\"{}\"", self.c.string_content(a.node)) } else { a.ns.as_str().to_owned() };
                        let raw = if case_sensitive { raw } else { raw.to_lowercase() };
                        let key: Vec<u16> = raw.encode_utf16().collect();
                        if let Some(l) = &last {
                            if l.as_slice() > key.as_slice() {
                                let p = self.pos(a.node);
                                self.lint(FType::UnsortedRequiredNamespaces, p, format!("Unsorted namespace: {}", a.ns.as_str()));
                                break;
                            }
                        }
                        last = Some(key);
                    }
                }
            }
        }
        for a in &cs.analyzed {
            let pos = self.pos(a.node);
            let gi = if top_level { 0 } else { a.group };
            let si = match seen.iter().position(|(g, _)| *g == gi || (top_level && *g == usize::MAX)) {
                Some(i) => i,
                None => {
                    seen.push((gi, Vec::new()));
                    seen.len() - 1
                }
            };
            let mut v = std::mem::take(&mut seen[si].1);
            self.lint_duplicate_requires(&mut v, a.ns, pos);
            seen[si].1 = v;
            let (ns, as_alias) = (a.ns, a.as_alias);
            self.lns_at(idx).required.push(lint::nsl::Req { ns, pos, as_alias });
            let self_macro = a.req_macros && self.base == BaseLang::Cljc && self.lang == Lang::Cljs && ns == self.nss[idx].name;
            for &(local, name, p) in &a.lref {
                self.lns_at(idx).referred.push(lint::nsl::Ref { local, ns, name, pos: p, self_macro });
            }
            if let Some(n) = a.ra_node {
                let np = self.pos(n);
                let kw = self.kind(n) == Kind::Keyword;
                let is_use = a.use_ && (kw || self.is_sym_named(n, "use"));
                let l = self.lns_at(idx);
                match l.refer_alls.iter_mut().find(|r| r.ns == ns) {
                    Some(r) => {
                        r.pos = np;
                        r.is_use = is_use;
                        r.kw = kw;
                    }
                    None => l.refer_alls.push(lint::nsl::RAll { ns, pos: np, is_use, kw, used: Vec::new() }),
                }
                l.used.insert(ns);
            } else if a.as_.is_none() && a.referred.is_empty() {
                self.lns_at(idx).used.insert(ns);
            }
        }
    }

    /// kondo `normalize-libspec` (syntax only; findings are skipped).
    fn normalize_libspec(&mut self, prefix: Option<SymId>, spec: NodeId, out: &mut Vec<Lib>) {
        let (spec, _) = self.lift_meta(spec);
        let cljs = self.is_cljs();
        let fixname = |a: &Self, s: SymId| -> SymId {
            if cljs {
                match s.as_str() {
                    "clojure.test" => return intern("cljs.test"),
                    "clojure.pprint" => return intern("cljs.pprint"),
                    _ => {}
                }
            }
            let _ = a;
            s
        };
        let full = |p: Option<SymId>, n: SymId| -> SymId {
            match p {
                Some(p) => intern(&format!("{}.{}", p.as_str(), n.as_str())),
                None => n,
            }
        };
        match self.kind(spec) {
            Kind::Symbol => {
                let nm = if self.c.ns(spec).is_none() { self.c.name(spec) } else { intern(&self.node_str(spec)) };
                out.push(Lib { spec, node: spec, name: fixname(self, full(prefix, nm)), opts: Vec::new(), bare: true, prefix });
            }
            Kind::String => {
                let s = intern(self.c.string_content(spec));
                out.push(Lib { spec, node: spec, name: full(prefix, s), opts: Vec::new(), bare: true, prefix });
            }
            Kind::Vector | Kind::List => {
                let kids = self.kids(spec);
                if kids.is_empty() {
                    return;
                }
                let first = kids[0];
                let has_kw = kids.iter().any(|&k| self.kind(k) == Kind::Keyword);
                if self.kind(first) == Kind::Symbol && !has_kw && kids.len() > 1 {
                    let p = if self.c.ns(first).is_none() { self.c.name(first) } else { intern(&self.node_str(first)) };
                    let p = full(prefix, p);
                    for &k in &kids[1..] {
                        self.normalize_libspec(Some(p), k, out);
                    }
                } else if matches!(self.kind(first), Kind::Symbol | Kind::String) && (kids.len() == 1 || self.kind(kids[1]) == Kind::Keyword) {
                    let mut tmp = Vec::new();
                    self.normalize_libspec(prefix, first, &mut tmp);
                    for mut l in tmp {
                        l.spec = spec;
                        l.opts = kids[1..].to_vec();
                        l.bare = false;
                        out.push(l);
                    }
                }
            }
            _ => {}
        }
    }

    /// kondo `analyze-libspec`.
    fn analyze_libspec(&mut self, require_kw: &str, kw_node: NodeId, group: usize, lib: &Lib, cs: &mut Clauses) {
        let use_ = require_kw == "use";
        // kondo `analyze-libspec`: self-requiring-namespace (not for require-macros, comments, `:as-alias`)
        if lib.name == self.cur_ns_name() && require_kw != "require-macros" && !self.ctx.in_comment && !(!lib.bare && lib.opts.first().map_or(false, |&o| self.kind(o) == Kind::Keyword && self.c.name(o).as_str() == "as-alias")) {
            let p = self.pos(lib.spec);
            self.lint(FType::SelfRequiringNamespace, p, format!("Namespace is requiring itself: {}", lib.name.as_str()));
        }
        let mut a = Analyzed { ns: lib.name, node: lib.node, as_: None, excluded: Vec::new(), referred: Vec::new(), refer_all: use_, has_as: false, group, spec: lib.spec, as_alias: false, lref: Vec::new(), ra_node: if use_ { Some(kw_node) } else { None }, use_, req_macros: require_kw == "require-macros", prefix: lib.prefix };
        if lib.bare {
            cs.analyzed.push(a);
            return;
        }
        let ns_name = lib.name;
        let mut i = 0;
        let mut renamed: Vec<(SymId, SymId, Pos)> = Vec::new();
        while i < lib.opts.len() {
            let child = lib.opts[i];
            let opt = lib.opts.get(i + 1).copied();
            i += 2;
            if self.kind(child) != Kind::Keyword {
                continue;
            }
            match self.c.name(child).as_str() {
                "refer" | "refer-macros" | "only" => {
                    if let Some(o) = opt {
                        match self.kind(o) {
                            Kind::Vector | Kind::List | Kind::Set => {
                                if use_ && self.c.name(child).as_str() == "only" {
                                    a.refer_all = false;
                                    a.ra_node = None;
                                    let mut names: Vec<String> = self.kids(o).iter().map(|&x| self.node_str(x)).collect();
                                    names.sort();
                                    let sym_form = self.kind(kw_node) == Kind::Symbol;
                                    let p = self.pos(kw_node);
                                    self.lint(FType::Use, p, format!("use {}require with alias or :refer [{}]", if sym_form { "" } else { ":" }, names.join(" ")));
                                }
                                let ch = self.kids(o);
                                for &r in &ch {
                                    if self.kind(r) == Kind::Symbol {
                                        let nm = self.c.name(r);
                                        a.referred.push((nm, (ns_name, nm)));
                                        let p = self.pos(r);
                                        a.lref.push((nm, nm, p));
                                        self.note_used(ns_name);
                                        let res = resolve::Resolved { found: true, ns: ns_name, name: nm, ..resolve::Resolved::none() };
                                        self.reg_var_usage(VarUsageArgs { name: nm, pos: p, name_pos: p, arity: NO_ARITY, r: res, refer: true, defmethod: false, dispatch_val: SymId::NONE, testing: SymId::NONE, derived: false, derived_name: false, no_from_var: false, from: SymId::NONE, written: (SymId::NONE, nm) });
                                    }
                                }
                            }
                            Kind::Keyword if self.c.name(o).as_str() == "all" => {
                                a.refer_all = true;
                                a.ra_node = Some(o);
                                a.use_ = false;
                            }
                            _ => {}
                        }
                    }
                }
                "as" | "as-alias" => {
                    if let Some(o) = opt {
                        if self.kind(o) == Kind::Symbol {
                            a.as_ = Some(o);
                            a.has_as = true;
                            a.as_alias = self.c.name(child).as_str() == "as-alias";
                        }
                    }
                }
                "default" => {
                    if let Some(o) = opt {
                        if self.kind(o) == Kind::Symbol {
                            let nm = self.c.name(o);
                            a.referred.push((nm, (ns_name, nm)));
                            let p = self.pos(o);
                            a.lref.push((nm, nm, p));
                        }
                    }
                }
                "exclude" => {
                    if let Some(o) = opt {
                        for r in self.kids(o) {
                            if self.kind(r) == Kind::Symbol {
                                a.excluded.push(self.c.name(r));
                            }
                        }
                    }
                }
                "rename" => {
                    if let Some(o) = opt {
                        let kv = self.kids(o);
                        let mut j = 0;
                        while j + 1 < kv.len() {
                            if self.kind(kv[j]) == Kind::Symbol && self.kind(kv[j + 1]) == Kind::Symbol {
                                let (orig, new) = (self.c.name(kv[j]), self.c.name(kv[j + 1]));
                                let p = self.pos(kv[j + 1]);
                                renamed.push((orig, new, p));
                                a.excluded.push(orig);
                            }
                            j += 2;
                        }
                    }
                }
                _ => {}
            }
        }
        for (orig, new, p) in renamed {
            a.referred.retain(|(n, _)| *n != orig);
            a.referred.push((new, (ns_name, orig)));
            a.lref.retain(|r| r.0 != orig);
            a.lref.push((new, orig, p));
        }
        cs.analyzed.push(a);
    }

    fn analyze_require_clauses(&mut self, kw_libspecs: &[(String, NodeId, Vec<NodeId>)]) -> Clauses {
        let mut cs = Clauses::default();
        for (gi, (kw, kwn, specs)) in kw_libspecs.iter().enumerate() {
            for &s in specs {
                let mut libs = Vec::new();
                self.normalize_libspec(None, s, &mut libs);
                for l in libs {
                    self.analyze_libspec(kw, *kwn, gi, &l, &mut cs);
                }
            }
        }
        cs
    }

    /// Apply analyzed require clauses to a namespace state and emit namespace-usages.
    fn apply_clauses(&mut self, idx: usize, cs: &Clauses, emit: bool, replace: bool) {
        let from = self.nss[idx].name;
        let lang = self.lang;
        let base_clj = self.base == BaseLang::Clj;
        let mut referred_all_names: Vec<(SymId, Vec<SymId>)> = Vec::new();
        for a in &cs.analyzed {
            let n = a.ns;
            let st = &mut self.nss[idx];
            st.qualify.entry(n).or_insert(n);
            if let Some(al) = a.as_ {
                let alias = self.c.name(al);
                let st = &mut self.nss[idx];
                st.qualify.insert(alias, n);
                st.aliases.insert(alias, n);
            }
            let st = &mut self.nss[idx];
            for (local, v) in &a.referred {
                st.referred.insert(*local, *v);
            }
            if a.refer_all {
                st.refer_alls.push((n, a.excluded.clone()));
                if base_clj || (lang == Lang::Clj && self.base == BaseLang::Clj) {
                    referred_all_names.push((n, a.excluded.clone()));
                }
                self.out.refer_alls.push((from, n, a.excluded.clone()));
            }
        }
        // `:refer :all` for clj: referred vars come from the cache (kondo `from-cache-1`)
        for (n, excl) in referred_all_names {
            let names = self.defs.names(Src::Clj, n);
            let st = &mut self.nss[idx];
            for nm in names {
                if !excl.contains(&nm) {
                    st.referred.entry(nm).or_insert((n, nm));
                }
            }
        }
        let _ = replace;
        self.lint_clauses(idx, cs, !replace);
        if emit {
            for a in &cs.analyzed {
                let np = self.pos(a.node);
                let (alias, alias_pos) = match a.as_ {
                    Some(al) => (self.c.name(al), self.pos(al)),
                    None => (SymId::NONE, Pos { row: 0, col: 0, end_row: 0, end_col: 0 }),
                };
                self.out.namespace_usages.push(NsUsage { name_pos: np, alias_pos, from, to: a.ns, alias, lang: self.ltag });
            }
        }
    }

    /// kondo `analyze-ns-decl`.
    /// kondo `analyze-ns-decl`: `namespace-name-mismatch` (first per file) and `underscore-in-namespace`.
    fn lint_ns_name(&mut self, ns_name: SymId, pos: Pos) {
        let name = ns_name.as_str();
        if let Some(fd) = &self.ex.file_dotted {
            if ns_name != syms().user && self.lc().level(FType::NamespaceNameMismatch) != lint::OFF {
                let munged = name.replace('-', "_");
                let mismatch = !fd.ends_with(&munged);
                let seen = self.out.findings.iter().any(|f| f.ty == FType::NamespaceNameMismatch);
                if mismatch && !seen {
                    self.lint(FType::NamespaceNameMismatch, pos, format!("Namespace name does not match file name: {name}"));
                }
            }
        }
        if name.contains('_') && self.lc().level(FType::UnderscoreInNamespace) != lint::OFF {
            self.lint(FType::UnderscoreInNamespace, pos, format!("Avoid underscore in namespace name: {name}"));
        }
    }

    pub fn analyze_ns_decl(&mut self, expr: NodeId) {
        // kondo: the whole ns form is walked as quoted (keyword usages; `from` is the previous namespace)
        self.analyze_usages2(expr, true, false);
        let kids = self.kids(expr);
        let pos = self.pos(expr);
        let (name_t, name_meta) = match kids.get(1) {
            Some(&n) => self.lift_meta(n),
            None => (expr, MetaInfo::default()),
        };
        let name_ok = kids.len() > 1 && self.kind(name_t) == Kind::Symbol;
        let name_pos = if name_ok { self.pos(name_t) } else { Pos { row: 0, col: 0, end_row: 0, end_col: 0 } };
        let rest: Vec<NodeId> = kids.iter().skip(2).copied().collect();
        let mut doc: Option<String> = None;
        let mut doc_raw = false;
        if let Some(&f) = rest.first() {
            if let Some(d) = self.string_of(f) {
                doc = Some(d);
                doc_raw = true;
            }
        }
        let meta_node = match rest.first() {
            Some(&f) if self.kind(f) == Kind::Map => Some(f),
            Some(_) => rest.get(1).copied().filter(|&s| self.kind(s) == Kind::Map),
            None => None,
        };
        if let Some(mn) = meta_node {
            self.analyze_expression(mn);
        }
        let mut ns_meta = name_meta.clone();
        if let Some(mn) = meta_node {
            let mut mi = MetaInfo::default();
            self.fold_meta_inner(mn, &mut mi);
            if mi.doc.is_some() {
                doc = mi.doc.clone();
            }
            merge_meta(&mut ns_meta, &mi);
        }
        if doc.is_none() && name_meta.doc_present {
            doc = name_meta.doc.clone();
        }
        let ns_name = if name_ok { self.c.name(name_t) } else { syms().user };
        let mut clauses: &[NodeId] = &rest;
        if doc_raw {
            clauses = &clauses[1..];
        }
        if meta_node.is_some() && !clauses.is_empty() {
            clauses = &clauses[1..];
        }
        // require clauses
        let mut kw_specs: Vec<(String, NodeId, Vec<NodeId>)> = Vec::new();
        let mut imports: Vec<(SymId, SymId, NodeId)> = Vec::new();
        let mut raw_imports: Vec<NodeId> = Vec::new();
        let mut excluded: Vec<SymId> = Vec::new();
        let mut excluded_nodes: Vec<(SymId, NodeId)> = Vec::new();
        let mut renamed: Vec<(SymId, SymId)> = Vec::new();
        let mut globals: Vec<(SymId, SymId)> = Vec::new();
        for &cl in clauses {
            let cl = self.c.unwrap_meta(cl);
            if self.kind(cl) != Kind::List {
                continue;
            }
            let ck = self.kids(cl);
            let Some(&k0) = ck.first() else { continue };
            if self.kind(k0) != Kind::Keyword || !self.c.ns(k0).is_none() {
                continue;
            }
            let kw = self.c.name(k0).as_str();
            match kw {
                "require" | "require-macros" | "use" | "require-global" => kw_specs.push((kw.to_owned(), k0, ck[1..].to_vec())),
                "import" => {
                    for &l in &ck[1..] {
                        raw_imports.push(l);
                        imports.extend(self.import_libspec(l));
                    }
                }
                "refer-clojure" => {
                    let mut j = 1;
                    while j + 1 < ck.len() {
                        let (k, v) = (ck[j], ck[j + 1]);
                        j += 2;
                        if self.kind(k) != Kind::Keyword {
                            continue;
                        }
                        match self.c.name(k).as_str() {
                            "exclude" => {
                                for x in self.kids(v) {
                                    if self.kind(x) == Kind::Symbol {
                                        excluded.push(self.c.name(x));
                                        excluded_nodes.push((self.c.name(x), x));
                                    }
                                }
                            }
                            "rename" => {
                                let kv = self.kids(v);
                                let mut q = 0;
                                while q + 1 < kv.len() {
                                    if self.kind(kv[q]) == Kind::Symbol && self.kind(kv[q + 1]) == Kind::Symbol {
                                        renamed.push((self.c.name(kv[q]), self.c.name(kv[q + 1])));
                                        excluded.push(self.c.name(kv[q]));
                                        excluded_nodes.push((self.c.name(kv[q]), kv[q]));
                                    }
                                    q += 2;
                                }
                            }
                            _ => {}
                        }
                    }
                }
                "refer-global" => {
                    let mut j = 1;
                    while j + 1 < ck.len() {
                        let (k, v) = (ck[j], ck[j + 1]);
                        j += 2;
                        if self.is_kw_named(k, "only") {
                            for x in self.kids(v) {
                                if self.kind(x) == Kind::Symbol {
                                    globals.push((self.c.name(x), self.c.name(x)));
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        // new namespace state (keeps vars of a same-named ns seen earlier in the file)
        let mut st = NsState::new(ns_name, self.lang);
        if let Some(i) = self.nss.iter().position(|n| n.name == ns_name) {
            // deep-merge with the previous state of the same namespace
            let old = std::mem::replace(&mut self.nss[i], NsState::new(ns_name, self.lang));
            st.vars = old.vars;
            for (k, v) in old.qualify {
                st.qualify.entry(k).or_insert(v);
            }
            for (k, v) in old.aliases {
                st.aliases.entry(k).or_insert(v);
            }
            for (k, v) in old.referred {
                st.referred.entry(k).or_insert(v);
            }
            st.imports = old.imports;
            self.nss[i] = st;
            self.cur = i;
        } else {
            self.nss.push(st);
            self.cur = self.nss.len() - 1;
        }
        let idx = self.cur;
        self.lint_ns_local_config(kids.get(1).copied(), meta_node);
        if name_ok {
            self.lint_ns_name(ns_name, name_pos);
        }
        self.lint_excluded_vars(idx, &excluded_nodes);
        let mut seen_imports: Vec<SymId> = Vec::new();
        // unsorted-imports: kondo `lint-unsorted-required-namespaces!` over the raw import libspec nodes (first out-of-order one only)
        if self.lc().level(FType::UnsortedImports) != lint::OFF {
            let cs = self.lc().lint_sort_case_sensitive(FType::UnsortedImports);
            let mut last: Option<Vec<u16>> = None;
            for &n in &raw_imports {
                let raw = self.node_str(n);
                let key: Vec<u16> = (if cs { raw } else { raw.to_lowercase() }).encode_utf16().collect();
                if let Some(l) = &last {
                    if l.as_slice() > key.as_slice() {
                        let p = self.pos(n);
                        let m = format!("Unsorted import: {}", self.node_str(n));
                        self.lint(FType::UnsortedImports, p, m);
                        break;
                    }
                }
                last = Some(key);
            }
        }
        for (class, pkg, node) in &imports {
            let first = !seen_imports.contains(class);
            seen_imports.push(*class);
            self.lint_add_import(idx, *class, *pkg, *node, false);
            self.nss[idx].imports.insert(*class, *pkg);
            if first {
                self.java_class_import(*class, *pkg, *node, true);
            }
        }
        for x in excluded {
            self.nss[idx].clojure_excluded.insert(x);
        }
        for (orig, new) in renamed {
            self.nss[idx].referred.insert(new, (syms().clojure_core, orig));
        }
        for (a, b) in globals {
            self.nss[idx].referred_globals.insert(a, b);
        }
        let cs = self.analyze_require_clauses(&kw_specs);
        // namespace definition comes before its usages
        self.out.namespace_definitions.push(NsDef {
            pos,
            name_pos,
            name: ns_name,
            doc: doc.as_deref().map_or(SymId::NONE, intern),
            no_doc: ns_meta.no_doc,
            deprecated: ns_meta.deprecated,
            added: ns_meta.added,
            author: ns_meta.author,
            in_ns: false,
            lang: self.ltag,
        });
        self.apply_clauses(idx, &cs, true, true);
        let core = self.core_ns();
        self.note_used(core);
        self.note_used(ns_name);
        self.lint_use_ns_at(idx, core);
        self.lint_use_ns_at(idx, ns_name);
        for a in &cs.analyzed {
            if a.as_.is_none() && a.referred.is_empty() {
                self.note_used(a.ns);
            }
            if a.refer_all {
                self.note_used(a.ns);
            }
        }
    }

    /// kondo `analyze-require` for a top-level `(require '[foo :as f])` / `(use ...)`.
    pub fn analyze_require(&mut self, expr: NodeId) {
        let kids = self.kids(expr);
        let kw = self.c.name(kids[0]).as_str().to_owned();
        let mut quoted: Vec<NodeId> = Vec::new();
        let mut rest: Vec<NodeId> = Vec::new();
        for &c in &kids[1..] {
            match self.kind(c) {
                Kind::Quote => {
                    if let Some(x) = self.c.nth(c, 0) {
                        quoted.push(x);
                    }
                }
                Kind::List if self.symbol_call_name(c) == Some("quote") => {
                    if let Some(x) = self.c.nth(c, 1) {
                        quoted.push(x);
                    }
                }
                _ => rest.push(c),
            }
        }
        for &q in &quoted {
            self.analyze_usages2(q, true, false);
        }
        let idx = self.cur;
        let kwn = kids[0];
        let cs = self.analyze_require_clauses(&[(kw, kwn, quoted)]);
        self.apply_clauses(idx, &cs, true, false);
        self.analyze_children(&rest);
    }

    pub fn symbol_call_name(&self, n: NodeId) -> Option<&'static str> {
        if self.kind(n) != Kind::List {
            return None;
        }
        let f = self.c.nth(n, 0)?;
        if self.kind(f) == Kind::Symbol && self.c.ns(f).is_none() {
            Some(self.c.name(f).as_str())
        } else {
            None
        }
    }
}

#[allow(dead_code)]
fn _u(_: FastMap<u8, u8>, _: HashSet<u8>) {}
