#!/bin/bash
# tools/conformance-report.sh
#
# THE one-command entry point for the project's headline compatibility
# guarantee. Runs, in order:
#   1. tools/verify-vendor.sh          -- anti-cheating manifest check
#   2. tools/check-shim-selftest.sh     -- clojure.test shim selftest (guarded --
#                                          if the tool doesn't exist yet, this step is
#                                          skipped and its guarantee-check row reports
#                                          NOT MEASURED). The Clojure-suite score in
#                                          step 4 is only as trustworthy as this shim --
#                                          it already shipped one silent false-pass, so
#                                          this gate runs the shim's own selftest before
#                                          the score is trusted for this pass.
#   3. cargo test --test conformance_test --release
#                                       -- expression conformance (corpus vs real Clojure)
#   4. tools/clojure-suite-run.sh       -- Clojure-suite score (vendored test_clojure/*.clj)
#   5. tools/check-regression.sh        -- per-file regression vs the committed baseline
#                                          (tests/clojure-suite/BASELINE.edn); guarded --
#                                          if the tool doesn't exist yet, this step is
#                                          skipped and its guarantee-check row reports
#                                          NOT MEASURED instead of silently passing or
#                                          aborting the whole script.
#   6. tools/core-var-inventory.sh      -- clojure.core public-var presence census, writes
#                                          compat/core-var-inventory.edn; also guarded --
#                                          if absent, the report's "clojure.core surface
#                                          coverage" section is simply omitted.
# then regenerates COMPATIBILITY.md at the repo root from the combined
# results (tools/generate-compat-report.bb). COMPATIBILITY.md is fully
# generated -- never hand-edit it, rerun this script instead.
#
# Deliberately does NOT `set -e`: a failing guarantee check (e.g. a
# tampered vendor file) must still produce a full report showing FAIL,
# not silently abort with no report at all. The script's own exit code
# reflects whether every guarantee check passed.
#
# THE INVARIANT: every guarantee check gates the build. A FAIL or a NOT
# MEASURED anywhere in COMPATIBILITY.md's "## Guarantee checks" table --
# including the checks computed entirely INSIDE tools/generate-compat-
# report.bb (oracle-version-pinned, every-exclusion-has-a-reason,
# deviations-still-mismatch, MANIFEST/exclusion-ledger internal
# consistency, ground-truth census freshness) and not just the four
# shell-level gates this script measures itself (vendor-verify,
# corpus-test, regression, shim-selftest) -- means this script exits
# non-zero. tools/generate-compat-report.bb's own exit status is captured
# below and folded into this script's final exit condition for exactly
# that reason: a guarantee check that reports a problem but does not stop
# the build is not a guarantee, it's documentation.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

echo "== 1/6: vendor manifest verification =="
if bash tools/verify-vendor.sh; then
  VENDOR_VERIFY_PASS=true
else
  VENDOR_VERIFY_PASS=false
  echo "!! vendor verification FAILED -- see above" >&2
fi
echo

echo "== 2/6: clojure.test shim selftest =="
if [ -x tools/check-shim-selftest.sh ]; then
  if bash tools/check-shim-selftest.sh; then
    SHIM_SELFTEST_PASS=true
  else
    SHIM_SELFTEST_PASS=false
    echo "!! shim selftest FAILED -- see above -- the Clojure-suite score below is not trustworthy this pass" >&2
  fi
else
  SHIM_SELFTEST_PASS=""
  echo "-- tools/check-shim-selftest.sh not present yet -- shim-selftest gate will report NOT MEASURED --"
fi
echo

echo "== 3/6: expression conformance (corpus vs real Clojure) =="
if cargo test --test conformance_test --release 2>&1 | tee /tmp/conformance-report-cargo.log | tail -20; then
  if grep -q "test result: ok" /tmp/conformance-report-cargo.log; then
    CORPUS_TEST_PASS=true
  else
    CORPUS_TEST_PASS=false
  fi
else
  CORPUS_TEST_PASS=false
fi
echo "corpus test: $([ "$CORPUS_TEST_PASS" = true ] && echo PASS || echo FAIL)"
echo

echo "== 4/6: Clojure-suite score =="
bash tools/clojure-suite-run.sh
echo

echo "== 5/6: per-file regression vs committed baseline =="
if [ -x tools/check-regression.sh ]; then
  if bash tools/check-regression.sh; then
    REGRESSION_PASS=true
  else
    REGRESSION_PASS=false
    echo "!! regression check FAILED -- see above" >&2
  fi
else
  REGRESSION_PASS=""
  echo "-- tools/check-regression.sh not present yet -- regression gate will report NOT MEASURED --"
fi
echo

echo "== 6/6: clojure.core public-var surface census =="
if [ -x tools/core-var-inventory.sh ]; then
  bash tools/core-var-inventory.sh
else
  echo "-- tools/core-var-inventory.sh not present yet -- surface-coverage section will be omitted --"
fi
echo

echo "== regenerating COMPATIBILITY.md =="
export COMPAT_VENDOR_VERIFY_PASS="$VENDOR_VERIFY_PASS"
export COMPAT_CORPUS_TEST_PASS="$CORPUS_TEST_PASS"
export COMPAT_REGRESSION_PASS="$REGRESSION_PASS"
export COMPAT_SHIM_SELFTEST_PASS="$SHIM_SELFTEST_PASS"
if bb tools/generate-compat-report.bb; then
  GENERATOR_EXIT=0
else
  GENERATOR_EXIT=$?
  echo "!! tools/generate-compat-report.bb exited $GENERATOR_EXIT -- a guarantee gate in COMPATIBILITY.md's" >&2
  echo "!! Guarantee checks table is FAIL or NOT MEASURED -- see its stderr summary above" >&2
fi

echo
echo "done. See COMPATIBILITY.md at the repo root."

if [ "$VENDOR_VERIFY_PASS" = true ] && [ "$CORPUS_TEST_PASS" = true ] && [ "$REGRESSION_PASS" != false ] && [ "$SHIM_SELFTEST_PASS" != false ] && [ "$GENERATOR_EXIT" -eq 0 ]; then
  exit 0
else
  exit 1
fi
