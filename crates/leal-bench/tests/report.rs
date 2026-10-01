//! The benchmark report: reading criterion's results, and the checks CI
//! fails on (budgets and regressions).

use std::fs;
use std::path::{Path, PathBuf};

use leal_bench::budgets::Budget;
use leal_bench::report::{self, Measurement, Status};

/// A fresh, empty directory for one test.
fn scratch(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("report")
        .join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn estimate(point: f64) -> String {
    format!(
        r#"{{"confidence_interval":{{"confidence_level":0.95,"lower_bound":{lo},"upper_bound":{hi}}},"point_estimate":{point},"standard_error":1.0}}"#,
        lo = point * 0.99,
        hi = point * 1.01,
    )
}

/// Writes one benchmark's results the way criterion lays them out.
fn write_bench(root: &Path, id: &str, median_ns: f64, bytes: Option<u64>, change: Option<f64>) {
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
    let e = estimate(median_ns);
    fs::write(
        dir.join("new/estimates.json"),
        format!(r#"{{"mean":{e},"median":{e},"median_abs_dev":{e},"slope":null,"std_dev":{e}}}"#),
    )
    .unwrap();
    if let Some(change) = change {
        fs::create_dir_all(dir.join("change")).unwrap();
        let c = estimate(change);
        fs::write(
            dir.join("change/estimates.json"),
            format!(r#"{{"mean":{c},"median":{c}}}"#),
        )
        .unwrap();
    }
}

fn measurement(id: &str, median_ns: f64, change: Option<f64>) -> Measurement {
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
    write_bench(&dir, "baseline/scan", 25e6, Some(100_000_000), Some(0.02));
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
                change: Some(0.02),
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
fn a_slowdown_past_the_threshold_is_a_regression() {
    let found = [
        measurement("baseline/scan", 1e6, Some(0.01)),
        measurement("index/build", 1e6, Some(0.30)),
        measurement("rows/parse", 1e6, Some(0.20)),
        measurement("rows/new", 1e6, None),
    ];
    let report = report::evaluate(&found, &[], 0.25);
    assert_eq!(status_of(&report, "index/build"), &Status::Regression);
    assert_eq!(status_of(&report, "rows/parse"), &Status::Ok);
    assert_eq!(status_of(&report, "rows/new"), &Status::Ok);
    assert_eq!(status_of(&report, "baseline/scan"), &Status::Canary);
    assert!(!report.noisy);
    assert!(report.failed());
}

#[test]
fn speedups_never_fail() {
    let found = [measurement("index/build", 1e6, Some(-0.60))];
    let report = report::evaluate(&found, &[], 0.25);
    assert_eq!(status_of(&report, "index/build"), &Status::Ok);
    assert!(!report.failed());
}

/// The `baseline/` benchmarks run the same code before and after, so if
/// they move by more than the threshold, the runner is too noisy to judge.
#[test]
fn a_moving_canary_turns_regressions_into_warnings() {
    for canary_change in [0.40, -0.40] {
        let found = [
            measurement("baseline/read", 1e6, Some(canary_change)),
            measurement("index/build", 1e6, Some(0.50)),
        ];
        let report = report::evaluate(&found, &[], 0.25);
        assert!(report.noisy);
        assert_eq!(status_of(&report, "index/build"), &Status::NoisyRegression);
        assert!(!report.failed());
    }
}

#[test]
fn over_budget_fails_even_on_a_noisy_run() {
    let budgets = [Budget {
        id: "index/build",
        max_ms: 500.0,
        source: "DESIGN §1",
    }];
    let found = [
        measurement("baseline/read", 1e6, Some(0.90)),
        measurement("index/build", 501e6, None),
    ];
    let report = report::evaluate(&found, &budgets, 0.25);
    assert_eq!(status_of(&report, "index/build"), &Status::OverBudget);
    assert!(report.failed());

    let found = [measurement("index/build", 499e6, Some(0.10))];
    let report = report::evaluate(&found, &budgets, 0.25);
    assert_eq!(status_of(&report, "index/build"), &Status::Ok);
    assert!(!report.failed());
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
    let report = report::evaluate(&[], &budgets, 0.25);
    assert_eq!(status_of(&report, "index/build"), &Status::Missing);
    assert!(report.failed());
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
            change: Some(0.012),
        },
        measurement("index/build", 320e6, Some(-0.05)),
    ];
    let markdown = report::evaluate(&found, &budgets, 0.25).markdown();
    assert!(markdown.contains("| `baseline/scan` | 20.0 ms | 4.88 GiB/s | +1.2% |"));
    assert!(markdown.contains("| `index/build` | 320 ms |  | -5.0% | 500 ms (DESIGN §1) | ok |"));
}

/// Every budget names a benchmark id, and no two budgets name the same one.
#[test]
fn budgets_are_well_formed() {
    let budgets = leal_bench::budgets::BUDGETS;
    for budget in budgets {
        assert!(budget.id.contains('/'), "`{}` is `group/name`", budget.id);
        assert!(budget.max_ms > 0.0);
        assert!(!budget.source.is_empty());
    }
    let mut ids: Vec<_> = budgets.iter().map(|budget| budget.id).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), budgets.len(), "duplicate budget ids");
}
