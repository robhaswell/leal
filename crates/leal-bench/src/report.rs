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
//!   above the threshold means a single noisy median doesn't fail. A noisy
//!   run never excuses a budget failure.
//! - **Noisy** if a noise canary moved, in either direction, by more than
//!   [`Thresholds::noise`] (and nothing failed). The canaries are the
//!   `memchr3_scan` baselines ([`CANARIES`]), which run first, and again as
//!   `baseline-late` after all the others. Their code is the same on both
//!   sides of a comparison, so when one moves, the machine moved, and the
//!   run can't judge regressions.
//! - **Pass** otherwise.
//!
//! [`Report::outcome`] turns the verdict into what `just bench-compare`
//! does ([`Outcome`]): a first attempt that is noisy or shows a regression
//! is rerun, and a noisy last attempt is **inconclusive**, which warns but
//! passes. Failing the job because the runner was noisy would turn CI red
//! for no reason.
//!
//! **A regression fails only if every attempt shows it.** The rerun's
//! report is judged with the first attempt's ([`Report::after`]). A
//! benchmark whose whole interval is above the threshold on both fails,
//! whether either attempt was noisy or not. One that regressed only on the
//! rerun is inconclusive. One that regressed only on the first attempt
//! passes if the rerun was quiet, and is inconclusive if it was noisy.
//! Budgets fail on any attempt. On main at `ed09773`, the first attempt
//! showed no regression. The rerun showed 13, in code that was identical
//! on both sides, because noise began part-way through it
//! (`docs/tasks/1.2b.md`).
//!
//! `sequential_read`, the other baseline benchmark, is reported for
//! information only ([`Status::Info`]): it is no canary, has no budget and
//! can't regress. It takes about 7 ms on CI and depends on the page cache
//! and the VM's I/O, so on a shared GitHub runner it moved by 10–16%
//! between runs of identical code while `memchr3_scan` (CPU-bound, about
//! 47 ms) stayed within 6% (`docs/tasks/1.2b.md`).

use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::Path;

use serde_json::Value;

use crate::budgets::Budget;

/// The criterion groups of the speed-of-light baseline: `baseline` runs
/// with the other benchmarks, and `bench-compare` runs it again as
/// `baseline-late` after them (`benches/baseline.rs`).
pub const BASELINE_GROUPS: [&str; 2] = ["baseline", "baseline-late"];

/// The noise canaries: the CPU-bound scan, in both baseline groups. The
/// other baseline benchmark, `sequential_read`, is too short and too
/// sensitive to I/O to be one (see the module docs).
pub const CANARIES: [&str; 2] = ["baseline/memchr3_scan", "baseline-late/memchr3_scan"];

/// True if `id` (`group/name`) is a noise canary.
#[must_use]
pub fn is_canary(id: &str) -> bool {
    CANARIES.contains(&id)
}

/// Benchmarks gated on their budget only, never on change between commits,
/// because shared runners make them too noisy to compare: the simulated
/// removable open reads through `pread` under background load, and its
/// 95% interval spans tens of percent between identical commits.
pub const BUDGET_ONLY: [&str; 1] = ["open/first_paint_removable_under_load"];

/// True if `id` is gated on its budget only (see [`BUDGET_ONLY`]).
#[must_use]
pub fn is_budget_only(id: &str) -> bool {
    BUDGET_ONLY.contains(&id)
}

/// True if `id` is in one of the [`BASELINE_GROUPS`]. The baseline's code
/// is the same on both sides of every comparison, so its benchmarks never
/// gate: each is either a canary or information.
#[must_use]
pub fn is_baseline(id: &str) -> bool {
    id.split_once('/')
        .is_some_and(|(group, _)| BASELINE_GROUPS.contains(&group))
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
    /// A baseline benchmark that isn't a canary (`sequential_read`):
    /// reported, never gated.
    Info,
    /// Slower than the baseline past the threshold, on a quiet run; on a
    /// rerun ([`Report::after`]), on the first attempt too, noisy or not.
    /// On a first attempt it is rerun to confirm; on the last, it fails.
    Regression,
    /// As `Regression`, but on a noisy run, so it can't be judged.
    NoisyRegression,
    /// A regression on the rerun that the first attempt didn't show, so it
    /// isn't judged ([`Report::after`]).
    UnconfirmedRegression,
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
            Self::Info => "info (not gated)",
            Self::Regression => "**regression**",
            Self::NoisyRegression => "regression? (noisy run)",
            Self::UnconfirmedRegression => "regression? (not on attempt 1)",
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
    /// A canary moved, so regressions can't be judged.
    Noisy,
}

/// What `just bench-compare` does with a report, given whether it may
/// still rerun ([`Report::outcome`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Nothing failed, and the run was quiet.
    Pass,
    /// A budget failed, or a benchmark regressed on every attempt.
    Fail,
    /// The run was noisy or a benchmark regressed, and this wasn't the last
    /// attempt: rerun.
    Rerun,
    /// Nothing failed, but the last attempt was noisy, or a benchmark
    /// regressed on it and not on the first: warn, but pass.
    Inconclusive,
}

impl Outcome {
    /// `bench-report`'s exit status: 0 to pass (inconclusive included), 1
    /// to fail, 3 to ask for a rerun.
    #[must_use]
    pub fn exit_code(self) -> u8 {
        match self {
            Self::Pass | Self::Inconclusive => 0,
            Self::Fail => 1,
            Self::Rerun => 3,
        }
    }

    /// The outcome in a few words, for the job summary's heading.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Rerun => "rerunning to confirm",
            Self::Inconclusive => "inconclusive",
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
    /// The thresholds used.
    pub thresholds: Thresholds,
    /// True if a noise canary moved by more than the noise threshold.
    pub noisy: bool,
    /// For a rerun ([`Report::after`]): the benchmarks that regressed on
    /// the first attempt but not on this one, sorted by id.
    pub unconfirmed: Vec<String>,
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
            } else if is_baseline(&m.id) || is_budget_only(&m.id) {
                Status::Info
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
        unconfirmed: Vec::new(),
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

    /// True if `id`'s whole interval was above the regression threshold
    /// in this report, on a quiet run or a noisy one.
    #[must_use]
    pub fn regressed(&self, id: &str) -> bool {
        self.rows.iter().any(|row| {
            row.id == id
                && matches!(
                    row.status,
                    Status::Regression | Status::NoisyRegression | Status::UnconfirmedRegression
                )
        })
    }

    /// This report, a rerun's, judged with the `first` attempt's. A
    /// regression here counts ([`Status::Regression`], noisy or not) only
    /// if `first` showed it too; otherwise it is
    /// [`Status::UnconfirmedRegression`]. The first attempt's regressions
    /// that this one doesn't show are listed in [`Report::unconfirmed`].
    #[must_use]
    pub fn after(mut self, first: &Report) -> Report {
        for row in &mut self.rows {
            if matches!(row.status, Status::Regression | Status::NoisyRegression) {
                row.status = if first.regressed(&row.id) {
                    Status::Regression
                } else {
                    Status::UnconfirmedRegression
                };
            }
        }
        self.unconfirmed = first
            .rows
            .iter()
            .filter(|row| first.regressed(&row.id) && !self.regressed(&row.id))
            .map(|row| row.id.clone())
            .collect();
        self
    }

    /// What to do with this report.
    ///
    /// - A budget failure fails, on any attempt, noisy or not.
    /// - Otherwise, before the `last_attempt`, a noisy run or a regression
    ///   (noisy or not) is rerun, so that a regression fails only if every
    ///   attempt shows it.
    /// - On the last attempt, a [`Status::Regression`] fails. On a rerun
    ///   ([`Report::after`]), that means the first attempt showed it too.
    ///   A noisy run, or a regression on this attempt only, is
    ///   inconclusive and passes: a noisy runner is no reason to fail the
    ///   job.
    #[must_use]
    pub fn outcome(&self, last_attempt: bool) -> Outcome {
        let any = |wanted: &[Status]| self.rows.iter().any(|row| wanted.contains(&row.status));
        if any(&[Status::OverBudget, Status::Missing]) {
            Outcome::Fail
        } else if !last_attempt {
            if self.noisy || any(&[Status::Regression, Status::NoisyRegression]) {
                Outcome::Rerun
            } else {
                Outcome::Pass
            }
        } else if any(&[Status::Regression]) {
            Outcome::Fail
        } else if self.noisy || any(&[Status::NoisyRegression, Status::UnconfirmedRegression]) {
            Outcome::Inconclusive
        } else {
            Outcome::Pass
        }
    }

    /// The rows worth an annotation: failures, and what made a run noisy.
    pub fn problems(&self) -> impl Iterator<Item = &Row> {
        self.rows
            .iter()
            .filter(|row| !matches!(row.status, Status::Ok | Status::Canary | Status::Info))
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
            "\nA benchmark regressed if its change's whole 95% interval is above {}, \
             and fails only if it regressed on every attempt. The run is noisy if a canary (`{}` or `{}`) moved by more than {}. \
             The other `baseline` benchmarks are information only.",
            percent(self.thresholds.regression),
            CANARIES[0],
            CANARIES[1],
            percent(self.thresholds.noise),
        );
        if self.noisy {
            out.push_str(" **This run was noisy**, so regressions in it can't be judged.");
        }
        if !self.unconfirmed.is_empty() {
            let ids: Vec<String> = self
                .unconfirmed
                .iter()
                .map(|id| format!("`{id}`"))
                .collect();
            let _ = write!(
                out,
                " Regressed on attempt 1 but not on this attempt, so not failed: {}.",
                ids.join(", ")
            );
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
