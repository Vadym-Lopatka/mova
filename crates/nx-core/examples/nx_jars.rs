//! `nx_jars <project-root> [--cache dir] [--out dir] [--jars-file f]`: native jar analysis of the project's classpath.
//! Prints cold/warm timings, sizes; with `--out` writes oracle-schema JSON per jar entry (`<jar>!/<entry>`).
use nx_core::analyzer::{emit, DefsIndex, FileKind};
use nx_core::io::classpath::{self, Classpath};
use nx_core::io::project;
use nx_core::jars::{self, java};
use std::path::{Path, PathBuf};
use std::time::Instant;

fn js(s: &str) -> String {
    format!("{:?}", s) // class names/paths: no control chars; Rust escapes are JSON-compatible for these
}

fn footprint_kb() -> u64 {
    let o = std::process::Command::new("footprint").arg(std::process::id().to_string()).output().ok();
    let t = o.map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
    let l = t.lines().find(|l| l.contains("Footprint:")).unwrap_or("");
    let v = l.split("Footprint:").nth(1).unwrap_or("").trim();
    let mut it = v.split_whitespace();
    let n: f64 = it.next().and_then(|x| x.parse().ok()).unwrap_or(0.0);
    (n * match it.next() { Some("MB") => 1024.0, Some("GB") => 1048576.0, _ => 1.0 }) as u64
}

/// `--bench`: cold run (cache dir must be empty), then 20 warm runs (load + feed), footprints.
fn bench(cp: &Classpath, cache: &Path, cfg: &nx_core::analyzer::Config) {
    let _ = std::fs::remove_dir_all(cache);
    let fp0 = footprint_kb();
    let t = Instant::now();
    let layer = jars::analyze_classpath_with(cp, cache, cfg);
    let cold = t.elapsed().as_secs_f64() * 1e3;
    let mut defs = DefsIndex::new();
    let t = Instant::now();
    layer.feed(&mut defs);
    let feed_cold = t.elapsed().as_secs_f64() * 1e3;
    let st = layer.stats;
    drop((layer, defs));
    let mut warm = Vec::new();
    let mut feed = Vec::new();
    for _ in 0..20 {
        let t = Instant::now();
        let l = jars::analyze_classpath_with(cp, cache, cfg);
        warm.push(t.elapsed().as_secs_f64() * 1e3);
        let mut d = DefsIndex::new();
        let t = Instant::now();
        l.feed(&mut d);
        feed.push(t.elapsed().as_secs_f64() * 1e3);
    }
    warm.sort_by(|a, b| a.partial_cmp(b).unwrap());
    feed.sort_by(|a, b| a.partial_cmp(b).unwrap());
    // footprint of one held warm layer + fed index
    let fp1 = footprint_kb();
    let l = jars::analyze_classpath_with(cp, cache, cfg);
    let mut d = DefsIndex::new();
    let fp2 = footprint_kb();
    l.feed(&mut d);
    let fp3 = footprint_kb();
    let elems = st.defs + st.classes;
    println!("jars {} files {} var/ns defs {} classes {} cache {:.2} MB", st.jars, st.files, st.defs, st.classes, st.cache_bytes as f64 / 1e6);
    println!("cold (analyze + write): {cold:.1} ms; feed {feed_cold:.2} ms");
    println!("warm load: min {:.2} median {:.2} max {:.2} ms; feed median {:.2} ms", warm[0], warm[10], warm[19], feed[10]);
    println!("footprint KB: base {fp0}, after cold+20 warm {fp1}; layer held {} KB (+{} for layer, +{} for feed); {:.1} B/element (layer+feed)", fp3 as i64 - fp1 as i64, fp2 as i64 - fp1 as i64, fp3 as i64 - fp2 as i64, (fp3 as i64 - fp1 as i64) as f64 * 1024.0 / elems as f64);
    drop((l, d));
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let Some(root) = a.get(1).cloned().or_else(|| std::env::var("MOVA_NX_LSP_DIR").ok().map(|d| format!("{d}/lib"))) else {
        println!("skipped: pass <root> or set MOVA_NX_LSP_DIR");
        return;
    };
    let root = PathBuf::from(root);
    let opt = |k: &str| a.iter().position(|x| x == k).and_then(|i| a.get(i + 1)).cloned();
    let cache = opt("--cache").map(PathBuf::from).unwrap_or_else(|| nx_core::io::cache_root().join("jars"));
    let cp = if let Some(f) = opt("--jars-file") {
        Classpath { jars: std::fs::read_to_string(f).unwrap().lines().filter(|l| !l.is_empty()).map(String::from).collect(), dirs: vec![] }
    } else {
        classpath::resolve(&root, &project::Settings::load(&root)).classpath
    };
    if a.iter().any(|x| x == "--fp") {
        // fresh process, warm cache: footprint of the loaded layer, then after feeding a DefsIndex
        let cfg = nx_core::analyzer::Config::new();
        let _ = std::hint::black_box(nx_core::analyzer::DefsIndex::new());
        let fp0 = footprint_kb();
        let l = jars::analyze_classpath_with(&cp, &cache, &cfg);
        let fp1 = footprint_kb();
        let mut d = DefsIndex::new();
        l.feed(&mut d);
        let fp2 = footprint_kb();
        let n = l.stats.defs;
        println!("warm={} KB base {fp0}; layer +{} KB; feed (DefsIndex jar layer + interned names) +{} KB; per def {:.0} B", l.stats.warm, fp1 as i64 - fp0 as i64, fp2 as i64 - fp1 as i64, (fp2 as i64 - fp0 as i64) as f64 * 1024.0 / n as f64);
        return;
    }
    if a.iter().any(|x| x == "--bench") {
        let cfg = opt("--cfg-root").map(|r| nx_core::analyzer::Config::load(Path::new(&r))).unwrap_or_else(nx_core::analyzer::Config::new);
        return bench(&cp, &cache, &cfg);
    }
    let t = Instant::now();
    let cfg = opt("--cfg-root").map(|r| nx_core::analyzer::Config::load(Path::new(&r))).unwrap_or_else(nx_core::analyzer::Config::new);
    let layer = jars::analyze_classpath_with(&cp, &cache, &cfg);
    let d1 = t.elapsed();
    println!("analyze_classpath: {:.1} ms  {:?}", d1.as_secs_f64() * 1e3, layer.stats);
    let t = Instant::now();
    let mut defs = DefsIndex::new();
    layer.feed(&mut defs);
    println!("feed DefsIndex: {:.2} ms", t.elapsed().as_secs_f64() * 1e3);
    if let Some(out) = opt("--out") {
        let out = PathBuf::from(out);
        let _ = std::fs::remove_dir_all(&out);
        let mut n = 0;
        for j in &layer.jars {
            let base = Path::new(&j.path).file_name().unwrap().to_string_lossy().to_string();
            for i in 0..j.file_count {
                let (Some(name), Some(fa)) = (j.file_name(i), j.file(i)) else { continue };
                let lang = match FileKind::from_path(name) {
                    Some(FileKind::Clj) => "clj",
                    Some(FileKind::Cljs) => "cljs",
                    _ => "cljc",
                };
                let rel = format!("{base}!/{name}");
                let p = out.join(format!("{rel}.json"));
                std::fs::create_dir_all(p.parent().unwrap()).unwrap();
                std::fs::write(p, emit::to_json(&rel, lang, &fa)).unwrap();
                n += 1;
            }
            let mut by: Vec<(String, Vec<jars::ClassDef>)> = Vec::new();
            for c in j.classes() {
                match by.iter_mut().find(|x| x.0 == c.entry) {
                    Some(x) => x.1.push(c),
                    None => by.push((c.entry.clone(), vec![c])),
                }
            }
            for (entry, cs) in by {
                let rel = format!("{base}!/{entry}");
                let els: Vec<String> = cs
                    .iter()
                    .map(|c| {
                        let fl: Vec<String> = java::flag_names(c.flags).map(|x| js(x)).collect();
                        format!("{{\"class\":{},\"uri\":{},\"flags\":[{}]}}", js(&c.class), js(&j.entry_uri(&entry)), fl.join(","))
                    })
                    .collect();
                let body = format!("{{\"file\":{},\"lang\":\"other\",\"analysis\":{{\"java-class-definitions\":[{}]}},\"findings\":[]}}", js(&rel), els.join(","));
                let p = out.join(format!("{rel}.json"));
                std::fs::create_dir_all(p.parent().unwrap()).unwrap();
                std::fs::write(p, body).unwrap();
                n += 1;
            }
        }
        println!("wrote {n} files");
    }
}
