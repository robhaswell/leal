## leal-perf (task 2.0a, branch task/2.0a)

- Machine: Mac17,8, Apple M5 Pro (6 performance + 12 efficiency cores), 48 GB, macOS 27.0
- Power: Now drawing from 'AC Power'; powermode 0
- Load average: { 8.96 15.58 14.87 }; 1085 processes
- Screen: unlocked
- Displays: Display Type: Built-in Liquid Retina XDR Display; Resolution: 3456 x 2234 Retina; Main Display: Yes; Resolution: 2560 x 1440 (QHD/WQHD - Wide Quad High Definition); UI Looks like: 2560 x 1440 @ 60.00Hz

| Budget (DESIGN §1) | Measured on this Mac | How | Verdict |
|---|---|---|---|
| Launch < 300 ms | 273.8 ms (196.2–493.8, 3 runs) | process start to the end of `applicationDidFinishLaunching` ("Launched" signpost); Leal opens no empty window | mixed |
| Launched with a file, to its first rows < 450 ms | 377.0 ms (365.6–385.1, 3 runs) | process start to the grid's first draw with rows, reference file: the cold open, judged as part of launch against the launch and open budgets together | pass |
| Open to first rows < 150 ms | 46.4 ms (39.5–57.8, 15 runs) | `read(from:)` to the grid's first draw with rows ("Open to first rows" signpost), reference file, closed and opened again in the running app: the warm open (bench build, `-LealReopen`) | pass |
| Full index < 500 ms | 166.0 ms (165.0–173.3, 3 runs) | the core's "Index" signpost in the app, reference file, with diagnostics | pass |
| Scrolling: no dropped frames at 120 Hz | 1 of 7,558 late (0.01%); 2 of 7,558 late (0.03%); 0 of 7,560 late (0.00%); main-thread work per frame p50 1.4–1.5 ms, p99 2.2–2.4 ms | `ScrollBench` flings, reference file, after indexing; late = missed a refresh; 3× rule (DESIGN §1): main-thread work per frame ≤ 2.8 ms at p50 and p99 | fail (late frames) |
| … including while background work runs | 0 of 7,557 late (0.00%); 1 of 7,557 late (0.01%); 0 of 7,557 late (0.00%); main-thread work per frame p50 1.5–1.6 ms, p99 2.5–2.6 ms | the same from the first rows, while the index and review run, with a search running throughout; 3× rule (DESIGN §1): main-thread work per frame ≤ 2.8 ms at p50 and p99 | fail (late frames) |
| … stress: background work not pausing (beyond the budget) | 0 of 7,557 late (0.00%); 2 of 7,557 late (0.03%); 4 of 7,555 late (0.05%); main-thread work per frame p50 0.8–0.9 ms, p99 1.7–1.9 ms | 1 GB variant from the first rows: index, review and a search all running, the scroll not reported as input; judged on late frames only | fail |
| Leal's heap, reference file < 40 MB | 9.0 MB (9.0–9.0, 3 runs) after opening; settled after scrolling, 10.8 MB (9.1–11.4, 3 runs) without a search and 19.7 MB (10.6–20.4, 3 runs) with a search's results held. Not judged, all zones: 30.0 MB (30.0–30.0, 3 runs) after opening; peak while scrolling 37.9 MB (34.3–42.2, 6 runs) | all malloc zones minus the per-window AppKit baseline (21 MB, a two-row file open: docs/tasks/1.10.md): `heap -s` after the review finished, and the bench's `malloc_zone_statistics` once settled after scrolling; AppKit's drawing peaks are left out | pass |
| Idle app, no document < 30 MB footprint | 17.3 MB (17.2–17.8, 3 runs) footprint. Not judged: resident size 77.6 MB (77.4–80.9, 3 runs) | `heap -s` physical footprint (Activity Monitor's Memory) after the app settles; the resident size (RSS) also counts shared system libraries | pass |

Details:

- `open` to "Launched": 301.2 ms (227.1–527.4, 3 runs)
- Idle heap: 9.6 MB (9.6–9.6, 3 runs)
- With the file: the cold open (`read(from:)` to first rows, the app launched with it) 101.8 ms (101.1–111.6, 3 runs); the core's first paint 1.9 ms (1.8–2.0, 3 runs); footprint after opening 59.3 MB (58.9–59.4, 3 runs)
- Scroll `afterLoad` run 1: 1 of 7,558 late (0.01%), frame p99 8.3 ms, main-thread CPU p50/p99 1.5/2.4 ms, 0 frames busy over 8.3 ms, 5.2 M instructions a frame at 2.66 GHz; while indexing 0 of 0 late (0.00%); while searching 0 of 0 late (0.00%) (0 searches); heap peak 34.4 MB; screen 120 Hz
- Scroll `afterLoad` run 2: 2 of 7,558 late (0.03%), frame p99 8.3 ms, main-thread CPU p50/p99 1.5/2.4 ms, 1 frames busy over 8.3 ms, 5.2 M instructions a frame at 2.64 GHz; while indexing 0 of 0 late (0.00%); while searching 0 of 0 late (0.00%) (0 searches); heap peak 34.6 MB; screen 120 Hz
- Scroll `afterLoad` run 3: 0 of 7,560 late (0.00%), frame p99 8.3 ms, main-thread CPU p50/p99 1.4/2.2 ms, 0 frames busy over 8.3 ms, 5.3 M instructions a frame at 3.06 GHz; while indexing 0 of 0 late (0.00%); while searching 0 of 0 late (0.00%) (0 searches); heap peak 34.3 MB; screen 120 Hz
- Scroll `duringLoadWithFind` run 1: 0 of 7,557 late (0.00%), frame p99 8.3 ms, main-thread CPU p50/p99 1.5/2.6 ms, 1 frames busy over 8.3 ms, 6.0 M instructions a frame at 2.90 GHz; while indexing 0 of 3 late (0.00%); while searching 0 of 7,557 late (0.00%) (18 searches); heap peak 41.4 MB; screen 120 Hz
- Scroll `duringLoadWithFind` run 2: 1 of 7,557 late (0.01%), frame p99 8.3 ms, main-thread CPU p50/p99 1.5/2.5 ms, 2 frames busy over 8.3 ms, 6.0 M instructions a frame at 2.98 GHz; while indexing 1 of 2 late (50.00%); while searching 1 of 7,557 late (0.01%) (18 searches); heap peak 42.2 MB; screen 120 Hz
- Scroll `duringLoadWithFind` run 3: 0 of 7,557 late (0.00%), frame p99 8.3 ms, main-thread CPU p50/p99 1.6/2.6 ms, 0 frames busy over 8.3 ms, 6.0 M instructions a frame at 2.73 GHz; while indexing 0 of 2 late (0.00%); while searching 0 of 7,557 late (0.00%) (14 searches); heap peak 41.3 MB; screen 120 Hz
- Scroll `bigFileNoPause` run 1: 0 of 7,557 late (0.00%), frame p99 8.3 ms, main-thread CPU p50/p99 0.8/1.7 ms, 1 frames busy over 8.3 ms, 5.1 M instructions a frame at 4.15 GHz; while indexing 0 of 156 late (0.00%); while searching 0 of 7,548 late (0.00%) (11 searches); heap peak 267.2 MB; screen 120 Hz
- Scroll `bigFileNoPause` run 2: 2 of 7,557 late (0.03%), frame p99 8.3 ms, main-thread CPU p50/p99 0.9/1.9 ms, 3 frames busy over 8.3 ms, 5.1 M instructions a frame at 3.72 GHz; while indexing 2 of 138 late (1.45%); while searching 2 of 7,548 late (0.03%) (11 searches); heap peak 267.8 MB; screen 120 Hz
- Scroll `bigFileNoPause` run 3: 4 of 7,555 late (0.05%), frame p99 8.3 ms, main-thread CPU p50/p99 0.9/1.9 ms, 3 frames busy over 8.3 ms, 5.1 M instructions a frame at 3.48 GHz; while indexing 4 of 137 late (2.92%); while searching 4 of 7,541 late (0.05%) (16 searches); heap peak 267.1 MB; screen 120 Hz
