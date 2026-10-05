//! Fixed-size element structs of the extra buckets (keywords, symbols, protocol-impls, java-*, instance-invocations).
use crate::cst::Pos;
use crate::intern::SymId;

pub const KW_AUTO: u8 = 1;
pub const KW_PREFIX: u8 = 2;
pub const KW_KEYS_DESTR: u8 = 4;
pub const KW_NS_MOD: u8 = 8;

/// kondo `reg-keyword-usage!`; `ns`/`alias`/`reg`/`from*` are `SymId::NONE` when absent.
#[derive(Clone, Copy, Debug)]
pub struct Keyword {
    pub pos: Pos,
    pub name: SymId,
    pub from: SymId,
    pub from_var: SymId,
    pub ns: SymId,
    pub alias: SymId,
    pub reg: SymId,
    pub lang: u8,
    pub flags: u8,
}

/// kondo `reg-symbol!` (quoted qualified symbols).
#[derive(Clone, Copy, Debug)]
pub struct SymbolUse {
    pub pos: Pos,
    pub name: SymId,
    pub symbol: SymId,
    pub to: SymId,
    pub from: SymId,
    /// 0 none (cljc emits per-element lang too), 1 clj, 2 cljs, 3 edn
    pub lang: u8,
}

/// kondo `reg-protocol-impl!`.
#[derive(Clone, Copy, Debug)]
pub struct ProtocolImpl {
    pub pos: Pos,
    pub name_pos: Pos,
    pub method_name: SymId,
    pub protocol_name: SymId,
    pub protocol_ns: SymId,
    pub impl_ns: SymId,
    pub defined_by: (SymId, SymId),
    pub defined_by_lint_as: (SymId, SymId),
    pub derived: bool,
}

/// kondo `reg-instance-invocation!`.
#[derive(Clone, Copy, Debug)]
pub struct InstanceInvocation {
    pub name_pos: Pos,
    pub method_name: SymId,
    pub derived: bool,
    pub lang: u8,
}

pub const JU_IMPORT: u8 = 1;
pub const JU_MARK_USED: u8 = 2;
pub const JU_SKIP: u8 = 4;
pub const JU_HAS_NAME: u8 = 8;
pub const JU_CLJC: u8 = 16;
pub const JU_CLJS: u8 = 32;

/// kondo `java/reg-class-usage!`. `pos.row == 0` means the location is absent; `call`: 0 nil, 1 false, 2 true.
#[derive(Clone, Copy, Debug)]
pub struct JavaUsage {
    pub pos: Pos,
    pub name_pos: Pos,
    pub class: SymId,
    pub method: SymId,
    pub branch: SymId,
    /// JSON text of `:tag` / `:user-meta` residue (NONE when absent).
    pub tag: SymId,
    pub call: u8,
    pub flags: u8,
}

/// kondo `java/reg-class-def!`: `flags` bit set over `JF_NAMES`.
#[derive(Clone, Debug)]
pub struct JavaClassDef {
    pub class: SymId,
    pub flags: u16,
}

/// Flag names in output (alphabetical) order.
pub const JF_NAMES: [(&str, u16); 15] = [
    ("abstract", 1),
    ("default", 2),
    ("final", 4),
    ("interface", 8),
    ("native", 16),
    ("non-sealed", 32),
    ("private", 64),
    ("protected", 128),
    ("public", 256),
    ("sealed", 512),
    ("static", 1024),
    ("strictfp", 2048),
    ("synchronized", 4096),
    ("transient", 8192),
    ("volatile", 16384),
];

pub fn jf_bit(name: &str) -> u16 {
    JF_NAMES.iter().find(|(n, _)| *n == name).map_or(0, |x| x.1)
}
/// Analyzer-side state of the extra buckets.
#[derive(Default)]
pub struct ExtraState {
    /// Dotted file path (kondo `filename` sans extension) for `namespace-name-mismatch`.
    pub file_dotted: Option<String>,
    /// Keys of a namespaced map -> resolved prefix (kondo `:prefix` on the key nodes).
    pub kw_prefix: super::defs::FastMap<crate::cst::NodeId, SymId>,
    /// Keyword name nodes of `s/def` (kondo `:reg`).
    pub kw_reg: super::defs::FastMap<crate::cst::NodeId, SymId>,
    /// `(new Foo ...)` whose class node is being analyzed (kondo `:constructor-expr`).
    pub ctor: Option<crate::cst::NodeId>,
    /// Analyzing an EDN file.
    pub edn: bool,
    /// Inside an `are` / `do-template` expansion: kondo marks every node there as generated.
    pub gen: bool,
    /// Nodes picked by a reader conditional (kondo `:branch` metadata; cljc only).
    pub branch: super::defs::FastSet<crate::cst::NodeId>,
    /// `^Tag (...)` list nodes -> the tag symbol (leaks into java-class-usages as `:tag` / `:user-meta`).
    pub list_tag: super::defs::FastMap<crate::cst::NodeId, SymId>,
}
