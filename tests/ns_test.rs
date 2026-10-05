//! Namespaces and multi-file loading (v0.5 / R1, `src/ns.rs`).
//!
//! Every behavioral case runs through BOTH tiers -- the compiled one and
//! the tree-walker (`Interp::with_compile_enabled(false)`) -- because the
//! compiled tier resolves globals to cells once, at closure creation, while
//! the tree-walker resolves them per access: namespace resolution is
//! precisely where those two could drift. `both` is this file's version of
//! `differential_test.rs`'s `agree`, with a module path attached.
//!
//! Fixtures are real files in a `tests/tmp-ns-*` directory created and
//! removed per test (no `tempfile` dependency, and the crate has no
//! dev-dependencies to spend).

use std::path::{Path, PathBuf};

use mova::internal::Interp;

// ---------------------------------------------------------------------------
// Fixture + harness
// ---------------------------------------------------------------------------

/// A throwaway directory of `.mova` files, removed on drop.
struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Fixture {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("tests/tmp-ns-{tag}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("creating the fixture directory");
        Fixture { root }
    }

    /// Writes `rel` (a path relative to the fixture root), creating parent
    /// directories as needed.
    fn file(&self, rel: &str, contents: &str) -> &Fixture {
        let path = self.root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("creating a fixture subdirectory");
        }
        std::fs::write(&path, contents).expect("writing a fixture file");
        self
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[derive(PartialEq, Eq, Debug)]
enum Outcome {
    Ok(String),
    Err(String),
}

fn eval_one(interp: &mut Interp, src: &str) -> Outcome {
    match interp.eval_str("ns-test", src) {
        Ok(v) => match interp.realize_deep(&v) {
            Ok(realized) => Outcome::Ok(mova::internal::pr_str(&realized)),
            Err(e) => Outcome::Err(e.message),
        },
        Err(e) => Outcome::Err(e.message),
    }
}

/// Runs `src` with `fixture` on the module path in BOTH tiers, asserts they
/// agree, and returns the shared outcome.
fn both(fixture: &Fixture, src: &str) -> Outcome {
    let mut compiled = Interp::new();
    compiled.module_paths = vec![fixture.root.clone()];
    let mut walked = Interp::with_compile_enabled(false);
    walked.module_paths = vec![fixture.root.clone()];
    let a = eval_one(&mut compiled, src);
    let b = eval_one(&mut walked, src);
    assert_eq!(a, b, "tier divergence for: {src}");
    a
}

fn ok(fixture: &Fixture, src: &str, expected: &str) {
    assert_eq!(both(fixture, src), Outcome::Ok(expected.to_string()), "for: {src}");
}

/// Same as `conformance_test.rs`'s `CONFORMANCE_STACK_SIZE` and
/// `differential_test.rs`'s `DIFFERENTIAL_STACK_SIZE`, for the same
/// reason: a `cargo test` thread's default stack is far smaller than the
/// main thread's, and a DEBUG build's frames are several times fatter
/// than a release build's -- deeply recursive mova code (test.check's
/// lazy-seq/rose-tree machinery, in this file's case) overruns it.
const NS_TEST_STACK_SIZE: usize = 64 * 1024 * 1024;

/// Runs `f` on a thread with a real stack. Only the embedded-stdlib tests
/// below need it; every other test in this file is shallow.
fn on_big_stack(f: fn()) {
    std::thread::Builder::new()
        .stack_size(NS_TEST_STACK_SIZE)
        .spawn(f)
        .expect("failed to spawn the ns-test worker thread")
        .join()
        .unwrap_or_else(|payload| std::panic::resume_unwind(payload));
}

fn err_contains(fixture: &Fixture, src: &str, needle: &str) {
    match both(fixture, src) {
        Outcome::Err(m) => assert!(
            m.contains(needle),
            "for: {src}\n  expected an error containing: {needle}\n  got: {m}"
        ),
        other => panic!("expected an error for: {src}, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Duplicate bare names across files
// ---------------------------------------------------------------------------

/// The mission's reason to exist: 38 files, 77 duplicate bare top-level
/// names. Two namespaces defining `step` must not see each other's.
#[test]
fn same_bare_name_in_two_namespaces_stays_separate() {
    let fx = Fixture::new("dup");
    fx.file("app/a.mova", "(ns app.a)\n(defn step [x] [:a x])\n")
        .file("app/b.mova", "(ns app.b)\n(defn step [x] [:b x])\n");
    ok(
        &fx,
        "(ns main (:require [app.a :as a] [app.b :as b]))
         [(a/step 1) (b/step 1) (app.a/step 2) (app.b/step 2)]",
        "[[:a 1] [:b 1] [:a 2] [:b 2]]",
    );
}

/// Each file's own unqualified calls reach ITS `step`, not the other's --
/// the case a flat global table gets wrong even when the call sites are
/// qualified.
#[test]
fn unqualified_calls_inside_a_file_reach_that_files_definitions() {
    let fx = Fixture::new("own");
    fx.file(
        "app/a.mova",
        "(ns app.a)\n(defn step [x] [:a x])\n(defn run [x] (step x))\n",
    )
    .file(
        "app/b.mova",
        "(ns app.b)\n(defn step [x] [:b x])\n(defn run [x] (step x))\n",
    );
    ok(
        &fx,
        "(ns main (:require [app.a :as a] [app.b :as b])) [(a/run 1) (b/run 1)]",
        "[[:a 1] [:b 1]]",
    );
}

#[test]
fn an_alias_that_is_not_declared_does_not_resolve() {
    let fx = Fixture::new("noalias");
    fx.file("app/a.mova", "(ns app.a)\n(defn step [x] [:a x])\n");
    err_contains(
        &fx,
        "(ns main (:require [app.a :as a])) (nope/step 1)",
        "Unable to resolve symbol: nope/step",
    );
}

// ---------------------------------------------------------------------------
// :refer
// ---------------------------------------------------------------------------

#[test]
fn refer_binds_unqualified_names_from_another_namespace() {
    let fx = Fixture::new("refer");
    fx.file(
        "app/util.mova",
        "(ns app.util)\n(defn twice [x] (* 2 x))\n(defn thrice [x] (* 3 x))\n",
    );
    ok(
        &fx,
        "(ns main (:require [app.util :as u :refer [twice thrice]])) [(twice 4) (thrice 4) (u/twice 5)]",
        "[8 12 10]",
    );
}

/// `:refer :all` refers every def the target namespace made -- upstream's
/// own test suite requires clojure.test this way in 6 of its files
/// (`(:require [clojure.test :refer :all])`), which is what motivated
/// supporting it (COMPATIBILITY.md build queue, 2026-08-20).
#[test]
fn refer_all_binds_every_def_from_the_namespace() {
    let fx = Fixture::new("refer-all-binds");
    fx.file(
        "app/util.mova",
        "(ns app.util)\n(defn twice [x] (* 2 x))\n(defn thrice [x] (* 3 x))\n(def base 7)\n",
    );
    ok(
        &fx,
        "(ns main (:require [app.util :refer :all])) [(twice 4) (thrice 4) base]",
        "[8 12 7]",
    );
}

/// Clojure accepts a LIST of symbols in `:refer` as well as a vector --
/// upstream's parse.clj uses `(:require [x :refer (defspec)])`.
#[test]
fn refer_accepts_a_list_of_symbols() {
    let fx = Fixture::new("refer-list");
    fx.file("app/util.mova", "(ns app.util)\n(defn twice [x] (* 2 x))\n");
    ok(
        &fx,
        "(ns main (:require [app.util :refer (twice)])) (twice 21)",
        "42",
    );
}

/// A refer is per-namespace, not global: the namespace that did NOT refer
/// `twice` must still fail to resolve it.
#[test]
fn refers_do_not_leak_into_other_namespaces() {
    let fx = Fixture::new("refer-scope");
    fx.file("app/util.mova", "(ns app.util)\n(defn twice [x] (* 2 x))\n")
        .file(
            "app/other.mova",
            "(ns app.other (:require [app.util :refer [twice]]))\n(defn use-it [x] (twice x))\n",
        );
    ok(
        &fx,
        "(ns main (:require [app.other :as o])) (o/use-it 3)",
        "6",
    );
    err_contains(
        &fx,
        "(ns main (:require [app.other :as o])) (twice 3)",
        "Unable to resolve symbol: twice",
    );
}

// (A `refer_all_is_rejected_rather_than_half_implemented` test lived here
// until 2026-08-20: `:refer :all` used to be deliberately refused. It is
// now implemented -- see `refer_all_binds_every_def_from_the_namespace`
// above, which replaced that test rather than coexisting with it.)

// ---------------------------------------------------------------------------
// Loading: chains, idempotence, cycles, missing files, extensions
// ---------------------------------------------------------------------------

#[test]
fn a_require_chain_loads_transitively() {
    let fx = Fixture::new("chain");
    fx.file("chain/c.mova", "(ns chain.c)\n(def base 1)\n")
        .file(
            "chain/b.mova",
            "(ns chain.b (:require [chain.c :as c]))\n(def mid (+ c/base 10))\n",
        )
        .file(
            "chain/a.mova",
            "(ns chain.a (:require [chain.b :as b]))\n(def top (+ b/mid 100))\n",
        );
    ok(
        &fx,
        "(ns main (:require [chain.a :as a])) a/top",
        "111",
    );
}

/// Requiring the same namespace twice (here directly AND through a chain)
/// loads its file exactly once: the counter file appends to an atom that
/// would show 2 if it ran twice.
#[test]
fn double_require_loads_the_file_once() {
    let fx = Fixture::new("idem");
    fx.file(
        "idem/leaf.mova",
        "(ns idem.leaf)\n(def loads (atom 0))\n(swap! loads inc)\n(defn count-loads [] @loads)\n",
    )
    .file(
        "idem/mid.mova",
        "(ns idem.mid (:require [idem.leaf :as l]))\n(defn via [] (l/count-loads))\n",
    );
    ok(
        &fx,
        "(ns main (:require [idem.leaf :as l] [idem.mid :as m] [idem.leaf :as l2]))
         [(l/count-loads) (m/via) (l2/count-loads)]",
        "[1 1 1]",
    );
}

#[test]
fn a_require_cycle_is_a_clear_error_not_a_hang() {
    let fx = Fixture::new("cycle");
    fx.file("cyc/a.mova", "(ns cyc.a (:require [cyc.b :as b]))\n(def x 1)\n")
        .file("cyc/b.mova", "(ns cyc.b (:require [cyc.a :as a]))\n(def y 2)\n");
    err_contains(
        &fx,
        "(ns main (:require [cyc.a :as a])) a/x",
        "circular namespace dependency: cyc.a -> cyc.b -> cyc.a",
    );
}

#[test]
fn a_missing_namespace_names_what_it_looked_for() {
    let fx = Fixture::new("missing");
    err_contains(
        &fx,
        "(ns main (:require [app.nowhere :as n]))",
        "could not locate namespace app.nowhere on the module path: no app/nowhere.mova or app/nowhere.clj or app/nowhere.cljc in",
    );
}

// ---------------------------------------------------------------------------
// SPEC-W1 task 1: `.clj` in the require/load extension search
// ---------------------------------------------------------------------------

/// A namespace whose only source file is plain `.clj` resolves. The
/// motivating real case is vendored `clojure.test.check.random`, the ONE
/// `.clj` file in an otherwise-`.cljc` library -- before this, requiring
/// `clojure.test.check` failed on that single file.
#[test]
fn a_clj_only_namespace_is_found() {
    let fx = Fixture::new("clj-ext");
    fx.file("app/plain.clj", "(ns app.plain)\n(defn step [x] [:clj x])\n");
    ok(
        &fx,
        "(ns main (:require [app.plain :as p])) (p/step 1)",
        "[:clj 1]",
    );
}

/// `.mova` stays the override: with both files present, the `.mova` one
/// wins (it is first in `ns_file_names`).
#[test]
fn a_mova_file_shadows_a_clj_twin() {
    let fx = Fixture::new("clj-shadow");
    fx.file("app/twin.mova", "(ns app.twin)\n(def which :mova)\n")
        .file("app/twin.clj", "(ns app.twin)\n(def which :clj)\n");
    ok(&fx, "(ns main (:require [app.twin :as t])) t/which", ":mova");
}

/// `.clj` beats `.cljc` when both exist -- real Clojure's own preference
/// order (`root-resource` tries `.clj` first).
#[test]
fn a_clj_file_shadows_a_cljc_twin() {
    let fx = Fixture::new("clj-over-cljc");
    fx.file("app/pref.clj", "(ns app.pref)\n(def which :clj)\n")
        .file("app/pref.cljc", "(ns app.pref)\n(def which :cljc)\n");
    ok(&fx, "(ns main (:require [app.pref :as p])) p/which", ":clj");
}

/// Reader conditionals stay `.cljc`-ONLY: the same `#?(...)` text is
/// dispatched in a `.cljc` file and REJECTED outright in a `.clj` one --
/// exactly the split real Clojure draws (its own `.clj` read raises
/// "Conditional read not allowed").
#[test]
fn reader_conditionals_are_cljc_only_not_clj() {
    let fx = Fixture::new("clj-nocond");
    fx.file(
        "app/cond_c.cljc",
        "(ns app.cond-c)\n(def v #?(:clj :from-clj :cljs :from-cljs))\n",
    )
    .file(
        "app/cond_j.clj",
        "(ns app.cond-j)\n(def v #?(:clj :from-clj :cljs :from-cljs))\n",
    );
    ok(&fx, "(ns m1 (:require [app.cond-c :as c])) c/v", ":from-clj");
    err_contains(
        &fx,
        "(ns m2 (:require [app.cond-j :as j])) j/v",
        "Conditional read not allowed",
    );
}

/// `load` shares the extension search: a `.clj` part-file of a multi-file
/// namespace resolves the same way `require` resolves a `.clj` namespace.
#[test]
fn load_finds_a_clj_part_file() {
    let fx = Fixture::new("clj-load");
    fx.file("app/multi.clj", "(ns app.multi)\n(load \"multi/part\")\n")
        .file(
            "app/multi/part.clj",
            "(in-ns 'app.multi)\n(def part :loaded)\n",
        );
    ok(&fx, "(ns main (:require [app.multi :as m])) m/part", ":loaded");
}

/// Hyphens in a namespace segment are underscores in the file name.
#[test]
fn munged_file_names_are_found() {
    let fx = Fixture::new("munge");
    fx.file(
        "oma/core/mode_line.mova",
        "(ns oma.core.mode-line)\n(defn render [] :mode-line)\n",
    );
    ok(
        &fx,
        "(ns main (:require [oma.core.mode-line :as ml])) (ml/render)",
        ":mode-line",
    );
}

/// The namespaces the interpreter provides itself (`clojure.string`,
/// `flow`) have no file, and requiring one must record its alias rather
/// than hunt the module path.
#[test]
fn requiring_a_built_in_namespace_records_the_alias_without_a_file() {
    let fx = Fixture::new("builtin-ns");
    ok(
        &fx,
        "(ns main (:require [clojure.string :as s])) (s/upper-case \"ab\")",
        "\"AB\"",
    );
    ok(
        &fx,
        "(ns main (:require [clojure.string :refer [join]])) (join \",\" [1 2])",
        "\"1,2\"",
    );
}

#[test]
fn a_bare_require_spec_loads_without_aliasing() {
    let fx = Fixture::new("bare");
    fx.file("app/thing.mova", "(ns app.thing)\n(def answer 42)\n");
    ok(
        &fx,
        "(ns main (:require app.thing)) app.thing/answer",
        "42",
    );
}

/// A reader or eval error inside a required file surfaces with THAT file's
/// path as the diagnostic source, not the requiring file's.
#[test]
fn errors_in_a_loaded_file_point_at_that_file() {
    let fx = Fixture::new("badfile");
    fx.file("bad/reader.mova", "(ns bad.reader)\n(def x [1 2\n")
        .file("bad/runtime.mova", "(ns bad.runtime)\n(def x (nope 1))\n");
    for (ns, file) in [
        ("bad.reader", "bad/reader.mova"),
        ("bad.runtime", "bad/runtime.mova"),
    ] {
        let mut interp = Interp::new();
        interp.module_paths = vec![fx.root.clone()];
        let err = interp
            .eval_str("ns-test", &format!("(ns main (:require [{ns} :as x]))"))
            .expect_err("the required file must fail to load");
        assert!(
            interp.source_name.ends_with(file),
            "diagnostic source should be {file}, was {}",
            interp.source_name
        );
        assert!(err.span.is_some(), "the error should carry a span into {file}");
    }
}

// ---------------------------------------------------------------------------
// Shadowing core, macros, forward references
// ---------------------------------------------------------------------------

/// A namespace may define its own `count`; unqualified uses inside it mean
/// ITS definition, while every other namespace still sees core's. This is
/// what makes `(:refer-clojure :exclude [count])` safe to ignore.
#[test]
fn a_namespace_can_shadow_a_core_name_for_itself_only() {
    let fx = Fixture::new("shadow");
    fx.file(
        "sh/own.mova",
        "(ns sh.own (:refer-clojure :exclude [count]))\n\
         (defn count [x] :mine)\n\
         (defn use-it [x] (count x))\n",
    )
    .file(
        "sh/other.mova",
        "(ns sh.other)\n(defn use-core [x] (count x))\n",
    );
    ok(
        &fx,
        "(ns main (:require [sh.own :as o] [sh.other :as t]))
         [(o/use-it [1 2 3]) (t/use-core [1 2 3]) (count [1 2 3])]",
        "[:mine 3 3]",
    );
}

/// The intrinsic path specifically: `+` is compiled to an `IntrinOp`, so a
/// namespace that defines its own `+` must degrade that node to an ordinary
/// call -- while core's `+` stays pristine for everyone else.
#[test]
fn a_namespace_can_shadow_an_arithmetic_intrinsic() {
    let fx = Fixture::new("shadow-intrin");
    fx.file(
        "sh/plus.mova",
        "(ns sh.plus)\n(defn + [a b] [:plus a b])\n(defn use-it [] (+ 1 2))\n",
    );
    ok(
        &fx,
        "(ns main (:require [sh.plus :as p])) [(p/use-it) (+ 1 2)]",
        "[[:plus 1 2] 3]",
    );
}

/// A `def` in a namespace must never write through a bare builtin's cell:
/// `resolve` here is a stand-in for the 77 duplicate names, and core's own
/// fns must keep working afterwards.
#[test]
fn a_namespace_def_never_clobbers_the_bare_builtin_cell() {
    let fx = Fixture::new("noclobber");
    fx.file(
        "sh/clash.mova",
        "(ns sh.clash)\n(defn str [& xs] :not-str)\n(defn mine [] (str 1 2))\n",
    );
    ok(
        &fx,
        "(ns main (:require [sh.clash :as c])) [(c/mine) (str 1 2) (str \"a\" \"b\")]",
        "[:not-str \"12\" \"ab\"]",
    );
}

#[test]
fn macros_expand_across_namespaces() {
    let fx = Fixture::new("macro");
    fx.file(
        "mac/m.mova",
        "(ns mac.m)\n(defmacro twice [x] (list 'do x x))\n(defmacro plus1 [x] (list '+ x 1))\n",
    );
    // Through an alias, through a refer, and inside a fn body -- the last
    // one is the compile-tier path (macros expand at closure creation).
    ok(
        &fx,
        "(ns main (:require [mac.m :as m :refer [plus1]]))
         (defn f [x] (m/plus1 x))
         (defn g [x] (plus1 x))
         [(m/plus1 1) (plus1 1) (f 10) (g 10)]",
        "[2 2 11 11]",
    );
}

/// A macro used UNQUALIFIED inside its own namespace, both at top level and
/// from a fn body (where the compiled tier expands it at closure creation),
/// alongside the core macros (`defn`, `when`) every file leans on.
#[test]
fn a_namespace_sees_its_own_macros_and_cores() {
    let fx = Fixture::new("macro-own");
    fx.file(
        "mac/own.mova",
        "(ns mac.own)\n\
         (defmacro twice [x] (list '+ x x))\n\
         (defn f [y] (twice y))\n\
         (defn g [y] (when (twice y) :yes))\n",
    );
    ok(
        &fx,
        "(ns main (:require [mac.own :as o])) [(o/f 2) (o/g 1)]",
        "[4 :yes]",
    );
}

/// A macro whose *body* calls a helper defined in the macro's own
/// namespace: expansion runs in that namespace, not the caller's.
#[test]
fn a_macro_body_resolves_in_its_own_namespace() {
    let fx = Fixture::new("macro-helper");
    fx.file(
        "mac/h.mova",
        "(ns mac.h)\n(defn wrap [x] (list :wrapped x))\n(defmacro emit [x] (list 'quote (wrap x)))\n",
    );
    ok(
        &fx,
        "(ns main (:require [mac.h :as h])) (h/emit 7)",
        "(:wrapped 7)",
    );
}

/// A fn may call a fn defined LATER in the same file: the qualified cell is
/// interned unbound at compile time and late-binds through the same cell.
#[test]
fn forward_references_within_a_namespace_late_bind() {
    let fx = Fixture::new("forward");
    fx.file(
        "fw/f.mova",
        "(ns fw.f)\n(defn top [x] (helper x))\n(defn helper [x] [:helped x])\n",
    );
    ok(&fx, "(ns main (:require [fw.f :as f])) (f/top 1)", "[:helped 1]");
}

/// Mutual recursion across a namespace boundary, in both directions.
#[test]
fn cross_namespace_calls_work_in_both_directions() {
    let fx = Fixture::new("cross");
    fx.file("x/lib.mova", "(ns x.lib)\n(defn twice [f v] (f (f v)))\n")
        .file(
            "x/app.mova",
            "(ns x.app (:require [x.lib :as lib]))\n(defn inc2 [v] (+ v 1))\n(defn run [v] (lib/twice inc2 v))\n",
        );
    ok(&fx, "(ns main (:require [x.app :as a])) (a/run 5)", "7");
}

/// A `future` spawned inside a loaded namespace's fn: the forked `Interp`
/// shares the namespace registry, and the fn it calls carries its own
/// namespace, so resolution inside the thread is the defining one.
#[test]
fn a_future_spawned_from_a_loaded_namespace_resolves_in_that_namespace() {
    let fx = Fixture::new("future");
    fx.file(
        "fut/f.mova",
        "(ns fut.f)\n(defn helper [x] [:from-fut x])\n(defn run [x] @(future (helper x)))\n",
    );
    ok(&fx, "(ns main (:require [fut.f :as f])) (f/run 1)", "[:from-fut 1]");
}

// ---------------------------------------------------------------------------
// A qualified symbol never resolves to a local (this mission's bug)
// ---------------------------------------------------------------------------

/// The exact repro: `(defn f [state] (mode/state state :x))` -- the call
/// `mode/state` must reach the global fn `oc.mode/state`, never the `state`
/// parameter that merely shares its bare name. `ok`/`both` already checks
/// both tiers agree.
#[test]
fn qualified_symbols_never_resolve_to_locals() {
    let fx = Fixture::new("qual-local");
    fx.file("oc/mode.mova", "(ns oc.mode)\n(defn state [s k] [:global s k])\n");
    ok(
        &fx,
        "(ns main (:require [oc.mode :as mode]))
         (defn f [state] (mode/state state :x))
         (f 99)",
        "[:global 99 :x]",
    );
}

/// Same rule for a `let` binding -- real Clojure's `(let [state 1]
/// some.ns/state)` never sees the local either.
#[test]
fn qualified_symbols_never_resolve_to_let_locals() {
    let fx = Fixture::new("qual-let");
    fx.file("oc/mode.mova", "(ns oc.mode)\n(def state :global-val)\n");
    ok(
        &fx,
        "(ns main (:require [oc.mode :as mode]))
         (let [state :local-val] mode/state)",
        ":global-val",
    );
}

/// Same rule for a `loop` binding.
#[test]
fn qualified_symbols_never_resolve_to_loop_bindings() {
    let fx = Fixture::new("qual-loop");
    fx.file("oc/mode.mova", "(ns oc.mode)\n(def state :global-val)\n");
    ok(
        &fx,
        "(ns main (:require [oc.mode :as mode]))
         (loop [state :local-val n 0]
           (if (< n 1) (recur state (inc n)) mode/state))",
        ":global-val",
    );
}

/// Same rule inside a nested `fn` (a `#(...)`-style closure): the qualified
/// symbol must skip the captured `state` param even though the closure
/// crosses a fn boundary to reach it -- the compile tier's by-value capture
/// path is where this bug could most easily resurface.
#[test]
fn qualified_symbols_never_resolve_inside_nested_closures() {
    let fx = Fixture::new("qual-nested");
    fx.file("oc/mode.mova", "(ns oc.mode)\n(defn state [x] [:global x])\n");
    ok(
        &fx,
        "(ns main (:require [oc.mode :as mode]))
         (defn f [state] (map (fn [x] (mode/state x)) [1 2]))
         (f 5)",
        "([:global 1] [:global 2])",
    );
}

// ---------------------------------------------------------------------------
// The default namespace: REPL/script behavior must be unchanged
// ---------------------------------------------------------------------------

#[test]
fn plain_scripts_still_work_in_the_user_namespace() {
    let fx = Fixture::new("user");
    ok(&fx, "(def x 1) (defn f [y] (+ x y)) (f 41)", "42");
    ok(&fx, "(def x 1) user/x", "1");
    // A file with no `ns` form at all, loaded as a namespace, defs into the
    // namespace it was required as.
    fx.file("plain/p.mova", "(def value 9)\n");
    ok(&fx, "(ns main (:require [plain.p :as p])) p/value", "9");
}

// ---------------------------------------------------------------------------
// The CLI flag
// ---------------------------------------------------------------------------

#[test]
fn cli_module_path_flag_resolves_requires() {
    let fx = Fixture::new("cli");
    fx.file("cli/lib.mova", "(ns cli.lib)\n(defn greet [] \"hi\")\n");
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_mova"))
        .arg("--module-path")
        .arg(&fx.root)
        .arg("-e")
        .arg("(ns main (:require [cli.lib :as l])) (l/greet)")
        .output()
        .expect("running the mova binary");
    assert!(
        out.status.success(),
        "exit {:?}, stderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "\"hi\"");
}

/// Without the flag, a file's own directory is the module path, so a script
/// can require its siblings.
#[test]
fn cli_file_argument_requires_resolve_against_the_files_directory() {
    let fx = Fixture::new("cli-file");
    fx.file("lib.mova", "(ns lib)\n(def answer 42)\n")
        .file("main.mova", "(ns main (:require [lib :as l]))\n(println l/answer)\n");
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_mova"))
        .arg(fx.root.join("main.mova"))
        .output()
        .expect("running the mova binary");
    assert!(
        out.status.success(),
        "exit {:?}, stderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "42");
}

/// A load failure exits non-zero and renders the diagnostic against the
/// FAILING file, not the one that required it.
#[test]
fn cli_reports_a_failing_required_file_by_name() {
    let fx = Fixture::new("cli-bad");
    fx.file("broken.mova", "(ns broken)\n(def x (nope 1))\n")
        .file("entry.mova", "(ns entry (:require broken))\n");
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_mova"))
        .arg(fx.root.join("entry.mova"))
        .output()
        .expect("running the mova binary");
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("broken.mova") && stderr.contains("Unable to resolve symbol: nope"),
        "stderr was: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// S6/Blocker-1+2: `clojure.core` self-aliasing, and `eval`'s namespace
// ---------------------------------------------------------------------------

/// S6/Blocker-1: aliasing `clojure.core` (a pre-loaded/builtin namespace,
/// not a file) must make its STRUCTURAL special forms (`let`, `fn`, `if`,
/// ...) reachable through the alias, exactly like bare -- not just its
/// ordinary `core.mova`-bootstrapped macros/fns, which were already
/// reachable via `for_each_global_candidate`'s bare-name fallback (they
/// intern BARE inside `CORE_NS`, so the alias-expanded-qualified probe
/// misses but the trailing bare probe still finds them). Special forms
/// have no such fallback -- they are matched ONLY by `eval_list`'s/
/// `compile_list`'s dispatch gate (measured before this fix: `core/inc`
/// resolved, `core/let` did not, see `ns::Interp::is_bare_or_core_alias`'s
/// doc comment). test.check's own `generators.cljc` does exactly this
/// (`(:require [clojure.core :as core]) ... (core/let [v ...] ...)`).
#[test]
fn core_alias_dispatches_special_forms_like_bare() {
    let fx = Fixture::new("core-alias");
    ok(
        &fx,
        "(ns main (:require [clojure.core :as c])) (c/let [x 1] (c/inc x))",
        "2",
    );
    ok(
        &fx,
        "(ns main2 (:require [clojure.core :as c])) (c/if true :yes :no)",
        ":yes",
    );
}

/// S6/Blocker-2: `eval` operates against the CURRENT VALUE of `*ns*`
/// (Clojure's own docstring wording for `eval`), a genuinely dynamic
/// value that only changes via `ns`/`in-ns`/`require`'s file-level
/// switch -- NOT against whatever namespace a currently-executing
/// closure's body happens to be lexically resolving its OWN free symbols
/// against (`Interp::current_ns`, which `apply_closure` overwrites for
/// the duration of every fn/macro call, per this module's "which
/// namespace is current" doc).
///
/// Reproduces the measured root cause behind `api.clj`/`data_structures.
/// clj`'s "Unable to resolve symbol: cgen/ednable" failure: a macro
/// (`definer.mac`, standing in for `clojure.test.generative`'s `defspec`)
/// calls `eval` on a symbol reachable only through an alias the CALLING
/// file declares, not one the macro's OWN defining namespace has. Two
/// bugs had to both be fixed for this to work: (1) `eval` must consult
/// `*ns*`, not `current_ns` (`builtins::reflect::eval_native`), and (2)
/// `*ns*` itself must not go stale after `require_ns` finishes loading a
/// file -- it used to restore ONLY the `current_ns` FIELD afterward
/// (`self.current_ns = prev_ns`), leaving the global `*ns*` var pointing
/// at whichever namespace was loaded LAST, which this fixture's
/// `(:require [gen :as cgen] [definer :as mac])` order (`definer` last)
/// is deliberately shaped to expose.
#[test]
fn eval_inside_a_macro_body_sees_the_callers_ns_not_the_macros_definer() {
    let fx = Fixture::new("eval-ns");
    fx.file("gen.mova", "(ns gen)\n(def answer 42)\n")
        .file(
            "definer.mova",
            "(ns definer)\n\
             (defmacro mac [args]\n\
             \x20 (let [v (eval (:tag (meta (first args))))]\n\
             \x20   (list (quote quote) v)))\n",
        );
    ok(
        &fx,
        "(ns main (:require [gen :as cgen] [definer :refer [mac]]))\n\
         (mac [^{:tag cgen/answer} o])",
        "42",
    );
}

// ---------------------------------------------------------------------------
// f4/ns: `ns-interns`/`ns-publics`/`ns-map`'s genuine-mapping gate
//
// (W-DECL integration fix, oracle transcript
// `compat/w-decl-fix-ns-machinery-oracle-transcript.txt`, measured against
// real Clojure 1.13.0-alpha6.) Before this fix all three surfaces gated on
// BOUNDNESS (`Env::find_bound_cell`), which meant a `(declare x)` or 1-arg
// `(def x)` -- genuinely interned, but never given a root value -- vanished
// from every one of them, even though the JVM's own `Namespace.
// getMappings` lists it. `ns-interns`/`ns-map` didn't exist in mova at
// all before this fix; `ns-publics` existed but had this exact gate bug
// (and, separately, never filtered `^:private` vars in the first place).
// ---------------------------------------------------------------------------

/// `(declare x)` interns `x` with no root value -- `ns-interns` (which
/// narrows by nothing except genuineness, see its own builtin doc in
/// `src/builtins/nsfns.rs`) must list it anyway. This is the exact shape
/// `mova-test-shim.mova`'s `test-all-vars`/`clojure.test`-alike helper
/// would need if it ever called `ns-interns` on a namespace with a bare
/// `declare` in it.
#[test]
fn declare_then_ns_interns_lists_the_unbound_var() {
    let fx = Fixture::new("declare-interns");
    fx.file("app/decl.mova", "(ns app.decl)\n(declare later-var)\n");
    ok(
        &fx,
        "(ns main (:require [app.decl]))\n\
         (contains? (ns-interns 'app.decl) 'later-var)",
        "true",
    );
}

/// A 1-arg `(def x)` -- W-DECL's genuinely-unbound-var shape, as opposed to
/// the pre-W-DECL `nil`-bound one -- must show up in `ns-publics` (public
/// by default, no `^:private` metadata) even though nothing ever gave it a
/// root value. This is the oracle transcript's `unbound-pub` case, minus
/// the sibling private var (covered separately below).
#[test]
fn one_arg_def_then_ns_publics_lists_the_unbound_var() {
    let fx = Fixture::new("def1-publics");
    fx.file("app/d1.mova", "(ns app.d1)\n(def unbound-thing)\n");
    ok(
        &fx,
        "(ns main (:require [app.d1]))\n\
         (contains? (ns-publics 'app.d1) 'unbound-thing)",
        "true",
    );
}

/// The oracle transcript's exact pairing, reproduced end to end: a
/// `^{:private true}` unbound var and a plain unbound var in the same
/// namespace. `ns-interns` lists BOTH (privacy is a resolution-time gate on
/// the JVM, never a listing-time one there); `ns-publics` narrows to just
/// the public one, via `VarCell::is_private` -- the same check `refer` and
/// `Interp::private_var_violation` already used, now shared three ways.
#[test]
fn ns_publics_hides_a_private_var_that_ns_interns_still_lists() {
    let fx = Fixture::new("privacy-listing");
    fx.file(
        "app/priv.mova",
        "(ns app.priv)\n(def ^{:private true} hidden-var)\n(def unbound-pub)\n",
    );
    ok(
        &fx,
        "(ns main (:require [app.priv]))\n\
         [(sort (keys (ns-interns 'app.priv))) (sort (keys (ns-publics 'app.priv)))]",
        "[(hidden-var unbound-pub) (unbound-pub)]",
    );
}

/// `ns-map` must also see the private, unbound var -- the oracle
/// transcript's own third listing surface (`(contains? (ns-map tns)
/// 'hidden-var)` => `true`), exercised here through the same fixture as the
/// privacy-listing test above.
#[test]
fn ns_map_contains_a_private_unbound_var() {
    let fx = Fixture::new("ns-map-hidden");
    fx.file(
        "app/priv2.mova",
        "(ns app.priv2)\n(def ^{:private true} hidden-var)\n(def unbound-pub)\n",
    );
    ok(
        &fx,
        "(ns main (:require [app.priv2]))\n\
         (contains? (ns-map 'app.priv2) 'hidden-var)",
        "true",
    );
}

/// The other side of the fix, and the reason a plain widen-the-gate patch
/// would have been wrong: a symbol the COMPILER only ever probed while
/// resolving some other candidate -- `probe-fn`'s free reference to a
/// global that no `def`/`declare` ever reaches -- gets interned as a
/// `speculative` placeholder (`Env::intern_speculative`,
/// `compile::resolve::global_chain`), not a genuine namespace mapping.
/// `Env::find_any_cell`'s `!c.0.is_speculative()` filter is what keeps that
/// placeholder OUT of `ns-interns`/`ns-publics`/`ns-map` -- listing it
/// would leak a phantom var for every global-candidate name the compiler
/// ever happened to look at. Only `probe-fn` itself (a genuine `defn`)
/// should appear.
#[test]
fn a_compiler_probed_but_never_defined_name_does_not_leak_into_ns_interns() {
    let fx = Fixture::new("speculative-listing");
    fx.file(
        "app/spec.mova",
        "(ns app.spec)\n(defn probe-fn [] totally-undefined-global-xyz)\n",
    );
    ok(
        &fx,
        "(ns main (:require [app.spec]))\n\
         [(contains? (ns-interns 'app.spec) 'totally-undefined-global-xyz)\n\
         \x20(sort (keys (ns-interns 'app.spec)))]",
        "[false (probe-fn)]",
    );
}

// ---------------------------------------------------------------------------
// SPEC-W1: `for`/`doseq` expand to `clojure.core/`-qualified core names
// ---------------------------------------------------------------------------

/// A namespace that shadows `seq`/`first`/`rest`/`cons` must still get the
/// REAL core fns inside a `for`/`doseq` expansion. `core.mova` builds those
/// two macros' output with `(list 'sym ...)` rather than syntax-quote, so
/// nothing qualified the emitted names and they resolved in the CALLING
/// namespace -- measured on vendored `clojure.test.check.rose-tree`, whose
/// own `seq` (a shrink-tree walk) got spliced into `permutations`' `for`
/// loop and took down every collection generator. See `doseq-step`'s own
/// comment in `core/core.mova`.
#[test]
fn for_and_doseq_are_immune_to_a_namespace_shadowing_core_seq() {
    let fx = Fixture::new("shadow-seq");
    fx.file(
        "app/shadowed.mova",
        "(ns app.shadowed)\n\
         (defn seq [x] :shadowed-seq)\n\
         (defn first [x] :shadowed-first)\n\
         (defn rest [x] :shadowed-rest)\n\
         (defn cons [a b] :shadowed-cons)\n\
         (defn next [x] :shadowed-next)\n\
         (def squares (vec (for [x [1 2 3]] (* x x))))\n\
         (def cross (vec (for [x [1 2] y [10 20]] [x y])))\n\
         (def filtered (vec (for [x [1 2 3 4] :when (even? x)] x)))\n\
         (def stepped (let [acc (atom [])]\n\
         \x20                (doseq [x [1 2 3]] (swap! acc clojure.core/conj x))\n\
         \x20                @acc))\n",
    );
    ok(
        &fx,
        "(ns main (:require [app.shadowed :as s]))\n\
         [s/squares s/cross s/filtered s/stepped]",
        "[[1 4 9] [[1 10] [1 20] [2 10] [2 20]] [2 4] [1 2 3]]",
    );
}

// ---------------------------------------------------------------------------
// SPEC-W1 task 2: the embedded stdlib module table
// ---------------------------------------------------------------------------

/// A namespace in `crate::stdlib`'s table is `require`-able with NO module
/// path at all -- the property `clojure.spec.alpha` needs from its
/// generator backend, since a spec user has no reason to put test.check on
/// a module path. See `src/stdlib.rs`.
#[test]
fn an_embedded_namespace_requires_with_no_module_path() {
    on_big_stack(|| {
        let fx = Fixture::new("embedded-none");
        // Empty fixture: nothing whatsoever on the module path.
        ok(
            &fx,
            "(ns main (:require [clojure.test.check.generators :as gen]))\n\
             [(count (gen/sample gen/boolean 5)) (gen/generate (gen/return :ok))]",
            "[5 :ok]",
        );
    });
}

/// The whole embedded stack loads and a property actually runs -- the
/// acceptance shape for the table (`quick-check` reaches `random`,
/// `rose-tree`, `results`, `impl`, `generators` and `properties` all at
/// once).
#[test]
fn quick_check_runs_off_the_embedded_stack() {
    on_big_stack(|| {
        let fx = Fixture::new("embedded-qc");
        ok(
            &fx,
            "(ns main (:require [clojure.test.check :as tc]\n\
             \x20                  [clojure.test.check.generators :as gen]\n\
             \x20                  [clojure.test.check.properties :as prop]))\n\
             (let [r (tc/quick-check 25 (prop/for-all [v (gen/vector gen/small-integer)]\n\
             \x20                        (= (count v) (count (reverse v)))))]\n\
             \x20 [(:pass? r) (:num-tests r)])",
            "[true 25]",
        );
    });
}

/// DISK WINS: a file on the module path shadows the embedded copy, so
/// `--module-path` can always override anything shipped in the binary.
/// The clojure-suite runner depends on exactly this (it materializes its
/// own copy of these same files and must keep scoring that copy).
#[test]
fn a_module_path_file_shadows_an_embedded_namespace() {
    // Shallow (the shadowing file is two lines), so no big stack needed.
    let fx = Fixture::new("embedded-shadow");
    fx.file(
        "clojure/test/check/generators.cljc",
        "(ns clojure.test.check.generators)\n(def marker :from-disk)\n",
    );
    ok(
        &fx,
        "(ns main (:require [clojure.test.check.generators :as gen])) gen/marker",
        ":from-disk",
    );
}

/// A namespace that is neither on disk nor in the table still fails the
/// same clear way it always did -- the table adds names, it does not turn
/// `require` permissive. (SPEC-W5 embedded `clojure.spec.test.alpha`,
/// which used to be this test's example; `clojure.pprint` is the honest
/// replacement -- mova ships a namespace-ONLY stub for it in
/// `clojure.core`'s bootstrap and the real vendored library is a
/// `tests/clojure-suite` artifact, never an embedded row, which is
/// precisely why `clojure.spec.test.alpha`'s port had to drop it from its
/// `ns` form. See MOVA-PATCH P14 in docs/SPEC-PORT-PATCHES.md.)
#[test]
fn a_namespace_outside_the_table_still_fails_cleanly() {
    let fx = Fixture::new("embedded-miss");
    err_contains(
        &fx,
        "(ns main (:require [clojure.java.shell :as sh]))",
        "could not locate namespace clojure.java.shell on the module path",
    );
}

// ---------------------------------------------------------------------------
// SPEC-W3 task 2: `clojure.spec.alpha` itself is embedded
// ---------------------------------------------------------------------------

/// THE acceptance shape for wave 3: a bare binary, no `--module-path`,
/// `(require '[clojure.spec.alpha :as s])`, and the operations a spec user
/// reaches for first. Nothing is staged -- every byte, spec itself,
/// `clojure.spec.gen.alpha` and the `clojure.walk` spec's own `ns` form
/// requires, comes out of `crate::stdlib`.
#[test]
fn spec_alpha_requires_and_works_with_no_module_path() {
    on_big_stack(|| {
        let fx = Fixture::new("embedded-spec");
        ok(
            &fx,
            "(ns main (:require [clojure.spec.alpha :as s]))\n\
             (s/def ::big (s/and int? #(> % 100)))\n\
             [(s/conform ::big 500)\n\
             \x20(s/conform ::big 5)\n\
             \x20(s/valid? ::big 500)\n\
             \x20(s/explain-str ::big 5)\n\
             \x20(s/describe ::big)]",
            "[500 :clojure.spec.alpha/invalid true \
             \"5 - failed: (> % 100) spec: :main/big\\n\" (and int? (> % 100))]",
        );
    });
}

/// `clojure.walk` rides along as its own embedded namespace, so it is
/// `require`-able in its own right and not merely a hidden dependency of
/// spec's `ns` form.
#[test]
fn clojure_walk_requires_with_no_module_path() {
    let fx = Fixture::new("embedded-walk");
    ok(
        &fx,
        "(ns main (:require [clojure.walk :as walk]))\n\
         (walk/postwalk (fn [x] (if (number? x) (inc x) x)) {:a [1 2]})",
        "{:a [2 3]}",
    );
}

/// `s/exercise` is the whole dynaload chain end to end on a bare binary:
/// spec -> `clojure.spec.gen.alpha` -> (`delay` + `require` + `resolve`)
/// -> the embedded `clojure.test.check.generators`. Its output is random,
/// so the assertion is on shape: five `[value conformed]` pairs, each of
/// which round-trips through the spec that generated it.
#[test]
fn spec_exercise_reaches_the_embedded_test_check() {
    on_big_stack(|| {
        let fx = Fixture::new("embedded-exercise");
        ok(
            &fx,
            "(ns main (:require [clojure.spec.alpha :as s]))\n\
             (let [xs (s/exercise int? 5)]\n\
             \x20 [(count xs)\n\
             \x20  (every? (fn [p] (and (vector? p) (= 2 (count p)))) xs)\n\
             \x20  (every? (fn [p] (int? (first p))) xs)\n\
             \x20  (every? (fn [p] (= (first p) (second p))) xs)])",
            "[5 true true true]",
        );
    });
}

/// SPEC-W3 task 4: the three constructs W2 had to leave UNTESTED --
/// `s/fspec`, `s/multi-spec` and `s/inst-in` -- all run end to end on a
/// bare binary now that W1's engine features (`reify
/// clojure.lang.ILookup`, `.dispatchFn`/`.getMethod`, `#inst` +
/// `inst-ms`) are merged. Every one of the three is upstream-verbatim in
/// the port: not one needed a `MOVA-PATCH`.
///
/// The oracle-diffed versions live in `tests/spec-smoke/smoke.mova`
/// sections 9-11; this is the `cargo test`-resident check, so a
/// regression fails the build instead of waiting for a manual corpus run.
#[test]
fn fspec_runs_conform_valid_and_the_ilookup_lookup() {
    on_big_stack(|| {
        let fx = Fixture::new("embedded-fspec");
        // `s/valid?` on a fn drives `validate-fn`, which GENERATES
        // arguments and reads `(instance? Throwable ret)` on the result;
        // `(:args ..)` goes through `fspec-impl`'s ILookup reify, on a
        // value `with-name` has wrapped in metadata.
        // `*fspec-iterations*` is upstream's own var, bound down from 21
        // only for speed.
        ok(
            &fx,
            "(ns main (:require [clojure.spec.alpha :as s]))\n\
             (s/def ::inc-like (s/fspec :args (s/cat :x int?) :ret int?))\n\
             (binding [s/*fspec-iterations* 5]\n\
             \x20 [(s/valid? ::inc-like inc)\n\
             \x20  (s/valid? ::inc-like str)\n\
             \x20  (identical? inc (s/conform ::inc-like inc))\n\
             \x20  (s/describe ::inc-like)\n\
             \x20  (s/form (:args (s/get-spec ::inc-like)))\n\
             \x20  (:fn (s/get-spec ::inc-like))])",
            "[true false true (fspec :args (cat :x int?) :ret int? :fn nil) \
             (clojure.spec.alpha/cat :x clojure.core/int?) nil]",
        );
    });
}

/// `s/multi-spec`: upstream spells the dispatch lookup `(.getMethod mm
/// ((.dispatchFn mm) x))`, which W1 made resolvable as interop.
/// Conform, explain and the generator all go through it.
#[test]
fn multi_spec_runs_conform_explain_and_gen() {
    on_big_stack(|| {
        let fx = Fixture::new("embedded-multispec");
        ok(
            &fx,
            "(ns main (:require [clojure.spec.alpha :as s]))\n\
             (s/def :ev/type keyword?)\n\
             (s/def :ev/q string?)\n\
             (defmulti ev :ev/type)\n\
             (defmethod ev :ev/search [_] (s/keys :req [:ev/type :ev/q]))\n\
             (s/def ::event (s/multi-spec ev :ev/type))\n\
             [(s/describe ::event)\n\
             \x20(s/valid? ::event {:ev/type :ev/search :ev/q \"hi\"})\n\
             \x20(s/valid? ::event {:ev/type :ev/search})\n\
             \x20(s/valid? ::event {:ev/type :ev/nope})\n\
             \x20(every? (fn [v] (s/valid? ::event v))\n\
             \x20        (clojure.spec.gen.alpha/sample (s/gen ::event) 10))]",
            "[(multi-spec ev :ev/type) true false false true]",
        );
    });
}

/// `s/inst-in` / `s/inst-in-range?`: W2 could not run these for want of
/// `clojure.core/inst-ms`, which W1 added along with reading `#inst` into
/// a real `java.util.Date`. Conform hands the instant straight back, and
/// every generated sample lands inside the range.
#[test]
fn inst_in_conforms_describes_and_generates() {
    on_big_stack(|| {
        let fx = Fixture::new("embedded-instin");
        ok(
            &fx,
            "(ns main (:require [clojure.spec.alpha :as s]))\n\
             (s/def ::the90s (s/inst-in #inst \"1990\" #inst \"2000\"))\n\
             [(s/valid? ::the90s #inst \"1995-06-01\")\n\
             \x20(s/valid? ::the90s #inst \"2000\")\n\
             \x20(s/valid? ::the90s \"1995\")\n\
             \x20(s/conform ::the90s #inst \"1995-06-01\")\n\
             \x20(s/inst-in-range? #inst \"1990\" #inst \"2000\" #inst \"1995\")\n\
             \x20(every? (fn [v] (and (inst? v) (s/valid? ::the90s v)))\n\
             \x20        (clojure.spec.gen.alpha/sample (s/gen ::the90s) 10))]",
            "[true false false #inst \"1995-06-01T00:00:00.000-00:00\" true true]",
        );
    });
}

/// SPEC-W3 (defect ledger item 10): `clojure.core/resolve` reads the
/// DYNAMIC `*ns*`, because upstream is literally `(defn resolve [sym]
/// (ns-resolve *ns* sym))` -- a runtime function reading a runtime var.
///
/// mova resolved against the LEXICAL `current_ns` (the defining
/// namespace of whatever fn was running), which differs from `*ns*`
/// exactly when a fn body runs with `*ns*` pointing elsewhere. `require
/// :as` writes its alias to the DYNAMIC namespace (`clojure.core/alias`
/// is `(.addAlias *ns* ..)`), so `resolve` had to read the same table to
/// find it -- vendored `ns_libs.clj`'s `require-as-alias` is the measured
/// case, worth 3 census assertions.
///
/// `ns-resolve` is unaffected: it is handed the namespace to look in.
/// Everything else -- `expand_alias`, `refer_source`, ordinary symbol
/// resolution in both tiers -- still reads LEXICALLY, which is the
/// compile-time question and a different one.
#[test]
fn resolve_reads_the_dynamic_ns_not_the_defining_one() {
    let fx = Fixture::new("resolve-dynamic");
    fx.file(
        "app/lib.mova",
        "(ns app.lib)\n(def marker :from-lib)\n\
         ;; `look` is DEFINED in app.lib, so its lexical ns is app.lib --\n\
         ;; but it must resolve against whatever *ns* is when it RUNS.\n\
         (defn look [s] (resolve s))\n",
    );
    // The alias `L` exists only in `main`. Calling `app.lib/look` from
    // `main` must still see it, because `*ns*` is `main` for the call.
    ok(
        &fx,
        "(ns main (:require [app.lib :as L]))\n(app.lib/look 'L/marker)",
        "#'app.lib/marker",
    );
    // ... and the same call under an explicit `binding` of `*ns*` follows
    // the binding, which is the whole point of it being dynamic.
    ok(
        &fx,
        "(ns main2 (:require [app.lib :as L]))\n\
         (binding [*ns* (the-ns 'app.lib)] (app.lib/look 'L/marker))",
        "nil",
    );
    // `ns-resolve` keeps taking the namespace it is given.
    ok(
        &fx,
        "(ns main3 (:require [app.lib :as L]))\n\
         [(ns-resolve 'app.lib 'marker) (ns-resolve 'main3 'marker)]",
        "[#'app.lib/marker nil]",
    );
}
