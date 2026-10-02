# Performance

The viewer's DESIGN §1 budgets, measured on the app as it ships (task
1.10). `just perf` measures them all again and prints this table. DESIGN
§1 says what each budget means; Rob decided those readings at the phase 1
gate (2 October 2026, "Decisions" below).

**Read the table with the 3× rule in mind.** It was measured on an M5 Pro,
much faster than the reference machine (a base M1 Air). None is available,
so the M5 Pro must show about 3× headroom: main-thread work per scroll
frame of at most about 2.8 ms, checked at p50 and p99. An Air is checked
during the beta.

## Results

Rob's run, 2 October 2026, screen **unlocked**, 3 runs of each, on `main`
at the phase 1 gate (product code as of `09664f9`). The full output is in
[`perf-runs/2026-10-02-m5pro-unlocked.md`](perf-runs/2026-10-02-m5pro-unlocked.md);
the verdicts below are `just perf`'s current ones, from that run's
numbers (`leal-perf --report`).

- **Machine:** MacBook Pro `Mac17,8`, Apple M5 Pro (6 performance + 12
  efficiency cores), 48 GB, macOS 27.0. **Not the reference machine** (a
  base M1 MacBook Air: 4 + 4 cores, 8 GB, 60 Hz display).
- **Displays:** the built-in Liquid Retina XDR (ProMotion, 120 Hz, main
  display) and a 2560 × 1440 display at 60 Hz. The scroll benchmark's
  window was on the built-in display at 120 Hz in every run.
- **Power:** on AC (`powermode 0`).
- **Load:** load average 5.3 / 4.2 / 3.2 at the start, about 1,090
  processes.
- **Screen:** unlocked, with Leal in front.

| Budget (DESIGN §1) | Measured on this Mac | How | Verdict |
|---|---|---|---|
| Launch < 300 ms | 159 ms (133–433) | process start to the end of `applicationDidFinishLaunching` ("Launched" signpost). Leal opens no empty window | mixed: one run took 433 ms (see "To watch") |
| Launched with a file, to its first rows < 450 ms | 313 ms (312–355) | process start to the grid's first draw with rows, the reference file: the cold open, judged as part of launch | pass |
| Open to first rows < 150 ms | 39 ms (34–47, 15 opens) | `read(from:)` to the grid's first draw with rows ("Open to first rows" signpost), the reference file, closed and opened again in the running app: the warm open | pass |
| Full index < 500 ms | 170 ms (163–215) | the core's "Index" signpost in the app, the reference file, with diagnostics | pass |
| Scrolling: no dropped frames at 120 Hz | 4, 3 and 6 late of about 7,550 (0.04–0.08%); main-thread work per frame p50 2.8–3.6 ms, p99 5.8–6.8 ms | the scroll benchmark's flings on the reference file after indexing; the 3× rule | **fail** (late frames; 3× rule) |
| … including while background work runs | 4, 4 and 2 late of about 7,555 (0.03–0.05%); p50 2.7–3.1 ms, p99 6.5–7.0 ms | the same from the first rows, while the index and review run, with a search for `SKU-` (every row) running and highlighted throughout | **fail** (late frames; 3× rule) |
| … stress: background work not pausing | 8 and 5 late of about 7,550 (0.07–0.11%) in runs 2 and 3; run 1 ignored | the 1 GB variant from the first rows: index, review and search all running at full speed; late frames only | **fail** |
| Leal's own heap, reference file < 40 MB | 8.7 MB after opening; settled after scrolling, 7.6–14.3 MB without a search and 19.4–19.7 MB with a search's results held | every malloc zone minus the per-window AppKit baseline (21 MB) | pass |
| Idle app, no document < 30 MB footprint | 17.1 MB | `heap -s`'s physical footprint, 5 s after launch | pass |

**Scrolling fails the 3× rule.** Main-thread work per frame is p50
2.7–3.6 ms and p99 5.8–7.0 ms on the reference file, against about
2.8 ms. About half of each frame is AppKit rebuilding the whole visible
content layer on every scroll step (1.6a, docs/tasks/1.10.md). Normal
scrolling ran at about 2.4 GHz (2.24–2.44 GHz). PLAN task 2.0a, "Scrolling
headroom", takes this on.

The late frames are few (2–6 in a run of about 7,550, against 7–59 on the
locked screen), but the budget is no dropped frames, so they still fail.

Also measured:

- **The cold open on its own:** 96 ms (95–115), `read(from:)` to the first
  rows with the app launched with the file. It is judged as part of launch
  (the table's second row). `open` to "Launched": 176 ms (144–454).
- **First paint in the core** (P0): 1.9 ms. Most of the cold open is
  AppKit's first window (see "Where the cold open goes").
- **Idle heap:** 9.6 MB. **Footprint with the reference file open:** 59
  MB (it counts graphics surfaces; mapped file pages are clean and not in
  it). The idle footprint's resident size (RSS) is 80 MB, not judged.
- **The heap, every zone:** 29.7 MB after opening; peaks while scrolling
  39.7–52.9 MB, which are AppKit's drawing and aren't judged. Settled after
  scrolling, every zone, 28.6–35.3 MB without a search and 40.4–40.7 MB
  with one.
- **Scroll work per frame** (main thread, reference file): 13.0–18.5 M
  instructions a frame at 2.24–2.44 GHz; 12–42 frames a run whose
  main-thread work took longer than 8.3 ms.
- **The stress runs** ran at 4.25 GHz, raised by the busy background
  threads: p50 1.4–1.5 ms and p99 3.3–4.7 ms a frame.

### To watch

- **One launch took 433 ms** (the first of three; median 159 ms). It is
  one run, but it is over the 300 ms budget, so watch for it in the next
  runs.
- **The `bigFileNoPause` run 1 outlier is ignored** (2,048 of 5,455 late,
  37.5%). Rob was using the Mac and moving windows around during it. Runs
  2 and 3 are representative.
- **The per-window AppKit baseline** (21 MB) was measured on the locked
  screen in 1.10. The idle heap is higher unlocked (9.6 MB against
  2.5 MB), so the baseline may be too. `leal-perf` doesn't measure it
  itself (`APPKIT_WINDOW_HEAP_MB` in `crates/leal-bench/src/perf.rs`).

### Earlier: the locked run

1 October 2026, commit `2de0aee` (task/1.10), on the same Mac with the
screen **locked**, while other agents were building and testing on it
(load averages of 3–7). Launch 110 ms (92–195); the cold open 95 ms
(94–99), the warm open 36 ms (31–41); the index 120 ms (119–131); scrolling
7–51 late frames a run after indexing and 48–59 with a search; the heap
28 MB after opening, all zones; idle footprint 10.9 MB (RSS 65–68 MB).

With the screen locked no app is frontmost, and macOS ran Leal's main
thread at about 1.8 GHz in every scroll run: 3.5–3.6 ms a frame at p50 and
7.2–7.8 ms at p99. In that run's stress case busy background threads
raised the clock to 4.25 GHz, and the same instructions took 0.7 ms a
frame: about 2.0 instructions a cycle against 4.3. So the locked screen's
main thread was most likely on an efficiency core (an inference from the
counters). WindowServer composites nothing for a locked screen either.
That is why the scrolling verdicts needed Rob's unlocked run.

## Caveats

- **This isn't the reference machine.** The 1.3 notes expect a base M1
  Air to be 2–3× slower than the M5 Pro. Hence the 3× rule for scrolling.
  At that rate the full index (170 ms) and the warm open (39 ms) would
  still pass, launch (159 ms, so 320–480 ms) would be borderline, and
  launch with a file (313 ms, so 630–940 ms) would fail. That is a guess,
  not a measurement. Memory doesn't depend on the core,
  so the heap and idle numbers carry over.
- **The reference machine's display is 60 Hz.** A base M1 Air can't show
  120 Hz, and on a 60 Hz display a late frame is one that missed 16.7 ms,
  which says nothing about 120 Hz. So when a run's display is slower than
  120 Hz, `just perf` judges the late-frame check from **frame work**: a
  frame counts as dropped if the main thread's work alone took longer than
  8.3 ms (`busyOver120Hz`), and the row says "120 Hz judged from frame
  work; the display is 60 Hz". That was chosen over printing "untested"
  because it still answers the question for the main thread, which is
  where Leal's time goes. It is one-sided: a frame over 8.3 ms would
  certainly have dropped, but one under it could still drop in rendering
  or compositing. So on a 60 Hz display a **fail is real and a pass is
  provisional**. To see 120 Hz itself, attach a 120 Hz display.
- **On a base M1 Air the 3× rule doesn't apply.** It stands in for the
  Air, so `just perf` leaves it out when the machine is a `MacBookAir10,1`
  and says so in the row.
- **Runs vary.** The after-load scroll gave 3, 4 and 6 late frames; the
  launch 133–433 ms.
- **`just perf --no-scroll`** measures the heap only after opening, with no
  search's results held, so its heap row says "pass (search results
  untested)" rather than "pass". So does a saved run from before the
  scroll benchmark reported `heapSettledMB`.
- **What the heap counts** (DESIGN §1): every malloc zone minus the
  per-window AppKit baseline, the heap with a two-row file open (21 MB in
  1.10, against 2.5 MB with no window). With stack logging
  (`malloc_history`), the core's share for the reference file is the row
  index (3.8 MB) and the diagnostics (1 MB). A running search holds 12
  bytes per matching row (12 MB when every row matches), which counts.
  While scrolling the heap rises by 10–14 MB for a moment (the drawing's
  display lists, laid-out lines and cell tiles) and falls back; those
  peaks don't count. `just perf` judges the heap after opening and once
  settled after the scroll runs, when a search's results are still held.

### Where the cold open goes

Time Profiler, headless, around one cold open (`xctrace record --template
'Time Profiler' --all-processes`), main thread, from `read(from:)` to the
first rows: 92 of 109 ms busy (the locked run, 1.10). `CSVDocument.open`
(the core's open and the P0 sizing) 12 ms; creating the window 12 ms;
showing it 30 ms, of which `NSCell` sizing 14 ms is mostly the system's
SwiftUI title bar; the first Core Animation commit 12 ms; the rest is the
process's first use of classes, fonts and methods. Opening the same file
again in the running app takes 36–39 ms.

## Decisions

Decided by Rob at the phase 1 gate, 2 October 2026
(docs/reports/phase-1-decisions.md), and written into DESIGN §1:

- **Idle memory** is measured as the physical footprint, which Activity
  Monitor's Memory column shows, not RSS. No AppKit app is under 30 MB of
  RSS, because RSS counts the shared system libraries every app maps.
- **The heap budget** is every malloc zone minus the per-window AppKit
  baseline (about 21 MB, measured). Search results count. AppKit's brief
  drawing peaks while scrolling don't.
- **Open** is judged warm, in a running app, against 150 ms. The cold open
  counts as part of launch: launch with a file to its first rows, against
  the launch and open budgets together (450 ms).
- **Launch** is measured to the end of `applicationDidFinishLaunching`,
  since Leal opens no empty window.
- **Search memory** of 12 bytes per matching row is accepted for v1: 12 MB
  on the reference file when every row matches, 120 MB on the 1 GB file. A
  more compact store (a bitmap of rows, `u32` counts) is left to PLAN 4.1's
  audit.
- **The 3× rule.** The base M1 Air stays the design target. None is
  available, so the M5 Pro must show about 3× headroom (main-thread work
  per scroll frame ≤ about 2.8 ms), and an Air is checked during the beta.
  `just perf` checks it at p50 and p99. Scrolling fails it today: PLAN 2.0a.

## Commands

Everything at once, about 25 minutes (Leal's windows come to the front):

```sh
just perf                       # builds the apps and files, runs everything, prints the table
just perf --runs 1 --no-scroll  # launch, open, index and memory only: about a minute
```

`just perf` writes the table to `target/perf/perf-latest.md` and every
number to `target/perf/perf-<time>.json`;
`cargo run --release -p leal-bench --bin leal-perf -- --report <json>`
prints a saved run's table again, with the current verdicts.

One measurement at a time:

```sh
# Launch, open, index, heap and idle memory, from the Release app's signposts and `heap -s`:
just perf --no-scroll
# Scrolling after the load (1.6's benchmark), while the load and a search run, and the stress case:
just bench-scroll target/bench-data/reference-v1.csv
just bench-scroll target/bench-data/reference-v1.csv fast release -LealBenchDuringLoad YES -LealBenchFind SKU-
just bench-scroll target/bench-data/reference-v1-10m.csv fast release -LealBenchDuringLoad YES -LealBenchFind SKU- -LealBenchNoPause YES
# The core: the index and its longest chunks (worst/…), against the budgets:
just bench index/
just bench-compare
```

The 1 GB file is made by `just perf` (or `cargo run --release -p
leal-bench --bin leal-refgen -- --rows 10000000 --out
target/bench-data/reference-v1-10m.csv`); the reference file by `just
reference-file`.

## How each budget is measured

Everything runs on the app as it ships: the Release build, sandboxed,
launched with `open` (LaunchServices), in front. A shell launch puts the
main thread on other cores at other clocks and isn't comparable
(docs/tasks/1.6.md, "Scroll performance"). Nothing sends input to the
system, and `leal-perf` quits only the processes it started, by PID.

- **Signposts.** The app emits two of its own, beside the core's ("First
  paint", "Index", "Review", "Find", …), in the `io.github.robhaswell.leal`
  subsystem, Points of Interest category:
  - **"Launched"**, an event at the end of `applicationDidFinishLaunching`,
    with the milliseconds since the process started (the kernel's process
    start time);
  - **"Open to first rows"**, an interval from `CSVDocument.read(from:)` to
    the grid's first draw with rows in it.

  `leal-perf` reads them with `log stream --signpost`. Any Instruments
  template shows them too, in the Points of Interest lane.
- **Memory.** `heap -s <pid>`: "All zones … bytes" is the heap (every
  malloc zone; mapped file pages and graphics surfaces aren't in it), and
  "Physical footprint" is what Activity Monitor shows as Memory. `ps -o
  rss` gives the resident size, reported but not judged. The heap verdict
  subtracts the per-window AppKit baseline, `APPKIT_WINDOW_HEAP_MB` in
  `crates/leal-bench/src/perf.rs`, from the heap after opening and from
  the scroll benchmark's heap once settled after scrolling
  (`heapSettledMB`).
- **Open.** The warm open, which the budget judges: the bench build's
  `-LealReopen 5` closes the document once it is indexed and opens the file
  again through the document controller, as Open Recent does, five times.
  The cold open: the Release app launched with the file, from the process
  starting to the first rows, judged against 450 ms.
- **Scrolling.** The bench build (`LEAL_BENCH`, never shipped: `just
  check-no-bench`) scrolls itself with a display link (`ScrollBench`,
  docs/tasks/1.6.md): vertical flings at 60,000 pt/s over 25,000 rows,
  horizontal flings to the far right, vertical there, back, then jumps to
  the end and the top. A frame is **late** if it came more than 1.5
  refreshes after the one before: it missed at least one refresh. The
  window floats, moves to the screen with the fastest refresh, and the run
  declares user activity every 5 s so a locked Mac keeps its display on.
  - **The 3× rule** is judged from the main thread's CPU time per frame,
    p50 and p99 over every frame scrolled (`cpuP50`, `cpuP99`), against `HEADROOM_FRAME_MS`
    (2.8 ms), on
    the reference file's two rows. A run without those figures leaves the
    rule untested, never passed. The stress row is judged on late frames
    only.
  - `-LealBenchDuringLoad YES` starts at the first rows instead of after
    indexing; `-LealBenchFind <text>` keeps a search running and its
    matches highlighted, starting it again when it finishes; the scroll
    still counts as input, so P2 work pauses as DESIGN §3.10 rule 3 says
    (it runs in the gaps: 18 searches in each run).
  - `-LealBenchNoPause YES` stops the scroll counting as input, so the
    review and the search run at full speed alongside: the worst case, not
    what real scrolling does.
  - Frames scrolled while the index or a search was running are also
    reported on their own (`whileIndexing`, `whileFinding`). On the
    reference file the index finishes soon after the first rows, so
    almost no frames are scrolled during it; the 1 GB file has about 140.

## Measuring on a base M1 Air

For the beta, or for Rob on any Mac.

1. **Set up.** Xcode, `brew install just xcodegen`, and rustup (the
   toolchain comes from `rust-toolchain.toml`). Clone the repository.
2. **Prepare the Mac.** Plug it in; turn off Low Power Mode; quit other
   apps; leave the screen unlocked and the lid open; turn off automatic
   display sleep for the run, or keep it short. On a Mac with an external
   display, the bench uses the faster one.
3. **Run** `just perf` from the repository. It takes about 25 minutes (more
   on an Air) and needs no input. Leal's windows open and close by
   themselves and float in front during the scroll runs: **don't use the
   Mac meanwhile**. A click or a key would go to them, and other apps'
   work would change the numbers.
4. **Read** `target/perf/perf-latest.md`. It records the machine, power,
   load, displays and whether the screen was locked. Save it under
   `docs/perf-runs/` with the date and the machine in its name, update the
   results above, and keep `target/perf/perf-<time>.json`.
5. **On the Air's 60 Hz display** the late-frame check is judged from
   frame work and says so (see "Caveats"): a fail is real, a pass
   provisional. To see 120 Hz itself, attach a 120 Hz display. The 3× rule
   doesn't apply on the Air.
6. If anything looks off, `just perf --runs 1 --no-scroll` takes a minute,
   and `just bench-scroll target/bench-data/reference-v1.csv` one scroll
   run.
