//! Linters driven by var usages: unresolved-symbol, unresolved-var, invalid-arity, private-call,
//! deprecated-var, unused-value (calls). kondo `linters/lint-var-usage`, `lint-resolved-call!`,
//! `lint-valid-call!`. Per usage, the analyzer records an `LUse` (this module); `check` runs in
//! `finish_usages` once the called var's definition is known.
use super::*;
use crate::analyzer::defs::{DefsIndex, VarInfo};
use crate::analyzer::expr::VarUsageArgs;
use crate::analyzer::resolve::class_name_p;
use crate::analyzer::types::*;
use crate::intern::SymId;

pub const F_HOF: u16 = 1;
pub const F_COMMENT: u16 = 2;
pub const F_CORE: u16 = 4;
pub const F_PRIV_ACC: u16 = 8;
pub const F_COND: u16 = 16;
/// `invalid-arity` disabled (ctx) or skipped by `:skip-args`.
pub const F_ARITY_OFF: u16 = 32;
/// unresolved-symbol disabled in this context (also disables unresolved-var).
pub const F_SYM_DISABLED: u16 = 64;
/// unresolved-symbol excluded by config.
pub const F_SYM_EXCL: u16 = 128;
/// unresolved-var excluded by config.
pub const F_VAR_EXCL: u16 = 256;
pub const F_PRIV_OFF: u16 = 512;
pub const F_GEN: u16 = 1024;
pub const F_REFER: u16 = 2048;
pub const F_USE: u16 = 4096;
/// A matching `:discouraged-var` entry exists (record in `FileAnalysis::lint_disc`).
pub const F_DISC: u16 = 8192;

/// Pending `:discouraged-var` finding of a usage; the arity filter needs the called var.
#[derive(Clone, Debug)]
pub struct DiscRec {
    pub level: u8,
    pub msg: String,
    pub arities: Option<Vec<i32>>,
    pub call: bool,
}

/// Contexts in which linters are disabled (`Ctx::off`).
/// `LintState::cfg_flags`: config options that some hot paths consult.
pub const CF_SKIP_ARITY: u8 = 1;
pub const CF_VAR_EXCL: u8 = 2;
pub const CF_DISC: u8 = 4;

pub const OFF_SYM: u16 = 1;
pub const OFF_ARITY: u16 = 2;
pub const OFF_NS: u16 = 4;
pub const OFF_PRIV: u16 = 8;
pub const OFF_NOTFN: u16 = 16;
pub const OFF_VAR: u16 = 32;
pub const OFF_TYPE: u16 = 64;

#[derive(Clone, Copy, Debug)]
pub struct LUse {
    /// Parent call frame (`(second callstack)` of the call).
    pub parent: (SymId, SymId),
    pub idx: u32,
    pub len: u32,
    pub in_def: SymId,
    /// The symbol as written.
    pub written: (SymId, SymId),
    pub flags: u16,
}

impl LUse {
    #[inline]
    pub fn has(&self, f: u16) -> bool {
        self.flags & f != 0
    }
}

impl<'a> Analyzer<'a> {
    /// Record lint info for the usage just pushed to `var_usages`.
    pub fn lint_reg_use(&mut self, a: &VarUsageArgs) {
        if !self.lon {
            return;
        }
        let mut f = 0u16;
        let ctx = self.ctx;
        if ctx.in_comment {
            f |= F_COMMENT;
        }
        if a.r.resolved_core {
            f |= F_CORE;
        }
        if ctx.private_access {
            f |= F_PRIV_ACC;
        }
        if ctx.off & OFF_ARITY != 0 {
            f |= F_ARITY_OFF;
        }
        if ctx.off & OFF_SYM != 0 || ctx.sq > 0 || std::mem::take(&mut self.lt.qualify_self) {
            f |= F_SYM_DISABLED;
        }
        if ctx.sq > 0 {
            f |= F_PRIV_ACC;
        }
        if ctx.off & OFF_PRIV != 0 {
            f |= F_PRIV_OFF;
        }
        if a.refer {
            f |= F_REFER;
        }
        if a.derived {
            f |= F_GEN;
        }
        if a.arity != NO_ARITY && a.name_pos.row == 0 {
            f |= F_HOF;
        }
        let is_call = a.arity != NO_ARITY;
        if self.lt.cond_pos == (a.pos.row, a.pos.col) && self.lt.cond_pos.0 != 0 {
            f |= F_COND;
        }
        if a.r.unresolved && !a.r.resolved_core {
            let own = if a.arity != NO_ARITY && a.r.found {
                let ns_star = if a.r.ns == crate::analyzer::syms().unknown_ns { self.cur_ns_name() } else { a.r.ns };
                Some((ns_star, a.r.name))
            } else {
                None
            };
            if self.sym_excluded(a.name, own) {
                f |= F_SYM_EXCL;
            }
        }
        if is_call && f & F_ARITY_OFF == 0 && self.skip_arity() {
            f |= F_ARITY_OFF;
        }
        if !a.r.unresolved && a.r.found && !a.r.resolved_core && self.lt.cfg_flags & CF_VAR_EXCL != 0 && self.var_excluded(a.r.ns, a.name) {
            f |= F_VAR_EXCL;
        }
        let parent = self.cs.last().copied().unwrap_or((SymId::NONE, SymId::NONE));
        if f & F_HOF != 0 {
            if let Some(h) = self.lt.hof_head.take() {
                let i = self.out.lint_uses.len() as u32;
                self.out.lint_hofs.push((i, h));
            }
        }
        if self.lt.cfg_flags & CF_DISC != 0 && !a.r.unresolved && a.r.found && !a.derived {
            if let Some(rec) = self.disc_rec(a, is_call && f & F_HOF == 0 && f & F_REFER == 0) {
                f |= F_DISC;
                self.out.lint_disc.push((self.out.lint_uses.len() as u32, rec));
            }
        }
        self.out.lint_uses.push(LUse { parent, idx: ctx.idx, len: ctx.len, in_def: ctx.in_def, written: a.written, flags: f });
    }

    /// kondo `namespace/lint-discouraged-var!` up to the arity filter.
    fn disc_rec(&self, a: &VarUsageArgs, call: bool) -> Option<DiscRec> {
        let cfg = self.lc().linter_cfg(FType::DiscouragedVar)?;
        let d = cfg.disc.get(&(a.r.ns, a.r.name))?;
        if d.off || d.positions & (if call { 1 } else { 2 }) == 0 {
            return None;
        }
        if let Some(l) = &d.langs {
            let me = if self.is_cljs() { "cljs" } else { "clj" };
            if !l.iter().any(|x| x == me) {
                return None;
            }
        }
        let msg = d.message.clone().unwrap_or_else(|| format!("Discouraged var: {}/{}", a.r.ns.as_str(), a.r.name.as_str()));
        Some(DiscRec { level: d.level, msg, arities: d.arities.clone(), call })
    }

    /// kondo `config/unresolved-symbol-excluded` (callstack = current frames).
    fn sym_excluded(&self, name: SymId, own: Option<(SymId, SymId)>) -> bool {
        use cfgl::Excl;
        let Some(c) = self.lc().linter_cfg(FType::UnresolvedSymbol) else { return false };
        let s = name.as_str();
        c.exclude.iter().any(|e| match e {
            Excl::Sym(x) => x.as_str() == s,
            Excl::Re(r) => r.is_match(s),
            Excl::Call(ns, nm, list) => (self.cs.iter().any(|&(cns, cname)| cns == *ns && cname == *nm) || own == Some((*ns, *nm))) && list.as_ref().map_or(true, |l| l.contains(&name)),
        })
    }

    pub fn skip_arity_pub(&self) -> bool {
        self.skip_arity()
    }

    /// `:skip-args` of invalid-arity: a frame of the callstack is one of the configured fq symbols.
    fn skip_arity(&self) -> bool {
        if self.lt.cfg_flags & CF_SKIP_ARITY == 0 {
            return false;
        }
        let Some(c) = self.lc().linter_cfg(FType::InvalidArity) else { return false };
        c.skip_args.iter().any(|fq| {
            let (ns, nm) = fq.as_str().split_once('/').unwrap_or(("", fq.as_str()));
            self.cs.iter().any(|&(cns, cname)| !cns.is_none() && cns.as_str() == ns && cname.as_str() == nm)
        })
    }

    /// kondo `config/unresolved-var-excluded`.
    fn var_excluded(&self, ns: SymId, name: SymId) -> bool {
        let Some(c) = self.lc().linter_cfg(FType::UnresolvedVar) else { return false };
        c.excl_ns.contains(&ns) || c.excl_vars.contains(&(ns, name))
    }
}

pub fn show_arities_pub(fixed: Arities, varargs: u16) -> String {
    show_arities(fixed, varargs)
}

fn show_arities(fixed: Arities, varargs: u16) -> String {
    let mut arities: Vec<u32> = fixed.iter().collect();
    let max_fixed = arities.last().copied();
    if varargs != NO_ARITY {
        if max_fixed != Some(varargs as u32) {
            arities.push(varargs as u32);
        }
        let v: Vec<String> = arities.iter().map(|a| a.to_string()).collect();
        format!("{} or more", v.join(", "))
    } else if arities.len() == 1 {
        arities[0].to_string()
    } else {
        let last = arities.pop().unwrap_or(0);
        let v: Vec<String> = arities.iter().map(|a| a.to_string()).collect();
        format!("{} or {}", v.join(", "), last)
    }
}

/// Everything `check` needs besides the usage.
pub struct CheckCtx<'a> {
    pub levels: &'a [u8],
    pub base: BaseLang,
    /// First namespace of the file (kondo `:top-ns`).
    pub top_ns: SymId,
    pub defs: &'a DefsIndex,
    pub disc: Option<&'a DiscRec>,
    /// Name of the imported var when the called var is a potemkin import.
    pub imp_name: Option<SymId>,
    /// Mova dialect: a var may be used before its definition in the file (globals are late-bound).
    pub mova: bool,
}

impl<'a> CheckCtx<'a> {
    #[inline]
    fn on(&self, t: FType) -> bool {
        self.levels.get(t as usize).copied().unwrap_or_else(|| t.default_level()) != OFF
    }
}

fn written_str(w: (SymId, SymId)) -> String {
    if w.0.is_none() {
        w.1.as_str().to_owned()
    } else {
        format!("{}/{}", w.0.as_str(), w.1.as_str())
    }
}

/// kondo `lint-resolved-call!` for one usage. `called` = the resolved definition (None when unknown).
pub fn check(out: &mut Vec<Finding>, cc: &CheckCtx, u: &VarUsage, lu: &LUse, called: Option<VarInfo>, lang: u8, hof_head: Option<(SymId, SymId)>) -> bool {
    let is_call = !lu.has(F_REFER) && u.arity != NO_ARITY;
    let (unresolved_ns_dummy, unresolved) = (false, u.unresolved);
    let _ = unresolved_ns_dummy;
    let s = crate::analyzer::syms();
    let push = |out: &mut Vec<Finding>, ty: FType, pos: crate::cst::Pos, msg: String| {
        let mut f = Finding::new(ty, pos, msg);
        f.lang = lang;
        out.push(f);
    };
    let name_pos = if is_call && u.name_pos.row != 0 { u.name_pos } else { u.pos };
    // unresolved var
    let mut unresolved_var = false;
    if called.is_none() && u.pos.row != 0 && !lu.has(F_CORE) && !lu.has(F_COMMENT) && !unresolved && u.resolved_ns != s.unknown_ns && !u.resolved_ns.is_none() && cc.defs.ns_known(u.resolved_ns) {
        unresolved_var = true;
        let nm = u.name.as_str();
        if cc.on(FType::UnresolvedVar) && !lu.has(F_SYM_DISABLED) && !lu.has(F_VAR_EXCL) && !nm.starts_with('.') && !class_name_p(nm) {
            if lu.has(F_HOF) {
                // kondo: a hof usage carries the hof call expression: its message is the call head, without a location
                if let Some(h) = hof_head {
                    let mut f = Finding::new(FType::UnresolvedVar, crate::cst::Pos { row: 0, col: 0, end_row: 0, end_col: 0 }, format!("Unresolved var: {}", written_str(h)));
                    f.lang = lang;
                    f.null_pos = true;
                    out.push(f);
                }
            } else {
                push(out, FType::UnresolvedVar, name_pos, format!("Unresolved var: {}", written_str(lu.written)));
            }
        }
    }
    // valid-call?
    let valid_call = (lu.has(F_COMMENT) && called.is_some())
        || !unresolved
        || (called.is_some()
            && match cc.defs.def_pos(u.to, u.name) {
                Some((row, col, top)) => cc.mova || top != cc.top_ns || u.pos.row > row || (u.pos.row == row && u.pos.col > col),
                None => true,
            });
    if !unresolved_var && !valid_call && cc.on(FType::UnresolvedSymbol) && !lu.has(F_SYM_DISABLED) && !lu.has(F_SYM_EXCL) {
        let nm = u.name.as_str();
        if !(nm.len() > 1 && nm.starts_with('.')) && !class_name_p(nm) {
            push(out, FType::UnresolvedSymbol, name_pos, format!("Unresolved symbol: {}", nm));
        }
    }
    if !valid_call {
        return false;
    }
    let Some(v) = called else { return false };
    let mut arity_error = false;
    let fn_ns = u.to;
    let vn = cc.imp_name.unwrap_or(u.name);
    // arity
    if is_call && !lu.has(F_ARITY_OFF) && cc.on(FType::InvalidArity) && (u.has_fixed || u.varargs_min != NO_ARITY) && !u.fixed.has(u.arity as u32) && !(u.varargs_min != NO_ARITY && u.arity >= u.varargs_min) {
        let _ = v;
        arity_error = true;
        push(out, FType::InvalidArity, u.pos, format!("{}/{} is called with {} {} but expects {}", fn_ns.as_str(), vn.as_str(), u.arity, if u.arity == 1 { "arg" } else { "args" }, show_arities(u.fixed, u.varargs_min)));
    }
    // discouraged var
    if let Some(d) = cc.disc {
        let arity = if d.call && u.arity != NO_ARITY { Some(u.arity as i32) } else { None };
        let ok = match (&d.arities, arity) {
            (Some(l), Some(n)) => {
                let called = if u.has_fixed && u.fixed.has(n as u32) { Some(n) } else if u.varargs_min != NO_ARITY && n >= u.varargs_min as i32 { Some(-1) } else { None };
                called.map_or(false, |c| l.contains(&c))
            }
            _ => true,
        };
        if ok && !lu.has(F_GEN) {
            let mut f = Finding::new(FType::DiscouragedVar, u.pos, d.msg.clone());
            f.lang = lang;
            f.level = d.level;
            out.push(f);
        }
    }
    // private call
    if u.private && cc.on(FType::PrivateCall) && fn_ns != u.from && !lu.has(F_PRIV_ACC) && !lu.has(F_PRIV_OFF) {
        push(out, FType::PrivateCall, u.pos, format!("#'{}/{} is private", fn_ns.as_str(), vn.as_str()));
    }
    // unused value: a call of a pure core fn whose result is discarded
    if is_call && !lu.has(F_HOF) && !lu.has(F_COND) && !lu.has(F_GEN) && cc.on(FType::UnusedValue) {
        let ns = if fn_ns == s.cljs_core { s.clojure_core } else { fn_ns };
        if uval::unused_values().contains(&(ns, u.name)) && uval::call_unused(lu.parent, lu.idx, lu.len) {
            push(out, FType::UnusedValue, u.pos, "Unused value".to_owned());
        }
    }
    // deprecated
    if !u.deprecated.is_none() && cc.on(FType::DeprecatedVar) && !(fn_ns == u.from && u.name == lu.in_def) {
        let mut msg = format!("#'{}/{} is deprecated", fn_ns.as_str(), vn.as_str());
        let d = u.deprecated.0.as_str();
        if d != "true" {
            msg.push_str(&format!(" since {}", d.trim_matches('"')));
        }
        push(out, FType::DeprecatedVar, u.pos, msg);
    }
    arity_error
}
