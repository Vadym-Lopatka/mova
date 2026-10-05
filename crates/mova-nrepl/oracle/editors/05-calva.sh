#!/usr/bin/env bash
# Client 5: Calva cannot run headless. calva-replay.py replays the op sequence of Calva's src/nrepl/index.ts
# (connect handshake, eval with ns/line/column/file + pprint options, load-file, interrupt, stdin, close).
. "$(dirname "$0")/lib.sh"
for kind in mova jvm; do
  start_server $kind || exit 1
  python3 "$ED_DIR/calva-replay.py" "$PORT" "$WORK/calva-$kind.wire" "$SRV_DIR" > "$WORK/calva-$kind.out" 2>&1
  if [ $kind = mova ]; then grep '^RESULT' "$WORK/calva-mova.out" || { result calva connect FAIL "replay failed"; cat "$WORK/calva-mova.out"; }
  else sed 's/^RESULT/JVMREF/' "$WORK/calva-jvm.out" | grep '^JVMREF'; fi
done
echo "--- wire diff Mova vs JVM"; python3 "$ED_DIR/wirediff.py" "$WORK/calva-mova.wire" "$WORK/calva-jvm.wire"
grep '^INFO' "$WORK/calva-mova.out"
