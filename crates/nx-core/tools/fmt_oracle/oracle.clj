;; Usage: clojure -M oracle.clj <in-root> <out-root> [options.edn]
;; Formats every .clj/.cljc/.cljs/.bb/.edn under in-root exactly like clojure-lsp's formatting handler
;; (default cljfmt config, wrap-normalize-newlines, :alias-map not-empty). Writes out-root/<rel>;
;; on parse/format error writes out-root/<rel>.ERR. Also handles an optional range mode via RANGES env (unused).
(require 'clojure.edn '[cljfmt.config :as cfg] '[cljfmt.core :as cljfmt]
         '[clojure.java.io :as io] '[clojure.string :as str])

(defn- merge-configs [a b]
  (-> (merge a b)
      (assoc :indents (merge (:indents a {}) (:indents b)))
      (assoc :extra-indents (merge (:extra-indents a {}) (:extra-indents b)))))

;; optional 3rd arg: EDN file with cljfmt options (as in .cljfmt.edn), merged like clojure-lsp does
(def user-cfg
  (when-let [f (nth *command-line-args* 2 nil)]
    (clojure.edn/read-string {:readers {'re re-pattern}} (slurp f))))

(def settings (-> (merge-configs cfg/default-config user-cfg)
                  (update :alias-map not-empty)))

(defn fmt [text]
  ((cljfmt/wrap-normalize-newlines #(cljfmt/reformat-string % settings)) text))

(let [[in out] *command-line-args*
      inf (io/file in)
      ip (.getPath (.getCanonicalFile inf))
      files (->> (file-seq inf)
                 (filter #(.isFile ^java.io.File %))
                 (filter #(re-find #"\.(clj[cs]?|bb|edn)$" (.getName ^java.io.File %))))]
  (doseq [^java.io.File f files]
    (let [rel (subs (.getPath (.getCanonicalFile f)) (inc (count ip)))
          o (io/file out rel)]
      (io/make-parents o)
      (try
        (spit o (fmt (slurp f)))
        (catch Throwable e
          (spit (io/file (str (.getPath o) ".ERR")) (str (.getMessage e))))))))
(shutdown-agents)
