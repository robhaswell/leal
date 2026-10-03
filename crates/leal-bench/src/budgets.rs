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

/// The budgets CI enforces.
pub const BUDGETS: &[Budget] = &[
    // "Open to first rows visible: < 150 ms, independent of file size,
    // before indexing finishes", with P1–P3 work forced to run at the same
    // time (DESIGN §3.10, "Measuring it"): from an internal volume, and
    // through the removable-drive path (ADR-0006).
    Budget {
        id: "open/first_paint_under_load",
        max_ms: 150.0,
        source: "DESIGN §1",
    },
    Budget {
        id: "open/first_paint_removable_under_load",
        max_ms: 150.0,
        source: "DESIGN §1",
    },
    // A file on a network share (ADR-0009) keeps the same first-paint
    // budget: the reason the share takes the removable path.
    Budget {
        id: "open/first_paint_share_under_load",
        max_ms: 150.0,
        source: "DESIGN §1, ADR-0009",
    },
    // A share whose every read takes 20 ms: first paint is one round trip,
    // whatever the file's size (task 2.0 review).
    Budget {
        id: "open/first_paint_slow_share_small",
        max_ms: 60.0,
        source: "ADR-0009",
    },
    Budget {
        id: "open/first_paint_slow_share",
        max_ms: 60.0,
        source: "ADR-0009",
    },
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
    // The same with diagnostics collected in the pass (DESIGN §3.3), as the
    // app indexes.
    Budget {
        id: "index/build_diagnostics",
        max_ms: 500.0,
        source: "DESIGN §1",
    },
    Budget {
        id: "index/run_diagnostics",
        max_ms: 500.0,
        source: "DESIGN §1",
    },
    // "Reads for visible cells are synchronous and must take under 1 ms":
    // one screenful (60 rows × 12 columns) parsed and displayed, uncached.
    Budget {
        id: "rows/screen",
        max_ms: 1.0,
        source: "DESIGN §3.9",
    },
    // The same with a 1 MB field on screen, read as the grid reads cells
    // (`display_prefix`): plain UTF-8, with `""` escapes, and UTF-16.
    Budget {
        id: "rows/screen_long_field_utf8",
        max_ms: 1.0,
        source: "DESIGN §3.9",
    },
    Budget {
        id: "rows/screen_long_field_escaped",
        max_ms: 1.0,
        source: "DESIGN §3.9",
    },
    Budget {
        id: "rows/screen_long_field_utf16",
        max_ms: 1.0,
        source: "DESIGN §3.9",
    },
    // A screenful as the grid reads it while a search for the find bar
    // runs on the background pool (PLAN 1.8): find never slows the grid.
    Budget {
        id: "find/screen_during_find",
        max_ms: 1.0,
        source: "DESIGN §3.9",
    },
    // "Save after one edit: < 500 ms": the reference file, one cell edited,
    // saved over itself (PLAN 2.2).
    Budget {
        id: "save/one_edit",
        max_ms: 500.0,
        source: "DESIGN §1",
    },
    // Save As UTF-8 of the reference file written as UTF-16 (PLAN 2.3):
    // DESIGN §1 has no budget for it. Save's 500 ms, doubled for a file
    // twice the size, every byte of it converted (docs/tasks/2.3.md).
    Budget {
        id: "save/utf8_from_utf16",
        max_ms: 1000.0,
        source: "task 2.3",
    },
];
