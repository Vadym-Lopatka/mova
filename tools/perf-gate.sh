#!/bin/bash
# One-command perf gate for `mova nrepl`. Usage: tools/perf-gate.sh [mova-binary]
# Prints load, 1M println msg/s (top-level loop) and warm eval round trip.
# Targets: println >= 3.3M msg/s; warm eval p50 <= 30us. (session KB, soak: round 2.)
M=${1:-$PWD/target/release/mova}
P=$PWD/tools/p2-measure/target/release/p2-measure
[ -x "$P" ] || (cd tools/p2-measure && cargo build --release 2>&1 | tail -1)
echo "load: $(uptime | sed 's/.*load/load/')"
$P println 1000000 "$M" | head -3
$P warm 2000 "$M" | head -3
pkill -f "$M nrepl" 2>/dev/null; true
