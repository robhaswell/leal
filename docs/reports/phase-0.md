# Phase 0 report — Foundations

Date: 2026-10-01 · Status: **awaiting Rob's approval**

## What Rob needs to decide

1. **Approve phase 0.** If approved, `main` at the approved commit is tagged
   `phase-0` and phase 1 starts.
2. **Approve ADR-0005** (nine decisions from the phase 0 review), or say
   which decisions to change. Phase 1 tasks that depend on it are marked
   "(ADR-0005, pending)" in PLAN.md and won't start their pending parts
   until it's accepted.
3. **Real-world test files (task 1.2a).** Could you supply a few genuine
   exports from Excel (both "CSV UTF-8" and "CSV (Windows)"), Numbers and
   Google Sheets? Any small non-sensitive spreadsheet will do. If they
   haven't arrived by the phase 1 gate, the task moves to phase 4.

## What was built

| Task | Result |
|---|---|
| 0.1 Workspace scaffold | Cargo workspace (core, FFI, CLI), `justfile`, macOS CI. |
| 0.2 Test harness and corpus | `leal-testkit`: an independent oracle for parsing, diagnostics and saving, property-test generators, fidelity assertions, and a 43-case byte-exact corpus with expected results. |
| 0.3 Walking skeleton | Rust core → UniFFI → Swift → a running AppKit app, from one command (`just run`). Universal, ad-hoc-signed builds. |
| 0.4 Grid spike | Measured four grid designs. ADR-0001: a custom-drawn grid, with a row-drawing `NSTableView` as the fallback. |

Tests on `main`: 129 Rust tests (plus deep runs of 20,000 cases per push and
100,000 nightly), 6 app tests, a rustdoc gate and an `actionlint`-checked CI
that builds the release app for both architectures.

## Decisions made

| ADR | Decision | Status |
|---|---|---|
| 0001 | Custom grid (B), row-drawing table (C) as fallback | Accepted |
| 0002 | UI mockups | Accepted |
| 0003 | Parsing and diagnostics rules | Accepted |
| 0004 | Save edge cases (11 decisions) | Accepted |
| 0005 | Decisions from the phase 0 review (9) | **Proposed** |

## How it was reviewed

Every task was reviewed by a fresh agent before landing, and the big ones
twice. Review rounds found real problems before they reached `main`, for
example:
- The test oracle showed `"a"b` as `ab`, contradicting the approved mockups.
- The grid spike's tuning of `NSTableView` was broken, which made its main
  argument wrong. The re-measurement kept the recommendation but changed
  the reasoning.
- Builds could link a stale Rust library, and a test-only function could end
  up in a release-folder app.

The **phase-end review** ran five reviewers in parallel (fidelity, Rust
quality, test strength with `cargo-mutants`, the app and build, and docs
consistency), each followed by a verifier that tried to disprove every
finding.
- **47 findings were confirmed** (5 must-fix) and 1 was refuted.
- **46 are fixed.** The remaining one (structured I/O errors for Swift) is
  deferred to task 1.1, where the file-opening code is rewritten.
- The design questions behind 5 findings went into ADR-0005.
- The fixes were reviewed again before landing.

Notable fixes:
- The shipped Rust library wasn't getting link-time optimisation. It's now
  6 MB instead of 19 MB, and the app's framework is 1.1 MB.
- Swift can't cancel Rust work through UniFFI's async support, so
  cancellation is now an explicit design (ADR-0005 decision 6).
- Every phase 1–4 task now carries the obligations from the ADRs.

## Performance

Phase 0 has no product hot paths yet. The grid spike measured, at 200
columns on an M5 Pro (not the M1 Air reference):
- the custom grid: 0.02% of frames late, with even the slowest 1% of frames
  well inside the 8.3 ms budget;
- the row-drawing table: 1.9% of frames late.

Full results are in `docs/tasks/0.4-results.md`. To rerun the comparison
yourself on an unlocked Mac (about 3 minutes), use the command in ADR-0001.

## Things to know

- **Process deviation:** task 0.2 landed while ADR-0003 and ADR-0004 were
  still proposed, against CLAUDE.md's rule. It was test code only, and both
  ADRs were accepted afterwards. ADR-0005 decision 9 proposes making that
  allowed for test code only.
- **CI runs on GitHub's Xcode 27 preview runners** to match your Xcode. The
  preview has no uptime guarantee; the fallback (macOS 26 with Xcode 26.6)
  is documented.
- One agent's automation sent keystrokes to your frontmost app early on.
  That's now forbidden in CLAUDE.md, and nothing similar has happened since.

## Rust notes

Each task's notes explain the Rust concepts it used, in plain terms:
- `docs/tasks/0.1.md`: workspaces, crates, crate types, editions, lints and
  nextest.
- `docs/tasks/0.2.md`: dev-dependencies, integration vs unit tests, property
  testing and `prop_assert!`.
- `docs/tasks/0.3.md`: FFI, UniFFI, proc macros, `staticlib`, errors across
  the boundary, and why panics matter there.

## Next: phase 1 (Viewer)

Tasks 1.1–1.10:
- opening files without copying them;
- detecting the format;
- the row index;
- the work scheduler that keeps first paint fast;
- diagnostics;
- the real document window with the custom grid;
- find, go to row, copy, and the cell inspector;
- external-change detection;
- the viewer performance milestone.
