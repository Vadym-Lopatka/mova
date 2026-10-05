//! `bench_analyze <root> [reps]`: analyze all sources under `root` `reps` times in memory (profiling aid).
use nx_core::analyzer::*;
use std::path::{Path, PathBuf};
use std::time::Instant;

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        let name = e.file_name().to_string_lossy().to_string();
        if p.is_dir() {
            if matches!(name.as_str(), ".git" | ".cpcache" | "node_modules" | ".cache" | ".lsp" | "target") {
                continue;
            }
            walk(&p, out);
        } else if FileKind::from_path(&name).is_some() {
            out.push(p);
        }
    }
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let root = PathBuf::from(&a[1]);
    let reps: usize = a.get(2).and_then(|x| x.parse().ok()).unwrap_or(20);
    let mut files = Vec::new();
    walk(&root, &mut files);
    let srcs: Vec<(FileKind, String)> = files.iter().filter_map(|p| Some((FileKind::from_path(&p.to_string_lossy())?, std::fs::read_to_string(p).ok()?))).collect();
    let total: usize = srcs.iter().map(|s| s.1.len()).sum();
    let cfg = Config::load(&root);
    let defs = DefsIndex::new();
    let mut n = 0usize;
    let t = Instant::now();
    for _ in 0..reps {
        for (k, s) in &srcs {
            let fa = analyze_file(s, *k, &cfg, &defs);
            n += fa.findings.len() + fa.var_usages.len();
        }
    }
    let d = t.elapsed();
    println!("{} files {} bytes x{}: {:.1} ms/rep = {:.2} ns/byte (n={})", srcs.len(), total, reps, d.as_secs_f64() * 1e3 / reps as f64, d.as_secs_f64() * 1e9 / (reps * total) as f64, n);
}
