//! The benchmark report: reading criterion's results, and the checks CI
//! fails on (budgets, regressions and noise).

use std::fs;
use std::path::{Path, PathBuf};

use leal_bench::budgets::Budget;
use leal_bench::report::{self, Change, Measurement, Outcome, Status, Thresholds, Verdict};

/// The thresholds CI uses: a regression is a confidence interval wholly
/// above +20%; a canary that moves by more than 10% means noise.
const CI: Thresholds = Thresholds {
    regression: 0.20,
    noise: 0.10,
};

/// A fresh, empty directory for one test.
fn scratch(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("report")
        .join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn estimate(point: f64, lower: f64, upper: f64) -> String {
    format!(
        r#"{{"confidence_interval":{{"confidence_level":0.95,"lower_bound":{lower},"upper_bound":{upper}}},"point_estimate":{point},"standard_error":1.0}}"#
    )
}

/// Writes one benchmark's results the way criterion lays them out.
fn write_bench(root: &Path, id: &str, median_ns: f64, bytes: Option<u64>, change: Option<Change>) {
    let dir = root.join(id.replace('/', "_"));
    let throughput = bytes.map_or("null".to_owned(), |b| format!(r#"{{"Bytes":{b}}}"#));
    fs::create_dir_all(dir.join("new")).unwrap();
    fs::write(
        dir.join("new/benchmark.json"),
        format!(
            r#"{{"group_id":"g","function_id":"f","value_str":null,"throughput":{throughput},"full_id":"{id}","directory_name":"{id}","title":"{id}"}}"#
        ),
    )
    .unwrap();
    let e = estimate(median_ns, median_ns * 0.99, median_ns * 1.01);
    fs::write(
        dir.join("new/estimates.json"),
        format!(r#"{{"mean":{e},"median":{e},"median_abs_dev":{e},"slope":null,"std_dev":{e}}}"#),
    )
    .unwrap();
    if let Some(change) = change {
        fs::create_dir_all(dir.join("change")).unwrap();
        // The mean's interval differs from the median's, so reading the
        // wrong one would show.
        let mean = estimate(9.0, 8.0, 10.0);
        let median = estimate(change.median, change.lower, change.upper);
        fs::write(
            dir.join("change/estimates.json"),
            format!(r#"{{"mean":{mean},"median":{median}}}"#),
        )
        .unwrap();
    }
}

/// A change whose 95% interval is `median ± 0.03`.
fn change(median: f64) -> Change {
    Change {
        median,
        lower: median - 0.03,
        upper: median + 0.03,
    }
}

fn measurement(id: &str, median_ns: f64, change: Option<Change>) -> Measurement {
    Measurement {
        id: id.to_owned(),
        median_ns,
        throughput_bytes: None,
        change,
    }
}

fn status_of<'a>(report: &'a report::Report, id: &str) -> &'a Status {
    &report.rows.iter().find(|row| row.id == id).unwrap().status
}

#[test]
fn collect_reads_criterion_results() {
    let dir = scratch("collect");
    let scan_change = Change {
        median: 0.02,
        lower: -0.01,
        upper: 0.05,
    };
    write_bench(
        &dir,
        "baseline/memchr3_scan",
        25e6,
        Some(100_000_000),
        Some(scan_change),
    );
    write_bench(&dir, "index/build", 300e6, None, None);
    // Criterion's own summary folder and saved baselines are not results.
    fs::create_dir_all(dir.join("report")).unwrap();
    fs::create_dir_all(dir.join("baseline_memchr3_scan/base")).unwrap();

    let found = report::collect(&dir).unwrap();

    assert_eq!(
        found,
        [
            Measurement {
                id: "baseline/memchr3_scan".to_owned(),
                median_ns: 25e6,
                throughput_bytes: Some(100_000_000),
                change: Some(scan_change),
            },
            Measurement {
                id: "index/build".to_owned(),
                median_ns: 300e6,
                throughput_bytes: None,
                change: None,
            },
        ]
    );
}

#[test]
fn collect_fails_on_a_missing_directory() {
    let dir = scratch("missing").join("nothing-here");
    assert!(report::collect(&dir).is_err());
}

/// Only the CPU-bound scan is a canary, in both baseline groups.
/// `sequential_read` is a baseline benchmark but not a canary.
#[test]
fn only_memchr3_scan_is_a_canary() {
    assert!(report::is_canary("baseline/memchr3_scan"));
    assert!(report::is_canary("baseline-late/memchr3_scan"));
    assert!(!report::is_canary("baseline/sequential_read"));
    assert!(!report::is_canary("baseline-late/sequential_read"));
    assert!(!report::is_canary("baselines/memchr3_scan"));
    assert!(!report::is_canary("index/build"));

    assert!(report::is_baseline("baseline/sequential_read"));
    assert!(report::is_baseline("baseline-late/sequential_read"));
    assert!(!report::is_baseline("baselines/x"));
    assert!(!report::is_baseline("index/build"));
}

/// `sequential_read` is I/O and page-cache bound and takes about 7 ms, so
/// on a shared runner it moves by 10–16% with no code change. It is
/// reported for information, and neither makes a run noisy nor regresses.
#[test]
fn sequential_read_moving_is_not_noise() {
    let found = [
        measurement("baseline/sequential_read", 7e6, Some(change(0.20))),
        measurement("baseline-late/sequential_read", 7e6, Some(change(-0.20))),
        measurement("baseline/memchr3_scan", 47e6, Some(change(0.02))),
        measurement("baseline-late/memchr3_scan", 47e6, Some(change(-0.01))),
        measurement("index/build", 1e6, Some(change(0.04))),
    ];
    let report = report::evaluate(&found, &[], CI);
    assert!(!report.noisy);
    assert_eq!(
        status_of(&report, "baseline/sequential_read"),
        &Status::Info
    );
    assert_eq!(
        status_of(&report, "baseline-late/sequential_read"),
        &Status::Info
    );
    assert_eq!(status_of(&report, "baseline/memchr3_scan"), &Status::Canary);
    assert_eq!(report.problems().count(), 0);
    assert_eq!(report.verdict(), Verdict::Pass);
    assert_eq!(report.outcome(false), Outcome::Pass);
    assert_eq!(report.outcome(true), Outcome::Pass);
}

/// Even a slowdown big enough to count as a regression elsewhere doesn't
/// gate `sequential_read`: the baseline's code is the same on both sides.
#[test]
fn sequential_read_never_regresses() {
    let found = [measurement(
        "baseline/sequential_read",
        7e6,
        Some(change(0.60)),
    )];
    let report = report::evaluate(&found, &[], CI);
    assert_eq!(
        status_of(&report, "baseline/sequential_read"),
        &Status::Info
    );
    assert_eq!(report.verdict(), Verdict::Pass);
}

/// A regression is a slowdown whose whole 95% interval is above the
/// threshold, so a noisy median alone doesn't fail the job.
#[test]
fn a_regression_needs_the_interval_above_the_threshold() {
    let found = [
        measurement("baseline/memchr3_scan", 1e6, Some(change(0.01))),
        measurement("baseline-late/memchr3_scan", 1e6, Some(change(-0.02))),
        // Interval 0.27..0.33: a regression.
        measurement("index/build", 1e6, Some(change(0.30))),
        // Median 0.25, but the interval reaches down to 0.15.
        measurement(
            "rows/parse",
            1e6,
            Some(Change {
                median: 0.25,
                lower: 0.15,
                upper: 0.35,
            }),
        ),
        // Interval 0.17..0.23: not wholly above 0.20.
        measurement("rows/cache", 1e6, Some(change(0.20))),
        measurement("rows/new", 1e6, None),
    ];
    let report = report::evaluate(&found, &[], CI);
    assert_eq!(status_of(&report, "index/build"), &Status::Regression);
    assert_eq!(status_of(&report, "rows/parse"), &Status::Ok);
    assert_eq!(status_of(&report, "rows/cache"), &Status::Ok);
    assert_eq!(status_of(&report, "rows/new"), &Status::Ok);
    assert_eq!(status_of(&report, "baseline/memchr3_scan"), &Status::Canary);
    assert_eq!(
        status_of(&report, "baseline-late/memchr3_scan"),
        &Status::Canary
    );
    assert!(!report.noisy);
    assert_eq!(report.verdict(), Verdict::Fail);
}

/// A real regression on a quiet run fails, on any attempt.
#[test]
fn a_regression_on_a_quiet_run_fails() {
    let found = [
        measurement("baseline/memchr3_scan", 47e6, Some(change(0.03))),
        measurement("baseline-late/memchr3_scan", 47e6, Some(change(-0.04))),
        measurement("index/build", 300e6, Some(change(0.40))),
    ];
    let report = report::evaluate(&found, &[], CI);
    assert!(!report.noisy);
    assert_eq!(status_of(&report, "index/build"), &Status::Regression);
    for last_attempt in [false, true] {
        let outcome = report.outcome(last_attempt);
        assert_eq!(outcome, Outcome::Fail, "last attempt: {last_attempt}");
        assert_eq!(outcome.exit_code(), 1);
    }
}

#[test]
fn speedups_never_fail() {
    let found = [measurement("index/build", 1e6, Some(change(-0.60)))];
    let report = report::evaluate(&found, &[], CI);
    assert_eq!(status_of(&report, "index/build"), &Status::Ok);
    assert_eq!(report.verdict(), Verdict::Pass);
}

/// The canaries run the same code before and after, so if one moves by
/// more than the noise threshold, either way, the machine was too noisy to
/// judge: `bench-compare` reruns it.
#[test]
fn a_moving_canary_makes_the_run_noisy() {
    for (canary, canary_change) in [
        ("baseline/memchr3_scan", 0.11),
        ("baseline/memchr3_scan", -0.11),
        ("baseline-late/memchr3_scan", 0.11),
    ] {
        let found = [
            measurement(canary, 1e6, Some(change(canary_change))),
            measurement("index/build", 1e6, Some(change(0.50))),
            measurement("rows/parse", 1e6, Some(change(0.0))),
        ];
        let report = report::evaluate(&found, &[], CI);
        assert!(report.noisy, "{canary} {canary_change}");
        assert_eq!(status_of(&report, canary), &Status::NoisyCanary);
        assert_eq!(status_of(&report, "index/build"), &Status::NoisyRegression);
        assert_eq!(status_of(&report, "rows/parse"), &Status::Ok);
        assert_eq!(report.verdict(), Verdict::Noisy);
        assert_eq!(report.outcome(false), Outcome::Rerun);
        assert_eq!(report.outcome(false).exit_code(), 3);
    }
}

/// Noisy on the last attempt too (the 79fa5af failure): the run is
/// inconclusive, which passes. The possible regression in it isn't judged,
/// and a noisy runner is no reason to turn CI red.
#[test]
fn a_noisy_last_attempt_is_inconclusive_and_passes() {
    let budgets = [Budget {
        id: "index/build",
        max_ms: 500.0,
        source: "DESIGN §1",
    }];
    let found = [
        measurement("baseline/memchr3_scan", 47e6, Some(change(0.02))),
        measurement("baseline-late/memchr3_scan", 47e6, Some(change(0.16))),
        measurement("index/build", 300e6, Some(change(0.50))),
    ];
    let report = report::evaluate(&found, &budgets, CI);
    assert!(report.noisy);
    assert_eq!(status_of(&report, "index/build"), &Status::NoisyRegression);
    assert_eq!(report.verdict(), Verdict::Noisy);
    let outcome = report.outcome(true);
    assert_eq!(outcome, Outcome::Inconclusive);
    assert_eq!(outcome.exit_code(), 0);
    assert_eq!(outcome.label(), "inconclusive");
}

#[test]
fn a_canary_within_the_noise_threshold_is_quiet() {
    let found = [
        measurement("baseline/memchr3_scan", 1e6, Some(change(0.09))),
        measurement("baseline-late/memchr3_scan", 1e6, Some(change(-0.09))),
    ];
    let report = report::evaluate(&found, &[], CI);
    assert!(!report.noisy);
    assert_eq!(report.verdict(), Verdict::Pass);
}

#[test]
fn over_budget_fails_even_on_a_noisy_run() {
    let budgets = [Budget {
        id: "index/build",
        max_ms: 500.0,
        source: "DESIGN §1",
    }];
    let found = [
        measurement("baseline/memchr3_scan", 1e6, Some(change(0.90))),
        measurement("index/build", 501e6, None),
    ];
    let report = report::evaluate(&found, &budgets, CI);
    assert!(report.noisy);
    assert_eq!(status_of(&report, "index/build"), &Status::OverBudget);
    assert_eq!(report.verdict(), Verdict::Fail);
    // On the first attempt and the last: noise never excuses a budget.
    for last_attempt in [false, true] {
        let outcome = report.outcome(last_attempt);
        assert_eq!(outcome, Outcome::Fail, "last attempt: {last_attempt}");
        assert_eq!(outcome.exit_code(), 1);
    }

    let found = [measurement("index/build", 499e6, Some(change(0.10)))];
    let report = report::evaluate(&found, &budgets, CI);
    assert_eq!(status_of(&report, "index/build"), &Status::Ok);
    assert_eq!(report.verdict(), Verdict::Pass);
}

/// A budget whose benchmark didn't run (renamed, deleted, filtered out)
/// would otherwise stop being checked without anyone noticing.
#[test]
fn a_budget_with_no_result_fails() {
    let budgets = [Budget {
        id: "index/build",
        max_ms: 500.0,
        source: "DESIGN §1",
    }];
    let report = report::evaluate(&[], &budgets, CI);
    assert_eq!(status_of(&report, "index/build"), &Status::Missing);
    assert_eq!(report.verdict(), Verdict::Fail);
}

#[test]
fn markdown_lists_every_benchmark() {
    let budgets = [Budget {
        id: "index/build",
        max_ms: 500.0,
        source: "DESIGN §1",
    }];
    let found = [
        Measurement {
            id: "baseline/memchr3_scan".to_owned(),
            median_ns: 20e6,
            throughput_bytes: Some(100 << 20),
            change: Some(Change {
                median: 0.012,
                lower: -0.004,
                upper: 0.031,
            }),
        },
        measurement("index/build", 320e6, Some(change(-0.05))),
    ];
    let markdown = report::evaluate(&found, &budgets, CI).markdown();
    assert!(
        markdown.contains(
            "| `baseline/memchr3_scan` | 20.0 ms | 4.88 GiB/s | +1.2% (-0.4% to +3.1%) |"
        ),
        "{markdown}"
    );
    assert!(
        markdown.contains(
            "| `index/build` | 320 ms |  | -5.0% (-8.0% to -2.0%) | 500 ms (DESIGN §1) | ok |"
        ),
        "{markdown}"
    );
}

/// Every budget names a benchmark id, and no two budgets name the same one.
#[test]
fn budgets_are_well_formed() {
    let budgets = leal_bench::budgets::BUDGETS;
    for budget in budgets {
        assert!(budget.id.contains('/'), "`{}` is `group/name`", budget.id);
        assert!(
            !report::is_baseline(budget.id),
            "baseline benchmarks have no budget"
        );
        assert!(budget.max_ms > 0.0);
        assert!(!budget.source.is_empty());
    }
    let mut ids: Vec<_> = budgets.iter().map(|budget| budget.id).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), budgets.len(), "duplicate budget ids");
}

/// Runs `bench-report` as `just bench-compare` does on GitHub Actions, and
/// returns its exit status, its output and the job summary it wrote.
fn run_bench_report(dir: &Path, last_attempt: bool) -> (Option<i32>, String, String) {
    let summary = dir.join("summary.md");
    let _ = fs::remove_file(&summary);
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_bench-report"));
    if last_attempt {
        command.arg("--last-attempt");
    }
    let output = command
        .arg(dir.join("criterion"))
        .env("GITHUB_ACTIONS", "true")
        .env("GITHUB_STEP_SUMMARY", &summary)
        .output()
        .unwrap();
    (
        output.status.code(),
        String::from_utf8(output.stdout).unwrap(),
        fs::read_to_string(&summary).unwrap_or_default(),
    )
}

/// Results for the real budgets' benchmarks, at `index_ms`, and for the
/// baseline, with `baseline-late/memchr3_scan` moved by `late_change` and
/// `baseline-late/sequential_read` by +15.7% (as at 79fa5af).
fn write_run(dir: &Path, index_ms: f64, late_change: f64) {
    let criterion = dir.join("criterion");
    for budget in leal_bench::budgets::BUDGETS {
        write_bench(
            &criterion,
            budget.id,
            index_ms * 1e6,
            None,
            Some(change(0.04)),
        );
    }
    write_bench(
        &criterion,
        "baseline/memchr3_scan",
        47e6,
        None,
        Some(change(0.02)),
    );
    write_bench(
        &criterion,
        "baseline-late/memchr3_scan",
        47e6,
        None,
        Some(change(late_change)),
    );
    write_bench(
        &criterion,
        "baseline-late/sequential_read",
        7e6,
        None,
        Some(change(0.157)),
    );
}

/// A noisy run asks for a rerun (exit 3). Noisy on the last attempt too, it
/// warns, marks the job summary inconclusive, and passes (exit 0).
#[test]
fn bench_report_passes_a_noisy_last_attempt_with_a_warning() {
    let dir = scratch("bin-noisy");
    write_run(&dir, 300.0, 0.157);

    let (code, stdout, summary) = run_bench_report(&dir, false);
    assert_eq!(code, Some(3), "{stdout}");
    assert!(
        summary.contains("## Benchmarks: noisy, rerunning"),
        "{summary}"
    );

    let (code, stdout, summary) = run_bench_report(&dir, true);
    assert_eq!(code, Some(0), "{stdout}");
    assert!(
        stdout.contains("::warning::benchmarks inconclusive"),
        "{stdout}"
    );
    assert!(!stdout.contains("::error::"), "{stdout}");
    assert!(summary.contains("## Benchmarks: inconclusive"), "{summary}");
    assert!(summary.contains("**Inconclusive:**"), "{summary}");
}

/// Over budget on a noisy last attempt still fails, with an error.
#[test]
fn bench_report_fails_a_budget_on_a_noisy_last_attempt() {
    let dir = scratch("bin-over-budget");
    write_run(&dir, 501.0, 0.157);

    let (code, stdout, summary) = run_bench_report(&dir, true);
    assert_eq!(code, Some(1), "{stdout}");
    assert!(
        stdout.contains("::error::benchmark index/build is over budget"),
        "{stdout}"
    );
    assert!(summary.contains("## Benchmarks: fail"), "{summary}");
}

/// A quiet run within budget passes, whatever `sequential_read` did.
#[test]
fn bench_report_passes_a_quiet_run() {
    let dir = scratch("bin-quiet");
    write_run(&dir, 300.0, -0.01);

    let (code, stdout, summary) = run_bench_report(&dir, false);
    assert_eq!(code, Some(0), "{stdout}");
    assert!(!stdout.contains("::warning::"), "{stdout}");
    assert!(summary.contains("## Benchmarks: pass"), "{summary}");
}
