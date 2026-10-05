//! Store + query integration: two-pass project analysis, incremental re-analysis of one file, JVM-shaped answers.
use nx_core::engine::{ClientOpts, Engine, Snapshot};
use nx_core::query::{answer, At};

fn tmp_project() -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("nx-feat-{}-{:?}", std::process::id(), std::thread::current().id()).replace(['(', ')', ' '], ""));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("src/app")).unwrap();
    std::fs::write(d.join("deps.edn"), "{:paths [\"src\"]}").unwrap();
    std::fs::write(d.join("src/app/util.clj"), "(ns app.util)\n\n(defn greet [name]\n  (str \"hi \" name))\n\n(defn unused [] 1)\n").unwrap();
    std::fs::write(d.join("src/app/core.clj"), "(ns app.core\n  (:require [app.util :as u]))\n\n(defn run [x]\n  (u/greet x))\n").unwrap();
    d
}

fn settle(e: &Engine, total: usize) {
    let mut done = 0;
    while done < total {
        let r = e.await_results(64).unwrap();
        done += r.len();
        e.commit(&r);
    }
}

fn q(s: &Snapshot, m: &str, path: &std::path::Path, line: u32, ch: u32) -> String {
    let uri = nx_core::engine::scan::path_to_uri(path);
    answer(s, m, At { uri: &uri, line, ch }, true).unwrap()
}

#[test]
fn two_pass_definition_references_and_incremental_update() {
    let root = tmp_project();
    let e = Engine::new(2);
    e.set_client_opts(ClientOpts::default());
    // no classpath resolution here (no clojure CLI needed): analyze the files directly
    let (_b, total, _) = {
        let mut info = nx_core::engine::scan::discover(&root);
        let files = std::mem::take(&mut info.files);
        let n = files.len();
        e.store.set_project(std::sync::Arc::new(info));
        let b = e.pool.submit_batch(files);
        (b, n, ())
    };
    settle(&e, total);
    let core = root.join("src/app/core.clj");
    let util = root.join("src/app/util.clj");
    let s = e.store.snapshot();
    // definition of u/greet (line 4, col 4) -> util.clj line 2 (0-based)
    let d = q(&s, "definition", &core, 4, 5);
    assert!(d.contains("util.clj") && d.contains("\"line\":2"), "{d}");
    // references to greet from its definition: the usage in core + the definition
    let r = q(&s, "references", &util, 2, 7);
    assert!(r.contains("core.clj") && r.contains("util.clj"), "{r}");
    // usage target info was filled by the second pass: arity table present on the usage
    let f = s.get(&nx_core::engine::scan::path_to_uri(&core)).unwrap();
    let fa = f.fa().unwrap();
    assert!(fa.var_usages.iter().any(|u| u.name.as_str() == "greet" && u.to.as_str() == "app.util" && u.has_fixed));
    // hover plaintext first item is the signature
    let h = q(&s, "hover", &core, 4, 5);
    assert!(h.contains("(app.util/greet [name])") || h.contains("app.util/greet"), "{h}");

    // incremental: rename greet -> hello in util (open doc change); core's usage now points nowhere
    let cutil = nx_core::engine::scan::path_to_uri(&std::fs::canonicalize(&util).unwrap());
    e.analyze_text(&cutil, 1, "(ns app.util)\n\n(defn hello [name]\n  (str \"hi \" name))\n".to_string());
    let r = e.await_results(8).unwrap();
    e.commit(&r);
    let s2 = e.store.snapshot();
    assert!(s2.version > s.version);
    let d2 = q(&s2, "definition", &core, 4, 5);
    // alias exists but the var is gone: clojure-lsp falls back to the namespace definition (line 0)
    assert!(d2.contains("util.clj") && d2.contains("\"line\":0"), "{d2}");
    // and the old snapshot is untouched (readers are lock-free on immutable data)
    assert!(q(&s, "definition", &core, 4, 5).contains("util.clj"));
    // completion through the alias sees the new name
    let ccore = nx_core::engine::scan::path_to_uri(&std::fs::canonicalize(&core).unwrap());
    e.analyze_text(&ccore, 1, "(ns app.core\n  (:require [app.util :as u]))\n\n(defn run [x]\n  (u/he x))\n".to_string());
    let r = e.await_results(8).unwrap();
    e.commit(&r);
    let s3 = e.store.snapshot();
    let c = q(&s3, "completion", &core, 4, 7);
    assert!(c.contains("u/hello"), "{c}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn unused_public_var_depends_on_other_files() {
    let root = tmp_project();
    let e = Engine::new(2);
    let mut info = nx_core::engine::scan::discover(&root);
    let files = std::mem::take(&mut info.files);
    let n = files.len();
    e.store.set_project(std::sync::Arc::new(info));
    e.pool.submit_batch(files);
    settle(&e, n);
    let s = e.store.snapshot();
    let util = nx_core::engine::scan::path_to_uri(&root.join("src/app/util.clj"));
    let ds = nx_core::query::diag::diagnostics(&s, &util);
    let msgs: Vec<&str> = ds.iter().map(|d| d.message.as_str()).collect();
    assert_eq!(msgs, ["Unused public var 'app.util/unused'"], "{msgs:?}");
    assert_eq!((ds[0].severity, ds[0].source, ds[0].tags.clone()), (3, "clojure-lsp", vec![1]));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn cursor_ties_follow_bucket_order() {
    use nx_core::engine::index::{hash_rank, B};
    // JVM PersistentHashMap order of the >8-bucket analysis map (verified against a Clojure REPL)
    let r = hash_rank();
    assert!(r[B::NsUsage as usize] < r[B::VarDef as usize]);
    assert!(r[B::VarDef as usize] < r[B::VarUsage as usize]);
    assert!(r[B::Local as usize] < r[B::LocalUsage as usize]);
}

#[test]
fn unused_keyword_definition_diagnostic() {
    let root = tmp_project();
    std::fs::write(root.join("src/app/specs.clj"), "(ns app.specs\n  (:require [clojure.spec.alpha :as s]))\n\n(s/def ::used int?)\n(s/def ::orphan string?)\n").unwrap();
    std::fs::write(root.join("src/app/core.clj"), "(ns app.core\n  (:require [app.specs]))\n\n(defn run [] :app.specs/used)\n").unwrap();
    let e = Engine::new(2);
    let mut info = nx_core::engine::scan::discover(&root);
    let files = std::mem::take(&mut info.files);
    let n = files.len();
    e.store.set_project(std::sync::Arc::new(info));
    e.pool.submit_batch(files);
    settle(&e, n);
    let s = e.store.snapshot();
    let uri = nx_core::engine::scan::path_to_uri(&root.join("src/app/specs.clj"));
    let msgs: Vec<String> = nx_core::query::diag::diagnostics(&s, &uri).into_iter().map(|d| d.message).collect();
    assert!(msgs.contains(&"Unused public keyword ':app.specs/orphan'".to_string()), "{msgs:?}");
    assert!(!msgs.iter().any(|m| m.contains("used'")), "{msgs:?}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn kondo_linter_findings_are_diagnostics() {
    let root = tmp_project();
    std::fs::write(root.join("src/app/lint.clj"), "(ns app.lint)\n\n(defn f [x]\n  (let [y 1]\n    x))\n(defn g [] (undefined-sym 1))\n").unwrap();
    let e = Engine::new(2);
    let mut info = nx_core::engine::scan::discover(&root);
    let files = std::mem::take(&mut info.files);
    let n = files.len();
    e.store.set_project(std::sync::Arc::new(info));
    e.pool.submit_batch(files);
    settle(&e, n);
    let s = e.store.snapshot();
    let uri = nx_core::engine::scan::path_to_uri(&root.join("src/app/lint.clj"));
    let ds = nx_core::query::diag::diagnostics(&s, &uri);
    let codes: Vec<(&str, u32, u32)> = ds.iter().map(|d| (d.code.as_str(), d.line, d.character)).collect();
    assert!(codes.contains(&("unused-binding", 3, 8)), "{codes:?}");
    assert!(codes.iter().any(|c| c.0 == "unresolved-symbol" && c.1 == 5), "{codes:?}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn project_kondo_lint_as_and_lsp_excludes_apply() {
    // B4: `.clj-kondo/config.edn` :lint-as (macro -> deftest) and `.lsp/config.edn` unused-public-var settings
    let root = tmp_project();
    std::fs::create_dir_all(root.join(".clj-kondo")).unwrap();
    std::fs::create_dir_all(root.join(".lsp")).unwrap();
    std::fs::write(root.join(".clj-kondo/config.edn"), "{:lint-as {app.mac/defthing clojure.test/deftest}}").unwrap();
    std::fs::write(root.join(".lsp/config.edn"), "{:linters {:clojure-lsp/unused-public-var {:exclude [app.lib/keep-me] :exclude-regex [\"app\\\\.lib/skip-.*\"]}}}").unwrap();
    std::fs::write(root.join("src/app/mac.clj"), "(ns app.mac)\n(defmacro defthing [n & b] nil)\n").unwrap();
    std::fs::write(root.join("src/app/lib.clj"), "(ns app.lib\n  (:require [app.mac :refer [defthing]]))\n\n(defthing a-test 1)\n(defn keep-me [] 1)\n(defn skip-me [] 1)\n(defn drop-me [] 1)\n").unwrap();
    let e = Engine::new(2);
    let mut info = nx_core::engine::scan::discover(&root);
    let files = std::mem::take(&mut info.files);
    let n = files.len();
    e.store.ctx().cfg.store(std::sync::Arc::new(nx_core::analyzer::Config::load(&root)));
    e.store.set_project(std::sync::Arc::new(info));
    e.pool.submit_batch(files);
    settle(&e, n);
    let s = e.store.snapshot();
    let uri = nx_core::engine::scan::path_to_uri(&root.join("src/app/lib.clj"));
    let msgs: Vec<String> = nx_core::query::diag::diagnostics(&s, &uri).into_iter().filter(|d| d.code.contains("unused-public")).map(|d| d.message).collect();
    assert_eq!(msgs, ["Unused public var 'app.lib/drop-me'"], "{msgs:?}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn references_on_special_forms_have_no_line0_locations() {
    // B6: macro-derived `if`/`let` usages (from `when`, `and`, `or`) carry no position and must not be listed
    let root = tmp_project();
    std::fs::write(root.join("src/app/sf.clj"), "(ns app.sf)\n\n(defn f [a b]\n  (if a (and a b) (when b (or a b))))\n").unwrap();
    let e = Engine::new(2);
    let mut info = nx_core::engine::scan::discover(&root);
    let files = std::mem::take(&mut info.files);
    let n = files.len();
    e.store.set_project(std::sync::Arc::new(info));
    e.pool.submit_batch(files);
    settle(&e, n);
    let s = e.store.snapshot();
    let sf = root.join("src/app/sf.clj");
    for (line, col) in [(3, 3), (3, 9), (3, 18), (3, 28)] {
        for inc in [true, false] {
            let uri = nx_core::engine::scan::path_to_uri(&sf);
            let r = answer(&s, "references", At { uri: &uri, line, ch: col }, inc).unwrap();
            assert!(!r.contains("\"line\":0,\"character\":0},\"end\":{\"line\":0,\"character\":0"), "{line}:{col} {r}");
        }
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn completion_in_unparsable_document_returns_all_candidates() {
    // B7: JVM `safe-zloc-of-file` is nil for an unbalanced buffer (Emacs typing at EOF): no cursor symbol -> everything
    let root = tmp_project();
    std::fs::write(root.join("src/app/typing.clj"), "(ns app.typing)\n\n(defn helper-fn [] 1)\n\n(defn probe [] (helper-f").unwrap();
    std::fs::write(root.join("src/app/fine.clj"), "(ns app.fine)\n\n(defn helper-fn [] 1)\n\n(defn probe [] (helper-f))\n").unwrap();
    let e = Engine::new(2);
    let mut info = nx_core::engine::scan::discover(&root);
    let files = std::mem::take(&mut info.files);
    let n = files.len();
    e.store.set_project(std::sync::Arc::new(info));
    e.pool.submit_batch(files);
    settle(&e, n);
    let s = e.store.snapshot();
    let count = |f: &str, line: u32, ch: u32| q(&s, "completion", &root.join(f), line, ch).matches("\"label\"").count();
    let typing = count("src/app/typing.clj", 4, 24);
    let fine = count("src/app/fine.clj", 4, 24);
    assert!(typing > 100, "unparsable buffer: all candidates, got {typing}");
    assert_eq!(fine, 1, "balanced buffer: filtered by the typed prefix");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn kondo_config_dirs_resolve_like_kondo() {
    // B13: `.clj-kondo/<org>/<lib>/config.edn` (auto), `imports/<org>/<lib>`, `:config-paths` (nested), `:auto-load-configs false`
    use nx_core::intern::intern;
    let lint = |c: &nx_core::analyzer::Config, a: &str, n: &str| c.lint_as(intern(a), intern(n)).map(|(x, y)| format!("{}/{}", x.as_str(), y.as_str()));
    let root = tmp_project();
    let k = root.join(".clj-kondo");
    for (d, t) in [("funcool/promesa", "{:lint-as {promesa.core/let clojure.core/let}}"), ("imports/a/b", "{:lint-as {a.b/mac clojure.core/def}}"),
                   ("extra", "{:lint-as {x.y/m clojure.core/defn} :config-paths [\"../nested\"]}"), ("nested", "{:lint-as {n.n/m clojure.core/fn}}"), ("deep/a/b/c", "{:lint-as {deep.d/m clojure.core/fn}}")] {
        std::fs::create_dir_all(k.join(d)).unwrap();
        std::fs::write(k.join(d).join("config.edn"), t).unwrap();
    }
    std::fs::write(k.join("config.edn"), "{:config-paths [\"extra\"] :lint-as {p.q/m clojure.core/let}}").unwrap();
    let c = nx_core::analyzer::Config::load(&root);
    assert_eq!(lint(&c, "promesa.core", "let").as_deref(), Some("clojure.core/let"));
    assert_eq!(lint(&c, "a.b", "mac").as_deref(), Some("clojure.core/def"));
    assert_eq!(lint(&c, "x.y", "m").as_deref(), Some("clojure.core/defn"));
    assert_eq!(lint(&c, "n.n", "m").as_deref(), Some("clojure.core/fn"));
    assert_eq!(lint(&c, "p.q", "m").as_deref(), Some("clojure.core/let"));
    assert_eq!(lint(&c, "deep.d", "m"), None, "depth > 3 is not discovered");
    std::fs::write(k.join("config.edn"), "{:auto-load-configs false}").unwrap();
    let c = nx_core::analyzer::Config::load(&root);
    assert_eq!(lint(&c, "promesa.core", "let"), None);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn unresolved_ns_usage_resolves_like_clojure_lsp() {
    // B18: `(m.other/g 1)` without a require: clojure-lsp turns the unresolved-namespace finding into a var-usage
    let root = tmp_project();
    std::fs::write(root.join("src/app/other.clj"), "(ns app.other)\n(defn g [x] x)\n").unwrap();
    std::fs::write(root.join("src/app/lib.clj"), "(ns app.lib)\n\n(defn f []\n  (app.other/g 1))\n").unwrap();
    let e = Engine::new(2);
    let mut info = nx_core::engine::scan::discover(&root);
    let files = std::mem::take(&mut info.files);
    let n = files.len();
    e.store.set_project(std::sync::Arc::new(info));
    e.pool.submit_batch(files);
    settle(&e, n);
    let s = e.store.snapshot();
    let lib = root.join("src/app/lib.clj");
    let d = q(&s, "definition", &lib, 3, 8);
    assert!(d.contains("other.clj") && d.contains("\"line\":1"), "{d}");
    let p = q(&s, "prepareRename", &lib, 3, 8);
    assert!(p.contains("\"line\":3") && p.contains("\"character\":3") && p.contains("\"character\":14"), "{p}");
    // references from the usage itself find the var (dependents of the def do not include a non-requiring file: as JVM)
    let r = q(&s, "references", &lib, 3, 8);
    assert!(r.contains("other.clj"), "{r}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn symlinked_source_dir_is_canonicalized() {
    // B22: `src -> real`: locations carry the real path (JVM canonical paths); an open doc through the link maps to it
    let root = tmp_project();
    std::fs::remove_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join("real/app")).unwrap();
    std::fs::write(root.join("real/app/util.clj"), "(ns app.util)\n(defn greet [n] n)\n").unwrap();
    std::fs::write(root.join("real/app/core.clj"), "(ns app.core\n  (:require [app.util :as u]))\n(u/greet 1)\n").unwrap();
    std::os::unix::fs::symlink(root.join("real"), root.join("src")).unwrap();
    let e = Engine::new(2);
    let mut info = nx_core::engine::scan::discover(&root);
    let files = std::mem::take(&mut info.files);
    let n = files.len();
    e.store.set_project(std::sync::Arc::new(info));
    e.pool.submit_batch(files);
    settle(&e, n);
    let root_c = root.canonicalize().unwrap();
    let link_uri = nx_core::engine::scan::path_to_uri(&root.join("src/app/core.clj"));
    e.analyze_text(&link_uri, 1, "(ns app.core\n  (:require [app.util :as u]))\n(u/greet 1)\n".into());
    settle(&e, 1);
    let s = e.store.snapshot();
    let d = answer(&s, "definition", At { uri: &link_uri, line: 2, ch: 4 }, true).unwrap();
    assert!(d.contains(&format!("{}/real/app/util.clj", root_c.display())), "{d}");
    let _ = std::fs::remove_dir_all(&root);
}
