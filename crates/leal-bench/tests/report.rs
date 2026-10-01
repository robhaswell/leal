//! The benchmark report: reading criterion's results, and the checks CI
//! fails on (budgets, regressions and noise).

use std::fs;
use std::path::{Path, PathBuf};

use leal_bench::budgets::Budget;
use leal_bench::report::{self, Change, Measurement, Status, Thresholds, Verdict};

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
        "baseline/scan",
        25e6,
        Some(100_000_000),
        Some(scan_change),
    );
    write_bench(&dir, "index/build", 300e6, None, None);
    // Criterion's own summary folder and saved baselines are not results.
    fs::create_dir_all(dir.join("report")).unwrap();
    fs::create_dir_all(dir.join("baseline_scan/base")).unwrap();

    let found = report::collect(&dir).unwrap();

    assert_eq!(
        found,
        [
            Measurement {
                id: "baseline/scan".to_owned(),
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

#[test]
fn both_canary_groups_are_canaries() {
    assert!(report::is_canary("baseline/memchr3_scan"));
    assert!(report::is_canary("baseline-late/memchr3_scan"));
    assert!(!report::is_canary("baselines/x"));
    assert!(!report::is_canary("index/build"));
}

/// A regression is a slowdown whose whole 95% interval is above the
/// threshold, so a noisy median alone doesn't fail the job.
#[test]
fn a_regression_needs_the_interval_above_the_threshold() {
    let found = [
        measurement("baseline/scan", 1e6, Some(change(0.01))),
        measurement("baseline-late/scan", 1e6, Some(change(-0.02))),
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
    assert_eq!(status_of(&report, "baseline/scan"), &Status::Canary);
    assert_eq!(status_of(&report, "baseline-late/scan"), &Status::Canary);
    assert!(!report.noisy);
    assert_eq!(report.verdict(), Verdict::Fail);
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
/// judge: the run is inconclusive, and `bench-compare` reruns it.
#[test]
fn a_moving_canary_makes_the_run_noisy() {
    for (canary, canary_change) in [
        ("baseline/read", 0.11),
        ("baseline/read", -0.11),
        ("baseline-late/read", 0.11),
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
    }
}

#[test]
fn a_canary_within_the_noise_threshold_is_quiet() {
    let found = [
        measurement("baseline/read", 1e6, Some(change(0.09))),
        measurement("baseline-late/read", 1e6, Some(change(-0.09))),
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
        measurement("baseline/read", 1e6, Some(change(0.90))),
        measurement("index/build", 501e6, None),
    ];
    let report = report::evaluate(&found, &budgets, CI);
    assert!(report.noisy);
    assert_eq!(status_of(&report, "index/build"), &Status::OverBudget);
    assert_eq!(report.verdict(), Verdict::Fail);

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
            id: "baseline/scan".to_owned(),
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
        markdown.contains("| `baseline/scan` | 20.0 ms | 4.88 GiB/s | +1.2% (-0.4% to +3.1%) |"),
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
        assert!(!report::is_canary(budget.id), "canaries have no budget");
        assert!(budget.max_ms > 0.0);
        assert!(!budget.source.is_empty());
    }
    let mut ids: Vec<_> = budgets.iter().map(|budget| budget.id).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), budgets.len(), "duplicate budget ids");
}
