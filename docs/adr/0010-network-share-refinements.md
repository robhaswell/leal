# 0010 — Network shares: refinements from the build

- Status: proposed
- Date: 2026-10-02
- Refines: ADR-0009

## Context

Task 2.0 built ADR-0009, and two rounds of review tightened it. The build
now differs from the ADR's wording in a few places. Every difference is
stricter or safer than the ADR, but CLAUDE.md says an accepted ADR takes
precedence over DESIGN. These differences therefore need Rob's approval to
stand. DESIGN §3.1 already describes the build. Details are in
`docs/tasks/2.0.md`.

## Options

1. Accept the refinements below.
2. Revert the build to ADR-0009's letter. This would bring back the
   problems the reviews found: a server restart reported as "deleted", a
   network blip never recovering, and a file replaced by another computer
   reported as deleted.

## Decision

Option 1, if Rob approves.

1. **More network errors are retried.** ADR-0009 names six errnos. The
   build also retries EIO, ENOTCONN, ECONNREFUSED, ECONNABORTED, EPIPE,
   ESHUTDOWN and EAGAIN, which smbfs returns during a blip. "Briefly" means
   a 3.5 s wall-clock window, backing off from 100 ms. Any other failure on
   a share means Disconnected, never a plain error that leaves Save on.
2. **ESTALE or ENOENT is checked, not trusted.** ADR-0009 reports these as
   "deleted by another computer". On NFS, ESTALE is usual after a server
   restart, and after another computer saves by renaming over the file.
   Leal now looks at the file's current path:
   - the same file is there → Disconnected (it reconnects);
   - a different file is there → changed while reading (Reload);
   - nothing is there and the folder is on the same share → Deleted;
   - anything else → Disconnected.
3. **The share is never read on the main thread at all,** first paint
   included. Every open, Reload and file check runs off the main thread,
   for every file, not only shares.
4. **Additions the ADR didn't mention:**
   - a Deleted state with its own banner, offering Save As;
   - a check every 5 s while a share is disconnected, stopping after three
     identical failures;
   - the first-paint bytes compared with the copy, and a short read counted
     as a change;
   - a share's files closed on their own thread, so closing a window can't
     hang.
5. **Network home folder (known limit).** If Leal's temporary folder is
   itself on a share, the copy can't be on the internal disk. Leal reads it
   rather than mapping it, so it can't crash if the share drops. Such a file
   gets no whole-file review, and a hung home share can still stall the
   window.

## Consequences

- DESIGN §3.1 already matches (commit `de6b5ac`).
- Save As from an incomplete document (ADR-0008 decision 6) also covers
  "deleted on another computer while reading". DESIGN §3.7 and PLAN 2.2 and
  2.5 are updated to say so once this is accepted.
