//! textDocument/codeLens + codeLens/resolve (feature/code_lens.clj).
use super::symbols::doc_var_defs;
use super::*;
use std::collections::HashSet;

const EXCLUDED_DEFINED_BY: [(&str, &str); 4] = [
    ("clojure.test", "deftest"),
    ("cljs.test", "deftest"),
    ("state-flow.cljtest", "defflow"),
    ("potemkin", "import-vars"),
];

/// `q/exclude-public-definition?` with default settings, for var definitions.
fn excluded_var(d: &VarDef) -> bool {
    for p in [d.defined_by, d.defined_by_lint_as] {
        if p.1.is_none() {
            continue;
        }
        if EXCLUDED_DEFINED_BY.contains(&(p.0.as_str(), p.1.as_str())) {
            return true;
        }
        if p.0.as_str() == "clojure.core" && p.1.as_str() == "definterface" && !d.protocol_name.is_none() {
            return true;
        }
    }
    d.name.as_str() == "-main"
}

pub fn code_lens(q: &Q, uri: &str) -> String {
    let Some(f) = q.s.id(uri) else { return "[]".into() };
    let Some(fa) = q.entry(f).fa() else { return "[]".into() };
    let mut ps: Vec<Pos> = fa.namespace_definitions.iter().map(|n| n.name_pos).collect();
    for i in doc_var_defs(fa, true) {
        let d = &fa.var_definitions[i];
        if !excluded_var(d) {
            ps.push(d.name_pos);
        }
    }
    let mut seen: HashSet<(u32, u32, u32, u32)> = HashSet::new();
    for k in &fa.keywords {
        if !k.reg.is_none() && seen.insert((k.ns.0, k.name.0, k.pos.row, k.pos.col)) {
            ps.push(k.pos);
        }
    }
    let mut out = String::from("[");
    let mut first = true;
    for p in ps.iter().filter(|p| p.row != 0 && p.col != 0) {
        if !first {
            out.push(',');
        }
        first = false;
        let _ = write!(out, "{{\"range\":{},\"data\":[{},{},{}]}}", range_json(*p), json_str(uri), p.row, p.col);
    }
    out.push(']');
    out
}

fn test_reference(uri: &str) -> bool {
    // default `test-locations-regex`: `_test\.clj[a-z]?$`
    if uri.ends_with("_test.mova") {
        return true;
    }
    let Some(k) = uri.rfind("_test.clj") else { return false };
    let rest = &uri[k + 9..];
    rest.is_empty() || (rest.len() == 1 && rest.as_bytes()[0].is_ascii_lowercase())
}

/// Source-path uri containing `uri` (first match), as `uri->source-path` + `filename->uri`.
fn source_uri(q: &Q, uri: &str) -> Option<String> {
    let path = crate::engine::scan::uri_to_path(uri)?;
    let proj = q.s.project.as_ref()?;
    let sp = proj.source_paths.iter().find(|sp| path.starts_with(sp.as_str()))?;
    Some(crate::engine::scan::path_to_uri(std::path::Path::new(sp)))
}

/// `nums` = [data row, data col, range start line, start char, end line, end char]; default settings (segregation on).
pub fn resolve(q: &Q, uri: &str, nums: &[i64]) -> String {
    let n = |i: usize| nums.get(i).copied().unwrap_or(0).max(0) as u32;
    let (row, col) = (n(0), n(1));
    let at = At { uri, line: row.saturating_sub(1), ch: col.saturating_sub(1) };
    let refs = q.references_from_cursor(at, false, false).unwrap_or_default();
    let src = source_uri(q, uri);
    let (mut main, mut test) = (0usize, 0usize);
    for h in &refs {
        let ru = q.uri(h.f);
        if src.as_deref().map_or(false, |s| !ru.starts_with(s)) && test_reference(ru) {
            test += 1;
        } else {
            main += 1;
        }
    }
    let plural = |c: usize, w: &str| if c == 1 { format!("{c} {w}") } else { format!("{c} {w}s") };
    let title = if test > 0 { format!("{} | {}", plural(main, "reference"), plural(test, "test")) } else { plural(main, "reference") };
    format!(
        "{{\"range\":{{\"start\":{{\"line\":{},\"character\":{}}},\"end\":{{\"line\":{},\"character\":{}}}}},\"command\":{{\"title\":{},\"command\":\"code-lens-references\",\"arguments\":[{},{},{}]}}}}",
        n(2),
        n(3),
        n(4),
        n(5),
        json_str(&title),
        json_str(uri),
        row,
        col
    )
}
