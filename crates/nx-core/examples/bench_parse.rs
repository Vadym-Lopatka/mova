//! Whole-corpus single-thread parse bench. Run: cargo run --release --example bench_parse
use nx_core::parse;
use std::path::{Path, PathBuf};
use std::time::Instant;

fn walk(p: &Path, out: &mut Vec<PathBuf>) {
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

fn main() {
    let Ok(dir) = std::env::var("MOVA_NX_CORPUS_DIR") else {
        println!("skipped: set MOVA_NX_CORPUS_DIR");
        return;
    };
    let roots = [dir];
    let mut files = vec![];
    for r in roots {
        walk(Path::new(&r), &mut files);
    }
    let srcs: Vec<String> = files.iter().filter_map(|f| std::fs::read(f).ok()).map(|b| String::from_utf8_lossy(&b).into_owned()).collect();
    let bytes: usize = srcs.iter().map(|s| s.len()).sum();
    let passes = 5;
    let mut best = f64::MAX;
    let mut nodes = 0;
    let mut errs = 0;
    for _ in 0..passes {
        let t = Instant::now();
        let (mut n, mut e) = (0, 0);
        for s in &srcs {
            let c = parse(s);
            n += c.len();
            e += c.errors().len();
            std::hint::black_box(&c);
        }
        let dt = t.elapsed().as_secs_f64();
        best = best.min(dt);
        nodes = n;
        errs = e;
    }
    println!(
        "files {} | {:.2} MB | best of {} passes {:.1} ms | {:.1} ns/byte | nodes {} | {:.1} bytes/node | errors {} | interned {}",
        srcs.len(), bytes as f64 / 1e6, passes, best * 1e3, best * 1e9 / bytes as f64, nodes, bytes as f64 / nodes as f64, errs, nx_core::intern::len()
    );
}
