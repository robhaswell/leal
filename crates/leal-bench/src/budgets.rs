//! The performance budgets (DESIGN §1) that CI enforces.
//!
//! A task that adds a benchmark for a budget adds a line here, with the
//! benchmark's criterion id (`group/name`). `bench-report` then fails CI's
//! benchmark job when that benchmark's median is over the budget, or when it
//! didn't run at all. Budgets are absolute, so runner noise only matters
//! when a result is already close to its budget, which is worth knowing
//! about anyway. See `docs/tasks/1.2b.md`.

/// One budget: a benchmark whose median must stay at or under `max_ms`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Budget {
    /// The criterion benchmark id, `group/name` (as in `target/criterion`).
    pub id: &'static str,
    /// The budget, in milliseconds.
    pub max_ms: f64,
    /// Where the budget comes from, for the report: `"DESIGN §1"` and so on.
    pub source: &'static str,
}

/// The budgets CI enforces. 1.3a adds first paint (< 150 ms).
pub const BUDGETS: &[Budget] = &[
    // "Full index built: < 500 ms" for the reference file, on the calling
    // thread and through the progressive path the app uses.
    Budget {
        id: "index/build",
        max_ms: 500.0,
        source: "DESIGN §1",
    },
    Budget {
        id: "index/run",
        max_ms: 500.0,
        source: "DESIGN §1",
    },
];
