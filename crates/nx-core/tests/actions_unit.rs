//! Unit tests of the native refactorings against output captured from the real clojure-lsp (JVM, from source:
//! `refactor/transform.clj`, `feature/thread_get.clj` called on `parser/z-of-string*` zippers).
use nx_core::actions::refactors;
use nx_core::actions::rz::{from_tree, Loc};
use nx_core::actions::thread_get as tg;
use nx_core::actions::transform as t;
use nx_core::actions::tree::Tree;
use nx_core::actions::zops::find_at_pos;

fn loc_at(text: &str, row: u32, col: u32) -> Loc {
    let tree = Tree::parse(text);
    assert!(!tree.err);
    let root = Loc::of_node(from_tree(&tree));
    find_at_pos(&root, row, col).expect("loc")
}

fn edits(v: Vec<t::ZE>) -> Vec<((u32, u32, u32, u32), String)> {
    v.into_iter().map(|z| z.range.map(|m| ((m.row, m.col, m.end_row, m.end_col), z.text)).unwrap()).collect()
}

fn one(r: (u32, u32, u32, u32), s: &str) -> Vec<((u32, u32, u32, u32), String)> {
    vec![(r, s.to_string())]
}

#[test]
fn parse_roundtrip() {
    for src in [
        "(ns a (:require [b :as c]))\n\n(defn f [x] ; c\n  #?(:clj 1 :cljs 2) ^:private y #_(z) 'q `(a ~b ~@c) #\"re\" \\a \"s\\\"t\" ::k :a/b)\n",
        "{:a 1, :b [1 2 3] #:x{:y 1}} #{1} #(inc %) @a #'v ##Inf 1/2 0x1F",
        "(a\r\n b)\r\n;; c\r\n",
    ] {
        let tree = Tree::parse(src);
        assert!(!tree.err, "{src:?}");
        assert_eq!(from_tree(&tree).string(), src);
    }
}

#[test]
fn zipper_remove_trims_whitespace() {
    // rewrite-clj docs: `[1 |2  3] => [|1 3]`, `[1 |2] => [|1]`, `[|1 2] => |[2]`
    let l = loc_at("[1 2  3]", 1, 4);
    assert_eq!(l.remove().root().string(), "[1 3]");
    let l = loc_at("[1 2]", 1, 4);
    assert_eq!(l.remove().root().string(), "[1]");
    let l = loc_at("[1 2]", 1, 2);
    assert_eq!(l.remove().root().string(), "[2]");
}

#[test]
fn colls_and_privacy() {
    assert_eq!(edits(t::change_coll(&loc_at("(def x [1 2 3])", 1, 8), "set")), one((1, 8, 1, 15), "#{1 2 3}"));
    assert_eq!(edits(t::cycle_coll(&loc_at("(def x {:a 1})", 1, 8))), one((1, 8, 1, 14), "[:a 1]"));
    assert_eq!(edits(t::cycle_privacy(&loc_at("(defn foo [] 1)", 1, 3), false)), one((1, 2, 1, 6), "defn-"));
    assert_eq!(edits(t::cycle_privacy(&loc_at("(def foo 1)", 1, 6), false)), one((1, 6, 1, 9), "^:private foo"));
    assert_eq!(edits(t::cycle_privacy(&loc_at("(def ^:private foo 1)", 1, 6), false)), one((1, 6, 1, 19), "foo"));
}

#[test]
fn threading() {
    assert_eq!(
        edits(t::thread_all(&loc_at("(defn f [x]\n  (foo (bar (baz x 1) 2) 3))", 2, 4), "->", false)),
        one((2, 3, 2, 28), "(-> x\n      (baz 1)\n      (bar 2)\n      (foo 3))")
    );
    assert_eq!(
        edits(t::thread_all(&loc_at("(defn f [x]\n  (foo 3 (bar 2 (baz 1 x))))", 2, 4), "->>", false)),
        one((2, 3, 2, 28), "(->> x\n       (baz 1)\n       (bar 2)\n       (foo 3))")
    );
    assert_eq!(edits(t::unwind_all(&loc_at("(-> x (foo 1) (bar 2) baz)", 1, 2))), one((1, 1, 1, 27), "(baz (bar (foo x 1) 2))"));
    assert_eq!(edits(t::unwind_thread(&loc_at("(->> x (foo 1) (bar 2))", 1, 2))), one((1, 1, 1, 24), "(->> (foo 1 x) (bar 2))"));
}

#[test]
fn get_in() {
    assert_eq!(edits(tg::get_in_all(&loc_at("(get (:a (:b m)) :c)", 1, 2))), one((1, 2, 1, 5), "get"));
    assert_eq!(edits(tg::get_in_none(&loc_at("(get-in m [:a :b])", 1, 2))), one((1, 2, 1, 8), "get-in"));
}

#[test]
fn lets_and_defs() {
    let (r, l) = refactors::introduce_let_raw(&loc_at("(defn f [a]\n  (inc a))", 2, 4), "x").unwrap();
    let m = r.unwrap();
    assert_eq!(((m.row, m.col, m.end_row, m.end_col), l.string()), ((2, 4, 2, 7), "(let [x inc]\n     x)".to_string()));
    let e = refactors::extract_to_def(&loc_at("(defn f []\n  (+ 1 2))", 2, 4), None, true).unwrap();
    assert_eq!(
        edits(e),
        vec![((1, 1, 1, 1), "\n(def ^:private new-value\n  +)\n".to_string()), ((2, 4, 2, 5), "new-value".to_string())]
    );
    let e = refactors::suppress_diagnostic(&loc_at("(defn f []\n  (let [x 1] 2))", 2, 8), "unused-binding").unwrap();
    assert_eq!(edits(e), one((2, 3, 2, 3), "#_{:clj-kondo/ignore [:unused-binding]}\n  "));
}
