//! Leal's benchmark support (PLAN 1.2b): the reference file generator and
//! the report that CI runs over criterion's results.
//!
//! The benchmarks themselves are in `benches/`, and use only the public API
//! of the crates they measure. Run them with `just bench`. See
//! `docs/tasks/1.2b.md` for how CI runs them and what it checks.

pub mod budgets;
pub mod perf;
pub mod reference;
pub mod report;
