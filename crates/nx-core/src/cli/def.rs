//! `nx def <sym> [--in file]`: where, signature, doc and source of a definition.
use super::*;
use crate::query::callh::file_text;

pub const DOC_CAP: usize = 12;
pub const SRC_CAP: usize = 30;

pub fn run(o: &Opts) -> Result<Reply, String> {
    let [sym] = o.args.as_slice() else { return Err("usage: nx def <sym> [--in file]".to_string()) };
    let p = Project::load(find_root(o.root.as_deref())?);
    let in_file = o.in_file.as_deref().map(|f| p.file_arg(f)).transpose()?;
    let s = p.snap();
    let q = Q::new(&s);
    match resolve_one(&p, &q, resolve(&s, sym, in_file.as_deref()), sym, o.all) {
        Ok(e) => Ok(Reply::out(describe(&p, &q, e, o), 0)),
        Err(r) => Ok(r),
    }
}

/// `path:row ns/name (kind)` and its `path:row` part.
pub fn head(p: &Project, q: &Q, e: El) -> (String, String) {
    let d = &q.fa(e.f).var_definitions[e.i as usize];
    let h = q.hdef(e);
    let loc = loc(p, q, e);
    let kind = if h.native.is_some() { "native fn" } else if d.macro_ { "macro" } else if d.has_arglists { "fn" } else { "var" };
    let kind = if d.private { format!("private {kind}") } else { kind.to_string() };
    (format!("{loc} {}/{} ({kind})", d.ns.as_str(), d.name.as_str()), loc)
}

/// The top-level form of the definition (a native: its registration lines); external files only with `--all`.
pub fn source(q: &Q, e: El, all: bool) -> Vec<String> {
    if let Some((_, src)) = q.hdef(e).native {
        return src.lines().map(str::to_string).collect();
    }
    if !all && !q.internal(e.f) {
        return Vec::new();
    }
    let d = &q.fa(e.f).var_definitions[e.i as usize];
    let text = file_text(q, e.f).unwrap_or_default();
    text.lines().skip((d.pos.row as usize).saturating_sub(1)).take((d.pos.end_row.saturating_sub(d.pos.row)) as usize + 1).map(str::to_string).collect()
}

/// `lines` cut at `cap` (`--all`: not cut), with a `... +N lines (--all)` last line.
pub fn cut(lines: &[String], cap: usize, all: bool, indent: &str) -> Vec<String> {
    let n = if all { lines.len() } else { lines.len().min(cap) };
    let mut out: Vec<String> = lines[..n].iter().map(|l| format!("{indent}{l}").trim_end().to_string()).collect();
    if n < lines.len() {
        out.push(format!("  ... +{} lines (--all)", lines.len() - n));
    }
    out
}

fn describe(p: &Project, q: &Q, e: El, o: &Opts) -> String {
    let h = q.hdef(e);
    let (first, loc) = head(p, q, e);
    let doc: Vec<String> = h.doc.map(|d| d.lines().map(|l| l.trim().to_string()).collect()).unwrap_or_default();
    let src = source(q, e, o.all);
    if o.json {
        let d = &q.fa(e.f).var_definitions[e.i as usize];
        let j = jobj(vec![("location", jstr(&loc)), ("ns", jstr(d.ns.as_str())), ("name", jstr(d.name.as_str())), ("signature", jstr(&first)), ("arglists", Json::Arr(h.arglists.iter().map(|a| jstr(a)).collect())), ("doc", jstr(h.doc.unwrap_or(""))), ("source", jstr(&src.join("\n")))]);
        return json_text(&j) + "\n";
    }
    let mut out = vec![first];
    // the source of a definition has its arglists and doc; a native's registration lines and a jar var without source do not
    if h.native.is_some() || src.is_empty() {
        if !h.arglists.is_empty() {
            out.push(format!("  {}", h.arglists.join(" ")));
        }
        out.extend(cut(&doc, DOC_CAP, o.all, "  "));
    }
    out.extend(cut(&src, SRC_CAP, o.all, ""));
    out.join("\n") + "\n"
}
