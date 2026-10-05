#!/bin/bash
# One-command soak for `mova nrepl`. Usage: tools/soak.sh [mova-binary] [redefn|unique|both]
# 10 sessions, 100k evals; footprint (phys_footprint) every 10k; PASS = last <= 50 MB and
# last-third slope <= 0.5 MB per 10k evals. SOAK_N=20000 for a short run.
M=${1:-$PWD/target/release/mova}
P=$PWD/tools/p2-measure/target/release/p2-measure
(cd tools/p2-measure && cargo build --release 2>&1 | tail -1)
echo "load: $(uptime | sed 's/.*load/load/')"
$P soak "$M" "${2:-both}"
rc=$?
pkill -f "$M nrepl" 2>/dev/null
exit $rc
