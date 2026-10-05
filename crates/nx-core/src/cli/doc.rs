//! `nx doc <sym> [--in file]`: where a name comes from (project, Mova native, Mova stdlib, Clojure jar), arglists and doc.
use super::*;
use crate::engine::index::B;
use crate::mova::{RANK_NATIVE, RANK_STDLIB};
use crate::query::special_forms::SPECIAL_FORMS;

pub fn run(o: &Opts) -> Result<Reply, String> {
    let [sym] = o.args.as_slice() else { return Err("usage: nx doc <sym> [--in file]".to_string()) };
    let p = Project::load(find_root(o.root.as_deref())?);
    let in_file = o.in_file.as_deref().map(|f| p.file_arg(f)).transpose()?;
    // no project at all: a bare name is a Mova core var (when a `mova` is around), as in a Mova project
    if !p.snap().project.as_ref().map_or(false, |i| i.mova) && nearest_root(&p.root).is_none() {
        p.e.enable_mova(None);
    }
    let s = p.snap();
    let q = Q::new(&s);
    let found = resolve(&s, sym, in_file.as_deref());
    if let (Found::Missing, Some(f)) = (&found, SPECIAL_FORMS.iter().find(|f| f.0 == sym)) {
        return Ok(special(f, o));
    }
    match resolve_one(&p, &q, found, sym, o.all) {
        Ok(e) => Ok(Reply::out(describe(&p, &q, e, o), 0)),
        Err(r) => Ok(r),
    }
}

/// A special form: its signatures and doc.
fn special(f: &(&str, &[&str], &str, &str), o: &Opts) -> Reply {
    let doc: Vec<String> = f.2.lines().map(|l| l.trim().to_string()).collect();
    if o.json {
        let j = jobj(vec![("name", jstr(f.0)), ("origin", jstr("special form")), ("arglists", Json::Arr(f.1.iter().map(|a| jstr(a)).collect())), ("doc", jstr(&doc.join("\n")))]);
        return Reply::out(json_text(&j) + "\n", 0);
    }
    let mut out = vec![format!("{}  special form", f.0)];
    out.extend(f.1.iter().map(|a| format!("  {a}")));
    out.extend(def::cut(&doc, def::DOC_CAP, o.all, "  "));
    Reply::out(out.join("\n") + "\n", 0)
}

/// File name of a jar entry uri (`jar:file:///a/b/x.jar!/p/q.clj` -> `x.jar`).
fn jar_name(uri: &str) -> &str {
    let jar = uri.split("!/").next().unwrap_or(uri);
    jar.rsplit('/').next().unwrap_or(jar)
}

/// A `clojure.core` var that Mova has too.
fn is_core(q: &Q, e: El) -> bool {
    let d = &q.fa(e.f).var_definitions[e.i as usize];
    d.ns.as_str() == "clojure.core" && crate::mova::is_core_var(d.name.as_str())
}

/// The origin line text of a definition.
fn origin(p: &Project, q: &Q, e: El) -> String {
    let rank = q.s.mova.as_ref().and_then(|m| m.rank.get(&e.f).copied());
    let (at, uri) = (loc(p, q, e), q.uri(e.f));
    match rank {
        _ if uri == crate::mova::BUILTIN_URI => "Mova core var (Rust native; no Mova index here: put `mova` on PATH or set MOVA_BIN for its source)".to_string(),
        Some(RANK_NATIVE) => format!("Mova native (Rust) {at}"),
        Some(RANK_STDLIB) => format!("Mova stdlib {at}"),
        _ if q.internal(e.f) => format!("project {at}"),
        _ if uri.starts_with("jar:") && q.s.mova.is_some() && is_core(q, e) => format!("Mova core var, no Mova index here (put `mova` on PATH or set MOVA_BIN); the doc below is Clojure's ({})", jar_name(uri)),
        _ if uri.starts_with("jar:") && q.s.mova.is_some() => format!("Clojure only ({}): not in the Mova index", jar_name(uri)),
        _ if uri.starts_with("jar:") => jar_name(uri).to_string(),
        _ => at,
    }
}

/// The arglists and doc of the same `ns/name` in a classpath jar (Clojure's own), under `  Clojure doc:`; empty when no jar defines it.
fn clojure_doc(q: &Q, ns: SymId, name: SymId, o: &Opts) -> Vec<String> {
    let Some(jars) = &q.s.jars else { return Vec::new() };
    let Some(h) = jars.locate_all(ns, name).into_iter().find_map(|f| q.var_def_idx(f, ns, name).first().map(|i| q.hdef(El { f, b: B::VarDef, i: *i }))) else { return Vec::new() };
    let doc: Vec<String> = h.doc.map(|d| d.lines().map(|l| l.trim().to_string()).collect()).unwrap_or_default();
    let mut out = vec!["  Clojure doc:".to_string()];
    if !h.arglists.is_empty() {
        out.push(format!("  {}", h.arglists.join(" ")));
    }
    out.extend(def::cut(&doc, def::DOC_CAP, o.all, "  "));
    out
}

fn describe(p: &Project, q: &Q, e: El, o: &Opts) -> String {
    let h = q.hdef(e);
    let d = &q.fa(e.f).var_definitions[e.i as usize];
    let name = format!("{}/{}", d.ns.as_str(), d.name.as_str());
    let org = origin(p, q, e);
    let doc: Vec<String> = h.doc.map(|d| d.lines().map(|l| l.trim().to_string()).collect()).unwrap_or_default();
    let reg: Vec<String> = h.native.as_ref().map(|n| n.1.lines().map(str::to_string).collect()).unwrap_or_default();
    if o.json {
        let j = jobj(vec![("name", jstr(&name)), ("origin", jstr(&org)), ("location", jstr(&loc(p, q, e))), ("arglists", Json::Arr(h.arglists.iter().map(|a| jstr(a)).collect())), ("doc", jstr(h.doc.unwrap_or(""))), ("source", jstr(&reg.join("\n")))]);
        return json_text(&j) + "\n";
    }
    let mut out = vec![format!("{name}  {org}")];
    if !h.arglists.is_empty() {
        out.push(format!("  {}", h.arglists.join(" ")));
    }
    out.extend(def::cut(&doc, def::DOC_CAP, o.all, "  "));
    out.extend(reg.iter().map(|l| format!("  {l}")));
    if h.native.is_some() && h.arglists.is_empty() && h.doc.is_none() {
        out.extend(clojure_doc(q, d.ns, d.name, o));
    }
    out.join("\n") + "\n"
}
