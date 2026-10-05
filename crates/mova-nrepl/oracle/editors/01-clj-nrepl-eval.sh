#!/usr/bin/env bash
# Client 1: clj-nrepl-eval (persistent session per host:port, kept in a file under HOME/tmp).
# Steps not offered by this client (complete, doc, close) are SKIP.
. "$(dirname "$0")/lib.sh"
C=clj-nrepl-eval
command -v clj-nrepl-eval >/dev/null || { for s in connect eval print error multi load complete doc interrupt stdin close; do result $C $s SKIP "not installed"; done; exit 0; }
start_server "${SERVER:-mova}" || { result $C connect FAIL "no server"; exit 1; }
cd "$SRV_DIR"
E() { clj-nrepl-eval -p "$PORT" "$@" 2>&1; }

o=$(E "(+ 1 2)");                       check $C connect "$o" "=> 3"
o=$(E "(+ 1 2)");                       check $C eval "$o" "=> 3"
o=$(E '(println "hello-out")');         check $C print "$o" "hello-out"
o=$(E '(/ 1 0)');                       check $C error "$o" "Divide by zero"
o=$(E '(def a 1) (def b 2) (+ a b)');   check $C multi "$o" "=> 3"
printf '(defn sq [x]\n  (* x x))\n(println "loaded")\n' > load.clj
E -f load.clj >/dev/null; o=$(E '(sq 9)'); check $C load "$o" "=> 81"
result $C complete SKIP "client has no completion command"
result $C doc SKIP "client has no doc command"
t0=$(date +%s)
o=$(E --timeout 1500 '(Thread/sleep 60000)'); t1=$(date +%s)
o2=$(E '(+ 20 22)')
if [ $((t1-t0)) -lt 10 ] && printf '%s' "$o2" | grep -qF "=> 42"; then result $C interrupt PASS "timeout sent :interrupt in $((t1-t0))s, session reusable ($(printf '%s' "$o" | tr '\n' ' '))"
else result $C interrupt FAIL "took $((t1-t0))s; next eval: $o2"; fi
o=$(echo '(read-line)' | E --timeout 1500)
result $C stdin SKIP "client sends no :stdin; read-line blocks until timeout then interrupts ($(printf '%s' "$o" | tr '\n' ' ' | cut -c1-80))"
E --reset-session '(+ 1 1)' >/dev/null; result $C close SKIP "no close command; --reset-session used, sessions are per-port files"
