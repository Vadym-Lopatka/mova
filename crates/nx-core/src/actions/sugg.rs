//! Require / import suggestions (`feature/add_missing_libspec.clj` find-* + dep-graph alias data).
use crate::engine::store::FileId;
use crate::intern::SymId;
use crate::query::{file_langs, Q, CLJ, CLJS};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

#[derive(Clone, Debug, PartialEq)]
pub struct Suggestion {
    pub ns: String,
    pub alias: Option<String>,
    pub refer: Option<String>,
    pub count: Option<u32>,
}

#[derive(Clone, Debug)]
pub struct Pair {
    pub ns: String,
    pub alias: Option<String>,
    pub count: Option<u32>,
}

const COMMON_ALIASES: [(&str, &str); 13] = [
    ("async", "clojure.core.async"),
    ("csv", "clojure.data.csv"),
    ("xml", "clojure.data.xml"),
    ("edn", "clojure.edn"),
    ("io", "clojure.java.io"),
    ("sh", "clojure.java.shell"),
    ("pprint", "clojure.pprint"),
    ("repl", "clojure.repl"),
    ("set", "clojure.set"),
    ("spec", "clojure.spec.alpha"),
    ("str", "clojure.string"),
    ("walk", "clojure.walk"),
    ("zip", "clojure.zip"),
];

const COMMON_REFERS: [(&str, &str); 33] = [
    ("deftest", "clojure.test"),
    ("testing", "clojure.test"),
    ("is", "clojure.test"),
    ("are", "clojure.test"),
    ("use-fixture", "clojure.test"),
    ("run-tests", "clojure.test"),
    ("doc", "clojure.repl"),
    ("<!", "clojure.core.async"),
    ("<!!", "clojure.core.async"),
    (">!", "clojure.core.async"),
    (">!!", "clojure.core.async"),
    ("alt!", "clojure.core.async"),
    ("alt!!", "clojure.core.async"),
    ("alts!", "clojure.core.async"),
    ("chan", "clojure.core.async"),
    ("put!", "clojure.core.async"),
    ("take!", "clojure.core.async"),
    ("alts!!", "clojure.core.async"),
    ("go", "clojure.core.async"),
    ("go-loop", "clojure.core.async"),
    ("ANY", "compojure.core"),
    ("DELETE", "compojure.core"),
    ("GET", "compojure.core"),
    ("PATCH", "compojure.core"),
    ("POST", "compojure.core"),
    ("PUT", "compojure.core"),
    ("context", "compojure.core"),
    ("defroutes", "compojure.core"),
    ("defentity", "korma.core"),
    ("reg-event-db", "re-frame.core"),
    ("reg-sub", "re-frame.core"),
    ("reg-event-fx", "re-frame.core"),
    ("fact", "midje.sweet"),
];

pub fn common_alias(a: &str) -> Option<&'static str> {
    COMMON_ALIASES.iter().find(|(k, _)| *k == a).map(|(_, v)| *v)
}
pub fn common_refer(a: &str) -> Option<&'static str> {
    if a == "facts" {
        return Some("midje.sweet");
    }
    COMMON_REFERS.iter().find(|(k, _)| *k == a).map(|(_, v)| *v)
}

struct JarAlias {
    counts: HashMap<(SymId, SymId), u32>,
    langs: HashMap<SymId, u8>,
}

fn jar_alias_index(q: &Q) -> Option<Arc<JarAlias>> {
    static CACHE: OnceLock<Mutex<HashMap<usize, Arc<JarAlias>>>> = OnceLock::new();
    let jars = q.s.jars.as_ref()?;
    let key = Arc::as_ptr(&jars.layer) as usize;
    let m = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(v) = m.lock().unwrap().get(&key) {
        return Some(v.clone());
    }
    let mut counts: HashMap<(SymId, SymId), u32> = HashMap::new();
    let mut langs: HashMap<SymId, u8> = HashMap::new();
    for j in &jars.layer.jars {
        let mut fi = 0;
        while let Some(name) = j.file_name(fi) {
            let fl = file_langs(name);
            if let Some(fa) = j.file(fi) {
                for u in &fa.namespace_usages {
                    let ul = match u.lang {
                        1 => CLJ,
                        2 => CLJS,
                        _ => fl,
                    };
                    *langs.entry(u.to).or_default() |= ul;
                    if !u.alias.is_none() {
                        *counts.entry((u.to, u.alias)).or_default() += 1;
                    }
                }
            }
            fi += 1;
        }
    }
    let v = Arc::new(JarAlias { counts, langs });
    m.lock().unwrap().insert(key, v.clone());
    Some(v)
}

fn internal_files(q: &Q) -> Vec<FileId> {
    let mut out = Vec::new();
    for u in q.s.uris() {
        if let Some(id) = q.s.id(u) {
            if id < crate::engine::jarview::EXT_BASE && q.entry(id).internal {
                out.push(id);
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// dep-graph `ns-aliases-for-langs` + `ns-names-for-langs` rows.
pub fn alias_ns_pairs(q: &Q, uri: &str) -> Vec<Pair> {
    let langs = file_langs(uri);
    let jar = jar_alias_index(q);
    let mut counts: HashMap<(SymId, SymId), u32> = HashMap::new();
    let mut tos_internal: HashMap<SymId, u8> = HashMap::new();
    let mut ns_names: Vec<(String, u8)> = Vec::new();
    for f in internal_files(q) {
        let e = q.entry(f);
        let Some(fa) = e.fa() else { continue };
        let fl = file_langs(&e.uri);
        for u in &fa.namespace_usages {
            let ul = match u.lang {
                1 => CLJ,
                2 => CLJS,
                _ => fl,
            };
            *tos_internal.entry(u.to).or_default() |= ul;
            if !u.alias.is_none() {
                *counts.entry((u.to, u.alias)).or_default() += 1;
            }
        }
        for n in &fa.namespace_definitions {
            ns_names.push((n.name.as_str().to_string(), fl));
        }
    }
    let mut all_langs: HashMap<SymId, u8> = tos_internal.clone();
    if let Some(j) = &jar {
        for ((to, alias), c) in &j.counts {
            *counts.entry((*to, *alias)).or_default() += c;
        }
        for (to, l) in &j.langs {
            *all_langs.entry(*to).or_default() |= l;
        }
    }
    if std::env::var_os("NX_DEBUG_ALIAS").is_some() {
        for ((to, alias), c) in &counts {
            if alias.as_str() == "io" || alias.as_str() == "jio" {
                eprintln!("alias {} {} {} internal={}", to.as_str(), alias.as_str(), c, tos_internal.contains_key(to));
            }
        }
        eprintln!("jar present={}", jar.is_some());
    }
    let mut rows: Vec<Pair> = Vec::new();
    let mut keys: Vec<(&(SymId, SymId), &u32)> = counts.iter().collect();
    keys.sort_by_key(|((to, alias), _)| (to.as_str(), alias.as_str()));
    for ((to, alias), c) in keys {
        if tos_internal.contains_key(to) && all_langs.get(to).map_or(false, |l| l & langs != 0) {
            rows.push(Pair { ns: to.as_str().to_string(), alias: Some(alias.as_str().to_string()), count: Some(*c) });
        }
    }
    // ns-names-for-langs: project namespaces + dependency namespaces (by file extension)
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (n, l) in ns_names {
        if l & langs != 0 && seen.insert(n.clone()) {
            rows.push(Pair { ns: n, alias: None, count: None });
        }
    }
    if let Some(j) = q.s.jars.as_ref() {
        for ns in j.all_ns() {
            let files = j.ns_files(*ns);
            let l: u8 = files.iter().map(|f| file_langs(q.uri(*f))).fold(0, |a, b| a | b);
            if l & langs != 0 && seen.insert(ns.as_str().to_string()) {
                rows.push(Pair { ns: ns.as_str().to_string(), alias: None, count: None });
            }
        }
    }
    rows
}

fn sub_segment(alias_segs: &[&str], def_segs: &[&str]) -> bool {
    let (mut def, mut alias) = (def_segs, alias_segs);
    let (mut i, mut j) = (0usize, 0usize);
    let mut found = false;
    loop {
        if def.is_empty() {
            return alias.is_empty();
        }
        let Some(a) = alias.get(i) else { return false };
        if let Some(d) = def.get(j) {
            if d.starts_with(a) {
                def = &def[j + 1..];
                alias = &alias[i + 1..];
                i = 0;
                j = 0;
                found = true;
            } else if found {
                return false;
            } else {
                j += 1;
            }
        } else if found {
            i += 1;
            j = 0;
        } else {
            return false;
        }
    }
}

type AliasToNs = Vec<(Option<String>, Vec<(String, Option<u32>)>)>; // alias -> [(ns, count)] (first-seen order)
type NsToAliases = Vec<(String, Vec<(String, u32)>)>; // ns -> [(alias, freq)]

fn build_maps(pairs: &[Pair]) -> (AliasToNs, NsToAliases) {
    let mut a2n: AliasToNs = Vec::new();
    for p in pairs {
        let entry = match a2n.iter_mut().find(|(a, _)| *a == p.alias) {
            Some(e) => e,
            None => {
                a2n.push((p.alias.clone(), Vec::new()));
                a2n.last_mut().unwrap()
            }
        };
        match entry.1.iter_mut().find(|(n, _)| *n == p.ns) {
            Some(x) => x.1 = p.count,
            None => entry.1.push((p.ns.clone(), p.count)),
        }
    }
    let mut n2a: NsToAliases = Vec::new();
    for p in pairs {
        let entry = match n2a.iter_mut().find(|(n, _)| *n == p.ns) {
            Some(e) => e,
            None => {
                n2a.push((p.ns.clone(), Vec::new()));
                n2a.last_mut().unwrap()
            }
        };
        if let Some(a) = &p.alias {
            match entry.1.iter_mut().find(|(x, _)| x == a) {
                Some(x) => x.1 += 1,
                None => entry.1.push((a.clone(), 1)),
            }
        }
    }
    for (_, v) in n2a.iter_mut() {
        v.sort_by(|a, b| (std::cmp::Reverse(a.1), &a.0).cmp(&(std::cmp::Reverse(b.1), &b.0)));
    }
    (a2n, n2a)
}

fn best_alias_suggestions(ns_str: &str, a2n: &AliasToNs) -> Option<String> {
    let mut segs: Vec<&str> = ns_str.split('.').collect();
    segs.reverse();
    let segs: Vec<&str> = {
        let mut s = segs.as_slice();
        while !s.is_empty() && s[0] == "core" {
            s = &s[1..];
        }
        s.to_vec()
    };
    let mut acc: Option<String> = None;
    let mut cands: Vec<String> = Vec::new();
    for (i, s) in segs.iter().enumerate() {
        acc = Some(if i == 0 { s.to_string() } else { format!("{}.{}", s, acc.unwrap()) });
        cands.push(acc.clone().unwrap());
    }
    cands.into_iter().find(|c| !a2n.iter().any(|(a, _)| a.as_deref() == Some(c.as_str())) && c != ns_str)
}

fn best_namespaces_suggestions(given: &str, a2n: &AliasToNs, n2a: &NsToAliases) -> Vec<Suggestion> {
    let given_segs: Vec<&str> = given.split('.').collect();
    let mut defs: Vec<Vec<&str>> = n2a.iter().map(|(n, _)| n.split('.').collect()).collect();
    defs.retain(|d| sub_segment(&given_segs, d));
    defs.retain(|d| !d.last().map_or(false, |l| l.ends_with("-test")));
    defs.sort();
    let mut out: Vec<(u8, Suggestion)> = Vec::new();
    for segs in &defs {
        let suggested = segs.join(".");
        if a2n.iter().any(|(a, _)| a.as_deref() == Some(suggested.as_str())) {
            continue;
        }
        let aliases = n2a.iter().find(|(n, _)| *n == suggested).map(|(_, v)| v.clone()).unwrap_or_default();
        if !aliases.is_empty() {
            for (alias, n) in aliases {
                out.push((0, Suggestion { ns: suggested.clone(), alias: Some(alias), refer: None, count: Some(n) }));
            }
            continue;
        }
        let single = given_segs.len() == 1;
        let matches_last = segs.last() == given_segs.last();
        let ns_like = segs.len() == given_segs.len();
        let expand_last = !single && !ns_like && !matches_last;
        let best = best_alias_suggestions(&suggested, a2n).map(|a| (if matches_last { 0u8 } else { 1u8 }, Suggestion { ns: suggested.clone(), alias: Some(a), refer: None, count: None }));
        if single {
            out.extend(best);
        } else if matches_last {
            out.push((2, Suggestion { ns: suggested.clone(), alias: Some(given.to_string()), refer: None, count: None }));
        } else if ns_like {
            out.extend(best);
            out.push((3, Suggestion { ns: suggested.clone(), alias: None, refer: None, count: None }));
        } else if expand_last {
            let mut parts: Vec<&str> = given_segs[..given_segs.len() - 1].to_vec();
            parts.push(segs.last().unwrap());
            out.push((4, Suggestion { ns: suggested.clone(), alias: Some(parts.join(".")), refer: None, count: None }));
        }
    }
    out.retain(|(_, s)| s.alias.as_deref() != Some(s.ns.as_str()));
    out.sort_by(|a, b| (a.0, &a.1.ns).cmp(&(b.0, &b.1.ns)));
    let mut res: Vec<Suggestion> = Vec::new();
    for (_, s) in out {
        if !res.contains(&s) {
            res.push(s);
        }
    }
    res
}

pub fn namespace_suggestions(cursor_ns: &str, pairs: &[Pair]) -> Vec<Suggestion> {
    let (a2n, n2a) = build_maps(pairs);
    let alias_namespaces = a2n.iter().find(|(a, _)| a.as_deref() == Some(cursor_ns)).map(|(_, v)| v.clone());
    let namespace_aliases = n2a.iter().find(|(n, _)| n == cursor_ns).map(|(_, v)| v.clone());
    let common_ns = common_alias(cursor_ns);
    let common_aliases = common_ns.and_then(|c| n2a.iter().find(|(n, _)| n == c)).map(|(_, v)| v.clone()).filter(|v| !v.is_empty());
    if let Some(an) = alias_namespaces {
        return an.into_iter().map(|(n, c)| Suggestion { ns: n, alias: Some(cursor_ns.to_string()), refer: None, count: c }).collect();
    }
    if let Some(na) = &namespace_aliases {
        if !na.is_empty() {
            return na.iter().map(|(a, c)| Suggestion { ns: cursor_ns.to_string(), alias: Some(a.clone()), refer: None, count: Some(*c) }).collect();
        }
    }
    if namespace_aliases.is_some() {
        let mut v: Vec<Suggestion> = best_alias_suggestions(cursor_ns, &a2n).into_iter().map(|a| Suggestion { ns: cursor_ns.to_string(), alias: Some(a), refer: None, count: None }).collect();
        v.push(Suggestion { ns: cursor_ns.to_string(), alias: None, refer: None, count: None });
        return v;
    }
    if let (Some(cn), Some(ca)) = (common_ns, &common_aliases) {
        return ca.iter().map(|(a, c)| Suggestion { ns: cn.to_string(), alias: Some(a.clone()), refer: None, count: Some(*c) }).collect();
    }
    if let Some(cn) = common_ns {
        return vec![
            Suggestion { ns: cn.to_string(), alias: Some(cursor_ns.to_string()), refer: None, count: None },
            Suggestion { ns: cn.to_string(), alias: None, refer: None, count: None },
        ];
    }
    best_namespaces_suggestions(cursor_ns, &a2n, &n2a)
}

fn merge_ns_by_count(pairs: &[Pair], sugg: Vec<Suggestion>) -> Vec<Suggestion> {
    let mut totals: HashMap<&str, u32> = HashMap::new();
    for p in pairs {
        *totals.entry(p.ns.as_str()).or_default() += p.count.unwrap_or(0);
    }
    let mut v: Vec<Suggestion> = sugg
        .into_iter()
        .map(|mut s| {
            let c = totals.get(s.ns.as_str()).copied().unwrap_or(0);
            s.count = if c == 0 { None } else { Some(c) };
            s
        })
        .collect();
    v.sort_by_key(|s| std::cmp::Reverse(s.count.unwrap_or(0)));
    v
}

/// Public var definitions named `name` outside of `skip_nses` whose language overlaps `langs`: namespaces.
fn refer_namespaces(q: &Q, name: &str, langs: u8, skip_nses: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for f in internal_files(q) {
        let e = q.entry(f);
        let Some(fa) = e.fa() else { continue };
        for d in &fa.var_definitions {
            if d.name.as_str() == name && !d.private {
                let l = match d.lang {
                    1 => CLJ,
                    2 => CLJS,
                    _ => file_langs(&e.uri),
                };
                let ns = d.ns.as_str().to_string();
                if l & langs != 0 && !skip_nses.contains(&ns) {
                    out.push(ns);
                }
            }
        }
    }
    if let Some(j) = q.s.jars.as_ref() {
        let nm = crate::intern::intern(name);
        for jar in &j.layer.jars {
            jar.for_each_def(|r| {
                if r.name == nm && r.info.flags & crate::analyzer::defs::F_PRIVATE == 0 {
                    let l = match r.src {
                        crate::analyzer::defs::Src::Clj => CLJ,
                        crate::analyzer::defs::Src::Cljs => CLJS,
                        _ => CLJ | CLJS,
                    };
                    let ns = r.ns.as_str().to_string();
                    if l & langs != 0 && !skip_nses.contains(&ns) {
                        out.push(ns);
                    }
                }
            });
        }
    }
    out
}

/// `find-require-suggestions` for a cursor symbol.
pub fn require_suggestions(q: &Q, uri: &str, cursor_sym: &str) -> Vec<Suggestion> {
    let pairs = alias_ns_pairs(q, uri);
    require_suggestions_with(q, uri, cursor_sym, &pairs)
}

pub fn require_suggestions_with(q: &Q, uri: &str, cursor_sym: &str, pairs: &[Pair]) -> Vec<Suggestion> {
    let (cursor_ns, cursor_name) = match cursor_sym.split_once('/') {
        Some((n, nm)) if !n.is_empty() && !nm.is_empty() => (Some(n.to_string()), nm.to_string()),
        _ => (None, cursor_sym.to_string()),
    };
    let langs = file_langs(uri);
    let ns_sugg = namespace_suggestions(cursor_ns.as_deref().unwrap_or(&cursor_name), pairs);
    if cursor_ns.is_some() {
        return ns_sugg;
    }
    let uri_nses: Vec<String> = q.s.id(uri).and_then(|f| q.entry(f).fa()).map(|fa| fa.namespace_definitions.iter().map(|n| n.name.as_str().to_string()).collect()).unwrap_or_default();
    let refers: Vec<Suggestion> = if let Some(cr) = common_refer(&cursor_name) {
        vec![Suggestion { ns: cr.to_string(), alias: None, refer: Some(cursor_name.clone()), count: None }]
    } else {
        refer_namespaces(q, &cursor_name, langs, &uri_nses).into_iter().map(|ns| Suggestion { ns, alias: None, refer: Some(cursor_name.clone()), count: None }).collect()
    };
    let mut all = ns_sugg;
    all.extend(merge_ns_by_count(pairs, refers));
    all
}

/// `find-missing-ns-alias-require`: the unique namespace for the alias (from project aliases or common aliases).
pub fn missing_alias_ns(pairs: &[Pair], alias: &str) -> Option<String> {
    let mut poss: Vec<String> = Vec::new();
    for p in pairs {
        if p.alias.as_deref() == Some(alias) && !poss.contains(&p.ns) {
            poss.push(p.ns.clone());
        }
    }
    if poss.is_empty() {
        if let Some(c) = common_alias(alias) {
            poss.push(c.to_string());
        }
    }
    if poss.len() == 1 { poss.pop() } else { None }
}
