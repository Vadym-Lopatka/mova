// Usage: XDG_CACHE_HOME=<private dir> cargo run --release --example bench_io [root]
use nx_core::io::{classpath, jar, project};
use std::path::Path;
use std::time::Instant;

fn main() {
    let Some(root) = std::env::args().nth(1).or_else(|| std::env::var("MOVA_NX_LSP_DIR").ok().map(|d| format!("{d}/lib"))) else {
        println!("skipped: pass [root] or set MOVA_NX_LSP_DIR");
        return;
    };
    let root = Path::new(&root);
    let st = project::Settings::load(root);
    let t = Instant::now();
    let r = classpath::resolve(root, &st);
    println!("classpath resolve (from_cache={}): {:.1} ms; errors={:?}", r.from_cache, t.elapsed().as_secs_f64() * 1e3, r.errors.iter().map(|e| e.1.lines().next().unwrap_or("").to_string()).collect::<Vec<_>>());
    let t = Instant::now();
    let n = 1000;
    for _ in 0..n {
        std::hint::black_box(classpath::lookup_cached(root, &st));
    }
    println!("warm lookup: {:.1} us avg ({} jars, {} dirs)", t.elapsed().as_secs_f64() * 1e6 / n as f64, r.classpath.jars.len(), r.classpath.dirs.len());
    let sp = project::source_paths(root, &st, &r.classpath.dirs);
    println!("source paths: {sp:?}");

    let jars = &r.classpath.jars;
    let t = Instant::now();
    let (mut bytes, mut files, mut buf) = (0usize, 0usize, Vec::new());
    for j in jars {
        let Ok(mut jr) = jar::Jar::open(j) else { continue };
        jr.for_each_source(&mut buf, |_, _, b| {
            bytes += b.len();
            files += 1
        })
        .unwrap();
    }
    let s = t.elapsed().as_secs_f64();
    println!("read sources: {files} files, {:.2} MB, {:.1} ms, {:.0} MB/s", bytes as f64 / 1e6, s * 1e3, bytes as f64 / 1e6 / s);

    let t = Instant::now();
    let mut classes = 0;
    for j in jars {
        if let Ok(mut jr) = jar::Jar::open(j) {
            classes += jr.class_names().len();
        }
    }
    println!("list .class: {classes} names, {:.1} ms", t.elapsed().as_secs_f64() * 1e3);

    let t = Instant::now();
    for j in jars {
        std::hint::black_box(jar::stat_key(Path::new(j)).ok());
    }
    println!("stat_key x{}: {:.1} us", jars.len(), t.elapsed().as_secs_f64() * 1e6);
    let t = Instant::now();
    for j in jars {
        std::hint::black_box(jar::content_hash(Path::new(j)).ok());
    }
    println!("content_hash x{}: {:.1} ms", jars.len(), t.elapsed().as_secs_f64() * 1e3);
}
