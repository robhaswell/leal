# 0003 — Parsing and diagnostics clarifications

- Status: accepted (approved by Rob, 2026-09-30)
- Date: 2026-09-30

## Context

Building the test harness (task 0.2) needed an exact oracle for the parser.
Writing it showed six places where DESIGN §3.2, §3.4 and §3.5 are ambiguous
or contradict each other. The parser (tasks 1.2–1.5) needs one answer for
each. None of these changes a goal; they make existing rules precise.

## Decisions

**1. Invalid UTF-8 versus Windows-1252.** §3.2 said "UTF-8 if valid;
otherwise single-byte", which would mean the invalid-UTF-8 warning in §3.5
could never fire. Rule: if the file has at least one valid multibyte UTF-8
sequence and valid multibyte sequences outnumber invalid bytes, it is
**UTF-8 with an invalid-encoding warning**. Otherwise, a file with high bytes
is single-byte (Windows-1252 by default). A file that is pure ASCII is UTF-8.

**2. Display value of text after a closing quote.** ADR-0002 (question 6)
already decided this: the grid shows such a field **raw, exactly as its
bytes read**. So `"a"b` displays as `"a"b`, not `ab`. The field's raw span
covers every byte from its first byte to the next delimiter or line end.

**3. Quotes inside text after a closing quote** are literal. They don't
reopen quoting. For example, `"a"b"c",` is one field with raw bytes
`"a"b"c"`.

**4. What counts as one occurrence, and tie-breaks.**
- Per **row**: ragged row, blank line, mixed line endings (one per row whose
  line ending differs from the dominant one).
- Per **field**: text after closing quote, invalid encoding, NUL bytes.
- Per **file**: unterminated quote (at most one, since it runs to end of
  file), BOM present.
- **Dominant field count** (for ragged rows) is the most common field count.
  A tie goes to the count that appears first in the file.
  Blank lines are left out of this count and are never also reported as
  ragged rows.
- **Dominant line ending** is the most common one. A tie goes to the first
  one seen.
- Locations are the first 1,000 occurrences in file order.

**5. Blank lines.** A blank line is a row with no bytes before its line
ending, anywhere in the file, including at the end. The file's final line
ending is not itself a blank line: `a\nb\n` has two rows and no blank lines,
while `a\nb\n\n` has three rows, the last of them blank.

**6. Offsets.** All positions in the core (row index, field spans,
diagnostic locations and test sidecars) are **byte offsets into the file as
stored**. That includes UTF-16 files; transcoding is for display only. Row
and field numbers are 0-based physical positions.

**7. UTF-16 diagnostics.** In a UTF-16 file, a NUL is a U+0000 code unit,
not a 0x00 byte (every ASCII character in UTF-16 contains a 0x00 byte). An
unpaired surrogate is reported as `invalid_encoding`, once per field, and is
shown as U+FFFD, the same as invalid UTF-8. In both cases the location is
the byte offset of the code unit's first byte.

## Consequences

- DESIGN §3.2, §3.4 and §3.5 get these rules written into them once this
  ADR is approved.
- The testkit oracle, the corpus sidecars and the task notes for 0.2 follow
  this ADR.
