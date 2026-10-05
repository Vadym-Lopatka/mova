#!/bin/bash
# tools/clojure-suite-run.sh
#
# Scores mova against the vendored Clojure test suite
# (tests/clojure-suite/vendor/*.clj) by concatenating each file with the
# mova clojure.test shim and running it through mova under a hard
# timeout. Writes tests/clojure-suite/scoreboard.edn.
#
# The actual per-file orchestration and EDN output live in
# tools/clojure-suite-run.bb (babashka) -- EDN in, EDN out, no fragile
# bash grep/sed text parsing.
#
# Env overrides:
#   MOVA_BIN               path to the mova binary (default: target/release/mova)
#   CLOJURE_SUITE_TIMEOUT   per-file timeout in seconds (default: 8)
#   CLOJURE_SUITE_SCRATCH   scratch dir for concatenated temp files
#   CLOJURE_SUITE_OUT       where to write the scoreboard
#                           (default: tests/clojure-suite/scoreboard.edn)
#   CLOJURE_SUITE_ONLY      comma-separated vendored BASENAMES to run, e.g.
#                           "spec.clj,instr.clj,multi_spec.clj". Every vendored
#                           file is still MATERIALIZED (a run file may require
#                           a sibling), only the RUN set shrinks. The result is
#                           a PARTIAL scoreboard: it is tagged :partial-run
#                           true, and the run refuses to start unless
#                           CLOJURE_SUITE_OUT points somewhere other than the
#                           committed scoreboard, because check-regression.sh
#                           would otherwise read every unrun file as vanished.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if ! command -v bb >/dev/null 2>&1; then
  echo "FAIL: babashka (bb) is required but not on PATH" >&2
  exit 1
fi

if ! command -v timeout >/dev/null 2>&1; then
  echo "FAIL: GNU coreutils 'timeout' is required but not on PATH" >&2
  exit 1
fi

MOVA_BIN="${MOVA_BIN:-$ROOT/target/release/mova}"
if [ ! -x "$MOVA_BIN" ]; then
  echo "FAIL: mova binary not found or not executable at $MOVA_BIN (build with: cargo build --release)" >&2
  exit 1
fi

export MOVA_BIN
exec bb "$ROOT/tools/clojure-suite-run.bb"
