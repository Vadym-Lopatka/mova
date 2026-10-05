;; Usage: clojure -M range.clj <in-root> <out-root>
;; Replays clojure-lsp's `range-formatting` (feature/format.clj, refactor/edit.clj find-at-pos) for 4 deterministic
;; ranges per file. Writes out-root/ranges.tsv (rel i row col end-row end-col) and out-root/<rel>.r<i>
;; (first line: "sl sc el ec" 0-based LSP range, then the new text) or <rel>.r<i>.ERR.
(require '[cljfmt.config :as cfg] '[cljfmt.core :as cljfmt]
         '[rewrite-clj.node :as n] '[rewrite-clj.parser :as p] '[rewrite-clj.zip :as z]
         '[clojure.java.io :as io] '[clojure.string :as str])

(def settings (update cfg/default-config :alias-map not-empty))

;; ---- copied from clojure-lsp refactor/edit.clj ----
(defn root? [loc] (identical? :forms (z/tag loc)))
(defn top? [loc] (root? (z/up loc)))
(defn to-top [loc] (z/find loc z/up top?))
(defn in-range? [{:keys [row col end-row end-col]} {r :row c :col er :end-row ec :end-col}]
  (and (>= r row) (<= er end-row)
       (if (= r row) (>= c col) true)
       (if (= er end-row) (< ec end-col) true)))
(defn zloc-in-range? [loc pos]
  (or (some-> loc z/node meta (in-range? pos))
      (and (= (-> loc z/node meta :end-col) (:end-col pos))
           (z/rightmost? loc)
           (contains? #{:list :vector :map :set :fn} (z/tag (z/up loc)))
           (some-> loc z/up z/node meta (in-range? pos)))))
(defn find-by-heritability [start-zloc inherits?]
  (loop [zloc (cond-> start-zloc (= :forms (z/tag start-zloc)) z/down*)]
    (if (z/end? zloc)
      zloc
      (if (inherits? zloc)
        (if-let [inner (some-> zloc z/down* (z/find z/right* inherits?))]
          (recur inner)
          zloc)
        (recur (z/right* zloc))))))
(defn to-pos [zloc row col]
  (let [pos {:row row :col col :end-row row :end-col col}]
    (find-by-heritability zloc #(zloc-in-range? % pos))))

(defn take-upto [pred coll]
  (let [[pre post] (split-with (complement pred) coll)] (concat pre (take 1 post))))

(defn range-format [text {:keys [row col end-row end-col]}]
  (let [root-loc (some-> text p/parse-string-all (z/of-node* {}))
        start-loc (or (to-pos root-loc row col) (z/leftmost* root-loc))
        start-top-loc (to-top start-loc)
        end-loc (or (to-pos start-top-loc end-row end-col) (z/rightmost* root-loc))
        end-top-loc (or (to-top end-loc) root-loc)
        forms (->> start-top-loc (iterate z/right*) (take-while (complement z/end?)) (take-upto #(= % end-top-loc)))
        span (merge (-> start-top-loc z/node meta (select-keys [:row :col]))
                    (-> end-top-loc z/node meta (select-keys [:end-row :end-col])))]
    {:range [(dec (:row span)) (dec (:col span)) (dec (:end-row span)) (dec (:end-col span))]
     :new-text (-> (map z/node forms) n/forms-node (cljfmt/reformat-form settings) n/string)}))

(when (seq *command-line-args*)
 (let [[in out] *command-line-args*
      inf (io/file in)
      ip (.getPath (.getCanonicalFile inf))
      files (->> (file-seq inf) (filter #(.isFile ^java.io.File %))
                 (filter #(re-find #"\.(clj[cs]?|bb)$" (.getName ^java.io.File %))))
      tsv (java.io.StringWriter.)]
  (doseq [^java.io.File f files]
    (let [rel (subs (.getPath (.getCanonicalFile f)) (inc (count ip)))
          text (slurp f)
          lines (vec (str/split text #"\n" -1))
          rnd (java.util.Random. (hash rel))
          nl (count lines)]
      (dotimes [i 4]
        (let [r1 (inc (.nextInt rnd nl))
              c1 (inc (.nextInt rnd (inc (count (lines (dec r1))))))
              r2 (min nl (+ r1 (.nextInt rnd 12)))
              c2 (if (= i 3) (inc (count (lines (dec r2)))) (inc (.nextInt rnd (inc (count (lines (dec r2)))))))
              [r2 c2] (if (neg? (compare [r2 c2] [r1 c1])) [r1 c1] [r2 c2])
              pos {:row r1 :col c1 :end-row r2 :end-col c2}
              o (io/file out (str rel ".r" i))]
          (.write tsv (str/join "\t" [rel i r1 c1 r2 c2 "\n"]))
          (io/make-parents o)
          (try
            (let [{:keys [range new-text]} (range-format text pos)]
              (spit o (str (str/join " " range) "\n" new-text)))
            (catch Throwable e
              (spit (io/file (str (.getPath o) ".ERR")) (str e))))))))
  (spit (io/file out "ranges.tsv") (str tsv))))
(shutdown-agents)
