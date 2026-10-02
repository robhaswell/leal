# 0012 — The core does the safe save; four save details

- Status: accepted (approved by Rob, 2026-10-03)
- Date: 2026-10-03
- Changes: DESIGN §3.7 ("the core writes to the temporary URL AppKit
  provides, which is then swapped in atomically")

## Context

Task 2.2 built the save writer. Four parts need a decision, because they
change DESIGN or settle a conflict between two ADRs. Details are in
`docs/tasks/2.2.md`, "Decisions and interpretations".

## Decisions

**1. The core does the atomic replace, not NSDocument.**

DESIGN §3.7 has AppKit swap the written file into place. In the build, the
core does it, all under one lock:
- the check that the file hasn't changed elsewhere;
- writing the new file in a folder on the same volume;
- copying permissions, extended attributes, ACLs and Finder metadata;
- writing Leal's two attributes with the new file's fingerprint;
- the rename into place, with the watcher told to expect it;
- the rebase onto the new file (ADR-0008 decision 1).

If AppKit does the swap, there is a gap between Leal's check and the
rename. A change made in that gap goes unseen, and Leal's own rename can
show up as an outside change. With the core doing it, Leal's own watcher
can't miss a change: the rename swaps the files (`renamex_np` with
`RENAME_SWAP`), Leal checks the file it swapped out against the one it
checked, and swaps back and refuses if they differ. A write by another
process through a descriptor it already holds can still land in the old
file after the swap; no Mac API closes that.

Taking the replace from NSDocument means the core takes over NSDocument's
guards too:
- **Files the user can't write.** A rename only needs write access to the
  folder, so the core checks that the file itself is writable and not
  locked before it writes anything. If either check fails, it refuses with
  a distinct reason, so the app can offer Duplicate or Unlock.
- **Metadata is copied best-effort, by an explicit policy.** Each
  extended attribute is copied on its own, following the system's save
  rules (`XATTR_OPERATION_INTENT_SAVE`) plus a keep-list (Finder info,
  tags). Attributes that can't be set are skipped and logged, so one
  protected attribute never makes a file unsaveable. Leal's two attributes
  are then written, and the ACL, mode and flags come last. Task 2.5 overrides NSDocument's save to start
the core's job. Nothing changes for the user: the replace is still atomic
and metadata is kept. It still works in the sandbox, because the app hands
the core the replacement folder that `FileManager` gives it.

The other option is to keep NSDocument's swap. The core would then need a
"write to this path" mode and a separate rebase, and 2.5 would have to stop
the watcher reporting AppKit's swap as an outside change. It is more code,
and the gap above stays.

**2. Output of 4 GiB or more is refused before anything is written.**

Leal can't open files that large (DESIGN §1 non-goal, 32-bit offsets), so
the reopen guarantee (ADR-0004 decision 10) can't hold. F5 says a save
either succeeds exactly or stops with an explanation. The save computes
the output length first and refuses with "too large". The other option is
to write the file anyway and refuse only to reopen it.

**3. A BOM file that already has `com.apple.TextEncoding` keeps it,
updated.**

Two ADRs disagree here. ADR-0004 decision 11, and the oracle, keep and
update the attribute. ADR-0008 decision 8, read literally, removes it,
because a BOM already says the encoding. The build follows ADR-0004 and the
oracle. Keeping it is harmless and matches what other apps wrote. The
other option is to remove it, following ADR-0008 decision 8 literally, and
change the oracle.

**4. Undo across a save works by value.**

After a save, the saved file is the new base (DESIGN §3.6), and undo
puts a cell's earlier value back; it doesn't restore the earlier bytes.
So undoing an edit to a missing cell after saving leaves an empty field
(`a,,` instead of `a`), and a field that was saved quoted stays quoted.
F3 ("undoing all edits restores byte-identical output") holds from the
last save, not from the file as first opened. Restoring the bytes would
need a structural "remove fields" command, and the user would see the
same values either way.

## Consequences

- DESIGN §5's F3 says it holds from the last save (decision 4).
- ADR-0008 decision 8's wording on BOM files is read as ADR-0004
  decision 11 (decision 3).
- DESIGN §3.7's safe-save paragraph is rewritten (the proposed wording is
  in `docs/tasks/2.2.md`, "Proposed DESIGN wording").
- PLAN 2.5 overrides NSDocument's save to start the core's save job, and
  for "nothing changes for the user" to hold it must also:
  - wrap the job in an `NSFileCoordinator` write with `.forReplacing`
    (the document as file presenter), serialised with
    `performAsynchronousFileAccess`, so other apps, iCloud Drive and File
    Provider folders get coordinated notice and NSDocument doesn't react
    to its own save;
  - set `fileModificationDate` from the save's outcome, so NSDocument's
    own "changed by another application" check doesn't fire;
  - always pass an item-replacement folder on the file's volume, with a
    defined fallback where AppKit can't make one (shares, FAT).
- The main thread never waits for a save (DESIGN §3.9): the save holds
  the document's lock only to take a snapshot of the edits and to swap in
  the result, and edits made during the save carry over.
