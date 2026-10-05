(ns fx.add-op
  "Adds an op: `fx/add` answers with the sum of `a` and `b`."
  (:require [nrepl.middleware :refer [set-descriptor!]]
            [nrepl.misc :refer [response-for]]
            [nrepl.transport :as t]))

(defn wrap-add-op [h]
  (fn [{:keys [op transport] :as msg}]
    (if (= "fx/add" op)
      (t/send transport
              (response-for msg
                            :status :done
                            :value (str (+ (parse-long (:a msg)) (parse-long (:b msg))))))
      (h msg))))

(set-descriptor! #'wrap-add-op
                 {:requires #{}
                  :expects #{}
                  :handles {"fx/add" {:doc "Adds two integers."
                                      :requires {"a" "First integer, as a string."
                                                 "b" "Second integer, as a string."}
                                      :optional {}
                                      :returns {"value" "The sum, as a string."}}}})
