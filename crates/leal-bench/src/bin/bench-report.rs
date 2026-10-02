//! Reports criterion's results: budgets, regressions and noise.
//! `just bench-compare` runs it, and so does CI.
//!
//! ```text
//! bench-report [--regression FRACTION] [--noise FRACTION] [--last-attempt]
//!              [--first-attempt FIRST_DIR] CRITERION_DIR
//! ```
//!
//! Prints a Markdown table, then exits with status 0 if the report passed,
//! 1 if it failed, 2 on an error, or 3 to ask for a rerun. A budget fails
//! on any attempt. A regression (a change whose 95% interval is wholly
//! above `--regression`, default 0.20) fails only if every attempt shows
//! it. So on a first attempt, a regression asks for a rerun, as a noisy
//! run does (a canary moved by more than `--noise`, default 0.10).
//!
//! `--first-attempt FIRST_DIR` gives the first attempt's results to a
//! rerun, which is the last attempt (it implies `--last-attempt`). A
//! regression fails only if the first attempt showed it too. One that only
//! the rerun shows, or a noisy rerun, is inconclusive. On the last attempt,
//! inconclusive is a warning and exits with 0.
//!
//! If a criterion directory has a `bench-compare-order` file
//! (`report::ORDER_FILE`), the report says which order that attempt ran
//! the sides in. `just bench-compare` runs attempt 1 base first and the
//! rerun head first, so a drift over the job can't fail both; if both
//! attempts ran in the same order, it warns. On GitHub Actions it also
//! writes the table to the job summary, headed with the outcome, and each
//! problem as an annotation.

use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use leal_bench::budgets::BUDGETS;
use leal_bench::report::{self, Order, Outcome, Report, Status, Thresholds};

const USAGE: &str = "usage: bench-report [--regression FRACTION] [--noise FRACTION] \
                     [--last-attempt] [--first-attempt FIRST_DIR] CRITERION_DIR";

fn main() -> ExitCode {
    match run() {
        Ok(outcome) => ExitCode::from(outcome.exit_code()),
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::from(2)
        }
    }
}

fn read_report(dir: &Path, thresholds: Thresholds) -> Result<Report, String> {
    let measurements =
        report::collect(dir).map_err(|e| format!("reading {}: {e}", dir.display()))?;
    let mut report = report::evaluate(&measurements, BUDGETS, thresholds);
    report.order = report::read_order(dir).map_err(|e| format!("reading the order: {e}"))?;
    Ok(report)
}

/// " (base first)", or nothing if the order isn't known.
fn ran(order: Option<Order>) -> String {
    order.map_or_else(String::new, |order| format!(" ({})", order.short()))
}

fn run() -> Result<Outcome, String> {
    let mut thresholds = Thresholds {
        regression: 0.20,
        noise: 0.10,
    };
    let mut last_attempt = false;
    let mut first_dir = None;
    let mut dir = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let target = match arg.as_str() {
            "--regression" => &mut thresholds.regression,
            "--noise" => &mut thresholds.noise,
            "--last-attempt" => {
                last_attempt = true;
                continue;
            }
            "--first-attempt" => {
                let value = args.next().ok_or(format!("{arg} needs a value"))?;
                first_dir = Some(PathBuf::from(value));
                continue;
            }
            _ if dir.is_none() && !arg.starts_with('-') => {
                dir = Some(PathBuf::from(arg));
                continue;
            }
            _ => return Err(format!("unexpected argument `{arg}`")),
        };
        let value = args.next().ok_or(format!("{arg} needs a value"))?;
        *target = value.parse().map_err(|e| format!("{arg} `{value}`: {e}"))?;
    }
    let dir = dir.ok_or(USAGE)?;

    let mut report = read_report(&dir, thresholds)?;
    if let Some(first_dir) = &first_dir {
        // The rerun is the last attempt.
        last_attempt = true;
        let first = read_report(first_dir, thresholds)?;
        report = report.after(&first);
    }
    let outcome = report.outcome(last_attempt);
    let unconfirmed = report
        .rows
        .iter()
        .any(|row| row.status == Status::UnconfirmedRegression);
    let mut markdown = report.markdown();
    if outcome == Outcome::Inconclusive {
        let why = match (report.noisy, unconfirmed) {
            (true, true) => {
                "a canary moved on the last attempt, and a benchmark regressed on it \
                 but not on attempt 1"
            }
            (true, false) => "a canary moved on the last attempt too",
            (false, _) => "a benchmark regressed on the last attempt but not on attempt 1",
        };
        markdown.push_str(&format!(
            "\n**Inconclusive:** {why}, so regressions weren't judged. A regression fails \
             only if every attempt shows it. Budgets were checked and passed. This doesn't \
             fail the job.\n"
        ));
    }
    if outcome == Outcome::Rerun {
        markdown.push_str(
            "\n**Rerunning:** this attempt was noisy or showed a regression. A regression \
             fails only if the rerun shows it too.\n",
        );
    }
    println!("{markdown}");

    let github = std::env::var_os("GITHUB_ACTIONS").is_some();
    if let Some(summary) = std::env::var_os("GITHUB_STEP_SUMMARY") {
        let mut file = OpenOptions::new()
            .append(true)
            .create(true)
            .open(&summary)
            .map_err(|e| format!("opening the job summary: {e}"))?;
        writeln!(file, "## Benchmarks: {}\n\n{markdown}", outcome.label())
            .map_err(|e| format!("writing the job summary: {e}"))?;
    }
    let annotate = |level: &str, message: &str| {
        if github {
            println!("::{level}::{message}");
        } else {
            println!("{level}: {message}");
        }
    };
    let unconfirmed_on_rerun = format!(
        "regressed on the rerun{} but not on attempt 1{}, so it wasn't judged",
        ran(report.order),
        ran(report.first_order)
    );
    for row in report.problems() {
        let (level, what) = match row.status {
            Status::Regression if outcome == Outcome::Rerun => {
                ("warning", "regressed on this attempt; rerunning to confirm")
            }
            Status::Regression => ("error", "is a regression"),
            Status::NoisyRegression => ("warning", "may be a regression (noisy run)"),
            Status::UnconfirmedRegression => ("warning", unconfirmed_on_rerun.as_str()),
            Status::NoisyCanary => ("warning", "moved, so the run was noisy"),
            Status::OverBudget => ("error", "is over budget"),
            Status::Missing => ("error", "has a budget but no result"),
            Status::Ok | Status::Canary | Status::Info => continue,
        };
        annotate(level, &format!("benchmark {} {what}", row.id));
    }
    for id in &report.unconfirmed {
        annotate(
            "warning",
            &format!(
                "benchmark {id} regressed on attempt 1{} but not on the rerun{}, so it passed",
                ran(report.first_order),
                ran(report.order)
            ),
        );
    }
    if report.same_order() {
        annotate(
            "warning",
            "bench-compare ran both attempts in the same order, so a drift over the job \
             counted against the same side twice; it should run the rerun head first \
             (docs/tasks/1.2b.md)",
        );
    }
    if outcome == Outcome::Inconclusive {
        annotate(
            "warning",
            "benchmarks inconclusive: a regression fails only if every attempt shows it, and \
             the last attempt was noisy or showed one the first didn't (budgets passed); see \
             docs/tasks/1.2b.md",
        );
    }
    Ok(outcome)
}
