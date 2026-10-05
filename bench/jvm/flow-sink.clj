;; flow-sink.clj (JVM counterpart) -- identical N/shape/output-contract to
;; ../flow-sink.clj, run against real clojure.core.async.flow
;; v1.9.808-alpha1. Invoked by bench/run.sh as:
;;
;;   clojure -Sdeps '{:deps {org.clojure/core.async {:mvn/version "1.9.808-alpha1"}}}' -M bench/jvm/flow-sink.clj

(require '[clojure.core.async.flow :as flow])
(require '[clojure.core.async :as a :refer [chan >!! <!!]])

(def N 1600000)
(def done-ch (chan 1))

(def sink
  (flow/map->step
   {:describe (fn [] {:ins {:in {}} :outs {:done {}}})
    :init (fn [_] {:n 0 :clojure.core.async.flow/out-ports {:done done-ch}})
    :transform (fn [s _ m]
                 (let [n2 (inc (:n s))]
                   (if (= n2 N)
                     [(assoc s :n n2) {:done [n2]}]
                     [(assoc s :n n2) {}])))}))

(def fl (flow/create-flow {:procs {:sink {:proc (flow/process sink)}} :conns []}))
(flow/start fl)
(flow/resume fl)

(def t0 (System/currentTimeMillis))
(flow/inject fl [:sink :in] (range N))
(<!! done-ch)
(def elapsed (max 1 (- (System/currentTimeMillis) t0)))
(def rate (quot (* N 1000) elapsed))
(println "flow-sink" N elapsed rate)
(flow/stop fl)

(shutdown-agents)
