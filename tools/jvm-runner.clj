;; jvm-runner.clj -- evaluates a corpus file's forms, in order, in ONE
;; session, against REAL Clojure on the JVM (pinned version: see
;; tests/conformance/CLOJURE_VERSION -- the ONE place that version is
;; written down; nothing here hardcodes it). This is the GENERIC sibling of
;; tools/jvm-flow-runner.clj: that file unconditionally `require`s
;; `clojure.core.async.flow` before running any form, which only
;; flow.corpus (via its `;;ENGINE jvm-flow` directive) needs and no other
;; corpus file can even load without core.async on the classpath. This
;; file requires NOTHING beyond bare `clojure.core` up front -- any corpus
;; file that needs an extra namespace in scope (e.g. async.corpus needing
;; `clojure.core.async`) brings it in itself via its own `;;PRELUDE`
;; line, exactly like every other engine. That's what makes this runner
;; generic: it is tools/gen-golden.bb's DEFAULT engine (see that file's
;; module doc for why babashka stopped being the default), used for every
;; corpus file except the one that opts into `;;ENGINE jvm-flow` instead.
;;
;; Invoked by tools/gen-golden.bb (see that file's `jvm-golden!`) as:
;;
;;   CORPUS_FILE=<abs-path-to-corpus> \
;;     clojure -Sdeps '{:deps {org.clojure/clojure
;;                              {:mvn/version "<CLOJURE_VERSION contents>"}
;;                              org.clojure/core.async
;;                              {:mvn/version "1.9.808-alpha1"}}}' \
;;     -M -e '(load-file "tools/jvm-runner.clj")'
;;
;; (the corpus path travels via an env var, not a CLI arg -- `clojure -M -e
;; <form> <extra-args>` treats trailing positional args as script files to
;; load, not as *command-line-args* for the -e form, so a CLI arg doesn't
;; work here; an env var sidesteps that entirely -- same reasoning as
;; tools/jvm-flow-runner.clj's own module doc).
;;
;; core.async rides along on every invocation (not just async.corpus's)
;; for the same reason gen-golden.bb's bb-engine path doesn't special-case
;; which corpus file needs which bb namespace: it's cheap to have on the
;; classpath and unused by any corpus file that doesn't `require` it via
;; `;;PRELUDE`, so there is no need for gen-golden.bb to know which corpus
;; files want it.
;;
;; This file's own top-level `(ns jvm-runner ...)` becomes the ONE session
;; every corpus form evaluates in -- forms may `def` and have later forms
;; in the SAME corpus file see those defs, matching every other engine's
;; session model (one corpus FILE == one session; see
;; tools/gen-golden.bb's own module doc and tests/conformance_test.rs's
;; matching one-`Interp`-per-file rule). Because gen-golden.bb shells out
;; to a FRESH `clojure` process per corpus file, there is no need for the
;; multi-file namespace-per-file trick bb's own script uses internally
;; (bb evaluates every corpus file in a single long-lived process) -- one
;; process here already IS one file's session.
;;
;; Output contract (must match every other engine's golden format exactly,
;; see tools/gen-golden.bb's module doc): for each corpus form, in order,
;; print ONE line to stdout:
;;   OK<TAB><pr-str of the form's result>
;;   ERR<TAB><ExceptionSimpleName>         (the form threw)
;; The exception line carries `(.getSimpleName (class e))` of the caught
;; Throwable, RAW/outer -- no unwrapping of
;; `clojure.lang.Compiler$CompilerException` to its `.getCause` -- exactly
;; like tools/jvm-pending-runner.clj's own `eval-form` (see that file's
;; module doc point 1 for why: consistency with the pending ledger's
;; established convention matters more than any one runner's opinion about
;; which exception is "more informative"). NEVER the exception message --
;; CONFORMANCE-GUARANTEE.md's canonicalization rule 6 compares exceptions
;; by occurrence and coarse kind only, never by message text, and mova's
;; own messages are deliberately different. ONLY those lines go to
;; stdout -- everything else (JVM startup noise, per-form error detail for
;; debugging a failing corpus form) is sent to *err*, since gen-golden.bb
;; captures stdout verbatim as the .golden file contents.

(ns jvm-runner
  (:require [clojure.string :as str]))

(defn skip-line?
  "Must match tools/gen-golden.bb's `skip-line?` / conformance_test.rs's
  `is_skippable` exactly: blank, or a `;;`-prefixed comment (after leading
  whitespace) -- this also transparently skips a `;;PRELUDE`/`;;ENGINE`
  directive line itself, since both start with `;;` like any other
  comment."
  [line]
  (let [t (str/trim line)]
    (or (str/blank? t) (str/starts-with? t ";;"))))

(defn prelude-form
  "The code text after `;;PRELUDE ` on the first line of `lines` that has
  that prefix, or nil if none does -- same opt-in mechanism as
  tools/gen-golden.bb's own `prelude-form` and
  tools/jvm-flow-runner.clj's. async.corpus is the one corpus file that
  uses this today, to `require` `clojure.core.async`'s bare names into
  scope (mova's own core.async surface is global, so it needs no such
  prelude)."
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
        (println "jvm-runner: form errored:" (pr-str src))
        (println "  ->" (.getSimpleName (class e)) "--" (or (.getMessage e) (str e))))
      (str "ERR\t" (.getSimpleName (class e))))))

(defn -main []
  (let [path (System/getenv "CORPUS_FILE")]
    (when (str/blank? path)
      (binding [*out* *err*] (println "jvm-runner: CORPUS_FILE env var not set"))
      (System/exit 1))
    ;; S3: same fix (and same rationale, verbatim) as
    ;; tools/jvm-pending-runner.clj's -main -- this file's own `(ns
    ;; jvm-runner ...)` moved *ns* away from `user`, but corpus authors
    ;; write forms assuming a fresh session's `user` namespace: 
    ;; `defrecord`/`deftype` class names embed the defining ns
    ;; (user.Rec123, never jvm_runner.Rec123), and `::foo`/`` `sym ``
    ;; resolve against *ns* at read time. mova's own corpus sessions run
    ;; in `user`, so this is the only namespace under which the two
    ;; engines' outputs are even comparable. The helper fns above stay
    ;; compiled against their original jvm-runner Vars, so switching
    ;; mid-`-main` is safe.
    (in-ns 'user)
    (let [lines (str/split-lines (slurp path))]
      (when-let [prelude (prelude-form lines)]
        (eval (read-string prelude)))
      (doseq [src (remove skip-line? lines)]
        (println (eval-form src)))))
  (flush)
  (shutdown-agents))

(-main)
