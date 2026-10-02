//! The benchmark report: reading criterion's results, and the checks CI
//! fails on (budgets, regressions and noise).

use std::fs;
use std::path::{Path, PathBuf};

use leal_bench::budgets::Budget;
use leal_bench::report::{
    self, Canaries, Change, GroupCanary, Measurement, Order, Outcome, Side, Status, Thresholds,
    Verdict, group_canary_id,
};

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

/// A budget-only benchmark never fails on change between commits, but
/// still fails when it goes over its budget.
#[test]
fn budget_only_benchmarks_gate_on_their_budget_alone() {
    let id = "open/first_paint_removable_under_load";
    let budget = [Budget {
        id,
        max_ms: 150.0,
        source: "DESIGN §1",
    }];
    let slower = [measurement(id, 7e6, Some(change(0.70)))];
    let report = report::evaluate(&slower, &budget, CI);
    assert_eq!(status_of(&report, id), &Status::Info);
    assert_eq!(report.verdict(), Verdict::Pass);

    let over = [measurement(id, 151e6, Some(change(0.0)))];
    let report = report::evaluate(&over, &budget, CI);
    assert_eq!(status_of(&report, id), &Status::OverBudget);
    assert_ne!(report.verdict(), Verdict::Pass);
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

/// A regression on a quiet first attempt is rerun to confirm it. If that
/// was the only attempt (`--last-attempt`, with no rerun), it fails.
#[test]
fn a_regression_on_a_quiet_run_is_rerun_then_fails() {
    let found = [
        measurement("baseline/memchr3_scan", 47e6, Some(change(0.03))),
        measurement("baseline-late/memchr3_scan", 47e6, Some(change(-0.04))),
        measurement("index/build", 300e6, Some(change(0.40))),
    ];
    let report = report::evaluate(&found, &[], CI);
    assert!(!report.noisy);
    assert_eq!(status_of(&report, "index/build"), &Status::Regression);
    assert_eq!(report.verdict(), Verdict::Fail);
    assert_eq!(report.outcome(false), Outcome::Rerun);
    assert_eq!(report.outcome(false).exit_code(), 3);
    assert_eq!(report.outcome(true), Outcome::Fail);
    assert_eq!(report.outcome(true).exit_code(), 1);
}

/// One attempt's results: the two canaries moved by `early` and `late`,
/// `index/build` (budget 500 ms) at `build_ms` and changed by
/// `build_change`, and `rows/parse` changed by `parse_change`.
fn attempt(
    early: f64,
    late: f64,
    build_ms: f64,
    build_change: f64,
    parse_change: f64,
) -> report::Report {
    let budgets = [Budget {
        id: "index/build",
        max_ms: 500.0,
        source: "DESIGN §1",
    }];
    let found = [
        measurement("baseline/memchr3_scan", 47e6, Some(change(early))),
        measurement("baseline-late/memchr3_scan", 47e6, Some(change(late))),
        measurement("index/build", build_ms * 1e6, Some(change(build_change))),
        measurement("rows/parse", 12e3, Some(change(parse_change))),
    ];
    report::evaluate(&found, &budgets, CI)
}

/// A regression on both attempts fails at once if both were quiet for it.
/// If either was noisy for it (here, with no group canaries, the run-wide
/// ones judge it), a third attempt settles it; where none may run (the
/// rerun is the last attempt), it fails, as it always did.
#[test]
fn a_regression_on_every_attempt_fails() {
    for first_canary in [0.02, -0.15] {
        let first = attempt(first_canary, 0.01, 300.0, 0.0, 0.35);
        assert_eq!(first.outcome(false), Outcome::Rerun);
        for rerun_canary in [0.03, 0.16] {
            let context = format!("canaries {first_canary} then {rerun_canary}");
            let rerun = attempt(0.01, rerun_canary, 300.0, 0.0, 0.33).after(&first);
            assert!(rerun.unconfirmed.is_empty());
            let quiet = first_canary.abs() < 0.1 && rerun_canary.abs() < 0.1;
            if quiet {
                assert_eq!(status_of(&rerun, "rows/parse"), &Status::Regression);
                // No third attempt: it fails even when one may run.
                assert_eq!(rerun.outcome(false), Outcome::Fail, "{context}");
                assert_eq!(rerun.third_attempt_plan(), None);
            } else {
                assert_eq!(status_of(&rerun, "rows/parse"), &Status::Recheck);
                assert_eq!(rerun.outcome(false), Outcome::Rerun, "{context}");
            }
            let outcome = rerun.outcome(true);
            assert_eq!(outcome, Outcome::Fail, "{context}");
            assert_eq!(outcome.exit_code(), 1);
        }
    }
}

/// A regression only on the rerun isn't judged: the run is inconclusive.
/// This was main at `ed09773`: attempt 1 was noisy (a canary at −15%) with
/// no regression, and attempt 2 showed +30% in code identical on both
/// sides, with the late canary at +10.0%, just under the noise limit.
#[test]
fn a_regression_only_on_the_rerun_is_inconclusive() {
    let first = attempt(-0.153, 0.043, 300.0, 0.0, -0.08);
    assert!(first.noisy);
    assert_eq!(first.outcome(false), Outcome::Rerun);
    for rerun_canary in [0.10, 0.16] {
        let rerun = attempt(0.044, rerun_canary, 300.0, 0.0, 0.307).after(&first);
        assert_eq!(
            status_of(&rerun, "rows/parse"),
            &Status::UnconfirmedRegression
        );
        let outcome = rerun.outcome(true);
        assert_eq!(outcome, Outcome::Inconclusive, "late canary {rerun_canary}");
        assert_eq!(outcome.exit_code(), 0);
    }
}

/// A regression only on the first attempt passes if the rerun is quiet,
/// and is inconclusive if the rerun is noisy.
#[test]
fn a_regression_only_on_the_first_attempt() {
    let first = attempt(0.02, 0.01, 300.0, 0.0, 0.35);
    assert_eq!(first.outcome(false), Outcome::Rerun);

    let quiet = attempt(0.01, -0.02, 300.0, 0.0, 0.01).after(&first);
    assert_eq!(status_of(&quiet, "rows/parse"), &Status::Ok);
    assert_eq!(quiet.unconfirmed, ["rows/parse"]);
    assert_eq!(quiet.outcome(true), Outcome::Pass);
    assert!(
        quiet.markdown().contains(
            "Regressed on attempt 1 but not on this attempt, so not failed: `rows/parse`."
        ),
        "{}",
        quiet.markdown()
    );

    let noisy = attempt(0.01, 0.16, 300.0, 0.0, 0.01).after(&first);
    assert_eq!(noisy.unconfirmed, ["rows/parse"]);
    assert_eq!(noisy.outcome(true), Outcome::Inconclusive);
}

/// A budget fails on either attempt, noisy or not, with or without a
/// regression.
#[test]
fn a_budget_fails_on_either_attempt() {
    for canary in [0.02, 0.16] {
        // On the first attempt: no rerun.
        let first = attempt(canary, 0.01, 501.0, 0.0, 0.35);
        assert_eq!(first.outcome(false), Outcome::Fail, "canary {canary}");

        // On the rerun, after a first attempt within budget.
        let first = attempt(canary, 0.01, 300.0, 0.0, 0.35);
        let rerun = attempt(0.01, canary, 501.0, 0.0, 0.0).after(&first);
        assert_eq!(status_of(&rerun, "index/build"), &Status::OverBudget);
        assert_eq!(rerun.outcome(true), Outcome::Fail, "canary {canary}");
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
            "| `index/build` | 320 ms |  | -5.0% (-8.0% to -2.0%) | run-wide | 500 ms (DESIGN §1) | ok |"
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

/// Results for the real budgets' benchmarks, each at `of_budget` times its
/// own budget (budgets differ: 500 ms for the index, 1 ms for a screen of
/// rows), and for the baseline, with `baseline-late/memchr3_scan` moved by
/// `late_change` and `baseline-late/sequential_read` by +15.7% (as at
/// 79fa5af).
fn write_run(dir: &Path, of_budget: f64, late_change: f64) {
    let criterion = dir.join("criterion");
    for budget in leal_bench::budgets::BUDGETS {
        write_bench(
            &criterion,
            budget.id,
            budget.max_ms * of_budget * 1e6,
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
    write_run(&dir, 0.6, 0.157);

    let (code, stdout, summary) = run_bench_report(&dir, false);
    assert_eq!(code, Some(3), "{stdout}");
    assert!(
        summary.contains("## Benchmarks: rerunning to confirm"),
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
    write_run(&dir, 1.002, 0.157);

    let (code, stdout, summary) = run_bench_report(&dir, true);
    assert_eq!(code, Some(1), "{stdout}");
    assert!(
        stdout.contains("::error::benchmark index/build is over budget"),
        "{stdout}"
    );
    assert!(summary.contains("## Benchmarks: fail"), "{summary}");
}

/// Runs `bench-report --first-attempt <dir>/first/criterion` on
/// `<dir>/criterion`, as `just bench-compare` does for the rerun.
fn run_bench_report_rerun(dir: &Path) -> (Option<i32>, String) {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_bench-report"))
        .arg("--first-attempt")
        .arg(dir.join("first").join("criterion"))
        .arg(dir.join("criterion"))
        .env("GITHUB_ACTIONS", "true")
        .env_remove("GITHUB_STEP_SUMMARY")
        .output()
        .unwrap();
    (
        output.status.code(),
        String::from_utf8(output.stdout).unwrap(),
    )
}

/// The rerun, given the first attempt's results: a regression on both
/// fails; one only on the rerun is inconclusive; one only on the first
/// attempt passes when the rerun is quiet.
#[test]
fn bench_report_fails_a_regression_only_on_every_attempt() {
    let regressed = |dir: &Path| {
        write_bench(
            &dir.join("criterion"),
            "rows/parse",
            12e3,
            None,
            Some(change(0.31)),
        );
    };
    let held = |dir: &Path| {
        write_bench(
            &dir.join("criterion"),
            "rows/parse",
            12e3,
            None,
            Some(change(0.01)),
        );
    };

    let dir = scratch("bin-rerun-both");
    write_run(&dir.join("first"), 0.6, -0.01);
    regressed(&dir.join("first"));
    write_run(&dir, 0.6, -0.01);
    regressed(&dir);
    let (code, stdout) = run_bench_report_rerun(&dir);
    assert_eq!(code, Some(1), "{stdout}");
    assert!(
        stdout.contains("::error::benchmark rows/parse is a regression"),
        "{stdout}"
    );

    let dir = scratch("bin-rerun-only");
    write_run(&dir.join("first"), 0.6, -0.01);
    held(&dir.join("first"));
    write_run(&dir, 0.6, 0.10);
    regressed(&dir);
    let (code, stdout) = run_bench_report_rerun(&dir);
    assert_eq!(code, Some(0), "{stdout}");
    assert!(!stdout.contains("::error::"), "{stdout}");
    assert!(
        stdout.contains("::warning::benchmarks inconclusive"),
        "{stdout}"
    );
    assert!(
        stdout.contains("regression? (not on attempt 1)"),
        "{stdout}"
    );

    let dir = scratch("bin-rerun-first-only");
    write_run(&dir.join("first"), 0.6, -0.01);
    regressed(&dir.join("first"));
    write_run(&dir, 0.6, -0.01);
    held(&dir);
    let (code, stdout) = run_bench_report_rerun(&dir);
    assert_eq!(code, Some(0), "{stdout}");
    assert!(!stdout.contains("::error::"), "{stdout}");
    assert!(!stdout.contains("inconclusive"), "{stdout}");
    assert!(
        stdout.contains("benchmark rows/parse regressed on attempt 1 but not on the rerun"),
        "{stdout}"
    );
}

/// A quiet run within budget passes, whatever `sequential_read` did.
#[test]
fn bench_report_passes_a_quiet_run() {
    let dir = scratch("bin-quiet");
    write_run(&dir, 0.6, -0.01);

    let (code, stdout, summary) = run_bench_report(&dir, false);
    assert_eq!(code, Some(0), "{stdout}");
    assert!(!stdout.contains("::warning::"), "{stdout}");
    assert!(summary.contains("## Benchmarks: pass"), "{summary}");
}

/// The order's names in `bench-compare-order`, and reading that file: none
/// when `bench-compare` wrote none, an error when it names no order.
#[test]
fn read_order_reads_what_bench_compare_writes() {
    use report::Order;

    for order in [Order::BaseFirst, Order::HeadFirst] {
        assert_eq!(order.name().parse::<Order>(), Ok(order));
    }
    assert_eq!(Order::BaseFirst.name(), "base-first");
    assert_eq!(Order::HeadFirst.name(), "head-first");
    assert!("base".parse::<Order>().is_err());

    let dir = scratch("order");
    assert_eq!(report::read_order(&dir).unwrap(), None);
    // `echo` adds a newline.
    fs::write(dir.join(report::ORDER_FILE), "head-first\n").unwrap();
    assert_eq!(report::read_order(&dir).unwrap(), Some(Order::HeadFirst));
    // The order file isn't a benchmark.
    assert!(report::collect(&dir).unwrap().is_empty());
    fs::write(dir.join(report::ORDER_FILE), "sideways\n").unwrap();
    assert!(report::read_order(&dir).is_err());
}

/// One attempt's results in the case that swapping the order is for
/// (main at `2cc71bb`, compared with `33e4bef`): `rows/parse` and
/// `navigate/next_nul_wide` changed by `parse` and `nul`, the first
/// canary by `early`, and the attempt ran in `order`.
fn drift_attempt(order: report::Order, early: f64, parse: Change, nul: Change) -> report::Report {
    let found = [
        measurement("baseline/memchr3_scan", 48e6, Some(change(early))),
        measurement("baseline-late/memchr3_scan", 50e6, Some(change(0.02))),
        measurement("rows/parse", 13.7e3, Some(parse)),
        measurement("navigate/next_nul_wide", 4.06e3, Some(nul)),
    ];
    let mut report = report::evaluate(&found, &[], CI);
    report.order = Some(order);
    report
}

/// Attempt 1 at `2cc71bb`, base first: noisy (`baseline/memchr3_scan`
/// −13.3%), with `rows/parse` at +26.4% (+25.7% to +26.8%) and
/// `navigate/next_nul_wide` at +40.0% (+34.6% to +47.7%).
fn drift_first() -> report::Report {
    drift_attempt(
        report::Order::BaseFirst,
        -0.133,
        Change {
            median: 0.264,
            lower: 0.257,
            upper: 0.268,
        },
        Change {
            median: 0.400,
            lower: 0.346,
            upper: 0.477,
        },
    )
}

/// The rerun runs head first, so the drift that slowed head on attempt 1
/// slows base instead, and the regressions don't repeat: the run passes,
/// naming them. The rerun's numbers are what a quiet Mac measured for the
/// same commits (`rows/parse` −3.6%, `next_nul_wide` +1.9%).
#[test]
fn a_drift_regression_does_not_repeat_in_the_other_order() {
    use report::Order;

    let first = drift_first();
    assert_eq!(first.outcome(false), Outcome::Rerun);

    let rerun = drift_attempt(Order::HeadFirst, 0.01, change(-0.036), change(0.019)).after(&first);
    assert_eq!(rerun.first_order, Some(Order::BaseFirst));
    assert_eq!(rerun.order, Some(Order::HeadFirst));
    assert!(!rerun.same_order());
    assert_eq!(status_of(&rerun, "rows/parse"), &Status::Ok);
    assert_eq!(rerun.unconfirmed, ["navigate/next_nul_wide", "rows/parse"]);
    assert_eq!(rerun.outcome(true), Outcome::Pass);

    let markdown = rerun.markdown();
    for expected in [
        "This attempt benchmarked this commit first, then the base commit.",
        "Attempt 1 benchmarked the base commit first, then this commit.",
        "Regressed on attempt 1 but not on this attempt, so not failed: \
         `navigate/next_nul_wide`, `rows/parse`.",
    ] {
        assert!(markdown.contains(expected), "{expected}\n\n{markdown}");
    }
    assert!(!markdown.contains("too**"), "{markdown}");
}

/// A real regression shows in both orders, so it still fails. Attempt 1
/// was noisy, so a third attempt, base first again, confirms it.
#[test]
fn a_real_regression_fails_in_both_orders() {
    let first = drift_first();
    let rerun =
        drift_attempt(report::Order::HeadFirst, 0.01, change(0.31), change(0.019)).after(&first);
    assert!(!rerun.same_order());
    assert_eq!(status_of(&rerun, "rows/parse"), &Status::Recheck);
    assert_eq!(rerun.unconfirmed, ["navigate/next_nul_wide"]);
    assert_eq!(rerun.outcome(false), Outcome::Rerun);
    assert_eq!(rerun.outcome(true), Outcome::Fail);

    let third = drift_attempt(report::Order::BaseFirst, 0.01, change(0.30), change(0.0));
    let settled = rerun.settle(third);
    assert_eq!(status_of(&settled, "rows/parse"), &Status::Regression);
    assert_eq!(settled.rechecked, ["rows/parse"]);
    assert_eq!(settled.outcome(true), Outcome::Fail);
}

/// What happened at `2cc71bb`, before the swap: both attempts ran base
/// first and both showed the drift. The rule is unchanged (a regression on
/// every attempt fails), but the report says the order was the same.
#[test]
fn the_same_order_on_both_attempts_is_flagged() {
    let first = drift_first();
    let rerun =
        drift_attempt(report::Order::BaseFirst, 0.08, change(0.264), change(0.264)).after(&first);
    assert!(rerun.same_order());
    assert_eq!(rerun.outcome(true), Outcome::Fail);
    assert!(
        rerun
            .markdown()
            .contains("**Attempt 1 benchmarked the base commit first, then this commit too**"),
        "{}",
        rerun.markdown()
    );

    // An unknown order (an attempt from before the swap) is never the same.
    let mut unknown = drift_first();
    unknown.order = None;
    let rerun =
        drift_attempt(report::Order::HeadFirst, 0.01, change(0.0), change(0.0)).after(&unknown);
    assert_eq!(rerun.first_order, None);
    assert!(!rerun.same_order());
}

/// Writes `bench-compare-order` into `<dir>/criterion`.
fn write_order(dir: &Path, order: &str) {
    fs::write(
        dir.join("criterion").join(report::ORDER_FILE),
        format!("{order}\n"),
    )
    .unwrap();
}

/// `bench-report` reads each attempt's order, names it in the annotations
/// and the table, warns when both attempts ran in the same order, and
/// fails (exit 2) on an order file it can't read.
#[test]
fn bench_report_reports_each_attempts_order() {
    // `rows/parse` as `bench-compare` leaves it: on a head-first attempt,
    // criterion's change is base's relative to head. Write the order first.
    let parse = |dir: &Path, regressed: bool| {
        let head = change(if regressed { 0.31 } else { -0.036 });
        let head_first =
            report::read_order(&dir.join("criterion")).unwrap() == Some(report::Order::HeadFirst);
        let written = if head_first { head.inverted() } else { head };
        write_bench(
            &dir.join("criterion"),
            "rows/parse",
            12e3,
            None,
            Some(written),
        );
    };

    let dir = scratch("bin-order-swapped");
    write_run(&dir.join("first"), 0.6, -0.01);
    write_order(&dir.join("first"), "base-first");
    parse(&dir.join("first"), true);
    write_run(&dir, 0.6, -0.01);
    write_order(&dir, "head-first");
    parse(&dir, false);
    let (code, stdout) = run_bench_report_rerun(&dir);
    assert_eq!(code, Some(0), "{stdout}");
    assert!(
        stdout.contains(
            "::warning::benchmark rows/parse regressed on attempt 1 (base first) but not on \
             the rerun (head first), so it passed"
        ),
        "{stdout}"
    );
    assert!(
        stdout.contains("This attempt benchmarked this commit first"),
        "{stdout}"
    );
    assert!(!stdout.contains("same order"), "{stdout}");

    let dir = scratch("bin-order-rerun-only");
    write_run(&dir.join("first"), 0.6, -0.01);
    write_order(&dir.join("first"), "base-first");
    parse(&dir.join("first"), false);
    write_run(&dir, 0.6, -0.01);
    write_order(&dir, "head-first");
    parse(&dir, true);
    let (code, stdout) = run_bench_report_rerun(&dir);
    assert_eq!(code, Some(0), "{stdout}");
    assert!(
        stdout.contains(
            "::warning::benchmark rows/parse regressed on the rerun (head first) but not on \
             attempt 1 (base first), so it wasn't judged"
        ),
        "{stdout}"
    );

    let dir = scratch("bin-order-same");
    write_run(&dir.join("first"), 0.6, -0.01);
    write_order(&dir.join("first"), "base-first");
    parse(&dir.join("first"), true);
    write_run(&dir, 0.6, -0.01);
    write_order(&dir, "base-first");
    parse(&dir, true);
    let (code, stdout) = run_bench_report_rerun(&dir);
    assert_eq!(code, Some(1), "{stdout}");
    assert!(
        stdout.contains("::warning::bench-compare ran both attempts in the same order"),
        "{stdout}"
    );

    write_order(&dir, "sideways");
    let (code, stdout) = run_bench_report_rerun(&dir);
    assert_eq!(code, Some(2), "{stdout}");
}

/// Inverting a change gives the other side's change relative to this one:
/// +25% one way is −20% the other, the interval's ends swap, and inverting
/// twice gives the change back.
#[test]
fn inverting_a_change_swaps_the_sides() {
    let base_vs_head = Change {
        median: 0.25,
        lower: 0.10,
        upper: 0.50,
    };
    let head_vs_base = base_vs_head.inverted();
    let close = |a: f64, b: f64| (a - b).abs() < 1e-12;
    assert!(close(head_vs_base.median, -0.20), "{head_vs_base:?}");
    assert!(
        close(head_vs_base.lower, 1.0 / 1.5 - 1.0),
        "{head_vs_base:?}"
    );
    assert!(
        close(head_vs_base.upper, 1.0 / 1.1 - 1.0),
        "{head_vs_base:?}"
    );
    assert!(head_vs_base.lower < head_vs_base.median);
    assert!(head_vs_base.median < head_vs_base.upper);
    let back = head_vs_base.inverted();
    assert!(close(back.median, 0.25), "{back:?}");
    assert!(close(back.lower, 0.10), "{back:?}");
    assert!(close(back.upper, 0.50), "{back:?}");
    assert!(close(change(0.0).inverted().median, 0.0));
}

/// On a head-first attempt, criterion compared base with head, so
/// `collect` inverts every change to be head's relative to base. On a
/// base-first attempt, or with no order recorded, it leaves them alone.
#[test]
fn collect_inverts_the_changes_of_a_head_first_attempt() {
    let dir = scratch("collect-head-first");
    // Base 25% slower than head: head is 20% faster than base.
    write_bench(&dir, "rows/parse", 12e3, None, Some(change(0.25)));
    // Only head has it, so there is nothing to invert.
    write_bench(&dir, "rows/new", 12e3, None, None);
    let find = |found: &[Measurement], id: &str| found.iter().find(|m| m.id == id).unwrap().clone();

    let found = report::collect(&dir).unwrap();
    assert_eq!(find(&found, "rows/parse").change, Some(change(0.25)));
    fs::write(dir.join(report::ORDER_FILE), "base-first\n").unwrap();
    let found = report::collect(&dir).unwrap();
    assert_eq!(find(&found, "rows/parse").change, Some(change(0.25)));

    fs::write(dir.join(report::ORDER_FILE), "head-first\n").unwrap();
    let found = report::collect(&dir).unwrap();
    let parse = find(&found, "rows/parse");
    assert_eq!(parse.change, Some(change(0.25).inverted()));
    assert!((parse.change.unwrap().median + 0.20).abs() < 1e-12);
    // The median time is head's in either order: it isn't a change.
    assert!((parse.median_ns - 12e3).abs() < 1e-9, "{parse:?}");
    assert_eq!(find(&found, "rows/new").change, None);

    fs::write(dir.join(report::ORDER_FILE), "sideways\n").unwrap();
    assert!(report::collect(&dir).is_err());
}

/// A head-first rerun as `bench-compare` leaves it, with criterion's
/// changes the wrong way round: a real head regression still fails when
/// attempt 1 showed it too, and head getting faster (base slower) never
/// does.
#[test]
fn bench_report_judges_a_head_first_rerun_by_heads_change() {
    for (name, head_change, code) in [("slower", 0.31, 1), ("faster", -0.31, 0)] {
        let dir = scratch(&format!("bin-head-first-{name}"));
        write_run(&dir.join("first"), 0.6, -0.01);
        write_order(&dir.join("first"), "base-first");
        write_bench(
            &dir.join("first").join("criterion"),
            "rows/parse",
            12e3,
            None,
            Some(change(0.31)),
        );
        write_run(&dir, 0.6, -0.01);
        write_order(&dir, "head-first");
        write_bench(
            &dir.join("criterion"),
            "rows/parse",
            12e3,
            None,
            Some(change(head_change).inverted()),
        );
        let (status, stdout) = run_bench_report_rerun(&dir);
        assert_eq!(status, Some(code), "head {name}: {stdout}");
    }
}

// Group canaries and the third attempt (after run 37069372104).

/// `group`'s canaries in the bench target `bench`, moved by `before` and
/// `after`.
fn group_canaries(bench: &str, group: &str, before: f64, after: f64) -> [Measurement; 2] {
    [(Side::Before, before), (Side::After, after)].map(|(side, moved)| {
        measurement(
            &group_canary_id(bench, group, side),
            15e6,
            Some(change(moved)),
        )
    })
}

/// One attempt shaped like run 37069372104: the first run-wide canary
/// moved by `run`, the `marks` group's canaries (in `benches/index.rs`) by
/// `marks`, and `marks/next_wide` changed by `next_wide`. The `rows`
/// group, with quiet canaries, holds still.
fn marks_attempt(order: Order, run: f64, marks: (f64, f64), next_wide: f64) -> report::Report {
    let mut found = vec![
        measurement("baseline/memchr3_scan", 45.6e6, Some(change(run))),
        measurement("baseline-late/memchr3_scan", 50.4e6, Some(change(-0.017))),
        measurement("marks/next_narrow", 27e3, Some(change(0.01))),
        measurement("marks/next_wide", 767e3, Some(change(next_wide))),
        measurement("rows/parse", 12e3, Some(change(0.02))),
    ];
    found.extend(group_canaries("index", "marks", marks.0, marks.1));
    found.extend(group_canaries("rows", "rows", 0.01, -0.01));
    found.sort_by(|a, b| a.id.cmp(&b.id));
    let mut report = report::evaluate(&found, &[], CI);
    report.order = Some(order);
    report
}

/// A group canary's id names its bench target, group and side, and is a
/// canary of its own kind: not a run-wide one, and never a regression.
#[test]
fn group_canary_ids_round_trip() {
    let id = group_canary_id("index", "marks", Side::Before);
    assert_eq!(id, "canary/index.marks.before");
    // Criterion puts the group's name before the canary's own.
    assert_eq!(
        format!(
            "{}/{}",
            report::GROUP_CANARY,
            report::group_canary_name("index", "marks", Side::Before)
        ),
        id
    );
    assert_eq!(
        GroupCanary::parse(&id),
        Some(GroupCanary {
            bench: "index",
            group: "marks",
            side: Side::Before
        })
    );
    assert_eq!(
        GroupCanary::parse("canary/open.open.after").map(|c| c.side),
        Some(Side::After)
    );
    for not_one in [
        "canary/index.marks",
        "canary/index.marks.during",
        "canary/.marks.before",
        "canary/marks.before",
        "canaries/index.marks.before",
        "marks/next_wide",
        "baseline/memchr3_scan",
    ] {
        assert!(!report::is_group_canary(not_one), "{not_one}");
    }
    assert!(!report::is_canary(&id));
    assert_eq!(report::group_of("marks/next_wide"), "marks");

    // Even moved a long way, a group canary is a canary, not a regression,
    // and doesn't make the run noisy.
    let found = [measurement(&id, 15e6, Some(change(0.60)))];
    let report = report::evaluate(&found, &[], CI);
    assert_eq!(status_of(&report, &id), &Status::NoisyCanary);
    assert!(!report.noisy);
    assert_eq!(report.verdict(), Verdict::Pass);
    assert_eq!(report.problems().count(), 0);
}

/// A benchmark's noise is judged by its own group's canaries: moved ones
/// make the attempt noisy for that group only, and quiet ones keep it
/// quiet even when a run-wide canary, minutes away, moved.
#[test]
fn the_nearest_canaries_judge_a_benchmarks_noise() {
    // The marks canaries moved (+14% before the group): marks is noisy,
    // rows isn't.
    let report = marks_attempt(Order::BaseFirst, 0.01, (0.14, 0.02), 0.32);
    assert!(!report.noisy);
    let wide = report.row("marks/next_wide").unwrap();
    assert!(wide.noisy);
    assert_eq!(
        wide.canaries,
        Canaries::Group {
            before: Some(0.14),
            after: Some(0.02)
        }
    );
    assert_eq!(wide.status, Status::NoisyRegression);
    assert!(!report.row("rows/parse").unwrap().noisy);
    assert_eq!(report.outcome(false), Outcome::Rerun);
    let markdown = report.markdown();
    assert!(
        markdown.contains(
            "| +32.0% (+29.0% to +35.0%) | +14.0% / +2.0% |  | regression? (noisy for it) |"
        ),
        "{markdown}"
    );
    assert!(
        markdown.contains(
            "| `canary/index.marks.before` | 15.0 ms |  | +14.0% (+11.0% to +17.0%) |  |  | \
             canary moved (noisy for its group) |"
        ),
        "{markdown}"
    );

    // A run-wide canary moved (−20%, as on attempt 1 of run 37069372104),
    // but the marks canaries held still: noisy run, quiet for marks.
    let report = marks_attempt(Order::BaseFirst, -0.203, (0.01, -0.02), 0.32);
    assert!(report.noisy);
    assert!(!report.row("marks/next_wide").unwrap().noisy);
    assert_eq!(status_of(&report, "marks/next_wide"), &Status::Regression);
}

/// A group whose canaries didn't run on both sides (the base is from
/// before group canaries) is judged by the run-wide canaries.
#[test]
fn a_group_without_canaries_on_both_sides_falls_back_to_the_run_wide_ones() {
    for run in [0.01, 0.15] {
        let found = [
            measurement("baseline/memchr3_scan", 47e6, Some(change(run))),
            // Head ran it, base didn't: no change.
            measurement("canary/index.marks.before", 15e6, None),
            measurement("marks/next_wide", 767e3, Some(change(0.32))),
        ];
        let report = report::evaluate(&found, &[], CI);
        let wide = report.row("marks/next_wide").unwrap();
        assert_eq!(wide.canaries, Canaries::RunWide);
        assert_eq!(wide.noisy, run > 0.1);
        let expected = if run > 0.1 {
            Status::NoisyRegression
        } else {
            Status::Regression
        };
        assert_eq!(wide.status, expected);
    }
}

/// A real, consistent regression on attempts quiet for it fails on the
/// rerun, with no third attempt, as it always did.
#[test]
fn a_consistent_regression_on_quiet_runs_fails() {
    let first = marks_attempt(Order::BaseFirst, 0.02, (0.01, -0.03), 0.30);
    assert_eq!(status_of(&first, "marks/next_wide"), &Status::Regression);
    assert_eq!(first.outcome(false), Outcome::Rerun);

    let rerun = marks_attempt(Order::HeadFirst, -0.01, (0.02, 0.04), 0.29).after(&first);
    assert_eq!(status_of(&rerun, "marks/next_wide"), &Status::Regression);
    assert_eq!(rerun.third_attempt_plan(), None);
    // Fails whether or not a third attempt could run.
    assert_eq!(rerun.outcome(false), Outcome::Fail);
    assert_eq!(rerun.outcome(true), Outcome::Fail);
}

/// Run 37069372104: `marks/next_wide` at +32% on both attempts, in code
/// identical on both sides, with the marks canaries showing the VM's
/// contention on each. A third attempt of the marks group alone, base
/// first and quiet, shows nothing, so it passes, with a warning.
#[test]
fn a_regression_on_two_noisy_attempts_and_a_quiet_third_without_it_passes_with_a_warning() {
    let first = marks_attempt(Order::BaseFirst, -0.203, (0.15, 0.03), 0.324);
    assert_eq!(
        status_of(&first, "marks/next_wide"),
        &Status::NoisyRegression
    );
    assert_eq!(first.outcome(false), Outcome::Rerun);

    let rerun = marks_attempt(Order::HeadFirst, 0.031, (0.02, -0.12), 0.313).after(&first);
    assert_eq!(status_of(&rerun, "marks/next_wide"), &Status::Recheck);
    assert_eq!(rerun.outcome(false), Outcome::Rerun);
    // With no third attempt allowed, it fails, as before.
    assert_eq!(rerun.outcome(true), Outcome::Fail);

    let plan = rerun.third_attempt_plan().unwrap();
    assert_eq!(plan.groups, ["marks"]);
    assert_eq!(plan.benches, ["index"]);
    assert_eq!(plan.to_text(), format!("index\n{}\n", plan.filter));

    let third = marks_attempt(Order::BaseFirst, 0.01, (0.01, 0.02), 0.004);
    let settled = rerun.settle(third);
    assert_eq!(settled.attempt, 3);
    assert_eq!(settled.rechecked, ["marks/next_wide"]);
    assert_eq!(status_of(&settled, "marks/next_wide"), &Status::Cleared);
    assert_eq!(settled.outcome(true), Outcome::Pass);
    assert_eq!(settled.problems().count(), 1, "the warning for next_wide");
    let markdown = settled.markdown();
    for expected in [
        "**Attempt 3** reran the benchmark `marks/next_wide`",
        "It benchmarked the base commit first, then this commit, for the group `marks` only.",
        "| `marks/next_wide` | 767 µs |  | +0.4% (-2.6% to +3.4%) | +1.0% / +2.0% |  | \
         regression? (not on attempt 3) |",
    ] {
        assert!(markdown.contains(expected), "{expected}\n\n{markdown}");
    }
}

/// A regression on all three attempts fails, whether attempt 3 was quiet
/// for it or noisy.
#[test]
fn a_regression_on_all_three_attempts_fails() {
    for third_canary in [0.01, 0.18] {
        let first = marks_attempt(Order::BaseFirst, -0.203, (0.15, 0.03), 0.324);
        let rerun = marks_attempt(Order::HeadFirst, 0.031, (0.02, -0.12), 0.313).after(&first);
        let third = marks_attempt(Order::BaseFirst, 0.01, (third_canary, 0.0), 0.30);
        assert_eq!(
            third.row("marks/next_wide").unwrap().noisy,
            third_canary > 0.1
        );
        let settled = rerun.settle(third);
        assert_eq!(status_of(&settled, "marks/next_wide"), &Status::Regression);
        let outcome = settled.outcome(true);
        assert_eq!(outcome, Outcome::Fail, "third canary {third_canary}");
        assert_eq!(outcome.exit_code(), 1);
    }
}

/// A third attempt that doesn't show the regression but was itself noisy
/// for it settles nothing: inconclusive, which warns and passes.
#[test]
fn a_noisy_third_attempt_without_the_regression_is_inconclusive() {
    let first = marks_attempt(Order::BaseFirst, 0.01, (0.15, 0.03), 0.324);
    let rerun = marks_attempt(Order::HeadFirst, 0.01, (0.02, -0.12), 0.313).after(&first);
    // The canary after the group got 13% faster on head relative to base:
    // base slowed, which can hide a regression.
    let third = marks_attempt(Order::BaseFirst, 0.01, (0.02, -0.13), 0.05);
    assert!(third.row("marks/next_wide").unwrap().could_hide);
    let settled = rerun.clone().settle(third);
    assert_eq!(status_of(&settled, "marks/next_wide"), &Status::Unsettled);
    let outcome = settled.outcome(true);
    assert_eq!(outcome, Outcome::Inconclusive);
    assert_eq!(outcome.exit_code(), 0);

    // Noisy the other way (head slowed, +13%) can fake a regression but
    // not hide one: it is cleared, as on a quiet attempt.
    let third = marks_attempt(Order::BaseFirst, 0.01, (0.02, 0.13), 0.05);
    let wide = third.row("marks/next_wide").unwrap();
    assert!(wide.noisy && !wide.could_hide);
    let settled = rerun.settle(third);
    assert_eq!(status_of(&settled, "marks/next_wide"), &Status::Cleared);
    assert_eq!(settled.outcome(true), Outcome::Pass);
}

/// A regression on attempt 1 that the rerun doesn't show is judged the
/// way attempt 3 judges one: if the rerun's nearest canaries say its base
/// side slowed, it may have hidden the regression, so it is
/// `Status::Unsettled` (inconclusive), with the same label; otherwise it
/// passes with a warning.
#[test]
fn a_regression_a_later_attempt_may_have_hidden_is_unsettled_on_the_rerun_too() {
    let first = marks_attempt(Order::BaseFirst, 0.01, (0.01, 0.02), 0.324);
    assert_eq!(status_of(&first, "marks/next_wide"), &Status::Regression);

    // The rerun ran head first; its marks canary moved −15% (head's
    // relative to base), so base slowed. The run-wide canaries are quiet.
    let rerun = marks_attempt(Order::HeadFirst, 0.01, (-0.15, 0.0), 0.01).after(&first);
    assert!(!rerun.noisy);
    assert_eq!(status_of(&rerun, "marks/next_wide"), &Status::Unsettled);
    assert!(rerun.unconfirmed.is_empty(), "{:?}", rerun.unconfirmed);
    assert_eq!(rerun.third_attempt_plan(), None);
    for last_attempt in [false, true] {
        assert_eq!(rerun.outcome(last_attempt), Outcome::Inconclusive);
    }

    // The same status, and so the same label, as attempt 3's case.
    let second = marks_attempt(Order::HeadFirst, 0.01, (0.15, 0.0), 0.313).after(&first);
    let settled = second.settle(marks_attempt(Order::BaseFirst, 0.01, (-0.15, 0.0), 0.01));
    assert_eq!(status_of(&settled, "marks/next_wide"), &Status::Unsettled);
    let label = "| regression? (not shown again, base side slowed) |";
    assert!(rerun.markdown().contains(label), "{}", rerun.markdown());
    assert!(settled.markdown().contains(label), "{}", settled.markdown());

    // Moved the other way (+15%, head slowed), the rerun can't have hidden
    // it: it passes with a warning, as on a quiet rerun.
    let rerun = marks_attempt(Order::HeadFirst, 0.01, (0.15, 0.0), 0.01).after(&first);
    assert_eq!(status_of(&rerun, "marks/next_wide"), &Status::Ok);
    assert_eq!(rerun.unconfirmed, ["marks/next_wide"]);
    assert_eq!(rerun.outcome(true), Outcome::Pass);
}

/// The first run after group canaries land compares with a base without
/// them, so every attempt, the third included, falls back to the run-wide
/// canaries.
#[test]
fn a_third_attempt_against_a_base_without_group_canaries_uses_the_run_wide_ones() {
    // Head has its canaries, base doesn't: no change for them.
    let attempt = |order, run: f64, next_wide: f64| {
        let found = [
            measurement("baseline/memchr3_scan", 47e6, Some(change(run))),
            measurement("baseline-late/memchr3_scan", 47e6, Some(change(0.01))),
            measurement("canary/index.marks.before", 15e6, None),
            measurement("canary/index.marks.after", 15e6, None),
            measurement("marks/next_wide", 767e3, Some(change(next_wide))),
        ];
        let mut report = report::evaluate(&found, &[], CI);
        report.order = Some(order);
        report
    };
    let first = attempt(Order::BaseFirst, -0.203, 0.324);
    assert_eq!(
        first.row("marks/next_wide").unwrap().canaries,
        Canaries::RunWide
    );
    let rerun = attempt(Order::HeadFirst, 0.01, 0.313).after(&first);
    assert_eq!(status_of(&rerun, "marks/next_wide"), &Status::Recheck);
    // The canaries still name the bench target.
    assert_eq!(rerun.third_attempt_plan().unwrap().benches, ["index"]);

    for (run, next_wide, expected, outcome) in [
        // Quiet, no regression: cleared.
        (0.01, 0.0, Status::Cleared, Outcome::Pass),
        // A run-wide canary 15% faster on head (base slowed): may have
        // hidden it.
        (-0.15, 0.0, Status::Unsettled, Outcome::Inconclusive),
        // 15% slower on head: can't have hidden it. Attempt 3's run-wide
        // noise doesn't make the result inconclusive on its own.
        (0.15, 0.0, Status::Cleared, Outcome::Pass),
        // The regression again: fails, noisy or not.
        (0.15, 0.30, Status::Regression, Outcome::Fail),
        (0.01, 0.30, Status::Regression, Outcome::Fail),
    ] {
        let settled = rerun
            .clone()
            .settle(attempt(Order::BaseFirst, run, next_wide));
        let context = format!("run {run}, next_wide {next_wide}");
        assert_eq!(
            status_of(&settled, "marks/next_wide"),
            &expected,
            "{context}"
        );
        assert_eq!(settled.outcome(true), outcome, "{context}");
    }
}

/// A rechecked benchmark that attempt 3 has no comparison for (it didn't
/// run, or only on one side) isn't cleared: it regressed on both attempts.
#[test]
fn a_third_attempt_with_no_result_for_it_fails() {
    let first = marks_attempt(Order::BaseFirst, 0.01, (0.15, 0.03), 0.324);
    let rerun = marks_attempt(Order::HeadFirst, 0.01, (0.02, -0.12), 0.313).after(&first);
    let missing = report::evaluate(&[], &[], CI);
    assert_eq!(
        status_of(&rerun.clone().settle(missing), "marks/next_wide"),
        &Status::Regression
    );
    let uncompared = report::evaluate(&[measurement("marks/next_wide", 767e3, None)], &[], CI);
    let settled = rerun.settle(uncompared);
    assert_eq!(status_of(&settled, "marks/next_wide"), &Status::Regression);
    assert_eq!(settled.outcome(true), Outcome::Fail);
}

/// Budgets are unaffected: one over budget fails on any attempt, a third
/// one included, and before any third attempt runs.
#[test]
fn budgets_are_unaffected_by_the_third_attempt() {
    let budgets = [Budget {
        id: "marks/next_wide",
        max_ms: 1.0,
        source: "test",
    }];
    let attempt = |order, marks, next_wide, median_ns| {
        let mut found = vec![
            measurement("baseline/memchr3_scan", 47e6, Some(change(0.01))),
            measurement("marks/next_wide", median_ns, Some(change(next_wide))),
        ];
        found.extend(group_canaries("index", "marks", marks, 0.0));
        let mut report = report::evaluate(&found, &budgets, CI);
        report.order = Some(order);
        report
    };
    let first = attempt(Order::BaseFirst, 0.15, 0.32, 0.8e6);
    let rerun = attempt(Order::HeadFirst, 0.0, 0.31, 0.8e6).after(&first);
    assert_eq!(rerun.outcome(false), Outcome::Rerun);

    // Over budget on attempt 3, which cleared the regression: fails.
    let settled = rerun
        .clone()
        .settle(attempt(Order::BaseFirst, 0.0, 0.0, 1.2e6));
    assert_eq!(status_of(&settled, "marks/next_wide"), &Status::OverBudget);
    assert_eq!(settled.outcome(true), Outcome::Fail);

    // Within budget on attempt 3: cleared, and passes.
    let settled = rerun.settle(attempt(Order::BaseFirst, 0.0, 0.0, 0.8e6));
    assert_eq!(status_of(&settled, "marks/next_wide"), &Status::Cleared);
    assert_eq!(settled.outcome(true), Outcome::Pass);

    // Over budget on the rerun: fails there, with no third attempt.
    let rerun = attempt(Order::HeadFirst, 0.0, 0.31, 1.2e6).after(&first);
    assert_eq!(rerun.outcome(false), Outcome::Fail);
}

/// The third attempt's filter (a criterion regex over benchmark ids)
/// selects every benchmark in the groups to recheck and their canaries,
/// and nothing else.
#[test]
fn the_third_attempt_selects_only_the_groups_to_recheck() {
    let mut found = vec![
        measurement("marks/next_wide", 767e3, Some(change(0.32))),
        measurement("rows/parse", 12e3, Some(change(0.31))),
        measurement("index/build", 300e6, Some(change(0.30))),
    ];
    found.extend(group_canaries("index", "marks", 0.15, 0.0));
    found.extend(group_canaries("rows", "rows", 0.15, 0.0));
    found.extend(group_canaries("index", "index", 0.0, 0.0));
    let first = report::evaluate(&found, &[], CI);
    let rerun = report::evaluate(&found, &[], CI).after(&first);
    // index/build was quiet for it on both: it fails, and isn't rechecked.
    assert_eq!(status_of(&rerun, "index/build"), &Status::Regression);
    assert_eq!(rerun.outcome(false), Outcome::Fail);

    let plan = rerun.third_attempt_plan().unwrap();
    assert_eq!(plan.groups, ["marks", "rows"]);
    assert_eq!(plan.benches, ["index", "rows"]);
    let filter = regex::Regex::new(&plan.filter).unwrap();
    for selected in [
        "marks/next_wide",
        "marks/previous_narrow",
        "rows/parse",
        "canary/index.marks.before",
        "canary/index.marks.after",
        "canary/rows.rows.before",
    ] {
        assert!(filter.is_match(selected), "{selected} by {}", plan.filter);
    }
    for left_out in [
        "index/build",
        "worst/blank_lines",
        "marksx/next",
        "canary/index.index.before",
        "canary/index.worst.after",
        "baseline/memchr3_scan",
        "baseline-late/memchr3_scan",
        "navigate/next_rows",
    ] {
        assert!(!filter.is_match(left_out), "{left_out} by {}", plan.filter);
    }

    // A group with no canary: every bench target runs, filtered.
    let found = [measurement("edits/screen", 1e3, Some(change(0.4)))];
    let noisy = [
        measurement("baseline/memchr3_scan", 47e6, Some(change(0.2))),
        measurement("edits/screen", 1e3, Some(change(0.4))),
    ];
    let rerun = report::evaluate(&found, &[], CI).after(&report::evaluate(&noisy, &[], CI));
    let plan = rerun.third_attempt_plan().unwrap();
    assert!(plan.benches.is_empty());
    assert_eq!(plan.to_text().lines().next(), Some(""));
}

/// Every bench group in `benches/` has its canaries around it: for each
/// `benchmark_group("<group>")`, the file calls
/// `common::canary(c, "<group>", Side::Before)` somewhere before it and
/// `common::canary(c, "<group>", Side::After)` somewhere after it. So no
/// group is judged only by the run-wide canaries. (It checks the order in
/// the source, not that nothing else runs in between.)
#[test]
fn every_bench_group_has_canaries() {
    let benches = Path::new(env!("CARGO_MANIFEST_DIR")).join("benches");
    let mut checked = 0;
    for entry in fs::read_dir(&benches).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_str().unwrap().to_owned();
        // The baseline's own memchr3_scan is the run-wide canary.
        if path.extension().is_none_or(|extension| extension != "rs") || name == "baseline.rs" {
            continue;
        }
        let source = fs::read_to_string(&path).unwrap();
        for (at, _) in source.match_indices("benchmark_group(\"") {
            let rest = &source[at + "benchmark_group(\"".len()..];
            let group = rest.split('"').next().unwrap();
            let before = format!("common::canary(c, \"{group}\", Side::Before);");
            let after = format!("common::canary(c, \"{group}\", Side::After);");
            assert!(
                source[..at].contains(&before),
                "{name}: no `{before}` before the group"
            );
            assert!(
                rest.contains(&after),
                "{name}: no `{after}` after the group"
            );
            checked += 1;
        }
    }
    assert!(checked >= 8, "only {checked} groups found");
}

/// Writes `id`'s result into `<dir>/criterion` with head's change
/// `head_change`, as criterion leaves it for the order already written
/// there: inverted on a head-first attempt.
fn write_head(dir: &Path, id: &str, median_ns: f64, head_change: f64) {
    let criterion = dir.join("criterion");
    let head = change(head_change);
    let written = if report::read_order(&criterion).unwrap() == Some(Order::HeadFirst) {
        head.inverted()
    } else {
        head
    };
    write_bench(&criterion, id, median_ns, None, Some(written));
}

/// One attempt of run 37069372104's shape, as `bench-compare` leaves it in
/// `<dir>/criterion`: the budgets' benchmarks at 60% of budget, the
/// run-wide canaries quiet, the marks canaries moved by `marks`, and
/// `marks/next_wide` changed by `next_wide`. A third attempt (`partial`)
/// has only the marks group, its canaries and the run-wide canaries.
fn write_marks_attempt(dir: &Path, order: &str, partial: bool, marks: (f64, f64), next_wide: f64) {
    fs::create_dir_all(dir.join("criterion")).unwrap();
    write_order(dir, order);
    if !partial {
        for budget in leal_bench::budgets::BUDGETS {
            write_head(dir, budget.id, budget.max_ms * 0.6e6, 0.04);
        }
        write_head(dir, "marks/next_narrow", 27e3, 0.01);
    }
    write_head(dir, "baseline/memchr3_scan", 47e6, 0.02);
    write_head(dir, "baseline-late/memchr3_scan", 47e6, -0.01);
    write_head(dir, "canary/index.marks.before", 15e6, marks.0);
    write_head(dir, "canary/index.marks.after", 15e6, marks.1);
    write_head(dir, "marks/next_wide", 767e3, next_wide);
}

/// Runs `bench-report` with `args` then `<dir>/criterion`, as
/// `bench-compare` does on GitHub Actions.
fn run_bench_report_with(dir: &Path, args: &[&Path]) -> (Option<i32>, String) {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_bench-report"))
        .args(args)
        .arg(dir.join("criterion"))
        .env("GITHUB_ACTIONS", "true")
        .env_remove("GITHUB_STEP_SUMMARY")
        .output()
        .unwrap();
    (
        output.status.code(),
        String::from_utf8(output.stdout).unwrap(),
    )
}

/// `bench-report` end to end, as `bench-compare` runs it: the rerun asks
/// for a third attempt (exit 3) and writes its plan; the third attempt,
/// of the marks group alone, passes with a warning if it is quiet and
/// doesn't show the regression, fails if it shows it, and is inconclusive
/// if it is noisy for it and doesn't. It doesn't report the budgets of the
/// groups it didn't run as missing.
#[test]
fn bench_report_runs_a_third_attempt_for_a_regression_on_noisy_attempts() {
    let dir = scratch("bin-third");
    let (first, second) = (dir.join("first"), dir.join("second"));
    write_marks_attempt(&first, "base-first", false, (0.15, 0.03), 0.324);
    write_marks_attempt(&second, "head-first", false, (0.02, -0.12), 0.313);
    let plan_file = dir.join("plan");
    let first_criterion = first.join("criterion");
    let (code, stdout) = run_bench_report_with(
        &second,
        &[
            Path::new("--first-attempt"),
            &first_criterion,
            Path::new("--plan"),
            &plan_file,
        ],
    );
    assert_eq!(code, Some(3), "{stdout}");
    assert!(stdout.contains("**Third attempt:**"), "{stdout}");
    assert!(
        stdout.contains(
            "::warning::benchmark marks/next_wide regressed on both attempts, but at least one \
             was noisy for it; rerunning its group a third time, base first"
        ),
        "{stdout}"
    );
    assert!(!stdout.contains("::error::"), "{stdout}");
    let plan = fs::read_to_string(&plan_file).unwrap();
    assert!(plan.starts_with("index\n^"), "{plan}");

    // Without --plan, the rerun is the last attempt, and it fails as before.
    let (code, stdout) =
        run_bench_report_with(&second, &[Path::new("--first-attempt"), &first_criterion]);
    assert_eq!(code, Some(1), "{stdout}");
    assert!(
        stdout.contains("::error::benchmark marks/next_wide is a regression"),
        "{stdout}"
    );

    let second_criterion = second.join("criterion");
    let third_args = [
        Path::new("--first-attempt"),
        &first_criterion,
        Path::new("--second-attempt"),
        &second_criterion,
    ];
    for (name, marks, next_wide, code, expected) in [
        (
            "quiet",
            (0.01, 0.02),
            0.004,
            0,
            "::warning::benchmark marks/next_wide regressed on attempts 1 and 2 (at least one \
             noisy for it) but not on attempt 3 (base first), with no sign that its base side \
             slowed, so it passed",
        ),
        (
            "regressed",
            (0.01, 0.02),
            0.30,
            1,
            "::error::benchmark marks/next_wide is a regression: it regressed on all three \
             attempts",
        ),
        (
            "noisy",
            (-0.14, 0.02),
            0.004,
            0,
            "::warning::benchmark marks/next_wide regressed on attempts 1 and 2 but not on \
             attempt 3 (base first), whose nearest canaries say its base side slowed, which can \
             hide a regression, so it wasn't judged",
        ),
        (
            "no-result",
            (0.01, 0.02),
            0.004,
            1,
            "::error::benchmark marks/next_wide regressed on attempts 1 and 2, and attempt 3 has \
             no result for it",
        ),
    ] {
        let third = dir.join(name);
        write_marks_attempt(&third, "base-first", true, marks, next_wide);
        if name == "no-result" {
            // It ran on head only, so criterion compared nothing.
            fs::remove_dir_all(third.join("criterion/marks_next_wide/change")).unwrap();
        }
        let (status, stdout) = run_bench_report_with(&third, &third_args);
        assert_eq!(status, Some(code), "{name}: {stdout}");
        assert!(stdout.contains(expected), "{name}: {expected}\n\n{stdout}");
        assert!(
            !stdout.contains("has a budget but no result"),
            "{name}: {stdout}"
        );
        assert!(stdout.contains("**Attempt 3** reran"), "{name}: {stdout}");
        if code == 0 {
            assert!(!stdout.contains("::error::"), "{name}: {stdout}");
        }
    }
}
