#!/usr/bin/env bb
;; tools/check-perf-regression.bb
;;
;; Implementation for tools/check-perf-regression.sh -- mechanizes the
;; conductor's manual perf A/B gate (docs/W-LENS-DESIGN.md Stage 3,
;; W-LENS-3): measures the house benchmarks (lane, flow-2hop, flow-4hop,
;; delays wall, and parse wall -- the fifth metric per
;; NEXT-SESSION-STARTER.md's scope extension), writes a perf scoreboard
;; EDN, and diffs it against a committed baseline EDN -- same shape, env
;; conventions, output contract, and exit-code contract as
;; tools/check-regression.bb (see that file's own module doc for the
;; EDN-in/EDN-out, no-fragile-text-parsing convention this mirrors).
;;
;; UNLIKE check-regression.bb (which only DIFFS a scoreboard some other
;; script already wrote), this tool both MEASURES and DIFFS in one run:
;; there is no separate "run the benchmarks first" step, because the
;; five house benchmarks are independent, fast-to-medium invocations
;; (lane/flow-2hop/flow-4hop/delays: seconds; parse: ~200s on a quiet
;; machine) rather than a 50+-file suite that needs its own scratch/
;; materialization pass ahead of time.
;;
;; ============================ THE FIVE METRICS ==============================
;;
;; 1. lane      (iters/s, HIGHER better, hard bar >= 250M):
;;    `cargo test --release --test lane_hand_wire_bench -- --nocapture`;
;;    the test itself is median-of-5 internally, so each "round" here is
;;    one full cargo-test invocation and its own printed median becomes
;;    that round's sample (tests/lane_hand_wire_bench.rs's
;;    `hand_wired_lane_clears_250m_iters_per_s`).
;; 2. flow-2hop (msg/s, HIGHER better): `<mova-bin> bench/flow-2hop.mova`
;; 3. flow-4hop (msg/s, HIGHER better): `<mova-bin> bench/flow-4hop.mova`
;;    -- each prints exactly one line `<name> <msgs> <ms> <msg/s>`; 1
;;    discarded warmup round + N measured rounds, sample = msg/s.
;; 4. delays    (wall seconds, LOWER better, hard bar < 8.0s)
;; 5. parse     (wall seconds, LOWER better, no fixed bar --
;;    regression-only) -- both assemble the single suite file exactly the
;;    way tools/clojure-suite-run.bb's `run-one` does: splice the
;;    clojure.test + clojure.test-helper shims in immediately after the
;;    vendored file's own `(ns ...)` form, re-enter its declared
;;    namespace, wrap the trailing call in `((fn [] (ns user)
;;    (run-tests)))`, run under `timeout <secs>s <bin>`, and verify the
;;    last `#SUMMARY` line is all-pass (fail=0, error=0,
;;    assertions=assertions-passed, assertions>0) before trusting the
;;    wall time -- if it isn't, this ABORTS LOUDLY: the perf gate must
;;    never bless (or even report) a wall time measured against a
;;    correctness break.
;;
;;    ASSEMBLY LOGIC IS A DOCUMENTED COPY, not a require/load-file of
;;    clojure-suite-run.bb: that file ends with an unconditional
;;    `(-main)` call which materializes + RUNS THE ENTIRE 50+-file suite
;;    as a side effect of merely being loaded -- exactly what this tool
;;    (and the stall-prevention discipline its own wave spec was written
;;    under) must never trigger. The handful of pure functions below
;;    (`skip-balanced-form` / `skip-ws` / `skip-ns-metadata` /
;;    `declared-ns` / `inject-shim-after-ns`, plus the shim/vendor-libs
;;    materialization fns) are verbatim copies of clojure-suite-run.bb's
;;    own -- if that file's assembly logic ever changes, this one needs a
;;    matching update. Refactoring clojure-suite-run.bb itself (e.g.
;;    guarding its `(-main)` behind an env check so it could be
;;    `load-file`d safely instead) was judged out of scope for this wave,
;;    since it is the live implementation behind the conformance gate and
;;    this wave's mission is additive, not a refactor of shared machinery.
;;
;; ============================ STATISTICS + VERDICT ==========================
;;
;; Per metric: N measured rounds -> median/min/max stored in the
;; scoreboard (median of an even N is the arithmetic mean of the middle
;; two samples -- only parse's default N=2 is even). Verdict mirrors
;; bench/run.sh's `verdict()` / `verdict_lower_better()`: REGRESSED only
;; when the current [min,max] range is entirely on the bad side of the
;; baseline [min,max] range (no overlap); IMPROVED when entirely on the
;; good side; else UNCHANGED. Never a single-sample delta, never a fixed
;; percent threshold. Ledgered hard bars (lane, delays) are checked
;; INDEPENDENTLY of the baseline diff -- a bar failure is a regression
;; even if the baseline also failed it. Regression is decided per-metric,
;; never aggregate: exit 1 iff any metric REGRESSED or any hard bar
;; failed (this is a narrower trigger than check-regression.bb's own
;; conformance gate, which also treats VANISHED as a regression -- here
;; a metric going missing is reported but does not by itself fail the
;; gate, per this wave's spec).
;;
;; ============================== ENV VARS =====================================
;;
;;   PERF_SCOREBOARD_PATH    live measurement output (default: bench/perf-scoreboard.edn;
;;                           NOT committed -- gitignored)
;;   PERF_BASELINE_PATH      committed reference point (default: bench/PERF-BASELINE.edn)
;;   PERF_UPDATE_BASELINE=1  bless the just-measured scoreboard as the new baseline
;;                           (always exits 0; refuses on a PERF_METRICS subset run)
;;   PERF_PROVISIONAL=1      (bless only) stamp :provisional-loaded-machine true into the
;;                           baseline's provenance -- for blessing on a loaded dev machine
;;                           pending a quiet-machine re-bless at merge
;;   MOVA_BIN               binary under test (default: target/release/mova)
;;   PERF_METRICS            comma-separated subset, e.g. "lane,flow-2hop" (default: all
;;                           five; a subset run never blesses and never reports VANISHED
;;                           for metrics it didn't run -- it diffs only what it ran)
;;   PERF_ROUNDS_LANE / PERF_ROUNDS_FLOW_2HOP / PERF_ROUNDS_FLOW_4HOP /
;;   PERF_ROUNDS_DELAYS / PERF_ROUNDS_PARSE   override that metric's round count
;;   PERF_TIMEOUT_DELAYS / PERF_TIMEOUT_PARSE per-round subprocess wall-clock timeout in
;;                           seconds (default 60 / 400 -- parse runs ~200s on a quiet
;;                           machine, per docs/W-PARSE-poison-shadow-decision.md's
;;                           best-of-2 methodology)
;;
;; ============================== STDOUT CONTRACT ==============================
;;
;; Human-readable sections REGRESSED / IMPROVED / UNCHANGED / NEW /
;; VANISHED with counts, then a final machine-parseable line:
;;
;;   #PERF-REGRESSION-SUMMARY {:metrics-compared N, :improved N,
;;     :unchanged N, :regressed N, :new N, :vanished N, :bars-failed N,
;;     :regression? bool}
;;
;; exit 1 iff :regression?. A bless run prints
;; `#PERF-REGRESSION-SUMMARY {:blessed true, ...}` instead and always
;; exits 0.

(require '[babashka.process :as p]
         '[clojure.edn :as edn]
         '[clojure.java.io :as io]
         '[clojure.string :as str]
         '[clojure.pprint :as pprint])

(import '[java.time Instant])

(def root (-> *file* io/file .getParentFile .getParentFile .getCanonicalPath))
(def bench-dir (str root "/bench"))
(def suite-dir (str root "/tests/clojure-suite"))
(def vendor-dir (str suite-dir "/vendor"))
(def vendor-libs-dir (str suite-dir "/vendor-libs"))
(def shim-path (str suite-dir "/mova-test-shim.mova"))
(def helper-shim-path (str suite-dir "/mova-test-helper-shim.mova"))

(def scoreboard-path (or (System/getenv "PERF_SCOREBOARD_PATH") (str bench-dir "/perf-scoreboard.edn")))
(def baseline-path (or (System/getenv "PERF_BASELINE_PATH") (str bench-dir "/PERF-BASELINE.edn")))
(def mova-bin (or (System/getenv "MOVA_BIN") (str root "/target/release/mova")))
(def provisional? (= "1" (System/getenv "PERF_PROVISIONAL")))
(def update-baseline? (= "1" (System/getenv "PERF_UPDATE_BASELINE")))

(def delays-timeout-secs (or (some-> (System/getenv "PERF_TIMEOUT_DELAYS") parse-long) 60))
(def parse-timeout-secs (or (some-> (System/getenv "PERF_TIMEOUT_PARSE") parse-long) 400))

(def default-rounds {:lane 1 :flow-2hop 5 :flow-4hop 5 :delays 5 :parse 2})
(def rounds-env-suffix {:lane "LANE" :flow-2hop "FLOW_2HOP" :flow-4hop "FLOW_4HOP" :delays "DELAYS" :parse "PARSE"})

(defn rounds-for [mkey]
  (or (some-> (System/getenv (str "PERF_ROUNDS_" (get rounds-env-suffix mkey))) parse-long)
      (get default-rounds mkey)))

(def all-metrics [:lane :flow-2hop :flow-4hop :delays :parse])

(def selected-metrics
  (if-let [sel (System/getenv "PERF_METRICS")]
    (let [wanted (into #{} (map (comp keyword str/trim)) (str/split sel #","))]
      (vec (filter wanted all-metrics)))
    all-metrics))

(def subset-run? (not= (set selected-metrics) (set all-metrics)))

;; :hard-bar's :cmp is an in-memory fn value only -- never spilled into
;; scoreboard/baseline EDN (only the resulting boolean :bar-pass? is).
(def metric-meta
  {:lane      {:unit "iters/s" :direction :higher :hard-bar {:cmp >= :value 250e6 :label "lane median >= 250M iters/s"}}
   :flow-2hop {:unit "msg/s"   :direction :higher :hard-bar nil}
   :flow-4hop {:unit "msg/s"   :direction :higher :hard-bar nil}
   :delays    {:unit "s"       :direction :lower  :hard-bar {:cmp < :value 8.0 :label "delays median < 8.0s"}}
   :parse     {:unit "s"       :direction :lower  :hard-bar nil}})

(defn bar-pass? [mkey stats]
  (let [bar (:hard-bar (get metric-meta mkey))]
    (if bar ((:cmp bar) (:median stats) (:value bar)) true)))

;; ------------------------------- generic helpers ----------------------------

(defn abort! [msg]
  (binding [*out* *err*]
    (println "================================================================")
    (println "ABORT (perf gate correctness break, or an unparseable measurement):")
    (println msg)
    (println "================================================================"))
  (println (str "#PERF-REGRESSION-SUMMARY " (pr-str {:aborted true :reason msg})))
  (System/exit 1))

(defn median-of [xs]
  (let [s (vec (sort xs)) n (count s)]
    (if (odd? n)
      (nth s (quot n 2))
      (/ (+ (nth s (dec (quot n 2))) (nth s (quot n 2))) 2.0))))

;; A round-fn's result is either a bare scalar (flow-2hop/flow-4hop/
;; delays/parse -- one number per round) or a full {:median :min :max}
;; TRIPLE (lane -- see lane-round!'s doc for why). Normalize a scalar
;; into a degenerate triple (min = max = median = the sample) so every
;; metric's per-round results can be combined through one path.
(defn round->triple [result]
  (if (map? result)
    result
    (let [v (double result)] {:median v :min v :max v})))

;; Combine N per-round triples into the metric's overall stats. For the
;; scalar metrics (every triple degenerate) this reduces EXACTLY to
;; median/min/max over the N raw samples -- the median of N degenerate
;; medians is the median of the samples, and min-of-mins / max-of-maxes
;; are the sample min/max. For lane, this also folds each invocation's
;; OWN internal [min,max] noise band into the overall range instead of
;; discarding it, which is the fix for the point-range flap PERF_ROUNDS_LANE=1
;; would otherwise have (see lane-round!'s doc comment).
(defn combine-triples [triples]
  {:median (double (median-of (map :median triples)))
   :min (double (apply min (map :min triples)))
   :max (double (apply max (map :max triples)))
   :samples (vec (map :median triples))})

;; ---------------------- copied from tools/clojure-suite-run.bb ----------------------
;; (module doc above explains why this is a documented copy, not a require)

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

(defn declared-ns [src]
  (some->> (str/index-of src "(ns ")
           (+ 4)
           (skip-ns-metadata src)
           (subs src)
           (re-find #"^\s*([a-zA-Z0-9_.\-]+)")
           second))

(defn inject-shim-after-ns [test-src shim-src]
  (let [start (str/index-of test-src "(ns ")]
    (if (nil? start)
      (str shim-src "\n\n" test-src)
      (loop [i start depth 0]
        (if (>= i (count test-src))
          (str shim-src "\n\n" test-src)
          (let [c (.charAt test-src i)
                depth' (cond (= c \() (inc depth)
                             (= c \)) (dec depth)
                             :else depth)]
            (if (and (= c \)) (zero? depth'))
              (str (subs test-src 0 (inc i))
                   "\n\n;; ==================== mova clojure.test shim, injected by "
                   "tools/check-perf-regression.bb after the ns form above ====================\n\n"
                   shim-src
                   "\n\n;; ==================== rest of vendored test file ====================\n\n"
                   (subs test-src (inc i)))
              (recur (inc i) depth'))))))))

;; scratch dir: OWN directory (not clojure-suite-run.bb's), so a
;; concurrent conformance-suite run never races this tool's
;; materialization/tmp files or vice versa (same W4 lesson that file's
;; own scratch-dir doc cites, applied to a second, independent tool).
(def scratch-dir
  (str (System/getProperty "java.io.tmpdir") "/perf-regression-run-" (format "%08x" (hash root))))

(defn materialize-shim-as-clojure-test []
  (let [target (io/file scratch-dir "clojure" "test.mova")]
    (io/make-parents target)
    (spit target (str "(ns clojure.test)\n\n" (slurp shim-path)))))

(defn materialize-shim-as-clojure-test-helper []
  (let [target (io/file scratch-dir "clojure" "test_helper.mova")]
    (io/make-parents target)
    (spit target (str "(ns clojure.test-helper)\n\n"
                      (slurp shim-path) "\n\n" (slurp helper-shim-path)))))

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

(defn ensure-suite-materialized! []
  (materialize-shim-as-clojure-test)
  (materialize-shim-as-clojure-test-helper)
  (doseq [f (->> (file-seq (io/file vendor-libs-dir))
                 (filter #(and (.isFile %)
                               (or (str/ends-with? (.getName %) ".clj")
                                   (str/ends-with? (.getName %) ".cljc"))))
                 (map str) sort)]
    (materialize-vendor-lib-file f)))

(def materialized (delay (ensure-suite-materialized!)))

;; ---------------------------- per-metric round fns ---------------------------

;; Returns a full {:median :min :max} TRIPLE, not a scalar -- the Rust
;; test is already median-of-5 internally and prints its own [min-max]
;; noise band (tests/lane_hand_wire_bench.rs:142-149); collapsing that
;; down to a single scalar sample per invocation would throw the noise
;; band away and make the default PERF_ROUNDS_LANE=1 case a degenerate
;; single-point "range" that flips REGRESSED/IMPROVED on any rerun under
;; load (measured: exactly this flap on back-to-back runs before this
;; fix -- see the wave report). `measure-metric!`'s `round->triple` /
;; `combine-triples` merge this triple with the (degenerate,
;; min=max=median) triples the other four metrics' scalar samples become,
;; so one combining path serves both shapes.
(defn lane-round! []
  (let [{:keys [out err exit]} @(p/process {:out :string :err :string :continue true :dir root}
                                           "cargo" "test" "--release" "--test" "lane_hand_wire_bench" "--" "--nocapture")
        m (re-find #"median\s+([0-9.]+)M iters/s\s+\[([0-9.]+)-([0-9.]+)M\]" (or out ""))]
    (when-not m
      (abort! (str "lane: could not find the 'median <X>M iters/s [<min>-<max>M]' line "
                   "(tests/lane_hand_wire_bench.rs:142-149) in cargo test output.\n"
                   "exit=" exit "\n--- stdout ---\n" out "\n--- stderr ---\n" err)))
    (let [[_ med lo hi] m]
      {:median (* 1e6 (Double/parseDouble med))
       :min (* 1e6 (Double/parseDouble lo))
       :max (* 1e6 (Double/parseDouble hi))})))

(defn flow-round! [mova-file expect-name]
  (let [line-re (re-pattern (str "^" expect-name " [0-9]+ [0-9]+ [0-9]+$"))
        {:keys [out err exit]} @(p/process {:out :string :err :string :continue true}
                                           mova-bin (str bench-dir "/" mova-file))
        line (->> (str/split-lines (or out "")) (filter #(re-matches line-re %)) last)]
    (when-not line
      (abort! (str expect-name ": " mova-bin " " bench-dir "/" mova-file
                   " produced no matching '<name> <msgs> <ms> <msg/s>' line.\n"
                   "exit=" exit "\n--- stdout ---\n" out "\n--- stderr ---\n" err)))
    (Double/parseDouble (last (str/split (str/trim line) #"\s+")))))

(defn flow-2hop-round! [] (flow-round! "flow-2hop.mova" "flow-2hop"))
(defn flow-4hop-round! [] (flow-round! "flow-4hop.mova" "flow-4hop"))

(defn assemble-suite-tmp! [vendor-filename]
  (let [file (str vendor-dir "/" vendor-filename)
        src (slurp file)
        shim-src (str (slurp shim-path) "\n\n" (slurp helper-shim-path))
        assembled (inject-shim-after-ns src shim-src)
        run-ns (declared-ns src)
        tmp (str scratch-dir "/" vendor-filename)]
    (io/make-parents tmp)
    (spit tmp (str ";; ==================== vendored test file: " vendor-filename
                   " (shim injected after its ns form) ====================\n\n"
                   assembled
                   (if run-ns (str "\n\n(ns " run-ns ")\n") "\n")
                   "\n((fn [] (ns user) (run-tests)))\n"))
    tmp))

;; Assembles + runs `vendor-filename` under `timeout <secs>s`, measuring
;; wall clock externally (bb's own System/nanoTime bracketing the whole
;; process -- the spec's other allowed option, `/usr/bin/time -p`, would
;; need its own stderr parsing for no accuracy gain here). Verifies the
;; last `#SUMMARY` line is genuinely all-pass before returning the wall
;; time; anything else (timeout, no #SUMMARY line, a non-all-pass
;; #SUMMARY) is a correctness break -- ABORT, never silently "regress".
(defn suite-round! [vendor-filename timeout-secs]
  @materialized
  (let [tmp (assemble-suite-tmp! vendor-filename)
        t0 (System/nanoTime)
        {:keys [out err exit]} @(p/process {:out :string :err :string :continue true}
                                           "timeout" (str timeout-secs "s") mova-bin tmp)
        t1 (System/nanoTime)
        wall-s (/ (- t1 t0) 1e9)
        summary-line (->> (str/split-lines (or out "")) (filter #(str/starts-with? % "#SUMMARY ")) last)]
    (when (= exit 124)
      (abort! (str vendor-filename ": TIMED OUT after " timeout-secs "s -- an actual hang, not a slow "
                   "round; not safe to interpret as a perf sample. Investigate before re-running the gate.")))
    (when-not summary-line
      (abort! (str vendor-filename ": mova exited without printing a #SUMMARY line (blocked/crashed).\n"
                   "exit=" exit "\n--- stderr ---\n" (or err "") "\n--- stdout (tail) ---\n"
                   (str/join "\n" (take-last 20 (str/split-lines (or out "")))))))
    (let [summary (edn/read-string (subs summary-line (count "#SUMMARY ")))
          fail (:fail summary 0) error (:error summary 0)
          assertions (:assertions summary 0) passed (:assertions-passed summary 0)]
      (when-not (and (zero? fail) (zero? error) (pos? assertions) (= assertions passed))
        (abort! (str vendor-filename ": CORRECTNESS BREAK -- #SUMMARY is not all-pass (fail=" fail
                     " error=" error " assertions=" assertions " assertions-passed=" passed
                     "). The perf gate refuses to measure/bless a wall time against a broken run.")))
      wall-s)))

(defn delays-round! [] (suite-round! "delays.clj" delays-timeout-secs))
(defn parse-round! [] (suite-round! "parse.clj" parse-timeout-secs))

(def round-fns
  {:lane lane-round! :flow-2hop flow-2hop-round! :flow-4hop flow-4hop-round!
   :delays delays-round! :parse parse-round!})

;; flow-2hop/flow-4hop get 1 discarded warmup round per the spec's "house:
;; 1 warmup + N measured rounds per metric"; lane is already internally
;; median-of-5-with-its-own-warmup, and delays/parse's own per-round cost
;; (esp. parse's ~200s) makes an extra discarded round too expensive for
;; the noise-reduction it would buy.
(def warmup-metrics #{:flow-2hop :flow-4hop})

(defn measure-metric! [mkey]
  (let [n (rounds-for mkey)
        round-fn (get round-fns mkey)]
    (when (contains? warmup-metrics mkey) (round-fn))
    (let [triples (mapv (fn [_] (round->triple (round-fn))) (range n))]
      (assoc (combine-triples triples)
             :rounds n
             :unit (:unit (get metric-meta mkey))
             :direction (:direction (get metric-meta mkey))))))

;; ------------------------------- verdict / diff ------------------------------

;; Mirrors bench/run.sh's verdict()/verdict_lower_better(): a win/loss is
;; claimed only when the two [min,max] ranges do not overlap at all.
(defn range-verdict [direction old-stats new-stats]
  (let [o-min (:min old-stats) o-max (:max old-stats)
        n-min (:min new-stats) n-max (:max new-stats)]
    (case direction
      :higher (cond (> n-min o-max) :improved
                    (> o-min n-max) :regressed
                    :else :unchanged)
      :lower  (cond (< n-max o-min) :improved
                    (< o-max n-min) :regressed
                    :else :unchanged))))

(defn classify [mkey old new]
  (cond
    (and old (not new)) :vanished
    (and new (not old)) :new
    :else
    (let [dir (:direction (get metric-meta mkey))]
      (if-not (bar-pass? mkey new)
        :regressed ;; hard bar fails independently of the baseline diff
        (range-verdict dir old new)))))

;; ------------------------------- blessing ------------------------------------

(defn provenance-header []
  (str
   ";; bench/PERF-BASELINE.edn\n"
   ";;\n"
   ";; The committed reference point for tools/check-perf-regression.sh.\n"
   ";; Every run of that tool measures the five house benchmarks fresh into\n"
   ";; bench/perf-scoreboard.edn (gitignored, live) and compares them\n"
   ";; per-metric against THIS file -- see tools/check-perf-regression.bb's\n"
   ";; own module doc for the exact REGRESSED/IMPROVED/UNCHANGED/hard-bar\n"
   ";; rules (mirrors tools/check-regression.bb's conformance gate, adapted\n"
   ";; for range-overlap perf verdicts instead of per-file assertion counts).\n"
   ";;\n"
   ";; THIS FILE IS RE-BLESSED ONLY VIA:\n"
   ";;\n"
   ";;   PERF_UPDATE_BASELINE=1 bash tools/check-perf-regression.sh\n"
   ";;\n"
   ";; which overwrites it with the just-measured scoreboard and prints\n"
   ";; loudly what changed relative to the previous baseline. Never\n"
   ";; hand-edit this file -- there is no other supported way to update it.\n\n"))

(defn bless! [scoreboard git-head]
  (when subset-run?
    (abort! "refusing to bless from a PERF_METRICS subset run -- bless only from a full run (all five metrics)."))
  (let [prior (when (.exists (io/file baseline-path)) (edn/read-string (slurp baseline-path)))
        now (str (Instant/now))
        machine-load (str/trim (:out @(p/process {:out :string :continue true} "uptime")))
        baseline {:baseline-provenance
                  {:blessed-at now
                   :blessed-from scoreboard-path
                   :blessed-by "tools/check-perf-regression.sh (PERF_UPDATE_BASELINE=1)"
                   :git-head git-head
                   :machine-load machine-load
                   :provisional-loaded-machine provisional?
                   :rebless-instructions "PERF_UPDATE_BASELINE=1 bash tools/check-perf-regression.sh"}
                  :metrics (:metrics scoreboard)}]
    (io/make-parents baseline-path)
    (spit baseline-path (str (provenance-header) (with-out-str (pprint/pprint baseline))))
    (println "================================================================")
    (println "BLESSING NEW PERF BASELINE -- bench/PERF-BASELINE.edn")
    (println "================================================================")
    (if prior
      (do
        (println (str "Previous baseline blessed at: " (get-in prior [:baseline-provenance :blessed-at] "(unknown)")))
        (doseq [[k v] (sort-by key (:metrics baseline))]
          (let [old (get-in prior [:metrics k])]
            (if old
              (println (format "  %-10s median %.4g -> %.4g %s" (name k) (double (:median old)) (double (:median v)) (:unit v)))
              (println (format "  %-10s NEW (median %.4g %s)" (name k) (double (:median v)) (:unit v)))))))
      (println "No previous baseline existed -- this is the INITIAL bless."))
    (println (format "Blessed %d metrics at %s (git %s%s)"
                     (count (:metrics baseline)) now git-head
                     (if provisional? ", PROVISIONAL/loaded-machine (conductor re-blesses on the quiet machine)" "")))
    (println "================================================================")
    (println (str "#PERF-REGRESSION-SUMMARY " (pr-str {:blessed true :metrics (count (:metrics baseline))
                                                       :blessed-at now :provisional provisional?})))
    (System/exit 0)))

;; -------------------------------- checking ------------------------------------

(defn run-check! [scoreboard]
  (when-not (.exists (io/file baseline-path))
    (binding [*out* *err*]
      (println (str "FAIL: no baseline blessed yet at " baseline-path
                    " -- run: PERF_UPDATE_BASELINE=1 bash tools/check-perf-regression.sh")))
    (System/exit 1))
  (let [baseline (edn/read-string (slurp baseline-path))
        old-metrics (:metrics baseline)
        new-metrics (:metrics scoreboard)
        ;; VANISHED only among metrics this run actually intended to
        ;; measure (selected-metrics) -- a subset run never claims an
        ;; unselected baseline metric vanished, per the spec.
        compare-keys (into (sorted-set)
                           (concat (filter (set selected-metrics) (keys old-metrics)) (keys new-metrics)))
        grouped (group-by (fn [k] (classify k (get old-metrics k) (get new-metrics k))) compare-keys)
        bars-failed (count (filter (fn [k] (let [new (get new-metrics k)] (and new (not (bar-pass? k new)))))
                                   selected-metrics))
        describe (fn [k]
                   (let [old (get old-metrics k) new (get new-metrics k)]
                     (cond
                       (and old (not new))
                       (format "  - %s: baseline median %.4g %s, missing from this run" (name k) (double (:median old)) (:unit old))
                       (and new (not old))
                       (format "  - %s: median %.4g [%.4g-%.4g] %s (not in baseline, bar %s)"
                               (name k) (double (:median new)) (double (:min new)) (double (:max new)) (:unit new)
                               (if (bar-pass? k new) "OK" "FAILED"))
                       :else
                       (format "  - %s: baseline [%.4g-%.4g] -> now [%.4g-%.4g] %s (bar %s)"
                               (name k) (double (:min old)) (double (:max old))
                               (double (:min new)) (double (:max new)) (:unit new)
                               (if (bar-pass? k new) "OK" "FAILED")))))
        section (fn [label ks]
                  (println (format "%s (%d):" label (count ks)))
                  (doseq [k (sort ks)] (println (describe k)))
                  (println))]
    (println "================================================================")
    (println (str "check-perf-regression: " scoreboard-path))
    (println (str "                    vs " baseline-path
                  " (blessed " (get-in baseline [:baseline-provenance :blessed-at] "unknown") ")"))
    (when subset-run? (println (str "SUBSET run -- metrics: " (str/join ", " (map name selected-metrics)))))
    (println "================================================================")
    (println)
    (section "REGRESSED" (get grouped :regressed))
    (section "IMPROVED" (get grouped :improved))
    (println (format "UNCHANGED (%d)" (count (get grouped :unchanged))))
    (println)
    (section "NEW" (get grouped :new))
    (section "VANISHED" (get grouped :vanished))
    (let [summary {:metrics-compared (count compare-keys)
                   :improved (count (get grouped :improved))
                   :unchanged (count (get grouped :unchanged))
                   :regressed (count (get grouped :regressed))
                   :new (count (get grouped :new))
                   :vanished (count (get grouped :vanished))
                   :bars-failed bars-failed
                   :regression? (boolean (or (pos? (count (get grouped :regressed))) (pos? bars-failed)))}]
      (println "----------------------------------------------------------------")
      (println (format "SUMMARY: %d metrics compared, %d improved, %d unchanged, %d regressed, %d new, %d vanished, %d bars failed"
                       (:metrics-compared summary) (:improved summary) (:unchanged summary)
                       (:regressed summary) (:new summary) (:vanished summary) (:bars-failed summary)))
      (println (str "#PERF-REGRESSION-SUMMARY " (pr-str summary)))
      (println "----------------------------------------------------------------")
      (if (:regression? summary)
        (do (println "RESULT: PERF REGRESSION DETECTED -- blocking. If deliberate/understood, re-bless with:")
            (println "        PERF_UPDATE_BASELINE=1 bash tools/check-perf-regression.sh")
            (System/exit 1))
        (do (println "RESULT: no perf regression.")
            (System/exit 0))))))

;; ------------------------------------ main ------------------------------------

(defn -main []
  (when-not (.exists (io/file mova-bin))
    (binding [*out* *err*]
      (println (str "FAIL: MOVA_BIN not found at " mova-bin " -- run: cargo build --release"))))
  (println (str "root: " root))
  (println (str "mova-bin: " mova-bin))
  (println (str "selected metrics: " (str/join ", " (map name selected-metrics))
                (when subset-run? " (SUBSET run)")))
  (println)
  (let [results (into {}
                      (map (fn [mkey]
                             (println (format "-- %s (%d round(s)) --" (name mkey) (rounds-for mkey)))
                             (let [r (measure-metric! mkey)
                                   r (assoc r :bar-pass? (bar-pass? mkey r))]
                               (println (format "   median %.4g  [%.4g-%.4g] %s  (bar %s)"
                                                (double (:median r)) (double (:min r)) (double (:max r))
                                                (:unit r) (if (:bar-pass? r) "OK" (if (:hard-bar (get metric-meta mkey)) "FAILED" "n/a"))))
                               [mkey r])))
                      selected-metrics)
        git-head (str/trim (:out @(p/process {:out :string :continue true :dir root} "git" "rev-parse" "HEAD")))
        scoreboard {:measured-at (str (Instant/now))
                    :git-head git-head
                    :mova-bin mova-bin
                    :metrics results}]
    (println)
    (io/make-parents scoreboard-path)
    (spit scoreboard-path (with-out-str (pprint/pprint scoreboard)))
    (println (str "wrote " scoreboard-path))
    (println)
    (if update-baseline?
      (bless! scoreboard git-head)
      (run-check! scoreboard))))

(-main)
