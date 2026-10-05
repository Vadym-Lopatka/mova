(require '[clojure.string :as str])
(let [names (set (str/split-lines (slurp "mova-core-names.txt")))
      vars (->> (ns-publics 'clojure.core)
                (filter (fn [[s _]] (contains? names (str s))))
                (sort-by (comp str key)))
      sb (StringBuilder.)]
  (doseq [[s v] vars]
    (let [m (meta v)]
      (when (or (:doc m) (:arglists m) (:added m))
        (.append sb (str s))
        (.append sb "\u001f") (.append sb (str (:added m)))
        (.append sb "\u001f") (.append sb (if (:arglists m) (pr-str (:arglists m)) ""))
        (.append sb "\u001f") (.append sb (or (:doc m) ""))
        (.append sb "\u001e"))))
  (spit "core-docs.dat" (str sb))
  (println (count vars) "vars," (.length sb) "chars, mova names" (count names)))
