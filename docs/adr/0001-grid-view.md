# 0001 — Grid view: a custom-drawn grid, with a row-drawing NSTableView as fallback

- Status: accepted
- Date: 2026-09-30
- Approved by Rob

## Recommendation in one minute

*Rob accepted this on 2026-09-30: B, with C as the documented fallback. The
re-measured numbers below were taken after the review, and they still
support it.*

**Recommendation: build Leal's grid as a custom view that draws the visible
cells itself (option B). Keep option C, an `NSTableView` whose rows draw
their own cells, as the documented fallback.**

- **What decides it is one view per cell, not NSTableView as such.**
  NSTableView makes a view for every column of each row it shows, including
  off-screen columns: 7,601 views at 200 columns. Any design with a view per
  cell gets slower as files get wider. Even the tuned version (A-lite) is
  fine at 12 columns but late on 74% of frames at 200.
- **Two designs avoid per-cell views, and both scroll smoothly on this Mac.**
  - **C** keeps NSTableView for rows, the header and columns, but each row
    draws its own cells.
  - **B** draws everything itself.
- **Why B over C: headroom on the machine that matters.** The budget is no
  dropped frames at 120 Hz (8.3 ms a frame) on a base M1 Air.
  - On this M5 Pro, roughly twice as fast per core, C's slowest 1% of
    frames at 200 columns already take 12 ms (fast) to 15 ms (moderate) of
    main-thread time, over the 8.3 ms frame. That is mostly horizontal
    scrolling.
  - B's slowest 1% take 4.5–6.4 ms, about half the frame.
  - On an M1 Air, C would be expected to drop frames on wide files, and B
    has room to spare.
- **What C would give us that B must build:**
  - The native header with sort indicators.
  - Column resize and drag-to-reorder.
  - A row selection model.
  - Row-level accessibility.

  These are real costs for B, listed under *What C keeps that B must
  build*, with accessibility at 2–3 weeks. They are one-off build costs.
  C's lack of headroom is a cost users would feel on every wide file.
- **Memory is mixed.** B keeps Leal's own heap small at every width (about
  15 MB, the same as C). But its total footprint is higher than standard
  NSTableView's at 12 columns: 86 MB against 69 MB settled, and 208 MB
  against 157 MB just after scrolling. It is about the same as C's (84 MB).
  The reason is explained under *Memory and start-up*.
- **How sure this is.** The numbers come from an M5 Pro with the screen
  locked. So compositor cost isn't measured, and the reference machine is
  slower. Ranked by main-thread time in the slowest 1% of frames, the order
  was B < C < A-lite < A in every configuration. Rob can re-check the
  headline on an unlocked Mac in about 3 minutes:

  ```sh
  cd spikes/grid-spike && ./build.sh && ./bench.sh compare
  ```

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

DESIGN §2 assumed `NSTableView` and flagged the risk: it makes views only for
visible rows, but for every column. PLAN 0.4 is the spike that measures this.

## Options

All four draw the same mockup styling. They share the data source, the
row-number gutter and the text-drawing code, so they differ only in how cells
reach the screen.

- **A: `NSTableView` with standard cells.** Each cell is a reusable
  `NSTableCellView` with an `NSTextField`, the standard Apple pattern.
  - It has a native header, alternating rows, grid lines, and VoiceOver text
    for free.
  - Each text field keeps its own layer (Core Animation's unit of drawing):
    1,772 at 12 columns.
- **A-lite: `NSTableView` with light, flattened cells.** Each cell is a small
  view that draws its own text. Each row draws all its cells into one layer
  (`canDrawSubviewsIntoLayer`), leaving 166 layers at 12 columns. There is
  still one view per cell.
- **C: `NSTableView` with self-drawing rows.** There are no cell views. Each
  row view draws its row's visible cells with the same drawing code as B.
  - NSTableView still manages rows, the header, columns and row selection.
  - The in-cell editor is a real text-field cell, created only for the cell
    being edited.
- **B: custom grid.** An `NSScrollView` whose document view draws only the
  cells in the area that needs repainting, with separate header and gutter
  views. The in-cell editor is a standard `NSTextField` placed over the cell.
- **Width-based hybrid (A-lite for narrow files, B for wide ones):** not
  proposed. C already covers every width with a single NSTableView design, so
  two grids would add cost without benefit.
- **Not measured:**
  - Cell-based `NSTableView`, deprecated since macOS 10.10.
  - `NSCollectionView`, which has the same per-cell-view cost as A.
  - SwiftUI `Table` and `Grid`, built on the same machinery with less
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
15,000 pt/s over 10,000 rows. The moderate profile spends proportionally
more time scrolling sideways.

Figures are medians of 3 runs, with all 72 runs in one interleaved session.
They were measured on an M5 Pro MacBook Pro at 120 Hz, with a 1200 × 780
window. The numbers agree with the reviewer's independent re-measurement.

"Late" means a frame that missed at least one 120 Hz refresh. "Main-thread
work" is how much of each frame the app spent on layout, drawing and handing
the frame to Core Animation. **The 8.3 ms frame is the hard limit, and on a
base M1 Air the main-thread numbers would be roughly double.**

### Scrolling

| Grid | Columns | Late frames, fast | Late frames, moderate | 2+ refreshes late, fast | p50 frame, fast | p99 frame, fast | Main-thread work p50 / p99, fast | Main-thread p99, moderate | Jump to last row |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| A | 12 | **35%** | **8.4%** | 621 of 5,128 | 8.3 ms | 26.7 ms | 9.8 / 24.9 ms | 13.8 ms | 30 ms |
| A | 50 | **99%** | **91%** | 1,613 of 1,784 | 31.0 ms | 69.2 ms | 30.3 / 67.6 ms | 54.3 ms | 77 ms |
| A | 200 | **100%** | **100%** | 1,421 of 1,421 | 175.9 ms | 298.1 ms | 174.4 / 294.8 ms | 221.6 ms | 304 ms |
| A-lite | 12 | 0.05% | 0.05% | 1 of 7,557 | 8.3 ms | 8.3 ms | 5.2 / 8.2 ms | 6.9 ms | 8 ms |
| A-lite | 50 | **15%** | **5.6%** | 398 of 6,341 | 8.3 ms | 24.4 ms | 6.5 / 22.2 ms | 21.0 ms | 23 ms |
| A-lite | 200 | **74%** | **47%** | 1,140 of 2,544 | 18.8 ms | 77.9 ms | 16.0 / 75.2 ms | 56.1 ms | 80 ms |
| C | 12 | 0.04% | 0.05% | 1 of 7,558 | 8.3 ms | 8.3 ms | 4.9 / 6.7 ms | 6.3 ms | 8 ms |
| C | 50 | 0.05% | **2.9%** | 1 of 7,690 | 8.3 ms | 8.3 ms | 4.6 / 6.7 ms | 14.0 ms | 17 ms |
| C | 200 | **1.9%** | **12%** | 5 of 8,434 | 8.3 ms | 17.5 ms | 5.4 / **12.0** ms | **14.8** ms | 20 ms |
| B | 12 | 0.03% | 0.06% | 1 of 7,557 | 8.3 ms | 8.3 ms | 2.5 / 4.5 ms | 4.1 ms | 8 ms |
| B | 50 | 0.03% | 0.04% | 1 of 7,692 | 8.3 ms | 8.3 ms | 2.4 / 4.4 ms | 6.8 ms | 8 ms |
| B | 200 | 0.02% | 0.04% | 2 of 8,601 | 8.3 ms | 8.3 ms | 2.5 / **4.5** ms | **6.4** ms | 8 ms |

How to read it:

- **A and A-lite slow down as columns are added, because NSTableView makes a
  view for every column of every visible row.** That is 7,601 cell views at
  200 columns, even with its overdraw clamped to the visible area. Its
  prepared area spans the full 23,560 pt width.
  - Flattening fixes A-lite's layer count (155 layers instead of 1,773), and
    that is why A-lite is fine at 12 columns.
  - It can't remove the per-view cost of making and configuring 200 views a
    row.
- **C meets the budget scrolling vertically at every width,** with
  main-thread p99 about 8 ms or under. Its misses are almost all in
  *horizontal* scrolling: each of the ~38 row views redraws its own newly
  exposed strip.
  - At 200 columns, horizontal main-thread p99 is 13–16 ms, and 31–39% of
    horizontal frames are late in the moderate profile.
  - At 50 columns it is 15–19 ms.
  - This is on a Mac about twice as fast as the reference.
- **B stays under 5 ms p99 scrolling vertically and under 9 ms horizontally,
  at every width.** That is about half of C's main-thread time in the worst
  case. It draws one strip for the whole grid instead of one per row.

### Memory and start-up

| Grid | Columns | Heap after scrolling (budget 40 MB) | Heap peak | Footprint after load | Footprint peak | Footprint 1 s after scrolling | Footprint settled | Layers | Views | Launch → first rows | of which auto-size | Edit commit + redraw |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| A | 12 | 28 MB | 28 MB | 50 MB | 178 MB | 157 MB | 69 MB | 1,773 | 456 cell | 280 ms | 35 ms | 3.4 ms |
| A | 50 | 60 MB | 63 MB | 81 MB | 209 MB | 117 MB | – | 5,693 | 1,900 cell | 504 ms | 134 ms | 6.2 ms |
| A | 200 | **181 MB** | 198 MB | 180 MB | 364 MB | 246 MB | – | 21,181 | 7,600 cell | 1,512 ms | 527 ms | 19.1 ms |
| A-lite | 12 | 17 MB | 18 MB | 55 MB | 270 MB | 171 MB | – | 155 | 457 cell | 220 ms | 35 ms | 1.9 ms |
| A-lite | 50 | 22 MB | 29 MB | 56 MB | 251 MB | 178 MB | – | 155 | 1,901 cell | 348 ms | 134 ms | 2.8 ms |
| A-lite | 200 | 39 MB | 57 MB | 71 MB | 253 MB | 193 MB | – | 155 | 7,601 cell | 828 ms | 538 ms | 3.8 ms |
| C | 12 | 16 MB | 16 MB | 54 MB | 261 MB | 159 MB | 84 MB | 189 | 38 row | 228 ms | 34 ms | 1.8 ms |
| C | 50 | 16 MB | 27 MB | 51 MB | 245 MB | 175 MB | – | 189 | 38 row | 323 ms | 134 ms | 2.0 ms |
| C | 200 | 17 MB | 36 MB | 51 MB | 253 MB | 182 MB | 80 MB | 189 | 38 row | 713 ms | 526 ms | 2.1 ms |
| B | 12 | 15 MB | 26 MB | 53 MB | 241 MB | **208 MB** | **86 MB** | 31 | – | 213 ms | 35 ms | 1.5 ms |
| B | 50 | 15 MB | 24 MB | 53 MB | 239 MB | 211 MB | – | 31 | – | 317 ms | 136 ms | 1.6 ms |
| B | 200 | 15 MB | 24 MB | 53 MB | 247 MB | 212 MB | 88 MB | 31 | – | 705 ms | 548 ms | 1.7 ms |

"Settled" is `footprint(1)` a few seconds after scrolling stopped, from one
extra probe run per configuration.

- **Heap** (Leal's own memory; the budget is 40 MB) is 15–17 MB for B and C
  at every width. A's heap grows with the number of columns, to 181 MB at
  200.
- **Footprint** is everything macOS charges to the app, including the
  graphics buffers that hold drawn pixels. **B's is higher than A's at 12
  columns:** 86 MB against 69 MB settled, and 208 MB against 157 MB just
  after scrolling. B is about the same as C (84 MB).
  - **Why:** B draws into one window-sized layer, and macOS keeps that
    layer's pixels in shared graphics buffers (IOSurfaces). `vmmap` shows
    27 MB of them in B, against about 2.5 MB in A and C. One full-window
    bitmap of the grid area at Retina resolution is 13 MB (1117 × 728 pt ×
    2 × 2 × 4 bytes), so 27 MB is consistent with two of them: one on screen
    and one being drawn.
  - A and C keep their pixels in many small per-view or per-row buffers,
    which `vmmap` counts as ordinary memory instead. That is part of why A's
    heap is larger.
  - While scrolling, every option's footprint rises by 100–200 MB. This is
    Core Animation's pool of recycled buffers. `vmmap` shows most of it as
    *reclaimable* once scrolling stops, and it is larger for B because B
    recycles larger buffers.
  - The locked screen may inflate all of this (see *Caveats*). PLAN 1.6
    re-measures on an unlocked screen.
- **Launch to first rows** is measured from process launch, so it is
  stricter than the "open to first rows" budget. It includes auto-sizing the
  columns from 1,000 rows, which alone costs about 530 ms at 200 columns for
  every option. So first paint must size columns from the first screen only,
  as DESIGN §3.10 P2 already allows. Without that, B and C show first rows
  160–195 ms after launch at any width, including about 70 ms of process
  start-up.
- **Editing:** every option passed the scripted edit in every run. The
  editor opens, the value commits on Return through the normal field-editor
  path, and the cell redraws with its edited triangle. Commit plus redraw is
  under 2.2 ms for B and C, against 19 ms for A at 200 columns (budget:
  16 ms). B's editor is an ordinary `NSTextField` placed over the cell. It
  scrolls with the content and gets undo, input methods and spell checking
  from the system.
- **Large files:** B and A drew correctly at the last rows of 1M-row and
  40M-row files (22M and 880M points down; 40M rows is about the 4 GiB
  limit). A-lite and C drew correctly at the end of 1M rows.

### Where the time goes

These Time Profiler traces are from the first session.

- **A at 12 columns:** about 85% of main-thread time is Core
  Animation creating and drawing a separate layer for every cell view and
  text field. Leal's own code is about 2%.
- **B at 200 columns:** our drawing is about a third of the time. Creating a
  Core Text line for each newly exposed cell is the largest single item
  (20%). That is the obvious thing to cache in PLAN 1.6, and it would help C
  just as much.

## What C keeps that B must build, and what B still lacks

**What C keeps from NSTableView that B has to write:**

- **The header row:** the native `NSTableHeaderView`, with sort indicators
  (mockup 04b) and click-to-sort.
- **Column resize and drag-to-reorder**, with the cursor feedback and
  auto-scroll macOS users expect.
- **A row selection model** (click, shift-click, ⌘-click), and
  `NSTableView`'s row keyboard navigation.
- **Row-level accessibility:** VoiceOver sees a table with rows and column
  headers. C would still need cell-level elements, because its cells aren't
  views.

**What the spike's B doesn't have yet**, as costs for PLAN 1.6 and 1.8
(rough estimates):

| Missing in B | Mockup / PLAN | Estimate |
|---|---|---|
| Header sort indicator and click-to-sort | 04b, 3.3 | 1 day |
| Column resize by dragging the header edge, double-click to fit | 1.6 | 1–2 days |
| Column drag-to-reorder (if wanted in v1) | – | 1–2 days |
| Multi-cell selection: click, shift-click, drag, ⌘A, and drawing a range | 1.8, 2.6 | 2–3 days |
| Keyboard navigation (arrows, Page Up/Down, ⌘↑/⌘↓, Tab), keeping the active cell visible | 1.8 | 1–2 days |
| Invalid-bytes callout anchored to the edited cell | 05b, 2.5 | 0.5–1 day |
| Accessibility (see below) | 4.2 | 2–3 weeks |

Some of this is needed with any option. NSTableView selects rows, not cells
(ADR-0002 q3), so cell selection, cell keyboard navigation and the callout
are custom work in A and C too. The genuine B-only items are the header
behaviour, column resize and reorder, and most of the accessibility work:
about a week, plus two weeks of accessibility.

## Accessibility

**What NSTableView gives for free (A, A-lite, C).** VoiceOver sees a table:

- Rows, column headers from the header view, and row and column counts.
- Row selection announcements and arrow-key row navigation.
- In A, each cell's text read from its text field.

A-lite and C lose the automatic cell text: A-lite's cells must return their
own value, and C has no cell views at all. So C needs cell elements much like
B's.

**What B needs, and C needs most of.** B must implement Apple's
`NSAccessibility` table protocols itself. The items are:

1. **Row and cell elements for visible rows only**, built as lightweight
   `NSAccessibilityElement`s, created lazily and dropped on scroll. Each
   reports its row and column index, its value, and its frame on screen.
2. **The table itself:** the table role, row and column counts that include
   off-screen rows, column headers, the gutter as row headers, and the
   selected cells.
3. **Cell lookup that scrolls on demand.** When VoiceOver asks for the cell
   at row R and column C, or moves beyond the visible area, the grid scrolls
   there and creates the element.
4. **A settable editing path:** setting a cell's value through accessibility
   starts and commits an edit, with the same undo and fidelity rules as
   typing.
5. **Announcements for ragged, edited and invalid cells** (for example
   "missing field", "edited", "contains an invalid byte"), and for find
   results and diagnostics as the user moves.
6. **Custom rotors** for find matches and for diagnostics, so VoiceOver
   users can jump between them as the gutter markers let sighted users.
7. **Notifications** for focus, selection, value and layout changes.
8. **Tests:** audits with Accessibility Inspector and the XCUITest
   accessibility audit, a scripted VoiceOver pass, and full keyboard access.

**Estimate: 2–3 weeks for B**, landing in PLAN 4.2. C would need most of items
1, 3, 4, 5 and 6 as well, because its cells aren't views either. With C the
estimate is about 1.5–2 weeks, so B's extra cost is about one week. Only A
gets most of this free, and A is ruled out on performance.

## Decision

Accepted by Rob on 2026-09-30: **option B, a custom-drawn grid, for every
file width. Option C is the fallback.** If B hits a wall in PLAN 1.6 (for example, accessibility or
column interaction turns out much harder than estimated), switch to C. C
reuses B's cell-drawing code, because C's row view draws cells exactly as B's
grid does, so little work would be lost.

## Consequences

- **PLAN 1.6 (document and grid)** builds B:
  - The drawing parts: a document view that draws visible cells with Core
    Text, header and gutter views, column geometry held as running totals,
    and column resize.
  - First paint sizes columns from the first screen of rows, and the
    1,000-row sizing runs as P2 work.
  - The scrollbar's estimated row count (§3.10) is simply the document view's
    height. A plain tall document view drew correctly at the last rows of 1M
    and 40M rows (22M and 880M points down), so 1.6 keeps a test for that.
  - 1.6 must also:
    - Cache laid-out text for visible cells (the largest item in B's
      profile).
    - Measure footprint and hitches on an unlocked screen, and on a base M1
      Air if one is available.
    - Keep the cell drawing in one place that C could reuse, so the fallback
      stays cheap.
- **PLAN 1.8 (find, go to row, copy, inspector):**
  - Find highlights are drawn by the grid, as in the spike.
  - Go-to-row is scroll arithmetic.
  - Cell selection, multi-cell selection and keyboard navigation are custom
    code (estimates above).
- **PLAN 2.5 (editing):**
  - In-cell editing uses an `NSTextField` overlaid on the cell, as proven in
    the spike. Return commits and Esc cancels through the field editor.
  - The invalid-bytes callout is a small view anchored to the same rectangle.
- **PLAN 3.3:** the sort indicator in the header is B's own drawing.
- **PLAN 4.2 (accessibility):** its scope grows by 2–3 weeks for the items
  listed above. PLAN should say so.
- **DESIGN §2** changes once this is accepted:
  - "Why this split" no longer relies on NSTableView.
  - The "Risk" paragraph is resolved by this ADR.
  - §4.4 notes that grid accessibility is custom.
- **If Rob chooses C instead:**
  - 1.6 builds an `NSTableView` with self-drawing row views.
  - The header, column resize and reorder, and sort indicators come native.
  - Accessibility is 1.5–2 weeks.
  - Wide files need profiling on an M1 Air early, since C has less headroom.
- **The spike** (`spikes/grid-spike/`) is deleted after Rob decides, per
  PLAN 0.4.

## Caveats

- **Faster Mac than the reference.** The spike ran on an M5 Pro (Mac17,8), not
  a base M1 Air, which is roughly half as fast per core (an estimate). This
  is why C's p99 at 200 columns matters: at over 8.3 ms here, it would be
  about twice that on an M1 Air. B's p99 would also rise, towards the budget
  in the fastest flings, which is why 1.6 must cache text and re-measure.
- **The screen was locked** (the owner was away). The app still laid out,
  drew and committed every frame, so main-thread cost is measured. But
  WindowServer was not compositing the window, so:
  - GPU and compositor hitches are not measured.
  - An Instruments *Animation Hitches* trace was not meaningful. *Time
    Profiler* traces were taken instead.
  - Snapshots were rendered from the app's own layers, because
    `screencapture` had no permission.
  - A locked Mac turns its display off after about a minute. The bench runs
    `caffeinate -u` before each run so the display link doesn't stall.

  Compositing cost grows with the number of layers, and A has by far the
  most.
- **Background load.** Other apps were running (Chrome, Cursor, iTerm,
  WindowServer).
  - The 1-minute load average at the start of each run had a median of 4.2,
    and ranged from 2.5 to 24.8. The peak was Spotlight indexing the fresh
    build at the start of the session.
  - The top three processes before each run are in the bench log, and the
    load is recorded for every run in `results.md`.
  - During the reviewer's earlier runs, `mediaanalysisd` used over 200% CPU.
  - Configurations were interleaved so load affected every option alike, and
    the 3 runs of each agree closely.
- **Scrolling was programmatic, not trackpad events,** because synthetic
  input is forbidden. Real trackpad scrolling adds AppKit's "responsive
  scrolling", which can hide a late frame by showing pre-drawn content. It
  can't remove the work.
- **Synthetic data.** Cell text is generated in Swift. The real app asks the
  Rust core through FFI, which adds the same per-row cost to every option.
- **To reproduce the headline yourself** on an unlocked Mac, in about 3
  minutes:

  ```sh
  cd spikes/grid-spike && ./build.sh && ./bench.sh compare
  ```

  It scrolls A, C and B at 200 columns and prints late frames, p99 and
  main-thread time for each.
