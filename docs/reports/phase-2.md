# Phase 2 report — Editing

Date: 2026-10-05 · Status: **approved by Rob (tag `phase-2`), 2026-10-05**

## What Rob decided

Rob approved all seven recommendations on 2026-10-05, in
[`phase-2-decisions.md`](phase-2-decisions.md): `""` in a hatched cell is no
edit; Save's `F_BARRIERFSYNC`; a 0.05% late-frame tolerance; ADR-0015
(report-only structural-save budgets); undo of a cell edit after a save
restoring text, not bytes; the undo history's memory cost.

## What was built

Leal now edits CSV files and saves them byte-faithfully.

| Task | Result |
|---|---|
| 2.0, 2.0a, 2.0b | Network shares stream like drives. Scrolling draws in strips and passes the 3× headroom rule (ADR-0009 to 0011). |
| 2.1, 2.1a | Cells can be edited, hatched (missing) ones too. Find, copy and the marks see edits. |
| 2.2, 2.3 | Save writes only what changed, in the file's own encoding. Save As UTF-8. Text an encoding can't hold is refused, naming the cells (ADR-0012, 0013). |
| 2.4, 2.5a | Insert, delete and duplicate rows and columns, undoable and saved (ADR-0014). |
| 2.5.1, 2.5.2 | In-cell and inspector editing, header cells included. Undo, redo, the unsaved dot, and **Recover changes** after a failure. |
| 2.5.3a–c | Save (progress, ⌘. to cancel, Save Anyway, Unlock), Save As, Duplicate, Rename, Move To, Revert. After a save Leal carries on from the new file with no false "changed" banner. |
| 2.6, 2.6a, 2.7, 2.G | Paste, Delete clears, Cut. Review tidy-up. Fuzzing of five targets. The gate's fixes (rounds A, B, C). |

## Tests and CI

About 974 Rust tests and 382 app tests. CI also runs deep property tests,
nightly fuzzing, the benchmark gate (budgets, then a 20% regression check)
and, since the gate, `sandbox-save-check` (saving in the real sandbox, on the
internal disk, FAT32 and exFAT).

## Performance (M5 Pro, unlocked, Rob's run of 2026-10-05)

| Budget | Result |
|---|---|
| Launch | 184.5 ms median; one 371 ms first launch after a build (the system's first-run scan, not Leal): mixed |
| Open to first rows | 52.5 ms warm (budget 150); 368 ms cold, counted as launch: pass |
| Full index, 100 MB | 160 ms: pass |
| Scrolling | 0 late frames after load; 1.2 ms p50, 2.1 ms p99 of main-thread work (limit 2.8): pass |
| Scrolling while loading | 0.04% late at worst: **fail as written**, passes the proposed 0.05% |
| Memory | Heap 7.8 MB settled (budget 40); idle 17.5 MB footprint (budget 30): pass |
| Cell edit to screen | 6.0 ms median, 18.1 ms max (budget 16): mixed; the slowest edits widen a column |

Save after one edit: 122 ms here, 245 ms on CI (budget 500). Save after a
column insert: 339 ms here but 1.06 s on CI (decision 4).

## The gate

Five parallel reviewers (fidelity, Rust, performance, tests, the app), each
with adversarial verification: 23 agents. They confirmed **12 findings, 2
must-fix**, refuted 3, and listed 21 nits. The fidelity reviewer ran 2,100
random edit-and-save sessions and found nothing that writes wrong bytes.

| Round | Fixed |
|---|---|
| A | **Must:** undo of a big delete after a save was slow and used GBs. A column undo is 3× faster, rows 10–20%. A 999,999-row undo still takes 1 s and 1.8 GB, which Rob accepted. Saved index no longer double-sized. |
| B | **Must:** the Benchmarks workflow had failed on every push since 2.4c. Column-insert save 20% faster. Two flaky tests fixed. `sandbox-save-check` in CI. |
| C | A file created before 1970 saves. Reconnecting to a share no longer blocks edits. Copy then Paste keeps a trailing empty row. Duplicate Row and hatched cells in new rows are in the oracle and tests. Nits: Escape on Save Anyway, ⌘↩ in the inspector, pasted line breaks. |

## Bugs found afterwards, and lessons

- **Save As UTF-8 lost an edit** in rare cases (fixed in 03f28cc): a row
  rewritten by a column insert or delete, whose new UTF-8 equalled its old
  single-byte bytes, was written as the old text. The deep property test found
  it; it dated from 2.4c. **Plain Save was never affected.**
- **A journal-pruning bug, caught in review before landing.** Dropping an edit
  and its undo also dropped a pair straddling a running save, so Recover
  changes would have lost that undo. Fixed, with three tests.
- **The Benchmarks workflow failed unnoticed** for eight pushes, and a failed
  budget hid the regression check. Now a budget failure waits until
  regressions are judged. CI flakes were fixed at the root.
- Rob's checks found two Save faults the tests missed (no File ▸ Save item;
  `EPERM` in the real sandbox), hence `sandbox-save-check` in CI.

## What Rob tested on screen

Editing and undo; Save, Save As, Duplicate, Rename, Move To and Revert
(including changed-elsewhere, locked and Windows-1252 files, and closing or
quitting mid-edit); insert, delete and duplicate rows and columns; paste, cut
and clear.

**Parked** (need a slow volume or hardware): cancel, close and quit during a
large file's save; a network share (no leftover temp files, Save As share to
share, a disconnect mid-read, Revert without a beachball); a real USB stick.

## Rob's decisions this phase

- **Duplicate** = save a copy and switch the window to it. The banner's second
  button is **Save As…**. Save As UTF-8 onto its own file converts it in place.
- **Return stays on the cell** (issue #2). **⌘⇧↩** duplicates rows, keeping
  the column; a duplicated blank line is `""` (ADR-0014 d9).
- **Paste past the last row or column is refused.** **Cut** of whole rows (by
  row number) removes them; ⌘A then Cut clears cells. Pasted line breaks become
  the file's own ending. A closed banner stays closed across a save.
- **Priorities:** first paint and reading matter; slow operations are fine if
  nothing obvious is wasted; 10 MB of undo history per save is fine.

## Open issues

**#1** launch with no file shows nothing; **#2** preferences window (Return
moving down); **#3** `com.apple.quarantine` on saved files (probably normal);
**#4** "Close quote at end of line" repair; **#5** show the detected dialect,
with code snippets. Left open on purpose (PLAN 2.6a): some test gaps, Save As
onto a hard link counting as the open file, and the first nightly fuzz runs'
memory (peaks 586–885 MB, limit 2,048). Decision 5's limit is not fixed.

## Rust concepts used this phase (details: "Rust notes" in `docs/tasks/2.*.md`)

- **Copy-on-write** (`Arc::make_mut`, `Cow`, `Weak`): 2.1, 2.3, 2.5a, 2.6.
- **Enums that carry data**: 2.1a, 2.4b, 2.4c. **Trait objects**: 2.2, 2.4a.
- **`let … else`, `?` on `Option`**: 2.0, 2.1, 2.G-c. **Lifetimes**: 2.1, 2.4c.
- **Locks and atomics** (`try_lock`, `Condvar`, `OnceLock`): 2.1, 2.2, 2.5.3a;
  never hold a lock across I/O: 2.G-c. **`Drop` as clean-up**: 2.0, 2.2.
- **Test-only code** (`cfg(test)`, `thread_local!`): 2.0, 2.1a. **Fuzzing**: 2.7.

## Next

Rob plans to pause after phase 2. On approval: tag `phase-2`, install a release build.
