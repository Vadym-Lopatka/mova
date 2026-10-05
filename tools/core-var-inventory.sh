#!/bin/bash
# tools/core-var-inventory.sh
#
# Mechanical census of how many real Clojure 1.13.0-alpha6 clojure.core
# public vars are bare-resolvable under mova. Writes
# compat/core-var-inventory.edn.
#
# This is a PRESENCE count, not a conformance number -- see the :note key
# in the generated EDN and the module doc in tools/core-var-inventory.bb
# for why that distinction matters and is enforced.
#
# The actual oracle-query / probe-script / classification logic lives in
# tools/core-var-inventory.bb (babashka) -- EDN in, EDN out, no fragile
# bash grep/sed text parsing, per this repo's own convention (see
# tools/clojure-suite-run.sh for the same thin-wrapper pattern).
#
# Env overrides:
#   MOVA_BIN                  path to the mova binary (default: target/release/mova)
#   CORE_VAR_INVENTORY_SCRATCH scratch dir for the generated probe script (default: system tmpdir)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if ! command -v bb >/dev/null 2>&1; then
  echo "FAIL: babashka (bb) is required but not on PATH" >&2
  exit 1
fi

if ! command -v clojure >/dev/null 2>&1; then
  echo "FAIL: the 'clojure' CLI is required (to query the real Clojure oracle for clojure.core publics) but not on PATH" >&2
  exit 1
fi

MOVA_BIN="${MOVA_BIN:-$ROOT/target/release/mova}"
if [ ! -x "$MOVA_BIN" ]; then
  echo "FAIL: mova binary not found or not executable at $MOVA_BIN (build with: cargo build --release)" >&2
  exit 1
fi

export MOVA_BIN
exec bb "$ROOT/tools/core-var-inventory.bb"
