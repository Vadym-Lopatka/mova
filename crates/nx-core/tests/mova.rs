//! Mova projects: dialect rules of `.mova` files and the Mova layer (stdlib sources, Rust natives, host natives).
use nx_core::analyzer::*;
use nx_core::engine::{ClientOpts, Engine, Snapshot};
use nx_core::query::{answer, At};
use std::path::{Path, PathBuf};

fn lint(src: &str, mova: bool) -> Vec<String> {
    let cfg = Config::new();
    let mut defs = DefsIndex::new();
    let mut opts = Options::internal();
    opts.mova = mova;
    let mut fa = analyze_file_opts(src, FileKind::Clj, &cfg, &defs, opts);
    defs.add_file(&fa);
    finish_usages(&mut fa, &defs);
    let mut v: Vec<String> = lint::finish::finalize(fa.base_lang, &fa.findings).into_iter().map(|(f, _)| format!("{} {}", f.ty.name(), f.msg)).collect();
    v.sort();
    v
}

#[test]
fn untyped_catch_binds_the_value() {
    let src = "(ns a)\n(defn f [g]\n  (try (g) (catch e (str e)))\n  (try (g) (catch _ nil))\n  (try (g) (catch Exception e (str e)))\n  (try (g) (catch clojure.lang.ExceptionInfo e (str e))))\n";
    assert_eq!(lint(src, true), Vec::<String>::new());
    // the same text as Clojure: `e` is read as a class, `(str e)` as the binding
    let clj = lint(src, false);
    assert!(clj.iter().any(|f| f.starts_with("syntax unsupported binding form")), "{clj:?}");
    // an unused catch binding is still reported
    assert_eq!(lint("(ns a)\n(defn f [g] (try (g) (catch e nil)))\n", true), ["unused-binding unused binding e"]);
}

#[test]
fn forward_reference_is_valid() {
    let src = "(ns a)\n(defn f [] (later 1))\n(defn later [x] x)\n";
    assert_eq!(lint(src, true), Vec::<String>::new());
    assert_eq!(lint(src, false), ["unresolved-symbol Unresolved symbol: later"]);
    // a name defined nowhere stays unresolved, and arity is still checked on a forward call
    assert_eq!(lint("(ns a)\n(defn f [] (nowhere 1) (later))\n(defn later [x] x)\n", true), ["invalid-arity a/later is called with 0 args but expects 1", "unresolved-symbol Unresolved symbol: nowhere"]);
}

#[test]
fn throw_takes_any_value() {
    let src = "(ns a)\n(defn f [] (throw \"boom\"))\n";
    assert_eq!(lint(src, true), Vec::<String>::new());
    assert_eq!(lint(src, false), ["type-mismatch Expected: throwable, received: string."]);
}

#[test]
fn bare_go_loop_is_a_loop() {
    // `go-loop` itself resolves through the Mova layer (see `layer_end_to_end`); here only its loop shape matters
    let v = lint("(ns a)\n(defn f [ch]\n  (go-loop [i 0]\n    (when (< i 3) (recur (inc i)))))\n", true);
    assert!(!v.iter().any(|f| f.contains("recur")), "{v:?}");
}

fn write(p: &Path, s: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, s).unwrap();
}

/// A fake Mova checkout + a project that embeds it, with a source index pointing at the checkout.
fn fixture() -> (PathBuf, PathBuf) {
    let base = std::env::temp_dir().join(format!("nx-mova-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let (mova, proj) = (base.join("mova"), base.join("proj"));
    write(&mova.join("core/core.mova"), ";; core\n(defn update-vals\n  \"Applies f to every value.\"\n  [m f]\n  (reduce-kv (fn [a k v] (assoc a k (f v))) {} m))\n\n(defn- hidden [] 1)\n");
    write(&mova.join("core/async.mova"), "(defmacro go-loop [bindings & body]\n  `(loop ~bindings ~@body))\n");
    write(&mova.join("src/builtins/sys.rs"), "fn register(i: &mut Interp) {\n    reg(i, \"time-ms\", ArityHint::Exact(0), time_ms);\n    reg(\n        i,\n        \"chan\",\n        ArityHint::Range(0, 1), chan);\n    reg_fs(i, \"read\", read);\n}\n");
    write(&proj.join("native/host/Cargo.toml"), "[package]\nname = \"host\"\n\n[dependencies]\nmova = { path = \"../../../mova\" }\n");
    write(&proj.join("native/host/src/lib.rs"), "pub fn register(e: &mut Engine) {\n    e.register_fn_with_arity(\n        \"pg-now\",\n        Arity::Exact(0), now);\n}\n");
    write(&proj.join("src/app/util.mova"), "(ns app.util)\n\n(defn double [x] (* 2 x))\n");
    write(
        &proj.join("src/app/main.mova"),
        "(ns app.main\n  (:require [app.util :as u]\n            [clojure.core.async :as a]))\n\n(defn run [m]\n  (let [t (time-ms)\n        c (a/chan)\n        d (async/chan 1)]\n    (go-loop [i 0] (when (< i 2) (recur (inc i))))\n    (try (pg-now) (catch e (helper e)))\n    [t c d (mova.fs/read \"x\") (update-vals m u/double) (missing 1)]))\n\n(defn helper [e] (throw (str e)))\n",
    );
    // loaded at run time: qualified calls without a `:require`; `sleep-ms` is a core var the index above does not list
    write(&proj.join("src/app/dyn.mova"), "(ns app.dyn)\n\n(defn f [] [(app.util/double 2) (nowhere.ns/x 1) (sleep-ms 5)])\n");
    write(&proj.join("src/app/dynclj.clj"), "(ns app.dynclj)\n\n(defn f [] (app.util/double 2))\n");
    write(&proj.join("lib-mova/app/extra.mova"), "(ns app.extra\n  (:require [app.main :as main]))\n\n(defn go! [] (main/run {}))\n");
    let ix = format!(
        r#"{{"v":2,"root":"{}","namespaces":[{{"ns":"clojure.core","file":"core/core.mova"}},{{"ns":"clojure.core","file":"core/async.mova"}}],
"natives":[{{"ns":"clojure.core","name":"time-ms","file":"src/builtins/sys.rs","line":2}},{{"ns":"clojure.core","name":"chan","file":"src/builtins/sys.rs","line":3}},
{{"ns":"mova.fs","name":"read","file":"src/builtins/sys.rs","line":7}},{{"ns":"Math","name":"sqrt","file":"src/builtins/sys.rs","line":1}}],
"aliases":[{{"ns":"clojure.core.async","name":"chan","to_ns":"clojure.core","to":"chan"}},{{"ns":"clojure.core.async","name":"go-loop","to_ns":"clojure.core","to":"go-loop"}}],
"default_aliases":[{{"alias":"async","ns":"clojure.core.async"}}]}}"#,
        mova.display()
    );
    write(&base.join("index.json"), &ix);
    std::env::set_var("MOVA_SOURCE_INDEX", base.join("index.json"));
    std::env::set_var("XDG_CACHE_HOME", base.join("xdg"));
    (std::fs::canonicalize(&mova).unwrap(), std::fs::canonicalize(&proj).unwrap())
}

fn settle(e: &Engine, total: usize) {
    let mut done = 0;
    while done < total {
        let r = e.await_results(64).unwrap();
        done += r.len();
        e.commit(&r);
    }
}

fn q(s: &Snapshot, m: &str, path: &Path, line: u32, ch: u32) -> String {
    let uri = nx_core::engine::scan::path_to_uri(path);
    answer(s, m, At { uri: &uri, line, ch }, true).unwrap()
}

/// `uri-suffix:line` (1-based) of a definition answer.
fn loc(ans: &str, root: &Path) -> String {
    let uri = ans.split("\"uri\":\"").nth(1).and_then(|s| s.split('"').next()).unwrap_or("");
    let line: u32 = ans.split("\"start\":{").nth(1).and_then(|s| s.split("\"line\":").nth(1)).and_then(|s| s.split(|c: char| !c.is_ascii_digit()).next()).and_then(|s| s.parse().ok()).unwrap_or(9999);
    let p = uri.strip_prefix("file://").unwrap_or(uri);
    format!("{}:{}", p.strip_prefix(&format!("{}/", root.parent().unwrap().display())).unwrap_or(p), line + 1)
}

#[test]
fn layer_end_to_end() {
    let (mova, proj) = fixture();
    let e = Engine::new(2);
    e.set_client_opts(ClientOpts { hover_markdown: true, ..ClientOpts::default() });
    let mut info = nx_core::engine::scan::discover(&proj);
    assert!(info.mova);
    // every top-level dir with `.mova` files is a source path
    let sps: Vec<String> = info.source_paths.iter().map(|s| s.rsplit('/').next().unwrap().to_string()).collect();
    assert_eq!(sps, ["src", "test", "lib-mova"], "{:?}", info.source_paths);
    let files = std::mem::take(&mut info.files);
    let n = files.len();
    assert_eq!(n, 5);
    e.store.ctx().cfg.store(std::sync::Arc::new(Config::load(&proj)));
    e.store.ctx().source_paths.store(std::sync::Arc::new(info.source_paths.iter().map(PathBuf::from).collect()));
    e.store.set_project(std::sync::Arc::new(info));
    e.load_classpath_jars(&proj);
    e.pool.submit_batch(files);
    settle(&e, n);
    let s = e.store.snapshot();
    let main = proj.join("src/app/main.mova");
    // diagnostics: only the name defined nowhere
    let uri = nx_core::engine::scan::path_to_uri(&main);
    let msgs: Vec<String> = nx_core::query::diag::diagnostics(&s, &uri).into_iter().map(|d| d.message).collect();
    assert_eq!(msgs, ["Unresolved symbol: missing"], "{msgs:?}");
    // `.mova`: a namespace defined by a project file needs no `:require`, one defined nowhere is reported; `.clj` is unchanged
    let diag = |p: &str| -> Vec<String> { nx_core::query::diag::diagnostics(&s, &nx_core::engine::scan::path_to_uri(&proj.join(p))).into_iter().map(|d| d.message).filter(|m| !m.starts_with("Unused public var")).collect() };
    assert_eq!(diag("src/app/dyn.mova"), ["Unresolved namespace nowhere.ns. Are you missing a require?"]);
    assert_eq!(diag("src/app/dynclj.clj"), ["Unresolved namespace app.util. Are you missing a require?"]);
    // definitions: native, native through a required alias, through the default alias, a native namespace with no
    // require, host native, stdlib source, forward reference, a file of another module dir
    let def = |line, ch| loc(&q(&s, "definition", &main, line, ch), &mova);
    assert_eq!(def(5, 12), "mova/src/builtins/sys.rs:2"); // time-ms
    assert_eq!(def(6, 14), "mova/src/builtins/sys.rs:3"); // a/chan
    assert_eq!(def(7, 18), "mova/src/builtins/sys.rs:3"); // async/chan
    assert_eq!(def(10, 22), "mova/src/builtins/sys.rs:7"); // mova.fs/read
    assert_eq!(def(9, 12), "proj/native/host/src/lib.rs:2"); // pg-now
    assert_eq!(def(10, 33), "mova/core/core.mova:2"); // update-vals
    assert_eq!(def(8, 6), "mova/core/async.mova:1"); // go-loop
    assert_eq!(def(9, 30), "proj/src/app/main.mova:13"); // helper, defined below its use
    let extra = proj.join("lib-mova/app/extra.mova");
    assert_eq!(loc(&q(&s, "definition", &extra, 3, 16), &mova), "proj/src/app/main.mova:5"); // main/run
    // hover: where the native is registered and its registration source
    let h = q(&s, "hover", &main, 5, 12);
    assert!(h.contains("Mova native (Rust): src/builtins/sys.rs:2") && h.contains("reg(i, \\\"time-ms\\\", ArityHint::Exact(0), time_ms);"), "{h}");
    let h = q(&s, "hover", &main, 9, 12);
    assert!(h.contains("Mova native (Rust): native/host/src/lib.rs:2") && h.contains("e.register_fn_with_arity("), "{h}");
    let h = q(&s, "hover", &main, 10, 33);
    assert!(h.contains("Applies f to every value.") && h.contains("[m f]") && !h.contains("Mova native"), "{h}");
    // completion (open document): natives, host natives, stdlib vars, native namespaces, default aliases
    let text = "(ns app.c)\n(defn f []\n  [(time-) (pg-) (update-v) (mova.fs/r) (async/c) (go-)])\n";
    let cu = proj.join("src/app/c.mova");
    e.analyze_text(&nx_core::engine::scan::path_to_uri(&cu), 1, text.to_string());
    settle(&e, 1);
    let s = e.store.snapshot();
    let labels = |ch| {
        let c = q(&s, "completion", &cu, 2, ch);
        let mut v: Vec<String> = c.split("\"label\":\"").skip(1).map(|x| x.split('"').next().unwrap().to_string()).collect();
        v.sort();
        v
    };
    assert_eq!(labels(9), ["time-ms"]);
    assert_eq!(labels(15), ["pg-now"]);
    assert_eq!(labels(26), ["update-vals"]);
    assert_eq!(labels(38), ["mova.fs/read"]);
    assert_eq!(labels(48), ["async/chan"]);
    assert_eq!(labels(54), ["go-loop"]);
    // references reach the other module dir
    let r = q(&s, "references", &main, 4, 7); // run
    assert!(r.contains("lib-mova/app/extra.mova"), "{r}");
    let _ = std::fs::remove_dir_all(proj.parent().unwrap());
}

#[test]
fn core_vars_table_is_a_fallback() {
    // the generated table lists vars the Clojure tables lack, and skips the ones a layer file defines
    let none = std::collections::HashSet::new();
    let all = nx_core::mova::missing_core_names(&none);
    for n in ["time-ms", "sleep-ms", "chan?", "sliding-buffer", "sqrt", "codepoint-str"] {
        assert!(all.iter().any(|x| x == n), "{n}");
    }
    assert!(!all.iter().any(|x| x == "map" || x == "clojure.lang.Var" || x == "Exception"));
    let defined: std::collections::HashSet<_> = [nx_core::intern::intern("sleep-ms")].into_iter().collect();
    assert!(!nx_core::mova::missing_core_names(&defined).iter().any(|x| x == "sleep-ms"));
}
