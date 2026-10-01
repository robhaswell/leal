//! Reports criterion's results: budgets, regressions and noise.
//! `just bench-compare` runs it, and so does CI.
//!
//! ```text
//! bench-report [--threshold FRACTION] CRITERION_DIR
//! ```
//!
//! Prints a Markdown table and exits with status 1 if a benchmark is over
//! budget, missing, or a regression past the threshold (default 0.25, that
//! is 25% slower). On GitHub Actions it also writes the table to the job
//! summary and each problem as an annotation.

use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::ExitCode;

use leal_bench::budgets::BUDGETS;
use leal_bench::report::{self, Status};

fn main() -> ExitCode {
    match run() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::from(2)
        }
    }
}

/// Returns whether the report passed.
fn run() -> Result<bool, String> {
    let mut threshold = 0.25;
    let mut dir = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--threshold" => {
                let value = args.next().ok_or("--threshold needs a value")?;
                threshold = value
                    .parse()
                    .map_err(|e| format!("--threshold `{value}`: {e}"))?;
            }
            _ if dir.is_none() && !arg.starts_with('-') => dir = Some(PathBuf::from(arg)),
            _ => return Err(format!("unexpected argument `{arg}`")),
        }
    }
    let dir = dir.ok_or("usage: bench-report [--threshold FRACTION] CRITERION_DIR")?;

    let measurements =
        report::collect(&dir).map_err(|e| format!("reading {}: {e}", dir.display()))?;
    let report = report::evaluate(&measurements, BUDGETS, threshold);
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
    Ok(!report.failed())
}
