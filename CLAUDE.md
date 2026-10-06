# Leal

Fast, low-memory CSV viewer and editor for macOS that never changes bytes the
user didn't edit. Rust core (`crates/`) + thin Swift/AppKit app (`app/`).

## Read first

- `docs/DESIGN.md` — what we're building and why.
- `docs/PLAN.md` — the task list. Work the next unticked task unless told otherwise.
- `docs/adr/` — decisions that changed or refined the design. **An accepted
  ADR takes precedence over DESIGN** where they disagree; DESIGN is updated
  to match, but may lag. A proposed ADR decides nothing yet.
- Before starting a task, read the ADRs its PLAN entry and its DESIGN
  sections refer to, and the earlier task notes (`docs/tasks/`) its PLAN
  entry names.

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
  `just check` (and `just check-all` if the task touches `app/` or
  `crates/leal-ffi`), fast-forwards `main`, pushes, and deletes the branch and
  worktree. CI runs on every push to `main`. The task is ticked in
  `docs/PLAN.md` once CI is green.
- **Docs-only changes** (ADRs, plan updates, mockups) are committed straight
  to `main`.
- **Token budget** (Rob, 2026-10-03). Usage limits are real, so the build is
  run lean:
  - **Review once, at the right depth.** One combined reviewer per task; two
    only where file safety is at stake (saving, fidelity). Fix must-fix and
    should-fix findings; collect nits for a periodic tidy-up task. Check
    the fixes with one narrow re-review of the changed code, not a full
    round.
  - **Fresh agents for fix rounds.** Give a new agent a short brief (the
    findings and the files involved) instead of resuming one whose context
    is large.
  - **Read narrowly.** Read files in targeted ranges; grep long logs and
    bench output instead of reading them whole.
  - **Smaller tasks.** Split a task bigger than about a day into parts
    (e.g. 2.2 would have been four).
  - **Model by job** (Rob, 2026-10-05). The orchestrating session runs
    Opus 5.5 at high effort. Implementers and the main review of risky
    tasks run Opus 5.5 at **medium** effort (pass the effort explicitly
    where a tool allows it, e.g. a Workflow's `effort: 'medium'`). Sonnet
    for docs merges, CI and flaky-test fixes, narrow re-reviews and
    tidy-ups. Don't change `settings.json` for this.
  - **Short task notes.** At most about 200 lines; review history goes in
    commit messages, not the notes.
  - **At most two agents at once.**
- **Phase gates.** At the end of each phase, parallel reviewers cover
  fidelity/correctness, Rust quality, performance, test strength
  (`cargo-mutants`) and Swift/AppKit. Each finding is verified before it is
  fixed. Rob reads the phase report and approves the phase; the approved
  commit is tagged `phase-<n>`. The next phase starts after approval.
- **Lessons from phase 2** (Rob, 2026-10-06):
  - **Watch CI after every push to `main`** (`gh run list`,
    `gh run view --log-failed`), including Deep tests and Benchmarks. Don't
    start the next task while any of them is red. A run cancelled by a newer
    push hasn't passed, so check the run for the commit you mean. Poll `gh`
    from Python, not a zsh loop (`set -- $var` doesn't split words).
  - **Nits never go into fix rounds.** They wait for the tidy-up task. A nit
    added to a fix round caused a must-fix bug (journal pruning, 2.G).
  - **Before landing a change to the core's save or edit paths**, run the
    saving and edit property tests at `PROPTEST_CASES=20000`. Deep tests
    found two bugs that every per-task review missed.
  - **Tests never show real UI.** The `NoRealUI` net fails any un-stubbed
    alert or panel. Stub sheets through the existing hooks.
  - **Each agent uses its own log and scratch paths.** At every landing,
    check for leftover disk images (`hdiutil info`) and stray Leal processes
    that an agent started.
  - **Pause agents before timing-sensitive runs** (`just perf`). Their
    builds and benches skew the numbers.
  - **Budgets are hypotheses.** Hold hard budgets on opening, first paint,
    reading and scrolling. For slow edits, saves and undo, look only for
    obvious waste; their budgets are report-only (ADR-0015).
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
  Test and oracle code may land against a proposed ADR, marked
  provisional; product code that depends on it waits until the ADR is
  accepted (ADR-0005 decision 9).
- **Logic lives in `leal-core`.** Swift handles presentation and macOS
  integration only. `leal-ffi` only wraps.
- **Performance budgets** in DESIGN §1 are requirements. Tasks that touch hot
  paths include a benchmark result in their task notes.
- **Dependencies:** prefer well-maintained crates (`memchr`, `memmap2`,
  `encoding_rs`, `rayon`, `regex`, `uniffi`, `proptest`, `criterion`).
  Justify any new dependency in the task notes.
- No `unsafe` outside `source` (mmap) and `leal-ffi`, and each `unsafe` block
  has a `// SAFETY:` comment.
- **Never kill Leal (or any app) by name.** Several agents run Leal test
  hosts and benchmarks on this Mac at once, so `pkill Leal` or
  `killall Leal` can kill another agent's run. Kill only processes you
  started, by the PID you recorded. Agents don't use `just run` (it quits
  any running Leal for Rob's convenience).
- **Never send synthetic input to the system.** No AppleScript/System Events
  keystrokes, `cliclick`, CGEvent posting or similar: whatever app is frontmost
  (possibly one of Rob's) receives it. Drive the app from inside itself
  (launch arguments, test hooks, XCTest/XCUITest), and quit anything you
  launch.

## Commands

- `just check` — fmt check, clippy (`-D warnings`), all Rust tests. Must pass before every commit.
- `just check-all` — `just check` plus the app's XCTest suite. Must pass
  before landing any task that touches `app/` or `crates/leal-ffi`.
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
- [ ] `just check` passes (and `just check-all` if `app/` or `crates/leal-ffi` changed)
- [ ] Tests added (and fidelity tests, if the change touches parsing or saving)
- [ ] Benchmarks included if a hot path changed
- [ ] Rust notes for Rob
- [ ] UI changes: screenshots of the running app next to the approved mockup
