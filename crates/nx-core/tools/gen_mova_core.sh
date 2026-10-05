#!/bin/sh
# Regenerates src/mova/core_vars.txt: the public vars of `clojure.core` in a fresh `mova` process (no require).
# Usage: tools/gen_mova_core.sh [path/to/mova]    (default: `mova` on PATH)
set -e
MOVA=${1:-mova}
OUT="$(dirname "$0")/../src/mova/core_vars.txt"
"$MOVA" -e "(doseq [s (sort (map str (keys (ns-publics 'clojure.core))))] (println s))" | LC_ALL=C sort -u > "$OUT.tmp"
test -s "$OUT.tmp"
mv "$OUT.tmp" "$OUT"
wc -l < "$OUT"
