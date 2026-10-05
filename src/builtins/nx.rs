//! `mova.nx/*` natives: binding to the nx-core crate (nx DESIGN.md).
//! JSON is NOT here: `mova.json/parse-string`/`generate-string` already exist (io.rs).

use std::sync::{Arc, OnceLock};

use crate::embed::host::{wrap_struct, Shape, ShapeBuilder};
use crate::embed::Value as EV;
use crate::error::RjError;
use crate::eval::Interp;
use crate::value::{Keyword, Str, Symbol, Value};
use nx_core::engine::lsp::Diagnostic;
use nx_core::engine::{Done, Engine, Snapshot};

#[track_caller] // the source index records the call site of each registration
fn def(i: &mut Interp, name: &str, f: impl Fn(&mut Interp, &[Value]) -> Result<Value, RjError> + Send + Sync + 'static) {
    let native = crate::value::NativeFn::new(name, f);
    i.globals.set_builtin(
        Symbol { ns: Some(Str::from("mova.nx")), name: Str::from(name) },
        Value::Native(Arc::new(native)),
    );
}

fn kw(n: &str) -> Value {
    Value::Keyword(Keyword::construct(n))
}

fn map(pairs: Vec<(&str, Value)>) -> Value {
    Value::Map(pairs.into_iter().map(|(k, v)| (kw(k), v)).collect())
}

fn sv(s: &str) -> Value {
    Value::Str(Str::from(s.to_string()))
}

// ---- opaque handles (HostStruct views; only a few small fields are visible to Mova) ----
struct EngineH(Arc<Engine>);
struct SnapH(Arc<Snapshot>);
struct ResultsH {
    /// Taken by `commit!` (the store then owns the entries and finishes them in place).
    done: std::sync::Mutex<Vec<Done>>,
    /// (batch id, done, total) of the background batch these results belong to, at receive time.
    bg: Option<(u64, usize, usize)>,
}

fn ev<T: Into<EV>>(t: T) -> EV {
    t.into()
}

fn engine_shape() -> &'static Shape<EngineH> {
    static S: OnceLock<Shape<EngineH>> = OnceLock::new();
    S.get_or_init(|| ShapeBuilder::<EngineH>::new("nx.Engine").field("workers", |e| ev(e.0.pool.workers as i64)).build())
}
fn snap_shape() -> &'static Shape<SnapH> {
    static S: OnceLock<Shape<SnapH>> = OnceLock::new();
    S.get_or_init(|| {
        ShapeBuilder::<SnapH>::new("nx.Snapshot")
            .field("version", |s| ev(s.0.version as i64))
            .field("files", |s| ev(s.0.file_count as i64))
            .build()
    })
}
fn results_shape() -> &'static Shape<ResultsH> {
    static S: OnceLock<Shape<ResultsH>> = OnceLock::new();
    S.get_or_init(|| {
        let opt = |v: Option<i64>| match v {
            Some(x) => ev(x),
            None => EV::from(()),
        };
        ShapeBuilder::<ResultsH>::new("nx.Results")
            .field("n", |r| ev(r.done.lock().unwrap().len() as i64))
            .field("bg-batch", move |r| opt(r.bg.map(|b| b.0 as i64)))
            .field("bg-done", move |r| opt(r.bg.map(|b| b.1 as i64)))
            .field("bg-total", move |r| opt(r.bg.map(|b| b.2 as i64)))
            .build()
    })
}

fn wrap<T: std::any::Any + Send + Sync>(t: T, shape: &Shape<T>) -> Value {
    wrap_struct(Arc::new(t), shape).into_inner()
}

fn handle<'a, T: std::any::Any>(v: &'a Value, what: &str) -> Result<&'a T, RjError> {
    if let Value::HostStruct(hs) = v {
        if let Some(t) = crate::host_struct::downcast_ref::<T>(hs) {
            return Ok(t);
        }
    }
    Err(RjError::type_err(format!("mova.nx: expected {what} handle")))
}

fn want_str<'a>(a: &'a [Value], i: usize, op: &str) -> Result<&'a str, RjError> {
    match a.get(i) {
        Some(Value::Str(s)) => Ok(s.as_ref()),
        _ => Err(RjError::type_err(format!("mova.nx/{op}: arg {i} must be a string"))),
    }
}

fn arity(a: &[Value], n: usize, op: &str) -> Result<(), RjError> {
    if a.len() != n {
        return Err(RjError::arity(format!("mova.nx/{op}: expected {n} args")));
    }
    Ok(())
}

static ENGINE: OnceLock<Arc<Engine>> = OnceLock::new();

fn the_engine(workers: usize) -> Arc<Engine> {
    ENGINE
        .get_or_init(|| {
            nx_core::jdk::set_release_hook(crate::memstat::collect);
            nx_core::clojuredocs::set_fetcher(|url| {
                use std::io::Read;
                let r = crate::http::get(url, Some(5000), Some(30000))?;
                if r.status != 200 {
                    return Err(format!("HTTP {}", r.status));
                }
                let mut v = Vec::new();
                r.body.take(64 << 20).read_to_end(&mut v).map_err(|e| e.to_string())?;
                Ok(v)
            });
            nx_core::engine::set_trim_hook(crate::memstat::collect);
            Engine::new(workers)
        })
        .clone()
}

fn json_quote(t: &str) -> String {
    let mut o = String::with_capacity(t.len() + 2);
    o.push('"');
    for c in t.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn diag_value(d: &Diagnostic) -> Value {
    let pos = |l: u32, c: u32| map(vec![("line", Value::Int(l as i64)), ("character", Value::Int(c as i64))]);
    map(vec![
        ("range", map(vec![("start", pos(d.line, d.character)), ("end", pos(d.end_line, d.end_character))])),
        ("tags", Value::Vector(d.tags.iter().map(|t| Value::Int(*t as i64)).collect())),
        ("message", sv(&d.message)),
        ("code", sv(&d.code)),
        ("severity", Value::Int(d.severity as i64)),
        ("source", sv(d.source)),
    ])
}

pub fn register(i: &mut Interp) {
    def(i, "version", |_i, _a| Ok(sv(nx_core::version())));
    // (mova.nx/apply-edit text sl sc el ec new): one LSP incremental change, UTF-16 cols.
    def(i, "apply-edit", |_i, a| {
        arity(a, 6, "apply-edit")?;
        let (Value::Str(t), Value::Str(n)) = (&a[0], &a[5]) else {
            return Err(RjError::type_err("mova.nx/apply-edit: text and new-text must be strings".to_string()));
        };
        let num = |v: &Value| match v {
            Value::Int(x) if *x >= 0 => Ok(*x as u64),
            _ => Err(RjError::type_err("mova.nx/apply-edit: positions must be non-negative ints".to_string())),
        };
        Ok(Value::Str(Str::from(apply_edit(t.as_ref(), num(&a[1])?, num(&a[2])?, num(&a[3])?, num(&a[4])?, n.as_ref()))))
    });
    // (mova.nx/engine) | (mova.nx/engine workers): the process singleton (workers only used on first call).
    def(i, "engine", |_i, a| {
        let w = match a.first() {
            Some(Value::Int(n)) if *n > 0 => *n as usize,
            _ => 0,
        };
        Ok(wrap(EngineH(the_engine(w)), engine_shape()))
    });
    // (mova.nx/analyze-text eng uri version text) -> nil. Queues at high priority; never blocks.
    def(i, "analyze-text", |_i, a| {
        arity(a, 4, "analyze-text")?;
        let e = handle::<EngineH>(&a[0], "engine")?;
        let v = match &a[2] {
            Value::Int(v) => *v,
            _ => return Err(RjError::type_err("mova.nx/analyze-text: version must be int".to_string())),
        };
        let (uri, text) = (want_str(a, 1, "analyze-text")?, want_str(a, 3, "analyze-text")?);
        e.0.analyze_text(uri, v, text.to_string());
        e.0.mova_open(uri, text); // a `.mova` file the project pass did not cover: Mova layer + its module dir (background)
        Ok(Value::Nil)
    });
    // (mova.nx/analyze-disk eng uri) -> nil. Re-read from disk, replacing an open-doc entry (after didClose).
    def(i, "analyze-disk", |_i, a| {
        arity(a, 2, "analyze-disk")?;
        handle::<EngineH>(&a[0], "engine")?.0.analyze_disk_override(want_str(a, 1, "analyze-disk")?);
        Ok(Value::Nil)
    });
    // (mova.nx/analyze-project eng root-uri-or-path) -> {:batch :total :source-paths}. Does fs IO: call from an :io proc.
    def(i, "analyze-project", |_i, a| {
        arity(a, 2, "analyze-project")?;
        let e = handle::<EngineH>(&a[0], "engine")?.0.clone();
        let arg = want_str(a, 1, "analyze-project")?;
        let root = nx_core::engine::scan::uri_to_path(arg).unwrap_or_else(|| std::path::PathBuf::from(arg));
        let (batch, total, sps) = e.analyze_project(&root);
        for (uri, _, text) in e.pool.open_docs() {
            e.mova_open(&uri, &text); // docs opened before the project was known
        }
        Ok(map(vec![
            ("batch", Value::Int(batch as i64)),
            ("total", Value::Int(total as i64)),
            ("source-paths", Value::Vector(sps.iter().map(|s| sv(s)).collect())),
        ]))
    });
    // (mova.nx/await eng) -> Results handle | nil when closed. BLOCKS: call from an :io thread only.
    def(i, "await", |_i, a| {
        arity(a, 1, "await")?;
        let e = handle::<EngineH>(&a[0], "engine")?.0.clone();
        let Some(done) = e.await_results(64) else { return Ok(Value::Nil) };
        let bg = done.iter().map(|d| d.batch).find(|b| *b != 0).and_then(|b| e.pool.batch_progress(b).map(|(d, t)| (b, d, t)));
        Ok(wrap(ResultsH { done: std::sync::Mutex::new(done), bg }, results_shape()))
    });
    // (mova.nx/commit! eng results) -> {:snap Snapshot :changed [{:uri :version}] :dropped n}. Store proc only.
    // :changed lists open-doc entries only (version >= 0); background disk entries stay inside the store.
    def(i, "commit!", |_i, a| {
        arity(a, 2, "commit!")?;
        let e = handle::<EngineH>(&a[0], "engine")?;
        let r = handle::<ResultsH>(&a[1], "results")?;
        let c = e.0.commit_with(std::mem::take(&mut *r.done.lock().unwrap()));
        let changed: Value = Value::Vector(
            c.changed.iter().filter(|x| x.version >= 0).map(|x| map(vec![("uri", sv(&x.uri)), ("version", Value::Int(x.version))])).collect(),
        );
        let disk: Value = Value::Vector(c.changed.iter().filter(|x| x.disk).map(|x| sv(&x.uri)).collect());
        Ok(map(vec![
            ("snap", wrap(SnapH(c.snapshot), snap_shape())),
            ("changed", changed),
            ("disk", disk),
            ("dropped", Value::Int(c.dropped as i64)),
        ]))
    });
    // (mova.nx/watch-changed eng uri) -> [uri]: watched file created/changed on disk, queued for analysis (open docs skipped).
    def(i, "watch-changed", |_i, a| {
        arity(a, 2, "watch-changed")?;
        let e = handle::<EngineH>(&a[0], "engine")?;
        Ok(Value::Vector(e.0.watch_changed(want_str(a, 1, "watch-changed")?).iter().map(|u| sv(u)).collect()))
    });
    // (mova.nx/watch-deleted eng uri) -> {:deleted [uri] :refs [uri]}: watched file deleted on disk, removed from the store.
    def(i, "watch-deleted", |_i, a| {
        arity(a, 2, "watch-deleted")?;
        let e = handle::<EngineH>(&a[0], "engine")?;
        let (d, r) = e.0.watch_deleted(want_str(a, 1, "watch-deleted")?);
        Ok(map(vec![
            ("deleted", Value::Vector(d.iter().map(|u| sv(u)).collect())),
            ("refs", Value::Vector(r.iter().map(|u| sv(u)).collect())),
        ]))
    });
    // (mova.nx/reference-uris snap uri) -> [uri]: clojure-lsp reference-uris (dependents + dependencies).
    def(i, "reference-uris", |_i, a| {
        arity(a, 2, "reference-uris")?;
        let s = handle::<SnapH>(&a[0], "snapshot")?;
        Ok(Value::Vector(s.0.reference_uris(want_str(a, 1, "reference-uris")?).iter().map(|u| sv(u)).collect()))
    });
    // (mova.nx/snapshot eng) -> Snapshot handle (lock-free read).
    def(i, "snapshot", |_i, a| {
        arity(a, 1, "snapshot")?;
        Ok(wrap(SnapH(handle::<EngineH>(&a[0], "engine")?.0.store.snapshot()), snap_shape()))
    });
    // (mova.nx/client-options eng jar-scheme? hover-markdown? arity-on-same-line? hide-file? hide-call?) -> nil.
    def(i, "client-options", |_i, a| {
        arity(a, 6, "client-options")?;
        let e = handle::<EngineH>(&a[0], "engine")?;
        let b = |k: usize| !matches!(a[k], Value::Nil | Value::Bool(false));
        e.0.set_client_opts(nx_core::engine::ClientOpts {
            jar_scheme: b(1),
            hover_markdown: b(2),
            arity_on_same_line: b(3),
            hide_file_location: b(4),
            hide_signature_call: b(5),
            ..(*e.0.store.snapshot().opts).clone()
        });
        Ok(Value::Nil)
    });
    // (mova.nx/clojuredocs eng root-uri hover-clojuredocs-init-option) -> nil. Starts the background clojuredocs load (never blocks).
    def(i, "clojuredocs", |_i, a| {
        arity(a, 3, "clojuredocs")?;
        let _ = handle::<EngineH>(&a[0], "engine")?;
        let root = match &a[1] {
            Value::Nil => None,
            _ => nx_core::engine::scan::uri_to_path(want_str(a, 1, "clojuredocs")?),
        };
        let opt = match a[2] {
            Value::Bool(b) => Some(b),
            _ => None,
        };
        nx_core::clojuredocs::start(root.as_deref(), opt);
        Ok(Value::Nil)
    });
    // (mova.nx/completion-options eng resolve-documentation? resolve-alias-edit? markdown? snippets? use-metadata-privacy? additional-snippets) -> nil.
    // additional-snippets: vector of [name detail snippet] vectors (detail may be nil). Merged into the client options.
    def(i, "completion-options", |_i, a| {
        arity(a, 7, "completion-options")?;
        let e = handle::<EngineH>(&a[0], "engine")?;
        let b = |k: usize| !matches!(a[k], Value::Nil | Value::Bool(false));
        let mut o = (*e.0.store.snapshot().opts).clone();
        o.resolve_documentation = b(1);
        o.resolve_alias_edit = b(2);
        o.completion_markdown = b(3);
        o.completion_snippets = b(4);
        o.use_metadata_privacy = b(5);
        let st = |v: &Value| if let Value::Str(t) = v { Some(t.as_ref().to_string()) } else { None };
        o.additional_snippets = match &a[6] {
            Value::Vector(v) => v
                .iter()
                .filter_map(|x| if let Value::Vector(t) = x { Some((st(t.get(0)?)?, t.get(1).and_then(st), st(t.get(2)?)?)) } else { None })
                .collect(),
            _ => Vec::new(),
        };
        e.0.set_client_opts(o);
        Ok(Value::Nil)
    });
    // (mova.nx/resolve-completion snap id item-json) -> full JSON-RPC response frame of `completionItem/resolve`.
    def(i, "resolve-completion", |_i, a| {
        arity(a, 3, "resolve-completion")?;
        let s = handle::<SnapH>(&a[0], "snapshot")?;
        let id = match &a[1] {
            Value::Int(n) => n.to_string(),
            Value::Str(t) => json_quote(t.as_ref()),
            _ => return Err(RjError::type_err("mova.nx/resolve-completion: id must be int or string".to_string())),
        };
        let res = nx_core::query::resolve_completion(&s.0, want_str(a, 2, "resolve-completion")?);
        Ok(Value::Str(Str::from(format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{res}}}"))))
    });
    // (mova.nx/open-external eng uri) -> true when a jar entry was promoted to a fully analyzed file.
    def(i, "open-external", |_i, a| {
        arity(a, 2, "open-external")?;
        let e = handle::<EngineH>(&a[0], "engine")?;
        Ok(Value::Bool(e.0.open_external(want_str(a, 1, "open-external")?)))
    });
    // (mova.nx/client-caps eng doc-changes? resource-ops? annotations?) -> nil (workspace.workspaceEdit capabilities).
    def(i, "client-caps", |_i, a| {
        arity(a, 4, "client-caps")?;
        let e = handle::<EngineH>(&a[0], "engine")?;
        let b = |k: usize| !matches!(a[k], Value::Nil | Value::Bool(false));
        let mut o = (*e.0.store.snapshot().opts).clone();
        o.we_doc_changes = b(1);
        o.we_resource_ops = b(2);
        o.we_annotations = b(3);
        e.0.set_client_opts(o);
        Ok(Value::Nil)
    });
    // (mova.nx/query snap id method uri line character include-declaration? [extra]) -> full JSON-RPC response frame text,
    // or nil when the method is not answered natively. Pure read of the snapshot; no allocation per request beyond the reply.
    def(i, "query", |_i, a| {
        if a.len() != 7 && a.len() != 8 {
            return Err(RjError::type_err("mova.nx/query: expects 7 or 8 arguments".to_string()));
        }
        let s = handle::<SnapH>(&a[0], "snapshot")?;
        let extra = match a.get(7) {
            Some(Value::Str(t)) => t.as_ref().to_string(),
            _ => String::new(),
        };
        let id = match &a[1] {
            Value::Int(n) => n.to_string(),
            Value::Str(t) => json_quote(t.as_ref()),
            _ => return Err(RjError::type_err("mova.nx/query: id must be int or string".to_string())),
        };
        let num = |v: &Value| match v {
            Value::Int(x) if *x >= 0 => *x as u32,
            _ => 0,
        };
        let inc = !matches!(a[6], Value::Nil | Value::Bool(false));
        let at = nx_core::query::At { uri: want_str(a, 3, "query")?, line: num(&a[4]), ch: num(&a[5]) };
        match nx_core::query::answer_x(&s.0, want_str(a, 2, "query")?, at, inc, &extra) {
            Some(res) if res.starts_with('\u{1}') => {
                let (code, msg) = res[1..].split_once('\u{1}').unwrap_or(("-32603", "Internal error"));
                Ok(Value::Str(Str::from(format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"error\":{{\"code\":{code},\"message\":{}}}}}", json_quote(msg)))))
            }
            Some(res) => Ok(Value::Str(Str::from(format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{res}}}")))),
            None => Ok(Value::Nil),
        }
    });
    // (mova.nx/query-n snap id method uri [ints]) -> like `query` for requests carrying extra integers.
    def(i, "query-n", |_i, a| {
        arity(a, 5, "query-n")?;
        let s = handle::<SnapH>(&a[0], "snapshot")?;
        let id = match &a[1] {
            Value::Int(n) => n.to_string(),
            Value::Str(t) => json_quote(t.as_ref()),
            _ => return Err(RjError::type_err("mova.nx/query-n: id must be int or string".to_string())),
        };
        let nums: Vec<i64> = match &a[4] {
            Value::Vector(v) => v.iter().map(|x| if let Value::Int(n) = x { *n } else { 0 }).collect(),
            _ => Vec::new(),
        };
        match nx_core::query::answer_n(&s.0, want_str(a, 2, "query-n")?, want_str(a, 3, "query-n")?, &nums) {
            Some(res) => Ok(Value::Str(Str::from(format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{res}}}")))),
            None => Ok(Value::Nil),
        }
    });
    // (mova.nx/query-j snap id method json-string) -> like `query` for requests carrying a JSON parameter string.
    def(i, "query-j", |_i, a| {
        arity(a, 4, "query-j")?;
        let s = handle::<SnapH>(&a[0], "snapshot")?;
        let id = match &a[1] {
            Value::Int(n) => n.to_string(),
            Value::Str(t) => json_quote(t.as_ref()),
            _ => return Err(RjError::type_err("mova.nx/query-j: id must be int or string".to_string())),
        };
        match nx_core::query::answer_j(&s.0, want_str(a, 2, "query-j")?, want_str(a, 3, "query-j")?) {
            Some(res) => Ok(Value::Str(Str::from(format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{res}}}")))),
            None => Ok(Value::Nil),
        }
    });
    // (mova.nx/entry-version snap uri) -> version of the stored entry (-1 = disk) or nil.
    def(i, "entry-version", |_i, a| {
        arity(a, 2, "entry-version")?;
        let s = handle::<SnapH>(&a[0], "snapshot")?;
        Ok(s.0.get(want_str(a, 1, "entry-version")?).map_or(Value::Nil, |e| Value::Int(e.version)))
    });
    // (mova.nx/project-diagnostics snap) -> [{:uri :diagnostics [...]}] for internal files with findings (startup lint).
    def(i, "project-diagnostics", |_i, a| {
        arity(a, 1, "project-diagnostics")?;
        let s = handle::<SnapH>(&a[0], "snapshot")?;
        let out: Vec<Value> = nx_core::query::diag::project_diagnostics(&s.0)
            .iter()
            .map(|(u, ds)| map(vec![("uri", sv(u)), ("diagnostics", Value::Vector(ds.iter().map(diag_value).collect()))]))
            .collect();
        Ok(Value::Vector(out.into_iter().collect()))
    });
    // (mova.nx/diagnostics snap uri) -> vector of LSP diagnostic maps, JVM clojure-lsp shape.
    def(i, "diagnostics", |_i, a| {
        arity(a, 2, "diagnostics")?;
        let s = handle::<SnapH>(&a[0], "snapshot")?;
        let uri = want_str(a, 1, "diagnostics")?;
        let out: Vec<Value> = nx_core::query::diag::diagnostics(&s.0, uri).iter().map(diag_value).collect();
        Ok(Value::Vector(out.into_iter().collect()))
    });
}

/// Byte offset of (line, UTF-16 column) in `t`. Lines end at \n, \r\n or \r (LSP).
/// Past-the-end positions clamp to the end of line / text.
fn pos_to_offset(t: &str, line: u64, ch: u64) -> usize {
    let b = t.as_bytes();
    let (mut off, mut l) = (0usize, 0u64);
    while l < line {
        match b[off..].iter().position(|&c| c == b'\n' || c == b'\r') {
            None => return t.len(),
            Some(i) => {
                let at = off + i;
                off = at + 1;
                if b[at] == b'\r' && b.get(off) == Some(&b'\n') {
                    off += 1;
                }
                l += 1;
            }
        }
    }
    let mut units = 0u64;
    for (i, c) in t[off..].char_indices() {
        if c == '\n' || c == '\r' || units >= ch {
            return off + i;
        }
        units += c.len_utf16() as u64;
    }
    t.len()
}

/// Replace range [(sl,sc),(el,ec)) (0-based line, UTF-16 col) of `t` with `new`.
pub fn apply_edit(t: &str, sl: u64, sc: u64, el: u64, ec: u64, new: &str) -> String {
    let s = pos_to_offset(t, sl, sc);
    let e = pos_to_offset(t, el, ec).max(s);
    let mut out = String::with_capacity(t.len() - (e - s) + new.len());
    out.push_str(&t[..s]);
    out.push_str(new);
    out.push_str(&t[e..]);
    out
}

#[cfg(test)]
mod tests {
    use super::apply_edit;
    #[test]
    fn edits() {
        assert_eq!(apply_edit("ab\ncd", 0, 1, 1, 1, "X"), "aXd");
        assert_eq!(apply_edit("a\r\nb", 1, 0, 1, 1, "Z"), "a\r\nZ");
        // U+1F600 is 2 UTF-16 units: col 2 is right after it
        assert_eq!(apply_edit("\u{1F600}b", 0, 2, 0, 3, "c"), "\u{1F600}c");
        assert_eq!(apply_edit("ab", 5, 9, 5, 9, "!"), "ab!");
        assert_eq!(apply_edit("ab\ncd", 0, 9, 0, 9, "!"), "ab!\ncd");
    }
}
