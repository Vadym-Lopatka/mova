#!/usr/bin/env bash
# Client 4: Conjure in headless Neovim (private config/data dirs); Mova, then JVM for a wire diff.
# Conjure is cloned once into $MOVA_ED_NVIM (default $TMPDIR/mova-editors-nvim).
. "$(dirname "$0")/lib.sh"
STEPS="connect eval print error multi load complete doc interrupt stdin close"
command -v nvim >/dev/null || { for s in $STEPS; do result conjure $s SKIP "no nvim"; done; exit 0; }
NV="${MOVA_ED_NVIM:-${TMPDIR:-/tmp}/mova-editors-nvim}"
CJ="$NV/data/site/pack/p/start/conjure"
if [ ! -d "$CJ" ]; then
  mkdir -p "$NV/data/site/pack/p/start" "$NV/config" "$NV/state" "$NV/cache"
  git clone -q --depth 1 https://github.com/Olical/conjure "$CJ" 2>"$WORK/clone.log" || { rm -rf "$CJ"; for s in $STEPS; do result conjure $s SKIP "conjure clone failed (no network?)"; done; exit 0; }
fi
run_one() {
  local kind=$1
  start_server "$kind" || return 1
  local pp; pp="$(free_port)"
  python3 "$ED_DIR/proxy.py" "$pp" "$PORT" "$WORK/conjure-$kind.wire" & PIDS+=($!)
  sleep 0.5
  ( cd "$SRV_DIR"; CONJURE_DIR="$CJ" NREPL_PORT=$pp WORK_DIR="$SRV_DIR" XDG_CONFIG_HOME="$NV/config" XDG_DATA_HOME="$NV/data" XDG_STATE_HOME="$NV/state" XDG_CACHE_HOME="$NV/cache" \
      timeout 90 nvim --headless -u NONE -l "$ED_DIR/conjure-drive.lua" >"$WORK/conjure-$kind.out" 2>&1 )
}
run_one mova; grep '^RESULT' "$WORK/conjure-mova.out" || result conjure connect FAIL "nvim produced no result, see $WORK/conjure-mova.out"
if [ "${WIRE_DIFF:-1}" = 1 ]; then
  run_one jvm
  echo "--- Conjure on JVM nREPL (reference)"; grep '^RESULT' "$WORK/conjure-jvm.out" | sed 's/^RESULT/JVMREF/'
  echo "--- wire diff Mova vs JVM"; python3 "$ED_DIR/wirediff.py" "$WORK/conjure-mova.wire" "$WORK/conjure-jvm.wire"
  [ -n "${KEEP_WORK:-}" ] && echo "kept $WORK"
fi
