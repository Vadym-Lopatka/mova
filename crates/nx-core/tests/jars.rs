//! Native jar analysis: codec round trip, java scanner, end-to-end on a tiny generated jar, warm cache.
use nx_core::analyzer::*;
use nx_core::intern::intern;
use nx_core::io::classpath::Classpath;
use nx_core::jars::{self, codec, java};
use std::io::Write;

const SRC: &str = "(ns foo.bar \"doc\" (:require [clojure.string :as str]))\n(defn hello \"greets\" ([a] (str/upper-case a)) ([a b & more] nil))\n(defmacro mm [x] x)\n(def ^:private secret 1)\n(defprotocol P (m ^String [this x]))\n(defrecord R [a] P (m [this x] {:a x ::k 1 :foo/bar (java.util.Date.) 'foo.bar/baz (String/valueOf 1)}))\n(s/def ::thing int?)\n";

#[test]
fn fa_roundtrip() {
    let fa = analyze_file(SRC, FileKind::Clj, &Config::new(), &DefsIndex::new());
    assert!(!fa.var_definitions.is_empty());
    assert!(!fa.keywords.is_empty() && !fa.protocol_impls.is_empty() && !fa.java_class_usages.is_empty() && !fa.symbols.is_empty(), "extras present");
    let (mut w, mut t) = (codec::W::default(), codec::StrTab::default());
    codec::encode_fa(&fa, &mut w, &mut t);
    let mut tb = Vec::new();
    t.write(&mut tb);
    let strs = codec::Strs::parse(&tb).unwrap();
    let back = codec::decode_fa(&w.b, &strs).unwrap();
    assert_eq!(emit::to_json("x.clj", "clj", &fa), emit::to_json("x.clj", "clj", &back));
    // standalone Encode/Decode
    use nx_core::io::cache::{Decode, Encode};
    let mut b = Vec::new();
    fa.encode(&mut b);
    let back2 = FileAnalysis::decode(&b).unwrap();
    assert_eq!(emit::to_json("x.clj", "clj", &fa), emit::to_json("x.clj", "clj", &back2));
    assert!(FileAnalysis::decode(&b[..b.len() / 2]).is_none());
}

#[test]
fn java_source_classes() {
    let s = "package a.b;\n/* class X */ public class Outer<T> extends Q { // class no\n  private static class In {}\n  interface I { void f(); }\n  void m() { new Object() { }; Foo.class.getName(); String s = \"class Z\"; }\n  enum E { A { }, B }\n}\nabstract class Second {}\n";
    let v = java::source_classes(s);
    let names: Vec<_> = v.iter().map(|x| (x.0.as_str(), java::flag_names(x.1).collect::<Vec<_>>().join(","))).collect();
    assert_eq!(names, vec![("a.b.Outer", "public".to_string()), ("a.b.Outer$In", "private,static".to_string()), ("a.b.Outer$I", "interface".to_string()), ("a.b.Outer$E", "".to_string()), ("a.b.Second", "abstract".to_string())]);
}

fn make_jar(path: &std::path::Path) {
    let f = std::fs::File::create(path).unwrap();
    let mut z = zip::ZipWriter::new(f);
    let o = zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Stored);
    z.start_file("foo/bar.clj", o).unwrap();
    z.write_all(SRC.as_bytes()).unwrap();
    z.start_file("foo/Impl.java", o).unwrap();
    z.write_all(b"package foo; public final class Impl {}").unwrap();
    z.start_file("clj-kondo.exports/x/config.edn", o).unwrap();
    z.write_all(b"{}").unwrap();
    z.finish().unwrap();
}

#[test]
fn jar_end_to_end_and_warm() {
    let dir = std::env::temp_dir().join(format!("nx-jars-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let jar = dir.join("x-1.0.jar");
    make_jar(&jar);
    let cp = Classpath { jars: vec![jar.to_string_lossy().into_owned(), "/nonexistent.jar".into()], dirs: vec![] };
    let cache = dir.join("cache");
    let cold = jars::analyze_classpath(&cp, &cache);
    assert_eq!((cold.stats.cold, cold.stats.warm, cold.stats.failed), (1, 0, 1));
    let warm = jars::analyze_classpath(&cp, &cache);
    assert_eq!((warm.stats.cold, warm.stats.warm), (0, 1));
    for l in [&cold, &warm] {
        let mut defs = DefsIndex::new();
        l.feed(&mut defs);
        defs.mark_used(intern("foo.bar"), false); // lookups only see namespaces some file used
        let g = |n: &str| defs.get(Src::Clj, intern("foo.bar"), intern(n));
        let hello = g("hello").unwrap();
        assert!(hello.fixed.has(1) && hello.varargs_min == 2);
        assert!(g("mm").unwrap().flags & defs::F_MACRO != 0);
        assert!(g("secret").unwrap().flags & defs::F_PRIVATE != 0);
        assert!(g("nope").is_none());
        let (j, f) = l.locate(intern("foo.bar"), intern("hello")).unwrap();
        let fa = l.file(j, f).unwrap();
        let d = fa.var_definitions.iter().find(|d| d.name == intern("hello")).unwrap();
        assert_eq!(d.doc.as_str(), "greets");
        assert_eq!(l.jars[j].file_name(f), Some("foo/bar.clj"));
        assert_eq!(l.classes().len(), 1);
        assert_eq!(l.classes()[0].1.class, "foo.Impl");
        assert!(l.locate_ns(intern("foo.bar")).is_some());
    }
    // a corrupt cache file is rebuilt, not trusted
    let f = std::fs::read_dir(&cache).unwrap().next().unwrap().unwrap().path();
    let mut b = std::fs::read(&f).unwrap();
    let n = b.len();
    b[n / 2] ^= 0xff;
    std::fs::write(&f, b).unwrap();
    let again = jars::analyze_classpath(&cp, &cache);
    assert_eq!((again.stats.cold, again.stats.failed), (1, 1));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn locate_is_deterministic_across_cold_builds() {
    // `str` is a fn in core.cljs and a macro in core.cljc: locate must always pick the same file
    let dir = std::env::temp_dir().join(format!("nx-loc-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let jar = dir.join("a.jar");
    let f = std::fs::File::create(&jar).unwrap();
    let mut z = zip::ZipWriter::new(f);
    let o = zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Stored);
    z.start_file("cljs/core.cljs", o).unwrap();
    z.write_all(b"(ns cljs.core)\n(defn str [& xs] xs)\n(defn other [] 1)\n(defn another [] 2)\n(defn more [] 3)\n").unwrap();
    z.start_file("cljs/core.cljc", o).unwrap();
    z.write_all(b"(ns cljs.core)\n(defmacro str [& xs] xs)\n(defmacro mac [] 1)\n").unwrap();
    z.finish().unwrap();
    for i in 0..12 {
        let cache = dir.join(format!("c{i}"));
        let layer = jars::analyze_jars(&[jar.to_string_lossy().into_owned()], &cache, &Config::new());
        let (j, fi) = layer.locate(intern("cljs.core"), intern("str")).unwrap();
        assert_eq!(layer.jars[j].file_name(fi), Some("cljs/core.cljc"), "build {i}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
