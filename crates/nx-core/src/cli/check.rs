//! `nx check [file...] [--new] [--errors] [--info] [--all]`: diagnostics, plus errors the edit caused in other files.
use super::*;
use crate::engine::lsp::Diagnostic;
use crate::engine::types::Lang;
use crate::query::callh::file_text;
use crate::query::diag::diagnostics;
use std::collections::HashMap;

/// A finding with the uri of its file.
type Item = (String, Diagnostic);

pub fn run(o: &Opts) -> Result<Reply, String> {
    let p = Project::load(find_root(o.root.as_deref())?);
    let files = o.args.iter().map(|a| p.file_arg(a)).collect::<Result<Vec<_>, _>>()?;
    check(&p, &files, o)
}

/// A finding that always means broken code: an error, or an unresolved name (clj-kondo only warns for these).
fn breaking(d: &Diagnostic) -> bool {
    d.severity == 1 || matches!(d.code.as_str(), "unresolved-symbol" | "unresolved-var" | "unresolved-namespace")
}

/// Findings of `uri`: errors and warnings (`--info`: all); a file that does not parse shows only its syntax errors.
fn items(s: &Snapshot, uri: &str, info: bool) -> Vec<Item> {
    let mut d = diagnostics(s, uri);
    if d.iter().any(|x| x.code == "syntax") {
        d.retain(|x| x.code == "syntax");
    }
    d.retain(|x| info || x.severity <= 2);
    d.sort_by_key(|x| (x.code != "syntax", x.line, x.character));
    d.into_iter().map(|x| (uri.to_string(), x)).collect()
}

/// Text of `path` at git HEAD (None: no git, untracked, or no commit).
fn head_text(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let out = std::process::Command::new("git").current_dir(root).arg("show").arg(format!("HEAD:./{}", rel.display())).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Drop the findings of `cur` that `old` also has, matched by (file, code, message) as a multiset.
fn without(cur: Vec<Item>, old: &[Item]) -> Vec<Item> {
    let key = |i: &Item| (i.0.clone(), i.1.code.clone(), i.1.message.clone());
    let mut count: HashMap<_, usize> = HashMap::new();
    for i in old {
        *count.entry(key(i)).or_default() += 1;
    }
    cur.into_iter().filter(|i| count.get_mut(&key(i)).map_or(true, |n| if *n > 0 { *n -= 1; false } else { true })).collect()
}

/// Re-read `uris` from disk: a commit re-finishes the dependents of a changed definition but keeps their old findings.
fn reload(p: &Project, uris: &[String]) {
    for u in uris {
        p.e.analyze_disk_override(u);
        p.settle_one();
    }
}

/// Findings of the HEAD version of `uri` (analysed in place of the file, then the file is restored) and of the `refs` files with it.
fn head_items(p: &Project, uri: &str, path: &Path, refs: &[String], info: bool) -> Option<(Vec<Item>, Vec<Item>)> {
    let text = head_text(&p.root, path)?;
    p.e.analyze_text(uri, 1, text);
    p.settle_one();
    reload(p, refs);
    let s = p.snap();
    let own = items(&s, uri, info);
    let elsewhere = refs.iter().flat_map(|r| items(&s, r, info)).filter(|i| breaking(&i.1)).collect();
    p.e.analyze_disk_override(uri);
    p.settle_one();
    reload(p, refs);
    Some((own, elsewhere))
}

/// Check `files` (empty: the whole project; with `--new`: the files git reports as changed). Reply: findings, two lines each; exit 1 when there are any.
pub fn check(p: &Project, files: &[PathBuf], o: &Opts) -> Result<Reply, String> {
    let changed;
    let files = if o.new && files.is_empty() {
        changed = changed_files(&p.root).unwrap_or_default();
        if changed.is_empty() {
            return Ok(Reply::out(String::new(), 0));
        }
        &changed[..]
    } else {
        files
    };
    let s = p.snap();
    let uris: Vec<String> = if files.is_empty() {
        let mut v: Vec<String> = s.uris().filter(|u| s.get(u).is_some_and(|e| e.internal && e.lang != Lang::Edn)).map(|u| u.to_string()).collect();
        v.sort();
        v
    } else {
        files.iter().map(|f| scan::path_to_uri(f)).collect()
    };
    if let Some(u) = uris.iter().find(|u| s.get(u).is_none()) {
        return Err(format!("not a project file: {}", p.show(u)));
    }
    let mut own: Vec<Item> = uris.iter().flat_map(|u| items(&s, u, o.info)).collect();
    if o.errors {
        own.retain(|i| breaking(&i.1));
    }
    let mut refs: Vec<String> = Vec::new();
    if !files.is_empty() {
        for r in uris.iter().flat_map(|u| s.reference_uris(u)) {
            if !uris.contains(&r) && !refs.contains(&r) {
                refs.push(r);
            }
        }
    }
    let mut elsewhere: Vec<Item> = refs.iter().flat_map(|r| items(&s, r, o.info)).filter(|i| breaking(&i.1)).collect();
    if o.new {
        let (mut old_own, mut old_else): (Vec<Item>, Vec<Item>) = (Vec::new(), Vec::new());
        for (u, f) in uris.iter().zip(files) {
            if let Some((a, b)) = head_items(p, u, f, &refs, o.info) {
                old_own.extend(a);
                old_else.extend(b);
            }
        }
        own = without(own, &old_own);
        elsewhere = without(elsewhere, &old_else);
    }
    if own.is_empty() && elsewhere.is_empty() {
        return Ok(Reply::out(String::new(), 0));
    }
    let s = p.snap();
    let q = Q::new(&s);
    let mut texts: HashMap<String, String> = HashMap::new();
    let mut source = |i: &Item| -> String {
        let t = texts.entry(i.0.clone()).or_insert_with(|| s.id(&i.0).and_then(|f| file_text(&q, f)).unwrap_or_default());
        t.lines().nth(i.1.line as usize).unwrap_or("").trim().chars().take(120).collect()
    };
    let sev = |d: &Diagnostic| ["", "error", "warning", "info"][d.severity as usize % 4];
    if o.json {
        let mut obj = |v: &[Item]| Json::Arr(v.iter().map(|i| jobj(vec![("file", jstr(&p.show(&i.0))), ("line", Json::Num(i.1.line as f64 + 1.0)), ("col", Json::Num(i.1.character as f64 + 1.0)), ("severity", jstr(sev(&i.1))), ("code", jstr(&i.1.code)), ("message", jstr(&i.1.message)), ("source", jstr(&source(i)))])).collect());
        let j = jobj(vec![("findings", obj(&own)), ("elsewhere", obj(&elsewhere))]);
        return Ok(Reply::out(json_text(&j) + "\n", 1));
    }
    let mut line = |i: &Item| {
        let src = source(i);
        let head = format!("{}:{}:{} {} {} {}", p.show(&i.0), i.1.line + 1, i.1.character + 1, sev(&i.1), i.1.code, i.1.message.replace('\n', " "));
        if src.is_empty() { head } else { format!("{head}\n    {src}") }
    };
    // errors first and never capped, then warnings and info (in file/line order) under the cap
    own.sort_by_key(|i| (!breaking(&i.1), i.1.severity));
    let n_err = own.iter().take_while(|i| breaking(&i.1)).count();
    let mut lines: Vec<String> = own[..n_err].iter().map(&mut line).collect();
    if !elsewhere.is_empty() {
        lines.push("broken elsewhere:".to_string());
        lines.extend(elsewhere.iter().map(&mut line));
    }
    let rest: Vec<String> = own[n_err..].iter().map(&mut line).collect();
    let rest = capped(&rest, o.all, LIST_CAP);
    if !rest.is_empty() {
        lines.push(rest);
    }
    Ok(Reply::out(lines.join("\n") + "\n", 1))
}
