#!/bin/sh
# Client gate: JVM nREPL 1.8.0 client-side tests against `mova nrepl`.
#   ./run.sh                      all cases
#   ./run.sh --only edn           cases whose name contains "edn"
#   MOVA=/path/to/mova ./run.sh   the binary to test (default: target/release/mova of this repo)
#   MOVA_TLS=/path/to/mova-tls    a binary built with --features tls (enables gate/tls-url)
#   NREPL_SRC=/path/to/nrepl      the JVM nREPL checkout with test/clojure (required)
# Needs `clojure` and `java` on the PATH. The first run downloads the test libraries.
set -e
HERE=$(cd "$(dirname "$0")" && pwd)
NREPL_SRC=${NREPL_SRC:?usage: NREPL_SRC=/path/to/nrepl ./run.sh (JVM nREPL checkout with test/clojure)}
export MOVA=${MOVA:-$(cd "$HERE/../../../.." && pwd)/target/release/mova}
CP_FILE=$HERE/.classpath
if [ ! -s "$CP_FILE" ]; then
  clojure -Sdeps "{:deps {local/gate {:local/root \"$HERE\"}}}" -Spath | tail -1 > "$CP_FILE"
fi
# some upstream tests use ./target and ./load-file-test relative to the working directory
WORK=/tmp/mova-client-gate  # short: Unix socket paths are limited to about 100 bytes
mkdir -p "$WORK/target"
[ -e "$WORK/load-file-test" ] || ln -s "$NREPL_SRC/load-file-test" "$WORK/load-file-test"
cd "$WORK"
# The gate starts `mova nrepl` servers as children of the JVM. Run it in its own
# process group and kill the group on exit, so no server outlives the run (an
# orphan keeps the stdout pipe open, which also made `run.sh | tail` hang).
set -m
java -Dclojure.main.report=stderr -cp "$(cat "$CP_FILE"):$NREPL_SRC/test/clojure" clojure.main -m gate "$@" &
PGID=$!
trap 'kill -TERM -$PGID 2>/dev/null' INT TERM
set +e
wait $PGID
RC=$?
kill -TERM -$PGID 2>/dev/null
exit $RC
