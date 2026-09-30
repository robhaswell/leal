# 0001 — Grid view: a custom-drawn grid, not NSTableView

- Status: proposed
- Date: 2026-09-30

## Recommendation in one minute

*Decision needed from Rob before phase 1 UI work (PLAN 0.4): approve option B,
or ask for more evidence.*

**Build Leal's grid as a custom view that draws only the visible cells
(option B below), not with AppKit's `NSTableView`.**

- **Wide files are unusable with NSTableView.** At 200 columns every frame
  is late, and a typical frame takes 180 ms (about 5–6 frames a second
  against a 120 Hz target). It builds a view for every column of every row
  that scrolls into view, including columns that are off-screen. The custom
  grid stays at 120 Hz: 99.6% of frames are on time.
- **NSTableView misses the budget even for the reference file.** At 12
  columns it misses 51–77% of frames while scrolling, depending on speed.
  Even the leanest NSTableView I could build misses 20–41%. The custom grid
  misses 0.1% or fewer.
- **The custom grid has headroom.** It uses about 3–4 ms of the 8.3 ms frame
  budget in a typical frame and under 8 ms in its slowest 1%, whatever the
  width. That leaves room for the slower reference Mac and for background
  indexing (DESIGN §3.10).
- **It keeps Leal's own memory small.** Its heap is about 10 MB at every
  width. NSTableView's heap grows with the column count, to about 175 MB at
  200 columns, against a 40 MB budget.
- **The cost is accessibility and a few built-ins.** We must build the
  VoiceOver table support ourselves (about a week, part of PLAN 4.2), plus
  the header, column sizing and hit-testing that NSTableView gives for free.
  Cell-level keyboard navigation and editing are custom work either way.
- **No hybrid.** NSTableView fails at 12 columns too, so a "table for narrow
  files, custom for wide" split would mean two grids and no benefit.

The numbers were taken on a much faster Mac than the reference machine, with
the screen locked (see *Caveats*). Neither changes the verdict: NSTableView
misses the budget by 1.5× to 20× here, and it would be slower still on a base
M1 Air.

## Context

The grid is the product. DESIGN §1 sets the budgets it must meet on the
reference machine (base M1 MacBook Air, 1M rows × 12 columns):

| Budget | Value |
|---|---|
| Scrolling | No dropped frames at 120 Hz (8.3 ms a frame), including while background work runs |
| Open to first rows visible | < 150 ms, before indexing finishes |
| Cell edit to screen | < 16 ms |
| Leal's own heap (excluding mapped file pages) | < 40 MB |

Leal also promises to handle wide files (hundreds of columns) and to draw
everything in the approved mockups (ADR-0002): a row-number gutter, a sticky
header, auto-sized columns, right-aligned numbers, alternate row shading, an
active-cell ring, edited-cell triangles, hatched cells for ragged rows,
find-match highlights and an in-cell editor.

DESIGN §2 assumed `NSTableView` and flagged the risk: it virtualizes rows
(it only makes views for visible rows) but not columns. PLAN 0.4 is the spike
that measures this.

## Options

- **A: view-based `NSTableView`.** One table column per CSV column. Each cell
  is a reusable `NSTableCellView` with an `NSTextField`, the standard Apple
  pattern. It has a native sticky header, alternate row colours, grid lines,
  VoiceOver support and row keyboard navigation.
- **A-lite: `NSTableView` with self-drawing cells.** The same table, but each
  cell view draws its own text with Core Text instead of holding an
  `NSTextField`. This is the leanest NSTableView I could build while keeping
  the table. I also tried the usual "flatten layers" optimisation
  (`canDrawSubviewsIntoLayer` on row and cell views); it made no difference,
  because NSTableView and NSTextField keep their own layers.
- **B: custom grid.** An `NSScrollView` whose document view is sized to the
  whole table and draws only the cells in the region that needs repainting,
  using Core Text. The header and row-number gutter are small separate views
  that follow the scroll position. The in-cell editor is a standard
  `NSTextField` placed over the cell.
- **Hybrid: A for up to about 50 columns, B for wider files.** Only worth
  its cost (two grids to style, edit and make accessible) if A met the budget
  for ordinary files. It doesn't, so this option is shown for completeness.
- **Not measured, and why:**
  - Cell-based `NSTableView` has been deprecated since macOS 10.10.
  - `NSCollectionView` has the same one-view-per-cell cost as A.
  - SwiftUI `Table` and `Grid` are built on the same machinery, with less
    control.

## Measurements

The spike app is in `spikes/grid-spike/`, with method details in
`docs/tasks/0.4.md` and every run in `spikes/grid-spike/results.md`. It has
1,000,000 synthetic rows at 12, 50 and 200 columns, generated on demand, with
all the mockup styling switched on and one in-cell editor open. The app
scrolls itself frame by frame from a display link:

1. Vertical flings.
2. Horizontal flings to the far right.
3. More vertical flings while scrolled right.
4. Horizontal flings back.
5. A jump to the last row.
6. An edit committed with Return.

There were two speeds. *Fast* is 60,000 pt/s flings over 50,000 rows, where
most of the screen changes every frame at the peak. *Moderate* is
15,000 pt/s over 10,000 rows, about 6 rows per frame at the peak. Figures are
medians of 3 runs. They were measured on an M5 Pro MacBook Pro at 120 Hz,
with a 1200 × 780 window.

"Late frames" means frames that missed at least one 120 Hz refresh: the next
frame came 16.7 ms or more after the last one instead of 8.3 ms. "Main-thread
work" is how much of each frame the app spent on layout, drawing and handing
the frame to Core Animation. It must stay under 8.3 ms.

### Scrolling

| Grid | Columns | Late frames, fast | Late frames, moderate | Of the fast ones, 2+ refreshes late | Typical frame (p50), fast | Slowest 1% (p99), fast | Main-thread work p50 / p99, fast | Jump to last row |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| A | 12 | **77%** (2,665 of 3,463) | **51%** | 1,136 | 18.3 ms | 32.6 ms | 15.4 / 31.5 ms | 34 ms |
| A | 50 | **100%** | **98%** | 1,446 | 53.5 ms | 82.8 ms | 51.8 / 77.7 ms | 76 ms |
| A | 200 | **100%** (1,421 of 1,421) | **100%** | 1,421 | 180 ms | 313 ms | 177 / 312 ms | 316 ms |
| A-lite | 12 | 41% | 20% | 122 | 8.3 ms | 22.3 ms | 11.0 / 19.2 ms | 20 ms |
| A-lite | 50 | 88% | 73% | 1,362 | 21.6 ms | 47.5 ms | 18.7 / 44.3 ms | 42 ms |
| A-lite | 200 | 100% | 100% | 1,426 | 101 ms | 264 ms | 99 / 262 ms | 142 ms |
| B | 12 | 0.04% (3 of 7,556) | 0.1% | 2 | 8.3 ms | 8.3 ms | 2.6 / 6.2 ms | 8.3 ms |
| B | 50 | 0.01% (1 of 7,694) | 0.1% | 1 | 8.3 ms | 8.3 ms | 2.6 / 6.1 ms | 8.3 ms |
| B | 200 | 0.4% (37 of 8,559) | 0.04% | 6 | 8.3 ms | 8.3 ms | 4.1 / 7.5 ms | 8.3 ms |

The target is every frame at 8.3 ms with main-thread work under 8.3 ms. A
row that is late 100% of the time is running at a fraction of 120 Hz: A at
200 columns manages about 5–6 frames a second. The fewer frames A has in the
same scroll, the worse it is, because each frame took longer. The "jump to
last row" column is the frame after ⌘↓ on a 1M-row file.

### Memory and start-up

Medians of the 3 fast-profile runs. "Settled" is `footprint(1)` a few
seconds after scrolling stopped, from one extra probe run per configuration.

| Grid | Columns | Heap after scrolling (budget 40 MB) | Heap peak | Footprint after load | Footprint peak while scrolling | Footprint settled | Launch → first rows | of which column auto-size |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| A | 12 | 23 MB | 23 MB | 43 MB | 176 MB | 58 MB | 237 ms | 35 ms |
| A | 50 | 55 MB | 58 MB | 74 MB | 216 MB | – | 489 ms | 136 ms |
| A | 200 | **177 MB** | 196 MB | 174 MB | 373 MB | 257 MB | 1,516 ms | 537 ms |
| A-lite | 12 | 15 MB | 15 MB | 36 MB | 167 MB | – | 203 ms | 35 ms |
| A-lite | 50 | 20 MB | 24 MB | 47 MB | 180 MB | – | 327 ms | 135 ms |
| A-lite | 200 | 42 MB | 60 MB | 71 MB | 207 MB | – | 882 ms | 536 ms |
| B | 12 | **10 MB** | 22 MB | 49 MB | 261 MB | 82 MB | 177 ms | 35 ms |
| B | 50 | 10 MB | 19 MB | 49 MB | 265 MB | – | 275 ms | 135 ms |
| B | 200 | **11 MB** | 19 MB | 49 MB | 275 MB | 86 MB | 688 ms | 547 ms |

- **Heap** is what the "Leal's own heap < 40 MB" budget measures. B's is
  about 10 MB at every width. A's grows with the number of columns, because
  it keeps a view for every column of every visible row (7,600 cell views at
  200 columns). At 200 columns A's heap is over four times the budget
  before any file data is loaded.
- **Footprint** is everything macOS charges to the app, including the
  graphics buffers for drawn pixels.
  - While scrolling, every option's footprint rises by 130–230 MB. This is
    Core Animation's pool of drawing surfaces, and `vmmap` shows most of it
    as *reclaimable* once scrolling stops.
  - Settled, B holds 82–86 MB, of which about 27 MB is drawing surfaces.
    That is about 25 MB more than A at 12 columns, and a third of A at 200
    columns.
  - The locked screen may inflate these surface numbers (see *Caveats*).
    PLAN 1.6 should measure on an unlocked screen. If B's surfaces are
    real, drawing into a window-sized surface instead of the tall document
    view would cap them.
- **Launch to first rows** is measured from process launch, so it is
  stricter than the "open to first rows" budget. It includes auto-sizing the
  columns from 1,000 rows. That alone costs about 540 ms at 200 columns
  (200,000 text measurements) for every option. So first paint must size
  columns from the first screen only and refine later, as DESIGN §3.10 P2
  already allows. Without it, B shows first rows about 140 ms after launch at
  any width. A at 200 columns takes about 1 s, because it builds 200 views
  per visible row.

### In-cell editing

All options passed the scripted edit in every run: the editor opens, the
value commits on Return through the normal field-editor path, and the cell
redraws with its edited triangle. Commit plus redraw took 0.9–1.5 ms in B,
against 3–5 ms in A at 12–50 columns and about 17 ms in A at 200 columns
(budget: 16 ms).

For B, the editor is an ordinary `NSTextField` added as a subview of the grid
at the cell's rectangle. It scrolls with the content and gets undo, input
methods and spell checking from the system field editor. This is the approach
PLAN 2.5 should use. The invalid-bytes callout in mockup 05b is an ordinary
view anchored to the same rectangle.

### Where A's time goes

A Time Profiler trace of A at 12 columns shows about 85% of main-thread time
in Core Animation's commit: creating, drawing and uploading a separate
backing layer for every cell view and text field. Only about 2% is Leal's own
code (data plus configuring cells). So this isn't a tuning problem in the
spike. It is how view-based NSTableView works, and it scales with visible
rows × all columns. At 12 columns the window already holds 1,772 layers, and
at 200 columns it holds 21,180. B holds 31 at any width.

In B's trace (200 columns), our own drawing is about a third of main-thread
time. Creating a Core Text line for each newly exposed cell is the largest
single item (20%). That is the obvious thing to cache in PLAN 1.6.

## Accessibility

**What A gives for free.** NSTableView exposes itself to VoiceOver as a
table:

- Rows and cells, column headers from the header view, and row and column
  counts.
- Text read from each cell's text field.
- Row selection announcements and arrow-key row navigation.

Leal selects *cells*, not rows (ADR-0002 q3), so even with A we would write
our own cell focus, cell keyboard navigation and "focused cell changed"
announcements. A-lite also loses the automatic text; each cell must return
its value itself, which is easy.

**What B needs.** The grid must implement Apple's `NSAccessibility` table
protocols itself:

- **The grid view** takes the table role and reports its row and column
  counts, visible rows, column header elements, the gutter as row headers,
  and the selected cells.
- **Lightweight row and cell elements** (`NSAccessibilityElement`) are
  created lazily for visible rows. Each reports its row and column index, its
  value, and its frame on screen.
- **The cell-for-row-and-column lookup** that VoiceOver uses for table
  navigation.
- **Notifications** when the focused cell, the selection or a value changes,
  and on scrolling.
- **Testing** with VoiceOver, Accessibility Inspector and the XCUITest
  accessibility audit.

**Estimate.** About 4–6 days for B, against about 1–2 days of cell-focus work
that A would also need. So the extra cost of B is about **one working week**,
landing in PLAN 4.2. This is well-trodden ground: spreadsheets and code
editors on macOS do the same.

## Decision

Proposed: **option B, a custom-drawn grid**, for every file width. We would
not use NSTableView for the grid, and would not build a hybrid.

## Consequences

- **PLAN 1.6 (document and grid)** builds the custom grid:
  - A document view that draws visible cells with Core Text, a header view, a
    gutter view, and column geometry held as running totals, for fast "which
    columns are visible" lookups.
  - First paint sizes columns from the first screen of rows, and the
    1,000-row sizing runs as P2 work.
  - The scrollbar's estimated row count (§3.10) is simply the document view's
    height.
  - The spike checked drawing at the last rows of 1M-row and 40M-row files
    (22M and 880M points down): rows, gutter, ring and editor all line up.
    So a plain tall document view is fine up to the 4 GiB limit, and 1.6
    should keep a test for it.
  - 1.6 must also:
    - Measure footprint on an unlocked screen.
    - Cache laid-out text for visible cells.
    - Add Instruments hitch runs on a base M1 Air if one is available.
  - The grid is about 600–900 lines of Swift, more than an NSTableView
    wrapper, but all of it is presentation code.
- **PLAN 1.8 (find, go to row, copy, inspector):**
  - Find highlights are drawn by the grid, which the spike already does.
  - Go-to-row is scroll arithmetic.
  - Cell selection, keyboard navigation (arrows, Page Up/Down, ⌘↑/⌘↓) and
    copy are custom code. They would have been largely custom with
    NSTableView too, since it selects rows, not cells.
- **PLAN 2.5 (editing):** in-cell editing uses an `NSTextField` overlaid on
  the cell, as proven in the spike. Return commits and Esc cancels through the
  field editor. `NSUndoManager` integration is unchanged.
- **PLAN 4.2 (accessibility):** add implementing the `NSAccessibility`
  table, row and cell elements for the grid, about one extra week, plus a
  VoiceOver test pass. The task's scope in PLAN should say so.
- **DESIGN §2** changes once this is accepted:
  - "Why this split" no longer relies on NSTableView.
  - The "Risk" paragraph is resolved by this ADR.
  - §4.4 notes that grid accessibility is custom.
- **The spike** (`spikes/grid-spike/`) is deleted after Rob decides, per
  PLAN 0.4.

## Caveats

- **Faster Mac than the reference.** The spike ran on an M5 Pro (Mac17,8).
  A base M1 Air is roughly half as fast per core. B's busiest frames
  (p99 6–7.5 ms here) could then exceed 8.3 ms in the fastest flings.
  PLAN 1.6 has room to cut that, for example by caching laid-out text for
  visible cells, redrawing the gutter only where rows change, and doing less
  per frame during horizontal scrolling. A would be about twice as far over
  budget as it is here.
- **The screen was locked** (the owner was away). The app still laid out,
  drew and committed every frame, and the display link ran at 120 Hz, so
  main-thread cost, which is what separates A from B, is measured. But macOS
  was not compositing the window. So:
  - GPU and compositor hitches are not captured.
  - An Instruments *Animation Hitches* trace would not be meaningful and was
    not taken. A *Time Profiler* trace was taken instead.
  - Screenshots with `screencapture` were refused (no screen-recording
    permission). Snapshots were rendered from the app's own layers instead.

  Compositing would add cost to A, which has hundreds to thousands of layers
  (1,770 at 12 columns, 21,000 at 200), much more than to B (31 layers).
- **Scrolling was programmatic, not trackpad events.** Real trackpad
  scrolling uses AppKit's "responsive scrolling", which can show pre-drawn
  rows while the main thread catches up. That would hide some of A's misses
  as briefly blank or stale rows rather than stutter. It can't create the
  missing work time, and we were told not to send synthetic input.
- **Synthetic data.** Cell text is generated in Swift. The real app will ask
  the Rust core through FFI, which adds per-row cost to every option equally.
