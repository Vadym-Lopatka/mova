#!/usr/bin/env bb
;; tools/check-regression.bb
;;
;; Implementation for tools/check-regression.sh (kept as babashka rather
;; than bash+grep/sed per this repo's own convention -- see
;; tools/clojure-suite-run.bb's module doc). Makes CONFORMANCE-GUARANTEE.md's
;; audit-ritual step 2 mechanical:
;;
;;   "Read `git diff` on scoreboard.edn. Any assertion that flipped
;;    pass -> fail is a regression and blocks, even while the total is
;;    far from 100%."
;;
;; That sentence was a HUMAN instruction that nothing enforced. This
;; script enforces it: compares tests/clojure-suite/scoreboard.edn (the
;; live result of the most recent tools/clojure-suite-run.sh) against
;; tests/clojure-suite/BASELINE.edn (the last deliberately-blessed
;; snapshot), file by file, and fails the build on any regression.
;;
;; ============================ WHY PER-FILE, NEVER GLOBAL ==================
;;
;; scoreboard.edn's :totals :assertions / :assertions-passed are NOT safe
;; to gate on. When a blocked file becomes unblocked, its own assertion
;; count jumps from 0 (never attempted) to some real N -- and that N
;; usually includes both new passes AND new failures, because the file is
;; now actually running deftests instead of crashing before any of them.
;; That can make the GLOBAL attempted-ratio (:assertions-passed /
;; :assertions) go DOWN even though every individual file only ever
;; improved or held steady -- the ratio is non-monotone by construction,
;; not because anything regressed. Gating on it would either (a) block
;; genuine progress (unblocking a file is unambiguously good) or (b), if
;; loosened to tolerate that, accidentally tolerate a real regression
;; hiding in the same commit. So this script never even looks at
;; :totals for the pass/fail decision -- only per-file comparisons decide
;; regressed/not, and :totals is reported purely as human-readable
;; context in the summary.
;;
;; ============================ REGRESSION DEFINITION ========================
;;
;; A file counts as REGRESSED, versus baseline, when ANY of:
;;   - its :assertions-passed DECREASED, or
;;   - its :status went from :ok to :blocked or :timeout, or
;;   - it disappeared from the scoreboard entirely (VANISHED -- a
;;     vendored file that stopped being run at all, e.g. deleted from
;;     tests/clojure-suite/vendor/ or dropped by a broken run).
;;
;; NOT a regression -- reported as informational, never fails the build:
;;   - a NEW file appearing in the scoreboard that wasn't in baseline
;;     (more files got vendored) -- assertions-passed 0 is expected and
;;     fine, since it means "no baseline to compare against", not "zero
;;     progress".
;;   - :assertions rising while :assertions-passed holds steady or rises
;;     (more of the file is now being attempted; that's neutral-to-good).
;;   - :status improving (:blocked/:timeout -> :ok).
;;
;; A file whose :assertions-passed and :status both hold steady is
;; UNCHANGED. A file whose :assertions-passed rose, OR whose :status
;; improved with :assertions-passed non-decreasing, is IMPROVED.
;;
;; ============================== RE-BLESSING =================================
;;
;; Env `COMPAT_UPDATE_BASELINE=1` re-blesses the baseline: copies the
;; CURRENT scoreboard.edn into BASELINE.edn (with a provenance header
;; recording oracle version, blessing timestamp, and how it was blessed),
;; prints loudly what changed relative to the PREVIOUS baseline (if any),
;; and exits 0. This is the ONLY way BASELINE.edn is ever written by this
;; tool -- there is no code path that re-blesses as a side effect of a
;; normal (non-COMPAT_UPDATE_BASELINE) run, by design: re-blessing must
;; always be an explicit, deliberate, loudly-logged act, never automatic
;; fallout of running the gate.
;;
;; ============================== STDOUT CONTRACT =============================
;;
;; Human-readable sections (REGRESSED / VANISHED / IMPROVED / NEW /
;; UNCHANGED), in that order, each headed by a count. The final line of
;; every non-bless run is a single machine-parseable summary, EDN after a
;; `#REGRESSION-SUMMARY ` marker (same convention as
;; tools/clojure-suite-run.bb's own `#SUMMARY`/`#RESULT` lines), shaped:
;;
;;   #REGRESSION-SUMMARY {:files-compared N, :improved N, :unchanged N,
;;                         :regressed N, :new N, :vanished N,
;;                         :assertions-passed-delta N, :regression? bool}
;;
;; `:regression?` is exactly `(or (pos? regressed) (pos? vanished))`, and
;; is the same boolean the process exit code reflects (exit 1 iff true).
;; A bless run (COMPAT_UPDATE_BASELINE=1) prints
;; `#REGRESSION-SUMMARY {:blessed true, ...}` instead and always exits 0.

(require '[clojure.edn :as edn]
         '[clojure.java.io :as io]
         '[clojure.pprint :as pprint])

(import '[java.time Instant])

(def root (-> *file* io/file .getParentFile .getParentFile .getCanonicalPath))
(def suite-dir (str root "/tests/clojure-suite"))

(def scoreboard-path (or (System/getenv "COMPAT_SCOREBOARD_PATH") (str suite-dir "/scoreboard.edn")))
(def baseline-path (or (System/getenv "COMPAT_BASELINE_PATH") (str suite-dir "/BASELINE.edn")))

(defn read-edn-file [path label]
  (when-not (.exists (io/file path))
    (binding [*out* *err*]
      (println (str "FAIL: " label " not found at " path)))
    (System/exit 1))
  (edn/read-string (slurp path)))

(defn by-file [scoreboard] (into {} (map (fn [f] [(:file f) f])) (:files scoreboard)))

;; ------------------------------- blessing ----------------------------------

(defn provenance-header [oracle-version]
  (str
   ";; tests/clojure-suite/BASELINE.edn\n"
   ";;\n"
   ";; The committed reference point for tools/check-regression.sh. Every\n"
   ";; run of that tool compares the LIVE tests/clojure-suite/scoreboard.edn\n"
   ";; against this file, per-file, and fails the build on any regression\n"
   ";; (see tools/check-regression.bb's module doc for the exact\n"
   ";; regressed/improved/unchanged/new/vanished rules -- comparisons are\n"
   ";; always per-file, never on the global attempted-ratio, which is\n"
   ";; non-monotone by construction).\n"
   ";;\n"
   ";; THIS FILE IS RE-BLESSED ONLY VIA:\n"
   ";;\n"
   ";;   COMPAT_UPDATE_BASELINE=1 bash tools/check-regression.sh\n"
   ";;\n"
   ";; which overwrites it with the CURRENT scoreboard.edn and prints\n"
   ";; loudly what changed relative to the previous baseline. Never\n"
   ";; hand-edit this file -- there is no other supported way to update\n"
   ";; it, and a hand-edit defeats the entire point of the gate.\n"
   ";;\n"
   ";; :baseline-provenance below records when/how/against-what-oracle\n"
   ";; this snapshot was blessed. Everything else in the map is a verbatim\n"
   ";; copy of scoreboard.edn's own :oracle-version / :files / :totals at\n"
   ";; blessing time.\n\n"))

(defn bless! []
  (let [scoreboard (read-edn-file scoreboard-path "scoreboard.edn (run tools/clojure-suite-run.sh first)")
        prior-baseline (when (.exists (io/file baseline-path)) (edn/read-string (slurp baseline-path)))
        now (str (Instant/now))
        baseline {:baseline-provenance
                  {:oracle-version (:oracle-version scoreboard)
                   :blessed-at now
                   :blessed-from "tests/clojure-suite/scoreboard.edn"
                   :blessed-by "tools/check-regression.sh (COMPAT_UPDATE_BASELINE=1)"
                   :rebless-instructions "COMPAT_UPDATE_BASELINE=1 bash tools/check-regression.sh"}
                  :oracle-version (:oracle-version scoreboard)
                  :files (:files scoreboard)
                  :totals (:totals scoreboard)}]
    (io/make-parents baseline-path)
    (spit baseline-path (str (provenance-header (:oracle-version scoreboard))
                             (with-out-str (pprint/pprint baseline))))
    (println "================================================================")
    (println "BLESSING NEW BASELINE -- tests/clojure-suite/BASELINE.edn")
    (println "================================================================")
    (if prior-baseline
      (let [old-by-file (by-file prior-baseline)
            new-by-file (by-file scoreboard)
            all-files (into (sorted-set) (concat (keys old-by-file) (keys new-by-file)))]
        (println (str "Previous baseline blessed at: " (get-in prior-baseline [:baseline-provenance :blessed-at] "(unknown -- pre-provenance baseline)")))
        (println (str "Previous baseline oracle-version: " (:oracle-version prior-baseline)))
        (println (str "New baseline oracle-version: " (:oracle-version scoreboard)))
        (println "Per-file changes being blessed in:")
        (doseq [f all-files]
          (let [old (get old-by-file f) new (get new-by-file f)]
            (cond
              (and old (not new)) (println (format "  VANISHED  %s (was assertions-passed=%d, status=%s)" f (:assertions-passed old) (str (:status old))))
              (and new (not old)) (println (format "  NEW       %s (assertions-passed=%d, status=%s)" f (:assertions-passed new) (str (:status new))))
              (not= (:assertions-passed old) (:assertions-passed new))
              (println (format "  CHANGED   %s: assertions-passed %d -> %d, status %s -> %s" f (:assertions-passed old) (:assertions-passed new) (str (:status old)) (str (:status new))))
              (not= (:status old) (:status new))
              (println (format "  CHANGED   %s: status %s -> %s (assertions-passed unchanged at %d)" f (str (:status old)) (str (:status new)) (:assertions-passed new)))
              :else nil)))
        (println (format "  (%d files unchanged, not listed individually)"
                         (count (filter (fn [f] (let [old (get old-by-file f) new (get new-by-file f)]
                                                  (and old new (= (:assertions-passed old) (:assertions-passed new)) (= (:status old) (:status new)))))
                                        all-files)))))
      (println "No previous baseline existed -- this is the INITIAL bless."))
    (println (format "Blessed %d files, %d total assertions-passed, oracle %s, at %s"
                     (count (:files scoreboard)) (get-in scoreboard [:totals :assertions-passed]) (:oracle-version scoreboard) now))
    (println "================================================================")
    (println (str "#REGRESSION-SUMMARY " (pr-str {:blessed true
                                                  :files (count (:files scoreboard))
                                                  :assertions-passed (get-in scoreboard [:totals :assertions-passed])
                                                  :oracle-version (:oracle-version scoreboard)
                                                  :blessed-at now})))))

;; ------------------------------- comparison ---------------------------------

(defn regressed? [old new]
  (or (< (:assertions-passed new 0) (:assertions-passed old 0))
      (and (= :ok (:status old)) (contains? #{:blocked :timeout} (:status new)))))

(defn improved? [old new]
  (or (> (:assertions-passed new 0) (:assertions-passed old 0))
      (and (contains? #{:blocked :timeout} (:status old)) (= :ok (:status new)))))

(defn classify [old-by-file new-by-file f]
  (let [old (get old-by-file f) new (get new-by-file f)]
    (cond
      (and old (not new)) :vanished
      (and new (not old)) :new
      (regressed? old new) :regressed
      (improved? old new) :improved
      :else :unchanged)))

(defn describe [f old new]
  (cond
    (and old (not new))
    (format "  - %s: was assertions-passed=%d status=%s in baseline, no longer in scoreboard" f (:assertions-passed old) (str (:status old)))
    (and new (not old))
    (format "  - %s: assertions-passed=%d status=%s (not in baseline)" f (:assertions-passed new) (str (:status new)))
    :else
    (format "  - %s: assertions-passed %d -> %d, status %s -> %s"
            f (:assertions-passed old) (:assertions-passed new) (str (:status old)) (str (:status new)))))

(defn run-check! []
  (let [baseline (read-edn-file baseline-path "BASELINE.edn (no baseline blessed yet -- run: COMPAT_UPDATE_BASELINE=1 bash tools/check-regression.sh)")
        scoreboard (read-edn-file scoreboard-path "scoreboard.edn (run tools/clojure-suite-run.sh first)")
        old-by-file (by-file baseline)
        new-by-file (by-file scoreboard)
        all-files (into (sorted-set) (concat (keys old-by-file) (keys new-by-file)))
        grouped (group-by (fn [f] (classify old-by-file new-by-file f)) all-files)
        section (fn [k label]
                  (let [fs (sort (get grouped k))]
                    (println (format "%s (%d):" label (count fs)))
                    (doseq [f fs] (println (describe f (get old-by-file f) (get new-by-file f))))
                    (println)))]
    (println "================================================================")
    (println (str "check-regression: " scoreboard-path))
    (println (str "                vs " baseline-path
                  " (blessed " (get-in baseline [:baseline-provenance :blessed-at] "unknown") ")"))
    (println "================================================================")
    (println)
    (section :regressed "REGRESSED")
    (section :vanished "VANISHED")
    (section :improved "IMPROVED")
    (section :new "NEW")
    (println (format "UNCHANGED (%d)" (count (get grouped :unchanged))))
    (println)
    (let [delta (- (get-in scoreboard [:totals :assertions-passed] 0)
                   (get-in baseline [:totals :assertions-passed] 0))
          summary {:files-compared (count all-files)
                   :improved (count (get grouped :improved))
                   :unchanged (count (get grouped :unchanged))
                   :regressed (count (get grouped :regressed))
                   :new (count (get grouped :new))
                   :vanished (count (get grouped :vanished))
                   :assertions-passed-delta delta
                   :regression? (boolean (seq (concat (get grouped :regressed) (get grouped :vanished))))}]
      (println "----------------------------------------------------------------")
      (println (format "SUMMARY: %d files compared, %d improved, %d unchanged, %d regressed, %d new, %d vanished, total assertions-passed delta %+d"
                       (:files-compared summary) (:improved summary) (:unchanged summary)
                       (:regressed summary) (:new summary) (:vanished summary) (:assertions-passed-delta summary)))
      (println (str "#REGRESSION-SUMMARY " (pr-str summary)))
      (println "----------------------------------------------------------------")
      (if (:regression? summary)
        (do (println "RESULT: REGRESSION DETECTED -- blocking. Fix the regression, or if it is a deliberate,")
            (println "        understood tradeoff, re-bless with: COMPAT_UPDATE_BASELINE=1 bash tools/check-regression.sh")
            (System/exit 1))
        (do (println "RESULT: no regression.")
            (System/exit 0))))))

(defn -main []
  (if (= "1" (System/getenv "COMPAT_UPDATE_BASELINE"))
    (bless!)
    (run-check!)))

(-main)
