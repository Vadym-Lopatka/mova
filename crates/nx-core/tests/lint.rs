//! Linter smoke tests (parity with clj-kondo is checked against the JVM oracle, see tools/parity.sh).
use nx_core::analyzer::*;

fn lint(src: &str, kind: FileKind) -> Vec<(String, u32, u32, String)> {
    let cfg = Config::new();
    let mut defs = DefsIndex::new();
    let mut fa = analyze_file(src, kind, &cfg, &defs);
    defs.add_file(&fa);
    finish_usages(&mut fa, &defs);
    let mut v: Vec<_> = lint::finish::finalize(fa.base_lang, &fa.findings).into_iter().map(|(f, _)| (f.ty.name().to_owned(), f.pos.row, f.pos.col, f.msg)).collect();
    v.sort();
    v
}

fn has(v: &[(String, u32, u32, String)], ty: &str, msg: &str) -> bool {
    v.iter().any(|f| f.0 == ty && f.3 == msg)
}

#[test]
fn unused_and_unresolved() {
    let v = lint("(ns a (:require [clojure.string :as str] [clojure.set :as set]))\n(defn f [x y] (let [z 1] (undefined-fn x)) (str/join \",\" []))\n", FileKind::Clj);
    assert!(has(&v, "unused-namespace", "namespace clojure.set is required but never used"), "{:?}", v);
    assert!(has(&v, "unused-binding", "unused binding y"));
    assert!(has(&v, "unused-binding", "unused binding z"));
    assert!(has(&v, "unresolved-symbol", "Unresolved symbol: undefined-fn"));
}

#[test]
fn arity_and_types() {
    let v = lint("(ns a)\n(defn g [a] a)\n(defn f [] (g) (g 1 2) (inc \"a\") (map))\n", FileKind::Clj);
    assert!(has(&v, "invalid-arity", "a/g is called with 0 args but expects 1"));
    assert!(has(&v, "invalid-arity", "a/g is called with 2 args but expects 1"));
    assert!(has(&v, "type-mismatch", "Expected: number, received: string."));
    assert!(v.iter().any(|f| f.0 == "invalid-arity" && f.3.starts_with("clojure.core/map is called with 0 args")));
}

#[test]
fn forms_and_ignore() {
    let v = lint("(ns a)\n(defn f [x]\n  (when x)\n  (if x 1)\n  (do x)\n  #_:clj-kondo/ignore\n  (let [unused 1] x))\n", FileKind::Clj);
    assert!(has(&v, "missing-body-in-when", "Missing body in when"));
    assert!(has(&v, "missing-else-branch", "Missing else branch."));
    assert!(has(&v, "redundant-do", "redundant do"));
    assert!(!v.iter().any(|f| f.0 == "unused-binding"), "ignored: {:?}", v);
}

#[test]
fn cljc_collapses_languages() {
    let v = lint("(ns a (:require [clojure.string :as s]))\n(defn f [x] (let [y 1] x))\n", FileKind::Cljc);
    assert_eq!(v.iter().filter(|f| f.0 == "unused-binding").count(), 1, "{:?}", v);
}
