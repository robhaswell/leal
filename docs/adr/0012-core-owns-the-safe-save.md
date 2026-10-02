# 0012 — The core does the safe save; three save details

- Status: proposed
- Date: 2026-10-03
- Changes: DESIGN §3.7 ("the core writes to the temporary URL AppKit
  provides, which is then swapped in atomically")

## Context

Task 2.2 built the save writer. Three parts need a decision, because they
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
show up as an outside change. Task 2.5 overrides NSDocument's save to start
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

## Consequences

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
- Product code from task 2.2 lands once this is accepted.
