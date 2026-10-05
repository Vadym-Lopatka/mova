#!/bin/bash
# tools/verify-vendor.sh
#
# Anti-cheating check for the Clojure-suite conformance machinery's
# "no-edit rule": every file under tests/clojure-suite/vendor/ (vendored
# TEST files, scored in the North Star) and tests/clojure-suite/vendor-libs/
# (vendored test-support LIBRARIES -- test.check, test.generative,
# data.generators, clojure.walk/template -- materialized-only, never
# scored) must be byte-identical to their pinned upstream release. This
# script re-hashes every vendored file against MANIFEST.sha256 (vendor/)
# and MANIFEST-LIBS.sha256 (vendor-libs/) and fails (non-zero exit) on
# any mismatch, missing file, or extra un-manifested file in EITHER tree.
# Deliberately simple (a straight `shasum -c` plus a file-set diff, run
# twice) so it's auditable at a glance -- the score cannot be raised by
# editing a vendored test file, and a vendored library cannot be quietly
# swapped for a modified one, without this catching it.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SUITE_DIR="$ROOT/tests/clojure-suite"
VENDOR_DIR="$SUITE_DIR/vendor"
MANIFEST="$SUITE_DIR/MANIFEST.sha256"
VENDOR_LIBS_DIR="$SUITE_DIR/vendor-libs"
MANIFEST_LIBS="$SUITE_DIR/MANIFEST-LIBS.sha256"

if [ ! -f "$MANIFEST" ]; then
  echo "FAIL: manifest not found at $MANIFEST" >&2
  exit 1
fi

if [ ! -d "$VENDOR_DIR" ]; then
  echo "FAIL: vendor dir not found at $VENDOR_DIR" >&2
  exit 1
fi

if [ ! -f "$MANIFEST_LIBS" ]; then
  echo "FAIL: manifest not found at $MANIFEST_LIBS" >&2
  exit 1
fi

if [ ! -d "$VENDOR_LIBS_DIR" ]; then
  echo "FAIL: vendor-libs dir not found at $VENDOR_LIBS_DIR" >&2
  exit 1
fi

status=0

# 1. Every manifested file must hash-match.
echo "== checksum verification (vendor/) =="
if (cd "$VENDOR_DIR" && shasum -a 256 -c "$MANIFEST" --ignore-missing); then
  echo "OK: all manifested files match their recorded SHA-256"
else
  echo "FAIL: one or more vendored files do not match MANIFEST.sha256 (edited vendor file, or corruption)" >&2
  status=1
fi

# 2. File-set diff: every manifested file must exist in vendor/, and
#    every file in vendor/ must be listed in the manifest (catches
#    silently-added, un-manifested files just as much as edits).
manifest_files=$(grep -vE '^\s*#' "$MANIFEST" | grep -vE '^\s*$' | awk '{print $2}' | sort)
vendor_files=$(cd "$VENDOR_DIR" && ls -1 *.clj 2>/dev/null | sort)

missing=$(comm -23 <(echo "$manifest_files") <(echo "$vendor_files"))
extra=$(comm -13 <(echo "$manifest_files") <(echo "$vendor_files"))

if [ -n "$missing" ]; then
  echo "FAIL: files listed in manifest but missing from vendor/:" >&2
  echo "$missing" >&2
  status=1
fi

if [ -n "$extra" ]; then
  echo "FAIL: files present in vendor/ but not listed in manifest (un-manifested vendor file):" >&2
  echo "$extra" >&2
  status=1
fi

if [ -z "$missing" ] && [ -z "$extra" ] && (cd "$VENDOR_DIR" && shasum -a 256 -c "$MANIFEST" --ignore-missing >/dev/null 2>&1); then
  count=$(echo "$vendor_files" | grep -c . || true)
  echo "OK: vendor/ and MANIFEST.sha256 agree exactly ($count files)"
fi
echo

# 3. Same two checks, for vendor-libs/ against MANIFEST-LIBS.sha256.
#    Files there are nested (e.g. clojure/test/check/generators.cljc), so
#    the file-set listing uses `find` (relative paths) instead of
#    vendor/'s flat `ls -1 *.clj`.
echo "== checksum verification (vendor-libs/) =="
if (cd "$VENDOR_LIBS_DIR" && shasum -a 256 -c "$MANIFEST_LIBS" --ignore-missing); then
  echo "OK: all manifested vendor-libs files match their recorded SHA-256"
else
  echo "FAIL: one or more vendor-libs files do not match MANIFEST-LIBS.sha256 (edited library file, or corruption)" >&2
  status=1
fi

manifest_libs_files=$(grep -vE '^\s*#' "$MANIFEST_LIBS" | grep -vE '^\s*$' | awk '{print $2}' | sort)
vendor_libs_files=$(cd "$VENDOR_LIBS_DIR" && find . -type f | sed 's|^\./||' | sort)

missing_libs=$(comm -23 <(echo "$manifest_libs_files") <(echo "$vendor_libs_files"))
extra_libs=$(comm -13 <(echo "$manifest_libs_files") <(echo "$vendor_libs_files"))

if [ -n "$missing_libs" ]; then
  echo "FAIL: files listed in MANIFEST-LIBS.sha256 but missing from vendor-libs/:" >&2
  echo "$missing_libs" >&2
  status=1
fi

if [ -n "$extra_libs" ]; then
  echo "FAIL: files present in vendor-libs/ but not listed in MANIFEST-LIBS.sha256 (un-manifested vendor-libs file):" >&2
  echo "$extra_libs" >&2
  status=1
fi

if [ -z "$missing_libs" ] && [ -z "$extra_libs" ] && (cd "$VENDOR_LIBS_DIR" && shasum -a 256 -c "$MANIFEST_LIBS" --ignore-missing >/dev/null 2>&1); then
  count_libs=$(echo "$vendor_libs_files" | grep -c . || true)
  echo "OK: vendor-libs/ and MANIFEST-LIBS.sha256 agree exactly ($count_libs files)"
fi

exit "$status"
