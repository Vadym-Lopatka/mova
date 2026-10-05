//! `nx ns [prefix]`: namespaces of the project: file, public var count, dependents.
use super::*;
use std::collections::BTreeSet;

const DEP_CAP: usize = 5;

pub fn run(o: &Opts) -> Result<Reply, String> {
    let prefix = match o.args.as_slice() {
        [] => "",
        [p] => p.as_str(),
        _ => return Err("usage: nx ns [prefix]".to_string()),
    };
    let p = Project::load(find_root(o.root.as_deref())?);
    let s = p.snap();
    let q = Q::new(&s);
    // (ns, rel path, public vars, dependents)
    let mut rows: Vec<(String, String, usize, Vec<String>)> = Vec::new();
    for (k, fs) in s.ns_files.iter() {
        let ns = SymId(*k);
        if !ns.as_str().starts_with(prefix) {
            continue;
        }
        let deps: BTreeSet<&str> = s.ns_deps.get(k).into_iter().flatten().filter(|(from, f)| q.internal(*f) && *from != *k && !SymId(*from).is_none()).map(|(from, _)| SymId(*from).as_str()).collect();
        for f in fs.iter().filter(|f| q.internal(**f)) {
            let public = outline::top_defs(q.fa(*f)).iter().filter(|d| !d.1.private).count();
            rows.push((ns.as_str().to_string(), p.show(q.uri(*f)), public, deps.iter().map(|d| d.to_string()).collect()));
        }
    }
    rows.sort_by(|a, b| (is_test(&a.1), &a.0, &a.1).cmp(&(is_test(&b.1), &b.0, &b.1)));
    if rows.is_empty() {
        return Ok(Reply::err(String::new(), 1));
    }
    if o.json {
        let list = rows.iter().map(|r| jobj(vec![("ns", jstr(&r.0)), ("file", jstr(&r.1)), ("public", Json::Num(r.2 as f64)), ("dependents", Json::Arr(r.3.iter().map(|d| jstr(d)).collect()))])).collect();
        return Ok(Reply::out(json_text(&jobj(vec![("namespaces", Json::Arr(list))])) + "\n", 0));
    }
    let (nw, fw) = (rows.iter().map(|r| r.0.chars().count()).max().unwrap_or(0), rows.iter().map(|r| r.1.chars().count()).max().unwrap_or(0));
    let lines: Vec<String> = rows
        .iter()
        .map(|(ns, f, n, deps)| {
            let mut shown: Vec<String> = deps.iter().take(if o.all { usize::MAX } else { DEP_CAP }).cloned().collect();
            if shown.len() < deps.len() {
                shown.push(format!("+{}", deps.len() - shown.len()));
            }
            let tail = if shown.is_empty() { String::new() } else { format!("  <- {}", shown.join(", ")) };
            format!("{ns:<nw$}  {f:<fw$}  {n} public{tail}")
        })
        .collect();
    Ok(Reply::out(capped(&lines, o.all, LIST_CAP) + "\n", 0))
}
