# Performance

The viewer's DESIGN §1 budgets, measured on the app as it ships (task
1.10). `just perf` measures them all again and prints this table.

**Read the table with the caveats below.** It was measured on a Mac much
faster than the reference machine, with the screen locked. A pass here is
not a pass on a base M1 Air; a fail here is real, though the locked screen
makes the scrolling numbers worse than they would be in use.

## Results

Measured 1 October 2026, commit `2de0aee` (task/1.10), 3 runs of each.

- **Machine:** MacBook Pro `Mac17,8`, Apple M5 Pro (6 performance + 12
  efficiency cores), 48 GB, macOS 27.0. **Not the reference machine** (a
  base M1 MacBook Air: 4 + 4 cores, 8 GB, 60 Hz display).
- **Displays:** the built-in Liquid Retina XDR (ProMotion, 120 Hz, main
  display) and a Dell U2515H at 60 Hz. The scroll benchmark's window was on
  the built-in display at 120 Hz in every run.
- **Power:** on AC, battery 80%, not in Low Power Mode.
- **Load:** load average 6.6 / 4.1 / 3.7 at the start, about 1,090
  processes; other agents were building and testing on the same Mac.
- **Screen: locked** for every run. See the caveats.

| Budget (DESIGN §1) | Measured on this Mac | How | Verdict |
|---|---|---|---|
| Launch to empty window < 300 ms | 110 ms (92–195) | process start to the end of `applicationDidFinishLaunching` ("Launched" signpost). Leal opens no empty window | pass |
| Open to first rows < 150 ms | 95 ms (94–99) | `read(from:)` to the grid's first draw with rows ("Open to first rows" signpost), the reference file, the app launched with it | pass |
| … opened in a running app | 36 ms (31–41, 15 opens) | the same signpost, closing the file and opening it again | pass |
| Full index < 500 ms | 120 ms (119–131) | the core's "Index" signpost in the app, the reference file, with diagnostics | pass |
| Scrolling: no dropped frames at 120 Hz | 7, 23 and 51 late of about 7,530 (0.09–0.68%) | the scroll benchmark's flings on the reference file after indexing | **fail** |
| … including while background work runs | 48, 59 and 48 late of about 7,500 (0.64–0.79%) | the same from the first rows, while the index and review run, with a search for `SKU-` (every row) running and highlighted throughout | **fail** |
| … stress: background work not pausing | 2, 3 and 3 late of about 7,555 (0.03–0.04%) | the 1 GB variant from the first rows: index, review and search all running at full speed | **fail** (see caveats: this one ran at 4.25 GHz) |
| Leal's own heap, reference file < 40 MB | 28 MB after opening; peaks while scrolling 39–42 MB, and 44–50 MB with the search running | `heap -s` (every malloc zone) once the review finished; the scroll benchmark's highest `malloc_zone_statistics` | **fail** (settled: pass) |
| Idle app, no document < 30 MB resident | 10.9 MB footprint (RSS 65–68 MB) | `heap -s`'s physical footprint, 5 s after launch | pass |

Also measured:

- **Launch with a file to its first rows:** 263 ms (247–288), from the
  process starting. `open` to "Launched": 123 ms (104–209).
- **First paint in the core** (P0): 1.8 ms. The rest of the 95 ms cold
  open is AppKit's first window (see "Where the cold open goes").
- **Idle heap:** 2.5 MB. **Footprint with the reference file open:** 40–43
  MB (it counts graphics surfaces; mapped file pages are clean and not in
  it).
- **Scroll work per frame** (main thread, reference file): CPU p50
  3.5–3.6 ms, p99 7.2–7.8 ms; 12.7 M instructions a frame; 55–73 frames a
  run whose main-thread work took longer than 8.3 ms; the main thread ran
  at 1.76–1.84 GHz. With the search highlighted: 14.4 M instructions,
  85–93 frames over 8.3 ms.
- **The core's benchmarks** (criterion, same Mac): `index/run` 44.6 ms,
  `index/build` 40.7 ms (both without diagnostics); the plain index's
  longest chunk on blank lines 0.94–1.09 ms.

## Caveats

- **This isn't the reference machine.** The M5 Pro's cores are much faster
  than a base M1 Air's. The 1.3 notes expect the Air to be 2–3× slower.
  At that rate the full index (120 ms) and opening in a running app
  (36 ms) would still pass, launch (110 ms, so 220–330 ms) would be
  borderline, and the cold open (95 ms, so 190–285 ms) would fail. That is
  a guess, not a measurement, and these runs were on slow clocks too (next
  point). Memory doesn't depend on the core, so the heap and idle numbers
  carry over.
- **The screen was locked.** Then no app is frontmost, and macOS ran
  Leal's main thread at about 1.8 GHz in every scroll run (the bench
  reports the clock: cycles over CPU time). When Rob scrolls, Leal is in
  front. Evidence that the clock dominates: in the stress run, busy
  background threads raised the clock to 4.25 GHz, and with the same
  instructions per frame (12.7–12.9 M) the main thread took 0.7 ms a frame
  and dropped 2–3 frames instead of 7–51. Also, WindowServer composites
  nothing for a locked screen, so compositing isn't measured at all.
  **The scrolling verdicts need an unlocked run.**
- **The reference machine's display is 60 Hz.** A base M1 Air can't show
  120 Hz, so its late frames are judged against 16.7 ms. For the 120 Hz
  budget, read the bench's `busyOver120Hz` (frames whose main-thread work
  took longer than 8.3 ms), or attach a 120 Hz display.
- **Other agents were working on this Mac**, at load averages of 3–7.
  Runs vary: the after-load scroll gave 7, 23 and 51 late frames.
- **"Launch to empty window"**: Leal opens no empty window, so it is
  measured to when the app has finished launching and can take File ▸
  Open.
- **The heap budget counts every malloc zone**, which includes what
  AppKit and Core Animation allocate for the window. With a two-row file
  the heap is 21 MB after opening, against 2.5 MB with no window, so most
  of the 28 MB is the window's, not Leal's data. With stack logging
  (`malloc_history`), the core's share for the reference file is the row
  index (3.8 MB) and the diagnostics (1 MB). While scrolling it rises by
  10–14 MB for a moment and falls back to 26–32 MB; a running search adds
  12 bytes per matching row (12 MB when every row matches). Whether the
  budget means all of that is an open question (docs/tasks/1.10.md).

### Where the cold open goes

Time Profiler, headless, around one cold open (`xctrace record --template
'Time Profiler' --all-processes`), main thread, from `read(from:)` to the
first rows: 92 of 109 ms busy. `CSVDocument.open` (the core's open and the
P0 sizing) 12 ms; creating the window 12 ms; showing it 30 ms, of which
`NSCell` sizing 14 ms is mostly the system's SwiftUI title bar; the first
Core Animation commit 12 ms; the rest is the process's first use of
classes, fonts and methods. Opening the same file again in the running
app takes 36 ms.

## Commands

Everything at once, about 25 minutes (Leal's windows come to the front):

```sh
just perf                       # builds the apps and files, runs everything, prints the table
just perf --runs 1 --no-scroll  # launch, open, index and memory only: about a minute
```

`just perf` writes the table to `target/perf/perf-latest.md` and every
number to `target/perf/perf-<time>.json`;
`cargo run --release -p leal-bench --bin leal-perf -- --report <json>`
prints a saved run's table again.

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
  rss` gives the resident size.
- **Open in a running app.** The bench build's `-LealReopen 5` closes the
  document once it is indexed and opens the file again through the
  document controller, as Open Recent does, five times.
- **Scrolling.** The bench build (`LEAL_BENCH`, never shipped: `just
  check-no-bench`) scrolls itself with a display link (`ScrollBench`,
  docs/tasks/1.6.md): vertical flings at 60,000 pt/s over 25,000 rows,
  horizontal flings to the far right, vertical there, back, then jumps to
  the end and the top. A frame is **late** if it came more than 1.5
  refreshes after the one before: it missed at least one refresh. The
  window floats, moves to the screen with the fastest refresh, and the run
  declares user activity every 5 s so a locked Mac keeps its display on.
  - `-LealBenchDuringLoad YES` starts at the first rows instead of after
    indexing; `-LealBenchFind <text>` keeps a search running and its
    matches highlighted, starting it again when it finishes; the scroll
    still counts as input, so P2 work pauses as DESIGN §3.10 rule 3 says
    (it runs in the gaps: 20 searches finished in each run).
  - `-LealBenchNoPause YES` stops the scroll counting as input, so the
    review and the search run at full speed alongside: the worst case, not
    what real scrolling does.
  - Frames scrolled while the index or a search was running are also
    reported on their own (`whileIndexing`, `whileFinding`). On the
    reference file the index finishes 20–35 ms after the first rows, so
    almost no frames are scrolled during it; the 1 GB file has about 140.

## Measuring on a base M1 Air, or an unlocked Mac

For Rob. The numbers that decide the phase 1 gate need either.

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
   load, displays and whether the screen was locked. Paste it into this
   file under a new heading with the date, and keep
   `target/perf/perf-<time>.json`.
5. **On the Air's 60 Hz display**, judge the 120 Hz budget by
   "frames busy over 8.3 ms" in the details, not by "late". To see
   120 Hz itself, attach a 120 Hz display.
6. If anything looks off, `just perf --runs 1 --no-scroll` takes a minute,
   and `just bench-scroll target/bench-data/reference-v1.csv` one scroll
   run.
