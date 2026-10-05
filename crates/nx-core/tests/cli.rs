//! The `nx` binary on small temp projects: exact stdout and exit code.
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

static N: AtomicU32 = AtomicU32::new(0);

const CORE: &str = "(ns app.core)\n\n(defn add [a b] (+ a b))\n";
const FIX_NOW: &str = "(fix them now; look things up with `nx def|refs|doc <sym>`; re-check with `nx check <file>`)\n";
const MAIN: &str = "(ns app.main\n  (:require [app.core :as core]))\n\n(defn run [] (core/add 1 2))\n";

/// A temp project (`deps.edn` + files) with its own cache dir.
struct Proj {
    dir: PathBuf,
    cache: PathBuf,
}

impl Proj {
    fn new(files: &[(&str, &str)]) -> Proj {
        let base = std::env::temp_dir().join(format!("nx-cli-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        let _ = std::fs::remove_dir_all(&base);
        let p = Proj { dir: base.join("p"), cache: base.join("cache") };
        std::fs::create_dir_all(&p.dir).unwrap();
        p.write("deps.edn", "{:paths [\"src\"]}\n");
        for (f, t) in files {
            p.write(f, t);
        }
        p
    }
    fn write(&self, rel: &str, text: &str) {
        let path = self.dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    fn git(&self, args: &[&str]) {
        let ok = Command::new("git").current_dir(&self.dir).args(["-c", "user.name=t", "-c", "user.email=t@t"]).args(args).output().unwrap().status.success();
        assert!(ok, "git {args:?}");
    }
    /// `git init` and commit everything.
    fn commit(&self) {
        self.git(&["init", "-q"]);
        self.git(&["add", "."]);
        self.git(&["commit", "-q", "-m", "init"]);
    }
    /// (stdout, stderr, exit code) of `nx args` run in the project dir.
    fn nx(&self, args: &[&str]) -> (String, String, i32) {
        self.nx_in(&self.dir, args, "")
    }
    fn nx_in(&self, cwd: &Path, args: &[&str], stdin: &str) -> (String, String, i32) {
        let mut c = Command::new(env!("CARGO_BIN_EXE_nx")).current_dir(cwd).args(args).env("XDG_CACHE_HOME", &self.cache).env_remove("MOVA_BIN").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        c.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
        let o = c.wait_with_output().unwrap();
        (String::from_utf8_lossy(&o.stdout).into_owned(), String::from_utf8_lossy(&o.stderr).into_owned(), o.status.code().unwrap())
    }
    fn hook(&self, file: &str) -> (String, String, i32) {
        let json = format!("{{\"tool_name\":\"Edit\",\"tool_input\":{{\"file_path\":\"{}\"}}}}", self.dir.join(file).display());
        self.nx_in(&self.dir, &["hook"], &json)
    }
}

#[test]
fn check_clean_is_silent() {
    let p = Proj::new(&[("src/app/core.clj", CORE), ("src/app/main.clj", MAIN)]);
    assert_eq!(p.nx(&["check"]), (String::new(), String::new(), 0));
    assert_eq!(p.nx(&["check", "src/app/main.clj"]), (String::new(), String::new(), 0));
}

#[test]
fn check_unresolved_symbol_and_arity() {
    let main = "(ns app.main\n  (:require [app.core :as core]))\n\n(defn run [] (core/add 1) (nope 2))\n";
    let p = Proj::new(&[("src/app/core.clj", CORE), ("src/app/main.clj", main)]);
    let want = "src/app/main.clj:4:14 error invalid-arity app.core/add is called with 1 arg but expects 2\n    (defn run [] (core/add 1) (nope 2))\nsrc/app/main.clj:4:28 error unresolved-symbol Unresolved symbol: nope\n    (defn run [] (core/add 1) (nope 2))\n";
    assert_eq!(p.nx(&["check", "src/app/main.clj"]), (want.to_string(), String::new(), 1));
}

#[test]
fn check_errors_come_first_and_are_never_capped() {
    let warns: String = (0..30).map(|i| format!("(defn f{i} [x] (let [y 1] x))\n")).collect();
    let p = Proj::new(&[("src/app/a.clj", &format!("(ns app.a)\n{warns}")), ("src/app/z.clj", "(ns app.z)\n(defn g [] (nope))\n")]);
    let (out, _, code) = p.nx(&["check"]);
    assert_eq!(code, 1);
    assert!(out.starts_with("src/app/z.clj:2:13 error unresolved-symbol Unresolved symbol: nope\n"), "{out}");
    assert!(out.contains("more (--all)"), "{out}");
}

#[test]
fn check_reports_errors_broken_elsewhere() {
    let p = Proj::new(&[("src/app/core.clj", CORE), ("src/app/main.clj", MAIN)]);
    p.write("src/app/core.clj", "(ns app.core)\n\n(defn add [a b c] (+ a b c))\n");
    let want = "broken elsewhere:\nsrc/app/main.clj:4:14 error invalid-arity app.core/add is called with 2 args but expects 3\n    (defn run [] (core/add 1 2))\n";
    assert_eq!(p.nx(&["check", "src/app/core.clj"]), (want.to_string(), String::new(), 1));
}

#[test]
fn check_new_hides_old_findings() {
    let old = "(ns app.core)\n\n(defn add [a b] (+ a b old-bad))\n";
    let p = Proj::new(&[("src/app/core.clj", old)]);
    p.commit();
    assert_eq!(p.nx(&["check", "--new", "src/app/core.clj"]), (String::new(), String::new(), 0));
    p.write("src/app/core.clj", "(ns app.core)\n\n(defn add [a b] (+ a b old-bad))\n\n(defn sub [a b] (- a b new-bad))\n");
    let want = "src/app/core.clj:5:24 error unresolved-symbol Unresolved symbol: new-bad\n    (defn sub [a b] (- a b new-bad))\n";
    assert_eq!(p.nx(&["check", "--new", "src/app/core.clj"]), (want.to_string(), String::new(), 1));
    let both = p.nx(&["check", "src/app/core.clj"]);
    assert_eq!((both.0.matches("unresolved-symbol").count(), both.2), (2, 1));
}

#[test]
fn check_new_reports_errors_broken_elsewhere() {
    let p = Proj::new(&[("src/app/core.clj", CORE), ("src/app/main.clj", MAIN)]);
    p.commit();
    p.write("src/app/core.clj", "(ns app.core)\n\n(defn add [a b c] (let [unused 1] (+ a b c)))\n");
    let broken = "broken elsewhere:\nsrc/app/main.clj:4:14 error invalid-arity app.core/add is called with 2 args but expects 3\n    (defn run [] (core/add 1 2))\n";
    let warn = "src/app/core.clj:3:25 warning unused-binding unused binding unused\n    (defn add [a b c] (let [unused 1] (+ a b c)))\n";
    assert_eq!(p.nx(&["check", "--new", "src/app/core.clj"]), (format!("{broken}{warn}"), String::new(), 1));
    assert_eq!(p.nx(&["check", "--new", "--errors", "src/app/core.clj"]), (broken.to_string(), String::new(), 1));
    let want = format!("nx: 1 new error(s) after this edit\n{broken}{FIX_NOW}");
    assert_eq!(p.hook("src/app/core.clj"), (String::new(), want, 2));
}

#[test]
fn check_new_hides_errors_elsewhere_that_exist_at_head() {
    let p = Proj::new(&[("src/app/core.clj", "(ns app.core)\n\n(defn add [a b c] (+ a b c))\n"), ("src/app/main.clj", MAIN)]);
    p.commit();
    p.write("src/app/core.clj", "(ns app.core)\n\n(defn add [a b c] (+ a b c))\n\n(defn sub [a b] (- a b))\n");
    assert_eq!(p.nx(&["check", "--new", "src/app/core.clj"]), (String::new(), String::new(), 0));
}

#[test]
fn check_syntax_error_shows_only_syntax() {
    let p = Proj::new(&[("src/app/core.clj", "(ns app.core)\n\n(defn add [a b] (+ a b nope)\n")]);
    let (out, _, code) = p.nx(&["check", "src/app/core.clj"]);
    assert_eq!(code, 1);
    let want = "src/app/core.clj:3:1 error syntax Found an opening ( with no matching )\n    (defn add [a b] (+ a b nope)\nsrc/app/core.clj:4:1 error syntax Expected a ) to match ( from line 3\n";
    assert_eq!(out, want);
}

#[test]
fn def_by_qualified_and_bare_name() {
    let p = Proj::new(&[("src/app/core.clj", "(ns app.core)\n\n(defn add\n  \"Adds two numbers.\"\n  [a b]\n  (+ a b))\n\n(defn- hidden [] 1)\n"), ("src/app/main.clj", MAIN)]);
    let want = "src/app/core.clj:3 app.core/add (fn)\n(defn add\n  \"Adds two numbers.\"\n  [a b]\n  (+ a b))\n";
    assert_eq!(p.nx(&["def", "app.core/add"]), (want.to_string(), String::new(), 0));
    assert_eq!(p.nx(&["def", "add"]), (want.to_string(), String::new(), 0));
    assert_eq!(p.nx(&["def", "core/add", "--in", "src/app/main.clj"]), (want.to_string(), String::new(), 0));
    assert!(p.nx(&["def", "hidden"]).0.starts_with("src/app/core.clj:8 app.core/hidden (private fn)\n"));
}

#[test]
fn def_ambiguous_name_lists_candidates() {
    let p = Proj::new(&[("src/app/core.clj", CORE), ("src/app/other.clj", "(ns app.other)\n\n(defn add [x] x)\n")]);
    let want = "src/app/core.clj:3 app.core/add (fn)\nsrc/app/other.clj:3 app.other/add (fn)\n";
    assert_eq!(p.nx(&["def", "add"]), (want.to_string(), String::new(), 1));
}

#[test]
fn def_not_found() {
    let p = Proj::new(&[("src/app/core.clj", CORE)]);
    assert_eq!(p.nx(&["def", "no-such-thing"]), (String::new(), "not found: no-such-thing\n".to_string(), 1));
}

#[test]
fn def_by_file_line_col() {
    let main = "(ns app.main\n  (:require [app.core :as core]))\n\n(println (core/add 1 2))\n";
    let p = Proj::new(&[("src/app/core.clj", CORE), ("src/app/main.clj", main)]);
    let (out, _, code) = p.nx(&["def", "src/app/main.clj:4:17"]);
    assert_eq!(code, 0);
    assert!(out.starts_with("src/app/core.clj:3 app.core/add (fn)\n(defn add"), "{out}");
    assert_eq!(p.nx(&["def", "src/app/main.clj:4"]).0, out);
}

#[test]
fn hook_prints_new_findings_on_stderr() {
    let p = Proj::new(&[("src/app/core.clj", CORE)]);
    p.commit();
    p.write("src/app/core.clj", "(ns app.core)\n\n(defn add [a b] (+ a b nope))\n");
    let want = format!("nx: 1 new error(s) after this edit\nsrc/app/core.clj:3:24 error unresolved-symbol Unresolved symbol: nope\n    (defn add [a b] (+ a b nope))\n{FIX_NOW}");
    assert_eq!(p.hook("src/app/core.clj"), (String::new(), want, 2));
}

#[test]
fn hook_ignores_warnings_and_old_errors() {
    let p = Proj::new(&[("src/app/core.clj", "(ns app.core)\n\n(defn add [a b] (+ a b old-bad))\n")]);
    p.commit();
    assert_eq!(p.hook("src/app/core.clj"), (String::new(), String::new(), 0));
    p.write("src/app/core.clj", "(ns app.core)\n\n(defn add [a b] (+ a b old-bad))\n\n(defn sub [a b] (let [unused 1] (- a b)))\n");
    assert_eq!(p.hook("src/app/core.clj"), (String::new(), String::new(), 0));
}

#[test]
fn hook_stop_checks_changed_and_untracked_files() {
    let p = Proj::new(&[("src/app/core.clj", CORE), ("src/app/main.clj", MAIN)]);
    p.commit();
    let stop = |active: bool| p.nx_in(&p.dir, &["hook"], &format!("{{\"hook_event_name\":\"Stop\",\"cwd\":\"{}\",\"stop_hook_active\":{active}}}", p.dir.display()));
    assert_eq!(stop(false), (String::new(), String::new(), 0));
    p.write("src/app/core.clj", "(ns app.core)\n\n(defn add [a b] (+ a b nope))\n");
    p.write("src/app/new.clj", "(ns app.new)\n\n(defn f [] (missing))\n");
    let want = "nx: 2 new error(s) in files changed in this session. Fix them before you finish.\nsrc/app/core.clj:3:24 error unresolved-symbol Unresolved symbol: nope\n    (defn add [a b] (+ a b nope))\nsrc/app/new.clj:3:13 error unresolved-symbol Unresolved symbol: missing\n    (defn f [] (missing))\n";
    assert_eq!(stop(false), (String::new(), want.to_string(), 2));
    assert_eq!(stop(true), (String::new(), String::new(), 0));
}

#[test]
fn check_errors_hides_warnings() {
    let p = Proj::new(&[("src/app/core.clj", "(ns app.core)\n\n(defn f [x] (let [y 1] x))\n\n(defn g [] (nope))\n")]);
    let (out, _, code) = p.nx(&["check", "--errors"]);
    assert_eq!((out.as_str(), code), ("src/app/core.clj:5:13 error unresolved-symbol Unresolved symbol: nope\n    (defn g [] (nope))\n", 1));
    assert!(p.nx(&["check"]).0.contains("warning"));
}

#[test]
fn hook_clean_and_non_source_are_silent() {
    let p = Proj::new(&[("src/app/core.clj", CORE), ("notes.txt", "hello\n")]);
    p.commit();
    assert_eq!(p.hook("src/app/core.clj"), (String::new(), String::new(), 0));
    assert_eq!(p.hook("notes.txt"), (String::new(), String::new(), 0));
    assert_eq!(p.nx_in(&p.dir, &["hook"], "not json"), (String::new(), String::new(), 0));
}

#[test]
fn usage_errors_exit_2() {
    let p = Proj::new(&[]);
    assert_eq!(p.nx(&["frobnicate"]).2, 2);
    assert_eq!(p.nx(&[]).2, 2);
    assert_eq!(p.nx(&["check", "--bogus"]).2, 2);
    assert_eq!(p.nx(&["check", "missing.clj"]).2, 2);
    assert!(p.nx(&["frobnicate"]).1.contains("usage: nx"));
    for c in ["refs", "doc", "outline", "find", "def"] {
        assert_eq!(p.nx(&[c]).2, 2, "{c}");
    }
}

#[test]
fn mova_project_is_clean() {
    let src = "(ns app.m)\n\n(defn f [g] (try (g) (catch e (str e))))\n\n(defn caller [] (later 1))\n\n(defn later [x] x)\n";
    let p = Proj::new(&[("src/app/m.mova", src)]);
    assert_eq!(p.nx(&["check"]), (String::new(), String::new(), 0));
}

const LIB: &str = "(ns app.lib)\n\n(defmacro twice\n  \"Runs the body twice.\n  Second line.\"\n  [& body]\n  `(do ~@body ~@body))\n\n(defn- helper [x] x)\n\n(defn pub\n  \"Public fn.\"\n  ([a] (helper a))\n  ([a b] [a b]))\n\n(def limit 10)\n";

#[test]
fn root_is_the_nearest_project_marker() {
    let p = Proj::new(&[]);
    p.commit();
    p.write("sub/deps.edn", "{:paths [\"src\"]}\n");
    p.write("sub/src/app/x.clj", "(ns app.x)\n\n(defn only-here [] (nope))\n");
    let sub = p.dir.join("sub");
    let want = "src/app/x.clj:3 app.x/only-here (fn)\n(defn only-here [] (nope))\n";
    assert_eq!(p.nx_in(&sub, &["def", "only-here"], ""), (want.to_string(), String::new(), 0));
    let want = format!("nx: 1 new error(s) after this edit\nsrc/app/x.clj:3:21 error unresolved-symbol Unresolved symbol: nope\n    (defn only-here [] (nope))\n{FIX_NOW}");
    assert_eq!(p.hook("sub/src/app/x.clj"), (String::new(), want, 2));
}

#[test]
fn refs_groups_by_file_with_enclosing_fn() {
    let main = "(ns app.main\n  (:require [app.core :as core]))\n\n(defn run [] (core/add 1 2))\n\n(defn twice [x]\n  (core/add x x))\n\n(println (core/add 0 0))\n";
    let test = "(ns app.core-test\n  (:require [app.core :as core]))\n\n(defn t [] (core/add 3 4))\n";
    let p = Proj::new(&[("src/app/core.clj", CORE), ("src/app/main.clj", main), ("test/app/core_test.clj", test)]);
    p.write("deps.edn", "{:paths [\"src\" \"test\"]}\n");
    let want = "app.core/add: 4 uses in 2 files\nsrc/app/main.clj\n  4 run    (defn run [] (core/add 1 2))\n  7 twice  (core/add x x))\n  9 (top)  (println (core/add 0 0))\ntest/app/core_test.clj\n  4 t  (defn t [] (core/add 3 4))\n";
    assert_eq!(p.nx(&["refs", "app.core/add"]), (want.to_string(), String::new(), 0));
    assert_eq!(p.nx(&["refs", "add"]).0, want);
    let (out, _, code) = p.nx(&["refs", "core/add", "--in", "src/app/main.clj"]);
    assert_eq!((out.as_str(), code), (want, 0));
    assert_eq!(p.nx(&["refs", "nope"]), (String::new(), "not found: nope\n".to_string(), 1));
}

#[test]
fn refs_without_uses() {
    let p = Proj::new(&[("src/app/core.clj", CORE)]);
    assert_eq!(p.nx(&["refs", "add"]), ("app.core/add: 0 uses in 0 files\n".to_string(), String::new(), 0));
}

#[test]
fn outline_of_file_and_ns() {
    let p = Proj::new(&[("src/app/lib.clj", LIB), ("src/app/main.clj", MAIN)]);
    let want = "app.lib  src/app/lib.clj\n   3 macro twice [& body]  Runs the body twice.\n   9 fn-   helper [x]\n  11 fn    pub [a] [a b]   Public fn.\n  16 var   limit\n";
    assert_eq!(p.nx(&["outline", "src/app/lib.clj"]), (want.to_string(), String::new(), 0));
    assert_eq!(p.nx(&["outline", "app.lib"]).0, want);
    let want = "app.main  src/app/main.clj  (requires app.core)\n  4 fn run []\n";
    assert_eq!(p.nx(&["outline", "app.main"]), (want.to_string(), String::new(), 0));
    assert_eq!(p.nx(&["outline", "app.none"]), (String::new(), "not found: app.none\n".to_string(), 1));
}

#[test]
fn ns_lists_namespaces_with_dependents() {
    let p = Proj::new(&[("src/app/core.clj", CORE), ("src/app/main.clj", MAIN), ("src/app/lib.clj", LIB)]);
    let want = "app.core  src/app/core.clj  1 public  <- app.main\napp.lib   src/app/lib.clj   3 public\napp.main  src/app/main.clj  1 public\n";
    assert_eq!(p.nx(&["ns"]), (want.to_string(), String::new(), 0));
    assert_eq!(p.nx(&["ns", "app.l"]).0, "app.lib  src/app/lib.clj  3 public\n");
    assert_eq!(p.nx(&["ns", "zzz"]), (String::new(), String::new(), 1));
}

#[test]
fn ns_all_lists_every_dependent() {
    let mut files = vec![("src/app/core.clj".to_string(), CORE.to_string())];
    files.extend((0..7).map(|i| (format!("src/app/u{i}.clj"), format!("(ns app.u{i}\n  (:require [app.core :as core]))\n"))));
    let p = Proj::new(&files.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect::<Vec<_>>());
    let first = |a: &[&str]| p.nx(a).0.lines().next().unwrap().to_string();
    assert_eq!(first(&["ns", "app.core"]), "app.core  src/app/core.clj  1 public  <- app.u0, app.u1, app.u2, app.u3, app.u4, +2");
    assert_eq!(first(&["--all", "ns", "app.core"]), "app.core  src/app/core.clj  1 public  <- app.u0, app.u1, app.u2, app.u3, app.u4, app.u5, app.u6");
}

#[test]
fn find_matches_names_case_insensitively() {
    let p = Proj::new(&[("src/app/core.clj", CORE), ("src/app/lib.clj", LIB)]);
    assert_eq!(p.nx(&["find", "HELPER"]).0, "src/app/lib.clj:9 app.lib/helper (private fn)\n");
    assert_eq!(p.nx(&["find", "wIc"]).0, "src/app/lib.clj:3 app.lib/twice (macro)\n");
    let (out, _, code) = p.nx(&["find", "e"]);
    let project: Vec<&str> = out.lines().take(2).collect();
    assert_eq!((project, code, out.lines().count()), (vec!["src/app/lib.clj:3 app.lib/twice (macro)", "src/app/lib.clj:9 app.lib/helper (private fn)"], 0, 41));
    assert!(out.ends_with("more (--all)\n"), "{out}");
    assert_eq!(p.nx(&["find", "zzz-none"]), (String::new(), String::new(), 1));
}

#[test]
fn doc_of_project_fn_and_special_form() {
    let p = Proj::new(&[("src/app/lib.clj", LIB)]);
    assert_eq!(p.nx(&["doc", "app.lib/pub"]), ("app.lib/pub  project src/app/lib.clj:11\n  [a] [a b]\n  Public fn.\n".to_string(), String::new(), 0));
    assert_eq!(p.nx(&["doc", "twice"]).0, "app.lib/twice  project src/app/lib.clj:3\n  [& body]\n  Runs the body twice.\n  Second line.\n");
    let (out, _, code) = p.nx(&["doc", "if"]);
    assert_eq!(code, 0);
    assert!(out.starts_with("if  special form\n  (if test then else?)\n  Evaluates test."), "{out}");
    assert_eq!(p.nx(&["doc", "no-such"]), (String::new(), "not found: no-such\n".to_string(), 1));
}

#[test]
fn json_output_of_new_commands() {
    let main = "(ns app.main\n  (:require [app.core :as core]))\n\n(defn run [] (core/add 1 2))\n";
    let p = Proj::new(&[("src/app/core.clj", CORE), ("src/app/main.clj", main)]);
    assert_eq!(p.nx(&["refs", "add", "--json"]).0, "{\"symbol\":\"app.core/add\",\"count\":1,\"files\":[{\"file\":\"src/app/main.clj\",\"uses\":[{\"line\":4,\"in\":\"run\",\"source\":\"(defn run [] (core/add 1 2))\"}]}]}\n");
    assert_eq!(p.nx(&["outline", "app.core", "--json"]).0, "{\"files\":[{\"ns\":\"app.core\",\"file\":\"src/app/core.clj\",\"requires\":[],\"vars\":[{\"line\":3,\"kind\":\"fn\",\"name\":\"add\",\"arglists\":\"[a b]\",\"doc\":\"\"}]}]}\n");
    assert_eq!(p.nx(&["ns", "app.core", "--json"]).0, "{\"namespaces\":[{\"ns\":\"app.core\",\"file\":\"src/app/core.clj\",\"public\":1,\"dependents\":[\"app.main\"]}]}\n");
    assert_eq!(p.nx(&["find", "ADD", "--json"]).0.get(..120).unwrap_or(""), "{\"matches\":[{\"location\":\"src/app/core.clj:3\",\"symbol\":\"app.core/add\",\"line\":\"src/app/core.clj:3 app.core/add (fn)\"},{\"lo");
    assert_eq!(p.nx(&["doc", "add", "--json"]).0, "{\"name\":\"app.core/add\",\"origin\":\"project src/app/core.clj:3\",\"location\":\"src/app/core.clj:3\",\"arglists\":[\"[a b]\"],\"doc\":\"\",\"source\":\"\"}\n");
}

#[test]
fn namespace_suffix_resolves_on_a_segment_boundary() {
    let p = Proj::new(&[("src/app/core/stats.clj", "(ns app.core.stats)\n\n(defn snap [] 1)\n"), ("src/app/other/stats.clj", "(ns app.other.stats)\n\n(defn snap [] 2)\n"), ("src/app/x.clj", "(ns app.x)\n\n(defn only [] 1)\n")]);
    assert!(p.nx(&["def", "core.stats/snap"]).0.starts_with("src/app/core/stats.clj:3 app.core.stats/snap (fn)\n"));
    assert!(p.nx(&["def", "x/only"]).0.starts_with("src/app/x.clj:3 app.x/only (fn)\n"));
    assert_eq!(p.nx(&["def", "tats/snap"]).2, 1);
    assert_eq!(p.nx(&["def", "ore.stats/snap"]).2, 1);
    let want = "src/app/core/stats.clj:3 app.core.stats/snap (fn)\nsrc/app/other/stats.clj:3 app.other.stats/snap (fn)\n";
    assert_eq!(p.nx(&["def", "stats/snap"]), (want.to_string(), String::new(), 1));
}

#[test]
fn errors_are_one_line_except_usage() {
    let p = Proj::new(&[("src/app/core.clj", CORE)]);
    assert_eq!(p.nx(&["def", "x", "--in", "nope.clj"]), (String::new(), "nx: no such file: nope.clj\n".to_string(), 2));
    assert_eq!(p.nx(&["check", "nope.clj"]), (String::new(), "nx: no such file: nope.clj\n".to_string(), 2));
    assert_eq!(p.nx(&["check", "deps.edn"]), (String::new(), "nx: not a project file: deps.edn\n".to_string(), 2));
    assert!(p.nx(&["def"]).1.starts_with("nx: usage: nx def <sym> [--in file]\nusage: nx [--root"));
}

#[test]
fn check_new_without_files_checks_changed_files() {
    let old = "(ns app.core)\n\n(defn add [a b] (let [old 1] (+ a b)))\n";
    let p = Proj::new(&[("src/app/core.clj", old), ("src/app/main.clj", MAIN)]);
    assert_eq!(p.nx(&["check", "--new"]), (String::new(), String::new(), 0));
    p.commit();
    assert_eq!(p.nx(&["check", "--new"]), (String::new(), String::new(), 0));
    p.write("src/app/core.clj", &format!("{old}\n(defn sub [a b] (let [unused 1] (- a b nope)))\n"));
    let err = "src/app/core.clj:5:40 error unresolved-symbol Unresolved symbol: nope\n    (defn sub [a b] (let [unused 1] (- a b nope)))\n";
    let warn = "src/app/core.clj:5:23 warning unused-binding unused binding unused\n    (defn sub [a b] (let [unused 1] (- a b nope)))\n";
    assert_eq!(p.nx(&["check", "--new"]), (format!("{err}{warn}"), String::new(), 1));
    assert_eq!(p.nx(&["check", "--new", "--errors"]), (err.to_string(), String::new(), 1));
}

#[test]
fn check_new_without_files_outside_git_is_silent() {
    let p = Proj::new(&[("src/app/core.clj", "(ns app.core)\n\n(defn f [] (nope))\n")]);
    assert_eq!(p.nx(&["check", "--new"]), (String::new(), String::new(), 0));
}

const UTIL: &str = "(ns app.u)\n\n(defn f [x] x)\n";

#[test]
fn hook_and_check_report_unresolved_var_and_namespace() {
    let p = Proj::new(&[("src/app/u.clj", UTIL)]);
    p.commit();
    p.write("src/app/a.clj", "(ns app.a\n  (:require [app.u :as u]))\n\n(defn g [] (u/nope 1))\n");
    p.write("src/app/b.clj", "(ns app.b)\n\n(defn g [] (u/f 1))\n");
    for f in ["src/app/a.clj", "src/app/b.clj"] {
        let (_, err, code) = p.hook(f);
        assert_eq!(code, 2, "{err}");
        assert!(err.starts_with("nx: 1 new error(s) after this edit\n"), "{err}");
        assert_eq!(p.nx(&["check", "--errors", f]).2, 1);
    }
    assert!(p.nx(&["check", "--errors", "src/app/a.clj"]).0.contains(" warning unresolved-var "));
    assert!(p.nx(&["check", "--errors", "src/app/b.clj"]).0.contains(" warning unresolved-namespace "));
}

#[test]
fn check_reports_unresolved_var_broken_elsewhere() {
    let p = Proj::new(&[("src/app/u.clj", UTIL), ("src/app/a.clj", "(ns app.a\n  (:require [app.u :as u]))\n\n(defn g [] (u/f 1))\n")]);
    p.write("src/app/u.clj", "(ns app.u)\n\n(defn h [x] x)\n");
    let (out, _, code) = p.nx(&["check", "src/app/u.clj"]);
    assert_eq!(code, 1);
    assert!(out.starts_with("broken elsewhere:\nsrc/app/a.clj:4:13 warning unresolved-var "), "{out}");
}

#[test]
fn help_and_version_exit_0() {
    let p = Proj::new(&[]);
    for a in [&["--help"][..], &["-h"], &["help"], &["check", "--help"]] {
        let (out, err, code) = p.nx(a);
        assert!(out.starts_with("usage: nx ") && out.contains("check [file...]"), "{a:?}: {out}");
        assert_eq!((err.as_str(), code), ("", 0), "{a:?}");
    }
    assert_eq!(p.nx(&["--version"]), (format!("nx {}\n", env!("CARGO_PKG_VERSION")), String::new(), 0));
}

/// `nx doc` with a fake Mova index: (stdout of `doc map`, of `doc time-ms`, of `doc clojure.core/map`) in `dir`.
fn mova_doc(dir: &Path, base: &Path) -> (String, String, String) {
    let mova = base.join("mova");
    std::fs::create_dir_all(mova.join("core")).unwrap();
    std::fs::create_dir_all(mova.join("src")).unwrap();
    std::fs::write(mova.join("core/core.mova"), "(defn map\n  \"Maps f over coll.\"\n  [f coll]\n  coll)\n").unwrap();
    std::fs::write(mova.join("src/sys.rs"), "reg(i, \"time-ms\", ArityHint::Exact(0), time_ms);\n").unwrap();
    let ix = format!(r#"{{"v":2,"root":"{}","namespaces":[{{"ns":"clojure.core","file":"core/core.mova"}}],"natives":[{{"ns":"clojure.core","name":"time-ms","file":"src/sys.rs","line":1}}],"aliases":[],"default_aliases":[]}}"#, mova.display());
    std::fs::write(base.join("index.json"), ix).unwrap();
    let run = |sym: &str| {
        let o = Command::new(env!("CARGO_BIN_EXE_nx")).current_dir(dir).args(["doc", sym]).env("XDG_CACHE_HOME", base.join("cache")).env("MOVA_SOURCE_INDEX", base.join("index.json")).env_remove("MOVA_BIN").output().unwrap();
        String::from_utf8_lossy(&o.stdout).into_owned()
    };
    (run("map"), run("time-ms"), run("clojure.core/map"))
}

#[test]
fn doc_of_mova_core_in_a_mova_project_and_without_a_project() {
    let p = Proj::new(&[("src/app/m.mova", "(ns app.m)\n\n(defn f [] (time-ms))\n")]);
    let base = p.cache.parent().unwrap().to_path_buf();
    let (map, native, qualified) = mova_doc(&p.dir, &base);
    assert!(map.starts_with("clojure.core/map  Mova stdlib ") && map.contains("[f coll]") && map.contains("Maps f over coll."), "{map}");
    assert!(native.starts_with("clojure.core/time-ms  Mova native (Rust) "), "{native}");
    assert_eq!(map, qualified);
    // no project marker at all: the same answers
    let empty = base.join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    let (map, native, _) = mova_doc(&empty, &base);
    assert!(map.starts_with("clojure.core/map  Mova stdlib ") && map.contains("Maps f over coll."), "{map}");
    assert!(native.starts_with("clojure.core/time-ms  Mova native (Rust) "), "{native}");
}

#[test]
fn mova_core_natives_resolve_without_an_index() {
    // no `mova` binary and no index: the generated core table still knows the natives
    let p = Proj::new(&[("src/app/m.mova", "(ns app.m)\n\n(defn f [] (sleep-ms (time-ms)) (sliding-buffer 1) (nowhere))\n")]);
    let (out, _, code) = p.nx(&["check"]);
    assert_eq!(code, 1);
    assert!(out.contains("Unresolved symbol: nowhere") && out.matches("Unresolved symbol").count() == 1, "{out}");
}
