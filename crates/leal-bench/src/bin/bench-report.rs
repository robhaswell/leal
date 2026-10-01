//! Reports criterion's results: budgets, regressions and noise.
//! `just bench-compare` runs it, and so does CI.
//!
//! ```text
//! bench-report [--regression FRACTION] [--noise FRACTION] CRITERION_DIR
//! ```
//!
//! Prints a Markdown table, then exits with status 0 if the report passed,
//! 1 if it failed (a budget, or a regression whose 95% interval is wholly
//! above `--regression`, default 0.20), 3 if the run was too noisy to judge
//! (a canary moved by more than `--noise`, default 0.10), or 2 on an error.
//! On GitHub Actions it also writes the table to the job summary and each
//! problem as an annotation.

use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::ExitCode;

use leal_bench::budgets::BUDGETS;
use leal_bench::report::{self, Status, Thresholds, Verdict};

fn main() -> ExitCode {
    match run() {
        Ok(Verdict::Pass) => ExitCode::SUCCESS,
        Ok(Verdict::Fail) => ExitCode::from(1),
        Ok(Verdict::Noisy) => ExitCode::from(3),
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<Verdict, String> {
    let mut thresholds = Thresholds {
        regression: 0.20,
        noise: 0.10,
    };
    let mut dir = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let target = match arg.as_str() {
            "--regression" => &mut thresholds.regression,
            "--noise" => &mut thresholds.noise,
            _ if dir.is_none() && !arg.starts_with('-') => {
                dir = Some(PathBuf::from(arg));
                continue;
            }
            _ => return Err(format!("unexpected argument `{arg}`")),
        };
        let value = args.next().ok_or(format!("{arg} needs a value"))?;
        *target = value.parse().map_err(|e| format!("{arg} `{value}`: {e}"))?;
    }
    let dir =
        dir.ok_or("usage: bench-report [--regression FRACTION] [--noise FRACTION] CRITERION_DIR")?;

    let measurements =
        report::collect(&dir).map_err(|e| format!("reading {}: {e}", dir.display()))?;
    let report = report::evaluate(&measurements, BUDGETS, thresholds);
    let markdown = report.markdown();
    println!("{markdown}");

    let github = std::env::var_os("GITHUB_ACTIONS").is_some();
    if let Some(summary) = std::env::var_os("GITHUB_STEP_SUMMARY") {
        let mut file = OpenOptions::new()
            .append(true)
            .create(true)
            .open(&summary)
            .map_err(|e| format!("opening the job summary: {e}"))?;
        writeln!(file, "## Benchmarks\n\n{markdown}")
            .map_err(|e| format!("writing the job summary: {e}"))?;
    }
    for row in report.problems() {
        let (level, what) = match row.status {
            Status::Regression => ("error", "is a regression"),
            Status::NoisyRegression => ("warning", "may be a regression (noisy run)"),
            Status::NoisyCanary => ("warning", "moved, so the run was noisy"),
            Status::OverBudget => ("error", "is over budget"),
            Status::Missing => ("error", "has a budget but no result"),
            Status::Ok | Status::Canary => continue,
        };
        if github {
            println!("::{level}::benchmark {} {what}", row.id);
        } else {
            println!("{level}: benchmark {} {what}", row.id);
        }
    }
    Ok(report.verdict())
}
