;; tools/oracle-census.clj
;;
;; The real Clojure/JVM side of tools/oracle-census.sh (a thin bash
;; orchestrator invokes this file once per vendored test file, in its OWN
;; fresh `clojure` process, via `clojure -Scp <classpath> -M -i
;; tools/oracle-census.clj -e '(census-one (quote some.ns) "/out/path.edn")'`
;; -- see that script for how the classpath, extra deps and Java fixture
;; classes it depends on get built).
;;
;; No `(ns ...)` form on purpose: this file is loaded with `-i` (or
;; `load-file`), not `require`d, so it is exempt from the usual
;; hyphen-in-namespace / underscore-in-filename convention and top-level
;; defs simply land in whatever *ns* the loader was sitting in (`user` for
;; `-i`). That sidesteps a real constraint: this file's own name
;; (`oracle-census.clj`, hyphenated) cannot be `require`d as a namespace
;; segment under Clojure's classloader rules without being renamed to
;; `oracle_census.clj`, and the task that produced this file is not
;; allowed to touch any filename other than the ones already given to it.
;;
;; Two entry points:
;;   (census-one ns-sym out-path)   -- runs ONE vendored file's namespace
;;                                     under real clojure.test and writes
;;                                     its counters as EDN to out-path.
;;                                     Never throws.
;;   (aggregate manifest-path run1-dir run2-dir out-path
;;              oracle-version upstream-commit vendor-fingerprint-path)
;;                                  -- combines two independent census runs
;;                                     (for the nondeterminism check) into
;;                                     the final tests/clojure-suite/
;;                                     ORACLE-ASSERTIONS.edn artifact, plus
;;                                     the :vendor-fingerprint recorded at
;;                                     vendor-fingerprint-path (an EDN map
;;                                     tools/oracle-census.sh computes from
;;                                     the CURRENT vendor/ directory and
;;                                     MANIFEST.sha256 -- see that script
;;                                     for exactly what it contains and
;;                                     why) -- this is what lets
;;                                     tools/generate-compat-report.bb
;;                                     detect a stale census cheaply,
;;                                     without re-running this ~44-minute
;;                                     tool.

(require '[clojure.test]
         '[clojure.string :as str]
         '[clojure.java.io :as io]
         '[clojure.edn :as edn]
         '[clojure.pprint :as pprint])

;; ---------------------------------------------------------------------
;; Fixed preamble, required once per process before any vendored
;; namespace is required. This is not a convenience shortcut -- it is
;; what makes a handful of vendored files even COMPILE when run in their
;; own isolated process:
;;
;;   - clojure.test-helper: upstream's OWN test suite is never run one
;;     file at a time. Its real runner (src/script/run_test.clj) uses
;;     tools.namespace to find and `require` EVERY namespace under test/
;;     -- including clojure.test-helper -- into one shared process before
;;     calling run-tests across all of them. clojure.test-helper installs
;;     `is`-form extensions via `defmethod clojure.test/assert-expr` for
;;     `thrown-with-cause-msg?` and `fails-with-cause?`, used (unrequired,
;;     because the multimethod dispatches on the literal symbol, not a
;;     resolvable var) by errors.clj, fn.clj, ns_libs.clj and others.
;;     Without this preload those files fail to COMPILE at all --
;;     "Unable to resolve symbol: thrown-with-cause-msg?" -- which is a
;;     harness artifact of per-file isolation, not a real gap.
;;   - clojure.set: ns_libs.clj contains a bare `#'clojure.set/union`
;;     var-quote with no `:require` of clojure.set in its own `ns` form,
;;     relying (in the real upstream batch run) on some other namespace
;;     in the same process having already loaded clojure.set as a side
;;     effect. Isolated, that var-quote fails to compile.
;;   - clojure.pprint: transducers.clj references `clojure.pprint/pprint`
;;     fully-qualified without requiring it, same shape of problem.
;;
;; This preamble can shift the pass/fail SPLIT of a small number of
;; assertions whose behavior is sensitive to what's already loaded (e.g.
;; repl.clj's `test-doc` checks output that differs depending on whether
;; clojure.pprint was already required) -- but it does not change the
;; ASSERTION COUNT of any file, which is the number this whole tool
;; exists to get right. Where it visibly matters this is called out in
;; that file's :note in the generated EDN.
(require 'clojure.test-helper 'clojure.set 'clojure.pprint)

(defn- cause-chain
  "Walks a Throwable's cause chain (bounded, so one pathological exception
   can't blow up the output) into a single human-readable line."
  [^Throwable t]
  (->> (iterate (fn [^Throwable e] (when e (.getCause e))) t)
       (take-while some?)
       (take 6)
       (map (fn [^Throwable e] (str (.getSimpleName (class e)) ": " (.getMessage e))))
       (str/join " -> ")))

(defn census-one
  "Requires ns-sym (expected to already be resolvable on the classpath --
   see tools/oracle-census.sh) and runs it under real clojure.test/run-tests,
   writing the resulting counters to out-path as one EDN map:

     {:status :ok | :load-error
      :assertions <int-or-nil>   ; (+ pass fail error), nil if :load-error
      :deftests   <int-or-nil>   ; clojure.test's :test count, nil if :load-error
      :clojure-pass  <int>       ; real Clojure's own pass count (0 if :load-error)
      :clojure-fail  <int>       ; real Clojure's own fail count
      :clojure-error <int>       ; real Clojure's own error count
      :note <string>}            ; empty on :ok, verbatim cause chain on :load-error

   Never throws -- any failure to require or run the namespace (including a
   real Clojure suite file that does not load standalone at all) is caught
   and recorded as :load-error rather than propagated, since the whole
   point of running one file per process is that a single bad file must
   never take the census down."
  [ns-sym out-path]
  (let [sink (java.io.StringWriter.)
        during (atom :require)]
    (binding [*out* sink
              clojure.test/*test-out* sink]
      (try
        (require ns-sym)
        (reset! during :run-tests)
        (let [summary (clojure.test/run-tests ns-sym)
              pass (long (:pass summary 0))
              fail (long (:fail summary 0))
              err (long (:error summary 0))]
          (spit out-path
                (pr-str {:status :ok
                         :assertions (+ pass fail err)
                         :deftests (long (:test summary 0))
                         :clojure-pass pass
                         :clojure-fail fail
                         :clojure-error err
                         :note ""})))
        (catch Throwable t
          (spit out-path
                (pr-str {:status :load-error
                         :assertions nil
                         :deftests nil
                         :clojure-pass 0
                         :clojure-fail 0
                         :clojure-error 0
                         :note (str "during " (name @during) ": " (cause-chain t))})))))))

(defn- read-run
  "Reads one file's census result for ns-sym out of dir, written by an
   earlier census-one call in a separate process. A missing file means
   that process never got to write it -- either tools/oracle-census.sh's
   `timeout` killed it, or it crashed hard enough to skip even the catch
   in census-one (e.g. an OutOfMemoryError) -- so it is reported as
   :timeout, never silently dropped."
  [dir ns-sym]
  (let [f (io/file dir (str ns-sym ".edn"))]
    (if (.exists f)
      (edn/read-string (slurp f))
      {:status :timeout
       :assertions nil :deftests nil
       :clojure-pass 0 :clojure-fail 0 :clojure-error 0
       :note (str "no output file written before tools/oracle-census.sh's "
                  "per-file timeout killed the process (expected at "
                  (.getPath f) ")")})))

(def ^:private header
  (str
   ";; tests/clojure-suite/ORACLE-ASSERTIONS.edn\n"
   ";;\n"
   ";; ============================================================\n"
   ";; THIS FILE IS THE DENOMINATOR OF THIS PROJECT'S NORTH STAR METRIC.\n"
   ";; ============================================================\n"
   ";;\n"
   ";; CONFORMANCE-GUARANTEE.md defines mova's headline compatibility number\n"
   ";; as \"the percentage of assertions in Clojure 1.13.0-alpha6's own\n"
   ";; test/clojure/test_clojure/ suite that pass when run under mova\". The\n"
   ";; NUMERATOR (assertions mova actually passed) lives in\n"
   ";; tests/clojure-suite/scoreboard.edn. The DENOMINATOR -- how many\n"
   ";; assertions each vendored file actually contains, a fact about the\n"
   ";; test file itself, independent of what mova currently does -- lives\n"
   ";; HERE, and only here. Deriving the denominator from mova's own run\n"
   ";; (as scoreboard.edn alone used to do) makes the headline number\n"
   ";; NON-MONOTONE: fixing a bug that unblocks a file injects that file's\n"
   ";; failing assertions into the denominator and the number can go DOWN\n"
   ";; even though mova got strictly better. This file breaks that by being\n"
   ";; a fixed, mova-independent ground truth.\n"
   ";;\n"
   ";; Regenerate with:      bash tools/oracle-census.sh\n"
   ";; (needs the Clojure CLI + JDK 21 on PATH, network access the first\n"
   ";; time to resolve org.clojure/test.check + org.clojure/test.generative\n"
   ";; from Maven Central, and tools/bootstrap-oracle.sh's pinned real\n"
   ";; Clojure 1.13.0-alpha6 oracle -- run tools/bootstrap-oracle.sh first\n"
   ";; if .oracle/ does not exist yet. Takes several minutes: every one of\n"
   ";; the ~48 vendored files is run TWICE, each time in its own fresh JVM\n"
   ";; process, under a wall-clock timeout.)\n"
   ";;\n"
   ";; HOW THE NUMBERS ARE PRODUCED: each vendored file under\n"
   ";; tests/clojure-suite/vendor/ is required and run via real\n"
   ";; clojure.test/run-tests -- NOT mova, NOT the mova-test-shim, NOT a\n"
   ";; grep over `(is ...)`/`(are ...)` text (grepping badly undercounts:\n"
   ";; `are` expands to N assertions per form, and `is` forms inside\n"
   ";; loops/doseq execute many times each). clojure.test's own\n"
   ";; *report-counters* after run-tests -- :pass + :fail + :error -- is\n"
   ";; the executed-assertion count; :test is the deftest count. Real\n"
   ";; Clojure's own pass/fail/error breakdown is recorded per file too:\n"
   ";; if real Clojure itself fails or errors on an assertion, mova failing\n"
   ";; it later is not mova's bug.\n"
   ";;\n"
   ";; The classpath (built by tools/oracle-census.sh) includes the pinned\n"
   ";; upstream test/ tree so a file's own :requires of companion\n"
   ";; namespaces resolve (e.g. protocols.clj's protocols/examples.clj\n"
   ";; subdirectory companions), org.clojure/test.check and\n"
   ";; org.clojure/test.generative at the exact versions the pinned\n"
   ";; commit's own pom.xml declares, the suite's compiled Java test\n"
   ";; fixtures (test/java/**), and a small fixed preamble\n"
   ";; (clojure.test-helper, clojure.set, clojure.pprint -- see\n"
   ";; tools/oracle-census.clj's own module doc for exactly why) that\n"
   ";; mirrors what upstream's own batch runner has already loaded as a\n"
   ";; side effect by the time any one file runs.\n"
   ";;\n"
   ";; STALENESS WARNING (the failure mode this file is most exposed to):\n"
   ";; this artifact's :assertions/:deftests totals are a fact about the\n"
   ";; vendored file SET at the moment this census was run. If a file is\n"
   ";; later added to, removed from, or edited within\n"
   ";; tests/clojure-suite/vendor/ and this census is NOT re-run, the\n"
   ";; committed totals below silently stop matching the actual vendored\n"
   ";; set -- and because the denominator staying the SAME while mova's\n"
   ";; numerator changes is exactly what makes the headline percentage\n"
   ";; look better without mova improving, a stale census is a flattering\n"
   ";; lie, not just an inaccuracy. The `:vendor-fingerprint` key below\n"
   ";; (SHA-256 of tests/clojure-suite/MANIFEST.sha256 itself, plus the\n"
   ";; vendored file count and sorted basenames, computed fresh by\n"
   ";; tools/oracle-census.sh from the CURRENT vendor/ directory every\n"
   ";; time this file is regenerated) exists so staleness can be checked\n"
   ";; CHEAPLY, without re-running this ~44-minute census:\n"
   ";; tools/generate-compat-report.bb compares this fingerprint against\n"
   ";; a freshly-computed one on every report run and FAILS its\n"
   ";; guarantee-check row (with a visible STALE marker next to the\n"
   ";; headline denominator) on any mismatch. That freshness gate runs on\n"
   ";; EVERY tools/conformance-report.sh pass; re-running this census\n"
   ";; itself stays a deliberate, occasional, manual command -- see\n"
   ";; tools/oracle-census.sh's own module doc for why those two things\n"
   ";; are deliberately kept separate. An artifact from before this key\n"
   ";; existed has no `:vendor-fingerprint` at all; the freshness gate\n"
   ";; renders that as NOT MEASURED (never :pass) and tells the reader to\n"
   ";; re-run tools/oracle-census.sh.\n"
   ";;\n"
   ";; WHAT :load-error MEANS: a file real Clojure itself cannot load even\n"
   ";; with all of the above gets :status :load-error, :assertions and\n"
   ";; :deftests nil, and is EXCLUDED from :totals -- but it is still\n"
   ";; listed below with the verbatim cause-chain reason, and :totals\n"
   ";; carries both :files-counted and :files-total so the shortfall is\n"
   ";; visible arithmetic, never a silently shifted denominator. A file\n"
   ";; whose count MOVED between this census's own two independent runs is\n"
   ";; flagged :nondeterministic true with both counts in its :note.\n"
   ";;\n"
   ";; TAMPERING WARNING: this file must ONLY ever be produced by running\n"
   ";; tools/oracle-census.sh end to end. Hand-editing any number in here\n"
   ";; changes the project's headline compatibility percentage without\n"
   ";; changing anything real about mova or about the suite. If a number\n"
   ";; here looks wrong, the fix is to fix tools/oracle-census.clj's\n"
   ";; counting logic (or its classpath/preamble) and regenerate -- never\n"
   ";; to hand-edit this file.\n"
   ";;\n"))

(defn aggregate
  "Combines two independent census runs (run1-dir, run2-dir -- each a
   directory of <ns>.edn files written by census-one, one process per
   file) plus manifest-path (an EDN vector of {:file \"x.clj\" :ns
   the.ns.sym-or-nil}, written by tools/oracle-census.sh from a plain scan
   of every vendored file's own (ns ...) form) into the final
   tests/clojure-suite/ORACLE-ASSERTIONS.edn at out-path.

   vendor-fingerprint-path points at an EDN map ({:manifest-sha256 ...
   :vendored-count ... :vendored-files [...]}) computed by
   tools/oracle-census.sh from the vendor/ directory and MANIFEST.sha256
   as they stood at census time; it is slurped verbatim and stored under
   :vendor-fingerprint in the output so tools/generate-compat-report.bb
   can detect a stale census later without re-running this tool. See this
   file's own header (spliced in below) for the full rationale."
  [manifest-path run1-dir run2-dir out-path oracle-version upstream-commit
   vendor-fingerprint-path]
  (let [manifest (edn/read-string (slurp manifest-path))
        vendor-fingerprint (edn/read-string (slurp vendor-fingerprint-path))
        jdk (System/getProperty "java.specification.version")
        entries
        (for [{:keys [file ns]} manifest]
          (if (nil? ns)
            [file {:assertions nil :deftests nil
                   :clojure-pass 0 :clojure-fail 0 :clojure-error 0
                   :status :load-error
                   :note (str "tools/oracle-census.sh's plain scan for a leading "
                              "(ns ...) form found none in this vendored file; "
                              "not run at all")}]
            (let [r1 (read-run run1-dir ns)
                  r2 (read-run run2-dir ns)
                  moved? (and (= :ok (:status r1)) (= :ok (:status r2))
                              (or (not= (:assertions r1) (:assertions r2))
                                  (not= (:deftests r1) (:deftests r2))))
                  canonical (cond-> r2
                              moved?
                              (assoc :nondeterministic true
                                     :note (str "counts differed between this census's "
                                                "own two independent runs -- run1: "
                                                (:assertions r1) " assertions/" (:deftests r1)
                                                " deftests, run2: " (:assertions r2)
                                                " assertions/" (:deftests r2) " deftests. "
                                                (:note r2))))]
              [file canonical])))
        files (into (sorted-map) entries)
        ok-files (filter #(= :ok (:status (val %))) files)
        totals {:files-total (count manifest)
                :files-counted (count ok-files)
                :assertions (reduce + 0 (map (comp :assertions val) ok-files))
                :deftests (reduce + 0 (map (comp :deftests val) ok-files))}
        data {:oracle-version oracle-version
              :upstream-commit upstream-commit
              :generated-by "tools/oracle-census.sh"
              :jdk jdk
              :vendor-fingerprint vendor-fingerprint
              :files files
              :totals totals}]
    (io/make-parents out-path)
    (spit out-path (str header (with-out-str (pprint/pprint data))))
    (println (format "oracle-census: %d/%d files counted (%d excluded), %d assertions, %d deftests"
                     (:files-counted totals) (:files-total totals)
                     (- (:files-total totals) (:files-counted totals))
                     (:assertions totals) (:deftests totals)))
    (let [nondet (filter #(:nondeterministic (val %)) files)]
      (if (seq nondet)
        (println (format "oracle-census: %d file(s) flagged :nondeterministic -- %s"
                         (count nondet) (str/join ", " (map key nondet))))
        (println "oracle-census: both runs agreed on every file (no :nondeterministic flags)")))))
