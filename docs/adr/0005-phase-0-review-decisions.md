# 0005 — Decisions from the phase 0 review

- Status: proposed (Rob decides at the phase 0 gate)
- Date: 2026-10-01

## Context

The phase 0 review (five reviewers, each finding independently verified)
found places where accepted decisions contradict each other, rely on
something that can't work, or leave a case undefined. Each needs an answer
before phase 1 or 2 builds on it. Until Rob accepts this ADR, only docs and
plan changes are made; no product or oracle code depends on it.

## Decisions

**1. What a reopen must preserve, and remembering Leal's interpretation.**
ADR-0004 decision 10 promised "the same dialect" on reopen. But the delimiter
and the header row are guessed from the whole file, like the encoding, so an
edit anywhere can change the guess and no change next to the edit can
prevent it. Decision 10 is narrowed: the bytes guarantee covers the BOM, the
quote character, line endings and every row's values. The delimiter and the
header choice are **remembered**, the same way decision 11 remembers the
encoding: Leal stores them in its own extended attribute
(`io.github.robhaswell.leal.interpretation`) when a reopen would otherwise
guess differently, or when the user chose them. On open, Leal honours the
attribute if the file still parses sensibly with it. Other apps will still
guess for themselves.

**2. Editing a hatched (missing) cell.** The approved mockups show short rows
with hatched empty cells, and the user can type into any cell. Editing a
hatched cell is allowed. Leal appends the delimiters needed to reach that
column, plus the new value, at the end of the row before its line ending.
Nothing else in the row or file changes, which extends F2: "only that
field's bytes, or bytes appended at the end of that row". A blank line
edited in column *c* becomes a row of *c* + 1 fields. Edits past an
unterminated quote are still rejected (ADR-0004 decision 8).

**3. Per-column quoting (ADR-0004 decision 2), made precise.** A column's
fields are the fields at that index in non-blank rows long enough to have
one, header row included. A new field is quoted if the column has at least
one non-empty field and every non-empty field in it is quoted. This is
implemented in task 2.4, in both the oracle and the product.

**4. Encoding at first paint versus the whole-file rule.** First paint can't
read the whole file (DESIGN §3.10), but ADR-0003 rule 1 decides the encoding
from the whole file. So the encoding is chosen in this order: BOM, then the
attribute (ADR-0004 decision 11), then the rule applied to the first 64 KB.
The whole-file rule runs afterwards as P2 work. If it disagrees, Leal shows
a suggestion ("This file looks like Windows-1252 — Reopen as Windows-1252"),
the same way as the delimiter suggestion in DESIGN §3.2. It never re-decodes
silently. The encoding in use is the document's encoding for saving and for
the reopen guarantee.

**5. Supported encodings in v1.**
- **Detected automatically:** UTF-8 (with or without BOM), UTF-16 LE/BE
  with BOM (read-only, DESIGN §4.3), and Windows-1252.
- **Available via the attribute or "Reopen with encoding…" only:** other
  single-byte, ASCII-compatible encodings: Windows-1250, 1251 and 1253–1258,
  ISO-8859-1, ISO-8859-2 and ISO-8859-15, and Mac Roman. These are safe
  because the delimiter, quote and line-ending bytes can't appear inside a
  character.
- **Not supported:** multibyte encodings such as Shift_JIS, whose second
  bytes can equal `|` or `\`.
- **Attribute values** are matched by their CFStringEncoding number. An
  unsupported or unreadable attribute is ignored, with a status bar note.

**6. Cancellation is explicit.** UniFFI's Swift async support doesn't pass
Swift task cancellation through to Rust, so DESIGN §3.9 can't rely on it.
Every long job (indexing, filtering, sorting, saving) has a handle object
with `cancel()`. Calling it sets a flag that the Rust job checks at its
chunk boundaries (DESIGN §3.10 rule 3). Swift wraps each await in
`withTaskCancellationHandler`, which calls `cancel()`. Long work runs on
Rust-owned threads or pools; the async function only reports completion.

**7. Where the clone lives.** `clonefile` only works within one volume, so a
file on an external APFS drive can't be cloned into the boot volume's
temporary folder. Leal clones into a temporary folder **on the file's own
volume** (`FileManager.url(for: .itemReplacementDirectory, …,
appropriateFor:)`). An EXDEV error means "clone elsewhere", not "no
cloning". The read-into-memory and copy fallbacks apply only to volumes
that can't clone at all. Crash cleanup checks every folder Leal recorded.

**8. Changing the interpretation, in the UI.** DESIGN §3.2 and ADR-0004
promise a way to change the delimiter and encoding, but no task builds it
and the mockups don't show it. The core can re-index with a new dialect or
encoding (tasks 1.2/1.3). The app adds these elements:
- the encoding's source in the status bar;
- a **Treat as** delimiter menu;
- **Reopen with encoding…**;
- the suggestion banners from decisions 4 and DESIGN §3.2.

They follow the existing ADR-0002 status-bar and banner styles, with no
separate mockup round. Rob sees screenshots at the phase 1 gate.

**9. Landing test code against a proposed ADR.** CLAUDE.md says design
changes need an approved ADR before code lands. In phase 0, test-oracle code
landed while ADR-0003 and ADR-0004 were still proposed. The rule is made
explicit: **test and oracle code** may land against a proposed ADR, marked
provisional. **Product code** that depends on it waits until the ADR is
accepted.

## Consequences

- ADR-0004 decision 10 is narrowed by decision 1. DESIGN §3.1, §3.2, §3.9
  and §5 (F2) are updated once this ADR is accepted.
- PLAN gains these obligations:
  - 1.1: decision 7;
  - 1.2: decisions 1, 4 and 5;
  - 1.3a/3.1: decision 6;
  - 1.6/1.7: decision 8;
  - 2.1/2.4: decisions 2 and 3;
  - 2.5: writing the interpretation attribute.
- CLAUDE.md gains decision 9.
