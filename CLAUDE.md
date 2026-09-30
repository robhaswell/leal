# Leal

Fast, low-memory CSV viewer and editor for macOS that never changes bytes the
user didn't edit. Rust core (`crates/`) + thin Swift/AppKit app (`app/`).

## Read first

- `docs/DESIGN.md` — what we're building and why. It is the source of truth.
- `docs/PLAN.md` — the task list. Work the next unticked task unless told otherwise.
- `docs/adr/` — decisions that changed or refined the design.

## How the build is run

An orchestrating Claude session runs the build and assigns each PLAN task to
an implementer sub-agent working in its own git worktree.

```
task/<id>-<slug>  ──PR──►  phase/<n>  ──phase PR──►  main
```

- **Task PRs target the phase branch** (`phase/0`, `phase/1`, …), never
  `main`. A fresh reviewer agent reviews each task PR, and the orchestrator
  merges it once review findings are fixed and CI is green.
- **Phase PRs target `main`.** They merge only after the phase-end review
  (parallel reviewers for fidelity/correctness, Rust quality, performance,
  test strength with `cargo-mutants`, and Swift/AppKit; each finding verified
  before it is fixed) and **Rob's approval**.
- **Rob approves phase gates and ADRs only.** Everything else is reviewed by
  agents.
- **Implementers never merge, and never push to `main` or a phase branch.**
- **When blocked, stop and report.** If a task needs a design change, a weaker
  fidelity test or a missed performance budget, write an ADR and report back
  instead of working around it.
- **Report back briefly** (about 150 words): status, PR link, test results,
  benchmark results, deviations from the design, and open questions. Details
  go in the PR, not the report.

## Working rules

- **One task per branch and PR**: `task/<id>-<slug>`, branched from and
  targeting `phase/<n>`. Keep PRs to that task.
- **Tests first.** For core work, write the failing tests (including fidelity
  property tests where relevant) before the implementation.
- **Never weaken a fidelity test** (DESIGN §5) to make a change pass. If the
  contract seems wrong, stop and write an ADR for Rob.
- **Design changes need an ADR** (`docs/adr/NNNN-title.md`, template in
  `docs/adr/0000-template.md`) approved by Rob before the code lands.
- **Logic lives in `leal-core`.** Swift handles presentation and macOS
  integration only. `leal-ffi` only wraps.
- **Performance budgets** in DESIGN §1 are requirements. Tasks that touch hot
  paths include a benchmark result in the PR.
- **Dependencies:** prefer well-maintained crates (`memchr`, `memmap2`,
  `encoding_rs`, `rayon`, `regex`, `uniffi`, `proptest`, `criterion`).
  Justify any new dependency in the PR description.
- No `unsafe` outside `source` (mmap) and `leal-ffi`, and each `unsafe` block
  has a `// SAFETY:` comment.

## Commands

- `just check` — fmt check, clippy (`-D warnings`), all tests. Must pass before a PR.
- `just test` / `just bench` / `just run` (build and launch the app).
- Rust toolchain is pinned in `rust-toolchain.toml`; the Xcode project is
  generated from `app/project.yml` by XcodeGen and is not committed.

## Rob is new to Rust

In each PR description, add a short **"Rust notes"** section explaining any
Rust concepts the change relies on (ownership, lifetimes, traits, `Arc`,
async, `unsafe`, etc.) in plain terms, with pointers to the relevant code.
These are collected into the phase report Rob reads at each phase gate. Keep
code straightforward over clever.

## PR checklist

- [ ] Acceptance criteria from `docs/PLAN.md` met; box ticked
- [ ] `just check` passes
- [ ] Tests added (and fidelity tests, if the change touches parsing or saving)
- [ ] Benchmarks included if a hot path changed
- [ ] Rust notes for Rob
- [ ] UI changes: screenshots of the running app next to the approved mockup
