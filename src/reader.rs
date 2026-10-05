//! Hand-written lexer + parser: source text -> `Vec<Form>` with byte-offset
//! spans. Implements the full surface syntax from ARCHITECTURE.md, including
//! reader macros (`'` `` ` `` `~` `~@` `@` `#(...)` `#_` `;`).

use std::collections::HashMap;

use crate::bignum::Reduced;
use crate::error::{JvmClass, RjError};
use crate::value::{PMap, Str, Symbol, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

#[derive(Clone, Debug)]
pub struct Form {
    pub value: FormValue,
    pub span: Span,
    /// S5 / M3: the `^...` metadata written in front of this form, as the
    /// (already-desugared, see `Reader::read_meta`) map form it stands
    /// for -- `None` for the overwhelming majority of forms.
    ///
    /// # Why a FIELD and not a `FormValue::Meta` variant
    ///
    /// A wrapper variant was the obvious shape (it's what `Value::Meta`
    /// does on the runtime side, where the compiler's exhaustiveness
    /// checking makes the sweep safe). It is the WRONG shape here,
    /// because `FormValue` is matched in ~96 places, and almost all of
    /// them are `if let`/`match` arms with a fallthrough rather than
    /// exhaustive matches -- so a wrapper would compile clean and then
    /// silently stop matching. The positions that would break are not
    /// exotic: `(fn [^long x] ...)` and `(defn f [^String s] ...)` put
    /// metadata on a PARAMETER symbol, `(let [^Foo x ...] ...)` on a
    /// destructuring pattern, and every one of those code paths looks
    /// for a bare `FormValue::Atom(Value::Sym(_))`. Type hints are the
    /// single most common metadata in real Clojure source (18 `^String`,
    /// 14 `^long`, 12 `^Object` in this repo's own test corpora alone),
    /// so "type hints stop binding parameters" would have been the
    /// headline regression.
    ///
    /// As a field, metadata is transparent by construction: a hinted
    /// parameter is still exactly `FormValue::Atom(Value::Sym(_))` to
    /// all ~96 of those sites, carrying one extra `None`-in-practice
    /// pointer they never look at. The two places that DO care read it
    /// explicitly -- `eval`/`compile::resolve` (attach it to the
    /// evaluated value) and `def`/`defn` (flow it into the var's meta).
    pub meta: Option<Box<Form>>,
}

impl Form {
    /// A form with no `^` metadata -- the shape every reader/expander
    /// construction site wants. See [`Form::meta`] for why the field
    /// exists at all.
    pub fn bare(value: FormValue, span: Span) -> Form {
        Form { value, span, meta: None }
    }
}

#[derive(Clone, Debug)]
pub enum FormValue {
    Atom(Value), // literals incl. symbols/keywords
    List(Vec<Form>),
    Vector(Vec<Form>),
    Map(Vec<(Form, Form)>),
    Set(Vec<Form>),
}

/// `Form -> Value`, dropping spans (used by `quote`).
///
/// S5/M3: `^` metadata rides along UNEVALUATED, which is what makes
/// `(meta (quote ^:a x))` -> `{:a true}` and `(meta (read-string
/// "^String x"))` -> `{:tag String}` (a bare symbol, measured -- nothing
/// resolves `String` on this path). The EVALUATING counterpart is
/// `Interp::eval_form`'s own metadata handling.
pub fn form_to_value(form: &Form) -> Value {
    let bare = form_to_value_bare(form);
    match &form.meta {
        Some(m) => Value::attach_meta(bare, form_to_value(m)),
        None => bare,
    }
}

/// [`form_to_value`] without the metadata attachment -- the recursive
/// worker, split out so the metadata form itself can be converted
/// without re-entering the attach step for it.
/// One key of a `#:ns{...}` namespaced map literal, qualified against
/// `ns` -- `LispReader.namespaceMap`'s rule, verbatim (SPEC-W6b):
///
/// * an UNQUALIFIED keyword or symbol gains `ns`;
/// * one whose namespace is the literal `_` LOSES its namespace, which is
///   upstream's documented per-key opt-out (`#:a{:_/b 1}` -> `{:b 1}`);
/// * an already-qualified key, and any key that is neither a keyword nor
///   a symbol (a string, a number, a nested collection), is returned
///   completely unchanged.
///
/// Only KEYS go through here. Values are never rewritten -- a nested
/// `#:other{...}` value is its own literal and was qualified by its own
/// read.
fn qualify_ns_map_key(key: Form, ns: &Str) -> Form {
    let Form { meta, value, span } = key;
    let value = match value {
        FormValue::Atom(Value::Keyword(k)) => {
            let text = k.text_ref().as_ref().to_string();
            match text.split_once('/') {
                None => FormValue::Atom(Value::Keyword(crate::keyword::Keyword::from_owned(
                    Str::from(format!("{ns}/{text}")),
                ))),
                Some(("_", name)) => FormValue::Atom(Value::Keyword(
                    crate::keyword::Keyword::from_owned(Str::from(name.to_string())),
                )),
                Some(_) => FormValue::Atom(Value::Keyword(k)),
            }
        }
        FormValue::Atom(Value::Sym(s)) => match s.ns.as_deref() {
            None => FormValue::Atom(Value::Sym(Symbol {
                ns: Some(ns.clone()),
                name: s.name,
            })),
            Some("_") => FormValue::Atom(Value::Sym(Symbol { ns: None, name: s.name })),
            Some(_) => FormValue::Atom(Value::Sym(s)),
        },
        other => other,
    };
    Form { meta, value, span }
}

fn form_to_value_bare(form: &Form) -> Value {
    match &form.value {
        FormValue::Atom(v) => v.clone(),
        FormValue::List(items) => Value::List(items.iter().map(form_to_value).collect()),
        FormValue::Vector(items) => Value::Vector(items.iter().map(form_to_value).collect()),
        FormValue::Map(pairs) => {
            let mut m = PMap::new();
            for (k, v) in pairs {
                m.insert(form_to_value(k), form_to_value(v));
            }
            Value::Map(m)
        }
        FormValue::Set(items) => {
            let mut t = champ::PersistentHashSet::new().transient();
            for it in items {
                t.insert(form_to_value(it));
            }
            Value::Set(t.persistent())
        }
    }
}

/// W4C-NS (`protocols.clj`'s `exercise-literals`, "that ctor literals only
/// work with constants or statics"): whether a `#Class[...]` ctor-literal
/// ARGUMENT is a legitimate constant/static, matching what the real JVM
/// reader's `LispReader.readRecord` would successfully hand to
/// `Reflector.invokeConstructor`.
///
/// Real Clojure's ctor-literal reader constructs the object AT READ TIME
/// directly from the raw, UNEVALUATED forms the reader already produced
/// (`RT.toArray(recordEntries)`, straight off the just-read
/// `IPersistentVector`) -- it never compiles or evaluates anything. A
/// literal like `42`/`"en"`/`:kw` reads as the real value a constructor
/// could take; `()` reads as `PersistentList.EMPTY`, itself a piece of
/// DATA, not a function call. But `(str 'en)` reads as a `PersistentList`
/// of the symbols `str`/`quote`/`en` -- no real constructor accepts a
/// list, so `Reflector.invokeConstructor` fails to find a matching ctor
/// and throws. mova's ctor-literal desugars into ordinary CODE evaluated
/// normally later (`read_tagged_literal`'s own doc explains why: no live
/// class registry exists at read time to construct against), so `(str
/// 'en)` would otherwise EVALUATE to a legitimate 1-arg string and
/// silently construct where the real JVM throws. This check reproduces
/// that read-time rejection by SHAPE rather than reflection: a bare
/// literal atom, an empty list (data, not a call), or a NESTED ctor
/// literal (this reader's own `#Class[...]`/`#Class{...}` desugaring,
/// recognizable by its synthesized head symbol -- `read_ctor_vector_
/// literal`'s trailing-`.` name or `read_ctor_map_literal`'s `.../create`)
/// all count as constants; anything else (a bare, unquoted symbol -- a
/// real static-field reference mova can't resolve at read time either --
/// or any other call-shaped list) does not. Measured against the oracle:
/// `compat/w4c-locale-oracle-transcript.txt`.
fn is_ctor_literal_constant(form: &Form) -> bool {
    match &form.value {
        // A bare symbol reads as a real static-field REFERENCE on the
        // JVM (`Foo/BAR`), which mova has no read-time way to resolve --
        // treated as non-constant rather than silently wrong. Every other
        // atom (`Value::Sym` aside) is genuine self-evaluating data.
        FormValue::Atom(v) => !matches!(v, Value::Sym(_)),
        FormValue::List(items) => {
            items.is_empty()
                || matches!(
                    items.first().map(|h| &h.value),
                    Some(FormValue::Atom(Value::Sym(s)))
                        if (s.ns.is_none() && s.name.ends_with('.')) || s.name == "create"
                )
        }
        // Vector/map/set literals are themselves just data once read
        // (their own elements aren't independently validated here --
        // no vendored ctor-literal argument needs that today).
        FormValue::Vector(_) | FormValue::Map(_) | FormValue::Set(_) => true,
    }
}

/// `Value -> Form`, synthesizing every node with `span` (typically the
/// macro-expansion call site, so errors inside expanded code point there).
pub fn value_to_form(value: &Value, span: Span) -> Form {
    // S5/M3: a `Value::Meta` round-trips back into a metadata-carrying
    // `Form`, so a macro that returns a `with-meta`'d value doesn't
    // silently lose it on the way back into the evaluator. The metadata
    // map becomes a literal `Form` whose own re-evaluation is a no-op
    // (it's already a value), which is exactly what the `Const`/`Atom`
    // arms below give it.
    if let Value::Meta(m) = value {
        let mut inner = value_to_form(&m.inner, span);
        inner.meta = Some(Box::new(value_to_form(&m.meta, span)));
        return inner;
    }
    let fv = match value {
        Value::List(items) => {
            FormValue::List(items.iter().map(|v| value_to_form(v, span)).collect())
        }
        Value::Vector(items) => {
            FormValue::Vector(items.iter().map(|v| value_to_form(v, span)).collect())
        }
        Value::Map(m) => FormValue::Map(
            m.iter()
                .map(|(k, v)| (value_to_form(k, span), value_to_form(v, span)))
                .collect(),
        ),
        Value::Set(items) => {
            FormValue::Set(items.iter().map(|v| value_to_form(v, span)).collect())
        }
        other => FormValue::Atom(other.clone()),
    };
    Form { value: fv, span, meta: None }
}

/// Read-time namespace context `::kw`/`::alias/kw` (auto-resolved
/// keywords, C3d) resolve against -- real Clojure resolves these at READ
/// time, not eval time, against whatever `*ns*` is bound to right then
/// (measured: `(read-string "::foo")` uses `*ns*` as of the `read-string`
/// call; `(binding [*ns* ...] (read-string "::bar"))` picks up the
/// rebinding). A plain owned snapshot -- current namespace name plus its
/// `:as`/`:as-alias` table (both land in the same table, `ns.rs::
/// add_alias`) -- rather than a trait object or a borrow into
/// `eval::Interp`/`ns.rs`'s `NsRegistry`: this file has no dependency on
/// `eval`/`ns` today and this keeps it that way. `Interp::
/// reader_ns_context` (`ns.rs`) is the one place that builds a real one,
/// fresh before every top-level form read (`eval::Interp::eval_str`/
/// `eval_str_allow_read_cond`) so a `(require '[x :as y])` earlier in the
/// SAME file is visible to a later `::y/z` -- measured,
/// `tests/clojure-suite/vendor/special.clj`'s
/// `resolve-keyword-ns-alias-in-destructuring` (a separate top-level
/// `(require '[clojure.string :as s])` before the `deftest` that reads
/// `::s/x`). `NsContext::user()` is the default every OTHER call site
/// (this file's own tests, and any `read_all`/`read_one` caller that
/// doesn't have a real `Interp` at hand) gets.
#[derive(Clone, Debug)]
pub(crate) struct NsContext {
    pub(crate) current_ns: Str,
    pub(crate) aliases: HashMap<Str, Str>,
}

impl NsContext {
    /// Same default a fresh REPL's `*ns*` starts in: `user`, no aliases.
    pub(crate) fn user() -> Self {
        NsContext {
            current_ns: Str::from("user"),
            aliases: HashMap::new(),
        }
    }
}

impl Default for NsContext {
    fn default() -> Self {
        Self::user()
    }
}

/// Reads every top-level form in `src` against the default `NsContext::
/// user()` -- fine for a test that doesn't care about `::kw` resolution,
/// but production loading always needs the REAL current namespace/alias
/// table (C3d), so `eval::Interp::eval_str`/`eval_str_allow_read_cond`
/// build their own `Reader` (`Reader::new`/`new_allow_read_cond`) and
/// drive it form-by-form instead of calling this (see `eval_str`'s doc).
/// `#[cfg(test)]`: this file's own tests are its only remaining caller.
/// Reader conditionals (`#?`/`#?@`) are NOT allowed here -- see
/// `Reader::allow_read_cond`'s doc comment -- matching plain-`.clj`-style
/// loading.
#[cfg(test)]
pub fn read_all(src: &str) -> Result<Vec<Form>, RjError> {
    let mut r = Reader::new(src);
    let mut forms = Vec::new();
    while let Some(f) = r.next_form()? {
        forms.push(f);
    }
    Ok(forms)
}

/// Reads exactly the FIRST form in `src` against the default `NsContext::
/// user()` -- test-only (`#[cfg(test)]`, same reasoning as `read_all`);
/// `(read-string s)` itself goes through `read_one_with_ns` so it sees the
/// CALLER's live `*ns*` rather than this fixed default. `None` at true
/// EOF (an empty or all-trivia string). Reader conditionals are NOT
/// allowed -- matches `(read-string s)` with no `:read-cond` option
/// (measured: throws `Conditional read not allowed`); see
/// `read_one_allow_cond` for the `{:read-cond :allow}` sibling.
#[cfg(test)]
pub fn read_one(src: &str) -> Result<Option<Form>, RjError> {
    Reader::new(src).next_form()
}

/// `read_one`, but resolving `::kw`/`::alias/kw` against `ctx` (C3d)
/// instead of the `NsContext::user()` default -- what `(read-string s)`
/// actually needs, since real Clojure's `read-string` reads against the
/// CALLER's live `*ns*` (`Interp::reader_ns_context`), not a fixed
/// namespace.
pub(crate) fn read_one_with_ns(src: &str, ctx: NsContext) -> Result<Option<Form>, RjError> {
    read_one_with_ns_ctors(src, ctx, false).map(|(f, _, _)| f)
}

/// W3d2: one `(start offset, is the positional `[..]` spelling)` entry per
/// CONSTRUCTOR literal a read desugared -- see
/// `Reader::ctor_literal_starts`' doc for both halves.
pub(crate) type CtorLiteralStarts = Vec<(usize, bool)>;

/// W4-PRINTER: one `(span-start, tag)` entry per GENERIC tagged literal a
/// read passed through -- see `Reader::tag_literal_starts`' doc.
pub(crate) type TagLiteralStarts = Vec<(usize, String)>;

/// W3d2: `read_one_with_ns`/`read_one_allow_cond_with_ns` PLUS the start
/// offsets of every CONSTRUCTOR literal (`#pkg.Class[..]` /
/// `#pkg.Class{..}`) the read desugared, PLUS (W4-PRINTER) every generic
/// tagged literal's `(span-start, tag)` -- see `Reader::
/// ctor_literal_starts`'/`Reader::tag_literal_starts`' docs for why they
/// exist, and `builtins::reflect::read_string` for the only caller that
/// reads either.
pub(crate) fn read_one_with_ns_ctors(
    src: &str,
    ctx: NsContext,
    allow_read_cond: bool,
) -> Result<(Option<Form>, CtorLiteralStarts, TagLiteralStarts), RjError> {
    let mut r = if allow_read_cond {
        Reader::new_allow_read_cond(src)
    } else {
        Reader::new(src)
    };
    r.set_ns_ctx(ctx);
    let form = r.next_form()?;
    Ok((form, r.ctor_literal_starts, r.tag_literal_starts))
}

/// `read_one`, but with reader-conditional dispatch enabled --
/// `#[cfg(test)]` for the same reason as `read_one`; `(read-string
/// {:read-cond :allow} s)` itself goes through
/// `read_one_allow_cond_with_ns`.
#[cfg(test)]
pub fn read_one_allow_cond(src: &str) -> Result<Option<Form>, RjError> {
    Reader::new_allow_read_cond(src).next_form()
}

/// `read_one_allow_cond`, but resolving `::kw`/`::alias/kw` against `ctx`
/// -- the `{:read-cond :allow}` sibling of `read_one_with_ns`.
pub(crate) fn read_one_allow_cond_with_ns(src: &str, ctx: NsContext) -> Result<Option<Form>, RjError> {
    read_one_with_ns_ctors(src, ctx, true).map(|(f, _, _)| f)
}

/// `'` is deliberately absent: it is a legal Clojure symbol-constituent
/// character (`cam'`, `x''`) once a token is already under way. Leading
/// `'` still reads as the quote reader macro -- that dispatch happens in
/// `parse_one_form`, on the character BEFORE any token scanning starts, so
/// it never consults this predicate at all.
/// ASCII (0..128) fast-path table for [`is_delim`], computed once at
/// compile time from the exact same predicate the fallback below still
/// uses for non-ASCII input: real Rust `char::is_whitespace()` ASCII
/// whitespace is `\t \n \x0B \x0C \r` (0x09..=0x0D) plus space (0x20), so
/// the `const fn` below matches on those bytes explicitly rather than
/// hardcoding an assumption -- if that ever drifted from `is_whitespace`'s
/// real ASCII behavior this table (not just the comment) would be wrong.
const fn build_delim_table() -> [bool; 128] {
    let mut table = [false; 128];
    let mut i = 0;
    while i < 128 {
        let is_ws = matches!(i, 0x09..=0x0D | 0x20);
        let is_other_delim = matches!(
            i as u8 as char,
            ',' | '(' | ')' | '[' | ']' | '{' | '}' | '"' | ';' | '`' | '~' | '@'
        );
        table[i] = is_ws || is_other_delim;
        i += 1;
    }
    table
}

/// edn/fast: exposed `pub(crate)` so `edn_fast`'s byte-level scanner can
/// index it directly for ASCII delimiter detection instead of duplicating
/// this table -- see that module's doc for why identical delimiter
/// semantics matter (a token boundary mismatch would silently diverge from
/// the general reader on SUPPORTED input, which the fast path may never do).
pub(crate) static DELIM_TABLE: [bool; 128] = build_delim_table();

#[inline(always)]
fn is_delim(c: char) -> bool {
    if (c as u32) < 128 {
        DELIM_TABLE[c as usize]
    } else {
        c.is_whitespace()
    }
}

pub(crate) struct Reader<'a> {
    src: &'a str,
    /// BYTE offset into `src` -- invariant: always on a UTF-8 char
    /// boundary (every mutation goes through `advance`, which steps by
    /// exactly `char::len_utf8()`). edn/fast Wave 1b: this used to be an
    /// index into a pre-materialized `Vec<(usize, char)>` cursor (one
    /// `char_indices().collect()` per `Reader`, a full decode pass plus a
    /// ~16-byte-per-char allocation before any parsing began); `peek`/
    /// `peek_at`/`advance` now decode directly from `src` on demand
    /// instead.
    pos: usize,
    /// Gates `#?`/`#?@` reader-conditional dispatch (S5 / reader
    /// conditionals). Real Clojure only allows conditional read in two
    /// contexts: loading a `.cljc` file, or an explicit `(read-string
    /// {:read-cond :allow} s)` -- a bare `(read-string "#?(:clj 1)")`
    /// throws `"Conditional read not allowed"` (measured against the
    /// pinned oracle, `tests/conformance/corpus/reader-cond.corpus`).
    /// mova has no `.cljc` file-extension handling yet (`ns.rs`'s
    /// `ns_file_names` only tries `.mova`) -- that lands with the actual
    /// test.check vendoring next session -- so today this flag is `false`
    /// for every entry point EXCEPT the explicit opt-in threaded through
    /// by `Reader::new_allow_read_cond` (used by `read_one_allow_cond`,
    /// which `builtins::reflect::read_string`'s 2-arity
    /// `{:read-cond :allow}` form and (once wired) the `.cljc` file loader
    /// both call). Keeping the default OFF everywhere else -- `read_all`/
    /// `read_one` (used by plain `read-string`, `eval_str`, and thus every
    /// corpus/REPL/`.mova`-file read) -- means a bare `#?` in ordinary
    /// source is a reader error here exactly like on the JVM, rather than
    /// silently doing something mova-specific.
    allow_read_cond: bool,
    /// `{:read-cond :preserve}` (nREPL `read-cond: preserve`): a `#?(...)` is
    /// kept as a reader-conditional object instead of being resolved.
    preserve_read_cond: bool,
    /// C3d: `::kw`/`::alias/kw` auto-resolution context -- see
    /// `NsContext`'s doc. Defaults to `NsContext::user()`; `set_ns_ctx`
    /// (used by `eval::Interp::eval_str`/`eval_str_allow_read_cond`,
    /// fresh before every top-level form) overrides it with the real
    /// current namespace and alias table.
    ns_ctx: NsContext,
    /// W3d2: the START OFFSET of every CONSTRUCTOR literal this reader
    /// desugared (`#pkg.Class[..]` / `#pkg.Class{..}` -- see
    /// `read_tagged_literal`).
    ///
    /// Real Clojure's reader BUILDS the object while reading, so
    /// `(read-string "#user.R{:a 42}")` hands back the record and a
    /// literal naming an unconstructible class throws FROM `read-string`.
    /// mova's reader desugars to the equivalent `(pkg.Class. ..)` /
    /// `(pkg.Class/create {..})` form instead, and has to: `read_all`
    /// parses a whole FILE before evaluating any of it, so a `defrecord`
    /// further down the same file is not defined yet at read time (real
    /// Clojure reads and evaluates one form at a time and never has this
    /// problem).
    ///
    /// This list is how the ONE caller that CAN close the gap honestly --
    /// `builtins::reflect::read_string`, which has an interpreter and a
    /// single self-contained string, so every type it names is either
    /// already defined or genuinely absent -- finds those sub-forms and
    /// evaluates them. Empty for every other entry point, and never
    /// consulted by them.
    ///
    /// The `bool` is "this is the POSITIONAL `[..]` spelling" (as opposed
    /// to the `{..}` map spelling). The two forms' measured FAILURE
    /// CLASSES differ, and `hinting-test` asserts both separately:
    /// `#R[""]` on a `^long`-hinted field is `IllegalArgumentException`
    /// (the reflective `Reflector.invokeConstructor` path -- "no matching
    /// ctor"), while `#R{:a ""}` is `ClassCastException` (the
    /// `create`/`map->` path, which casts).
    ctor_literal_starts: CtorLiteralStarts,
    /// W4-PRINTER (task 2, print-throwable's `*data-readers*` binding):
    /// the `(span-start, tag)` of every GENERIC tagged literal
    /// (`#foo bar`, neither a ctor literal nor `#uuid`) this reader
    /// passed through -- see `read_tagged_literal`'s doc for why the
    /// returned `Form` is the PAYLOAD's own (not a synthesized `#foo`
    /// wrapper), so this is the only place the tag name survives at all.
    ///
    /// Same shape and same ONE caller as `ctor_literal_starts`:
    /// `builtins::reflect::read_string` is the one entry point with an
    /// `Interp` (and thus a live `*data-readers*` binding) to resolve
    /// these against post-parse -- every other caller (ordinary file
    /// loads, the REPL) never has a reason to consult `*data-readers*`
    /// mid-parse (`read_all` reads a whole file before any of it
    /// evaluates), so this stays empty and unconsulted there, at zero
    /// extra cost beyond the `push` itself (which only fires when a
    /// generic tagged literal is actually present in the source -- the
    /// overwhelmingly common case, no `#tag` at all, never touches this
    /// field).
    tag_literal_starts: TagLiteralStarts,
}

impl<'a> Reader<'a> {
    pub(crate) fn new(src: &'a str) -> Self {
        Reader {
            src,
            pos: 0,
            allow_read_cond: false,
            preserve_read_cond: false,
            ns_ctx: NsContext::user(),
            ctor_literal_starts: Vec::new(),
            tag_literal_starts: Vec::new(),
        }
    }

    /// Resumes reading `src` at byte `pos` (host form-by-form feeding, see
    /// `formfeed`). Spans stay relative to the whole `src`.
    pub(crate) fn resume(src: &'a str, pos: usize, allow_read_cond: bool) -> Self {
        let mut r = if allow_read_cond { Self::new_allow_read_cond(src) } else { Self::new(src) };
        r.pos = pos;
        r
    }

    /// Tags of the generic tagged literals the last read passed through, with
    /// the byte offset of each payload (see `tag_literal_starts`).
    pub(crate) fn tag_literals(&self) -> &[(usize, String)] {
        &self.tag_literal_starts
    }

    /// Byte offset just after the last form read.
    pub(crate) fn position(&self) -> usize {
        self.pos
    }

    /// Same as `new`, but with reader-conditional dispatch enabled -- see
    /// `allow_read_cond`'s own doc comment for which callers use this.
    pub(crate) fn new_allow_read_cond(src: &'a str) -> Self {
        Reader {
            src,
            pos: 0,
            allow_read_cond: true,
            preserve_read_cond: false,
            ns_ctx: NsContext::user(),
            ctor_literal_starts: Vec::new(),
            tag_literal_starts: Vec::new(),
        }
    }

    /// Overrides the `::kw`/`::alias/kw` resolution context for every
    /// form read from this point on -- see `NsContext`'s doc for why the
    /// caller (`eval::Interp::eval_str`/`eval_str_allow_read_cond`) calls
    /// this fresh before EACH top-level form rather than once up front.
    /// Keep `#?(...)` as a reader-conditional object (see `reader_cond_value`).
    pub(crate) fn set_preserve_read_cond(&mut self, on: bool) {
        self.preserve_read_cond = on;
    }

    pub(crate) fn set_ns_ctx(&mut self, ctx: NsContext) {
        self.ns_ctx = ctx;
    }

    /// ASCII fast path first (the overwhelming common case in real
    /// source): a single byte compare and cast, no UTF-8 decode. Only
    /// falls to `str::chars().next()` -- a real decode -- when the byte at
    /// `pos` starts a multi-byte sequence.
    #[inline(always)]
    fn peek(&self) -> Option<char> {
        let b = *self.src.as_bytes().get(self.pos)?;
        if b < 0x80 {
            Some(b as char)
        } else {
            self.src[self.pos..].chars().next()
        }
    }

    /// `k` is always a small constant (1) at every call site in this
    /// reader (one-character-of-lookahead dispatch, e.g. `#_`/`#?`/`\u`) --
    /// this is O(k) via `chars().nth(k)` rather than O(1), which would
    /// only matter if some caller looped a growing `k`, which none do.
    fn peek_at(&self, k: usize) -> Option<char> {
        self.src[self.pos..].chars().nth(k)
    }

    #[inline(always)]
    fn offset(&self) -> usize {
        self.pos
    }

    fn advance(&mut self) -> Option<char> {
        let c = self.peek();
        if let Some(c) = c {
            self.pos += c.len_utf8();
        }
        c
    }

    /// edn/fast Wave 1b-fix: advances past `c`, a char the caller ALREADY
    /// obtained from `peek()` -- unlike `advance()`, this never re-decodes
    /// it. Every hot per-character loop below (`skip_trivia`, `read_token`,
    /// `read_string`) was structurally `match self.peek() { Some(c) => {
    /// ...; self.advance() } }` -- `self.advance()`'s OWN first act is to
    /// call `self.peek()` again, so each consumed character paid for the
    /// decode twice. `bump` is the fix: the caller already knows `c`, so
    /// stepping `pos` by `c.len_utf8()` needs no second decode at all.
    /// Debug-only assertion guards against a caller passing a `c` that
    /// doesn't actually match what's at `pos` right now (stale/wrong
    /// char), which would silently desync the byte-offset invariant in
    /// release builds -- checked only in debug because `peek()` itself
    /// isn't free, and this fn's whole point is to avoid paying for it
    /// again in release.
    #[inline(always)]
    fn bump(&mut self, c: char) {
        debug_assert_eq!(self.peek(), Some(c), "bump: c does not match what peek() sees at pos");
        self.pos += c.len_utf8();
    }

    /// Skips whitespace, commas, `;` line comments, and `#_form` discards,
    /// leaving `peek()` at the next substantive character (or EOF).
    fn skip_trivia(&mut self) -> Result<(), RjError> {
        loop {
            match self.peek() {
                Some(c) if c.is_whitespace() || c == ',' => {
                    self.bump(c);
                }
                Some(';') => {
                    while let Some(c) = self.peek() {
                        if c == '\n' {
                            break;
                        }
                        self.bump(c);
                    }
                }
                Some('#') if self.peek_at(1) == Some('_') => {
                    let hash_start = self.offset();
                    self.advance();
                    self.advance();
                    self.skip_trivia()?;
                    match self.peek() {
                        None => {
                            return Err(RjError::reader(
                                "expected a form to discard after #_",
                                Span {
                                    start: hash_start,
                                    end: hash_start + 2,
                                },
                                "expected a form to follow this #_",
                            ))
                        }
                        Some(c) => {
                            // The discarded form may itself be `#?(...)`
                            // -- must still be read (and, per the oracle,
                            // fully VALIDATED as a reader conditional,
                            // including its own feature-matching and
                            // nested content) even though the result is
                            // thrown away; see `read_form_cond_aware`'s
                            // doc comment.
                            self.read_form_cond_aware(c)?;
                        }
                    }
                }
                _ => break,
            }
        }
        Ok(())
    }

    /// Reads the next top-level form, or `None` at true EOF. A `#?(...)`
    /// here that matches no feature vanishes entirely (Clojure: reading
    /// `"#?(:cljs 1)"` alone hits EOF, since there was never a form there
    /// at all) -- so this loops rather than returning `None` prematurely.
    /// `#?@` splicing is deliberately NOT allowed at this one call site
    /// (`splice_allowed: false` below) -- measured:
    /// `(read-string {:read-cond :allow} "#?@(:clj [1 2])")` throws
    /// "Reader conditional splicing not allowed at the top level"; every
    /// other reader-conditional call site in this file passes `true`.
    pub(crate) fn next_form(&mut self) -> Result<Option<Form>, RjError> {
        loop {
            self.skip_trivia()?;
            match self.peek() {
                None => return Ok(None),
                Some('#') if self.peek_at(1) == Some('?') => {
                    let hash_start = self.offset();
                    self.advance(); // consume '#'
                    let items = self.read_reader_cond(hash_start, false)?;
                    if let Some(f) = items.into_iter().next() {
                        return Ok(Some(f));
                    }
                    // no feature matched: this `#?(...)` produced no form
                    // at all -- keep scanning for the next real one.
                }
                Some(c) => return Ok(Some(self.parse_one_form(c)?)),
            }
        }
    }

    /// Reads one syntactic unit at `c`: an ordinary form (exactly one
    /// result), or a reader conditional (`c == '#'` and the next char is
    /// `?`) which may yield zero results (no feature matched), one
    /// (`#?`, or `#?@` used where only its first element ends up
    /// mattering -- see `read_reader_cond`'s doc comment), or many
    /// (`#?@`'s splice). Used by `read_delimited` so a `#?@` splices
    /// directly into the surrounding list/vector/map/set, and by
    /// `read_form_cond_aware` for every OTHER reader-conditional-aware
    /// call site (feature/value slots inside `read_cond_body` itself,
    /// `#_`'s discard target, metadata and its target in `read_meta`, a
    /// tagged literal's payload).
    fn parse_one_unit(&mut self, c: char) -> Result<Vec<Form>, RjError> {
        if c == '#' && self.peek_at(1) == Some('?') {
            let hash_start = self.offset();
            self.advance(); // consume '#'
            self.read_reader_cond(hash_start, true)
        } else {
            Ok(vec![self.parse_one_form(c)?])
        }
    }

    /// Reads exactly ONE form at `c`, reader-conditional-aware: if `c`
    /// starts a `#?`/`#?@` dispatch, resolves it via `parse_one_unit` and
    /// takes its single result. A `#?@` that spliced to more than one
    /// element here (only possible when a splicing conditional is used
    /// somewhere OTHER than directly inside a collection literal -- e.g.
    /// as a reader conditional's own feature/value slot) keeps only the
    /// FIRST spliced element; the JVM reader's actual behavior for that
    /// obscure corner is to queue the rest on an internal pending-forms
    /// list for the *next* `read` call to drain (measured:
    /// `#?(:clj #?@(:clj [1 2]))` => `1`, not `[1 2]` or an error) -- no
    /// real `.cljc` file relies on this, so this file approximates it
    /// (first element only, remainder silently dropped) rather than
    /// implementing the JVM's pending-forms queue. A `#?(...)` that
    /// matched no feature here (zero results) is an error: a value was
    /// syntactically required at this position.
    fn read_form_cond_aware(&mut self, c: char) -> Result<Form, RjError> {
        let start = self.offset();
        let mut items = self.parse_one_unit(c)?;
        if items.is_empty() {
            return Err(RjError::reader(
                "reader conditional matched no feature but a form was required here",
                Span { start, end: self.offset() },
                "this reader conditional produced no value",
            ));
        }
        Ok(items.remove(0))
    }

    /// Reads exactly one form. Assumes `skip_trivia` has already run and
    /// `c == self.peek().unwrap()`.
    fn parse_one_form(&mut self, c: char) -> Result<Form, RjError> {
        match c {
            '(' => self.read_list(),
            '[' => self.read_vector(),
            '{' => self.read_map(),
            '#' => self.read_hash(),
            ')' | ']' | '}' => {
                let start = self.offset();
                self.advance();
                Err(RjError::reader(
                    format!("unexpected '{c}'"),
                    Span {
                        start,
                        end: start + 1,
                    },
                    "no matching opening delimiter",
                ))
            }
            '"' => self.read_string(),
            '\\' => self.read_char(),
            ':' => self.read_keyword(),
            '\'' => self.read_prefixed_form(1, "quote"),
            '`' => self.read_prefixed_form(1, "quasiquote"),
            '~' => {
                if self.peek_at(1) == Some('@') {
                    self.read_prefixed_form(2, "unquote-splicing")
                } else {
                    self.read_prefixed_form(1, "unquote")
                }
            }
            '@' => self.read_prefixed_form(1, "deref"),
            '^' => self.read_meta(),
            _ => self.read_atom(),
        }
    }

    /// Consumes `open` (assumed to be `peek()`), reads forms until `close`,
    /// and returns them along with the span covering the whole delimited
    /// region (from `open` through `close`, inclusive).
    fn read_delimited(&mut self, close: char, ctx: &str) -> Result<(Vec<Form>, Span), RjError> {
        let start = self.offset();
        self.advance();
        let mut items = Vec::new();
        loop {
            self.skip_trivia()?;
            match self.peek() {
                None => {
                    return Err(RjError::reader(
                        format!("unclosed {ctx}"),
                        Span {
                            start,
                            end: start + 1,
                        },
                        format!("unclosed {ctx}, opened here"),
                    ))
                }
                Some(c) if c == close => {
                    self.advance();
                    let end = self.offset();
                    return Ok((items, Span { start, end }));
                }
                // `parse_one_unit`, not `parse_one_form`: a `#?@(...)`
                // here splices its matched list/vector's elements in as
                // siblings (0, 1, or many items), the whole reason
                // `#?@` exists -- see that method's doc comment.
                Some(c) => items.extend(self.parse_one_unit(c)?),
            }
        }
    }

    fn read_list(&mut self) -> Result<Form, RjError> {
        let (items, span) = self.read_delimited(')', "list")?;
        Ok(Form {
            meta: None,
            value: FormValue::List(items),
            span,
        })
    }

    fn read_vector(&mut self) -> Result<Form, RjError> {
        let (items, span) = self.read_delimited(']', "vector")?;
        Ok(Form {
            meta: None,
            value: FormValue::Vector(items),
            span,
        })
    }

    fn read_map(&mut self) -> Result<Form, RjError> {
        let (items, span) = self.read_delimited('}', "map literal")?;
        if items.len() % 2 != 0 {
            return Err(RjError::reader(
                "map literal must contain an even number of forms",
                span,
                "odd number of forms in this map literal",
            ));
        }
        let mut pairs = Vec::with_capacity(items.len() / 2);
        let mut it = items.into_iter();
        while let (Some(k), Some(v)) = (it.next(), it.next()) {
            pairs.push((k, v));
        }
        // C10: a literal map with a repeated key is a READ-time error on
        // the real JVM (measured: `(read-string "{:a 1 :a 2}")` throws
        // `IllegalArgumentException: Duplicate key: :a`), not merely a
        // last-value-wins runtime collapse -- `data_structures.clj`'s
        // `test-duplicates` asserts the throw directly. Checked via
        // `form_to_value` + `Value`'s own `PartialEq`/`Hash` -- the SAME
        // equality a map literal's eventual `Value::Map` construction
        // uses for key uniqueness, so "duplicate" here means exactly what
        // it means at eval time.
        //
        // PERF: was O(n^2) over the pairs -- worse, it re-ran
        // `form_to_value` on every EARLIER key on every outer step
        // (O(n^2) conversions, not just comparisons), measured to cost
        // 25+ms alone on a large real-world literal (`clojure-lsp.
        // common-symbols`'s `#{...}` of ~1000 `{:name .. :kind ..}`
        // maps -- see `read_set` below, the same fix, for that one). A
        // running `HashSet<Value>` (`Value` already has `Hash`+`Eq` for
        // exactly this reason -- it backs `Value::Map`'s own key
        // uniqueness) converts each key exactly ONCE and detects the
        // FIRST repeat in one linear pass, reporting the identical
        // duplicate (same index, same span, same message) as the old
        // pairwise scan: `HashSet::insert` returning `false` on `pairs[idx]`
        // means some EARLIER `pairs[..idx]` key already equals it, which
        // is exactly the old inner loop's condition.
        let mut seen = std::collections::HashSet::with_capacity(pairs.len());
        for pair in &pairs {
            let key = form_to_value(&pair.0);
            if !seen.insert(key.clone()) {
                return Err(RjError::reader(
                    format!("Duplicate key: {}", crate::printer::pr_str(&key)),
                    pair.0.span,
                    "duplicate key in this map literal",
                )
                // W3a: the measured class is
                // `java.lang.IllegalArgumentException` -- real Clojure's
                // reader raises it RAW, it does NOT wrap this one in a
                // `LispReader$ReaderException` (measured:
                // `(read-string "{:a 1 :a 2}")` surfaces the IAE
                // directly), so `ErrorKind::Reader`'s default chain
                // would have been wrong here.
                .with_class(JvmClass::IllegalArgument));
            }
        }
        Ok(Form {
            meta: None,
            value: FormValue::Map(pairs),
            span,
        })
    }

    fn read_hash(&mut self) -> Result<Form, RjError> {
        let start = self.offset();
        self.advance(); // consume '#'
        match self.peek() {
            Some('{') => self.read_set(start),
            Some('(') => self.read_fn_literal(start),
            Some('"') => self.read_regex(start),
            // `#'x` -> `(var x)`.
            Some('\'') => self.read_hash_prefixed_form(start, 1, "var"),
            // `#^{:x 1} foo` is the archaic spelling of `^{:x 1} foo` --
            // identical semantics (measured against 1.13.0-alpha6:
            // `(read-string "#^{:x 1} foo")` => `foo`, `(meta ...)` =>
            // `{:x 1}`). `self.peek()` is still `Some('^')` here (only the
            // `#` was consumed above), so this hands off to exactly the
            // same `read_meta` the bare top-level `'^'` arm calls -- that
            // method's own first act is `self.advance()` over the `^`, so
            // there's no consumption-offset mismatch to account for.
            // MUST be checked before the permissive tagged-literal arm
            // below, else `^` (not a delimiter) reads as a tag symbol and
            // leaves the metadata map and its target as two separate
            // top-level forms -- the exact bug this arm fixes.
            Some('^') => self.read_meta(),
            // `##Inf`/`##-Inf`/`##NaN`: symbolic float values. MUST be
            // checked before the permissive tagged-literal arm below --
            // without this, `#` immediately followed by another `#` fell
            // into `read_tagged_literal`, which read "Inf" as a TAG and
            // then tried to read the NEXT form as its payload, producing
            // bogus "unexpected ')'" errors on every one of `math.clj`'s
            // 136 `##Inf`/`##NaN` occurrences (COMPATIBILITY.md's
            // data-derived build queue).
            Some('#') => self.read_symbolic_value(start),
            // SPEC-W6b: `#:ns{...}` / `#::{...}` / `#::alias{...}` --
            // Clojure 1.9's NAMESPACED MAP literal. MUST be checked
            // before the tagged-literal arm below: `:` is not a
            // delimiter, so `#:clojure.spec.alpha{..}` used to read as a
            // tag `":clojure.spec.alpha"` and, because that tag contains
            // a `.` and is followed by `{`, as a RECORD ctor literal --
            // `(:clojure.spec.alpha/create {...})`, a list, silently.
            // `#:foo{..}` (no dot) fell through the other way and read as
            // the bare map with its keys UNqualified, which is worse: a
            // wrong value with no error at all.
            Some(':') => self.read_ns_map_literal(start),
            // Any other non-delimiter char starts a tag symbol: `#tag form`
            // (R2's permissive tagged-literal pass-through, see
            // `read_tagged_literal`'s doc).
            Some(c) if !is_delim(c) => self.read_tagged_literal(start),
            Some(c) => {
                let end = self.offset() + c.len_utf8();
                Err(RjError::reader(
                    format!("unsupported reader macro '#{c}'"),
                    Span { start, end },
                    "unsupported reader macro",
                ))
            }
            None => Err(RjError::reader(
                "unexpected EOF after '#'",
                Span {
                    start,
                    end: start + 1,
                },
                "expected a dispatch character after this #",
            )),
        }
    }

    /// `#?(:clj X :cljs Y :default Z)` / `#?@(...)` (S5 / reader
    /// conditionals). `hash_start` is the offset of the already-consumed
    /// `#`; `self.peek() == Some('?')` here (callers -- `next_form`,
    /// `parse_one_unit` -- check this before consuming the `#` and
    /// calling in). `splice_allowed` is `false` only for `next_form`'s
    /// own top-level call (see that method's doc comment); every nested
    /// call site passes `true`.
    ///
    /// Returns the selected form(s): empty (no feature in the active set
    /// `{:clj :default}` matched -- mova has no `:cljs`/other platform,
    /// so those never match), a single-element vec (`#?`, or `#?@` used
    /// outside a collection -- see `read_form_cond_aware`), or (only for
    /// `#?@` whose caller is `read_delimited`, i.e. actually splicing
    /// into a collection literal) the matched list/vector's own elements.
    ///
    /// Gating and grammar, all measured against the pinned oracle
    /// (`tests/conformance/corpus/reader-cond.corpus`,
    /// `(read-string {:read-cond :allow} "...")` transcripts):
    /// - `!self.allow_read_cond` -> throws unconditionally, matching
    ///   `(read-string "#?(:clj 1)")` => "Conditional read not allowed".
    /// - `splicing && !splice_allowed` -> "Reader conditional splicing
    ///   not allowed at the top level".
    /// - body must start with `(` after skipping trivia (and an optional
    ///   `@`).
    fn read_reader_cond(&mut self, hash_start: usize, splice_allowed: bool) -> Result<Vec<Form>, RjError> {
        self.advance(); // consume '?'
        let splicing = self.peek() == Some('@');
        if splicing {
            self.advance();
        }
        if !self.allow_read_cond {
            return Err(RjError::reader(
                "Conditional read not allowed",
                Span {
                    start: hash_start,
                    end: self.offset(),
                },
                "reader conditionals need {:read-cond :allow} (read-string) or a .cljc file",
            ));
        }
        if splicing && !splice_allowed {
            return Err(RjError::reader(
                "Reader conditional splicing not allowed at the top level",
                Span {
                    start: hash_start,
                    end: self.offset(),
                },
                "wrap this #?@ in a surrounding collection",
            ));
        }
        self.skip_trivia()?;
        match self.peek() {
            Some('(') => {}
            _ => {
                return Err(RjError::reader(
                    "reader conditional body must start with '('",
                    Span {
                        start: hash_start,
                        end: self.offset(),
                    },
                    "expected '(' here",
                ))
            }
        }
        if self.preserve_read_cond {
            let body = self.read_form_cond_aware('(')?;
            let span = Span { start: hash_start, end: body.span.end };
            let list = form_to_value(&body);
            return Ok(vec![Form { meta: None, value: FormValue::Atom(reader_cond_value(list, splicing)), span }]);
        }
        let matched = self.read_cond_body()?;
        match matched {
            None => Ok(Vec::new()),
            Some(form) if splicing => match form.value {
                FormValue::List(items) | FormValue::Vector(items) => Ok(items),
                _ => Err(RjError::reader(
                    "Spliced form list in read-cond-splicing must implement java.util.List",
                    form.span,
                    "this must read as a list or vector to splice",
                )),
            },
            Some(form) => Ok(vec![form]),
        }
    }

    /// Reads the parenthesized `(:feature form :feature form ...)` body of
    /// a reader conditional (assumes `peek() == Some('(')`), returning the
    /// FIRST matching pair's form, or `None` if no feature in the active
    /// set matched. Mirrors `clojure.lang.LispReader.readCond`'s measured
    /// behavior exactly:
    /// - Before a match is found, pairs are strictly validated: the
    ///   feature MUST read as a keyword, else "Feature should be a
    ///   keyword: ...".
    /// - Once matched, EVERY remaining token up to the closing `)` is read
    ///   and discarded ONE AT A TIME with NO pairing/keyword validation at
    ///   all -- measured: `#?(:clj 1 :cljs)` (odd trailing count) and
    ///   `#?(:clj 1 6 7)` (non-keyword trailing tokens) both succeed
    ///   silently once `:clj` has already matched.
    /// - First match wins by SOURCE ORDER regardless of which specific
    ///   feature matched (`:default` first beats `:clj` second, and vice
    ///   versa) -- measured `#?(:default 1 :clj 2)` => `1` and
    ///   `#?(:cljs 9 :default 1 :clj 2)` => `1`.
    fn read_cond_body(&mut self) -> Result<Option<Form>, RjError> {
        let open = self.offset();
        self.advance(); // consume '('
        let mut matched: Option<Form> = None;
        loop {
            self.skip_trivia()?;
            let c = match self.peek() {
                None => {
                    return Err(RjError::reader(
                        "unclosed reader conditional",
                        Span { start: open, end: open + 1 },
                        "unclosed reader conditional, opened here",
                    ))
                }
                Some(')') => {
                    self.advance();
                    break;
                }
                Some(c) => c,
            };
            if matched.is_some() {
                // Post-match: discard exactly one form, no validation.
                self.read_form_cond_aware(c)?;
                continue;
            }
            let feature = self.read_form_cond_aware(c)?;
            let is_kw = matches!(&feature.value, FormValue::Atom(Value::Keyword(_)));
            if !is_kw {
                return Err(RjError::reader(
                    "Feature should be a keyword",
                    feature.span,
                    "expected a keyword feature here",
                ));
            }
            self.skip_trivia()?;
            let form_c = self.peek().ok_or_else(|| {
                RjError::reader(
                    "unclosed reader conditional",
                    Span { start: open, end: open + 1 },
                    "unclosed reader conditional, opened here",
                )
            })?;
            let form = self.read_form_cond_aware(form_c)?;
            if matched.is_none() && Self::feature_matches(&feature) {
                matched = Some(form);
            }
        }
        Ok(matched)
    }

    /// The active reader-conditional feature set is `{:mova :clj :default}`
    /// -- mova targets the JVM-shaped host platform, so `:cljs`/`:cljr`/
    /// any other platform keyword never matches (measured:
    /// `#?(:cljs 1)` alone => vanishes / EOF, matching real Clojure run on
    /// a JVM `clojure` process, never `:cljs`). `:mova` lets ported
    /// libraries branch around JVM-only code; like every feature, the
    /// first matching branch in source order wins, so ports write
    /// `#?(:mova m :clj j)`.
    fn feature_matches(feature: &Form) -> bool {
        matches!(&feature.value, FormValue::Atom(Value::Keyword(k)) if k.as_ref() == "mova" || k.as_ref() == "clj" || k.as_ref() == "default")
    }

    /// `#<extra><form>` -> `(<sym_name> <form>)`, e.g. `#'x` -> `(var x)`.
    /// `hash_start` is the offset of the already-consumed `#`; `extra_len`
    /// is how many more prefix characters (here just the `'`) to consume
    /// before reading the wrapped form. Mirrors `read_prefixed_form` but
    /// anchors the synthesized head symbol's span at the `#`, not at
    /// `extra_len` characters past it.
    fn read_hash_prefixed_form(&mut self, hash_start: usize, extra_len: usize, sym_name: &str) -> Result<Form, RjError> {
        for _ in 0..extra_len {
            self.advance();
        }
        let prefix_end = self.offset();
        match self.next_form()? {
            None => Err(RjError::reader(
                format!("expected a form after '{}'", &self.src[hash_start..prefix_end]),
                Span {
                    start: hash_start,
                    end: prefix_end,
                },
                format!("expected a form to follow this '{}'", &self.src[hash_start..prefix_end]),
            )),
            Some(inner) => {
                let end = inner.span.end;
                let head = Form {
                    meta: None,
                    value: FormValue::Atom(Value::Sym(Symbol::simple(sym_name))),
                    span: Span {
                        start: hash_start,
                        end: prefix_end,
                    },
                };
                Ok(Form {
                    meta: None,
                    value: FormValue::List(vec![head, inner]),
                    span: Span { start: hash_start, end },
                })
            }
        }
    }

    /// `#:ns{...}` -- Clojure 1.9's NAMESPACED MAP literal (SPEC-W6b).
    /// `hash_start` is the offset of the already-consumed `#`;
    /// `self.peek() == Some(':')` here.
    ///
    /// Three spellings, all of which name a namespace and then a map:
    ///
    /// * `#:foo{:a 1}`        -> `{:foo/a 1}`
    /// * `#::{:a 1}`          -> `{:<current-ns>/a 1}`
    /// * `#::alias{:a 1}`     -> `{:<what alias names>/a 1}`
    ///
    /// The `::`/`::alias` spellings resolve against exactly the same
    /// read-time `ns_ctx` that `::kw`/`::alias/kw` do (see
    /// `resolve_auto_keyword`), so the two features cannot disagree about
    /// what a namespace alias means.
    ///
    /// Qualification follows `LispReader.namespaceMap` exactly, and
    /// applies to KEYS only:
    ///
    /// * an unqualified keyword or symbol key gains the map's namespace;
    /// * a key whose namespace is the literal `_` LOSES its namespace
    ///   (`#:foo{:_/a 1}` -> `{:a 1}`, upstream's documented opt-out);
    /// * an already-qualified key, and any key that is neither a keyword
    ///   nor a symbol, is left exactly as written.
    ///
    /// Values are never touched (a nested `#:other{...}` value is its own
    /// literal and was already qualified by its own read).
    fn read_ns_map_literal(&mut self, hash_start: usize) -> Result<Form, RjError> {
        self.advance(); // consume the ':'
        let auto_resolve = self.peek() == Some(':');
        if auto_resolve {
            self.advance(); // consume the second ':'
        }
        let (token, _) = self.read_token();
        let prefix_end = self.offset();
        let invalid = |end: usize| {
            RjError::reader(
                format!("Invalid token: {}", &self.src[hash_start..end]),
                Span { start: hash_start, end },
                "invalid namespaced map literal",
            )
        };
        // Resolve the prefix to a real namespace NAME.
        let ns: Str = if auto_resolve {
            if token.is_empty() {
                // `#::{...}` -- the current namespace.
                self.ns_ctx.current_ns.clone()
            } else if token.contains('/') {
                return Err(invalid(prefix_end));
            } else {
                match self.ns_ctx.aliases.get(token) {
                    Some(full) => full.clone(),
                    // Upstream: "Unknown auto-resolved namespace alias".
                    None => return Err(invalid(prefix_end)),
                }
            }
        } else if token.is_empty() || token.contains('/') {
            return Err(invalid(prefix_end));
        } else {
            Str::from(token)
        };

        self.skip_trivia()?;
        if self.peek() != Some('{') {
            let end = self.offset().max(prefix_end);
            return Err(RjError::reader(
                format!(
                    "Namespaced map must specify a map: {}",
                    &self.src[hash_start..prefix_end]
                ),
                Span { start: hash_start, end },
                "expected a map literal after this namespace prefix",
            ));
        }
        let map = self.read_map()?;
        let span = Span { start: hash_start, end: map.span.end };
        let FormValue::Map(pairs) = map.value else {
            // `read_map` only ever returns `FormValue::Map`.
            unreachable!("read_map returned a non-map form");
        };
        let pairs: Vec<(Form, Form)> = pairs
            .into_iter()
            .map(|(k, v)| (qualify_ns_map_key(k, &ns), v))
            .collect();
        // Re-run `read_map`'s own duplicate-key check: qualification can
        // MAKE two distinct written keys equal (`#:a{:x 1 :a/x 2}`), and
        // the JVM reader throws `Duplicate key` for exactly that.
        for idx in 1..pairs.len() {
            let key = form_to_value(&pairs[idx].0);
            for earlier in &pairs[..idx] {
                if form_to_value(&earlier.0) == key {
                    return Err(RjError::reader(
                        format!("Duplicate key: {}", crate::printer::pr_str(&key)),
                        span,
                        "this key appears twice once the map's namespace is applied",
                    ));
                }
            }
        }
        Ok(Form {
            meta: None,
            value: FormValue::Map(pairs),
            span,
        })
    }

    /// `#tag form`: reads and discards the tag symbol, returning `form`
    /// UNCHANGED. mova has no data-reader registry (no `*data-readers*`,
    /// no `#uuid`/`#inst` special-casing) -- this is a deliberately
    /// permissive stand-in for Clojure's `*default-data-reader-fn*`, wired
    /// to `identity`: every dispatch tag that isn't one of the built-ins
    /// above (`#(`, `#{`, `#"`, `#_`, `#'`) is accepted and its payload
    /// passed through as-is. The motivating case is the target codebase's
    /// `#cpp 300`, a C++-side literal tag that should just read as `300`
    /// here; a genuinely malformed dispatch (a delimiter with no tag, EOF)
    /// still reaches `read_hash`'s error arms, since only non-delimiter
    /// characters land here.
    fn read_tagged_literal(&mut self, hash_start: usize) -> Result<Form, RjError> {
        let (tag, _) = self.read_token();
        self.skip_trivia()?;
        // C14 (protocols): `#pkg.Class[args...]` / `#pkg.Class{k v ...}`
        // -- Clojure's CONSTRUCTOR-LITERAL reader syntax (`protocols.clj`'s
        // `test-ctor-literals`/`exercise-literals`/`test-statics`/
        // `hinting-test` deftests), distinct from the ordinary tagged-
        // literal passthrough below. The real reader decides "is this a
        // ctor literal" by resolving `tag` against the LIVE classpath at
        // READ time; mova's reader has no such registry (and couldn't:
        // `read_all` parses an entire file before any of it evaluates, so
        // a `defrecord` earlier in the same file isn't "defined" yet at
        // read time either way). The heuristic used instead: a DOTTED tag
        // (contains `.` -- true of every real class name, and of every
        // ctor literal this suite's own vendored source spells out; a
        // bare `#uuid`/`#inst`/future custom tag never is) immediately
        // followed by `[` or `{` is read as sugar for the ordinary forms
        // that already build a value of that class -- `#C[a b]` desugars
        // to the LIST form `(C. a b)` (the existing `(Ctor. args)` head
        // hook, `eval::types_forms::eval_ctor_form`), `#C{k v}` to
        // `(C/create {k v})` (the existing static-factory route,
        // `eval_deftype_like`'s `create` registration -- records only,
        // never called with `{}` for a `deftype` anywhere in scope).
        // Resolution of `C` itself -- builtin class, user record/deftype,
        // or unknown -- happens at EVAL time, same as it would for
        // hand-written `(C. a b)` source; an unresolvable tag is an
        // ordinary "unresolved symbol"/"no constructor interop" runtime
        // error, not a read-time one. A dotted tag followed by `(` is a
        // read-time error ("Unreadable constructor form"), matching the
        // measured shape of `#java.util.Locale("en")` -- real ctor
        // literals are never parenthesized. `*constants only*`
        // validation (`exercise-literals`' "only work with constants or
        // statics" sub-test) is NOT enforced here -- out of scope for
        // this veneer, see the module's own campaign notes.
        //
        // `!tag.contains('/')` matters: a NAMESPACED data-reader tag like
        // `#my.ns/tag [1 2 3]` (`r2_test.rs`'s own
        // `other_tags_also_pass_through_unchanged`, pre-existing/measured
        // passthrough behavior) also contains a `.` but is never a class
        // name -- real class tokens never contain `/`, that character is
        // reserved for the ns/name split. Without this guard the
        // namespaced-tag test regressed: `[1 2 3]` right after the tag
        // was wrongly read as this branch's ctor-vector-literal sugar.
        if tag.contains('.') && !tag.contains('/') {
            match self.peek() {
                Some('[') => return self.read_ctor_vector_literal(hash_start, tag),
                Some('{') => return self.read_ctor_map_literal(hash_start, tag),
                Some('(') => {
                    let start = self.offset();
                    return Err(RjError::reader(
                        format!("Unreadable constructor form: \"#{tag} (...)\""),
                        Span { start, end: start + 1 },
                        "constructor literals use [args] or {field val ...}, never (...)",
                    ));
                }
                _ => {}
            }
        }
        // S6 (assert/namespace/uuid batch): `#uuid` is the ONE tag that
        // stops being a pass-through -- see `Value::Uuid`'s own doc for
        // the representation. `#inst` and every other tag are
        // DELIBERATELY left alone (still identity passthrough, per the
        // doc below) -- no vendored-suite site needs `#inst` to be a
        // real value, only `#uuid` (predicates.clj's `java.util.UUID/
        // randomUUID` truth-table row, which round-trips through
        // `pr-str`/`read-string`).
        if tag == "uuid" {
            return self.read_uuid_literal(hash_start);
        }
        // SPEC-W1 task 4: `#inst` is the SECOND tag that stops being a
        // pass-through. It reads into mova's existing instant shape --
        // `HostKind::Date`, epoch millis, the same value `(java.util.
        // Date. n)` builds -- so `inst?`/`inst-ms` answer it without a
        // second instant representation existing. Grammar and the
        // millisecond truncation rule: `hostclass::parse_inst`'s own doc.
        if tag == "inst" {
            return self.read_inst_literal(hash_start);
        }
        match self.peek() {
            None => Err(RjError::reader(
                "expected a form after tagged literal",
                Span {
                    start: hash_start,
                    end: self.offset(),
                },
                "expected a form to follow this tag",
            )),
            Some(c) => {
                let payload = self.read_form_cond_aware(c)?;
                // W4-PRINTER: record `(payload's own span start, tag)` --
                // the returned `Form` IS the payload, unwrapped (see this
                // fn's own doc: "reads and discards the tag symbol,
                // returning form UNCHANGED"), so the payload's span start
                // is the only stable handle back to which literal this
                // was once `read_string` (the one caller with a live
                // `*data-readers*` binding to resolve it against) walks
                // the fully-parsed tree afterward.
                self.tag_literal_starts.push((payload.span.start, tag.to_string()));
                Ok(payload)
            }
        }
    }

    /// `#pkg.Class[a b c]` -> the LIST form `(pkg.Class. a b c)` -- see
    /// `read_tagged_literal`'s doc for the full rationale. `tag`'s trailing
    /// `.` makes `special_forms.rs`'s ctor-form head check (`ends_with('.')
    /// && len() > 1`) fire exactly the way hand-written `(pkg.Class. a b
    /// c)` source would.
    fn read_ctor_vector_literal(&mut self, hash_start: usize, tag: &str) -> Result<Form, RjError> {
        let (items, span) = self.read_delimited(']', "constructor literal")?;
        let full_span = Span { start: hash_start, end: span.end };
        // W4C-NS (`exercise-literals`'s "that ctor literals only work with
        // constants or statics"): same shape as `read_ctor_map_literal`'s
        // sibling keyword-keys check just below -- see
        // `is_ctor_literal_constant`'s own doc for why this is needed at
        // all and why checking each arg's SHAPE (not evaluating it)
        // reproduces the real JVM reader's effective behavior.
        for item in &items {
            if !is_ctor_literal_constant(item) {
                return Err(RjError::reader(
                    format!("Unreadable constructor form: \"#{tag}[...]\""),
                    item.span,
                    "a constructor literal's arguments must be constants or statics, not arbitrary forms",
                ));
            }
        }
        let head = Form {
            meta: None,
            value: FormValue::Atom(Value::Sym(Symbol::simple(format!("{tag}.")))),
            span: full_span,
        };
        let mut list = Vec::with_capacity(items.len() + 1);
        list.push(head);
        list.extend(items);
        self.ctor_literal_starts.push((full_span.start, true));
        Ok(Form {
            meta: None,
            value: FormValue::List(list),
            span: full_span,
        })
    }

    /// `#pkg.Record{:a 1 :b 2}` -> the LIST form `(pkg.Record/create {:a 1
    /// :b 2})` -- see `read_tagged_literal`'s doc. Only records ever
    /// spell a ctor literal this way in the vendored suite (a `deftype`
    /// has no map nature to construct from), and `Class/create` (`eval::
    /// types_forms::eval_deftype_like`'s registration, C14) is exactly a
    /// record's map-factory, reached via the SAME `Symbol{ns, name}`
    /// static-call resolution `RecordName/getBasis` uses.
    fn read_ctor_map_literal(&mut self, hash_start: usize, tag: &str) -> Result<Form, RjError> {
        let map_form = self.read_map()?;
        // W3d2, measured (`exercise-literals`' "that ctor literals only
        // work with constants or statics"): a record ctor literal's KEYS
        // must be keyword literals. `#user.R{(keyword "a") 42}` is a
        // READ-time error on the real JVM (`LispReader.readRecord` reads
        // the map and requires every key to be a `Keyword` before handing
        // it to the record's `create`) -- and without the check it would
        // quietly SUCCEED once `read-string` evaluates ctor literals
        // (below), since `(keyword "a")` evaluates to `:a`.
        if let FormValue::Map(pairs) = &map_form.value {
            for (k, _) in pairs {
                if !matches!(&k.value, FormValue::Atom(Value::Keyword(_))) {
                    return Err(RjError::reader(
                        format!("Unreadable constructor form: \"#{tag}{{...}}\""),
                        k.span,
                        "a record constructor literal's keys must be keywords",
                    ));
                }
            }
        }
        let full_span = Span { start: hash_start, end: map_form.span.end };
        let head = Form {
            meta: None,
            value: FormValue::Atom(Value::Sym(Symbol {
                ns: Some(Str::from(tag)),
                name: Str::from("create"),
            })),
            span: full_span,
        };
        self.ctor_literal_starts.push((full_span.start, false));
        Ok(Form {
            meta: None,
            value: FormValue::List(vec![head, map_form]),
            span: full_span,
        })
    }

    /// `#uuid "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx"` -> `Value::Uuid`.
    /// Measured against 1.13.0-alpha6 (`clojure.lang.UUID`'s data-reader
    /// fn, effectively `UUID/fromString`): a non-string payload throws
    /// AT READ TIME (`#uuid [1 2]` => `IllegalArgumentException: #uuid
    /// data reader expected string`), and so does malformed string
    /// content (`#uuid "not-a-uuid"` => `IllegalArgumentException:
    /// Invalid UUID string: not-a-uuid`) -- both reproduced here as
    /// reader errors (message text is not conformance-scored, only
    /// occurrence/coarse kind, per CONFORMANCE-GUARANTEE.md's comparison
    /// rules). This retires the old `roundtrip("#uuid [1 2]") ==
    /// "[1 2]"` unit test below (`#uuid` is no longer a blind
    /// passthrough) -- see that test's own updated doc.
    /// `#inst "<rfc3339>"` -> a `HostKind::Date` value. Same shape as
    /// `read_uuid_literal` below (string payload, parse, or a reader
    /// error); real Clojure's own failure here is a `RuntimeException`
    /// reading "Unrecognized date/time syntax", raised from
    /// `clojure.instant/parse-timestamp` at READ time, same as this.
    fn read_inst_literal(&mut self, hash_start: usize) -> Result<Form, RjError> {
        let inner = match self.peek() {
            None => {
                return Err(RjError::reader(
                    "expected a form after tagged literal",
                    Span {
                        start: hash_start,
                        end: self.offset(),
                    },
                    "expected a string to follow #inst",
                ))
            }
            Some(c) => self.read_form_cond_aware(c)?,
        };
        let Some(Value::Str(s)) = (match &inner.value {
            FormValue::Atom(v) => Some(v.clone()),
            _ => None,
        }) else {
            return Err(RjError::reader(
                "#inst data reader expected string",
                Span {
                    start: hash_start,
                    end: inner.span.end,
                },
                "#inst must be followed by a string literal",
            ));
        };
        let Some(millis) = crate::hostclass::parse_inst(&s) else {
            return Err(RjError::reader(
                format!("Unrecognized date/time syntax: {s}"),
                Span {
                    start: hash_start,
                    end: inner.span.end,
                },
                "not a well-formed yyyy[-MM[-ddTHH:mm:ss.fff]][Z|+HH:MM] instant",
            ));
        };
        Ok(Form {
            meta: None,
            value: FormValue::Atom(crate::hostclass::mk_date(millis)),
            span: Span {
                start: hash_start,
                end: inner.span.end,
            },
        })
    }

    fn read_uuid_literal(&mut self, hash_start: usize) -> Result<Form, RjError> {
        let inner = match self.peek() {
            None => {
                return Err(RjError::reader(
                    "expected a form after tagged literal",
                    Span {
                        start: hash_start,
                        end: self.offset(),
                    },
                    "expected a string to follow #uuid",
                ))
            }
            Some(c) => self.read_form_cond_aware(c)?,
        };
        let Some(Value::Str(s)) = (match &inner.value {
            FormValue::Atom(v) => Some(v.clone()),
            _ => None,
        }) else {
            return Err(RjError::reader(
                "#uuid data reader expected string",
                Span {
                    start: hash_start,
                    end: inner.span.end,
                },
                "#uuid must be followed by a string literal",
            ));
        };
        let Some(bits) = Value::parse_uuid(&s) else {
            return Err(RjError::reader(
                format!("Invalid UUID string: {s}"),
                Span {
                    start: hash_start,
                    end: inner.span.end,
                },
                "not a well-formed xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx UUID",
            ));
        };
        Ok(Form {
            meta: None,
            value: FormValue::Atom(Value::Uuid(std::sync::Arc::new(bits))),
            span: Span {
                start: hash_start,
                end: inner.span.end,
            },
        })
    }

    /// `##Inf` -> `Value::Float(f64::INFINITY)`, `##-Inf` -> NEG_INFINITY,
    /// `##NaN` -> NAN. `hash_start` is the offset of the FIRST (already
    /// consumed) `#`; this consumes the second `#` plus the following
    /// token and spans the whole `##token`. Measured Clojure error text
    /// for anything else (`##Infinity` -> `Unknown symbolic value:
    /// ##Infinity`, `##foo` -> `Unknown symbolic value: ##foo`) -- message
    /// text is not conformance-scored (CONFORMANCE-GUARANTEE.md rule 6)
    /// but matching it is free.
    ///
    /// `-##Inf` deliberately does NOT reach here: `-` is not `#`, so it
    /// starts an ordinary symbol token in `read_atom` (`-` is a valid
    /// non-delimiter character and `is_delim` never treats `#` as a
    /// delimiter either, so `read_token` swallows the whole `-##Inf` as
    /// one symbol) -- matching measured Clojure, which reads `-##Inf` as a
    /// symbol, not a number.
    fn read_symbolic_value(&mut self, hash_start: usize) -> Result<Form, RjError> {
        self.advance(); // consume the second '#'
        let (token, _) = self.read_token();
        let end = self.offset();
        let span = Span {
            start: hash_start,
            end,
        };
        let value = match token {
            "Inf" => Value::Float(f64::INFINITY),
            "-Inf" => Value::Float(f64::NEG_INFINITY),
            "NaN" => Value::Float(f64::NAN),
            _ => {
                return Err(RjError::reader(
                    format!("Unknown symbolic value: ##{token}"),
                    span,
                    "unknown symbolic value",
                ))
            }
        };
        Ok(Form {
            meta: None,
            value: FormValue::Atom(value),
            span,
        })
    }

    /// S5 / M3: `^:kw`/`^{...}`/`^sym`/`^"str"` before a form -- reads the
    /// metadata, DESUGARS it to a map form, and ATTACHES it to the form
    /// that follows (`Form::meta`). Before M3 this parsed and discarded.
    ///
    /// # Desugaring (each shorthand measured on 1.13.0-alpha6)
    ///
    /// | source        | metadata map      |
    /// |---------------|-------------------|
    /// | `^:kw x`      | `{:kw true}`      |
    /// | `^{:a 1} x`   | `{:a 1}`          |
    /// | `^String x`   | `{:tag String}`   |
    /// | `^"foo" x`    | `{:tag "foo"}`    |
    ///
    /// Those four are the ONLY legal metadata forms (Clojure's own
    /// `LispReader.MetaReader` throws "Metadata must be Symbol, Keyword,
    /// String or Map" for anything else), which is what makes the
    /// stacking rule below expressible as plain pair concatenation:
    /// every shorthand lands on a `FormValue::Map`, so there is never a
    /// non-map metadata form to combine.
    ///
    /// # Stacking
    ///
    /// `^:a ^:b x` is read here as ONE loop rather than by recursing,
    /// and the collected metadata maps are concatenated INNERMOST-FIRST:
    /// `{:b true, :a true}`, measured -- note the printed order, `:b`
    /// (inner, written second) really does come first. Concatenating in
    /// that order also gets the conflict rule right for free, since a
    /// later pair overwrites an earlier one in both `form_to_value` and
    /// `Interp::eval_form`'s map arms: `^{:a 1} ^{:a 2} x` is `{:a 1}`,
    /// measured -- the OUTER (leftmost) write wins.
    ///
    /// # Metadata on a non-`IMeta` literal
    ///
    /// `^:a 1` / `^:a "s"` / `^:a :kw` are a READ-time error in Clojure
    /// ("Metadata can only be applied to IMetas", measured), not a
    /// runtime one, so they're rejected here. Symbols and the four
    /// collection literals are accepted; a `(f x)` list is accepted too
    /// (whether its RESULT can carry metadata isn't knowable until it
    /// runs -- Clojure defers that case the same way).
    fn read_meta(&mut self) -> Result<Form, RjError> {
        let start = self.offset();
        // Innermost-first accumulation, per the stacking rule above:
        // each trip round the loop peels one MORE-OUTER `^`, so the
        // pairs are pushed outermost-first here and reversed at the end.
        let mut stacked: Vec<Vec<(Form, Form)>> = Vec::new();
        loop {
            self.advance(); // consume '^'
            self.skip_trivia()?;
            let Some(c) = self.peek() else {
                return Err(RjError::reader(
                    "expected metadata after '^'",
                    Span {
                        start,
                        end: self.offset(),
                    },
                    "expected a metadata form to follow this '^'",
                ));
            };
            let meta_form = self.read_form_cond_aware(c)?;
            stacked.push(self.desugar_meta(meta_form)?);
            self.skip_trivia()?;
            match self.peek() {
                Some('^') => continue,
                Some(_) => break,
                None => {
                    return Err(RjError::reader(
                        "expected a form after metadata",
                        Span {
                            start,
                            end: self.offset(),
                        },
                        "expected a form to follow this metadata",
                    ));
                }
            }
        }

        let c2 = self.peek().expect("peeked Some(_) to break the loop above");
        let mut target = self.read_form_cond_aware(c2)?;

        if let FormValue::Atom(v) = &target.value {
            if !matches!(v, Value::Sym(_)) {
                return Err(RjError::reader(
                    "metadata can only be applied to symbols and collections",
                    Span {
                        start,
                        end: target.span.end,
                    },
                    "this literal cannot carry metadata (Clojure: \"Metadata can only be applied to IMetas\")",
                ));
            }
        }

        // Innermost-first (see "Stacking"), and MERGED WITH whatever the
        // target form already carried -- `(quote ^:a ^:b x)` reaches this
        // point with `target.meta == None`, but a form built by an inner
        // reader macro may not, and dropping its metadata here would be a
        // silent loss.
        let mut pairs: Vec<(Form, Form)> = Vec::new();
        if let Some(existing) = target.meta.take() {
            match existing.value {
                FormValue::Map(existing_pairs) => pairs.extend(existing_pairs),
                other => pairs.extend(Self::desugar_meta_value(Form::bare(other, existing.span))),
            }
        }
        for group in stacked.into_iter().rev() {
            pairs.extend(group);
        }

        let meta_span = Span {
            start,
            end: target.span.start,
        };
        target.meta = Some(Box::new(Form::bare(FormValue::Map(pairs), meta_span)));
        Ok(target)
    }

    /// One `^`-metadata form -> the key/value pairs it stands for. See
    /// [`Reader::read_meta`]'s desugaring table; this is where the
    /// "Metadata must be Symbol, Keyword, String or Map" rule is
    /// enforced.
    fn desugar_meta(&self, meta_form: Form) -> Result<Vec<(Form, Form)>, RjError> {
        let legal = match &meta_form.value {
            FormValue::Map(_) => true,
            FormValue::Atom(v) => matches!(v, Value::Sym(_) | Value::Keyword(_) | Value::Str(_)),
            _ => false,
        };
        if !legal {
            return Err(RjError::reader(
                "metadata must be a symbol, keyword, string, or map",
                meta_form.span,
                "only `^:kw`, `^{...}`, `^Tag` and `^\"tag\"` are metadata",
            ));
        }
        Ok(Self::desugar_meta_value(meta_form))
    }

    /// The desugaring itself, split out so [`read_meta`](Reader::read_meta)
    /// can also re-desugar metadata that arrived already attached to the
    /// target form. Assumes [`desugar_meta`](Reader::desugar_meta) already
    /// validated the shape.
    fn desugar_meta_value(meta_form: Form) -> Vec<(Form, Form)> {
        let span = meta_form.span;
        let kw = |name: &str| Form::bare(FormValue::Atom(Value::Keyword(name.into())), span);
        match meta_form.value {
            FormValue::Map(pairs) => pairs,
            // `^:kw` -> `{:kw true}`
            FormValue::Atom(Value::Keyword(k)) => vec![(
                Form::bare(FormValue::Atom(Value::Keyword(k)), span),
                Form::bare(FormValue::Atom(Value::Bool(true)), span),
            )],
            // `^Tag` / `^"tag"` -> `{:tag Tag}` / `{:tag "tag"}`
            other => vec![(kw("tag"), Form::bare(other, span))],
        }
    }

    fn read_set(&mut self, hash_start: usize) -> Result<Form, RjError> {
        let (items, mut span) = self.read_delimited('}', "set literal")?;
        span.start = hash_start;
        // C10: same read-time duplicate check as `read_map` above, same
        // rationale (measured: `(read-string "#{1 2 3 4 1 5}")` throws
        // `IllegalArgumentException: Duplicate key: 1`) -- see that fn's
        // own doc, including the PERF note: a running `HashSet<Value>`
        // instead of the old O(n^2) (each conversion repeated for every
        // earlier element too) pairwise scan. This is the literal that
        // motivated the fix -- `clojure-lsp.common-symbols`'s `clj-syms`/
        // `cljs-syms` sets each hold ~1000 `{:name .. :kind ..}` maps, so
        // the old scan's ~500K redundant `form_to_value` conversions cost
        // 25+ms of this one file's read time alone (measured via
        // `MOVA_LOAD_TRACE=1`, `[load-trace] clojure-lsp.common-symbols`
        // line).
        let mut seen = std::collections::HashSet::with_capacity(items.len());
        for item in &items {
            let val = form_to_value(item);
            if !seen.insert(val.clone()) {
                return Err(RjError::reader(
                    format!("Duplicate key: {}", crate::printer::pr_str(&val)),
                    item.span,
                    "duplicate element in this set literal",
                )
                // W3a: `IllegalArgumentException`, raw -- see the
                // matching note in `read_map` above.
                .with_class(JvmClass::IllegalArgument));
            }
        }
        Ok(Form {
            meta: None,
            value: FormValue::Set(items),
            span,
        })
    }

    fn read_fn_literal(&mut self, hash_start: usize) -> Result<Form, RjError> {
        let (items, paren_span) = self.read_delimited(')', "fn literal")?;
        let body = Form {
            meta: None,
            value: FormValue::List(items),
            span: paren_span,
        };
        let outer_span = Span {
            start: hash_start,
            end: paren_span.end,
        };
        Ok(desugar_fn_literal(body, outer_span))
    }

    fn read_prefixed_form(&mut self, prefix_len: usize, sym_name: &str) -> Result<Form, RjError> {
        let start = self.offset();
        for _ in 0..prefix_len {
            self.advance();
        }
        let prefix_end = self.offset();
        match self.next_form()? {
            None => Err(RjError::reader(
                format!(
                    "expected a form after '{}'",
                    &self.src[start..prefix_end]
                ),
                Span {
                    start,
                    end: prefix_end,
                },
                format!(
                    "expected a form to follow this '{}'",
                    &self.src[start..prefix_end]
                ),
            )),
            Some(inner) => {
                let end = inner.span.end;
                let head = Form {
                    meta: None,
                    value: FormValue::Atom(Value::Sym(Symbol::simple(sym_name))),
                    span: Span {
                        start,
                        end: prefix_end,
                    },
                };
                Ok(Form {
                    meta: None,
                    value: FormValue::List(vec![head, inner]),
                    span: Span { start, end },
                })
            }
        }
    }

    fn read_string(&mut self) -> Result<Form, RjError> {
        let start = self.offset();
        self.advance(); // consume opening quote
        let mut s = String::new();
        loop {
            match self.peek() {
                None => {
                    return Err(RjError::reader(
                        "unclosed string",
                        Span {
                            start,
                            end: start + 1,
                        },
                        "unclosed string, opened here",
                    ))
                }
                Some('"') => {
                    self.bump('"');
                    break;
                }
                Some('\\') => {
                    let esc_start = self.offset();
                    self.bump('\\');
                    match self.advance() {
                        Some('n') => s.push('\n'),
                        Some('t') => s.push('\t'),
                        Some('r') => s.push('\r'),
                        Some('\\') => s.push('\\'),
                        Some('"') => s.push('"'),
                        // Clojure `LispReader` also has these two -- not
                        // exercised by the pre-M1 corpus, but measured and
                        // in scope here alongside `\uXXXX`/octal.
                        Some('b') => s.push('\u{8}'),
                        Some('f') => s.push('\u{c}'),
                        Some('u') => {
                            let hi = self.read_string_hex4(esc_start)?;
                            self.push_unicode_string_escape(hi, esc_start, &mut s)?;
                        }
                        // `\0`-`\377`: 1-3 OCTAL digits, greedy (matches
                        // Java's `LispReader`), range-checked at the end
                        // since a leading `4`-`7` digit is a valid octal
                        // digit even though 3 of them can overflow 0o377
                        // (`\400` = 0o400 = 256 > 255).
                        Some(c) if ('0'..='7').contains(&c) => {
                            let mut val = u32::from(c) - u32::from('0');
                            let mut count = 1;
                            while count < 3 {
                                match self.peek() {
                                    Some(d) if ('0'..='7').contains(&d) => {
                                        val = val * 8 + (u32::from(d) - u32::from('0'));
                                        self.bump(d);
                                        count += 1;
                                    }
                                    _ => break,
                                }
                            }
                            if val > 0o377 {
                                let esc_end = self.offset();
                                return Err(RjError::reader(
                                    "Octal escape sequence must be in range [0, 377].",
                                    Span {
                                        start: esc_start,
                                        end: esc_end,
                                    },
                                    "octal escape out of range",
                                ));
                            }
                            // Safe: val <= 0o377 = 255, always a valid
                            // Latin-1 scalar value.
                            s.push(val as u8 as char);
                        }
                        Some(other) => {
                            let esc_end = self.offset();
                            return Err(RjError::reader(
                                format!("invalid escape '\\{other}'"),
                                Span {
                                    start: esc_start,
                                    end: esc_end,
                                },
                                "invalid escape sequence",
                            ));
                        }
                        None => {
                            return Err(RjError::reader(
                                "unclosed string",
                                Span {
                                    start,
                                    end: start + 1,
                                },
                                "unclosed string, opened here",
                            ))
                        }
                    }
                }
                Some(c) => {
                    s.push(c);
                    self.bump(c);
                }
            }
        }
        let end = self.offset();
        Ok(Form {
            meta: None,
            value: FormValue::Atom(Value::Str(s.into())),
            span: Span { start, end },
        })
    }

    /// Reads exactly 4 hex-digit characters (the fixed width Clojure's
    /// `\uXXXX` string escape requires) starting at the current position
    /// (just past the already-consumed `\u`) and returns their value.
    /// `esc_start` is the offset of the escape's leading backslash, used
    /// only for the error span when fewer than 4 hex digits are present
    /// before a non-hex-digit char or EOF.
    fn read_string_hex4(&mut self, esc_start: usize) -> Result<u32, RjError> {
        let mut digits = String::new();
        for _ in 0..4 {
            match self.peek() {
                Some(c) if c.is_ascii_hexdigit() => {
                    digits.push(c);
                    self.advance();
                }
                _ => break,
            }
        }
        if digits.chars().count() != 4 {
            let end = self.offset();
            return Err(RjError::reader(
                format!("Invalid unicode escape: \\u{digits}"),
                Span {
                    start: esc_start,
                    end,
                },
                "expected exactly 4 hex digits after \\u",
            ));
        }
        // Safe: `digits` was just validated to be exactly 4 ASCII hex
        // digit characters.
        Ok(u32::from_str_radix(&digits, 16).unwrap_or(0))
    }

    /// Pushes the character denoted by a `\uXXXX` string escape whose code
    /// unit is `hi`, onto `s`. Java strings are UTF-16, so a lone
    /// high-surrogate escape is legal Java/Clojure input; Rust `char`/
    /// `String` are UTF-8 and cannot hold a lone surrogate at all. So:
    /// when `hi` is a high surrogate (`0xD800..=0xDBFF`), look ahead for
    /// an immediately following `\uXXXX` escape that yields a low
    /// surrogate (`0xDC00..=0xDFFF`) and combine the pair into the one
    /// `char` they jointly denote (measured: `(read-string "\"😀\"")`
    /// reads as the single character `😀`). A lone/unpaired surrogate --
    /// on either side -- is this crate's own deliberate, narrow divergence
    /// from Java (which accepts it): a reader error here, forced by
    /// `String` being UTF-8, not a silently mangled character.
    fn push_unicode_string_escape(&mut self, hi: u32, esc_start: usize, s: &mut String) -> Result<(), RjError> {
        let lone_surrogate_err = |code: u32, end: usize| {
            RjError::reader(
                format!("Invalid unicode escape: \\u{code:04X}"),
                Span {
                    start: esc_start,
                    end,
                },
                "lone UTF-16 surrogate cannot be a Rust char (String is UTF-8)",
            )
        };
        if (0xD800..=0xDBFF).contains(&hi) {
            if self.peek() == Some('\\') && self.peek_at(1) == Some('u') {
                let lo_esc_start = self.offset();
                self.advance(); // '\'
                self.advance(); // 'u'
                let lo = self.read_string_hex4(lo_esc_start)?;
                if (0xDC00..=0xDFFF).contains(&lo) {
                    let combined = 0x10000 + (hi - 0xD800) * 0x400 + (lo - 0xDC00);
                    match char::from_u32(combined) {
                        Some(c) => s.push(c),
                        // Unreachable: hi/lo are each 16-bit surrogate
                        // halves, so `combined` is always in
                        // 0x10000..=0x10FFFF, a valid supplementary-plane
                        // scalar value.
                        None => return Err(lone_surrogate_err(hi, self.offset())),
                    }
                    return Ok(());
                }
                return Err(lone_surrogate_err(hi, self.offset()));
            }
            let end = self.offset();
            return Err(lone_surrogate_err(hi, end));
        }
        if (0xDC00..=0xDFFF).contains(&hi) {
            let end = self.offset();
            return Err(lone_surrogate_err(hi, end));
        }
        // Safe: `hi` is outside both surrogate ranges (0xD800..=0xDFFF),
        // so it is always a valid Unicode scalar value on its own.
        s.push(char::from_u32(hi).unwrap_or('\u{FFFD}'));
        Ok(())
    }

    /// `#"pattern"`. Unlike `read_string`, backslashes pass through to the
    /// pattern text RAW (Clojure semantics: `#"\d"` is the two-character
    /// pattern `\d`, not a `d` char-escape) -- the lone exception is `\"`,
    /// which yields a literal `"` in the pattern without ending the literal
    /// (so `#"a\"b"` is the pattern `a"b`, matching how `printer.rs`'s
    /// `Value::Regex` arm re-escapes it on the way back out). `hash_start`
    /// is the offset of the `#` (already consumed by `read_hash`); the
    /// returned span covers `#"..."` in full.
    ///
    /// Compiled here, at read time, against the `regex` crate -- patterns
    /// in this codebase arrive in Java (`java.util.regex.Pattern`) syntax,
    /// which `regex` accepts for every construct this corpus uses (no
    /// lookaround, no backreferences); those unsupported constructs simply
    /// fail to compile and surface as this reader error rather than being
    /// silently mistranslated.
    fn read_regex(&mut self, hash_start: usize) -> Result<Form, RjError> {
        let quote_start = self.offset();
        self.advance(); // consume opening quote
        let mut pattern = String::new();
        loop {
            match self.peek() {
                None => {
                    return Err(RjError::reader(
                        "unclosed regex literal",
                        Span {
                            start: hash_start,
                            end: hash_start + 1,
                        },
                        "unclosed regex literal, opened here",
                    ))
                }
                Some('"') => {
                    self.advance();
                    break;
                }
                Some('\\') => {
                    self.advance();
                    match self.peek() {
                        Some('"') => {
                            pattern.push('"');
                            self.advance();
                        }
                        Some(other) => {
                            pattern.push('\\');
                            pattern.push(other);
                            self.advance();
                        }
                        None => {
                            return Err(RjError::reader(
                                "unclosed regex literal",
                                Span {
                                    start: hash_start,
                                    end: hash_start + 1,
                                },
                                "unclosed regex literal, opened here",
                            ))
                        }
                    }
                }
                Some(c) => {
                    pattern.push(c);
                    self.advance();
                }
            }
        }
        let end = self.offset();
        let span = Span {
            start: hash_start,
            end,
        };
        // C4: translate Java-style `\Q...\E` literal-quoting before
        // compiling -- see `builtins::regex::translate_java_regex_quoting`
        // doc comment for the measured edge-case matrix.
        let translated = crate::builtins::regex::translate_java_regex_quoting(&pattern);
        let re = fancy_regex::Regex::new(&translated).map_err(|e| {
            RjError::reader(
                format!("invalid regex pattern: {e}"),
                Span {
                    start: quote_start,
                    end,
                },
                "invalid regex pattern",
            )
        })?;
        Ok(Form {
            meta: None,
            value: FormValue::Atom(Value::Regex(std::sync::Arc::new(re.into()))),
            span,
        })
    }

    fn read_char(&mut self) -> Result<Form, RjError> {
        let start = self.offset();
        self.advance(); // consume backslash
        let first = match self.advance() {
            Some(c) => c,
            None => {
                return Err(RjError::reader(
                    "unexpected EOF in character literal",
                    Span {
                        start,
                        end: start + 1,
                    },
                    "expected a character after this backslash",
                ))
            }
        };
        let ch = if first.is_alphabetic() {
            let mut word = String::new();
            word.push(first);
            while let Some(c) = self.peek() {
                if is_delim(c) {
                    break;
                }
                word.push(c);
                self.advance();
            }
            // Mirrors Clojure's own `CharacterReader`: a single-character
            // word is checked FIRST and short-circuits every named/`u`/`o`
            // interpretation below -- so a lone `\u` (next char is a
            // delimiter, so `word == "u"`) reads as the character `u`
            // itself, not an unterminated unicode escape. This is why the
            // `\uXXXX`/`\oNNN` arms below are reached only for `word.len()
            // > 1`, and can never see an empty digit slice to panic on.
            if word.chars().count() == 1 {
                first
            } else {
                match word.as_str() {
                    "newline" => '\n',
                    "space" => ' ',
                    "tab" => '\t',
                    "return" => '\r',
                    "backspace" => '\u{8}',
                    "formfeed" => '\u{c}',
                    _ if word.starts_with('u') => parse_unicode_char_literal(&word, start, self.offset())?,
                    _ if word.starts_with('o') => parse_octal_char_literal(&word, start, self.offset())?,
                    _ => {
                        let end = self.offset();
                        return Err(RjError::reader(
                            format!("unknown character literal '\\{word}'"),
                            Span { start, end },
                            "unknown named character literal",
                        ));
                    }
                }
            }
        } else {
            first
        };
        let end = self.offset();
        Ok(Form {
            meta: None,
            value: FormValue::Atom(Value::Char(ch)),
            span: Span { start, end },
        })
    }

    fn read_keyword(&mut self) -> Result<Form, RjError> {
        let start = self.offset();
        self.advance(); // consume first ':'
        // C3d: a SECOND ':' (`::foo`, `::alias/foo`) is Clojure's
        // auto-resolved keyword -- read-time-qualified against `*ns*`
        // (`self.ns_ctx`), not a plain literal. `::` itself (two colons,
        // nothing else) is handled by `resolve_auto_keyword` below, same
        // as every other malformed shape -- real Clojure's own "Invalid
        // token: ::" (measured), not this file's ordinary "missing a
        // name" message, which stays reserved for a bare single `:`.
        let auto_resolve = self.peek() == Some(':');
        if auto_resolve {
            self.advance(); // consume second ':'
        }
        let (token, _) = self.read_token();
        let end = self.offset();
        if !auto_resolve && token.is_empty() {
            return Err(RjError::reader(
                "expected a name after ':'",
                Span { start, end },
                "keyword is missing a name",
            ));
        }
        // PERF (reader throughput): the non-auto-resolve case is the
        // overwhelming common one, and `token` is already a borrowed
        // slice of `src` -- constructing straight from it skips an
        // otherwise-wasted `String` allocation that `Keyword::construct`
        // (behind `From<String>`) would immediately discard on every
        // REPEAT of a keyword text (the common case: `:kind`/`:function`-
        // shaped keywords recur thousands of times in a large literal).
        // Only the auto-resolve path genuinely needs an owned buffer
        // (`format!` builds `ns/name`), so it alone keeps the `String`.
        let kw = if auto_resolve {
            crate::keyword::Keyword::construct(&self.resolve_auto_keyword(token, start, end)?)
        } else {
            crate::keyword::Keyword::construct(token)
        };
        Ok(Form {
            meta: None,
            value: FormValue::Atom(Value::Keyword(kw)),
            span: Span { start, end },
        })
    }

    /// `::name` -> `<current-ns>/name`; `::alias/name` -> looks `alias`
    /// up in the current namespace's alias table (`:as` and `:as-alias`
    /// both land there, `ns.rs::add_alias`) and becomes
    /// `<aliased-full-ns>/name`. `token` is everything read after the
    /// second `:` (so for a bare `::`, `token` is empty).
    ///
    /// Anything that doesn't fit that shape -- empty, a `/` at either
    /// end, more than one `/`, or an alias with no entry in the current
    /// ns -- is the JVM's own `"Invalid token: <literal source text>"`
    /// (measured against 1.13.0-alpha6: `::`, `::/foo`, `::foo/`,
    /// `::foo/bar/baz`, and `::nosuch/foo` inside a ns with no `nosuch`
    /// alias all throw that exact shape, using the RAW characters as
    /// typed -- hence `&self.src[start..end]` rather than reassembling
    /// `token` by hand).
    fn resolve_auto_keyword(&self, token: &str, start: usize, end: usize) -> Result<String, RjError> {
        let invalid = || {
            RjError::reader(
                format!("Invalid token: {}", &self.src[start..end]),
                Span { start, end },
                "invalid auto-resolved keyword",
            )
        };
        match token.find('/') {
            None if !token.is_empty() => Ok(format!("{}/{token}", self.ns_ctx.current_ns)),
            Some(i) if i > 0 && i < token.len() - 1 && !token[i + 1..].contains('/') => {
                let alias = &token[..i];
                let name = &token[i + 1..];
                match self.ns_ctx.aliases.get(alias) {
                    Some(full) => Ok(format!("{full}/{name}")),
                    None => Err(invalid()),
                }
            }
            _ => Err(invalid()),
        }
    }

    /// Tokens are contiguous spans of `src` with no character-level
    /// transformation (no escapes, no case-folding), so this just advances
    /// the cursor and hands back a borrowed slice of `src` instead of
    /// building an owned `String` one `char` at a time -- `src`'s `&'a str`
    /// field is `Copy`, so the returned slice's lifetime is `'a` (tied to
    /// the source, not to this `&mut self` borrow).
    fn read_token(&mut self) -> (&'a str, Span) {
        let start = self.offset();
        while let Some(c) = self.peek() {
            if is_delim(c) {
                break;
            }
            self.bump(c);
        }
        let end = self.offset();
        (&self.src[start..end], Span { start, end })
    }

    fn read_atom(&mut self) -> Result<Form, RjError> {
        let (token, span) = self.read_token();
        if looks_like_number_start(token) {
            return match parse_number(token) {
                Ok(v) => Ok(Form {
                    meta: None,
                    value: FormValue::Atom(v),
                    span,
                }),
                // `NumError::Custom`: a specific message this spec's
                // measured ground-truth table quotes verbatim (radix
                // range/shape errors) -- reproduced because it's free
                // (CONFORMANCE-GUARANTEE.md rule 6), not because it's
                // conformance-scored.
                Err(NumError::Custom(msg)) => Err(RjError::reader(msg, span, "invalid number literal")),
                Err(NumError::Generic) => Err(RjError::reader(
                    format!("invalid number literal '{token}'"),
                    span,
                    "invalid number literal",
                )),
            };
        }
        let value = match token {
            "nil" => Value::Nil,
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            _ => Value::Sym(parse_symbol(token)),
        };
        Ok(Form {
            meta: None,
            value: FormValue::Atom(value),
            span,
        })
    }
}

/// `\uXXXX` character literal: `word` is the whole alphabetic run starting
/// with `u` (already known to be longer than 1 char, per `read_char`'s
/// length-1 short-circuit), so `word[1..]` is everything after the `u`.
/// Requires EXACTLY 4 hex digits (measured: `\u12` -> `Invalid unicode
/// character: \u12`, wrong digit count) and rejects a lone UTF-16
/// surrogate (measured: `\uD800` -> `Invalid character constant: \ud800`
/// -- Rust `char` cannot hold one, and Clojure rejects it too, so this is
/// not even a divergence). `start`/`end` bound the whole `\word` literal
/// for the error span.
fn parse_unicode_char_literal(word: &str, start: usize, end: usize) -> Result<char, RjError> {
    let digits = &word[1..];
    let valid = digits.chars().count() == 4 && digits.chars().all(|c| c.is_ascii_hexdigit());
    if !valid {
        return Err(RjError::reader(
            format!("Invalid unicode character: \\{word}"),
            Span { start, end },
            "expected exactly 4 hex digits after \\u",
        ));
    }
    // Safe: `digits` was just validated as exactly 4 ASCII hex digits.
    let code = u32::from_str_radix(digits, 16).unwrap_or(0);
    if (0xD800..=0xDFFF).contains(&code) {
        return Err(RjError::reader(
            format!("Invalid character constant: \\u{code:04x}"),
            Span { start, end },
            "lone UTF-16 surrogate cannot be a Rust char (String is UTF-8)",
        ));
    }
    // Safe: code <= 0xFFFF (4 hex digits) and outside the surrogate range,
    // so it is always a valid Unicode scalar value.
    Ok(char::from_u32(code).unwrap_or('\u{FFFD}'))
}

/// `\oNNN` character literal: 1-3 OCTAL digits after the `o`, value must
/// be `<= 0o377` (measured: `\o101` -> `\A`, `\o377` -> U+00FF, `\o400` ->
/// `Octal escape sequence must be in range [0, 377].`, matching the
/// string-literal octal escape's identical range check in `read_string`).
fn parse_octal_char_literal(word: &str, start: usize, end: usize) -> Result<char, RjError> {
    let digits = &word[1..];
    let count = digits.chars().count();
    let valid = (1..=3).contains(&count) && digits.chars().all(|c| ('0'..='7').contains(&c));
    if !valid {
        return Err(RjError::reader(
            format!("Invalid octal character: \\{word}"),
            Span { start, end },
            "expected 1 to 3 octal digits after \\o",
        ));
    }
    // Safe: `digits` was just validated as 1-3 ASCII octal digits.
    let value = u32::from_str_radix(digits, 8).unwrap_or(0);
    if value > 0o377 {
        return Err(RjError::reader(
            "Octal escape sequence must be in range [0, 377].",
            Span { start, end },
            "octal character escape out of range",
        ));
    }
    // Safe: value <= 0o377 = 255, always a valid Latin-1 scalar value.
    Ok(value as u8 as char)
}

/// `pub(crate)`: `crate::embed::Engine::get`/`register_fn` reuse this exact
/// ns/name split (rather than reimplementing "split on the first `/`,
/// except the literal `/` symbol") so a host embedding mova parses
/// `"db/lookup"` identically to how the reader would.
pub(crate) fn parse_symbol(token: &str) -> Symbol {
    if token == "/" {
        return Symbol::simple("/");
    }
    if let Some(idx) = token.find('/') {
        if idx > 0 && idx + 1 < token.len() {
            return Symbol {
                ns: Some(token[..idx].into()),
                name: token[idx + 1..].into(),
            };
        }
    }
    Symbol::simple(token)
}

/// edn/fast: `pub(crate)` so the fast-path scanner can reuse this exact
/// "does this token even start a number" check instead of reimplementing
/// it (see `edn_fast`'s module doc).
pub(crate) fn looks_like_number_start(s: &str) -> bool {
    let mut cs = s.chars();
    match cs.next() {
        Some(c) if c.is_ascii_digit() => true,
        Some('+') | Some('-') => matches!(cs.next(), Some(c) if c.is_ascii_digit()),
        _ => false,
    }
}

/// A malformed number token's outcome: `Generic` gets the caller's uniform
/// "invalid number literal '<token>'" wording; `Custom` carries an exact
/// message this spec's measured-against-real-Clojure ground-truth table
/// quotes verbatim (radix literals' `Radix out of range` / `Invalid
/// number: ...`). Reproducing `Custom`'s text is free
/// (CONFORMANCE-GUARANTEE.md rule 6: message text is never
/// conformance-scored) but matches real Clojure anyway.
pub(crate) enum NumError {
    Generic,
    Custom(String),
}

/// Parse `digits` in `radix`, apply `neg`, and collapse to `Value::Int`
/// when the result fits an `i64` -- otherwise promote to `Value::BigInt`.
/// Shared by all four integer productions below (decimal/hex/octal/radix)
/// so out-of-i64-range overflow behaves identically everywhere (measured:
/// `9223372036854775808` -> `9223372036854775808N`,
/// `0xFFFFFFFFFFFFFFFFF` -> `295147905179352825855N`,
/// `36rZZZZZZZZZZZZZZZZ` -> `7958661109946400884391935N` -- all promote,
/// none of them read-time error the way they used to before this Value
/// variant existed).
fn int_or_bigint(digits: &str, radix: u32, neg: bool) -> Result<Value, NumError> {
    // Fast path: a plain base-10 run of up to 18 ASCII digits always fits
    // an i64 (10^18 < 2^63 - 1), so it can be accumulated by hand into a
    // u64 with no possibility of overflow and no heap-allocating
    // `num_bigint` detour -- this is the overwhelmingly common shape (any
    // ordinary integer literal in real source). 19+ digits, any other
    // radix, an empty `digits`, or a non-ASCII-digit byte all fall through
    // UNCHANGED to the existing BigInt path below, which already handles
    // overflow-promotion and the `-9223372036854775808` (i64::MIN) edge
    // correctly -- this fast path deliberately does not try to also cover
    // 19-digit numbers (some fit i64, some don't; correctness beats the
    // last few percent).
    if radix == 10 && !digits.is_empty() && digits.len() <= 18 {
        let db = digits.as_bytes();
        if db.iter().all(u8::is_ascii_digit) {
            let mut v: u64 = 0;
            for &b in db {
                v = v * 10 + u64::from(b - b'0');
            }
            return Ok(Value::Int(if neg { -(v as i64) } else { v as i64 }));
        }
    }
    let bi = crate::bignum::parse_bigint_radix(digits, radix, neg).ok_or(NumError::Generic)?;
    match bi.to_i64_exact() {
        Some(i) => Ok(Value::Int(i)),
        None => Ok(Value::BigInt(std::sync::Arc::new(bi))),
    }
}

/// Same as [`int_or_bigint`] but for the `N`-suffixed forms (`7N`,
/// `0xFFN`, `010N`), which are ALWAYS `Value::BigInt` -- measured: `7N`
/// stays BigInt even though it trivially fits an `i64`, unlike plain
/// overflow promotion above which only promotes when it must.
fn always_bigint(digits: &str, radix: u32, neg: bool) -> Result<Value, NumError> {
    let bi = crate::bignum::parse_bigint_radix(digits, radix, neg).ok_or(NumError::Generic)?;
    Ok(Value::BigInt(std::sync::Arc::new(bi)))
}

/// `1/3`-shaped ratio literal (Clojure's `ratioPat`): numerator
/// `[-+]?[0-9]+`, denominator `[0-9]+` (no sign, no suffix on either
/// side). `parse_number` calls this whenever `token` contains a `/` --
/// none of the int/hex/octal/radix/float productions ever contain one, so
/// a `/` unambiguously means "ratio-shaped or a read error", never a
/// fallback to another production (measured: `1/-2` and `3/4N`/`3N/4`
/// are read-time errors, not e.g. symbols).
fn parse_ratio(token: &str) -> Result<Value, NumError> {
    let invalid = || NumError::Custom(format!("Invalid number: {token}"));
    let mut parts = token.split('/');
    let num_part = parts.next().unwrap_or("");
    let den_part = parts.next().ok_or_else(invalid)?;
    if parts.next().is_some() {
        return Err(invalid()); // more than one '/'
    }
    let nb = num_part.as_bytes();
    let n_sign_len = usize::from(!nb.is_empty() && (nb[0] == b'-' || nb[0] == b'+'));
    if nb.len() == n_sign_len || !nb[n_sign_len..].iter().all(u8::is_ascii_digit) {
        return Err(invalid());
    }
    let db = den_part.as_bytes();
    if db.is_empty() || !db.iter().all(u8::is_ascii_digit) {
        return Err(invalid());
    }
    let num = num_bigint::BigInt::parse_bytes(num_part.as_bytes(), 10).ok_or_else(invalid)?;
    let den = num_bigint::BigInt::parse_bytes(den_part.as_bytes(), 10).ok_or_else(invalid)?;
    match crate::bignum::RatioVal::reduce(num, den) {
        // Clojure: `1/0` throws `ArithmeticException: Divide by zero` at
        // READ time, not lazily when the value is later used.
        Err(crate::bignum::DivideByZero) => Err(NumError::Custom("Divide by zero".to_string())),
        // `4/2` -> Long `2`, `0/5` -> Long `0` (den==1 after reduction);
        // still routes through the i64-fit check since a huge numerator
        // can reduce down to something that no longer fits (unlikely but
        // this keeps the collapse rule identical to the plain-int sites).
        Ok(Reduced::Int(bi)) => match bi.to_i64_exact() {
            Some(i) => Ok(Value::Int(i)),
            None => Ok(Value::BigInt(std::sync::Arc::new(bi))),
        },
        Ok(Reduced::Ratio(r)) => Ok(Value::Ratio(std::sync::Arc::new(r))),
    }
}

/// Strictly validates + parses a decimal integer or float token, a
/// `0x`/`0X`-prefixed hex integer, a leading-zero octal integer (Java and
/// Clojure both read `0700` as 448; a lone `0` stays decimal `0` since it
/// has no second digit to make it octal; an `8`/`9` digit after a leading
/// zero is invalid, same as Clojure's `Invalid number: 0800`), a
/// `<radix>r<digits>` radix literal (`2r1010` -> 10, `16rFF` -> 255, radix
/// 2..=36, digits case-insensitive), a `1/3`-shaped ratio literal, or an
/// `N`/`M`-suffixed BigInt/BigDecimal literal. Returns `Err` if `token`
/// merely looks like it starts a number but isn't a well-formed one.
/// edn/fast: `pub(crate)` so the fast-path scanner can hand it a bounded
/// token and reuse ALL of its number-construction semantics (bigint
/// promotion, radix/ratio/BigDecimal productions, the exact `Err` shape) --
/// see `edn_fast`'s module doc for why this is a REUSE, never a
/// reimplementation.
pub(crate) fn parse_number(token: &str) -> Result<Value, NumError> {
    let bytes = token.as_bytes();
    let neg = bytes[0] == b'-';
    let sign_len = usize::from(bytes[0] == b'+' || bytes[0] == b'-');
    let rest = &token[sign_len..];

    // `1/3`: none of the productions below ever contain a `/`, so this is
    // unambiguous and checked first.
    if token.contains('/') {
        return parse_ratio(token);
    }

    // `<radix>r<digits>`: sign goes BEFORE the radix (`-2r1010` -> -10);
    // a sign after `r` is invalid (`2r-1010` is not a radix digit in any
    // base <= 36, so it falls out of the digit-validity check below as
    // measured Clojure's `Invalid number: 2r-1010`). Deliberately does
    // NOT strip a trailing `N` -- radix digits are `[0-9A-Za-z]+`, which
    // EATS an `N` as just another digit character, so `2r1010N` fails the
    // digit-validity check below for base 2 (measured: reader error) same
    // as real Clojure's `NumberFormatException`, with no special-casing
    // needed.
    if let Some(r_idx) = rest.find(['r', 'R']) {
        if r_idx > 0 && rest.as_bytes()[..r_idx].iter().all(u8::is_ascii_digit) {
            let radix_digits = &rest[..r_idx];
            let digits = &rest[r_idx + 1..];
            // A radix string can't overflow: any run of ASCII digits too
            // long to be a real radix (<=36) also fails the range check,
            // and `Radix out of range` is the measured message either way.
            let radix: u32 = radix_digits.parse().unwrap_or(u32::MAX);
            if !(2..=36).contains(&radix) {
                return Err(NumError::Custom("Radix out of range".to_string()));
            }
            if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
                return Err(NumError::Custom(format!("Invalid number: {token}")));
            }
            return int_or_bigint(digits, radix, neg);
        }
    }

    if let Some(hex) = rest.strip_prefix("0x").or_else(|| rest.strip_prefix("0X")) {
        // `N` works on hex (measured: `0xFFN` -> `255N`), unlike radix.
        let (hex, has_n) = match hex.strip_suffix('N') {
            Some(h) => (h, true),
            None => (hex, false),
        };
        if hex.is_empty() || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(NumError::Generic);
        }
        return if has_n {
            always_bigint(hex, 16, neg)
        } else {
            int_or_bigint(hex, 16, neg)
        };
    }
    // A leading-zero token with a second digit and nothing else (no `.`/`e`,
    // which would make it a float instead -- floats keep leading zeros'
    // ordinary decimal meaning) is octal.
    if rest.len() > 1 && rest.as_bytes()[0] == b'0' {
        // `N` works on octal too (measured: `010N` -> `8N`). Stripped
        // before the octal-shape check so `010N`'s body (`010`) still
        // reads as octal digits; a token that merely ENDS in `N` but
        // isn't actually octal underneath (e.g. `"0N"`, `body.len() <=
        // 1`) intentionally falls through to the decimal/float
        // productions below instead of erroring here.
        let (body, has_n) = match rest.strip_suffix('N') {
            Some(b) => (b, true),
            None => (rest, false),
        };
        if body.len() > 1 && body.bytes().all(|b| b.is_ascii_digit()) {
            if !body.bytes().all(|b| b.is_ascii_digit() && b < b'8') {
                return Err(NumError::Generic); // an 8 or 9 digit: Clojure's "Invalid number"
            }
            return if has_n {
                always_bigint(body, 8, neg)
            } else {
                int_or_bigint(body, 8, neg)
            };
        }
    }

    // `M` suffix: BigDecimal. Checked here -- after ratio/radix/hex/octal,
    // which all handle (or reject) their own shapes first -- so it only
    // ever applies to the plain decimal int/float bodies `BigDecVal::
    // parse` understands (`7M`, `1.5M`, `1.50M`, `1e3M`, `-0.0M`,
    // `1.23e-4M`, `1e309M`: measured, and note the LAST one -- the body
    // never becomes an `f64`, so it never overflows to `Infinity` the way
    // the un-suffixed float path below would).
    if let Some(body) = token.strip_suffix('M') {
        return match crate::bignum::BigDecVal::parse(body) {
            Some(bd) => Ok(Value::BigDec(std::sync::Arc::new(bd))),
            None => Err(NumError::Generic),
        };
    }

    let mut i = sign_len;
    let digits_start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == digits_start {
        return Err(NumError::Generic);
    }
    // `N` suffix on a bare decimal integer: ALWAYS BigInt, even when the
    // value trivially fits an `i64` (measured: `7N` stays BigInt, never
    // collapsing to `Value::Int` the way plain overflow-promotion does
    // below). Checked right after the digit run and before the `.`/`e`
    // float productions since `N` is only valid on an INTEGER token --
    // Clojure has no `1.5N`.
    if i < bytes.len() && bytes[i] == b'N' && i + 1 == bytes.len() {
        return always_bigint(&token[digits_start..i], 10, neg);
    }
    let mut is_float = false;
    if i < bytes.len() && bytes[i] == b'.' {
        is_float = true;
        i += 1;
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i == start {
            return Err(NumError::Generic);
        }
    }
    if i < bytes.len() && (bytes[i] == b'e' || bytes[i] == b'E') {
        is_float = true;
        i += 1;
        if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
            i += 1;
        }
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i == start {
            return Err(NumError::Generic);
        }
    }
    if i != bytes.len() {
        return Err(NumError::Generic);
    }
    if is_float {
        token.parse::<f64>().map(Value::Float).map_err(|_| NumError::Generic)
    } else {
        int_or_bigint(&token[digits_start..i], 10, neg)
    }
}

/// Desugars `#(...)` into `(fn* [%1 .. %n & %&] (...))`. `%` is rewritten
/// to `%1` in the body so a single generated param name covers both
/// spellings. The `fn*` head is Clojure's own -- see the comment on the
/// head construction below for the three things that depend on it.
fn desugar_fn_literal(body: Form, outer_span: Span) -> Form {
    let mut max_num: u32 = 0;
    let mut has_bare = false;
    let mut has_amp = false;
    collect_percent_symbols(&body, &mut max_num, &mut has_bare, &mut has_amp);
    let effective_max = max_num.max(u32::from(has_bare));
    let rewritten_body = rewrite_bare_percent(&body);

    let mut params: Vec<Form> = (1..=effective_max)
        .map(|n| Form {
            meta: None,
            value: FormValue::Atom(Value::Sym(Symbol::simple(format!("%{n}")))),
            span: outer_span,
        })
        .collect();
    if has_amp {
        params.push(Form {
            meta: None,
            value: FormValue::Atom(Value::Sym(Symbol::simple("&"))),
            span: outer_span,
        });
        params.push(Form {
            meta: None,
            value: FormValue::Atom(Value::Sym(Symbol::simple("%&"))),
            span: outer_span,
        });
    }

    // SPEC-W6b: the head is `fn*`, not `fn` -- what Clojure's own reader
    // emits (`LispReader.FnReader` builds `(fn* [args] body)`), and what
    // any code that INSPECTS a `#()` form is written against. Three
    // things depend on it, all measured:
    //   * `tests/clojure-suite/vendor/spec.clj`'s `coll-form` deftest
    //     asserts `(s/form (s/coll-of int? :gen #(gen/return [1 2])))`
    //     ends in `(fn* [] (gen/return [1 2]))`;
    //   * `clojure.spec.alpha`'s own `unfn` recognises a reader-generated
    //     fn by exactly this head, which is why the port needed MOVA-PATCH
    //     P9 to also accept `fn` -- that patch is now retired;
    //   * `fn*` is a TRUE special form, so `eval_list` routes it through
    //     `eval_special` before any namespace mapping is consulted, which
    //     means a `#()` inside a namespace that shadows `clojure.core/fn`
    //     (spec.alpha shadows plenty) can no longer be diverted.
    // The PARAMETER names stay `%1`/`%2`/`%&` rather than the JVM's
    // `p1__42#` gensyms -- deliberate, and the one remaining difference:
    // they are stable, which several of this repo's own printed
    // comparisons rely on (see tests/spec-smoke/RUNNING.md).
    let fn_sym = Form {
        meta: None,
        value: FormValue::Atom(Value::Sym(Symbol::simple("fn*"))),
        span: outer_span,
    };
    let params_form = Form {
        meta: None,
        value: FormValue::Vector(params),
        span: outer_span,
    };
    Form {
        meta: None,
        value: FormValue::List(vec![fn_sym, params_form, rewritten_body]),
        span: outer_span,
    }
}

fn collect_percent_symbols(form: &Form, max_num: &mut u32, has_bare: &mut bool, has_amp: &mut bool) {
    match &form.value {
        FormValue::Atom(Value::Sym(sym)) if sym.ns.is_none() => {
            let name = sym.name.as_ref();
            if name == "%" {
                *has_bare = true;
            } else if name == "%&" {
                *has_amp = true;
            } else if let Some(rest) = name.strip_prefix('%') {
                if let Ok(n) = rest.parse::<u32>() {
                    *max_num = (*max_num).max(n);
                }
            }
        }
        FormValue::Atom(_) => {}
        FormValue::List(items) | FormValue::Vector(items) | FormValue::Set(items) => {
            for it in items {
                collect_percent_symbols(it, max_num, has_bare, has_amp);
            }
        }
        FormValue::Map(pairs) => {
            for (k, v) in pairs {
                collect_percent_symbols(k, max_num, has_bare, has_amp);
                collect_percent_symbols(v, max_num, has_bare, has_amp);
            }
        }
    }
}

/// W4B-WARNINGS: every branch below now carries `form.meta.clone()`
/// through the rewrite instead of hardcoding `meta: None` -- measured
/// against the oracle (`(meta (nth (nth (read-string "#(inc ^long %)")
/// 2) 1))` -- real Clojure DOES retain a `^long`-on-`%` occurrence's tag
/// on the substituted `%1` symbol, since `#(...)` is a literal AST
/// rewrite (symbol substitution only), never a fresh read that could
/// drop metadata written elsewhere in the body. This was a real,
/// pre-existing gap (not something this task's feature introduced): ANY
/// metadata anywhere inside a `#(...)` body -- not just on `%` itself --
/// was silently dropped before this fix. It is what `numbers.clj`'s
/// `warn-on-boxed` deftest's `(check-warn-on-box false (#(inc ^long %)
/// 2))` row needs: `crate::reflwarn`'s analysis reads a `^long` tag
/// directly off the FORM at each expression's occurrence (matching how
/// `Interp::eval_form_with_meta`'s own C11 fix already treats expression-
/// level type hints as inert-but-present metadata, see that fn's doc),
/// so a dropped tag here read as "unhinted" and produced a false-positive
/// boxed-math warning.
fn rewrite_bare_percent(form: &Form) -> Form {
    match &form.value {
        FormValue::Atom(Value::Sym(sym)) if sym.ns.is_none() && sym.name.as_ref() == "%" => Form {
            meta: form.meta.clone(),
            value: FormValue::Atom(Value::Sym(Symbol::simple("%1"))),
            span: form.span,
        },
        FormValue::Atom(_) => form.clone(),
        FormValue::List(items) => Form {
            meta: form.meta.clone(),
            value: FormValue::List(items.iter().map(rewrite_bare_percent).collect()),
            span: form.span,
        },
        FormValue::Vector(items) => Form {
            meta: form.meta.clone(),
            value: FormValue::Vector(items.iter().map(rewrite_bare_percent).collect()),
            span: form.span,
        },
        FormValue::Set(items) => Form {
            meta: form.meta.clone(),
            value: FormValue::Set(items.iter().map(rewrite_bare_percent).collect()),
            span: form.span,
        },
        FormValue::Map(pairs) => Form {
            meta: form.meta.clone(),
            value: FormValue::Map(
                pairs
                    .iter()
                    .map(|(k, v)| (rewrite_bare_percent(k), rewrite_bare_percent(v)))
                    .collect(),
            ),
            span: form.span,
        },
    }
}

/// The `clojure.lang.ReaderConditional` of `{:read-cond :preserve}`: a deftype
/// shaped value holding the form (a list) and the splicing flag. It prints as
/// `#?(...)` / `#?@(...)` and evaluates to itself.
fn reader_cond_tdef() -> std::sync::Arc<crate::types::TypeDef> {
    use std::sync::{Arc, OnceLock};
    static TDEF: OnceLock<Arc<crate::types::TypeDef>> = OnceLock::new();
    TDEF.get_or_init(|| {
        Arc::new(crate::types::TypeDef {
            name: Str::from("clojure.lang.ReaderConditional"),
            basis: vec![Str::from("form"), Str::from("splicing?")],
            is_record: false,
            interfaces: Vec::new(),
            field_tags: Vec::new(),
            mutable: Vec::new(),
            methods: Default::default(),
            protocols: Vec::new(),
        })
    })
    .clone()
}

pub fn reader_cond_value(form: Value, splicing: bool) -> Value {
    use crate::value::{PMap, PVec};
    let mut fields = PVec::new();
    fields.push_back(form);
    fields.push_back(Value::Bool(splicing));
    Value::Inst(std::sync::Arc::new(crate::types::InstVal {
        tdef: reader_cond_tdef(),
        data: PMap::new(),
        fields: std::sync::Mutex::new(fields),
        meta: None,
    }))
}

/// `(form, splicing?)` of a reader-conditional object.
pub fn reader_cond_parts(v: &Value) -> Option<(Value, bool)> {
    if let Value::Inst(inst) = v {
        if std::sync::Arc::ptr_eq(&inst.tdef, &reader_cond_tdef()) {
            let f = crate::sync::lock_mutex(&inst.fields);
            return Some((f.get(0).cloned().unwrap_or(Value::Nil), matches!(f.get(1), Some(Value::Bool(true)))));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::printer::pr_str;

    fn roundtrip(src: &str) -> String {
        let forms = read_all(src).unwrap_or_else(|e| panic!("reader error on {src:?}: {e:?}"));
        assert_eq!(forms.len(), 1, "expected exactly one form in {src:?}");
        pr_str(&form_to_value(&forms[0]))
    }

    #[test]
    fn round_trip_battery() {
        let cases: &[(&str, &str)] = &[
            ("nil", "nil"),
            ("true", "true"),
            ("false", "false"),
            ("42", "42"),
            ("-42", "-42"),
            ("3.14", "3.14"),
            ("-0.5", "-0.5"),
            // Measured JDK 21: (pr-str 1e10) => "1.0E10" (S12 Java float
            // printing; the old expectation here was mova's pre-S12 bug).
            ("1e10", "1.0E10"),
            ("\"hello\"", "\"hello\""),
            ("\"a\\nb\\t\\\"c\\\"\"", "\"a\\nb\\t\\\"c\\\"\""),
            ("\\a", "\\a"),
            ("\\newline", "\\newline"),
            ("\\space", "\\space"),
            ("\\tab", "\\tab"),
            (":foo", ":foo"),
            (":ns/foo", ":ns/foo"),
            ("foo", "foo"),
            ("ns/foo", "ns/foo"),
            ("/", "/"),
            ("+", "+"),
            ("->", "->"),
            ("(1 2 3)", "(1 2 3)"),
            ("()", "()"),
            ("[1 2 3]", "[1 2 3]"),
            ("[]", "[]"),
            ("{:a 1}", "{:a 1}"),
            ("{}", "{}"),
            ("#{1 2 3}", "#{1 2 3}"),
            ("#{}", "#{}"),
            ("'x", "(quote x)"),
            ("`x", "(quasiquote x)"),
            ("~x", "(unquote x)"),
            ("~@x", "(unquote-splicing x)"),
            ("@x", "(deref x)"),
            ("#(+ % 1)", "(fn* [%1] (+ %1 1))"),
            ("#(+ %1 %2)", "(fn* [%1 %2] (+ %1 %2))"),
            ("#(apply + %&)", "(fn* [& %&] (apply + %&))"),
            ("#(vector)", "(fn* [] (vector))"),
            ("#_1 2", "2"),
            (";; comment\n42", "42"),
            ("(1 ,, 2)", "(1 2)"),
        ];
        for (src, expected) in cases {
            assert_eq!(&roundtrip(src), expected, "source: {src:?}");
        }
    }

    /// Reads exactly one form from `src` with `ns_ctx.current_ns = ns` and
    /// `ns_ctx.aliases = aliases` (C3d test helper -- mirrors
    /// `Interp::reader_ns_context`'s shape without needing a real
    /// `Interp`).
    fn read_one_ns(src: &str, ns: &str, aliases: &[(&str, &str)]) -> Result<Form, RjError> {
        let mut r = Reader::new(src);
        r.set_ns_ctx(NsContext {
            current_ns: Str::from(ns),
            aliases: aliases.iter().map(|(a, f)| (Str::from(*a), Str::from(*f))).collect(),
        });
        r.next_form()?.ok_or_else(|| panic!("no form read from {src:?}"))
    }

    #[test]
    fn auto_resolved_keyword_qualifies_into_current_ns() {
        // Oracle-measured (1.13.0-alpha6, `(ns my.test.ns) (namespace
        // ::foo)` => `"my.test.ns"`, `(name ::foo)` => `"foo"`): `::foo`
        // reads as ONE flat `Value::Keyword("my.test.ns/foo")`, not a
        // keyword whose name still carries a stray leading `:`.
        let form = read_one_ns("::foo", "my.test.ns", &[]).unwrap();
        assert!(
            matches!(&form.value, FormValue::Atom(Value::Keyword(k)) if k.as_ref() == "my.test.ns/foo"),
            "expected :my.test.ns/foo, got {form:?}"
        );
        // Default context (no real `Interp`): same as a fresh REPL's
        // `*ns*`, `user`.
        assert_eq!(roundtrip("::foo"), ":user/foo");
    }

    #[test]
    fn auto_resolved_alias_keyword_expands_through_the_alias_table() {
        // Oracle-measured: `(require '[clojure.string :as s]) ::s/x` =>
        // `:clojure.string/x` -- both `:as` and `:as-alias` land in the
        // same table (`ns.rs::add_alias`), so this helper doesn't need to
        // distinguish them.
        let form = read_one_ns("::s/x", "my.test.ns", &[("s", "clojure.string")]).unwrap();
        assert!(
            matches!(&form.value, FormValue::Atom(Value::Keyword(k)) if k.as_ref() == "clojure.string/x"),
            "expected :clojure.string/x, got {form:?}"
        );
    }

    #[test]
    fn auto_resolved_keyword_unknown_alias_is_invalid_token() {
        // Oracle-measured: `(read-string "::nosuch/thing")` inside a ns
        // with no `nosuch` alias throws `"Invalid token: ::nosuch/thing"`
        // -- the literal source text, not a reconstructed message.
        let err = read_one_ns("::nosuch/thing", "my.test.ns", &[]).unwrap_err();
        assert_eq!(err.kind, crate::error::ErrorKind::Reader);
        assert_eq!(err.message, "Invalid token: ::nosuch/thing");
    }

    #[test]
    fn auto_resolved_keyword_malformed_shapes_are_invalid_tokens() {
        // Oracle-measured (1.13.0-alpha6): a bare `::`, a leading or
        // trailing `/`, and more than one `/` are ALL "Invalid token:
        // <text>", the same shape as the unknown-alias case above.
        for src in ["::", "::/foo", "::foo/", "::foo/bar/baz"] {
            let err = read_one_ns(src, "my.test.ns", &[]).unwrap_err();
            assert_eq!(err.kind, crate::error::ErrorKind::Reader, "source: {src:?}");
            assert_eq!(err.message, format!("Invalid token: {src}"), "source: {src:?}");
        }
    }

    #[test]
    fn auto_resolved_keyword_printed_form_does_not_abbreviate() {
        // The printer never special-cases an auto-resolved keyword's
        // ORIGIN -- once read, `::foo` is indistinguishable from
        // `:my.test.ns/foo` typed out by hand, so it round-trips through
        // `pr-str` as the fully-qualified form (measured: real Clojure's
        // printer has no `::`-abbreviation mode either).
        let form = read_one_ns("::foo", "my.test.ns", &[]).unwrap();
        assert_eq!(pr_str(&form_to_value(&form)), ":my.test.ns/foo");
    }

    #[test]
    fn metadata_is_parsed_and_discarded() {
        assert_eq!(roundtrip("^:private x"), "x");
        assert_eq!(roundtrip("^{:a 1} x"), "x");
        assert_eq!(roundtrip("^sym x"), "x");
        assert_eq!(roundtrip("^\"str\" x"), "x");
        // Stacked metadata.
        assert_eq!(roundtrip("^:a ^:b x"), "x");
        assert_eq!(roundtrip("(def ^:private x 1)"), "(def x 1)");
    }

    #[test]
    fn hex_literals_parse_as_integers() {
        assert_eq!(roundtrip("0x1e1e1e"), "1973790");
        assert_eq!(roundtrip("0X1A"), "26");
        assert_eq!(roundtrip("-0x10"), "-16");
        assert_eq!(roundtrip("0x0"), "0");
    }

    #[test]
    fn hex_literals_reject_malformed_tokens() {
        assert!(read_all("0x").is_err(), "bare 0x has no digits");
        assert!(read_all("0xZZ").is_err(), "not hex digits");
    }

    #[test]
    fn octal_literals_parse_leading_zero_integers() {
        assert_eq!(roundtrip("0700"), "448");
        assert_eq!(roundtrip("00"), "0");
        // A lone `0` has no second digit, so it stays decimal (same value
        // either way, but exercises the "no octal branch" path).
        assert_eq!(roundtrip("0"), "0");
        assert_eq!(roundtrip("-010"), "-8");
        // A leading zero followed by `.`/`e` is a FLOAT, not octal: floats
        // keep leading zeros' ordinary decimal meaning.
        assert_eq!(roundtrip("0.5"), "0.5");
        assert_eq!(roundtrip("00.5"), "0.5");
    }

    #[test]
    fn octal_literals_reject_8_and_9_digits() {
        // Clojure: `(read-string "0800")` => "Invalid number: 0800".
        let err = read_all("0800").unwrap_err();
        assert_eq!(err.kind, crate::error::ErrorKind::Reader);
        assert!(read_all("0009").is_err());
    }

    /// SPEC-B-bignum-wiring.md §2/§6: every literal row of the measured
    /// ground-truth table that produces a VALUE (as opposed to a read
    /// error, covered separately below). `roundtrip` prints via `pr_str`
    /// (readable mode), matching the table's `[pr-str, class]` shape.
    #[test]
    fn bignum_literal_battery() {
        let cases: &[(&str, &str)] = &[
            // Ratio: reduces, collapses to Long when den==1, sign on the
            // numerator only.
            ("1/3", "1/3"),
            ("4/2", "2"),
            ("0/5", "0"),
            ("-6/4", "-3/2"),
            ("+6/4", "3/2"),
            // Numerator too big for i64 -- stays a genuine Ratio with a
            // BigInt numerator.
            ("92233720368547758080/3", "92233720368547758080/3"),
            // Same big numerator, but this one reduces down to something
            // too big for i64 too -- collapses to BigInt, not Long.
            ("92233720368547758080/2", "46116860184273879040N"),
            // `N` suffix: ALWAYS BigInt, even though `7`/`-7` trivially
            // fit an i64 -- never collapses to `Value::Int`.
            ("7N", "7N"),
            ("-7N", "-7N"),
            // `N` works on hex and octal, not just plain decimal.
            ("0xFFN", "255N"),
            ("010N", "8N"),
            // Out-of-i64-range integers (decimal/hex/radix) promote to
            // BigInt instead of read-time erroring.
            ("9223372036854775808", "9223372036854775808N"),
            ("-9223372036854775809", "-9223372036854775809N"),
            // i64::MIN fits exactly -- must stay `Value::Int`, not
            // promote, even though it's parsed via the same BigInt ->
            // negate -> i64-fit-check path as the overflow cases above.
            ("-9223372036854775808", "-9223372036854775808"),
            ("0xFFFFFFFFFFFFFFFFF", "295147905179352825855N"),
            ("36rZZZZZZZZZZZZZZZZ", "7958661109946400884391935N"),
            // `M` suffix: BigDecimal, scale preserved verbatim (never
            // canonicalized on print, even though `1.5M`/`1.50M` are `=`).
            ("7M", "7M"),
            ("1.5M", "1.5M"),
            ("1.50M", "1.50M"),
            // BigInt has no -0; falls out of BigDecVal::parse automatically.
            ("-0.0M", "0.0M"),
            ("1e3M", "1E+3M"),
            ("1.23e-4M", "0.000123M"),
            // No float overflow -- the body never becomes an f64, unlike
            // the un-suffixed `1e309` row right below.
            ("1e309M", "1E+309M"),
            // Already-existing float path, unchanged by this spec.
            ("1e309", "##Inf"),
        ];
        for (src, expected) in cases {
            assert_eq!(&roundtrip(src), expected, "source: {src:?}");
        }
    }

    /// edn/fast: `int_or_bigint`'s new <=18-digit manual-accumulation fast
    /// path (radix 10 only) must produce byte-for-byte the same
    /// `Value::Int`/`Value::BigInt` split as the pre-existing
    /// `parse_bigint_radix` -> `to_i64_exact` path it short-circuits.
    /// Exercised both directly (so the exact boundary -- 18 vs 19 digits --
    /// is pinned regardless of which `parse_number` production a token
    /// happens to reach) and through `roundtrip` (so the end-to-end reader
    /// path, including the sign and leading-zero handling done by the
    /// digit-scanning loop above this function, is covered too).
    #[test]
    fn int_or_bigint_fast_path_matches_slow_path() {
        // Zero and single digits -- shortest possible fast-path input.
        assert!(matches!(int_or_bigint("0", 10, false), Ok(Value::Int(0))));
        assert!(matches!(int_or_bigint("7", 10, false), Ok(Value::Int(7))));
        assert!(matches!(int_or_bigint("7", 10, true), Ok(Value::Int(-7))));
        // Leading zeros: manual accumulation must still yield 7, not treat
        // the leading zeros as octal or otherwise misparse.
        assert!(matches!(int_or_bigint("007", 10, false), Ok(Value::Int(7))));
        assert!(matches!(int_or_bigint("007", 10, true), Ok(Value::Int(-7))));
        // 18 nines: the longest digit run the fast path accepts, still
        // comfortably fits i64 (999999999999999999 < i64::MAX).
        assert!(matches!(
            int_or_bigint("999999999999999999", 10, false),
            Ok(Value::Int(999_999_999_999_999_999))
        ));
        assert!(matches!(
            int_or_bigint("999999999999999999", 10, true),
            Ok(Value::Int(-999_999_999_999_999_999))
        ));
        // i64::MAX itself is 19 digits -- falls to the slow BigInt path,
        // which must still collapse it back down to `Value::Int` (fits
        // exactly, no promotion).
        assert!(matches!(
            int_or_bigint("9223372036854775807", 10, false),
            Ok(Value::Int(i64::MAX))
        ));
        // i64::MAX + 1 (19 digits): slow path, promotes to BigInt.
        assert!(matches!(int_or_bigint("9223372036854775808", 10, false), Ok(Value::BigInt(_))));
        // i64::MIN's magnitude (19 digits) is the one case where negating
        // AFTER parsing the unsigned magnitude matters -- slow path,
        // unchanged by this spec, still collapses to `Value::Int`.
        assert!(matches!(
            int_or_bigint("9223372036854775808", 10, true),
            Ok(Value::Int(i64::MIN))
        ));
        // Same boundary end-to-end through the real reader, not just the
        // helper directly.
        assert_eq!(roundtrip("9223372036854775807"), "9223372036854775807");
        assert_eq!(roundtrip("-9223372036854775808"), "-9223372036854775808");
        assert_eq!(roundtrip("9223372036854775808"), "9223372036854775808N");
        assert_eq!(roundtrip("999999999999999999"), "999999999999999999");
        assert_eq!(roundtrip("-999999999999999999"), "-999999999999999999");
        assert_eq!(roundtrip("007"), "7");
    }

    /// SPEC-B-bignum-wiring.md §2/§6: the table's read-time ERROR rows.
    #[test]
    fn bignum_literal_errors() {
        // `1/0`: Clojure throws `ArithmeticException: Divide by zero` at
        // READ time.
        let err = read_all("1/0").unwrap_err();
        assert!(
            format!("{err:?}").contains("Divide by zero"),
            "expected a 'Divide by zero' reader error, got {err:?}"
        );
        // Denominator must be `[0-9]+` -- no sign.
        assert!(read_all("1/-2").is_err(), "1/-2: denominator may not carry a sign");
        // Neither side of a ratio may carry the `N`/`M` suffixes.
        assert!(read_all("3/4N").is_err(), "3/4N: denominator may not carry N");
        assert!(read_all("3N/4").is_err(), "3N/4: numerator may not carry N");
        // Radix digits `[0-9A-Za-z]+` EAT a trailing `N` as just another
        // (invalid, for base 2) digit character -- `N` suffix is NOT
        // supported on radix literals, so this is a NumberFormat-style
        // reader error, not `1010N` read as `Value::BigInt`.
        assert!(read_all("2r1010N").is_err(), "2r1010N: N is not a valid base-2 digit");
    }

    #[test]
    fn apostrophe_is_a_symbol_constituent_after_the_first_character() {
        assert_eq!(roundtrip("cam'"), "cam'");
        assert_eq!(roundtrip("x''"), "x''");
        assert_eq!(roundtrip("it's-fine"), "it's-fine");
        // Leading `'` is still the quote reader macro, unaffected.
        assert_eq!(roundtrip("'x"), "(quote x)");
        assert_eq!(roundtrip("'cam'"), "(quote cam')");
    }

    #[test]
    fn tagged_literal_passes_the_form_through_unchanged() {
        assert_eq!(roundtrip("#cpp 300"), "300");
    }

    /// SPEC-W1 task 4: `#inst` is no longer a blind pass-through -- see
    /// `read_inst_literal`'s doc. It reads into mova's `HostKind::Date`
    /// and prints back as real Clojure's own `print-method` for
    /// `java.util.Date` spells it, so the literal round-trips (it used
    /// to degrade into the bare payload STRING, `"2020"`).
    #[test]
    fn inst_literal_reads_as_a_date_and_round_trips() {
        assert_eq!(roundtrip("#inst \"2020\""), "#inst \"2020-01-01T00:00:00.000-00:00\"");
        assert_eq!(
            roundtrip("#inst \"2020-01-01\""),
            "#inst \"2020-01-01T00:00:00.000-00:00\""
        );
        assert_eq!(
            roundtrip("#inst \"1970-01-01T00:00:00.100-00:00\""),
            "#inst \"1970-01-01T00:00:00.100-00:00\""
        );
        // Offsets are applied, not ignored.
        assert_eq!(
            roundtrip("#inst \"2020-01-01T05:30:00+05:30\""),
            "#inst \"2020-01-01T00:00:00.000-00:00\""
        );
        assert_eq!(
            roundtrip("#inst \"1969-12-31T23:59:59.999Z\""),
            "#inst \"1969-12-31T23:59:59.999-00:00\""
        );
    }

    // S6 (assert/namespace/uuid batch): `#uuid` is no longer a blind
    // pass-through -- see `read_uuid_literal`'s doc. This retires the
    // old `roundtrip("#uuid [1 2]") == "[1 2]"` case from the test just
    // above (a non-string payload now throws, matching real Clojure's
    // own `IllegalArgumentException: #uuid data reader expected string`
    // at read time).
    #[test]
    fn uuid_literal_reads_as_a_real_uuid_value() {
        assert_eq!(
            roundtrip("#uuid \"550e8400-e29b-41d4-a716-446655440000\""),
            "#uuid \"550e8400-e29b-41d4-a716-446655440000\""
        );
        // Mixed-case input normalizes to lowercase on print (measured).
        assert_eq!(
            roundtrip("#uuid \"550E8400-E29B-41D4-A716-446655440000\""),
            "#uuid \"550e8400-e29b-41d4-a716-446655440000\""
        );
        assert!(read_all("#uuid [1 2]").is_err(), "non-string payload must throw, like the real reader");
        assert!(read_all("#uuid \"not-a-uuid\"").is_err(), "malformed UUID text must throw, like the real reader");
    }

    #[test]
    fn var_quote_reads_as_a_var_special_form() {
        assert_eq!(roundtrip("#'x"), "(var x)");
        assert_eq!(roundtrip("#'ns/x"), "(var ns/x)");
    }

    #[test]
    fn read_one_stops_after_the_first_form() {
        let form = read_one("(+ 1 2) garbage)").unwrap().expect("a form");
        assert_eq!(pr_str(&form_to_value(&form)), "(+ 1 2)");
        assert!(read_one("").unwrap().is_none());
        assert!(read_one("   ").unwrap().is_none());
    }

    #[test]
    fn discard_drops_element_from_list() {
        let forms = read_all("(1 #_2 3)").unwrap();
        assert_eq!(pr_str(&form_to_value(&forms[0])), "(1 3)");
    }

    #[test]
    fn multiple_top_level_forms() {
        let forms = read_all("1 2 3").unwrap();
        assert_eq!(forms.len(), 3);
    }

    #[test]
    fn unclosed_list_reports_reader_error_with_span() {
        let err = read_all("(1 2").unwrap_err();
        assert_eq!(err.kind, crate::error::ErrorKind::Reader);
        let span = err.span.expect("span present");
        assert_eq!(span, Span { start: 0, end: 1 });
        assert_eq!(err.label.as_deref(), Some("unclosed list, opened here"));
    }

    #[test]
    fn bad_escape_reports_reader_error() {
        let err = read_all("\"a\\qb\"").unwrap_err();
        assert_eq!(err.kind, crate::error::ErrorKind::Reader);
        assert!(err.span.is_some());
    }

    #[test]
    fn stray_unquote_splicing_at_eof_is_a_reader_error() {
        let err = read_all("~@").unwrap_err();
        assert_eq!(err.kind, crate::error::ErrorKind::Reader);
        assert_eq!(err.span, Some(Span { start: 0, end: 2 }));
    }

    // S5 reader conditionals: every case below was cross-checked against
    // the pinned oracle (`(read-string {:read-cond :allow} "...")` on real
    // Clojure 1.13.0-alpha6, see the session's own transcripts) before
    // being encoded as a mova test.

    fn allow_cond_roundtrip(src: &str) -> String {
        let form = read_one_allow_cond(src)
            .unwrap_or_else(|e| panic!("reader error on {src:?}: {e:?}"))
            .unwrap_or_else(|| panic!("no form read from {src:?}"));
        pr_str(&form_to_value(&form))
    }

    #[test]
    fn read_cond_gated_off_by_default() {
        let err = read_one("#?(:clj 1)").unwrap_err();
        assert_eq!(err.kind, crate::error::ErrorKind::Reader);
        assert_eq!(err.message, "Conditional read not allowed");
    }

    #[test]
    fn read_cond_allowed_picks_clj_branch() {
        assert_eq!(allow_cond_roundtrip("#?(:clj 1 :cljs 2)"), "1");
        assert_eq!(allow_cond_roundtrip("#?(:cljs 2 :clj 1)"), "1");
    }

    #[test]
    fn read_cond_default_matches_when_clj_absent() {
        assert_eq!(allow_cond_roundtrip("#?(:cljs 1 :default 99)"), "99");
    }

    #[test]
    fn read_cond_no_match_vanishes_in_a_vector() {
        assert_eq!(allow_cond_roundtrip("[1 #?(:cljs 2) 3]"), "[1 3]");
        assert_eq!(allow_cond_roundtrip("[1 #?() 3]"), "[1 3]");
    }

    #[test]
    fn read_cond_no_match_at_top_level_is_eof() {
        // Matches real Clojure: `(read-string {:read-cond :allow}
        // "#?(:cljs 1)")` throws "EOF while reading" -- there was never a
        // form there at all.
        assert!(read_one_allow_cond("#?(:cljs 1)").unwrap().is_none());
    }

    #[test]
    fn read_cond_splice_into_vector_map_set() {
        assert_eq!(allow_cond_roundtrip("[1 #?@(:clj [10 20]) 9]"), "[1 10 20 9]");
        assert_eq!(
            allow_cond_roundtrip("{:a 1 #?@(:clj [:b 2 :c 3])}"),
            "{:a 1, :b 2, :c 3}"
        );
        assert_eq!(allow_cond_roundtrip("[1 #?@(:cljs [2 3]) 4]"), "[1 4]");
    }

    #[test]
    fn read_cond_splicing_not_allowed_at_top_level() {
        let err = read_one_allow_cond("#?@(:clj [1 2])").unwrap_err();
        assert_eq!(err.kind, crate::error::ErrorKind::Reader);
        assert_eq!(
            err.message,
            "Reader conditional splicing not allowed at the top level"
        );
    }

    #[test]
    fn read_cond_splice_of_non_list_errors() {
        let err = read_one_allow_cond("[1 #?@(:clj 10) 9]").unwrap_err();
        assert_eq!(err.kind, crate::error::ErrorKind::Reader);
    }

    #[test]
    fn read_cond_nested_inside_matching_branch_expands() {
        assert_eq!(
            allow_cond_roundtrip("[1 #?(:clj [#?(:clj 10 :cljs 20)]) 2]"),
            "[1 [10] 2]"
        );
    }

    #[test]
    fn read_cond_nested_inside_skipped_branch_is_still_read_syntactically() {
        // The whole outer `#?` doesn't match (we're not `:cljs`), so this
        // vanishes -- but the nested `#?` inside the discarded branch must
        // still be READ (paren-matched) without erroring the whole parse.
        assert_eq!(
            allow_cond_roundtrip("[1 #?(:cljs #?(:clj (whatever) :clj 2)) 3]"),
            "[1 3]"
        );
    }

    #[test]
    fn read_cond_post_match_trailing_content_not_validated() {
        // Measured: once `:clj` matches, an odd trailing feature with no
        // paired form, and non-keyword trailing tokens, are both silently
        // skipped rather than erroring.
        assert_eq!(allow_cond_roundtrip("#?(:clj 1 :cljs)"), "1");
        assert_eq!(allow_cond_roundtrip("[1 #?(:clj 1 6 7) 9]"), "[1 1 9]");
    }

    #[test]
    fn read_cond_pre_match_feature_must_be_a_keyword() {
        let err = read_one_allow_cond("[1 #?(:cljs (bad 2 :default 5) 6]").unwrap_err();
        assert_eq!(err.kind, crate::error::ErrorKind::Reader);
    }

    #[test]
    fn value_to_form_synthesizes_call_site_span() {
        let span = Span { start: 5, end: 9 };
        let v = Value::List(crate::pvec![Value::Int(1), Value::Int(2)]);
        let form = value_to_form(&v, span);
        assert_eq!(form.span, span);
        if let FormValue::List(items) = &form.value {
            assert!(items.iter().all(|f| f.span == span));
        } else {
            panic!("expected list form");
        }
    }
}
