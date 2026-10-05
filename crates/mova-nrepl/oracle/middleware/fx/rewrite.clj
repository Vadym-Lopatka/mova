(ns fx.rewrite
  "Changes the message and passes it on: evals of the code `(rewrite-me)` run
  `(+ 20 22)`; others are untouched."
  (:require [nrepl.middleware :refer [set-descriptor!]]))

(defn wrap-rewrite [h]
  (fn [{:keys [op code] :as msg}]
    (h (if (and (= "eval" op) (= "(rewrite-me)" code))
         (assoc msg :code "(+ 20 22)")
         msg))))

(set-descriptor! #'wrap-rewrite {:requires #{"clone"} :expects #{"eval"} :handles {}})
