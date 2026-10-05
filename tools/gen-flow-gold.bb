#!/usr/bin/env bb
;; gen-flow-gold.bb -- regenerates tests/conformance/flow-gold/goldens/*.golden
;; by running each tests/conformance/flow-gold/scenarios/NN-name.mova scenario
;; against a REAL JVM `clojure.core.async.flow` (see
;; tests/conformance/flow-gold/SPEC.md's "Comparison semantics" section,
;; which this script implements verbatim). Distinct from
;; tools/gen-golden.bb's own `;;ENGINE jvm-flow` path (that one pins
;; core.async via `:mvn/version "1.9.808-alpha1"`, a released build that
;; predates flow being deleted from core.async master); this script instead
;; points at a LOCAL checkout of the still-living flow implementation
;; (GOLD_ROOT below) via `:local/root`, per SPEC.md.
;;
;; Usage:
;;   bb tools/gen-flow-gold.bb                  ;; regenerate every scenario
;;   bb tools/gen-flow-gold.bb 00-smoke          ;; just one (bare name)
;;   bb tools/gen-flow-gold.bb 00-smoke.mova      ;; ...or with extension
;;   bb tools/gen-flow-gold.bb scenarios/00-smoke.mova  ;; ...or a path
;;
;; Env:
;;   FLOW_GOLD_ROOT   overrides the default gold checkout
;;                    (a git worktree
;;                    of core.async pinned at origin/dev-flow-alpha 42dbd51 --
;;                    see SPEC.md's header). Must have
;;                    src/main/clojure/clojure/core/async/flow*.clj.
;;
;; Per scenario, this script:
;;   1. Parses header directives from the scenario text (;;RUNS n, default
;;      5; ;;TIMEOUT-MS ms, default 30000; ;;DIVERGENT, a flag -- see
;;      SPEC.md's "Scenario format"). Directives are themselves ORDINARY
;;      Clojure `;;`-comments, so they are NOT stripped out of the body --
;;      the same scenario text that's parsed for directives here is also
;;      literally spliced into the JVM tmpfile below; the reader just skips
;;      them as comments on both sides, exactly like a normal `;; comment`.
;;   2. Assembles a tmpfile = JVM prelude (the one require form every
;;      scenario is written against, SPEC.md §"Body rules" 1) + the
;;      scenario's raw text + postlude (`(System/exit 0)` -- futurized
;;      executors otherwise keep the JVM alive forever, SPEC.md §"Body
;;      rules" 5).
;;   3. Runs `clojure -Sdeps '{:deps {org.clojure/core.async {:local/root
;;      <GOLD_ROOT>}}}' -M <tmpfile>` RUNS times, each under its own
;;      TIMEOUT-MS wall-clock budget (killed with `destroyForcibly` on
;;      expiry -- a hung scenario is a generation FAILURE, never a golden).
;;   4. Requires ALL runs' stdout to be BYTE-IDENTICAL (raw bytes, not just
;;      line-equal) -- a scenario whose JVM output isn't stable run-to-run
;;      is not canonical; fix the scenario (reduce nondeterminism to a
;;      verdict, SPEC.md §"Body rules" 4), don't commit a flaky golden. On
;;      any mismatch (or timeout, or nonzero exit), this script prints a
;;      rich failure for that scenario and does NOT write/touch its golden
;;      file, then continues to the next scenario; it exits nonzero overall
;;      if anything failed.
;;   5. On success, writes the (single, shared) stdout bytes verbatim to
;;      goldens/NN-name.golden. stderr is captured separately (redirected to
;;      its own temp file, never merged with stdout) and only surfaced on
;;      failure or via -v, per SPEC.md/FINDINGS.md's stdout/stderr
;;      separation finding.
;;
;; A final summary table lists every scenario processed with its status.

(require '[clojure.string :as str]
         '[clojure.java.io :as io])

(import '[java.util.concurrent TimeUnit]
        '[java.nio.file Files])

(def default-runs 5)
(def default-timeout-ms 30000)

;; SPEC.md §"Body rules" 1 -- the ONE JVM prelude every scenario body is
;; written against. Must stay textually in sync with that section and with
;; tests/flow_gold_test.rs's doc comment (mova side needs no prelude: these
;; names are global builtins there already).
(def jvm-prelude
  "(require '[clojure.core.async :as a :refer [chan >!! <!! close! timeout]] '[clojure.core.async.flow :as flow])\n")

;; SPEC.md §"Body rules" 5 -- appended after every scenario body for the JVM
;; side only. mova's CLI process exits on its own once the script's last
;; form completes, so it needs no equivalent.
(def jvm-postlude "\n(System/exit 0)\n")

(def flow-gold-dir (io/file "tests/conformance/flow-gold"))
(def scenarios-dir (io/file flow-gold-dir "scenarios"))
(def goldens-dir (io/file flow-gold-dir "goldens"))

(defn gold-root []
  (let [v (System/getenv "FLOW_GOLD_ROOT")]
    (if (str/blank? v)
      (do (println "usage: FLOW_GOLD_ROOT=/path/to/core.async-flow bb tools/gen-flow-gold.bb") (System/exit 2))
      v)))

(defn- resolve-scenario-arg
  "Turns a CLI arg (bare name, name.mova, or a path) into a scenario java.io.File."
  [arg]
  (let [as-given (io/file arg)]
    (cond
      (.isFile as-given) as-given
      (str/ends-with? arg ".mova") (io/file scenarios-dir arg)
      :else (io/file scenarios-dir (str arg ".mova")))))

(defn scenario-files [argv]
  (if (seq argv)
    (mapv resolve-scenario-arg argv)
    (->> (.listFiles scenarios-dir)
         (filter #(and (.isFile %) (str/ends-with? (.getName %) ".mova")))
         (sort-by #(.getName %)))))

(defn scenario-name [file]
  (str/replace (.getName file) #"\.mova$" ""))

(defn parse-directives
  "Reads header directives out of a scenario's raw text. Directives are
  plain `;;`-prefixed comment lines anywhere in the file (conventionally at
  the top, per SPEC.md) -- this scans every line rather than only a leading
  block, since a directive left in the body is just an inert Clojure
  comment either way and being permissive here costs nothing."
  [text]
  (reduce
   (fn [acc line]
     (let [t (str/trim line)
           runs-m (re-matches #";;RUNS\s+(\d+)" t)
           timeout-m (re-matches #";;TIMEOUT-MS\s+(\d+)" t)]
       (cond
         runs-m (assoc acc :runs (Long/parseLong (second runs-m)))
         timeout-m (assoc acc :timeout-ms (Long/parseLong (second timeout-m)))
         (= t ";;DIVERGENT") (assoc acc :divergent true)
         :else acc)))
   {:runs default-runs :timeout-ms default-timeout-ms :divergent false}
   (str/split-lines text)))

(defn read-bytes ^bytes [^java.io.File f]
  (Files/readAllBytes (.toPath f)))

(defn- new-temp-file [prefix suffix]
  (doto (java.io.File/createTempFile prefix suffix)
    (.deleteOnExit)))

;; One JVM run of a scenario's assembled tmpfile. Returns a map:
;;   {:run n :timeout? bool :exit int-or-nil :stdout bytes :stderr bytes}
;; stdout/stderr are captured to their OWN temp files (never merged) so a
;; large/blocked stream can't deadlock the child (no in-process pipe
;; reading needed) and so the golden never accidentally picks up stderr
;; noise (JVM startup banners, warnings, etc. -- see FINDINGS.md's
;; stdout/stderr-separation finding).
(defn run-once [run-idx ^java.io.File tmp-file timeout-ms]
  (let [out-file (new-temp-file "flow-gold-out" ".bin")
        err-file (new-temp-file "flow-gold-err" ".bin")
        deps-str (format "{:deps {org.clojure/core.async {:local/root %s}}}" (pr-str (gold-root)))
        pb (ProcessBuilder. ^"[Ljava.lang.String;"
            (into-array String ["clojure" "-Sdeps" deps-str "-M" (.getPath tmp-file)]))]
    (.redirectOutput pb out-file)
    (.redirectError pb err-file)
    (try
      (let [proc (.start pb)
            finished? (.waitFor proc timeout-ms TimeUnit/MILLISECONDS)]
        (if-not finished?
          (do (.destroyForcibly proc)
              ;; give the OS a moment to flush partial output to the redirect files
              (.waitFor proc 2 TimeUnit/SECONDS)
              {:run run-idx :timeout? true :exit nil
               :stdout (read-bytes out-file) :stderr (read-bytes err-file)})
          {:run run-idx :timeout? false :exit (.exitValue proc)
           :stdout (read-bytes out-file) :stderr (read-bytes err-file)}))
      (catch java.io.IOException e
        {:run run-idx :timeout? false :exit :spawn-error
         :stdout (byte-array 0)
         :stderr (.getBytes (str "gen-flow-gold: failed to spawn `clojure` -- is it on PATH? " (.getMessage e)))}))))

(defn bytes= [^bytes a ^bytes b]
  (java.util.Arrays/equals a b))

(defn- preview [^bytes b]
  (let [s (String. b "UTF-8")
        lines (str/split-lines s)]
    (if (> (count lines) 20)
      (str (str/join "\n" (take 20 lines)) "\n  ... (" (- (count lines) 20) " more line(s))")
      s)))

(defn- print-failure-header [name reason]
  (println (format "\n=== FAIL %s: %s ===" name reason)))

(defn- print-run-diff
  "Rich side-by-side-ish diff between the first two runs whose stdout
  differs, plus a listing of every run's stdout byte length so a
  systematically-truncating hang is visible at a glance."
  [results]
  (println "  run byte-lengths:" (str/join ", " (map #(format "run %d=%dB" (:run %) (alength ^bytes (:stdout %))) results)))
  (let [first-run (first results)
        divergent (first (remove #(bytes= (:stdout first-run) (:stdout %)) (rest results)))]
    (when divergent
      (println (format "  first divergence: run %d vs run %d" (:run first-run) (:run divergent)))
      (println (format "  --- run %d stdout ---" (:run first-run)))
      (println (preview (:stdout first-run)))
      (println (format "  --- run %d stdout ---" (:run divergent)))
      (println (preview (:stdout divergent))))))

(defn process-scenario [file]
  (let [name (scenario-name file)]
    (if-not (.isFile file)
      (do (print-failure-header name (str "scenario file not found: " (.getPath file)))
          {:name name :status :fail})
      (let [text (slurp file)
            {:keys [runs timeout-ms divergent]} (parse-directives text)
            tmp (new-temp-file (str "flow-gold-" name "-") ".mova")]
        (spit tmp (str jvm-prelude text jvm-postlude))
        (println (format "-> %-32s RUNS=%d TIMEOUT-MS=%d%s" name runs timeout-ms (if divergent " DIVERGENT" "")))
        (let [results (mapv #(run-once % tmp timeout-ms) (range 1 (inc runs)))
              timeouts (filter :timeout? results)
              spawn-errors (filter #(= :spawn-error (:exit %)) results)
              nonzero (filter #(and (not (:timeout? %))
                                    (not= :spawn-error (:exit %))
                                    (not (zero? (:exit %))))
                              results)]
          (cond
            (seq spawn-errors)
            (do (print-failure-header name "could not spawn `clojure`")
                (println (String. ^bytes (:stderr (first spawn-errors)) "UTF-8"))
                {:name name :status :fail})

            (seq timeouts)
            (do (print-failure-header name (format "%d/%d run(s) exceeded TIMEOUT-MS=%d"
                                                   (count timeouts) runs timeout-ms))
                (doseq [t timeouts]
                  (println (format "  run %d: partial stdout (%dB):" (:run t) (alength ^bytes (:stdout t))))
                  (println (preview (:stdout t)))
                  (when (pos? (alength ^bytes (:stderr t)))
                    (println (format "  run %d stderr:" (:run t)))
                    (println (preview (:stderr t)))))
                {:name name :status :fail})

            (seq nonzero)
            (do (print-failure-header name (format "%d/%d run(s) exited nonzero" (count nonzero) runs))
                (doseq [r nonzero]
                  (println (format "  run %d: exit=%s" (:run r) (:exit r)))
                  (println "  stdout:") (println (preview (:stdout r)))
                  (println "  stderr:") (println (preview (:stderr r))))
                {:name name :status :fail})

            (not (every? #(bytes= (:stdout (first results)) (:stdout %)) (rest results)))
            (do (print-failure-header name (format "%d run(s) were NOT byte-identical -- scenario is not canonical, golden NOT written" runs))
                (print-run-diff results)
                {:name name :status :fail})

            :else
            (let [golden-bytes (:stdout (first results))
                  golden-file (io/file goldens-dir (str name ".golden"))]
              (.mkdirs goldens-dir)
              (io/copy golden-bytes golden-file)
              (println (format "   ok  %d/%d runs identical, %dB -> %s"
                               runs runs (alength ^bytes golden-bytes) (.getPath golden-file)))
              (let [any-stderr? (some #(pos? (alength ^bytes (:stderr %))) results)]
                (when any-stderr?
                  (println "   note: JVM stderr was non-empty on at least one run (not part of the golden); rerun with -v to inspect")))
              {:name name :status :ok :runs runs :bytes (alength ^bytes golden-bytes) :divergent divergent})))))))

(defn print-summary [results]
  (println "\n=== gen-flow-gold summary ===")
  (println (format "%-32s %-10s %6s %8s" "scenario" "status" "runs" "bytes"))
  (doseq [r results]
    (println (format "%-32s %-10s %6s %8s"
                     (:name r)
                     (str (name (:status r)) (when (:divergent r) "*"))
                     (str (or (:runs r) "-"))
                     (str (or (:bytes r) "-")))))
  (let [failed (filter #(= :fail (:status %)) results)]
    (println (format "\n%d scenario(s), %d failed" (count results) (count failed)))
    (when (some :divergent results)
      (println "* = ;;DIVERGENT scenario (golden still holds JVM output; see DIVERGENCES.md for mova's expected output)"))
    failed))

(defn -main [& argv]
  (println "gen-flow-gold: GOLD_ROOT =" (gold-root))
  (let [files (scenario-files argv)]
    (when (empty? files)
      (println "no scenario .mova files found under" (.getPath scenarios-dir))
      (System/exit 1))
    (let [results (mapv process-scenario files)
          failed (print-summary results)]
      (System/exit (if (seq failed) 1 0)))))

(apply -main *command-line-args*)
