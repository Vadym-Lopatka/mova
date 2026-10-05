use nx_core::cst::*;
use nx_core::*;

fn d(c: &Cst, id: NodeId) -> String {
    let k = c.kind(id);
    let tok = |n: &str| format!("{n}<{}>", c.text(id));
    match k {
        Kind::Symbol => {
            let (ns, nm) = (c.ns(id), c.name(id));
            if ns.is_none() { format!("sym:{}", nm.as_str()) } else { format!("sym:{}/{}", ns.as_str(), nm.as_str()) }
        }
        Kind::Keyword => {
            let (ns, nm) = (c.ns(id), c.name(id));
            let a = if c.flags(id) & F_AUTO != 0 { "::" } else { ":" };
            if ns.is_none() { format!("kw{a}{}", nm.as_str()) } else { format!("kw{a}{}/{}", ns.as_str(), nm.as_str()) }
        }
        Kind::Number => format!("num{}<{}>", c.flags(id) & F_NUM_MASK, c.text(id)),
        Kind::String => tok("str"),
        Kind::Regex => tok("re"),
        Kind::Char => tok("chr"),
        Kind::Nil => "nil".into(),
        Kind::True => "true".into(),
        Kind::False => "false".into(),
        Kind::Symbolic => format!("##{}", c.name(id).as_str()),
        _ => {
            let kids: Vec<_> = c.children(id).iter().map(|&x| d(c, x)).collect();
            let extra = if k == Kind::ReaderCond && c.flags(id) & F_SPLICING != 0 { "@" } else { "" };
            format!("({:?}{} {})", k, extra, kids.join(" ")).replace(" )", ")")
        }
    }
}
fn dump(s: &str) -> String {
    let c = parse(s);
    d(&c, c.root())
}
fn errs(s: &str) -> Vec<(u32, u32, String)> {
    parse(s).errors().iter().map(|e| (e.row, e.col, e.msg.clone())).collect()
}
fn pos_of(s: &str, nth: usize) -> (u32, u32, u32, u32) {
    let c = parse(s);
    let id = c.children(c.root())[nth];
    let p = c.pos(id);
    (p.row, p.col, p.end_row, p.end_col)
}

#[test]
fn node_is_32_bytes() {
    assert_eq!(std::mem::size_of::<Node>(), 32);
}

#[test]
fn collections() {
    assert_eq!(dump("(a [b] {c d} #{e})"), "(Root (List sym:a (Vector sym:b) (Map sym:c sym:d) (Set sym:e)))");
    assert_eq!(dump("#(+ % 1)"), "(Root (AnonFn sym:+ sym:% num1<1>))");
    assert_eq!(dump(""), "(Root)");
    assert_eq!(dump("()"), "(Root (List))");
}

#[test]
fn symbols_keywords() {
    assert_eq!(dump("a/b clojure.core// / foo.bar"), "(Root sym:a/b sym:clojure.core// sym:/ sym:foo.bar)");
    assert_eq!(dump(":a :a/b ::c ::al/d"), "(Root kw:a kw:a/b kw::c kw::al/d)");
    assert_eq!(dump("can-move-to-:let? a'b"), "(Root sym:can-move-to-:let? sym:a'b)");
    assert_eq!(dump(":a:b"), "(Root kw:a:b)");
    assert_eq!(dump("nil true false nilx"), "(Root nil true false sym:nilx)");
    assert_eq!(dump("ሴ->𝄞"), "(Root sym:ሴ->𝄞)");
}

#[test]
fn strings_regex_chars() {
    assert_eq!(dump(r#""a\"b" #"x\d+""#), r#"(Root str<"a\"b"> re<#"x\d+">)"#);
    assert_eq!(dump(r"\a \newline \ሴ \o12 \u03A9 \( \\ \space"), r"(Root chr<\a> chr<\newline> chr<\ሴ> chr<\o12> chr<\u03A9> chr<\(> chr<\\> chr<\space>)");
    assert_eq!(dump(r"[\a\b]"), r"(Root (Vector chr<\a> chr<\b>))");
    let c = parse(r#""hi" #"re""#);
    let k = c.children(c.root());
    assert_eq!(c.string_content(k[0]), "hi");
    assert_eq!(c.string_content(k[1]), "re");
}

#[test]
fn numbers() {
    for (s, t) in [
        ("1", NUM_INT), ("-1", NUM_INT), ("+1", NUM_INT), ("0x1F", NUM_INT), ("017", NUM_INT), ("2r1010", NUM_INT), ("36rZZ", NUM_INT),
        ("1N", NUM_BIGINT), ("1.5", NUM_FLOAT), ("1e10", NUM_FLOAT), ("1.5e-3", NUM_FLOAT), ("1.", NUM_FLOAT), ("1.5M", NUM_BIGDEC), ("1M", NUM_BIGDEC),
        ("22/7", NUM_RATIO), ("-1/2", NUM_RATIO), ("1.2.3", NUM_INVALID), ("1abc", NUM_INVALID),
    ] {
        let c = parse(s);
        let id = c.children(c.root())[0];
        assert_eq!(c.kind(id), Kind::Number, "{s}");
        assert_eq!(c.flags(id) & F_NUM_MASK, t, "{s}");
        assert_eq!(c.errors().len(), (t == NUM_INVALID) as usize, "{s}");
    }
    assert_eq!(errs("1.2.3")[0].2, "Invalid number: 1.2.3.");
    assert_eq!(dump("-a - +"), "(Root sym:-a sym:- sym:+)");
    assert_eq!(dump("##Inf ##-Inf ##NaN"), "(Root ##Inf ##-Inf ##NaN)");
}

#[test]
fn reader_macros() {
    assert_eq!(dump("'a `b ~c ~@d @e"), "(Root (Quote sym:a) (SyntaxQuote sym:b) (Unquote sym:c) (UnquoteSplicing sym:d) (Deref sym:e))");
    assert_eq!(dump("#'a #=(b)"), "(Root (Var sym:a) (Eval (List sym:b)))");
    assert_eq!(dump("^:foo a ^{:x 1} ^String b #^:z c"), "(Root (Meta kw:foo sym:a) (Meta (Map kw:x num1<1>) (Meta sym:String sym:b)) (Meta kw:z sym:c))");
    assert_eq!(dump("#inst \"2020\" #foo/bar [1]"), "(Root (Tagged sym:inst str<\"2020\">) (Tagged sym:foo/bar (Vector num1<1>)))");
    assert_eq!(dump("#?(:clj 1 :cljs 2) #?@(:clj [a])"), "(Root (ReaderCond (List kw:clj num1<1> kw:cljs num1<2>)) (ReaderCond@ (List kw:clj (Vector sym:a))))");
    assert_eq!(dump("#:a{:b 1} #::{:c 2} #::al{:d 3}"), "(Root (NsMap kw:a (Map kw:b num1<1>)) (NsMap kw:: (Map kw:c num1<2>)) (NsMap kw::al (Map kw:d num1<3>)))");
}

#[test]
fn meta_api() {
    let c = parse("^:a ^:b x");
    let m = c.children(c.root())[0];
    let (mf, t) = c.meta(m).unwrap();
    assert_eq!(c.text(mf), ":a");
    assert_eq!(c.text(c.unwrap_meta(m)), "x");
    assert_eq!(c.kind(t), Kind::Meta);
}

#[test]
fn uneval_kept() {
    assert_eq!(dump("#_:clj-kondo/ignore (foo)"), "(Root (Uneval kw:clj-kondo/ignore) (List sym:foo))");
    assert_eq!(dump("[a #_b c]"), "(Root (Vector sym:a (Uneval sym:b) sym:c))");
    assert_eq!(dump("#_ #_ a b c"), "(Root (Uneval (Uneval sym:a) sym:b) sym:c)");
    assert_eq!(dump("(a #_)"), "(Root (List sym:a (Uneval)))");
    let c = parse("[a #_b c]");
    let v = c.children(c.root())[0];
    assert_eq!(c.sig_children(v).count(), 2);
}

#[test]
fn trivia_skipped() {
    assert_eq!(dump("#!/usr/bin/env bb\n; c\n(a) ;; x\n, ,b"), "(Root (List sym:a) sym:b)");
    assert_eq!(dump("a\u{3000}b"), "(Root sym:a sym:b)");
    assert_eq!(dump("a\u{a0}b"), "(Root sym:a\u{a0}b)");
}

#[test]
fn positions_basic() {
    assert_eq!(pos_of("(foo\n  bar)", 0), (1, 1, 2, 7));
    assert_eq!(pos_of("a\n\n  b", 1), (3, 3, 3, 4));
}

#[test]
fn positions_tabs_crlf() {
    assert_eq!(pos_of("\tfoo", 0), (1, 2, 1, 5)); // tab = 1 col
    assert_eq!(pos_of("a\r\nb", 1), (2, 1, 2, 2)); // CRLF = one break
    assert_eq!(pos_of("a\rb", 1), (2, 1, 2, 2)); // lone CR = break
    assert_eq!(pos_of("a\r\n\r\n  b", 1), (3, 3, 3, 4));
    assert_eq!(pos_of("\"x\r\ny\" z", 1), (2, 4, 2, 5));
}

#[test]
fn positions_utf16() {
    // astral char = 2 UTF-16 units; BMP (ሴ) = 1
    assert_eq!(pos_of("\"𝄞\" a", 1), (1, 6, 1, 7));
    assert_eq!(pos_of("\"ሴ\" a", 1), (1, 5, 1, 6));
    assert_eq!(pos_of("𝄞ሴ", 0), (1, 1, 1, 4));
    assert_eq!(pos_of("\\𝄞 a", 0), (1, 1, 1, 4));
}

#[test]
fn positions_multiline_string() {
    assert_eq!(pos_of("\"ab\ncd\" x", 0), (1, 1, 2, 4));
    assert_eq!(pos_of("\"ab\ncd\" x", 1), (2, 5, 2, 6));
}

#[test]
fn wide_columns() {
    let s = format!("{} a", " ".repeat(70_000));
    let c = parse(&s);
    let id = c.children(c.root())[0];
    assert!(c.flags(id) & F_WIDE != 0);
    let p = c.pos(id);
    assert_eq!((p.row, p.col, p.end_col), (1, 70_002, 70_003));
}

#[test]
fn errors_kondo_messages() {
    assert_eq!(errs(")"), vec![(1, 1, "Unmatched bracket: unexpected )".to_string())]);
    assert_eq!(
        errs("(a\n(b"),
        vec![
            (2, 1, "Found an opening ( with no matching )".to_string()),
            (2, 3, "Expected a ) to match ( from line 2".to_string()),
            (1, 1, "Found an opening ( with no matching )".to_string()),
            (2, 3, "Expected a ) to match ( from line 1".to_string()),
        ]
    );
    let e = errs("(a\n ]");
    assert_eq!(e[0], (1, 1, "Mismatched bracket: found an opening ( and a closing ] on line 2".to_string()));
    assert_eq!(e[1], (2, 2, "Mismatched bracket: found an opening ( on line 1 and a closing ]".to_string()));
    assert_eq!(errs("\"abc")[0].2, "Unexpected EOF while reading string.");
    assert_eq!(errs("'")[0].2, "Unexpected EOF.");
    assert_eq!(errs(": a")[0].2, "Invalid token: :");
    assert_eq!(errs(":")[0].2, "unexpected EOF while reading keyword.");
    // recovery: keeps parsing after the error
    assert_eq!(dump("(a) ) (b)"), "(Root (List sym:a) (List sym:b))");
    assert_eq!(dump("(a ]  b"), "(Root (List sym:a) sym:b)");
}

#[test]
fn never_panics_on_garbage() {
    let samples = ["#", "##", "#:", "#:{", "#?", "#?@", "^", "~", "~@", "\\", "\"\\", "#\"", "#_", "((((", "))))", "{:a", "#{", "\u{0}", "\u{feff}(a)", "#!", "::", ":::a", "a/", "/a", "#_#_#_"];
    for s in samples {
        let _ = parse(s);
    }
    // every prefix/suffix of a mixed sample, on char boundaries
    let big = "(ns a.b \"doc ሴ\" (:require [x :as y])) #?(:clj ^:a {:a 1 #_2 \\c} :cljs #\"re\") #:a{:b ~@c} ##Inf \\newline 1/2 'x `y";
    for i in 0..=big.len() {
        if big.is_char_boundary(i) {
            let _ = parse(&big[..i]);
            let _ = parse(&big[i..]);
        }
    }
    let deep = "(".repeat(100_000);
    let c = parse(&deep);
    assert!(!c.errors().is_empty());
    let deep2 = "[".repeat(300) + &"]".repeat(300);
    assert!(parse(&deep2).errors().is_empty());
}

#[test]
fn text_and_spans() {
    let c = parse("(defn f [x] x)");
    let l = c.children(c.root())[0];
    assert_eq!(c.text(l), "(defn f [x] x)");
    assert_eq!(c.span(l), (0, 14));
    assert_eq!(c.text(c.children(l)[2]), "[x]");
}

#[test]
#[ignore]
fn corpus() {
    use std::path::Path;
    let Ok(dir) = std::env::var("MOVA_NX_CORPUS_DIR") else {
        println!("skipped: set MOVA_NX_CORPUS_DIR");
        return;
    };
    fn walk(p: &Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(rd) = std::fs::read_dir(p) else { return };
        for e in rd.flatten() {
            let q = e.path();
            let Ok(md) = std::fs::symlink_metadata(&q) else { continue };
            if md.is_symlink() {
                continue;
            }
            if md.is_dir() {
                walk(&q, out);
            } else if matches!(q.extension().and_then(|x| x.to_str()), Some("clj" | "cljc" | "cljs" | "edn" | "bb")) {
                out.push(q);
            }
        }
    }
    let roots = [dir];
    let mut files = vec![];
    for r in roots {
        walk(Path::new(&r), &mut files);
    }
    let (mut bytes, mut nodes, mut bad) = (0usize, 0usize, 0usize);
    for f in &files {
        let Ok(b) = std::fs::read(f) else { continue };
        let src = String::from_utf8_lossy(&b).into_owned();
        bytes += src.len();
        let r = std::panic::catch_unwind(|| {
            let c = parse(&src);
            check_invariants(&c);
            (c.len(), c.errors().iter().take(3).map(|e| format!("{}:{} {}", e.row, e.col, e.msg)).collect::<Vec<_>>())
        });
        match r {
            Err(_) => panic!("PANIC on {}", f.display()),
            Ok((n, e)) => {
                nodes += n;
                if !e.is_empty() {
                    bad += 1;
                    println!("ERR {} {:?}", f.display(), e);
                }
            }
        }
    }
    println!("corpus: {} files, {} bytes, {} nodes, {} files with errors", files.len(), bytes, nodes, bad);
}

/// Children are ordered, non-overlapping and inside the parent span.
fn check_invariants(c: &Cst) {
    for i in 0..c.len() {
        let id = NodeId(i as u32);
        let (s, e) = c.span(id);
        assert!(s <= e);
        let mut prev = s;
        for &k in c.children(id) {
            let (ks, ke) = c.span(k);
            assert!(ks >= prev && ke <= e, "child span outside parent");
            prev = ke;
        }
    }
}

#[test]
fn wide_line_positions_are_exact_and_fast() {
    // one 400 KB line with multibyte chars: cols past 65535 must be exact (UTF-16) and cheap
    let mut s = String::from("{");
    for i in 0..40000 {
        s.push_str(&format!(":k{i} \"é\" "));
    }
    s.push_str("}\n(a b)");
    let c = parse(&s);
    let root = c.children(c.root())[0];
    let ch = c.children(root);
    let t = std::time::Instant::now();
    let ps: Vec<u32> = ch.iter().map(|&id| c.pos(id).col).collect();
    assert!(t.elapsed().as_millis() < 500, "quadratic pos");
    for (k, &id) in ch.iter().enumerate().step_by(997) {
        let exact = s[..c.node(id).start as usize].encode_utf16().count() as u32 + 1;
        assert_eq!(ps[k], exact);
    }
    let l2 = c.children(c.root())[1];
    assert_eq!((c.pos(l2).row, c.pos(l2).col), (2, 1));
}

#[test]
fn bare_uneval_at_eof_message() {
    // B21: kondo/edamame: `#_` with nothing after it
    assert_eq!(errs("#_")[0].2, ":uneval node expects 1 value.");
}
