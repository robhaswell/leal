# 0013 — Editing UTF-16 files, Leal's own encoding tags, and a Save As UTF-8 budget

- Status: accepted (decided by Rob, 2026-10-03)
- Date: 2026-10-03
- Changes: DESIGN §1 (budgets), DESIGN §4.3 (UTF-16 read-only); refines
  ADR-0004 decision 11

## Context

Task 2.3 (encoding on save) raised three questions; details are in
`docs/tasks/2.3.md`.

1. ADR-0008 decision 7 says that when Save As UTF-8 refuses because some
   cells can't be converted, the user can edit those cells and try again.
   But DESIGN §4.3 shows UTF-16 files read-only, so they can't. The core
   already supports edits on UTF-16 documents.
2. ADR-0004 decision 11 ignores a single-byte `com.apple.TextEncoding`
   (other than UTF-8 and Windows-1252) when any byte doesn't decode in it.
   So a file Leal itself saved in, say, Windows-1253, containing a byte
   that encoding leaves unassigned, reopens in a different encoding.
3. Save As UTF-8 has a benchmark but no budget.

## Decision

1. **UTF-16 files can be edited.** Save stays off for them. Save As UTF-8
   is the only way to save one, so the user can fix the cells it names and
   try again. The window says so (the 06a banner and its Save As UTF-8
   button already explain it).
2. **Leal trusts encoding tags it wrote itself.** When Leal's own
   interpretation attribute (ADR-0007) has a fingerprint matching the file,
   the file's encoding tag is honoured even if a byte doesn't decode in it.
   That byte is shown faithfully, with a warning, as any undecodable byte
   is. Tags written by other apps keep ADR-0004 decision 11's rule.
3. **Budget:** Save As UTF-8 of the 100 MB reference file, from UTF-16,
   takes under 1 s (measured at 184 ms on the M5 Pro).

## Consequences

- DESIGN §1 gains the budget. DESIGN §4.3 no longer says UTF-16 is
  read-only; it says Save is off and Save As UTF-8 is the way out.
- Task 2.3 implements decision 2 in detection and its tests. Task 2.5
  (editing in the app) makes UTF-16 documents editable with Save off.
