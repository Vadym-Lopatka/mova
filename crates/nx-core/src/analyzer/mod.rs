//! Native analyzer: a function-by-function port of clj-kondo's `analyzer.clj` producing kondo
//! analysis elements (namespace-definitions, namespace-usages, var-definitions, var-usages,
//! locals, local-usages) per file.
//!
//! Pipeline: `analyze_file` (per file, parallel-safe, reads only an immutable `DefsIndex` for
//! `:refer :all`) -> `FileAnalysis`; then `DefsIndex::add_file` for every file (project layer) and
//! `finish_usages(&mut fa, &defs)` fills usage target info (arities, macro, private, `to`).
//!
//! Built-in var info (clojure.core arities, macro flags, `var_info_gen` tables) is embedded from
//! `builtin.txt` / `varinfo.txt`; regenerate with
//! `python3 tools/gen_builtin.py <clj-kondo>/clj_kondo/impl` (see the script header).
//! Parity vs the JVM oracle: `tools/parity.sh <corpus> --show 20`.
//!
//! Naming follows kondo (`analyze_expression`, `analyze_call`, `analyze_defn`, `resolve_name`, ...) so
//! gaps are greppable. Plug points for other buckets (keywords, symbols, protocol-impls, java-*,
//! instance-invocations) are the no-op functions in `extras.rs`, called where kondo calls
//! `analyze-keyword`, `reg-class-usage!`, `reg-protocol-impl!`, ...

pub mod bindings;
pub mod call;
pub mod config;
pub mod defs;
pub mod emit;
pub mod expr;
pub mod extras;
pub mod extras_emit;
pub mod extras_java;
pub mod extras_types;
pub mod forms;
pub mod hooks;
pub mod java;
pub mod json;
pub mod lint;
pub mod macroexp;
pub mod norm;
pub mod potemkin;
pub mod ns;
pub mod resolve;
pub mod schema;
pub mod special;
pub mod types;

pub use config::Config;
pub use extras::finish_extras;
pub use defs::{DefsIndex, Src, VarInfo};
pub use norm::Lang;
pub use types::*;

use crate::cst::*;
use crate::intern::{intern, SymId};
use defs::{fast_map, FastMap, FastSet};
use std::collections::HashSet;

/// File kind by extension.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FileKind {
    Clj,
    Cljs,
    Cljc,
    Edn,
}

impl FileKind {
    pub fn from_path(p: &str) -> Option<FileKind> {
        let e = p.rsplit('.').next()?;
        match e {
            "clj" | "bb" | "mova" => Some(FileKind::Clj),
            "cljs" => Some(FileKind::Cljs),
            "cljc" => Some(FileKind::Cljc),
            "edn" => Some(FileKind::Edn),
            _ => None,
        }
    }
}

pub type Name = (SymId, SymId);

/// Well-known symbols, interned once.
pub struct Syms {
    pub clojure_core: SymId,
    pub cljs_core: SymId,
    pub user: SymId,
    pub unknown_ns: SymId,
    pub dot: SymId,
    pub amp: SymId,
    pub percent: SymId,
    pub empty: SymId,
    pub ns_kw: SymId,
    pub new_: SymId,
    pub deref: SymId,
    pub fn_star: SymId,
    pub let_: SymId,
    pub if_: SymId,
}

static SYMS: std::sync::OnceLock<Syms> = std::sync::OnceLock::new();
pub fn syms() -> &'static Syms {
    SYMS.get_or_init(|| Syms {
        clojure_core: intern("clojure.core"),
        cljs_core: intern("cljs.core"),
        user: intern("user"),
        unknown_ns: intern("clj-kondo/unknown-namespace"),
        dot: intern("."),
        amp: intern("&"),
        percent: intern("%"),
        empty: intern(""),
        ns_kw: intern("ns"),
        new_: intern("new"),
        deref: intern("deref"),
        fn_star: intern("fn*"),
        let_: intern("let"),
        if_: intern("if"),
    })
}

/// Lexical binding visible to name resolution.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Binding {
    /// Symbol the binding is found by.
    pub key: SymId,
    /// Name reported in local-usages (`NONE` for pseudo bindings such as type hints).
    pub name: SymId,
    /// 0 = none (pseudo bindings such as type hints, `%`).
    pub id: u32,
    pub gen: bool,
    pub mark_used: bool,
    /// Local fn arity info: 0 = none, else index + 1 into `LintState::arities` (kondo `:arities` of the ctx).
    pub ar: u32,
    /// Type tag (kondo `:tag` of the binding): `(type id << 1) | nilable`, 0 = unknown.
    pub tag: u16,
    /// Bound to a literal `nil` (kondo `:nil-literal` meta): a condition on it is an intentional dead branch.
    pub nil_lit: bool,
}

/// Functional context, copied on scope entry (kondo's `ctx` map).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Ctx {
    pub quoted: bool,
    pub sq: i32,
    pub top_level: bool,
    pub in_comment: bool,
    pub in_def: SymId,
    pub macro_: bool,
    pub defmulti: bool,
    pub in_fn_literal: bool,
    pub shallow: bool,
    pub defmethod: bool,
    pub dispatch_val: SymId,
    pub docstring: bool,
    pub in_meta: bool,
    pub private_access: bool,
    pub resolved_as_core: SymId,
    pub mark_bindings_used: bool,
    pub skip_reg_var: bool,
    pub protocol_fn: bool,
    /// Prefix of an enclosing namespaced map (`#:foo{...}`), resolved; `NONE` otherwise.
    pub nsmap_prefix: SymId,
    /// Inside a construct whose analysis result kondo discards (its calls do not count as used namespaces).
    pub dropped: bool,
    /// Position of the expression among the children of the enclosing `analyze_children` (kondo `:idx`/`:len`), `u32::MAX` = none.
    pub idx: u32,
    pub len: u32,
    /// Linters disabled in this context (kondo `ctx-with-linter-disabled`), see `lint::OFF_*`.
    pub off: u16,
    /// Expected `recur` argument count (kondo `:recur-arity`): number, or `lint::R_*` markers.
    pub recur: u32,
    /// Index into `LintState::seen_recur` of the enclosing fn/loop (`u32::MAX` = none).
    pub seen: u32,
    /// Inside the single-form body of a `let` (kondo `:let-parent`).
    pub let_parent: bool,
    /// The innermost enclosing call is generated (hook expansion / synthetic): kondo `(:clj-kondo.impl/generated (meta parent-call))`.
    pub gen_call: bool,
}

impl Ctx {
    fn new() -> Ctx {
        Ctx {
            quoted: false,
            sq: 0,
            top_level: true,
            in_comment: false,
            in_def: SymId::NONE,
            macro_: false,
            defmulti: false,
            in_fn_literal: false,
            shallow: false,
            defmethod: false,
            dispatch_val: SymId::NONE,
            docstring: false,
            in_meta: false,
            private_access: false,
            resolved_as_core: SymId::NONE,
            mark_bindings_used: false,
            skip_reg_var: false,
            protocol_fn: false,
            nsmap_prefix: SymId::NONE,
            dropped: false,
            idx: u32::MAX,
            len: u32::MAX,
            off: 0,
            recur: lint::R_NONE,
            seen: u32::MAX,
            let_parent: false,
            gen_call: false,
        }
    }
}

/// State of one namespace during analysis (kondo `namespaces` atom entry).
pub(crate) struct NsState {
    pub name: SymId,
    /// alias-or-ns -> ns
    pub qualify: FastMap<SymId, SymId>,
    pub aliases: FastMap<SymId, SymId>,
    /// referred name -> (ns, original name)
    pub referred: FastMap<SymId, (SymId, SymId)>,
    /// (ns, excluded names)
    pub refer_alls: Vec<(SymId, Vec<SymId>)>,
    /// simple class name -> package
    pub imports: FastMap<SymId, SymId>,
    pub vars: FastSet<SymId>,
    pub clojure_excluded: FastSet<SymId>,
    pub referred_globals: FastMap<SymId, SymId>,
}

impl NsState {
    pub fn new(name: SymId, lang: Lang) -> NsState {
        let s = syms();
        let mut qualify = fast_map();
        qualify.insert(name, name);
        match lang {
            Lang::Clj => {
                qualify.insert(s.clojure_core, s.clojure_core);
            }
            Lang::Cljs => {
                qualify.insert(s.cljs_core, s.cljs_core);
                qualify.insert(s.clojure_core, s.cljs_core);
            }
        }
        NsState {
            name,
            qualify,
            aliases: fast_map(),
            referred: fast_map(),
            refer_alls: Vec::new(),
            imports: fast_map(),
            vars: HashSet::default(),
            clojure_excluded: HashSet::default(),
            referred_globals: fast_map(),
        }
    }
}

pub(crate) struct Analyzer<'a> {
    pub c: Cst,
    pub out: FileAnalysis,
    pub lang: Lang,
    pub base: BaseLang,
    /// Element language tag (only set for cljc).
    pub ltag: u8,
    pub cfg: &'a Config,
    pub defs: &'a DefsIndex,
    pub nss: Vec<NsState>,
    pub cur: usize,
    pub ctx: Ctx,
    pub opts: Options,
    pub cs: Vec<Name>,
    pub bindings: Vec<Binding>,
    pub gensym: u32,
    /// Content of synthetic string nodes (hook-generated docstrings).
    pub syn_str: FastMap<NodeId, String>,
    /// Lint state (see `lint`).
    pub lt: lint::LintState,
    /// Linters enabled for this run (internal analysis only).
    pub lon: bool,
    pub ex: ExtraState,
}

/// What to analyze (kondo `:analysis` config). `internal` = project sources, `external` = dependency jars.
#[derive(Clone, Copy, Debug)]
pub struct Options {
    pub var_usages: bool,
    pub locals: bool,
    pub callstack: bool,
    /// Do not analyze function/def bodies (`:var-definitions {:shallow true}`).
    pub shallow: bool,
    /// Namespace of forms before the first `ns` (kondo: `user`, `leiningen.core.project` for project.clj); `NONE` = user.
    pub init_ns: SymId,
    /// Dependency (jar) analysis: no symbols / instance-invocations / java-class-usages.
    pub external: bool,
    /// Linters disabled for the whole file (`lint::uses::OFF_*`), e.g. data_readers.clj.
    pub off: u16,
    /// Mova dialect (`.mova` file): `(catch e ..)`, forward references to later defs, `throw` of any value.
    pub mova: bool,
}

impl Options {
    pub fn internal() -> Options {
        Options { var_usages: true, locals: true, callstack: true, shallow: false, init_ns: SymId::NONE, external: false, off: 0, mova: false }
    }
    /// File-specific settings by relative path: `project.clj` namespace, `data_readers.clj[c]` disabled linters.
    pub fn apply_path(&mut self, rel: &str) {
        self.mova = rel.ends_with(".mova");
        match rel.rsplit('/').next() {
            Some("project.clj") => self.init_ns = intern("leiningen.core.project"),
            Some("data_readers.clj") | Some("data_readers.cljc") => self.off |= lint::uses::OFF_SYM | lint::uses::OFF_NS | lint::uses::OFF_PRIV,
            _ => {}
        }
    }
    pub fn external() -> Options {
        Options { var_usages: false, locals: false, callstack: false, shallow: true, init_ns: SymId::NONE, external: true, off: 0, mova: false }
    }
}

/// Analyze one file: parse, normalize per language (cljc analyzes `:clj` then `:cljs`), analyze.
pub fn analyze_file(src: &str, kind: FileKind, cfg: &Config, defs: &DefsIndex) -> FileAnalysis {
    analyze_file_opts(src, kind, cfg, defs, Options::internal())
}

pub fn analyze_file_opts(src: &str, kind: FileKind, cfg: &Config, defs: &DefsIndex, opts: Options) -> FileAnalysis {
    let cst = crate::reader::parse(src);
    analyze_cst(cst, kind, cfg, defs, opts)
}

pub fn analyze_cst(cst: Cst, kind: FileKind, cfg: &Config, defs: &DefsIndex, opts: Options) -> FileAnalysis {
    analyze_cst_at(cst, kind, cfg, defs, opts, None)
}

/// kondo's `filename` as a dotted string for `namespace-name-mismatch`: no extension, `/` and `\` -> `.`.
pub fn dotted_path(path: &str) -> String {
    let p = match path.rfind('.') {
        Some(i) => &path[..i],
        None => path,
    };
    p.replace(['/', '\\'], ".")
}

/// `analyze_cst` with the file path (`None` for stdin-like sources: no file-name check).
pub fn analyze_cst_at(cst: Cst, kind: FileKind, cfg: &Config, defs: &DefsIndex, opts: Options, path: Option<&str>) -> FileAnalysis {
    let mut fa = FileAnalysis::default();
    let (base, langs): (BaseLang, &[Lang]) = match kind {
        FileKind::Clj => (BaseLang::Clj, &[Lang::Clj]),
        FileKind::Cljs => (BaseLang::Cljs, &[Lang::Cljs]),
        FileKind::Cljc => (BaseLang::Cljc, &[Lang::Clj, Lang::Cljs]),
        FileKind::Edn => (BaseLang::Clj, &[Lang::Clj]),
    };
    fa.base_lang = Some(base);
    fa.has_callstack = opts.callstack;
    fa.mova = opts.mova;
    if lint::lint_enabled(opts.locals) && lint::reader_errors(&cst, cfg, &mut fa) {
        // kondo gives up on the file (no analysis)
        return fa;
    }
    fa.lint_levels = cfg.levels.clone();
    if opts.locals {
        fa.lint_ignores = lint::ignore::collect(&cst);
    }
    let need = norm::needs_norm(&cst);
    let mut cst = Some(cst);
    for (i, &lang) in langs.iter().enumerate() {
        let mut c = if i + 1 == langs.len() { cst.take().unwrap() } else { cst.as_ref().unwrap().clone() };
        let branch = if need { norm::normalize(&mut c, lang) } else { Vec::new() };
        let ltag = if base == BaseLang::Cljc { if lang == Lang::Clj { L_CLJ } else { L_CLJS } } else { L_NONE };
        let mut a = Analyzer {
            c,
            out: std::mem::take(&mut fa),
            lang,
            base,
            ltag,
            cfg,
            defs,
            nss: vec![NsState::new(if opts.init_ns.is_none() { syms().user } else { opts.init_ns }, lang)],
            cur: 0,
            ctx: Ctx::new(),
            opts,
            cs: Vec::new(),
            bindings: Vec::new(),
            gensym: 0,
            syn_str: fast_map(),
            lt: lint::LintState::new(cfg),
            lon: lint::lint_enabled(opts.locals),
            ex: ExtraState { file_dotted: path.map(dotted_path), edn: kind == FileKind::Edn, branch: if branch.is_empty() { Default::default() } else { branch.into_iter().collect() }, ..Default::default() },
        };
        a.analyze_expressions();
        a.lint_end();
        fa = a.out;
    }
    fa
}

impl<'a> Analyzer<'a> {
    /// kondo `analyze-expressions`.
    fn analyze_expressions(&mut self) {
        let root = self.c.root();
        let r = self.c.kids(root);
        for i in 0..r.1 {
            let n = self.c.kid(r, i);
            self.bindings.clear();
            self.cs.clear();
            self.ctx = Ctx::new();
            self.ctx.shallow = self.opts.shallow;
            self.ctx.off = self.opts.off;
            if self.ex.edn {
                self.analyze_edn(n);
            } else {
                self.analyze_expression(n);
            }
        }
    }

    // ---- scoping ----
    /// Run `f`, then restore ctx, callstack and bindings (kondo passes `ctx` functionally).
    #[inline]
    pub fn scope<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        let ctx = self.ctx;
        let cs = self.cs.len();
        let b = self.bindings.len();
        let r = f(self);
        self.ctx = ctx;
        self.cs.truncate(cs);
        self.bindings.truncate(b);
        r
    }

    // ---- node helpers ----
    #[inline]
    pub fn kind(&self, n: NodeId) -> Kind {
        self.c.kind(n)
    }
    #[inline]
    pub fn pos(&self, n: NodeId) -> Pos {
        if self.c.has_pos(n) {
            self.c.pos(n)
        } else {
            Pos { row: 0, col: 0, end_row: 0, end_col: 0 }
        }
    }
    #[inline]
    pub fn is_sym_named(&self, n: NodeId, name: &str) -> bool {
        self.c.kind(n) == Kind::Symbol && self.c.ns(n).is_none() && self.c.name(n).as_str() == name
    }
    #[inline]
    pub fn is_kw_named(&self, n: NodeId, name: &str) -> bool {
        self.c.kind(n) == Kind::Keyword && self.c.ns(n).is_none() && self.c.flags(n) & F_AUTO == 0 && self.c.name(n).as_str() == name
    }
    pub fn kids(&self, n: NodeId) -> Vec<NodeId> {
        self.c.children(n).to_vec()
    }
    pub fn node_str(&self, n: NodeId) -> String {
        node_str(&self.c, n)
    }
    pub fn fresh(&mut self) -> u32 {
        self.gensym += 1;
        self.gensym
    }
    pub fn cur_ns(&self) -> &NsState {
        &self.nss[self.cur]
    }
    pub fn cur_ns_mut(&mut self) -> &mut NsState {
        &mut self.nss[self.cur]
    }
    pub fn cur_ns_name(&self) -> SymId {
        self.nss[self.cur].name
    }
    /// Record a namespace as used (kondo `:used-namespaces`, decides which cached namespaces are loaded).
    pub fn note_used(&mut self, ns: SymId) {
        if !ns.is_none() {
            self.out.used_ns.push(ns);
        }
    }
    /// Run `f` with results treated as discarded by kondo.
    pub fn dropped<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        let d = self.ctx.dropped;
        self.ctx.dropped = true;
        let r = f(self);
        self.ctx.dropped = d;
        r
    }
    pub fn is_cljs(&self) -> bool {
        self.lang == Lang::Cljs
    }
}

/// `(str node)` as rewrite-clj prints a whitespace-free tree: children joined by one space.
pub fn node_str(c: &Cst, n: NodeId) -> String {
    let mut s = String::new();
    node_str_into(c, n, &mut s);
    s
}

fn node_str_into(c: &Cst, n: NodeId, s: &mut String) {
    if c.kind(n) == Kind::Meta {
        // kondo attaches metadata to the node; `str` prints the target only
        let t = c.unwrap_meta(n);
        if t != n {
            node_str_into(c, t, s);
            return;
        }
    }
    let (open, close): (&str, &str) = match c.kind(n) {
        Kind::List => ("(", ")"),
        Kind::Vector => ("[", "]"),
        Kind::Map => ("{", "}"),
        Kind::Set => ("#{", "}"),
        Kind::AnonFn => ("#(", ")"),
        Kind::Quote => ("'", ""),
        Kind::SyntaxQuote => ("`", ""),
        Kind::Unquote => ("~", ""),
        Kind::UnquoteSplicing => ("~@", ""),
        Kind::Deref => ("@", ""),
        Kind::Var => ("#'", ""),
        Kind::Meta => ("^", ""),
        Kind::Tagged => ("#", ""),
        Kind::NsMap => ("#", ""),
        Kind::Eval => ("#=", ""),
        _ => {
            if !c.has_pos(n) {
                match c.kind(n) {
                    Kind::Symbol | Kind::Keyword => {
                        let ns = c.ns(n);
                        if !ns.is_none() {
                            s.push_str(ns.as_str());
                            s.push('/');
                        }
                        s.push_str(c.name(n).as_str());
                    }
                    _ => {}
                }
            } else {
                s.push_str(c.text(n));
            }
            return;
        }
    };
    s.push_str(open);
    let mut first = true;
    for &k in c.children(n) {
        if c.kind(k) == Kind::Uneval {
            continue;
        }
        if !first && !matches!(c.kind(n), Kind::Quote | Kind::SyntaxQuote | Kind::Unquote | Kind::UnquoteSplicing | Kind::Deref | Kind::Var | Kind::Eval) {
            s.push(' ');
        }
        first = false;
        node_str_into(c, k, s);
    }
    s.push_str(close);
}

/// Decode the escapes of a string token body.
pub fn unescape(raw: &str) -> String {
    if !raw.contains('\\') {
        return raw.to_owned();
    }
    let mut out = String::with_capacity(raw.len());
    let mut it = raw.chars();
    while let Some(ch) = it.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match it.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('b') => out.push('\u{8}'),
            Some('f') => out.push('\u{c}'),
            Some('\\') => out.push('\\'),
            Some('"') => out.push('"'),
            Some('u') => {
                let hex: String = it.by_ref().take(4).collect();
                if let Some(c) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                    out.push(c);
                }
            }
            Some(o) => {
                out.push('\\');
                out.push(o);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// Fill target info of usages from the defs index (kondo `lint-var-usage` -> `reg-usage!`).
pub fn finish_usages(fa: &mut FileAnalysis, defs: &DefsIndex) {
    while fa.var_usages.last().is_some_and(|u| u.synth) {
        fa.var_usages.pop();
    }
    let base = fa.base_lang.unwrap_or(BaseLang::Clj);
    let s = syms();
    let refer_alls = std::mem::take(&mut fa.refer_alls);
    let lint_uses = std::mem::take(&mut fa.lint_uses);
    let lint_on = lint_uses.len() == fa.var_usages.len() && !lint_uses.is_empty();
    let top_ns = fa.namespace_definitions.first().map_or(SymId::NONE, |n| n.name);
    let mut found = std::mem::take(&mut fa.findings);
    let mut ra_used: Vec<(SymId, SymId, SymId, u8)> = Vec::new();
    let mut tf_ptr = 0usize;
    for (ui, u) in fa.var_usages.iter_mut().enumerate() {
        let unknown = u.resolved_ns == s.unknown_ns;
        let fn_ns = if unknown { u.from } else { u.resolved_ns };
        let (mut called, imp) = defs.resolve_call_to(base, u.call_lang, fn_ns, u.name, unknown && u.unresolved);
        let mut refer_ns = SymId::NONE;
        if called.is_none() && u.unresolved {
            // refer-alls of the caller, then clojure.core / cljs.core
            for (from, rns, excl) in &refer_alls {
                if *from == u.from && !excl.contains(&u.name) {
                    if let Some(v) = defs.resolve_call(base, u.call_lang, *rns, u.name, false) {
                        called = Some(v);
                        refer_ns = *rns;
                        break;
                    }
                }
            }
            if called.is_none() && !u.clojure_excluded {
                let core = if u.call_lang == L_CLJS { s.cljs_core } else { s.clojure_core };
                if let Some(v) = defs.resolve_call(base, u.call_lang, core, u.name, false) {
                    called = Some(v);
                    refer_ns = core;
                }
            }
        }
        match called {
            Some(v) => {
                u.to = if !refer_ns.is_none() { refer_ns } else if let Some(i) = imp { i.0 } else { fn_ns };
                u.has_fixed = v.flags & defs::F_FIXED != 0;
                u.fixed = v.fixed;
                u.varargs_min = v.varargs_min;
                u.macro_ = v.flags & defs::F_MACRO != 0;
                u.private = v.flags & defs::F_PRIVATE != 0;
                u.deprecated = v.deprecated;
            }
            None => u.to = if unknown { s.unknown_ns } else { u.resolved_ns },
        }
        if lint_on {
            let (levels, local) = match fa.lint_ns_levels.iter().rev().find(|(n, _)| *n == u.from) {
                Some((_, lv)) => (lv.as_slice(), true),
                None => (fa.lint_levels.as_slice(), false),
            };
            let disc = if lint_uses[ui].flags & lint::uses::F_DISC != 0 { fa.lint_disc.binary_search_by_key(&(ui as u32), |h| h.0).ok().map(|i| &fa.lint_disc[i].1) } else { None };
            let cc = lint::uses::CheckCtx { levels, base, top_ns, defs, disc, imp_name: imp.map(|i| i.1), mova: fa.mova };
            let before = found.len();
            let hof = if lint_uses[ui].flags & lint::uses::F_HOF != 0 { fa.lint_hofs.binary_search_by_key(&(ui as u32), |h| h.0).ok().map(|i| fa.lint_hofs[i].1) } else { None };
            let arity_err = lint::uses::check(&mut found, &cc, u, &lint_uses[ui], called, u.lang, hof);
            while tf_ptr < fa.lint_tfind.len() && (fa.lint_tfind[tf_ptr].0 as usize) <= ui {
                if fa.lint_tfind[tf_ptr].0 as usize == ui && !arity_err && lint_uses[ui].flags & lint::uses::F_ARITY_OFF == 0 && levels.get(lint::FType::TypeMismatch as usize).copied().unwrap_or(2) != 0 {
                    found.push(fa.lint_tfind[tf_ptr].1.clone());
                }
                tf_ptr += 1;
            }
            if local {
                for f in found[before..].iter_mut().filter(|f| f.level == 0) {
                    f.level = levels.get(f.ty as usize).copied().unwrap_or(0);
                }
            }
            if !fa.lint_ralls.is_empty() && lint_uses[ui].written.0.is_none() && called.is_some() && !u.to.is_none() {
                ra_used.push((u.from, u.to, u.name, u.lang));
            }
        }
    }
    fa.findings = found;
    lint::finish::refer_alls(fa, &ra_used);
    lint::proto::check(fa, defs);
    lint::ignore::apply(fa);
    fa.lint_uses = lint_uses;
    fa.refer_alls = refer_alls;
    synth_unresolved_ns_usages(fa);
}

/// clojure-lsp `findings->analysis`: every reported `unresolved-namespace` finding becomes a var-usage
/// `{:to <ns> :name <name> :unresolved? true}` at the finding's range, so `m.other/g` (ns not required) resolves to its var.
fn synth_unresolved_ns_usages(fa: &mut FileAnalysis) {
    if fa.findings.is_empty() {
        return;
    }
    let unq = |s: &str| s.trim_matches('"').replace("\\\"", "\"");
    let mut add = Vec::new();
    for (f, lv, _) in lint::final_findings(fa) {
        if f.ty != lint::FType::UnresolvedNamespace || lv == 0 {
            continue;
        }
        let get = |k: &str| f.extra.iter().find(|(n, _)| *n == k).map(|(_, v)| unq(v));
        let (Some(ns), Some(name)) = (get("ns"), get("name")) else { continue };
        let p = f.pos;
        let u = VarUsage { pos: p, name_pos: p, name: intern(&name), resolved_ns: intern(&ns), to: intern(&ns), ..blank_usage() };
        add.push(u);
    }
    fa.var_usages.extend(add);
}

fn blank_usage() -> VarUsage {
    let p = Pos { row: 0, col: 0, end_row: 0, end_col: 0 };
    VarUsage { pos: p, name_pos: p, name: SymId::NONE, from: SymId::NONE, from_var: SymId::NONE, arity: NO_ARITY, alias: SymId::NONE, refer: false, defmethod: false, derived: false, derived_name: false, dispatch_val_str: SymId::NONE, ctx_testing: SymId::NONE, resolved_ns: SymId::NONE, unresolved: true, clojure_excluded: false, lang: 0, to: SymId::NONE, fixed: Arities::default(), has_fixed: false, varargs_min: NO_ARITY, macro_: false, private: false, deprecated: Val::NONE, call_lang: L_CLJ, synth: true }
}
