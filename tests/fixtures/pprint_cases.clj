(require '[clojure.pprint :as pp])
(def forms ['(vec (range 30))
 '{:name "x" :items (vec (range 30)) :nested {:k (vec (range 40)) :z "a long string value here to force breaks of lines"}}
 '(list 1 2 3)
 ''(defn foo [x y] (let [a 1 b 2] (+ a b x y 1000000 2000000 3000000 4000000 5000000 6000000)))
 '(vec (repeat 5 (vec (range 20))))
 '{:a {:b {:c {:d 1}}}}
 '{:long-key-number-one [1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30]}
 '[[1 2 [3 4 [5 [6]]]] #{} {} ()]
 '(range 5)])
(prn (vec (concat
  (map (fn [f] (with-out-str (pp/pprint (eval f)))) forms)
  [(with-out-str (binding [*print-length* 5] (pp/pprint (range 30))))
   (with-out-str (binding [*print-level* 2] (pp/pprint [1 [2 [3 [4]]] {:a {:b 1}}])))
   (pp/write (vec (range 12)) :right-margin 20 :stream nil)
   (with-out-str (pp/pprint (vec (range 12)) *out*))])))
