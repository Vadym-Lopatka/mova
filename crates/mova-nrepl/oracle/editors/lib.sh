# Shared helpers. Source this file. Output lines: "RESULT <client> <step> PASS|FAIL|SKIP <detail>"
ED_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ORACLE="$(cd "$ED_DIR/.." && pwd)"
REPO="$(cd "$ORACLE/../../.." && pwd)"
MOVA="${MOVA:-$REPO/target/release/mova}"
CP_FILE="$ORACLE/classpath.txt"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/mova-editors.XXXXXX")"
PIDS=()

free_port() { python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close()'; }

wait_port() { # port
  for _ in $(seq 1 100); do
    python3 -c "import socket,sys;socket.create_connection(('127.0.0.1',$1),0.2)" 2>/dev/null && return 0
    sleep 0.2
  done
  return 1
}

# start_server mova|jvm [extra args] -> sets PORT, SRV_DIR
start_server() {
  local kind="$1"; shift
  PORT="$(free_port)"
  SRV_DIR="$(mktemp -d "$WORK/srv.XXXXXX")"
  ( cd "$SRV_DIR" &&
    if [ "$kind" = mova ]; then exec "$MOVA" nrepl -p "$PORT" "$@" >server.log 2>&1
    else exec java -cp "$(cat "$CP_FILE")" clojure.main -m nrepl.cmdline -p "$PORT" "$@" >server.log 2>&1; fi ) &
  PIDS+=($!)
  LAST_PID=$!
  wait_port "$PORT" || { echo "server did not start" >&2; return 1; }
}

cleanup() {
  for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null; done
  sleep 0.2
  for p in "${PIDS[@]}"; do kill -9 "$p" 2>/dev/null; done
  [ -n "${KEEP_WORK:-}" ] || rm -rf "$WORK"
}
trap cleanup EXIT

# step CLIENT STEP "label" condition-exit-code detail
result() { echo "RESULT $1 $2 $3 ${4:-}"; }
# check CLIENT STEP "text" "needle"   -> PASS if text contains needle
check() {
  if printf '%s' "$3" | grep -qF -- "$4"; then result "$1" "$2" PASS
  else result "$1" "$2" FAIL "expected [$4] got [$(printf '%s' "$3" | tr '\n' '|' | cut -c1-200)]"; fi
}
