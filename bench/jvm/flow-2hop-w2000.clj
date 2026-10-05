;; flow-2hop-w2000.clj (JVM counterpart) -- identical N/WORK/shape/output-
;; contract to ../flow-2hop-w2000.clj, run against real
;; clojure.core.async.flow v1.9.808-alpha1. This is the scenario where the
;; JVM's JIT is expected to start winning over mova's tree-walking
;; interpreter -- see bench/RESULTS.md's verdict. Invoked by bench/run.sh
;; as:
;;
;;   clojure -Sdeps '{:deps {org.clojure/core.async {:mvn/version "1.9.808-alpha1"}}}' -M bench/jvm/flow-2hop-w2000.clj

(require '[clojure.core.async.flow :as flow])
(require '[clojure.core.async :as a :refer [chan >!! <!!]])

(defn lcg-work [n seed]
  (loop [i 0 x seed]
    (if (>= i n)
      x
      (recur (inc i) (mod (+ (* x 1103515245) 12345) 2147483648)))))

(def N 900)
(def WORK 2000)
(def done-ch (chan 1))

(def relay
  (flow/map->step
   {:describe (fn [] {:ins {:in {}} :outs {:out {}}})
    :transform (fn [s _ m] (lcg-work WORK m) [s {:out [m]}])}))

(def sink
  (flow/map->step
   {:describe (fn [] {:ins {:in {}} :outs {:done {}}})
    :init (fn [_] {:n 0 :clojure.core.async.flow/out-ports {:done done-ch}})
    :transform (fn [s _ m]
                 (lcg-work WORK m)
                 (let [n2 (inc (:n s))]
                   (if (= n2 N)
                     [(assoc s :n n2) {:done [n2]}]
                     [(assoc s :n n2) {}])))}))

(def fl (flow/create-flow
         {:procs {:r {:proc (flow/process relay)} :sink {:proc (flow/process sink)}}
          :conns [[[:r :out] [:sink :in]]]}))
(flow/start fl)
(flow/resume fl)

(def t0 (System/currentTimeMillis))
(flow/inject fl [:r :in] (range 1 (inc N)))
(<!! done-ch)
(def elapsed (max 1 (- (System/currentTimeMillis) t0)))
(def rate (quot (* N 1000) elapsed))
(println "flow-2hop-w2000" N elapsed rate)
(flow/stop fl)

(shutdown-agents)
