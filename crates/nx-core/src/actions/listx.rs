//! Diagnostic driven and alias-swap actions of the code action list.
use super::sugg::{self, Suggestion};
use super::tree::*;
use super::{Act, Req, A};
use crate::query::{file_langs, Q};

const RESOLVABLE: [&str; 5] = ["unresolved-namespace", "unresolved-symbol", "unresolved-var", "refer-all", "syntax"];

pub struct Rd<'a> {
    pub diag: &'a super::Diag,
    pub z: Z<'a>,
}

pub fn resolvable<'a>(tree: &'a Tree<'a>, r: &'a Req) -> Vec<Rd<'a>> {
    r.diags
        .iter()
        .filter(|d| RESOLVABLE.contains(&d.code.as_str()))
        .filter_map(|d| find_at_pos(tree, d.line + 1, d.ch + 1).map(|z| Rd { diag: d, z }))
        .collect()
}

/// `safe-sym` over the read-only tree.
pub fn safe_sym(z: Z) -> Option<String> {
    if matches!(z.tag(), Tag::Whitespace | Tag::Newline | Tag::Comma | Tag::Comment | Tag::Uneval) {
        return None;
    }
    match (z.tag(), z.tk()) {
        (Tag::Token, Tk::Sym) => Some(z.text().to_string()),
        (Tag::Token, Tk::KwAuto) if z.text().starts_with("::") => Some(z.text()[2..].to_string()),
        _ => None,
    }
}

fn namespace_of(sym: &str) -> Option<&str> {
    sym.find('/').filter(|&i| i > 0 && sym != "/").map(|i| &sym[..i])
}

fn format_require(s: &Suggestion) -> String {
    format!(
        "Add require '[{}{}{}]'{}",
        s.ns,
        s.alias.as_ref().map_or(String::new(), |a| format!(" :as {a}")),
        s.refer.as_ref().map_or(String::new(), |r| format!(" :refer [{r}]")),
        s.count.map_or(String::new(), |c| format!(" × {c}"))
    )
}

fn find_class_name(z: Z) -> Option<String> {
    let sym = safe_sym(z)?;
    let value = z.text();
    let first = value.chars().next();
    let class = first.map_or(false, |c| c.is_uppercase());
    let dot_call = value.contains('.') && value.len() - 1 == value.find('.').unwrap_or(usize::MAX);
    if !class {
        return None;
    }
    if let Some(ns) = namespace_of(&sym) {
        return ns.split('.').last().map(|s| s.to_string());
    }
    if dot_call {
        return Some(value[..value.len() - 1].to_string());
    }
    Some(value.to_string())
}

fn missing_imports(q: &Q, z: Z) -> Vec<(String, usize)> {
    let Some(simple) = find_class_name(z) else { return vec![] };
    let mut classes: Vec<String> = Vec::new();
    let mut push = |c: String| {
        if !classes.contains(&c) {
            classes.push(c)
        }
    };
    // project java classes
    for u in q.s.uris() {
        if let Some(id) = q.s.id(u) {
            if id < crate::engine::jarview::EXT_BASE {
                if let Some(fa) = q.entry(id).fa() {
                    for d in &fa.java_class_defs {
                        let n = d.class.as_str();
                        if n.rsplit('.').next() == Some(simple.as_str()) {
                            push(n.to_string());
                        }
                    }
                }
            }
        }
    }
    if let Some(j) = q.s.jars.as_ref() {
        for (_, cd) in j.layer.classes() {
            if cd.class.rsplit('.').next() == Some(simple.as_str()) {
                push(cd.class.clone());
            }
        }
    }
    if let Some(jdk) = crate::jdk::wait(std::time::Duration::from_millis(50)) {
        for c in jdk.classes_simple(&simple) {
            push(jdk.class_name(c).to_string());
        }
    }
    let mut out: Vec<(String, usize)> = classes
        .into_iter()
        .map(|c| {
            let mut n = 0usize;
            for u in q.s.uris() {
                if let Some(id) = q.s.id(u) {
                    if id < crate::engine::jarview::EXT_BASE {
                        if let Some(fa) = q.entry(id).fa() {
                            n += fa.java_class_usages.iter().filter(|x| x.flags & crate::analyzer::JU_IMPORT != 0 && x.class.as_str() == c).count();
                        }
                    }
                }
            }
            (c, n)
        })
        .collect();
    out.sort_by_key(|x| x.1); // stable ascending, then reverse
    out.reverse();
    out
}

/// Actions of the diagnostics part, in `code_actions.clj/all` order up to the public-function action.
pub fn diag_actions(q: &Q, r: &Req, tree: &Tree, zo: Option<Z>, out: &mut Vec<Act>) {
    let rd = resolvable(tree, r);
    let uri = &r.uri;
    let u = || A::S(uri.clone());
    // refer-all
    for d in rd.iter().filter(|d| d.diag.code == "refer-all") {
        if d.diag.refers.is_empty() {
            continue;
        }
        let refers = format!("[{}]", d.diag.refers.join(" "));
        let t1 = format!("Replace ':refer :all' with ':refer {refers}'");
        let mut a1 = Act::new(&t1, "quickfix", "replace-refer-all-with-refer", vec![u(), A::N(d.diag.line as i64), A::N(d.diag.ch as i64), A::Arr(d.diag.refers.iter().map(|s| A::S(s.clone())).collect())]);
        a1.preferred = true;
        out.push(a1);
        out.push(Act::new("Replace ':refer :all' with alias", "quickfix", "replace-refer-all-with-alias", vec![u(), A::N(d.diag.line as i64), A::N(d.diag.ch as i64)]));
    }
    let req_diags: Vec<&Rd> = rd.iter().filter(|d| matches!(d.diag.code.as_str(), "unresolved-namespace" | "unresolved-symbol" | "syntax")).collect();
    // missing requires only filter the suggestions by comparing string namespaces with symbols in clojure-lsp: never effective
    let pairs = if req_diags.is_empty() { Vec::new() } else { sugg::alias_ns_pairs(q, uri) };
    // missing imports
    let mut imports: Vec<(String, usize, u32, u32)> = Vec::new();
    for d in &req_diags {
        for (c, n) in missing_imports(q, d.z) {
            imports.push((c, n, d.diag.line, d.diag.ch));
        }
    }
    for (c, n, l, ch) in imports {
        let mut a = Act::new(&format!("Add import '{c}' × {n}"), "quickfix", "add-missing-import", vec![u(), A::N(l as i64), A::N(ch as i64), A::S(c)]).ct("Add missing import");
        a.preferred = true;
        out.push(a);
    }
    // require suggestions
    let mut suggestions: Vec<(Suggestion, u32, u32)> = Vec::new();
    for d in &req_diags {
        if let Some(sym) = safe_sym(d.z) {
            for s in sugg::require_suggestions_with(q, uri, &sym, &pairs) {
                let item = (s, d.diag.line, d.diag.ch);
                if !suggestions.contains(&item) {
                    suggestions.push(item);
                }
            }
        }
    }
    for (s, l, ch) in suggestions {
        let mut a = Act::new(
            &format_require(&s),
            "quickfix",
            "add-require-suggestion",
            vec![u(), A::N(l as i64), A::N(ch as i64), A::S(s.ns.clone()), s.alias.clone().map_or(A::Nil, A::S), s.refer.clone().map_or(A::Nil, A::S), A::Nil],
        )
        .ct("Add require suggestion");
        a.preferred = true;
        out.push(a);
    }
    // alias swap options
    if let Some(z) = zo {
        for s in alias_suggestions(q, uri, tree, z) {
            let title = format!(
                "Swap namespace with alias '[{}{}]'{}",
                s.ns,
                s.alias.as_ref().map_or(String::new(), |a| format!(" :as {a}")),
                s.count.map_or(String::new(), |c| format!(" × {c}"))
            );
            let m = z.meta();
            let mut a = Act::new(&title, "quickfix", "swap-namespace-with-alias", vec![u(), A::N(m.row as i64 - 1), A::N(m.col as i64 - 1), A::S(s.ns.clone()), s.alias.clone().map_or(A::Nil, A::S)]).ct("Swap namespace with alias suggestion");
            a.preferred = true;
            out.push(a);
        }
    }
    // private function to create
    if let Some(d) = rd.iter().find(|d| d.diag.code == "unresolved-symbol") {
        if matches!(d.z.tag(), Tag::List | Tag::Token) {
            let name = d.diag.message.rsplit("Unresolved symbol: ").next().unwrap_or("").to_string();
            out.push(Act::new(&format!("Create private function '{name}'"), "quickfix", "create-function", vec![u(), A::N(d.diag.line as i64), A::N(d.diag.ch as i64)]).ct("Create function"));
        }
    }
    // public function to create
    if let Some(d) = rd.iter().find(|d| matches!(d.diag.code.as_str(), "unresolved-var" | "unresolved-namespace")) {
        if let Some(t) = public_function_title(q, uri, d.z) {
            out.push(Act::new(&t, "quickfix", "create-function", vec![u(), A::N(d.diag.line as i64), A::N(d.diag.ch as i64)]).ct("Create function"));
        }
    }
}

fn public_function_title(q: &Q, uri: &str, z: Z) -> Option<String> {
    if z.tag() != Tag::Token || z.tk() != Tk::Sym {
        return None;
    }
    let sym = z.text();
    let (ns, name) = sym.split_once('/').filter(|(a, b)| !a.is_empty() && !b.is_empty())?;
    let f = q.s.id(uri)?;
    let fa = q.fa(f);
    let alias = crate::intern::intern(ns);
    let usage = fa.namespace_usages.iter().rposition(|u| u.alias == alias);
    let def = usage.and_then(|i| q.find_definition(crate::query::El { f, b: crate::engine::index::B::NsUsage, i: i as u32 }));
    match (def, usage) {
        (Some(d), _) => {
            if !q.internal(d.f) {
                None
            } else {
                Some(format!("Create function '{name}' on '{}'", q.name(d).as_str()))
            }
        }
        (None, Some(i)) => Some(format!("Create namespace '{}' and '{name}' function", fa.namespace_usages[i].to.as_str())),
        (None, None) => Some(format!("Create namespace '{ns}' and '{name}' function")),
    }
}

/// `find-alias-suggestions` for the cursor symbol.
pub fn alias_suggestions(q: &Q, uri: &str, tree: &Tree, z: Z) -> Vec<Suggestion> {
    let Some(sym) = safe_sym(z) else { return vec![] };
    let Some(cursor_ns) = namespace_of(&sym).map(|s| s.to_string()) else { return vec![] };
    let Some(ns_loc) = super::preds::find_namespace(tree) else { return vec![] };
    // find-require: from the ns name, the first list whose first child is :require
    let Some(name) = ns_loc.down().and_then(|d| d.right()) else { return vec![] };
    let mut req = None;
    let mut c = Some(name);
    while let Some(x) = c {
        if x.down().map_or(false, |d| d.tag() == Tag::Token && d.text() == ":require") {
            req = Some(x);
            break;
        }
        c = x.right();
    }
    let Some(req) = req else { return vec![] };
    let Some(start) = req.down() else { return vec![] };
    // ns -> (alias option)
    let mut infos: Vec<(String, Option<String>, Z)> = Vec::new();
    let mut c = start.right();
    while let Some(x) = c {
        if x.tag() == Tag::Vector {
            let ns = x.down().map(|d| d.text().to_string()).unwrap_or_default();
            let mut alias = None;
            let mut k = x.down();
            while let Some(y) = k {
                if y.tag() == Tag::Token && y.text() == ":as" {
                    alias = y.right().map(|a| a.text().to_string());
                    break;
                }
                k = y.right();
            }
            infos.push((ns, alias, x));
        } else {
            infos.push((x.text().to_string(), None, x));
        }
        c = x.right();
    }
    let Some(m) = infos.iter().rev().find(|(n, _, _)| *n == cursor_ns) else { return vec![] };
    let (_, alias, libspec) = m;
    let alias_loc_exists = libspec.tag() == Tag::Vector && alias.is_some();
    if !alias_loc_exists {
        let pairs = sugg::alias_ns_pairs(q, uri);
        let mut matching: Vec<(String, u32)> = pairs.iter().filter(|p| p.ns == cursor_ns && p.alias.is_some()).map(|p| (p.alias.clone().unwrap(), p.count.unwrap_or(0))).collect();
        let found: Vec<String> = matching.iter().map(|(a, _)| a.clone()).collect();
        let mut all: Vec<(String, Option<u32>)> = matching.drain(..).map(|(a, c)| (a, Some(c))).collect();
        for (a, ns) in [("async", "clojure.core.async"), ("csv", "clojure.data.csv"), ("xml", "clojure.data.xml"), ("edn", "clojure.edn"), ("io", "clojure.java.io"), ("sh", "clojure.java.shell"), ("pprint", "clojure.pprint"), ("repl", "clojure.repl"), ("set", "clojure.set"), ("spec", "clojure.spec.alpha"), ("str", "clojure.string"), ("walk", "clojure.walk"), ("zip", "clojure.zip")] {
            if ns == cursor_ns && !found.iter().any(|f| f == a) {
                all.push((a.to_string(), None));
            }
        }
        let in_use: Vec<String> = infos.iter().filter_map(|(_, a, _)| a.clone()).collect();
        let mut out: Vec<Suggestion> = all.into_iter().filter(|(a, _)| !in_use.contains(a)).map(|(a, c)| Suggestion { ns: cursor_ns.clone(), alias: Some(a), refer: None, count: c }).collect();
        // sort-by :count descending (nil last), stable
        out.sort_by(|a, b| match (a.count, b.count) {
            (Some(x), Some(y)) => y.cmp(&x),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        });
        if out.is_empty() {
            let parts: Vec<&str> = cursor_ns.split('.').collect();
            let mut s: Vec<String> = parts[..parts.len() - 1].iter().map(|p| p.chars().next().map(|c| c.to_string()).unwrap_or_default()).collect();
            s.push(parts.last().unwrap().to_string());
            return vec![Suggestion { ns: cursor_ns, alias: Some(s.join(".")), refer: None, count: None }];
        }
        return out;
    }
    alias.clone().map(|a| vec![Suggestion { ns: cursor_ns.clone(), alias: Some(a), refer: None, count: None }]).unwrap_or_default()
}

#[allow(dead_code)]
fn _l(_: fn(&str) -> u8) {
    let _ = file_langs;
}
