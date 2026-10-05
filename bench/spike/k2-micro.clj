(ns k2.micro
  (:require [clj-kondo.impl.utils :as u]
            [clj-kondo.impl.parser :as p]
            [clj-kondo.impl.rewrite-clj.node.protocols :as node]))
(def tok (first (:children (p/parse-string "foo"))))
(def lst (first (:children (p/parse-string "(foo bar)"))))
(defn f1 [x] x)
(defn f3 [x y z] y)
(defn kget [x] (:value x))
(defn b-empty [n x] (loop [i 0 a nil] (if (< i n) (recur (inc i) x) a)))
(defn b-native [n x] (loop [i 0 a nil] (if (< i n) (recur (inc i) (identity x)) a)))
(defn b-f1 [n x] (loop [i 0 a nil] (if (< i n) (recur (inc i) (f1 x)) a)))
(defn b-f3 [n x] (loop [i 0 a nil] (if (< i n) (recur (inc i) (f3 x x x)) a)))
(defn b-kw [n x] (loop [i 0 a nil] (if (< i n) (recur (inc i) (:value x)) a)))
(defn b-kget [n x] (loop [i 0 a nil] (if (< i n) (recur (inc i) (kget x)) a)))
(defn b-ptag [n x] (loop [i 0 a nil] (if (< i n) (recur (inc i) (node/tag x)) a)))
(defn b-utag [n x] (loop [i 0 a nil] (if (< i n) (recur (inc i) (u/tag x)) a)))
(defn b-let [n x] (loop [i 0 a nil] (if (< i n) (recur (inc i) (let [y x z y] (if z y x))) a)))
(defn b-assoc [n x] (loop [i 0 a nil] (if (< i n) (recur (inc i) (assoc x :k i)) a)))
(def only (System/getenv "K2B"))
(doseq [[l f x] [["empty" b-empty tok] ["native identity" b-native tok] ["clj f1" b-f1 tok] ["clj f3" b-f3 tok]
                 ["kw inline" b-kw tok] ["kget fn" b-kget tok] ["proto tag tok" b-ptag tok] ["proto tag seq" b-ptag lst]
                 ["u/tag tok" b-utag tok] ["let/if" b-let tok] ["assoc rec" b-assoc tok]]
        :when (or (nil? only) (= only l))]
  (f 100000 x)
  (let [n (if only 20000000 1000000)
        ts (vec (for [_ (range 5)] (let [t0 (System/nanoTime)] (f n x) (- (System/nanoTime) t0))))]
    (println l (format "%.1f" (/ (apply min ts) (double n))) "ns/iter")))
