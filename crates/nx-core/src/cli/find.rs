//! `nx find <text>`: var definitions whose name contains the text (project first, then dependencies).
use super::*;
use std::collections::BTreeSet;

pub fn run(o: &Opts) -> Result<Reply, String> {
    let [text] = o.args.as_slice() else { return Err("usage: nx find <text>".to_string()) };
    let p = Project::load(find_root(o.root.as_deref())?);
    let s = p.snap();
    let q = Q::new(&s);
    let want = text.to_lowercase();
    let hit = |n: &str| n.to_lowercase().contains(&want);
    let (mut project, mut deps): (BTreeSet<(&str, &str)>, BTreeSet<(&str, &str)>) = (BTreeSet::new(), BTreeSet::new());
    for (k, fs) in s.defs.iter() {
        let (ns, name) = (SymId(k.0), SymId(k.1));
        if !name.is_none() && hit(name.as_str()) {
            if fs.iter().any(|f| q.internal(*f)) { &mut project } else { &mut deps }.insert((ns.as_str(), name.as_str()));
        }
    }
    if let Some(j) = &s.jars {
        j.layer.jars.iter().for_each(|jar| jar.for_each_def(|r| if !r.name.is_none() && hit(r.name.as_str()) { deps.insert((r.ns.as_str(), r.name.as_str())); }));
    }
    let resolve = |(ns, name): &(&str, &str)| {
        let e = q.last_var_def(intern(ns), intern(name), crate::query::CLJ | crate::query::CLJS, false)?;
        let (line, at) = def::head(&p, &q, e);
        Some((line, at, format!("{ns}/{name}")))
    };
    // project hits first, test files last; dependencies are resolved only until the cap is reached
    let mut rows: Vec<(String, String, String)> = project.iter().filter_map(resolve).collect();
    rows.sort_by_key(|r| {
        let (file, row) = r.1.rsplit_once(':').unwrap_or((&r.1, "0"));
        (is_test(file), file.to_string(), row.parse::<u32>().unwrap_or(0))
    });
    let mut used = project.len();
    for d in deps.iter().filter(|d| !project.contains(*d)) {
        if !o.all && rows.len() >= LIST_CAP {
            break;
        }
        used += 1;
        rows.extend(resolve(d));
    }
    let mut more = (project.len() + deps.iter().filter(|d| !project.contains(*d)).count()) - used;
    if !o.all && rows.len() > LIST_CAP {
        more += rows.len() - LIST_CAP;
        rows.truncate(LIST_CAP);
    }
    if rows.is_empty() {
        return Ok(Reply::err(String::new(), 1));
    }
    if o.json {
        let list = rows.iter().map(|r| jobj(vec![("location", jstr(&r.1)), ("symbol", jstr(&r.2)), ("line", jstr(&r.0))])).collect();
        return Ok(Reply::out(json_text(&jobj(vec![("matches", Json::Arr(list)), ("more", Json::Num(more as f64))])) + "\n", 0));
    }
    let mut lines: Vec<String> = rows.into_iter().map(|r| r.0).collect();
    if more > 0 {
        lines.push(format!("... +{more} more (--all)"));
    }
    Ok(Reply::out(lines.join("\n") + "\n", 0))
}
