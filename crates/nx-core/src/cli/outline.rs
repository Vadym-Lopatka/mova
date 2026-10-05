//! `nx outline <file|ns>`: the var definitions of a namespace in file order: line, kind, name, arglists, first doc line.
use super::*;
use crate::analyzer::types::{FileAnalysis, VarDef};
use crate::engine::index::B;
use crate::engine::store::FileId;

const NAME_COL: usize = 60;
const DOC_CLIP: usize = 80;

/// Top-level var definitions of a file with their index, in file order (a `declare` is not one).
pub fn top_defs(fa: &FileAnalysis) -> Vec<(u32, &VarDef)> {
    fa.var_definitions.iter().enumerate().filter(|(_, d)| !d.name.is_none() && !d.declared).map(|(i, d)| (i as u32, d)).collect()
}

/// `fn`, `macro` or `var`; `-` suffix when private.
fn kind(d: &VarDef) -> String {
    let k = if d.macro_ { "macro" } else if d.has_arglists { "fn" } else { "var" };
    format!("{k}{}", if d.private { "-" } else { "" })
}

pub fn run(o: &Opts) -> Result<Reply, String> {
    let [arg] = o.args.as_slice() else { return Err("usage: nx outline <file|ns>".to_string()) };
    let p = Project::load(find_root(o.root.as_deref())?);
    let s = p.snap();
    let q = Q::new(&s);
    let files: Vec<FileId> = match p.file_arg(arg) {
        Ok(path) => s.id(&scan::path_to_uri(&path)).into_iter().collect(),
        Err(_) => s.ns_files_of(intern(arg)),
    };
    let files: Vec<FileId> = files.into_iter().filter(|f| q.internal(*f)).collect();
    if files.is_empty() {
        return Ok(Reply::err(format!("not found: {arg}\n"), 1));
    }
    let mut out = Vec::new();
    let mut jfiles = Vec::new();
    for f in files {
        let fa = q.fa(f);
        let ns = fa.namespace_definitions.first().map_or("", |n| n.name.as_str());
        let mut reqs: Vec<&str> = fa.namespace_usages.iter().filter(|u| !u.to.is_none() && u.to.as_str() != ns).map(|u| u.to.as_str()).collect();
        reqs.sort();
        reqs.dedup();
        let rel = p.show(q.uri(f));
        let rows: Vec<(u32, String, String, String, String)> = top_defs(fa)
            .into_iter()
            .map(|(i, d)| {
                let h = q.hdef(El { f, b: B::VarDef, i });
                (d.name_pos.row, kind(d), d.name.as_str().to_string(), h.arglists.join(" "), clip(first_line(h.doc.unwrap_or("")), DOC_CLIP))
            })
            .collect();
        if o.json {
            let vars = rows.iter().map(|r| jobj(vec![("line", Json::Num(r.0 as f64)), ("kind", jstr(&r.1)), ("name", jstr(&r.2)), ("arglists", jstr(&r.3)), ("doc", jstr(&r.4))])).collect();
            jfiles.push(jobj(vec![("ns", jstr(ns)), ("file", jstr(&rel)), ("requires", Json::Arr(reqs.iter().map(|r| jstr(r)).collect())), ("vars", Json::Arr(vars))]));
            continue;
        }
        out.push(if reqs.is_empty() { format!("{ns}  {rel}") } else { format!("{ns}  {rel}  (requires {})", reqs.join(", ")) });
        let (lw, kw) = (rows.iter().map(|r| r.0.to_string().len()).max().unwrap_or(0), rows.iter().map(|r| r.1.len()).max().unwrap_or(0));
        let sigs: Vec<String> = rows.iter().map(|r| format!("{} {}", r.2, r.3).trim_end().to_string()).collect();
        let sw = sigs.iter().map(|s| s.chars().count()).max().unwrap_or(0).min(NAME_COL);
        let lines: Vec<String> = rows.iter().zip(&sigs).map(|(r, sig)| format!("  {:>lw$} {:<kw$} {sig:<sw$}  {}", r.0, r.1, r.4).trim_end().to_string()).collect();
        out.push(capped(&lines, o.all, LIST_CAP));
    }
    if o.json {
        return Ok(Reply::out(json_text(&jobj(vec![("files", Json::Arr(jfiles))])) + "\n", 0));
    }
    Ok(Reply::out(out.join("\n") + "\n", 0))
}
