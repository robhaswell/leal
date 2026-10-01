#!/bin/bash
# Run the grid spike measurements.
#   ./bench.sh compare         quick headline check (C vs B at 200 columns, and
#                              A for contrast), one run each, about 3 minutes
#   ./bench.sh snap            first-screen and last-rows snapshots into runs/
#   ./bench.sh run [RUNS]      RUNS (default 3) runs per configuration, then
#                              aggregate into results.md (about 2 hours)
#   ./bench.sh one IMPL COLS [args]   a single run, JSON to stdout
# Variants: A (--impl table), A-lite (table-lite), C (table-rowdraw), B (custom).
# Configurations run interleaved (A12, A-lite12, C12, B12, A50, ...) so drift in
# machine state affects every variant equally.
set -euo pipefail
cd "$(dirname "$0")"
APP=build/GridSpike.app/Contents/MacOS/GridSpike
mkdir -p runs
# Keep the display awake for the whole script: the display link stops when the
# display sleeps, and a locked Mac turns its display off after about a minute
# even with caffeinate -d. So also keep refreshing the "user is active"
# assertion (caffeinate -u). These are power assertions, not input events.
caffeinate -d -w $$ &
(while kill -0 $$ 2>/dev/null; do caffeinate -u -t 20; done) &

impl_for() {
  case $1 in
    A) echo table ;;
    A-lite) echo table-lite ;;
    C) echo table-rowdraw ;;
    B) echo custom ;;
  esac
}

# Wake the display if it has turned off (a locked Mac turns it off after about
# a minute even with caffeinate -d), then note what else is using the CPU.
prepare() {
  caffeinate -u -t 1
  echo "  load: $(sysctl -n vm.loadavg) top: $(ps -Ao pcpu=,comm= -r | head -3 | awk '{printf "%s %s; ", $1, $NF}')"
}

case "${1:-run}" in
compare)
  for v in C B A; do
    prepare
    timeout 900 "$APP" --impl "$(impl_for $v)" --cols 200 --speed 60000 --fling-rows 20000 --bench \
      --out "$PWD/runs/compare-$v.json" 2>/dev/null
  done
  python3 - <<'PY'
import json
print(f"{'variant':8} {'late %':>7} {'p99 ms':>7} {'busy p50':>9} {'busy p99':>9} {'heap MB':>8}")
for v in ["A", "C", "B"]:
    r = json.load(open(f"runs/compare-{v}.json")); s = r["scroll"]; b = r["busy"]
    print(f"{v:8} {100 * s['over8'] / max(1, s['frames']):7.1f} {s['p99']:7.1f} {b['p50']:9.1f} {b['p99']:9.1f} {r['heapAfterScrollMB']:8.1f}")
print("Late % = frames that missed a 120 Hz refresh; busy = main-thread ms per frame (budget 8.3).")
PY
  ;;
snap)
  for cols in 12 200; do
    for v in A A-lite C B; do
      prepare >/dev/null
      "$APP" --impl "$(impl_for $v)" --cols "$cols" --snapshot "$PWD/runs/snap-$v-$cols.png" 2>/dev/null
      "$APP" --impl "$(impl_for $v)" --cols "$cols" --snapshot-end "$PWD/runs/snap-$v-$cols-end.png" 2>/dev/null
    done
  done
  ;;
one)
  prepare >&2
  "$APP" --impl "$2" --cols "$3" --bench "${@:4}"
  ;;
run)
  # Profiles: fast = 60,000 pt/s flings over 50,000 rows; moderate = 15,000 pt/s
  # over 10,000 rows.
  RUNS=${2:-3}
  rm -f runs/run-*.json
  for i in $(seq 1 "$RUNS"); do
    for prof in fast moderate; do
      if [ "$prof" = fast ]; then P="--speed 60000 --fling-rows 50000"; else P="--speed 15000 --fling-rows 10000"; fi
      for cols in 12 50 200; do
        for v in A A-lite C B; do
          echo "$(date +%H:%M:%S) run $i: $prof $v $cols"
          prepare
          # shellcheck disable=SC2086
          timeout 900 "$APP" --impl "$(impl_for $v)" --cols "$cols" $P --bench \
            --out "$PWD/runs/run-$v-$cols-$prof-$i.json" 2>/dev/null || echo "  run failed or timed out"
          sleep 2
        done
      done
    done
  done
  python3 aggregate.py runs > results.md
  echo "wrote results.md"
  ;;
esac
