# Rob's unlocked run, task 2.0b (strips), 2026-10-03

`just perf --compare-drawing` on branch `task/2.0b` (rebased on a40851a),
Mac idle and unlocked, run by Rob. Read with the table at the end:

- **The 3× rule passes with strips:** main-thread work per frame p50
  1.0–1.2 ms, p99 1.95–2.1 ms on the reference file, at the M5 Pro's normal
  2.0–2.2 GHz (the clock of Rob's earlier runs). AppKit's drawing (2.0a
  alone) gives p50 1.7–1.8 ms and p99 2.5–3.0 ms: it passes after
  indexing but not with a search running (2.89–2.99 ms).
- **Late frames:** 1 in 7,559 in one strips run, against 0–3 per run with
  AppKit's drawing. The budget allows none, so the verdict reads "fail";
  for the phase 2 gate.
- **Memory:** strips add about 20 MB footprint peak on the reference file;
  the heap is unchanged.
- **Launch:** one of three launches took 479 ms (median 170), as in the
  2026-10-02 run (433 ms). The first launch after a build is the likely
  cause; worth a look.

## leal-perf

- Machine: Mac17,8, Apple M5 Pro (6 performance + 12 efficiency cores), 48 GB, macOS 27.0
- Power: Now drawing from 'AC Power'; powermode 0
- Load average: { 9.25 4.86 3.49 }; 980 processes
- Screen: unlocked
- Displays: Display Type: Built-in Liquid Retina XDR Display; Resolution: 3456 x 2234 Retina; Main Display: Yes; Resolution: 2560 x 1440 (QHD/WQHD - Wide Quad High Definition); UI Looks like: 2560 x 1440 @ 60.00Hz

| Budget (DESIGN §1) | Measured on this Mac | How | Verdict |
|---|---|---|---|
| Launch < 300 ms | 170.4 ms (156.4–479.0, 3 runs) | process start to the end of `applicationDidFinishLaunching` ("Launched" signpost); Leal opens no empty window | mixed |
| Launched with a file, to its first rows < 450 ms | 364.0 ms (334.8–369.9, 3 runs) | process start to the grid's first draw with rows, reference file: the cold open, judged as part of launch against the launch and open budgets together | pass |
| Open to first rows < 150 ms | 47.6 ms (41.2–51.8, 15 runs) | `read(from:)` to the grid's first draw with rows ("Open to first rows" signpost), reference file, closed and opened again in the running app: the warm open (bench build, `-LealReopen`) | pass |
| Full index < 500 ms | 141.1 ms (140.7–155.9, 3 runs) | the core's "Index" signpost in the app, reference file, with diagnostics | pass |
| Scrolling: no dropped frames at 120 Hz | 1 of 7,559 late (0.01%); 0 of 7,560 late (0.00%); 0 of 7,560 late (0.00%); main-thread work per frame p50 1.2 ms, p99 2.0 ms | `ScrollBench` flings, reference file, after indexing; late = missed a refresh; 3× rule (DESIGN §1): main-thread work per frame ≤ 2.8 ms at p50 and p99 | fail (late frames) |
| … including while background work runs | 0 of 7,559 late (0.00%); 0 of 7,559 late (0.00%); 0 of 7,556 late (0.00%); main-thread work per frame p50 1.0–1.1 ms, p99 2.0–2.1 ms | the same from the first rows, while the index and review run, with a search running throughout; 3× rule (DESIGN §1): main-thread work per frame ≤ 2.8 ms at p50 and p99 | pass |
| … stress: background work not pausing (beyond the budget) | 0 of 7,557 late (0.00%); 4 of 7,556 late (0.05%); 2 of 7,556 late (0.03%); main-thread work per frame p50 0.5 ms, p99 1.8 ms | 1 GB variant from the first rows: index, review and a search all running, the scroll not reported as input; judged on late frames only | fail |
| Leal's heap, reference file < 40 MB | 10.7 MB (10.6–10.9, 3 runs) after opening; settled after scrolling, 9.5 MB (9.3–11.3, 3 runs) without a search and 11.7 MB (10.5–22.3, 3 runs) with a search's results held. Not judged, all zones: 31.7 MB (31.6–31.9, 3 runs) after opening; peak while scrolling 37.9 MB (32.7–43.5, 6 runs) | all malloc zones minus the per-window AppKit baseline (21 MB, a two-row file open: docs/tasks/1.10.md): `heap -s` after the review finished, and the bench's `malloc_zone_statistics` once settled after scrolling; AppKit's drawing peaks are left out | pass |
| Idle app, no document < 30 MB footprint | 17.0 MB (16.9–17.0, 3 runs) footprint. Not judged: resident size 80.5 MB (77.4–80.6, 3 runs) | `heap -s` physical footprint (Activity Monitor's Memory) after the app settles; the resident size (RSS) also counts shared system libraries | pass |

Details:

- `open` to "Launched": 183.8 ms (168.0–501.5, 3 runs)
- Idle heap: 9.6 MB (9.6–9.6, 3 runs)
- With the file: the cold open (`read(from:)` to first rows, the app launched with it) 115.0 ms (112.4–124.2, 3 runs); the core's first paint 1.8 ms (1.8–1.9, 3 runs); footprint after opening 63.6 MB (63.1–67.5, 3 runs)
- Scroll `afterLoad` run 1: 1 of 7,559 late (0.01%), frame p99 8.3 ms, main-thread CPU p50/p99 1.2/2.0 ms, 1 frames busy over 8.3 ms, 2.5 M instructions a frame at 2.06 GHz; other threads 1.88 ms and WindowServer 3.83 ms a frame; while indexing 0 of 0 late (0.00%); while searching 0 of 0 late (0.00%) (0 searches); heap peak 32.7 MB, footprint peak 233 MB; drawn by strips; screen 120 Hz
- Scroll `afterLoad` run 2: 0 of 7,560 late (0.00%), frame p99 8.3 ms, main-thread CPU p50/p99 1.2/2.0 ms, 0 frames busy over 8.3 ms, 2.6 M instructions a frame at 2.05 GHz; other threads 1.92 ms and WindowServer 3.91 ms a frame; while indexing 0 of 0 late (0.00%); while searching 0 of 0 late (0.00%) (0 searches); heap peak 32.9 MB, footprint peak 238 MB; drawn by strips; screen 120 Hz
- Scroll `afterLoad` run 3: 0 of 7,560 late (0.00%), frame p99 8.3 ms, main-thread CPU p50/p99 1.1/1.9 ms, 0 frames busy over 8.3 ms, 2.6 M instructions a frame at 1.98 GHz; other threads 1.90 ms and WindowServer 3.91 ms a frame; while indexing 0 of 0 late (0.00%); while searching 0 of 0 late (0.00%) (0 searches); heap peak 33.0 MB, footprint peak 238 MB; drawn by strips; screen 120 Hz
- Scroll `duringLoadWithFind` run 1: 0 of 7,559 late (0.00%), frame p99 8.3 ms, main-thread CPU p50/p99 1.0/2.0 ms, 1 frames busy over 8.3 ms, 3.1 M instructions a frame at 2.16 GHz; other threads 2.07 ms and WindowServer 3.61 ms a frame; while indexing 0 of 0 late (0.00%); while searching 0 of 7,559 late (0.00%) (19 searches); heap peak 43.4 MB, footprint peak 246 MB; drawn by strips; screen 120 Hz
- Scroll `duringLoadWithFind` run 2: 0 of 7,559 late (0.00%), frame p99 8.3 ms, main-thread CPU p50/p99 1.1/2.0 ms, 0 frames busy over 8.3 ms, 3.1 M instructions a frame at 2.14 GHz; other threads 2.09 ms and WindowServer 3.82 ms a frame; while indexing 0 of 0 late (0.00%); while searching 0 of 7,559 late (0.00%) (19 searches); heap peak 42.9 MB, footprint peak 252 MB; drawn by strips; screen 120 Hz
- Scroll `duringLoadWithFind` run 3: 0 of 7,556 late (0.00%), frame p99 8.3 ms, main-thread CPU p50/p99 1.1/2.1 ms, 0 frames busy over 8.3 ms, 3.2 M instructions a frame at 2.15 GHz; other threads 2.09 ms and WindowServer 3.87 ms a frame; while indexing 0 of 0 late (0.00%); while searching 0 of 7,556 late (0.00%) (18 searches); heap peak 43.5 MB, footprint peak 241 MB; drawn by strips; screen 120 Hz
- Scroll `bigFileNoPause` run 1: 0 of 7,557 late (0.00%), frame p99 8.3 ms, main-thread CPU p50/p99 0.5/1.8 ms, 4 frames busy over 8.3 ms, 3.8 M instructions a frame at 4.23 GHz; other threads 10.14 ms and WindowServer 3.88 ms a frame; while indexing 0 of 150 late (0.00%); while searching 0 of 7,536 late (0.00%) (24 searches); heap peak 269.2 MB, footprint peak 811 MB; drawn by strips; screen 120 Hz
- Scroll `bigFileNoPause` run 2: 4 of 7,556 late (0.05%), frame p99 8.3 ms, main-thread CPU p50/p99 0.5/1.8 ms, 4 frames busy over 8.3 ms, 3.8 M instructions a frame at 4.23 GHz; other threads 10.15 ms and WindowServer 3.93 ms a frame; while indexing 3 of 103 late (2.91%); while searching 4 of 7,534 late (0.05%) (25 searches); heap peak 269.1 MB, footprint peak 802 MB; drawn by strips; screen 120 Hz
- Scroll `bigFileNoPause` run 3: 2 of 7,556 late (0.03%), frame p99 8.3 ms, main-thread CPU p50/p99 0.5/1.8 ms, 2 frames busy over 8.3 ms, 3.8 M instructions a frame at 4.23 GHz; other threads 10.16 ms and WindowServer 3.96 ms a frame; while indexing 2 of 109 late (1.83%); while searching 2 of 7,534 late (0.03%) (25 searches); heap peak 269.5 MB, footprint peak 665 MB; drawn by strips; screen 120 Hz
- Scroll `afterLoad`, AppKit's drawing run 1: 0 of 7,560 late (0.00%), frame p99 8.3 ms, main-thread CPU p50/p99 1.8/2.6 ms, 0 frames busy over 8.3 ms, 5.5 M instructions a frame at 2.14 GHz; other threads 1.95 ms and WindowServer 3.82 ms a frame; while indexing 0 of 0 late (0.00%); while searching 0 of 0 late (0.00%) (0 searches); heap peak 33.2 MB, footprint peak 213 MB; drawn by appkit; screen 120 Hz
- Scroll `afterLoad`, AppKit's drawing run 2: 2 of 7,558 late (0.03%), frame p99 8.3 ms, main-thread CPU p50/p99 1.8/2.7 ms, 4 frames busy over 8.3 ms, 5.5 M instructions a frame at 2.14 GHz; other threads 1.97 ms and WindowServer 3.96 ms a frame; while indexing 0 of 0 late (0.00%); while searching 0 of 0 late (0.00%) (0 searches); heap peak 33.9 MB, footprint peak 213 MB; drawn by appkit; screen 120 Hz
- Scroll `afterLoad`, AppKit's drawing run 3: 0 of 7,560 late (0.00%), frame p99 8.3 ms, main-thread CPU p50/p99 1.7/2.5 ms, 4 frames busy over 8.3 ms, 5.6 M instructions a frame at 2.03 GHz; other threads 1.87 ms and WindowServer 3.90 ms a frame; while indexing 0 of 0 late (0.00%); while searching 0 of 0 late (0.00%) (0 searches); heap peak 33.6 MB, footprint peak 213 MB; drawn by appkit; screen 120 Hz
- Scroll `duringLoadWithFind`, AppKit's drawing run 1: 3 of 7,553 late (0.04%), frame p99 8.3 ms, main-thread CPU p50/p99 1.8/2.9 ms, 1 frames busy over 8.3 ms, 6.9 M instructions a frame at 2.20 GHz; other threads 2.21 ms and WindowServer 3.72 ms a frame; while indexing 0 of 0 late (0.00%); while searching 3 of 7,553 late (0.04%) (19 searches); heap peak 41.8 MB, footprint peak 230 MB; drawn by appkit; screen 120 Hz
- Scroll `duringLoadWithFind`, AppKit's drawing run 2: 3 of 7,553 late (0.04%), frame p99 8.3 ms, main-thread CPU p50/p99 1.8/2.9 ms, 3 frames busy over 8.3 ms, 6.9 M instructions a frame at 2.18 GHz; other threads 2.21 ms and WindowServer 3.81 ms a frame; while indexing 0 of 0 late (0.00%); while searching 3 of 7,553 late (0.04%) (18 searches); heap peak 42.8 MB, footprint peak 224 MB; drawn by appkit; screen 120 Hz
- Scroll `duringLoadWithFind`, AppKit's drawing run 3: 0 of 7,554 late (0.00%), frame p99 8.3 ms, main-thread CPU p50/p99 1.8/3.0 ms, 2 frames busy over 8.3 ms, 6.9 M instructions a frame at 2.22 GHz; other threads 2.17 ms and WindowServer 3.85 ms a frame; while indexing 0 of 0 late (0.00%); while searching 0 of 7,554 late (0.00%) (19 searches); heap peak 42.2 MB, footprint peak 229 MB; drawn by appkit; screen 120 Hz
- Scroll `bigFileNoPause`, AppKit's drawing run 1: 2 of 7,556 late (0.03%), frame p99 8.3 ms, main-thread CPU p50/p99 0.7/1.7 ms, 2 frames busy over 8.3 ms, 5.8 M instructions a frame at 4.24 GHz; other threads 10.08 ms and WindowServer 3.86 ms a frame; while indexing 0 of 109 late (0.00%); while searching 2 of 7,534 late (0.03%) (25 searches); heap peak 267.5 MB, footprint peak 511 MB; drawn by appkit; screen 120 Hz
- Scroll `bigFileNoPause`, AppKit's drawing run 2: 1 of 7,556 late (0.01%), frame p99 8.3 ms, main-thread CPU p50/p99 0.7/1.8 ms, 2 frames busy over 8.3 ms, 5.8 M instructions a frame at 4.24 GHz; other threads 10.06 ms and WindowServer 3.86 ms a frame; while indexing 0 of 107 late (0.00%); while searching 1 of 7,534 late (0.01%) (25 searches); heap peak 267.9 MB, footprint peak 732 MB; drawn by appkit; screen 120 Hz
- Scroll `bigFileNoPause`, AppKit's drawing run 3: 1 of 7,557 late (0.01%), frame p99 8.3 ms, main-thread CPU p50/p99 0.7/1.6 ms, 0 frames busy over 8.3 ms, 5.8 M instructions a frame at 4.24 GHz; other threads 10.07 ms and WindowServer 3.94 ms a frame; while indexing 1 of 106 late (0.94%); while searching 1 of 7,535 late (0.01%) (25 searches); heap peak 267.6 MB, footprint peak 513 MB; drawn by appkit; screen 120 Hz

Drawing compared (`--compare-drawing`, runs alternated; task 2.0b): strips against AppKit's drawing

| Scroll run | Main-thread CPU p50 / p99 | Instructions a frame | Other threads | WindowServer | Late frames | Footprint peak |
|---|---|---|---|---|---|---|
| `afterLoad`, strips | 1.12–1.16 ms / 1.95–1.98 ms | 2.5–2.6 M | 1.88–1.92 ms | 3.83–3.91 ms | 1, 0, 0 | 233–238 MB |
| `afterLoad`, AppKit | 1.69–1.81 ms / 2.49–2.68 ms | 5.5–5.6 M | 1.87–1.97 ms | 3.82–3.96 ms | 0, 2, 0 | 213 MB |
| `duringLoadWithFind`, strips | 1.04–1.11 ms / 1.96–2.09 ms | 3.1–3.2 M | 2.07–2.09 ms | 3.61–3.87 ms | 0, 0, 0 | 241–252 MB |
| `duringLoadWithFind`, AppKit | 1.79–1.81 ms / 2.89–2.99 ms | 6.9 M | 2.17–2.21 ms | 3.72–3.85 ms | 3, 3, 0 | 224–230 MB |
| `bigFileNoPause`, strips | 0.50–0.54 ms / 1.79–1.81 ms | 3.8 M | 10.14–10.16 ms | 3.88–3.96 ms | 0, 4, 2 | 665–811 MB |
| `bigFileNoPause`, AppKit | 0.69–0.72 ms / 1.64–1.77 ms | 5.8 M | 10.06–10.08 ms | 3.86–3.94 ms | 2, 1, 1 | 511–732 MB |
