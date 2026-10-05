#!/bin/bash
# tools/check-shim-selftest.sh
#
# Enforcement gate for tests/clojure-suite/SHIM-LIMITS.md's central claim:
# "A regression here cannot land silently: the selftest's process exit
# code is nonzero if any of these checks fail." Until this script existed
# that sentence was FALSE -- `grep -rl shim-selftest tools/ tests/*.rs`
# returned no hits at all. tests/clojure-suite/shim-selftest.mova sat on
# disk, mechanically correct, and nothing ever ran it. Its exit code
# enforced nothing because nobody ever asked for it.
#
# This matters concretely: the shim has already shipped one SILENT
# FALSE-PASS bug. `is`'s special-cased `(= a b c ...)` handling used to
# read only the first two operands (`(second form)`/`(nth form 2)`), so
# `(is (= 1 1 2))` evaluated as `(= 1 1)` and scored :pass -- a false
# pass on every 3+-arity `(is (= ...))` in the vendored corpus (22 such
# forms at the time it was found). Every number in COMPATIBILITY.md's
# Clojure-suite section is downstream of this shim: if it silently
# regresses again, all of those numbers are corrupt and, without this
# gate, nothing notices.
#
# mova has no multi-file `load`/`require` (see mova-test-shim.mova's own
# header), so the selftest cannot simply `require` the shim. This script
# concatenates the shim's source, THEN the test-helper shim's source,
# THEN the selftest's own source, into ONE temp file and runs that single
# file through mova -- the same "one process, one assembled file"
# convention tools/clojure-suite-run.bb established for running vendored
# files against the shims (unlike that script, this one does not need to
# inject after a `(ns ...)` form: shim-selftest.mova has none of its own
# to switch away from, so plain concatenation is enough). The helper shim
# (W4-SHIM, 2026-08-21 -- it used to be left out entirely, so nothing in
# mova-test-helper-shim.mova could be selftested at all) is included
# unconditionally rather than behind its own env knob's absence, because
# that is what every real vendored file actually gets: both shims are
# ALWAYS spliced together, same order, everywhere tools/clojure-suite-
# run.bb assembles a file (that script's own module doc: "the two shims
# define disjoint names") -- concatenating only one here would test a
# combination no vendored file ever actually runs under.
#
# PASS requires BOTH:
#   - process exit code 0
#   - a `#SELFTEST {:status :pass ...}` line on stdout
# Both matter independently: exit 0 with no `#SELFTEST` line at all would
# mean the assembled file ran but silently never reached its own verdict
# line (e.g. it crashed inside a `catch` that swallowed the failure, or a
# future edit removed the final form) -- that must NOT be reported as a
# pass just because the process happened to return 0.
#
# Env overrides:
#   MOVA_BIN               path to the mova binary
#                            (default: target/release/mova)
#   SHIM_SELFTEST_TIMEOUT    wall-clock timeout in seconds (default: 30 --
#                            mova has a known infinite-hang bug on
#                            non-tail `recur`; a hung selftest must not
#                            wedge tools/conformance-report.sh)
#   SHIM_FILE                path to mova-test-shim.mova (default:
#                            tests/clojure-suite/mova-test-shim.mova) --
#                            override to point this gate at an alternate
#                            shim copy without touching the real one, e.g.
#                            to prove the gate can actually fail against a
#                            deliberately-regressed shim.
#   HELPER_SHIM_FILE         path to mova-test-helper-shim.mova (default:
#                            tests/clojure-suite/mova-test-helper-shim.mova)
#                            -- same override use case as SHIM_FILE, for
#                            the helper shim.
#   SELFTEST_FILE             path to shim-selftest.mova (default:
#                            tests/clojure-suite/shim-selftest.mova)
#   SHIM_SELFTEST_SCRATCH    scratch dir for the assembled temp file
#                            (default: a fresh dir under $TMPDIR)
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

MOVA_BIN="${MOVA_BIN:-$ROOT/target/release/mova}"
if [ ! -x "$MOVA_BIN" ]; then
  echo "FAIL: mova binary not found or not executable at $MOVA_BIN (build with: cargo build --release)" >&2
  exit 1
fi

if ! command -v timeout >/dev/null 2>&1; then
  echo "FAIL: 'timeout' is required but not on PATH" >&2
  exit 1
fi

SHIM_FILE="${SHIM_FILE:-$ROOT/tests/clojure-suite/mova-test-shim.mova}"
HELPER_SHIM_FILE="${HELPER_SHIM_FILE:-$ROOT/tests/clojure-suite/mova-test-helper-shim.mova}"
SELFTEST_FILE="${SELFTEST_FILE:-$ROOT/tests/clojure-suite/shim-selftest.mova}"
TIMEOUT_SECS="${SHIM_SELFTEST_TIMEOUT:-30}"
SCRATCH="${SHIM_SELFTEST_SCRATCH:-$(mktemp -d "${TMPDIR:-/tmp}/shim-selftest.XXXXXX")}"

[ -f "$SHIM_FILE" ] || { echo "FAIL: shim file not found at $SHIM_FILE" >&2; exit 1; }
[ -f "$HELPER_SHIM_FILE" ] || { echo "FAIL: helper shim file not found at $HELPER_SHIM_FILE" >&2; exit 1; }
[ -f "$SELFTEST_FILE" ] || { echo "FAIL: selftest file not found at $SELFTEST_FILE" >&2; exit 1; }

mkdir -p "$SCRATCH"
ASSEMBLED="$SCRATCH/shim-selftest-assembled.mova"
{
  echo ";; ==== assembled by tools/check-shim-selftest.sh: shim + helper-shim + selftest in one file -- mova has no multi-file load ===="
  cat "$SHIM_FILE"
  echo
  echo ";; ==== mova-test-helper-shim.mova ===="
  cat "$HELPER_SHIM_FILE"
  echo
  echo ";; ==== shim-selftest.mova ===="
  cat "$SELFTEST_FILE"
} > "$ASSEMBLED"

echo "check-shim-selftest: shim=$SHIM_FILE helper_shim=$HELPER_SHIM_FILE selftest=$SELFTEST_FILE mova=$MOVA_BIN timeout=${TIMEOUT_SECS}s" >&2

OUT_FILE="$SCRATCH/out.log"
timeout "${TIMEOUT_SECS}s" "$MOVA_BIN" "$ASSEMBLED" > "$OUT_FILE" 2>&1
EXIT_CODE=$?

SELFTEST_LINE="$(grep -m1 '^#SELFTEST ' "$OUT_FILE" || true)"

if [ "$EXIT_CODE" -eq 124 ]; then
  echo "FAIL: shim selftest TIMED OUT after ${TIMEOUT_SECS}s (mova has a known infinite-hang bug on non-tail recur)" >&2
  echo "-- output so far --" >&2
  cat "$OUT_FILE" >&2
  exit 1
fi

if [ -z "$SELFTEST_LINE" ]; then
  echo "FAIL: no '#SELFTEST {...}' line found on stdout (process exit code was $EXIT_CODE) -- the file crashed before reaching its own verdict line, or silently did nothing" >&2
  echo "-- full output --" >&2
  cat "$OUT_FILE" >&2
  exit 1
fi

STATUS="$(echo "$SELFTEST_LINE" | grep -oE ':status :[a-z]+' | awk '{print $2}')"
CHECKS="$(echo "$SELFTEST_LINE" | grep -oE ':checks [0-9]+' | awk '{print $2}')"
FAILURES="$(echo "$SELFTEST_LINE" | grep -oE ':failures [0-9]+' | awk '{print $2}')"

if [ "$EXIT_CODE" -eq 0 ] && [ "$STATUS" = ":pass" ]; then
  echo "OK: shim selftest PASSED ($CHECKS checks, $FAILURES failures, exit 0)"
  exit 0
else
  echo "FAIL: shim selftest FAILED (exit code $EXIT_CODE, status ${STATUS:-<none>}, $CHECKS checks, $FAILURES failures)" >&2
  echo "-- failing check lines --" >&2
  if ! grep -E '^\s*FAIL - ' "$OUT_FILE" >&2; then
    echo "(none found in captured output -- failure was signaled by exit code or a missing/non-:pass #SELFTEST line, not by an individual FAIL - line)" >&2
  fi
  exit 1
fi
