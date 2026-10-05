# 0015 — Report-only budgets for structural saves

- Status: accepted (approved by Rob, 2026-10-05)
- Date: 2026-10-05

## Context

DESIGN §1 says "Save after one edit < 500 ms (a column insert counts as one
edit)". `save/column_insert` (PLAN 2.4c) measures that save: the reference
file (100 MB, about a million rows, 12 columns) gets a column inserted near
the start of every row, then it is saved over itself. Every row's bytes
change, so the writer parses each row and writes it out whole. It isn't a
splice of one edit into a copied file.

That benchmark has been over its budget on the CI runner since 2.4c, and the
Benchmarks workflow has failed on every push to `main` since then (runs
37170907835 to 37290386769):

| Where | `save/column_insert` | `save/column_insert_no_disk` | `save/one_edit` | `save/write_no_disk` |
|---|---|---|---|---|
| CI runner, run 37290386769 (5b5e534) | 1.06 s | 739 ms | 245 ms | 134 ms |
| CI runner, runs since 2.4c | 712 ms–1.06 s | | | |
| M5 Pro, 5b5e534 | 409 ms | 374 ms | | |
| M5 Pro, after task 2.G-b | 339 ms | 303 ms | | |

The runner is roughly as fast as a base M1 Air, the reference machine
(docs/tasks/1.2b.md). The M5 Pro needs about 3× headroom over the
reference machine (DESIGN §1), and it doesn't have that either.

Task 2.G-b profiled the save once, with `sample`. The work is the per-row
walk, about 95% of the save on the M5 Pro. Two cheap fixes took about 20%
off it:

- The encoding census of the bytes written now uses `str::from_utf8`
  instead of `utf8_chunks`, which also ran twice over each piece.
- The parser's field list now starts with room for 16 fields. Growing it
  from empty cost three reallocations a row.

What is left is spread over the walk: parsing each row (about 20%), the
writer's per-row work, assembling the row's bytes (about 15%), and the
allocations for each row's cells. Getting under 500 ms on the runner with
margin would need a different writer: for example, one that writes each
untouched field straight from the mapped file and splices in only the new
field, without parsing the row into cells and putting it back together.
That is a design change to the 2.4c writer, which the fidelity tests
(DESIGN §5) guard closely. It's a lot of risk for an operation that isn't
on the paths Rob cares most about.

Rob, 5 October 2026: "My main goal for performance is on the first paint
and reading the files. If the app is sluggish to do a genuinely slow
operation, such as adding a column to 1 million rows, then that's fine, as
long as there is no obvious performance which is being left on the table.
I don't want to hyperfixate on chasing down budgets."

A second problem came with the first. When a budget failed on attempt 1,
`bench-compare` stopped there, so attempts 2 and 3 never ran and no
regression was judged from 2.4c to the gate. Task 2.G-b fixed that, whatever
this ADR decides. A budget failure now waits until the regressions have
been judged, then fails the job at the end.

## Options

1. **Keep the hard 500 ms budget and rewrite the writer** to splice fields
   (above). It might fit the budget on the runner, but it reopens 2.4c's
   writer and its fidelity cases. It's days of work, done for a budget.
2. **Raise the budget** (for example to 1.5 s) and keep it hard. That's
   simple, but the number would be as arbitrary as the 500 ms was. It
   would also fail again whenever the runner is slow, and it doesn't
   decide anything that Rob asked for.
3. **Make the budget for structural saves report-only.** Over its limit,
   it warns in the job summary and passes. A missing result still fails,
   so the benchmark can't vanish unnoticed. Its no-disk twin
   (`save/column_insert_no_disk`) is still judged against the 20%
   regression threshold between commits, so a slower writer is still
   caught. **Recommended.**
4. **Drop the benchmark.** That loses the number and the regression check.

## Decision

Option 3, approved by Rob at the phase 2 gate (2026-10-05).

- In DESIGN §1, "Save after one edit < 500 ms" stays hard for a cell edit
  (`save/one_edit`) and for row deletes (`save/rows_deleted`).
- A save after a column insert or delete is a structural save: every row is
  rewritten. Its 500 ms target is reported but not enforced.
  `save/column_insert` is in `crates/leal-bench/src/budgets.rs`'s
  `REPORT_ONLY` list. `save/column_delete` (task 2.G-b) has no budget and
  is information only.
- Each structural save keeps a `_no_disk` benchmark, compared between
  commits.
- Main-thread batch operations (paste, clear, duplicate rows) get no new
  budgets. Their benchmarks (`edits/paste_100k_cells`,
  `edits/clear_100k_cells`, `row_edits/duplicate_10k_rows`) are compared
  between commits only.

Task 2.G-b did this in code, marked provisional, so the Benchmarks
workflow could go green while Rob decided. It is now final. To make the
budget hard again, take `save/column_insert` out of `REPORT_ONLY`.

## Consequences

- DESIGN §1's table changes: "Save after one edit < 500 ms" applies to
  cell edits and row deletes. A column insert or delete is a structural
  save with a report-only 500 ms target.
- PLAN: record that Benchmarks was red from 2.4c to the phase 2 gate, and
  why.
- A splicing writer (option 1) stays possible later, if users find
  structural saves slow.
