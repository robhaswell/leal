# Phase 1 report — Viewer

Date: 2026-10-02 · Status: **awaiting Rob's approval**

## What Rob needs to decide

The full list, with a recommendation for each, is in
[`phase-1-decisions.md`](phase-1-decisions.md). The main ones:

1. **ADR-0008**: ten editing and saving rules for phase 2.
2. **The budget definitions**: what idle memory and the heap budget count,
   and whether the open budget is cold or warm.
3. **Scrolling on an unlocked Mac.** Every scroll measurement so far was
   taken with the screen locked. Run `just perf` once, unlocked.
4. **Screenshots of the UI that has no mockup** (30 images,
   `docs/tasks/p1-gate-*.png`).
5. **Network shares**: keep reading them in full at open, as ADR-0006
   says (recommended), or stream them.

## What was built

Leal now opens and displays CSV files.

| Task | Result |
|---|---|
| 1.1, 1.1a | Files are opened without copying them. Removable drives are read safely and copied to the internal disk, so pulling the drive never crashes the app. |
| 1.2, 1.2b | The delimiter, encoding and header are detected and remembered. A 100 MB reference file and CI benchmarks were added. |
| 1.3, 1.3a | A quote-aware row index. The work scheduler shows the first screen in about 2 ms while everything else runs in the background. |
| 1.4, 1.5 | Rows and fields, and warnings for messy files. Every warning matches the test corpus exactly. |
| 1.6, 1.6a | The document window, with the custom-drawn grid. |
| 1.7 | The warning banner and details, Treat As and Reopen with encoding, and the drive banners. |
| 1.8 | Find, Go to Row, Copy as TSV, and the cell inspector. |
| 1.9 | Leal notices when another app changes, moves, deletes or safe-saves the file, and when a drive comes back. |
| 1.10 | Every viewer budget measured, in [`docs/perf.md`](../perf.md). |

At the end of the phase there are 620 Rust tests and 135 app tests. The
deep property tests run 20,000 cases on every push and 100,000 nightly, in
6 shards. CI checks that no test or benchmark code ships in the release
app.

## Performance (M5 Pro, screen locked)

| Budget | Result |
|---|---|
| Launch to empty window | 110 ms — pass |
| Open to first rows | 95 ms cold, 36 ms warm — pass |
| Full index, 100 MB | 120 ms — pass |
| Scrolling | Some late frames — **fail, probably an artefact of the locked screen**: the main thread was on a slow core |
| Heap | 28 MB settled; peaks of 39–50 MB, about 21 MB of which is AppKit's per window — **depends on what the budget counts** |
| Idle | 10.9 MB footprint — pass, if footprint is the measure |

## How it was reviewed

- **Every task** was reviewed before landing, most of them twice, and
  several three times.
- **The phase review** ran five reviewers (fidelity, concurrency,
  the app, test strength, consistency), each followed by a verifier.
  - It confirmed 46 findings, 5 of them must-fix, and refuted 2.
  - Every confirmed finding is fixed, apart from the phase 2 design
    questions, which went into ADR-0008.
  - The fixes were reviewed again before landing.
  - The fidelity reviewer compared everything the app shows, finds and
    copies against the test harness, in every encoding and on the
    removable-drive path, and found no disagreement.

Notable problems the reviews caught:
- Unplugging an external drive would have crashed Leal (ADR-0006).
- A normal safe-save by another app was reported as "deleted".
- Copy waited about 250 ms and could paste stale data.
- A title-bar layout crash happened with a second display attached.
- Several races appeared only when features were combined.

## Things to know

- **CI flakiness was fixed at the root** wherever it came up: disk images,
  timing tests, and benchmark noise. A regression now has to show up in
  every attempt.
- **Agents interfering with each other.** One agent killed another's test
  app by name. CLAUDE.md now forbids that.
- **What's left unfixed.** One priority-inversion warning remains, inside
  AppKit's own text input. It is not Leal's code.
- **Not tested.** Removable drives can't be tested end to end inside the
  sandbox. A five-minute manual check with a real USB stick is on the
  decisions list.

## Next: phase 2 (Editing)

Phase 2 covers:
- the edit overlay and undo;
- the splice serializer, with the fidelity property tests the phase 0
  oracle was built for;
- encoding on save;
- inserting and deleting rows and columns;
- in-place editing and Save, Save As and Revert, under ADR-0008's rules.
