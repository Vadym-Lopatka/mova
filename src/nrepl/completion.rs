//! `completions`: a port of `nrepl.util.completion` (compliment lite) over
//! the interpreter's own tables. One pass over the globals table, no
//! reflection, no classpath scan.
//!
//! Sources, in the JVM's order (the result is sorted by `candidate`, a stable
//! sort, so the order only matters for equal candidates):
//!
//! 1. static members (`System/get`): the natives interned under the class name
//! 2. namespaces (`clojure.str`, alias names with `/`)
//! 3. classes (the classes Mova knows; no JDK scan)
//! 4. vars (`ma`, `str/j`, `clojure.string/`, `#'x`)
//! 5. keywords (`:ke`, `::foo`, `::alias/x`)
//! 6. special forms
//!
//! Instance members (`.getMonth`) need reflection: not offered.

use crate::env::VarCell;
use crate::eval::Interp;
use crate::keyword::Keyword;
use crate::value::{Str, Symbol, Value};
use std::collections::HashSet;
use std::sync::Arc;

/// One candidate. Fields that are `None` are left out of the reply.
#[derive(Debug, Clone, Default)]
pub(crate) struct Cand {
    pub candidate: String,
    pub typ: &'static str,
    pub ns: Option<String>,
    pub file: Option<String>,
    pub package: Option<String>,
    pub priority: bool,
}

impl Cand {
    fn new(candidate: String, typ: &'static str) -> Cand {
        Cand { candidate, typ, ..Cand::default() }
    }
}

const SPECIAL_FORMS: &[&str] = &[
    "def", "if", "do", "quote", "var", "recur", "throw", "try", "catch", "monitor-enter", "monitor-exit", "new", "set!", "true",
    "false", "nil",
];

/// `compliment.utils/fuzzy-matches?`: `prefix` against `symbol`, split on `sep`.
pub(crate) fn fuzzy_matches(prefix: &str, symbol: &str, sep: char) -> bool {
    if prefix.is_ascii() && symbol.is_ascii() && sep.is_ascii() {
        fuzzy_slice(prefix.as_bytes(), symbol.as_bytes(), sep as u8)
    } else {
        let p: Vec<char> = prefix.chars().collect();
        let s: Vec<char> = symbol.chars().collect();
        fuzzy_slice(&p, &s, sep)
    }
}

fn fuzzy_slice<T: Copy + PartialEq>(p: &[T], s: &[T], sep: T) -> bool {
    let (pn, sn) = (p.len(), s.len());
    if pn == 0 {
        return true;
    }
    if sn == 0 || p[0] != s[0] {
        return false;
    }
    let (mut pi, mut si, mut skipping) = (1, 1, false);
    loop {
        if pi >= pn {
            return true;
        }
        if si >= sn {
            return false;
        }
        let matched = p[pi] == s[si];
        if s[si] == sep {
            if matched {
                pi += 1;
            }
            si += 1;
            skipping = false;
        } else if skipping || !matched {
            si += 1;
            skipping = true;
        } else {
            pi += 1;
            si += 1;
            skipping = false;
        }
    }
}

/// `compliment.lite/camel-case-matches?`: `getDeF` matches `getDeclaredFields`.
fn camel_matches(prefix: &str, name: &str) -> bool {
    let p: Vec<char> = prefix.chars().collect();
    let s: Vec<char> = name.chars().collect();
    let (pn, sn) = (p.len(), s.len());
    if pn == 0 {
        return true;
    }
    if sn == 0 || p[0] != s[0] {
        return false;
    }
    let (mut pi, mut si, mut skipping) = (1, 1, false);
    loop {
        if pi >= pn {
            return true;
        }
        if si >= sn {
            return false;
        }
        if skipping {
            if s[si].is_uppercase() {
                skipping = false;
            } else {
                si += 1;
            }
        } else if p[pi] == s[si] {
            pi += 1;
            si += 1;
        } else {
            si += 1;
            skipping = true;
        }
    }
}

/// `(re-matches #"(@{0,2}#'|'|@)?(.*)" s)`: the leading quote / var / deref marks.
fn split_literals(s: &str) -> (&str, &str) {
    let b = s.as_bytes();
    let at = b.iter().take(2).take_while(|c| **c == b'@').count();
    // `@{0,2}#'`, longest first
    for n in (0..=at).rev() {
        if b.len() >= n + 2 && &b[n..n + 2] == b"#'" {
            return s.split_at(n + 2);
        }
    }
    if b.first() == Some(&b'\'') || b.first() == Some(&b'@') {
        return s.split_at(1);
    }
    ("", s)
}

/// `var-symbol?`: `(?:([^/:][^/]*)/)?(|[^/:][^/]*)`. `(scope, name)`.
fn var_symbol(x: &str) -> Option<(Option<&str>, &str)> {
    let first_ok = |t: &str| t.chars().next().is_some_and(|c| c != '/' && c != ':');
    if let Some(i) = x.find('/') {
        let (scope, rest) = (&x[..i], &x[i + 1..]);
        if !first_ok(scope) {
            return None;
        }
        if rest.is_empty() || (first_ok(rest) && !rest.contains('/')) {
            return Some((Some(scope), rest));
        }
        return None;
    }
    if x.is_empty() || first_ok(x) {
        Some((None, x))
    } else {
        None
    }
}

/// `nscl-symbol?`: `[^/:]+` and not starting with a dot.
fn nscl_symbol(x: &str) -> bool {
    !x.is_empty() && !x.contains('/') && !x.contains(':') && !x.starts_with('.')
}

/// `qualified-member-symbol?`: `([^/:.][^:]*)(?<!\.)/(.*)`, greedy.
fn qualified_member(x: &str) -> Option<(&str, &str)> {
    let first = x.chars().next()?;
    if first == '/' || first == ':' || first == '.' {
        return None;
    }
    // greedy: the last '/' whose class part has no ':' and does not end in '.'
    for (i, c) in x.char_indices().rev() {
        if c == '/' && i > 0 && !x[..i].ends_with('.') && !x[..i].contains(':') {
            return Some((&x[..i], &x[i + 1..]));
        }
    }
    None
}

/// Java `String.compareTo` order (UTF-16 code units).
pub(crate) fn java_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    if a.is_ascii() && b.is_ascii() {
        a.as_bytes().cmp(b.as_bytes())
    } else {
        a.encode_utf16().cmp(b.encode_utf16())
    }
}

/// All candidates for `prefix` as seen from namespace `ns` (a name; an unknown
/// one falls back to `user`, as `compliment.core/ensure-ns` does).
pub(crate) fn completions(interp: &Interp, prefix: &str, ns: &str) -> Vec<Cand> {
    let ns: Str = if interp.find_ns_value(&Value::Sym(Symbol::simple(ns))).is_some() {
        Str::from(ns)
    } else {
        Str::from(crate::ns::USER_NS)
    };
    let mut out: Vec<Cand> = Vec::new();
    static_members(interp, prefix, &ns, &mut out);
    namespaces(interp, prefix, &ns, &mut out);
    classes(interp, prefix, &mut out);
    vars(interp, prefix, &ns, &mut out);
    keywords(interp, prefix, &ns, &mut out);
    if var_symbol(prefix).is_some() {
        for f in SPECIAL_FORMS {
            if fuzzy_matches(prefix, f, '-') {
                out.push(Cand::new((*f).to_string(), "special-form"));
            }
        }
    }
    out.sort_by(|a, b| java_cmp(&a.candidate, &b.candidate));
    out
}

/// The class a name stands for in `ns`: `(ns-resolve ns sym)` when it is a class.
fn resolve_class(interp: &Interp, name: &str) -> Option<String> {
    match interp.lookup_global(&Symbol::simple(name)) {
        Some(Value::Class(c)) => Some(c.name().to_string()),
        _ => None,
    }
}

fn short_name(full: &str) -> &str {
    full.rsplit(['.', '$']).next().unwrap_or(full)
}

fn static_members(interp: &Interp, prefix: &str, _ns: &Str, out: &mut Vec<Cand>) {
    let Some((cl_name, member_prefix)) = qualified_member(prefix) else { return };
    if member_prefix.starts_with('.') {
        return;
    }
    let Some(canon) = resolve_class(interp, cl_name) else { return };
    let short = short_name(&canon).to_string();
    let inparts = member_prefix.chars().any(|c| c.is_uppercase());
    let mut seen: HashSet<String> = HashSet::new();
    let mut found: Vec<Cand> = Vec::new();
    interp.globals.for_each_cell(&mut |sym, cell| {
        let Some(q) = &sym.ns else { return };
        if q.as_ref() != canon && q.as_ref() != short {
            return;
        }
        let name: &str = &sym.name;
        let ok = if inparts { camel_matches(member_prefix, name) } else { name.starts_with(member_prefix) };
        if !ok || !seen.insert(name.to_string()) {
            return;
        }
        let typ = match cell.raw_root() {
            Some(Value::Native(_)) | Some(Value::Fn(_)) | Some(Value::Macro(_)) => "static-method",
            _ => "static-field",
        };
        found.push(Cand { candidate: format!("{cl_name}/{name}"), typ, priority: true, ..Cand::default() });
    });
    out.extend(found);
}

fn namespaces(interp: &Interp, prefix: &str, ns: &Str, out: &mut Vec<Cand>) {
    if !nscl_symbol(prefix) {
        return;
    }
    let (literals, p) = split_literals(prefix);
    for (alias, _) in interp.ns_aliases_of(ns) {
        if fuzzy_matches(p, &alias, '.') {
            out.push(Cand { candidate: format!("{literals}{alias}/"), typ: "namespace", priority: true, ..Cand::default() });
        }
    }
    for name in interp.user_visible_ns_names() {
        if fuzzy_matches(p, &name, '.') {
            out.push(Cand { candidate: format!("{literals}{name}"), typ: "namespace", priority: true, ..Cand::default() });
        }
    }
}

/// The canonical names of the classes the globals table binds (bare, in
/// clojure.core). Cached per global generation: a `def` or `import` anywhere
/// bumps it, so a stale list is never served.
fn known_classes(interp: &Interp) -> Arc<Vec<String>> {
    use std::sync::Mutex;
    static CACHE: Mutex<Option<(u64, Arc<Vec<String>>)>> = Mutex::new(None);
    let generation = crate::env::global_generation();
    if let Some((g, v)) = CACHE.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        if *g == generation {
            return v.clone();
        }
    }
    let mut all: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    interp.globals.for_each_cell(&mut |sym, cell| {
        if sym.ns.is_some() {
            return;
        }
        if let Some(Value::Class(c)) = cell.raw_root() {
            if matches!(*c, crate::types::ClassVal::User(_)) {
                return;
            }
            if seen.insert(c.name().to_string()) {
                all.push(c.name().to_string());
            }
        }
    });
    all.sort();
    let v = Arc::new(all);
    *CACHE.lock().unwrap_or_else(|e| e.into_inner()) = Some((generation, v.clone()));
    v
}

fn classes(interp: &Interp, prefix: &str, out: &mut Vec<Cand>) {
    if !nscl_symbol(prefix) {
        return;
    }
    let all = known_classes(interp);
    let all: &[String] = &all;
    let has_dot = prefix.contains('.');
    let upper = prefix.chars().next().is_some_and(|c| c.is_uppercase());
    let mut seen: HashSet<String> = HashSet::new();
    let mut found: Vec<Cand> = Vec::new();
    // classes imported into the namespace: full and simple names
    for full in all {
        let simple = short_name(full);
        if fuzzy_matches(prefix, full, '.') && seen.insert(full.clone()) {
            found.push(Cand { candidate: full.clone(), typ: "class", priority: true, ..Cand::default() });
        }
        if fuzzy_matches(prefix, simple, '.') && seen.insert(simple.to_string()) {
            let pkg = full.rfind('.').map(|i| full[..i].to_string());
            found.push(Cand { candidate: simple.to_string(), typ: "class", package: pkg, priority: true, ..Cand::default() });
        }
    }
    if upper {
        for full in all {
            if short_name(full).starts_with(prefix) && seen.insert(full.clone()) {
                found.push(Cand { candidate: full.clone(), typ: "class", priority: true, ..Cand::default() });
            }
        }
    }
    let mut roots: Vec<&str> = all.iter().filter_map(|f| f.split('.').next().filter(|r| *r != f.as_str())).collect();
    roots.sort_unstable();
    roots.dedup();
    if has_dot || roots.contains(&prefix) {
        for full in all {
            if full.starts_with(prefix) && seen.insert(full.clone()) {
                found.push(Cand { candidate: full.clone(), typ: "class", priority: true, ..Cand::default() });
            }
        }
    } else {
        for r in roots {
            if r.starts_with(prefix) {
                found.push(Cand { candidate: format!("{r}."), typ: "class", priority: true, ..Cand::default() });
            }
        }
    }
    out.extend(found);
}

/// The namespace a scope name means in `ns`: an alias of `ns`, else a namespace.
fn resolve_namespace(interp: &Interp, name: &str, ns: &Str) -> Option<Str> {
    if let Some((_, full)) = interp.ns_aliases_of(ns).into_iter().find(|(a, _)| a.as_ref() == name) {
        return Some(full);
    }
    let name = Str::from(name);
    interp.user_visible_ns_names().into_iter().find(|n| *n == name)
}

fn truthy_key(m: &crate::value::PMap, key: &str) -> bool {
    matches!(m.get(&Value::Keyword(Keyword::from(key))), Some(v) if !matches!(v, Value::Nil | Value::Bool(false)))
}

/// `macro`, `function` (has `:arglists`) or `var`; `None` when `:completion/hidden`.
fn var_type(cell: &VarCell, root: &Option<Value>) -> Option<&'static str> {
    let meta = cell.var_meta();
    let mut has_arglists = false;
    let mut is_macro = matches!(root, Some(Value::Macro(_)));
    if let Value::Map(m) = &meta {
        if truthy_key(m, "completion/hidden") {
            return None;
        }
        is_macro |= truthy_key(m, "macro");
        has_arglists = m.contains_key(&Value::Keyword(Keyword::from("arglists")));
    }
    if is_macro {
        return Some("macro");
    }
    if !has_arglists {
        // natives carry no `:arglists`; neither do core fns until the docs table is asked
        has_arglists = matches!(root, Some(Value::Native(_)))
            || (cell.name.ns.is_none() && (crate::coredocs::has_arglists(&cell.name.name) || matches!(root, Some(Value::Fn(_)))));
    }
    Some(if has_arglists { "function" } else { "var" })
}

fn var_cand(literals: &str, shown: &str, cell: &Arc<VarCell>) -> Option<Cand> {
    // `--name` vars are the interpreter's own helpers, not for users
    if cell.name.name.starts_with("--") {
        return None;
    }
    let root = cell.raw_root();
    if matches!(root, Some(Value::Class(_))) {
        return None;
    }
    let typ = var_type(cell, &root)?;
    let home = cell.name.ns.as_deref().unwrap_or(crate::ns::CORE_NS).to_string();
    Some(Cand { candidate: format!("{literals}{shown}"), typ, ns: Some(home), priority: true, ..Cand::default() })
}

fn vars(interp: &Interp, prefix: &str, ns: &Str, out: &mut Vec<Cand>) {
    let (literals, p) = split_literals(prefix);
    let Some((scope_name, pfx)) = var_symbol(p) else { return };
    let first = pfx.as_bytes().first().copied();
    let quick = |name: &str| first.is_none_or(|f| name.as_bytes().first() == Some(&f) || !name.is_ascii() || !pfx.is_ascii());
    if let Some(sn) = scope_name {
        let Some(scope) = resolve_namespace(interp, sn, ns) else { return };
        let privates = literals.ends_with("#'");
        let core = scope.as_ref() == crate::ns::CORE_NS;
        let mut found: Vec<Cand> = Vec::new();
        interp.globals.for_each_cell(&mut |sym, cell| {
            let own = sym.ns.as_ref() == Some(&scope) || (core && sym.ns.is_none());
            if !own || !quick(&sym.name) || !fuzzy_matches(pfx, &sym.name, '-') {
                return;
            }
            if !privates && cell.is_private() {
                return;
            }
            if let Some(c) = var_cand(literals, &format!("{sn}/{}", sym.name), cell) {
                found.push(c);
            }
        });
        out.extend(found);
        return;
    }
    // ns-map: the namespace's own vars, then its refers, then clojure.core
    let core_ns = ns.as_ref() == crate::ns::CORE_NS;
    let sees_core = core_ns || interp.ns_sees_core(ns);
    let mut rows: Vec<(Str, u8, Arc<VarCell>)> = Vec::new();
    interp.globals.for_each_cell(&mut |sym, cell| {
        let rank = if !core_ns && sym.ns.as_ref() == Some(ns) {
            0
        } else if sym.ns.is_none() && sees_core {
            2
        } else {
            return;
        };
        if quick(&sym.name) && fuzzy_matches(pfx, &sym.name, '-') {
            rows.push((sym.name.clone(), rank, cell.clone()));
        }
    });
    for (local, (from, source)) in interp.ns_refers_of(ns) {
        if !fuzzy_matches(pfx, &local, '-') {
            continue;
        }
        let sym = crate::ns::var_symbol_in(&from, &source);
        if let Some(cell) = interp.globals.find_any_cell(&sym) {
            rows.push((local, 1, cell));
        }
    }
    rows.sort_by(|a, b| (a.0.as_ref(), a.1).cmp(&(b.0.as_ref(), b.1)));
    rows.dedup_by(|b, a| a.0 == b.0);
    for (name, _, cell) in rows {
        if let Some(c) = var_cand(literals, &name, &cell) {
            out.push(c);
        }
    }
}

fn keywords(interp: &Interp, prefix: &str, ns: &Str, out: &mut Vec<Cand>) {
    let kw = |s: String| Cand::new(s, "keyword");
    if let Some(rest) = prefix.strip_prefix("::") {
        if let Some((alias, name_prefix)) = rest.split_once('/') {
            if alias.is_empty() {
                return;
            }
            let target = resolve_namespace(interp, alias, ns).map(|s| s.to_string()).unwrap_or_default();
            crate::keyword::for_each_interned(&mut |text| {
                if let Some((kns, kname)) = text.split_once('/') {
                    if kns == target && kname.starts_with(name_prefix) {
                        out.push(kw(format!("::{alias}/{kname}")));
                    }
                }
            });
            return;
        }
        crate::keyword::for_each_interned(&mut |text| {
            if let Some((kns, kname)) = text.split_once('/') {
                if kns == ns.as_ref() && kname.starts_with(rest) {
                    out.push(kw(format!("::{kname}")));
                }
            }
        });
        for (alias, _) in interp.ns_aliases_of(ns) {
            if alias.starts_with(rest) {
                out.push(kw(format!("::{alias}/")));
            }
        }
    } else if let Some(rest) = prefix.strip_prefix(':') {
        crate::keyword::for_each_interned(&mut |text| {
            if text.starts_with(rest) {
                out.push(kw(format!(":{text}")));
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_dash_and_dot() {
        assert!(fuzzy_matches("ma", "map-indexed", '-'));
        assert!(fuzzy_matches("m-i", "map-indexed", '-'));
        // after a separator the next prefix char may match the start of a part
        assert!(fuzzy_matches("ma", "mix-a", '-'));
        assert!(!fuzzy_matches("mb", "mix-a", '-'));
        assert!(fuzzy_matches("c.s", "clojure.string", '.'));
        assert!(!fuzzy_matches("s", "clojure.string", '.'));
        assert!(fuzzy_matches("", "x", '-'));
    }

    #[test]
    fn camel() {
        assert!(camel_matches("getDeF", "getDeclaredFields"));
        assert!(!camel_matches("getX", "getDeclaredFields"));
    }

    #[test]
    fn literals_and_symbols() {
        assert_eq!(split_literals("@@#'foo"), ("@@#'", "foo"));
        assert_eq!(split_literals("#'foo"), ("#'", "foo"));
        assert_eq!(split_literals("'foo"), ("'", "foo"));
        assert_eq!(split_literals("@foo"), ("@", "foo"));
        assert_eq!(split_literals("foo"), ("", "foo"));
        assert_eq!(var_symbol("ma"), Some((None, "ma")));
        assert_eq!(var_symbol("a/b"), Some((Some("a"), "b")));
        assert_eq!(var_symbol("clojure.string/"), Some((Some("clojure.string"), "")));
        assert_eq!(var_symbol(""), Some((None, "")));
        assert_eq!(var_symbol("a/b/c"), None);
        assert_eq!(var_symbol(":kw"), None);
        assert_eq!(var_symbol("/x"), None);
        assert_eq!(qualified_member("System/get"), Some(("System", "get")));
        assert_eq!(qualified_member("a.b/"), Some(("a.b", "")));
        assert_eq!(qualified_member("a./x"), None);
        assert_eq!(qualified_member("System"), None);
    }
}
