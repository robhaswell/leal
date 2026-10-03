# 0011 — Grid drawing: strips of rows that scrolling moves

- Status: accepted (Rob, 2026-10-02), conditional on tests and an on-screen
  check
- Date: 2026-10-02
- Refines: ADR-0001 (its option B: how the cells reach the screen)

## In one minute

**Decision: the grid and the gutter draw into strips of rows that scrolling
moves, instead of letting AppKit rebuild the visible grid on every scroll
step. Grids wider than 1.5 times the visible width (or 4,096 pt) keep
today's drawing.** It lands only once
the prototype has tests and has been checked on screen, as a follow-up
task; task 2.0a lands without it.

- Task 2.0a cut Leal's own work per scroll frame (text layout and core
  reads moved off the main thread, batched drawing): 5.2–6.4 M instructions
  a frame against 13.0–18.5 M. Its `just perf` run gave p99 2.2–2.6 ms on
  a loaded Mac with a fast clock; in Rob's conditions that would be about
  2.75–3.6 ms after indexing and 3.1–3.9 ms with a search, likely over the
  3× rule's 2.8 ms. The rule's p99 is unproven until an unloaded run.
- About half of what is left in each frame is AppKit rebuilding the
  visible grid's and gutter's layers from their recorded drawing. A
  prototype that draws into strips instead (branch `task/2.0a-strips`)
  takes the reference file to p50 0.93–0.98 ms and p99 1.68–1.90 ms on
  this Mac, against 1.59–1.63 / 2.35–2.58 ms for 2.0a in the same runs:
  **1.24–1.54× better at p99**.
- Costs and risks: 10–30 MB more footprint while scrolling, wide grids keep
  today's drawing (where 2.0a's p99 is already over 2.8 ms at 200 columns),
  phase 2's in-cell editor must sit above the strips, and the risks below.

## Context

ADR-0001 chose a custom grid: an `NSScrollView` whose document view draws
only the cells AppKit asks for. Task 1.6a found the catch: AppKit backs a
view in a scroll view with a layer the size of the visible rectangle, and
on every scroll step rebuilds it from the view's recorded drawing (a
display list), though only a strip of it is new. It also redraws the whole
visible view about every 1.25 s while scrolling (2.0a). The gutter, a
second scrolled view, pays the same.

1.10 tried tiles of 512 × 256 pt that scrolling moves, and rejected them:
a frame in which a row of tiles came into view drew all of them at once
(ten times the late frames), and they added about 180 MB of footprint.

2.0a moved Leal's own per-frame work off the main thread. What remains in a
frame is mostly AppKit's:

| Main thread, average frame (Time Profiler, 2.0a) | Time |
|---|---:|
| AppKit and Core Animation rebuilding the visible layers | 0.73 ms (47%) |
| AppKit's display cycle (tracking areas, layout) | 0.21 ms |
| Core Animation commit, other | 0.13 ms |
| Leal's own drawing | 0.25 ms |
| Scrolling, run loop and the rest | 0.25 ms |

## Options

**Keep AppKit's drawing (2.0a as landed).** No new code. Per-frame work is
about 5 M instructions, half of it AppKit's rebuild; p99 is about 2.4 ms
on this Mac at 2.6–3.1 GHz and an estimated 3.1–3.9 ms in Rob's
conditions.

**Strips of rows (the prototype).**

- The grid's cells are drawn into strips four rows (88 pt) tall and as wide
  as the grid, each a `CALayer` with its own backing store, drawn
  asynchronously (`drawsAsynchronously`: the drawing is recorded on the
  main thread and rasterised on Core Animation's threads). They sit in a
  layer-hosting view over the clip view, under the scrollers, and take no
  clicks.
- A scroll moves the strips; a strip is drawn when it comes into view or
  when something in it changes (selection, find, new rows, appearance), or
  one strip ahead of the scroll in a quiet frame. A horizontal scroll draws
  nothing. Strips are placed relative to the visible rectangle, so 880M pt
  grids stay exact.
- The gutter's numbers are drawn the same way, in a view over its clip
  view.
- `GridView` and the gutter stay where they are, for events, scrolling and
  `cacheDisplay`. Their own layers are left empty (`wantsUpdateLayer`), so
  AppKit records and replays nothing, and its requests to redraw the strip
  a scroll exposes are ignored.
- What is drawn comes from the same code (`GridView`'s drawing), so it is
  the same drawing; only where it lands changes.

Measured against 2.0a, same binary, alternating runs, the full scroll
benchmark (vertical flings, horizontal, vertical at the far right), this
Mac:

| | 2.0a | Strips |
|---|---|---|
| Reference file: CPU a frame p50 / p99 | 1.59–1.63 / 2.35–2.58 ms | 0.93–0.98 / 1.68–1.90 ms |
| … instructions p50 / p99 | 5.4–5.5 / 8.5–9.0 M | 2.2 / 5.9–6.5 M |
| … late frames | 0 | 0 |
| … footprint peak / settled | 212 / 87 MB | 224–238 / 86–88 MB |
| 200 columns (column tiles of 512 pt): CPU p50 / p99 | 1.59 / 2.88 ms | 1.15 / 4.07 ms |
| … horizontal flings, p99 | 3.1 ms | 7.3 ms |

On the 200-column file a grid wider than 4,096 pt is cut into columns of
tiles, and a column of them coming into view during a horizontal fling
draws at once (the 1.10 problem turned sideways); tiles of 256 or 1,117 pt
were no better. Hence the 4,096 pt line.

**ADR-0001's fallback, an `NSTableView` with self-drawing rows.** Row views
move like strips, but NSTableView makes them for every row in view and
redraws each row's newly exposed strip on a horizontal scroll; ADR-0001
measured it at about twice the custom grid's main-thread time on wide
files. Not re-measured.

## Decision

Accepted by Rob on 2 October 2026, conditional: **strips for grids up to
1.5 times the visible width (never past 4,096 pt), today's drawing for
wider ones**, switching when the width crosses the line (a column resized,
more columns read, the window resized), with hysteresis: strips return
below 1.4 times the visible width (never past 3,840 pt), so a column
dragged back and forth across the line doesn't switch on every step. Rob
accepted the 1.5 times / 1.4 times line on 3 October 2026 (it refines the
4,096 pt line he accepted first; see "Measured in task 2.0b"). The
follow-up task lands it only once the prototype has tests and an on-screen
check. Wide files keep 2.0a's drawing, where p99 is 2.9 ms at 200 columns
on this Mac (the 3× rule judges only the reference file, but wide files
are already over its number).

## Risks to settle in the follow-up task

From the 2.0a review:

- **Stale or misplaced strips during a fling**: a strip shown before its
  drawing is in, or at the wrong place for a frame (positions are set
  relative to the visible rectangle on every scroll). Check on screen, at
  120 Hz, with fast flings and with the scroller.
- **The in-cell editor** (2.5) and the invalid-bytes callout must sit above
  the strips' view and follow the scroll, IME included (marked text, the
  candidate window's position).
- **Selection and find redraw cost**: a selection or find change redraws
  whole strips (four rows across the grid), not just the cells.
- **Resize and display-scale changes**: a window resize, a column resize,
  and moving the window between a 1× and a 2× display must redraw or
  rescale every strip.
- **Memory at 4,096 pt on Retina**: a strip is 4,096 × 88 pt, about 5.8 MB
  at 2×; with the visible strips, the margin and spares that is about
  80–90 MB, and double that if the layers are extended range (EDR). The
  line may need to be lower, or strips narrower than the grid.
- **Tests and screenshots draw through `cacheDisplay`**, which bypasses the
  strips: they would no longer see what is on screen. The follow-up needs
  a pixel test that renders the strips' rasters and compares them with the
  grid's synchronous drawing (`cacheDisplay`), in light and dark, at 1× and
  2×.
- **VoiceOver frames** (4.2) come from the grid's geometry, which doesn't
  move; check they still line up with what is drawn.
- **Render-server CPU**: more layers (about a dozen strips) for
  WindowServer to composite; measure its CPU and late frames, not only
  Leal's main thread.

## Measured in task 2.0b (answered: accepted by Rob, 2026-10-03)

Rebuilt on 2.0a as landed, same binary, alternated, 1,200 × 784 pt window
on Retina, visible width 1,134 pt (docs/tasks/2.0b.md, "Where strips pay"):

| Grid width | Instructions p50 / p99, strips vs AppKit | Footprint peak, strips − AppKit |
|---|---|---|
| 1,230 pt (reference file) | 2.1 / 6.1 vs 6.4 / 9.7 M | +47 MB |
| 1,691 pt (1.5 × visible) | 2.2–2.3 / 6.9–7.4 vs 6.4 / 9.7 M | +68–74 MB |
| 1,996 pt (1.76 ×) | 2.2–2.3 / 9.5–30.7 vs 5.8–6.4 / 9.8–10.1 M | +83–85 MB |
| 3,720 pt (3.3 ×) | 2.8 / 84 vs 5.9 / 10.8 M | +185 MB |

WindowServer's CPU a frame is about the same with strips (1.97 against
1.85 ms, and 2.20 against 2.40 ms with a search).

**Refinement of the line (accepted by Rob, 2026-10-03)**: strips while the grid is within 1.5
times the visible width and no wider than 4,096 pt; back to strips below
1.4 times (3,840 pt). A strip's cost grows with the grid's width and
AppKit's with the visible width, so a fixed 4,096 pt line costs as much
memory as 1.10's tiles and is slower at p99. Task 2.0b implements this
refinement, and the Decision above says so.

## Consequences

- PLAN: a follow-up task (the coordinator adds it) to land strips with the
  tests and the on-screen check above, the 4,096 pt switch with its
  hysteresis, footprint checked in `just perf` against Rob's run, and the
  in-cell editor's place. It re-measures the comparison above against the
  reviewed 2.0a binary (the numbers here are from 2.0a before its review's
  fixes).
- Phase 2 (2.5, in-cell editing): the editor and the callout are placed
  above the strips' view, not inside the document view.
- DESIGN §2's grid description gains a sentence on strips when it lands.
- The prototype is one commit on branch `task/2.0a-strips`, behind
  `-LealStrips YES`, with tuning options (`-LealStripRows`,
  `-LealStripTileWidth`, `-LealStripSpares`, `-LealStripAhead`). It has no
  tests, keeps column tiles for wide grids instead of switching, predates
  the 2.0a review's fixes, and hasn't been checked on screen beyond the
  benchmark.
