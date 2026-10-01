# Leal — Design

Status: draft v0.1 (2026-09-30) · Owner: Rob Haswell

Leal is a fast, low-memory CSV viewer and editor for macOS. It shows delimited
text files in a clean, readable grid, supports filtering and sorting, and edits
cells in place **without changing any byte of the file that the user did not
edit**. The name is old Scots/English for *faithful*; the command is `leal`.

---

## 1. Goals and non-goals

### Goals

1. **Fidelity.** Leal never reformats a file. Saving changes only the bytes of
   the cells, rows or columns the user edited. This is the product's core
   promise and is enforced by tests (see §5).
2. **Performance.** Comfortable with 100 MB files (≈1M rows); usable, if less
   snappy, up to 1 GB.
3. **Low memory.** Memory use is dominated by the file's own pages, which the OS
   can evict, not by Leal's data structures.
4. **Honesty about messy input.** Irregular files are shown exactly as they are,
   with a clear warning. Leal never silently "fixes" anything.
5. **A clean reading interface**, native to macOS.
6. **Public release quality:** signed, notarized, accessible, documented.

### Non-goals (v1)

- Formulas, formatting, charts, multiple sheets, `.xlsx`.
- SQL querying or joins (VisiData and DuckDB already do this well).
- Windows or Linux GUIs. The core stays portable so these remain possible.
- Editing UTF-16 files. v1 opens them read-only with a notice (§4.3).
- Files larger than 4 GiB.

### Performance and memory budgets

Reference machine: base M1 MacBook Air. Reference file: 100 MB, 1M rows × 12
columns, UTF-8, quoted fields containing some newlines.

| Metric | Budget |
|---|---|
| Launch to empty window | < 300 ms |
| Open to first rows visible | < 150 ms, **independent of file size**, before indexing finishes |
| Full index built | < 500 ms |
| Scrolling | No dropped frames at 120 Hz, **including while background work runs** |
| Cell edit to screen | < 16 ms |
| Filter with full scan | < 300 ms |
| Sort on one column | < 1 s |
| Save after one edit | < 500 ms |
| Leal's own heap for the reference file (not counting mapped file pages) | < 40 MB |
| Idle app with no document | < 30 MB resident |

These budgets are checked by benchmarks in CI (core) and Instruments runs
before each release (app). A regression past a budget blocks release.

**First paint and scrolling come first.** Reading the file always wins over
preparing to filter, sort or analyse it. No background work may delay the
first rows appearing or make scrolling stutter. See §3.10 for how this is
enforced.

---

## 2. Architecture

```
┌──────────────────────── Leal.app (Swift, AppKit) ────────────────────────┐
│ NSDocument · grid view · filter bar · diagnostics banner · status bar     │
└───────────────▲──────────────────────────────────────────────────────────┘
                │ generated Swift bindings (UniFFI)
┌───────────────┴─────────── leal-ffi (Rust) ──────────────────────────────┐
│ Thin wrapper: handle objects, async jobs, progress callbacks              │
└───────────────▲──────────────────────────────────────────────────────────┘
┌───────────────┴─────────── leal-core (Rust) ─────────────────────────────┐
│ source · dialect · encoding · index · rows · diagnostics · edits ·       │
│ serialize · view (filter/sort)                                            │
└──────────────────────────────────────────────────────────────────────────┘
                 leal-cli (Rust): `leal` command, uses leal-core
```

- **leal-core** has everything that matters for correctness and performance.
  It has no UI dependencies and is tested on its own.
- **leal-ffi** exposes the core to Swift through UniFFI. It holds no logic.
- **Leal.app** is a thin AppKit layer. Swift code handles presentation and
  macOS integration only. If Swift code starts making decisions about file
  contents, that logic belongs in the core.
- **leal-cli** provides `leal <file>` (opens the app) and `leal check <file>`
  (prints dialect and diagnostics, useful in scripts and bug reports).

### Why this split

- AppKit gives Leal native windows, documents, text editing, menus, dark mode
  and accessibility APIs. Rebuilding that in a Rust GUI toolkit would cost
  months.
- The grid itself is a custom AppKit view that draws only the visible cells
  with Core Text (ADR-0001, option B). The phase 0 spike showed that
  `NSTableView` makes a view for every column of each visible row, so it
  slows down on wide files. The custom grid stayed well inside the frame
  budget at 200 columns, on a faster Mac than the reference (1.6
  re-measures). The fallback, if B hits a wall, is an `NSTableView` whose
  rows draw their own cells (option C), reusing the same drawing code.
- Keeping the core in Rust gives predictable memory use, fast byte scanning,
  and makes the fidelity guarantees testable without a UI.
- Tauri or Electron would reintroduce the memory overhead Leal exists to avoid.

**Cost of the custom grid:** the header, column resize, cell selection,
keyboard navigation and accessibility are Leal's own code rather than
`NSTableView`'s. ADR-0001 lists them with estimates, and PLAN 1.6, 1.8, 2.5,
3.3 and 4.2 carry them.

---

## 3. Core design

### 3.1 Source: getting bytes without copying them

> Open question, see ADR-0005 §7 (proposed): `clonefile` fails across
> volumes, so where the clone lives and what EXDEV means.

On open, the core never reads the whole file into its heap.

1. **Clone** the file into Leal's temporary directory with `clonefile(2)`. On
   APFS this is nearly instant and uses no extra disk space (copy-on-write).
2. **Memory-map** the clone read-only (`memmap2`).

Mapping the *clone* rather than the original solves a real problem: if another
program truncates a mapped file, reading the missing pages crashes the process
(SIGBUS). The clone cannot be changed by other programs, so Leal cannot crash
this way, and it always has a stable snapshot of what it opened.

Fallbacks, when the volume does not support cloning (network shares, exFAT,
some USB drives):
- up to 512 MB: read the file into memory, and show a small status bar note;
- above that: copy to the temporary directory, then map the copy.

Clones are deleted when the document closes. Leftover clones from a crash are
removed at launch.

The original file is watched with a dispatch source (kqueue). If it changes or
is deleted, the window shows a banner with **Reload** and **Keep editing**.
Saving checks the original's identity (inode, size, mtime) against what was
opened, and asks before overwriting a file changed elsewhere.

### 3.2 Dialect and encoding detection

> Exact rules for encoding choice and positions: ADR-0003 (§1, §6, §7) and
> ADR-0004 §11 (the `com.apple.TextEncoding` attribute). The ADRs take
> precedence over this section.
>
> Open questions, see ADR-0005 (proposed): §1 (remembering the delimiter and
> header choice), §4 (encoding at first paint versus the whole-file rule),
> §5 (the supported encodings) and §8 (the UI for changing them).

Detected from the first 64 KB plus samples from the middle and end of the file:

- **Delimiter:** `,` `;` `\t` `|`, chosen by the most consistent field count
  across sampled rows.
- **Quote character:** `"`. Other quote characters are out of scope for v1.
- **Line endings:** LF, CRLF or CR, and whether they are mixed.
- **BOM:** UTF-8, UTF-16 LE/BE.
- **Encoding:** UTF-8 if the file is pure ASCII, or if its valid multibyte
  UTF-8 sequences outnumber its invalid bytes (with an invalid-encoding
  warning for those). Otherwise single-byte, Windows-1252 by default
  (ADR-0003 §1). The user can override.
- **Header row:** heuristic (first row text, later rows typed differently).
  Only affects display.
- **Trailing newline** at end of file: present or not.

Dialect is an *interpretation*. The user can change it (for example, "treat as
semicolon-separated"), which re-indexes the file but never changes its bytes.

For first paint, the dialect is decided from the **first 64 KB only**. The
middle and end samples are checked afterwards in the background. If they
disagree, Leal does not re-lay out the grid under the user; it shows a
suggestion instead ("This file looks semicolon-separated — Switch").

### 3.3 Row index

A single pass over the bytes records where each row starts. The scanner uses
`memchr` to jump between quote, CR and LF bytes, and tracks whether it is
inside a quoted field, so newlines inside quotes do not start a new row.

- Offsets are stored as `u32` for files under 4 GiB, giving 4 MB for 1M rows.
- Indexing runs on a background thread and publishes progress in chunks. The
  UI shows the first rows immediately and the row count grows as it goes.
- Diagnostics (§3.5) are collected during the same pass.

Parallel indexing is not needed at the 100 MB target (a single SIMD-assisted
pass is ~100 ms) and is deliberately left out; quoted newlines make it complex.

### 3.4 Rows and fields

> Exact rules for text after a closing quote and quotes inside it: ADR-0003
> (§2, §3). The ADR takes precedence over this section.

A row is parsed only when needed, into a list of field spans:
`(start, len, quoted)`. Parsing is lenient and precisely defined:

- A quote is special only as the **first byte of a field**. A quote elsewhere
  in an unquoted field is literal text (`a"b` is the value `a"b`).
- Inside a quoted field, `""` is an escaped quote.
- Bytes between a closing quote and the next delimiter (`"a"b,`) are kept as
  part of the field's raw bytes and reported as a diagnostic.
- An unterminated quote runs to end of file, and is reported prominently.

Display values are derived from raw bytes: unquote, unescape, decode. Parsed
rows for the visible area are kept in a small LRU cache.

### 3.5 Diagnostics (messy input)

> What counts as one occurrence, tie-breaks, blank lines and UTF-16 rules:
> ADR-0003 (§4, §5, §7). How ragged rows and text after a closing quote are
> shown in the grid (hatched cells, an extra "Column N", raw text): ADR-0002
> (questions 5 and 6). The ADRs take precedence over this section.

Leal's rule for irregular input: **show it faithfully, warn clearly, never fix
it silently.**

| Kind | Severity | Example |
|---|---|---|
| Unterminated quote | Error | A quote opened and never closed; the rest of the file becomes one cell |
| Ragged rows | Warning | Row has a different field count to most rows |
| Text after closing quote | Warning | `"a"b` |
| Invalid encoding | Warning | Invalid UTF-8 sequences, shown as `�` |
| NUL bytes | Warning | Often means the file is binary or UTF-16 without BOM |
| Mixed line endings | Info | Some rows LF, some CRLF |
| Blank lines | Info | Empty rows in the middle of the file |
| BOM present | Info | Shown in the status bar only |

Each diagnostic records its kind, a count and the first 1,000 locations.

In the UI, a non-modal banner appears when there is any warning or error, for
example: "This file has 2 kinds of irregularity. It's shown exactly as written."
The banner has a details popover listing each kind with **Previous/Next**
navigation. Affected rows have a small marker in the row-number gutter.
Info-level items appear only in the status bar and details view.

Editing a cell that contains invalid bytes replaces those bytes; the edit
field says so before the change is committed.

### 3.6 Edits

> Reverting by value, including cells with invalid bytes: ADR-0004 §9.
>
> Open question, see ADR-0005 §2 (proposed): editing a hatched (missing)
> cell of a short or blank row.

The original bytes are never modified. Edits live in an overlay:

- **Cell edits:** map of `(physical row, column) → new value`.
- **Row structure:** a piece list over rows, each piece either a range of
  original rows or a run of inserted rows. Deleting a row splits a piece.
- **Column structure:** a column map applied when rows are serialized.

Every change is a command storing both the old and new values, in logical
coordinates. This gives undo/redo, and lets undo history survive a save: after
saving, the saved file becomes the new base and the stored values still apply.

Setting a cell back to exactly its original display value removes the edit,
so the original bytes (including their quoting) come back.

### 3.7 Saving

> Edge cases (quoting new fields, end-of-file line endings, ragged rows and
> blank lines, empty rows, BOM-like starts, unterminated quotes, the reopen
> guarantee and remembering guessed encodings): ADR-0004. The ADR takes
> precedence over this section.
>
> Open questions, see ADR-0005 (proposed): §1 (what the reopen guarantee in
> ADR-0004 §10 covers), §2 (saving an edit to a hatched cell) and §3 (exactly
> when a new field is quoted per column).

Saving streams the document out:

1. An **untouched row** is copied byte-for-byte, including its line ending.
2. An **edited row** is rebuilt field by field:
   - untouched fields: original raw bytes, unchanged;
   - edited fields: new value, encoded in the file's encoding, quoted if the
     value needs it (contains delimiter, quote, CR or LF) **or** the original
     field was quoted **or** the file quotes every field;
   - delimiters and the line ending: the row's originals.
3. **New rows** use the file's most common line ending and quoting style.
4. The trailing newline at end of file is kept as it was.

The app saves through `NSDocument`'s safe-save: the core writes to the
temporary URL AppKit provides, which is then swapped in atomically. File
permissions, extended attributes and Finder metadata are preserved.

If a new value cannot be represented in the file's encoding (for example an
emoji in a Windows-1252 file), saving stops with a message naming the cells,
and offers **Save As UTF-8** instead. Leal never drops or substitutes
characters.

### 3.8 Views: filter and sort

Filtering and sorting never reorder the file. They produce a **view**: a list
of physical row numbers to show, in order.

- **Filters:** per column — contains (case-insensitive by default), equals,
  starts/ends with, regex, numeric `< ≤ = ≥ >`, empty, not empty. Combined with
  AND across columns, plus a quick search box across all columns.
- **Sort:** by one or more columns, stable, with numeric-aware comparison when
  a column looks numeric.
- Views are computed on background threads (`rayon`), are cancellable, and
  stream partial results so the grid updates while a scan runs. They follow
  the priority rules in §3.10 and never delay viewing or scrolling.
- Editing works in filtered and sorted views; edits map to physical rows. An
  edited row that no longer matches the filter stays visible, marked, until the
  filter is re-applied, so rows don't vanish mid-edit.

### 3.9 Threading and the FFI boundary

> Open question, see ADR-0005 §6 (proposed): UniFFI doesn't pass Swift task
> cancellation through to Rust, so ADR-0005 proposes explicit cancellation.

- A document is an `Arc`-shared object. Reads for visible cells are synchronous
  and must take under 1 ms. They are safe to call on the main thread.
- Indexing, filtering, sorting and saving are async (UniFFI async functions
  exposed as Swift `async`), with progress callbacks and cancellation.
- The main thread never waits on a long operation. While indexing, the row
  count is "rows indexed so far".

### 3.10 First paint and work priority

> Open question, see ADR-0005 §4 (proposed): ADR-0005 proposes that the
> encoding at P0 comes from the first 64 KB, with the whole-file check
> running later.

Opening a file starts several jobs. They run in a strict priority order, and
lower-priority work must never delay higher-priority work.

| Priority | Work | When | QoS |
|---|---|---|---|
| **P0** | Clone, map, detect dialect from the first 64 KB, parse the first screen of rows, paint | Immediately, before anything else starts | User-interactive |
| **P1** | Row index (§3.3); parsing rows as the user scrolls | Straight after P0 | User-initiated |
| **P2** | Diagnostics details, dialect check on later samples, refined column widths, number detection for alignment | Alongside or after P1 | Utility |
| **P3** | Filter and sort acceleration (below) | Only on first use of filter/sort, or when idle | Utility, lowered to background during scrolling |

Rules:

1. **First paint does not wait for the index.** P0 parses the first rows
   directly from the start of the file. It touches only the first few pages of
   the file.
2. **Scrolling never waits for filter preparation.** Filter and sort
   acceleration structures are never built during open. They are built
   lazily, when the user first opens the filter bar or sorts, or in idle time
   once P1 and P2 have finished.
3. **Background work yields to the user.** While the user is scrolling or
   editing, P3 jobs pause at their next chunk boundary and resume when input
   has been idle for about 250 ms. Jobs work in chunks of at most ~5 ms so
   they can pause quickly.
4. **Separate thread pools.** The index runs on its own thread. P2 and P3 run
   on a `rayon` pool limited to (performance cores − 1) threads, so the main
   thread and the indexer always have a core free.
5. **Scrolling before the index finishes.** The scrollbar is sized from an
   estimated row count (file size ÷ average row length so far), refined as the
   index grows. Scrolling within indexed rows is instant. Jumping past the
   indexed region (⌘↓, go to row) moves the indexer to the front of the queue
   and shows the target as soon as it's reached; at the 100 MB target that
   is under half a second.
6. **Filters work while indexing.** A filter applied before the index
   finishes scans the rows indexed so far and keeps up as more arrive.
7. **Acceleration structures are optional.** Every filter and sort works
   correctly by plain scanning; acceleration only makes repeat operations
   faster. They count toward the memory budget, and are dropped under memory
   pressure and rebuilt on demand.

**Filter and sort acceleration (P3).** Examples, each built per column and
only for columns the user actually filters or sorts:
- parsed numeric values, for numeric comparisons and sorting;
- case-folded text, for case-insensitive contains/equals;
- sort keys, for re-sorting after edits without a full re-parse.

**Measuring it.** The core and app emit `os_signpost` intervals for P0 (open
to first paint) and for every background job, so Instruments shows exactly
what ran when. A CI benchmark opens the reference file with P1–P3 work forced
to run concurrently and asserts first paint is still under 150 ms.

---

## 4. App design

### 4.1 Window

- **Grid:** a custom-drawn view (ADR-0001) with a row-number gutter, sticky
  header row, columns auto-sized (with a maximum width) from the first
  screen of rows at first paint and then from the first 1,000 rows as P2
  work (§3.10), numbers right-aligned, subtle alternate row shading,
  optional monospaced font.
- **Filter bar:** hidden until used (⌘F for find, ⌥⌘F for filters).
- **Cell inspector:** a bottom pane for long or multiline values, with editing.
- **Status bar:** `1,000,000 rows × 12 columns · Comma · CRLF · UTF-8 (BOM)`,
  filter count (`12,345 of 1,000,000`), and the diagnostics indicator.

> Open question, see ADR-0005 §8 (proposed): the status-bar encoding source,
> the **Treat as** delimiter menu, **Reopen with encoding…** and the
> suggestion banners.

### 4.2 Interaction

| Action | Keys |
|---|---|
| Move | Arrows, Page Up/Down, ⌘↑/⌘↓ |
| Edit cell | Return, or start typing |
| Commit / cancel | Return, Tab / Esc |
| Clear cells | Delete |
| Undo / redo | ⌘Z / ⇧⌘Z |
| Find | ⌘F, ⌘G / ⇧⌘G |
| Go to row | ⌘L |
| Copy / paste | ⌘C / ⌘V (TSV on the clipboard, multi-cell paste) |
| Insert / delete row | ⌘↩ / ⌘⌫ |
| Show / hide cell inspector | ⌘I |
| Commit an edit in the inspector (Return inserts a newline there) | ⌘↩ |

### 4.3 Documents

- `NSDocument`-based: Open Recent, window tabs, Save, Save As, Revert, dirty
  indicator, Versions where supported.
- **Autosave-in-place is off.** Leal only writes the file when the user saves.
- UTF-16 files open read-only in v1, with a notice and **Save As UTF-8**.
- Sandbox-compatible from the start (security-scoped access, temp files in the
  container), so a Mac App Store build stays possible.

### 4.4 Accessibility and localization

- Grid, banner and inspector work with VoiceOver and full keyboard access.
  The grid draws its own cells (ADR-0001), so its accessibility is custom:
  lightweight row and cell elements for the visible area, the table
  protocols, rotors and announcements, listed in ADR-0001 (PLAN 4.2).
- Respects Increase Contrast and Reduce Motion.
- All user-facing strings are in a String Catalog, English only for v1.

---

## 5. Fidelity contract and testing

### The contract

> Open question, see ADR-0005 §2 (proposed): F2 for an edit to a hatched
> cell, which would append bytes at the end of that row, if accepted.

- **F1** Save As with no edits writes a byte-identical file.
- **F2** Editing field *(r, c)* changes only that field's bytes. Every other
  byte is identical and in the same order.
- **F3** Undoing all edits, or setting a cell back to its original value,
  restores byte-identical output.
- **F4** No irregular construct in §3.5 is ever normalized on save.
- **F5** Saving never loses or substitutes characters. It succeeds exactly, or
  stops with an explanation.
- **F6** Row and column inserts/deletes change only the bytes of the rows and
  fields involved, plus the delimiters directly next to them.

### Test layers

1. **Unit tests** in each core module.
2. **Corpus tests:** `tests/corpus/` holds small, hand-made files for each
   dialect and each diagnostic kind, plus real-world exports (Excel, Google
   Sheets, Numbers, pandas, PostgreSQL `COPY`). Each has expected dialect,
   row/field counts and diagnostics in a sidecar file.
3. **Property tests** (`proptest`): generators for arbitrary bytes and for
   "CSV-like" files with random delimiters, quoting, line endings, BOMs, ragged
   rows and invalid UTF-8. They check F1–F6 using a helper that asserts output
   equals `prefix + new bytes + suffix`.
4. **Fuzzing** (`cargo-fuzz`) of the indexer, row parser and serializer. Run
   nightly in CI; crashes become corpus tests.
5. **Benchmarks** (`criterion`) for the §1 budgets, with regression alerts.
6. **App tests:** XCTest for document lifecycle and save paths; XCUITest for
   the main editing flows.

**Rule:** fidelity tests are never weakened to make a change pass. A change
that needs a contract change needs an ADR first.

---

## 6. Distribution

- Universal binary (Apple silicon and Intel), macOS 14 Sonoma or later.
- Developer ID signed and notarized DMG on GitHub Releases, built by GitHub
  Actions.
- Updates via Sparkle. No other network access and no telemetry.
- Homebrew cask (first in `robhaswell/homebrew-tap`, then `homebrew/cask`).
  The cask links the bundled `leal` command.
- Free and open source.

---

## 7. Repository layout

```
leal/
├── Cargo.toml              workspace
├── rust-toolchain.toml
├── justfile                task runner: check, test, bench, run, release
├── crates/
│   ├── leal-core/          the engine (everything in §3)
│   ├── leal-ffi/           UniFFI wrapper for Swift
│   ├── leal-cli/           the `leal` command
│   ├── leal-testkit/       test-only: corpus loader, oracles, proptest
│   │                       strategies (a dev-dependency, never shipped)
│   └── uniffi-bindgen/     host-only tool that generates the Swift bindings
├── app/
│   ├── project.yml         XcodeGen spec (the .xcodeproj is generated)
│   ├── Sources/
│   ├── Resources/
│   └── Tests/              XCTest
├── tests/corpus/           hand-made files with expected-result sidecars
├── fuzz/                   cargo-fuzz targets (PLAN 2.7)
├── spikes/                 throwaway experiments (the grid spike, until PLAN 1.6)
├── docs/
│   ├── DESIGN.md           this file
│   ├── PLAN.md             build plan and task status
│   ├── adr/                architecture decision records
│   ├── mockups/            approved UI mockups (ADR-0002)
│   └── tasks/              notes written by each task
└── .github/workflows/
```

---

## 8. Decisions and open questions

The approved UI is recorded in ADR-0002, with mockups in `docs/mockups/`.
Where a mockup and this document disagree, the ADR wins and this document is
updated.

Decided:

1. **License:** MIT OR Apache-2.0. The repo is public.
2. **Header row:** trust the detection. When no header is detected, the
   header shows 1, 2, 3… in grey, and a "Header row" toggle in the status bar
   switches it (ADR-0002, question 13).

Open:

3. **Minimum macOS.** Draft assumes 14 Sonoma.
4. **Mac App Store** as well as direct download? The design keeps it possible.
