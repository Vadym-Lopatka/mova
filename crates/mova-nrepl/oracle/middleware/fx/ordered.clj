(ns fx.ordered
  "Two middleware ordered by descriptors. `wrap-audit` requires `wrap-log`, and
  both expect `eval`; each adds its name to `trail` in every eval reply, so the
  trail shows the order the stack was built in."
  (:require [nrepl.middleware :refer [set-descriptor!]]
            [nrepl.transport :as t]))

(defn- trailing-transport [transport name]
  (reify t/Transport
    (recv [_] (t/recv transport))
    (recv [_ timeout] (t/recv transport timeout))
    (send [this resp]
      (t/send transport (update resp :trail (fnil conj []) name))
      this)))

(defn- trailing [name]
  (fn [h]
    (fn [{:keys [op] :as msg}]
      (if (= "eval" op)
        (h (update msg :transport trailing-transport name))
        (h msg)))))

(def wrap-log (trailing "log"))
(def wrap-audit (trailing "audit"))

(set-descriptor! #'wrap-log {:requires #{"clone"}
                             :expects #{"eval"}
                             :handles {}})

(set-descriptor! #'wrap-audit {:requires #{#'wrap-log}
                               :expects #{"eval"}
                               :handles {}})
