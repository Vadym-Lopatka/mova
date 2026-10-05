#!/usr/bin/env bb
;; tools/generate-compat-report.bb
;;
;; Assembles COMPATIBILITY.md from data already on disk:
;;   - tests/clojure-suite/MANIFEST.sha256   (oracle tag/commit header, AND
;;                                            the upstream/vendored/excluded
;;                                            file counts -- see note below)
;;   - tests/conformance/CLOJURE_VERSION     (pinned oracle version)
;;   - tests/conformance/corpus/*.corpus     (expression-conformance total,
;;                                            split clojure.core vs library
;;                                            -- see "corpus classification"
;;                                            below)
;;   - tests/conformance/DEVIATIONS.md       (documented deviation rows)
;;   - tests/clojure-suite/NOT-PORTABLE.md   (per-file exclusion reasons only)
;;   - tests/clojure-suite/scoreboard.edn    (Clojure-suite score, per-file
;;                                            results, and -- optionally --
;;                                            :oracle-assertions-total /
;;                                            :oracle-deftests-total /
;;                                            :oracle-census-present under
;;                                            :totals, written by a separate
;;                                            ground-truth census tool)
;;   - tests/clojure-suite/ORACLE-ASSERTIONS.edn (OPTIONAL -- read directly,
;;                                            not just via scoreboard.edn's
;;                                            merged-in oracle-* totals, so
;;                                            this script can check its
;;                                            own :vendor-fingerprint
;;                                            against the CURRENT vendor/
;;                                            directory -- see "ground-
;;                                            truth census freshness"
;;                                            below)
;;   - compat/core-var-inventory.edn         (OPTIONAL -- clojure.core public
;;                                            var presence census, written by
;;                                            tools/core-var-inventory.sh; the
;;                                            "## clojure.core surface
;;                                            coverage" section is omitted,
;;                                            never fabricated, if absent)
;;
;; Why the upstream-file-count denominator is read from MANIFEST.sha256,
;; not derived here or read from NOT-PORTABLE.md's own prose: the pinned
;; upstream Clojure clone used to DECIDE the 67/45/22 vendor/exclude split
;; lives at a session-scratchpad path that will not exist on a later
;; checkout or machine. Re-deriving "67 files in the upstream suite" at
;; report-generation time would either silently succeed against stale
;; data or silently fail to find the clone at all. MANIFEST.sha256 records
;; the count once, at vendor time, as a plain fact alongside the tag/
;; commit it was measured against -- an artifact that DOES survive into a
;; fresh checkout -- and this script only reads it back. (Previously this
;; script parsed the same number out of NOT-PORTABLE.md's summary
;; sentence, which was self-referential: NOT-PORTABLE.md and vendor/ were
;; both written by the same one-time act, so cross-checking one against
;; the other proved nothing a reviewer couldn't already see by eye.)
;;
;; Takes the corpus-test, vendor-verify, and per-file-regression PASS/FAIL
;; as env vars (COMPAT_VENDOR_VERIFY_PASS / COMPAT_CORPUS_TEST_PASS /
;; COMPAT_REGRESSION_PASS, each "true"/"false") since those come from shell
;; exit codes in tools/conformance-report.sh -- this script does not re-run
;; any of those checks itself, only reads and reports.
;;
;; GUARANTEE-GATE INTEGRITY (the whole point of this file): a report whose
;; job is anti-cheating must never print a verdict it did not measure. Every
;; guarantee-check row is therefore a TRI-STATE value -- :pass / :fail /
;; :not-measured -- not a boolean. An env var of "true" means :pass, "false"
;; means :fail, and anything else (unset, empty, running this script
;; directly instead of via tools/conformance-report.sh) means
;; :not-measured, rendered as "NOT MEASURED", never as a silent "FAIL". If
;; either of the two original anti-cheating gates (vendor-verify,
;; corpus-test -- and corpus-test's derived deviations-still-mismatch
;; check) comes back :not-measured, this script prints a warning to *err*,
;; stamps a highly visible "INCOMPLETE REPORT" banner at the top of
;; COMPATIBILITY.md, and exits non-zero, so that running this script
;; directly (skipping tools/conformance-report.sh, which always measures
;; both) cannot silently produce a report that LOOKS complete. The
;; newer, additive audit gates (regression-vs-baseline, ground-truth
;; census, shim-selftest) also render tri-state and NEVER show FAIL when
;; merely unmeasured -- but, deliberately, do not by themselves trigger
;; the "INCOMPLETE REPORT" banner: each depends on a sibling tool
;; (tools/check-regression.sh, tools/oracle-census.sh,
;; tools/check-shim-selftest.sh) that is allowed to not exist yet, and a
;; report run before those land is still a legitimate, complete run of
;; the original two-gate contract this script was built around. Once
;; those tools are a hard requirement, fold them into the same banner
;; trigger set.
;;
;; THE INVARIANT (what a skeptic or a CI job actually relies on): every
;; guarantee check gates the build. A row that renders FAIL anywhere in
;; the "## Guarantee checks" table -- ANY row, banner-gated or additive,
;; env-driven or computed straight from disk (like census-freshness) --
;; makes this script exit non-zero, full stop; there is no such thing as
;; a guarantee check that merely prints a problem without stopping
;; anything. Separately (and this is the ONLY place :not-measured also
;; forces a non-zero exit), the original two-gate contract above still
;; exits non-zero and stamps the "INCOMPLETE REPORT" banner when
;; vendor-verify, corpus-test, or deviations-still-mismatch is
;; :not-measured -- the additive gates being :not-measured (e.g. a
;; freshly checked-out tree with no ORACLE-ASSERTIONS.edn yet, or
;; tools/oracle-census.sh not having been run this pass) does NOT by
;; itself fail the build, exactly as before. The two conditions
;; ("something FAILED" vs "something was not measured") are kept
;; distinguishable on purpose: a FAIL is never relabelled as merely
;; incomplete, and the INCOMPLETE REPORT banner never appears for a run
;; that measured everything but got a bad answer. tools/conformance-
;; report.sh propagates this script's exit status into its own, so a
;; FAIL or a NOT MEASURED anywhere in COMPATIBILITY.md's guarantee table
;; means `tools/conformance-report.sh` itself exits non-zero.
;;
;; GROUND-TRUTH CENSUS FRESHNESS (a DIFFERENT kind of check from all of
;; the above -- not env-driven, always computed): tests/clojure-suite/
;; ORACLE-ASSERTIONS.edn is the DENOMINATOR of the project's North Star
;; metric, and it is expensive to regenerate (~44 minutes -- see
;; tools/oracle-census.sh), so it is deliberately NOT re-run on every
;; tools/conformance-report.sh pass; it stays a manual, occasional
;; command, and this script never invokes it. But that separation means
;; the artifact can go STALE: vendor a 49th file and the census artifact
;; keeps the old, smaller denominator, so the headline percentage
;; silently goes UP without mova improving -- the exact inflation this
;; whole project's guarantee machinery exists to prevent. The fix is a
;; CHEAP check that runs every time THIS script runs, even though the
;; expensive census itself does not: ORACLE-ASSERTIONS.edn carries a
;; :vendor-fingerprint (SHA-256 of MANIFEST.sha256 itself, vendored file
;; count, sorted basenames -- written by tools/oracle-census.sh, see its
;; module doc and tools/oracle-census.clj's header for the exact shape),
;; and this script recomputes the same fingerprint from the CURRENT tree
;; on every run and compares. A mismatch is a real :fail (not a soft
;; warning) in the guarantee table below, and the section-2 headline
;; denominator itself gets a visible STALE marker so a reader skimming
;; just the number, not the guarantee table, still can't miss it. An
;; artifact from before this key existed (no :vendor-fingerprint at all)
;; renders :not-measured, not :pass -- silence about freshness must never
;; read as freshness.
;;
;; RULE 0 FOR THIS FILE ITSELF: this generator must never emit a measured
;; quantity (a file count, form count, assertion count, version string,
;; etc.) that it did not derive from the input data listed above. A number
;; typed straight into a prose string is exactly the defect this whole
;; project's "Rule 0" (see CLOJURE-COMPAT-PLAN.md / CONFORMANCE-GUARANTEE.md)
;; forbids elsewhere -- and it is worse here, in the tool that exists to
;; enforce that rule on everything else. The entire point of a GENERATED
;; report is that it cannot go stale; a hardcoded number defeats that the
;; moment the underlying data changes. If you add a new sentence to the
;; rendered output and it needs a number, read that number from disk (or
;; from a `def` already derived from disk) -- never write the digits
;; yourself.
;;
;; Never hand-edit COMPATIBILITY.md; rerun tools/conformance-report.sh
;; instead.

(require '[clojure.edn :as edn]
         '[clojure.string :as str]
         '[clojure.java.io :as io]
         '[clojure.set :as set])

(def root (-> *file* io/file .getParentFile .getParentFile .getCanonicalPath))

(defn slurp-trim [path] (str/trim (slurp path)))

;; ---------- oracle version / commit ----------

(def oracle-version (slurp-trim (str root "/tests/conformance/CLOJURE_VERSION")))
;; Major.minor prefix of the pinned oracle version (e.g. "1.13" out of
;; "1.13.0-alpha6"), derived so prose that refers to "clojure.core 1.13"
;; tracks CLOJURE_VERSION instead of hardcoding a digit pair that goes
;; stale the moment the pin moves to 1.14.
(def oracle-version-major-minor (str/join "." (take 2 (str/split oracle-version #"\."))))

(def manifest-header (slurp (str root "/tests/clojure-suite/MANIFEST.sha256")))
(def oracle-tag
  (some->> manifest-header str/split-lines
           (some #(re-find #"tag:\s*(\S+)" %))
           second))
(def oracle-commit
  (some->> manifest-header str/split-lines
           (some #(re-find #"commit:\s*(\S+)" %))
           second))

;; ---------- (1) expression conformance ----------

(defn skippable? [line]
  (let [t (str/trim line)]
    (or (empty? t) (str/starts-with? t ";;"))))

(defn corpus-form-count [path]
  (->> (slurp path) str/split-lines (remove skippable?) count))

(def corpus-files
  (->> (io/file root "tests/conformance/corpus")
       file-seq
       (filter #(str/ends-with? (.getName %) ".corpus"))
       (map str)
       sort))

;; Corpus classification: clojure.core vs library. Every *.corpus file is
;; conformance-checked against SOME real implementation, but two of them
;; (flow.corpus, async.corpus, as of this writing) are checked against
;; org.clojure/core.async(.flow), not against clojure.core 1.13 itself --
;; genuine, valuable conformance work, but a different oracle, so folding
;; their forms into "corpus vs real Clojure" quietly launders 10% of the
;; headline into a library it doesn't test. The split below is DERIVED from
;; each file's own directives, never a hardcoded file-name list, so a new
;; file automatically lands on the right side without this script changing:
;;   - its first non-blank line is an `;;ENGINE <name>` directive naming an
;;     engine other than the default `jvm` (e.g. flow.corpus's
;;     `;;ENGINE jvm-flow`, JVM-verified against core.async.flow); or
;;   - it carries a `;;PRELUDE (require '[<ns> ...])` directive whose
;;     required namespace is not `clojure.core` itself (e.g. async.corpus's
;;     `clojure.core.async` prelude).
(defn corpus-first-nonblank-line [path]
  (->> (slurp path) str/split-lines (remove #(empty? (str/trim %))) first))

(defn corpus-prelude-namespace [path]
  (some->> (slurp path) str/split-lines
           (some #(re-find #"^;;PRELUDE\s+.*require\s+'\[([\w.\-]+)" %))
           second))

(defn library-corpus-file? [path]
  (let [first-line (corpus-first-nonblank-line path)
        prelude-ns (corpus-prelude-namespace path)]
    (boolean
     (or (and first-line
              (str/starts-with? first-line ";;ENGINE ")
              (not= first-line ";;ENGINE jvm"))
         (and prelude-ns (not= prelude-ns "clojure.core"))))))

(def corpus-file-info
  (for [f corpus-files]
    {:path f :basename (.getName (io/file f))
     :forms (corpus-form-count f) :library? (library-corpus-file? f)}))

(def library-corpus-files (filter :library? corpus-file-info))
(def core-corpus-files (remove :library? corpus-file-info))

(def corpus-total (reduce + (map :forms corpus-file-info)))
(def core-corpus-total (reduce + (map :forms core-corpus-files)))
(def library-corpus-total (reduce + (map :forms library-corpus-files)))

(defn trim-pipes [s]
  (-> s str/trim (str/replace #"^\|+" "") (str/replace #"\|+$" "")))

;; Same dumb-on-purpose parser as tests/conformance_test.rs's
;; parse_deviations: any `|`-led line where the 2nd column parses as a
;; plain integer is a deviation row (skips header/separator rows for free).
(defn parse-deviations [path]
  (->> (slurp path)
       str/split-lines
       (map str/trim)
       (filter #(str/starts-with? % "|"))
       (keep (fn [line]
               (let [cols (->> (str/split (trim-pipes line) #"\|") (map str/trim) vec)]
                 (when (>= (count cols) 2)
                   (when (re-matches #"\d+" (nth cols 1))
                     {:file (nth cols 0) :line (nth cols 1)})))))
       vec))

(def deviations (parse-deviations (str root "/tests/conformance/DEVIATIONS.md")))
(def corpus-conformant (- corpus-total (count deviations)))

(def library-basenames (set (map :basename library-corpus-files)))
(def library-deviations (filter #(library-basenames (:file %)) deviations))
(def core-deviations (remove #(library-basenames (:file %)) deviations))
(def core-corpus-conformant (- core-corpus-total (count core-deviations)))
(def library-corpus-conformant (- library-corpus-total (count library-deviations)))

;; ---------- (2) clojure-suite score ----------

(def scoreboard (edn/read-string (slurp (str root "/tests/clojure-suite/scoreboard.edn"))))
(def totals (:totals scoreboard))

;; Ground-truth census (optional, from a sibling tool -- see module doc).
;; Contract: :totals gains :oracle-assertions-total / :oracle-deftests-total
;; (ints) and :oracle-census-present (bool, false when the census artifact
;; was missing when the suite ran). Consumed if present; degrades
;; gracefully (never fabricated) if absent or the keys are missing.
(def oracle-assertions-total (:oracle-assertions-total totals))
(def oracle-deftests-total (:oracle-deftests-total totals))
(def oracle-census-tri
  (if (and (true? (:oracle-census-present totals))
           (integer? oracle-assertions-total)
           (integer? oracle-deftests-total))
    :pass
    :not-measured))

;; ---------- ground-truth census freshness (see module doc "GROUND-TRUTH
;; CENSUS FRESHNESS" above) -- NOT env-driven like the other gates: this
;; is computed fresh, straight from disk, every time this script runs,
;; because the whole point is to catch staleness even when nobody
;; remembered to re-run tools/oracle-census.sh. Reads
;; tests/clojure-suite/ORACLE-ASSERTIONS.edn directly (scoreboard.edn
;; only carries the already-merged oracle-*-total numbers, not the
;; :vendor-fingerprint) ----------

;; ORACLE_ASSERTIONS_PATH env override exists purely for testability --
;; it lets a scratch copy with a deliberately-mutated :vendor-fingerprint
;; be pointed at directly, to prove the freshness gate below actually
;; fails on a real mismatch, without ever touching the committed
;; tests/clojure-suite/ORACLE-ASSERTIONS.edn. Unset in every real run.
(def oracle-assertions-path
  (io/file (or (System/getenv "ORACLE_ASSERTIONS_PATH")
               (str root "/tests/clojure-suite/ORACLE-ASSERTIONS.edn"))))
(def oracle-assertions-data
  (when (.exists oracle-assertions-path)
    (try (edn/read-string (slurp oracle-assertions-path)) (catch Exception _e nil))))
(def recorded-vendor-fingerprint (:vendor-fingerprint oracle-assertions-data))

(defn sha256-hex [f]
  (let [md (java.security.MessageDigest/getInstance "SHA-256")
        digest (.digest md (.readAllBytes (io/input-stream f)))]
    (apply str (map #(format "%02x" (bit-and (int %) 0xff)) digest))))

(def current-manifest-sha256
  (sha256-hex (io/file root "tests/clojure-suite/MANIFEST.sha256")))
(def current-vendored-files
  (->> (io/file root "tests/clojure-suite/vendor")
       file-seq
       (filter #(and (.isFile %) (str/ends-with? (.getName %) ".clj")))
       (map #(.getName %))
       sort
       vec))
(def current-vendor-fingerprint
  {:manifest-sha256 current-manifest-sha256
   :vendored-count (count current-vendored-files)
   :vendored-files current-vendored-files})

;; Fingerprint comparison is deliberately NOT a plain `=` on the two maps.
;; :vendored-files is produced by TWO different producers with TWO
;; different sort collations: tools/oracle-census.sh sorts with the shell
;; `sort` builtin (locale-aware collation, which folds punctuation, e.g.
;; treats `.`/`_` as equivalent-ish separators), while this script sorts
;; with Clojure's `sort` (plain codepoint order, where `.` = 0x2E sorts
;; strictly before `_` = 0x5F). The two vectors can therefore contain the
;; exact same 48 filenames -- the exact same SET -- in a different order,
;; which made naive `=` report a stale census on a perfectly healthy tree.
;; Comparing :vendored-files as a SET (order-independent) fixes that false
;; positive while still catching every real drift: an added, removed, or
;; renamed vendored file changes the set and is still correctly :fail.
;; :manifest-sha256 and :vendored-count are still compared directly since
;; neither one is order-sensitive. Do NOT "simplify" this back to a bare
;; `=` on the two maps -- that reintroduces the collation false-fail.
(defn vendor-fingerprints-match? [recorded current]
  (and (= (:manifest-sha256 recorded) (:manifest-sha256 current))
       (= (:vendored-count recorded) (:vendored-count current))
       (= (set (:vendored-files recorded)) (set (:vendored-files current)))))

;; :not-measured (never :pass) when the artifact predates fingerprinting
;; entirely -- an old ORACLE-ASSERTIONS.edn with no :vendor-fingerprint
;; key says nothing about freshness, so it must not be read as fresh.
(def census-freshness-tri
  (cond
    (nil? recorded-vendor-fingerprint) :not-measured
    (vendor-fingerprints-match? recorded-vendor-fingerprint current-vendor-fingerprint) :pass
    :else :fail))

(defn census-freshness-detail []
  (when (= census-freshness-tri :fail)
    (let [rec-files (set (:vendored-files recorded-vendor-fingerprint))
          cur-files (set (:vendored-files current-vendor-fingerprint))
          only-recorded (sort (set/difference rec-files cur-files))
          only-current (sort (set/difference cur-files rec-files))]
      (str " [recorded manifest-sha256 "
           (if (= (:manifest-sha256 recorded-vendor-fingerprint) (:manifest-sha256 current-vendor-fingerprint))
             "matches" "does NOT match")
           " current; recorded vendored-count " (:vendored-count recorded-vendor-fingerprint)
           ", current vendored-count " (:vendored-count current-vendor-fingerprint)
           (when (seq only-recorded) (str "; only in recorded: " (str/join ", " only-recorded)))
           (when (seq only-current) (str "; only in current: " (str/join ", " only-current)))
           "]"))))

(defn census-freshness-cell []
  (case census-freshness-tri
    :pass "PASS"
    :fail (str "FAIL" (census-freshness-detail))
    :not-measured "NOT MEASURED (ORACLE-ASSERTIONS.edn has no :vendor-fingerprint yet -- re-run `tools/oracle-census.sh`)"))

;; File counts: read from MANIFEST.sha256's header (see module doc above
;; for why NOT from NOT-PORTABLE.md's prose summary).
(defn manifest-int [key-name]
  (some->> manifest-header str/split-lines
           (some #(re-find (re-pattern (str key-name ":\\s*(\\d+)")) %))
           second
           parse-long))

(def upstream-top-level-count (manifest-int "upstream-top-level-clj-files"))
(def manifest-vendored-count (manifest-int "vendored-count"))
(def excluded-count (manifest-int "excluded-count"))
;; companion-count (added alongside the companion-namespace-materialization
;; fix, tools/clojure-suite-run.bb): a handful of subdirectory files
;; (e.g. protocols/examples.clj) that a vendored top-level file genuinely
;; `:require`s/`:use`s are ALSO vendored in vendor/, flattened to
;; `<parent>_<child>.clj`, but are NOT part of the 67-file top-level
;; universe the upstream-top-level-count/vendored-count/excluded-count
;; arithmetic below is about. 0 when absent (older MANIFEST.sha256 with
;; no companion files vendored yet) so this stays backward-compatible.
(def manifest-companion-count (or (manifest-int "companion-count") 0))
;; spec-alpha-count (added by SPEC-W6b): the three OFFICIAL
;; clojure.spec.alpha test files -- spec.clj, instr.clj, multi_spec.clj --
;; vendored from github.com/clojure/spec.alpha rather than from
;; github.com/clojure/clojure. Same standing in this arithmetic as
;; companion-count, for exactly the same reason: they physically live in
;; vendor/ and ARE scored, but they are not part of the 67-file top-level
;; universe of the PRIMARY repo that
;; upstream-top-level-count/vendored-count/excluded-count is about. This
;; is not a softer standard -- clojure.spec.alpha ships WITH Clojure and
;; these three files are that library's own test suite; see
;; MANIFEST.sha256's "SECOND REPO" header block. 0 when absent, so an
;; older MANIFEST.sha256 keeps working unchanged.
(def manifest-spec-alpha-count (or (manifest-int "spec-alpha-count") 0))

;; NOT_PORTABLE_PATH env override exists purely for testability -- it
;; lets a scratch copy with a deliberately-deleted exclusion-table row be
;; pointed at directly, to prove exclusion-ledger-row-count's check below
;; actually fails on real drift, without ever touching the committed
;; tests/clojure-suite/NOT-PORTABLE.md. Unset in every real run.
(def not-portable-text
  (slurp (or (System/getenv "NOT_PORTABLE_PATH")
             (str root "/tests/clojure-suite/NOT-PORTABLE.md"))))

;; ---------- guarantee checks (tri-state: :pass / :fail / :not-measured) ----------

;; The PRIMARY exclusion table's row shape, and ONLY that table: any
;; `|`-led line whose first cell is a `` `*.clj` `` file AND which has
;; exactly 2 columns after trimming the outer pipes. NOT-PORTABLE.md also
;; has a "Previously excluded, now vendored" table whose rows share the
;; same leading `` `foo.clj` `` cell but have 4 columns (file / reason it
;; WAS excluded / why that reason failed the rule / milestone) -- those
;; rows are NOT exclusion-ledger rows (the files they name are currently
;; vendored, not excluded), so the 2-column filter is what tells the two
;; tables apart. every-exclusion-has-reason? and exclusion-ledger-row-count
;; both call this single function so the two checks provably agree on what
;; "an exclusion row" is -- they used to duplicate this predicate, and the
;; duplication is exactly how one of them drifted to accept 4-column rows.
(defn primary-exclusion-rows []
  (->> (str/split-lines not-portable-text)
       (filter #(re-matches #"\|\s*`[^`]+\.clj`.*\|.*" %))
       (filter (fn [row] (= 2 (count (str/split (trim-pipes row) #"\|")))))))

(defn every-exclusion-has-reason? []
  (let [rows (primary-exclusion-rows)]
    (and (seq rows)
         (every? (fn [row]
                   (let [cols (->> (str/split (trim-pipes row) #"\|") (map str/trim))]
                     (seq (second cols))))
                 rows))))

;; The three top-level MANIFEST.sha256 counts must add up (48 + 19 = 67)
;; AND "vendored-count" PLUS "companion-count" together must match the
;; actual number of files currently in tests/clojure-suite/vendor/ --
;; catches the header going stale relative to the directory (files
;; added/removed without updating the recorded count), independent of the
;; SHA-256 per-file check. companion-count is deliberately excluded from
;; the 67-arithmetic: subdirectory companion files (e.g.
;; protocols_examples.clj) are not part of the upstream top-level file
;; universe that 67/48/19 is about, even though they physically live in
;; vendor/ as flattened files -- see MANIFEST.sha256's own header and
;; tests/clojure-suite/NOT-PORTABLE.md's "Companion namespaces, now
;; vendorable" section.
(def actual-vendor-count
  (count (filter #(str/ends-with? (.getName %) ".clj")
                 (file-seq (io/file (str root "/tests/clojure-suite/vendor"))))))

(defn manifest-counts-consistent? []
  (and upstream-top-level-count manifest-vendored-count excluded-count
       (= upstream-top-level-count (+ manifest-vendored-count excluded-count))
       (= (+ manifest-vendored-count manifest-companion-count manifest-spec-alpha-count)
          actual-vendor-count)))

;; Exclusion-ledger drift: NOT-PORTABLE.md's own text requires its
;; exclusion table to contain exactly `excluded-count` rows (MANIFEST.
;; sha256), but nothing checked that until now -- and it was violated
;; earlier today when a file was dropped from the table while the count
;; still said 19. Shares primary-exclusion-rows (above) with
;; every-exclusion-has-reason? so the two checks provably agree on what
;; "an exclusion row" is -- see that function's doc for why the 4-column
;; "Previously excluded, now vendored" table must NOT be counted here.
(defn exclusion-ledger-row-count []
  (count (primary-exclusion-rows)))

(def ledger-row-count (exclusion-ledger-row-count))
(def ledger-count-matches-excluded-count? (= ledger-row-count excluded-count))

;; scoreboard.edn/MANIFEST drift: same class of silent-inflation risk as
;; the exclusion ledger -- if scoreboard.edn's own :files-vendored ever
;; disagrees with MANIFEST.sha256's vendored-count + companion-count
;; (scoreboard.edn's :files-vendored counts every *.clj under vendor/,
;; top-level AND companion), one of the two artifacts is stale and the
;; report would be blending numbers from two different vendored-file
;; snapshots without saying so.
(def scoreboard-files-vendored (:files-vendored totals))
(def scoreboard-vendored-matches-manifest?
  (= scoreboard-files-vendored
     (+ manifest-vendored-count manifest-companion-count manifest-spec-alpha-count)))

(defn manifest-consistency-detail []
  (let [problems
        (cond-> []
          (not (manifest-counts-consistent?))
          (conj "upstream-top-level-clj-files/vendored-count/excluded-count arithmetic or the actual tests/clojure-suite/vendor/ file count is inconsistent (see MANIFEST.sha256's header)")
          (not ledger-count-matches-excluded-count?)
          (conj (str "NOT-PORTABLE.md's exclusion table has " ledger-row-count " row"
                     (when (not= ledger-row-count 1) "s")
                     ", but MANIFEST.sha256's excluded-count is " excluded-count))
          (not scoreboard-vendored-matches-manifest?)
          (conj (str "scoreboard.edn's :files-vendored is " scoreboard-files-vendored
                     ", but MANIFEST.sha256's vendored-count is " manifest-vendored-count)))]
    (when (seq problems) (str " [" (str/join "; " problems) "]"))))

(def manifest-counts-pass
  (and (manifest-counts-consistent?)
       ledger-count-matches-excluded-count?
       scoreboard-vendored-matches-manifest?))

;; "true" -> :pass, "false" -> :fail, absent/empty/anything else ->
;; :not-measured. This is the fix for the defect that motivated this whole
;; tri-state scheme: running this script directly (without
;; tools/conformance-report.sh setting the env var) must never silently
;; read as :fail.
(defn env-tri-state [var-name]
  (case (System/getenv var-name)
    "true" :pass
    "false" :fail
    :not-measured))

(def vendor-verify-tri (env-tri-state "COMPAT_VENDOR_VERIFY_PASS"))
(def corpus-test-tri (env-tri-state "COMPAT_CORPUS_TEST_PASS"))
(def regression-tri (env-tri-state "COMPAT_REGRESSION_PASS"))
(def shim-selftest-tri (env-tri-state "COMPAT_SHIM_SELFTEST_PASS"))
(def oracle-pinned (and (seq oracle-version) (seq oracle-commit)))
(def exclusions-have-reasons (every-exclusion-has-reason?))
;; "every documented deviation still actually mismatches" is exactly what
;; tests/conformance_test.rs's own assertion enforces (a stale whitelist
;; entry fails that test) -- so this box mirrors the corpus test's own
;; tri-state result.
(def deviations-still-mismatch-tri corpus-test-tri)

;; The gates that gate the "INCOMPLETE REPORT" banner and non-zero exit --
;; see module doc for why regression-tri / oracle-census-tri /
;; shim-selftest-tri are tracked and rendered tri-state but deliberately
;; excluded from this set. census-freshness-tri is excluded too, but for
;; a different reason: it is not env-driven at all (see module doc
;; "GROUND-TRUTH CENSUS FRESHNESS") -- it is computed fresh from disk on
;; every run, so it is never "not measured this pass" in the sense this
;; banner exists to catch; its own :fail already renders directly and
;; visibly in the guarantee table and the section-2 STALE marker.
(def banner-gates [vendor-verify-tri corpus-test-tri deviations-still-mismatch-tri])
(def any-banner-gate-not-measured? (boolean (some #{:not-measured} banner-gates)))

;; ALL guarantee-check gates that render in the "## Guarantee checks" table
;; below, named for the stderr summary. This is deliberately the FULL set
;; -- including the additive audit gates that are excluded from
;; banner-gates above -- because a :fail is a :fail regardless of which
;; gate produced it: nothing in this table is allowed to print "FAIL" and
;; then let the build exit 0. (Only :not-measured gets the narrower
;; banner-gates-only treatment; see module doc "THE INVARIANT" above.)
(def all-gates
  [["vendor-verify" vendor-verify-tri]
   ["oracle-version-pinned" (if oracle-pinned :pass :fail)]
   ["every-exclusion-has-a-reason" (if exclusions-have-reasons :pass :fail)]
   ["deviations-still-mismatch" deviations-still-mismatch-tri]
   ["manifest-ledger-consistency" (if manifest-counts-pass :pass :fail)]
   ["regression-vs-baseline" regression-tri]
   ["oracle-census-available" oracle-census-tri]
   ["census-freshness" census-freshness-tri]
   ["shim-selftest" shim-selftest-tri]])

(def failed-gate-names (vec (for [[n s] all-gates :when (= s :fail)] n)))
(def not-measured-gate-names (vec (for [[n s] all-gates :when (= s :not-measured)] n)))
(def any-gate-fail? (boolean (seq failed-gate-names)))

;; The actual exit predicate (see module doc "THE INVARIANT"): a FAIL
;; ANYWHERE in the table, OR a NOT MEASURED among the original two-gate
;; banner-gates set, exits non-zero.
(def exit-non-zero? (or any-banner-gate-not-measured? any-gate-fail?))

;; ---------- (4) build queue: blocked files grouped by root-cause signature ----------

(defn normalize-signature [detail]
  (cond
    (str/includes? detail "invalid number literal")
    "invalid number literal (ratio `N/M`, BigDecimal `M` suffix, or BigInt `N` suffix -- mova has no ratio/bignum reader syntax)"

    :else detail))

(defn error-headline [first-error]
  (let [lines (str/split-lines (or first-error ""))
        x-line (first (filter #(str/starts-with? (str/trim %) "x ") lines))
        raw (or x-line (second lines) (first lines) "")]
    (-> raw str/trim (str/replace #"^x\s+" ""))))

(def blocked-files (filter #(= :blocked (:status %)) (:files scoreboard)))

(def build-queue
  (->> blocked-files
       (map (fn [f] (assoc f :signature (normalize-signature (error-headline (:first-error f))))))
       (group-by :signature)
       (map (fn [[sig fs]] {:signature sig :count (count fs) :files (sort (map :file fs))}))
       (sort-by :count >)))

;; ---------- (5) clojure.core surface coverage (optional) ----------

(def core-var-inventory-path (io/file root "compat/core-var-inventory.edn"))
(def core-var-inventory
  (when (.exists core-var-inventory-path)
    (edn/read-string (slurp core-var-inventory-path))))

;; ---------- render ----------

(defn status-cell [status]
  (case status
    :pass "PASS"
    :fail "FAIL"
    :not-measured "NOT MEASURED (not run this pass -- run `tools/conformance-report.sh` for a complete report)"))

;; The pending ledger: derived from the committed `.golden`/`.mova` column pair
;; of every area under tests/conformance/pending/. This is a summary for the
;; report only -- the LIVE, authoritative classification (which re-runs every
;; form through the binary under a timeout) is `cargo test --test
;; pending_conformance_test`, which is also the thing that fails on a CONFORM.
;; Reading the committed columns here keeps report generation cheap; if the two
;; ever disagree, the Rust driver is right and the `.mova` column is stale
;; (regenerate it with `tools/gen-pending.sh`).
(defn pending-ledger-section []
  (let [dir (io/file root "tests/conformance/pending")
        areas (->> (.listFiles dir)
                   (map #(.getName %))
                   (filter #(str/ends-with? % ".corpus"))
                   (map #(str/replace % #"\.corpus$" ""))
                   sort)
        rows (for [a areas
                   :let [gp (io/file dir (str a ".golden"))
                         mp (io/file dir (str a ".mova"))]
                   :when (and (.exists gp) (.exists mp))
                   :let [gs (str/split-lines (slurp gp))
                         ms (str/split-lines (slurp mp))
                         pairs (map vector gs ms)
                         ;; Same rule as tests/pending_conformance_test.rs:
                         ;; CONFORM only when BOTH sides succeeded with the same
                         ;; value. Two errors are ERR-BOTH, never agreement --
                         ;; mova has no exception taxonomy, so "both threw"
                         ;; proves nothing about WHAT was thrown.
                         timeout (count (filter (fn [[_ m]] (str/starts-with? m "TIMEOUT")) pairs))
                         err-both (count (filter (fn [[g m]]
                                                   (and (str/starts-with? g "ERR")
                                                        (str/starts-with? m "ERR")))
                                                 pairs))
                         conform (count (filter (fn [[g m]]
                                                  (and (= g m) (str/starts-with? g "OK")))
                                                pairs))]]
               {:area a :forms (count pairs) :conform conform
                :err-both err-both :timeout timeout
                :diverge (- (count pairs) conform err-both timeout)})
        tot (fn [k] (reduce + (map k rows)))]
    (str "| area | forms | DIVERGE | ERR-BOTH | TIMEOUT | CONFORM (must be 0) |\n"
         "|---|---|---|---|---|---|\n"
         (str/join (for [r rows]
                     (str "| `" (:area r) "` | " (:forms r) " | " (:diverge r) " | "
                          (:err-both r) " | " (:timeout r) " | " (:conform r) " |\n")))
         "| **total (" (count rows) " areas)** | **" (tot :forms) "** | **" (tot :diverge)
         "** | **" (tot :err-both) "** | **" (tot :timeout) "** | **" (tot :conform) "** |\n")))

(defn incomplete-banner []
  (when any-banner-gate-not-measured?
    (str "> **INCOMPLETE REPORT** -- one or more guarantee checks were not measured in this "
         "run (the report was generated without running them). Re-run "
         "`tools/conformance-report.sh` to publish a complete report.\n\n")))

;; Visible next to the headline denominator whenever the ground-truth
;; census's own :vendor-fingerprint no longer matches the current
;; vendored file set -- a reader must never see a stale denominator
;; presented as current, even if they never scroll down to the
;; guarantee-checks table. Deliberately does NOT fire on :not-measured
;; (no fingerprint recorded at all, e.g. a pre-fingerprinting artifact):
;; that case is already visibly different because oracle-census-tri
;; itself only renders :pass once :oracle-census-present/:assertions/
;; :deftests are all there, independent of the fingerprint.
(defn stale-marker []
  (when (= census-freshness-tri :fail)
    " **[STALE DENOMINATOR -- vendored file set changed since the last `tools/oracle-census.sh` run; see Guarantee checks below]**"))

(defn primary-assertions-line []
  (if (= oracle-census-tri :pass)
    (str "**" (:assertions-passed totals) " / " oracle-assertions-total "** assertions passing" (stale-marker) ", "
         "measured against the ground-truth assertion count real Clojure " oracle-version " itself "
         "reports for the same " (:files-vendored totals) " vendored files. This denominator is fixed and independent of "
         "how many files mova currently manages to run, which is what makes this number "
         "monotone: unblocking a file can only ever move it toward, never away from, the total.")
    (str "**" (:assertions-passed totals) " / (ground truth not measured — run "
         "`tools/oracle-census.sh`)**.")))

(defn deftests-line []
  (if (= oracle-census-tri :pass)
    (str "**" (:tests totals) " / " oracle-deftests-total "** deftests attempted" (stale-marker) " ("
         (:pass totals) " pass / " (:fail totals) " fail / " (:error totals)
         " error among those attempted).")
    (str "**" (:tests totals) " / (ground truth not measured — run `tools/oracle-census.sh`)** "
         "deftests attempted (" (:pass totals) " pass / " (:fail totals) " fail / "
         (:error totals) " error among those attempted).")))

(defn core-var-inventory-section []
  (when core-var-inventory
    (str "## clojure.core surface coverage\n\n"
         "- **" (:present-in-mova core-var-inventory) " / " (:clojure-publics core-var-inventory)
         "** clojure.core public vars resolve in mova (**" (:missing core-var-inventory)
         "** missing). Source: `compat/core-var-inventory.edn` "
         "(oracle: real Clojure `" (:oracle-version core-var-inventory) "`).\n"
         "- This is a PRESENCE count, not a conformance number -- a var resolving proves nothing "
         "about whether it behaves correctly; presence is not correctness. It exists only to size "
         "the remaining build queue.\n\n")))

(def report
  (str
   "<!-- GENERATED FILE -- do not hand-edit. Regenerate with:\n"
   "       tools/conformance-report.sh\n"
   "     Source data: tests/clojure-suite/{MANIFEST.sha256,NOT-PORTABLE.md,scoreboard.edn},\n"
   "     tests/conformance/{CLOJURE_VERSION,DEVIATIONS.md,corpus/*.corpus},\n"
   "     compat/core-var-inventory.edn (optional). -->\n\n"
   (incomplete-banner)
   "# mova / Clojure compatibility\n\n"
   "**Oracle**: real Clojure `" oracle-version "` (tag `" oracle-tag "`, commit `" oracle-commit "`) -- "
   "the only reference this whole report answers to. Pinned at `tests/conformance/CLOJURE_VERSION`.\n\n"
   "Every number below comes from a command you can re-run yourself:\n"
   "```\n"
   "tools/conformance-report.sh\n"
   "```\n"
   "which runs `tools/verify-vendor.sh`, `tools/check-shim-selftest.sh`, "
   "`cargo test --test conformance_test`, `tools/clojure-suite-run.sh`, `tools/check-regression.sh`, "
   "and `tools/core-var-inventory.sh`, then regenerates this file from their output "
   "(`tools/oracle-census.sh` is the one exception -- it is a separate, ~44-minute, deliberately "
   "manual command; this script only checks that its output is still fresh). "
   "Nothing here is hand-typed.\n\n"

   "## The three numbers (never blended into one)\n\n"

   "### 1. Expression conformance (corpus vs real Clojure, form-by-form)\n\n"
   "- **" corpus-total "/" corpus-total "** corpus forms accounted for: **" corpus-conformant
   "** byte-identical to real Clojure, **" (count deviations) "** documented intentional "
   "deviations (`tests/conformance/DEVIATIONS.md`). This number is 100% by construction -- any "
   "form that is neither identical nor a listed deviation FAILS the build.\n"
   "- clojure.core expression conformance: **" core-corpus-total "/" core-corpus-total "** forms "
   "accounted for (" (count core-corpus-files) " files) -- " core-corpus-conformant
   " identical, " (count core-deviations) " documented deviations.\n"
   "- library expression conformance (`core.async`, `core.async.flow`): **" library-corpus-total
   "/" library-corpus-total "** forms accounted for (" (count library-corpus-files) " files) -- "
   library-corpus-conformant " identical, " (count library-deviations) " documented deviations.\n"
   "- These are split, not blended, because only the first line measures conformance to the "
   "pinned oracle in `tests/conformance/CLOJURE_VERSION` (real clojure.core " oracle-version-major-minor "); the library "
   "line measures conformance to org.clojure/core.async and org.clojure/core.async.flow instead "
   "-- real, valuable, JVM-verified work, but a different oracle, so it does not belong in a "
   "number labelled \"vs real Clojure\".\n"
   "- `cargo test --test conformance_test`: **" (status-cell corpus-test-tri) "** "
   "(this test also asserts every whitelisted deviation still actually mismatches -- see the "
   "guarantee checks below).\n\n"

   "### 2. Clojure-suite score (vendored upstream `test_clojure/*.clj`, run through mova)\n\n"
   "- " (primary-assertions-line) "\n"
   "- Of the assertions mova actually reached: **" (:assertions-passed totals) "/" (:assertions totals)
   "** `is`/`are` assertions passing (only counting files that ran far enough to attempt any -- "
   "assertions in blocked/timed-out files are NOT counted as attempted). This is a progress "
   "diagnostic, NOT the headline: it is NON-MONOTONE by construction (unblocking a file injects "
   "that file's assertions, most of which fail on day one, so this number can DROP as coverage "
   "improves) and must never be quoted as the compatibility number.\n"
   "- " (deftests-line) "\n"
   "- **" (:files-run totals) " files run / " (:files-vendored totals) " files vendored / "
   upstream-top-level-count " top-level files in the upstream suite** (every vendored file is run "
   "every time; " excluded-count " excluded with reasons in "
   "`tests/clojure-suite/NOT-PORTABLE.md`"
   (let [extras (cond-> []
                  (pos? manifest-companion-count)
                  (conj (str manifest-companion-count " subdirectory companion file(s)"))
                  (pos? manifest-spec-alpha-count)
                  (conj (str manifest-spec-alpha-count
                             " official clojure.spec.alpha test file(s) from the"
                             " clojure/spec.alpha repo")))]
     (when (seq extras)
       (str "; the vendored count also includes " (str/join " and " extras)
            ", which are not part of that top-level arithmetic")))
   "). Of the "
   (:files-run totals) " run: " (:files-ok totals) " completed (produced a `#SUMMARY`, though "
   "individual tests inside may have failed), " (:files-blocked totals)
   " blocked before any test ran (reader/resolve error), " (:files-timeout totals)
   " timed out (killed after " (:timeout-secs scoreboard) "s each).\n\n"

   "### 3. Real-world libraries\n\n"
   "- Not yet run. No corpus of real third-party Clojure libraries has been exercised against "
   "mova yet -- this line is a placeholder until that harness exists.\n\n"

   "## Compatibility debt (the pending ledger)\n\n"
   "The three numbers above say what already conforms. This says what does NOT, in the same "
   "executable form: every entry is a real form with real Clojure " oracle-version "'s answer "
   "recorded next to mova's current one. Debt only shrinks by promotion -- when a pending form "
   "starts conforming, `tests/pending_conformance_test.rs` FAILS and demands it be moved into "
   "the main corpus, so a paid-off entry cannot quietly linger.\n\n"
   (pending-ledger-section)
   "\nAn `ERR-BOTH` row is NOT scored as agreement: both implementations threw, but mova has no "
   "exception taxonomy yet, so there is no evidence they threw the same thing. Those rows "
   "convert to real passes or real failures only once typed exceptions exist.\n\n"

   (core-var-inventory-section)

   "## Guarantee checks\n\n"
   "| check | status |\n"
   "|---|---|\n"
   "| Vendor manifest verified (`tools/verify-vendor.sh`, SHA-256 of every file in `tests/clojure-suite/vendor/`) | "
   (status-cell vendor-verify-tri) " |\n"
   "| Oracle version pinned (`tests/conformance/CLOJURE_VERSION` non-empty, upstream commit recorded) | "
   (status-cell (if oracle-pinned :pass :fail)) " |\n"
   "| Every exclusion in `tests/clojure-suite/NOT-PORTABLE.md` has a non-empty reason | "
   (status-cell (if exclusions-have-reasons :pass :fail)) " |\n"
   "| Every documented deviation in `tests/conformance/DEVIATIONS.md` still actually mismatches "
   "(enforced by `tests/conformance_test.rs` itself) | " (status-cell deviations-still-mismatch-tri) " |\n"
   "| MANIFEST.sha256 file counts are internally consistent (vendored + excluded = upstream total, "
   "vendored-count matches the actual file count in `tests/clojure-suite/vendor/`, "
   "NOT-PORTABLE.md's exclusion-table row count matches excluded-count, and scoreboard.edn's "
   ":files-vendored matches vendored-count)" (manifest-consistency-detail) " | "
   (status-cell (if manifest-counts-pass :pass :fail)) " |\n"
   "| No per-file regression vs committed baseline (`tools/check-regression.sh`) | "
   (status-cell regression-tri) " |\n"
   "| Ground-truth assertion/deftest census available (`tools/oracle-census.sh`) for the "
   "Clojure-suite denominator above | " (status-cell oracle-census-tri) " |\n"
   "| Ground-truth census is CURRENT for the vendored file set "
   "(`tests/clojure-suite/ORACLE-ASSERTIONS.edn`'s `:vendor-fingerprint` matches "
   "`tests/clojure-suite/MANIFEST.sha256` and the actual `tests/clojure-suite/vendor/` contents) | "
   (census-freshness-cell) " |\n"
   "| clojure.test shim selftest passes (`tools/check-shim-selftest.sh`) -- the Clojure-suite "
   "score is only valid if the shim is | " (status-cell shim-selftest-tri) " |\n\n"

   "## Build queue: blocked files, ranked by root cause\n\n"
   "Files that crashed (reader error, unresolved symbol, etc.) before running a single `deftest`. "
   "Grouped by the specific error mova printed (verbatim, from `tests/clojure-suite/scoreboard.edn`'s "
   "`:first-error`), ranked by how many files that one root cause blocks -- fixing the top rows first "
   "clears the most files.\n\n"
   (str/join "\n"
             (for [{:keys [signature count files]} build-queue]
               (str "- **" count " file" (when (not= count 1) "s") "** -- `" signature "`\n"
                    "  - " (str/join ", " files))))
   "\n\n"
   "(" (:files-timeout totals) " files timed out; see `tests/clojure-suite/scoreboard.edn` for those, "
   "if any.)\n"))

(spit (str root "/COMPATIBILITY.md") report)
(println "wrote COMPATIBILITY.md")

;; One-line summary to *err* naming which gates failed and which were not
;; measured, so a CI log says why without needing to open the markdown --
;; and so the two conditions stay distinguishable to the reader (a FAIL is
;; never relabelled as merely incomplete, and vice versa).
(when exit-non-zero?
  (binding [*out* *err*]
    (println (str "!! guarantee gate summary: FAILED=[" (str/join ", " failed-gate-names)
                  "] NOT-MEASURED=[" (str/join ", " not-measured-gate-names) "]"))
    (when any-gate-fail?
      (println "!! one or more guarantee gates FAILED -- see the Guarantee checks table in COMPATIBILITY.md."))
    (when any-banner-gate-not-measured?
      (println "!! COMPATIBILITY.md is an INCOMPLETE REPORT -- one or more guarantee gates were not measured.")
      (println "!! This is expected only when running generate-compat-report.bb directly.")
      (println "!! Run tools/conformance-report.sh for a complete report."))))

(System/exit (if exit-non-zero? 1 0))
