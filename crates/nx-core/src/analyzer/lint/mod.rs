//! kondo findings (linters). One `Finding` per report; emitted as oracle `findings`.
//! Per-linter code lives in submodules; kondo source references are in each fn doc.
//! Default levels follow kondo `config.clj` `default-config`; `:linters {:x {:level ..}}` overrides come
//! from `Config` (`lint_levels`).
pub mod cfgl;
pub mod finish;
pub mod forms;
pub mod ignore;
pub mod nsl;
pub mod proto;
pub mod rets;
pub mod tags;
pub mod types;
pub mod uses;
pub mod uval;

use crate::cst::Pos;

macro_rules! ftypes {
    ($($v:ident $name:literal $lvl:literal),* $(,)?) => {
        #[derive(Clone, Copy, PartialEq, Eq, Debug)]
        #[repr(u8)]
        pub enum FType { $($v),* }
        pub const FTYPES: &[(FType, &str, u8)] = &[$((FType::$v, $name, $lvl)),*];
        impl FType {
            pub fn name(self) -> &'static str { FTYPES[self as usize].1 }
            pub fn default_level(self) -> u8 { FTYPES[self as usize].2 }
            pub fn from_name(s: &str) -> Option<FType> { FTYPES.iter().find(|t| t.1 == s).map(|t| t.0) }
        }
    };
}

/// `Ctx::recur` markers: no enclosing fn/loop, non-tail position (empty map), unknown arity (`{:fixed-arity nil}`).
pub const R_NONE: u32 = u32::MAX;
pub const R_NONTAIL: u32 = u32::MAX - 1;
pub const R_NIL: u32 = u32::MAX - 2;

/// Levels: 0 off, 1 info, 2 warning, 3 error.
pub const OFF: u8 = 0;
pub const INFO: u8 = 1;
pub const WARNING: u8 = 2;
pub const ERROR: u8 = 3;

pub fn level_name(l: u8) -> &'static str {
    match l {
        INFO => "info",
        WARNING => "warning",
        ERROR => "error",
        _ => "off",
    }
}

ftypes! {
    UnusedBinding "unused-binding" 2,
    UnusedNamespace "unused-namespace" 2,
    UnusedReferredVar "unused-referred-var" 2,
    UnusedImport "unused-import" 2,
    UnusedPrivateVar "unused-private-var" 2,
    UnusedValue "unused-value" 2,
    UnresolvedSymbol "unresolved-symbol" 3,
    UnresolvedNamespace "unresolved-namespace" 2,
    UnresolvedVar "unresolved-var" 2,
    InvalidArity "invalid-arity" 3,
    TypeMismatch "type-mismatch" 3,
    Syntax "syntax" 3,
    RedundantDo "redundant-do" 2,
    RedundantLet "redundant-let" 2,
    RedundantFnWrapper "redundant-fn-wrapper" 0,
    RedundantStrCall "redundant-str-call" 1,
    RedundantNestedCall "redundant-nested-call" 1,
    RedundantFormat "redundant-format" 1,
    MissingBodyInWhen "missing-body-in-when" 2,
    MissingElseBranch "missing-else-branch" 2,
    ConstantCondition "constant-condition" 2,
    MissingProtocolMethod "missing-protocol-method" 2,
    UnresolvedProtocolMethod "unresolved-protocol-method" 2,
    ProtocolMethodArityMismatch "protocol-method-arity-mismatch" 2,
    DuplicateMapKey "duplicate-map-key" 3,
    DuplicateSetKey "duplicate-set-key" 3,
    MissingMapValue "missing-map-value" 3,
    LoopWithoutRecur "loop-without-recur" 2,
    UnexpectedRecur "unexpected-recur" 3,
    RedefinedVar "redefined-var" 2,
    InlineDef "inline-def" 2,
    PrivateCall "private-call" 3,
    DeprecatedVar "deprecated-var" 2,
    Format "format" 3,
    MisplacedDocstring "misplaced-docstring" 2,
    NotAFunction "not-a-function" 3,
    DuplicateRequire "duplicate-require" 2,
    AliasedReferredVar "aliased-referred-var" 1,
    CondElse "cond-else" 2,
    CaseDuplicateTest "case-duplicate-test" 3,
    Use "use" 2,
    ReferAll "refer-all" 2,
    UnquoteNotSyntaxQuoted "unquote-not-syntax-quoted" 2,
    MissingClauseInTry "missing-clause-in-try" 2,
    ShadowedFnParam "shadowed-fn-param" 2,
    UninitializedVar "uninitialized-var" 2,
    DoTemplate "do-template" 2,
    SingleOperandComparison "single-operand-comparison" 2,
    LockingSuspiciousLock "locking-suspicious-lock" 2,
    UnsortedRequiredNamespaces "unsorted-required-namespaces" 0,
    NotEmpty "not-empty?" 2,
    CaseQuotedTest "case-quoted-test" 2,
    CaseSymbolTest "case-symbol-test" 0,
    DuplicateRefer "duplicate-refer" 2,
    SelfRequiringNamespace "self-requiring-namespace" 2,
    UnderscoreInNamespace "underscore-in-namespace" 2,
    NamespaceNameMismatch "namespace-name-mismatch" 3,
    ConflictingAlias "conflicting-alias" 3,
    ConflictingFnArity "conflicting-fn-arity" 3,
    DuplicateField "duplicate-field" 3,
    RedundantDeclare "redundant-declare" 2,
    VarSameNameExceptCase "var-same-name-except-case" 2,
    UnreachableCode "unreachable-code" 2,
    UnboundDestructuringDefault "unbound-destructuring-default" 2,
    UnknownRequireOption "unknown-require-option" 2,
    DocstringBlank "docstring-blank" 2,
    MissingTestAssertion "missing-test-assertion" 2,
    SingleLogicalOperand "single-logical-operand" 2,
    RedundantExpression "redundant-expression" 2,
    NonArgVecReturnTypeHint "non-arg-vec-return-type-hint" 2,
    EarmuffedVarNotDynamic "earmuffed-var-not-dynamic" 2,
    UnresolvedExcludedVar "unresolved-excluded-var" 1,
    UnusedExcludedVar "unused-excluded-var" 1,
    DestructuredOrBindingOfSameMap "destructured-or-binding-of-same-map" 2,
    IsMessageNotString "is-message-not-string" 1,
    RedundantPrimitiveCoercion "redundant-primitive-coercion" 1,
    JavaStaticFieldCall "java-static-field-call" 3,
    ProtocolMethodVarargs "protocol-method-varargs" 3,
    DuplicateKeyArgs "duplicate-key-args" 2,
    AwaitWithoutAsyncFn "await-without-async-fn" 3,
    MisplacedAsyncMetadata "misplaced-async-metadata" 2,
    RedundantLetBinding "redundant-let-binding" 0,
    ShadowedVar "shadowed-var" 0,
    IfNilReturn "if-nil-return" 0,
    RedundantCall "redundant-call" 0,
    EqualsFloat "equals-float" 0,
    KeywordBinding "keyword-binding" 0,
    DefFn "def-fn" 0,
    DiscouragedVar "discouraged-var" 2,
    EqualsExpectedPosition "equals-expected-position" 0,
    UnusedAlias "unused-alias" 0,
    DeprecatedNamespace "deprecated-namespace" 2,
    MultipleAsyncInDeftest "multiple-async-in-deftest" 2,
    ReduceWithoutInit "reduce-without-init" 0,
    MissingDocstring "missing-docstring" 0,
    DocstringNoSummary "docstring-no-summary" 0,
    UnsortedImports "unsorted-imports" 0,
    Refer "refer" 0,
    SingleKeyIn "single-key-in" 0,
    ConditionalBuildUp "conditional-build-up" 0,
    MainWithoutGenClass "main-without-gen-class" 0,
    UnusedIgnore "unused-ignore" 0,
    RedundantIgnore "redundant-ignore" 1,
    Hook "hook" 3,
}

#[derive(Clone, Debug)]
pub struct Finding {
    pub ty: FType,
    pub pos: Pos,
    pub msg: String,
    /// Element language tag (`L_*`); only meaningful for cljc files.
    pub lang: u8,
    /// Emit `lang`/`cljc`/`langs` even for non-cljc files (findings registered with an explicit language).
    pub explicit_lang: bool,
    /// Level override (namespace-local config); 0 = the file's level for the type.
    pub level: u8,
    /// Emit `row`/`col`/`end-row`/`end-col` as explicit nulls (location keys present but nil).
    pub null_pos: bool,
    /// Extra keys with pre-rendered JSON values (`ns`, `refer`, ...).
    pub extra: Vec<(&'static str, String)>,
}

impl Finding {
    pub fn new(ty: FType, pos: Pos, msg: impl Into<String>) -> Finding {
        Finding { ty, pos, msg: msg.into(), lang: 0, explicit_lang: false, level: 0, null_pos: false, extra: Vec::new() }
    }
    pub fn with(mut self, k: &'static str, json: String) -> Finding {
        self.extra.push((k, json));
        self
    }
}

impl super::Config {
    /// Level of a linter (0 = off).
    #[inline]
    pub fn level(&self, ty: FType) -> u8 {
        self.levels.get(ty as usize).copied().unwrap_or_else(|| ty.default_level())
    }
}

/// Write the findings of a file as oracle JSON objects (comma separated).
pub fn emit_findings(out: &mut String, fa: &super::types::FileAnalysis) {
    use super::expr::json_str;
    use super::types::*;
    use std::fmt::Write;
    let level = |t: FType| fa.lint_levels.get(t as usize).copied().unwrap_or_else(|| t.default_level());
    let cljc = fa.base_lang == Some(BaseLang::Cljc);
    let mut first = true;
    for (f, langs) in finish::finalize(fa.base_lang, &fa.findings) {
        if !first {
            out.push(',');
        }
        first = false;
        let _ = write!(out, "{{\"type\":\"{}\",\"level\":\"{}\"", f.ty.name(), level_name(if f.level != 0 { f.level } else { level(f.ty) }));
        if f.pos.row != 0 {
            let _ = write!(out, ",\"row\":{},\"col\":{}", f.pos.row, f.pos.col);
        } else if f.null_pos {
            out.push_str(",\"row\":null,\"col\":null");
        }
        if f.pos.end_row != 0 {
            let _ = write!(out, ",\"end-row\":{},\"end-col\":{}", f.pos.end_row, f.pos.end_col);
        } else if f.null_pos {
            out.push_str(",\"end-row\":null,\"end-col\":null");
        }
        let _ = write!(out, ",\"message\":{}", json_str(&f.msg));
        let lname = |l: u8| if l == L_CLJS { "cljs" } else { "clj" };
        if cljc {
            let _ = write!(out, ",\"cljc\":true,\"lang\":\"{}\"", lname(f.lang));
        } else if f.explicit_lang {
            let _ = write!(out, ",\"cljc\":null,\"lang\":\"{}\"", lname(f.lang));
        }
        let langs: Vec<u8> = if f.explicit_lang && !cljc { vec![f.lang] } else { langs };
        out.push_str(",\"langs\":[");
        for (i, l) in langs.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            let _ = write!(out, "\"{}\"", lname(*l));
        }
        out.push(']');
        for (k, v) in &f.extra {
            let _ = write!(out, ",\"{}\":{}", k, v);
        }
        out.push('}');
    }
}

/// A registered binding (kondo namespace `:bindings`), indexed by local id.
#[derive(Clone, Copy)]
pub struct LBind {
    pub active: bool,
    pub used: bool,
    pub skip: bool,
    pub name: crate::intern::SymId,
    pub pos: Pos,
    pub lang: u8,
}

impl LBind {
    pub const EMPTY: LBind = LBind { active: false, used: false, skip: false, name: crate::intern::SymId::NONE, pos: Pos { row: 0, col: 0, end_row: 0, end_col: 0 }, lang: 0 };
}

/// Arities of a local fn.
#[derive(Clone, Copy)]
pub struct ArInfo {
    pub fixed: super::types::Arities,
    pub varargs_min: Option<u32>,
}

#[derive(Default)]
pub struct LintState {
    /// Non-keyword binding tags (`rets::Rt`), indexed by tag id - `rets::RT_BASE`.
    pub rtab: Vec<rets::Rt>,
    /// Return tags of the defns of this file, by (ns, name).
    pub fn_rets: crate::analyzer::defs::FastMap<(crate::intern::SymId, crate::intern::SymId), Vec<rets::FnRet>>,
    /// Conditions whose tag contains an unresolved call.
    pub deferred: Vec<rets::Deferred>,
    /// Name of the defn whose arity body is analyzed next (consumed by `analyze_fn_body`).
    pub ret_target: Option<crate::intern::SymId>,
    /// Tail expression of the defn body being analyzed (a `let` there records its body tag in `let_rets`).
    pub tail_node: Option<NodeId>,
    pub let_rets: crate::analyzer::defs::FastMap<NodeId, Option<rets::Rt>>,
    /// Some binding of this file carries a type tag (symbol args can be tagged).
    pub tagged_any: bool,
    /// Union tags of let-bound locals (tag ids `UNION_BASE + index`).
    pub utab: Vec<Vec<types::Kw>>,
    /// Names bound by the destructuring map whose `:or` defaults are being analyzed.
    pub undefined_locals: Vec<crate::intern::SymId>,
    /// Written head of the call being analyzed (for hof findings).
    pub written_head: Option<(crate::intern::SymId, crate::intern::SymId)>,
    pub hof_head: Option<(crate::intern::SymId, crate::intern::SymId)>,
    /// Config of the current namespace when it has `:clj-kondo/config` metadata (kondo local config).
    pub cur_cfg: Option<std::rc::Rc<super::Config>>,
    /// Index of the namespace being linted in `lint_ns_end`.
    pub ns_cursor: usize,
    /// Hot-path config summary (`uses::CF_*`).
    pub cfg_flags: u8,
    /// Start position of the condition expression of the enclosing `if`/`when`/... (kondo `:condition`), (0,0) = none.
    pub cond_pos: (u32, u32),
    /// Start position of the innermost enclosing call expression (kondo frame meta), (0,0) = none.
    pub call_pos: (u32, u32),
    /// Last namespace marked used (cheap dedupe of `lint_use_ns`).
    pub last_used_ns: Option<(usize, crate::intern::SymId)>,
    /// Arity info of local fns referenced by `Binding::ar`.
    pub arities: Vec<ArInfo>,
    /// Param names seen while extracting the bindings of one fn arglist (kondo `:fn-dupes`).
    pub fn_dupes: Option<Vec<crate::intern::SymId>>,
    /// Var records for redefined-var, by (namespace, name).
    pub vars: crate::analyzer::defs::FastMap<(crate::intern::SymId, crate::intern::SymId), forms::VarRec>,
    /// The call enclosing the call being analyzed is generated (kondo `(:clj-kondo.impl/generated (meta parent-call))`).
    pub parent_gen: bool,
    /// Keyword tests of `cond->` expansions (kondo `:cond-arrow-test`).
    pub cond_arrow: Vec<NodeId>,
    /// The call being analyzed was mapped by `:lint-as` (kondo `lint-as?`).
    pub lint_as_call: bool,
    /// The next `analyze_fn` is a protocol method implementation (kondo `:protocol-fn`).
    pub protocol_next: bool,
    /// Per fn/loop: `recur` seen (kondo `:seen-recur?` volatiles).
    pub seen_recur: Vec<bool>,
    /// Set by `usage_symbol` when the symbol is its own `:qualify-ns` entry (e.g. a bare namespace name).
    pub qualify_self: bool,
    pub ns: Vec<nsl::LNs>,
    pub binds: Vec<LBind>,
    /// `:or` destructuring defaults: (key pos, binding ids reading the key, lang).
    pub defaults: Vec<(Pos, Vec<u32>, u8)>,
}

use super::{Analyzer, NodeId};

impl<'a> Analyzer<'a> {
    /// Register a finding if its linter is on (kondo `findings/reg-finding!`).
    pub fn lint(&mut self, ty: FType, pos: Pos, msg: impl Into<String>) -> Option<&mut Finding> {
        if !self.lon || self.lc().level(ty) == OFF || self.ctx.off & off_bit(ty) != 0 {
            return None;
        }
        let mut f = Finding::new(ty, pos, msg);
        f.lang = self.ltag;
        if self.lt.cur_cfg.is_some() {
            f.level = self.lc().level(ty);
        }
        self.out.findings.push(f);
        self.out.findings.last_mut()
    }

    /// kondo `namespace/reg-binding!`.
    pub fn lint_reg_binding(&mut self, id: u32, name: crate::intern::SymId, tok: NodeId, skip: bool) {
        if !self.lon {
            return;
        }
        let i = id as usize;
        if self.lt.binds.len() <= i {
            self.lt.binds.resize(i + 1, LBind::EMPTY);
        }
        let pos = self.pos(tok);
        self.lt.binds[i] = LBind { active: true, used: false, skip, name, pos, lang: self.ltag };
    }

    /// kondo `namespace/reg-used-binding!`.
    #[inline]
    pub fn lint_use_binding(&mut self, id: u32) {
        if let Some(b) = self.lt.binds.get_mut(id as usize) {
            b.used = true;
        }
    }

    pub fn lint_used(&self, id: u32) -> bool {
        self.lt.binds.get(id as usize).map_or(false, |b| b.used)
    }
    pub fn lint_set_used(&mut self, id: u32, v: bool) {
        if let Some(b) = self.lt.binds.get_mut(id as usize) {
            b.used = v;
        }
    }

    /// kondo `namespace/reg-destructuring-default!`: one default per `:or` key, bindings = those of the form named like the key.
    pub fn lint_default(&mut self, pos: Pos, key: crate::intern::SymId, mark: usize) {
        if !self.lon {
            return;
        }
        let ids: Vec<u32> = self.bindings[mark..].iter().filter(|b| b.key == key && b.id != 0).map(|b| b.id).collect();
        if !ids.is_empty() {
            self.lt.defaults.push((pos, ids, self.ltag));
        }
    }

    /// End of a language pass: kondo `linters/lint-bindings!` etc.
    pub fn lint_end(&mut self) {
        if !self.lon {
            return;
        }
        self.lint_deferred();
        self.lint_unused_bindings();
        self.lint_ns_end();
        self.lint_unused_private_vars();
    }

    /// kondo `lint-unused-bindings!`.
    fn lint_unused_bindings(&mut self) {
        if self.lc().level(FType::UnusedBinding) == OFF {
            return;
        }
        let binds = std::mem::take(&mut self.lt.binds);
        for b in &binds {
            if b.active && !b.used && !b.skip && !b.name.as_str().starts_with('_') {
                self.lint(FType::UnusedBinding, b.pos, format!("unused binding {}", b.name.as_str()));
            }
        }
        let defaults = std::mem::take(&mut self.lt.defaults);
        for (pos, ids, lang) in &defaults {
            if ids.iter().all(|&i| !binds[i as usize].used) {
                let nm = binds[ids[0] as usize].name;
                let saved = self.ltag;
                self.ltag = *lang;
                self.lint(FType::UnusedBinding, *pos, format!("unused default for binding {}", nm.as_str()));
                self.ltag = saved;
            }
        }
        self.lt.binds = binds;
    }
}

/// Bit of `Ctx::off` that disables a finding type (kondo `ctx-with-linter-disabled`).
#[inline]
pub fn off_bit(ty: FType) -> u16 {
    match ty {
        FType::UnresolvedSymbol => uses::OFF_SYM,
        FType::InvalidArity => uses::OFF_ARITY,
        FType::UnresolvedNamespace => uses::OFF_NS,
        FType::PrivateCall => uses::OFF_PRIV,
        FType::NotAFunction => uses::OFF_NOTFN,
        FType::UnresolvedVar => uses::OFF_VAR,
        FType::TypeMismatch => uses::OFF_TYPE,
        _ => 0,
    }
}

/// Reader problems as `syntax` findings (row/col only, like kondo's parse errors).
/// Returns true for errors after which kondo produces no analysis at all (unterminated string).
pub fn reader_errors(cst: &crate::cst::Cst, cfg: &super::Config, fa: &mut super::types::FileAnalysis) -> bool {
    let fatal = cst.errors().iter().any(|e| e.msg.starts_with("Unexpected EOF while reading string"));
    if cfg.level(FType::Syntax) == OFF {
        return fatal;
    }
    // kondo `utils/lint-unreachable-reader-conditional!`
    if cfg.level(FType::UnreachableCode) != OFF && fa.base_lang == Some(super::types::BaseLang::Cljc) && cst.src().contains("#?") {
        for i in 0..cst.len() {
            let n = crate::cst::NodeId(i as u32);
            if cst.kind(n) != crate::cst::Kind::ReaderCond {
                continue;
            }
            let Some(list) = cst.children(n).iter().copied().find(|&x| cst.kind(x) != crate::cst::Kind::Uneval) else { continue };
            let kids: Vec<crate::cst::NodeId> = cst.children(list).iter().copied().filter(|&x| cst.kind(x) != crate::cst::Kind::Uneval).collect();
            let mut j = 0;
            while j + 1 < kids.len() {
                let k = kids[j];
                if cst.kind(k) == crate::cst::Kind::Keyword && cst.ns(k).is_none() && cst.name(k).as_str() == "default" && j + 2 < kids.len() {
                    for lang in [super::types::L_CLJ, super::types::L_CLJS] {
                        let mut f = Finding::new(FType::UnreachableCode, cst.pos(k), "Unreachable code: default reader conditional branch should go last");
                        f.lang = lang;
                        fa.findings.push(f);
                    }
                }
                j += 2;
            }
        }
    }
    for e in cst.errors() {
        if fatal && !e.msg.starts_with("Unexpected EOF while reading string") {
            continue;
        }
        let f = Finding::new(FType::Syntax, Pos { row: e.row, col: e.col, end_row: 0, end_col: 0 }, e.msg.clone());
        fa.findings.push(f);
    }
    fatal
}

/// Linters run for internal (project) analysis; `NX_NOLINT=1` switches them off (measurements).
pub fn lint_enabled(internal: bool) -> bool {
    use std::sync::OnceLock;
    static OFF: OnceLock<bool> = OnceLock::new();
    internal && !*OFF.get_or_init(|| std::env::var_os("NX_NOLINT").is_some())
}

impl LintState {
    pub fn new(cfg: &super::Config) -> LintState {
        let mut s = LintState::default();
        s.cfg_flags = LintState::flags_of(cfg);
        s
    }

    pub fn flags_of(cfg: &super::Config) -> u8 {
        let mut f = 0;
        if cfg.linter_cfg(FType::InvalidArity).map_or(false, |c| !c.skip_args.is_empty()) {
            f |= uses::CF_SKIP_ARITY;
        }
        if cfg.linter_cfg(FType::UnresolvedVar).map_or(false, |c| !c.excl_ns.is_empty() || !c.excl_vars.is_empty()) {
            f |= uses::CF_VAR_EXCL;
        }
        if cfg.level(FType::DiscouragedVar) != OFF && cfg.linter_cfg(FType::DiscouragedVar).map_or(false, |c| !c.disc.is_empty()) {
            f |= uses::CF_DISC;
        }
        f
    }
}

impl<'a> Analyzer<'a> {
    /// Config in effect for the current namespace (local `:clj-kondo/config` over the project config).
    #[inline]
    pub fn lc(&self) -> &super::Config {
        match &self.lt.cur_cfg {
            Some(c) => c,
            None => self.cfg,
        }
    }

    /// Install the namespace-local config found in the `ns` form metadata (or reset it).
    pub fn lint_ns_local_config(&mut self, name_node: Option<NodeId>, meta_node: Option<NodeId>) {
        if !self.lon {
            return;
        }
        let mut found: Option<NodeId> = None;
        let mut scan = |a: &Self, m: NodeId| {
            if a.kind(m) == crate::cst::Kind::Map {
                let kids: Vec<NodeId> = a.c.sig_children(m).collect();
                let mut i = 0;
                while i + 1 < kids.len() {
                    let k = kids[i];
                    if a.kind(k) == crate::cst::Kind::Keyword && a.c.ns(k).as_str() == "clj-kondo" && a.c.name(k).as_str() == "config" {
                        found = Some(kids[i + 1]);
                    }
                    i += 2;
                }
            }
        };
        if let Some(n) = name_node {
            let mut cur = n;
            while let Some((m, t)) = self.c.meta(cur) {
                scan(self, m);
                cur = t;
            }
        }
        if let Some(m) = meta_node {
            scan(self, m);
        }
        let node = found.map(|v| if self.kind(v) == crate::cst::Kind::Quote { self.c.nth(v, 0).unwrap_or(v) } else { v });
        let ns_name = self.cur_ns_name();
        let in_ns: Vec<String> = self.cfg.in_ns.get(&ns_name).cloned().unwrap_or_default();
        self.lt.cur_cfg = match node {
            Some(n) if self.kind(n) == crate::cst::Kind::Map => {
                let mut cfg = (*self.cfg).clone();
                for t in &in_ns {
                    cfg.merge_edn(t);
                }
                cfg.merge_map(&self.c, n);
                self.lt.cfg_flags = LintState::flags_of(&cfg);
                Some(std::rc::Rc::new(cfg))
            }
            _ if !in_ns.is_empty() => {
                let mut cfg = (*self.cfg).clone();
                for t in &in_ns {
                    cfg.merge_edn(t);
                }
                self.lt.cfg_flags = LintState::flags_of(&cfg);
                Some(std::rc::Rc::new(cfg))
            }
            _ => {
                self.lt.cfg_flags = LintState::flags_of(self.cfg);
                None
            }
        };
        // namespaces remember their config for the end-of-pass linters
        let cur = self.lt.cur_cfg.clone();
        let i = self.cur;
        if let Some(c) = &cur {
            let ns = self.cur_ns_name();
            self.out.lint_ns_levels.push((ns, c.levels.clone()));
        }
        self.lns_at(i).local = cur;
    }
}

/// Finding extra keys are `&'static str`; map a decoded key to a static one (unknown keys are leaked once).
pub fn static_key(k: &str) -> &'static str {
    const KNOWN: &[&str] = &["ns", "referred-ns", "refer", "refers", "duplicate-ns", "class", "name", "methods", "protocol-name", "protocol-ns", "linters", "user-meta"];
    if let Some(s) = KNOWN.iter().find(|s| **s == k) {
        return s;
    }
    Box::leak(k.to_owned().into_boxed_str())
}

#[cfg(test)]
mod size_tests {
    #[test]
    fn sizes() {
        eprintln!("Ctx={} Binding={} LUse={} VarUsage={} Finding={} LintState={}", std::mem::size_of::<super::super::Ctx>(), std::mem::size_of::<super::super::Binding>(), std::mem::size_of::<super::uses::LUse>(), std::mem::size_of::<super::super::types::VarUsage>(), std::mem::size_of::<super::Finding>(), std::mem::size_of::<super::LintState>());
    }
}

/// Final findings of an analyzed file (after `finish_usages`): cljc collapse, sort, dedupe; with their level
/// (1 info, 2 warning, 3 error) and the languages they apply to (`L_CLJ`/`L_CLJS`, empty for non-cljc files).
pub fn final_findings(fa: &super::types::FileAnalysis) -> Vec<(Finding, u8, Vec<u8>)> {
    finish::finalize(fa.base_lang, &fa.findings)
        .into_iter()
        .map(|(f, langs)| {
            let lv = if f.level != 0 { f.level } else { fa.lint_levels.get(f.ty as usize).copied().unwrap_or_else(|| f.ty.default_level()) };
            (f, lv, langs)
        })
        .collect()
}
