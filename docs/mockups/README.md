# UI mockups

Approved by Rob on 2026-09-30 with no changes (ADR-0002). The source canvas
is at https://claude.ai/artifact/EjwLf7oADYjdyBQ96DXFvH (private; each
artboard there has dark-mode and monospace tweaks). Images are 2× PNG
exports of a 1200 × 780 window.

When a UI task lands, its task notes include a screenshot of the running app
next to the matching image here.

| Image | Shows | DESIGN | PLAN tasks |
|---|---|---|---|
| `01a-clean-file-light.png` | Clean file: gutter, sticky header, auto-sized columns, right-aligned numbers, alternate row shading, selected cell, status bar | §4.1 | 1.6 |
| `01b-clean-file-dark.png` | The same window in dark mode | §4.1, §4.4 | 1.6, 4.2 |
| `02a-opening-first-paint.png` | Large file just after first paint: rows visible, status bar shows the estimated row count and indexing progress, scrollbar sized from the estimate | §3.10 | 1.3a, 1.6 |
| `02b-opening-jump-past-index.png` | ⌘↓ past the indexed region: skeleton rows and a floating status pill until the target row arrives | §3.10 | 1.3a, 1.6 |
| `03a-messy-file-banner.png` | Messy file: non-modal banner, gutter markers, hatched missing cell (row 4), extra "Column 8" (row 9), invalid UTF-8 shown as � (row 11), text after a closing quote shown raw (row 17), status-bar badge | §3.5 | 1.5, 1.7 |
| `03b-messy-file-details.png` | Details popover: one entry per kind with count and Previous/Next; info-level items listed without navigation; current row selected | §3.5 | 1.7 |
| `04a-find.png` | Find bar (⌘F) with match count, Previous/Next, Done; matches highlighted, current match stronger and its row selected | §4.1, §4.2 | 1.8 |
| `04b-filter-and-sort.png` | Filter bar (⌥⌘F) with column/operator/value chips, "+ Filter", sort chevron on the sorted column, original row numbers kept in the gutter, "1,204 of 1,000,000 rows" | §3.8, §4.1 | 3.3 |
| `05a-inspector-multiline-edit.png` | Cell inspector pane editing a multiline value; edited-but-unsaved cells carry a blue corner triangle; window title shows "— Edited" | §4.1, §4.3 | 1.8, 2.5 |
| `05b-in-cell-edit-invalid-bytes.png` | In-cell editor on a cell with an invalid byte, with the callout that explains the replacement before commit | §3.5, §4.2 | 2.5 |
| `06a-utf16-read-only.png` | UTF-16 file: info banner with "Save As UTF-8…", lock glyph beside the title, "Read-only" in the status bar | §4.3 | 1.2, 2.3 |
| `06b-no-header-row.png` | No header detected: header cells show 1, 2, 3… in grey; status-bar "Header row: off" toggle | §8 item 2 | 1.6 |
| `06c-keyboard-shortcuts.png` | Keyboard shortcut reference (§4.2, including ⌘I for the inspector) | §4.2 | 1.8, 4.4 |
