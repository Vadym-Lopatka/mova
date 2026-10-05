//! `nx_analyze <corpus-root> <out-dir> [--jars-golden-dir <dir>] [--only-golden <dir>] [--threads N] [--bench]`
//! Writes one oracle-schema JSON per source file (same relative paths as the goldens).
use nx_core::analyzer::json::{self, Json};
use nx_core::analyzer::*;
use nx_core::intern;
use std::path::{Path, PathBuf};
use std::time::Instant;

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut es: Vec<_> = rd.flatten().collect();
    es.sort_by_key(|e| e.file_name());
    for e in es {
        let p = e.path();
        let name = e.file_name().to_string_lossy().to_string();
        if p.is_dir() {
            if matches!(name.as_str(), ".git" | ".cpcache" | "node_modules" | ".cache" | ".lsp" | "target") {
                continue;
            }
            walk(&p, out);
        } else if FileKind::from_path(&name).is_some() || name.ends_with(".java") || name.ends_with(".class") {
            out.push(p);
        }
    }
}

fn load_jars(dir: &Path, defs: &mut DefsIndex) -> usize {
    let mut files = Vec::new();
    fn w(d: &Path, o: &mut Vec<PathBuf>) {
        if let Ok(rd) = std::fs::read_dir(d) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    w(&p, o);
                } else if p.extension().map_or(false, |x| x == "json") {
                    o.push(p);
                }
            }
        }
    }
    w(dir, &mut files);
    let mut n = 0;
    for f in files {
        let Ok(txt) = std::fs::read_to_string(&f) else { continue };
        let Some(doc) = json::parse(&txt) else { continue };
        let file = doc.get("file").and_then(|x| x.as_str()).unwrap_or("");
        let ext = file.rsplit('.').next().unwrap_or("");
        let Some(an) = doc.get("analysis") else { continue };
        let src_of = |el: &Json| -> Option<Src> {
            match ext {
                "clj" | "bb" => Some(Src::Clj),
                "cljs" => Some(Src::Cljs),
                "cljc" => Some(if el.get("lang").and_then(|l| l.as_str()) == Some("cljs") { Src::CljcCljs } else { Src::CljcClj }),
                _ => None,
            }
        };
        if let Some(Json::Arr(nsd)) = an.get("namespace-definitions") {
            for e in nsd {
                if let (Some(s), Some(name)) = (src_of(e), e.get("name").and_then(|x| x.as_str())) {
                    defs.add_jar_ns(s, intern(name));
                }
            }
        }
        if let Some(Json::Arr(vds)) = an.get("var-definitions") {
            for e in vds {
                let (Some(s), Some(ns), Some(name)) = (src_of(e), e.get("ns").and_then(|x| x.as_str()), e.get("name").and_then(|x| x.as_str())) else { continue };
                let mut fl = 0u8;
                if e.get("macro").is_some() {
                    fl |= defs::F_MACRO;
                }
                if e.get("private").is_some() {
                    fl |= defs::F_PRIVATE;
                }
                if e.get("fixed-arities").is_some() {
                    fl |= defs::F_FIXED;
                }
                let mut ar = Arities::default();
                if let Some(Json::Arr(a)) = e.get("fixed-arities") {
                    for x in a {
                        if let Some(n) = x.as_f64() {
                            ar.add(n as u32);
                        }
                    }
                }
                let dep = match e.get("deprecated") {
                    Some(Json::Bool(true)) => Val(intern("true")),
                    Some(Json::Str(s)) => Val(intern(&format!("\"{}\"", s))),
                    _ => Val::NONE,
                };
                let info = VarInfo { flags: fl, varargs_min: e.get("varargs-min-arity").and_then(|x| x.as_f64()).map_or(NO_ARITY, |x| x as u16), fixed: ar, deprecated: dep };
                // the file named after the namespace wins over secondary files (`in-ns` continuations)
                let primary = file.ends_with(&format!("{}.{}", ns.replace('.', "/").replace('-', "_"), ext));
                if primary {
                    defs.add_jar_var(s, intern(ns), intern(name), info);
                } else {
                    defs.add_jar_var_weak(s, intern(ns), intern(name), info);
                }
                n += 1;
            }
        }
    }
    n
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: nx_analyze <corpus-root> <out-dir> [--jars-golden-dir d] [--only-golden d] [--threads N] [--bench]");
        std::process::exit(2);
    }
    let root = PathBuf::from(&args[1]);
    let out_dir = PathBuf::from(&args[2]);
    let mut jars: Option<PathBuf> = None;
    let mut jars_file: Option<PathBuf> = None;
    let mut jar_cache: Option<PathBuf> = None;
    let mut only: Option<PathBuf> = None;
    let mut threads = 1usize;
    let mut bench = false;
    let mut external = false;
    let mut i = 3;
    while i < args.len() {
        match args[i].as_str() {
            "--jars-golden-dir" => {
                jars = Some(PathBuf::from(&args[i + 1]));
                i += 1;
            }
            "--jars-file" => {
                jars_file = Some(PathBuf::from(&args[i + 1]));
                i += 1;
            }
            "--jar-cache" => {
                jar_cache = Some(PathBuf::from(&args[i + 1]));
                i += 1;
            }
            "--only-golden" => {
                only = Some(PathBuf::from(&args[i + 1]));
                i += 1;
            }
            "--threads" => {
                threads = args[i + 1].parse().unwrap();
                i += 1;
            }
            "--bench" => bench = true,
            "--external" => external = true,
            _ => {}
        }
        i += 1;
    }
    let mut files = Vec::new();
    walk(&root, &mut files);
    if let Some(g) = &only {
        files.retain(|p| {
            let rel = p.strip_prefix(&root).unwrap();
            g.join(format!("{}.json", rel.to_string_lossy())).exists()
        });
    }
    let (jfiles, files): (Vec<PathBuf>, Vec<PathBuf>) = files.into_iter().partition(|p| FileKind::from_path(&p.to_string_lossy()).is_none());
    let cfg = Config::load(&root);
    let mut defs = DefsIndex::new();
    let t0 = Instant::now();
    if let Some(j) = &jars {
        let n = load_jars(j, &mut defs);
        eprintln!("jar vars: {} ({:.0} ms)", n, t0.elapsed().as_secs_f64() * 1e3);
    }
    if let Some(f) = &jars_file {
        // native jar analysis (replaces the golden loader); one jar path per line
        let cp = nx_core::io::classpath::Classpath { jars: std::fs::read_to_string(f).unwrap().lines().filter(|l| !l.is_empty()).map(String::from).collect(), dirs: vec![] };
        let cache = jar_cache.clone().unwrap_or_else(|| nx_core::io::cache_root().join("jars"));
        let layer = nx_core::jars::analyze_classpath_with(&cp, &cache, &cfg);
        layer.feed(&mut defs);
        eprintln!("native jars: {:?} ({:.0} ms)", layer.stats, t0.elapsed().as_secs_f64() * 1e3);
    }
    let srcs: Vec<(String, String)> = files.iter().map(|p| (p.strip_prefix(&root).unwrap().to_string_lossy().to_string(), std::fs::read_to_string(p).unwrap_or_default())).collect();
    let total: usize = srcs.iter().map(|s| s.1.len()).sum();
    // pass 1
    let t1 = Instant::now();
    let chunk = (srcs.len() + threads - 1) / threads.max(1);
    let defs_ref = &defs;
    let cfg_ref = &cfg;
    let mut results: Vec<FileAnalysis> = Vec::with_capacity(srcs.len());
    std::thread::scope(|sc| {
        let hs: Vec<_> = srcs
            .chunks(chunk.max(1))
            .map(|ch| {
                sc.spawn(move || {
                    ch.iter()
                        .map(|(rel, text)| {
                            let kind = FileKind::from_path(rel).unwrap();
                            if rel.contains("clj-kondo.exports") {
                                // kondo does not analyze exported config sources
                                return FileAnalysis { base_lang: Some(BaseLang::Clj), ..Default::default() };
                            }
                            {
                                let mut o = if external { Options::external() } else { Options::internal() };
                                o.apply_path(rel);
                                nx_core::analyzer::analyze_cst_at(nx_core::reader::parse(text), kind, cfg_ref, defs_ref, o, if external { None } else { Some(rel.as_str()) })
                            }
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        for h in hs {
            results.extend(h.join().unwrap());
        }
    });
    let d1 = t1.elapsed();
    if bench {
        eprintln!("pass1: {} files {} bytes {:.1} ms = {:.1} ns/byte ({} threads)", srcs.len(), total, d1.as_secs_f64() * 1e3, d1.as_secs_f64() * 1e9 / total as f64, threads);
    }
    for ((rel, _), fa) in srcs.iter().zip(results.iter()) {
        defs.add_file_at(fa, Some(rel));
    }
    let t2 = Instant::now();
    for fa in results.iter_mut() {
        finish_usages(fa, &defs);
        finish_extras(fa, &defs);
    }
    if bench {
        eprintln!("pass2: {:.1} ms", t2.elapsed().as_secs_f64() * 1e3);
    }
    if bench && only.is_none() {
        return;
    }
    for ((rel, _), fa) in srcs.iter().zip(results.iter()) {
        let lang = match FileKind::from_path(rel) {
            Some(FileKind::Clj) => "clj",
            Some(FileKind::Cljs) => "cljs",
            Some(FileKind::Cljc) => "cljc",
            _ => "other",
        };
        let j = emit::to_json(rel, lang, fa);
        let p = out_dir.join(format!("{}.json", rel));
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, j).unwrap();
    }
    // java / class files: class definitions only
    for p in &jfiles {
        let rel = p.strip_prefix(&root).unwrap().to_string_lossy().to_string();
        let Ok(bytes) = std::fs::read(p) else { continue };
        let mut fa = FileAnalysis::default();
        if rel.ends_with(".class") {
            fa.java_class_defs.extend(java::class_def_from_class(&bytes));
        } else {
            fa.java_class_defs = java::class_defs_from_source(&String::from_utf8_lossy(&bytes));
        }
        let j = emit::to_json(&rel, "other", &fa);
        let o = out_dir.join(format!("{}.json", rel));
        std::fs::create_dir_all(o.parent().unwrap()).unwrap();
        std::fs::write(o, j).unwrap();
    }
    eprintln!("wrote {} files", srcs.len() + jfiles.len());
}
