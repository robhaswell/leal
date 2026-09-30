# 0004 — Save edge cases

- Status: proposed (Rob decides at the phase 0 gate; implemented in phase 2)
- Date: 2026-09-30

## Context

Building the save oracle (task 0.2) turned up nine cases that DESIGN §3.6
and §3.7 don't settle. Each needs one answer before the serializer (tasks
2.2 and 2.4) is written. The testkit oracle currently implements the
provisional answers below; if Rob changes one, the oracle changes with it.

Leal's guiding rules apply throughout: keep the user's data, change as few
bytes as possible, and never change a value silently.

## Decisions

1. **"The file quotes every field"** means every field in every non-blank
   row is quoted.
2. **How new fields are quoted.** A new field (in an inserted row or
   column) is quoted if the file quotes every field, **or if every existing
   non-empty field in that column is quoted**. That second part keeps files
   like pandas' "quote text columns" style consistent. Otherwise it is
   quoted only when needed. *(The oracle currently checks only the whole
   file; per-column detection is added when this ADR is accepted.)*
3. **Line ending for a new row** in a file that has none (a single row with
   no line ending): LF.
4. **End of file.** Whether the file ends with a line ending is kept.
   Appending a row to a file with no final line ending adds one to the old
   last row and none to the new one. Deleting the last row removes the line
   ending from the new last row. This is the line ending "directly next to"
   the change, which F6 allows.
5. **Column insert or delete with ragged rows.** Rows too short to have that
   column are left untouched, not padded.
6. **A row whose bytes would become empty** (for example, clearing the only
   field in a single-column file) is written as `""`, so it stays a row with
   one empty field instead of turning into a blank line or vanishing. An
   original row that was already blank stays blank.
7. **A field that would become a BOM.** If deleting rows moves a field that
   starts with U+FEFF to the very start of a file without a BOM, it would be
   read back as a BOM and lose that character. The serializer quotes that
   field, which is the smallest change that keeps its value.
8. **Rows after an unterminated quote.** An unterminated quote swallows the
   rest of the file into one field, so a row inserted after it would land
   inside the quote. Leal doesn't allow it: inserting a row or column after
   that field is disabled, with an explanation. Inserting a row before it
   (at its own row index) is fine. Editing the swallowed cell itself is
   allowed. Any edit sequence that would leave the unterminated field
   anywhere but at the end of the file is rejected.
9. **Setting a cell with invalid bytes to its displayed value.** §3.6 wins:
   an edit equal to the original display value is no edit, so the original
   bytes, invalid ones included, come back. This also makes pressing Return
   in an unchanged editor harmless. To actually replace the invalid bytes,
   the user types a different value, as the §3.5 editing notice explains.

10. **Reopening gives the same file structure.** Opening a saved file must
    give the same dialect, encoding, BOM and rows as the document had
    before saving. Where the smallest splice would break this, the
    serializer makes the smallest extra change next to the edit. Property
    tests check this for every generated edit. Two known cases:
    - A field that would put BOM-like bytes at the start of a file without
      that BOM (U+FEFF, or `EF BB BF`, `FF FE` or `FE FF` in a single-byte
      file) is quoted. This generalises decision 7.
    - A row ending in a lone CR followed directly by a row starting with LF
      would read back as one CRLF. The later row's line ending is changed to
      CR (for a blank row) so the two stay separate.

## Consequences

- DESIGN §3.6 and §3.7 get these rules once the ADR is accepted.
- Task 2.4 adds per-column quoting detection to the oracle (decision 2).
- The app disables row and column insertion after an unterminated quote
  (decision 8, task 2.5).
- The serializer's property tests include the reopen check (decision 10).
