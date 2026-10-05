//! `RjError` + miette rendering. Every reader/runtime error carries an
//! optional labeled source span; runtime errors carry an mova call stack;
//! system errors carry errno + the failing syscall name (ARCHITECTURE.md).

use std::fmt;

use miette::{Diagnostic, GraphicalReportHandler, LabeledSpan, NamedSource, SourceCode};

use crate::eval::Frame;
use crate::reader::Span;
use crate::value::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    Reader,
    Arity,
    TypeErr,
    Unresolved,
    DivideByZero,
    /// S5: `java.lang.ArithmeticException` that is not a division by zero
    /// (`long overflow`, non-terminating decimal expansion). See
    /// [`RjError::arithmetic`].
    Arithmetic,
    Sys,
    Thrown,
    Recur,
    /// The embedding-fuel budget (`Interp::fuel`, see `eval::Interp`'s field
    /// doc) reached zero. Deliberately its OWN kind, not `Other`: like
    /// `Recur`, it must unwind straight through a script-level `(try ..
    /// (catch ..))` rather than being caught there (see `eval_try`/
    /// `compile::exec::exec_try`'s explicit exclusion) -- an untrusted
    /// script must not be able to swallow its own exhaustion signal and keep
    /// running. The HOST embedding mova still sees it as an ordinary `Err`
    /// from `eval_str`/`eval_form`/`call`.
    FuelExhausted,
    /// P0c: nREPL stage-1 interrupt. Catchable (class InterruptedException).
    Interrupted,
    /// P0c: nREPL stage-2 interrupt. Not catchable; `finally` runs.
    InterruptedHard,
    Other,
}

/// W3a: the JVM exception class an internal `RjError` presents as to a
/// typed `catch` clause (and therefore to `clojure.test`'s class-aware
/// `thrown?`/`thrown-with-msg?` family), when its `ErrorKind` alone is too
/// coarse to say. `ErrorKind` is mova's OWN taxonomy -- deliberately
/// small, and shared by call sites real Clojure resolves to different JVM
/// classes: one `ErrorKind::TypeErr` covers both "wrong type, JVM throws
/// `ClassCastException`" and "wrong type, JVM throws
/// `IllegalArgumentException`" and "argument was nil, JVM throws
/// `NullPointerException`"; one `ErrorKind::Other` covers everything from
/// `(pop [])` (`IllegalStateException`) to `(nth {} 0)`
/// (`UnsupportedOperationException`). Rather than widen `ErrorKind` (which
/// every `match` in the crate would have to grow arms for, and which would
/// still be a guess at sites nobody measured) or sniff `message` substrings
/// (the C3g stopgap, explicitly documented there as not extensible past
/// two patterns), a site that KNOWS -- because it was measured against the
/// 1.13.0-alpha6 oracle -- which JVM class real Clojure raises for exactly
/// that condition tags its error with it here.
///
/// Every variant's `chain` below is the class's REAL JVM ancestry, read
/// off `.getSuperclass` at oracle-measure time (transcript in the W3a
/// landing commit), not hand-guessed -- `StringIndexOutOfBoundsException`
/// really does extend `IndexOutOfBoundsException`, `NumberFormatException`
/// really does extend `IllegalArgumentException`, and `IllegalAccessError`
/// really is an `Error`, NOT an `Exception` (so a `(catch Exception e ..)`
/// correctly does NOT catch it -- the one variant here whose chain does
/// not end `.. Exception Throwable`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JvmClass {
    /// The same class `ErrorKind::TypeErr` already defaults to -- named
    /// explicitly for sites that want to say "measured: THIS one is
    /// genuinely a cast failure" next to a sibling that is not.
    ClassCast,
    IllegalArgument,
    NullPointer,
    UnsupportedOperation,
    IndexOutOfBounds,
    ArrayIndexOutOfBounds,
    StringIndexOutOfBounds,
    IllegalState,
    IllegalAccess,
    /// W3d2: `java.lang.AbstractMethodError` -- what the JVM raises for a
    /// method the receiver's class does not define or inherit. Measured on
    /// 1.13.0-alpha6 for BOTH shapes `protocols.clj`'s `reify-test` and
    /// `protocols-test` assert: an unimplemented method NAME on a `reify`
    /// (`(.add (reify java.util.List (contains ..)) :x)`) and an
    /// unimplemented ARITY of a protocol method (`(baz (reify P (baz [_ o]
    /// ..)))`). Like `IllegalAccess` above and for the same reason, this is
    /// an `Error`, NOT an `Exception`.
    AbstractMethod,
    /// `clojure.lang.Compiler$CompilerException` -- what real Clojure
    /// wraps EVERY compile/macroexpand-time failure in before it reaches
    /// user code (measured: `(eval 'bar)`, `(eval '(defrecord R [:k]))`,
    /// `(eval '(case 0 1 :x 1 :y))` all surface as this, with the actual
    /// condition as `.getCause`).
    CompilerException,
    /// `clojure.lang.ExceptionInfo` -- for a condition real Clojure
    /// reports via `ex-info`, most notably the `clojure.spec` macro-arglist
    /// failures behind `fn`/`defn`/`let` ("Call to clojure.core/fn did not
    /// conform to spec.").
    ExceptionInfo,
    /// `java.util.NoSuchElementException` -- what real `StringTokenizer.
    /// nextToken()`/`Iterator.next()` throw once exhausted (measured:
    /// extends `RuntimeException` directly, no intermediate class).
    NoSuchElement,
    /// `java.lang.NumberFormatException` (an `IllegalArgumentException`): `Integer/parseInt "x"`.
    NumberFormat,
    /// `java.io.FileNotFoundException`: `(require 'no.such.ns)`.
    FileNotFound,
}

impl JvmClass {
    /// Own class first, then each superclass up to `Throwable` -- the shape
    /// `eval::special_forms::catch_class_matches` matches a written `catch`
    /// class-name symbol against.
    pub fn chain(self) -> &'static [&'static str] {
        const RT: &str = "java.lang.RuntimeException";
        const EX: &str = "java.lang.Exception";
        const TH: &str = "java.lang.Throwable";
        match self {
            JvmClass::ClassCast => &["java.lang.ClassCastException", RT, EX, TH],
            JvmClass::IllegalArgument => &["java.lang.IllegalArgumentException", RT, EX, TH],
            JvmClass::NullPointer => &["java.lang.NullPointerException", RT, EX, TH],
            JvmClass::UnsupportedOperation => &["java.lang.UnsupportedOperationException", RT, EX, TH],
            JvmClass::IndexOutOfBounds => &["java.lang.IndexOutOfBoundsException", RT, EX, TH],
            JvmClass::ArrayIndexOutOfBounds => &[
                "java.lang.ArrayIndexOutOfBoundsException",
                "java.lang.IndexOutOfBoundsException",
                RT,
                EX,
                TH,
            ],
            JvmClass::StringIndexOutOfBounds => &[
                "java.lang.StringIndexOutOfBoundsException",
                "java.lang.IndexOutOfBoundsException",
                RT,
                EX,
                TH,
            ],
            JvmClass::IllegalState => &["java.lang.IllegalStateException", RT, EX, TH],
            // NOT an `Exception`: `IllegalAccessError <: IncompatibleClass
            // ChangeError <: LinkageError <: Error <: Throwable` (measured).
            JvmClass::IllegalAccess => &[
                "java.lang.IllegalAccessError",
                "java.lang.IncompatibleClassChangeError",
                "java.lang.LinkageError",
                "java.lang.Error",
                TH,
            ],
            // Same `Error` (not `Exception`) ancestry as `IllegalAccess`
            // above -- measured: `AbstractMethodError <:
            // IncompatibleClassChangeError <: LinkageError <: Error <:
            // Throwable`.
            JvmClass::AbstractMethod => &[
                "java.lang.AbstractMethodError",
                "java.lang.IncompatibleClassChangeError",
                "java.lang.LinkageError",
                "java.lang.Error",
                TH,
            ],
            JvmClass::CompilerException => &["clojure.lang.Compiler$CompilerException", RT, EX, TH],
            JvmClass::ExceptionInfo => &["clojure.lang.ExceptionInfo", RT, EX, TH],
            JvmClass::NoSuchElement => &["java.util.NoSuchElementException", RT, EX, TH],
            JvmClass::FileNotFound => &["java.io.FileNotFoundException", "java.io.IOException", EX, TH],
            JvmClass::NumberFormat => &["java.lang.NumberFormatException", "java.lang.IllegalArgumentException", RT, EX, TH],
        }
    }
}

/// `Clone` (v0.2 / A1): a `Value::Future`'s cell stores the full `RjError`
/// on failure (see `value::FutureState::Failed`) so every `deref` of that
/// future -- there may be more than one -- can re-propagate an equivalent
/// error without moving it out of the `Mutex`-guarded cell.
#[derive(Debug, Clone)]
pub struct RjError {
    pub kind: ErrorKind,
    pub message: String,
    pub span: Option<Span>,
    /// field5/W-SPAN rider: `Interp::source_id` stamped by [`with_stack`]
    /// (the near-universal choke point every error already passes through
    /// with `Interp`/`interp` in scope) at the same moment the call stack
    /// is attached -- `span` was almost always constructed a line or two
    /// earlier in the SAME call, against whatever buffer was current then,
    /// so this is the best available answer to "which buffer does `span`
    /// belong to" without threading an id through every one of `with_span`'s
    /// ~300 call sites. [`UNKNOWN_SOURCE`](crate::source_registry::UNKNOWN_SOURCE)
    /// (the default) means "not stamped" -- `error::render` falls back to
    /// its caller-supplied `(source_name, source)` pair, exactly the old
    /// best-effort behavior.
    pub span_source_id: u32,
    pub label: Option<String>,        // what to print under the span caret
    pub stack: Vec<Frame>,            // innermost-last mova frames
    pub errno: Option<(i32, String, String)>, // (errno, strerror, syscall name)
    pub thrown: Option<Value>,        // for user (throw v)
    /// C3c (errors.clj's `arity-exception` deftest): the actual argument
    /// count for an `ErrorKind::Arity` error -- what real `clojure.lang.
    /// ArityException`'s own `actual` field carries (measured via
    /// `.getFields`: the class has exactly TWO public fields, `actual
    /// int` and `name String` -- there is no `expected` field on the real
    /// JVM class at all, so this is deliberately the only structured
    /// datum threaded through; `expected` is not modeled because nothing
    /// real reads it). `None` for an arity error that never populated it
    /// (every native-builtin arity check via `builtins::reg`'s wrapper,
    /// and `builtins::statics::reg_static_fn`'s twin, plus `apply.rs`'s
    /// two user-closure call sites, all do; anything else defaults to
    /// `None` and `catch`-binds to the plain info map, same as before this
    /// task).
    pub arity_actual: Option<i64>,
    /// C3c (special.clj's `quote-with-multiple-args`): an optional
    /// `.getCause` value for the `clojure.lang.ArityException`
    /// `hostclass::mk_arity_exception` builds from an `ErrorKind::Arity`
    /// error that populated `arity_actual` -- `None` (the overwhelming
    /// majority of arity errors) for a plain `.getCause` => `nil`; `Some`
    /// only at `eval_quote`'s one call site, which needs a real cause
    /// value (an `ex-info`-shaped map carrying `:form`) for its ONE
    /// deftest to read back through `(.getCause e)` `(ex-data ..)`
    /// `(:form ..)`. Not a general "every arity error can have a cause"
    /// mechanism -- just this one measured shape.
    /// `Box`ed (not a bare `Option<Value>`) so this rarely-populated field
    /// doesn't grow every `RjError` -- and, transitively,
    /// `crate::value::FutureState::Failed`'s variant, which already
    /// carries a whole `RjError` and is size-compared against
    /// `FutureState::Done(Value)` by clippy's `large_enum_variant` lint --
    /// by a full `Value`'s width for a field that is `None` on every
    /// arity error except `eval_quote`'s one call site.
    pub arity_cause: Option<Box<Value>>,
    /// W3a: an oracle-measured JVM class override for this error -- see
    /// [`JvmClass`]. `None` (the overwhelming majority) falls back to the
    /// `ErrorKind` -> chain table in
    /// `eval::special_forms::error_kind_class_chain`, unchanged.
    pub jvm_class: Option<JvmClass>,
}

impl RjError {
    fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        RjError {
            kind,
            message: message.into(),
            span: None,
            span_source_id: crate::source_registry::UNKNOWN_SOURCE,
            label: None,
            stack: Vec::new(),
            errno: None,
            thrown: None,
            arity_actual: None,
            arity_cause: None,
            jvm_class: None,
        }
    }

    pub fn reader(message: impl Into<String>, span: Span, label: impl Into<String>) -> Self {
        let mut e = Self::new(ErrorKind::Reader, message);
        e.span = Some(span);
        e.label = Some(label.into());
        e
    }

    pub fn arity(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Arity, message)
    }

    pub fn type_err(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::TypeErr, message)
    }

    pub fn unresolved(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Unresolved, message)
    }

    pub fn divide_by_zero(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::DivideByZero, message)
    }

    /// S5 (SPEC-numtower): a checked-arithmetic failure that is NOT a
    /// division by zero -- `long overflow` (transcript rows 1-6) and
    /// `Non-terminating decimal expansion; ...` (row 71). Its own kind,
    /// alongside `DivideByZero`, because both map to the JVM's single
    /// `java.lang.ArithmeticException` and a future error-taxonomy pass
    /// (milestone M8) will want to recognize them together without having
    /// to string-match the message.
    pub fn arithmetic(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Arithmetic, message)
    }

    pub fn recur(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Recur, message)
    }

    /// The fuel budget (`Interp::fuel`) reached zero at a checked back-edge
    /// (loop `recur`, fn self-recur, or fn call entry, in either tier).
    pub fn fuel_exhausted(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::FuelExhausted, message)
    }

    pub fn interrupted(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Interrupted, message)
    }
    pub fn interrupted_hard(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::InterruptedHard, message)
    }

    pub fn other(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Other, message)
    }

    /// `errno` should be a raw OS error number (e.g. from `*std::io::Error`
    /// after a failed libc call); `syscall` is the failing call's name, e.g.
    /// `"openat"`.
    pub fn sys(message: impl Into<String>, errno: i32, syscall: impl Into<String>) -> Self {
        let mut e = Self::new(ErrorKind::Sys, message);
        let strerror = format!("{} — {}", errno_name(errno), strerror_only(errno));
        e.errno = Some((errno, strerror, syscall.into()));
        e
    }

    pub fn thrown(value: Value) -> Self {
        let mut e = Self::new(ErrorKind::Thrown, "user exception");
        e.thrown = Some(value);
        e
    }

    pub fn with_span(mut self, span: Span) -> Self {
        self.span = Some(span);
        self
    }

    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// `source_id` is `Interp::source_id`/`interp.source_id` at the call
    /// site -- see [`RjError::span_source_id`]'s doc for why this
    /// near-universal choke point is where it gets stamped, rather than
    /// threading it through every `with_span` call site. Only stamps when
    /// `span_source_id` is still unset, so a caller that already knows the
    /// real originating buffer is never overwritten by a LATER `with_stack`
    /// call picking up a since-changed `source_id`.
    pub fn with_stack(mut self, stack: Vec<Frame>, source_id: u32) -> Self {
        self.stack = stack;
        if self.span_source_id == crate::source_registry::UNKNOWN_SOURCE {
            self.span_source_id = source_id;
        }
        self
    }

    /// C3c: attaches the actual-argument-count real `clojure.lang.
    /// ArityException.actual` carries -- see `arity_actual`'s own doc.
    /// Only meaningful on an `ErrorKind::Arity` error; callers only ever
    /// chain this directly onto `RjError::arity(..)`/`arity_here(..)`, so
    /// there is no runtime check enforcing that here.
    pub fn with_arity_actual(mut self, actual: i64) -> Self {
        self.arity_actual = Some(actual);
        self
    }

    /// W3a: tags this error with the oracle-measured JVM exception class
    /// real Clojure raises for the same condition -- see [`JvmClass`].
    pub fn with_class(mut self, class: JvmClass) -> Self {
        self.jvm_class = Some(class);
        self
    }

    /// C3c: attaches a `.getCause` value -- see `arity_cause`'s own doc.
    pub fn with_arity_cause(mut self, cause: Value) -> Self {
        self.arity_cause = Some(Box::new(cause));
        self
    }

    pub fn push_frame(mut self, frame: Frame) -> Self {
        self.stack.push(frame);
        self
    }

    /// True when this is a reader error caused by input ending before a
    /// form was complete (unclosed list/vector/map/set/string/fn-literal,
    /// a dangling reader-macro prefix like `'`/`` ` ``/`~`/`@`/`#_` with
    /// nothing after it, or EOF mid character-literal) -- as opposed to a
    /// genuinely malformed form (stray `)`, bad escape, bad number). P3b's
    /// REPL uses this to decide whether to keep reading continuation lines
    /// instead of reporting the error. `reader.rs` has no typed variant for
    /// "incomplete" vs. "malformed" reader errors, so this is a documented
    /// message-sniffing discriminator (coordinate risk noted in P3b's brief
    /// as low: reader.rs's error messages are stable, hand-written text).
    pub fn is_incomplete(&self) -> bool {
        if self.kind != ErrorKind::Reader {
            return false;
        }
        let m = &self.message;
        m.starts_with("unclosed")
            || m.contains("unexpected EOF")
            || m.starts_with("expected a form after")
            || m.starts_with("expected a form to discard after")
    }
}

impl fmt::Display for RjError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for RjError {}

impl Diagnostic for RjError {
    fn code<'a>(&'a self) -> Option<Box<dyn fmt::Display + 'a>> {
        Some(Box::new(error_kind_header(self.kind)))
    }
}

/// Human-readable report header for each `ErrorKind`, in place of a bare
/// `{:?}` debug tag (e.g. "Sys") -- this is what miette prints at the top
/// of the rendered diagnostic.
fn error_kind_header(kind: ErrorKind) -> &'static str {
    match kind {
        ErrorKind::Reader => "reader error",
        ErrorKind::Arity => "arity error",
        ErrorKind::TypeErr => "type error",
        ErrorKind::Unresolved => "unresolved symbol",
        ErrorKind::DivideByZero => "divide by zero",
        ErrorKind::Arithmetic => "arithmetic error",
        ErrorKind::Sys => "system error",
        ErrorKind::Thrown => "thrown value",
        ErrorKind::FuelExhausted => "fuel exhausted",
        ErrorKind::Interrupted | ErrorKind::InterruptedHard => "interrupted",
        ErrorKind::Recur | ErrorKind::Other => "error",
    }
}

/// `std::io::Error`'s `Display` for a raw OS error renders as e.g.
/// `"No such file or directory (os error 2)"` -- the human strerror text
/// plus a redundant `"(os error N)"` suffix. `render()` already appends its
/// own `"(errno N)"` suffix, so this strips that trailing parenthetical to
/// avoid printing the errno number twice.
fn strerror_only(errno: i32) -> String {
    let full = std::io::Error::from_raw_os_error(errno).to_string();
    match full.rfind(" (os error ") {
        Some(idx) => full[..idx].to_string(),
        None => full,
    }
}

/// Small, deliberately partial errno -> symbolic-name table covering the
/// syscalls `builtins::sys` is expected to wrap; falls back to "UNKNOWN"
/// rather than guessing.
fn errno_name(e: i32) -> &'static str {
    match e {
        x if x == libc::ENOENT => "ENOENT",
        x if x == libc::EACCES => "EACCES",
        x if x == libc::EEXIST => "EEXIST",
        x if x == libc::EINVAL => "EINVAL",
        x if x == libc::EBADF => "EBADF",
        x if x == libc::ENOTDIR => "ENOTDIR",
        x if x == libc::EISDIR => "EISDIR",
        x if x == libc::EPERM => "EPERM",
        x if x == libc::ENOSPC => "ENOSPC",
        x if x == libc::EPIPE => "EPIPE",
        x if x == libc::EINTR => "EINTR",
        x if x == libc::EAGAIN => "EAGAIN",
        _ => "UNKNOWN",
    }
}

/// Pairs an `RjError` with the source text it should be rendered against,
/// purely so it can implement `Diagnostic::source_code`/`labels` for
/// `render()`. `RjError` itself doesn't carry source text (that lives on
/// `Interp`), so this wrapper only exists for the duration of one render.
struct SourceAnnotated<'a> {
    err: &'a RjError,
    source: NamedSource<String>,
}

impl fmt::Debug for SourceAnnotated<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.err, f)
    }
}

impl fmt::Display for SourceAnnotated<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self.err, f)
    }
}

impl std::error::Error for SourceAnnotated<'_> {}

impl Diagnostic for SourceAnnotated<'_> {
    fn code<'a>(&'a self) -> Option<Box<dyn fmt::Display + 'a>> {
        self.err.code()
    }

    fn source_code(&self) -> Option<&dyn SourceCode> {
        Some(&self.source)
    }

    fn labels(&self) -> Option<Box<dyn Iterator<Item = LabeledSpan> + '_>> {
        let span = self.err.span?;
        let label = self.err.label.clone();
        let len = span.end.saturating_sub(span.start).max(1);
        Some(Box::new(std::iter::once(LabeledSpan::new(
            label, span.start, len,
        ))))
    }
}

/// Resolves the buffer a labeled span/frame should be rendered against:
/// `source_id` (a [`crate::source_registry`] id, when the caller has one)
/// wins when it actually [`resolve`](crate::source_registry::resolve)s;
/// otherwise falls back to the caller-supplied `(fallback_name,
/// fallback_source)` pair -- the pre-W-SPAN-rider best-effort behavior,
/// kept so a frame/span that was never taught to carry a real id (or an
/// overflowed intern) still renders SOMETHING instead of nothing.
fn resolve_source<'a>(
    source_id: u32,
    fallback_name: &'a str,
    fallback_source: &'a str,
) -> (std::borrow::Cow<'a, str>, std::borrow::Cow<'a, str>) {
    match crate::source_registry::resolve(source_id) {
        Some((name, text)) => (
            std::borrow::Cow::Owned(name.as_ref().to_string()),
            std::borrow::Cow::Owned(text.as_ref().to_string()),
        ),
        None => (
            std::borrow::Cow::Borrowed(fallback_name),
            std::borrow::Cow::Borrowed(fallback_source),
        ),
    }
}

/// Renders `err` as a fancy miette report (labeled snippet when a span is
/// present), followed by mova call-stack frames, followed by an errno line
/// when present.
///
/// field5/W-SPAN rider: the label and EACH stack frame resolve their OWN
/// source via `err.span_source_id`/`Frame::source_id` (falling back to the
/// caller-supplied `(source_name, source)` pair when unset or unresolvable)
/// instead of rendering the whole multi-frame stack against one buffer --
/// the bug this fixes: a deeper/shallower frame's span landing on a
/// DIFFERENT file than `(source_name, source)` made miette's own snippet
/// machinery slice out of bounds and print "Failed to read contents for
/// label ... OutOfBounds" in place of a useful snippet. Whatever happens to
/// the snippet, `err.message` is ALWAYS shown -- rendered directly, never
/// only through miette's fallible label machinery -- so a source lookup
/// failure can degrade the pretty output but can never hide the real
/// error.
pub fn render(err: &RjError, source_name: &str, source: &str) -> String {
    let (label_name, label_source) = resolve_source(err.span_source_id, source_name, source);

    // Guard against miette's own "Failed to read contents for label"
    // substitution: only ask it for a labeled snippet when the span is
    // actually in bounds for the buffer we resolved. Out of bounds (or no
    // span at all), skip the fancy label entirely and fall through to the
    // plain header below -- the message is never gated on this succeeding.
    let span_in_bounds = err
        .span
        .is_some_and(|s| s.end.max(s.start) <= label_source.len());

    let mut out = String::new();
    if span_in_bounds {
        let diag = SourceAnnotated {
            err,
            source: NamedSource::new(label_name.as_ref(), label_source.to_string()),
        };
        let handler = GraphicalReportHandler::new();
        let _ = handler.render_report(&mut out, &diag);
    }
    if out.is_empty() {
        // No span, an out-of-bounds span, or `render_report` produced
        // nothing (e.g. a write failure) -- plain header, same message a
        // labeled render would have shown up top, just without the
        // snippet.
        out.push_str(&format!(
            "{}\n\n  {}\n",
            error_kind_header(err.kind),
            err.message
        ));
    }

    for frame in &err.stack {
        let (frame_name, frame_source) = resolve_source(frame.source_id, source_name, source);
        let (line, col) = line_col(&frame_source, frame.span.start);
        out.push_str(&format!(
            "  at ({}) {}:{}:{}\n",
            frame.name, frame_name, line, col
        ));
    }

    if let Some((errno, strerror, syscall)) = &err.errno {
        out.push_str(&format!(
            "  system: {syscall}(2) failed: {strerror} (errno {errno})\n"
        ));
    }

    out
}

/// The rich snippet of [`render`] without its header and message lines: the
/// `,-[file:line:col]` block with numbered source lines, the underline and
/// the label (`err.label`, or `label` when given). `None` when the error has
/// no span or the span is outside its buffer. `colour` picks ANSI styling.
/// Used by `errinfo` for `ErrorInfo::report`, which sits next to the
/// JVM-compatible text and so must not repeat the message.
pub fn render_snippet(
    err: &RjError,
    source_name: &str,
    source: &str,
    label: Option<&str>,
    colour: bool,
) -> Option<String> {
    let span = err.span?;
    let (label_name, label_source) = resolve_source(err.span_source_id, source_name, source);
    if span.end.max(span.start) > label_source.len() {
        return None;
    }
    let mut e2 = err.clone();
    if let Some(l) = label {
        e2.label = Some(l.to_string());
    }
    let diag = SourceAnnotated {
        err: &e2,
        // a line-0 buffer starts with a NUL marker (see `line_col`): show it as a blank
        source: NamedSource::new(label_name.as_ref(), label_source.replacen('\0', " ", 1)),
    };
    let theme = if colour { miette::GraphicalTheme::ascii() } else { miette::GraphicalTheme::none() };
    let handler = GraphicalReportHandler::new_themed(theme);
    let mut out = String::new();
    handler.render_report(&mut out, &diag).ok()?;
    // Drop the code header and the message line; keep from the snippet on.
    let lines: Vec<&str> = out.lines().collect();
    let start = lines.iter().position(|l| {
        let t = l.trim_start();
        t.starts_with(",-[") || t.starts_with("\u{256d}\u{2500}[") || t.contains("-[")
    })?;
    // A host that pads the source with newlines (nREPL `line` / `column`) leaves
    // blank numbered lines before the code: drop them.
    let mut body: Vec<&str> = Vec::new();
    let mut seen_code = false;
    for (k, l) in lines[start..].iter().enumerate() {
        if k > 0 && !seen_code {
            if let Some((num, rest)) = l.split_once('|') {
                if num.trim().chars().all(|c| c.is_ascii_digit()) && !num.trim().is_empty() {
                    if rest.trim().is_empty() {
                        continue;
                    }
                    seen_code = true;
                }
            }
        }
        body.push(l);
    }
    let mut res = body.join("\n");
    res.push('\n');
    Some(res)
}

/// `pub(crate)`, not private: `eval::special_forms::publish_var_meta` (C3h,
/// clojure.repl surface) reuses this SAME byte-offset-to-(line,column)
/// mapping to compute a def'd var's `:line`/`:column` metadata -- the one
/// other place in the crate that needs "which line does this span start
/// on", so it shares this function instead of re-deriving it.
pub(crate) fn line_col(source: &str, byte_offset: usize) -> (usize, usize) {
    // A host that starts a buffer at line 0 (nREPL `line: 0`) marks it with a
    // leading NUL: the first line is line 0 and the NUL takes no column.
    let zero_based = source.starts_with('\0');
    let mut line = if zero_based { 0usize } else { 1usize };
    let mut col = 1usize;
    for (i, c) in source.char_indices() {
        if i >= byte_offset {
            break;
        }
        if zero_based && i == 0 {
            continue;
        }
        if c == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    (line, col)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::Str;

    #[test]
    fn render_includes_message_and_stack_and_errno() {
        let err = RjError::sys("boom", libc::ENOENT, "openat")
            .with_span(Span { start: 0, end: 1 })
            .push_frame(Frame {
                name: Str::from("my-fn"),
                span: Span { start: 0, end: 1 },
                source_id: crate::source_registry::UNKNOWN_SOURCE,
            });
        let rendered = render(&err, "repl", "x");
        assert!(rendered.contains("at (my-fn) repl:1:1"));
        assert!(rendered.contains("system: openat(2) failed:"));
        assert!(rendered.contains("errno 2"));
    }

    /// field5/W-SPAN rider on `RjError`/`Frame`: a two-file call stack --
    /// the failure itself is in `b.mova`, reached by a call made from
    /// `a.mova` -- renders the label against `b.mova` (where the span
    /// actually is) and the stack-frame line against `a.mova` (where THAT
    /// span actually is), even though `render`'s caller only ever supplies
    /// one unrelated `(source_name, source)` fallback pair. Before this
    /// fix, both the label and the frame line were computed against
    /// whichever single buffer the caller passed, which for a real
    /// multi-file failure silently produced garbage line:col numbers (or,
    /// when the span landed past the end of that one buffer, made miette
    /// print "Failed to read contents for label ... OutOfBounds" in place
    /// of a snippet) instead of pointing into the file each span actually
    /// came from.
    #[test]
    fn render_resolves_each_frame_against_its_own_file() {
        const TEXT_A: &str = "line1\nline2\ncall-b\n";
        const TEXT_B: &str = "defn-b\nboom\n";
        // `source_registry::resolve` now re-reads its `name` off disk (RSS
        // lever, see that module's `Entry` doc), so these need to be real
        // files for resolution to see the right per-file content.
        let dir = std::env::temp_dir();
        let (path_a, path_b) = (dir.join("w-error-render-a.mova"), dir.join("w-error-render-b.mova"));
        std::fs::write(&path_a, TEXT_A).unwrap();
        std::fs::write(&path_b, TEXT_B).unwrap();
        let id_a = crate::source_registry::intern(path_a.to_str().unwrap(), TEXT_A);
        let id_b = crate::source_registry::intern(path_b.to_str().unwrap(), TEXT_B);

        let boom_offset = TEXT_B.find("boom").unwrap();
        let err = RjError::type_err("boom in b")
            .with_span(Span {
                start: boom_offset,
                end: boom_offset + 4,
            })
            .with_stack(
                vec![Frame {
                    name: Str::from("call-in-a"),
                    span: Span { start: 12, end: 18 }, // "call-b" on a.mova's line 3
                    source_id: id_a,
                }],
                id_b,
            );

        // Caller passes an UNRELATED fallback pair -- neither file the
        // spans actually belong to -- to prove resolution comes from the
        // registry, not from this argument.
        let rendered = render(&err, "unrelated", "unrelated fallback text");

        assert!(rendered.contains("boom in b"), "real message missing: {rendered}");
        assert!(
            !rendered.contains("OutOfBounds"),
            "should never fall back to miette's OOB label text: {rendered}"
        );
        let expect_a = format!("at (call-in-a) {}:3:1", path_a.to_str().unwrap());
        assert!(rendered.contains(&expect_a), "frame should resolve against a.mova, not the fallback pair: {rendered}");
        let _ = std::fs::remove_file(&path_a);
        let _ = std::fs::remove_file(&path_b);
    }

    /// The real message is shown even when the label's span is out of
    /// bounds for every source available (no source-id resolves, and the
    /// caller-supplied fallback is too short) -- miette's own "Failed to
    /// read contents for label" substitution must never be the ONLY thing
    /// printed.
    #[test]
    fn render_always_shows_message_even_when_span_is_out_of_bounds() {
        let err = RjError::type_err("boom real message").with_span(Span { start: 50, end: 51 });
        let rendered = render(&err, "repl", "x");
        assert!(rendered.contains("boom real message"), "{rendered}");
    }

    #[test]
    fn line_col_counts_newlines() {
        assert_eq!(line_col("ab\ncd", 3), (2, 1));
        assert_eq!(line_col("ab\ncd", 4), (2, 2));
        assert_eq!(line_col("abc", 0), (1, 1));
    }
}
