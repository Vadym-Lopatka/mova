use nx_core::io::{cache::Cache, classpath, jar, project};
use std::path::{Path, PathBuf};

/// clojure-lsp-nx checkout (`MOVA_NX_LSP_DIR`) and e2test checkout (`MOVA_NX_E2TEST_DIR`): tests that need them skip when unset.
fn lsp() -> Option<String> {
    let v = std::env::var("MOVA_NX_LSP_DIR").ok();
    if v.is_none() {
        println!("skipped: set MOVA_NX_LSP_DIR");
    }
    v
}
fn tmp(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join("nx/io/test").join(name);
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn kinds(root: &str) -> Vec<project::Kind> {
    let r = Path::new(root);
    project::discover(r, &project::Settings::load(r)).iter().map(|s| s.kind).collect()
}

#[test]
fn discover_projects() {
    use project::Kind::*;
    let Some(lsp_dir) = lsp() else { return };
    let Ok(e2test) = std::env::var("MOVA_NX_E2TEST_DIR") else {
        println!("skipped: set MOVA_NX_E2TEST_DIR");
        return;
    };
    assert_eq!(kinds(&format!("{lsp_dir}/lib")), vec![Deps]);
    assert_eq!(kinds(&format!("{lsp_dir}/mova/lsp-e2e/projects/bb")), vec![Bb]);
    assert_eq!(kinds(&format!("{lsp_dir}/mova/lsp-e2e/projects/clj")), vec![Deps]);
    assert_eq!(kinds(&format!("{lsp_dir}/mova/lsp-e2e/projects/cljs")), vec![Deps]);
    let k = kinds(&e2test);
    assert!(k.contains(&Deps) && k.contains(&Bb), "{k:?}");
}

#[test]
fn default_commands() {
    let s = project::default_specs(&["dev".into(), "test".into()]);
    let d = s.iter().find(|x| x.file == "deps.edn").unwrap();
    assert_eq!(d.cmd, ["clojure", "-A:dev:test", "-Spath"]);
    let l = s.iter().find(|x| x.file == "project.clj").unwrap();
    assert_eq!(l.cmd, ["lein", "with-profile", "+dev,+test", "classpath"]);
    let q = s.iter().find(|x| x.file == "squint.edn").unwrap();
    assert_eq!(q.cmd, ["clojure", "-Sdeps", "squint.edn", "-Spath", "-A:dev:test"]);
}

#[test]
fn static_source_paths() {
    let Some(lsp_dir) = lsp() else { return };
    let r = PathBuf::from(format!("{lsp_dir}/mova/lsp-e2e/projects/clj"));
    let st = project::Settings::load(&r);
    let specs = project::discover(&r, &st);
    let dirs = project::static_dirs(&r, &specs, &st);
    let sp = project::source_paths(&r, &st, &dirs);
    assert_eq!(sp.len(), 1);
    assert!(sp[0].ends_with("/projects/clj/src"), "{sp:?}");
    // no classpath dirs -> defaults src,test; ignore-regex target.* applied
    let sp = project::source_paths(&r, &st, &["target/classes".into(), "src".into(), "/x/y.jar".into()]);
    assert_eq!(sp.len(), 1);
}

#[test]
fn classpath_cache_roundtrip() {
    let t = tmp("cp");
    std::fs::write(t.join("deps.edn"), "{:paths [\"src\"]}").unwrap();
    let st = project::Settings::default();
    let k1 = classpath::key(&t, &st).unwrap();
    std::fs::write(t.join("deps.edn"), "{:paths [\"src\" \"x\"]}").unwrap();
    assert_ne!(k1, classpath::key(&t, &st).unwrap());
}

fn make_jar(path: &Path) {
    use std::io::Write;
    let mut z = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
    let o = zip::write::FileOptions::default();
    for (n, b) in [("a/b.clj", "(ns a.b)"), ("a/C.class", "x"), ("clj-kondo.exports/x/y/config.edn", "{}"), ("README.md", "hi"), ("a/d.cljs", "(ns a.d)")] {
        z.start_file(n, o).unwrap();
        z.write_all(b.as_bytes()).unwrap();
    }
    z.finish().unwrap();
}

#[test]
fn jar_read() {
    let t = tmp("jar");
    let p = t.join("t.jar");
    make_jar(&p);
    let mut j = jar::Jar::open(&p).unwrap();
    assert_eq!(j.source_names(), ["a/b.clj", "a/d.cljs"]);
    assert_eq!(j.class_names(), ["a/C.class"]);
    let mut buf = Vec::new();
    let mut seen = vec![];
    j.for_each_source(&mut buf, |n, k, b| seen.push((n.to_string(), k, b.len()))).unwrap();
    assert_eq!(seen.len(), 3);
    assert_eq!(jar::stat_key(&p).unwrap().len(), 32);
    assert_eq!(jar::content_hash(&p).unwrap().len(), 32);
    assert!(jar::Jar::open(t.join("nope.jar")).is_err());
}

#[test]
fn cache_blobs_and_corruption() {
    let t = tmp("cache");
    let p = t.join("x.nxc");
    {
        let mut c = Cache::open(&p, 7);
        assert!(c.get(b"k").is_none());
        c.put(b"k", b"hello").unwrap();
        c.put(b"k2", b"").unwrap();
        c.put_value(b"n", &42u64).unwrap();
        assert_eq!(c.get(b"k"), Some(&b"hello"[..]));
        c.put(b"k", b"world!!").unwrap();
    }
    let c = Cache::open(&p, 7);
    assert_eq!(c.get(b"k"), Some(&b"world!!"[..]));
    assert_eq!(c.get_value::<u64>(b"n"), Some(42));
    assert!(!c.recovered);
    // wrong schema -> empty
    assert!(Cache::open(&p, 8).is_empty());
    // truncate mid-record: valid prefix survives, then writes recover
    let full = std::fs::read(&p).unwrap();
    for cut in [full.len() - 3, full.len() - 20, 17, 5, 0] {
        std::fs::write(&p, &full[..cut]).unwrap();
        let mut c = Cache::open(&p, 7);
        let _ = c.get(b"k");
        c.put(b"z", b"zz").unwrap();
        c.remap();
        assert_eq!(c.get(b"z"), Some(&b"zz"[..]), "cut {cut}");
    }
    // bit flip in the middle
    let mut bad = full.clone();
    let m = bad.len() / 2;
    bad[m] ^= 0xff;
    std::fs::write(&p, &bad).unwrap();
    let c = Cache::open(&p, 7);
    assert!(c.recovered || c.len() <= 3);
    // garbage file
    std::fs::write(&p, b"garbage garbage garbage garbage").unwrap();
    let mut c = Cache::open(&p, 7);
    assert!(c.is_empty());
    c.put(b"a", b"b").unwrap();
    assert_eq!(Cache::open(&p, 7).get(b"a"), Some(&b"b"[..]));
}
