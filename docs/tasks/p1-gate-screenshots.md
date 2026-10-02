# Phase 1 gate: screenshots of the UI with no mockup

ADR-0005 decision 8 skipped a mockup round for the interpretation controls
and the banners, on the condition that Rob sees screenshots at the phase 1
gate. 1.7 and 1.9 deferred them to the gate (phase 1 review, cons-11). These
are the states that had none.

## How they were made

They are offscreen renders of the running Debug app, taken with
`just snapshot`, at 1× and a 1000 × 620 content size, in light and dark. No
input was sent to the system. The files are synthetic: invented company
names, `.example` e-mail addresses, and nothing from a real file.

- **Menus and the sheet are drawn by the scripted run.** A menu or a sheet
  is a window of its own, and those don't draw offscreen. So, as for 1.7's
  details popover, the run draws their content on a plain panel where they
  would be. The menu panel draws the real `NSMenu`'s items (title, check
  mark, greyed header) in the menu font. The sheet is the real Go to Row
  `NSAlert`, laid out and drawn into the window under the title bar. On
  screen they have the system's menu and sheet chrome.
- **The drive banners use the core's test hooks**
  (`-LealSimulateFault`). The file is opened as if it were on a removable
  drive that vanishes, or whose file changes, part-way through the copy. The
  hosted `RemovableDriveTests` show the same banner on a real disk image
  (cons-12).
- **The changed and deleted banners** come from a copy of the file that
  the script changes (rewrites, as another app's save) or deletes six
  seconds after Leal opens it.
- A button's primary tint looks grey in an offscreen render of an inactive
  window, as in 1.6's 06a shot.

New options for the scripted run (`ScriptedRun`, `LEAL_BENCH` builds only):
`-LealMenu treatAs|reopen`, `-LealGoToRow YES`, `-LealFindWrap YES`,
`-LealWaitForReview YES` and, in Debug, `-LealSimulateFault
disconnect:<byte>|change:<byte>`. `-LealWaitForChange YES` also waits for a
disconnected drive. For example:

```sh
just snapshot orders.csv out.png -LealWindowSize 1000x620 -LealAppearance dark -LealMenu treatAs
just snapshot orders-on-usb.csv out.png -LealSimulateFault disconnect:300000 -LealSnapshotEarly YES -LealWaitForChange YES
```

## The status bar's menus (ADR-0005 decision 8)

| | Light | Dark |
|---|---|---|
| **Treat As**, from the delimiter | ![](p1-gate-menu-treat-as-light.png) | ![](p1-gate-menu-treat-as-dark.png) |
| **Reopen with Encoding**, from the encoding (the encodings the file's BOM allows) | ![](p1-gate-menu-reopen-encoding-light.png) | ![](p1-gate-menu-reopen-encoding-dark.png) |

## The suggestions (DESIGN §3.2, ADR-0005 decision 4)

The review found what the first 64 KB couldn't show. Each file also has an
irregularity, so the diagnostics banner shows above the suggestion: rows
read with commas are ragged, and the late Windows-1252 byte isn't valid
UTF-8.

| | Light | Dark |
|---|---|---|
| Delimiter: "This file looks semicolon-separated." **Switch** | ![](p1-gate-suggestion-delimiter-light.png) | ![](p1-gate-suggestion-delimiter-dark.png) |
| Encoding: "This file looks like Windows-1252." **Reopen as Windows-1252** | ![](p1-gate-suggestion-encoding-light.png) | ![](p1-gate-suggestion-encoding-dark.png) |

## Removable drives (ADR-0006, 1.1a)

| | Light | Dark |
|---|---|---|
| Drive disconnected part-way: **Save As…**, "Drive disconnected" in the status bar | ![](p1-gate-drive-disconnected-light.png) | ![](p1-gate-drive-disconnected-dark.png) |
| Changed while reading: **Reload**, "Changed while reading" | ![](p1-gate-changed-while-reading-light.png) | ![](p1-gate-changed-while-reading-dark.png) |

While the drive is away, the status bar still says "Indexing… about N
rows", with its progress bar stopped. That is as built in 1.7: the drive
may come back and the index carry on.

## The file changed or deleted elsewhere (task 1.9)

These replace 1.9's shots, which had no dark deleted banner.

| | Light | Dark |
|---|---|---|
| Changed on disk: **Reload**, **Keep Editing** | ![](p1-gate-changed-on-disk-light.png) | ![](p1-gate-changed-on-disk-dark.png) |
| Deleted: **Save As…**, **Keep Editing** | ![](p1-gate-deleted-light.png) | ![](p1-gate-deleted-dark.png) |

## Find and Go to Row (task 1.8)

| | Light | Dark |
|---|---|---|
| **Go to Row** (⌘L) | ![](p1-gate-go-to-row-light.png) | ![](p1-gate-go-to-row-dark.png) |
| Find's "wrapped" sign, after ⇧⌘G went back past the first match to the last | ![](p1-gate-find-wrapped-light.png) | ![](p1-gate-find-wrapped-dark.png) |

The Go to Row sheet shows the generic icon: Leal has no app icon yet.

## Not shown here

The phase 1 review's list also named the unsupported or unreadable
attribute notes, the Memory, Copy and Reading notes, the "Drive not
connected" note and File ▸ Reload from Disk. These are status bar text and a
menu item with no layout of their own. The notes are covered by the status
bar tests (`StatusText`), and "Working from a copy" shows in the hosted
removable-drive test.
