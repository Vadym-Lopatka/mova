//! `pr_str` (readable) and `display_str` (human) printers, Clojure
//! conventions: `(1 2)`, `[1 2]`, `{:a 1, :b 2}`, `#{1}`.

use crate::value::{Keyword, Symbol, Value};

thread_local! {
    /// S5/M3: whether the CURRENT thread is inside a print that should
    /// render `IObj` metadata (`^{:a 1} [1 2]` instead of `[1 2]`).
    ///
    /// # Why a thread-local instead of a parameter
    ///
    /// `*print-meta*` is a dynamic var, so its value belongs to the print
    /// *call*, not to any one `write_value` frame -- and `write_value`
    /// recurses through elements, map values and (see the `Value::Meta`
    /// arm) the metadata map itself, so threading a flag down by hand
    /// would mean touching every recursive call in this file. More
    /// importantly `pr_str`/`display_str` are `&Value -> String` free
    /// functions with no `Interp` in scope: they're called from `Debug
    /// for Value`, from error messages, and from a dozen builtins, none
    /// of which can look a var up. Reading the var ONCE at the print
    /// builtin's boundary (`builtins::strings::realize_all_pr_str`) and
    /// parking the answer here keeps every one of those callers
    /// unchanged and correctly gets the innermost `binding` frame *on
    /// this thread*, which is exactly the scoping a Clojure dynamic var
    /// has.
    ///
    /// Defaults to `false` (Clojure's own default for `*print-meta*`),
    /// so every internal `pr_str` -- diagnostics, `Debug`, corpus output
    /// -- is unaffected unless a script explicitly asks otherwise.
    static PRINT_META: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    // C14 (protocols): `*print-dup*`/`*verbose-defrecords*` -- same
    // thread-local-scope shape as `PRINT_META` above, for the same reason
    // (`write_value` is a free fn with no `Interp`). `defrecord-printing`'s
    // deftest is the sole consumer: `*print-dup*` true (and
    // `*verbose-defrecords*` false) switches a RECORD's print from
    // `#ns.R{:a 1, :b 2}` to the constructor-literal-readable `#ns.R[1,
    // 2]`; `*verbose-defrecords*` true forces the map form back on even
    // under `*print-dup*` (measured).
    static PRINT_DUP: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static VERBOSE_DEFRECORDS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// D5: `*print-namespace-maps*` -- same thread-local-scope shape as
    /// `PRINT_META`/`PRINT_DUP` above, read at the same one boundary
    /// (`builtins::strings::realize_all_pr_str`).
    ///
    /// When on, a map whose keys are ALL qualified idents sharing ONE
    /// namespace prints in the prefix form `#:user{:a 1, :b 2}` instead
    /// of `{:user/a 1, :user/b 2}`. Default `false` -- Clojure's own
    /// `clojure.core` default (the REPL binds it true; a plain `pr-str`
    /// does not), so nothing mova prints changes unless a program asks.
    ///
    /// This is the `pr`-side twin of `clojure.core/lift-ns`, which
    /// `core.mova` defines for the vendored `clojure.pprint` to call --
    /// same rule, and `printer.clj`'s `print-ns-maps` deftest asserts
    /// both spellings agree row for row, which is what pins them
    /// together.
    static PRINT_NAMESPACE_MAPS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// W3b: `*print-length*`/`*print-level*` -- same thread-local-scope
    /// shape and same one boundary (`builtins::strings::realize_all_pr_str`/
    /// `realize_all_print_family_str`) as `PRINT_META`/`PRINT_DUP` above.
    ///
    /// `None` (the default -- unbound or `nil`, Clojure's own default for
    /// both vars) means unlimited: no collection is ever truncated or
    /// replaced with `#`. `Some(n)` mirrors real Clojure's
    /// `RT.printLength`/print-level machinery, measured against
    /// `printer.clj`'s `print-length-*`/`print-level-*` deftests and a
    /// hand-run oracle matrix (maps, sets, nested combinations, infinite
    /// seqs):
    ///
    /// - `*print-length*` caps the number of ELEMENTS a collection prints
    ///   at every nesting level independently (the SAME bound applies at
    ///   every depth, not decremented per level) -- once truncated, a
    ///   trailing `...` is appended after the collection's own item
    ///   separator (`", ..."` for a map's `, `-joined entries, `" ..."`
    ///   for every space-joined collection). An EMPTY collection never
    ///   grows an `...` even at length 0 (measured: `(binding
    ///   [*print-length* 0] (print-str ()))` is `"()"`, not `"(...)"`).
    /// - `*print-level*` caps NESTING depth: the value passed to
    ///   `pr_str`/`display_str` itself is depth 0, and a collection whose
    ///   depth is `>= *print-level*` prints as a bare `#` instead of
    ///   expanding at all (its own brackets/braces included) -- measured:
    ///   `(binding [*print-level* 0] (print-str '(0 (1))))` is `"#"`.
    ///   Only collections are ever elided this way; a scalar element (a
    ///   symbol, a number, ...) always prints in full regardless of depth.
    static PRINT_LENGTH: std::cell::Cell<Option<i64>> = const { std::cell::Cell::new(None) };
    static PRINT_LEVEL: std::cell::Cell<Option<i64>> = const { std::cell::Cell::new(None) };
}

/// [`PRINT_LENGTH`]/[`PRINT_LEVEL`]'s RAII restore guard -- same shape as
/// [`PrintMetaGuard`].
pub struct PrintLimitsGuard(Option<i64>, Option<i64>);

impl Drop for PrintLimitsGuard {
    fn drop(&mut self) {
        PRINT_LENGTH.with(|c| c.set(self.0));
        PRINT_LEVEL.with(|c| c.set(self.1));
    }
}

/// Turn `*print-length*`/`*print-level*` on/off for the rest of this
/// scope. See [`PRINT_LENGTH`]/[`PRINT_LEVEL`].
#[must_use = "the guard restores the previous *print-length*/*print-level* when dropped"]
pub fn print_limits_scope(length: Option<i64>, level: Option<i64>) -> PrintLimitsGuard {
    PrintLimitsGuard(PRINT_LENGTH.with(|c| c.replace(length)), PRINT_LEVEL.with(|c| c.replace(level)))
}

/// `true` iff a collection at `depth` (0 = the value handed to
/// `pr_str`/`display_str` itself) should print as a bare `#` under the
/// current `*print-level*`. See [`PRINT_LEVEL`]'s doc for the measured
/// depth-counting rule.
fn level_exceeded(depth: usize) -> bool {
    PRINT_LEVEL.with(|c| c.get()).is_some_and(|lvl| depth as i64 >= lvl)
}

/// Writes up to `*print-length*` items (or all of them, if unset) by
/// calling `write_item(index, out)` for each kept index, joined by `sep`,
/// then appends `sep` + `"..."` when the current `*print-length*`
/// truncated anything -- see [`PRINT_LENGTH`]'s doc. `count` is the
/// collection's total element count (already known by every caller, which
/// all iterate an already-collected/known-length sequence).
fn write_truncated<F: FnMut(usize, &mut String)>(count: usize, sep: &str, out: &mut String, mut write_item: F) {
    let limit = PRINT_LENGTH.with(|c| c.get()).map(|n| n.max(0) as usize);
    let take_n = limit.map_or(count, |lim| lim.min(count));
    for i in 0..take_n {
        if i > 0 {
            out.push_str(sep);
        }
        write_item(i, out);
    }
    if let Some(lim) = limit {
        if count > lim {
            if take_n > 0 {
                out.push_str(sep);
            }
            out.push_str("...");
        }
    }
}

/// RAII restore for [`PRINT_META`]. Restoring the PREVIOUS value (rather
/// than resetting to `false`) is what keeps nested prints -- a
/// `print-method`-ish path, or simply a `pr-str` called from inside a
/// value being `pr-str`ed -- from clearing an outer `binding`'s answer.
pub struct PrintMetaGuard(bool);

impl Drop for PrintMetaGuard {
    fn drop(&mut self) {
        PRINT_META.with(|c| c.set(self.0));
    }
}

/// Turn metadata printing on/off for the rest of this scope. See
/// [`PRINT_META`].
#[must_use = "the guard restores the previous *print-meta* when dropped"]
pub fn print_meta_scope(on: bool) -> PrintMetaGuard {
    PrintMetaGuard(PRINT_META.with(|c| c.replace(on)))
}

/// [`PRINT_DUP`]'s RAII restore guard -- same shape as [`PrintMetaGuard`].
pub struct PrintDupGuard(bool, bool);

impl Drop for PrintDupGuard {
    fn drop(&mut self) {
        PRINT_DUP.with(|c| c.set(self.0));
        VERBOSE_DEFRECORDS.with(|c| c.set(self.1));
    }
}

/// Turn `*print-dup*`/`*verbose-defrecords*` on/off for the rest of this
/// scope. See [`PRINT_DUP`].
#[must_use = "the guard restores the previous *print-dup*/*verbose-defrecords* when dropped"]
pub fn print_dup_scope(dup: bool, verbose: bool) -> PrintDupGuard {
    PrintDupGuard(PRINT_DUP.with(|c| c.replace(dup)), VERBOSE_DEFRECORDS.with(|c| c.replace(verbose)))
}

/// D5: [`PRINT_NAMESPACE_MAPS`]' RAII restore guard -- same shape as
/// [`PrintMetaGuard`].
pub struct PrintNsMapsGuard(bool);

impl Drop for PrintNsMapsGuard {
    fn drop(&mut self) {
        PRINT_NAMESPACE_MAPS.with(|c| c.set(self.0));
    }
}

/// Turn `*print-namespace-maps*` on/off for the rest of this scope. See
/// [`PRINT_NAMESPACE_MAPS`].
#[must_use = "the guard restores the previous *print-namespace-maps* when dropped"]
pub fn print_ns_maps_scope(on: bool) -> PrintNsMapsGuard {
    PrintNsMapsGuard(PRINT_NAMESPACE_MAPS.with(|c| c.replace(on)))
}

/// D5: the one namespace every key of `pairs` shares, when
/// `*print-namespace-maps*` is on and the map is liftable -- i.e. every
/// key is a QUALIFIED ident (keyword or symbol) and they all name the
/// same namespace. `None` means print the map the ordinary way.
///
/// This is `clojure.core/lift-ns`'s rule exactly (see `core.mova`'s port
/// of it, which the vendored `clojure.pprint` calls): an empty map is not
/// liftable (there is no namespace to lift), a single unqualified key
/// disqualifies the whole map, and mixed namespaces disqualify it too.
/// Keyword and symbol keys may be MIXED as long as the namespace agrees
/// -- measured, `printer.clj`'s `print-ns-maps` has exactly that row:
/// `{:user/a 1, 'user/b 2}` prints `#:user{:a 1, b 2}`.
fn lifted_ns(pairs: &[(&Value, &Value)]) -> Option<String> {
    if !PRINT_NAMESPACE_MAPS.with(|c| c.get()) || pairs.is_empty() {
        return None;
    }
    let mut found: Option<String> = None;
    for (k, _) in pairs {
        let ns = match k {
            Value::Keyword(s) => crate::builtins::strings::symbol_from_str(s).ns,
            Value::Sym(s) => s.ns.clone(),
            _ => return None,
        }?;
        match &found {
            None => found = Some(ns.to_string()),
            Some(prev) if prev == ns.as_ref() => {}
            Some(_) => return None,
        }
    }
    found
}

/// D5: one key of a lifted `#:ns{...}` map, with its namespace stripped
/// -- `clojure.core`'s own `strip-ns`, kind-preserving (a keyword stays a
/// keyword, a symbol stays a symbol).
fn write_stripped_key(k: &Value, readable: bool, depth: usize, out: &mut String) {
    match k {
        Value::Keyword(s) => {
            let bare = crate::builtins::strings::symbol_from_str(s).name;
            write_value(&Value::Keyword(Keyword::from(bare)), readable, depth, out);
        }
        Value::Sym(s) => {
            write_value(&Value::Sym(crate::value::Symbol::simple(s.name.clone())), readable, depth, out);
        }
        other => write_value(other, readable, depth, out),
    }
}

// ==================== W4-PRINTER: print-throwable (`#error {...}`) ====================
//
// `printer.clj`'s `print-throwable` deftest asserts, for 4 templated
// exceptions (a plain `Exception.`, a `Throwable.`/`Exception.` cause
// chain, an `ex-info`, and a MIXED chain interleaving host exceptions and
// `ex-info`s), that `(Throwable->map e)` structurally EQUALS
// `(read-string (pr-str e))` under a `*data-readers*` binding that maps
// the `error` tag to `identity`. That equality holds regardless of what
// `:trace` actually contains (mova has no real stack trace, see
// `core.mova`'s `Throwable->map` doc) -- it only requires `pr_str`'s
// `#error {...}` map to be BUILT THE SAME WAY `Throwable->map` itself
// builds its map, so the two sides never drift. `throwable_to_map` below
// is a direct Rust port of `core.mova`'s own `Throwable->map` (see that
// fn's doc for the unified host-Throwable/`ex-info` walk this mirrors) --
// ported rather than called back into, because `write_value` has no
// `Interp` to evaluate Clojure code with.
//
// W-ERR (field2) DISCLOSED DRIFT: `core.mova`'s `Throwable->map` gained a
// third link shape this wave -- a plain mova-internal-error map
// (`{:type :error/.., :message ..}`, what a typed `catch` binds `e` to
// for an internal error), reached MID-CHAIN when a user wraps a caught
// internal error in `ex-info` (`(ex-info "outer" {} caught-e)`) and calls
// `Throwable->map` on the result. This Rust port's walk was intentionally
// NOT extended to match (out of scope for this wave: printer.rs's top-
// level `is_throwable_shaped` guard already excluded this shape at the
// entry point and had no crash to fix there, so widening the walk here
// would be adding new behavior, not closing a totality hole). The
// "identical walk" claim above therefore no longer holds for that ONE
// mixed shape: `pr_str`'s `#error {...}` on such a value mislabels the
// internal-error link as `clojure.lang.ExceptionInfo` (this fn's
// `throwable_type_symbol` default arm) and drops its `:message` entirely,
// where `core.mova`'s `Throwable->map` on the identical value now
// correctly carries the message through. Every OTHER shape (a bare host
// `Throwable` chain, a bare `ex-info` chain, or a mixed host/`ex-info`
// chain with no internal-error link) is unaffected -- the two sides still
// agree exactly there.

/// `mova`'s exception `Value::Inst`s (`hostclass::mk_exception`) all carry
/// `"java.lang.Throwable"` as either their own class name or an ancestor
/// interface -- the one predicate needed to recognize "this Inst is
/// exception-shaped at all".
pub(crate) fn inst_is_throwable(inst: &crate::types::InstVal) -> bool {
    inst.tdef.name.as_ref() == "java.lang.Throwable"
        || inst.tdef.interfaces.iter().any(|s| s.as_ref() == "java.lang.Throwable")
}

/// Looks a basis method NAME up in `tdef.basis` fresh, rather than
/// assuming a fixed field index -- `mk_arity_exception`'s basis is
/// `["actual" "getMessage" "getCause"]`, a DIFFERENT layout from the
/// plain `["getMessage" "getCause"]` every other `mk_exception` build
/// uses, so position 0/1 isn't safe to hardcode.
fn inst_basis_field(inst: &crate::types::InstVal, name: &str) -> Option<Value> {
    let idx = inst.tdef.basis.iter().position(|b| b.as_ref() == name)?;
    crate::sync::lock_mutex(&inst.fields).get_owned(idx)
}

/// Whether `v` is throwable-SHAPED at all: a host exception `Value::Inst`
/// (`inst_is_throwable`), or an `ex-info` map -- identified the same way
/// `types::builtin_class_name`'s `:ex/message` special case and
/// `eval::special_forms::thrown_value_class_chain` both already do.
fn is_throwable_shaped(v: &Value) -> bool {
    match v {
        Value::Inst(inst) => inst_is_throwable(inst),
        Value::Map(m) => m.get(&Value::Keyword("ex/message".into())).is_some(),
        _ => false,
    }
}

/// `.getMessage`, polymorphic across both throwable shapes -- `Nil`
/// (absent) collapses to `None` either way, matching `core.mova`'s own
/// `(when msg ...)` gate.
fn throwable_message(v: &Value) -> Option<Value> {
    match v {
        Value::Inst(inst) => inst_basis_field(inst, "getMessage"),
        Value::Map(m) => m.get(&Value::Keyword("ex/message".into())).cloned(),
        _ => None,
    }
    .filter(|m| !matches!(m, Value::Nil))
}

/// `.getCause`, polymorphic across both throwable shapes -- the walk's
/// loop terminator.
fn throwable_cause(v: &Value) -> Option<Value> {
    match v {
        Value::Inst(inst) => inst_basis_field(inst, "getCause"),
        Value::Map(m) => m.get(&Value::Keyword("ex/cause".into())).cloned(),
        _ => None,
    }
    .filter(|c| !matches!(c, Value::Nil))
}

/// `ex-data`: only an `ex-info` map ever carries structured data (a plain
/// host `Throwable` genuinely has none on the real JVM either).
fn throwable_data(v: &Value) -> Option<Value> {
    match v {
        Value::Map(m) => m.get(&Value::Keyword("ex/data".into())).cloned(),
        _ => None,
    }
}

/// The class NAME, as `Throwable->map`'s `:type` needs it -- a SYMBOL
/// (`(symbol (str (class cur)))`, per `core.mova`'s own doc), NOT a
/// `Value::Class` object. Measured against real Clojure's own
/// `core_print.clj` source: `(symbol (.getName (class t)))`. Load-bearing
/// for `print-throwable`'s round-trip equality -- `read-string` on
/// `pr-str`'s printed `#error {...}` can only ever produce a plain symbol
/// for a bare `java.lang.Exception` token (the reader never resolves
/// symbols to classes), so storing an actual `Value::Class` here would
/// make the two sides never `=`.
fn throwable_type_symbol(v: &Value) -> Value {
    let name: crate::value::Str = match v {
        Value::Inst(inst) => inst.tdef.name.clone(),
        // Every OTHER throwable-shaped value is the `ex-info` map arm --
        // `is_throwable_shaped`'s only other `true` case.
        _ => "clojure.lang.ExceptionInfo".into(),
    };
    Value::Sym(Symbol::simple(name))
}

/// Builds EXACTLY the map `core.mova`'s own `Throwable->map` returns for
/// `v` (see this section's module doc, and that fn's own doc for the
/// walk this mirrors). `None` when `v` isn't throwable-shaped.
pub(crate) fn throwable_to_map(v: &Value) -> Option<Value> {
    if !is_throwable_shaped(v) {
        return None;
    }
    let mut via: Vec<Value> = Vec::new();
    let mut cur = v.clone();
    let (final_msg, final_data) = loop {
        let msg = throwable_message(&cur);
        let data = throwable_data(&cur);
        let mut entry: Vec<(Value, Value)> =
            vec![(Value::Keyword("type".into()), throwable_type_symbol(&cur))];
        if let Some(m) = &msg {
            entry.push((Value::Keyword("message".into()), m.clone()));
        }
        if let Some(d) = &data {
            entry.push((Value::Keyword("data".into()), d.clone()));
        }
        via.push(Value::Map(entry.into_iter().collect()));
        match throwable_cause(&cur) {
            Some(next) => cur = next,
            None => break (msg, data),
        }
    };
    let mut top: Vec<(Value, Value)> = vec![
        (Value::Keyword("via".into()), Value::Vector(via.into_iter().collect())),
        (Value::Keyword("trace".into()), Value::Vector(std::iter::empty().collect())),
    ];
    if let Some(m) = final_msg {
        top.push((Value::Keyword("cause".into()), m));
    }
    if let Some(d) = final_data {
        top.push((Value::Keyword("data".into()), d));
    }
    Some(Value::Map(top.into_iter().collect()))
}

/// Readable form: strings are quoted/escaped, chars print as `\a`.
pub fn pr_str(v: &Value) -> String {
    let mut out = String::new();
    write_value(v, true, 0, &mut out);
    out
}

/// Human form: strings and chars print raw. Collection ELEMENTS still
/// print readably (see `DISPLAY_PROMOTES_ELEMENTS`'s doc) -- this is
/// `str`'s shape, matching Clojure's `RT.printString`/`.toString()` path,
/// whose ambient `*print-readably*` stays `true` since nothing here binds
/// it. For the print/println family's genuinely non-readable-all-the-way-
/// down form, see `print_family_str`.
pub fn display_str(v: &Value) -> String {
    // e2: `(str string-writer)` is its text (JVM StringWriter.toString).
    if let Value::Atom(a) = v {
        if a.string_writer {
            return display_str(&crate::sync::lock_mutex(&a.state).1);
        }
    }
    // e2: `(str ex)` is Throwable.toString -- "<class>: <message>" (log lines show the real message).
    if let Value::Inst(inst) = v {
        if inst_is_throwable(inst) {
            return match throwable_message(v) {
                Some(m) => format!("{}: {}", inst.tdef.name, display_str(&m)),
                None => inst.tdef.name.to_string(),
            };
        }
    }
    // `(str x)` is `.toString()`: `class@hash` for fns and reference types,
    // `class java.lang.Long` for a class.
    {
        let tostr = |class: &str, addr: usize| format!("{class}@{addr:x}");
        match v {
            Value::Fn(c) | Value::Macro(c) => {
                {
                let cls = fn_class_name(&c.ns, c.name.as_deref());
                return tostr(&cls, name_hash(&cls));
            }
            }
            Value::Native(n) => {
                {
                let cls = fn_class_name("clojure.core", Some(n.name.as_ref()));
                return tostr(&cls, name_hash(&cls));
            }
            }
            Value::Atom(a) => return tostr(if a.agent { "clojure.lang.Agent" } else { "clojure.lang.Atom" }, idhash(std::sync::Arc::as_ptr(a) as usize)),
            Value::Volatile(x) => return tostr("clojure.lang.Volatile", idhash(std::sync::Arc::as_ptr(x) as usize)),
            Value::Delay(c) => return tostr("clojure.lang.Delay", idhash(std::sync::Arc::as_ptr(c) as usize)),
            Value::Class(c) => {
                if crate::types::array_class_print_name(c.name()).is_none() {
                    let kind = if matches!(**c, crate::types::ClassVal::Interface { .. }) { "interface" } else { "class" };
                    return format!("{kind} {}", c.name());
                }
            }
            _ => {}
        }
    }
    let mut out = String::new();
    write_value(v, false, 0, &mut out);
    out
}

thread_local! {
    /// Wave-C small sweep item 4: whether entering a COLLECTION while
    /// already printing non-readably (`readable == false`) should
    /// PROMOTE its elements back to readable printing.
    ///
    /// Defaults to `true` -- matching every display-ish call site that
    /// existed before this flag did (`str`, error messages, `format`,
    /// ...): Clojure's `str` never binds `*print-readably*`, so its
    /// ambient default (`true`) governs every element `.toString()`
    /// touches, even though the OUTER value itself skips quoting (a bare
    /// string's own `.toString()` is identity, no `*print-readably*`
    /// involved at all). Measured: `(str [1/3 7N 1.5M])` ->
    /// `"[1/3 7N 1.5M]"` (suffixes kept), `(str ["a"])` -> `"[\"a\"]"`
    /// (quoted) -- collection elements ALWAYS print readably under `str`.
    ///
    /// `print`/`println`, by contrast, `(binding [*print-readably* nil]
    /// ...)` for their WHOLE dynamic extent (real Clojure source), so
    /// every element at every depth sees the binding -- `print_family_str`
    /// clears this flag for its call, and every nested `write_value` call
    /// checks it via `child_readable` below instead of hardcoding `true`.
    /// This retires the "KNOWN TRADE" this module used to carry: mova
    /// previously had no way to spell `print`/`println`'s recursive
    /// non-readable walk at all (`(println ["a" "b"])` wrongly printed
    /// `["a" "b"]"` instead of the oracle's `[a b]`); it's the SAME
    /// underlying `readable: bool` parameter throughout `write_value`,
    /// just no longer hardcoding "promote to readable" at every collection
    /// boundary regardless of context.
    static DISPLAY_PROMOTES_ELEMENTS: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
}

/// RAII restore for [`DISPLAY_PROMOTES_ELEMENTS`], same shape as
/// [`PrintMetaGuard`] just below.
struct DisplayPromoteGuard(bool);

impl Drop for DisplayPromoteGuard {
    fn drop(&mut self) {
        DISPLAY_PROMOTES_ELEMENTS.with(|c| c.set(self.0));
    }
}

/// The print/println family's non-readable form: like [`display_str`]
/// at the top (bare strings/chars unquoted), but nested collection
/// elements stay non-readable too, all the way down -- see
/// `DISPLAY_PROMOTES_ELEMENTS`'s doc for why this differs from
/// `display_str`/`str`. Measured against the oracle: `(println ["a"
/// "b"])` -> `[a b]`, `(println [\a])` -> `[a]`, `(println {:a "x"})` ->
/// `{:a x}` -- keywords/symbols are unaffected either way (they have no
/// separate readable/display spelling).
pub fn print_family_str(v: &Value) -> String {
    let guard = DisplayPromoteGuard(DISPLAY_PROMOTES_ELEMENTS.with(|c| c.replace(false)));
    let mut out = String::new();
    write_value(v, false, 0, &mut out);
    drop(guard);
    out
}

/// Whether a COLLECTION currently printing with `readable == parent`
/// should print ITS elements readably. `true` propagates unchanged
/// (`pr_str`'s whole recursion, or a subtree `str`/`print_family_str`
/// already promoted to readable); `false` consults
/// `DISPLAY_PROMOTES_ELEMENTS` -- `str`'s ambient default promotes,
/// `print_family_str`'s cleared flag does not.
fn child_readable(parent: bool) -> bool {
    parent || DISPLAY_PROMOTES_ELEMENTS.with(|c| c.get())
}

fn write_value(v: &Value, readable: bool, depth: usize, out: &mut String) {
    // W3b (item 2, measured): `*print-dup*` forces EVERY value -- at every
    // depth, including nested elements -- to print in its READABLE form,
    // regardless of whether this call started life as `pr`/`pr-str`
    // (already readable) or `print`/`println`/`print-str` (normally
    // display-only): real Clojure's print dispatch checks `*print-dup*`
    // BEFORE consulting `*print-readably*`/the pr-vs-print distinction at
    // all, and print-dup's own registered methods for ordinary types just
    // emit the standard reader-readable literal. Measured:
    // `(binding [*print-dup* true] (print-str 1N))` is `"1N"`, not the
    // plain `str`-style `"1"` `print-str` would otherwise give a BigInt.
    // Recomputing this at every recursive `write_value` call (rather than
    // once at the top) is what makes it apply to nested elements too, the
    // same way `child_readable` propagates an already-readable outer call.
    let readable = readable || PRINT_DUP.with(|c| c.get());
    match v {
        Value::Nil => out.push_str("nil"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Int(i) => out.push_str(&i.to_string()),
        Value::Float(f) => write_float(*f, readable, out),
        Value::Str(s) => {
            // M8: rope-native print (spec op-routing: "write/print via
            // chunks(), no flattening to print a big buffer") -- `Str::
            // write_into`/`write_quoted_into` walk a `Rope`-backed string's
            // chunks directly instead of `Deref`-materializing the whole
            // document just to copy it straight back out again.
            if readable {
                s.write_quoted_into(out);
            } else {
                s.write_into(out);
            }
        }
        Value::Sym(sym) => write_symbol(sym, out),
        Value::Keyword(k) => {
            out.push(':');
            out.push_str(k);
        }
        Value::Char(c) => {
            if readable {
                write_char_literal(*c, out);
            } else {
                out.push(*c);
            }
        }
        // Collections print their ELEMENTS readably even under `str`,
        // but NOT under `print`/`println` -- see `DISPLAY_PROMOTES_
        // ELEMENTS`'s doc for the full measured matrix (this retires the
        // "KNOWN TRADE" note this arm used to carry). `child_readable`
        // computes the flag every collection arm below passes to its
        // elements: unchanged (`true`) once already-readable, otherwise
        // `str`'s ambient default promotes and `print_family_str`'s
        // cleared flag does not.
        Value::List(items) => write_seq(items.iter(), '(', ')', child_readable(readable), depth, out),
        Value::Vector(items) => write_seq(items.iter(), '[', ']', child_readable(readable), depth, out),
        // S7: a map entry prints EXACTLY as the 2-vector it is -- measured,
        // `(pr-str (first {:a 1}))` and `(str (first {:a 1}))` are both
        // `"[:a 1]"`, and `(pr-str (seq {:a 1 :b 2}))` is
        // `"([:a 1] [:b 2])"`. Nothing about entry-ness is printable.
        Value::MapEntry(items) => write_seq(items.iter(), '[', ']', child_readable(readable), depth, out),
        // C10: real Clojure 1.13.0-alpha6 has NO readable `print-method`
        // for `PersistentQueue` at all -- measured, `(pr-str
        // clojure.lang.PersistentQueue/EMPTY)` is `"#object[clojure.lang.
        // PersistentQueue 0x52b56a3e \"clojure.lang.PersistentQueue@1\"]"`,
        // a hashcode/identity string that's DIFFERENT on every process run
        // and therefore impossible to corpus/golden-match (and, per this
        // task's brief, out of scope to reproduce bit-for-bit -- nothing
        // in the vendored suite asserts an exact printed queue string).
        // `#queue [..]` is the tagged-literal spelling later real Clojure
        // versions print (and `read-string` accepts back), which is a
        // more useful, fully deterministic stand-in for diagnostics
        // (`is`'s failure messages, `println`, ...) than either the ugly
        // hashcoded form above or silently reusing `[..]`/`(..)`'s own
        // syntax (which would print `identical?`-looking output for
        // types the reader can't actually round-trip either way).
        Value::Queue(items) => {
            // 1.13.0-alpha6 has no readable print-method for a queue.
            let hash = items.iter().fold(1usize, |h, v| h.wrapping_mul(31).wrapping_add(pr_str(v).len()));
            write_object(out, "clojure.lang.PersistentQueue", hash, None);
        }
        // W4-PRINTER (print-throwable): an `ex-info` map prints as
        // `#error {...}` under the READABLE (`pr`/`pr-str`) path, same as
        // a host exception `Value::Inst` just below -- see this module's
        // `throwable_to_map` doc for why the map has to be built the SAME
        // way `Throwable->map` builds it. Gated on `readable`: real
        // Clojure's `str`/`.toString()` path (mova's `display_str`) never
        // goes through `print-method` dispatch at all (measured: `(str
        // (Exception. "x"))` is `"java.lang.Exception: x"`, not
        // `#error {...}`) -- out of scope here since nothing in the
        // vendored suite asserts `str` on a throwable, only `pr-str`.
        Value::Map(m) if readable && m.get(&Value::Keyword("ex/message".into())).is_some() => {
            if let Some(err_map) = throwable_to_map(v) {
                write_error_layout(&err_map, depth, out);
            }
        }
        Value::Map(m) => {
            if level_exceeded(depth) {
                out.push('#');
                return;
            }
            let mut pairs: Vec<(&Value, &Value)> = m.iter().collect();
            // W4-PRINTER: `PMap::Small` (real Clojure's `PersistentArrayMap`
            // shape, <=`PMAP_SMALL_MAX` entries) preserves INSERTION order --
            // measured, `(pr-str {:user/a 1 :b 2})` is `"{:user/a 1, :b 2}"`,
            // in literal/insertion order, not resorted. Only `PMap::Big` (the
            // CHAMP HAMT, entered once an `assoc` grows past the threshold)
            // gets the synthetic sort below: its real iteration order is
            // CHAMP's hash order (deterministic given the same contents, but
            // not insertion order and not sorted), which is not reproducible
            // here bit-for-bit, so sorting by the key's `pr_str` is the
            // best available stand-in for deterministic, reproducible
            // *printed* output. Sorting a `Small` map here (the pre-existing
            // behavior) was a deviation that broke `printer.clj`'s own
            // `print-ns-maps` deftest (rows whose insertion order is not
            // already alphabetical, e.g. `{:user/a 1, :b 2}`).
            if matches!(m, crate::value::PMap::Big(_) | crate::value::PMap::Shaped(_)) {
                pairs.sort_by_key(|(k, _)| pr_str(k));
            }
            let elem_readable = child_readable(readable);
            // D5: `*print-namespace-maps*`'s `#:ns{...}` prefix form --
            // see `lifted_ns`. `None` (the default, and any map that
            // isn't liftable) prints exactly as before.
            let ns = lifted_ns(&pairs);
            if let Some(ns) = &ns {
                out.push_str("#:");
                out.push_str(ns);
            }
            out.push('{');
            write_truncated(pairs.len(), ", ", out, |i, out| {
                let (k, v) = pairs[i];
                if ns.is_some() {
                    write_stripped_key(k, elem_readable, depth + 1, out);
                } else {
                    write_value(k, elem_readable, depth + 1, out);
                }
                out.push(' ');
                write_value(v, elem_readable, depth + 1, out);
            });
            out.push('}');
        }
        // W3: shape order, NOT `Value::Map`'s alphabetical-by-key sort
        // above -- a `HostStruct` prints its fields in the order its
        // `Shape` declared them, matching `keys`/`vals`/`seq`'s own
        // shape-order guarantee (see `crate::host_struct`'s doc for why
        // this deliberately does NOT delegate through `as_pmap`: touch-only
        // ops read the `Shape` directly instead of materializing).
        Value::LazyMap(lm) => write_value(&Value::Map(crate::lazy_map::as_pmap(lm).clone()), readable, depth, out),
        Value::HostStruct(hs) => {
            let elem_readable = child_readable(readable);
            out.push('{');
            for (i, field) in hs.shape.fields.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push(':');
                out.push_str(&field.key);
                out.push(' ');
                write_value(&crate::host_struct::get_field(hs, i), elem_readable, depth + 1, out);
            }
            out.push('}');
        }
        // S3 (measured): a record prints `#user.R{:a 1, :b 2}` with basis
        // fields in DECLARATION order (then ext keys); a deftype prints
        // `#object[user.T 0x<addr> "user.T@<addr>"]` (the JVM's identity
        // hash -- inherently nondeterministic, so no corpus form can
        // assert it; mova uses the Arc address for the same shape); a
        // class prints its bare name (`(pr-str (class 3))` ->
        // `java.lang.Long`).
        // S4 (measured): `(str *ns*)` is just the bare namespace name
        // ("user"), NOT the generic deftype `#object[...]` shape below --
        // `(pr-str *ns*)`, by contrast, DOES use that shape on the real
        // JVM (`#object[clojure.lang.Namespace 0x... "user"]`, address
        // nondeterministic, so no corpus form can assert it either way).
        Value::Inst(_) if crate::reader::reader_cond_parts(v).is_some() => {
            let (form, splicing) = crate::reader::reader_cond_parts(v).expect("checked by the guard above");
            out.push_str(if splicing { "#?@" } else { "#?" });
            write_value(&form, readable, depth + 1, out);
        }
        Value::Inst(inst) if readable && crate::ns::ns_value_name(v).is_some() => {
            let name = crate::ns::ns_value_name(v).expect("checked by the guard above");
            let _ = inst;
            let addr = name_hash(&name);
            out.push_str(&format!("#object[clojure.lang.Namespace 0x{addr:x} \"{name}\"]"));
        }
        Value::Inst(_) if !readable && crate::ns::ns_value_name(v).is_some() => {
            out.push_str(&crate::ns::ns_value_name(v).expect("checked by the guard above"));
        }
        // W4-PRINTER (print-throwable): a host exception `Value::Inst`
        // (`Exception.`/`Throwable.`/`Error.`/...) prints `#error {...}`
        // under the readable path, same rationale and same `readable`
        // gate as the `ex-info` map arm above.
        Value::Inst(inst) if readable && inst_is_throwable(inst) => {
            if let Some(err_map) = throwable_to_map(v) {
                write_error_layout(&err_map, depth, out);
            }
        }
        Value::Inst(inst) => {
            if inst.tdef.is_record {
                // C15: record elements follow the same child-readability
                // rule as every other collection (see `child_readable`).
                let elem_readable = child_readable(readable);
                let dup = PRINT_DUP.with(|c| c.get()) && !VERBOSE_DEFRECORDS.with(|c| c.get());
                out.push('#');
                out.push_str(&inst.tdef.name);
                if dup {
                    // C14 (protocols): measured -- `*print-dup*` true
                    // (and `*verbose-defrecords*` false) prints the
                    // constructor-literal-readable positional form,
                    // `#ns.R[1, 2]`, values only, comma-separated.
                    // `*print-dup*` demands re-readable output, so the
                    // values print readably regardless of the outer mode.
                    out.push('[');
                    for (i, (_, v)) in inst.ordered_entries().iter().enumerate() {
                        if i > 0 {
                            out.push_str(", ");
                        }
                        write_value(v, true, depth + 1, out);
                    }
                    out.push(']');
                } else {
                    out.push('{');
                    for (i, (k, v)) in inst.ordered_entries().iter().enumerate() {
                        if i > 0 {
                            out.push_str(", ");
                        }
                        write_value(k, elem_readable, depth + 1, out);
                        out.push(' ');
                        write_value(v, elem_readable, depth + 1, out);
                    }
                    out.push('}');
                }
            } else {
                let addr = idhash(std::sync::Arc::as_ptr(inst) as usize);
                out.push_str(&format!(
                    "#object[{} 0x{:x} \"{}@{:x}\"]",
                    inst.tdef.name, addr, inst.tdef.name, addr
                ));
            }
        }
        // S4/1D: an array class prints `<component>/<dims>`, not its raw
        // JVM binary name -- see `types::array_class_print_name`'s doc.
        Value::Class(c) => match crate::types::array_class_print_name(c.name()) {
            Some(friendly) => out.push_str(&friendly),
            None => out.push_str(c.name()),
        },
        Value::Set(s) => {
            if level_exceeded(depth) {
                out.push('#');
                return;
            }
            let mut items: Vec<&Value> = s.iter().collect();
            items.sort_by_key(|v| pr_str(v));
            let elem_readable = child_readable(readable);
            out.push_str("#{");
            write_truncated(items.len(), " ", out, |i, out| {
                write_value(items[i], elem_readable, depth + 1, out);
            });
            out.push('}');
        }
        // JVM shape: `#object[user$f 0x<hash> "user$f@<hash>"]`.
        Value::Fn(c) | Value::Macro(c) => {
            let cls = fn_class_name(&c.ns, c.name.as_deref());
            write_object(out, &cls, name_hash(&cls), None);
        }
        Value::Native(n) => {
            let cls = fn_class_name("clojure.core", Some(n.name.as_ref()));
            write_object(out, &cls, name_hash(&cls), None);
        }
        Value::Atom(a) if a.string_writer => {
            let mut st = String::new();
            write_value(&crate::sync::lock_mutex(&a.state).1, readable, depth, &mut st);
            write_object_ptr(out, "java.io.StringWriter", std::sync::Arc::as_ptr(a) as usize, Some(&st));
        }
        Value::Atom(a) => {
            let mut st = String::from("{:status :ready, :val ");
            write_value(&crate::sync::lock_mutex(&a.state).1, true, depth, &mut st);
            st.push('}');
            let class = if a.agent { "clojure.lang.Agent" } else { "clojure.lang.Atom" };
            write_object_ptr(out, class, std::sync::Arc::as_ptr(a) as usize, Some(&st));
        }
        Value::Volatile(v) => {
            let mut st = String::from("{:status :ready, :val ");
            write_value(&crate::sync::lock_read(v), true, depth, &mut st);
            st.push('}');
            write_object_ptr(out, "clojure.lang.Volatile", std::sync::Arc::as_ptr(v) as usize, Some(&st));
        }
        // C3e: `LazyTail` prints exactly as the `Lazy` it wraps. Reached
        // only when something prints a RAW improper list without
        // `Interp::realize_deep` first (an internal diagnostic path --
        // every user-facing printer realizes, and realizing splices the
        // marked tail's elements in, so the marker is invisible there).
        Value::Lazy(l) | Value::LazyTail(l) => match crate::sync::lock_mutex(&l.realized).as_ref() {
            Some(realized) => write_value(realized, readable, depth, out),
            None => out.push_str("#<lazy-seq>"),
        },
        Value::Future(cell) => {
            let state = crate::sync::lock_mutex(&cell.state);
            let mut st = String::new();
            match &*state {
                crate::value::FutureState::Pending => st.push_str("{:status :pending, :val nil}"),
                crate::value::FutureState::Done(v) => {
                    st.push_str("{:status :ready, :val ");
                    write_value(v, true, depth, &mut st);
                    st.push('}');
                }
                crate::value::FutureState::Failed(_) => st.push_str("{:status :failed, :val nil}"),
            }
            write_object_ptr(
                out,
                "clojure.core$future_call$reify__8673",
                std::sync::Arc::as_ptr(cell) as usize,
                Some(&st),
            );
        }
        Value::Promise(cell) => {
            let state = crate::sync::lock_mutex(&cell.state);
            let mut st = String::new();
            match &*state {
                crate::value::PromiseState::Pending => st.push_str("{:status :pending, :val nil}"),
                crate::value::PromiseState::Delivered(v) => {
                    st.push_str("{:status :ready, :val ");
                    write_value(v, true, depth, &mut st);
                    st.push('}');
                }
            }
            write_object_ptr(
                out,
                "clojure.core$promise$reify__8720",
                std::sync::Arc::as_ptr(cell) as usize,
                Some(&st),
            );
        }
        Value::Delay(cell) => {
            let mut st = String::new();
            match cell.result.get() {
                Some(Ok(v)) => {
                    st.push_str("{:status :ready, :val ");
                    write_value(v, true, depth, &mut st);
                    st.push('}');
                }
                Some(Err(_)) => st.push_str("{:status :failed, :val nil}"),
                None => st.push_str("{:status :pending, :val nil}"),
            }
            write_object_ptr(out, "clojure.lang.Delay", std::sync::Arc::as_ptr(cell) as usize, Some(&st));
        }
        // C10: real Clojure's `(pr-str (reduced 5))` is
        // `#object[clojure.lang.Reduced 0x<hash> {:status :ready, :val
        // 5}]` -- an identity-hashed, non-deterministic string (same
        // "no golden could pin it either" situation `Matcher` below is
        // in), and a bare `Reduced` escaping to `pr-str` isn't a shape
        // any measured test needs anyway (it's meant to be unwrapped by
        // a reduce loop before printing). Prints its wrapped value so a
        // stray one is at least legible rather than opaque.
        Value::Reduced(v) => {
            out.push_str("#<reduced ");
            write_value(v, readable, depth, out);
            out.push('>');
        }
        Value::Channel(cell) => {
            let status = if crate::sync::lock_mutex(&cell.state).closed {
                "closed"
            } else {
                "open"
            };
            out.push_str("#<chan ");
            out.push_str(status);
            out.push('>');
        }
        // S4 (everyday3): no corpus form prints a bare matcher (real
        // Clojure's own `#object[java.util.regex.Matcher 0x<hash> "..."]`
        // is nondeterministic -- identity hash -- so no golden could pin
        // it either); this shape only needs to be *some* stable, non-
        // reader-syntax string.
        Value::Matcher(_) => out.push_str("#<matcher>"),
        // TIMER-CANCEL: same policy -- some stable, non-reader-syntax
        // string. Deliberately NOT the armed/settled status: printing a
        // racy snapshot would make `pr-str` of the same live value
        // nondeterministic (the fire can win mid-print), and the one
        // caller who can act on the status already has `timer-armed?`.
        Value::Timer(_) => out.push_str("#<timer>"),
        // SPEC-W6a: same policy again. Real test.check prints a
        // `JavaUtilSplittableRandom` as `#object[clojure.test.check.
        // random.JavaUtilSplittableRandom 0x<identity-hash> "..."]`,
        // which no golden could ever pin, so this is just SOME stable,
        // non-reader-syntax string. Deliberately NOT the gamma/state
        // pair: printing a seeded RNG's internals would invite corpora
        // to depend on them, and they are an implementation detail of
        // the algorithm, not part of its contract.
        Value::TcRandom(_) => out.push_str("#<random>"),
        // S5: like `Matcher` above -- no corpus form prints a bare host
        // instance raw (real Clojure's own `#object[java.util.Random
        // 0x<hash> "..."]` is nondeterministic, identity hash), so this
        // only needs to be SOME stable, non-reader-syntax string.
        // SPEC-W1 task 4: a `HostKind::Date` is the ONE exception -- it
        // prints as `#inst "..."`, exactly what real Clojure's own
        // `print-method` for `java.util.Date` emits (measured: the
        // pending-conformance golden for `#inst "2020-01-01"` is `#inst
        // "2020-01-01T00:00:00.000-00:00"`), so a `#inst` literal now
        // round-trips through `pr-str`/`read-string`. Both print modes
        // get the same text: `str` on a real `java.util.Date` is
        // `Date.toString()`, which is locale- AND timezone-dependent
        // (`"Thu Jan 01 00:00:00 UTC 1970"`) and not reproducible
        // portably, and no corpus line asserts it -- a deterministic
        // `#inst` is closer to the truth than the old `#<date>` either
        // way.
        Value::HostInst(h) => {
            if let Some(ms) = crate::hostclass::date_millis(v) {
                out.push_str("#inst \"");
                out.push_str(&crate::hostclass::format_inst(ms));
                out.push('"');
            } else if let Some(path) = crate::hostclass::java_file_path(v) {
                // kondo-wave: real `java.io.File.toString()` is just the
                // stored path, no quoting -- clj-kondo/babashka.fs code
                // does `(str file)` expecting exactly that (e.g.
                // `impl/core.clj`'s `excluded?`: `(str file)` fed to a
                // regex). `pr-str` has no vendored assertion either way
                // (real Clojure's own is the nondeterministic-hash
                // `#object[...]` form, same as `Matcher`/`Random` above),
                // so this reuses the plain path for both rather than
                // minting an unreadable literal.
                out.push_str(path.as_ref());
            } else if let Some(u) = crate::hostclass::url_str_of(v) {
                // e2: `URL.toString()` is its spelling.
                out.push_str(u.as_ref());
            } else if !readable
                && matches!(
                    h.kind,
                    crate::hostclass::HostKind::StringBuilder | crate::hostclass::HostKind::StringBuffer
                )
            {
                // clojure-lsp campaign (mova/PLAN.md): `str`/`display_str`
                // on a real `StringBuilder`/`StringBuffer` IS its content
                // (`String.valueOf(Object)` calls `.toString()`), not a
                // diagnostic placeholder -- `rewrite-clj.parser`'s own
                // `parse-token` builds every token with `(str buf)`, never
                // `.toString`, so this is the ONLY path clojure-lsp's own
                // token reading has to reach the buffer's real content.
                // `pr-str` (readable) keeps the placeholder below,
                // unmeasured and non-deterministic on the JVM too (a real
                // `#object[...]` identity-hash address), same as `Date`'s
                // fallback right above.
                let guard = crate::sync::lock_mutex(&h.state);
                if let crate::hostclass::HostState::CharBuf(s) = &*guard {
                    out.push_str(s);
                }
            } else if matches!(h.kind, crate::hostclass::HostKind::Thread) {
                let text = crate::hostclass::thread_to_string(&crate::sync::lock_mutex(&h.state));
                if readable {
                    write_object_ptr(out, "java.lang.Thread", std::sync::Arc::as_ptr(h) as usize, Some(&format!("\"{text}\"")));
                } else {
                    out.push_str(&text);
                }
            } else if matches!(h.kind, crate::hostclass::HostKind::Object) {
                write_object_ptr(out, "java.lang.Object", std::sync::Arc::as_ptr(h) as usize, None);
            } else {
                out.push_str("#<");
                out.push_str(h.kind.diagnostic_name());
                out.push('>');
            }
        }
        // S6: `#uuid "..."` under `pr-str` (measured: matches real
        // Clojure's own `print-method` for `java.util.UUID` exactly,
        // unlike every other host shape here); `str` drops the `#uuid `
        // prefix, matching `UUID.toString()`.
        Value::Uuid(u) => {
            if readable {
                out.push_str("#uuid \"");
                out.push_str(&Value::format_uuid(**u));
                out.push('"');
            } else {
                out.push_str(&Value::format_uuid(**u));
            }
        }
        // S6: like `Matcher`/`HostInst` above -- real Clojure's own
        // `java.net.URI` print is `#object[java.net.URI 0x<hash> "..."]`
        // (nondeterministic identity hash, measured), so no golden could
        // pin the readable form either way; this is SOME stable,
        // non-reader-syntax string. `str` is exact (`URI.toString()` is
        // just the text the constructor was given).
        Value::Uri(s) => {
            if readable {
                out.push_str("#<uri ");
                s.write_into(out);
                out.push('>');
            } else {
                s.write_into(out);
            }
        }
        // Matches Clojure: `(str #"a")` -> `"a"` (bare pattern text, no
        // `#`/quotes), `(pr-str #"a")` -> `#"a"`. Only `"` needs escaping to
        // stay re-readable -- the reader (see `read_regex`) passes every
        // other backslash sequence through raw, so re-escaping backslashes
        // here (the way `Str::write_quoted_into` does for plain strings) would
        // change the pattern on round-trip instead of preserving it.
        Value::Regex(re) => {
            if readable {
                out.push_str("#\"");
                for c in re.as_str().chars() {
                    if c == '"' {
                        out.push('\\');
                    }
                    out.push(c);
                }
                out.push('"');
            } else {
                out.push_str(re.as_str());
            }
        }
        // `#'name` -- readable AND display form are the same, matching
        // Clojure's `(pr-str #'x)` / `(str #'x)` (both print `#'ns/x`).
        Value::Var(cell) => {
            // W3b (item 4): `*print-meta*` on a var prints its OWN
            // mutable metadata (`VarCell::var_meta`, an `IReference` slot
            // -- see that fn's doc), not `Value::Meta`'s IObj wrapper (a
            // var is never `IObj`). Gated the same way `Value::Meta`'s
            // arm is (`readable && *print-meta*`); measured against
            // `printer.clj`'s `print-meta` deftest, which only checks the
            // printed meta chunk equals `(pr-str (meta x))` verbatim, so
            // this deliberately skips `write_meta_prefix`'s `^Tag` reader
            // shorthand (which would print a DIFFERENT string than
            // `(pr-str (meta x))` for a var whose meta happened to be
            // exactly `{:tag X}`) and always writes the map in full
            // instead.
            if readable && PRINT_META.with(|c| c.get()) {
                let meta = crate::coredocs::with_docs(cell, cell.var_meta());
                if !meta_map_is_empty(&meta) {
                    out.push('^');
                    write_value(&meta, true, 0, out);
                    out.push(' ');
                }
            }
            out.push_str("#'");
            // W3b (item 3): a cell interned in `clojure.core` carries a
            // BARE symbol (`ns: None`, see `VarCell::name`'s doc and
            // `crate::ns::var_symbol_in`) -- printing it needs its own
            // `clojure.core/` prefix, since `write_symbol` only ever
            // prints the ns a symbol ALREADY carries. Measured:
            // `(pr-str #'pr-str)` is `"#'clojure.core/pr-str"`, not
            // `"#'pr-str"`.
            if cell.name.ns.is_none() {
                out.push_str(crate::ns::CORE_NS);
                out.push('/');
            }
            write_symbol(&cell.name, out);
        }
        Value::Flow(cell) => {
            let status = match *crate::sync::lock_mutex(&cell.phase) {
                crate::value::FlowPhase::Created => "created",
                crate::value::FlowPhase::Running => "running",
                crate::value::FlowPhase::Stopped => "stopped",
            };
            out.push_str("#<flow ");
            out.push_str(status);
            out.push('>');
        }
        // SPEC-B-bignum-wiring.md §3: `1/3` prints identically in BOTH
        // modes -- Clojure's `Ratio.toString()` has no separate readable
        // form (measured: `(str 1/3)` and `(pr-str 1/3)` are both `"1/3"`).
        Value::Ratio(r) => out.push_str(&r.to_ratio_string()),
        // `7N` -- `N` suffix only in readable (pr-str) mode; `str` prints
        // the plain digits (measured: `(str 7N)` -> `"7"`, `(pr-str 7N)`
        // -> `"7N"`), matching `BigInt.toString()`'s two Clojure call
        // sites (`print-method` vs plain `.toString`).
        Value::BigInt(b) => {
            out.push_str(&b.to_decimal_string());
            if readable {
                out.push('N');
            }
        }
        // S5: a `java.math.BigInteger` prints as bare digits in BOTH modes
        // -- no `N`, because that suffix is `clojure.lang.BigInt`'s
        // reader-round-trip marker and a `BigInteger` has no reader
        // syntax at all (measured: `(numerator 1/3)` prints `1`,
        // `(pr-str (biginteger 5))` is `"5"`). This is the ONE observable
        // difference from the `BigInt` arm directly above, which is why
        // the two are separate `Value` variants over the same
        // `BigIntVal`.
        // W3b (item 2): under `*print-dup*`, a bare `BigInteger` (which
        // has no reader syntax of its own -- see the doc above) prints
        // the reader-EVAL form real Clojure's `print-dup` uses for any
        // type with no literal syntax: `#=(<ctor-form>)`, read back by
        // re-evaluating the form. Measured: `(binding [*print-dup* true]
        // (print-str (java.math.BigInteger. "1")))` is
        // `"#=(java.math.BigInteger. \"1\")"`. Every OTHER numeric type
        // this file prints (`Int`/`Float`/`BigInt`/`BigDec`) already has
        // its own reader syntax, so `*print-dup*` changes nothing for
        // them (measured: `1N`/`1M` print identically whether
        // `*print-dup*` is bound or not) -- this is the one arm that
        // actually branches on it.
        Value::BigInteger(b) => {
            if PRINT_DUP.with(|c| c.get()) {
                out.push_str("#=(java.math.BigInteger. \"");
                out.push_str(&b.to_decimal_string());
                out.push_str("\")");
            } else {
                out.push_str(&b.to_decimal_string());
            }
        }
        // `1.5M` -- same readable/str split as `BigInt` above, and NEVER
        // canonicalized here: `1.50M` must print with its scale intact
        // (measured `(pr-str 1.50M)` -> `"1.50M"`) even though `1.5M` and
        // `1.50M` are `=` and hash the same (see `Value`'s `PartialEq`).
        Value::BigDec(d) => {
            out.push_str(&d.to_java_string());
            if readable {
                out.push('M');
            }
        }
        // S4/1D (measured): `(pr-str (into-array [1 2]))` ->
        // `#object["[Ljava.lang.Long;" 0x775594f2 "[Ljava.lang.Long;@775594f2"]`
        // (quoted class name, unlike a deftype's UNQUOTED
        // `#object[user.T 0x.. ..]` above -- an array's JVM class name has
        // `[`/`;` in it, so real Clojure's own `print-method` quotes it);
        // `(str (into-array [1 2]))` -> bare `[Ljava.lang.Long;@2228db21`
        // (`.toString`, no `#object` wrapper at all -- unlike deftype's
        // printer above, this DOES branch on `readable`, since the two
        // forms are genuinely different strings here, not just quoted vs.
        // not). The identity hash is the JVM's per-run-random
        // `Object.hashCode` -- inherently nondeterministic, so no corpus
        // form may assert it (see tests/conformance/DEVIATIONS.md); mova
        // substitutes its own `Arc` address for the same shape, same
        // caveat as the deftype arm above.
        Value::Array(arr) => {
            let cls = crate::types::array_jvm_name_dims(&arr.kind, arr.dims);
            let addr = idhash(std::sync::Arc::as_ptr(arr) as usize);
            if readable {
                out.push_str(&format!("#object[\"{cls}\" 0x{addr:x} \"{cls}@{addr:x}\"]"));
            } else {
                out.push_str(&format!("{cls}@{addr:x}"));
            }
        }
        // S4: entries are already comparator-sorted incrementally (see
        // `value::SortedMapVal`'s doc) -- print in THAT order, deliberately
        // NOT `Value::Map`'s alphabetical-by-pr_str-of-key resort above
        // (measured: `(pr-str (sorted-map-by > 1 :a 2 :b))` is `"{2 :b, 1
        // :a}"`, descending, not re-sorted by key text).
        Value::SortedMap(m) => {
            if level_exceeded(depth) {
                out.push('#');
                return;
            }
            let elem_readable = child_readable(readable);
            // W4-PRINTER (CLJ-2537): `*print-namespace-maps*`'s `#:ns{...}`
            // lift applies to a sorted map exactly like an ordinary one --
            // measured, `printer.clj`'s `print-ns-maps` deftest has a
            // `sorted-map-by` row expecting `#:x.y{...}`. `lifted_ns` only
            // asks "do these keys share a namespace" (order-independent),
            // so it's safe to run over the entries in their EXISTING
            // comparator order -- that order itself is never touched here.
            let pairs: Vec<(&Value, &Value)> = m.entries.iter().map(|(k, v)| (k, v)).collect();
            let ns = lifted_ns(&pairs);
            if let Some(ns) = &ns {
                out.push_str("#:");
                out.push_str(ns);
            }
            out.push('{');
            write_truncated(pairs.len(), ", ", out, |i, out| {
                let (k, v) = pairs[i];
                if ns.is_some() {
                    write_stripped_key(k, elem_readable, depth + 1, out);
                } else {
                    write_value(k, elem_readable, depth + 1, out);
                }
                out.push(' ');
                write_value(v, elem_readable, depth + 1, out);
            });
            out.push('}');
        }
        Value::SortedSet(s) => {
            if level_exceeded(depth) {
                out.push('#');
                return;
            }
            let elem_readable = child_readable(readable);
            out.push_str("#{");
            write_truncated(s.entries.len(), " ", out, |i, out| {
                write_value(&s.entries[i], elem_readable, depth + 1, out);
            });
            out.push('}');
        }
        // C2 (defstruct): fixed layout order (basis keys, then extension
        // keys -- see `value::StructMapVal`'s doc), deliberately NOT
        // `Value::Map`'s alphabetical-by-pr_str resort above (measured:
        // `(pr-str (struct-map s :c 3 :a 1 :b 2))` is `"{:a 1, :b 2, :c
        // 3}"` -- basis order first regardless of call order).
        Value::StructMap(sm) => {
            if level_exceeded(depth) {
                out.push('#');
                return;
            }
            let elem_readable = child_readable(readable);
            // W4-PRINTER (CLJ-2469): a struct-map lifts to `#:ns{...}` just
            // like an ordinary map -- measured, `printer.clj`'s
            // `print-ns-maps` deftest asserts `(struct (create-struct :q/a
            // :q/b :q/c) 1 2 3)` prints `"#:q{:a 1, :b 2, :c 3}"`. Basis
            // order (this arm's existing, deliberately-not-resorted layout)
            // is untouched either way -- `lifted_ns` doesn't reorder.
            let pairs: Vec<(&Value, &Value)> = sm.entries.iter().map(|(k, v)| (k, v)).collect();
            let ns = lifted_ns(&pairs);
            if let Some(ns) = &ns {
                out.push_str("#:");
                out.push_str(ns);
            }
            out.push('{');
            write_truncated(pairs.len(), ", ", out, |i, out| {
                let (k, v) = pairs[i];
                if ns.is_some() {
                    write_stripped_key(k, elem_readable, depth + 1, out);
                } else {
                    write_value(k, elem_readable, depth + 1, out);
                }
                out.push(' ');
                write_value(v, elem_readable, depth + 1, out);
            });
            out.push('}');
        }
        // C2: no corpus form prints a bare basis object raw (real
        // Clojure's own `#object[clojure.lang.PersistentStructMap$Def
        // 0x<hash> ...]` is nondeterministic -- identity hash -- so no
        // golden could pin it either way), same reasoning as `Matcher`/
        // `HostInst` below -- this is just SOME stable, non-reader-syntax
        // string.
        Value::StructBasis(_) => out.push_str("#<struct-basis>"),
        // S4: a typed vector prints exactly like a plain `Vector` of its
        // (already-coerced) elements -- `kind` is never printed (measured:
        // `(pr-str (vector-of :char 65))` is `"[\\A]"`, no `:char` tag).
        Value::TypedVec(v) => write_seq(v.data.iter(), '[', ']', child_readable(readable), depth, out),
        // C7 (vecveneer): both `VecSeqKind`s are `ISeq`s, printed exactly
        // like any other seq -- parens, not brackets (measured: `(pr-str
        // (.rseq [0 1 2]))` is `"(2 1 0)"`).
        Value::VecSeq(vs) => write_seq(vs.items.iter(), '(', ')', child_readable(readable), depth, out),
        // S5/M3: metadata is INVISIBLE by default -- `(pr-str (with-meta
        // [1 2] {:a 1}))` is `"[1 2]"` (measured) -- and only surfaces
        // under `*print-meta*`. The gate is `*print-meta*` AND
        // `readable`, mirroring Clojure's own `print-meta` helper, which
        // checks `(and *print-meta* *print-readably* (meta o))`: that
        // second conjunct is why `(print-str (with-meta [1 2] {:a 1}))`
        // stays `"[1 2]"` even under the binding (measured).
        Value::Meta(m) => {
            if readable && PRINT_META.with(|c| c.get()) {
                write_meta_prefix(&m.meta, out);
            }
            write_value(&m.inner, readable, depth, out);
        }
        // C13: like `Matcher`/`HostInst`/`Uri` above -- real Clojure's own
        // print is `#object[clojure.lang.Reduced 0x<hash> {:status ...
        // :val ...}]` (nondeterministic identity hash, measured), so no
        // golden could pin the readable form either way; this is SOME
    }
}

/// `print-throwable`'s exact text layout (`clojure.core-print`), so an nREPL
/// client sees what the JVM sends: `#error {\n :cause ..\n :via\n [{:type ..`.
fn write_error_layout(err_map: &Value, depth: usize, out: &mut String) {
    let Value::Map(m) = err_map else { return };
    let get = |k: &str| m.get(&Value::Keyword(k.into())).cloned();
    let w = |v: &Value, out: &mut String| write_value(v, true, depth, out);
    out.push_str("#error {\n :cause ");
    w(&get("cause").unwrap_or(Value::Nil), out);
    if let Some(d) = get("data") {
        if !matches!(d, Value::Nil) {
            out.push_str("\n :data ");
            w(&d, out);
        }
    }
    if let Some(Value::Vector(via)) = get("via") {
        out.push_str("\n :via\n [");
        for (i, entry) in via.iter().enumerate() {
            if i > 0 {
                out.push_str("\n  ");
            }
            let Value::Map(e) = entry else { continue };
            let g = |k: &str| e.get(&Value::Keyword(k.into())).cloned();
            out.push_str("{:type ");
            w(&g("type").unwrap_or(Value::Nil), out);
            out.push_str("\n   :message ");
            w(&g("message").unwrap_or(Value::Nil), out);
            if let Some(d) = g("data") {
                if !matches!(d, Value::Nil) {
                    out.push_str("\n   :data ");
                    w(&d, out);
                }
            }
            if let Some(at) = g("at") {
                if !matches!(at, Value::Nil) {
                    out.push_str("\n   :at ");
                    w(&at, out);
                }
            }
            out.push('}');
        }
        out.push(']');
    }
    if let Some(Value::Vector(trace)) = get("trace") {
        out.push_str("\n :trace\n [");
        for (i, t) in trace.iter().enumerate() {
            if i > 0 {
                out.push_str("\n  ");
            }
            w(t, out);
        }
        out.push(']');
    }
    out.push('}');
}

/// Writes the `^...` prefix `*print-meta*` puts in front of a value,
/// including the single trailing space, or NOTHING at all when the
/// metadata map is empty.
///
/// Two measured details this encodes:
///
/// - An EMPTY metadata map prints no prefix: `(binding [*print-meta*
///   true] (pr-str (with-meta [1] {})))` is `"[1]"`, not `"^{} [1]"`.
///   (An empty meta map is still genuinely *present* -- `(meta (with-meta
///   [] {}))` is `{}`, not `nil` -- it just doesn't print.)
/// - A metadata map that is EXACTLY `{:tag x}` prints in reader
///   shorthand: `^String s`, not `^{:tag String} s` (measured). Any other
///   single-entry map, and any map with a second entry, prints in full.
///
/// The value is written with `readable = true` unconditionally: a
/// metadata map only ever prints at all in readable mode (see the
/// `Value::Meta` arm's gate), and it must round-trip through the reader.
fn write_meta_prefix(meta: &Value, out: &mut String) {
    let tag_only = match meta {
        Value::Map(m) if m.len() == 1 => m.get(&Value::Keyword("tag".into())).cloned(),
        _ => None,
    };
    match tag_only {
        Some(tag) => {
            out.push('^');
            write_value(&tag, true, 0, out);
            out.push(' ');
        }
        None => {
            // `is_empty` via the generic printed form would be wrong for
            // a non-`Map` meta; ask the value itself.
            if meta_map_is_empty(meta) {
                return;
            }
            out.push('^');
            write_value(meta, true, 0, out);
            out.push(' ');
        }
    }
}

/// `true` iff `meta` is a map-ish value with no entries. Anything that
/// isn't map-ish can't be metadata in the first place (`with-meta`
/// rejects it), so the fallback is "not empty" -- print it and let the
/// reader complain, rather than silently dropping it.
fn meta_map_is_empty(meta: &Value) -> bool {
    match meta {
        Value::Map(m) => m.is_empty(),
        Value::SortedMap(m) => m.entries.is_empty(),
        _ => false,
    }
}

fn write_seq<'a, I: Iterator<Item = &'a Value>>(
    items: I,
    open: char,
    close: char,
    readable: bool,
    depth: usize,
    out: &mut String,
) {
    // W3b: `*print-level*` replaces the WHOLE collection (brackets and
    // all) with a bare `#` once `depth >= *print-level*` -- see
    // `level_exceeded`'s doc. Checked before any item is realized against
    // `*print-length*` below, matching real Clojure's precedence
    // (measured: `(binding [*print-level* 0 *print-length* 5]
    // (print-str '(1 2)))` is `"#"`, not `"(...)"`).
    if level_exceeded(depth) {
        out.push('#');
        return;
    }
    out.push(open);
    let items: Vec<&Value> = items.collect();
    write_truncated(items.len(), " ", out, |i, out| {
        write_value(items[i], readable, depth + 1, out);
    });
    out.push(close);
}

/// `#object[<class> 0x<hash> <rest or "class@hash">]`.
/// The JVM prints an identity hash (31 bits, at most 8 hex digits), never an
/// address: derive one from the object's address (stable for the object's life).
fn idhash(addr: usize) -> usize {
    (((addr as u64) >> 3).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 33) as usize & 0x7fff_ffff
}

/// `write_object` for an object known by its address.
fn write_object_ptr(out: &mut String, class: &str, ptr: usize, rest: Option<&str>) {
    write_object(out, class, idhash(ptr), rest)
}

fn write_object(out: &mut String, class: &str, addr: usize, rest: Option<&str>) {
    match rest {
        Some(r) => out.push_str(&format!("#object[{class} 0x{addr:x} {r}]")),
        None => out.push_str(&format!("#object[{class} 0x{addr:x} \"{class}@{addr:x}\"]")),
    }
}

/// A stable stand-in for the JVM identity hash of a fn (same on both tiers).
fn name_hash(class: &str) -> usize {
    class.bytes().fold(0x811c9dc5usize, |h, b| (h ^ b as usize).wrapping_mul(16777619)) & 0x7fff_ffff
}

/// The JVM class name a fn prints under: `user$f`, `user$eval1$fn__2`.
fn fn_class_name(ns: &str, name: Option<&str>) -> String {
    match name {
        Some(n) => {
            let m: String = n
                .chars()
                .map(|c| match c {
                    '-' => '_',
                    '?' => '_',
                    '!' => '_',
                    '*' => '_',
                    '>' => '_',
                    '<' => '_',
                    '=' => '_',
                    '+' => '_',
                    c => c,
                })
                .collect();
            format!("{ns}${m}")
        }
        None => format!("{ns}$eval1$fn__2"),
    }
}

fn write_tagged(out: &mut String, tag: &str, name: Option<&str>) {
    out.push_str("#<");
    out.push_str(tag);
    if let Some(name) = name {
        out.push(' ');
        out.push_str(name);
    }
    out.push('>');
}

/// S12 (CLOJURE-COMPAT-PLAN.md §2): must match `java.lang.Double.toString`,
/// not Rust's `f64::to_string` (which never uses scientific notation --
/// mova used to print `1e10` as `10000000000.0` and `1e300` as 300 raw
/// digits). `readable` picks the symbolic-value spelling for NaN/Infinity:
/// measured `(pr-str ##Inf)` -> `##Inf` but `(str ##Inf)` -> `Infinity`
/// (finite floats format identically either way).
fn write_float(f: f64, readable: bool, out: &mut String) {
    if f.is_nan() {
        out.push_str(if readable { "##NaN" } else { "NaN" });
        return;
    }
    if f.is_infinite() {
        if f > 0.0 {
            out.push_str(if readable { "##Inf" } else { "Infinity" });
        } else {
            out.push_str(if readable { "##-Inf" } else { "-Infinity" });
        }
        return;
    }
    write_finite_float_java(f, out);
}

/// Reformats a finite `f64` to match `java.lang.Double.toString`. Verified
/// fact this builds on (checked 28 values spanning denormals to
/// `f64::MAX`, including `1.2345678901234568E17` <-> `123456789012345680.0`
/// and `4.9E-324`): Rust's shortest-round-tripping digits (`{:e}`,
/// `LowerExp`) are IDENTICAL to Java's on JDK 21 -- both implement the same
/// shortest-representation algorithm. So this is purely a
/// RE-FORMATTING problem, never a re-computation one: pull the digits and
/// decimal exponent out of Rust's `{:e}` rendering, then lay them out by
/// Java's rule instead of Rust's.
///
/// Java's rule: let `m` be the digits and `n` the decimal exponent such
/// that value = `m x 10^n` with one digit before the point. If `10^-3 <=
/// |value| < 10^7` (the `{:e}` exponent is in `-3..=6`): plain decimal,
/// with at least one digit after the point (`100.0`, `0.001`,
/// `9999999.0`). Otherwise: `D.DDDEn` -- exactly one digit before the
/// point, at least one after it (`1e23` -> `1.0E23`, not `1E23`),
/// uppercase `E`, `-` for a negative exponent and no `+`/leading zeros for
/// a positive one (`1.0E7`, `1.0E-4`).
///
/// `pub(crate)`, not private: SPEC-C-casts.md's `bigdec` cast reuses this
/// EXACT function to turn a `Float` into the digit string it feeds
/// `BigDecVal::parse` (Clojure's `(bigdec 1.5)` goes through
/// `BigDecimal.valueOf(double)`, which is `Double.toString` underneath --
/// the same algorithm this function already implements). Reusing it
/// rather than `format!("{}", f)` is load-bearing: the plain `Display`
/// impl never uses scientific notation, so `(bigdec 1e300)` would produce
/// 300 raw digits instead of the `1.0E300`-shaped string this algorithm
/// gives, which `BigDecVal::parse`'s exponent grammar expects.
pub(crate) fn write_finite_float_java(f: f64, out: &mut String) {
    if f == 0.0 {
        // Covers +0.0 AND -0.0 (IEEE-754 equality does not distinguish
        // them); `is_sign_negative` reads the sign bit directly.
        if f.is_sign_negative() {
            out.push('-');
        }
        out.push_str("0.0");
        return;
    }
    let neg = f.is_sign_negative();
    if f.abs() == 5e-324 {
        // Java's `Double.MIN_VALUE` prints as 4.9E-324 (not the shortest round trip).
        out.push_str(if neg { "-4.9E-324" } else { "4.9E-324" });
        return;
    }
    let sci = format!("{:e}", f.abs());
    // Rust's `LowerExp` always emits exactly one `e`, e.g. "1.2345e7" or
    // "1e0" (no fractional part when the shortest round-trip is a single
    // digit) -- never zero-padded, never a `+` on a positive exponent.
    let (mantissa, exp_str) = sci.split_once('e').unwrap_or((sci.as_str(), "0"));
    let exp: i32 = exp_str.parse().unwrap_or(0);
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();

    if neg {
        out.push('-');
    }
    if (-3..=6).contains(&exp) {
        if exp >= 0 {
            let int_digits = exp as usize + 1;
            if digits.len() <= int_digits {
                out.push_str(&digits);
                out.push_str(&"0".repeat(int_digits - digits.len()));
                out.push_str(".0");
            } else {
                out.push_str(&digits[..int_digits]);
                out.push('.');
                out.push_str(&digits[int_digits..]);
            }
        } else {
            out.push_str("0.");
            out.push_str(&"0".repeat((-exp - 1) as usize));
            out.push_str(&digits);
        }
    } else {
        out.push_str(&digits[..1]);
        out.push('.');
        if digits.len() > 1 {
            out.push_str(&digits[1..]);
        } else {
            out.push('0');
        }
        out.push('E');
        out.push_str(&exp.to_string());
    }
}

fn write_char_literal(c: char, out: &mut String) {
    out.push('\\');
    match c {
        '\n' => out.push_str("newline"),
        ' ' => out.push_str("space"),
        '\t' => out.push_str("tab"),
        '\r' => out.push_str("return"),
        '\u{8}' => out.push_str("backspace"),
        '\u{c}' => out.push_str("formfeed"),
        _ => out.push(c),
    }
}

fn write_symbol(sym: &Symbol, out: &mut String) {
    if let Some(ns) = &sym.ns {
        out.push_str(ns);
        out.push('/');
    }
    out.push_str(&sym.name);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn scalars() {
        assert_eq!(pr_str(&Value::Nil), "nil");
        assert_eq!(pr_str(&Value::Bool(true)), "true");
        assert_eq!(pr_str(&Value::Int(42)), "42");
        assert_eq!(pr_str(&Value::Float(1.0)), "1.0");
        assert_eq!(pr_str(&Value::Float(1.5)), "1.5");
    }

    #[test]
    fn strings_quoted_in_pr_str_raw_in_display_str() {
        let s = Value::Str("hi\nthere".into());
        assert_eq!(pr_str(&s), "\"hi\\nthere\"");
        assert_eq!(display_str(&s), "hi\nthere");
    }

    #[test]
    fn chars_readable_vs_display() {
        let c = Value::Char('\n');
        assert_eq!(pr_str(&c), "\\newline");
        assert_eq!(display_str(&c), "\n");
        assert_eq!(pr_str(&Value::Char('a')), "\\a");
    }

    #[test]
    fn collections() {
        let list = Value::List(crate::pvec![Value::Int(1), Value::Int(2)]);
        assert_eq!(pr_str(&list), "(1 2)");
        let vector = Value::Vector(crate::pvec![Value::Int(1), Value::Int(2)]);
        assert_eq!(pr_str(&vector), "[1 2]");

        let mut m = crate::value::PMap::new();
        m.insert(Value::Keyword("a".into()), Value::Int(1));
        m.insert(Value::Keyword("b".into()), Value::Int(2));
        assert_eq!(pr_str(&Value::Map(m)), "{:a 1, :b 2}");

        let s: champ::PersistentHashSet<Value> = std::iter::once(Value::Int(1)).collect();
        assert_eq!(pr_str(&Value::Set(s)), "#{1}");
    }

    #[test]
    fn atoms_and_lazy() {
        let atom = Value::Atom(Arc::new(crate::value::AtomCell::new(Value::Int(5))));
        assert!(pr_str(&atom).ends_with("{:status :ready, :val 5}]"));

        let lazy = Value::Lazy(Arc::new(crate::value::LazySeq {
            thunk: Mutex::new(None),
            realized: Mutex::new(None),
        }));
        assert_eq!(pr_str(&lazy), "#<lazy-seq>");
    }
}
