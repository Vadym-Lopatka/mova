//! The embeddable facade's error type. Wraps `crate::error::RjError`
//! (miette `Diagnostic`, mova call stack, errno) behind a plain
//! `std::error::Error` so a host crate isn't forced to depend on `miette`
//! itself just to propagate a script failure with `?`.

use crate::error::RjError;

/// A script/call failure. [`Display`](std::fmt::Display) gives the bare
/// message (what you'd want in a one-line log); [`Error::render_plain`]
/// gives the full miette-rendered diagnostic (labeled source snippet, mova
/// call stack, errno line) as plain text -- see `main.rs` for the same
/// rendering mova's own CLI uses.
pub struct Error {
    // Boxed: `RjError` is deliberately large (ARCHITECTURE.md mandates its
    // exact rich shape -- span, label, stack, errno, thrown value -- see
    // `lib.rs`'s crate-level `#![allow(clippy::result_large_err)]`). That
    // allow only covers code inside THIS crate; an embedder's own
    // `Result<embed::Value, embed::Error>` call sites would trip the same
    // clippy lint with no way to silence it short of copying that
    // crate-level allow into their own code. Boxing here keeps `Error`
    // pointer-sized so the lint never reaches through the facade.
    inner: Box<RjError>,
    source_name: String,
    source: String,
}

impl Error {
    /// Built by `Engine` at the moment an `RjError` crosses back into
    /// embed-land, from whatever source text the `Interp` currently has
    /// loaded (`main.rs` renders errors the same way, off `interp.
    /// source_name`/`interp.source`) -- so `render_plain` can reproduce the
    /// same labeled snippet the CLI would have shown for the same failure.
    pub(crate) fn from_engine(inner: RjError, source_name: &str, source: &str) -> Self {
        Error {
            inner: Box::new(inner),
            source_name: source_name.to_string(),
            source: source.to_string(),
        }
    }

    /// A host-constructed error with no mova source text behind it (e.g. a
    /// `register_fn` closure rejecting a bad argument). `render_plain`
    /// still works -- just without a labeled snippet, since there's no span
    /// to point at.
    pub fn other(message: impl Into<String>) -> Self {
        Error {
            inner: Box::new(RjError::other(message)),
            source_name: String::new(),
            source: String::new(),
        }
    }

    pub(crate) fn into_rj_error(self) -> RjError {
        *self.inner
    }

    /// Whether this error is the embedding-fuel budget running out
    /// (`crate::error::ErrorKind::FuelExhausted`) -- the signal a host
    /// running a [`crate::embed::Profile::Untrusted`] engine (or any engine
    /// with `EngineBuilder::fuel`/`Engine::set_fuel` configured) should
    /// branch on to distinguish "this script hit its budget" from an
    /// ordinary script error, rather than matching on `render_plain`'s
    /// message text.
    pub fn is_fuel_exhausted(&self) -> bool {
        self.inner.kind == crate::error::ErrorKind::FuelExhausted
    }

    /// Whether this error is the reader's unclosed-delimiter/unexpected-EOF
    /// case (`crate::error::RjError::is_incomplete`) -- input that ended
    /// before a top-level form was complete, as opposed to a genuinely
    /// malformed one. This is what lets a host REPL keep reading
    /// continuation lines: on `Engine::eval`/`eval_named`, a reader error is
    /// detected before anything is evaluated (`Interp::eval_str` reads the
    /// whole source first), so `is_incomplete_input() == true` means the
    /// accumulated buffer had no side effects yet and the host should
    /// append another line and retry the same call, exactly like `mova`'s
    /// own REPL does.
    pub fn is_incomplete_input(&self) -> bool {
        self.inner.is_incomplete()
    }

    /// The full miette-rendered diagnostic as plain text: header, labeled
    /// source snippet (when a span is available), mova call stack, and an
    /// errno line for a system error. Safe to print directly to a
    /// terminal -- this is exactly what `mova`'s own CLI shows on
    /// failure.
    pub fn render_plain(&self) -> String {
        crate::error::render(&self.inner, &self.source_name, &self.source)
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.inner.message)
    }
}

impl std::fmt::Debug for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.inner, f)
    }
}

impl std::error::Error for Error {}
