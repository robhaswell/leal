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

Rob's run of 3 October 2026, task 2.0b (strips), screen **unlocked**, 3 runs
of each, on branch `task/2.0b` rebased on `a40851a`, with the Mac idle. The
full output, with every scroll run and the strips-against-AppKit table, is
in [`perf-runs/2026-10-03-m5pro-unlocked-2.0b.md`](perf-runs/2026-10-03-m5pro-unlocked-2.0b.md).

- **Machine:** MacBook Pro `Mac17,8`, Apple M5 Pro (6 performance + 12
  efficiency cores), 48 GB, macOS 27.0. **Not the reference machine** (a
  base M1 MacBook Air: 4 + 4 cores, 8 GB, 60 Hz display).
- **Display:** the built-in Liquid Retina XDR (ProMotion, 120 Hz, main
  display), which the scroll benchmark's window was on in every run.
- **Power:** on AC (`powermode 0`).
- **Load:** load average 9.25 / 4.86 / 3.49 at the start, 980 processes.
- **Screen:** unlocked, with Leal in front.


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

**Scrolling meets the 3× rule with strips (2.0b).** Main-thread work per
frame on the reference file is p50 1.0–1.2 ms and p99 1.95–2.1 ms, against
about 2.8 ms, after indexing and with a search running, at the M5 Pro's
normal 2.0–2.2 GHz (the clock of the earlier runs). That is 2.5–2.6 M
instructions a frame after indexing and 3.1–3.2 M with a search, against
13.0–18.5 M before 2.0a. AppKit's drawing on its own (2.0a, the
`--compare-drawing` runs) gives p50 1.7–1.8 ms and p99 2.5–3.0 ms: it
passed after indexing but not with a search running (2.89–2.99 ms), so the
provisional wording about 2.0a is settled: strips were needed, and they
pass. ADR-0011's conditions are met.

**Late frames fail "no dropped frames".** After indexing, 1 frame in about
7,560 was late in one of three runs (0.01%); the other two runs and all
three runs with a search had none. The budget allows none, so the verdict
reads "fail", and the question of a tolerance is on the phase 2 gate list
(PLAN 2.7). The stress runs (background work not pausing) had 0, 4 and 2
late frames (up to 0.05%) and also fail; AppKit's drawing had 0–3 a run.

**Memory.** Strips add about 20 MB to the footprint peak while scrolling
on the reference file (233–238 MB after indexing, 241–252 MB with a
search, against 213 MB and 224–230 MB with AppKit's drawing); the heap is
unchanged and passes.

**One launch took 479 ms**, against a median of 170 ms (the three launches:
156–479 ms). The 2 October run had the same: 433 ms against 159 ms. It is
over the 300 ms budget, so the launch row says "mixed". Task 2.6a found the
cause (below): it is the first launch of a new copy of the app, which macOS
checks first, and not Leal.

### The slow first launch (task 2.6a)

Measured on 4 October 2026 (M5 Pro, screen locked, load 4–6), with
`leal-perf --only-launch --app <Leal.app> --runs 10`: ten launches in a
row of the Release app, each as the rest of `just perf` launches it
(`open -n`), from process start to "Launched".

| Case | Launch 1 | Launches 2–10 |
|---|---|---|
| The app just rebuilt (`just app release`, the old product deleted) | 387.7 ms | 104.5–138.5 ms (median 113) |
| A `cp -R` of that app, same bytes and CDHash, at a new path (A) | 311.5 ms | 109.1–110.3 ms |
| Another copy of A (B) | 289.4 ms | 110.6–112.5 ms |
| Another copy of A (C) | 333.9 ms | 126.1 ms |

The first launch of every new path is 180–280 ms slower, and the launches
after it are the same whichever path. The system log (`/usr/bin/log show`)
says why. For each of the four first launches, and for no other, `amfid`
logged "Entering OSX path" for the new executable, and `syspolicyd` then
logged "GK Xprotect results" (Gatekeeper's XProtect scan of it) 110–190 ms
later; `amfid` was asked about the `LealFFI` framework 5–7 ms after that.
So the process waits for the scan before `dyld` loads its first framework. A copy with the same CDHash is scanned again, so
it is the file at a new path, not the signature, that the system checks.

So it is not Leal's code: nothing in Leal runs before the scan finishes,
and the launches after it (104–139 ms here, 156–170 ms in Rob's runs, when
the Mac was busier) are within the 300 ms budget. The 433 and 479 ms
launches were the first of each `just perf` run, which follows the build
of the Release app. Nothing to fix in Leal. A user sees the same cost once
after installing or updating the app (not measured here: a notarised
release build may be assessed differently, which the beta can check). The
table's median of three launches is not affected; expect the first launch
after a build to be an outlier of about 200 ms.

### Cell edit to screen (task 2.5.1)

Not in Rob's run above: task 2.5.1 added the row. The implementer's run of
4 October 2026 (`just perf --only-edit --runs 2`, M5 Pro, screen locked,
load average about 7–9, other agents' builds running):

| Budget (DESIGN §1) | Measured on this Mac | How | Verdict |
|---|---|---|---|
| Cell edit to screen < 16 ms | 4.4 ms (1.6–10.7, 40 edits) | Return in the in-cell editor to the commit of the transaction that draws the edit ("Cell edit to screen" signpost), reference file, after indexing, 20 edits a run (bench build, `-LealBenchEdit`) | pass |

The slowest edits are those that widen their column, which redraws every
strip. Rob's next full run replaces this.

### Earlier runs

- **2 October, after 2.0a** (AppKit's drawing alone, a loaded machine;
  [`perf-runs/2026-10-02-m5pro-unlocked-2.0a.md`](perf-runs/2026-10-02-m5pro-unlocked-2.0a.md)):
  p50 1.4–1.6 ms and p99 2.2–2.6 ms after indexing, 5.2–6.4 M instructions
  a frame against 13.0–18.5 M, 0–2 late frames a run. It was provisional
  then, since the main thread ran faster than in Rob's run and the p99 with
  a search was likely over 2.8 ms. The 2.0b run settles it: it was.
  docs/tasks/2.0a.md has the breakdown.
- **2 October, the phase 1 gate** (`main` at `09664f9`;
  [`perf-runs/2026-10-02-m5pro-unlocked.md`](perf-runs/2026-10-02-m5pro-unlocked.md)):
  scrolling failed the 3× rule, p50 2.7–3.6 ms and p99 5.8–7.0 ms, with
  3–8 late frames a run. About half of each frame was AppKit rebuilding the
  whole visible content layer on every scroll step (1.6a,
  docs/tasks/1.10.md), which is what 2.0a and 2.0b took on. Launch 159 ms
  (133–433), the cold open 96 ms, the index 170 ms, the heap 8.7 MB after
  opening, idle footprint 17.1 MB.

Also measured in the 2.0b run:

- **The cold open on its own:** 115.0 ms (112.4–124.2), `read(from:)` to
  the first rows with the app launched with the file. It is judged as part
  of launch (the table's second row). `open` to "Launched": 183.8 ms
  (168.0–501.5).
- **First paint in the core** (P0): 1.8 ms. Most of the cold open is
  AppKit's first window (see "Where the cold open goes").
- **Idle heap:** 9.6 MB. **Footprint with the reference file open:** 63.6
  MB (it counts graphics surfaces; mapped file pages are clean and not in
  it). The idle footprint's resident size (RSS) is 80.5 MB, not judged.
- **The heap, every zone:** 31.7 MB after opening; peaks while scrolling
  37.9 MB (32.7–43.5), which aren't judged.
- **The stress runs** ran at 4.23 GHz, raised by the busy background
  threads: p50 0.5 ms and p99 1.8 ms a frame with strips.

### To watch

- **Slow launches:** 433 ms (2 October) and 479 ms (3 October), each the
  first of three, against medians of 159 and 170 ms: the system's first-run
  scan of a freshly built app, not Leal (task 2.6a, "The slow first
  launch").
- **The `bigFileNoPause` run 1 outlier of 2 October is ignored** (2,048 of
  5,455 late, 37.5%). Rob was using the Mac and moving windows around
  during it. The 3 October runs had no such outlier.
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
  At that rate the full index (141 ms) and the warm open (48 ms) would
  still pass, launch (170 ms, so 340–510 ms) would be borderline, and
  launch with a file (364 ms, so 730–1,090 ms) would fail. That is a guess,
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
- **Runs vary.** The after-load scroll gave 1, 0 and 0 late frames with
  strips (0, 2 and 0 with AppKit's drawing); the launch 156–479 ms.
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
  `just perf` checks it at p50 and p99. Scrolling failed it then; 2.0a and 2.0b fixed it ("Results").

## Commands

Everything at once, about 25 minutes (Leal's windows come to the front):

```sh
just perf                       # builds the apps and files, runs everything, prints the table
just perf --runs 1 --no-scroll  # launch, open, index and memory only: about a minute
leal-perf --only-launch --app build/DerivedData/Build/Products/Release/Leal.app --runs 10  # launches only, one line each (cargo run --release -p leal-bench --bin leal-perf --)
just perf --only-edit           # cell edit to screen only (task 2.5.1): about a minute
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

- **Signposts.** The app emits three of its own, beside the core's ("First
  paint", "Index", "Review", "Find", …), in the `io.github.robhaswell.leal`
  subsystem, Points of Interest category:
  - **"Launched"**, an event at the end of `applicationDidFinishLaunching`,
    with the milliseconds since the process started (the kernel's process
    start time);
  - **"Open to first rows"**, an interval from `CSVDocument.read(from:)` to
    the grid's first draw with rows in it.
  - **"Cell edit to screen"** (task 2.5.1), an interval from Return in the
    in-cell editor to the completion of the Core Animation transaction
    that draws the edited cell: the strips are drawn at that commit, so
    it covers the core's edit, the invalidation and the redraw on the
    main thread, not the render server's frame.

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
