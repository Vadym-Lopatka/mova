;; Portable bench: runs on `mova` (interp / MOVA_JIT=1) and `clojure -M`.
;; Mimics clj-kondo's rewrite-clj node walk (token.clj TokenNode, seq.clj
;; SeqNode) and analyzer hot loop (case-dispatch on tag, reduce over
;; children, one get-in read per node).

(defprotocol Node (tag [n]))
(defrecord TokenNode [value] Node (tag [_] :token))
(defrecord SeqNode [tag* children] Node (tag [_] tag*))

(defn mk-vector-node []
  (->SeqNode :vector [(->TokenNode :kw) (->TokenNode 'sym) (->TokenNode 42)]))

(defn mk-list-node []
  (->SeqNode :list
             (vec (concat
                    (for [i (range 9)]
                      (->TokenNode (case (mod i 3) 0 :kw 1 'sym 2 i)))
                    [(mk-vector-node)]))))

(def forms (vec (repeatedly 1000 mk-list-node)))

;; ~1000 lists + 1000*9 tokens + 1000 nested vector-nodes + 1000*3 tokens
;; = 14000 nodes.
(def node-count 14000)

(defn analyze [ctx node]
  (get-in ctx [:config :lang])
  (case (tag node)
    :token (update ctx :tokens inc)
    (:list :vector) (reduce analyze (assoc ctx :depth (inc (:depth ctx))) (:children node))))

(defn run-once []
  (:tokens (reduce analyze {:tokens 0 :depth 0 :config {:lang :clj}} forms)))

(defn median [v]
  (nth (sort v) 2))

(defn bench []
  (dotimes [_ 3] (run-once))
  (let [checksum (run-once)
        samples (vec (repeatedly 5
                                 (fn []
                                   (let [t0 (System/nanoTime)]
                                     (run-once)
                                     (- (System/nanoTime) t0)))))
        med-ns (median samples)
        ns-per-node (/ (double med-ns) node-count)]
    (println "samples-ns:" samples)
    (println "median-ns:" med-ns)
    (println "node-count:" node-count)
    (println "ns-per-node:" ns-per-node)
    (println "checksum:" checksum)))

(bench)
