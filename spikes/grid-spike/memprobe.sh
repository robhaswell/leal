#!/bin/bash
# Cross-check the in-app phys_footprint numbers with footprint(1) and vmmap(1):
# run one bench, keep the app open, inspect it, then quit it.
#   ./memprobe.sh IMPL COLS [extra GridSpike args]
# IMPL is table, table-lite, table-rowdraw or custom.
set -euo pipefail
cd "$(dirname "$0")"
IMPL=$1 COLS=$2; shift 2
APP=build/GridSpike.app/Contents/MacOS/GridSpike
mkdir -p runs
OUT="runs/probe-$IMPL-$COLS"
caffeinate -d -w $$ &
(while kill -0 $$ 2>/dev/null; do caffeinate -u -t 20; done) &  # display on (see bench.sh)
"$APP" --impl "$IMPL" --cols "$COLS" --bench --hold --out "$PWD/$OUT.json" "$@" >"$OUT.log" 2>&1 &
PID=$!
until grep -q holding "$OUT.log" 2>/dev/null; do
  kill -0 "$PID" 2>/dev/null || { echo "app exited early"; cat "$OUT.log"; exit 1; }
  sleep 1
done
footprint "$PID" >"$OUT.footprint.txt" 2>&1 || true
vmmap --summary "$PID" >"$OUT.vmmap.txt" 2>&1 || true
kill "$PID"
wait "$PID" 2>/dev/null || true
grep -E 'phys_footprint|Footprint' "$OUT.footprint.txt" | head -3
python3 -c "import json;d=json.load(open('$OUT.json'));print('in-app after scroll MB', round(d['footprintAfterScrollMB'],1), 'peak', round(d['footprintPeakMB'],1))"
