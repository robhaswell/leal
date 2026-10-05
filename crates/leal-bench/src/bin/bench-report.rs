//! Reports criterion's results: budgets, regressions and noise.
//! `just bench-compare` runs it, and so does CI.
//!
//! ```text
//! bench-report [--regression FRACTION] [--noise FRACTION] [--last-attempt]
//!              [--first-attempt FIRST_DIR [--plan PLAN_FILE | --second-attempt SECOND_DIR]]
//!              CRITERION_DIR
//! ```
//!
//! Prints a Markdown table, then exits with status 0 if the report passed,
//! 1 if it failed, 2 on an error, or 3 to ask for another attempt. A budget
//! fails on any attempt, but an attempt that would be rerun still asks for
//! the rerun, so regressions are judged before the job fails (task 2.G-b).
//! A report-only budget only warns. A regression (a change whose 95% interval is
//! wholly above `--regression`, default 0.20) fails only if every attempt
//! shows it. A canary moved if it changed by more than `--noise` (default
//! 0.10): a run-wide one makes the run noisy, and a group's makes the
//! attempt noisy for that group's benchmarks (`report`'s module docs).
//!
//! - **Attempt 1** (`CRITERION_DIR` alone): a regression or a noisy run
//!   asks for a rerun.
//! - **The rerun** (`--first-attempt FIRST_DIR`): a regression fails if the
//!   first attempt showed it too and both were quiet for it. If either was
//!   noisy for it, `--plan PLAN_FILE` asks for a third attempt, writing
//!   what it should run to `PLAN_FILE` (`report::Plan::to_text`). Without
//!   `--plan`, the rerun is the last attempt, and that regression fails.
//! - **Attempt 3** (`--first-attempt FIRST_DIR --second-attempt
//!   SECOND_DIR`): `CRITERION_DIR` holds only the groups to recheck. Each
//!   fails if attempt 3 shows its regression too, or has no result for it.
//!   Otherwise it passes with a warning, or is inconclusive if attempt 3's
//!   nearest canaries say its base side slowed, which can hide a
//!   regression.
//!
//! Inconclusive is a warning, and exits with 0: the rerun's run was noisy
//! (a run-wide canary moved), a regression showed on the rerun but not on
//! attempt 1, or a regression didn't show again on a later attempt that
//! may have hidden it (`report::Status::Unsettled`). `--last-attempt`
//! makes attempt 1 the last (a base with no benchmarks, which never
//! reruns).
//!
//! If a criterion directory has a `bench-compare-order` file
//! (`report::ORDER_FILE`), the report says which order that attempt ran
//! the sides in. `just bench-compare` runs attempts 1 and 3 base first and
//! the rerun head first, so a drift over the job can't fail both; if the
//! first two attempts ran in the same order, it warns. On GitHub Actions
//! it also writes the table to the job summary, headed with the outcome,
//! and each problem as an annotation.

use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use leal_bench::budgets::{BUDGETS, Budget};
use leal_bench::report::{self, Order, Outcome, Report, Status, Thresholds};

const USAGE: &str = "usage: bench-report [--regression FRACTION] [--noise FRACTION] \
                     [--last-attempt] [--first-attempt FIRST_DIR [--plan PLAN_FILE | \
                     --second-attempt SECOND_DIR]] CRITERION_DIR";

fn main() -> ExitCode {
    match run() {
        Ok(outcome) => ExitCode::from(outcome.exit_code()),
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::from(2)
        }
    }
}

/// The report on `dir`. A third attempt (`only_measured`) runs only some
/// groups, so only the budgets of the benchmarks it measured are checked:
/// the rest were checked on the attempts before it.
fn read_report(dir: &Path, thresholds: Thresholds, only_measured: bool) -> Result<Report, String> {
    let measurements =
        report::collect(dir).map_err(|e| format!("reading {}: {e}", dir.display()))?;
    let budgets: Vec<Budget> = BUDGETS
        .iter()
        .filter(|b| !only_measured || measurements.iter().any(|m| m.id == b.id))
        .copied()
        .collect();
    let mut report = report::evaluate(&measurements, &budgets, thresholds);
    report.order = report::read_order(dir).map_err(|e| format!("reading the order: {e}"))?;
    Ok(report)
}

/// " (base first)", or nothing if the order isn't known.
fn ran(order: Option<Order>) -> String {
    order.map_or_else(String::new, |order| format!(" ({})", order.short()))
}

/// Why `report` is inconclusive, in a few clauses.
fn why_inconclusive(report: &Report) -> String {
    let has = |status: Status| report.rows.iter().any(|row| row.status == status);
    let mut why = Vec::new();
    if report.noisy {
        why.push(if report.attempt == 1 {
            "a run-wide canary moved on this attempt"
        } else {
            "a run-wide canary moved on the rerun too"
        });
    }
    if has(Status::NoisyRegression) {
        why.push("a benchmark regressed on an attempt that was noisy for it");
    }
    if has(Status::UnconfirmedRegression) {
        why.push("a benchmark regressed on the rerun but not on attempt 1");
    }
    if has(Status::Unsettled) {
        why.push(
            "a regression didn't show again on a later attempt whose nearest canaries say its \
             base side slowed, which can hide one",
        );
    }
    why.join("; ")
}

fn run() -> Result<Outcome, String> {
    let mut thresholds = Thresholds {
        regression: 0.20,
        noise: 0.10,
    };
    let mut last_attempt = false;
    let mut first_dir = None;
    let mut second_dir = None;
    let mut plan_file = None;
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
            "--first-attempt" | "--second-attempt" | "--plan" => {
                let value = PathBuf::from(args.next().ok_or(format!("{arg} needs a value"))?);
                match arg.as_str() {
                    "--first-attempt" => first_dir = Some(value),
                    "--second-attempt" => second_dir = Some(value),
                    _ => plan_file = Some(value),
                }
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
    if first_dir.is_none() && (second_dir.is_some() || plan_file.is_some()) {
        return Err(format!(
            "--second-attempt and --plan need --first-attempt\n{USAGE}"
        ));
    }
    if second_dir.is_some() && plan_file.is_some() {
        return Err(format!(
            "--plan is for the rerun, and --second-attempt for attempt 3: not both\n{USAGE}"
        ));
    }

    let mut report = read_report(&dir, thresholds, second_dir.is_some())?;
    if let Some(first_dir) = &first_dir {
        let first = read_report(first_dir, thresholds, false)?;
        if let Some(second_dir) = &second_dir {
            // Attempt 3, the last.
            last_attempt = true;
            let second = read_report(second_dir, thresholds, false)?.after(&first);
            report = second.settle(report);
        } else {
            // The rerun. It may ask for a third attempt only with --plan.
            last_attempt |= plan_file.is_none();
            report = report.after(&first);
        }
    }
    let outcome = report.outcome(last_attempt);
    let plan = match (outcome, report.attempt, &plan_file) {
        (Outcome::Rerun, 2, Some(file)) => {
            let plan = report
                .third_attempt_plan()
                .ok_or("a third attempt with nothing to recheck")?;
            std::fs::write(file, plan.to_text())
                .map_err(|e| format!("writing {}: {e}", file.display()))?;
            Some(plan)
        }
        _ => None,
    };

    let mut markdown = report.markdown();
    if outcome == Outcome::Inconclusive {
        markdown.push_str(&format!(
            "\n**Inconclusive:** {}, so not every regression could be judged. A regression \
             fails only if every attempt shows it. Budgets were checked and passed. This \
             doesn't fail the job.\n",
            why_inconclusive(&report)
        ));
    }
    let over_budget = report.over_budget();
    if outcome == Outcome::Rerun && !over_budget.is_empty() {
        let ids: Vec<String> = over_budget.iter().map(|id| format!("`{id}`")).collect();
        markdown.push_str(&format!(
            "\n**Over budget:** {}. The job will fail, but it runs the next attempt first, so \
             regressions are still judged.\n",
            ids.join(", ")
        ));
    }
    if outcome == Outcome::Rerun {
        match &plan {
            None => markdown.push_str(
                "\n**Rerunning:** this attempt was noisy or showed a regression. A regression \
                 fails only if the rerun shows it too.\n",
            ),
            Some(plan) => {
                let groups: Vec<String> = plan.groups.iter().map(|g| format!("`{g}`")).collect();
                markdown.push_str(&format!(
                    "\n**Third attempt:** a benchmark regressed on both attempts, but at least \
                     one of them was noisy for it, so its group runs again, base first ({}). \
                     It fails only if the third attempt shows the regression too.\n",
                    groups.join(", ")
                ));
            }
        }
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
    annotate_problems(&report, outcome, last_attempt, github);
    Ok(outcome)
}

/// Prints an annotation for each problem in `report`.
fn annotate_problems(report: &Report, outcome: Outcome, last_attempt: bool, github: bool) {
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
    let on_every_attempt = match report.attempt {
        1 => "is a regression".to_owned(),
        2 => format!(
            "is a regression: it regressed on attempt 1{} and on the rerun{}, both quiet for it",
            ran(report.first_order),
            ran(report.order)
        ),
        _ => "is a regression: it regressed on all three attempts".to_owned(),
    };
    let third_order = ran(report.third.as_ref().and_then(|third| third.order));
    for row in report.problems() {
        let rechecked = report.rechecked.contains(&row.id);
        let (level, what) = match row.status {
            Status::Regression if outcome == Outcome::Rerun && report.attempt == 1 => (
                "warning",
                "regressed on this attempt; rerunning to confirm".to_owned(),
            ),
            Status::Regression
                if rechecked
                    && report.third.as_ref().is_none_or(|third| {
                        third
                            .row(&row.id)
                            .and_then(|again| again.measurement.as_ref())
                            .is_none_or(|m| m.change.is_none())
                    }) =>
            {
                (
                    "error",
                    "regressed on attempts 1 and 2, and attempt 3 has no result for it, so \
                     nothing cleared it"
                        .to_owned(),
                )
            }
            Status::Regression if report.attempt == 3 && !rechecked => {
                ("error", "is a regression".to_owned())
            }
            Status::Regression => ("error", on_every_attempt.clone()),
            Status::NoisyRegression => (
                "warning",
                "may be a regression (this attempt was noisy for it)".to_owned(),
            ),
            Status::UnconfirmedRegression => ("warning", unconfirmed_on_rerun.clone()),
            Status::Recheck if outcome == Outcome::Rerun => (
                "warning",
                "regressed on both attempts, but at least one was noisy for it; rerunning its \
                 group a third time, base first"
                    .to_owned(),
            ),
            Status::Recheck if last_attempt => (
                "error",
                "is a regression: it regressed on both attempts (at least one noisy for it), \
                 and no third attempt was allowed"
                    .to_owned(),
            ),
            Status::Recheck => (
                "warning",
                "regressed on both attempts, but at least one was noisy for it; not rechecked, \
                 because the job fails anyway"
                    .to_owned(),
            ),
            Status::Cleared => (
                "warning",
                format!(
                    "regressed on attempts 1 and 2 (at least one noisy for it) but not on \
                     attempt 3{third_order}, with no sign that its base side slowed, so it \
                     passed"
                ),
            ),
            Status::Unsettled => {
                // The same case after the rerun as after attempt 3, in the
                // same words.
                let (earlier, later) = if rechecked {
                    (
                        "attempts 1 and 2".to_owned(),
                        format!("attempt 3{third_order}"),
                    )
                } else {
                    (
                        format!("attempt 1{}", ran(report.first_order)),
                        format!("the rerun{}", ran(report.order)),
                    )
                };
                (
                    "warning",
                    format!(
                        "regressed on {earlier} but not on {later}, whose nearest canaries say \
                         its base side slowed, which can hide a regression, so it wasn't judged"
                    ),
                )
            }
            Status::NoisyCanary => ("warning", "moved, so the run was noisy".to_owned()),
            Status::OverBudget if outcome == Outcome::Rerun => (
                "error",
                "is over budget; the job will fail, but the next attempt still runs, so \
                 regressions are judged"
                    .to_owned(),
            ),
            Status::OverBudget => ("error", "is over budget".to_owned()),
            Status::OverReportOnlyBudget => (
                "warning",
                "is over its budget, which is report-only (docs/adr/0015-structural-save-budgets.md)"
                    .to_owned(),
            ),
            Status::Missing => ("error", "has a budget but no result".to_owned()),
            Status::Ok | Status::Canary | Status::Info => continue,
        };
        annotate(level, &format!("benchmark {} {what}", row.id));
    }
    for id in &report.earlier_over_budget {
        if report
            .row(id)
            .is_none_or(|row| !matches!(row.status, Status::OverBudget | Status::Missing))
        {
            annotate(
                "error",
                &format!(
                    "benchmark {id} was over its budget (or missing) on an earlier attempt, so \
                     the job fails"
                ),
            );
        }
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
            &format!(
                "benchmarks inconclusive: {} (budgets passed). A regression fails only if every \
                 attempt shows it; see docs/tasks/1.2b.md",
                why_inconclusive(report)
            ),
        );
    }
}
