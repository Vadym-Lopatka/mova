#!/bin/bash
# Regenerates core/core-docs.dat: :added / :arglists / :doc of every clojure.core
# public var that mova also has, taken from REAL Clojure (classpath in
# $CLOJURE_CP, e.g. the nREPL oracle's classpath.txt). One record per var:
#   name US added US arglists US doc RS   (US = 0x1f, RS = 0x1e), sorted by name.
# The file is embedded with include_str! and read lazily by src/coredocs.rs;
# nothing from it is loaded at boot.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CP="${CLOJURE_CP:?set CLOJURE_CP to a Clojure 1.13 classpath}"
TMP="$(mktemp -d)"
"$ROOT/target/release/mova" -e '(doseq [s (sort (map str (keys (ns-publics (quote clojure.core)))))] (println s))' > "$TMP/mova-core-names.txt"
cp "$ROOT/tools/gen-core-docs.clj" "$TMP/"
(cd "$TMP" && java -cp "$CP" clojure.main gen-core-docs.clj)
cp "$TMP/core-docs.dat" "$ROOT/core/core-docs.dat"
