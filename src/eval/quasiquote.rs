//! `quasiquote`/`unquote`/`unquote-splicing` expansion. Handles lists,
//! vectors, sets and maps; `~@` splices in all four (C11 added the map
//! case, which expands the literal as one flat `k0 v0 k1 v1 ...`
//! sequence, exactly as the JVM reader's `(apply hash-map (seq (concat
//! ...)))` does).
//!
//! ## The oracle-measured corpus
//!
//! `compat/qq-probe.clj` + `compat/qq-oracle-transcript.txt` are ~100 rows
//! of `~@`/`~`/nesting behaviour measured on real Clojure 1.13.0-alpha6.
//! Every row matches as of C11 except the four `nested-qq` rows (the
//! depth-tracking deviation documented just below).
//!
//! Deviation from real Clojure (documented): no nested-depth tracking, so
//! `unquote`/`unquote-splicing` fire at *any* nesting depth rather than
//! only at the innermost backtick. Fine for v0's single-level macro use.
//!
//! ## Auto-gensym (A3)
//!
//! A bare symbol ending in `#` (e.g. `x#`) appearing anywhere in a
//! quasiquoted form -- other than inside an `~unquote`/`~@unquote-splicing`
//! (those are ordinary evaluated code, not quoted data) -- resolves to a
//! freshly generated symbol `<base>__<n>__auto__`. Every occurrence of the
//! *same* `x#` within ONE top-level `` ` `` expansion resolves to the same
//! generated symbol (hygiene within a single syntax-quote); a separate
//! `eval_quasiquote_top` call (a different backtick, e.g. in another
//! `defmacro` invocation or another top-level quasiquote) gets a fresh
//! mapping, so `x#` in one and `x#` in another never collide. The mapping
//! is threaded through the recursive expansion as a plain local `HashMap`
//! (not `Interp` state) so nothing here needs `src/eval/mod.rs` changes.
//!
//! ## Namespace qualification (W3e-1)
//!
//! Real Clojure resolves every non-gensym, non-special template symbol
//! against the *reading* namespace, so `` `map `` reads as
//! `clojure.core/map` and a macro's expansion keeps working no matter what
//! `*ns*` is when it is finally evaluated. mova used to leave every symbol
//! bare, which made macro output resolve against the CALLER's namespace --
//! visible as `clojure.test-clojure.repl/test-dynamic-ns` blowing up with
//! "Unable to resolve symbol: ts-testing-ctx" the moment a test body called
//! a macro that runs `(ns a#)` mid-file.
//!
//! mova does the same resolution at EXPANSION time rather than read time,
//! which lands on the same answer: `Closure::ns` + `apply_closure`'s
//! `current_ns` swap mean a macro body always expands with `current_ns`
//! set to the namespace the backtick was WRITTEN in, and a top-level
//! backtick expands under whatever `ns`/`in-ns` most recently ran (mova's
//! `eval_forms` evaluates form by form, so a file's own leading `(ns ...)`
//! has already taken effect). A read-time implementation is not available:
//! `reader::read_all` is a pure `&str -> Vec<Form>` with no `Interp`, and
//! `Interp::eval_str` reads the WHOLE file before evaluating any of it.
//!
//! The rule set is ported branch-for-branch from the JVM's
//! `LispReader/syntaxQuote` + `Compiler/resolveSymbol` -- see
//! `is_syntax_quote_special` and `Interp::syntax_quote_resolve` for the
//! per-branch oracle measurements (`compat/sq-qualify-probe.clj`,
//! `compat/sq-qualify-oracle-transcript.txt`).

// `Str`'s cache fields (ASCII/char-count, see value.rs) are `Atomic*`,
// which trips clippy's `mutable_key_type` lint on the `HashMap<Str, _>`
// gensym maps threaded through this file -- but `Str`'s `Hash`/`Eq` are
// derived purely from its immutable text, never from the cache, so the
// lint's premise (mutation could invalidate a key's hash bucket) doesn't
// apply here.
#![allow(clippy::mutable_key_type)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use super::Interp;
use crate::env::Env;
use crate::error::RjError;
use crate::reader::{Form, FormValue, Span};
use crate::value::{PMap, PVec, Str, Symbol, Value};

enum UnquoteKind<'a> {
    Plain(&'a Form),
    Splice(&'a Form),
}

fn unquote_kind(form: &Form) -> Option<UnquoteKind<'_>> {
    if let FormValue::List(items) = &form.value {
        if items.len() == 2 {
            if let FormValue::Atom(Value::Sym(sym)) = &items[0].value {
                if sym.ns.is_none() {
                    match sym.name.as_ref() {
                        "unquote" => return Some(UnquoteKind::Plain(&items[1])),
                        "unquote-splicing" => return Some(UnquoteKind::Splice(&items[1])),
                        _ => {}
                    }
                }
            }
        }
    }
    None
}

/// Own counter for auto-gensym symbols (`x#` -> `x__<n>__auto__`),
/// independent of the explicit `(gensym)` native's counter in
/// `builtins::strings` (that module is private to `builtins`, so this file
/// can't share it without touching `builtins/mod.rs`, which is out of
/// scope here -- see PLAN.md's A3 file-ownership split). Two independent
/// monotonic counters are still each individually unique, which is all
/// hygiene requires.
pub(crate) static AUTOGENSYM_COUNTER: AtomicU64 = AtomicU64::new(1);

fn next_autogensym_id() -> u64 {
    AUTOGENSYM_COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// Resolves one quoted symbol against the current quasiquote's auto-gensym
/// map: non-`#`-suffixed (or namespaced) symbols pass through unchanged;
/// `x#` reuses a previously generated symbol for this expansion or mints
/// one and remembers it.
fn resolve_auto_gensym(sym: &Symbol, gensyms: &mut HashMap<Str, Symbol>) -> Option<Symbol> {
    if sym.ns.is_none() {
        let name = sym.name.as_ref();
        if name.len() > 1 && name.ends_with('#') {
            if let Some(existing) = gensyms.get(&sym.name) {
                return Some(existing.clone());
            }
            let base = &name[..name.len() - 1];
            let fresh = Symbol::simple(format!("{base}__{}__auto__", next_autogensym_id()));
            gensyms.insert(sym.name.clone(), fresh.clone());
            return Some(fresh);
        }
    }
    None
}

/// W3e-1: the names real Clojure's `LispReader/syntaxQuote` leaves BARE --
/// its very first branch is `if(Compiler.isSpecial(form)) ret = RT.list(
/// Compiler.QUOTE, form)`, i.e. a special-form name is quoted verbatim and
/// never sent through `Compiler.resolveSymbol`. Measured on real Clojure
/// 1.13.0-alpha6 in namespace `probe` (see `compat/sq-qualify-probe.clj` /
/// `compat/sq-qualify-oracle-transcript.txt`): `` `if `` => `if`,
/// `` `let* `` => `let*`, `` `& `` => `&`, `` `. `` => `.`, while
/// `` `let `` => `clojure.core/let` and `` `import* `` => `probe/import*`
/// (`import*` is in the JVM's `specials` map only under its QUALIFIED
/// spelling `clojure.core/import*`, so the bare symbol is not special).
///
/// This is deliberately Clojure's list, NOT mova's `eval_special` list.
/// mova implements as structural special forms several things real Clojure
/// implements as `clojure.core` MACROS (`let`, `fn`, `loop`, `binding`,
/// `defmacro`, `ns`, `defrecord`, ...), and those must qualify to
/// `clojure.core/<name>` exactly like the oracle does -- which stays
/// correct because `Interp::is_bare_or_core_alias` already makes a
/// `clojure.core/`-qualified head dispatch through `eval_special` (see its
/// doc comment: that gate exists precisely so `core/let` behaves like
/// `let`).
/// SPEC-PORT: `%`, `%1`..`%N` and `%&` -- the parameters mova's `#()`
/// reader introduces. Real Clojure's reader rewrites them to GENSYMS
/// (`p1__42#`) before syntax-quote ever sees the form, so they are plain
/// locals there and are never namespace-qualified; mova's reader keeps
/// the literal `%N` names (which is what makes a `#()` form printable,
/// and what `clojure.spec.alpha`'s `unfn` keys off), so syntax-quote has
/// to be told not to qualify them. Without this, every `` `#(..) `` in a
/// macro BODY expanded into code referring to `<macro-ns>/%1` -- an
/// unresolved symbol the moment that code ran (`clojure.spec.alpha`'s
/// `int-in`, `double-in`, `inst-in`, `every`, `fspec`, ... all use the
/// shape).
fn is_fn_shorthand_param(name: &str) -> bool {
    match name.strip_prefix('%') {
        None => false,
        Some(rest) => rest.is_empty() || rest == "&" || rest.bytes().all(|b| b.is_ascii_digit()),
    }
}

fn is_syntax_quote_special(sym: &Symbol) -> bool {
    if sym.ns.is_some() {
        return false;
    }
    if is_fn_shorthand_param(sym.name.as_ref()) {
        return true;
    }
    matches!(
        sym.name.as_ref(),
        "def"
            | "loop*"
            | "recur"
            | "if"
            | "case*"
            | "let*"
            | "letfn*"
            | "do"
            | "fn*"
            | "quote"
            | "var"
            | "."
            | "set!"
            | "deftype*"
            | "reify*"
            | "try"
            | "throw"
            | "monitor-enter"
            | "monitor-exit"
            | "catch"
            | "finally"
            | "new"
            | "&"
    )
}

impl Interp {
    /// W3e-1: `Compiler.resolveSymbol`'s answer for `sym` -- "which symbol
    /// does this bare/aliased name MEAN in the current namespace", as a
    /// SYMBOL (not a value). Ported branch-for-branch from
    /// `.oracle/clojure-src/src/jvm/clojure/lang/Compiler.java`'s
    /// `resolveSymbol`, with mova's own tables standing in for the JVM's
    /// `Namespace.getMapping`:
    ///
    /// * a name with an interior `.` (`java.lang.String`, `foo.bar`) is
    ///   already a class/fully-spelled name -- unchanged (oracle:
    ///   `` `java.lang.String `` => `java.lang.String`, `` `foo.bar `` =>
    ///   `foo.bar`);
    /// * a QUALIFIED symbol has its namespace run through the `:as` /
    ///   imported-class-short-name table (oracle: `` `str/join `` =>
    ///   `clojure.string/join`, `` `String/valueOf `` =>
    ///   `java.lang.String/valueOf`), and is left alone when that table
    ///   knows nothing about it (oracle: `` `nonexistent/foo `` unchanged).
    ///   DESIGN-flow-namespace.md Part 1 point 3 nuance (mova-only, no
    ///   oracle -- upstream has no such table): `expand_alias` ALSO
    ///   consults the engine-owned default-alias table (`ns::
    ///   DEFAULT_ALIASES`, e.g. `flow -> clojure.core.async.flow`) as its
    ///   last resort, so `` `flow/create-flow `` expands to `` `clojure.
    ///   core.async.flow/create-flow ``, not "unchanged", exactly like any
    ///   OTHER known alias -- the "knows nothing about it" case above is
    ///   now specifically "neither the current ns's `:as` table, nor a
    ///   literal LOADED namespace of that name, nor `DEFAULT_ALIASES`".
    ///   ns.rs's item-5 fix (this fix's FIX 3) is what makes that
    ///   precedence deterministic: only a literal LOADED namespace (not a
    ///   mere transient `reg.namespaces` registry entry, e.g. from a
    ///   passing `(in-ns 'flow)`) can shadow the default table here;
    /// * a BARE symbol resolves through mova's ordinary global candidate
    ///   order (`ns::for_each_global_candidate` -- NOT `resolve_symbol`:
    ///   Clojure asks `currentNS().getMapping`, which never sees locals,
    ///   and neither may this). A hit on a class value with a dotted name
    ///   becomes that bare class name (oracle: `` `String `` =>
    ///   `java.lang.String`); any other hit becomes the defining var's own
    ///   `ns/name` (oracle: `` `map `` => `clojure.core/map`), where a
    ///   BARE-interned cell means `clojure.core` -- that is what bare
    ///   interning MEANS in mova, see `ns::CORE_NS`;
    /// * a miss becomes `current-ns/name` (oracle: `` `unmapped-thing ``
    ///   => `probe/unmapped-thing`).
    fn syntax_quote_resolve(&self, sym: &Symbol) -> Symbol {
        // `name.indexOf('.') > 0` in Compiler.resolveSymbol -- note ">0",
        // so a LEADING dot (`.foo`, a method name) is not covered here; it
        // never reaches this fn (handled by its caller, matching the
        // reader's own earlier `startsWith(".")` branch).
        if sym.name.chars().skip(1).any(|c| c == '.') {
            return sym.clone();
        }
        if let Some(q) = &sym.ns {
            let full = self.expand_alias(q);
            if full != *q {
                return Symbol {
                    ns: Some(full),
                    name: sym.name.clone(),
                };
            }
            // `expand_alias` only consults the current ns's `:as` table and
            // its ns-qualified imported-class cells; mova's DEFAULT classes
            // (`String`, `Long`, ...) are interned BARE instead, so probe
            // those too before giving up -- otherwise `` `String/valueOf ``
            // would stay unqualified where the oracle says
            // `java.lang.String/valueOf`.
            if let Some(Value::Class(c)) = self.globals.get_exact(&Symbol::simple(q.clone())) {
                let cname = c.name();
                if cname.contains('.') {
                    return Symbol {
                        ns: Some(Str::from(cname)),
                        name: sym.name.clone(),
                    };
                }
            }
            return sym.clone();
        }
        match self.for_each_global_candidate(sym, |cand| self.globals.find_bound_cell(cand)) {
            Some(cell) => {
                if let Some(Value::Class(c)) = cell.get() {
                    let cname = c.name();
                    // Only a DOTTED class name is a globally-resolvable
                    // spelling in mova (`java.lang.String`,
                    // `clojure.lang.PersistentQueue`). A `deftype`/
                    // `defrecord` class interned under a short name is
                    // reachable only through its var cell, so fall through
                    // to the cell-name branch for it rather than emitting a
                    // bare name that resolves nowhere outside its own ns.
                    if cname.contains('.') {
                        return Symbol::simple(Str::from(cname));
                    }
                }
                match &cell.name.ns {
                    Some(_) => cell.name.clone(),
                    None => Symbol {
                        ns: Some(Str::from(crate::ns::CORE_NS)),
                        name: cell.name.name.clone(),
                    },
                }
            }
            // No var cell anywhere -- but mova's STRUCTURAL special forms
            // (`let`, `fn`, `ns`, `binding`, `defrecord`, ...) have no cell
            // by construction, while real Clojure has an ordinary
            // `clojure.core` macro var for each of them that its own
            // `getMapping` finds. `super::special_forms::
            // SPECIAL_FORM_NAMES` stands in for that mapping, so
            // `` `(let [x# 1] x#) `` reads as `(clojure.core/let ...)`
            // exactly like the oracle -- and NOT as `probe/let`, which
            // would resolve nowhere and break every macro that
            // syntax-quotes a binding form.
            None if super::special_forms::is_special_form_name(&sym.name) => Symbol {
                ns: Some(Str::from(crate::ns::CORE_NS)),
                name: sym.name.clone(),
            },
            None => Symbol {
                ns: Some(self.current_ns.clone()),
                name: sym.name.clone(),
            },
        }
    }

    /// W3e-1: one template symbol's syntax-quote reading, in the exact
    /// branch order of the JVM reader's `syntaxQuote` (see
    /// `is_syntax_quote_special` for the measured corpus this was written
    /// against).
    fn syntax_quote_symbol(&mut self, sym: &Symbol, gensyms: &mut HashMap<Str, Symbol>) -> Symbol {
        // Both of these are pure and cheap, so they stay OUTSIDE the cache:
        // a special form never varies, and an auto-gensym must mint a fresh
        // name per expansion, which is the one thing a cache must not do.
        if is_syntax_quote_special(sym) {
            return sym.clone();
        }
        if let Some(g) = resolve_auto_gensym(sym, gensyms) {
            return g;
        }
        // W3e2: everything below reads the global mapping tables, so it is
        // memoised per (reading ns, symbol) until the mapping world moves.
        // See `Interp::sq_cache`'s doc for the measurement that made this
        // necessary.
        let gen = crate::env::global_generation();
        if self.sq_cache_gen != gen {
            self.sq_cache.clear();
            self.sq_cache_gen = gen;
        } else if let Some(hit) = self.sq_cache.get(&(self.current_ns.clone(), sym.clone())) {
            return hit.clone();
        }
        let resolved = self.syntax_quote_resolve_uncached(sym);
        self.sq_cache
            .insert((self.current_ns.clone(), sym.clone()), resolved.clone());
        resolved
    }

    /// The uncached body of [`Self::syntax_quote_symbol`]'s mapping half --
    /// the `Ctor.`/`.method` shapes and then `syntax_quote_resolve`.
    fn syntax_quote_resolve_uncached(&self, sym: &Symbol) -> Symbol {
        if sym.ns.is_none() {
            let name = sym.name.as_ref();
            // `Ctor.` -- resolve the class part, then re-append the dot as
            // a BARE symbol (the JVM keeps only `csym.name`, dropping any
            // namespace `resolveSymbol` may have added). Oracle:
            // `` `String. `` => `java.lang.String.`,
            // `` `java.math.MathContext. `` => `java.math.MathContext.`.
            if name.len() > 1 && name.ends_with('.') {
                let base = Symbol::simple(Str::from(&name[..name.len() - 1]));
                let resolved = self.syntax_quote_resolve(&base);
                return Symbol::simple(format!("{}.", resolved.name));
            }
            // `.method` -- quoted verbatim (oracle: `` `.foo `` => `.foo`).
            if name.starts_with('.') {
                return sym.clone();
            }
        }
        self.syntax_quote_resolve(sym)
    }

    pub(super) fn eval_quasiquote_top(&mut self, args: &[Form], span: Span, env: &Env) -> Result<Value, RjError> {
        if args.len() != 1 {
            return Err(RjError::arity(format!("quasiquote: expected 1 argument, got {}", args.len()))
                .with_span(span)
                .with_stack(self.stack_snapshot(), self.source_id));
        }
        // Fresh per top-level `` ` `` expansion -- see this module's doc
        // comment on auto-gensym hygiene.
        let mut gensyms: HashMap<Str, Symbol> = HashMap::new();
        self.eval_quasiquote(&args[0], env, &mut gensyms)
    }

    fn eval_quasiquote(&mut self, form: &Form, env: &Env, gensyms: &mut HashMap<Str, Symbol>) -> Result<Value, RjError> {
        match unquote_kind(form) {
            Some(UnquoteKind::Plain(inner)) => return self.eval_form_in(inner, env),
            Some(UnquoteKind::Splice(_)) => {
                return Err(RjError::other("unquote-splicing (~@) used outside of a sequence")
                    .with_span(form.span)
                    .with_stack(self.stack_snapshot(), self.source_id))
            }
            None => {}
        }
        match &form.value {
            // C11: real Clojure compiles a syntax-quoted LIST to `(seq
            // (concat ...))`, so a non-empty template whose contents all
            // splice away to nothing is `nil`, not `()` -- measured:
            // `` `(~@[]) `` => `nil`, while the empty literal `` `() ``
            // (the reader's own special case) stays `()`, and `` `[~@[]] ``
            // / `` `#{~@[]} `` stay `[]`/`#{}`. This arm is the only one
            // that collapses, exactly matching that measurement (see
            // `compat/qq-probe.clj` rows 010-015 / 093).
            FormValue::List(items) => {
                let out = self.qq_expand_seq(items, env, gensyms)?;
                if out.is_empty() && !items.is_empty() {
                    Ok(Value::Nil)
                } else {
                    Ok(Value::List(out))
                }
            }
            FormValue::Vector(items) => Ok(Value::Vector(self.qq_expand_seq(items, env, gensyms)?)),
            FormValue::Set(items) => Ok(Value::Set(self.qq_expand_seq(items, env, gensyms)?.into_iter().collect())),
            // C11: a map template expands as one FLAT `k0 v0 k1 v1 ...`
            // sequence, which is what makes `~@` work inside it --
            // `` `{~@[:a 1] ~@[:b 2]} `` is `{:a 1, :b 2}` on the oracle,
            // where mova previously raised "unquote-splicing (~@) used
            // outside of a sequence". (Real Clojure emits `(apply hash-map
            // (seq (concat ...)))` for exactly this reason.) Note the
            // reader still requires an EVEN number of source forms, and
            // `~@x` counts as one, so the odd-arity error below is only
            // reachable when a splice contributes an odd element count at
            // RUNTIME; its message/`display_str` formatting matches the
            // `array-map` precedent in `builtins::collections` (measured:
            // `IllegalArgumentException: No value supplied for key: :b`).
            FormValue::Map(pairs) => {
                let flat = self.qq_expand_forms(pairs.iter().flat_map(|(k, v)| [k, v]), env, gensyms)?;
                if flat.len() % 2 != 0 {
                    let last = flat.last().expect("odd length is never 0, so there is a last element");
                    return Err(RjError::other(format!(
                        "No value supplied for key: {}",
                        crate::printer::display_str(last)
                    ))
                    // W3a: measured class, already named in the comment
                    // above -- `java.lang.IllegalArgumentException`.
                    .with_class(crate::error::JvmClass::IllegalArgument)
                    .with_span(form.span)
                    .with_stack(self.stack_snapshot(), self.source_id));
                }
                let mut m = PMap::new();
                let mut it = flat.into_iter();
                while let Some(k) = it.next() {
                    let v = it.next().expect("even length checked above");
                    m.insert(k, v);
                }
                Ok(Value::Map(m))
            }
            FormValue::Atom(Value::Sym(sym)) => Ok(Value::Sym(self.syntax_quote_symbol(sym, gensyms))),
            FormValue::Atom(_) => Ok(crate::reader::form_to_value(form)),
        }
    }

    fn qq_expand_seq(&mut self, items: &[Form], env: &Env, gensyms: &mut HashMap<Str, Symbol>) -> Result<PVec, RjError> {
        self.qq_expand_forms(items.iter(), env, gensyms)
    }

    /// The one sequence-position expander: every `~@` splices its value's
    /// ELEMENTS in, every `~` contributes exactly one element, everything
    /// else is quoted data. Takes an ITERATOR of forms (rather than a
    /// slice) so the map-template arm above can feed it a flattened
    /// `k0 v0 k1 v1 ...` view of its `(key, value)` pairs without cloning
    /// any `Form`.
    fn qq_expand_forms<'a>(
        &mut self,
        items: impl IntoIterator<Item = &'a Form>,
        env: &Env,
        gensyms: &mut HashMap<Str, Symbol>,
    ) -> Result<PVec, RjError> {
        let mut out = PVec::new();
        for item in items {
            match unquote_kind(item) {
                // C11: `~@` splices the ELEMENTS of `v`, so it needs a
                // fully-realized element list -- `Interp::seq_items` is
                // the shared choke point that produces one, and as of
                // C11 it applies `builtins::lazy_tail_split` (the single
                // definition of mova's improper-list rule, shared with
                // `uncons`/`materialize`) instead of handing back the raw
                // `[head, <lazy rest>]` slots of a lazy seq. That raw
                // shape was the measured double-wrap: `` `(do ~@(map f
                // xs)) `` expanded to `(do (f x0) ((f x1) (f x2)))`,
                // silently collapsing 7 `do-template` groups into 2 (see
                // `compat/qq-probe.clj` / `compat/qq-oracle-transcript.txt`).
                Some(UnquoteKind::Splice(inner)) => {
                    let v = self.eval_form_in(inner, env)?;
                    if let Some(seq) = self.seq_items(&v)? {
                        out.extend(seq);
                    }
                }
                Some(UnquoteKind::Plain(inner)) => out.push_back(self.eval_form_in(inner, env)?),
                None => out.push_back(self.eval_quasiquote(item, env, gensyms)?),
            }
        }
        Ok(out)
    }
}
