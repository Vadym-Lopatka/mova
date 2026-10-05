;; flow-2hop.clj (JVM counterpart) -- identical N/HOPS/shape/output-contract
;; to ../flow-2hop.clj, run against real clojure.core.async.flow
;; v1.9.808-alpha1. Invoked by bench/run.sh as:
;;
;;   clojure -Sdeps '{:deps {org.clojure/core.async {:mvn/version "1.9.808-alpha1"}}}' -M bench/jvm/flow-2hop.clj

(require '[clojure.core.async.flow :as flow])
(require '[clojure.core.async :as a :refer [chan >!! <!!]])

(def N 1100000)
(def HOPS 2)
(def done-ch (chan 1))

(def relay
  (flow/map->step
   {:describe (fn [] {:ins {:in {}} :outs {:out {}}})
    :transform (fn [s _ m] [s {:out [m]}])}))

(def sink
  (flow/map->step
   {:describe (fn [] {:ins {:in {}} :outs {:done {}}})
    :init (fn [_] {:n 0 :clojure.core.async.flow/out-ports {:done done-ch}})
    :transform (fn [s _ m]
                 (let [n2 (inc (:n s))]
                   (if (= n2 N)
                     [(assoc s :n n2) {:done [n2]}]
                     [(assoc s :n n2) {}])))}))

(def relay-pids (map #(keyword (str "r" %)) (range (dec HOPS))))
(def relay-procs (into {} (map (fn [p] [p {:proc (flow/process relay)}]) relay-pids)))
(def procs (assoc relay-procs :sink {:proc (flow/process sink)}))
(def chain-pids (vec (concat relay-pids [:sink])))
(def conns (vec (map (fn [a b] [[a :out] [b :in]]) chain-pids (rest chain-pids))))

(def fl (flow/create-flow {:procs procs :conns conns}))
(flow/start fl)
(flow/resume fl)

(def t0 (System/currentTimeMillis))
(flow/inject fl [(first chain-pids) :in] (range N))
(<!! done-ch)
(def elapsed (max 1 (- (System/currentTimeMillis) t0)))
(def rate (quot (* N 1000) elapsed))
(println "flow-2hop" N elapsed rate)
(flow/stop fl)

(shutdown-agents)
