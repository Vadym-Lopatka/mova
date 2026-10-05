(ns fx.tag-eval
  "Wraps the transport of `eval` so that every reply carries `tag`."
  (:require [nrepl.middleware :refer [set-descriptor!]]
            [nrepl.transport :as t]))

(defn- tagging-transport [transport]
  (reify t/Transport
    (recv [_] (t/recv transport))
    (recv [_ timeout] (t/recv transport timeout))
    (send [this resp]
      (t/send transport (assoc resp :tag "fx"))
      this)))

(defn wrap-tag-eval [h]
  (fn [{:keys [op] :as msg}]
    (if (= "eval" op)
      (h (update msg :transport tagging-transport))
      (h msg))))

(set-descriptor! #'wrap-tag-eval
                 {:requires #{"clone"}
                  :expects #{"eval"}
                  :handles {}})
