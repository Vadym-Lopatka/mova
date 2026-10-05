//! Special forms: `def`, `set!`, `fn`, `defmacro`, `let`, `if`, `do`,
//! `loop`, `recur`, `quote`, `try`/`catch`/`finally`, `throw`, `ns`,
//! `macroexpand-1`/`macroexpand`. `quasiquote` itself is dispatched here but
//! implemented in `quasiquote.rs`.
//!
//! ## Destructuring (A3)
//!
//! `Interp::bind_pattern` is the one recursive engine behind every binding
//! site: `let`/`loop` bind their pattern(s) directly; `fn`/`defmacro`
//! params are desugared at *parse* time (`parse_single_arity`) instead --
//! any non-symbol parameter is replaced by a generated `__p<n>` symbol (so
//! `value.rs`'s `Arity::params: Vec<Symbol>` never has to change) and the
//! arity's body is wrapped in a synthesized `(let [<pattern> __p<n> ...]
//! body...)` that re-destructures on every call (including every `recur`
//! iteration, since the wrapper re-runs as part of the ordinary body).
//!
//! `loop`/`recur` needs its own twist: `recur` rebinds by *value*, not by
//! destructured name, so `eval_loop` introduces one internal `__loopN`
//! symbol per *binding pair* (not per name the pattern introduces) to hold
//! the raw value and re-runs `bind_pattern` against it every iteration --
//! this is what keeps `recur`'s arity check keyed on the number of loop
//! bindings even when one of them is a compound pattern like `[a & rest]`.

use std::sync::{Arc, Mutex, OnceLock};

use super::{form_as_symbol, Interp};
use crate::env::{Env, FrameGuard};
use crate::error::{ErrorKind, RjError};
use crate::reader::{Form, FormValue, Span};
use crate::value::{Arity, Closure, Keyword, PMap, PVec, Str, Symbol, Value};

// H2/fn-template-cache: the tree-walker creates a FRESH `Closure` (fresh
// parsed `arities`, fresh `CompileSlot`) every time it EVALUATES a `(fn
// ...)` literal -- so a fn literal evaluated N times (a loop body, or,
// dominant at clj-kondo scale, a `#(...)` inside a macro whose expansion is
// now cached -- see `eval::mod`'s macro-expansion cache -- and thus
// structurally IDENTICAL across all N expansions) pays parse + compile N
// times for work that only depends on the literal's own params/body, never
// on the creation env's VALUES: `resolve::compile`'s
// `debug_assert!(ctx.captures.is_empty())` proves the outermost-compiled-fn
// case (the only case reached from `eval_fn_form`) captures nothing by
// value -- free symbols become `Ir::CreationEnvLookup`, re-resolved against
// `Closure::env` at CALL time -- so the compiled result is safe to share
// across every instance.
//
// Key: `args.as_ptr()` (the fn's own param-vector+body Forms), for lookup
// speed only -- verified by full structural equality (`forms_equal`)
// against the cached `args_form`, so an address that was freed and reused
// for an unrelated form is just a cache miss (see `eval::mod`'s macro cache
// for the measured collision this same pattern was built to close: two
// sibling forms produced by one macro expansion share their outer span, so
// span-only keys are NOT safe here either).
struct FnTemplate {
    name: Option<Str>,
    arities: Arc<Vec<Arity>>,
    args_form: Vec<Form>,
    /// Settled at most once, on the 2nd+ sighting of this literal (or on
    /// the 1st under `force_eager`) -- see `eval_fn_form`.
    compiled: OnceLock<(Option<crate::compile::CompiledClosure>, u32)>,
}

const FN_TEMPLATE_CACHE_CAP: usize = 100_000;

fn fn_template_cache() -> &'static Mutex<std::collections::HashMap<usize, Arc<FnTemplate>>> {
    static CACHE: OnceLock<Mutex<std::collections::HashMap<usize, Arc<FnTemplate>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

fn fn_template_cache_get(args_ptr: usize, args: &[Form]) -> Option<Arc<FnTemplate>> {
    let guard = fn_template_cache().lock().ok()?;
    let t = guard.get(&args_ptr)?;
    if super::forms_equal(&t.args_form, args) {
        Some(t.clone())
    } else {
        None
    }
}

fn fn_template_cache_put(args_ptr: usize, args_form: Vec<Form>, name: Option<Str>, arities: Arc<Vec<Arity>>) -> Arc<FnTemplate> {
    let t = Arc::new(FnTemplate { name, arities, args_form, compiled: OnceLock::new() });
    if let Ok(mut guard) = fn_template_cache().lock() {
        if guard.len() < FN_TEMPLATE_CACHE_CAP || guard.contains_key(&args_ptr) {
            guard.insert(args_ptr, t.clone());
            TEMPLATE_SCOPE.with(|s| {
                if let Some(v) = s.borrow_mut().as_mut() {
                    v.push(args_ptr);
                }
            });
        }
    }
    t
}

thread_local! {
    /// Keys this thread added to the template cache since [`template_scope_begin`].
    static TEMPLATE_SCOPE: std::cell::RefCell<Option<Vec<usize>>> = const { std::cell::RefCell::new(None) };
}

/// nREPL memory: a REPL form's AST dies when the eval ends, but the cache keeps
/// a copy of every fn literal's forms for good (about 5-7 KB per `defn`). The
/// cache only pays off while the same literal is evaluated again (a loop), which
/// happens inside one top-level form. So the server opens a scope per form and
/// drops what the form added at the end. Live closures do not hold a template.
pub(crate) fn template_scope_begin() {
    TEMPLATE_SCOPE.with(|s| *s.borrow_mut() = Some(Vec::new()));
}

pub(crate) fn template_scope_end() {
    let keys = TEMPLATE_SCOPE.with(|s| s.borrow_mut().take()).unwrap_or_default();
    if keys.is_empty() {
        return;
    }
    if let Ok(mut guard) = fn_template_cache().lock() {
        for k in keys {
            guard.remove(&k);
        }
    }
}

/// Every fixed name [`Interp::eval_special`] recognizes, as data.
///
/// MUST stay in sync with that fn's `match` arms, in BOTH directions --
/// `special_form_names_match_dispatch` is the gate that makes "must" true:
/// it feeds every name here back through `eval_special`, AND re-scans that
/// fn's own source for head names this list is missing. The second half is
/// not belt-and-braces: a head added to `eval_special` and forgotten here
/// silently makes `` `that-head `` read as `<current-ns>/that-head` instead
/// of `clojure.core/that-head`, i.e. every macro that syntax-quotes it
/// breaks, with nothing failing. (It has already happened once: merging
/// main brought `reify` (D1) and `.` (D5) in while this list sat still.)
/// The two `.`-shaped guard arms (`(.field x)` / `(Ctor. args)`) are
/// deliberately NOT listed: they are pattern-shaped, not fixed names, and
/// the scanner skips them for the same reason.
///
/// The one consumer is `quasiquote`'s W3e-1 symbol resolution: real Clojure
/// implements most of these as ordinary `clojure.core` MACROS, so a
/// syntax-quoted `let`/`fn`/`ns`/`defrecord`/... resolves through the
/// `clojure.core` mapping and reads as `clojure.core/let` and friends. mova
/// implements them as structural dispatch with no var cell to find, so
/// there is nothing for the ordinary global lookup to hit -- this list is
/// what stands in for that mapping. (See `Interp::is_bare_or_core_alias`
/// for why emitting the `clojure.core/`-qualified spelling keeps working.)
pub(crate) const SPECIAL_FORM_NAMES: &[&str] = &[
    "def",
    "set!",
    "binding",
    "with-redefs",
    "fn",
    "fn*",
    "defmacro",
    "let",
    "letfn*",
    "if",
    "do",
    "loop",
    "recur",
    "quote",
    "var",
    "quasiquote",
    "try",
    "throw",
    "ns",
    "import",
    "macroexpand-1",
    "macroexpand",
    "defprotocol",
    "defrecord",
    "deftype",
    "definterface",
    // D1, arrived with main: `reify` is a `clojure.core` defmacro on the
    // real platform, so `` `reify `` reads as `clojure.core/reify`.
    "reify",
    "extend-type",
    "extend-protocol",
    // D5, arrived with main. Listed for the contract's sake only: bare `.`
    // IS one of Clojure's own special forms, so `quasiquote`'s
    // `is_syntax_quote_special` keeps it bare and never reaches this list
    // for it (measured: `` `. `` => `.`).
    ".",
    "new",
    "proxy",
    "defmulti",
    "defmethod",
];

/// True when `name` is one of [`SPECIAL_FORM_NAMES`].
pub(crate) fn is_special_form_name(name: &str) -> bool {
    SPECIAL_FORM_NAMES.contains(&name)
}

/// D5: is `name` one of the heads mova implements as a special form but
/// real Clojure implements as an ordinary `clojure.core` MACRO?
///
/// Only those may be shadowed by a namespace's own `def`/`defmacro` of the
/// same name, because on the real platform that shadowing is not a special
/// case at all -- it is just `(:refer-clojure :exclude (deftype))` plus an
/// ordinary def, and the compiler never had a claim on the name. The
/// vendored `clojure.pprint` does exactly that: `pretty_writer.clj`
/// defines a legacy `deftype` macro over `defstruct`, and every `(deftype
/// buffer-blob :data ...)` in that file must reach it.
///
/// Clojure's TRUE special forms are deliberately absent: `def`, `if`,
/// `do`, `let*`/`let`, `fn*`/`fn`, `loop*`/`loop`, `recur`, `quote`,
/// `var`, `try`, `throw`, `new`, `set!`, `.`-forms. Those cannot be
/// shadowed on the real platform either (the compiler checks the head
/// before it ever consults a namespace mapping), so allowing it here
/// would make mova MORE permissive than Clojure -- a divergence, not a
/// convenience. Everything listed below is a `defmacro` in
/// `clojure/core.clj` today, checked name by name against the oracle
/// source.
pub(crate) fn is_shadowable_special(name: &str) -> bool {
    matches!(
        name,
        "binding"
            | "with-redefs"
            | "defmacro"
            | "ns"
            | "import"
            | "macroexpand"
            | "macroexpand-1"
            | "defprotocol"
            | "defrecord"
            | "deftype"
            | "definterface"
            | "reify"
            | "extend-type"
            | "extend-protocol"
            | "proxy"
            | "defmulti"
            | "defmethod"
    )
}

impl Interp {
    /// D5: does the CURRENT namespace have its own binding for `sym`,
    /// shadowing a mova special form? See `is_shadowable_special` and
    /// `Interp::special_shadows`.
    ///
    /// W3e2 (merge reconciliation): only a BARE head can be shadowed. That
    /// is Clojure's own rule -- `(:refer-clojure :exclude (deftype))` plus
    /// an ordinary `def` rebinds what the plain name `deftype` MAPS to in
    /// that namespace, and says nothing about the fully-qualified
    /// `clojure.core/deftype`, which still names core's macro. Restricting
    /// the gate this way costs the D5 case nothing (vendored
    /// `clojure.pprint`'s `pretty_writer.clj` writes bare `(deftype
    /// buffer-blob ...)`), and it closes a HANG this branch would otherwise
    /// have introduced: W3e-2 gave every structural special form a bare
    /// `clojure.core` forwarding macro var whose expansion is the
    /// `clojure.core/`-qualified call. With a name-only gate, evaluating
    /// that expansion inside a shadowing namespace skipped `eval_special`,
    /// fell through to the macro lookup, found the forwarding macro again
    /// through `for_each_global_candidate`'s bare fallback, and expanded to
    /// itself forever.
    pub(crate) fn shadows_special(&self, sym: &Symbol) -> bool {
        sym.ns.is_none()
            && !self.special_shadows.is_empty()
            && self.special_shadows.contains(&(self.current_ns.clone(), sym.name.clone()))
    }

    /// D5: record that `qualified` shadows a special form, if it names one
    /// that may be shadowed. Called from `def` and `defmacro` -- the only
    /// two ways a namespace acquires a binding of its own.
    pub(crate) fn note_special_shadow(&mut self, qualified: &Symbol) {
        if is_shadowable_special(qualified.name.as_ref()) {
            if let Some(ns) = &qualified.ns {
                self.special_shadows.insert((ns.clone(), qualified.name.clone()));
            }
        }
    }

    /// Returns `Some(result)` if `name` is a recognized special form (the
    /// call has been fully handled), `None` if the caller should fall
    /// through to ordinary function/macro application.
    pub(super) fn eval_special(
        &mut self,
        name: &str,
        args: &[Form],
        span: Span,
        env: &Env,
    ) -> Option<Result<Value, RjError>> {
        match name {
            "def" => Some(self.eval_def(args, span, env)),
            "set!" => Some(self.eval_set_bang(args, span, env)),
            "binding" => Some(self.eval_binding(args, span, env)),
            "with-redefs" => Some(self.eval_with_redefs(args, span, env)),
            // `fn*` is Clojure's primitive fn special form; mova's `fn` IS
            // that primitive (no destructuring-macro layering on top needed
            // to reach it), so `fn*` is a plain alias -- same handler, same
            // semantics, both the tree-walk and compiled tiers (see
            // `compile_special` in resolve.rs for the other side).
            "fn" | "fn*" => Some(self.eval_fn_form(args, span, env)),
            "defmacro" => Some(self.eval_defmacro(args, span, env)),
            "let" => Some(self.eval_let(args, span, env, false)),
            "letfn*" => Some(self.eval_let(args, span, env, true)),
            "if" => Some(self.eval_if(args, span, env)),
            "do" => Some(self.eval_do_body(args, env)),
            "loop" => Some(self.eval_loop(args, span, env)),
            "recur" => Some(self.eval_recur(args, span, env)),
            "quote" => Some(self.eval_quote(args, span)),
            "var" => Some(self.eval_var(args, span)),
            "quasiquote" => Some(self.eval_quasiquote_top(args, span, env)),
            "try" => Some(self.eval_try(args, env)),
            "throw" => Some(self.eval_throw(args, span, env)),
            "ns" => Some(self.eval_ns(args, span)),
            // S4: `import` is a MACRO in real Clojure (its args are never
            // evaluated, only optionally `quote`-unwrapped -- see
            // `eval::types_forms::eval_import`'s doc), hence a special
            // form here rather than a native fn; `require` needs no such
            // treatment (real Clojure's `require` IS a plain function) and
            // is registered as an ordinary native instead
            // (`builtins::nsfns`).
            "import" => Some(self.eval_import(args, span, env)),
            "macroexpand-1" => Some(self.eval_macroexpand1(args, span, env)),
            "macroexpand" => Some(self.eval_macroexpand(args, span, env)),
            // S3 type system (eval::types_forms). Tree-walk only: the
            // compiled tier sees these heads as unresolvable symbols and
            // bails the whole fn to the tree-walker, which is the honest
            // v1 (type definitions and test bodies are not hot paths).
            "defprotocol" => Some(self.eval_defprotocol(args, span, env)),
            "defrecord" => Some(self.eval_defrecord(args, span, env)),
            "deftype" => Some(self.eval_deftype(args, span, env)),
            // S5: `definterface` -- a macro in real Clojure (its body is
            // unevaluated signature syntax), hence a special form here for
            // the same reason `import` is one.
            "definterface" => Some(self.eval_definterface(args, span, env)),
            // D1: `reify` -- an anonymous instance, built from the same
            // group-splitting/method-collecting pieces `deftype` uses
            // (see `eval::types_forms::eval_reify`'s doc). A special form
            // for the same reason those are: its body is unevaluated
            // implements-and-method syntax, not arguments.
            "reify" => Some(self.eval_reify(args, span, env)),
            "extend-type" => Some(self.eval_extend_type(args, span, env)),
            "extend-protocol" => Some(self.eval_extend_protocol(args, span, env)),
            // D5: `.` -- Clojure's ONE true interop special form, of which
            // `(.method x ...)`/`(.-field x)`/`(Class/static ...)` are
            // reader-level sugar. mova grew the sugar first and never the
            // form itself; the vendored `clojure.pprint` writes it out
            // longhand in three places (`dispatch.clj`'s `use-method`,
            // `pprint_base.clj`'s `binding-map`), so it is now a
            // first-class head. See `eval_dot_special`.
            "." => Some(self.eval_dot_special(args, span, env)),
            "new" => Some(self.eval_new(args, span, env)),
            // S5 (host-class shims): the ONE narrow `proxy` shape
            // `test.check`'s `random.clj` needs -- see `eval::types_forms::
            // eval_proxy`'s doc for the exact supported shape and why
            // nothing broader is implemented.
            "proxy" => Some(self.eval_proxy(args, span, env)),
            // S4 multimethods (eval::multi_forms). Tree-walk only, same
            // reasoning as the S3 type-system heads directly above.
            "defmulti" => Some(self.eval_defmulti(args, span, env)),
            "defmethod" => Some(self.eval_defmethod(args, span, env)),
            // `(.field x)` / `(Ctor. args)` symbol-shaped hooks -- only
            // when the spelling can't be anything else (a lone "." or
            // ".." is not a hook).
            other if other.len() > 1 && other.starts_with('.') && other != ".." => {
                Some(self.eval_dot_form(other, args, span, env))
            }
            // C7 (vecveneer): ".." also ENDS with '.', so without this
            // exclusion it fell through to the ctor-form hook below --
            // `eval_ctor_form` strips ONE trailing char (".." -> ".") and
            // tries to resolve the bare 1-char symbol "." as a class,
            // which is neither a class NOR anything else, so it produced
            // a confusing "Unable to resolve symbol: ." (a symbol the
            // user never wrote) instead of ever reaching a `..` macro.
            // `..` itself is a plain `core.mova` macro (see
            // `defmacro ..` there) -- this arm's job is only to make sure
            // it's never intercepted here first.
            other if other.len() > 1 && other.ends_with('.') && other != ".." => {
                Some(self.eval_ctor_form(other, args, span, env))
            }
            _ => None,
        }
    }

    fn eval_def(&mut self, args: &[Form], span: Span, env: &Env) -> Result<Value, RjError> {
        // `(def name "doc" value)` -- S5/M3: the docstring is no longer
        // discarded, it becomes the var's `:doc` metadata (measured:
        // `(do (def dv2 "docstr" 1) (:doc (meta (var dv2))))` is
        // `"docstr"`).
        let docless: [Form; 2];
        let mut docstring: Option<Value> = None;
        let args = if args.len() == 3
            && matches!(&args[1].value, FormValue::Atom(Value::Str(_)))
        {
            if let FormValue::Atom(s @ Value::Str(_)) = &args[1].value {
                docstring = Some(s.clone());
            }
            docless = [args[0].clone(), args[2].clone()];
            &docless[..]
        } else {
            args
        };
        if args.is_empty() || args.len() > 2 {
            return Err(self.err_here(
                RjError::arity(format!(
                    "def: expected 1 or 2 arguments, got {}",
                    args.len()
                )),
                span,
            ));
        }
        let sym = form_as_symbol(&args[0]).cloned().ok_or_else(|| {
            self.err_here(
                RjError::other("def: first argument must be a symbol"),
                args[0].span,
            )
        })?;
        // W-DECL: `(def name)` -- no init form at all -- is NOT `(def name
        // nil)`. Real Clojure's `Compiler.DefExpr` only calls
        // `Var.bindRoot` when the source actually wrote an init
        // expression; a bare `(def name)` on a fresh name interns the var
        // and leaves it genuinely UNBOUND (`Var$Unbound`), and on an
        // ALREADY-bound name touches nothing (existing root survives).
        // Before this fix `eval_def` always computed `value` (defaulting
        // an absent init to `Value::Nil`) and unconditionally `set` it --
        // so `(def name)` silently behaved like `(def name nil)`, which is
        // exactly why `declare` (a macro built entirely out of 1-arg
        // `def`, matching real `clojure.core/declare`'s own expansion)
        // needed its own bypass native (`--intern-unbound!`) instead of
        // just using `def` directly. This is what makes `declare` a
        // faithful, ordinary macro again -- see core/core.mova's own
        // comment on `declare`.
        let has_init = args.len() == 2;
        let value = if has_init {
            self.eval_form_in(&args[1], env)?
        } else {
            Value::Nil
        };
        // Interned as `current-ns/name` (`crate::ns`), so a def can never
        // clobber a bare builtin cell -- it shadows it for this namespace.
        let qualified = self.qualify_def(&sym);
        self.warn_if_def_shadows(&qualified);
        self.warn_if_non_dynamic_earmuff(&sym, &args[0], span, env)?;
        self.note_special_shadow(&qualified);
        if has_init {
            self.globals.set(qualified.clone(), value.clone());
        } else {
            // Intern-only: get-or-create the cell, never touching an
            // existing root value (bound or still unbound) and never
            // storing `Value::Nil` as a fake "unbound" marker the way the
            // old always-`set` path did.
            self.globals.intern(&qualified);
        }
        // S5/M3: the DEFINITION's `^{...}` (written on the name symbol)
        // becomes the VAR's `IReference` metadata -- this is what makes
        // `(do (def ^:private x 1) (:private (meta #'x)))` true,
        // measured. Note the direction: metadata written on the symbol
        // in source ends up on the var, NOT on the value (measured:
        // `(do (defn ^:foo f [] 1) (meta f))` is `nil` while
        // `(:foo (meta #'f))` is `true`).
        self.publish_var_meta(&qualified, &args[0], docstring, span, env)?;
        // Like Clojure, `def` returns the VAR (`#'user/x`), not the value.
        let _ = value;
        Ok(Value::Var(self.globals.intern(&qualified)))
    }

    /// W3e-4: real Clojure's `def`-shadows-a-referred-name warning.
    ///
    /// `clojure.test-clojure.rt/error-messages` measures it directly:
    /// `(defn prefers [] ...)` in a namespace that only sees `prefers`
    /// through `clojure.core` must print `WARNING: prefers already refers
    /// to: #'clojure.core/prefers in namespace: <ns>, being replaced by:
    /// #'<ns>/prefers` to `*err*`. The rule and the text are
    /// `Interp::shadow_warning`'s -- the SAME ones the `intern` native has
    /// used since S7, which is exactly right: on the JVM both routes go
    /// through `Namespace.intern` -> `checkReplacement`. The comment at
    /// that native's call site claiming this "matches `def`'s own
    /// core-shadowing warning" was aspirational until now; it is true.
    ///
    /// A `def` in `CORE_NS` interns bare (`qualify_def`), so `qualified.ns`
    /// is `None` there and this returns early -- core defining its own
    /// names is never a shadowing event.
    ///
    /// The channel is `write_shim_err` (see its doc): mova has no real
    /// `*err*` stream, so this reaches the vendored suite's shim-local
    /// dynamic `*err*` and is a silent no-op everywhere else.
    fn warn_if_def_shadows(&mut self, qualified: &Symbol) {
        let Some(ns_name) = qualified.ns.clone() else {
            return;
        };
        // Already interned HERE: a redefinition, which Clojure returns
        // early from without warning.
        if self.globals.find_bound_cell(qualified).is_some() {
            return;
        }
        // SPEC-W3: a name this namespace listed in `(:refer-clojure
        // :exclude [...])` is one the programmer has already said they
        // mean to replace, and real Clojure prints nothing for it (there
        // is no mapping left to warn about). See
        // `Interp::refer_clojure_excludes`.
        if self.refer_clojure_excluded(&ns_name, &qualified.name) {
            return;
        }
        let bare = Symbol::simple(qualified.name.clone());
        let prior = self.for_each_global_candidate(&bare, |cand| self.globals.find_bound_cell(cand));
        if let Some(w) = Interp::shadow_warning(&ns_name, &qualified.name, prior.as_ref()) {
            crate::builtins::nsfns::write_shim_err(self, &w);
        }
    }

    /// SPEC-W3: did `ns_name` list `name` in `(:refer-clojure :exclude
    /// [...])`? The `is_empty` guard keeps this free for every program
    /// that never excludes anything, exactly as `shadows_special`'s does.
    pub(crate) fn refer_clojure_excluded(&self, ns_name: &Str, name: &Str) -> bool {
        !self.refer_clojure_excludes.is_empty()
            && self
                .refer_clojure_excludes
                .contains(&(ns_name.clone(), name.clone()))
    }

    /// W4-SHIM: real Clojure's `def`-of-an-earmuffed-but-non-dynamic-name
    /// warning, transcribed verbatim from
    /// `.oracle/clojure-src/src/jvm/clojure/lang/Compiler.java`'s
    /// `DefExpr.Parser.parse`:
    ///
    /// ```java
    /// boolean isDynamic = RT.booleanCast(RT.get(mm, dynamicKey));
    /// if(isDynamic) v.setDynamic();
    /// if(!isDynamic && sym.name.startsWith("*") && sym.name.endsWith("*")
    ///     && sym.name.length() > 2)
    ///     RT.errPrintWriter().format("Warning: %1$s not declared dynamic
    ///     and thus is not dynamically rebindable, but its name suggests
    ///     otherwise. Please either indicate ^:dynamic %1$s or change the
    ///     name. (%2$s:%3$d)\n", sym, SOURCE_PATH.get(), LINE.get());
    /// ```
    ///
    /// The rule is on `sym.name` alone (the name AS WRITTEN, never the
    /// namespace-qualified spelling) with `length() > 2` -- this is why
    /// `**` (length 2) never warns while `*hello*` (length 8) does, the
    /// exact two rows `def.clj`'s `non-dynamic-warnings` deftest checks.
    /// Unlike `warn_if_def_shadows`, there is no "already interned here"
    /// early return: real Clojure re-warns on every redefinition of the
    /// same earmuffed name, since the check runs unconditionally on every
    /// `def`, never gated on whether the var already existed.
    ///
    /// `^:dynamic` is read the same way `publish_var_meta` reads the rest
    /// of the name symbol's `^{...}` -- straight off `name_form.meta`,
    /// since mova's `def` has no separate "is this var dynamic" flag of
    /// its own (confirmed: `grep -rn "is_dynamic\|set_dynamic"` finds
    /// nothing outside this crate's own doc comments) -- dynamism is
    /// purely a var-metadata fact here, exactly like every other `^{...}`
    /// key `publish_var_meta` already threads through.
    ///
    /// Routed through the same `write_shim_err` channel as
    /// `warn_if_def_shadows` (mova has no real `*err*` stream; this is a
    /// silent no-op everywhere else, see that channel's own doc).
    fn warn_if_non_dynamic_earmuff(
        &mut self,
        sym: &Symbol,
        name_form: &Form,
        span: Span,
        env: &Env,
    ) -> Result<(), RjError> {
        let name = sym.name.as_ref();
        let earmuffed = name.len() > 2 && name.starts_with('*') && name.ends_with('*');
        if !earmuffed {
            return Ok(());
        }
        let is_dynamic = match &name_form.meta {
            Some(meta_form) => match self.eval_meta_form(meta_form, env)? {
                Value::Map(m) => m
                    .get(&Value::Keyword("dynamic".into()))
                    .is_some_and(Value::truthy),
                _ => false,
            },
            None => false,
        };
        if is_dynamic {
            return Ok(());
        }
        let (line, _column) = crate::error::line_col(&self.source, span.start);
        let w = format!(
            "Warning: {name} not declared dynamic and thus is not dynamically rebindable, \
             but its name suggests otherwise. Please either indicate ^:dynamic {name} or \
             change the name. ({file}:{line})\n",
            name = sym.name,
            file = self.source_name,
        );
        crate::builtins::nsfns::write_shim_err(self, &w);
        Ok(())
    }

    /// S5/M3: writes a `def`/`defn`'s var metadata -- the name symbol's
    /// `^{...}`, plus the `(def name "doc" v)` docstring, plus
    /// `:name`/`:ns`/`:file`/`:line`/`:column`.
    ///
    /// C3h (clojure.repl surface): the three source-position keys this
    /// fn's doc used to call a KNOWN GAP are now filled in, and turned out
    /// to be a small addition rather than the "needs a span-to-line/column
    /// mapping this evaluator doesn't carry" gap the old comment
    /// predicted -- that mapping already existed (`error::line_col`, used
    /// for stack-frame rendering) and `Interp` already tracks the CURRENT
    /// file's path/text through every def (`source_name`/`source`, kept
    /// accurate across `require_ns`'s file switches -- see that fn's
    /// doc). `span` is the ENCLOSING form's span -- for a plain `(def ...)`
    /// that's this call's own span; for a `defn`-expanded one it is the
    /// ORIGINAL `(defn name ...)` call's span, because macroexpansion
    /// (`crate::reader::value_to_form`) stamps every synthesized sub-form
    /// with the macro CALL's span, not a fresh one -- so `:line` ends up
    /// exactly where real Clojure's does, the line the `(defn ...)` form
    /// itself starts on, without this fn needing to special-case macros.
    /// This is what unblocks `clojure.repl/source-fn`: given `:file`
    /// (`source_name`, a real path once the def came from a `require`d
    /// file) and `:line`, `source-fn` re-reads the file and slices the
    /// exact original text of the defining form -- see
    /// `builtins::reflect::source_fn_native`'s doc.
    fn publish_var_meta(
        &mut self,
        qualified: &Symbol,
        name_form: &Form,
        docstring: Option<Value>,
        span: Span,
        env: &Env,
    ) -> Result<(), RjError> {
        let cell = self.globals.intern(qualified);
        let mut m = PMap::new();
        m.insert(
            Value::Keyword("name".into()),
            Value::Sym(Symbol::simple(qualified.name.as_ref())),
        );
        if let Some(ns) = &qualified.ns {
            m.insert(Value::Keyword("ns".into()), Value::Sym(Symbol::simple(ns.as_ref())));
        }
        if let Some(doc) = docstring {
            m.insert(Value::Keyword("doc".into()), doc);
        }
        m.insert(Value::Keyword("file".into()), Value::Str(self.def_file.clone().unwrap_or_else(|| self.source_name.clone())));
        let (line, column) = crate::error::line_col(&self.source, span.start);
        m.insert(Value::Keyword("line".into()), Value::Int(line as i64));
        m.insert(Value::Keyword("column".into()), Value::Int(column as i64));
        // The symbol's own `^{...}` goes LAST so an explicit
        // `^{:doc "..."}` beats the positional docstring, and an
        // explicit `^{:name ...}` beats the synthesized one.
        if let Some(meta_form) = &name_form.meta {
            let attached = self.eval_meta_form(meta_form, env)?;
            if let Value::Map(attached) = attached {
                for (k, v) in attached.iter() {
                    m.insert(k.clone(), v.clone());
                }
            }
        }
        cell.set_var_meta(Value::Map(m));
        Ok(())
    }

    /// `(set! sym expr)` -- the MINIMAL honest slice (SPEC-D): assigns an
    /// existing global var and returns the assigned value, exactly what
    /// real Clojure's `set!` does at namespace/REPL scope (`(set!
    /// *warn-on-reflection* true)` => `true`, and a later read sees it).
    ///
    /// Divergence from real Clojure, recorded here rather than reproduced:
    /// real Clojure additionally requires the var be THREAD-BOUND (`set!`
    /// on a global that is merely `def`d, never `binding`-bound in this
    /// thread, throws `IllegalStateException`). mova has no binding stack
    /// yet (that is milestone M4b), so this slice allows `set!` on any
    /// EXISTING global, plain or "dynamic-knob" alike -- honest because it
    /// is strictly more permissive than real Clojure, never less, so no
    /// script that works in real Clojure is rejected here, only the
    /// reverse can happen (a script real Clojure would reject on the
    /// thread-bound check instead succeeds). M4b closes this gap once a
    /// binding stack exists to check against.
    ///
    /// Resolution mirrors `eval_var`/`resolve_var_cell` (the SAME candidate
    /// order `def` uses to find its cell) but does NOT fall back to
    /// interning a fresh unbound cell the way `resolve_var_cell` does for
    /// `(var x)`/`#'x`: `set!` on a symbol with no existing global binding
    /// is the ordinary unresolved-symbol error, at the symbol's own span --
    /// matching real Clojure's "Unable to resolve" `CompilerException`.
    fn eval_set_bang(&mut self, args: &[Form], span: Span, env: &Env) -> Result<Value, RjError> {
        if args.len() != 2 {
            return Err(self.err_here(
                RjError::arity(format!(
                    "Too few arguments to set!: expected 2, got {}",
                    args.len()
                )),
                span,
            ));
        }
        let sym = form_as_symbol(&args[0]).cloned().ok_or_else(|| {
            self.err_here(
                RjError::other("set!: first argument must be a symbol"),
                args[0].span,
            )
        })?;
        // Evaluation order per spec: `expr` first, THEN resolve `sym` --
        // matches `eval_def`'s "value before the write" order.
        let value = self.eval_form_in(&args[1], env)?;
        // clojure-lsp campaign (mova/PLAN.md): a mutable `deftype` field
        // local (real Clojure's OTHER `set!` target besides a Var --
        // `^:unsynchronized-mutable`/`^:volatile-mutable` fields) takes
        // priority over global resolution, exactly like an ordinary local
        // shadows a global of the same name everywhere else. `wrap_fields_
        // let` binds a mutable field `f` alongside a hidden `__mutfield_
        // owner_f -> this` marker (see its doc); finding that marker in
        // scope is how this recognizes "sym is a mutable field local" with
        // no local-mutation primitive exposed for anything else (an
        // ordinary `let`/fn-param local has no such marker, so it is
        // untouched and still falls through to the global-or-error path
        // below, matching real Clojure's "Cannot assign" rejection in
        // spirit if not in exact wording).
        if sym.ns.is_none() {
            let owner_sym = Symbol::simple(format!("__mutfield_owner_{}", sym.name));
            if let Some(Value::Inst(inst)) = env.get_local(&owner_sym) {
                if let Some(idx) = inst.tdef.basis.iter().position(|b| b.as_ref() == sym.name.as_ref())
                {
                    if inst.tdef.mutable.get(idx).copied().unwrap_or(false) {
                        crate::sync::lock_mutex(&inst.fields).set(idx, value.clone());
                        env.set_local_in_place(&sym, value.clone());
                        return Ok(value);
                    }
                }
            }
        }
        let cell = self
            .for_each_global_candidate(&sym, |cand| self.globals.find_bound_cell(cand))
            .ok_or_else(|| {
                self.err_here(
                    RjError::unresolved(format!(
                        "Unable to resolve symbol: {}",
                        crate::printer::pr_str(&Value::Sym(sym.clone()))
                    )),
                    args[0].span,
                )
                .with_label("undefined here")
            })?;
        // M4b: with a `binding` frame on this thread, `set!` writes THAT frame.
        // With none, it throws, as `Var.set` does on the JVM (never a root write).
        // Entry points (script runner, nREPL session) bind the vars scripts `set!`.
        if !cell.set_binding(value.clone()) && !self.bind_script_var(&cell, &value) {
            // JVM `Var.set`: a var with no thread binding (dynamic or not, core or
            // not) cannot be set; it never writes the root.
            return Err(self
                .err_here(
                    RjError::other(format!("Can't change/establish root binding of: {} with set", cell.name.name)),
                    args[0].span,
                )
                .with_class(crate::error::JvmClass::IllegalState));
        }
        // W3e-3: `*math-context*`'s value is cached in a thread-local the
        // numeric tower reads (it cannot reach the interpreter -- see
        // `builtins::numbers::MATH_CONTEXT`), so a write to that ONE var
        // has to refresh the cache or `(set! *math-context* ...)` moves a
        // var nothing consults.
        if crate::builtins::numbers::is_math_context_var(&cell.name) {
            crate::builtins::numbers::sync_math_context(&value);
        }
        Ok(value)
    }

    /// Shared skeleton of `binding`/`with-redefs` (M4b): validates the
    /// bindings vector, resolves every var cell, and evaluates every init
    /// BEFORE any effect takes place -- measured Clojure semantics
    /// ("bindings are made in parallel": `(binding [*a* 10 *b* *a*] ...)`
    /// sees the OLD `*a*` in `*b*`'s init), and it also means a failing
    /// init can never leave half the frames dangling.
    ///
    /// `allow_unbound`: W-DECL. `binding` passes `true` -- an
    /// interned-but-unbound cell (`(declare ^:dynamic p)`, never `def`d a
    /// value) is a legitimate `binding` target on the real JVM
    /// (`Var.pushThreadBindings` only ever checks `v.dynamic`, never
    /// `v.hasRoot()`), so it must resolve here via [`find_any_cell`]
    /// rather than disappear the way an absent symbol does.
    /// `with-redefs` keeps `false` (unchanged, `find_bound_cell` only):
    /// out of this task's scope, and restoring an unbound var's root via
    /// `raw_root()`/rebind is untested territory this fix does not touch.
    ///
    /// [`find_any_cell`]: crate::env::Env::find_any_cell
    fn resolve_binding_pairs(
        &mut self,
        who: &str,
        args: &[Form],
        span: Span,
        env: &Env,
        allow_unbound: bool,
    ) -> Result<Vec<(std::sync::Arc<crate::env::VarCell>, Value)>, RjError> {
        let Some(bindings_form) = args.first() else {
            return Err(self.err_here(
                RjError::other(format!("{who}: expected a bindings vector")),
                span,
            ));
        };
        let FormValue::Vector(pairs) = &bindings_form.value else {
            return Err(self.err_here(
                RjError::other(format!("{who}: expected a bindings vector")),
                bindings_form.span,
            ));
        };
        if pairs.len() % 2 != 0 {
            return Err(self.err_here(
                RjError::other(format!(
                    "{who}: bindings vector must have an even number of forms"
                )),
                bindings_form.span,
            ));
        }
        let mut resolved = Vec::with_capacity(pairs.len() / 2);
        for pair in pairs.chunks(2) {
            let sym = form_as_symbol(&pair[0]).cloned().ok_or_else(|| {
                self.err_here(
                    RjError::other(format!("{who}: binding names must be symbols")),
                    pair[0].span,
                )
            })?;
            let cell = self.resolve_dyn_var_cell(&sym, pair[0].span, allow_unbound)?;
            let init = self.eval_form_in(&pair[1], env)?;
            resolved.push((cell, init));
        }
        Ok(resolved)
    }

    /// C1: the var half of `resolve_binding_pairs`, shared with the compiled
    /// tier's `Ir::DynBind` so both resolve through the SAME candidate walk,
    /// at the same moment (run time, before that pair's init), with the same error.
    pub(crate) fn resolve_dyn_var_cell(
        &self,
        sym: &Symbol,
        sym_span: Span,
        allow_unbound: bool,
    ) -> Result<std::sync::Arc<crate::env::VarCell>, RjError> {
        self.for_each_global_candidate(sym, |cand| {
            if allow_unbound {
                self.globals.find_any_cell(cand)
            } else {
                self.globals.find_bound_cell(cand)
            }
        })
        .ok_or_else(|| {
            self.err_here(
                RjError::unresolved(format!(
                    "Unable to resolve symbol: {}",
                    crate::printer::pr_str(&Value::Sym(sym.clone()))
                )),
                sym_span,
            )
            .with_label("undefined here")
        })
    }

    /// W-DECL: real `Var.pushThreadBindings`'s own gate, reproduced --
    /// `Err` (plain runtime `IllegalStateException`, not a
    /// `CompilerException`-wrapped one: on the JVM this check runs
    /// inside `push-thread-bindings`, an ordinary function call the
    /// `binding` macro expands to, not anything the compiler itself
    /// inspects) unless the cell's own `IReference` metadata carries a
    /// truthy `:dynamic`. Measured (`compat/w-decl-binding-non-dynamic.txt`):
    /// `(def ndx) (binding [ndx 1] ndx)` => `java.lang.
    /// IllegalStateException: Can't dynamically bind non-dynamic var:
    /// user/ndx`; a `^:dynamic`-declared-but-still-UNBOUND var binds
    /// fine (`find_any_cell` in `resolve_binding_pairs` is what makes it
    /// reach this check at all instead of erroring earlier as
    /// unresolved).
    fn check_dynamic_or_err(
        &self,
        cell: &std::sync::Arc<crate::env::VarCell>,
        span: Span,
    ) -> Result<(), RjError> {
        let meta = cell.var_meta();
        // A handful of core vars (measured: `*ns*` -- `ns.rs`'s
        // `set_dynamic_ns`/`Interp` bootstrap wire it up directly with
        // `Env::intern`/`Env::set`, never through `eval_def`) are
        // genuinely dynamic in real Clojure but were never routed
        // through `publish_var_meta`, so their `IReference` meta is bare
        // `Value::Nil` -- not an empty map, NO map at all, which is only
        // otherwise true of a cell nothing has ever called `def`
        // (`declare` included) on. `eval_binding`'s own extensive `*ns*`
        // handling right below this call is proof that binding it has
        // always been load-bearing across this codebase (namespace-
        // switching inside a running fn body, the whole vendored-suite
        // runner's per-file harness). Auditing every such
        // meta-pipeline-bypassing native cell to stamp real `:dynamic`
        // meta by hand is out of this task's scope; treating "no meta at
        // all" as permissively bindable -- exactly the behavior EVERY
        // cell had before this check existed -- is the safe, narrow
        // reading: it never rejects anything the pre-W-DECL engine
        // accepted, and it still rejects the two shapes this task's
        // oracle actually pins (`(def x 1) (binding [x ..])`, `(declare
        // x) (binding [x ..])` with no `^:dynamic`), because BOTH of
        // those go through `eval_def`'s meta pipeline and so carry a real
        // (if `:dynamic`-less) map, not `Nil`.
        if matches!(meta, Value::Nil) {
            return Ok(());
        }
        let is_dynamic = matches!(
            meta,
            Value::Map(m) if matches!(
                m.get(&Value::Keyword(Keyword::from("dynamic"))),
                Some(v) if v.truthy()
            )
        );
        if is_dynamic {
            return Ok(());
        }
        let msg = format!(
            "Can't dynamically bind non-dynamic var: {}",
            crate::printer::pr_str(&Value::Sym(cell.name.clone()))
        );
        Err(self
            .err_here(RjError::other(msg), span)
            .with_class(crate::error::JvmClass::IllegalState))
    }

    /// M4b `(binding [v1 e1 ...] body...)`: per-thread dynamic frames,
    /// popped on ANY exit including a throw (measured `:throw-restores`
    /// probe -- Rust `?` on the body would skip the pops, so the result is
    /// held and the frames unwound by hand, reverse push order, exactly
    /// Clojure's popThreadBindings discipline).
    ///
    /// W-DECL: the old comment here ("mova has no metadata yet, so any
    /// resolvable global binds") is stale -- var metadata has existed
    /// since S5/M3. Real Clojure's `Var.pushThreadBindings` refuses a
    /// non-`^:dynamic` var ("Can't dynamically bind non-dynamic var:
    /// ns/name", `IllegalStateException`, measured against the oracle --
    /// `compat/w-decl-binding-non-dynamic.txt`); `check_dynamic_or_err`
    /// below reproduces exactly that check, over ALL resolved pairs
    /// before ANY push (matching real Clojure: every init in the
    /// bindings vector is evaluated -- by `resolve_binding_pairs` above --
    /// before `push-thread-bindings` ever runs its own per-entry dynamic
    /// check).
    fn eval_binding(&mut self, args: &[Form], span: Span, env: &Env) -> Result<Value, RjError> {
        let resolved = self.resolve_binding_pairs("binding", args, span, env, true)?;
        let frame = self.enter_binding(&resolved, span)?;
        let result = self.eval_do_body(&args[1..], env);
        self.leave_binding(&resolved, frame);
        result
    }

    /// C1: everything `binding` does between evaluating the inits and running
    /// the body -- shared verbatim with the compiled tier's `Ir::DynBind`.
    /// Returns (saved `current_ns`, math-context flag) for `leave_binding`.
    pub(crate) fn enter_binding(
        &mut self,
        resolved: &[(std::sync::Arc<crate::env::VarCell>, Value)],
        span: Span,
    ) -> Result<(Option<Str>, bool), RjError> {
        for (cell, _) in resolved {
            self.check_dynamic_or_err(cell, span)?;
        }
        // C3f, measured: `(binding [*ns* *ns*] (in-ns 'tmp.zzz)) orig` must
        // leave `orig` resolvable once the `binding` exits -- real
        // Clojure's `in-ns` is `set!` on `*ns*` (see `Interp::
        // set_current_ns`'s doc), so a `binding` of `*ns*` composes with it
        // automatically THERE; mova additionally has to save/restore
        // `current_ns` by hand around this one var, because (unlike every
        // other dynamic var) `current_ns` is a plain `Interp` field the
        // tree-walker actually resolves symbols against, not something
        // `VarCell`'s per-thread frame stack tracks for us. `saved_ns` is
        // `Some` the first time a `*ns*` pair is seen (a bindings vector
        // binding `*ns*` twice, like any var bound twice, is unusual but
        // not rejected -- only the OUTERMOST saved value matters for
        // restoration either way).
        //
        // field2/W-NS: the SAVE stays (an `in-ns` reached at depth 0 inside
        // the body still moves the lexical field, and nothing else would
        // put it back); the corresponding WRITE -- `current_ns = name` on
        // the way in -- is gone. Measured against the oracle
        // (`compat/w-ns-lexical-dynamic-probe.clj` row 6): real Clojure
        // compiles the whole `binding` form in the namespace it is WRITTEN
        // in, so `(def e-marker :x) (binding [*ns* (the-ns 'other)]
        // e-marker)` reads `:x`; moving the lexical field made mova answer
        // "Unable to resolve symbol: e-marker" instead. `binding` of `*ns*`
        // is a DYNAMIC-var operation -- what it must reach is `eval`,
        // `load`, `refer`/`alias` and `read-string` (all of which now go
        // through `dynamic_ns_name`/`add_alias`/`add_refer_as`), never this
        // body's own symbol resolution.
        let ns_cell = self.globals.intern(&Symbol::simple("*ns*"));
        let mut saved_ns: Option<Str> = None;
        for (cell, v) in resolved {
            cell.push_binding(v.clone());
            if Arc::ptr_eq(cell, &ns_cell) && saved_ns.is_none() {
                saved_ns = Some(self.current_ns.clone());
            }
        }
        // W3e-3: same reason as `set!`'s -- see the comment there and
        // `builtins::numbers::sync_math_context`. Refreshed on the way in
        // AND on the way out, so an unwinding error restores the outer
        // context too. `with-precision` also installs its own scope
        // (`with-precision*`); the two agree because both derive from the
        // same `{:precision .. :rounding-mode ..}` value.
        let math_ctx = resolved
            .iter()
            .any(|(cell, _)| crate::builtins::numbers::is_math_context_var(&cell.name));
        if math_ctx {
            self.refresh_math_context(resolved);
        }
        Ok((saved_ns, math_ctx))
    }

    /// C1: `binding`'s unwind (run on EVERY exit), shared with `Ir::DynBind`.
    pub(crate) fn leave_binding(
        &mut self,
        resolved: &[(std::sync::Arc<crate::env::VarCell>, Value)],
        (saved_ns, math_ctx): (Option<Str>, bool),
    ) {
        for (cell, _) in resolved.iter().rev() {
            cell.pop_binding();
        }
        if math_ctx {
            self.refresh_math_context(resolved);
        }
        if let Some(saved) = saved_ns {
            self.current_ns = saved;
        }
    }

    /// W3e-3: re-derive the ambient `MathContext` from whichever of
    /// `resolved`'s cells is `*math-context*`, reading it back through
    /// `VarCell::get` so the answer is whatever is visible NOW -- the frame
    /// just pushed, or, after the pop, the outer frame or the root.
    fn refresh_math_context(&self, resolved: &[(std::sync::Arc<crate::env::VarCell>, Value)]) {
        for (cell, _) in resolved {
            if crate::builtins::numbers::is_math_context_var(&cell.name) {
                crate::builtins::numbers::sync_math_context(&cell.get().unwrap_or(Value::Nil));
            }
        }
    }

    /// M4b `(with-redefs [v1 e1 ...] body...)`: temporarily replaces each
    /// var's ROOT value (visible to every thread, unlike `binding`),
    /// restoring the saved originals on any exit -- reverse order, so
    /// redefining one var twice in a single vector still lands back on the
    /// original. An unbound var errors (Clojure's with-redefs reads
    /// `.getRawRoot` before swapping and would NPE-adjacent fail too).
    ///
    /// W3e-3: the save reads [`VarCell::raw_root`], not `get`. Real
    /// `with-redefs-fn` is `.getRawRoot` in, `.bindRoot` out; reading the
    /// thread-VISIBLE value instead meant a `with-redefs` nested inside a
    /// `binding` of the same var wrote the binding's value back as the new
    /// root when it exited. Measured on the oracle
    /// (`compat/with-redefs-probe.clj`): inside `(binding [dv 2]
    /// (with-redefs [dv 3] ...))` the raw root reads `3` and `dv` reads
    /// `2`; after the `with-redefs` the raw root is back to `1`; after the
    /// `binding` `dv` reads `1`.
    fn eval_with_redefs(&mut self, args: &[Form], span: Span, env: &Env) -> Result<Value, RjError> {
        let resolved = self.resolve_binding_pairs("with-redefs", args, span, env, false)?;
        let saved = self.enter_with_redefs(&resolved, span)?;
        let result = self.eval_do_body(&args[1..], env);
        Self::leave_with_redefs(&saved);
        result
    }

    /// C1: `with-redefs`' root swap, shared with `Ir::DynBind`; returns the originals.
    pub(crate) fn enter_with_redefs(
        &self,
        resolved: &[(std::sync::Arc<crate::env::VarCell>, Value)],
        span: Span,
    ) -> Result<Vec<(std::sync::Arc<crate::env::VarCell>, Value)>, RjError> {
        // Save EVERY original before storing ANY replacement -- an unbound
        // var mid-vector must error with the world untouched, not with the
        // earlier vars already swapped and nothing left to restore them.
        let mut saved = Vec::with_capacity(resolved.len());
        for (cell, _) in resolved {
            let original = cell.raw_root().ok_or_else(|| {
                self.err_here(
                    RjError::other(format!(
                        "with-redefs: var {} is unbound",
                        crate::printer::pr_str(&Value::Sym(cell.name.clone()))
                    )),
                    span,
                )
            })?;
            saved.push((cell.clone(), original));
        }
        for (cell, replacement) in resolved {
            cell.store(replacement.clone(), false);
        }
        Ok(saved)
    }

    /// C1: `with-redefs`' restore (run on EVERY exit), shared with `Ir::DynBind`.
    pub(crate) fn leave_with_redefs(saved: &[(std::sync::Arc<crate::env::VarCell>, Value)]) {
        for (cell, original) in saved.iter().rev() {
            cell.store(original.clone(), false);
        }
    }

    /// The compiled-fn tier's single hook (v0.3 / S2; lazy tier-up v0.6):
    /// the legacy closure is built exactly as before. Under
    /// `MOVA_EAGER_COMPILE=1` (or `MOVA_EXPLAIN`/`compile-explain`, which
    /// need the def-time answer to report on), `compile_fn` runs right
    /// here, exactly as it always did. Otherwise the fn is created
    /// tree-walk-only, with an empty `CompileSlot` -- `apply_closure`'s
    /// `on_call` hook fires the first (real) compile attempt later. Either
    /// way the closure is complete and tree-walkable from the moment it is
    /// built.
    pub(super) fn eval_fn_form(&mut self, args: &[Form], span: Span, env: &Env) -> Result<Value, RjError> {
        // H2/fn-template-cache: see the module-level doc on `FnTemplate`.
        // `is_repeat` is `false` only for the very first time THIS
        // syntactic literal is seen (a fresh cache entry) -- a one-shot
        // lambda never gets past this, so it costs nothing extra.
        let args_ptr = args.as_ptr() as usize;
        let (template, is_repeat) = match fn_template_cache_get(args_ptr, args) {
            Some(t) => (t, true),
            None => {
                let (name, arities) = self.parse_fn_like(args, span)?;
                (fn_template_cache_put(args_ptr, args.to_vec(), name, Arc::new(arities)), false)
            }
        };
        let name = template.name.clone();
        let arities = template.arities.clone();
        // `MOVA_EXPLAIN=1` needs the def-time answer to report on (its
        // `eprintln` fires from inside `compile_fn` itself), so it forces
        // eager compilation exactly like `MOVA_EAGER_COMPILE=1` (checked
        // via `Interp::eager_compile`) -- see `compile::explain`'s doc.
        let force_eager = self.eager_compile() || crate::compile::explain::explain_enabled();
        let compiled = if force_eager || is_repeat {
            // field4/W-LENS-1: `compile_fn` hands back the regret-ledger
            // site for this fn alongside the IR -- `lens::NO_SITE` unless
            // the fn bailed or carries escapes. See `Closure::lens_site`.
            //
            // `get_or_init`: at most one `compile_fn` call per TEMPLATE,
            // ever -- a losing racer under concurrent eval just reads back
            // the winner's result, same discipline as `CompileSlot::on_call`.
            let (cc, site) = template
                .compiled
                .get_or_init(|| crate::compile::compile_fn(self, name.as_ref(), &arities, env, span))
                .clone();
            crate::value::CompileSlot::settled(cc, site)
        } else {
            crate::value::CompileSlot::pending()
        };
        // W4C: the one dynamic-var read this closure's `unchecked_math` flag
        // ever gets -- see that field's own doc on `Closure`.
        let unchecked_math = self.unchecked_math_active();
        #[cfg(feature = "leak-probe")]
        crate::value::CLOSURE_CREATED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let closure = Arc::new(Closure {
            name,
            arities,
            env: env.clone(),
            ns: self.current_ns.clone(),
            compiled,
            unchecked_math,
            def_span: span,
            def_source_id: crate::source_registry::SrcRef::new(self.source_id),
            native_macro: None,
        });
        Ok(Value::Fn(closure))
    }

    fn eval_defmacro(&mut self, args: &[Form], span: Span, env: &Env) -> Result<Value, RjError> {
        if args.is_empty() {
            return Err(self.err_here(
                RjError::arity("defmacro: expected a name and at least one arity"),
                span,
            ));
        }
        let name_sym = form_as_symbol(&args[0]).cloned().ok_or_else(|| {
            self.err_here(
                RjError::other("defmacro: first argument must be a name symbol"),
                args[0].span,
            )
        })?;
        let (cleaned, doc, leading_attr, trailing_attr) = extract_macro_doc_and_attrs(args);
        let (_, arities) = self.parse_fn_like(&cleaned, span)?;
        // field4/W-LENS-1: every macro gets a ledger site. Unlike a fn,
        // there is no "clean" outcome to exclude: the compiled tier freezes
        // expansion, so a macro reached by tree-walked code is re-expanded
        // on EVERY evaluation, and that count is exactly the trigger the
        // macro-expansion-cache wave needs.
        let (lens_site, first) = self.lens_site_for(
            crate::lens::SiteKind::Macro,
            Some(&name_sym.name),
            span,
        );
        if first {
            crate::lens::set_site_reason(
                lens_site,
                "tree-walked call sites re-expand this macro on every evaluation".to_string(),
            );
        }
        // Macro bodies are never compiled: they run at expansion time
        // against unevaluated forms, and the compiled tier's one deviation
        // is precisely that it freezes expansion (see `compile`'s doc).
        #[cfg(feature = "leak-probe")]
        crate::value::CLOSURE_CREATED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let closure = Arc::new(Closure {
            name: Some(name_sym.name.clone()),
            arities: Arc::new(arities),
            env: env.clone(),
            ns: self.current_ns.clone(),
            compiled: crate::value::CompileSlot::settled(None, lens_site),
            // W4C: a macro body expands unevaluated forms, never runs
            // arithmetic of its own accord -- `unchecked_math` is never
            // consulted for a `Value::Macro` closure, so this is inert
            // rather than a real capture (unlike `eval_fn_form`'s).
            unchecked_math: false,
            def_span: span,
            def_source_id: crate::source_registry::SrcRef::new(self.source_id),
            native_macro: None,
        });
        let qualified = self.qualify_def(&name_sym);
        self.note_special_shadow(&qualified);
        self.globals.set(qualified.clone(), Value::Macro(closure.clone()));
        // H2/macro-expansion-cache: every `defmacro` (first def or
        // redefinition) busts the cache -- see `bump_macro_def_epoch`'s doc
        // for why a raw `Arc<Closure>` pointer check alone is not safe.
        crate::eval::bump_macro_def_epoch();
        // S6: `defmacro` has the SAME `name doc-string? attr-map?
        // ([params] body)+ attr-map?` signature `defn` does (measured
        // against the oracle), but `defmacro` is a Rust special form with
        // no `core.mova`-level macro layer of its own to do the parsing
        // (unlike `defn`, which is bootstrapped in `core.mova` on top of
        // plain `def` and gets this for free) -- so it's replicated here.
        // Precedence (highest wins), measured: `:macro true` (forced,
        // unconditionally -- real Clojure sets it via a separate
        // `(.setMacro)` call after the `def`) > trailing attr-map > leading
        // attr-map > docstring > the name symbol's own `^{...}` reader
        // meta > synthesized `:name`/`:ns`. This is the OPPOSITE order
        // from plain `def` (`eval_def`/`publish_var_meta` above), where
        // the name's own `^{...}` meta wins over everything -- `def` has
        // no attr-map concept at all, so that precedence doesn't apply
        // here; `defmacro`/`defn`'s derived precedence is what the oracle
        // actually shows (e.g. `(defn ^{:foo 1} f {:foo 2} [x] x)` ->
        // `(:foo (meta #'f)) => 2`, attr-map beats the symbol's own meta).
        self.publish_macro_var_meta(&qualified, &args[0], doc, leading_attr, trailing_attr, env)?;
        let _ = closure;
        Ok(Value::Var(self.globals.intern(&qualified)))
    }

    /// Builds and writes a `defmacro` var's metadata -- see the S6 comment
    /// at its one call site (`eval_defmacro`) for the precedence this
    /// mirrors from `defn`'s `core.mova` bootstrap.
    fn publish_macro_var_meta(
        &mut self,
        qualified: &Symbol,
        name_form: &Form,
        doc: Option<Value>,
        leading_attr: Option<Value>,
        trailing_attr: Option<Value>,
        env: &Env,
    ) -> Result<(), RjError> {
        let cell = self.globals.intern(qualified);
        let mut base = PMap::new();
        base.insert(
            Value::Keyword("name".into()),
            Value::Sym(Symbol::simple(qualified.name.as_ref())),
        );
        if let Some(ns) = &qualified.ns {
            base.insert(Value::Keyword("ns".into()), Value::Sym(Symbol::simple(ns.as_ref())));
        }
        // Own `^{...}` meta on the name symbol is the LOW-precedence base
        // of the doc/attr chain (see the S6 comment above for why this is
        // the opposite of plain `def`'s precedence).
        if let Some(meta_form) = &name_form.meta {
            let attached = self.eval_meta_form(meta_form, env)?;
            if let Value::Map(attached) = attached {
                for (k, v) in attached.iter() {
                    base.insert(k.clone(), v.clone());
                }
            }
        }
        if let Some(doc) = doc {
            base.insert(Value::Keyword("doc".into()), doc);
        }
        for attr in [leading_attr, trailing_attr].into_iter().flatten() {
            if let Value::Map(m) = attr {
                for (k, v) in m.iter() {
                    base.insert(k.clone(), v.clone());
                }
            }
        }
        base.insert(Value::Keyword("macro".into()), Value::Bool(true));
        cell.set_var_meta(Value::Map(base));
        Ok(())
    }

/// W3a/W4B-MESSAGES (fn.clj's `fn-error-checking`, def.clj's
/// `defn-error-messages`): every "this `fn`/`defn`/`defmacro` arglist is
/// not a legal arglist" rejection, tagged with the class real Clojure
/// reports the same condition as AND worded the way real Clojure words
/// it for a BARE `fn` call.
///
/// Real Clojure validates these arglists with `clojure.spec` at
/// macroexpansion time, and the failure surfaces as a
/// `clojure.lang.Compiler$CompilerException` ("Syntax error macroexpanding
/// clojure.core/fn at (..)") whose `.getCause` is a
/// `clojure.lang.ExceptionInfo` reading "Call to clojure.core/fn did not
/// conform to spec." -- measured for all eight forms fn.clj feeds `eval`.
/// mova has no `clojure.spec` and is not growing one: it rejects exactly
/// the same forms with its own, hand-written, more specific shape check,
/// and now genuinely reports the SAME leading text real Clojure does
/// (W4B-MESSAGES: `ex-message` on this class was already non-nil --
/// `error_to_info_map` surfaces an `RjError`'s own message text verbatim
/// -- so the shim's `fails-with-cause?` regex check was ALREADY being
/// applied for real, and failing for real, on the old "fn: ..." wording;
/// this is not a new code path, it is the existing one finally saying
/// what real Clojure says). `ExceptionInfo` (the cause) rather than
/// `Compiler$CompilerException` (the wrapper) because mova has no cause
/// chain and the vendored file reaches for the cause: it asserts with
/// `fails-with-cause? clojure.lang.ExceptionInfo`, which under
/// `mova-test-shim.mova` checks the exception actually caught.
///
/// def.clj's `defn-error-messages` feeds these SAME five malformed
/// shapes to `defn`, not bare `fn` -- and needs "Call to
/// clojure.core/defn did not conform to spec" instead, because upstream's
/// `defn` carries its OWN spec, checked BEFORE ever macroexpanding to
/// `fn`, and reports itself. This fn's message would be wrong for that
/// caller (it always says "fn"), so `defn` (core.mova) no longer relies
/// on falling through to this check at all: it validates the identical
/// `fdecl` shape itself, first, with its own message, right next to its
/// pre-existing "bad name" check (see `defn`'s own doc comment in
/// `core/core.mova`) -- so a `defn`-originated malformed arglist never
/// reaches this Rust-level check in the first place. A BARE `(fn ...)`
/// call (no `defn` in between) is the only way to still hit this fn's
/// message, which is why it can unconditionally say "fn".
fn fn_spec_err(detail: &str) -> RjError {
    RjError::other(format!(
        "Call to clojure.core/fn did not conform to spec: {detail}"
    ))
    .with_class(crate::error::JvmClass::ExceptionInfo)
}

    /// Shared by `fn` and `defmacro`: `[name?] ([params] body...)+` or
    /// `[name?] [params] body...`.
    pub(crate) fn parse_fn_like(&self, args: &[Form], span: Span) -> Result<(Option<crate::value::Str>, Vec<Arity>), RjError> {
        if args.is_empty() {
            return Err(self.err_here(
                Self::fn_spec_err("expected a parameter vector or name"),
                span,
            ));
        }
        let mut i = 0;
        let name = match &args[0].value {
            FormValue::Atom(Value::Sym(s)) if s.ns.is_none() => {
                i = 1;
                Some(s.name.clone())
            }
            _ => None,
        };
        if i >= args.len() {
            return Err(self.err_here(Self::fn_spec_err("missing parameter list"), span));
        }
        let rest = &args[i..];
        let arities = if matches!(&rest[0].value, FormValue::Vector(_)) {
            vec![self.parse_single_arity(&rest[0], &rest[1..])?]
        } else {
            let mut arities = Vec::with_capacity(rest.len());
            for a in rest {
                match &a.value {
                    FormValue::List(items) if !items.is_empty() && matches!(items[0].value, FormValue::Vector(_)) => {
                        arities.push(self.parse_single_arity(&items[0], &items[1..])?);
                    }
                    _ => {
                        return Err(self.err_here(
                            Self::fn_spec_err("expected an (params body...) arity clause"),
                            a.span,
                        ))
                    }
                }
            }
            if arities.is_empty() {
                return Err(self.err_here(Self::fn_spec_err("no arities given"), span));
            }
            arities
        };
        Ok((name, arities))
    }

    /// Parses one `[params...]` vector into an `Arity`. Any parameter that
    /// isn't a plain symbol (a destructuring pattern -- `[a b]`, `{:keys
    /// [..]}`, etc.) is replaced by a generated `__p<n>` symbol and its
    /// original pattern form is queued for `wrap_let_form` below, which
    /// splices a re-destructuring `let` around `body` (see this module's
    /// doc comment).
    fn parse_single_arity(&self, params_form: &Form, body: &[Form]) -> Result<Arity, RjError> {
        let items = match &params_form.value {
            FormValue::Vector(v) => v,
            _ => {
                return Err(self.err_here(
                    Self::fn_spec_err("expected a parameter vector"),
                    params_form.span,
                ))
            }
        };
        let mut params = Vec::with_capacity(items.len());
        let mut rest = None;
        // D9: parallel to `params`, and thrown away again below unless at
        // least one slot is `Some` -- see `Arity::coerce`. Built here, at
        // PARSE time, so a call never looks at a hint.
        let mut coerce: Vec<Option<crate::value::PrimCast>> = Vec::new();
        let mut any_coerce = false;
        let mut destructure_bindings: Vec<Form> = Vec::new();
        let mut gensym_counter = 0usize;
        let mut i = 0;
        while i < items.len() {
            if is_amp(&items[i]) {
                i += 1;
                if i >= items.len() {
                    return Err(self.err_here(
                        Self::fn_spec_err("expected a binding after '&'"),
                        params_form.span,
                    ));
                }
                let (sym, extra) = param_binding(&items[i], &mut gensym_counter);
                if let Some((pat, sym_form)) = extra {
                    destructure_bindings.push(pat);
                    destructure_bindings.push(sym_form);
                }
                rest = Some(sym);
                i += 1;
                if i < items.len() {
                    return Err(self.err_here(
                        Self::fn_spec_err("only one binding allowed after '&'"),
                        items[i].span,
                    ));
                }
            } else {
                let (sym, extra) = param_binding(&items[i], &mut gensym_counter);
                // D9: only a PLAIN-SYMBOL parameter can carry a primitive
                // hint. A destructuring pattern's hint is inert on the
                // oracle too (measured: `((fn [^long [a b]] a) [1 2])` =>
                // `1`, i.e. the vector reached the destructuring intact
                // rather than dying in a cast), which falls out for free
                // here: `extra.is_some()` is exactly "this param was a
                // pattern".
                let cast = if extra.is_none() { param_prim_cast(&items[i]) } else { None };
                any_coerce |= cast.is_some();
                coerce.push(cast);
                if let Some((pat, sym_form)) = extra {
                    destructure_bindings.push(pat);
                    destructure_bindings.push(sym_form);
                }
                params.push(sym);
                i += 1;
            }
        }
        // clojure-lsp campaign (mova/PLAN.md): `{:pre [...] :post [...]}`
        // extracted BEFORE the destructuring wrap below, so `:pre` sees
        // the same (already-destructured) param names the real body
        // does -- `wrap_pre_post`'s own doc has the full rationale.
        let body = wrap_pre_post(body, params_form.span);
        let mut final_body = if destructure_bindings.is_empty() {
            body
        } else {
            vec![wrap_let_form(destructure_bindings, &body, params_form.span)]
        };
        final_body.shrink_to_fit();
        Ok(Arity {
            params,
            rest,
            body: final_body.into(),
            // The `Vec` is dropped outright for the unhinted majority, so
            // no `Arity` in a hint-free program keeps a heap allocation --
            // only the 16 bytes of the `None`.
            coerce: any_coerce.then(|| coerce.into_boxed_slice()),
        })
    }

    fn eval_let(&mut self, args: &[Form], span: Span, env: &Env, letfn: bool) -> Result<Value, RjError> {
        if args.is_empty() {
            return Err(self.err_here(RjError::arity("let: missing bindings vector"), span));
        }
        let items = match &args[0].value {
            FormValue::Vector(v) => v,
            _ => {
                return Err(self.err_here(
                    RjError::other("let: first argument must be a vector of bindings"),
                    args[0].span,
                ))
            }
        };
        if items.len() % 2 != 0 {
            return Err(self.err_here(
                RjError::other("let: bindings must be an even number of forms"),
                args[0].span,
            ));
        }
        // Lexical `let` (JVM Clojure semantics). The tree-walker's env chain
        // is dynamic (a closure captures a FRAME, not a value snapshot), so
        // a name bound AFTER a closure was made is still visible to it if
        // nothing splits the frame -- that's exactly how letfn's mutual
        // forward references work (a sibling name not yet bound at closure-
        // creation time, `env.get` -> `None`, resolves once the later pair
        // binds it into the SAME frame). It only becomes the bug when the
        // rebound name ALREADY resolved to something at closure-creation
        // time (an outer/global binding, a param, or an earlier pair of
        // this same `let`) -- then a later pair overwriting it retroactively
        // changes what the earlier closure sees. So: split to a fresh child
        // frame before a binding pair whenever (a) some earlier pair's value
        // expression syntactically contained a nested `fn`/`fn*` (so a
        // closure may have captured names free), AND (b) this pair's
        // pattern names something that ALREADY resolves to a value right
        // now (a real rebind, not a forward reference to nothing). Plain
        // new-name introductions, and forward refs to not-yet-bound names,
        // keep using the current frame, so letfn is unaffected. `frames` is
        // a stack of `FrameGuard`s -- normally just one -- each still the
        // frame's sole persistent handle at the moment it retires (see
        // `FrameGuard`'s doc): the IIFE below runs the bindings+body and
        // hands back a `Result` instead of returning directly, so every
        // exit path can drain `frames` newest-first (mirrors ordinary
        // reverse-declaration drop order) before this fn returns -- an
        // ancestor frame's cycle check is only valid once its child frame's
        // own drop has already released the child's `parent` reference to
        // it.
        // `letfn`'s own shape (core/core.mova's `let`-of-mutually-recursive-
        // fns expansion, ~329-344) must be EXEMPT from the split above, the
        // same way `compile::resolve`'s `rec_group_run`/`MakeRecGroup`
        // exempts it from the compiled tier's poison rule: a maximal run of
        // 2+ CONSECUTIVE bindings, each a bare symbol bound to a literal
        // `fn`/`fn*` form, is a mutual-recursion group -- one sibling may
        // "shadow" an outer name of the same name (e.g. a global), and that
        // must NOT split the frame, because every OTHER sibling still needs
        // the one shared frame to see it. Anything else that merely
        // resembles a rebind (the value isn't a bare-symbol/literal-fn
        // pair) still splits normally.
        let num_pairs = items.len() / 2;
        let is_fn_bound: Vec<bool> = (0..num_pairs)
            .map(|k| matches!(&items[k * 2].value, FormValue::Atom(Value::Sym(_))) && form_is_fn_literal(&items[k * 2 + 1]))
            .collect();
        // Only `letfn*` (the `letfn` expansion) is late-binding; plain `let` is strictly sequential.
        let in_letfn_run = |k: usize| -> bool { letfn && is_fn_bound[k] };
        let mut frames: Vec<FrameGuard> = vec![FrameGuard(env.child())];
        let mut saw_fn_since_split = false;
        let result: Result<Value, RjError> = (|| {
            let mut i = 0;
            while i < items.len() {
                // W4B-MESSAGES: validate THIS pair's pattern SHAPE before
                // touching its value expression at all -- see
                // `validate_pattern_shape`'s doc for why the order matters.
                self.validate_pattern_shape(&items[i], false)?;
                let val = self.eval_form_in(&items[i + 1], frames.last().unwrap())?;
                let mut names = Vec::new();
                collect_let_pattern_names(&items[i], &mut names);
                // Full symbol resolution (lexical chain, THEN the
                // namespace-aware global order -- see `resolve_symbol`'s
                // doc), matching exactly what a free reference to `n`
                // inside an earlier closure would have resolved to. A
                // plain `env.get`/`get_local` only covers the lexical half
                // and misses a global `def` in the current namespace,
                // which is the repro's own failing case (`ctx` is a
                // top-level `def`, not a local).
                let shadow = names.iter().any(|n| self.resolve_symbol(frames.last().unwrap(), n).is_some());
                if saw_fn_since_split && shadow && !in_letfn_run(i / 2) {
                    let next = frames.last().unwrap().child();
                    frames.push(FrameGuard(next));
                    saw_fn_since_split = false;
                }
                self.bind_pattern(&items[i], val, frames.last().unwrap())?;
                saw_fn_since_split |= form_has_nested_fn(&items[i + 1]);
                i += 2;
            }
            self.eval_do_body(&args[1..], frames.last().unwrap())
        })();
        while let Some(g) = frames.pop() {
            drop(g);
        }
        result
    }

    fn eval_if(&mut self, args: &[Form], span: Span, env: &Env) -> Result<Value, RjError> {
        if args.len() < 2 || args.len() > 3 {
            return Err(self.err_here(
                RjError::arity(format!("if: expected 2 or 3 arguments, got {}", args.len())),
                span,
            ));
        }
        let test = self.eval_form_in(&args[0], env)?;
        if test.truthy() {
            self.eval_form_in(&args[1], env)
        } else if args.len() == 3 {
            self.eval_form_in(&args[2], env)
        } else {
            Ok(Value::Nil)
        }
    }

    fn eval_loop(&mut self, args: &[Form], span: Span, env: &Env) -> Result<Value, RjError> {
        if args.is_empty() {
            return Err(self.err_here(RjError::arity("loop: missing bindings vector"), span));
        }
        let bindings_items = match &args[0].value {
            FormValue::Vector(v) => v,
            _ => {
                return Err(self.err_here(
                    RjError::other("loop: first argument must be a vector of bindings"),
                    args[0].span,
                ))
            }
        };
        if bindings_items.len() % 2 != 0 {
            return Err(self.err_here(
                RjError::other("loop: bindings must be an even number of forms"),
                args[0].span,
            ));
        }
        let n = bindings_items.len() / 2;
        // One internal `__loopN` symbol per binding *pair* (not per name a
        // pattern introduces) -- see this module's doc comment.
        let internal_names: Vec<Symbol> = (0..n).map(|idx| Symbol::simple(format!("__loop{idx}"))).collect();
        let patterns: Vec<Form> = (0..n).map(|idx| bindings_items[idx * 2].clone()).collect();

        // Same lexical-rebind split as `eval_let` (see its doc comment),
        // applied to `loop`'s INITIAL bindings vector only -- a `recur`
        // rebind below is a fresh sibling frame off the outer `env` every
        // iteration (never chained to a previous iteration's frame), so it
        // was already lexically correct and is untouched.
        // Same letfn-run exemption as `eval_let` (see its doc comment).
        let in_letfn_run = |_k: usize| -> bool { false };
        let mut init_frames: Vec<FrameGuard> = vec![FrameGuard(env.child())];
        let mut saw_fn_since_split = false;
        for idx in 0..n {
            // W4B-MESSAGES: same pattern-before-value ordering as `let`
            // -- see `validate_pattern_shape`'s doc.
            self.validate_pattern_shape(&patterns[idx], false)?;
            let val = self.eval_form_in(&bindings_items[idx * 2 + 1], init_frames.last().unwrap())?;
            let mut names = Vec::new();
            collect_let_pattern_names(&patterns[idx], &mut names);
            if saw_fn_since_split && !in_letfn_run(idx) && names.iter().any(|n| self.resolve_symbol(init_frames.last().unwrap(), n).is_some()) {
                let next = init_frames.last().unwrap().child();
                init_frames.push(FrameGuard(next));
                saw_fn_since_split = false;
            }
            let cur = init_frames.last().unwrap();
            cur.set(internal_names[idx].clone(), val.clone());
            self.bind_pattern(&patterns[idx], val, cur)?;
            saw_fn_since_split |= form_has_nested_fn(&bindings_items[idx * 2 + 1]);
        }
        // The innermost init frame becomes the loop's per-iteration guard,
        // exactly as the single-frame version did; any superseded ancestor
        // frames stay in `init_frames`, kept alive only via its `parent`
        // chain, and are drained (newest-first) after `cur_env` -- their
        // one and only descendant -- has fully dropped below.
        let mut cur_env = init_frames.pop().expect("at least one frame");

        let body = &args[1..];
        let result: Result<Value, RjError> = (|| {
            loop {
                match self.eval_do_body(body, &cur_env) {
                    Ok(v) => return Ok(v),
                    Err(e) if e.kind == ErrorKind::Recur => {
                        // Fuel: `loop` back-edge (tree-walker). See
                        // `eval::apply::run_closure_trampoline`'s identical
                        // check for the fn-self-recur twin of this site.
                        self.tick_edge().map_err(|fe| fe.with_span(e.span.unwrap_or(span)))?;
                        let new_vals = self.recur_args(&e)?;
                        if new_vals.len() != n {
                            return Err(self.err_here(
                                RjError::arity(format!(
                                    "loop: recur expected {n} argument(s), got {}",
                                    new_vals.len()
                                )),
                                e.span.unwrap_or(span),
                            ));
                        }
                        // Wrapped at creation, BEFORE the `?`-carrying binds
                        // below, so an error there still retires this frame.
                        let next_env = FrameGuard(env.child());
                        for (name, val) in internal_names.iter().zip(new_vals.iter()) {
                            next_env.set(name.clone(), val.clone());
                        }
                        for (pat, val) in patterns.iter().zip(new_vals.into_iter()) {
                            self.bind_pattern(pat, val, &next_env)?;
                        }
                        cur_env = next_env;
                    }
                    Err(e) => return Err(e),
                }
            }
        })();
        drop(cur_env);
        while let Some(g) = init_frames.pop() {
            drop(g);
        }
        result
    }

    fn eval_recur(&mut self, args: &[Form], span: Span, env: &Env) -> Result<Value, RjError> {
        let mut vals: PVec = PVec::new();
        for a in args {
            vals.push_back(self.eval_form_in(a, env)?);
        }
        let mut e = RjError::recur("recur used outside loop/fn tail position");
        e.thrown = Some(Value::Vector(vals));
        e.span = Some(span);
        e.stack = self.stack_snapshot();
        Err(e)
    }

    fn eval_quote(&mut self, args: &[Form], span: Span) -> Result<Value, RjError> {
        if args.len() != 1 {
            // C3c (special.clj's `quote-with-multiple-args`): real
            // Clojure throws `clojure.lang.Compiler$CompilerException`
            // here, whose `.getCause` is an `ex-info`-shaped exception
            // carrying `:form` (the WHOLE offending form, `(quote 1 2
            // 3)`, not just its args) in its data -- measured via
            // `.oracle`: `(-> ex (.getCause) (ex-data) (:form))` is `'(quote
            // 1 2 3)`. Reproduced narrowly, for this one call site only
            // (not a general Compiler-exception-wrapping mechanism):
            // the "cause" is a plain `Value::Map` shaped exactly like
            // `core/core.mova`'s own `ex-info` return value
            // (`{:ex/message .. :ex/data {:form ..}}`), which is what
            // that same `core.mova`'s `ex-data`/`Throwable->map` already
            // know how to read (`(map? e)` branch) -- no NEW cause-value
            // shape invented, just this module reaching for the one
            // `ex-info` already produces.
            let mut whole_form = PVec::new();
            whole_form.push_back(Value::Sym(Symbol::simple("quote")));
            for a in args {
                whole_form.push_back(crate::reader::form_to_value(a));
            }
            let mut data = PMap::new();
            data.insert(Value::Keyword("form".into()), Value::List(whole_form));
            let mut cause = PMap::new();
            cause.insert(
                Value::Keyword("ex/message".into()),
                Value::Str("Too many arguments to quote".into()),
            );
            cause.insert(Value::Keyword("ex/data".into()), Value::Map(data));
            return Err(self.err_here(
                // W3a (special.clj's `quote-with-multiple-args`): measured
                // -- `(eval '(quote 1 2 3))` surfaces a
                // `clojure.lang.Compiler$CompilerException` ("Syntax error
                // compiling quote at (..)") whose `.getCause` is the
                // `ExceptionInfo` carrying the offending `:form`, and that
                // deftest reads BOTH levels. The `arity_cause` C3c already
                // built here IS that cause; tagging the class is what makes
                // `error_to_info_map` mint the outer wrapper too, instead
                // of catch-binding the cause's own `ArityException` as if
                // it were the top-level exception.
                RjError::arity(format!("quote: expected 1 argument, got {}", args.len()))
                    .with_class(crate::error::JvmClass::CompilerException)
                    .with_arity_actual(args.len() as i64)
                    .with_arity_cause(Value::Map(cause)),
                span,
            ));
        }
        Ok(crate::reader::form_to_value(&args[0]))
    }

    /// `(var x)` (the reader desugars `#'x` to this): resolves `x` to its
    /// `Arc<VarCell>` under the same namespace candidate order every other
    /// global lookup uses (`crate::ns::Interp::resolve_var_cell`), and
    /// wraps it as an invocable `Value::Var`. NOT evaluated against `env`'s
    /// lexical frames -- `var`, like `quote`, takes its argument as a
    /// literal symbol, never a locally-bound value.
    fn eval_var(&mut self, args: &[Form], span: Span) -> Result<Value, RjError> {
        if args.len() != 1 {
            return Err(self.err_here(
                RjError::arity(format!("var: expected 1 argument, got {}", args.len())),
                span,
            ));
        }
        let sym = form_as_symbol(&args[0]).cloned().ok_or_else(|| {
            self.err_here(RjError::other("var: argument must be a symbol"), args[0].span)
        })?;
        Ok(Value::Var(self.resolve_var_cell(&sym)))
    }

    /// `(try body... (catch C1 e1 ...) (catch C2 e2 ...) ... (finally
    /// ...))` -- C3g: any number of `catch` clauses (was: at most one, a
    /// hard hard "only one catch clause is supported" hasError), each
    /// optionally typed. On a throw, clauses are tried IN ORDER and the
    /// first whose class matches wins (`catch_class_matches`) -- real
    /// `catch`-clause semantics. An UNTYPED clause (`(catch e ...)`, no
    /// class symbol -- `parse_catch_head` returns `None` for its class)
    /// matches UNCONDITIONALLY, identical to every pre-C3g single-catch
    /// call site (this corpus and `core.mova` both lean on that: see
    /// `tests/clojure-suite/mova-test-shim.mova`'s and several `.mova`
    /// examples' bare `(catch e ...)`). A throw matching no clause at all
    /// (typed clauses only, none of which matches) propagates, same as
    /// today's no-catch-at-all case.
    fn eval_try(&mut self, args: &[Form], env: &Env) -> Result<Value, RjError> {
        let mut body: Vec<Form> = Vec::new();
        let mut catches: Vec<(Option<Symbol>, Symbol, Vec<Form>)> = Vec::new();
        let mut finally: Option<Vec<Form>> = None;
        for a in args {
            if let FormValue::List(items) = &a.value {
                if let Some(sym) = items.first().and_then(form_as_symbol) {
                    if sym.ns.is_none() && sym.name.as_ref() == "catch" {
                        if items.len() < 2 {
                            return Err(self.err_here(
                                RjError::other("catch: expected a binding symbol"),
                                a.span,
                            ));
                        }
                        let (class, bind, consumed) =
                            parse_catch_head(&items[1..]).ok_or_else(|| {
                                self.err_here(
                                    RjError::other("catch: binding must be a symbol"),
                                    items[1].span,
                                )
                            })?;
                        catches.push((class, bind, items[1 + consumed..].to_vec()));
                        continue;
                    }
                    if sym.ns.is_none() && sym.name.as_ref() == "finally" {
                        if finally.is_some() {
                            return Err(self.err_here(
                                RjError::other("try: only one finally clause is supported"),
                                a.span,
                            ));
                        }
                        finally = Some(items[1..].to_vec());
                        continue;
                    }
                }
            }
            body.push(a.clone());
        }

        let result = match self.eval_do_body(&body, env) {
            Ok(v) => Ok(v),
            Err(e) if e.kind == ErrorKind::Recur => Err(e),
            // Fuel exhaustion must NOT be catchable by script-level `try`:
            // an untrusted script that could swallow its own exhaustion
            // signal in a `(catch :default _ ...)` and keep running would
            // defeat the whole mechanism. Excluded here exactly like
            // `Recur` above (a control signal, not a script-visible error);
            // the HOST still sees it as a plain `Err` from `eval_str`/
            // `eval_form`/`call`, which never runs inside a script's `try`.
            Err(e) if e.kind == ErrorKind::FuelExhausted || e.kind == ErrorKind::InterruptedHard => Err(e),
            Err(e) => {
                match catches.iter().find(|(class, _, _)| {
                    class.as_ref().is_none_or(|c| catch_class_matches(c, &e))
                }) {
                    Some((_, bind, catch_body)) => {
                        trace_catch(&e, &self.current_ns);
                        let thrown_value = if e.kind == ErrorKind::Thrown {
                            e.thrown.clone().unwrap_or(Value::Nil)
                        } else {
                            error_to_info_map(&e)
                        };
                        let catch_env = env.child();
                        catch_env.set(bind.clone(), thrown_value);
                        self.eval_do_body(catch_body, &catch_env)
                    }
                    None => Err(e),
                }
            }
        };

        if let Some(fb) = &finally {
            let fin_env = env.child();
            // `finally` always runs; if it errors, that error wins (masks
            // the original) which matches typical try/finally semantics.
            self.eval_do_body(fb, &fin_env)?;
        }
        result
    }

    fn eval_throw(&mut self, args: &[Form], span: Span, env: &Env) -> Result<Value, RjError> {
        if args.len() != 1 {
            return Err(self.err_here(
                RjError::arity(format!("throw: expected 1 argument, got {}", args.len())),
                span,
            ));
        }
        let v = self.eval_form_in(&args[0], env)?;
        Err(self.err_here(RjError::thrown(v), span))
    }

    /// A script entry point (`clojure.main`: `Engine::eval_named`) binds the vars a script may
    /// `set!`. Binding them on first use costs nothing for a script that never sets them.
    /// Returns true when `cell` was one of those vars and now holds `value`.
    fn bind_script_var(&mut self, cell: &Arc<crate::env::VarCell>, value: &Value) -> bool {
        let Some(frame) = self.script_frame.as_mut() else { return false };
        if !crate::embed::is_script_bound_var(&cell.name.name) || cell.current_binding().is_some() {
            return false;
        }
        cell.push_binding(cell.raw_root().unwrap_or(Value::Nil));
        frame.push(cell.clone());
        cell.set_binding(value.clone())
    }

    /// `(ns name & clauses)`: makes `name` current, then processes the
    /// clauses it understands. `:require` loads and aliases; S5 adds
    /// `:use` (require + refer) and `:import` (builtin class/interface
    /// resolution), both of which silently skip anything they cannot
    /// resolve. EVERY other clause -- `(:refer-clojure :exclude [...])`
    /// (unnecessary here, since a namespace's own defs already shadow core
    /// for it), `:gen-class`, a docstring, an attribute map -- is ignored
    /// in silence, which is what lets real Clojure/jank source load
    /// unedited.
    fn eval_ns(&mut self, args: &[Form], span: Span) -> Result<Value, RjError> {
        let Some(sym) = args.first().and_then(form_as_symbol) else {
            return Err(self.err_here(
                RjError::other("ns: expected a namespace name symbol"),
                span,
            ));
        };
        // field2/W-NS: `switch_ns`, not `set_current_ns` -- a mid-body
        // `(ns ...)` moves only the dynamic `*ns*`. See that fn's doc.
        self.switch_ns(sym.name.clone());
        // `ns` refers clojure.core (`:refer-clojure` options only narrow it)
        self.set_ns_no_core(&sym.name, false);
        for clause in &args[1..] {
            let items = match &clause.value {
                FormValue::List(items) | FormValue::Vector(items) => items,
                _ => continue, // docstring, attribute map, ...
            };
            let Some(head) = items.first() else { continue };
            match form_keyword_name(head) {
                Some("require") => {
                    for spec in &items[1..] {
                        self.eval_require_spec(spec)?;
                    }
                }
                // S5: `:use` is `require` + `refer` (see `crate::ns::
                // Interp::use_spec_value` for the full rationale,
                // including why an unresolvable spec stays silently
                // ignored rather than blocking the file).
                Some("use") => {
                    for spec in &items[1..] {
                        let value = crate::reader::form_to_value(spec);
                        // S6: `use_spec_value` now propagates a genuine
                        // load failure (found the ns, but it errored while
                        // loading) instead of swallowing it -- only "not
                        // found on the module path" still returns `Ok(())`
                        // silently. See its doc comment in `crate::ns`.
                        self.use_spec_value(&value, spec.span)?;
                    }
                }
                // S5: `:import` resolves against the builtin class and
                // interface tables (`import`'s own path), binding short
                // names in this namespace. Unknown classes stay ignored,
                // same tolerance and same reason as `:use` above: mova has
                // no JVM classpath, and most vendored `:import`s name JVM
                // classes that will never exist here.
                //
                // kondo-wave: a `(java.io InputStream BufferedReader
                // Closeable)`-shaped package spec is expanded to ONE
                // `(pkg cls)` pair PER class, each tried (and ignored on
                // failure) separately, rather than handing the whole list
                // to `import_spec_value` in one call. `import_spec_value`
                // uses `?` between classes of the SAME list (correct for
                // the real `import` special form, which must abort and
                // report on the first missing class) -- so, undetected
                // until this task, one unknown class anywhere in a
                // multi-class `:import` list silently dropped every class
                // AFTER it too, contradicting this arm's own "unknown
                // classes stay ignored" comment above. Measured case:
                // `clj-kondo.impl.toolsreader`'s `reader_types.clj` does
                // `(:import (java.io InputStream BufferedReader
                // Closeable))` -- `InputStream` isn't in mova's builtin
                // table, which used to take `Closeable` down with it even
                // though `Closeable` IS registered (`types::
                // builtin_interfaces()`).
                Some("import") => {
                    for spec in &items[1..] {
                        let value = crate::reader::form_to_value(spec);
                        match &value {
                            Value::List(entries) | Value::Vector(entries) => {
                                let mut it = entries.iter();
                                let Some(pkg @ Value::Sym(_)) = it.next() else {
                                    continue;
                                };
                                for cls in it {
                                    let one =
                                        crate::pvec![pkg.clone(), cls.clone()];
                                    let _ = self
                                        .import_spec_value(&Value::List(one), spec.span);
                                }
                            }
                            _ => {
                                let _ = self.import_spec_value(&value, spec.span);
                            }
                        }
                    }
                }
                // SPEC-W3 (defect ledger D7): `(:refer-clojure :exclude
                // [...])`. mova still reaches `clojure.core` by fallback
                // rather than by a per-namespace mapping table, so the
                // "un-refer" half of this clause remains unimplemented on
                // purpose -- see `Interp::refer_clojure_excludes`'s doc
                // and the ledger. What is recorded here is the part mova
                // observably got WRONG: a name the namespace explicitly
                // excluded must not warn "already refers to
                // #'clojure.core/<name>" when the namespace then defines
                // it. Only `:exclude` is read; `:only`/`:rename` are
                // ignored in the same silence as before.
                Some("refer-clojure") => {
                    let mut rest = items[1..].iter();
                    while let Some(opt) = rest.next() {
                        // `:rename {old new, ...}` is recorded into the SAME
                        // `refer_clojure_excludes` set as `:exclude`: real
                        // Clojure's `:rename` gives the referred core var a
                        // different LOCAL name (`new`), which frees up the
                        // BARE `old` name -- exactly what `:exclude` does --
                        // so a namespace that then defines its own `old`
                        // must not warn "already refers to #'clojure.core/
                        // old" either. Measured: `datalog.parser.impl`
                        // (vendored `.cljc`) reads, under the `:clj` reader-
                        // conditional branch, as `(:refer-clojure :rename
                        // {distinct? core-distinct?})` with NO `:exclude`
                        // clause at all (its `:exclude` is `#?@(:cljs ...)`-
                        // gated) -- before this, mova had no path that
                        // suppressed the warning for a `:rename`-only ns
                        // form, so `(defn- distinct? ...)` right after it
                        // warned even though real Clojure is silent.
                        let opt_name = form_keyword_name(opt);
                        if opt_name != Some("exclude") && opt_name != Some("rename") {
                            continue;
                        }
                        let is_rename = opt_name == Some("rename");
                        let Some(names) = rest.next() else { break };
                        if is_rename {
                            let FormValue::Map(entries) = &names.value else {
                                continue;
                            };
                            for (k, v) in entries {
                                if let Some(s) = form_as_symbol(k) {
                                    self.refer_clojure_excludes
                                        .insert((sym.name.clone(), s.name.clone()));
                                    // e2: the new local name must resolve to the core var (`core-distinct?` in datalog.parser.impl).
                                    if let Some(to) = form_as_symbol(v) {
                                        self.add_refer_as(to.name.clone(), "clojure.core".into(), s.name.clone());
                                    }
                                }
                            }
                            continue;
                        }
                        let (FormValue::Vector(names) | FormValue::List(names)) = &names.value
                        else {
                            continue;
                        };
                        for n in names {
                            if let Some(s) = form_as_symbol(n) {
                                self.refer_clojure_excludes
                                    .insert((sym.name.clone(), s.name.clone()));
                            }
                        }
                    }
                }
                _ => continue,
            }
        }
        // field2/W-NS, measured against `clojure/core.clj`'s own `ns` macro
        // (1.12.5, line 5875): its expansion's LAST form is
        // `(if (.equals '<name> 'clojure.core) nil (do (dosync (commute
        // @#'*loaded-libs* conj '<name>)) nil))` -- declaring a namespace
        // MARKS IT LOADED, so a later `(require '<name>)` is a no-op
        // instead of a "could not locate namespace" failure. Confirmed
        // directly on the oracle: `(ns a__zz__auto__) (in-ns 'user)` then
        // `(contains? @#'clojure.core/*loaded-libs* 'a__zz__auto__)` =>
        // true, and `(require 'a__zz__auto__)` => nil, while `(require
        // 'totally.not.there)` still throws FileNotFoundException. This is
        // the second half of `repl.clj`'s `test-dynamic-ns`: `(let [a
        // (call-ns-sym)] (require a))` must be `nil` for a namespace that
        // only ever existed because a mid-body `(ns a#)` created it.
        //
        // AFTER the clause loop, matching the expansion's own order (a
        // `:require` that throws never reaches the `commute`), and never
        // for `clojure.core` itself -- both exactly as the macro has it.
        if sym.name.as_ref() != crate::ns::CORE_NS {
            self.mark_ns_loaded(sym.name.clone());
        }
        Ok(Value::Nil)
    }

    /// One `:require` spec: `x.y`, `[x.y :as z]`, `[x.y :refer [f g]]`, or
    /// any combination of those options. S4: the actual parsing/dispatch
    /// now lives in `crate::ns::Interp::require_spec_value` (a `Value`,
    /// not `Form`, based parser -- also the callable `require` native's
    /// implementation, `builtins::nsfns`), so there is exactly one
    /// libspec parser for both the `(ns ...)` clause path and top-level/
    /// programmatic `require` calls. `:require` clause specs are literal,
    /// UNEVALUATED syntax (Clojure doesn't quote them:
    /// `(:require [clojure.string :as s])`, no `'`), which is exactly
    /// what `crate::reader::form_to_value` gives back.
    fn eval_require_spec(&mut self, spec: &Form) -> Result<(), RjError> {
        let value = crate::reader::form_to_value(spec);
        self.require_spec_value(&value, spec.span)
    }

    fn eval_macroexpand1(&mut self, args: &[Form], span: Span, env: &Env) -> Result<Value, RjError> {
        if args.len() != 1 {
            return Err(self.err_here(
                RjError::arity(format!(
                    "macroexpand-1: expected 1 argument, got {}",
                    args.len()
                )),
                span,
            ));
        }
        let v = self.eval_form_in(&args[0], env)?;
        // field2/W-NS: the EXPANSION itself runs against the DYNAMIC
        // `*ns*`, not this body's lexical one -- see `eval_macroexpand`'s
        // twin bracket below for the measured reason.
        let dyn_ns = self.dynamic_ns_name();
        let saved = std::mem::replace(&mut self.current_ns, dyn_ns);
        let result = self.macroexpand_1_value(&v, span, env);
        self.current_ns = saved;
        let (expanded, _) = result?;
        Ok(expanded)
    }

    fn eval_macroexpand(&mut self, args: &[Form], span: Span, env: &Env) -> Result<Value, RjError> {
        if args.len() != 1 {
            return Err(self.err_here(
                RjError::arity(format!("macroexpand: expected 1 argument, got {}", args.len())),
                span,
            ));
        }
        let mut v = self.eval_form_in(&args[0], env)?;
        // field2/W-NS: `macroexpand`/`macroexpand-1` are COMPILE-TIME
        // operations on the real JVM -- `Compiler.macroexpand1` decides
        // "is this head a macro" through `namesStaticMember`/`lookupVar`,
        // i.e. against `currentNS()`, the dynamic `*ns*`, exactly like
        // `eval` and `load`. mova's tree-walker otherwise expands against
        // the LEXICAL namespace of the body that called `macroexpand`,
        // which is right for the macros the body itself writes but wrong
        // for a name that only exists in `*ns*`. Measured vendored case:
        // `errors.clj`'s `assert-arg-messages` does `(refer 'clojure.core
        // :rename '{with-open renamed-with-open})` -- a RUNTIME refer into
        // `*ns*` (see `ns::Interp::add_refer_as`) -- and then asserts that
        // `(macroexpand (read-string "(renamed-with-open [a])"))` throws
        // naming the LOCAL name; only a `*ns*`-based expansion can see
        // that rename. Bracketed and restored on every path, same shape as
        // `builtins::reflect::eval_native`'s.
        let dyn_ns = self.dynamic_ns_name();
        let saved = std::mem::replace(&mut self.current_ns, dyn_ns);
        let result = (|itp: &mut Self| -> Result<Value, RjError> {
            loop {
                let (expanded, did_expand) = itp.macroexpand_1_value(&v, span, env)?;
                if !did_expand {
                    return Ok(v);
                }
                v = expanded;
            }
        })(self);
        self.current_ns = saved;
        result
    }

    fn macroexpand_1_value(&mut self, v: &Value, span: Span, env: &Env) -> Result<(Value, bool), RjError> {
        if let Value::List(items) = v {
            if let Some(Value::Sym(sym)) = items.get(0) {
                // W3e-2: a special-form head is never macroexpanded --
                // `Compiler.macroexpand1`'s own first line is
                // `if(isSpecial(op)) return x;`, and BOTH of mova's
                // evaluation tiers already match special forms before
                // macros. Without this the `clojure.core`-visibility
                // forwarding macros (`core.mova`'s W3e-2 block) would
                // expand `(let ...)` to `(clojure.core/let ...)` and then
                // expand THAT to itself forever inside `eval_macroexpand`'s
                // fixpoint loop.
                if self.is_bare_or_core_alias(sym) && is_special_form_name(&sym.name) {
                    return Ok((v.clone(), false));
                }
                if let Some(Value::Macro(closure)) = self.resolve_symbol(env, sym) {
                    let arg_forms: Vec<Form> = items
                        .iter()
                        .skip(1)
                        .map(|it| crate::reader::value_to_form(it, span))
                        .collect();
                    let expanded = if let Some(nf) = closure.native_macro {
                        nf(self, &arg_forms, span, &closure)?
                    } else {
                        self.apply_macro(&closure, &arg_forms, v.clone(), span)?
                    };
                    // SPEC-W4: `eval_macroexpand`'s fixpoint loop above
                    // matches a `Value::List` head to decide whether to
                    // expand again, so a lazily-`concat`-built expansion
                    // (`->`, `doto`, `dotimes`, ..) has to be realized
                    // first -- otherwise `(macroexpand '(-> x f g))` would
                    // stop after one step. See
                    // `Interp::realize_form_value`.
                    let expanded = self.realize_form_value(&expanded)?.unwrap_or(expanded);
                    return Ok((expanded, true));
                }
            }
        }
        Ok((v.clone(), false))
    }

    /// Recursively binds `pattern` (unevaluated binding-site syntax --
    /// `let`/`loop`/the `fn`-param desugar's synthesized wrapper) against
    /// the already-evaluated `value`, defining every name it introduces in
    /// `env`. This is the one engine behind every binding site's
    /// destructuring; see this module's doc comment for how `loop`/`recur`
    /// and `fn` params route through it.
    fn bind_pattern(&mut self, pattern: &Form, value: Value, env: &Env) -> Result<(), RjError> {
        match &pattern.value {
            // S7 (tail wave), measured: `(let [& 42] &)`/`(fn [& 42] ..)`
            // throw on the real JVM -- `&` is the reserved rest-args
            // marker EVERYWHERE a binding-symbol position can appear, not
            // just inside a vector-destructuring pattern (where
            // `bind_seq_pattern` already consumes it as a marker before
            // ever reaching this arm). Reached here only when `&` shows up
            // as an entire top-level binding target on its own (a plain
            // `let`/`loop`/`fn`-param position, not preceded by a
            // vector-pattern's own `&`), which is exactly the misuse
            // `special.clj`'s `amp-not-allowed-as-let-or-loop*-binding-name`
            // catches.
            FormValue::Atom(Value::Sym(sym)) if sym.ns.is_none() && sym.name.as_ref() == "&" => {
                Err(self.err_here(
                    RjError::other("&: not allowed as a let/loop/fn binding name"),
                    pattern.span,
                ))
            }
            FormValue::Atom(Value::Sym(sym)) => {
                env.set(sym.clone(), value);
                Ok(())
            }
            FormValue::Vector(items) => self.bind_seq_pattern(items, value, env),
            // `force_select`/`force_all` are false here: this is a
            // top-level (or ordinarily-nested) map pattern, not one being
            // read back by an enclosing `:select`/`:all` (see
            // `bind_map_pattern`'s own doc) -- nobody needs its
            // `NestedSelectAll` output.
            FormValue::Map(pairs) => self
                .bind_map_pattern(pairs, value, env, false, false, pattern.span)
                .map(|_| ()),
            _ => Err(self.err_here(
                RjError::other(format!(
                    "invalid destructuring pattern: {}",
                    crate::printer::pr_str(&crate::reader::form_to_value(pattern))
                )),
                pattern.span,
            )),
        }
    }

    /// W4B-MESSAGES (special.clj's `keywords-not-allowed-in-let-bindings`,
    /// `namespaced-syms-only-allowed-in-map-destructuring`,
    /// `binding-types-not-allowed`): real Clojure validates a `let`/
    /// `loop` binding PATTERN's shape via `clojure.spec` at
    /// MACROEXPANSION time -- entirely BEFORE the paired VALUE
    /// expression is ever compiled, let alone evaluated. Measured
    /// directly (compat/w4b-special-oracle-transcript.txt's last probe):
    /// `(let [{:a1/keys [b/c]} some-unresolved-var] c)` throws "did not
    /// conform to spec", NEVER "Unable to resolve symbol", even though
    /// the value expression alone would also fail on its own merits --
    /// pattern validation wins the race unconditionally. `eval_let`/
    /// `eval_loop` evaluate each pair's value BEFORE calling
    /// `bind_pattern`, so the shape checks `bind_pattern`/
    /// `bind_map_pattern`/`bind_directive_entries` already make ran too
    /// late to win that race whenever the paired value expression is
    /// independently broken (which is exactly `binding-types-not-
    /// allowed`'s doseq: 9 of its 12 malformed-directive-item forms pair
    /// the bad pattern with a value expression that itself doesn't
    /// resolve, and used to report THAT failure instead).
    ///
    /// This is a lightweight, VALUE-INDEPENDENT pre-check run before
    /// evaluating a pair's value -- not a spec engine, exactly the three
    /// malformed shapes this corpus exercises, one named rule each:
    ///   - a bare keyword anywhere a binding name is expected (`:a`, or
    ///     nested in a vector pattern, `[:a]`).
    ///   - a namespaced symbol anywhere OUTSIDE a map-destructuring
    ///     general entry's bind position (`a/x` as a whole pattern, or
    ///     `[a/x]` inside a vector pattern -- map destructuring's OWN
    ///     `a/x`-as-general-entry-key shorthand is a SEPARATE, legal
    ///     thing, not this condition).
    ///   - a namespaced `:keys!`/`:syms!`/`:keys`/`:syms`/`:strs`(`!`)
    ///     directive's item list containing anything other than a
    ///     simple symbol -- duplicates the check `bind_directive_entries`
    ///     already makes (CLJ-2968, see that fn's own doc), run here
    ///     early enough to beat a broken value expression.
    ///
    /// Everything else (plain symbols, `&`, `:as`, well-formed nested
    /// patterns, non-namespaced directive items, general map entries)
    /// is deliberately left alone here -- `bind_pattern`/`bind_map_
    /// pattern` still own validating and ACTUALLY BINDING those, this
    /// fn only front-runs the handful of conditions that must fire
    /// before the value is touched at all.
    fn validate_pattern_shape(&self, pattern: &Form, in_map_general_entry: bool) -> Result<(), RjError> {
        fn spec_err(interp: &Interp, detail: String, span: Span) -> RjError {
            interp
                .err_here(
                    RjError::other(format!("Call to clojure.core/let did not conform to spec: {detail}"))
                        .with_class(crate::error::JvmClass::ExceptionInfo),
                    span,
                )
        }
        match &pattern.value {
            FormValue::Atom(Value::Keyword(k)) => Err(spec_err(
                self,
                format!(
                    "{} - a keyword is not a valid let/loop binding name",
                    crate::printer::pr_str(&Value::Keyword(k.clone()))
                ),
                pattern.span,
            )),
            FormValue::Atom(Value::Sym(s)) if s.ns.is_some() && !in_map_general_entry => Err(spec_err(
                self,
                format!(
                    "{} - a namespaced symbol is only allowed as a map-destructuring key",
                    crate::printer::pr_str(&Value::Sym(s.clone()))
                ),
                pattern.span,
            )),
            FormValue::Vector(items) => {
                // `&` (rest marker) and `:as name` (whole-value binding)
                // are markers, not destructuring sub-patterns -- `&`
                // isn't a namespaced-anything so it would pass the checks
                // below unscathed either way, but `:as` IS a bare keyword
                // and would otherwise be wrongly rejected by the keyword
                // check above (measured regression:
                // `conformance_test.rs`'s `[a b :as all]`/`[a & r :as
                // all]` rows). `all` (the name right after `:as`) is an
                // ordinary plain-symbol bind target in every real usage,
                // so it's still validated normally, just not mistaken
                // for a positional destructuring item in the loop below.
                let mut i = 0;
                while i < items.len() {
                    if is_amp(&items[i]) {
                        i += 1;
                        continue;
                    }
                    if is_as_kw(&items[i]) {
                        i += 1;
                        if i < items.len() {
                            self.validate_pattern_shape(&items[i], false)?;
                            i += 1;
                        }
                        continue;
                    }
                    self.validate_pattern_shape(&items[i], false)?;
                    i += 1;
                }
                Ok(())
            }
            FormValue::Map(pairs) => {
                for (k, v) in pairs {
                    match form_keyword_name(k) {
                        Some("or") | Some("as") | Some("select") | Some("all") | Some("defaults") => {}
                        Some(flat) => {
                            let (mkns, mkn) = split_directive_ns(flat);
                            let is_dir = mkn.starts_with("keys") || mkn.starts_with("strs") || mkn.starts_with("syms");
                            if is_dir && mkns.is_some() {
                                if let FormValue::Vector(items) = &v.value {
                                    // Mirrors `bind_directive_entries`'s
                                    // own `preamp` tracking: the "only
                                    // simple symbols" restriction is
                                    // PRE-`&` only -- items after `&` are
                                    // literal `:select`/`:all` keys, not
                                    // binding symbols, and may legally be
                                    // keywords/namespaced things even
                                    // under a namespaced directive
                                    // (measured regression:
                                    // data_structures.clj's `keys-bang`/
                                    // `syms-bang`/`select-directive`,
                                    // which all use post-`&` namespaced-
                                    // keyword `:select` keys under
                                    // namespaced directives).
                                    let mut preamp = true;
                                    for item in items {
                                        if is_amp(item) {
                                            preamp = false;
                                            continue;
                                        }
                                        if preamp
                                            && !matches!(&item.value, FormValue::Atom(Value::Sym(s)) if s.ns.is_none())
                                        {
                                            return Err(spec_err(
                                                self,
                                                format!(
                                                    "{} - only simple symbols allowed in a namespaced destructuring directive ({})",
                                                    crate::printer::pr_str(&crate::reader::form_to_value(item)),
                                                    crate::printer::pr_str(&crate::reader::form_to_value(k))
                                                ),
                                                item.span,
                                            ));
                                        }
                                    }
                                }
                            }
                        }
                        // A general map entry's bind side (`k`) may
                        // legally be a namespaced symbol (map-
                        // destructuring's own shorthand) -- validated
                        // with `in_map_general_entry: true`.
                        None => self.validate_pattern_shape(k, true)?,
                    }
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// Sequential destructuring `[a b & rest :as all]`: works on any
    /// seqable via the `uncons` cons-cell protocol (vectors, lists, lazy
    /// seqs, strings, nil) -- missing positions bind `nil`, `& rest` binds
    /// the (still-lazy, if it was) remainder or `nil` when nothing is
    /// left, and `:as` binds the ORIGINAL `value`, not the walked-down
    /// cursor.
    fn bind_seq_pattern(&mut self, items: &[Form], value: Value, env: &Env) -> Result<(), RjError> {
        let mut cur = value.clone();
        let mut i = 0;
        while i < items.len() {
            if is_amp(&items[i]) {
                i += 1;
                if i >= items.len() {
                    return Err(self.err_here(
                        RjError::other("destructuring: expected a binding after '&'"),
                        items[i - 1].span,
                    ));
                }
                let rest_val = match crate::builtins::uncons(self, &cur)? {
                    None => Value::Nil,
                    Some(_) => cur.clone(),
                };
                self.bind_pattern(&items[i], rest_val, env)?;
                i += 1;
                continue;
            }
            if is_as_kw(&items[i]) {
                i += 1;
                if i >= items.len() {
                    return Err(self.err_here(
                        RjError::other("destructuring: expected a binding after ':as'"),
                        items[i - 1].span,
                    ));
                }
                self.bind_pattern(&items[i], value.clone(), env)?;
                i += 1;
                continue;
            }
            let elem = match crate::builtins::uncons(self, &cur)? {
                Some((h, t)) => {
                    cur = t;
                    h
                }
                None => Value::Nil,
            };
            self.bind_pattern(&items[i], elem, env)?;
            i += 1;
        }
        Ok(())
    }

    /// Map destructuring: the full Clojure 1.13.0-alpha6 `destmap*`/`push1`
    /// surface (`compat/notes-destructuring.md`, `CLOJURE-COMPAT-PLAN.md`
    /// §5) -- plain `{binding key}` pairs (pattern-valued bindings recurse,
    /// so `{[x y] :point}` works), `:keys`/`:strs`/`:syms` (optionally
    /// namespace-prefixed: `:foo/keys`), their required `!`-suffixed
    /// siblings (`:keys!`/`:strs!`/`:syms!`/`:foo/keys!`/...: a missing key
    /// throws `IllegalArgumentException "Missing required key: <k>"` at
    /// BIND time), `&` inside a directive vector (everything after it is
    /// declared but not bound, except a `!`-required entry -- still
    /// enforced, just not locally named), `:select`/`:all` (bind a map of
    /// the declared/all keys, with `:or` defaults and nested sub-selects
    /// folded in), and `:defaults` (bind the resolved `:or` map; requires
    /// `:or`). `:or` defaults apply only when the key is genuinely MISSING
    /// (Clojure semantics: `{:keys [a] :or {a :dflt}}` against `{:a nil}`
    /// binds `nil`, not `:dflt`), are evaluated as expressions in `env`,
    /// and (back-compat, §5) are validated -- "does not refer to a
    /// binding", "appear only in `:or`" -- ONLY when `:select`/`:all`/
    /// `:defaults` is also present; a dangling `:or` key with none of
    /// those present stays silently ignored, same as before this port.
    /// Works on `nil` (every key reads as missing) same as real Clojure.
    /// `value` is coerced through `coerce_map_pattern_source` first (a
    /// `seq?` value -- e.g. the rest-arg list a fn's `& {:keys [...]}`
    /// variadic-kwargs param binds -- becomes a map); `:as` binds this
    /// coerced value, matching Clojure.
    ///
    /// Ported from `destmap*`/`push1`'s ALGORITHM
    /// (`.oracle/clojure-src/src/clj/clojure/core.clj`), not its
    /// macroexpansion-into-`get`-calls strategy: mova destructures by
    /// direct execution (`bind_pattern`'s whole design), so this performs
    /// the lookups/throws/binds real Clojure would have generated CODE
    /// for. One documented simplification from the ported algorithm: real
    /// Clojure evaluates every `:or` default expression EXACTLY ONCE (each
    /// bound to its own gensym, consulted by both the inline fallback and
    /// the `:select`/`:all`/`:defaults` output); this port evaluates each
    /// default LAZILY, at every point it's actually consulted, which can
    /// evaluate a default up to twice when BOTH an inline fallback AND
    /// `:select`/`:all`/`:defaults` need it. Unobservable for a pure
    /// default expression (every corpus/suite case), would double a side
    /// effect for an impure one -- not exercised anywhere in this repo.
    ///
    /// `force_select`/`force_all` exist ONLY for `destmap*`'s nested
    /// `:select`/`:all` propagation (`subs`/`suba`): a PARENT map pattern
    /// with its own `:select`/`:all` recurses into a map-valued entry with
    /// the matching flag set so it can read back THAT entry's own
    /// select/all computation (even when the nested pattern doesn't
    /// declare `:select`/`:all` itself) for its own merge -- see the
    /// call site below and `NestedSelectAll`. An ordinary top-level or
    /// non-propagating call passes `false, false` (`bind_pattern`'s Map
    /// arm) and ignores the returned `NestedSelectAll`.
    fn bind_map_pattern(
        &mut self,
        pairs: &[(Form, Form)],
        value: Value,
        env: &Env,
        force_select: bool,
        force_all: bool,
        span: Span,
    ) -> Result<NestedSelectAll, RjError> {
        let value = self.coerce_map_pattern_source(&value)?;

        let mut or_defaults: Vec<(OrKey, Form)> = Vec::new();
        // Whether an `:or` key was seen AT ALL, distinct from `or_defaults`
        // being empty: `:or {}` (a present-but-EMPTY map) is still truthy
        // in Clojure (`(and {} ...)` doesn't short-circuit), so `:defaults
        // d :or {}` is legal (`d` binds `{}`), only `:defaults` with NO
        // `:or` key at all is "Can't specify :defaults without :or" --
        // oracle-verified live (`select-or-defaults`'s own first case).
        let mut or_present = false;
        let mut as_pattern: Option<Form> = None;
        let mut select_form: Option<Form> = None;
        let mut all_form: Option<Form> = None;
        let mut defaults_form: Option<Form> = None;

        enum Entry<'a> {
            Dir {
                kind: DirKind,
                req: bool,
                mkns: Option<Str>,
                items: &'a [Form],
                dir_name: String,
            },
            // The key form is stored, NOT pre-evaluated: real Clojure's
            // `destmap*` splices a general entry's key form VERBATIM into
            // the generated `(get gmap <key-form>)` call, so it is an
            // ordinary EXPRESSION, evaluated once the enclosing code runs
            // -- observably different from a literal for anything that
            // isn't self-evaluating, e.g. `{a 'b}` (the key is `(quote
            // b)`, evaluating to the symbol `b`) or `{a some-var}` (the
            // key is a var/local reference) -- both oracle-confirmed live,
            // and both exercised by `keys-bang`/`syms-bang`'s `'sym`-keyed
            // nested entries. Evaluated lazily, in entry-processing order,
            // in the SAME accumulating `env` every other part of this
            // pattern binds into (so an earlier sibling binding is visible
            // to a later entry's key expression, exactly like `:or`
            // defaults already were).
            General {
                bind_form: &'a Form,
                key_form: &'a Form,
            },
        }
        let mut entries: Vec<Entry> = Vec::new();

        for (k, v) in pairs {
            match form_keyword_name(k) {
                Some("or") => {
                    or_present = true;
                    let or_pairs = match &v.value {
                        FormValue::Map(p) => p,
                        _ => return Err(self.err_here(RjError::other(":or value must be a map"), v.span)),
                    };
                    for (okey, oval) in or_pairs {
                        let key = match form_as_symbol(okey) {
                            Some(sym) => OrKey::Sym(sym.name.clone()),
                            None => OrKey::Lit(crate::reader::form_to_value(okey), okey.clone()),
                        };
                        or_defaults.push((key, oval.clone()));
                    }
                }
                Some("as") => as_pattern = Some(v.clone()),
                Some("select") => select_form = Some(v.clone()),
                Some("all") => all_form = Some(v.clone()),
                Some("defaults") => defaults_form = Some(v.clone()),
                Some(flat) => {
                    let (mkns, mkn) = split_directive_ns(flat);
                    let kind = if mkn.starts_with("keys") {
                        DirKind::Keys
                    } else if mkn.starts_with("strs") {
                        DirKind::Strs
                    } else if mkn.starts_with("syms") {
                        DirKind::Syms
                    } else {
                        return Err(self.err_here(
                            RjError::other(format!(
                                "Unsupported map directive: {}",
                                crate::printer::pr_str(&crate::reader::form_to_value(k))
                            )),
                            k.span,
                        ));
                    };
                    let req = mkn.ends_with('!');
                    let items: &[Form] = match &v.value {
                        FormValue::Vector(items) => items,
                        _ => {
                            return Err(self.err_here(
                                RjError::other(format!(
                                    "{} value must be a vector",
                                    crate::printer::pr_str(&crate::reader::form_to_value(k))
                                )),
                                v.span,
                            ))
                        }
                    };
                    entries.push(Entry::Dir {
                        kind,
                        req,
                        mkns,
                        items,
                        dir_name: crate::printer::pr_str(&crate::reader::form_to_value(k)),
                    });
                }
                None => {
                    entries.push(Entry::General { bind_form: k, key_form: v });
                }
            }
        }

        // §5: "Can't specify :defaults without :or" -- checked up front,
        // before any binding happens, matching the real compile-time
        // ordering (this throws before real Clojure's `let` even finishes
        // macroexpanding, so no sibling binding is ever observably in
        // scope by the time it fires there either).
        if let Some(d) = &defaults_form {
            if !or_present {
                return Err(self.err_here(RjError::other("Can't specify :defaults without :or"), d.span));
            }
        }

        let mut sel: std::collections::HashSet<Value> = std::collections::HashSet::new();
        let mut b_to_k: std::collections::HashMap<Str, Value> = std::collections::HashMap::new();
        let mut subs: Vec<(Value, Value)> = Vec::new();
        let mut suba: Vec<(Value, Value)> = Vec::new();

        let effective_select = select_form.is_some() || force_select;
        let effective_all = all_form.is_some() || force_all;

        for entry in &entries {
            match entry {
                Entry::Dir {
                    kind,
                    req,
                    mkns,
                    items,
                    dir_name,
                } => {
                    self.bind_directive_entries(
                        *kind,
                        *req,
                        mkns.as_ref(),
                        items,
                        dir_name,
                        &value,
                        &or_defaults,
                        &mut sel,
                        &mut b_to_k,
                        env,
                    )?;
                }
                Entry::General { bind_form, key_form } => {
                    // `match_key` is the RAW, unevaluated key FORM --
                    // `resolve_push_value`'s own `:or`-default MATCHING and
                    // error-message text use it (real Clojure's `push1`
                    // builds both entirely at macro-expansion time, from
                    // the raw read form). `lookup_key` is that form
                    // EVALUATED: the actual `gmap` fetch, AND every OUTPUT
                    // structure (`sel`/`b_to_k`/`subs`/`suba` below) --
                    // real Clojure's equivalents of those get SPLICED into
                    // the generated `let*` as literal source code, so a
                    // raw key form embedded in them is evaluated ONE MORE
                    // TIME once that code actually runs (`OrKey::Lit`'s own
                    // doc has the full oracle-confirmed example). Both
                    // coincide for any self-evaluating key (`:a`/`"a"`/`0`,
                    // the overwhelming common case); they diverge for
                    // `'sym` (raw: the list `(quote sym)`, evaluated: the
                    // symbol `sym`) or a bare var/local reference used as a
                    // key.
                    let match_key = crate::reader::form_to_value(key_form);
                    let lookup_key = self.eval_form_in(key_form, env)?;
                    // `sel`/`b_to_k`/`subs`/`suba` all feed OUTPUT
                    // positions (select_keys' key set, dm's symbol
                    // resolution, the `:select`/`:all` merge) -- EVALUATED,
                    // per `OrKey::Lit`'s doc. Only `resolve_push_value`'s
                    // own `:or` MATCHING and error-message printing use the
                    // raw `match_key`.
                    sel.insert(lookup_key.clone());
                    let local_name = form_as_symbol(bind_form).map(|s| s.name.clone());
                    if let Some(name) = &local_name {
                        b_to_k.insert(name.clone(), lookup_key.clone());
                    }
                    let bv = self.resolve_push_value(
                        local_name.as_ref(),
                        &match_key,
                        &lookup_key,
                        false,
                        &value,
                        &or_defaults,
                        env,
                        bind_form.span,
                    )?;
                    if let FormValue::Map(sub_pairs) = &bind_form.value {
                        if effective_select || effective_all {
                            let nested = self.bind_map_pattern(
                                sub_pairs,
                                bv,
                                env,
                                effective_select,
                                effective_all,
                                bind_form.span,
                            )?;
                            if effective_select {
                                if let Some(sv) = nested.select {
                                    subs.push((lookup_key.clone(), sv));
                                }
                            }
                            if effective_all {
                                if let Some(av) = nested.all {
                                    suba.push((lookup_key.clone(), av));
                                }
                            }
                            continue;
                        }
                    }
                    self.bind_pattern(bind_form, bv, env)?;
                }
            }
        }

        if let Some(asp) = &as_pattern {
            self.bind_pattern(asp, value.clone(), env)?;
        }

        // §5 `:or` validation: "new-or-code" (real Clojure's own name for
        // this) is active only when `:or` is paired with `:select`/`:all`/
        // `:defaults` -- otherwise a dangling `:or` key silently no-ops
        // (back-compat, confirmed live against the oracle: row 19 of
        // `compat/destructuring-113.corpus`). W4C-NS: "paired with
        // `:select`/`:all`" means the EFFECTIVE (possibly `force_select`/
        // `force_all`-synthesized) local, not just an EXPLICIT `:select`/
        // `:all` key written on THIS pattern -- real `destmap*` injects a
        // synthetic `:select`/`:all` gensym onto a nested map-pattern that
        // doesn't already have one whenever the PARENT pattern's own
        // `:select`/`:all` is active (`subsel?`/`suball?`), so from that
        // nested pattern's own perspective `select`/`all` (the local var)
        // is just as "present" as if it had been written explicitly.
        // Gating on `select_form`/`all_form` alone (the previous shape)
        // meant a nested `{a :a :or {a 42}}` pattern (no `:select` of its
        // own, only `force_select` from the parent's `:select`) never
        // built `dm` at all, silently dropping its `:or` defaults --
        // measured against the oracle
        // (`compat/w4c-select-oracle-transcript.txt`).
        let new_or_code = or_present && (defaults_form.is_some() || effective_select || effective_all);
        let mut dm: std::collections::HashMap<Value, Value> = std::collections::HashMap::new();
        if defaults_form.is_some() || effective_select || effective_all {
            for (ork, default_form) in &or_defaults {
                let resolved_key = match ork {
                    // Evaluated, NOT the raw form -- `dm` feeds
                    // `:defaults`'s bound value and the `:select`/`:all`
                    // merge, both OUTPUT positions (`OrKey::Lit`'s own doc).
                    OrKey::Lit(_, okey_form) => Some(self.eval_form_in(okey_form, env)?),
                    OrKey::Sym(name) => match b_to_k.get(name) {
                        Some(k) => Some(k.clone()),
                        None => {
                            if new_or_code {
                                return Err(self.err_here(
                                    RjError::other(format!("symbol {name} in :or does not refer to a binding")),
                                    default_form.span,
                                ));
                            }
                            None
                        }
                    },
                };
                if let Some(k) = resolved_key {
                    let v = self.eval_form_in(default_form, env)?;
                    dm.insert(k, v);
                }
            }
            if new_or_code {
                let matched = dm.iter().filter(|(k, _)| sel.contains(*k)).count();
                if matched != or_defaults.len() {
                    let extras: champ::PersistentHashSet<Value> =
                        dm.keys().filter(|k| !sel.contains(*k)).cloned().collect();
                    return Err(self.err_here(
                        RjError::other(format!(
                            "keys {} appear only in :or",
                            crate::printer::pr_str(&Value::Set(extras))
                        )),
                        span,
                    ));
                }
            }
        }

        let mut result = NestedSelectAll { select: None, all: None };

        if effective_select {
            // W4C-NS: real `destmap*` builds `mm#` from `(merge (some-vals
            // dm) gmap (some-vals subs))`, THEN `when-let`s on THAT merge
            // result -- not on `gmap`/`value` alone. `some-vals` returns
            // nil for a map with no non-nil values, so the merge (and thus
            // `:select`) is only genuinely absent when ALL THREE inputs
            // are absent: `dm` (the `:or` defaults) has no non-nil entry,
            // `gmap` itself is falsy, AND `subs` (nested `:select`
            // results) has no non-nil entry either. Gating purely on
            // `value.truthy()` (the previous shape) dropped `:or`
            // defaults whenever the destructured source itself was
            // nil/missing -- measured against the oracle
            // (`compat/w4c-select-oracle-transcript.txt`,
            // `data_structures.clj`'s `select-directive` "defaults can
            // turn nothing into something": `(let [{{a :a :or {a 42}} :n
            // :select s} nil] s)` must be `{:n {:a 42}}`, not `nil`) --
            // `dm`'s default alone must be enough to make the merge
            // truthy even when the map being destructured is `nil`.
            let dm_present = dm.iter().any(|(_, v)| !matches!(v, Value::Nil));
            let subs_present = subs.iter().any(|(_, v)| !matches!(v, Value::Nil));
            if value.truthy() || dm_present || subs_present {
                let mut mm = PMap::new();
                for (k, v) in &dm {
                    if !matches!(v, Value::Nil) {
                        mm.insert(k.clone(), v.clone());
                    }
                }
                if let Value::Map(gm) = &value {
                    for (k, v) in gm.iter() {
                        mm.insert(k.clone(), v.clone());
                    }
                }
                for (k, v) in &subs {
                    if !matches!(v, Value::Nil) {
                        mm.insert(k.clone(), v.clone());
                    }
                }
                let mut out = PMap::new();
                for k in &sel {
                    if let Some(v) = mm.get(k) {
                        out.insert(k.clone(), v.clone());
                    }
                }
                let sv = Value::Map(out);
                if let Some(target) = &select_form {
                    self.bind_pattern(target, sv.clone(), env)?;
                }
                result.select = Some(sv);
            } else {
                // `(when-let [mm# ...] ...)`: the merge itself came back
                // nil (falsy `gmap`, no `:or` defaults, no nested
                // `:select` values) -- `:select` binds `nil` outright, no
                // map is built.
                if let Some(target) = &select_form {
                    self.bind_pattern(target, Value::Nil, env)?;
                }
                result.select = Some(Value::Nil);
            }
        }

        if effective_all {
            let mut mm = PMap::new();
            for (k, v) in &dm {
                if !matches!(v, Value::Nil) {
                    mm.insert(k.clone(), v.clone());
                }
            }
            if let Value::Map(gm) = &value {
                for (k, v) in gm.iter() {
                    mm.insert(k.clone(), v.clone());
                }
            }
            for (k, v) in &suba {
                if !matches!(v, Value::Nil) {
                    mm.insert(k.clone(), v.clone());
                }
            }
            let av = Value::Map(mm);
            if let Some(target) = &all_form {
                self.bind_pattern(target, av.clone(), env)?;
            }
            result.all = Some(av);
        }

        if let Some(target) = &defaults_form {
            let dm_val = Value::Map(dm.iter().map(|(k, v)| (k.clone(), v.clone())).collect());
            self.bind_pattern(target, dm_val, env)?;
        }

        Ok(result)
    }

    /// The `push1`/`resolve_push_value` shared half of both directive
    /// entries (`:keys`/`:strs!`/...) and general `{binding key}` entries:
    /// resolves what value a single destructured name should get, honoring
    /// `:or` (by LOCAL name -- `local_name`, or by literal MAP KEY --
    /// `or_defaults`'s `OrKey::Lit`) and `req` (backed by `req!`'s own
    /// "Missing required key" semantics, via `map_pattern_lookup`'s
    /// present/absent distinction rather than a real global `req!` call, so
    /// the thrown error can carry this destructuring site's span/stack).
    /// Both a symbol's OWN local-default and a MAP-KEY default matching for
    /// the SAME entry is `destmap*`'s "Multiple :or defaults" compile
    /// error; supplying ANY default for a `req`uired entry is "Can't
    /// supply default value for required key".
    /// `match_key`/`lookup_key` are the SAME value for every directive
    /// entry (`:keys`/`:strs!`/...), but DIFFER for a general `{binding
    /// key}` entry: real Clojure's `destmap*` compares `:or` defaults
    /// against the binding's RAW, UNEVALUATED key FORM (`bk`, the reader's
    /// parsed pattern data -- `contains?`/`zipmap`/`=`-style structural
    /// comparisons, never spliced into generated code), but performs the
    /// ACTUAL `(get gmap bk)` lookup against `bk` SPLICED INTO generated
    /// code, i.e. EVALUATED once that code runs. For a self-evaluating key
    /// (`:a`, `"a"`, `0`) these coincide, so almost every call site can
    /// pass the same value twice -- they diverge only for a non-self-
    /// evaluating general-entry key, e.g. `{b 'b}` (raw form `(quote b)`,
    /// evaluated lookup value the symbol `b`) or `{a some-var}` (raw form
    /// the symbol `some-var`, evaluated lookup value whatever it resolves
    /// to) -- both oracle-confirmed live, and the first exercised by
    /// `syms-bang`/`select-or-defaults`'s `'sym`-keyed entries. `sel`/
    /// `b_to_k`/`subs`/`suba` (the caller's own bookkeeping) and this fn's
    /// error messages all use `match_key`, matching `push1`'s `bk` used as
    /// DATA; `map_pattern_lookup` uses `lookup_key`, matching `bk` used as
    /// CODE.
    fn resolve_push_value(
        &mut self,
        local_name: Option<&Str>,
        match_key: &Value,
        lookup_key: &Value,
        req: bool,
        gmap: &Value,
        or_defaults: &[(OrKey, Form)],
        env: &Env,
        span: Span,
    ) -> Result<Value, RjError> {
        let local_default = local_name.and_then(|n| {
            or_defaults
                .iter()
                .find(|(k, _)| matches!(k, OrKey::Sym(s) if s.as_ref() == n.as_ref()))
        });
        let key_default = or_defaults
            .iter()
            .find(|(k, _)| matches!(k, OrKey::Lit(v, _) if v == match_key));
        match (local_default, key_default) {
            (Some(_), Some(_)) => Err(self.err_here(
                RjError::other(format!(
                    "Multiple :or defaults for same key: {} '{}'",
                    crate::printer::pr_str(match_key),
                    local_name.expect("local_default only matches when local_name is Some")
                )),
                span,
            )),
            (Some((_, default_form)), None) | (None, Some((_, default_form))) => {
                if req {
                    return Err(self.err_here(
                        RjError::other(format!(
                            "Can't supply default value for required key: {}",
                            crate::printer::pr_str(match_key)
                        )),
                        span,
                    ));
                }
                let (present, found) = map_pattern_lookup(gmap, lookup_key);
                if present {
                    Ok(found)
                } else {
                    self.eval_form_in(default_form, env)
                }
            }
            (None, None) => {
                let (present, found) = map_pattern_lookup(gmap, lookup_key);
                if present {
                    Ok(found)
                } else if req {
                    Err(self.err_here(
                        RjError::other(format!("Missing required key: {}", crate::printer::pr_str(match_key))),
                        span,
                    ))
                } else {
                    Ok(Value::Nil)
                }
            }
        }
    }

    /// One `:keys`/`:strs`/`:syms` (optionally `!`-required,
    /// optionally namespace-prefixed) directive's vector: walks it left to
    /// right tracking whether `&` has been seen (`preamp`), matching
    /// `destmap*`'s inner loop exactly --
    ///
    /// - before `&`: each item MUST be a plain symbol; it's transformed
    ///   into the map key via `kind`/`mkns` (`xf` in the oracle source),
    ///   bound to its bare (namespace-stripped) name, and (whether or not
    ///   `req`) looked up/validated now.
    /// - `&` itself: allowed exactly once (a second one is "& can only
    ///   appear once in <dir>").
    /// - after `&`: each item is a LITERAL key form (keyword/string/quoted
    ///   symbol -- evaluated as an ordinary expression, which is exactly
    ///   what a reader-quoted `'sym` already is) declared into `:select`/
    ///   `:all`'s key set but bound to nothing; a bare symbol there is
    ///   "'x' - binding symbols can only appear before '&', use keys
    ///   after". A `req`uired directive STILL validates (throws if
    ///   missing) every post-`&` key, just without creating a local for
    ///   it -- `:keys! [a & :b]` on a map missing `:b` throws even though
    ///   `b` is never bound.
    #[allow(clippy::too_many_arguments)]
    fn bind_directive_entries(
        &mut self,
        kind: DirKind,
        req: bool,
        mkns: Option<&Str>,
        items: &[Form],
        dir_name: &str,
        gmap: &Value,
        or_defaults: &[(OrKey, Form)],
        sel: &mut std::collections::HashSet<Value>,
        b_to_k: &mut std::collections::HashMap<Str, Value>,
        env: &Env,
    ) -> Result<(), RjError> {
        let mut preamp = true;
        for item in items {
            if is_amp(item) {
                if preamp {
                    preamp = false;
                } else {
                    return Err(self.err_here(
                        RjError::other(format!("& can only appear once in {dir_name}")),
                        item.span,
                    ));
                }
                continue;
            }
            if !preamp {
                if let Some(sym) = form_as_symbol(item) {
                    return Err(self.err_here(
                        RjError::other(format!(
                            "'{}' - binding symbols can only appear before '&', use keys after",
                            crate::printer::pr_str(&Value::Sym(sym.clone()))
                        )),
                        item.span,
                    ));
                }
            }
            if preamp {
                // §5: a pre-`&` item is usually a plain symbol (`a`,
                // `foo/a`), but real Clojure's `xf` transform calls the
                // generic `namespace`/`name` functions on it -- which ALSO
                // accept a KEYWORD (`clojure.lang.Named` covers both) --
                // so `::a`/`:foo/a` work too (`keys-bang`'s "a broad range
                // of qualified names/declarators" case, oracle-confirmed:
                // `(let [{:keys! [::a & ::b]} {::a 1, ::b 2}] a)` is `1`).
                // Either way `localize` (real Clojure's own name) strips
                // the namespace for the LOCAL binding -- only the derived
                // map KEY keeps it.
                let (ns_from_item, name) = item_ns_name(item).ok_or_else(|| {
                    self.err_here(
                        RjError::other("destructuring: :keys/:strs/:syms entries must be symbols or keywords"),
                        item.span,
                    )
                })?;
                // CLJ-2968: when the DIRECTIVE ITSELF carries a namespace
                // (`:a1/keys`/`:a1/syms`/`:a1/keys!`/`:a1/syms!`, `mkns`
                // is `Some`), real Clojure's spec restricts every pre-`&`
                // item to a SIMPLE (unqualified) symbol -- a keyword item
                // (however spelled) or a symbol that carries its OWN
                // namespace does not conform, and throws at
                // destructure-parse time, before the map's value is even
                // consulted (oracle-verified: `(let [{:a1/keys [b/c]}
                // {:a1/c 1}] c)` throws with a cause reading "... did not
                // conform to spec"). This is UNRELATED to the already-
                // legal `::a`/`:foo/a` keyword items the comment above
                // documents -- those are legal only under a BARE
                // (non-namespaced) directive; a namespaced directive is
                // the strictly narrower CLJ-2968 case.
                if mkns.is_some() && !matches!(&item.value, FormValue::Atom(Value::Sym(s)) if s.ns.is_none()) {
                    return Err(self.err_here(
                        RjError::other(format!(
                            "Call to clojure.core/destructure did not conform to spec: {} - only simple symbols allowed in a namespaced destructuring directive ({dir_name})",
                            crate::printer::pr_str(&crate::reader::form_to_value(item))
                        )),
                        item.span,
                    ));
                }
                let key_val = match kind {
                    DirKind::Keys => {
                        let ns = mkns.cloned().or_else(|| ns_from_item.clone());
                        let s: Str = match &ns {
                            Some(n) => format!("{n}/{name}").into(),
                            None => name.clone(),
                        };
                        self.keywords.intern(&s);
                        Value::Keyword(Keyword::from(s))
                    }
                    DirKind::Syms => {
                        let ns = mkns.cloned().or_else(|| ns_from_item.clone());
                        Value::Sym(Symbol { ns, name: name.clone() })
                    }
                    // Real Clojure's `xf` uses bare `str` for `:strs` --
                    // unlike :keys/:syms, a directive-level namespace
                    // (`:foo/strs`) is NOT forced on; only the item's OWN
                    // namespace (if it has one) survives, exactly like
                    // `(str 'foo/a)` => "foo/a", `(str 'a)` => "a".
                    DirKind::Strs => {
                        let s: Str = match &ns_from_item {
                            Some(n) => format!("{n}/{name}").into(),
                            None => name.clone(),
                        };
                        Value::Str(s)
                    }
                };
                sel.insert(key_val.clone());
                b_to_k.insert(name.clone(), key_val.clone());
                let bv = self.resolve_push_value(
                    Some(&name),
                    &key_val,
                    &key_val,
                    req,
                    gmap,
                    or_defaults,
                    env,
                    item.span,
                )?;
                env.set(Symbol::simple(name), bv);
            } else {
                let key_val = self.eval_form_in(item, env)?;
                sel.insert(key_val.clone());
                if req {
                    self.resolve_push_value(None, &key_val, &key_val, true, gmap, or_defaults, env, item.span)?;
                }
            }
        }
        Ok(())
    }

    /// Real Clojure's map-destructuring path implicitly runs `(if (seq?
    /// gmap) (apply hash-map gmap) gmap)` before doing any `:keys`/`:strs`/
    /// general lookups against the bound value. Without this, the common
    /// `(defn f [& {:keys [a b]}] ...)` variadic-kwargs idiom silently
    /// binds every key to `nil` in mova, because a fn's `& rest` arg is
    /// always a plain `List`, never a `Map` -- and it's the same reason
    /// `(let [{:keys [a]} (list :a 1)] a)` must work too. Non-seq values
    /// (`Map`, `Vector`, `nil`, anything else) pass through unchanged;
    /// delegates the actual seq-shape logic to `seq_to_map_for_destructuring`
    /// (also the `seq-to-map-for-destructuring` builtin's implementation --
    /// §5/M2's 1.11 helper -- so both call sites can never disagree).
    pub(crate) fn coerce_map_pattern_source(&mut self, value: &Value) -> Result<Value, RjError> {
        // MOVA-PATCH: real Clojure's `gmap` is `(if (seq? map) (apply
        // hash-map map) map)` -- for an already-map-shaped value that's
        // an IDENTITY no-op, so `:as` (which binds this fn's return value)
        // must see the ORIGINAL value, metadata included (measured: `(let
        // [{:as m} (with-meta {:x 5} {:m 1})] (meta m))` is `{:m 1}`).
        // Only the seq-of-kvs branch below builds a genuinely fresh map
        // (no meta to preserve there). `map_pattern_lookup` sees through
        // `Value::Meta` itself, so entries still resolve correctly.
        let inner = value.unmeta();
        if !matches!(inner, Value::List(_) | Value::Lazy(_)) {
            return Ok(value.clone());
        }
        seq_to_map_for_destructuring(self, inner)
    }

    /// Attaches the current call stack to a freshly built error at `span`.
    pub(super) fn err_here(&self, err: RjError, span: Span) -> RjError {
        err.with_span(span).with_stack(self.stack_snapshot(), self.source_id)
    }
}

/// Which names a `let`/`loop` binding PATTERN introduces, best-effort:
/// mirrors `compile::resolve::collect_pattern_names`'s shape-walk (kept as a
/// separate, infallible copy here -- this file owns the tree-walker's
/// `let`/`loop` rebind-split, that one owns the compiled tier's poison
/// rule, and neither should have to reach across tiers to stay in sync). A
/// malformed pattern that slips past `validate_pattern_shape` is left for
/// `bind_pattern` to reject with its own error; this fn only needs a
/// decent-effort answer to decide whether a later pair in the SAME vector
/// is shadowing a name, so it silently collects nothing further for a
/// shape it doesn't recognize.
pub(crate) fn collect_let_pattern_names(form: &Form, out: &mut Vec<Symbol>) {
    match &form.value {
        FormValue::Atom(Value::Sym(s)) => out.push(s.clone()),
        FormValue::Vector(items) => {
            for it in items {
                if is_amp(it) || is_as_kw(it) {
                    continue;
                }
                collect_let_pattern_names(it, out);
            }
        }
        FormValue::Map(pairs) => {
            for (k, v) in pairs {
                match form_keyword_name(k) {
                    // `:or`'s keys name bindings introduced elsewhere in
                    // the same pattern (by `:keys`/`:strs`/`:syms`), not
                    // new ones.
                    Some("or") => {}
                    Some("as") => collect_let_pattern_names(v, out),
                    Some("keys" | "strs" | "syms") => {
                        if let FormValue::Vector(items) = &v.value {
                            for it in items {
                                collect_let_pattern_names(it, out);
                            }
                        }
                    }
                    _ => collect_let_pattern_names(k, out),
                }
            }
        }
        _ => {}
    }
}

/// True if `form` is, at its own top level, a literal `(fn ...)`/`(fn* ...)`
/// form -- the building block `eval_let`/`eval_loop` use to detect a
/// `letfn`-style mutual-recursion run (see their doc comments / the
/// compiled tier's `rec_group_run`, `compile/resolve.rs`).
pub(crate) fn form_is_fn_literal(form: &Form) -> bool {
    match &form.value {
        FormValue::List(items) => matches!(items.first().map(|f| &f.value),
            Some(FormValue::Atom(Value::Sym(s))) if s.ns.is_none() && (s.name.as_ref() == "fn" || s.name.as_ref() == "fn*")),
        _ => false,
    }
}

/// Syntactic (pre-evaluation) scan for a nested `fn`/`fn*` literal anywhere
/// in `form` -- the signal `eval_let`/`eval_loop` use to decide whether an
/// earlier binding pair MAY have created a closure capable of capturing a
/// name free (see their doc comments). Deliberately cheap and
/// over-approximate: it does not resolve macros, so a macro that expands to
/// a `fn` without spelling `fn`/`fn*` directly in this form is missed (rare
/// in practice -- `#(...)` reader syntax already reads as `fn*`, and
/// `letfn`'s own expansion spells `fn` directly, core/core.mova ~329-344).
/// Missing a case only reintroduces the tree-walker's old one-frame
/// behavior for that specific pattern; it can never wrongly split letfn's
/// forward references, since the split ALSO requires the rebound name to
/// already resolve to something (see the `get(n).is_some()` check at both
/// call sites).
pub(crate) fn form_has_nested_fn(form: &Form) -> bool {
    match &form.value {
        FormValue::List(items) => {
            if let Some(first) = items.first() {
                if let FormValue::Atom(Value::Sym(s)) = &first.value {
                    if s.ns.is_none() && (s.name.as_ref() == "fn" || s.name.as_ref() == "fn*") {
                        return true;
                    }
                }
            }
            items.iter().any(form_has_nested_fn)
        }
        FormValue::Vector(items) | FormValue::Set(items) => items.iter().any(form_has_nested_fn),
        FormValue::Map(pairs) => pairs.iter().any(|(k, v)| form_has_nested_fn(k) || form_has_nested_fn(v)),
        _ => false,
    }
}

/// True if `form` is the bare `&` symbol (sequential-destructuring / fn
/// param rest marker).
pub(crate) fn is_amp(form: &Form) -> bool {
    matches!(&form.value, FormValue::Atom(Value::Sym(s)) if s.ns.is_none() && s.name.as_ref() == "&")
}

/// True if `form` is the `:as` keyword.
pub(crate) fn is_as_kw(form: &Form) -> bool {
    form_keyword_name(form) == Some("as")
}

/// Parses a `(catch ...)` clause's head -- everything after the literal
/// `catch` symbol, i.e. `items[1..]` of the whole clause, guaranteed
/// non-empty by both call sites. Class-tolerant (R2), now class-USING
/// (C3g): if the FIRST symbol looks like a class name (dotted, or
/// capitalized) AND there's a second symbol to serve as the binding, that
/// second symbol is the binding and the class token is returned as
/// `Some(class)` -- matched at catch-time by `catch_class_matches`, NEVER
/// resolved/evaluated as a var (several names the corpus actually catches,
/// e.g. `ArithmeticException`, are not registered `ClassVal`s at all; see
/// that fn's own doc). Otherwise the first symbol itself is the (untyped,
/// `None`-classed) binding, which matches UNCONDITIONALLY at catch-time --
/// unchanged from every pre-C3g single-catch call site. Returns `(class,
/// binding, consumed)` where `consumed` is how many leading elements of
/// `rest` the head used (1 for untyped, 2 for class-tolerant) -- the
/// caller's catch body is `rest[consumed..]`.
///
/// Shared by BOTH tiers (`Interp::eval_try` / `compile::resolve`'s
/// `compile_try`) so they cannot disagree about which shape a given clause
/// is -- exactly the same reasoning as `crate::ns`'s one resolution order.
///
/// The ambiguity the heuristic resolves in favor of the class-form: a
/// two-symbol head `(catch E e)` where `E` happens to be uppercase reads as
/// "class `E`, binding `e`", not "binding `E`, body `(e)`" -- deliberate,
/// documented in the R2 mission notes.
pub(crate) fn parse_catch_head(rest: &[Form]) -> Option<(Option<Symbol>, Symbol, usize)> {
    if rest.len() >= 2 {
        if let FormValue::Atom(Value::Sym(class_sym)) = &rest[0].value {
            if looks_like_class_name(class_sym) {
                return match &rest[1].value {
                    FormValue::Atom(Value::Sym(bind)) => Some((Some(class_sym.clone()), bind.clone(), 2)),
                    _ => None,
                };
            }
        }
    }
    match &rest[0].value {
        FormValue::Atom(Value::Sym(bind)) => Some((None, bind.clone(), 1)),
        _ => None,
    }
}

/// A syntax-only "is this a class name" heuristic: dotted (like
/// `jank.runtime.object_ref`) or capitalized (like `Exception`). Used only
/// to decide which shape a `catch` clause's head is (see
/// `parse_catch_head`) -- mova has no reflective class hierarchy to
/// resolve the token against at PARSE time; the returned symbol is matched
/// at CATCH time instead, by `catch_class_matches` below.
fn looks_like_class_name(sym: &Symbol) -> bool {
    sym.ns.is_none()
        && (sym.name.contains('.') || sym.name.chars().next().is_some_and(|c| c.is_uppercase()))
}

/// C3g: whether a typed `(catch class_sym ...)` clause should catch `err`.
/// `class_sym` is the RAW symbol written at the catch site (e.g.
/// `ArithmeticException` or `java.lang.ArithmeticException`) -- never
/// resolved/evaluated as a var (`looks_like_class_name`'s doc explains why:
/// several names the corpus catches by, like `ArithmeticException` or
/// `clojure.lang.ArityException`, are not registered `ClassVal`s at all, so
/// resolving them would error where today's class-blind `catch` does not).
/// Two independent matching paths:
///
/// - `err.kind == ErrorKind::Thrown` (a script-level `(throw v)`): match
///   the actual thrown `Value`'s own ancestry (`thrown_value_class_chain`)
///   -- a host exception `Value::Inst` or an `ex-info` map.
/// - every other kind: an INTERNAL mova error. Matched against a fixed,
///   oracle-measured `ErrorKind` -> ancestor-chain table
///   (`error_kind_class_chain`) so e.g. `(catch ArithmeticException ...)`
///   catches a checked-arithmetic overflow and `(catch Exception ...)` /
///   `(catch Throwable ...)` still catch everything internal, exactly like
///   the untyped single-catch fast path used to.
pub(crate) fn catch_class_matches(class_sym: &Symbol, err: &RjError) -> bool {
    if err.kind == ErrorKind::Thrown {
        let thrown = err.thrown.as_ref().unwrap_or(&Value::Nil);
        catch_class_name_in_chain(class_sym, &thrown_value_class_chain(thrown))
    } else {
        catch_class_name_in_chain(class_sym, &error_kind_class_chain(err))
    }
}

/// Whether class-name symbol `class_sym` (as written at a `catch` site)
/// names one of `chain`'s fully-qualified ancestor class names: either an
/// exact match (`java.lang.ArithmeticException` against
/// `java.lang.ArithmeticException`) or a match against the chain entry's
/// bare, last-dot-segment name (`ArithmeticException` against
/// `java.lang.ArithmeticException`) -- mirroring the way `java.lang`/
/// `clojure.lang` classes are auto-imported in real Clojure and so may be
/// spelled either way at a real `catch` site.
fn catch_class_name_in_chain(class_sym: &Symbol, chain: &[impl AsRef<str>]) -> bool {
    let written = match &class_sym.ns {
        Some(ns) => format!("{ns}/{}", class_sym.name),
        None => class_sym.name.to_string(),
    };
    chain.iter().any(|full| {
        let full = full.as_ref();
        // `rsplit` iterates dot-segments RIGHT to LEFT, so its first item
        // is the class's bare (last-dot-segment) name -- `.next()`, not
        // `.next_back()` (which would undo the reversal and hand back the
        // FIRST segment, e.g. "java", exactly the wrong end).
        full == written || full.rsplit('.').next() == Some(written.as_str())
    })
}

/// Ancestor-chain class names (own class first, then each superclass up to
/// `Throwable`) an internal `RjError` is matched against by a typed
/// `catch` clause -- the sibling of `thrown_value_class_chain`, which
/// covers `ErrorKind::Thrown` instead. Each mapping is chosen to match
/// what the real JVM actually throws for the analogous condition, kept
/// intentionally narrow to what `tests/clojure-suite/vendor*` catch
/// clauses AND `thrown?`/`thrown-with-msg?`/`thrown-with-cause-msg?`/
/// `fails-with-cause?` calls actually name (surveyed at C3g landing time
/// across the whole vendored corpus, not just literal `catch` clauses --
/// see the landing commit's class-count table). Most kinds resolve on
/// `err.kind` alone; `ErrorKind::Other` -- the catch-all bucket for every
/// internal error too varied to give its own `ErrorKind` -- additionally
/// SNIFFS `err.message` for two narrow, hand-written, stable substrings
/// this module's own Rust source produces consistently (same class of
/// technique `RjError::is_incomplete` already uses on reader-error
/// messages): "out of bounds" (every vector/string/array index-range
/// error in `builtins::collections`/`builtins::strings`, grepped:
/// ALWAYS this exact phrase, never used for anything else) and the
/// literal substring `"UnsupportedOperationException"` (which
/// `builtins::recorddot`'s own `unsupported()` helper spells out
/// VERBATIM in the message it builds for record-mutation attempts,
/// specifically so this sniff has something stable to match -- see that
/// fn's doc). Both are high-confidence, low-risk, oracle-measured
/// refinements: real ancestry is `StringIndexOutOfBoundsException <:
/// IndexOutOfBoundsException <: RuntimeException` and
/// `UnsupportedOperationException <: RuntimeException` respectively, and
/// no vendored assertion catches one of these on a value that would
/// wrongly answer true for the other (verified by grep: the corpus's
/// four `StringIndexOutOfBoundsException` catches are ALL on a string
/// `nth`, its 18 `IndexOutOfBoundsException` catches never also name
/// `StringIndexOutOfBoundsException` for the same call). Everything else
/// still honestly answers `Exception`/`RuntimeException`/`Throwable`
/// (real Clojure's own built-in errors are, without exception,
/// `RuntimeException` subclasses), so a broad `(catch Exception e ...)`/
/// `(catch Throwable e ...)` keeps matching every internal error exactly
/// like the untyped catch used to. NOT sniffed (disclosed, deliberate
/// gaps, not chased further -- see the landing commit's report): a
/// `NullPointerException`-shaped condition (mova's own "expected X, got
/// nil" `TypeErr` wording is used far too broadly across unrelated
/// validation failures to sniff without guessing) and `IllegalArgumentException`
/// beyond the two ancestor chains that already carry it (`Arity`, and any
/// host `Value::Inst` actually constructed as one) -- inventing a message
/// sniff for either would go deeper than this task's own "never deeper
/// than the suite measurably demands" mandate.
pub(crate) fn error_kind_class_chain(err: &RjError) -> Vec<&'static str> {
    // field4/W-LENS-1: one class-chain REBUILD -- a fresh `Vec` per `catch`
    // clause per throw, thrown away as soon as the match is decided. The
    // chains are constant; only their allocation is not. This counter is
    // the revival trigger for the declined exception-cost items.
    crate::lens::event(crate::lens::Event::CatchChainRebuild);
    // W3a: a site that MEASURED which JVM class real Clojure raises for
    // exactly this condition wins over the coarse per-kind table below --
    // see `error::JvmClass`. This is the extensible replacement for the two
    // `ErrorKind::Other` message sniffs at the bottom of this function,
    // which are kept only as the fallback for the index/record-mutation
    // sites that have not been individually tagged.
    if let Some(class) = err.jvm_class {
        return class.chain().to_vec();
    }
    match err.kind {
        ErrorKind::DivideByZero | ErrorKind::Arithmetic => vec![
            "java.lang.ArithmeticException",
            "java.lang.RuntimeException",
            "java.lang.Exception",
            "java.lang.Throwable",
        ],
        // Real ancestry: `clojure.lang.ArityException extends
        // IllegalArgumentException` -- measured in `errors.clj`'s own
        // `arity-exception` deftest, which checks `f0`'s wrong-arity call
        // with `(thrown-with-msg? IllegalArgumentException ...)` (the
        // "pre-1.3" superclass) alongside sibling assertions using
        // `ArityException` directly.
        ErrorKind::Arity => vec![
            "clojure.lang.ArityException",
            "java.lang.IllegalArgumentException",
            "java.lang.RuntimeException",
            "java.lang.Exception",
            "java.lang.Throwable",
        ],
        // Bad-cast/wrong-shape argument errors -- `ClassCastException`
        // extends `RuntimeException` directly on the JVM.
        ErrorKind::TypeErr => vec![
            "java.lang.ClassCastException",
            "java.lang.RuntimeException",
            "java.lang.Exception",
            "java.lang.Throwable",
        ],
        // Real Clojure's own symbol-resolution failure is raised as a bare
        // `RuntimeException` (`Compiler.java`'s `resolveIn`: `new
        // RuntimeException("Unable to resolve symbol: " + ...)`) -- but it
        // is raised DURING COMPILATION, and nothing outside the compiler
        // ever sees it in that form: `Compiler.load`/`eval` catch it and
        // rethrow it wrapped, so what reaches user code is always a
        // `clojure.lang.Compiler$CompilerException` with the
        // `RuntimeException` as its `.getCause`. W3a measurement (every row
        // from the live 1.13.0-alpha6 oracle):
        //
        //   (eval 'bar)             => Compiler$CompilerException
        //                              cause RuntimeException
        //                              "Unable to resolve symbol: bar in this context"
        //   (eval 'if)              => same shape (special-form names are
        //                              not resolvable as values either)
        //   (eval 'java.lang.FooBar)=> Compiler$CompilerException
        //                              cause ClassNotFoundException
        //   (eval '(foobar 1 2))    => Compiler$CompilerException
        //                              cause RuntimeException
        //
        // Every `RjError::unresolved` call site in this crate is one of
        // those conditions (a symbol/var/class name that will not resolve;
        // grepped: `eval::eval_form_in`, `compile::exec::unresolved`,
        // `set!`, `binding`, `defmethod`, and `types_forms`'s three
        // dot-form heads), so this maps uniformly. mova has no cause chain
        // to hang the inner `RuntimeException` off, so the chain carries
        // BOTH classes -- which is exactly true anyway, since
        // `Compiler$CompilerException extends RuntimeException` (measured).
        // Nothing that matched this chain before stops matching: the
        // `RuntimeException`/`Exception`/`Throwable` tail is unchanged, a
        // strictly more specific head was prepended.
        ErrorKind::Unresolved => vec![
            "clojure.lang.Compiler$CompilerException",
            "java.lang.RuntimeException",
            "java.lang.Exception",
            "java.lang.Throwable",
        ],
        // Syscall failures (`builtins::sys`) -- `java.io.IOException`,
        // a CHECKED `Exception` direct subclass (not `RuntimeException`).
        ErrorKind::Sys => vec!["java.io.IOException", "java.lang.Exception", "java.lang.Throwable"],
        ErrorKind::Interrupted => vec!["java.lang.InterruptedException", "java.lang.Exception", "java.lang.Throwable"],
        ErrorKind::Reader => vec![
            "clojure.lang.LispReader$ReaderException",
            "java.lang.RuntimeException",
            "java.lang.Exception",
            "java.lang.Throwable",
        ],
        // `Other` is the catch-all bucket for every internal error this
        // module has no dedicated `ErrorKind` for -- message-sniffed for
        // the two narrow, stable, high-confidence patterns documented
        // above; everything else in this bucket gets the same honest
        // `RuntimeException`-only default every other unmapped kind gets,
        // rather than an invented, unmeasured leaf class.
        ErrorKind::Other => {
            if err.message.contains("out of bounds") {
                vec![
                    "java.lang.StringIndexOutOfBoundsException",
                    "java.lang.IndexOutOfBoundsException",
                    "java.lang.RuntimeException",
                    "java.lang.Exception",
                    "java.lang.Throwable",
                ]
            } else if err.message.contains("UnsupportedOperationException") {
                vec![
                    "java.lang.UnsupportedOperationException",
                    "java.lang.RuntimeException",
                    "java.lang.Exception",
                    "java.lang.Throwable",
                ]
            } else {
                vec!["java.lang.RuntimeException", "java.lang.Exception", "java.lang.Throwable"]
            }
        }
        // Never reach catch-class matching: both callers (`eval_try`/
        // `compile::exec::exec_try`) exclude these before ever consulting
        // a catch clause (`Recur`/`FuelExhausted` are control signals, not
        // script-visible errors -- see their own doc comments), and
        // `Thrown` is handled by `thrown_value_class_chain` instead of
        // this table, in `catch_class_matches` above. Empty, not absent,
        // so the match stays exhaustive without a wildcard arm hiding a
        // future `ErrorKind` variant from this table.
        ErrorKind::Recur | ErrorKind::FuelExhausted | ErrorKind::InterruptedHard | ErrorKind::Thrown => vec![],
    }
}

/// Ancestor-chain class names for a value passed to `(throw v)` -- the
/// sibling of `error_kind_class_chain`, covering `ErrorKind::Thrown`
/// instead of every other kind. Two shapes mova can actually produce carry
/// SPECIFIC ancestry: a host exception `Value::Inst` (`tdef.name` +
/// `tdef.interfaces`, both already fully-qualified -- see
/// `hostclass.rs`'s exception-veneer module doc) and an `ex-info` map
/// (identified the same way `core.mova`'s own `ex-message`/`Throwable->map`
/// do: the `:ex/message` key is present), which real Clojure represents as
/// `clojure.lang.ExceptionInfo`, a measured `RuntimeException` subclass.
///
/// W-ERR (field2, host application field report): every OTHER thrown value (a plain
/// string, a keyword, a number, `nil`, ...) used to return an EMPTY chain
/// here, so `(catch Exception e ...)`/`(catch Throwable e ...)` -- the
/// exact idiom real-world embedders reach for to guard foreign/plugin code
/// -- silently failed to catch it and let it escape the process. Real
/// Clojure indeed requires `throw`'s argument to already BE a `Throwable`,
/// so there is genuinely no JVM-faithful SPECIFIC class to attribute to a
/// bare thrown keyword/number/string; but mova, unlike the JVM, actually
/// PERMITS throwing arbitrary values, so refusing to classify them at all
/// made `catch` non-total for a whole category of real programs -- and an
/// embedder cannot reasonably be expected to predict every shape of value
/// third-party/plugin code might throw before deciding whether `Exception`
/// or `Throwable` should catch it. The fix: fall back to the same generic
/// tail every internal-error kind in `error_kind_class_chain` already ends
/// in (`RuntimeException`/`Exception`/`Throwable`, `ErrorKind::Other`'s own
/// default arm) -- there is no more-specific HONEST class to give it, but
/// the generic tail is not a guess, it's the same "some kind of runtime
/// exception" answer every unclassified internal error already gets. This
/// makes typed `catch Exception`/`catch Throwable` TOTAL: nothing mova can
/// throw, internal or user-thrown, escapes an "exception" or "throwable"
/// -class catch anymore (owner directive, W-ERR spec). `instance?` is
/// deliberately NOT changed to match -- `(instance? Exception 42)` stays
/// `false`, since `instance?` answers a narrower, JVM-faithful "is this
/// really shaped like one" question that `catch`'s "should this be treated
/// as if it derives from Exception/Throwable" question does not need to
/// share; see tests/conformance/DEVIATIONS.md for that now-visible
/// asymmetry, disclosed rather than papered over.
fn thrown_value_class_chain(v: &Value) -> Vec<Str> {
    // field4/W-LENS-1: the `ErrorKind::Thrown` sibling of
    // `error_kind_class_chain`'s rebuild -- same event, same consumer.
    crate::lens::event(crate::lens::Event::CatchChainRebuild);
    match v {
        Value::Inst(inst) => {
            let mut chain = Vec::with_capacity(1 + inst.tdef.interfaces.len());
            chain.push(inst.tdef.name.clone());
            chain.extend(inst.tdef.interfaces.iter().cloned());
            chain
        }
        Value::Map(m) if m.get(&Value::Keyword(Keyword::from("ex/message"))).is_some() => vec![
            Str::from("clojure.lang.ExceptionInfo"),
            Str::from("java.lang.RuntimeException"),
            Str::from("java.lang.Exception"),
            Str::from("java.lang.Throwable"),
        ],
        _ => vec![
            Str::from("java.lang.RuntimeException"),
            Str::from("java.lang.Exception"),
            Str::from("java.lang.Throwable"),
        ],
    }
}

pub(crate) fn form_keyword_name(form: &Form) -> Option<&str> {
    if let FormValue::Atom(Value::Keyword(k)) = &form.value {
        Some(k.as_ref())
    } else {
        None
    }
}

pub(crate) fn symbol_name_of_pattern(form: &Form) -> Option<Str> {
    if let FormValue::Atom(Value::Sym(s)) = &form.value {
        Some(s.name.clone())
    } else {
        None
    }
}

/// §5/M2 (1.13 required-keys destructuring, `bind_map_pattern`): a key in a
/// `:or {...}` map is either a bare SYMBOL -- matched against the bare
/// LOCAL name a binding introduces (`destmap*`'s `local-default?`) -- or a
/// literal value (keyword/string/quoted-symbol/anything else `form_to_value`
/// produces) -- matched against the actual MAP KEY a binding resolves to
/// (`key-default?`). An `:or` entry can supply a default via EITHER route;
/// matching both for the very same binding is the "Multiple :or defaults"
/// compile error (`resolve_push_value`).
#[derive(Clone)]
pub(crate) enum OrKey {
    Sym(Str),
    /// Carries BOTH the raw (unevaluated) form value -- used by
    /// `resolve_push_value`'s `key_default` matching and its error-message
    /// printing, matching real Clojure's `push1`, which builds these
    /// entirely at macro-expansion time from the RAW read form -- and the
    /// literal key FORM itself, re-evaluated on demand wherever an `:or`
    /// entry's key ends up in an OUTPUT structure (`dm`, ultimately
    /// `:defaults`'s bound value and the `:select`/`:all` merge): real
    /// Clojure's `dm` gets SPLICED into the generated `let*`'s bindings as
    /// literal source code, so a non-self-evaluating key form (`(quote
    /// b)`) is evaluated ONE MORE TIME when that generated code actually
    /// runs -- oracle-confirmed live (`select-or-defaults`'s `'b`-keyed
    /// `:defaults` row: the bound `d` has the SYMBOL `b` as its key, not
    /// the list `(quote b)`).
    Lit(Value, Form),
}

/// §5/M2: which family a `:keys`/`:strs`/`:syms` (optionally `!`-suffixed
/// and/or namespace-prefixed, e.g. `:foo/keys!`) directive belongs to --
/// decides how a PRE-`&` raw symbol turns into the map key it looks up
/// (`destmap*`'s `xf`).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum DirKind {
    Keys,
    Strs,
    Syms,
}

/// §5/M2: what a map pattern's own `:select`/`:all` directives computed,
/// read back by an enclosing map pattern that force-recursed into this one
/// (`destmap*`'s `subs`/`suba`: a PARENT with its own `:select`/`:all`
/// wants a nested map-valued entry's FILTERED/FULL view, not the raw
/// sub-map, when building its OWN `:select`/`:all` output). `None` in a
/// slot means that computation was never requested (`force_select`/
/// `force_all` both false, and this pattern doesn't declare the directive
/// itself either) -- distinct from `Some(Value::Nil)`, which means it WAS
/// computed and genuinely came out `nil` (e.g. `:select` over a `nil`
/// source map).
pub(crate) struct NestedSelectAll {
    pub select: Option<Value>,
    pub all: Option<Value>,
}

/// Splits a directive keyword's flat `"ns/name"` (or bare `"name"`) repr --
/// `Value::Keyword`'s own representation, see that variant's doc -- into
/// `(namespace, bare-name)`, e.g. `"foo/keys!"` -> `(Some("foo"), "keys!")`,
/// `"keys!"` -> `(None, "keys!")`. Used only for the `:keys`/`:strs`/`:syms`
/// family (`:or`/`:as`/`:select`/`:all`/`:defaults` are matched by exact
/// flat-string equality before this ever runs, so they're never
/// namespaced here even though `::as` would technically parse).
fn split_directive_ns(flat: &str) -> (Option<Str>, &str) {
    match flat.rfind('/') {
        Some(i) if i > 0 && i < flat.len() - 1 => (Some(Str::from(&flat[..i])), &flat[i + 1..]),
        _ => (None, flat),
    }
}

/// §5/M2 (`bind_directive_entries`): extracts `(namespace, bare-name)` from
/// a pre-`&` `:keys`/`:strs`/`:syms` item, which real Clojure's `xf`
/// accepts as EITHER a symbol (`a`, `foo/a`) or a keyword (`:a`, `:foo/a`,
/// `::a`) -- both implement `clojure.lang.Named`, which is all `xf` (via
/// `namespace`/`name`) actually requires. `None` for anything else (a
/// string, a nested pattern, ...), which the caller turns into
/// "destructuring: :keys/:strs/:syms entries must be symbols or keywords".
fn item_ns_name(item: &Form) -> Option<(Option<Str>, Str)> {
    match &item.value {
        FormValue::Atom(Value::Sym(s)) => Some((s.ns.clone(), s.name.clone())),
        FormValue::Atom(Value::Keyword(k)) => {
            let (ns, name) = split_directive_ns(k.as_ref());
            Some((ns, Str::from(name)))
        }
        _ => None,
    }
}

/// §5/M2 (also the `seq-to-map-for-destructuring` builtin, 1.11): builds a
/// map from a seq exactly as real Clojure's `seq-to-map-for-destructuring`/
/// `destmap*`'s inline `gmap` coercion do (`RT.java`'s
/// `PersistentArrayMap.createAsIfByAssoc`, measured against the oracle):
///
/// - 0 elements -> `{}`.
/// - 1 element -> that element ITSELF (not wrapped in a map) -- this is
///   what makes a fn's `& {:keys [a b]}` kwargs idiom accept a single
///   trailing map argument (`(f {:a 1 :b 2})`) as well as inline pairs
///   (`(f :a 1 :b 2)`), and is `singleton-map-in-destructure-context`'s
///   whole point: `(let [{:keys [a] :as m} (list {:a 1 :b 2})] ...)`
///   binds `m` to the map itself, not `{(list ...) nil}`.
/// - 2+ elements, EVEN count -> alternating key/value pairs folded into a
///   map (later entries overwrite earlier ones on a duplicate key, same as
///   sequential `assoc`).
/// - 2+ elements, ODD count -> the same pairing for all but the LAST
///   element, which is a TRAILING value merged in as extra key/value pairs
///   (a `Map`'s entries, or `nil`/absent -- Clojure's `IPersistentMap` merges
///   via `cons`, which uses the SAME "map -> its entries" convention);
///   this is the "trailing map destructuring" kwargs convention
///   (`(f :a 1 {:b 2})`).
pub(crate) fn seq_to_map_for_destructuring(interp: &mut Interp, value: &Value) -> Result<Value, RjError> {
    let mut elems: Vec<Value> = Vec::new();
    let mut cur = value.clone();
    while let Some((h, t)) = crate::builtins::uncons(interp, &cur)? {
        elems.push(h);
        cur = t;
    }
    if elems.is_empty() {
        return Ok(Value::Map(PMap::new()));
    }
    if elems.len() == 1 {
        return Ok(elems.into_iter().next().unwrap());
    }
    let trailing = if elems.len() % 2 == 1 { elems.pop() } else { None };
    let mut m = PMap::new();
    let mut it = elems.into_iter();
    while let (Some(k), Some(v)) = (it.next(), it.next()) {
        m.insert(k, v);
    }
    if let Some(t) = trailing {
        match t {
            Value::Map(tm) => {
                for (k, v) in tm.iter() {
                    m.insert(k.clone(), v.clone());
                }
            }
            Value::Vector(items) | Value::MapEntry(items) if items.len() == 2 => {
                m.insert(items[0].clone(), items[1].clone());
            }
            other => {
                return Err(RjError::other(format!(
                    "seq-to-map-for-destructuring: trailing element must be a map or a key/value pair, got {}",
                    crate::printer::pr_str(&other)
                )))
            }
        }
    }
    Ok(Value::Map(m))
}

/// `(present?, value-or-nil)` map-pattern lookup: maps look up by key
/// directly; vectors accept a non-negative int index (so `{a 0}` can pull
/// the first element of a vector); anything else (including `nil`) reads
/// as "not present" rather than erroring, matching `get`'s permissiveness.
pub(crate) fn map_pattern_lookup(coll: &Value, key: &Value) -> (bool, Value) {
    match coll {
        // MOVA-PATCH: coerce_map_pattern_source now keeps a map's own
        // metadata (for `:as`), so entries lookup must see through it here.
        Value::Meta(m) => map_pattern_lookup(&m.inner, key),
        Value::Map(m) => {
            crate::builtins::map_probe::record("destructure-get", m.len());
            match m.get(key) {
                Some(v) => (true, v.clone()),
                None => (false, Value::Nil),
            }
        }
        // W3: `{:keys [...]}`/`{:strs [...]}`/general `{binding key}`
        // destructuring over a `HostStruct` -- touch-only fast path for
        // the (overwhelmingly common) keyword-key case, `as_pmap`
        // fallback for anything else (`:strs`'s `Value::Str` keys, or a
        // general `{binding key}` pair whose key isn't a keyword).
        // S3: `{:keys [...]}` destructuring over a record (measured:
        // `(let [{:keys [a b]} (->R 1 2)] ...)` binds the fields).
        Value::Inst(inst) if inst.tdef.is_record => match inst.data.get(key) {
            Some(v) => (true, v.clone()),
            None => (false, Value::Nil),
        },
        Value::HostStruct(hs) => match key {
            Value::Keyword(kw) => match crate::host_struct::shape_field_index(&hs.shape, kw.text_ref()) {
                Some(idx) => (true, crate::host_struct::get_field(hs, idx)),
                None => (false, Value::Nil),
            },
            _ => match crate::host_struct::as_pmap(hs).get(key) {
                Some(v) => (true, v.clone()),
                None => (false, Value::Nil),
            },
        },
        Value::LazyMap(lm) => match key {
            Value::Keyword(kw) => match crate::lazy_map::lookup(lm, kw.text_ref()) {
                Some(v) => (true, v),
                None => (false, Value::Nil),
            },
            _ => (false, Value::Nil),
        },
        // S7: `{a 0}`-style index destructuring over a map ENTRY works
        // like it does over the 2-vector the entry is.
        Value::Vector(items) | Value::MapEntry(items) => match key {
            Value::Int(n) if *n >= 0 && (*n as usize) < items.len() => (true, items[*n as usize].clone()),
            _ => (false, Value::Nil),
        },
        _ => (false, Value::Nil),
    }
}

/// `defmacro`'s docstring + attr-map support -- the same `name
/// doc-string? attr-map? ([params] body)+ attr-map?` structural shape
/// `defn` parses (`core.mova`), reimplemented here since `defmacro` is a
/// Rust special form with no `core.mova` macro layer of its own to do the
/// parsing. Order is STRICT, matching the oracle (measured: `(defmacro m
/// {..} "doc" [x] x)` is a real `clojure.core.specs.alpha` syntax error in
/// real Clojure -- a leading map before a docstring is not recognized as
/// an attr-map at all, it's read as the params vector's replacement and
/// fails to parse; a previous version of this comment claimed "either
/// order" worked, which was never actually measured). When the body uses
/// the multi-arity `([...] ...)+` shape, one more attr-map may trail
/// *after* the last arity clause (`(defmacro m ([a] a) {..})`). A trailing
/// map is only special-cased for the multi-arity shape -- for a single
/// `[params] body...` arity a trailing map is just the last body form
/// (`(defmacro m [] 42 {:x 1})` macroexpands with `{:x 1}` staying in the
/// body, not stripped, exactly like `defn`'s oracle-measured behavior).
/// Returns the cleaned `[name, arity-forms...]` sequence ready for
/// `parse_fn_like`, plus the raw docstring/leading-attr/trailing-attr
/// `Value`s (as literal, UNEVALUATED data -- matching `defn`'s own
/// attr-map handling, which never evaluates attr-map values either) for
/// `eval_defmacro`'s caller (`publish_macro_var_meta`) to merge.
fn extract_macro_doc_and_attrs(
    args: &[Form],
) -> (Vec<Form>, Option<Value>, Option<Value>, Option<Value>) {
    let mut out = vec![args[0].clone()];
    let mut i = 1;
    let mut doc = None;
    if i < args.len() {
        if let FormValue::Atom(s @ Value::Str(_)) = &args[i].value {
            doc = Some(s.clone());
            i += 1;
        }
    }
    let mut leading_attr = None;
    if i < args.len() {
        if let FormValue::Map(_) = &args[i].value {
            leading_attr = Some(crate::reader::form_to_value(&args[i]));
            i += 1;
        }
    }
    let rest = &args[i..];
    if rest.is_empty() {
        return (out, doc, leading_attr, None);
    }
    let is_multi_arity = matches!(
        &rest[0].value,
        FormValue::List(items) if !items.is_empty() && matches!(items[0].value, FormValue::Vector(_))
    );
    let mut trailing_attr = None;
    if is_multi_arity && matches!(&rest[rest.len() - 1].value, FormValue::Map(_)) {
        trailing_attr = Some(crate::reader::form_to_value(&rest[rest.len() - 1]));
        out.extend_from_slice(&rest[..rest.len() - 1]);
    } else {
        out.extend_from_slice(rest);
    }
    (out, doc, leading_attr, trailing_attr)
}

/// For a `fn`/`defmacro` parameter form: if it's already a plain symbol,
/// used as-is (`None` extra); otherwise (a destructuring pattern) an
/// internal `__p<n>` symbol takes the actual parameter's place -- keeping
/// `value.rs`'s `Arity::params: Vec<Symbol>` unchanged -- and the original
/// pattern form is returned paired with a fresh atom-form for that
/// generated symbol, ready to splice into `wrap_let_form`.
fn param_binding(form: &Form, counter: &mut usize) -> (Symbol, Option<(Form, Form)>) {
    if let FormValue::Atom(Value::Sym(s)) = &form.value {
        return (s.clone(), None);
    }
    let name = format!("__p{}", *counter);
    *counter += 1;
    let sym = Symbol::simple(name);
    let sym_form = Form {
        meta: None,
        value: FormValue::Atom(Value::Sym(sym.clone())),
        span: form.span,
    };
    (sym, Some((form.clone(), sym_form)))
}

/// D9: the `^long`/`^double` primitive hint on a `fn` parameter form, if
/// any -- see [`crate::value::PrimCast`] and `Arity::coerce`.
///
/// Reader metadata lives in `Form::meta` as the already-desugared map form
/// (`^long x` and `^{:tag long} x` are the same thing by the time they get
/// here), and a `defn`-expanded param vector arrives with the same shape,
/// because `defn` splices `fdecl` through untouched and `value_to_form`
/// restores a `Value::Meta` as `Form::meta` (verified: `(:arglists (meta
/// #'f))` reports `{:tag long}` on the parameter for `(defn f [^long x]
/// x)`).
///
/// The recognition rule is `Compiler.tagClass` -> `Compiler.primClass`,
/// transcribed, and it is looser than it looks in two ways -- both
/// measured, both because `primClass` tests `sym.name` ALONE:
///
/// * the tag's namespace is ignored (`^{:tag clojure.core/long}` and even
///   `^{:tag foo/long}` coerce);
/// * a STRING tag works too (`^"long"` coerces), since `tagClass` falls
///   through to `HostExpr.tagToClass`, which resolves the same primitive
///   names.
///
/// Anything else -- `^int`, `^float`, `^boolean` (which real Clojure
/// REJECTS at compile time with "Only long and double primitives are
/// supported"), `^Long`, `^Object`, `^longs`, `^String` -- returns `None`
/// and stays the inert reflection hint it already was in mova. Mova does
/// not reproduce the JVM's *rejections* here (that set also includes "fns
/// taking primitives support only 4 or fewer args", "fns taking primitives
/// cannot be variadic" and "& arg cannot have type hint"): they are
/// artifacts of JVM code generation -- there is no `invokePrim` interface
/// past 4 arguments and no primitive `RestFn` -- not of Clojure's
/// semantics, and mova only ever accepts MORE than the oracle here, so no
/// program that compiles on the JVM behaves differently. See
/// `docs/SPEC-PORT-PATCHES.md` item 9.
fn param_prim_cast(form: &Form) -> Option<crate::value::PrimCast> {
    let meta = form.meta.as_deref()?;
    let FormValue::Map(pairs) = &meta.value else {
        return None;
    };
    for (k, v) in pairs {
        match &k.value {
            FormValue::Atom(Value::Keyword(kw)) if &**kw.text_ref() == "tag" => {}
            _ => continue,
        }
        let name: &str = match &v.value {
            FormValue::Atom(Value::Sym(s)) => &s.name,
            FormValue::Atom(Value::Str(s)) => s,
            _ => return None,
        };
        return match name {
            "long" => Some(crate::value::PrimCast::Long),
            "double" => Some(crate::value::PrimCast::Double),
            _ => None,
        };
    }
    None
}

/// Wraps `body` in a synthesized `(let [pat1 __p0 pat2 __p1 ...] body...)`
/// so fn-param destructuring desugars to zero `value.rs` changes (see this
/// module's doc comment / PLAN.md's A3 section).
fn wrap_let_form(bindings: Vec<Form>, body: &[Form], span: Span) -> Form {
    let let_sym = Form {
        meta: None,
        value: FormValue::Atom(Value::Sym(Symbol::simple("let"))),
        span,
    };
    let bindings_vec = Form {
        meta: None,
        value: FormValue::Vector(bindings),
        span,
    };
    let mut list_items = vec![let_sym, bindings_vec];
    list_items.extend(body.iter().cloned());
    Form {
        meta: None,
        value: FormValue::List(list_items),
        span,
    }
}

fn mk_sym_form(name: &str, span: Span) -> Form {
    Form {
        meta: None,
        value: FormValue::Atom(Value::Sym(Symbol::simple(name))),
        span,
    }
}

/// clojure-lsp campaign (mova/PLAN.md): `(when-not cond (throw (new
/// AssertionError (str "Assert failed: " (pr-str 'cond)))))` -- the exact
/// shape real Clojure's own `assert` macro expands to (see the ported
/// copy just above `defmacro assert` in `core.mova`), reused here so a
/// failing `:pre`/`:post` condition throws the SAME class with the SAME
/// message real Clojure does, one throw per condition (never `and`-
/// combined, so the message names the one predicate that actually
/// failed).
fn pre_post_assert_form(cond: Form, span: Span) -> Form {
    let quoted = Form { meta: None, value: FormValue::List(vec![mk_sym_form("quote", span), cond.clone()]), span };
    let pr_str_call = Form { meta: None, value: FormValue::List(vec![mk_sym_form("pr-str", span), quoted]), span };
    let prefix = Form { meta: None, value: FormValue::Atom(Value::Str(Str::from("Assert failed: "))), span };
    let str_call = Form {
        meta: None,
        value: FormValue::List(vec![mk_sym_form("str", span), prefix, pr_str_call]),
        span,
    };
    let new_err = Form {
        meta: None,
        value: FormValue::List(vec![mk_sym_form("new", span), mk_sym_form("AssertionError", span), str_call]),
        span,
    };
    let throw_form = Form { meta: None, value: FormValue::List(vec![mk_sym_form("throw", span), new_err]), span };
    Form {
        meta: None,
        value: FormValue::List(vec![mk_sym_form("when-not", span), cond, throw_form]),
        span,
    }
}

/// clojure-lsp campaign (mova/PLAN.md): real Clojure's `fn`/`defn`
/// `{:pre [...] :post [...]}` condition map -- previously unimplemented
/// (the map was just evaluated as an ordinary, discarded, harmless body
/// form). rewrite-clj's `zip/removez.cljc` writes `{:pre [zloc] :post
/// [%]}` on `remove`, and bare `%` has no meaning outside a `#(...)`
/// literal, so evaluating it as a plain body expression threw "Unable to
/// resolve symbol: %" the moment `remove` was called -- measured via
/// `mova/smoke/rewrite_clj_smoke.clj`.
///
/// Rewritten into ordinary `when-not`/`throw`/`let` forms at PARSE time
/// (mirroring `wrap_let_form`'s destructuring sugar just above it in this
/// file), so no new eval-tier support is needed and the compiled tier
/// sees nothing but forms it already knows: each `:pre` condition is
/// asserted before the real body runs; each `:post` condition after, with
/// `%` `let`-bound to the body's own result, exactly real Clojure's own
/// macroexpansion. A leading map with no `:pre`/`:post` key, or a body of
/// only that one map form, is left untouched -- real Clojure only treats
/// the map as a condition map when at least one of those keys is present
/// AND at least one more body form follows it.
fn wrap_pre_post(body: &[Form], span: Span) -> Vec<Form> {
    let Some((head, rest)) = body.split_first() else {
        return body.to_vec();
    };
    if rest.is_empty() {
        return body.to_vec();
    }
    let FormValue::Map(entries) = &head.value else {
        return body.to_vec();
    };
    let mut pre = None;
    let mut post = None;
    for (k, v) in entries {
        if let FormValue::Atom(Value::Keyword(kw)) = &k.value {
            match kw.text_ref().as_ref() {
                "pre" => pre = Some(v.clone()),
                "post" => post = Some(v.clone()),
                _ => {}
            }
        }
    }
    if pre.is_none() && post.is_none() {
        return body.to_vec();
    }
    let conds_of = |m: Option<Form>| -> Vec<Form> {
        match m.map(|f| f.value) {
            Some(FormValue::Vector(v)) => v,
            _ => Vec::new(),
        }
    };
    let pre_conds = conds_of(pre);
    let post_conds = conds_of(post);
    let mut out: Vec<Form> = pre_conds.into_iter().map(|c| pre_post_assert_form(c, span)).collect();
    if post_conds.is_empty() {
        out.extend(rest.iter().cloned());
    } else {
        let do_body = Form {
            meta: None,
            value: FormValue::List({
                let mut v = vec![mk_sym_form("do", span)];
                v.extend(rest.iter().cloned());
                v
            }),
            span,
        };
        let pct = mk_sym_form("%", span);
        let binding_vec = Form { meta: None, value: FormValue::Vector(vec![pct.clone(), do_body]), span };
        let mut let_items = vec![mk_sym_form("let", span), binding_vec];
        let_items.extend(post_conds.into_iter().map(|c| pre_post_assert_form(c, span)));
        let_items.push(pct);
        out.push(Form { meta: None, value: FormValue::List(let_items), span });
    }
    out
}

/// C3c (errors.clj's `arity-exception` deftest): an `ErrorKind::Arity`
/// error that populated `arity_actual` (both `apply.rs`'s user-fn/macro
/// arity check and `builtins::reg`'s native-builtin one now do, see
/// `RjError::arity_actual`'s own doc) catch-binds to a REAL `clojure.
/// lang.ArityException` instance (`hostclass::mk_arity_exception`) rather
/// than the generic `{:type :error/arity :message ..}` info map every
/// other error kind still gets below -- `.-actual`/`.getMessage` on it
/// then resolve through the ordinary deftype field-access dot-dispatch,
/// same mechanism the S6 exception classes (`Exception.`/
/// `RuntimeException.`/...) already ride. Falls through to the plain map
/// for an arity error that never populated `arity_actual` (there is no
/// such call site left as of this task, but nothing guarantees a FUTURE
/// one couldn't raise `RjError::arity` directly without it).
/// `MOVA_TRACE_CATCH` debug mode: one line per exception a script-level
/// `catch` clause actually catches (swallows), so a host gap that empties
/// clojure-lsp/clj-kondo analysis stops being silent. Unset (default) is
/// `Off` -- zero cost, read once via `OnceLock` like `MOVA_EXPLAIN`.
/// Empty value ("set but no path") writes to stderr (stdout must stay
/// clean for the LSP transport); any other value is a file path, appended.
enum CatchTraceTarget {
    Off,
    Stderr,
    File(String),
}

fn catch_trace_target() -> &'static CatchTraceTarget {
    static TARGET: std::sync::OnceLock<CatchTraceTarget> = std::sync::OnceLock::new();
    TARGET.get_or_init(|| match std::env::var("MOVA_TRACE_CATCH") {
        Err(_) => CatchTraceTarget::Off,
        Ok(v) if v.is_empty() => CatchTraceTarget::Stderr,
        Ok(v) => CatchTraceTarget::File(v),
    })
}

fn trace_catch(e: &RjError, ns: &str) {
    use std::io::Write;
    let target = catch_trace_target();
    if matches!(target, CatchTraceTarget::Off) {
        return;
    }
    let class = e
        .jvm_class
        .map(|c| format!("{c:?}"))
        .unwrap_or_else(|| format!("{:?}", e.kind));
    let line = format!("[trace-catch] ns={ns} class={class} message={}\n", e.message);
    match target {
        CatchTraceTarget::Stderr => {
            let _ = std::io::stderr().write_all(line.as_bytes());
        }
        CatchTraceTarget::File(path) => {
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
                let _ = f.write_all(line.as_bytes());
            }
        }
        CatchTraceTarget::Off => unreachable!(),
    }
}

pub(crate) fn error_to_info_map(e: &RjError) -> Value {
    // W3a: a compile/macroexpand-time rejection that ALSO carries the
    // condition it wrapped catch-binds to a real
    // `clojure.lang.Compiler$CompilerException` whose `.getCause` is that
    // condition -- the exact two-level shape real Clojure surfaces for
    // every such failure. Checked before the `Arity` arm below because the
    // one site that reaches it (`eval_quote`, see its own comment) is an
    // arity error whose `arity_cause` is the inner `ExceptionInfo`: without
    // this, the CAUSE's class would be catch-bound as if it were the
    // top-level exception.
    if e.jvm_class == Some(crate::error::JvmClass::CompilerException) {
        if let Some(cause) = e.arity_cause.as_deref() {
            return crate::hostclass::mk_compiler_exception(e.message.clone(), cause.clone());
        }
    }
    if e.kind == ErrorKind::Arity {
        if let Some(actual) = e.arity_actual {
            let cause = e.arity_cause.as_deref().cloned().unwrap_or(Value::Nil);
            return crate::hostclass::mk_arity_exception(actual, e.message.clone(), cause);
        }
    }
    // M8 slice 1 / defect D13: an error whose SITE measured which JVM class
    // real Clojure raises for exactly this condition (`error::JvmClass`,
    // already consulted by `error_kind_class_chain` for typed-`catch`
    // MATCHING) now also catch-BINDS a real host exception instance of that
    // class, so `(class e)` / `(.getName (class e))` inside the catch
    // answers `java.lang.NullPointerException` rather than
    // `clojure.lang.PersistentArrayMap`. Built from the very
    // `JvmClass::chain()` the matcher reads, so the class a `catch` clause
    // matched and the class the bound value reports cannot disagree.
    //
    // Deliberately scoped to `jvm_class.is_some()`: an error that was never
    // measured (the overwhelming majority -- every `ErrorKind::TypeErr`
    // without a tag, `DivideByZero`, `Sys`, `Reader`, ...) keeps the legacy
    // `{:type :error/<kind> :message ..}` info map below, because inventing
    // a leaf class for it would be a guess, and because that map is what
    // `hostclass::is_error_info_map` / `types_forms`'s `.getMessage` arm /
    // `instance?`'s `ERROR_INFO_CHAIN` still recognize. Widening past the
    // tagged set is the rest of milestone M8, one measured site at a time.
    if e.jvm_class.is_some() {
        return crate::errinfo::exception_value(e);
    }
    // nREPL gaps: every ordinary internal error also catch-binds a host
    // exception instance of its JVM class (`ArithmeticException`, ...);
    // compile-time ones are `Compiler$CompilerException` with a cause. Only
    // control signals / thrown values keep the legacy map below.
    if matches!(
        e.kind,
        ErrorKind::DivideByZero
            | ErrorKind::Arithmetic
            | ErrorKind::Arity
            | ErrorKind::TypeErr
            | ErrorKind::Unresolved
            | ErrorKind::Reader
            | ErrorKind::Sys
            | ErrorKind::Other
    ) {
        return crate::errinfo::exception_value(e);
    }
    let kind_kebab = match e.kind {
        ErrorKind::Reader => "reader",
        ErrorKind::Arity => "arity",
        ErrorKind::TypeErr => "type",
        ErrorKind::Unresolved => "unresolved",
        ErrorKind::DivideByZero => "divide-by-zero",
        ErrorKind::Arithmetic => "arithmetic",
        ErrorKind::Sys => "sys",
        ErrorKind::Thrown => "thrown",
        ErrorKind::Recur => "recur",
        // Both callers (`eval_try` above and `compile::exec::exec_try`)
        // already exclude `FuelExhausted` from ever reaching this function
        // -- it is not catchable, so it never becomes a `catch`-bound info
        // map. This arm exists only so the match stays exhaustive; the
        // kebab string is a defensive fallback, not a load-bearing one.
        ErrorKind::FuelExhausted => "fuel-exhausted",
        ErrorKind::Interrupted | ErrorKind::InterruptedHard => "interrupted",
        ErrorKind::Other => "other",
    };
    let mut m = PMap::new();
    m.insert(
        Value::Keyword("type".into()),
        Value::Keyword(format!("error/{kind_kebab}").into()),
    );
    m.insert(Value::Keyword("message".into()), Value::Str(e.message.clone().into()));
    Value::Map(m)
}
