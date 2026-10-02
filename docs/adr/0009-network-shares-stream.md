# 0009 — Network shares stream like removable drives

- Status: accepted (decided by Rob, 2026-10-02)
- Date: 2026-10-02
- Changes: ADR-0006 (network shares no longer read or copy in full at open)
- Refined by: ADR-0010

## Context

ADR-0006 reads a file on a network share (SMB, NFS) in full, or copies it,
before first paint. That is safe but slow. A 100 MB file over gigabit takes
about a second, against the 150 ms first-paint budget. Removable drives
already use ADR-0006 option C: ordinary reads for the first screen, a
streamed copy to the internal disk in the background, then a switch to the
internal copy. Task 1.1a's notes proposed using the same path for shares.

## Decision

Network shares use the removable-drive path (ADR-0006 option C):
1. First paint uses ordinary reads of the first screen.
2. The indexing pass streams the file and copies it to Leal's temporary
   folder on the internal disk.
3. Leal maps the internal copy once the copy is complete.
4. A share that drops out mid-copy gives the same "disconnected" state and
   banner as an unplugged drive.

Rob's priority is first paint, so the extra work and risk are accepted.
Shares have these safety rules on top:

- **Never read uncopied bytes on the main thread.** A hard NFS mount or an
  SMB reconnect can block a read for a long time. Rows not yet copied are
  read on a background thread, and the grid shows them as loading until
  they arrive.
- **Network errors are ambiguous, not fatal.** ETIMEDOUT, EHOSTDOWN,
  EHOSTUNREACH, ENETDOWN, ENETUNREACH and ECONNRESET are retried briefly in
  the background before Leal treats the share as disconnected. On a share,
  ESTALE or ENOENT may mean another computer deleted the file, and is
  reported as that, not as a disconnect.
- **Changes are detected less reliably.** Network clients cache file
  details, so a change made by another computer during the copy may go
  unnoticed. The pre-save re-check (ADR-0008 decision 9) still opens the
  file afresh and reads it with `fstat`. The remaining SMB limit is a known
  v1 limitation in DESIGN §3.1.

## Consequences

- PLAN gains task **2.0 Network shares stream**, done before the phase 2
  editing tasks, because it changes the source layer they build on.
- DESIGN §3.1 is updated: shares follow the removable-drive path.
- The routing switch noted in `docs/tasks/1.1a.md` is the starting point.
