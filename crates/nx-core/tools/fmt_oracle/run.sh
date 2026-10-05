#!/bin/sh
# Needs MOVA_FMT_VENDOR_DIR: dir holding cljfmt-0.16.4, rewrite-clj-1.2.55, tools.reader-1.6.0 sources.
# Builds scratch corpora outputs: run.sh [scratch]  (corp/<name> must exist: lib, e2test, jars)
S=${1:-${TMPDIR:-/tmp}/nx/fmt}
D=$(cd "$(dirname "$0")" && pwd)
V=${MOVA_FMT_VENDOR_DIR:?usage: MOVA_FMT_VENDOR_DIR=/path/to/vendor $0 [scratch]}
SD="{:paths [\"$V/cljfmt-0.16.4\" \"$V/rewrite-clj-1.2.55\" \"$V/tools.reader-1.6.0\" \".\"]}"
for c in lib e2test jars; do
  python3 $D/messy.py $S/corp/$c $S/corp/messy-$c 1
  (cd $D && clojure -Sdeps "$SD" -M oracle.clj $S/corp/$c $S/out/$c 2>/dev/null)
  (cd $D && clojure -Sdeps "$SD" -M oracle.clj $S/corp/messy-$c $S/out/messy-$c 2>/dev/null)
done
# Options variants: clojure -M oracle.clj <in> <out> cfg/<name>.edn  (compare with `fmt_cmp ... --cfg <name>`)
# Range formatting: clojure -M range.clj <in> <out>  (compare with `fmt_range_cmp <in> <out>`)
# Unit-test cases: clojure -M gen_cases.clj  (regenerates src/fmt/cases.rs)
