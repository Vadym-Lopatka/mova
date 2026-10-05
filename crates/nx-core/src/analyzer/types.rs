//! Output model: one fixed-size struct per kondo analysis element, names as `SymId`.
//! Absent optional values: `SymId::NONE`, `Val::NONE`, row 0 (positions), `NO_ARITY`.
use crate::cst::Pos;
use crate::intern::SymId;
pub use super::extras_types::*;

pub const NO_ARITY: u16 = u16::MAX;
/// Base language of a file.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BaseLang {
    Clj,
    Cljs,
    Cljc,
}
/// Per-element language tag (only emitted for cljc files).
pub const L_NONE: u8 = 0;
pub const L_CLJ: u8 = 1;
pub const L_CLJS: u8 = 2;

/// An arbitrary metadata value kept as its JSON text (e.g. `true`, `"1.2"`); `Val::NONE` when absent.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Val(pub SymId);
impl Val {
    pub const NONE: Val = Val(SymId::NONE);
    pub fn is_none(self) -> bool {
        self.0.is_none()
    }
}

/// Set of fixed arities 0..=63.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct Arities(pub u64);
impl Arities {
    pub fn add(&mut self, n: u32) {
        if n < 64 {
            self.0 |= 1u64 << n;
        }
    }
    pub fn has(self, n: u32) -> bool {
        n < 64 && self.0 & (1u64 << n) != 0
    }
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }
    pub fn iter(self) -> impl Iterator<Item = u32> {
        (0..64u32).filter(move |&i| self.0 & (1u64 << i) != 0)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct NsDef {
    pub pos: Pos,
    pub name_pos: Pos,
    pub name: SymId,
    pub doc: SymId,
    pub no_doc: Val,
    pub deprecated: Val,
    pub added: Val,
    pub author: Val,
    pub in_ns: bool,
    pub lang: u8,
}

#[derive(Clone, Copy, Debug)]
pub struct NsUsage {
    /// row/col of the libspec name; `name_*` fields equal it.
    pub name_pos: Pos,
    pub alias_pos: Pos,
    pub from: SymId,
    pub to: SymId,
    pub alias: SymId,
    pub lang: u8,
}

#[derive(Clone, Copy, Debug)]
pub struct VarDef {
    pub pos: Pos,
    pub name_pos: Pos,
    pub name: SymId,
    pub ns: SymId,
    pub defined_by: (SymId, SymId),
    pub defined_by_lint_as: (SymId, SymId),
    /// Range into `FileAnalysis::callstacks`.
    pub cs: (u32, u32),
    pub doc: SymId,
    /// Range into `FileAnalysis::strs` (`arglist-strs`); len 0 = absent.
    pub arglists: (u32, u32),
    pub fixed: Arities,
    pub has_fixed: bool,
    pub has_arglists: bool,
    pub declared: bool,
    pub varargs_min: u16,
    pub private: bool,
    pub macro_: bool,
    pub test: bool,
    pub deprecated: Val,
    pub added: Val,
    pub export: Val,
    pub protocol_name: SymId,
    pub protocol_ns: SymId,
    /// `meta` of the var: JSON object text restricted to configured keys (`{}` normally).
    pub meta: Val,
    /// potemkin `import-vars`: (imported ns, imported var).
    pub imported: (SymId, SymId),
    pub lang: u8,
}

#[derive(Clone, Copy, Debug)]
pub struct VarUsage {
    pub pos: Pos,
    pub name_pos: Pos,
    pub name: SymId,
    pub from: SymId,
    pub from_var: SymId,
    pub arity: u16,
    pub alias: SymId,
    pub refer: bool,
    pub defmethod: bool,
    pub derived: bool,
    pub derived_name: bool,
    pub dispatch_val_str: SymId,
    /// `testing` string of the clojure.test context (or NONE).
    pub ctx_testing: SymId,
    /// Namespace the name resolved to during analysis (`UNKNOWN_NS` when unresolved).
    pub resolved_ns: SymId,
    pub unresolved: bool,
    pub clojure_excluded: bool,
    pub lang: u8,
    // filled by `DefsIndex::finish_usages`:
    pub to: SymId,
    pub fixed: Arities,
    pub has_fixed: bool,
    pub varargs_min: u16,
    pub macro_: bool,
    pub private: bool,
    pub deprecated: Val,
    /// Base language and call language (for cljc lookup rules): `call_lang` 1 = clj, 2 = cljs.
    pub call_lang: u8,
    /// clojure-lsp `findings->analysis`: a var-usage made from an `unresolved-namespace` finding (`m.other/g`); kept at
    /// the tail of `var_usages`, rebuilt by every `finish_usages`, never emitted or put in the position index.
    pub synth: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct Local {
    pub id: u32,
    pub name: SymId,
    pub str_: SymId,
    pub pos: Pos,
    pub scope_end_row: u32,
    pub scope_end_col: u32,
    pub lang: u8,
}

#[derive(Clone, Copy, Debug)]
pub struct LocalUsage {
    /// 0 = absent (bindings without id, e.g. type-hint pseudo locals).
    pub id: u32,
    pub name: SymId,
    pub pos: Pos,
    pub name_pos: Pos,
    pub lang: u8,
}

/// Exact-size copy of `v` (always copies; `realloc` keeps the old block when a shrink is < 2x, so `shrink_to_fit`
/// alone leaves up to 50% of the bytes of every stored bucket). The store writer copies every stored bucket this way
/// (`keep`): survivors then live in pages owned by ONE thread, apart from worker scratch (CST, growth garbage), so
/// worker pages free completely and open / close churn allocates and frees on the same thread.
pub fn shrink_vec<T: Clone>(v: &mut Vec<T>) {
    if v.capacity() == 0 {
        return;
    }
    let mut c = Vec::with_capacity(v.len());
    c.extend_from_slice(v);
    *v = c;
}

impl FileAnalysis {
    /// Drop push-growth slack of every bucket (a stored analysis lives as long as the process).
    pub fn shrink(&mut self) {
        shrink_vec(&mut self.namespace_definitions);
        shrink_vec(&mut self.namespace_usages);
        shrink_vec(&mut self.var_definitions);
        shrink_vec(&mut self.var_usages);
        shrink_vec(&mut self.locals);
        shrink_vec(&mut self.local_usages);
        shrink_vec(&mut self.callstacks);
        shrink_vec(&mut self.strs);
        shrink_vec(&mut self.used_ns);
        shrink_vec(&mut self.refer_alls);
        shrink_vec(&mut self.findings);
        shrink_vec(&mut self.lint_levels);
        shrink_vec(&mut self.lint_uses);
        shrink_vec(&mut self.lint_protos);
        shrink_vec(&mut self.lint_ignores);
        shrink_vec(&mut self.lint_ralls);
        shrink_vec(&mut self.lint_ns_levels);
        shrink_vec(&mut self.lint_hofs);
        shrink_vec(&mut self.lint_disc);
        shrink_vec(&mut self.lint_tfind);
        shrink_vec(&mut self.keywords);
        shrink_vec(&mut self.symbols);
        shrink_vec(&mut self.protocol_impls);
        shrink_vec(&mut self.instance_invocations);
        shrink_vec(&mut self.java_class_usages);
        shrink_vec(&mut self.java_class_defs);
    }
    /// Copy every bucket into the keep heap (store writer thread).
    pub fn keep(&mut self) {
        shrink_vec(&mut self.namespace_definitions);
        shrink_vec(&mut self.namespace_usages);
        shrink_vec(&mut self.var_definitions);
        shrink_vec(&mut self.var_usages);
        shrink_vec(&mut self.locals);
        shrink_vec(&mut self.local_usages);
        shrink_vec(&mut self.callstacks);
        shrink_vec(&mut self.strs);
        shrink_vec(&mut self.used_ns);
        shrink_vec(&mut self.refer_alls);
        shrink_vec(&mut self.findings);
        shrink_vec(&mut self.lint_levels);
        shrink_vec(&mut self.lint_uses);
        shrink_vec(&mut self.lint_protos);
        shrink_vec(&mut self.lint_ignores);
        shrink_vec(&mut self.lint_ralls);
        shrink_vec(&mut self.lint_ns_levels);
        shrink_vec(&mut self.lint_hofs);
        shrink_vec(&mut self.lint_disc);
        shrink_vec(&mut self.lint_tfind);
        shrink_vec(&mut self.keywords);
        shrink_vec(&mut self.symbols);
        shrink_vec(&mut self.protocol_impls);
        shrink_vec(&mut self.instance_invocations);
        shrink_vec(&mut self.java_class_usages);
        shrink_vec(&mut self.java_class_defs);
    }
}

/// Everything the analyzer produces for one file. Each bucket is a plain vector; next buckets
/// (keywords, symbols, protocol-impls, java-*, instance-invocations) are added here by their modules.
#[derive(Default, Debug, Clone)]
pub struct FileAnalysis {
    pub base_lang: Option<BaseLang>,
    /// Mova dialect file (`Options::mova`): forward references to later defs are valid.
    pub mova: bool,
    pub namespace_definitions: Vec<NsDef>,
    pub namespace_usages: Vec<NsUsage>,
    pub var_definitions: Vec<VarDef>,
    pub var_usages: Vec<VarUsage>,
    pub locals: Vec<Local>,
    pub local_usages: Vec<LocalUsage>,
    pub callstacks: Vec<(SymId, SymId)>,
    pub strs: Vec<SymId>,
    pub next_local_id: u32,
    /// Whether var-definitions carry `callstack` (internal analysis only).
    pub has_callstack: bool,
    /// `(from ns, referred-all ns, excluded names)` for usages that resolve through `:refer :all`.
    /// Namespaces this file used (per kondo `:used-namespaces`); drives which cached namespaces load.
    pub used_ns: Vec<SymId>,
    pub refer_alls: Vec<(SymId, SymId, Vec<SymId>)>,
    /// kondo findings (linters), see `lint`.
    pub findings: Vec<super::lint::Finding>,
    /// Linter levels in effect (copied from `Config`, empty = defaults).
    pub lint_levels: Vec<u8>,
    /// Lint info per var usage (parallel to `var_usages`).
    pub lint_uses: Vec<super::lint::uses::LUse>,
    /// Registered protocol implementations (see `lint::proto`).
    pub lint_protos: Vec<super::lint::proto::LProto>,
    /// Ignore markers (`#_:clj-kondo/ignore`), applied at the end of `finish_usages`.
    pub lint_ignores: Vec<super::lint::ignore::IgnoreRegion>,
    /// `:refer :all` / `:use` records awaiting usage resolution.
    pub lint_ralls: Vec<super::lint::nsl::RAllRec>,
    /// Levels of namespaces with local config (`:clj-kondo/config`), by namespace.
    pub lint_ns_levels: Vec<(SymId, Vec<u8>)>,
    /// Written head symbol of the call enclosing each hof usage, by `lint_uses` index.
    pub lint_hofs: Vec<(u32, (SymId, SymId))>,
    /// Pending `:discouraged-var` findings by `lint_uses` index (ascending).
    pub lint_disc: Vec<(u32, super::lint::uses::DiscRec)>,
    /// Type-mismatch findings by usage index, emitted unless the call has an arity error.
    pub lint_tfind: Vec<(u32, super::lint::Finding)>,
    pub keywords: Vec<Keyword>,
    pub symbols: Vec<SymbolUse>,
    pub protocol_impls: Vec<ProtocolImpl>,
    pub instance_invocations: Vec<InstanceInvocation>,
    pub java_class_usages: Vec<JavaUsage>,
    pub java_class_defs: Vec<JavaClassDef>,
}
