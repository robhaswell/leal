//! `leal-perf`'s parsing and verdicts (task 1.10): signposts from `log
//! stream`, `heap -s`'s report, the scroll benchmark's JSON, and the
//! budget table.

use leal_bench::perf::{
    HeapReport, Row, ScrollRun, Signpost, SignpostKind, Spread, Timeline, Verdict,
    launched_after_ms, parse_timestamp, scroll_verdict, table, thousands,
};

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-6
}

/// A signpost line as `log stream --signpost --style ndjson` prints it,
/// trimmed to the fields that matter.
fn line(pid: u32, name: &str, kind: &str, id: u64, time: &str, message: &str) -> String {
    format!(
        r#"{{"eventType":"signpostEvent","signpostID":{id},"subsystem":"io.github.robhaswell.leal","category":"PointsOfInterest","signpostType":"{kind}","timestamp":"{time}","signpostName":"{name}","eventMessage":"{message}","processID":{pid}}}"#
    )
}

#[test]
fn timestamps_are_read_with_their_offset() {
    let t = parse_timestamp("2026-10-01 21:04:09.317471+0100").unwrap();
    assert!(close(t, 1_790_885_049.317_471), "{t}");
    // The same moment in UTC and in another zone.
    assert!(close(
        parse_timestamp("2026-10-01 20:04:09.317471+0000").unwrap(),
        t
    ));
    assert!(close(
        parse_timestamp("2026-10-01 15:34:09.317471-0430").unwrap(),
        t
    ));
    // A leap day, and the epoch.
    assert!(close(
        parse_timestamp("2024-02-29 23:59:59.000000+0000").unwrap(),
        1_709_251_199.0
    ));
    assert!(close(
        parse_timestamp("1970-01-01 00:00:00.000000+0000").unwrap(),
        0.0
    ));
    for bad in [
        "",
        "2026-10-01",
        "2026-13-01 00:00:00.0+0000",
        "2026-10-01 00:00:00.0 0000",
        "x",
    ] {
        assert_eq!(parse_timestamp(bad), None, "{bad:?}");
    }
}

#[test]
fn signposts_are_read_from_ndjson() {
    let text = line(
        42,
        "Launched",
        "event",
        7,
        "2026-10-01 21:04:09.317471+0100",
        "252.335 ms after the process started",
    );
    let signpost = Signpost::parse(&text).unwrap();
    assert_eq!(signpost.pid, 42);
    assert_eq!(signpost.name, "Launched");
    assert_eq!(signpost.kind, SignpostKind::Event);
    assert_eq!(signpost.id, 7);
    assert!(close(launched_after_ms(&signpost).unwrap(), 252.335));
    // The stream's header and other log lines aren't signposts.
    assert_eq!(
        Signpost::parse("Filtering the log data using \"subsystem == …\""),
        None
    );
    assert_eq!(
        Signpost::parse(r#"{"eventType":"logEvent","processID":1}"#),
        None
    );
}

#[test]
fn intervals_pair_by_id_within_a_process() {
    let lines = [
        line(
            1,
            "Index",
            "begin",
            2,
            "2026-10-01 21:04:09.325144+0100",
            "",
        ),
        // Another process's interval of the same name, and id, in between.
        line(
            9,
            "Index",
            "begin",
            2,
            "2026-10-01 21:04:09.326000+0100",
            "",
        ),
        line(9, "Index", "end", 2, "2026-10-01 21:04:09.327000+0100", ""),
        // A second interval of this process that ends first.
        line(1, "Index", "end", 3, "2026-10-01 21:04:09.400000+0100", ""),
        line(1, "Index", "end", 2, "2026-10-01 21:04:09.490870+0100", ""),
        line(
            1,
            "Launched",
            "event",
            5,
            "2026-10-01 21:04:09.317471+0100",
            "1.5 ms after",
        ),
    ];
    let all: Vec<Signpost> = lines.iter().filter_map(|l| Signpost::parse(l)).collect();
    assert_eq!(all.len(), 6);
    let timeline = Timeline::of(&all, 1);
    let index = timeline.duration_ms("Index").unwrap();
    assert!((index - 165.726).abs() < 1e-3, "{index}");
    assert!(timeline.event("Launched").is_some());
    assert_eq!(timeline.duration_ms("Review"), None);
    // Interval 3 has no begin, so only interval 2 counts.
    let both = timeline.durations_ms("Index");
    assert_eq!(both.len(), 1, "{both:?}");
    let reopened = [
        line(
            4,
            "Open to first rows",
            "begin",
            1,
            "2026-10-01 21:00:00.000000+0100",
            "",
        ),
        line(
            4,
            "Open to first rows",
            "end",
            1,
            "2026-10-01 21:00:00.100000+0100",
            "",
        ),
        line(
            4,
            "Open to first rows",
            "begin",
            8,
            "2026-10-01 21:00:01.000000+0100",
            "",
        ),
        line(
            4,
            "Open to first rows",
            "begin",
            9,
            "2026-10-01 21:00:02.000000+0100",
            "",
        ),
        line(
            4,
            "Open to first rows",
            "end",
            8,
            "2026-10-01 21:00:01.030000+0100",
            "",
        ),
    ];
    let reopened: Vec<Signpost> = reopened.iter().filter_map(|l| Signpost::parse(l)).collect();
    let opens = Timeline::of(&reopened, 4).durations_ms("Open to first rows");
    assert_eq!(opens.len(), 2, "the unfinished one is left out: {opens:?}");
    assert!(
        (opens[0] - 100.0).abs() < 1e-3 && (opens[1] - 30.0).abs() < 1e-3,
        "{opens:?}"
    );
    let other = Timeline::of(&all, 9);
    assert!((other.duration_ms("Index").unwrap() - 1.0).abs() < 1e-3);
    assert!(other.event("Launched").is_none());
}

#[test]
fn heap_reports_are_read() {
    let text = "Process:         Leal [17007]\n\
        Physical footprint:         11.4M\n\
        Physical footprint (peak):  138.0M\n\
        ----\n\
        Process 17007: 4 zones\n\
        All zones: 25149 nodes malloced - Sizes: 48KB[2] 20KB[3]\n\
        -----------------------------------------------------------------------\n\
        All zones: 25149 nodes (2607696 bytes)\n";
    let report = HeapReport::parse(text).unwrap();
    assert_eq!(report.heap_bytes, 2_607_696);
    assert!(close(report.footprint_mb, 11.4));
    assert!(close(report.footprint_peak_mb, 138.0));
    assert!((report.heap_mb() - 2.486_8).abs() < 1e-3);
    let gigabytes =
        HeapReport::parse("Physical footprint: 1.5G\nAll zones: 1 nodes (10 bytes)\n").unwrap();
    assert!(close(gigabytes.footprint_mb, 1536.0));
    assert_eq!(HeapReport::parse("Physical footprint: 11.4M\n"), None);
    assert_eq!(HeapReport::parse("All zones: 1 nodes (10 bytes)\n"), None);
}

#[test]
fn spreads_and_verdicts() {
    let odd = Spread::of(&[3.0, 1.0, 2.0]).unwrap();
    assert_eq!((odd.median, odd.min, odd.max, odd.runs), (2.0, 1.0, 3.0, 3));
    assert_eq!(odd.describe("ms", 1), "2.0 ms (1.0–3.0, 3 runs)");
    let even = Spread::of(&[4.0, 1.0, 2.0, 10.0]).unwrap();
    assert!(close(even.median, 3.0));
    assert_eq!(
        Spread::of(&[5.0]).unwrap().describe("MB", 2),
        "5.00 MB (1 run)"
    );
    assert_eq!(Spread::of(&[]), None);

    assert_eq!(
        Verdict::below(Spread::of(&[100.0, 120.0]), 150.0),
        Verdict::Pass
    );
    assert_eq!(
        Verdict::below(Spread::of(&[100.0, 120.0, 160.0]), 150.0),
        Verdict::Mixed
    );
    assert_eq!(
        Verdict::below(Spread::of(&[160.0, 170.0, 100.0]), 150.0),
        Verdict::Fail
    );
    // At the budget is not under it.
    assert_eq!(Verdict::below(Spread::of(&[150.0]), 150.0), Verdict::Fail);
    assert_eq!(Verdict::below(None, 150.0), Verdict::Untested);
}

#[test]
fn scroll_runs_are_read_and_judged() {
    let json: serde_json::Value = serde_json::from_str(
        r#"{"scroll":{"frames":6150,"late":3,"p99":8.3,"cpuP50":4.1,"cpuP99":6.4,"busyOver120Hz":2,"instructionsMean":17.9,"mainThreadGHz":2.0},
            "heapPeakMB":35.7,"screenMaxFPS":120,"whileIndexing":{"frames":20,"late":1},"findRuns":4,
            "displayLinkStalls":0,"windowVisible":true}"#,
    )
    .unwrap();
    let run = ScrollRun::parse(&json).unwrap();
    assert_eq!((run.frames, run.late), (6150, 3));
    assert_eq!(run.while_indexing, (1, 20));
    assert_eq!(run.busy_over_120hz, 2);
    assert_eq!(run.while_finding, (0, 0));
    assert_eq!(run.find_runs, 4);
    assert!(run.visible);
    assert!(close(run.heap_peak_mb, 35.7));
    assert_eq!(ScrollRun::describe_late(3, 6150), "3 of 6,150 late (0.05%)");
    assert_eq!(ScrollRun::describe_late(0, 0), "0 of 0 late (0.00%)");
    assert_eq!(ScrollRun::parse(&serde_json::json!({})), None);

    let clean = ScrollRun {
        frames: 10,
        ..ScrollRun::default()
    };
    assert_eq!(scroll_verdict(&[]), Verdict::Untested);
    assert_eq!(
        scroll_verdict(&[clean.clone(), clean.clone()]),
        Verdict::Pass
    );
    assert_eq!(scroll_verdict(&[clean, run]), Verdict::Fail);
}

#[test]
fn the_table_has_a_row_per_budget() {
    assert_eq!(thousands(0), "0");
    assert_eq!(thousands(999), "999");
    assert_eq!(thousands(1000), "1,000");
    assert_eq!(thousands(1_234_567), "1,234,567");
    let rows = [Row {
        budget: "Open to first rows < 150 ms".into(),
        measured: "80.0 ms".into(),
        how: "signposts".into(),
        verdict: Verdict::Pass,
    }];
    let text = table(&rows);
    assert!(text.starts_with("| Budget (DESIGN §1) |"));
    assert!(text.contains("| Open to first rows < 150 ms | 80.0 ms | signposts | pass |\n"));
    assert_eq!(text.lines().count(), 3);
}
