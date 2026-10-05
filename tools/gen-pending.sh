#!/bin/bash
# tools/gen-pending.sh
#
# Regenerates the two GENERATED columns of a pending-conformance area file
# under tests/conformance/pending/ (the third column, `<area>.corpus`, is
# hand-authored and never touched here):
#
#   <area>.golden -- real Clojure 1.13.0-alpha6 (pinned in
#                    tests/conformance/CLOJURE_VERSION), via
#                    tools/jvm-pending-runner.clj. One JVM process per
#                    area file, one session per file (later forms see
#                    earlier def/defn -- same session model as
#                    tools/jvm-runner.clj).
#
#   <area>.mova   -- mova's CURRENT behavior, via the real `mova` binary.
#                    Documentation only: tests/pending_conformance_test.rs
#                    never reads this file, so it can never make that test
#                    fail merely because mova's error text changed. Uses
#                    the IDENTICAL session model as the live driver -- see
#                    tests/pending_conformance_test.rs's module doc,
#                    "Session model": ONE `mova <script>` process for the
#                    whole area file (a `def` on an earlier line stays
#                    visible to a later line, matching the golden side),
#                    forms wrapped in mova's own `try`/`catch` and tagged
#                    with `##PENDING-{BEGIN,OK,ERR}##<index>` markers so
#                    hang safety survives without falling back to
#                    per-form isolation. An EARLIER version of this
#                    script (and of the live driver) ran one fresh
#                    `mova -e <form>` process per form instead; that was
#                    a real correctness bug, not a simplification -- see
#                    the driver's module doc for the concrete repro (a
#                    `def` invisible to a later form makes mova look like
#                    it doesn't support something it actually does, which
#                    can hide a real, promotable CONFORM behind a fake
#                    divergence). The `.mova` and `.golden` columns must
#                    use the same session model, or a diff between them
#                    means nothing.
#
# Usage:
#   tools/gen-pending.sh              # regenerate every area
#   tools/gen-pending.sh foo bar      # regenerate only foo.{golden,mova}
#                                       and bar.{golden,mova}
#
# Both columns are ALWAYS regenerated together for a given area: a golden
# and a mova column captured against different code states (a rebased
# corpus vs. a stale binary) is worse than either being merely out of
# date, since a diff between them then means nothing.
#
# The canonicalization lint (rejecting a golden whose value depends on
# hash-iteration order, gensym names, wall-clock, or randomness) lives in
# tests/pending_conformance_test.rs, not here -- see that file's
# `lint_nondeterminism` doc comment for exactly what it checks. This
# script's job is purely regeneration; run
# `cargo test --test pending_conformance_test` afterward (or just let CI
# do it) to have the lint judge what this script produced.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PENDING_DIR="$ROOT/tests/conformance/pending"
VERSION_FILE="$ROOT/tests/conformance/CLOJURE_VERSION"
RUNNER="$ROOT/tools/jvm-pending-runner.clj"

if [ ! -f "$VERSION_FILE" ]; then
  echo "FAIL: $VERSION_FILE not found" >&2
  exit 1
fi
CLOJURE_VERSION="$(tr -d '[:space:]' < "$VERSION_FILE")"

# GNU `timeout` under either name; macOS ships neither in its BSD base, so
# this also works out of the box when GNU coreutils is installed via
# Homebrew (the dev environment this was written against has both names
# on PATH).
if command -v timeout >/dev/null 2>&1; then
  TIMEOUT_BIN="timeout"
elif command -v gtimeout >/dev/null 2>&1; then
  TIMEOUT_BIN="gtimeout"
else
  echo "FAIL: need a 'timeout' (or 'gtimeout') binary on PATH -- GNU coreutils" >&2
  exit 1
fi
# Per-FILE timeout (see tests/pending_conformance_test.rs's module doc,
# "Session model"): a generous flat base plus a small per-form allowance
# for slack on a slow machine -- every normal, non-hanging file finishes
# in well under a second regardless of form count, since it's ONE process
# for the whole file. Mirrors the live driver's `file_timeout` formula
# (10s + 150ms/form) closely enough that a form classified TIMEOUT here
# and one classified TIMEOUT by `cargo test` are the same real-world
# event, without needing bash floating-point to match it to the
# millisecond.
file_timeout_secs() {
  local num_forms="$1"
  echo $((10 + (150 * num_forms + 999) / 1000))
}

echo "gen-pending: real Clojure $CLOJURE_VERSION (tests/conformance/CLOJURE_VERSION), mova session model: one process per area file"

# ---------------------------------------------------------------------------
# .golden -- one real-JVM `clojure` process per area file, via
# tools/jvm-pending-runner.clj (this project's OWN runner, distinct from
# tools/jvm-runner.clj which the main corpus owns and which emits bare
# `ERR` instead of `ERR<TAB><ExceptionSimpleName>`).
# ---------------------------------------------------------------------------
gen_golden() {
  local corpus="$1" golden="$2" num_forms="$3"
  local deps_edn
  deps_edn=$(printf '{:deps {org.clojure/clojure {:mvn/version "%s"}}}' "$CLOJURE_VERSION")
  local out
  if ! out=$(CORPUS_FILE="$corpus" clojure -Sdeps "$deps_edn" -M -e "(load-file \"$RUNNER\")" 2>&1 1>"$golden.tmp"); then
    rm -f "$golden.tmp"
    echo "FAIL: jvm-pending-runner errored on $corpus:" >&2
    echo "$out" >&2
    exit 1
  fi
  local got
  got=$(wc -l < "$golden.tmp" | tr -d '[:space:]')
  if [ "$got" != "$num_forms" ]; then
    rm -f "$golden.tmp"
    echo "FAIL: $corpus has $num_forms form(s) but jvm-pending-runner produced $got golden line(s)" >&2
    exit 1
  fi
  mv "$golden.tmp" "$golden"
}

# ---------------------------------------------------------------------------
# .mova -- ONE `mova <script>` process for the WHOLE area file, matching
# tests/pending_conformance_test.rs's `run_session`/`build_session_script`
# exactly (same protocol tags, same eval-via-string-literal technique, same
# try/catch-per-form wrapping, same timeout formula) so the `.mova` column
# and a live `cargo test` run can never disagree by construction.
# ---------------------------------------------------------------------------

BEGIN_TAG="##PENDING-BEGIN##"
OK_TAG="##PENDING-OK##"
ERR_TAG="##PENDING-ERR##"

# Generates the one mova program that evaluates every (comment/blank
# filtered) form in $1, in order, in a single session -- see this file's
# module doc and tests/pending_conformance_test.rs's `build_session_script`
# doc comment for the full rationale (forms embedded as string literals
# read via `eval`/`read-string`, not spliced as raw source, so a
# deliberately-malformed form -- reader.corpus exists to probe exactly
# that -- can't corrupt the wrapper's own parens; every form wrapped in
# `try`/`catch` so one throwing can't stop the forms after it).
build_session_script() {
  local corpus="$1" script_path="$2"
  {
    printf '(def pending-src ['
    grep -vE '^[[:space:]]*(;;|$)' "$corpus" | awk '
      {
        line = $0
        gsub(/\\/, "\\\\", line)
        gsub(/"/, "\\\"", line)
        printf "\"%s\" ", line
      }
    '
    printf '])\n'
    cat <<MOVA_EOF
(doseq [i (range (count pending-src))]
  (println (str "$BEGIN_TAG" i))
  (try
    (println (str "$OK_TAG" i "\\t" (pr-str (eval (read-string (nth pending-src i))))))
    (catch e
      (println (str "$ERR_TAG" i "\\t" (pr-str e))))))
MOVA_EOF
  } > "$script_path"
}

gen_mova() {
  local corpus="$1" mova="$2" binary="$3"
  local num_forms
  num_forms=$(grep -vE '^[[:space:]]*(;;|$)' "$corpus" | wc -l | tr -d '[:space:]')

  local script_path out_path secs
  script_path=$(mktemp)
  out_path=$(mktemp)
  build_session_script "$corpus" "$script_path"
  secs=$(file_timeout_secs "$num_forms")

  # Merge stderr into stdout (`> out 2>&1`): mova writes diagnostics to
  # stderr, and a crash diagnostic there must never be silently dropped
  # -- see this file's module doc. Exit status is ignored (`|| true`):
  # a timeout-killed or crashed process is expected and handled entirely
  # by which ##PENDING-*## markers the awk parser below finds, not by
  # the process's exit code.
  "$TIMEOUT_BIN" "${secs}s" "$binary" "$script_path" >"$out_path" 2>&1 || true

  awk -v n="$num_forms" -v begin_tag="$BEGIN_TAG" -v ok_tag="$OK_TAG" -v err_tag="$ERR_TAG" '
    BEGIN {
      for (i = 0; i < n; i++) { began[i] = 0; result[i] = "" }
    }
    {
      line = $0
      if (index(line, begin_tag) == 1) {
        idx = substr(line, length(begin_tag) + 1) + 0
        if (idx >= 0 && idx < n) began[idx] = 1
      } else if (index(line, ok_tag) == 1) {
        rest = substr(line, length(ok_tag) + 1)
        tab = index(rest, "\t")
        if (tab > 0) {
          idx = substr(rest, 1, tab - 1) + 0
          if (idx >= 0 && idx < n) result[idx] = "OK\t" substr(rest, tab + 1)
        }
      } else if (index(line, err_tag) == 1) {
        rest = substr(line, length(err_tag) + 1)
        tab = index(rest, "\t")
        if (tab > 0) {
          idx = substr(rest, 1, tab - 1) + 0
          if (idx >= 0 && idx < n) result[idx] = "ERR\t" substr(rest, tab + 1)
        }
      }
    }
    END {
      for (i = 0; i < n; i++) {
        if (result[i] != "") print result[i]
        else if (began[i]) print "TIMEOUT"
        else print "UNREACHED"
      }
    }
  ' "$out_path" >"$mova.tmp"

  local got
  got=$(wc -l < "$mova.tmp" | tr -d '[:space:]')
  if [ "$got" != "$num_forms" ]; then
    echo "FAIL: $corpus has $num_forms form(s) but session parsing produced $got .mova line(s) (raw session output kept at $out_path for inspection)" >&2
    exit 1
  fi

  if [ "$num_forms" -gt 0 ] && ! grep -qvE '^UNREACHED$' "$mova.tmp"; then
    echo "FAIL: every form in $corpus came back UNREACHED -- the session process likely crashed or failed to start; raw session output:" >&2
    cat "$out_path" >&2
    rm -f "$mova.tmp"
    exit 1
  fi

  mv "$mova.tmp" "$mova"
  rm -f "$script_path" "$out_path"
}

areas=()
if [ "$#" -gt 0 ]; then
  areas=("$@")
else
  while IFS= read -r -d '' f; do
    areas+=("$(basename "$f" .corpus)")
  done < <(find "$PENDING_DIR" -maxdepth 1 -name '*.corpus' -print0 | sort -z)
fi

if [ "${#areas[@]}" -eq 0 ]; then
  echo "FAIL: no pending areas found under $PENDING_DIR" >&2
  exit 1
fi

# Build once, up front, so gen_mova's per-form loop below shells out to a
# binary that isn't itself being rebuilt mid-run.
echo "gen-pending: cargo build --release --bin mova"
(cd "$ROOT" && cargo build --release --bin mova --quiet)
BINARY="$ROOT/target/release/mova"

for area in "${areas[@]}"; do
  corpus="$PENDING_DIR/$area.corpus"
  golden="$PENDING_DIR/$area.golden"
  mova="$PENDING_DIR/$area.mova"
  if [ ! -f "$corpus" ]; then
    echo "FAIL: no such pending corpus: $corpus" >&2
    exit 1
  fi
  num_forms=$(grep -vE '^[[:space:]]*(;;|$)' "$corpus" | wc -l | tr -d '[:space:]')

  gen_golden "$corpus" "$golden" "$num_forms"
  gen_mova "$corpus" "$mova" "$BINARY"

  echo "  $area: $num_forms forms -> $golden (real Clojure $CLOJURE_VERSION), $mova (mova, one session)"
done

echo "done."
