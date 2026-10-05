use nx_core::engine::{lsp, Engine};
use std::path::PathBuf;

fn drain(e: &Engine, n: usize) -> Vec<nx_core::engine::Done> {
    let mut all = Vec::new();
    while all.len() < n {
        all.extend(e.await_results(64).unwrap());
    }
    all
}

#[test]
fn text_commit_and_stale() {
    let e = Engine::new(2);
    let u = "file:///t/a.clj";
    e.analyze_text(u, 1, "(ns a)\n)".to_string());
    let c = e.commit(&drain(&e, 1));
    assert_eq!(c.changed.len(), 1);
    let ds = lsp::to_diagnostics(&c.snapshot.get(u).unwrap().findings);
    assert_eq!(ds.len(), 1);
    assert_eq!((ds[0].severity, ds[0].code.as_str(), ds[0].source), (1, "syntax", "clj-kondo"));
    // fix in v2
    e.analyze_text(u, 2, "(ns a)\n(def x 1)\n".to_string());
    let c = e.commit(&drain(&e, 1));
    assert!(c.snapshot.get(u).unwrap().findings.is_empty());
    // stale v1 result arriving late is dropped
    e.analyze_text(u, 1, "(".to_string());
    let c = e.commit(&drain(&e, 1));
    assert_eq!((c.changed.len(), c.dropped), (0, 1));
    assert_eq!(c.snapshot.get(u).unwrap().version, 2);
}

#[test]
fn project_pass_skips_open_and_reports_progress() {
    let e = Engine::new(3);
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/proj");
    let (batch, total, sps) = e.analyze_project(&root);
    assert!(total >= 2, "total {total}");
    assert!(!sps.is_empty());
    let done = drain(&e, total);
    let c = e.commit(&done);
    assert!(c.snapshot.file_count >= 2);
    assert_eq!(e.pool.batch_progress(batch), Some((total, total)));
    let bad = c.changed.iter().filter(|c| c.uri.ends_with("bad.clj")).count();
    assert_eq!(bad, 1);
}

#[test]
fn analyze_paths_ordered() {
    let e = Engine::new(2);
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/proj/src");
    let ps = vec![root.join("bad.clj"), root.join("ok.clj"), root.join("missing.clj")];
    let r = e.analyze_paths(ps);
    assert!(!r[0].as_ref().unwrap().findings.is_empty());
    assert!(r[1].as_ref().unwrap().findings.is_empty());
    assert!(r[2].is_none());
}

#[test]
fn watched_files_create_change_delete() {
    // B3: disk changes reported by the client update the store; open docs are skipped; deletes drop the entry
    let root = std::env::temp_dir().join(format!("nx-watch-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("src")).unwrap();
    let root = std::fs::canonicalize(&root).unwrap();
    std::fs::write(root.join("src/a.clj"), "(ns a)\n(defn foo [] 1)\n").unwrap();
    let e = Engine::new(2);
    let mut info = nx_core::engine::scan::discover(&root);
    let files = std::mem::take(&mut info.files);
    let n = files.len();
    e.store.set_project(std::sync::Arc::new(info));
    e.pool.submit_batch(files);
    let c = e.commit(&drain(&e, n));
    let uri = |f: &str| nx_core::engine::scan::path_to_uri(&root.join(f));
    assert!(c.snapshot.get(&uri("src/a.clj")).is_some());
    // created
    std::fs::write(root.join("src/b.clj"), "(ns b (:require [a]))\n(a/foo)\n").unwrap();
    assert_eq!(e.watch_changed(&uri("src/b.clj")), vec![uri("src/b.clj")]);
    let c = e.commit(&drain(&e, 1));
    assert_eq!(c.changed.len(), 1);
    assert!(c.changed[0].disk);
    let s = c.snapshot;
    assert!(s.get(&uri("src/b.clj")).is_some());
    assert_eq!(s.reference_uris(&uri("src/a.clj")), vec![uri("src/b.clj")]);
    // unknown file types are ignored
    std::fs::write(root.join("src/notes.txt"), "x").unwrap();
    assert!(e.watch_changed(&uri("src/notes.txt")).is_empty());
    // open in the editor: skipped
    e.analyze_text(&uri("src/a.clj"), 1, "(ns a)\n".to_string());
    e.commit(&drain(&e, 1));
    assert!(e.watch_changed(&uri("src/a.clj")).is_empty());
    // deleted
    std::fs::remove_file(root.join("src/b.clj")).unwrap();
    let (gone, refs) = e.watch_deleted(&uri("src/b.clj"));
    assert_eq!(gone, vec![uri("src/b.clj")]);
    assert_eq!(refs, vec![uri("src/a.clj")]);
    assert!(e.store.snapshot().get(&uri("src/b.clj")).is_none());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn set_project_is_not_lost_to_concurrent_commits() {
    // B12: set_project / set_opts raced with the writer's commit (read-modify-write of the snapshot)
    let e = Engine::new(2);
    let dir = std::env::temp_dir().join(format!("nx-b12-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("deps.edn"), "{:paths [\"src\"]}").unwrap();
    for round in 0..200 {
        let info = nx_core::engine::scan::discover(&dir);
        let uri = format!("file:///x/a{round}.clj");
        e.analyze_text(&uri, 1, "(ns a)".into());
        e.store.set_project(std::sync::Arc::new(info));
        e.store.set_opts(nx_core::engine::ClientOpts::default());
        let r = e.await_results(64).unwrap();
        e.commit(&r);
        assert!(e.store.snapshot().project.is_some(), "round {round}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
