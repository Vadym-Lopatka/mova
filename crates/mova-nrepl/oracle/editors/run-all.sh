#!/usr/bin/env bash
# Run every editor script against `mova nrepl` (release binary) and print PASS/FAIL/SKIP per client and step.
# Env: MOVA (binary), WIRE_DIFF=0 to skip the JVM comparison runs, MOVA_ED_PKG / MOVA_ED_NVIM (private install dirs,
# created on first run: CIDER from MELPA, Conjure from GitHub; needs network once).
cd "$(dirname "$0")"
out=$(mktemp); rc=0
for s in 0[1-5]-*.sh; do
  echo "=== $s"; ./$s 2>&1 | grep -v "Terminated" | tee -a "$out"
done
echo; echo "=== SUMMARY (Mova server)"
grep '^RESULT' "$out" | awk '{c=$2; s=$3; st=$4; $1=$2=$3=$4=""; printf "%-14s %-10s %-5s %s\n", c, s, st, substr($0,1,110)}'
echo; printf "PASS=%s FAIL=%s SKIP=%s\n" "$(grep -c '^RESULT.* PASS' "$out")" "$(grep -c '^RESULT.* FAIL' "$out")" "$(grep -c '^RESULT.* SKIP' "$out")"
grep -q '^RESULT.* FAIL' "$out" && rc=1
rm -f "$out"; exit $rc
