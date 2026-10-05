#!/bin/bash
# Parity vs the JVM oracle goldens. Usage: tools/parity.sh <lib|e2test|e2e-clj|e2e-bb|e2e-cljs|lint|real|real2> [diff.py args]
# Jars: native analysis by default (NATIVE_JARS=0 = golden jar dir, test oracle only).
# Needs scratch copies of the corpora (see nx/oracle/gen.sh): LIB_ROOT / E2TEST_ROOT (oracle work dirs),
# and for e2test the jar golden of its own classpath (E2TEST_JARS, made with `nx.oracle jars` on
# `clojure -Spath -A:dev:test` jars); other corpora use the lib jar golden or none (e2e).
set -e
G=${G:-$HOME/.cache/nx-oracle/golden}
WT=${WT:?usage: WT=/path/to/clojure-lsp-nx tools/parity.sh <corpus>}
OUT=${OUT:-${TMPDIR:-/tmp}/nx/an/out}
B=$(cd "$(dirname "$0")/.." && pwd)/target/release/examples/nx_analyze
c=$1; shift
case $c in
  lib) R=${LIB_ROOT:-${TMPDIR:-/tmp}/nx/oracle/work/lib}; J=$G/jars;;
  e2test) R=${E2TEST_ROOT:-${TMPDIR:-/tmp}/nx/oracle/work/e2test}; J=${E2TEST_JARS:?e2test jar golden dir};;
  lint) R=${LINT_ROOT:-${TMPDIR:-/tmp}/nx/oracle/work/lint}; J=${EMPTY_JARS:-${TMPDIR:-/tmp}/nx/an/emptyjars};;
  real2) R=${REAL2_ROOT:-${TMPDIR:-/tmp}/nx/oracle/work/real2}; J=${EMPTY_JARS:-${TMPDIR:-/tmp}/nx/an/emptyjars};;
  real) R=${REAL_ROOT:-${TMPDIR:-/tmp}/nx/oracle/work/real}; J=${EMPTY_JARS:-${TMPDIR:-/tmp}/nx/an/emptyjars};;
  e2e-*) R=$WT/mova/lsp-e2e/projects/${c#e2e-}; J=${EMPTY_JARS:-${TMPDIR:-/tmp}/nx/an/emptyjars};;
esac
(cd "$(dirname "$0")/.." && cargo build --release --example nx_analyze 2>&1 | tail -1)
rm -rf "$OUT/$c"; mkdir -p "$OUT/$c"; [ "${NATIVE_JARS:-1}" = 1 ] || mkdir -p "$J"
# NATIVE_JARS=1 (default): native jar analysis from the classpath jar list ($LIB_JARS/$E2TEST_JARS_LIST); NATIVE_JARS=0: golden jars
if [ "${NATIVE_JARS:-1}" = 1 ]; then
  W=${TMPDIR:-/tmp}/nx/oracle/work
  case $c in lib) JL=${LIB_JARS_LIST:-$W/lib.jars};; e2test) JL=${E2TEST_JARS_LIST:-$W/e2test.jars};; *) JL=;; esac
  if [ -n "$JL" ]; then JARARGS="--jars-file $JL --jar-cache ${JAR_CACHE:-${TMPDIR:-/tmp}/nx/jar/cache-parity}"; else JARARGS=; fi
  "$B" "$R" "$OUT/$c" $JARARGS --only-golden "$G/$c"
else
  "$B" "$R" "$OUT/$c" --jars-golden-dir "$J" --only-golden "$G/$c"
fi
python3 "$WT/nx/oracle/diff.py" "$G/$c" "$OUT/$c" "$@"
