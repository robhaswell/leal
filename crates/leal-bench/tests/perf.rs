//! `leal-perf`'s parsing and verdicts (task 1.10): signposts from `log
//! stream`, `heap -s`'s report, the scroll benchmark's JSON, and the
//! budget table, including every row's verdict on Rob's unlocked run
//! (`data/perf-2026-10-02-m5pro-unlocked.json`).

use leal_bench::perf::{
    HEADROOM_FRAME_MS, HeapReport, PerfRun, Row, ScrollRun, Signpost, SignpostKind, Spread,
    Timeline, Verdict, describe_frame_work, headroom_verdict, is_reference_machine,
    launched_after_ms, machine_model, parse_timestamp, scroll_verdict, table, thousands,
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
fn two_checks_of_one_budget_combine() {
    use Verdict::{Fail, Mixed, OpenOnly, Pass, Untested};
    assert_eq!(Pass.and(Pass), Pass);
    assert_eq!(Pass.and(Fail), Fail);
    assert_eq!(Fail.and(Pass), Fail);
    assert_eq!(Mixed.and(Fail), Fail);
    assert_eq!(Pass.and(Mixed), Mixed);
    assert_eq!(Mixed.and(Pass), Mixed);
    // An untested check is left out.
    assert_eq!(Untested.and(Pass), Pass);
    assert_eq!(Fail.and(Untested), Fail);
    assert_eq!(Untested.and(Untested), Untested);
    // A partial pass stays partial beside a pass, and gives way to worse.
    assert_eq!(OpenOnly.and(Pass), OpenOnly);
    assert_eq!(Pass.and(OpenOnly), OpenOnly);
    assert_eq!(OpenOnly.and(OpenOnly), OpenOnly);
    assert_eq!(OpenOnly.and(Untested), OpenOnly);
    assert_eq!(OpenOnly.and(Mixed), Mixed);
    assert_eq!(Fail.and(OpenOnly), Fail);
    assert_eq!(OpenOnly.label(), "pass (search results untested)");
}

/// The model comes from the environment's `model`, or from the "Machine"
/// line of a run saved before that was recorded. Models have commas in
/// them, and the line separates fields with ", ".
#[test]
fn the_machine_model_is_read_from_the_environment() {
    let line = |text: &str| serde_json::json!({ "lines": [text, "Power: AC"] });
    let m5 = line(
        "Machine: Mac17,8, Apple M5 Pro (6 performance + 12 efficiency cores), 48 GB, macOS 27.0",
    );
    assert_eq!(machine_model(&m5), "Mac17,8");
    assert!(!is_reference_machine(&machine_model(&m5)));
    let air = line(
        "Machine: MacBookAir10,1, Apple M1 (4 performance + 4 efficiency cores), 8 GB, macOS 14.6",
    );
    assert_eq!(machine_model(&air), "MacBookAir10,1");
    assert!(is_reference_machine(&machine_model(&air)));
    // The field wins over the line.
    let both = serde_json::json!({ "model": "MacBookAir10,1", "lines": ["Machine: Mac17,8, Apple M5 Pro"] });
    assert_eq!(machine_model(&both), "MacBookAir10,1");
    // A line with nothing after the model, and no environment at all.
    assert_eq!(machine_model(&line("Machine: Mac17,8")), "Mac17,8");
    assert_eq!(machine_model(&serde_json::json!({})), "");
    assert!(!is_reference_machine(""));
}

fn work(p50: f64, p99: f64) -> ScrollRun {
    ScrollRun {
        frames: 100,
        cpu_p50_ms: Some(p50),
        cpu_p99_ms: Some(p99),
        screen_fps: 120.0,
        ..ScrollRun::default()
    }
}

#[test]
fn the_three_times_rule_judges_main_thread_work_per_frame() {
    // Both p50 and p99 must be within it; at the limit is within it.
    assert_eq!(
        work(1.4, 2.8).within_headroom(HEADROOM_FRAME_MS),
        Some(true)
    );
    assert_eq!(
        work(2.9, 2.0).within_headroom(HEADROOM_FRAME_MS),
        Some(false)
    );
    assert_eq!(
        work(1.5, 3.3).within_headroom(HEADROOM_FRAME_MS),
        Some(false)
    );
    // Rob's unlocked runs (docs/perf-runs/2026-10-02-m5pro-unlocked.md).
    let unlocked = [work(2.9, 5.8), work(2.8, 6.4), work(3.6, 6.8)];
    assert_eq!(
        headroom_verdict(&unlocked, HEADROOM_FRAME_MS),
        Verdict::Fail
    );
    assert_eq!(
        headroom_verdict(&[work(1.0, 2.0), work(2.8, 2.8)], HEADROOM_FRAME_MS),
        Verdict::Pass
    );
    assert_eq!(
        headroom_verdict(&[work(1.0, 2.0), work(1.0, 3.0)], HEADROOM_FRAME_MS),
        Verdict::Fail
    );
    assert_eq!(headroom_verdict(&[], HEADROOM_FRAME_MS), Verdict::Untested);
    assert_eq!(
        describe_frame_work(&unlocked),
        "p50 2.8–3.6 ms, p99 5.8–6.8 ms"
    );
    assert_eq!(
        describe_frame_work(&[work(2.0, 4.0)]),
        "p50 2.0 ms, p99 4.0 ms"
    );
    assert_eq!(describe_frame_work(&[]), "—");
    assert_eq!(work(2.94, 5.85).describe_cpu(), "2.9/5.8 ms");
}

/// A run that didn't report its main-thread work never passes the rule,
/// and its work isn't shown as 0.0.
#[test]
fn missing_frame_work_leaves_the_rule_untested() {
    let missing = ScrollRun {
        cpu_p50_ms: None,
        cpu_p99_ms: None,
        ..work(0.0, 0.0)
    };
    let half = ScrollRun {
        cpu_p99_ms: None,
        ..work(1.0, 0.0)
    };
    assert_eq!(missing.within_headroom(HEADROOM_FRAME_MS), None);
    assert_eq!(half.within_headroom(HEADROOM_FRAME_MS), None);
    assert_eq!(
        headroom_verdict(std::slice::from_ref(&missing), HEADROOM_FRAME_MS),
        Verdict::Untested
    );
    assert_eq!(
        headroom_verdict(&[work(1.0, 2.0), missing.clone()], HEADROOM_FRAME_MS),
        Verdict::Untested
    );
    // A run over the limit still fails it.
    assert_eq!(
        headroom_verdict(&[work(1.0, 3.0), missing.clone()], HEADROOM_FRAME_MS),
        Verdict::Fail
    );
    assert_eq!(missing.describe_cpu(), "—/— ms");
    assert_eq!(describe_frame_work(std::slice::from_ref(&missing)), "—");
    assert_eq!(
        describe_frame_work(&[missing, work(2.0, 4.0)]),
        "p50 2.0 ms, p99 4.0 ms"
    );
    let json = serde_json::json!({ "scroll": { "frames": 10, "late": 0 } });
    let parsed = ScrollRun::parse(&json).unwrap();
    assert_eq!((parsed.cpu_p50_ms, parsed.cpu_p99_ms), (None, None));
    assert_eq!(parsed.heap_settled_mb, None);
}

#[test]
fn scroll_runs_are_read_and_judged() {
    let json: serde_json::Value = serde_json::from_str(
        r#"{"scroll":{"frames":6150,"late":3,"p99":8.3,"cpuP50":4.1,"cpuP99":6.4,"busyOver120Hz":2,"instructionsMean":17.9,"mainThreadGHz":2.0},
            "heapPeakMB":35.7,"heapSettledMB":30.2,"screenMaxFPS":120,"whileIndexing":{"frames":20,"late":1},"findRuns":4,
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
    assert_eq!(run.heap_settled_mb, Some(30.2));
    assert_eq!((run.cpu_p50_ms, run.cpu_p99_ms), (Some(4.1), Some(6.4)));
    assert_eq!(ScrollRun::describe_late(3, 6150), "3 of 6,150 late (0.05%)");
    assert_eq!(ScrollRun::describe_late(0, 0), "0 of 0 late (0.00%)");
    assert_eq!(ScrollRun::parse(&serde_json::json!({})), None);

    // On a 60 Hz display, late frames are 60 Hz ones: the verdict comes
    // from the frames with more than 8.3 ms of work instead.
    let sixty = ScrollRun {
        frames: 100,
        late: 0,
        busy_over_120hz: 4,
        screen_fps: 60.0,
        ..ScrollRun::default()
    };
    assert!(!sixty.at_budget_rate());
    assert_eq!(sixty.dropped_at_120hz(), 4);
    assert_eq!(scroll_verdict(std::slice::from_ref(&sixty)), Verdict::Fail);
    assert_eq!(
        sixty.describe(),
        "4 of 100 frames over 8.3 ms of main-thread work (60 Hz display)"
    );
    let sixty_clean = ScrollRun {
        busy_over_120hz: 0,
        late: 7,
        ..sixty
    };
    assert_eq!(scroll_verdict(&[sixty_clean]), Verdict::Pass);
    assert!(run.at_budget_rate());
    assert_eq!(run.dropped_at_120hz(), 3);
    assert_eq!(run.describe(), "3 of 6,150 late (0.05%)");

    let clean = ScrollRun {
        frames: 10,
        screen_fps: 120.0,
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
    let rows = [
        Row {
            budget: "Open to first rows < 150 ms".into(),
            measured: "80.0 ms".into(),
            how: "signposts".into(),
            verdict: Verdict::Pass,
            note: None,
        },
        Row {
            budget: "Scrolling".into(),
            measured: "0 late".into(),
            how: "flings".into(),
            verdict: Verdict::Fail,
            note: Some("3× rule"),
        },
    ];
    let text = table(&rows);
    assert!(text.starts_with("| Budget (DESIGN §1) |"));
    assert!(text.contains("| Open to first rows < 150 ms | 80.0 ms | signposts | pass |\n"));
    assert!(text.contains("| Scrolling | 0 late | flings | fail (3× rule) |\n"));
    assert_eq!(text.lines().count(), 4);
}

// MARK: Rows from a whole run

/// A trimmed copy of Rob's unlocked run on the M5 Pro, 2 October 2026
/// (docs/perf-runs/2026-10-02-m5pro-unlocked.md), saved before the
/// environment had a `model` field.
fn robs_run() -> serde_json::Value {
    serde_json::from_str(include_str!("data/perf-2026-10-02-m5pro-unlocked.json")).unwrap()
}

/// Each row's budget, verdict and note.
fn verdicts(raw: &serde_json::Value) -> Vec<(String, Verdict, Option<&'static str>)> {
    PerfRun::parse(raw)
        .rows()
        .into_iter()
        .map(|r| (r.budget, r.verdict, r.note))
        .collect()
}

fn row<'a>(rows: &'a [Row], budget: &str) -> &'a Row {
    rows.iter()
        .find(|r| r.budget.starts_with(budget))
        .unwrap_or_else(|| panic!("no row for {budget}"))
}

/// Applies `change` to every run of the named scroll scenarios.
fn edit_scrolls(
    raw: &mut serde_json::Value,
    names: &[&str],
    change: impl Fn(&mut serde_json::Value),
) {
    for name in names {
        for run in raw["scrolls"][*name].as_array_mut().unwrap() {
            change(run);
        }
    }
}

const REFERENCE_ROWS: [&str; 2] = ["afterLoad", "duringLoadWithFind"];

#[test]
fn robs_run_gives_the_verdicts_in_perf_md() {
    use Verdict::{Fail, Mixed, Pass};
    let raw = robs_run();
    let expected: Vec<(String, Verdict, Option<&str>)> = vec![
        ("Launch < 300 ms".into(), Mixed, None),
        (
            "Launched with a file, to its first rows < 450 ms".into(),
            Pass,
            None,
        ),
        ("Open to first rows < 150 ms".into(), Pass, None),
        ("Full index < 500 ms".into(), Pass, None),
        (
            "Scrolling: no dropped frames at 120 Hz".into(),
            Fail,
            Some("late frames; 3× rule"),
        ),
        (
            "… including while background work runs".into(),
            Fail,
            Some("late frames; 3× rule"),
        ),
        (
            "… stress: background work not pausing (beyond the budget)".into(),
            Fail,
            None,
        ),
        ("Leal's heap, reference file < 40 MB".into(), Pass, None),
        ("Idle app, no document < 30 MB footprint".into(), Pass, None),
    ];
    assert_eq!(verdicts(&raw), expected);

    let run = PerfRun::parse(&raw);
    assert_eq!(run.model, "Mac17,8");
    let rows = run.rows();
    // The open budget is judged on the 15 warm opens, the cold one as launch.
    assert!(
        row(&rows, "Open to first rows")
            .measured
            .starts_with("38.7 ms (33.7–46.9, 15 runs)")
    );
    assert!(
        row(&rows, "Launched with a file")
            .measured
            .starts_with("313.4 ms")
    );
    // The heap leaves out the window's baseline, and reports the runs with
    // and without a search apart.
    let heap = &row(&rows, "Leal's heap").measured;
    assert!(
        heap.starts_with("8.7 MB (8.7–8.8, 3 runs) after opening"),
        "{heap}"
    );
    assert!(
        heap.contains("10.7 MB (7.6–14.3, 3 runs) without a search"),
        "{heap}"
    );
    assert!(
        heap.contains("19.7 MB (19.4–19.7, 3 runs) with a search's results held"),
        "{heap}"
    );
    assert!(
        heap.contains("29.7 MB (29.7–29.8, 3 runs) after opening; peak while scrolling 45.3 MB"),
        "{heap}"
    );
    // The idle row is judged on the footprint; RSS (80 MB) is only shown.
    assert!(
        row(&rows, "Idle app")
            .measured
            .starts_with("17.1 MB (17.1–17.2, 3 runs) footprint")
    );
    let scrolling = row(&rows, "Scrolling");
    assert!(
        scrolling
            .measured
            .ends_with("main-thread work per frame p50 2.8–3.6 ms, p99 5.8–6.8 ms")
    );
    assert!(scrolling.how.contains("3× rule (DESIGN §1)"));
    assert!(!row(&rows, "… stress").how.contains("3× rule (DESIGN"));
}

/// Without late frames, the M5 Pro still fails on the 3× rule; the base
/// M1 Air, which the rule stands in for, passes.
#[test]
fn the_three_times_rule_applies_except_on_the_reference_machine() {
    let mut raw = robs_run();
    edit_scrolls(&mut raw, &REFERENCE_ROWS, |run| {
        run["scroll"]["late"] = 0.into()
    });
    let scrolling = |raw: &serde_json::Value| {
        let rows = PerfRun::parse(raw).rows();
        let r = row(&rows, "Scrolling");
        (r.verdict, r.note, r.how.clone())
    };
    let (verdict, note, _) = scrolling(&raw);
    assert_eq!((verdict, note), (Verdict::Fail, Some("3× rule")));

    raw["environment"]["model"] = "MacBookAir10,1".into();
    let (verdict, note, how) = scrolling(&raw);
    assert_eq!((verdict, note), (Verdict::Pass, None));
    assert!(how.contains("the reference machine, so the 3× rule doesn't apply"));

    // The Air still fails on late frames.
    let mut late = robs_run();
    late["environment"]["model"] = "MacBookAir10,1".into();
    let (verdict, note, _) = scrolling(&late);
    assert_eq!((verdict, note), (Verdict::Fail, None));

    // And on the M5 Pro, frame work within the rule passes.
    edit_scrolls(&mut raw, &REFERENCE_ROWS, |run| {
        run["scroll"]["cpuP50"] = 1.0.into();
        run["scroll"]["cpuP99"] = 2.5.into();
    });
    raw["environment"]["model"] = "Mac17,8".into();
    let (verdict, note, _) = scrolling(&raw);
    assert_eq!((verdict, note), (Verdict::Pass, None));
}

/// `--no-scroll`: the scroll rows are untested, and the heap passes only
/// partly, since no search's results were held.
#[test]
fn a_run_with_no_scroll_runs() {
    use Verdict::{Mixed, OpenOnly, Pass, Untested};
    let mut raw = robs_run();
    raw["scrolls"] = serde_json::json!({});
    let rows = verdicts(&raw);
    let expected: Vec<(String, Verdict, Option<&str>)> = vec![
        ("Launch < 300 ms".into(), Mixed, None),
        (
            "Launched with a file, to its first rows < 450 ms".into(),
            Pass,
            None,
        ),
        ("Open to first rows < 150 ms".into(), Pass, None),
        ("Full index < 500 ms".into(), Pass, None),
        (
            "Scrolling: no dropped frames at 120 Hz".into(),
            Untested,
            None,
        ),
        (
            "… including while background work runs".into(),
            Untested,
            None,
        ),
        (
            "… stress: background work not pausing (beyond the budget)".into(),
            Untested,
            None,
        ),
        ("Leal's heap, reference file < 40 MB".into(), OpenOnly, None),
        ("Idle app, no document < 30 MB footprint".into(), Pass, None),
    ];
    assert_eq!(rows, expected);
    let heap = PerfRun::parse(&raw).rows();
    assert!(
        row(&heap, "Leal's heap")
            .measured
            .contains("— without a search and — with a search")
    );
}

/// A run saved before the scroll JSON had `heapSettledMB` is judged on the
/// heap after opening only, and says so.
#[test]
fn a_run_without_the_settled_heap() {
    let mut raw = robs_run();
    edit_scrolls(&mut raw, &REFERENCE_ROWS, |run| {
        run.as_object_mut().unwrap().remove("heapSettledMB");
    });
    let rows = PerfRun::parse(&raw).rows();
    assert_eq!(row(&rows, "Leal's heap").verdict, Verdict::OpenOnly);
    // The heap with a search held counts: over the budget, it fails.
    let mut over = robs_run();
    edit_scrolls(&mut over, &["duringLoadWithFind"], |run| {
        run["heapSettledMB"] = 70.0.into()
    });
    let rows = PerfRun::parse(&over).rows();
    assert_eq!(row(&rows, "Leal's heap").verdict, Verdict::Fail);
    // A drawing peak over 40 MB doesn't.
    let mut peak = robs_run();
    edit_scrolls(&mut peak, &REFERENCE_ROWS, |run| {
        run["heapPeakMB"] = 90.0.into()
    });
    let rows = PerfRun::parse(&peak).rows();
    assert_eq!(row(&rows, "Leal's heap").verdict, Verdict::Pass);
}

/// Runs without `cpuP50` never pass the 3× rule, and their work isn't
/// printed as 0.0.
#[test]
fn a_run_without_frame_work() {
    let mut raw = robs_run();
    edit_scrolls(&mut raw, &["afterLoad"], |run| {
        run["scroll"]["late"] = 0.into();
        run["scroll"].as_object_mut().unwrap().remove("cpuP50");
    });
    let rows = PerfRun::parse(&raw).rows();
    let scrolling = row(&rows, "Scrolling");
    assert_eq!(scrolling.verdict, Verdict::Untested);
    assert_eq!(scrolling.note, Some("3× rule untested"));
    assert!(
        scrolling
            .measured
            .ends_with("main-thread work per frame p50 —, p99 5.8–6.8 ms"),
        "{}",
        scrolling.measured
    );
    assert!(!scrolling.measured.contains("p50 0.0"));
}
