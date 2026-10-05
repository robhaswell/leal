# Rob's unlocked run, the phase 2 gate, 2026-10-05

`just perf` on `main` at 5b5e534, Rob's M5 Pro, screen unlocked, with the
other agents paused. Read with the table below:

- **Scrolling passes the 3× rule:** main-thread work per frame p50
  1.1–1.2 ms, p99 2.1–2.3 ms, after indexing and with a search running.
- **Late frames:** none after indexing. With a search running
  (`duringLoadWithFind`), 3 of 7,551 (0.04%) in one run, 0 and 1 in the
  others. That fails "no dropped frames" as written, but passes the
  proposed 0.05% tolerance Rob decides at the phase 2 gate. The stress
  runs (beyond the budget) had 7, 2 and 3.
- **Launch "mixed":** one launch of 371 ms, the known first launch after
  a build, which macOS scans first (docs/perf.md, "The slow first
  launch"); median 184.5 ms.
- **Cell edit to screen:** median 6.0 ms, max 18.1 ms over 60 edits, so
  "mixed": the slowest edits widen their column, which redraws every
  strip.

## leal-perf

- Machine: Mac17,8, Apple M5 Pro (6 performance + 12 efficiency cores), 48 GB, macOS 27.0
- Power: Now drawing from 'AC Power'; powermode 0
- Load average: { 6.83 4.23 7.76 }; 990 processes
- Screen: unlocked
- Displays: Display Type: Built-in Liquid Retina XDR Display; Resolution: 3456 x 2234 Retina; Main Display: Yes; Resolution: 2560 x 1440 (QHD/WQHD - Wide Quad High Definition); UI Looks like: 2560 x 1440 @ 60.00Hz

| Budget (DESIGN §1) | Measured on this Mac | How | Verdict |
|---|---|---|---|
| Launch < 300 ms | 184.5 ms (156.9–371.1, 3 runs) | process start to the end of `applicationDidFinishLaunching` ("Launched" signpost); Leal opens no empty window | mixed |
| Launched with a file, to its first rows < 450 ms | 367.7 ms (361.7–401.4, 3 runs) | process start to the grid's first draw with rows, reference file: the cold open, judged as part of launch against the launch and open budgets together | pass |
| Open to first rows < 150 ms | 52.5 ms (45.9–57.0, 15 runs) | `read(from:)` to the grid's first draw with rows ("Open to first rows" signpost), reference file, closed and opened again in the running app: the warm open (bench build, `-LealReopen`) | pass |
| Full index < 500 ms | 160.1 ms (157.6–165.7, 3 runs) | the core's "Index" signpost in the app, reference file, with diagnostics | pass |
| Cell edit to screen < 16 ms | 6.0 ms (2.3–18.1, 60 edits) | Return in the in-cell editor to the commit of the transaction that draws the edit ("Cell edit to screen" signpost), reference file, after indexing, 20 edits a run (bench build, `-LealBenchEdit`) | mixed |
| Scrolling: no dropped frames at 120 Hz | 0 of 7,560 late (0.00%); 0 of 7,560 late (0.00%); 0 of 7,560 late (0.00%); main-thread work per frame p50 1.2 ms, p99 2.1 ms | `ScrollBench` flings, reference file, after indexing; late = missed a refresh; 3× rule (DESIGN §1): main-thread work per frame ≤ 2.8 ms at p50 and p99 | pass |
| … including while background work runs | 3 of 7,551 late (0.04%); 0 of 7,555 late (0.00%); 1 of 7,555 late (0.01%); main-thread work per frame p50 1.1 ms, p99 2.3 ms | the same from the first rows, while the index and review run, with a search running throughout; 3× rule (DESIGN §1): main-thread work per frame ≤ 2.8 ms at p50 and p99 | fail (late frames) |
| … stress: background work not pausing (beyond the budget) | 7 of 7,546 late (0.09%); 2 of 7,555 late (0.03%); 3 of 7,556 late (0.04%); main-thread work per frame p50 0.4 ms, p99 1.6 ms | 1 GB variant from the first rows: index, review and a search all running, the scroll not reported as input; judged on late frames only | fail |
| Leal's heap, reference file < 40 MB | 0.7 MB (0.7–0.9, 3 runs) after opening; settled after scrolling, 7.8 MB (7.8–7.9, 3 runs) without a search and 20.0 MB (10.3–21.1, 3 runs) with a search's results held. Not judged, all zones: 21.7 MB (21.7–21.9, 3 runs) after opening; peak while scrolling 36.2 MB (31.0–42.2, 6 runs) | all malloc zones minus the per-window AppKit baseline (21 MB, a two-row file open: docs/tasks/1.10.md): `heap -s` after the review finished, and the bench's `malloc_zone_statistics` once settled after scrolling; AppKit's drawing peaks are left out | pass |
| Idle app, no document < 30 MB footprint | 17.5 MB (17.3–17.5, 3 runs) footprint. Not judged: resident size 81.4 MB (78.0–81.5, 3 runs) | `heap -s` physical footprint (Activity Monitor's Memory) after the app settles; the resident size (RSS) also counts shared system libraries | pass |

Details:

- `open` to "Launched": 202.2 ms (175.9–397.8, 3 runs)
- Idle heap: 9.6 MB (9.5–9.6, 3 runs)
- With the file: the cold open (`read(from:)` to first rows, the app launched with it) 104.2 ms (97.8–134.3, 3 runs); the core's first paint 2.0 ms (1.7–2.0, 3 runs); footprint after opening 76.3 MB (76.0–76.5, 3 runs)
- Scroll `afterLoad` run 1: 0 of 7,560 late (0.00%), frame p99 8.3 ms, main-thread CPU p50/p99 1.2/2.1 ms, 0 frames busy over 8.3 ms, 2.8 M instructions a frame at 2.02 GHz; other threads 1.85 ms and WindowServer 4.08 ms a frame; while indexing 0 of 0 late (0.00%); while searching 0 of 0 late (0.00%) (0 searches); heap peak 31.2 MB, footprint peak 224 MB; drawn by strips; screen 120 Hz
- Scroll `afterLoad` run 2: 0 of 7,560 late (0.00%), frame p99 8.3 ms, main-thread CPU p50/p99 1.2/2.1 ms, 0 frames busy over 8.3 ms, 2.8 M instructions a frame at 1.97 GHz; other threads 1.86 ms and WindowServer 4.09 ms a frame; while indexing 0 of 0 late (0.00%); while searching 0 of 0 late (0.00%) (0 searches); heap peak 31.1 MB, footprint peak 236 MB; drawn by strips; screen 120 Hz
- Scroll `afterLoad` run 3: 0 of 7,560 late (0.00%), frame p99 8.3 ms, main-thread CPU p50/p99 1.2/2.1 ms, 0 frames busy over 8.3 ms, 2.8 M instructions a frame at 1.98 GHz; other threads 1.85 ms and WindowServer 4.09 ms a frame; while indexing 0 of 0 late (0.00%); while searching 0 of 0 late (0.00%) (0 searches); heap peak 31.0 MB, footprint peak 224 MB; drawn by strips; screen 120 Hz
- Scroll `duringLoadWithFind` run 1: 3 of 7,551 late (0.04%), frame p99 8.3 ms, main-thread CPU p50/p99 1.1/2.3 ms, 2 frames busy over 8.3 ms, 3.5 M instructions a frame at 2.09 GHz; other threads 2.11 ms and WindowServer 4.08 ms a frame; while indexing 1 of 6 late (16.67%); while searching 3 of 7,551 late (0.04%) (20 searches); heap peak 41.9 MB, footprint peak 251 MB; drawn by strips; screen 120 Hz
- Scroll `duringLoadWithFind` run 2: 0 of 7,555 late (0.00%), frame p99 8.3 ms, main-thread CPU p50/p99 1.1/2.3 ms, 2 frames busy over 8.3 ms, 3.5 M instructions a frame at 2.13 GHz; other threads 2.12 ms and WindowServer 4.09 ms a frame; while indexing 0 of 7 late (0.00%); while searching 0 of 7,555 late (0.00%) (19 searches); heap peak 42.2 MB, footprint peak 255 MB; drawn by strips; screen 120 Hz
- Scroll `duringLoadWithFind` run 3: 1 of 7,555 late (0.01%), frame p99 8.3 ms, main-thread CPU p50/p99 1.1/2.3 ms, 1 frames busy over 8.3 ms, 3.4 M instructions a frame at 2.10 GHz; other threads 2.11 ms and WindowServer 4.09 ms a frame; while indexing 0 of 3 late (0.00%); while searching 1 of 7,555 late (0.01%) (19 searches); heap peak 41.2 MB, footprint peak 241 MB; drawn by strips; screen 120 Hz
- Scroll `bigFileNoPause` run 1: 7 of 7,546 late (0.09%), frame p99 8.3 ms, main-thread CPU p50/p99 0.4/1.6 ms, 5 frames busy over 8.3 ms, 4.1 M instructions a frame at 4.20 GHz; other threads 10.04 ms and WindowServer 4.03 ms a frame; while indexing 5 of 206 late (2.43%); while searching 7 of 7,523 late (0.09%) (26 searches); heap peak 268.1 MB, footprint peak 835 MB; drawn by strips; screen 120 Hz
- Scroll `bigFileNoPause` run 2: 2 of 7,555 late (0.03%), frame p99 8.3 ms, main-thread CPU p50/p99 0.4/1.6 ms, 1 frames busy over 8.3 ms, 4.0 M instructions a frame at 4.20 GHz; other threads 10.05 ms and WindowServer 4.03 ms a frame; while indexing 2 of 129 late (1.55%); while searching 2 of 7,533 late (0.03%) (25 searches); heap peak 267.5 MB, footprint peak 596 MB; drawn by strips; screen 120 Hz
- Scroll `bigFileNoPause` run 3: 3 of 7,556 late (0.04%), frame p99 8.3 ms, main-thread CPU p50/p99 0.4/1.6 ms, 2 frames busy over 8.3 ms, 4.0 M instructions a frame at 4.21 GHz; other threads 10.04 ms and WindowServer 4.00 ms a frame; while indexing 2 of 129 late (1.55%); while searching 3 of 7,534 late (0.04%) (25 searches); heap peak 267.7 MB, footprint peak 754 MB; drawn by strips; screen 120 Hz
