#!/usr/bin/env bash
# Client 3: CIDER in batch Emacs against Mova, then the same script against a JVM nREPL; wire diff.
# CIDER is installed once into a private package dir (MOVA_ED_PKG, default $TMPDIR/mova-editors-elpa).
. "$(dirname "$0")/lib.sh"
command -v emacs >/dev/null || { for s in connect eval print error multi load complete doc interrupt stdin close; do result cider $s SKIP "no emacs"; done; exit 0; }
export MOVA_ED_PKG="${MOVA_ED_PKG:-${TMPDIR:-/tmp}/mova-editors-elpa}"
if [ ! -d "$MOVA_ED_PKG/archives" ]; then
  emacs --batch -Q -l "$ED_DIR/cider-install.el" >"$WORK/install.log" 2>&1 || { for s in connect eval print error multi load complete doc interrupt stdin close; do result cider $s SKIP "CIDER install failed (no network?)"; done; exit 0; }
fi
run_one() { # kind -> $WORK/cider-$kind.{out,wire}
  local kind=$1
  start_server "$kind" || return 1
  local pp; pp="$(free_port)"
  python3 "$ED_DIR/proxy.py" "$pp" "$PORT" "$WORK/cider-$kind.wire" & PIDS+=($!)
  sleep 0.5
  ( cd "$SRV_DIR"; NREPL_PORT=$pp WORK_DIR="$SRV_DIR" timeout 150 emacs --batch -Q -l "$ED_DIR/cider-drive.el" >"$WORK/cider-$kind.out" 2>&1 )
}
run_one mova; grep '^RESULT' "$WORK/cider-mova.out" || result cider connect FAIL "emacs produced no result, see $WORK/cider-mova.out"
if [ "${WIRE_DIFF:-1}" = 1 ]; then
  run_one jvm
  echo "--- CIDER on JVM nREPL (reference)"; grep '^RESULT' "$WORK/cider-jvm.out" | sed 's/^RESULT/JVMREF/'
  echo "--- messages CIDER printed (Mova)"; grep -E "^\[nREPL\]|^Error|^Warning \(cider|requires the nREPL op|MSGS" "$WORK/cider-mova.out" | cut -c1-600
  echo "--- wire diff Mova vs JVM"; python3 "$ED_DIR/wirediff.py" "$WORK/cider-mova.wire" "$WORK/cider-jvm.wire"
  [ -n "${KEEP_WORK:-}" ] && echo "kept $WORK"
fi
