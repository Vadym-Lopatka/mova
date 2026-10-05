;; Portable Clojure/Mova op-cost scoreboard. Runs unchanged on JVM and Mova.
(defn f0 [] 1)
(defn f1 [x] (+ x 1))
(defn f3 [x y z] (+ x y z))
(defn fm
  ([x] x)
  ([x y] (+ x y))
  ([x y z] (+ x y z)))
(defn fv [x & more] (+ x (count more)))
(def m4 {:a 1 :b 2 :c 3 :d 4})
(defprotocol Sizeable (psize [this]))
(deftype Box [v] Sizeable (psize [this] v))
(def box (Box. 7))
(defmulti mm class)
(defmethod mm :default [x] (inc x))

(defn t-empty [n] (loop [i 0 acc 0] (if (< i n) (recur (inc i) (+ acc i)) acc)))
(defn t-call0 [n] (loop [i 0 acc 0] (if (< i n) (recur (inc i) (+ acc (f0))) acc)))
(defn t-call1 [n] (loop [i 0 acc 0] (if (< i n) (recur (inc i) (+ acc (f1 i))) acc)))
(defn t-call3 [n] (loop [i 0 acc 0] (if (< i n) (recur (inc i) (+ acc (f3 i i i))) acc)))
(defn t-local-fn [n g] (loop [i 0 acc 0] (if (< i n) (recur (inc i) (+ acc (g i))) acc)))
(defn t-closure [n]
  (let [k 10 g (fn [x] (+ x k))]
    (loop [i 0 acc 0] (if (< i n) (recur (inc i) (+ acc (g i))) acc))))
(defn t-multi-arity [n] (loop [i 0 acc 0] (if (< i n) (recur (inc i) (+ acc (fm i i))) acc)))
(defn t-variadic [n] (loop [i 0 acc 0] (if (< i n) (recur (inc i) (+ acc (fv i i i))) acc)))
(defn t-var-deref [n] (loop [i 0 acc 0] (if (< i n) (recur (inc i) (+ acc (:a m4) (count m4) 0)) acc)))
(defn t-kw-get [n] (loop [i 0 acc 0] (if (< i n) (recur (inc i) (+ acc (:a m4))) acc)))
(defn t-get [n] (loop [i 0 acc 0] (if (< i n) (recur (inc i) (+ acc (get m4 :b))) acc)))
(defn t-assoc [n] (loop [i 0 acc 0] (if (< i n) (recur (inc i) (+ acc (:e (assoc m4 :e i)))) acc)))
(defn t-conj [n] (loop [i 0 acc []] (if (< i n) (recur (inc i) (conj (if (> (count acc) 8) [] acc) i)) (count acc))))
(defn t-let [n] (loop [i 0 acc 0] (if (< i n) (let [a i b (+ a 1) c (+ a b)] (recur (inc i) (+ acc c))) acc)))
(defn t-destructure [n] (loop [i 0 acc 0] (if (< i n) (let [{:keys [a b]} m4] (recur (inc i) (+ acc a b))) acc)))
(defn t-protocol [n] (loop [i 0 acc 0] (if (< i n) (recur (inc i) (+ acc (psize box))) acc)))
(defn t-multimethod [n] (loop [i 0 acc 0] (if (< i n) (recur (inc i) (+ acc (mm i))) acc)))
(defn t-instance [n] (loop [i 0 acc 0] (if (< i n) (recur (inc i) (if (instance? Box box) (+ acc 1) acc)) acc)))
(defn t-lazy-map [n] (loop [i 0 acc 0] (if (< i n) (recur (inc i) (+ acc (first (map inc [i])))) acc)))
(defn t-reduce [n] (loop [i 0 acc 0] (if (< i n) (recur (inc i) (+ acc (reduce + [1 2 3 4]))) acc)))

(defn median5 [f n]
  (f (min n 1000))
  (let [ts (for [_ (range 5)]
             (let [t0 (System/nanoTime)]
               (f n)
               (- (System/nanoTime) t0)))]
    (/ (double (nth (sort ts) 2)) n)))

(defn row [name f n]
  (println "ROW" name (format "%.1f" (median5 f n))))

(def N 1000000)
(def NS 100000)

(row "empty-loop" t-empty N)
(row "call-0arg" t-call0 N)
(row "call-1arg" t-call1 N)
(row "call-3arg" t-call3 N)
(row "call-local-fn" #(t-local-fn % f1) N)
(row "call-closure-capture" t-closure N)
(row "call-multi-arity" t-multi-arity N)
(row "call-variadic" t-variadic N)
(row "var-deref" t-var-deref N)
(row "kw-get-4map" t-kw-get N)
(row "get-4map" t-get N)
(row "assoc-4map" t-assoc N)
(row "conj-vector" t-conj N)
(row "let-binding" t-let N)
(row "destructure-keys" t-destructure N)
(row "protocol-call" t-protocol N)
(row "multimethod" t-multimethod N)
(row "instance?" t-instance N)
(row "lazy-map-per-elem" t-lazy-map NS)
(row "reduce-per-step" t-reduce NS)
