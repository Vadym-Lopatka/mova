//! `add-known-alias` (feature/add_missing_libspec.clj `add-to-namespace*`, libspec `:require` + alias): the ns form
//! rewritten with `[lib :as alias]` appended, as a single edit over the whole ns form (rewrite-clj whitespace rules).
use super::text::Doc;
use super::*;
use crate::cst::{Kind, NodeId};

pub struct Edit {
    pub range: Pos,
    pub text: String,
}

impl Edit {
    pub fn json(&self) -> String {
        format!("{{\"newText\":{},\"range\":{}}}", json_str(&self.text), range_json(self.range))
    }
}

/// Depth-first pre-order walk (`z/next` order).
fn walk(doc: &Doc, n: NodeId, f: &mut dyn FnMut(NodeId)) {
    f(n);
    for &c in doc.cst.children(n) {
        walk(doc, c, f);
    }
}

/// The edit adding `[lib :as alias]` to the file's `ns` form; None when nothing needs adding or there is no `ns` form.
pub fn add_alias_edit(doc: &Doc, lib: &str, alias: &str) -> Option<Edit> {
    let cst = &doc.cst;
    let ns = cst.children(cst.root()).iter().copied().find(|&n| {
        cst.kind(n) == Kind::List && cst.sig_children(n).next().map_or(false, |h| cst.kind(h) == Kind::Symbol && cst.text(h) == "ns")
    })?;
    let (mut lib_found, mut alias_found, mut require_kw) = (false, false, None);
    walk(doc, ns, &mut |n| match cst.kind(n) {
        Kind::Symbol => {
            let t = cst.text(n);
            lib_found |= t == lib;
            alias_found |= t == alias;
        }
        Kind::Keyword if require_kw.is_none() && cst.text(n) == ":require" => require_kw = Some(n),
        _ => {}
    });
    if lib_found && alias_found {
        return None; // need-to-add-libspec?
    }
    let (ns_start, ns_end) = cst.span(ns);
    let (ns_start, ns_end) = (ns_start as usize, ns_end as usize);
    let src = cst.src();
    let libspec = format!("[{lib} :as {alias}]");
    let (at, ins) = match require_kw.and_then(|k| doc.parent(k).map(|p| (k, p))) {
        Some((_, list)) => {
            let last = cst.sig_children(list).last()?;
            let col = cst.pos(last).col as usize;
            (cst.span(list).1 as usize - 1, format!("\n{}{libspec}", " ".repeat(col.saturating_sub(1))))
        }
        None => {
            let at = ns_end - 1;
            let sep = if src[..at].chars().next_back().map_or(false, char::is_whitespace) { "" } else { " " };
            (at, format!("{sep}\n  (:require\n    {libspec})"))
        }
    };
    if at < ns_start || at > ns_end || src.as_bytes().get(ns_end - 1) != Some(&b')') {
        return None;
    }
    let mut text = String::with_capacity(ns_end - ns_start + ins.len());
    text.push_str(&src[ns_start..at]);
    text.push_str(&ins);
    text.push_str(&src[at..ns_end]);
    Some(Edit { range: cst.pos(ns), text })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn edit(src: &str, lib: &str, alias: &str) -> Option<String> {
        add_alias_edit(&Doc::new(src), lib, alias).map(|e| e.text)
    }
    #[test]
    fn no_require_form() {
        // JVM sample: `(:import ...)` then the new `(:require` form after " \n  "
        let t = edit("(ns app.util\n  (:import [java.util Date UUID]))\n", "app.util", "u").unwrap();
        assert_eq!(t, "(ns app.util\n  (:import [java.util Date UUID]) \n  (:require\n    [app.util :as u]))");
    }
    #[test]
    fn existing_require_aligns_with_last_libspec() {
        let t = edit("(ns a\n  (:require [b :as bb]\n            [c :as cc]))\n", "clojure.string", "str").unwrap();
        assert_eq!(t, "(ns a\n  (:require [b :as bb]\n            [c :as cc]\n            [clojure.string :as str]))");
    }
    #[test]
    fn already_there_is_no_edit() {
        assert!(edit("(ns a\n  (:require [clojure.string :as str]))\n", "clojure.string", "str").is_none());
    }
}
