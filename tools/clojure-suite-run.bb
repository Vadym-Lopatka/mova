#!/usr/bin/env bb
;; tools/clojure-suite-run.bb
;;
;; Implementation for tools/clojure-suite-run.sh (kept as babashka rather
;; than bash+grep/sed per this repo's own convention: EDN in, EDN out, no
;; fragile text parsing). For every vendored Clojure test file, this:
;;   1. injects the mova clojure.test shim's source immediately AFTER the
;;      file's own leading `(ns ...)` form (see "why the shim is injected
;;      after ns" below), producing shim + test body in ONE namespace,
;;      plus a trailing `(run-tests)` call, into a temp file under the
;;      scratchpad -- the vendored file's own bytes are never edited, only
;;      the temp copy's assembly order changes,
;;   2. runs it through mova with a hard wall-clock timeout (mova has a
;;      known infinite-hang bug on non-tail `recur` -- a hung file must be
;;      recorded as :timeout, never allowed to wedge the whole run),
;;   3. parses the shim's `#RESULT {...}` / `#SUMMARY {...}` lines (which
;;      are themselves valid EDN after stripping the leading marker) out
;;      of stdout,
;;   4. records one of three file-level outcomes:
;;        :ok      -- mova ran to completion and printed a #SUMMARY line
;;                    (individual deftests inside may still have failed --
;;                    that's captured in the file's own pass/fail/error
;;                    counts, not the file-level status)
;;        :blocked -- mova exited (crashed, unresolved symbol, reader
;;                    error, etc.) before ever printing a #SUMMARY line;
;;                    the first non-blank line of stderr (falling back to
;;                    stdout) is captured VERBATIM as :first-error
;;        :timeout -- killed by the wall-clock timeout
;;
;; Output: tests/clojure-suite/scoreboard.edn -- one map, sorted by file
;; name, diffable across runs.
;;
;; Before any of the above, EVERY vendored file is also materialized as a
;; loadable module under the module path its OWN `(ns ...)` form implies
;; (see `materialize-vendored-companion` below for the mechanics and the
;; shim-splice decision) -- this is what lets one vendored file's
;; `:require` of another vendored file's namespace (a "companion
;; namespace", e.g. `protocols.clj` requiring
;; `clojure.test-clojure.protocols.more-examples`) actually resolve.
;;
;; WHY THE SHIM IS INJECTED AFTER `ns`, NOT LOADED BEFORE IT (load-bearing,
;; and worth spelling out -- this replaced an earlier elide-the-ns-form
;; design after a correctness review):
;;
;; confirmed by hand-probing mova before writing the original version of
;; this script -- mova's `(ns ...)` is not a no-op namespace declaration,
;; it actually SWITCHES to a new, isolated namespace, and top-level
;; defs/defmacros loaded BEFORE that switch (i.e. the shim, if
;; concatenated ahead of an unmodified test file) become unresolved
;; afterward. Nor is `(:require [clojure.test :refer [deftest is ...]])`
;; a working substitute in that "shim first" ordering: mova's
;; macroexpansion is not namespace-hygienic, so even when `deftest` itself
;; resolves cross-namespace, its expansion references the shim's OWN
;; internal helper symbols (`ts-registry`, `ts-report-pass`, etc)
;; unqualified, and those fail to resolve from the caller's namespace
;; ("Unable to resolve symbol: ts-registry").
;;
;; The fix is ordering, not deletion: load the shim's source AFTER the
;; vendored file's `ns` form has already run and switched namespace, so
;; the shim's defs/defmacros land in the SAME namespace as the test body
;; that follows -- no cross-namespace macro hygiene is ever exercised, and
;; the file's own `:require`/`:as` clauses (e.g. `[clojure.set :as set]`)
;; stay live for the whole file, because `ns` ran first and unlike the
;; earlier elide-the-form design, nothing removes it. Spot-checked against
;; `clojure_set.clj`: `set/union`, `set/difference` etc. resolve and run
;; for real under this ordering, where the elide-first design would have
;; reported "unresolved symbol: set" regardless of whether mova's
;; `clojure.set` support was actually there or not -- a false negative
;; that taught nothing. Where a `:require` genuinely fails under mova
;; (e.g. no real `clojure.set` module, or `:refer :all` rejected), the
;; file still blocks -- but now that block is attributable to a real
;; language gap, which is the honest, useful signal this whole system
;; exists to produce.

(require '[babashka.process :as p]
         '[clojure.edn :as edn]
         '[clojure.string :as str]
         '[clojure.java.io :as io]
         '[clojure.pprint :as pprint])

(def root (-> *file* io/file .getParentFile .getParentFile .getCanonicalPath))
(def suite-dir (str root "/tests/clojure-suite"))
(def vendor-dir (str suite-dir "/vendor"))
(def vendor-libs-dir (str suite-dir "/vendor-libs"))
(def shim-path (str suite-dir "/mova-test-shim.mova"))
(def helper-shim-path (str suite-dir "/mova-test-helper-shim.mova"))
(def stacktrace-companion-path (str suite-dir "/mova-stacktrace-companion.mova"))
(def edn-companion-path (str suite-dir "/mova-edn-companion.mova"))
(def data-companion-path (str suite-dir "/mova-data-companion.mova"))
(def version-path (str root "/tests/conformance/CLOJURE_VERSION"))
(def mova-bin (or (System/getenv "MOVA_BIN") (str root "/target/release/mova")))
(def timeout-secs (or (some-> (System/getenv "CLOJURE_SUITE_TIMEOUT") parse-long) 8))
(def scratch-dir
  ;; Default is PER-CHECKOUT (suffixed with a hash of this repo root's
  ;; absolute path), not a single shared directory. W4 (2026-08-21)
  ;; measured the shared-default failure mode this prevents: two
  ;; checkouts' suite runs racing on one scratch dir, the second run's
  ;; materialization overwriting companion modules under the first
  ;; mid-run (a stale-runner run re-polluted repl/example.mova while a
  ;; newer-runner gate was scoring repl.clj, silently costing it 2
  ;; assertions). Worktree agents and CI can still pin an explicit dir
  ;; via CLOJURE_SUITE_SCRATCH; same-checkout reruns still reuse their
  ;; own dir (the hash is stable per path), so nothing is rebuilt
  ;; needlessly.
  (or (System/getenv "CLOJURE_SUITE_SCRATCH")
      (str (System/getProperty "java.io.tmpdir")
           "/clojure-suite-run-"
           (format "%08x" (hash root)))))

(defn slurp-trim [path]
  (str/trim (slurp path)))

;; Skip past zero or more stacked `^meta` forms (a balanced `{...}`/
;; `[...]`/`(...)` reader form, or a bare `^:keyword`/`^symbol`/`^"str"`
;; shorthand) immediately following index `i` in `s`, returning the index
;; where the next real token starts. String-literal-aware -- a `}`/`)`/
;; `]` inside a doc string's prose (e.g. vendor-libs/clojure/walk.clj's
;; multi-paragraph docstring) can't desync the brace count. Deliberately
;; dumb/auditable (character-scan, not a real reader) in the same spirit
;; as `inject-shim-after-ns` below, not a general reader-form skipper --
;; sufficient for the ns-metadata shapes actually vendored.
(defn- skip-balanced-form [s i]
  (let [c (.charAt s i)]
    (cond
      (contains? #{\{ \( \[} c)
      (let [close ({\{ \} \( \) \[ \]} c)]
        (loop [j (inc i) depth 1 in-str? false]
          (cond
            (>= j (count s)) j
            in-str? (let [cj (.charAt s j)]
                      (cond
                        (= cj \\) (recur (+ j 2) depth true)
                        (= cj \") (recur (inc j) depth false)
                        :else (recur (inc j) depth true)))
            :else (let [cj (.charAt s j)]
                    (cond
                      (= cj \") (recur (inc j) depth true)
                      (= cj c) (recur (inc j) (inc depth) false)
                      (= cj close) (if (= depth 1) (inc j) (recur (inc j) (dec depth) false))
                      :else (recur (inc j) depth false))))))
      (= c \")
      (loop [j (inc i)]
        (cond
          (>= j (count s)) j
          (= (.charAt s j) \\) (recur (+ j 2))
          (= (.charAt s j) \") (inc j)
          :else (recur (inc j))))
      :else
      (loop [j i]
        (if (or (>= j (count s))
                (Character/isWhitespace (.charAt s j))
                (contains? #{\( \) \[ \] \{ \}} (.charAt s j)))
          j
          (recur (inc j)))))))

(defn- skip-ws [s i]
  (loop [i i]
    (if (and (< i (count s)) (Character/isWhitespace (.charAt s i)))
      (recur (inc i))
      i)))

(defn- skip-ns-metadata [s i]
  (loop [i (skip-ws s i)]
    (if (and (< i (count s)) (= (.charAt s i) \^))
      (recur (skip-ws s (skip-balanced-form s (inc i))))
      i)))

;; The namespace a vendored file's own leading `(ns ...)` form declares.
;; Used for two things: the module path its companion copy is
;; materialized under, and -- see `run-one` -- the namespace the trailing
;; `(run-tests)` is made to execute in.
;;
;; Skips ns-attached metadata (`(ns ^{:author ... :doc ...} the.actual.ns)`)
;; before applying the symbol regex -- none of the 51 vendor/ files use
;; this shape (verified: `grep -l '^(ns \^' vendor/*.clj` is empty), so
;; this is pure added capability, not a behavior change, for those files.
;; 5 of vendor-libs/'s 14 files DO use it (random.clj, results.cljc,
;; data/generators.clj, walk.clj, zip.clj) and would otherwise silently
;; fail to materialize (nil ns-name -> materialize fn no-ops).
(defn declared-ns [src]
  (some->> (str/index-of src "(ns ")
           (+ 4)
           (skip-ns-metadata src)
           (subs src)
           (re-find #"^\s*([a-zA-Z0-9_.\-]+)")
           second))

;; Inject `shim-src` immediately after the vendored file's leading
;; `(ns ...)` top-level form (see module-doc above for why). Finds the
;; first "(ns " and paren-balances forward to its matching close paren --
;; deliberately dumb/auditable rather than a real reader, which is safe
;; here because every vendored file's first `(ns ...)` is the genuine one
;; (nothing upstream of it in these files contains a string/regex literal
;; with a stray "(ns " substring; spot-checked). The vendored file's own
;; text is never altered -- this only decides where in the ASSEMBLED temp
;; copy the shim's source gets spliced in.
(defn inject-shim-after-ns [test-src shim-src]
  (let [start (str/index-of test-src "(ns ")]
    (if (nil? start)
      ;; No `ns` form found (shouldn't happen for these vendored files,
      ;; but stay correct if it ever does) -- fall back to shim-first.
      (str shim-src "\n\n" test-src)
      (loop [i start depth 0]
        (if (>= i (count test-src))
          (str shim-src "\n\n" test-src) ; unbalanced -- bail out, shim-first fallback
          (let [c (.charAt test-src i)
                depth' (cond (= c \() (inc depth)
                             (= c \)) (dec depth)
                             :else depth)]
            (if (and (= c \)) (zero? depth'))
              (str (subs test-src 0 (inc i))
                   "\n\n;; ==================== mova clojure.test shim, injected by "
                   "tools/clojure-suite-run.bb after the ns form above ====================\n\n"
                   shim-src
                   "\n\n;; ==================== rest of vendored test file ====================\n\n"
                   (subs test-src (inc i)))
              (recur (inc i) depth'))))))))

;; mova's own error output is a multi-line boxed diagnostic (error class
;; on the first line, e.g. "reader error", then a `x <specific message>`
;; line with source location) -- the specific message and location is the
;; actually valuable part, not just the first line's generic class, so
;; capture the whole block (bounded, so one pathological error can't blow
;; up scoreboard.edn).
(def max-error-lines 20)

(defn error-detail [s]
  (let [trimmed (str/trim (or s ""))]
    (if (empty? trimmed)
      nil
      (let [lines (str/split-lines trimmed)]
        (str/join "\n" (take max-error-lines lines))))))

(defn run-one [file]
  (let [name (.getName (io/file file))
        tmp (str scratch-dir "/" name)
        ;; Both shims are spliced into the SAME namespace, immediately
        ;; after the vendored file's own `ns` form -- same reasoning as
        ;; the clojure.test shim alone (see module doc above): a bare
        ;; `(:use clojure.test-helper)` or an unqualified `:refer` needs
        ;; test-helper's names to live in the test body's own namespace,
        ;; not a separate required one. Order (clojure.test shim first,
        ;; then test-helper) doesn't matter -- the two shims define
        ;; disjoint names.
        shim-src (str (slurp shim-path) "\n\n" (slurp helper-shim-path))
        src (slurp file)
        assembled (inject-shim-after-ns src shim-src)
        ;; Re-enter the file's OWN declared namespace before running, so
        ;; the trailing `(run-tests)` reads the registry the spliced shim
        ;; built there. Load-bearing for any vendored file that switches
        ;; namespace part-way through: `protocols.clj` declares
        ;; `clojure.test-clojure.protocols`, defines 23 deftests in it,
        ;; then does a mid-file `(ns clojure.test-clojure.protocols.other
        ;; (:use clojure.test))` and defines 2 more there. Without this
        ;; re-entry, `(run-tests)` runs in `...protocols.other` against
        ;; the `clojure.test` module's registry and sees only those last
        ;; 2, silently hiding the other 23.
        ;;
        ;; This MATCHES the ground-truth oracle rather than diverging from
        ;; it: tools/oracle-census.clj runs `(clojure.test/run-tests
        ;; ns-sym)` with the file's own declared namespace, which is
        ;; exactly why ORACLE-ASSERTIONS.edn records 23 deftests for
        ;; protocols.clj and not the 25 `(deftest` forms the file's text
        ;; contains. Numerator and denominator now count the same set.
        ;;
        ;; `(ns X)` on an already-current/already-existing namespace is a
        ;; no-op switch in mova (it `entry(..).or_default()`s, keeping
        ;; every def/alias/refer already there), so for the 50 files that
        ;; never switch namespace this appends a form that changes
        ;; nothing -- verified: no other file's scoreboard row moves.
        ;;
        ;; field2/W-NS (2026-08-22): the re-entry above puts the LEXICAL
        ;; namespace back, and the trailing call is now wrapped in an
        ;; immediately-invoked `(fn [] (ns user) (run-tests))` so that the
        ;; tests execute with the DYNAMIC `*ns*` = `user`, which is the
        ;; shape the ground-truth oracle harness actually has:
        ;; `tools/oracle-census.clj` does `(require ns-sym)` -- real
        ;; `Compiler.load` restores `*ns*` when it returns -- and only THEN
        ;; `(clojure.test/run-tests ns-sym)`, from the process's `user`
        ;; namespace, so a deftest BODY runs at `*ns*` = user while its
        ;; symbols were already resolved against the file's own ns.
        ;;
        ;; The wrapper is the whole mechanism: mova's `ns`/`in-ns` inside a
        ;; running fn body now moves ONLY the dynamic `*ns*` (src/ns.rs's
        ;; `Interp::switch_ns`, field2/W-NS), so `(ns user)` here does not
        ;; disturb the anonymous fn's own lexical namespace -- the trailing
        ;; `(run-tests)` still resolves in `run-ns`, exactly as before,
        ;; while everything it calls sees `*ns*` = user. Before that
        ;; interpreter split this wrapper was impossible, which is the only
        ;; reason the runner ever diverged from the oracle here (see
        ;; tests/conformance/DEVIATIONS.md's "W4 close" root 3 and
        ;; compat/w4c-ns-libs-honest-miss.txt).
        run-ns (declared-ns src)
        ;; W-DECL: the field2/W-NS `declare` workaround this preamble used
        ;; to install into `user` (redefining `declare` to `def` each name
        ;; to `nil`, VERBATIM mirroring mova-test-shim.mova's OWN former
        ;; shadow) is gone -- both engine defects it worked around are
        ;; fixed (core/core.mova's `declare` doc comment; `eval::
        ;; special_forms::check_dynamic_or_err`), and mova-test-shim.mova
        ;; no longer shadows `declare` either (see that file's own
        ;; comment), so `user` needs no separate patch: `def.clj`'s
        ;; `nested-dynamic-declaration` (the one vendored test that
        ;; `eval`s a `declare`-then-`binding` program in `user`, per the
        ;; field2/W-NS namespace split above) now reaches the SAME real
        ;; `declare` core.mova defines everywhere else.
        user-preamble ""]
    (io/make-parents tmp)
    (spit tmp (str ";; ==================== vendored test file: " name " (shim injected after its ns form) ====================\n\n"
                   assembled
                   user-preamble
                   (if run-ns (str "\n\n(ns " run-ns ")\n") "\n")
                   "\n((fn [] (ns user) (run-tests)))\n"))
    (let [{:keys [out err exit timeout]}
          (try
            (let [proc (p/process {:out :string :err :string :continue true}
                                  "timeout" (str timeout-secs "s") mova-bin tmp)
                  result @proc]
              {:out (:out result) :err (:err result) :exit (:exit result) :timeout (= 124 (:exit result))})
            (catch Exception e
              {:out "" :err (str "runner exception: " (ex-message e)) :exit -1 :timeout false}))
          summary-line (->> (str/split-lines (or out ""))
                            (filter #(str/starts-with? % "#SUMMARY "))
                            last)]
      (cond
        timeout
        {:file name :status :timeout :tests 0 :pass 0 :fail 0 :error 0
         :assertions 0 :assertions-passed 0
         :first-error (str "timed out after " timeout-secs "s")}

        summary-line
        (let [summary (edn/read-string (subs summary-line (count "#SUMMARY ")))]
          {:file name :status :ok
           :tests (:tests summary 0) :pass (:pass summary 0) :fail (:fail summary 0)
           :error (:error summary 0) :assertions (:assertions summary 0)
           :assertions-passed (:assertions-passed summary 0)
           :first-error nil})

        :else
        (let [first-err (or (error-detail err) (error-detail out) "(no output captured, and exit was clean -- shim likely emitted nothing, e.g. zero deftests in file)")]
          {:file name :status :blocked :tests 0 :pass 0 :fail 0 :error 0
           :assertions 0 :assertions-passed 0
           :first-error first-err})))))

;; ---------- ground-truth oracle census (tests/clojure-suite/ORACLE-ASSERTIONS.edn) ----------
;;
;; Written by tools/oracle-census.sh/.clj: real Clojure 1.13.0-alpha6,
;; real clojure.test, run per-file, independent of what mova does. This
;; is the North Star's DENOMINATOR -- see that file's own header comment
;; for the full rationale of why the denominator must not come from
;; mova's own run (it would make the headline percentage non-monotone).
;;
;; MUST degrade gracefully: this suite run must never hard-depend on the
;; JVM/oracle being installed (CONFORMANCE-GUARANTEE.md promises the
;; committed scoreboard.edn works without one), so a missing or unreadable
;; ORACLE-ASSERTIONS.edn is not an error here -- every oracle-* value
;; below just comes back nil / :oracle-census-present false.
(def oracle-assertions-path (str suite-dir "/ORACLE-ASSERTIONS.edn"))

(def oracle-census-data
  (when (.exists (io/file oracle-assertions-path))
    (try
      (edn/read-string (slurp oracle-assertions-path))
      (catch Exception _e nil))))

(def oracle-census-present (boolean oracle-census-data))
(def oracle-census-files (:files oracle-census-data {}))
(def oracle-census-totals (:totals oracle-census-data {}))

(defn oracle-lookup
  "Per-file :oracle-assertions/:oracle-deftests to merge into a
   scoreboard file entry -- both nil when the census is absent or this
   particular file has no entry in it (e.g. a file vendored after the
   last census run)."
  [file]
  (let [entry (get oracle-census-files file)]
    {:oracle-assertions (:assertions entry)
     :oracle-deftests (:deftests entry)}))

;; Materialize the shim ALSO as a loadable `clojure/test.mova` on the
;; scratch dir (which doubles as mova's module path for the assembled temp
;; file). Six vendored files say `(:require [clojure.test :refer :all])`
;; rather than `(:use clojure.test)` -- `:use` is a tolerated-ignored ns
;; clause (the spliced shim provides the names), but `:require` genuinely
;; resolves against the module path and used to kill those six files with
;; "could not locate namespace clojure.test". The materialized copy exists
;; to satisfy that resolution; the SPLICED copy still shadows every
;; referred name inside the assembled file (splice comes after the ns
;; form), so the deftest registry stays the spliced shim's -- one
;; registry, no split-brain. None of the vendored files aliases
;; clojure.test (`:as`), which is what would bypass the shadowing;
;; verified 2026-08-20 across all 48 vendored files, and worth re-checking
;; if the vendored set is ever re-pinned. Disclosed in SHIM-LIMITS.md.
(defn materialize-shim-as-clojure-test []
  (let [target (io/file scratch-dir "clojure" "test.mova")]
    (io/make-parents target)
    (spit target (str "(ns clojure.test)\n\n" (slurp shim-path)))))

;; Same trick, same reason, for `clojure.test-helper` (mova-test-helper-
;; shim.mova materialized as clojure/test_helper.mova): several vendored
;; files `:require` it (some with `:refer [...]`, one -- numbers.clj --
;; with `:as helper`), and `:require` genuinely resolves against the
;; module path regardless of whether `:use`'s tolerated-ignored clause is
;; also present elsewhere in the same `ns` form. The SPLICED copy (see
;; run-one above) is still what actually backs every unqualified/`:refer`
;; call, for the same one-registry-no-split-brain reason as clojure.test;
;; this materialized copy exists purely so the `:require` itself resolves,
;; and additionally gives numbers.clj's `helper/`-qualified calls (`:as
;; helper`) somewhere real to resolve against -- unlike clojure.test's own
;; `:as` case (documented as a theoretical split-brain risk there because
;; no vendored file does it), this one is REAL and exercised: numbers.clj
;; calls `helper/with-err-string-writer` and `helper/eval-in-temp-ns`
;; through the alias. Neither is defined in this shim (see mova-test-
;; helper-shim.mova's own header for why -- no `*err*`, no `in-ns`), so
;; those specific qualified calls will honestly error as unresolved
;; symbols when actually invoked, not silently do nothing.
;;
;; W3e-1: this materialized copy carries BOTH shims, in the same order the
;; spliced per-file copy uses (`run-one` above), not the helper shim alone.
;; mova-test-helper-shim.mova says so in its own header -- its
;; `with-err-string-writer` is "built on `*err*`/`ts-err-write`
;; (mova-test-shim.mova ...)", i.e. names that live in the OTHER shim --
;; and its `with-err-string-writer` comment spells out that it used to
;; reach them by relying on "mova's macroexpansion being non-hygienic
;; across namespaces": the expansion's bare `*err*` resolved against
;; whatever namespace the macro was CALLED from. Once syntax-quote started
;; namespace-qualifying its free symbols (real Clojure's behaviour, see
;; src/eval/quasiquote.rs), that expansion correctly names
;; `clojure.test-helper/*err*` -- and this materialized namespace, built
;; from the helper shim alone, was the one place where that var genuinely
;; did not exist, so numbers.clj's `helper/with-err-string-writer` calls
;; (the only alias-qualified shim calls in the corpus) went from honest
;; failures to "Unable to resolve symbol". Including both shims here makes
;; the materialized namespace contain exactly what the spliced one does,
;; which is the invariant the helper shim was written against.
(defn materialize-shim-as-clojure-test-helper []
  (let [target (io/file scratch-dir "clojure" "test_helper.mova")]
    (io/make-parents target)
    (spit target (str "(ns clojure.test-helper)\n\n"
                      (slurp shim-path) "\n\n" (slurp helper-shim-path)))))

;; S5: same trick as the two shims above, for `clojure.stacktrace`.
;; `test.clj` (`vendor/test.clj`) does `(:require [clojure.stacktrace :as
;; stack])`, which used to fail outright with "could not locate namespace
;; clojure.stacktrace" -- unlike `clojure.test`/`clojure.test-helper`,
;; there is no vendored `.clj` file for this namespace at all (it isn't
;; part of the corpus), so it can't be reached via
;; `materialize-vendored-companion` below; it needs its own hand-written
;; mova source (`mova-stacktrace-companion.mova`, see that file's own
;; header for what it can and can't faithfully reimplement without a real
;; `Throwable` object model) materialized the same way as the two shims.
;; Not spliced into anything -- `stacktrace.mova` is a plain, self-
;; contained module (no shim names referenced), so it needs only the `ns`
;; wrapper, same shape as `materialize-shim-as-clojure-test` above.
(defn materialize-companion-clojure-stacktrace []
  (let [target (io/file scratch-dir "clojure" "stacktrace.mova")]
    (io/make-parents target)
    (spit target (str "(ns clojure.stacktrace)\n\n" (slurp stacktrace-companion-path)))))

;; C3: same trick as the stacktrace companion above, for `clojure.edn`.
;; `edn.clj` (`vendor/edn.clj`, the ONLY vendored consumer -- confirmed by
;; `grep -rn 'edn/' vendor/`) does `(:require [clojure.edn :as edn])`,
;; which used to fail outright with "could not locate namespace
;; clojure.edn" -- like `clojure.stacktrace`, there is no vendored `.clj`
;; file for this namespace (it isn't part of the corpus), so it needs its
;; own hand-written mova source (`mova-edn-companion.mova`, see that
;; file's own header for its API surface and every measured divergence
;; from real `clojure.edn`) materialized the same way. Not spliced into
;; anything -- `edn.mova` is a plain, self-contained module (no shim names
;; referenced), so it needs only the `ns` wrapper, same shape as
;; `materialize-companion-clojure-stacktrace` above.
(defn materialize-companion-clojure-edn []
  (let [target (io/file scratch-dir "clojure" "edn.mova")]
    (io/make-parents target)
    (spit target (str "(ns clojure.edn)\n\n" (slurp edn-companion-path)))))

;; Wave-C small sweep item 2: same trick as the edn/stacktrace companions
;; above, for `clojure.data`. `data.clj` (`vendor/data.clj`, the ONLY
;; vendored consumer) does `(:use clojure.data clojure.test)`, which used
;; to fail outright with "could not locate namespace clojure.data" -- like
;; `clojure.edn`/`clojure.stacktrace`, there is no vendored `.clj` file for
;; this namespace. Unlike those two, this ISN'T because no upstream source
;; exists (`clojure/data.clj` IS pure Clojure, no JVM-only imports beyond
;; `clojure.set`) -- byte-identical vendoring was tried first and measured
;; to fail structurally, not just on a missing symbol: upstream dispatches
;; `equality-partition`/`diff-similar` via `extend-protocol` on
;; `java.util.Set`/`List`/`Map`, and mova's protocol dispatch is
;; EXACT-CLASS-only (no interface ancestor walk -- see `mova-data-
;; companion.mova`'s own header for the full measured chain). So, same as
;; edn/stacktrace, this is a hand-written mova workalike materialized the
;; same way.
(defn materialize-companion-clojure-data []
  (let [target (io/file scratch-dir "clojure" "data.mova")]
    (io/make-parents target)
    (spit target (str "(ns clojure.data)\n\n" (slurp data-companion-path)))))

;; Materialize EVERY vendored file, additionally, as a loadable module
;; under the module path its OWN `(ns ...)` form implies -- e.g.
;; vendor/protocols_examples.clj declares `(ns
;; clojure.test-clojure.protocols.examples)`, so it also gets written to
;; <scratch>/clojure/test_clojure/protocols/examples.mova (dots -> dirs,
;; dashes -> underscores, same convention `run-one`'s temp filename
;; already implies and the reader error messages already show, e.g.
;; "no clojure/test_clojure/protocols/more_examples.mova"). This is what
;; actually unblocks companion-namespace files: `protocols.clj`
;; `:require`s `clojure.test-clojure.protocols.more-examples`,
;; `run_single_test.clj` `:require`s `clojure.test-clojure.test-fixtures`,
;; and `data_structures.clj` `:require`s `clojure.test-clojure.generators`
;; -- all three are genuinely vendored files (`protocols_more_examples.clj`,
;; `test_fixtures.clj`, `generators.clj`), just not previously reachable
;; by module-path resolution because only the two hand-written shims got
;; this treatment. Doing it uniformly for all 51 vendored files (not just
;; the ones known today to be required by another file) means a future
;; vendored companion "just works" without a runner change.
;;
;; WHETHER THE SHIM IS SPLICED INTO THESE MATERIALIZED COPIES (measured
;; empirically, not assumed, by running `protocols.clj` and
;; `run_single_test.clj` through the runner before/after each variant):
;; CONDITIONALLY, per `needs-shim?` below -- revised W4-SHIM (2026-08-21),
;; superseding the "splice unconditionally, it's a harmless no-op" call
;; this comment used to make. Several companion files are not passive
;; data/protocol modules, they themselves use `clojure.test` shim names at
;; the top level. `test_fixtures.clj` (companion of `run_single_test.clj`,
;; required `:as tf`) is the proof case -- its body is `(deftest
;; can-use-once-fixtures ...)` etc, i.e. it calls the shim's `deftest`
;; macro merely to be LOADED (a `deftest` form has to expand and compile
;; to define the var `tf/can-use-once-fixtures` that `run_single_test.clj`
;; then references as `(run-test tf/can-use-once-fixtures)`); without the
;; shim spliced in, loading this materialized copy fails outright with
;; "Unable to resolve symbol: deftest" and `run_single_test.clj` stays
;; blocked one require deeper than before -- `needs-shim?` must keep
;; splicing for this file, and does (it contains `deftest`).
;;
;; It is NOT a harmless no-op for every other companion, though, which is
;; what this comment used to claim and what W4-SHIM measured false:
;; `repl_example.clj` (companion of `repl.clj`, no `clojure.test` names at
;; all -- plain `(ns ...)` plus two `defn`s) got the shim spliced in
;; anyway under the old unconditional rule, which makes every one of the
;; shim's ~50 top-level def/defmacro names a PUBLIC var in
;; `clojure.test-clojure.repl.example`'s materialized namespace (mova's
;; `defn`/`def` are public unless `defn-`) -- names the vendored source
;; never declared. `repl.clj`'s own `test-dir` deftest calls `(dir-fn
;; 'clojure.test-clojure.repl.example)`, which is exactly `(ns-publics
;; ...)` under the hood (`core/core.mova`'s `dir-fn`): the oracle's answer
;; is `[bar foo]` (the file's only two real defns), but the unconditional
;; splice made it 50+ names, all of them the shim's own internal
;; machinery. `protocols_examples.clj`/`protocols_more_examples.clj`
;; escape this (their `defprotocol`/`definterface` forms aren't the kind
;; of thing anything calls `ns-publics`/`dir-fn` on in this corpus,
;; verified by grep), so the old claim happened to score identically
;; there -- but "no test in this corpus happens to look" is not the same
;; claim as "harmless no-op", and `repl.clj` is the counter-example that
;; shows the difference. `needs-shim?` (below) is `false` for exactly this
;; one file among all 51 vendored (`grep -rL deftest
;; tests/clojure-suite/vendor/*.clj` -- confirmed the ONLY file with zero
;; `deftest` forms, and reading it directly confirms no other
;; `clojure.test` surface either), so this fix changes treatment for
;; `repl_example.clj` alone: it still materializes (so `repl.clj`'s
;; `:use` of it keeps resolving), just without the shim splice.
;;
;; WHAT IS DELIBERATELY *NOT* APPENDED, UNLIKE THE PRIMARY PER-FILE RUN:
;; a trailing `(run-tests)` call. `deftest` only DEFINES a test var (a
;; plain `def` of a 0-arg fn under the hood); it does not invoke it.
;; `require`-ing a companion module is therefore side-effect-free with
;; respect to its own `deftest` bodies -- their assertions never execute
;; just because another file required the namespace they live in, exactly
;; matching real Clojure/JVM `require` semantics for a namespace that
;; merely happens to contain `deftest` forms. Appending `run-tests` here
;; would silently double-run+double-count `test_fixtures.clj`'s own
;; assertions as an uncredited side effect of loading it as
;; `run_single_test.clj`'s companion, and would emit a second, spurious
;; `#SUMMARY` line into stdout that this script's `run-one` would then
;; have to sort out from the real one (it takes the LAST `#SUMMARY` line,
;; so it would happen to still pick the right one today, but only by
;; the accident of ordering -- not appending `run-tests` here removes the
;; whole risk class instead of relying on that accident).
;; W4-SHIM: `true` when `src` (a vendored companion's own text) needs the
;; `clojure.test` shim spliced in to even LOAD -- i.e. it calls a shim
;; macro/name at the top level merely to define its own vars (the
;; `test_fixtures.clj` case the big comment above walks through in
;; detail). `deftest` itself is the only such name any vendored companion
;; actually uses today (measured: `grep -lE '\(is |\(testing
;; |use-fixtures' tests/clojure-suite/vendor/*.clj` -- 38 files -- is a
;; strict subset of `grep -l deftest` on the same glob -- 39 files, the
;; one-file difference being `data.clj` (has `deftest`, but no separately
;; matching `is`/`testing`/`use-fixtures` call of its own) -- nothing in
;; this corpus calls an `is`/`testing`/`use-fixtures` form outside a
;; `deftest` body), so a plain substring check for `deftest` is exactly as
;; precise as scanning for the full shim surface would be, for every file
;; this corpus actually has. A future companion that used shim macros
;; WITHOUT ever writing `deftest` would need this check widened -- not
;; expected today, but worth naming so the next person doesn't have to
;; rediscover it.
(defn needs-shim? [src]
  (str/includes? src "deftest"))

(defn materialize-vendored-companion [file shim-src]
  (let [src (slurp file)
        ns-name (declared-ns src)]
    (when ns-name
      (let [path-part (-> ns-name
                          (str/replace "-" "_")
                          (str/replace "." "/"))
            rel-path (str path-part ".mova")
            target (io/file scratch-dir rel-path)
            assembled (if (needs-shim? src)
                        (inject-shim-after-ns src shim-src)
                        src)]
        (io/make-parents target)
        (spit target assembled)))))

;; ---------- vendor-libs materialization (Wave B: test.check,
;; test.generative, data.generators, clojure.walk/template/zip --
;; test-support LIBRARIES, not test files) ----------
;;
;; Every file under tests/clojure-suite/vendor-libs/ is materialized into
;; the scratch dir under the module path its OWN `(ns ...)` form declares
;; -- same `declared-ns` mechanism as `materialize-vendored-companion`
;; above -- with two differences from that function:
;;
;;   - .cljc files keep the .cljc extension (never renamed to .mova).
;;     This is LOAD-BEARING: mova's `require` tries `<ns>.mova` first,
;;     then falls back to `<ns>.cljc` (src/ns.rs), and ONLY a `.cljc`
;;     load gets `#?`/`#?@` reader-conditional dispatch turned ON
;;     (src/reader.rs's `allow_read_cond`) -- a bare `read-string`/`.mova`
;;     load leaves it off, matching the JVM's own split. test.check has
;;     ~74 `#?` sites across its .cljc files; materializing them as
;;     `.mova` would silently read every `#?(...)` form as a literal
;;     4-element list instead of dispatching on :clj/:default, corrupting
;;     each one instead of erroring loud.
;;   - .clj files materialize as .mova, identically to
;;     `materialize-vendored-companion`'s own .clj -> .mova handling.
;;
;; NO shim splicing: these are libraries, not test files -- none of the
;; 14 defines a `deftest`/`is` of its own (verified: `grep -l
;; 'deftest\|(is ' vendor-libs -r` is empty). Their own
;; `(:require [clojure.test ...])` (data.generators' dev-only test ns, if
;; ever exercised) resolves against the already-materialized
;; `clojure.test` shim module (`materialize-shim-as-clojure-test`, above)
;; exactly like any other `:require` -- no splice needed because nothing
;; here calls a shim macro merely to be loaded (contrast
;; `materialize-vendored-companion`'s `test_fixtures.clj` case, which
;; does).
;;
;; These files are NEVER added to `vendored` (below, in -main) -- that
;; list reads vendor-dir only, and stays that way on purpose -- so
;; vendor-libs files are materialized-only: never run as a suite file,
;; never given a `run-one` scoreboard row, never counted in
;; scoreboard.edn's totals.
;; D5: a THIRD difference from `materialize-vendored-companion`, added for
;; `clojure.pprint`: a vendor-libs file with NO `(ns ...)` form of its own
;; is materialized at its OWN PATH under vendor-libs/ instead of being
;; skipped.
;;
;; This is not a special case for pprint, it is how multi-file namespaces
;; are spelled in Clojure generally. `clojure/pprint.clj` is a 51-line `ns`
;; form followed by seven `(load "pprint/<part>")` calls, and each part
;; (`clojure/pprint/utilities.clj`, ...) opens with `(in-ns 'clojure.pprint)`
;; rather than an `ns` form -- so `declared-ns` returns nil for all seven
;; and the ns-derived path has nothing to derive from. Their real identity
;; is their PATH, which is exactly what `load` resolves against (see
;; `ns::Interp::load_path`), and vendor-libs/ already mirrors the upstream
;; source tree byte-for-byte -- so the file's own relative path IS the
;; module path, with no derivation needed.
;;
;; The two cases agree wherever both apply: `clojure/pprint.clj` declares
;; `clojure.pprint` AND lives at `clojure/pprint.clj`, so ns-derived and
;; path-derived produce the same `clojure/pprint.mova`. The ns form stays
;; authoritative when present, because a namespace is allowed to live at a
;; path that does not match its name and several vendored libs' `.cljc`
;; files rely on that.
(defn materialize-vendor-lib-file [file]
  (let [src (slurp file)
        ns-name (declared-ns src)
        cljc? (str/ends-with? (.getName (io/file file)) ".cljc")
        rel-under-vendor-libs (-> (.getCanonicalPath (io/file file))
                                  (str/replace-first
                                   (str (.getCanonicalPath (io/file vendor-libs-dir)) "/") "")
                                  (str/replace #"\.(clj|cljc)$" ""))
        path-part (if ns-name
                    (-> ns-name (str/replace "-" "_") (str/replace "." "/"))
                    rel-under-vendor-libs)
        rel-path (str path-part (if cljc? ".cljc" ".mova"))
        target (io/file scratch-dir rel-path)]
    (io/make-parents target)
    (spit target src)))

;; ---------- targeted runs (SPEC-W6b) ----------
;;
;; CLOJURE_SUITE_ONLY: a comma-separated list of vendored BASENAMES
;; (e.g. "spec.clj,instr.clj,multi_spec.clj"). When set, only those files
;; are RUN. Materialization is deliberately NOT filtered -- every vendored
;; file and every vendor-lib is still written into the scratch tree, because
;; a run file may `:require` a sibling (multi_spec.clj requires
;; clojure.test-clojure.spec, protocols.clj requires its two companions),
;; and a filtered materialization would turn a real dependency into a
;; missing-module error.
;;
;; A filtered run produces a PARTIAL scoreboard, which
;; tools/check-regression.sh would read as 48 vanished files, so the two
;; guards below are not optional politeness:
;;   * a filtered run REFUSES to write the committed
;;     tests/clojure-suite/scoreboard.edn -- CLOJURE_SUITE_OUT must name
;;     somewhere else;
;;   * the scoreboard it writes carries :partial-run true plus the filter
;;     that produced it, so nothing downstream can mistake it for a census.
;; CLOJURE_SUITE_OUT on its own (no filter) is a plain full run written
;; elsewhere, which is how a gate run can leave the committed scoreboard
;; untouched.
(def only-files
  (some->> (System/getenv "CLOJURE_SUITE_ONLY")
           (#(str/split % #","))
           (map str/trim)
           (remove str/blank?)
           set
           not-empty))

(def default-scoreboard-path (str suite-dir "/scoreboard.edn"))
(def scoreboard-out-path
  (or (System/getenv "CLOJURE_SUITE_OUT") default-scoreboard-path))

(defn -main []
  (when (and only-files (= scoreboard-out-path default-scoreboard-path))
    (binding [*out* *err*]
      (println (str "clojure-suite-run: FATAL -- CLOJURE_SUITE_ONLY is set, so this run scores only "
                    (count only-files) " of the vendored files. Writing that partial result to "
                    default-scoreboard-path " would look like every other file VANISHED to "
                    "tools/check-regression.sh. Set CLOJURE_SUITE_OUT to a scratch path.")))
    (System/exit 2))
  (materialize-shim-as-clojure-test)
  (materialize-shim-as-clojure-test-helper)
  (materialize-companion-clojure-stacktrace)
  (materialize-companion-clojure-edn)
  (materialize-companion-clojure-data)
  (let [vendor-libs (->> (file-seq (io/file vendor-libs-dir))
                         (filter #(and (.isFile %)
                                       (or (str/ends-with? (.getName %) ".clj")
                                           (str/ends-with? (.getName %) ".cljc"))))
                         (map str)
                         sort)
        _ (doseq [f vendor-libs] (materialize-vendor-lib-file f))
        vendored (->> (file-seq (io/file vendor-dir))
                      (filter #(str/ends-with? (.getName %) ".clj"))
                      (map str)
                      sort)
        combined-shim-src (str (slurp shim-path) "\n\n" (slurp helper-shim-path))
        ;; Materialize EVERY vendored file (see the note above), then run
        ;; only the selected ones.
        _ (doseq [f vendored] (materialize-vendored-companion f combined-shim-src))
        to-run (if only-files
                 (filterv #(contains? only-files (.getName (io/file %))) vendored)
                 vendored)
        _ (when only-files
            (let [found (set (map #(.getName (io/file %)) to-run))
                  missing (sort (remove found only-files))]
              (when (seq missing)
                (binding [*out* *err*]
                  (println (str "clojure-suite-run: FATAL -- CLOJURE_SUITE_ONLY names file(s) that "
                                "are not vendored: " (str/join ", " missing))))
                (System/exit 2))))
        oracle-version (slurp-trim version-path)
        results (mapv #(merge % (oracle-lookup (:file %))) (mapv run-one to-run))
        totals {:files-vendored (count vendored)
                :files-run (count results)
                :files-ok (count (filter #(= :ok (:status %)) results))
                :files-blocked (count (filter #(= :blocked (:status %)) results))
                :files-timeout (count (filter #(= :timeout (:status %)) results))
                :tests (reduce + (map :tests results))
                :pass (reduce + (map :pass results))
                :fail (reduce + (map :fail results))
                :error (reduce + (map :error results))
                :assertions (reduce + (map :assertions results))
                :assertions-passed (reduce + (map :assertions-passed results))
                :oracle-assertions-total (:assertions oracle-census-totals)
                :oracle-deftests-total (:deftests oracle-census-totals)
                :oracle-census-present oracle-census-present}
        scoreboard (cond-> {:oracle-version oracle-version
                            :timeout-secs timeout-secs
                            :files (vec (sort-by :file results))
                            :totals totals}
                     only-files (assoc :partial-run true
                                       :only-files (vec (sort only-files))))]
    (spit scoreboard-out-path (with-out-str (pprint/pprint scoreboard)))
    (println (format "clojure-suite-run: %d files, %d ok, %d blocked, %d timeout"
                     (:files-run totals) (:files-ok totals) (:files-blocked totals) (:files-timeout totals)))
    (when only-files
      (println (format "  PARTIAL RUN: only %d of %d vendored files were run (CLOJURE_SUITE_ONLY)"
                       (:files-run totals) (:files-vendored totals))))
    (println (format "  assertions: %d/%d passed" (:assertions-passed totals) (:assertions totals)))
    (if oracle-census-present
      (println (format "  ground-truth oracle: %d assertions / %d deftests (tests/clojure-suite/ORACLE-ASSERTIONS.edn)"
                       (:oracle-assertions-total totals) (:oracle-deftests-total totals)))
      (println "  ground-truth oracle: NOT PRESENT (run tools/oracle-census.sh to generate tests/clojure-suite/ORACLE-ASSERTIONS.edn)"))
    (println (format "  wrote %s" scoreboard-out-path))))

(-main)
