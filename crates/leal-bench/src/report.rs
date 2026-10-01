//! Reads criterion's results and decides what CI reports and fails on.
//!
//! Criterion writes each benchmark's results to
//! `<criterion dir>/<benchmark>/new/` (`benchmark.json` for its id and
//! throughput, `estimates.json` for its statistics). When it compares with
//! a saved baseline (`--baseline-lenient base`), it also writes
//! `<benchmark>/change/estimates.json`, the relative change. This module
//! reads those files; `bench-report` prints the result.
//!
//! Two checks fail the report:
//!
//! - **Budgets** ([`crate::budgets`]): the median is over the budget, or the
//!   budgeted benchmark has no result.
//! - **Regressions**: the median is slower than the baseline's by more than
//!   the threshold. The `baseline/` benchmarks are the noise canaries: their
//!   code is the same on both sides of a comparison, so when one of them
//!   moves by more than the threshold, the machine was too noisy to judge
//!   and regressions are reported as warnings instead.

use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::Path;

use serde_json::Value;

use crate::budgets::Budget;

/// Benchmarks with this id prefix are the noise canaries.
pub const CANARY_PREFIX: &str = "baseline/";

/// One benchmark's result.
#[derive(Debug, Clone, PartialEq)]
pub struct Measurement {
    /// Criterion's id, `group/name`.
    pub id: String,
    /// The median time per iteration, in nanoseconds.
    pub median_ns: f64,
    /// Bytes processed per iteration, if the benchmark declares it.
    pub throughput_bytes: Option<u64>,
    /// The median's relative change from the baseline (`0.1` is 10% slower),
    /// if there was one to compare with.
    pub change: Option<f64>,
}

/// What the report says about one benchmark.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// Within its budget and not a regression.
    Ok,
    /// A noise canary (`baseline/`): reported, never failed.
    Canary,
    /// Slower than the baseline by more than the threshold. Fails.
    Regression,
    /// As `Regression`, but on a run whose canaries moved too: a warning.
    NoisyRegression,
    /// The median is over the benchmark's budget. Fails.
    OverBudget,
    /// A budgeted benchmark that has no result. Fails.
    Missing,
}

impl Status {
    fn fails(&self) -> bool {
        matches!(self, Self::Regression | Self::OverBudget | Self::Missing)
    }

    fn label(&self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Canary => "canary",
            Self::Regression => "**regression**",
            Self::NoisyRegression => "regression? (noisy run)",
            Self::OverBudget => "**over budget**",
            Self::Missing => "**missing**",
        }
    }
}

/// One row of the report.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    /// The benchmark id.
    pub id: String,
    /// Its result, if it ran.
    pub measurement: Option<Measurement>,
    /// Its budget, if it has one.
    pub budget: Option<Budget>,
    /// The verdict.
    pub status: Status,
}

/// The whole report.
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    /// One row per benchmark (and per budget with no result), sorted by id.
    pub rows: Vec<Row>,
    /// The regression threshold used, as a fraction (`0.25` is 25%).
    pub threshold: f64,
    /// True if a noise canary moved by more than the threshold.
    pub noisy: bool,
}

/// Reads every benchmark result under `dir`, sorted by id.
///
/// # Errors
///
/// `dir` can't be read, or a result file is missing or malformed.
pub fn collect(dir: &Path) -> io::Result<Vec<Measurement>> {
    let mut found = Vec::new();
    visit(dir, &mut found)?;
    found.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(found)
}

/// Collects the benchmark in `dir`, if it is one, and searches its
/// subdirectories. A benchmark directory is one with `new/benchmark.json`.
fn visit(dir: &Path, found: &mut Vec<Measurement>) -> io::Result<()> {
    let new = dir.join("new");
    if new.join("benchmark.json").is_file() {
        found.push(read_measurement(dir)?);
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            visit(&entry.path(), found)?;
        }
    }
    Ok(())
}

fn read_measurement(dir: &Path) -> io::Result<Measurement> {
    let benchmark = read_json(&dir.join("new").join("benchmark.json"))?;
    let estimates = read_json(&dir.join("new").join("estimates.json"))?;
    let change_file = dir.join("change").join("estimates.json");
    let change = if change_file.is_file() {
        Some(median(&read_json(&change_file)?, &change_file)?)
    } else {
        None
    };
    let id = benchmark["full_id"]
        .as_str()
        .ok_or_else(|| malformed(dir, "no `full_id` in benchmark.json"))?;
    Ok(Measurement {
        id: id.to_owned(),
        median_ns: median(&estimates, dir)?,
        throughput_bytes: benchmark["throughput"]["Bytes"].as_u64(),
        change,
    })
}

fn median(estimates: &Value, path: &Path) -> io::Result<f64> {
    estimates["median"]["point_estimate"]
        .as_f64()
        .ok_or_else(|| malformed(path, "no median in estimates.json"))
}

fn read_json(path: &Path) -> io::Result<Value> {
    let text = fs::read_to_string(path)?;
    serde_json::from_str(&text).map_err(|error| malformed(path, &error.to_string()))
}

fn malformed(path: &Path, why: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("{}: {why}", path.display()),
    )
}

/// Checks `measurements` against `budgets` and against their baselines,
/// failing a slowdown of more than `threshold` (a fraction).
#[must_use]
pub fn evaluate(measurements: &[Measurement], budgets: &[Budget], threshold: f64) -> Report {
    let noisy = measurements.iter().any(|m| {
        m.id.starts_with(CANARY_PREFIX) && m.change.is_some_and(|change| change.abs() > threshold)
    });

    let mut rows: Vec<Row> = measurements
        .iter()
        .map(|m| {
            let budget = budgets.iter().find(|b| b.id == m.id).copied();
            let status = if budget.is_some_and(|b| m.median_ns > b.max_ms * 1e6) {
                Status::OverBudget
            } else if m.id.starts_with(CANARY_PREFIX) {
                Status::Canary
            } else if m.change.is_some_and(|change| change > threshold) {
                if noisy {
                    Status::NoisyRegression
                } else {
                    Status::Regression
                }
            } else {
                Status::Ok
            };
            Row {
                id: m.id.clone(),
                measurement: Some(m.clone()),
                budget,
                status,
            }
        })
        .collect();

    for budget in budgets {
        if !measurements.iter().any(|m| m.id == budget.id) {
            rows.push(Row {
                id: budget.id.to_owned(),
                measurement: None,
                budget: Some(*budget),
                status: Status::Missing,
            });
        }
    }
    rows.sort_by(|a, b| a.id.cmp(&b.id));

    Report {
        rows,
        threshold,
        noisy,
    }
}

impl Report {
    /// True if any benchmark is over budget, missing, or a regression on a
    /// quiet run.
    #[must_use]
    pub fn failed(&self) -> bool {
        self.rows.iter().any(|row| row.status.fails())
    }

    /// The rows that fail the report or warn about it, for annotations.
    pub fn problems(&self) -> impl Iterator<Item = &Row> {
        self.rows
            .iter()
            .filter(|row| row.status.fails() || row.status == Status::NoisyRegression)
    }

    /// The report as a Markdown table, for the terminal and for GitHub's job
    /// summary.
    #[must_use]
    pub fn markdown(&self) -> String {
        let mut out = String::new();
        out.push_str("| Benchmark | Median | Throughput | Change | Budget | Status |\n");
        out.push_str("|---|---|---|---|---|---|\n");
        for row in &self.rows {
            let (median, throughput, change) = match &row.measurement {
                Some(m) => (
                    duration(m.median_ns),
                    m.throughput_bytes
                        .map(|bytes| gib_per_s(bytes, m.median_ns))
                        .unwrap_or_default(),
                    m.change
                        .map_or_else(|| "—".to_owned(), |c| format!("{:+.1}%", c * 100.0)),
                ),
                None => ("—".to_owned(), String::new(), "—".to_owned()),
            };
            let budget = row
                .budget
                .map(|b| format!("{} ({})", duration(b.max_ms * 1e6), b.source))
                .unwrap_or_default();
            let _ = writeln!(
                out,
                "| `{}` | {median} | {throughput} | {change} | {budget} | {} |",
                row.id,
                row.status.label()
            );
        }
        let _ = write!(
            out,
            "\nChange is the median's change from the baseline; a slowdown of more than {:.0}% is a regression.",
            self.threshold * 100.0
        );
        if self.noisy {
            out.push_str(
                " A `baseline/` canary moved by more than that, so this run was too noisy to judge regressions; rerun it.",
            );
        }
        out.push('\n');
        out
    }
}

/// A duration in nanoseconds, to three significant figures.
fn duration(ns: f64) -> String {
    let (value, unit) = if ns >= 1e9 {
        (ns / 1e9, "s")
    } else if ns >= 1e6 {
        (ns / 1e6, "ms")
    } else if ns >= 1e3 {
        (ns / 1e3, "µs")
    } else {
        (ns, "ns")
    };
    let decimals = if value >= 100.0 {
        0
    } else if value >= 10.0 {
        1
    } else {
        2
    };
    format!("{value:.decimals$} {unit}")
}

/// Throughput in GiB/s, as criterion prints it.
fn gib_per_s(bytes: u64, ns: f64) -> String {
    // Precision loss above 2^53 bytes doesn't matter here.
    #[allow(clippy::cast_precision_loss)]
    let bytes = bytes as f64;
    format!("{:.2} GiB/s", bytes / (ns / 1e9) / f64::from(1u32 << 30))
}
