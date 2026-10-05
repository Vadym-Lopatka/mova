//! fmt_cmp <in-root> <oracle-out-root> [--show N]: compare nx_core::fmt against JVM cljfmt outputs; prints ns/byte.
use nx_core::fmt::{self, FmtConfig, FnArgIndent, Key, Part, Spec};
use std::path::{Path, PathBuf};
use std::time::Instant;

fn walk(d: &Path, out: &mut Vec<PathBuf>) {
    let mut es: Vec<_> = std::fs::read_dir(d).unwrap().flatten().map(|e| e.path()).collect();
    es.sort();
    for p in es {
        if p.is_dir() {
            walk(&p, out);
        } else if let Some(ext) = p.extension().and_then(|e| e.to_str()) {
            if matches!(ext, "clj" | "cljc" | "cljs" | "bb" | "edn") {
                out.push(p);
            }
        }
    }
}

/// Mirrors tools/fmt_oracle/cfg/*.edn.
fn preset(name: &str) -> FmtConfig {
    let mut c = FmtConfig::default();
    match name {
        "cursive" => c.function_arguments_indentation = FnArgIndent::Cursive,
        "zprint" => c.function_arguments_indentation = FnArgIndent::Zprint,
        "comments" => {
            c.indent_line_comments = true;
            c.remove_multiple_non_indenting_spaces = true;
            c.normalize_newlines_at_file_end = true;
        }
        "extra" => {
            c.set_indent(Key::Sym("defn".into()), vec![Spec::Block(1)]);
            c.set_indent(Key::Sym("fn".into()), vec![Spec::Inner(0, Some(0))]);
            c.set_indent(Key::Qual("clojure.core".into(), "when".into()), vec![Spec::Block(2)]);
            c.set_indent(Key::Qual("clojure.string".into(), "join".into()), vec![Spec::Block(0)]);
            c.set_indent(Key::Sym("my-macro".into()), vec![Spec::Inner(0, None)]);
            c.set_indent(Key::Re("^go-|^async".into()), vec![Spec::Block(1)]);
            c.set_indent(Key::Vec(Part::Re("^clojure".into()), Part::Re("^if".into())), vec![Spec::Inner(0, None)]);
            c.set_indent(Key::Sym("->".into()), vec![Spec::Block(1)]);
            c.set_indent(Key::Sym("->>".into()), vec![Spec::Inner(0, None), Spec::Block(2)]);
            c.set_indent(Key::Sym("reduce".into()), vec![Spec::Inner(1, None)]);
            c.set_indent(Key::Sym("let".into()), vec![Spec::Block(0)]);
        }
        _ => {}
    }
    c
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let (inr, outr) = (PathBuf::from(&a[1]), PathBuf::from(&a[2]));
    let show: usize = a.iter().position(|x| x == "--show").map(|i| a[i + 1].parse().unwrap()).unwrap_or(5);
    let mut files = vec![];
    walk(&inr, &mut files);
    let cfg = preset(a.iter().position(|x| x == "--cfg").map(|i| a[i + 1].as_str()).unwrap_or(""));
    let (mut ok, mut bad, mut both_err, mut bytes, mut ns) = (0, 0, 0, 0usize, 0u128);
    let mut shown = 0;
    let mut srcs: Vec<String> = Vec::new();
    for f in &files {
        let rel = f.strip_prefix(&inr).unwrap();
        let bytes_in = std::fs::read(f).unwrap();
        let Ok(src) = String::from_utf8(bytes_in) else { continue };
        let exp_path = outr.join(rel);
        let err_path = PathBuf::from(format!("{}.ERR", exp_path.display()));
        let t0 = Instant::now();
        let got = fmt::try_format(&src, &cfg);
        ns += t0.elapsed().as_nanos();
        bytes += src.len();
        srcs.push(src.clone());
        if err_path.exists() {
            if got.is_err() { both_err += 1 } else { bad += 1; if shown < show { shown += 1; println!("MISMATCH (oracle error, we formatted) {}", rel.display()); } }
            continue;
        }
        let Ok(exp) = std::fs::read(&exp_path) else { continue };
        let exp = String::from_utf8_lossy(&exp).to_string();
        match got {
            Ok(g) if g == exp => ok += 1,
            Ok(g) => {
                bad += 1;
                if shown < show {
                    shown += 1;
                    let (gl, el): (Vec<&str>, Vec<&str>) = (g.split('\n').collect(), exp.split('\n').collect());
                    let i = (0..gl.len().min(el.len())).find(|&i| gl[i] != el[i]).unwrap_or(gl.len().min(el.len()));
                    println!("MISMATCH {} line {}\n  got: {:?}\n  exp: {:?}", rel.display(), i + 1, gl.get(i), el.get(i));
                }
            }
            Err(e) => {
                bad += 1;
                if shown < show { shown += 1; println!("MISMATCH (we errored: {:?}) {}", e, rel.display()); }
            }
        }
    }
    if a.iter().any(|x| x == "--bench") {
        let total: usize = srcs.iter().map(|s| s.len()).sum();
        let mut best = f64::MAX;
        for _ in 0..7 {
            let t0 = Instant::now();
            let mut n = 0usize;
            for s in &srcs {
                n += fmt::format(s, &cfg).len();
            }
            std::hint::black_box(n);
            best = best.min(t0.elapsed().as_nanos() as f64 / total as f64);
        }
        println!("bench best-of-7 in-memory: {:.1} ns/byte over {} bytes", best, total);
    }
    println!("files ok={} bad={} both_err={}  bytes={} ns/byte={:.1}", ok, bad, both_err, bytes, ns as f64 / bytes.max(1) as f64);
}
