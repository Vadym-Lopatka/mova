//! `nx_jars <jars.paths> <out-dir> [--only-golden <dir>] [--bench]`
//! External (dependency) analysis of every jar listed in the paths file; writes one oracle-schema JSON per entry
//! (`<jar-basename>!/<entry>.json`, same layout as the jar goldens). Sources use `Options::external()`,
//! `.class` / `.java` entries yield java-class-definitions.
use nx_core::analyzer::*;
use nx_core::io::jar::Jar;
use std::path::{Path, PathBuf};
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let paths = std::fs::read_to_string(&args[1]).expect("paths file");
    let out_dir = PathBuf::from(&args[2]);
    let mut only: Option<PathBuf> = None;
    let mut bench = false;
    let mut i = 3;
    while i < args.len() {
        match args[i].as_str() {
            "--only-golden" => {
                only = Some(PathBuf::from(&args[i + 1]));
                i += 1;
            }
            "--bench" => bench = true,
            _ => {}
        }
        i += 1;
    }
    let cfg = Config::default();
    let mut defs = DefsIndex::new();
    let mut results: Vec<(String, &'static str, FileAnalysis)> = Vec::new();
    let (mut n, mut bytes) = (0usize, 0usize);
    let mut total = std::time::Duration::ZERO;
    let mut buf = Vec::new();
    for line in paths.lines().filter(|l| l.ends_with(".jar")) {
        let Ok(mut jar) = Jar::open(line) else { continue };
        let base = Path::new(line).file_name().unwrap().to_string_lossy().to_string();
        let names: Vec<String> = jar.names(&[nx_core::io::jar::EntryKind::Source, nx_core::io::jar::EntryKind::Class]).into_iter().map(|x| x.0).collect();
        for name in names {
            let file = format!("{}!/{}", base, name);
            if let Some(g) = &only {
                if !g.join(format!("{}.json", file)).exists() {
                    continue;
                }
            }
            if jar.read(&name, &mut buf).is_err() {
                continue;
            }
            let t0 = Instant::now();
            let (fa, lang) = if name.ends_with(".class") {
                let mut fa = FileAnalysis::default();
                fa.java_class_defs.extend(java::class_def_from_class(&buf));
                (fa, "other")
            } else {
                let kind = FileKind::from_path(&name).unwrap();
                let src = String::from_utf8_lossy(&buf).to_string();
                bytes += src.len();
                let mut o = Options::external();
                // kondo's filename is `<jar>:<entry>`: only nested project.clj files have file-name `project.clj`
                if name.contains('/') && name.rsplit('/').next() == Some("project.clj") {
                    o.init_ns = nx_core::intern("leiningen.core.project");
                }
                let fa = analyze_file_opts(&src, kind, &cfg, &defs, o);
                (fa, match kind { FileKind::Clj => "clj", FileKind::Cljs => "cljs", FileKind::Cljc => "cljc", _ => "other" })
            };
            total += t0.elapsed();
            n += 1;
            results.push((file, lang, fa));
        }
    }
    // second pass: cross-file protocol resolution (`in-ns` continuations)
    for (file, _, fa) in &results {
        defs.add_file_at(fa, Some(file.split_once("!/").map_or(file.as_str(), |x| x.1)));
    }
    for (file, lang, fa) in results.iter_mut() {
        finish_extras(fa, &defs);
        if !bench {
            let j = emit::to_json(file, lang, fa);
            let p = out_dir.join(format!("{}.json", file));
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, j).unwrap();
        }
    }
    // `.java` entries are not classified by io::jar: read them with the zip crate directly
    for line in paths.lines().filter(|l| l.ends_with(".jar")) {
        let Ok(f) = std::fs::File::open(line) else { continue };
        let Ok(mut ar) = zip::ZipArchive::new(f) else { continue };
        let base = Path::new(line).file_name().unwrap().to_string_lossy().to_string();
        for i in 0..ar.len() {
            let Ok(mut e) = ar.by_index(i) else { continue };
            let name = e.name().to_string();
            if !name.ends_with(".java") {
                continue;
            }
            let file = format!("{}!/{}", base, name);
            if let Some(g) = &only {
                if !g.join(format!("{}.json", file)).exists() {
                    continue;
                }
            }
            buf.clear();
            use std::io::Read;
            let _ = e.read_to_end(&mut buf);
            let mut fa = FileAnalysis::default();
            fa.java_class_defs = java::class_defs_from_source(&String::from_utf8_lossy(&buf));
            n += 1;
            if !bench {
                let j = emit::to_json(&file, "other", &fa);
                let p = out_dir.join(format!("{}.json", file));
                std::fs::create_dir_all(p.parent().unwrap()).unwrap();
                std::fs::write(p, j).unwrap();
            }
        }
    }
    eprintln!("{} entries, {} source bytes, analysis {:.1} ms = {:.1} ns/byte", n, bytes, total.as_secs_f64() * 1e3, total.as_secs_f64() * 1e9 / bytes.max(1) as f64);
}
