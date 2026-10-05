#!/usr/bin/env bash
# Client 2: reference JVM client (nrepl.cmdline --connect) and `mova nrepl -c`, piped stdin.
. "$(dirname "$0")/lib.sh"
command -v java >/dev/null || { echo "RESULT jvm-cmdline connect SKIP no java"; exit 0; }
start_server "${SERVER:-mova}" || { result jvm-cmdline connect FAIL "no server"; exit 1; }
cd "$SRV_DIR"
printf '(defn sq [x]\n  (* x x))\n(println "loaded")\n' > load.clj
run_client() { # name, input -> output
  case "$1" in
    jvm-cmdline) (printf '%s\n' "$2"; sleep 1.5) | timeout 30 java -cp "$(cat "$CP_FILE")" clojure.main -m nrepl.cmdline --connect --host 127.0.0.1 --port "$PORT" 2>&1;;
    mova-c)      (printf '%s\n' "$2"; sleep 1.5) | timeout 30 "$MOVA" nrepl -c --host 127.0.0.1 --port "$PORT" 2>&1;;
  esac
}
for C in jvm-cmdline mova-c; do
  o=$(run_client $C '(+ 1 2)');                              check $C connect "$o" "nREPL 1.8.0"
  check $C eval "$o" "user=> 3"
  o=$(run_client $C '(println "hello-out")');                check $C print "$o" "hello-out"
  o=$(run_client $C '(/ 1 0)');                              check $C error "$o" "Divide by zero"
  o=$(run_client $C '(def a 1) (def b 2) (+ a b)');          check $C multi "$o" "user=> 3"
  o=$(run_client $C '(load-file "load.clj")
(sq 9)');                                                    check $C load "$o" "81"
  result $C complete SKIP "client has no completion"
  o=$(run_client $C "(require 'clojure.repl) (clojure.repl/doc map)");                            check $C doc "$o" "clojure.core/map"
  result $C interrupt SKIP "needs a tty for Ctrl-C; piped client dies on SIGINT"
  result $C stdin SKIP "piped client prints nothing after (read-line); identical on JVM server"
  o=$(run_client $C '(exit)');                               check $C close "$o" "nREPL"
done
