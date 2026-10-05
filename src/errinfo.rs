//! Error facts for a HOST (an nREPL server, a REPL, an IDE bridge).
//!
//! An [`RjError`] is mova's own error. A host that wants to print what
//! `clojure.main` prints, or to answer `ex` / `root-ex` the way the JVM
//! nREPL does, needs more than the message: the JVM class name, the root
//! cause class, the phase, and the source place. This module builds that
//! from an `RjError`:
//!
//! - [`error_info`] gives an [`ErrorInfo`] (class, root class, message,
//!   phase, file, line, column, the `clojure.main` text, and the exception
//!   VALUE a host can store in `*e`).
//! - [`exception_value`] builds just the exception value (also what a
//!   script `catch` binds, see `eval::special_forms::error_to_info_map`).
//!
//! No JVM frames exist. Where `clojure.main` prints `at user/eval7 (REPL:1)`
//! we print `user/eval<N>` (`N` is [`ErrorCtx::eval_id`]) at top level, or
//! `<ns>/<fn>` when the error carries a mova call frame.

use std::sync::Arc;

use crate::error::{ErrorKind, RjError};
use crate::eval::Interp;
use crate::keyword::Keyword;
use crate::value::{PMap, PVec, Str, Symbol, Value};

/// The `:clojure.error/phase` of an error, as `clojure.main/ex-triage` sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    ReadSource,
    MacroSyntaxCheck,
    Macroexpansion,
    CompileSyntaxCheck,
    Compilation,
    Execution,
    ReadEvalResult,
    PrintEvalResult,
}

impl Phase {
    /// The keyword name without the colon, e.g. `"read-source"`.
    pub fn keyword(self) -> &'static str {
        match self {
            Phase::ReadSource => "read-source",
            Phase::MacroSyntaxCheck => "macro-syntax-check",
            Phase::Macroexpansion => "macroexpansion",
            Phase::CompileSyntaxCheck => "compile-syntax-check",
            Phase::Compilation => "compilation",
            Phase::Execution => "execution",
            Phase::ReadEvalResult => "read-eval-result",
            Phase::PrintEvalResult => "print-eval-result",
        }
    }
}

/// What the host knows and mova does not.
#[derive(Debug, Clone, Default)]
pub struct ErrorCtx {
    /// Force a phase. `None`: reader error => `ReadSource`, unresolved
    /// symbol => `CompileSyntaxCheck`, everything else => `Execution`.
    pub phase: Option<Phase>,
    /// Number printed in `user/eval<N>`.
    pub eval_id: u64,
    /// The form is an `(ns ...)` form: the JVM names its code `eval<N>$loading`.
    pub loading: bool,
    /// Line / column to use when the error carries no span.
    pub fallback_line: Option<usize>,
    pub fallback_column: Option<usize>,
    /// Source name to use when it cannot be found (default `REPL`).
    pub file: Option<String>,
    /// ANSI colour in `ErrorInfo::report` (default off).
    pub colour: bool,
}

/// Everything a host needs about one error.
#[derive(Debug, Clone)]
pub struct ErrorInfo {
    /// JVM class name of the exception, e.g. `java.lang.ArithmeticException`.
    pub class: String,
    /// JVM class name of the root cause (`clojure.main/root-cause`: stops at
    /// a `Compiler$CompilerException`).
    pub root_class: String,
    /// `.getMessage` of the exception.
    pub message: Option<String>,
    /// `.getMessage` of the root cause.
    pub root_message: Option<String>,
    pub phase: Phase,
    /// Source name as given to the interpreter (short form is used in text).
    pub file: Option<String>,
    pub line: Option<usize>,
    pub column: Option<usize>,
    /// The text `clojure.main/err->msg` gives, e.g.
    /// `Execution error (ArithmeticException) at user/eval7 (REPL:1).\nDivide by zero\n`.
    pub text: String,
    /// The exception value (what `*e` holds).
    pub exception: Value,
    /// Mova's own rich diagnostic for the same error (rustc style: source
    /// snippet with real line numbers, underline and label, `help:`/`note:`
    /// lines, mova call frames, cause chain). It never repeats the header
    /// line or message of `text`; a server sends `text` alone in strict mode
    /// and `text` + blank line + `report` otherwise. `None` when mova has
    /// nothing to add.
    pub report: Option<String>,
}

impl ErrorInfo {
    /// `"class java.lang.ArithmeticException"`, the nREPL `ex` field.
    pub fn ex(&self) -> String {
        format!("class {}", self.class)
    }
    /// The nREPL `root-ex` field.
    pub fn root_ex(&self) -> String {
        format!("class {}", self.root_class)
    }
}

fn kw(s: &str) -> Value {
    Value::Keyword(Keyword::from(s))
}

const RT: &str = "java.lang.RuntimeException";
const EX: &str = "java.lang.Exception";
const TH: &str = "java.lang.Throwable";
pub const COMPILER_EXCEPTION: &str = "clojure.lang.Compiler$CompilerException";
pub const EXCEPTION_INFO: &str = "clojure.lang.ExceptionInfo";

fn owned(chain: &[&str]) -> Vec<String> {
    chain.iter().map(|s| s.to_string()).collect()
}

/// Own class first, then superclasses up to `Throwable`.
pub fn class_chain(err: &RjError) -> Vec<String> {
    if err.kind == ErrorKind::Other && err.message == "stack overflow" {
        return owned(&[
            "java.lang.StackOverflowError",
            "java.lang.VirtualMachineError",
            "java.lang.Error",
            TH,
        ]);
    }
    if err.kind == ErrorKind::Reader {
        return owned(&[RT, EX, TH]);
    }
    if err.kind == ErrorKind::Other && err.message.starts_with("could not locate namespace ") {
        return owned(&["java.io.FileNotFoundException", "java.io.IOException", EX, TH]);
    }
    crate::eval::special_forms::error_kind_class_chain(err)
        .into_iter()
        .map(|s| s.to_string())
        .collect()
}

/// Builds one exception instance. `getData` is the `ex-data` map (or nil).
pub fn mk_exception(chain: &[String], message: Option<String>, cause: Value, data: Value) -> Value {
    let tdef = Arc::new(crate::types::TypeDef {
        name: chain[0].as_str().into(),
        basis: vec!["getMessage".into(), "getCause".into(), "getData".into()],
        is_record: false,
        interfaces: chain[1..].iter().map(|s| s.as_str().into()).collect(),
        field_tags: Vec::new(),
        mutable: Vec::new(),
        methods: Default::default(),
        protocols: Vec::new(),
    });
    let msg = message.map(|m| Value::Str(Str::from(m))).unwrap_or(Value::Nil);
    Value::Inst(Arc::new(crate::types::InstVal {
        tdef,
        data: PMap::new(),
        fields: std::sync::Mutex::new([msg, cause, data].into_iter().collect()),
        meta: None,
    }))
}

/// `(throwable-field e "getData")`: a named basis field of a host exception
/// instance, or nil. Used by `ex-data` / `Throwable->map` in `core.mova`.
fn inst_field(v: &Value, name: &str) -> Value {
    if let Value::Inst(i) = v {
        if let Some(pos) = i.tdef.basis.iter().position(|b| b.as_ref() == name) {
            let f = crate::sync::lock_mutex(&i.fields);
            return f.get(pos).cloned().unwrap_or(Value::Nil);
        }
    }
    Value::Nil
}

fn str_of(v: &Value) -> Option<String> {
    match v {
        Value::Str(s) => Some(s.to_string()),
        _ => None,
    }
}

/// `.getMessage` of any throwable shape mova has.
pub fn throwable_message(v: &Value) -> Option<String> {
    match v {
        Value::Inst(_) => str_of(&inst_field(v, "getMessage")),
        Value::Map(m) => m
            .get(&kw("ex/message"))
            .and_then(str_of)
            .or_else(|| m.get(&kw("message")).and_then(str_of)),
        _ => None,
    }
}

/// `.getCause` of any throwable shape mova has.
pub fn throwable_cause(v: &Value) -> Option<Value> {
    let c = match v {
        Value::Inst(_) => inst_field(v, "getCause"),
        Value::Map(m) => m.get(&kw("ex/cause")).cloned().unwrap_or(Value::Nil),
        _ => Value::Nil,
    };
    if matches!(c, Value::Nil) {
        None
    } else {
        Some(c)
    }
}

/// JVM class name of a thrown value.
pub fn throwable_class(v: &Value) -> String {
    match v {
        Value::Inst(i) => i.tdef.name.to_string(),
        Value::Map(m) if m.get(&kw("ex/message")).is_some() => EXCEPTION_INFO.to_string(),
        Value::Nil => "java.lang.NullPointerException".to_string(),
        _ => RT.to_string(),
    }
}

/// `clojure.main/root-cause`: follow causes, but stop at a
/// `Compiler$CompilerException`.
pub fn root_cause(v: &Value) -> Value {
    let mut cur = v.clone();
    for _ in 0..1000 {
        if throwable_class(&cur) == COMPILER_EXCEPTION {
            return cur;
        }
        match throwable_cause(&cur) {
            Some(c) => cur = c,
            None => return cur,
        }
    }
    cur
}

fn short_file(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// JVM class name for a mova `type_name`, as it appears in cast messages.
fn jvm_type(t: &str) -> Option<&'static str> {
    Some(match t {
        "int" => "java.lang.Long",
        "float" => "java.lang.Double",
        "string" => "java.lang.String",
        "keyword" => "clojure.lang.Keyword",
        "symbol" => "clojure.lang.Symbol",
        "vector" => "clojure.lang.PersistentVector",
        "map" => "clojure.lang.PersistentArrayMap",
        "set" => "clojure.lang.PersistentHashSet",
        "list" => "clojure.lang.PersistentList",
        "bool" | "boolean" => "java.lang.Boolean",
        _ => return None,
    })
}

fn module_of(c: &str) -> &'static str {
    if c.starts_with("java.") {
        "module java.base of loader 'bootstrap'"
    } else {
        "unnamed module of loader 'app'"
    }
}

/// `class A cannot be cast to class B (...)`, the JVM text.
fn cast_message(from: &str, to: &str) -> String {
    let (mf, mt) = (module_of(from), module_of(to));
    if mf == mt {
        format!("class {from} cannot be cast to class {to} ({from} and {to} are in {mf})")
    } else {
        format!("class {from} cannot be cast to class {to} ({from} is in {mf}; {to} is in {mt})")
    }
}

/// Fixes mova's wording to the JVM's where the JVM text is well known.
/// `None` = the JVM message is null.
fn jvm_message(err: &RjError, for_host: bool) -> Option<String> {
    let m = &err.message;
    if let Some(t) = m.strip_prefix("don't know how to create a seq from ") {
        if let Some(c) = jvm_type(t.trim()) {
            return Some(format!("Don't know how to create ISeq from: {c}"));
        }
    }
    match err.kind {
        ErrorKind::Unresolved
            if m.starts_with("Unable to resolve symbol: ") && !m.contains(" in this context") =>
        {
            Some(format!("{m} in this context"))
        }
        ErrorKind::Arity if !m.starts_with("Wrong number of args") && m.contains(": expected ") => {
            let name = m.split(':').next().unwrap_or("");
            let got = m.rsplit("got ").next().unwrap_or("0").trim();
            let sym = if name.contains('/') { name.to_string() } else { format!("clojure.core/{name}") };
            Some(format!("Wrong number of args ({got}) passed to: {sym}"))
        }
        ErrorKind::TypeErr => {
            if let Some((_, got)) = m.split_once(": expected a number, got ") {
                if let Some(c) = jvm_type(got.trim()) {
                    return Some(cast_message(c, "java.lang.Number"));
                }
            }
            Some(m.clone())
        }
        ErrorKind::Other if m == "stack overflow" && for_host => None,
        // the JVM looks on the classpath; Mova's own wording stays in the report
        ErrorKind::Other if m.starts_with("could not locate namespace ") => {
            let ns = m["could not locate namespace ".len()..].split(' ').next().unwrap_or("");
            let base = ns.replace('.', "/").replace('-', "_");
            Some(format!("Could not locate {base}__init.class, {base}.clj or {base}.cljc on classpath."))
        }
        ErrorKind::Other if m.starts_with("nth: index ") && m.contains("for vector") => None,
        _ => {
            if let Some(t) = m.strip_prefix("don't know how to create a seq from ") {
                if let Some(c) = jvm_type(t.trim()) {
                    return Some(format!("Don't know how to create ISeq from: {c}"));
                }
            }
            Some(m.clone())
        }
    }
}

/// Maps a mova reader message to `(JVM class, JVM text)`.
fn jvm_reader_error(msg: &str, start_line: Option<usize>) -> (&'static str, String) {
    let eof = |what: &str| (RT, what.to_string());
    const IAE: &str = "java.lang.IllegalArgumentException";
    if msg.starts_with("unclosed string") || msg.contains("unterminated string") {
        return eof("EOF while reading string");
    }
    if msg.contains("character") && (msg.starts_with("unclosed") || msg.contains("EOF")) {
        return eof("EOF while reading character");
    }
    if msg.starts_with("unclosed") {
        return match start_line {
            Some(l) => (RT, format!("EOF while reading, starting at line {l}")),
            None => eof("EOF while reading"),
        };
    }
    if msg.contains("unexpected EOF") || msg.starts_with("expected a form after") && !msg.contains("tagged literal") {
        return eof("EOF while reading");
    }
    if msg.starts_with("expected a form after tagged literal") {
        return (RT, "Unreadable form".to_string());
    }
    if let Some(rest) = msg.strip_prefix("unexpected '") {
        let c = rest.chars().next().unwrap_or(')');
        return (RT, format!("Unmatched delimiter: {c}"));
    }
    if msg.starts_with("map literal must contain") {
        return (RT, "Map literal must contain an even number of forms".to_string());
    }
    if msg.starts_with("Duplicate key") {
        return (IAE, msg.to_string());
    }
    if msg == "Divide by zero" {
        return ("java.lang.ArithmeticException", msg.to_string());
    }
    if let Some(rest) = msg.strip_prefix("invalid number literal '") {
        return (
            "java.lang.NumberFormatException",
            format!("Invalid number: {}", rest.trim_end_matches('\'')),
        );
    }
    (RT, msg.to_string())
}

/// Where the JVM reader reports a read error, 1-based, against `text`.
fn reader_error_place(msg: &str, text: &str, span: crate::reader::Span) -> (usize, usize) {
    let eof_place = || {
        if text.ends_with('\n') {
            crate::error::line_col(text, text.len())
        } else {
            (crate::error::line_col(text, text.len()).0 + 1, 1)
        }
    };
    let incomplete = msg.starts_with("unclosed")
        || msg.contains("unexpected EOF")
        || (msg.starts_with("expected a form after") && !msg.contains("tagged literal"));
    if incomplete {
        return eof_place();
    }
    if msg.starts_with("expected a form after tagged literal") {
        let (l, c) = crate::error::line_col(text, span.start);
        return (l, c + 2);
    }
    // A token that runs to the end of the input: the reader also consumed
    // the newline the host appends.
    let token_error = msg == "Divide by zero"
        || msg.starts_with("No reader function")
        || msg.starts_with("invalid number literal")
        || msg.starts_with("Invalid token");
    if token_error && span.end >= text.trim_end_matches('\n').len() {
        return eof_place();
    }
    let mut end = span.end;
    if msg.starts_with("Duplicate key") {
        end = end_of_enclosing(text, span.end);
    }
    crate::error::line_col(text, end.min(text.len()))
}

/// Byte offset just after the closing delimiter of the collection that
/// contains `from`.
fn end_of_enclosing(text: &str, from: usize) -> usize {
    let mut depth = 0i32;
    let mut in_str = false;
    let mut esc = false;
    for (i, c) in text[from.min(text.len())..].char_indices() {
        if in_str {
            if esc {
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => {
                if depth == 0 {
                    return from + i + c.len_utf8();
                }
                depth -= 1;
            }
            _ => {}
        }
    }
    text.len()
}

/// Source name, text, and 1-based (line, column) of `byte` against the buffer
/// the error was raised in.
fn locate(interp: &Interp, err: &RjError, byte: usize) -> (Option<String>, Option<(usize, usize)>) {
    let (name, text) = match crate::source_registry::resolve(err.span_source_id) {
        Some((n, t)) => (n.to_string(), t),
        None => (interp.source_name.to_string(), interp.source.clone()),
    };
    if text.is_empty() {
        return (Some(name), None);
    }
    (Some(name), Some(crate::error::line_col(text.as_ref(), byte)))
}

struct Loc {
    file: Option<String>,
    line: Option<usize>,
    column: Option<usize>,
}

fn find_loc(interp: &Interp, err: &RjError, ctx: &ErrorCtx, phase: Phase, rf: &Refine) -> Loc {
    let mut file = ctx.file.clone();
    let mut line = ctx.fallback_line;
    let mut column = ctx.fallback_column;
    if let Some(span) = err.span {
        let (name, text) = source_of(Some(interp), err);
        if file.is_none() && !name.is_empty() {
            file = Some(name);
        }
        if !text.is_empty() {
            let (l, c) = if phase == Phase::ReadSource {
                reader_error_place(&err.message, &text, span)
            } else if is_compile_phase(phase) {
                crate::error::line_col(&text, enclosing_open(&text, span.start))
            } else {
                crate::error::line_col(&text, span.start)
            };
            line = Some(l);
            column = Some(c);
        }
    }
    if file.is_none() {
        file = Some(interp.source_name.to_string()).filter(|s| !s.is_empty());
    }
    if rf.zero_loc && is_compile_phase(phase) {
        line = Some(0);
        column = Some(0);
    }
    Loc { file, line, column }
}

fn is_compile_phase(p: Phase) -> bool {
    matches!(p, Phase::CompileSyntaxCheck | Phase::Compilation | Phase::MacroSyntaxCheck | Phase::Macroexpansion)
}

fn data_map(phase: Phase, loc: &Loc, symbol: Option<&str>) -> Value {
    let mut m = PMap::new();
    m.insert(kw("clojure.error/phase"), kw(phase.keyword()));
    if let Some(l) = loc.line {
        m.insert(kw("clojure.error/line"), Value::Int(l as i64));
    }
    if let Some(c) = loc.column {
        m.insert(kw("clojure.error/column"), Value::Int(c as i64));
    }
    if let Some(f) = &loc.file {
        m.insert(kw("clojure.error/source"), Value::Str(Str::from(f.as_str())));
    }
    if let Some(sym) = symbol {
        let (ns, name) = match sym.split_once('/') {
            Some((a, b)) => (Some(Str::from(a)), b),
            None => (None, sym),
        };
        m.insert(kw("clojure.error/symbol"), Value::Sym(Symbol { ns, name: Str::from(name) }));
    }
    Value::Map(m)
}

fn loc_text(loc: &Loc) -> String {
    format!(
        "({}:{}:{})",
        loc.file.as_deref().map(short_file).unwrap_or("REPL"),
        loc.line.unwrap_or(1),
        loc.column.unwrap_or(1)
    )
}

fn mk_ex_info(msg: String, data: Value, cause: Value) -> Value {
    let mut m = PMap::new();
    m.insert(kw("ex/message"), Value::Str(Str::from(msg)));
    m.insert(kw("ex/data"), data);
    if !matches!(cause, Value::Nil) {
        m.insert(kw("ex/cause"), cause);
    }
    Value::Map(m)
}

/// `(ex-info nil data cause)`: the wrapper nREPL puts around an error of the
/// print phase (`ex` is `ExceptionInfo`, `root-ex` the original class).
fn mk_ex_info_nil(data: Value, cause: Value) -> Value {
    let mut m = PMap::new();
    m.insert(kw("ex/message"), Value::Nil);
    m.insert(kw("ex/data"), data);
    m.insert(kw("ex/cause"), cause);
    Value::Map(m)
}

/// Builds the exception value for a non-thrown `RjError`. For
/// `ErrorKind::Thrown` the thrown value itself is returned.
fn build(
    interp: Option<&Interp>,
    err: &RjError,
    ctx: &ErrorCtx,
    phase: Phase,
    loc: &Loc,
    rf: &Refine,
) -> Value {
    if err.kind == ErrorKind::Thrown {
        let v = err.thrown.clone().unwrap_or(Value::Nil);
        // Host path only: the JVM rejects `(throw 1)` / `(throw nil)`.
        if interp.is_some() {
            match &v {
                Value::Nil => {
                    return mk_exception(
                        &owned(&["java.lang.NullPointerException", RT, EX, TH]),
                        Some("Cannot throw exception because \"null\" is null".into()),
                        Value::Nil,
                        Value::Nil,
                    )
                }
                Value::Inst(_) => {}
                Value::Map(m) if m.get(&kw("ex/message")).is_some() => {}
                other => {
                    let from = jvm_type(other.type_name()).unwrap_or("java.lang.Object");
                    return mk_exception(
                        &owned(&["java.lang.ClassCastException", RT, EX, TH]),
                        Some(cast_message(from, "java.lang.Throwable")),
                        Value::Nil,
                        Value::Nil,
                    );
                }
            }
        }
        if phase == Phase::PrintEvalResult {
            return mk_ex_info_nil(data_map(phase, loc, None), v);
        }
        return v;
    }
    let chain = class_chain(err);
    let msg_opt = jvm_message(err, interp.is_some());
    match phase {
        Phase::ReadSource => {
            let start_line = err.span.and_then(|s| {
                interp.and_then(|i| {
                    let (_, lc) = locate(i, err, s.start);
                    lc.map(|(l, _)| l)
                })
            });
            let (cls, text) = jvm_reader_error(&err.message, start_line);
            let inner_chain = vec![cls.to_string(), EX.to_string(), TH.to_string()];
            let inner = mk_exception(&inner_chain, Some(text), Value::Nil, Value::Nil);
            mk_ex_info(
                format!("Syntax error reading source at {}.", loc_text(loc)),
                data_map(phase, loc, None),
                inner,
            )
        }
        Phase::CompileSyntaxCheck | Phase::Compilation | Phase::MacroSyntaxCheck | Phase::Macroexpansion => {
            let (inner_chain, inner_msg) = match &rf.cause {
                Some((cls, m)) => (owned(&[cls, RT, EX, TH]), Some(m.clone())),
                None if chain.first().map(String::as_str) == Some(COMPILER_EXCEPTION) => {
                    (owned(&[RT, EX, TH]), msg_opt)
                }
                None => (chain.clone(), msg_opt),
            };
            let inner_cls = inner_chain[0].clone();
            let inner = mk_exception(&inner_chain, inner_msg, Value::Nil, Value::Nil);
            let head = match (phase, &rf.symbol) {
                (Phase::MacroSyntaxCheck, Some(sym)) => format!("Syntax error macroexpanding {sym} at"),
                (_, Some(sym)) => {
                    let short = inner_cls.rsplit('.').next().unwrap_or("");
                    if inner_cls == RT || inner_cls == EX {
                        format!("Syntax error compiling {sym} at")
                    } else {
                        format!("Syntax error ({short}) compiling {sym} at")
                    }
                }
                _ => "Syntax error compiling at".to_string(),
            };
            mk_exception(
                &owned(&[COMPILER_EXCEPTION, RT, EX, TH]),
                Some(format!("{head} {}.", loc_text(loc))),
                inner,
                data_map(phase, loc, rf.symbol.as_deref()),
            )
        }
        Phase::PrintEvalResult => {
            let inner = mk_exception(&chain, msg_opt, Value::Nil, Value::Nil);
            mk_ex_info_nil(data_map(phase, loc, None), inner)
        }
        _ => {
            let _ = ctx;
            mk_exception(&chain, msg_opt, Value::Nil, Value::Nil)
        }
    }
}

/// Facts that mova's error does not carry but its message and source text
/// show: which compile-time shape the JVM would report.
#[derive(Default, Clone)]
struct Refine {
    phase: Option<Phase>,
    /// `:clojure.error/symbol` (the form being compiled/expanded).
    symbol: Option<String>,
    /// JVM class and message of the compile error's cause.
    cause: Option<(&'static str, String)>,
    /// A bare unresolved symbol at top level is reported at 0:0 by the JVM.
    zero_loc: bool,
    /// Extra help line for the report.
    help: Option<String>,
    note: Option<String>,
}

fn bracket_depth_at(text: &str, upto: usize) -> i32 {
    let mut depth = 0;
    let (mut in_str, mut esc, mut in_cmt) = (false, false, false);
    for (i, c) in text.char_indices() {
        if i >= upto {
            break;
        }
        if in_cmt {
            if c == '\n' {
                in_cmt = false;
            }
            continue;
        }
        if in_str {
            if esc {
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            ';' => in_cmt = true,
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            _ => {}
        }
    }
    depth
}

fn refine(err: &RjError, text: &str) -> Refine {
    let mut r = Refine::default();
    let m = err.message.as_str();
    let span = err.span;
    let slice = span.and_then(|s| text.get(s.start..s.end.min(text.len())));
    if err.kind == ErrorKind::Other {
        for (head, sym) in [("let", "clojure.core/let"), ("loop", "clojure.core/loop")] {
            if m.starts_with(&format!("{head}: bindings must be an even number")) {
                let b = slice.unwrap_or("[]").to_string();
                r.phase = Some(Phase::MacroSyntaxCheck);
                r.symbol = Some(sym.to_string());
                r.cause = Some((
                    "clojure.lang.ExceptionInfo",
                    format!(
                        "{b} - failed: even-number-of-forms? at: [:bindings] spec: :clojure.core.specs.alpha/bindings"
                    ),
                ));
                r.help = Some("every binding needs a name and a value; add the missing value or remove the name".into());
                r.note = Some(format!("`{head}` was macroexpanded with the bindings {b}"));
                return r;
            }
        }
    }
    if err.kind == ErrorKind::Arity {
        // `(if)` / `(def)`: special forms checked by the compiler.
        if let Some((head, rest)) = m.split_once(": expected ") {
            if (head == "if" || head == "def") && rest.contains(" or ") {
                let nums: Vec<i64> = rest
                    .split(|c: char| !c.is_ascii_digit())
                    .filter_map(|t| t.parse().ok())
                    .collect();
                if nums.len() >= 3 {
                    let few = nums[2] < nums[0];
                    r.phase = Some(Phase::CompileSyntaxCheck);
                    r.symbol = Some(head.to_string());
                    r.cause = Some((
                        RT,
                        format!("{} arguments to {head}", if few { "Too few" } else { "Too many" }),
                    ));
                    r.help = Some(format!("`{head}` takes {} or {} arguments", nums[0], nums[1]));
                }
            }
        }
    }
    if err.kind == ErrorKind::Unresolved {
        r.phase = Some(Phase::CompileSyntaxCheck);
        if let Some(sym) = m.strip_prefix("Unable to resolve symbol: ") {
            let sym = sym.trim_end_matches(" in this context");
            if let Some((ns, _)) = sym.split_once('/') {
                if !ns.is_empty() {
                    r.cause = Some((RT, format!("No such namespace: {ns}")));
                    r.help = Some(format!("namespace `{ns}` is not loaded; try (require '{ns})"));
                }
            } else if let Some(s) = span {
                let before = text.get(..s.start).unwrap_or("").trim_end();
                if before.ends_with("(new") {
                    r.symbol = Some("new".into());
                    r.cause = Some(("java.lang.IllegalArgumentException", format!("Unable to resolve classname: {sym}")));
                    r.help = Some(format!("class `{sym}` is unknown; use a fully qualified class name or import it"));
                } else if bracket_depth_at(text, s.start) == 0 {
                    r.zero_loc = true;
                }
            }
        }
    }
    r
}

fn default_phase(err: &RjError, rf: &Refine) -> Phase {
    if let Some(p) = rf.phase {
        return p;
    }
    match err.kind {
        ErrorKind::Reader => Phase::ReadSource,
        ErrorKind::Unresolved => Phase::CompileSyntaxCheck,
        _ => Phase::Execution,
    }
}

/// The closure a mova call frame names, if it can be found.
fn frame_closure(interp: &Interp, name: &str) -> Option<std::sync::Arc<crate::value::Closure>> {
    let mut cands: Vec<Symbol> = Vec::new();
    match name.split_once('/') {
        Some((ns, n)) => cands.push(Symbol { ns: Some(ns.into()), name: n.into() }),
        None => {
            cands.push(Symbol { ns: Some(interp.current_ns.clone()), name: name.into() });
            cands.push(Symbol::simple(name));
        }
    }
    cands.into_iter().find_map(|s| match interp.lookup_global(&s) {
        Some(Value::Fn(c)) => Some(c),
        _ => None,
    })
}

/// The buffer id an error's span really belongs to. An error raised inside a
/// fn defined in an EARLIER eval (or file) carries a span relative to that
/// fn's own buffer, which mova records on the closure (`def_source_id`);
/// `err.span_source_id` is only the buffer current at throw time.
fn eff_source_id(interp: &Interp, err: &RjError) -> u32 {
    if let Some(f) = err.stack.last() {
        if let Some(c) = frame_closure(interp, f.name.as_ref()) {
            if c.def_source_id.get() != crate::source_registry::UNKNOWN_SOURCE
                && err.span.is_some_and(|s| {
                    crate::source_registry::resolve(c.def_source_id.get()).is_some_and(|(_, t)| s.end <= t.len())
                })
            {
                return c.def_source_id.get();
            }
        }
    }
    err.span_source_id
}

/// Start of the innermost `(` form that contains `pos` (or `pos` itself when
/// it is the head right after a `(`): where the JVM compiler reports an error.
fn enclosing_open(text: &str, pos: usize) -> usize {
    let mut depth = 0i32;
    for (i, c) in text[..pos.min(text.len())].char_indices().rev() {
        match c {
            ')' | ']' | '}' => depth += 1,
            '(' => {
                if depth == 0 {
                    return i;
                }
                depth -= 1;
            }
            '[' | '{' => {
                if depth > 0 {
                    depth -= 1;
                }
            }
            _ => {}
        }
    }
    pos
}

/// The buffer an error's span belongs to: `(name, text)`.
fn source_of(interp: Option<&Interp>, err: &RjError) -> (String, String) {
    let id = interp.map(|i| eff_source_id(i, err)).unwrap_or(err.span_source_id);
    match crate::source_registry::resolve(id) {
        Some((n, t)) => (n.to_string(), t.to_string()),
        None => match interp {
            Some(i) => (i.source_name.to_string(), i.source.to_string()),
            None => (String::new(), String::new()),
        },
    }
}

/// The exception value for `err`, without a host context. This is what a
/// script `catch` clause binds. Compile-phase errors are
/// `Compiler$CompilerException` with the real error as cause; everything
/// else is an instance of its JVM class.
pub fn exception_value(err: &RjError) -> Value {
    let ctx = ErrorCtx::default();
    let (_, src_text) = source_of(None, err);
    let rf = refine(err, &src_text);
    let phase = default_phase(err, &rf);
    let loc = {
        let mut loc = Loc { file: None, line: None, column: None };
        if let (Some(span), Some((n, t))) = (err.span, crate::source_registry::resolve(err.span_source_id)) {
            let off = if is_compile_phase(phase) { enclosing_open(t.as_ref(), span.start) } else { span.start };
            let (l, c) = crate::error::line_col(t.as_ref(), off);
            loc = Loc { file: Some(n.to_string()), line: Some(l), column: Some(c) };
        }
        loc
    };
    // A script-level reader error (`read-string`, `load-string`) is a plain
    // `RuntimeException`, as on the JVM.
    if err.kind == ErrorKind::Reader {
        let (cls, text) = jvm_reader_error(&err.message, None);
        return mk_exception(&owned(&[cls, EX, TH]), Some(text), Value::Nil, Value::Nil);
    }
    build(None, err, &ctx, phase, &loc, &rf)
}

fn frame_symbol(interp: &Interp, err: &RjError, ctx: &ErrorCtx) -> (String, String) {
    // (class symbol `ns$fn`, method) as a JVM trace element would carry.
    let ns = interp.current_ns.to_string();
    let name = err
        .stack
        .last()
        .map(|f| f.name.to_string())
        .filter(|n| !n.is_empty() && n != "anonymous-fn" && !n.starts_with('('));
    match name {
        Some(n) if n.contains('/') => {
            let (a, b) = n.split_once('/').unwrap();
            (format!("{a}${b}"), "invoke".to_string())
        }
        Some(n) => (format!("{ns}${n}"), "invoke".to_string()),
        None if ctx.loading => (format!("{ns}$eval{}$loading", ctx.eval_id), "invoke".to_string()),
        None => (format!("{ns}$eval{}", ctx.eval_id), "invoke".to_string()),
    }
}

/// The `loc` map `clojure.main/err->msg*` takes: phase and one trace element.
fn loc_map(interp: &Interp, err: &RjError, ctx: &ErrorCtx, phase: Phase, loc: &Loc) -> Value {
    let (clazz, method) = frame_symbol(interp, err, ctx);
    let file = loc.file.as_deref().map(short_file).unwrap_or("REPL");
    let frame = Value::Vector(PVec::from_iter([
        Value::Sym(Symbol::simple(clazz)),
        Value::Sym(Symbol::simple(method)),
        Value::Str(Str::from(file)),
        Value::Int(loc.line.unwrap_or(1) as i64),
    ]));
    let mut m = PMap::new();
    m.insert(kw("phase"), kw(phase.keyword()));
    m.insert(kw("trace"), Value::Vector(PVec::from_iter([frame])));
    Value::Map(m)
}

/// Facts about `err` for a host. `interp` is used for the current ns, the
/// current source buffer and (for the text) calling `clojure.main/err->msg*`.
pub fn error_info(interp: &mut Interp, err: &RjError, ctx: &ErrorCtx) -> ErrorInfo {
    let (_, src_text) = source_of(Some(interp), err);
    let rf = refine(err, &src_text);
    let phase = ctx.phase.unwrap_or_else(|| default_phase(err, &rf));
    let loc = find_loc(interp, err, ctx, phase, &rf);
    let exception = build(Some(interp), err, ctx, phase, &loc, &rf);
    let class = throwable_class(&exception);
    let root = root_cause(&exception);
    let root_class = throwable_class(&root);
    let message = throwable_message(&exception);
    let root_message = throwable_message(&root);
    let locv = loc_map(interp, err, ctx, phase, &loc);
    let text = render_text(interp, &exception, locv).unwrap_or_else(|| {
        format!("{}: {}\n", class, message.clone().unwrap_or_default())
    });
    let report = build_report(interp, err, ctx, &class, &rf, &exception, &text);
    ErrorInfo {
        class,
        root_class,
        message,
        root_message,
        phase,
        file: loc.file,
        line: loc.line,
        column: loc.column,
        text,
        exception,
        report,
    }
}

fn lev(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut cur = vec![i];
        for j in 1..=b.len() {
            let c = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            cur.push((prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + c));
        }
        prev = cur;
    }
    prev[b.len()]
}

fn did_you_mean(interp: &Interp, name: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut cands: Vec<(usize, String)> = Vec::new();
    let max = (name.chars().count() / 3).clamp(1, 3);
    for ns in [interp.current_ns.clone(), Str::from(crate::ns::CORE_NS)] {
        for n in interp.globals.names_in_ns(&ns) {
            let n = n.to_string();
            let bare = n.rsplit('/').next().unwrap_or(&n).to_string();
            if bare == name || !seen.insert(bare.clone()) {
                continue;
            }
            let d = lev(name, &bare);
            if d <= max {
                cands.push((d, bare));
            }
        }
    }
    cands.sort();
    cands.into_iter().take(3).map(|(_, n)| n).collect()
}

fn arglists_of(c: &crate::value::Closure) -> String {
    c.arities
        .iter()
        .map(|a| {
            let mut ps: Vec<String> = a.params.iter().map(|p| p.name.to_string()).collect();
            if let Some(r) = &a.rest {
                ps.push("&".into());
                ps.push(r.name.to_string());
            }
            format!("[{}]", ps.join(" "))
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Mova's own diagnostic for `err`: no header, no message already in `text`.
fn build_report(
    interp: &Interp,
    err: &RjError,
    ctx: &ErrorCtx,
    class: &str,
    rf: &Refine,
    exception: &Value,
    text_shown: &str,
) -> Option<String> {
    let (name, text) = source_of(Some(interp), err);
    let m = err.message.as_str();
    let mut help: Vec<String> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    let mut label: Option<String> = None;
    match err.kind {
        ErrorKind::Unresolved => {
            let sym = m.strip_prefix("Unable to resolve symbol: ").unwrap_or(m);
            label = Some(format!("`{sym}` is not defined in namespace {}", interp.current_ns));
            if !sym.contains('/') {
                let ds = did_you_mean(interp, sym);
                if !ds.is_empty() {
                    help.push(format!(
                        "did you mean {}?",
                        ds.iter().map(|d| format!("`{d}`")).collect::<Vec<_>>().join(", ")
                    ));
                }
            }
        }
        ErrorKind::Arity => {
            label = Some(m.to_string());
            if let Some(fname) = m.strip_prefix("Wrong number of args (").and_then(|r| r.split_once(") passed to: ")).map(|(_, f)| f) {
                let sym = match fname.split_once('/') {
                    Some((ns, n)) => Symbol { ns: Some(ns.into()), name: n.into() },
                    None => Symbol::simple(fname),
                };
                if let Some(Value::Fn(c)) = interp.lookup_global(&sym) {
                    help.push(format!("`{fname}` accepts: {}", arglists_of(&c)));
                }
            }
        }
        ErrorKind::DivideByZero => label = Some("the divisor is zero here".into()),
        ErrorKind::TypeErr => label = Some(m.to_string()),
        ErrorKind::Reader => {
            label = Some(err.label.clone().unwrap_or_else(|| m.to_string()));
            if m.starts_with("unclosed") {
                help.push("add the missing closing delimiter, or remove this opening one".into());
            }
        }
        _ => {
            if err.kind != ErrorKind::Thrown {
                label = Some(m.to_string());
            }
        }
    }
    if let Some(h) = &rf.help {
        help.push(h.clone());
    }
    if let Some(n) = &rf.note {
        notes.push(n.clone());
    }
    let jvm = jvm_message(err, true);
    if err.kind == ErrorKind::TypeErr || (err.kind == ErrorKind::Other && jvm.as_deref() != Some(m)) {
        // keep mova's own wording where the JVM text differs
    }
    // Thrown values: show ex-data and the cause chain.
    if err.kind == ErrorKind::Thrown {
        if let Value::Map(mm) = exception {
            if let Some(d) = mm.get(&kw("ex/data")) {
                if !matches!(d, Value::Nil) {
                    notes.push(format!("ex-data: {}", crate::printer::pr_str(d)));
                }
            }
        }
    }
    // The cause chain: only the links whose message `text` does not show.
    let shown = |msg: &str| text_shown.lines().any(|l| l.trim() == msg.trim());
    let mut cur = Some(exception.clone());
    let mut guard = 0;
    while let Some(c) = cur {
        guard += 1;
        if guard > 20 {
            break;
        }
        let msg = throwable_message(&c).unwrap_or_default();
        let has_cause = throwable_cause(&c).is_some();
        // a CompilerException's message is the header line `text` already has
        let header_like = matches!(
            throwable_class(&c).as_str(),
            "clojure.lang.Compiler$CompilerException" | "java.util.concurrent.ExecutionException"
        );
        if !msg.is_empty() && !header_like && !shown(&msg) && (has_cause || guard > 1) {
            notes.push(format!("{} (cause chain): {msg}", throwable_class(&c)));
        }
        cur = throwable_cause(&c);
    }
    if class != "java.lang.Object" && err.kind == ErrorKind::Thrown && !matches!(err.thrown, Some(Value::Inst(_)) | Some(Value::Map(_))) {
        notes.push("mova lets any value be thrown; the JVM only accepts a Throwable".into());
    }
    let mut out = String::new();
    let mut e2 = err.clone();
    e2.span_source_id = eff_source_id(interp, err);
    if let Some(sn) = crate::error::render_snippet(&e2, &name, &text, label.as_deref(), ctx.colour) {
        out.push_str(&sn);
    }
    // Frames, innermost first. Frame k's span is a call site inside the body
    // of frame k-1's callee (a fn that may come from an earlier eval or file),
    // or in the current buffer for the outermost frame.
    let n = err.stack.len();
    for k in (0..n).rev() {
        let f = &err.stack[k];
        let id = if k == 0 {
            f.source_id
        } else {
            frame_closure(interp, err.stack[k - 1].name.as_ref())
                .map(|c| c.def_source_id.get())
                .filter(|id| {
                    crate::source_registry::resolve(*id).is_some_and(|(_, t)| f.span.end <= t.len())
                })
                .unwrap_or(f.source_id)
        };
        let (fname, ftext) = match crate::source_registry::resolve(id) {
            Some((nm, t)) => (nm.to_string(), t.to_string()),
            None => (interp.source_name.to_string(), interp.source.to_string()),
        };
        let (l, c) = crate::error::line_col(&ftext, f.span.start);
        let qn = if f.name.contains('/') { f.name.to_string() } else { format!("{}/{}", interp.current_ns, f.name) };
        out.push_str(&format!("  at {} ({}:{}:{})\n", qn, short_file(&fname), l, c));
    }
    for h in help {
        out.push_str(&format!("  help: {h}\n"));
    }
    for n in notes {
        out.push_str(&format!("  note: {n}\n"));
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

fn render_text(interp: &mut Interp, exception: &Value, locv: Value) -> Option<String> {
    let sym = Symbol { ns: Some("clojure.main".into()), name: "err->msg*".into() };
    let f = interp.lookup_global(&sym)?;
    match interp.call(&f, &[exception.clone(), locv]) {
        Ok(Value::Str(s)) => Some(s.to_string()),
        _ => None,
    }
}

/// Registers `(throwable-field e "name")` for `core.mova`.
pub fn register(i: &mut Interp) {
    use crate::builtins::{reg, ArityHint};
    // `(fn-unmunged-name f)`: the plain name of a named fn / native, else nil.
    reg(i, "fn-unmunged-name", ArityHint::Exact(1), |_i, args| {
        Ok(match &args[0] {
            Value::Fn(c) | Value::Macro(c) => c.name.clone().map(Value::Str).unwrap_or(Value::Nil),
            Value::Native(n) => Value::Str(Str::from(n.name.as_ref())),
            _ => Value::Nil,
        })
    });
    reg(i, "throwable-field", ArityHint::Exact(2), |_i, args| {
        let name = match &args[1] {
            Value::Str(s) => s.to_string(),
            _ => return Ok(Value::Nil),
        };
        Ok(inst_field(&args[0], &name))
    });
}
