#!/bin/bash
# tools/oracle-census.sh
#
# Thin bash orchestrator for tools/oracle-census.clj -- the real
# Clojure/JVM side that does all the counting. This script's only jobs
# are: locate the pinned oracle, build the classpath every per-file
# process needs, loop over every vendored file spawning ONE FRESH
# `clojure` process per file (under a wall-clock timeout, so a single
# hung or crashing file can never wedge the whole census), do that TWICE
# (an independent second pass, so a file whose assertion count is
# nondeterministic -- randomized/generative tests -- can be detected by
# diffing the two runs), and hand the results to oracle-census.clj's
# `aggregate` to write the final artifact. No numeric/EDN logic lives
# here -- see tools/oracle-census.clj's module doc for why, and for what
# this whole thing IS and why it must never be hand-edited.
#
# Also computes a :vendor-fingerprint (SHA-256 of MANIFEST.sha256 itself,
# plus the vendored file count and sorted basenames) and hands it to
# `aggregate` too, so the artifact can prove it was measured against the
# CURRENT vendored file set -- see the fingerprint block below and
# tools/generate-compat-report.bb's freshness gate, which is the CHEAP
# check that actually enforces this on every report run. This census is
# ~44 minutes and deliberately stays a manual, occasional command --
# it is never re-run automatically by tools/conformance-report.sh.
#
# Output: tests/clojure-suite/ORACLE-ASSERTIONS.edn
#
# Usage: bash tools/oracle-census.sh
#   ORACLE_CENSUS_TIMEOUT   per-file wall-clock timeout in seconds
#                           (default 180 -- clojure.test-clojure.vars.clj
#                           alone has been observed to take ~60s under
#                           real Clojure; this leaves real margin)
#   ORACLE_CENSUS_SCRATCH   scratch dir for per-run intermediate EDN files
#                           (default: a fresh dir under $TMPDIR; wiped and
#                           recreated at the start of every run so a prior
#                           run's leftovers can never leak into this one)
#   ORACLE_CENSUS_RESUME    1 to KEEP the scratch dir and skip any per-file
#                           census whose result EDN is already in it. Pair
#                           with ORACLE_CENSUS_SCRATCH so the same dir is
#                           found again. See "Resuming" below.
#   ORACLE_CENSUS_BUDGET    wall-clock seconds this INVOCATION may spend
#                           censusing files before it stops early and exits
#                           3, leaving the scratch dir intact for the next
#                           resumed invocation. 0/unset = no budget.
#
# Resuming (SPEC-W6b). This census is ~44 minutes end to end, which is
# longer than some agents' and CI steps' per-command wall-clock limit, and
# the old all-or-nothing shape meant such a caller could not run it AT ALL
# -- the failure mode that invites hand-editing the artifact, which its
# own header forbids in capitals. RESUME+BUDGET makes the SAME run
# splittable across several invocations:
#
#   S=/tmp/oracle-census-resume
#   for i in 1 2 3 4 5 6 7 8; do
#     ORACLE_CENSUS_SCRATCH=$S ORACLE_CENSUS_RESUME=1 \
#     ORACLE_CENSUS_BUDGET=480 bash tools/oracle-census.sh && break
#   done
#
# Nothing about the RESULT changes: every per-file number still comes from
# a fresh `clojure` process running `census-one` on the current vendored
# bytes, both independent passes still happen, and `aggregate` still writes
# the whole artifact in one go from those two directories. What resuming
# skips is only re-running a file whose result is already on disk from
# THIS scratch dir. Do not point a resumed run at a scratch dir left over
# from a census of DIFFERENT vendored bytes -- that is the one way to get
# a stale number in, and it is why the default (no RESUME) still wipes.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VENDOR_DIR="$ROOT/tests/clojure-suite/vendor"
MANIFEST_SHA="$ROOT/tests/clojure-suite/MANIFEST.sha256"
VERSION_FILE="$ROOT/tests/conformance/CLOJURE_VERSION"
OUT_EDN="$ROOT/tests/clojure-suite/ORACLE-ASSERTIONS.edn"
CENSUS_CLJ="$ROOT/tools/oracle-census.clj"
BOOTSTRAP="$ROOT/tools/bootstrap-oracle.sh"

PER_FILE_TIMEOUT="${ORACLE_CENSUS_TIMEOUT:-180}"
RESUME="${ORACLE_CENSUS_RESUME:-0}"
BUDGET="${ORACLE_CENSUS_BUDGET:-0}"
# Set to 1 by run_census when it stopped early because BUDGET ran out.
BUDGET_HIT=0
INVOCATION_START="$(date +%s)"
SCRATCH="${ORACLE_CENSUS_SCRATCH:-${TMPDIR:-/tmp}/oracle-census-run}"

# Fallback locations if tools/bootstrap-oracle.sh (a sibling agent's
# script) isn't present yet on this checkout -- see this task's own brief
# for these exact paths. bootstrap-oracle.sh is always preferred: it
# verifies the pin, not just assumes a directory exists.
FALLBACK_DEPS_DIR="${TMPDIR:-/tmp}/clj113"
FALLBACK_SRC_DIR="${TMPDIR:-/tmp}/clojure-1.13.0-alpha6"

log() { echo "oracle-census: $*" >&2; }

[ -f "$VERSION_FILE" ] || { log "FATAL: missing $VERSION_FILE"; exit 1; }
PIN_VERSION="$(tr -d '[:space:]' < "$VERSION_FILE")"
log "pinned oracle version: $PIN_VERSION"

if [ -x "$BOOTSTRAP" ]; then
  log "== locating oracle via tools/bootstrap-oracle.sh (materializes + verifies) =="
  DEPS_DIR="$(bash "$BOOTSTRAP" --print-deps-dir | tail -1)"
  SRC_DIR="$(bash "$BOOTSTRAP" --print-src-dir | tail -1)"
else
  log "WARNING: $BOOTSTRAP not found -- falling back to hardcoded scratch paths (unverified)"
  DEPS_DIR="$FALLBACK_DEPS_DIR"
  SRC_DIR="$FALLBACK_SRC_DIR"
fi
[ -f "$DEPS_DIR/deps.edn" ] || { log "FATAL: no deps.edn at $DEPS_DIR -- run tools/bootstrap-oracle.sh"; exit 1; }
[ -d "$SRC_DIR/test/clojure/test_clojure" ] || { log "FATAL: no test/clojure/test_clojure under $SRC_DIR"; exit 1; }
log "  deps dir: $DEPS_DIR"
log "  src dir:  $SRC_DIR"

command -v clojure >/dev/null 2>&1 || { log "FATAL: no 'clojure' CLI on PATH"; exit 1; }

UPSTREAM_COMMIT="$(grep -m1 -E '^#[[:space:]]*commit:' "$MANIFEST_SHA" | sed -E 's/^#[[:space:]]*commit:[[:space:]]*//')"
[ -n "$UPSTREAM_COMMIT" ] || log "WARNING: could not read upstream commit from $MANIFEST_SHA"
log "upstream commit: ${UPSTREAM_COMMIT:-<unknown>}"

# A handful of vendored files (api.clj, edn.clj, data_structures.clj,
# generators.clj, numbers.clj, parse.clj, sequences.clj, transducers.clj)
# :require external test-scope libraries that the pinned oracle's own
# deps.edn (org.clojure/clojure only) does not include, but that the
# pinned commit's OWN pom.xml declares as real test dependencies. Read
# the exact versions from that pom.xml rather than hardcoding them, so a
# future pin bump picks up new versions automatically instead of silently
# going stale.
pom_dep_version() {
  grep -A5 "<artifactId>${1}</artifactId>" "$SRC_DIR/pom.xml" \
    | grep -m1 -oE '<version>[^<]+</version>' \
    | sed -E 's/<\/?version>//g'
}
TEST_CHECK_VERSION="$(pom_dep_version 'test\.check')"
TEST_GENERATIVE_VERSION="$(pom_dep_version 'test\.generative')"
[ -n "$TEST_CHECK_VERSION" ] || { TEST_CHECK_VERSION="1.1.3"; log "WARNING: could not read org.clojure/test.check version from pom.xml, falling back to $TEST_CHECK_VERSION"; }
[ -n "$TEST_GENERATIVE_VERSION" ] || { TEST_GENERATIVE_VERSION="1.1.1"; log "WARNING: could not read org.clojure/test.generative version from pom.xml, falling back to $TEST_GENERATIVE_VERSION"; }
log "extra test-scope deps (from pinned pom.xml): org.clojure/test.check $TEST_CHECK_VERSION, org.clojure/test.generative $TEST_GENERATIVE_VERSION (pulls org.clojure/data.generators transitively)"

if [ "$RESUME" = "1" ]; then
  log "RESUME: keeping any existing results in $SCRATCH (a file already censused there is skipped)"
else
  rm -rf "$SCRATCH"
fi
mkdir -p "$SCRATCH/run1" "$SCRATCH/run2" "$SCRATCH/java-classes"
log "scratch dir: $SCRATCH"

log "== resolving classpath (network on first run; cached under ~/.m2 after) =="
JARS_CP="$(clojure -Sdeps "{:deps {org.clojure/clojure {:mvn/version \"$PIN_VERSION\"} org.clojure/test.check {:mvn/version \"$TEST_CHECK_VERSION\"} org.clojure/test.generative {:mvn/version \"$TEST_GENERATIVE_VERSION\"}}}" -Spath)"

# The suite's own Java test fixtures (test/java/clojure/test/*.java etc.)
# -- try_catch.clj needs clojure.test.ReflectorTryCatchFixture, and other
# vendored files reference sibling fixtures from the same directory.
# Compiled best-effort: a javac failure here degrades the affected
# file(s) to an honest :load-error rather than aborting the whole census.
if command -v javac >/dev/null 2>&1; then
  log "== compiling upstream Java test fixtures ($SRC_DIR/test/java) =="
  find "$SRC_DIR/test/java" -name '*.java' > "$SCRATCH/java-sources.txt"
  if ! javac -d "$SCRATCH/java-classes" -cp "$JARS_CP" @"$SCRATCH/java-sources.txt" > "$SCRATCH/javac.log" 2>&1; then
    log "WARNING: javac failed compiling upstream test fixtures (see $SCRATCH/javac.log) -- files needing them (e.g. try_catch.clj) will honestly :load-error"
  fi
else
  log "WARNING: no 'javac' on PATH -- files needing compiled Java test fixtures (e.g. try_catch.clj) will honestly :load-error"
fi

FULL_CP="$JARS_CP:$SRC_DIR/test:$SCRATCH/java-classes"

# ---------------------------------------------------------------------
# Manifest: one {:file "x.clj" :ns the.ns.sym} entry per vendored file,
# derived from each file's own leading (ns ...) form. Deliberately dumb
# (grep, not a real reader) -- fine here because every vendored file's
# first (ns ...) is the genuine one and stays on one line (spot-checked
# against all vendored files when this script was written). A file where
# this scan finds nothing still gets a manifest entry (:ns nil) so it is
# never silently dropped from :files-total -- see oracle-census.clj's
# `aggregate`.
MANIFEST="$SCRATCH/manifest.edn"
{
  echo "["
  while IFS= read -r f; do
    bn="$(basename "$f")"
    ns="$(grep -m1 -oE '\(ns +[a-zA-Z0-9.\-]+' "$f" | awk '{print $2}')"
    if [ -n "$ns" ]; then
      printf '{:file "%s" :ns %s}\n' "$bn" "$ns"
    else
      printf '{:file "%s" :ns nil}\n' "$bn"
      log "WARNING: no (ns ...) form found by the plain scan in $bn"
    fi
  done < <(find "$VENDOR_DIR" -maxdepth 1 -name '*.clj' | sort)
  echo "]"
} > "$MANIFEST"
FILE_COUNT="$(grep -c ':file ' "$MANIFEST")"
log "manifest: $FILE_COUNT vendored files"

# ---------------------------------------------------------------------
# Vendored-source fallback tree (SPEC-W6b).
#
# `census-one` `require`s each file's namespace off the classpath, and for
# 48 of the 51 vendored files that resolves inside the pinned upstream
# clone at $SRC_DIR/test. The three clojure.spec.alpha test files
# (spec.clj, instr.clj, multi_spec.clj) do not live there -- they come
# from the github.com/clojure/spec.alpha repository, which is a SEPARATE
# repo even though the library itself ships with Clojure (see
# tests/clojure-suite/MANIFEST.sha256's "SECOND REPO" block).
#
# Rather than adding a second upstream checkout path to this script, every
# vendored file is materialized here at its OWN ns-implied path and that
# tree is appended to the classpath LAST. Two consequences, both wanted:
#
#   * for the 48 primary files, $SRC_DIR/test still wins -- the numbers
#     this census has always produced are produced from exactly the same
#     bytes as before;
#   * the 3 spec files (and any future vendored file whose upstream is not
#     the primary clone) resolve here, from the SAME hash-locked bytes
#     tools/verify-vendor.sh checks and tools/clojure-suite-run.bb scores
#     mova against. Numerator and denominator read one set of bytes.
VENDOR_SRC="$SCRATCH/vendor-src"
rm -rf "$VENDOR_SRC"
mkdir -p "$VENDOR_SRC"
vendor_src_count=0
while IFS= read -r entry; do
  bn="$(echo "$entry" | sed -E 's/^\{:file "([^"]*)".*/\1/')"
  ns="$(echo "$entry" | grep -oE ':ns [a-zA-Z0-9.\-]+' | awk '{print $2}')"
  [ -n "$ns" ] || continue
  rel="$(echo "$ns" | tr '.-' '/_')"
  mkdir -p "$VENDOR_SRC/$(dirname "$rel")"
  cp "$VENDOR_DIR/$bn" "$VENDOR_SRC/$rel.clj"
  vendor_src_count=$((vendor_src_count + 1))
done < <(grep -o '{:file "[^"]*" :ns [a-zA-Z0-9.\-]*}' "$MANIFEST")
log "vendored-source fallback tree: $vendor_src_count file(s) under $VENDOR_SRC (classpath LAST, so the pinned clone still wins)"
FULL_CP="$FULL_CP:$VENDOR_SRC"

# ---------------------------------------------------------------------
# Vendor fingerprint: a fact about WHICH files are currently vendored
# (independent of anything real Clojure or mova does to them), so the
# census artifact can prove it was measured against the CURRENT vendored
# set and not a stale, smaller one. Without this, vendoring a 49th file
# leaves the artifact's :assertions/:deftests totals unchanged (still the
# 48-file numbers) while the headline percentage silently goes UP,
# because the denominator never moved. See
# tests/clojure-suite/ORACLE-ASSERTIONS.edn's header and
# tools/generate-compat-report.bb's freshness gate for the other half of
# this fix -- this script only RECORDS the fingerprint; the report
# generator is what actually enforces it on every run (cheaply, without
# re-running this ~44-minute census).
#
#   :manifest-sha256  -- SHA-256 of tests/clojure-suite/MANIFEST.sha256
#                        ITSELF (the whole file, not a per-entry hash).
#                        Changes if any vendored file's content changes
#                        (its per-file hash in MANIFEST.sha256 changes)
#                        or if a file is added/removed (the manifest's
#                        line count and header counts change).
#   :vendored-count    -- count of *.clj files directly under
#                        tests/clojure-suite/vendor/ at census time.
#   :vendored-files    -- sorted basenames of those same files, so a
#                        same-count swap (remove one file, add a
#                        differently-named one) is still detected even
#                        though :vendored-count alone would not catch it.
VENDOR_FINGERPRINT_EDN="$SCRATCH/vendor-fingerprint.edn"
MANIFEST_SELF_SHA256="$(shasum -a 256 "$MANIFEST_SHA" | awk '{print $1}')"
{
  echo "{:manifest-sha256 \"$MANIFEST_SELF_SHA256\""
  echo " :vendored-count $FILE_COUNT"
  echo " :vendored-files ["
  find "$VENDOR_DIR" -maxdepth 1 -name '*.clj' -exec basename {} \; | sort | while IFS= read -r bn; do
    printf ' "%s"\n' "$bn"
  done
  echo "]}"
} > "$VENDOR_FINGERPRINT_EDN"
log "vendor fingerprint: manifest-sha256=$MANIFEST_SELF_SHA256 vendored-count=$FILE_COUNT"

run_census() {
  local run_dir="$1"
  local start end skipped=0 ran=0
  start=$(date +%s)
  while IFS= read -r entry; do
    local ns
    ns="$(echo "$entry" | grep -oE ':ns [a-zA-Z0-9.\-]+' | awk '{print $2}')"
    [ -n "$ns" ] || continue
    local out="$run_dir/$ns.edn"
    # RESUME: a result already on disk in THIS scratch dir was produced by
    # the same `census-one` against the same vendored bytes, so re-running
    # it would only cost time. (See the "Resuming" note at the top for the
    # one thing this must not be pointed at: a scratch dir from a census of
    # DIFFERENT vendored bytes.)
    if [ "$RESUME" = "1" ] && [ -s "$out" ]; then
      skipped=$((skipped + 1))
      continue
    fi
    # BUDGET: stop cleanly BEFORE starting a file we may not be able to
    # finish, so the caller can re-invoke and pick up exactly here.
    if [ "$BUDGET" != "0" ]; then
      local elapsed=$(( $(date +%s) - INVOCATION_START ))
      if [ "$elapsed" -ge "$BUDGET" ]; then
        BUDGET_HIT=1
        log "  budget of ${BUDGET}s reached after ${elapsed}s -- stopping before $ns"
        break
      fi
    fi
    ran=$((ran + 1))
    if ! timeout "${PER_FILE_TIMEOUT}s" clojure -Scp "$FULL_CP" -M -i "$CENSUS_CLJ" \
         -e "(census-one '$ns \"$out\")" \
         > "$run_dir/$ns.log" 2>&1; then
      log "  $ns: nonzero exit (timeout or crash before census-one's own catch could write a result) -- see $run_dir/$ns.log"
    fi
  done < <(grep -o '{:file "[^"]*" :ns [a-zA-Z0-9.\-]*}' "$MANIFEST")
  end=$(date +%s)
  log "  pass over $run_dir: $ran censused, $skipped already present, $((end - start))s"
}

log "== run 1/2 =="
run_census "$SCRATCH/run1"
if [ "$BUDGET_HIT" = "0" ]; then
  log "== run 2/2 (independent -- for the nondeterminism check) =="
  run_census "$SCRATCH/run2"
fi

if [ "$BUDGET_HIT" = "1" ]; then
  log "INCOMPLETE: this invocation stopped on its ${BUDGET}s budget. $OUT_EDN was NOT written."
  log "Re-invoke with the SAME ORACLE_CENSUS_SCRATCH and ORACLE_CENSUS_RESUME=1 to continue."
  exit 3
fi

log "== aggregating into $OUT_EDN =="
clojure -Scp "$FULL_CP" -M -i "$CENSUS_CLJ" \
  -e "(aggregate \"$MANIFEST\" \"$SCRATCH/run1\" \"$SCRATCH/run2\" \"$OUT_EDN\" \"$PIN_VERSION\" \"$UPSTREAM_COMMIT\" \"$VENDOR_FINGERPRINT_EDN\")"

log "wrote $OUT_EDN"
