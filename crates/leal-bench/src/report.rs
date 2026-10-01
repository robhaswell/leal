//! Reads criterion's results and decides what CI reports and fails on.
//!
//! Criterion writes each benchmark's results to
//! `<criterion dir>/<benchmark>/new/` (`benchmark.json` for its id and
//! throughput, `estimates.json` for its statistics). When it compares with
//! a saved baseline (`--baseline-lenient base`), it also writes
//! `<benchmark>/change/estimates.json`: the relative change, with a 95%
//! confidence interval. This module reads those files; `bench-report`
//! prints the result.
//!
//! The report has one of three verdicts ([`Verdict`]):
//!
//! - **Fail** if a benchmark is over its budget ([`crate::budgets`]), a
//!   budgeted benchmark has no result, or, on a quiet run, a benchmark
//!   regressed: the lower end of the 95% interval for its median's change
//!   is above [`Thresholds::regression`]. Requiring the whole interval to be
//!   above the threshold means a single noisy median doesn't fail.
//! - **Noisy** if a noise canary moved, in either direction, by more than
//!   [`Thresholds::noise`] (and nothing is over budget). The canaries are
//!   the `baseline` benchmarks, which run first, and again as
//!   `baseline-late` after all the others. Their code is the same on both
//!   sides of a comparison, so when one moves, the machine moved, and the
//!   run can't judge regressions. `just bench-compare` reruns once, then
//!   fails.
//! - **Pass** otherwise.

use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::Path;

use serde_json::Value;

use crate::budgets::Budget;

/// The criterion groups that are noise canaries: `baseline` runs with the
/// other benchmarks, and `bench-compare` runs it again as `baseline-late`
/// after them (`benches/baseline.rs`).
pub const CANARY_GROUPS: [&str; 2] = ["baseline", "baseline-late"];

/// True if `id` (`group/name`) is a noise canary.
#[must_use]
pub fn is_canary(id: &str) -> bool {
    id.split_once('/')
        .is_some_and(|(group, _)| CANARY_GROUPS.contains(&group))
}

/// One benchmark's result.
#[derive(Debug, Clone, PartialEq)]
pub struct Measurement {
    /// Criterion's id, `group/name`.
    pub id: String,
    /// The median time per iteration, in nanoseconds.
    pub median_ns: f64,
    /// Bytes processed per iteration, if the benchmark declares it.
    pub throughput_bytes: Option<u64>,
    /// The change from the baseline, if there was one to compare with.
    pub change: Option<Change>,
}

/// The relative change of a benchmark's median from the baseline, as
/// fractions: `0.1` is 10% slower, `-0.1` 10% faster.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Change {
    /// Criterion's point estimate.
    pub median: f64,
    /// The lower end of criterion's 95% confidence interval.
    pub lower: f64,
    /// The upper end of criterion's 95% confidence interval.
    pub upper: f64,
}

/// The limits the report checks changes against, as fractions.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Thresholds {
    /// A benchmark regressed if its change's lower bound is above this.
    pub regression: f64,
    /// The run is noisy if a canary's median moved by more than this.
    pub noise: f64,
}

/// What the report says about one benchmark.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// Within its budget and not a regression.
    Ok,
    /// A noise canary that held still.
    Canary,
    /// A noise canary that moved by more than the noise threshold.
    NoisyCanary,
    /// Slower than the baseline past the threshold, on a quiet run. Fails.
    Regression,
    /// As `Regression`, but on a noisy run, so it can't be judged.
    NoisyRegression,
    /// The median is over the benchmark's budget. Fails.
    OverBudget,
    /// A budgeted benchmark that has no result. Fails.
    Missing,
}

impl Status {
    fn label(&self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Canary => "canary",
            Self::NoisyCanary => "**canary moved (noisy run)**",
            Self::Regression => "**regression**",
            Self::NoisyRegression => "regression? (noisy run)",
            Self::OverBudget => "**over budget**",
            Self::Missing => "**missing**",
        }
    }
}

/// The report's overall result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Nothing failed, and the run was quiet enough to judge.
    Pass,
    /// A budget failed, or a regression on a quiet run.
    Fail,
    /// A canary moved, so regressions can't be judged: rerun.
    Noisy,
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
    /// The thresholds used.
    pub thresholds: Thresholds,
    /// True if a noise canary moved by more than the noise threshold.
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
    let benchmark_file = dir.join("new").join("benchmark.json");
    let estimates_file = dir.join("new").join("estimates.json");
    let change_file = dir.join("change").join("estimates.json");

    let benchmark = read_json(&benchmark_file)?;
    let estimates = read_json(&estimates_file)?;
    let change = if change_file.is_file() {
        let median = &read_json(&change_file)?["median"];
        let number = |value: &Value| {
            value
                .as_f64()
                .ok_or_else(|| malformed(&change_file, "no median estimate"))
        };
        Some(Change {
            median: number(&median["point_estimate"])?,
            lower: number(&median["confidence_interval"]["lower_bound"])?,
            upper: number(&median["confidence_interval"]["upper_bound"])?,
        })
    } else {
        None
    };
    let id = benchmark["full_id"]
        .as_str()
        .ok_or_else(|| malformed(&benchmark_file, "no `full_id`"))?;
    let median_ns = estimates["median"]["point_estimate"]
        .as_f64()
        .ok_or_else(|| malformed(&estimates_file, "no median estimate"))?;
    Ok(Measurement {
        id: id.to_owned(),
        median_ns,
        throughput_bytes: benchmark["throughput"]["Bytes"].as_u64(),
        change,
    })
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

/// Checks `measurements` against `budgets`, and their changes from the
/// baseline against `thresholds`.
#[must_use]
pub fn evaluate(
    measurements: &[Measurement],
    budgets: &[Budget],
    thresholds: Thresholds,
) -> Report {
    let canary_moved = |m: &Measurement| {
        is_canary(&m.id)
            && m.change
                .is_some_and(|change| change.median.abs() > thresholds.noise)
    };
    let noisy = measurements.iter().any(canary_moved);

    let mut rows: Vec<Row> = measurements
        .iter()
        .map(|m| {
            let budget = budgets.iter().find(|b| b.id == m.id).copied();
            let status = if budget.is_some_and(|b| m.median_ns > b.max_ms * 1e6) {
                Status::OverBudget
            } else if canary_moved(m) {
                Status::NoisyCanary
            } else if is_canary(&m.id) {
                Status::Canary
            } else if m
                .change
                .is_some_and(|change| change.lower > thresholds.regression)
            {
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
        thresholds,
        noisy,
    }
}

impl Report {
    /// Fail if a budget failed or (on a quiet run) a benchmark regressed;
    /// otherwise noisy if a canary moved; otherwise pass. A budget failure
    /// is a failure even on a noisy run: budgets are absolute.
    #[must_use]
    pub fn verdict(&self) -> Verdict {
        let any = |wanted: &[Status]| self.rows.iter().any(|row| wanted.contains(&row.status));
        if any(&[Status::OverBudget, Status::Missing, Status::Regression]) {
            Verdict::Fail
        } else if self.noisy {
            Verdict::Noisy
        } else {
            Verdict::Pass
        }
    }

    /// The rows worth an annotation: failures, and what made a run noisy.
    pub fn problems(&self) -> impl Iterator<Item = &Row> {
        self.rows
            .iter()
            .filter(|row| !matches!(row.status, Status::Ok | Status::Canary))
    }

    /// The report as a Markdown table, for the terminal and for GitHub's job
    /// summary.
    #[must_use]
    pub fn markdown(&self) -> String {
        let mut out = String::new();
        out.push_str(
            "| Benchmark | Median | Throughput | Change (95% interval) | Budget | Status |\n",
        );
        out.push_str("|---|---|---|---|---|---|\n");
        for row in &self.rows {
            let (median, throughput, change) = match &row.measurement {
                Some(m) => (
                    duration(m.median_ns),
                    m.throughput_bytes
                        .map(|bytes| gib_per_s(bytes, m.median_ns))
                        .unwrap_or_default(),
                    m.change.map_or_else(
                        || "—".to_owned(),
                        |c| {
                            format!(
                                "{} ({} to {})",
                                percent(c.median),
                                percent(c.lower),
                                percent(c.upper)
                            )
                        },
                    ),
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
            "\nA benchmark regressed if its change's whole 95% interval is above {}. \
             The run is noisy if a `baseline` or `baseline-late` canary moved by more than {}.",
            percent(self.thresholds.regression),
            percent(self.thresholds.noise),
        );
        if self.noisy {
            out.push_str(" **This run was noisy**, so regressions in it can't be judged.");
        }
        out.push('\n');
        out
    }
}

/// A fraction as a signed percentage: `+1.2%`.
fn percent(fraction: f64) -> String {
    format!("{:+.1}%", fraction * 100.0)
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
