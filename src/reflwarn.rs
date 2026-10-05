//! W4B-WARNINGS: `*warn-on-reflection*` reflection warnings and
//! `*unchecked-math*` `:warn-on-boxed` boxed-math warnings.
//!
//! ## Why this exists
//!
//! `rt.clj`'s `error-messages` deftest (`should-print-err-message`) and
//! `numbers.clj`'s `warn-on-boxed` deftest assert that specific host-interop
//! shapes -- an unhinted `.field`/`.method` dot-form, an ambiguous overload,
//! an unresolved constructor, an unhinted arg to `inc`/`+`/`<` under
//! `:warn-on-boxed` -- print a one-line diagnostic to `*err*` at the moment
//! the code is DEFINED (a `defn`/`fn`/top-level form), not when it later
//! runs. `core/core.mova` has carried `*warn-on-reflection*`/
//! `*unchecked-math*` as inert plain globals since SPEC-D ("nothing in
//! mova actually CONSULTS them yet ... milestone M4b's job") -- this module
//! is that consultation.
//!
//! ## Honesty boundary
//!
//! mova is a tree-walking interpreter with no JVM-style static compiler, so
//! "reflection" here does not mean "the JVM couldn't emit a direct
//! invokevirtual". It means the SAME thing observably: mova's own
//! analysis, walking a `defn`/`fn` body against the type hints actually in
//! scope (param tags, let-binding tags, literal types, ctor-result types,
//! `defn` return-type tags), cannot statically prove which real member a
//! dot-form/ctor-form/static-call resolves to, so the call will fall back
//! to a dynamic, name-based dispatch at eval time -- mova's own equivalent
//! of the JVM's reflective fallback. Every member table below
//! ([`string_instance_sigs`], [`string_ctor_sigs`], [`bigdecimal_instance_sigs`],
//! [`integer_static_sigs`]) is a real, measured slice of the named JVM
//! class's actual overload set (arities + real parameter classes), not a
//! per-row hardcoded string. Where a class isn't tabulated at all (anything
//! other than `String`/`Object`/`BigDecimal`/`Integer`), this module stays
//! silent rather than guess -- see [`resolve_class_name`]'s doc.
//!
//! ## Where the flag check sits (perf guardrail)
//!
//! [`analyze_top_level`] is called ONCE, from `Interp::eval_form` (the
//! non-recursive top-level entry every file-load/`eval`/REPL path already
//! funnels through -- see that fn's own doc), BEFORE the form is evaluated.
//! Its first line reads the two dynamic vars and returns immediately if
//! both are inert (the default): one `Env::get` lookup per DYNAMIC var,
//! per TOP-LEVEL form -- not per subform, not per function call (a
//! `defn`'d closure's body is walked exactly once, at `defn`-eval time, via
//! this same top-level hook; CALLING that closure later never re-enters
//! this module at all, since closure invocation runs body forms through
//! `eval_form_in`, never `eval_form`). Arithmetic/dot-form semantics
//! themselves are completely untouched by this module -- it only ever
//! calls [`crate::builtins::nsfns::write_shim_err`], never evaluates
//! anything.
//!
//! ## Source locations
//!
//! The suite's regexes (`rt.clj`: `re-matches`, full-string;
//! `numbers.clj`: `re-find` on the `"^Boxed math warning"` prefix only) both
//! merely require SOME `\d+:\d+` digits in the reflection case, and no
//! location info at all in the boxed-math case (measured against the
//! actual vendored regexes -- see `compat/reflwarn-oracle-transcript.txt`
//! and this crate's own `error::line_col`, reused here unchanged rather
//! than re-derived).

use std::collections::HashMap;

use crate::eval::Interp;
use crate::reader::{Form, FormValue, Span};
use crate::value::{Str, Symbol, Value};

/// A local symbol's statically known type, as far as this module's narrow
/// analysis can establish it. `Prim` is a JVM primitive spelling (`"long"`,
/// `"int"`, ...); `Class` is a fully-qualified JVM class name. Anything not
/// established -- an unhinted param, an unhinted `let`-bound expression, a
/// call to an unknown fn -- is `Unknown`, which this module NEVER treats as
/// "known to be absent": an `Unknown` target only drives the
/// "target class is unknown" reflection wording, and an `Unknown` argument
/// only ever WIDENS an overload-resolution filter (see [`compatible`]), it
/// never narrows one. That asymmetry is what keeps this module from
/// spuriously warning on the vast majority of the corpus, which has no type
/// hints at all and isn't part of either target deftest.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Hint {
    Unknown,
    Prim(&'static str),
    Class(&'static str),
    /// A real `^Tag`/ctor-result/deftype-field hint whose class this
    /// module has no member table for (e.g. `^StringBuilder`,
    /// `^java.io.Reader`, `^Closeable`, a deftype's own hinted field).
    /// Real `javac` resolves these statically off the hint alone, so --
    /// unlike `Unknown` -- this NEVER drives a "can't be resolved"
    /// warning; it only means this module can't also check arity/overload
    /// shape the way it does for the handful of classes it models.
    OtherClass,
}

impl Hint {
    fn describe(self) -> String {
        match self {
            Hint::Unknown => "unknown".to_string(),
            Hint::Prim(p) => p.to_string(),
            Hint::Class(c) => c.to_string(),
            Hint::OtherClass => "unknown".to_string(),
        }
    }
}

/// One overload's declared parameter shape, position by position, as far
/// as this module's own tables track it. `AnyArray` stands for "an array
/// type" (real `byte[]`/`char[]`) -- coarse, but the only distinction any
/// targeted row needs: it is never `compatible` with a known non-array
/// hint, only with `Hint::Unknown` (see [`compatible`]).
#[derive(Clone, Copy, PartialEq, Eq)]
enum ParamKind {
    Exact(&'static str),
    AnyArray,
}

fn compatible(pk: ParamKind, h: Hint) -> bool {
    match h {
        Hint::Unknown => true,
        Hint::Class(c) => matches!(pk, ParamKind::Exact(p) if p == c),
        Hint::Prim(p2) => matches!(pk, ParamKind::Exact(p) if p == p2),
        // A definite, different (if unmodeled) real class -- never matches
        // one of THIS table's exact param types, same as any other wrong
        // concrete class would not.
        Hint::OtherClass => false,
    }
}

enum Resolution {
    Resolve,
    NoSuchMember,
    Ambiguous,
}

/// THE single overload-resolution rule this module uses for instance
/// methods, static methods, AND constructors alike -- measured against the
/// oracle (`compat/reflwarn-oracle-transcript.txt`) to agree with every one
/// of R1-R9 and C1-C12 simultaneously (see this module's doc for the
/// worked reasoning): raw name+arity candidate count of 0 is an absent
/// member; exactly 1 always resolves (real javac never type-checks a
/// unique candidate, matching C7's "unique arity resolves even with
/// unknown arg types"); 2+ raw candidates get filtered by whichever
/// argument positions have a KNOWN (non-`Unknown`) hint, and resolve only
/// if filtering narrows to exactly 1 (matches C6's `(String. "x")`,
/// C5's `(Integer/valueOf "1")`) -- otherwise it's ambiguous (matches R4/
/// R5/R6/R8's argument-types wording and R9's ctor case).
fn resolve_member(sigs_at_arity: &[&[ParamKind]], arg_hints: &[Hint]) -> Resolution {
    let raw = sigs_at_arity.len();
    if raw == 0 {
        return Resolution::NoSuchMember;
    }
    if raw == 1 {
        return Resolution::Resolve;
    }
    let filtered = sigs_at_arity
        .iter()
        .filter(|sig| {
            sig.len() == arg_hints.len()
                && sig.iter().zip(arg_hints.iter()).all(|(pk, h)| compatible(*pk, *h))
        })
        .count();
    if filtered == 1 {
        Resolution::Resolve
    } else {
        Resolution::Ambiguous
    }
}

fn sigs_of_arity<'a>(all: &[&'a [ParamKind]], arity: usize) -> Vec<&'a [ParamKind]> {
    all.iter().copied().filter(|s| s.len() == arity).collect()
}

/// `java.lang.String` instance methods this module knows about -- the
/// original `length`/`charAt`/`concat`/`getBytes` (each arity's REAL
/// overload count taken from the `java.lang.String` javadoc, measured not
/// guessed: `getBytes()` is the sole zero-arg overload; `getBytes(String)`/
/// `getBytes(Charset)` are the two one-arg overloads that make R4/R5
/// genuinely ambiguous), plus the real-corpus methods `indexOf`/
/// `lastIndexOf` (two 1-arg overloads apiece, `(int)` and `(String)` --
/// both corpus call sites pass a literal/hinted `String` arg, which
/// narrows to the unique `(String)` overload exactly like R4/R5's
/// `Hint`-filtering already does for `getBytes`), `startsWith`/`endsWith`/
/// `contains`/`substring`/`trim`, each with only ONE real overload at the
/// arity the corpus actually calls (so it resolves unconditionally, same
/// as `concat`).
fn string_instance_sigs(name: &str) -> &'static [&'static [ParamKind]] {
    use ParamKind::Exact;
    match name {
        "length" => &[&[] as &[ParamKind]],
        "charAt" => &[&[Exact("int")] as &[ParamKind]],
        "concat" => &[&[Exact("java.lang.String")] as &[ParamKind]],
        "getBytes" => &[
            &[] as &[ParamKind],
            &[Exact("java.lang.String")],
            &[Exact("java.nio.charset.Charset")],
        ],
        // kondo-wave: real overload sets for the other `String` instance
        // methods `clojure.tools.reader`'s vendored files call under
        // `^String`-hinted locals -- same "measured real JVM overload
        // set" discipline as the rows above, not guesses.
        "substring" => &[&[Exact("int")] as &[ParamKind], &[Exact("int"), Exact("int")]],
        "startsWith" => &[
            &[Exact("java.lang.String")] as &[ParamKind],
            &[Exact("java.lang.String"), Exact("int")],
        ],
        "endsWith" => &[&[Exact("java.lang.String")] as &[ParamKind]],
        "indexOf" => &[
            &[Exact("int")] as &[ParamKind],
            &[Exact("java.lang.String")],
            &[Exact("int"), Exact("int")],
            &[Exact("java.lang.String"), Exact("int")],
        ],
        "lastIndexOf" => &[
            &[Exact("int")] as &[ParamKind],
            &[Exact("java.lang.String")],
            &[Exact("int"), Exact("int")],
            &[Exact("java.lang.String"), Exact("int")],
        ],
        "contains" => &[&[Exact("java.lang.CharSequence")] as &[ParamKind]],
        "trim" => &[&[] as &[ParamKind]],
        _ => &[],
    }
}

/// `java.lang.String`'s real constructors at the arities this module's
/// rows touch: `String()`; the FIVE real one-arg ctors
/// (`String(String)`/`String(byte[])`/`String(char[])`/
/// `String(StringBuilder)`/`String(StringBuffer)`) -- only
/// `String(String)` is `Exact`-compatible with a `Hint::Class("java.lang.
/// String")` argument, which is what makes `(String. "x")` (C6) resolve
/// uniquely; and the two real three-arg byte/char-array-plus-offset-plus-
/// length ctors, neither of which any `AnyArray` slot can match against a
/// plain numeric argument, which is what makes `(String. 1 2 3)` (R9) fail
/// to resolve.
fn string_ctor_sigs(arity: usize) -> Vec<&'static [ParamKind]> {
    use ParamKind::{AnyArray, Exact};
    const ARITY0: &[ParamKind] = &[];
    const ARITY1_STR: &[ParamKind] = &[Exact("java.lang.String")];
    const ARITY1_BYTES: &[ParamKind] = &[AnyArray];
    const ARITY1_CHARS: &[ParamKind] = &[AnyArray];
    const ARITY1_SB: &[ParamKind] = &[Exact("java.lang.StringBuilder")];
    const ARITY1_SBUF: &[ParamKind] = &[Exact("java.lang.StringBuffer")];
    const ARITY3_BYTES: &[ParamKind] = &[AnyArray, Exact("int"), Exact("int")];
    const ARITY3_CHARS: &[ParamKind] = &[AnyArray, Exact("int"), Exact("int")];
    let all: &[&[ParamKind]] = &[
        ARITY0,
        ARITY1_STR,
        ARITY1_BYTES,
        ARITY1_CHARS,
        ARITY1_SB,
        ARITY1_SBUF,
        ARITY3_BYTES,
        ARITY3_CHARS,
    ];
    sigs_of_arity(all, arity)
}

fn object_ctor_sigs(arity: usize) -> Vec<&'static [ParamKind]> {
    const ARITY0: &[ParamKind] = &[];
    let all: &[&[ParamKind]] = &[ARITY0];
    sigs_of_arity(all, arity)
}

/// `java.math.BigDecimal.divide`'s real two-arg overload set (three of
/// them: the deprecated `divide(BigDecimal,int)` legacy-rounding-mode
/// form, `divide(BigDecimal,RoundingMode)`, and `divide(BigDecimal,
/// MathContext)`) -- R6's `(.divide 1M a nil)` has two unhinted/nil args,
/// both `Hint::Unknown`, so filtering can't narrow these three down,
/// which is exactly the real ambiguity the oracle's "(argument types:
/// unknown, unknown)" reports.
fn bigdecimal_instance_sigs(name: &str) -> &'static [&'static [ParamKind]] {
    use ParamKind::Exact;
    match name {
        "divide" => &[
            &[Exact("java.math.BigDecimal")] as &[ParamKind],
            &[Exact("java.math.BigDecimal"), Exact("int")],
            &[Exact("java.math.BigDecimal"), Exact("java.math.RoundingMode")],
            &[Exact("java.math.BigDecimal"), Exact("java.math.MathContext")],
            &[Exact("java.math.BigDecimal"), Exact("int"), Exact("java.math.RoundingMode")],
            &[Exact("java.math.BigDecimal"), Exact("int"), Exact("int")],
        ],
        _ => &[],
    }
}

/// `java.lang.Integer`'s real static `valueOf` overloads: `valueOf(String)`
/// and `valueOf(int)` at arity 1 (the two that make R8/C5 genuinely
/// overload-ambiguous until argument types filter them), `valueOf(String,
/// int)` (radix) at arity 2.
fn integer_static_sigs(name: &str) -> &'static [&'static [ParamKind]] {
    use ParamKind::Exact;
    match name {
        "valueOf" => &[
            &[Exact("java.lang.String")] as &[ParamKind],
            &[Exact("int")],
            &[Exact("java.lang.String"), Exact("int")],
        ],
        _ => &[],
    }
}

/// The ONLY classes this module recognizes by bare name -- deliberately
/// narrow (see this module's doc: an unrecognized `^Tag` must map to
/// `Hint::Unknown`, never to a "known but empty" class, or every member
/// access on it would spuriously "can't be resolved"). Recognizing MORE
/// classes is pure upside (closes more corpus rows) and can be added
/// exactly like these four, each with its own measured member table --
/// out of scope for this pass, whose only mandate is the 9 rt.clj rows +
/// 2 numbers.clj rows.
fn resolve_class_name(bare: &str) -> Option<&'static str> {
    match bare {
        "String" | "java.lang.String" => Some("java.lang.String"),
        "Object" | "java.lang.Object" => Some("java.lang.Object"),
        "Integer" | "java.lang.Integer" => Some("java.lang.Integer"),
        "BigDecimal" | "java.math.BigDecimal" => Some("java.math.BigDecimal"),
        _ => None,
    }
}

/// A handful of `clojure.core` builtins whose REAL definitions carry a
/// return-type hint (`(defn ^String str ...)`, `(defn ^String name ...)`,
/// `(defn ^Class class ...)`) -- consulted the same way a user `defn`'s own
/// return hint is (see [`Ctx::fn_return_hints`]), so `(.trim (str x))`/
/// `(.getName (class x))` don't warn just because mova's own core doesn't
/// carry the hint mova never evaluates anyway.
fn builtin_return_hint(name: &str) -> Option<Hint> {
    if let Some(p) = resolve_prim_name(name) {
        // `(int x)`/`(long x)`/... are real primitive-coercion builtins,
        // each always producing exactly the primitive its name says.
        return Some(Hint::Prim(p));
    }
    match name {
        "str" | "name" | "format" | "subs" => Some(Hint::Class("java.lang.String")),
        // Each names a real, unmodeled class this module still has no
        // member table for -- `class` -> `java.lang.Class`, `biginteger`
        // -> `java.math.BigInteger`, `re-matcher` -> `java.util.regex.
        // Matcher` -- but a definite one, never `Unknown`.
        "class" | "biginteger" | "re-matcher" => Some(Hint::OtherClass),
        _ => None,
    }
}

/// Bare JVM primitive type-hint spellings this module tracks as `Prim`
/// hints (param tags, let-binding tags, expression meta tags -- e.g. the
/// `^long` in `(let [^String x ...])`'s sibling shapes `^long (first ...)`
/// from `numbers.clj`'s boxed-math rows).
fn resolve_prim_name(bare: &str) -> Option<&'static str> {
    match bare {
        "long" => Some("long"),
        "int" => Some("int"),
        "double" => Some("double"),
        "float" => Some("float"),
        "short" => Some("short"),
        "byte" => Some("byte"),
        "char" => Some("char"),
        "boolean" => Some("boolean"),
        _ => None,
    }
}

/// A `:tag` meta VALUE (already evaluated, from a var's own meta or an
/// `:arglists` params-vector's) turned into a `Hint`, the runtime-`Value`
/// counterpart of `hint_from_tag_name`'s Form-level tag-NAME parsing.
fn hint_from_tag_value(tag: &Value) -> Option<Hint> {
    match tag {
        Value::Sym(s) if s.ns.is_none() => Some(hint_from_tag_name(s.name.as_ref())),
        Value::Str(s) => Some(hint_from_tag_name(s.as_ref())),
        // `eval_meta_entry` resolves a bare `:tag` symbol as an ordinary
        // expression before falling back to the literal symbol (see its
        // own doc) -- a class-name symbol like `^Pattern`/`^java.util.
        // regex.Pattern` DOES resolve (to the `Class` value `class`/
        // `instance?` use), so the stored `:tag` is that resolved
        // `Value::Class`, not the symbol, and needs its own name read
        // off it instead of `hint_from_tag_name`'s tag-NAME parsing.
        Value::Class(c) => Some(hint_from_tag_name(c.name())),
        _ => None,
    }
}

/// Merges an `if`'s two branch hints into what real Clojure's local-
/// clearing analyzer would report for the whole expression: identical
/// hints stay as-is; two DIFFERENT but still real, non-primitive classes
/// (`Hint::Class`/`Hint::OtherClass` in any combination -- `(.negate bn)`
/// is genuinely some real, if untabulated, class, exactly like `bn`
/// itself is) still merge to `Hint::OtherClass` (real Clojure resolves
/// the call the SAME way either branch would, even though this module
/// can't name which specific class the merge landed on); anything else
/// (a `Prim` mismatch, either side `Unknown`) merges to `Unknown` --
/// never guess a hint neither branch actually agrees on.
fn merge_hints(a: Hint, b: Hint) -> Hint {
    if a == b {
        return a;
    }
    let class_like = |h: Hint| matches!(h, Hint::Class(_) | Hint::OtherClass);
    if class_like(a) && class_like(b) {
        return Hint::OtherClass;
    }
    Hint::Unknown
}

fn tag_key() -> Value {
    Value::Keyword(crate::keyword::Keyword::from("tag"))
}

/// Looks up `name` as a GLOBAL var (never a local -- callers only reach
/// this after `HintEnv` already missed) and reads its `:tag` meta, the
/// same `(def ^Tag name ...)` metadata `hint_from_tag_name` turns into a
/// `Hint` for a local. `Interp::try_resolve_var_cell` is the read-only
/// resolve (`resolve`/`ns-resolve`'s own lookup) -- it returns `None`
/// instead of interning a placeholder, so probing a symbol that turns out
/// not to be a var at all is side-effect-free.
fn global_var_hint(ctx: &Ctx, name: &Str) -> Option<Hint> {
    let cell = ctx.interp.try_resolve_var_cell(&Symbol::simple(name.clone()))?;
    let Value::Map(meta) = cell.var_meta() else {
        return None;
    };
    hint_from_tag_value(meta.get(&tag_key())?)
}

/// A `defn`'s RETURN-type hint written on its params VECTOR rather than
/// its name (`(defn f ^Tag [x] ...)`, real Clojure's OTHER return-hint
/// spelling -- see `walk_defn`'s name-tag handling for the first one) is
/// real Clojure per-arity method metadata, not var meta -- it surfaces at
/// `(:tag (meta (first (:arglists (meta #'f)))))`, which is exactly what
/// `core/core.mova`'s own `defn` macro's SPEC-W5 auto-`:arglists` computes
/// (measured: `(:arglists (meta #'f))` keeps each params vector, meta and
/// all). Only the FIRST arglist's tag is read -- a reasonable
/// approximation for the single-arity case this closes (a later arity
/// disagreeing is no worse than this lookup not existing at all).
fn global_fn_return_hint(ctx: &Ctx, name: &Str) -> Option<Hint> {
    let cell = ctx.interp.try_resolve_var_cell(&Symbol::simple(name.clone()))?;
    let Value::Map(meta) = cell.var_meta() else {
        return None;
    };
    let arglists = meta.get(&Value::Keyword(crate::keyword::Keyword::from("arglists")))?;
    let first = match arglists {
        Value::List(v) | Value::Vector(v) => v.get(0)?,
        _ => return None,
    };
    let Value::Map(pmeta) = first.obj_meta() else {
        return None;
    };
    hint_from_tag_value(pmeta.get(&tag_key())?)
}

fn hint_from_tag_name(name: &str) -> Hint {
    if let Some(p) = resolve_prim_name(name) {
        return Hint::Prim(p);
    }
    if let Some(c) = resolve_class_name(name) {
        return Hint::Class(c);
    }
    // kondo-wave: a tag naming any OTHER real class (fully-qualified, or
    // a bare name real Clojure resolves via `java.lang`/an `:import`) is
    // a REAL hint, just one whose method table isn't tabulated below --
    // `Hint::Class` (not `Hint::Unknown`, "no hint at all") so `check_
    // dot_form`'s "not tabulated" branch stays silent instead of
    // falsely reporting a reflection warning the real JVM never emits
    // (the real class resolves the member fine; mova just doesn't model
    // its signatures -- same honesty-boundary rule this module's doc
    // already states for anything outside String/BigDecimal). Leaked
    // once per distinct tag spelling actually analyzed for the
    // `'static` lifetime `Hint::Class` needs -- bounded, negligible.
    Hint::Class(Box::leak(name.to_string().into_boxed_str()))
}

/// Reads a form's own `^Tag` metadata (already desugared by the reader
/// into a bare `{:tag Tag}` map, per `reader::Reader::read_meta`'s own
/// doc) as a plain tag-name string, with no evaluation at all -- exactly
/// the same "leave the symbol unresolved" reading `Interp::eval_meta_form`
/// uses at eval time (see that fn's doc), just consulted here at analysis
/// time instead.
fn extract_tag_name(form: &Form) -> Option<String> {
    let meta = form.meta.as_ref()?;
    if let FormValue::Map(pairs) = &meta.value {
        for (k, v) in pairs {
            if let FormValue::Atom(Value::Keyword(kw)) = &k.value {
                if kw.as_ref() == "tag" {
                    return match &v.value {
                        FormValue::Atom(Value::Sym(s)) if s.ns.is_none() => {
                            Some(s.name.to_string())
                        }
                        FormValue::Atom(Value::Str(s)) => Some(s.to_string()),
                        _ => None,
                    };
                }
            }
        }
    }
    None
}

fn as_symbol(form: &Form) -> Option<&Symbol> {
    match &form.value {
        FormValue::Atom(Value::Sym(s)) => Some(s),
        _ => None,
    }
}

/// Lexical hint scope: local symbol name -> statically known [`Hint`].
/// Cloned (never mutated in place across sibling scopes) on every `fn`
/// param list / `let` binding group, which is correct AND cheap here --
/// this whole module only ever runs while `*warn-on-reflection*` or
/// `*unchecked-math*` is actively non-default, i.e. never on the fast
/// path (see this module's doc).
type HintEnv = HashMap<Str, Hint>;

/// One analysis pass's own state: the interpreter (for `write_shim_err`
/// and span-to-line/col), whether each of the two warning kinds is
/// currently active, and a `defn`-return-type-hint table SCOPED TO THIS
/// SINGLE TOP-LEVEL FORM ONLY (mirrors `numbers.clj`/`rt.clj`'s own probed
/// shape: `should-print-err-message`'s C11 control row packs both the
/// return-hinted `defn` and its caller into ONE `(do ...)` top-level form,
/// so a walk-local table -- populated left-to-right as `do`'s children are
/// walked in order -- is all real Clojure's OWN compile-once-per-top-
/// level-form semantics need; a `defn` in an EARLIER top-level form is a
/// documented, out-of-scope limitation, not a silent wrong answer).
struct Ctx<'a> {
    interp: &'a mut Interp,
    warn_reflection: bool,
    warn_boxed: bool,
    fn_return_hints: HashMap<Str, Hint>,
}

/// Entry point: called once per top-level form from `Interp::eval_form`,
/// BEFORE that form is evaluated. See this module's doc for why this is
/// the right and only hook, and for the perf guardrail this first check
/// implements.
/// True when reflection/boxed-math analysis is force-disabled at the
/// process level (`--no-reflection-warnings` CLI flag, wired by `main.rs`
/// to set this same env var before any code loads, or
/// `MOVA_REFLECTION_WARNINGS=0` directly), regardless of
/// `*warn-on-reflection*` in loaded code. Read once via `OnceLock`, same
/// discipline as `MOVA_EXPLAIN` (`compile::explain::explain_enabled`):
/// zero cost on the default path, and skips the analysis pass entirely
/// (saves load time too).
pub(crate) fn force_disabled() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| std::env::var("MOVA_REFLECTION_WARNINGS").is_ok_and(|v| v == "0"))
}

pub(crate) fn analyze_top_level(interp: &mut Interp, form: &Form) {
    if force_disabled() {
        return;
    }
    let warn_reflection = interp
        .globals
        .get(&Symbol::simple("*warn-on-reflection*"))
        .is_some_and(|v| v.truthy());
    let warn_boxed = matches!(
        interp.globals.get(&Symbol::simple("*unchecked-math*")),
        Some(Value::Keyword(k)) if k.as_ref() == "warn-on-boxed"
    );
    if !warn_reflection && !warn_boxed {
        return;
    }
    let mut ctx = Ctx {
        interp,
        warn_reflection,
        warn_boxed,
        fn_return_hints: HashMap::new(),
    };
    let mut hints = HintEnv::new();
    walk(&mut ctx, form, &mut hints);
}

fn emit_reflection(ctx: &mut Ctx, span: Span, reason: &str) {
    let (line, col) = crate::error::line_col(ctx.interp.source.as_ref(), span.start);
    let msg = format!(
        "Reflection warning, {}:{}:{} - {}.\n",
        ctx.interp.source_name, line, col, reason
    );
    crate::builtins::nsfns::write_shim_err(ctx.interp, &msg);
}

fn emit_boxed(ctx: &mut Ctx, span: Span, call_desc: &str) {
    let (line, col) = crate::error::line_col(ctx.interp.source.as_ref(), span.start);
    let msg = format!(
        "Boxed math warning, {}:{}:{} - call: {}.\n",
        ctx.interp.source_name, line, col, call_desc
    );
    crate::builtins::nsfns::write_shim_err(ctx.interp, &msg);
}

/// Recursive walker over already-read `Form`s (pre-macroexpansion syntax:
/// `defn`/`fn`/`let`/`do`/`if`/`loop` are handled directly here because
/// every one of them is a REAL mova special form already recognized at
/// this same raw-syntax level -- see `eval::special_forms`'s own dispatch
/// -- except `defn`, a `core.mova` macro, handled directly below rather
/// than requiring macroexpansion first, since its shape (`name doc? attr?
/// ([params] body)+ attr?`) is simple and fixed). Any OTHER list is walked
/// generically (recurse into every element) -- correct and safe: a
/// mis-recognized/unfamiliar form just gets its subforms visited in case
/// they contain a dot-form/ctor-form/static-call/arithmetic-call of
/// interest, never mis-warned on itself.
fn walk(ctx: &mut Ctx, form: &Form, hints: &mut HintEnv) {
    match &form.value {
        FormValue::List(items) => walk_list(ctx, items, form.span, hints),
        FormValue::Vector(items) | FormValue::Set(items) => {
            for f in items {
                walk(ctx, f, hints);
            }
        }
        FormValue::Map(pairs) => {
            for (k, v) in pairs {
                walk(ctx, k, hints);
                walk(ctx, v, hints);
            }
        }
        FormValue::Atom(_) => {}
    }
}

fn walk_list(ctx: &mut Ctx, items: &[Form], span: Span, hints: &mut HintEnv) {
    if items.is_empty() {
        return;
    }
    if let Some(head) = as_symbol(&items[0]) {
        let name = head.name.as_ref();
        if head.ns.is_none() {
            match name {
                "quote" => return,
                "fn" | "fn*" => {
                    walk_fn_like(ctx, &items[1..], None, hints);
                    return;
                }
                "defn" | "defn-" => {
                    walk_defn(ctx, &items[1..], hints);
                    return;
                }
                // kondo-wave: `defmethod`'s trailing `[params] body` is a
                // real fn arity for hinting purposes -- e.g. `(defmethod
                // print-method SomeClass [o ^java.io.Writer w] (.write w
                // ...))` resolves `.write` on the real JVM because `w`'s
                // hint is in scope; without this case it fell through to
                // the generic walk below, which never registers `w`'s
                // hint, so `.write` looked unresolvable and false-
                // positived a reflection warning the oracle never emits.
                "defmethod" => {
                    walk_defmethod(ctx, &items[1..], hints);
                    return;
                }
                // kondo-wave: `deftype`/`defrecord` field TAGS are real
                // hints for every method body's implicit lexical field
                // access (mova/real Clojure both expose deftype fields
                // as plain lexical locals inside the type's own method
                // bodies, not through a `this.field` dot-form) -- e.g.
                // `(deftype R [^InputStream is] Closeable (close [this]
                // (.close is)))`'s `is` needs `is`'s field tag in scope
                // for `.close` to resolve on the real JVM. Without this
                // case, field tags were never registered anywhere and
                // every field reference fell through as `Hint::Unknown`,
                // false-positiving a warning the oracle never emits.
                "deftype" | "deftype*" | "defrecord" => {
                    walk_deftype(ctx, &items[1..], hints);
                    return;
                }
                "let" | "loop" | "loop*" | "when-let" | "if-let" => {
                    // kondo-wave: `when-let`/`if-let` have the exact same
                    // `[sym init]` binding-vector shape as `let` at the
                    // raw-form level this module walks (pre-macroexpand)
                    // -- reusing `walk_let` registers the binding's own
                    // `^Tag` (or inferred ctor hint) for the body, e.g.
                    // `(when-let [^StringBuilder buffer ...] (.append
                    // buffer ...))`. Slightly loose for `if-let`'s ELSE
                    // branch (walked in the same scope, though the
                    // binding isn't really live there) -- never a false
                    // POSITIVE, only a possible missed warning, which
                    // matches this module's existing silent-bias design.
                    walk_let(ctx, &items[1..], hints);
                    return;
                }
                "try" => {
                    walk_try(ctx, &items[1..], hints);
                    return;
                }
                "doto" => {
                    walk_doto(ctx, &items[1..], hints);
                    return;
                }
                "do" => {
                    for f in &items[1..] {
                        walk(ctx, f, hints);
                    }
                    return;
                }
                "if" => {
                    for f in &items[1..] {
                        walk(ctx, f, hints);
                    }
                    return;
                }
                "def" => {
                    if items.len() > 2 {
                        walk(ctx, &items[2], hints);
                    }
                    return;
                }
                _ => {}
            }
            if name.len() > 1 && name.starts_with('.') && name != ".." {
                if ctx.warn_reflection {
                    check_dot_form(ctx, name, &items[1..], span, hints);
                }
                for f in &items[1..] {
                    walk(ctx, f, hints);
                }
                return;
            }
            if name.len() > 1 && name.ends_with('.') && name != ".." {
                if ctx.warn_reflection {
                    check_ctor_form(ctx, name, &items[1..], span, hints);
                }
                for f in &items[1..] {
                    walk(ctx, f, hints);
                }
                return;
            }
            if ctx.warn_boxed && is_boxed_math_op(name) {
                check_boxed_math(ctx, name, &items[1..], span, hints);
            }
        } else if let Some(ns) = &head.ns {
            if ctx.warn_reflection {
                check_static_form(ctx, ns.as_ref(), name, &items[1..], span, hints);
            }
        }
    }
    for f in items {
        walk(ctx, f, hints);
    }
}

fn is_boxed_math_op(name: &str) -> bool {
    matches!(
        name,
        "inc" | "dec" | "+" | "-" | "*" | "quot" | "rem" | "mod" | "<" | ">" | "<=" | ">=" | "=="
    )
}

/// A JVM-shaped signature description for the honest "call: ..." text --
/// matches the oracle's own shape (`unchecked_inc(java.lang.Object)`,
/// `unchecked_add(java.lang.Object,java.lang.Object)`, `lt(java.lang.
/// Object,long)`) closely enough to be a genuine description of what mova
/// itself would have to dispatch dynamically here, but the suite only
/// `re-find`s the `"^Boxed math warning"` prefix (measured: `numbers.clj`'s
/// `check-warn-on-box`), so exact spelling beyond the prefix is not
/// suite-load-bearing.
fn boxed_call_desc(op: &str, arg_hints: &[Hint]) -> String {
    let native = match op {
        "inc" => "unchecked_inc",
        "dec" => "unchecked_dec",
        "+" => "unchecked_add",
        "-" => "unchecked_minus",
        "*" => "unchecked_multiply",
        "<" => "lt",
        ">" => "gt",
        "<=" => "lte",
        ">=" => "gte",
        "==" => "equiv",
        "quot" => "quotient",
        "rem" => "remainder",
        "mod" => "modulus",
        other => other,
    };
    let params: Vec<String> = arg_hints
        .iter()
        .map(|h| match h {
            Hint::Unknown => "java.lang.Object".to_string(),
            Hint::Prim(p) => p.to_string(),
            Hint::Class(c) => c.to_string(),
            Hint::OtherClass => "java.lang.Object".to_string(),
        })
        .collect();
    format!("clojure.lang.Numbers.{native}({})", params.join(","))
}

fn check_boxed_math(ctx: &mut Ctx, op: &str, args: &[Form], span: Span, hints: &HintEnv) {
    let arg_hints: Vec<Hint> = args.iter().map(|a| infer_hint(ctx, a, hints)).collect();
    let any_boxed = arg_hints.iter().any(|h| !matches!(h, Hint::Prim(_)));
    if any_boxed {
        let desc = boxed_call_desc(op, &arg_hints);
        emit_boxed(ctx, span, &desc);
    }
}

fn check_dot_form(ctx: &mut Ctx, name: &str, args: &[Form], span: Span, hints: &HintEnv) {
    if args.is_empty() {
        return;
    }
    let field_only = name.starts_with(".-");
    let field = name
        .strip_prefix(".-")
        .or_else(|| name.strip_prefix('.'))
        .unwrap_or(name);
    if field_only {
        // Not exercised by any targeted row (real `.-field` on a class
        // with no public instance fields is a hard error, not a
        // reflection warning) -- deliberately silent rather than guess.
        return;
    }
    let target_hint = infer_hint(ctx, &args[0], hints);
    let extra = &args[1..];
    let arity = extra.len();
    match target_hint {
        Hint::Unknown => {
            if arity == 0 {
                emit_reflection(ctx, span, &format!("reference to field {field} can't be resolved"));
            } else {
                emit_reflection(
                    ctx,
                    span,
                    &format!("call to method {field} can't be resolved (target class is unknown)"),
                );
            }
        }
        Hint::Class(cls) => {
            // Only `java.lang.String`/`java.math.BigDecimal` are tabulated
            // with a real instance-method signature set below; any other
            // class (including ones the leak fallback above just minted)
            // resolves fine on the real JVM by definition (it's a real
            // hint) and mova simply doesn't model its members -- stay
            // silent rather than guess, per this module's own doc.
            if !matches!(cls, "java.lang.String" | "java.math.BigDecimal") {
                return;
            }
            let all_sigs = string_or_bigdecimal_instance_sigs(cls, field);
            let sigs = sigs_of_arity(&all_sigs, arity);
            let arg_hints: Vec<Hint> = extra.iter().map(|a| infer_hint(ctx, a, hints)).collect();
            match resolve_member(&sigs, &arg_hints) {
                Resolution::Resolve => {}
                Resolution::NoSuchMember => {
                    if arity == 0 {
                        emit_reflection(
                            ctx,
                            span,
                            &format!("reference to field {field} on {cls} can't be resolved"),
                        );
                    } else {
                        emit_reflection(
                            ctx,
                            span,
                            &format!("call to method {field} on {cls} can't be resolved (no such method)"),
                        );
                    }
                }
                Resolution::Ambiguous => {
                    if arity == 0 {
                        emit_reflection(
                            ctx,
                            span,
                            &format!("reference to field {field} on {cls} can't be resolved"),
                        );
                    } else {
                        let types = arg_hints.iter().map(|h| h.describe()).collect::<Vec<_>>().join(", ");
                        emit_reflection(
                            ctx,
                            span,
                            &format!(
                                "call to method {field} on {cls} can't be resolved (argument types: {types})"
                            ),
                        );
                    }
                }
            }
        }
        Hint::Prim(_) => {}
        // A real hint, just for a class this module keeps no member table
        // for -- real `javac` resolves the call off the hint alone, so
        // this module stays silent rather than guess at "no such member"
        // from a table that was never meant to cover this class.
        Hint::OtherClass => {}
    }
}

fn string_or_bigdecimal_instance_sigs(cls: &str, field: &str) -> Vec<&'static [ParamKind]> {
    match cls {
        "java.lang.String" => string_instance_sigs(field).to_vec(),
        "java.math.BigDecimal" => bigdecimal_instance_sigs(field).to_vec(),
        _ => Vec::new(),
    }
}

fn check_ctor_form(ctx: &mut Ctx, name: &str, args: &[Form], span: Span, hints: &HintEnv) {
    let bare = name.strip_suffix('.').unwrap_or(name);
    let Some(full_cls) = resolve_class_name(bare) else {
        return;
    };
    let arity = args.len();
    let sigs = match full_cls {
        "java.lang.String" => string_ctor_sigs(arity),
        "java.lang.Object" => object_ctor_sigs(arity),
        _ => return,
    };
    let arg_hints: Vec<Hint> = args.iter().map(|a| infer_hint(ctx, a, hints)).collect();
    if let Resolution::Resolve = resolve_member(&sigs, &arg_hints) {
        return;
    }
    emit_reflection(ctx, span, &format!("call to {full_cls} ctor can't be resolved"));
}

fn check_static_form(ctx: &mut Ctx, ns: &str, name: &str, args: &[Form], span: Span, hints: &HintEnv) {
    let Some(cls) = resolve_class_name(ns) else {
        return;
    };
    let all_sigs: Vec<&'static [ParamKind]> = match cls {
        "java.lang.Integer" => integer_static_sigs(name).to_vec(),
        _ => Vec::new(),
    };
    if all_sigs.is_empty() {
        return;
    }
    let arity = args.len();
    let sigs = sigs_of_arity(&all_sigs, arity);
    let arg_hints: Vec<Hint> = args.iter().map(|a| infer_hint(ctx, a, hints)).collect();
    match resolve_member(&sigs, &arg_hints) {
        Resolution::Resolve => {}
        Resolution::NoSuchMember => {
            emit_reflection(
                ctx,
                span,
                &format!("call to static method {name} on {cls} can't be resolved (no such method)"),
            );
        }
        Resolution::Ambiguous => {
            let types = arg_hints.iter().map(|h| h.describe()).collect::<Vec<_>>().join(", ");
            emit_reflection(
                ctx,
                span,
                &format!("call to static method {name} on {cls} can't be resolved (argument types: {types})"),
            );
        }
    }
}

/// Infers a form's static [`Hint`] without evaluating anything: an
/// explicit `^Tag` on the form always wins (matches `numbers.clj`'s
/// EXPRESSION-level meta-hints, e.g. `^long (first (range 3))`); otherwise
/// literal atoms get their real JVM literal type (`"s"` -> `String`, `#"x"`
/// -> `Pattern`, `1M` -> `BigDecimal`, a plain integer -> `long`, matching
/// the spec's measured "literal types" rule); a bound symbol reads its
/// scope hint; a ctor-form's result is that ctor's class (flows through a
/// `let` init the same way, via [`walk_let`]); a call to a `defn` with a
/// known return-type hint (THIS SAME top-level form only, see [`Ctx`]'s
/// doc) inherits that hint; anything else is `Unknown`.
fn infer_hint(ctx: &Ctx, form: &Form, hints: &HintEnv) -> Hint {
    if let Some(tag) = extract_tag_name(form) {
        return hint_from_tag_name(&tag);
    }
    match &form.value {
        FormValue::Atom(Value::Str(_)) => Hint::Class("java.lang.String"),
        FormValue::Atom(Value::Regex(_)) => Hint::Class("java.util.regex.Pattern"),
        FormValue::Atom(Value::BigDec(_)) => Hint::Class("java.math.BigDecimal"),
        FormValue::Atom(Value::Int(_)) => Hint::Prim("long"),
        FormValue::Atom(Value::Sym(s)) if s.ns.is_none() => {
            if let Some(h) = hints.get(&s.name) {
                return *h;
            }
            // A GLOBAL var's own `:tag` meta (`(def ^Pattern int-pattern
            // #"...")`) is a real hint too, visible from ANY later
            // top-level form -- real Clojure's compiler sees it off the
            // already-interned Var, not just within the defining form.
            // `try_resolve_var_cell` is read-only (never auto-interns a
            // placeholder, unlike `resolve_var_cell`/`#'sym`), so a bare
            // symbol that ISN'T a var either (a typo, a destructured
            // binding this module didn't track) safely falls through to
            // `Unknown` with no side effect.
            global_var_hint(ctx, &s.name).unwrap_or(Hint::Unknown)
        }
        FormValue::List(items) if !items.is_empty() => {
            if let Some(head) = as_symbol(&items[0]) {
                let name = head.name.as_ref();
                // `(doto x forms...)` evaluates to `x` itself -- its
                // hint IS `x`'s hint (recursively, so `(doto (Foo.) ...)`
                // flows the ctor's class through same as a `let` init).
                if head.ns.is_none() && name == "doto" && items.len() > 1 {
                    return infer_hint(ctx, &items[1], hints);
                }
                // `(if test then else)`: real Clojure's local-clearing
                // analyzer merges both branches' static types, and a
                // caller sees the merged type when both branches agree
                // (`(if negate? (.negate bn) bn)` -- both branches are
                // `bn`'s own class). No `else` (nil possible) or
                // disagreeing branches genuinely IS unknown -- never
                // guess, matching `Hint::Unknown`'s own contract of only
                // ever widening, never narrowing.
                if head.ns.is_none() && name == "if" && items.len() >= 3 {
                    let then_hint = infer_hint(ctx, &items[2], hints);
                    return if items.len() > 3 {
                        merge_hints(then_hint, infer_hint(ctx, &items[3], hints))
                    } else {
                        Hint::Unknown
                    };
                }
                if head.ns.is_none() && name.len() > 1 && name.ends_with('.') && name != ".." {
                    let bare = name.strip_suffix('.').unwrap_or(name);
                    // kondo-wave: ANY ctor call's result is a real
                    // instance of ITS OWN class, tabulated or not (a
                    // `(StringBuilder.)` result is genuinely a
                    // `StringBuilder`, even though this module has no
                    // method table for it) -- same leak-fallback logic
                    // as `hint_from_tag_name`, so a chained `.append`/
                    // etc on the fresh instance stays silent instead of
                    // reading as `Hint::Unknown`.
                    return hint_from_tag_name(bare);
                }
                if head.ns.is_none() {
                    if let Some(h) = ctx.fn_return_hints.get(&head.name) {
                        return *h;
                    }
                    if let Some(h) = builtin_return_hint(name) {
                        return h;
                    }
                    if let Some(h) = global_fn_return_hint(ctx, &head.name) {
                        return h;
                    }
                    // `(.method target ...)`: when the TARGET is a real,
                    // hinted class (modeled or not), the real JVM resolves
                    // this call statically off THAT hint (whatever member
                    // table this module does or doesn't have for it), so
                    // its result is likewise a real, if untabulated,
                    // class -- `Hint::OtherClass`, so a further chained
                    // `.method` on the result (`(.matches (.matcher p
                    // s))`) stays silent too. When the target is
                    // `Unknown`, this call is ALSO reflective on the real
                    // JVM, so its result stays `Unknown` -- never widen a
                    // truly unknown chain into a false "resolved".
                    if name.len() > 1 && name.starts_with('.') && name != ".." && items.len() > 1
                    {
                        return match infer_hint(ctx, &items[1], hints) {
                            Hint::Unknown => Hint::Unknown,
                            _ => Hint::OtherClass,
                        };
                    }
                } else if let Some(ns) = &head.ns {
                    // A qualified call whose namespace segment starts with
                    // an uppercase letter is, by the same convention every
                    // Clojure static-analysis tool relies on (`clojure.
                    // core` namespaces are never capitalized), a JAVA
                    // CLASS reference (`Class/forName`, `Integer/valueOf`,
                    // `TimeZone/getTimeZone`), never a Clojure namespace
                    // alias -- so a call through it resolves on the real
                    // JVM the same way a ctor call does, tabulated or not.
                    if ns.as_ref().chars().next().is_some_and(|c| c.is_ascii_uppercase()) {
                        return Hint::OtherClass;
                    }
                }
            }
            Hint::Unknown
        }
        _ => Hint::Unknown,
    }
}

/// Extracts `(name, params-form, body)` groups from a `fn`/`fn*`/`defn`
/// arg list -- tolerant of an optional leading name symbol (recursive
/// `fn`), a single `[params] body...` arity, or one-or-more `([params]
/// body...)` arities. Anything that doesn't fit this shape yields no
/// groups (the caller still gets to walk whatever IS there generically
/// via its own fallback), never an error -- this module never fails a
/// build, it only sometimes fails to warn.
fn split_arities(args: &[Form]) -> Vec<(&[Form], &[Form])> {
    if args.is_empty() {
        return Vec::new();
    }
    if let FormValue::Vector(params) = &args[0].value {
        return vec![(params.as_slice(), &args[1..])];
    }
    let mut out = Vec::new();
    for a in args {
        if let FormValue::List(inner) = &a.value {
            if let Some(first) = inner.first() {
                if let FormValue::Vector(params) = &first.value {
                    out.push((params.as_slice(), &inner[1..]));
                    continue;
                }
            }
        }
        // Not an arity group (e.g. a trailing attr-map on `defn`) --
        // ignore it, don't misparse it as one.
    }
    out
}

fn param_hints(params: &[Form]) -> (HashMap<Str, Hint>, Vec<Str>) {
    let mut out = HashMap::new();
    let mut names = Vec::new();
    for p in params {
        if let FormValue::Atom(Value::Sym(s)) = &p.value {
            if s.name.as_ref() == "&" {
                continue; // rest-arg marker, next param is the rest binding
            }
            let hint = extract_tag_name(p).map(|t| hint_from_tag_name(&t)).unwrap_or(Hint::Unknown);
            out.insert(s.name.clone(), hint);
            names.push(s.name.clone());
        }
        // Destructuring param patterns: no hint extracted, safe default.
    }
    (out, names)
}

fn walk_fn_like(ctx: &mut Ctx, args: &[Form], return_hint: Option<Hint>, hints: &mut HintEnv) {
    let args = if let Some(first) = args.first() {
        if as_symbol(first).is_some() {
            &args[1..]
        } else {
            args
        }
    } else {
        args
    };
    for (params, body) in split_arities(args) {
        let (param_hint_map, _names) = param_hints(params);
        let mut scope = hints.clone();
        scope.extend(param_hint_map);
        for b in body {
            walk(ctx, b, &mut scope);
        }
    }
    let _ = return_hint; // reserved: return-type hints are registered by `walk_defn`, not here.
}

/// kondo-wave: `(defmethod name dispatch-val [params] body...)` -- the
/// dispatch-val form is walked generically (it's an ordinary expression),
/// then `[params] body...` goes through the exact same arity/hint
/// machinery `walk_fn_like` gives `fn`/`defn`.
/// kondo-wave: `(deftype/defrecord Name [fields...] spec-or-method...)`
/// -- registers each field's own `^Tag` as a hint visible in every
/// method body (see the `walk_list` case's doc for why), then walks
/// each `(method-name [params] body...)` spec with field hints as the
/// base scope, params layered on top (shadowing a same-named field,
/// exactly like real lexical scoping). A bare protocol/interface name
/// spec (`Reader`, `Closeable`, `Object`, ...) has no params vector, so
/// it falls through to a plain generic walk instead.
fn walk_deftype(ctx: &mut Ctx, args: &[Form], hints: &mut HintEnv) {
    if args.len() < 2 {
        for f in args {
            walk(ctx, f, hints);
        }
        return;
    }
    let mut base = hints.clone();
    if let FormValue::Vector(fields) = &args[1].value {
        let (field_hint_map, _names) = param_hints(fields);
        base.extend(field_hint_map);
    }
    for spec in &args[2..] {
        if let FormValue::List(items) = &spec.value {
            if items.len() >= 2 {
                if let FormValue::Vector(params) = &items[1].value {
                    let (param_hint_map, _names) = param_hints(params);
                    let mut scope = base.clone();
                    scope.extend(param_hint_map);
                    for b in &items[2..] {
                        walk(ctx, b, &mut scope);
                    }
                    continue;
                }
            }
        }
        walk(ctx, spec, &mut base);
    }
}

fn walk_defmethod(ctx: &mut Ctx, args: &[Form], hints: &mut HintEnv) {
    if args.len() < 3 {
        for f in args {
            walk(ctx, f, hints);
        }
        return;
    }
    walk(ctx, &args[1], hints);
    walk_fn_like(ctx, &args[2..], None, hints);
}

fn walk_defn(ctx: &mut Ctx, args: &[Form], hints: &mut HintEnv) {
    if args.is_empty() {
        return;
    }
    let name_form = &args[0];
    let Some(name_sym) = as_symbol(name_form) else {
        // Malformed to this module's eyes -- still walk whatever's there.
        for f in args {
            walk(ctx, f, hints);
        }
        return;
    };
    let return_hint = extract_tag_name(name_form).map(|t| hint_from_tag_name(&t));
    if let Some(rh) = return_hint {
        ctx.fn_return_hints.insert(name_sym.name.clone(), rh);
    }
    // Skip an optional docstring and/or leading attr-map before the
    // arity group(s), same shape `eval_defmacro`'s own
    // `extract_macro_doc_and_attrs` recognizes for real (this module
    // just needs to not misparse them as an arity, not fully replicate
    // that fn).
    let mut rest = &args[1..];
    if let Some(FormValue::Atom(Value::Str(_))) = rest.first().map(|f| &f.value) {
        rest = &rest[1..];
    }
    if let Some(FormValue::Map(_)) = rest.first().map(|f| &f.value) {
        rest = &rest[1..];
    }
    walk_fn_like(ctx, rest, None, hints);
}

fn walk_let(ctx: &mut Ctx, args: &[Form], hints: &mut HintEnv) {
    let Some(bindings_form) = args.first() else {
        return;
    };
    let FormValue::Vector(pairs) = &bindings_form.value else {
        for f in args {
            walk(ctx, f, hints);
        }
        return;
    };
    let mut scope = hints.clone();
    let mut i = 0;
    while i + 1 < pairs.len() {
        let target = &pairs[i];
        let init = &pairs[i + 1];
        walk(ctx, init, &mut scope);
        if let FormValue::Atom(Value::Sym(s)) = &target.value {
            if s.ns.is_none() {
                let hint = extract_tag_name(target)
                    .map(|t| hint_from_tag_name(&t))
                    .unwrap_or_else(|| infer_hint(ctx, init, &scope));
                scope.insert(s.name.clone(), hint);
            }
        }
        i += 2;
    }
    for body_form in &args[1..] {
        walk(ctx, body_form, &mut scope);
    }
}

/// `(try body... (catch ExcClass e handler...) ... (finally ...))` --
/// each `catch` binding is ALWAYS typed by its declared exception class
/// (real `javac` never reflects on it), so it's registered as a hint,
/// modeled or not, exactly like any other explicit `^Tag`.
fn walk_try(ctx: &mut Ctx, items: &[Form], hints: &mut HintEnv) {
    for item in items {
        if let FormValue::List(inner) = &item.value {
            if let Some(head) = inner.first().and_then(as_symbol) {
                if head.ns.is_none() && head.name.as_ref() == "catch" && inner.len() >= 3 {
                    let exc_hint = as_symbol(&inner[1])
                        .map(|s| hint_from_tag_name(s.name.as_ref()))
                        .unwrap_or(Hint::Unknown);
                    if let Some(bind) = as_symbol(&inner[2]) {
                        if bind.ns.is_none() {
                            let mut scope = hints.clone();
                            scope.insert(bind.name.clone(), exc_hint);
                            for b in &inner[3..] {
                                walk(ctx, b, &mut scope);
                            }
                            continue;
                        }
                    }
                } else if head.ns.is_none() && head.name.as_ref() == "finally" {
                    for b in &inner[1..] {
                        walk(ctx, b, hints);
                    }
                    continue;
                }
            }
        }
        walk(ctx, item, hints);
    }
}

/// `(doto x (.method a b) (.-field) form...)` -- each `(.method ...)`/
/// `(.-field)` body form is threading SUGAR missing its own receiver (the
/// macro splices `x` in as the first real arg on expansion), so walking
/// it as an ordinary form would wrongly treat its own first arg as the
/// dot-form's target. Rebuilds the real `(.method x a b)` shape before
/// checking it, and walks everything else (including `x` itself) as
/// usual. `infer_hint`'s own `doto` case (this call's RESULT is `x`) is
/// separate and unaffected by this.
fn walk_doto(ctx: &mut Ctx, args: &[Form], hints: &mut HintEnv) {
    let Some(target) = args.first() else {
        return;
    };
    walk(ctx, target, hints);
    for f in &args[1..] {
        if let FormValue::List(inner) = &f.value {
            if let Some(head) = inner.first().and_then(as_symbol) {
                let name = head.name.as_ref();
                if head.ns.is_none() && name.len() > 1 && name.starts_with('.') && name != ".." {
                    if ctx.warn_reflection {
                        let mut full_args = Vec::with_capacity(inner.len());
                        full_args.push(target.clone());
                        full_args.extend(inner[1..].iter().cloned());
                        check_dot_form(ctx, name, &full_args, f.span, hints);
                    }
                    for a in &inner[1..] {
                        walk(ctx, a, hints);
                    }
                    continue;
                }
            }
        }
        walk(ctx, f, hints);
    }
}
