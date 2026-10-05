#!/usr/bin/env bash
# One-command Mova vs JVM Clojure op-cost scoreboard. Usage: tools/scoreboard.sh [--mova-only]
set -euo pipefail
cd "$(dirname "$0")/.."
MOVA_BIN="${MOVA_BIN:-target/release/mova}"
OPS=bench/scoreboard/ops.clj
OUT_MD=bench/scoreboard/latest.md
OUT_EDN=bench/scoreboard/latest.edn
MOVA_ONLY=0
[[ "${1:-}" == "--mova-only" ]] && MOVA_ONLY=1

# run ops.clj with a binary/cmd, extract "ROW name ns" lines into name->ns awk assoc via temp file
run_ops() {
  "$@" "$OPS" 2>/dev/null | awk '/^ROW /{print $2, $3}'
}

echo "Running Mova ops..." >&2
MOVA_RAW=$(run_ops "$MOVA_BIN")

if [[ $MOVA_ONLY -eq 1 ]]; then
  echo "Loading JVM baseline from $OUT_EDN's source (bench/scoreboard/jvm-baseline.edn)..." >&2
  JVM_EDN=bench/scoreboard/jvm-baseline.edn
  [[ -f "$JVM_EDN" ]] || { echo "missing $JVM_EDN; run a full scoreboard first" >&2; exit 1; }
  JVM_RAW=$(sed -n 's/.*:\([a-zA-Z0-9?_-]*\) {:jvm \([0-9.]*\)}.*/\1 \2/p' "$JVM_EDN")
else
  echo "Running JVM ops..." >&2
  JVM_RAW=$(run_ops /opt/homebrew/bin/clojure -M)
fi

# startup rows for Mova only
echo "Timing mova --version (median of 10)..." >&2
VER_TIMES=()
for i in $(seq 1 10); do
  t0=$(date +%s%N)
  "$MOVA_BIN" --version >/dev/null 2>&1 || true
  t1=$(date +%s%N)
  VER_TIMES+=($(( (t1 - t0) / 1000000 )))
done
VER_MS=$(printf '%s\n' "${VER_TIMES[@]}" | sort -n | sed -n '6p')

echo "Timing mova -e '(+ 1 2)' (wall ms + max RSS)..." >&2
TIME_OUT=$(/usr/bin/time -l "$MOVA_BIN" -e "(+ 1 2)" 2>&1 1>/dev/null)
EVAL_REAL=$(echo "$TIME_OUT" | awk '/real/{print $1}')
EVAL_MS=$(awk -v r="$EVAL_REAL" 'BEGIN{printf "%.0f", r*1000}')
EVAL_RSS_BYTES=$(echo "$TIME_OUT" | awk '/maximum resident set size/{print $1}')
EVAL_RSS_MB=$(awk -v b="$EVAL_RSS_BYTES" 'BEGIN{printf "%.1f", b/1024/1024}')

# build lookups via temp files (bash 3.2 on macOS lacks associative arrays)
MOVA_TF=$(mktemp); JVM_TF=$(mktemp)
printf '%s\n' "$MOVA_RAW" > "$MOVA_TF"
printf '%s\n' "$JVM_RAW" > "$JVM_TF"
trap 'rm -f "$MOVA_TF" "$JVM_TF"' EXIT

lookup() { awk -v k="$1" '$1==k{print $2; found=1} END{if(!found) print "NA"}' "$2"; }

ROWS="empty-loop call-0arg call-1arg call-3arg call-local-fn call-closure-capture call-multi-arity call-variadic var-deref kw-get-4map get-4map assoc-4map conj-vector let-binding destructure-keys protocol-call multimethod instance? lazy-map-per-elem reduce-per-step"

{
  echo "| op | Mova ns | JVM ns | ratio |"
  echo "|---|---|---|---|"
  for r in $ROWS; do
    m=$(lookup "$r" "$MOVA_TF")
    j=$(lookup "$r" "$JVM_TF")
    if [[ "$m" != "NA" && "$j" != "NA" && "$j" != "0.0" ]]; then
      ratio=$(awk -v m="$m" -v j="$j" 'BEGIN{printf "%.1f", m/j}')
    else
      ratio="NA"
    fi
    echo "| $r | $m | $j | ${ratio}x |"
  done
  echo ""
  echo "| startup | value |"
  echo "|---|---|"
  echo "| mova --version (median ms, n=10) | $VER_MS |"
  echo "| mova -e \"(+ 1 2)\" wall ms | $EVAL_MS |"
  echo "| mova -e \"(+ 1 2)\" max RSS MB | $EVAL_RSS_MB |"
} | tee "$OUT_MD"

{
  echo "{"
  first=1
  for r in $ROWS; do
    m=$(lookup "$r" "$MOVA_TF"); [[ "$m" == "NA" ]] && m=null
    j=$(lookup "$r" "$JVM_TF"); [[ "$j" == "NA" ]] && j=null
    [[ $first -eq 1 ]] || echo ","
    first=0
    printf ' :%s {:mova %s :jvm %s}' "$r" "$m" "$j"
  done
  echo ""
  echo "}"
} > "$OUT_EDN"

if [[ $MOVA_ONLY -eq 0 ]]; then
  JVM_EDN=bench/scoreboard/jvm-baseline.edn
  {
    echo "{"
    first=1
    for r in $ROWS; do
      j=$(lookup "$r" "$JVM_TF"); [[ "$j" == "NA" ]] && j=null
      [[ $first -eq 1 ]] || echo ","
      first=0
      printf ' :%s {:jvm %s}' "$r" "$j"
    done
    echo ""
    echo "}"
  } > "$JVM_EDN"
  echo "Wrote $JVM_EDN" >&2
fi

echo "Wrote $OUT_MD and $OUT_EDN" >&2
