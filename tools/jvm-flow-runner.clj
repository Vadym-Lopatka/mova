;; jvm-flow-runner.clj -- evaluates a `;;ENGINE jvm-flow` corpus file's forms,
;; in order, in ONE session, against REAL `clojure.core.async.flow` on the
;; JVM. Invoked by tools/gen-golden.bb (see that file's `jvm-flow-golden!`)
;; as:
;;
;;   CORPUS_FILE=<abs-path-to-corpus> \
;;     clojure -Sdeps '{:deps {org.clojure/core.async
;;                              {:mvn/version "1.9.808-alpha1"}}}' \
;;     -M -e '(load-file "tools/jvm-flow-runner.clj")'
;;
;; (the corpus path travels via an env var, not a CLI arg -- `clojure -M -e
;; <form> <extra-args>` treats trailing positional args as script files to
;; load, not as *command-line-args* for the -e form, so a CLI arg doesn't
;; work here; an env var sidesteps that entirely).
;;
;; This file's own top-level `(ns jvm-flow-runner ...)` becomes the ONE
;; session every corpus form evaluates in -- forms may `def` and have later
;; forms in the SAME corpus file see those defs, matching every other
;; engine's session model (one corpus FILE == one session; see
;; tools/gen-golden.bb's own module doc and tests/conformance_test.rs's
;; matching one-`Interp`-per-file rule). Because gen-golden.bb shells out to
;; a FRESH `clojure` process per `;;ENGINE jvm-flow` corpus file, there is
;; no need for the multi-file namespace-per-file trick bb's own script uses
;; internally (bb evaluates every corpus file in a single long-lived
;; process) -- one process here already IS one file's session.
;;
;; Output contract (must match every other engine's golden format exactly,
;; see tools/gen-golden.bb's module doc): for each corpus form, in order,
;; print ONE line to stdout:
;;   OK<TAB><pr-str of the form's result>
;;   ERR<TAB><ExceptionSimpleName>         (the form threw)
;; The exception line carries `(.getSimpleName (class e))` of the caught
;; Throwable, RAW/outer -- no unwrapping of
;; `clojure.lang.Compiler$CompilerException` to its `.getCause` -- exactly
;; like tools/jvm-pending-runner.clj's own `eval-form` and
;; tools/jvm-runner.clj's. NEVER the exception message --
;; CONFORMANCE-GUARANTEE.md's canonicalization rule 6 compares exceptions
;; by occurrence and coarse kind only, never by message text. ONLY those
;; lines go to stdout -- everything else (JVM/clojure.core.async startup
;; noise, per-form error detail for debugging a failing corpus form) is
;; sent to *err*, since gen-golden.bb captures stdout verbatim as the
;; .golden file contents.

(ns jvm-flow-runner
  (:require [clojure.string :as str]))

(defn skip-line?
  "Must match tools/gen-golden.bb's `skip-line?` / conformance_test.rs's
  `is_skippable` exactly: blank, or a `;;`-prefixed comment (after leading
  whitespace) -- this also transparently skips the `;;ENGINE jvm-flow`
  directive line itself, since it starts with `;;` like any other comment."
  [line]
  (let [t (str/trim line)]
    (or (str/blank? t) (str/starts-with? t ";;"))))

(defn prelude-form
  "Same opt-in mechanism as tools/gen-golden.bb's own `;;PRELUDE <code>`:
  the code text after `;;PRELUDE ` on the first line that has that prefix,
  evaluated once before any real form, itself producing no golden line.
  Not needed by flow.corpus today (the `flow`/`a` aliases and channel-op
  refers below already cover it), but kept for parity/extensibility."
  [lines]
  (some (fn [line]
          (let [t (str/trim line)]
            (when (str/starts-with? t ";;PRELUDE ")
              (subs t (count ";;PRELUDE ")))))
        lines))

(defn eval-form
  "Evaluates one form's source text in the current (session) namespace,
  returning the golden line for it -- mirrors tools/gen-golden.bb's
  `eval-form` (`load-string` there, `read-string`+`eval` here, since bb's
  `load-string` isn't available; behaviorally identical: read once,
  evaluate once, in the current ns). On error, the golden line carries the
  caught exception's simple class name -- see this file's own module doc
  and tools/jvm-pending-runner.clj's `eval-form`, whose derivation this
  matches exactly (RAW/outer exception, no CompilerException unwrapping)."
  [src]
  (try
    (let [v (eval (read-string src))]
      (str "OK\t" (pr-str v)))
    (catch Throwable e
      (binding [*out* *err*]
        (println "jvm-flow-runner: form errored:" (pr-str src))
        (println "  ->" (.getSimpleName (class e)) "--" (or (.getMessage e) (str e))))
      (str "ERR\t" (.getSimpleName (class e))))))

(defn -main []
  ;; `flow`/`a` aliases give corpus text like `(flow/create-flow ...)` and
  ;; `(a/whatever ...)` the exact same reading on both engines (mova's
  ;; `flow/`-prefixed globals are literal flat symbol names, so no alias is
  ;; needed there -- see FLOW-DESIGN.md). The :refer list brings the bare
  ;; channel-op names mova has as globals into scope here too, matching
  ;; async.corpus's `;;PRELUDE` convention.
  (require '[clojure.core.async.flow :as flow])
  (require '[clojure.core.async :as a
             :refer [chan >!! <!! close! timeout alts!! offer! poll! put! take!
                     go go-loop >! <! thread onto-chan! dropping-buffer sliding-buffer]])
  (let [path (System/getenv "CORPUS_FILE")]
    (when (str/blank? path)
      (binding [*out* *err*] (println "jvm-flow-runner: CORPUS_FILE env var not set"))
      (System/exit 1))
    (let [lines (str/split-lines (slurp path))]
      (when-let [prelude (prelude-form lines)]
        (eval (read-string prelude)))
      (doseq [src (remove skip-line? lines)]
        (println (eval-form src)))))
  (flush)
  (shutdown-agents))

(-main)
