# 0006 — Files on removable drives

- Status: accepted (option C, approved by Rob, 2026-10-01); network shares changed by ADR-0009
- Date: 2026-10-01

## Context

Leal reads files through a memory map of a clone (DESIGN §3.1). ADR-0005
decision 7 puts the clone on the file's own volume, so files on external
APFS drives are cloned there instead of being copied into memory.

The task 1.1 review found the cost: if a removable drive is **unplugged or
force-ejected** while a file on it is open, touching the mapped pages kills
Leal with SIGBUS. Every window closes and all unsaved edits are lost. The
reviewer reproduced this with a disk image and `hdiutil detach -force`.

A normal eject is safe: macOS refuses to eject a drive while Leal has the
clone open ("the disk is in use"). The risk is only an unplug without
ejecting, or a forced eject. Internal drives can't disappear. Network
volumes can't clone, so they already use the read-into-memory or copy
fallbacks, which are safe.

## Options

**A. Accept and document it.** Keep today's behaviour and warn about it in
the docs.
- Simplest, and fastest everywhere.
- A pulled cable loses every unsaved edit in every window, which goes
  against the spirit of "never lose the user's data".

**B. Treat removable drives like network volumes.** Read the file into
memory (up to 512 MB) or copy it to the internal disk **before** first
paint.
- Simple and safe.
- First paint waits for the whole file. A 100 MB file on a USB hard disk
  takes about 1 s, against a 150 ms budget, and memory use rises by the
  file size for files up to 512 MB.

**C. Read safely first, map later (recommended).** For files on removable
volumes:
1. **First paint** reads the first screen with ordinary reads. These return
   an error if the drive vanishes; they never crash. The 150 ms budget is
   kept.
2. **Indexing** (P1) streams the file once. The same pass builds the row
   index and writes a copy to Leal's temporary folder on the internal disk.
3. **Until the copy is complete,** rows are read with ordinary reads from
   the clone on the external drive. Once the copy is complete, Leal maps
   the internal copy and drops the external clone.
4. **If the drive disappears before the copy completes,** Leal shows a
   banner saying the drive was disconnected. The document stays open with
   the rows it has already read and the user's edits intact. Saving is
   blocked until the drive is back, because the unread bytes are needed to
   write the file; **Save As** is offered with an explanation.

C is safe and keeps first paint fast, at the cost of more code in task 1.1.
It needs an "ordinary reads" path alongside the map; the row parser already
works on byte slices, so that is a contained change. It also uses disk
space on the internal drive equal to the file size, but no extra memory.

## Decision

**C**, approved by Rob on 2026-10-01.

## Consequences

- DESIGN §3.1 gains the removable-volume path. Task 1.1 adds it
  (detection via the volume's "is internal" and "is ejectable" properties,
  ordinary reads, copy during indexing, the switch to the map, and the
  disconnect banner's core state). Task 1.7 shows the banner.
- Task 1.1 landed before this decision, with the risk documented in its
  `SAFETY` comment and task notes. Task 1.1a implements option C.
