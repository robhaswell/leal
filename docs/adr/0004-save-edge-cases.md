# 0004 — Save edge cases

- Status: accepted (approved by Rob, 2026-09-30)
- Date: 2026-09-30

## Context

Building the save oracle (task 0.2) turned up nine cases that DESIGN §3.6
and §3.7 don't settle. Each needs one answer before the serializer (tasks
2.2 and 2.4) is written. The testkit oracle implements the answers below,
except where a decision says otherwise.

Leal's guiding rules apply throughout: keep the user's data, change as few
bytes as possible, and never change a value silently.

## Decisions

1. **"The file quotes every field"** means every field in every non-blank
   row is quoted.
2. **How new fields are quoted.** A new field (in an inserted row or
   column) is quoted if the file quotes every field, **or if every existing
   non-empty field in that column is quoted**. That second part keeps files
   like pandas' "quote text columns" style consistent. Otherwise it is
   quoted only when needed. *(The oracle checks only the whole file for
   now; task 2.4 adds per-column detection, in the oracle and the product.
   ADR-0005 decision 3, proposed, makes the rule precise for inserted and
   empty columns.)*
3. **Line ending for a new row** in a file that has none (a single row with
   no line ending): LF.
4. **End of file.** Whether the file ends with a line ending is kept.
   Appending a row to a file with no final line ending adds one to the old
   last row and none to the new one. Deleting the last row removes the line
   ending from the new last row. This is the line ending "directly next to"
   the change, which F6 allows. A file whose last row is an unterminated
   quote counts as having no final line ending, since any trailing line
   ending is inside the quoted field: `a\n"b\nc\n` with that row deleted
   becomes `a`.
5. **Column insert or delete with ragged rows.** Rows too short to have that
   column are left untouched, not padded. Blank lines count as too short
   for every column, so a column insert never turns a blank line into data.
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

10. **Reopening gives the same file structure.** *(ADR-0005 decision 1,
    proposed, narrows "the same dialect" here: the delimiter and header
    choice are guessed from the whole file, so they are remembered rather
    than guaranteed.)* Opening a saved file must
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

11. **Remembering a guessed encoding.** Without a BOM, the encoding is a
    guess from the whole file, so an edit anywhere can change what a
    reopen guesses (for example, a Windows-1252 file whose remaining bytes
    happen to be valid UTF-8, so `Ã©` would suddenly show as `é`). No change
    next to the edit can prevent this. Leal records the encoding in the
    standard macOS `com.apple.TextEncoding` extended attribute, as TextEdit
    does, which changes the file's metadata but not its bytes:
    - Leal writes the attribute on save when a reopen would otherwise guess
      a different encoding, and updates it if the file already has one.
    - On open, a valid attribute beats the guess, as long as the bytes
      decode under it. For UTF-8, that means they are valid UTF-8, or the
      usual invalid-encoding warning applies.
    - A UTF-8 or Windows-1252 attribute is always honoured. Overriding a
      UTF-8 attribute because of invalid bytes would defeat it in exactly
      the case it exists for (a file whose guess flipped after an edit).
      Invalid bytes under a UTF-8 attribute get the usual §3.5 warning, and
      the status bar says the encoding came from the file's attribute, with
      **Reopen with encoding…** to override it.
    - A UTF-16 attribute on a file without a UTF-16 BOM is ignored.
    - The attribute doesn't travel everywhere (email, git, some cloud
      drives). Elsewhere the guess applies again; that's a property of the
      file, not something Leal can fix.

## Consequences

- DESIGN §3.6 and §3.7 get these rules once the ADR is accepted.
- Task 2.4 adds per-column quoting detection to the oracle and the
  product (decision 2).
- The app disables row and column insertion after an unterminated quote
  (decision 8, task 2.5).
- The serializer's property tests include the reopen check (decision 10),
  with the encoding attribute modelled (decision 11).
- Tasks 1.1/1.2 read the attribute on open; task 2.5 writes it on save.
