//! Reads forms ONE AT A TIME from a string, for a host that must answer after
//! each form (an nREPL `eval`): each form comes with its line/column, and a
//! read error comes only after the earlier forms were returned.
//!
//! The start `line`/`column` (1-based, the nREPL `line`/`column` params) are
//! honoured by padding the source with newlines and spaces, so the line and
//! column that `def` metadata (`:line`, `:column`) and error locations show
//! are already absolute. Column only shifts the first line.
//!
//! ```ignore
//! let mut feed = FormFeed::new(&mut interp, "REPL", "(+ 1 2) (foo", 1, 1, false);
//! while let Some(f) = feed.next_form(&mut interp)? {   // Err on the 2nd call
//!     let v = interp.eval_form(&f.form)?;
//! }
//! ```

use crate::error::RjError;
use crate::eval::Interp;
pub use crate::reader::Form;
use crate::value::Str;

/// One form read from the feed.
#[derive(Debug, Clone)]
pub struct FeedForm {
    pub form: Form,
    /// 1-based line and column of the form's first character (offset applied).
    pub line: usize,
    pub column: usize,
    /// Byte offsets of the form in the user's code (padding removed).
    pub start: usize,
    pub end: usize,
}

pub struct FormFeed {
    /// Padded source.
    src: String,
    /// Length of the padding in bytes.
    pad: usize,
    pos: usize,
    allow_read_cond: bool,
    preserve_read_cond: bool,
    failed: bool,
    /// Frees the interned source when the feed ends (see `new_transient`).
    _lease: Option<crate::source_registry::SourceLease>,
}

impl FormFeed {
    /// Makes `code` the interpreter's current source (`file` is the name shown
    /// in errors) and starts at `line` / `column` (both 1-based; a `column` of 0
    /// counts as 1, a `line` of 0 is kept: the JVM numbers such a buffer from 0).
    pub fn new(
        interp: &mut Interp,
        file: &str,
        code: &str,
        line: usize,
        column: usize,
        allow_read_cond: bool,
    ) -> FormFeed {
        Self::build(interp, file, code, line, column, allow_read_cond, false)
    }

    /// Like [`FormFeed::new`], for a host that runs one request per feed (nREPL):
    /// the source's registry slot is freed when the feed is dropped, unless a
    /// fn or macro defined by the code refers to it. Keeps the registry bounded
    /// over a long session. Errors are reported while the feed is alive.
    pub fn new_transient(
        interp: &mut Interp,
        file: &str,
        code: &str,
        line: usize,
        column: usize,
        allow_read_cond: bool,
    ) -> FormFeed {
        Self::build(interp, file, code, line, column, allow_read_cond, true)
    }

    fn build(
        interp: &mut Interp,
        file: &str,
        code: &str,
        line: usize,
        column: usize,
        allow_read_cond: bool,
        transient: bool,
    ) -> FormFeed {
        // line 0 is the JVM's `REPL:0`: no padding, and a NUL marker (see `error::line_col`)
        let mut src = if line == 0 { "\0".to_string() } else { "\n".repeat(line - 1) };
        src.push_str(&" ".repeat(column.max(1) - 1));
        let pad = src.len();
        src.push_str(code);
        interp.source_name = Str::from(file);
        interp.source = Str::from(src.as_str());
        let lease = if transient {
            let (id, lease) = crate::source_registry::intern_transient(file, &src);
            interp.source_id = id;
            Some(lease)
        } else {
            interp.source_id = crate::source_registry::intern(file, &src);
            None
        };
        FormFeed { src, pad, pos: pad, allow_read_cond, preserve_read_cond: false, failed: false, _lease: lease }
    }

    /// `{:read-cond :preserve}`: a `#?(...)` stays a reader-conditional object.
    pub fn set_preserve_read_cond(&mut self, on: bool) {
        self.preserve_read_cond = on;
        if on {
            self.allow_read_cond = true;
        }
    }

    /// Next form, `Ok(None)` at end of input. After an `Err` the feed is
    /// finished: later calls give `Ok(None)`.
    pub fn next_form(&mut self, interp: &mut Interp) -> Result<Option<FeedForm>, RjError> {
        if self.failed {
            return Ok(None);
        }
        let mut reader = crate::reader::Reader::resume(&self.src, self.pos, self.allow_read_cond);
        reader.set_ns_ctx(interp.reader_ns_context());
        reader.set_preserve_read_cond(self.preserve_read_cond);
        match reader.next_form() {
            Ok(Some(mut form)) => {
                // `#=(form)`: the JVM evaluates the form while reading and the
                // result is the form. Only as a whole top-level form.
                if reader.tag_literals().iter().any(|(at, t)| t == "=" && *at == form.span.start) {
                    self.failed = true;
                    let read_eval = matches!(
                        interp.globals.get_exact(&crate::value::Symbol::simple("*read-eval*")),
                        Some(crate::value::Value::Bool(false))
                    );
                    if read_eval {
                        return Err(crate::error::RjError::reader(
                            "EvalReader not allowed when *read-eval* is false.",
                            form.span,
                            "`#=` is refused while *read-eval* is false",
                        )
                        .with_stack(Vec::new(), interp.source_id));
                    }
                    let v = interp
                        .eval_form(&form)
                        .map_err(|e| e.with_stack(Vec::new(), interp.source_id))?;
                    self.failed = false;
                    form = crate::reader::value_to_form(&v, form.span);
                    self.pos = reader.position();
                    let (line, column) = crate::error::line_col(&self.src, form.span.start);
                    return Ok(Some(FeedForm {
                        start: form.span.start.saturating_sub(self.pad),
                        end: form.span.end.saturating_sub(self.pad),
                        form,
                        line,
                        column,
                    }));
                }
                // JVM: a tagged literal needs a reader fn at read time.
                if let Some((_, tag)) = reader
                    .tag_literals()
                    .iter()
                    .find(|(_, t)| t != "uuid" && t != "inst" && !data_reader_known(interp, t))
                {
                    self.failed = true;
                    return Err(crate::error::RjError::reader(
                        format!("No reader function for tag {tag}"),
                        form.span,
                        "this tag has no reader function",
                    )
                    .with_stack(Vec::new(), interp.source_id));
                }
                self.pos = reader.position();
                let (line, column) = crate::error::line_col(&self.src, form.span.start);
                Ok(Some(FeedForm {
                    start: form.span.start.saturating_sub(self.pad),
                    end: form.span.end.saturating_sub(self.pad),
                    form,
                    line,
                    column,
                }))
            }
            Ok(None) => {
                self.pos = self.src.len();
                Ok(None)
            }
            Err(e) => {
                self.failed = true;
                // Spans point into the padded source, which is `interp.source`.
                Err(e.with_stack(Vec::new(), interp.source_id))
            }
        }
    }

    /// The user's code after the last form read (all of it, before the first
    /// read, and the unread tail after a read error).
    pub fn rest(&self) -> &str {
        &self.src[self.pos.min(self.src.len())..]
    }
}

/// Whether `*data-readers*` (or `default-data-readers`) has a fn for `tag`.
fn data_reader_known(interp: &mut Interp, tag: &str) -> bool {
    let key = match tag.split_once('/') {
        Some((ns, n)) => crate::value::Symbol { ns: Some(ns.into()), name: n.into() },
        None => crate::value::Symbol::simple(tag),
    };
    match interp.globals.get(&crate::value::Symbol::simple("*data-readers*")) {
        Some(crate::value::Value::Map(m)) => m.get(&crate::value::Value::Sym(key)).is_some(),
        _ => false,
    }
}
