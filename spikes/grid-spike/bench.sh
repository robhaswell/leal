#!/bin/bash
# Run the grid spike measurements.
#   ./bench.sh snap            window snapshots of A and B at each width
#   ./bench.sh run [RUNS]      RUNS (default 3) scripted-scroll runs per config,
#                              then aggregate into results.md
#   ./bench.sh one IMPL COLS   a single run, JSON to stdout
# Configurations run interleaved (A12, B12, A50, …) so drift in machine state
# affects both options equally.
set -euo pipefail
cd "$(dirname "$0")"
APP=build/GridSpike.app/Contents/MacOS/GridSpike
mkdir -p runs
# Keep the display awake; the display link stops when it sleeps.
caffeinate -d -w $$ &

case "${1:-run}" in
snap)
  # First screen, and the last rows (checks drawing at a 22M pt offset).
  for cols in 12 200; do
    for v in A A-lite B; do
      case $v in
        A) V="--impl table" ;;
        A-lite) V="--impl table --lite" ;;
        B) V="--impl custom" ;;
      esac
      # shellcheck disable=SC2086
      "$APP" $V --cols "$cols" --snapshot "$PWD/runs/snap-$v-$cols.png" 2>/dev/null
      # shellcheck disable=SC2086
      "$APP" $V --cols "$cols" --snapshot-end "$PWD/runs/snap-$v-$cols-end.png" 2>/dev/null
    done
  done
  ;;
one)
  "$APP" --impl "$2" --cols "$3" --bench "${@:4}"
  ;;
run)
  # Variants: A (NSTableView + NSTextField cells), A-lite (NSTableView with
  # self-drawing cells), B (custom grid). Profiles: fast = 60,000 pt/s flings
  # over 50,000 rows; moderate = 15,000 pt/s over 10,000 rows.
  RUNS=${2:-3}
  rm -f runs/run-*.json
  for i in $(seq 1 "$RUNS"); do
    for prof in fast moderate; do
      if [ "$prof" = fast ]; then P="--speed 60000 --fling-rows 50000"; else P="--speed 15000 --fling-rows 10000"; fi
      for cols in 12 50 200; do
        for v in A A-lite B; do
          case $v in
            A) V="--impl table" ;;
            A-lite) V="--impl table --lite" ;;
            B) V="--impl custom" ;;
          esac
          echo "$(date +%H:%M:%S) run $i: $prof $v $cols"
          # shellcheck disable=SC2086
          timeout 900 "$APP" $V --cols "$cols" $P --bench --out "$PWD/runs/run-$v-$cols-$prof-$i.json" 2>/dev/null \
            || echo "  run failed or timed out"
          sleep 2
        done
      done
    done
  done
  python3 aggregate.py runs > results.md
  echo "wrote results.md"
  ;;
esac
