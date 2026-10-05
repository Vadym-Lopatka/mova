;; Regenerates src/analyzer/lint/specs.txt: `cd nx/oracle && clojure -M <this file> > crates/nx-core/src/analyzer/lint/specs.txt`
;; (clj-kondo built-in type specs: ns name arities-edn per line).
(require '[clj-kondo.impl.types :as t])
(doseq [[ns specs] (sort-by (comp str key) t/built-in-specs)
        [sym spec] (sort-by (comp str key) specs)
        :when (:arities spec)]
  (println (str ns) (str sym) (pr-str (:arities spec))))
