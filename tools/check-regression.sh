#!/bin/bash
# tools/check-regression.sh
#
# Mechanizes CONFORMANCE-GUARANTEE.md's audit-ritual step 2: "any
# assertion that flipped pass -> fail is a regression and blocks, even
# while the total is far from 100%." Compares tests/clojure-suite/scoreboard.edn
# against the committed tests/clojure-suite/BASELINE.edn, file by file
# (never on the global attempted-ratio, which is non-monotone by
# construction -- see tools/check-regression.bb's module doc for why).
#
# Exit 0 = no regression. Exit 1 = at least one regression (or, if no
# baseline has ever been blessed, a clear error telling you to bless one).
#
# Re-blessing the baseline (copying the current scoreboard into
# BASELINE.edn) is an explicit, deliberate act, never automatic:
#
#   COMPAT_UPDATE_BASELINE=1 bash tools/check-regression.sh
#
# The actual comparison / blessing logic lives in
# tools/check-regression.bb (babashka) -- EDN in, EDN out, no fragile
# bash grep/sed text parsing, per this repo's own convention (see
# tools/clojure-suite-run.sh for the same thin-wrapper pattern).
#
# Env overrides (mainly for testing this gate itself against a hand-edited
# COPY of the scoreboard without touching the real file):
#   COMPAT_SCOREBOARD_PATH   path to the scoreboard EDN (default: tests/clojure-suite/scoreboard.edn)
#   COMPAT_BASELINE_PATH     path to the baseline EDN (default: tests/clojure-suite/BASELINE.edn)
#   COMPAT_UPDATE_BASELINE   set to "1" to bless the current scoreboard as the new baseline
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if ! command -v bb >/dev/null 2>&1; then
  echo "FAIL: babashka (bb) is required but not on PATH" >&2
  exit 1
fi

exec bb "$ROOT/tools/check-regression.bb"
