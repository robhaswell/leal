# 0002 — UI mockups approved for phase 1–3 app work

- Status: accepted
- Date: 2026-09-30
- Approved by Rob

## Context

`docs/PLAN.md` makes building conditional on Rob approving UI mockups. The
mockups had to cover DESIGN §3.5 (diagnostics banner), §3.10 (first paint
while indexing) and §4 (window, grid, filter bar, cell inspector, status bar,
shortcuts), in light and dark mode, before any AppKit code is written.
ADR-0001 is reserved for the grid spike (PLAN 0.4).

The mockups are a claude.ai design canvas:
https://claude.ai/artifact/EjwLf7oADYjdyBQ96DXFvH (private to Rob). The
exports and an index are in `docs/mockups/`.

## Options

Static HTML/CSS mockups on a canvas versus a throwaway AppKit prototype. The
canvas was chosen: faster to iterate, covers many states cheaply, and leaves
no code to be tempted to keep. Within the mockups, fourteen questions were put
to Rob with the drawn option and an alternative; they are listed below.

## Decision

Rob approved the mockups as drawn on 2026-09-30 ("that looks great. no
notes"). Each open question is resolved the way the mockups draw it:

1. **Font and shading.** System font (SF Pro, 13 px, 22 px rows) by default,
   monospace as an option. Alternate row shading stays as drawn (very light).
2. **Column width.** Columns auto-size from the first 1,000 rows with a
   260 px maximum; wider content clips at the window edge with horizontal
   scrolling.
3. **Selection.** A 2 px accent ring on the active cell and a highlighted row
   number. No column highlight.
4. **Indexing feedback.** Lives in the status bar: "Indexing… about N rows"
   with a small progress bar, no sheet. Jumping past the indexed region shows
   skeleton rows and a floating pill. Option drawn: **row numbers stay blank
   in the skeleton** (not estimated numbers in grey).
5. **Ragged rows.** Option drawn: **shown in the grid**. A short row shows
   hatched empty cells; a long row spills into an extra, dimmed "Column N"
   (not inspector-only).
6. **Text after a closing quote.** Option drawn: **shown raw in the grid,
   exactly as the bytes read** (not the parsed value with raw in the
   inspector).
7. **Banner.** Wording "This file has N kinds of irregularity. It's shown
   exactly as written." with Details and a dismiss button. It stays until
   dismissed; the status-bar badge remains afterwards. Info-level items
   appear in the status bar and popover only, with no gutter marker.
8. **Filtered row numbers.** Option drawn: **original row numbers are kept in
   the gutter** (14, 27, 31…), not renumbered.
9. **Find.** A macOS-style bar under the title bar with a match count and
   Previous/Next. **No Replace in v1.**
10. **Unsaved edits.** Edited-but-unsaved cells carry a small accent-coloured
    corner triangle; the window title shows "— Edited".
11. **Cell inspector.** A bottom pane (about 190 px) showing the selected
    cell's full value in an editable text area. Return inserts a newline
    there and ⌘Return commits; in-cell editing commits on Return. Multiline
    values show on one grid line with a small ↵ between lines. ⌘I toggles the
    inspector.
12. **Invalid bytes.** Editing such a cell shows a callout under it saying
    the invalid byte will be replaced on commit; Esc keeps the original bytes.
13. **Header row (DESIGN §8, "Header row", now decided item 2).** Trust the detection. When no header is
    found, the header cells show 1, 2, 3… in grey. Option drawn: **the toggle
    is in the status bar ("Header row: off")**, not the header corner cell.
14. **UTF-16.** An info banner with a primary "Save As UTF-8…" button, a lock
    glyph beside the title, and "Read-only" in the status bar.

Also settled by the mockups: no toolbar (find, filters and the inspector are
keyboard-first, in the spirit of Tad); system blue is the only accent; banners,
popovers and the find bar follow macOS conventions.

## Consequences

- Phase 1–3 app tasks (1.6, 1.7, 1.8, 2.5, 2.6, 3.3) build to these mockups.
  Each UI task's notes include a screenshot next to the matching image in
  `docs/mockups/README.md`.
- `DESIGN.md` updates to make when convenient: add ⌘I (cell inspector) to
  §4.2; record the header-row answer in §8 (trust detection, status-bar
  toggle); note in §3.5 that ragged rows and text after a closing quote are
  shown in the grid as described above. *(All three are done: §4.2, §8
  item 2, and a pointer to decisions 5 and 6 at the top of §3.5.)*
- The grid spike (0.4, ADR-0001) must be able to draw everything shown here:
  gutter markers, per-cell styling for selection, edits and hatched cells,
  in-cell editors and highlighted find matches.
