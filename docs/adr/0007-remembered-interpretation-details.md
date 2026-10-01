# 0007 — Remembered interpretation: two details

- Status: proposed (needs Rob's decision)
- Date: 2026-10-01

## Context

ADR-0005 decision 1 has Leal remember its delimiter and header choice in
its own extended attribute, and "honour the attribute if the file still
parses sensibly with it". Building it (task 1.2) needed two more details.

## Decisions

**1. A fingerprint decides when to trust the remembered choice.** The
attribute also records the file's length and a hash of its first 64 KB.
- **If both still match,** Leal uses the remembered choices as they are.
  First paint sees exactly the same bytes Leal saw when it wrote them, so
  they still apply. Hashing takes about 0.06 ms.
- **If either has changed,** something else edited the file, and Leal falls
  back to the "parses sensibly" check from ADR-0005.

The property tests showed why this is needed. A "parses sensibly" check on
its own sometimes threw away choices Leal had saved itself, for example a
header choice the user made on a file the guess would read differently.

The edge case is an outside edit that keeps the length and the first 64 KB
the same. The remembered choices are then still used, which is almost
certainly right, since the start of the file and its size haven't moved.

**2. An attribute saying "us-ascii" is read as UTF-8.** ASCII is a subset of
UTF-8, so this can't misread any valid ASCII file. Today such an attribute
is ignored, with an "unsupported encoding" note.

## Consequences

- ADR-0005 decision 1's "parses sensibly" is refined by decision 1 here.
- Task 1.2 implements both decisions.
