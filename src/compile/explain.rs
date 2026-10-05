//! field1/W-EXPLAIN: tier-decision observability -- the "field report"
//! client's #1 wishlist item. A real program that silently tree-walks
//! (a dot-form buried in an otherwise-hot `fn`, metadata on a non-symbol
//! form, a `binding`/`with-redefs`) or whose hot `loop` silently stays the
//! generic node instead of `Ir::NumLoop` has, until this module, exactly one
//! observable symptom: missing wall-clock. This module turns every such
//! cliff into a five-minute fix by recording WHY, at the exact moment
//! `compile::resolve` decides, and surfacing it two ways:
//!
//! - `MOVA_EXPLAIN=1`: one `eprintln` line per bail/decline, at the moment
//!   it happens (see [`report_fn`]/[`report_loops`]). Zero cost when unset
//!   (`explain_enabled`'s `OnceLock`, same discipline as `MOVA_NO_NUMLOOP`
//!   in `compile::resolve`).
//! - `(compile-explain f)` (`builtins::meta`): a data structure describing
//!   the LAST compile decision made for a fn of `f`'s name, read back out of
//!   `Interp::compile_explain` -- see that field's doc for why a
//!   name-keyed registry, not a field on `Closure`, is this session's
//!   chosen storage (least invasive; documented as a v1 tradeoff).
//!
//! Building this must never change WHAT compiles -- only record WHY it
//! didn't. `compile::resolve` computes the SAME `Bail`/`LoopDecision` data
//! whether or not anyone ever reads it; this module is purely a consumer.

use crate::reader::{Form, FormValue, Span};
use crate::value::{Str, Value};

/// One `loop` form's `Ir::NumLoop` specialization decision, computed by
/// `compile::resolve::specialize_num_loop` regardless of whether anyone
/// reads it (recording is never allowed to perturb the decision itself).
pub enum LoopDecision {
    /// Specialized into `Ir::NumLoop`. `lanes` is true iff at least one
    /// interpreted lane variant was ALSO built on top of that (W1); if
    /// `lanes` is false, `superloop` is always false too (nothing to attach
    /// a shape-specialized superloop, W6, to). Per the task brief: only the
    /// NumLoop-vs-generic decision is guaranteed reported per loop --
    /// lane/superloop attachment is folded into the SAME record here
    /// because both are read straight off `NumLoop::lane_variants` after
    /// the fact, at negligible extra cost, rather than threaded as a
    /// separate reporting pass.
    Specialized { lanes: bool, superloop: bool },
    /// Stayed the generic `Ir::Loop`; `reason` is why the recognizer
    /// declined (see `compile::resolve::build_num_loop` and its helpers for
    /// the full catalog: binding count/shape, seed purity, body shape, test
    /// op, register overflow, unsupported expression node, ...).
    Generic { reason: &'static str },
}

/// One `loop` form's decision plus the source position of the `loop` form
/// itself, for the explain line / `compile-explain` record.
pub struct LoopExplain {
    pub span: Span,
    /// field5/W-SPAN: `Interp::source_id` at the moment this record was
    /// constructed (`compile::resolve`'s `compile_loop`) -- what lets
    /// `explain::at`/`builtins::meta::at_string` resolve `span` against the
    /// buffer it was actually read from, not whatever buffer is current
    /// when this record is LATER rendered. See `source_registry`'s module
    /// doc for the bug this replaces.
    pub source_id: u32,
    pub decision: LoopDecision,
}

/// Which tier a whole `fn` ended up on, and why.
pub enum FnTier {
    /// `compile::resolve::compile` bailed on some arity; the WHOLE fn
    /// tree-walks. `reason`/`span` are the INNERMOST cause (nested-fn bails
    /// are re-thrown with a `"nested fn: "`-prefixed reason but the
    /// original inner span -- see `compile_fn_expr`'s doc in
    /// `compile::resolve`).
    TreeWalk {
        reason: String,
        span: Span,
        /// field5/W-SPAN: same "stamped at construction, not at render"
        /// discipline as `LoopExplain::source_id` -- see that field's doc.
        source_id: u32,
        /// H1: the fn's own body text (first ~80 chars), independent of
        /// `span`/`source_id` -- a macro-expanded fn (e.g. a `(fn ...)`
        /// produced by a quasiquoted macro template) has every synthesized
        /// `Form` stamped with the MACRO CALL SITE's span (see
        /// `Interp::value_to_form_realized`), so `at(source_id, span)`
        /// above points at the call site, not the fn's real source. This
        /// is `None` unless `explain_enabled()` -- rendering it costs a
        /// `pr_str` walk of the fn body, which must not run on every
        /// tree-walked-fn bail in the hot path (e.g. the ~6000
        /// syntax-quote anon fns clj-kondo's tree-walker recompiles per
        /// instance).
        preview: Option<String>,
    },
    /// Every arity compiled. `loops` is one entry per `loop` form the
    /// `NumLoop` recognizer inspected anywhere in this fn (including nested
    /// `fn`s compiled as part of it) -- possibly empty, for a loop-free fn.
    ///
    /// `escapes` (field3/W-RESOLVE) is how many `Ir::Escape` nodes the fn
    /// emitted: interop forms that compiled to "hand this one form back to
    /// the tree-walker" rather than bailing the whole fn. Reporting it is
    /// the honesty condition on that node -- a fn with escapes did NOT
    /// compile cleanly, and EXPLAIN must not imply it did.
    Compiled { loops: Vec<LoopExplain>, escapes: usize },
}

/// One fn's whole compile-explain record.
pub struct FnExplain {
    pub name: Option<Str>,
    pub tier: FnTier,
}

/// True when `MOVA_EXPLAIN=1` was set at process start. Read exactly once,
/// same discipline as `compile::disabled_by_env`/`resolve::
/// num_loop_disabled_by_env`: the hot compile path never touches the
/// environment more than this.
pub(crate) fn explain_enabled() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| std::env::var("MOVA_EXPLAIN").is_ok_and(|v| v == "1"))
}

fn fn_label(name: &Option<Str>) -> String {
    match name {
        Some(n) => format!("'{n}'"),
        None => "<anonymous>".to_string(),
    }
}

/// H1: `MOVA_EXPLAIN=1`-only preview of a bailed fn's own params/body text,
/// truncated to 80 chars -- see `FnTier::TreeWalk::preview`'s doc for why
/// this exists (a macro-synthesized fn's `span` points at the macro CALL
/// site, not the fn's own source). Reconstructed from the fn's OWN parsed
/// `Arity` data (params + body forms), never from `span`, so it names the
/// real fn even when every `Form` in it carries a borrowed span.
pub(super) fn fn_preview(arities: &[crate::value::Arity]) -> String {
    let Some(a) = arities.first() else {
        return "(fn [])".to_string();
    };
    let sym_str = |s: &crate::value::Symbol| match &s.ns {
        Some(ns) => format!("{ns}/{}", s.name),
        None => s.name.to_string(),
    };
    let mut params: Vec<String> = a.params.iter().map(sym_str).collect();
    if let Some(r) = &a.rest {
        params.push("&".to_string());
        params.push(sym_str(r));
    }
    let mut s = format!("(fn [{}]", params.join(" "));
    for form in a.body.iter() {
        s.push(' ');
        s.push_str(&crate::printer::pr_str(&crate::reader::form_to_value(form)));
        if s.len() >= 80 {
            break;
        }
    }
    if s.len() > 80 {
        let mut end = 80;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
    s
}

/// field5/W-SPAN: thin wrapper over `source_registry::render_at` -- see
/// that fn's doc for the fallback behavior when `source_id` is 0/unknown.
fn at(interp: &crate::eval::Interp, source_id: u32, span: Span) -> String {
    crate::source_registry::render_at(interp, source_id, span)
}

/// `MOVA_EXPLAIN=1`'s one line per fn that tree-walks. A no-op (not even
/// the `OnceLock` check duplicated -- `record` already did it) when
/// `explain.tier` is `Compiled`, since a successful compile is not a cliff.
fn report_fn(interp: &crate::eval::Interp, explain: &FnExplain) {
    match &explain.tier {
        FnTier::TreeWalk { reason, span, source_id, preview } => eprintln!(
            "explain: fn {} tree-walks -- {reason} ({}){}",
            fn_label(&explain.name),
            at(interp, *source_id, *span),
            match preview {
                Some(p) => format!(" [{p}]"),
                None => String::new(),
            }
        ),
        // field3/W-RESOLVE: a clean compile stays silent (it is not a
        // cliff, and the W-ADX 18->1 bootstrap-noise cut must not be
        // regressed) -- but a compile carrying escapes is a partial one,
        // and saying nothing would be the dishonest option. ONE line, and
        // only for a fn that actually has an escape.
        FnTier::Compiled { escapes, .. } if *escapes > 0 => eprintln!(
            "explain: fn {} compiles with {escapes} interop escape{} -- each escaped form is tree-walked, the rest of the fn is compiled",
            fn_label(&explain.name),
            if *escapes == 1 { "" } else { "s" }
        ),
        FnTier::Compiled { .. } => {}
    }
}

/// `MOVA_EXPLAIN=1`'s one line per loop that stays generic. Specialized
/// loops are silent on stderr (only a bail/decline is a cliff worth
/// eprintln-ing); they are still recorded for `compile-explain` to read.
fn report_loops(interp: &crate::eval::Interp, explain: &FnExplain) {
    let FnTier::Compiled { loops, .. } = &explain.tier else {
        return;
    };
    for l in loops {
        if let LoopDecision::Generic { reason } = &l.decision {
            eprintln!(
                "explain: loop in {} stays generic -- {reason} ({})",
                fn_label(&explain.name),
                at(interp, l.source_id, l.span)
            );
        }
    }
}

/// Pre-expansion heads that create a `fn` body. A `loop`/`dotimes`/`while`
/// found INSIDE one of these compiles normally -- it's a candidate for
/// `Ir::NumLoop` via that fn's own `compile::resolve` pass, and any decline
/// is already covered by [`report_loops`] above -- so
/// [`find_top_level_loop`] must never descend into one.
fn is_fn_creating_head(name: &str) -> bool {
    matches!(name, "fn" | "fn*" | "defn" | "defn-" | "defmacro" | "letfn")
}

/// The three special-form heads whose body never compiles when they sit
/// directly at top level (outside any `fn`): only fn bodies ever reach
/// `compile::resolve` -- see `Interp::eval_form`'s own W4C doc for why a
/// bare top-level form runs as an interpreted "anonymous zero-arg invoke"
/// on the real JVM too, never a compiled one. `loop*` is included alongside
/// `loop` because it's the same real special form (`loop` desugars to it,
/// see `reflwarn.rs`'s own `"let" | "loop" | "loop*"` grouping); `for`/
/// `doseq` are deliberately excluded -- lazy/seq machinery, a different
/// story from the eager `NumLoop` recognizer this whole module exists to
/// explain.
fn is_top_level_loop_head(name: &str) -> bool {
    matches!(name, "loop" | "loop*" | "dotimes" | "while")
}

/// Syntactic, PRE-macroexpansion search for a `loop`/`dotimes`/`while` form
/// that is not nested inside any fn-creating form (see
/// [`is_fn_creating_head`]) -- the reflwarn.rs `walk`/`walk_list` pattern,
/// narrowed to exactly this one question. First hit wins (depth-first,
/// left-to-right over `items`/list contents, then vector/set elements, then
/// map key/value pairs) -- one explain line per top-level form is the
/// contract, not one per loop.
///
/// A user macro (`defn-switchable` & co) that expands to a fn-creating form
/// is NOT misclassified here: eligibility must be decided independent of
/// which macro produced the `fn`, but ACTUALLY macroexpanding to check is
/// off the table for a diagnostic scan that runs on every top-level form --
/// a macro is arbitrary user code (`resolve_symbol`/`apply_macro` can run
/// side effects, and some native macros build their expansion by directly
/// constructing runtime closures rather than pure code, so speculatively
/// expanding here was measured to double-create/double-report those
/// closures' own `MOVA_EXPLAIN` lines -- a worse dishonesty than the one
/// being fixed). So the classifier stays conservative instead: a call whose
/// head resolves to a `Value::Macro` (checked via [`ns::Interp::
/// resolve_symbol`], a pure lookup, never invoked) is UNKNOWN, not
/// syntactically loop-or-fn-creating -- and an unknown call is treated the
/// same as [`is_fn_creating_head`]'s "don't know, don't descend" rather
/// than walked into as plain data. That is strictly safer than today's
/// syntactic-only walk (a real top-level loop hidden inside some OTHER
/// macro's expansion was already invisible here, an accepted v1 gap -- see
/// below) and it eliminates the false positive: `defn-switchable`'s own
/// `loop` is never again reported as "top-level" merely because the
/// classifier didn't recognize the macro that wrapped it in a `defn`.
pub(crate) fn find_top_level_loop(interp: &crate::eval::Interp, env: &crate::env::Env, form: &Form) -> Option<Span> {
    match &form.value {
        FormValue::List(items) => {
            if let Some(first) = items.first() {
                if let FormValue::Atom(Value::Sym(s)) = &first.value {
                    if s.ns.is_none() {
                        let name = s.name.as_ref();
                        if is_top_level_loop_head(name) {
                            return Some(form.span);
                        }
                        if is_fn_creating_head(name) {
                            return None;
                        }
                    }
                    // Not a syntactically-recognized head -- if it resolves
                    // to a macro, its expansion is unknown (could create a
                    // fn scope, could be anything) so stop here rather than
                    // walk its raw, pre-expansion children as if they were
                    // ordinary call arguments. A plain function call (or an
                    // unresolved symbol) still descends as before.
                    if let Some(Value::Macro(_)) = interp.resolve_symbol(env, s) {
                        return None;
                    }
                }
            }
            items.iter().find_map(|it| find_top_level_loop(interp, env, it))
        }
        FormValue::Vector(items) | FormValue::Set(items) => {
            items.iter().find_map(|it| find_top_level_loop(interp, env, it))
        }
        FormValue::Map(pairs) => pairs
            .iter()
            .find_map(|(k, v)| find_top_level_loop(interp, env, k).or_else(|| find_top_level_loop(interp, env, v))),
        FormValue::Atom(_) => None,
    }
}

/// `MOVA_EXPLAIN=1`'s one line for the top-level-loop cliff: a hot `loop`/
/// `dotimes`/`while` sitting directly at top level (outside any `fn`) never
/// reaches `compile::resolve` at all -- only fn bodies ever attempt
/// compilation (see `compile::compile_fn`'s own entry point) -- so it stays
/// on the tree-walker forever, with none of `report_fn`/`report_loops`'s
/// machinery ever firing for it (there is no `FnExplain` to record). Called
/// from `Interp::eval_form`, the same non-recursive top-level hook
/// `reflwarn::analyze_top_level` uses, gated by [`explain_enabled`] alone
/// (NOT `*warn-on-reflection*` -- a different gate for a different
/// concern). Zero cost when the env var is unset: one `OnceLock` bool
/// check, then an immediate return, before this fn ever looks at `form`.
///
/// W-ADX item 4c: also silenced by `interp.suppress_explain` (set for the
/// duration of `load_core`/`load_core_async`/`load_core_flow`) so
/// core-bootstrap top-level loops never print -- see that field's own
/// doc.
pub(crate) fn report_top_level_loop(interp: &crate::eval::Interp, env: &crate::env::Env, form: &Form) {
    if !explain_enabled() || interp.suppress_explain {
        return;
    }
    if let Some(span) = find_top_level_loop(interp, env, form) {
        eprintln!(
            "explain: top-level loop never compiles -- only fn bodies compile; wrap it in a defn ({})",
            // Not a persisted struct -- rendered synchronously against
            // whatever buffer is current RIGHT NOW, which is correct here
            // (this fires from `Interp::eval_form`'s own per-top-level-form
            // hook, on the raw form it was just handed).
            at(interp, interp.source_id, span)
        );
    }
}

/// The one entry point, called from `compile::compile_fn` on EVERY
/// invocation (bail or success) -- see that fn's doc. Always stores into the
/// registry (cheap: one `HashMap` insert, only when the fn is named); only
/// prints to stderr when `MOVA_EXPLAIN=1` AND `interp.suppress_explain`
/// is false (W-ADX item 4c: silenced for the entire core.mova/async.mova/
/// flow.mova bootstrap so MOVA_EXPLAIN=1 shows only the USER's own fns --
/// see that field's doc). The registry insert below stays UNCONDITIONAL
/// either way, so `(compile-explain <core-fn>)` keeps working.
pub(super) fn record(interp: &mut crate::eval::Interp, explain: FnExplain) {
    if explain_enabled() && !interp.suppress_explain {
        report_fn(interp, &explain);
        report_loops(interp, &explain);
    }
    if let Some(name) = explain.name.clone() {
        interp.compile_explain.insert(name, explain);
    }
}
