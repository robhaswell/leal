# Leal

Fast, low-memory CSV viewer and editor for macOS that never changes bytes the
user didn't edit. Rust core (`crates/`) + thin Swift/AppKit app (`app/`).

## Read first

- `docs/DESIGN.md` — what we're building and why. It is the source of truth.
- `docs/PLAN.md` — the task list. Work the next unticked task unless told otherwise.
- `docs/adr/` — decisions that changed or refined the design.

## How the build is run

An orchestrating Claude session runs the build and assigns each PLAN task to
an implementer sub-agent. Branches are used only where they help mechanically;
there are no pull requests and no long-lived branches.

- **Each task works on a local branch in its own git worktree** (`task/<id>`).
  This isolates agents running in parallel, keeps `main` green while a task is
  in progress, and gives the reviewer an exact diff (`main...task/<id>`).
  Task branches are never pushed.
- **Review before landing.** When the implementer is done, a fresh reviewer
  agent reviews the branch diff and the implementer fixes the findings on the
  branch. This happens before the task lands, so later tasks never build on
  unreviewed code.
- **Landing.** The orchestrator rebases the branch onto `main`, runs
  `just check`, fast-forwards `main`, pushes, and deletes the branch and
  worktree. CI runs on every push to `main`. The task is ticked in
  `docs/PLAN.md` once CI is green.
- **Docs-only changes** (ADRs, plan updates, mockups) are committed straight
  to `main`.
- **Phase gates.** At the end of each phase, parallel reviewers cover
  fidelity/correctness, Rust quality, performance, test strength
  (`cargo-mutants`) and Swift/AppKit. Each finding is verified before it is
  fixed. Rob reads the phase report and approves the phase; the approved
  commit is tagged `phase-<n>`. The next phase starts after approval.
- **Rob approves phase gates and ADRs only.** Everything else is reviewed by
  agents.
- **Commit messages** start with the task ID, e.g. `1.3: Build quote-aware
  row index`. Keep each commit building and passing `just check`.
- **When blocked, stop and report.** If a task needs a design change, a weaker
  fidelity test or a missed performance budget, write an ADR and report back
  instead of working around it.
- **Report back briefly** (about 150 words): status, commit range, test
  results, benchmark results, deviations from the design, and open questions.
  Put detail in commit messages and the task notes (below), not the report.

## Working rules

- **Stay within the task.** Don't mix unrelated changes into a task's commits.
- **Tests first.** For core work, write the failing tests (including fidelity
  property tests where relevant) before the implementation.
- **Never weaken a fidelity test** (DESIGN §5) to make a change pass. If the
  contract seems wrong, stop and write an ADR for Rob.
- **Design changes need an ADR** (`docs/adr/NNNN-title.md`, template in
  `docs/adr/0000-template.md`) approved by Rob before the code lands.
- **Logic lives in `leal-core`.** Swift handles presentation and macOS
  integration only. `leal-ffi` only wraps.
- **Performance budgets** in DESIGN §1 are requirements. Tasks that touch hot
  paths include a benchmark result in their task notes.
- **Dependencies:** prefer well-maintained crates (`memchr`, `memmap2`,
  `encoding_rs`, `rayon`, `regex`, `uniffi`, `proptest`, `criterion`).
  Justify any new dependency in the task notes.
- No `unsafe` outside `source` (mmap) and `leal-ffi`, and each `unsafe` block
  has a `// SAFETY:` comment.

## Commands

- `just check` — fmt check, clippy (`-D warnings`), all tests. Must pass before every commit.
- `just test` / `just bench` / `just run` (build and launch the app).
- Rust toolchain is pinned in `rust-toolchain.toml`; the Xcode project is
  generated from `app/project.yml` by XcodeGen and is not committed.

## Rob is new to Rust

In each task's notes, add a short **"Rust notes"** section explaining any
Rust concepts the change relies on (ownership, lifetimes, traits, `Arc`,
async, `unsafe`, etc.) in plain terms, with pointers to the relevant code.
These are collected into the phase report Rob reads at each phase gate. Keep
code straightforward over clever.

## Task notes and checklist

Each task writes `docs/tasks/<id>.md` with what was built, benchmark results,
new dependencies, deviations, and Rust notes. The phase report is compiled
from these.

- [ ] Acceptance criteria from `docs/PLAN.md` met; box ticked after review
- [ ] `just check` passes
- [ ] Tests added (and fidelity tests, if the change touches parsing or saving)
- [ ] Benchmarks included if a hot path changed
- [ ] Rust notes for Rob
- [ ] UI changes: screenshots of the running app next to the approved mockup
