//! `nx refs <sym> [--in file]`: uses of a var, grouped by file, each with its enclosing function and source line.
use super::*;
use crate::query::callh::file_text;
use std::collections::BTreeMap;

pub fn run(o: &Opts) -> Result<Reply, String> {
    let [sym] = o.args.as_slice() else { return Err("usage: nx refs <sym> [--in file]".to_string()) };
    let p = Project::load(find_root(o.root.as_deref())?);
    let in_file = o.in_file.as_deref().map(|f| p.file_arg(f)).transpose()?;
    let s = p.snap();
    let q = Q::new(&s);
    let e = match resolve_one(&p, &q, resolve(&s, sym, in_file.as_deref()), sym, o.all) {
        Ok(e) => e,
        Err(r) => return Ok(r),
    };
    let d = &q.fa(e.f).var_definitions[e.i as usize];
    let name = format!("{}/{}", d.ns.as_str(), d.name.as_str());
    // (file rel path, [(row, enclosing fn, source line)]), project files first, test files last
    let mut by_file: BTreeMap<(bool, String), Vec<(u32, String, String)>> = BTreeMap::new();
    let mut texts: Vec<(u32, String)> = Vec::new();
    for u in q.find_references(e, false, None) {
        let row = q.name_pos(u).row;
        let fa = q.fa(u.f);
        let encl = fa.var_definitions.iter().filter(|d| !d.name.is_none() && d.pos.row <= row && row <= d.pos.end_row).min_by_key(|d| d.pos.end_row - d.pos.row).map_or("(top)", |d| d.name.as_str());
        if !texts.iter().any(|t| t.0 == u.f) {
            texts.push((u.f, file_text(&q, u.f).unwrap_or_default()));
        }
        let text = texts.iter().find(|t| t.0 == u.f).map_or("", |t| t.1.as_str());
        let src = clip(text.lines().nth((row as usize).saturating_sub(1)).unwrap_or("").trim(), 120);
        let rel = p.show(q.uri(u.f));
        by_file.entry((is_test(&rel), rel)).or_default().push((row, encl.to_string(), src));
    }
    by_file.values_mut().for_each(|v| v.sort());
    let uses: usize = by_file.values().map(Vec::len).sum();
    if o.json {
        let files = by_file.iter().map(|((_, f), v)| jobj(vec![("file", jstr(f)), ("uses", Json::Arr(v.iter().map(|(r, c, t)| jobj(vec![("line", Json::Num(*r as f64)), ("in", jstr(c)), ("source", jstr(t))])).collect()))])).collect();
        return Ok(Reply::out(json_text(&jobj(vec![("symbol", jstr(&name)), ("count", Json::Num(uses as f64)), ("files", Json::Arr(files))])) + "\n", 0));
    }
    let plural = |n: usize, w: &str| format!("{n} {w}{}", if n == 1 { "" } else { "s" });
    let mut lines = Vec::new();
    for ((_, f), v) in &by_file {
        lines.push(f.clone());
        let (rw, cw) = (v.iter().map(|x| x.0.to_string().len()).max().unwrap_or(0), v.iter().map(|x| x.1.chars().count()).max().unwrap_or(0));
        lines.extend(v.iter().map(|(r, c, t)| format!("  {r:>rw$} {c:<cw$}  {t}")));
    }
    let head = format!("{name}: {} in {}", plural(uses, "use"), plural(by_file.len(), "file"));
    let body = if lines.is_empty() { String::new() } else { capped(&lines, o.all, LIST_CAP) + "\n" };
    Ok(Reply::out(format!("{head}\n{body}"), 0))
}
