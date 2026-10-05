(ns fx.identity
  "Does nothing: passes every message on."
  (:require [nrepl.middleware :refer [set-descriptor!]]))

(defn wrap-identity [h]
  (fn [msg] (h msg)))

(set-descriptor! #'wrap-identity {:requires #{} :expects #{} :handles {}})
