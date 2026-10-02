# 0011 — Grid drawing: strips of rows that scrolling moves

- Status: proposed
- Date: 2026-10-02
- Refines: ADR-0001 (option B: how the cells reach the screen)

## In one minute

**Question for Rob: should the grid draw its cells and row numbers into
strips of rows that scrolling moves, instead of letting AppKit redraw the
visible grid on every scroll step?**

- Task 2.0a brought scrolling within the 3× rule without changing the
  drawing design: on the M5 Pro, main-thread work per frame is now p50
  1.4–1.6 ms and p99 2.2–2.6 ms on the reference file (`just perf`,
  docs/tasks/2.0a.md), against 2.8 ms.
- That margin depends on the clock. The run's main thread was at
  2.6–3.1 GHz; Rob's run was at 2.2–2.4 GHz, where the p99 would be about
  3 ms (an estimate). On a base M1 Air the beta check could fail.
- About half of what is left in each frame is AppKit rebuilding the
  visible grid's and gutter's layers from their recorded drawing. A
  prototype that draws into strips instead (branch `task/2.0a-strips`)
  takes the reference file to **p50 0.93–0.98 ms and p99 1.68–1.90 ms**:
  about 1.5× more headroom.
- Costs: 10–30 MB more footprint while scrolling, wide grids need to keep
  today's drawing, and phase 2's in-cell editor must sit above the strips.
- **Recommendation: accept, with wide grids (more than 4,096 pt) keeping
  today's drawing.** If Rob prefers to wait, the 3× rule passes today on
  this Mac, and the decision can be taken with the base M1 Air's numbers
  in the beta.

## Context

ADR-0001 chose a custom grid (option B): an `NSScrollView` whose document
view draws only the cells AppKit asks for. Task 1.6a found the catch:
AppKit backs a view in a scroll view with a layer the size of the visible
rectangle, and on every scroll step rebuilds it from the view's recorded
drawing (a display list), though only a strip of it is new. It also
redraws the whole visible view about every 1.25 s while scrolling (2.0a).
The gutter, a second scrolled view, pays the same.

1.10 tried tiles of 512 × 256 pt that scrolling moves, and rejected them:
a frame in which a row of tiles came into view drew all of them at once
(ten times the late frames), and they added about 180 MB of footprint. 1.10
noted that tiles small enough that one row of them costs no more than a
strip would need to keep their per-tile overhead low, and left it to an
ADR.

2.0a moved Leal's own per-frame work off the main thread (text layout and
core reads ahead of the scroll, batched drawing). What remains in a frame
is mostly AppKit's:

| Main thread, average frame (Time Profiler, 2.0a) | Time |
|---|---:|
| AppKit and Core Animation rebuilding the visible layers | 0.73 ms (47%) |
| AppKit's display cycle (tracking areas, layout) | 0.21 ms |
| Core Animation commit, other | 0.13 ms |
| Leal's own drawing | 0.25 ms |
| Scrolling, run loop and the rest | 0.25 ms |

## Options

**A. Keep AppKit's drawing (2.0a as landed).** Passes the 3× rule on this
Mac's run, with a thin margin at a low clock. No new code.

**B. Strips of rows (the prototype).**

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
  `cacheDisplay` (snapshots and tests draw them as today). Their own layers
  are left empty (`wantsUpdateLayer`), so AppKit records and replays
  nothing, and its requests to redraw the strip a scroll exposes are
  ignored.
- What is drawn comes from the same code (`GridView.drawContent`), so it is
  the same drawing; only where it lands changes.

Measured against A, same binary, alternating runs, the full scroll
benchmark (vertical flings, horizontal, vertical at the far right):

| | A | B |
|---|---|---|
| Reference file: CPU a frame p50 / p99 | 1.59–1.63 / 2.35–2.58 ms | 0.93–0.98 / 1.68–1.90 ms |
| … instructions p50 / p99 | 5.4–5.5 / 8.5–9.0 M | 2.2 / 5.9–6.5 M |
| … late frames | 0 | 0 |
| … footprint peak / settled | 212 / 87 MB | 224–238 / 86–88 MB |
| 200 columns (column tiles of 512 pt): CPU p50 / p99 | 1.59 / 2.88 ms | 1.15 / 4.07 ms |
| … horizontal flings, p99 | 3.1 ms | 7.3 ms |

On the 200-column file a grid wider than 4,096 pt is cut into columns of
tiles, and a column of them coming into view during a horizontal fling
draws at once, the 1.10 problem turned sideways; tiles of 256 or 1,117 pt
were no better. Hence B's rule below for wide grids.

**C. ADR-0001's fallback (`NSTableView` with self-drawing rows).** Row views
move like strips, but NSTableView makes them for every row in view and
redraws each row's newly exposed strip on a horizontal scroll; ADR-0001
measured it at about twice B's main-thread time on wide files. Not
re-measured; it would lose more than it gains here.

## Decision

Proposed: **B for grids up to 4,096 pt wide, A for wider ones.** The grid
switches when its width crosses the line (a column resized, more columns
read). The reference file and most CSVs are well under it (12 columns are
about 1,300 pt); a wide file keeps 2.0a's drawing, which on this Mac gives
p50 1.6 ms and p99 2.9 ms at 200 columns (the rule judges only the
reference file).

## Consequences

- PLAN: a task to land B, with tests that strips draw what `cacheDisplay`
  draws (the strip transforms are the main risk), the switch for wide
  grids, footprint checked in `just perf` against Rob's run, and the
  in-cell editor's place.
- Phase 2 (2.5, in-cell editing): the editor and the invalid-bytes callout
  must be placed above the strips' view, not inside the document view.
- Phase 4 (4.2, accessibility): unaffected; elements come from geometry.
- DESIGN §2's grid description gains a sentence on strips.
- The prototype is one commit on branch `task/2.0a-strips`, behind
  `-LealStrips YES`, with its tuning options (`-LealStripRows`,
  `-LealStripTileWidth`, `-LealStripSpares`, `-LealStripAhead`). It is not
  for landing as is: it has no tests, keeps column tiles for wide grids
  instead of switching, and hasn't been checked on screen against the
  current drawing beyond the benchmark.
