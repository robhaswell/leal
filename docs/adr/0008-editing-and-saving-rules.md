# 0008 — Editing and saving: decisions from the phase 1 review

- Status: accepted (approved by Rob, 2026-10-02)
- Date: 2026-10-02

## Context

The phase 1 review checked whether phase 1 leaves phase 2 (editing and
saving) on solid ground. It found several places where phase 1 features
would quietly break once edits exist, or where saving is undefined. Each
needs a rule before tasks 2.1–2.5 start. Nothing in phase 1 depends on
these; they only shape phase 2.

## Decisions

**1. Leal's own save is not an outside change.** After a successful save,
Leal rebases the document onto the file it just wrote:
- it takes a new snapshot (clone, or copy on removable drives) and
  re-indexes;
- it gives the file watcher the new identity, and treats the event from its
  own write as expected rather than as a change elsewhere;
- it clears the "changed elsewhere" flag;
- edits and undo carry on, because commands are stored in logical
  coordinates with old and new values (DESIGN §3.6).

Saving twice in a row never shows a banner or an "overwrite?" prompt.

**2. Edits are visible everywhere.** Every reader goes through the edit
overlay: the grid, find (matches on edited values, and stops matching the
values they replaced), copy (the promise snapshots the overlay at ⌘C), the
inspector, diagnostics markers (an edited cell is re-checked on its own new
value), and column widths and number detection.

**3. The editor always starts from the full value.** The in-cell editor and
the inspector start from the core's full display value, never from the
grid's shortened text or its ↵ ⇥ ␀ symbols. A value too long for the
inspector's 64,000-character view is loaded in full before it can be
edited. Committing a long or multiline value unchanged is no edit.

**4. Re-reading a document that has unsaved edits.**
- **Reload** and **Revert to Saved** ask to discard the edits first. Revert
  goes through the same path as Reload, never AppKit's default
  `read(from:)`.
- **Treat As** and **Reopen with encoding** are disabled while there are
  unsaved edits, with "Save or revert your changes first" as the reason.
  Edits are tied to how the file was split into cells, so they can't move
  across a new delimiter or encoding.
- **The header-row toggle** stays available. It changes only how row 1 is
  displayed, not where the edits are.
- **A drive coming back** keeps the edits, because Leal has confirmed the
  file is unchanged.

**5. Recovering edits after an internal error.** If a document fails
(DESIGN §3.9) while it has unsaved edits, the edits must not be lost. The
app keeps its own record of edit commands (the undo history). The failure
alert offers **Recover changes**: Leal opens the file afresh and replays
the commands into it.
- If the file is unchanged, the window carries on with the edits.
- If it changed, or a command no longer applies, Leal offers Save As of
  what it could recover, and names any edits it couldn't apply.

**6. Save As from an incomplete document** (a drive disconnected, or the
file changed while it was being read). Save As writes only **complete
rows** from the bytes Leal trusts, cut at the last row boundary, with the
user's edits applied. It never writes half a row, half a character or an
open quote. The dialog says plainly that the copy is incomplete ("about N
of M rows"). Nothing is added to the file to mark it.

**7. Save As UTF-8.**
- **What's kept:** line endings, quoting style and delimiters, in meaning.
- **BOM:** a UTF-8 BOM is written only if the original file had a BOM.
- **Attributes:** `com.apple.TextEncoding` is set to UTF-8, and the
  interpretation attribute is rewritten for the new bytes.
- **Text that can't be converted** (an unpaired surrogate or odd final byte
  in UTF-16, or an unmapped byte in a single-byte encoding): Save As refuses
  and names the cells, as F5 requires. The user can edit those cells and
  try again. Nothing is substituted silently.

**8. Remembered interpretation on save.** On every save, Leal writes the
interpretation attribute with a fingerprint of the bytes it just wrote
(ADR-0007) whenever:
- a reopen's first paint or whole-file review would guess differently;
- the user chose the delimiter or header; or
- the choice came from the attribute.

Otherwise it removes any old attribute. The same rule applies to
`com.apple.TextEncoding`, comparing against both the first-64 KB and
whole-file guesses. Attributes copied over from the old file never keep a
stale fingerprint.

**9. Checking for changes right before saving.** The pre-save check opens
the file afresh and reads its identity with `fstat`, which makes network
file systems revalidate. On SMB shares, a change made by another computer
may still not show; this is recorded as a known v1 limitation in DESIGN
§3.1.

**10. No Versions browser in v1.** Autosave-in-place is off (DESIGN §4.3),
and AppKit's Versions browser requires it, so v1 offers **Revert to Saved**
only. DESIGN §4.3 is corrected.

## Consequences

- PLAN tasks 2.1–2.5 gain these as acceptance criteria, marked pending until
  this ADR is accepted.
- DESIGN §3.1, §3.6, §3.7, §3.9 and §4.3 are updated once it is accepted.
