//! The eval loop: one `eval` request on an interpreter whose thread already
//! has the session's binding frame (see `bindings.rs`).
//!
//! Per request (design 5.2, 5.3):
//!
//! 1. `ns` param: unknown -> `namespace-not-found`; known -> `*ns*` is bound
//!    to it for this request only.
//! 2. `file` / `line` / `column`: forms are read with their real place
//!    (`FormFeed`); `*file*` and `*source-path*` are bound for the request.
//! 3. For each form: read, eval, then on success `*3 <- *2 <- *1 <- value`,
//!    flush `err` then `out`, send `{ns, value}`. On an eval or print error:
//!    set `*e`, send the `err` text, then `{ex, root-ex, status eval-error}`
//!    and go on with the next form.
//! 4. A read error ends the request (forms before it were already answered).
//! 5. At the end: flush `err` and `out`. The caller sends `done`.
//!
//! `out` / `err` text is written by the evaluated code into the request's two
//! sinks (`output.rs`) through `*out*` / `*err*`.

use super::bindings::{Vars, STAR1, STAR2, STAR3, STARE};
use super::output::Sink;
use super::print::{self, PrintOpts, Printer};
use super::session_thread::SessionShared;
use crate::env::VarCell;
use crate::errinfo::{error_info, ErrorCtx, Phase};
use crate::error::RjError;
use crate::eval::Interp;
use crate::formfeed::FormFeed;
use crate::value::{Str, Symbol, Value};
use mova_nrepl::{status, Responder, V};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// How the request asked for `#?` to be read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ReadCond {
    /// Absent or `allow`: `:clj` branches are taken.
    Allow,
    /// `preserve`: a `#?(...)` is kept as a reader-conditional object.
    Preserve,
    /// Any other value: the reader refuses `#?` ("Conditional read not allowed").
    Refuse,
}

/// What the `err` message holds for an error (`--errors=rich|jvm`).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ErrorMode {
    /// `ErrorInfo.text`, then a blank line and Mova's rich report (when there is one).
    #[default]
    Rich,
    /// Exactly `ErrorInfo.text` (what the JVM nREPL sends; used by the wire gate).
    Jvm,
}

impl ErrorMode {
    pub fn parse(s: &str) -> Option<ErrorMode> {
        match s {
            "rich" => Some(ErrorMode::Rich),
            "jvm" => Some(ErrorMode::Jvm),
            _ => None,
        }
    }
}

/// The text of the `err` message for one error.
fn err_text(info: &crate::errinfo::ErrorInfo, mode: ErrorMode) -> String {
    match mode {
        ErrorMode::Jvm => info.text.clone(),
        // The JVM text first, then Mova's own report (it never repeats the header).
        ErrorMode::Rich => match &info.report {
            Some(r) if !r.trim().is_empty() => {
                let mut t = info.text.clone();
                if !t.ends_with('\n') {
                    t.push('\n');
                }
                t.push('\n');
                t.push_str(r);
                if !t.ends_with('\n') {
                    t.push('\n');
                }
                t
            }
            _ => info.text.clone(),
        },
    }
}

/// One `eval` request, copied off the IO thread.
pub(crate) struct EvalJob {
    pub code: String,
    pub ns: Option<String>,
    pub file: Option<String>,
    pub file_name: Option<String>,
    pub line: usize,
    pub column: usize,
    pub read_cond: ReadCond,
    pub errors: ErrorMode,
    /// `load-file`: `code` is the file; one `value` (no `ns`), stop at the first error.
    pub load_file: bool,
    pub print: PrintOpts,
    /// `eval` param: symbol of a fn that evaluates each form instead of the interpreter.
    pub eval_fn: Option<String>,
    pub reply: Responder,
    /// `out-limit`: bytes after which `out` / `err` chunks are sent.
    pub out_limit: Option<usize>,
}

impl EvalJob {
    /// The final `done` (with the reply keys the client asked for through `keys`).
    pub(crate) fn send_done(&self) {
        let extra = print::keys_fields(&self.print);
        if extra.is_empty() {
            self.reply.send_status(status::DONE);
        } else {
            let mut f: Vec<(&str, V<'_>)> = vec![("status", V::Strs(status::DONE))];
            for (k, v) in &extra {
                f.retain(|(n, _)| n != k);
                f.push((k.as_str(), V::Str(v)));
            }
            self.reply.send(&f);
        }
    }
}

/// What the running eval needs to know about the session it runs in.
pub(crate) struct Ctl<'a>(pub Option<&'a SessionShared>);

impl Ctl<'_> {
    fn interrupted(&self) -> bool {
        self.0.is_some_and(|s| s.was_interrupted())
    }
    /// The eval no longer counts as running (no more interrupts for it).
    fn settle(&self) {
        if let Some(s) = self.0 {
            s.settle();
        }
    }
}

/// Numbers `user/eval<N>` in error text, like the JVM's compiler counter.
static EVAL_IDS: AtomicU64 = AtomicU64::new(1);

/// Pops the frames a request pushed, also when the eval panics.
struct TempFrames(Vec<Arc<VarCell>>);

impl Drop for TempFrames {
    fn drop(&mut self) {
        for c in self.0.drain(..).rev() {
            c.pop_binding();
        }
    }
}

/// Runs the request. Returns the namespace the session is in afterwards.
/// Sends everything except the final `done`.
/// The bool is false when the reply already ended with its own status (`namespace-not-found`).
pub(crate) fn run(interp: &mut Interp, vars: &Vars, job: &EvalJob, ctl: &Ctl<'_>) -> (Str, bool) {
    let reply = &job.reply;
    let mut temp = TempFrames(Vec::new());

    if let Some(ns) = &job.ns {
        if interp.find_ns_value(&Value::Sym(Symbol::simple(ns.as_str()))).is_none() {
            reply.send(&[("ns", V::Str(ns)), ("status", V::Strs(status::NAMESPACE_NOT_FOUND))]);
            return (interp.dynamic_ns_name(), false);
        }
        vars.ns.push_binding(crate::ns::ns_value(&Str::from(ns.as_str())));
        temp.0.push(vars.ns.clone());
    }
    if let Some(file) = &job.file {
        // the JVM session keeps `*file*` after the request: a later def without `file` still records it
        if let Some(c) = &vars.file {
            c.set_binding(Value::Str(Str::from(file.as_str())));
        }
        if let Some(c) = &vars.source_path {
            let short = job.file_name.as_deref().unwrap_or_else(|| short_file_name(file));
            c.push_binding(Value::Str(Str::from(short)));
            temp.0.push(c.clone());
        }
    }

    let extra = print::keys_fields(&job.print);
    let out = Sink::new("out", reply.clone(), extra.clone());
    let err = Sink::new("err", reply.clone(), extra);
    if let Some(n) = job.out_limit {
        out.set_limit(n);
        err.set_limit(n);
    }
    let out_stream = out.stream_value();
    // `print`/`println` write straight into the sink while `*out*` is this stream.
    if let Value::HostInst(h) = &out_stream {
        crate::builtins::strings::set_fast_out(Some((Arc::as_ptr(h) as usize, out.clone())));
    }
    vars.out.set_binding(out_stream);
    vars.err.set_binding(err.stream_value());
    // Design 5.4: the interrupt flag is cleared at eval start, set only while an eval runs.
    interp.intr.clear();

    // `load-file` runs the file in the session's namespace but a `(ns ..)` or
    // `(in-ns ..)` in it must not move the session (JVM: `*ns*` is re-bound).
    if job.load_file && job.ns.is_none() {
        vars.ns.push_binding(crate::ns::ns_value(&interp.dynamic_ns_name()));
        temp.0.push(vars.ns.clone());
    }

    let printer = Printer::new(interp, job.print.clone(), reply);
    let caught_fn = match &job.print.caught {
        Some(c) => {
            let f = print::resolve_fn(interp, c);
            if f.is_none() {
                reply.send(&[
                    (print::CAUGHT_ERROR, V::Str(&format!("Couldn't resolve var {c}"))),
                    ("status", V::Strs(&[print::CAUGHT_ERROR])),
                ]);
            }
            f
        }
        None => None,
    };
    let eval_fn = job.eval_fn.as_deref().and_then(|s| print::resolve_fn(interp, s));
    let rep = Reporter { vars, err: &err, reply, mode: job.errors, printer: &printer, caught_fn: caught_fn.as_ref(), caught_print: job.print.caught_print };

    let source_name = job.file.as_deref().unwrap_or("REPL");
    let mut feed = FormFeed::new_transient(interp, source_name, &job.code, job.line, job.column, job.read_cond != ReadCond::Refuse);
    feed.set_preserve_read_cond(job.read_cond == ReadCond::Preserve);
    // what a `def` records as `:file` (the JVM's `*file*`)
    interp.def_file = Some(match vars.file.as_ref().and_then(|c| c.current_binding()) {
        Some(Value::Str(f)) => f,
        _ => Str::from("NO_SOURCE_PATH"),
    });
    let mut errored = false;
    // load-file: the last value, sent when the file is done (no `ns`)
    let mut last: Option<Value> = None;
    loop {
        interp.current_ns = interp.dynamic_ns_name();
        let form = match feed.next_form(interp) {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => {
                // The JVM reader names its source `REPL` whatever `file` says.
                let ctx = ErrorCtx { phase: Some(Phase::ReadSource), file: Some("REPL".into()), ..ErrorCtx::default() };
                errored = true;
                rep.report(interp, &e, &ctx);
                break;
            }
        };
        let loading = matches!(&form.form.value, crate::reader::FormValue::List(items)
            if matches!(items.first().map(|f| &f.value), Some(crate::reader::FormValue::Atom(Value::Sym(s))) if s.ns.is_none() && &*s.name == "ns"));
        let ctx = ErrorCtx { eval_id: EVAL_IDS.fetch_add(1, Ordering::Relaxed), loading, ..ErrorCtx::default() };
        let result = match &eval_fn {
            Some(f) => {
                let fv = crate::reader::form_to_value(&form.form);
                interp.call(f, &[fv])
            }
            None => {
                crate::eval::special_forms::template_scope_begin();
                crate::eval::macro_scope_begin();
                let r = super::toplevel::eval_form(interp, &form.form);
                crate::eval::special_forms::template_scope_end();
                crate::eval::macro_scope_end();
                r
            }
        };
        if ctl.interrupted() {
            // The session thread of the JVM is replaced after an interrupt: no
            // more forms. An error that came from the interrupt is reported
            // unless it is the loop kind (the JVM's ThreadDeath: silent).
            ctl.settle();
            interp.intr.clear();
            if let Err(e) = &result {
                if !silent_interrupt(e) {
                    rep.report(interp, e, &ctx);
                }
            } else if let Ok(v) = &result {
                set_stars(vars, v);
                let ns = interp.dynamic_ns_name();
                let _ = printer.reply_value(interp, reply, v, &ns);
            }
            break;
        }
        match result {
            Ok(value) => {
                set_stars(vars, &value);
                // `err` first, then `out`: both before the value, as on the JVM.
                err.flush();
                out.flush();
                let ns = interp.dynamic_ns_name();
                interp.current_ns = ns.clone();
                if job.load_file {
                    last = Some(value);
                } else if let Err(e) = printer.reply_value(interp, reply, &value, &ns) {
                    let ctx = ErrorCtx { phase: Some(Phase::PrintEvalResult), ..ctx };
                    rep.report(interp, &e, &ctx);
                }
            }
            Err(e) => {
                errored = true;
                rep.report(interp, &e, &ctx);
                if job.load_file {
                    break;
                }
            }
        }
    }
    ctl.settle();
    err.flush();
    out.flush();
    if job.load_file && !errored && !ctl.interrupted() {
        if let Some(v) = last {
            if let Err(e) = printer.reply_value_no_ns(interp, reply, &v) {
                let ctx = ErrorCtx { phase: Some(Phase::PrintEvalResult), ..ErrorCtx::default() };
                rep.report(interp, &e, &ctx);
            }
        }
    }
    crate::builtins::strings::set_fast_out(None);
    vars.out.set_binding(Value::Nil);
    vars.err.set_binding(Value::Nil);
    interp.def_file = None;
    // the request's source slot is freed with the feed: nothing may keep its id
    interp.source_id = crate::source_registry::UNKNOWN_SOURCE;
    drop(temp);
    interp.current_ns = interp.dynamic_ns_name();
    (interp.current_ns.clone(), true)
}

fn set_stars(vars: &Vars, value: &Value) {
    vars.set(STAR3, vars.get(STAR2));
    vars.set(STAR2, vars.get(STAR1));
    vars.set(STAR1, value.clone());
}

/// An interrupt that ends an eval without an `err` report: a loop back-edge
/// (soft) or the hard stage. The JVM's `ThreadDeath` case.
fn silent_interrupt(e: &RjError) -> bool {
    use crate::error::ErrorKind;
    e.kind == ErrorKind::InterruptedHard || (e.kind == ErrorKind::Interrupted && e.message == crate::interrupt::LOOP_INTERRUPT_MSG)
}

/// Reports an error: `*e`, the caught hook, `ex` / `root-ex` (design 5.3).
struct Reporter<'a> {
    vars: &'a Vars,
    err: &'a Sink,
    reply: &'a Responder,
    mode: ErrorMode,
    printer: &'a Printer,
    caught_fn: Option<&'a Value>,
    caught_print: bool,
}

impl Reporter<'_> {
    fn report(&self, interp: &mut Interp, e: &RjError, ctx: &ErrorCtx) {
        let info = error_info(interp, e, ctx);
        self.vars.set(STARE, info.exception.clone());
        match self.caught_fn {
            // a custom hook gets the throwable; it prints what it likes
            Some(f) => {
                let _ = interp.call(f, &[info.exception.clone()]);
            }
            None => {
                self.err.write_text(&err_text(&info, self.mode));
                self.err.flush();
            }
        }
        let ex = info.ex();
        let root = info.root_ex();
        let mut text = None;
        let mut truncated = false;
        if self.caught_print {
            let t = self.printer.throwable(interp, self.reply, &info.exception);
            text = t.text;
            truncated = t.truncated;
        }
        let mut f: Vec<(&str, V<'_>)> = vec![("ex", V::Str(&ex)), ("root-ex", V::Str(&root))];
        if truncated {
            f.push(("status", V::Strs(&[print::TRUNCATED_STATUS, "eval-error"])));
            f.push((print::TRUNCATED_KEYS_KEY, V::Strs(&[print::THROWABLE_KEY])));
        } else {
            f.push(("status", V::Strs(status::EVAL_ERROR)));
        }
        if let Some(t) = &text {
            f.push((print::THROWABLE_KEY, V::Str(t)));
        }
        self.reply.send(&f);
    }
}

fn short_file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}
