//! create-test (`refactor/transform.clj`): list predicate + command.
use super::exec::{ask, Ctx, Edit, Out, ResourceChange, Show};
use super::rz::*;
use super::tree::{Meta, Tag, Z};
use crate::query::Q;

fn canon(p: &str) -> String {
    std::fs::canonicalize(p).map(|c| c.to_string_lossy().to_string()).unwrap_or_else(|_| p.to_string())
}

/// Source paths of the project that contain `uri`'s file (`shared/uri->source-paths`).
pub fn uri_source_paths(q: &Q, uri: &str) -> Vec<String> {
    let Some(proj) = q.s.project.as_ref() else { return vec![] };
    let Some(path) = crate::engine::scan::uri_to_path(uri) else { return vec![] };
    // JVM compares the raw uri path with canonical source paths (a /var -> /private/var symlink breaks the match)
    let p = path.to_string_lossy().to_string();
    super::info::jvm_source_paths(proj).iter().filter(|sp| p.starts_with(&format!("{}/", canon(sp).trim_end_matches('/')))).cloned().collect()
}

fn name_of_top(z: Z) -> Option<String> {
    // (some-> loc to-top z/next var-name-loc-from-op)
    let top = z.to_top()?;
    let op = top.next()?;
    let n1 = op.next()?;
    let name = match n1.tag() {
        Tag::Map => n1.right(),
        Tag::Meta if n1.down().map_or(false, |d| d.tag() == Tag::Map) => n1.down().and_then(|d| d.rightmost()),
        Tag::Meta => n1.next().and_then(|x| x.next()),
        _ => Some(n1),
    }?;
    Some(name.text().to_string())
}

/// `can-create-test?`: the function name for the title.
pub fn can_create_test(q: &Q, uri: &str, z: Z) -> Option<String> {
    let name = name_of_top(z)?;
    let sps = uri_source_paths(q, uri);
    sps.iter().find(|sp| !sp.contains("test"))?;
    Some(name)
}

fn namespace_of(q: &Q, uri: &str, source_path: &str) -> Option<String> {
    let path = crate::engine::scan::uri_to_path(uri)?;
    let p = canon(&path.to_string_lossy());
    let sp = canon(source_path);
    let rel = p.strip_prefix(&format!("{}/", sp.trim_end_matches('/')))?;
    let stem = rel.rsplit_once('.').map_or(rel, |(a, _)| a);
    let _ = q;
    Some(stem.replace('/', ".").replace('_', "-"))
}

pub fn create_test(c: &Ctx, z: &Loc) -> Out {
    let Some(text) = c.text.clone() else { return Out::Nil };
    let tree = super::tree::Tree::parse(&text);
    if tree.err {
        return Out::Nil;
    }
    let Some(zz) = super::tree::find_at_pos(&tree, c.row, c.col) else { return Out::Nil };
    let _ = z;
    let Some(fn_name) = can_create_test(c.q, &c.uri, zz) else { return Out::Nil };
    let Some(proj) = c.q.s.project.as_ref() else { return Out::Nil };
    let sps = uri_source_paths(c.q, &c.uri);
    let Some(current) = sps.iter().find(|sp| !sp.contains("test")).cloned() else { return Out::Nil };
    let tests: Vec<String> = super::info::jvm_source_paths(proj).iter().filter(|sp| **sp != current).cloned().collect();
    let chosen = if tests.len() == 1 {
        tests[0].clone()
    } else if tests.len() > 1 {
        let all = super::info::jvm_source_paths(proj);
        let titles: Vec<&str> = all.iter().map(|s| s.as_str()).collect();
        match ask(c, 0, "Choose a source-path to create the test file", &titles) {
            Err(o) => return o,
            Ok(Some(a)) => a,
            Ok(None) => return Out::Nil,
        }
    } else {
        return Out::Err("No source-paths besides current one found".into(), -32602);
    };
    let file_type = c.uri.rsplit('.').next().unwrap_or("clj").to_string();
    let Some(ns) = namespace_of(c.q, &c.uri, &current) else { return Out::Nil };
    let ns_test = format!("{ns}-test");
    let filename = std::path::Path::new(&chosen).join(format!("{}.{}", ns_test.replace('-', "_").replace('.', "/"), file_type));
    let test_uri = crate::engine::scan::path_to_uri(&filename);
    let test_name = format!("{fn_name}-test");
    let max = |m: Meta| m;
    let _ = max;
    match std::fs::read_to_string(&filename) {
        Ok(existing) => {
            // an existing deftest with that name: just show it
            let t = super::tree::Tree::parse(&existing);
            let mut found: Option<Meta> = None;
            if !t.err {
                for top in t.root().kid_zs() {
                    if top.tag() == Tag::List {
                        let op = top.down();
                        let nm = op.and_then(|o| o.right());
                        if op.map_or(false, |o| o.is_sym() && o.text().rsplit('/').next() == Some("deftest")) && nm.map_or(false, |n| n.text() == test_name) {
                            found = Some(top.meta());
                            break;
                        }
                    }
                }
            }
            if let Some(m) = found {
                return Out::Map { changes: vec![], resources: vec![], show: Some(Show { uri: test_uri, range: Some(m) }) };
            }
            // `(count (string/split existing-text #"\n"))`: Java drops trailing empty strings
            let lines = if !existing.contains('\n') {
                1
            } else {
                let mut v: Vec<&str> = existing.split('\n').collect();
                while v.last() == Some(&"") {
                    v.pop();
                }
                v.len()
            };
            let body = format!("\n(deftest {test_name}\n  (is (= 1 1)))");
            Out::Map {
                changes: vec![(test_uri.clone(), vec![Edit { range: Some(Meta { row: lines as u32 + 1, col: 1, end_row: lines as u32 + 3, end_col: 1 }), text: body }])],
                resources: vec![],
                show: Some(Show { uri: test_uri, range: None }),
            }
        }
        Err(_) => {
            let rt = if file_type == "cljs" { "cljs" } else { "clojure" };
            let ns_text = format!("(ns {ns_test}\n  (:require\n   [{rt}.test :refer [deftest is]]\n   [{ns} :as subject]))");
            let test_text = format!("(deftest {test_name}\n  (is (= true\n         (subject/foo))))");
            let full = format!("{ns_text}\n\n{test_text}");
            // forms node meta: start (1,1), end of the last node
            let (er, ec) = {
                let lines: Vec<&str> = full.split('\n').collect();
                (lines.len() as u32, lines.last().map_or(0, |l| l.encode_utf16().count() as u32) + 1)
            };
            Out::Map {
                changes: vec![(test_uri.clone(), vec![Edit { range: Some(Meta { row: 1, col: 1, end_row: er, end_col: ec }), text: full }])],
                resources: vec![ResourceChange { uri: test_uri.clone() }],
                show: Some(Show { uri: test_uri, range: None }),
            }
        }
    }
}
