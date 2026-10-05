;; raw-chan.clj (JVM counterpart) -- identical N/shape/output-contract to
;; ../raw-chan.clj, run against real clojure.core.async v1.9.808-alpha1 on
;; the JVM instead of mova's native channels. Invoked by bench/run.sh as:
;;
;;   clojure -Sdeps '{:deps {org.clojure/core.async {:mvn/version "1.9.808-alpha1"}}}' -M bench/jvm/raw-chan.clj

(require '[clojure.core.async :as a :refer [chan >!! <!! close! thread]])

(def N 800000)
(def ch (chan 1000))
(def t0 (System/currentTimeMillis))

(thread
  (loop [i 0]
    (when (< i N)
      (>!! ch i)
      (recur (inc i))))
  (close! ch))

(loop [n 0]
  (let [v (<!! ch)]
    (if (nil? v)
      (let [elapsed (max 1 (- (System/currentTimeMillis) t0))
            rate (quot (* n 1000) elapsed)]
        (println "raw-chan" n elapsed rate))
      (recur (inc n)))))

(shutdown-agents)
