# Phase 2 gate — decisions for Rob

Date: 2026-10-05 · Status: **approved by Rob (tag `phase-2`), 2026-10-05**

Every decision the phase 2 gate needs from Rob, each with context, options
and a recommendation. Collected from PLAN 2.7's gate items, ADR-0015, the
gate review and docs/perf.md.

## Decided

### 1. `""` typed into a hatched cell is no edit

**Decided (Rob, 2026-10-05): (a).** `""` in a hatched cell is no edit; noted in ADR-0005 decision 2.

- **Context.** A hatched cell is one a short row doesn't have. ADR-0005 d2
  lets the user type into it: the save appends the delimiters needed, then
  the value. Typing nothing (`""`) and pressing Return leaves the cell as it
  was, so Leal counts it as no edit. Read literally, d2 would pad the row.
- **Options.** (a) Accept: no edit; a "Fill missing cells" command could pad
  a row on purpose, later. (b) Follow d2 literally: an empty value pads the
  row, so Return on an untouched hatched cell changes the file.
- **Recommend (a).** (b) lets a stray Return change the file.
  → `docs/tasks/2.1.md`, "Decisions and interpretations"; ADR-0005 d2

### 2. Save flushes with `F_BARRIERFSYNC`, not `F_FULLFSYNC`

**Decided (Rob, 2026-10-05): (a).** `F_BARRIERFSYNC`, recorded as ADR-0012 decision 5; DESIGN §3.7 agrees.

- **Context.** The barrier puts the new file's bytes ahead of the rename that
  makes them the file, so a crash leaves the old file or the whole new one,
  never a mix. It doesn't wait for the drive's cache to empty, so a power cut
  within about a second of a save can give back the old file. You were told
  when you accepted ADR-0012, but the ADR doesn't say so.
- **Options.** (a) Accept and record it in ADR-0012. (b) Use `F_FULLFSYNC`:
  survives the power cut, but each save waits for the drive's whole cache.
- **Recommend (a).** The file is never torn, and the risk is the old file
  after a power cut. → `docs/tasks/2.2.md`, "Decisions and interpretations"

### 3. Scrolling "no dropped frames": allow up to 0.05% late frames

**Decided (Rob, 2026-10-05): (a).** At most 0.05% late frames while background work runs, zero after loading (DESIGN §1, docs/perf.md). `leal-perf`'s verdict follows later (PLAN 2.6a).

- **Context.** DESIGN §1 says no dropped frames. After loading, scrolling had
  0 late frames in 22,680. While the index and a search run, it had 3 late in
  7,551 (0.04%), then 0 and 1 in two more runs. Read literally, that fails;
  main-thread work (1.1 ms p50, 2.3 ms p99) is inside the 3× rule.
- **Options.** (a) Allow up to 0.05% late frames; zero stays the budget after
  load. (b) Keep zero and chase single frames.
- **Recommend (a).** 0.04% is one frame in 2,500. →
  `docs/perf-runs/2026-10-05-m5pro-unlocked-phase2.md`

### 4. ADR-0015 (accepted): structural-save budgets are report-only

**Decided (Rob, 2026-10-05): (3).** ADR-0015 accepted; DESIGN §1 notes structural-save budgets are report-only.

- **Context.** DESIGN §1 says a save after one edit takes under 500 ms, a
  column insert counting as one. A column insert rewrites every row. It takes
  1.06 s on the CI runner (about an M1 Air) and 409 ms locally; round B cut
  about 20% (339 ms locally). It kept the Benchmarks workflow red.
- **Options.** (1) Rewrite the writer to splice fields: days of work and risk
  to the fidelity tests. (2) Raise the budget to about 1.5 s, still hard:
  arbitrary. (3) Report-only: over budget warns, a missing result still
  fails, the no-disk twin is still checked for 20% regressions. (4) Drop it.
- **Recommend (3).** It matches your priorities. It is already in code,
  provisionally; to reject it, remove one line from `REPORT_ONLY`. →
  `docs/adr/0015-structural-save-budgets.md`

### 5. Undoing a cell edit after a save restores its text, not its bytes

**Decided (Rob, 2026-10-05): (b).** A known limit in ADR-0012 decision 4, with a clarifying line in ADR-0014 decision 8; tracked as GitHub issue #6.

- **Context.** After a save the saved file is the new base, and undo puts the
  cell's text back (ADR-0012 d4, "values, not bytes"). Bytes that weren't
  valid text come back as U+FFFD, and an oddly quoted field is requoted.
  ADR-0014 d8's wording ("an unedited field's own bytes") reads as if it
  covered this, so the two ADRs disagree.
- **Options.** (a) Fix: keep the old field's bytes in the edit. About 2–3
  days, about 10 files (core, writer, FFI), medium-high risk. (b) Record it
  as a known limit in ADR-0012 and file an issue; fix it if anyone is hit.
- **Recommend (b) for now.** It takes invalid bytes, a cell edit, a save and
  an undo, in that order, and the fix lands on the edit store and writer the
  fidelity tests guard. A related nit (undo then redo of a row insert across
  a save can change its quoting) goes in the same note. →
  `docs/tasks/2.G-a.md`, "Fidelity estimate"

### 6. The undo history keeps an old reading per structural command

**Decided (Rob, 2026-10-05): (a).** The cost is recorded in ADR-0014 decision 3, unbounded.

- **Context.** ADR-0014 d3: a row or column command keeps the file it was
  made on, so an undo after a save works by value. Each such command keeps
  about 6 MB of an old reading per save (10 MB before round A) plus a
  file-sized clone on disk: eight delete-and-save rounds take the heap from
  5 MB to 56 MB. You said about 10 MB is fine.
- **Options.** (a) Record the cost in d3 and leave it unbounded. (b) Bound the
  undo history: it changes what undo can do, which d3 chose not to.
- **Recommend (a).** → `docs/tasks/2.G-a.md`, "Measurements"; ADR-0014 d3

### 7. Approve phase 2 and tag `phase-2`

**Decided (Rob, 2026-10-05): approved.** Phase 2 is approved; the `phase-2` tag follows.

- **Recommend** approving once the above are settled. You plan to pause after
  this, so the tag, and a release build installed into `/Applications`, are
  the next step. → `CLAUDE.md`, "Phase gates"

## Already decided (no action needed)

- **Duplicate** saves a copy and switches the window to it; the changed-while-
  reading banner's second button is **Save As…**; Save As UTF-8 onto its own
  file converts it in place after Save's checks (2.5.3c).
- **Return stays on the cell** (issue #2). **⌘⇧↩** duplicates rows, keeping
  the column. A duplicated blank line is saved as `""` (ADR-0014 d9).
- **Paste** past the last row or column is refused. **Cut** of whole rows (by
  row number) removes them; ⌘A then Cut clears cells. Pasted line breaks
  become the file's own ending (2.6). A closed banner stays closed across a
  save (2.5.3b).
- **Performance priorities:** first paint and reading matter; a slow operation
  is fine if nothing obvious is wasted. A huge undo after a save may
  beachball: no limit, no background job (2.G-a).
- **Still parked:** a large file's save cancelled or closed on a slow volume,
  a network share and a real USB stick (also phase 1's item 12); issues #1 to
  #5 are filed for later.
