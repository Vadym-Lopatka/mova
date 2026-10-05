use super::cases::CASES;
use super::*;

#[test]
fn oracle_cases_match_cljfmt() {
    let cfg = FmtConfig::default();
    for (i, (inp, exp)) in CASES.iter().enumerate() {
        assert_eq!(&format(inp, &cfg), exp, "case {} input {:?}", i, inp);
    }
}

fn te(sl: u32, sc: u32, el: u32, ec: u32, t: &str) -> TextEdit {
    TextEdit { start_line: sl, start_col: sc, end_line: el, end_col: ec, new_text: t.into() }
}

#[test]
fn range_formatting_matches_clojure_lsp() {
    let cfg = FmtConfig::default();
    let d = "(a\n b)\n\n(c\nd)\n(e\nf)\n";
    // expected values produced by tools/fmt_oracle/range.clj (clojure-lsp range-formatting replay)
    assert_eq!(format_range_pos(d, 4, 1, 5, 1, &cfg), vec![te(3, 0, 4, 2, "(c\n d)")]);
    assert_eq!(format_range_pos(d, 3, 1, 4, 2, &cfg), vec![te(1, 3, 4, 2, "\n\n(c\n d)")]);
    let d2 = "(defn f [x]\n(let [a 1]\na))\n\n  (foo\nbar)";
    assert_eq!(format_range_pos(d2, 2, 3, 2, 4, &cfg), vec![te(0, 0, 2, 3, "(defn f [x]\n  (let [a 1]\n    a))")]);
    // line-range helper: lines 3..=4 (0-based) cover `(c\nd)`
    assert_eq!(format_range(d, 3, 4, &cfg), vec![te(3, 0, 4, 2, "(c\n d)")]);
    // position outside any node / parse error -> no edits
    assert!(format_range_pos(d, 50, 1, 51, 1, &cfg).is_empty());
    assert!(format_range_pos("(a", 1, 1, 1, 2, &cfg).is_empty());
}

#[test]
fn whole_document_edit_shape() {
    let cfg = FmtConfig::default();
    assert!(format_edits("(a)\n", &cfg).is_empty());
    let e = format_edits("(a\nb)", &cfg);
    assert_eq!(e, vec![te(0, 0, 999_999, 999_999, "(a\n b)")]);
    // parse errors leave the text alone
    assert_eq!(format("(a", &cfg), "(a");
    assert!(format_edits("(a", &cfg).is_empty());
}

#[test]
fn options() {
    let mut c = FmtConfig::default();
    c.indentation = false;
    assert_eq!(format("(a\n b)   \n", &c), "(a\n b)\n");
    let mut c = FmtConfig::default();
    c.function_arguments_indentation = FnArgIndent::Cursive;
    assert_eq!(format("(foo\nbar)", &c), "(foo\n  bar)");
    c.set_indent(Key::Sym("foo".into()), vec![Spec::Block(1)]);
    assert_eq!(format("(foo a\nbar)", &c), "(foo a\n  bar)");
    let mut c = FmtConfig::default();
    c.normalize_newlines_at_file_end = true;
    assert_eq!(format("(a)\n\n\n", &c), "(a)\n");
    // qualified key resolved through the ns alias map
    let mut c = FmtConfig::default();
    c.set_indent(Key::Qual("my.ns".into(), "with-x".into()), vec![Spec::Block(1)]);
    assert_eq!(format("(ns t (:require [my.ns :as m]))\n(m/with-x a\nb)", &c), "(ns t (:require [my.ns :as m]))\n(m/with-x a\n  b)");
    assert_eq!(format("(ns t (:require [my.ns :as m]))\n(m/other a\nb)", &c), "(ns t (:require [my.ns :as m]))\n(m/other a\n         b)");
}

#[test]
fn unicode_and_crlf() {
    let cfg = FmtConfig::default();
    // margin counts UTF-16 units: the emoji is 2 units
    // (value from the JVM oracle)
    assert_eq!(format("[\"\u{1F600}\" [a\nb]]", &cfg), "[\"\u{1F600}\" [a\n       b]]");
    assert_eq!(format("(a\r\nb)\r\n", &cfg), "(a\r\n b)\r\n");
}
