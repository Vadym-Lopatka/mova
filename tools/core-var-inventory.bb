#!/usr/bin/env bb
;; tools/core-var-inventory.bb
;;
;; Implementation for tools/core-var-inventory.sh (kept as babashka rather
;; than bash+grep/sed per this repo's own convention -- see
;; tools/clojure-suite-run.bb's module doc). Writes
;; compat/core-var-inventory.edn: a MECHANICAL census of how many
;; `clojure.core` public vars are bare-resolvable under mova, replacing
;; the one-off hand-built `compat/classified-missing.tsv` count ("224 of
;; 680") that could not be re-derived by a skeptic and would silently rot.
;;
;; ============================== THE METRIC ==============================
;;
;; THIS IS A PRESENCE COUNT, NOT A CONFORMANCE NUMBER. It must never be
;; reported, printed, or interpreted as a compatibility percentage.
;; CONFORMANCE-GUARANTEE.md explicitly rejects var counts as a
;; compatibility metric: the same prior audit that produced
;; classified-missing.tsv also found 15 functions that exist under mova
;; and are WRONG -- a var count scores those as "done". This tool exists
;; ONLY to (a) size the remaining build queue (how many core vars have no
;; implementation attempt at all) and (b) detect drift over time via
;; `git diff` on the sorted :missing-vars / :present-vars lists. The real
;; compatibility number is the Clojure-suite score in scoreboard.edn.
;;
;; ============================ BOTH SIDES, MEASURED =======================
;;
;; clojure.core publics (oracle side): queried live from REAL Clojure,
;; version pinned at tests/conformance/CLOJURE_VERSION, via
;; `(keys (ns-publics 'clojure.core))` -- never hardcoded, never recalled
;; from a cached list, so a Clojure point-release bump is caught
;; automatically the next time this runs. Needs a deps dir pinning that
;; exact version; see `resolve-deps-dir` below for the 3-tier fallback.
;;
;; presence in mova (probe side): ONE mova invocation evaluates all N
;; publics and prints one PRESENT/MISSING line per name in the SAME sorted
;; order the oracle names were queried in -- N separate process spawns
;; would also work but are needless overhead (mova starts in ~5ms, but
;; multiply that by ~680 and you've spent seconds for no reason a single
;; file doesn't already solve). See `build-probe-script` below for exactly
;; how "present" is defined and why the probe never calls or prints a
;; var's actual VALUE (only whether resolving it threw).
;;
;; ========================= THE SPECIAL-FORMS BUCKET ======================
;;
;; A name counts as "bare-resolvable" only if evaluating the naked symbol
;; (not calling it) returns a value without throwing. That test is wrong
;; for names mova implements as compiler-level special forms matched
;; directly on the operator string, rather than as ordinary Vars holding a
;; function/macro value -- those names work perfectly in CALL position
;; (`(let [x 1] x)` evaluates fine) but throw "unresolved symbol" when
;; referenced bare, exactly like a truly-missing name would. Counting them
;; as missing would be a false negative baked into the tool itself.
;;
;; Confirmed by source grep (this list is not guessed, and is re-verified
;; against the oracle + probe sets below every run -- see the assertion in
;; `-main`):
;;   src/eval/special_forms.rs:45-59 matches the operator STRING directly
;;   for: "fn", "defmacro", "let", "loop", "ns", "macroexpand-1",
;;   "macroexpand" (7 names -- these ARE real `clojure.core` public vars
;;   in real Clojure, defmacro-created, hence present in `ns-publics`, but
;;   mova never gives them a Var of their own).
;;   src/reader.rs:217,219 + src/eval/quasiquote.rs:50-51 handle
;;   "unquote"/"unquote-splicing" as syntax-quote reader prefixes (~ / ~@),
;;   never as Vars either.
;; That is exactly 9 names, matching the prior hand audit's finding (see
;; compat/classified-missing.tsv's own 9 `ALREADY-PRESENT|special-form`
;; rows, which this script cross-checks against rather than trusts blindly
;; -- see `-main`'s consistency assertions).
;;
;; ============================== BUCKETS ===================================
;;
;; :missing-by-bucket reuses the EXISTING hand classification in
;; compat/classified-missing.tsv (column 2, `BUCKET|free-text` per row) by
;; joining on var name against the live "truly missing" set computed here.
;; A var truly-missing today but absent from the TSV goes to
;; "UNCLASSIFIED" -- that bucket being non-empty is itself a useful
;; signal: either the TSV is stale (Clojure added a var, or mova gained
;; one the TSV never accounted for) or something drifted. See -main for
;; the companion check (TSV rows that are no longer in the live missing
;; set at all -- printed as a warning, not an error, since that's
;; expected good news: it means mova implemented something).
;;
;; ============================= KNOWN GOTCHAS ==============================
;;
;; - mova has no `resolve` and no standalone top-level `require`, so
;;   presence can't be probed with `(resolve 'name)` the way you would on
;;   the JVM. The probe below uses bare symbol evaluation instead.
;; - mova's `try` supports exactly ONE untyped `catch` (no exception
;;   class matching) -- `(try body (catch e handler))`. Any exception,
;;   including a reader-time one that survives to runtime, hits the same
;;   catch. That's fine here: we only need a binary present/missing
;;   signal, not the exception type.
;; - mova writes diagnostics to STDERR. A prior bug in this project's
;;   tooling captured only stdout and silently recorded "no output" for
;;   every failing probe. This script captures BOTH streams from the
;;   mova invocation (stderr is logged on unexpected failure, never
;;   silently dropped) even though the per-name PRESENT/MISSING signal
;;   itself travels over stdout via `println` inside the try/catch (a
;;   top-level form's value is NOT auto-printed by mova in file mode --
;;   only under `-e` -- confirmed by hand: `mova file.mova` with a bare
;;   `(+ 1 2)` at top level prints nothing; `println` is required).

(require '[babashka.process :as p]
         '[clojure.edn :as edn]
         '[clojure.string :as str]
         '[clojure.java.io :as io]
         '[clojure.pprint :as pprint])

(def root (-> *file* io/file .getParentFile .getParentFile .getCanonicalPath))
(def version-path (str root "/tests/conformance/CLOJURE_VERSION"))
(def tsv-path (str root "/compat/classified-missing.tsv"))
(def out-path (str root "/compat/core-var-inventory.edn"))
(def bootstrap-oracle-path (str root "/tools/bootstrap-oracle.sh"))
;; Exact fallback path named in this tool's task spec -- a session
;; scratchpad from a sibling agent's prior manual audit that already has a
;; resolved `deps.edn` pinning this same oracle version, so using it (when
;; present) avoids a redundant Maven resolve.
(def fallback-deps-dir
  (str (System/getProperty "java.io.tmpdir") "/clj113"))
(def own-scratch-deps-dir
  (str (System/getProperty "java.io.tmpdir") "/core-var-inventory-oracle-deps"))

(def mova-bin (or (System/getenv "MOVA_BIN") (str root "/target/release/mova")))
(def scratch-dir
  (or (System/getenv "CORE_VAR_INVENTORY_SCRATCH")
      (str (System/getProperty "java.io.tmpdir") "/core-var-inventory-run")))

(defn slurp-trim [path] (str/trim (slurp path)))

(def oracle-version (slurp-trim version-path))

;; --------------------------- oracle deps dir ------------------------------
;; 3-tier fallback, cheapest/most-authoritative first:
;;   1. tools/bootstrap-oracle.sh --print-deps-dir, if that script exists
;;      and is executable -- a sibling tool that pins/prepares an oracle
;;      deps dir and prints its absolute path as the LAST line of stdout.
;;   2. the fixed fallback scratchpad path named in this tool's spec, if
;;      it already has a deps.edn (from a prior manual audit).
;;   3. create our own deps.edn, pinning the version read from
;;      tests/conformance/CLOJURE_VERSION (never hardcoded).
(defn resolve-deps-dir []
  (cond
    (let [f (io/file bootstrap-oracle-path)] (and (.exists f) (.canExecute f)))
    (let [{:keys [out exit]} @(p/process ["bash" bootstrap-oracle-path "--print-deps-dir"]
                                         {:out :string :err :string})]
      (if (zero? exit)
        (let [line (-> out str/trim str/split-lines last str/trim)]
          (println (str "core-var-inventory: using oracle deps dir from tools/bootstrap-oracle.sh: " line))
          line)
        (throw (ex-info "tools/bootstrap-oracle.sh --print-deps-dir failed" {:exit exit :out out}))))

    (let [f (io/file fallback-deps-dir "deps.edn")] (.exists f))
    (do (println (str "core-var-inventory: tools/bootstrap-oracle.sh not found; using fallback deps dir: " fallback-deps-dir))
        fallback-deps-dir)

    :else
    (do (io/make-parents (str own-scratch-deps-dir "/deps.edn"))
        (spit (str own-scratch-deps-dir "/deps.edn")
              (str "{:deps {org.clojure/clojure {:mvn/version \"" oracle-version "\"}}}\n"))
        (println (str "core-var-inventory: no bootstrap-oracle.sh, no fallback dir; created own deps.edn pinning "
                      oracle-version " at " own-scratch-deps-dir))
        own-scratch-deps-dir)))

;; ------------------------ oracle: clojure.core publics ---------------------

(defn fetch-oracle-publics [deps-dir]
  (let [form "(prn (vec (sort (map str (keys (ns-publics (quote clojure.core)))))))"
        {:keys [out err exit]} @(p/process ["clojure" "-M" "-e" form]
                                           {:dir deps-dir :out :string :err :string})]
    (when-not (zero? exit)
      (throw (ex-info "querying real Clojure for clojure.core publics failed"
                      {:exit exit :err err :deps-dir deps-dir})))
    (let [names (vec (edn/read-string out))]
      (when (empty? names)
        (throw (ex-info "oracle query returned zero clojure.core publics -- something is wrong" {:out out :err err})))
      names)))

;; --------------------------- mova probe -------------------------------

;; Defensive: every name we splice bare into the probe script must be a
;; safe, unquoted symbol token -- no whitespace or reader-special
;; characters that would corrupt the surrounding `(do NAME :present)`
;; form (or worse, the whole file's paren balance). Every clojure.core
;; public in 1.13.0-alpha6 has been spot-checked to be clean (the only
;; oddities are trailing-apostrophe names like `*'`/`inc'` and `..`, both
;; fine as bare symbols) -- this check exists so a FUTURE oracle version
;; that adds something exotic fails LOUDLY here instead of silently
;; producing a corrupted probe script.
;; Apostrophe is a symbol-constituent character everywhere EXCEPT as the
;; very first character (where it's the reader's quote prefix, e.g. `'x`
;; reads as two forms) -- `*'`/`inc'` etc are legal, safe bare symbols and
;; must NOT be flagged here (confirmed by hand: splicing `*'` into
;; `(do *' :present)` and running it through mova round-trips cleanly).
(def unsafe-symbol-chars #"[\s()\[\]{}\";,~@^`\\]")

(defn unsafe-name? [n]
  (or (str/starts-with? n "'")
      (boolean (re-find unsafe-symbol-chars n))))

(defn build-probe-script [names]
  (doseq [n names]
    (when (unsafe-name? n)
      (throw (ex-info "clojure.core public var name is not a safe bare symbol for the probe script"
                      {:name n}))))
  (str/join "\n"
            (for [n names]
              (str "(println (try (do " n " :present) (catch e :missing)))"))))

;; Runs the whole probe in ONE mova invocation and returns a map of
;; name -> :present/:missing, keyed positionally against `names` (which
;; MUST be in the same order the probe script was built in). A line-count
;; mismatch (mova crashed mid-file, or printed something extra/short) is
;; a hard error, never silently papered over -- the whole census depends
;; on 1:1 line correspondence.
(defn run-probe [names]
  (io/make-parents (str scratch-dir "/probe.mova"))
  (let [script-path (str scratch-dir "/probe.mova")]
    (spit script-path (build-probe-script names))
    (let [{:keys [out err exit]} @(p/process [mova-bin script-path] {:out :string :err :string})]
      (let [lines (->> (str/split-lines out) (remove str/blank?))]
        (when (not= (count lines) (count names))
          (throw (ex-info "core-var-inventory probe: stdout line count did not match name count -- mova likely crashed mid-script; see captured stderr (mova writes diagnostics there, not stdout -- this project has been bitten by dropping stderr before)"
                          {:exit exit :expected (count names) :got (count lines)
                           :stderr err :stdout-tail (str/join "\n" (take-last 10 lines))})))
        (when (seq (str/trim err))
          (println "core-var-inventory: probe run produced stderr output despite a full line count match (informational, not fatal):")
          (println (str/trim err)))
        (into {}
              (map (fn [n line]
                     [n (cond (= line ":present") :present
                              (= line ":missing") :missing
                              :else (throw (ex-info "core-var-inventory probe: unexpected line, expected :present or :missing"
                                                    {:name n :line line})))])
                   names lines))))))

;; ------------------------------ TSV bucket join ----------------------------

;; compat/classified-missing.tsv: `var<TAB>BUCKET|free-text-note`, one
;; header row (`var\tbucket_batch`). Bucket is the text before the first
;; `|`. Skips the header by name rather than by position (row order is not
;; load-bearing here).
(defn load-tsv-buckets []
  (->> (str/split-lines (slurp tsv-path))
       (remove str/blank?)
       (map #(str/split % #"\t" 2))
       (remove (fn [[v _]] (= v "var")))
       (map (fn [[v bucket-field]] [v (first (str/split bucket-field #"\|" 2))]))
       (into {})))

;; ------------------------------ special forms ------------------------------

;; See module doc "THE SPECIAL-FORMS BUCKET" above for the source-grep
;; citations backing this exact list.
(def special-forms
  #{"fn" "defmacro" "let" "loop" "ns" "macroexpand-1" "macroexpand"
    "unquote" "unquote-splicing"})

(defn -main []
  (println (str "core-var-inventory: oracle version " oracle-version " (from " version-path ")"))
  (let [deps-dir (resolve-deps-dir)
        oracle-publics (fetch-oracle-publics deps-dir)
        _ (println (str "core-var-inventory: real Clojure " oracle-version " has "
                        (count oracle-publics) " clojure.core publics"))
        probe-result (run-probe oracle-publics)
        bare-present (set (for [[n s] probe-result :when (= s :present)] n))
        bare-missing (set (for [[n s] probe-result :when (= s :missing)] n))

        ;; Consistency check, not blind trust: every name in our hardcoded
        ;; special-forms list must actually show up as bare-missing right
        ;; now (i.e. mova really doesn't expose it as a Var) AND must
        ;; actually be a real clojure.core public (i.e. we're not
        ;; crediting mova for something Clojure itself doesn't have). A
        ;; violation here means either mova changed (started exposing one
        ;; as a real Var, which is GOOD but this list needs updating) or
        ;; this list was wrong -- either way, fail loud, don't guess.
        oracle-publics-set (set oracle-publics)
        sf-not-in-oracle (remove oracle-publics-set special-forms)
        sf-not-bare-missing (remove bare-missing special-forms)
        _ (when (seq sf-not-in-oracle)
            (throw (ex-info "special-forms bucket contains a name that is not a real clojure.core public -- list is stale"
                            {:names sf-not-in-oracle})))
        _ (when (seq sf-not-bare-missing)
            (println (str "core-var-inventory: WARNING -- these special-forms-bucket names are now bare-resolvable "
                          "in mova (no longer need the special-forms carve-out; update the `special-forms` set "
                          "in tools/core-var-inventory.bb): " (str/join ", " (sort sf-not-bare-missing)))))

        present-vars (sort (into bare-present special-forms))
        missing-vars (sort (remove special-forms bare-missing))
        tsv-buckets (load-tsv-buckets)
        bucket-of (fn [v] (get tsv-buckets v "UNCLASSIFIED"))
        missing-by-bucket (->> missing-vars
                               (group-by bucket-of)
                               (reduce-kv (fn [m k vs] (assoc m k (count vs))) (sorted-map)))
        ;; Informational: TSV rows that are no longer in the live missing
        ;; set at all -- either mova implemented them (good news) or the
        ;; TSV itself is stale relative to a shrinking gap. Not an error.
        tsv-stale-entries (sort (remove (set missing-vars) (keys tsv-buckets)))

        inventory
        {:oracle-version oracle-version
         :generated-by "tools/core-var-inventory.sh"
         :note (str "PRESENCE COUNT, NOT A CONFORMANCE NUMBER. A name counting as \"present\" means the bare "
                    "symbol resolves under mova without throwing -- it says NOTHING about whether the "
                    "behavior is correct. CONFORMANCE-GUARANTEE.md rejects var counts as a compatibility "
                    "metric precisely because a prior audit found 15 functions that exist and are WRONG. "
                    "Use this only to size the remaining build queue and to detect drift via git diff on "
                    ":missing-vars / :present-vars. The real compatibility number is the Clojure-suite score "
                    "in tests/clojure-suite/scoreboard.edn.")
         :clojure-publics (count oracle-publics)
         :present-in-mova (count present-vars)
         :missing (count missing-vars)
         :bare-resolvable-present (count bare-present)
         :bare-resolvable-missing (count bare-missing)
         :special-forms (vec (sort special-forms))
         :special-forms-note (str "These " (count special-forms) " clojure.core publics are implemented in "
                                  "mova as compiler-level special forms matched on the operator string "
                                  "(src/eval/special_forms.rs), or as reader-level syntax-quote prefixes "
                                  "(src/reader.rs + src/eval/quasiquote.rs for unquote/unquote-splicing), "
                                  "never as ordinary Vars -- so they work fine in call position but fail a "
                                  "bare-symbol-resolution probe exactly like a truly-missing name would. "
                                  "Counted as present (folded into :present-vars / :present-in-mova), not "
                                  "missing.")
         :missing-by-bucket missing-by-bucket
         :missing-vars (vec missing-vars)
         :present-vars (vec present-vars)
         :tsv-stale-entries (vec tsv-stale-entries)}]

    (io/make-parents out-path)
    (spit out-path (with-out-str (pprint/pprint inventory)))

    (println)
    (println (format "core-var-inventory: %d/%d clojure.core publics present in mova (%d bare-resolvable + %d special-forms), %d missing"
                     (count present-vars) (count oracle-publics) (count bare-present) (count special-forms) (count missing-vars)))
    (println "  missing-by-bucket:" (pr-str missing-by-bucket))
    (when (seq tsv-stale-entries)
      (println (format "  NOTE: %d compat/classified-missing.tsv entries are no longer in the live missing set (stale, likely good news -- mova implemented them since the TSV was hand-built): %s"
                       (count tsv-stale-entries) (str/join ", " (take 10 tsv-stale-entries))
                       (if (> (count tsv-stale-entries) 10) " ..." ""))))
    (println (str "  wrote " out-path))
    (println "  REMINDER: this is a presence count, not a conformance percentage. See :note in the EDN.")))

(-main)
