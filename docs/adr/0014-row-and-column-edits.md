# 0014 — Row and column edits: when they run, Find, undo's base, quoting

- Status: proposed
- Date: 2026-10-03
- Changes: DESIGN §3.6 (Edits), §3.7 (rule 3), §3.9 (Find catching up;
  edits during a save); refines ADR-0005 decision 3

## Context

Task 2.4's design (`docs/tasks/2.4.md`) needs four choices that DESIGN
either doesn't make or makes differently.

## Options

Where there was a real alternative, the decision below names it and why it
was rejected.

## Decision

1. **Row and column inserts and deletes need the whole file, and no save
   running.** They are refused until the index pass is complete and
   trusted (`StillReading`), together with a removable drive's or share's
   copy, and while a save runs (`Saving`). The app disables them, giving
   the reason. Cell edits are unchanged.
   - **Why:** the piece list then covers a fixed row count with final row
     marks, and a save's carry-over only has to handle cell edits.
   - **Cost:** a few seconds after opening a large file, and while saving
     one.
   - **Rejected:** allowing them within the rows indexed so far, which
     brings an open-ended last piece, marks that aren't final, and
     untrusted rows from the first 64 KB (1.9) into the map.
2. **Find restarts after a column insert or delete.** It still catches up,
   without restarting, after cell edits and row inserts and deletes, keyed
   by row id.
   - **Why:** a column insert or delete can change every row's matches,
     so catching up would be a full search anyway.
3. **A structural command keeps the file it was made on.** Undo within
   the session restores by identity, so the original bytes come back.
   After a save, or in a replay, it works by value: the deleted rows' or
   column's values are read lazily from the base reading that the command
   holds.
   - **Cost:** that old snapshot stays until the undo history lets go of
     the command. Its disk space is a clone, or a copy on a removable
     drive, as large as the file.
   - **Also:** undoing a column insert or delete after a save reads every
     row it touched, because it checks values (§3.6).
   - **Rejected:** taking the values when the command is made. That costs
     a parse of the whole file for every column delete, and holds the
     values in memory.
4. **Per-column quoting looks at the document as it is now.** The
   "column" in ADR-0005 decision 3 is the logical column at save time. Its
   fields are the original fields at that position, with edited fields
   judged by their original bytes; new cells don't count.
   - Hatched cells keep 2.2's rule: quoted if needed, or if the file
     quotes every field. ADR-0004 decision 2 names only inserted rows and
     columns.

## Consequences

- **DESIGN §3.6:** add when structural edits are allowed; that their undo
  restores bytes within the same base and works by value after a save or
  in a replay; and that the base stays alive.
- **DESIGN §3.9:** "Find catches up with edits without restarting" gains
  the column exception, and "edits carry on during a save" now means cell
  edits.
- **DESIGN §3.7, rule 3:** the precise meaning of "column".
- **Task 2.5:**
  - disable Insert/Delete Row/Column while the file is being read or
    saved;
  - show Find starting over after a column insert or delete;
  - expect a structural undo to come back with `Saving`.
