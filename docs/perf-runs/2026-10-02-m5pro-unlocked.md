## leal-perf

- Machine: Mac17,8, Apple M5 Pro (6 performance + 12 efficiency cores), 48 GB, macOS 27.0
- Power: Now drawing from 'AC Power'; powermode 0
- Load average: { 5.25 4.24 3.18 }; 1091 processes
- Screen: unlocked
- Displays: Display Type: Built-in Liquid Retina XDR Display; Resolution: 3456 x 2234 Retina; Main Display: Yes; Resolution: 2560 x 1440 (QHD/WQHD - Wide Quad High Definition); UI Looks like: 2560 x 1440 @ 60.00Hz

| Budget (DESIGN §1) | Measured on this Mac | How | Verdict |
|---|---|---|---|
| Launch to empty window < 300 ms | 159.3 ms (133.2–432.8, 3 runs) | process start to `applicationDidFinishLaunching` ("Launched" signpost); Leal opens no empty window | mixed |
| Open to first rows < 150 ms | 95.9 ms (95.4–114.7, 3 runs) | `read(from:)` to the grid's first draw with rows ("Open to first rows" signpost), reference file, the app launched with it: includes the process's first window | pass |
| … in a running app | 38.7 ms (33.7–46.9, 15 runs) | the same signpost when the file is closed and opened again in the running app (bench build, `-LealReopen`) | pass |
| Full index < 500 ms | 170.3 ms (163.0–214.9, 3 runs) | the core's "Index" signpost in the app, reference file, with diagnostics | pass |
| Scrolling: no dropped frames at 120 Hz | 4 of 7,554 late (0.05%); 3 of 7,552 late (0.04%); 6 of 7,549 late (0.08%) | `ScrollBench` flings, reference file, after indexing; late = missed a refresh | fail |
| … including while background work runs | 4 of 7,553 late (0.05%); 4 of 7,553 late (0.05%); 2 of 7,557 late (0.03%) | the same from the first rows, while the index and review run, with a search running throughout | fail |
| … stress: background work not pausing (beyond the budget) | 2,048 of 5,455 late (37.54%); 8 of 7,542 late (0.11%); 5 of 7,552 late (0.07%) | 1 GB variant from the first rows: index, review and a search all running, the scroll not reported as input | fail |
| Leal's heap, reference file < 40 MB | 29.7 MB (29.7–29.8, 3 runs) after opening; peak while scrolling 45.3 MB (39.7–52.9, 6 runs) | `heap -s` (all malloc zones) after the review finished; the bench's `malloc_zone_statistics` peak | fail |
| Idle app, no document < 30 MB resident | 17.1 MB (17.1–17.2, 3 runs) footprint; 80.2 MB (77.6–80.4, 3 runs) resident (RSS) | `heap -s` physical footprint (Activity Monitor's Memory) after the app settles; RSS also counts shared system libraries, so it is over 30 MB for any AppKit app | pending Rob: fail on resident size (RSS), as DESIGN §1 words it; pass on the proposed footprint reading |

Details:

- `open` to "Launched": 175.9 ms (144.0–453.5, 3 runs)
- Idle heap: 9.6 MB (9.6–9.6, 3 runs)
- With the file: launch to first rows 313.4 ms (312.3–355.1, 3 runs); the core's first paint 1.9 ms (1.8–1.9, 3 runs); footprint after opening 58.8 MB (58.7–58.9, 3 runs)
- Scroll `afterLoad` run 1: 4 of 7,554 late (0.05%), frame p99 8.3 ms, main-thread CPU p50/p99 2.9/5.8 ms, 12 frames busy over 8.3 ms, 18.5 M instructions a frame at 2.44 GHz; while indexing 0 of 0 late (0.00%); while searching 0 of 0 late (0.00%) (0 searches); heap peak 45.0 MB; screen 120 Hz
- Scroll `afterLoad` run 2: 3 of 7,552 late (0.04%), frame p99 8.3 ms, main-thread CPU p50/p99 2.8/6.4 ms, 27 frames busy over 8.3 ms, 13.0 M instructions a frame at 2.43 GHz; while indexing 0 of 0 late (0.00%); while searching 0 of 0 late (0.00%) (0 searches); heap peak 39.7 MB; screen 120 Hz
- Scroll `afterLoad` run 3: 6 of 7,549 late (0.08%), frame p99 8.3 ms, main-thread CPU p50/p99 3.6/6.8 ms, 40 frames busy over 8.3 ms, 18.3 M instructions a frame at 2.42 GHz; while indexing 0 of 0 late (0.00%); while searching 0 of 0 late (0.00%) (0 searches); heap peak 45.6 MB; screen 120 Hz
- Scroll `duringLoadWithFind` run 1: 4 of 7,553 late (0.05%), frame p99 8.3 ms, main-thread CPU p50/p99 2.9/6.7 ms, 32 frames busy over 8.3 ms, 13.8 M instructions a frame at 2.43 GHz; while indexing 2 of 8 late (25.00%); while searching 4 of 7,553 late (0.05%) (18 searches); heap peak 52.9 MB; screen 120 Hz
- Scroll `duringLoadWithFind` run 2: 4 of 7,553 late (0.05%), frame p99 8.3 ms, main-thread CPU p50/p99 3.1/7.0 ms, 42 frames busy over 8.3 ms, 14.7 M instructions a frame at 2.24 GHz; while indexing 0 of 3 late (0.00%); while searching 4 of 7,553 late (0.05%) (18 searches); heap peak 40.9 MB; screen 120 Hz
- Scroll `duringLoadWithFind` run 3: 2 of 7,557 late (0.03%), frame p99 8.3 ms, main-thread CPU p50/p99 2.7/6.5 ms, 24 frames busy over 8.3 ms, 14.3 M instructions a frame at 2.29 GHz; while indexing 2 of 4 late (50.00%); while searching 2 of 7,557 late (0.03%) (18 searches); heap peak 48.5 MB; screen 120 Hz
- Scroll `bigFileNoPause` run 1: 2,048 of 5,455 late (37.54%), frame p99 16.7 ms, main-thread CPU p50/p99 1.5/4.7 ms, 26 frames busy over 8.3 ms, 16.1 M instructions a frame at 4.23 GHz; while indexing 0 of 171 late (0.00%); while searching 2,037 of 5,436 late (37.47%) (22 searches); heap peak 273.7 MB; screen 120 Hz
- Scroll `bigFileNoPause` run 2: 8 of 7,542 late (0.11%), frame p99 8.3 ms, main-thread CPU p50/p99 1.5/3.5 ms, 15 frames busy over 8.3 ms, 18.1 M instructions a frame at 4.25 GHz; while indexing 1 of 137 late (0.73%); while searching 8 of 7,521 late (0.11%) (24 searches); heap peak 263.5 MB; screen 120 Hz
- Scroll `bigFileNoPause` run 3: 5 of 7,552 late (0.07%), frame p99 8.3 ms, main-thread CPU p50/p99 1.4/3.3 ms, 10 frames busy over 8.3 ms, 18.4 M instructions a frame at 4.25 GHz; while indexing 1 of 133 late (0.75%); while searching 5 of 7,530 late (0.07%) (24 searches); heap peak 263.1 MB; screen 120 Hz
