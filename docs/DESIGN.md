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
| Launch, to the end of `applicationDidFinishLaunching` | < 300 ms |
| Launched with a file, to its first rows (the cold open) | < 450 ms |
| Open to first rows visible, in a running app | < 150 ms, **independent of file size**, before indexing finishes |
| Full index built | < 500 ms |
| Scrolling | No dropped frames at 120 Hz, **including while background work runs** |
| Cell edit to screen | < 16 ms |
| Filter with full scan | < 300 ms |
| Sort on one column | < 1 s |
| Save after one edit | < 500 ms |
| Leal's own heap for the reference file (see below) | < 40 MB |
| Idle app with no document | < 30 MB physical footprint |

What the budgets mean (decided by Rob at the phase 1 gate, 2026-10-02):

- **Launch** is measured from the process starting to the end of
  `applicationDidFinishLaunching`. Leal opens no empty window, so that is
  when it can take File ▸ Open.
- **Open** is judged warm: a file opened in an app that is already
  running. The cold open, when the app is launched with a file, counts as
  part of launch: launch with a file to its first rows, against the launch
  and open budgets together (< 450 ms).
- **Heap** is every malloc zone minus the per-window AppKit baseline
  (about 21 MB, measured with a two-row file open). Search results count.
  AppKit's brief drawing peaks while scrolling don't. Mapped file pages
  aren't in the heap.
- **Search memory** of 12 bytes per matching row is accepted for v1: 12 MB
  on the reference file when every row matches.
- **Idle memory** is the physical footprint, which Activity Monitor shows
  as Memory. Resident size (RSS) also counts shared system libraries.
- **The 3× rule.** The base M1 Air stays the design target. None is
  available, so the faster Mac the budgets are measured on (an M5 Pro)
  must show about 3× headroom: main-thread work per scroll frame of at
  most about 2.8 ms (a third of a 120 Hz frame), as well as no dropped
  frames. It is checked at p50 and p99 (Rob, 2026-10-02), because a slow
  1% of frames is what shows as stutter. An Air is checked during the beta.

These budgets are checked by benchmarks in CI (core) and by `just perf`
and Instruments runs before each release (app; docs/perf.md). A regression
past a budget blocks release.

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

On open, the core never reads the whole file into its heap.

1. **Clone** the file with `clonefile(2)` into a temporary folder **on the
   file's own volume** (`FileManager.url(for: .itemReplacementDirectory, …,
   appropriateFor:)`), because `clonefile` only works within one volume
   (ADR-0005 decision 7). On APFS this is nearly instant and uses no extra
   disk space (copy-on-write).
2. **Memory-map** the clone read-only (`memmap2`).

Mapping the *clone* rather than the original solves a real problem: if another
program truncates a mapped file, reading the missing pages crashes the process
(SIGBUS). The clone cannot be changed by other programs, so Leal cannot crash
this way, and it always has a stable snapshot of what it opened.

Fallbacks, only when the volume can't clone at all and can't vanish (an
internal volume that isn't APFS). Removable drives and network shares
follow the path below instead. An EXDEV error means "clone elsewhere", not
"no cloning":
- up to 512 MB: read the file into memory, and show a small status bar note;
- above that: copy to the temporary directory, then map the copy.

Clones are deleted when the document closes. Leftover clones from a crash are
removed at launch, from every folder Leal recorded.

The original file is watched with kqueue, on one small thread per open
document in leal-core (1.9). If it changes, the window shows a banner with
**Reload** and **Keep Editing**; if it is deleted or moved to the Trash,
the banner has **Save As…** and **Keep Editing**, since there is nothing to
reload.

**Checking before a save** (ADR-0008 decision 9). Saving checks the
original's identity (inode, size, mtime) against what was opened, and asks
before overwriting a file changed elsewhere. The check runs immediately
before writing, whatever the watcher last said: it opens the file afresh
and reads its identity with `fstat`, which makes network file systems
revalidate.

**Leal's own save is not an outside change** (ADR-0008 decision 1). After
a successful save, Leal rebases the document onto the file it just wrote:
- it takes a new snapshot (a clone, or a copy on removable drives and
  shares) and re-indexes it;
- it gives the watcher the new identity, and treats the event from its own
  write as expected;
- it clears the "changed elsewhere" flag;
- edits and undo carry on (§3.6).

Saving twice in a row never shows a banner or an "overwrite?" prompt.

**Opening off the main thread.** The app opens every file's core document
on a background queue, before AppKit makes its window. Reload and every
check of the original run there too. This holds for every file, not only
shares: asking whether a file is on a share would itself touch its volume,
and the open costs the same on either thread. The main thread builds the
window from the first 64 KB already read. An open that takes more than
half a second shows "Opening…", and the app stays responsive.

**Removable drives and network shares (ADR-0006, ADR-0009).** A file on a
volume that can vanish is never mapped from that volume, because touching
mapped pages of a vanished volume crashes the process. These volumes are
removable drives (external drives, disk images) and network shares. A
share is any volume that isn't local (`MNT_LOCAL` missing), or whose file
system is `smbfs`, `nfs`, `afpfs`, `webdav` or `ftp`. A share is
recognised before the "same volume as Leal's temporary folder" shortcut,
so files in a network home folder are shares too. For these files:
1. First paint uses ordinary reads, which fail with an error, not a crash.
   On a removable drive, early scrolling reads the drive the same way. On
   a share, rows come only from the copy (below).
2. The indexing pass streams the file and copies it to Leal's temporary
   folder on the internal disk.
3. Once the copy is complete, Leal maps the internal copy and drops what it
   held on the external volume.
4. If the drive or share vanishes first, Leal shows a "disconnected"
   banner, keeps the rows it has read and the user's edits, blocks Save
   until the drive or share returns, and offers Save As. Save As from an
   incomplete document writes only complete rows (§3.7, ADR-0008 decision
   6).

A removable drive is cloned on the drive if it can clone. A share is never
cloned: Leal makes nothing on the user's share. It reads the user's file
itself, and checks it with `fstat` after every read.

Shares have more rules (ADR-0009):
- **The share is never read on the main thread,** where a share that stops
  answering would freeze the app. Only first paint's 64 KB read and the
  indexing pass read it, both in the background. Rows are read only from
  the internal copy; rows not copied yet show as loading until the pass
  brings them. Debug builds assert this. A share's files are closed on a
  thread of their own, because a close on a hung share can block.
- **Network errors are retried.** `ETIMEDOUT`, `EHOSTDOWN`, `EHOSTUNREACH`,
  `ENETDOWN`, `ENETUNREACH`, `ECONNRESET`, `ECONNREFUSED`, `ECONNABORTED`,
  `ENOTCONN`, `EPIPE`, `ESHUTDOWN`, `EAGAIN` and `EIO` (SMB's timed-out
  request) are retried with backoff, from 100 ms, doubling. A retry starts
  only within about 3.5 s of the first failure. If the read still fails,
  the share is disconnected. Any other failure on a share disconnects it
  at once, without retries; it is never an ordinary read error.
- **`ESTALE` or `ENOENT` is checked against the path** where the file is
  now, because an NFS server that restarted gives `ESTALE` for a file that
  is still there:
  - the same file there: only the handle went stale, so the share is
    disconnected, and it reconnects;
  - another file there: the file was replaced, so it changed while being
    read, and the banner offers Reload;
  - nothing there, with its folder present on the same device: another
    computer deleted it (**Deleted**, below);
  - anything else (no folder, a folder on another device such as an empty
    mount point, or a look that fails): disconnected.
- **Deleted** is a state of its own. As when disconnected, the rows copied
  stay readable and Save is off. Unlike a disconnection, it never
  reconnects. Its banner says the file was deleted on another computer
  while Leal was reading it, and that Leal shows the rows it had read. Its
  button is **Save As…**. It replaces the plain "deleted" banner.
- **Changes during the copy.** The copy's first chunks are checked against
  first paint's bytes, which catches a change between the two even when
  the size and time are put back. A short read of the file is a change. A
  change found by a look at the file during the copy means the file
  changed while being read, because the rest of the copy would be the new
  version.
- **Reconnecting.** A disconnected share reconnects, if the file is
  unchanged, when a volume mounts, when the app becomes active, or at a
  check every 5 s. The check is needed because an SMB session that comes
  back by itself posts no mount notification. The copy then carries on.
  After three disconnections in a row at the same place, the periodic
  check stops, and the banner stays; a mount, app activation or Reload
  starts it again.

**Known v1 limits.**
- **Changes on SMB and NFS shares.** Network clients cache file details,
  so a change that another computer makes may not show: during the copy,
  to the watcher (which sees only this Mac's changes), or even to the
  fresh `fstat` before a save. Leal may miss such a change in v1 (ADR-0008
  decision 9, ADR-0009).
- **A network home folder** puts Leal's temporary folder on the network
  too. The complete copy is then read with ordinary reads, never mapped,
  so the whole-file review (§3.2) doesn't run. Rows are read from that
  copy on the main thread, so a home share that stops answering can stall
  the window. Save still works.
- **Revert to Saved** is AppKit's second read of the file, on the main
  thread. Nothing saves in phase 1, so it can't be reached yet; task 2.5
  sends it through Reload.

### 3.2 Dialect and encoding detection

> Exact rules for encoding choice and positions: ADR-0003 (§1, §6, §7),
> ADR-0004 §11 (the `com.apple.TextEncoding` attribute) and ADR-0005 (§1,
> §4, §5). The ADRs take precedence over this section.

Detected from the first 64 KB, then checked against the whole file in the
background (1.2):

- **Delimiter:** `,` `;` `\t` `|`, chosen by the most consistent field count
  across rows.
- **Quote character:** `"`. Other quote characters are out of scope for v1.
- **Line endings:** LF, CRLF or CR, and whether they are mixed.
- **BOM:** UTF-8, UTF-16 LE/BE.
- **Encoding:** UTF-8 if the file is pure ASCII, or if its valid multibyte
  UTF-8 sequences outnumber its invalid bytes (with an invalid-encoding
  warning for those). Otherwise single-byte, Windows-1252 by default
  (ADR-0003 §1). The user can override it with **Reopen with encoding…**
  (§4.1).
- **Header row:** heuristic (first row text, later rows typed differently).
  Only affects display.
- **Trailing newline** at end of file: present or not.

**Supported encodings in v1** (ADR-0005 decision 5):
- detected automatically: UTF-8 (with or without BOM), UTF-16 LE/BE with a
  BOM (read-only, §4.3) and Windows-1252;
- only from the attribute or **Reopen with encoding…**: the other
  single-byte, ASCII-compatible encodings (Windows-1250, 1251 and
  1253–1258, ISO-8859-1, -2 and -15, and Mac Roman), which are safe because
  the delimiter, quote and line-ending bytes can't appear inside a
  character;
- not supported: multibyte encodings such as Shift_JIS, whose second bytes
  can equal `|` or `\`.

Attribute values are matched by their CFStringEncoding number. An
unsupported or unreadable attribute is ignored, with a status bar note.

Dialect is an *interpretation*. The user can change it (for example, "treat as
semicolon-separated"), which re-indexes the file but never changes its bytes.

**Remembering the interpretation** (ADR-0005 decision 1). The delimiter and
header choice are guessed from the whole file, so an edit anywhere can
change what a reopen guesses. Leal stores them in its own extended
attribute, `io.github.robhaswell.leal.interpretation`, on save when a
reopen would otherwise guess differently, or when the user chose them. On
open, Leal honours the attribute if the file still parses sensibly with it.
Other apps still guess for themselves.

For first paint, the dialect is decided from the **first 64 KB only**. The
whole file is checked afterwards in the background (samples can't know
whether they start inside a quoted field). If it disagrees, Leal does not
re-lay out the grid under the user; it shows a suggestion instead ("This
file looks semicolon-separated — Switch").

The encoding at first paint is chosen in this order (ADR-0005 decision 4):
the BOM, then the `com.apple.TextEncoding` attribute, then the encoding rule
above applied to the first 64 KB. The whole-file rule runs afterwards as P2
work. If it disagrees, Leal shows a suggestion ("This file looks like
Windows-1252 — Reopen as Windows-1252") and never re-decodes silently. The
encoding in use is the document's encoding for saving and for the reopen
guarantee (§3.7).

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
> Editing hatched cells: ADR-0005 §2.

The original bytes are never modified. Edits live in an overlay:

- **Cell edits:** for each edited row, its new cell values, by the cell's
  place in the row: one of its own fields, or a cell past its end (a
  hatched cell). Looking a row up is O(log n) in the edited rows, and free
  when there are none.
- **Row structure:** a piece list over rows, each piece either a range of
  original rows or a run of inserted rows. Deleting a row splits a piece.
  Inserted rows keep their cells in the overlay too, by an id of their own.
- **Column structure:** a list of column inserts and deletes, each
  recording the rows it applied to, since a row too short for a column
  isn't changed (ADR-0004 §5).

**Commands.** Every change is a command storing both the old and new
values, in logical coordinates. A missing cell is distinct from an empty
one (`None` versus `""`), and a command's new value is what the cell reads
as afterwards. Each command belongs to a **lineage**: the edits of one way
of splitting the file into cells. A command applies only in its own
lineage, and only if its cells still hold what it expects, so a stale
command never lands on the wrong cell. A command of several cells (a paste)
is checked whole before anything changes, and applies whole or not at all.
Undoing a row or column delete restores the original bytes, not just the
values.

**Undo is the app's.** The app's `NSUndoManager` holds the commands, and
the core keeps no undo stack: undo applies a command's inverse, and redo
applies it again. The commands survive a save: after saving, the saved file
becomes the new base, in the same lineage, and the stored values still
apply (§3.1, ADR-0008 decision 1), except that a hatched cell, a real field
once saved, can't be made missing again by an undo (task 2.2 decides what
that undo does).

**Recovery by replay** (§3.9, ADR-0008 decision 5). An undo manager can't
be listed, so the app also keeps an append-only journal of every command it
applies (edits, undos and redos, in order), with the reading's choices
(delimiter, encoding, header row). To recover a failed document's edits, it
opens the file afresh with those choices, waits for the index, and replays
the journal. Replay checks values, not lineage. It skips each command that
no longer applies and reports it with the reason, and returns the ones that
applied, in the new document's lineage, as the new undo history.

**Edits are visible everywhere** (ADR-0008 decision 2). Every reader goes
through the overlay: the grid, find (it matches edited values, and stops
matching the values they replaced), copy (the copy snapshots the overlay
when it is made), the inspector, the diagnostics markers (an edited cell is
checked again on its new value; only a NUL can be in one, and the
diagnostics report itself still describes the file as it was read), and
column widths and number detection.

**Editing starts from the full value** (ADR-0008 decision 3). The in-cell
editor and the inspector start from the core's full display value, never
from the grid's shortened text or its ↵ ⇥ ␀ symbols. A value too long for
the inspector's 64,000-character view is loaded in full before it can be
edited. Committing a long or multiline value unchanged is no edit.

**Re-reading a document that has unsaved edits** (ADR-0008 decision 4).
Edits are tied to how the file was split into cells, so they can't move
across a new delimiter or encoding:
- **Reload** and **Revert to Saved** ask to discard the edits first. Revert
  goes through the same path as Reload (§4.3).
- **Treat As** and **Reopen with encoding** are disabled while there are
  unsaved edits, with "Save or revert your changes first" as the reason.
  The core refuses a new delimiter or encoding meanwhile. Once there are no
  edits, a new split starts a new lineage, so commands from before it are
  refused and the app clears its undo history.
- **The header-row toggle** stays available. It changes only how row 1 is
  displayed, not where the edits are, so the lineage stays.
- **A drive coming back** keeps the edits and their lineage, because Leal
  has confirmed the file is unchanged.

Setting a cell back to exactly its original display value removes the edit,
so the original bytes (including their quoting) come back. The document has
unsaved edits only while some cell reads differently from the file, so an
edit set back by hand leaves it clean.

A **hatched cell** (a missing field of a short or blank row) can be edited
(ADR-0005 decision 2). Saving appends the delimiters needed to reach that
column, then the new value, at the end of the row before its line ending. A
blank line edited in column *c* becomes a row of *c* + 1 fields. Committing
`""` to a hatched cell is no edit, and setting an edited one back to `""`
makes it missing again, so the row's original bytes come back. This
narrows ADR-0005 decision 2 and is for Rob to confirm at the phase 2 gate;
padding a short row with empty fields would be a separate command, "Fill
missing cells". Edits past an unterminated quote are still rejected
(ADR-0004 §8).

**Edits on rows that turn out stale** (§3.1). Rows of the first 64 KB can
be edited before the index reaches them. If the file then turns out to have
changed while it was read, those bytes aren't trusted. Such an edit is
kept, never dropped: it records a hash of its row's bytes, and the core
reports as **edit conflicts** the edited cells whose rows the trusted copy
lacks or has with other bytes, for the app and Save As from an incomplete
document (ADR-0008 decision 6) to name.

### 3.7 Saving

> Edge cases (quoting new fields, end-of-file line endings, ragged rows and
> blank lines, empty rows, BOM-like starts, unterminated quotes, the reopen
> guarantee and remembering guessed encodings): ADR-0004, refined by
> ADR-0005 (§1, §2, §3). The ADRs take precedence over this section.

Saving streams the document out:

1. An **untouched row** is copied byte-for-byte, including its line ending.
2. An **edited row** is rebuilt field by field:
   - untouched fields: original raw bytes, unchanged;
   - edited fields: new value, encoded in the file's encoding, quoted if the
     value needs it (contains delimiter, quote, CR or LF) **or** the original
     field was quoted **or** the file quotes every field;
   - delimiters and the line ending: the row's originals;
   - an edited hatched cell: the delimiters needed to reach it and the new
     value, appended before the line ending (§3.6).
3. **New rows** use the file's most common line ending. A **new field** (in
   an inserted row or column) is quoted if the value needs it, if the file
   quotes every field, or if its column does: the column has at least one
   non-empty field and every non-empty field in it is quoted. A column's
   fields are the fields at that index in non-blank rows long enough to
   have one, header row included (ADR-0004 §2, ADR-0005 decision 3).
4. The trailing newline at end of file is kept as it was.

**Reopening** the saved file gives the same BOM, quote character, line
endings and row values (ADR-0004 §10, narrowed by ADR-0005 decision 1). The
encoding, delimiter and header choice are guessed from the whole file, so
Leal remembers them in extended attributes instead (§3.2).

The app saves through `NSDocument`'s safe-save: the core writes to the
temporary URL AppKit provides, which is then swapped in atomically. File
permissions, extended attributes and Finder metadata are preserved, except
the two attributes Leal writes itself (below). Just before writing, Leal
checks that the original hasn't changed elsewhere; after a save it rebases
the document onto the new file, so its own save never looks like an outside
change (§3.1, ADR-0008 decisions 1 and 9).

**Attributes written on save** (ADR-0008 decision 8). On every save, Leal
writes the interpretation attribute (§3.2), with a fingerprint of the bytes
it just wrote (ADR-0007), whenever:
- a reopen's first paint or whole-file review would guess differently;
- the user chose the delimiter or header; or
- the choice came from the attribute.

Otherwise it removes any old attribute. The same rule applies to
`com.apple.TextEncoding`, comparing against both the first-64 KB and
whole-file guesses. Attributes copied over from the old file never keep a
stale fingerprint.

If a new value cannot be represented in the file's encoding (for example an
emoji in a Windows-1252 file), saving stops with a message naming the cells,
and offers **Save As UTF-8** instead. Leal never drops or substitutes
characters.

**Save As UTF-8** (ADR-0008 decision 7) keeps line endings, quoting style
and delimiters, in meaning. It writes a UTF-8 BOM only if the original file
had a BOM. It sets `com.apple.TextEncoding` to UTF-8 and rewrites the
interpretation attribute for the new bytes. Text that can't be converted
(an unpaired surrogate or odd final byte in UTF-16, or an unmapped byte in
a single-byte encoding) makes it refuse and name the cells, as F5 requires.
The user can edit those cells and try again. Nothing is substituted
silently.

**Save As from an incomplete document** (ADR-0008 decision 6): a drive or
share disconnected, the file changed while it was being read, or it was
deleted on another computer while being read (§3.1, ADR-0010).
Save As writes only **complete rows** from the bytes Leal trusts, cut at
the last row boundary, with the user's edits applied. It never writes half
a row, half a character or an open quote. The dialog says plainly that the
copy is incomplete ("about N of M rows"). Nothing is added to the file to
mark it.

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

- A document is an `Arc`-shared object. Reads for visible cells are synchronous
  and must take under 1 ms. They are safe to call on the main thread.
- Indexing, filtering, sorting and saving are long jobs. They run on
  Rust-owned threads or pools and report progress through callbacks. Their
  UniFFI async functions (Swift `async`) only report completion.
- **Cancellation is explicit** (ADR-0005 decision 6), because UniFFI doesn't
  pass Swift task cancellation through to Rust. Each long job has a handle
  object with `cancel()`, which sets a flag the job checks at its chunk
  boundaries (§3.10 rule 3). Swift wraps each await in
  `withTaskCancellationHandler`, which calls `cancel()`.
- The main thread never waits on a long operation. While indexing, the row
  count is "rows indexed so far".
- **Edits are synchronous** calls, safe on the main thread, and serialized
  with each other and with re-reading. A search takes only the edits of the
  rows it is reading, never the whole overlay, so an edit during one stays
  cheap. A copy snapshots the overlay when it is made (§3.6).
- **Find catches up with edits** without restarting. A search recounts the
  rows edited since it last looked. A query from the main thread does this
  itself only when the work is small; otherwise it starts a catch-up job
  (which checkpoints like any job, §3.10 rule 3) and, until that finishes,
  reports `catchingUp`, answers Next and Previous with `Pending`, and gives
  the counts as they are. The app polls the progress, or awaits the
  catch-up job, and retries a pending step.
- A Rust panic that UniFFI catches at the boundary reaches Swift as an
  error, but it can leave a `Mutex` inside the document poisoned. So after
  a caught panic, that document is treated as failed: Leal makes no further
  calls on its handle, and the app shows an error and offers to reopen the
  file.
- **A failed document's unsaved edits are not lost** (ADR-0008 decision 5).
  The app keeps its own journal of the edit commands it applied, with the
  reading's choices, and the failure alert offers **Recover changes**: Leal
  opens the file afresh with those choices and replays the journal into it
  (§3.6). If the file is unchanged, the window carries on with the edits.
  If it changed, or a command no longer applies, Leal offers Save As of
  what it could recover, and names any edits it couldn't apply.

### 3.10 First paint and work priority

Opening a file starts several jobs. They run in a strict priority order, and
lower-priority work must never delay higher-priority work.

| Priority | Work | When | QoS |
|---|---|---|---|
| **P0** | Off the main thread (§3.1): clone, map (ordinary reads on removable drives and shares), detect dialect and encoding from the first 64 KB. On the main thread: parse the first screen of rows, paint | Immediately, before anything else starts | User-initiated (the open), user-interactive (paint) |
| **P1** | Row index (§3.3) and diagnostics (§3.5), in one pass (1.5); parsing rows as the user scrolls | Straight after P0 | User-initiated |
| **P2** | Whole-file dialect and encoding check (§3.2), refined column widths, number detection for alignment | Alongside or after P1 | Utility |
| **P3** | Filter and sort acceleration (below) | Only on first use of filter/sort, or when idle | Utility, paused while the user scrolls or edits (rule 3) |

Rules:

1. **First paint does not wait for the index.** P0 parses the first rows
   directly from the start of the file. It touches only the first few pages of
   the file. Its encoding comes from the BOM, the attribute or the first
   64 KB; the whole-file rule runs as P2 work and can only suggest a change
   (§3.2, ADR-0005 decision 4).
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
- **Changing the interpretation** (ADR-0005 decision 8): the status bar says
  where the encoding came from (BOM, attribute or guess); a **Treat as**
  delimiter menu; **Reopen with encoding…**; and the delimiter and encoding
  suggestion banners (§3.2). These use the ADR-0002 status-bar and banner
  styles.

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

- `NSDocument`-based: Open Recent, window tabs, Save, Save As, Revert to
  Saved, dirty indicator.
- **Autosave-in-place is off.** Leal only writes the file when the user saves.
- **No Versions browser in v1** (ADR-0008 decision 10). AppKit's Versions
  browser needs autosave-in-place, so v1 offers **Revert to Saved** only.
- **Revert to Saved** asks to discard unsaved edits first, and goes through
  the same path as File ▸ Reload from Disk, never AppKit's default
  `read(from:)` (§3.6, ADR-0008 decision 4).
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

- **F1** Save As with no edits writes a byte-identical file.
- **F2** Editing field *(r, c)* changes only that field's bytes, or, for a
  hatched (missing) cell, bytes appended at the end of that row (ADR-0005
  decision 2). Every other byte is identical and in the same order.
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
│   ├── leal-bench/         benchmarks: reference file generator, criterion
│   │                       benches, CI report (never shipped)
│   └── uniffi-bindgen/     host-only tool that generates the Swift bindings
├── app/
│   ├── project.yml         XcodeGen spec (the .xcodeproj is generated)
│   ├── Sources/
│   ├── Resources/
│   ├── Tests/              XCTest of the bindings (not hosted)
│   └── AppTests/           XCTest hosted in Leal.app (documents, grid)
├── tests/corpus/           hand-made files with expected-result sidecars
├── fuzz/                   cargo-fuzz targets (PLAN 2.7)
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
3. **Minimum macOS:** 14 Sonoma (Rob, phase 1 gate, 2026-10-02).
4. **No column drag-to-reorder in v1** (Rob, phase 1 gate). It isn't in the
   mockups; the column map (§3.6) keeps it possible later.

Open:

5. **Mac App Store** as well as direct download? Decided at PLAN 4.5. The
   design keeps it possible.
