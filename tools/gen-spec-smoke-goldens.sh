#!/bin/bash
# tools/gen-spec-smoke-goldens.sh
#
# Regenerates tests/spec-smoke/*.golden from the ORACLE -- real Clojure,
# never mova. Each golden is the byte-exact stdout of running the corpus
# file itself under `clojure -M -i`, minus the trailing `:end` marker.
# `tests/spec_smoke_test.rs` then diffs mova's stdout against it on every
# `cargo test`, so a golden regenerated from mova's own output would turn
# the gate into a tautology. Run this ONLY when a corpus file changes, and
# only with the pinned deps below.
#
# Oracle pin (identical to tests/spec-smoke/RUNNING.md section 2 and to
# tests/conformance/CLOJURE_VERSION):
#   org.clojure/clojure     1.13.0-alpha6   (ships clojure.spec.alpha)
#   org.clojure/test.check  1.1.1
#
# `2>/dev/null` drops stderr only: both hosts print WARNING: / Reflection
# warning, lines there and neither side's stderr is compared.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SMOKE_DIR="$ROOT/tests/spec-smoke"

DEPS='{:deps {org.clojure/clojure {:mvn/version "1.13.0-alpha6"}
              org.clojure/test.check {:mvn/version "1.1.1"}}}'

cd "$ROOT"
for name in smoke stest-smoke seeded-gen; do
  src="$SMOKE_DIR/$name.mova"
  dst="$SMOKE_DIR/$name.golden"
  [ -f "$src" ] || { echo "FAIL: no corpus at $src" >&2; exit 1; }
  # -i loads the file without echoing each top-level form's value; the
  # trailing -e ':end' makes the exit status meaningful and its printed
  # line is stripped by `sed '$d'`.
  clojure -Sdeps "$DEPS" -M -i "$src" -e ':end' 2>/dev/null | sed '$d' > "$dst"
  echo "wrote $dst ($(wc -l < "$dst" | tr -d ' ') lines)"
done
