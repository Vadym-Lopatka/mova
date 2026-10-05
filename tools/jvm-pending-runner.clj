;; jvm-pending-runner.clj -- the pending-corpus sibling of
;; tools/jvm-runner.clj (owned by another workstream; this file is NOT a
;; modification of it, it's a separate runner for a separate golden
;; format). Evaluates a pending area file's forms, in order, in ONE
;; session, against REAL Clojure on the JVM (pinned version: see
;; tests/conformance/CLOJURE_VERSION -- read from there, never hardcoded
;; here either).
;;
;; Invoked by tools/gen-pending.sh as:
;;
;;   CORPUS_FILE=<abs-path-to-pending-corpus> \
;;     clojure -Sdeps '{:deps {org.clojure/clojure
;;                              {:mvn/version "<CLOJURE_VERSION contents>"}}}' \
;;     -M -e '(load-file "tools/jvm-pending-runner.clj")'
;;
;; (env var, not a CLI arg, for the same reason tools/jvm-runner.clj uses
;; one: `clojure -M -e <form> <extra-args>` treats trailing positional args
;; as script files to load, not as args to the `-e` form.)
;;
;; Session model: one process per pending file == one session, matching
;; every other engine here (forms may `def`/`defn` and have later forms in
;; the SAME file see those defs).
;;
;; THE TWO REAL DIFFERENCES from tools/jvm-runner.clj's golden format and
;; behavior:
;;
;; 1. An erroring form's line is `ERR<TAB><ExceptionSimpleName>`, not bare
;;    `ERR`. The pending ledger's ERR-BOTH bucket ("both sides threw") is
;;    explicitly documented as provisional in
;;    tests/pending_conformance_test.rs because mova has no
;;    exception-class taxonomy yet -- carrying Clojure's own exception
;;    simple-name forward (even though mova's `.mova` column can't yet
;;    be compared against it) is what lets that taxonomy be built later
;;    without regenerating every golden again. The RAW/outer exception's
;;    simple name is recorded, with NO unwrapping of
;;    `clojure.lang.Compiler$CompilerException` to its `.getCause` --
;;    earlier draft of this file did unwrap, on the theory that the
;;    compiler-wrapper class is uninformative plumbing, but that
;;    contradicted the convention several pending area files already
;;    established independently before this runner existed (e.g.
;;    `vars-binding.corpus`'s `two` -- a macro symbol used as a value --
;;    records `CompilerException`, not its `RuntimeException` cause).
;;    Consistency across every area's golden matters more than any one
;;    runner's opinion about which exception is "more informative", so
;;    this now matches that convention exactly: report `(class e)`,
;;    unwrapped.
;;
;; 2. `;;PRELUDE <clojure-code>` support, identical in spirit to
;;    tools/gen-golden.bb's own opt-in mechanism (see that file's module
;;    doc): the code text after `;;PRELUDE ` on the FIRST such line in the
;;    corpus is evaluated once, before any real form, and produces no
;;    golden line of its own. `numerics.corpus` needs this to `require`
;;    `clojure.math` (not auto-loaded just by referencing
;;    `clojure.math/sqrt`) before its clojure.math interop forms run.
;;
;; Output contract: for each pending form, in order, print ONE line to
;; stdout:
;;   OK<TAB><pr-str of the form's result>
;;   ERR<TAB><ExceptionSimpleName>
;; ONLY those lines go to stdout -- everything else (JVM startup noise,
;; per-form error detail for debugging) goes to *err*, since gen-pending.sh
;; captures stdout verbatim as the `.golden` file contents.

(ns jvm-pending-runner
  (:require [clojure.string :as str]))

(defn skip-line?
  "Must match every other engine's skip rule exactly: blank, or a
  `;;`-prefixed comment (after leading whitespace) -- this also
  transparently skips a `;;PRELUDE` directive line itself, since it starts
  with `;;` like any other comment."
  [line]
  (let [t (str/trim line)]
    (or (str/blank? t) (str/starts-with? t ";;"))))

(defn prelude-form
  "The code text after `;;PRELUDE ` on the first line of `lines` that has
  that prefix, or nil if none does -- same opt-in mechanism as
  tools/gen-golden.bb's own `prelude-form` and tools/jvm-runner.clj's."
  [lines]
  (some (fn [line]
          (let [t (str/trim line)]
            (when (str/starts-with? t ";;PRELUDE ")
              (subs t (count ";;PRELUDE ")))))
        lines))

(defn eval-form
  "Evaluates one form's source text in the current (session) namespace,
  returning the golden line for it."
  [src]
  (try
    (let [v (eval (read-string src))]
      (str "OK\t" (pr-str v)))
    (catch Throwable e
      (binding [*out* *err*]
        (println "jvm-pending-runner: form errored:" (pr-str src))
        (println "  ->" (.getSimpleName (class e)) "--" (or (.getMessage e) (str e))))
      (str "ERR\t" (.getSimpleName (class e))))))

(defn -main []
  (let [path (System/getenv "CORPUS_FILE")]
    (when (str/blank? path)
      (binding [*out* *err*] (println "jvm-pending-runner: CORPUS_FILE env var not set"))
      (System/exit 1))
    ;; This file's own `(ns jvm-pending-runner ...)` above already moved
    ;; *ns* away from `user` (that's what the `ns` macro does). Pending
    ;; corpus authors write forms assuming the SAME default namespace a
    ;; fresh `clojure -M -e` session starts in -- `user` -- since that's
    ;; the only namespace name a form can hardcode and have it actually
    ;; mean something (auto-resolved keywords `::foo`, syntax-quote
    ;; `` `sym ``, and anything printing a namespace-qualified symbol are
    ;; all *ns*-sensitive at read/expand time). `in-ns` here switches the
    ;; CURRENT namespace back to `user` before any corpus form is read;
    ;; this file's own helper functions above stay compiled against their
    ;; original `jvm-pending-runner`-qualified Vars (Clojure resolves a
    ;; compiled fn's free vars once, not by re-resolving through *ns* on
    ;; every call), so this is safe to do mid-`-main`.
    (in-ns 'user)
    (let [lines (str/split-lines (slurp path))]
      (when-let [prelude (prelude-form lines)]
        (eval (read-string prelude)))
      (doseq [src (remove skip-line? lines)]
        (println (eval-form src)))))
  (flush)
  (shutdown-agents))

(-main)
