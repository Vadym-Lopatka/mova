#!/bin/bash
# tools/check-perf-regression.sh
#
# Mechanizes the conductor's manual perf A/B gate (docs/W-LENS-DESIGN.md
# Stage 3, W-LENS-3): measures the five house benchmarks -- lane,
# flow-2hop, flow-4hop, delays wall, parse wall -- with the house
# best-of-N statistics (median/min/max over N rounds), writes them to
# bench/perf-scoreboard.edn, and diffs that against the committed
# bench/PERF-BASELINE.edn, file-by-file... well, metric-by-metric, the
# same way tools/check-regression.sh diffs conformance. Runs quiet at
# merge.
#
# Exit 0 = no regression (or a bless run, which always exits 0). Exit 1
# = at least one metric REGRESSED, or a ledgered hard bar failed (lane
# median >= 250M iters/s; delays median < 8.0s) -- or no baseline has
# ever been blessed, which is reported as a clear error.
#
# Re-blessing the baseline (writing the just-measured scoreboard into
# PERF-BASELINE.edn) is an explicit, deliberate act, never automatic:
#
#   PERF_UPDATE_BASELINE=1 bash tools/check-perf-regression.sh
#
# The actual measurement / comparison / blessing logic lives in
# tools/check-perf-regression.bb (babashka) -- EDN in, EDN out, no
# fragile bash grep/sed text parsing, per this repo's own convention
# (see tools/check-regression.sh for the same thin-wrapper pattern this
# mirrors).
#
# Env overrides (see tools/check-perf-regression.bb's own module doc for
# the full list and defaults):
#   PERF_SCOREBOARD_PATH   live measurement output (default: bench/perf-scoreboard.edn)
#   PERF_BASELINE_PATH     committed reference point (default: bench/PERF-BASELINE.edn)
#   PERF_UPDATE_BASELINE   set to "1" to bless the just-measured scoreboard as the new baseline
#   PERF_PROVISIONAL       (bless only) set to "1" to stamp :provisional-loaded-machine true
#   MOVA_BIN              binary under test (default: target/release/mova)
#   PERF_METRICS           comma-separated subset, e.g. "lane,flow-2hop" (default: all five)
#   PERF_ROUNDS_<METRIC>   override that metric's round count (LANE/FLOW_2HOP/FLOW_4HOP/DELAYS/PARSE)
#   PERF_TIMEOUT_DELAYS / PERF_TIMEOUT_PARSE   per-round subprocess timeout in seconds
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if ! command -v bb >/dev/null 2>&1; then
  echo "FAIL: babashka (bb) is required but not on PATH" >&2
  exit 1
fi

exec bb "$ROOT/tools/check-perf-regression.bb"
